//! Recording live MIDI: takes into the arrangement and Session view slot recordings. Nothing
//! played is ever lost: a take that can't go where it was meant to goes on a new playlist, or
//! waits in the recovery list (Event › Recover MIDI Take).
//!
//! The player captures each live event the target plays, at the timeline sample it played on,
//! with the transport's jumps marked (`soundcraft_playback::capture`). [`frame`] drains that
//! queue every UI frame into what is recording:
//!
//! - **Arrangement takes.** Recording (Record + Play) with the live MIDI target record-armed
//!   collects its events; when recording stops, the take is split into passes at the loop wraps
//!   and locates, and `midi.record_take` turns each into a clip over the time it was played (one
//!   undo step), within the punch range if there is one. Recording stop never refuses: a pass
//!   over another clip goes on a new playlist made active (`on_overlap: new_playlist`), the
//!   clips under it intact on the one before, as audio loop passes do; with MIDI Merge on it is
//!   merged into the clip. Recording needs no audio input when only an instrument track is armed.
//!   While a slot recording runs, what the target plays goes into the slot, not into the take.
//! - **Slot recordings** (`live.record_slot`). Armed on an empty slot of an active instrument
//!   column, the column's track is the live target ([`target_override`]: the old target comes
//!   back afterwards). Capture starts at the next launch boundary `start` and runs for the slot's
//!   bars, to `end`. A slot loops seamlessly only if it launches exactly at `end`, and the audio
//!   thread must have the launch before it renders `end`: so the slot is written
//!   (`live.slot_from_take`) and its launch at `end` sent once the player has rendered to
//!   [`write_margin`] before `end` (the launch margin plus a UI frame). What is played in that
//!   last stretch (about 120 ms: notes started, notes released) is written in once the player
//!   has rendered past `end`. Both writes are one undo group (`Engine::begin_undo_group`); a
//!   playing slot edited away from its playhead plays on without a cut, so the loop's first pass
//!   is complete and nothing it sounds is interrupted. If a frame comes so late that the player
//!   is past the margin, the launch lands late (the audio thread keeps its phase) and the status
//!   line says so. If the slot was filled meanwhile, the take goes into the column's next empty
//!   slot, or, with none, waits in the recovery list. Stop, Esc or a new audio device cancels (a
//!   take already played is kept for recovery).
//!
//! **What a take keeps** ([`Thinner`]). The MIDI Input Filter's kinds (`setup.midi_input_filter`)
//! are left out. Redundant controller events are thinned as they arrive: a controller (CC, pitch
//! bend, channel or poly pressure) repeating its last value is dropped, and a second value of one
//! controller on one tick replaces the first. A take holds at most [`MAX_TAKE_EVENTS`] events;
//! beyond that controller events are dropped, and beyond [`HARD_TAKE_EVENTS`] notes too, counted
//! and said at the end. A take longer than one command takes ([`CHUNK_EVENTS`]) is placed in
//! several `midi.record_take`s inside one undo group ([`chunks`]): split between passes, or
//! inside a pass where no note is held (else where it must, the held notes struck again where
//! the next part starts).
//!
//! **Recovery** ([`KeptTake`]). The last [`KEPT_TAKES`] takes are kept, newest first, in memory
//! and in the autosave folder ([`TAKES_FILE`], written off the UI thread when large), so they
//! survive a crash. Event › Recover MIDI Take lists them (age, track, notes) and
//! `midi.recover_take {index?}` places one again (the newest by default) on a new playlist of its
//! track, one undo step: the track with its id while it has its name, else the MIDI track with
//! its name (another session), else a new MIDI track of that name. Takes never placed (a slot
//! take with nowhere to go, a take the engine refused) are not pushed out by placed ones: the
//! oldest placed take goes first.

use crate::SoundApp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use soundcraft_engine::cmd::{InputFilter, MAX_TAKE_EVENTS as COMMAND_EVENTS, live_launch_margin};
use soundcraft_midi_io::MidiMessage;
use soundcraft_model::{Session, TrackId};
use soundcraft_playback::{CAPTURE_CAPACITY, CaptureEvent, Captured, TakePass, midi_in, take_passes};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Beyond the launch margin, a slot is written this long before its end: one slow UI frame.
pub const FRAME_GUARD_S: f64 = 0.05;

/// The undo group both writes of a slot recording make.
const SLOT_GROUP: &str = "Record Slot Take";

/// The undo group of a take placed in several commands (or on a new track): their own label.
const TAKE_GROUP: &str = "Record MIDI";

/// Events an arrangement take keeps before it drops controller events.
pub const MAX_TAKE_EVENTS: usize = 2_000_000;
/// Events an arrangement take keeps at all.
pub const HARD_TAKE_EVENTS: usize = 3_000_000;
/// Events per `midi.record_take` the app sends (well within the engine's limit).
pub const CHUNK_EVENTS: usize = 200_000;
/// Takes kept for recovery.
pub const KEPT_TAKES: usize = 8;
/// Events the kept takes hold in all: beyond it the oldest (placed first) go.
pub const KEPT_EVENTS: usize = 4_000_000;
/// The kept takes' copy in the autosave folder.
pub const TAKES_FILE: &str = "last_midi_takes.json";
/// Kept events above which the copy is written off the UI thread.
const WRITE_INLINE_EVENTS: usize = 50_000;

/// A take's events as they arrive: filtered, thinned and bounded (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct Thinner {
    /// Controller events left out as redundant.
    pub thinned: u64,
    /// Events left out because the take was full.
    pub dropped: u64,
    /// Events the MIDI Input Filter left out.
    pub filtered: u64,
    /// Events kept before controllers are dropped, and at all.
    soft: usize,
    hard: usize,
    /// Per controller (kind, channel, number): the tick, index and value of its last event kept.
    last: HashMap<(u8, u8, u8), (i64, usize, u16)>,
    /// The pass the last event was in.
    pass: Option<u32>,
}

impl Thinner {
    /// One keeping up to `soft` events, then only notes up to `hard`.
    pub fn new(soft: usize, hard: usize) -> Thinner {
        Thinner { thinned: 0, dropped: 0, filtered: 0, soft, hard: hard.max(soft), last: HashMap::new(), pass: None }
    }

    /// Adds `c` to `out` (see the module docs), its tick read on `s`'s tempo.
    pub fn push(&mut self, out: &mut Vec<Captured>, c: Captured, s: &Session, filter: InputFilter) {
        if self.pass != Some(c.pass) {
            self.last.clear();
            self.pass = Some(c.pass);
        }
        let Some(m) = c.msg() else {
            // A jump: the next pass reads its controllers afresh.
            self.last.clear();
            out.push(c);
            return;
        };
        if !filter.keeps(midi_in::raw(m)[0]) {
            self.filtered = self.filtered.saturating_add(1);
            return;
        }
        if let Some((key, value)) = controller(&m) {
            let tick = tick_of(s, c.at);
            if let Some(l) = self.last.get_mut(&key) {
                if l.2 == value {
                    self.thinned = self.thinned.saturating_add(1);
                    return;
                }
                if l.0 == tick
                    && let Some(slot) = out.get_mut(l.1)
                {
                    *slot = c;
                    l.2 = value;
                    self.thinned = self.thinned.saturating_add(1);
                    return;
                }
            }
            if out.len() >= self.soft {
                self.dropped = self.dropped.saturating_add(1);
                return;
            }
            self.last.insert(key, (tick, out.len(), value));
            out.push(c);
            return;
        }
        if out.len() >= self.hard {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        out.push(c);
    }
}

/// A controller message's controller (kind, channel, number) and value; `None` for the rest.
fn controller(m: &MidiMessage) -> Option<((u8, u8, u8), u16)> {
    Some(match *m {
        MidiMessage::ControlChange { ch, cc, value } => ((0xB0, ch, cc), u16::from(value)),
        MidiMessage::PitchBend { ch, value } => ((0xE0, ch, 0), u16::try_from(i32::from(value) + 8192).unwrap_or(0)),
        MidiMessage::ChannelPressure { ch, pressure } => ((0xD0, ch, 0), u16::from(pressure)),
        MidiMessage::PolyPressure { ch, note, pressure } => ((0xA0, ch, note), u16::from(pressure)),
        _ => return None,
    })
}

/// The tick a sample is on, rounded as takes count it.
fn tick_of(s: &Session, at: Samples) -> i64 {
    let t = s.tempo.samples_to_ticks_f(at, s.sample_rate).round();
    if t.is_finite() { t.clamp(-1e15, 1e15) as i64 } else { 0 }
}

/// An arrangement take being recorded on `track` from `start`.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiTake {
    pub track: TrackId,
    pub start: Samples,
    pub events: Vec<Captured>,
    pub thin: Thinner,
    /// The player's capture drops when it started.
    pub dropped: u64,
}

impl MidiTake {
    /// A take on `track` from `start`, bounded at [`MAX_TAKE_EVENTS`] and [`HARD_TAKE_EVENTS`].
    pub fn new(track: TrackId, start: Samples, dropped: u64) -> MidiTake {
        MidiTake { track, start, events: Vec::new(), thin: Thinner::new(MAX_TAKE_EVENTS, HARD_TAKE_EVENTS), dropped }
    }
}

/// A Session view slot recording (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct SlotRecording {
    pub column: usize,
    /// The slot it writes (the next empty one when the slot asked for was filled meanwhile).
    pub slot: usize,
    /// The slot asked for.
    pub asked: usize,
    pub bars: u32,
    pub replace: bool,
    /// The column's track: the live target while armed.
    pub track: TrackId,
    /// The boundary recording starts on, and where it ends (`bars` 4/4 bars later).
    pub start: Samples,
    pub end: Samples,
    /// What was played from `start` to `end`.
    pub events: Vec<Captured>,
    pub thin: Thinner,
    /// Events written by the first write (`None`: not written yet).
    pub written: Option<usize>,
    /// The first write put notes in the slot: it is ours to write again.
    pub ours: bool,
    /// Its launch at `end` was sent.
    pub launched: bool,
    /// The player has reported playing since it was armed.
    pub seen_playing: bool,
    /// The transport jumped while recording: what was played no longer fits the slot.
    pub jumped: bool,
    /// The player's capture drops and late launches when it was armed.
    pub dropped: u64,
    pub late: u64,
    /// The player it was armed on (`Player::id`): a new one (an audio device change) cancels it.
    pub player: Option<u64>,
}

/// What a slot recording does next, with the player rendered to `pos`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStep {
    Wait,
    /// Write the slot and send its launch at `end`.
    Write,
    /// Past the end: write in what was played since the first write (if anything), and finish.
    Finish,
}

impl SlotRecording {
    /// A recording of `bars` bars into `slot` of `column` from `start`.
    pub fn new(s: &Session, column: usize, slot: usize, bars: u32, replace: bool, track: TrackId, start: Samples) -> SlotRecording {
        let t0 = s.tempo.samples_to_ticks_f(start, s.sample_rate).round() as i64;
        let len = i64::from(bars) * 4 * TICKS_PER_QUARTER;
        let end = s.tempo.tick_to_samples(t0.saturating_add(len), s.sample_rate);
        SlotRecording {
            column,
            slot,
            asked: slot,
            bars,
            replace,
            track,
            start,
            end,
            events: Vec::new(),
            // A slot is written in one command: it keeps what one command takes.
            thin: Thinner::new(COMMAND_EVENTS / 2, COMMAND_EVENTS),
            written: None,
            ours: false,
            launched: false,
            seen_playing: false,
            jumped: false,
            dropped: 0,
            late: 0,
            player: None,
        }
    }

    /// The next step with the player rendered to `pos`, writing `margin` before the end.
    pub fn step(&self, pos: Samples, margin: Samples) -> SlotStep {
        match self.written {
            None if pos >= self.end => SlotStep::Finish,
            None if pos.saturating_add(margin) >= self.end => SlotStep::Write,
            Some(_) if pos >= self.end => SlotStep::Finish,
            _ => SlotStep::Wait,
        }
    }

    /// Keeps what was played inside the recording (filtered and thinned on `s`, see the module
    /// docs); a jump of the transport after its start marks it as jumped.
    pub fn collect(&mut self, events: &[Captured], s: &Session, filter: InputFilter) {
        for c in events {
            match c.event {
                CaptureEvent::Midi(_) if (self.start..self.end).contains(&c.at) => self.thin.push(&mut self.events, *c, s, filter),
                CaptureEvent::Jump { .. } if c.at >= self.start && c.at < self.end => self.jumped = true,
                _ => {}
            }
        }
    }

    /// How far it is, 0..=1, with the player at `pos` (0 while armed).
    pub fn progress(&self, pos: Samples) -> f32 {
        let len = (self.end - self.start).max(1) as f64;
        ((pos - self.start) as f64 / len).clamp(0.0, 1.0) as f32
    }

    /// The `live.slot_from_take` parameters for what was played so far, into `slot`.
    pub fn params(&self, slot: usize, replace: bool) -> Value {
        json!({"column": self.column, "slot": slot, "start": self.start, "bars": self.bars, "events": events_json(&self.events),
            "replace": replace})
    }

    /// The take as an arrangement take on the column's track, for recovery.
    pub fn kept(&self, s: &Session) -> KeptTake {
        let events = self.events.iter().filter_map(|c| c.msg().map(|m| (c.at, midi_in::raw(m)))).collect();
        KeptTake::new(s, self.track, vec![KeptPass { start: self.start, end: self.end, events }])
    }
}

/// How long before a slot recording's end it is written: the launch margin for the player's
/// largest callback (`callback`, session samples) plus [`FRAME_GUARD_S`].
pub fn write_margin(s: &Session, callback: Samples) -> Samples {
    live_launch_margin(s, callback).saturating_add(s.sample_rate.samples(FRAME_GUARD_S))
}

fn event_json(at: Samples, [status, data1, data2]: [u8; 3]) -> Value {
    json!({"at": at, "status": status, "data1": data1, "data2": data2})
}

/// Captured live events as the engine's take commands take them (jumps left out).
pub fn events_json(events: &[Captured]) -> Value {
    Value::Array(events.iter().filter_map(|c| c.msg().map(|m| event_json(c.at, midi_in::raw(m)))).collect())
}

/// Passes as `midi.record_take` takes them.
pub fn passes_json(passes: &[KeptPass]) -> Value {
    Value::Array(
        passes
            .iter()
            .map(|p| {
                let events: Vec<Value> = p.events.iter().map(|(at, b)| event_json(*at, *b)).collect();
                json!({"start": p.start, "end": p.end, "events": events})
            })
            .collect(),
    )
}

/// One pass of a kept take: its time and its events as MIDI 1.0 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeptPass {
    pub start: Samples,
    pub end: Samples,
    pub events: Vec<(Samples, [u8; 3])>,
}

impl KeptPass {
    fn of(p: &TakePass) -> KeptPass {
        KeptPass { start: p.start, end: p.end, events: p.events.iter().map(|(at, m)| (*at, midi_in::raw(*m))).collect() }
    }
}

/// A take kept for recovery (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeptTake {
    /// When it was kept: seconds since the Unix epoch (0: unknown).
    pub at_s: u64,
    /// Its track's id and name when it was recorded.
    pub track: u64,
    pub track_name: String,
    /// Note-ons played.
    pub notes: usize,
    /// It went onto a track: it goes before takes that never did.
    pub placed: bool,
    pub passes: Vec<KeptPass>,
}

impl KeptTake {
    /// A take of `passes` on `track` of `s`, kept now.
    pub fn new(s: &Session, track: TrackId, passes: Vec<KeptPass>) -> KeptTake {
        let notes = passes.iter().flat_map(|p| &p.events).filter(|(_, b)| b[0] & 0xF0 == 0x90 && b[2] > 0).count();
        KeptTake {
            at_s: now_s(),
            track: track.0,
            track_name: s.track(track).map(|t| t.name.clone()).unwrap_or_default(),
            notes,
            placed: false,
            passes,
        }
    }

    /// Events over its passes.
    pub fn events(&self) -> usize {
        self.passes.iter().map(|p| p.events.len()).sum()
    }

    /// "2 min ago · Lead · 16 notes" at `now_s` (seconds since the Unix epoch).
    pub fn label(&self, now_s: u64) -> String {
        let age = now_s.saturating_sub(self.at_s);
        let when = match age {
            _ if self.at_s == 0 => "earlier".to_string(),
            0..60 => "just now".to_string(),
            60..3_600 => format!("{} min ago", age / 60),
            3_600..86_400 => format!("{} h ago", age / 3_600),
            _ => format!("{} days ago", age / 86_400),
        };
        let unplaced = if self.placed { "" } else { " (not placed)" };
        format!("{when} · {} · {} note{}{unplaced}", self.track_name, self.notes, if self.notes == 1 { "" } else { "s" })
    }
}

/// Seconds since the Unix epoch (0 if the clock is before it).
fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Writes of the kept takes' copy: in order, the newest list winning.
#[derive(Debug, Default, Clone)]
pub struct TakeWriter {
    /// The newest write asked for.
    latest: Arc<AtomicU64>,
    /// Held while writing.
    lock: Arc<Mutex<()>>,
}

/// The live target a slot recording sets while armed (it comes before `midi.target`'s).
pub fn target_override(app: &SoundApp) -> Option<TrackId> {
    app.slot_recording.as_ref().map(|r| r.track)
}

/// The track an arrangement take records on: the live target, when it is record-armed.
pub fn armed_target(app: &SoundApp) -> Option<TrackId> {
    let s = app.engine.session();
    crate::midi_input::resolve_target(s, target_override(app).or(app.midi_target)).filter(|t| s.track(*t).is_some_and(|x| x.mixer.record_arm))
}

/// Record-armed instrument tracks that are not the live target (no live MIDI reaches them), by
/// name.
pub fn armed_not_target(app: &SoundApp) -> Vec<String> {
    let s = app.engine.session();
    let target = crate::midi_input::resolve_target(s, target_override(app).or(app.midi_target));
    s.tracks.iter().filter(|t| t.kind.is_midi() && t.mixer.record_arm && Some(t.id) != target).map(|t| t.name.clone()).collect()
}

fn drops(app: &SoundApp) -> u64 {
    app.player.as_ref().map_or(0, soundcraft_playback::Player::midi_capture_dropped)
}

fn lates(app: &SoundApp) -> u64 {
    app.player.as_ref().map_or(0, |p| p.live_state().iter().map(|c| u64::from(c.late)).sum())
}

/// What was lost: events the capture dropped since `since`, and `full` the take had no room for.
fn dropped_note(app: &SoundApp, since: u64, full: u64) -> String {
    let mut out = String::new();
    let n = drops(app).saturating_sub(since);
    if n > 0 {
        out.push_str(&format!(
            " ({n} live events were dropped: the capture queue was full, so the take may be missing notes and its timing may be approximate)"
        ));
    }
    if full > 0 {
        out.push_str(&format!(" ({full} events were left out: the take reached its size limit)"));
    }
    out
}

/// Loads the copy of the kept takes from the autosave folder once (after a crash), behind the
/// takes kept since.
pub fn ensure_loaded(app: &mut SoundApp) {
    if app.midi_takes_loaded {
        return;
    }
    app.midi_takes_loaded = true;
    let Some(dir) = &app.autosave_dir else { return };
    let path = dir.join(TAKES_FILE);
    if !path.is_file() {
        return;
    }
    let read: Option<Vec<KeptTake>> = soundcraft_engine::io::read_file(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    match read {
        Some(takes) => {
            for t in takes.into_iter().take(KEPT_TAKES) {
                if !app.midi_takes.contains(&t) {
                    app.midi_takes.push(t);
                }
            }
            evict(app);
        }
        None => log::warn!("{}: not a list of MIDI takes", path.display()),
    }
}

/// Keeps `take` as the newest for recovery (see [`evict`]), and saves the list.
pub fn keep(app: &mut SoundApp, take: KeptTake) {
    ensure_loaded(app);
    app.midi_takes.insert(0, take);
    evict(app);
    save_takes(app);
}

/// Lets the oldest placed takes go, else the oldest, until the list fits [`KEPT_TAKES`] and
/// [`KEPT_EVENTS`]; never the newest. Letting an unplaced one go is said.
fn evict(app: &mut SoundApp) {
    let fits = |t: &[KeptTake]| t.len() <= KEPT_TAKES && t.iter().map(KeptTake::events).sum::<usize>() <= KEPT_EVENTS;
    while app.midi_takes.len() > 1 && !fits(&app.midi_takes) {
        let at = app.midi_takes.iter().rposition(|t| t.placed).filter(|i| *i > 0).unwrap_or(app.midi_takes.len() - 1);
        let gone = app.midi_takes.remove(at);
        if !gone.placed {
            let line = format!("The oldest unplaced MIDI take ({}) no longer fits the recovery list and was let go", gone.label(now_s()));
            log::warn!("{line}");
            app.ui.status = line;
        }
    }
}

/// Writes `takes` to `path` unless a newer list (`w`, past `turn`) is on its way.
fn write_takes(takes: &[KeptTake], path: &str, w: &TakeWriter, turn: u64) {
    let _held = w.lock.lock().unwrap_or_else(PoisonError::into_inner);
    if w.latest.load(Ordering::SeqCst) != turn {
        return;
    }
    let r = serde_json::to_vec(takes).map_err(|e| e.to_string()).and_then(|b| soundcraft_engine::io::write_file(path, &b).map_err(|e| e.to_string()));
    if let Err(e) = r {
        log::warn!("keeping the MIDI takes for recovery: {e}");
    }
}

/// Saves the kept takes in the autosave folder: inline while small, else on a thread.
fn save_takes(app: &SoundApp) {
    let Some(dir) = &app.autosave_dir else { return };
    let path = dir.join(TAKES_FILE).to_string_lossy().into_owned();
    let w = app.take_writer.clone();
    let turn = w.latest.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    if cfg!(not(target_arch = "wasm32")) && app.midi_takes.iter().map(KeptTake::events).sum::<usize>() > WRITE_INLINE_EVENTS {
        let (takes, p, w2) = (app.midi_takes.clone(), path.clone(), w.clone());
        match std::thread::Builder::new().name("midi-takes".into()).spawn(move || write_takes(&takes, &p, &w2, turn)) {
            Ok(_) => return,
            // Written here instead.
            Err(e) => log::warn!("writing the MIDI takes on a thread: {e}"),
        }
    }
    write_takes(&app.midi_takes, &path, &w, turn);
}

/// Every UI frame, after the transport requests (see the module docs).
pub fn frame(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(p) = &app.player else { return };
    if !p.has_midi_capture() {
        app.midi_capture = Some(p.midi_capture(CAPTURE_CAPACITY));
    }
    let player = p.id();
    // Where the player has rendered to, read before draining: everything played before it is in.
    let (pos, playing) = (p.position(), p.is_playing());
    let mut got = Vec::new();
    if let Some(c) = &mut app.midi_capture {
        c.drain_into(&mut got);
    }
    let s = app.engine.session();
    let filter = InputFilter::of(s);
    let slot = app.slot_recording.is_some();
    if let Some(t) = &mut app.midi_take {
        take_in(t, &got, s, filter, slot);
    }
    let Some(r) = &mut app.slot_recording else { return };
    r.collect(&got, s, filter);
    ctx.request_repaint();
    if r.player.is_some_and(|id| id != player) {
        cancel_slot(app, "Slot recording cancelled: the audio device changed");
        return;
    }
    if playing {
        r.seen_playing = true;
    } else if r.seen_playing {
        cancel_slot(app, "Slot recording cancelled: the transport stopped");
        return;
    }
    if r.jumped {
        cancel_slot(app, "Slot recording cancelled: the transport jumped");
        return;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        cancel_slot(app, "Slot recording cancelled");
        return;
    }
    let margin = write_margin(app.engine.session(), app.callback_samples());
    slot_step(app, pos, margin);
}

/// Carries out the slot recording's next step with the player rendered to `pos`.
pub fn slot_step(app: &mut SoundApp, pos: Samples, margin: Samples) {
    let Some(r) = app.slot_recording.clone() else { return };
    let written = match r.step(pos, margin) {
        SlotStep::Wait => return,
        SlotStep::Write => {
            if !app.engine.in_undo_group() {
                app.engine.begin_undo_group(SLOT_GROUP);
            }
            match write_slot(app, &r, r.replace) {
                Ok((slot, n)) => {
                    let launched = n > 0 && launch(app, &r, slot);
                    if let Some(x) = &mut app.slot_recording {
                        (x.written, x.ours, x.launched, x.slot) = (Some(r.events.len()), n > 0, launched, slot);
                    }
                }
                Err(why) => keep_for_recovery(app, &r, &why),
            }
            return;
        }
        SlotStep::Finish if r.written == Some(r.events.len()) => Ok((r.slot, slot_notes(app, &r))),
        // Played since the first write (or nothing written yet: a frame came after the end).
        // A first write that put notes in the slot is ours: the whole take replaces it.
        SlotStep::Finish => {
            if !app.engine.in_undo_group() {
                app.engine.begin_undo_group(SLOT_GROUP);
            }
            write_slot(app, &r, r.replace || r.ours)
        }
    };
    match written {
        Ok((slot, n)) => {
            if n > 0 && !r.launched {
                // Late: it keeps its phase from `end`.
                launch(app, &r, slot);
            }
            finish_slot(app, &r, (n > 0).then_some((n, slot)));
        }
        Err(why) => keep_for_recovery(app, &r, &why),
    }
}

/// Sends the slot's launch at the recording's end.
fn launch(app: &SoundApp, r: &SlotRecording, slot: usize) -> bool {
    let Some(p) = &app.player else { return false };
    p.live(soundcraft_mix::LiveCmd::Launch { column: r.column, slot, at: r.end });
    true
}

/// Notes in the slot the recording writes.
fn slot_notes(app: &SoundApp, r: &SlotRecording) -> usize {
    app.engine.session().live.slot(r.column, r.slot).and_then(|x| x.midi()).map_or(0, |m| m.sequence.notes.len())
}

/// Writes what the recording holds into its slot, or, when that slot was filled meanwhile, the
/// column's next empty one: the slot written and its notes, or why not.
fn write_slot(app: &mut SoundApp, r: &SlotRecording, replace: bool) -> Result<(usize, usize), String> {
    let mut slot = r.slot;
    let v = match app.engine.execute("live.slot_from_take", &r.params(slot, replace)) {
        Ok(v) => v,
        Err(soundcraft_engine::EngineError::WouldReplace { .. }) => {
            let s = app.engine.session();
            let free = s.live.columns.get(r.column).and_then(soundcraft_model::LiveColumn::next_empty);
            let Some(free) = free else {
                return Err(format!("slot {} of Live {} was filled meanwhile and the column has no empty slot", r.slot + 1, r.column + 1));
            };
            slot = free;
            app.engine.execute("live.slot_from_take", &r.params(slot, false)).map_err(|e| e.to_string())?
        }
        Err(e) => return Err(e.to_string()),
    };
    if let Some(p) = &app.player {
        p.update_session(app.engine.session_arc());
    }
    if let Some(x) = &mut app.slot_recording {
        x.slot = slot;
    }
    Ok((slot, v.get("notes").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0)))
}

fn finish_slot(app: &mut SoundApp, r: &SlotRecording, recorded: Option<(usize, usize)>) {
    app.slot_recording = None;
    app.engine.commit_undo_group();
    if let Some(p) = &app.player {
        p.capture_midi_while_playing(false);
    }
    let dropped = dropped_note(app, r.dropped, r.thin.dropped);
    let late = lates(app) > r.late;
    app.ui.status = match recorded {
        Some((n, slot)) => {
            let moved = if slot != r.asked { format!(" (slot {} was filled meanwhile)", r.asked + 1) } else { String::new() };
            let how = if late { "it launched late, on the beat from where it was seen" } else { "looping" };
            format!("Recorded {n} note(s) into slot {} of Live {}{moved}: {how}{dropped}", slot + 1, r.column + 1)
        }
        None => format!("Nothing recorded into the slot: no notes were played{dropped}"),
    };
}

/// The slot couldn't be written (`why`): the take waits in the recovery list.
fn keep_for_recovery(app: &mut SoundApp, r: &SlotRecording, why: &str) {
    if r.events.iter().any(|c| c.msg().is_some()) {
        let kept = r.kept(app.engine.session());
        keep(app, kept);
        stop_slot(app, &format!("Slot take not written: {why}. Event › Recover MIDI Take puts it on the track"));
    } else {
        stop_slot(app, &format!("Slot recording stopped: {why}"));
    }
}

/// Stops a slot recording (Stop, Esc, a jump, a refusal, a new audio device): what was already
/// written stays, and what was played but not written is kept for recovery.
pub fn cancel_slot(app: &mut SoundApp, why: &str) {
    let Some(r) = &app.slot_recording else { return };
    let unwritten = r.written.is_none_or(|n| n < r.events.len()) && r.events.iter().any(|c| c.msg().is_some());
    if unwritten {
        let kept = r.kept(app.engine.session());
        keep(app, kept);
        stop_slot(app, &format!("{why} (what was played is kept: Event › Recover MIDI Take)"));
    } else {
        stop_slot(app, why);
    }
}

/// Ends the slot recording, saying `status`.
fn stop_slot(app: &mut SoundApp, status: &str) {
    if app.slot_recording.take().is_none() {
        return;
    }
    app.engine.commit_undo_group();
    if let Some(p) = &app.player {
        p.capture_midi_while_playing(false);
    }
    app.ui.status = status.to_string();
}

/// Arms a slot recording from the boundary `at` (see [`crate::SoundApp::live_request`]). Refused
/// for a column whose track is inactive: it doesn't play, so live MIDI would go elsewhere.
pub fn arm_slot(app: &mut SoundApp, column: usize, slot: usize, bars: u32, replace: bool, at: Samples, looped: Option<Range>) -> bool {
    let s = app.engine.session();
    let Some(track) = s.live.column_track(column, &s.tracks) else {
        app.ui.status = format!("Live column {} has lost its track", column + 1);
        return false;
    };
    if let Some(t) = s.track(track).filter(|t| !crate::midi_input::plays_midi(t)) {
        app.ui.status = format!("Live {}'s track \"{}\" is inactive: make it active to record into its slots", column + 1, t.name);
        return false;
    }
    let mut r = SlotRecording::new(s, column, slot, bars, replace, track, at);
    if looped.is_some_and(|l| r.end > l.end) {
        app.ui.status = "The slot recording would run past the loop's end: turn off Loop Playback or loop a longer selection".into();
        return false;
    }
    if let Some(old) = &app.slot_recording {
        let why = format!("Slot recording into slot {} of Live {} replaced", old.slot + 1, old.column + 1);
        cancel_slot(app, &why);
    }
    (r.dropped, r.late) = (drops(app), lates(app));
    if let Some(p) = &app.player {
        p.capture_midi_while_playing(true);
        r.player = Some(p.id());
    }
    app.ui.status = format!("Recording {bars} bar(s) into slot {} of Live {} from the next boundary (Esc cancels)", slot + 1, column + 1);
    app.slot_recording = Some(r);
    true
}

/// Starts an arrangement take on the armed target from `start`, if there is one.
pub fn start_take(app: &mut SoundApp, start: Samples) -> bool {
    let Some(track) = armed_target(app) else { return false };
    if let Some(c) = &mut app.midi_capture {
        // What was played before recording is no part of the take.
        while c.pop().is_some() {}
    }
    app.midi_take = Some(MidiTake::new(track, start, drops(app)));
    true
}

/// Adds what the capture `got` to an arrangement take. While a slot records, the target plays into
/// the slot: only the transport's jumps go into the take.
fn take_in(t: &mut MidiTake, got: &[Captured], s: &Session, filter: InputFilter, slot_recording: bool) {
    for c in got {
        if !slot_recording || c.msg().is_none() {
            t.thin.push(&mut t.events, *c, s, filter);
        }
    }
}

/// Recording stopped with the player rendered to `end` (and the punch range `punch`, if any):
/// the take's passes become clips, a pass over another clip on a new playlist. The take is kept
/// for recovery first. Returns the status to show, if a take was recording.
pub fn finish_take(app: &mut SoundApp, end: Samples, punch: Option<Range>) -> Option<String> {
    let mut take = app.midi_take.take()?;
    let mut rest = Vec::new();
    if let Some(c) = &mut app.midi_capture {
        c.drain_into(&mut rest);
    }
    take_in(&mut take, &rest, app.engine.session(), InputFilter::of(app.engine.session()), app.slot_recording.is_some());
    let mut passes = take_passes(take.start, end, &take.events);
    if let Some(r) = punch {
        for p in &mut passes {
            (p.start, p.end) = (p.start.max(r.start), p.end.min(r.end));
        }
        passes.retain(|p| p.end > p.start);
    }
    let dropped = dropped_note(app, take.dropped, take.thin.dropped);
    if passes.is_empty() {
        return Some(format!("Nothing recorded: the MIDI take was empty{dropped}"));
    }
    let passes: Vec<KeptPass> = passes.iter().map(KeptPass::of).collect();
    let track = take.track;
    drop(take);
    let kept = passes.iter().any(|p| !p.events.is_empty());
    if kept {
        let k = KeptTake::new(app.engine.session(), track, passes.clone());
        keep(app, k);
    }
    Some(match place(app, track, &passes, false) {
        Ok(v) => {
            if kept && let Some(k) = app.midi_takes.first_mut() {
                k.placed = true;
                save_takes(app);
            }
            format!("{}{dropped}", v.get("message").and_then(Value::as_str).unwrap_or("Recorded the MIDI take"))
        }
        Err(e) => format!("MIDI take not placed: {e}. Event › Recover MIDI Take tries again{dropped}"),
    })
}

/// Places `passes` on `track` with `midi.record_take`, a pass over another clip on a new
/// playlist (`fresh`: every pass on a new playlist, as recovery does). A take longer than one
/// command goes in several ([`chunks`]), one undo step; if one fails, none stays. The last
/// command's result, its notes and message counting them all.
fn place(app: &mut SoundApp, track: TrackId, passes: &[KeptPass], fresh: bool) -> Result<Value, String> {
    let parts = chunks(passes, CHUNK_EVENTS);
    let grouped = parts.len() > 1 && !app.engine.in_undo_group();
    let steps = app.engine.undo_history().len();
    if grouped {
        app.engine.begin_undo_group(TAKE_GROUP);
    }
    let mut last = Value::Null;
    let (mut notes, mut new_playlist) = (0u64, false);
    for part in &parts {
        let mut p = json!({"track": track.0, "passes": passes_json(part), "on_overlap": "new_playlist"});
        if fresh {
            p["new_playlist"] = json!(true);
        }
        match app.run_typed("midi.record_take", p) {
            Ok(v) => {
                notes = notes.saturating_add(v["notes"].as_u64().unwrap_or(0));
                new_playlist |= v["new_playlist"] == true;
                last = v;
            }
            Err(e) => {
                if grouped {
                    app.engine.commit_undo_group();
                    if app.engine.undo_history().len() > steps {
                        app.engine.undo();
                    }
                }
                return Err(e.to_string());
            }
        }
    }
    if grouped {
        app.engine.commit_undo_group();
    }
    if parts.len() > 1 {
        let name = app.engine.session().track(track).map(|t| t.name.clone()).unwrap_or_default();
        let msg = match last["playlist"].as_str() {
            Some(pl) if new_playlist => {
                format!("Recorded {notes} MIDI note(s) on `{name}`, on the new playlist `{pl}` (the clips under it stay on the playlist before)")
            }
            _ => format!("Recorded {notes} MIDI note(s) on `{name}`"),
        };
        last["notes"] = json!(notes);
        last["message"] = json!(msg);
        last["commands"] = json!(parts.len());
    }
    Ok(last)
}

/// Note-ons and note-offs change what is held: `(channel, pitch)` → velocity.
fn hold(held: &mut BTreeMap<(u8, u8), u8>, [status, d1, d2]: [u8; 3]) {
    match status & 0xF0 {
        0x90 if d2 > 0 => {
            held.insert((status & 0x0F, d1), d2);
        }
        0x80 | 0x90 => {
            held.remove(&(status & 0x0F, d1));
        }
        _ => {}
    }
}

/// `passes` in groups of at most about `max` events (and 1000 passes), one `midi.record_take`
/// each: a longer pass is split where no note is held, else at `max` events with the notes held
/// there struck again where the next part starts.
pub fn chunks(passes: &[KeptPass], max: usize) -> Vec<Vec<KeptPass>> {
    let max = max.max(2);
    let mut parts = Vec::new();
    for p in passes {
        split_pass(p, max, &mut parts);
    }
    let mut out: Vec<Vec<KeptPass>> = Vec::new();
    let (mut cur, mut n) = (Vec::new(), 0usize);
    for p in parts {
        if !cur.is_empty() && (n.saturating_add(p.events.len()) > max || cur.len() >= 1_000) {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        n = n.saturating_add(p.events.len());
        cur.push(p);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Splits one pass into parts of at most about `max` events (see [`chunks`]).
fn split_pass(p: &KeptPass, max: usize, out: &mut Vec<KeptPass>) {
    let ev = &p.events;
    if ev.len() <= max {
        out.push(p.clone());
        return;
    }
    // Before each event: nothing is held and time moved on (a clean place to split).
    let mut quiet = vec![false; ev.len()];
    let mut held = BTreeMap::new();
    for (i, e) in ev.iter().enumerate() {
        if let (Some(q), Some(prev)) = (quiet.get_mut(i), i.checked_sub(1).and_then(|j| ev.get(j))) {
            *q = held.is_empty() && prev.0 < e.0;
        }
        hold(&mut held, e.1);
    }
    held.clear();
    let (mut from, mut start) = (0usize, p.start);
    let mut lead: Vec<(Samples, [u8; 3])> = Vec::new();
    while ev.len() - from + lead.len() > max {
        let limit = (from + max.saturating_sub(lead.len()).max(1)).min(ev.len() - 1);
        let clean = (from + 1..=limit).rev().find(|i| quiet.get(*i).copied().unwrap_or(false));
        let mut cut = clean.unwrap_or(limit).max(from + 1);
        if clean.is_none() {
            // On a sample boundary, so no event of the part falls on its end.
            while cut > from + 1 && ev.get(cut - 1).map(|e| e.0) == ev.get(cut).map(|e| e.0) {
                cut -= 1;
            }
        }
        let at = ev.get(cut).map_or(p.end, |e| e.0);
        let mut events = std::mem::take(&mut lead);
        if let Some(part) = ev.get(from..cut) {
            events.extend_from_slice(part);
            part.iter().for_each(|e| hold(&mut held, e.1));
        }
        out.push(KeptPass { start, end: at.max(start.saturating_add(1)), events });
        // Notes held across the split: cut here, struck again at the next part's start.
        lead = held.iter().map(|(&(ch, pitch), &vel)| (at, [0x90 | ch, pitch, vel])).collect();
        (from, start) = (cut, at);
    }
    let mut events = lead;
    events.extend_from_slice(ev.get(from..).unwrap_or_default());
    out.push(KeptPass { start, end: p.end.max(start.saturating_add(1)), events });
}

/// The track a kept take goes back on: its own while it has its name, else the MIDI track with
/// its name (another session), else a new MIDI track of that name; and a note saying which.
fn recover_track(app: &mut SoundApp, take: &KeptTake) -> Result<(TrackId, String), String> {
    let s = app.engine.session();
    if let Some(t) = s.track(TrackId(take.track)).filter(|t| t.name == take.track_name && t.kind.is_midi()) {
        return Ok((t.id, String::new()));
    }
    if let Some(t) = s.tracks.iter().find(|t| t.name == take.track_name && t.kind.is_midi()) {
        return Ok((t.id, String::new()));
    }
    let name = if take.track_name.is_empty() { "Recovered MIDI" } else { take.track_name.as_str() };
    let v = app.run("track.new", json!({"kind": "midi", "name": name}))?;
    let id = TrackId(v["tracks"][0].as_u64().ok_or("no track was made for the take")?);
    let made = app.engine.session().track(id).map(|t| t.name.clone()).unwrap_or_default();
    Ok((id, format!(" (its track isn't in this session: on the new MIDI track `{made}`)")))
}

/// `midi.recover_take {index?, list?}`: places a kept take (`index` in the list, 0 the newest,
/// the default) again, on a new playlist of its track, one undo step; with `list`, lists them.
pub fn recover(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    ensure_loaded(app);
    if p.get("list").and_then(Value::as_bool) == Some(true) {
        let now = now_s();
        let takes: Vec<Value> = app
            .midi_takes
            .iter()
            .enumerate()
            .map(|(i, t)| {
                json!({"index": i, "label": t.label(now), "track": t.track, "track_name": t.track_name, "notes": t.notes, "placed": t.placed,
                    "at_s": t.at_s, "events": t.events()})
            })
            .collect();
        return Ok(json!({"takes": takes}));
    }
    let index = match p.get("index") {
        None | Some(Value::Null) => 0,
        Some(v) => v.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or("`index` is a take's place in the list (0: the newest)")?,
    };
    let Some(take) = app.midi_takes.get(index).cloned() else {
        return Err(match app.midi_takes.len() {
            0 => "no MIDI take to recover: nothing was recorded".to_string(),
            n => format!("no MIDI take {index}: {n} kept (0 is the newest)"),
        });
    };
    let grouped = !app.engine.in_undo_group();
    let steps = app.engine.undo_history().len();
    if grouped {
        app.engine.begin_undo_group(TAKE_GROUP);
    }
    let r = recover_track(app, &take).and_then(|(track, how)| place(app, track, &take.passes, true).map(|v| (v, how)));
    if grouped {
        app.engine.commit_undo_group();
        if r.is_err() && app.engine.undo_history().len() > steps {
            app.engine.undo();
        }
    }
    let (mut v, how) = r?;
    if let Some(t) = app.midi_takes.get_mut(index) {
        t.placed = true;
    }
    save_takes(app);
    let msg = format!("{}{how}", v.get("message").and_then(Value::as_str).unwrap_or("Recovered the MIDI take"));
    v["message"] = json!(msg);
    app.ui.status = msg;
    Ok(v)
}

/// Event › Recover MIDI Take ▸: the kept takes, newest first (age, track, notes).
pub fn menu(app: &mut SoundApp, ui: &mut egui::Ui) {
    ui.menu_button("Recover MIDI Take", |ui| {
        ensure_loaded(app);
        if app.midi_takes.is_empty() {
            ui.add_enabled(false, egui::Button::new("No MIDI takes kept"));
            return;
        }
        let now = now_s();
        let labels: Vec<String> = app.midi_takes.iter().map(|t| t.label(now)).collect();
        for (i, label) in labels.into_iter().enumerate() {
            if ui.button(label).on_hover_text("Place it again, on a new playlist of its track").clicked() {
                let _ = app.run("midi.recover_take", json!({"index": i}));
                ui.close();
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_midi_io::MidiMessage;

    fn rec() -> SlotRecording {
        SlotRecording::new(&Session::default(), 4, 2, 1, false, TrackId(9), 96_000)
    }

    #[test]
    fn a_slot_recording_writes_a_margin_before_its_end_and_finishes_after_it() {
        let s = Session::default();
        let mut r = rec();
        assert_eq!(r.end, 192_000, "one 4/4 bar at 120 bpm");
        let m = write_margin(&s, 256);
        assert_eq!(m, 3_360 + 2_400, "70 ms plus a 50 ms frame");
        assert_eq!(r.step(100_000, m), SlotStep::Wait);
        assert_eq!(r.step(192_000 - m - 1, m), SlotStep::Wait);
        assert_eq!(r.step(192_000 - m, m), SlotStep::Write);
        r.written = Some(0);
        assert_eq!(r.step(191_999, m), SlotStep::Wait);
        assert_eq!(r.step(192_000, m), SlotStep::Finish);
        // A frame that only comes after the end writes and finishes at once.
        assert_eq!(rec().step(200_000, m), SlotStep::Finish);
        assert_eq!((rec().progress(90_000), rec().progress(144_000), rec().progress(300_000)), (0.0, 0.5, 1.0));
    }

    #[test]
    fn only_what_is_played_inside_the_recording_is_kept_and_a_jump_is_noticed() {
        let mut r = rec();
        let on = |at| Captured { at, pass: 0, event: CaptureEvent::Midi(MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 }) };
        let (s, f) = (Session::default(), InputFilter::default());
        r.collect(&[on(95_999), on(96_000), on(191_999), on(192_000)], &s, f);
        assert_eq!(r.events, [on(96_000), on(191_999)]);
        let p = r.params(3, true);
        assert_eq!(p["events"][0], json!({"at": 96_000, "status": 0x90, "data1": 60, "data2": 100}));
        assert_eq!((p["replace"].as_bool(), p["bars"].as_u64(), p["slot"].as_u64()), (Some(true), Some(1), Some(3)));
        assert!(!r.jumped);
        r.collect(&[Captured { at: 150_000, pass: 0, event: CaptureEvent::Jump { to: 0 } }], &s, f);
        assert!(r.jumped);
        let take = r.kept(&s);
        assert_eq!((take.passes[0].start, take.passes[0].events.len(), take.notes), (96_000, 2, 2));
    }

    fn cc(at: Samples, pass: u32, value: u8) -> Captured {
        Captured { at, pass, event: CaptureEvent::Midi(MidiMessage::ControlChange { ch: 0, cc: 1, value }) }
    }

    fn values(out: &[Captured]) -> Vec<(Samples, u8)> {
        out.iter()
            .filter_map(|c| match c.msg() {
                Some(MidiMessage::ControlChange { value, .. }) => Some((c.at, value)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn controllers_are_thinned_and_a_full_take_drops_controllers_before_notes() {
        let s = Session::default();
        let f = InputFilter::default();
        let mut t = Thinner::new(100, 200);
        let mut out = Vec::new();
        // 48 kHz, 120 bpm, 960 ticks a beat: 25 samples a tick.
        for c in [cc(0, 0, 10), cc(100, 0, 10), cc(200, 0, 11), cc(201, 0, 12), cc(400, 0, 12), cc(500, 0, 13)] {
            t.push(&mut out, c, &s, f);
        }
        assert_eq!(values(&out), [(0, 10), (201, 12), (500, 13)], "repeats dropped, the tick's last value kept");
        assert_eq!(t.thinned, 3);
        // A new pass reads the controller afresh.
        t.push(&mut out, cc(0, 1, 13), &s, f);
        assert_eq!(values(&out).len(), 4);
        // Full: controllers go, notes stay until the hard limit.
        let mut t = Thinner::new(2, 3);
        let mut out = Vec::new();
        let on = |at| Captured { at, pass: 0, event: CaptureEvent::Midi(MidiMessage::NoteOn { ch: 0, note: 60, vel: 1 }) };
        for c in [on(0), on(1), cc(2, 0, 5), on(3), on(4)] {
            t.push(&mut out, c, &s, f);
        }
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| matches!(c.msg(), Some(MidiMessage::NoteOn { .. }))));
        assert_eq!(t.dropped, 2);
    }

    #[test]
    fn the_input_filter_leaves_its_kinds_out_as_they_arrive() {
        let mut s = Session::default();
        s.edit.set_flag("midi.filter.cc", true);
        let mut t = Thinner::new(10, 10);
        let mut out = Vec::new();
        let on = Captured { at: 0, pass: 0, event: CaptureEvent::Midi(MidiMessage::NoteOn { ch: 0, note: 60, vel: 1 }) };
        t.push(&mut out, cc(0, 0, 3), &s, InputFilter::of(&s));
        t.push(&mut out, on, &s, InputFilter::of(&s));
        assert_eq!((out.len(), t.filtered), (1, 1));
    }

    #[test]
    fn a_slot_recording_keeps_the_arrangement_take_to_its_jumps() {
        let s = Session::default();
        let mut t = MidiTake::new(TrackId(1), 0, 0);
        let on = Captured { at: 10, pass: 0, event: CaptureEvent::Midi(MidiMessage::NoteOn { ch: 0, note: 60, vel: 1 }) };
        let jump = Captured { at: 20, pass: 0, event: CaptureEvent::Jump { to: 0 } };
        take_in(&mut t, &[on, jump], &s, InputFilter::default(), true);
        assert_eq!(t.events, [jump], "the slot's notes are not the take's");
        take_in(&mut t, &[on], &s, InputFilter::default(), false);
        assert_eq!(t.events.len(), 2);
    }

    fn pass(events: Vec<(Samples, [u8; 3])>, end: Samples) -> KeptPass {
        KeptPass { start: 0, end, events }
    }

    #[test]
    fn a_long_pass_splits_where_no_note_is_held_else_strikes_held_notes_again() {
        // Notes of 5 samples every 10: quiet before each note-on.
        let mut ev = Vec::new();
        for i in 0..100 {
            ev.push((i * 10, [0x90, 60, 100]));
            ev.push((i * 10 + 5, [0x80, 60, 0]));
        }
        let parts = chunks(&[pass(ev.clone(), 1_000)], 30);
        let flat: Vec<&KeptPass> = parts.iter().flatten().collect();
        assert!(parts.iter().all(|g| g.iter().map(|p| p.events.len()).sum::<usize>() <= 30));
        assert_eq!(flat.iter().map(|p| p.events.len()).sum::<usize>(), 200, "nothing lost or added");
        for w in flat.windows(2) {
            assert_eq!(w[0].end, w[1].start, "parts follow each other");
            assert_eq!(w[1].events[0].1[0], 0x90, "each part starts on a note-on: split where nothing is held");
        }
        assert_eq!((flat[0].start, flat.last().unwrap().end), (0, 1_000));
        // One note held throughout under a stream of controllers: split anyway, the note struck
        // again at each part's start.
        let mut ev = vec![(0, [0x90, 40, 90])];
        ev.extend((1..100).map(|i| (i, [0xB0, 1, (i % 128) as u8])));
        ev.push((100, [0x80, 40, 0]));
        let parts = chunks(&[pass(ev, 200)], 30);
        let flat: Vec<&KeptPass> = parts.iter().flatten().collect();
        assert!(flat.len() > 3);
        for p in &flat[1..] {
            assert_eq!(p.events[0], (p.start, [0x90, 40, 90]), "struck again where the part starts");
        }
        // Short passes are grouped, not split.
        let parts = chunks(&[pass(vec![(0, [0x90, 1, 1])], 10), pass(vec![(0, [0x90, 2, 1])], 10)], 30);
        assert_eq!((parts.len(), parts[0].len()), (1, 2));
    }

    fn kept(placed: bool, n: usize) -> KeptTake {
        KeptTake { at_s: 1, track: 1, track_name: format!("T{n}"), notes: n, placed, passes: vec![] }
    }

    #[test]
    fn placed_takes_go_before_unplaced_ones_and_the_list_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("djforge-kept-takes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
        app.autosave_dir = Some(dir.clone());
        keep(&mut app, kept(false, 0));
        for n in 1..KEPT_TAKES {
            keep(&mut app, kept(true, n));
        }
        assert_eq!(app.midi_takes.len(), KEPT_TAKES);
        keep(&mut app, kept(false, 99));
        assert_eq!(app.midi_takes.len(), KEPT_TAKES);
        assert!(app.midi_takes.iter().any(|t| t.notes == 0), "the oldest unplaced take stays");
        assert!(!app.midi_takes.iter().any(|t| t.notes == 1), "the oldest placed one went");
        assert_eq!(app.midi_takes[0].notes, 99, "newest first");
        // A new run reads them back.
        let mut again = SoundApp::new(soundcraft_engine::Engine::default(), None, crate::Services::default());
        again.autosave_dir = Some(dir.clone());
        ensure_loaded(&mut again);
        assert_eq!(again.midi_takes, app.midi_takes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_label_says_when_where_and_how_many() {
        let t = KeptTake { at_s: 1_000, track: 1, track_name: "Bass".into(), notes: 16, placed: true, passes: vec![] };
        assert_eq!(t.label(1_130), "2 min ago · Bass · 16 notes");
        assert_eq!(KeptTake { placed: false, notes: 1, ..t }.label(1_000), "just now · Bass · 1 note (not placed)");
    }
}
