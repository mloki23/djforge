//! The DJ mixer's channel strip for a live deck ([`crate::decks`]): trim, a 3-band isolator EQ,
//! a bipolar filter and the channel fader; plus the crossfader's curves.
//!
//! - **Isolator EQ.** The signal is split into three bands by Linkwitz-Riley 24 dB/oct crossovers
//!   at [`LOW_MID_HZ`] and [`MID_HIGH_HZ`] (two Butterworth sections each), the low band passed
//!   through the 2.5 kHz pair's all-pass so the bands sum flat in level. Each band's gain runs
//!   from a kill (silent) to +6 dB.
//! - **Filter.** One knob, centre off: left a low-pass sweeping down from 20 kHz to 80 Hz, right a
//!   high-pass sweeping up from 20 Hz to 10 kHz, 24 dB/oct with a resonant second section, built
//!   from state-variable sections that stay clean while the knob moves, and faded in and out over
//!   [`WET_FRAMES`] at the dead zone.
//! - Gains ramp across each block and the filter is re-designed every [`FILTER_STEP`] frames, so
//!   moving a control never clicks or zippers. Nothing here allocates once built.

use soundcraft_dsp::biquad::{Biquad, FilterType};
use soundcraft_dsp::db_to_gain;

/// The low/mid crossover (Hz).
pub const LOW_MID_HZ: f32 = 200.0;
/// The mid/high crossover (Hz).
pub const MID_HIGH_HZ: f32 = 2_500.0;
/// Most an EQ band boosts (dB).
pub const EQ_MAX_DB: f32 = 6.0;
/// An EQ band at or below this (dB) is killed: silent.
pub const EQ_KILL_DB: f32 = -80.0;
/// Trim range (dB).
pub const TRIM_DB: (f32, f32) = (-24.0, 12.0);
/// The filter knob's centre dead zone: within it the filter is off.
pub const FILTER_DEAD: f32 = 0.02;
/// Frames between filter re-designs while its knob moves.
const FILTER_STEP: usize = 16;
/// Frames the filter takes to fade in or out.
const WET_FRAMES: usize = 64;
const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// The resonant section's Q.
const RESONANCE_Q: f32 = 1.4;

/// An EQ band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqBand {
    Low,
    Mid,
    High,
}

impl EqBand {
    pub fn index(self) -> usize {
        match self {
            EqBand::Low => 0,
            EqBand::Mid => 1,
            EqBand::High => 2,
        }
    }

    pub fn parse(s: &str) -> Option<EqBand> {
        match s {
            "low" => Some(EqBand::Low),
            "mid" => Some(EqBand::Mid),
            "high" => Some(EqBand::High),
            _ => None,
        }
    }
}

/// The channel fader's curve: gain for fader position `v` (0..=1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FaderCurve {
    /// Gain = position.
    #[default]
    Linear,
    /// Slow start: gain = position² (most of the throw in the upper half).
    Smooth,
    /// Fast start: gain = 1 − (1 − position)³ (near full early in the throw).
    Sharp,
}

impl FaderCurve {
    pub fn gain(self, v: f32) -> f32 {
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        match self {
            FaderCurve::Linear => v,
            FaderCurve::Smooth => v * v,
            FaderCurve::Sharp => 1.0 - (1.0 - v).powi(3),
        }
    }

    pub fn parse(s: &str) -> Option<FaderCurve> {
        match s {
            "linear" => Some(FaderCurve::Linear),
            "smooth" => Some(FaderCurve::Smooth),
            "sharp" => Some(FaderCurve::Sharp),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            FaderCurve::Linear => "linear",
            FaderCurve::Smooth => "smooth",
            FaderCurve::Sharp => "sharp",
        }
    }
}

/// The crossfader's curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum XfadeCurve {
    /// Constant power: each side at −3 dB in the centre.
    #[default]
    Smooth,
    /// Cut: both sides full over most of the throw, each cut in the last [`CUT_ZONE`] of it.
    Sharp,
}

/// The part of the crossfader's throw at each end where a sharp curve cuts.
pub const CUT_ZONE: f32 = 0.08;

impl XfadeCurve {
    /// Gains of sides A and B at crossfader position `x` (−1 = all A, +1 = all B).
    pub fn gains(self, x: f32) -> (f32, f32) {
        let x = if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 };
        match self {
            XfadeCurve::Smooth => {
                let th = (x + 1.0) * 0.25 * std::f32::consts::PI;
                (th.cos().max(0.0), th.sin().max(0.0))
            }
            XfadeCurve::Sharp => (((1.0 - x) / CUT_ZONE).clamp(0.0, 1.0), ((1.0 + x) / CUT_ZONE).clamp(0.0, 1.0)),
        }
    }

    pub fn parse(s: &str) -> Option<XfadeCurve> {
        match s {
            "smooth" => Some(XfadeCurve::Smooth),
            "sharp" => Some(XfadeCurve::Sharp),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            XfadeCurve::Smooth => "smooth",
            XfadeCurve::Sharp => "sharp",
        }
    }
}

/// Which side of the crossfader a deck plays through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum XfadeSide {
    #[default]
    A,
    B,
    /// Not through the crossfader: always heard.
    Thru,
}

impl XfadeSide {
    pub fn parse(s: &str) -> Option<XfadeSide> {
        match s {
            "a" | "A" => Some(XfadeSide::A),
            "b" | "B" => Some(XfadeSide::B),
            "thru" => Some(XfadeSide::Thru),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            XfadeSide::A => "a",
            XfadeSide::B => "b",
            XfadeSide::Thru => "thru",
        }
    }

    /// The gain of this side given the crossfader's (A, B) gains.
    pub fn pick(self, (a, b): (f32, f32)) -> f32 {
        match self {
            XfadeSide::A => a,
            XfadeSide::B => b,
            XfadeSide::Thru => 1.0,
        }
    }

    pub(crate) fn code(self) -> u64 {
        match self {
            XfadeSide::A => 0,
            XfadeSide::B => 1,
            XfadeSide::Thru => 2,
        }
    }

    pub(crate) fn from_code(c: u64) -> XfadeSide {
        match c {
            0 => XfadeSide::A,
            1 => XfadeSide::B,
            _ => XfadeSide::Thru,
        }
    }
}

/// The linear gain of an EQ band set to `db`: 0 at or below [`EQ_KILL_DB`].
pub fn eq_gain(db: f32) -> f32 {
    if !db.is_finite() || db <= EQ_KILL_DB { 0.0 } else { db_to_gain(db.min(EQ_MAX_DB)) }
}

/// One channel's crossover: Linkwitz-Riley 4th order at both splits.
#[derive(Debug, Clone, Copy)]
struct Crossover {
    lp1: [Biquad; 2],
    hp1: [Biquad; 2],
    lp2: [Biquad; 2],
    hp2: [Biquad; 2],
    /// The low band's all-pass, matching the phase of the mid/high split.
    ap: Biquad,
}

impl Crossover {
    fn new(sr: f32) -> Crossover {
        let b = |k, f| Biquad::design(k, f, BUTTERWORTH_Q, 0.0, sr);
        Crossover {
            lp1: [b(FilterType::LowPass, LOW_MID_HZ); 2],
            hp1: [b(FilterType::HighPass, LOW_MID_HZ); 2],
            lp2: [b(FilterType::LowPass, MID_HIGH_HZ); 2],
            hp2: [b(FilterType::HighPass, MID_HIGH_HZ); 2],
            ap: b(FilterType::AllPass, MID_HIGH_HZ),
        }
    }

    /// The three bands of `x`.
    #[inline]
    fn split(&mut self, x: f32) -> [f32; 3] {
        let [a, b] = &mut self.lp1;
        let low = self.ap.process_sample(b.process_sample(a.process_sample(x)));
        let [a, b] = &mut self.hp1;
        let rest = b.process_sample(a.process_sample(x));
        let [a, b] = &mut self.lp2;
        let mid = b.process_sample(a.process_sample(rest));
        let [a, b] = &mut self.hp2;
        let high = b.process_sample(a.process_sample(rest));
        [low, mid, high]
    }

    fn reset(&mut self) {
        for f in self.lp1.iter_mut().chain(self.hp1.iter_mut()).chain(self.lp2.iter_mut()).chain(self.hp2.iter_mut()) {
            f.reset();
        }
        self.ap.reset();
    }
}

/// A topology-preserving state-variable filter section (Zavalishin, "The Art of VA Filter
/// Design"): the same bilinear low- or high-pass as a biquad, but its state stays meaningful
/// when its cutoff moves every few samples, so a filter sweep never rings or clicks.
#[derive(Debug, Clone, Copy, Default)]
struct Svf {
    ic1: f32,
    ic2: f32,
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    high: bool,
}

impl Svf {
    fn set(&mut self, kind: FilterType, freq: f32, q: f32, _gain: f32, sr: f32) {
        let f = if freq.is_finite() { freq.clamp(5.0, sr * 0.49) } else { 1000.0 };
        let g = (std::f32::consts::PI * f / sr).tan();
        self.k = 1.0 / q.max(0.05);
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
        self.high = matches!(kind, FilterType::HighPass | FilterType::HighPass1);
    }

    fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    #[inline]
    fn process_sample(&mut self, x: f32) -> f32 {
        let v3 = x - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        if !(self.ic1.is_finite() && self.ic2.is_finite()) {
            self.reset();
            return 0.0;
        }
        if self.high { x - self.k * v1 - v2 } else { v2 }
    }
}

/// The filter's response for knob `v`: none in the dead zone, else (kind, cutoff Hz).
pub fn filter_response(v: f32) -> Option<(FilterType, f32)> {
    let v = if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
    if v.abs() < FILTER_DEAD {
        return None;
    }
    let t = (v.abs() - FILTER_DEAD) / (1.0 - FILTER_DEAD);
    Some(if v < 0.0 {
        (FilterType::LowPass, 20_000.0 * (80.0f32 / 20_000.0).powf(t))
    } else {
        (FilterType::HighPass, 20.0 * (10_000.0f32 / 20.0).powf(t))
    })
}

/// A deck's channel strip.
#[derive(Debug, Clone)]
pub struct DjStrip {
    sr: f32,
    pub trim_db: f32,
    pub eq_db: [f32; 3],
    pub filter: f32,
    pub fader: f32,
    pub curve: FaderCurve,
    pub side: XfadeSide,
    xo: [Crossover; 2],
    filt: [[Svf; 2]; 2],
    /// The filter's kind last designed (a change of kind resets its state).
    filt_kind: Option<FilterType>,
    /// The knob value the filter was last designed for.
    filt_at: f32,
    /// How much of the filtered signal is heard (fades in and out with the dead zone).
    wet: f32,
    /// Gains as of the end of the last block (ramped from towards the targets).
    g_trim: f32,
    g_eq: [f32; 3],
    g_fader: f32,
}

impl DjStrip {
    /// A strip at unity: trim 0 dB, EQ flat, filter off, fader up.
    pub fn new(sample_rate: f32, side: XfadeSide) -> DjStrip {
        let sr = if sample_rate.is_finite() && sample_rate > 0.0 { sample_rate } else { 48_000.0 };
        DjStrip {
            sr,
            trim_db: 0.0,
            eq_db: [0.0; 3],
            filter: 0.0,
            fader: 1.0,
            curve: FaderCurve::Linear,
            side,
            xo: [Crossover::new(sr); 2],
            filt: [[Svf::default(); 2]; 2],
            filt_kind: None,
            filt_at: 0.0,
            wet: 0.0,
            g_trim: 1.0,
            g_eq: [1.0; 3],
            g_fader: 1.0,
        }
    }

    /// Clears the filters' state (a deck stopped: its tail must not ring into the next start).
    pub fn reset(&mut self) {
        self.xo.iter_mut().for_each(Crossover::reset);
        self.filt.iter_mut().flatten().for_each(Svf::reset);
    }

    /// The fader's gain now (its curve applied).
    pub fn fader_gain(&self) -> f32 {
        self.curve.gain(self.fader)
    }

    /// Designs the filter for knob `v` (a response: outside the dead zone). Its state is cleared
    /// only while it is not heard (`wet` 0), so nothing clicks.
    fn design_filter(&mut self, v: f32) {
        match filter_response(v) {
            None => {}
            Some((kind, f)) => {
                if self.wet <= 0.0 {
                    self.filt.iter_mut().flatten().for_each(Svf::reset);
                }
                for ch in self.filt.iter_mut() {
                    let [a, b] = ch;
                    a.set(kind, f, BUTTERWORTH_Q, 0.0, self.sr);
                    b.set(kind, f, RESONANCE_Q, 0.0, self.sr);
                }
                self.filt_kind = Some(kind);
            }
        }
        self.filt_at = v;
    }

    /// Runs `frames` of the deck's stereo audio in `buf` through the strip, in place.
    pub fn process(&mut self, buf: &mut [Vec<f32>], frames: usize) {
        let n = buf.iter().map(Vec::len).min().unwrap_or(0).min(frames);
        if n == 0 {
            return;
        }
        let t_trim = db_to_gain(self.trim_db.clamp(TRIM_DB.0, TRIM_DB.1));
        let t_eq = self.eq_db.map(eq_gain);
        let t_fader = self.fader_gain();
        let ramp = |a: f32, b: f32, i: usize| a + (b - a) * (i + 1) as f32 / n as f32;
        let target_filter = if self.filter.is_finite() { self.filter.clamp(-1.0, 1.0) } else { 0.0 };
        let start_filter = self.filt_at;
        let mut i = 0;
        while i < n {
            let len = FILTER_STEP.min(n - i);
            // The knob's value at the end of this step, so a sweep moves smoothly over the block.
            let v = start_filter + (target_filter - start_filter) * (i + len) as f32 / n as f32;
            // The filter fades in and out over a step (and out before it changes kind), so
            // the knob crossing the dead zone never clicks.
            let target_wet = match (filter_response(v), self.filt_kind) {
                (None, _) => {
                    self.filt_at = v;
                    0.0
                }
                (Some((k, _)), Some(cur)) if k != cur && self.wet > 0.0 => 0.0,
                (Some(_), _) => {
                    if v != self.filt_at || self.wet <= 0.0 {
                        self.design_filter(v);
                    }
                    1.0
                }
            };
            let step = 1.0 / WET_FRAMES as f32;
            let wet0 = self.wet;
            for (ch, b) in buf.iter_mut().enumerate().take(2) {
                let (Some(xo), Some(fl)) = (self.xo.get_mut(ch), self.filt.get_mut(ch)) else { continue };
                for k in i..i + len {
                    let Some(x) = b.get_mut(k) else { continue };
                    let gt = ramp(self.g_trim, t_trim, k);
                    let bands = xo.split(*x * gt);
                    let mut y = 0.0;
                    for (j, band) in bands.iter().enumerate() {
                        let g0 = self.g_eq.get(j).copied().unwrap_or(1.0);
                        let g1 = t_eq.get(j).copied().unwrap_or(1.0);
                        y += band * ramp(g0, g1, k);
                    }
                    let wet = (wet0 + (target_wet - wet0).clamp(-step, step) * (k - i + 1) as f32).clamp(0.0, 1.0);
                    let wet = if target_wet > wet0 { wet.min(target_wet) } else { wet.max(target_wet) };
                    if self.filt_kind.is_some() && wet > 0.0 {
                        let [fa, fb] = &mut *fl;
                        let yf = fb.process_sample(fa.process_sample(y));
                        y += (yf - y) * wet;
                    }
                    *x = y * ramp(self.g_fader, t_fader, k);
                }
            }
            let moved = (target_wet - wet0).clamp(-step * len as f32, step * len as f32);
            self.wet = (wet0 + moved).clamp(0.0, 1.0);
            i += len;
        }
        self.g_trim = t_trim;
        self.g_eq = t_eq;
        self.g_fader = t_fader;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (std::f32::consts::TAU * hz * i as f32 / SR).sin() * 0.5).collect()
    }

    /// RMS (dB) of `hz` through `strip`, after the filters settle.
    fn level(strip: &mut DjStrip, hz: f32) -> f32 {
        let n = 24_000;
        let x = sine(hz, n);
        let mut buf = [x.clone(), x];
        // Blocks of 512, as the mixer runs it.
        for start in (0..n).step_by(512) {
            let mut b: Vec<Vec<f32>> = buf.iter().map(|c| c[start..(start + 512).min(n)].to_vec()).collect();
            let len = b[0].len();
            strip.process(&mut b, len);
            for (dst, src) in buf.iter_mut().zip(&b) {
                dst[start..start + len].copy_from_slice(src);
            }
        }
        let tail = &buf[0][n / 2..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        20.0 * (rms / (0.5 / 2f32.sqrt())).max(1e-9).log10()
    }

    #[test]
    fn a_flat_eq_passes_every_band_at_unity() {
        for hz in [50.0, 200.0, 1_000.0, 2_500.0, 10_000.0] {
            let db = level(&mut DjStrip::new(SR, XfadeSide::A), hz);
            assert!(db.abs() < 0.3, "{hz} Hz: {db} dB");
        }
    }

    #[test]
    fn eq_kill_takes_its_band_down_by_more_than_30_db() {
        for (band, hz) in [(0, 50.0), (1, 800.0), (2, 10_000.0)] {
            let mut s = DjStrip::new(SR, XfadeSide::A);
            s.eq_db[band] = EQ_KILL_DB;
            let db = level(&mut s, hz);
            assert!(db < -30.0, "band {band} at {hz} Hz: {db} dB");
            // The other bands stay.
            let other = if band == 2 { 50.0 } else { 10_000.0 };
            let mut s = DjStrip::new(SR, XfadeSide::A);
            s.eq_db[band] = EQ_KILL_DB;
            assert!(level(&mut s, other) > -1.0, "band {band} killed takes {other} Hz with it");
        }
        // +6 dB boosts.
        let mut s = DjStrip::new(SR, XfadeSide::A);
        s.eq_db[1] = 6.0;
        assert!((level(&mut s, 800.0) - 6.0).abs() < 0.5);
    }

    #[test]
    fn the_filter_is_a_low_pass_left_and_a_high_pass_right() {
        let mut lp = DjStrip::new(SR, XfadeSide::A);
        lp.filter = -0.5;
        let (_, f) = filter_response(-0.5).unwrap();
        assert!((900.0..2_000.0).contains(&f), "{f}");
        assert!(level(&mut lp.clone(), 100.0) > -1.0);
        assert!(level(&mut lp, 10_000.0) < -40.0);
        let mut hp = DjStrip::new(SR, XfadeSide::A);
        hp.filter = 0.5;
        assert!(level(&mut hp.clone(), 50.0) < -30.0);
        assert!(level(&mut hp, 8_000.0) > -1.0);
        // Centre: off.
        let mut off = DjStrip::new(SR, XfadeSide::A);
        off.filter = 0.01;
        assert!(level(&mut off, 15_000.0).abs() < 0.3);
        // Resonance: a small peak near the cutoff.
        let mut res = DjStrip::new(SR, XfadeSide::A);
        res.filter = -0.5;
        assert!(level(&mut res, f) > -6.0);
    }

    #[test]
    fn the_filter_fades_in_and_out_without_a_click() {
        let mut s = DjStrip::new(SR, XfadeSide::A);
        let x = sine(300.0, 512 * 40);
        let mut out = Vec::new();
        for (k, block) in x.chunks(512).enumerate() {
            // In, across to the other side, out again: a knob jumping between blocks.
            s.filter = [0.0, -0.6, 0.6, 0.0, 0.9, -0.9, 0.0][k % 7];
            let mut b = vec![block.to_vec(), block.to_vec()];
            s.process(&mut b, 512);
            out.extend_from_slice(&b[0]);
        }
        let slope = std::f32::consts::TAU * 300.0 / SR * 0.5;
        let worst = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(worst < 3.0 * slope, "step {worst} vs slope {slope}");
    }

    #[test]
    fn crossfader_curves() {
        let s = XfadeCurve::Smooth;
        let (a, b) = s.gains(0.0);
        assert!((a - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6 && (b - a).abs() < 1e-6, "constant power: -3 dB each in the centre");
        assert_eq!(s.gains(-1.0).0, 1.0);
        assert!(s.gains(-1.0).1 < 1e-6 && s.gains(1.0).0 < 1e-6);
        for x in [-0.7f32, -0.2, 0.3, 0.9] {
            let (a, b) = s.gains(x);
            assert!((a * a + b * b - 1.0).abs() < 1e-5, "power stays constant at {x}");
        }
        let c = XfadeCurve::Sharp;
        assert_eq!(c.gains(0.0), (1.0, 1.0));
        assert_eq!(c.gains(0.9), (1.0, 1.0));
        assert_eq!(c.gains(1.0), (0.0, 1.0));
        assert_eq!(c.gains(-1.0), (1.0, 0.0));
        assert!((c.gains(0.96).0 - 0.5).abs() < 1e-5);
        assert_eq!(XfadeSide::Thru.pick(c.gains(1.0)), 1.0);
        assert_eq!(FaderCurve::Smooth.gain(0.5), 0.25);
        assert_eq!(FaderCurve::Linear.gain(0.5), 0.5);
        assert_eq!(FaderCurve::Sharp.gain(0.5), 0.875);
        assert_eq!(FaderCurve::Linear.gain(f32::NAN), 0.0);
    }
}
