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
//! dropped off the audio thread.

use soundcraft_midi_io::MidiMessage;
use soundcraft_time::Samples;

/// Events a capture queue holds by default between two UI frames (far more than hands play).
pub const CAPTURE_CAPACITY: usize = 16_384;

/// One live event as it played: the timeline sample and the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Captured {
    pub at: Samples,
    pub msg: MidiMessage,
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
