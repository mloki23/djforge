//! The real MIDI outputs, through `midir`.
//!
//! A port is opened (connected) on the caller's thread, as inputs are, and its connection is then
//! handed to a sender thread that owns every open connection. [`MidiOutputs::send`] only puts the
//! message on that thread's queue, so a slow or stuck backend never holds up the UI. The queue is
//! bounded by a counter ([`OUT_CAPACITY`] messages waiting): past it, messages are dropped and
//! counted. Closing a port queues its close behind the messages already queued for it, so lights
//! turned off just before a close still go out.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use midir::{MidiOutput, MidiOutputConnection, MidiOutputPort};

use crate::ports::{PortChange, PortTable, listing};
use crate::{MidiIoError, MidiMessage, MidiOutputs, OUT_CAPACITY, PortId, PortInfo};

/// Client name shown to the OS MIDI system.
const CLIENT: &str = "djforge";

/// How long dropping the outputs waits for the sender thread to send what is queued (the lights
/// turned off at exit) before leaving it to finish on its own.
const DRAIN_WAIT: Duration = Duration::from_millis(500);

/// What the sender thread is asked to do.
enum Op {
    Attach(PortId, Box<MidiOutputConnection>),
    Detach(PortId),
    Send(PortId, [u8; 3], usize),
}

/// An open port's handle on this side: dropping it closes the connection on the thread, after
/// the messages queued before.
struct OutConn {
    id: PortId,
    tx: Sender<Op>,
}

impl Drop for OutConn {
    fn drop(&mut self) {
        // The thread has stopped only if the outputs are being dropped: nothing to close then.
        let _ = self.tx.send(Op::Detach(self.id));
    }
}

/// The system's MIDI output ports (WinMM, CoreMIDI, ALSA). See the module docs.
pub struct SystemOutputs {
    // Declared first and cleared in `Drop`, so the connections close before the thread is left.
    table: PortTable<OutConn>,
    tx: Option<Sender<Op>>,
    thread: Option<JoinHandle<()>>,
    /// Messages queued and not yet taken by the thread.
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
}

impl SystemOutputs {
    /// No ports open; starts the sender thread.
    pub fn new() -> Result<Self, MidiIoError> {
        let (tx, rx) = channel();
        let pending = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let (p, d) = (Arc::clone(&pending), Arc::clone(&dropped));
        let thread = std::thread::Builder::new()
            .name("midi-out".into())
            .spawn(move || sender(&rx, &p, &d))
            .map_err(|e| MidiIoError::Router(format!("MIDI output thread: {e}")))?;
        Ok(Self { table: PortTable::default(), tx: Some(tx), thread: Some(thread), pending, dropped })
    }

    /// A fresh backend client and its ports, listed. midir's enumeration can panic (it asserts on
    /// WinMM); that becomes an error.
    fn enumerate() -> Result<(MidiOutput, Vec<(PortInfo, MidiOutputPort)>), String> {
        std::panic::catch_unwind(|| {
            let output = MidiOutput::new(CLIENT).map_err(|e| e.to_string())?;
            let ports = output.ports();
            let infos = listing(ports.iter().map(|p| (output.port_name(p).unwrap_or_else(|_| "Unknown MIDI output".to_string()), p.id())));
            Ok((output, infos.into_iter().zip(ports).collect()))
        })
        .unwrap_or_else(|_| Err("the MIDI backend panicked while listing outputs".to_string()))
    }
}

/// The sender thread: owns the connections and sends in queue order until the queue closes.
fn sender(rx: &Receiver<Op>, pending: &AtomicUsize, dropped: &AtomicU64) {
    let mut conns: Vec<(PortId, Box<MidiOutputConnection>)> = Vec::new();
    let mut warned = false;
    while let Ok(op) = rx.recv() {
        match op {
            Op::Attach(id, conn) => {
                conns.retain(|(c, _)| *c != id);
                conns.push((id, conn));
            }
            Op::Detach(id) => {
                if let Some(i) = conns.iter().position(|(c, _)| *c == id) {
                    let (_, conn) = conns.swap_remove(i);
                    conn.close();
                }
            }
            Op::Send(id, bytes, n) => {
                pending.fetch_sub(1, Ordering::Relaxed);
                let Some((_, conn)) = conns.iter_mut().find(|(c, _)| *c == id) else { continue };
                if let Err(e) = conn.send(bytes.get(..n).unwrap_or_default()) {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    if !warned {
                        log::warn!("MIDI output: {e}");
                        warned = true;
                    }
                }
            }
        }
    }
    for (_, conn) in conns {
        conn.close();
    }
}

impl Drop for SystemOutputs {
    fn drop(&mut self) {
        self.table.close_all();
        // The last sender gone: the thread sends what is queued, closes and ends.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let start = Instant::now();
            while !t.is_finished() && start.elapsed() < DRAIN_WAIT {
                std::thread::sleep(Duration::from_millis(2));
            }
            if t.is_finished() && t.join().is_err() {
                log::warn!("MIDI output thread ended abnormally");
            }
        }
    }
}

impl MidiOutputs for SystemOutputs {
    fn list(&self) -> Vec<PortInfo> {
        match Self::enumerate() {
            Ok((_, ports)) => ports.into_iter().map(|(info, _)| info).collect(),
            Err(e) => {
                log::warn!("listing MIDI outputs failed: {e}");
                Vec::new()
            }
        }
    }

    fn open(&mut self, name: &str) -> Result<PortId, MidiIoError> {
        let backend = |reason: String| MidiIoError::Backend { port: name.to_string(), reason };
        let (output, ports) = Self::enumerate().map_err(backend)?;
        let now: Vec<PortInfo> = ports.iter().map(|(info, _)| info.clone()).collect();
        let tx = self.tx.clone().ok_or_else(|| MidiIoError::Router("the MIDI output thread has stopped".into()))?;
        if !now.iter().any(|p| p.name == name) {
            return Err(MidiIoError::NoSuchOutput(name.to_string()));
        }
        self.table.open(
            &now,
            name,
            |_| true,
            |id, info| {
                let port =
                    ports.iter().find(|(i, _)| i.same_port(info)).map(|(_, p)| p).ok_or_else(|| MidiIoError::NoSuchOutput(name.to_string()))?;
                let connect = std::panic::AssertUnwindSafe(move || output.connect(port, "djforge-out").map_err(|e| e.to_string()));
                let conn = std::panic::catch_unwind(connect)
                    .unwrap_or_else(|_| Err("the MIDI backend panicked while connecting".to_string()))
                    .map_err(backend)?;
                tx.send(Op::Attach(id, Box::new(conn))).map_err(|_| MidiIoError::Router("the MIDI output thread has stopped".into()))?;
                Ok(OutConn { id, tx: tx.clone() })
            },
        )
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
        let (Some(tx), Some(p)) = (&self.tx, self.table.by_id(id)) else { return false };
        if self.pending.load(Ordering::Relaxed) >= OUT_CAPACITY {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let (bytes, n) = msg.wire();
        self.pending.fetch_add(1, Ordering::Relaxed);
        if tx.send(Op::Send(p.conn.id, bytes, n)).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn refresh(&mut self) -> Vec<PortChange> {
        match Self::enumerate() {
            Ok((_, ports)) => {
                let now: Vec<PortInfo> = ports.into_iter().map(|(info, _)| info).collect();
                self.table.refresh(&now, |_| true)
            }
            Err(e) => {
                log::warn!("listing MIDI outputs failed: {e}");
                Vec::new()
            }
        }
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
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No port is opened (tests never touch real devices): the thread starts, refuses sends to
    /// ports that aren't open, and stops promptly when dropped.
    #[test]
    fn the_sender_thread_starts_refuses_unopened_ports_and_stops() {
        let mut o = SystemOutputs::new().unwrap();
        assert!(!o.send(PortId(0), MidiMessage::NoteOn { ch: 0, note: 1, vel: 127 }));
        assert!(!o.is_open_id(PortId(0)));
        assert_eq!(o.dropped(), 0);
        let start = Instant::now();
        drop(o);
        assert!(start.elapsed() < DRAIN_WAIT);
    }
}
