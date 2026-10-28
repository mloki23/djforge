//! The Session view: Live columns of clip slots launched over a playing set.
//!
//! What a slot holds is part of the document (saved, undoable to edit); what is playing is not.

use crate::{MAX_NAME_CHARS, MAX_WARP_BEATS, STEMS, Source, SourceId, StemSet, TrackId, WarpMap, truncate_chars};
use serde::Deserialize;
use serde::de::Deserializer;

/// Slots in each Live column.
pub const LIVE_SLOTS: usize = 8;
/// Live columns `live.setup` makes by default.
pub const LIVE_COLUMNS: usize = 4;
/// Most Live columns a session keeps.
pub const MAX_LIVE_COLUMNS: usize = 16;
/// Longest loop a slot may hold, in beats (256 bars of 4/4).
pub const MAX_SLOT_BEATS: f64 = 1024.0;
/// Quietest and loudest slot gain, in dB.
pub const SLOT_GAIN_DB: (f32, f32) = (-144.0, 24.0);

/// How far ahead a launch waits: for the next boundary of this many beats or bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Quantise {
    /// At once.
    None,
    /// The next quarter note.
    Quarter,
    /// The next half note.
    Half,
    /// The next bar line of every `n` bars (1, 2, 4 or 8).
    Bar(u8),
}

/// One bar.
impl Default for Quantise {
    fn default() -> Self {
        Quantise::Bar(1)
    }
}

impl Quantise {
    /// The bar counts a [`Quantise::Bar`] may hold.
    pub const BARS: [u8; 4] = [1, 2, 4, 8];

    /// Every choice, in menu order.
    pub const ALL: [Quantise; 7] =
        [Quantise::None, Quantise::Quarter, Quantise::Half, Quantise::Bar(1), Quantise::Bar(2), Quantise::Bar(4), Quantise::Bar(8)];

    /// The same choice with a bar count it may not hold (and the default) as 1 bar.
    pub fn normalised(self) -> Quantise {
        match self {
            Quantise::Bar(n) if Self::BARS.contains(&n) => self,
            Quantise::Bar(_) => Quantise::Bar(1),
            q => q,
        }
    }

    /// The id commands and menus use: `none`, `1/4`, `1/2`, `1`, `2`, `4` or `8` (bars).
    pub fn id(self) -> String {
        match self.normalised() {
            Quantise::None => "none".into(),
            Quantise::Quarter => "1/4".into(),
            Quantise::Half => "1/2".into(),
            Quantise::Bar(n) => n.to_string(),
        }
    }

    /// The choice for an id from [`Quantise::id`] (`1 bar`, `2 bars` too).
    pub fn from_id(s: &str) -> Option<Quantise> {
        let s = s.trim().to_ascii_lowercase();
        let s = s.trim_end_matches("bars").trim_end_matches("bar").trim();
        Quantise::ALL.into_iter().find(|q| q.id() == s)
    }
}

/// How a slot plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotMode {
    /// Loops its beats, locked to the session tempo.
    #[default]
    Loop,
    /// Plays once, at its own rate.
    OneShot,
}

impl SlotMode {
    pub fn id(self) -> &'static str {
        match self {
            SlotMode::Loop => "loop",
            SlotMode::OneShot => "one_shot",
        }
    }

    pub fn from_id(s: &str) -> Option<SlotMode> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "loop" => Some(SlotMode::Loop),
            "one_shot" | "oneshot" => Some(SlotMode::OneShot),
            _ => None,
        }
    }
}

/// A clip slot's material: a stretch of a source, beat-locked through its warp map (or a
/// one-shot without one).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Slot {
    pub name: String,
    pub color: [u8; 3],
    pub source: SourceId,
    /// Where the source's beats fall; `None` for a one-shot that plays at its own rate. Its lock
    /// is not used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warp: Option<WarpMap>,
    /// First beat of the loop, in the source's own track beats.
    #[serde(default)]
    pub start_beat: f64,
    /// Beats the loop lasts (at least 1).
    #[serde(default = "one_beat")]
    pub length_beats: f64,
    /// The source's stems, played instead of it, when it was split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stems: Option<StemSet>,
    /// Which stems play, in [`STEMS`] order.
    #[serde(default = "all_stems")]
    pub stem_mask: [bool; 4],
    #[serde(default)]
    pub mode: SlotMode,
    #[serde(default)]
    pub gain_db: f32,
}

fn one_beat() -> f64 {
    1.0
}

fn all_stems() -> [bool; STEMS.len()] {
    [true; STEMS.len()]
}

impl Slot {
    /// A one-shot of the whole of `source`.
    pub fn one_shot(name: impl Into<String>, color: [u8; 3], source: SourceId) -> Slot {
        Slot {
            name: name.into(),
            color,
            source,
            warp: None,
            start_beat: 0.0,
            length_beats: 1.0,
            stems: None,
            stem_mask: all_stems(),
            mode: SlotMode::OneShot,
            gain_db: 0.0,
        }
    }

    /// Every audio source the slot plays or depends on: its source, then its stems (if split).
    pub fn audio_sources(&self) -> impl Iterator<Item = SourceId> + '_ {
        std::iter::once(self.source).chain(self.stems.iter().flat_map(|set| set.sources))
    }

    /// Forget the slot's stems: it plays its own audio again, every stem on.
    pub fn unsplit(&mut self) {
        self.stems = None;
        self.stem_mask = all_stems();
    }

    /// Repair invariants after loading untrusted data. A map that can't drive playback is dropped
    /// (the slot becomes a one-shot), and so is its lock, which slots don't use.
    pub fn sanitize(&mut self) {
        truncate_chars(&mut self.name, MAX_NAME_CHARS);
        if let Some(w) = &mut self.warp {
            w.sanitize();
            w.lock = None;
        }
        if !self.warp.as_ref().is_some_and(WarpMap::is_usable) {
            self.warp = None;
            self.mode = SlotMode::OneShot;
        }
        let max_start = MAX_WARP_BEATS as f64;
        self.start_beat = if self.start_beat.is_finite() { self.start_beat.clamp(-max_start, max_start) } else { 0.0 };
        self.length_beats = if self.length_beats.is_finite() { self.length_beats.clamp(1.0, MAX_SLOT_BEATS) } else { 1.0 };
        self.gain_db = if self.gain_db.is_finite() { self.gain_db.clamp(SLOT_GAIN_DB.0, SLOT_GAIN_DB.1) } else { 0.0 };
        if let Some(set) = &mut self.stems {
            truncate_chars(&mut set.key, MAX_NAME_CHARS);
        } else {
            self.unsplit();
        }
    }
}

/// A Live column: an audio track ("Live 1"…) whose input the column's slots play into.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveColumn {
    pub track: TrackId,
    /// Reading is lenient: a malformed slot reads as empty, and the list is cut or padded to
    /// [`LIVE_SLOTS`].
    #[serde(deserialize_with = "loadable_slots")]
    pub slots: [Option<Slot>; LIVE_SLOTS],
}

impl LiveColumn {
    pub fn new(track: TrackId) -> LiveColumn {
        LiveColumn { track, slots: Default::default() }
    }

    /// The first empty slot.
    pub fn next_empty(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }
}

/// The slots that read; a malformed one is empty.
fn loadable_slots<'de, D: Deserializer<'de>>(d: D) -> Result<[Option<Slot>; LIVE_SLOTS], D::Error> {
    let list = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(a) => a,
        _ => Vec::new(),
    };
    let mut slots: [Option<Slot>; LIVE_SLOTS] = Default::default();
    for (slot, v) in slots.iter_mut().zip(list) {
        *slot = serde_json::from_value(v).ok().flatten();
    }
    Ok(slots)
}

/// The Session view's saved state: its columns and the launch quantise.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveState {
    /// A malformed column is dropped rather than failing the session load.
    #[serde(default, deserialize_with = "loadable_columns")]
    pub columns: Vec<LiveColumn>,
    #[serde(default, deserialize_with = "lenient_quantise")]
    pub quantise: Quantise,
}

fn loadable_columns<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<LiveColumn>, D::Error> {
    let list = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(a) => a,
        _ => Vec::new(),
    };
    Ok(list.into_iter().filter_map(|c| serde_json::from_value(c).ok()).collect())
}

fn lenient_quantise<'de, D: Deserializer<'de>>(d: D) -> Result<Quantise, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value::<Quantise>(v).unwrap_or_default().normalised())
}

impl LiveState {
    /// No columns and the default quantise: nothing to save.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.quantise.normalised() == Quantise::default()
    }

    /// The slot at `column`, `slot`, if both exist and it is filled.
    pub fn slot(&self, column: usize, slot: usize) -> Option<&Slot> {
        self.columns.get(column).and_then(|c| c.slots.get(slot)).and_then(Option::as_ref)
    }

    /// Every filled slot, with its column and row.
    pub fn slots(&self) -> impl Iterator<Item = (usize, usize, &Slot)> {
        self.columns.iter().enumerate().flat_map(|(c, col)| col.slots.iter().enumerate().filter_map(move |(r, s)| s.as_ref().map(|s| (c, r, s))))
    }

    /// Repair invariants after loading untrusted data: a column whose track is gone (or that
    /// repeats one) is dropped, at most [`MAX_LIVE_COLUMNS`] are kept, a slot whose source is
    /// missing is emptied, stems naming a missing source are dropped, and each slot is bounded.
    pub fn sanitize(&mut self, has_track: impl Fn(TrackId) -> bool, sources: &[Source]) {
        let known = |id: SourceId| sources.iter().any(|s| s.id == id);
        let mut seen = std::collections::BTreeSet::new();
        self.columns.retain(|c| has_track(c.track) && seen.insert(c.track));
        self.columns.truncate(MAX_LIVE_COLUMNS);
        for slot in self.columns.iter_mut().flat_map(|c| c.slots.iter_mut()) {
            if slot.as_ref().is_some_and(|s| !known(s.source)) {
                *slot = None;
            }
            if let Some(s) = slot {
                if s.stems.as_ref().is_some_and(|set| set.sources.iter().any(|id| !known(*id))) {
                    s.unsplit();
                }
                s.sanitize();
            }
        }
        self.quantise = self.quantise.normalised();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn quantise_ids_round_trip_and_odd_bar_counts_read_as_one_bar() {
        for q in Quantise::ALL {
            assert_eq!(Quantise::from_id(&q.id()), Some(q));
        }
        assert_eq!(Quantise::from_id("2 bars"), Some(Quantise::Bar(2)));
        assert_eq!(Quantise::from_id("3"), None);
        assert_eq!(Quantise::Bar(3).normalised(), Quantise::Bar(1));
        assert_eq!(Quantise::default(), Quantise::Bar(1));
    }

    #[test]
    fn a_hostile_column_reads_with_eight_slots() {
        let c: LiveColumn = serde_json::from_str(r#"{"track": 3, "slots": [null, 7, {"name": "x"}]}"#).unwrap();
        assert_eq!((c.track, c.slots.iter().filter(|s| s.is_some()).count()), (TrackId(3), 0));
        let c: LiveColumn = serde_json::from_str(&format!(r#"{{"track": 3, "slots": [{}]}}"#, vec!["null"; 20].join(","))).unwrap();
        assert_eq!(c.slots.len(), LIVE_SLOTS);
    }

    #[test]
    fn a_slot_is_bounded_and_without_a_usable_map_is_a_one_shot() {
        let mut s = Slot::one_shot("s", [1, 2, 3], SourceId(1));
        s.mode = SlotMode::Loop;
        s.length_beats = 0.25;
        s.start_beat = f64::NAN;
        s.gain_db = 1e9;
        s.stem_mask = [false; 4];
        s.sanitize();
        assert_eq!((s.mode, s.length_beats, s.start_beat, s.gain_db, s.stem_mask), (SlotMode::OneShot, 1.0, 0.0, 24.0, [true; 4]));
        let mut w = WarpMap::new(vec![0.0, 10.0], 0.0).unwrap();
        w.lock = Some(crate::WarpLock { start: 0, length: 10, offset: 0, tempo: 1 });
        s.warp = Some(w);
        s.mode = SlotMode::Loop;
        s.length_beats = 1e9;
        s.sanitize();
        assert_eq!((s.mode, s.length_beats, s.warp.as_ref().unwrap().lock), (SlotMode::Loop, MAX_SLOT_BEATS, None));
    }
}
