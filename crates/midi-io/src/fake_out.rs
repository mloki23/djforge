use crate::ports::{PortChange, PortTable, listing};
use crate::{MidiIoError, MidiMessage, MidiOutputs, OUT_CAPACITY, PortId, PortInfo};

/// A simulated output connection: whether its device has been pulled out.
struct FakeOut {
    dead: bool,
}

/// [`MidiOutputs`] with simulated devices, for tests: plug and unplug devices, open them like real
/// ports, and read back every message sent to each (in order). Never touches a real port.
///
/// Like hardware, an unplugged device's open port stays open (sending to it is lost) until
/// [`MidiOutputs::refresh`] reports it closed. A device can be made to refuse messages
/// ([`FakeOutputs::set_full`]) as a full queue would.
pub struct FakeOutputs {
    table: PortTable<FakeOut>,
    devices: Vec<(String, String)>,
    next_backend: u32,
    sent: Vec<(PortId, MidiMessage)>,
    full: bool,
    dropped: u64,
}

impl Default for FakeOutputs {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeOutputs {
    /// No devices.
    pub fn new() -> Self {
        Self { table: PortTable::default(), devices: Vec::new(), next_backend: 0, sent: Vec::new(), full: false, dropped: 0 }
    }

    /// Plugs in a new output device (listed last); returns its backend id.
    pub fn plug(&mut self, raw_name: &str) -> String {
        let backend_id = format!("fake-out:{}", self.next_backend);
        self.next_backend = self.next_backend.saturating_add(1);
        self.plug_device(raw_name, &backend_id);
        backend_id
    }

    /// Plugs in a device with a given backend id (the same device again, after [`FakeOutputs::unplug`]).
    pub fn plug_device(&mut self, raw_name: &str, backend_id: &str) {
        self.devices.push((raw_name.to_string(), backend_id.to_string()));
    }

    /// Pulls out the device listed as `name`; false if not listed.
    pub fn unplug(&mut self, name: &str) -> bool {
        let Some(info) = self.list().into_iter().find(|p| p.name == name) else { return false };
        self.devices.retain(|(raw, id)| !(*id == info.backend_id && *raw == info.raw_name));
        for p in self.table.iter_mut().filter(|p| p.info.same_port(&info)) {
            p.conn.dead = true;
        }
        true
    }

    /// While `full`, every send is refused and counted dropped, as with a full queue.
    pub fn set_full(&mut self, full: bool) {
        self.full = full;
    }

    /// Every message sent so far with its port, oldest first; clears the record.
    pub fn take_sent(&mut self) -> Vec<(PortId, MidiMessage)> {
        std::mem::take(&mut self.sent)
    }

    /// The messages sent so far, without clearing.
    pub fn sent(&self) -> &[(PortId, MidiMessage)] {
        &self.sent
    }
}

impl MidiOutputs for FakeOutputs {
    fn as_fake_mut(&mut self) -> Option<&mut FakeOutputs> {
        Some(self)
    }

    fn list(&self) -> Vec<PortInfo> {
        listing(self.devices.iter().cloned())
    }

    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError> {
        let now = self.list();
        if !now.iter().any(|p| p.name == name) {
            return Err(MidiIoError::NoSuchOutput(name.to_string()));
        }
        self.table.open(&now, name, |c| !c.dead, |_, _| Ok(FakeOut { dead: false }))
    }

    fn close(&mut self, name: &str) -> bool {
        self.table.close(name).is_some()
    }

    fn is_open(&self, name: &str) -> bool {
        self.table.is_open(name)
    }

    fn is_open_id(&self, id: PortId) -> bool {
        self.table.by_id(id).is_some()
    }

    fn send(&mut self, id: PortId, msg: MidiMessage) -> bool {
        let Some(p) = self.table.by_id(id) else { return false };
        if self.full || self.sent.len() >= OUT_CAPACITY * 64 {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        if !p.conn.dead {
            self.sent.push((id, msg));
        }
        true
    }

    fn refresh(&mut self) -> Vec<PortChange> {
        let now = self.list();
        self.table.refresh(&now, |c| !c.dead)
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

    fn dropped(&self) -> u64 {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(note: u8) -> MidiMessage {
        MidiMessage::NoteOn { ch: 0, note, vel: 127 }
    }

    #[test]
    fn sends_are_recorded_per_open_port_in_order() {
        let mut o = FakeOutputs::new();
        o.plug("DDJ-FLX4");
        o.plug("Other");
        assert_eq!(o.list().iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["DDJ-FLX4", "Other"]);
        let id = o.open("DDJ-FLX4").unwrap();
        assert_eq!(o.open("DDJ-FLX4").unwrap(), id, "opening twice is idempotent");
        assert!(o.is_open_id(id) && o.is_open("DDJ-FLX4"));
        assert_eq!(o.raw_name(id), Some("DDJ-FLX4"));
        assert!(o.send(id, on(1)));
        assert!(o.send(id, on(2)));
        assert!(!o.send(PortId(9), on(3)), "not open");
        assert_eq!(o.take_sent(), vec![(id, on(1)), (id, on(2))]);
        assert!(o.sent().is_empty());
        assert!(o.close("DDJ-FLX4"));
        assert!(!o.send(id, on(4)));
        assert_eq!(o.open("Nope"), Err(MidiIoError::NoSuchOutput("Nope".into())));
    }

    #[test]
    fn unplug_loses_sends_until_refresh_closes_and_replug_keeps_the_id() {
        let mut o = FakeOutputs::new();
        let backend = o.plug("DDJ-FLX4");
        let id = o.open("DDJ-FLX4").unwrap();
        assert!(o.unplug("DDJ-FLX4"));
        assert!(o.send(id, on(1)), "still open until noticed");
        assert!(o.sent().is_empty(), "but nothing arrives");
        assert!(o.refresh().contains(&PortChange::Closed { port: id, name: "DDJ-FLX4".into() }));
        assert!(!o.is_open_id(id));
        o.plug_device("DDJ-FLX4", &backend);
        assert_eq!(o.open("DDJ-FLX4").unwrap(), id);
    }

    #[test]
    fn a_full_queue_drops_and_counts() {
        let mut o = FakeOutputs::new();
        o.plug("X");
        let id = o.open("X").unwrap();
        o.set_full(true);
        assert!(!o.send(id, on(1)));
        assert_eq!(o.dropped(), 1);
    }
}
