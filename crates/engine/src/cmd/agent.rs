//! mixforge for set planning and library housekeeping: `library.suggest` (next-track
//! suggestions), `dj.plan_set` (mixforge's set planner) and `library.analyze` (mixforge's
//! analysis), each a background job ([`super::jobs`]). The work is mixforge's own code, run through
//! the sidecar `mixforge_agent.py` with the python in `DJFORGE_PYTHON` against the mixforge
//! checkout in `DJFORGE_MIXFORGE` ([`mixforge_agent`]); the app sets the runner up
//! ([`Engine::set_agent`]), and tests give a fake, so no test runs mixforge.
//!
//! suggest and plan only read mixforge's library; analyze writes mixforge's own analysis cache, as
//! `mixforge analyze` does. No music file is written.

use super::decks::{bool_param, num};
use super::*;
use crate::cmd;
use serde_json::json;
use std::io::{BufRead as _, Read as _, Write as _};
use std::sync::Arc;

/// Runs a sidecar op: `(op, args, progress, stop)` → the op's result. `progress` is given each
/// progress report; `stop` says when to give up (the run then ends with an error).
pub type AgentRunner = Arc<dyn Fn(&str, &Value, &dyn Fn(Value), &dyn Fn() -> bool) -> std::result::Result<Value, String> + Send + Sync>;

/// The sidecar: JSON lines of progress, then a result or an error.
const MIXFORGE_AGENT: &str = include_str!("mixforge_agent.py");

/// The runner that runs mixforge's code through the sidecar.
pub fn mixforge_agent() -> AgentRunner {
    Arc::new(run_sidecar)
}

fn run_sidecar(op: &str, args: &Value, progress: &dyn Fn(Value), stop: &dyn Fn() -> bool) -> std::result::Result<Value, String> {
    let var = |name: &str, what: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| format!("set {name} to {what}"));
    let python = var("DJFORGE_PYTHON", "the python.exe of mixforge's environment")?;
    let mixforge = var("DJFORGE_MIXFORGE", "the mixforge folder (the one holding the `mixforge` package)")?;
    let mut command = std::process::Command::new(&python);
    command
        .args(["-I", "-c", MIXFORGE_AGENT, &mixforge, op])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the GUI must not flash a console.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|err| format!("could not run DJFORGE_PYTHON ({python}): {err}"))?;
    let input = serde_json::to_vec(args).map_err(|e| e.to_string())?;
    if let Some(mut pipe) = child.stdin.take() {
        let _ = std::thread::Builder::new().name("agent-stdin".into()).spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    // stdout: JSON lines, read as they come; stderr: kept for an error's last lines.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    if let Some(out) = child.stdout.take() {
        let _ = std::thread::Builder::new().name("agent-stdout".into()).spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(std::result::Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let err_text = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(mut err) = child.stderr.take() {
        let keep = Arc::clone(&err_text);
        let _ = std::thread::Builder::new().name("agent-stderr".into()).spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = err.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut s) = keep.lock() {
                    s.push_str(&String::from_utf8_lossy(buf.get(..n).unwrap_or_default()));
                    // Only the end is wanted.
                    if s.len() > 16_384 {
                        let cut = s.len() - 8_192;
                        let cut = (cut..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(s.len());
                        s.drain(..cut);
                    }
                }
            }
        });
    }
    let mut answer: Option<std::result::Result<Value, String>> = None;
    loop {
        if stop() {
            kill_tree(&mut child);
            return Err("cancelled".into());
        }
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(line) => match serde_json::from_str::<Value>(&line) {
                Ok(v) if v.get("progress").is_some() => progress(v.get("progress").cloned().unwrap_or(Value::Null)),
                Ok(v) if v.get("result").is_some() => answer = Some(Ok(v.get("result").cloned().unwrap_or(Value::Null))),
                Ok(v) if v.get("error").is_some() => {
                    answer = Some(Err(v.get("error").and_then(Value::as_str).unwrap_or("mixforge failed").to_string()));
                }
                // Anything else (a worker's own printing) is not ours.
                _ => {}
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    answer.unwrap_or_else(|| {
        let err = err_text.lock().map(|s| s.clone()).unwrap_or_default();
        let lines: Vec<&str> = err.lines().collect();
        let tail = lines.get(lines.len().saturating_sub(5)..).unwrap_or_default().join("\n");
        Err(format!("mixforge gave no answer ({status}): {tail}"))
    })
}

/// Stops the sidecar and the workers it started (mixforge analyses in a process pool).
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x0800_0000)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

const SUGGEST: &str = "library.suggest";
const PLAN: &str = "dj.plan_set";
const ANALYZE: &str = "library.analyze";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "library.suggest",
            "Suggest Next Tracks",
            [],
            None,
            "{from: path, set?: [paths already in the set], direction?: steady|up|down (default steady), limit?: 1..=200 (default 25)} — a background job (returns {job}; see job.status): mixforge's next-track suggestions after `from`, best first, from the tracks mixforge analysed: each {path, title, artist, bpm, camelot, key, key_source, energy, energy_delta, relation (same key, +1 up, -1 down, A→B switch, +2 energy boost, +7 semitone boost, -2 wind down, diagonal, curveball…), rank (0 safe … 4 curveball), bpm_delta_pct, duration, same_song_as / similar_to (a track of the set it sounds like), saved_pairing, score (lower is better)}. up raises tempo and energy a step, down lowers them; tracks whose tempo isn't steady are left out. Needs mixforge (DJFORGE_PYTHON, DJFORGE_MIXFORGE) and `from` analysed by it",
            always,
            suggest
        ),
        cmd!(
            noundo "dj.plan_set",
            "Plan DJ Set",
            [],
            None,
            "{tracks?: [paths], source?: path or [paths] (playlists .m3u/.m3u8, folders, files), out?: path for plan.json (default Music/djforge-plans/<first source's name>.plan.json), order?: keep|harmonic|rising (default keep: the order given), tempo?: ramp|master (default ramp: each track at its own tempo, gliding through transitions), bpm? (one master tempo), bars? (transition length), minutes_per_track? (play about this much of each), style?: auto|blend|quick|long|filter|echo (default auto), keep_variable?: bool} — a background job (returns {job}; see job.status): mixforge's set planner, as `mixforge plan` (tracks not yet analysed are analysed first, into mixforge's cache). Writes the plan to `out` (load it with dj.import_set) and returns {plan, master_bpm, tempo_mode, tracks: [{path, title, artist, camelot, bpm, play_bpm, mix_in_s, mix_out_s (in the track's file), transition_out: {style, bars, notes}}], warnings (clashes, same songs, off-grid outros, journey notes), failed (files mixforge couldn't analyse), left_out (tracks not planned: tempo not steady)}",
            always,
            plan_set
        ),
        cmd!(
            noundo "library.analyze",
            "Analyse Library Tracks",
            [],
            None,
            "{paths: [paths], force?: bool (analyse again even when mixforge has it), sections?: bool (default true: also all-in-one's sections, which runs in WSL and is slow)} — a background job (returns {job}; see job.status, job.cancel): mixforge's analysis of the files (BPM, beat grid, key, loudness, sections), stored in mixforge's own library as `mixforge analyze` does. Progress {stage, done, total, current, failed}; result {analysed, failed: [{path, error}], tracks: [{path, title, bpm, camelot, …}]}. library.health lists the files that need it. Music files are only read",
            always,
            analyze
        ),
    ]
}

fn runner(e: &Engine, cmd: &str) -> Result<AgentRunner> {
    e.agent.clone().ok_or_else(|| {
        bad(cmd, "mixforge isn't set up for this engine: run it in djforge (or djforge-cli mcp), with DJFORGE_PYTHON and DJFORGE_MIXFORGE set")
    })
}

/// Starts `op` with `args` as a `kind` job for `cmd`.
fn start_op(e: &mut Engine, cmd: &str, kind: &'static str, op: &'static str, args: Value) -> Result<Value> {
    let run = runner(e, cmd)?;
    super::jobs::start(e, cmd, kind, move |ctx| {
        let progress = |v: Value| ctx.progress(v);
        let stop = || ctx.cancelled();
        run(op, &args, &progress, &stop).map_err(|why| bad(cmd_name(op), why))
    })
}

fn cmd_name(op: &str) -> &'static str {
    match op {
        "suggest" => SUGGEST,
        "plan" => PLAN,
        _ => ANALYZE,
    }
}

/// A path parameter: a file that may be read.
fn readable(cmd: &str, path: &str) -> Result<String> {
    crate::file_access::check_read(path).map_err(|why| bad(cmd, why))?;
    if crate::io::size_and_mtime(path).is_none() {
        return Err(bad(cmd, format!("{path}: no such file")));
    }
    Ok(path.to_string())
}

fn paths_param(cmd: &str, p: &Value, key: &str) -> Result<Vec<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad(cmd, format!("`{key}` must be paths (strings)")))).collect()
        }
        Some(_) => Err(bad(cmd, format!("`{key}` must be a path or a list of paths"))),
    }
}

fn suggest(e: &mut Engine, p: &Value) -> Result<Value> {
    let from = str_param(p, "from").filter(|s| !s.trim().is_empty()).ok_or_else(|| bad(SUGGEST, "`from` required: the track to follow"))?;
    let from = readable(SUGGEST, from)?;
    let set = paths_param(SUGGEST, p, "set")?;
    let direction = match str_param(p, "direction") {
        None => "steady",
        Some(d @ ("steady" | "up" | "down")) => d,
        Some(_) => return Err(bad(SUGGEST, "`direction` must be steady, up or down")),
    };
    let limit = match p.get("limit") {
        None | Some(Value::Null) => 25,
        Some(v) => v.as_u64().filter(|n| (1..=200).contains(n)).ok_or_else(|| bad(SUGGEST, "`limit` must be 1..=200"))?,
    };
    let args = json!({"from": from, "set": set, "direction": direction, "limit": limit});
    start_op(e, SUGGEST, "suggest", "suggest", args)
}

fn plan_set(e: &mut Engine, p: &Value) -> Result<Value> {
    let mut sources = paths_param(PLAN, p, "tracks")?;
    sources.extend(paths_param(PLAN, p, "source")?);
    if sources.is_empty() {
        return Err(bad(PLAN, "give `tracks` (paths) or `source` (a playlist, folder or files)"));
    }
    for s in &sources {
        crate::file_access::check_read(s).map_err(|why| bad(PLAN, why))?;
    }
    let out = match str_param(p, "out").filter(|s| !s.trim().is_empty()) {
        Some(o) => o.to_string(),
        None => default_plan_path(sources.first().map(String::as_str))?,
    };
    crate::file_access::check_write(&out).map_err(|why| bad(PLAN, why))?;
    let pick = |key: &str, allowed: &[&str], default: &str| -> Result<String> {
        match str_param(p, key) {
            None => Ok(default.to_string()),
            Some(v) if allowed.contains(&v) => Ok(v.to_string()),
            Some(_) => Err(bad(PLAN, format!("`{key}` must be one of {}", allowed.join(", ")))),
        }
    };
    let order = pick("order", &["keep", "harmonic", "rising"], "keep")?;
    let tempo = pick("tempo", &["ramp", "master"], "ramp")?;
    let style = pick("style", &["auto", "blend", "quick", "long", "filter", "echo"], "auto")?;
    let bpm = num(PLAN, p, "bpm", 60.0..=200.0)?;
    let bars = match p.get("bars") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().filter(|b| (1..=128).contains(b)).ok_or_else(|| bad(PLAN, "`bars` must be 1..=128"))?),
    };
    let minutes = num(PLAN, p, "minutes_per_track", 0.5..=60.0)?;
    let keep_variable = bool_param(PLAN, p, "keep_variable", Some(false))?;
    let args = json!({"sources": sources, "out": out, "order": order, "tempo": tempo, "style": style, "bpm": bpm, "bars": bars,
        "minutes_per_track": minutes, "keep_variable": keep_variable});
    start_op(e, PLAN, "plan", "plan", args)
}

/// Where a plan goes without `out`: `Music/djforge-plans/<source name>.plan.json` (beside prep's
/// folder), a number added when that file is there already.
fn default_plan_path(source: Option<&str>) -> Result<String> {
    let dir = super::prep::default_root().map_err(|_| bad(PLAN, "no Music folder known: give `out`"))?.with_file_name("djforge-plans");
    let stem = source.and_then(|s| std::path::Path::new(s).file_stem()).map_or_else(|| "set".to_string(), |s| s.to_string_lossy().into_owned());
    let mut path = dir.join(format!("{stem}.plan.json"));
    let mut n = 2;
    while crate::io::size_and_mtime(&*path.to_string_lossy()).is_some() && n < 1000 {
        path = dir.join(format!("{stem} {n}.plan.json"));
        n += 1;
    }
    Ok(path.to_string_lossy().into_owned())
}

fn analyze(e: &mut Engine, p: &Value) -> Result<Value> {
    let paths = paths_param(ANALYZE, p, "paths")?;
    if paths.is_empty() {
        return Err(bad(ANALYZE, "`paths` required: the files to analyse (library.health lists the ones that need it)"));
    }
    let paths: Vec<String> = paths.iter().map(|s| readable(ANALYZE, s)).collect::<Result<_>>()?;
    let force = bool_param(ANALYZE, p, "force", Some(false))?;
    let sections = bool_param(ANALYZE, p, "sections", Some(true))?;
    start_op(e, ANALYZE, "analyze", "analyze", json!({"paths": paths, "force": force, "sections": sections}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn wait(e: &mut Engine, v: &Value) -> Value {
        let id = v["job"].as_u64().unwrap();
        let start = std::time::Instant::now();
        loop {
            let s = e.execute("job.status", &json!({"job": id})).unwrap();
            if s["state"] != "running" && s["state"] != "stopping" {
                return s;
            }
            assert!(start.elapsed().as_secs() < 10, "{s}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn suggest_plan_and_analyze_run_mixforge_as_jobs_with_checked_params() {
        let dir = std::env::temp_dir().join(format!("djforge-agent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let track = dir.join("a.wav");
        std::fs::write(&track, b"x").unwrap();
        let track = track.to_string_lossy().into_owned();
        let mut e = Engine::default();
        // Without mixforge set up: refused, saying so.
        let err = e.execute("library.suggest", &json!({"from": track})).unwrap_err().to_string();
        assert!(err.contains("mixforge isn't set up"), "{err}");
        let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let log = Arc::clone(&seen);
        e.set_agent(Arc::new(move |op: &str, args: &Value, progress: &dyn Fn(Value), _stop: &dyn Fn() -> bool| {
            log.lock().unwrap().push((op.to_string(), args.clone()));
            progress(json!({"done": 1, "total": 1}));
            Ok(json!({"op": op}))
        }));
        let s = {
            let v = e.execute("library.suggest", &json!({"from": track, "direction": "up"})).unwrap();
            wait(&mut e, &v)
        };
        assert_eq!(s["result"]["op"], "suggest");
        assert_eq!(s["progress"]["done"], 1);
        let out = dir.join("set.plan.json").to_string_lossy().into_owned();
        let s = {
            let v = e.execute("dj.plan_set", &json!({"tracks": [track], "out": out, "order": "harmonic", "minutes_per_track": 5})).unwrap();
            wait(&mut e, &v)
        };
        assert_eq!(s["state"], "done");
        {
            let v = e.execute("library.analyze", &json!({"paths": [track]})).unwrap();
            wait(&mut e, &v)
        };
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls[0].1["direction"], "up");
        assert_eq!(calls[0].1["limit"], 25);
        assert_eq!(calls[1].1["order"], "harmonic");
        assert_eq!(calls[1].1["sources"][0], json!(track));
        assert_eq!(calls[2].1["sections"], true);
        // Bad parameters never reach mixforge.
        assert!(e.execute("library.suggest", &json!({"from": track, "direction": "sideways"})).is_err());
        assert!(e.execute("library.suggest", &json!({"from": dir.join("none.wav").to_string_lossy()})).is_err());
        assert!(e.execute("library.analyze", &json!({})).is_err());
        assert_eq!(seen.lock().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
