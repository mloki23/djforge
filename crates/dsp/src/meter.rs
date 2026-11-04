//! Level meters: DAW-style peak meter, RMS meter, and an ITU-R BS.1770 loudness meter
//! (momentary, short-term and gated integrated LUFS plus 4× oversampled true peak).
//!
//! None of the `process` methods allocate.

use crate::biquad::{Biquad, Coeffs};
use crate::util::sane_sr;
use crate::{MIN_DB, gain_to_db};

/// Peak meter with instant attack, linear-in-dB release, peak hold and a clip latch.
#[derive(Debug, Clone)]
pub struct PeakMeter {
    sr: f32,
    level_db: f32,
    hold_db: f32,
    hold_left: f32,
    hold_secs: f32,
    release_db_per_sec: f32,
    clipped: bool,
}

impl PeakMeter {
    /// A meter with 20 dB/s release and a 2 s peak hold.
    pub fn new(sample_rate: f32) -> Self {
        Self { sr: sane_sr(sample_rate), level_db: MIN_DB, hold_db: MIN_DB, hold_left: 0.0, hold_secs: 2.0, release_db_per_sec: 20.0, clipped: false }
    }

    pub fn set_release_db_per_sec(&mut self, rate: f32) {
        if rate.is_finite() {
            self.release_db_per_sec = rate.clamp(0.1, 1000.0);
        }
    }

    pub fn set_hold_secs(&mut self, secs: f32) {
        if secs.is_finite() {
            self.hold_secs = secs.clamp(0.0, 60.0);
        }
    }

    /// Feeds one block of samples.
    pub fn process(&mut self, samples: &[f32]) {
        let peak = samples.iter().filter(|v| v.is_finite()).fold(0.0f32, |m, v| m.max(v.abs()));
        self.feed(peak, samples.len());
    }

    /// Feeds an already-computed block peak (linear) covering `frames` samples.
    pub fn feed(&mut self, peak: f32, frames: usize) {
        let dt = frames as f32 / self.sr;
        if peak >= 1.0 {
            self.clipped = true;
        }
        let peak_db = gain_to_db(peak);
        let decayed = (self.level_db - self.release_db_per_sec * dt).max(MIN_DB);
        self.level_db = decayed.max(peak_db);
        if peak_db >= self.hold_db {
            self.hold_db = peak_db;
            self.hold_left = self.hold_secs;
        } else if self.hold_left > 0.0 {
            self.hold_left -= dt;
        } else {
            self.hold_db = (self.hold_db - self.release_db_per_sec * dt).max(self.level_db);
        }
    }

    /// Displayed level in dBFS.
    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    /// Held peak in dBFS.
    pub fn peak_hold_db(&self) -> f32 {
        self.hold_db
    }

    /// True once any sample reached full scale, until [`PeakMeter::reset_clip`].
    pub fn clipped(&self) -> bool {
        self.clipped
    }

    pub fn reset_clip(&mut self) {
        self.clipped = false;
    }

    pub fn reset(&mut self) {
        self.level_db = MIN_DB;
        self.hold_db = MIN_DB;
        self.hold_left = 0.0;
        self.clipped = false;
    }
}

/// Exponentially averaged RMS meter (300 ms time constant by default).
#[derive(Debug, Clone)]
pub struct RmsMeter {
    sr: f32,
    coef: f32,
    ms: f64,
}

impl RmsMeter {
    pub fn new(sample_rate: f32) -> Self {
        let mut m = Self { sr: sane_sr(sample_rate), coef: 0.0, ms: 0.0 };
        m.set_window_ms(300.0);
        m
    }

    pub fn set_window_ms(&mut self, ms: f32) {
        self.coef = crate::util::time_coef(if ms.is_finite() { ms.clamp(1.0, 10_000.0) } else { 300.0 }, self.sr);
    }

    pub fn process(&mut self, samples: &[f32]) {
        let k = f64::from(1.0 - self.coef);
        for &x in samples {
            let x = if x.is_finite() { f64::from(x) } else { 0.0 };
            self.ms += (x * x - self.ms) * k;
        }
        if self.ms < 1e-30 {
            self.ms = 0.0;
        }
    }

    /// Current RMS level in dBFS (sine-referenced: an RMS of 0.707 reads -3 dB).
    pub fn rms_db(&self) -> f32 {
        gain_to_db(self.ms.sqrt() as f32)
    }

    pub fn reset(&mut self) {
        self.ms = 0.0;
    }
}

const SUB_BLOCKS: usize = 30; // 3 s of 100 ms sub-blocks
const HIST_MIN: f32 = -70.0;
const HIST_STEP: f32 = 0.05;
const HIST_BINS: usize = 1800; // -70 .. +20 LUFS
/// True peak (ITU-R BS.1770-4 Annex 2): 4× oversampling through a Kaiser-windowed sinc
/// interpolation filter of `TP_TAPS * TP_PHASES` (96) taps.
const TP_PHASES: usize = 4;
const TP_TAPS: usize = 24;
/// Kaiser window shape for the meter: passband flat (±0.05 dB) to about 20 kHz at 48 kHz.
const TP_KAISER_BETA: f64 = 6.0;

/// Modified Bessel function of the first kind, order 0 (the Kaiser window's kernel).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0f64, 1.0f64);
    for k in 1..64 {
        term *= (x / (2.0 * f64::from(k))).powi(2);
        sum += term;
        if term < 1e-15 * sum {
            break;
        }
    }
    sum
}

/// A polyphase interpolation filter: a Kaiser-windowed (`beta`) sinc with its cutoff at the
/// original Nyquist, split into `P` branches of `T` taps. Applied to the newest `T` samples
/// (newest first), branch `k` estimates the signal `k / P` of a sample after the one `T / 2`
/// samples back: branch 0 is that sample itself, the others lie between it and the next. The branches are not renormalised: the prototype's own passband
/// gain is what keeps high-frequency inter-sample peaks at their real height.
pub(crate) fn interpolation_coefs<const P: usize, const T: usize>(beta: f64) -> [[f32; T]; P] {
    let mut coefs = [[0.0f32; T]; P];
    let half = (T / 2) as f64;
    let i0_beta = bessel_i0(beta);
    for (k, ph) in coefs.iter_mut().enumerate() {
        for (j, slot) in ph.iter_mut().enumerate() {
            // Distance (in input samples) from the estimated point to sample `j` back.
            let d = j as f64 - half + k as f64 / P as f64;
            let r = d / half;
            let w = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - r * r).sqrt()) / i0_beta };
            let pd = std::f64::consts::PI * d;
            let sinc = if pd.abs() < 1e-12 { 1.0 } else { pd.sin() / pd };
            *slot = (sinc * w) as f32;
        }
    }
    coefs
}

/// The largest absolute value among the inter-sample estimates (branches 1.. of `coefs`) from
/// the history `h` (the newest `T` samples, newest first). Branch 0 is left out: it is the
/// sample `T / 2` back itself (its other taps sit on the sinc's zeros), which the caller has.
pub(crate) fn between_peak<const P: usize, const T: usize>(coefs: &[[f32; T]; P], h: &[f32]) -> f32 {
    coefs.iter().skip(1).fold(0.0f32, |peak, ph| peak.max(dot(ph, h).abs()))
}

/// `a · b` over their common length, in eight running sums (so it vectorises).
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    for (x, y) in ca.iter().zip(cb) {
        for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
            *s += x * y;
        }
    }
    acc.iter().sum::<f32>() + tail
}

/// The newest `N / 2` samples of a signal, newest first, as one contiguous window: each sample
/// is written twice, `N / 2` apart, so pushing one is two stores, not a shift of the window.
#[derive(Debug, Clone, Copy)]
pub(crate) struct History<const N: usize> {
    buf: [f32; N],
    pos: usize,
}

impl<const N: usize> Default for History<N> {
    fn default() -> Self {
        History { buf: [0.0; N], pos: 0 }
    }
}

impl<const N: usize> History<N> {
    /// Pushes `x` as the newest sample.
    pub(crate) fn push(&mut self, x: f32) {
        let half = N / 2;
        self.pos = if self.pos == 0 { half.saturating_sub(1) } else { self.pos - 1 };
        for at in [self.pos, self.pos + half] {
            if let Some(v) = self.buf.get_mut(at) {
                *v = x;
            }
        }
    }

    /// The window, newest first.
    pub(crate) fn window(&self) -> &[f32] {
        self.buf.get(self.pos..self.pos + N / 2).unwrap_or(&[])
    }

    /// The sample `k` back (0: the newest).
    pub(crate) fn back(&self, k: usize) -> f32 {
        self.window().get(k).copied().unwrap_or(0.0)
    }
}

/// The meter's true-peak filter (see [`interpolation_coefs`]), cutoff at the original Nyquist.
fn true_peak_coefs() -> [[f32; TP_TAPS]; TP_PHASES] {
    interpolation_coefs(TP_KAISER_BETA)
}

/// ITU-R BS.1770 / EBU R128 loudness meter.
#[derive(Debug, Clone)]
pub struct LoudnessMeter {
    sr: f32,
    weights: Vec<f32>,
    filters: Vec<[Biquad; 2]>,
    block_len: usize,
    block_pos: usize,
    block_acc: Vec<f64>,
    sub: [f64; SUB_BLOCKS],
    sub_idx: usize,
    sub_filled: usize,
    hist_count: Vec<u64>,
    hist_energy: Vec<f64>,
    tp_coefs: [[f32; TP_TAPS]; TP_PHASES],
    tp_hist: Vec<History<{ 2 * TP_TAPS }>>,
    true_peak: f32,
    max_momentary: f32,
}

/// Loudness from mean-square energy (BS.1770 eq. 2).
fn lufs(energy: f64) -> f32 {
    if energy <= 1e-20 { MIN_DB } else { (-0.691 + 10.0 * energy.log10()) as f32 }
}

/// K-weighting pre-filter (high shelf) and RLB high-pass designed for any sample rate.
fn k_weighting(sr: f32) -> [Coeffs; 2] {
    let sr = f64::from(sr);
    // Bilinear prewarp. A corner at or past Nyquist wraps tan() negative (poles outside the unit
    // circle) or aliases it, so it is held just below Nyquist, as `Coeffs::design` does.
    let prewarp = |f0: f64| (std::f64::consts::PI * f0.min(sr * 0.49) / sr).tan();
    // Stage 1: shelving.
    let (f0, g, q) = (1_681.974_450_955_533, 3.999_843_853_973_347, 0.707_175_236_955_419_6);
    let k = prewarp(f0);
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let s1 = Coeffs {
        b0: ((vh + vb * k / q + k * k) / a0) as f32,
        b1: (2.0 * (k * k - vh) / a0) as f32,
        b2: ((vh - vb * k / q + k * k) / a0) as f32,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / q + k * k) / a0) as f32,
    };
    // Stage 2: high-pass.
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = prewarp(f0);
    let a0 = 1.0 + k / q + k * k;
    let s2 = Coeffs { b0: 1.0, b1: -2.0, b2: 1.0, a1: (2.0 * (k * k - 1.0) / a0) as f32, a2: ((1.0 - k / q + k * k) / a0) as f32 };
    [s1, s2]
}

impl LoudnessMeter {
    /// Creates a meter. Channel weights default to 1.0, except a 6-channel (5.1: L R C LFE Ls Rs)
    /// layout which uses 0 for LFE and 1.41 for the surrounds.
    pub fn new(sample_rate: f32, channels: usize) -> Self {
        let sr = sane_sr(sample_rate);
        let channels = channels.clamp(1, crate::util::MAX_CHANNELS);
        let weights = if channels == 6 { vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41] } else { vec![1.0; channels] };
        let [c1, c2] = k_weighting(sr);
        let tp_coefs = true_peak_coefs();
        Self {
            sr,
            weights,
            filters: vec![[Biquad::new(c1), Biquad::new(c2)]; channels],
            block_len: ((sr * 0.1).round() as usize).max(1),
            block_pos: 0,
            block_acc: vec![0.0; channels],
            sub: [0.0; SUB_BLOCKS],
            sub_idx: 0,
            sub_filled: 0,
            hist_count: vec![0; HIST_BINS],
            hist_energy: vec![0.0; HIST_BINS],
            tp_coefs,
            tp_hist: vec![History::default(); channels],
            true_peak: 0.0,
            max_momentary: MIN_DB,
        }
    }

    /// Overrides a channel's weighting (e.g. 1.41 for surrounds, 0 to exclude LFE).
    pub fn set_channel_weight(&mut self, channel: usize, weight: f32) {
        if let Some(w) = self.weights.get_mut(channel) {
            *w = if weight.is_finite() { weight.clamp(0.0, 4.0) } else { 1.0 };
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    pub fn reset(&mut self) {
        for f in &mut self.filters {
            f[0].reset();
            f[1].reset();
        }
        self.block_pos = 0;
        self.block_acc.iter_mut().for_each(|v| *v = 0.0);
        self.sub = [0.0; SUB_BLOCKS];
        self.sub_idx = 0;
        self.sub_filled = 0;
        self.hist_count.iter_mut().for_each(|v| *v = 0);
        self.hist_energy.iter_mut().for_each(|v| *v = 0.0);
        self.tp_hist.iter_mut().for_each(|h| *h = History::default());
        self.true_peak = 0.0;
        self.max_momentary = MIN_DB;
    }

    /// Feeds planar audio. Channels beyond the meter's channel count are ignored.
    pub fn process(&mut self, io: &[Vec<f32>], frames: usize) {
        let ch = self.filters.len().min(io.len());
        let frames = io.iter().take(ch).map(Vec::len).fold(frames, usize::min);
        for n in 0..frames {
            for c in 0..ch {
                let x = io.get(c).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
                if let Some([f1, f2]) = self.filters.get_mut(c) {
                    let y = f2.process_sample(f1.process_sample(x));
                    if let Some(acc) = self.block_acc.get_mut(c) {
                        *acc += f64::from(y) * f64::from(y);
                    }
                }
                self.true_peak_sample(c, x);
            }
            self.block_pos += 1;
            if self.block_pos >= self.block_len {
                self.finish_sub_block();
            }
        }
    }

    fn true_peak_sample(&mut self, c: usize, x: f32) {
        let Some(h) = self.tp_hist.get_mut(c) else { return };
        h.push(x);
        // Each sample counts as it comes in; the estimates between samples, half the filter later.
        let peak = between_peak(&self.tp_coefs, h.window()).max(x.abs());
        if peak > self.true_peak {
            self.true_peak = peak;
        }
    }

    fn finish_sub_block(&mut self) {
        let len = self.block_pos.max(1) as f64;
        let mut energy = 0.0;
        for (acc, w) in self.block_acc.iter_mut().zip(&self.weights) {
            energy += f64::from(*w) * *acc / len;
            *acc = 0.0;
        }
        self.block_pos = 0;
        if let Some(s) = self.sub.get_mut(self.sub_idx) {
            *s = energy;
        }
        self.sub_idx = (self.sub_idx + 1) % SUB_BLOCKS;
        self.sub_filled = (self.sub_filled + 1).min(SUB_BLOCKS);
        if self.sub_filled >= 4 {
            let z = self.recent_energy(4);
            let l = lufs(z);
            self.max_momentary = self.max_momentary.max(l);
            if l > HIST_MIN {
                let bin = (((l - HIST_MIN) / HIST_STEP) as usize).min(HIST_BINS - 1);
                if let (Some(c), Some(e)) = (self.hist_count.get_mut(bin), self.hist_energy.get_mut(bin)) {
                    *c += 1;
                    *e += z;
                }
            }
        }
    }

    /// Mean energy of the last `n` completed 100 ms sub-blocks.
    fn recent_energy(&self, n: usize) -> f64 {
        let n = n.min(self.sub_filled);
        if n == 0 {
            return 0.0;
        }
        let mut sum = 0.0;
        for k in 1..=n {
            let idx = (self.sub_idx + SUB_BLOCKS - k) % SUB_BLOCKS;
            sum += self.sub.get(idx).copied().unwrap_or(0.0);
        }
        sum / n as f64
    }

    /// Momentary loudness (400 ms window), LUFS. -144 until 400 ms have been measured.
    pub fn momentary_lufs(&self) -> f32 {
        if self.sub_filled < 4 { MIN_DB } else { lufs(self.recent_energy(4)) }
    }

    /// Short-term loudness (3 s window, or what is available so far), LUFS.
    pub fn short_term_lufs(&self) -> f32 {
        if self.sub_filled < 4 { MIN_DB } else { lufs(self.recent_energy(SUB_BLOCKS)) }
    }

    /// Highest momentary loudness seen.
    pub fn max_momentary_lufs(&self) -> f32 {
        self.max_momentary
    }

    /// Gated integrated loudness (absolute gate -70 LUFS, relative gate -10 LU), LUFS.
    pub fn integrated_lufs(&self) -> f32 {
        let (mut n, mut e) = (0u64, 0.0f64);
        for (c, s) in self.hist_count.iter().zip(&self.hist_energy) {
            n += c;
            e += s;
        }
        if n == 0 {
            return MIN_DB;
        }
        let gate = lufs(e / n as f64) - 10.0;
        let (mut n2, mut e2) = (0u64, 0.0f64);
        for (i, (c, s)) in self.hist_count.iter().zip(&self.hist_energy).enumerate() {
            let center = HIST_MIN + (i as f32 + 0.5) * HIST_STEP;
            if center > gate {
                n2 += c;
                e2 += s;
            }
        }
        if n2 == 0 { MIN_DB } else { lufs(e2 / n2 as f64) }
    }

    /// Maximum true peak (4× oversampled) in dBTP.
    pub fn true_peak_db(&self) -> f32 {
        gain_to_db(self.true_peak)
    }
}
