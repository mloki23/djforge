//! The Decks tab's waveforms: the big scrolling one of each deck, centred on its playhead, and
//! the overview of the whole track.
//!
//! Neither reads the samples per frame. Each column (one physical pixel) sums up a window of
//! source samples anchored to an absolute index, from the audio's min/max mipmap
//! (`SourceAudio::peaks`) and its band mipmap (`SourceAudio::bands`), and the columns are kept in
//! a [`ColumnCache`]: while a deck plays, a frame computes only the few columns that scrolled
//! into view. The cache starts over when the track, the zoom, the tempo (the deck's rate scales
//! the waveform so synced decks' beats line up) or the bands change.

use crate::theme::Tokens;
use crate::waveform::{band_colour, smooth_bands, smoothing_radius};
use egui::{Align2, Color32, Mesh, Painter, Pos2, Rect, Stroke, pos2, vec2};
use soundcraft_mix::{DeckState, DeckTrack, HOT_CUES};
use soundcraft_model::SourceAudio;
use std::collections::VecDeque;

/// The hot cue colours, A to H.
pub const HOT_CUE_COLOURS: [Color32; HOT_CUES] = [
    Color32::from_rgb(0x3c, 0xcf, 0x4e),
    Color32::from_rgb(0xff, 0x4f, 0x5e),
    Color32::from_rgb(0xff, 0x9a, 0x2e),
    Color32::from_rgb(0xff, 0xd5, 0x2e),
    Color32::from_rgb(0x2e, 0xc7, 0xff),
    Color32::from_rgb(0x5a, 0x74, 0xff),
    Color32::from_rgb(0xb0, 0x5c, 0xff),
    Color32::from_rgb(0xff, 0x5c, 0xcf),
];

/// A hot cue's letter (slot 0 is A).
pub fn hot_cue_letter(slot: usize) -> char {
    char::from(b'A'.saturating_add(u8::try_from(slot).unwrap_or(25).min(25)))
}

/// The colour of hot cue `slot`.
pub fn hot_cue_colour(slot: usize) -> Color32 {
    HOT_CUE_COLOURS.get(slot).copied().unwrap_or(Color32::GRAY)
}

/// The most columns a waveform keeps (wider than any screen).
const MAX_COLS: usize = 8192;

/// One column: (low, high) of the samples in −1..1, and its colour. `None` outside the audio.
pub type Column = Option<(f32, f32, Color32)>;

/// Columns of one waveform, kept between frames.
#[derive(Debug, Default, Clone)]
pub struct ColumnCache {
    key: Option<(u64, u64, bool, u32)>,
    /// The absolute index of the first column kept.
    first: i64,
    cols: VecDeque<Column>,
    /// Columns computed since the cache was made (for the timing harness).
    pub computed: usize,
}

impl ColumnCache {
    /// The columns `[first, first + n)` of `audio` at `step` source samples each, computing only
    /// those not kept. `load` names the track (a new load starts over), `wave` colours columns
    /// without bands.
    pub fn columns(&mut self, audio: &SourceAudio, load: u64, step: f64, first: i64, n: usize, wave: Color32, by_freq: bool) -> &VecDeque<Column> {
        let n = n.min(MAX_COLS);
        let bands = by_freq && audio.bands_ready();
        let key = Some((load, step.to_bits(), bands, wave.to_srgba_unmultiplied().iter().fold(0u32, |a, b| a << 8 | u32::from(*b))));
        let end = first.saturating_add(n as i64);
        let kept_end = self.first.saturating_add(self.cols.len() as i64);
        if self.key != key || end <= self.first || first >= kept_end {
            self.key = key;
            self.first = first;
            self.cols = compute(audio, first, n, step, wave, bands).into();
            self.computed = self.computed.saturating_add(n);
            return &self.cols;
        }
        // Drop what scrolled out, then add what scrolled in at either end.
        while self.first < first && self.cols.pop_front().is_some() {
            self.first += 1;
        }
        while self.first.saturating_add(self.cols.len() as i64) > end && self.cols.pop_back().is_some() {}
        if first < self.first {
            let k = usize::try_from(self.first - first).unwrap_or(0);
            for c in compute(audio, first, k, step, wave, bands).into_iter().rev() {
                self.cols.push_front(c);
            }
            self.first = first;
            self.computed = self.computed.saturating_add(k);
        }
        let have = self.first.saturating_add(self.cols.len() as i64);
        if have < end {
            let k = usize::try_from(end - have).unwrap_or(0);
            self.cols.extend(compute(audio, have, k, step, wave, bands));
            self.computed = self.computed.saturating_add(k);
        }
        &self.cols
    }
}

/// Columns `[first, first + n)` at `step` samples each: both channels' (min, max), coloured by
/// the first channel's bands (`bands`) or `wave`.
fn compute(audio: &SourceAudio, first: i64, n: usize, step: f64, wave: Color32, bands: bool) -> Vec<Column> {
    let start = first as f64 * step;
    let mut spans: Vec<Option<(f32, f32)>> = vec![None; n];
    for (ch, peaks) in audio.peaks.iter().enumerate().take(2) {
        let Some(samples) = audio.channel(ch) else { continue };
        for (acc, s) in spans.iter_mut().zip(peaks.spans(samples, start, step, 0.0, n)) {
            *acc = match (*acc, s) {
                (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
                (x, None) => x,
                (None, y) => y,
            };
        }
    }
    let colours: Vec<Option<[f32; 3]>> = match audio.bands(0).filter(|b| bands && !b.is_empty()) {
        Some(b) => smooth_bands(&b.spans(start, step, 0.0, n), smoothing_radius(step, f64::from(audio.buffer.sample_rate))),
        None => Vec::new(),
    };
    spans
        .into_iter()
        .enumerate()
        .map(|(i, s)| s.map(|(lo, hi)| (lo, hi, colours.get(i).copied().flatten().and_then(band_colour).unwrap_or(wave))))
        .collect()
}

/// What a scrolling waveform shows.
pub struct Scrolling<'a> {
    pub track: &'a DeckTrack,
    pub state: &'a DeckState,
    /// The playhead (source samples), smoothed between published states.
    pub position: f64,
    /// Seconds of playback across the width.
    pub seconds: f64,
    /// The session's sample rate (the track's audio is at it).
    pub rate: f64,
    /// The steady tempo factor it is drawn at ([`steady_tempo`]).
    pub tempo: f64,
    pub by_freq: bool,
    pub tint: Color32,
}

/// A deck's free-running tempo as the waveforms take it: its rate, held through a jog nudge's
/// bend (gone within 60 ms of the last event) and followed once a new rate has lasted
/// [`RateHold::SETTLE`] seconds or is the pitch fader's.
#[derive(Debug, Default, Clone, Copy)]
pub struct RateHold {
    held: f64,
    last: f64,
    since: f64,
}

impl RateHold {
    pub const SETTLE: f64 = 0.25;

    /// The held rate for the deck's state `s` at UI time `now`.
    pub fn update(&mut self, s: &DeckState, now: f64) -> f64 {
        let fader = 1.0 + s.pitch;
        let fader = if fader.is_finite() && fader > 0.1 { fader } else { 1.0 };
        let rate = if s.rate.is_finite() && s.rate > 0.1 { s.rate } else { fader };
        if (rate - self.last).abs() > 1e-6 {
            self.last = rate;
            self.since = now;
        }
        if self.held <= 0.0 || self.held.is_nan() || (rate - fader).abs() < 1e-4 || now - self.since >= Self::SETTLE {
            self.held = rate;
        }
        self.held
    }
}

/// The tempo factor a deck's waveform is drawn at: its steady tempo, without a jog nudge's bend
/// or sync's phase correction, so the waveform holds still while they act. Free, that is its
/// held rate (`free`, from [`RateHold`]); synced, its master's over the two grids' tempos, so
/// synced beats are equally spaced on both. `bpm` are the grids' tempos. Rounded so a steady
/// deck keeps its columns.
pub fn steady_tempo(s: &DeckState, free: f64, bpm: Option<f64>, other: &DeckState, other_free: f64, other_bpm: Option<f64>) -> f64 {
    let ok = |b: Option<f64>| b.filter(|b| b.is_finite() && *b > 0.0);
    let r = match (ok(bpm), ok(other_bpm)) {
        // Following the other deck (when both say they follow, each keeps its own).
        (Some(mine), Some(theirs)) if s.synced && !s.master && other.load != 0 && !(other.synced && !other.master) => other_free * theirs / mine,
        _ => free,
    };
    let r = if r.is_finite() && (0.25..=4.0).contains(&r) { r } else { 1.0 };
    (r * 1000.0).round() / 1000.0
}

/// Draws a deck's scrolling waveform in `rect`, its playhead fixed in the centre.
pub fn scrolling(painter: &Painter, rect: Rect, cache: &mut ColumnCache, w: &Scrolling<'_>) {
    let t = Tokens::current();
    painter.rect_filled(rect, 2.0, t.window_bg);
    let ppp = painter.pixels_per_point();
    let px = 1.0 / ppp;
    let width = f64::from(rect.width().max(1.0));
    // Source samples per point.
    let spp = (w.seconds * w.rate * w.tempo / width).max(1e-3);
    let step = spp * f64::from(px);
    let cx = rect.center().x;
    let x_at = |pos: f64| cx + ((pos - w.position) / spp) as f32;
    let left = w.position - spp * f64::from(cx - rect.min.x);
    let first = (left / step).floor() as i64;
    let n = ((rect.width() * ppp).ceil() as usize).saturating_add(2);
    let p = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    // The loop, under the waveform.
    if let (Some(a), Some(b)) = (w.state.loop_in, w.state.loop_out) {
        let r = Rect::from_x_y_ranges(x_at(a).max(rect.min.x)..=x_at(b).min(rect.max.x), rect.y_range());
        if r.width() > 0.0 {
            // A light band with bars along its top and foot, and its ends marked.
            let (fill, edge) = if w.state.loop_on {
                (Color32::from_rgba_unmultiplied(255, 170, 40, 18), Color32::from_rgb(255, 170, 40))
            } else {
                (Color32::from_white_alpha(8), Color32::from_white_alpha(70))
            };
            p.rect_filled(r, 0.0, fill);
            for y in [r.min.y + 2.0, r.max.y - 2.0] {
                p.hline(r.x_range(), y, Stroke::new(3.0, edge.gamma_multiply(0.8)));
            }
            for x in [x_at(a), x_at(b)] {
                if r.x_range().contains(x) {
                    p.vline(x, rect.y_range(), Stroke::new(1.5, edge));
                }
            }
        }
    }
    let wave = crate::widgets::shade(w.tint, -0.15);
    let cols = cache.columns(&w.track.source, w.state.load, step, first, n, wave, w.by_freq);
    let cy = rect.center().y;
    let half = rect.height() * 0.46;
    let mut mesh = Mesh::default();
    for (i, c) in cols.iter().enumerate() {
        let Some((lo, hi, colour)) = *c else { continue };
        let pos = (first + i as i64) as f64 * step;
        let x0 = x_at(pos);
        let x1 = x0 + px;
        let top = cy - hi.clamp(-1.0, 1.0) * half;
        let bot = (cy - lo.clamp(-1.0, 1.0) * half).max(top + px);
        mesh.add_colored_rect(Rect::from_min_max(pos2(x0, top), pos2(x1, bot)), colour);
    }
    p.add(egui::Shape::mesh(mesh));
    // Beats and bars.
    if let Some(g) = &w.track.grid {
        let right = w.position + spp * f64::from(rect.max.x - cx);
        let (b0, b1) = (g.beat_at(left).ceil(), g.beat_at(right).floor());
        let mut b = b0;
        let mut drawn = 0;
        while b <= b1 && drawn < 1024 {
            let x = x_at(g.pos_at(b));
            let bar = (b.rem_euclid(4.0)).abs() < 1e-6;
            let (stroke, reach) =
                if bar { (Stroke::new(1.5, Color32::from_white_alpha(160)), 1.0) } else { (Stroke::new(1.0, Color32::from_white_alpha(115)), 0.3) };
            p.vline(x, rect.min.y..=rect.min.y + rect.height() * reach, stroke);
            if !bar {
                p.vline(x, rect.max.y - rect.height() * reach..=rect.max.y, stroke);
            }
            b += 1.0;
            drawn += 1;
        }
    }
    // The cue point and the hot cues.
    cue_flag(&p, x_at(w.state.cue), rect, t.accent, "CUE");
    for (slot, h) in w.state.hot.iter().enumerate() {
        if let Some(h) = h {
            let x = x_at(*h);
            if x >= rect.min.x - 20.0 && x <= rect.max.x + 20.0 {
                let c = hot_cue_colour(slot);
                p.vline(x, rect.y_range(), Stroke::new(1.0, c.gamma_multiply(0.7)));
                let tag = Rect::from_min_size(pos2(x, rect.min.y), vec2(14.0, 14.0));
                p.rect_filled(tag, 2.0, c);
                p.text(tag.center(), Align2::CENTER_CENTER, hot_cue_letter(slot), crate::theme::bold(10.0), Color32::BLACK);
            }
        }
    }
    // The playhead, fixed.
    p.vline(cx, rect.y_range(), Stroke::new(2.0, Color32::WHITE));
    p.add(egui::Shape::convex_polygon(
        vec![pos2(cx - 5.0, rect.min.y), pos2(cx + 5.0, rect.min.y), pos2(cx, rect.min.y + 6.0)],
        Color32::WHITE,
        Stroke::NONE,
    ));
}

/// A cue mark: a line and a small triangle at the bottom.
fn cue_flag(p: &Painter, x: f32, rect: Rect, c: Color32, _label: &str) {
    if x < rect.min.x - 8.0 || x > rect.max.x + 8.0 {
        return;
    }
    p.vline(x, rect.y_range(), Stroke::new(1.0, c.gamma_multiply(0.8)));
    p.add(egui::Shape::convex_polygon(vec![pos2(x - 6.0, rect.max.y), pos2(x + 6.0, rect.max.y), pos2(x, rect.max.y - 8.0)], c, Stroke::NONE));
}

/// The overview's columns, kept until the track, the width or the bands change.
#[derive(Debug, Default, Clone)]
pub struct OverviewCache {
    key: Option<(u64, usize, bool)>,
    cols: Vec<Column>,
}

/// Draws the whole track across `rect`: the played part dimmed, the loop, the cue, the hot cues
/// and the playhead.
pub fn overview(
    painter: &Painter,
    rect: Rect,
    cache: &mut OverviewCache,
    track: &DeckTrack,
    s: &DeckState,
    position: f64,
    by_freq: bool,
    tint: Color32,
) {
    let t = Tokens::current();
    painter.rect_filled(rect, 2.0, t.window_bg);
    let frames = track.frames().max(1) as f64;
    let ppp = painter.pixels_per_point();
    let n = ((rect.width() * ppp).ceil() as usize).clamp(1, MAX_COLS);
    let bands = by_freq && track.source.bands_ready();
    let key = Some((s.load, n, bands));
    if cache.key != key {
        cache.key = key;
        cache.cols = compute(&track.source, 0, n, frames / n as f64, crate::widgets::shade(tint, -0.15), bands);
    }
    let x_at = |pos: f64| rect.min.x + (pos / frames) as f32 * rect.width();
    let played = x_at(position);
    let (cy, half) = (rect.center().y, rect.height() * 0.46);
    let px = 1.0 / ppp;
    let mut mesh = Mesh::default();
    for (i, c) in cache.cols.iter().enumerate() {
        let Some((lo, hi, colour)) = *c else { continue };
        let x0 = rect.min.x + i as f32 * px;
        let top = cy - hi.clamp(-1.0, 1.0) * half;
        let bot = (cy - lo.clamp(-1.0, 1.0) * half).max(top + px);
        let colour = if x0 < played { colour.gamma_multiply(0.4) } else { colour };
        mesh.add_colored_rect(Rect::from_min_max(pos2(x0, top), pos2(x0 + px, bot)), colour);
    }
    let p = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    p.add(egui::Shape::mesh(mesh));
    if let (Some(a), Some(b)) = (s.loop_in, s.loop_out) {
        let r = Rect::from_x_y_ranges(x_at(a)..=x_at(b).max(x_at(a) + 2.0), rect.y_range());
        p.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(255, 170, 40, if s.loop_on { 80 } else { 30 }));
    }
    let cue = x_at(s.cue);
    p.add(egui::Shape::convex_polygon(
        vec![pos2(cue - 4.0, rect.max.y), pos2(cue + 4.0, rect.max.y), pos2(cue, rect.max.y - 6.0)],
        t.accent,
        Stroke::NONE,
    ));
    for (slot, h) in s.hot.iter().enumerate() {
        if let Some(h) = h {
            let x = x_at(*h);
            let c = hot_cue_colour(slot);
            p.vline(x, rect.y_range(), Stroke::new(1.0, c));
            let tag = Rect::from_min_size(Pos2::new(x, rect.min.y), vec2(10.0, 10.0));
            p.rect_filled(tag, 1.0, c);
            p.text(tag.center(), Align2::CENTER_CENTER, hot_cue_letter(slot), crate::theme::bold(8.0), Color32::BLACK);
        }
    }
    p.vline(played, rect.y_range(), Stroke::new(1.5, Color32::WHITE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;

    fn audio(n: usize) -> SourceAudio {
        let c: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![c.clone(), c] })
    }

    #[test]
    fn scrolling_computes_only_the_columns_that_come_into_view() {
        let a = audio(48_000 * 20);
        let mut cache = ColumnCache::default();
        let fresh = compute(&a, 1000, 1500, 160.0, Color32::RED, false);
        assert_eq!(cache.columns(&a, 1, 160.0, 1000, 1500, Color32::RED, false).iter().copied().collect::<Vec<_>>(), fresh);
        assert_eq!(cache.computed, 1500);
        // Four columns later: four computed, the rest kept, the same as computed afresh.
        let moved: Vec<Column> = cache.columns(&a, 1, 160.0, 1004, 1500, Color32::RED, false).iter().copied().collect();
        assert_eq!(cache.computed, 1504);
        assert_eq!(moved, compute(&a, 1004, 1500, 160.0, Color32::RED, false));
        // Back again, and a new zoom starts over.
        let back: Vec<Column> = cache.columns(&a, 1, 160.0, 990, 1500, Color32::RED, false).iter().copied().collect();
        assert_eq!(back, compute(&a, 990, 1500, 160.0, Color32::RED, false));
        cache.columns(&a, 1, 320.0, 990, 1500, Color32::RED, false);
        assert_eq!(cache.computed, 1504 + 14 + 1500);
    }

    #[test]
    fn the_waveform_scale_holds_through_a_nudge_and_follows_a_lasting_rate() {
        let mut h = RateHold::default();
        let mut s = DeckState { load: 1, playing: true, rate: 1.02, pitch: 0.02, ..Default::default() };
        assert_eq!(h.update(&s, 0.0), 1.02);
        // A jog nudge bends the rate for a moment: held.
        s.rate = 1.12;
        assert_eq!(h.update(&s, 0.05), 1.02);
        s.rate = 1.02;
        assert_eq!(h.update(&s, 0.1), 1.02);
        // Sync off leaves a rate the fader is not at: followed once it lasts.
        s.rate = 1.035;
        assert_eq!(h.update(&s, 1.0), 1.02);
        assert_eq!(h.update(&s, 1.0 + RateHold::SETTLE), 1.035);
        // Synced, the scale is the master's over the grids' tempos, whatever the correction does.
        let master = DeckState { load: 1, playing: true, master: true, ..Default::default() };
        let follower = DeckState { load: 2, playing: true, synced: true, rate: 1.0461, ..Default::default() };
        let r = steady_tempo(&follower, 1.0461, Some(132.0), &master, 1.01, Some(138.0));
        assert!((r - 1.01 * 138.0 / 132.0).abs() < 1e-3, "{r}");
        assert_eq!(steady_tempo(&master, 1.01, Some(138.0), &follower, 1.0461, Some(132.0)), 1.01);
    }

    #[test]
    fn columns_outside_the_audio_are_empty() {
        let a = audio(4800);
        let c = compute(&a, -10, 20, 100.0, Color32::RED, false);
        assert!(c.first().is_some_and(Option::is_none));
        assert!(c.get(15).is_some_and(Option::is_some));
    }
}
