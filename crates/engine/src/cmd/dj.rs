//! DJ commands: beat-locked track loading and tempo ramps.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{Clip, MAX_WARP_BEATS, Session, WarpLock, WarpMap, tempo_fingerprint};
use soundcraft_time::{MAX_POSITION, SampleRate, TICKS_PER_QUARTER, TempoMap};

#[path = "dj_align.rs"]
mod align;
#[path = "dj_import.rs"]
mod import;
#[path = "dj_move.rs"]
mod moving;
#[path = "dj_plan.rs"]
pub(crate) mod plan;
#[path = "dj_recipe.rs"]
pub(crate) mod recipe;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "dj.load_warped",
            "Load Track Beat-Locked",
            [],
            None,
            "{path, beats?: [seconds, first a downbeat] | bpm + first_beat (seconds of a downbeat) (default: mixforge's analysis of the file, needs DJFORGE_PYTHON and DJFORGE_MIXFORGE), at_bar?: 1 (the clip starts on this bar; the first downbeat lands on the first bar line at or after the file start), from_beat?: track beat playing at the clip start, track?} — imports an audio file whose beat grid follows the session tempo map in realtime, pitch preserved",
            always,
            load_warped
        ),
        cmd!(
            query "dj.analyze",
            "Analyse Track (mixforge)",
            [],
            None,
            "{path} — BPM, first downbeat, key and loudness from mixforge's analysis (needs DJFORGE_PYTHON: mixforge's python.exe, and DJFORGE_MIXFORGE: the mixforge folder)",
            always,
            |_, p| analyze_file("dj.analyze", str_param(p, "path").ok_or_else(|| bad("dj.analyze", "`path` required"))?)
        ),
        cmd!(
            "dj.tempo_ramp",
            "Tempo Ramp",
            [],
            None,
            "{to_bpm, from_bar?, bars? (default: the whole bars of the timeline selection), from_bpm? (default: the tempo at from_bar)} — glides the session tempo from `from_bpm` to `to_bpm` over `bars`, one tempo step per beat, holding `to_bpm` afterwards",
            always,
            tempo_ramp
        ),
        cmd!(
            "dj.crossfade",
            "Crossfade Transition",
            [],
            None,
            "{out_clip, in_clip (default: the two selected clips), shape?: equal power} — fades `out_clip` out and `in_clip` in across the span where they overlap",
            always,
            crossfade
        ),
        cmd!(
            "dj.nudge",
            "Nudge Beat-Locked Track",
            [],
            None,
            "{clip (default: the selected clip), ms: -1000..=1000 | beats: -8..=8} — plays the clip's music `ms` (at the clip-start tempo) or `beats` later (negative: earlier), keeping it beat-locked",
            always,
            nudge
        ),
        cmd!(
            query "dj.suggest_nudge",
            "Suggest Transition Nudge",
            [],
            None,
            "{out_clip, in_clip} — measures how far the incoming clip's beats sit from the outgoing clip's across their overlap (at least 8 beats), from low/mid/high onset profiles folded onto the session beat; analyses the last 128 beats; returns {nudge_ms, nudge_beats (apply either with dj.nudge {clip: in_clip, ms | beats}), reliable, note (why not), ambiguity (second peak / best), score, agree (bands within 10 ms of it), bands (each band's own nudge_ms), overlap_beats, analysed_beats}. Changes nothing",
            always,
            align::suggest_nudge
        ),
        cmd!(
            "dj.transition",
            "Build Transition",
            [],
            None,
            "{out_clip, in_clip} | {transition: id} (default: the two selected clips), style?: blend|quick, build_bars?: 1..=256, out_bars?: 1..=256, entry_bar?, force?: bool — records the mix from `out_clip` into `in_clip` (entry_bar default: the first bar at or after the incoming clip's start; style default blend; bars default from the style, 16/8 blend, 8/4 quick, and a style given without bars brings its own) and writes its recipe: EQ 7-Band in insert slot a of both decks (inserted if the slot is empty), the incoming low shelf -24 dB at entry_bar to 0 at the hit (entry_bar + build_bars), the outgoing one 0 at the hit to -24 dB at the gone bar (hit + out_bars), an equal-power fade-in from the incoming clip's start to the hit and fade-out from the hit to the outgoing clip's end. Refuses a transition edited by hand unless `force`. Returns {transition, hit_bar, gone_bar, recipe: {entry, hit, gone, in_fade, out_fade, points}, eq_inserted}",
            always,
            recipe::transition
        ),
        cmd!(
            "dj.import_set",
            "Import mixforge Set",
            [],
            None,
            "{plan: path of mixforge's plan.json, report?: path of a text report to write} — into a session without beat-locked clips: Deck A/B (alternating) with every track beat-locked, its first downbeat on the bar its transition enters on (the bar line nearest the previous track's out_start); the plan's tempo (a ramp over each transition's bars, or a step on its hit bar); clip gains; a Maximizer on the master fader; each transition with Marty's recipe (dj.transition; shortened when the clips are too short); each incoming track nudged by the plan's nudge_ms, else by a reliable dj.suggest_nudge. One undo step. Returns {tracks, track_list, sample_rate, decks, master, transitions: [{index, transition, out_title, in_title, out_camelot, in_camelot, style, mixforge_bars, entry_bar, entry_time, hit_bar, nudge_beats, nudge_ms, nudge_source, reliable, ambiguity, alt_ms, doubts}], warnings, ignored_fx}",
            always,
            import::import_set
        ),
        cmd!(
            "dj.move_transition",
            "Move Transition",
            [],
            None,
            "{transition: id (default: the one into the selected clip), bars: non-zero whole bars, later if positive} — moves the mix point: the incoming track and every later one in the chain, and every clip, automation point, marker, key signature and tempo or meter event from the old entry on, go `bars` later (earlier), each keeping its beats (automation over a clip that stays keeps its beat; clips on inactive playlists don't move); the tempo before the old entry stays (moving later, the outgoing track's tempo fills the gap; moving earlier, the overlapped tempo events go). Outgoing clips end 16 bars after their new gone bar (within their audio) and the moved recipes are rebuilt, skipping (and listing) transitions edited by hand. Refuses an entry before the outgoing track starts or before the previous transition's bass is gone. Returns {transition, bars, entry_bar, moved_clips, rebuilt, skipped: [{transition, reason}]}",
            always,
            moving::move_transition
        ),
        cmd!(
            query "dj.transitions",
            "List Transitions",
            [],
            None,
            "{} — every transition with its hit_bar and gone_bar. Changes nothing",
            always,
            recipe::transitions
        ),
    ]
}

/// The one selected clip.
pub(super) fn selected_one(s: &Session) -> Option<ClipId> {
    match s.edit.selected_clips.as_slice() {
        [c] => Some(*c),
        _ => None,
    }
}

/// The two selected clips, the one starting earlier first (out, in).
pub(super) fn selected_pair(s: &Session) -> Option<(ClipId, ClipId)> {
    let mut v: Vec<(Samples, ClipId)> = s.edit.selected_clips.iter().filter_map(|c| s.find_clip(*c).map(|(_, x)| (x.start, *c))).collect();
    v.sort();
    match v.as_slice() {
        [a, b] if s.edit.selected_clips.len() == 2 => Some((a.1, b.1)),
        _ => None,
    }
}

/// Shifts a beat-locked clip's music later by `beats` session beats, or by `ms` (negative:
/// earlier) without moving the clip: the anchor moves by those beats. `ms` converts at the tempo
/// of the clip start, so across a tempo ramp the shift in milliseconds varies slightly along the
/// clip; it stays constant in beats.
fn nudge(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.nudge";
    let id = match p.get("clip") {
        Some(v) => v.as_u64().map(ClipId).ok_or_else(|| bad(CMD, "`clip` must be a clip id"))?,
        None => selected_one(e.session()).ok_or_else(|| bad(CMD, "`clip` required (or select one clip)"))?,
    };
    let num = |key: &str, max: f64| p.get(key).map(|v| v.as_f64().filter(|x| x.is_finite() && x.abs() <= max));
    let s = e.session();
    let c = s.find_clip(id).map(|(_, c)| c).ok_or_else(|| bad(CMD, format!("no clip {}", id.0)))?;
    align::beat_locked(c).ok_or_else(|| align::not_beat_locked(CMD, id))?;
    let beat_ms = 60_000.0 / s.tempo.tempo_at_tick(s.tempo.samples_to_ticks(c.start, s.sample_rate));
    let beats = match (num("ms", 1000.0), num("beats", 8.0)) {
        (Some(ms), None) => ms.ok_or_else(|| bad(CMD, "`ms` must be -1000..=1000"))? / beat_ms,
        (None, Some(beats)) => beats.ok_or_else(|| bad(CMD, "`beats` must be -8..=8"))?,
        _ => return Err(bad(CMD, "give one of `ms` or `beats`")),
    };
    let w = e.session_mut().find_clip_mut(id).and_then(|c| c.warp.as_mut()).ok_or_else(|| align::not_beat_locked(CMD, id))?;
    w.anchor -= beats;
    Ok(json!({"clip": id, "anchor": w.anchor, "beats": beats, "ms": beats * beat_ms}))
}

fn crossfade(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.crossfade";
    let id = |key: &str| p.get(key).and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(CMD, format!("`{key}` required")));
    let (out_id, in_id) = match (p.get("out_clip"), p.get("in_clip")) {
        (None, None) => selected_pair(e.session()).ok_or_else(|| bad(CMD, "`out_clip` and `in_clip` required (or select two clips)"))?,
        _ => (id("out_clip")?, id("in_clip")?),
    };
    let shape = str_param(p, "shape").and_then(soundcraft_model::FadeShape::from_id).unwrap_or_default();
    let s = e.session_mut();
    let range = |s: &soundcraft_model::Session, c: ClipId| s.find_clip(c).map(|(_, c)| c.range()).ok_or_else(|| bad(CMD, format!("no clip {}", c.0)));
    let (ro, ri) = (range(s, out_id)?, range(s, in_id)?);
    let overlap = ro
        .intersect(&ri)
        .filter(|r| !r.is_empty() && ri.start >= ro.start)
        .ok_or_else(|| bad(CMD, "the incoming clip must start inside the outgoing one"))?;
    // The outgoing clip ends where the overlap does, so both fades span the same samples.
    if let Some(c) = s.find_clip_mut(out_id) {
        c.length = overlap.end - c.start;
        c.fade_out = soundcraft_model::Fade { len: overlap.len(), shape, curvature: 0.0 };
    }
    if let Some(c) = s.find_clip_mut(in_id) {
        c.fade_in = soundcraft_model::Fade { len: overlap.len(), shape, curvature: 0.0 };
    }
    Ok(json!({"start": overlap.start, "end": overlap.end, "length": overlap.len()}))
}

fn load_warped(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.load_warped";
    const BAD_BEATS: &str = "`beats` must be at least two finite, non-negative, strictly increasing times";
    let path = str_param(p, "path").ok_or_else(|| bad(CMD, "`path` required"))?.to_string();
    let given: Option<Vec<f64>> = match p.get("beats") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_array().and_then(|a| a.iter().map(Value::as_f64).collect::<Option<Vec<f64>>>()).ok_or_else(|| bad(CMD, BAD_BEATS))?),
    };
    let bpm = p.get("bpm").and_then(Value::as_f64);
    if let Some(b) = bpm
        && !(b.is_finite() && (20.0..=400.0).contains(&b))
    {
        return Err(bad(CMD, "`bpm` must be 20..=400"));
    }
    let first_beat = f64_or(p, "first_beat", 0.0);
    if !(first_beat.is_finite() && (0.0..=600.0).contains(&first_beat)) {
        return Err(bad(CMD, "`first_beat` must be 0..=600 seconds"));
    }
    // Refuse a bad list before importing, so it leaves no clip behind.
    if let Some(b) = &given
        && grid(b, 48_000.0, i64::MAX / 4, 4, None).is_none()
    {
        return Err(bad(CMD, BAD_BEATS));
    }
    // mixforge runs before the import too, so a failed analysis leaves no clip behind.
    let mut analysis = None;
    let source = match (given, bpm) {
        (Some(b), _) => Beats::List(b),
        (None, Some(bpm)) => Beats::Constant { bpm, first: first_beat },
        (None, None) => {
            let a = analyze_file(CMD, &path)?;
            let (bpm, first) = grid_of(&a).ok_or_else(|| bad(CMD, "mixforge's analysis has no usable bpm/first_downbeat"))?;
            analysis = Some(a);
            Beats::Constant { bpm, first }
        }
    };
    let at_bar = i64_or(p, "at_bar", 1).clamp(-10_000, 100_000);
    let target = track_param(e, CMD, p, "track")?;
    let from_beat = p.get("from_beat").and_then(Value::as_f64);
    let placed = place_warped(e, CMD, &path, source, at_bar, target, from_beat)?;
    let mut out = json!({"track": placed.track, "clip": placed.clip, "start": placed.start, "length": placed.length, "beats": placed.beats, "anchor": placed.anchor, "downbeat_bar": placed.downbeat_bar});
    if let (Some(a), Some(o)) = (analysis, out.as_object_mut()) {
        for k in ["bpm", "key", "camelot", "lufs", "stale_cache"] {
            o.insert(k.to_string(), a.get(k).cloned().unwrap_or(Value::Null));
        }
    }
    Ok(out)
}

/// Where a beat-locked track's beat times come from.
pub(super) enum Beats {
    /// Analysed beat times (seconds, the first a downbeat).
    List(Vec<f64>),
    /// A constant tempo from a first downbeat (seconds).
    Constant { bpm: f64, first: f64 },
}

/// A beat-locked clip [`place_warped`] put on the timeline.
pub(super) struct Placed {
    /// The track it is on (as the import reported it).
    pub track: Value,
    pub clip: ClipId,
    pub start: Samples,
    pub length: Samples,
    /// Session beats the clip lasts.
    pub beats: f64,
    pub anchor: f64,
    /// Bar its first downbeat plays on.
    pub downbeat_bar: i64,
}

/// Imports the audio file at `path` beat-locked on `at_bar` (on track `target`, else a new one),
/// its beats from `source`; `from_beat` as for `dj.load_warped`. Errors name command `cmd`; a
/// failure after the import is rolled back by `Engine::execute`.
pub(super) fn place_warped(
    e: &mut Engine,
    cmd: &str,
    path: &str,
    source: Beats,
    at_bar: i64,
    target: Option<TrackId>,
    from_beat: Option<f64>,
) -> Result<Placed> {
    const BAD_BEATS: &str = "`beats` must be at least two finite, non-negative, strictly increasing times";
    let sr = e.session().sample_rate;
    let start_tick = e.session().tempo.bar_start_tick(at_bar);
    let at = e.session().tempo.tick_to_samples(start_tick, sr);

    let bytes = crate::io::read_file(path)?;
    let name = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string();
    let r = crate::io::import_audio_bytes(e, &name, &bytes, Some(path), target, at)?;
    let clip = r.get("clip").and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(cmd, "import placed no clip"))?;
    let frames = r.get("frames").and_then(Value::as_i64).unwrap_or(0);

    // Beat times in seconds of the source-pool audio (the import resampled the file to the
    // session rate). A failure from here on is rolled back by `Engine::execute`.
    let rate = sr.as_f64();
    let secs: Vec<f64> = match source {
        Beats::List(b) => b,
        Beats::Constant { bpm, first } => {
            let beat = 60.0 / bpm;
            let n = ((frames as f64 / rate - first) / beat).ceil().max(1.0) as usize + 1;
            (0..n.min(MAX_WARP_BEATS)).map(|i| first + i as f64 * beat).collect()
        }
    };
    let per_bar = i64::from(e.session().tempo.meter_at_tick(start_tick).numerator.max(1));
    let (warp, track_beats) = grid(&secs, rate, frames, per_bar, from_beat).ok_or_else(|| bad(cmd, BAD_BEATS))?;
    let anchor = warp.anchor;
    // The clip lasts until the file's last sample, counted in session beats from its start.
    let end_tick = start_tick.saturating_add((track_beats * TICKS_PER_QUARTER as f64) as i64);
    let length = e.session().tempo.tick_to_samples(end_tick, sr).saturating_sub(at).max(1);
    let downbeat_bar = at_bar.saturating_add((-anchor / per_bar as f64).ceil().clamp(-1e9, 1e9) as i64);
    let s = e.session_mut();
    let c = s.find_clip_mut(clip).ok_or_else(|| bad(cmd, "imported clip vanished"))?;
    c.warp = Some(warp);
    c.length = length;
    Ok(Placed { track: r.get("track").cloned().unwrap_or(Value::Null), clip, start: at, length, beats: track_beats, anchor, downbeat_bar })
}

/// The mixforge sidecar: prints one JSON object with a file's analysis, or `{error}`.
const MIXFORGE_BEATS: &str = include_str!("mixforge_beats.py");

/// mixforge's analysis of the audio file at `path`, through the sidecar run with the python in
/// `DJFORGE_PYTHON` against the mixforge checkout in `DJFORGE_MIXFORGE`. Errors name command `cmd`.
pub(crate) fn analyze_file(cmd: &str, path: &str) -> Result<Value> {
    crate::file_access::check_read(path).map_err(EngineError::Io)?;
    let var =
        |name: &str, what: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| bad(cmd, format!("set {name} to {what}")));
    let python = var("DJFORGE_PYTHON", "the python.exe of mixforge's environment")?;
    let mixforge = var("DJFORGE_MIXFORGE", "the mixforge folder (the one holding the `mixforge` package)")?;
    let mut command = std::process::Command::new(&python);
    command.args(["-I", "-c", MIXFORGE_BEATS, &mixforge, path]).stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the GUI must not flash a console.
        command.creation_flags(0x0800_0000);
    }
    let out = match output_within(&mut command, SIDECAR_TIMEOUT) {
        Ok(Some(out)) => out,
        Ok(None) => return Err(bad(cmd, format!("{TIMED_OUT} after {} s", SIDECAR_TIMEOUT.as_secs()))),
        Err(err) => return Err(bad(cmd, format!("could not run DJFORGE_PYTHON ({python}): {err}"))),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines.get(lines.len().saturating_sub(5)..).unwrap_or_default().join("\n");
        return Err(bad(cmd, format!("mixforge printed nothing ({}): {tail}", out.status)));
    }
    parse_analysis(cmd, &stdout)
}

/// How the error for a sidecar that ran out of time starts.
pub(crate) const TIMED_OUT: &str = "timed out";
/// Longest the mixforge sidecar may take over one file.
const SIDECAR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Runs `command` (stdout and stderr captured) and returns its output, or `None` (after killing
/// it) when it runs longer than `limit`, or when a process it started keeps its output open past
/// `limit` after it exited.
fn output_within(command: &mut std::process::Command, limit: std::time::Duration) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read as _;
    command.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    // Both pipes are drained as the child writes, so a full pipe can't stall it. Each reader sends
    // what it read once its pipe closes; one that never closes is not waited for past `limit`.
    let (tx, rx) = std::sync::mpsc::channel();
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>, is_stdout: bool| {
        let tx = tx.clone();
        // A reader that can't start drops its sender: that output is lost, nothing waits for it.
        let _ = std::thread::Builder::new().name("sidecar-pipe".into()).spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send((is_stdout, buf));
        });
    };
    drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), true);
    drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), false);
    drop(tx);
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            for _ in 0..2 {
                match rx.recv_timeout(limit.saturating_sub(started.elapsed())) {
                    Ok((true, buf)) => stdout = buf,
                    Ok((false, buf)) => stderr = buf,
                    // A process the child started still holds a pipe: as good as timed out.
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                    // A reader ended without sending (it panicked): its output is lost.
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            return Ok(Some(std::process::Output { status, stdout, stderr }));
        }
        if started.elapsed() >= limit {
            // The readers end when the pipes close; they are not waited for, since a process the
            // child started may still hold them.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The sidecar's output as a JSON object; its `{error}` becomes command `cmd`'s error.
fn parse_analysis(cmd: &str, out: &str) -> Result<Value> {
    let text = out.trim();
    let head = || text.chars().take(300).collect::<String>();
    let v: Value = serde_json::from_str(text).map_err(|_| bad(cmd, format!("mixforge printed something that is not JSON: {}", head())))?;
    if let Some(msg) = v.get("error") {
        return Err(bad(cmd, msg.as_str().map_or_else(|| msg.to_string(), str::to_string)));
    }
    if !v.is_object() {
        return Err(bad(cmd, format!("mixforge printed something that is not an object: {}", head())));
    }
    Ok(v)
}

/// (bpm, first downbeat in seconds) of an analysis, when both are usable.
fn grid_of(a: &Value) -> Option<(f64, f64)> {
    let bpm = a.get("bpm")?.as_f64().filter(|b| b.is_finite() && (20.0..=400.0).contains(b))?;
    let first = a.get("first_downbeat")?.as_f64().filter(|f| f.is_finite() && (0.0..=600.0).contains(f))?;
    Some((bpm, first))
}

/// Lead-in (seconds) before a first downbeat that is dropped rather than given a bar of its own.
const PRE_ROLL_SECS: f64 = 0.060;

/// The warp map for analysed beat times `secs` (seconds, `secs[0]` a downbeat) of a file of
/// `frames` samples at `rate`, and how many session beats the clip lasts. By default the clip
/// starts on a bar line and the first downbeat falls on the first bar line at or after the file
/// start; `from_beat` overrides that. `None` unless the beats are finite, non-negative, strictly
/// increasing and at least two.
fn grid(secs: &[f64], rate: f64, frames: i64, beats_per_bar: i64, from_beat: Option<f64>) -> Option<(WarpMap, f64)> {
    if secs.len() < 2 || secs.len() > MAX_WARP_BEATS || !(rate.is_finite() && rate > 0.0) {
        return None;
    }
    // Checked in samples, so times that overflow or collapse when scaled are refused too.
    let beats: Vec<f64> = secs.iter().map(|s| s * rate).collect();
    if !beats.iter().all(|b| b.is_finite() && *b >= 0.0) || !beats.windows(2).all(|w| w.first() < w.get(1)) {
        return None;
    }
    let (first, second) = (*beats.first()?, *beats.get(1)?);
    let per_bar = beats_per_bar.max(1) as f64;
    let bar = (second - first) * per_bar;
    let anchor = match from_beat {
        Some(b) if b.is_finite() => b,
        // A downbeat this close to the file start goes on the clip-start bar line, dropping those
        // few milliseconds rather than adding a silent bar.
        _ if first <= PRE_ROLL_SECS * rate => 0.0,
        _ => -(first / bar).ceil() * per_bar,
    };
    let w = WarpMap::new(beats, anchor)?;
    let track_beats = w.beat_at_source(frames as f64) - w.anchor;
    track_beats.is_finite().then_some((w, track_beats.max(0.0)))
}

fn tempo_ramp(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.tempo_ramp";
    // Without a start bar, over the timeline selection: from the first bar line in it, for its
    // whole bars (a start bar given without bars takes nothing from the selection).
    let s = e.session();
    let sel = s.edit.selection;
    let bar_of = |at: Samples| {
        let bb = s.tempo.bar_beat_at(at, s.sample_rate);
        if s.tempo.tick_to_samples(s.tempo.bar_start_tick(bb.bar), s.sample_rate) < at { bb.bar.saturating_add(1) } else { bb.bar }
    };
    let (sel_from, sel_to) = (bar_of(sel.start), s.tempo.bar_beat_at(sel.end, s.sample_rate).bar);
    let from_bar = i64_or(p, "from_bar", sel_from).clamp(1, 100_000);
    let bars = i64_or(p, "bars", if sel.is_empty() || p.get("from_bar").is_some() { 0 } else { sel_to - from_bar });
    if !(1..=256).contains(&bars) {
        return Err(bad(CMD, "`bars` must be 1..=256 (or select at least a whole bar)"));
    }
    let start_bpm = s.tempo.tempo_at_tick(s.tempo.bar_start_tick(from_bar));
    let (a, b) = (f64_or(p, "from_bpm", start_bpm), f64_or(p, "to_bpm", f64::NAN));
    if ![a, b].iter().all(|v| v.is_finite() && (20.0..=400.0).contains(v)) {
        return Err(bad(CMD, "`from_bpm` and `to_bpm` must be 20..=400"));
    }
    if (a - b).abs() < 1e-9 {
        return Err(bad(CMD, format!("the tempo is already {b} bpm at bar {from_bar}; give the tempo to ramp to as `to_bpm`")));
    }
    let s = e.session_mut();
    let start = s.tempo.bar_start_tick(from_bar);
    let end = s.tempo.bar_start_tick(from_bar.saturating_add(bars));
    let beat = s.tempo.meter_at_tick(start).ticks_per_beat().max(1);
    let steps = ((end - start) / beat).max(1);
    for k in 0..steps {
        // Each beat plays at the tempo halfway through it, so the ramp's length is exact.
        let bpm = a + (b - a) * (k as f64 + 0.5) / steps as f64;
        s.tempo.set_tempo(start + k * beat, bpm).map_err(|err| bad(CMD, err.to_string()))?;
    }
    s.tempo.set_tempo(end, b).map_err(|err| bad(CMD, err.to_string()))?;
    Ok(json!({"from_tick": start, "to_tick": end, "steps": steps}))
}

/// Timeline sample (rounded) of fractional session beat `b`; linear within a tick, since tempo
/// events sit on whole ticks.
fn sample_at_beat(tempo: &TempoMap, sr: SampleRate, b: f64) -> Samples {
    let tick = b * TICKS_PER_QUARTER as f64;
    let k = tick.floor();
    let a = tempo.tick_to_samples(k as i64, sr);
    let n = tempo.tick_to_samples((k as i64).saturating_add(1), sr);
    (a as f64 + (tick - k) * n.saturating_sub(a) as f64).round() as Samples
}

/// Session beat (fractional) at timeline sample `t`.
fn beat_at(tempo: &TempoMap, sr: SampleRate, t: Samples) -> f64 {
    tempo.samples_to_ticks_f(t, sr) / TICKS_PER_QUARTER as f64
}

/// The tempo map and sample rate an edit's timeline positions are measured under.
#[derive(Clone, Copy)]
pub(crate) struct Timebase<'a> {
    tempo: &'a TempoMap,
    sr: SampleRate,
    fp: u64,
}

impl<'a> Timebase<'a> {
    pub(crate) fn new(tempo: &'a TempoMap, sr: SampleRate) -> Self {
        Timebase { tempo, sr, fp: tempo_fingerprint(tempo, sr) }
    }

    /// Session beat (fractional) at timeline sample `t`.
    fn beat(&self, t: Samples) -> f64 {
        beat_at(self.tempo, self.sr, t)
    }

    /// Timeline sample of fractional session beat `b`.
    fn sample(&self, b: f64) -> Samples {
        sample_at_beat(self.tempo, self.sr, b)
    }

    /// Track beats that `d` source-offset samples from timeline sample `at` span.
    fn span(&self, at: Samples, d: Samples) -> f64 {
        self.beat(at.saturating_add(d).clamp(-MAX_POSITION, MAX_POSITION)) - self.beat(at)
    }
}

/// A beat-locked clip's new placement after an edit: clip `clip` of track `track`'s playlist.
pub(crate) struct Relock {
    track: usize,
    clip: usize,
    start: Samples,
    length: Samples,
    anchor: f64,
    lock: WarpLock,
}

/// Where beat-locked clip `c` belongs after an edit that turned timebase `old` into `new`, as
/// (start, length, anchor, lock); `None` when it is already there or isn't a beat-locked audio clip.
///
/// The clip's music was anchored where its lock records it, under the tempo map the lock names.
/// When the source offset moved by `d` since (a trim, a split, a copied part, or a slip edit, which
/// keeps the start and so slides the music), the clip now starts with what played `d` after that
/// old start, wherever the clip sits now: the anchor moves by the beats `d` spanned there. A plain
/// move keeps the offset, so the music moves with the clip. A clip locked under `old` when the
/// tempo changed is put on the same session beats under `new`. A lock taken under `new` already
/// matches (an undo, redo or opened session swapped in a consistent document); one taken under
/// neither map (or none) is only re-recorded.
fn relocked(c: &Clip, old: Timebase, new: Timebase) -> Option<(Samples, Samples, f64, WarpLock)> {
    const EPS: f64 = 1e-9;
    let w = c.warp.as_ref().filter(|_| c.is_audio())?;
    let offset = c.source_offset();
    if w.lock == Some(WarpLock { start: c.start, length: c.length, offset, tempo: new.fp }) {
        return None;
    }
    let under = match w.lock {
        Some(l) if l.tempo == new.fp => Some((l, new)),
        Some(l) if l.tempo == old.fp => Some((l, old)),
        _ => None,
    };
    let (mut start, mut length, mut anchor) = (c.start, c.length, w.anchor);
    if let Some((l, tb)) = under {
        if l.offset != offset {
            anchor += tb.span(l.start, offset.saturating_sub(l.offset));
        }
        if tb.fp != new.fp {
            let (from, to) = (tb.beat(c.start), tb.beat(c.end()));
            start = new.sample(from).clamp(-MAX_POSITION, MAX_POSITION);
            length = new.sample(to).saturating_sub(start).clamp(1, MAX_POSITION);
            // The start rounds to a sample: keep the beat heard at every session beat exact.
            anchor += new.beat(start) - from;
        }
    }
    let lock = WarpLock { start, length, offset, tempo: new.fp };
    let unchanged = start == c.start && length == c.length && (anchor - w.anchor).abs() <= EPS && w.lock == Some(lock);
    (!unchanged).then_some((start, length, anchor, lock))
}

/// What [`apply_relocks`] must change so every beat-locked clip stays on its music after an edit
/// that turned tempo map `old` at rate `old_sr` into the session's (see [`relocked`]); empty when
/// nothing moves. Clips without a recorded lock just get one.
///
/// Not handled yet: a clip that grows with a tempo change may overlap a neighbour that isn't
/// beat-locked; a take placed straight by recording is relocked only by the next command;
/// `event.move_song_start` moves clips on tick-timebase tracks itself and shifts the tempo events,
/// so a beat-locked clip on such a track is moved twice (clips on sample-timebase tracks follow
/// their beats, as with any tempo change).
pub(crate) fn relocks(s: &Session, old: &TempoMap, old_sr: SampleRate) -> Vec<Relock> {
    let new = Timebase::new(&s.tempo, s.sample_rate);
    let old = if old_sr == s.sample_rate && *old == s.tempo { new } else { Timebase::new(old, old_sr) };
    let mut out = Vec::new();
    for (track, tr) in s.tracks.iter().enumerate() {
        let Some(pl) = tr.playlist() else { continue };
        for (clip, c) in pl.clips.iter().enumerate() {
            if let Some((start, length, anchor, lock)) = relocked(c, old, new) {
                out.push(Relock { track, clip, start, length, anchor, lock });
            }
        }
    }
    out
}

/// Applies [`relocks`]' result to the session it was computed from. A tempo change's resize
/// stretches the clip's envelope, fades and sync point with its beats.
pub(crate) fn apply_relocks(s: &mut Session, relocks: &[Relock]) {
    // `relocks` lists clips track by track, so a track's entries are adjacent and comparing with
    // the last one recorded is enough to list each moved track once.
    let mut moved: Vec<usize> = Vec::new();
    for r in relocks {
        let Some(c) = s.tracks.get_mut(r.track).and_then(|t| t.playlist_mut()).and_then(|p| p.clips.get_mut(r.clip)) else { continue };
        if r.start != c.start && moved.last() != Some(&r.track) {
            moved.push(r.track);
        }
        if r.length != c.length {
            c.rescale_to(r.length);
        }
        c.start = r.start;
        if let Some(w) = c.warp.as_mut() {
            w.anchor = r.anchor;
            w.lock = Some(r.lock);
        }
    }
    for t in moved {
        if let Some(pl) = s.tracks.get_mut(t).and_then(|t| t.playlist_mut()) {
            pl.sort();
        }
    }
}

/// Records a lock, under the session's own tempo map, for every beat-locked clip of a session
/// entering the engine outside a command (a new engine, an opened or recovered file), so the
/// first tempo change moves it with its beats. Locks already taken under that map are kept.
pub(crate) fn record_locks(s: &mut Session) {
    let r = relocks(s, &s.tempo, s.sample_rate);
    if !r.is_empty() {
        apply_relocks(s, &r);
    }
}

/// Settles copied parts of beat-locked clips on the clipboard under the current timebase `now`:
/// each part's anchor takes the beats its offset change spanned, and its lock records where that
/// part played. Pasting it later, after a tempo change too, then plays the part that was copied.
pub(crate) fn settle_clipboard(cb: &mut crate::Clipboard, s: &Session) {
    let mut now = None;
    for c in cb.tracks.iter_mut().flatten() {
        let (offset, length, audio) = (c.source_offset(), c.length, c.is_audio());
        let Some(w) = c.warp.as_mut().filter(|_| audio) else { continue };
        let Some(l) = w.lock.filter(|l| l.offset != offset) else { continue };
        let tb = *now.get_or_insert_with(|| Timebase::new(&s.tempo, s.sample_rate));
        if l.tempo != tb.fp {
            continue;
        }
        let d = offset.saturating_sub(l.offset);
        w.anchor += tb.span(l.start, d);
        w.lock = Some(WarpLock { start: l.start.saturating_add(d).clamp(-MAX_POSITION, MAX_POSITION), length, offset, tempo: l.tempo });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_sidecar_that_runs_too_long_is_stopped() {
        let command = |argv: &[&str]| {
            let mut c = std::process::Command::new(argv.first().copied().unwrap_or_default());
            c.args(argv.get(1..).unwrap_or_default());
            c
        };
        let (mut quick, mut slow) = if cfg!(windows) {
            (command(&["cmd", "/C", "echo hi"]), command(&["ping", "-n", "30", "127.0.0.1"]))
        } else {
            (command(&["echo", "hi"]), command(&["sleep", "30"]))
        };
        let out = output_within(&mut quick, std::time::Duration::from_secs(20)).unwrap().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
        let started = std::time::Instant::now();
        assert!(output_within(&mut slow, std::time::Duration::from_millis(300)).unwrap().is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_process_left_holding_the_pipes_does_not_outlast_the_limit() {
        // The shell exits at once, leaving a process it started holding its stdout for 20 s.
        let mut held = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start /b ping -n 20 127.0.0.1"]);
            c
        } else {
            let mut c = std::process::Command::new("sh");
            c.args(["-c", "sleep 20 &"]);
            c
        };
        let started = std::time::Instant::now();
        assert!(output_within(&mut held, std::time::Duration::from_secs(2)).unwrap().is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }
    use crate::edit as ops;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, TrackKind};

    const SR: i64 = 48_000;

    /// A 30 s source with a 125 bpm beat list (a beat every 0.48 s from 0.1 s), played from 1 s
    /// for 20 s at the session's 120 bpm, with its lock recorded.
    fn warped() -> (Engine, TrackId, ClipId) {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: vec![vec![0.1; 30 * SR as usize]] };
        let src = crate::io::add_source(&mut s, "src", buf, None, FileFormat::Wav);
        let id = s.new_clip_id();
        let mut c = Clip::audio(id, "c", src, 0, SR, 20 * SR);
        c.warp = WarpMap::new((0..60).map(|k| 4_800.0 + k as f64 * 23_040.0).collect(), -1.0);
        ops::place_clip(&mut s, t, c);
        let old = s.tempo.clone();
        relock(&mut s, &old);
        (Engine::new(s), t, id)
    }

    /// Relocks `s` after an edit that turned tempo map `old` into its own.
    fn relock(s: &mut Session, old: &TempoMap) {
        let r = relocks(s, old, s.sample_rate);
        apply_relocks(s, &r);
    }

    /// Source sample heard at timeline sample `t` from whichever clip holds `t` (after a split too).
    fn heard_in(s: &Session, t: Samples) -> f64 {
        let c = s.tracks.iter().filter_map(|tr| tr.playlist()).flat_map(|p| p.clips.iter()).find(|c| c.range().contains(t)).unwrap();
        c.warp.as_ref().unwrap().source_at_time(&s.tempo, s.sample_rate, c.start, t)
    }

    fn heard(e: &Engine, t: Samples) -> f64 {
        heard_in(e.session(), t)
    }

    /// Timeline sample of whole session beat `b`.
    fn at_beat(s: &Session, b: i64) -> Samples {
        s.tempo.tick_to_samples(b * TICKS_PER_QUARTER, s.sample_rate)
    }

    /// (start, length, source offset) of every clip on the session's tracks.
    fn clips(e: &Engine) -> Vec<(Samples, Samples, Samples)> {
        let s = e.session();
        s.tracks.iter().filter_map(|tr| tr.playlist()).flat_map(|p| p.clips.iter()).map(|c| (c.start, c.length, c.source_offset())).collect()
    }

    #[test]
    fn grid_from_a_beat_list_puts_the_first_downbeat_on_a_bar_line() {
        // 120 bpm, first downbeat 0.5 s in: one bar of lead-in (4 beats) before it.
        let secs: Vec<f64> = (0..40).map(|k| 0.5 + k as f64 * 0.5).collect();
        let (w, beats) = grid(&secs, 48_000.0, 48_000 * 21, 4, None).unwrap();
        assert_eq!(w.anchor, -4.0);
        assert_eq!(w.beats[0], 24_000.0);
        // The file ends at 21 s: 41 beats after the 0.5 s downbeat, plus the 4 lead-in beats.
        assert!((beats - 45.0).abs() < 1e-9);
    }

    #[test]
    fn a_downbeat_a_few_milliseconds_in_lands_on_the_clip_start() {
        let secs: Vec<f64> = (0..8).map(|k| 0.0045 + k as f64 * 0.5).collect();
        assert_eq!(grid(&secs, 48_000.0, 48_000 * 4, 4, None).unwrap().0.anchor, 0.0);
        // Past the pre-roll it gets a bar of lead-in.
        let secs: Vec<f64> = (0..8).map(|k| 0.07 + k as f64 * 0.5).collect();
        assert_eq!(grid(&secs, 48_000.0, 48_000 * 4, 4, None).unwrap().0.anchor, -4.0);
    }

    #[test]
    fn grid_with_a_downbeat_at_the_file_start_needs_no_lead_in() {
        let secs: Vec<f64> = (0..8).map(|k| k as f64 * 0.5).collect();
        assert_eq!(grid(&secs, 48_000.0, 48_000 * 4, 4, None).unwrap().0.anchor, 0.0);
        assert_eq!(grid(&secs, 48_000.0, 48_000 * 4, 4, Some(0.25)).unwrap().0.anchor, 0.25);
    }

    #[test]
    fn mixforge_output_is_parsed_and_its_errors_kept() {
        let a = parse_analysis("dj.analyze", "{\"bpm\": 145.0, \"first_downbeat\": 0.2, \"camelot\": \"4B\"}\n").unwrap();
        assert_eq!(a["camelot"], "4B");
        let e = parse_analysis("dj.load_warped", r#"{"error":"not analysed by mixforge"}"#).unwrap_err();
        assert!(matches!(&e, EngineError::BadParams(c, m) if c == "dj.load_warped" && m == "not analysed by mixforge"), "{e}");
        assert!(parse_analysis("dj.analyze", "Traceback (most recent call last):\n  File ...").is_err());
        assert!(parse_analysis("dj.analyze", "[1, 2]").is_err());
    }

    #[test]
    fn grid_of_wants_a_usable_bpm_and_downbeat() {
        assert_eq!(grid_of(&json!({"bpm": 145.0, "first_downbeat": 0.2})), Some((145.0, 0.2)));
        assert_eq!(grid_of(&json!({"first_downbeat": 0.2})), None);
        assert_eq!(grid_of(&json!({"bpm": 500.0, "first_downbeat": 0.2})), None);
        assert_eq!(grid_of(&json!({"bpm": null, "first_downbeat": 0.2})), None);
        assert_eq!(grid_of(&json!({"bpm": 145.0, "first_downbeat": -0.1})), None);
    }

    #[test]
    fn load_warped_puts_the_first_downbeat_on_bar_two() {
        let dir = std::env::temp_dir().join(format!("sc-dj-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 10 * SR as usize]] };
        let path = dir.join("track.wav");
        std::fs::write(&path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
        let path = path.to_string_lossy().into_owned();
        let mut e = Engine::default();
        let r = e.execute("dj.load_warped", &json!({"path": path, "bpm": 120, "first_beat": 0.5})).unwrap();
        assert_eq!(r["anchor"], -4.0);
        assert_eq!(r["downbeat_bar"], 2);
        // A bad beat list is refused before anything is imported.
        let tracks = e.session().tracks.len();
        assert!(e.execute("dj.load_warped", &json!({"path": path, "beats": [1.0, "x"]})).is_err());
        assert!(e.execute("dj.load_warped", &json!({"path": path, "beats": [2.0, 1.0]})).is_err());
        assert_eq!(e.session().tracks.len(), tracks);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tempo_ramp_needs_bars_and_a_new_tempo() {
        let mut e = Engine::default();
        e.session_mut().edit.selection = soundcraft_time::Range::new(0, 20 * SR);
        // A start bar without bars doesn't borrow the selection's.
        let err = e.execute("dj.tempo_ramp", &json!({"from_bar": 5, "to_bpm": 130.0})).unwrap_err().to_string();
        assert!(err.contains("`bars`"), "{err}");
        let err = e.execute("dj.tempo_ramp", &json!({"from_bar": 5, "bars": 4, "to_bpm": 120.0})).unwrap_err().to_string();
        assert!(err.contains("already 120 bpm"), "{err}");
        // Over the selection (bars 1..11 at 120 bpm): ten bars.
        let r = e.execute("dj.tempo_ramp", &json!({"to_bpm": 130.0})).unwrap();
        assert_eq!(r["steps"], 40, "{r}");
    }

    #[test]
    fn hostile_beat_lists_are_refused() {
        assert!(grid(&[1.0], 48_000.0, 48_000, 4, None).is_none());
        assert!(grid(&[1.0, 1.0, 2.0], 48_000.0, 48_000, 4, None).is_none());
        assert!(grid(&[f64::NAN, 1.0], 48_000.0, 48_000, 4, None).is_none());
        assert!(grid(&[-1.0, 1.0], 48_000.0, 48_000, 4, None).is_none());
    }

    #[test]
    fn trimming_the_start_keeps_the_music() {
        let (mut e, _, id) = warped();
        let before = heard(&e, 5 * SR);
        e.execute("edit.trim_clip", &json!({"clip": id.0, "start": 3 * SR})).unwrap();
        assert_eq!(clips(&e), vec![(3 * SR, 18 * SR, 2 * SR)]);
        assert!((heard(&e, 5 * SR) - before).abs() < 1.0);
    }

    #[test]
    fn splitting_keeps_the_music() {
        let (mut e, t, _) = warped();
        let (first, before) = (heard(&e, 3 * SR), heard(&e, 8 * SR));
        e.execute("edit.separate", &json!({"tracks": [t.0], "at": 6 * SR})).unwrap();
        assert_eq!(clips(&e), vec![(SR, 5 * SR, 0), (6 * SR, 15 * SR, 5 * SR)]);
        assert!((heard(&e, 3 * SR) - first).abs() < 1.0);
        assert!((heard(&e, 8 * SR) - before).abs() < 1.0);
    }

    #[test]
    fn moving_a_clip_moves_the_music() {
        let (mut e, _, id) = warped();
        let before = heard(&e, 5 * SR);
        e.execute("edit.move_clips", &json!({"clips": [id.0], "by": SR / 2})).unwrap();
        assert_eq!(clips(&e), vec![(SR + SR / 2, 20 * SR, 0)]);
        assert!((heard(&e, 5 * SR + SR / 2) - before).abs() < 1.0);
    }

    #[test]
    fn a_tempo_change_keeps_clips_on_their_beats() {
        // The clip spans session beats 2..42 at 120 bpm.
        let (mut e, _, id) = warped();
        let before = heard(&e, at_beat(e.session(), 10));
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        let s = e.session();
        let c = s.find_clip(id).unwrap().1;
        assert_ne!(c.start, SR, "the tempo change did not move the clip");
        assert_eq!((c.start, c.end()), (at_beat(s, 2), at_beat(s, 42)));
        assert!((heard(&e, at_beat(s, 10)) - before).abs() < 1.0);
    }

    #[test]
    fn a_multi_segment_tempo_map_keeps_the_clip_edges_to_the_sample() {
        let (mut e, _, id) = warped();
        let before = heard(&e, at_beat(e.session(), 30));
        e.execute("event.tempo", &json!({"bpm": 140.0, "at": 5 * SR})).unwrap();
        e.execute("event.tempo", &json!({"bpm": 97.0, "at": 12 * SR})).unwrap();
        let s = e.session();
        assert!(s.tempo.tempos().len() >= 3);
        let c = s.find_clip(id).unwrap().1;
        assert!((c.start - at_beat(s, 2)).abs() <= 1, "start {} vs {}", c.start, at_beat(s, 2));
        assert!((c.end() - at_beat(s, 42)).abs() <= 1, "end {} vs {}", c.end(), at_beat(s, 42));
        assert!((heard(&e, at_beat(s, 30)) - before).abs() < 1.0);
    }

    #[test]
    fn trimming_the_end_keeps_the_music() {
        let (mut e, _, id) = warped();
        let before = heard(&e, 10 * SR);
        e.execute("edit.trim_clip", &json!({"clip": id.0, "end": 15 * SR})).unwrap();
        assert_eq!(clips(&e), vec![(SR, 14 * SR, 0)]);
        assert!((heard(&e, 10 * SR) - before).abs() < 1.0);
    }

    #[test]
    fn duplicating_part_of_a_clip_repeats_that_part() {
        let (mut e, t, _) = warped();
        let before = heard(&e, 5 * SR + SR / 2);
        e.execute("edit.duplicate", &json!({"tracks": [t.0], "start": 5 * SR, "end": 6 * SR})).unwrap();
        assert!(clips(&e).contains(&(6 * SR, SR, 4 * SR)), "{:?}", clips(&e));
        assert!((heard(&e, 6 * SR + SR / 2) - before).abs() < 1.0);
    }

    #[test]
    fn a_part_pasted_far_away_plays_that_part() {
        let (mut e, t, _) = warped();
        let before = heard(&e, 5 * SR + SR / 2);
        e.execute("edit.copy", &json!({"tracks": [t.0], "start": 5 * SR, "end": 6 * SR})).unwrap();
        e.execute("edit.paste", &json!({"tracks": [t.0], "at": 25 * SR})).unwrap();
        assert!(clips(&e).contains(&(25 * SR, SR, 4 * SR)), "{:?}", clips(&e));
        assert!((heard(&e, 25 * SR + SR / 2) - before).abs() < 1.0);
    }

    #[test]
    fn a_split_and_a_tempo_change_in_one_edit_keep_both_halves() {
        let (e, t, _) = warped();
        let mut s = e.session().clone();
        let (first, second) = (heard_in(&s, at_beat(&s, 6)), heard_in(&s, at_beat(&s, 30)));
        let old = s.tempo.clone();
        assert_eq!(ops::separate_at(&mut s, t, 6 * SR), 1);
        s.tempo.set_tempo(0, 140.0).unwrap();
        relock(&mut s, &old);
        let got: Vec<_> = s.tracks.iter().filter_map(|tr| tr.playlist()).flat_map(|p| p.clips.iter()).map(|c| (c.start, c.end())).collect();
        assert_eq!(got, vec![(at_beat(&s, 2), at_beat(&s, 12)), (at_beat(&s, 12), at_beat(&s, 42))]);
        assert!((heard_in(&s, at_beat(&s, 6)) - first).abs() < 1.0);
        assert!((heard_in(&s, at_beat(&s, 30)) - second).abs() < 1.0);
    }

    #[test]
    fn undo_and_redo_of_a_tempo_change_restore_positions_exactly() {
        let (mut e, _, _) = warped();
        let at_120 = (clips(&e), e.session().clone());
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        let at_140 = (clips(&e), e.session().clone());
        assert_ne!(at_120.0, at_140.0);
        // Through the commands, which swap whole documents inside `Engine::execute`.
        for _ in 0..2 {
            e.execute("edit.undo", &json!({})).unwrap();
            assert_eq!(clips(&e), at_120.0);
            assert!(*e.session() == at_120.1, "undo moved beat-locked clips");
            e.execute("edit.redo", &json!({})).unwrap();
            assert_eq!(clips(&e), at_140.0);
            assert!(*e.session() == at_140.1, "redo moved beat-locked clips");
        }
    }

    #[test]
    fn opening_a_session_saved_at_another_tempo_keeps_its_clips() {
        let (mut e, _, _) = warped();
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        let saved = clips(&e);
        let anchors = |e: &Engine| -> Vec<f64> {
            e.session()
                .tracks
                .iter()
                .filter_map(|t| t.playlist())
                .flat_map(|p| p.clips.iter())
                .filter_map(|c| c.warp.as_ref().map(|w| w.anchor))
                .collect()
        };
        let saved_anchors = anchors(&e);
        let dir = std::env::temp_dir().join(format!("sc-dj-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Warped.scraft").to_string_lossy().into_owned();
        e.execute("session.save_as", &json!({"path": path})).unwrap();
        // An engine at 120 bpm, holding beat-locked clips of its own.
        let (mut other, _, _) = warped();
        other.execute("session.open", &json!({"path": path})).unwrap();
        assert_eq!(clips(&other), saved);
        assert_eq!(anchors(&other), saved_anchors);
        assert!(!other.is_dirty(), "opening marked the session modified");
        other.execute("session.revert", &json!({})).unwrap();
        assert_eq!(clips(&other), saved);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_part_copied_before_a_tempo_change_and_pasted_after_plays_that_part() {
        let (mut e, t, _) = warped();
        let beat_heard = |e: &Engine, at: Samples| {
            let s = e.session();
            let c = s.tracks.iter().filter_map(|tr| tr.playlist()).flat_map(|p| p.clips.iter()).find(|c| c.range().contains(at)).unwrap();
            c.warp.as_ref().unwrap().track_beat_at(&s.tempo, s.sample_rate, c.start, at)
        };
        let copied = beat_heard(&e, 5 * SR);
        e.execute("edit.copy", &json!({"tracks": [t.0], "start": 5 * SR, "end": 6 * SR})).unwrap();
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        e.execute("edit.paste", &json!({"tracks": [t.0], "at": 25 * SR})).unwrap();
        assert!(clips(&e).iter().any(|&(start, _, offset)| start == 25 * SR && offset == 4 * SR), "{:?}", clips(&e));
        assert!((beat_heard(&e, 25 * SR) - copied).abs() < 1e-6, "pasted part starts on track beat {} instead of {copied}", beat_heard(&e, 25 * SR));
    }

    #[test]
    fn nudge_moves_the_music_later_by_ms() {
        let (mut e, _, id) = warped();
        let (snapshot, before) = (e.session().clone(), heard(&e, 5 * SR));
        e.execute("dj.nudge", &json!({"clip": id.0, "ms": 10.0})).unwrap();
        // 10 ms later at 48 kHz: what played at 5 s now plays 480 samples later.
        assert!((heard(&e, 5 * SR + 480) - before).abs() < 1.0);
        // The relock pass leaves the nudge alone, and it undoes as one step.
        assert_eq!(clips(&e), vec![(SR, 20 * SR, 0)]);
        assert!(e.undo());
        assert!(*e.session() == snapshot);
        // In beats: a quarter beat at 120 bpm is 125 ms, 6 000 samples.
        e.execute("dj.nudge", &json!({"clip": id.0, "beats": 0.25})).unwrap();
        assert!((heard(&e, 5 * SR + 6_000) - before).abs() < 1.0);
        let rev = e.session().clone();
        for bad in
            [json!({"ms": f64::NAN}), json!({"ms": 1500.0}), json!({"beats": 9.0}), json!({"ms": 5.0, "beats": 0.1}), json!({}), json!({"ms": "5"})]
        {
            let mut p = bad.clone();
            p["clip"] = json!(id.0);
            assert!(e.execute("dj.nudge", &p).is_err(), "{p}");
        }
        assert!(e.execute("dj.nudge", &json!({"clip": 9999, "ms": 5.0})).is_err());
        assert!(*e.session() == rev);
    }

    /// The instruments of a [`drums`] source.
    #[derive(Clone, Copy)]
    struct Kit {
        kick_hz: f64,
        /// Kick decay time constant (seconds).
        kick_decay: f64,
        /// A continuous 55 Hz bassline under everything.
        bass: bool,
    }

    const KIT: Kit = Kit { kick_hz: 60.0, kick_decay: 0.08, bass: false };

    /// A 30 s source with a 125 bpm beat list from 0.1 s whose sound comes `late` seconds after it:
    /// a kick and a 1.5 kHz click on every beat, a noise hat on every off-beat.
    fn drums(late: f64, kit: Kit) -> AudioBuffer {
        let (n, sr) = (30 * SR as usize, SR as f64);
        let tau = std::f64::consts::TAU;
        let mut x: Vec<f32> = (0..n).map(|i| if kit.bass { (0.3 * (tau * 55.0 * i as f64 / sr).sin()) as f32 } else { 0.0 }).collect();
        let mut seed: u32 = 1;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        };
        for k in 0.. {
            let at = ((0.1 + k as f64 * 0.24 + late) * sr).round() as usize;
            if at >= n {
                break;
            }
            for j in 0..(0.4 * sr) as usize {
                let (t, Some(y)) = (j as f64 / sr, x.get_mut(at + j)) else { break };
                let v = if k % 2 == 0 {
                    0.5 * (tau * kit.kick_hz * t).sin() * (-t / kit.kick_decay).exp() + 0.3 * (tau * 1_500.0 * t).sin() * (-t / 0.005).exp()
                } else {
                    0.3 * f64::from(noise()) * (-t / 0.007).exp()
                };
                *y += v as f32;
            }
        }
        AudioBuffer { sample_rate: SR as u32, channels: vec![x] }
    }

    /// A transition in a session at 128 bpm (`step`: then this bpm from this session beat on):
    /// clip A plays source `a` on session beats 2..42 and clip B source `b` from session beat
    /// `from_b` for 40 beats, starting on track beat `anchor_b`. Both have the 125 bpm beat list
    /// of [`drums`].
    fn transition_of(a: AudioBuffer, b: AudioBuffer, from_b: i64, anchor_b: f64, step: Option<(i64, f64)>) -> (Engine, ClipId, ClipId) {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        if let Some((beat, bpm)) = step {
            s.tempo.set_tempo(beat * TICKS_PER_QUARTER, bpm).unwrap();
        }
        let mut ids = Vec::new();
        for (buf, from, anchor) in [(a, 2, 0.0), (b, from_b, anchor_b)] {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            let src = crate::io::add_source(&mut s, "drums", buf, None, FileFormat::Wav);
            let id = s.new_clip_id();
            let start = at_beat(&s, from);
            let mut c = Clip::audio(id, "c", src, 0, start, at_beat(&s, from + 40) - start);
            c.warp = WarpMap::new((0..62).map(|k| 4_800.0 + k as f64 * 23_040.0).collect(), anchor);
            ops::place_clip(&mut s, t, c);
            ids.push(id);
        }
        let old = s.tempo.clone();
        relock(&mut s, &old);
        (Engine::new(s), ids[0], ids[1])
    }

    /// [`transition_of`] two [`KIT`] sources, B's sound `late_b` seconds late.
    fn transition(late_b: f64, from_b: i64) -> (Engine, ClipId, ClipId) {
        transition_of(drums(0.0, KIT), drums(late_b, KIT), from_b, 0.0, None)
    }

    fn suggest(e: &mut Engine, a: ClipId, b: ClipId) -> Result<Value> {
        e.execute("dj.suggest_nudge", &json!({"out_clip": a.0, "in_clip": b.0}))
    }

    /// The suggestion's nudge_ms, asserting it is reliable.
    fn reliable_ms(r: &Value) -> f64 {
        assert_eq!(r["reliable"], true, "{r}");
        r["nudge_ms"].as_f64().unwrap()
    }

    #[test]
    fn suggest_nudge_measures_a_late_incoming_track_and_the_nudge_closes_the_loop() {
        let (mut e, a, b) = transition(0.030, 18);
        let before = e.session().clone();
        let r = suggest(&mut e, a, b).unwrap();
        assert!(*e.session() == before, "the suggestion changed the session");
        // 30 ms of 125 bpm source is heard over 30 * 125 / 128 ms at 128 bpm.
        let want = -30.0 * 125.0 / 128.0;
        let got = reliable_ms(&r);
        assert!((got - want).abs() < 2.0, "{r}");
        assert!(r["agree"].as_u64().unwrap() >= 2, "{r}");
        assert!((r["overlap_beats"].as_f64().unwrap() - 24.0).abs() < 1e-6, "{r}");
        e.execute("dj.nudge", &json!({"clip": b.0, "ms": got})).unwrap();
        let again = reliable_ms(&suggest(&mut e, a, b).unwrap());
        assert!(again.abs() < 2.0, "after the nudge: {again}");
    }

    #[test]
    fn suggest_nudge_resolves_lags_between_bins() {
        // 20 ms of source is 5.33 bins (3.66 ms each) at 128 bpm: the nearest bin alone is 1.2 ms off,
        // the parabola through the peak gets within 1 ms.
        let (mut e, a, b) = transition(0.020, 18);
        let r = suggest(&mut e, a, b).unwrap();
        assert!((reliable_ms(&r) + 20.0 * 125.0 / 128.0).abs() < 1.0, "{r}");
        // An early incoming track asks for a later nudge.
        let (mut e, a, b) = transition(-0.012, 18);
        let r = suggest(&mut e, a, b).unwrap();
        assert!((reliable_ms(&r) - 12.0 * 125.0 / 128.0).abs() < 1.0, "{r}");
    }

    #[test]
    fn suggest_nudge_is_right_or_unsure_far_off_the_beat() {
        // Over a quarter and nearly half a beat late at 128 bpm (a beat is 469 ms).
        for late in [0.130, 0.200] {
            let (mut e, a, b) = transition(late, 18);
            let r = suggest(&mut e, a, b).unwrap();
            let want = -late * 1000.0 * 125.0 / 128.0;
            let right = (r["nudge_ms"].as_f64().unwrap() - want).abs() < 2.0;
            assert!(right || r["reliable"] == false, "a confident wrong answer for {late} s late: {r}");
        }
        // Exactly half a beat (of the 125 bpm source) late: early or late is a coin toss.
        let (mut e, a, b) = transition(0.240, 18);
        let r = suggest(&mut e, a, b).unwrap();
        assert_eq!(r["reliable"], false, "{r}");
        assert!(r["note"].as_str().unwrap().contains("half a beat"), "{r}");
    }

    #[test]
    fn suggest_nudge_finds_nothing_to_fix_between_identical_tracks() {
        let (mut e, a, b) = transition(0.0, 10);
        let r = suggest(&mut e, a, b).unwrap();
        assert!(reliable_ms(&r).abs() < 2.0, "{r}");
        assert_eq!(r["agree"], 3, "{r}");
    }

    #[test]
    fn suggest_nudge_follows_kicks_not_bass_waveforms() {
        // Different kicks over the same bassline: the low band must follow the kicks' envelopes.
        let (kick_a, kick_b) = (Kit { bass: true, ..KIT }, Kit { kick_hz: 45.0, kick_decay: 0.15, bass: true });
        let (mut e, a, b) = transition_of(drums(0.0, kick_a), drums(0.0, kick_b), 18, 0.0, None);
        let r = suggest(&mut e, a, b).unwrap();
        assert!(reliable_ms(&r).abs() < 2.0, "{r}");
        let (mut e, a, b) = transition_of(drums(0.0, kick_a), drums(0.030, kick_b), 18, 0.0, None);
        let r = suggest(&mut e, a, b).unwrap();
        let want = -30.0 * 125.0 / 128.0;
        assert!((reliable_ms(&r) - want).abs() < 2.0, "{r}");
        assert!((r["bands"]["low"].as_f64().unwrap() - want).abs() < 5.0, "{r}");
    }

    #[test]
    fn suggest_nudge_handles_a_lead_in_before_the_first_beat() {
        // B starts a bar before its first beat.
        let (mut e, a, b) = transition_of(drums(0.0, KIT), drums(0.030, KIT), 14, -4.0, None);
        let r = suggest(&mut e, a, b).unwrap();
        let got = reliable_ms(&r);
        assert!((got + 30.0 * 125.0 / 128.0).abs() < 2.0, "{r}");
        e.execute("dj.nudge", &json!({"clip": b.0, "ms": got})).unwrap();
        assert!(reliable_ms(&suggest(&mut e, a, b).unwrap()).abs() < 2.0);
    }

    #[test]
    fn suggest_nudge_round_trips_across_a_tempo_change_in_the_overlap() {
        let (mut e, a, b) = transition_of(drums(0.0, KIT), drums(0.030, KIT), 18, 0.0, Some((26, 140.0)));
        let r = suggest(&mut e, a, b).unwrap();
        // Lateness is constant in beats: 30 ms of 125 bpm source is 0.0625 beats.
        assert!((r["nudge_beats"].as_f64().unwrap() + 0.0625).abs() < 0.004, "{r}");
        // Converted at the incoming clip's start tempo (128 bpm), as dj.nudge does.
        let got = reliable_ms(&r);
        assert!((got + 30.0 * 125.0 / 128.0).abs() < 2.0, "{r}");
        e.execute("dj.nudge", &json!({"clip": b.0, "ms": got})).unwrap();
        let again = reliable_ms(&suggest(&mut e, a, b).unwrap());
        assert!(again.abs() < 0.5, "after the nudge: {again}");
    }

    #[test]
    fn suggest_nudge_needs_an_overlap_of_beat_locked_clips() {
        let (mut e, a, b) = transition(0.0, 50);
        assert!(suggest(&mut e, a, b).is_err());
        // Four beats of overlap are too few.
        let (mut e, a, b) = transition(0.0, 38);
        assert!(suggest(&mut e, a, b).is_err());
        assert!(e.execute("dj.suggest_nudge", &json!({"out_clip": a.0})).is_err());
        assert!(e.execute("dj.suggest_nudge", &json!({"out_clip": a.0, "in_clip": 9999})).is_err());
    }

    #[test]
    fn a_consistent_session_is_left_alone() {
        let (mut e, _, id) = warped();
        // Off the tick grid (a tick is 25 samples here), so rounding to ticks would show.
        e.execute("edit.move_clips", &json!({"clips": [id.0], "by": 7})).unwrap();
        let tempo = e.session().tempo.clone();
        assert!(relocks(e.session(), &tempo, e.session().sample_rate).is_empty());
        // A no-op edit leaves the document alone.
        let before = e.session().clone();
        let mut s = before.clone();
        relock(&mut s, &tempo);
        assert!(s == before, "relock changed a consistent session");
    }

    /// Every beat-locked clip's lock (None when it has none).
    fn locks(e: &Engine) -> Vec<Option<WarpLock>> {
        e.session().tracks.iter().filter_map(|t| t.playlist()).flat_map(|p| p.clips.iter()).filter_map(|c| c.warp.as_ref().map(|w| w.lock)).collect()
    }

    /// After loading, the clip spanning beats 2..42 follows its beats through a tempo change.
    fn follows_a_tempo_change(mut e: Engine, id: ClipId) {
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        let s = e.session();
        let c = s.find_clip(id).unwrap().1;
        assert_eq!((c.start, c.end()), (at_beat(s, 2), at_beat(s, 42)));
    }

    #[test]
    fn a_new_engine_records_locks_without_marking_itself_modified() {
        let (e, _, id) = warped();
        let mut s = e.session().clone();
        s.find_clip_mut(id).unwrap().warp.as_mut().unwrap().lock = None;
        let e = Engine::new(s);
        assert!(locks(&e).iter().all(Option::is_some));
        assert!(!e.is_dirty());
        follows_a_tempo_change(e, id);
    }

    #[test]
    fn opening_a_file_without_locks_records_them() {
        let (mut e, _, id) = warped();
        e.session_mut().find_clip_mut(id).unwrap().warp.as_mut().unwrap().lock = None;
        let dir = std::env::temp_dir().join(format!("sc-dj-unlocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Unlocked.scraft").to_string_lossy().into_owned();
        crate::io::save_session(&mut e, &path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"lock\""), "the saved file still has a lock");
        let mut opened = Engine::default();
        crate::io::open_session(&mut opened, &path).unwrap();
        assert!(locks(&opened).iter().all(Option::is_some));
        assert!(!opened.is_dirty());
        follows_a_tempo_change(opened, id);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
