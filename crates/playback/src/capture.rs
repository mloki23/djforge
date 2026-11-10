//! Capturing the live MIDI the target plays, for recording takes.
//!
//! While the transport plays, the audio thread copies each live event the target's instrument
//! plays (a MIDI port's or the on-screen keyboard's) as a [`Captured`]: the event and the exact
//! timeline sample it played on, the block's position plus its offset in the block (clamped into
//! the block, as the instrument plays it). So a take lines up with what was heard: the live
//! latency `L` is already in where the event landed.
//!
//! It captures while recording (`Player::set_recording`) and the target track is record-armed,
//! or, for a Session view slot recording, whenever the transport plays
//! ([`crate::Player::capture_midi_while_playing`]). The queue is a bounded lock-free ring made off
//! the audio thread ([`crate::Player::midi_capture`]); an event that finds it full is dropped and
//! counted ([`crate::Player::midi_capture_dropped`]). The queue it replaces goes back to be
//! dropped off the audio thread. Transport jumps while capturing are marked in the queue, and
//! each event carries its pass (see [`Captured`]).

use soundcraft_midi_io::MidiMessage;
use soundcraft_time::Samples;

/// Events a capture queue holds by default between two UI frames (far more than hands play).
pub const CAPTURE_CAPACITY: usize = 16_384;

/// One live event as it played (or a jump of the transport), at timeline sample `at`, in pass
/// `pass`.
///
/// A pass is a stretch of timeline played without a jump: a loop wrap, a locate or a play from
/// elsewhere while playing ends it, marked by a [`CaptureEvent::Jump`] at the sample it left
/// (in the pass it ends). So loop passes, which replay the same samples, stay apart. Passes count
/// up from wherever the player is (compare them, don't read them as numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Captured {
    pub at: Samples,
    pub pass: u32,
    pub event: CaptureEvent,
}

/// What was captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEvent {
    /// A live message the target played.
    Midi(MidiMessage),
    /// The transport jumped from `at` to `to`: the next pass starts there.
    Jump { to: Samples },
}

impl Captured {
    /// The message, for a live event.
    pub fn msg(&self) -> Option<MidiMessage> {
        match self.event {
            CaptureEvent::Midi(m) => Some(m),
            CaptureEvent::Jump { .. } => None,
        }
    }
}

/// The UI thread's end of a capture queue.
pub struct MidiCapture {
    pub(crate) rx: rtrb::Consumer<Captured>,
}

impl MidiCapture {
    /// The next event captured, oldest first.
    pub fn pop(&mut self) -> Option<Captured> {
        self.rx.pop().ok()
    }

    /// Moves every event waiting into `out`.
    pub fn drain_into(&mut self, out: &mut Vec<Captured>) {
        while let Some(c) = self.pop() {
            out.push(c);
        }
    }

    /// Events waiting.
    pub fn len(&self) -> usize {
        self.rx.slots()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A new capture queue of `capacity` events: the audio thread's end and the UI's.
pub(crate) fn queue(capacity: usize) -> (rtrb::Producer<Captured>, MidiCapture) {
    let (tx, rx) = rtrb::RingBuffer::new(capacity.max(1));
    (tx, MidiCapture { rx })
}

/// One pass of a take: from `start` to `end` without a jump, and what was played in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakePass {
    pub start: Samples,
    pub end: Samples,
    pub events: Vec<(Samples, MidiMessage)>,
}

/// A take captured from `start` until the player rendered to `end`, split into its passes at
/// the transport's jumps (see [`Captured`]), in the order played. Passes that played nothing
/// are kept (an empty stretch is still where the transport went), but none is empty of time.
pub fn passes(start: Samples, end: Samples, captured: &[Captured]) -> Vec<TakePass> {
    let mut out = Vec::new();
    let mut cur = TakePass { start, end, events: Vec::new() };
    let mut pass = captured.first().map(|c| c.pass);
    for c in captured {
        if pass != Some(c.pass) {
            // A jump whose marker was dropped: the pass ends at its last event.
            let last = cur.events.last().map_or(cur.start, |e| e.0.saturating_add(1));
            let next = TakePass { start: c.at, end, events: Vec::new() };
            out.push(TakePass { end: last.max(cur.start.saturating_add(1)), ..std::mem::replace(&mut cur, next) });
            pass = Some(c.pass);
        }
        match c.event {
            CaptureEvent::Midi(m) => cur.events.push((c.at, m)),
            CaptureEvent::Jump { to } => {
                let next = TakePass { start: to, end, events: Vec::new() };
                out.push(TakePass { end: c.at, ..std::mem::replace(&mut cur, next) });
                pass = Some(c.pass.wrapping_add(1));
            }
        }
    }
    out.push(cur);
    out.retain(|p| p.end > p.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(at: Samples, pass: u32) -> Captured {
        Captured { at, pass, event: CaptureEvent::Midi(MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 }) }
    }

    #[test]
    fn a_take_splits_into_passes_at_its_jumps() {
        let jump = |at, pass, to| Captured { at, pass, event: CaptureEvent::Jump { to } };
        let msg = MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 };
        // Loop 0..100 from 30, two wraps, stopped at 50.
        let got = passes(30, 50, &[on(40, 7), jump(100, 7, 0), on(10, 8), jump(100, 8, 0), on(20, 9)]);
        assert_eq!(
            got,
            [
                TakePass { start: 30, end: 100, events: vec![(40, msg)] },
                TakePass { start: 0, end: 100, events: vec![(10, msg)] },
                TakePass { start: 0, end: 50, events: vec![(20, msg)] },
            ]
        );
        // A dropped jump marker: the pass changes anyway, ending after its last event.
        let got = passes(0, 500, &[on(40, 1), on(300, 2)]);
        assert_eq!(got.iter().map(|p| (p.start, p.end)).collect::<Vec<_>>(), [(0, 41), (300, 500)]);
        assert_eq!(passes(0, 10, &[]), [TakePass { start: 0, end: 10, events: vec![] }]);
    }
}
