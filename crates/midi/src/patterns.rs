//! Deterministic starter patterns for producing over a DJ set: drum layers, in-key basslines and
//! synths, and risers / downlifters / impacts.
//!
//! [`generate`] turns a [`PatternSpec`] into a [`Generated`] clip: a [`Sequence`] at [`PPQ`]
//! ticks plus any [`AutoCurve`]s (for FX sweeps). The clip is exactly `bars * 4` beats long and
//! the same spec (including the seed) always gives the same output; the seed only varies small
//! velocity details.
//!
//! Drums use General MIDI drum notes on channel 10 (index 9); everything else is on channel 1
//! (index 0). Harmony follows a four-chord loop, one chord per bar (per two bars when the clip
//! has 8 or more bars): i–VI–III–VII in minor keys and I–vi–IV–V in major keys. Bass octaves
//! follow [`crate::note_name`] (middle C = C3): bass roots sit in octave 1 (pitches 36..=47),
//! synths in octave 3.

use crate::scale::Key;
use crate::{Note, PPQ, Sequence, XorShift};

/// Longest clip [`generate`] will build, in bars.
pub const MAX_BARS: u32 = 1024;

/// GM kick (bass drum 1).
pub const KICK: u8 = 36;
/// GM side stick, used as a percussion layer.
pub const SIDE_STICK: u8 = 37;
/// GM acoustic snare.
pub const SNARE: u8 = 38;
/// GM hand clap.
pub const CLAP: u8 = 39;
/// GM closed hi-hat.
pub const CLOSED_HAT: u8 = 42;
/// GM open hi-hat.
pub const OPEN_HAT: u8 = 46;
/// GM claves, used as a percussion layer.
pub const CLAVES: u8 = 75;
/// Pitch of the single held note in FX patterns.
pub const FX_PITCH: u8 = 60;

/// MIDI channel index for drums (GM channel 10).
const DRUM_CH: u8 = 9;
/// MIDI channel index for bass, synth and FX.
const MELODIC_CH: u8 = 0;

const BEAT: i64 = PPQ as i64;
const SIXTEENTH: i64 = BEAT / 4;
const EIGHTH: i64 = BEAT / 2;
const BAR: i64 = BEAT * 4;

/// Bass octave (note-name octaves: 36..=47 holds the root).
const BASS_OCTAVE: i32 = 1;
/// Synth octave.
const SYNTH_OCTAVE: i32 = 3;
/// Chord-root scale degrees (0-based) of the minor loop i–VI–III–VII.
const MINOR_PROGRESSION: [i32; 4] = [0, 5, 2, 6];
/// Chord-root scale degrees (0-based) of the major loop I–vi–IV–V.
const MAJOR_PROGRESSION: [i32; 4] = [0, 5, 3, 4];

/// Errors from [`generate`] and the kind / style parsers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error("unknown pattern kind {0:?} (expected drums, bass, synth or fx)")]
    UnknownKind(String),
    #[error("unknown pattern style {0:?}")]
    UnknownStyle(String),
    #[error("style {style} is not a {kind} style")]
    StyleMismatch { style: &'static str, kind: &'static str },
    #[error("only 4 beats per bar are supported (got {0})")]
    UnsupportedMeter(u32),
    #[error("bars must be between 1 and {max} (got {bars})")]
    BadBars { bars: u32, max: u32 },
    #[error("style {0} needs a key")]
    KeyRequired(&'static str),
    #[error("scale degree {degree} in octave {octave} is outside the MIDI note range")]
    PitchOutOfRange { degree: i32, octave: i32 },
}

/// What a pattern is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternKind {
    Drums,
    Bass,
    Synth,
    Fx,
}

impl PatternKind {
    /// All kinds.
    pub const ALL: [PatternKind; 4] = [PatternKind::Drums, PatternKind::Bass, PatternKind::Synth, PatternKind::Fx];

    /// Lower-case name ("drums", "bass", "synth", "fx").
    pub fn name(self) -> &'static str {
        match self {
            PatternKind::Drums => "drums",
            PatternKind::Bass => "bass",
            PatternKind::Synth => "synth",
            PatternKind::Fx => "fx",
        }
    }

    /// Parses a name produced by [`PatternKind::name`] (case-insensitive).
    pub fn parse(text: &str) -> Result<Self, PatternError> {
        let t = text.trim();
        Self::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(t)).ok_or_else(|| PatternError::UnknownKind(text.to_string()))
    }
}

/// A concrete pattern style; each belongs to one [`PatternKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternStyle {
    /// Drums: a kick on every beat and nothing else.
    FourFloor,
    /// Drums: four-to-the-floor kick, open hats on the off-beats, clap on 2 and 4, 16th closed
    /// hats with accents.
    TranceFull,
    /// Drums: kick, off-beat closed hats and syncopated side-stick / claves.
    RollingPerc,
    /// Drums: the full trance groove plus a 16th snare roll with rising velocity in the last bar.
    Fill,
    /// Bass: 16ths on the key's root, skipping the 16th under each kick (trance gallop).
    Rolling,
    /// Bass: the key's root on the 8th off-beats.
    Offbeat,
    /// Bass: off-beat root / fifth following the chord loop.
    RootFifth,
    /// Synth: off-beat triad stabs following the chord loop.
    Stabs,
    /// Synth: 16th arpeggio up each chord (root, third, fifth, octave).
    Arp,
    /// FX: one held note with cutoff and volume sweeping up to the end.
    Riser,
    /// FX: one held note with cutoff and volume sweeping down from the start.
    Downlifter,
    /// FX: one beat-long hit at the start.
    Impact,
}

impl PatternStyle {
    /// All styles.
    pub const ALL: [PatternStyle; 12] = [
        PatternStyle::FourFloor,
        PatternStyle::TranceFull,
        PatternStyle::RollingPerc,
        PatternStyle::Fill,
        PatternStyle::Rolling,
        PatternStyle::Offbeat,
        PatternStyle::RootFifth,
        PatternStyle::Stabs,
        PatternStyle::Arp,
        PatternStyle::Riser,
        PatternStyle::Downlifter,
        PatternStyle::Impact,
    ];

    /// Snake-case name, e.g. "four_floor", "root_fifth".
    pub fn name(self) -> &'static str {
        match self {
            PatternStyle::FourFloor => "four_floor",
            PatternStyle::TranceFull => "trance_full",
            PatternStyle::RollingPerc => "rolling_perc",
            PatternStyle::Fill => "fill",
            PatternStyle::Rolling => "rolling",
            PatternStyle::Offbeat => "offbeat",
            PatternStyle::RootFifth => "root_fifth",
            PatternStyle::Stabs => "stabs",
            PatternStyle::Arp => "arp",
            PatternStyle::Riser => "riser",
            PatternStyle::Downlifter => "downlifter",
            PatternStyle::Impact => "impact",
        }
    }

    /// Parses a name produced by [`PatternStyle::name`] (case-insensitive).
    pub fn parse(text: &str) -> Result<Self, PatternError> {
        let t = text.trim();
        Self::ALL.into_iter().find(|s| s.name().eq_ignore_ascii_case(t)).ok_or_else(|| PatternError::UnknownStyle(text.to_string()))
    }

    /// The kind this style belongs to.
    pub fn kind(self) -> PatternKind {
        match self {
            PatternStyle::FourFloor | PatternStyle::TranceFull | PatternStyle::RollingPerc | PatternStyle::Fill => PatternKind::Drums,
            PatternStyle::Rolling | PatternStyle::Offbeat | PatternStyle::RootFifth => PatternKind::Bass,
            PatternStyle::Stabs | PatternStyle::Arp => PatternKind::Synth,
            PatternStyle::Riser | PatternStyle::Downlifter | PatternStyle::Impact => PatternKind::Fx,
        }
    }

    /// Whether the style needs a [`Key`] (bass and synth styles).
    pub fn needs_key(self) -> bool {
        matches!(self.kind(), PatternKind::Bass | PatternKind::Synth)
    }

    /// The styles of one kind.
    pub fn of_kind(kind: PatternKind) -> impl Iterator<Item = PatternStyle> {
        Self::ALL.into_iter().filter(move |s| s.kind() == kind)
    }
}

/// What to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternSpec {
    pub kind: PatternKind,
    pub style: PatternStyle,
    /// Clip length in bars, 1..=[`MAX_BARS`].
    pub bars: u32,
    /// Beats per bar; only 4 is supported.
    pub beats_per_bar: u32,
    /// Key for bass and synth styles (required there, ignored otherwise).
    pub key: Option<Key>,
    /// Seed for the small velocity variations.
    pub seed: u64,
}

/// An automated parameter of an FX pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutoParam {
    /// Filter cutoff, 0 = closed .. 1 = open.
    Cutoff,
    /// Volume, 0 = silent .. 1 = full.
    Volume,
}

impl AutoParam {
    /// Lower-case name ("cutoff", "volume").
    pub fn name(self) -> &'static str {
        match self {
            AutoParam::Cutoff => "cutoff",
            AutoParam::Volume => "volume",
        }
    }
}

/// An automation curve: `(tick, value 0..=1)` points in ascending tick order, linearly
/// interpolated between points.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoCurve {
    pub param: AutoParam,
    pub points: Vec<(i64, f32)>,
}

/// A generated clip.
#[derive(Debug, Clone, PartialEq)]
pub struct Generated {
    /// The notes (sorted).
    pub sequence: Sequence,
    /// Automation curves (FX styles only).
    pub automation: Vec<AutoCurve>,
    /// Clip length in ticks: `bars * 4 * PPQ`. Every note ends at or before this tick.
    pub length_ticks: i64,
}

/// Generates the pattern described by `spec`.
pub fn generate(spec: &PatternSpec) -> Result<Generated, PatternError> {
    let style = spec.style;
    if style.kind() != spec.kind {
        return Err(PatternError::StyleMismatch { style: style.name(), kind: spec.kind.name() });
    }
    if spec.beats_per_bar != 4 {
        return Err(PatternError::UnsupportedMeter(spec.beats_per_bar));
    }
    if spec.bars == 0 || spec.bars > MAX_BARS {
        return Err(PatternError::BadBars { bars: spec.bars, max: MAX_BARS });
    }
    let key = match (style.needs_key(), spec.key) {
        (true, None) => return Err(PatternError::KeyRequired(style.name())),
        (_, k) => k,
    };
    let bars = i64::from(spec.bars);
    let mut g = Gen { notes: Vec::new(), rng: XorShift::new(spec.seed), bars };
    let mut automation = Vec::new();
    let length = bars * BAR;
    match (style, key) {
        (PatternStyle::FourFloor, _) => g.kicks(),
        (PatternStyle::TranceFull, _) => g.trance_groove(),
        (PatternStyle::RollingPerc, _) => g.rolling_perc(),
        (PatternStyle::Fill, _) => {
            g.trance_groove();
            g.snare_roll();
        }
        (PatternStyle::Rolling, Some(k)) => g.rolling_bass(k)?,
        (PatternStyle::Offbeat, Some(k)) => g.offbeat_bass(k)?,
        (PatternStyle::RootFifth, Some(k)) => g.root_fifth(k)?,
        (PatternStyle::Stabs, Some(k)) => g.stabs(k)?,
        (PatternStyle::Arp, Some(k)) => g.arp(k)?,
        (PatternStyle::Riser, _) => {
            g.push(MELODIC_CH, FX_PITCH, 0, length, 100);
            automation = sweep(length, false);
        }
        (PatternStyle::Downlifter, _) => {
            g.push(MELODIC_CH, FX_PITCH, 0, length, 100);
            automation = sweep(length, true);
        }
        (PatternStyle::Impact, _) => g.push(MELODIC_CH, FX_PITCH, 0, BEAT, 127),
        // Keyed styles without a key were rejected above.
        (_, None) => return Err(PatternError::KeyRequired(style.name())),
    }
    let mut sequence = Sequence { notes: g.notes, ctrls: Vec::new() };
    sequence.sort();
    Ok(Generated { sequence, automation, length_ticks: length })
}

/// Note builder shared by the styles.
struct Gen {
    notes: Vec<Note>,
    rng: XorShift,
    bars: i64,
}

impl Gen {
    fn push(&mut self, channel: u8, pitch: u8, start: i64, length: i64, velocity: u8) {
        self.notes.push(Note { pitch, velocity: velocity.clamp(1, 127), release_velocity: 64, channel, start, length: length.max(1) });
    }

    /// `base` nudged by up to ±`spread` from the seeded generator.
    fn vary(&mut self, base: u8, spread: f64) -> u8 {
        let v = f64::from(base) + self.rng.next_signed() * spread;
        v.round().clamp(1.0, 127.0) as u8
    }

    fn kicks(&mut self) {
        for beat in 0..self.bars * 4 {
            self.push(DRUM_CH, KICK, beat * BEAT, SIXTEENTH, 120);
        }
    }

    fn trance_groove(&mut self) {
        self.kicks();
        for t in sixteenths(self.bars) {
            match (t % BEAT) / SIXTEENTH {
                // Off-beat 8th: open hat instead of a closed one.
                2 => {
                    let v = self.vary(92, 4.0);
                    self.push(DRUM_CH, OPEN_HAT, t, SIXTEENTH, v);
                }
                // Accent the 16th just before the next beat.
                step => {
                    let base = if step == 3 { 96 } else { 68 };
                    let v = self.vary(base, 6.0);
                    self.push(DRUM_CH, CLOSED_HAT, t, SIXTEENTH / 2, v);
                }
            }
            // Clap on beats 2 and 4.
            if t % (2 * BEAT) == BEAT {
                self.push(DRUM_CH, CLAP, t, SIXTEENTH, 110);
            }
        }
    }

    fn rolling_perc(&mut self) {
        self.kicks();
        for bar in 0..self.bars {
            let t0 = bar * BAR;
            for beat in 0..4 {
                let v = self.vary(84, 5.0);
                self.push(DRUM_CH, CLOSED_HAT, t0 + beat * BEAT + EIGHTH, SIXTEENTH / 2, v);
            }
            // Two alternating one-bar variations of syncopated side-stick and claves 16th steps.
            let (sticks, claves): (&[i64], &[i64]) = if bar % 2 == 0 { (&[3, 6, 10, 13], &[7, 15]) } else { (&[3, 6, 11, 14], &[7, 12, 15]) };
            for &s in sticks {
                let v = self.vary(80, 8.0);
                self.push(DRUM_CH, SIDE_STICK, t0 + s * SIXTEENTH, SIXTEENTH, v);
            }
            for &s in claves {
                let v = self.vary(70, 8.0);
                self.push(DRUM_CH, CLAVES, t0 + s * SIXTEENTH, SIXTEENTH, v);
            }
        }
    }

    /// 16 snares over the last bar, velocity rising from 40 to 127.
    fn snare_roll(&mut self) {
        let t0 = (self.bars - 1) * BAR;
        for i in 0..16i64 {
            let v = 40 + (87 * i) / 15;
            self.push(DRUM_CH, SNARE, t0 + i * SIXTEENTH, SIXTEENTH, u8::try_from(v).unwrap_or(127));
        }
    }

    /// Scale degree of the chord root under `tick`.
    fn chord_root(&self, key: Key, tick: i64) -> i32 {
        let bar = tick / BAR;
        let index = if self.bars >= 8 { bar / 2 } else { bar };
        let prog = if key.minor { &MINOR_PROGRESSION } else { &MAJOR_PROGRESSION };
        usize::try_from(index.rem_euclid(4)).ok().and_then(|i| prog.get(i).copied()).unwrap_or(0)
    }

    /// The MIDI note of scale `degree` of `key` in `octave`, or an error outside the MIDI range.
    fn pitch(key: Key, degree: i32, octave: i32) -> Result<u8, PatternError> {
        key.degree(degree, octave).ok_or(PatternError::PitchOutOfRange { degree, octave })
    }

    fn rolling_bass(&mut self, key: Key) -> Result<(), PatternError> {
        let root = Self::pitch(key, 0, BASS_OCTAVE)?;
        for t in sixteenths(self.bars) {
            let step = (t % BEAT) / SIXTEENTH;
            // The kick owns the beat; the bass fills the other three 16ths.
            if step == 0 {
                continue;
            }
            let v = self.vary(if step == 2 { 104 } else { 92 }, 4.0);
            self.push(MELODIC_CH, root, t, SIXTEENTH - 40, v);
        }
        Ok(())
    }

    fn offbeat_bass(&mut self, key: Key) -> Result<(), PatternError> {
        let root = Self::pitch(key, 0, BASS_OCTAVE)?;
        for beat in 0..self.bars * 4 {
            let v = self.vary(104, 4.0);
            self.push(MELODIC_CH, root, beat * BEAT + EIGHTH, EIGHTH - 120, v);
        }
        Ok(())
    }

    fn root_fifth(&mut self, key: Key) -> Result<(), PatternError> {
        for beat in 0..self.bars * 4 {
            let t = beat * BEAT + EIGHTH;
            let root = self.chord_root(key, t);
            // Root on beats 1 and 3, the chord's (diatonic) fifth on 2 and 4.
            let degree = if beat % 2 == 0 { root } else { root + 4 };
            let v = self.vary(104, 4.0);
            self.push(MELODIC_CH, Self::pitch(key, degree, BASS_OCTAVE)?, t, EIGHTH - 120, v);
        }
        Ok(())
    }

    fn stabs(&mut self, key: Key) -> Result<(), PatternError> {
        for beat in 0..self.bars * 4 {
            let t = beat * BEAT + EIGHTH;
            let root = self.chord_root(key, t);
            let v = self.vary(100, 5.0);
            for d in [root, root + 2, root + 4] {
                self.push(MELODIC_CH, Self::pitch(key, d, SYNTH_OCTAVE)?, t, SIXTEENTH, v);
            }
        }
        Ok(())
    }

    fn arp(&mut self, key: Key) -> Result<(), PatternError> {
        for t in sixteenths(self.bars) {
            let root = self.chord_root(key, t);
            let step = (t % BEAT) / SIXTEENTH;
            let degree = match step {
                0 => root,
                1 => root + 2,
                2 => root + 4,
                _ => root + 7,
            };
            let v = self.vary(if step == 0 { 100 } else { 86 }, 5.0);
            self.push(MELODIC_CH, Self::pitch(key, degree, SYNTH_OCTAVE)?, t, SIXTEENTH - 20, v);
        }
        Ok(())
    }
}

/// Start ticks of every 16th in a clip of `bars` bars.
fn sixteenths(bars: i64) -> impl Iterator<Item = i64> {
    (0..bars * 16).map(|i| i * SIXTEENTH)
}

/// Exponential-ish 0..1 shape: slow start, fast finish.
fn ease_in(x: f64) -> f64 {
    const K: f64 = 3.0;
    ((K * x.clamp(0.0, 1.0)).exp() - 1.0) / (K.exp() - 1.0)
}

/// Cutoff (0.05..1) and volume (0.3..1) curves over `[0, length]`, one point per 16th plus the
/// end. A riser rises to 1.0 at `length`; a downlifter (`reverse`) is its mirror image, starting
/// at 1.0 on tick 0.
fn sweep(length: i64, reverse: bool) -> Vec<AutoCurve> {
    let curve = |param: AutoParam, low: f64| {
        let mut ticks: Vec<i64> = (0..length).step_by(SIXTEENTH as usize).collect();
        ticks.push(length);
        let points = ticks
            .into_iter()
            .map(|t| {
                let x = t as f64 / length.max(1) as f64;
                let shape = if reverse { ease_in(1.0 - x) } else { ease_in(x) };
                // Pin the ends exactly so a riser really finishes on 1.0.
                let value = if shape >= 1.0 { 1.0 } else { low + (1.0 - low) * shape };
                (t, value as f32)
            })
            .collect();
        AutoCurve { param, points }
    };
    vec![curve(AutoParam::Cutoff, 0.05), curve(AutoParam::Volume, 0.3)]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_scale_degree_outside_the_midi_range_is_an_error() {
        let am = Key::parse("Am").unwrap();
        assert!(Gen::pitch(am, 0, BASS_OCTAVE).is_ok());
        assert_eq!(Gen::pitch(am, 0, 20), Err(PatternError::PitchOutOfRange { degree: 0, octave: 20 }));
        assert_eq!(Gen::pitch(am, -100, 0), Err(PatternError::PitchOutOfRange { degree: -100, octave: 0 }));
    }
}
