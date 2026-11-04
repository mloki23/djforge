use crate::queue::{Hub, PortSink};
use crate::{MidiClock, MidiIoError, MidiMessage, MidiSource, PortId, PortIds, PortInfo, Subscriber, port_infos, unique_names};

/// A [`MidiSource`] with simulated devices, for tests of this crate and of the layers above it.
///
/// Plug and unplug devices by name, open them like real ports, and inject messages (or raw bytes,
/// through the same parser as the real ports) with chosen times. Events go through the same router
/// and subscriber queues as [`crate::MidiInputs`]; only the device end is simulated.
pub struct FakeSource {
    hub: Hub,
    ids: PortIds,
    /// Plugged devices in list order, by their raw (possibly duplicate) names.
    devices: Vec<String>,
    open: Vec<(String, PortSink)>,
}

impl FakeSource {
    /// An empty source stamping events on `clock`.
    pub fn new(clock: MidiClock) -> Result<Self, MidiIoError> {
        Ok(Self { hub: Hub::new(clock)?, ids: PortIds::default(), devices: Vec::new(), open: Vec::new() })
    }

    /// Plugs in a device; it is listed at the end.
    pub fn plug(&mut self, raw_name: &str) {
        self.devices.push(raw_name.to_string());
    }

    /// Unplugs the device listed as `name` (closing it if open); false if it is not listed.
    pub fn unplug(&mut self, name: &str) -> bool {
        let Some(i) = unique_names(self.devices.iter().cloned()).iter().position(|n| n == name) else {
            return false;
        };
        if i < self.devices.len() {
            self.devices.remove(i);
        }
        self.close(name);
        true
    }

    /// Sends a message on an open port, stamped `time_ns`; false (and nothing sent) if it is not open.
    pub fn inject(&mut self, name: &str, msg: MidiMessage, time_ns: u64) -> bool {
        match self.sink(name) {
            Some(sink) => {
                sink.send(msg, time_ns);
                true
            }
            None => false,
        }
    }

    /// Sends raw bytes on an open port through its parser, stamped `time_ns`; false if not open.
    pub fn inject_bytes(&mut self, name: &str, bytes: &[u8], time_ns: u64) -> bool {
        match self.sink(name) {
            Some(sink) => {
                sink.feed_at(bytes, time_ns);
                true
            }
            None => false,
        }
    }

    fn sink(&mut self, name: &str) -> Option<&mut PortSink> {
        self.open.iter_mut().find(|(n, _)| n == name).map(|(_, s)| s)
    }
}

impl MidiSource for FakeSource {
    fn list(&self) -> Vec<PortInfo> {
        port_infos(unique_names(self.devices.iter().cloned()))
    }

    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError> {
        if self.is_open(name) {
            return Ok(self.ids.id_for(name));
        }
        if !self.list().iter().any(|p| p.name == name) {
            return Err(MidiIoError::NoSuchPort(name.to_string()));
        }
        let id = self.ids.id_for(name);
        let sink = self.hub.port_sink(id)?;
        self.open.push((name.to_string(), sink));
        Ok(id)
    }

    fn close(&mut self, name: &str) -> bool {
        let before = self.open.len();
        self.open.retain(|(n, _)| n != name);
        self.open.len() != before
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
    fn list_names_duplicates_uniquely() {
        let s = source(&["DJ Controller", "Pads", "DJ Controller"]);
        let names: Vec<_> = s.list().into_iter().map(|p| (p.name, p.index)).collect();
        assert_eq!(names, vec![("DJ Controller".into(), 0), ("Pads".into(), 1), ("DJ Controller #2".into(), 2)]);
    }

    #[test]
    fn open_and_close() {
        let mut s = source(&["Pads", "Keys"]);
        let mut sub = s.subscribe(64).unwrap();
        assert!(!s.is_open("Pads"));
        assert!(!s.inject("Pads", on(36), 5), "a closed port delivers nothing");
        let id = s.open("Pads").unwrap();
        assert!(s.is_open("Pads"));
        assert_eq!(s.open("Pads").unwrap(), id, "opening twice is idempotent");
        assert_eq!(s.port_name(id), Some("Pads"));
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
    fn reopen_by_name_after_replug_keeps_the_port_id() {
        let mut s = source(&["Pads", "Keys"]);
        let mut sub = s.subscribe(64).unwrap();
        let id = s.open("Pads").unwrap();
        assert!(s.unplug("Pads"));
        assert!(!s.is_open("Pads"));
        // Plugged back in, now at a different index.
        s.plug("Pads");
        assert_eq!(s.list().iter().find(|p| p.name == "Pads").map(|p| p.index), Some(1));
        assert_eq!(s.open("Pads").unwrap(), id);
        assert!(s.inject("Pads", on(40), 1));
        assert_eq!(recv(&mut sub, 1).first().map(|e| e.port), Some(id));
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
    }
}
