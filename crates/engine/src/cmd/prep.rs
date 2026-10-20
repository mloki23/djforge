//! `prep.tracks`: level-matched, tagged copies of a set's or a folder's MP3s. Each copy's frames
//! get a lossless global_gain change (whole 1.5 dB steps, no re-encode) towards a loudness target
//! without passing a true-peak ceiling; missing key and BPM tags are filled from mixforge's
//! analysis. The originals are only read, and nothing in `out` but earlier prep output is replaced.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_audio_io::mp3gain::{self, GainError, STEP_DB};
use soundcraft_audio_io::mp3tags::{self, TagFill};
use soundcraft_audio_io::{AudioBuffer, decode, probe};
use soundcraft_dsp::meter::LoudnessMeter;
use soundcraft_model::Session;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

const CMD: &str = "prep.tracks";
/// The report written next to the copies.
const REPORT: &str = "prep-report.txt";
/// How the report starts: an existing `prep-report.txt` is replaced only when it starts so.
const REPORT_HEADER: &str = "djforge track prep: ";
/// Integrated loudness at or below this is too quiet to measure (the meter's absolute gate).
const QUIET_LUFS: f64 = -70.0;
/// Tracks within this many dB of the target count as on target in the totals.
const ON_TARGET_DB: f64 = 0.75;
/// How far over the ceiling a measured true peak may read before the copy counts as over it.
const TP_SLACK_DB: f64 = 0.05;
/// A decoded sample at or past this is clamped (the MP3 decoder stops at full scale), so the
/// audio there is louder than it reads.
const CLAMPED: f32 = 1.0 - 1e-6;
/// Most steps cut past the pick to bring a clipped master's real peaks under the ceiling.
const MAX_EXTRA_CUTS: i32 = 12;
/// Longest track prepped, in seconds: each is decoded whole, twice.
const MAX_SECS: f64 = 30.0 * 60.0;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        noundo "prep.tracks",
        "Prep Tracks",
        [],
        None,
        "{source: a mixforge plan.json or a folder of MP3s, out?: folder for the prepped copies (default Music/djforge-prepped/<set or folder name>), target_lufs?: -14, ceiling?: -1} — copies every MP3 to `out` level-matched without re-encoding (global_gain steps of 1.5 dB, never above the ceiling in dBTP) with missing key/BPM tags filled from mixforge's analysis; originals are never written, and a file already in `out` is replaced only if it is an earlier prep copy or report. Tracks over 30 minutes are copied unadjusted. Writes out/prep-report.txt. Returns {out, report, target_lufs, ceiling, tracks: [{file, lufs_before, tp_before, steps, gain_db, lufs_after, tp_after, limited_by, filled, doubts, status: ok|not_adjusted|failed}], totals}",
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

/// `p` made absolute with `.` and `..` resolved by name (not by following links), in the
/// platform's own separators.
fn normalise(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            // `..` at the root stays at the root.
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The tracks of `source` (a plan.json, or a folder's MP3s sorted by name) and the name the
/// default output folder takes (the plan's folder, or the folder). A plan naming one file twice
/// is refused.
fn sources(source: &str) -> Result<(String, Vec<PathBuf>)> {
    let path = normalise(Path::new(source));
    let folder_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_string);
    let is_plan = path.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("json"));
    if is_plan {
        let plan = super::dj::plan::parse_plan(&crate::io::read_text(&path)?).map_err(|m| bad(CMD, m))?;
        let dir = path.parent().unwrap_or(Path::new(""));
        let files: Vec<PathBuf> = plan
            .tracks
            .iter()
            .map(|t| {
                let p = Path::new(&t.path);
                normalise(&if p.is_relative() { dir.join(p) } else { p.to_path_buf() })
            })
            .collect();
        let mut seen = HashSet::new();
        if let Some(twice) = files.iter().find(|f| !seen.insert(crate::io::file_key(f))) {
            return Err(bad(CMD, format!("listed twice: {}; the plan must name each file once (nothing was written)", twice.display())));
        }
        let name = path.parent().and_then(folder_name).unwrap_or_else(|| "set".to_string());
        return Ok((name, files));
    }
    let mut files: Vec<PathBuf> = crate::io::list_files(&path)
        .map_err(|err| EngineError::Io(format!("{err} (`source` must be a plan.json or a folder of MP3s)")))?
        .into_iter()
        .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3")))
        .collect();
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    if files.is_empty() {
        return Err(bad(CMD, format!("{source}: holds no MP3s")));
    }
    let name = folder_name(&path).unwrap_or_else(|| "folder".to_string());
    Ok((name, files))
}

/// `Music/djforge-prepped/<name>` in the home folder.
fn default_out(name: &str) -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(var).filter(|h| !h.is_empty()).ok_or_else(|| bad(CMD, format!("{var} is not set; pass `out`")))?;
    Ok(normalise(&PathBuf::from(home).join("Music").join("djforge-prepped").join(name)))
}

/// The text of the earlier prep report at `path`, if that is what it is.
fn prep_report(path: &Path) -> Option<String> {
    crate::io::read_text(path).ok().filter(|t| t.starts_with(REPORT_HEADER))
}

/// Doubts that mean prep wrote a copy without its `TXXX:djforge_prep` note.
const UNNOTED: [&str; 3] = [TAGS_LEFT, NOT_MPEG, NOT_AN_MP3];
const TAGS_LEFT: &str = "tags left as they are";
const NOT_MPEG: &str = "not MPEG audio";
const NOT_AN_MP3: &str = "not an MP3";

/// Whether `report` (an earlier prep report) says it wrote a copy named `name` without its note,
/// so the copy can't be recognised by its tag.
fn listed_unnoted(report: &str, name: &str) -> bool {
    let line = format!("  {name}  before ");
    report.lines().any(|l| l.contains(&line) && UNNOTED.iter().any(|d| l.contains(d)))
}

/// `out` is not a source's folder and nothing in it is a source (by name or link): checked by
/// name before `out` exists and again, links followed, once it does.
fn refuse_sources(files: &[PathBuf], out: &Path, dests: &[PathBuf]) -> Result<()> {
    let key = crate::io::file_key;
    let out_key = key(out);
    if let Some(f) = files.iter().find(|f| f.parent().is_some_and(|p| p == out || key(p) == out_key)) {
        return Err(bad(CMD, format!("`out` is the folder of source {}; choose another folder so the originals are never overwritten", f.display())));
    }
    let source_keys: HashSet<PathBuf> = files.iter().map(|f| key(f)).collect();
    if let Some(d) = dests.iter().chain([&out.join(REPORT)]).find(|d| source_keys.contains(&key(d))) {
        return Err(bad(CMD, format!("{}: is a source file; choose another `out`", d.display())));
    }
    Ok(())
}

/// Where each track's copy goes (`out/<file name>`), once every refusal and access check passed:
/// `out` is not a source's folder, no two sources share a name, no copy (or the report) would
/// land on a source or a session's audio, and every file already there is earlier prep output.
fn destinations(s: &Session, files: &[PathBuf], out: &Path) -> Result<Vec<PathBuf>> {
    let mut names: HashMap<String, &Path> = HashMap::new();
    let mut dests = Vec::with_capacity(files.len());
    for f in files {
        crate::file_access::check_read(f).map_err(EngineError::Io)?;
        let name = f.file_name().and_then(|n| n.to_str()).ok_or_else(|| bad(CMD, format!("{}: not a file name", f.display())))?;
        let lower = name.to_lowercase();
        if lower == REPORT {
            return Err(bad(CMD, format!("{}: is named like the report; rename it", f.display())));
        }
        if let Some(other) = names.insert(lower, f) {
            return Err(bad(CMD, format!("two tracks share the name `{name}` ({} and {}); neither can be written", other.display(), f.display())));
        }
        dests.push(out.join(name));
    }
    refuse_sources(files, out, &dests)?;
    // Only earlier prep output may be replaced: a copy carrying prep's note, or one the earlier
    // report there lists as written without it (tags left alone, not MPEG, not an MP3).
    let report = out.join(REPORT);
    let earlier = if crate::io::path_exists(&report) {
        Some(
            prep_report(&report)
                .ok_or_else(|| bad(CMD, format!("{}: exists and is not a prep report; move it or choose another `out`", report.display())))?,
        )
    } else {
        None
    };
    for d in &dests {
        if !crate::io::path_exists(d) {
            continue;
        }
        let bytes = crate::io::read_file(d)
            .map_err(|err| bad(CMD, format!("{}: exists and can't be read ({err}); move it or choose another `out`", d.display())))?;
        let name = d.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let mp3 = d.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3"));
        let ours = (mp3 && mp3tags::has_prep_note(&bytes)) || earlier.as_deref().is_some_and(|r| listed_unnoted(r, name));
        if !ours {
            return Err(bad(CMD, format!("{}: exists and is not a prep copy; move it or choose another `out`", d.display())));
        }
    }
    for d in dests.iter().chain([&report]) {
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
        Some(o) => normalise(Path::new(o)),
        None => default_out(&name)?,
    };
    let dests = destinations(s, &files, &out)?;
    crate::io::make_dir(&out)?;
    // Now that it exists, `out` is compared with the sources again with its links followed.
    refuse_sources(&files, &out, &dests)?;
    // Temporary files a run that stopped part-way left behind.
    crate::io::remove_stale_temps(&out);

    // One track at a time, so only one track's audio is in memory.
    // After one analysis times out the rest are skipped, so a stuck sidecar costs one timeout,
    // not one per track.
    let timed_out = std::cell::Cell::new(false);
    let analyse = |path: &str| {
        if timed_out.get() {
            return Err("skipped after an earlier timeout".to_string());
        }
        let r = analyse(path);
        if r.as_ref().is_err_and(|m| m.starts_with(super::dj::TIMED_OUT)) {
            timed_out.set(true);
        }
        r
    };
    let rows: Vec<Row> = files
        .iter()
        .zip(&dests)
        .map(|(src, dest)| {
            let mut row = prep_one(s, src, dest, target_lufs, ceiling, &analyse);
            if row.status == "failed" && crate::io::path_exists(dest) {
                row.doubts.push("the earlier copy in the output folder was left as it was".into());
            }
            row
        })
        .collect();

    let count = |st: &str| rows.iter().filter(|r| r.status == st).count();
    let written = || rows.iter().filter(|r| r.status != "failed");
    let measured = written().filter(|r| r.lufs_after.is_some()).count();
    let on_target = written().filter(|r| r.lufs_after.is_some_and(|l| (l - target_lufs).abs() <= ON_TARGET_DB)).count();
    let over = written().filter(|r| r.over(ceiling)).count();
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
    crate::io::replace_user_file(s, &report_path, text_report(&r, &rows, utc_now()).as_bytes())?;
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

    /// Whether the copy's true peak is over `ceiling`.
    fn over(&self, ceiling: f64) -> bool {
        self.tp_after.is_some_and(|tp| tp > ceiling + TP_SLACK_DB)
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

/// Integrated loudness (LUFS) and true peak (dBTP), and whether any decoded sample was clamped at
/// full scale (then the true values are higher than these).
#[derive(Debug, Clone, Copy)]
struct Level {
    lufs: f64,
    tp: f64,
    clamped: bool,
}

impl Level {
    /// These levels moved by `steps` gain steps.
    fn moved(self, steps: i32) -> Self {
        let db = f64::from(steps) * STEP_DB;
        Self { lufs: self.lufs + db, tp: self.tp + db, clamped: self.clamped }
    }
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
    let clamped = buf.channels.iter().flatten().any(|s| s.abs() >= CLAMPED);
    let level = Level { lufs: f64::from(meter.integrated_lufs()), tp: f64::from(meter.true_peak_db()), clamped };
    Ok((buf, level))
}

/// Whether `y` is `x` scaled by exactly `steps` global_gain steps: the same shape, a least-squares
/// ratio within 0.01 dB of the step, and nowhere off that ratio by more than 1e-3 of the peak (so
/// a level change part-way through fails even when the overall ratio is close). Samples clamped
/// at full scale in either are left out (the decoder lost their true value); at least half the
/// samples must remain.
fn verify(x: &AudioBuffer, y: &AudioBuffer, steps: i32) -> std::result::Result<(), String> {
    if x.channels.len() != y.channels.len() || x.channels.iter().zip(&y.channels).any(|(a, b)| a.len() != b.len()) {
        return Err("the copy decodes to a different length".into());
    }
    let all = || x.channels.iter().zip(&y.channels).flat_map(|(a, b)| a.iter().zip(b));
    let pairs = || all().filter(|(a, b)| a.abs() < CLAMPED && b.abs() < CLAMPED).map(|(&a, &b)| (f64::from(a), f64::from(b)));
    let (used, total) = (pairs().count(), all().count());
    if used == 0 || used < total / 2 {
        return Err(format!("too few unclipped samples to compare ({used} of {total})"));
    }
    let (xy, xx, peak) = pairs().fold((0.0, 0.0, 0.0f64), |(xy, xx, pk), (a, b)| (xy + a * b, xx + a * a, pk.max(a.abs()).max(b.abs())));
    if !(xx > 0.0 && xy > 0.0) {
        return Err("the audio is silent or inverted".into());
    }
    let ratio = xy / xx;
    let off_db = 20.0 * ratio.log10() - f64::from(steps) * STEP_DB;
    if off_db.is_nan() || off_db.abs() >= 0.01 {
        return Err(format!("the copy is {off_db:+.3} dB off the step"));
    }
    let residual = pairs().map(|(a, b)| (b - ratio * a).abs()).fold(0.0, f64::max);
    if residual > 1e-3 * peak {
        return Err(format!("the copy is not one gain throughout (off by up to {residual:.2e})"));
    }
    Ok(())
}

/// MP3 `bytes` moved by `steps` and checked against the original's decoded audio `x` (at `before`):
/// the copy and its level, or `None` (with `row` failed) when it does not decode or verify.
fn attempt(bytes: &[u8], x: &AudioBuffer, before: Level, steps: i32, row: &mut Row) -> std::result::Result<Option<(Vec<u8>, Level)>, GainError> {
    if steps == 0 {
        return Ok(Some((bytes.to_vec(), before)));
    }
    let adjusted = mp3gain::adjust(bytes, steps)?;
    let tried = format!("{steps} steps, {:+.1} dB", f64::from(steps) * STEP_DB);
    let checked = measure(&adjusted).and_then(|(y, after)| verify(x, &y, steps).map(|()| after));
    match checked {
        Ok(after) => Ok(Some((adjusted, after))),
        Err(m) => {
            row.failed(format!("the adjusted copy ({tried}) did not verify: {m}"));
            Ok(None)
        }
    }
}

/// The gain-adjusted copy of MP3 `bytes` (whose step range is `range`), filling `row`'s levels;
/// the bytes unchanged when they can't be adjusted, `None` when a copy fails verification.
///
/// A clipped master decodes clamped at full scale, so its first measurement reads low. A copy
/// that still clamps, or whose true peak is over the ceiling, is cut one more step (up to
/// [`MAX_EXTRA_CUTS`]); once a copy is clean, the original's levels are taken from it (the copy's
/// less the gain) and the step is picked again from those.
fn level(bytes: &[u8], range: std::result::Result<(i32, i32), GainError>, target: f64, ceiling: f64, row: &mut Row) -> Option<Vec<u8>> {
    let (x, measured) = match measure(bytes) {
        Ok(m) => m,
        Err(m) => {
            row.not_adjusted(format!("does not decode ({m})"));
            return Some(bytes.to_vec());
        }
    };
    let set_before = |row: &mut Row, l: Level| (row.lufs_before, row.tp_before) = (Some(l.lufs), Some(l.tp));
    let set_after = |row: &mut Row, l: Level| (row.lufs_after, row.tp_after) = (Some(l.lufs), Some(l.tp));
    set_before(row, measured);
    set_after(row, measured);
    let clip_note = |l: Level| format!("original clips at 0 dBFS (true peak ≈ {:+.1} dBTP)", l.tp);
    let range = match range {
        Ok(r) => r,
        Err(err) => {
            if measured.clamped {
                row.doubts.push(format!("original clips at 0 dBFS (true peak above {:+.1} dBTP)", measured.tp));
            }
            row.not_adjusted(err);
            return Some(bytes.to_vec());
        }
    };
    let measurable = |l: Level| if l.lufs > QUIET_LUFS { l.lufs } else { f64::NAN };
    let mut pick = pick_steps(measurable(measured), measured.tp, target, ceiling, range);
    if pick.limited_by == Some("unmeasurable") {
        row.not_adjusted("too quiet to measure");
        return Some(bytes.to_vec());
    }
    let (mut k, mut cuts, mut repicked) = (pick.steps, 0, false);
    let (copy, after) = loop {
        let (copy, after) = match attempt(bytes, &x, measured, k, row) {
            Ok(Some(a)) => a,
            Ok(None) => return None,
            Err(err) => {
                row.not_adjusted(err);
                return Some(bytes.to_vec());
            }
        };
        if measured.clamped && !after.clamped && k != 0 && !repicked {
            repicked = true;
            let honest = after.moved(-k);
            let again = pick_steps(measurable(honest), honest.tp, target, ceiling, range);
            if again.limited_by != Some("unmeasurable") {
                pick = again;
                if again.steps != k {
                    k = again.steps;
                    continue;
                }
            }
        }
        let hot = after.clamped || after.tp > ceiling + TP_SLACK_DB;
        if hot && cuts < MAX_EXTRA_CUTS && k > range.0 {
            (k, cuts) = (k - 1, cuts + 1);
            continue;
        }
        break (copy, after);
    };
    drop(x);
    row.steps = k;
    set_after(row, after);
    if measured.clamped {
        if after.clamped || k == 0 {
            row.doubts.push(format!("original clips at 0 dBFS (true peak above {:+.1} dBTP)", measured.tp));
        } else {
            let honest = after.moved(-k);
            set_before(row, honest);
            row.doubts.push(clip_note(honest));
        }
    }
    let hot = after.clamped || after.tp > ceiling + TP_SLACK_DB;
    row.limited_by = if hot {
        Some(if k <= range.0 { "global_gain" } else { "peaks" })
    } else if k < pick.steps {
        Some("peaks")
    } else {
        pick.limited_by
    };
    if after.clamped {
        row.doubts.push("the copy still clips at 0 dBFS".into());
    }
    Some(copy)
}

/// `k` as a Camelot key (`1A`–`12B`): Camelot itself (`4B`, `04b`) or musical notation (`Am`,
/// `F#m`, `Db`, `Ebmin`, `C major`).
fn to_camelot(k: &str) -> Option<String> {
    let k = k.trim();
    let upper = k.to_ascii_uppercase();
    if let Some(digits) = upper.strip_suffix(['A', 'B']).filter(|d| !d.is_empty() && d.len() <= 2 && d.bytes().all(|b| b.is_ascii_digit())) {
        let n: u8 = digits.parse().ok()?;
        let letter = upper.chars().last()?;
        return (1..=12).contains(&n).then(|| format!("{n}{letter}"));
    }
    let mut chars = k.chars();
    let root: u32 = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest = chars.as_str();
    let (shift, rest) = match (rest.strip_prefix(['#', '♯']), rest.strip_prefix(['b', '♭'])) {
        (Some(r), _) => (1, r),
        (_, Some(r)) => (11, r),
        _ => (0, rest),
    };
    let minor = match rest.trim().to_ascii_lowercase().as_str() {
        "" | "maj" | "major" => false,
        "m" | "min" | "minor" => true,
        _ => return None,
    };
    let pc = (root + shift) % 12;
    // Camelot numbers follow the circle of fifths from C major = 8B; a minor key is its
    // relative major's number with A.
    let major = if minor { (pc + 3) % 12 } else { pc };
    let n = (major * 7 % 12 + 7) % 12 + 1;
    Some(format!("{n}{}", if minor { 'A' } else { 'B' }))
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
        Some(k) => to_camelot(k).or_else(|| {
            doubts.push(format!("mixforge's key `{k}` is not a key (not filled)"));
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

/// Preps MP3 `bytes` from `src`: the copy to write, or `None` when the track failed.
fn prep_mp3(
    bytes: Vec<u8>,
    src: &Path,
    target: f64,
    ceiling: f64,
    row: &mut Row,
    analyse: &dyn Fn(&str) -> std::result::Result<Value, String>,
) -> Option<Vec<u8>> {
    let range = mp3gain::step_range(&bytes);
    if matches!(range, Err(GainError::NotLayer3)) {
        // No MPEG audio frames: nothing to adjust, and tags would be written into a file no
        // player reads as an MP3.
        row.status = "not_adjusted";
        row.doubts.push(NOT_MPEG.into());
        return Some(bytes);
    }
    let long = probe(&bytes, Some("mp3")).is_ok_and(|i| i.duration_secs() > MAX_SECS);
    let adjusted = if long {
        row.not_adjusted("longer than 30 min");
        bytes
    } else {
        let adjusted = level(&bytes, range, target, ceiling, row)?;
        drop(bytes);
        adjusted
    };
    if let (Some(why), Some(after)) = (row.limited_by, row.lufs_after) {
        row.doubts.insert(0, format!("{why}: reaches {after:.1} LUFS"));
    }
    tagged(adjusted, src, target, row, analyse)
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
        let Some(copy) = prep_mp3(bytes, src, target, ceiling, &mut row, analyse) else { return row };
        copy
    } else {
        row.not_adjusted(NOT_AN_MP3);
        bytes
    };
    if row.over(ceiling) {
        row.doubts.push(format!("above the ceiling: tp {} dBTP", db(row.tp_after)));
    }
    if let Err(err) = crate::io::replace_user_file(s, dest, &copy) {
        row.failed(err);
    }
    row
}

/// `adjusted` with missing key and BPM tags filled from mixforge and the prep note added; with its
/// tags untouched when they can't be rewritten. Notes what the tags say in `row`. `None` (the
/// track failed) if the rewrite changed anything after the tag.
fn tagged(
    adjusted: Vec<u8>,
    src: &Path,
    target: f64,
    row: &mut Row,
    analyse: &dyn Fn(&str) -> std::result::Result<Value, String>,
) -> Option<Vec<u8>> {
    let (key, bpm) = analysis(src, analyse, &mut row.doubts);
    let note = prep_note(row, target);
    let (bytes, tags) = match mp3tags::fill(&adjusted, &TagFill { camelot: key.as_deref(), bpm, prep_note: &note }) {
        Ok(x) => x,
        Err(err) => {
            row.doubts.push(format!("{TAGS_LEFT}: {err}"));
            return Some(adjusted);
        }
    };
    if bytes.get(mp3gain::skip_id3v2(&bytes)..) != adjusted.get(mp3gain::skip_id3v2(&adjusted)..) {
        row.failed("writing the tags changed the audio after them");
        return None;
    }
    drop(adjusted);
    row.filled = tags.filled.clone();
    if let Some(file) = tags.existing_key.as_deref().map(str::trim) {
        match (to_camelot(file), &key) {
            (Some(c), Some(mf)) if &c != mf => {
                let shown = if c == file { c.clone() } else { format!("{file} ({c})") };
                row.doubts.push(format!("key: file {shown}, mixforge {mf} (file kept)"));
            }
            (None, _) => row.doubts.push(format!("key: file `{file}` is not a key (file kept)")),
            _ => {}
        }
    }
    if let Some(file) = tags.existing_bpm.as_deref().map(str::trim) {
        match (file.parse::<f64>().ok().filter(|f| f.is_finite()), bpm) {
            (Some(f), Some(mf)) if (f - mf).abs() > 1.0 => {
                let twice = (f - 2.0 * mf).abs() <= 1.0 || (f - mf / 2.0).abs() <= 1.0;
                row.doubts.push(format!("bpm: file {f}, mixforge {mf:.1}{} (file kept)", if twice { " (half/double)" } else { "" }));
            }
            (None, _) => row.doubts.push(format!("bpm: file `{file}` is not a number (file kept)")),
            _ => {}
        }
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
    Some(bytes)
}

/// The plain-text report: a header, one line per track, then the totals.
fn text_report(r: &Value, rows: &[Row], date: String) -> String {
    let num = |k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let src = r.get("source").and_then(Value::as_str).unwrap_or("");
    let mut out = format!("{REPORT_HEADER}{src}\ntarget {} LUFS, ceiling {} dBTP, {date}\n\n", num("target_lufs"), num("ceiling"));
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
    fn verify_leaves_out_samples_the_decoder_clamped() {
        // A master driven 6 dB past full scale, decoded clamped; cut 4 steps, it is clean.
        let loud = |n: usize| 4.0 * (n as f32 * 0.05).sin();
        let clamp = |v: f32| v.clamp(-1.0, 1.0);
        let x = AudioBuffer { sample_rate: 48_000, channels: vec![(0..4_800).map(|n| clamp(loud(n))).collect()] };
        let cut = 10f32.powf((-4.0 * STEP_DB / 20.0) as f32);
        let y = AudioBuffer { sample_rate: 48_000, channels: vec![(0..4_800).map(|n| clamp(loud(n) * cut)).collect()] };
        // Most samples clamp at 4×: too few left to compare.
        assert!(verify(&x, &y, -4).unwrap_err().contains("too few"));
        // At 1.1× about a quarter clamp; the rest compare exactly.
        let mild = |n: usize| 1.1 * (n as f32 * 0.05).sin();
        let x = AudioBuffer { sample_rate: 48_000, channels: vec![(0..4_800).map(|n| clamp(mild(n))).collect()] };
        let y = AudioBuffer { sample_rate: 48_000, channels: vec![(0..4_800).map(|n| clamp(mild(n) * cut)).collect()] };
        assert_eq!(verify(&x, &y, -4), Ok(()));
        assert!(verify(&x, &y, -3).is_err());
    }

    #[test]
    fn keys_in_either_notation_become_camelot() {
        for (k, want) in [
            (" 4b ", "4B"),
            ("12A", "12A"),
            ("04B", "4B"),
            ("Am", "8A"),
            ("F#m", "11A"),
            ("Db", "3B"),
            ("D#", "5B"),
            ("Dm", "7A"),
            ("Bbm", "3A"),
            ("Ebmin", "2A"),
            ("C major", "8B"),
            ("E", "12B"),
        ] {
            assert_eq!(to_camelot(k).as_deref(), Some(want), "{k}");
        }
        for bad in ["13A", "0A", "+4B", "A1", "4C", "", "H", "Am7", "100A"] {
            assert_eq!(to_camelot(bad), None, "{bad}");
        }
    }

    #[test]
    fn dots_in_a_path_are_resolved_by_name() {
        let base = std::path::absolute(".").unwrap();
        assert_eq!(normalise(&base.join("a").join("..").join(".").join("b")), normalise(&base.join("b")));
    }

    #[test]
    fn unmeasurable_input_gives_no_step() {
        let none = Pick { steps: 0, limited_by: Some("unmeasurable") };
        assert_eq!(pick_steps(f64::NAN, -2.7, -14.0, -1.0, (-50, 50)), none);
        assert_eq!(pick_steps(-10.9, f64::INFINITY, -14.0, -1.0, (-50, 50)), none);
        assert_eq!(pick_steps(-10.9, -2.7, f64::NAN, -1.0, (-50, 50)), none);
    }
}
