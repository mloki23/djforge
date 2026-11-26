//! The built-in layout for the Pioneer DJ DDJ-FLX4 (two decks, a two-channel mixer, 8 pads a
//! deck). The messages are from Pioneer's own *List of MIDI messages* (Ver 1.0); the hardware
//! doc, `docs/hardware/ddj-flx4.md`, lists them with what djforge does with each, and a test
//! keeps this table and that doc in step.
//!
//! The layout reads the controller's messages and turns them into deck and mixer commands
//! ([`Action`]s), and mirrors the decks' state onto the controller's lights ([`Light`]s). It holds
//! no app state of its own besides what the hardware needs remembered: the 14-bit MSBs, the
//! pad mode, held pads and cues, jog counts waiting for the frame, and the tempo slider's soft
//! takeover.

use std::collections::HashMap;

use serde_json::json;
use soundcraft_midi_io::MidiMessage;
use soundcraft_mix::{DeckId, EqBand, PITCH_RANGES};
use soundcraft_playback::MidiControl;

use crate::controller_layouts::{Action, Controller, Ctx, DeckCtx, Light};

/// The layout's name, as the MIDI Devices window shows it.
pub const NAME: &str = "DDJ-FLX4";

/// Whether an input's raw name is a DDJ-FLX4 (any case: CoreMIDI and ALSA add to the name).
pub fn recognise(raw_name: &str) -> bool {
    raw_name.to_ascii_uppercase().contains("DDJ-FLX4")
}

/// A note or a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Note,
    Cc,
}

/// Which SHIFT layer a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shift {
    No,
    Yes,
    /// The same message with or without SHIFT.
    Either,
}

/// The pad modes, in the order of their note blocks (mode `k`'s pad 1 is note `16 k`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PadMode {
    #[default]
    HotCue,
    PadFx1,
    BeatJump,
    Sampler,
    Keyboard,
    PadFx2,
    BeatLoop,
    KeyShift,
}

impl PadMode {
    pub const ALL: [PadMode; 8] = [
        PadMode::HotCue,
        PadMode::PadFx1,
        PadMode::BeatJump,
        PadMode::Sampler,
        PadMode::Keyboard,
        PadMode::PadFx2,
        PadMode::BeatLoop,
        PadMode::KeyShift,
    ];

    /// The doc's name for the mode.
    pub fn id(self) -> &'static str {
        match self {
            PadMode::HotCue => "hot_cue",
            PadMode::PadFx1 => "pad_fx1",
            PadMode::BeatJump => "beat_jump",
            PadMode::Sampler => "sampler",
            PadMode::Keyboard => "keyboard",
            PadMode::PadFx2 => "pad_fx2",
            PadMode::BeatLoop => "beat_loop",
            PadMode::KeyShift => "key_shift",
        }
    }

    /// Pad 1's note.
    pub fn base(self) -> u8 {
        16 * (self as u8)
    }

    /// The mode button's LED note (the button that selects it, SHIFT layer for the second mode
    /// of a button).
    fn button(self) -> u8 {
        match self {
            PadMode::HotCue => 0x1B,
            PadMode::Keyboard => 0x69,
            PadMode::PadFx1 => 0x1E,
            PadMode::PadFx2 => 0x6B,
            PadMode::BeatJump => 0x20,
            PadMode::BeatLoop => 0x6D,
            PadMode::Sampler => 0x22,
            PadMode::KeyShift => 0x6F,
        }
    }

    /// Whether djforge does something with the pads in this mode.
    pub fn mapped(self) -> bool {
        matches!(self, PadMode::HotCue | PadMode::BeatJump | PadMode::BeatLoop)
    }

    fn of_note(note: u8) -> Option<PadMode> {
        PadMode::ALL.get(usize::from(note >> 4)).copied()
    }
}

/// The pad channel (1–16) of a deck and SHIFT layer: 8/9 for Deck 1, 10/11 for Deck 2.
pub fn pad_channel(deck: DeckId, shift: bool) -> u8 {
    8 + 2 * (deck.index() as u8) + u8::from(shift)
}

/// Beat jump pads, in beats: pairs back and on (◁1 1▷ ◁4 4▷ / ◁16 16▷ ◁32 32▷).
pub const JUMP_PADS: [f64; 8] = [-1.0, 1.0, -4.0, 4.0, -16.0, 16.0, -32.0, 32.0];
/// Beat loop pads, in beats.
pub const LOOP_PADS: [f64; 8] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0];

/// What a control does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ctl {
    Play,
    Cue,
    /// SHIFT + CUE: back to the start.
    ToStart,
    Shift,
    /// The platter turned: fine search paused, nudge playing.
    JogTop,
    /// SHIFT + platter: fast search.
    JogFast,
    JogTouch,
    /// The wheel's side: nudge (search while paused).
    JogSide,
    LoopIn,
    LoopOut,
    /// 4 BEAT / EXIT.
    Loop4,
    Reloop,
    /// CUE/LOOP CALL ◁: halve while looping, else jump back.
    CallBack,
    /// CUE/LOOP CALL ▷: double while looping, else jump on.
    CallFwd,
    Halve,
    Double,
    Sync,
    SyncOff,
    TempoRange,
    Tempo,
    Mode(PadMode),
    MasterLevel,
    Trim,
    Eq(EqBand),
    Cfx,
    HeadphoneCue,
    /// MASTER CUE: the master joins the cue bus (toggles).
    MasterCue,
    /// HEADPHONE MIX: cue ↔ master.
    HeadphoneMix,
    /// HEADPHONE LEVEL: djforge's headphone level, only with the device's option on
    /// ([`Ctx::headphone_knob`]); else the hardware's own.
    HeadphoneLevel,
    Fader,
    Crossfader,
    /// The browse knob, rows per click.
    Browse(isize),
    BrowsePress,
    Load,
    /// Claimed (it never plays the instrument) but does nothing yet.
    Unmapped,
}

/// One input message of the controller.
#[derive(Debug, Clone, Copy)]
pub struct InputDef {
    /// The hardware doc's id.
    pub id: &'static str,
    pub deck: Option<DeckId>,
    pub shift: Shift,
    pub kind: Kind,
    /// MIDI channel 1–16, as Pioneer numbers it.
    pub ch: u8,
    pub num: u8,
    /// A 14-bit control's LSB controller.
    pub lsb: Option<u8>,
    pub ctl: Ctl,
}

const A: Option<DeckId> = Some(DeckId::A);
const B: Option<DeckId> = Some(DeckId::B);

const fn def(id: &'static str, deck: Option<DeckId>, shift: Shift, kind: Kind, ch: u8, num: u8, lsb: Option<u8>, ctl: Ctl) -> InputDef {
    InputDef { id, deck, shift, kind, ch, num, lsb, ctl }
}

/// A button (a note).
const fn note(id: &'static str, deck: Option<DeckId>, shift: Shift, ch: u8, num: u8, ctl: Ctl) -> InputDef {
    def(id, deck, shift, Kind::Note, ch, num, None, ctl)
}

/// A relative controller.
const fn cc(id: &'static str, deck: Option<DeckId>, shift: Shift, ch: u8, num: u8, ctl: Ctl) -> InputDef {
    def(id, deck, shift, Kind::Cc, ch, num, None, ctl)
}

/// A 14-bit knob or fader: MSB `num`, LSB `num + 32`.
const fn wide(id: &'static str, deck: Option<DeckId>, shift: Shift, ch: u8, num: u8, ctl: Ctl) -> InputDef {
    def(id, deck, shift, Kind::Cc, ch, num, Some(num + 32), ctl)
}

use Shift::{Either, No, Yes};

/// The deck section and the per-deck mixer controls of one deck (its channel `ch`).
macro_rules! deck {
    ($d:expr, $ch:expr, $cfx:expr) => {
        [
            note("play", $d, No, $ch, 0x0B, Ctl::Play),
            note("play_shift", $d, Yes, $ch, 0x0E, Ctl::Unmapped),
            note("cue", $d, No, $ch, 0x0C, Ctl::Cue),
            note("cue_shift", $d, Yes, $ch, 0x48, Ctl::ToStart),
            note("shift", $d, No, $ch, 0x3F, Ctl::Shift),
            cc("jog_top", $d, No, $ch, 0x22, Ctl::JogTop),
            cc("jog_top_vinyl_off", $d, No, $ch, 0x23, Ctl::JogTop),
            cc("jog_top_shift", $d, Yes, $ch, 0x29, Ctl::JogFast),
            note("jog_touch", $d, No, $ch, 0x36, Ctl::JogTouch),
            note("jog_touch_shift", $d, Yes, $ch, 0x67, Ctl::JogTouch),
            cc("jog_side", $d, Either, $ch, 0x21, Ctl::JogSide),
            note("loop_in", $d, No, $ch, 0x10, Ctl::LoopIn),
            note("loop_in_shift", $d, Yes, $ch, 0x4C, Ctl::Unmapped),
            note("loop_out", $d, No, $ch, 0x11, Ctl::LoopOut),
            note("loop_out_shift", $d, Yes, $ch, 0x4E, Ctl::Unmapped),
            note("loop_4beat", $d, No, $ch, 0x4D, Ctl::Loop4),
            note("loop_4beat_shift", $d, Yes, $ch, 0x50, Ctl::Reloop),
            note("loop_call_back", $d, No, $ch, 0x51, Ctl::CallBack),
            note("loop_call_back_shift", $d, Yes, $ch, 0x3E, Ctl::Halve),
            note("loop_call_fwd", $d, No, $ch, 0x53, Ctl::CallFwd),
            note("loop_call_fwd_shift", $d, Yes, $ch, 0x3D, Ctl::Double),
            note("sync", $d, No, $ch, 0x58, Ctl::Sync),
            note("sync_long", $d, No, $ch, 0x5C, Ctl::SyncOff),
            note("sync_shift", $d, Yes, $ch, 0x60, Ctl::TempoRange),
            wide("tempo", $d, Either, $ch, 0x00, Ctl::Tempo),
            note("mode_hot_cue", $d, No, $ch, 0x1B, Ctl::Mode(PadMode::HotCue)),
            note("mode_keyboard", $d, Yes, $ch, 0x69, Ctl::Mode(PadMode::Keyboard)),
            note("mode_pad_fx1", $d, No, $ch, 0x1E, Ctl::Mode(PadMode::PadFx1)),
            note("mode_pad_fx2", $d, Yes, $ch, 0x6B, Ctl::Mode(PadMode::PadFx2)),
            note("mode_beat_jump", $d, No, $ch, 0x20, Ctl::Mode(PadMode::BeatJump)),
            note("mode_beat_loop", $d, Yes, $ch, 0x6D, Ctl::Mode(PadMode::BeatLoop)),
            note("mode_sampler", $d, No, $ch, 0x22, Ctl::Mode(PadMode::Sampler)),
            note("mode_key_shift", $d, Yes, $ch, 0x6F, Ctl::Mode(PadMode::KeyShift)),
            wide("trim", $d, No, $ch, 0x04, Ctl::Trim),
            wide("eq_hi", $d, No, $ch, 0x07, Ctl::Eq(EqBand::High)),
            wide("eq_mid", $d, No, $ch, 0x0B, Ctl::Eq(EqBand::Mid)),
            wide("eq_low", $d, No, $ch, 0x0F, Ctl::Eq(EqBand::Low)),
            wide("cfx", $d, No, 7, $cfx, Ctl::Cfx),
            note("ch_cue", $d, No, $ch, 0x54, Ctl::HeadphoneCue),
            note("ch_cue_shift", $d, Yes, $ch, 0x68, Ctl::Unmapped),
            wide("fader", $d, No, $ch, 0x13, Ctl::Fader),
            note("fader_start_play", $d, Yes, $ch, 0x66, Ctl::Unmapped),
            note("fader_start_cue", $d, Yes, $ch, 0x52, Ctl::Unmapped),
            note("load", $d, No, 7, 0x46 + $ch - 1, Ctl::Load),
        ]
    };
}

const DECK_A: [InputDef; 44] = deck!(A, 1, 0x17);
const DECK_B: [InputDef; 44] = deck!(B, 2, 0x18);

/// The controls that aren't per deck (effects, master, browse), and the per-deck ones the
/// macro doesn't cover (the SHIFT loads, whose notes don't follow the deck).
const SHARED: [InputDef; 31] = [
    note("fx_ch_select_10", None, No, 5, 0x10, Ctl::Unmapped),
    note("fx_ch_select_11", None, No, 5, 0x11, Ctl::Unmapped),
    note("fx_ch_select_10", None, No, 6, 0x10, Ctl::Unmapped),
    note("fx_ch_select_11", None, No, 6, 0x11, Ctl::Unmapped),
    note("fx_select", None, No, 5, 0x63, Ctl::Unmapped),
    note("fx_select_shift", None, Yes, 5, 0x64, Ctl::Unmapped),
    note("fx_beat_back", None, No, 5, 0x4A, Ctl::Unmapped),
    note("fx_beat_back_shift", None, Yes, 5, 0x66, Ctl::Unmapped),
    note("fx_beat_fwd", None, No, 5, 0x4B, Ctl::Unmapped),
    note("fx_beat_fwd_shift", None, Yes, 5, 0x6B, Ctl::Unmapped),
    wide("fx_level", None, No, 5, 0x02, Ctl::Unmapped),
    note("fx_on", None, No, 5, 0x47, Ctl::Unmapped),
    note("fx_on", None, No, 6, 0x47, Ctl::Unmapped),
    note("fx_on_shift", None, Yes, 5, 0x43, Ctl::Unmapped),
    note("fx_on_shift", None, Yes, 6, 0x43, Ctl::Unmapped),
    wide("master_level", None, No, 7, 0x08, Ctl::MasterLevel),
    note("master_cue", None, No, 7, 0x63, Ctl::MasterCue),
    note("master_cue_shift", None, Yes, 7, 0x78, Ctl::Unmapped),
    wide("crossfader", None, No, 7, 0x1F, Ctl::Crossfader),
    wide("mic_level", None, No, 7, 0x05, Ctl::Unmapped),
    note("smart_cfx", None, No, 7, 0x00, Ctl::Unmapped),
    note("smart_cfx_shift", None, Yes, 7, 0x08, Ctl::Unmapped),
    wide("headphone_mix", None, No, 7, 0x0C, Ctl::HeadphoneMix),
    wide("headphone_level", None, No, 7, 0x0D, Ctl::HeadphoneLevel),
    note("smart_fader", None, No, 7, 0x01, Ctl::Unmapped),
    note("smart_fader_shift", None, Yes, 7, 0x09, Ctl::Unmapped),
    note("android_mono_stereo", None, No, 7, 0x6D, Ctl::Unmapped),
    cc("browse", None, No, 7, 0x40, Ctl::Browse(1)),
    cc("browse_shift", None, Yes, 7, 0x64, Ctl::Browse(8)),
    note("browse_press", None, No, 7, 0x41, Ctl::BrowsePress),
    note("browse_press_shift", None, Yes, 7, 0x42, Ctl::Unmapped),
];

const LOAD_SHIFT: [InputDef; 2] = [note("load_shift", A, Yes, 7, 0x68, Ctl::Unmapped), note("load_shift", B, Yes, 7, 0x7A, Ctl::Unmapped)];

/// Every input message except the performance pads (see [`pads`]).
pub fn inputs() -> impl Iterator<Item = &'static InputDef> {
    DECK_A.iter().chain(DECK_B.iter()).chain(SHARED.iter()).chain(LOAD_SHIFT.iter())
}

/// Every pad block: (mode, SHIFT, deck, channel 1–16). Pad `n` (1–8) is note `mode.base() + n − 1`.
pub fn pads() -> impl Iterator<Item = (PadMode, bool, DeckId, u8)> {
    DeckId::ALL.into_iter().flat_map(|d| PadMode::ALL.into_iter().flat_map(move |m| [false, true].map(|s| (m, s, d, pad_channel(d, s)))))
}

/// A 14-bit value as 0..=1.
fn unit(v: u16) -> f64 {
    f64::from(v.min(16383)) / 16383.0
}

/// A bipolar 14-bit value as −1..=1, with ±32 counts around the centre read as 0.
fn bipolar(v: u16) -> f64 {
    let x = (f64::from(v.min(16383)) - 8191.5) / 8191.5;
    if x.abs() < 32.0 / 8191.5 { 0.0 } else { x.clamp(-1.0, 1.0) }
}

fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// Jog counts per revolution (assumed: the list doesn't say).
pub const JOG_TICKS: f64 = 720.0;
/// Beats a full platter turn searches, paused.
pub const SEARCH_BEATS_PER_TURN: f64 = 2.0;
/// Beats a full turn searches with SHIFT.
pub const FAST_BEATS_PER_TURN: f64 = 32.0;
/// Jog counts a second for a full nudge (10 % bend), as the on-screen jog's drag speed.
pub const NUDGE_TICKS_S: f64 = 1000.0;
/// Seconds after a tempo move before a deck pitch elsewhere re-arms the soft takeover (the
/// published state lags the command).
const TAKEOVER_SETTLE_S: f64 = 0.3;
/// Below this the level meter is dark.
const METER_FLOOR_DB: f32 = -24.0;
/// How fast the meter falls, dB a second.
const METER_FALL_DB_S: f32 = 30.0;
/// Positions (samples) this close to the cue point count as at it.
const AT_CUE: f64 = 256.0;

/// The tempo slider's soft takeover for one deck.
#[derive(Debug, Default, Clone, Copy)]
struct Pickup {
    picked: bool,
    /// The slider's last reading (percent).
    hw: Option<f64>,
    /// The last pitch sent (percent) and when.
    sent: Option<(f64, f64)>,
}

/// The DDJ-FLX4's layout state (see the module docs).
#[derive(Debug, Default)]
pub struct Flx4 {
    /// The last MSB per (channel 0–15, controller).
    msb: HashMap<(u8, u8), u8>,
    shift: [bool; 2],
    /// The platter touched (a nudge held: the UI then runs at a full frame rate).
    touch: [bool; 2],
    /// The toggles last asked for, per deck: play, sync, headphone cue.
    pending: [[Option<(bool, f64)>; 3]; 2],
    mode: [PadMode; 2],
    /// A hot cue pad held after a jump, which sends its release.
    held_pad: [Option<usize>; 2],
    cue_held: [bool; 2],
    search: [f64; 2],
    nudge: [i64; 2],
    tempo: [Pickup; 2],
    meter_db: [f32; 2],
    meter_at: Option<f64>,
    loads: [u64; 2],
    resend: bool,
}

/// A deck command with its deck.
fn run(id: &'static str, d: DeckId, mut p: serde_json::Value) -> Action {
    if let Some(o) = p.as_object_mut() {
        o.insert("deck".into(), json!(d.id()));
    }
    Action::Run(id, p)
}

/// How long a toggle's wanted state counts over what the deck reports (the report lags).
const PENDING_S: f64 = 0.5;

/// A toggle pressed: the new state, from the one asked for last if the deck hasn't reported it
/// yet, else from the reported one, so a quick double press sends on then off, not on twice.
fn toggle(pending: &mut Option<(bool, f64)>, reported: bool, now: f64) -> bool {
    let base = match *pending {
        Some((want, at)) if now - at < PENDING_S && (0.0..).contains(&(now - at)) && want != reported => want,
        _ => reported,
    };
    *pending = Some((!base, now));
    !base
}

/// Whether deck `d`'s fader follows the other deck's tempo: the engine's own takeover applies.
fn follows(ctx: &Ctx, d: DeckId) -> bool {
    ctx.deck(d).state.synced && ctx.deck(d.other()).state.master
}

/// The meter value for a peak (dB): see the hardware doc.
pub fn meter_value(db: f32) -> u8 {
    const POINTS: [(f32, f32); 5] = [(-24.0, 38.0), (-12.0, 65.0), (-6.0, 87.0), (-3.0, 101.0), (0.0, 119.0)];
    if !db.is_finite() || db < METER_FLOOR_DB {
        return 0;
    }
    if db > 0.0 {
        return 0x7F;
    }
    let v = POINTS.windows(2).find_map(|w| match w {
        [(d0, v0), (d1, v1)] if db <= *d1 => Some(v0 + (db - d0) / (d1 - d0) * (v1 - v0)),
        _ => None,
    });
    v.unwrap_or(119.0).round().clamp(0.0, 127.0) as u8
}

fn note_on(ch1: u8, note: u8, on: bool) -> MidiMessage {
    MidiMessage::NoteOn { ch: ch1.saturating_sub(1) & 0x0F, note, vel: if on { 0x7F } else { 0 } }
}

impl Flx4 {
    pub fn new() -> Flx4 {
        Flx4 { meter_db: [f32::NEG_INFINITY; 2], ..Flx4::default() }
    }

    /// The deck's pad mode, as last selected.
    pub fn mode(&self, d: DeckId) -> PadMode {
        self.mode.get(d.index()).copied().unwrap_or_default()
    }

    fn set_mode(&mut self, d: DeckId, m: PadMode) {
        if let Some(x) = self.mode.get_mut(d.index())
            && *x != m
        {
            *x = m;
            // The pads show the new mode's notes: send them afresh.
            self.resend = true;
        }
    }

    fn find(kind: Kind, ch1: u8, num: u8) -> Option<(&'static InputDef, bool)> {
        inputs().find_map(|d| {
            if d.kind != kind || d.ch != ch1 {
                None
            } else if d.num == num {
                Some((d, false))
            } else if d.lsb == Some(num) {
                Some((d, true))
            } else {
                None
            }
        })
    }

    fn pad(&mut self, ch1: u8, note: u8, down: bool, ctx: &Ctx) -> Option<Vec<Action>> {
        let (d, shift) = match ch1 {
            8 => (DeckId::A, false),
            9 => (DeckId::A, true),
            10 => (DeckId::B, false),
            11 => (DeckId::B, true),
            _ => return None,
        };
        let mode = PadMode::of_note(note)?;
        let pad = usize::from(note & 0x0F);
        if pad >= 8 {
            return None;
        }
        let mut out = Vec::new();
        if down {
            self.set_mode(d, mode);
        }
        let dc = ctx.deck(d);
        if !dc.loaded {
            return Some(out);
        }
        let i = d.index();
        match (mode, shift, down) {
            (PadMode::HotCue, false, true) => {
                let set = dc.state.hot.get(pad).copied().flatten().is_some();
                out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": if set { "jump" } else { "set" }})));
                if let Some(h) = self.held_pad.get_mut(i) {
                    *h = set.then_some(pad);
                }
            }
            // The release may come on the SHIFT channel: SHIFT pressed while the pad was held.
            (PadMode::HotCue, _, false) => {
                if let Some(h) = self.held_pad.get_mut(i)
                    && *h == Some(pad)
                {
                    *h = None;
                    out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": "release"})));
                }
            }
            (PadMode::HotCue, true, true) => out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": "clear"}))),
            (PadMode::BeatJump, _, true) => {
                if let Some(b) = JUMP_PADS.get(pad) {
                    out.push(run("deck.beatjump", d, json!({"beats": b})));
                }
            }
            (PadMode::BeatLoop, _, true) => {
                if let Some(b) = LOOP_PADS.get(pad) {
                    if dc.state.loop_on && dc.loop_beats.is_some_and(|x| (x - b).abs() < 1e-6) {
                        out.push(run("deck.loop", d, json!({"action": "exit"})));
                    } else {
                        out.push(run("deck.loop", d, json!({"beats": b})));
                    }
                }
            }
            _ => {}
        }
        Some(out)
    }

    /// A 14-bit control's full value, or `None` (an MSB, kept for its LSB; an LSB before any MSB).
    fn wide_value(&mut self, ch0: u8, def: &InputDef, lsb: bool, v: u8) -> Option<u16> {
        if !lsb {
            self.msb.insert((ch0, def.num), v);
            return None;
        }
        let msb = *self.msb.get(&(ch0, def.num))?;
        Some(u16::from(msb & 0x7F) << 7 | u16::from(v & 0x7F))
    }

    fn tempo(&mut self, d: DeckId, v: u16, ctx: &Ctx) -> Option<Action> {
        let dc = ctx.deck(d);
        let range = if dc.state.range > 0.0 { dc.state.range } else { 0.08 } * 100.0;
        let pct = round(bipolar(v) * range, 3);
        let p = self.tempo.get_mut(d.index())?;
        let prev = p.hw.replace(pct);
        if !dc.loaded {
            return None;
        }
        if !follows(ctx, d) && !p.picked {
            let cur = dc.state.pitch * 100.0;
            let window = range * 2.0 * 1.5 / 127.0;
            let crossed = prev.is_some_and(|h| (h - cur).signum() != (pct - cur).signum());
            if (pct - cur).abs() > window && !crossed {
                return None;
            }
            p.picked = true;
        }
        p.sent = Some((pct, ctx.now));
        Some(run("deck.pitch", d, json!({"value": pct})))
    }

    fn act(&mut self, def: &InputDef, down: bool, value: u8, wide: Option<u16>, ctx: &Ctx) -> Vec<Action> {
        // The shared controls (browse, master, crossfader) read no deck: A stands in.
        let d = def.deck.unwrap_or(DeckId::A);
        let dc = ctx.deck(d);
        let s = &dc.state;
        let i = d.index();
        let ticks = i64::from(value) - 64;
        let mut out = Vec::new();
        let press = down && def.kind == Kind::Note;
        match def.ctl {
            Ctl::Shift => {
                if let Some(x) = self.shift.get_mut(i) {
                    *x = down;
                }
            }
            Ctl::JogTouch => {
                if let Some(x) = self.touch.get_mut(i) {
                    *x = down;
                }
            }
            Ctl::Mode(m) if press => self.set_mode(d, m),
            Ctl::Browse(rows) => {
                let delta = if value >= 64 { i64::from(value) - 128 } else { i64::from(value) };
                out.push(Action::Browse(isize::try_from(delta).unwrap_or(0).saturating_mul(rows)));
            }
            Ctl::BrowsePress if press => out.push(Action::Load(None)),
            Ctl::Load if press => out.push(Action::Load(def.deck)),
            Ctl::MasterLevel => {
                if let Some(v) = wide {
                    let t = unit(v);
                    let db = if t <= 0.0 { -60.0 } else { ((t - 1.0) * 40.0).max(-60.0) };
                    out.push(Action::Run("deck.master_volume", json!({"db": round(db, 1)})));
                }
            }
            Ctl::MasterCue if press => out.push(Action::Run("deck.master_cue", json!({}))),
            Ctl::HeadphoneMix => {
                if let Some(v) = wide {
                    out.push(Action::Run("deck.headphone_mix", json!({"value": round(unit(v), 3)})));
                }
            }
            Ctl::HeadphoneLevel => {
                if let Some(v) = wide.filter(|_| ctx.headphone_knob) {
                    let (lo, hi) = soundcraft_mix::HEADPHONE_DB;
                    let db = f64::from(lo) + unit(v) * f64::from(hi - lo);
                    out.push(Action::Run("deck.headphone_level", json!({"db": round(db, 1)})));
                }
            }
            Ctl::Crossfader => {
                if let Some(v) = wide {
                    out.push(Action::Run("deck.xfader", json!({"value": round(unit(v) * 2.0 - 1.0, 3)})));
                }
            }
            Ctl::Trim => {
                if let Some(v) = wide {
                    out.push(run("deck.trim", d, json!({"db": round(f64::from(crate::knob::trim_db(unit(v) as f32)), 1)})));
                }
            }
            Ctl::Eq(band) => {
                if let Some(v) = wide {
                    let id = match band {
                        EqBand::Low => "low",
                        EqBand::Mid => "mid",
                        EqBand::High => "high",
                    };
                    out.push(run("deck.eq", d, json!({"band": id, "db": round(f64::from(crate::knob::eq_db(unit(v) as f32)), 1)})));
                }
            }
            Ctl::Cfx => {
                if let Some(v) = wide {
                    out.push(run("deck.filter", d, json!({"value": round(bipolar(v), 3)})));
                }
            }
            Ctl::Fader => {
                if let Some(v) = wide {
                    out.push(run("deck.fader", d, json!({"value": round(unit(v), 3)})));
                }
            }
            Ctl::HeadphoneCue if press => {
                let on = self.pending.get_mut(i).and_then(|p| p.get_mut(2)).map_or(!s.headphone_cue, |p| toggle(p, s.headphone_cue, ctx.now));
                out.push(run("deck.headphone_cue", d, json!({"on": on})));
            }
            // A held CUE let go, also on SHIFT+CUE's note (SHIFT pressed while it was held).
            Ctl::Cue | Ctl::ToStart if !down && self.cue_held.get(i).copied().unwrap_or(false) => {
                if let Some(x) = self.cue_held.get_mut(i) {
                    *x = false;
                }
                out.push(run("deck.cue", d, json!({"down": false})));
            }
            Ctl::Tempo => {
                if let Some(v) = wide {
                    out.extend(self.tempo(d, v, ctx));
                }
            }
            // The rest act on a loaded deck only.
            _ if !dc.loaded => {}
            Ctl::Play if press => {
                let play = self.pending.get_mut(i).and_then(|p| p.first_mut()).map_or(!s.playing, |p| toggle(p, s.playing, ctx.now));
                out.push(run(if play { "deck.play" } else { "deck.pause" }, d, json!({})));
            }
            Ctl::Cue if down && !self.cue_held.get(i).copied().unwrap_or(false) => {
                out.push(run("deck.cue", d, json!({"down": true})));
                if let Some(x) = self.cue_held.get_mut(i) {
                    *x = true;
                }
            }
            Ctl::ToStart if press => out.push(run("deck.seek", d, json!({"at": 0.0}))),
            Ctl::JogTop | Ctl::JogSide if def.kind == Kind::Cc => {
                if s.playing || s.previewing {
                    if let Some(n) = self.nudge.get_mut(i) {
                        *n = n.saturating_add(ticks);
                    }
                } else if let Some(b) = self.search.get_mut(i) {
                    *b += ticks as f64 * SEARCH_BEATS_PER_TURN / JOG_TICKS;
                }
            }
            Ctl::JogFast if def.kind == Kind::Cc => {
                if let Some(b) = self.search.get_mut(i) {
                    *b += ticks as f64 * FAST_BEATS_PER_TURN / JOG_TICKS;
                }
            }
            Ctl::LoopIn if press => out.push(run("deck.loop", d, json!({"action": "in"}))),
            Ctl::LoopOut if press => out.push(run("deck.loop", d, json!({"action": "out"}))),
            Ctl::Loop4 if press => {
                out.push(if s.loop_on { run("deck.loop", d, json!({"action": "exit"})) } else { run("deck.loop", d, json!({"beats": 4})) })
            }
            Ctl::Reloop if press => out.push(run("deck.loop", d, json!({"action": "reloop"}))),
            Ctl::CallBack if press => {
                out.push(if s.loop_on { run("deck.loop", d, json!({"action": "halve"})) } else { run("deck.beatjump", d, json!({"beats": -4})) })
            }
            Ctl::CallFwd if press => {
                out.push(if s.loop_on { run("deck.loop", d, json!({"action": "double"})) } else { run("deck.beatjump", d, json!({"beats": 4})) })
            }
            Ctl::Halve if press => out.push(run("deck.loop", d, json!({"action": "halve"}))),
            Ctl::Double if press => out.push(run("deck.loop", d, json!({"action": "double"}))),
            Ctl::Sync if press => {
                let on = self.pending.get_mut(i).and_then(|p| p.get_mut(1)).map_or(!s.synced, |p| toggle(p, s.synced, ctx.now));
                out.push(run("deck.sync", d, json!({"on": on})));
            }
            Ctl::SyncOff if press => out.push(run("deck.sync", d, json!({"on": false}))),
            Ctl::TempoRange if press => {
                let now = s.range;
                let next = PITCH_RANGES.iter().position(|r| (r - now).abs() < 1e-6).and_then(|k| PITCH_RANGES.get(k + 1)).or(PITCH_RANGES.first());
                if let Some(r) = next {
                    out.push(run("deck.pitch", d, json!({"range": round(r * 100.0, 0)})));
                }
            }
            _ => {}
        }
        out
    }
}

impl Controller for Flx4 {
    fn name(&self) -> &'static str {
        NAME
    }

    fn claims(&self, msg: &MidiMessage) -> bool {
        match *msg {
            MidiMessage::NoteOn { ch, note, .. } | MidiMessage::NoteOff { ch, note, .. } => {
                let ch1 = ch + 1;
                ((8..=11).contains(&ch1) && note & 0x0F < 8) || Self::find(Kind::Note, ch1, note).is_some()
            }
            MidiMessage::ControlChange { ch, cc, .. } => Self::find(Kind::Cc, ch + 1, cc).is_some(),
            _ => false,
        }
    }

    fn controls(&self) -> Vec<MidiControl> {
        let mut out = Vec::new();
        for d in inputs() {
            let ch = d.ch.saturating_sub(1);
            match d.kind {
                Kind::Note => out.push(MidiControl::Note { ch, note: d.num }),
                Kind::Cc => {
                    out.push(MidiControl::Cc { ch, cc: d.num });
                    if let Some(l) = d.lsb {
                        out.push(MidiControl::Cc { ch, cc: l });
                    }
                }
            }
        }
        for (m, _, _, ch1) in pads() {
            for k in 0..8 {
                out.push(MidiControl::Note { ch: ch1 - 1, note: m.base() + k });
            }
        }
        out
    }

    fn input(&mut self, msg: &MidiMessage, ctx: &Ctx) -> Vec<Action> {
        match *msg {
            MidiMessage::NoteOn { ch, note, vel } | MidiMessage::NoteOff { ch, note, vel } => {
                let down = matches!(msg, MidiMessage::NoteOn { .. }) && vel > 0;
                if let Some(out) = self.pad(ch + 1, note, down, ctx) {
                    return out;
                }
                match Self::find(Kind::Note, ch + 1, note) {
                    Some((def, _)) => self.act(def, down, vel, None, ctx),
                    None => Vec::new(),
                }
            }
            MidiMessage::ControlChange { ch, cc, value } => {
                let Some((def, lsb)) = Self::find(Kind::Cc, ch + 1, cc) else { return Vec::new() };
                if def.lsb.is_some() {
                    match self.wide_value(ch, def, lsb, value) {
                        Some(v) => self.act(def, false, value, Some(v), ctx),
                        None => Vec::new(),
                    }
                } else {
                    self.act(def, false, value, None, ctx)
                }
            }
            _ => Vec::new(),
        }
    }

    fn tick(&mut self, ctx: &Ctx, dt: f64) -> Vec<Action> {
        let mut out = Vec::new();
        let dt = dt.clamp(1.0 / 240.0, 0.1);
        for d in DeckId::ALL {
            let i = d.index();
            let dc = ctx.deck(d);
            let beats = self.search.get_mut(i).map(std::mem::take).unwrap_or(0.0);
            let ticks = self.nudge.get_mut(i).map(std::mem::take).unwrap_or(0);
            if dc.loaded {
                let mut left = beats;
                for _ in 0..64 {
                    if left.abs() < 1e-4 {
                        break;
                    }
                    let step = left.clamp(-1.0, 1.0);
                    out.push(run("deck.jog", d, json!({"delta": round(step, 4), "mode": "search"})));
                    left -= step;
                }
                if ticks != 0 {
                    let delta = (ticks as f64 / dt / NUDGE_TICKS_S).clamp(-1.0, 1.0);
                    out.push(run("deck.jog", d, json!({"delta": round(delta, 4), "mode": "nudge"})));
                }
            }
            // A deck tempo moved elsewhere (a click, sync, a load) re-arms the soft takeover.
            if let Some(p) = self.tempo.get_mut(i)
                && p.picked
                && !follows(ctx, d)
                && let Some((v, at)) = p.sent
            {
                let range = if dc.state.range > 0.0 { dc.state.range } else { 0.08 } * 100.0;
                if ctx.now - at > TAKEOVER_SETTLE_S && (dc.state.pitch * 100.0 - v).abs() > range * 2.0 * 1.5 / 127.0 {
                    p.picked = false;
                }
            }
        }
        out
    }

    fn lights(&mut self, ctx: &Ctx) -> Vec<Light> {
        let dt = self.meter_at.map_or(0.0, |at| (ctx.now - at).clamp(0.0, 1.0)) as f32;
        self.meter_at = Some(ctx.now);
        let blink = (ctx.now * 2.0).rem_euclid(1.0) < 0.5;
        let mut out = Vec::new();
        let mut led = |ch1: u8, note: u8, on: bool| out.push(Light { msg: note_on(ch1, note, on), throttled: false });
        for d in DeckId::ALL {
            let dc: &DeckCtx = ctx.deck(d);
            let s = &dc.state;
            let ch1 = 1 + d.index() as u8;
            let heard = s.playing || s.previewing;
            let play = heard || (dc.loaded && blink);
            led(ch1, 0x0B, play);
            led(ch1, 0x0E, play);
            let at_cue = (s.position - s.cue).abs() <= AT_CUE;
            let cue = s.previewing || (dc.loaded && !s.playing && (at_cue || blink));
            led(ch1, 0x0C, cue);
            led(ch1, 0x48, cue);
            led(ch1, 0x10, dc.loaded && s.loop_on);
            led(ch1, 0x11, dc.loaded && s.loop_on);
            led(ch1, 0x4C, false);
            led(ch1, 0x4E, false);
            led(ch1, 0x58, dc.loaded && s.synced);
            led(ch1, 0x60, dc.loaded && s.master);
            let mode = self.mode(d);
            for m in PadMode::ALL {
                led(ch1, m.button(), m == mode);
            }
            led(ch1, 0x54, s.headphone_cue);
            led(ch1, 0x68, s.headphone_cue);
            for k in 0..8usize {
                let on = dc.loaded
                    && match mode {
                        PadMode::HotCue => s.hot.get(k).copied().flatten().is_some(),
                        PadMode::BeatJump => true,
                        PadMode::BeatLoop => s.loop_on && LOOP_PADS.get(k).zip(dc.loop_beats).is_some_and(|(b, x)| (b - x).abs() < 1e-6),
                        _ => false,
                    };
                for shift in [false, true] {
                    led(pad_channel(d, shift), mode.base() + k as u8, on);
                }
            }
        }
        led(7, 0x00, false);
        led(7, 0x01, false);
        for d in DeckId::ALL {
            let dc = ctx.deck(d);
            let peak = dc.state.level.iter().copied().filter(|x| x.is_finite()).fold(0.0f32, f32::max);
            let db = if peak > 0.0 { 20.0 * peak.log10() } else { f32::NEG_INFINITY };
            if let Some(m) = self.meter_db.get_mut(d.index()) {
                let fallen = if m.is_finite() { *m - METER_FALL_DB_S * dt } else { f32::NEG_INFINITY };
                *m = if db > fallen { db } else { fallen.max(-120.0) };
                let v = if dc.loaded { meter_value(*m) } else { 0 };
                out.push(Light { msg: MidiMessage::ControlChange { ch: d.index() as u8, cc: 0x02, value: v }, throttled: true });
            }
        }
        out
    }

    fn events(&mut self, ctx: &Ctx) -> Vec<MidiMessage> {
        let mut out = Vec::new();
        for d in DeckId::ALL {
            let load = ctx.deck(d).state.load;
            if let Some(l) = self.loads.get_mut(d.index())
                && *l != load
            {
                *l = load;
                if load != 0 && ctx.deck(d).loaded {
                    // Track load illumination: channel 16, note 0 / 1.
                    out.push(MidiMessage::NoteOn { ch: 15, note: d.index() as u8, vel: 0x7F });
                }
            }
        }
        out
    }

    fn twin(&self, msg: &MidiMessage) -> Option<MidiControl> {
        let MidiMessage::ControlChange { ch, cc, .. } = *msg else { return None };
        let (def, lsb) = Self::find(Kind::Cc, ch + 1, cc)?;
        let other = if lsb { def.num } else { def.lsb? };
        Some(MidiControl::Cc { ch, cc: other })
    }

    fn release_all(&mut self, _ctx: &Ctx) -> Vec<Action> {
        let mut out = Vec::new();
        for d in DeckId::ALL {
            let i = d.index();
            if self.cue_held.get_mut(i).is_some_and(std::mem::take) {
                out.push(run("deck.cue", d, json!({"down": false})));
            }
            if let Some(pad) = self.held_pad.get_mut(i).and_then(Option::take) {
                out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": "release"})));
            }
            if let Some(t) = self.touch.get_mut(i) {
                *t = false;
            }
        }
        out
    }

    fn held(&self, ctx: &Ctx) -> bool {
        DeckId::ALL.into_iter().any(|d| self.touch.get(d.index()).copied().unwrap_or(false) && ctx.deck(d).state.playing)
    }

    fn take_resend(&mut self) -> bool {
        std::mem::take(&mut self.resend)
    }

    fn dark(&self) -> Vec<MidiMessage> {
        let mut out = Vec::new();
        for d in DeckId::ALL {
            let ch1 = 1 + d.index() as u8;
            for n in [0x0B, 0x0E, 0x0C, 0x48, 0x10, 0x11, 0x4C, 0x4E, 0x58, 0x60, 0x54, 0x68] {
                out.push(note_on(ch1, n, false));
            }
            for m in PadMode::ALL {
                out.push(note_on(ch1, m.button(), false));
                for shift in [false, true] {
                    for k in 0..8 {
                        out.push(note_on(pad_channel(d, shift), m.base() + k, false));
                    }
                }
            }
            out.push(MidiMessage::ControlChange { ch: d.index() as u8, cc: 0x02, value: 0 });
        }
        out.push(note_on(7, 0x00, false));
        out.push(note_on(7, 0x01, false));
        // Not FX ON/OFF: djforge never drives it, and it lights on a Note Off (the doc's
        // footnote *4).
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::controller_layouts::Mirror;
    use serde_json::Value;
    use soundcraft_mix::DeckState;

    const DOC: &str = include_str!("../../../../docs/hardware/ddj-flx4.md");

    /// The rows of the first table under `## {section}`, cells trimmed, header and rule skipped.
    fn table(section: &str) -> Vec<Vec<String>> {
        let start = DOC.find(&format!("## {section}\n")).unwrap_or_else(|| panic!("no section {section}"));
        DOC[start..]
            .lines()
            .skip(1)
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'))
            .skip(2)
            .map(|l| l.trim().trim_matches('|').split('|').map(|c| c.trim().to_string()).collect())
            .collect()
    }

    /// "11 (0x0B)" is 11, checking the hex agrees.
    fn number(cell: &str) -> u8 {
        let (dec, hex) = cell.split_once(" (0x").unwrap_or_else(|| panic!("not a number: {cell}"));
        let n: u8 = dec.parse().unwrap();
        assert_eq!(u8::from_str_radix(hex.trim_end_matches(')'), 16).unwrap(), n, "{cell}");
        n
    }

    fn deck_of(cell: &str) -> Option<DeckId> {
        match cell {
            "1" => Some(DeckId::A),
            "2" => Some(DeckId::B),
            "—" => None,
            other => panic!("deck {other}"),
        }
    }

    type Row = (String, Option<DeckId>, &'static str, &'static str, u8, u8, Option<u8>, bool);

    #[test]
    fn the_layout_table_matches_the_hardware_doc() {
        let shift_id = |s: Shift| match s {
            Shift::No => "no",
            Shift::Yes => "yes",
            Shift::Either => "either",
        };
        let mut doc: Vec<Row> = table("Inputs")
            .iter()
            .map(|r| {
                assert_eq!(r.len(), 10, "{r:?}");
                let shift = match r[3].as_str() {
                    "no" => "no",
                    "yes" => "yes",
                    "either" => "either",
                    other => panic!("shift {other}"),
                };
                let kind = match r[4].as_str() {
                    "Note" => "note",
                    "CC" => "cc",
                    other => panic!("type {other}"),
                };
                let lsb = (r[7] != "—").then(|| number(&r[7]));
                (r[0].clone(), deck_of(&r[2]), shift, kind, r[5].parse().unwrap(), number(&r[6]), lsb, r[9].starts_with("(unmapped"))
            })
            .collect();
        let mut rust: Vec<Row> = inputs()
            .map(|d| {
                let kind = if d.kind == Kind::Note { "note" } else { "cc" };
                (d.id.to_string(), d.deck, shift_id(d.shift), kind, d.ch, d.num, d.lsb, d.ctl == Ctl::Unmapped)
            })
            .collect();
        assert!(doc.len() > 100, "the doc's inputs table: {} rows", doc.len());
        let key = |r: &Row| format!("{r:?}");
        doc.sort_by_key(key);
        rust.sort_by_key(key);
        for r in &doc {
            assert!(rust.contains(r), "in the doc, not the layout: {r:?}");
        }
        for r in &rust {
            assert!(doc.contains(r), "in the layout, not the doc: {r:?}");
        }
        // Every message is listed once.
        let mut msgs: Vec<(&str, u8, u8)> = rust.iter().flat_map(|r| std::iter::once((r.3, r.4, r.5)).chain(r.6.map(|l| (r.3, r.4, l)))).collect();
        let n = msgs.len();
        msgs.sort();
        msgs.dedup();
        assert_eq!(msgs.len(), n, "a message listed twice");

        let mut doc_pads: Vec<(String, bool, DeckId, u8, u8, bool)> = table("Performance pads")
            .iter()
            .map(|r| (r[0].clone(), r[1] == "yes", deck_of(&r[2]).unwrap(), r[3].parse().unwrap(), number(&r[4]), !r[5].starts_with("(unmapped")))
            .collect();
        let mut rust_pads: Vec<_> = pads().map(|(m, s, d, ch)| (m.id().to_string(), s, d, ch, m.base(), m.mapped())).collect();
        doc_pads.sort_by_key(|r| format!("{r:?}"));
        rust_pads.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(doc_pads, rust_pads);
    }

    fn loaded() -> DeckCtx {
        DeckCtx { state: DeckState { load: 1, range: 0.08, ..DeckState::default() }, loaded: true, loop_beats: None }
    }

    fn ctx() -> Ctx {
        Ctx { decks: [loaded(), loaded()], now: 10.0, headphone_knob: false }
    }

    fn on(ch1: u8, note: u8) -> MidiMessage {
        MidiMessage::NoteOn { ch: ch1 - 1, note, vel: 0x7F }
    }

    fn off(ch1: u8, note: u8) -> MidiMessage {
        MidiMessage::NoteOff { ch: ch1 - 1, note, vel: 0 }
    }

    fn cc(ch1: u8, cc: u8, value: u8) -> MidiMessage {
        MidiMessage::ControlChange { ch: ch1 - 1, cc, value }
    }

    fn cmd(id: &'static str, p: Value) -> Action {
        Action::Run(id, p)
    }

    /// A 14-bit move: MSB then LSB.
    fn wide(f: &mut Flx4, ctx: &Ctx, ch1: u8, msb_cc: u8, v: u16) -> Vec<Action> {
        assert!(f.input(&cc(ch1, msb_cc, (v >> 7) as u8), ctx).is_empty(), "the MSB waits for its LSB");
        f.input(&cc(ch1, msb_cc + 32, (v & 0x7F) as u8), ctx)
    }

    #[test]
    fn deck_buttons_and_the_loop_section_run_deck_commands() {
        let mut f = Flx4::new();
        let mut c = ctx();
        assert_eq!(f.input(&on(1, 0x0B), &c), vec![cmd("deck.play", json!({"deck": "a"}))]);
        assert!(f.input(&off(1, 0x0B), &c).is_empty(), "play acts on the press");
        c.decks[1].state.playing = true;
        assert_eq!(f.input(&on(2, 0x0B), &c), vec![cmd("deck.pause", json!({"deck": "b"}))]);
        // CUE: press and release (CDJ).
        assert_eq!(f.input(&on(1, 0x0C), &c), vec![cmd("deck.cue", json!({"deck": "a", "down": true}))]);
        assert_eq!(f.input(&off(1, 0x0C), &c), vec![cmd("deck.cue", json!({"deck": "a", "down": false}))]);
        assert!(f.input(&off(1, 0x0C), &c).is_empty(), "no release without a press");
        assert_eq!(f.input(&on(1, 0x48), &c), vec![cmd("deck.seek", json!({"deck": "a", "at": 0.0}))], "SHIFT+CUE: to the start");
        // Sync: toggles from the state; long press off; SHIFT: the tempo range.
        assert_eq!(f.input(&on(1, 0x58), &c), vec![cmd("deck.sync", json!({"deck": "a", "on": true}))]);
        c.decks[0].state.synced = true;
        assert_eq!(f.input(&on(1, 0x58), &c), vec![cmd("deck.sync", json!({"deck": "a", "on": false}))]);
        assert_eq!(f.input(&on(1, 0x5C), &c), vec![cmd("deck.sync", json!({"deck": "a", "on": false}))]);
        assert_eq!(f.input(&on(1, 0x60), &c), vec![cmd("deck.pitch", json!({"deck": "a", "range": 16.0}))]);
        c.decks[0].state.range = 0.5;
        assert_eq!(f.input(&on(1, 0x60), &c), vec![cmd("deck.pitch", json!({"deck": "a", "range": 8.0}))], "50 % wraps to 8 %");
        // Loops.
        assert_eq!(f.input(&on(2, 0x10), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "in"}))]);
        assert_eq!(f.input(&on(2, 0x11), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "out"}))]);
        assert_eq!(f.input(&on(2, 0x4D), &c), vec![cmd("deck.loop", json!({"deck": "b", "beats": 4}))]);
        assert_eq!(f.input(&on(2, 0x51), &c), vec![cmd("deck.beatjump", json!({"deck": "b", "beats": -4}))]);
        assert_eq!(f.input(&on(2, 0x53), &c), vec![cmd("deck.beatjump", json!({"deck": "b", "beats": 4}))]);
        c.decks[1].state.loop_on = true;
        assert_eq!(f.input(&on(2, 0x4D), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "exit"}))]);
        assert_eq!(f.input(&on(2, 0x51), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "halve"}))]);
        assert_eq!(f.input(&on(2, 0x53), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "double"}))]);
        assert_eq!(f.input(&on(2, 0x3E), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "halve"}))]);
        assert_eq!(f.input(&on(2, 0x3D), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "double"}))]);
        assert_eq!(f.input(&on(2, 0x50), &c), vec![cmd("deck.loop", json!({"deck": "b", "action": "reloop"}))]);
        // An empty deck does nothing; unmapped controls are claimed and do nothing.
        c.decks[0].loaded = false;
        assert!(f.input(&on(1, 0x0B), &c).is_empty());
        assert!(f.claims(&on(1, 0x0E)) && f.input(&on(1, 0x0E), &c).is_empty());
        assert!(f.claims(&on(5, 0x47)), "beat FX is claimed");
        assert!(!f.claims(&on(1, 0x7E)) && !f.claims(&on(13, 0x00)), "not the FLX4's");
    }

    #[test]
    fn the_mixer_and_browse_section() {
        let mut f = Flx4::new();
        let c = ctx();
        assert_eq!(wide(&mut f, &c, 1, 0x13, 16383), vec![cmd("deck.fader", json!({"deck": "a", "value": 1.0}))]);
        assert_eq!(wide(&mut f, &c, 2, 0x13, 0), vec![cmd("deck.fader", json!({"deck": "b", "value": 0.0}))]);
        assert_eq!(wide(&mut f, &c, 7, 0x1F, 0), vec![cmd("deck.xfader", json!({"value": -1.0}))]);
        assert_eq!(wide(&mut f, &c, 7, 0x1F, 16383), vec![cmd("deck.xfader", json!({"value": 1.0}))]);
        // EQ and trim as the on-screen knobs: the centre is 0 dB.
        let centre = 8192;
        assert_eq!(wide(&mut f, &c, 1, 0x07, centre), vec![cmd("deck.eq", json!({"deck": "a", "band": "high", "db": 0.0}))]);
        let top = (f64::from(crate::knob::eq_db(1.0)) * 10.0).round() / 10.0;
        assert_eq!(wide(&mut f, &c, 2, 0x0F, 16383), vec![cmd("deck.eq", json!({"deck": "b", "band": "low", "db": top}))]);
        assert_eq!(wide(&mut f, &c, 1, 0x0B, centre), vec![cmd("deck.eq", json!({"deck": "a", "band": "mid", "db": 0.0}))]);
        assert_eq!(wide(&mut f, &c, 1, 0x04, centre), vec![cmd("deck.trim", json!({"deck": "a", "db": 0.0}))]);
        // CFX: the filter, off around the centre, on channel 7 (23 deck 1, 24 deck 2).
        assert_eq!(wide(&mut f, &c, 7, 0x17, centre), vec![cmd("deck.filter", json!({"deck": "a", "value": 0.0}))]);
        assert_eq!(wide(&mut f, &c, 7, 0x18, 0), vec![cmd("deck.filter", json!({"deck": "b", "value": -1.0}))]);
        assert_eq!(wide(&mut f, &c, 7, 0x08, 16383), vec![cmd("deck.master_volume", json!({"db": 0.0}))]);
        assert_eq!(wide(&mut f, &c, 7, 0x08, 0), vec![cmd("deck.master_volume", json!({"db": -60.0}))]);
        // An LSB with no MSB yet does nothing (it would jump to the bottom).
        let mut fresh = Flx4::new();
        assert!(fresh.input(&cc(1, 0x33, 5), &c).is_empty());
        // Headphone CUE toggles.
        assert_eq!(f.input(&on(1, 0x54), &c), vec![cmd("deck.headphone_cue", json!({"deck": "a", "on": true}))]);
        let mut cued = c;
        cued.decks[0].state.headphone_cue = true;
        assert_eq!(f.input(&on(1, 0x54), &cued), vec![cmd("deck.headphone_cue", json!({"deck": "a", "on": false}))]);
        // MASTER CUE toggles (from what the decks report); HEADPHONE MIX sets the mix, LEVEL is
        // the hardware's own.
        assert_eq!(f.input(&on(7, 0x63), &c), vec![cmd("deck.master_cue", json!({}))]);
        assert!(f.input(&off(7, 0x63), &c).is_empty());
        assert_eq!(wide(&mut f, &c, 7, 0x0C, 16383), vec![cmd("deck.headphone_mix", json!({"value": 1.0}))]);
        assert_eq!(wide(&mut f, &c, 7, 0x0C, 0), vec![cmd("deck.headphone_mix", json!({"value": 0.0}))]);
        assert!(wide(&mut f, &c, 7, 0x0D, 8192).is_empty() && f.claims(&cc(7, 0x0D, 0)), "HEADPHONE LEVEL is claimed, unmapped by default");
        let knob = Ctx { headphone_knob: true, ..c };
        assert_eq!(wide(&mut f, &knob, 7, 0x0D, 16383), vec![cmd("deck.headphone_level", json!({"db": 0.0}))], "with the option: djforge's level");
        assert_eq!(wide(&mut f, &knob, 7, 0x0D, 0), vec![cmd("deck.headphone_level", json!({"db": -60.0}))]);
        // Browse: two's complement clicks, SHIFT pages by 8; press and LOAD.
        assert_eq!(f.input(&cc(7, 0x40, 0x01), &c), vec![Action::Browse(1)]);
        assert_eq!(f.input(&cc(7, 0x40, 0x7E), &c), vec![Action::Browse(-2)]);
        assert_eq!(f.input(&cc(7, 0x64, 0x7F), &c), vec![Action::Browse(-8)]);
        assert_eq!(f.input(&on(7, 0x41), &c), vec![Action::Load(None)]);
        assert_eq!(f.input(&on(7, 0x46), &c), vec![Action::Load(Some(DeckId::A))]);
        assert_eq!(f.input(&on(7, 0x47), &c), vec![Action::Load(Some(DeckId::B))]);
        assert!(f.input(&on(7, 0x68), &c).is_empty() && f.claims(&on(7, 0x68)), "SHIFT+LOAD is unmapped");
    }

    #[test]
    fn the_tempo_slider_is_14_bit_with_soft_takeover() {
        let mut f = Flx4::new();
        let mut c = ctx();
        // At the deck's pitch (0): picked up at once. Full "+" is +8 %.
        assert_eq!(wide(&mut f, &c, 1, 0x00, 8192), vec![cmd("deck.pitch", json!({"deck": "a", "value": 0.0}))]);
        assert_eq!(wide(&mut f, &c, 1, 0x00, 16383), vec![cmd("deck.pitch", json!({"deck": "a", "value": 8.0}))]);
        // 14 bits: one LSB count is a step.
        let a = wide(&mut f, &c, 1, 0x00, 12288);
        let b = wide(&mut f, &c, 1, 0x00, 12289);
        assert_ne!(a, b);
        let Some(Action::Run("deck.pitch", p)) = a.first() else { panic!("{a:?}") };
        assert!((p["value"].as_f64().unwrap() - 4.0).abs() < 0.01, "three quarters up is +4 %: {p}");
        // The deck's pitch moved elsewhere (a click): once settled, the slider waits to reach it.
        c.decks[0].state.pitch = -0.02;
        c.now += 1.0;
        assert!(f.tick(&c, 0.016).is_empty());
        assert!(wide(&mut f, &c, 1, 0x00, 12000).is_empty(), "not picked up yet");
        assert!(wide(&mut f, &c, 1, 0x00, 9000).is_empty(), "still above −2 %");
        // Crossing it picks it up.
        let crossed = wide(&mut f, &c, 1, 0x00, 4000);
        assert_eq!(crossed.len(), 1, "{crossed:?}");
        // Deck B's slider is its own: at −8 % with the deck at 0, nothing until it gets there.
        assert!(wide(&mut f, &c, 2, 0x00, 0).is_empty());
        assert_eq!(wide(&mut f, &c, 2, 0x00, 8192).len(), 1, "reached the deck's pitch");
        // Synced to a master on the other deck: straight away (the engine's own takeover turns
        // sync off and waits).
        let mut g = Flx4::new();
        let mut s = ctx();
        s.decks[0].state.synced = true;
        s.decks[1].state.master = true;
        assert_eq!(wide(&mut g, &s, 1, 0x00, 0), vec![cmd("deck.pitch", json!({"deck": "a", "value": -8.0}))]);
        // Synced but itself the master: the slider sets the tempo, so it must reach it first.
        let mut h = Flx4::new();
        let mut m = ctx();
        m.decks[0].state.synced = true;
        m.decks[0].state.master = true;
        assert!(wide(&mut h, &m, 1, 0x00, 0).is_empty(), "no jump to −8 %");
        assert_eq!(wide(&mut h, &m, 1, 0x00, 8192).len(), 1);
    }

    #[test]
    fn the_jog_searches_paused_nudges_playing_and_shift_searches_fast() {
        let mut f = Flx4::new();
        let mut c = ctx();
        // Paused: the platter (0x22) and the side (0x21) search; 0x41 is +1, 0x3F is −1.
        for _ in 0..360 {
            f.input(&cc(1, 0x22, 0x41), &c);
        }
        assert_eq!(f.tick(&c, 0.016), vec![cmd("deck.jog", json!({"deck": "a", "delta": 1.0, "mode": "search"}))], "half a turn: one beat");
        f.input(&cc(1, 0x21, 0x40 - 36), &c);
        assert_eq!(f.tick(&c, 0.016), vec![cmd("deck.jog", json!({"deck": "a", "delta": -0.1, "mode": "search"}))]);
        // SHIFT + platter: 32 beats a turn.
        f.input(&cc(1, 0x29, 0x40 + 45), &c);
        let fast = vec![cmd("deck.jog", json!({"deck": "a", "delta": 1.0, "mode": "search"})); 2];
        assert_eq!(f.tick(&c, 0.016), fast);
        // Playing: a nudge as strong as the turn is fast.
        c.decks[1].state.playing = true;
        f.input(&cc(2, 0x21, 0x40 + 8), &c);
        assert_eq!(f.tick(&c, 0.016), vec![cmd("deck.jog", json!({"deck": "b", "delta": 0.5, "mode": "nudge"}))]);
        f.input(&cc(2, 0x22, 0x40 - 60), &c);
        assert_eq!(f.tick(&c, 0.016), vec![cmd("deck.jog", json!({"deck": "b", "delta": -1.0, "mode": "nudge"}))], "clamped");
        // Touch is state only.
        assert!(f.input(&on(2, 0x36), &c).is_empty());
        assert!(f.touch[1]);
        assert!(f.tick(&c, 0.016).is_empty(), "nothing waiting");
    }

    #[test]
    fn pads_follow_the_hardware_mode_including_the_shift_layers() {
        let mut f = Flx4::new();
        let mut c = ctx();
        c.decks[0].state.hot[2] = Some(48_000.0);
        // Hot cue: an empty pad sets, a set pad jumps and sends its release; SHIFT clears.
        assert_eq!(f.input(&on(8, 0x00), &c), vec![cmd("deck.hotcue", json!({"deck": "a", "n": 1, "action": "set"}))]);
        assert!(f.input(&off(8, 0x00), &c).is_empty());
        assert_eq!(f.input(&on(8, 0x02), &c), vec![cmd("deck.hotcue", json!({"deck": "a", "n": 3, "action": "jump"}))]);
        assert_eq!(f.input(&off(8, 0x02), &c), vec![cmd("deck.hotcue", json!({"deck": "a", "n": 3, "action": "release"}))]);
        assert_eq!(f.input(&on(9, 0x02), &c), vec![cmd("deck.hotcue", json!({"deck": "a", "n": 3, "action": "clear"}))]);
        assert_eq!(f.input(&on(10, 0x07), &c), vec![cmd("deck.hotcue", json!({"deck": "b", "n": 8, "action": "set"}))]);
        // Beat jump (pads 0x20..) and its SHIFT layer.
        assert_eq!(f.input(&on(10, 0x20), &c), vec![cmd("deck.beatjump", json!({"deck": "b", "beats": -1.0}))]);
        assert_eq!(f.input(&on(10, 0x27), &c), vec![cmd("deck.beatjump", json!({"deck": "b", "beats": 32.0}))]);
        assert_eq!(f.input(&on(11, 0x23), &c), vec![cmd("deck.beatjump", json!({"deck": "b", "beats": 4.0}))]);
        assert_eq!(f.mode(DeckId::B), PadMode::BeatJump, "the pad's note says the mode");
        // Beat loop (SHIFT + BEAT JUMP: pads 0x60..): a size, and the same size again exits.
        assert_eq!(f.input(&on(8, 0x64), &c), vec![cmd("deck.loop", json!({"deck": "a", "beats": 4.0}))]);
        c.decks[0].state.loop_on = true;
        c.decks[0].loop_beats = Some(4.0);
        assert_eq!(f.input(&on(8, 0x64), &c), vec![cmd("deck.loop", json!({"deck": "a", "action": "exit"}))]);
        assert_eq!(f.input(&on(8, 0x60), &c), vec![cmd("deck.loop", json!({"deck": "a", "beats": 0.25}))]);
        // Pad FX, sampler, keyboard, key shift: claimed, nothing.
        for base in [0x10, 0x30, 0x40, 0x50, 0x70] {
            assert!(f.claims(&on(8, base)) && f.input(&on(8, base), &c).is_empty(), "{base:#x}");
        }
        // The mode buttons, with their SHIFT modes.
        f.input(&on(1, 0x6D), &c);
        assert_eq!(f.mode(DeckId::A), PadMode::BeatLoop);
        f.input(&on(1, 0x69), &c);
        assert_eq!(f.mode(DeckId::A), PadMode::Keyboard);
        f.input(&on(1, 0x1B), &c);
        assert_eq!(f.mode(DeckId::A), PadMode::HotCue);
        assert!(!f.claims(&on(8, 0x08)), "only 8 pads a mode");
    }

    fn bytes(msgs: &[MidiMessage]) -> Vec<[u8; 3]> {
        msgs.iter().map(|m| m.wire().0).collect()
    }

    /// The lights for a sequence of deck states, as the host sends them (changes only).
    #[test]
    fn leds_send_only_what_changes_for_a_sequence_of_deck_states() {
        let mut f = Flx4::new();
        let mut m = Mirror::default();
        let mut c = ctx();
        c.now = 0.1;
        let first = bytes(&m.diff(&f.lights(&c), c.now, |_| true));
        assert!(first.len() > 50, "everything at first: {}", first.len());
        // Paused with a track at its cue: CUE lit, PLAY blinking (on in the first quarter second).
        assert!(first.contains(&[0x90, 0x0C, 0x7F]) && first.contains(&[0x90, 0x0B, 0x7F]), "{first:02X?}");
        // Nothing changed: nothing sent.
        assert!(m.diff(&f.lights(&c), c.now + 0.01, |_| true).is_empty());
        // Deck A plays, synced, in a loop, with hot cue C set and its channel cued.
        c.now = 0.2;
        let s = &mut c.decks[0].state;
        (s.playing, s.synced, s.loop_on, s.headphone_cue) = (true, true, true, true);
        s.position = 96_000.0;
        s.hot[2] = Some(1.0);
        let sent = bytes(&m.diff(&f.lights(&c), c.now, |_| true));
        for want in [
            [0x90, 0x58, 0x7F],
            [0x90, 0x10, 0x7F],
            [0x90, 0x11, 0x7F],
            [0x90, 0x54, 0x7F],
            [0x90, 0x68, 0x7F],
            [0x97, 0x02, 0x7F],
            [0x98, 0x02, 0x7F],
            [0x90, 0x0C, 0x00],
        ] {
            assert!(sent.contains(&want), "{want:02X?} not in {sent:02X?}");
        }
        assert!(!sent.iter().any(|b| b[0] == 0x91), "deck B didn't change: {sent:02X?}");
        // A level: the meter (CC 2), at most 30 times a second.
        c.decks[0].state.level = [0.5, 0.25];
        c.now = 0.21;
        let sent = bytes(&m.diff(&f.lights(&c), c.now, |_| true));
        let v = meter_value(20.0 * 0.5f32.log10());
        assert!(sent.contains(&[0xB0, 0x02, v]), "{sent:02X?}");
        assert!((0x57..=0x64).contains(&v), "−6 dB is orange 1: {v:#x}");
        c.now = 0.215;
        c.decks[0].state.level = [1.0, 1.0];
        assert!(!bytes(&m.diff(&f.lights(&c), c.now, |_| true)).iter().any(|b| b[0] == 0xB0), "too soon for the meter");
        c.now = 0.26;
        assert!(bytes(&m.diff(&f.lights(&c), c.now, |_| true)).iter().any(|b| b[0] == 0xB0 && b[2] >= 0x77), "red at 0 dB");
        // Beat jump mode: the pads show it (all lit with a track), sent afresh on the switch.
        f.input(&on(1, 0x20), &c);
        assert!(f.take_resend());
        m.reset();
        let sent = bytes(&m.diff(&f.lights(&c), c.now, |_| true));
        assert!((0x20..0x28).all(|n| sent.contains(&[0x97, n, 0x7F])));
        assert!(sent.contains(&[0x90, 0x20, 0x7F]) && sent.contains(&[0x90, 0x1B, 0x00]), "the mode LEDs");
    }

    #[test]
    fn the_meter_scale_follows_the_doc() {
        assert_eq!(meter_value(f32::NEG_INFINITY), 0);
        assert_eq!(meter_value(-30.0), 0);
        assert_eq!(meter_value(-24.0), 0x26);
        assert_eq!(meter_value(-12.0), 0x41);
        assert_eq!(meter_value(-6.0), 0x57);
        assert_eq!(meter_value(-3.0), 0x65);
        assert_eq!(meter_value(0.0), 0x77);
        assert_eq!(meter_value(1.0), 0x7F);
    }

    #[test]
    fn a_loaded_track_flashes_the_load_illumination_once() {
        let mut f = Flx4::new();
        let mut c = ctx();
        c.decks[1] = DeckCtx::default();
        assert_eq!(bytes(&f.events(&c)), vec![[0x9F, 0x00, 0x7F]]);
        assert!(f.events(&c).is_empty());
        c.decks[1] = loaded();
        c.decks[1].state.load = 7;
        assert_eq!(bytes(&f.events(&c)), vec![[0x9F, 0x01, 0x7F]]);
    }

    #[test]
    fn dark_turns_off_every_light_the_layout_lights() {
        let mut f = Flx4::new();
        let mut c = ctx();
        c.decks[0].state.playing = true;
        let dark = bytes(&f.dark());
        assert!(dark.iter().all(|b| b[2] == 0));
        let lit = bytes(&f.lights(&c).iter().map(|l| l.msg).collect::<Vec<_>>());
        for l in lit {
            assert!(dark.iter().any(|d| d[0] == l[0] && d[1] == l[1]), "{l:02X?} is never turned off");
        }
    }

    #[test]
    fn held_cues_are_let_go_on_the_shifted_note_and_by_release_all() {
        let mut f = Flx4::new();
        let mut c = ctx();
        c.decks[0].state.hot[1] = Some(1.0);
        // CUE held, SHIFT pressed mid-hold: the release comes as SHIFT+CUE's note-off.
        f.input(&on(1, 0x0C), &c);
        assert_eq!(f.input(&off(1, 0x48), &c), vec![cmd("deck.cue", json!({"deck": "a", "down": false}))]);
        assert!(f.input(&off(1, 0x0C), &c).is_empty(), "let go once");
        // A hot cue preview released on the SHIFT channel.
        f.input(&on(8, 0x01), &c);
        assert_eq!(f.input(&off(9, 0x01), &c), vec![cmd("deck.hotcue", json!({"deck": "a", "n": 2, "action": "release"}))]);
        // Held on both decks, then everything let go at once (unplug, layout off, learn, quit).
        f.input(&on(1, 0x0C), &c);
        f.input(&on(8, 0x01), &c);
        f.input(&on(2, 0x0C), &c);
        let all = f.release_all(&c);
        assert_eq!(
            all,
            vec![
                cmd("deck.cue", json!({"deck": "a", "down": false})),
                cmd("deck.hotcue", json!({"deck": "a", "n": 2, "action": "release"})),
                cmd("deck.cue", json!({"deck": "b", "down": false})),
            ]
        );
        assert!(f.release_all(&c).is_empty());
        assert!(f.input(&off(1, 0x0C), &c).is_empty() && f.input(&off(8, 0x01), &c).is_empty(), "nothing left to let go");
    }

    #[test]
    fn a_quick_double_press_of_a_toggle_sends_both_states() {
        let mut f = Flx4::new();
        let mut c = ctx();
        // The deck hasn't reported the first press yet: the second still turns it back.
        assert_eq!(f.input(&on(1, 0x0B), &c), vec![cmd("deck.play", json!({"deck": "a"}))]);
        c.now += 0.05;
        assert_eq!(f.input(&on(1, 0x0B), &c), vec![cmd("deck.pause", json!({"deck": "a"}))]);
        assert_eq!(f.input(&on(1, 0x58), &c), vec![cmd("deck.sync", json!({"deck": "a", "on": true}))]);
        assert_eq!(f.input(&on(1, 0x58), &c), vec![cmd("deck.sync", json!({"deck": "a", "on": false}))]);
        assert_eq!(f.input(&on(2, 0x54), &c), vec![cmd("deck.headphone_cue", json!({"deck": "b", "on": true}))]);
        assert_eq!(f.input(&on(2, 0x54), &c), vec![cmd("deck.headphone_cue", json!({"deck": "b", "on": false}))]);
        // Long after, the reported state counts again.
        c.now += 5.0;
        c.decks[1].state.headphone_cue = true;
        assert_eq!(f.input(&on(2, 0x54), &c), vec![cmd("deck.headphone_cue", json!({"deck": "b", "on": false}))]);
    }

    #[test]
    fn each_half_of_a_14_bit_control_names_its_twin_and_a_touched_platter_is_held() {
        let f = Flx4::new();
        assert_eq!(f.twin(&cc(1, 0x07, 1)), Some(MidiControl::Cc { ch: 0, cc: 0x27 }));
        assert_eq!(f.twin(&cc(1, 0x27, 1)), Some(MidiControl::Cc { ch: 0, cc: 0x07 }));
        assert_eq!(f.twin(&cc(7, 0x17, 1)), Some(MidiControl::Cc { ch: 6, cc: 0x37 }));
        assert_eq!(f.twin(&cc(1, 0x21, 0x41)), None, "the jog is 7-bit");
        assert_eq!(f.twin(&on(1, 0x0B)), None);
        let mut g = Flx4::new();
        let mut c = ctx();
        g.input(&on(1, 0x36), &c);
        assert!(!g.held(&c), "touched, but paused: no nudge");
        c.decks[0].state.playing = true;
        assert!(g.held(&c));
        g.input(&off(1, 0x36), &c);
        assert!(!g.held(&c));
    }

    /// "Note 11 (0x0B)" / "Note 0 / 1 (0x00 / 0x01)" / "CC 2 (0x02)": the kind and numbers.
    fn message(cell: &str) -> Option<(bool, Vec<u8>)> {
        let (kind, rest) = cell.split_once(' ')?;
        let note = match kind {
            "Note" => true,
            "CC" => false,
            _ => return None,
        };
        let dec = rest.split('(').next()?;
        Some((note, dec.split('/').map(|n| n.trim().parse().unwrap()).collect()))
    }

    #[test]
    fn the_led_table_matches_what_the_layout_lights_and_darkens() {
        // (status, data 1) of every LED the doc lists, but the pads (checked through `pads`)
        // and the one-off messages.
        let mut doc: Vec<(u8, u8)> = Vec::new();
        let mut one_off: Vec<(u8, u8)> = Vec::new();
        for r in table("Outputs (LEDs)") {
            assert_eq!(r.len(), 6, "{r:?}");
            let Some((note, nums)) = message(&r[3]) else {
                assert_eq!(r[0], "Performance pads", "{r:?}");
                continue;
            };
            let chans: Vec<u8> = r[2].split('/').map(|c| c.trim().parse().unwrap()).collect();
            let keys = chans.iter().flat_map(|ch| nums.iter().map(move |n| (if note { 0x90 } else { 0xB0 } | (ch - 1), *n)));
            if r[5].starts_with("sent when") || r[5].starts_with("not sent") {
                one_off.extend(keys);
            } else {
                doc.extend(keys);
            }
        }
        let pad = |b: u8| (0x97..=0x9A).contains(&b);
        let key = |m: &MidiMessage| {
            let (b, _) = m.wire();
            (b[0], b[1])
        };
        let mut c = ctx();
        c.decks[0].state.playing = true;
        let mut f = Flx4::new();
        let lit: Vec<(u8, u8)> = f.lights(&c).iter().map(|l| key(&l.msg)).filter(|k| !pad(k.0)).collect();
        let dark: Vec<(u8, u8)> = f.dark().iter().map(key).filter(|k| !pad(k.0)).collect();
        for k in &lit {
            assert!(doc.contains(k), "lit but not in the doc: {k:02X?}");
        }
        for k in &dark {
            assert!(doc.contains(k), "darkened but not in the doc: {k:02X?}");
        }
        for k in &doc {
            assert!(dark.contains(k), "in the doc but never darkened: {k:02X?}");
        }
        // FX ON/OFF lights on a Note Off: never sent, not even at the end.
        for k in [(0x94, 0x47), (0x95, 0x47), (0x94, 0x43), (0x95, 0x43)] {
            assert!(one_off.contains(&k) && !dark.contains(&k) && !lit.contains(&k), "{k:02X?}");
        }
        assert!(one_off.contains(&(0x9F, 0)) && one_off.contains(&(0x9F, 1)) && one_off.contains(&(0x90, 0x17)), "{one_off:02X?}");
        let flash: Vec<(u8, u8)> = f.events(&c).iter().map(key).collect();
        assert!(flash.iter().all(|k| one_off.contains(k)), "{flash:02X?}");
        // The pads: every block's notes on its channel are darkened.
        let all: Vec<(u8, u8)> = f.dark().iter().map(key).collect();
        for (m, _, _, ch) in pads() {
            for k in 0..8 {
                assert!(all.contains(&(0x90 | (ch - 1), m.base() + k)));
            }
        }
    }
}
