//! Model Context Protocol server (stdio, newline-delimited JSON-RPC 2.0).

use crate::Backend;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "djforge is a DJ-first digital audio workstation (based on SoundCraft, Pro Tools-class). Everything a user can do is a \
command: call `list_commands` to discover ids and their params, then `execute` (or `batch`). Positions are samples \
(integers), `{\"seconds\": x}`, or time strings like \"bars_beats:5|1|000\" / \"0:12.500\". Tracks are referenced by id \
or name. Verify your work with `inspect_session` (tracks, clips, mixer, automation, markers, selection). Programmatic \
calls never open dialogs. Tools marked APP_ONLY need the desktop app (`djforge --control 0` + `djforge-cli mcp --connect PORT`).

DJ work: plan and prepare sets and look after the library with the DJ tools (library_search, suggest_tracks, plan_set, \
import_set, set_overview, prep_tracks, export_set, library_health, analyze_tracks). A typical set: library_search (filter \
by BPM, Camelot keys, energy) or suggest_tracks to pick tracks that follow one another, plan_set to order and mix them \
with mixforge's planner (writes plan.json), new_session + import_set to build the set, set_overview to check its \
transitions, save_session, then prep_tracks / export_set to play it out. The DJ picks and listens: propose, explain \
your picks (key relation, tempo, energy), and leave the final say to them. Housekeeping is report-only: library_health \
lists what needs attention, and analyze_tracks re-analyses with mixforge (its own cache); never change tags or music \
files. Long work runs as a job: a tool waits up to wait_seconds, and if it is still running returns {job, progress}; \
call job_status to wait more, cancel_job to stop. The live decks are the DJ's: these tools don't play them.";

pub struct Server<B: Backend> {
    pub backend: B,
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": desc, "inputSchema": {"type": "object", "properties": props, "required": required}})
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "list_commands",
            "List djforge commands (id, label, menu path, shortcut, params doc, enabled).",
            json!({"filter": {"type": "string", "description": "substring of id or label"}}),
            &[],
        ),
        tool(
            "execute",
            "Run one command by id with JSON params. Returns the command's result.",
            json!({"command": {"type": "string"}, "params": {"type": "object"}}),
            &["command"],
        ),
        tool(
            "batch",
            "Run several commands in order; stops at the first error unless keep_going.",
            json!({"calls": {"type": "array", "items": {"type": "object", "properties": {"command": {"type": "string"}, "params": {"type": "object"}}}}, "keep_going": {"type": "boolean"}}),
            &["calls"],
        ),
        tool(
            "inspect_session",
            "The whole session as JSON: tracks (clips, inserts, sends, automation), busses, markers, groups, selection, transport.",
            json!({"detail": {"type": "string", "enum": ["summary", "full"]}}),
            &[],
        ),
        tool(
            "new_session",
            "Start a new session (optionally the built-in demo).",
            json!({"name": {"type": "string"}, "sample_rate": {"type": "integer"}, "template": {"type": "string", "enum": ["blank", "demo"]}}),
            &[],
        ),
        tool(
            "open_session",
            "Open a session file: .djforge, or a SoundCraft .scraft (saving it writes the .djforge beside it).",
            json!({"path": {"type": "string"}}),
            &["path"],
        ),
        tool(
            "save_session",
            "Save the session (path required the first time; replace: true to save over another existing file).",
            json!({"path": {"type": "string"}, "replace": {"type": "boolean"}}),
            &[],
        ),
        tool(
            "import_audio",
            "Import an audio file (WAV, AIFF, FLAC, MP3, OGG, AAC/M4A, ALAC, CAF) onto a new or existing track.",
            json!({"path": {"type": "string"}, "track": {"type": ["string", "integer"]}, "at": {}}),
            &["path"],
        ),
        tool(
            "bounce_mix",
            "Render the mix to an audio file. Returns peak dBFS and integrated LUFS.",
            json!({"path": {"type": "string"}, "format": {"type": "string", "enum": ["wav", "aiff", "flac"]}, "bit_depth": {"type": "string"}, "start": {}, "end": {}, "normalize": {"type": "boolean"}}),
            &["path"],
        ),
        tool(
            "library_search",
            "DJ. Search the decks' library (mixforge's analysed tracks, or a folder's audio files with `folder`), filtered by BPM, Camelot keys, energy and analysis. Rows: path, title, artist, bpm, key (Camelot), key_source, energy, length_s, analysed, stale, outdated, missing.",
            json!({"query": {"type": "string", "description": "words to find in title, artist or file name"}, "folder": {"type": "string"},
                "bpm_min": {"type": "number"}, "bpm_max": {"type": "number"}, "keys": {"type": "array", "items": {"type": "string"}, "description": "Camelot keys, e.g. [\"8A\", \"9A\"]"},
                "energy_min": {"type": "number"}, "energy_max": {"type": "number"}, "status": {"type": "string", "enum": ["any", "analysed", "unanalysed"]},
                "sort": {"type": "string", "enum": ["title", "artist", "bpm", "key", "energy", "length", "name"]}, "desc": {"type": "boolean"},
                "offset": {"type": "integer"}, "limit": {"type": "integer", "description": "1-1000, default 200"}}),
            &[],
        ),
        tool("library_folders", "DJ. The library's folders (the Music folder and those added) and mixforge's library file.", json!({}), &[]),
        tool(
            "suggest_tracks",
            "DJ. mixforge's next-track suggestions after one track (`from`), best first: key relation (same, ±1, relative, energy boosts, curveballs), tempo and energy direction, and tracks that sound like ones already in the set (`set`).",
            json!({"from": {"type": "string", "description": "path of the track to follow"}, "set": {"type": "array", "items": {"type": "string"}, "description": "paths already in the set (left out, and checked for same songs)"},
                "direction": {"type": "string", "enum": ["steady", "up", "down"], "description": "keep, raise or lower tempo and energy"}, "limit": {"type": "integer"}, "wait_seconds": {"type": "number"}}),
            &["from"],
        ),
        tool(
            "plan_set",
            "DJ. Plan a set with mixforge's planner from tracks, a playlist (.m3u/.m3u8) or a folder: order (keep, harmonic, rising), tempo (ramp or one master bpm), transitions. Analyses tracks mixforge hasn't yet. Writes plan.json to `out` and returns the running order, transitions and warnings (key clashes, same songs, off-grid outros).",
            json!({"tracks": {"type": "array", "items": {"type": "string"}}, "source": {"type": "string", "description": "a playlist, folder or file"}, "out": {"type": "string", "description": "where to write plan.json"},
                "order": {"type": "string", "enum": ["keep", "harmonic", "rising"]}, "tempo": {"type": "string", "enum": ["ramp", "master"]}, "bpm": {"type": "number"},
                "bars": {"type": "integer"}, "minutes_per_track": {"type": "number"}, "style": {"type": "string", "enum": ["auto", "blend", "quick", "long", "filter", "echo"]},
                "keep_variable": {"type": "boolean"}, "wait_seconds": {"type": "number"}}),
            &["out"],
        ),
        tool(
            "import_set",
            "DJ. Build the set from a mixforge plan.json in the open session (start one with new_session first): Deck A/B tracks beat-locked, the plan's tempo, each transition's EQ recipe. Then save_session.",
            json!({"plan": {"type": "string"}, "report": {"type": "string", "description": "path of a text report to write"}}),
            &["plan"],
        ),
        tool("set_overview", "DJ. The open set's transitions (from, to, bars, hit and gone bars). Changes nothing.", json!({}), &[]),
        tool(
            "prep_tracks",
            "DJ. Copies of a set's MP3s (a plan.json or a folder) level-matched without re-encoding, missing key/BPM tags filled; originals are never written. Returns each track's loudness before and after.",
            json!({"source": {"type": "string"}, "out": {"type": "string"}, "target_lufs": {"type": "number"}, "ceiling": {"type": "number"}, "wait_seconds": {"type": "number"}}),
            &["source"],
        ),
        tool(
            "export_set",
            "DJ. Export the open set for playing out: prepped copies plus a rekordbox XML (default). \"mixxx\" also writes Mixxx's library (a playlist, with a backup first; Mixxx must be closed) — only when the DJ asks for it.",
            json!({"targets": {"type": "array", "items": {"type": "string", "enum": ["rekordbox", "mixxx"]}}, "stems": {"type": "boolean"}, "out": {"type": "string"}, "wait_seconds": {"type": "number"}}),
            &[],
        ),
        tool(
            "library_health",
            "DJ. What in the library needs looking after: missing files, files changed since analysis (stale), old analyses, unanalysed files in the folders, likely duplicates, Mixed In Key/mixforge key disagreements, tracks lacking BPM/key/energy, and the files to re-analyse. Read only.",
            json!({"folders": {"type": "boolean"}, "limit": {"type": "integer", "description": "entries listed per kind (default 100)"}, "wait_seconds": {"type": "number"}}),
            &[],
        ),
        tool(
            "analyze_tracks",
            "DJ. Re-analyse files with mixforge (BPM, grid, key, loudness, sections), into mixforge's own cache. Slow (sections run in WSL): usually returns a running job; follow it with job_status.",
            json!({"paths": {"type": "array", "items": {"type": "string"}}, "force": {"type": "boolean"}, "sections": {"type": "boolean"}, "wait_seconds": {"type": "number"}}),
            &["paths"],
        ),
        tool(
            "job_status",
            "A job's state and progress; waits up to wait_seconds (default 60) for it to end, then gives its result.",
            json!({"job": {"type": "integer"}, "wait_seconds": {"type": "number"}}),
            &["job"],
        ),
        tool("cancel_job", "Ask a running job to stop.", json!({"job": {"type": "integer"}}), &["job"]),
        tool("parity", "Feature-parity report against the incumbent's menu catalog.", json!({}), &[]),
        tool("screenshot", "APP_ONLY. Screenshot the app window (PNG).", json!({"path": {"type": "string"}}), &[]),
        tool("ui_inspect", "APP_ONLY. UI state: front window, panels, layout rectangles of tracks, dialogs.", json!({}), &[]),
        tool("ui_set", "APP_ONLY. Set UI state fields, e.g. {\"window\": \"Mix\"}.", json!({"state": {"type": "object"}}), &["state"]),
        tool(
            "menu_invoke",
            "APP_ONLY. Invoke a menu item like a click (may open its dialog), e.g. \"Track > New...\".",
            json!({"path": {"type": "string"}}),
            &["path"],
        ),
        tool(
            "click",
            "APP_ONLY. Click at window coordinates (points).",
            json!({"x": {"type": "number"}, "y": {"type": "number"}, "button": {"type": "string"}, "count": {"type": "integer"}}),
            &["x", "y"],
        ),
        tool(
            "drag",
            "APP_ONLY. Drag with the primary button.",
            json!({"x": {"type": "number"}, "y": {"type": "number"}, "to_x": {"type": "number"}, "to_y": {"type": "number"}}),
            &["x", "y", "to_x", "to_y"],
        ),
        tool(
            "key",
            "APP_ONLY. Press a key with modifiers.",
            json!({"key": {"type": "string"}, "cmd": {"type": "boolean"}, "shift": {"type": "boolean"}, "alt": {"type": "boolean"}, "ctrl": {"type": "boolean"}}),
            &["key"],
        ),
    ]
}

fn text_result(v: &Value) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

fn error_result(e: &str) -> Value {
    json!({"content": [{"type": "text", "text": e}], "isError": true})
}

impl<B: Backend> Server<B> {
    pub fn new(backend: B) -> Self {
        Server { backend }
    }

    fn exec(&mut self, cmd: &str, params: Value) -> Result<Value, String> {
        self.backend.call("engine.execute", json!({"command": cmd, "params": params}))
    }

    /// [`Self::exec`], asked again while the libraries are first read (up to a minute): a
    /// listing then says `loading`, a scan that the libraries are being read.
    fn exec_library(&mut self, cmd: &str, params: Value) -> Result<Value, String> {
        let start = Instant::now();
        loop {
            let r = self.exec(cmd, params.clone());
            let reading = match &r {
                Ok(v) => v.get("loading").and_then(Value::as_bool) == Some(true),
                Err(e) => e.contains("being read for the first time"),
            };
            if !reading || start.elapsed() > Duration::from_secs(60) {
                return r;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Runs a command that starts a job and waits for it (see [`Self::wait_job`]).
    fn run_job(&mut self, cmd: &str, params: Value, wait: Duration) -> Result<Value, String> {
        let started = self.exec_library(cmd, params)?;
        match started.get("job").and_then(Value::as_u64) {
            Some(id) => self.wait_job(id, wait),
            None => Ok(started),
        }
    }

    /// Waits up to `wait` for job `id`: its result when done, its error when it failed or was
    /// cancelled, else `{job, state, progress, note}` to wait again with job_status.
    fn wait_job(&mut self, id: u64, wait: Duration) -> Result<Value, String> {
        let start = Instant::now();
        loop {
            let s = self.exec("job.status", json!({"job": id}))?;
            match s.get("state").and_then(Value::as_str) {
                Some("done") => return Ok(s.get("result").cloned().unwrap_or(Value::Null)),
                Some("failed" | "cancelled") => {
                    let why = s.get("error").and_then(Value::as_str).unwrap_or("the job failed");
                    return Err(format!("job {id} {}: {why}", s.get("state").and_then(Value::as_str).unwrap_or("failed")));
                }
                _ if start.elapsed() >= wait => {
                    return Ok(json!({"job": id, "state": s.get("state"), "progress": s.get("progress"), "seconds": s.get("seconds"),
                        "note": "still running: call job_status with this job to wait more, or cancel_job to stop it"}));
                }
                _ => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    /// Dispatch a tool call.
    pub fn call_tool(&mut self, name: &str, args: &Value) -> Value {
        let a = |k: &str| args.get(k).cloned();
        // How long a job tool waits before handing back the job (default 120 s, at most 15 min).
        let wait = |default: f64| {
            let s = args.get("wait_seconds").and_then(Value::as_f64).filter(|s| s.is_finite()).unwrap_or(default).clamp(0.0, 900.0);
            Duration::from_secs_f64(s)
        };
        // A tool's arguments as a command's params: its own options left out.
        let params = |drop: &[&str]| {
            let mut p = args.clone();
            if let Some(o) = p.as_object_mut() {
                for k in drop {
                    o.remove(*k);
                }
            }
            p
        };
        let r: Result<Value, String> = match name {
            "list_commands" => self.backend.call("engine.commands", json!({"filter": a("filter")})),
            "execute" => {
                let Some(cmd) = args.get("command").and_then(Value::as_str) else { return error_result("`command` required") };
                self.exec(cmd, a("params").unwrap_or(json!({})))
            }
            "batch" => {
                let keep = args.get("keep_going").and_then(Value::as_bool).unwrap_or(false);
                let Some(calls) = args.get("calls").and_then(Value::as_array) else {
                    return error_result("`calls` must be an array");
                };
                let mut out = Vec::new();
                let mut failed = false;
                for c in calls.iter().take(1000) {
                    let Some(cmd) = c.get("command").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
                        out.push(json!({"command": "", "ok": false, "error": "`command` required"}));
                        failed = true;
                        if !keep {
                            break;
                        }
                        continue;
                    };
                    let r = self.exec(cmd, c.get("params").cloned().unwrap_or(json!({})));
                    match r {
                        Ok(v) => out.push(json!({"command": cmd, "ok": true, "result": v})),
                        Err(e) => {
                            out.push(json!({"command": cmd, "ok": false, "error": e}));
                            failed = true;
                            if !keep {
                                break;
                            }
                        }
                    }
                }
                // Any failure sets isError so agents that only check that flag notice;
                // keep_going still runs the rest and the payload lists every call.
                if failed { Err(serde_json::to_string(&out).unwrap_or_default()) } else { Ok(json!(out)) }
            }
            "library_search" => {
                let mut p = params(&["folder"]);
                if let Some(f) = a("folder").filter(|f| !f.is_null()) {
                    p["source"] = json!("folder");
                    p["folder"] = f;
                }
                let has_query = p.get("query").and_then(Value::as_str).is_some_and(|q| !q.trim().is_empty());
                self.exec_library(if has_query { "library.search" } else { "library.list" }, p)
            }
            "library_folders" => self.exec("library.folders", json!({})),
            "suggest_tracks" => self.run_job("library.suggest", params(&["wait_seconds"]), wait(120.0)),
            "plan_set" => self.run_job("dj.plan_set", params(&["wait_seconds"]), wait(300.0)),
            "import_set" => self.exec("dj.import_set", args.clone()),
            "set_overview" => self.exec("dj.transitions", json!({})),
            "prep_tracks" => {
                let mut p = params(&["wait_seconds"]);
                p["background"] = json!(true);
                self.run_job("prep.tracks", p, wait(300.0))
            }
            "export_set" => {
                let mut p = params(&["wait_seconds"]);
                if p.get("targets").is_none_or(Value::is_null) {
                    p["targets"] = json!(["rekordbox"]);
                }
                p["background"] = json!(true);
                self.run_job("dj.export_set", p, wait(300.0))
            }
            "library_health" => self.run_job("library.health", params(&["wait_seconds"]), wait(120.0)),
            "analyze_tracks" => self.run_job("library.analyze", params(&["wait_seconds"]), wait(30.0)),
            "job_status" => match args.get("job").and_then(Value::as_u64) {
                Some(id) => self.wait_job(id, wait(60.0)),
                None => Err("`job` required".into()),
            },
            "cancel_job" => self.exec("job.cancel", json!({"job": a("job")})),
            "inspect_session" => self.backend.call("session.inspect", json!({"detail": a("detail")})),
            "new_session" => self.exec("session.new", args.clone()),
            "open_session" => self.exec("session.open", args.clone()),
            "save_session" => self.exec("session.save", args.clone()),
            "import_audio" => self.exec("file.import_audio", args.clone()),
            "bounce_mix" => self.exec("file.bounce_mix", args.clone()),
            "parity" => self.backend.call("engine.parity", json!({})),
            "screenshot" => {
                let r = self.backend.call("ui.screenshot", args.clone());
                if let Ok(v) = &r
                    && let Some(b64) = v.get("png_base64").and_then(Value::as_str)
                {
                    return json!({"content": [{"type": "image", "data": b64, "mimeType": "image/png"}], "isError": false});
                }
                r
            }
            "ui_inspect" => self.backend.call("ui.inspect", json!({})),
            "ui_set" => self.backend.call("ui.set", a("state").unwrap_or(json!({}))),
            "menu_invoke" => self.backend.call("ui.menu.invoke", args.clone()),
            "click" => self.backend.call("ui.click", args.clone()),
            "drag" => self.backend.call("ui.drag", args.clone()),
            "key" => self.backend.call("ui.key", args.clone()),
            other => Err(format!("unknown tool `{other}`")),
        };
        match r {
            Ok(v) => text_result(&v),
            Err(e) => error_result(&e),
        }
    }

    /// Handle one JSON-RPC message; returns the reply line (None for notifications).
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let id = id?;
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                let want = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
                let ver = if SUPPORTED.contains(&want) { want } else { PROTOCOL_VERSION };
                Ok(json!({
                    "protocolVersion": ver,
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "djforge", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": format!("{INSTRUCTIONS}\nBackend: {}.", self.backend.describe()),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_definitions()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(self.call_tool(name, &args))
            }
            // djforge's own URIs; `soundcraft://` ones still read the same, for clients written
            // against SoundCraft.
            "resources/list" => Ok(json!({"resources": [
                {"uri": "djforge://session", "name": "Session", "mimeType": "application/json", "description": "The open session as JSON"},
                {"uri": "djforge://commands", "name": "Commands", "mimeType": "application/json", "description": "All commands"},
            ]})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                let r = match uri {
                    "djforge://session" | "soundcraft://session" => self.backend.call("session.inspect", json!({"detail": "full"})),
                    "djforge://commands" | "soundcraft://commands" => self.backend.call("engine.commands", json!({})),
                    _ => Err(format!("unknown resource {uri}")),
                };
                match r {
                    Ok(v) => Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": v.to_string()}]})),
                    Err(e) => Err((-32002, e)),
                }
            }
            "" => Err((-32600, "invalid request".into())),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, m)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": m}}),
        })
    }

    /// Serve until EOF.
    pub fn serve(&mut self, input: impl BufRead, mut out: impl Write) -> std::io::Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let reply = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Array(batch)) => {
                    let replies: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
                    if replies.is_empty() { None } else { Some(Value::Array(replies)) }
                }
                Ok(msg) => self.handle(&msg),
                Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
            };
            if let Some(r) = reply {
                writeln!(out, "{r}")?;
                out.flush()?;
            }
        }
        Ok(())
    }
}
