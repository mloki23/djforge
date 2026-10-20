//! `prep.tracks`: level-matched, tagged copies of a set's or a folder's MP3s. Each copy's frames
//! get a lossless global_gain change (whole 1.5 dB steps, no re-encode) towards a loudness target
//! without passing a true-peak ceiling; missing key and BPM tags are filled from mixforge's
//! analysis. The originals are only read.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_audio_io::mp3gain::{self, STEP_DB};
use soundcraft_audio_io::mp3tags::{self, TagFill};
use soundcraft_audio_io::{AudioBuffer, decode};
use soundcraft_dsp::meter::LoudnessMeter;
use soundcraft_model::Session;
use std::path::{Path, PathBuf};

const CMD: &str = "prep.tracks";
/// The report written next to the copies.
const REPORT: &str = "prep-report.txt";
/// Integrated loudness at or below this is too quiet to measure (the meter's absolute gate).
const QUIET_LUFS: f64 = -70.0;
/// Tracks within this many dB of the target count as on target in the totals.
const ON_TARGET_DB: f64 = 0.75;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        noundo "prep.tracks",
        "Prep Tracks",
        [],
        None,
        "{source: a mixforge plan.json or a folder of MP3s, out?: folder for the prepped copies (default Music/djforge-prepped/<set or folder name>), target_lufs?: -14, ceiling?: -1} — copies every MP3 to `out` level-matched without re-encoding (global_gain steps of 1.5 dB, never above the ceiling in dBTP) with missing key/BPM tags filled from mixforge's analysis; originals are never written. Writes out/prep-report.txt. Returns {out, target_lufs, ceiling, tracks: [{file, lufs_before, tp_before, steps, gain_db, lufs_after, tp_after, limited_by, filled, doubts, status: ok|not_adjusted|failed}], totals}",
        always,
        prep_tracks
    )]
}

/// A number parameter: `d` when absent, an error when not a finite number in `range`.
fn num(p: &Value, key: &str, d: f64, range: std::ops::RangeInclusive<f64>) -> Result<f64> {
    let Some(v) = p.get(key) else { return Ok(d) };
    v.as_f64()
        .filter(|v| v.is_finite() && range.contains(v))
        .ok_or_else(|| bad(CMD, format!("`{key}` must be a number from {} to {}", range.start(), range.end())))
}

fn prep_tracks(e: &mut Engine, p: &Value) -> Result<Value> {
    let source = str_param(p, "source").ok_or_else(|| bad(CMD, "`source` required: a mixforge plan.json or a folder of MP3s"))?;
    let out = str_param(p, "out");
    let target = num(p, "target_lufs", -14.0, -40.0..=0.0)?;
    let ceiling = num(p, "ceiling", -1.0, -20.0..=0.0)?;
    let analyse = |path: &str| {
        super::dj::analyze_file(CMD, path).map_err(|err| match err {
            EngineError::BadParams(_, msg) => msg,
            other => other.to_string(),
        })
    };
    run(e.session(), source, out, target, ceiling, &analyse)
}

/// The tracks of `source` (a plan.json, or a folder's MP3s sorted by name) and the name the
/// default output folder takes (the plan's folder, or the folder).
fn sources(source: &str) -> Result<(String, Vec<PathBuf>)> {
    let path = Path::new(source);
    let folder_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
    let is_plan = path.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("json"));
    if is_plan {
        let plan = super::dj::plan::parse_plan(&crate::io::read_text(path)?).map_err(|m| bad(CMD, m))?;
        let dir = path.parent().unwrap_or(Path::new(""));
        let files = plan
            .tracks
            .iter()
            .map(|t| {
                let p = Path::new(&t.path);
                if p.is_relative() { dir.join(p) } else { p.to_path_buf() }
            })
            .collect();
        let name = std::path::absolute(path).ok().and_then(|p| p.parent().and_then(folder_name)).unwrap_or_else(|| "set".to_string());
        return Ok((name, files));
    }
    let mut files: Vec<PathBuf> = crate::io::list_files(path)
        .map_err(|err| EngineError::Io(format!("{err} (`source` must be a plan.json or a folder of MP3s)")))?
        .into_iter()
        .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3")))
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err(bad(CMD, format!("{source}: holds no MP3s")));
    }
    let name = std::path::absolute(path).ok().as_deref().and_then(folder_name).unwrap_or_else(|| "folder".to_string());
    Ok((name, files))
}

/// `Music/djforge-prepped/<name>` in the home folder.
fn default_out(name: &str) -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(var).filter(|h| !h.is_empty()).ok_or_else(|| bad(CMD, format!("{var} is not set; pass `out`")))?;
    Ok(PathBuf::from(home).join("Music").join("djforge-prepped").join(name))
}

/// Where each track's copy goes (`out/<file name>`), once every refusal and access check passed:
/// `out` is not a source's folder, no two sources share a name, and no copy (or the report)
/// would land on a source or a session's audio.
fn destinations(s: &Session, files: &[PathBuf], out: &Path) -> Result<Vec<PathBuf>> {
    let key = crate::io::file_key;
    let out_key = key(out);
    let source_keys: std::collections::HashSet<PathBuf> = files.iter().map(|f| key(f)).collect();
    let mut names: std::collections::HashMap<String, &Path> = std::collections::HashMap::new();
    let mut dests = Vec::with_capacity(files.len());
    for f in files {
        crate::file_access::check_read(f).map_err(EngineError::Io)?;
        let name = f.file_name().and_then(|n| n.to_str()).ok_or_else(|| bad(CMD, format!("{}: not a file name", f.display())))?;
        if f.parent().is_some_and(|p| key(p) == out_key) {
            return Err(bad(
                CMD,
                format!("`out` is the folder of source {}; choose another folder so the originals are never overwritten", f.display()),
            ));
        }
        let lower = name.to_lowercase();
        if lower == REPORT {
            return Err(bad(CMD, format!("{}: is named like the report; rename it", f.display())));
        }
        if let Some(other) = names.insert(lower, f) {
            return Err(bad(CMD, format!("two tracks share the name `{name}` ({} and {}); neither can be written", other.display(), f.display())));
        }
        dests.push(out.join(name));
    }
    for d in dests.iter().chain([&out.join(REPORT)]) {
        if source_keys.contains(&key(d)) {
            return Err(bad(CMD, format!("{}: is a source file; choose another `out`", d.display())));
        }
        crate::io::refuse_source_file(s, d)?;
        crate::file_access::check_write(d).map_err(EngineError::Io)?;
    }
    crate::file_access::check_write(out).map_err(EngineError::Io)?;
    Ok(dests)
}

/// Preps the tracks of `source` (a mixforge plan.json or a folder of MP3s) into `out` (default:
/// `Music/djforge-prepped/<name>`): see `prep.tracks`. `analyse` gives mixforge's analysis
/// (`camelot`, `bpm`) of a track's path. Writes go through `s`'s refusals; nothing is written
/// until every check has passed, and the originals are only read.
pub fn run(
    s: &Session,
    source: &str,
    out: Option<&str>,
    target_lufs: f64,
    ceiling: f64,
    analyse: &dyn Fn(&str) -> std::result::Result<Value, String>,
) -> Result<Value> {
    let started = std::time::Instant::now();
    if !(target_lufs.is_finite() && ceiling.is_finite()) {
        return Err(bad(CMD, "`target_lufs` and `ceiling` must be numbers"));
    }
    let (name, files) = sources(source)?;
    let out = match out {
        Some(o) => PathBuf::from(o),
        None => default_out(&name)?,
    };
    let dests = destinations(s, &files, &out)?;
    crate::io::make_dir(&out)?;

    // One track at a time, so only one track's audio is in memory.
    let rows: Vec<Row> = files.iter().zip(&dests).map(|(src, dest)| prep_one(s, src, dest, target_lufs, ceiling, analyse)).collect();

    let count = |st: &str| rows.iter().filter(|r| r.status == st).count();
    let written = || rows.iter().filter(|r| r.status != "failed");
    let measured = written().filter(|r| r.lufs_after.is_some()).count();
    let on_target = written().filter(|r| r.lufs_after.is_some_and(|l| (l - target_lufs).abs() <= ON_TARGET_DB)).count();
    let over = written().filter(|r| r.tp_after.is_some_and(|tp| tp > ceiling)).count();
    let totals = json!({
        "ok": count("ok"),
        "not_adjusted": count("not_adjusted"),
        "failed": count("failed"),
        "measured": measured,
        "on_target": on_target,
        "over_ceiling": over,
        "seconds": started.elapsed().as_secs_f64(),
    });
    let report_path = out.join(REPORT);
    let r = json!({
        "source": source,
        "out": out.to_string_lossy(),
        "report": report_path.to_string_lossy(),
        "target_lufs": target_lufs,
        "ceiling": ceiling,
        "tracks": rows.iter().map(Row::json).collect::<Vec<_>>(),
        "totals": totals,
    });
    let path = report_path.to_str().ok_or_else(|| EngineError::Io(format!("{}: not a Unicode path", report_path.display())))?;
    crate::io::write_user_file(s, path, text_report(&r, &rows, utc_now()).as_bytes())?;
    Ok(r)
}

/// One track's outcome.
#[derive(Debug, Clone, Default)]
struct Row {
    file: String,
    lufs_before: Option<f64>,
    tp_before: Option<f64>,
    steps: i32,
    lufs_after: Option<f64>,
    tp_after: Option<f64>,
    limited_by: Option<&'static str>,
    filled: Vec<&'static str>,
    doubts: Vec<String>,
    status: &'static str,
}

impl Row {
    fn not_adjusted(&mut self, why: impl std::fmt::Display) {
        self.status = "not_adjusted";
        self.steps = 0;
        self.doubts.push(format!("not adjusted: {why}"));
    }

    fn failed(&mut self, why: impl std::fmt::Display) {
        self.status = "failed";
        self.doubts.push(format!("failed: {why}"));
    }

    fn gain_db(&self) -> f64 {
        f64::from(self.steps) * STEP_DB
    }

    fn json(&self) -> Value {
        json!({
            "file": self.file,
            "lufs_before": self.lufs_before,
            "tp_before": self.tp_before,
            "steps": self.steps,
            "gain_db": self.gain_db(),
            "lufs_after": self.lufs_after,
            "tp_after": self.tp_after,
            "limited_by": self.limited_by,
            "filled": self.filled,
            "doubts": self.doubts,
            "status": self.status,
        })
    }
}

/// Integrated loudness (LUFS) and true peak (dBTP).
#[derive(Debug, Clone, Copy)]
struct Level {
    lufs: f64,
    tp: f64,
}

/// The decoded audio of MP3 `bytes` and its level.
fn measure(bytes: &[u8]) -> std::result::Result<(AudioBuffer, Level), String> {
    let (_, buf) = decode(bytes, Some("mp3")).map_err(|err| err.to_string())?;
    let frames = buf.channels.first().map_or(0, Vec::len);
    if frames == 0 {
        return Err("no audio".into());
    }
    let mut meter = LoudnessMeter::new(buf.sample_rate as f32, buf.channels.len());
    meter.process(&buf.channels, frames);
    let level = Level { lufs: f64::from(meter.integrated_lufs()), tp: f64::from(meter.true_peak_db()) };
    Ok((buf, level))
}

/// Whether `y` is `x` scaled by exactly `steps` global_gain steps: the same shape, a least-squares
/// ratio within 0.01 dB of the step, and nowhere off that ratio by more than 1e-3 of the peak (so
/// a level change part-way through fails even when the overall ratio is close).
fn verify(x: &AudioBuffer, y: &AudioBuffer, steps: i32) -> std::result::Result<(), String> {
    if x.channels.len() != y.channels.len() || x.channels.iter().zip(&y.channels).any(|(a, b)| a.len() != b.len()) {
        return Err("the copy decodes to a different length".into());
    }
    let pairs = || x.channels.iter().zip(&y.channels).flat_map(|(a, b)| a.iter().zip(b)).map(|(&a, &b)| (f64::from(a), f64::from(b)));
    let (xy, xx, peak) = pairs().fold((0.0, 0.0, 0.0f64), |(xy, xx, pk), (a, b)| (xy + a * b, xx + a * a, pk.max(a.abs()).max(b.abs())));
    if !(xx > 0.0 && xy > 0.0) {
        return Err("the audio is silent or inverted".into());
    }
    let ratio = xy / xx;
    let off_db = 20.0 * ratio.log10() - f64::from(steps) * STEP_DB;
    if off_db.is_nan() || off_db.abs() >= 0.01 {
        return Err(format!("the copy is {off_db:+.3} dB off the {:+.1} dB step", f64::from(steps) * STEP_DB));
    }
    let residual = pairs().map(|(a, b)| (b - ratio * a).abs()).fold(0.0, f64::max);
    if residual > 1e-3 * peak {
        return Err(format!("the copy is not one gain throughout (off by up to {residual:.2e})"));
    }
    Ok(())
}

/// The gain-adjusted copy of MP3 `bytes`, filling `row`'s levels; the bytes unchanged when they
/// can't be adjusted, `None` when the adjusted copy fails verification.
fn level(bytes: &[u8], target: f64, ceiling: f64, row: &mut Row) -> Option<Vec<u8>> {
    let (x, before) = match measure(bytes) {
        Ok(m) => m,
        Err(m) => {
            row.not_adjusted(format!("does not decode ({m})"));
            return Some(bytes.to_vec());
        }
    };
    (row.lufs_before, row.tp_before, row.lufs_after, row.tp_after) = (Some(before.lufs), Some(before.tp), Some(before.lufs), Some(before.tp));
    let range = match mp3gain::step_range(bytes) {
        Ok(r) => r,
        Err(err) => {
            row.not_adjusted(err);
            return Some(bytes.to_vec());
        }
    };
    let lufs = if before.lufs > QUIET_LUFS { before.lufs } else { f64::NAN };
    let pick = pick_steps(lufs, before.tp, target, ceiling, range);
    if pick.limited_by == Some("unmeasurable") {
        row.not_adjusted("too quiet to measure");
        return Some(bytes.to_vec());
    }
    row.limited_by = pick.limited_by;
    if pick.steps == 0 {
        return Some(bytes.to_vec());
    }
    let adjusted = match mp3gain::adjust(bytes, pick.steps) {
        Ok(a) => a,
        Err(err) => {
            row.limited_by = None;
            row.not_adjusted(err);
            return Some(bytes.to_vec());
        }
    };
    let checked = measure(&adjusted).and_then(|(y, after)| verify(&x, &y, pick.steps).map(|()| after));
    drop(x);
    match checked {
        Ok(after) => {
            row.steps = pick.steps;
            (row.lufs_after, row.tp_after) = (Some(after.lufs), Some(after.tp));
            Some(adjusted)
        }
        Err(m) => {
            row.failed(format!("the adjusted copy did not verify: {m}"));
            None
        }
    }
}

/// `1A`–`12B`, the Camelot keys Mixed In Key writes.
fn camelot(k: &str) -> Option<String> {
    let k = k.trim().to_ascii_uppercase();
    let n = k.strip_suffix(['A', 'B'])?;
    (!n.starts_with('0') && n.parse::<u8>().is_ok_and(|n| (1..=12).contains(&n))).then_some(k.clone())
}

/// mixforge's key and BPM for `src`, when usable; the doubts otherwise.
fn analysis(src: &Path, analyse: &dyn Fn(&str) -> std::result::Result<Value, String>, doubts: &mut Vec<String>) -> (Option<String>, Option<f64>) {
    let Some(path) = src.to_str() else {
        doubts.push("no mixforge analysis: the path is not Unicode".into());
        return (None, None);
    };
    let a = match analyse(path) {
        Ok(a) => a,
        Err(m) => {
            doubts.push(format!("no mixforge analysis: {m}"));
            return (None, None);
        }
    };
    let key = match a.get("camelot").and_then(Value::as_str) {
        Some(k) => camelot(k).or_else(|| {
            doubts.push(format!("mixforge's key `{k}` is not a Camelot key (not filled)"));
            None
        }),
        None => None,
    };
    let bpm = match a.get("bpm").and_then(Value::as_f64) {
        Some(b) if b.is_finite() && (20.0..=400.0).contains(&b) => Some(b),
        Some(b) => {
            doubts.push(format!("mixforge's BPM {b} is out of range (not filled)"));
            None
        }
        None => None,
    };
    (key, bpm)
}

/// `-3.0` style dB, or `?` when unknown.
fn db(v: Option<f64>) -> String {
    v.filter(|v| v.is_finite()).map_or_else(|| "?".to_string(), |v| format!("{v:.1}"))
}

/// The `TXXX:djforge_prep` note: `gain -3.0 dB; -10.9 -> -13.9 LUFS; TP -2.7 -> -5.7 dBTP; target -14`.
fn prep_note(row: &Row, target: f64) -> String {
    format!(
        "gain {:+.1} dB; {} -> {} LUFS; TP {} -> {} dBTP; target {target}",
        row.gain_db(),
        db(row.lufs_before),
        db(row.lufs_after),
        db(row.tp_before),
        db(row.tp_after)
    )
}

/// Preps one track from `src` to `dest`.
fn prep_one(s: &Session, src: &Path, dest: &Path, target: f64, ceiling: f64, analyse: &dyn Fn(&str) -> std::result::Result<Value, String>) -> Row {
    let mut row = Row { file: dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), status: "ok", ..Row::default() };
    let bytes = match crate::io::read_file(src) {
        Ok(b) => b,
        Err(err) => {
            row.failed(err);
            return row;
        }
    };
    let is_mp3 = src.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3"));
    let copy = if is_mp3 {
        let Some(adjusted) = level(&bytes, target, ceiling, &mut row) else { return row };
        drop(bytes);
        if let (Some(why), Some(after)) = (row.limited_by, row.lufs_after) {
            row.doubts.insert(0, format!("{why}: reaches {after:.1} LUFS"));
        }
        tagged(adjusted, src, target, &mut row, analyse)
    } else {
        row.not_adjusted("not an MP3");
        bytes
    };
    let written = dest
        .to_str()
        .ok_or_else(|| EngineError::Io(format!("{}: not a Unicode path", dest.display())))
        .and_then(|d| crate::io::write_user_file(s, d, &copy));
    if let Err(err) = written {
        row.failed(err);
    }
    row
}

/// `adjusted` with missing key and BPM tags filled from mixforge and the prep note added; with its
/// tags untouched when they can't be rewritten. Notes what the tags say in `row`.
fn tagged(adjusted: Vec<u8>, src: &Path, target: f64, row: &mut Row, analyse: &dyn Fn(&str) -> std::result::Result<Value, String>) -> Vec<u8> {
    let (key, bpm) = analysis(src, analyse, &mut row.doubts);
    let note = prep_note(row, target);
    let (bytes, tags) = match mp3tags::fill(&adjusted, &TagFill { camelot: key.as_deref(), bpm, prep_note: &note }) {
        Ok(x) => x,
        Err(err) => {
            row.doubts.push(format!("tags left as they are: {err}"));
            return adjusted;
        }
    };
    drop(adjusted);
    row.filled = tags.filled.clone();
    if let (Some(file), Some(mf)) = (&tags.existing_key, &key)
        && !file.trim().eq_ignore_ascii_case(mf)
    {
        row.doubts.push(format!("key: file {}, mixforge {mf} (file kept)", file.trim()));
    }
    if let (Some(file), Some(mf)) = (tags.existing_bpm.as_deref().and_then(|b| b.trim().parse::<f64>().ok()), bpm)
        && (file - mf).abs() > 1.0
    {
        let twice = (file - 2.0 * mf).abs() <= 1.0 || (file - mf / 2.0).abs() <= 1.0;
        row.doubts.push(format!("bpm: file {file}, mixforge {mf:.1}{} (file kept)", if twice { " (half/double)" } else { "" }));
    }
    if !tags.unreadable.is_empty() {
        row.doubts.push(format!("unreadable tag frames (kept as they are): {}", tags.unreadable.join(", ")));
    }
    if tags.energy.is_none() {
        row.doubts.push("no energy tag".into());
    }
    if let Some(g) = tags.genre.as_deref().filter(|g| g.trim().eq_ignore_ascii_case("Music")) {
        row.doubts.push(format!("junk genre `{}`", g.trim()));
    }
    if !tags.album {
        row.doubts.push("no album".into());
    }
    bytes
}

/// The plain-text report: a header, one line per track, then the totals.
fn text_report(r: &Value, rows: &[Row], date: String) -> String {
    let num = |k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let src = r.get("source").and_then(Value::as_str).unwrap_or("");
    let mut out = format!("djforge track prep: {src}\ntarget {} LUFS, ceiling {} dBTP, {date}\n\n", num("target_lufs"), num("ceiling"));
    for (i, row) in rows.iter().enumerate() {
        let filled = if row.filled.is_empty() { "-".to_string() } else { row.filled.join(",") };
        out.push_str(&format!(
            "{:>2}  {}  before {} LUFS / {} dBTP  gain {:+.1} dB  after {} / {}  filled {filled}  | {}\n",
            i + 1,
            row.file,
            db(row.lufs_before),
            db(row.tp_before),
            row.gain_db(),
            db(row.lufs_after),
            db(row.tp_after),
            row.doubts.join("; ")
        ));
    }
    let t = r.get("totals").cloned().unwrap_or(Value::Null);
    let int = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    out.push_str(&format!(
        "\n{} ok, {} not adjusted, {} failed; {} of {} within ±{ON_TARGET_DB} dB of the target; {} above the ceiling; {:.1} s\n",
        int("ok"),
        int("not_adjusted"),
        int("failed"),
        int("on_target"),
        int("measured"),
        int("over_ceiling"),
        t.get("seconds").and_then(Value::as_f64).unwrap_or(0.0)
    ));
    out
}

/// The current date and time, `YYYY-MM-DD HH:MM UTC`.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970-01-01 to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3_600, rem % 3_600 / 60)
}

/// The gain step picked for a track, and what held it back from the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pick {
    pub steps: i32,
    /// `"peaks"` (the ceiling), `"global_gain"` (the frames' 0..=255 room) or `"unmeasurable"`.
    pub limited_by: Option<&'static str>,
}

/// Steps of [`STEP_DB`] that bring integrated loudness `lufs` nearest `target` without taking
/// true peak `tp` above `ceiling` (a track already above it is cut, even below the target), within
/// the frames' step `range` (most negative, most positive).
pub(crate) fn pick_steps(lufs: f64, tp: f64, target: f64, ceiling: f64, range: (i32, i32)) -> Pick {
    if ![lufs, tp, target, ceiling].iter().all(|v| v.is_finite()) {
        return Pick { steps: 0, limited_by: Some("unmeasurable") };
    }
    // Finite inputs can still give steps far outside any frame's room; `as` saturates them.
    let want = ((target - lufs) / STEP_DB).round().clamp(-1e6, 1e6) as i32;
    let peak_max = ((ceiling - tp) / STEP_DB).floor().clamp(-1e6, 1e6) as i32;
    let (mut steps, mut limited_by) = if peak_max < want { (peak_max, Some("peaks")) } else { (want, None) };
    // Not `clamp`: that panics on a reversed range.
    let clamped = steps.max(range.0).min(range.1);
    if clamped != steps {
        (steps, limited_by) = (clamped, Some("global_gain"));
    }
    Pick { steps, limited_by }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boost_stops_at_the_ceiling() {
        assert_eq!(pick_steps(-17.0, -4.0, -14.0, -1.0, (-50, 50)), Pick { steps: 1, limited_by: Some("peaks") });
    }

    #[test]
    fn a_cut_reaches_the_nearest_step() {
        assert_eq!(pick_steps(-10.9, -2.7, -14.0, -1.0, (-50, 50)), Pick { steps: -2, limited_by: None });
    }

    #[test]
    fn a_hot_track_is_cut_below_the_target_to_clear_the_ceiling() {
        assert_eq!(pick_steps(-12.0, 2.0, -14.0, -1.0, (-50, 50)), Pick { steps: -2, limited_by: Some("peaks") });
    }

    #[test]
    fn the_frames_room_clamps_the_step() {
        assert_eq!(pick_steps(-10.9, -2.7, -14.0, -1.0, (-1, 5)), Pick { steps: -1, limited_by: Some("global_gain") });
        assert_eq!(pick_steps(-20.0, -15.0, -14.0, -1.0, (-5, 2)), Pick { steps: 2, limited_by: Some("global_gain") });
    }

    fn tone(gain: impl Fn(usize) -> f32) -> AudioBuffer {
        let ch = |phase: f32| (0..4_800).map(|n| gain(n) * (n as f32 * 0.05 + phase).sin() * 0.5).collect();
        AudioBuffer { sample_rate: 48_000, channels: vec![ch(0.0), ch(1.0)] }
    }

    #[test]
    fn verify_accepts_an_exact_step_and_nothing_else() {
        let x = tone(|_| 1.0);
        let step = 10f32.powf((2.0 * STEP_DB / 20.0) as f32);
        assert_eq!(verify(&x, &tone(|_| step), 2), Ok(()));
        assert!(verify(&x, &tone(|_| step), 1).is_err());
        // Two steps for the first half, three for the second: the overall ratio is near a step,
        // but the level changes part-way.
        let three = 10f32.powf((3.0 * STEP_DB / 20.0) as f32);
        let part = tone(|n| if n < 2_400 { step } else { three });
        assert_eq!((1..=3).map(|k| verify(&x, &part, k)).find(|r| r.is_ok()), None);
        // The last 8 samples left as they were (a frame or two not adjusted): within 0.01 dB
        // overall, so only the residual rule rejects it.
        let tail = tone(|n| if n < 4_792 { step } else { 1.0 });
        let (xy, xx) = x
            .channels
            .iter()
            .zip(&tail.channels)
            .flat_map(|(a, b)| a.iter().zip(b))
            .fold((0.0, 0.0), |(xy, xx), (&a, &b)| (xy + f64::from(a) * f64::from(b), xx + f64::from(a) * f64::from(a)));
        assert!((20.0 * (xy / xx).log10() - 2.0 * STEP_DB).abs() < 0.01);
        assert!(verify(&x, &tail, 2).unwrap_err().contains("not one gain"));
        assert!(verify(&x, &AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 4_800]; 2] }, 2).is_err());
        assert!(verify(&x, &AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 10]; 2] }, 2).is_err());
    }

    #[test]
    fn camelot_keys_are_normalised_or_refused() {
        assert_eq!(camelot(" 4b "), Some("4B".to_string()));
        assert_eq!(camelot("12A"), Some("12A".to_string()));
        for bad in ["13A", "0A", "04B", "A", "4C", "", "Dm"] {
            assert_eq!(camelot(bad), None, "{bad}");
        }
    }

    #[test]
    fn unmeasurable_input_gives_no_step() {
        let none = Pick { steps: 0, limited_by: Some("unmeasurable") };
        assert_eq!(pick_steps(f64::NAN, -2.7, -14.0, -1.0, (-50, 50)), none);
        assert_eq!(pick_steps(-10.9, f64::INFINITY, -14.0, -1.0, (-50, 50)), none);
        assert_eq!(pick_steps(-10.9, -2.7, f64::NAN, -1.0, (-50, 50)), none);
    }
}
