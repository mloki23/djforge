use midir::{Ignore, MidiInput, MidiInputConnection, MidiInputPort};

use crate::ports::{PortChange, PortTable, listing};
use crate::queue::{HopStats, Hub, PortSink};
use crate::{MidiClock, MidiIoError, MidiSource, PortId, PortInfo, Subscriber};

/// Client name shown to the OS MIDI system.
const CLIENT: &str = "djforge";

/// The real MIDI inputs, through `midir` (WinMM on Windows, CoreMIDI on macOS, ALSA on Linux).
///
/// Each open port has its own connection; its callback runs on the backend's thread, parses the
/// bytes and pushes into the port's wait-free queue (it never blocks or locks). SysEx, clock and
/// active sensing are filtered by the backend and again by the parser.
///
/// Unplug detection is by listing: [`MidiSource::refresh`] closes ports whose backend id is no
/// longer listed. WinMM sends no notice when a device goes, so a device pulled out and plugged back
/// in between two refreshes keeps a silent connection; `close` then `open` reconnects it.
pub struct MidiInputs {
    // Declared before `hub`, and cleared in `Drop`, so connections close before the router stops.
    table: PortTable<MidiInputConnection<PortSink>>,
    hub: Hub,
}

impl MidiInputs {
    /// No ports open; events will be stamped on `clock`.
    pub fn new(clock: MidiClock) -> Result<Self, MidiIoError> {
        Ok(Self { table: PortTable::default(), hub: Hub::new(clock)? })
    }

    /// A fresh backend client and its ports, listed. midir's WinMM port enumeration contains an
    /// `assert!`; enumeration never runs on the audio thread, so a panic there is caught and becomes
    /// an error rather than taking the app down.
    fn enumerate() -> Result<(MidiInput, Vec<(PortInfo, MidiInputPort)>), String> {
        std::panic::catch_unwind(Self::enumerate_inner).unwrap_or_else(|_| Err("the MIDI backend panicked while listing ports".to_string()))
    }

    fn enumerate_inner() -> Result<(MidiInput, Vec<(PortInfo, MidiInputPort)>), String> {
        let mut input = MidiInput::new(CLIENT).map_err(|e| e.to_string())?;
        input.ignore(Ignore::All);
        let ports = input.ports();
        let infos = listing(ports.iter().map(|p| (input.port_name(p).unwrap_or_else(|_| "Unknown MIDI input".to_string()), p.id())));
        Ok((input, infos.into_iter().zip(ports).collect()))
    }

    fn infos() -> Vec<PortInfo> {
        match Self::enumerate() {
            Ok((_, ports)) => ports.into_iter().map(|(info, _)| info).collect(),
            Err(e) => {
                log::warn!("listing MIDI inputs failed: {e}");
                Vec::new()
            }
        }
    }
}

impl Drop for MidiInputs {
    fn drop(&mut self) {
        self.table.close_all();
    }
}

impl MidiSource for MidiInputs {
    fn list(&self) -> Vec<PortInfo> {
        Self::infos()
    }

    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError> {
        let backend = |reason: String| MidiIoError::Backend { port: name.to_string(), reason };
        let (input, ports) = Self::enumerate().map_err(backend)?;
        let now: Vec<PortInfo> = ports.iter().map(|(info, _)| info.clone()).collect();
        let hub = &self.hub;
        // midir gives no liveness signal: a connection is alive while its device is listed.
        self.table.open(
            &now,
            name,
            |_| true,
            |id, info| {
                let port = ports.iter().find(|(i, _)| i.same_port(info)).map(|(_, p)| p).ok_or_else(|| MidiIoError::NoSuchPort(name.to_string()))?;
                let sink = hub.port_sink(id)?;
                // On failure the sink is dropped and the router retires its queue.
                input
                    .connect(port, "djforge-in", |_device_stamp, bytes, sink: &mut PortSink| sink.feed(bytes), sink)
                    .map_err(|e| backend(e.to_string()))
            },
        )
    }

    fn close(&mut self, name: &str) -> bool {
        // Closing stops the callback; the returned sink is dropped, retiring the port's queue.
        self.table.close(name).map(|conn| drop(conn.close())).is_some()
    }

    fn is_open(&self, name: &str) -> bool {
        self.table.is_open(name)
    }

    fn refresh(&mut self) -> Vec<PortChange> {
        match Self::enumerate() {
            Ok((_, ports)) => {
                let now: Vec<PortInfo> = ports.into_iter().map(|(info, _)| info).collect();
                self.table.refresh(&now, |_| true)
            }
            Err(e) => {
                // Not "every device vanished": keep the ports and try again next time.
                log::warn!("listing MIDI inputs failed: {e}");
                Vec::new()
            }
        }
    }

    fn subscribe(&self, capacity: usize) -> Result<Subscriber, MidiIoError> {
        self.hub.subscribe(capacity)
    }

    fn port_name(&self, id: PortId) -> Option<&str> {
        self.table.port_name(id)
    }

    fn raw_name(&self, id: PortId) -> Option<&str> {
        self.table.raw_name(id)
    }

    fn backend_id(&self, id: PortId) -> Option<&str> {
        self.table.backend_id(id)
    }

    fn clock(&self) -> MidiClock {
        self.hub.clock()
    }

    fn dropped(&self) -> u64 {
        self.hub.dropped()
    }

    fn hop_stats(&self, reset: bool) -> HopStats {
        self.hub.hop_stats(reset)
    }

    fn healthy(&self) -> bool {
        self.hub.healthy()
    }
}
