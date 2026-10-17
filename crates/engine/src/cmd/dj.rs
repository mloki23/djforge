//! DJ commands: beat-locked track loading and tempo ramps.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{MAX_WARP_BEATS, WarpMap};
use soundcraft_time::TICKS_PER_QUARTER;

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
