//! Live MIDI input on the audio thread: placing timed events in the blocks rendered.
//!
//! **Timing.** Events are stamped on a [`MidiClock`] when they arrive. At the start of each
//! audio callback the clock is read (`callback_ns`), and an event sounds `L` after it arrived:
//! at session sample `(time_ns + L − callback_ns) · rate` from the first sample rendered in that
//! callback (`rate`: session samples per second at the current speed). Audio rendered ahead in
//! an earlier callback and not yet played comes first, so the first sample rendered in this one
//! sits that far into it. `L` is one callback plus [`MIDI_SLACK_NS`]: an event arrives during the
//! callback before the one that plays it, so with `L` one callback long it lands at a steady
//! delay instead of on the next block boundary (jitter-free), and the slack absorbs
//! callback-start jitter.
//!
//! "One callback" is the longest of the last [`WINDOW`] callbacks, capped at twice their median
//! and at [`MAX_SPAN_NS`]: a single long callback (a device hiccup) raises `L` only for a while,
//! and the first [`SKIP`] callbacks (a device filling its buffers at start) don't count. `L` is
//! reported by [`crate::Player::midi_latency_ns`]. Events whose sample is already past go at the
//! block's start.
//!
//! **Pending.** An event due after the block being rendered waits in a fixed buffer of
//! [`PENDING`] events for a later block. When the buffer is full the new event is dropped and
//! counted, except a note-off, which takes the place of the newest pending event that is not a
//! note-off (that one is dropped and counted instead), so a full buffer never strands a note.
//!
//! **Order.** Events of one port come in order, and never overtake each other: once an event of
//! a port waits, the port's later events wait behind it, and offsets are clamped to be
//! non-decreasing per port within a block. Different ports are independent.
//!
//! **Controls.** Notes and controllers mapped to controls (MIDI learn) don't play the target:
//! the [`MidiControls`] filter, shared with the UI thread through atomics, drops their note-ons,
//! poly pressure and control changes as they arrive. Note-offs always pass, so a note that was
//! sounding when its key got mapped is still released.

use soundcraft_midi_io::{MidiClock, MidiEvent, MidiMessage, PortId, Subscriber};
use soundcraft_mix::{LiveEvent, MixEngine};
use std::sync::atomic::{AtomicU64, Ordering};

/// Added to the longest callback to make the live MIDI latency `L` (2 ms).
pub const MIDI_SLACK_NS: u64 = 2_000_000;

/// Events that can wait for a later block.
pub const PENDING: usize = 256;

/// Callbacks whose lengths set `L` (about 0.7 s of 128-frame callbacks, 2.7 s of 512-frame ones).
pub const WINDOW: usize = 256;

/// Callbacks at the start that don't count towards `L`.
pub const SKIP: u32 = 4;

/// The longest callback `L` allows for (50 ms).
pub const MAX_SPAN_NS: u64 = 50_000_000;

/// The live-event source id of the on-screen keyboard (no MIDI port uses it).
pub const KEYBOARD_SOURCE: u32 = u32::MAX;

/// Ports tracked per block for ordering; events of further ports are ordered conservatively.
const PORTS: usize = 16;

/// Ports [`MidiControls`] can filter; mappings on further ports also play the target.
pub const CONTROL_PORTS: usize = 16;

/// One bit per channel and note (the first 2048), then per channel and controller.
const CONTROL_WORDS: usize = 2 * 16 * 128 / 64;

/// What a mapped message is, for [`MidiControls`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Control {
    Note { ch: u8, note: u8 },
    Cc { ch: u8, cc: u8 },
}

impl Control {
    fn bit(self) -> usize {
        match self {
            Control::Note { ch, note } => usize::from(ch & 0x0F) * 128 + usize::from(note & 0x7F),
            Control::Cc { ch, cc } => 2048 + usize::from(ch & 0x0F) * 128 + usize::from(cc & 0x7F),
        }
    }

    /// The control `msg` belongs to (`None`: note-offs, which always pass, and the rest).
    fn of(msg: &MidiMessage) -> Option<Control> {
        match *msg {
            MidiMessage::NoteOn { ch, note, .. } | MidiMessage::PolyPressure { ch, note, .. } => Some(Control::Note { ch, note }),
            MidiMessage::ControlChange { ch, cc, .. } => Some(Control::Cc { ch, cc }),
            _ => None,
        }
    }
}

/// The notes and controllers, per port, that are mapped to controls and so don't play the live
/// target. The UI thread writes it ([`MidiControls::set`], one writer); the audio thread reads it
/// as events arrive, wait-free. A port keeps its row while it stays in the set, so changing its
/// bits never hides it; a change is not atomic as a whole: an event arriving during one may be
/// judged by the old or the new bits.
pub struct MidiControls {
    /// The port each row filters, as `port + 1` (0: a free row), so no port id can match a free
    /// row (not even [`KEYBOARD_SOURCE`]'s).
    ports: [AtomicU64; CONTROL_PORTS],
    bits: [[AtomicU64; CONTROL_WORDS]; CONTROL_PORTS],
}

impl Default for MidiControls {
    fn default() -> Self {
        MidiControls {
            ports: std::array::from_fn(|_| AtomicU64::new(FREE)),
            bits: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

/// A free row of [`MidiControls`].
const FREE: u64 = 0;

fn row_tag(p: PortId) -> u64 {
    u64::from(p.0) + 1
}

impl MidiControls {
    /// Replaces the set with `controls`. A port already in the set keeps its row; a new one takes
    /// a free row. Returns false when ports beyond [`CONTROL_PORTS`] were left out (their mapped
    /// messages still play the target).
    pub fn set(&self, controls: &[(PortId, Control)]) -> bool {
        let mut wanted: Vec<PortId> = controls.iter().map(|(p, _)| *p).collect();
        wanted.sort_unstable();
        wanted.dedup();
        // Rows whose port left the set are freed first.
        for port in &self.ports {
            let tag = port.load(Ordering::Acquire);
            if tag != FREE && !wanted.iter().any(|p| row_tag(*p) == tag) {
                port.store(FREE, Ordering::Release);
            }
        }
        let mut fits = true;
        for p in wanted {
            let tag = row_tag(p);
            let row = self.ports.iter().position(|r| r.load(Ordering::Acquire) == tag);
            let (row, new) = match row {
                Some(r) => (r, false),
                None => match self.ports.iter().position(|r| r.load(Ordering::Acquire) == FREE) {
                    Some(r) => (r, true),
                    None => {
                        fits = false;
                        continue;
                    }
                },
            };
            let mut words = [0u64; CONTROL_WORDS];
            for (_, c) in controls.iter().filter(|(q, _)| *q == p) {
                let b = c.bit();
                if let Some(w) = words.get_mut(b / 64) {
                    *w |= 1 << (b % 64);
                }
            }
            if let Some(bits) = self.bits.get(row) {
                for (a, w) in bits.iter().zip(words) {
                    a.store(w, Ordering::Relaxed);
                }
            }
            // A new row is claimed once its bits are in place.
            if new && let Some(r) = self.ports.get(row) {
                r.store(tag, Ordering::Release);
            }
        }
        fits
    }

    /// The row `port` has, if any (for tests of the row's stability).
    pub fn row_of(&self, port: PortId) -> Option<usize> {
        self.ports.iter().position(|r| r.load(Ordering::Acquire) == row_tag(port))
    }

    /// Whether `msg` from `port` is mapped to a control. No allocation, no lock.
    pub fn consumes(&self, port: PortId, msg: &MidiMessage) -> bool {
        let Some(c) = Control::of(msg) else { return false };
        let b = c.bit();
        let tag = row_tag(port);
        self.ports
            .iter()
            .zip(&self.bits)
            .find(|(p, _)| p.load(Ordering::Acquire) == tag)
            .and_then(|(_, bits)| bits.get(b / 64))
            .is_some_and(|w| w.load(Ordering::Relaxed) & (1 << (b % 64)) != 0)
    }
}

/// The MIDI input's state on the audio thread.
pub(crate) struct MidiIn {
    pub(crate) sub: Option<Subscriber>,
    clock: MidiClock,
    pending: Vec<MidiEvent>,
    /// The current callback's start on the clock.
    callback_ns: u64,
    /// Where the next block starts, in session samples from the callback's first output sample
    /// (audio rendered ahead earlier plus what this callback rendered so far).
    rendered: i64,
    /// The last [`WINDOW`] callback lengths (ns), a ring, and how many are in it.
    spans: [u64; WINDOW],
    nspans: usize,
    next_span: usize,
    /// Callbacks seen (saturating).
    seen: u32,
    /// Session samples per nanosecond at the current speed.
    per_ns: f64,
    /// The live latency `L`.
    pub(crate) latency_ns: u64,
    /// Events dropped from the full pending buffer.
    pub(crate) dropped: u64,
}

/// Per-port bookkeeping for one block: last offset emitted, and the ports whose events wait.
struct Order {
    last: [(PortId, usize); PORTS],
    nlast: usize,
    blocked: [PortId; PORTS],
    nblocked: usize,
    /// More ports blocked than fit: every port's events wait.
    all_blocked: bool,
}

impl Order {
    fn new() -> Order {
        Order { last: [(PortId(0), 0); PORTS], nlast: 0, blocked: [PortId(0); PORTS], nblocked: 0, all_blocked: false }
    }

    fn is_blocked(&self, p: PortId) -> bool {
        self.all_blocked || self.blocked.get(..self.nblocked).is_some_and(|b| b.contains(&p))
    }

    fn block(&mut self, p: PortId) {
        if self.is_blocked(p) {
            return;
        }
        match self.blocked.get_mut(self.nblocked) {
            Some(slot) => {
                *slot = p;
                self.nblocked += 1;
            }
            None => self.all_blocked = true,
        }
    }

    /// `offset` clamped to not precede the port's last event, recorded.
    fn clamp(&mut self, p: PortId, offset: usize) -> usize {
        let last = self.last.get(..self.nlast).and_then(|l| l.iter().position(|(q, _)| *q == p));
        match last.and_then(|i| self.last.get_mut(i)) {
            Some(e) => {
                e.1 = e.1.max(offset);
                e.1
            }
            None => {
                if let Some(slot) = self.last.get_mut(self.nlast) {
                    *slot = (p, offset);
                    self.nlast += 1;
                }
                offset
            }
        }
    }
}

/// A message as MIDI 1.0 bytes (no allocation, unlike [`MidiMessage::to_bytes`]).
pub fn raw(msg: MidiMessage) -> [u8; 3] {
    let st = |kind: u8, ch: u8| kind | (ch & 0x0F);
    match msg {
        MidiMessage::NoteOff { ch, note, vel } => [st(0x80, ch), note & 0x7F, vel & 0x7F],
        MidiMessage::NoteOn { ch, note, vel } => [st(0x90, ch), note & 0x7F, vel & 0x7F],
        MidiMessage::PolyPressure { ch, note, pressure } => [st(0xA0, ch), note & 0x7F, pressure & 0x7F],
        MidiMessage::ControlChange { ch, cc, value } => [st(0xB0, ch), cc & 0x7F, value & 0x7F],
        MidiMessage::ProgramChange { ch, program } => [st(0xC0, ch), program & 0x7F, 0],
        MidiMessage::ChannelPressure { ch, pressure } => [st(0xD0, ch), pressure & 0x7F, 0],
        MidiMessage::PitchBend { ch, value } => {
            let v = (i32::from(value).clamp(-8192, 8191) + 8192) as u16;
            [st(0xE0, ch), (v & 0x7F) as u8, ((v >> 7) & 0x7F) as u8]
        }
    }
}

/// The message MIDI 1.0 bytes from [`raw`] hold (`None` for a status that is no channel message).
/// A note-on with velocity 0 is a note-off. No allocation.
pub fn from_raw(bytes: [u8; 3]) -> Option<MidiMessage> {
    let [status, d1, d2] = bytes;
    let (ch, d1, d2) = (status & 0x0F, d1 & 0x7F, d2 & 0x7F);
    Some(match status & 0xF0 {
        0x80 => MidiMessage::NoteOff { ch, note: d1, vel: d2 },
        0x90 if d2 == 0 => MidiMessage::NoteOff { ch, note: d1, vel: 0 },
        0x90 => MidiMessage::NoteOn { ch, note: d1, vel: d2 },
        0xA0 => MidiMessage::PolyPressure { ch, note: d1, pressure: d2 },
        0xB0 => MidiMessage::ControlChange { ch, cc: d1, value: d2 },
        0xC0 => MidiMessage::ProgramChange { ch, program: d1 },
        0xD0 => MidiMessage::ChannelPressure { ch, pressure: d1 },
        0xE0 => MidiMessage::PitchBend { ch, value: ((i16::from(d2) << 7) | i16::from(d1)) - 8192 },
        _ => return None,
    })
}

fn is_note_off(e: &MidiEvent) -> bool {
    matches!(e.msg, MidiMessage::NoteOff { .. })
}

impl MidiIn {
    pub(crate) fn new() -> MidiIn {
        MidiIn {
            sub: None,
            clock: MidiClock::new(),
            pending: Vec::with_capacity(PENDING),
            callback_ns: 0,
            rendered: 0,
            spans: [0; WINDOW],
            nspans: 0,
            next_span: 0,
            seen: 0,
            per_ns: 48_000.0 / 1e9,
            latency_ns: MIDI_SLACK_NS,
            dropped: 0,
        }
    }

    /// Takes a new subscriber (stamped on `clock`), returning the old one; what waited is dropped.
    pub(crate) fn replace(&mut self, sub: Option<(Subscriber, MidiClock)>) -> Option<Subscriber> {
        self.pending.clear();
        let (sub, clock) = match sub {
            Some((s, c)) => (Some(s), c),
            None => (None, self.clock),
        };
        self.clock = clock;
        std::mem::replace(&mut self.sub, sub)
    }

    /// Now on the events' clock.
    pub(crate) fn now_ns(&self) -> u64 {
        self.clock.now_ns()
    }

    /// A callback starts at `now_ns`, `span_ns` long, rendering `per_s` session samples a second.
    pub(crate) fn begin(&mut self, now_ns: u64, span_ns: u64, per_s: f64) {
        self.callback_ns = now_ns;
        self.rendered = 0;
        if per_s.is_finite() && per_s > 0.0 {
            self.per_ns = per_s / 1e9;
        }
        self.seen = self.seen.saturating_add(1);
        if self.seen > SKIP {
            if let Some(s) = self.spans.get_mut(self.next_span) {
                *s = span_ns;
            }
            self.next_span = (self.next_span + 1) % WINDOW;
            self.nspans = (self.nspans + 1).min(WINDOW);
        }
        let span = match self.spans.get(..self.nspans).filter(|w| !w.is_empty()) {
            None => span_ns,
            Some(w) => {
                // No allocation: the window is sorted in a copy on the stack.
                let mut sorted = [0u64; WINDOW];
                let sorted = sorted.get_mut(..w.len()).unwrap_or_default();
                sorted.copy_from_slice(w);
                sorted.sort_unstable();
                let median = sorted.get(sorted.len() / 2).copied().unwrap_or(span_ns);
                let max = sorted.last().copied().unwrap_or(span_ns);
                max.min(median.saturating_mul(2))
            }
        };
        self.latency_ns = span.min(MAX_SPAN_NS).saturating_add(MIDI_SLACK_NS);
    }

    /// `ahead` session samples rendered in an earlier callback play first in this one.
    pub(crate) fn ahead(&mut self, ahead: f64) {
        self.rendered = if ahead.is_finite() && ahead > 0.0 { ahead.round().min(1e9) as i64 } else { 0 };
    }

    /// Drops the events waiting for a later block (a panic).
    pub(crate) fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Live input waits to be played: in the queue or for a later block.
    pub(crate) fn waiting(&self) -> bool {
        !self.pending.is_empty() || self.sub.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// Drops everything waiting and queued (nothing is listening).
    pub(crate) fn discard(&mut self) {
        self.pending.clear();
        if let Some(sub) = &mut self.sub {
            while sub.pop().is_some() {}
        }
    }

    /// The event's sample offset from the start of the block being rendered (negative: past).
    fn offset(&self, e: &MidiEvent) -> i64 {
        let ns = e.time_ns as i128 + i128::from(self.latency_ns) - i128::from(self.callback_ns);
        let at = (ns as f64 * self.per_ns).floor();
        let at = if at.is_finite() { at.clamp(-1e15, 1e15) as i64 } else { 0 };
        at.saturating_sub(self.rendered)
    }

    /// Hands the events due in the next block of `n` samples to `mix` (in port order), keeps the
    /// later ones, and counts `n` as rendered. New arrivals that `controls` consumes are dropped.
    pub(crate) fn feed(&mut self, mix: &mut MixEngine, n: usize, controls: &MidiControls) {
        let mut order = Order::new();
        let n_i = i64::try_from(n).unwrap_or(i64::MAX);
        // What waited, oldest first, compacted in place.
        let mut keep = 0;
        for i in 0..self.pending.len() {
            let Some(e) = self.pending.get(i).copied() else { break };
            let at = self.offset(&e);
            if order.is_blocked(e.port) || at >= n_i {
                order.block(e.port);
                if let Some(slot) = self.pending.get_mut(keep) {
                    *slot = e;
                }
                keep += 1;
            } else {
                emit(mix, &mut order, e, at);
            }
        }
        self.pending.truncate(keep);
        // New arrivals.
        while let Some(e) = self.sub.as_mut().and_then(Subscriber::pop) {
            if controls.consumes(e.port, &e.msg) {
                continue;
            }
            let at = self.offset(&e);
            if order.is_blocked(e.port) || at >= n_i {
                order.block(e.port);
                self.wait(e);
            } else {
                emit(mix, &mut order, e, at);
            }
        }
        self.rendered = self.rendered.saturating_add(n_i);
    }

    /// Keeps `e` for a later block (see the module docs for a full buffer).
    fn wait(&mut self, e: MidiEvent) {
        if self.pending.len() < PENDING {
            self.pending.push(e);
            return;
        }
        self.dropped = self.dropped.saturating_add(1);
        if is_note_off(&e)
            && let Some(i) = self.pending.iter().rposition(|p| !is_note_off(p))
        {
            self.pending.remove(i);
            self.pending.push(e);
        }
    }
}

fn emit(mix: &mut MixEngine, order: &mut Order, e: MidiEvent, at: i64) {
    let offset = order.clamp(e.port, usize::try_from(at.max(0)).unwrap_or(0));
    mix.live_input(LiveEvent { source: e.port.0, offset, msg: raw(e.msg) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_keeps_its_control_row_and_extra_ports_are_reported() {
        let c = MidiControls::default();
        let note = |n: u8| Control::Note { ch: 0, note: n };
        let on = |n: u8| MidiMessage::NoteOn { ch: 0, note: n, vel: 1 };
        assert!(c.set(&[(PortId(5), note(1)), (PortId(2), note(2))]));
        let row = c.row_of(PortId(5));
        assert!(c.set(&[(PortId(5), note(3)), (PortId(1), note(4))]), "2 leaves, 1 joins");
        assert_eq!(c.row_of(PortId(5)), row, "5 kept its row");
        assert!(c.consumes(PortId(5), &on(3)) && !c.consumes(PortId(5), &on(1)) && c.consumes(PortId(1), &on(4)));
        assert!(!c.consumes(PortId(2), &on(2)), "2 left");
        assert!(!c.consumes(PortId(KEYBOARD_SOURCE), &on(3)), "a free row matches no port");
        let many: Vec<(PortId, Control)> = (0..CONTROL_PORTS as u32 + 1).map(|p| (PortId(p), note(9))).collect();
        assert!(!c.set(&many), "one port too many");
        assert!(c.set(&[]));
        assert!(!c.consumes(PortId(5), &on(3)));
    }

    #[test]
    fn raw_bytes_match_the_wire_format() {
        let msgs = [
            MidiMessage::NoteOn { ch: 3, note: 60, vel: 100 },
            MidiMessage::NoteOff { ch: 0, note: 61, vel: 5 },
            MidiMessage::PolyPressure { ch: 1, note: 2, pressure: 3 },
            MidiMessage::ControlChange { ch: 15, cc: 64, value: 127 },
            MidiMessage::ProgramChange { ch: 2, program: 9 },
            MidiMessage::ChannelPressure { ch: 2, pressure: 9 },
            MidiMessage::PitchBend { ch: 0, value: -8192 },
            MidiMessage::PitchBend { ch: 0, value: 8191 },
        ];
        for m in msgs {
            let b = m.to_bytes();
            let r = raw(m);
            assert_eq!(&r[..b.len()], b.as_slice(), "{m:?}");
            assert_eq!(from_raw(r), Some(m), "round trip");
        }
        assert_eq!(from_raw([0x93, 60, 0]), Some(MidiMessage::NoteOff { ch: 3, note: 60, vel: 0 }));
        assert_eq!(from_raw([0xF8, 0, 0]), None);
    }

    impl MidiIn {
        /// (Events waiting, capacity).
        pub(crate) fn pending_len(&self) -> (usize, usize) {
            (self.pending.len(), self.pending.capacity())
        }

        pub(crate) fn pending_any(&self, f: impl Fn(&MidiEvent) -> bool) -> bool {
            self.pending.iter().any(f)
        }
    }
}
