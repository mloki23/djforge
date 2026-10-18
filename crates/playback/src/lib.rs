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

pub mod devices;
pub mod record;

pub use devices::AudioConfig;

use soundcraft_dsp::{Plugin, PluginEditor, PluginStateReader};
use soundcraft_mix::{MAX_CHANNELS, MixEngine, PreparedInstance, PreparedWarp, StripMeter, WarpPlayer, itu_stereo_fold, main_channels};
use soundcraft_model::{ClipContent, Session, TrackId};
use soundcraft_time::{Range, Samples};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

const BLOCK: usize = 512;

/// Room for replaced snapshots on their way back; `update_session` empties it before each send.
const OLD_SESSIONS: usize = 16;

/// The realtime audio thread marker lives in the mix engine, which carries it into the strips it
/// renders on worker threads; the cpal callbacks here set it.
pub use soundcraft_mix::{mark_audio_thread, on_audio_thread};

/// Meter values for the UI.
#[derive(Debug, Clone, Default)]
pub struct MeterSnapshot {
    pub tracks: HashMap<TrackId, StripMeter>,
    pub main: StripMeter,
}

/// Plugin states read from the live instances: `(track, slot or INSTRUMENT_SLOT, state)`.
pub type PluginStates = Vec<(TrackId, usize, Vec<u8>)>;

/// A parameter edit made in a plugin's own editor: `(track, slot, param id, value)`.
pub type EditorEdit = (TrackId, usize, String, f32);

enum Cmd {
    /// A new snapshot plus the third-party instances and warp players it needs (created on the
    /// UI thread).
    Session(Arc<Session>, Vec<PreparedInstance>, Vec<PreparedWarp>),
    /// More warp players for the current snapshot, built progressively from [`Player::idle`].
    Warps(Vec<PreparedWarp>),
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
}

struct Shared {
    position: AtomicI64,
    playing: AtomicBool,
    /// Speed × 1000.
    speed: AtomicU32,
    meters: Mutex<MeterSnapshot>,
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
}

/// Return channels for what the audio thread takes out of service, to be dropped on the UI thread.
struct Bin {
    plugins: Sender<Vec<Box<dyn Plugin>>>,
    warps: Sender<Vec<WarpPlayer>>,
    /// Emptied warp-player batches, so their buffers are freed off the audio thread too.
    batches: Sender<Vec<PreparedWarp>>,
}

/// The UI-thread ends of a [`Bin`].
struct BinRx {
    plugins: Receiver<Vec<Box<dyn Plugin>>>,
    warps: Receiver<Vec<WarpPlayer>>,
    batches: Receiver<Vec<PreparedWarp>>,
}

fn bin() -> (Bin, BinRx) {
    let (ptx, prx) = channel();
    let (wtx, wrx) = channel();
    let (btx, brx) = channel();
    (Bin { plugins: ptx, warps: wtx, batches: btx }, BinRx { plugins: prx, warps: wrx, batches: brx })
}

/// Most warp players built per [`Player::update_session`] or [`Player::idle`] call, so loading
/// a big set doesn't stall the UI; the rest follow from later `idle` calls.
const WARP_BATCH: usize = 8;

/// Capacity of every batch of warp players sent: room for the engine to merge several batches
/// that arrive between two renders without allocating (see `MixEngine::adopt_warps`).
const WARP_BATCH_CAP: usize = 8 * WARP_BATCH;

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
        }
    }

    fn drain(&mut self) {
        let mut changed = false;
        while let Ok(c) = self.rx.try_recv() {
            match c {
                Cmd::Input(r) => self.input = Some(r),
                Cmd::Recording(r) => self.recording = r,
                Cmd::Session(s, instances, warps) => {
                    changed = true;
                    if (s.sample_rate.as_f64() - self.session.sample_rate.as_f64()).abs() > 0.5 {
                        let mut old = std::mem::replace(&mut self.mix, new_engine(s.sample_rate.as_f64() as f32));
                        let _ = self.garbage.plugins.send(old.drain_plugins());
                        let _ = self.garbage.warps.send(old.drain_warps());
                    }
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
                    let old = std::mem::replace(&mut self.session, s);
                    if let Some(tx) = &self.old_sessions {
                        let _ = tx.try_send(old);
                    }
                }
                Cmd::Warps(warps) => self.adopt_warps(warps),
                Cmd::OldSessions(tx) => self.old_sessions = Some(tx),
                Cmd::Play { from, end, looped } => {
                    self.pos = from;
                    self.end = end;
                    self.looped = looped.filter(|r| r.len() > 64);
                    self.playing = true;
                    self.pending_len = 0;
                    self.read = 0.0;
                    self.mix.reset();
                }
                Cmd::Stop => {
                    self.playing = false;
                    self.end = None;
                    self.looped = None;
                    self.mix.reset();
                }
                Cmd::Locate(at) => {
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
            self.mix.retire_stale_warps(&self.session);
        }
        self.send_garbage();
        self.shared.playing.store(self.playing, Ordering::Relaxed);
    }

    /// Hands warp players to the engine; the emptied batch goes back to be freed off this thread.
    fn adopt_warps(&mut self, warps: Vec<PreparedWarp>) {
        let spent = self.mix.adopt_warps(warps);
        if spent.capacity() > 0 {
            let _ = self.garbage.batches.send(spent);
        }
    }

    fn send_garbage(&mut self) {
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
            self.pos = l.start;
        }
        if let Some(l) = self.looped {
            n = n.min(usize::try_from((l.end - self.pos).max(1)).unwrap_or(BLOCK));
        } else if let Some(e) = self.end {
            if self.pos >= e {
                self.playing = false;
                self.end = None;
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
        self.mix.render(&self.session, self.pos, n, &mut self.pending);
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
        self.drain();
        let channels = channels.max(1);
        let monitoring = self.input.is_some() && self.session.tracks.iter().any(|t| t.mixer.input_monitor || t.mixer.record_arm);
        if !self.playing && !monitoring {
            out.iter_mut().for_each(|x| *x = 0.0);
            if let Ok(mut m) = self.shared.meters.try_lock() {
                m.main = StripMeter::default();
                m.tracks.clear();
            }
            return;
        }
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
    /// Beat-locked clips whose warp player was sent: (track, clip id) -> (channels, sample rate).
    prepared_warps: HashMap<(TrackId, u64), (usize, u32)>,
    /// The snapshot last sent, while some of its warp players are still to be built.
    warps_for: Option<Arc<Session>>,
}

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
            warps_for: None,
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
        if let Some(rx) = &self.old_sessions {
            rx.try_iter().for_each(drop);
        }
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
        drop(side);
        let _ = self.tx.send(Cmd::Session(s, instances, warps));
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
            let warps = side.prepare_warps(&s, self.position(), WARP_BATCH);
            if !warps.is_empty() {
                let _ = self.tx.send(Cmd::Warps(warps));
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
        self.shared.position.store(from, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Play { from, end, looped });
    }

    /// Feed live input (from a `record::Recorder`) into the mix for input monitoring.
    pub fn set_input(&self, ring: Arc<record::InputRing>) {
        let _ = self.tx.send(Cmd::Input(ring));
    }

    pub fn set_recording(&self, on: bool) {
        let _ = self.tx.send(Cmd::Recording(on));
    }

    pub fn stop(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn locate(&self, at: Samples) {
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
    /// never builds a stretcher mid-mix: at most `budget`, nearest the playhead first. When all
    /// are built, `warps_for` is cleared. Only clips the engine would play through a player are
    /// built ([`soundcraft_mix::warp_clip_plays`]). Players are kept per track, so a clip moved to
    /// another track gets a new one; a change of source channels or sample rate replaces it. A
    /// clip played before its player arrives builds one on the audio thread (the fallback).
    fn prepare_warps(&mut self, s: &Session, playhead: Samples, budget: usize) -> Vec<PreparedWarp> {
        let sr = s.sample_rate.hz();
        let mut live = HashSet::new();
        let mut todo = Vec::new();
        for t in &s.tracks {
            for c in t.clips().iter().filter(|c| soundcraft_mix::warp_clip_plays(t, c)) {
                let ClipContent::Audio { source, .. } = &c.content else { continue };
                let ch = s.pool.get(*source).map_or(0, |a| a.buffer.channels.len());
                if ch == 0 {
                    continue;
                }
                let key = (t.id, c.id.0);
                live.insert(key);
                if self.prepared_warps.get(&key) != Some(&(ch, sr)) {
                    let r = c.range();
                    let distance = if r.contains(playhead) {
                        0
                    } else {
                        r.start.saturating_sub(playhead).saturating_abs().min(r.end.saturating_sub(playhead).saturating_abs())
                    };
                    todo.push((distance, t.id, c.id, ch));
                }
            }
        }
        self.prepared_warps.retain(|k, _| live.contains(k));
        todo.sort_unstable_by_key(|x| x.0);
        if todo.len() <= budget {
            self.warps_for = None;
        }
        let mut out = Vec::with_capacity(WARP_BATCH_CAP);
        for (_, track, clip, ch) in todo.into_iter().take(budget) {
            // Remembered even when the build fails: retrying every frame would not help.
            self.prepared_warps.insert((track, clip.0), (ch, sr));
            out.extend(PreparedWarp::new(track, clip, ch, s.sample_rate.as_f64() as f32));
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
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, Arc::clone(&shared), Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let plugin = soundcraft_dsp::create("gain").unwrap();
        tx.send(Cmd::Session(Arc::clone(&s), vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin }], Vec::new()))
            .unwrap();
        let mut out = vec![0.0f32; 256];
        st.fill(&mut out, 2);
        assert!(grx.plugins.try_recv().is_err(), "nothing retired yet");
        // A session without the insert retires the instance to the return channel.
        tx.send(Cmd::Session(Arc::new(soundcraft_model::Session::default()), Vec::new(), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.plugins.try_recv().unwrap().len(), 1);
        // A sample-rate change rebuilds the engine without dropping plugins on this thread.
        tx.send(Cmd::Session(
            Arc::clone(&s),
            vec![PreparedInstance { track: t, slot: 1, id: "clap:test.fake".into(), ch: 2, plugin: soundcraft_dsp::create("gain").unwrap() }],
            Vec::new(),
        ))
        .unwrap();
        st.fill(&mut out, 2);
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(Cmd::Session(Arc::new(s44), Vec::new(), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.plugins.try_recv().unwrap().len(), 1);
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
        });
        let (tx, rx) = channel();
        let (gtx, grx) = bin();
        let mut st = AudioState::new(rx, shared, Arc::new(soundcraft_model::Session::default()), 48_000.0, gtx);
        let mut side = Side::new(48_000.0, bin().1);
        let warps = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(warps.len(), 1);
        assert!(side.prepare_warps(&s, 0, WARP_BATCH).is_empty(), "built once per clip");
        tx.send(Cmd::Session(Arc::new(s.clone()), Vec::new(), warps)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 512];
        st.fill(&mut out, 2);
        assert!(grx.warps.try_recv().is_err(), "the clip's player is in use");
        // The clip moves to another track: a new player there, the old one comes back.
        let t2 = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let moved = s.track_mut(t).unwrap().playlist_mut().unwrap().clips.remove(0);
        s.track_mut(t2).unwrap().playlist_mut().unwrap().clips.push(moved);
        let warps = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(warps.len(), 1);
        tx.send(Cmd::Session(Arc::new(s), Vec::new(), warps)).unwrap();
        // `fill` drains commands (retiring stale players) before it sends the garbage, so the
        // retired player is already on the channel when it returns.
        st.fill(&mut out, 2);
        assert_eq!(grx.warps.try_recv().map(|w| w.len()).unwrap_or(0), 1);
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
        tx.send(Cmd::Session(Arc::clone(&s), Vec::new(), first)).unwrap();
        let rest = side.prepare_warps(&s, 0, WARP_BATCH);
        assert_eq!(rest.len(), 2);
        assert!(side.warps_for.is_none(), "all built");
        tx.send(Cmd::Warps(rest)).unwrap();
        tx.send(Cmd::Play { from: 0, end: None, looped: None }).unwrap();
        let mut out = vec![0.0f32; 512];
        st.fill(&mut out, 2);
        let players: Vec<_> = st.mix.warp_players().collect();
        assert_eq!(players.len(), n, "one per playable clip, none for the muted or unusable one");
        assert!(players.iter().all(|(_, _, p)| p.is_prepared()), "none built on the audio thread");
        // A sample-rate change replaces the engine; its players come back for dropping here.
        let mut s44 = (*s).clone();
        s44.sample_rate = soundcraft_time::SampleRate::new(44_100).unwrap();
        tx.send(Cmd::Session(Arc::new(s44), Vec::new(), Vec::new())).unwrap();
        st.fill(&mut out, 2);
        assert_eq!(grx.warps.try_iter().map(|w| w.len()).sum::<usize>(), n);
    }

    #[test]
    fn plugin_states_are_read_without_the_audio_thread() {
        let mut side = Side::new(48_000.0, bin().1);
        let (a, b) = (TrackId(1), TrackId(2));
        side.states.insert((b, 0), Box::new(|| Some(b"b0".to_vec())));
        side.states.insert((a, soundcraft_mix::INSTRUMENT_SLOT), Box::new(|| Some(b"synth".to_vec())));
        side.states.insert((a, 3), Box::new(|| Some(b"a3".to_vec())));
        side.states.insert((a, 4), Box::new(|| None));
        let want = vec![(a, 3, b"a3".to_vec()), (a, soundcraft_mix::INSTRUMENT_SLOT, b"synth".to_vec()), (b, 0, b"b0".to_vec())];
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
            tx.send(Cmd::Session(s, Vec::new(), Vec::new())).unwrap();
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
