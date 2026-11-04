//! SoundCraft MIDI input.
//!
//! - [`MidiMessage`] / [`MidiParser`]: live channel-voice messages, parsed with running status;
//!   SysEx, clock, active sensing and the rest of the system messages are dropped.
//! - [`MidiEvent`]: a message, the port it came from and its arrival time on a shared [`MidiClock`].
//! - [`MidiSource`]: a set of input ports opened by name. [`MidiInputs`] is the real one (`midir`:
//!   WinMM, CoreMIDI, ALSA); [`FakeSource`] lets tests inject events at chosen times.
//! - [`Subscriber`]: one consumer's bounded, wait-free queue. Every open port's events fan out to
//!   every subscriber through a router thread (see the `queue` module). Full queues drop and count.
//!
//! Ports are identified by name, so a controller that is unplugged and plugged back in (and comes
//! back at a different index) reopens by the same name and keeps its [`PortId`].
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod fake;
#[cfg(not(target_arch = "wasm32"))]
mod inputs;
mod message;
mod queue;

use std::collections::HashSet;
use std::time::Instant;

pub use fake::FakeSource;
#[cfg(not(target_arch = "wasm32"))]
pub use inputs::MidiInputs;
pub use message::{MidiMessage, MidiParser, parse_all};
pub use queue::{MAX_CAPACITY, PORT_CAPACITY, Subscriber};

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

/// A port's stable id within one source: the same name always gets the same id.
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
    /// The unique name to open it by. A second device with the same name is "Name #2", and so on.
    pub name: String,
    /// Its position in the current device list (changes when devices come and go).
    pub index: usize,
}

/// A set of MIDI input ports, opened and closed by name, delivering to [`Subscriber`]s.
pub trait MidiSource {
    /// The ports present now.
    fn list(&self) -> Vec<PortInfo>;
    /// Opens a port by its listed name. Opening an open port returns its id.
    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError>;
    /// Closes a port; false if it was not open.
    fn close(&mut self, name: &str) -> bool;
    /// Whether the port is open.
    fn is_open(&self, name: &str) -> bool;
    /// A new queue of `capacity` events (clamped to 1..=[`MAX_CAPACITY`]) receiving every event from
    /// every port, from now on.
    fn subscribe(&self, capacity: usize) -> Result<Subscriber, MidiIoError>;
    /// The name a port id was opened under.
    fn port_name(&self, id: PortId) -> Option<&str>;
    /// The clock events are stamped with.
    fn clock(&self) -> MidiClock;
    /// Events dropped so far because a queue was full.
    fn dropped(&self) -> u64;

    /// Opens every listed port that is not open yet; one result per attempted port.
    fn open_all(&mut self) -> Vec<Result<PortId, MidiIoError>> {
        let closed: Vec<PortInfo> = self.list().into_iter().filter(|p| !self.is_open(&p.name)).collect();
        closed.into_iter().map(|p| self.open(&p.name)).collect()
    }
}

/// Port name → [`PortId`], stable for the life of a source.
#[derive(Debug, Default)]
pub(crate) struct PortIds {
    names: Vec<String>,
}

impl PortIds {
    pub(crate) fn id_for(&mut self, name: &str) -> PortId {
        if let Some(i) = self.names.iter().position(|n| n == name) {
            return PortId(u32::try_from(i).unwrap_or(u32::MAX));
        }
        self.names.push(name.to_string());
        PortId(u32::try_from(self.names.len().saturating_sub(1)).unwrap_or(u32::MAX))
    }

    pub(crate) fn name(&self, id: PortId) -> Option<&str> {
        self.names.get(usize::try_from(id.0).ok()?).map(String::as_str)
    }
}

/// Makes device names unique in list order: the second "X" becomes "X #2", the third "X #3".
pub(crate) fn unique_names(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let raw: Vec<String> = raw.into_iter().collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for name in raw {
        let mut candidate = name.clone();
        let mut n = 2u32;
        while seen.contains(&candidate) {
            candidate = format!("{name} #{n}");
            n = n.saturating_add(1);
        }
        seen.insert(candidate.clone());
        out.push(candidate);
    }
    out
}

/// Lists names as [`PortInfo`]s.
pub(crate) fn port_infos(names: Vec<String>) -> Vec<PortInfo> {
    names.into_iter().enumerate().map(|(index, name)| PortInfo { name, index }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn duplicate_names_get_a_suffix() {
        let names = unique_names(["Pads", "Keys", "Pads", "Pads #2", "Pads"].map(String::from));
        assert_eq!(names, vec!["Pads", "Keys", "Pads #2", "Pads #2 #2", "Pads #3"]);
    }

    #[test]
    fn port_ids_are_stable_per_name() {
        let mut ids = PortIds::default();
        let a = ids.id_for("A");
        let b = ids.id_for("B");
        assert_ne!(a, b);
        assert_eq!(ids.id_for("A"), a);
        assert_eq!(ids.name(b), Some("B"));
        assert_eq!(ids.name(PortId(99)), None);
    }

    #[test]
    fn clock_measures_from_its_origin() {
        let origin = Instant::now();
        let clock = MidiClock::from_origin(origin);
        assert_eq!(clock.ns_at(origin + Duration::from_millis(3)), 3_000_000);
        assert_eq!(clock.ns_at(origin.checked_sub(Duration::from_millis(1)).unwrap_or(origin)), 0);
    }
}
