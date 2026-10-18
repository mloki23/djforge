//! DJ commands: beat-locked track loading and tempo ramps.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{Clip, MAX_WARP_BEATS, Session, WarpLock, WarpMap};
use soundcraft_time::{SampleRate, TICKS_PER_QUARTER, TempoMap};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "dj.load_warped",
            "Load Track Beat-Locked",
            [],
            None,
            "{path, bpm, first_beat: seconds of the first analysed beat, at_bar?: 1 (session bar the clip starts on), from_beat?: track beat playing at the clip start (default: the whole beat at or before the start of the file, negative when the file has audio before first_beat), track?} — imports an audio file whose beat grid follows the session tempo map in realtime, pitch preserved",
            always,
            load_warped
        ),
        cmd!(
            "dj.tempo_ramp",
            "Tempo Ramp",
            [],
            None,
            "{from_bar, bars, from_bpm, to_bpm} — glides the session tempo from `from_bpm` to `to_bpm` over `bars`, one tempo step per beat, holding `to_bpm` afterwards",
            always,
            tempo_ramp
        ),
        cmd!(
            "dj.crossfade",
            "Crossfade Transition",
            [],
            None,
            "{out_clip, in_clip, shape?: equal power} — fades `out_clip` out and `in_clip` in across the span where they overlap",
            always,
            crossfade
        ),
    ]
}

fn crossfade(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.crossfade";
    let id = |key: &str| p.get(key).and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(CMD, format!("`{key}` required")));
    let (out_id, in_id) = (id("out_clip")?, id("in_clip")?);
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
    let path = str_param(p, "path").ok_or_else(|| bad(CMD, "`path` required"))?.to_string();
    let bpm = f64_or(p, "bpm", f64::NAN);
    if !(bpm.is_finite() && (20.0..=400.0).contains(&bpm)) {
        return Err(bad(CMD, "`bpm` must be 20..=400"));
    }
    let first_beat = f64_or(p, "first_beat", 0.0);
    if !(first_beat.is_finite() && (0.0..=600.0).contains(&first_beat)) {
        return Err(bad(CMD, "`first_beat` must be 0..=600 seconds"));
    }
    let at_bar = i64_or(p, "at_bar", 1).clamp(-10_000, 100_000);
    let target = track_param(e, CMD, p, "track")?;
    let sr = e.session().sample_rate;
    let start_tick = e.session().tempo.bar_start_tick(at_bar);
    let at = e.session().tempo.tick_to_samples(start_tick, sr);

    let bytes = crate::io::read_file(&path)?;
    let name = std::path::Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string();
    let r = crate::io::import_audio_bytes(e, &name, &bytes, Some(path.as_str()), target, at)?;
    let clip = r.get("clip").and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(CMD, "import placed no clip"))?;
    let frames = r.get("frames").and_then(Value::as_i64).unwrap_or(0);

    // Beat grid in source-pool samples (the import resampled the file to the session rate).
    let rate = sr.as_f64();
    let beat_len = 60.0 / bpm * rate;
    let first = first_beat * rate;
    let count = ((frames as f64 - first) / beat_len).ceil().max(2.0) as usize + 1;
    let beats: Vec<f64> = (0..count.min(MAX_WARP_BEATS)).map(|i| first + i as f64 * beat_len).collect();
    let anchor = match p.get("from_beat").and_then(Value::as_f64) {
        Some(b) if b.is_finite() => b,
        // Whole beats back from the first beat, so the track's beats land on session beats; the
        // file's first sample then plays slightly after the clip start.
        _ => -(first / beat_len).ceil(),
    };
    let warp = WarpMap::new(beats, anchor).ok_or_else(|| bad(CMD, "could not build a beat grid"))?;
    // The clip lasts until the file's last sample, counted in session beats from its start.
    let track_beats = (frames as f64 - first) / beat_len - anchor;
    let end_tick = start_tick.saturating_add((track_beats.max(0.0) * TICKS_PER_QUARTER as f64) as i64);
    let length = e.session().tempo.tick_to_samples(end_tick, sr).saturating_sub(at).max(1);
    let s = e.session_mut();
    let c = s.find_clip_mut(clip).ok_or_else(|| bad(CMD, "imported clip vanished"))?;
    c.warp = Some(warp);
    c.length = length;
    Ok(json!({"track": r.get("track"), "clip": clip, "start": at, "length": length, "beats": track_beats, "anchor": anchor}))
}

fn tempo_ramp(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.tempo_ramp";
    let from_bar = i64_or(p, "from_bar", 1).clamp(1, 100_000);
    let bars = i64_or(p, "bars", 0);
    if !(1..=256).contains(&bars) {
        return Err(bad(CMD, "`bars` must be 1..=256"));
    }
    let (a, b) = (f64_or(p, "from_bpm", f64::NAN), f64_or(p, "to_bpm", f64::NAN));
    if ![a, b].iter().all(|v| v.is_finite() && (20.0..=400.0).contains(v)) {
        return Err(bad(CMD, "`from_bpm` and `to_bpm` must be 20..=400"));
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

/// Where a beat-locked clip belongs after an edit.
struct Relocked {
    start: Samples,
    length: Samples,
    anchor: f64,
    lock: WarpLock,
}

/// Where beat-locked clip `c` belongs after an edit, or `None` when it is already there (or isn't
/// beat-locked).
///
/// A tempo change moves the clip so it starts and ends on the same session beats as before. A
/// trim or split (the source offset changed) and any tempo change move the anchor by the beats
/// the clip start moved, so the beat heard at every session beat stays the same. A plain move
/// changes neither, so the music moves with the clip. A clip without a recorded lock just gets one.
fn relocked(c: &Clip, tempo: &TempoMap, sr: SampleRate, tempo_changed: bool) -> Option<Relocked> {
    const EPS: f64 = 1e-9;
    let w = c.warp.as_ref()?;
    let beat_at = |t: Samples| tempo.samples_to_ticks_f(t, sr) / TICKS_PER_QUARTER as f64;
    let offset = c.source_offset();
    let (mut start, mut length, mut shift) = (c.start, c.length, 0.0);
    if let Some(lock) = w.lock {
        if tempo_changed {
            start = sample_at_beat(tempo, sr, lock.start_beat);
            length = sample_at_beat(tempo, sr, lock.end_beat).saturating_sub(start).max(1);
        }
        if tempo_changed || offset != lock.offset {
            shift = beat_at(start) - lock.start_beat;
        }
    }
    let lock = WarpLock { start_beat: beat_at(start), end_beat: beat_at(start.saturating_add(length)), offset };
    let anchor = w.anchor + shift;
    let same_lock =
        w.lock.is_some_and(|l| l.offset == lock.offset && (l.start_beat - lock.start_beat).abs() <= EPS && (l.end_beat - lock.end_beat).abs() <= EPS);
    let unchanged = start == c.start && length == c.length && (anchor - w.anchor).abs() <= EPS && same_lock;
    (!unchanged).then_some(Relocked { start, length, anchor, lock })
}

/// True when [`relock`] would change any clip.
pub(crate) fn relock_needed(s: &Session, tempo_changed: bool) -> bool {
    s.tracks.iter().filter_map(|t| t.playlist()).any(|p| p.clips.iter().any(|c| relocked(c, &s.tempo, s.sample_rate, tempo_changed).is_some()))
}

/// Keeps every beat-locked clip on its music after an edit (see [`relocked`]).
pub(crate) fn relock(s: &mut Session, tempo_changed: bool) {
    let sr = s.sample_rate;
    let tempo = s.tempo.clone();
    for tr in &mut s.tracks {
        let Some(pl) = tr.playlist_mut() else { continue };
        let mut moved = false;
        for c in &mut pl.clips {
            let Some(r) = relocked(c, &tempo, sr, tempo_changed) else { continue };
            if r.start != c.start || r.length != c.length {
                moved |= r.start != c.start;
                c.start = r.start;
                c.length = r.length;
                c.clamp_fades();
            }
            if let Some(w) = c.warp.as_mut() {
                w.anchor = r.anchor;
                w.lock = Some(r.lock);
            }
        }
        if moved {
            pl.sort();
        }
    }
}

/// True when any clip in the session is beat-locked.
pub(crate) fn has_warped(s: &Session) -> bool {
    s.tracks.iter().filter_map(|t| t.playlist()).any(|p| p.clips.iter().any(|c| c.warp.is_some()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
        relock(&mut s, false);
        (Engine::new(s), t, id)
    }

    /// Source sample heard at timeline sample `t` from whichever clip holds `t` (after a split too).
    fn heard(e: &Engine, t: Samples) -> f64 {
        let s = e.session();
        let c = s.tracks.iter().filter_map(|tr| tr.playlist()).flat_map(|p| p.clips.iter()).find(|c| c.range().contains(t)).unwrap();
        c.warp.as_ref().unwrap().source_at_time(&s.tempo, s.sample_rate, c.start, t)
    }

    /// (start, length, source offset) of every clip on the session's tracks.
    fn clips(e: &Engine) -> Vec<(Samples, Samples, Samples)> {
        let s = e.session();
        s.tracks.iter().filter_map(|tr| tr.playlist()).flat_map(|p| p.clips.iter()).map(|c| (c.start, c.length, c.source_offset())).collect()
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
        let (mut e, _, id) = warped();
        let at_beat = |e: &Engine, b: f64| e.session().tempo.tick_to_samples((b * TICKS_PER_QUARTER as f64) as i64, e.session().sample_rate);
        let before = heard(&e, at_beat(&e, 10.0));
        let end_before = e.session().tempo.samples_to_ticks_f(e.session().find_clip(id).unwrap().1.end(), e.session().sample_rate);
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        assert_ne!(e.session().find_clip(id).unwrap().1.start, SR, "the tempo change did not move the clip");
        assert!((heard(&e, at_beat(&e, 10.0)) - before).abs() < 2.0);
        let s = e.session();
        let end_after = s.tempo.samples_to_ticks_f(s.find_clip(id).unwrap().1.end(), s.sample_rate);
        assert!((end_after - end_before).abs() < 2.0, "clip end moved from tick {end_before} to {end_after}");
    }

    #[test]
    fn a_consistent_session_is_left_alone() {
        let (mut e, _, id) = warped();
        // Off the tick grid (a tick is 25 samples here), so rounding to ticks would show.
        e.execute("edit.move_clips", &json!({"clips": [id.0], "by": 7})).unwrap();
        let (before, positions) = (e.session().clone(), clips(&e));
        assert!(!relock_needed(e.session(), true));
        assert!(!relock_needed(e.session(), false));
        relock(e.session_mut(), true);
        assert_eq!(clips(&e), positions);
        assert!(*e.session() == before, "relock changed a consistent session");
    }
}
