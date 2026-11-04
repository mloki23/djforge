/// One parsed channel-voice MIDI message. Channels are 0..=15; data values are 0..=127.
///
/// System messages (SysEx, MIDI clock, active sensing and the rest of system common / real-time)
/// are not represented: the parser drops them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiMessage {
    /// Note off. A note-on with velocity 0 also parses to this, with `vel` 0.
    NoteOff { ch: u8, note: u8, vel: u8 },
    /// Note on with velocity 1..=127.
    NoteOn { ch: u8, note: u8, vel: u8 },
    /// Polyphonic aftertouch for one note.
    PolyPressure { ch: u8, note: u8, pressure: u8 },
    /// Control change.
    ControlChange { ch: u8, cc: u8, value: u8 },
    /// Program change.
    ProgramChange { ch: u8, program: u8 },
    /// Channel (mono) aftertouch.
    ChannelPressure { ch: u8, pressure: u8 },
    /// Pitch bend, centred: -8192..=8191 (0 = no bend), as in `soundcraft-midi`'s model.
    PitchBend { ch: u8, value: i16 },
}

impl MidiMessage {
    /// The channel 0..=15.
    pub fn channel(&self) -> u8 {
        match *self {
            MidiMessage::NoteOff { ch, .. }
            | MidiMessage::NoteOn { ch, .. }
            | MidiMessage::PolyPressure { ch, .. }
            | MidiMessage::ControlChange { ch, .. }
            | MidiMessage::ProgramChange { ch, .. }
            | MidiMessage::ChannelPressure { ch, .. }
            | MidiMessage::PitchBend { ch, .. } => ch,
        }
    }

    /// The wire bytes, always with an explicit status byte (no running status). Out-of-range fields
    /// are masked to 4 / 7 / 14 bits. Allocates: meant for tests and output, not the audio thread.
    pub fn to_bytes(&self) -> Vec<u8> {
        let st = |kind: u8, ch: u8| kind | (ch & 0x0F);
        match *self {
            MidiMessage::NoteOff { ch, note, vel } => vec![st(0x80, ch), note & 0x7F, vel & 0x7F],
            MidiMessage::NoteOn { ch, note, vel } => vec![st(0x90, ch), note & 0x7F, vel & 0x7F],
            MidiMessage::PolyPressure { ch, note, pressure } => vec![st(0xA0, ch), note & 0x7F, pressure & 0x7F],
            MidiMessage::ControlChange { ch, cc, value } => vec![st(0xB0, ch), cc & 0x7F, value & 0x7F],
            MidiMessage::ProgramChange { ch, program } => vec![st(0xC0, ch), program & 0x7F],
            MidiMessage::ChannelPressure { ch, pressure } => vec![st(0xD0, ch), pressure & 0x7F],
            MidiMessage::PitchBend { ch, value } => {
                let raw = (i32::from(value).clamp(-8192, 8191) + 8192) as u16;
                vec![st(0xE0, ch), (raw & 0x7F) as u8, ((raw >> 7) & 0x7F) as u8]
            }
        }
    }
}

/// Number of data bytes after a channel-voice status byte.
fn data_len(status: u8) -> u8 {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

/// A streaming MIDI byte parser with running status.
///
/// Feed it bytes in any chunking; complete channel messages come out. It never panics and never
/// allocates. Rules:
/// - real-time bytes (0xF8..=0xFF: clock, start/stop, active sensing, reset) are dropped wherever they
///   appear, even inside another message, and leave running status alone;
/// - SysEx (0xF0 .. 0xF7) is skipped; it and the other system common messages cancel running status;
/// - data bytes with no running status (garbage, or the tail of a skipped message) are dropped;
/// - a new status byte abandons any half-received message.
#[derive(Debug, Clone, Copy, Default)]
pub struct MidiParser {
    /// Current channel-voice status (running status), if any.
    running: Option<u8>,
    /// First data byte of the message being received.
    first: Option<u8>,
    /// Inside a SysEx dump.
    in_sysex: bool,
}

impl MidiParser {
    /// A parser with no running status.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one byte; returns a message when it completes one.
    pub fn push(&mut self, byte: u8) -> Option<MidiMessage> {
        match byte {
            0xF8..=0xFF => None,
            0xF0 => {
                self.in_sysex = true;
                self.cancel();
                None
            }
            0xF1..=0xF7 => {
                self.in_sysex = false;
                self.cancel();
                None
            }
            0x80..=0xEF => {
                self.in_sysex = false;
                self.running = Some(byte);
                self.first = None;
                None
            }
            _ => {
                if self.in_sysex {
                    return None;
                }
                let status = self.running?;
                match (data_len(status), self.first) {
                    (1, _) => Some(decode(status, byte, 0)),
                    (_, None) => {
                        self.first = Some(byte);
                        None
                    }
                    (_, Some(d1)) => {
                        self.first = None;
                        Some(decode(status, d1, byte))
                    }
                }
            }
        }
    }

    /// Feeds a chunk of bytes, calling `out` for each completed message.
    pub fn feed(&mut self, bytes: &[u8], mut out: impl FnMut(MidiMessage)) {
        for &b in bytes {
            if let Some(m) = self.push(b) {
                out(m);
            }
        }
    }

    fn cancel(&mut self) {
        self.running = None;
        self.first = None;
    }
}

/// Builds a message from a channel-voice status and its data bytes (both already < 0x80).
fn decode(status: u8, d1: u8, d2: u8) -> MidiMessage {
    let ch = status & 0x0F;
    match status & 0xF0 {
        0x80 => MidiMessage::NoteOff { ch, note: d1, vel: d2 },
        0x90 if d2 == 0 => MidiMessage::NoteOff { ch, note: d1, vel: 0 },
        0x90 => MidiMessage::NoteOn { ch, note: d1, vel: d2 },
        0xA0 => MidiMessage::PolyPressure { ch, note: d1, pressure: d2 },
        0xB0 => MidiMessage::ControlChange { ch, cc: d1, value: d2 },
        0xC0 => MidiMessage::ProgramChange { ch, program: d1 },
        0xD0 => MidiMessage::ChannelPressure { ch, pressure: d1 },
        // 0xE0: the only status left (the caller only passes 0x80..=0xEF).
        _ => MidiMessage::PitchBend { ch, value: ((u16::from(d2) << 7) | u16::from(d1)) as i16 - 8192 },
    }
}

/// Parses a complete byte slice with a fresh parser. Allocates the result; for tests and tools.
pub fn parse_all(bytes: &[u8]) -> Vec<MidiMessage> {
    let mut out = Vec::new();
    MidiParser::new().feed(bytes, |m| out.push(m));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn all_kinds() -> Vec<MidiMessage> {
        vec![
            MidiMessage::NoteOff { ch: 0, note: 60, vel: 64 },
            MidiMessage::NoteOn { ch: 15, note: 127, vel: 1 },
            MidiMessage::PolyPressure { ch: 3, note: 40, pressure: 99 },
            MidiMessage::ControlChange { ch: 9, cc: 7, value: 127 },
            MidiMessage::ProgramChange { ch: 1, program: 81 },
            MidiMessage::ChannelPressure { ch: 2, pressure: 0 },
            MidiMessage::PitchBend { ch: 4, value: -8192 },
            MidiMessage::PitchBend { ch: 4, value: 0 },
            MidiMessage::PitchBend { ch: 4, value: 8191 },
        ]
    }

    #[test]
    fn parses_every_message_type() {
        for m in all_kinds() {
            assert_eq!(parse_all(&m.to_bytes()), vec![m], "{m:?}");
        }
        assert_eq!(parse_all(&[0x92, 0x3C, 0x50]), vec![MidiMessage::NoteOn { ch: 2, note: 60, vel: 80 }]);
        assert_eq!(parse_all(&[0xB1, 0x40, 0x7F]), vec![MidiMessage::ControlChange { ch: 1, cc: 64, value: 127 }]);
        // Centre, LSB/MSB order.
        assert_eq!(parse_all(&[0xE0, 0x00, 0x40]), vec![MidiMessage::PitchBend { ch: 0, value: 0 }]);
        assert_eq!(parse_all(&[0xE0, 0x01, 0x40]), vec![MidiMessage::PitchBend { ch: 0, value: 1 }]);
    }

    #[test]
    fn messages_in_one_chunk_all_come_out() {
        let bytes: Vec<u8> = all_kinds().iter().flat_map(|m| m.to_bytes()).collect();
        assert_eq!(parse_all(&bytes), all_kinds());
    }

    #[test]
    fn running_status_across_messages_and_chunks() {
        // One status, three note-ons; the third spans a chunk boundary.
        let mut p = MidiParser::new();
        let mut got = Vec::new();
        p.feed(&[0x90, 60, 100, 62, 101, 64], |m| got.push(m));
        p.feed(&[102], |m| got.push(m));
        assert_eq!(
            got,
            vec![
                MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 },
                MidiMessage::NoteOn { ch: 0, note: 62, vel: 101 },
                MidiMessage::NoteOn { ch: 0, note: 64, vel: 102 },
            ]
        );
        // One-data-byte messages run too.
        assert_eq!(
            parse_all(&[0xC5, 1, 2]),
            vec![MidiMessage::ProgramChange { ch: 5, program: 1 }, MidiMessage::ProgramChange { ch: 5, program: 2 }]
        );
    }

    #[test]
    fn note_on_velocity_zero_is_note_off() {
        assert_eq!(parse_all(&[0x93, 60, 0]), vec![MidiMessage::NoteOff { ch: 3, note: 60, vel: 0 }]);
        // The usual running-status idiom: on, "off" as on/0.
        assert_eq!(
            parse_all(&[0x90, 60, 100, 60, 0]),
            vec![MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 }, MidiMessage::NoteOff { ch: 0, note: 60, vel: 0 }]
        );
    }

    #[test]
    fn real_time_bytes_are_ignored_without_breaking_running_status() {
        let bytes = [0xF8, 0x90, 0xF8, 60, 0xFE, 100, 0xF8, 62, 0xFA, 0xFC, 101, 0xFF];
        assert_eq!(parse_all(&bytes), vec![MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 }, MidiMessage::NoteOn { ch: 0, note: 62, vel: 101 }]);
    }

    #[test]
    fn sysex_and_system_common_are_skipped_and_cancel_running_status() {
        // SysEx body (data bytes) is dropped; after it, data bytes without a status are dropped too.
        assert_eq!(parse_all(&[0x90, 60, 100, 0xF0, 0x7E, 0x00, 0x09, 0xF7, 61, 100]), vec![MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 }]);
        // A status byte inside an unterminated SysEx ends it.
        assert_eq!(parse_all(&[0xF0, 1, 2, 0xB0, 7, 100]), vec![MidiMessage::ControlChange { ch: 0, cc: 7, value: 100 }]);
        // Song position pointer and its data: nothing.
        assert!(parse_all(&[0xF2, 0x10, 0x20]).is_empty());
    }

    #[test]
    fn truncated_and_orphan_bytes_yield_nothing() {
        assert!(parse_all(&[0x90]).is_empty());
        assert!(parse_all(&[0x90, 60]).is_empty());
        assert!(parse_all(&[60, 100, 1, 2]).is_empty());
        assert!(parse_all(&[]).is_empty());
        // A half message abandoned by a new status.
        assert_eq!(parse_all(&[0x90, 60, 0xB0, 1, 2]), vec![MidiMessage::ControlChange { ch: 0, cc: 1, value: 2 }]);
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic_and_output_is_in_range(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            for m in parse_all(&bytes) {
                prop_assert!(m.channel() < 16);
                // Every parsed message re-encodes to bytes that parse back to itself.
                prop_assert_eq!(parse_all(&m.to_bytes()), vec![m]);
            }
        }

        #[test]
        fn chunking_does_not_change_the_result(bytes in proptest::collection::vec(any::<u8>(), 0..256), cut in 0usize..256) {
            let cut = cut.min(bytes.len());
            let (a, b) = bytes.split_at(cut);
            let mut p = MidiParser::new();
            let mut got = Vec::new();
            p.feed(a, |m| got.push(m));
            p.feed(b, |m| got.push(m));
            prop_assert_eq!(got, parse_all(&bytes));
        }
    }
}
