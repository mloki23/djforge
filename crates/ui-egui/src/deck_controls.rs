//! The deck and DJ mixer controls a MIDI controller drives, shared by the built-in layouts
//! ([`crate::layouts`]) and MIDI Learn ([`crate::midi_learn`]), so a learned button does exactly
//! what the same button of a built-in layout does:
//!
//! - **Buttons** ([`Button`], [`DeckControls::button`]): PLAY and SYNC toggle from the state asked
//!   for last while the deck hasn't reported it yet ([`toggle`]), so a quick double press sends
//!   on then off; CUE is the CDJ cue (press and release); a hot cue pad jumps to a set cue (its
//!   release ends a preview) or sets an empty one; pitch bend bends while held.
//! - **Jog** ([`DeckControls::jog`]): ticks add up over a frame; [`DeckControls::tick`] sends them
//!   once a frame, a search while the deck is paused and a nudge as strong as the turn is fast
//!   while it is heard.
//! - **Absolute controls** ([`Continuous`]): a control's travel 0..=1 as the deck command the
//!   on-screen knob or fader sends ([`Continuous::action`]). The pitch fader has soft takeover
//!   ([`Takeover`], [`DeckControls::tempo`]), bypassed only while its deck follows a master on the
//!   other deck (the engine's own takeover applies then).
//!
//! What a control holds (a CUE or hot cue preview, a pitch bend) is let go by
//! [`DeckControls::release_all`]: MIDI Learn starts, the device goes, the app quits.

use serde::{Deserialize, Serialize};
use serde_json::json;
use soundcraft_mix::{DeckId, EqBand, PITCH_RANGES};

use crate::controller_layouts::{Action, Ctx};

/// How long a toggle's wanted state counts over what the deck reports (the report lags).
pub const PENDING_S: f64 = 0.5;
/// Seconds after a soft-takeover move before the target moved elsewhere re-arms the takeover
/// (the published state lags the command).
pub const TAKEOVER_SETTLE_S: f64 = 0.3;
/// Jog counts per revolution (assumed for the DDJ-FLX4: Pioneer's list doesn't say).
pub const JOG_TICKS: f64 = 720.0;
/// Beats a full platter turn searches, paused.
pub const SEARCH_BEATS_PER_TURN: f64 = 2.0;
/// Beats a full turn searches with SHIFT.
pub const FAST_BEATS_PER_TURN: f64 = 32.0;
/// Jog counts a second for a full nudge (10 % bend), as the on-screen jog's drag speed.
pub const NUDGE_TICKS_S: f64 = 1000.0;
/// A held pitch bend button's nudge (of `deck.jog`'s ±1, a 10 % bend): 4 %.
pub const BEND: f64 = 0.4;

/// A deck command with its deck.
pub fn run(id: &'static str, d: DeckId, mut p: serde_json::Value) -> Action {
    if let Some(o) = p.as_object_mut() {
        o.insert("deck".into(), json!(d.id()));
    }
    Action::Run(id, p)
}

pub fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    (x * k).round() / k
}

/// A toggle pressed: the new state, from the one asked for last if the deck hasn't reported it
/// yet, else from the reported one, so a quick double press sends on then off, not on twice.
pub fn toggle(pending: &mut Option<(bool, f64)>, reported: bool, now: f64) -> bool {
    let base = match *pending {
        Some((want, at)) if now - at < PENDING_S && (0.0..).contains(&(now - at)) && want != reported => want,
        _ => reported,
    };
    *pending = Some((!base, now));
    !base
}

/// Whether deck `d`'s pitch follows the other deck's tempo: the engine's own takeover applies.
pub fn follows(ctx: &Ctx, d: DeckId) -> bool {
    ctx.deck(d).state.synced && ctx.deck(d.other()).state.master
}

/// A deck's pitch range in percent (8 % before it reports one).
pub fn range_pct(ctx: &Ctx, d: DeckId) -> f64 {
    let r = ctx.deck(d).state.range;
    if r > 0.0 { r * 100.0 } else { 8.0 }
}

/// Soft takeover of an absolute control: it acts only once it reaches (or crosses) the value its
/// target has, so nothing jumps; once the target moves elsewhere it has to be picked up again.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Takeover {
    picked: bool,
    /// The control's last reading.
    hw: Option<f64>,
    /// The last value sent, and when.
    sent: Option<(f64, f64)>,
}

impl Takeover {
    /// The control reads `v` (in its target's units) with the target at `cur`: whether it acts.
    /// Within `window` of the target counts as reaching it; `bypass` acts at once.
    pub fn admit(&mut self, v: f64, cur: f64, window: f64, bypass: bool, now: f64) -> bool {
        let prev = self.hw.replace(v);
        if !bypass && !self.picked {
            let crossed = prev.is_some_and(|h| (h - cur).signum() != (v - cur).signum());
            if (v - cur).abs() > window && !crossed {
                return false;
            }
            self.picked = true;
        }
        self.sent = Some((v, now));
        true
    }

    /// Notes the control's reading without acting (its deck has no track).
    pub fn observe(&mut self, v: f64) {
        self.hw = Some(v);
    }

    /// The target moved elsewhere (a click, sync, a load) since the last value sent settled:
    /// picked up again before it acts.
    pub fn rearm(&mut self, cur: f64, window: f64, bypass: bool, now: f64) {
        if self.picked
            && !bypass
            && let Some((v, at)) = self.sent
            && now - at > TAKEOVER_SETTLE_S
            && (cur - v).abs() > window
        {
            self.picked = false;
        }
    }

    pub fn picked(&self) -> bool {
        self.picked
    }

    /// The last value sent, when it was sent within [`TAKEOVER_SETTLE_S`] of `now` (the deck
    /// may not have reported it yet).
    pub fn recent(&self, now: f64) -> Option<f64> {
        self.sent.filter(|(_, at)| now - at <= TAKEOVER_SETTLE_S && now >= *at).map(|(v, _)| v)
    }
}

/// A deck's buttons (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Button {
    Play,
    /// The CDJ cue: press and release.
    Cue,
    /// Back to the start (SHIFT + CUE); also lets go of a held CUE.
    ToStart,
    Sync,
    SyncOff,
    /// The next pitch range (8, 16, 50 %).
    TempoRange,
    MasterTempo,
    HeadphoneCue,
    /// 4 BEAT / EXIT: a 4-beat loop, or out of the loop.
    Loop4,
    LoopIn,
    LoopOut,
    Reloop,
    Halve,
    Double,
    /// CUE/LOOP CALL ◁: halve while looping, else jump 4 beats back.
    CallBack,
    /// CUE/LOOP CALL ▷: double while looping, else jump 4 beats on.
    CallFwd,
    BeatJump(f64),
    /// Hot cue pad 0..8: jump to it (its release ends a preview), or set it.
    HotCue(usize),
    HotCueClear(usize),
    /// Pitch bend while held: up (faster) or down.
    Bend(bool),
    /// Loads the selected library row on the deck.
    Load,
}

impl Button {
    /// Whether its release does something (the rest act on the press).
    pub fn momentary(self) -> bool {
        matches!(self, Button::Cue | Button::ToStart | Button::HotCue(_) | Button::Bend(_))
    }
}

/// A control with a travel 0..=1 (see [`Continuous::action`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Continuous {
    /// The tempo fader: 0 is −range, 1 +range (soft takeover: [`DeckControls::tempo`]).
    Pitch,
    Trim,
    Eq(EqBand),
    /// The filter: 0 low-pass, ½ off, 1 high-pass.
    Filter,
    Fader,
    Crossfader,
    MasterVolume,
    HeadphoneMix,
    HeadphoneLevel,
}

/// A bipolar control's travel as −1..=1, with `dead` around the centre read as 0.
pub fn centred(t: f64, dead: f64) -> f64 {
    let x = (t.clamp(0.0, 1.0) - 0.5) * 2.0;
    if x.abs() < dead { 0.0 } else { x.clamp(-1.0, 1.0) }
}

fn band_id(band: EqBand) -> &'static str {
    match band {
        EqBand::Low => "low",
        EqBand::Mid => "mid",
        EqBand::High => "high",
    }
}

impl Continuous {
    /// The command for travel `t` on deck `d` (`dead`: the centre's dead zone, see [`centred`]).
    /// Not for the pitch, which goes through [`DeckControls::tempo`].
    pub fn action(self, d: DeckId, t: f64, dead: f64) -> Option<Action> {
        let t = t.clamp(0.0, 1.0);
        Some(match self {
            Continuous::Pitch => return None,
            Continuous::Trim => run("deck.trim", d, json!({"db": round(f64::from(crate::knob::trim_db(t as f32)), 1)})),
            Continuous::Eq(band) => run("deck.eq", d, json!({"band": band_id(band), "db": round(f64::from(crate::knob::eq_db(t as f32)), 1)})),
            Continuous::Filter => run("deck.filter", d, json!({"value": round(centred(t, dead), 3)})),
            Continuous::Fader => run("deck.fader", d, json!({"value": round(t, 3)})),
            Continuous::Crossfader => Action::Run("deck.xfader", json!({"value": round(t * 2.0 - 1.0, 3)})),
            Continuous::MasterVolume => {
                let db = if t <= 0.0 { -60.0 } else { ((t - 1.0) * 40.0).max(-60.0) };
                Action::Run("deck.master_volume", json!({"db": round(db, 1)}))
            }
            Continuous::HeadphoneMix => Action::Run("deck.headphone_mix", json!({"value": round(t, 3)})),
            Continuous::HeadphoneLevel => {
                let (lo, hi) = soundcraft_mix::HEADPHONE_DB;
                let db = f64::from(lo) + t * f64::from(hi - lo);
                Action::Run("deck.headphone_level", json!({"db": round(db, 1)}))
            }
        })
    }

    /// Its travel now, from what the decks report (`m` the mixer's state).
    pub fn travel(self, d: DeckId, ctx: &Ctx, m: &soundcraft_mix::DecksState) -> f64 {
        let s = &ctx.deck(d).state;
        let fin = |x: f32, or: f32| if x.is_finite() { x } else { or };
        let t = match self {
            Continuous::Pitch => {
                let r = range_pct(ctx, d);
                return ((s.pitch * 100.0 / r + 1.0) / 2.0).clamp(0.0, 1.0);
            }
            Continuous::Trim => crate::knob::trim_travel(fin(s.trim_db, 0.0)),
            Continuous::Eq(band) => crate::knob::eq_travel(fin(s.eq_db.get(band.index()).copied().unwrap_or(0.0), 0.0)),
            Continuous::Filter => fin(s.filter, 0.0) * 0.5 + 0.5,
            Continuous::Fader => fin(s.fader, 1.0),
            Continuous::Crossfader => fin(m.xfader, 0.0) * 0.5 + 0.5,
            Continuous::MasterVolume => (fin(m.master_db, 0.0) / 40.0 + 1.0).clamp(0.0, 1.0),
            Continuous::HeadphoneMix => fin(m.headphone_mix, 0.0),
            Continuous::HeadphoneLevel => {
                let (lo, hi) = soundcraft_mix::HEADPHONE_DB;
                (fin(m.headphone_db, soundcraft_mix::HEADPHONE_DEFAULT_DB) - lo) / (hi - lo)
            }
        };
        f64::from(t.clamp(0.0, 1.0))
    }
}

/// What the deck controls remember (see the module docs): per deck.
#[derive(Debug, Default)]
pub struct DeckControls {
    /// The toggles last asked for: play, sync, headphone cue, master tempo.
    pending: [[Option<(bool, f64)>; 4]; 2],
    cue_held: [bool; 2],
    /// A hot cue pad held after a jump, which sends its release.
    held_pad: [Option<usize>; 2],
    /// Jog search (beats) and nudge (counts) waiting for the frame.
    search: [f64; 2],
    nudge: [f64; 2],
    /// A pitch bend held: +1 up, −1 down.
    bend: [i8; 2],
    tempo: [Takeover; 2],
}

impl DeckControls {
    fn pending(&mut self, d: DeckId, k: usize) -> Option<&mut Option<(bool, f64)>> {
        self.pending.get_mut(d.index()).and_then(|p| p.get_mut(k))
    }

    fn flip(&mut self, d: DeckId, k: usize, reported: bool, now: f64) -> bool {
        self.pending(d, k).map_or(!reported, |p| toggle(p, reported, now))
    }

    /// Deck `d`'s button `b` pressed (`down`) or released.
    pub fn button(&mut self, d: DeckId, b: Button, down: bool, ctx: &Ctx) -> Vec<Action> {
        let dc = ctx.deck(d);
        let s = &dc.state;
        let i = d.index();
        let mut out = Vec::new();
        match b {
            // A held CUE let go, also on SHIFT+CUE's note (SHIFT pressed while it was held).
            Button::Cue | Button::ToStart if !down => {
                if self.cue_held.get_mut(i).is_some_and(std::mem::take) {
                    out.push(run("deck.cue", d, json!({"down": false})));
                }
            }
            // The release may come on another note (SHIFT pressed while the pad was held).
            Button::HotCue(pad) if !down => {
                if let Some(h) = self.held_pad.get_mut(i)
                    && *h == Some(pad)
                {
                    *h = None;
                    out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": "release"})));
                }
            }
            Button::Bend(_) if !down => {
                if let Some(x) = self.bend.get_mut(i) {
                    *x = 0;
                }
            }
            _ if !down => {}
            Button::HeadphoneCue => {
                let on = self.flip(d, 2, s.headphone_cue, ctx.now);
                out.push(run("deck.headphone_cue", d, json!({"on": on})));
            }
            Button::MasterTempo => {
                let on = self.flip(d, 3, s.key_lock, ctx.now);
                out.push(run("deck.master_tempo", d, json!({"on": on})));
            }
            Button::Load => out.push(Action::Load(Some(d))),
            Button::Bend(up) => {
                if let Some(x) = self.bend.get_mut(i) {
                    *x = if up { 1 } else { -1 };
                }
            }
            // The rest act on a loaded deck only.
            _ if !dc.loaded => {}
            Button::Play => {
                let play = self.flip(d, 0, s.playing, ctx.now);
                out.push(run(if play { "deck.play" } else { "deck.pause" }, d, json!({})));
            }
            Button::Cue => {
                if self.cue_held.get(i).is_some_and(|h| !h) {
                    out.push(run("deck.cue", d, json!({"down": true})));
                    if let Some(x) = self.cue_held.get_mut(i) {
                        *x = true;
                    }
                }
            }
            Button::ToStart => out.push(run("deck.seek", d, json!({"at": 0.0}))),
            Button::Sync => {
                let on = self.flip(d, 1, s.synced, ctx.now);
                out.push(run("deck.sync", d, json!({"on": on})));
            }
            Button::SyncOff => out.push(run("deck.sync", d, json!({"on": false}))),
            Button::TempoRange => {
                let now = s.range;
                let next = PITCH_RANGES.iter().position(|r| (r - now).abs() < 1e-6).and_then(|k| PITCH_RANGES.get(k + 1)).or(PITCH_RANGES.first());
                if let Some(r) = next {
                    out.push(run("deck.pitch", d, json!({"range": round(r * 100.0, 0)})));
                }
            }
            Button::Loop4 => {
                out.push(if s.loop_on { run("deck.loop", d, json!({"action": "exit"})) } else { run("deck.loop", d, json!({"beats": 4})) })
            }
            Button::LoopIn => out.push(run("deck.loop", d, json!({"action": "in"}))),
            Button::LoopOut => out.push(run("deck.loop", d, json!({"action": "out"}))),
            Button::Reloop => out.push(run("deck.loop", d, json!({"action": "reloop"}))),
            Button::Halve => out.push(run("deck.loop", d, json!({"action": "halve"}))),
            Button::Double => out.push(run("deck.loop", d, json!({"action": "double"}))),
            Button::CallBack => {
                out.push(if s.loop_on { run("deck.loop", d, json!({"action": "halve"})) } else { run("deck.beatjump", d, json!({"beats": -4})) })
            }
            Button::CallFwd => {
                out.push(if s.loop_on { run("deck.loop", d, json!({"action": "double"})) } else { run("deck.beatjump", d, json!({"beats": 4})) })
            }
            Button::BeatJump(beats) => {
                if beats.is_finite() && beats != 0.0 {
                    out.push(run("deck.beatjump", d, json!({"beats": beats.clamp(-64.0, 64.0)})));
                }
            }
            Button::HotCue(pad) => {
                let set = s.hot.get(pad).copied().flatten().is_some();
                out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": if set { "jump" } else { "set" }})));
                if let Some(h) = self.held_pad.get_mut(i) {
                    *h = set.then_some(pad);
                }
            }
            Button::HotCueClear(pad) => out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": "clear"}))),
        }
        out
    }

    /// The jog turned by `ticks` counts on deck `d`: a nudge while it is heard, else a search;
    /// `fast` searches [`FAST_BEATS_PER_TURN`] a turn. `scale` multiplies the counts (a learned
    /// jog's sensitivity). Sent by [`DeckControls::tick`].
    pub fn jog(&mut self, d: DeckId, ticks: f64, fast: bool, scale: f64, ctx: &Ctx) {
        let dc = ctx.deck(d);
        if !dc.loaded || !ticks.is_finite() {
            return;
        }
        let i = d.index();
        let ticks = ticks * scale;
        if fast {
            if let Some(b) = self.search.get_mut(i) {
                *b += ticks * FAST_BEATS_PER_TURN / JOG_TICKS;
            }
        } else if dc.state.playing || dc.state.previewing {
            if let Some(n) = self.nudge.get_mut(i) {
                *n += ticks;
            }
        } else if let Some(b) = self.search.get_mut(i) {
            *b += ticks * SEARCH_BEATS_PER_TURN / JOG_TICKS;
        }
    }

    /// The tempo fader of deck `d` at `pct` (percent), with soft takeover unless `pickup` is off
    /// or the deck follows a master on the other deck.
    pub fn tempo(&mut self, d: DeckId, pct: f64, pickup: bool, ctx: &Ctx) -> Option<Action> {
        let dc = ctx.deck(d);
        let range = range_pct(ctx, d);
        let p = self.tempo.get_mut(d.index())?;
        if !dc.loaded {
            p.observe(pct);
            return None;
        }
        let window = range * 2.0 * 1.5 / 127.0;
        if !p.admit(pct, dc.state.pitch * 100.0, window, follows(ctx, d) || !pickup, ctx.now) {
            return None;
        }
        Some(run("deck.pitch", d, json!({"value": pct})))
    }

    /// Once a frame, `dt` seconds after the last: the jog's search and nudge, a held pitch bend,
    /// and the tempo fader's takeover re-armed when the deck's tempo moved elsewhere.
    pub fn tick(&mut self, ctx: &Ctx, dt: f64) -> Vec<Action> {
        let mut out = Vec::new();
        let dt = dt.clamp(1.0 / 240.0, 0.1);
        for d in DeckId::ALL {
            let i = d.index();
            let dc = ctx.deck(d);
            let beats = self.search.get_mut(i).map(std::mem::take).unwrap_or(0.0);
            let ticks = self.nudge.get_mut(i).map(std::mem::take).unwrap_or(0.0);
            let bend = self.bend.get(i).copied().unwrap_or(0);
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
                let heard = dc.state.playing || dc.state.previewing;
                let delta = ticks / dt / NUDGE_TICKS_S + if heard { f64::from(bend) * BEND } else { 0.0 };
                if delta != 0.0 && delta.is_finite() {
                    out.push(run("deck.jog", d, json!({"delta": round(delta.clamp(-1.0, 1.0), 4), "mode": "nudge"})));
                }
            }
            let range = range_pct(ctx, d);
            if let Some(p) = self.tempo.get_mut(i) {
                p.rearm(dc.state.pitch * 100.0, range * 2.0 * 1.5 / 127.0, follows(ctx, d), ctx.now);
            }
        }
        out
    }

    /// Lets go of everything held (a CUE preview, a hot cue preview, a pitch bend): the commands
    /// that do it.
    pub fn release_all(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        for d in DeckId::ALL {
            let i = d.index();
            if self.cue_held.get_mut(i).is_some_and(std::mem::take) {
                out.push(run("deck.cue", d, json!({"down": false})));
            }
            if let Some(pad) = self.held_pad.get_mut(i).and_then(Option::take) {
                out.push(run("deck.hotcue", d, json!({"n": pad + 1, "action": "release"})));
            }
            if let Some(b) = self.bend.get_mut(i) {
                *b = 0;
            }
        }
        out
    }

    /// Whether a pitch bend is held on a deck that is heard (the UI then runs at a full frame
    /// rate, so the bend doesn't pulse).
    pub fn bending(&self, ctx: &Ctx) -> bool {
        DeckId::ALL.into_iter().any(|d| {
            let s = &ctx.deck(d).state;
            self.bend.get(d.index()).is_some_and(|b| *b != 0) && (s.playing || s.previewing)
        })
    }

    /// Whether anything is held (a CUE, a hot cue, a bend).
    pub fn holding(&self) -> bool {
        self.cue_held.iter().any(|h| *h) || self.held_pad.iter().any(Option::is_some) || self.bend.iter().any(|b| *b != 0)
    }
}

/// A deck control MIDI Learn can map (`midi.map`'s `{kind: "deck", deck, control, …}`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum DeckControl {
    Play,
    Cue,
    Sync,
    MasterTempo,
    /// The tempo fader (absolute, soft takeover).
    Pitch,
    PitchRange,
    PitchBendUp,
    PitchBendDown,
    /// The jog wheel (relative): nudge while playing, search while paused.
    Jog,
    /// Hot cue `n` (1..=8): jump or set; its release ends a preview.
    HotCue {
        n: u8,
    },
    HotCueClear {
        n: u8,
    },
    /// 4 BEAT / EXIT.
    Loop4,
    LoopIn,
    LoopOut,
    LoopHalve,
    LoopDouble,
    Reloop,
    /// A beat jump of the size the Decks tab shows for the deck.
    JumpBack,
    JumpForward,
    /// A beat jump of `beats` (±).
    BeatJump {
        beats: f64,
    },
    /// Loads the selected library row on the deck.
    Load,
    Trim,
    EqHigh,
    EqMid,
    EqLow,
    Filter,
    Fader,
    HeadphoneCue,
}

/// What a deck control is to a mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Button(Button),
    Continuous(Continuous),
    /// Relative only: the jog.
    Jog,
    /// A beat jump of the Decks tab's size: back (false) or on.
    Jump(bool),
}

impl DeckControl {
    pub fn kind(self) -> Kind {
        let pad = |n: u8| usize::from(n.clamp(1, 8) - 1);
        match self {
            DeckControl::Play => Kind::Button(Button::Play),
            DeckControl::Cue => Kind::Button(Button::Cue),
            DeckControl::Sync => Kind::Button(Button::Sync),
            DeckControl::MasterTempo => Kind::Button(Button::MasterTempo),
            DeckControl::Pitch => Kind::Continuous(Continuous::Pitch),
            DeckControl::PitchRange => Kind::Button(Button::TempoRange),
            DeckControl::PitchBendUp => Kind::Button(Button::Bend(true)),
            DeckControl::PitchBendDown => Kind::Button(Button::Bend(false)),
            DeckControl::Jog => Kind::Jog,
            DeckControl::HotCue { n } => Kind::Button(Button::HotCue(pad(n))),
            DeckControl::HotCueClear { n } => Kind::Button(Button::HotCueClear(pad(n))),
            DeckControl::Loop4 => Kind::Button(Button::Loop4),
            DeckControl::LoopIn => Kind::Button(Button::LoopIn),
            DeckControl::LoopOut => Kind::Button(Button::LoopOut),
            DeckControl::LoopHalve => Kind::Button(Button::Halve),
            DeckControl::LoopDouble => Kind::Button(Button::Double),
            DeckControl::Reloop => Kind::Button(Button::Reloop),
            DeckControl::JumpBack => Kind::Jump(false),
            DeckControl::JumpForward => Kind::Jump(true),
            DeckControl::BeatJump { beats } => Kind::Button(Button::BeatJump(beats)),
            DeckControl::Load => Kind::Button(Button::Load),
            DeckControl::Trim => Kind::Continuous(Continuous::Trim),
            DeckControl::EqHigh => Kind::Continuous(Continuous::Eq(EqBand::High)),
            DeckControl::EqMid => Kind::Continuous(Continuous::Eq(EqBand::Mid)),
            DeckControl::EqLow => Kind::Continuous(Continuous::Eq(EqBand::Low)),
            DeckControl::Filter => Kind::Continuous(Continuous::Filter),
            DeckControl::Fader => Kind::Continuous(Continuous::Fader),
            DeckControl::HeadphoneCue => Kind::Button(Button::HeadphoneCue),
        }
    }

    /// Whether its parameters are in range (a hot cue 1..=8, a beat jump ±1..=64).
    pub fn valid(self) -> bool {
        match self {
            DeckControl::HotCue { n } | DeckControl::HotCueClear { n } => (1..=8).contains(&n),
            DeckControl::BeatJump { beats } => beats.is_finite() && beats != 0.0 && beats.abs() <= 64.0,
            _ => true,
        }
    }

    /// "Cue", "Hot Cue C", "Beat Jump −4".
    pub fn label(self) -> String {
        let letter = |n: u8| char::from(b'A'.saturating_add(n.clamp(1, 8) - 1));
        match self {
            DeckControl::Play => "Play".into(),
            DeckControl::Cue => "Cue".into(),
            DeckControl::Sync => "Sync".into(),
            DeckControl::MasterTempo => "Master Tempo".into(),
            DeckControl::Pitch => "Pitch".into(),
            DeckControl::PitchRange => "Pitch Range".into(),
            DeckControl::PitchBendUp => "Pitch Bend +".into(),
            DeckControl::PitchBendDown => "Pitch Bend −".into(),
            DeckControl::Jog => "Jog".into(),
            DeckControl::HotCue { n } => format!("Hot Cue {}", letter(n)),
            DeckControl::HotCueClear { n } => format!("Clear Hot Cue {}", letter(n)),
            DeckControl::Loop4 => "4-Beat Loop / Exit".into(),
            DeckControl::LoopIn => "Loop In".into(),
            DeckControl::LoopOut => "Loop Out".into(),
            DeckControl::LoopHalve => "Loop Halve".into(),
            DeckControl::LoopDouble => "Loop Double".into(),
            DeckControl::Reloop => "Reloop / Exit".into(),
            DeckControl::JumpBack => "Beat Jump Back".into(),
            DeckControl::JumpForward => "Beat Jump Forward".into(),
            DeckControl::BeatJump { beats } => format!("Beat Jump {beats:+}"),
            DeckControl::Load => "Load".into(),
            DeckControl::Trim => "Trim".into(),
            DeckControl::EqHigh => "EQ HI".into(),
            DeckControl::EqMid => "EQ MID".into(),
            DeckControl::EqLow => "EQ LOW".into(),
            DeckControl::Filter => "Filter".into(),
            DeckControl::Fader => "Fader".into(),
            DeckControl::HeadphoneCue => "Headphone Cue".into(),
        }
    }
}

/// A DJ mixer control that isn't per deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixerControl {
    Crossfader,
    MasterVolume,
    HeadphoneMix,
    HeadphoneLevel,
    MasterCue,
}

impl MixerControl {
    /// `None`: a button (MASTER CUE).
    pub fn continuous(self) -> Option<Continuous> {
        match self {
            MixerControl::Crossfader => Some(Continuous::Crossfader),
            MixerControl::MasterVolume => Some(Continuous::MasterVolume),
            MixerControl::HeadphoneMix => Some(Continuous::HeadphoneMix),
            MixerControl::HeadphoneLevel => Some(Continuous::HeadphoneLevel),
            MixerControl::MasterCue => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MixerControl::Crossfader => "Crossfader",
            MixerControl::MasterVolume => "Master Volume",
            MixerControl::HeadphoneMix => "Headphone Mix",
            MixerControl::HeadphoneLevel => "Headphone Level",
            MixerControl::MasterCue => "Master Cue",
        }
    }
}

/// MASTER CUE: toggles from what the decks report, as the controller's button.
pub fn master_cue() -> Action {
    Action::Run("deck.master_cue", json!({}))
}

/// A library browser control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryControl {
    /// The browse knob (relative): rows per tick.
    Browse,
    Previous,
    Next,
    /// Loads the selected row on an empty deck (the browse knob's press).
    Load,
}

impl LibraryControl {
    pub fn label(self) -> &'static str {
        match self {
            LibraryControl::Browse => "Browse",
            LibraryControl::Previous => "Previous Track",
            LibraryControl::Next => "Next Track",
            LibraryControl::Load => "Load to an Empty Deck",
        }
    }
}

/// `DeckId` as a mapping keeps it: "a" or "b".
pub mod deck_id {
    use serde::{Deserialize, Deserializer, Serializer};
    use soundcraft_mix::DeckId;

    pub fn serialize<S: Serializer>(d: &DeckId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(d.id())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DeckId, D::Error> {
        let s = String::deserialize(d)?;
        DeckId::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("deck \"{s}\": a or b")))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::controller_layouts::DeckCtx;
    use soundcraft_mix::DeckState;

    fn ctx() -> Ctx {
        let loaded = DeckCtx { state: DeckState { load: 1, range: 0.08, ..DeckState::default() }, loaded: true, loop_beats: None };
        Ctx { decks: [loaded, loaded], now: 10.0, headphone_knob: false }
    }

    #[test]
    fn a_takeover_waits_for_the_target_and_re_arms_once_it_moved_elsewhere() {
        let mut t = Takeover::default();
        assert!(!t.admit(0.2, 0.5, 0.01, false, 0.0), "far below");
        assert!(t.admit(0.6, 0.5, 0.01, false, 0.1), "crossed");
        assert!(t.admit(0.1, 0.5, 0.01, false, 0.2), "picked: follows");
        t.rearm(0.8, 0.01, false, 0.3);
        assert!(t.picked(), "not settled yet");
        t.rearm(0.8, 0.01, false, 1.0);
        assert!(!t.picked(), "moved elsewhere");
        assert!(t.admit(0.0, 0.5, 0.01, true, 1.1), "bypassed");
    }

    #[test]
    fn master_tempo_toggles_with_a_pending_state_and_a_bend_nudges_while_held() {
        let mut k = DeckControls::default();
        let mut c = ctx();
        assert_eq!(k.button(DeckId::A, Button::MasterTempo, true, &c), vec![run("deck.master_tempo", DeckId::A, json!({"on": true}))]);
        assert_eq!(k.button(DeckId::A, Button::MasterTempo, true, &c), vec![run("deck.master_tempo", DeckId::A, json!({"on": false}))]);
        c.decks[0].state.playing = true;
        assert!(k.button(DeckId::A, Button::Bend(false), true, &c).is_empty());
        assert!(k.bending(&c) && k.holding());
        assert_eq!(k.tick(&c, 0.016), vec![run("deck.jog", DeckId::A, json!({"delta": -BEND, "mode": "nudge"}))]);
        assert_eq!(k.tick(&c, 0.016).len(), 1, "every frame while held");
        k.button(DeckId::A, Button::Bend(false), false, &c);
        assert!(k.tick(&c, 0.016).is_empty() && !k.holding(), "released");
        k.button(DeckId::B, Button::Bend(true), true, &c);
        assert!(k.release_all().is_empty() && !k.holding(), "a bend stops by itself");
    }

    #[test]
    fn each_continuous_control_round_trips_its_travel() {
        let mut c = ctx();
        let mut m = soundcraft_mix::DecksState::default();
        c.decks[0].state.trim_db = 6.0;
        c.decks[0].state.fader = 0.25;
        m.xfader = -0.5;
        m.master_db = -10.0;
        m.headphone_db = -30.0;
        assert!((Continuous::Trim.travel(DeckId::A, &c, &m) - 0.75).abs() < 1e-6);
        assert!((Continuous::Fader.travel(DeckId::A, &c, &m) - 0.25).abs() < 1e-6);
        assert!((Continuous::Crossfader.travel(DeckId::A, &c, &m) - 0.25).abs() < 1e-6);
        assert!((Continuous::MasterVolume.travel(DeckId::A, &c, &m) - 0.75).abs() < 1e-6);
        assert!((Continuous::HeadphoneLevel.travel(DeckId::A, &c, &m) - 0.5).abs() < 1e-6);
        assert_eq!(Continuous::Filter.action(DeckId::B, 64.0 / 127.0, 1.5 / 127.0), Some(run("deck.filter", DeckId::B, json!({"value": 0.0}))));
        assert_eq!(Continuous::HeadphoneLevel.action(DeckId::A, 0.5, 0.0), Some(Action::Run("deck.headphone_level", json!({"db": -30.0}))));
    }

    #[test]
    fn deck_controls_serialise_flat_and_refuse_bad_parameters() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct T {
            #[serde(with = "deck_id")]
            deck: DeckId,
            #[serde(flatten)]
            control: DeckControl,
        }
        let t = T { deck: DeckId::B, control: DeckControl::HotCue { n: 3 } };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v, json!({"deck": "b", "control": "hot_cue", "n": 3}));
        assert_eq!(serde_json::from_value::<T>(v).unwrap(), t);
        assert!(serde_json::from_value::<T>(json!({"deck": "c", "control": "play"})).is_err());
        assert!(!DeckControl::HotCue { n: 9 }.valid() && !DeckControl::BeatJump { beats: 0.0 }.valid());
        assert!(DeckControl::BeatJump { beats: -4.0 }.valid());
    }
}
