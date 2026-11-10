//! Recording live MIDI: takes into the arrangement and Session view slot recordings. Nothing
//! played is ever lost: a take that can't go where it was meant to goes on a new playlist, or
//! waits for Recover Last MIDI Take.
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
//!   merged into the clip. The last take is kept: `midi.recover_take` (Event › Recover Last MIDI
//!   Take) places it again, on a new playlist. Recording needs no audio input when only an
//!   instrument track is armed.
//! - **Slot recordings** (`live.record_slot`). Armed on an empty slot of an instrument column,
//!   the column's track is the live target ([`target_override`]: the old target comes back
//!   afterwards). Capture starts at the next launch boundary `start` and runs for the slot's
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
//!   slot, or, with none, waits for Recover Last MIDI Take. Stop or Esc cancels (a take already
//!   played is kept for recovery).

use crate::SoundApp;
use serde_json::{Value, json};
use soundcraft_engine::cmd::live_launch_margin;
use soundcraft_model::{Session, TrackId};
use soundcraft_playback::{CAPTURE_CAPACITY, CaptureEvent, Captured, TakePass, midi_in, take_passes};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER};

/// Beyond the launch margin, a slot is written this long before its end: one slow UI frame.
pub const FRAME_GUARD_S: f64 = 0.05;

/// The undo group both writes of a slot recording make.
const SLOT_GROUP: &str = "Record Slot Take";

/// An arrangement take being recorded on `track` from `start`.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiTake {
    pub track: TrackId,
    pub start: Samples,
    pub events: Vec<Captured>,
    /// The player's capture drops when it started.
    pub dropped: u64,
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
            written: None,
            ours: false,
            launched: false,
            seen_playing: false,
            jumped: false,
            dropped: 0,
            late: 0,
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

    /// Keeps what was played inside the recording; a jump of the transport after its start
    /// marks it as jumped.
    pub fn collect(&mut self, events: &[Captured]) {
        for c in events {
            match c.event {
                CaptureEvent::Midi(_) if (self.start..self.end).contains(&c.at) => self.events.push(*c),
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

    /// The take as an arrangement take on the column's track, for Recover Last MIDI Take.
    pub fn as_take(&self) -> Value {
        let pass = TakePass { start: self.start, end: self.end, events: self.events.iter().filter_map(|c| c.msg().map(|m| (c.at, m))).collect() };
        json!({"track": self.track.0, "passes": passes_json(&[pass]), "on_overlap": "new_playlist"})
    }
}

/// How long before a slot recording's end it is written: the launch margin for the player's
/// largest callback (`callback`, session samples) plus [`FRAME_GUARD_S`].
pub fn write_margin(s: &Session, callback: Samples) -> Samples {
    live_launch_margin(s, callback).saturating_add(s.sample_rate.samples(FRAME_GUARD_S))
}

fn event_json(at: Samples, m: soundcraft_midi_io::MidiMessage) -> Value {
    let [status, data1, data2] = midi_in::raw(m);
    json!({"at": at, "status": status, "data1": data1, "data2": data2})
}

/// Captured live events as the engine's take commands take them (jumps left out).
pub fn events_json(events: &[Captured]) -> Value {
    Value::Array(events.iter().filter_map(|c| c.msg().map(|m| event_json(c.at, m))).collect())
}

/// Passes as `midi.record_take` takes them.
pub fn passes_json(passes: &[TakePass]) -> Value {
    Value::Array(
        passes
            .iter()
            .map(|p| {
                let events: Vec<Value> = p.events.iter().map(|(at, m)| event_json(*at, *m)).collect();
                json!({"start": p.start, "end": p.end, "events": events})
            })
            .collect(),
    )
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

/// ", N events dropped" when the capture dropped some since `since`.
fn dropped_note(app: &SoundApp, since: u64) -> String {
    match drops(app).saturating_sub(since) {
        0 => String::new(),
        n => format!(" ({n} live events were dropped: the capture queue was full)"),
    }
}

/// Every UI frame, after the transport requests (see the module docs).
pub fn frame(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(p) = &app.player else { return };
    if !p.has_midi_capture() {
        app.midi_capture = Some(p.midi_capture(CAPTURE_CAPACITY));
    }
    // Where the player has rendered to, read before draining: everything played before it is in.
    let (pos, playing) = (p.position(), p.is_playing());
    let mut got = Vec::new();
    if let Some(c) = &mut app.midi_capture {
        c.drain_into(&mut got);
    }
    if let Some(t) = &mut app.midi_take {
        t.events.extend_from_slice(&got);
    }
    let Some(r) = &mut app.slot_recording else { return };
    r.collect(&got);
    ctx.request_repaint();
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
            app.engine.begin_undo_group(SLOT_GROUP);
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
            app.engine.begin_undo_group(SLOT_GROUP);
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
    let dropped = dropped_note(app, r.dropped);
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

/// The slot couldn't be written (`why`): the take waits for Recover Last MIDI Take.
fn keep_for_recovery(app: &mut SoundApp, r: &SlotRecording, why: &str) {
    if r.events.iter().any(|c| c.msg().is_some()) {
        app.last_take = Some(r.as_take());
        cancel_slot(app, &format!("Slot take not written: {why}. Event › Recover Last MIDI Take puts it on the track"));
    } else {
        cancel_slot(app, &format!("Slot recording stopped: {why}"));
    }
}

/// Stops a slot recording (Stop, Esc, a jump, a refusal): what was already written stays, and
/// what was played but not written is kept for Recover Last MIDI Take.
pub fn cancel_slot(app: &mut SoundApp, why: &str) {
    let Some(r) = app.slot_recording.take() else { return };
    app.engine.commit_undo_group();
    if let Some(p) = &app.player {
        p.capture_midi_while_playing(false);
    }
    let unwritten = r.written.is_none_or(|n| n < r.events.len()) && r.events.iter().any(|c| c.msg().is_some());
    if unwritten && app.last_take.as_ref() != Some(&r.as_take()) {
        app.last_take = Some(r.as_take());
        app.ui.status = format!("{why} (what was played is kept: Event › Recover Last MIDI Take)");
    } else {
        app.ui.status = why.to_string();
    }
}

/// Arms a slot recording from the boundary `at` (see [`crate::SoundApp::live_request`]).
pub fn arm_slot(app: &mut SoundApp, column: usize, slot: usize, bars: u32, replace: bool, at: Samples, looped: Option<Range>) -> bool {
    let s = app.engine.session();
    let Some(track) = s.live.column_track(column, &s.tracks) else {
        app.ui.status = format!("Live column {} has lost its track", column + 1);
        return false;
    };
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
    app.midi_take = Some(MidiTake { track, start, events: Vec::new(), dropped: drops(app) });
    true
}

/// Recording stopped with the player rendered to `end` (and the punch range `punch`, if any):
/// the take's passes become clips, a pass over another clip on a new playlist. Returns the
/// status to show, if a take was recording.
pub fn finish_take(app: &mut SoundApp, end: Samples, punch: Option<Range>) -> Option<String> {
    let mut take = app.midi_take.take()?;
    if let Some(c) = &mut app.midi_capture {
        c.drain_into(&mut take.events);
    }
    let mut passes = take_passes(take.start, end, &take.events);
    if let Some(r) = punch {
        for p in &mut passes {
            (p.start, p.end) = (p.start.max(r.start), p.end.min(r.end));
        }
        passes.retain(|p| p.end > p.start);
    }
    let dropped = dropped_note(app, take.dropped);
    if passes.is_empty() {
        return Some(format!("Nothing recorded: the MIDI take was empty{dropped}"));
    }
    let params = json!({"track": take.track.0, "passes": passes_json(&passes), "on_overlap": "new_playlist"});
    app.last_take = Some(params.clone());
    Some(match app.run_typed("midi.record_take", params) {
        Ok(v) => format!("{}{dropped}", v.get("message").and_then(Value::as_str).unwrap_or("Recorded the MIDI take")),
        Err(e) => format!("MIDI take not placed: {e}. Event › Recover Last MIDI Take tries again{dropped}"),
    })
}

/// `midi.recover_take {}`: places the last MIDI take (an arrangement take, or a slot take that
/// could not be written) again, on a new playlist of its track. One undo step.
pub fn recover(app: &mut SoundApp) -> Result<Value, String> {
    let mut p = app.last_take.clone().ok_or("no MIDI take to recover: nothing was recorded since SoundCraft started")?;
    if let Some(o) = p.as_object_mut() {
        o.insert("new_playlist".into(), json!(true));
    }
    let v = app.run("midi.record_take", p)?;
    app.ui.status = v.get("message").and_then(Value::as_str).unwrap_or("Recovered the MIDI take").to_string();
    Ok(v)
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
        r.collect(&[on(95_999), on(96_000), on(191_999), on(192_000)]);
        assert_eq!(r.events, [on(96_000), on(191_999)]);
        let p = r.params(3, true);
        assert_eq!(p["events"][0], json!({"at": 96_000, "status": 0x90, "data1": 60, "data2": 100}));
        assert_eq!((p["replace"].as_bool(), p["bars"].as_u64(), p["slot"].as_u64()), (Some(true), Some(1), Some(3)));
        assert!(!r.jumped);
        r.collect(&[Captured { at: 150_000, pass: 0, event: CaptureEvent::Jump { to: 0 } }]);
        assert!(r.jumped);
        let take = r.as_take();
        assert_eq!((take["passes"][0]["start"].as_i64(), take["passes"][0]["events"].as_array().map(Vec::len)), (Some(96_000), Some(2)));
    }
}
