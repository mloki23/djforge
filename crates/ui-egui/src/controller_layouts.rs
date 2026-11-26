//! Built-in controller layouts: a ready-made mapping of a known DJ controller to the decks and
//! the DJ mixer, with its lights following the decks. The one so far is the Pioneer DJ
//! DDJ-FLX4 ([`crate::layouts::ddj_flx4`], documented in `docs/hardware/ddj-flx4.md`).
//!
//! **Detection.** An open (enabled) MIDI input whose raw name a layout recognises uses it, unless
//! its "Use built-in layout" toggle in MIDI Devices is off (`midi.layout`, kept per device in the
//! preferences, `UiState::midi_layouts_off`). The device's output half is paired by name
//! ([`MidiHost::paired_output`]) and opened for the lights.
//!
//! **Input.** MIDI learn sees each event first ([`crate::midi_learn::handle`]): a learned mapping
//! of the message wins, so MIDI Learn overrides or extends a layout. An event no mapping takes goes
//! to the device's layout ([`handle`]), which turns it into deck commands ([`Action`]s) run as
//! a click on the Decks tab would run them. Every message of the controller a layout knows is
//! claimed, so none plays the live instrument (it joins the player's mapped-control filter
//! through [`controls`]); unmapped ones are claimed too.
//!
//! **Lights.** Every frame ([`frame`]) a layout says what each light should show for the decks'
//! published state (`decks.state`); only the changes are sent, the level meters at most
//! [`METER_HZ`] times a second. When the output opens (or reopens after a replug) everything is
//! sent; when the layout stops (the input is disabled or unplugged, the layout turned off, the
//! app quits: [`shutdown`]) every light is turned off. MIDI output never runs on the audio
//! thread: the host's outputs queue to a sender thread.

use std::collections::HashMap;

use serde_json::{Value, json};
use soundcraft_midi_io::{MidiMessage, PortChange, PortId};
use soundcraft_mix::{DeckId, DeckState};
use soundcraft_playback::MidiControl;

use crate::SoundApp;
use crate::midi_input::{MidiHost, MidiInputPref, REFRESH_S};

/// The level meters' update rate at most (Hz).
pub const METER_HZ: f64 = 30.0;

/// One deck as a layout sees it.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeckCtx {
    pub state: DeckState,
    /// It has a track.
    pub loaded: bool,
    /// The loop's length in beats, while one plays and the track has a grid.
    pub loop_beats: Option<f64>,
}

/// What a layout reads: the decks, and the UI clock (seconds).
#[derive(Debug, Clone, Copy, Default)]
pub struct Ctx {
    pub decks: [DeckCtx; 2],
    pub now: f64,
}

impl Ctx {
    /// Deck `d`.
    pub fn deck(&self, d: DeckId) -> &DeckCtx {
        match d {
            DeckId::A => &self.decks[0],
            DeckId::B => &self.decks[1],
        }
    }

    /// The decks as the app last heard from the audio engine.
    pub fn of(app: &SoundApp, now: f64) -> Ctx {
        let mut ctx = Ctx { now, ..Ctx::default() };
        for (i, c) in ctx.decks.iter_mut().enumerate() {
            let state = app.engine.deck_status.decks.get(i).copied().unwrap_or_default();
            let track = app.engine.deck_tracks.get(i).and_then(Option::as_ref);
            let loop_beats = match (state.loop_on, state.loop_in, state.loop_out, track.and_then(|t| t.grid.as_ref())) {
                (true, Some(a), Some(b), Some(g)) => Some(g.beat_at(b) - g.beat_at(a)),
                _ => None,
            };
            *c = DeckCtx { state, loaded: track.is_some(), loop_beats };
        }
        ctx
    }
}

/// What a control asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A command (`deck.*`), as the Decks tab runs it.
    Run(&'static str, Value),
    /// Moves the library selection by this many rows.
    Browse(isize),
    /// Loads the selected library row onto the deck (`None`: the first empty deck). Load Lock
    /// holds: a playing deck refuses, and the status line says so.
    Load(Option<DeckId>),
}

/// A light's wanted state: the message that sets it, and whether it is a meter (sent at most
/// [`METER_HZ`] times a second).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Light {
    pub msg: MidiMessage,
    pub throttled: bool,
}

/// A controller's layout: its input table and its lights.
pub trait Controller {
    /// The layout's name ("DDJ-FLX4").
    fn name(&self) -> &'static str;
    /// Whether `msg` is one of the controller's (it never plays the live instrument).
    fn claims(&self, msg: &MidiMessage) -> bool;
    /// Every message it claims, for the player's mapped-control filter.
    fn controls(&self) -> Vec<MidiControl>;
    /// One message from the controller.
    fn input(&mut self, msg: &MidiMessage, ctx: &Ctx) -> Vec<Action>;
    /// Once a frame, `dt` seconds after the last: what accumulates over a frame (jog turns).
    fn tick(&mut self, ctx: &Ctx, dt: f64) -> Vec<Action>;
    /// Every light's wanted state.
    fn lights(&mut self, ctx: &Ctx) -> Vec<Light>;
    /// One-off messages to send now (not lights: a track-loaded flash).
    fn events(&mut self, ctx: &Ctx) -> Vec<MidiMessage>;
    /// True when the lights should all be sent again (the pads changed mode).
    fn take_resend(&mut self) -> bool;
    /// The other half of a 14-bit control `msg` belongs to (its LSB for an MSB, and back), so
    /// a MIDI Learn mapping of either half covers both.
    fn twin(&self, msg: &MidiMessage) -> Option<MidiControl>;
    /// Lets go of everything held (a CUE preview, a hot cue preview): the commands that do it.
    fn release_all(&mut self, ctx: &Ctx) -> Vec<Action>;
    /// Whether a jog nudge is held now (the UI then runs at a full frame rate).
    fn held(&self, ctx: &Ctx) -> bool;
    /// The messages that turn every light off.
    fn dark(&self) -> Vec<MidiMessage>;
}

/// A built-in layout: its name, which devices it is for, and a fresh state.
pub struct LayoutInfo {
    pub name: &'static str,
    pub recognise: fn(&str) -> bool,
    pub make: fn() -> Box<dyn Controller>,
}

/// The built-in layouts.
pub const LAYOUTS: &[LayoutInfo] = &[LayoutInfo {
    name: crate::layouts::ddj_flx4::NAME,
    recognise: crate::layouts::ddj_flx4::recognise,
    make: || Box::new(crate::layouts::ddj_flx4::Flx4::new()),
}];

/// The layout for an input with this raw name.
pub fn layout_for(raw_name: &str) -> Option<&'static LayoutInfo> {
    LAYOUTS.iter().find(|l| (l.recognise)(raw_name))
}

/// The lights as last sent, to send only changes.
#[derive(Debug, Default)]
pub struct Mirror {
    /// Value by (status byte, data 1).
    sent: HashMap<(u8, u8), u8>,
    /// When the meters were last sent.
    meters_at: Option<f64>,
}

impl Mirror {
    /// Forget what was sent: the next [`Mirror::diff`] sends every light.
    pub fn reset(&mut self) {
        self.sent.clear();
        self.meters_at = None;
    }

    /// Sends (through `send`) what brings the controller from what was sent to `lights`, at
    /// `now` (seconds); meters wait until [`METER_HZ`] allows. A light counts as sent only when
    /// `send` took it. Returns what was sent.
    pub fn diff(&mut self, lights: &[Light], now: f64, mut send: impl FnMut(MidiMessage) -> bool) -> Vec<MidiMessage> {
        let meters_due = self.meters_at.is_none_or(|at| now - at >= 1.0 / METER_HZ || now < at);
        let mut out = Vec::new();
        let mut meters_sent = false;
        for l in lights {
            let (b, _) = l.msg.wire();
            let [status, data1, value] = b;
            if self.sent.get(&(status, data1)) == Some(&value) {
                continue;
            }
            if l.throttled {
                if !meters_due {
                    continue;
                }
                meters_sent = true;
            }
            if send(l.msg) {
                self.sent.insert((status, data1), value);
                out.push(l.msg);
            }
        }
        if meters_sent {
            self.meters_at = Some(now);
        }
        out
    }
}

/// A device using a layout.
struct Device {
    input: PortId,
    raw_name: String,
    backend_id: String,
    layout: &'static LayoutInfo,
    logic: Box<dyn Controller>,
    /// Its output port, once open.
    out: Option<PortId>,
    out_name: Option<String>,
    out_error: Option<String>,
    mirror: Mirror,
    /// When to try opening its output again, and the wait after the next failure (1 s, doubling
    /// to 30 s).
    retry_at: f64,
    backoff: f64,
}

/// The longest wait between tries at an output that won't open (seconds).
const MAX_BACKOFF_S: f64 = 30.0;

/// The devices using a built-in layout, and their upkeep.
#[derive(Default)]
pub struct Controllers {
    devices: Vec<Device>,
    /// When outputs were last looked for (UI seconds).
    looked: Option<f64>,
    last_frame: Option<f64>,
    /// The outputs' dropped count last seen: a rise means lights were lost, so all are resent.
    dropped: u64,
}

impl Controllers {
    /// Whether any device uses a layout (MIDI then wakes the UI at once).
    pub fn active(&self) -> bool {
        !self.devices.is_empty()
    }

    /// Whether a layout drives lights: they blink, so the UI keeps a few frames a second.
    pub fn lit(&self) -> bool {
        self.devices.iter().any(|d| d.out.is_some())
    }
}

/// How often the UI should run for the controllers, which act and light up only in UI frames:
/// a full frame rate (60 Hz) while a jog nudge is held, so nudges don't pulse; 30 Hz while a
/// deck plays, so the meters reach their rate; 8 Hz while lights blink; else nothing. An OS
/// that throttles a minimised window slows this down too (acceptable: the lights lag then).
pub fn repaint_every(app: &SoundApp) -> Option<std::time::Duration> {
    let c = &app.controllers;
    if c.devices.is_empty() {
        return None;
    }
    let ctx = Ctx::of(app, app.midi_learn.now);
    let ms = if c.devices.iter().any(|d| d.logic.held(&ctx)) {
        16
    } else if ctx.decks.iter().any(|d| d.state.playing || d.state.previewing) {
        33
    } else if c.lit() {
        125
    } else {
        return None;
    };
    Some(std::time::Duration::from_millis(ms))
}

/// Whether the device (raw name, backend id) has its built-in layout turned off.
pub fn layout_off(prefs: &[MidiInputPref], raw_name: &str, backend_id: &str) -> bool {
    prefs.iter().any(|p| p.raw_name == raw_name && p.backend_id == backend_id)
}

/// The open inputs that use a layout now: (port, raw name, backend id, layout).
fn wanted(app: &SoundApp) -> Vec<(PortId, String, String, &'static LayoutInfo)> {
    let Some(h) = &app.midi_host else { return Vec::new() };
    let src = h.source();
    h.opened()
        .iter()
        .filter(|id| h.input_open(**id))
        .filter_map(|id| {
            let (raw, backend) = (src.raw_name(*id)?, src.backend_id(*id)?);
            let layout = layout_for(raw)?;
            (!layout_off(&app.ui.midi_layouts_off, raw, backend)).then(|| (*id, raw.to_string(), backend.to_string(), layout))
        })
        .collect()
}

/// Stops a device's layout: lets go of what it holds (a CUE preview must not keep playing),
/// turns its lights off and closes its output (queued behind the lights).
fn stop(app: &mut SoundApp, mut dev: Device, now: f64) {
    let ctx = Ctx::of(app, now);
    for a in dev.logic.release_all(&ctx) {
        perform(app, dev.layout.name, a);
    }
    let (Some(h), Some(out)) = (app.midi_host.as_mut(), dev.out) else { return };
    let Some(o) = h.outputs_mut() else { return };
    for m in dev.logic.dark() {
        o.send(out, m);
    }
    if let Some(name) = o.port_name(out).map(str::to_string) {
        o.close(&name);
    }
}

/// Every UI frame, before MIDI learn's: starts the layouts of devices that opened (or whose
/// layout was turned on) and stops the others, so the frame's events find their layout.
pub fn sync(app: &mut SoundApp, now: f64) {
    let wanted = wanted(app);
    // Devices that stopped: lights off, output closed.
    let (keep, gone): (Vec<Device>, Vec<Device>) = std::mem::take(&mut app.controllers.devices)
        .into_iter()
        .partition(|d| wanted.iter().any(|w| w.0 == d.input && w.1 == d.raw_name && w.2 == d.backend_id));
    app.controllers.devices = keep;
    for d in gone {
        stop(app, d, now);
    }
    for (input, raw_name, backend_id, layout) in wanted {
        if !app.controllers.devices.iter().any(|d| d.input == input) {
            log::info!("{raw_name}: built-in layout {}", layout.name);
            app.controllers.devices.push(Device {
                input,
                raw_name,
                backend_id,
                layout,
                logic: (layout.make)(),
                out: None,
                out_name: None,
                out_error: None,
                mirror: Mirror::default(),
                retry_at: 0.0,
                backoff: REFRESH_S,
            });
            // Look for its output now, not at the next refresh.
            app.controllers.looked = None;
        }
    }
}

/// Every UI frame at `now`, after MIDI learn's (see the module docs): jog turns, then the lights.
pub fn frame(app: &mut SoundApp, now: f64) {
    let dt = app.controllers.last_frame.map_or(1.0 / 60.0, |t| now - t);
    app.controllers.last_frame = Some(now);
    if app.controllers.devices.is_empty() {
        return;
    }
    upkeep_outputs(app, now);
    let ctx = Ctx::of(app, now);
    for i in 0..app.controllers.devices.len() {
        let actions = app.controllers.devices.get_mut(i).map(|d| d.logic.tick(&ctx, dt)).unwrap_or_default();
        let name = app.controllers.devices.get(i).map_or("", |d| d.layout.name);
        for a in actions {
            perform(app, name, a);
        }
    }
    // Lights lost to a full queue: send them all again.
    let dropped = app.midi_host.as_ref().and_then(MidiHost::outputs).map_or(0, |o| o.dropped());
    let lost = dropped > app.controllers.dropped;
    app.controllers.dropped = dropped;
    // The commands just run change the state only once the audio engine reports back.
    let Some(o) = app.midi_host.as_mut().and_then(MidiHost::outputs_mut) else { return };
    for d in &mut app.controllers.devices {
        let Some(out) = d.out else { continue };
        if d.logic.take_resend() || lost {
            d.mirror.reset();
        }
        let lights = d.logic.lights(&ctx);
        for m in d.logic.events(&ctx) {
            o.send(out, m);
        }
        d.mirror.diff(&lights, now, |m| o.send(out, m));
    }
}

/// Once a second while a device uses a layout: notices outputs that went away and opens the
/// paired output of a device without one (its lights are then all sent).
fn upkeep_outputs(app: &mut SoundApp, now: f64) {
    let due = app.controllers.looked.is_none_or(|at| now - at >= REFRESH_S || now < at);
    if !due {
        return;
    }
    app.controllers.looked = Some(now);
    let Some(h) = app.midi_host.as_mut() else { return };
    let changes = h.outputs_mut().map(|o| o.refresh()).unwrap_or_default();
    for c in changes {
        if let PortChange::Closed { port, name } = c {
            for d in app.controllers.devices.iter_mut().filter(|d| d.out == Some(port)) {
                log::info!("{}: output {name} went away", d.raw_name);
                d.out = None;
            }
        }
    }
    let closed: Vec<usize> = app
        .controllers
        .devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.out.is_none_or(|o| !h.outputs().is_some_and(|x| x.is_open_id(o))))
        .map(|(i, _)| i)
        .collect();
    for i in closed {
        let Some(d) = app.controllers.devices.get_mut(i) else { continue };
        d.out = None;
        if now < d.retry_at {
            continue;
        }
        // Tried again after 1 s, then less and less often, up to every 30 s.
        d.retry_at = now + d.backoff;
        d.backoff = (d.backoff * 2.0).min(MAX_BACKOFF_S);
        let Some(port) = h.paired_output(d.input) else {
            d.out_error = Some(if h.outputs().is_some() { "no output of that name".into() } else { "no MIDI output on this system".into() });
            continue;
        };
        let Some(o) = h.outputs_mut() else { continue };
        match o.open(&port.name) {
            Ok(id) => {
                d.out = Some(id);
                d.out_name = Some(port.name.clone());
                d.out_error = None;
                d.backoff = REFRESH_S;
                // A new connection: every light again.
                d.mirror.reset();
            }
            Err(e) => d.out_error = Some(e.to_string()),
        }
    }
}

/// Commands that move continuously: a refusal (no track) isn't worth the status line.
fn quiet(id: &str) -> bool {
    matches!(id, "deck.pitch" | "deck.jog" | "deck.fader" | "deck.xfader" | "deck.eq" | "deck.trim" | "deck.filter" | "deck.master_volume")
}

fn perform(app: &mut SoundApp, layout: &str, a: Action) {
    match a {
        Action::Run(id, p) => {
            if let Err(e) = app.run(id, p)
                && !quiet(id)
            {
                app.ui.status = format!("{layout}: {e}");
            }
        }
        Action::Browse(n) => app.decks_view.browser.step_selection(n),
        Action::Load(deck) => match app.decks_view.browser.selected_path() {
            Some(p) => crate::decks_view::load(app, &p, deck, false),
            None => app.ui.status = format!("{layout}: pick a track in the library first"),
        },
    }
}

/// One event from input `port`, not taken by a MIDI learn mapping: the device's layout acts on
/// it. True when the layout claims it (whatever it did).
pub fn handle(app: &mut SoundApp, port: PortId, msg: &MidiMessage, now: f64) -> bool {
    let Some(i) = app.controllers.devices.iter().position(|d| d.input == port) else { return false };
    let ctx = Ctx::of(app, now);
    let Some(d) = app.controllers.devices.get_mut(i) else { return false };
    if !d.logic.claims(msg) {
        return false;
    }
    let actions = d.logic.input(msg, &ctx);
    let name = d.layout.name;
    for a in actions {
        perform(app, name, a);
    }
    true
}

/// The (port, control) pairs the layouts claim, for the player's mapped-control filter.
pub fn controls(app: &SoundApp) -> Vec<(PortId, MidiControl)> {
    app.controllers.devices.iter().flat_map(|d| d.logic.controls().into_iter().map(move |c| (d.input, c))).collect()
}

/// Lets go of everything the layouts hold, and turns their lights off and closes their outputs:
/// the app is quitting.
pub fn shutdown(app: &mut SoundApp) {
    let now = app.midi_learn.now;
    for d in std::mem::take(&mut app.controllers.devices) {
        stop(app, d, now);
    }
}

/// Lets go of everything the layouts hold (a CUE or hot cue preview): MIDI Learn starts, so the
/// release may never reach the layout.
pub fn release_holds(app: &mut SoundApp) {
    let ctx = Ctx::of(app, app.midi_learn.now);
    let mut actions = Vec::new();
    for d in &mut app.controllers.devices {
        let name = d.layout.name;
        actions.extend(d.logic.release_all(&ctx).into_iter().map(|a| (name, a)));
    }
    for (name, a) in actions {
        perform(app, name, a);
    }
}

/// The other half of the 14-bit control `msg` belongs to, on a device using a layout.
pub fn twin(app: &SoundApp, port: PortId, msg: &MidiMessage) -> Option<MidiControl> {
    app.controllers.devices.iter().find(|d| d.input == port)?.logic.twin(msg)
}

/// The layout of the input (raw name, backend id) for MIDI Devices and `midi.inputs`: its
/// name, whether it is on, and its output's state; `None` for a device no layout knows.
pub fn describe(app: &SoundApp, raw_name: &str, backend_id: &str) -> Option<Value> {
    let layout = layout_for(raw_name)?;
    let on = !layout_off(&app.ui.midi_layouts_off, raw_name, backend_id);
    let dev = app.controllers.devices.iter().find(|d| d.raw_name == raw_name && d.backend_id == backend_id);
    let output = match dev {
        None => "inactive",
        Some(d) if d.out.is_some() => "open",
        Some(_) => "not open",
    };
    Some(json!({
        "name": layout.name,
        "on": on,
        "output": output,
        "output_name": dev.and_then(|d| d.out_name.clone()),
        "output_error": dev.and_then(|d| d.out_error.clone()),
    }))
}

/// `midi.layout {name, on?}`: turns the built-in layout of the input listed as `name` on or off
/// (default: toggles), kept in the preferences.
pub fn command(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let name = p.get("name").and_then(Value::as_str).ok_or("`name` required")?;
    let host = app.midi_host.as_ref().ok_or("no MIDI input on this system")?;
    let port = host.source().list().into_iter().find(|x| x.name == name).ok_or_else(|| format!("no MIDI input named \"{name}\""))?;
    let layout = layout_for(&port.raw_name).ok_or_else(|| format!("{name} has no built-in layout"))?;
    let was = !layout_off(&app.ui.midi_layouts_off, &port.raw_name, &port.backend_id);
    let on = match p.get("on") {
        None | Some(Value::Null) => !was,
        Some(v) => v.as_bool().ok_or("`on` must be true or false")?,
    };
    app.ui.midi_layouts_off.retain(|x| !(x.raw_name == port.raw_name && x.backend_id == port.backend_id));
    if !on {
        app.ui.midi_layouts_off.push(MidiInputPref { name: port.name.clone(), raw_name: port.raw_name.clone(), backend_id: port.backend_id.clone() });
    }
    app.mark_prefs_changed();
    app.ui.status = format!("{name}: built-in {} layout {}", layout.name, if on { "on" } else { "off" });
    Ok(json!({"name": name, "layout": layout.name, "on": on}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn led(note: u8, on: bool) -> Light {
        Light { msg: MidiMessage::NoteOn { ch: 0, note, vel: if on { 0x7F } else { 0 } }, throttled: false }
    }

    fn meter(v: u8) -> Light {
        Light { msg: MidiMessage::ControlChange { ch: 0, cc: 2, value: v }, throttled: true }
    }

    #[test]
    fn the_mirror_sends_changes_only_meters_at_most_30_times_a_second_and_all_after_a_reset() {
        let mut m = Mirror::default();
        let ok = |_| true;
        let first = m.diff(&[led(1, true), led(2, false), meter(40)], 0.0, ok);
        assert_eq!(first.len(), 3, "everything at first");
        assert!(m.diff(&[led(1, true), led(2, false), meter(40)], 0.01, ok).is_empty());
        let next = m.diff(&[led(1, false), led(2, false), meter(50)], 0.02, ok);
        assert_eq!(next, vec![led(1, false).msg], "the meter waits");
        assert_eq!(m.diff(&[led(1, false), led(2, false), meter(50)], 0.04, ok), vec![meter(50).msg]);
        m.reset();
        assert_eq!(m.diff(&[led(1, false), led(2, false), meter(50)], 0.05, ok).len(), 3);
        // A light the output refused isn't counted as sent: it goes again next time.
        assert!(m.diff(&[led(1, true), led(2, false), meter(50)], 0.06, |_| false).is_empty());
        assert_eq!(m.diff(&[led(1, true), led(2, false), meter(50)], 0.07, ok), vec![led(1, true).msg]);
    }

    #[test]
    fn only_a_ddj_flx4_gets_the_layout() {
        assert!(layout_for("DDJ-FLX4").is_some());
        assert!(layout_for("DDJ-FLX4 MIDI 1").is_some());
        assert!(layout_for("ddj-flx4:ddj-flx4 midi 1 20:0").is_some());
        assert!(layout_for("DDJ-400").is_none());
        assert!(layout_for("Launchpad").is_none());
    }
}
