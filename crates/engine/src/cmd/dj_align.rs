//! Grid phase of a transition: how far apart two overlapping beat-locked clips put their beats,
//! measured from band-matched onset profiles folded onto the session's beats.

use super::*;
use soundcraft_dsp::biquad::{Biquad, FilterType};

/// Bins per beat of a folded onset profile.
const BINS: usize = 128;
/// Source samples per onset frame.
const HOP: usize = 48;
/// Largest lag tried, in bins: half a beat either way. Lags of ±half a beat are the same circular
/// shift, so a best lag there is ambiguous.
const MAX_LAG: i64 = BINS as i64 / 2;
/// Fewest session beats of overlap measured.
const MIN_BEATS: f64 = 8.0;
/// Most session beats analysed: the last this many of the overlap.
const MAX_BEATS: f64 = 128.0;
/// Source audio read on each side of the overlap so the filters settle (seconds).
const PAD_SECS: f64 = 0.5;
/// A band's own lag "agrees" with the combined one within this many milliseconds.
const AGREE_MS: f64 = 10.0;
/// A second peak scoring more than this fraction of the best makes the suggestion ambiguous.
const AMBIGUOUS: f64 = 0.8;
/// Names of the bands, in [`band_filters`]' order.
const BANDS: [&str; 3] = ["low", "mid", "high"];
/// Energy envelope smoothing per band (seconds): at least a period of the band's lowest frequency,
/// so the envelope follows the notes rather than the waveform.
const SMOOTH_SECS: [f64; 3] = [0.020, 0.010, 0.010];

/// `dj.suggest_nudge {out_clip, in_clip}`: the nudge for `dj.nudge` on `in_clip` that puts the
/// incoming clip's beats on the outgoing clip's across (the last [`MAX_BEATS`] of) their overlap.
/// Changes nothing.
///
/// The lag is measured in session beats (`nudge_beats`, exact for `dj.nudge {beats}`);
/// `nudge_ms` converts it at the tempo of the incoming clip's start, as `dj.nudge {ms}` does.
/// `reliable` is false, with the reasons in `note`, when the answer may be another beat phase:
/// the best lag is at the edge of the half-beat search or not a true peak, another peak scores
/// nearly as well (`ambiguity`: second best over best), or fewer than two bands agree.
pub(super) fn suggest_nudge(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.suggest_nudge";
    let id = |key: &str| p.get(key).and_then(Value::as_u64).map(ClipId).ok_or_else(|| bad(CMD, format!("`{key}` required")));
    let (out_id, in_id) = (id("out_clip")?, id("in_clip")?);
    let s = e.session();
    let (tempo, sr) = (&s.tempo, s.sample_rate);
    let (a, b) = (side(s, CMD, out_id)?, side(s, CMD, in_id)?);
    let overlap = a.0.range().intersect(&b.0.range()).filter(|r| !r.is_empty()).ok_or_else(|| bad(CMD, "the clips don't overlap"))?;
    let (b0, b1) = (beat_at(tempo, sr, overlap.start), beat_at(tempo, sr, overlap.end));
    if b1 - b0 < MIN_BEATS {
        return Err(bad(CMD, format!("the clips overlap for {:.1} beats; at least {MIN_BEATS} are needed", b1 - b0)));
    }
    let from = b0.max(b1 - MAX_BEATS);
    let span = Range { start: sample_at_beat(tempo, sr, from).clamp(overlap.start, overlap.end), end: overlap.end };
    let (pa, pb) = (profiles(s, a, span, from, b1), profiles(s, b, span, from, b1));
    let pairs: Vec<Option<(&[f64], &[f64])>> = pa.iter().zip(&pb).map(|(x, y)| Some((x.as_deref()?, y.as_deref()?))).collect();
    let used = pairs.iter().flatten().count();
    if used == 0 {
        return Err(bad(CMD, "no band has onsets in both clips across the overlap"));
    }
    let beat_ms = 60_000.0 / tempo.tempo_at_tick(tempo.samples_to_ticks(b.0.start, sr));
    let best = peak(&lag_scores(pairs.iter().flatten().copied()));
    let nudge_beats = -best.lag / BINS as f64;
    let nudge_ms = nudge_beats * beat_ms;
    let mut bands = serde_json::Map::new();
    let mut agree = 0;
    for (name, pair) in BANDS.iter().zip(&pairs) {
        let ms = pair.map(|pr| -peak(&lag_scores([pr])).lag / BINS as f64 * beat_ms);
        agree += usize::from(ms.is_some_and(|ms| (ms - nudge_ms).abs() <= AGREE_MS));
        bands.insert((*name).to_string(), ms.map_or(Value::Null, |ms| json!(ms)));
    }
    let mut doubts = Vec::new();
    if best.edge {
        doubts.push("the best lag is half a beat away, the edge of the search".to_string());
    } else if !best.local_max {
        doubts.push("the best lag is not a clear peak".to_string());
    }
    if best.ambiguity > AMBIGUOUS {
        doubts.push(format!("another lag scores {:.0}% of the best", best.ambiguity * 100.0));
    }
    if agree < 2 {
        doubts.push(format!("only {agree} of {used} bands agree within {AGREE_MS} ms"));
    }
    let note = if doubts.is_empty() { Value::Null } else { json!(format!("unreliable: {}; check by ear", doubts.join("; "))) };
    Ok(json!({
        "nudge_ms": nudge_ms,
        "nudge_beats": nudge_beats,
        "score": best.score / used as f64,
        "ambiguity": best.ambiguity,
        "agree": agree,
        "reliable": doubts.is_empty(),
        "note": note,
        "bands": bands,
        "overlap_beats": b1 - b0,
        "analysed_beats": b1 - from,
    }))
}

/// A beat-locked audio clip with its warp map and source channels.
type Side<'a> = (&'a Clip, &'a WarpMap, &'a [Vec<f32>]);

/// Clip `id` of `s` as a [`Side`], or command `cmd`'s error naming what it lacks.
fn side<'a>(s: &'a Session, cmd: &str, id: ClipId) -> Result<Side<'a>> {
    let c = s.find_clip(id).map(|(_, c)| c).ok_or_else(|| bad(cmd, format!("no clip {}", id.0)))?;
    let w = beat_locked(c).ok_or_else(|| not_beat_locked(cmd, id))?;
    let audio = c.source().and_then(|src| s.pool.get(src)).filter(|a| !a.buffer.channels.is_empty());
    let audio = audio.ok_or_else(|| bad(cmd, format!("clip {} has no audio", id.0)))?;
    Ok((c, w, &audio.buffer.channels))
}

/// The warp map of `c` when it is a beat-locked audio clip with a usable map.
pub(super) fn beat_locked(c: &Clip) -> Option<&WarpMap> {
    c.warp.as_ref().filter(|w| w.is_usable() && c.is_audio())
}

/// Command `cmd`'s error for clip `id` not being beat-locked.
pub(super) fn not_beat_locked(cmd: &str, id: ClipId) -> EngineError {
    bad(cmd, format!("clip {} is not beat-locked", id.0))
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

/// Per band, the unit-norm, zero-mean onset profile of side `(c, w, audio)` over timeline span
/// `span` (session beats `b0..b1`) folded onto the session beat; `None` for a band with no onsets.
///
/// The source span played there (plus [`PAD_SECS`] each side) is folded down to mono (the mean of
/// the channels, so sound that cancels between them is not measured), non-finite samples read as
/// silence, and each band's filtered energy is smoothed over [`SMOOTH_SECS`] by two one-pole
/// stages. Every [`HOP`] samples the rise of its log is the onset strength at that frame boundary,
/// whose source position the warp map turns into the session beat it is heard on.
fn profiles(s: &Session, (c, w, audio): Side<'_>, span: Range, b0: f64, b1: f64) -> [Option<Vec<f64>>; 3] {
    let (tempo, sr) = (&s.tempo, s.sample_rate);
    let rate = sr.as_f64();
    let frames = audio.iter().map(Vec::len).max().unwrap_or(0);
    let (p0, p1) = (w.source_at_time(tempo, sr, c.start, span.start), w.source_at_time(tempo, sr, c.start, span.end));
    let clamp = |x: f64| if x.is_finite() { x.clamp(0.0, frames as f64) as usize } else { 0 };
    let (lo, hi) = (clamp(p0.min(p1) - PAD_SECS * rate), clamp(p0.max(p1) + PAD_SECS * rate));
    let shift = beat_at(tempo, sr, c.start) - w.anchor;
    let gain = 1.0 / audio.len().max(1) as f32;
    let mut filters: [Vec<Biquad>; 3] = std::array::from_fn(|band| band_filters(band, rate as f32));
    // Two one-pole stages of half the smoothing time each.
    let coef = SMOOTH_SECS.map(|t| 1.0 - (-2.0 / (t * rate)).exp());
    let mut env = [[0.0f64; 2]; 3];
    let mut prev: [Option<f64>; 3] = [None; 3];
    let mut prof: [Vec<f64>; 3] = std::array::from_fn(|_| vec![0.0; BINS]);
    for k in 0..(hi - lo) / HOP {
        let at = lo + k * HOP;
        // Where in its session beat (in bins) this frame boundary is heard, when inside b0..b1.
        let sb = w.beat_at_source(at as f64) + shift;
        let bin = (b0..b1).contains(&sb).then(|| sb.rem_euclid(1.0) * BINS as f64);
        for ((e, last), pr) in env.iter().zip(&mut prev).zip(&mut prof) {
            let level = (e[1].max(0.0).sqrt() + 1e-6).ln();
            if let (Some(l), Some(b)) = (*last, bin) {
                deposit(pr, b, (level - l).max(0.0));
            }
            *last = Some(level);
        }
        for i in at..at + HOP {
            let x = audio.iter().map(|ch| ch.get(i).copied().filter(|v| v.is_finite()).unwrap_or(0.0)).sum::<f32>() * gain;
            for ((chain, e), a) in filters.iter_mut().zip(&mut env).zip(coef) {
                let y = f64::from(chain.iter_mut().fold(x, |x, f| f.process_sample(x)));
                e[0] += a * (y * y - e[0]);
                e[1] += a * (e[0] - e[1]);
            }
        }
    }
    prof.map(unit)
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

/// The best lag of a [`lag_scores`] result and how far to trust it.
struct Peak {
    /// Bins, refined between bins by a parabola through the peak and its neighbours.
    lag: f64,
    score: f64,
    /// The best score sits on the first or last lag tried.
    edge: bool,
    /// The best score is above both its neighbours.
    local_max: bool,
    /// The best other peak (a local maximum not next to the best) over the best score: 0 when there
    /// is none, 1 when the best score isn't positive.
    ambiguity: f64,
}

/// The [`Peak`] of `scores` (index `lag + MAX_LAG`).
fn peak(scores: &[f64]) -> Peak {
    let at = |j: Option<usize>| j.and_then(|j| scores.get(j)).copied();
    let (i, best) = scores.iter().copied().enumerate().fold((0, f64::NEG_INFINITY), |b, x| if x.1 > b.1 { x } else { b });
    let (left, right) = (at(i.checked_sub(1)), at(Some(i + 1)));
    let local_max = left.is_some_and(|l| l < best) && right.is_some_and(|r| r < best);
    let refine = match (left, right) {
        (Some(l), Some(r)) if l - 2.0 * best + r < 0.0 => (0.5 * (l - r) / (l - 2.0 * best + r)).clamp(-0.5, 0.5),
        _ => 0.0,
    };
    // Past either end counts as lower, so a lag at the edge can be the second peak.
    let is_peak = |j: usize, v: f64| at(j.checked_sub(1)).is_none_or(|l| l <= v) && at(Some(j + 1)).is_none_or(|r| r <= v);
    let second =
        scores.iter().copied().enumerate().filter(|&(j, v)| j.abs_diff(i) > 1 && is_peak(j, v)).map(|(_, v)| v).fold(f64::NEG_INFINITY, f64::max);
    let ambiguity = if best <= 0.0 {
        1.0
    } else if second.is_finite() {
        second.max(0.0) / best
    } else {
        0.0
    };
    Peak { lag: i as f64 - MAX_LAG as f64 + refine, score: best, edge: i == 0 || i + 1 == scores.len(), local_max, ambiguity }
}
