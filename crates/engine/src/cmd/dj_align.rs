//! Grid phase of a transition: how far apart two overlapping beat-locked clips put their beats,
//! measured from band-matched onset profiles folded onto the session's beats.

use super::*;
use soundcraft_dsp::biquad::{Biquad, FilterType};

/// Bins per beat of a folded onset profile.
const BINS: usize = 128;
/// Source samples per energy frame.
const HOP: usize = 48;
/// Largest lag tried, in bins (a quarter beat either way).
const MAX_LAG: i64 = BINS as i64 / 4;
/// Fewest session beats of overlap measured.
const MIN_BEATS: f64 = 8.0;
/// Source audio read on each side of the overlap so the filters settle (seconds).
const PAD_SECS: f64 = 0.5;
/// A band's own lag "agrees" with the combined one within this many milliseconds.
const AGREE_MS: f64 = 10.0;
/// Names of the bands, in [`band_filters`]' order.
const BANDS: [&str; 3] = ["low", "mid", "high"];

/// `dj.suggest_nudge {out_clip, in_clip}`: the nudge (ms, for `dj.nudge` on `in_clip`) that puts
/// the incoming clip's beats on the outgoing clip's across their overlap. Changes nothing.
pub(super) fn suggest_nudge(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.suggest_nudge";
    let id = |key: &str| p.get(key).and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(CMD, format!("`{key}` required")));
    let (out_id, in_id) = (id("out_clip")?, id("in_clip")?);
    let s = e.session();
    let (a, b) = (side(s, CMD, out_id)?, side(s, CMD, in_id)?);
    let overlap = a.0.range().intersect(&b.0.range()).filter(|r| !r.is_empty()).ok_or_else(|| bad(CMD, "the clips don't overlap"))?;
    let (b0, b1) = (beat_at(&s.tempo, s.sample_rate, overlap.start), beat_at(&s.tempo, s.sample_rate, overlap.end));
    if b1 - b0 < MIN_BEATS {
        return Err(bad(CMD, format!("the clips overlap for {:.1} beats; at least {MIN_BEATS} are needed", b1 - b0)));
    }
    let (pa, pb) = (profiles(s, a, overlap, b0, b1), profiles(s, b, overlap, b0, b1));
    let pairs: Vec<Option<(&[f64], &[f64])>> = pa.iter().zip(&pb).map(|(x, y)| Some((x.as_deref()?, y.as_deref()?))).collect();
    if pairs.iter().all(Option::is_none) {
        return Err(bad(CMD, "neither clip has onsets both sides share in the overlap"));
    }
    let mid = overlap.start.saturating_add(overlap.len() / 2);
    let bpm = s.tempo.tempo_at_tick(s.tempo.samples_to_ticks(mid, s.sample_rate));
    let beat_ms = 60_000.0 / bpm;
    let combined = lag_scores(pairs.iter().flatten().copied());
    let used = pairs.iter().flatten().count() as f64;
    let (lag, score) = best_lag(&combined);
    let nudge_ms = -lag / BINS as f64 * beat_ms;
    let mut bands = serde_json::Map::new();
    let mut agree = 0;
    for (name, pair) in BANDS.iter().zip(&pairs) {
        let v = pair.map(|pr| -best_lag(&lag_scores([pr])).0 / BINS as f64 * beat_ms);
        agree += usize::from(v.is_some_and(|ms| (ms - nudge_ms).abs() <= AGREE_MS));
        bands.insert((*name).to_string(), v.map_or(Value::Null, |ms| json!(ms)));
    }
    Ok(json!({"nudge_ms": nudge_ms, "score": score / used, "agree": agree, "bands": bands, "beats": b1 - b0}))
}

/// A beat-locked audio clip with its warp map and source channels.
type Side<'a> = (&'a Clip, &'a WarpMap, &'a [Vec<f32>]);

/// Clip `id` of `s` as a [`Side`], or command `cmd`'s error naming what it lacks.
fn side<'a>(s: &'a Session, cmd: &str, id: ClipId) -> Result<Side<'a>> {
    let c = s.find_clip(id).map(|(_, c)| c).ok_or_else(|| bad(cmd, format!("no clip {}", id.0)))?;
    let w = c.warp.as_ref().filter(|w| w.is_usable() && c.is_audio()).ok_or_else(|| bad(cmd, format!("clip {} is not beat-locked", id.0)))?;
    let audio = c.source().and_then(|src| s.pool.get(src)).filter(|a| !a.buffer.channels.is_empty());
    let audio = audio.ok_or_else(|| bad(cmd, format!("clip {} has no audio", id.0)))?;
    Ok((c, w, &audio.buffer.channels))
}

/// The filter cascade of band `k` (see [`BANDS`]): each 12 dB/oct section twice, for 24 dB/oct.
fn band_filters(k: usize, sr: f32) -> Vec<Biquad> {
    let q = std::f32::consts::FRAC_1_SQRT_2;
    let sections: &[(FilterType, f32)] = match k {
        0 => &[(FilterType::LowPass, 150.0)],
        1 => &[(FilterType::HighPass, 300.0), (FilterType::LowPass, 3_000.0)],
        _ => &[(FilterType::HighPass, 5_000.0)],
    };
    sections.iter().flat_map(|&(kind, f)| [Biquad::design(kind, f, q, 0.0, sr); 2]).collect()
}

/// Per band, the unit-norm, zero-mean onset profile of side `(c, w, audio)` over the overlap
/// (timeline samples `overlap`, session beats `b0..b1`) folded onto the session beat; `None` for a
/// band with no onsets there.
///
/// The source span the overlap plays (plus [`PAD_SECS`] each side) is mixed to mono and filtered;
/// every [`HOP`] samples the log energy's rise is the onset strength at the frame boundary, whose
/// source position the warp map turns into the session beat it is heard on.
fn profiles(s: &Session, (c, w, audio): Side<'_>, overlap: Range, b0: f64, b1: f64) -> [Option<Vec<f64>>; 3] {
    let (tempo, sr) = (&s.tempo, s.sample_rate);
    let rate = sr.as_f64();
    let frames = audio.iter().map(Vec::len).max().unwrap_or(0);
    let (p0, p1) = (w.source_at_time(tempo, sr, c.start, overlap.start), w.source_at_time(tempo, sr, c.start, overlap.end));
    let clamp = |x: f64| if x.is_finite() { x.clamp(0.0, frames as f64) as usize } else { 0 };
    let (lo, hi) = (clamp(p0.min(p1) - PAD_SECS * rate), clamp(p0.max(p1) + PAD_SECS * rate));
    // Where in its session beat (in bins) each frame boundary of the span is heard, when inside
    // the overlap.
    let shift = beat_at(tempo, sr, c.start) - w.anchor;
    let bins: Vec<Option<f64>> = (0..(hi - lo) / HOP)
        .map(|k| {
            let sb = w.beat_at_source((lo + k * HOP) as f64) + shift;
            ((b0..b1).contains(&sb)).then(|| sb.rem_euclid(1.0) * BINS as f64)
        })
        .collect();
    let gain = 1.0 / audio.len() as f32;
    std::array::from_fn(|band| {
        let mut filters = band_filters(band, rate as f32);
        let mut prof = vec![0.0f64; BINS];
        let mut prev: Option<f64> = None;
        for (k, bin) in bins.iter().enumerate() {
            let mut sum = 0.0f64;
            for i in lo + k * HOP..lo + (k + 1) * HOP {
                let x = audio.iter().map(|ch| ch.get(i).copied().unwrap_or(0.0)).sum::<f32>() * gain;
                let y = filters.iter_mut().fold(x, |x, f| f.process_sample(x));
                sum += f64::from(y) * f64::from(y);
            }
            let level = ((sum / HOP as f64).sqrt() + 1e-6).ln();
            if let (Some(p), Some(b)) = (prev, bin) {
                deposit(&mut prof, *b, (level - p).max(0.0));
            }
            prev = Some(level);
        }
        unit(prof)
    })
}

/// Adds `v` at fractional bin `pos` of circular profile `prof`, shared linearly between the two
/// nearest bins so an onset on a bin edge counts the same whichever side rounding puts it.
fn deposit(prof: &mut [f64], pos: f64, v: f64) {
    let n = prof.len();
    if n == 0 || !pos.is_finite() {
        return;
    }
    let i = pos.floor();
    let frac = pos - i;
    let i = (i as i64).rem_euclid(n as i64) as usize;
    if let Some(x) = prof.get_mut(i) {
        *x += (1.0 - frac) * v;
    }
    if let Some(x) = prof.get_mut((i + 1) % n) {
        *x += frac * v;
    }
}

/// `v` minus its mean, scaled to unit length; `None` when it is flat.
fn unit(mut v: Vec<f64>) -> Option<Vec<f64>> {
    let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
    v.iter_mut().for_each(|x| *x -= mean);
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    (norm > 1e-9).then(|| v.into_iter().map(|x| x / norm).collect())
}

/// Score of each lag `-MAX_LAG..=MAX_LAG` (index `lag + MAX_LAG`): the summed circular
/// correlation `Σ a[i]·b[i + lag]` of the profile pairs, high when `b` is `lag` bins later than `a`.
fn lag_scores<'a>(pairs: impl IntoIterator<Item = (&'a [f64], &'a [f64])>) -> Vec<f64> {
    let mut scores = vec![0.0; (2 * MAX_LAG + 1) as usize];
    for (a, b) in pairs {
        for (slot, lag) in scores.iter_mut().zip(-MAX_LAG..=MAX_LAG) {
            *slot +=
                a.iter().enumerate().map(|(i, x)| x * b.get((i as i64 + lag).rem_euclid(BINS as i64) as usize).copied().unwrap_or(0.0)).sum::<f64>();
        }
    }
    scores
}

/// The best lag in bins (refined between bins by a parabola through the peak and its neighbours)
/// and its score.
fn best_lag(scores: &[f64]) -> (f64, f64) {
    let (i, &peak) = scores.iter().enumerate().fold((0, &f64::NEG_INFINITY), |best, x| if x.1 > best.1 { x } else { best });
    let refine = match (i.checked_sub(1).and_then(|j| scores.get(j)), scores.get(i + 1)) {
        (Some(&l), Some(&r)) if l - 2.0 * peak + r < 0.0 => (0.5 * (l - r) / (l - 2.0 * peak + r)).clamp(-0.5, 0.5),
        _ => 0.0,
    };
    (i as f64 - MAX_LAG as f64 + refine, peak)
}
