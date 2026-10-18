//! Beat-locked playback: a clip whose source follows the session tempo map beat for beat.

use soundcraft_time::{SampleRate, Samples, TICKS_PER_QUARTER, TempoMap};

/// Most beats a warp map may hold (a 30-minute track at 200 bpm has 6 000).
pub const MAX_WARP_BEATS: usize = 20_000;

/// Where each beat of an audio clip's source falls, so playback can follow the session tempo.
///
/// `beats[i]` is the source position (in source-pool samples, i.e. at the session rate) of
/// quarter-note beat `i` of the track. The clip's start plays fractional beat `anchor`; from there
/// one session beat (as the tempo map counts them) advances one entry. Positions between beats are
/// interpolated linearly; before the first or after the last beat the edge interval repeats.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WarpMap {
    pub beats: Vec<f64>,
    #[serde(default)]
    pub anchor: f64,
}

impl WarpMap {
    /// A map from analysed beat times. `None` unless there are at least two strictly increasing,
    /// finite positions (at most [`MAX_WARP_BEATS`] are kept).
    pub fn new(beats: Vec<f64>, anchor: f64) -> Option<WarpMap> {
        let mut w = WarpMap { beats, anchor };
        w.sanitize();
        (w.beats.len() >= 2).then_some(w)
    }

    /// Keeps only a usable, strictly increasing prefix of finite beats and a finite anchor.
    pub fn sanitize(&mut self) {
        let mut last = f64::NEG_INFINITY;
        let mut keep = 0;
        for &b in self.beats.iter().take(MAX_WARP_BEATS) {
            if !b.is_finite() || b <= last {
                break;
            }
            last = b;
            keep += 1;
        }
        self.beats.truncate(keep);
        if !self.anchor.is_finite() {
            self.anchor = 0.0;
        }
    }

    /// True when the map can drive playback.
    pub fn is_usable(&self) -> bool {
        self.beats.len() >= 2
    }

    /// Source position (samples, fractional) of fractional track beat `beat`.
    pub fn source_at(&self, beat: f64) -> f64 {
        let n = self.beats.len();
        let (Some(&first), Some(&second), Some(&last)) = (self.beats.first(), self.beats.get(1), self.beats.last()) else {
            return 0.0;
        };
        if !beat.is_finite() {
            return first;
        }
        if beat <= 0.0 {
            return first + beat * (second - first);
        }
        let i = beat.floor();
        let idx = i as usize;
        if idx + 1 >= n {
            let prev = self.beats.get(n.saturating_sub(2)).copied().unwrap_or(first);
            return last + (beat - (n - 1) as f64) * (last - prev);
        }
        let a = self.beats.get(idx).copied().unwrap_or(first);
        let b = self.beats.get(idx + 1).copied().unwrap_or(last);
        a + (beat - i) * (b - a)
    }

    /// Fractional track beat at source position `pos` (samples): the inverse of [`WarpMap::source_at`].
    pub fn beat_at_source(&self, pos: f64) -> f64 {
        let n = self.beats.len();
        let (Some(&first), Some(&second), Some(&last)) = (self.beats.first(), self.beats.get(1), self.beats.last()) else {
            return 0.0;
        };
        if !pos.is_finite() {
            return 0.0;
        }
        if pos <= first {
            return (pos - first) / (second - first);
        }
        if pos >= last {
            let prev = self.beats.get(n.saturating_sub(2)).copied().unwrap_or(first);
            return (n - 1) as f64 + (pos - last) / (last - prev);
        }
        // First beat after `pos`; 1..n-1 here because first < pos < last.
        let i = self.beats.partition_point(|&b| b <= pos).max(1);
        let a = self.beats.get(i - 1).copied().unwrap_or(first);
        let b = self.beats.get(i).copied().unwrap_or(last);
        (i - 1) as f64 + (pos - a) / (b - a)
    }

    /// Track beat heard at timeline sample `t` when the clip starts at `clip_start`: the clip start
    /// plays [`WarpMap::anchor`] and each session beat after it advances one track beat.
    pub fn track_beat_at(&self, tempo: &TempoMap, sr: SampleRate, clip_start: Samples, t: Samples) -> f64 {
        let beat = |x: Samples| tempo.samples_to_ticks_f(x, sr) / TICKS_PER_QUARTER as f64;
        self.anchor + beat(t) - beat(clip_start)
    }

    /// Source position (samples) heard at timeline sample `t`.
    pub fn source_at_time(&self, tempo: &TempoMap, sr: SampleRate, clip_start: Samples, t: Samples) -> f64 {
        self.source_at(self.track_beat_at(tempo, sr, clip_start, t))
    }

    /// Number of beats in the map.
    pub fn len(&self) -> usize {
        self.beats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.beats.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_and_extrapolates() {
        let w = WarpMap::new(vec![100.0, 200.0, 400.0], 0.0).unwrap();
        assert_eq!(w.source_at(0.0), 100.0);
        assert_eq!(w.source_at(0.5), 150.0);
        assert_eq!(w.source_at(1.5), 300.0);
        assert_eq!(w.source_at(3.0), 600.0);
        assert_eq!(w.source_at(-1.0), 0.0);
        assert_eq!(w.source_at(f64::NAN), 100.0);
    }

    #[test]
    fn hostile_maps_are_cut_to_a_usable_prefix() {
        assert!(WarpMap::new(vec![1.0], 0.0).is_none());
        assert!(WarpMap::new(vec![f64::NAN, 2.0], 0.0).is_none());
        let w = WarpMap::new(vec![1.0, 2.0, 2.0, 3.0], f64::INFINITY).unwrap();
        assert_eq!((w.beats.len(), w.anchor), (2, 0.0));
    }

    #[test]
    fn beat_at_source_inverts_source_at() {
        let w = WarpMap::new(vec![100.0, 200.0, 400.0, 450.0], 0.0).unwrap();
        for b in [-2.0, -0.25, 0.0, 0.5, 1.0, 1.75, 2.5, 3.0, 4.5] {
            let back = w.beat_at_source(w.source_at(b));
            assert!((back - b).abs() < 1e-9, "beat {b} came back as {back}");
        }
        assert_eq!(w.beat_at_source(f64::NAN), 0.0);
    }

    #[test]
    fn track_beat_follows_the_tempo_map() {
        use soundcraft_time::{SampleRate, TempoMap};
        let sr = SampleRate::default();
        let tempo = TempoMap::new(120.0, 4, 4).unwrap();
        let w = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], -1.0).unwrap();
        // 120 bpm at 48 kHz: one beat every 24 000 samples. One beat after the clip start the
        // anchor (-1) has advanced to track beat 0.
        let at = 48_000;
        assert!((w.track_beat_at(&tempo, sr, at, at + 24_000) - 0.0).abs() < 1e-9);
        assert!((w.source_at_time(&tempo, sr, at, at + 36_000) - 12_000.0).abs() < 1e-6);
    }
}
