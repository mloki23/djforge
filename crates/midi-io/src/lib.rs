//! SoundCraft MIDI input.
//!
//! - [`MidiMessage`] / [`MidiParser`]: live channel-voice messages, parsed with running status;
//!   SysEx, clock, active sensing and the rest of the system messages are dropped.
//! - [`MidiEvent`]: a message, the port it came from and its arrival time on a shared [`MidiClock`].
//! - [`MidiSource`]: a set of input ports opened by name. [`MidiInputs`] is the real one (`midir`:
//!   WinMM, CoreMIDI, ALSA); `FakeSource` (the `testing` feature, for tests only) simulates
//!   devices and lets tests inject events at chosen times.
//! - [`MidiOutputs`]: output ports (a controller's lights), opened by the same identity as inputs.
//!   [`SystemOutputs`] is the real one: a sender thread owns the connections, so sending never
//!   blocks the caller; `FakeOutputs` (the `testing` feature) records what was sent.
//! - [`Subscriber`]: one consumer's bounded, wait-free queue. Every open port's events fan out to
//!   every subscriber through a router thread (see the `queue` module). Full queues drop and count;
//!   [`HopStats`] measures the router's delay.
//!
//! Devices come and go. Poll [`MidiSource::refresh`] (about once a second, off the UI and audio
//! threads): it closes ports whose device vanished, reporting [`PortChange::Closed`] so held notes
//! can be released, and reports new and renamed devices. A port is identified by its backend id and
//! raw name together (the ports of one multi-port device share a backend id on WinMM). It keeps its
//! [`PortId`] across replugs on Windows and macOS; on ALSA the backend id is `client:port`, which
//! changes on replug, so a replugged device reads as `Closed` then `Appeared` with a new id. See the
//! `ports` module for how listed names relate to all this.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::indexing_slicing)]

#[cfg(any(test, feature = "testing"))]
mod fake;
#[cfg(any(test, feature = "testing"))]
mod fake_out;
#[cfg(not(target_arch = "wasm32"))]
mod inputs;
mod message;
#[cfg(not(target_arch = "wasm32"))]
mod outputs;
mod ports;
mod queue;

use std::time::Instant;

#[cfg(any(test, feature = "testing"))]
pub use fake::FakeSource;
#[cfg(any(test, feature = "testing"))]
pub use fake_out::FakeOutputs;
#[cfg(not(target_arch = "wasm32"))]
pub use inputs::MidiInputs;
pub use message::{MidiMessage, MidiParser, parse_all};
#[cfg(not(target_arch = "wasm32"))]
pub use outputs::SystemOutputs;
pub use ports::PortChange;
pub use queue::{HOP_BUCKETS, HopStats, MAX_CAPACITY, PORT_CAPACITY, Subscriber};

/// Errors from opening ports or setting up the queue.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MidiIoError {
    #[error("no MIDI input named \"{0}\"")]
    NoSuchPort(String),
    #[error("no MIDI output named \"{0}\"")]
    NoSuchOutput(String),
    #[error("MIDI input \"{port}\": {reason}")]
    Backend { port: String, reason: String },
    #[error("MIDI router: {0}")]
    Router(String),
}

/// The monotonic clock MIDI events are stamped with: nanoseconds since an origin [`Instant`].
///
/// Create one and hand copies to the source and to every consumer that needs to relate event times
/// to its own (the audio thread maps them to sample positions). The device's own timestamp is not
/// used: backends disagree on its origin and units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiClock {
    origin: Instant,
}

impl MidiClock {
    /// A clock whose zero is now.
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }

    /// A clock whose zero is `origin`.
    pub fn from_origin(origin: Instant) -> Self {
        Self { origin }
    }

    /// The origin instant.
    pub fn origin(&self) -> Instant {
        self.origin
    }

    /// Nanoseconds from the origin to now.
    pub fn now_ns(&self) -> u64 {
        self.ns_at(Instant::now())
    }

    /// Nanoseconds from the origin to `t` (0 if `t` is earlier; saturates after ~584 years).
    pub fn ns_at(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.origin).as_nanos()).unwrap_or(u64::MAX)
    }
}

impl Default for MidiClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A port's id within one source, keyed on (backend id, raw name): the same port always gets the
/// same id under whatever listed name, and after replugs on Windows and macOS (not on ALSA, whose
/// backend id changes on replug).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId(pub u32);

/// One received message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidiEvent {
    /// The port it arrived on.
    pub port: PortId,
    pub msg: MidiMessage,
    /// Arrival time in nanoseconds on the source's [`MidiClock`].
    pub time_ns: u64,
}

/// An input port as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The name to open it by, unique within this listing only: a second device with the same name
    /// is "Name #2", and so on, and the suffix can change when devices come and go.
    pub name: String,
    /// The name as the backend reports it, without the suffix.
    pub raw_name: String,
    /// Its position in the current device list (changes when devices come and go).
    pub index: usize,
    /// The backend's id for the port. Opaque. On WinMM it is the device-interface path, shared by all
    /// ports of one device; on CoreMIDI the endpoint id; on ALSA `client:port` (changes on replug).
    /// Identify a port by this together with `raw_name`.
    pub backend_id: String,
}

/// A set of MIDI input ports, opened and closed by name, delivering to [`Subscriber`]s.
pub trait MidiSource {
    /// The ports present now. On real hardware this creates a backend client and enumerates
    /// devices: poll it (or [`MidiSource::refresh`]) at about 1 Hz, off the UI and audio threads.
    fn list(&self) -> Vec<PortInfo>;
    /// Opens a port by its listed name. If that port is already open (under any name) with a live
    /// connection, returns its id and connects nothing; a dead connection is replaced, keeping the id.
    /// On real hardware a connection only counts as dead once a listing has seen its device
    /// missing: a device unplugged and replugged between two listings keeps a silent connection, and
    /// `open` returns it. Close then open to force a reconnect (Task 5 offers this as Reconnect).
    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError>;
    /// Closes a port; false if it was not open.
    fn close(&mut self, name: &str) -> bool;
    /// Whether a port is open under this name (names follow renames seen by `open` / `refresh`).
    fn is_open(&self, name: &str) -> bool;
    /// Re-lists the devices, closes open ports whose device has gone or whose connection died, and
    /// returns every change since the last call (including ones noticed by `open`). Same cost as
    /// [`MidiSource::list`].
    fn refresh(&mut self) -> Vec<PortChange>;
    /// A new queue of `capacity` events (clamped to 1..=[`MAX_CAPACITY`]). It receives every event
    /// from every port that the router moves after the subscription is registered; nothing earlier
    /// is replayed.
    fn subscribe(&self, capacity: usize) -> Result<Subscriber, MidiIoError>;
    /// A port id's current listed name.
    fn port_name(&self, id: PortId) -> Option<&str>;
    /// A port id's raw (unsuffixed) name.
    fn raw_name(&self, id: PortId) -> Option<&str>;
    /// A port id's backend id.
    fn backend_id(&self, id: PortId) -> Option<&str>;
    /// The clock events are stamped with.
    fn clock(&self) -> MidiClock;
    /// Events dropped so far because a queue (port or subscriber) was full.
    fn dropped(&self) -> u64;
    /// The router's hop delay. With `reset`, the counters restart, so successive calls give a
    /// recent window (read e.g. once a second for a recent p99).
    fn hop_stats(&self, reset: bool) -> HopStats;
    /// False once the router thread has stopped: subscribers will receive nothing more.
    fn healthy(&self) -> bool;

    /// The simulated source behind this one, for tests that inject events into a source an app
    /// owns (`None` for real hardware). Tests only (the `testing` feature).
    #[cfg(any(test, feature = "testing"))]
    fn as_fake_mut(&mut self) -> Option<&mut FakeSource> {
        None
    }

    /// Opens every listed port that is not open yet; one result per attempted port.
    fn open_all(&mut self) -> Vec<Result<PortId, MidiIoError>> {
        let closed: Vec<PortInfo> = self.list().into_iter().filter(|p| !self.is_open(&p.name)).collect();
        closed.into_iter().map(|p| self.open(&p.name)).collect()
    }
}

/// Messages queued for an output port but not yet sent before new ones are dropped (and counted).
/// A controller's full set of lights is about 200 messages.
pub const OUT_CAPACITY: usize = 1024;

/// A set of MIDI output ports, opened and closed by name like [`MidiSource`]'s inputs and
/// identified the same way: (backend id, raw name), with [`PortId`]s that are stable for the
/// life of the set. The input and output halves of one controller are separate ports; match them
/// by raw name (on Windows a Pioneer DDJ-FLX4 lists "DDJ-FLX4" as both).
///
/// [`MidiOutputs::send`] never blocks: messages go into a bounded queue that a thread of the
/// implementation drains, in order, per set. Never call it from the audio thread.
pub trait MidiOutputs {
    /// The output ports present now. Creates a backend client on real hardware: poll at about
    /// 1 Hz, off the audio thread.
    fn list(&self) -> Vec<PortInfo>;
    /// Opens an output by its listed name; an already open port returns its id.
    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError>;
    /// Closes an output after the messages already queued for it are sent; false if not open.
    fn close(&mut self, name: &str) -> bool;
    /// Whether an output is open under this name.
    fn is_open(&self, name: &str) -> bool;
    /// Whether the output with this id is open.
    fn is_open_id(&self, id: PortId) -> bool;
    /// Queues `msg` for the open port `id`. False (nothing queued) when the port isn't open or
    /// the queue is full; the latter is counted in [`MidiOutputs::dropped`].
    fn send(&mut self, id: PortId, msg: MidiMessage) -> bool;
    /// Re-lists the outputs, closes open ones whose device went away, and returns the changes
    /// since the last call, as [`MidiSource::refresh`] does for inputs.
    fn refresh(&mut self) -> Vec<PortChange>;
    /// A port id's current listed name.
    fn port_name(&self, id: PortId) -> Option<&str>;
    /// A port id's raw (unsuffixed) name.
    fn raw_name(&self, id: PortId) -> Option<&str>;
    /// A port id's backend id.
    fn backend_id(&self, id: PortId) -> Option<&str>;
    /// Messages dropped so far because the queue was full or the backend refused them.
    fn dropped(&self) -> u64;

    /// The simulated outputs behind this one (`None` for real hardware). Tests only.
    #[cfg(any(test, feature = "testing"))]
    fn as_fake_mut(&mut self) -> Option<&mut FakeOutputs> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn clock_measures_from_its_origin() {
        let origin = Instant::now();
        let clock = MidiClock::from_origin(origin);
        assert_eq!(clock.ns_at(origin + Duration::from_millis(3)), 3_000_000);
        assert_eq!(clock.ns_at(origin.checked_sub(Duration::from_millis(1)).unwrap_or(origin)), 0);
    }
}
