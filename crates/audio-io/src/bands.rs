//! Frequency bands of a channel, for waveforms coloured by frequency.
//!
//! Each band ([`EDGES`]: low, mid, high) is the signal through 12 dB/octave Butterworth filters,
//! squared and smoothed over a few milliseconds. Like [`crate::peaks::Peaks`], level 0 sums up
//! each block of [`BLOCK`] samples and each following level [`FANOUT`] blocks of the level below,
//! until a level has a single block; a block keeps its mean energy as one byte per band on a log
//! scale ([`code`]). That is 3 bytes per block, 4 with the coarser levels: about 21.6 MB per
//! channel-hour at 48 kHz, 43 MB for an hour of stereo. The build streams: it holds the bytes it
//! keeps and one running sum per level, nothing per block besides.

use crate::peaks::{BLOCK, FANOUT, MAX_COLUMNS, cover, windows};
use std::sync::LazyLock;

/// The bands' edges in Hz: low is below the first, mid between them, high above the second.
pub const EDGES: [f64; 2] = [200.0, 2500.0];
/// The lowest sample rate bands are kept at: the high band needs room above its edge.
const MIN_RATE: u32 = 8000;
/// How long the bands' energy is smoothed over, in seconds, so a block's energy doesn't follow
/// the phase of a bass note.
const SMOOTHING: f64 = 0.003;
/// The quietest band level [`code`] tells apart from silence, in dB, and its step.
const FLOOR_DB: f64 = -90.0;
const STEP_DB: f64 = 0.375;
/// Samples between the build's checks that it is still wanted.
const CHECK_EVERY: usize = 1 << 20;

/// The byte a mean band energy (mean square) is stored as: 0 is silence (below [`FLOOR_DB`]),
/// and each step above it [`STEP_DB`] louder, up to about +5 dB.
fn code(energy: f64) -> u8 {
    let db = 10.0 * energy.max(0.0).log10();
    if !db.is_finite() || db < FLOOR_DB {
        return 0;
    }
    ((db - FLOOR_DB) / STEP_DB + 1.0).round().clamp(1.0, 255.0) as u8
}

/// The mean energy each [`code`] byte stands for.
static ENERGY: LazyLock<[f32; 256]> =
    LazyLock::new(|| std::array::from_fn(|k| if k == 0 { 0.0 } else { 10f64.powf((FLOOR_DB + (k - 1) as f64 * STEP_DB) / 10.0) as f32 }));

fn energy(code: u8) -> f32 {
    ENERGY.get(usize::from(code)).copied().unwrap_or(0.0)
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

/// The low, mid and high bands of one channel, per block and level.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bands {
    len: usize,
    /// Per level, each block's mean band energy ([`code`]).
    levels: Vec<Vec<[u8; 3]>>,
}

/// The running energy sum of the block a level is filling, and how many blocks of the level
/// below it holds.
#[derive(Clone, Copy, Default)]
struct Partial {
    sum: [f64; 3],
    children: usize,
}

impl Bands {
    /// The bands of `samples` (one channel) at `sample_rate`; empty below 8 kHz.
    pub fn build(samples: &[f32], sample_rate: u32) -> Bands {
        Bands::build_while(samples, sample_rate, &|| true).unwrap_or_default()
    }

    /// [`Bands::build`], giving up (`None`) as soon as `wanted` says the bands are no longer
    /// needed; it is asked every million samples or so. Not generic, so the filters are always
    /// compiled (and optimised) here rather than in each caller's crate.
    pub fn build_while(samples: &[f32], sample_rate: u32, wanted: &dyn Fn() -> bool) -> Option<Bands> {
        if sample_rate < MIN_RATE || samples.is_empty() {
            return Some(Bands::default());
        }
        let len = samples.len();
        // The levels' lengths, as `Peaks` has them.
        let mut lens = vec![len.div_ceil(BLOCK)];
        while let Some(&n) = lens.last().filter(|&&n| n > 1) {
            lens.push(n.div_ceil(FANOUT));
        }
        let mut levels: Vec<Vec<[u8; 3]>> = lens.iter().map(|&n| Vec::with_capacity(n)).collect();
        let mut partial = vec![Partial::default(); lens.len()];
        // The samples block `i` of level `lvl` covers (fewer for the last).
        let samples_in = |lvl: usize, i: usize| {
            let span = BLOCK.saturating_mul(FANOUT.saturating_pow(lvl as u32));
            i.saturating_add(1).saturating_mul(span).min(len).saturating_sub(i.saturating_mul(span)).max(1) as f64
        };
        // Store level `lvl`'s next block, of energy `sum`, and add it to the level above;
        // `full` when that completes the level above's block too.
        fn close(levels: &mut [Vec<[u8; 3]>], partial: &mut [Partial], lvl: usize, sum: [f64; 3], samples: f64) -> bool {
            if let Some(level) = levels.get_mut(lvl) {
                level.push(sum.map(|e| code(e / samples)));
            }
            let Some(up) = partial.get_mut(lvl + 1) else { return false };
            for (a, b) in up.sum.iter_mut().zip(sum) {
                *a += b;
            }
            up.children += 1;
            up.children == FANOUT
        }
        let r = f64::from(sample_rate);
        let [lo, hi] = EDGES;
        let mut low = Biquad::new(lo, r, false);
        let (mut mid_hp, mut mid_lp) = (Biquad::new(lo, r, true), Biquad::new(hi, r, false));
        let mut high = Biquad::new(hi, r, true);
        let k = 1.0 - (-1.0 / (SMOOTHING * r)).exp();
        let mut env = [0.0f64; 3];
        for (b, chunk) in samples.chunks(BLOCK).enumerate() {
            if (b * BLOCK).is_multiple_of(CHECK_EVERY) && !wanted() {
                return None;
            }
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
            // Close this block, and each coarser block it completes.
            let (mut lvl, mut sum, mut i) = (0, sum, b);
            while close(&mut levels, &mut partial, lvl, sum, samples_in(lvl, i)) {
                lvl += 1;
                i /= FANOUT;
                let p = partial.get_mut(lvl).map(std::mem::take).unwrap_or_default();
                sum = p.sum;
            }
        }
        // The last, partial block of each coarser level.
        for lvl in 1..lens.len() {
            let Some(p) = partial.get_mut(lvl).map(std::mem::take) else { break };
            if p.children > 0 {
                let i = levels.get(lvl).map_or(0, Vec::len);
                close(&mut levels, &mut partial, lvl, p.sum, samples_in(lvl, i));
            }
        }
        Some(Bands { len, levels })
    }

    /// Whether there are bands: none were kept for audio below 8 kHz, or no audio.
    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
    }

    /// The RMS of the low, mid and high bands over the blocks that overlap the sample range
    /// `[start, end)`, clamped to the data; `None` without bands or for an empty range.
    pub fn rms(&self, start: usize, end: usize) -> Option<[f32; 3]> {
        let blocks = self.levels.first()?.len();
        let end = end.min(self.len);
        let (lo, hi) = (start / BLOCK, end.div_ceil(BLOCK).min(blocks));
        if start >= end || lo >= hi {
            return None;
        }
        let (mut sum, mut weight) = ([0.0f32; 3], 0.0f32);
        cover(self.levels.len(), lo, hi, |lvl, i| {
            let Some(code) = self.levels.get(lvl).and_then(|l| l.get(i)) else { return };
            // The level-0 blocks this one stands for (fewer for the last).
            let span = FANOUT.saturating_pow(lvl as u32);
            let w = i.saturating_add(1).saturating_mul(span).min(blocks).saturating_sub(i.saturating_mul(span)) as f32;
            for (s, &c) in sum.iter_mut().zip(code) {
                *s += energy(c) * w;
            }
            weight += w;
        });
        (weight > 0.0).then(|| sum.map(|e| (e / weight).sqrt()))
    }

    /// [`Bands::rms`] over each span `Peaks::rms_spans` sums up: sample positions
    /// `[start + c * step, start + (c + 1) * step + overlap]` and the samples just outside them.
    /// `None` for spans outside the data, and for every span without bands.
    pub fn spans(&self, start: f64, step: f64, overlap: f64, cols: usize) -> Vec<Option<[f32; 3]>> {
        if self.is_empty() {
            return vec![None; cols.min(MAX_COLUMNS)];
        }
        windows(self.len, start, step, overlap, cols)
            .map(|w| w.and_then(|(a, b)| self.rms(a.floor() as usize, (b.ceil() as usize).saturating_add(1))))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peaks::Peaks;

    fn sine(hz: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * std::f32::consts::TAU * hz / 48_000.0).sin() * amp).collect()
    }

    /// Each band's share of the energy over the middle of `x`, past the filters' start.
    fn shares(x: &[f32]) -> [f32; 3] {
        let b = Bands::build(x, 48_000).rms(4800, x.len() - 4800).unwrap();
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
        let low = Bands::build(&sine(60.0, 0.5, 48_000), 48_000).rms(4800, 43_200).unwrap()[0];
        assert!((low - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01, "{low}");
    }

    #[test]
    fn a_mix_shows_its_bands_in_proportion() {
        let (a, b, c) = (sine(60.0, 0.5, 48_000), sine(1000.0, 0.25, 48_000), sine(8000.0, 0.125, 48_000));
        let x: Vec<f32> = a.iter().zip(&b).zip(&c).map(|((a, b), c)| a + b + c).collect();
        let [low, mid, high] = Bands::build(&x, 48_000).rms(4800, 43_200).unwrap();
        // Amplitudes 4 : 2 : 1, within the filters' overlap.
        assert!((low / mid - 2.0).abs() < 0.2 && (mid / high - 2.0).abs() < 0.2, "{low} {mid} {high}");
    }

    /// Each level's mean band energies, rebuilt the slow way: every coarse block as the
    /// sample-weighted mean of the level-0 blocks it covers.
    fn slow_levels(x: &[f32]) -> Vec<Vec<[f64; 3]>> {
        let full = Bands::build(x, 48_000);
        let level0: Vec<[f64; 3]> = full.levels[0].iter().map(|c| c.map(|c| f64::from(energy(c)))).collect();
        let counts: Vec<f64> = (0..level0.len()).map(|i| (((i + 1) * BLOCK).min(x.len()) - i * BLOCK) as f64).collect();
        let mut out = vec![level0.clone()];
        let mut span = 1;
        while out.last().is_some_and(|l| l.len() > 1) {
            span *= FANOUT;
            let n = level0.len().div_ceil(span);
            out.push(
                (0..n)
                    .map(|j| {
                        let r = j * span..((j + 1) * span).min(level0.len());
                        let w: f64 = counts[r.clone()].iter().sum();
                        let mut e = [0.0; 3];
                        for i in r {
                            for k in 0..3 {
                                e[k] += level0[i][k] * counts[i];
                            }
                        }
                        e.map(|e| e / w)
                    })
                    .collect(),
            );
        }
        out
    }

    #[test]
    fn coarse_levels_hold_the_mean_of_the_blocks_below() {
        // Loud lows for the first 3 s, then quiet highs, with a partial block at the end.
        let mut x = sine(60.0, 0.8, 144_000);
        x.extend(sine(8000.0, 0.05, 100_003));
        let b = Bands::build(&x, 48_000);
        let p = Peaks::build(&x);
        assert_eq!(b.levels.len(), p.num_levels());
        let slow = slow_levels(&x);
        assert_eq!(b.levels.iter().map(Vec::len).collect::<Vec<_>>(), slow.iter().map(Vec::len).collect::<Vec<_>>());
        for (l, (got, want)) in b.levels.iter().zip(&slow).enumerate() {
            for (i, (g, w)) in got.iter().zip(want).enumerate() {
                for k in 0..3 {
                    // Quantised once from exact sums: within a step (0.375 dB), plus level 0's own,
                    // and blocks near the -90 dB floor round to silence.
                    let (g, w) = (f64::from(energy(g[k])), w[k]);
                    assert!((g - w).abs() <= w * 0.2 + 1e-8, "level {l} block {i} band {k}: {g} vs {w}");
                }
            }
        }
        // Any range's bands are the mean of its blocks, whichever levels the query reads.
        let brute = |a: usize, c: usize| -> [f32; 3] {
            let blocks = &b.levels[0][a / BLOCK..c.div_ceil(BLOCK)];
            let mut e = [0.0f64; 3];
            for code in blocks {
                for k in 0..3 {
                    e[k] += f64::from(energy(code[k]));
                }
            }
            e.map(|e| (e / blocks.len() as f64).sqrt() as f32)
        };
        for (a, c) in [(0, 244_003), (0, 32), (5000, 200_000), (143_000, 145_000), (31, 33), (64_000, 244_003)] {
            let (got, want) = (b.rms(a, c).unwrap(), brute(a, c));
            for k in 0..3 {
                assert!((got[k] - want[k]).abs() <= want[k] * 0.025 + 1e-6, "{a}..{c} band {k}: {got:?} vs {want:?}");
            }
        }
        let [low, _, high] = b.rms(0, 244_003).unwrap();
        assert!(low > 10.0 * high, "{low} {high}");
        assert_eq!(b.rms(244_003, 300_000), None);
        assert_eq!(b.rms(10, 10), None);
        // One block, and one sample.
        assert_eq!(Bands::build(&[0.5; 20], 48_000).levels.len(), 1);
        assert_eq!(Bands::build(&[0.5], 48_000).levels.iter().map(Vec::len).collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn bands_need_a_rate_and_tolerate_bad_samples() {
        let x = sine(1000.0, 0.5, 10_000);
        assert!(Bands::build(&x, 0).is_empty());
        assert!(Bands::build(&x, 4000).is_empty());
        assert!(Bands::build(&[], 48_000).is_empty());
        assert_eq!(Bands::build(&x, 0).rms(0, 10_000), None);
        assert_eq!(Bands::build(&x, 0).spans(0.0, 10.0, 0.0, 3), vec![None; 3]);
        assert!(!Bands::build(&x, 44_100).is_empty());
        let mut y = x.clone();
        y[100] = f32::NAN;
        y[200] = f32::INFINITY;
        assert!(Bands::build(&y, 48_000).rms(0, 10_000).is_some_and(|b| b.iter().all(|v| v.is_finite())));
        assert_eq!(Bands::build(&[0.0; 4096], 48_000).rms(0, 4096), Some([0.0; 3]));
        assert_eq!(code(0.0), 0);
        assert_eq!(code(f64::NAN), 0);
        assert_eq!(code(1e6), 255);
    }

    #[test]
    fn a_build_no_longer_wanted_stops() {
        let x = sine(1000.0, 0.5, 3 * CHECK_EVERY);
        let asked = std::cell::Cell::new(0);
        let got = Bands::build_while(&x, 48_000, &|| {
            asked.set(asked.get() + 1);
            asked.get() < 2
        });
        assert_eq!((got, asked.get()), (None, 2));
        assert_eq!(Bands::build_while(&x, 48_000, &|| true), Some(Bands::build(&x, 48_000)));
    }

    #[test]
    fn spans_follow_the_windows() {
        let mut x = sine(60.0, 0.5, 48_000);
        x.extend(sine(8000.0, 0.5, 48_000));
        let b = Bands::build(&x, 48_000);
        let spans = b.spans(0.0, 9600.0, 0.0, 12);
        assert_eq!(spans.len(), 12);
        let low = |b: [f32; 3]| b[0] > b[2];
        assert!(spans[..4].iter().all(|b| b.is_some_and(low)), "{spans:?}");
        assert!(spans[6..10].iter().all(|b| b.is_some_and(|b| !low(b))), "{spans:?}");
        assert!(spans[10..].iter().all(Option::is_none), "{spans:?}");
        assert_eq!(b.spans(f64::NAN, 1.0, 0.0, 2), vec![None, None]);
    }

    /// Build time for 10 minutes of one channel at 48 kHz; time it in the release profile for
    /// figures that stand for the app: `cargo test --release -p soundcraft-audio-io --lib --
    /// --ignored --nocapture bands_build_time`.
    #[test]
    #[ignore = "timing harness"]
    fn bands_build_time_of_ten_minutes() {
        let x: Vec<f32> = (0..48_000 * 600).map(|i| (i as f32 * 0.037).sin() * (i as f32 * 0.0011).cos()).collect();
        let t = std::time::Instant::now();
        let b = Bands::build(&x, 48_000);
        println!("built {} band levels in {:.1} ms", b.levels.len(), t.elapsed().as_secs_f64() * 1e3);
    }
}
