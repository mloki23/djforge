//! Live MIDI input on the control side: the input devices, which of them are enabled (app
//! preferences, per machine), and which track live MIDI plays.
//!
//! The app owns a [`MidiHost`] over a [`MidiSource`] (`MidiInputs` on the desktop, a
//! `FakeSource` in tests). [`frame`] runs every UI frame and:
//! - hands each player a queue of the source's events (a new player, after an audio device
//!   change, gets a new one);
//! - once a second (not every frame: listing devices creates a backend client), refreshes the
//!   device list, forwarding `Closed` ports to the player so their notes are released, and
//!   opening enabled devices that appear;
//! - sends the player the live target ([`resolve_target`]), which only goes out when it changes.
//!
//! The `midi.*` commands live here as UI commands (like `ui.audio_device`): they need the
//! device list, the preferences and the player, which the app owns and the engine does not.

use crate::SoundApp;
use serde_json::{Value, json};
use soundcraft_midi_io::{MidiEvent, MidiMessage, MidiSource, PortChange, PortId, PortInfo, Subscriber};
use soundcraft_model::{Session, Track, TrackId, TrackKind};
use soundcraft_playback::Player;

/// Events the audio thread's queue holds between two callbacks.
pub const QUEUE_CAPACITY: usize = 4096;

/// Seconds between device-list refreshes.
pub const REFRESH_S: f64 = 1.0;

/// How long a port's activity light stays lit after a message (UI seconds).
pub const FLASH_S: f64 = 0.15;

/// How many recent messages the MIDI Devices window's monitor keeps.
pub const RECENT: usize = 8;

/// How often the waker thread looks at its queue while [`WAKE_URGENT`]: the longest a mapped
/// control waits for the UI to wake while nothing else repaints it.
pub const WAKE_POLL: std::time::Duration = std::time::Duration::from_millis(4);
/// How often it looks, and the repaint delay it asks for, while [`WAKE_DISPLAY`]: about 30 frames
/// a second for activity lights, however fast messages arrive.
pub const WAKE_DISPLAY_POLL: std::time::Duration = std::time::Duration::from_millis(33);
/// How often it looks while not wanted (it only empties its queue then).
pub const WAKE_IDLE: std::time::Duration = std::time::Duration::from_millis(250);

/// The waker's levels (see [`MidiHost::set_waker`]): no waking.
pub const WAKE_OFF: u8 = 0;
/// Waking for display only (activity lights): a repaint within [`WAKE_DISPLAY_POLL`].
pub const WAKE_DISPLAY: u8 = 1;
/// Waking at once (mappings, MIDI learn): a repaint now.
pub const WAKE_URGENT: u8 = 2;

/// A thread with its own queue of the source's events that wakes the UI (`wake`, given the
/// wanted level) when events arrive and `wanted` is above [`WAKE_OFF`]: egui sleeps between
/// frames while the transport is stopped, and MIDI input is no egui event. It sleeps between
/// looks ([`WAKE_POLL`] when urgent, [`WAKE_DISPLAY_POLL`] for display, else [`WAKE_IDLE`]); the
/// router does nothing extra for it.
struct Waker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Waker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            if t.join().is_err() {
                log::warn!("MIDI waker thread ended abnormally");
            }
        }
    }
}

/// An enabled MIDI input, as stored in the preferences: the port's identity (backend id and raw
/// name, see `soundcraft_midi_io::PortInfo`) and the name it was listed under when enabled.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MidiInputPref {
    pub name: String,
    pub raw_name: String,
    pub backend_id: String,
}

impl MidiInputPref {
    fn is(&self, raw_name: &str, backend_id: &str) -> bool {
        self.raw_name == raw_name && self.backend_id == backend_id
    }
}

/// Whether the port (raw name, backend id) is enabled in `prefs`. On ALSA a replugged device
/// gets a new backend id, so it is enabled again by hand.
pub fn enabled(prefs: &[MidiInputPref], raw_name: &str, backend_id: &str) -> bool {
    prefs.iter().any(|p| p.is(raw_name, backend_id))
}

/// The system's MIDI inputs (`None` on the web, or when the MIDI system can't start).
pub fn system_host() -> Option<MidiHost> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        match soundcraft_midi_io::MidiInputs::new(soundcraft_midi_io::MidiClock::new()) {
            Ok(m) => Some(MidiHost::new(Box::new(m))),
            Err(e) => {
                log::warn!("no MIDI input: {e}");
                None
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
}

/// The MIDI input devices and their upkeep (see the module docs).
pub struct MidiHost {
    source: Box<dyn MidiSource>,
    /// When the device list was last refreshed (UI time, seconds); `None` before the first frame.
    last_refresh: Option<f64>,
    /// The UI thread's own queue of every event, for MIDI learn and mapped controls
    /// (`crate::midi_learn`), drained every frame.
    control: Option<Subscriber>,
    /// The ports opened so far (their ids outlive a close), for the mapped-control filter.
    opened: Vec<PortId>,
    waker: Option<Waker>,
    /// When each port last sent a message (UI seconds), for the activity lights.
    activity: Vec<(PortId, f64)>,
    /// The last [`RECENT`] messages with their UI time, oldest first, for the monitor.
    recent: std::collections::VecDeque<(f64, MidiEvent)>,
    /// The router's hop delay p99 over the last refresh interval, in nanoseconds.
    hop_p99_ns: u64,
    /// When the hop delay was last sampled (UI seconds).
    last_hop: Option<f64>,
    /// Bumped whenever the device list or an open state changes, so a cached listing knows to
    /// list again.
    generation: u64,
    /// Why ports (raw name, backend id) last failed to open, until they open.
    open_errors: Vec<(String, String, String)>,
}

/// A port's state in the MIDI Devices window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    Open,
    Closed,
    /// Enabled, but its device is not listed (unplugged): it reopens when it appears.
    Disconnected,
}

impl PortState {
    pub fn as_str(self) -> &'static str {
        match self {
            PortState::Open => "open",
            PortState::Closed => "closed",
            PortState::Disconnected => "disconnected",
        }
    }
}

impl MidiHost {
    pub fn new(source: Box<dyn MidiSource>) -> MidiHost {
        MidiHost {
            source,
            last_refresh: None,
            control: None,
            opened: Vec::new(),
            waker: None,
            activity: Vec::new(),
            recent: std::collections::VecDeque::with_capacity(RECENT),
            hop_p99_ns: 0,
            last_hop: None,
            generation: 0,
            open_errors: Vec::new(),
        }
    }

    /// The events that arrived since the last call, oldest first (subscribing on the first call:
    /// nothing earlier is kept). Each lights its port's activity light at UI time `now` and goes
    /// into the monitor's recent messages.
    pub fn take_control_events(&mut self, now: f64) -> Vec<MidiEvent> {
        if self.control.is_none() {
            match self.source.subscribe(QUEUE_CAPACITY) {
                Ok(q) => self.control = Some(q),
                Err(e) => log::warn!("MIDI control input: {e}"),
            }
        }
        let mut out = Vec::new();
        while let Some(e) = self.control.as_mut().and_then(Subscriber::pop) {
            match self.activity.iter_mut().find(|(p, _)| *p == e.port) {
                Some(a) => a.1 = now,
                None => self.activity.push((e.port, now)),
            }
            if self.recent.len() >= RECENT {
                self.recent.pop_front();
            }
            self.recent.push_back((now, e));
            out.push(e);
        }
        out
    }

    /// When the port last sent a message (UI seconds), if it has.
    pub fn last_activity(&self, port: PortId) -> Option<f64> {
        self.activity.iter().find(|(p, _)| *p == port).map(|(_, at)| *at)
    }

    /// When any port last sent a message (UI seconds), if one has.
    pub fn any_activity(&self) -> Option<f64> {
        self.activity.iter().map(|(_, at)| *at).reduce(f64::max)
    }

    /// The last [`RECENT`] messages with their UI time, oldest first.
    pub fn recent(&self) -> impl Iterator<Item = &(f64, MidiEvent)> {
        self.recent.iter()
    }

    /// The id of the port (raw name, backend id), once it has been opened.
    pub fn port_id(&self, raw_name: &str, backend_id: &str) -> Option<PortId> {
        self.opened.iter().copied().find(|id| self.source.raw_name(*id) == Some(raw_name) && self.source.backend_id(*id) == Some(backend_id))
    }

    /// Bumped whenever the device list or an open state changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The router's hop delay p99 over the last refresh interval, in nanoseconds.
    pub fn hop_p99_ns(&self) -> u64 {
        self.hop_p99_ns
    }

    /// Events lost as (port, subscriber): the port figure counts events dropped before reaching
    /// any subscriber; the subscriber figure, events dropped by full subscriber queues (the
    /// player's, MIDI learn's and the waker's).
    pub fn dropped(&self) -> (u64, u64) {
        let port = self.control.as_ref().map_or(0, Subscriber::port_dropped);
        (port, self.source.dropped().saturating_sub(port))
    }

    /// The ports opened so far.
    pub fn opened(&self) -> &[PortId] {
        &self.opened
    }

    /// Whether [`MidiHost::set_waker`] has started a waker.
    pub fn has_waker(&self) -> bool {
        self.waker.is_some()
    }

    /// Calls `wake(level)` (from another thread) soon after MIDI events arrive while the `wanted`
    /// level is above [`WAKE_OFF`], replacing any earlier waker. False when its thread couldn't
    /// start.
    pub fn set_waker(&mut self, wanted: std::sync::Arc<std::sync::atomic::AtomicU8>, wake: Box<dyn Fn(u8) + Send>) -> bool {
        use std::sync::atomic::Ordering;
        self.waker = None;
        let mut sub = match self.source.subscribe(256) {
            Ok(q) => q,
            Err(e) => {
                log::warn!("MIDI waker: {e}");
                return false;
            }
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = std::sync::Arc::clone(&stop);
        let thread = std::thread::Builder::new().name("midi-ui-waker".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                std::thread::park_timeout(match wanted.load(Ordering::Relaxed) {
                    WAKE_OFF => WAKE_IDLE,
                    WAKE_DISPLAY => WAKE_DISPLAY_POLL,
                    _ => WAKE_POLL,
                });
                let mut any = false;
                while sub.pop().is_some() {
                    any = true;
                }
                let level = wanted.load(Ordering::Relaxed);
                if any && level != WAKE_OFF {
                    wake(level);
                }
            }
        });
        match thread {
            Ok(t) => {
                self.waker = Some(Waker { stop, thread: Some(t) });
                true
            }
            Err(e) => {
                log::warn!("MIDI waker: {e}");
                false
            }
        }
    }

    /// The source, for reading.
    pub fn source(&self) -> &dyn MidiSource {
        self.source.as_ref()
    }

    /// The source, for tests and the Setup window.
    pub fn source_mut(&mut self) -> &mut dyn MidiSource {
        self.source.as_mut()
    }

    /// One UI frame at `now` seconds (see the module docs). With `watch`, the device list is kept
    /// up even with nothing enabled (the MIDI Devices window is open). Returns status-line notes:
    /// a port disconnected, a port reopened.
    pub fn tick(&mut self, now: f64, prefs: &[MidiInputPref], player: Option<&Player>, watch: bool) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(p) = player.filter(|p| !p.has_midi_input()) {
            match self.source.subscribe(QUEUE_CAPACITY) {
                Ok(q) => p.set_midi_input(Some((q, self.source.clock()))),
                Err(e) => log::warn!("live MIDI input: {e}"),
            }
        }
        if self.last_hop.is_none_or(|at| now - at >= REFRESH_S || now < at) {
            // A recent window: the counters restart at each sample.
            self.hop_p99_ns = self.source.hop_stats(true).percentile_ns(0.99);
            self.last_hop = Some(now);
        }
        let Some(last) = self.last_refresh else {
            // The first frame: open the enabled devices present.
            self.last_refresh = Some(now);
            if !prefs.is_empty() {
                for port in self.source.list() {
                    if enabled(prefs, &port.raw_name, &port.backend_id)
                        && let Err(e) = self.open(&port)
                    {
                        notes.push(format!("{} won't open: {e}", port.name));
                    }
                }
            }
            return notes;
        };
        // Nothing enabled means nothing open (ports open only through the preferences): no
        // device list to keep up with, unless the window shows it.
        if (prefs.is_empty() && !watch) || (now - last < REFRESH_S && now >= last) {
            return notes;
        }
        self.last_refresh = Some(now);
        let changes = self.source.refresh();
        if changes.is_empty() {
            return notes;
        }
        self.generation = self.generation.wrapping_add(1);
        for change in changes {
            if let PortChange::Closed { port, name } = change {
                log::info!("MIDI input {name} went away");
                if let Some(p) = player {
                    p.midi_port_closed(port);
                }
                notes.push(format!("{name} disconnected – notes released"));
            }
        }
        // An enabled port listed but not open (it appeared, or its connection died while its
        // device stayed listed) opens again.
        if !prefs.is_empty() {
            for port in self.source.list() {
                if enabled(prefs, &port.raw_name, &port.backend_id) && !self.source.is_open(&port.name) {
                    notes.push(match self.open(&port) {
                        Ok(()) => format!("{} reconnected", port.name),
                        Err(e) => format!("{} won't open: {e}", port.name),
                    });
                }
            }
        }
        notes
    }

    /// Closes the port listed as `name` and opens it again: a device unplugged and replugged
    /// between two listings (which WinMM can miss) keeps a silent connection until then.
    /// Only an enabled port reconnects (`prefs`); its held notes are released on the close.
    pub fn reconnect(&mut self, name: &str, prefs: &[MidiInputPref], player: Option<&Player>) -> Result<Value, String> {
        let port = self.listed(name)?;
        if !enabled(prefs, &port.raw_name, &port.backend_id) {
            return Err(format!("{name} is not enabled: enable it to use it"));
        }
        let closed = self.close_releasing(&port, player);
        let r = self.source.open(name);
        self.generation = self.generation.wrapping_add(1);
        let id = self.opened_or_failed(&port, r)?;
        self.note_opened(id);
        Ok(json!({"name": name, "closed": closed, "open": self.source.is_open(name)}))
    }

    /// The port listed as `name`.
    fn listed(&self, name: &str) -> Result<PortInfo, String> {
        self.source.list().into_iter().find(|p| p.name == name).ok_or_else(|| format!("no MIDI input named \"{name}\""))
    }

    /// Closes a port and has the player release the notes it held; false if it was not open.
    fn close_releasing(&mut self, port: &PortInfo, player: Option<&Player>) -> bool {
        let id = self.port_id(&port.raw_name, &port.backend_id);
        let closed = self.source.close(&port.name);
        if closed && let (Some(p), Some(id)) = (player, id) {
            p.midi_port_closed(id);
        }
        closed
    }

    /// Records an open's outcome for the port's row: the error it gave, or none.
    fn opened_or_failed(&mut self, port: &PortInfo, r: Result<PortId, soundcraft_midi_io::MidiIoError>) -> Result<PortId, String> {
        self.open_errors.retain(|(raw, backend, _)| !(raw == &port.raw_name && backend == &port.backend_id));
        r.map_err(|e| {
            let e = e.to_string();
            self.open_errors.push((port.raw_name.clone(), port.backend_id.clone(), e.clone()));
            e
        })
    }

    /// Why the port (raw name, backend id) last failed to open, until it opens.
    pub fn open_error(&self, raw_name: &str, backend_id: &str) -> Option<&str> {
        self.open_errors.iter().find(|(raw, backend, _)| raw == raw_name && backend == backend_id).map(|(_, _, e)| e.as_str())
    }

    /// Opens a listed port; the error (also kept for its row) if it won't open.
    fn open(&mut self, port: &PortInfo) -> Result<(), String> {
        let r = self.source.open(&port.name);
        self.generation = self.generation.wrapping_add(1);
        match self.opened_or_failed(port, r) {
            Ok(id) => {
                self.note_opened(id);
                Ok(())
            }
            Err(e) => {
                log::warn!("{e}");
                Err(e)
            }
        }
    }

    fn note_opened(&mut self, id: PortId) {
        if !self.opened.contains(&id) {
            self.opened.push(id);
        }
    }

    /// The ports present, with whether each is open and enabled. Lists the devices (a backend
    /// client): for queries, not every frame.
    /// Enabled ports whose device is not listed come last, `disconnected`.
    pub fn inputs(&self, prefs: &[MidiInputPref]) -> Vec<Value> {
        let listed = self.source.list();
        let mut out: Vec<Value> = listed
            .iter()
            .map(|p| {
                let open = self.source.is_open(&p.name);
                json!({
                    "name": p.name,
                    "raw_name": p.raw_name,
                    "backend_id": p.backend_id,
                    "open": open,
                    "enabled": enabled(prefs, &p.raw_name, &p.backend_id),
                    "state": if open { PortState::Open } else { PortState::Closed }.as_str(),
                    "error": self.open_error(&p.raw_name, &p.backend_id),
                })
            })
            .collect();
        for p in prefs.iter().filter(|p| !listed.iter().any(|l| p.is(&l.raw_name, &l.backend_id))) {
            out.push(json!({
                "name": p.name,
                "raw_name": p.raw_name,
                "backend_id": p.backend_id,
                "open": false,
                "enabled": true,
                "state": PortState::Disconnected.as_str(),
            }));
        }
        out
    }

    /// Enables or disables the port listed as `name` in `prefs`, and opens or closes it (releasing
    /// the notes it held).
    pub fn set_enabled(&mut self, name: &str, on: bool, prefs: &mut Vec<MidiInputPref>, player: Option<&Player>) -> Result<Value, String> {
        let port = self.listed(name)?;
        prefs.retain(|p| !p.is(&port.raw_name, &port.backend_id));
        if on {
            prefs.push(MidiInputPref { name: port.name.clone(), raw_name: port.raw_name.clone(), backend_id: port.backend_id.clone() });
            // Enabled even if it won't open now: it opens when it next appears.
            self.open(&port)?;
        } else {
            self.close_releasing(&port, player);
            self.open_errors.retain(|(raw, backend, _)| !(raw == &port.raw_name && backend == &port.backend_id));
            self.generation = self.generation.wrapping_add(1);
        }
        Ok(json!({"name": port.name, "enabled": on, "open": self.source.is_open(&port.name)}))
    }
}

/// A track that plays live MIDI: an active instrument or MIDI track (its instrument plays the
/// notes; an inactive track doesn't render).
fn plays_midi(t: &Track) -> bool {
    matches!(t.kind, TrackKind::Instrument | TrackKind::Midi) && !t.inactive
}

/// The params of the `midi.*` UI commands, as `engine.commands` advertises them.
pub fn params_doc(id: &str) -> Option<&'static str> {
    Some(match id {
        "midi.inputs" | "midi.panic" => "{}",
        "midi.input_enable" => "{name, enabled: bool}",
        "midi.reconnect" => {
            "{name} — closes the listed input and opens it again (a device replugged between two listings can keep a silent connection)"
        }
        "midi.target" => "{track?: id | name | null}",
        "window.midi_devices" => "{value?: bool}",
        "midi.recover_take" => {
            "{} — places the last MIDI take again (an arrangement take, or a slot take that could not be written) on a new playlist of its track"
        }
        _ => return crate::midi_learn::params_doc(id),
    })
}

/// Why live MIDI plays the track it plays (see [`resolve_target`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetWhy {
    /// A slot recording plays its column's instrument.
    SlotRecording,
    /// The `midi.target` override.
    Override,
    /// The first record-armed instrument track.
    Armed,
    /// The first selected instrument track with input monitoring on.
    SelectedMonitored,
}

impl TargetWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetWhy::SlotRecording => "slot recording",
            TargetWhy::Override => "override",
            TargetWhy::Armed => "armed",
            TargetWhy::SelectedMonitored => "selected+monitored",
        }
    }
}

/// The target live MIDI plays now, for display: its name, or `None`.
pub fn target_name(app: &SoundApp) -> Option<String> {
    target(app).map(|(_, name, _)| name)
}

/// The target live MIDI plays now: its id, name and why it was chosen.
pub fn target(app: &SoundApp) -> Option<(TrackId, String, TargetWhy)> {
    let s = app.engine.session();
    let rec = crate::midi_record::target_override(app);
    let (id, why) = resolve_target_why(s, rec.or(app.midi_target))?;
    let why = if why == TargetWhy::Override && rec == Some(id) { TargetWhy::SlotRecording } else { why };
    s.track(id).map(|t| (id, t.name.clone(), why))
}

/// The track live MIDI input plays: the explicit override (`midi.target`) while it names an
/// instrument track; else the first record-armed instrument track in track order; else the
/// first selected instrument track whose input monitoring is on. `None`: live MIDI plays nothing.
pub fn resolve_target(s: &Session, override_: Option<TrackId>) -> Option<TrackId> {
    resolve_target_why(s, override_).map(|(t, _)| t)
}

/// [`resolve_target`], with why.
pub fn resolve_target_why(s: &Session, override_: Option<TrackId>) -> Option<(TrackId, TargetWhy)> {
    if let Some(t) = override_.and_then(|id| s.track(id)).filter(|t| plays_midi(t)) {
        return Some((t.id, TargetWhy::Override));
    }
    if let Some(t) = s.tracks.iter().find(|t| plays_midi(t) && t.mixer.record_arm) {
        return Some((t.id, TargetWhy::Armed));
    }
    s.edit
        .selected_tracks
        .iter()
        .filter_map(|id| s.track(*id))
        .find(|t| plays_midi(t) && t.mixer.input_monitor)
        .map(|t| (t.id, TargetWhy::SelectedMonitored))
}

/// The instrument tracks live MIDI can play (the override's choices).
pub fn target_choices(s: &Session) -> Vec<(TrackId, String)> {
    s.tracks.iter().filter(|t| plays_midi(t)).map(|t| (t.id, t.name.clone())).collect()
}

/// A message for the monitor: its type, its channel (1–16) and its data.
pub fn describe(msg: &MidiMessage) -> (&'static str, u8, String) {
    let ch = msg.channel().saturating_add(1);
    match *msg {
        MidiMessage::NoteOff { note, vel, .. } => ("Note Off", ch, format!("{note} vel {vel}")),
        MidiMessage::NoteOn { note, vel, .. } => ("Note On", ch, format!("{note} vel {vel}")),
        MidiMessage::PolyPressure { note, pressure, .. } => ("Poly Pressure", ch, format!("{note} {pressure}")),
        MidiMessage::ControlChange { cc, value, .. } => ("CC", ch, format!("{cc} = {value}")),
        MidiMessage::ProgramChange { program, .. } => ("Program", ch, program.to_string()),
        MidiMessage::ChannelPressure { pressure, .. } => ("Channel Pressure", ch, pressure.to_string()),
        MidiMessage::PitchBend { value, .. } => ("Pitch Bend", ch, value.to_string()),
    }
}

/// How long a light stays lit at this frame rate: [`FLASH_S`], or long enough to be drawn in the
/// next frame when frames are further apart.
pub fn flash_s(ctx: &egui::Context) -> f64 {
    FLASH_S.max(1.5 * f64::from(ctx.input(|i| i.predicted_dt)))
}

/// Whether the port (raw name, backend id) sent a message within `flash` seconds of `now`.
pub fn port_lit(app: &SoundApp, raw_name: &str, backend_id: &str, now: f64, flash: f64) -> bool {
    let Some(h) = &app.midi_host else { return false };
    h.port_id(raw_name, backend_id).and_then(|id| h.last_activity(id)).is_some_and(|at| (0.0..=flash).contains(&(now - at)))
}

/// Whether any port sent a message within `flash` seconds of `now`.
pub fn any_lit(app: &SoundApp, now: f64, flash: f64) -> bool {
    app.midi_host.as_ref().and_then(MidiHost::any_activity).is_some_and(|at| (0.0..=flash).contains(&(now - at)))
}

/// What MIDI Devices shows besides the port list, cheap enough for every frame: the latency L,
/// the hop p99, the dropped counts, the router's health, the target and the recent messages.
pub fn status(app: &SoundApp, now: f64) -> Value {
    let h = app.midi_host.as_ref();
    let (port_dropped, sub_dropped) = h.map_or((0, 0), MidiHost::dropped);
    let recent: Vec<Value> = h
        .map(|h| {
            h.recent()
                .map(|(at, e)| {
                    let (kind, ch, data) = describe(&e.msg);
                    let port = h.source().port_name(e.port).unwrap_or("?");
                    json!({"port": port, "type": kind, "ch": ch, "data": data, "age_s": (now - at).max(0.0)})
                })
                .collect()
        })
        .unwrap_or_default();
    let target = target(app).map(|(id, name, why)| json!({"track": id.0, "name": name, "why": why.as_str()}));
    json!({
        "available": h.is_some(),
        "healthy": h.is_none_or(|h| h.source().healthy()),
        "latency_ms": app.player.as_ref().map(|p| p.midi_latency_ns() as f64 / 1e6),
        "hop_p99_ms": h.map(|h| h.hop_p99_ns() as f64 / 1e6),
        "dropped_port": port_dropped,
        "dropped_subscribers": sub_dropped,
        "dropped_player": app.player.as_ref().map(Player::midi_dropped),
        "target": target,
        "override": app.midi_target.map(|t| t.0),
        "recent": recent,
    })
}

/// Every UI frame (see the module docs). `now` is the UI clock in seconds.
pub fn frame(app: &mut SoundApp, now: f64) {
    if let Some(h) = &mut app.midi_host {
        let notes = h.tick(now, &app.ui.midi_inputs, app.player.as_ref(), app.ui.show_midi_devices);
        if !notes.is_empty() {
            app.ui.status = notes.join("; ");
        }
    }
    if let Some(p) = &app.player {
        // A slot recording plays its column's instrument while armed.
        let over = crate::midi_record::target_override(app).or(app.midi_target);
        p.set_midi_target(resolve_target(app.engine.session(), over));
    }
}

/// The `midi.*` UI commands; `None` for other ids.
/// - `midi.inputs {}`: the ports (`name`, `raw_name`, `backend_id`, `open`, `enabled`, `state`)
///   and [`status`].
/// - `midi.input_enable {name, enabled}`: enables or disables a port in the preferences and opens
///   or closes it.
/// - `midi.reconnect {name}`: closes a port and opens it again.
/// - `midi.target {track?}`: the live target override (a track id or name; `null` or nothing
///   clears it). Returns the override and the target in effect.
/// - `midi.panic {}`: every live note off and the controllers reset.
/// - `setup.midi_input_devices` (the engine's deprecated command, which only kept names in the
///   session): with `enable` / `disable` names, enables or disables those inputs as
///   `midi.input_enable` does; otherwise opens MIDI Devices.
pub fn command(app: &mut SoundApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    Some(match id {
        "midi.inputs" => {
            let ports = app.midi_host.as_ref().map(|h| h.inputs(&app.ui.midi_inputs)).unwrap_or_default();
            let mut v = status(app, app.midi_learn.now);
            v["ports"] = Value::Array(ports);
            // The player's figure, as before.
            v["dropped"] = v["dropped_player"].clone();
            Ok(v)
        }
        "midi.input_enable" => (|| {
            let name = p.get("name").and_then(Value::as_str).ok_or("`name` required")?;
            let on = p.get("enabled").and_then(Value::as_bool).ok_or("`enabled` (true or false) required")?;
            let host = app.midi_host.as_mut().ok_or("no MIDI input on this system")?;
            let r = host.set_enabled(name, on, &mut app.ui.midi_inputs, app.player.as_ref());
            // Saved straight away, like the recent sessions.
            app.mark_prefs_changed();
            r
        })(),
        "midi.reconnect" => (|| {
            let name = p.get("name").and_then(Value::as_str).ok_or("`name` required")?;
            let host = app.midi_host.as_mut().ok_or("no MIDI input on this system")?;
            let r = host.reconnect(name, &app.ui.midi_inputs, app.player.as_ref());
            app.ui.status = match &r {
                Ok(_) => format!("{name} reconnected"),
                Err(e) => format!("{name} won't reopen: {e}"),
            };
            r
        })(),
        "midi.target" => (|| {
            let s = app.engine.session();
            let track = match p.get("track") {
                None | Some(Value::Null) => None,
                Some(Value::String(n)) => Some(s.track_by_name(n).ok_or_else(|| format!("no track named \"{n}\""))?),
                Some(v) => {
                    let id = v.as_u64().ok_or("`track` is a track id or name")?;
                    Some(s.track(TrackId(id)).ok_or_else(|| format!("no track {id}"))?)
                }
            };
            if let Some(t) = track.filter(|t| !plays_midi(t)) {
                return Err(format!("\"{}\" is not an active instrument track", t.name));
            }
            app.midi_target = track.map(|t| t.id);
            let target = resolve_target(app.engine.session(), crate::midi_record::target_override(app).or(app.midi_target));
            if let Some(pl) = &app.player {
                pl.set_midi_target(target);
            }
            Ok(json!({"override": app.midi_target.map(|t| t.0), "target": target.map(|t| t.0)}))
        })(),
        "midi.recover_take" => crate::midi_record::recover(app),
        "midi.panic" => {
            if let Some(pl) = &app.player {
                pl.midi_panic();
            }
            app.ui.status = "MIDI panic: every note off, controllers reset".into();
            Ok(json!({}))
        }
        "window.midi_devices" => {
            app.ui.show_midi_devices = p.get("value").and_then(Value::as_bool).unwrap_or(!app.ui.show_midi_devices);
            Ok(json!({"value": app.ui.show_midi_devices}))
        }
        "setup.midi_input_devices" => legacy_input_devices(app, p),
        _ => return None,
    })
}

/// The deprecated `setup.midi_input_devices` with the app present (see [`command`]).
fn legacy_input_devices(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let names = |key: &str| -> Vec<String> {
        p.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).take(64).map(str::to_string).collect()).unwrap_or_default()
    };
    let (on, off) = (names("enable"), names("disable"));
    if on.is_empty() && off.is_empty() {
        app.ui.show_midi_devices = true;
        return Ok(json!({"deprecated": "use midi.input_enable", "window": "midi_devices"}));
    }
    let mut results = Vec::new();
    for (name, enabled) in on.iter().map(|n| (n, true)).chain(off.iter().map(|n| (n, false))) {
        let r = command(app, "midi.input_enable", &json!({"name": name, "enabled": enabled})).unwrap_or_else(|| Err("no midi.input_enable".into()));
        results.push(match r {
            Ok(v) => v,
            Err(e) => json!({"name": name, "error": e}),
        });
    }
    Ok(json!({"deprecated": "use midi.input_enable", "results": results}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_model::ChannelFormat;

    /// The waker calls back soon after an event arrives, and only while wanted.
    #[test]
    fn the_waker_wakes_the_ui_for_midi_while_wanted() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
        let mut src = soundcraft_midi_io::FakeSource::new(soundcraft_midi_io::MidiClock::new()).unwrap();
        src.plug("DDJ");
        let mut h = MidiHost::new(Box::new(src));
        h.source_mut().open("DDJ").unwrap();
        let wanted = Arc::new(AtomicU8::new(WAKE_URGENT));
        let woken = Arc::new(AtomicUsize::new(0));
        let level = Arc::new(AtomicU8::new(WAKE_OFF));
        let (w, l) = (Arc::clone(&woken), Arc::clone(&level));
        assert!(h.set_waker(
            Arc::clone(&wanted),
            Box::new(move |at| {
                l.store(at, Ordering::Relaxed);
                w.fetch_add(1, Ordering::Relaxed);
            })
        ));
        let cc = soundcraft_midi_io::MidiMessage::ControlChange { ch: 0, cc: 7, value: 1 };
        let wait_wake = |what: &str| {
            let start = std::time::Instant::now();
            while woken.load(Ordering::Relaxed) == 0 {
                assert!(start.elapsed() < std::time::Duration::from_secs(2), "never woken: {what}");
                std::thread::yield_now();
            }
        };
        assert!(h.source_mut().as_fake_mut().unwrap().inject("DDJ", cc, 0));
        wait_wake("urgent");
        assert_eq!(level.load(Ordering::Relaxed), WAKE_URGENT, "urgent: a repaint now");
        // Display only: woken with that level (the UI then repaints at a capped rate).
        wanted.store(WAKE_DISPLAY, Ordering::Relaxed);
        std::thread::sleep(WAKE_POLL * 3);
        woken.store(0, Ordering::Relaxed);
        assert!(h.source_mut().as_fake_mut().unwrap().inject("DDJ", cc, 0));
        wait_wake("display");
        assert_eq!(level.load(Ordering::Relaxed), WAKE_DISPLAY);
        wanted.store(WAKE_OFF, Ordering::Relaxed);
        std::thread::sleep(WAKE_IDLE + WAKE_DISPLAY_POLL * 2);
        let before = woken.load(Ordering::Relaxed);
        assert!(h.source_mut().as_fake_mut().unwrap().inject("DDJ", cc, 0));
        std::thread::sleep(WAKE_IDLE * 2);
        assert_eq!(woken.load(Ordering::Relaxed), before, "not wanted: no wake");
        drop(h);
    }

    #[test]
    fn the_override_then_the_armed_then_the_selected_monitored_track_is_the_target() {
        let mut s = Session::default();
        let audio = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let a = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let b = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        assert_eq!(resolve_target(&s, None), None);
        s.edit.selected_tracks = vec![audio, b];
        s.track_mut(b).unwrap().mixer.input_monitor = true;
        assert_eq!(resolve_target(&s, None), Some(b), "selected and monitored");
        s.track_mut(a).unwrap().mixer.record_arm = true;
        assert_eq!(resolve_target(&s, None), Some(a), "armed comes first");
        assert_eq!(resolve_target(&s, Some(b)), Some(b), "the override comes first");
        assert_eq!(resolve_target(&s, Some(audio)), Some(a), "an audio track is no target");
        s.track_mut(a).unwrap().inactive = true;
        assert_eq!(resolve_target(&s, None), Some(b), "an inactive track is no target");
    }
}
