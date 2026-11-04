//! SoundCraft MIDI input.
//!
//! - [`MidiMessage`] / [`MidiParser`]: live channel-voice messages, parsed with running status;
//!   SysEx, clock, active sensing and the rest of the system messages are dropped.
//! - [`MidiEvent`]: a message, the port it came from and its arrival time on a shared [`MidiClock`].
//! - [`MidiSource`]: a set of input ports opened by name. [`MidiInputs`] is the real one (`midir`:
//!   WinMM, CoreMIDI, ALSA); [`FakeSource`] simulates devices and lets tests inject events at chosen
//!   times.
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

mod fake;
#[cfg(not(target_arch = "wasm32"))]
mod inputs;
mod message;
mod ports;
mod queue;

use std::time::Instant;

pub use fake::FakeSource;
#[cfg(not(target_arch = "wasm32"))]
pub use inputs::MidiInputs;
pub use message::{MidiMessage, MidiParser, parse_all};
pub use ports::PortChange;
pub use queue::{HOP_BUCKETS, HopStats, MAX_CAPACITY, PORT_CAPACITY, Subscriber};

/// Errors from opening ports or setting up the queue.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MidiIoError {
    #[error("no MIDI input named \"{0}\"")]
    NoSuchPort(String),
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

    /// Opens every listed port that is not open yet; one result per attempted port.
    fn open_all(&mut self) -> Vec<Result<PortId, MidiIoError>> {
        let closed: Vec<PortInfo> = self.list().into_iter().filter(|p| !self.is_open(&p.name)).collect();
        closed.into_iter().map(|p| self.open(&p.name)).collect()
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
