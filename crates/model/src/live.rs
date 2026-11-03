//! The Session view: Live columns of clip slots launched over a playing set.
//!
//! What a slot holds is part of the document (saved, undoable to edit); what is playing is not.
//!
//! A slot holds audio ([`Slot`]) or MIDI ([`MidiSlot`]), never both ([`SlotContent`]), and a
//! column takes the kind its track plays: an audio track's column holds audio slots, an
//! instrument track's (an *instrument column*) MIDI slots, which its instrument plays.

use crate::{MAX_NAME_CHARS, MAX_WARP_BEATS, STEMS, Source, SourceId, StemSet, Track, TrackId, TrackKind, WarpMap, truncate_chars};
use serde::Deserialize;
use serde::de::Deserializer;
use soundcraft_midi::Sequence;

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
        // Four distinct stem files, none of them the slot's own source.
        let distinct = |set: &StemSet| {
            let ids: std::collections::BTreeSet<SourceId> = set.sources.iter().copied().chain([self.source]).collect();
            ids.len() == STEMS.len() + 1
        };
        if !self.stems.as_ref().is_some_and(distinct) {
            self.unsplit();
        }
        if let Some(set) = &mut self.stems {
            truncate_chars(&mut set.key, MAX_NAME_CHARS);
        }
    }
}

/// Beats in a MIDI slot's bar: slots count 4/4 bars.
const BAR_BEATS: f64 = 4.0;

/// A MIDI clip slot: a sequence its column's instrument plays, looped over whole bars.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MidiSlot {
    pub name: String,
    pub color: [u8; 3],
    /// Notes from tick 0 (the loop's downbeat). Notes starting at `length_beats` or later don't
    /// play, and a note running past it is cut there.
    pub sequence: Sequence,
    /// Beats the loop lasts: whole 4/4 bars (at least one).
    #[serde(default = "one_bar")]
    pub length_beats: f64,
    /// Scales the notes' velocities (the instrument makes the audio, so there is none to scale).
    #[serde(default)]
    pub gain_db: f32,
}

fn one_bar() -> f64 {
    BAR_BEATS
}

impl MidiSlot {
    /// Repair invariants after loading untrusted data: whole bars within [`MAX_SLOT_BEATS`], a
    /// usable gain and notes within the model's bounds.
    pub fn sanitize(&mut self) {
        truncate_chars(&mut self.name, MAX_NAME_CHARS);
        let bars = if self.length_beats.is_finite() { (self.length_beats / BAR_BEATS).round() } else { 1.0 };
        self.length_beats = bars.clamp(1.0, (MAX_SLOT_BEATS / BAR_BEATS).floor()) * BAR_BEATS;
        self.gain_db = if self.gain_db.is_finite() { self.gain_db.clamp(SLOT_GAIN_DB.0, SLOT_GAIN_DB.1) } else { 0.0 };
        crate::clip::bound_sequence(&mut self.sequence);
    }
}

/// What a slot holds: audio or MIDI. Saved without a tag: a MIDI slot is the one with a
/// `sequence` (an audio slot has a `source`), so sessions from before MIDI slots load unchanged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum SlotContent {
    Audio(Slot),
    Midi(MidiSlot),
}

/// The kind of material a slot holds, and so the kind of column it goes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// Audio, in a column on an audio track.
    Audio,
    /// MIDI, in an instrument column (on an instrument track).
    Midi,
}

impl SlotKind {
    /// `audio` or `midi`.
    pub fn id(self) -> &'static str {
        match self {
            SlotKind::Audio => "audio",
            SlotKind::Midi => "midi",
        }
    }

    /// The slots a column on a `track` track holds; `None` for a kind no column plays into.
    pub fn of_track(track: TrackKind) -> Option<SlotKind> {
        match track {
            TrackKind::Audio => Some(SlotKind::Audio),
            TrackKind::Instrument => Some(SlotKind::Midi),
            _ => None,
        }
    }
}

impl From<Slot> for SlotContent {
    fn from(s: Slot) -> Self {
        SlotContent::Audio(s)
    }
}

impl From<MidiSlot> for SlotContent {
    fn from(s: MidiSlot) -> Self {
        SlotContent::Midi(s)
    }
}

impl SlotContent {
    pub fn kind(&self) -> SlotKind {
        match self {
            SlotContent::Audio(_) => SlotKind::Audio,
            SlotContent::Midi(_) => SlotKind::Midi,
        }
    }

    pub fn audio(&self) -> Option<&Slot> {
        match self {
            SlotContent::Audio(s) => Some(s),
            SlotContent::Midi(_) => None,
        }
    }

    pub fn audio_mut(&mut self) -> Option<&mut Slot> {
        match self {
            SlotContent::Audio(s) => Some(s),
            SlotContent::Midi(_) => None,
        }
    }

    pub fn midi(&self) -> Option<&MidiSlot> {
        match self {
            SlotContent::Midi(s) => Some(s),
            SlotContent::Audio(_) => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            SlotContent::Audio(s) => &s.name,
            SlotContent::Midi(s) => &s.name,
        }
    }

    pub fn color(&self) -> [u8; 3] {
        match self {
            SlotContent::Audio(s) => s.color,
            SlotContent::Midi(s) => s.color,
        }
    }

    pub fn length_beats(&self) -> f64 {
        match self {
            SlotContent::Audio(s) => s.length_beats,
            SlotContent::Midi(s) => s.length_beats,
        }
    }

    pub fn gain_db(&self) -> f32 {
        match self {
            SlotContent::Audio(s) => s.gain_db,
            SlotContent::Midi(s) => s.gain_db,
        }
    }

    /// How it plays: a MIDI slot always loops.
    pub fn mode(&self) -> SlotMode {
        match self {
            SlotContent::Audio(s) => s.mode,
            SlotContent::Midi(_) => SlotMode::Loop,
        }
    }

    /// Every audio source the slot plays or depends on (none for MIDI).
    pub fn audio_sources(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.audio().into_iter().flat_map(Slot::audio_sources)
    }

    /// Repair invariants after loading untrusted data (see [`Slot::sanitize`],
    /// [`MidiSlot::sanitize`]).
    pub fn sanitize(&mut self) {
        match self {
            SlotContent::Audio(s) => s.sanitize(),
            SlotContent::Midi(s) => s.sanitize(),
        }
    }
}

/// A Live column: an audio track ("Live 1"…) whose input the column's audio slots play into, or
/// an instrument track whose instrument its MIDI slots play.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveColumn {
    pub track: TrackId,
    /// Reading is lenient: a malformed slot reads as empty, and the list is cut or padded to
    /// [`LIVE_SLOTS`] (a missing list is all empty).
    #[serde(default, deserialize_with = "loadable_slots")]
    pub slots: [Option<SlotContent>; LIVE_SLOTS],
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
fn loadable_slots<'de, D: Deserializer<'de>>(d: D) -> Result<[Option<SlotContent>; LIVE_SLOTS], D::Error> {
    let list = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(a) => a,
        _ => Vec::new(),
    };
    let mut slots: [Option<SlotContent>; LIVE_SLOTS] = Default::default();
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
    pub fn slot(&self, column: usize, slot: usize) -> Option<&SlotContent> {
        self.columns.get(column).and_then(|c| c.slots.get(slot)).and_then(Option::as_ref)
    }

    /// The audio slot at `column`, `slot` (`None` when empty or MIDI).
    pub fn audio_slot(&self, column: usize, slot: usize) -> Option<&Slot> {
        self.slot(column, slot).and_then(SlotContent::audio)
    }

    /// The MIDI slot at `column`, `slot` (`None` when empty or audio).
    pub fn midi_slot(&self, column: usize, slot: usize) -> Option<&MidiSlot> {
        self.slot(column, slot).and_then(SlotContent::midi)
    }

    /// Every filled slot, with its column and row.
    pub fn slots(&self) -> impl Iterator<Item = (usize, usize, &SlotContent)> {
        self.columns.iter().enumerate().flat_map(|(c, col)| col.slots.iter().enumerate().filter_map(move |(r, s)| s.as_ref().map(|s| (c, r, s))))
    }

    /// The track column `column` plays into, while it has one: an audio or instrument track of
    /// `tracks` that no earlier column plays into. `None` for a column whose track was lost
    /// (deleted, of another kind, or taken by another column): it keeps its slots, plays nothing,
    /// and `live.setup` gives it a new track.
    pub fn column_track(&self, column: usize, tracks: &[Track]) -> Option<TrackId> {
        let id = self.columns.get(column)?.track;
        let earlier = self.columns.iter().take(column).any(|c| c.track == id);
        tracks.iter().any(|t| t.id == id && SlotKind::of_track(t.kind).is_some()).then_some(id).filter(|_| !earlier)
    }

    /// The kind of slot column `column` holds, from its track: audio for an audio track, MIDI
    /// for an instrument track. `None` for a lost column (see [`LiveState::column_track`]).
    pub fn column_kind(&self, column: usize, tracks: &[Track]) -> Option<SlotKind> {
        let id = self.column_track(column, tracks)?;
        tracks.iter().find(|t| t.id == id).and_then(|t| SlotKind::of_track(t.kind))
    }

    /// The columns whose track was lost (see [`LiveState::column_track`]).
    pub fn lost_columns(&self, tracks: &[Track]) -> Vec<usize> {
        (0..self.columns.len()).filter(|&c| self.column_track(c, tracks).is_none()).collect()
    }

    /// Repair invariants after loading untrusted data: at most [`MAX_LIVE_COLUMNS`] columns are
    /// kept (a column whose track was lost stays, see [`LiveState::column_track`]), a slot whose
    /// source is missing is emptied, stems naming a missing source are dropped, and each slot is
    /// bounded.
    ///
    /// A slot of the wrong kind for its column's track (MIDI on an audio track's column, audio
    /// on an instrument column) is emptied: it could never play there. A lost column keeps all
    /// its slots, since its kind is not known until `live.setup` gives it a track.
    pub fn sanitize(&mut self, sources: &[Source], tracks: &[Track]) {
        let known: std::collections::BTreeSet<SourceId> = sources.iter().map(|s| s.id).collect();
        self.columns.truncate(MAX_LIVE_COLUMNS);
        let kinds: Vec<Option<SlotKind>> = (0..self.columns.len()).map(|c| self.column_kind(c, tracks)).collect();
        for (col, kind) in self.columns.iter_mut().zip(kinds) {
            for slot in col.slots.iter_mut() {
                let wrong_kind = slot.as_ref().is_some_and(|s| kind.is_some_and(|k| k != s.kind()));
                let lost_source = slot.as_ref().and_then(SlotContent::audio).is_some_and(|s| !known.contains(&s.source));
                if wrong_kind || lost_source {
                    *slot = None;
                }
                if let Some(SlotContent::Audio(s)) = slot
                    && s.stems.as_ref().is_some_and(|set| set.sources.iter().any(|id| !known.contains(id)))
                {
                    s.unsplit();
                }
                if let Some(s) = slot {
                    s.sanitize();
                }
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
        let c: LiveColumn = serde_json::from_str(r#"{"track": 3}"#).unwrap();
        assert!(c.slots.iter().all(Option::is_none), "no slots: eight empty ones");
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

    #[test]
    fn stems_that_repeat_a_file_are_dropped() {
        let set = |sources: [u64; 4]| Some(StemSet { key: "k".into(), sources: sources.map(SourceId), scale_log2: 0 });
        for (stems, kept) in [(set([2, 3, 4, 5]), true), (set([2, 3, 3, 5]), false), (set([1, 3, 4, 5]), false)] {
            let mut s = Slot::one_shot("s", [0; 3], SourceId(1));
            s.stems = stems;
            s.stem_mask = [true, false, true, true];
            s.sanitize();
            assert_eq!((s.stems.is_some(), s.stem_mask == [true; 4]), (kept, !kept));
        }
    }

    #[test]
    fn a_column_without_its_own_audio_track_is_lost() {
        let track = |id: u64, kind| Track::new(TrackId(id), format!("t{id}"), kind, crate::ChannelFormat::Stereo, [0; 3]);
        let tracks = [track(1, TrackKind::Audio), track(2, TrackKind::Aux), track(3, TrackKind::Audio)];
        let live = LiveState { columns: [1, 2, 9, 3, 1].map(|t| LiveColumn::new(TrackId(t))).to_vec(), quantise: Quantise::default() };
        let tracks_of: Vec<_> = (0..5).map(|c| live.column_track(c, &tracks)).collect();
        assert_eq!(tracks_of, [Some(TrackId(1)), None, None, Some(TrackId(3)), None]);
        assert_eq!(live.lost_columns(&tracks), [1, 2, 4]);
    }
}
