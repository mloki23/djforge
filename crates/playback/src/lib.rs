//! Realtime playback through the system audio device (cpal: CoreAudio, WASAPI, ALSA/Pulse/
//! PipeWire via ALSA, JACK, WebAudio).
//!
//! The UI thread owns a [`Player`]; the audio callback owns the [`MixEngine`] and a snapshot of
//! the session (`Arc<Session>`) that the UI swaps whenever the document changes. Commands go to
//! the callback through a channel; position and meters come back through atomics and a
//! `try_lock`ed snapshot, so the callback never blocks. When no device is available the player
//! falls back to a silent clock thread so the transport, meters and automation still run.
//!
//! Surround: the device is opened with as many output channels as it offers, up to the width of
//! the session's main format. Main channels (SMPTE/WAV order) go to device channels in order; when
//! the device has fewer channels than the main mix, the mix is folded down to stereo (ITU-R
//! BS.775) on the first two device channels.
//!
//! Plugins and threads: hosted third-party plugins (CLAP/VST3) are created, given their stored
//! state and prepared on the thread that calls [`Player::update_session`] (the UI thread), and
//! travel to the audio thread together with the session snapshot that needs them; the mix
//! engine adopts them (see `soundcraft_mix`'s crate docs). The stretchers of beat-locked clips
//! are built the same way. Instances and stretchers the engine retires, and the session snapshots
//! the callback replaces, come back on return channels and are dropped on the UI thread
//! ([`Player::idle`]). Plugin editors are driven, and plugin states for saving read
//! ([`Player::capture_states`]), on the UI thread
//! through handles taken from each instance before it leaves ([`Player::open_editor`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod capture;
pub mod devices;
#[cfg(feature = "harness")]
#[doc(hidden)]
pub mod harness;
pub mod midi_in;
pub mod record;

pub use capture::{CAPTURE_CAPACITY, CaptureEvent, Captured, MidiCapture, TakePass, passes as take_passes};
pub use devices::AudioConfig;
pub use midi_in::{CONTROL_PORTS, Control as MidiControl, KEYBOARD_SOURCE, MIDI_SLACK_NS, MidiControls, PENDING as MIDI_PENDING};

use soundcraft_dsp::{Plugin, PluginEditor, PluginStateReader};
use soundcraft_midi_io::{MidiClock, MidiMessage, PortId, Subscriber};
use soundcraft_mix::{
    ColumnPlay, LiveCmd, LiveEvent, MAX_CHANNELS, MixEngine, PlayerStem, PreparedInstance, PreparedWarp, StripMeter, WarpPlayer, WarpRoom,
    itu_stereo_fold, main_channels,
};
use soundcraft_model::{MAX_LIVE_COLUMNS, Session, SourceId, TrackId};
use soundcraft_time::{Range, Samples};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

const BLOCK: usize = 512;

/// Room for replaced snapshots on their way back; `update_session` empties it before each send.
const OLD_SESSIONS: usize = 16;
/// Warp rooms in flight back to the UI thread (one per snapshot or batch, drained every frame).
/// The channel is bounded so sending never allocates on the audio thread.
const ROOMS: usize = 64;

/// The realtime audio thread marker lives in the mix engine, which carries it into the strips it
/// renders on worker threads; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

/// Meter values for the UI.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
}

/// Plugin states read from the live instances: `(track, slot or `soundcraft_mix::INSTRUMENT_INSTANCE`, state)`.
pub type PluginStates = Vec<(TrackId, usize, Vec<u8>)>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

enum Cmd {
    /// A new snapshot plus the third-party instances and warp players it needs, and the room to
    /// hold its warp players (all created on the UI thread). See [`session_cmd`].
    Session(Arc<Session>, Vec<PreparedInstance>, Vec<PreparedWarp>, WarpRoom),
    /// More warp players for the current snapshot, built progressively from [`Player::idle`],
    /// with room to retire as many. See [`warps_cmd`].
    Warps(Vec<PreparedWarp>, WarpRoom),
    /// Where the callback sends the snapshots it replaces (freeing one takes thousands of frees).
    OldSessions(SyncSender<Arc<Session>>),
    Input(Arc<record::InputRing>),
    Recording(bool),
    Play {
        from: Samples,
        end: Option<Samples>,
        looped: Option<Range>,
    },
    Stop,
    Locate(Samples),
    /// A Live slot launch or stop at its sample (see [`Player::live`]).
    Live(LiveCmd),
    /// Live MIDI input: the queue to drain and the clock its events are stamped on. The one it
    /// replaces goes back to be dropped off the audio thread.
    MidiIn(Option<(Subscriber, MidiClock)>),
    /// The track live MIDI plays (see [`Player::set_midi_target`]).
    MidiTarget(Option<TrackId>),
    /// A MIDI port's device went away: release its live keys.
    MidiClosed(PortId),
    /// All live notes off and the controllers reset.
    MidiPanic,
    /// A message from the on-screen keyboard, played at the next block's start.
    MidiPlay(MidiMessage),
    /// Where the live events the target plays are captured (see [`capture`]). The queue it
    /// replaces goes back to be dropped off the audio thread.
    MidiCapture(Option<rtrb::Producer<Captured>>),
    /// Capture whenever the transport plays, not only while recording onto an armed target.
    MidiCaptureWhilePlaying(bool),
}

/// Live MIDI figures the audio thread publishes, and what the player last sent it.
#[derive(Default)]
struct MidiShared {
    /// The live latency `L` (see [`midi_in`]).
    latency_ns: AtomicU64,
    /// Live events dropped (full pending buffer or engine inbox).
    dropped: AtomicU64,
    /// The target last sent (`u64::MAX`: none), so an unchanged target is not resent.
    target: AtomicU64,
    /// A MIDI input queue was handed to this player's audio thread.
    has_input: AtomicBool,
    /// Captured events dropped because the capture queue was full.
    capture_dropped: AtomicU64,
    /// A capture queue was handed to this player's audio thread.
    has_capture: AtomicBool,
    /// The notes and controllers mapped to controls, which don't play the target.
    controls: midi_in::MidiControls,
}

impl MidiShared {
    fn new() -> MidiShared {
        MidiShared { target: AtomicU64::new(u64::MAX), ..MidiShared::default() }
    }
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    meters: Mutex<MeterSnapshot>,
    /// What each Live column plays and has queued, published by the audio thread.
    live: LiveShared,
    midi: MidiShared,
}

/// What a Live column plays and has queued, as [`Player::live_state`] reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveColumnState {
    /// The row playing and the sample it started at.
    pub playing: Option<(usize, Samples)>,
    /// A launch (`Some(row)`) or stop (`None`) waiting for its sample.
    pub queued: Option<(Option<usize>, Samples)>,
    /// Launches the audio thread saw only after their sample, so far (they missed their start
    /// and play on the grid from where they were seen).
    pub late: u32,
    /// Where the playing slot's loop phase counts from (see `soundcraft_mix::LivePlaying::origin`):
    /// unlike its start, a transport wrap or locate does not move it.
    pub origin: Samples,
}

impl LiveColumnState {
    fn of(c: &ColumnPlay) -> LiveColumnState {
        LiveColumnState {
            playing: c.playing.map(|p| (p.slot, p.start)),
            queued: c.queued.map(|q| (q.slot, q.at)),
            late: c.late,
            origin: c.playing.map_or(0, |p| p.origin),
        }
    }
}

/// [`LiveColumnState`]s shared lock-free by the audio thread: a sequence counter (odd while it
/// writes) around six words per column (playing row or -1, its start, queued row or -1 for a
/// stop or -2 for none, its sample, late launches, the phase origin).
struct LiveShared {
    seq: AtomicU64,
    words: [[AtomicI64; 6]; MAX_LIVE_COLUMNS],
    /// The last snapshot read whole, for a read that keeps meeting a write (UI side only).
    last: Mutex<[LiveColumnState; MAX_LIVE_COLUMNS]>,
    /// The longest audio callback seen, in session samples.
    max_callback: AtomicI64,
}

impl Default for LiveShared {
    fn default() -> Self {
        LiveShared {
            seq: AtomicU64::new(0),
            words: std::array::from_fn(|_| std::array::from_fn(|i| AtomicI64::new(if i >= 4 { 0 } else { -2 }))),
            last: Mutex::new([LiveColumnState::default(); MAX_LIVE_COLUMNS]),
            max_callback: AtomicI64::new(0),
        }
    }
}

impl LiveShared {
    /// Publishes `state` (audio thread: no allocation, no lock).
    fn publish(&self, state: &[ColumnPlay]) {
        self.seq.fetch_add(1, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        for (w, c) in self.words.iter().zip(state.iter().map(LiveColumnState::of).chain(std::iter::repeat(LiveColumnState::default()))) {
            let row = |r: usize| i64::try_from(r).unwrap_or(-1);
            let (p, start) = c.playing.map_or((-1, 0), |(r, at)| (row(r), at));
            let (q, at) = match c.queued {
                None => (-2, 0),
                Some((None, at)) => (-1, at),
                Some((Some(r), at)) => (row(r), at),
            };
            for (a, v) in w.iter().zip([p, start, q, at, i64::from(c.late), c.origin]) {
                a.store(v, Ordering::Relaxed);
            }
        }
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// The last state published (a few retries when a write is under way; then the last state
    /// read whole).
    fn read(&self) -> [LiveColumnState; MAX_LIVE_COLUMNS] {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = [LiveColumnState::default(); MAX_LIVE_COLUMNS];
        for _ in 0..64 {
            let before = self.seq.load(Ordering::Acquire);
            for (o, w) in out.iter_mut().zip(&self.words) {
                let [p, start, q, at, late, origin] = [0, 1, 2, 3, 4, 5].map(|i| w.get(i).map_or(-2, |a| a.load(Ordering::Relaxed)));
                let row = |r: i64| usize::try_from(r).ok();
                *o = LiveColumnState {
                    playing: row(p).map(|r| (r, start)),
                    queued: match q {
                        -2 => None,
                        -1 => Some((None, at)),
                        r => Some((row(r), at)),
                    },
                    late: u32::try_from(late).unwrap_or(0),
                    origin,
                };
            }
            std::sync::atomic::fence(Ordering::Acquire);
            if before.is_multiple_of(2) && self.seq.load(Ordering::Relaxed) == before {
                *last = out;
                return out;
            }
            std::hint::spin_loop();
        }
        *last
    }
}

/// What stopped the audio output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTrouble {
    /// A block panicked; the output stays silent.
    Bug,
    /// The device went away.
    Gone,
    /// The device stopped asking for audio while playing.
    Stalled,
}

/// How the audio output is doing, for the UI.
#[derive(Default)]
struct Fault {
    /// A block panicked: output stays silent from then on.
    failed: AtomicBool,
    gone: AtomicBool,
    /// Blocks so far, to tell a device that stopped asking for audio.
    beats: std::sync::atomic::AtomicU64,
    /// The UI has been told.
    reported: AtomicBool,
}

impl Fault {
    /// What stopped the output, once.
    fn trouble(&self, stalled: bool) -> Option<OutputTrouble> {
        let trouble = if self.failed.load(Ordering::Relaxed) {
            OutputTrouble::Bug
        } else if self.gone.load(Ordering::Relaxed) {
            OutputTrouble::Gone
        } else if stalled {
            OutputTrouble::Stalled
        } else {
            return None;
        };
        (!self.reported.swap(true, Ordering::Relaxed)).then_some(trouble)
    }
}

/// How long a playing output may go without a block before it counts as stalled.
#[cfg(not(target_arch = "wasm32"))]
const STALL: std::time::Duration = std::time::Duration::from_secs(2);

/// True once the block count `beats` has stood still for `STALL` while playing; `seen` is the
/// count last seen and since when.
#[cfg(not(target_arch = "wasm32"))]
fn stalled(seen: &mut Option<(u64, std::time::Instant)>, beats: u64, playing: bool) -> bool {
    match *seen {
        Some((b, since)) if playing && b == beats => since.elapsed() >= STALL,
        _ => {
            *seen = Some((beats, std::time::Instant::now()));
            false
        }
    }
}

/// Fill one block with `fill`. If it panics, the block and every later one are silent and the
/// transport stops, rather than the panic taking the app down.
fn guarded_block(fault: &Fault, shared: &Shared, out: &mut [f32], fill: impl FnOnce(&mut [f32])) {
    fault.beats.fetch_add(1, Ordering::Relaxed);
    if fault.failed.load(Ordering::Relaxed) || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fill(out))).is_err() {
        fault.failed.store(true, Ordering::Relaxed);
        shared.playing.store(false, Ordering::Relaxed);
        out.fill(0.0);
    }
}

/// State owned by the audio callback.
struct AudioState {
    rx: Receiver<Cmd>,
    shared: Arc<Shared>,
    session: Arc<Session>,
    mix: MixEngine,
    pos: Samples,
    playing: bool,
    end: Option<Samples>,
    looped: Option<Range>,
    /// Rendered frames waiting to be consumed (planar, main channels) and the fractional read index.
    pending: Vec<Vec<f32>>,
    /// Stereo fold-down gains per main channel (used when the device has fewer channels).
    fold: [(f32, f32); MAX_CHANNELS],
    pending_len: usize,
    read: f64,
    device_rate: f64,
    input: Option<Arc<record::InputRing>>,
    recording: bool,
    /// Retired plugin instances and warp players go back to the UI thread to be dropped there.
    garbage: Bin,
    old_sessions: Option<SyncSender<Arc<Session>>>,
    /// A spent warp room the full `rooms` channel refused, sent again on the next callback.
    room_backlog: Option<WarpRoom>,
    /// The Live state last published to [`Shared::live`].
    live_seen: [ColumnPlay; MAX_LIVE_COLUMNS],
    /// Live MIDI input.
    midi: midi_in::MidiIn,
    /// A replaced MIDI queue the full `subs` channel refused, sent again on the next callback.
    sub_backlog: Option<Subscriber>,
    /// Where live events the target plays are captured (see [`capture`]).
    capture: Option<rtrb::Producer<Captured>>,
    /// Capture whenever playing, armed or not.
    capture_playing: bool,
    /// Captured events dropped (full queue).
    capture_dropped: u64,
    /// Which pass of the timeline capture is in: a transport jump starts the next.
    capture_pass: u32,
    /// A replaced capture queue the full `captures` channel refused, sent again later.
    capture_backlog: Option<rtrb::Producer<Captured>>,
}

/// Return channels for what the audio thread takes out of service, to be dropped on the UI thread.
struct Bin {
    plugins: Sender<Vec<Box<dyn Plugin>>>,
    warps: Sender<Vec<WarpPlayer>>,
    /// Emptied warp-player batches, so their buffers are freed off the audio thread too.
    batches: Sender<Vec<PreparedWarp>>,
    /// Replaced warp tables and retired lists (with the players that no longer play). Bounded:
    /// see [`ROOMS`].
    rooms: SyncSender<WarpRoom>,
    /// Replaced MIDI input queues (dropping one may free it). Bounded: sending never allocates.
    subs: SyncSender<Subscriber>,
    /// Replaced capture queues (dropping one may free it). Bounded: sending never allocates.
    captures: SyncSender<rtrb::Producer<Captured>>,
}

/// The UI-thread ends of a [`Bin`].
struct BinRx {
    plugins: Receiver<Vec<Box<dyn Plugin>>>,
    warps: Receiver<Vec<WarpPlayer>>,
    batches: Receiver<Vec<PreparedWarp>>,
    rooms: Receiver<WarpRoom>,
    subs: Receiver<Subscriber>,
    captures: Receiver<rtrb::Producer<Captured>>,
}

/// Replaced MIDI queues in flight back to the UI thread.
const SUBS: usize = 8;

fn bin() -> (Bin, BinRx) {
    let (ptx, prx) = channel();
    let (wtx, wrx) = channel();
    let (btx, brx) = channel();
    let (rtx, rrx) = sync_channel(ROOMS);
    let (stx, srx) = sync_channel(SUBS);
    let (ctx, crx) = sync_channel(SUBS);
    (
        Bin { plugins: ptx, warps: wtx, batches: btx, rooms: rtx, subs: stx, captures: ctx },
        BinRx { plugins: prx, warps: wrx, batches: brx, rooms: rrx, subs: srx, captures: crx },
    )
}

/// Most clips whose warp players are built per [`Player::update_session`] call (nearest the
/// playhead first), so loading a big set doesn't stall the UI; the rest follow from
/// [`Player::idle`]. A clip's players always come together: a split clip brings four.
const WARP_BATCH: usize = 8;

/// Most clips whose warp players are built per [`Player::idle`] call (a UI frame).
const WARP_IDLE_BATCH: usize = 2;

/// A snapshot command, with a warp room sized for the snapshot's players (built here, off the
/// audio thread) and for retiring the batch's replaced players plus `stale` more (players of
/// clips and tracks the snapshot no longer has).
///
/// Live-slot storage goes only to the column tracks `need_store` names (the audio thread keeps
/// the storage it has).
fn session_cmd_with(
    s: Arc<Session>,
    instances: Vec<PreparedInstance>,
    warps: Vec<PreparedWarp>,
    stale: usize,
    need_store: impl Fn(TrackId) -> bool,
) -> Cmd {
    let room = WarpRoom::for_session_with(&s, warps.len().saturating_add(stale), need_store);
    Cmd::Session(s, instances, warps, room)
}

/// [`session_cmd_with`] with nothing known to have gone, and storage for every column.
#[cfg(test)]
fn session_cmd(s: Arc<Session>, instances: Vec<PreparedInstance>, warps: Vec<PreparedWarp>) -> Cmd {
    session_cmd_with(s, instances, warps, 0, |_| true)
}

/// A command with more warp players for the current snapshot, and room to retire as many.
fn warps_cmd(warps: Vec<PreparedWarp>) -> Cmd {
    let room = WarpRoom::retiring(warps.len());
    Cmd::Warps(warps, room)
}

fn new_engine(sr: f32) -> MixEngine {
    let mut m = MixEngine::new(sr, BLOCK);
    m.set_external_instances(true);
    m.metronome = true;
    m
}

impl AudioState {
    fn new(rx: Receiver<Cmd>, shared: Arc<Shared>, session: Arc<Session>, device_rate: f64, garbage: Bin) -> Self {
        let sr = session.sample_rate.as_f64() as f32;
        let nch = main_channels(&session);
        AudioState {
            garbage,
            old_sessions: None,
            room_backlog: None,
            fold: fold_gains(&session),
            rx,
            shared,
            mix: new_engine(sr),
            session,
            pos: 0,
            playing: false,
            end: None,
            looped: None,
            pending: vec![vec![0.0; BLOCK]; nch],
            pending_len: 0,
            read: 0.0,
            device_rate,
            input: None,
            recording: false,
            live_seen: [ColumnPlay::default(); MAX_LIVE_COLUMNS],
            midi: midi_in::MidiIn::new(),
            sub_backlog: None,
            capture: None,
            capture_playing: false,
            capture_dropped: 0,
            capture_pass: 0,
            capture_backlog: None,
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                Cmd::Recording(r) => self.recording = r,
                Cmd::Session(s, instances, warps, room) => {
                    changed = true;
                    if (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5 {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        let _ = self.garbage.plugins.send(old.drain_plugins());
                        let _ = self.garbage.warps.send(old.drain_warps());
                    }
                    // Place the previous batch first, so the engine's inbox is empty to adopt into
                    // (see `MixEngine::adopt_warps`).
                    self.place_warps();
                    self.mix.adopt(instances);
                    self.adopt_warps(warps);
                    // The main format changed (a document edit, not steady state): resize.
                    let nch = main_channels(&s);
                    if self.pending.len() != nch {
                        self.pending = vec![vec![0.0; BLOCK]; nch];
                        self.pending_len = 0;
                        self.read = 0.0;
                    }
                    self.fold = fold_gains(&s);
                    if self.playing {
                        // Slots cleared or refilled fade out from the old snapshot.
                        self.mix.live_snapshot(&self.session, &s, self.pos);
                    }
                    let old = std::mem::replace(&mut self.session, s);
                    if let Some(tx) = &self.old_sessions {
                        let _ = tx.try_send(old);
                    }
                    // The snapshot's warp tables and retired list, sized on the UI thread, so
                    // syncing to it, placing and retiring its players never grows them here.
                    self.adopt_room(room);
                }
                Cmd::Warps(warps, room) => {
                    // Placed right away: while stopped, `render` (which also places) doesn't run.
                    self.place_warps();
                    self.adopt_room(room);
                    self.adopt_warps(warps);
                    self.place_warps();
                }
                Cmd::OldSessions(tx) => self.old_sessions = Some(tx),
                Cmd::Live(cmd) => self.mix.live(cmd),
                Cmd::Play { from, end, looped } => {
                    // Live slots restart across the transport's loop wraps like at their own.
                    self.mix.set_live_loop(looped.filter(|r| r.len() > 64));
                    if self.playing {
                        self.mix.live_locate(self.pos, from, false);
                        self.capture_jump(from);
                    } else {
                        // Launches sent just before the play command (for `from` on) stay queued.
                        self.mix.live_start(from);
                    }
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
                Cmd::MidiIn(sub) => {
                    let old = self.midi.replace(sub);
                    if let Some(old) = old {
                        self.return_sub(old);
                    }
                }
                Cmd::MidiTarget(t) => self.mix.set_live_target(t),
                Cmd::MidiClosed(port) => self.mix.live_release(port.0),
                Cmd::MidiPanic => {
                    self.midi.clear_pending();
                    self.mix.live_panic();
                }
                Cmd::MidiPlay(msg) => {
                    if self.mix.live_target().is_some() {
                        self.mix.live_input(LiveEvent { source: midi_in::KEYBOARD_SOURCE, offset: 0, msg: midi_in::raw(msg) });
                    }
                }
                Cmd::MidiCapture(q) => {
                    if let Some(old) = std::mem::replace(&mut self.capture, q) {
                        self.return_capture(old);
                    }
                }
                Cmd::MidiCaptureWhilePlaying(on) => self.capture_playing = on,
                Cmd::Stop => {
                    // The transport stopped: live keys are released too.
                    self.mix.live_release_all();
                    self.mix.live_clear();
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.mix.reset();
                }
                Cmd::Locate(at) => {
                    self.mix.live_locate(self.pos, at, false);
                    if self.playing {
                        self.capture_jump(at);
                    }
                    self.pos = at;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
            }
        }
        if changed {
            // Place adopted instances now (not only once playback starts), so editors see them;
            // `sync` creates no third-party plugins here.
            self.mix.ensure_synced(&self.session);
            self.mix.place_pending_warps(&self.session);
            self.mix.retire_stale_warps(&self.session);
        }
        self.send_garbage();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    /// Places warp players waiting in the engine into the strips of the current snapshot.
    fn place_warps(&mut self) {
        if self.mix.has_pending_warps() {
            self.mix.ensure_synced(&self.session);
            self.mix.place_pending_warps(&self.session);
        }
    }

    /// Hands the engine a warp room; the storage it replaces goes back to be freed off this thread.
    fn adopt_room(&mut self, room: WarpRoom) {
        let spent = self.mix.adopt_room(&self.session, room);
        self.return_room(spent);
    }

    /// Sends a spent room back to be freed on the UI thread, without allocating: a bare one is
    /// dropped here (it frees nothing), and one the full channel refuses waits for the next
    /// callback. Only when one is already waiting is a room freed here, as a last resort.
    fn return_room(&mut self, room: WarpRoom) {
        if room.is_bare() {
            return;
        }
        self.flush_rooms();
        match self.garbage.rooms.try_send(room) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(room)) => {
                if self.room_backlog.is_none() {
                    self.room_backlog = Some(room);
                }
            }
        }
    }

    /// Sends a replaced MIDI queue back to be dropped on the UI thread, without allocating; one
    /// the full channel refuses waits for the next callback (only when one already waits is a
    /// queue dropped here, as a last resort).
    fn return_sub(&mut self, sub: Subscriber) {
        self.flush_subs();
        match self.garbage.subs.try_send(sub) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(sub)) => {
                if self.sub_backlog.is_none() {
                    self.sub_backlog = Some(sub);
                }
            }
        }
    }

    /// [`AudioState::return_sub`] for a replaced capture queue.
    fn return_capture(&mut self, q: rtrb::Producer<Captured>) {
        self.flush_captures();
        match self.garbage.captures.try_send(q) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(q)) => {
                if self.capture_backlog.is_none() {
                    self.capture_backlog = Some(q);
                }
            }
        }
    }

    fn flush_captures(&mut self) {
        if let Some(q) = self.capture_backlog.take()
            && let Err(TrySendError::Full(q)) = self.garbage.captures.try_send(q)
        {
            self.capture_backlog = Some(q);
        }
    }

    /// Copies the live events the target plays in the next block (`n` samples from `self.pos`)
    /// into the capture queue, at the timeline sample each plays on (see [`capture`]). No
    /// allocation: a full queue drops the event and counts it.
    fn capture_live(&mut self, n: usize) {
        if !self.capturing() {
            return;
        }
        let Some(q) = self.capture.as_mut() else { return };
        let last = n.saturating_sub(1);
        let pass = self.capture_pass;
        for ev in self.mix.live_events() {
            let Some(msg) = midi_in::from_raw(ev.msg) else { continue };
            let at = self.pos.saturating_add(i64::try_from(ev.offset.min(last)).unwrap_or(0));
            if q.push(Captured { at, pass, event: CaptureEvent::Midi(msg) }).is_err() {
                self.capture_dropped = self.capture_dropped.saturating_add(1);
            }
        }
        self.shared.midi.capture_dropped.store(self.capture_dropped, Ordering::Relaxed);
    }

    /// Whether what the target plays is being captured now (see [`capture`]).
    fn capturing(&self) -> bool {
        let Some(target) = self.mix.live_target() else { return false };
        let armed = self.recording && self.session.track(target).is_some_and(|t| t.mixer.record_arm);
        self.capture.is_some() && self.playing && (armed || self.capture_playing)
    }

    /// The transport jumps from `self.pos` to `to` while playing (a loop wrap, a locate): the
    /// capture's pass ends here and the next starts at `to`, marked in the queue.
    fn capture_jump(&mut self, to: Samples) {
        if self.capturing()
            && let Some(q) = self.capture.as_mut()
            && q.push(Captured { at: self.pos, pass: self.capture_pass, event: CaptureEvent::Jump { to } }).is_err()
        {
            self.capture_dropped = self.capture_dropped.saturating_add(1);
            self.shared.midi.capture_dropped.store(self.capture_dropped, Ordering::Relaxed);
        }
        self.capture_pass = self.capture_pass.wrapping_add(1);
    }

    fn flush_subs(&mut self) {
        if let Some(sub) = self.sub_backlog.take()
            && let Err(TrySendError::Full(sub)) = self.garbage.subs.try_send(sub)
        {
            self.sub_backlog = Some(sub);
        }
    }

    /// Sends the room waiting from an earlier callback, if the channel has room now.
    fn flush_rooms(&mut self) {
        if let Some(room) = self.room_backlog.take()
            && let Err(TrySendError::Full(room)) = self.garbage.rooms.try_send(room)
        {
            self.room_backlog = Some(room);
        }
    }

    /// Hands warp players to the engine; the emptied batch goes back to be freed off this thread.
    fn adopt_warps(&mut self, warps: Vec<PreparedWarp>) {
        let spent = self.mix.adopt_warps(warps);
        if spent.capacity() > 0 {
            let _ = self.garbage.batches.send(spent);
        }
    }

    fn send_garbage(&mut self) {
        self.flush_rooms();
        self.flush_subs();
        self.flush_captures();
        let g = self.mix.take_retired();
        if !g.is_empty() {
            let _ = self.garbage.plugins.send(g);
        }
        let w = self.mix.take_retired_warps();
        if !w.is_empty() {
            let _ = self.garbage.warps.send(w);
        }
    }

    /// Render the next block at the session rate into `pending`.
    fn render_next(&mut self, quantum: usize) {
        let mut n = quantum.clamp(1, BLOCK);
        if let Some(l) = self.looped
            && self.pos >= l.end
        {
            self.mix.live_locate(self.pos, l.start, true);
            self.capture_jump(l.start);
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                // The end of play stops the transport, as `Cmd::Stop` does.
                self.playing = false;
                self.end = None;
                self.mix.live_release_all();
                self.mix.live_clear();
                self.mix.reset();
                self.shared.playing.store(false, Ordering::Relaxed);
                n = 0;
            } else {
                n = n.min(usize::try_from(e - self.pos).unwrap_or(BLOCK));
            }
        }
        if n == 0 {
            for c in &mut self.pending {
                c.iter_mut().for_each(|x| *x = 0.0);
            }
            self.pending_len = BLOCK;
            return;
        }
        if let Some(ring) = &self.input {
            let ch = ring.channels.load(Ordering::Relaxed).max(1);
            if self.mix.input.len() != ch {
                self.mix.input = vec![vec![0.0; BLOCK]; ch];
            }
            ring.pop_into(&mut self.mix.input, n, self.session.sample_rate.hz());
        }
        self.mix.monitor_only = !self.playing;
        self.mix.recording = self.recording;
        if self.mix.live_target().is_some() {
            self.midi.feed(&mut self.mix, n, &self.shared.midi.controls);
        } else {
            self.midi.discard();
        }
        self.capture_live(n);
        self.mix.render(&self.session, self.pos, n, &mut self.pending);
        self.shared.midi.dropped.store(self.midi.dropped.saturating_add(self.mix.live_dropped()), Ordering::Relaxed);
        self.send_garbage();
        self.pending_len = n;
        if let Ok(mut m) = self.shared.meters.try_lock() {
            m.main.copy_from(&self.mix.main_meter);
            for (k, v) in &self.mix.meters {
                match m.tracks.get_mut(k) {
                    Some(d) => d.copy_from(v),
                    None => {
                        m.tracks.insert(*k, v.clone());
                    }
                }
            }
        }
        if self.playing {
            self.pos += n as i64;
            self.shared.position.store(self.pos, Ordering::Relaxed);
        }
    }

    /// Fill an interleaved device buffer.
    fn fill(&mut self, out: &mut [f32], channels: usize) {
        let now = self.midi.now_ns();
        self.fill_at(out, channels, now);
    }

    /// [`AudioState::fill`] for a callback starting at `now_ns` on the MIDI clock.
    fn fill_at(&mut self, out: &mut [f32], channels: usize, now_ns: u64) {
        // The longest callback, in session samples, for the UI's launch margin.
        let speed = f64::from(self.shared.speed.load(Ordering::Relaxed)) / 1000.0;
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let frames = out.len() / channels.max(1);
        let span = (frames as f64 * step - 1e-6).ceil() as i64;
        self.shared.live.max_callback.fetch_max(span, Ordering::Relaxed);
        // Live MIDI: this callback's time, length and rate (session samples a second).
        let span_ns = (frames as f64 / self.device_rate.max(1.0) * 1e9) as u64;
        self.midi.begin(now_ns, span_ns, self.device_rate * step);
        self.shared.midi.latency_ns.store(self.midi.latency_ns, Ordering::Relaxed);
        self.fill_block(out, channels);
        // What the Live columns play, for the UI: published only when it changed.
        if self.mix.live_state() != self.live_seen.as_slice() {
            for (seen, c) in self.live_seen.iter_mut().zip(self.mix.live_state()) {
                *seen = *c;
            }
            self.shared.live.publish(&self.live_seen);
        }
    }

    fn fill_block(&mut self, out: &mut [f32], channels: usize) {
        self.drain();
        let channels = channels.max(1);
        // Live MIDI keeps the engine rendering while stopped only while there is something to
        // play or ring out, not merely because a target exists.
        let live = self.mix.live_awake() || (self.mix.live_target().is_some() && self.midi.waiting());
        let monitoring = (self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm)) || live;
        if !self.playing && !monitoring {
            // With no target nothing plays live MIDI: what arrived is dropped rather than kept for
            // later. With one, an event that arrived since the check above is the first note
            // after idle: it waits for the next callback.
            if self.mix.live_target().is_none() {
                self.midi.discard();
                self.mix.live_discard();
            }
            // Nothing rendered ahead survives the rest: waking renders afresh, so old audio from
            // before a stop never plays and the first note lands at its own offset.
            self.pending_len = 0;
            self.read = 0.0;
            out.iter_mut().for_each(|x| *x = 0.0);
            if let Ok(mut m) = self.shared.meters.try_lock() {
                // Zeroed in place: the map keeps its entries, so rendering again inserts nothing.
                zero_meter(&mut m.main);
                m.tracks.values_mut().for_each(zero_meter);
            }
            return;
        }
        // Audio rendered ahead in the last callback plays first in this one.
        self.midi.ahead(self.pending_len as f64 - self.read);
        let speed = f64::from(self.shared.speed.load(Ordering::Relaxed)) / 1000.0;
        let step = self.session.sample_rate.as_f64() / self.device_rate.max(1.0) * speed.clamp(0.1, 4.0);
        let callback_frames = out.len() / channels;
        let quantum = ((callback_frames as f64 * step).ceil() as usize).clamp(1, BLOCK);
        for frame in out.chunks_mut(channels) {
            while self.read >= self.pending_len as f64 {
                self.read -= self.pending_len as f64;
                if !self.playing && !monitoring {
                    break;
                }
                self.render_next(quantum);
                if self.pending_len == 0 {
                    break;
                }
            }
            let i0 = self.read.floor() as usize;
            let fr = (self.read - i0 as f64) as f32;
            let sounding = self.playing || monitoring;
            let nch = self.pending.len();
            let sample = |c: usize| {
                let ch = self.pending.get(c);
                let a = ch.and_then(|v| v.get(i0)).copied().unwrap_or(0.0);
                let b = ch.and_then(|v| v.get(i0 + 1)).copied().unwrap_or(a);
                a + (b - a) * fr
            };
            if nch > 2 && channels < nch {
                // Fewer device channels than the main mix: ITU fold-down to stereo.
                let (mut l, mut r) = (0.0f32, 0.0f32);
                if sounding {
                    for (c, (gl, gr)) in self.fold.iter().enumerate().take(nch) {
                        let v = sample(c);
                        l += v * gl;
                        r += v * gr;
                    }
                }
                for (c, o) in frame.iter_mut().enumerate() {
                    *o = match c {
                        0 => l.clamp(-1.0, 1.0),
                        1 => r.clamp(-1.0, 1.0),
                        _ => 0.0,
                    };
                }
            } else {
                for (c, o) in frame.iter_mut().enumerate() {
                    // Stereo mixes on a mono device play the left channel (as before).
                    *o = if c < nch.max(1) && sounding { sample(c).clamp(-1.0, 1.0) } else { 0.0 };
                }
            }
            self.read += step;
        }
    }
}

/// A meter at rest, without freeing its storage.
fn zero_meter(m: &mut StripMeter) {
    m.peak = [0.0; 2];
    m.peaks.iter_mut().for_each(|x| *x = 0.0);
    m.gain_reduction_db = 0.0;
}

/// UI-thread bookkeeping of the third-party instances the audio thread holds.
struct Side {
    /// What was last created for each slot: (plugin id, channels, created ok).
    shadow: HashMap<(TrackId, usize), (String, usize, bool)>,
    /// Session sample rate the instances were prepared for.
    rate: f32,
    /// Editor handles of the live instances.
    editors: HashMap<(TrackId, usize), Box<dyn PluginEditor>>,
    /// State readers of the live instances.
    states: BTreeMap<(TrackId, usize), Box<dyn PluginStateReader>>,
    /// Instances that refused their stored state: saves keep that state until the plugin's editor
    /// is opened.
    refused: HashSet<(TrackId, usize)>,
    garbage: BinRx,
    old_sessions: Option<Receiver<Arc<Session>>>,
    /// Warp players sent for beat-locked clips and Live slots: (track, clip or slot, stem or
    /// source) -> (the audio it plays, channels, sample rate).
    prepared_warps: HashMap<WarpKey, (SourceId, usize, u32)>,
    /// Column tracks whose Live-slot storage was sent (the engine keeps it while the track lives).
    live_stores: HashSet<TrackId>,
    /// The snapshot last sent (an extra reference, released here on the UI thread), while some
    /// of its warp players are still to be built.
    warps_for: Option<Arc<Session>>,
    /// Players the last `prepare_warps` forgot (their clip or track is gone): the audio thread
    /// may retire that many, so the snapshot's room is sized for them.
    stale_warps: usize,
    /// Players whose build failed, warned about once; they are retried.
    failed_warps: HashSet<WarpKey>,
}

/// What a warp player plays for: a clip by id, or a player of a Live column's pool (on its
/// track) by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum WarpFor {
    Clip(u64),
    Pool(usize),
}

/// A warp player's key on the UI side: its track, what it plays for and which audio.
type WarpKey = (TrackId, WarpFor, PlayerStem);

impl Side {
    fn new(rate: f32, garbage: BinRx) -> Side {
        Side {
            shadow: HashMap::new(),
            rate,
            editors: HashMap::new(),
            states: BTreeMap::new(),
            refused: HashSet::new(),
            garbage,
            old_sessions: None,
            prepared_warps: HashMap::new(),
            live_stores: HashSet::new(),
            warps_for: None,
            stale_warps: 0,
            failed_warps: HashSet::new(),
        }
    }

    /// Drops retired instances (closing their editors first, inside the plugin's drop) and
    /// replaced session snapshots.
    fn collect_garbage(&mut self) {
        while let Ok(g) = self.garbage.plugins.try_recv() {
            for p in g {
                log::debug!("plugin instance released: {}", p.info().name);
                drop(p);
            }
        }
        self.garbage.warps.try_iter().for_each(drop);
        self.garbage.batches.try_iter().for_each(drop);
        self.garbage.rooms.try_iter().for_each(drop);
        self.garbage.subs.try_iter().for_each(drop);
        self.garbage.captures.try_iter().for_each(drop);
        if let Some(rx) = &self.old_sessions {
            rx.try_iter().for_each(drop);
        }
    }

    /// A batch of the players still missing for the clips nearest `at` (see `Player::play`).
    fn prepare_around(&mut self, at: Samples) -> Option<Cmd> {
        let s = self.warps_for.clone()?;
        let warps = self.prepare_warps(&s, at, WARP_BATCH);
        (!warps.is_empty()).then(|| warps_cmd(warps))
    }

    fn return_old_sessions(&mut self, tx: &Sender<Cmd>) {
        let (otx, orx) = sync_channel(OLD_SESSIONS);
        self.old_sessions = Some(orx);
        let _ = tx.send(Cmd::OldSessions(otx));
    }

    fn forget(&mut self, key: (TrackId, usize)) {
        self.shadow.remove(&key);
        self.states.remove(&key);
        self.refused.remove(&key);
        if let Some(mut e) = self.editors.remove(&key) {
            e.close();
        }
    }
}

/// The transport and audio device.
pub struct Player {
    shared: Arc<Shared>,
    fault: Arc<Fault>,
    /// The block count last seen while playing, and since when.
    #[cfg(not(target_arch = "wasm32"))]
    watch: Mutex<Option<(u64, std::time::Instant)>>,
    tx: Sender<Cmd>,
    side: Mutex<Side>,
    #[cfg(not(target_arch = "wasm32"))]
    _stream: Option<cpal::Stream>,
    #[cfg(target_arch = "wasm32")]
    _stream: Option<cpal::Stream>,
    pub device_name: String,
    pub device_rate: u32,
    /// Output channels the device was opened with (0 for the silent clock).
    pub device_channels: u16,
    /// The device buffer in frames when one was chosen (`None`: the device default).
    pub device_buffer: Option<u32>,
    /// True when no audio device could be opened (silent clock).
    pub silent: bool,
}

impl Player {
    /// Open the default output device. Never fails: without a device it runs a silent clock.
    pub fn new(session: Arc<Session>) -> Player {
        Player::with_config(session, &AudioConfig::default())
    }

    /// Open the output device `cfg` chooses (system, device, rate, buffer size), falling back to the
    /// defaults for whatever is unavailable. Never fails: without a device it runs a silent clock.
    pub fn with_config(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        let p = Player::open(Arc::clone(&session), cfg);
        p.side().return_old_sessions(&p.tx);
        // Hand the initial session's third-party plugins over.
        p.update_session(session);
        p
    }

    /// What stopped the audio output, once: a bug in the engine, or the device went away or
    /// stopped asking for audio while playing.
    pub fn trouble(&self) -> Option<OutputTrouble> {
        #[cfg(not(target_arch = "wasm32"))]
        let stalled = {
            let playing = self.shared.playing.load(Ordering::Relaxed) && !self.silent;
            stalled(&mut self.watch.lock().unwrap_or_else(PoisonError::into_inner), self.fault.beats.load(Ordering::Relaxed), playing)
        };
        #[cfg(target_arch = "wasm32")]
        let stalled = false;
        self.fault.trouble(stalled)
    }

    fn side(&self) -> std::sync::MutexGuard<'_, Side> {
        self.side.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn open(session: Arc<Session>, cfg: &AudioConfig) -> Player {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let want = session.sample_rate.hz();
        let want_ch = u16::try_from(main_channels(&session)).unwrap_or(2);
        let side_rate = session.sample_rate.as_f64() as f32;
        let new_side = |garbage: BinRx| Mutex::new(Side::new(side_rate, garbage));
        match open_device(want, want_ch, cfg) {
            Ok((device, config, format, name)) => {
                // The chosen buffer size first; if the device refuses it, its default.
                let mut attempts = vec![config.clone()];
                if config.buffer_size != cpal::BufferSize::Default {
                    attempts.push(cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config });
                }
                for config in attempts {
                    let rate = config.sample_rate.0;
                    let channels = usize::from(config.channels);
                    let (tx, rx) = channel();
                    let (gtx, grx) = bin();
                    let state = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&session), f64::from(rate), gtx);
                    let fault = Arc::new(Fault::default());
                    use cpal::traits::StreamTrait;
                    let built = match format {
                        cpal::SampleFormat::I32 => build_output::<i32>(&device, &config, state, channels, &fault, &shared),
                        cpal::SampleFormat::I16 => build_output::<i16>(&device, &config, state, channels, &fault, &shared),
                        _ => build_output::<f32>(&device, &config, state, channels, &fault, &shared),
                    };
                    match built {
                        Ok(stream) => {
                            if let Err(e) = stream.play() {
                                log::warn!("audio stream failed to start: {e}");
                            }
                            let buffer = match config.buffer_size {
                                cpal::BufferSize::Fixed(n) => Some(n),
                                cpal::BufferSize::Default => None,
                            };
                            log::info!(
                                "audio device: {name} @ {rate} Hz, {channels} ch, {format}, buffer {}",
                                buffer.map_or("default".to_string(), |n| n.to_string())
                            );
                            return Player {
                                shared,
                                fault,
                                #[cfg(not(target_arch = "wasm32"))]
                                watch: Mutex::new(None),
                                tx,
                                side: new_side(grx),
                                _stream: Some(stream),
                                device_name: name,
                                device_rate: rate,
                                device_channels: config.channels,
                                device_buffer: buffer,
                                silent: false,
                            };
                        }
                        Err(e) => log::warn!("cannot open audio stream ({:?}): {e}", config.buffer_size),
                    }
                }
                log::warn!("no usable audio stream; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = bin();
                Player::silent_clock(shared, tx, rx, want, new_side(grx), gtx)
            }
            Err(e) => {
                log::warn!("no audio output: {e}; using a silent clock");
                let (tx, rx) = channel();
                let (gtx, grx) = bin();
                Player::silent_clock_with(shared, tx, rx, Arc::clone(&session), new_side(grx), gtx)
            }
        }
    }

    /// A player on the silent clock, without opening any audio device (headless runs, tests).
    pub fn silent(session: Arc<Session>) -> Player {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let side_rate = session.sample_rate.as_f64() as f32;
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let p = Player::silent_clock_with(shared, tx, rx, Arc::clone(&session), Mutex::new(Side::new(side_rate, grx)), gtx);
        p.side().return_old_sessions(&p.tx);
        p.update_session(session);
        p
    }

    fn silent_clock(shared: Arc<Shared>, tx: Sender<Cmd>, rx: Receiver<Cmd>, rate: u32, side: Mutex<Side>, gtx: Bin) -> Player {
        let s = Arc::new(Session::new("Untitled", soundcraft_time::SampleRate::new(rate).unwrap_or_default()));
        Player::silent_clock_with(shared, tx, rx, s, side, gtx)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn silent_clock_with(shared: Arc<Shared>, tx: Sender<Cmd>, rx: Receiver<Cmd>, session: Arc<Session>, side: Mutex<Side>, gtx: Bin) -> Player {
        let rate = session.sample_rate.hz();
        let mut state = AudioState::new(rx, Arc::clone(&shared), session, f64::from(rate), gtx);
        let fault = Arc::new(Fault::default());
        let failed = Arc::clone(&fault);
        let spawn = std::thread::Builder::new().name("soundcraft-clock".into()).spawn(move || {
            let mut buf = vec![0.0f32; 4096];
            let mut last = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let now = std::time::Instant::now();
                let frames = ((now - last).as_secs_f64() * state.device_rate) as usize;
                last = now;
                let n = (frames * 2).min(buf.len());
                if let Some(b) = buf.get_mut(..n) {
                    let shared = Arc::clone(&state.shared);
                    guarded_block(&failed, &shared, b, |b| state.fill(b, 2));
                }
                if Arc::strong_count(&state.shared) <= 1 {
                    break;
                }
            }
        });
        if let Err(e) = spawn {
            log::warn!("clock thread failed: {e}");
        }
        Player {
            shared,
            fault,
            watch: Mutex::new(None),
            tx,
            side,
            _stream: None,
            device_name: "No audio device (silent)".into(),
            device_rate: rate,
            device_channels: 0,
            device_buffer: None,
            silent: true,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn silent_clock_with(shared: Arc<Shared>, tx: Sender<Cmd>, _rx: Receiver<Cmd>, session: Arc<Session>, side: Mutex<Side>, _gtx: Bin) -> Player {
        Player {
            shared,
            fault: Arc::default(),
            tx,
            side,
            _stream: None,
            device_name: "No audio device".into(),
            device_rate: session.sample_rate.hz(),
            device_channels: 0,
            device_buffer: None,
            silent: true,
        }
    }

    /// Sends a new session snapshot to the audio thread, with freshly created (state-restored,
    /// prepared) instances of any third-party plugin it newly needs. Call on the UI thread.
    pub fn update_session(&self, s: Arc<Session>) {
        let mut side = self.side();
        let instances = side.prepare(&s);
        side.warps_for = Some(Arc::clone(&s));
        let warps = side.prepare_warps(&s, self.position(), WARP_BATCH);
        let stale = side.stale_warps;
        // Live-slot storage goes once per column track; a strip keeps it until its track goes.
        side.live_stores.retain(|t| s.track(*t).is_some());
        let sent = side.live_stores.clone();
        side.live_stores.extend((0..s.live.columns.len()).filter_map(|c| s.live.column_track(c, &s.tracks)));
        // Sent under the lock, so a batch from `idle` can't overtake the snapshot it belongs to.
        let _ = self.tx.send(session_cmd_with(s, instances, warps, stale, |t| !sent.contains(&t)));
        drop(side);
    }

    /// Reads the state of every live third-party plugin, for saving the session.
    pub fn capture_states(&self) -> PluginStates {
        self.side().capture_states()
    }

    /// Per-UI-frame housekeeping: drops retired instances and services plugin editors. Returns
    /// the parameter edits made in open editors since the last call.
    pub fn idle(&self) -> Vec<EditorEdit> {
        let mut side = self.side();
        side.collect_garbage();
        if let Some(s) = side.warps_for.clone() {
            let warps = side.prepare_warps(&s, self.position(), WARP_IDLE_BATCH);
            if !warps.is_empty() {
                let _ = self.tx.send(warps_cmd(warps));
            }
        }
        let mut out = Vec::new();
        for (&(t, slot), e) in side.editors.iter_mut() {
            for (id, v) in e.idle() {
                out.push((t, slot, id, v));
            }
        }
        out
    }

    /// True when the plugin in this slot has its own editor.
    pub fn has_editor(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.contains_key(&(track, slot))
    }

    /// Opens the plugin's own editor window. Main thread only.
    pub fn open_editor(&self, track: TrackId, slot: usize) -> Result<(), String> {
        self.side().open_editor((track, slot))
    }

    pub fn close_editor(&self, track: TrackId, slot: usize) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.close();
        }
    }

    /// Closes every plugin editor window (main thread). Call before replacing the player: its
    /// instances may be dropped on the audio thread, where editor windows cannot be closed.
    pub fn close_editors(&self) {
        for e in self.side().editors.values_mut() {
            e.close();
        }
    }

    pub fn editor_open(&self, track: TrackId, slot: usize) -> bool {
        self.side().editors.get(&(track, slot)).is_some_and(|e| e.is_open())
    }

    pub fn any_editor_open(&self) -> bool {
        self.side().editors.values().any(|e| e.is_open())
    }

    /// Shows a parameter change made elsewhere in the plugin's open editor.
    pub fn editor_set_param(&self, track: TrackId, slot: usize, id: &str, value: f32) {
        if let Some(e) = self.side().editors.get_mut(&(track, slot)) {
            e.set_param(id, value);
        }
    }

    /// Start playback at `from`, stopping at `end`, or looping `looped`.
    pub fn play(&self, from: Samples, end: Option<Samples>, looped: Option<Range>) {
        self.prepare_around(from);
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play { from, end, looped });
    }

    /// Launches or stops Live slots at the sample `cmd` names (the audio thread splits its block
    /// there). Players for the session's filled slots are prepared with each snapshot; a slot
    /// whose player has not arrived plays silence.
    pub fn live(&self, cmd: LiveCmd) {
        let _ = self.tx.send(Cmd::Live(cmd));
    }

    /// What each Live column plays and has queued, as the audio thread last published it (all
    /// idle until it has run).
    pub fn live_state(&self) -> [LiveColumnState; MAX_LIVE_COLUMNS] {
        self.shared.live.read()
    }

    /// The longest audio callback seen so far, in session samples (device frames × session rate
    /// / device rate × speed): a launch must be sent at least this far (plus a mix block) ahead
    /// of its sample to be heard from its start.
    pub fn max_callback_samples(&self) -> Samples {
        self.shared.live.max_callback.load(Ordering::Relaxed)
    }

    /// Hands the audio thread the MIDI input queue to play live (events stamped on `clock`),
    /// replacing any earlier one, which comes back to be dropped by [`Player::idle`]. `None`
    /// takes it away.
    pub fn set_midi_input(&self, input: Option<(Subscriber, MidiClock)>) {
        self.shared.midi.has_input.store(input.is_some(), Ordering::Relaxed);
        let _ = self.tx.send(Cmd::MidiIn(input));
    }

    /// Whether this player has a MIDI input queue (a new player needs one).
    pub fn has_midi_input(&self) -> bool {
        self.shared.midi.has_input.load(Ordering::Relaxed)
    }

    /// The track live MIDI input plays (`None`: none). A change releases the live notes held on
    /// the old target. Unchanged targets are not resent.
    pub fn set_midi_target(&self, target: Option<TrackId>) {
        let v = target.map_or(u64::MAX, |t| t.0);
        if self.shared.midi.target.swap(v, Ordering::Relaxed) != v {
            let _ = self.tx.send(Cmd::MidiTarget(target));
        }
    }

    /// The live MIDI target last set.
    pub fn midi_target(&self) -> Option<TrackId> {
        let v = self.shared.midi.target.load(Ordering::Relaxed);
        (v != u64::MAX).then_some(TrackId(v))
    }

    /// A MIDI port's device went away (`PortChange::Closed`): its live notes are released.
    pub fn midi_port_closed(&self, port: PortId) {
        let _ = self.tx.send(Cmd::MidiClosed(port));
    }

    /// MIDI panic: every live note off and the controllers reset.
    pub fn midi_panic(&self) {
        let _ = self.tx.send(Cmd::MidiPanic);
    }

    /// The notes and controllers (per port) mapped to controls: they no longer play the live
    /// target (note-offs still pass). Replaces the set given before.
    /// False when the ports went beyond [`CONTROL_PORTS`]: those ports' mapped messages still play.
    pub fn set_midi_controls(&self, controls: &[(PortId, midi_in::Control)]) -> bool {
        self.shared.midi.controls.set(controls)
    }

    /// Plays one message from the on-screen keyboard on the target at the next block.
    pub fn play_midi(&self, msg: MidiMessage) {
        let _ = self.tx.send(Cmd::MidiPlay(msg));
    }

    /// The live MIDI latency `L`: the longest callback plus [`MIDI_SLACK_NS`] (see [`midi_in`]).
    pub fn midi_latency_ns(&self) -> u64 {
        self.shared.midi.latency_ns.load(Ordering::Relaxed)
    }

    /// Live MIDI events dropped so far (the pending buffer or the engine's inbox was full).
    pub fn midi_dropped(&self) -> u64 {
        self.shared.midi.dropped.load(Ordering::Relaxed)
    }

    /// A new queue of `capacity` events capturing the live MIDI the target plays (see
    /// [`capture`]); the audio thread's previous queue goes. Made here, off the audio thread.
    pub fn midi_capture(&self, capacity: usize) -> MidiCapture {
        let (tx, rx) = capture::queue(capacity);
        self.shared.midi.has_capture.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::MidiCapture(Some(tx)));
        rx
    }

    /// Whether [`Player::midi_capture`] gave this player a queue.
    pub fn has_midi_capture(&self) -> bool {
        self.shared.midi.has_capture.load(Ordering::Relaxed)
    }

    /// Capture the target's live MIDI whenever the transport plays (a Session view slot
    /// recording), not only while recording onto a record-armed target.
    pub fn capture_midi_while_playing(&self, on: bool) {
        let _ = self.tx.send(Cmd::MidiCaptureWhilePlaying(on));
    }

    /// Captured events dropped so far because the capture queue was full.
    pub fn midi_capture_dropped(&self) -> u64 {
        self.shared.midi.capture_dropped.load(Ordering::Relaxed)
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        let _ = self.tx.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    /// Sends the warp players still missing for the clips nearest `at` ahead of a transport
    /// command that jumps there, so split clips play their stems from the first block.
    fn prepare_around(&self, at: Samples) {
        let mut side = self.side();
        if let Some(cmd) = side.prepare_around(at) {
            let _ = self.tx.send(cmd);
        }
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn locate(&self, at: Samples) {
        self.prepare_around(at);
        self.shared.position.store(at, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Locate(at));
    }

    pub fn set_speed(&self, speed: f32) {
        let v = if speed.is_finite() { (speed.clamp(0.1, 4.0) * 1000.0) as u32 } else { 1000 };
        self.shared.speed.store(v, Ordering::Relaxed);
    }

    /// Playback rate last requested, 1.0 at full speed.
    pub fn speed(&self) -> f32 {
        self.shared.speed.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn position(&self) -> Samples {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn meters(&self) -> MeterSnapshot {
        self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Copy meters into reusable UI storage. Once track/channel counts are stable this performs
    /// no allocation, unlike cloning a new snapshot every video frame.
    pub fn copy_meters(&self, out: &mut MeterSnapshot) {
        let source = self.shared.meters.lock().unwrap_or_else(PoisonError::into_inner);
        copy_meter_snapshot(&source, out);
    }
}

fn copy_meter_snapshot(source: &MeterSnapshot, out: &mut MeterSnapshot) {
    out.tracks.retain(|id, _| source.tracks.contains_key(id));
    for (id, meter) in &source.tracks {
        out.tracks.entry(*id).or_default().copy_from(meter);
    }
    out.main.copy_from(&source.main);
}

impl Side {
    fn capture_states(&mut self) -> PluginStates {
        self.states.iter_mut().filter(|(k, _)| !self.refused.contains(*k)).filter_map(|(&(t, slot), r)| Some((t, slot, r.save_state()?))).collect()
    }

    fn open_editor(&mut self, key: (TrackId, usize)) -> Result<(), String> {
        let e = self.editors.get_mut(&key).ok_or_else(|| "this plugin has no editor (or it is not loaded)".to_string())?;
        e.open()?;
        self.refused.remove(&key);
        Ok(())
    }

    /// Creates the third-party instances `s` needs that the audio thread does not have yet.
    fn prepare(&mut self, s: &Session) -> Vec<PreparedInstance> {
        self.collect_garbage();
        let sr = s.sample_rate.as_f64() as f32;
        if (sr - self.rate).abs() > 0.5 {
            // The audio thread rebuilds its engine: everything is created anew.
            self.live_stores.clear();
            let keys: Vec<(TrackId, usize)> = self.shadow.keys().copied().collect();
            for k in keys {
                self.forget(k);
            }
            self.rate = sr;
        }
        let specs = soundcraft_mix::instance_specs(s);
        let stale: Vec<(TrackId, usize)> = self.shadow.keys().filter(|k| !specs.iter().any(|x| (x.track, x.slot) == **k)).copied().collect();
        for k in stale {
            self.forget(k);
        }
        let mut out = Vec::new();
        for spec in specs {
            let key = (spec.track, spec.slot);
            if self.shadow.get(&key).is_some_and(|(id, ch, _)| *id == spec.id && *ch == spec.ch) {
                continue;
            }
            self.forget(key);
            let made = soundcraft_mix::create_instance(&spec.id, spec.state.as_deref(), sr, BLOCK, spec.ch);
            log::debug!("plugin instance for {:?} slot {}: {} ({} ch, created: {})", spec.track, spec.slot, spec.id, spec.ch, made.is_some());
            self.shadow.insert(key, (spec.id.clone(), spec.ch, made.is_some()));
            if let Some(soundcraft_mix::NewInstance { mut plugin, refused_state }) = made {
                if let Some(e) = plugin.editor() {
                    self.editors.insert(key, e);
                }
                if let Some(r) = plugin.state_reader() {
                    self.states.insert(key, r);
                }
                if refused_state {
                    self.refused.insert(key);
                }
                out.push(PreparedInstance { track: spec.track, slot: spec.slot, id: spec.id, ch: spec.ch, plugin });
            }
        }
        out
    }

    /// Builds warp players for beat-locked clips the audio thread has no player for yet, so it
    /// never builds a stretcher mid-mix: those of at most `budget` clips, nearest the playhead
    /// first. When all are built, `warps_for` is cleared. Only clips the engine would play
    /// through a player are built ([`soundcraft_mix::warp_clip_plays`]). A split clip needs one
    /// player per stem ([`soundcraft_mix::played_sources`]); a clip's missing players are always
    /// built together, so a batch never stops part way through its stems. Stems its mask or
    /// faders silence get one too, so bringing them back never waits for a player.
    ///
    /// Players are kept per track, so a clip moved to another track gets new ones; a change of
    /// audio (a re-split), source channels or sample rate replaces one. Until its player arrives
    /// a stem is left out; an unsplit clip builds its player on the audio thread (the fallback).
    fn prepare_warps(&mut self, s: &Session, playhead: Samples, budget: usize) -> Vec<PreparedWarp> {
        let sr = s.sample_rate.hz();
        let mut live = HashSet::new();
        let mut todo = Vec::new();
        for t in &s.tracks {
            for c in t.clips().iter().filter(|c| soundcraft_mix::warp_clip_plays(t, c)) {
                let mut missing = Vec::new();
                for (stem, source) in soundcraft_mix::played_sources(c) {
                    let ch = s.pool.get(source).map_or(0, |a| a.buffer.channels.len());
                    if ch == 0 {
                        continue;
                    }
                    let key = (t.id, WarpFor::Clip(c.id.0), stem);
                    live.insert(key);
                    if self.prepared_warps.get(&key) != Some(&(source, ch, sr)) {
                        missing.push((WarpFor::Clip(c.id.0), stem, source, ch));
                    }
                }
                if !missing.is_empty() {
                    let r = c.range();
                    let distance = if r.contains(playhead) {
                        0
                    } else {
                        r.start.saturating_sub(playhead).saturating_abs().min(r.end.saturating_sub(playhead).saturating_abs())
                    };
                    todo.push((distance, t.id, WarpFor::Clip(c.id.0), missing));
                }
            }
        }
        // Live columns' player pools (three players per audio their loops play, as wide as the
        // widest): ready before anything, since a launch can come at any moment. A lost column
        // plays nothing.
        for track in (0..s.live.columns.len()).filter_map(|c| s.live.column_track(c, &s.tracks)) {
            let (keys, ch) = soundcraft_mix::pool_needs(s, track);
            if ch == 0 {
                continue;
            }
            let mut missing = Vec::new();
            for stem in keys {
                for index in 0..3 {
                    let key = (track, WarpFor::Pool(index), stem);
                    live.insert(key);
                    if self.prepared_warps.get(&key) != Some(&(SourceId(0), ch, sr)) {
                        missing.push((WarpFor::Pool(index), stem, SourceId(0), ch));
                    }
                }
            }
            if !missing.is_empty() {
                todo.push((-1, track, WarpFor::Pool(0), missing));
            }
        }
        let before = self.prepared_warps.len();
        self.prepared_warps.retain(|k, _| live.contains(k));
        self.stale_warps = before - self.prepared_warps.len();
        self.failed_warps.retain(|k| live.contains(k));
        todo.sort_unstable_by_key(|x| (x.0, x.1, x.2));
        let all = todo.len() <= budget;
        let mut failed = false;
        let mut out = Vec::new();
        for (_, track, _, missing) in todo.into_iter().take(budget) {
            for (owner, stem, source, ch) in missing {
                let key = (track, owner, stem);
                let rate = s.sample_rate.as_f64() as f32;
                let made = match owner {
                    WarpFor::Clip(id) => PreparedWarp::new(track, soundcraft_model::ClipId(id), stem, ch, rate),
                    WarpFor::Pool(index) => PreparedWarp::for_pool(track, stem, index, ch, rate),
                };
                match made {
                    Some(w) => {
                        self.prepared_warps.insert(key, (source, ch, sr));
                        self.failed_warps.remove(&key);
                        out.push(w);
                    }
                    None => {
                        // Not remembered, so it is tried again (a stem would stay silent).
                        failed = true;
                        if self.failed_warps.insert(key) {
                            log::warn!("could not build a time-stretch player for {owner:?} (stem {stem}); retrying");
                        }
                    }
                }
            }
        }
        if all && !failed {
            self.warps_for = None;
        }
        out
    }
}

/// ITU stereo fold-down gains for each main channel of a session.
fn fold_gains(s: &Session) -> [(f32, f32); MAX_CHANNELS] {
    let mut g = [(0.0, 0.0); MAX_CHANNELS];
    let f = s.main_format();
    let sp = f.speakers();
    if sp.len() == f.channels() {
        for (o, x) in g.iter_mut().zip(sp.iter()) {
            *o = itu_stereo_fold(*x);
        }
    } else {
        // Ambisonics: W only, at -3 dB per side.
        g[0] = (std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2);
    }
    g
}

/// The output stream: the mix renders f32 into a scratch buffer allocated here (so the callback
/// never allocates), converted to the device's sample type.
fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: AudioState,
    channels: usize,
    fault: &Arc<Fault>,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let frame = channels.max(1);
    let mut scratch = vec![0.0f32; 8192 * frame];
    let chunk = scratch.len();
    let (failed, guard, gone) = (Arc::clone(fault), Arc::clone(shared), Arc::clone(fault));
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            mark_audio_thread();
            for out in data.chunks_mut(chunk) {
                let Some(s) = scratch.get_mut(..out.len()) else { return };
                guarded_block(&failed, &guard, s, |d| state.fill(d, channels));
                for (o, x) in out.iter_mut().zip(s.iter()) {
                    *o = T::from_sample(*x);
                }
            }
        },
        // cpal may call this on the audio thread (on ALSA in a loop while the device stays
        // broken): marked, so the logger hands the record off instead of writing it.
        move |e: cpal::StreamError| {
            mark_audio_thread();
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                gone.gone.store(true, Ordering::Relaxed);
            }
            log::warn!("audio stream error: {e}");
        },
        None,
    )
}

/// Open the output device `cfg` chooses (else the default) at the chosen rate (else the session
/// rate) if possible, in the best sample format it offers (f32, else 32- or 16-bit integers),
/// with as many channels as it offers up to `want_ch` (at least stereo when available), and the
/// chosen buffer size when the device allows it.
fn open_device(want_rate: u32, want_ch: u16, cfg: &AudioConfig) -> Result<(cpal::Device, cpal::StreamConfig, cpal::SampleFormat, String), String> {
    use cpal::traits::DeviceTrait;
    let host = devices::host(cfg);
    let device = devices::device(&host, cfg.output.as_deref(), true).ok_or_else(|| "no output device".to_string())?;
    #[allow(deprecated)]
    let name = device.name().unwrap_or_else(|_| "Audio device".into());
    // Prefer a config at the chosen (else session) rate; else the device default (we resample).
    let want_ch = want_ch.max(2);
    let rates: Vec<u32> = cfg.sample_rate.into_iter().chain(Some(want_rate)).collect();
    let mut chosen: Option<(cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat)> = None;
    // Better: a preferred format first, then more channels.
    let better = |c: &cpal::SupportedStreamConfigRange, x: &(cpal::StreamConfig, cpal::SupportedBufferSize, cpal::SampleFormat)| {
        let (rc, rx) = (devices::format_rank(c.sample_format()).unwrap_or(usize::MAX), devices::format_rank(x.2).unwrap_or(usize::MAX));
        rc < rx || (rc == rx && c.channels() > x.0.channels)
    };
    if let Ok(configs) = device.supported_output_configs() {
        let configs: Vec<cpal::SupportedStreamConfigRange> = configs.collect();
        for rate in rates {
            for c in &configs {
                if devices::format_rank(c.sample_format()).is_some()
                    && c.min_sample_rate().0 <= rate
                    && c.max_sample_rate().0 >= rate
                    && c.channels() >= 2
                    && c.channels() <= want_ch
                    && chosen.as_ref().is_none_or(|x| better(c, x))
                {
                    chosen = Some((c.with_sample_rate(cpal::SampleRate(rate)).config(), *c.buffer_size(), c.sample_format()));
                }
            }
            if chosen.is_some() {
                break;
            }
        }
    }
    let (config, format) = match chosen {
        Some((mut c, buffers, format)) => {
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
        None => {
            let d = device.default_output_config().map_err(|e| e.to_string())?;
            if devices::format_rank(d.sample_format()).is_none() {
                return Err(format!("unsupported device sample format {:?}", d.sample_format()));
            }
            let buffers = *d.buffer_size();
            let format = d.sample_format();
            let mut c = d.config();
            c.buffer_size = devices::buffer_size(cfg.buffer_size, &buffers);
            (c, format)
        }
    };
    Ok((device, config, format, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reusable_meter_snapshot_updates_and_drops_stale_tracks() {
        let keep = TrackId(1);
        let stale = TrackId(2);
        let mut source = MeterSnapshot::default();
        source.tracks.insert(keep, StripMeter { peak: [0.5, 0.25], peaks: vec![0.5, 0.25], gain_reduction_db: 3.0, ..StripMeter::default() });
        source.main.peaks = vec![0.75, 0.5];
        let mut out = MeterSnapshot::default();
        out.tracks.insert(stale, StripMeter::default());
        copy_meter_snapshot(&source, &mut out);
        assert_eq!(out.tracks.len(), 1);
        assert_eq!(out.tracks[&keep], source.tracks[&keep]);
        assert_eq!(out.main, source.main);
    }

    #[test]
    fn a_block_that_panics_goes_silent_and_is_reported_once() {
        let shared = Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(true),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        };
        let fault = Fault::default();
        let mut out = [0.5f32; 8];
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
        assert_eq!((out, fault.trouble(false)), ([0.25; 8], None));
        guarded_block(&fault, &shared, &mut out, |_| std::panic::panic_any("a bug in the mix"));
        guarded_block(&fault, &shared, &mut out, |o| o.fill(0.25));
        assert_eq!(out, [0.0; 8]);
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert_eq!((fault.trouble(false), fault.trouble(false)), (Some(OutputTrouble::Bug), None));
        assert_eq!(fault.beats.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_lost_or_stalled_device_is_reported_once() {
        let ago = |seen: &mut Option<(u64, std::time::Instant)>| {
            if let Some(s) = seen {
                s.1 = std::time::Instant::now().checked_sub(STALL).unwrap();
            }
        };
        let mut seen = None;
        assert!(!stalled(&mut seen, 5, true));
        ago(&mut seen);
        assert!(stalled(&mut seen, 5, true));
        assert!(!stalled(&mut seen, 6, true), "it asked again");
        ago(&mut seen);
        assert!(!stalled(&mut seen, 6, false), "stopped, so nothing to ask for");
        assert_eq!(Fault::default().trouble(true), Some(OutputTrouble::Stalled));
        let fault = Fault::default();
        fault.gone.store(true, Ordering::Relaxed);
        assert_eq!((fault.trouble(false), fault.trouble(true)), (Some(OutputTrouble::Gone), None));
    }

    #[test]
    fn audio_state_plays_and_stops_at_end() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, bin().0);
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(shared.position.load(Ordering::Relaxed) >= 1000);
        assert!(!shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn short_device_callbacks_render_short_mix_blocks() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, bin().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 2 * 64];
        st.fill(&mut out, 2);
        assert_eq!(shared.position.load(Ordering::Relaxed), 64);
    }

    #[test]
    fn surround_main_maps_to_device_channels_or_folds_down() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        let mut s = soundcraft_model::Session::default();
        s.outputs[0].format = ChannelFormat::Surround51;
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Surround51, None);
        let chans: Vec<Vec<f32>> = (0..6).map(|c| vec![0.1 * (c + 1) as f32; 48_000]).collect();
        s.pool.insert(SourceId(5), Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: chans })));
        let id = s.new_clip_id();
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 48_000));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::clone(&s), 48_000.0, bin().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // An 8-channel device: channels in order, the rest silent.
        let mut out = vec![0.0f32; 8 * 256];
        st.fill(&mut out, 8);
        let f = &out[8 * 100..8 * 101];
        for c in 0..6 {
            assert!((f[c] - 0.1 * (c + 1) as f32).abs() < 1e-5, "{f:?}");
        }
        assert!(f[6] == 0.0 && f[7] == 0.0);
        // A stereo device: ITU fold-down.
        let mut out = vec![0.0f32; 2 * 256];
        st.fill(&mut out, 2);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        let l = (0.1 + h * 0.3 + h * 0.5f32).min(1.0);
        assert!((out[200] - l).abs() < 1e-4, "{} vs {l}", out[200]);
        assert_eq!(shared.meters.lock().unwrap().main.peaks.len(), 6);
    }

    #[test]
    fn instances_arrive_with_the_session_and_leave_through_the_return_channel() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("clap:test.fake"));
        let s = Arc::new(s);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let plugin = soundcraft_dsp::create("gain").unwrap();
        tx.send(session_cmd(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }], Vec::new()))
            .unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert!(grx.plugins.try_recv().is_err(), "nothing retired yet");
        // A session without the insert retires the instance to the return channel.
        tx.send(session_cmd(Arc::new(soundcraft_model::Session::default()), Vec::new(), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.plugins.try_recv().unwrap().len(), 1);
        // A sample-rate change rebuilds the engine without dropping plugins on this thread.
        tx.send(session_cmd(
            Arc::clone(&s),
            vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin: soundcraft_dsp::create("gain").unwrap() }],
            Vec::new(),
        ))
        .unwrap();
        st.fill(&mut out, 2);
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(session_cmd(Arc::new(s44), Vec::new(), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.plugins.try_recv().unwrap().len(), 1);
    }

    /// Warp players the audio thread has handed back so far: retired ones, and those in the
    /// tables and lists a warp room replaced.
    fn returned(grx: &BinRx) -> usize {
        grx.warps.try_iter().map(|w| w.len()).sum::<usize>() + grx.rooms.try_iter().map(|r| r.players()).sum::<usize>()
    }

    #[test]
    fn warp_players_are_built_and_dropped_off_the_audio_thread() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]; 2] };
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(buf)));
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let id = s.new_clip_id();
        let mut c = Clip::audio(id, "c", SourceId(900), 0, 0, 48_000);
        c.warp = soundcraft_model::WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, bin().1);
        let warps = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(warps.len(), 1);
        assert!(side.prepare_warps(&s, 0, WARP_BATCH).is_empty(), "built once per clip");
        tx.send(session_cmd(Arc::new(s.clone()), Vec::new(), warps)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 512];
        st.fill(&mut out, 2);
        assert_eq!(returned(&grx), 0, "the clip's player is in use");
        // The clip moves to another track: a new player there, the old one comes back.
        let t2 = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let moved = s.track_mut(t).unwrap().playlist_mut().unwrap().clips.remove(0);
        s.track_mut(t2).unwrap().playlist_mut().unwrap().clips.push(moved);
        let warps = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(warps.len(), 1);
        tx.send(session_cmd(Arc::new(s), Vec::new(), warps)).unwrap();
        // `fill` drains commands (retiring stale players) before it sends the garbage, so the
        // retired player is already on the channel when it returns.
        st.fill(&mut out, 2);
        assert_eq!(returned(&grx), 1);
    }

    #[test]
    fn warp_players_sent_while_stopped_are_all_placed() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind, WarpMap};
        let mut s = soundcraft_model::Session::default();
        let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]] };
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(buf)));
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let n = 70;
        for k in 0..n {
            let id = s.new_clip_id();
            let mut c = Clip::audio(id, "c", SourceId(900), 0, k * 48_000, 48_000);
            c.warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
            s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
        }
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, bin().0);
        let mut side = Side::new(48_000.0, bin().1);
        let s = Arc::new(s);
        side.warps_for = Some(Arc::clone(&s));
        tx.send(session_cmd(Arc::clone(&s), Vec::new(), side.prepare_warps(&s, 0, WARP_BATCH))).unwrap();
        let mut out = vec![0.0f32; 256];
        // Stopped: every UI frame sends a batch, and the callback only drains commands.
        while side.warps_for.is_some() {
            tx.send(warps_cmd(side.prepare_warps(&s, 0, WARP_IDLE_BATCH))).unwrap();
            st.fill(&mut out, 2);
        }
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(st.mix.warp_players().count(), n as usize);
        assert!(st.mix.warp_players().all(|(_, _, _, p)| p.is_prepared()));
    }

    #[test]
    fn only_playable_warp_clips_get_players_built_progressively_nearest_first() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, TrackKind, WarpMap};
        let mut s = soundcraft_model::Session::default();
        let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]; 2] };
        s.pool.insert(SourceId(900), Arc::new(SourceAudio::new(buf)));
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
        let n = WARP_BATCH + 2;
        let clip = |s: &mut soundcraft_model::Session, at: i64| {
            let id = s.new_clip_id();
            let mut c = Clip::audio(id, "c", SourceId(900), 0, at, 48_000);
            c.warp = warp.clone();
            s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
            id
        };
        // Farthest from the playhead first, so ordering isn't left to clip order.
        let ids: Vec<_> = (0..n).rev().map(|k| clip(&mut s, k as i64 * 48_000)).collect();
        let muted = clip(&mut s, 0);
        let unusable = clip(&mut s, 0);
        for c in s.track_mut(t).unwrap().playlist_mut().unwrap().clips.iter_mut() {
            if c.id == muted {
                c.muted = true;
            } else if c.id == unusable
                && let Some(w) = &mut c.warp
            {
                w.beats.truncate(1);
            }
        }
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, bin().1);
        // As `update_session` then `idle` do it.
        let s = Arc::new(s);
        side.warps_for = Some(Arc::clone(&s));
        let first = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(first.len(), WARP_BATCH);
        let nearest: Vec<u64> = ids.iter().rev().take(WARP_BATCH).map(|c| c.0).collect();
        assert!(first.iter().all(|w| nearest.contains(&w.clip().0)), "nearest the playhead first");
        assert!(side.warps_for.is_some(), "more to build");
        tx.send(session_cmd(Arc::clone(&s), Vec::new(), first)).unwrap();
        let rest = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(rest.len(), 2);
        assert!(side.warps_for.is_none(), "all built");
        tx.send(warps_cmd(rest)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 512];
        st.fill(&mut out, 2);
        let players: Vec<_> = st.mix.warp_players().collect();
        assert_eq!(players.len(), n, "one per playable clip, none for the muted or unusable one");
        assert!(players.iter().all(|(_, _, _, p)| p.is_prepared()), "none built on the audio thread");
        // A sample-rate change replaces the engine; its players come back for dropping here.
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(session_cmd(Arc::new(s44), Vec::new(), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(returned(&grx), n);
    }

    /// A split clip gets a player per stem, masked stems too, and a batch takes whole clips (a
    /// split clip's four come together); none is built on the audio thread, a re-split stem gets
    /// a new player, and unsplitting sends the stems' players back.
    #[test]
    fn a_split_clip_gets_a_prepared_player_per_stem() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, StemSet, TrackKind, WarpMap};
        let mut s = soundcraft_model::Session::default();
        for k in 900..905 {
            let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]; 2] };
            s.pool.insert(SourceId(k), Arc::new(SourceAudio::new(buf)));
        }
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let near = s.new_clip_id();
        let mut c = Clip::audio(near, "split", SourceId(900), 0, 0, 48_000);
        c.warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
        c.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)] });
        c.stem_mask = [true, false, true, true];
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
        let far = s.new_clip_id();
        let mut c = Clip::audio(far, "plain", SourceId(900), 0, 96_000, 48_000);
        c.warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, bin().1);
        let first = side.prepare_warps(&s, 0, 1);
        assert_eq!(
            first.iter().map(|w| (w.clip(), w.stem())).collect::<Vec<_>>(),
            (0..4).map(|k| (near, k)).collect::<Vec<_>>(),
            "one clip: all four of its stems"
        );
        let rest = side.prepare_warps(&s, 0, 1);
        assert_eq!(rest.iter().map(|w| (w.clip(), w.stem())).collect::<Vec<_>>(), [(far, soundcraft_mix::SOURCE_PLAYER)]);
        tx.send(session_cmd(Arc::new(s.clone()), Vec::new(), first)).unwrap();
        tx.send(warps_cmd(rest)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 512];
        st.fill(&mut out, 2);
        assert_eq!(st.mix.warp_players().count(), 5);
        assert!(st.mix.warp_players().all(|(_, _, _, p)| p.is_prepared()), "none built on the audio thread");
        // Re-split with new drums: only that stem's player is rebuilt.
        if let Some(set) = &mut s.track_mut(t).unwrap().playlist_mut().unwrap().clips[0].stems {
            set.sources[1] = SourceId(900);
        }
        let resplit = side.prepare_warps(&s, 0, 8);
        assert_eq!(resplit.iter().map(|w| (w.clip(), w.stem())).collect::<Vec<_>>(), [(near, 1)]);
        tx.send(session_cmd(Arc::new(s.clone()), Vec::new(), resplit)).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(returned(&grx), 1, "the old drums player comes back");
        // Unsplit: the source needs a player again, and the four stem players come back.
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips[0].stems = None;
        let again = side.prepare_warps(&s, 0, 8);
        assert_eq!(again.iter().map(|w| (w.clip(), w.stem())).collect::<Vec<_>>(), [(near, soundcraft_mix::SOURCE_PLAYER)]);
        tx.send(session_cmd(Arc::new(s), Vec::new(), again)).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(returned(&grx), 4);
    }

    /// Adds a stereo audio track with `n` split, warped clips a second apart from `at` (pool
    /// sources 900..904 must exist). Returns the track and clip ids.
    fn add_split_track(s: &mut Session, n: i64, at: i64) -> (TrackId, Vec<soundcraft_model::ClipId>) {
        use soundcraft_model::{ChannelFormat, Clip, SourceId, StemSet, TrackKind, WarpMap};
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let mut ids = Vec::new();
        for k in 0..n {
            let id = s.new_clip_id();
            let mut c = Clip::audio(id, "split", SourceId(900), 0, at + k * 48_000, 48_000);
            c.warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
            c.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)] });
            s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
            ids.push(id);
        }
        (t, ids)
    }

    fn stem_pool() -> Session {
        let mut s = Session::default();
        for k in 900..905 {
            let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]; 2] };
            s.pool.insert(soundcraft_model::SourceId(k), Arc::new(soundcraft_model::SourceAudio::new(buf)));
        }
        s
    }

    /// [`live_session`], for the live state tests.
    pub(super) fn live_session_for_state() -> (Session, TrackId) {
        live_session()
    }

    /// A session with a Live column whose slots 0 (a loop) and 1 (a split loop) are filled.
    fn live_session() -> (Session, TrackId) {
        use soundcraft_model::{ChannelFormat, LiveColumn, Slot, SlotMode, SourceAudio, StemSet, TrackKind, WarpMap};
        let mut s = Session::default();
        for k in 900..905 {
            let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 192_000]; 2] };
            s.pool.insert(SourceId(k), Arc::new(SourceAudio::new(buf)));
        }
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let mut col = LiveColumn::new(t);
        let mut slot = Slot::one_shot("loop", [0; 3], SourceId(900));
        slot.warp = WarpMap::new((0..8).map(|k| f64::from(k) * 24_000.0).collect(), 0.0);
        slot.mode = SlotMode::Loop;
        slot.length_beats = 4.0;
        col.slots[0] = Some(slot.clone().into());
        slot.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)] });
        col.slots[1] = Some(slot.into());
        s.live.columns.push(col);
        (s, t)
    }

    /// Every looping slot's players are built on the UI side, first, and its column storage goes
    /// once; on the audio thread a launch plays at its sample, a locate keeps it playing and a
    /// transport stop clears it.
    #[test]
    fn live_slots_get_prepared_players_and_stop_with_the_transport() {
        let (s, t) = live_session();
        let mut side = Side::new(48_000.0, bin().1);
        let warps = side.prepare_warps(&s, 0, 1);
        let mut got: Vec<_> = warps.iter().map(|w| (w.pool_index(), w.stem())).collect();
        got.sort_unstable();
        let sp = soundcraft_mix::SOURCE_PLAYER;
        let want: Vec<_> = [0, 1, 2, 3, sp].into_iter().flat_map(|k| (0..3).map(move |i| (Some(i), k))).collect();
        let mut want = want;
        want.sort_unstable();
        assert_eq!(got, want, "a column's pool (three players per audio its loops play) comes as one in a batch");
        assert!(warps.iter().all(|w| w.player_channels() == 2), "as wide as its widest audio");
        let more = side.prepare_warps(&s, 0, WARP_BATCH);
        assert!(more.is_empty(), "built once");
        let (gtx, _grx) = bin();
        let (tx, mut st) = audio_state(gtx);
        let mut batch = warps;
        batch.extend(more);
        tx.send(session_cmd(Arc::new(s.clone()), Vec::new(), batch)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        mark_audio_thread();
        let mut out = vec![0.0f32; 1024];
        st.fill(&mut out, 2);
        assert_eq!(st.mix.slot_players().count(), 15);
        let cap = st.mix.slot_table_capacity(t);
        tx.send(Cmd::Live(LiveCmd::Launch { column: 0, slot: 1, at: 2_000 })).unwrap();
        for _ in 0..8 {
            st.fill(&mut out, 2);
        }
        assert!(st.mix.live_state()[0].playing.is_some_and(|p| p.slot == 1 && p.start == 2_000));
        assert!(out.iter().any(|x| x.abs() > 0.01), "the slot sounds");
        tx.send(Cmd::Locate(96_000)).unwrap();
        st.fill(&mut out, 2);
        assert!(st.mix.live_state()[0].playing.is_some(), "a locate keeps it playing");
        tx.send(Cmd::Stop).unwrap();
        st.fill(&mut out, 2);
        assert!(st.mix.live_state().iter().all(|c| c.playing.is_none() && c.queued.is_none()), "stop clears");
        assert_eq!(st.mix.slot_table_capacity(t), cap);
        // A later snapshot sends no second storage for the column.
        let mut side = Side::new(48_000.0, bin().1);
        side.live_stores.insert(t);
        let sent = side.live_stores.clone();
        let Cmd::Session(_, _, _, room) = session_cmd_with(Arc::new(s), Vec::new(), Vec::new(), 0, |t| !sent.contains(&t)) else { panic!() };
        assert_eq!(room.live_stores(), 0);
    }

    /// A launch sent before the play command (while stopped) plays; one for the end of the
    /// transport's loop range plays from its start when the transport wraps.
    #[test]
    fn launches_survive_play_and_loop_wraps() {
        let (s, _) = live_session();
        let (gtx, _grx) = bin();
        let (tx, mut st) = audio_state(gtx);
        tx.send(session_cmd(Arc::new(s), Vec::new(), Vec::new())).unwrap();
        tx.send(Cmd::Live(LiveCmd::Launch { column: 0, slot: 0, at: 1_000 })).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 8_192)) }).unwrap();
        let mut out = vec![0.0f32; 1024];
        for _ in 0..4 {
            st.fill(&mut out, 2);
        }
        assert!(st.mix.live_state()[0].playing.is_some_and(|p| p.start == 1_000), "sent before play, it plays");
        tx.send(Cmd::Live(LiveCmd::Stop { column: 0, at: 8_192 })).unwrap();
        while st.pos < 8_192 && st.pos >= 1_000 {
            st.fill(&mut out, 2);
        }
        // The stop for the loop's end landed on the wrap.
        for _ in 0..2 {
            st.fill(&mut out, 2);
        }
        assert!(st.mix.live_state()[0].playing.is_none() && st.mix.live_state()[0].queued.is_none());
    }

    fn audio_state(gtx: Bin) -> (Sender<Cmd>, AudioState) {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        (tx, AudioState::new(rx, shared, Arc::new(Session::default()), 48_000.0, gtx))
    }

    /// Spent warp rooms go back over a bounded channel: when the UI thread falls behind, the
    /// audio thread keeps the room it could not send and sends it on a later callback rather
    /// than allocating or freeing; a room holding no storage is not sent at all.
    #[test]
    fn a_full_room_channel_keeps_the_room_and_retries() {
        let mut s = stem_pool();
        add_split_track(&mut s, 1, 0);
        let s = Arc::new(s);
        let (gtx, grx) = bin();
        let (tx, mut st) = audio_state(gtx);
        let mut out = vec![0.0f32; 256];
        for _ in 0..ROOMS + 3 {
            tx.send(session_cmd(Arc::clone(&s), Vec::new(), Vec::new())).unwrap();
            st.fill(&mut out, 2);
        }
        assert_eq!(grx.rooms.try_iter().count(), ROOMS, "the channel is full");
        assert!(st.room_backlog.is_some(), "one room waits");
        st.fill(&mut out, 2);
        assert_eq!(grx.rooms.try_iter().count(), 1, "and is sent once there is room");
        assert!(st.room_backlog.is_none());
        tx.send(warps_cmd(Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.rooms.try_iter().count(), 0, "a bare room is not sent");
    }

    /// Deleting a track and re-splitting a clip in one snapshot retires 9 players (8 with the
    /// track, 1 replaced): the snapshot's room was sized for them on the UI thread, so the
    /// retired list comes back at exactly that size, never grown on the audio thread.
    #[test]
    fn deleting_a_track_and_re_splitting_never_grows_the_retired_list() {
        let mut s = stem_pool();
        let (gone, _) = add_split_track(&mut s, 2, 0);
        let (kept, _) = add_split_track(&mut s, 1, 0);
        let (gtx, grx) = bin();
        let (tx, mut st) = audio_state(gtx);
        let mut side = Side::new(48_000.0, bin().1);
        let warps = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(warps.len(), 12);
        tx.send(session_cmd_with(Arc::new(s.clone()), Vec::new(), warps, side.stale_warps, |_| true)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert_eq!(st.mix.warp_players().count(), 12);
        s.tracks.retain(|t| t.id != gone);
        if let Some(set) = &mut s.track_mut(kept).unwrap().playlist_mut().unwrap().clips[0].stems {
            set.sources[1] = soundcraft_model::SourceId(900);
        }
        let warps = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!((warps.len(), side.stale_warps), (1, 8));
        tx.send(session_cmd_with(Arc::new(s), Vec::new(), warps, side.stale_warps, |_| true)).unwrap();
        st.fill(&mut out, 2);
        let retired: Vec<_> = grx.warps.try_iter().collect();
        assert_eq!(retired.iter().map(|w| (w.len(), w.capacity())).collect::<Vec<_>>(), [(9, 9)]);
        assert_eq!(st.mix.warp_players().count(), 4);
    }

    /// Locating (or playing) far from the playhead first sends the players of the split clips
    /// there, so they play their stems from the first block.
    #[test]
    fn a_locate_prepares_the_clips_at_the_target_first() {
        let mut s = stem_pool();
        let (_, ids) = add_split_track(&mut s, 12, 0);
        let s = Arc::new(s);
        let (gtx, _grx) = bin();
        let (tx, mut st) = audio_state(gtx);
        let mut side = Side::new(48_000.0, bin().1);
        side.warps_for = Some(Arc::clone(&s));
        let first = side.prepare_warps(&s, 0, WARP_BATCH);
        tx.send(session_cmd(Arc::clone(&s), Vec::new(), first)).unwrap();
        let far = ids[11];
        let at = 11 * 48_000 + 1_000;
        // As `Player::locate` does it.
        if let Some(cmd) = side.prepare_around(at) {
            tx.send(cmd).unwrap();
        }
        tx.send(Cmd::Locate(at)).unwrap();
        tx.send(Cmd::Play { from: at, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        let mut there: Vec<_> = st.mix.warp_players().filter(|(_, c, _, _)| *c == far.0).map(|(_, _, k, p)| (k, p.is_prepared())).collect();
        there.sort_unstable();
        assert_eq!(there, (0..4).map(|k| (k, true)).collect::<Vec<_>>());
        assert!(out.iter().any(|x| x.abs() > 0.0), "the stems play from the first block");
    }

    /// Twelve split clips on one track need 48 players: the room sent with the snapshot holds
    /// them all, so placing them, batch by batch while playing, never grows the strip's table
    /// on the audio thread.
    #[test]
    fn split_clips_never_grow_the_audio_threads_player_table() {
        use soundcraft_model::{ChannelFormat, Clip, SourceAudio, SourceId, StemSet, TrackKind, WarpMap};
        let mut s = soundcraft_model::Session::default();
        for k in 900..905 {
            let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]; 2] };
            s.pool.insert(SourceId(k), Arc::new(SourceAudio::new(buf)));
        }
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        for k in 0..12 {
            let id = s.new_clip_id();
            let mut c = Clip::audio(id, "split", SourceId(900), 0, k * 48_000, 48_000);
            c.warp = WarpMap::new(vec![0.0, 24_000.0, 48_000.0], 0.0);
            c.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)] });
            s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
        }
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, bin().0);
        let mut side = Side::new(48_000.0, bin().1);
        let s = Arc::new(s);
        side.warps_for = Some(Arc::clone(&s));
        // The snapshot first, then its players batch by batch (8 clips, then 2 per UI frame).
        tx.send(session_cmd(Arc::clone(&s), Vec::new(), Vec::new())).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 512];
        st.fill(&mut out, 2);
        let cap = st.mix.warp_table_capacity(t);
        assert!(cap >= 48, "room for every player, got {cap}");
        let mut budget = WARP_BATCH;
        while side.warps_for.is_some() {
            tx.send(warps_cmd(side.prepare_warps(&s, 0, budget))).unwrap();
            budget = WARP_IDLE_BATCH;
            st.fill(&mut out, 2);
            assert_eq!(st.mix.warp_table_capacity(t), cap, "the table never grows on the audio thread");
        }
        assert_eq!(st.mix.warp_players().count(), 48);
        assert!(st.mix.warp_players().all(|(_, _, _, p)| p.is_prepared()));
    }

    #[test]
    fn plugin_states_are_read_without_the_audio_thread() {
        let mut side = Side::new(48_000.0, bin().1);
        let (a, b) = (TrackId(1), TrackId(2));
        side.states.insert((b, 0), Box::new(|| Some(b"b0".to_vec())));
        side.states.insert((a, soundcraft_mix::INSTRUMENT_INSTANCE), Box::new(|| Some(b"synth".to_vec())));
        side.states.insert((a, 3), Box::new(|| Some(b"a3".to_vec())));
        side.states.insert((a, 4), Box::new(|| None));
        let want = vec![(a, 3, b"a3".to_vec()), (a, soundcraft_mix::INSTRUMENT_INSTANCE, b"synth".to_vec()), (b, 0, b"b0".to_vec())];
        assert_eq!(side.capture_states(), want);
        side.forget((a, 3));
        assert_eq!(side.capture_states(), want[1..]);
    }

    struct Window;

    impl PluginEditor for Window {
        fn open(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn close(&mut self) {}
        fn is_open(&self) -> bool {
            false
        }
        fn idle(&mut self) -> Vec<(String, f32)> {
            Vec::new()
        }
    }

    #[test]
    fn a_refused_state_is_kept_until_the_editor_opens() {
        let mut side = Side::new(48_000.0, bin().1);
        let k = (TrackId(1), 0);
        side.states.insert(k, Box::new(|| Some(b"defaults".to_vec())));
        side.editors.insert(k, Box::new(Window));
        side.refused.insert(k);
        assert!(side.capture_states().is_empty());
        side.open_editor(k).unwrap();
        assert_eq!(side.capture_states(), vec![(TrackId(1), 0, b"defaults".to_vec())]);
    }

    #[test]
    fn side_creates_each_missing_instance_once() {
        use soundcraft_model::{ChannelFormat, Insert, TrackKind};
        let mut side = Side::new(48_000.0, bin().1);
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.inserts[0] = Some(Insert::new("clap:does.not.exist"));
        s.track_mut(t).unwrap().mixer.inserts[1] = Some(Insert::new("gain"));
        assert!(side.prepare(&s).is_empty(), "the plugin is not installed; built-ins stay with the engine");
        assert_eq!(side.shadow.get(&(t, 0)), Some(&("clap:does.not.exist".to_string(), 1, false)));
        assert!(side.prepare(&s).is_empty());
        assert_eq!(side.shadow.len(), 1, "failures are remembered, not retried every update");
        s.track_mut(t).unwrap().mixer.inserts[0] = None;
        side.prepare(&s);
        assert!(side.shadow.is_empty());
    }

    #[test]
    fn replaced_sessions_are_freed_on_the_ui_thread() {
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, grx);
        side.return_old_sessions(&tx);
        let mut out = vec![0.0f32; 256];
        let mut watched = Vec::new();
        for _ in 0..3 * OLD_SESSIONS {
            let s = Arc::new(soundcraft_model::Session::default());
            watched.push(Arc::downgrade(&s));
            side.collect_garbage();
            tx.send(session_cmd(s, Vec::new(), Vec::new())).unwrap();
            st.fill(&mut out, 2);
            let replaced = watched.len().checked_sub(2).and_then(|i| watched.get(i));
            assert!(replaced.is_none_or(|w| w.strong_count() == 1), "freed on the audio thread");
        }
        side.collect_garbage();
        assert_eq!(watched.iter().filter(|w| w.strong_count() > 0).count(), 1, "only the live snapshot is left");
    }

    #[test]
    fn looping_wraps() {
        let s = Arc::new(soundcraft_model::Session::default());
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), s, 48_000.0, bin().0);
        tx.send(Cmd::Play { from: 0, end: None, looped: Some(Range::new(0, 1000)) }).unwrap();
        let mut out = vec![0.0f32; 20_000];
        st.fill(&mut out, 2);
        let p = shared.position.load(Ordering::Relaxed);
        assert!(p <= 1000, "position {p} should stay inside the loop");
        assert!(shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn locate_while_playing_drops_the_old_position_audio() {
        use soundcraft_model::{ChannelFormat, Clip, Insert, SourceAudio, SourceId, TrackKind};
        for (plugin, params) in [("compressor", &[("lookahead", 5.0), ("ratio", 1.0)][..]), ("room_reverb", &[][..])] {
            let mut s = soundcraft_model::Session::default();
            s.edit.click = false;
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            s.pool.insert(
                SourceId(5),
                Arc::new(SourceAudio::new(soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4800]] })),
            );
            let id = s.new_clip_id();
            let tr = s.track_mut(t).unwrap();
            tr.playlist_mut().unwrap().clips.push(Clip::audio(id, "x", SourceId(5), 0, 0, 4800));
            let mut ins = Insert::new(plugin);
            for (k, v) in params {
                ins.params.insert((*k).into(), *v);
            }
            tr.mixer.inserts[0] = Some(ins);
            let shared = Arc::new(Shared {
                position: AtomicI64::new(0),
                playing: AtomicBool::new(false),
                speed: AtomicU32::new(1000),
                meters: Mutex::new(MeterSnapshot::default()),
                live: LiveShared::default(),
                midi: MidiShared::new(),
            });
            let (tx, rx) = channel();
            let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, bin().0);
            tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
            let mut out = vec![0.0f32; 2 * 512];
            let mut before = 0.0f32;
            for _ in 0..9 {
                st.fill(&mut out, 2);
                before = out.iter().fold(before, |m, x| m.max(x.abs()));
            }
            assert!(before > 0.1, "{plugin}: the clip plays before the locate");
            tx.send(Cmd::Locate(1_000_000)).unwrap();
            for _ in 0..8 {
                st.fill(&mut out, 2);
                assert!(out.iter().all(|x| x.abs() < 1e-6), "{plugin}: audio from before the locate");
            }
        }
    }

    #[test]
    fn armed_tracks_are_heard_and_metered_after_playback_stops() {
        use soundcraft_model::{ChannelFormat, TrackKind};
        let mut s = soundcraft_model::Session::default();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        s.track_mut(t).unwrap().mixer.record_arm = true;
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(s), 48_000.0, bin().0);
        let ring = record::InputRing::new(1, 48_000);
        tx.send(Cmd::Input(Arc::clone(&ring))).unwrap();
        tx.send(Cmd::Play { from: 0, end: Some(1000), looped: None }).unwrap();
        let mut out = vec![0.0f32; 4096];
        st.fill(&mut out, 2);
        assert!(!shared.playing.load(Ordering::Relaxed));
        ring.push(&[0.5; 4096]);
        st.fill(&mut out, 2);
        assert!(out.iter().any(|x| x.abs() > 0.1), "the input is heard");
        assert!(shared.meters.lock().unwrap().tracks.get(&t).is_some_and(|m| m.peak[0] > 0.1), "and metered");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod live_state_tests {
    use super::*;

    #[test]
    fn live_state_reads_back_what_the_audio_thread_published() {
        let shared = LiveShared::default();
        assert_eq!(shared.read(), [LiveColumnState::default(); MAX_LIVE_COLUMNS], "idle before anything is published");
        // Column 0 is queued to stop at 4800; column 2 waits to launch row 7 at 9600.
        let mut mix = new_engine(48_000.0);
        mix.live(LiveCmd::Launch { column: 2, slot: 7, at: 9_600 });
        mix.live(LiveCmd::Stop { column: 0, at: 4_800 });
        shared.publish(mix.live_state());
        let read = shared.read();
        assert_eq!(read[0], LiveColumnState { playing: None, queued: Some((None, 4_800)), late: 0, origin: 0 });
        assert_eq!(read[2], LiveColumnState { playing: None, queued: Some((Some(7), 9_600)), late: 0, origin: 0 });
        assert!(read.iter().enumerate().all(|(c, s)| c == 0 || c == 2 || *s == LiveColumnState::default()));
    }

    /// A read that keeps meeting a write under way returns the last state read whole, never a
    /// torn one.
    #[test]
    fn a_read_during_a_write_returns_the_last_whole_state() {
        let shared = LiveShared::default();
        let mut mix = new_engine(48_000.0);
        mix.live(LiveCmd::Stop { column: 0, at: 4_800 });
        shared.publish(mix.live_state());
        let good = shared.read();
        // A writer stuck half way: the sequence is odd and the words half new.
        shared.seq.fetch_add(1, Ordering::Relaxed);
        shared.words[0][3].store(9_999, Ordering::Relaxed);
        assert_eq!(shared.read(), good);
    }

    /// The longest callback is published in session samples (device frames at the session rate
    /// and speed); late launches are published per column.
    #[test]
    fn the_longest_callback_and_late_launches_are_published() {
        let (s, _) = super::tests::live_session_for_state();
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        // A 44.1 kHz device under a 48 kHz session: 441 frames are 480 session samples.
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(Session::default()), 44_100.0, bin().0);
        tx.send(session_cmd(Arc::new(s), Vec::new(), Vec::new())).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        st.fill(&mut vec![0.0f32; 2 * 441], 2);
        st.fill(&mut vec![0.0f32; 2 * 100], 2);
        assert_eq!(shared.live.max_callback.load(Ordering::Relaxed), 480);
        // A launch for a sample already played is late.
        tx.send(Cmd::Live(LiveCmd::Launch { column: 0, slot: 0, at: 10 })).unwrap();
        st.fill(&mut vec![0.0f32; 2 * 441], 2);
        assert_eq!(shared.live.read()[0].late, 1);
        assert_eq!(shared.live.read()[0].playing.map(|p| p.1), Some(10), "on the grid from its own sample");
    }
}

/// Live MIDI input through the audio thread, from a [`soundcraft_midi_io::FakeSource`] (no real
/// ports): timing against the callback and `L`, the pending buffer, per-port order, releases,
/// bursts, and the queue's return off the audio thread.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod midi_tests {
    use super::*;
    use soundcraft_midi_io::{FakeSource, MidiEvent, MidiSource, PortChange};
    use soundcraft_mix::INSTRUMENT_INSTANCE;
    use soundcraft_model::{ChannelFormat, Insert, TrackKind};

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Ev {
        On(u8),
        Off(u8),
        AllOff,
        Midi([u8; 3]),
    }

    /// Logs (block it was sent for, offset, event, timeline sample).
    type Log = Arc<Mutex<Vec<(u64, usize, Ev, i64)>>>;

    struct Recorder {
        inner: Box<dyn Plugin>,
        log: Log,
        blocks: u64,
        /// Blocks processed, for the test to read.
        processed: Arc<AtomicU64>,
        /// The block's timeline position (from the transport).
        pos: i64,
        /// The transport plays: the instrument then outputs a steady 0.5 (stale audio shows).
        playing: bool,
    }

    impl Recorder {
        fn push(&self, offset: usize, e: Ev) {
            self.log.lock().unwrap().push((self.blocks, offset, e, self.pos + offset as i64));
        }
    }

    impl Plugin for Recorder {
        fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
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
            self.blocks += 1;
            self.processed.fetch_add(1, Ordering::Relaxed);
            let v = if self.playing { 0.5 } else { 0.0 };
            for c in io.iter_mut() {
                c.iter_mut().take(frames).for_each(|x| *x = v);
            }
        }
        fn set_transport(&mut self, t: &soundcraft_dsp::Transport) {
            self.pos = t.samples;
            self.playing = t.playing;
        }
        fn note_on(&mut self, offset: usize, note: u8, _velocity: u8) {
            self.push(offset, Ev::On(note));
        }
        fn note_off(&mut self, offset: usize, note: u8) {
            self.push(offset, Ev::Off(note));
        }
        fn all_notes_off(&mut self) {
            self.push(0, Ev::AllOff);
        }
        fn midi(&mut self, offset: usize, msg: [u8; 3]) {
            self.push(offset, Ev::Midi(msg));
        }
    }

    /// 256-frame callbacks at 48 kHz on a 48 kHz session: one 256-sample block each.
    const CB: usize = 256;
    const CB_NS: u64 = 5_333_333;

    struct Rig {
        tx: Sender<Cmd>,
        st: AudioState,
        shared: Arc<Shared>,
        grx: BinRx,
        log: Log,
        track: TrackId,
        src: FakeSource,
        /// A second queue on the same source: once it has an event, so has the audio thread.
        probe: Subscriber,
        clock: MidiClock,
        /// The next callback's time.
        now: u64,
        /// Blocks the instrument processed.
        processed: Arc<AtomicU64>,
    }

    fn rig() -> Rig {
        rig_at(48_000.0)
    }

    /// The rig on a device running at `rate`.
    fn rig_at(rate: f64) -> Rig {
        let mut s = Session::default();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.rec"));
        let shared = Arc::new(Shared {
            position: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            speed: AtomicU32::new(1000),
            meters: Mutex::new(MeterSnapshot::default()),
            live: LiveShared::default(),
            midi: MidiShared::new(),
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let st = AudioState::new(rx, Arc::clone(&shared), Arc::new(Session::default()), rate, gtx);
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let processed = Arc::new(AtomicU64::new(0));
        let rec = Recorder {
            inner: soundcraft_dsp::create("gain").unwrap(),
            log: Arc::clone(&log),
            blocks: 0,
            processed: Arc::clone(&processed),
            pos: 0,
            playing: false,
        };
        tx.send(session_cmd(
            Arc::new(s),
            vec![PreparedInstance { track: t, slot: INSTRUMENT_INSTANCE, id: "clap:test.rec".into(), ch: 2, plugin: Box::new(rec) }],
            Vec::new(),
        ))
        .unwrap();
        let clock = MidiClock::new();
        let mut src = FakeSource::new(clock).unwrap();
        src.plug("Keys");
        src.plug("Pads");
        src.open("Keys").unwrap();
        src.open("Pads").unwrap();
        tx.send(Cmd::MidiIn(Some((src.subscribe(4096).unwrap(), clock)))).unwrap();
        tx.send(Cmd::MidiTarget(Some(t))).unwrap();
        let probe = src.subscribe(4096).unwrap();
        Rig { tx, st, shared, grx, log, track: t, src, probe, clock, now: 1_000_000_000, processed }
    }

    impl Rig {
        /// One callback at `self.now`, then the clock moves on a callback.
        fn callback(&mut self) {
            let mut out = vec![0.0f32; CB * 2];
            self.st.fill_at(&mut out, 2, self.now);
            self.now += CB_NS;
        }

        /// Waits until the router has delivered `n` more events.
        fn delivered(&mut self, n: usize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut seen = 0;
            while seen < n {
                if self.probe.pop().is_some() {
                    seen += 1;
                } else {
                    assert!(std::time::Instant::now() < deadline, "never delivered");
                    std::thread::yield_now();
                }
            }
        }

        /// Sends `msg` from port `name` stamped `t`, and waits until the router delivered it.
        fn send(&mut self, name: &str, msg: MidiMessage, t: u64) {
            assert!(self.src.inject(name, msg, t));
            self.delivered(1);
        }

        fn log(&self) -> Vec<(u64, usize, Ev, i64)> {
            self.log.lock().unwrap().clone()
        }

        fn notes(&self) -> Vec<(u64, usize, Ev, i64)> {
            self.log().into_iter().filter(|e| matches!(e.2, Ev::On(_) | Ev::Off(_))).collect()
        }

        /// Pitches still sounding.
        fn sounding(&self) -> Vec<u8> {
            let mut on = [false; 128];
            let mut log = self.log();
            log.sort_by_key(|e| (e.0, e.1));
            for (_, _, e, _) in log {
                match e {
                    Ev::On(p) => on[usize::from(p)] = true,
                    Ev::Off(p) => on[usize::from(p)] = false,
                    Ev::AllOff => on = [false; 128],
                    Ev::Midi(_) => {}
                }
            }
            (0..128u8).filter(|p| on[usize::from(*p)]).collect()
        }
    }

    fn note_on(note: u8) -> MidiMessage {
        MidiMessage::NoteOn { ch: 0, note, vel: 100 }
    }

    fn note_off(note: u8) -> MidiMessage {
        MidiMessage::NoteOff { ch: 0, note, vel: 0 }
    }

    /// The sample an event stamped `t` lands on, counted from the start of the callback at `now`.
    fn expected(t: u64, now: u64) -> i64 {
        let l = CB_NS + MIDI_SLACK_NS;
        ((t + l) as i64 - now as i64) * 48 / 1_000_000
    }

    #[test]
    fn a_note_on_lands_l_after_it_arrived_and_a_later_one_waits_for_its_block() {
        let mut r = rig();
        r.callback();
        assert_eq!(r.shared.midi.latency_ns.load(Ordering::Relaxed), CB_NS + MIDI_SLACK_NS, "L: one callback plus 2 ms");
        // Arrived 4 ms before this callback: due 3.33 ms into it.
        let t = r.now - 4_000_000;
        r.send("Keys", note_on(60), t);
        let at = expected(t, r.now);
        assert!((0..CB as i64).contains(&at));
        r.callback();
        let got = r.notes();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].2, Ev::On(60));
        assert!((got[0].1 as i64 - at).abs() <= 1, "offset {} for {at}", got[0].1);
        let block = got[0].0;
        // Arrived 1 ms before the callback: due 6.33 ms in, past this 5.33 ms block.
        let t = r.now - 1_000_000;
        r.send("Keys", note_off(60), t);
        r.callback();
        assert_eq!(r.notes().len(), 1, "waits");
        let next = expected(t, r.now);
        r.callback();
        let got = r.notes();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!((got[1].0, got[1].2), (block + 2, Ev::Off(60)));
        assert!((got[1].1 as i64 - next).abs() <= 1, "offset {} for {next}", got[1].1);
        // Late (arrived long ago): the block's start.
        let t = r.now - 50_000_000;
        r.send("Keys", note_on(62), t);
        r.callback();
        assert_eq!(r.notes().last().map(|e| (e.1, e.2)), Some((0, Ev::On(62))));
    }

    /// A port's events never overtake each other: one stamped earlier than the event waiting
    /// before it waits behind it and plays after it; another port's goes on its own time.
    #[test]
    fn each_ports_events_keep_their_order() {
        let mut r = rig();
        r.callback();
        let now = r.now;
        r.send("Keys", note_on(60), now + 2_000_000);
        r.send("Keys", note_on(61), now - 4_000_000);
        r.send("Pads", note_on(70), now - 4_000_000);
        r.callback();
        assert_eq!(r.notes().iter().map(|e| e.2).collect::<Vec<_>>(), [Ev::On(70)], "the 61 of Keys waits behind its 60");
        r.callback();
        r.callback();
        let got = r.notes();
        assert_eq!(got.iter().map(|e| e.2).collect::<Vec<_>>(), [Ev::On(70), Ev::On(60), Ev::On(61)]);
        assert_eq!(got[1].0, got[2].0, "in one block");
        assert!(got[2].1 >= got[1].1, "never before the one ahead of it: {got:?}");
    }

    #[test]
    fn no_note_is_left_sounding_after_a_target_change_a_stop_a_panic_or_an_unplug() {
        let mut r = rig();
        r.callback();
        let held = |r: &mut Rig, n: u8| {
            let t = r.now - 4_000_000;
            r.send("Keys", note_on(n), t);
            r.callback();
            assert_eq!(r.sounding(), [n]);
        };
        held(&mut r, 60);
        r.tx.send(Cmd::MidiTarget(None)).unwrap();
        r.callback();
        assert_eq!(r.sounding(), Vec::<u8>::new(), "target change");
        r.tx.send(Cmd::MidiTarget(Some(r.track))).unwrap();
        held(&mut r, 61);
        r.tx.send(Cmd::Stop).unwrap();
        r.callback();
        assert_eq!(r.sounding(), Vec::<u8>::new(), "stop");
        held(&mut r, 62);
        r.tx.send(Cmd::MidiPanic).unwrap();
        r.callback();
        assert_eq!(r.sounding(), Vec::<u8>::new(), "panic");
        assert!(r.log().iter().any(|e| e.2 == Ev::Midi([0xB0, 121, 0])), "controllers reset");
        held(&mut r, 63);
        assert!(r.src.unplug("Keys"));
        let closed: Vec<PortId> =
            r.src.refresh().into_iter().filter_map(|c| if let PortChange::Closed { port, .. } = c { Some(port) } else { None }).collect();
        assert_eq!(closed.len(), 1);
        r.tx.send(Cmd::MidiClosed(closed[0])).unwrap();
        r.callback();
        assert_eq!(r.sounding(), Vec::<u8>::new(), "unplugged");
    }

    /// A note and a controller mapped to controls on one port don't play the target from that
    /// port; from another port, and once unmapped, they do. Note-offs always pass.
    #[test]
    fn mapped_notes_and_controllers_do_not_play_the_target() {
        let mut r = rig();
        let keys = r.src.open("Keys").unwrap();
        let cc = |cc: u8| MidiMessage::ControlChange { ch: 0, cc, value: 99 };
        r.callback();
        // Held before its key is mapped: the mapping doesn't strand it.
        r.send("Keys", note_on(64), r.now - 4_000_000);
        r.callback();
        let map = [
            (keys, midi_in::Control::Note { ch: 0, note: 60 }),
            (keys, midi_in::Control::Cc { ch: 0, cc: 7 }),
            (keys, midi_in::Control::Note { ch: 0, note: 64 }),
        ];
        assert!(r.shared.midi.controls.set(&map));
        let t = r.now - 4_000_000;
        r.send("Keys", note_on(60), t);
        r.send("Keys", cc(7), t);
        r.send("Keys", note_on(62), t);
        r.send("Pads", note_on(60), t);
        r.send("Keys", note_off(64), t);
        r.callback();
        let got: Vec<Ev> = r.log().into_iter().map(|e| e.2).filter(|e| !matches!(e, Ev::Midi([0xB0, 121 | 123, _]))).collect();
        assert_eq!(got, [Ev::On(64), Ev::On(62), Ev::On(60), Ev::Off(64)], "Keys' 60 and CC 7 are controls; Pads' 60 plays");
        assert!(r.shared.midi.controls.set(&[]));
        r.send("Keys", note_on(60), r.now - 4_000_000);
        r.callback();
        assert_eq!(r.notes().last().map(|e| e.2), Some(Ev::On(60)), "unmapped: it plays");
    }

    /// A stopped transport with a target renders the target instrument; the on-screen
    /// keyboard's notes play there too.
    #[test]
    fn the_keyboard_plays_the_target_while_stopped() {
        let mut r = rig();
        r.tx.send(Cmd::MidiPlay(note_on(64))).unwrap();
        r.callback();
        r.tx.send(Cmd::MidiPlay(note_off(64))).unwrap();
        r.callback();
        assert_eq!(r.notes().iter().map(|e| (e.1, e.2)).collect::<Vec<_>>(), [(0, Ev::On(64)), (0, Ev::Off(64))]);
        // No target: nothing plays, and nothing piles up.
        r.tx.send(Cmd::MidiTarget(None)).unwrap();
        r.tx.send(Cmd::MidiPlay(note_on(65))).unwrap();
        r.callback();
        assert_eq!(r.notes().len(), 2);
    }

    /// A burst of 1000 events in one block plays without a panic; 1000 more for later fill the
    /// pending buffer, which never grows: the rest are dropped and counted, a note-off excepted.
    #[test]
    fn a_burst_of_a_thousand_events_is_played_or_dropped_and_counted() {
        let mut r = rig();
        r.callback();
        let now = r.now;
        for i in 0..1000u32 {
            let msg = MidiMessage::ControlChange { ch: 0, cc: 1, value: (i % 128) as u8 };
            assert!(r.src.inject("Keys", msg, now - 4_000_000));
        }
        r.delivered(1000);
        r.callback();
        assert_eq!(r.log().iter().filter(|e| matches!(e.2, Ev::Midi([0xB0, 1, _]))).count(), 1000);
        assert_eq!(r.shared.midi.dropped.load(Ordering::Relaxed), 0);
        // A burst due a second from now, from Pads, ending with a note-off.
        let later = r.now + 1_000_000_000;
        for i in 0..1000u32 {
            let msg = if i == 999 { note_off(60) } else { MidiMessage::ControlChange { ch: 0, cc: 2, value: 0 } };
            assert!(r.src.inject("Pads", msg, later));
        }
        r.delivered(1000);
        r.callback();
        assert_eq!(r.st.midi.pending_len(), (MIDI_PENDING, MIDI_PENDING), "full, and never grown");
        assert_eq!(r.shared.midi.dropped.load(Ordering::Relaxed), 1000 - MIDI_PENDING as u64);
        assert!(r.st.midi.pending_any(|e: &MidiEvent| matches!(e.msg, MidiMessage::NoteOff { .. })), "the note-off kept its place");
    }

    #[test]
    fn a_replaced_queue_comes_back_off_the_audio_thread() {
        let mut r = rig();
        r.callback();
        assert!(r.grx.subs.try_recv().is_err());
        r.tx.send(Cmd::MidiIn(Some((r.src.subscribe(16).unwrap(), r.clock)))).unwrap();
        r.callback();
        assert!(r.grx.subs.try_recv().is_ok(), "the old queue came back to be dropped here");
        r.tx.send(Cmd::MidiIn(None)).unwrap();
        r.callback();
        assert!(r.grx.subs.try_recv().is_ok());
    }

    impl Rig {
        /// One callback of `frames` device frames at `rate`, then the clock moves on by it.
        fn callback_of(&mut self, frames: usize, rate: f64) {
            let mut out = vec![0.0f32; frames * 2];
            self.st.fill_at(&mut out, 2, self.now);
            self.now += (frames as f64 / rate * 1e9) as u64;
        }

        fn latency(&self) -> u64 {
            self.shared.midi.latency_ns.load(Ordering::Relaxed)
        }

        /// The fixed storage the live path uses: (pending capacity, engine inbox capacity).
        fn capacities(&self) -> (usize, usize) {
            (self.st.midi.pending_len().1, self.st.mix.live_inbox_capacity())
        }
    }

    /// One long callback raises `L` (capped at twice the usual callback), and it comes back
    /// down once the long one has left the window.
    #[test]
    fn a_long_callback_raises_l_only_for_a_while() {
        let mut r = rig();
        for _ in 0..20 {
            r.callback();
        }
        let usual = r.latency();
        assert_eq!(usual, CB_NS + MIDI_SLACK_NS);
        r.callback_of(4096, 48_000.0);
        let raised = r.latency();
        assert!(raised > usual, "{raised} > {usual}");
        assert!(raised <= 2 * CB_NS + MIDI_SLACK_NS, "capped at twice the median: {raised}");
        for _ in 0..midi_in::WINDOW + 1 {
            r.callback();
        }
        assert_eq!(r.latency(), usual);
    }

    /// With a device rate other than the session's (callbacks of 300 frames at 44.1 kHz under a
    /// 48 kHz session: 326.5 session samples each, so audio is rendered ahead by a varying
    /// amount), events arriving once a callback land once every 326.5 samples on the timeline.
    #[test]
    fn offsets_stay_steady_when_device_and_session_rates_differ() {
        let mut r = rig_at(44_100.0);
        r.tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        for _ in 0..4 {
            r.callback_of(300, 44_100.0);
        }
        for k in 0..40u8 {
            let t = r.now - 3_000_000;
            r.send("Keys", note_on(20 + k), t);
            r.callback_of(300, 44_100.0);
        }
        r.callback_of(300, 44_100.0);
        let at: Vec<i64> = r.notes().iter().filter(|e| matches!(e.2, Ev::On(_))).map(|e| e.3).collect();
        assert_eq!(at.len(), 40);
        for w in at.windows(2) {
            let d = w[1] - w[0];
            assert!((325..=328).contains(&d), "steady spacing, got {d}: {at:?}");
        }
    }

    /// Stopped and idle, then a key: the note lands at its `L` offset in the first callback after
    /// idle (not a block late, not thrown away), and none of the audio rendered ahead before the
    /// stop plays.
    #[test]
    fn the_first_note_after_idle_plays_on_time_and_alone() {
        let mut r = rig();
        r.tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        // 1000-frame callbacks render 512-sample blocks: a callback ends inside one.
        for _ in 0..3 {
            r.callback_of(1000, 48_000.0);
        }
        assert!(r.st.pending_len as f64 > r.st.read, "rendered ahead when stopping");
        r.tx.send(Cmd::Stop).unwrap();
        for _ in 0..3 {
            r.callback();
        }
        let before = r.notes().len();
        let t = r.now - 4_000_000;
        r.send("Keys", note_on(60), t);
        let now = r.now;
        let mut out = vec![0.0f32; CB * 2];
        r.st.fill_at(&mut out, 2, now);
        let l = r.latency();
        let got = r.notes();
        assert_eq!(got.len(), before + 1, "played in the first callback after idle: {got:?}");
        let at = ((t + l) as i64 - now as i64) * 48 / 1_000_000;
        assert!((got[before].1 as i64 - at).abs() <= 1, "offset {} for {at}", got[before].1);
        assert!(out.iter().all(|x| x.abs() < 1e-6), "no audio from before the stop");
    }

    /// A target alone doesn't keep the engine rendering: an instrument with no input rests,
    /// plays while it has notes, and rests again once they have rung out.
    #[test]
    fn a_target_with_nothing_to_play_renders_nothing() {
        let mut r = rig();
        for _ in 0..5 {
            r.callback();
        }
        assert_eq!(r.processed.load(Ordering::Relaxed), 0);
        r.tx.send(Cmd::MidiPlay(note_on(60))).unwrap();
        r.callback();
        r.tx.send(Cmd::MidiPlay(note_off(60))).unwrap();
        r.callback();
        assert_eq!(r.processed.load(Ordering::Relaxed), 2);
        // About five seconds of ring-out, then rest.
        for _ in 0..1000 {
            r.callback();
        }
        let rested = r.processed.load(Ordering::Relaxed);
        assert!(rested < 1000, "{rested}");
        for _ in 0..10 {
            r.callback();
        }
        assert_eq!(r.processed.load(Ordering::Relaxed), rested);
    }

    /// The live path's storage never grows on the audio thread: through a burst, a full pending
    /// buffer, a target change, a panic and a queue swap, every buffer keeps its capacity. (A
    /// counting global allocator would check more, but needs `unsafe`, which this workspace
    /// forbids; capacities are the check the mix and playback tests use.)
    #[test]
    fn the_live_path_never_grows_its_storage() {
        let mut r = rig();
        r.callback();
        let start = r.capacities();
        assert_eq!(start, (MIDI_PENDING, soundcraft_mix::LIVE_INBOX));
        let now = r.now;
        for i in 0..1000u32 {
            assert!(r.src.inject("Keys", MidiMessage::ControlChange { ch: 0, cc: 1, value: (i % 128) as u8 }, now - 4_000_000));
        }
        for _ in 0..1000u32 {
            assert!(r.src.inject("Pads", note_on(60), now + 1_000_000_000));
        }
        r.delivered(2000);
        r.callback();
        assert_eq!(r.capacities(), start, "burst and full pending buffer");
        r.tx.send(Cmd::MidiTarget(None)).unwrap();
        r.tx.send(Cmd::MidiTarget(Some(r.track))).unwrap();
        r.callback();
        r.tx.send(Cmd::MidiPanic).unwrap();
        r.callback();
        r.tx.send(Cmd::MidiIn(Some((r.src.subscribe(16).unwrap(), r.clock)))).unwrap();
        r.callback();
        assert_eq!(r.capacities(), start, "target change, panic, queue swap");
        assert!(r.grx.subs.try_recv().is_ok());
    }

    /// Play Selection runs to its end in the middle of an arrangement note on the target: the
    /// note stops there, and stopped blocks after it leave nothing sounding.
    #[test]
    fn a_note_crossing_the_end_of_play_stops_there() {
        let mut r = rig();
        r.callback();
        let mut s = (*r.st.session).clone();
        let cid = s.new_clip_id();
        let note = soundcraft_midi::Note { pitch: 70, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 4 * 960 };
        let clip = soundcraft_model::Clip::midi(cid, "arr", 0, 96_000, soundcraft_midi::Sequence { notes: vec![note], ctrls: Vec::new() });
        s.track_mut(r.track).unwrap().playlist_mut().unwrap().clips.push(clip);
        r.tx.send(session_cmd(Arc::new(s), Vec::new(), Vec::new())).unwrap();
        r.tx.send(Cmd::Play { from: 0, end: Some(10_000), looped: None }).unwrap();
        for _ in 0..60 {
            r.callback();
        }
        assert!(!r.shared.playing.load(Ordering::Relaxed));
        assert!(r.log().iter().any(|e| e.2 == Ev::On(70)), "{:?}", r.log());
        assert_eq!(r.sounding(), Vec::<u8>::new(), "{:?}", r.log());
    }

    // ---- capture for recording (see `capture`) ------------------------------------------------

    impl Rig {
        /// A capture queue of `n` events on the audio thread.
        fn capture(&mut self, n: usize) -> MidiCapture {
            let (tx, rx) = capture::queue(n);
            self.tx.send(Cmd::MidiCapture(Some(tx))).unwrap();
            rx
        }

        /// A new snapshot of the session `edit` changes, applied at once.
        fn edit(&mut self, edit: impl FnOnce(&mut Session)) {
            // Snapshots sent earlier land first.
            self.callback();
            let mut s = (*self.st.session).clone();
            edit(&mut s);
            self.tx.send(session_cmd(Arc::new(s), Vec::new(), Vec::new())).unwrap();
            self.callback();
        }

        fn arm(&mut self, on: bool) {
            let t = self.track;
            self.edit(|s| s.track_mut(t).unwrap().mixer.record_arm = on);
        }

        /// Sends `msg` from "Keys", due in the next callback.
        fn play_now(&mut self, msg: MidiMessage) {
            let t = self.now - 4_000_000;
            self.send("Keys", msg, t);
        }

        /// Where the instrument played `e` (timeline samples), in order.
        fn played(&self, e: Ev) -> Vec<i64> {
            self.log().into_iter().filter(|x| x.2 == e).map(|x| x.3).collect()
        }
    }

    /// A live event captured in the first pass.
    fn mid(at: i64, msg: MidiMessage) -> Captured {
        Captured { at, pass: 0, event: CaptureEvent::Midi(msg) }
    }

    fn drained(c: &mut MidiCapture) -> Vec<Captured> {
        let mut v = Vec::new();
        c.drain_into(&mut v);
        v
    }

    /// While recording onto the record-armed target, each live event (ports and on-screen
    /// keyboard) is captured at the timeline sample the instrument played it on; not before
    /// recording, nor on an unarmed target unless capturing while playing.
    #[test]
    fn live_events_on_an_armed_recording_target_are_captured_where_they_played() {
        let mut r = rig();
        let mut cap = r.capture(64);
        r.callback();
        r.tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        r.callback();
        r.play_now(note_on(59));
        r.callback();
        r.callback();
        assert!(!r.played(Ev::On(59)).is_empty());
        assert!(cap.is_empty(), "not recording");
        r.tx.send(Cmd::Recording(true)).unwrap();
        r.play_now(note_on(58));
        r.callback();
        r.callback();
        assert!(cap.is_empty(), "recording, but the target is not armed");
        r.arm(true);
        r.play_now(note_on(60));
        r.callback();
        r.play_now(note_off(60));
        r.callback();
        r.callback();
        r.tx.send(Cmd::MidiPlay(note_on(64))).unwrap();
        r.callback();
        let got = drained(&mut cap);
        let want =
            vec![mid(r.played(Ev::On(60))[0], note_on(60)), mid(r.played(Ev::Off(60))[0], note_off(60)), mid(r.played(Ev::On(64))[0], note_on(64))];
        assert_eq!(got, want);
        assert!(want[0].at > 0 && want[0].at < want[1].at, "{want:?}");
        // Session view slot recordings capture while playing, recording or not.
        r.tx.send(Cmd::Recording(false)).unwrap();
        r.arm(false);
        r.tx.send(Cmd::MidiCaptureWhilePlaying(true)).unwrap();
        r.play_now(note_on(65));
        r.callback();
        r.callback();
        assert_eq!(drained(&mut cap), [mid(r.played(Ev::On(65))[0], note_on(65))]);
        // Stopped: nothing plays on the timeline, nothing is captured.
        r.tx.send(Cmd::Stop).unwrap();
        r.callback();
        r.play_now(note_on(66));
        r.callback();
        r.callback();
        assert!(cap.is_empty());
    }

    /// A burst into a full capture queue drops (and counts) what doesn't fit, without growing
    /// anything; a replaced queue goes back off the audio thread.
    #[test]
    fn a_burst_into_a_full_capture_queue_is_dropped_and_counted() {
        let mut r = rig();
        let mut cap = r.capture(8);
        r.tx.send(Cmd::MidiCaptureWhilePlaying(true)).unwrap();
        r.tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        r.callback();
        let start = r.capacities();
        let now = r.now;
        for i in 0..40u8 {
            assert!(r.src.inject("Keys", MidiMessage::ControlChange { ch: 0, cc: 1, value: i }, now - 4_000_000));
        }
        r.delivered(40);
        r.callback();
        r.callback();
        assert_eq!(cap.len(), 8);
        assert_eq!(r.shared.midi.capture_dropped.load(Ordering::Relaxed), 32);
        assert_eq!(r.capacities(), start);
        let kept: Vec<u8> = drained(&mut cap)
            .iter()
            .map(|c| match c.msg() {
                Some(MidiMessage::ControlChange { value, .. }) => value,
                _ => 255,
            })
            .collect();
        assert_eq!(kept, (0..8).collect::<Vec<u8>>(), "the oldest are kept, in order");
        let _new = r.capture(8);
        r.callback();
        assert!(r.grx.captures.try_recv().is_ok(), "the old queue came back to be dropped");
    }
}
