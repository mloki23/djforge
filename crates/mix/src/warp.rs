//! Realtime beat-locked playback of warped clips ([`soundcraft_model::WarpMap`]).
//!
//! Each block, the timeline position is turned into a session beat by the tempo map, the beat
//! into a source position by the clip's warp map, and the source span between the block's start
//! and end is fed through a pitch-preserving [`Stretcher`] that produces exactly the block's
//! frames. Tempo changes and ramps therefore follow the session beat for beat.
//!
//! Latency: the stretcher delays its output by `input_latency` (source frames) plus
//! `output_latency` (output frames). The player reads ahead by both, so the audio heard at
//! timeline sample `t` is the source at [`soundcraft_model::WarpMap::source_at_time`].

use soundcraft_model::{Clip, ClipContent, Session};
use soundcraft_stretch::{Quality, Stretcher};
use soundcraft_time::{Range, Samples};
use std::collections::HashMap;

/// Per-clip playback state, kept across blocks by the strip that plays the clip.
pub(crate) struct WarpPlayer {
    st: Stretcher,
    /// Timeline sample the next block is expected to start at (continuity check).
    next_pos: Samples,
    /// Source position fed next (fractional).
    next_src: f64,
    input: Vec<Vec<f32>>,
    out: Vec<Vec<f32>>,
}

/// Warp players by clip id.
pub(crate) type WarpPlayers = HashMap<u64, WarpPlayer>;

/// Largest block a player handles at once; longer requests are split.
const PLAYER_BLOCK: usize = 4096;

impl WarpPlayer {
    fn new(channels: usize, sample_rate: f32) -> Option<WarpPlayer> {
        let st = Stretcher::new(channels.max(1), sample_rate, PLAYER_BLOCK, Quality::Default).ok()?;
        let max_in = st.max_input();
        Some(WarpPlayer {
            input: vec![vec![0.0; max_in]; channels.max(1)],
            out: vec![vec![0.0; PLAYER_BLOCK]; channels.max(1)],
            st,
            next_pos: Samples::MIN,
            next_src: 0.0,
        })
    }
}

/// Copies source frames `[start, start + n)` of every channel into `dst`, silence outside the file.
fn fill_input(src: &[Vec<f32>], start: i64, n: usize, dst: &mut [Vec<f32>]) {
    let nsrc = src.len();
    for (c, d) in dst.iter_mut().enumerate() {
        let ch = if nsrc == 0 { None } else { src.get(c % nsrc) };
        for (i, x) in d.iter_mut().take(n).enumerate() {
            let idx = start.saturating_add(i as i64);
            *x = if idx < 0 { 0.0 } else { ch.and_then(|v| v.get(idx as usize)).copied().unwrap_or(0.0) };
        }
    }
}

/// Adds the warped clip's audio for `[pos, pos + frames)` into `buf`.
pub(crate) fn render_warped(s: &Session, clip: &Clip, pos: Samples, frames: usize, buf: &mut [Vec<f32>], players: &mut WarpPlayers) {
    let (ClipContent::Audio { source, .. }, Some(warp)) = (&clip.content, &clip.warp) else { return };
    if !warp.is_usable() {
        return;
    }
    let block = Range { start: pos, end: pos.saturating_add(frames as i64) };
    let Some(isect) = clip.range().intersect(&block) else { return };
    let Some(audio) = s.pool.get(*source) else { return };
    let src = &audio.buffer.channels;
    if src.is_empty() {
        return;
    }
    let sr = s.sample_rate.as_f64() as f32;
    let player = match players.entry(clip.id.0) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => match WarpPlayer::new(src.len(), sr) {
            Some(p) => e.insert(p),
            None => return,
        },
    };
    let lat_in = player.st.input_latency() as f64;
    let lat_out = player.st.output_latency() as i64;
    let feed = |t: Samples| warp.source_at_time(&s.tempo, s.sample_rate, clip.start, t.saturating_add(lat_out)) + lat_in;

    let mut t = isect.start;
    while t < isect.end {
        let n = usize::try_from(isect.end - t).unwrap_or(0).min(PLAYER_BLOCK);
        if n == 0 {
            break;
        }
        let t1 = t.saturating_add(n as i64);
        let p0 = feed(t);
        let p1 = feed(t1);
        if player.next_pos != t || (player.next_src - p0).abs() > 1.0 {
            // A jump (seek, first block, edit): restart and pre-roll the source leading up to p0.
            player.st.reset();
            let pre = (player.st.input_latency() * 2 + 1024).min(player.st.max_input());
            let start = p0.floor() as i64 - pre as i64;
            fill_input(src, start, pre, &mut player.input);
            let rate = ((p1 - p0) / n as f64).max(0.01);
            let _ = player.st.seek(&player.input, pre, rate);
            player.next_src = p0;
        }
        let a = player.next_src.floor() as i64;
        let b = p1.floor() as i64;
        let n_in = usize::try_from(b - a).unwrap_or(0).min(player.st.max_input());
        fill_input(src, a, n_in, &mut player.input);
        for c in player.out.iter_mut() {
            c.iter_mut().take(n).for_each(|x| *x = 0.0);
        }
        if player.st.process(&player.input, n_in, &mut player.out, n, 1.0).is_err() {
            return;
        }
        // Clip gain, envelope and fades, evaluated every 32 samples and interpolated.
        let base = usize::try_from(t - pos).unwrap_or(0);
        let nout = player.out.len();
        const STEP: usize = 32;
        let mut i = 0;
        while i < n {
            let seg = (i + STEP).min(n);
            let rel0 = t.saturating_add(i as i64).saturating_sub(clip.start);
            let rel1 = t.saturating_add(seg.saturating_sub(1) as i64).saturating_sub(clip.start).min(clip.length);
            let (g0, g1) = (clip.gain_at(rel0), clip.gain_at(rel1));
            let span = (seg - i).saturating_sub(1).max(1) as f32;
            for (ch, dst) in buf.iter_mut().enumerate() {
                let Some(o) = player.out.get(ch % nout) else { continue };
                for k in i..seg {
                    let g = g0 + (g1 - g0) * (k - i) as f32 / span;
                    if let (Some(d), Some(v)) = (dst.get_mut(base + k), o.get(k)) {
                        *d += v * g;
                    }
                }
            }
            i = seg;
        }
        player.next_pos = t1;
        player.next_src = if n_in == usize::try_from(b - a).unwrap_or(0) { p1 } else { a as f64 + n_in as f64 };
        t = t1;
    }
}
