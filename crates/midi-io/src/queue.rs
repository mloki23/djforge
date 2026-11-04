//! The MIDI event queue: port callbacks → router thread → one bounded queue per subscriber.
//!
//! ```text
//!  port callback ──SPSC──┐                 ┌──SPSC──▶ Subscriber (audio thread)
//!  port callback ──SPSC──┼──▶ router ──────┼──SPSC──▶ Subscriber (UI dispatcher)
//!  FakeSource    ──SPSC──┘    thread       └── ...
//! ```
//!
//! Every queue is an `rtrb` single-producer single-consumer ring buffer, so each side has exactly
//! one owner: a port's [`PortSink`] lives inside its device callback, the router owns every port
//! consumer and every subscriber producer, and each [`Subscriber`] is owned by its consumer thread.
//! Push and pop are wait-free and never allocate. The router is woken by `Thread::unpark` after each
//! push (no lock, no allocation), so delivery latency is a thread wake-up, not a polling period.
//! A full queue drops the event and counts it; nothing ever blocks a producer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::{MidiClock, MidiEvent, MidiIoError, MidiMessage, MidiParser, PortId};

/// Per-port queue capacity (port callback → router). A burst larger than this between two router
/// wake-ups is dropped and counted.
pub const PORT_CAPACITY: usize = 1024;
/// Largest capacity a subscriber may ask for; larger requests are clamped.
pub const MAX_CAPACITY: usize = 1 << 16;
/// How long the idle router sleeps before re-checking for closed ports and dropped subscribers.
const IDLE: Duration = Duration::from_millis(100);

fn clamp_capacity(capacity: usize) -> usize {
    capacity.clamp(1, MAX_CAPACITY)
}

/// The receiving end of one consumer's queue. Move it to the consuming thread.
///
/// [`Subscriber::pop`] is wait-free and does not allocate, so it is safe on the audio thread.
/// Dropping a subscriber unsubscribes it.
pub struct Subscriber {
    rx: Consumer<MidiEvent>,
    dropped: Arc<AtomicU64>,
}

impl Subscriber {
    /// The next event, oldest first, if any.
    pub fn pop(&mut self) -> Option<MidiEvent> {
        self.rx.pop().ok()
    }

    /// Events waiting in the queue.
    pub fn len(&self) -> usize {
        self.rx.slots()
    }

    /// True when no event is waiting.
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }

    /// The queue's capacity.
    pub fn capacity(&self) -> usize {
        self.rx.buffer().capacity()
    }

    /// Events dropped because this subscriber's queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The producing end for one open port: parses bytes and pushes events. Owned by the port's callback.
pub(crate) struct PortSink {
    tx: Producer<MidiEvent>,
    router: Thread,
    dropped: Arc<AtomicU64>,
    port: PortId,
    clock: MidiClock,
    parser: MidiParser,
}

impl PortSink {
    /// Parses `bytes` received now (time from the shared clock) and queues the messages.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        let t = self.clock.now_ns();
        self.feed_at(bytes, t);
    }

    /// Parses `bytes` and queues the messages stamped `time_ns`.
    pub(crate) fn feed_at(&mut self, bytes: &[u8], time_ns: u64) {
        let mut any = false;
        for &b in bytes {
            if let Some(msg) = self.parser.push(b) {
                self.enqueue(msg, time_ns);
                any = true;
            }
        }
        if any {
            self.router.unpark();
        }
    }

    /// Queues one already-parsed message.
    pub(crate) fn send(&mut self, msg: MidiMessage, time_ns: u64) {
        self.enqueue(msg, time_ns);
        self.router.unpark();
    }

    fn enqueue(&mut self, msg: MidiMessage, time_ns: u64) {
        if self.tx.push(MidiEvent { port: self.port, msg, time_ns }).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Registration sent to the router.
enum Ctl {
    Port(Consumer<MidiEvent>),
    Subscriber(Producer<MidiEvent>, Arc<AtomicU64>),
}

/// Owns the router thread. Dropping it stops the router; subscribers then receive nothing more.
pub(crate) struct Hub {
    ctl: Option<Sender<Ctl>>,
    router: Option<JoinHandle<()>>,
    waker: Thread,
    dropped: Arc<AtomicU64>,
    clock: MidiClock,
}

impl Hub {
    /// Starts the router thread.
    pub(crate) fn new(clock: MidiClock) -> Result<Self, MidiIoError> {
        let (ctl, rx) = mpsc::channel();
        let dropped = Arc::new(AtomicU64::new(0));
        let total = Arc::clone(&dropped);
        let router =
            thread::Builder::new().name("midi-router".into()).spawn(move || route(&rx, &total)).map_err(|e| MidiIoError::Router(e.to_string()))?;
        let waker = router.thread().clone();
        Ok(Self { ctl: Some(ctl), router: Some(router), waker, dropped, clock })
    }

    pub(crate) fn clock(&self) -> MidiClock {
        self.clock
    }

    /// All events dropped so far: full port queues plus full subscriber queues.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// A new subscriber receiving every event routed from now on.
    pub(crate) fn subscribe(&self, capacity: usize) -> Result<Subscriber, MidiIoError> {
        let (tx, rx) = RingBuffer::new(clamp_capacity(capacity));
        let dropped = Arc::new(AtomicU64::new(0));
        self.register(Ctl::Subscriber(tx, Arc::clone(&dropped)))?;
        Ok(Subscriber { rx, dropped })
    }

    /// A sink for a newly opened port. Dropping the sink retires the port's queue.
    pub(crate) fn port_sink(&self, port: PortId) -> Result<PortSink, MidiIoError> {
        let (tx, rx) = RingBuffer::new(PORT_CAPACITY);
        self.register(Ctl::Port(rx))?;
        Ok(PortSink { tx, router: self.waker.clone(), dropped: Arc::clone(&self.dropped), port, clock: self.clock, parser: MidiParser::new() })
    }

    fn register(&self, ctl: Ctl) -> Result<(), MidiIoError> {
        let sender = self.ctl.as_ref().ok_or_else(|| MidiIoError::Router("stopped".into()))?;
        sender.send(ctl).map_err(|_| MidiIoError::Router("the router thread has stopped".into()))?;
        self.waker.unpark();
        Ok(())
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // Closing the control channel tells the router to exit.
        self.ctl = None;
        self.waker.unpark();
        if let Some(h) = self.router.take()
            && h.join().is_err()
        {
            log::warn!("midi router thread ended abnormally");
        }
    }
}

/// The router loop: register new queues, move events from every port to every subscriber, reap
/// closed ports and dropped subscribers, sleep until woken.
fn route(ctl: &Receiver<Ctl>, dropped: &AtomicU64) {
    let mut ports: Vec<Consumer<MidiEvent>> = Vec::new();
    let mut subs: Vec<(Producer<MidiEvent>, Arc<AtomicU64>)> = Vec::new();
    loop {
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Port(c)) => ports.push(c),
                Ok(Ctl::Subscriber(p, d)) => subs.push((p, d)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        for port in &mut ports {
            while let Ok(ev) = port.pop() {
                for (tx, d) in &mut subs {
                    if tx.push(ev).is_err() {
                        d.fetch_add(1, Ordering::Relaxed);
                        dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        // A port whose sink is gone can push no more; retire it once drained (abandoned first,
        // then empty, so an event pushed just before the sink dropped is not lost).
        ports.retain(|c| !(c.is_abandoned() && c.is_empty()));
        subs.retain(|(p, _)| !p.is_abandoned());
        // An unpark since the last park makes this return at once, so no wake-up is lost.
        thread::park_timeout(IDLE);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Instant;

    /// Pops `n` events, waiting up to two seconds for the router to deliver them.
    pub(crate) fn recv(sub: &mut Subscriber, n: usize) -> Vec<MidiEvent> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut out = Vec::new();
        while out.len() < n && Instant::now() < deadline {
            match sub.pop() {
                Some(ev) => out.push(ev),
                None => thread::yield_now(),
            }
        }
        out
    }

    /// Waits until `f` holds, up to two seconds.
    pub(crate) fn eventually(f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            thread::yield_now();
        }
        f()
    }

    fn note(n: u8) -> MidiMessage {
        MidiMessage::NoteOn { ch: 0, note: n, vel: 100 }
    }

    #[test]
    fn events_reach_a_subscriber_in_order_with_their_times() {
        let hub = Hub::new(MidiClock::new()).unwrap();
        let mut sub = hub.subscribe(64).unwrap();
        let mut sink = hub.port_sink(PortId(3)).unwrap();
        sink.feed_at(&[0x90, 60, 100, 62, 100], 1_000);
        sink.send(note(64), 2_000);
        let got = recv(&mut sub, 3);
        assert_eq!(got.iter().map(|e| e.time_ns).collect::<Vec<_>>(), vec![1_000, 1_000, 2_000]);
        assert!(got.iter().all(|e| e.port == PortId(3)));
        assert_eq!(got.last().map(|e| e.msg), Some(note(64)));
    }

    #[test]
    fn fan_out_delivers_to_every_subscriber() {
        let hub = Hub::new(MidiClock::new()).unwrap();
        let mut subs: Vec<Subscriber> = (0..3).map(|_| hub.subscribe(64).unwrap()).collect();
        let mut a = hub.port_sink(PortId(0)).unwrap();
        let mut b = hub.port_sink(PortId(1)).unwrap();
        for i in 0..5 {
            a.send(note(i), u64::from(i));
            b.send(note(i + 10), u64::from(i));
        }
        for sub in &mut subs {
            let got = recv(sub, 10);
            assert_eq!(got.len(), 10);
            // Per port, order is preserved.
            let from_a: Vec<_> = got.iter().filter(|e| e.port == PortId(0)).map(|e| e.msg).collect();
            assert_eq!(from_a, (0..5).map(note).collect::<Vec<_>>());
        }
        assert_eq!(hub.dropped(), 0);
    }

    #[test]
    fn a_full_subscriber_queue_drops_and_counts_without_blocking_others() {
        let hub = Hub::new(MidiClock::new()).unwrap();
        let small = hub.subscribe(4).unwrap();
        let mut big = hub.subscribe(64).unwrap();
        let mut sink = hub.port_sink(PortId(0)).unwrap();
        for i in 0..10 {
            sink.send(note(i), 0);
        }
        assert_eq!(recv(&mut big, 10).len(), 10);
        assert!(eventually(|| small.dropped() == 6), "dropped {}", small.dropped());
        assert_eq!(small.len(), 4);
        assert_eq!(big.dropped(), 0);
        assert_eq!(hub.dropped(), 6);
    }

    #[test]
    fn a_full_port_queue_drops_and_counts() {
        // A sink with no router behind it: its queue fills and the rest is counted.
        let (tx, mut rx) = RingBuffer::new(2);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut sink = PortSink {
            tx,
            router: thread::current(),
            dropped: Arc::clone(&dropped),
            port: PortId(0),
            clock: MidiClock::new(),
            parser: MidiParser::new(),
        };
        sink.feed_at(&[0x90, 1, 1, 2, 1, 3, 1, 4, 1, 5, 1], 7);
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
        // The oldest events are kept; the overflow is what was dropped.
        assert_eq!(rx.pop().ok().map(|e| (e.msg, e.time_ns)), Some((MidiMessage::NoteOn { ch: 0, note: 1, vel: 1 }, 7)));
        assert_eq!(rx.pop().ok().map(|e| e.msg), Some(MidiMessage::NoteOn { ch: 0, note: 2, vel: 1 }));
        assert!(rx.pop().is_err());
    }

    #[test]
    fn capacity_is_clamped() {
        let hub = Hub::new(MidiClock::new()).unwrap();
        assert_eq!(hub.subscribe(0).unwrap().capacity(), 1);
        assert_eq!(hub.subscribe(usize::MAX).unwrap().capacity(), MAX_CAPACITY);
    }

    #[test]
    fn dropped_subscribers_and_closed_ports_do_not_stall_the_router() {
        let hub = Hub::new(MidiClock::new()).unwrap();
        drop(hub.subscribe(1).unwrap());
        drop(hub.port_sink(PortId(0)).unwrap());
        let mut sub = hub.subscribe(8).unwrap();
        let mut sink = hub.port_sink(PortId(1)).unwrap();
        sink.send(note(1), 0);
        assert_eq!(recv(&mut sub, 1).len(), 1);
    }
}
