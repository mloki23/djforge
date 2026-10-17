//! Realtime, pitch-preserving time stretch for the mix engine.
//!
//! A safe wrapper over [Signalsmith Stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch)
//! (MIT, vendored under `vendor/`). Each [`Stretcher`] is a streaming processor: every call
//! consumes `in_frames` of source audio and produces `out_frames` of output, so the ratio
//! between them is the playback rate for that block. Rate can change every block (tempo ramps)
//! without clicks; pitch is preserved unless [`Stretcher::set_transpose`] says otherwise.
//!
//! Buffers are channel-major (one slice or `Vec` per channel) like the rest of the engine.
//! Interleaving scratch is allocated once in [`Stretcher::new`]; `process` and `seek` never
//! allocate, block or panic, so they are safe on the audio thread.
//!
//! This is an isolated `unsafe` crate (craftrules never-crash): `unsafe` is denied crate-wide and
//! allowed only in the private `ffi` module.
//!
//! On `wasm32` the C++ library is not built and [`Stretcher::new`] returns
//! [`StretchError::Unsupported`].

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(not(target_arch = "wasm32"))]
mod ffi;

/// Why a stretcher could not be made or a call was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StretchError {
    #[error("time stretch is not available on this platform")]
    Unsupported,
    #[error("invalid stretcher setup: {0}")]
    Invalid(&'static str),
    #[error("block of {frames} frames exceeds the stretcher's limit of {limit}")]
    TooLong { frames: usize, limit: usize },
}

/// Analysis window size: `Default` is the library's recommended quality; `Cheaper` uses
/// smaller windows (less CPU, slightly more smearing on sustained material).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Default,
    Cheaper,
}

/// Largest playback rate a block may ask for (input frames per output frame). DJ use stays far
/// below this; the cap bounds the interleave scratch.
pub const MAX_RATE: usize = 8;

/// A streaming, pitch-preserving time stretcher for `channels` channels.
pub struct Stretcher {
    #[cfg(not(target_arch = "wasm32"))]
    handle: ffi::Handle,
    channels: usize,
    max_out: usize,
    max_in: usize,
    inbuf: Vec<f32>,
    outbuf: Vec<f32>,
}

impl Stretcher {
    /// A stretcher for blocks of at most `max_block` output frames, accepting up to
    /// `max_block * MAX_RATE` input frames per call.
    pub fn new(channels: usize, sample_rate: f32, max_block: usize, quality: Quality) -> Result<Stretcher, StretchError> {
        if channels == 0 || channels > 64 {
            return Err(StretchError::Invalid("channel count must be 1..=64"));
        }
        if !(sample_rate.is_finite() && (8_000.0..=768_000.0).contains(&sample_rate)) {
            return Err(StretchError::Invalid("sample rate out of range"));
        }
        if max_block == 0 || max_block > 1 << 16 {
            return Err(StretchError::Invalid("max block must be 1..=65536"));
        }
        let max_in = max_block.saturating_mul(MAX_RATE);
        #[cfg(target_arch = "wasm32")]
        {
            let _ = quality;
            Err(StretchError::Unsupported)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let handle = ffi::Handle::new(channels, sample_rate, quality == Quality::Cheaper).ok_or(StretchError::Unsupported)?;
            Ok(Stretcher {
                handle,
                channels,
                max_out: max_block,
                max_in,
                inbuf: vec![0.0; max_in.saturating_mul(channels)],
                outbuf: vec![0.0; max_block.saturating_mul(channels)],
            })
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Largest `in_frames` accepted by [`Stretcher::process`] and [`Stretcher::seek`].
    pub fn max_input(&self) -> usize {
        self.max_in
    }

    /// Clears all internal state (use before a jump, then [`Stretcher::seek`]).
    pub fn reset(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.handle.reset();
    }

    /// Delay (frames) from input to the centre of the stretch.
    pub fn input_latency(&self) -> usize {
        #[cfg(not(target_arch = "wasm32"))]
        return self.handle.input_latency();
        #[cfg(target_arch = "wasm32")]
        0
    }

    /// Delay (frames) from the centre of the stretch to the output.
    pub fn output_latency(&self) -> usize {
        #[cfg(not(target_arch = "wasm32"))]
        return self.handle.output_latency();
        #[cfg(target_arch = "wasm32")]
        0
    }

    /// Pitch multiplier (1.0 = unchanged). Non-finite or non-positive values reset to 1.0.
    pub fn set_transpose(&mut self, multiplier: f32) {
        let m = if multiplier.is_finite() && multiplier > 0.0 { multiplier.clamp(0.25, 4.0) } else { 1.0 };
        #[cfg(not(target_arch = "wasm32"))]
        self.handle.set_transpose(m);
        #[cfg(target_arch = "wasm32")]
        let _ = m;
    }

    /// Primes the stretcher after a jump: `input` is the source audio *leading up to* the new
    /// read position (ideally `input_latency() * 2` frames), at the coming playback `rate`.
    pub fn seek<V: AsRef<[f32]>>(&mut self, input: &[V], frames: usize, rate: f64) -> Result<(), StretchError> {
        let frames = self.check(frames, self.max_in)?;
        let rate = if rate.is_finite() && rate > 0.0 { rate.min(MAX_RATE as f64) } else { 1.0 };
        self.interleave_input(input, frames);
        #[cfg(not(target_arch = "wasm32"))]
        self.handle.seek(&mut self.inbuf, frames, rate);
        Ok(())
    }

    /// Consumes `in_frames` from `input` (channel-major; short or missing channels read as
    /// silence, fewer source channels repeat cyclically) and *adds* `out_frames` of stretched
    /// audio into `output`, scaled by `gain`.
    pub fn process<V: AsRef<[f32]>>(
        &mut self,
        input: &[V],
        in_frames: usize,
        output: &mut [Vec<f32>],
        out_frames: usize,
        gain: f32,
    ) -> Result<(), StretchError> {
        let in_frames = self.check(in_frames, self.max_in)?;
        let out_frames = self.check(out_frames, self.max_out)?;
        self.interleave_input(input, in_frames);
        #[cfg(not(target_arch = "wasm32"))]
        self.handle.process(&mut self.inbuf, in_frames, &mut self.outbuf, out_frames);
        let gain = if gain.is_finite() { gain } else { 0.0 };
        let ch = self.channels;
        for (c, dst) in output.iter_mut().enumerate() {
            let sc = c % ch;
            for (i, d) in dst.iter_mut().take(out_frames).enumerate() {
                *d += self.outbuf.get(i * ch + sc).copied().unwrap_or(0.0) * gain;
            }
        }
        Ok(())
    }

    fn check(&self, frames: usize, limit: usize) -> Result<usize, StretchError> {
        if frames > limit { Err(StretchError::TooLong { frames, limit }) } else { Ok(frames) }
    }

    fn interleave_input<V: AsRef<[f32]>>(&mut self, input: &[V], frames: usize) {
        let ch = self.channels;
        let n = input.len();
        for c in 0..ch {
            let src: &[f32] = if n == 0 { &[] } else { input.get(c % n).map_or(&[], AsRef::as_ref) };
            for i in 0..frames {
                if let Some(d) = self.inbuf.get_mut(i * ch + c) {
                    *d = src.get(i).copied().unwrap_or(0.0);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(hz: f32, frames: usize) -> Vec<f32> {
        (0..frames).map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / SR).sin() * 0.5).collect()
    }

    /// Zero-crossing frequency estimate over the steady middle of a signal.
    fn freq(x: &[f32]) -> f32 {
        let mid = &x[x.len() / 4..x.len() * 3 / 4];
        let crossings = mid.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        crossings as f32 * SR / mid.len() as f32
    }

    /// Runs `src` through the stretcher block by block at `rate` and returns the output.
    fn run(src: &[f32], rate: f64, block: usize) -> Vec<f32> {
        let mut s = Stretcher::new(1, SR, block, Quality::Default).unwrap();
        let mut out = Vec::new();
        let mut pos = 0.0f64;
        while (pos as usize) + (block as f64 * rate) as usize + 1 < src.len() {
            let start = pos as usize;
            let next = pos + block as f64 * rate;
            let n_in = next as usize - start;
            let mut o = vec![vec![0.0; block]];
            s.process(&[&src[start..start + n_in]], n_in, &mut o, block, 1.0).unwrap();
            out.extend_from_slice(&o[0]);
            pos = next;
        }
        out
    }

    #[test]
    fn stretching_keeps_pitch() {
        let src = sine(440.0, SR as usize * 4);
        for rate in [0.8, 1.05, 1.25] {
            let out = run(&src, rate, 512);
            let expected_len = src.len() as f64 / rate;
            assert!((out.len() as f64 - expected_len).abs() < 2048.0, "rate {rate}: {} vs {expected_len}", out.len());
            let f = freq(&out);
            assert!((f - 440.0).abs() < 4.0, "rate {rate}: pitch drifted to {f} Hz");
        }
    }

    #[test]
    fn rate_can_change_every_block() {
        let src = sine(220.0, SR as usize * 4);
        let mut s = Stretcher::new(2, SR, 256, Quality::Cheaper).unwrap();
        let mut pos = 0usize;
        for k in 0..300 {
            let rate = 0.9 + 0.2 * (k as f64 / 300.0);
            let n_in = (256.0 * rate) as usize;
            let mut o = vec![vec![0.0; 256], vec![0.0; 256]];
            s.process(&[&src[pos..pos + n_in]], n_in, &mut o, 256, 1.0).unwrap();
            assert!(o.iter().flatten().all(|v| v.is_finite() && v.abs() < 2.0));
            pos += n_in;
        }
    }

    #[test]
    fn hostile_sizes_are_refused_not_panics() {
        assert!(Stretcher::new(0, SR, 512, Quality::Default).is_err());
        assert!(Stretcher::new(2, f32::NAN, 512, Quality::Default).is_err());
        assert!(Stretcher::new(2, SR, 0, Quality::Default).is_err());
        let mut s = Stretcher::new(2, SR, 64, Quality::Default).unwrap();
        let mut o = vec![vec![0.0; 64]; 2];
        let none: &[&[f32]] = &[];
        assert!(s.process(none, 10_000, &mut o, 64, 1.0).is_err());
        assert!(s.process(none, 64, &mut o, 65, 1.0).is_err());
        // Missing input channels read as silence; more output channels than the stretcher wraps.
        let mut o3 = vec![vec![0.0; 64]; 3];
        assert!(s.process(none, 64, &mut o3, 64, f32::NAN).is_ok());
        assert!(s.seek(&[&[0.0; 10]], 10, f64::INFINITY).is_ok());
    }
}
