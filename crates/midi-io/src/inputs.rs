use midir::{Ignore, MidiInput, MidiInputConnection, MidiInputPort};

use crate::queue::{Hub, PortSink};
use crate::{MidiClock, MidiIoError, MidiSource, PortId, PortIds, PortInfo, Subscriber, port_infos, unique_names};

/// Client name shown to the OS MIDI system.
const CLIENT: &str = "djforge";

/// The real MIDI inputs, through `midir` (WinMM on Windows, CoreMIDI on macOS, ALSA on Linux).
///
/// Each open port has its own connection; its callback runs on the backend's thread, parses the
/// bytes and pushes into the port's wait-free queue (it never blocks or locks). SysEx, clock and
/// active sensing are filtered by the backend and again by the parser.
pub struct MidiInputs {
    hub: Hub,
    ids: PortIds,
    open: Vec<(String, MidiInputConnection<PortSink>)>,
}

impl MidiInputs {
    /// No ports open; events will be stamped on `clock`.
    pub fn new(clock: MidiClock) -> Result<Self, MidiIoError> {
        Ok(Self { hub: Hub::new(clock)?, ids: PortIds::default(), open: Vec::new() })
    }

    /// A fresh backend handle and its ports with unique names.
    fn enumerate() -> Result<(MidiInput, Vec<(String, MidiInputPort)>), String> {
        let mut input = MidiInput::new(CLIENT).map_err(|e| e.to_string())?;
        input.ignore(Ignore::All);
        let ports = input.ports();
        let names = unique_names(ports.iter().map(|p| input.port_name(p).unwrap_or_else(|_| "Unknown MIDI input".to_string())));
        Ok((input, names.into_iter().zip(ports).collect()))
    }
}

impl MidiSource for MidiInputs {
    fn list(&self) -> Vec<PortInfo> {
        match Self::enumerate() {
            Ok((_, ports)) => port_infos(ports.into_iter().map(|(n, _)| n).collect()),
            Err(e) => {
                log::warn!("listing MIDI inputs failed: {e}");
                Vec::new()
            }
        }
    }

    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError> {
        if self.is_open(name) {
            return Ok(self.ids.id_for(name));
        }
        let backend = |reason: String| MidiIoError::Backend { port: name.to_string(), reason };
        let (input, ports) = Self::enumerate().map_err(backend)?;
        let port = ports.into_iter().find(|(n, _)| n == name).map(|(_, p)| p).ok_or_else(|| MidiIoError::NoSuchPort(name.to_string()))?;
        let id = self.ids.id_for(name);
        let sink = self.hub.port_sink(id)?;
        // On failure the sink is dropped and the router retires its queue.
        let conn = input
            .connect(&port, "djforge-in", |_device_stamp, bytes, sink: &mut PortSink| sink.feed(bytes), sink)
            .map_err(|e| backend(e.to_string()))?;
        self.open.push((name.to_string(), conn));
        Ok(id)
    }

    fn close(&mut self, name: &str) -> bool {
        let Some(i) = self.open.iter().position(|(n, _)| n == name) else {
            return false;
        };
        let (_, conn) = self.open.swap_remove(i);
        // Closing stops the callback; the returned sink is dropped, retiring the port's queue.
        drop(conn.close());
        true
    }

    fn is_open(&self, name: &str) -> bool {
        self.open.iter().any(|(n, _)| n == name)
    }

    fn subscribe(&self, capacity: usize) -> Result<Subscriber, MidiIoError> {
        self.hub.subscribe(capacity)
    }

    fn port_name(&self, id: PortId) -> Option<&str> {
        self.ids.name(id)
    }

    fn clock(&self) -> MidiClock {
        self.hub.clock()
    }

    fn dropped(&self) -> u64 {
        self.hub.dropped()
    }
}
