//! Musical keys: parsing Camelot / Open Key / note-name keys and mapping scale degrees to pitches.
//!
//! A [`Key`] is a tonic pitch class plus major / natural-minor mode. Octaves follow
//! [`crate::note_name`]: middle C (60) is "C3", so a key's tonic in octave `o` is
//! `(o + 2) * 12 + tonic`.

use std::fmt;

/// Pitch-class spellings used by [`Key::name`] (Mixed In Key style: flats except F#).
const NAMES: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

/// Semitone offsets of the major scale.
const MAJOR: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
/// Semitone offsets of the natural minor scale.
const MINOR: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];

/// Error from [`Key::parse`]: the text is not a recognised key. Keys are never guessed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unrecognised key {0:?} (expected Camelot like \"8A\", Open Key like \"1m\", or a name like \"Am\" / \"F# minor\")")]
pub struct KeyParseError(pub String);

/// A musical key: tonic pitch class and mode (major or natural minor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// Tonic pitch class, 0 = C .. 11 = B.
    pub tonic: u8,
    /// Natural minor when set, otherwise major.
    pub minor: bool,
}

impl Key {
    /// Creates a key; the tonic is reduced modulo 12.
    pub fn new(tonic: u8, minor: bool) -> Self {
        Self { tonic: tonic % 12, minor }
    }

    /// Parses Camelot ("8A", "4B"), Open Key ("1m", "1d") or a note name ("Am", "A minor",
    /// "F#m", "Gb", "C major", "Bbmin"). Whitespace around the text is ignored; anything else
    /// unrecognised is an error.
    pub fn parse(text: &str) -> Result<Self, KeyParseError> {
        let s = text.trim();
        let err = || KeyParseError(text.to_string());
        if s.starts_with(|c: char| c.is_ascii_digit()) {
            let digits = s.chars().take_while(char::is_ascii_digit).count();
            let (num, rest) = s.split_at(digits);
            let n: u8 = num.parse().map_err(|_| err())?;
            if !(1..=12).contains(&n) {
                return Err(err());
            }
            return match rest {
                "A" | "a" => Ok(Self::from_camelot(n, true)),
                "B" | "b" => Ok(Self::from_camelot(n, false)),
                // Open Key n is Camelot n + 7 (1m = Am = 8A, 1d = C = 8B).
                "m" | "M" => Ok(Self::from_camelot((n + 6) % 12 + 1, true)),
                "d" | "D" => Ok(Self::from_camelot((n + 6) % 12 + 1, false)),
                _ => Err(err()),
            };
        }
        let mut chars = s.chars();
        let natural = match chars.next().map(|c| c.to_ascii_uppercase()) {
            Some('C') => 0u8,
            Some('D') => 2,
            Some('E') => 4,
            Some('F') => 5,
            Some('G') => 7,
            Some('A') => 9,
            Some('B') => 11,
            _ => return Err(err()),
        };
        let mut rest = chars.as_str();
        let mut tonic = natural;
        if let Some(r) = rest.strip_prefix('#').or_else(|| rest.strip_prefix('♯')) {
            tonic = (natural + 1) % 12;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('b').or_else(|| rest.strip_prefix('♭')) {
            tonic = (natural + 11) % 12;
            rest = r;
        }
        let minor = match rest.trim() {
            "" => false,
            "m" => true,
            other => match other.to_ascii_lowercase().as_str() {
                "maj" | "major" => false,
                "min" | "minor" => true,
                // A bare "M" is ambiguous (major in some tools, minor in others): refuse it.
                _ => return Err(err()),
            },
        };
        Ok(Self { tonic, minor })
    }

    /// The key for Camelot number `n` (1..=12, wrapped) on the minor ("A") or major ("B") wheel.
    fn from_camelot(n: u8, minor: bool) -> Self {
        // Camelot steps by fifths: 1A = Ab minor, 1B = B major.
        let step = (u32::from(n.clamp(1, 12)) - 1) * 7;
        let base = if minor { 8 } else { 11 };
        Self { tonic: ((base + step) % 12) as u8, minor }
    }

    /// The Camelot code, e.g. "8A" for A minor, "8B" for C major.
    pub fn camelot(&self) -> String {
        let base: u32 = if self.minor { 8 } else { 11 };
        // Inverse of `from_camelot`: 7 is its own inverse modulo 12.
        let n = ((u32::from(self.tonic % 12) + 12 - base) * 7) % 12 + 1;
        format!("{n}{}", if self.minor { 'A' } else { 'B' })
    }

    /// Short name, e.g. "Am", "F#m", "Eb" (major keys have no suffix).
    pub fn name(&self) -> String {
        let pc = NAMES.get(usize::from(self.tonic % 12)).copied().unwrap_or("?");
        if self.minor { format!("{pc}m") } else { pc.to_string() }
    }

    fn intervals(&self) -> &'static [u8; 7] {
        if self.minor { &MINOR } else { &MAJOR }
    }

    /// The seven pitch classes of the scale (natural minor or major), starting at the tonic.
    pub fn scale_pitches(&self) -> [u8; 7] {
        self.intervals().map(|i| (self.tonic % 12 + i) % 12)
    }

    /// Whether the MIDI pitch belongs to the scale.
    pub fn contains(&self, pitch: u8) -> bool {
        self.scale_pitches().contains(&(pitch % 12))
    }

    /// MIDI pitch of scale degree `n` (0 = tonic; 7 = tonic an octave up; negative goes down)
    /// counted from the tonic in `octave` (note-name octaves: middle C is octave 3). `None`
    /// when the result is outside 0..=127.
    pub fn degree(&self, n: i32, octave: i32) -> Option<u8> {
        let step = n.rem_euclid(7);
        let octave_shift = n.div_euclid(7);
        let interval = self.intervals().get(usize::try_from(step).ok()?).copied()?;
        let pitch = (i64::from(octave) + 2 + i64::from(octave_shift)) * 12 + i64::from(self.tonic % 12) + i64::from(interval);
        u8::try_from(pitch).ok().filter(|p| *p <= 127)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camelot_round_trips_all_24() {
        for n in 1..=12u8 {
            for side in ['A', 'B'] {
                let code = format!("{n}{side}");
                let key = Key::parse(&code).unwrap();
                assert_eq!(key.camelot(), code);
                assert_eq!(key.minor, side == 'A');
                // Name and Camelot agree.
                assert_eq!(Key::parse(&key.name()).unwrap(), key, "{code} -> {}", key.name());
            }
        }
    }

    #[test]
    fn known_keys() {
        let am = Key { tonic: 9, minor: true };
        for s in ["8A", "8a", "Am", "A minor", "a min", "1m", " Am "] {
            assert_eq!(Key::parse(s).unwrap(), am, "{s}");
        }
        assert_eq!(Key::parse("C major").unwrap(), Key { tonic: 0, minor: false });
        assert_eq!(Key::parse("8B").unwrap(), Key { tonic: 0, minor: false });
        assert_eq!(Key::parse("1d").unwrap(), Key { tonic: 0, minor: false });
        assert_eq!(Key::parse("F#m").unwrap(), Key { tonic: 6, minor: true });
        assert_eq!(Key::parse("F#m").unwrap().camelot(), "11A");
        assert_eq!(Key::parse("Gb").unwrap(), Key { tonic: 6, minor: false });
        assert_eq!(Key::parse("4B").unwrap(), Key { tonic: 8, minor: false });
        assert_eq!(Key::parse("Bbm").unwrap(), Key { tonic: 10, minor: true });
        assert_eq!(Key::parse("12d").unwrap(), Key::parse("7B").unwrap());
    }

    #[test]
    fn rejects_garbage() {
        for s in ["", "  ", "0A", "13A", "8C", "8", "H", "Am7", "AM", "A flat", "8AA", "x", "99999999999999A", "1mm", "#"] {
            assert!(Key::parse(s).is_err(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn scale_and_degrees() {
        let am = Key::parse("Am").unwrap();
        assert_eq!(am.scale_pitches(), [9, 11, 0, 2, 4, 5, 7]);
        assert!(am.contains(60) && !am.contains(61));
        // A2 is 57 in note-name octaves (middle C = C3).
        assert_eq!(am.degree(0, 2), Some(57));
        assert_eq!(am.degree(2, 2), Some(60));
        assert_eq!(am.degree(7, 2), Some(69));
        assert_eq!(am.degree(-1, 2), Some(55));
        assert_eq!(am.degree(0, 9), None);
        assert_eq!(am.degree(0, -3), None);
    }
}
