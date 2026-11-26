//! MIDI learn: notes and controllers of the MIDI inputs mapped to the app's controls (Session
//! view slots, scenes and stops, mixer faders and pans, stem faders, instrument and insert
//! parameters, any command, and the Decks tab's deck, DJ mixer and library controls).
//!
//! **Deck controls** ([`Target::Deck`], [`Target::Mixer`], [`Target::Library`]) act as the same
//! control of a built-in controller layout does: the code is shared ([`crate::deck_controls`]).
//! CUE is the CDJ cue (press and release), a hot cue pad jumps to a set cue (its release ends
//! the preview) or sets an empty one, PLAY, SYNC, MASTER TEMPO and the headphone CUE toggle from
//! the state asked for last while the deck hasn't reported it (a quick double press sends on
//! then off), the jog's ticks add up and go once a frame (a nudge while the deck is heard, a
//! search while paused), pitch bend nudges while held, and the knobs and faders follow an
//! absolute controller (7-bit, or 14-bit with its LSB: [`Mapping::lsb`], from the device's layout,
//! or seen while learning right after its MSB at least twice and never two MSBs in a row; a
//! device that sends the LSB first is learned 7-bit; an MSB that comes 8 times with no LSB is
//! read 7-bit until one does), upside down with [`Mapping::invert`] (off by default: CC 0 is the
//! bottom, −range for the pitch), with soft takeover; the pitch's takeover is bypassed only while its
//! deck follows a master on the other deck. What a learned deck control holds (a CUE or hot cue
//! preview, a bend) is let go when learning starts, the device holding it goes and the app quits
//! ([`release_holds`]). Right-click a Decks tab control for Learn MIDI and Remove MIDI Mapping.
//! Deck mappings are kept with the others, in the preferences per device: performance setup,
//! not session content. A mapping this version can't read (a target a later version added) is
//! left out when the preferences load, so it is gone once they are saved again.
//!
//! **Model.** A [`Mapping`] binds a device ([`DeviceKey`]: backend id and raw name, as the
//! enabled inputs are kept, plus the name it was listed under) and a [`Matcher`] (a note or a
//! controller on a channel) to a [`Target`], with a [`Mode`]. Mappings are app preferences
//! (`UiState::midi_mappings`), per machine, not part of the session. A target's track is a
//! [`TrackRef`]: its id, and its name as a fallback ([`resolve`]): the id when that track still
//! has the name, else the track with the name. A mapping whose target is missing (no such track,
//! slot, parameter or command) is skipped with a status message.
//!
//! **Dispatch** ([`frame`], every UI frame). The [`crate::midi_input::MidiHost`] keeps a second
//! queue of every event for the UI thread. Each event from a mapped device and matcher acts:
//! - *Triggers* (slot launch, scene, stop column, stop all, command) act on a press: a note-on, or a
//!   controller crossing up through 64. They run the commands the Session view's buttons run
//!   (`live.launch`, `live.scene`, `live.stop`, `live.stop_all`), so launches are quantised.
//!   Toggle on a slot launches it, or stops its column when it is the slot playing; Momentary
//!   triggers on the press (release does nothing: slots have no gate). The other triggers act on
//!   every press in either mode.
//! - *Continuous targets* (volume, pan, stem fader, parameters) work in control travel 0..1: the
//!   fader's own taper for volume and stems (`fader_db_to_pos`, the stem faders' +6 dB top), the
//!   parameter's taper for parameters, -1..1 for pan. An absolute controller maps 0..127 onto the
//!   travel. With pickup (soft takeover, the default) it is ignored until it reaches or crosses
//!   the current value, so nothing jumps; once the value moves elsewhere (the mouse, an undo) it
//!   has to be picked up again. A relative controller adds its ticks × the mapping's
//!   sensitivity (travel per tick). A note (or a controller in Momentary or Toggle mode) is a
//!   button: Momentary sets "on" while held and "off" on release; Toggle flips between them on
//!   each press ("on": 0 dB for volume and stems, the top for pan and parameters; "off": −inf, the
//!   bottom). Moves go through `Engine::execute_merged` under one key for every MIDI control
//!   ([`MIDI_GESTURE`]), and the gesture ends [`GESTURE_S`] after the last event: one knob turn,
//!   or several knobs turned together, is one undo step. A message that changes nothing (a
//!   jittery pot) edits nothing and doesn't hold the gesture open. While a track is in
//!   Touch/Latch automation and playing, a MIDI move of its volume, pan or a stem fader counts as
//!   touching that fader for [`GESTURE_S`], so the pass writes its lane as a mouse drag would.
//! - The same message from another device does nothing.
//!
//! **Waking.** egui sleeps between frames while nothing changes, and MIDI is no egui event: the
//! host's waker thread (`MidiHost::set_waker`, its own queue) repaints the UI as soon as MIDI
//! arrives while there are mappings or a learn is under way.
//!
//! **Coexistence with live play.** A mapped note or controller is a control, not music: while
//! it is mapped it doesn't play the live MIDI target. The player's audio thread drops it
//! (`Player::set_midi_controls`, a wait-free filter of per-port bit sets kept in step with the
//! mappings here); note-offs always pass, so a key held while it gets mapped is released. Unmapped
//! notes of the same device still play, so a keyboard can have a few keys as pads.
//!
//! **Learn.** Right-click a control and choose Learn MIDI (or `midi.learn`): the next note-on or
//! controller from any enabled input binds it, and a small overlay says "Move a control…", then
//! "Mapped to <device> CC 7". Esc cancels. A controller learned for a continuous target is
//! watched for [`DETECT_S`] (or [`DETECT_MSGS`] messages) to tell an encoder from a knob
//! ([`classify`]); those messages move nothing, and the overlay then offers the detected mode
//! as a selector, so a wrong guess is one click. Mapping a switch controller (CC 64..69, the
//! sustain pedal and its kin) sends it 0 on the live target, so a pedal held then can't stick. Learning a target again replaces its mapping; binding a
//! message another target already uses asks whether to replace that mapping or keep both.
//!
//! **Removing.** Delete, Remove All (after a confirmation) and `midi.unmap` keep what they
//! removed in the preferences (`midi_mappings_backup`, one level): Undo Remove in the MIDI
//! Mappings window, or `midi.restore_mappings`, puts it back.
//!
//! **Commands** (UI commands, see [`command`]): `midi.mappings`, `midi.map`, `midi.unmap`,
//! `midi.restore_mappings`, `midi.learn`, `midi.learn_command` and `window.midi_mappings`.

use crate::SoundApp;
use crate::controller_layouts::{Action, Ctx};
use crate::deck_controls::{Button, Continuous, DeckControl, DeckControls, LibraryControl, MixerControl, Takeover};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use soundcraft_midi_io::{MidiEvent, MidiMessage, PortId};
use soundcraft_mix::DeckId;
use soundcraft_model::{STEMS, Session, Track, TrackId};
use soundcraft_playback::MidiControl;
use std::collections::HashMap;

/// Seconds after a continuous control's last event that its gesture (undo step) ends.
pub const GESTURE_S: f64 = 0.5;
/// Travel per tick of a relative controller, until changed.
pub const DEFAULT_SENSITIVITY: f32 = 0.01;
/// An absolute controller this close to the value (in travel) picks it up.
const PICKUP_WINDOW: f32 = 1.5 / 127.0;
/// Seconds the "Mapped to …" overlay stays.
const OVERLAY_S: f64 = 2.5;
/// Seconds a mapping's activity light stays lit.
const ACTIVITY_S: f64 = 0.3;
/// The merge key every continuous MIDI move takes: moves of several controls within one
/// gesture are one undo step.
pub const MIDI_GESTURE: &str = "midi";
/// A newly learned controller is watched this long (or for [`DETECT_MSGS`] messages) to tell a
/// relative encoder from an absolute knob ([`classify`]).
pub const DETECT_S: f64 = 0.3;
pub const DETECT_MSGS: usize = 4;
/// The range of a relative controller's sensitivity (travel per tick).
pub const MIN_SENSITIVITY: f32 = 0.001;
pub const MAX_SENSITIVITY: f32 = 0.25;
/// Controllers whose value 0 releases something held (sustain, portamento, sostenuto, soft
/// pedal, legato, hold 2): reset on the live target when mapped, so a held pedal can't stick.
const SWITCH_CCS: std::ops::RangeInclusive<u8> = 64..=69;

/// A MIDI input device as a mapping names it: its identity (backend id and raw name, see
/// `soundcraft_midi_io::PortInfo`) and the name it was listed under.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceKey {
    pub backend_id: String,
    pub raw_name: String,
    pub name: String,
}

impl DeviceKey {
    /// The same device (the listed name doesn't count).
    pub fn same(&self, other: &DeviceKey) -> bool {
        self.backend_id == other.backend_id && self.raw_name == other.raw_name
    }

    fn is(&self, raw_name: &str, backend_id: &str) -> bool {
        self.raw_name == raw_name && self.backend_id == backend_id
    }
}

/// The message a mapping answers to. Channels are 0..=15 (shown 1..=16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Matcher {
    Note { ch: u8, note: u8 },
    Cc { ch: u8, cc: u8 },
}

/// What one event says to a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A note-on (true) or note-off.
    Note(bool),
    /// A controller's value.
    Cc(u8),
}

impl Matcher {
    /// The matcher and input of `msg` (`None`: not a note or controller).
    pub fn of(msg: &MidiMessage) -> Option<(Matcher, Input)> {
        Some(match *msg {
            MidiMessage::NoteOn { ch, note, .. } => (Matcher::Note { ch, note }, Input::Note(true)),
            MidiMessage::NoteOff { ch, note, .. } => (Matcher::Note { ch, note }, Input::Note(false)),
            MidiMessage::ControlChange { ch, cc, value } => (Matcher::Cc { ch, cc }, Input::Cc(value)),
            _ => return None,
        })
    }

    /// "CC 7", "Note 36 ch 10".
    pub fn label(&self) -> String {
        let (what, ch) = match *self {
            Matcher::Note { ch, note } => (format!("Note {note}"), ch),
            Matcher::Cc { ch, cc } => (format!("CC {cc}"), ch),
        };
        if ch == 0 { what } else { format!("{what} ch {}", u16::from(ch) + 1) }
    }

    fn control(self) -> MidiControl {
        match self {
            Matcher::Note { ch, note } => MidiControl::Note { ch, note },
            Matcher::Cc { ch, cc } => MidiControl::Cc { ch, cc },
        }
    }

    fn is_note(self) -> bool {
        matches!(self, Matcher::Note { .. })
    }

    /// Channel 0..=15 and note or controller 0..=127.
    pub fn valid(&self) -> bool {
        match *self {
            Matcher::Note { ch, note: n } | Matcher::Cc { ch, cc: n } => ch <= 15 && n <= 127,
        }
    }
}

/// How a mapping reads its messages (see the module docs). Notes are Momentary or Toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Momentary,
    Toggle,
    /// 0..127 over the control's travel.
    #[default]
    Absolute,
    /// Relative, two's complement: 1..63 up, 65..127 down (127 is −1).
    RelTwos,
    /// Relative around 64: 65 is +1, 63 is −1.
    RelOffset,
    /// Relative, sign and magnitude: bit 6 set is down (65 is −1), clear is up.
    RelSignMag,
}

impl Mode {
    pub const ALL: [Mode; 6] = [Mode::Momentary, Mode::Toggle, Mode::Absolute, Mode::RelTwos, Mode::RelOffset, Mode::RelSignMag];

    pub fn id(self) -> &'static str {
        match self {
            Mode::Momentary => "momentary",
            Mode::Toggle => "toggle",
            Mode::Absolute => "absolute",
            Mode::RelTwos => "rel_twos",
            Mode::RelOffset => "rel_offset",
            Mode::RelSignMag => "rel_sign_mag",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Momentary => "Momentary",
            Mode::Toggle => "Toggle",
            Mode::Absolute => "Absolute",
            Mode::RelTwos => "Relative (two's complement)",
            Mode::RelOffset => "Relative (64 = centre)",
            Mode::RelSignMag => "Relative (sign bit)",
        }
    }

    pub fn relative(self) -> bool {
        matches!(self, Mode::RelTwos | Mode::RelOffset | Mode::RelSignMag)
    }

    /// Whether `matcher` can use this mode (a note is a button).
    pub fn fits(self, matcher: &Matcher) -> bool {
        !matcher.is_note() || matches!(self, Mode::Momentary | Mode::Toggle)
    }

    /// The signed ticks a relative controller's value `v` means (0 for the other modes).
    pub fn delta(self, v: u8) -> i32 {
        let v = i32::from(v & 0x7F);
        match self {
            Mode::RelTwos if v >= 64 => v - 128,
            Mode::RelTwos => v,
            Mode::RelOffset => v - 64,
            Mode::RelSignMag if v & 0x40 != 0 => -(v & 0x3F),
            Mode::RelSignMag => v & 0x3F,
            _ => 0,
        }
    }
}

/// A track a mapping acts on: its id, and its name as the fallback (see [`resolve`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRef {
    pub id: u64,
    pub name: String,
}

impl TrackRef {
    pub fn of(t: &Track) -> TrackRef {
        TrackRef { id: t.id.0, name: t.name.clone() }
    }
}

/// The track `r` means in `s`: the track with its id while that one has its name, else the
/// track with its name (a session where the ids differ); `None` when neither exists.
pub fn resolve(s: &Session, r: &TrackRef) -> Option<TrackId> {
    if let Some(t) = s.track(TrackId(r.id)).filter(|t| t.name == r.name) {
        return Some(t.id);
    }
    s.track_by_name(&r.name).map(|t| t.id)
}

/// What a mapping controls. Columns, slots and rows count from 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    SlotLaunch {
        column: usize,
        slot: usize,
    },
    Scene {
        row: usize,
    },
    StopColumn {
        column: usize,
    },
    StopAll,
    TrackVolume {
        track: TrackRef,
    },
    TrackPan {
        track: TrackRef,
    },
    /// `stem` is a stem's name ("vocals", "drums", "bass", "other").
    StemFader {
        track: TrackRef,
        stem: String,
    },
    InstrumentParam {
        track: TrackRef,
        param: String,
    },
    InsertParam {
        track: TrackRef,
        slot: usize,
        param: String,
    },
    /// Any command, with its parameters; runs on a press.
    Command {
        id: String,
        #[serde(default)]
        params: Value,
    },
    /// A control of deck A or B (its buttons, pitch, jog, pads, loops, and its mixer channel):
    /// as the built-in layouts drive it ([`crate::deck_controls`]).
    Deck {
        #[serde(with = "crate::deck_controls::deck_id")]
        deck: DeckId,
        #[serde(flatten)]
        control: DeckControl,
    },
    /// A DJ mixer control that isn't per deck: crossfader, master, headphones.
    Mixer {
        control: MixerControl,
    },
    /// The Decks tab's library browser.
    Library {
        control: LibraryControl,
    },
}

/// What a deck, mixer or library target is to its mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DeckTarget {
    Button(DeckId, Button),
    /// A beat jump of the Decks tab's size: back (false) or on.
    Jump(DeckId, bool),
    Continuous(DeckId, Continuous),
    Jog(DeckId),
    MasterCue,
    Browse,
    /// The library's previous (−1) or next row.
    Step(isize),
    LoadEmpty,
}

impl Target {
    /// A button (acts on presses, and a deck's CUE, pads and pitch bend on releases too) rather
    /// than a range.
    pub fn is_trigger(&self) -> bool {
        match self.deck_target() {
            Some(DeckTarget::Continuous(..) | DeckTarget::Jog(_) | DeckTarget::Browse) => false,
            Some(_) => true,
            None => matches!(
                self,
                Target::SlotLaunch { .. } | Target::Scene { .. } | Target::StopColumn { .. } | Target::StopAll | Target::Command { .. }
            ),
        }
    }

    /// A deck, mixer or library control (see [`crate::deck_controls`]).
    pub fn is_deck(&self) -> bool {
        matches!(self, Target::Deck { .. } | Target::Mixer { .. } | Target::Library { .. })
    }

    /// Moves over a range an absolute controller spans (not a button, the jog or the browse
    /// knob, which only step).
    pub fn ranged(&self) -> bool {
        !self.is_trigger() && !matches!(self.deck_target(), Some(DeckTarget::Jog(_) | DeckTarget::Browse))
    }

    fn deck_target(&self) -> Option<DeckTarget> {
        use crate::deck_controls::Kind;
        Some(match self {
            Target::Deck { deck, control } => match control.kind() {
                Kind::Button(b) => DeckTarget::Button(*deck, b),
                Kind::Continuous(c) => DeckTarget::Continuous(*deck, c),
                Kind::Jog => DeckTarget::Jog(*deck),
                Kind::Jump(on) => DeckTarget::Jump(*deck, on),
            },
            Target::Mixer { control } => match control.continuous() {
                Some(c) => DeckTarget::Continuous(DeckId::A, c),
                None => DeckTarget::MasterCue,
            },
            Target::Library { control } => match control {
                LibraryControl::Browse => DeckTarget::Browse,
                LibraryControl::Previous => DeckTarget::Step(-1),
                LibraryControl::Next => DeckTarget::Step(1),
                LibraryControl::Load => DeckTarget::LoadEmpty,
            },
            _ => return None,
        })
    }

    /// Whether its parameters are in range (a deck's hot cue 1..=8, a beat jump ±1..=64).
    fn valid(&self) -> bool {
        match self {
            Target::Deck { control, .. } => control.valid(),
            _ => true,
        }
    }

    /// "Live 1 slot 3", "Kick volume", "Command transport.play".
    pub fn label(&self) -> String {
        match self {
            Target::SlotLaunch { column, slot } => format!("Live {} slot {}", column + 1, slot + 1),
            Target::Scene { row } => format!("Scene {}", row + 1),
            Target::StopColumn { column } => format!("Stop Live {}", column + 1),
            Target::StopAll => "Stop All".into(),
            Target::TrackVolume { track } => format!("{} volume", track.name),
            Target::TrackPan { track } => format!("{} pan", track.name),
            Target::StemFader { track, stem } => format!("{} {stem} stem", track.name),
            Target::InstrumentParam { track, param } => format!("{} instrument {param}", track.name),
            Target::InsertParam { track, slot, param } => {
                format!("{} insert {} {param}", track.name, char::from(b'a'.saturating_add(u8::try_from(*slot).unwrap_or(25))))
            }
            Target::Command { id, .. } => format!("Command {id}"),
            Target::Deck { deck, control } => format!("Deck {} {}", deck.id().to_uppercase(), control.label()),
            Target::Mixer { control } => control.label().into(),
            Target::Library { control } => format!("Library {}", control.label()),
        }
    }
}

/// One MIDI mapping (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mapping {
    pub device: DeviceKey,
    pub matcher: Matcher,
    pub target: Target,
    pub mode: Mode,
    /// Travel per tick of a relative controller.
    pub sensitivity: f32,
    /// Soft takeover for an absolute controller on a continuous target.
    pub pickup: bool,
    /// The LSB controller of a 14-bit control whose MSB is `matcher` (a deck or mixer control).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsb: Option<u8>,
    /// An absolute controller reads upside down (its top is the target's bottom): a pitch fader
    /// whose top sends 127 then has its top slower, as on a CDJ and on screen.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
}

impl Default for Mapping {
    fn default() -> Self {
        Mapping {
            device: DeviceKey::default(),
            matcher: Matcher::Cc { ch: 0, cc: 0 },
            target: Target::StopAll,
            mode: Mode::Absolute,
            sensitivity: DEFAULT_SENSITIVITY,
            pickup: true,
            lsb: None,
            invert: false,
        }
    }
}

impl Mapping {
    /// What identifies it for its activity and pickup state (its mode doesn't).
    fn key(&self) -> String {
        format!("{}|{}|{:?}|{}", self.device.backend_id, self.device.raw_name, self.matcher, serde_json::to_string(&self.target).unwrap_or_default())
    }

    /// Why its `lsb` can't be: `None` when it can (a deck or mixer knob or fader, absolute, on a
    /// controller other than its LSB).
    fn lsb_refused(&self) -> Option<&'static str> {
        let lsb = self.lsb?;
        let Matcher::Cc { cc, .. } = self.matcher else { return Some("`lsb` is for a controller, not a note") };
        if lsb > 127 {
            Some("`lsb` is a controller 0..=127")
        } else if lsb == cc {
            Some("`lsb` is another controller than the matcher's")
        } else if !(self.target.is_deck() && self.target.ranged()) {
            Some("`lsb` is for a deck or mixer knob or fader")
        } else if self.mode != Mode::Absolute {
            Some("`lsb` is for an absolute controller")
        } else {
            None
        }
    }

    fn uses_pickup(&self) -> bool {
        self.target.ranged() && !self.matcher.is_note() && self.mode == Mode::Absolute
    }

    /// Itself with its sensitivity in range and a mode its message can use (`None`: the message is
    /// out of range).
    pub fn normalised(mut self) -> Option<Mapping> {
        if !self.matcher.valid() || !self.target.valid() {
            return None;
        }
        if self.lsb.is_some() && self.lsb_refused().is_some() {
            self.lsb = None;
        }
        self.sensitivity = clamp_sensitivity(self.sensitivity);
        if !self.mode.fits(&self.matcher) {
            self.mode = default_mode(&self.matcher, &self.target);
        }
        Some(self)
    }

    fn json(&self) -> Value {
        json!({
            "device": self.device,
            "matcher": self.matcher,
            "message": self.matcher.label(),
            "target": self.target,
            "label": self.target.label(),
            "mode": self.mode.id(),
            "sensitivity": self.sensitivity,
            "pickup": self.pickup,
            "lsb": self.lsb,
            "invert": self.invert,
        })
    }
}

/// `s` within [`MIN_SENSITIVITY`]..=[`MAX_SENSITIVITY`] (the default when not a number).
pub fn clamp_sensitivity(s: f32) -> f32 {
    if s.is_finite() { s.clamp(MIN_SENSITIVITY, MAX_SENSITIVITY) } else { DEFAULT_SENSITIVITY }
}

/// The mode a controller's first messages after learning suggest (see [`DETECT_S`]): repeated
/// small steps around 0 (1..8 up, 120..127 down) are a two's-complement encoder, repeated values
/// around 64 (57..71) an offset encoder, repeated 1..8 and 65..72 a sign-bit encoder, and only
/// tiny values (1..3) an encoder ticking up; anything else (distinct values moving one way, say)
/// is an absolute knob or fader.
pub fn classify(values: &[u8]) -> Mode {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    let repeats = distinct.len() < values.len();
    let all = |f: &dyn Fn(u8) -> bool| !values.is_empty() && values.iter().all(|v| f(*v));
    if repeats && all(&|v| (1..=8).contains(&v) || (120..=127).contains(&v)) {
        Mode::RelTwos
    } else if repeats && all(&|v| (57..=71).contains(&v)) {
        Mode::RelOffset
    } else if repeats && all(&|v| (1..=8).contains(&v) || (65..=72).contains(&v)) {
        Mode::RelSignMag
    } else if all(&|v| (1..=3).contains(&v)) {
        Mode::RelTwos
    } else {
        Mode::Absolute
    }
}

/// The mode a newly learned mapping gets: buttons for triggers and notes, absolute for a
/// controller on a continuous target.
pub fn default_mode(matcher: &Matcher, target: &Target) -> Mode {
    match (matcher.is_note(), target.is_trigger()) {
        (_, true) => Mode::Momentary,
        (true, false) => Mode::Toggle,
        (false, false) => Mode::Absolute,
    }
}

/// Reads the preferences' mappings, skipping (rather than failing on) any it can't read.
pub fn lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Mapping>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(|m| serde_json::from_value::<Mapping>(m.clone()).ok().and_then(Mapping::normalised)).collect())
        .unwrap_or_default())
}

/// A learned message another target already uses: replace that mapping, or keep both.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub mapping: Mapping,
    /// The target already mapped to it.
    pub other: String,
    /// The learned controller's value (for encoder detection), `None` for a note or a trigger.
    pub first: Option<u8>,
}

/// A newly learned controller being watched to tell its kind ([`classify`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Detect {
    /// The mapping's key.
    pub key: String,
    pub device: DeviceKey,
    pub matcher: Matcher,
    pub values: Vec<u8>,
    /// When it was learned (UI time).
    pub started: f64,
    /// The device's last message was this controller (its MSB): an LSB now is its other half.
    pub msb_last: bool,
    /// MSB-then-LSB pairs seen: [`LSB_PAIRS`] make it 14-bit.
    pub pairs: u8,
    /// An MSB came right after another (no LSB between): not a 14-bit control.
    pub lone: bool,
}

/// MSB-then-LSB pairs a learned controller must send while it is watched to be taken as 14-bit,
/// with never two MSBs in a row (a separate control on controller + 32, a button say, doesn't
/// follow every move).
pub const LSB_PAIRS: u8 = 2;
/// MSBs in a row with no LSB after which a 14-bit mapping reads its MSB as 7-bit (until an LSB
/// shows up again): a wrong LSB never leaves a knob dead.
const LONE_MSBS: u8 = 8;

/// A mapping's state while running: pickup, the button's last state, activity.
#[derive(Debug, Clone, Default)]
struct Runtime {
    picked: bool,
    /// The travel last set (a different value now means it moved elsewhere).
    sent: Option<f32>,
    /// The controller's last position, while not picked up.
    last_in: Option<f32>,
    pressed: bool,
    /// The mode this state is for: a change of mode starts afresh.
    mode: Option<Mode>,
    /// When it last received something (UI time), and what.
    activity: Option<(f64, String)>,
    /// A 14-bit control's last MSB, waiting for its LSB, and the MSBs since that had none.
    msb: Option<u8>,
    lone_msbs: u8,
    /// A deck or mixer control's soft takeover.
    takeover: Takeover,
    /// The last value of an absolute controller on a stepping target (jog, browse).
    last_abs: Option<u8>,
}

/// MIDI learn's state (not saved).
#[derive(Debug, Default)]
pub struct LearnState {
    /// The control being learned.
    pub learning: Option<Target>,
    /// A learned message that asks Replace or Keep Both.
    pub conflict: Option<Conflict>,
    /// A message the overlay shows until a UI time.
    pub overlay: Option<(String, f64)>,
    /// The mapping (its key) whose mode the overlay offers, after learning a controller.
    pub overlay_mapping: Option<String>,
    /// A controller just learned, watched to detect an encoder.
    pub detect: Option<Detect>,
    /// The faders (volume, pan, stems) MIDI moved, and when (UI time): Touch/Latch automation
    /// counts them as touched for [`GESTURE_S`].
    pub(crate) touch: HashMap<(TrackId, soundcraft_model::AutoParam), f64>,
    /// How MIDI should wake the UI, read by the host's waker: [`crate::midi_input::WAKE_URGENT`]
    /// for mappings or learning, [`crate::midi_input::WAKE_DISPLAY`] for activity lights only,
    /// else [`crate::midi_input::WAKE_OFF`].
    pub wake_wanted: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Said once that mapped ports went beyond what the player filters.
    overflow_logged: bool,
    /// The waker couldn't start: try again from this UI time (the next device refresh).
    waker_retry_at: Option<f64>,
    /// The detected encoder's direction is a guess (all its ticks went one way): the overlay
    /// says to check it.
    pub check_direction: bool,
    runtime: HashMap<String, Runtime>,
    /// The merged gesture (its key) and the time of its last event.
    gesture: Option<(String, f64)>,
    /// The mapped-control filter last given to a player, and that player's id: a new player
    /// (an audio device change) starts with none, so it gets the filter again.
    controls_sent: Option<(u64, Vec<(PortId, MidiControl)>)>,
    /// The UI time of the last frame.
    pub(crate) now: f64,
    /// Remove All asks first: the question is up.
    pub confirm_remove_all: bool,
    /// What the learned deck controls remember (toggles asked for, held cues and pads, jog
    /// counts, the pitch's takeover), as a built-in layout does.
    pub(crate) decks: DeckControls,
    /// The learned deck controls held now (a CUE, a pad, a bend): their mapping (its key), device,
    /// deck and button. Its device going away, or the mapping going, lets go of it.
    holders: Vec<(String, DeviceKey, DeckId, Button)>,
    /// The UI time the deck controls last ticked.
    tick_at: Option<f64>,
    /// The deck actions learned mappings asked for (for tests).
    #[cfg(test)]
    pub(crate) actions: Vec<Action>,
}

/// Starts learning `target`: the next note-on or controller binds it.
pub fn start(app: &mut SoundApp, target: Target) {
    app.ui.status = format!("Learn MIDI for {}: move a control (Esc cancels)", target.label());
    // A controller's held CUE or pad would never hear its release while learning.
    crate::controller_layouts::release_holds(app);
    release_holds(app);
    app.midi_learn.learning = Some(target);
    app.midi_learn.conflict = None;
    app.midi_learn.overlay = None;
}

/// Stops learning, and drops a pending Replace or Keep Both question.
pub fn cancel(app: &mut SoundApp) {
    if app.midi_learn.learning.take().is_some() || app.midi_learn.conflict.take().is_some() {
        app.ui.status = "MIDI learn cancelled".into();
    }
}

/// The mappings of `target`.
pub fn mapped(app: &SoundApp, target: &Target) -> usize {
    app.ui.midi_mappings.iter().filter(|m| &m.target == target).count()
}

/// Adds `m`. A mapping of the same target goes (learning again replaces); with `replace`, so
/// does one of the same device and message.
pub fn commit(app: &mut SoundApp, m: Mapping, replace: bool) -> usize {
    app.ui.midi_mappings.retain(|o| o.target != m.target && !(replace && o.device.same(&m.device) && o.matcher == m.matcher));
    let line = format!("Mapped to {} {}", m.device.name, m.matcher.label());
    if let Matcher::Cc { ch, cc } = m.matcher
        && SWITCH_CCS.contains(&cc)
        && let Some(p) = &app.player
    {
        // Its messages stop reaching the live target now: release what it may be holding.
        p.play_midi(MidiMessage::ControlChange { ch, cc, value: 0 });
    }
    app.midi_learn.overlay_mapping = (!m.matcher.is_note() && !m.target.is_trigger()).then(|| m.key());
    app.ui.midi_mappings.push(m);
    app.midi_learn.overlay = Some((line.clone(), app.midi_learn.now + OVERLAY_S));
    app.ui.status = line;
    app.mark_prefs_changed();
    app.ui.midi_mappings.len() - 1
}

/// Binds the learned target to `matcher` of `device`, or asks first when another target uses it.
/// `twin`: the other half of the 14-bit control the device's layout says the message is.
fn bind(app: &mut SoundApp, device: DeviceKey, matcher: Matcher, input: Input, target: Target, twin: Option<MidiControl>) {
    let mode = default_mode(&matcher, &target);
    let mut m = Mapping { device, matcher, target, mode, ..Mapping::default() };
    let mut first = match input {
        Input::Cc(v) if !m.target.is_trigger() => Some(v),
        _ => None,
    };
    // A 14-bit knob or fader the layout knows, on a deck or mixer control: both halves, absolute.
    if m.target.is_deck()
        && m.target.ranged()
        && let (Matcher::Cc { ch, cc }, Some(MidiControl::Cc { ch: twin_ch, cc: other })) = (m.matcher, twin)
        && twin_ch == ch
    {
        if other < cc {
            m.matcher = Matcher::Cc { ch, cc: other };
        }
        m.lsb = Some(other.max(cc));
        first = None;
    }
    let other = app.ui.midi_mappings.iter().find(|o| o.device.same(&m.device) && o.matcher == m.matcher && o.target != m.target);
    if let Some(o) = other {
        app.midi_learn.conflict = Some(Conflict { other: o.target.label(), mapping: m, first });
        return;
    }
    learned(app, m, false, first);
}

/// Commits a learned mapping and, for a controller on a continuous target, starts telling an
/// encoder from a knob with its first value.
fn learned(app: &mut SoundApp, m: Mapping, replace: bool, first: Option<u8>) {
    let detect = first.map(|v| Detect {
        key: m.key(),
        device: m.device.clone(),
        matcher: m.matcher,
        values: vec![v],
        started: app.midi_learn.now,
        msb_last: true,
        pairs: 0,
        lone: false,
    });
    commit(app, m, replace);
    app.midi_learn.detect = detect;
}

/// Ends encoder detection: the mapping takes the mode its values suggest.
fn finish_detect(app: &mut SoundApp) {
    let Some(d) = app.midi_learn.detect.take() else { return };
    if d.lone
        && let Some(m) = app.ui.midi_mappings.iter_mut().find(|m| m.key() == d.key && m.lsb.is_some())
    {
        // Its MSB came twice without an LSB between: what followed it was another control.
        m.lsb = None;
    }
    // A 14-bit control's MSB repeats while its LSB moves: absolute, whatever it looks like.
    let wide = app.ui.midi_mappings.iter().any(|m| m.key() == d.key && m.lsb.is_some());
    let mode = if wide { Mode::Absolute } else { classify(&d.values) };
    app.midi_learn.check_direction = match mode {
        Mode::RelOffset => d.values.iter().all(|v| *v > 64),
        Mode::RelTwos => d.values.iter().all(|v| (1..=63).contains(v)),
        _ => false,
    };
    if let Some(m) = app.ui.midi_mappings.iter_mut().find(|m| m.key() == d.key)
        && m.mode != mode
    {
        m.mode = mode;
        if m.lsb_refused().is_some() {
            m.lsb = None;
        }
        app.mark_prefs_changed();
    }
    if let Some(m) = app.ui.midi_mappings.iter().find(|m| m.key() == d.key) {
        let line = format!("Mapped to {} {}", m.device.name, m.matcher.label());
        app.midi_learn.overlay = Some((line, app.midi_learn.now + OVERLAY_S));
        app.midi_learn.overlay_mapping = Some(d.key);
    }
}

/// Answers the pending question: `replace` the other mapping, or keep both.
pub fn resolve_conflict(app: &mut SoundApp, replace: bool) {
    if let Some(c) = app.midi_learn.conflict.take() {
        learned(app, c.mapping, replace, c.first);
    }
}

/// Removes mapping `i`, kept for Undo Remove.
pub fn remove(app: &mut SoundApp, i: usize) -> Option<Mapping> {
    (i < app.ui.midi_mappings.len()).then(|| {
        let m = app.ui.midi_mappings.remove(i);
        backup(app, vec![m.clone()]);
        m
    })
}

/// Removes the mappings `gone` says, keeping them for Undo Remove; how many went.
pub fn remove_where(app: &mut SoundApp, gone: impl Fn(&Mapping) -> bool) -> usize {
    let (out, kept): (Vec<Mapping>, Vec<Mapping>) = std::mem::take(&mut app.ui.midi_mappings).into_iter().partition(|m| gone(m));
    app.ui.midi_mappings = kept;
    let n = out.len();
    if n > 0 {
        backup(app, out);
    }
    n
}

/// Keeps `removed` as the one level Undo Remove restores (replacing the one before), saved with
/// the preferences.
fn backup(app: &mut SoundApp, removed: Vec<Mapping>) {
    app.ui.midi_mappings_backup = removed;
    app.mark_prefs_changed();
}

/// Undo Remove: the mappings removed last come back (a target mapped again since keeps its new
/// mapping and gets the old one too, as Keep Both would). How many came back.
pub fn restore(app: &mut SoundApp) -> usize {
    let back = std::mem::take(&mut app.ui.midi_mappings_backup);
    let mut n = 0;
    for m in back {
        if !app.ui.midi_mappings.contains(&m) {
            app.ui.midi_mappings.push(m);
            n += 1;
        }
    }
    app.mark_prefs_changed();
    n
}

/// Every UI frame at UI time `now`, after the MIDI host's: learns, dispatches, ends gestures and
/// keeps the player's mapped-control filter in step (see the module docs): a new player gets it
/// too.
pub fn frame(app: &mut SoundApp, now: f64) {
    app.midi_learn.now = now;
    let events = app.midi_host.as_mut().map(|h| h.take_control_events(now)).unwrap_or_default();
    for e in events {
        handle(app, &e, now);
    }
    deck_frame(app, now);
    if app.midi_learn.detect.as_ref().is_some_and(|d| now - d.started > DETECT_S) {
        finish_detect(app);
    }
    if let Some((key, at)) = &app.midi_learn.gesture
        && now - at > GESTURE_S
    {
        if app.engine.merge_key() == Some(key.as_str()) {
            app.engine.end_merge();
        }
        app.midi_learn.gesture = None;
    }
    app.midi_learn.touch.retain(|_, at| now - *at <= GESTURE_S);
    if !app.midi_learn.runtime.is_empty() {
        let keys: std::collections::HashSet<String> = app.ui.midi_mappings.iter().map(Mapping::key).collect();
        app.midi_learn.runtime.retain(|k, _| keys.contains(k));
    }
    use crate::midi_input::{WAKE_DISPLAY, WAKE_OFF, WAKE_URGENT};
    let wanted =
        if !app.ui.midi_mappings.is_empty() || app.midi_learn.learning.is_some() || app.midi_learn.detect.is_some() || app.controllers.active() {
            WAKE_URGENT
        } else if crate::midi_devices::lights_shown(app) {
            // Activity lights only: a capped repaint rate is enough.
            WAKE_DISPLAY
        } else {
            WAKE_OFF
        };
    app.midi_learn.wake_wanted.store(wanted, std::sync::atomic::Ordering::Relaxed);
    sync_controls(app);
}

/// Wakes the UI for MIDI (the host's waker) and for what MIDI learn waits on: a gesture to end,
/// encoder detection, a Touch/Latch touch to release.
pub fn wake_on_midi(app: &mut SoundApp, ctx: &egui::Context) {
    // Started once MIDI is first wanted; a failed start is tried again at the device refresh rate.
    let s = &mut app.midi_learn;
    let due = s.waker_retry_at.is_none_or(|at| s.now >= at);
    if let Some(h) = &mut app.midi_host
        && !h.has_waker()
        && due
        && s.wake_wanted.load(std::sync::atomic::Ordering::Relaxed) != crate::midi_input::WAKE_OFF
    {
        let c = ctx.clone();
        let wake = move |level: u8| {
            if level >= crate::midi_input::WAKE_URGENT {
                c.request_repaint();
            } else {
                c.request_repaint_after(crate::midi_input::WAKE_DISPLAY_POLL);
            }
        };
        let started = h.set_waker(std::sync::Arc::clone(&s.wake_wanted), Box::new(wake));
        s.waker_retry_at = (!started).then_some(s.now + crate::midi_input::REFRESH_S);
    }
    let s = &app.midi_learn;
    if s.gesture.is_some() || !s.touch.is_empty() || s.detect.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    }
    if app.midi_learn.decks.bending(&Ctx::of(app, s.now)) {
        // A held pitch bend nudges every frame.
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
    if let Some(every) = crate::controller_layouts::repaint_every(app) {
        // A controller acts and lights up in UI frames (see `repaint_every`).
        ctx.request_repaint_after(every);
    }
}

/// Whether MIDI moved `param` of `track` within the last [`GESTURE_S`] (Touch/Latch writing).
pub(crate) fn touched(app: &SoundApp, track: TrackId, param: &soundcraft_model::AutoParam) -> bool {
    app.midi_learn.touch.get(&(track, param.clone())).is_some_and(|at| app.midi_learn.now - at <= GESTURE_S)
}

/// Whether a MIDI control's gesture is merging now: the frame's end-of-gesture must leave it.
pub fn holds_merge(app: &SoundApp) -> bool {
    app.midi_learn.gesture.as_ref().is_some_and(|(key, _)| app.engine.merge_key() == Some(key.as_str()))
}

/// The (port, control) pairs mapped on the ports opened so far.
fn controls(app: &SoundApp) -> Vec<(PortId, MidiControl)> {
    let Some(h) = &app.midi_host else { return Vec::new() };
    let src = h.source();
    let mut out = Vec::new();
    for &port in h.opened() {
        let (Some(raw), Some(backend)) = (src.raw_name(port), src.backend_id(port)) else { continue };
        for m in app.ui.midi_mappings.iter().filter(|m| m.device.is(raw, backend)) {
            out.push((port, m.matcher.control()));
            if let (Matcher::Cc { ch, .. }, Some(cc)) = (m.matcher, m.lsb) {
                out.push((port, MidiControl::Cc { ch, cc }));
            }
        }
    }
    out.extend(crate::controller_layouts::controls(app));
    out
}

fn sync_controls(app: &mut SoundApp) {
    let Some(p) = &app.player else { return };
    let now = (p.id(), controls(app));
    if app.midi_learn.controls_sent.as_ref() != Some(&now) {
        if !p.set_midi_controls(&now.1) && !app.midi_learn.overflow_logged {
            log::warn!(
                "MIDI mappings on more than {} ports: mapped notes of the others also play the live target",
                soundcraft_playback::CONTROL_PORTS
            );
            app.midi_learn.overflow_logged = true;
        }
        app.midi_learn.controls_sent = Some(now);
    }
}

/// One event from the MIDI host at UI time `now`.
pub fn handle(app: &mut SoundApp, e: &MidiEvent, now: f64) {
    let Some((matcher, input)) = Matcher::of(&e.msg) else { return };
    let Some(device) = app.midi_host.as_ref().and_then(|h| {
        let src = h.source();
        Some(DeviceKey {
            backend_id: src.backend_id(e.port)?.to_string(),
            raw_name: src.raw_name(e.port)?.to_string(),
            name: src.port_name(e.port).or_else(|| src.raw_name(e.port)).unwrap_or_default().to_string(),
        })
    }) else {
        return;
    };
    if app.midi_learn.learning.is_some() {
        // A note-off (of the key that started learning, say) teaches nothing.
        if input != Input::Note(false)
            && let Some(target) = app.midi_learn.learning.take()
        {
            let twin = crate::controller_layouts::twin(app, e.port, &e.msg);
            bind(app, device, matcher, input, target, twin);
        }
        return;
    }
    // A learned deck or mixer controller's LSB (its controller + 32) right after its MSB while it
    // is watched, twice: a 14-bit control. Not the same controller + 32 at other times (a
    // separate button there). An LSB sent before its MSB isn't recognised: such a device's
    // knob is learned 7-bit (its layout, if it has one, names both halves).
    let eligible = |app: &SoundApp, key: &str| app.ui.midi_mappings.iter().any(|m| m.key() == key && m.target.is_deck() && m.target.ranged());
    if let (Some(d), Matcher::Cc { ch, cc }) = (&app.midi_learn.detect, matcher)
        && let Matcher::Cc { ch: dch, cc: dcc } = d.matcher
        && d.device.same(&device)
        && dch == ch
        && dcc < 32
        && cc == dcc + 32
        && d.msb_last
        && eligible(app, &d.key)
    {
        let key = d.key.clone();
        let pairs = d.pairs.saturating_add(1);
        if let Some(d) = &mut app.midi_learn.detect {
            d.pairs = pairs;
            d.msb_last = false;
        }
        if pairs >= LSB_PAIRS
            && !app.midi_learn.detect.as_ref().is_some_and(|d| d.lone)
            && let Some(m) = app.ui.midi_mappings.iter_mut().find(|m| m.key() == key)
            && m.lsb != Some(cc)
        {
            m.lsb = Some(cc);
            app.mark_prefs_changed();
        }
        return;
    }
    if let Some(d) = &mut app.midi_learn.detect
        && d.device.same(&device)
    {
        let msb = d.matcher == matcher;
        d.lone |= msb && d.msb_last;
        d.msb_last = msb;
    }
    if let (Some(d), Input::Cc(v)) = (&mut app.midi_learn.detect, input)
        && d.device.same(&device)
        && d.matcher == matcher
    {
        // Still telling an encoder from a knob: these messages only teach.
        d.values.push(v);
        if d.values.len() >= DETECT_MSGS {
            finish_detect(app);
        }
        return;
    }
    // A 14-bit mapping's LSB counts as its message too.
    let lsb_of = |m: &Mapping| matches!((m.matcher, matcher), (Matcher::Cc { ch: a, .. }, Matcher::Cc { ch: b, cc }) if a == b && m.lsb == Some(cc));
    let hits: Vec<(String, bool)> = app
        .ui
        .midi_mappings
        .iter()
        .filter(|m| m.device.same(&device) && (m.matcher == matcher || lsb_of(m)))
        .map(|m| (m.key(), m.matcher != matcher))
        .collect();
    if hits.is_empty() {
        // The other half of a learned 14-bit control: the mapping covers it and the layout
        // doesn't get it (the learned half drives the target).
        if let Some(t) = crate::controller_layouts::twin(app, e.port, &e.msg)
            && app.ui.midi_mappings.iter().any(|m| m.device.same(&device) && m.matcher.control() == t)
        {
            return;
        }
        // Not mapped: the device's built-in layout, if it has one, takes it.
        crate::controller_layouts::handle(app, e.port, &e.msg, now);
        return;
    }
    for (key, lsb) in hits {
        // Found again by key: a command run by an earlier hit may have changed the list.
        if let Some(m) = app.ui.midi_mappings.iter().find(|m| m.key() == key).cloned() {
            apply(app, &m, input, lsb, now);
        }
    }
}

/// The edge a button-like input makes: `Some(true)` pressed, `Some(false)` released, `None`
/// neither (a controller staying on one side of 64, a relative tick down).
fn edge(m: &Mapping, rt: &mut Runtime, input: Input) -> Option<bool> {
    let down = match input {
        Input::Note(on) => on,
        Input::Cc(v) if m.mode.relative() => m.mode.delta(v) > 0,
        Input::Cc(v) => v >= 64,
    };
    if let Input::Cc(_) = input {
        if down == rt.pressed {
            return None;
        }
        if m.mode.relative() {
            // Every tick up is a press.
            rt.pressed = false;
            return down.then_some(true);
        }
    }
    rt.pressed = down;
    Some(down)
}

fn apply(app: &mut SoundApp, m: &Mapping, input: Input, lsb: bool, now: f64) {
    let key = m.key();
    let mut rt = app.midi_learn.runtime.remove(&key).unwrap_or_default();
    if rt.mode != Some(m.mode) {
        // A new mode reads the control afresh (pickup, button state).
        rt = Runtime { activity: rt.activity, mode: Some(m.mode), ..Runtime::default() };
    }
    rt.activity = Some((
        now,
        match input {
            Input::Note(true) => "on".into(),
            Input::Note(false) => "off".into(),
            Input::Cc(v) => v.to_string(),
        },
    ));
    if m.target.is_deck() {
        deck_input(app, m, &mut rt, input, lsb, now);
    } else if m.target.is_trigger() {
        if edge(m, &mut rt, input) == Some(true) {
            trigger(app, m);
        }
    } else {
        continuous(app, m, &mut rt, input, now);
    }
    app.midi_learn.runtime.insert(key, rt);
}

/// A trigger target's press.
fn trigger(app: &mut SoundApp, m: &Mapping) {
    let r = match &m.target {
        Target::SlotLaunch { column, slot } => {
            let playing = app.engine.execute("live.state", &json!({})).ok().and_then(|s| s["columns"][*column]["playing"]["slot"].as_u64());
            if m.mode == Mode::Toggle && playing == Some(*slot as u64) {
                app.run("live.stop", json!({"column": column}))
            } else {
                app.run("live.launch", json!({"column": column, "slot": slot}))
            }
        }
        Target::Scene { row } => app.run("live.scene", json!({"row": row})),
        Target::StopColumn { column } => app.run("live.stop", json!({"column": column})),
        Target::StopAll => app.run("live.stop_all", json!({})),
        Target::Command { id, params } => {
            let params = if params.is_null() { json!({}) } else { params.clone() };
            app.run(id, params)
        }
        _ => Ok(Value::Null),
    };
    if let Err(e) = r {
        skipped(app, m, &e);
    }
}

/// A mapping whose target can't act now: said on the status line, never more.
fn skipped(app: &mut SoundApp, m: &Mapping, why: &str) {
    app.ui.status = format!("MIDI {} on {} → {}: {why}", m.matcher.label(), m.device.name, m.target.label());
    log::info!("{}", app.ui.status);
}

/// A 7-bit bipolar control's dead zone around the centre (64 and 63 read as the centre).
const DEAD_7: f64 = 1.5 / 127.0;
/// A 14-bit one's: ±32 counts, as the built-in layouts read it.
const DEAD_14: f64 = 32.0 / 8191.5;

/// A deck, mixer or library mapping's message: what the same control of a built-in layout does
/// ([`crate::deck_controls`]). Buttons act on their edges (a deck's CUE, hot cues and pitch bend
/// on the release too; Toggle and Momentary read alike, the control's own behaviour decides),
/// the jog and the browse knob step (an absolute controller by its change), and the knobs and
/// faders take an absolute controller (7- or 14-bit, soft takeover), a relative one or a button.
fn deck_input(app: &mut SoundApp, m: &Mapping, rt: &mut Runtime, input: Input, lsb: bool, now: f64) {
    let Some(target) = m.target.deck_target() else { return };
    let ctx = Ctx::of(app, now);
    let mut out = Vec::new();
    match target {
        DeckTarget::Button(d, b) => {
            if let Some(down) = edge(m, rt, input) {
                out = app.midi_learn.decks.button(d, b, down, &ctx);
                let key = m.key();
                app.midi_learn.holders.retain(|h| h.0 != key);
                if down && b.momentary() {
                    app.midi_learn.holders.push((key, m.device.clone(), d, b));
                }
            }
        }
        DeckTarget::Jump(d, on) => {
            if edge(m, rt, input) == Some(true) {
                let beats = crate::decks_view::jump_beats(app, d);
                out = app.midi_learn.decks.button(d, Button::BeatJump(if on { beats } else { -beats }), true, &ctx);
            }
        }
        DeckTarget::MasterCue => {
            if edge(m, rt, input) == Some(true) {
                out.push(crate::deck_controls::master_cue());
            }
        }
        DeckTarget::Step(n) => {
            if edge(m, rt, input) == Some(true) {
                out.push(Action::Browse(n));
            }
        }
        DeckTarget::LoadEmpty => {
            if edge(m, rt, input) == Some(true) {
                out.push(Action::Load(None));
            }
        }
        DeckTarget::Jog(d) => {
            let ticks = steps(m, rt, input);
            if ticks != 0 {
                let scale = f64::from(m.sensitivity / DEFAULT_SENSITIVITY);
                app.midi_learn.decks.jog(d, f64::from(ticks), false, scale, &ctx);
            }
        }
        DeckTarget::Browse => {
            let ticks = steps(m, rt, input);
            if ticks != 0 {
                out.push(Action::Browse(isize::try_from(ticks).unwrap_or(0)));
            }
        }
        DeckTarget::Continuous(d, c) => out.extend(ranged(app, m, rt, input, lsb, (d, c), &ctx)),
    }
    #[cfg(test)]
    app.midi_learn.actions.extend(out.iter().cloned());
    for a in out {
        crate::controller_layouts::perform(app, "MIDI", a);
    }
}

/// The steps a jog or browse mapping's message makes: a relative controller's ticks, an absolute
/// one's change since its last value, a button's press.
fn steps(m: &Mapping, rt: &mut Runtime, input: Input) -> i32 {
    match input {
        Input::Cc(v) if m.mode.relative() => m.mode.delta(v),
        Input::Cc(v) if m.mode == Mode::Absolute => {
            let d = rt.last_abs.map_or(0, |l| i32::from(v) - i32::from(l));
            rt.last_abs = Some(v);
            // A change of more than half the range is the controller wrapping (127 to 0).
            match d {
                65.. => d - 128,
                ..=-65 => d + 128,
                _ => d,
            }
        }
        _ => i32::from(edge(m, rt, input) == Some(true)),
    }
}

/// A deck or mixer control's travel for a button: ("on", "off"), the centre and the bottom for a
/// bipolar control, else the top and the bottom.
fn deck_on_off(c: Continuous) -> (f64, f64) {
    match c {
        Continuous::Pitch | Continuous::Trim | Continuous::Eq(_) | Continuous::Filter | Continuous::Crossfader => (0.5, 0.0),
        _ => (1.0, 0.0),
    }
}

/// A knob or fader of deck `d` (or the mixer): the command its message asks for.
fn ranged(app: &mut SoundApp, m: &Mapping, rt: &mut Runtime, input: Input, lsb: bool, (d, c): (DeckId, Continuous), ctx: &Ctx) -> Option<Action> {
    let cur = c.travel(d, ctx, &app.engine.deck_status);
    let now = ctx.now;
    let button = m.matcher.is_note() || matches!(m.mode, Mode::Momentary | Mode::Toggle);
    // The travel asked for, the centre's dead zone, and whether it acts at once (no takeover).
    let (to, dead, at_once) = if button {
        let (on, off) = deck_on_off(c);
        let to = match (edge(m, rt, input)?, m.mode) {
            (true, Mode::Toggle) => {
                if (cur - on).abs() <= (cur - off).abs() {
                    off
                } else {
                    on
                }
            }
            (true, _) => on,
            (false, Mode::Momentary) => off,
            (false, _) => return None,
        };
        (to, 0.0, true)
    } else if m.mode.relative() {
        let Input::Cc(v) = input else { return None };
        // From the value sent last while the deck hasn't reported it yet: no tick is lost.
        let base = rt.takeover.recent(now).unwrap_or(cur);
        ((base + f64::from(m.mode.delta(v)) * f64::from(m.sensitivity)).clamp(0.0, 1.0), 0.0, true)
    } else {
        let Input::Cc(v) = input else { return None };
        let (t, dead) = if m.lsb.is_some() && lsb {
            rt.lone_msbs = 0;
            let msb = rt.msb?;
            (f64::from(u16::from(msb & 0x7F) << 7 | u16::from(v & 0x7F)) / 16383.0, DEAD_14)
        } else if m.lsb.is_some() {
            // The MSB waits for its LSB; MSBs that never get one are read as 7-bit.
            if rt.msb.is_some() {
                rt.lone_msbs = rt.lone_msbs.saturating_add(1);
            }
            rt.msb = Some(v);
            if rt.lone_msbs < LONE_MSBS {
                return None;
            }
            (f64::from(v) / 127.0, DEAD_7)
        } else {
            (f64::from(v) / 127.0, DEAD_7)
        };
        (if m.invert { 1.0 - t } else { t }, dead, !m.pickup)
    };
    let window = f64::from(PICKUP_WINDOW);
    if c == Continuous::Pitch {
        // The pitch's takeover is the deck's, as a layout's tempo slider: bypassed only while
        // the deck follows a master on the other deck.
        rt.takeover.admit(to, cur, window, true, now);
        let pct = crate::deck_controls::round(crate::deck_controls::centred(to, dead) * crate::deck_controls::range_pct(ctx, d), 3);
        let a = app.midi_learn.decks.tempo(d, pct, !at_once, ctx);
        if a.is_none() && ctx.deck(d).loaded {
            pickup_hint(app, m, cur);
        }
        return a;
    }
    rt.takeover.rearm(cur, window, at_once, now);
    if !rt.takeover.admit(to, cur, window, at_once, now) {
        pickup_hint(app, m, cur);
        return None;
    }
    rt.picked = true;
    c.action(d, to, dead)
}

fn pickup_hint(app: &mut SoundApp, m: &Mapping, cur: f64) {
    app.ui.status = format!("{}: move {} to {:.0} to pick it up", m.target.label(), m.matcher.label(), cur * 127.0);
}

/// Lets go of everything the learned deck controls hold (a CUE or hot cue preview, a pitch
/// bend): learning starts, a device holding one goes away, the app quits.
pub fn release_holds(app: &mut SoundApp) {
    app.midi_learn.holders.clear();
    for a in app.midi_learn.decks.release_all() {
        crate::controller_layouts::perform(app, "MIDI", a);
    }
}

/// The inputs open now: (raw name, backend id).
fn open_devices(app: &SoundApp) -> Vec<(String, String)> {
    let Some(h) = &app.midi_host else { return Vec::new() };
    let src = h.source();
    h.opened().iter().filter(|p| h.input_open(**p)).filter_map(|p| Some((src.raw_name(*p)?.to_string(), src.backend_id(*p)?.to_string()))).collect()
}

/// Once a frame: a learned deck control held by a device that went away, or by a mapping
/// removed or replaced, lets go; the jog's counts and a held pitch bend are sent.
fn deck_frame(app: &mut SoundApp, now: f64) {
    let dt = app.midi_learn.tick_at.replace(now).map_or(1.0 / 60.0, |t| now - t);
    if !app.midi_learn.holders.is_empty() {
        let open = open_devices(app);
        let keys: std::collections::HashSet<String> = app.ui.midi_mappings.iter().map(Mapping::key).collect();
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut app.midi_learn.holders)
            .into_iter()
            .partition(|(key, dev, ..)| !keys.contains(key) || !open.iter().any(|(raw, backend)| dev.is(raw, backend)));
        app.midi_learn.holders = kept;
        if !gone.is_empty() {
            let ctx = Ctx::of(app, now);
            for (_, _, d, b) in gone {
                for a in app.midi_learn.decks.button(d, b, false, &ctx) {
                    crate::controller_layouts::perform(app, "MIDI", a);
                }
            }
        }
    }
    if !app.ui.midi_mappings.iter().any(|m| m.target.is_deck()) {
        return;
    }
    let ctx = Ctx::of(app, now);
    let out = app.midi_learn.decks.tick(&ctx, dt);
    #[cfg(test)]
    app.midi_learn.actions.extend(out.iter().cloned());
    for a in out {
        crate::controller_layouts::perform(app, "MIDI", a);
    }
}

/// Whether a mapping's target is there to act on.
fn resolved(app: &SoundApp, t: &Target) -> bool {
    t.is_trigger() || t.is_deck() || travel(app, t).is_ok()
}

/// A continuous target's travel ("on", "off") for buttons.
fn on_off(target: &Target) -> (f32, f32) {
    match target {
        Target::TrackVolume { .. } => (soundcraft_model::fader_db_to_pos(0.0), 0.0),
        Target::StemFader { .. } => (crate::stems::pos_of(0.0), 0.0),
        _ => (1.0, 0.0),
    }
}

fn continuous(app: &mut SoundApp, m: &Mapping, rt: &mut Runtime, input: Input, now: f64) {
    let cur = match travel(app, &m.target) {
        Ok(v) => v,
        Err(e) => return skipped(app, m, &e),
    };
    if rt.sent.is_some_and(|s| (s - cur).abs() > 1e-4) {
        // Moved elsewhere since: pick it up again.
        rt.picked = false;
        rt.sent = None;
    }
    let button = m.matcher.is_note() || matches!(m.mode, Mode::Momentary | Mode::Toggle);
    let to = match input {
        _ if button => {
            let (on, off) = on_off(&m.target);
            match (edge(m, rt, input), m.mode) {
                (Some(true), Mode::Toggle) => Some(if (cur - on).abs() <= (cur - off).abs() { off } else { on }),
                (Some(true), _) => Some(on),
                (Some(false), Mode::Momentary) => Some(off),
                _ => None,
            }
        }
        Input::Cc(v) if m.mode.relative() => Some((cur + m.mode.delta(v) as f32 * m.sensitivity).clamp(0.0, 1.0)),
        Input::Cc(v) => {
            let p = f32::from(v) / 127.0;
            let p = if m.invert { 1.0 - p } else { p };
            if m.pickup && !rt.picked {
                let crossed = rt.last_in.is_some_and(|l| (l - cur) * (p - cur) <= 0.0);
                if crossed || (p - cur).abs() <= PICKUP_WINDOW {
                    rt.picked = true;
                    rt.last_in = None;
                } else {
                    rt.last_in = Some(p);
                    app.ui.status = format!("{}: move {} to {:.0} to pick it up", m.target.label(), m.matcher.label(), cur * 127.0);
                }
            }
            (!m.pickup || rt.picked).then_some(p)
        }
        Input::Note(_) => None,
    };
    let Some(to) = to else { return };
    if (to - cur).abs() < 1e-4 {
        // Nothing to change (a jittery pot): no edit, and the gesture isn't held open.
        rt.picked = true;
        rt.sent = Some(cur);
        return;
    }
    let merged = !button;
    if !merged {
        app.engine.end_merge();
    }
    if let Err(e) = set_travel(app, &m.target, to, merged.then_some(MIDI_GESTURE)) {
        return skipped(app, m, &e);
    }
    rt.picked = true;
    rt.sent = travel(app, &m.target).ok();
    if merged {
        app.midi_learn.gesture = Some((MIDI_GESTURE.to_string(), now));
    }
    if let Some(key) = automation_param(app, &m.target) {
        app.midi_learn.touch.insert(key, now);
    }
}

/// The automation lane a MIDI move of `target` writes in Touch/Latch: volume, pan, stems.
fn automation_param(app: &SoundApp, target: &Target) -> Option<(TrackId, soundcraft_model::AutoParam)> {
    use soundcraft_model::AutoParam;
    let s = app.engine.session();
    let (track, param) = match target {
        Target::TrackVolume { track } => (track, AutoParam::Volume),
        Target::TrackPan { track } => (track, AutoParam::Pan(0)),
        Target::StemFader { track, stem } => (track, AutoParam::Stem(u8::try_from(stem_index(stem).ok()?).ok()?)),
        _ => return None,
    };
    Some((resolve(s, track)?, param))
}

fn track_of<'a>(s: &'a Session, r: &TrackRef) -> Result<&'a Track, String> {
    resolve(s, r).and_then(|id| s.track(id)).ok_or_else(|| format!("no track \"{}\"", r.name))
}

fn param_of(s: &Session, track: &TrackRef, slot: Option<usize>, param: &str) -> Result<(TrackId, &'static soundcraft_dsp::ParamInfo, f32), String> {
    let t = track_of(s, track)?;
    let ins = match slot {
        None => t.instrument.as_ref().ok_or_else(|| format!("\"{}\" has no instrument", t.name))?,
        Some(i) => t.mixer.inserts.get(i).and_then(Option::as_ref).ok_or_else(|| format!("\"{}\" has no insert in that slot", t.name))?,
    };
    let info = crate::mix_window::plugin_info(&ins.plugin).ok_or_else(|| format!("{} has no parameter list", ins.plugin))?;
    let p = info.param(param).ok_or_else(|| format!("{} has no parameter \"{param}\"", info.name))?;
    Ok((t.id, p, ins.params.get(param).copied().unwrap_or(p.default)))
}

fn stem_index(stem: &str) -> Result<usize, String> {
    STEMS.iter().position(|s| *s == stem).ok_or_else(|| format!("no stem \"{stem}\""))
}

/// A continuous target's value now, as travel 0..1.
pub fn travel(app: &SoundApp, target: &Target) -> Result<f32, String> {
    let s = app.engine.session();
    Ok(match target {
        Target::TrackVolume { track } => soundcraft_model::fader_db_to_pos(track_of(s, track)?.mixer.volume_db),
        Target::TrackPan { track } => {
            let t = track_of(s, track)?;
            (t.mixer.pan.first().copied().ok_or_else(|| format!("\"{}\" has no pan", t.name))? + 1.0) / 2.0
        }
        Target::StemFader { track, stem } => {
            let i = stem_index(stem)?;
            crate::stems::pos_of(track_of(s, track)?.mixer.stem_gain_db.get(i).copied().unwrap_or(0.0))
        }
        Target::InstrumentParam { track, param } => {
            let (_, p, v) = param_of(s, track, None, param)?;
            p.to_normalized(v)
        }
        Target::InsertParam { track, slot, param } => {
            let (_, p, v) = param_of(s, track, Some(*slot), param)?;
            p.to_normalized(v)
        }
        _ => return Err("not a continuous control".into()),
    })
}

/// 0 dB for a level within a thousandth of it (the taper's rounding at unity).
fn near_unity(db: f32) -> f32 {
    if db.abs() < 1e-3 { 0.0 } else { db }
}

/// Sets a continuous target to travel `to`, merged into the gesture `merge` (or its own step).
fn set_travel(app: &mut SoundApp, target: &Target, to: f32, merge: Option<&str>) -> Result<(), String> {
    let s = app.engine.session();
    let (id, params) = match target {
        Target::TrackVolume { track } => {
            let db = near_unity(soundcraft_model::fader_pos_to_db(to).clamp(-144.0, 12.0));
            ("mix.volume", json!({"track": track_of(s, track)?.id.0, "db": db}))
        }
        Target::TrackPan { track } => {
            let pan = to * 2.0 - 1.0;
            let pan = if pan.abs() < 0.008 { 0.0 } else { pan.clamp(-1.0, 1.0) };
            ("mix.pan", json!({"track": track_of(s, track)?.id.0, "pan": pan, "index": 0}))
        }
        Target::StemFader { track, stem } => {
            stem_index(stem)?;
            ("mix.stem_gain", json!({"track": track_of(s, track)?.id.0, "stem": stem, "db": near_unity(crate::stems::db_of(to))}))
        }
        Target::InstrumentParam { track, param } => {
            let (t, p, _) = param_of(s, track, None, param)?;
            ("mix.instrument_param", json!({"track": t.0, "param": param, "value": p.from_normalized(to)}))
        }
        Target::InsertParam { track, slot, param } => {
            let (t, p, _) = param_of(s, track, Some(*slot), param)?;
            let value = p.from_normalized(to);
            if let Some(pl) = &app.player {
                // Keep the plugin's own editor in step.
                pl.editor_set_param(t, *slot, param, value);
            }
            ("mix.insert_param", json!({"track": t.0, "slot": slot, "param": param, "value": value}))
        }
        _ => return Err("not a continuous control".into()),
    };
    let r = match merge {
        Some(key) => app.engine.execute_merged(id, &params, key),
        None => app.engine.execute(id, &params),
    };
    r.map(|_| ()).map_err(|e| e.to_string())
}

/// A mapping's last activity: how long ago (seconds) and what (a value, "on", "off").
pub fn activity(app: &SoundApp, m: &Mapping) -> Option<(f64, String)> {
    app.midi_learn.runtime.get(&m.key()).and_then(|r| r.activity.clone()).map(|(at, what)| (app.midi_learn.now - at, what))
}

/// The params of the MIDI learn commands, as `engine.commands` advertises them.
pub fn params_doc(id: &str) -> Option<&'static str> {
    Some(match id {
        "midi.mappings" => {
            "{kind?: deck|mixer|library|… (only mappings of targets of that kind)} — every mapping: {index, device, matcher, message, target, label, mode, sensitivity, pickup, lsb, resolved, activity_s}"
        }
        "window.midi_mappings" | "midi.learn_command" => "{}",
        "midi.map" => {
            "{device: listed input name | {backend_id, raw_name, name}, matcher: {type: note|cc, ch, note|cc}, target: TARGET, mode?: momentary|toggle|absolute|rel_twos|rel_offset|rel_sign_mag, sensitivity?, pickup?: bool, lsb?: cc (a 14-bit control's LSB: a deck or mixer knob or fader, absolute, another controller than the matcher's), invert?: bool (an absolute controller upside down: its top is the target's bottom; off by default, a learned pitch fader too: CC 0 is −range), replace?: bool} — TARGET: {kind: slot_launch {column, slot} | scene {row} | stop_column {column} | stop_all | track_volume {track} | track_pan {track} | stem_fader {track, stem} | instrument_param {track, param} | insert_param {track, slot, param} | command {id, params} | deck {deck: a|b, control: DECK} | mixer {control: crossfader|master_volume|headphone_mix|headphone_level|master_cue} | library {control: browse|previous|next|load}}; DECK: play|cue|sync|master_tempo|pitch|pitch_range|pitch_bend_up|pitch_bend_down|jog|hot_cue {n: 1..=8}|hot_cue_clear {n}|loop4|loop_in|loop_out|loop_halve|loop_double|reloop|jump_back|jump_forward|beat_jump {beats: ±1..=64}|load|trim|eq_high|eq_mid|eq_low|filter|fader|headphone_cue; track: id | name | {id, name}; replace also removes another target's mapping of the same message. Deck controls act as a built-in controller layout's (CUE press and release, hot cue set/jump/release, toggles, jog nudge or search, pitch soft takeover); a learned message overrides the device's layout"
        }
        "midi.unmap" => "{index} | {target} | {device, matcher} | {all: true} — what it removes is kept for midi.restore_mappings (one level)",
        "midi.restore_mappings" => "{} — the mappings removed last (Delete, Remove All, midi.unmap) come back",
        "midi.learn" => {
            "{target} | {cancel: true} — the next note-on or controller from an enabled input binds target (as midi.map's, deck and mixer controls too: {kind: deck, deck: a, control: cue}); a knob a device's layout knows as 14-bit is learned with both halves"
        }
        _ => return None,
    })
}

/// A target as the commands take it: a track may be an id, a name or `{id, name}`.
fn parse_target(s: &Session, v: &Value) -> Result<Target, String> {
    let mut v = v.clone();
    if let Some(t) = v.get_mut("track") {
        let found = match &*t {
            Value::String(n) => Some(s.track_by_name(n).ok_or_else(|| format!("no track named \"{n}\""))?),
            Value::Number(n) => {
                let id = n.as_u64().ok_or("`track` is a track id or name")?;
                Some(s.track(TrackId(id)).ok_or_else(|| format!("no track {id}"))?)
            }
            _ => None,
        };
        if let Some(found) = found {
            *t = json!(TrackRef::of(found));
        }
    }
    let target: Target = serde_json::from_value(v).map_err(|e| format!("bad `target`: {e}"))?;
    if let Target::StemFader { stem, .. } = &target {
        stem_index(stem)?;
    }
    if !target.valid() {
        return Err(format!("bad `target`: {} is out of range (hot cues 1..=8, beat jumps ±1..=64)", target.label()));
    }
    Ok(target)
}

fn parse_device(app: &SoundApp, v: &Value) -> Result<DeviceKey, String> {
    match v {
        Value::String(name) => {
            let host = app.midi_host.as_ref().ok_or("no MIDI input on this system")?;
            let p = host.source().list().into_iter().find(|p| p.name == *name).ok_or_else(|| format!("no MIDI input named \"{name}\""))?;
            Ok(DeviceKey { backend_id: p.backend_id, raw_name: p.raw_name, name: p.name })
        }
        Value::Object(_) => serde_json::from_value(v.clone()).map_err(|e| format!("bad `device`: {e}")),
        _ => Err("`device` (a listed input name, or {backend_id, raw_name, name}) required".into()),
    }
}

/// The MIDI learn UI commands; `None` for other ids.
/// - `midi.mappings {}`: `{mappings: [{index, device, matcher, message, target, label, mode,
///   sensitivity, pickup, resolved, activity_s}], learning}`.
/// - `midi.map {device, matcher, target, mode?, sensitivity?, pickup?, replace?}`: adds a mapping
///   (replacing the target's own); `{index, mapping}`.
/// - `midi.unmap {index} | {target} | {device, matcher} | {all}`: `{removed}`.
/// - `midi.learn {target} | {cancel}`, `midi.learn_command {}` (the command palette picks the
///   command), `window.midi_mappings {value?}`.
pub fn command(app: &mut SoundApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    Some(match id {
        "midi.mappings" => {
            let kind = p.get("kind").and_then(Value::as_str);
            let list: Vec<Value> = app
                .ui
                .midi_mappings
                .iter()
                .enumerate()
                .filter(|(_, m)| kind.is_none_or(|k| serde_json::to_value(&m.target).ok().is_some_and(|t| t["kind"] == k)))
                .map(|(i, m)| {
                    let mut v = m.json();
                    v["index"] = json!(i);
                    v["resolved"] = json!(resolved(app, &m.target));
                    v["activity_s"] = json!(activity(app, m).map(|a| a.0));
                    v
                })
                .collect();
            Ok(json!({"mappings": list, "learning": app.midi_learn.learning.as_ref().map(Target::label)}))
        }
        "midi.map" => (|| {
            let device = parse_device(app, p.get("device").unwrap_or(&Value::Null))?;
            let matcher: Matcher = serde_json::from_value(p.get("matcher").cloned().unwrap_or_default())
                .map_err(|e| format!("bad `matcher` ({{type: note|cc, ch, note|cc}}): {e}"))?;
            if !matcher.valid() {
                return Err("`matcher`: channels are 0..=15, notes and controllers 0..=127".into());
            }
            let target = parse_target(app.engine.session(), p.get("target").unwrap_or(&Value::Null))?;
            let mode = match p.get("mode") {
                None | Some(Value::Null) => default_mode(&matcher, &target),
                Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("bad `mode`: {e}"))?,
            };
            if !mode.fits(&matcher) {
                return Err(format!("a note is momentary or toggle, not {}", mode.id()));
            }
            let sensitivity = clamp_sensitivity(p.get("sensitivity").and_then(Value::as_f64).map_or(DEFAULT_SENSITIVITY, |s| s as f32));
            let pickup = p.get("pickup").and_then(Value::as_bool).unwrap_or(true);
            let replace = p.get("replace").and_then(Value::as_bool).unwrap_or(false);
            let lsb = match p.get("lsb") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().and_then(|n| u8::try_from(n).ok()).filter(|n| *n <= 127).ok_or("`lsb` is a controller 0..=127")?),
            };
            let invert = match p.get("invert") {
                None | Some(Value::Null) => false,
                Some(v) => v.as_bool().ok_or("`invert` must be true or false")?,
            };
            let m = Mapping { device, matcher, target, mode, sensitivity, pickup, lsb, invert };
            if let Some(why) = m.lsb_refused() {
                return Err(why.into());
            }
            let i = commit(app, m, replace);
            app.midi_learn.overlay_mapping = None;
            Ok(json!({"index": i, "mapping": app.ui.midi_mappings.get(i).map(Mapping::json)}))
        })(),
        "midi.unmap" => (|| {
            let removed = if p.get("all").and_then(Value::as_bool) == Some(true) {
                remove_where(app, |_| true)
            } else if let Some(i) = p.get("index").and_then(Value::as_u64) {
                remove(app, usize::try_from(i).unwrap_or(usize::MAX)).ok_or_else(|| format!("no mapping {i}"))?;
                1
            } else if let Some(t) = p.get("target") {
                let target = parse_target(app.engine.session(), t)?;
                remove_where(app, |m| m.target == target)
            } else if let (Some(d), Some(mv)) = (p.get("device"), p.get("matcher")) {
                let device = parse_device(app, d)?;
                let matcher: Matcher = serde_json::from_value(mv.clone()).map_err(|e| format!("bad `matcher`: {e}"))?;
                remove_where(app, |m| m.device.same(&device) && m.matcher == matcher)
            } else {
                return Err("`index`, `target`, `device` and `matcher`, or `all` required".into());
            };
            Ok(json!({"removed": removed, "backup": app.ui.midi_mappings_backup.len()}))
        })(),
        "midi.restore_mappings" => {
            let n = restore(app);
            app.ui.status = format!("{n} MIDI mapping(s) restored");
            Ok(json!({"restored": n}))
        }
        "midi.learn" => (|| {
            if p.get("cancel").and_then(Value::as_bool) == Some(true) {
                cancel(app);
                return Ok(json!({"learning": null}));
            }
            let target = parse_target(app.engine.session(), p.get("target").ok_or("`target` (or `cancel: true`) required")?)?;
            let label = target.label();
            start(app, target);
            Ok(json!({"learning": label}))
        })(),
        "midi.learn_command" => {
            app.ui.show_search = true;
            app.palette = crate::palette::PaletteState { learn: true, ..crate::palette::PaletteState::default() };
            Ok(json!({"value": true}))
        }
        "window.midi_mappings" => {
            app.ui.show_midi_mappings = p.get("value").and_then(Value::as_bool).unwrap_or(!app.ui.show_midi_mappings);
            Ok(json!({"value": app.ui.show_midi_mappings}))
        }
        _ => return None,
    })
}

/// A control's right-click menu items: Learn MIDI, and Remove MIDI Mapping when it has one.
pub fn menu_items(app: &mut SoundApp, ui: &mut egui::Ui, target: Target) {
    menu_items_for(app, ui, target, "");
}

/// [`menu_items`] for one of a control's several targets, named `what` ("Learn MIDI for Pitch
/// Bend +"); an empty `what` is the control's own.
pub fn menu_items_for(app: &mut SoundApp, ui: &mut egui::Ui, target: Target, what: &str) {
    let n = mapped(app, &target);
    let (learn, remove) = if what.is_empty() {
        ("Learn MIDI".to_string(), "Remove MIDI Mapping".to_string())
    } else {
        (format!("Learn MIDI for {what}"), format!("Remove MIDI Mapping for {what}"))
    };
    if ui.button(learn).clicked() {
        start(app, target.clone());
        ui.close();
    }
    if n > 0 && ui.button(remove).clicked() {
        remove_where(app, |m| m.target == target);
        ui.close();
    }
    // An absolute controller on a knob or fader can read upside down.
    let absolute = |m: &Mapping| m.target == target && m.target.ranged() && !m.matcher.is_note() && m.mode == Mode::Absolute;
    if app.ui.midi_mappings.iter().any(absolute) {
        let mut on = app.ui.midi_mappings.iter().any(|m| absolute(m) && m.invert);
        let label = if what.is_empty() { "Invert MIDI".to_string() } else { format!("Invert MIDI for {what}") };
        if ui.checkbox(&mut on, label).on_hover_text("The controller's top is this control's bottom").changed() {
            set_invert(app, &target, on);
        }
    }
}

/// Inverts (or not) the absolute controllers mapped to `target`.
pub fn set_invert(app: &mut SoundApp, target: &Target, on: bool) {
    for m in app.ui.midi_mappings.iter_mut().filter(|m| &m.target == target && !m.matcher.is_note()) {
        m.invert = on;
    }
    app.mark_prefs_changed();
}

/// Esc cancels learning (taken before the shortcuts see it).
pub fn keys(app: &mut SoundApp, ctx: &egui::Context) {
    if (app.midi_learn.learning.is_some() || app.midi_learn.conflict.is_some())
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        cancel(app);
    }
}

/// The learn overlay, the Replace or Keep Both question and the MIDI Mappings window.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let t = crate::theme::Tokens::current();
    let overlay = |ctx: &egui::Context, add: &mut dyn FnMut(&mut egui::Ui)| {
        egui::Area::new(egui::Id::new("midi_learn_overlay"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 70.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| add(ui));
            });
    };
    if let Some(target) = app.midi_learn.learning.clone() {
        let mut stop = false;
        overlay(ctx, &mut |ui| {
            ui.label(egui::RichText::new("Move a control…").strong());
            ui.label(egui::RichText::new(format!("Learn MIDI for {} (Esc cancels)", target.label())).color(t.text_dim));
            stop = ui.button("Cancel").clicked();
        });
        if stop {
            cancel(app);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
    } else if let Some(c) = app.midi_learn.conflict.clone() {
        let mut answer = None;
        overlay(ctx, &mut |ui| {
            ui.label(egui::RichText::new(format!("{} {} is mapped to {}", c.mapping.device.name, c.mapping.matcher.label(), c.other)).strong());
            ui.horizontal(|ui| {
                if ui.button("Replace").clicked() {
                    answer = Some(Some(true));
                }
                if ui.button("Keep Both").clicked() {
                    answer = Some(Some(false));
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(None);
                }
            });
        });
        match answer {
            Some(Some(replace)) => resolve_conflict(app, replace),
            Some(None) => cancel(app),
            None => {}
        }
    } else if let Some((line, until)) = app.midi_learn.overlay.clone() {
        if app.midi_learn.now < until {
            // A learned controller's mode, detected or not yet, one click away.
            let key = app.midi_learn.overlay_mapping.clone();
            let found = key.as_ref().and_then(|k| app.ui.midi_mappings.iter().position(|m| &m.key() == k));
            let detecting = app.midi_learn.detect.is_some();
            let check = app.midi_learn.check_direction && mode_is_relative(app, found);
            let mut mode = found.and_then(|i| app.ui.midi_mappings.get(i)).map(|m| m.mode);
            let mut held = false;
            overlay(ctx, &mut |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&line).strong());
                    if let Some(mode) = &mut mode {
                        if detecting {
                            ui.label(egui::RichText::new("detecting…").color(t.text_dim));
                        } else {
                            if check {
                                ui.label(egui::RichText::new("(check direction)").color(t.text_dim))
                                    .on_hover_text("Every tick went one way: turn it the other way to check the mode");
                            }
                            egui::ComboBox::new("midi_learn_overlay_mode", "Mode").selected_text(mode.label()).show_ui(ui, |ui| {
                                for x in Mode::ALL.into_iter().filter(|x| !matches!(x, Mode::Momentary | Mode::Toggle)) {
                                    ui.selectable_value(mode, x, x.label());
                                }
                            });
                        }
                    }
                });
                held = ui.ui_contains_pointer() || egui::Popup::is_any_open(ui.ctx());
            });
            if let (Some(i), Some(mode)) = (found, mode)
                && let Some(m) = app.ui.midi_mappings.get_mut(i)
                && m.mode != mode
            {
                m.mode = mode;
                app.ui.status = format!("{} {}: {}", m.device.name, m.matcher.label(), mode.label());
                app.mark_prefs_changed();
            }
            if held || detecting {
                // Stays while it is being used.
                app.midi_learn.overlay = Some((line, app.midi_learn.now + 1.0_f64.max(until - app.midi_learn.now)));
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        } else {
            app.midi_learn.overlay = None;
            app.midi_learn.overlay_mapping = None;
        }
    }
    mappings_window(app, ctx);
}

fn mode_is_relative(app: &SoundApp, i: Option<usize>) -> bool {
    i.and_then(|i| app.ui.midi_mappings.get(i)).is_some_and(|m| m.mode.relative())
}

/// Setup › MIDI › MIDI Mappings: every mapping, its mode, sensitivity and pickup, and activity.
fn mappings_window(app: &mut SoundApp, ctx: &egui::Context) {
    let mut open = app.ui.show_midi_mappings;
    if !open {
        return;
    }
    let t = crate::theme::Tokens::current();
    let mut delete = None;
    let mut changed = false;
    crate::windows::floating(ctx, "MIDI Mappings").id(egui::Id::new("midi_mappings")).open(&mut open).default_size(egui::vec2(640.0, 300.0)).show(
        ctx,
        |ui| {
            if app.ui.midi_mappings.is_empty() {
                ui.label("No MIDI mappings. Right-click a slot, fader, knob or parameter and choose Learn MIDI.");
            }
            let rows: Vec<(Mapping, Option<(f64, String)>, bool)> =
                app.ui.midi_mappings.iter().map(|m| (m.clone(), activity(app, m), resolved(app, &m.target))).collect();
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("midi_mappings_grid").num_columns(7).striped(true).show(ui, |ui| {
                    for h in ["", "Device", "Message", "Target", "Mode", "Settings", ""] {
                        ui.label(egui::RichText::new(h).strong());
                    }
                    ui.end_row();
                    for (i, (m, act, resolved)) in rows.iter().enumerate() {
                        let lit = act.as_ref().is_some_and(|(age, _)| *age < ACTIVITY_S);
                        let dot = egui::RichText::new("●").color(if lit { t.accent } else { t.text_dim.gamma_multiply(0.4) });
                        ui.label(dot).on_hover_text(
                            act.as_ref().map_or_else(|| "No activity yet".to_string(), |(age, what)| format!("{what}, {age:.1} s ago")),
                        );
                        ui.label(&m.device.name).on_hover_text(&m.device.raw_name);
                        ui.label(m.matcher.label());
                        let label = if *resolved {
                            egui::RichText::new(m.target.label())
                        } else {
                            egui::RichText::new(format!("{} (missing)", m.target.label())).color(t.text_dim)
                        };
                        ui.label(label);
                        let mut mode = m.mode;
                        egui::ComboBox::from_id_salt(("midi_map_mode", i)).selected_text(mode.label()).show_ui(ui, |ui| {
                            for x in Mode::ALL.into_iter().filter(|x| x.fits(&m.matcher)) {
                                ui.selectable_value(&mut mode, x, x.label());
                            }
                        });
                        let mut sens = m.sensitivity;
                        let mut pickup = m.pickup;
                        let mut invert = m.invert;
                        ui.horizontal(|ui| {
                            if mode.relative() {
                                ui.add(egui::DragValue::new(&mut sens).range(MIN_SENSITIVITY..=MAX_SENSITIVITY).speed(0.0005).prefix("step "));
                            }
                            if m.target.ranged() && !m.matcher.is_note() && mode == Mode::Absolute {
                                ui.checkbox(&mut pickup, "Pickup");
                                ui.checkbox(&mut invert, "Invert").on_hover_text("The controller's top is the control's bottom");
                            }
                        });
                        if ui.button("Delete").clicked() {
                            delete = Some(i);
                        }
                        ui.end_row();
                        if let Some(x) = app.ui.midi_mappings.get_mut(i)
                            && (x.mode != mode || x.sensitivity != sens || x.pickup != pickup || x.invert != invert)
                        {
                            x.mode = mode;
                            x.sensitivity = sens;
                            x.pickup = pickup;
                            x.invert = invert;
                            if x.lsb_refused().is_some() {
                                // A 14-bit pair is absolute only.
                                x.lsb = None;
                            }
                            changed = true;
                        }
                    }
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Learn MIDI for Command...").clicked() {
                    let _ = app.run("midi.learn_command", json!({}));
                }
                if !app.ui.midi_mappings.is_empty() && ui.button("Remove All").clicked() {
                    app.midi_learn.confirm_remove_all = true;
                }
                let back = app.ui.midi_mappings_backup.len();
                if back > 0 && ui.button(format!("Undo Remove ({back})")).on_hover_text("Put back the mapping(s) removed last").clicked() {
                    let _ = app.run("midi.restore_mappings", json!({}));
                }
            });
        },
    );
    if app.midi_learn.confirm_remove_all {
        let n = app.ui.midi_mappings.len();
        let mut answer = None;
        egui::Modal::new(egui::Id::new("midi_mappings_remove_all")).show(ctx, |ui| {
            ui.label(egui::RichText::new(format!("Remove all {n} MIDI mapping(s)?")).strong());
            ui.label("Undo Remove in this window puts them back.");
            ui.horizontal(|ui| {
                if ui.button("Remove All").clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(yes) = answer {
            app.midi_learn.confirm_remove_all = false;
            if yes {
                let _ = app.run("midi.unmap", json!({"all": true}));
            }
        }
    }
    if let Some(i) = delete {
        remove(app, i);
    }
    if changed {
        app.mark_prefs_changed();
    }
    if app.ui.midi_mappings.iter().any(|m| activity(app, m).is_some_and(|(age, _)| age < ACTIVITY_S)) {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    app.ui.show_midi_mappings = open;
}

/// Whether an absolute controller on this mapping waits to be picked up (for tests and display).
pub fn waiting_for_pickup(app: &SoundApp, m: &Mapping) -> bool {
    m.uses_pickup() && m.pickup && !app.midi_learn.runtime.get(&m.key()).is_some_and(|r| r.picked)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_midi_io::{FakeSource, MidiClock};

    #[test]
    fn relative_modes_wrap_centre_and_sign_as_their_devices_send() {
        let d = |m: Mode, v: u8| m.delta(v);
        assert_eq!([1, 63, 127, 65, 64, 0].map(|v| d(Mode::RelTwos, v)), [1, 63, -1, -63, -64, 0], "two's complement wraps at 64");
        assert_eq!([64, 65, 63, 127, 0].map(|v| d(Mode::RelOffset, v)), [0, 1, -1, 63, -64], "64 is centre");
        assert_eq!([1, 65, 63, 127, 64, 0].map(|v| d(Mode::RelSignMag, v)), [1, -1, 63, -63, 0, 0], "bit 6 is the sign");
        assert_eq!(d(Mode::Absolute, 5), 0);
    }

    #[test]
    fn a_track_resolves_by_id_with_its_name_else_by_name() {
        let mut s = Session::default();
        let a = s.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Stereo, Some("Kick"));
        let b = s.add_track(soundcraft_model::TrackKind::Audio, soundcraft_model::ChannelFormat::Stereo, Some("Bass"));
        assert_eq!(resolve(&s, &TrackRef { id: a.0, name: "Kick".into() }), Some(a));
        assert_eq!(resolve(&s, &TrackRef { id: a.0, name: "Bass".into() }), Some(b), "the id has another name: by name");
        assert_eq!(resolve(&s, &TrackRef { id: 999, name: "Kick".into() }), Some(a), "no such id: by name");
        assert_eq!(resolve(&s, &TrackRef { id: a.0, name: "Gone".into() }), None);
    }

    #[test]
    fn unreadable_mappings_in_the_preferences_are_skipped() {
        let ui: crate::UiState = serde_json::from_value(json!({"midi_mappings": [
            {"device": {"backend_id": "b", "raw_name": "Pads", "name": "Pads"}, "matcher": {"type": "note", "ch": 0, "note": 36},
             "target": {"kind": "scene", "row": 1}, "mode": "momentary"},
            {"target": {"kind": "warp_drive"}},
            "nonsense"
        ]}))
        .unwrap();
        assert_eq!(ui.midi_mappings.len(), 1);
        assert_eq!(ui.midi_mappings[0].target, Target::Scene { row: 1 });
        assert_eq!(ui.midi_mappings[0].sensitivity, DEFAULT_SENSITIVITY, "defaults fill what's missing");
    }

    /// An app on the demo session with a FakeSource host whose "DDJ" and "Keys" are open.
    fn app() -> (SoundApp, PortId, PortId) {
        let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, crate::Services::default());
        let mut src = FakeSource::new(MidiClock::new()).unwrap();
        src.plug("DDJ");
        src.plug("Keys");
        app.midi_host = Some(crate::midi_input::MidiHost::new(Box::new(src)));
        app.run("midi.input_enable", json!({"name": "DDJ", "enabled": true})).unwrap();
        app.run("midi.input_enable", json!({"name": "Keys", "enabled": true})).unwrap();
        let h = app.midi_host.as_mut().unwrap();
        let ddj = h.source_mut().open("DDJ").unwrap();
        let keys = h.source_mut().open("Keys").unwrap();
        (app, ddj, keys)
    }

    fn cc(port: PortId, cc: u8, value: u8) -> MidiEvent {
        MidiEvent { port, msg: MidiMessage::ControlChange { ch: 0, cc, value }, time_ns: 0 }
    }

    fn note(port: PortId, note: u8, on: bool) -> MidiEvent {
        let msg = if on { MidiMessage::NoteOn { ch: 0, note, vel: 100 } } else { MidiMessage::NoteOff { ch: 0, note, vel: 0 } };
        MidiEvent { port, msg, time_ns: 0 }
    }

    fn db(app: &SoundApp, name: &str) -> f32 {
        app.engine.session().track_by_name(name).unwrap().mixer.volume_db
    }

    fn map(app: &mut SoundApp, matcher: Value, target: Value, extra: Value) {
        let mut p = json!({"device": "DDJ", "matcher": matcher, "target": target});
        if let (Some(o), Some(x)) = (p.as_object_mut(), extra.as_object()) {
            o.extend(x.clone());
        }
        app.run("midi.map", p).unwrap();
    }

    #[test]
    fn learning_binds_the_next_controller_and_a_note_off_teaches_nothing() {
        let (mut app, ddj, _) = app();
        app.run("midi.learn", json!({"target": {"kind": "track_volume", "track": "Kick"}})).unwrap();
        handle(&mut app, &note(ddj, 40, false), 0.0);
        assert!(app.midi_learn.learning.is_some(), "a note-off teaches nothing");
        handle(&mut app, &cc(ddj, 7, 30), 0.0);
        assert!(app.midi_learn.learning.is_none());
        let m = &app.ui.midi_mappings[0];
        assert_eq!((m.matcher, m.mode, m.device.name.as_str()), (Matcher::Cc { ch: 0, cc: 7 }, Mode::Absolute, "DDJ"));
        assert_eq!(app.midi_learn.overlay.as_ref().map(|o| o.0.as_str()), Some("Mapped to DDJ CC 7"));
        assert!(app.take_recent_changed(), "the preferences are saved");
        // Learning the fader again replaces its mapping.
        app.run("midi.learn", json!({"target": {"kind": "track_volume", "track": "Kick"}})).unwrap();
        handle(&mut app, &cc(ddj, 8, 30), 0.0);
        assert_eq!(app.ui.midi_mappings.len(), 1);
        assert_eq!(app.ui.midi_mappings[0].matcher, Matcher::Cc { ch: 0, cc: 8 });
    }

    #[test]
    fn a_message_already_mapped_asks_replace_or_keep_both() {
        let (mut app, ddj, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "track_volume", "track": "Kick"}), json!({}));
        app.run("midi.learn", json!({"target": {"kind": "track_pan", "track": "Kick"}})).unwrap();
        handle(&mut app, &cc(ddj, 7, 30), 0.0);
        assert_eq!(app.midi_learn.conflict.as_ref().map(|c| c.other.as_str()), Some("Kick volume"));
        assert_eq!(app.ui.midi_mappings.len(), 1, "nothing until answered");
        resolve_conflict(&mut app, false);
        assert_eq!(app.ui.midi_mappings.len(), 2, "kept both");
        app.run("midi.learn", json!({"target": {"kind": "stop_all"}})).unwrap();
        handle(&mut app, &cc(ddj, 7, 30), 0.0);
        resolve_conflict(&mut app, true);
        assert_eq!(app.ui.midi_mappings.iter().map(|m| m.target.label()).collect::<Vec<_>>(), ["Stop All"], "replaced both");
    }

    #[test]
    fn an_absolute_fader_waits_to_be_picked_up_and_follows_the_ui_taper() {
        let (mut app, ddj, _) = app();
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "track_volume", "track": "Kick"}), json!({}));
        // 0 dB is at 0.75 of the fader's travel (CC 95.25).
        handle(&mut app, &cc(ddj, 7, 10), 0.0);
        handle(&mut app, &cc(ddj, 7, 40), 0.0);
        assert_eq!(db(&app, "Kick"), 0.0, "below the value: ignored");
        assert!(waiting_for_pickup(&app, &app.ui.midi_mappings[0].clone()));
        handle(&mut app, &cc(ddj, 7, 100), 0.0);
        let want = soundcraft_model::fader_pos_to_db(100.0 / 127.0);
        assert!((db(&app, "Kick") - want).abs() < 1e-4, "crossed: picked up at {want}, got {}", db(&app, "Kick"));
        handle(&mut app, &cc(ddj, 7, 127), 0.0);
        assert_eq!(db(&app, "Kick"), 12.0, "the top is +12 dB, as on screen");
        // Moved with the mouse: picked up again before it follows.
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": -20.0})).unwrap();
        handle(&mut app, &cc(ddj, 7, 126), 0.0);
        assert_eq!(db(&app, "Kick"), -20.0);
        // Without pickup it jumps.
        app.ui.midi_mappings[0].pickup = false;
        handle(&mut app, &cc(ddj, 7, 0), 0.0);
        assert_eq!(db(&app, "Kick"), -144.0);
    }

    #[test]
    fn relative_controllers_step_by_their_sensitivity() {
        let (mut app, ddj, _) = app();
        app.run("mix.pan", json!({"tracks": ["Kick"], "pan": 0.0})).unwrap();
        map(
            &mut app,
            json!({"type": "cc", "ch": 0, "cc": 16}),
            json!({"kind": "track_pan", "track": "Kick"}),
            json!({"mode": "rel_offset", "sensitivity": 0.05}),
        );
        for _ in 0..3 {
            handle(&mut app, &cc(ddj, 16, 65), 0.0);
        }
        let pan = |app: &SoundApp| app.engine.session().track_by_name("Kick").unwrap().mixer.pan[0];
        assert!((pan(&app) - 0.3).abs() < 1e-4, "3 ticks of 0.05 travel: pan +0.3, got {}", pan(&app));
        app.ui.midi_mappings[0].mode = Mode::RelTwos;
        handle(&mut app, &cc(ddj, 16, 127), 0.0);
        assert!((pan(&app) - 0.2).abs() < 1e-4, "127 is one tick down");
    }

    #[test]
    fn toggle_flips_on_press_and_momentary_holds_while_pressed() {
        let (mut app, ddj, _) = app();
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
        map(&mut app, json!({"type": "note", "ch": 0, "note": 36}), json!({"kind": "track_volume", "track": "Kick"}), json!({}));
        assert_eq!(app.ui.midi_mappings[0].mode, Mode::Toggle, "a note on a fader toggles by default");
        handle(&mut app, &note(ddj, 36, true), 0.0);
        handle(&mut app, &note(ddj, 36, false), 0.0);
        assert_eq!(db(&app, "Kick"), -144.0, "on (0 dB) flips off; the release does nothing");
        handle(&mut app, &note(ddj, 36, true), 0.0);
        assert_eq!(db(&app, "Kick"), 0.0, "and back on");
        app.ui.midi_mappings[0].mode = Mode::Momentary;
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": -144.0})).unwrap();
        handle(&mut app, &note(ddj, 36, true), 0.0);
        assert_eq!(db(&app, "Kick"), 0.0, "held: on");
        handle(&mut app, &note(ddj, 36, false), 0.0);
        assert_eq!(db(&app, "Kick"), -144.0, "released: off");
        // A controller button (127 / 0) toggles on its way up only.
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 20}), json!({"kind": "track_volume", "track": "Bass"}), json!({"mode": "toggle"}));
        app.run("mix.volume", json!({"tracks": ["Bass"], "db": 0.0})).unwrap();
        for v in [127, 127, 0] {
            handle(&mut app, &cc(ddj, 20, v), 0.0);
        }
        assert_eq!(db(&app, "Bass"), -144.0);
    }

    #[test]
    fn one_knob_turn_is_one_undo_step() {
        let (mut app, ddj, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 16}), json!({"kind": "track_pan", "track": "Kick"}), json!({"mode": "rel_twos"}));
        let steps = app.engine.undo_history().len();
        let mut now = 1.0;
        for _ in 0..20 {
            frame(&mut app, now);
            handle(&mut app, &cc(ddj, 16, 1), now);
            assert!(holds_merge(&app));
            now += 0.02;
        }
        assert_eq!(app.engine.undo_history().len(), steps + 1, "one turn, one step");
        frame(&mut app, now + GESTURE_S + 0.1);
        assert!(!holds_merge(&app), "the gesture ends after a pause");
        handle(&mut app, &cc(ddj, 16, 1), now + 1.0);
        assert_eq!(app.engine.undo_history().len(), steps + 2, "the next turn is its own step");
    }

    #[test]
    fn two_controls_moved_together_are_one_undo_step() {
        let (mut app, ddj, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 16}), json!({"kind": "track_pan", "track": "Kick"}), json!({"mode": "rel_twos"}));
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 17}), json!({"kind": "track_volume", "track": "Bass"}), json!({"mode": "rel_twos"}));
        let steps = app.engine.undo_history().len();
        let mut now = 1.0;
        for i in 0..20 {
            frame(&mut app, now);
            handle(&mut app, &cc(ddj, if i % 2 == 0 { 16 } else { 17 }, 1), now);
            now += 0.02;
        }
        assert_eq!(app.engine.undo_history().len(), steps + 1, "interleaved, still one step");
    }

    #[test]
    fn an_unchanged_value_neither_edits_nor_holds_the_gesture() {
        let (mut app, ddj, _) = app();
        app.run("mix.pan", json!({"tracks": ["Kick"], "pan": 1.0})).unwrap();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 16}), json!({"kind": "track_pan", "track": "Kick"}), json!({"mode": "rel_twos"}));
        let rev = app.engine.revision;
        handle(&mut app, &cc(ddj, 16, 1), 0.0);
        assert_eq!(app.engine.revision, rev, "already at the top: nothing to do");
        assert!(!holds_merge(&app) && app.midi_learn.gesture.is_none());
    }

    #[test]
    fn a_new_mode_reads_the_control_afresh_and_removed_mappings_leave_no_state() {
        let (mut app, ddj, _) = app();
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "track_volume", "track": "Kick"}), json!({}));
        handle(&mut app, &cc(ddj, 7, 95), 0.0);
        let m = app.ui.midi_mappings[0].clone();
        assert!(!waiting_for_pickup(&app, &m), "picked up");
        app.ui.midi_mappings[0].mode = Mode::RelTwos;
        handle(&mut app, &cc(ddj, 7, 1), 0.0);
        app.ui.midi_mappings[0].mode = Mode::Absolute;
        let before = db(&app, "Kick");
        handle(&mut app, &cc(ddj, 7, 10), 0.0);
        assert_eq!(db(&app, "Kick"), before, "back to absolute: picked up again first");
        app.run("midi.unmap", json!({"all": true})).unwrap();
        frame(&mut app, 0.0);
        assert!(app.midi_learn.runtime.is_empty());
    }

    #[test]
    fn out_of_range_messages_are_refused_and_dropped_from_the_preferences() {
        let (mut app, _, _) = app();
        let bad = |m: Value| json!({"device": "DDJ", "matcher": m, "target": {"kind": "stop_all"}});
        assert!(app.run("midi.map", bad(json!({"type": "cc", "ch": 16, "cc": 1}))).is_err());
        assert!(app.run("midi.map", bad(json!({"type": "note", "ch": 0, "note": 128}))).is_err());
        let r = app
            .run(
                "midi.map",
                json!({"device": "DDJ", "matcher": {"type": "cc", "ch": 0, "cc": 1}, "target": {"kind": "stop_all"}, "sensitivity": 9.0}),
            )
            .unwrap();
        assert_eq!(r["mapping"]["sensitivity"].as_f64(), Some(f64::from(MAX_SENSITIVITY)), "clamped");
        let ui: crate::UiState = serde_json::from_value(json!({"midi_mappings": [
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "note", "ch": 0, "note": 200}, "target": {"kind": "stop_all"}},
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "note", "ch": 0, "note": 36}, "target": {"kind": "stop_all"}, "mode": "rel_twos", "sensitivity": -1.0}
        ]}))
        .unwrap();
        assert_eq!(ui.midi_mappings.len(), 1, "note 200 dropped");
        assert_eq!((ui.midi_mappings[0].mode, ui.midi_mappings[0].sensitivity), (Mode::Momentary, MIN_SENSITIVITY), "normalised");
    }

    #[test]
    fn encoders_and_knobs_are_told_apart_by_their_first_messages() {
        assert_eq!(classify(&[1, 1, 1, 1]), Mode::RelTwos, "repeated small steps up");
        assert_eq!(classify(&[127, 127, 126, 127]), Mode::RelTwos, "and down");
        assert_eq!(classify(&[65, 65, 66, 65]), Mode::RelOffset, "repeated around 64");
        assert_eq!(classify(&[63, 63, 62]), Mode::RelOffset);
        assert_eq!(classify(&[1, 65, 65, 1]), Mode::RelSignMag, "sign bit set and clear");
        assert_eq!(classify(&[20, 24, 29, 33]), Mode::Absolute, "distinct, moving one way");
        assert_eq!(classify(&[90, 80, 70]), Mode::Absolute);
        assert_eq!(classify(&[1, 2]), Mode::RelTwos, "only upward ticks");
        assert_eq!(classify(&[40]), Mode::Absolute);
    }

    #[test]
    fn a_learned_encoder_gets_its_mode_and_its_first_ticks_move_nothing() {
        let (mut app, ddj, _) = app();
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
        frame(&mut app, 1.0);
        app.run("midi.learn", json!({"target": {"kind": "track_volume", "track": "Kick"}})).unwrap();
        for _ in 0..DETECT_MSGS {
            handle(&mut app, &cc(ddj, 7, 127), 1.0);
        }
        assert!(app.midi_learn.detect.is_none(), "decided after {DETECT_MSGS} messages");
        assert_eq!(app.ui.midi_mappings[0].mode, Mode::RelTwos);
        assert_eq!(db(&app, "Kick"), 0.0, "the learning messages moved nothing");
        assert_eq!(app.midi_learn.overlay_mapping.as_deref(), Some(app.ui.midi_mappings[0].key().as_str()), "the overlay offers the mode");
        // A knob: a couple of distinct values, then the time runs out.
        app.run("midi.learn", json!({"target": {"kind": "track_volume", "track": "Bass"}})).unwrap();
        handle(&mut app, &cc(ddj, 8, 30), 1.0);
        handle(&mut app, &cc(ddj, 8, 33), 1.1);
        frame(&mut app, 1.0 + DETECT_S + 0.05);
        assert!(app.midi_learn.detect.is_none());
        assert_eq!(app.ui.midi_mappings[1].mode, Mode::Absolute);
        assert!(!app.midi_learn.check_direction);
        // Ticks only one way: its mode is a guess, and the overlay says so.
        app.run("midi.learn", json!({"target": {"kind": "track_volume", "track": "Verb"}})).unwrap();
        for _ in 0..DETECT_MSGS {
            handle(&mut app, &cc(ddj, 9, 1), 2.0);
        }
        assert_eq!(app.ui.midi_mappings[2].mode, Mode::RelTwos);
        assert!(app.midi_learn.check_direction);
    }

    #[test]
    fn the_waker_starts_only_once_midi_is_wanted() {
        let (mut app, _, _) = app();
        // An enabled input wants it too (the toolbar's activity light): none here.
        let enabled = std::mem::take(&mut app.ui.midi_inputs);
        let ctx = egui::Context::default();
        frame(&mut app, 0.0);
        wake_on_midi(&mut app, &ctx);
        assert!(!app.midi_host.as_ref().unwrap().has_waker(), "no mappings, no learn, no input enabled: no waker");
        app.ui.midi_inputs = enabled;
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "stop_all"}), json!({}));
        frame(&mut app, 0.1);
        wake_on_midi(&mut app, &ctx);
        assert!(app.midi_host.as_ref().unwrap().has_waker());
    }

    #[test]
    fn touch_automation_records_midi_moves_of_volume_pan_and_stems() {
        use soundcraft_model::AutoParam;
        let (mut app, ddj, _) = app();
        {
            let s = app.engine.session_mut();
            let t = s.track_by_name("Kick").unwrap().id;
            let clip = s.track(t).unwrap().clips().first().unwrap().id;
            let src = s.find_clip(clip).unwrap().1.source().unwrap();
            s.find_clip_mut(clip).unwrap().stems = Some(soundcraft_model::StemSet { key: "fake".into(), sources: [src; 4], scale_log2: 0 });
        }
        app.run("mix.automation_mode", json!({"tracks": ["Kick"], "mode": "touch"})).unwrap();
        for (cc_, target) in [
            (16, json!({"kind": "track_volume", "track": "Kick"})),
            (17, json!({"kind": "track_pan", "track": "Kick"})),
            (18, json!({"kind": "stem_fader", "track": "Kick", "stem": "drums"})),
        ] {
            map(&mut app, json!({"type": "cc", "ch": 0, "cc": cc_}), target, json!({"mode": "rel_twos", "sensitivity": 0.02}));
        }
        app.run("transport.play", json!({})).unwrap();
        let ctx = egui::Context::default();
        let mut now = 1.0;
        for i in 0..30u8 {
            app.handle_transport(1.0 / 30.0);
            frame(&mut app, now);
            handle(&mut app, &cc(ddj, 16 + i % 3, 127), now);
            app.write_automation(&ctx);
            now += 1.0 / 30.0;
        }
        app.run("transport.stop", json!({})).unwrap();
        app.handle_transport(0.0);
        app.write_automation(&ctx);
        let t = app.engine.session().track_by_name("Kick").unwrap();
        for p in [AutoParam::Volume, AutoParam::Pan(0), AutoParam::Stem(1)] {
            let n = t.lane(&p).map_or(0, |l| l.points.len());
            assert!(n >= 2, "{p:?}: {n} points");
        }
        assert!(t.lane(&AutoParam::Stem(0)).is_none_or(|l| l.points.is_empty()), "an untouched stem writes nothing");
    }

    #[test]
    fn a_missing_target_is_skipped_with_a_message_and_a_renamed_one_resolves_by_name() {
        let (mut app, ddj, _) = app();
        let src = app.midi_host.as_ref().unwrap().source();
        let device = DeviceKey { backend_id: src.backend_id(ddj).unwrap().into(), raw_name: "DDJ".into(), name: "DDJ".into() };
        let gone = Mapping {
            device,
            matcher: Matcher::Cc { ch: 0, cc: 7 },
            target: Target::TrackVolume { track: TrackRef { id: 999, name: "Gone".into() } },
            pickup: false,
            ..Mapping::default()
        };
        commit(&mut app, gone.clone(), false);
        handle(&mut app, &cc(ddj, 7, 64), 0.0);
        assert!(app.ui.status.contains("no track \"Gone\""), "{}", app.ui.status);
        // A mapping made in another session (other ids) finds its track by name.
        let kick = Mapping {
            matcher: Matcher::Cc { ch: 0, cc: 8 },
            target: Target::TrackVolume { track: TrackRef { id: 999, name: "Kick".into() } },
            ..gone
        };
        commit(&mut app, kick, false);
        handle(&mut app, &cc(ddj, 8, 0), 0.0);
        assert_eq!(db(&app, "Kick"), -144.0);
    }

    #[test]
    fn the_same_message_from_another_device_does_nothing() {
        let (mut app, _, keys) = app();
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "track_volume", "track": "Kick"}), json!({"pickup": false}));
        handle(&mut app, &cc(keys, 7, 0), 0.0);
        assert_eq!(db(&app, "Kick"), 0.0);
    }

    #[test]
    fn command_targets_run_their_command_on_a_press() {
        let (mut app, ddj, _) = app();
        map(&mut app, json!({"type": "note", "ch": 0, "note": 50}), json!({"kind": "command", "id": "window.transport", "params": {}}), json!({}));
        assert!(!app.ui.show_transport);
        handle(&mut app, &note(ddj, 50, true), 0.0);
        handle(&mut app, &note(ddj, 50, false), 0.0);
        assert!(app.ui.show_transport, "pressed: run");
        handle(&mut app, &note(ddj, 50, true), 0.0);
        assert!(!app.ui.show_transport, "pressed again: run again");
        map(&mut app, json!({"type": "note", "ch": 0, "note": 51}), json!({"kind": "command", "id": "no.such_command"}), json!({}));
        handle(&mut app, &note(ddj, 51, true), 0.0);
        assert!(app.ui.status.contains("no.such_command"), "{}", app.ui.status);
    }

    #[test]
    fn mappings_list_and_unmap() {
        let (mut app, _, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "track_volume", "track": "Kick"}), json!({}));
        map(&mut app, json!({"type": "note", "ch": 9, "note": 36}), json!({"kind": "slot_launch", "column": 0, "slot": 2}), json!({}));
        let v = app.run("midi.mappings", json!({})).unwrap();
        assert_eq!(v["mappings"][1]["message"], "Note 36 ch 10");
        assert_eq!(v["mappings"][1]["label"], "Live 1 slot 3");
        assert_eq!(v["mappings"][0]["target"]["track"]["name"], "Kick");
        assert!(
            app.run(
                "midi.map",
                json!({"device": "DDJ", "matcher": {"type": "note", "ch": 0, "note": 1}, "target": {"kind": "stop_all"}, "mode": "absolute"})
            )
            .is_err()
        );
        assert!(app.run("midi.map", json!({"device": "Nope", "matcher": {"type": "cc", "ch": 0, "cc": 1}, "target": {"kind": "stop_all"}})).is_err());
        assert_eq!(app.run("midi.unmap", json!({"target": {"kind": "track_volume", "track": "Kick"}})).unwrap()["removed"], 1);
        assert_eq!(app.run("midi.unmap", json!({"index": 0})).unwrap()["removed"], 1);
        assert!(app.run("midi.unmap", json!({"index": 0})).is_err());
    }

    #[test]
    fn removed_mappings_are_kept_for_undo_remove() {
        let (mut app, _, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "track_volume", "track": "Kick"}), json!({}));
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 8}), json!({"kind": "track_volume", "track": "Bass"}), json!({}));
        let r = app.run("midi.unmap", json!({"all": true})).unwrap();
        assert_eq!((r["removed"].as_u64(), r["backup"].as_u64()), (Some(2), Some(2)));
        assert!(app.ui.midi_mappings.is_empty());
        let back: crate::UiState = serde_json::from_value(serde_json::to_value(&app.ui).unwrap()).unwrap();
        assert_eq!(back.midi_mappings_backup.len(), 2, "the backup is in the preferences");
        assert_eq!(app.run("midi.restore_mappings", json!({})).unwrap()["restored"], 2);
        assert_eq!(app.ui.midi_mappings.len(), 2);
        // A single Delete has the same one-level restore.
        remove(&mut app, 0);
        assert_eq!((app.ui.midi_mappings.len(), app.ui.midi_mappings_backup.len()), (1, 1));
        restore(&mut app);
        assert_eq!(app.ui.midi_mappings.len(), 2);
        assert!(app.ui.midi_mappings_backup.is_empty());
    }

    /// Both decks with a (silent) track, at ±8 %, as the audio engine would report them.
    fn load_decks(app: &mut SoundApp) {
        for i in 0..2 {
            let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 4800]; 2] };
            let track = soundcraft_mix::DeckTrack {
                source: std::sync::Arc::new(soundcraft_model::SourceAudio::new(buf)),
                grid: None,
                bpm: Some(128.0),
                key: None,
                cues: [None; soundcraft_mix::HOT_CUES],
                title: String::new(),
                artist: String::new(),
                path: String::new(),
                grid_from: None,
                sections: Vec::new(),
            };
            app.engine.deck_tracks[i] = Some(std::sync::Arc::new(track));
            app.engine.deck_status.decks[i].load = 1;
            app.engine.deck_status.decks[i].range = 0.08;
        }
    }

    fn deck(d: &str, control: Value) -> Value {
        let mut t = json!({"kind": "deck", "deck": d});
        if let (Some(o), Some(c)) = (t.as_object_mut(), control.as_object()) {
            o.extend(c.clone());
        }
        t
    }

    fn run_(id: &'static str, p: Value) -> Action {
        Action::Run(id, p)
    }

    fn taken(app: &mut SoundApp) -> Vec<Action> {
        std::mem::take(&mut app.midi_learn.actions)
    }

    #[test]
    fn deck_buttons_act_as_the_layout_cue_press_release_hot_cues_and_toggles() {
        let (mut app, ddj, _) = app();
        load_decks(&mut app);
        map(&mut app, json!({"type": "note", "ch": 0, "note": 12}), deck("a", json!({"control": "cue"})), json!({}));
        map(&mut app, json!({"type": "note", "ch": 0, "note": 11}), deck("a", json!({"control": "play"})), json!({}));
        map(&mut app, json!({"type": "note", "ch": 0, "note": 20}), deck("b", json!({"control": "hot_cue", "n": 3})), json!({}));
        map(&mut app, json!({"type": "note", "ch": 0, "note": 21}), deck("b", json!({"control": "hot_cue_clear", "n": 3})), json!({}));
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 30}), deck("b", json!({"control": "sync"})), json!({}));
        assert_eq!(app.ui.midi_mappings[0].mode, Mode::Momentary, "a button");
        // CUE: press and release (CDJ); a second release sends nothing.
        handle(&mut app, &note(ddj, 12, true), 1.0);
        handle(&mut app, &note(ddj, 12, false), 1.0);
        handle(&mut app, &note(ddj, 12, false), 1.0);
        assert_eq!(
            taken(&mut app),
            vec![run_("deck.cue", json!({"deck": "a", "down": true})), run_("deck.cue", json!({"deck": "a", "down": false}))]
        );
        // PLAY toggles from the state asked for while the deck hasn't reported it.
        handle(&mut app, &note(ddj, 11, true), 1.0);
        handle(&mut app, &note(ddj, 11, false), 1.0);
        handle(&mut app, &note(ddj, 11, true), 1.05);
        assert_eq!(taken(&mut app), vec![run_("deck.play", json!({"deck": "a"})), run_("deck.pause", json!({"deck": "a"}))], "a quick double press");
        // A hot cue: empty sets (no release), set jumps and its release ends the preview.
        handle(&mut app, &note(ddj, 20, true), 1.0);
        handle(&mut app, &note(ddj, 20, false), 1.0);
        app.engine.deck_status.decks[1].hot[2] = Some(1000.0);
        handle(&mut app, &note(ddj, 20, true), 1.0);
        handle(&mut app, &note(ddj, 20, false), 1.0);
        handle(&mut app, &note(ddj, 21, true), 1.0);
        assert_eq!(
            taken(&mut app),
            vec![
                run_("deck.hotcue", json!({"deck": "b", "n": 3, "action": "set"})),
                run_("deck.hotcue", json!({"deck": "b", "n": 3, "action": "jump"})),
                run_("deck.hotcue", json!({"deck": "b", "n": 3, "action": "release"})),
                run_("deck.hotcue", json!({"deck": "b", "n": 3, "action": "clear"})),
            ]
        );
        // A controller button: on its way up only; the double press sends on then off.
        for v in [127, 0, 127, 0] {
            handle(&mut app, &cc(ddj, 30, v), 2.0);
        }
        assert_eq!(taken(&mut app), vec![run_("deck.sync", json!({"deck": "b", "on": true})), run_("deck.sync", json!({"deck": "b", "on": false}))]);
        // An empty deck plays nothing.
        app.engine.deck_tracks[0] = None;
        handle(&mut app, &note(ddj, 11, true), 9.0);
        assert!(taken(&mut app).is_empty());
    }

    #[test]
    fn a_learned_jog_adds_up_its_ticks_nudging_while_playing_and_searching_while_paused() {
        let (mut app, ddj, _) = app();
        load_decks(&mut app);
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 33}), deck("a", json!({"control": "jog"})), json!({"mode": "rel_offset"}));
        // Paused: 360 counts of +1 (half a turn at the layout's 720) is a beat of search.
        for _ in 0..360 {
            handle(&mut app, &cc(ddj, 33, 65), 1.0);
        }
        assert!(taken(&mut app).is_empty(), "sent once a frame");
        frame(&mut app, 1.016);
        assert_eq!(taken(&mut app), vec![run_("deck.jog", json!({"deck": "a", "delta": 1.0, "mode": "search"}))]);
        // Playing: a nudge as strong as the turn is fast.
        app.engine.deck_status.decks[0].playing = true;
        handle(&mut app, &cc(ddj, 33, 72), 1.02);
        frame(&mut app, 1.032);
        assert_eq!(taken(&mut app), vec![run_("deck.jog", json!({"deck": "a", "delta": 0.5, "mode": "nudge"}))]);
        // Sensitivity scales the counts.
        app.ui.midi_mappings[0].sensitivity = 0.02;
        handle(&mut app, &cc(ddj, 33, 68), 1.04);
        frame(&mut app, 1.048);
        assert_eq!(taken(&mut app), vec![run_("deck.jog", json!({"deck": "a", "delta": 0.5, "mode": "nudge"}))]);
        // A held pitch bend nudges every frame until let go.
        map(&mut app, json!({"type": "note", "ch": 0, "note": 40}), deck("a", json!({"control": "pitch_bend_up"})), json!({}));
        handle(&mut app, &note(ddj, 40, true), 2.0);
        frame(&mut app, 2.016);
        frame(&mut app, 2.032);
        handle(&mut app, &note(ddj, 40, false), 2.04);
        frame(&mut app, 2.05);
        let bend = run_("deck.jog", json!({"deck": "a", "delta": crate::deck_controls::BEND, "mode": "nudge"}));
        assert_eq!(taken(&mut app), vec![bend.clone(), bend]);
    }

    #[test]
    fn a_learned_pitch_waits_for_the_deck_tempo_and_a_14_bit_one_reads_both_halves() {
        let (mut app, ddj, _) = app();
        load_decks(&mut app);
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 9}), deck("a", json!({"control": "pitch"})), json!({}));
        // At 0 % the centre (64): far below waits, crossing picks it up.
        handle(&mut app, &cc(ddj, 9, 10), 1.0);
        handle(&mut app, &cc(ddj, 9, 30), 1.0);
        assert!(taken(&mut app).is_empty(), "not picked up");
        assert!(app.ui.status.contains("pick it up"), "{}", app.ui.status);
        handle(&mut app, &cc(ddj, 9, 100), 1.0);
        let sent = taken(&mut app);
        let Some(Action::Run("deck.pitch", p)) = sent.first() else { panic!("{sent:?}") };
        assert!((p["value"].as_f64().unwrap() - (100.0 / 127.0 * 2.0 - 1.0) * 8.0).abs() < 0.01, "{p}");
        // Synced to a master on the other deck: no takeover (the engine's own applies).
        app.engine.deck_status.decks[1].synced = true;
        app.engine.deck_status.decks[0].master = true;
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 10}), deck("b", json!({"control": "pitch"})), json!({}));
        handle(&mut app, &cc(ddj, 10, 0), 1.0);
        assert_eq!(taken(&mut app), vec![run_("deck.pitch", json!({"deck": "b", "value": -8.0}))]);
        // 14 bits: the MSB waits, the LSB sends; one LSB count is a step.
        map(&mut app, json!({"type": "cc", "ch": 1, "cc": 0}), deck("b", json!({"control": "fader"})), json!({"lsb": 32, "pickup": false}));
        let wide = |app: &mut SoundApp, v: u16| {
            for (cc_, x) in [(0u8, (v >> 7) as u8), (32, (v & 0x7F) as u8)] {
                handle(app, &MidiEvent { port: ddj, msg: MidiMessage::ControlChange { ch: 1, cc: cc_, value: x }, time_ns: 0 }, 1.0);
            }
            taken(app)
        };
        assert_eq!(wide(&mut app, 16383), vec![run_("deck.fader", json!({"deck": "b", "value": 1.0}))]);
        assert_eq!(wide(&mut app, 8192), vec![run_("deck.fader", json!({"deck": "b", "value": 0.5}))]);
        assert_ne!(wide(&mut app, 12288), wide(&mut app, 12300));
        // The mixer: a 7-bit crossfader with pickup from the centre.
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 11}), json!({"kind": "mixer", "control": "crossfader"}), json!({}));
        handle(&mut app, &cc(ddj, 11, 0), 1.0);
        assert!(taken(&mut app).is_empty(), "waits for the centre");
        handle(&mut app, &cc(ddj, 11, 64), 1.0);
        handle(&mut app, &cc(ddj, 11, 127), 1.0);
        assert_eq!(taken(&mut app).last(), Some(&Action::Run("deck.xfader", json!({"value": 1.0}))));
    }

    #[test]
    fn a_learned_hold_is_let_go_when_learning_starts() {
        let (mut app, ddj, _) = app();
        load_decks(&mut app);
        map(&mut app, json!({"type": "note", "ch": 0, "note": 12}), deck("a", json!({"control": "cue"})), json!({}));
        app.engine.deck_status.decks[0].hot[0] = Some(1.0);
        map(&mut app, json!({"type": "note", "ch": 0, "note": 13}), deck("a", json!({"control": "hot_cue", "n": 1})), json!({}));
        handle(&mut app, &note(ddj, 12, true), 1.0);
        handle(&mut app, &note(ddj, 13, true), 1.0);
        taken(&mut app);
        app.run("midi.learn", json!({"target": {"kind": "stop_all"}})).unwrap();
        // Let go through the app (the player): nothing is left held.
        assert!(!app.midi_learn.decks.holding());
        app.run("midi.learn", json!({"cancel": true})).unwrap();
        handle(&mut app, &note(ddj, 12, false), 1.1);
        handle(&mut app, &note(ddj, 13, false), 1.1);
        assert!(taken(&mut app).is_empty(), "the hardware's late releases send nothing more");
    }

    #[test]
    fn deck_targets_round_trip_through_the_preferences_and_old_ones_still_load() {
        let (mut app, _, _) = app();
        map(&mut app, json!({"type": "note", "ch": 0, "note": 12}), deck("a", json!({"control": "hot_cue", "n": 8})), json!({}));
        map(&mut app, json!({"type": "cc", "ch": 6, "cc": 31}), json!({"kind": "mixer", "control": "crossfader"}), json!({"lsb": 63}));
        map(&mut app, json!({"type": "cc", "ch": 6, "cc": 64}), json!({"kind": "library", "control": "browse"}), json!({"mode": "rel_twos"}));
        let saved = serde_json::to_value(&app.ui).unwrap();
        assert_eq!(saved["midi_mappings"][0]["target"], json!({"kind": "deck", "deck": "a", "control": "hot_cue", "n": 8}));
        assert!(saved["midi_mappings"][0].get("lsb").is_none(), "no LSB, not written");
        let back: crate::UiState = serde_json::from_value(saved).unwrap();
        assert_eq!(back.midi_mappings, app.ui.midi_mappings);
        // Preferences from before deck targets (no `lsb`), with a deck target a later version
        // might add and a hot cue out of range: the rest load.
        let old: crate::UiState = serde_json::from_value(json!({"midi_mappings": [
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "cc", "ch": 0, "cc": 7},
             "target": {"kind": "track_volume", "track": {"id": 1, "name": "Kick"}}, "mode": "absolute", "sensitivity": 0.01, "pickup": true},
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "note", "ch": 0, "note": 1},
             "target": {"kind": "deck", "deck": "a", "control": "warp"}},
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "note", "ch": 0, "note": 2},
             "target": {"kind": "deck", "deck": "a", "control": "hot_cue", "n": 9}}
        ]}))
        .unwrap();
        assert_eq!(old.midi_mappings.len(), 1);
        assert_eq!(old.midi_mappings[0].lsb, None);
        // The commands list, filter and forget deck targets.
        let v = app.run("midi.mappings", json!({"kind": "deck"})).unwrap();
        assert_eq!(v["mappings"].as_array().map(Vec::len), Some(1));
        assert_eq!(v["mappings"][0]["label"], "Deck A Hot Cue H");
        assert_eq!(v["mappings"][0]["resolved"], true);
        assert!(
            app.run(
                "midi.map",
                json!({"device": "DDJ", "matcher": {"type": "note", "ch": 0, "note": 1}, "target": deck("c", json!({"control": "play"}))})
            )
            .is_err()
        );
        assert!(
            app.run(
                "midi.map",
                json!({"device": "DDJ", "matcher": {"type": "note", "ch": 0, "note": 1}, "target": deck("a", json!({"control": "hot_cue", "n": 0}))})
            )
            .is_err()
        );
        let r = app.run("midi.unmap", json!({"target": deck("a", json!({"control": "hot_cue", "n": 8}))})).unwrap();
        assert_eq!(r["removed"], 1);
        assert!(params_doc("midi.map").is_some_and(|d| d.contains("hot_cue {n: 1..=8}") && d.contains("mixer {control")));
    }

    #[test]
    fn a_separate_control_on_cc_plus_32_is_not_taken_for_an_lsb_and_a_wrong_lsb_heals() {
        let (mut app, ddj, _) = app();
        // A nanoKONTROL2-like fader on CC 0 with a solo button on CC 32: the button pressed once
        // just after a fader move, once not.
        app.run("midi.learn", json!({"target": deck("a", json!({"control": "fader"}))})).unwrap();
        for (c, v) in [(0, 10), (32, 127), (0, 12), (0, 14), (32, 0), (0, 16)] {
            handle(&mut app, &cc(ddj, c, v), 1.0);
        }
        assert!(app.midi_learn.detect.is_none());
        assert_eq!((app.ui.midi_mappings[0].lsb, app.ui.midi_mappings[0].mode), (None, Mode::Absolute), "7-bit");
        // A real 14-bit pair: MSB then LSB, twice.
        app.run("midi.learn", json!({"target": deck("b", json!({"control": "fader"}))})).unwrap();
        for (c, v) in [(1, 64), (33, 5), (1, 64), (33, 40)] {
            handle(&mut app, &cc(ddj, c, v), 2.0);
        }
        frame(&mut app, 2.0 + DETECT_S + 0.1);
        assert_eq!(app.ui.midi_mappings[1].lsb, Some(33));
        // A wrong LSB: once its MSB has come alone 8 times, it is read as 7-bit.
        load_decks(&mut app);
        app.ui.midi_mappings[1].pickup = false;
        taken(&mut app);
        for _ in 0..LONE_MSBS {
            handle(&mut app, &cc(ddj, 1, 127), 3.0);
        }
        assert!(taken(&mut app).is_empty(), "the MSBs wait for an LSB a while");
        handle(&mut app, &cc(ddj, 1, 0), 3.0);
        assert_eq!(taken(&mut app), vec![run_("deck.fader", json!({"deck": "b", "value": 0.0}))], "then read as 7-bit");
        handle(&mut app, &cc(ddj, 33, 0), 3.0);
        taken(&mut app);
        handle(&mut app, &cc(ddj, 1, 64), 3.0);
        assert!(taken(&mut app).is_empty(), "an LSB again: the MSB waits for it again");
    }

    #[test]
    fn an_lsb_is_only_for_an_absolute_deck_knob_or_fader_and_invert_flips_it() {
        let (mut app, ddj, _) = app();
        let bad = |target: Value, extra: Value| {
            let mut p = json!({"device": "DDJ", "matcher": {"type": "cc", "ch": 0, "cc": 7}, "target": target, "lsb": 39});
            if let (Some(o), Some(x)) = (p.as_object_mut(), extra.as_object()) {
                o.extend(x.clone());
            }
            p
        };
        for (p, why) in [
            (bad(json!({"kind": "track_volume", "track": "Kick"}), json!({})), "deck or mixer"),
            (bad(deck("a", json!({"control": "jog"})), json!({})), "deck or mixer"),
            (bad(deck("a", json!({"control": "cue"})), json!({})), "deck or mixer"),
            (bad(deck("a", json!({"control": "fader"})), json!({"mode": "rel_twos"})), "absolute"),
            (bad(deck("a", json!({"control": "fader"})), json!({"lsb": 7})), "another controller"),
            (bad(deck("a", json!({"control": "fader"})), json!({"matcher": {"type": "note", "ch": 0, "note": 7}})), "not a note"),
        ] {
            let e = app.run("midi.map", p).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
        assert!(app.run("midi.map", bad(deck("a", json!({"control": "fader"})), json!({}))).is_ok());
        let ui: crate::UiState = serde_json::from_value(json!({"midi_mappings": [
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "cc", "ch": 0, "cc": 7},
             "target": {"kind": "track_volume", "track": {"id": 1, "name": "Kick"}}, "lsb": 39},
            {"device": {"backend_id": "b", "raw_name": "P", "name": "P"}, "matcher": {"type": "cc", "ch": 0, "cc": 7},
             "target": {"kind": "mixer", "control": "crossfader"}, "lsb": 39, "invert": true}
        ]}))
        .unwrap();
        assert_eq!(
            ui.midi_mappings.iter().map(|m| (m.lsb, m.invert)).collect::<Vec<_>>(),
            [(None, false), (Some(39), true)],
            "an impossible LSB dropped"
        );
        // Invert: a learned pitch fader whose top sends 127 is slower at the top.
        app.run("midi.unmap", json!({"all": true})).unwrap();
        load_decks(&mut app);
        app.run(
            "midi.map",
            json!({"device": "DDJ", "matcher": {"type": "cc", "ch": 0, "cc": 9}, "target": deck("a", json!({"control": "pitch"})), "pickup": false, "invert": true}),
        )
        .unwrap();
        handle(&mut app, &cc(ddj, 9, 127), 1.0);
        assert_eq!(taken(&mut app), vec![run_("deck.pitch", json!({"deck": "a", "value": -8.0}))]);
        assert_eq!(app.run("midi.mappings", json!({})).unwrap()["mappings"][0]["invert"], true);
        // And an existing fader target.
        app.run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
        map(
            &mut app,
            json!({"type": "cc", "ch": 0, "cc": 7}),
            json!({"kind": "track_volume", "track": "Kick"}),
            json!({"pickup": false, "invert": true}),
        );
        handle(&mut app, &cc(ddj, 7, 127), 1.0);
        assert_eq!(db(&app, "Kick"), -144.0, "the top is the bottom");
        let t = app.ui.midi_mappings[1].target.clone();
        set_invert(&mut app, &t, false);
        assert!(!app.ui.midi_mappings[1].invert);
    }

    #[test]
    fn an_absolute_browse_knob_wrapping_from_127_to_0_is_one_step_on() {
        let (mut app, ddj, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 33}), json!({"kind": "library", "control": "browse"}), json!({"mode": "absolute"}));
        for v in [126, 127, 0, 1, 0, 127] {
            handle(&mut app, &cc(ddj, 33, v), 1.0);
        }
        assert_eq!(taken(&mut app), [1, 1, 1, -1, -1].map(Action::Browse).to_vec());
    }

    #[test]
    fn learning_a_deck_knob_from_a_layout_knows_both_halves_and_a_generic_lsb_is_detected() {
        let (mut app, ddj, _) = app();
        // A device without a layout: the LSB (CC + 32) seen while the knob is watched.
        app.run("midi.learn", json!({"target": deck("a", json!({"control": "eq_low"}))})).unwrap();
        for (c, v) in [(15, 64), (47, 3), (15, 64), (47, 9), (15, 64), (47, 20)] {
            handle(&mut app, &cc(ddj, c, v), 1.0);
        }
        frame(&mut app, 1.0 + DETECT_S + 0.1);
        assert!(app.midi_learn.detect.is_none());
        let m = &app.ui.midi_mappings[0];
        assert_eq!((m.matcher, m.lsb, m.mode), (Matcher::Cc { ch: 0, cc: 15 }, Some(47), Mode::Absolute), "a repeated MSB isn't an encoder");
        assert!(controls(&app).contains(&(ddj, MidiControl::Cc { ch: 0, cc: 47 })), "the LSB never plays the instrument");
    }

    #[test]
    fn remove_all_in_the_window_asks_first() {
        let (mut app, _, _) = app();
        map(&mut app, json!({"type": "cc", "ch": 0, "cc": 7}), json!({"kind": "stop_all"}), json!({}));
        app.ui.show_midi_mappings = true;
        let ctx = egui::Context::default();
        // Window titles are set in the app's bold face.
        crate::fonts::install(&ctx);
        let frame = |app: &mut SoundApp, events: Vec<egui::Event>| {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
                events,
                ..Default::default()
            };
            ctx.run_ui(raw, |ui| show(app, ui.ctx())).textures_delta.clear();
        };
        frame(&mut app, vec![]);
        app.midi_learn.confirm_remove_all = true;
        frame(&mut app, vec![]);
        assert_eq!(app.ui.midi_mappings.len(), 1, "nothing removed until answered");
        assert!(app.midi_learn.confirm_remove_all, "the question is up");
    }
}
