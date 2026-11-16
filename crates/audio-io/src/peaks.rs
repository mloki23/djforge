//! Multi-resolution overview for waveform drawing.
//!
//! Level 0 sums up each block of [`BLOCK`] samples (min, max and sum of squares); each following
//! level merges [`FANOUT`] blocks of the level below, until a level has a single block. Range
//! queries combine the coarsest whole blocks that fit inside the range and read the partial blocks
//! at its edges from the samples, so a query is exact and costs O(log n) regardless of zoom.
//!
//! [`Peaks::build_with_bands`] also keeps, per block and level, the energy of three frequency
//! bands ([`BANDS`]: low, mid, high), for waveforms coloured by frequency. Each band is the
//! signal through 12 dB/octave Butterworth filters, squared and smoothed over a few
//! milliseconds; a block stores its mean as one byte per band on a log scale ([`band_code`]), so
//! the bands add 3 bytes per [`BLOCK`] samples (4 with the coarser levels): about 21.6 MB per
//! channel-hour at 48 kHz, 43 MB for an hour of stereo, next to the 86 MB per channel-hour of
//! the min/max blocks.

use std::sync::LazyLock;

/// Samples per level-0 block.
pub const BLOCK: usize = 32;
/// Blocks of level `n` merged into one block of level `n + 1`.
pub const FANOUT: usize = 4;
/// Upper bound on the number of spans [`Peaks::spans`] will produce.
pub const MAX_COLUMNS: usize = 1 << 20;

/// Multi-resolution overview of one channel for waveform drawing.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Peaks {
    len: usize,
    levels: Vec<Vec<Block>>,
    /// Per level, each block's mean band energy ([`band_code`]); empty when built without bands.
    #[serde(default)]
    bands: Vec<Vec<[u8; 3]>>,
}

/// The bands' edges in Hz: low is below the first, mid between them, high above the second.
pub const BANDS: [f64; 2] = [200.0, 2500.0];
/// The lowest sample rate bands are kept at: the high band needs room above its edge.
const BAND_MIN_RATE: u32 = 8000;
/// How long the bands' energy is smoothed over, in seconds, so a block's energy doesn't follow
/// the phase of a bass note.
const BAND_SMOOTHING: f64 = 0.003;
/// The quietest band level [`band_code`] tells apart from silence, in dB, and its step.
const BAND_FLOOR_DB: f64 = -90.0;
const BAND_STEP_DB: f64 = 0.375;

/// The byte a mean band energy (mean square) is stored as: 0 is silence (below
/// [`BAND_FLOOR_DB`]), and each step above it [`BAND_STEP_DB`] louder, up to about +5 dB.
fn band_code(energy: f64) -> u8 {
    let db = 10.0 * energy.max(0.0).log10();
    if !db.is_finite() || db < BAND_FLOOR_DB {
        return 0;
    }
    ((db - BAND_FLOOR_DB) / BAND_STEP_DB + 1.0).round().clamp(1.0, 255.0) as u8
}

/// The mean energy each [`band_code`] byte stands for.
static BAND_ENERGY: LazyLock<[f32; 256]> =
    LazyLock::new(|| std::array::from_fn(|k| if k == 0 { 0.0 } else { 10f64.powf((BAND_FLOOR_DB + (k - 1) as f64 * BAND_STEP_DB) / 10.0) as f32 }));

fn band_energy(code: u8) -> f32 {
    BAND_ENERGY.get(usize::from(code)).copied().unwrap_or(0.0)
}

/// A second-order filter (transposed direct form II), in f64 so a 200 Hz edge stays exact at
/// high sample rates.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    /// A Butterworth low-pass (`high` false) or high-pass at `hz` (RBJ's cookbook, Q = 1/sqrt 2).
    fn new(hz: f64, rate: f64, high: bool) -> Biquad {
        let w = std::f64::consts::TAU * hz / rate;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / std::f64::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high { ((1.0 + cos) / 2.0, -(1.0 + cos)) } else { ((1.0 - cos) / 2.0, 1.0 - cos) };
        Biquad { b: [b0 / a0, b1 / a0, b0 / a0], a: [-2.0 * cos / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    /// Silence decays the state into subnormals, which are slow to compute with: clear them.
    fn flush(&mut self) {
        for z in &mut self.z {
            if z.abs() < 1e-30 {
                *z = 0.0;
            }
        }
    }
}

/// Per [`BLOCK`] of `samples` at `rate`: the sums of the low, mid and high bands' smoothed
/// energy, and the number of samples summed.
fn band_blocks(samples: &[f32], rate: u32) -> Vec<([f64; 3], usize)> {
    let r = f64::from(rate);
    let [lo, hi] = BANDS;
    let mut low = Biquad::new(lo, r, false);
    let (mut mid_hp, mut mid_lp) = (Biquad::new(lo, r, true), Biquad::new(hi, r, false));
    let mut high = Biquad::new(hi, r, true);
    let k = 1.0 - (-1.0 / (BAND_SMOOTHING * r)).exp();
    let mut env = [0.0f64; 3];
    samples
        .chunks(BLOCK)
        .map(|chunk| {
            let mut sum = [0.0f64; 3];
            for &s in chunk {
                let x = if s.is_finite() { f64::from(s) } else { 0.0 };
                let bands = [low.run(x), mid_lp.run(mid_hp.run(x)), high.run(x)];
                for ((e, sum), y) in env.iter_mut().zip(&mut sum).zip(bands) {
                    *e += k * (y * y - *e);
                    *sum += *e;
                }
            }
            for f in [&mut low, &mut mid_hp, &mut mid_lp, &mut high] {
                f.flush();
            }
            env = env.map(|e| if e < 1e-60 { 0.0 } else { e });
            (sum, chunk.len())
        })
        .collect()
}

/// What a run of samples sums up to; NaN samples count for nothing.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct Block {
    min: f32,
    max: f32,
    squares: f32,
}

impl Block {
    const EMPTY: Block = Block { min: f32::INFINITY, max: f32::NEG_INFINITY, squares: 0.0 };

    fn point(s: f32) -> Block {
        Block { min: s, max: s, squares: if s.is_finite() { s * s } else { 0.0 } }
    }

    fn of(samples: &[f32]) -> Block {
        samples.iter().fold(Block::EMPTY, |b, &s| b.merge(Block::point(s)))
    }

    fn merge(self, o: Block) -> Block {
        Block { min: self.min.min(o.min), max: self.max.max(o.max), squares: self.squares + o.squares }
    }

    /// (min, max), or (0, 0) when nothing was summed up.
    fn range(self) -> (f32, f32) {
        if self.min > self.max { (0.0, 0.0) } else { (self.min, self.max) }
    }
}

impl Peaks {
    /// Build the overview of `samples` (one channel). NaN samples are ignored.
    pub fn build(samples: &[f32]) -> Peaks {
        if samples.is_empty() {
            return Peaks::default();
        }
        let mut levels: Vec<Vec<Block>> = vec![samples.chunks(BLOCK).map(Block::of).collect()];
        while let Some(last) = levels.last().filter(|l| l.len() > 1) {
            let next = last.chunks(FANOUT).map(|c| c.iter().fold(Block::EMPTY, |a, &b| a.merge(b))).collect();
            levels.push(next);
        }
        Peaks { len: samples.len(), levels, bands: Vec::new() }
    }

    /// [`Peaks::build`], also keeping each block's band energies ([`BANDS`]) for `samples` at
    /// `sample_rate`. Below 8 kHz the bands are left out, as [`Peaks::build`] does.
    pub fn build_with_bands(samples: &[f32], sample_rate: u32) -> Peaks {
        let mut p = Peaks::build(samples);
        if sample_rate < BAND_MIN_RATE || samples.is_empty() {
            return p;
        }
        // Each level sums the energy of the level below and is stored as its mean, so the
        // coarser levels carry no rounding from the finer ones.
        let mut level = band_blocks(samples, sample_rate);
        while p.bands.len() < p.levels.len() {
            p.bands.push(level.iter().map(|(sum, n)| sum.map(|e| band_code(e / (*n).max(1) as f64))).collect());
            level = level
                .chunks(FANOUT)
                .map(|c| c.iter().fold(([0.0; 3], 0), |(a, n), (b, m)| ([a[0] + b[0], a[1] + b[1], a[2] + b[2]], n + m)))
                .collect();
        }
        p
    }

    /// Whether the overview keeps band energies ([`Peaks::build_with_bands`]).
    pub fn has_bands(&self) -> bool {
        !self.bands.is_empty()
    }

    /// Number of samples the overview was built from.
    pub fn len_samples(&self) -> usize {
        self.len
    }

    /// Number of resolution levels (0 for an empty overview).
    pub fn num_levels(&self) -> usize {
        self.levels.len()
    }

    /// (min, max) over the sample range `[start, end)`, clamped to the data. `samples` must be the
    /// data the overview was built from. Returns `(0.0, 0.0)` for an empty range.
    pub fn minmax(&self, samples: &[f32], start: usize, end: usize) -> (f32, f32) {
        self.sum(samples, start, end).range()
    }

    fn sum(&self, samples: &[f32], start: usize, end: usize) -> Block {
        let end = end.min(self.len).min(samples.len());
        let (first, last) = (start.div_ceil(BLOCK), end / BLOCK);
        let raw = |a: usize, b: usize| Block::of(samples.get(a..b).unwrap_or_default());
        if first >= last {
            return raw(start, end);
        }
        raw(start, first * BLOCK).merge(raw(last * BLOCK, end)).merge(self.blocks(first, last))
    }

    /// The level-0 blocks `[lo, hi)`, summed from the coarsest blocks that fit.
    fn blocks(&self, lo: usize, hi: usize) -> Block {
        let mut acc = Block::EMPTY;
        self.cover(lo, hi, |lvl, i| acc = acc.merge(self.levels.get(lvl).and_then(|l| l.get(i)).copied().unwrap_or(Block::EMPTY)));
        acc
    }

    /// Call `add(level, index)` for the coarsest blocks that together cover the level-0 blocks
    /// `[lo, hi)` exactly.
    fn cover(&self, mut lo: usize, mut hi: usize, mut add: impl FnMut(usize, usize)) {
        let n = self.levels.len();
        for lvl in 0..n {
            if lvl + 1 == n || hi.saturating_sub(lo) < 2 * FANOUT {
                (lo..hi).for_each(|i| add(lvl, i));
                break;
            }
            while !lo.is_multiple_of(FANOUT) {
                add(lvl, lo);
                lo += 1;
            }
            while !hi.is_multiple_of(FANOUT) {
                hi -= 1;
                add(lvl, hi);
            }
            lo /= FANOUT;
            hi /= FANOUT;
        }
    }

    /// The RMS of the low, mid and high bands over the blocks that overlap the sample range
    /// `[start, end)`, clamped to the data; `None` without bands or for an empty range.
    pub fn bands(&self, start: usize, end: usize) -> Option<[f32; 3]> {
        let blocks = self.bands.first()?.len();
        let end = end.min(self.len);
        let (lo, hi) = (start / BLOCK, end.div_ceil(BLOCK).min(blocks));
        if start >= end || lo >= hi {
            return None;
        }
        let (mut sum, mut weight) = ([0.0f32; 3], 0.0f32);
        self.cover(lo, hi, |lvl, i| {
            let Some(code) = self.bands.get(lvl).and_then(|l| l.get(i)) else { return };
            // The level-0 blocks this one stands for (fewer for the last).
            let span = FANOUT.saturating_pow(lvl as u32);
            let w = i.saturating_add(1).saturating_mul(span).min(blocks).saturating_sub(i.saturating_mul(span)) as f32;
            for (s, &c) in sum.iter_mut().zip(code) {
                *s += band_energy(c) * w;
            }
            weight += w;
        });
        (weight > 0.0).then(|| sum.map(|e| (e / weight).sqrt()))
    }

    /// [`Peaks::bands`] over each span [`Peaks::rms_spans`] sums up; `None` for spans outside
    /// the data, and for every span without bands.
    pub fn band_spans(&self, start: f64, step: f64, overlap: f64, cols: usize) -> Vec<Option<[f32; 3]>> {
        if !self.has_bands() {
            return vec![None; cols.min(MAX_COLUMNS)];
        }
        self.windows(self.len, start, step, overlap, cols)
            .map(|w| w.and_then(|(a, b)| self.bands(a.floor() as usize, (b.ceil() as usize).saturating_add(1))))
            .collect()
    }

    /// The spans of sample positions `[start + c * step, start + (c + 1) * step + overlap]` that
    /// reach the data, clamped to it.
    fn windows(&self, samples: usize, start: f64, step: f64, overlap: f64, cols: usize) -> impl Iterator<Item = Option<(f64, f64)>> {
        let n = self.len.min(samples);
        let valid = n > 0 && start.is_finite() && step.is_finite() && step > 0.0 && overlap.is_finite() && overlap >= 0.0;
        let last = n.saturating_sub(1) as f64;
        (0..cols.min(MAX_COLUMNS)).map(move |c| {
            let (a, b) = (start + step * c as f64, start + step * (c + 1) as f64 + overlap);
            (valid && b >= 0.0 && a <= last).then(|| (a.max(0.0), b.min(last)))
        })
    }

    /// (min, max) of the line through `samples` over each of `cols` spans of sample positions
    /// `[start + c * step, start + (c + 1) * step + overlap]`; fractional positions interpolate
    /// between samples. Neighbouring spans share at least their boundary, so a waveform drawn from
    /// them stays connected at any zoom. `None` for spans outside the data.
    pub fn spans(&self, samples: &[f32], start: f64, step: f64, overlap: f64, cols: usize) -> Vec<Option<(f32, f32)>> {
        let at = |t: f64| {
            let i = t.floor();
            let a = samples.get(i as usize).copied().unwrap_or(0.0);
            let b = samples.get(i as usize + 1).copied().unwrap_or(a);
            Block::point(a + (b - a) * (t - i) as f32)
        };
        self.windows(samples.len(), start, step, overlap, cols)
            .map(|w| w.map(|(a, b)| self.sum(samples, a.ceil() as usize, b.floor() as usize + 1).merge(at(a)).merge(at(b)).range()))
            .collect()
    }

    /// The RMS of `samples` over each span [`Peaks::spans`] gives, including the samples just
    /// outside a span's ends.
    pub fn rms_spans(&self, samples: &[f32], start: f64, step: f64, overlap: f64, cols: usize) -> Vec<Option<f32>> {
        self.windows(samples.len(), start, step, overlap, cols)
            .map(|w| {
                w.map(|(a, b)| {
                    let (s, e) = (a.floor() as usize, (b.ceil() as usize).saturating_add(1).min(self.len).min(samples.len()));
                    (self.sum(samples, s, e).squares / e.saturating_sub(s).max(1) as f32).sqrt()
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brute(samples: &[f32], start: usize, end: usize) -> (f32, f32) {
        let s = &samples[start..end];
        (s.iter().copied().fold(f32::INFINITY, f32::min), s.iter().copied().fold(f32::NEG_INFINITY, f32::max))
    }

    fn signal(n: usize) -> Vec<f32> {
        (0..n).map(|i| ((i as f32 * 0.037).sin() * (i as f32 * 0.0011).cos()) + if i % 997 == 0 { 0.5 } else { 0.0 }).collect()
    }

    #[test]
    fn every_range_is_exact() {
        let x = signal(100_003);
        let p = Peaks::build(&x);
        for &(a, b) in &[(0, 32), (0, 100_003), (32, 64_000), (3200, 3232), (96, 99_968), (64, 100_003), (5, 6), (31, 33), (1000, 1031)] {
            assert_eq!(p.minmax(&x, a, b), brute(&x, a, b), "range {a}..{b}");
        }
        let mut seed = 12345u64;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = (seed >> 33) as usize % 100_003;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let b = a + 1 + (seed >> 33) as usize % (100_003 - a);
            assert_eq!(p.minmax(&x, a, b), brute(&x, a, b), "range {a}..{b}");
        }
    }

    #[test]
    fn degenerate_inputs() {
        let p = Peaks::build(&[]);
        assert_eq!(p.len_samples(), 0);
        assert_eq!(p.minmax(&[], 0, 10), (0.0, 0.0));
        assert_eq!(p.spans(&[], 0.0, 1.0, 0.0, 4), vec![None; 4]);
        let x = [0.5, -0.25];
        let p = Peaks::build(&x);
        assert_eq!(p.minmax(&x, 0, 2), (-0.25, 0.5));
        assert_eq!(p.minmax(&x, 5, 3), (0.0, 0.0));
        assert_eq!(p.minmax(&x, 0, usize::MAX), (-0.25, 0.5));
        assert_eq!(p.minmax(&x[..1], 0, 2), (0.5, 0.5));
        for (start, step, overlap) in [
            (f64::NAN, 1.0, 0.0),
            (0.0, 0.0, 0.0),
            (0.0, -1.0, 0.0),
            (0.0, f64::INFINITY, 0.0),
            (f64::NEG_INFINITY, 1.0, 0.0),
            (0.0, 1.0, -0.5),
            (0.0, 1.0, f64::NAN),
        ] {
            assert_eq!(p.spans(&x, start, step, overlap, 3), vec![None; 3]);
        }
        assert_eq!(p.spans(&x, 0.0, 1.0, 0.0, 0).len(), 0);
        assert_eq!(p.spans(&x, 0.0, 1.0, 0.0, usize::MAX).len(), MAX_COLUMNS);
        let x = [f32::NAN, 1.0];
        let p = Peaks::build(&x);
        assert_eq!(p.minmax(&x, 0, 2), (1.0, 1.0));
        assert_eq!(p.spans(&x, 0.0, 1.0, 0.0, 1), vec![Some((1.0, 1.0))]);
        assert!(p.rms_spans(&x, 0.0, 1.0, 0.0, 1)[0].is_some_and(f32::is_finite));
    }

    #[test]
    fn rms_spans_match_the_samples() {
        let x = signal(20_000);
        let p = Peaks::build(&x);
        let rms = |s: usize, e: usize| (x[s..e].iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / (e - s) as f64).sqrt() as f32;
        for (step, overlap) in [(1.5, 0.0), (37.0, 37.0), (1000.0, 0.0), (4096.0, 512.0)] {
            let cols = ((19_999.0 - overlap) / step) as usize;
            for (c, got) in p.rms_spans(&x, 0.0, step, overlap, cols).into_iter().enumerate() {
                let (a, b) = (c as f64 * step, ((c + 1) as f64 * step + overlap).min(19_999.0));
                let (got, want) = (got.unwrap(), rms(a.floor() as usize, (b.ceil() as usize + 1).min(20_000)));
                assert!((got - want).abs() <= want * 1e-4 + 1e-6, "span {c} at step {step}: {got} vs {want}");
            }
        }
        let sine: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let p = Peaks::build(&sine);
        let r = p.rms_spans(&sine, 0.0, 47_000.0, 0.0, 1)[0].unwrap();
        assert!((r - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3, "{r}");
        assert_eq!(p.rms_spans(&sine, f64::NAN, 1.0, 0.0, 2), vec![None, None]);
        assert_eq!(p.rms_spans(&sine, 50_000.0, 1.0, 0.0, 1), vec![None]);
    }

    #[test]
    fn spans_cover_their_samples_and_touch_their_neighbours() {
        let x = signal(10_000);
        let p = Peaks::build(&x);
        for step in [0.3, 1.0, 7.5, 100.0, 333.3] {
            let cols = (9_999.0 / step) as usize;
            let spans = p.spans(&x, 0.0, step, 0.0, cols);
            for (c, s) in spans.iter().enumerate() {
                let (lo, hi) = s.unwrap_or_else(|| panic!("span {c} at step {step}"));
                let (a, b) = ((c as f64 * step).ceil() as usize, ((c + 1) as f64 * step).floor() as usize);
                if a <= b {
                    let truth = brute(&x, a, b + 1);
                    assert!(lo <= truth.0 && hi >= truth.1, "span {c} at step {step}");
                }
                if let Some(Some((nlo, nhi))) = spans.get(c + 1) {
                    assert!(*nlo <= hi && *nhi >= lo, "spans {c} and {} at step {step} don't touch", c + 1);
                }
            }
            let all = spans.iter().flatten().fold((f32::INFINITY, f32::NEG_INFINITY), |a, s| (a.0.min(s.0), a.1.max(s.1)));
            assert_eq!(all, brute(&x, 0, ((cols as f64 * step).floor() as usize + 1).min(10_000)), "step {step}");
        }
    }

    #[test]
    fn spans_follow_the_line_between_samples() {
        let x = [0.0, 1.0, -1.0, 0.5];
        let p = Peaks::build(&x);
        assert_eq!(p.spans(&x, 0.0, 0.25, 0.0, 4), vec![Some((0.0, 0.25)), Some((0.25, 0.5)), Some((0.5, 0.75)), Some((0.75, 1.0))]);
        assert_eq!(p.spans(&x, 0.5, 1.0, 0.0, 1), vec![Some((0.0, 1.0))]);
        let spans = vec![None, Some((0.0, 0.0)), Some((0.0, 1.0)), Some((-1.0, 1.0)), Some((-1.0, 0.5)), Some((0.5, 0.5)), None];
        assert_eq!(p.spans(&x, -2.0, 1.0, 0.0, 7), spans);
        assert_eq!(p.spans(&x, 0.0, 1.0, 1.0, 3), vec![Some((-1.0, 1.0)), Some((-1.0, 1.0)), Some((-1.0, 0.5))]);
        assert_eq!(p.spans(&x, 0.0, 1.0, 0.5, 3), vec![Some((0.0, 1.0)), Some((-1.0, 1.0)), Some((-1.0, 0.5))]);
    }

    fn sine(hz: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * std::f32::consts::TAU * hz / 48_000.0).sin() * amp).collect()
    }

    /// Each band's share of the energy over the middle of `x`, past the filters' start.
    fn shares(x: &[f32]) -> [f32; 3] {
        let p = Peaks::build_with_bands(x, 48_000);
        let b = p.bands(4800, x.len() - 4800).unwrap();
        let total: f32 = b.iter().map(|v| v * v).sum();
        b.map(|v| v * v / total)
    }

    #[test]
    fn a_tone_lands_in_its_band() {
        let [low, ..] = shares(&sine(60.0, 0.5, 48_000));
        assert!(low > 0.95, "60 Hz: {low}");
        let [_, mid, _] = shares(&sine(1000.0, 0.5, 48_000));
        assert!(mid > 0.9, "1 kHz: {mid}");
        let [.., high] = shares(&sine(8000.0, 0.5, 48_000));
        assert!(high > 0.95, "8 kHz: {high}");
        // A tone's band RMS is its own: a sine of amplitude 0.5 has an RMS of 0.354.
        let p = Peaks::build_with_bands(&sine(60.0, 0.5, 48_000), 48_000);
        let low = p.bands(4800, 43_200).unwrap()[0];
        assert!((low - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01, "{low}");
    }

    #[test]
    fn a_mix_shows_its_bands_in_proportion() {
        let (a, b, c) = (sine(60.0, 0.5, 48_000), sine(1000.0, 0.25, 48_000), sine(8000.0, 0.125, 48_000));
        let x: Vec<f32> = a.iter().zip(&b).zip(&c).map(|((a, b), c)| a + b + c).collect();
        let p = Peaks::build_with_bands(&x, 48_000);
        let [low, mid, high] = p.bands(4800, 43_200).unwrap();
        // Amplitudes 4 : 2 : 1, within the filters' overlap.
        assert!((low / mid - 2.0).abs() < 0.2 && (mid / high - 2.0).abs() < 0.2, "{low} {mid} {high}");
    }

    #[test]
    fn coarse_levels_hold_the_mean_of_the_blocks_below() {
        // Loud lows for the first 3 s, then quiet highs: any range's bands are the energy mean of
        // its blocks, whichever levels the query reads them from.
        let mut x = sine(60.0, 0.8, 144_000);
        x.extend(sine(8000.0, 0.05, 100_003));
        let p = Peaks::build_with_bands(&x, 48_000);
        assert_eq!(p.bands.len(), p.levels.len());
        for (l, level) in p.bands.iter().enumerate() {
            assert_eq!(level.len(), p.levels[l].len(), "level {l}");
        }
        let brute = |a: usize, b: usize| -> [f32; 3] {
            let blocks = &p.bands[0][a / BLOCK..b.div_ceil(BLOCK)];
            let mut e = [0.0f64; 3];
            for code in blocks {
                for k in 0..3 {
                    e[k] += f64::from(band_energy(code[k]));
                }
            }
            e.map(|e| (e / blocks.len() as f64).sqrt() as f32)
        };
        for (a, b) in [(0, 244_003), (0, 32), (5000, 200_000), (143_000, 145_000), (31, 33), (64_000, 244_003)] {
            let (got, want) = (p.bands(a, b).unwrap(), brute(a, b));
            for k in 0..3 {
                // Each coarse level is quantised once from exact sums: half a step, 0.19 dB.
                assert!((got[k] - want[k]).abs() <= want[k] * 0.025 + 1e-6, "{a}..{b} band {k}: {got:?} vs {want:?}");
            }
        }
        let [low, _, high] = p.bands(0, 244_003).unwrap();
        assert!(low > 10.0 * high, "{low} {high}");
        assert_eq!(p.bands(244_003, 300_000), None);
        assert_eq!(p.bands(10, 10), None);
    }

    #[test]
    fn bands_need_a_rate_and_tolerate_bad_samples() {
        let x = sine(1000.0, 0.5, 10_000);
        assert!(!Peaks::build(&x).has_bands());
        assert!(!Peaks::build_with_bands(&x, 0).has_bands());
        assert!(!Peaks::build_with_bands(&x, 4000).has_bands());
        assert!(!Peaks::build_with_bands(&[], 48_000).has_bands());
        assert_eq!(Peaks::build(&x).bands(0, 10_000), None);
        assert_eq!(Peaks::build(&x).band_spans(0.0, 10.0, 0.0, 3), vec![None; 3]);
        // The min/max overview is the same with or without bands.
        let p = Peaks::build_with_bands(&x, 44_100);
        assert_eq!(p.levels, Peaks::build(&x).levels);
        let mut y = x.clone();
        y[100] = f32::NAN;
        y[200] = f32::INFINITY;
        let p = Peaks::build_with_bands(&y, 48_000);
        assert!(p.bands(0, 10_000).is_some_and(|b| b.iter().all(|v| v.is_finite())));
        let silent = Peaks::build_with_bands(&[0.0; 4096], 48_000);
        assert_eq!(silent.bands(0, 4096), Some([0.0; 3]));
        assert_eq!(band_code(0.0), 0);
        assert_eq!(band_code(f64::NAN), 0);
        assert_eq!(band_code(1e6), 255);
    }

    #[test]
    fn band_spans_follow_the_spans() {
        let mut x = sine(60.0, 0.5, 48_000);
        x.extend(sine(8000.0, 0.5, 48_000));
        let p = Peaks::build_with_bands(&x, 48_000);
        let spans = p.band_spans(0.0, 9600.0, 0.0, 12);
        assert_eq!(spans.len(), 12);
        let low = |b: [f32; 3]| b[0] > b[2];
        assert!(spans[..4].iter().all(|b| b.is_some_and(low)), "{spans:?}");
        assert!(spans[6..10].iter().all(|b| b.is_some_and(|b| !low(b))), "{spans:?}");
        assert!(spans[10..].iter().all(Option::is_none), "{spans:?}");
        assert_eq!(p.band_spans(f64::NAN, 1.0, 0.0, 2), vec![None, None]);
    }

    /// Build time for 10 minutes of one channel at 48 kHz, in the dev profile:
    /// `cargo test -p soundcraft-audio-io --lib -- --ignored --nocapture build_time`.
    #[test]
    #[ignore = "timing harness"]
    fn build_time_of_ten_minutes() {
        let x = signal(48_000 * 600);
        let t = std::time::Instant::now();
        let p = Peaks::build(&x);
        println!("built {} levels in {:.1} ms", p.num_levels(), t.elapsed().as_secs_f64() * 1e3);
        let t = std::time::Instant::now();
        let p = Peaks::build_with_bands(&x, 48_000);
        println!("built {} levels with bands in {:.1} ms", p.num_levels(), t.elapsed().as_secs_f64() * 1e3);
    }
}
