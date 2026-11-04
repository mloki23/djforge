use crate::ports::{PortChange, PortTable, listing};
use crate::queue::{HopStats, Hub, PortSink};
use crate::{MidiClock, MidiIoError, MidiMessage, MidiSource, PortId, PortInfo, Subscriber};

/// A simulated connection: the sink, and whether its device has been pulled out.
struct FakeConn {
    sink: PortSink,
    dead: bool,
}

/// A simulated device.
struct FakeDevice {
    name: String,
    backend_id: String,
}

/// A [`MidiSource`] with simulated devices, for tests of this crate and of the layers above it.
///
/// Plug and unplug devices, open them like real ports, and inject messages (or raw bytes, through
/// the same parser as the real ports) with chosen times. Events go through the same router and
/// subscriber queues as [`crate::MidiInputs`]; only the device end is simulated.
///
/// Unplugging behaves like hardware: the device leaves the listing and its open connection goes
/// dead (injecting into it delivers nothing), but the port stays open until [`MidiSource::refresh`]
/// (or an `open` of the same name) notices and reports it [`PortChange::Closed`].
pub struct FakeSource {
    // Declared before `hub`, and cleared in `Drop`, so connections close before the router stops.
    table: PortTable<FakeConn>,
    hub: Hub,
    devices: Vec<FakeDevice>,
    next_backend: u32,
}

impl FakeSource {
    /// An empty source stamping events on `clock`.
    pub fn new(clock: MidiClock) -> Result<Self, MidiIoError> {
        Ok(Self { table: PortTable::default(), hub: Hub::new(clock)?, devices: Vec::new(), next_backend: 0 })
    }

    /// Plugs in a new device (listed at the end) and returns its backend id. Two devices plugged
    /// with the same name are two distinct devices.
    pub fn plug(&mut self, raw_name: &str) -> String {
        let backend_id = format!("fake:{}", self.next_backend);
        self.next_backend = self.next_backend.saturating_add(1);
        self.plug_device(raw_name, &backend_id);
        backend_id
    }

    /// Plugs in a device with a given backend id: use the id [`FakeSource::plug`] returned to plug
    /// the same device back in.
    pub fn plug_device(&mut self, raw_name: &str, backend_id: &str) {
        self.devices.push(FakeDevice { name: raw_name.to_string(), backend_id: backend_id.to_string() });
    }

    /// Pulls out the device listed as `name`; false if it is not listed. Its connection, if open,
    /// goes dead but stays open until `refresh`.
    pub fn unplug(&mut self, name: &str) -> bool {
        let Some(info) = self.list().into_iter().find(|p| p.name == name) else {
            return false;
        };
        self.devices.retain(|d| !(d.backend_id == info.backend_id && d.name == info.raw_name));
        for p in self.table.iter_mut().filter(|p| p.info.same_port(&info)) {
            p.conn.dead = true;
        }
        true
    }

    /// Sends a message on an open, live port, stamped `time_ns`; false (nothing sent) otherwise.
    pub fn inject(&mut self, name: &str, msg: MidiMessage, time_ns: u64) -> bool {
        self.live_sink(name).map(|s| s.send(msg, time_ns)).is_some()
    }

    /// Sends raw bytes on an open, live port through its parser, stamped `time_ns`; false otherwise.
    pub fn inject_bytes(&mut self, name: &str, bytes: &[u8], time_ns: u64) -> bool {
        self.live_sink(name).map(|s| s.feed_at(bytes, time_ns)).is_some()
    }

    fn live_sink(&mut self, name: &str) -> Option<&mut PortSink> {
        self.table.get_mut(name).filter(|p| !p.conn.dead).map(|p| &mut p.conn.sink)
    }
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        self.table.close_all();
    }
}

impl MidiSource for FakeSource {
    fn list(&self) -> Vec<PortInfo> {
        listing(self.devices.iter().map(|d| (d.name.clone(), d.backend_id.clone())))
    }

    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError> {
        let now = self.list();
        let hub = &self.hub;
        self.table.open(&now, name, |c| !c.dead, |id, _| Ok(FakeConn { sink: hub.port_sink(id)?, dead: false }))
    }

    fn close(&mut self, name: &str) -> bool {
        self.table.close(name).is_some()
    }

    fn is_open(&self, name: &str) -> bool {
        self.table.is_open(name)
    }

    fn refresh(&mut self) -> Vec<PortChange> {
        let now = self.list();
        self.table.refresh(&now, |c| !c.dead)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::{eventually, recv};

    fn on(note: u8) -> MidiMessage {
        MidiMessage::NoteOn { ch: 0, note, vel: 100 }
    }

    fn source(devices: &[&str]) -> FakeSource {
        let mut s = FakeSource::new(MidiClock::new()).unwrap();
        for d in devices {
            s.plug(d);
        }
        s
    }

    #[test]
    fn list_names_duplicates_uniquely_with_distinct_backend_ids() {
        let s = source(&["DJ Controller", "Pads", "DJ Controller"]);
        let names: Vec<_> = s.list().into_iter().map(|p| (p.name, p.index)).collect();
        assert_eq!(names, vec![("DJ Controller".into(), 0), ("Pads".into(), 1), ("DJ Controller #2".into(), 2)]);
        let ids: Vec<_> = s.list().into_iter().map(|p| p.backend_id).collect();
        assert_eq!(ids, vec!["fake:0", "fake:1", "fake:2"]);
    }

    #[test]
    fn open_and_close() {
        let mut s = source(&["Pads", "Keys"]);
        let mut sub = s.subscribe(64).unwrap();
        assert!(s.healthy());
        assert!(!s.is_open("Pads"));
        assert!(!s.inject("Pads", on(36), 5), "a closed port delivers nothing");
        let id = s.open("Pads").unwrap();
        assert!(s.is_open("Pads"));
        assert_eq!(s.open("Pads").unwrap(), id, "opening twice is idempotent");
        assert_eq!(s.port_name(id), Some("Pads"));
        assert_eq!(s.backend_id(id), Some("fake:0"));
        assert!(s.inject("Pads", on(36), 5));
        assert!(s.inject_bytes("Pads", &[0x80, 36, 0], 9));
        let got = recv(&mut sub, 2);
        assert_eq!(
            got.iter().map(|e| (e.port, e.msg, e.time_ns)).collect::<Vec<_>>(),
            vec![(id, on(36), 5), (id, MidiMessage::NoteOff { ch: 0, note: 36, vel: 0 }, 9)]
        );
        assert!(s.close("Pads"));
        assert!(!s.close("Pads"));
        assert!(!s.is_open("Pads"));
        assert!(!s.inject("Pads", on(37), 10));
        assert_eq!(s.open("Nope"), Err(MidiIoError::NoSuchPort("Nope".into())));
    }

    #[test]
    fn open_all_opens_every_listed_port_once() {
        let mut s = source(&["A", "B", "A"]);
        s.open("B").unwrap();
        let opened = s.open_all();
        assert_eq!(opened.len(), 2);
        assert!(opened.iter().all(Result::is_ok));
        assert!(["A", "B", "A #2"].iter().all(|n| s.is_open(n)));
        assert!(s.open_all().is_empty());
    }

    #[test]
    fn unplug_kills_the_connection_and_refresh_closes_it() {
        let mut s = source(&["Pads", "Keys"]);
        let mut sub = s.subscribe(64).unwrap();
        let id = s.open("Pads").unwrap();
        assert!(s.unplug("Pads"));
        assert!(!s.unplug("Pads"));
        // Like hardware: still "open" until noticed, but nothing arrives.
        assert!(s.is_open("Pads"));
        assert!(!s.inject("Pads", on(1), 0));
        let changes = s.refresh();
        assert!(changes.contains(&PortChange::Closed { port: id, name: "Pads".into() }), "{changes:?}");
        assert!(!s.is_open("Pads"));
        assert!(!changes.iter().any(|c| matches!(c, PortChange::Disappeared { .. })), "{changes:?}");
        assert!(sub.pop().is_none());
    }

    #[test]
    fn refresh_reports_appeared_and_disappeared_devices() {
        let mut s = source(&["Pads"]);
        let first = s.refresh();
        assert_eq!(first, vec![PortChange::Appeared { name: "Pads".into(), raw_name: "Pads".into(), backend_id: "fake:0".into() }]);
        assert!(s.refresh().is_empty());
        let keys = s.plug("Keys");
        assert_eq!(s.refresh(), vec![PortChange::Appeared { name: "Keys".into(), raw_name: "Keys".into(), backend_id: keys.clone() }]);
        s.unplug("Keys");
        assert_eq!(s.refresh(), vec![PortChange::Disappeared { name: "Keys".into(), raw_name: "Keys".into(), backend_id: keys }]);
    }

    #[test]
    fn replug_and_reopen_by_name_keeps_the_port_id() {
        let mut s = source(&["Pads", "Keys"]);
        let mut sub = s.subscribe(64).unwrap();
        let id = s.open("Pads").unwrap();
        s.unplug("Pads");
        s.refresh();
        // Plugged back in (same device), now at a different index.
        s.plug_device("Pads", "fake:0");
        assert_eq!(s.list().iter().find(|p| p.name == "Pads").map(|p| p.index), Some(1));
        assert_eq!(s.open("Pads").unwrap(), id);
        assert!(s.inject("Pads", on(40), 1));
        assert_eq!(recv(&mut sub, 1).first().map(|e| e.port), Some(id));
    }

    #[test]
    fn open_on_a_dead_connection_reconnects() {
        // Unplugged and plugged back in between two refreshes: `open` must not hand back the dead one.
        let mut s = source(&["Pads"]);
        let mut sub = s.subscribe(64).unwrap();
        let id = s.open("Pads").unwrap();
        s.unplug("Pads");
        s.plug_device("Pads", "fake:0");
        assert!(!s.inject("Pads", on(1), 0), "the old connection is dead");
        assert_eq!(s.open("Pads").unwrap(), id);
        assert!(s.inject("Pads", on(2), 0));
        assert_eq!(recv(&mut sub, 1).first().map(|e| e.msg), Some(on(2)));
        // The dead connection's closing is still reported, so its held notes get released.
        assert!(s.refresh().contains(&PortChange::Closed { port: id, name: "Pads".into() }));
    }

    #[test]
    fn identical_devices_keep_their_ids_when_the_first_is_unplugged() {
        let mut s = source(&["X", "X"]);
        let mut sub = s.subscribe(64).unwrap();
        let first = s.open("X").unwrap();
        let second = s.open("X #2").unwrap();
        assert_ne!(first, second);
        s.unplug("X");
        // The second device is now listed as plain "X".
        assert_eq!(s.list().iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["X"]);
        let changes = s.refresh();
        assert!(changes.contains(&PortChange::Closed { port: first, name: "X".into() }), "{changes:?}");
        assert!(changes.contains(&PortChange::Renamed { port: second, from: "X #2".into(), to: "X".into() }), "{changes:?}");
        // Opening "X" now finds the second device already open: same id, no second connection.
        assert_eq!(s.open("X").unwrap(), second);
        assert!(s.open_all().is_empty());
        assert!(s.is_open("X") && !s.is_open("X #2"));
        assert_eq!(s.port_name(second), Some("X"));
        assert!(s.inject("X", on(9), 0));
        assert_eq!(recv(&mut sub, 1).first().map(|e| e.port), Some(second));
        assert!(eventually(|| s.hop_stats(false).count == 1));
        assert!(sub.pop().is_none(), "delivered exactly once");
    }

    #[test]
    fn ports_of_one_device_keep_separate_identities() {
        // A multi-port device (a Launchpad's MIDI and DAW ports) shares one backend id on WinMM.
        let mut s = source(&[]);
        s.plug_device("LPX MIDI", "usb:lpx");
        s.plug_device("LPX DAW", "usb:lpx");
        let mut sub = s.subscribe(64).unwrap();
        let midi = s.open("LPX MIDI").unwrap();
        let daw = s.open("LPX DAW").unwrap();
        assert_ne!(midi, daw);
        assert!(s.is_open("LPX MIDI") && s.is_open("LPX DAW"));
        assert_eq!((s.raw_name(daw), s.backend_id(daw)), (Some("LPX DAW"), Some("usb:lpx")));
        assert!(s.inject("LPX MIDI", on(1), 0));
        assert!(s.inject("LPX DAW", on(2), 0));
        let mut got: Vec<_> = recv(&mut sub, 2).iter().map(|e| (e.port, e.msg)).collect();
        got.sort_by_key(|(p, _)| *p);
        assert_eq!(got, vec![(midi, on(1)), (daw, on(2))]);
        // Unplugging one port's listing leaves the other open.
        s.unplug("LPX DAW");
        let changes = s.refresh();
        assert_eq!(changes.iter().filter(|c| matches!(c, PortChange::Closed { .. })).count(), 1, "{changes:?}");
        assert!(s.is_open("LPX MIDI") && !s.is_open("LPX DAW"));
    }

    #[test]
    fn the_same_device_is_never_open_twice_under_two_names() {
        // The rename happens without a refresh: `open` reconciles first.
        let mut s = source(&["X", "X"]);
        let second = s.open("X #2").unwrap();
        s.unplug("X");
        assert_eq!(s.open("X").unwrap(), second);
        assert!(!s.is_open("X #2"));
    }

    #[test]
    fn every_subscriber_sees_every_port() {
        let mut s = source(&["Controller", "Pads", "Keys"]);
        let mut audio = s.subscribe(256).unwrap();
        let mut ui = s.subscribe(256).unwrap();
        s.open_all();
        for (i, port) in ["Controller", "Pads", "Keys"].iter().enumerate() {
            assert!(s.inject(port, on(i as u8), i as u64));
        }
        for sub in [&mut audio, &mut ui] {
            let mut ports: Vec<_> = recv(sub, 3).iter().filter_map(|e| s.port_name(e.port).map(String::from)).collect();
            ports.sort();
            assert_eq!(ports, vec!["Controller", "Keys", "Pads"]);
        }
    }

    #[test]
    fn overflow_is_counted_by_the_source() {
        let mut s = source(&["Pads"]);
        let sub = s.subscribe(2).unwrap();
        s.open("Pads").unwrap();
        for i in 0..5 {
            s.inject("Pads", on(i), 0);
        }
        assert!(eventually(|| s.dropped() == 3), "dropped {}", s.dropped());
        assert_eq!(sub.dropped(), 3);
        assert!(eventually(|| s.hop_stats(false).count == 5));
    }
}
