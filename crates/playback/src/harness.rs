//! A player's audio thread driven by hand, for end-to-end tests (the `harness` feature; not part
//! of the app). Callbacks run on the test's thread at a clock it sets, so what plays where is
//! exact: one 256-frame callback (5.33 ms at 48 kHz) renders one 256-sample block of the
//! session. The rig's track plays through an instrument that logs every note it is sent, at the
//! timeline sample it sounds on. Live MIDI comes from a [`FakeSource`] port ("Keys", no real
//! device) or the on-screen keyboard's path.

use crate::capture::{self, MidiCapture};
use crate::{AudioState, Cmd, LiveShared, MeterSnapshot, MidiShared, Shared, bin, session_cmd_with};
use soundcraft_dsp::{Plugin, PluginInfo};
use soundcraft_midi_io::{FakeSource, MidiClock, MidiMessage, MidiSource, Subscriber};
use soundcraft_mix::{ColumnPlay, INSTRUMENT_INSTANCE, LiveCmd, PreparedInstance};
use soundcraft_model::{Insert, Session, TrackId};
use soundcraft_time::{Range, Samples};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

/// Frames per callback.
pub const CALLBACK: usize = 256;
const CALLBACK_NS: u64 = 5_333_333;
const LOGGER: &str = "clap:harness.log";

/// A note the logging instrument was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Played {
    On(u8),
    Off(u8),
}

type Log = Arc<Mutex<Vec<(Played, Samples)>>>;

/// The instrument that logs what it is sent (see the module docs).
struct Logger {
    inner: Box<dyn Plugin>,
    log: Log,
    pos: Samples,
}

impl Logger {
    fn push(&self, offset: usize, p: Played) {
        let at = self.pos.saturating_add(i64::try_from(offset).unwrap_or(0));
        self.log.lock().unwrap_or_else(PoisonError::into_inner).push((p, at));
    }
}

impl Plugin for Logger {
    fn info(&self) -> &'static PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
        self.inner.prepare(sr, mb, ch);
    }
    fn reset(&mut self) {}
    fn set_param(&mut self, id: &str, v: f32) -> bool {
        self.inner.set_param(id, v)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        for c in io.iter_mut() {
            c.iter_mut().take(frames).for_each(|x| *x = 0.0);
        }
    }
    fn set_transport(&mut self, t: &soundcraft_dsp::Transport) {
        self.pos = t.samples;
    }
    fn note_on(&mut self, offset: usize, note: u8, _velocity: u8) {
        self.push(offset, Played::On(note));
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        self.push(offset, Played::Off(note));
    }
}

/// The audio thread of a player, driven by hand (see the module docs).
pub struct Rig {
    tx: Sender<Cmd>,
    st: AudioState,
    log: Log,
    /// The track that plays through the logging instrument; live MIDI's target.
    pub track: TrackId,
    src: FakeSource,
    probe: Subscriber,
    now: u64,
}

impl Rig {
    /// A rig over `session` whose `track` (an instrument track) plays through the logging
    /// instrument and is the live MIDI target. `None` when the fake MIDI source can't start.
    pub fn new(mut session: Session, track: TrackId) -> Option<Rig> {
        session.track_mut(track)?.instrument = Some(Insert::new(LOGGER));
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, _grx) = bin();
        let st = AudioState::new(rx, shared, Arc::new(Session::default()), 48_000.0, gtx);
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let inner = soundcraft_dsp::create("gain")?;
        let logger = Logger { inner, log: Arc::clone(&log), pos: 0 };
        let instance = PreparedInstance { track, slot: INSTRUMENT_INSTANCE, id: LOGGER.into(), ch: 2, plugin: Box::new(logger) };
        let _ = tx.send(session_cmd_with(Arc::new(session), vec![instance], Vec::new(), 0, |_| true));
        let clock = MidiClock::new();
        let mut src = FakeSource::new(clock).ok()?;
        src.plug("Keys");
        src.open("Keys").ok()?;
        let _ = tx.send(Cmd::MidiIn(Some((src.subscribe(4096).ok()?, clock))));
        let _ = tx.send(Cmd::MidiTarget(Some(track)));
        let probe = src.subscribe(4096).ok()?;
        let mut rig = Rig { tx, st, log, track, src, probe, now: 1_000_000_000 };
        rig.callback();
        Some(rig)
    }

    /// One callback, then the clock moves on a callback.
    pub fn callback(&mut self) {
        let mut out = vec![0.0f32; CALLBACK * 2];
        self.st.fill_at(&mut out, 2, self.now);
        self.now += CALLBACK_NS;
    }

    /// Callbacks until the next block starts at `at` or later.
    pub fn render_to(&mut self, at: Samples) {
        while self.st.pos < at {
            self.callback();
        }
    }

    /// Where the next block starts.
    pub fn pos(&self) -> Samples {
        self.st.pos
    }

    /// The session the audio thread plays.
    pub fn session(&self) -> Arc<Session> {
        Arc::clone(&self.st.session)
    }

    /// Plays `session` from the next callback (its instrument stays the logger).
    pub fn update(&mut self, session: Arc<Session>) {
        let _ = self.tx.send(session_cmd_with(session, Vec::new(), Vec::new(), 0, |_| true));
    }

    pub fn play(&mut self, from: Samples, looped: Option<Range>) {
        let _ = self.tx.send(Cmd::Play { from, end: None, looped });
    }

    pub fn locate(&mut self, at: Samples) {
        let _ = self.tx.send(Cmd::Locate(at));
    }

    pub fn recording(&mut self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    /// A capture queue of `n` events (see [`crate::Player::midi_capture`]).
    pub fn capture(&mut self, n: usize) -> MidiCapture {
        let (tx, rx) = capture::queue(n);
        let _ = self.tx.send(Cmd::MidiCapture(Some(tx)));
        rx
    }

    pub fn capture_while_playing(&mut self, on: bool) {
        let _ = self.tx.send(Cmd::MidiCaptureWhilePlaying(on));
    }

    pub fn live(&mut self, cmd: LiveCmd) {
        let _ = self.tx.send(Cmd::Live(cmd));
    }

    /// `msg` from the on-screen keyboard: it plays at the next block's start.
    pub fn keyboard(&mut self, msg: MidiMessage) {
        let _ = self.tx.send(Cmd::MidiPlay(msg));
    }

    /// `msg` from the fake port, stamped so it plays in the next callback. False if the router
    /// never delivered it.
    pub fn port(&mut self, msg: MidiMessage) -> bool {
        if !self.src.inject("Keys", msg, self.now.saturating_sub(4_000_000)) {
            return false;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.probe.pop().is_none() {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// Where the instrument was sent `p`, in order.
    pub fn played(&self, p: Played) -> Vec<Samples> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|e| e.0 == p).map(|e| e.1).collect()
    }

    /// Launches that came after their sample.
    pub fn live_late(&self) -> u32 {
        self.st.mix.live_late()
    }

    /// What Live column `c` plays: (row, start).
    pub fn live_playing(&self, c: usize) -> Option<(usize, Samples)> {
        self.st.mix.live_state().get(c).and_then(|p: &ColumnPlay| p.playing).map(|p| (p.slot, p.start))
    }
}
