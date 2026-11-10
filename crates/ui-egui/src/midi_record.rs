//! Recording live MIDI: takes into the arrangement and Session view slot recordings.
//!
//! The player captures each live event the target plays, at the timeline sample it played on
//! (`soundcraft_playback::capture`). [`frame`] drains that queue every UI frame into what is
//! recording:
//!
//! - **Arrangement takes.** Recording (Record + Play) with the live MIDI target record-armed
//!   collects its events; when recording stops, `midi.record_take` turns them into one clip (one
//!   undo step). A take over another clip is refused and the app offers Replace (or MIDI Merge
//!   overdubs it, see the engine's `midi_take` docs). Recording needs no audio input when only
//!   an instrument track is armed.
//! - **Slot recordings** (`live.record_slot`). Armed on an empty slot of an instrument column,
//!   the column's track is the live target ([`target_override`]: the old target comes back
//!   afterwards). Capture starts at the next launch boundary `start` and runs for the slot's
//!   bars, to `end`. A slot loops seamlessly only if it launches exactly at `end`, and the audio
//!   thread must have the launch before it renders `end`: so the slot is written
//!   (`live.slot_from_take`) and its launch at `end` sent once the player has rendered to
//!   [`write_margin`] before `end` (the launch margin plus a UI frame). What is played in that
//!   last stretch (about 120 ms: notes started, notes released) is written in once the player
//!   has rendered past `end`, merged into the same undo step; a playing slot edited away from
//!   its playhead plays on without a cut, so the loop's first pass is complete and nothing it
//!   sounds is interrupted. If a frame comes so late that the player is past the margin, the
//!   launch lands late (the audio thread keeps its phase, and the status line says so).
//!   Stop or Esc cancels.

use crate::SoundApp;
use crate::produce::offer_replace;
use serde_json::{Value, json};
use soundcraft_engine::cmd::live_launch_margin;
use soundcraft_model::{Session, TrackId};
use soundcraft_playback::{CAPTURE_CAPACITY, Captured, midi_in};
use soundcraft_time::{Samples, TICKS_PER_QUARTER};

/// Beyond the launch margin, a slot is written this long before its end: one slow UI frame.
pub const FRAME_GUARD_S: f64 = 0.05;

/// The undo-merge key both writes of one slot recording share.
fn merge_key(r: &SlotRecording) -> String {
    format!("slot_take:{}:{}:{}", r.column, r.slot, r.start)
}

/// An arrangement take being recorded on `track` from `start`.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiTake {
    pub track: TrackId,
    pub start: Samples,
    pub events: Vec<Captured>,
}

/// A Session view slot recording (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct SlotRecording {
    pub column: usize,
    pub slot: usize,
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
    /// Its launch at `end` was sent.
    pub launched: bool,
    /// The player has reported playing since it was armed.
    pub seen_playing: bool,
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
        SlotRecording { column, slot, bars, replace, track, start, end, events: Vec::new(), written: None, launched: false, seen_playing: false }
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

    /// Keeps what was played inside the recording.
    pub fn collect(&mut self, events: &[Captured]) {
        self.events.extend(events.iter().filter(|c| (self.start..self.end).contains(&c.at)));
    }

    /// How far it is, 0..=1, with the player at `pos` (0 while armed).
    pub fn progress(&self, pos: Samples) -> f32 {
        let len = (self.end - self.start).max(1) as f64;
        ((pos - self.start) as f64 / len).clamp(0.0, 1.0) as f32
    }

    /// The `live.slot_from_take` parameters for what was played so far.
    pub fn params(&self, replace: bool) -> Value {
        json!({"column": self.column, "slot": self.slot, "start": self.start, "bars": self.bars, "events": events_json(&self.events),
            "replace": replace})
    }
}

/// How long before a slot recording's end it is written: the launch margin for the player's
/// largest callback (`callback`, session samples) plus [`FRAME_GUARD_S`].
pub fn write_margin(s: &Session, callback: Samples) -> Samples {
    live_launch_margin(s, callback).saturating_add(s.sample_rate.samples(FRAME_GUARD_S))
}

/// Captured events as the engine's take commands take them.
pub fn events_json(events: &[Captured]) -> Value {
    Value::Array(
        events
            .iter()
            .map(|c| {
                let [status, data1, data2] = midi_in::raw(c.msg);
                json!({"at": c.at, "status": status, "data1": data1, "data2": data2})
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
    let notes = match r.step(pos, margin) {
        SlotStep::Wait => return,
        SlotStep::Write => match write_slot(app, &r, r.replace) {
            Ok(n) => {
                let launched = n > 0 && launch(app, &r);
                if let Some(x) = &mut app.slot_recording {
                    (x.written, x.launched) = (Some(r.events.len()), launched);
                }
                return;
            }
            Err(why) => return cancel_slot(app, &why),
        },
        SlotStep::Finish if r.written == Some(r.events.len()) => Ok(slot_notes(app, &r)),
        // Played since the first write (or nothing written yet: a frame came after the end).
        // The first write, if any, is our own: the whole take replaces it.
        SlotStep::Finish => write_slot(app, &r, r.replace || r.written.is_some()),
    };
    match notes {
        Ok(n) => {
            if n > 0 && !r.launched {
                // Late: it keeps its phase from `end`.
                launch(app, &r);
            }
            finish_slot(app, (n > 0).then_some((n, r.column, r.slot)));
        }
        Err(why) => cancel_slot(app, &why),
    }
}

/// Sends the slot's launch at the recording's end.
fn launch(app: &SoundApp, r: &SlotRecording) -> bool {
    let Some(p) = &app.player else { return false };
    p.live(soundcraft_mix::LiveCmd::Launch { column: r.column, slot: r.slot, at: r.end });
    true
}

/// Notes in the slot the recording writes.
fn slot_notes(app: &SoundApp, r: &SlotRecording) -> usize {
    app.engine.session().live.slot(r.column, r.slot).and_then(|x| x.midi()).map_or(0, |m| m.sequence.notes.len())
}

/// Writes what the recording holds into its slot (one undo step for both writes); the notes
/// written, or why not.
fn write_slot(app: &mut SoundApp, r: &SlotRecording, replace: bool) -> Result<usize, String> {
    let v = app.engine.execute_merged("live.slot_from_take", &r.params(replace), &merge_key(r)).map_err(|e| e.to_string())?;
    if let Some(p) = &app.player {
        p.update_session(app.engine.session_arc());
    }
    Ok(v.get("notes").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0))
}

fn finish_slot(app: &mut SoundApp, recorded: Option<(usize, usize, usize)>) {
    app.slot_recording = None;
    app.engine.end_merge();
    if let Some(p) = &app.player {
        p.capture_midi_while_playing(false);
    }
    app.ui.status = match recorded {
        Some((n, column, slot)) => format!("Recorded {n} note(s) into slot {} of Live {}: looping", slot + 1, column + 1),
        None => "Nothing recorded into the slot: no notes were played".into(),
    };
}

/// Stops a slot recording (Stop, Esc, or a refusal): what was already written stays.
pub fn cancel_slot(app: &mut SoundApp, why: &str) {
    if app.slot_recording.take().is_none() {
        return;
    }
    app.engine.end_merge();
    if let Some(p) = &app.player {
        p.capture_midi_while_playing(false);
    }
    app.ui.status = why.to_string();
}

/// Arms a slot recording from the boundary `at` (see [`crate::SoundApp::live_request`]).
pub fn arm_slot(
    app: &mut SoundApp,
    column: usize,
    slot: usize,
    bars: u32,
    replace: bool,
    at: Samples,
    looped: Option<soundcraft_time::Range>,
) -> bool {
    let s = app.engine.session();
    let Some(track) = s.live.column_track(column, &s.tracks) else {
        app.ui.status = format!("Live column {} has lost its track", column + 1);
        return false;
    };
    let r = SlotRecording::new(s, column, slot, bars, replace, track, at);
    if looped.is_some_and(|l| r.end > l.end) {
        app.ui.status = "The slot recording would run past the loop's end: turn off Loop Playback or loop a longer selection".into();
        return false;
    }
    if let Some(old) = &app.slot_recording {
        let why = format!("Slot recording into slot {} of Live {} replaced", old.slot + 1, old.column + 1);
        cancel_slot(app, &why);
    }
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
    app.midi_take = Some(MidiTake { track, start, events: Vec::new() });
    true
}

/// Recording stopped with the player rendered to `end` (and the punch range `punch`, if any):
/// the take becomes a clip. Returns the status to show, if a take was recording.
pub fn finish_take(app: &mut SoundApp, end: Samples, punch: Option<soundcraft_time::Range>) -> Option<String> {
    let mut take = app.midi_take.take()?;
    if let Some(c) = &mut app.midi_capture {
        c.drain_into(&mut take.events);
    }
    let (start, end) = match punch {
        Some(r) => (take.start.max(r.start), end.min(r.end)),
        None => (take.start, end),
    };
    if end <= start {
        return Some("Nothing recorded: the MIDI take was empty".into());
    }
    let params = json!({"track": take.track.0, "start": start, "end": end, "events": events_json(&take.events)});
    Some(match app.run_typed("midi.record_take", params.clone()) {
        Ok(v) => match v.get("clip") {
            Some(Value::Null) | None => v.get("message").and_then(Value::as_str).unwrap_or("Nothing recorded").to_string(),
            Some(_) => {
                let name = app.engine.session().track(take.track).map(|t| t.name.clone()).unwrap_or_default();
                let how = if v["merged"] == true { "merged into the clip on" } else { "on" };
                format!("Recorded {} MIDI note(s) {how} {name}", v["notes"])
            }
        },
        Err(e) if e.would_replace() => {
            offer_replace(app, "midi.record_take", params, &e);
            format!("The MIDI take overlaps a clip: {e}")
        }
        Err(e) => format!("MIDI take not recorded: {e}"),
    })
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
    fn only_what_is_played_inside_the_recording_is_kept() {
        let mut r = rec();
        let on = |at| Captured { at, msg: MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 } };
        r.collect(&[on(95_999), on(96_000), on(191_999), on(192_000)]);
        assert_eq!(r.events, [on(96_000), on(191_999)]);
        let p = r.params(true);
        assert_eq!(p["events"][0], json!({"at": 96_000, "status": 0x90, "data1": 60, "data2": 100}));
        assert_eq!((p["replace"].as_bool(), p["bars"].as_u64()), (Some(true), Some(1)));
    }
}
