//! Window › Decks (Cmd+6): the live decks, as in a Rekordbox-style Performance view.
//!
//! - **Top:** each deck's big scrolling waveform ([`crate::deck_wave`]), Deck A above Deck B,
//!   centred on a fixed playhead, at one time scale for both (zoom +/−), so synced beats line up.
//! - **Middle:** the deck panels (track, BPM, key, times, overview, transport, sync, pitch fader,
//!   hot cue pads, loops, beat jump, jog and Load) either side of the mixer (trim, EQ with kills,
//!   filter, channel faders, crossfader, master, meters).
//! - **Bottom:** the library browser ([`crate::library_browser`]).
//!
//! Everything acts through the `deck.*` commands, so a click does what an agent or a MIDI
//! mapping does. The playhead between the states the audio engine publishes (once a block) is
//! extrapolated from the deck's rate and the wall clock, and eased back to each published state,
//! so the waveforms scroll smoothly at the display's rate rather than in block steps.
//!
//! Keys while the tab is shown (not while typing in the library's search): Space plays or pauses
//! the focused deck (never the arrangement), Left/Right focus Deck A/B, Q and W are Deck A's and
//! B's CUE (held), 1–8 the focused deck's hot cues (held; Shift clears), S its sync, L a 4-beat
//! loop (again: exit), comma and full stop a beat jump back and on. Up/Down pick a library row,
//! Enter loads it on an empty deck, Shift+Left/Right on Deck A/B. Other plain keys do nothing
//! here, and of the Cmd shortcuts only windows, Search, Save and Quit work
//! ([`crate::shortcuts`]): none can start the arrangement, which would stop the decks.
//!
//! A held CUE or pad (pointer or key) is let go by [`release_holds`] every frame, whatever the
//! window: when the button or key comes up, the tab is left or the app loses focus.

use crate::deck_wave::{self, ColumnCache, OverviewCache, RateHold, Scrolling, hot_cue_colour, hot_cue_letter, steady_tempo};
use crate::knob::{self, Knob};
use crate::library_browser::{self, Browser, Dragged};
use crate::theme::{Tokens, bold, mono, regular};
use crate::widgets::{elided_text, key as key_face, key_fill, meter, shade};
use crate::{MainWindow, SoundApp};
use egui::{Align2, Color32, Id, Key, Modifiers, Rect, Response, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_mix::{DeckId, DeckState, DeckTrack, EqBand, FaderCurve, HOT_CUES, PITCH_RANGES, XfadeCurve, XfadeSide};
use std::collections::HashMap;
use std::sync::Arc;

/// Seconds of playback across a scrolling waveform, by zoom step.
const ZOOMS: [f64; 7] = [2.0, 4.0, 6.0, 8.0, 12.0, 16.0, 32.0];
const DEFAULT_ZOOM: usize = 3;
/// The height of a deck's loop and jump rows (the pitch range key lines up with the last).
const KEY_ROW: f32 = 30.0;
/// Beat jump sizes.
const JUMPS: [f64; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
const DEFAULT_JUMP: usize = 2;
/// Beat loop lengths with buttons of their own (1/4 and 1/2 are in a menu).
const LOOPS: [f64; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
/// A jog disc drag: this many points are a beat of search, and this many points a second a full
/// nudge.
const JOG_PX: f32 = 40.0;
const NUDGE_PX_S: f32 = 1200.0;
/// The beat loop the L key sets.
const KEY_LOOP: f64 = 4.0;
/// A playhead this far (seconds) from where the extrapolation has it jumps there (a seek, a
/// loop, a hot cue) instead of easing.
const SNAP_S: f64 = 0.06;
/// The share of the gap to the published playhead closed each frame.
const EASE: f64 = 0.18;

/// A deck's playhead as shown: extrapolated between the published states.
#[derive(Debug, Default, Clone, Copy)]
struct Clock {
    load: u64,
    /// The last published position and when (UI time) it was first seen.
    published: f64,
    seen: f64,
    /// The position shown and when.
    shown: f64,
    at: f64,
}

impl Clock {
    /// The playhead to show at `now` for the deck's state `s` (`sr` the sample rate).
    fn advance(&mut self, s: &DeckState, now: f64, sr: f64) -> f64 {
        if s.load != self.load || !s.position.is_finite() {
            *self = Clock { load: s.load, published: s.position, seen: now, shown: s.position, at: now };
            return self.shown;
        }
        if s.position != self.published {
            self.published = s.position;
            self.seen = now;
        }
        let rate = if s.rate.is_finite() { s.rate.clamp(0.0, 4.0) } else { 1.0 };
        // In a loop the playhead goes round it, not on past its end until the next state.
        let wrap = |x: f64| match (s.loop_on, s.loop_in, s.loop_out) {
            (true, Some(a), Some(b)) if b > a && x >= b && (a..b).contains(&s.position) => a + (x - b) % (b - a),
            _ => x,
        };
        if !(s.playing || s.previewing) {
            self.shown = s.position;
        } else {
            let target = wrap(self.published + (now - self.seen).clamp(0.0, 0.1) * rate * sr);
            let predicted = wrap(self.shown + (now - self.at).clamp(0.0, 0.1) * rate * sr);
            let gap = target - predicted;
            self.shown = if gap.abs() > SNAP_S * sr { target } else { predicted + gap * EASE };
        }
        self.at = now;
        self.shown
    }
}

/// What a held CUE or pad does on release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Cue,
    /// A hot cue pad: its slot, and whether its release is sent (a jump; a set or a clear has none).
    Pad {
        slot: usize,
        release: bool,
    },
}

/// What holds it down: the pointer, or a key (its logical key, as `key_down` knows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum By {
    Pointer,
    Key(Key),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hold {
    what: Held,
    by: By,
}

/// What the Decks tab keeps between frames.
#[derive(Debug, Default)]
pub struct DecksView {
    /// The deck Space plays (0 A, 1 B): the last one used.
    pub focus: usize,
    zoom: Option<usize>,
    clocks: [Clock; 2],
    waves: [ColumnCache; 2],
    overviews: [OverviewCache; 2],
    /// Each deck's held CUE or pad (one at a time), let go by [`release_holds`].
    holds: [Option<Hold>; 2],
    /// A pointer hold let go this frame: its click is not also a tap.
    released: [bool; 2],
    /// The waveforms' scale: each deck's held rate, and the steady tempo drawn at.
    rates: [RateHold; 2],
    tempo: [f64; 2],
    /// A paused waveform drag: where the playhead was when it began, and the points dragged.
    search: [Option<(f64, f32)>; 2],
    jump: [Option<usize>; 2],
    /// Each deck's last `deck.load` report, and its last refusal or failure.
    pub info: [Option<Value>; 2],
    pub load_error: [Option<String>; 2],
    pub browser: Browser,
    /// Meter levels shown: A, B, master, headphones (L, R), falling smoothly.
    meters: [[f32; 2]; 4],
    /// The headphone route `Engine::headphone_output` was last set for (`None`: no player yet).
    pub(crate) phones_route: Option<soundcraft_playback::HeadphoneRoute>,
    /// Values of the controls being dragged, until the decks report them.
    held: HashMap<Id, f32>,
    /// Columns the waveforms computed (for the timing harness).
    pub last_columns: usize,
}

impl DecksView {
    /// Seconds of playback across a scrolling waveform.
    pub fn seconds(&self) -> f64 {
        ZOOMS.get(self.zoom.unwrap_or(DEFAULT_ZOOM)).copied().unwrap_or(8.0)
    }

    /// Columns both scrolling waveforms have computed so far.
    pub fn columns_computed(&self) -> usize {
        self.waves.iter().map(|w| w.computed).sum()
    }
}

/// Whether a deck plays, so the tab needs frames at the display's rate.
pub fn moving(app: &SoundApp) -> bool {
    app.engine.deck_status.decks.iter().any(|d| d.playing || d.previewing) || app.engine.deck_loading()
}

fn deck_of(i: usize) -> DeckId {
    if i == 0 { DeckId::A } else { DeckId::B }
}

/// A deck's colour.
fn tint(d: DeckId) -> Color32 {
    if d == DeckId::A { crate::theme::studio::DECK_A } else { crate::theme::studio::DECK_B }
}

fn name(d: DeckId) -> String {
    d.id().to_uppercase()
}

fn run(app: &mut SoundApp, id: &str, p: Value) {
    let _ = app.run(id, p);
}

/// The deck's state and track.
fn deck(app: &SoundApp, d: DeckId) -> (DeckState, Option<Arc<DeckTrack>>) {
    (app.engine.deck_status.decks.get(d.index()).copied().unwrap_or_default(), app.engine.deck_tracks.get(d.index()).cloned().flatten())
}

/// Loads `path` on `deck`, or on an empty deck (refused when neither is, so a track cued up on
/// the other deck is never replaced unasked); `force` loads onto a playing deck. A refusal shows
/// on the deck and in the status line.
pub fn load(app: &mut SoundApp, path: &str, deck: Option<DeckId>, force: bool) {
    let deck = deck.or_else(|| {
        DeckId::ALL.into_iter().find(|d| app.engine.deck_tracks.get(d.index()).is_none_or(Option::is_none) && !app.engine.deck_loading_on(*d))
    });
    let Some(deck) = deck else {
        app.ui.status = "Both decks have a track: load with Shift+Left or Shift+Right, a deck's LOAD, or by dragging onto a deck".into();
        return;
    };
    let mut p = json!({"path": path, "deck": deck.id()});
    if force {
        p["force"] = json!(true);
    }
    match app.run("deck.load", p) {
        Ok(v) => {
            if let Some(d) = v.get("deck").and_then(Value::as_str).and_then(DeckId::parse) {
                if let Some(e) = app.decks_view.load_error.get_mut(d.index()) {
                    *e = None;
                }
                app.decks_view.focus = d.index();
                let file = std::path::Path::new(path).file_name().map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned());
                app.ui.status = format!("Deck {}: analysing {file}…", name(d));
            }
        }
        Err(e) => {
            app.ui.status = format!("Deck {}: {e}", name(deck));
            if let Some(slot) = app.decks_view.load_error.get_mut(deck.index()) {
                *slot = Some(e);
            }
        }
    }
}

/// The status line for a finished `deck.load`: what loaded, where its grid and key came from,
/// and any analysis that fell back.
pub fn load_status(v: &Value) -> String {
    let deck = v.get("deck").and_then(Value::as_str).unwrap_or("?").to_uppercase();
    let title = v.get("title").and_then(Value::as_str).unwrap_or("");
    let mut s = format!("Deck {deck}: loaded {title}");
    match (v.get("bpm").and_then(Value::as_f64), v.get("grid_from").and_then(Value::as_str)) {
        (Some(b), Some(from)) => s.push_str(&format!(" — {b:.1} BPM ({})", grid_source(from))),
        _ => s.push_str(" — no beat grid: sync, beat loops and quantise are off for it"),
    }
    if let (Some(k), src) = (v.get("key").and_then(Value::as_str), v.get("key_source").and_then(Value::as_str)) {
        s.push_str(&format!(", key {k}"));
        if let Some(src) = src {
            s.push_str(&format!(" ({})", library_browser::source_short(src)));
        }
    }
    if let Some(err) = v.get("analysis_error").and_then(Value::as_str).filter(|_| v.get("grid_from").and_then(Value::as_str) != Some("given")) {
        s.push_str(&format!("; mixforge's analysis: {err}"));
    }
    s
}

/// The Decks tab's keys (`k` the logical key, `physical` the key pressed). True when the key is
/// the tab's or does nothing here; false for a Cmd, Ctrl or Alt chord, which
/// [`crate::shortcuts`] lets through only when it is safe while the decks play.
pub fn key(app: &mut SoundApp, k: Key, physical: Key, m: Modifiers) -> bool {
    if m.command || m.ctrl || m.alt {
        return false;
    }
    let focus = deck_of(app.decks_view.focus);
    let (s, track) = deck(app, focus);
    let loaded = track.is_some();
    match k {
        Key::Space => {
            if loaded {
                run(app, if s.playing { "deck.pause" } else { "deck.play" }, json!({"deck": focus.id()}));
            }
        }
        Key::ArrowLeft | Key::ArrowRight if m.shift => {
            let d = if k == Key::ArrowLeft { DeckId::A } else { DeckId::B };
            if let Some(p) = app.decks_view.browser.selected_path() {
                load(app, &p, Some(d), false);
            }
        }
        Key::ArrowLeft => app.decks_view.focus = 0,
        Key::ArrowRight => app.decks_view.focus = 1,
        Key::ArrowUp | Key::ArrowDown => app.decks_view.browser.step_selection(if k == Key::ArrowUp { -1 } else { 1 }),
        Key::Enter => {
            if let Some(p) = app.decks_view.browser.selected_path() {
                load(app, &p, None, false);
            }
        }
        Key::Q | Key::W => {
            let d = if k == Key::Q { DeckId::A } else { DeckId::B };
            if deck(app, d).1.is_some() {
                app.decks_view.focus = d.index();
                press(app, d, Held::Cue, By::Key(k), false);
            }
        }
        Key::S if loaded => run(app, "deck.sync", json!({"deck": focus.id(), "on": !s.synced})),
        Key::L if loaded => {
            let p = if s.loop_on { json!({"deck": focus.id(), "action": "exit"}) } else { json!({"deck": focus.id(), "beats": KEY_LOOP}) };
            run(app, "deck.loop", p);
        }
        Key::Comma | Key::Period if loaded => {
            let jump = jump_beats(app, focus);
            run(app, "deck.beatjump", json!({"deck": focus.id(), "beats": if k == Key::Comma { -jump } else { jump }}));
        }
        _ => {
            let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8];
            if let Some(slot) = digits.iter().position(|d| *d == physical)
                && loaded
            {
                let set = s.hot.get(slot).copied().flatten().is_some();
                press(app, focus, Held::Pad { slot, release: set }, By::Key(k), m.shift);
            }
        }
    }
    true
}

/// The beats a beat jump on deck `d` moves.
fn jump_beats(app: &SoundApp, d: DeckId) -> f64 {
    JUMPS.get(app.decks_view.jump.get(d.index()).copied().flatten().unwrap_or(DEFAULT_JUMP)).copied().unwrap_or(4.0)
}

/// Presses deck `d`'s CUE or a pad, held by `by` until [`release_holds`] lets it go; nothing
/// while the deck holds something else. A pad (`release`: it is set): `clear` clears it, else it
/// jumps to a set cue or sets an empty one.
fn press(app: &mut SoundApp, d: DeckId, what: Held, by: By, clear: bool) {
    let i = d.index();
    if app.decks_view.holds.get(i).copied().flatten().is_some() {
        return;
    }
    app.decks_view.focus = i;
    let what = match what {
        Held::Cue => {
            run(app, "deck.cue", json!({"deck": d.id(), "down": true}));
            Held::Cue
        }
        Held::Pad { slot, release } => {
            let action = if clear {
                "clear"
            } else if release {
                "jump"
            } else {
                "set"
            };
            run(app, "deck.hotcue", json!({"deck": d.id(), "n": slot + 1, "action": action}));
            Held::Pad { slot, release: release && !clear }
        }
    };
    if let Some(h) = app.decks_view.holds.get_mut(i) {
        *h = Some(Hold { what, by });
    }
}

/// Sends the release of deck `d`'s hold `h`.
fn release(app: &mut SoundApp, d: DeckId, h: Hold) {
    match h.what {
        Held::Cue => run(app, "deck.cue", json!({"deck": d.id(), "down": false})),
        Held::Pad { slot, release: true } => run(app, "deck.hotcue", json!({"deck": d.id(), "n": slot + 1, "action": "release"})),
        Held::Pad { .. } => {}
    }
}

/// Once a frame, whatever the window: lets go of a held CUE or pad once its pointer button or
/// key is up, the Decks tab is not shown or the app has lost focus, so a preview never keeps
/// playing on its own.
pub fn release_holds(app: &mut SoundApp, ctx: &egui::Context) {
    let shown = app.ui.window == MainWindow::Decks;
    for d in DeckId::ALL {
        let i = d.index();
        let Some(h) = app.decks_view.holds.get(i).copied().flatten() else { continue };
        let down = ctx.input(|inp| {
            inp.focused
                && match h.by {
                    By::Pointer => inp.pointer.primary_down(),
                    By::Key(k) => inp.key_down(k),
                }
        });
        if down && shown {
            continue;
        }
        if let Some(slot) = app.decks_view.holds.get_mut(i) {
            *slot = None;
        }
        if h.by == By::Pointer
            && let Some(r) = app.decks_view.released.get_mut(i)
        {
            *r = true;
        }
        release(app, d, h);
    }
}

/// A pointer press on deck `d`'s CUE or pad `resp` this frame: held until the button comes up.
/// A press and release inside one frame (a slow frame, a tap) is a tap: pressed and let go.
fn pointer_hold(app: &mut SoundApp, ui: &Ui, resp: &Response, d: DeckId, what: Held, clear: bool) {
    let i = d.index();
    let free = app.decks_view.holds.get(i).copied().flatten().is_none();
    let released = app.decks_view.released.get(i).copied().unwrap_or(false);
    if !free {
        return;
    }
    if resp.is_pointer_button_down_on() && ui.input(|inp| inp.pointer.primary_down()) {
        press(app, d, what, By::Pointer, clear);
    } else if resp.clicked() && !released {
        press(app, d, what, By::Pointer, clear);
        if let Some(h) = app.decks_view.holds.get_mut(i).and_then(Option::take) {
            release(app, d, h);
        }
    }
}

/// Draws the Decks tab.
pub fn show(app: &mut SoundApp, ui: &mut Ui) {
    let t = Tokens::current();
    egui::Panel::bottom("decks_status")
        .exact_size(22.0)
        .frame(egui::Frame::NONE.fill(t.toolbar_bg))
        .show(ui, |ui| crate::edit_window::status_bar(app, ui));
    let full = ui.available_rect_before_wrap();
    ui.painter().rect_filled(full, 0.0, t.window_bg);
    let now = ui.input(|i| i.time);
    let sr = app.engine.session().sample_rate.as_f64().max(1.0);
    let mut shown = [0.0; 2];
    for (i, d) in DeckId::ALL.into_iter().enumerate() {
        let (s, _) = deck(app, d);
        if let (Some(c), Some(x)) = (app.decks_view.clocks.get_mut(i), shown.get_mut(i)) {
            *x = c.advance(&s, now, sr);
        }
    }
    // The waveforms' scale: each deck's steady tempo (not a nudge's bend or sync's correction).
    let states = [deck(app, DeckId::A), deck(app, DeckId::B)];
    let free = [0, 1].map(|i| app.decks_view.rates.get_mut(i).map_or(1.0, |r| r.update(&states[i].0, now)));
    let bpm = |i: usize| states[i].1.as_ref().and_then(|t| t.bpm);
    app.decks_view.tempo = [
        steady_tempo(&states[0].0, free[0], bpm(0), &states[1].0, free[1], bpm(1)),
        steady_tempo(&states[1].0, free[1], bpm(1), &states[0].0, free[0], bpm(0)),
    ];
    feed_meters(app, ui.input(|i| i.stable_dt).clamp(0.0, 0.25));
    let wave_h = (full.height() * 0.25).clamp(150.0, 260.0);
    let mid_h = (full.height() * 0.41).clamp(360.0, 430.0);
    let waves = Rect::from_min_size(full.min, vec2(full.width(), wave_h));
    let mid = Rect::from_min_size(pos2(full.min.x, waves.max.y + 4.0), vec2(full.width(), mid_h));
    let lib = Rect::from_min_max(pos2(full.min.x, mid.max.y + 4.0), full.max);
    let before = app.decks_view.columns_computed();
    let gap = 12.0;
    let h = (waves.height() - 4.0 - gap) / 2.0;
    for (i, d) in DeckId::ALL.into_iter().enumerate() {
        let r = Rect::from_min_size(pos2(waves.min.x + 4.0, waves.min.y + 2.0 + i as f32 * (h + gap)), vec2(waves.width() - 8.0, h));
        scrolling_wave(app, ui, r, d, shown.get(i).copied().unwrap_or(0.0));
    }
    let meter_r = Rect::from_min_size(pos2(waves.min.x + 38.0, waves.min.y + 2.0 + h + 2.0), vec2(waves.width() - 42.0, gap - 4.0));
    phase_meter(app, ui, meter_r);
    app.decks_view.last_columns = app.decks_view.columns_computed().saturating_sub(before);
    let mixer_w = 380.0_f32.min(mid.width() * 0.32);
    let side = (mid.width() - mixer_w - 16.0) / 2.0;
    let a = Rect::from_min_size(pos2(mid.min.x + 4.0, mid.min.y), vec2(side, mid.height()));
    let m = Rect::from_min_size(pos2(a.max.x + 4.0, mid.min.y), vec2(mixer_w, mid.height()));
    let b = Rect::from_min_size(pos2(m.max.x + 4.0, mid.min.y), vec2(side, mid.height()));
    deck_panel(app, ui, a, DeckId::A, shown[0]);
    mixer(app, ui, m);
    deck_panel(app, ui, b, DeckId::B, shown[1]);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(lib.shrink2(vec2(4.0, 0.0))));
    child.set_clip_rect(lib);
    library_browser::show(app, &mut child);
    app.decks_view.released = [false; 2];
}

/// Between the waveforms: how far the following deck's beat is from its master's (±½ beat),
/// while both play. Centred and green is in phase.
fn phase_meter(app: &SoundApp, ui: &Ui, r: Rect) {
    let t = Tokens::current();
    let p = ui.painter();
    p.rect_filled(r, 2.0, t.window_bg);
    p.vline(r.center().x, r.y_range(), Stroke::new(1.0, t.text_dim));
    for q in [0.25_f32, 0.75] {
        p.vline(r.min.x + r.width() * q, r.y_range(), Stroke::new(1.0, t.text_dim.gamma_multiply(0.4)));
    }
    let st = &app.engine.deck_status;
    let Some((d, e)) =
        DeckId::ALL.into_iter().find_map(|d| st.decks.get(d.index()).and_then(|s| s.phase_error).filter(|e| e.is_finite()).map(|e| (d, e)))
    else {
        return;
    };
    let x = r.center().x + (e.clamp(-0.5, 0.5) as f32) * r.width();
    let c = if e.abs() < 0.02 { t.meter_green } else { tint(d) };
    p.rect_filled(Rect::from_center_size(pos2(x, r.center().y), vec2(10.0, r.height())), 2.0, c);
    let ms = st.decks.get(d.index()).map(|s| s.bpm).filter(|b| *b > 0.0).map(|b| e * 60_000.0 / b);
    if let Some(ms) = ms {
        p.text(pos2(r.max.x - 4.0, r.center().y), Align2::RIGHT_CENTER, format!("{} {ms:+.0} ms", name(d)), regular(9.0), t.text_dim);
    }
}

fn feed_meters(app: &mut SoundApp, dt: f32) {
    let st = app.engine.deck_status;
    let fall = 10f32.powf(-26.0 * dt / 20.0);
    let levels = [st.decks[0].level, st.decks[1].level, st.master_level, st.cue_level];
    for (shown, new) in app.decks_view.meters.iter_mut().zip(levels) {
        for (s, n) in shown.iter_mut().zip(new) {
            let n = if n.is_finite() { n.max(0.0) } else { 0.0 };
            *s = if n >= *s { n } else { (*s * fall).max(n) };
            if *s < 1e-5 {
                *s = 0.0;
            }
        }
    }
}

/// A small rounded label ending at `right` (its top at `right.y`): filled in `c` when `filled`,
/// else outlined. Returns its rect.
fn chip(ui: &Ui, right: egui::Pos2, text: &str, c: Color32, filled: bool) -> Rect {
    let t = Tokens::current();
    let g = ui.painter().layout_no_wrap(text.to_string(), if filled { bold(11.0) } else { mono(11.0) }, if filled { t.text_dark } else { c });
    let r = Rect::from_min_max(pos2(right.x - g.size().x - 14.0, right.y), pos2(right.x, right.y + g.size().y + 4.0));
    if filled {
        ui.painter().rect_filled(r, r.height() / 2.0, c);
    } else {
        ui.painter().rect_stroke(r, r.height() / 2.0, Stroke::new(1.0, c.gamma_multiply(0.5)), egui::StrokeKind::Inside);
    }
    ui.painter().galley(pos2(r.min.x + 7.0, r.min.y + 2.0), g, c);
    r
}

/// A key with text: lit in `on_colour` while `on`. Its accessible name is `label`.
fn button(ui: &mut Ui, r: Rect, text: &str, on: bool, on_colour: Color32, label: &str, size: f32) -> Response {
    let t = Tokens::current();
    let resp = ui.interact(r, Id::new(("decks_btn", label)), Sense::click());
    key_face(ui.painter(), r, &resp, if on { on_colour } else { key_fill(&resp) });
    ui.painter().text(r.center(), Align2::CENTER_CENTER, text, bold(size), if on { t.text_dark } else { t.text });
    let label = label.to_string();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, &label));
    resp
}

/// A drop of a library track inside `r` this frame.
fn dropped(ui: &Ui, r: Rect) -> Option<String> {
    let ctx = ui.ctx();
    if !egui::DragAndDrop::has_payload_of_type::<Dragged>(ctx) {
        return None;
    }
    let (released, at) = ctx.input(|i| (i.pointer.any_released(), i.pointer.interact_pos()));
    if released && at.is_some_and(|p| r.contains(p)) {
        return egui::DragAndDrop::take_payload::<Dragged>(ctx).map(|d| d.0.clone());
    }
    if at.is_some_and(|p| r.contains(p)) {
        ui.painter().rect_stroke(r, 3.0, Stroke::new(2.0, Tokens::current().accent), egui::StrokeKind::Inside);
    }
    None
}

/// The jog disc dragged: a nudge as strong as the drag is fast while the deck is heard, else a
/// search of a beat per [`JOG_PX`] points (none lost to a fast flick).
fn jog_drag(app: &mut SoundApp, ui: &Ui, resp: &Response, d: DeckId, s: &DeckState) {
    if !resp.dragged() {
        return;
    }
    let dx = resp.drag_delta().x;
    if dx.abs() < 0.5 {
        return;
    }
    app.decks_view.focus = d.index();
    if s.playing || s.previewing {
        let dt = ui.input(|i| i.stable_dt).clamp(1.0 / 240.0, 0.1);
        let delta = f64::from((dx / dt / NUDGE_PX_S).clamp(-1.0, 1.0));
        run(app, "deck.jog", json!({"deck": d.id(), "delta": delta, "mode": "nudge"}));
        return;
    }
    let mut beats = f64::from(dx / JOG_PX);
    for _ in 0..64 {
        if beats.abs() < 1e-4 {
            break;
        }
        let step = beats.clamp(-1.0, 1.0);
        run(app, "deck.jog", json!({"deck": d.id(), "delta": step, "mode": "search"}));
        beats -= step;
    }
}

fn scrolling_wave(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, position: f64) {
    let t = Tokens::current();
    let tint = tint(d);
    let strip = Rect::from_min_size(r.min, vec2(30.0, r.height()));
    let wave = Rect::from_min_max(pos2(strip.max.x + 4.0, r.min.y), r.max);
    ui.painter().rect_filled(strip, 3.0, t.panel_bg);
    ui.painter().text(pos2(strip.center().x, strip.min.y + 14.0), Align2::CENTER_CENTER, name(d), bold(15.0), tint);
    let bh = 20.0;
    let zin = Rect::from_center_size(pos2(strip.center().x, strip.max.y - bh * 1.6 - 4.0), vec2(24.0, bh));
    let zout = Rect::from_center_size(pos2(strip.center().x, strip.max.y - bh * 0.5 - 2.0), vec2(24.0, bh));
    let zoom = app.decks_view.zoom.unwrap_or(DEFAULT_ZOOM);
    if button(ui, zin, "+", false, t.accent, &format!("Deck {} Zoom In", name(d)), 13.0).on_hover_text("Zoom in (both decks)").clicked() {
        app.decks_view.zoom = Some(zoom.saturating_sub(1));
    }
    if button(ui, zout, "−", false, t.accent, &format!("Deck {} Zoom Out", name(d)), 13.0).on_hover_text("Zoom out (both decks)").clicked() {
        app.decks_view.zoom = Some((zoom + 1).min(ZOOMS.len() - 1));
    }
    let (s, track) = deck(app, d);
    let resp = ui.interact(wave, Id::new(("decks_wave", d.id())), Sense::drag());
    let Some(track) = track else {
        ui.painter().rect_filled(wave, 2.0, t.panel_bg);
        let msg = if app.engine.deck_loading_on(d) { "Analysing…" } else { "Drop a track here, or double-click one in the library" };
        ui.painter().text(wave.center(), Align2::CENTER_CENTER, msg, regular(13.0), t.text_dim);
        if let Some(p) = dropped(ui, wave) {
            load(app, &p, Some(d), false);
        }
        return;
    };
    let by_freq = app.waveform_bands();
    let seconds = app.decks_view.seconds();
    let sr = app.engine.session().sample_rate.as_f64().max(1.0);
    let tempo = app.decks_view.tempo.get(d.index()).copied().filter(|t| *t > 0.0).unwrap_or(1.0);
    let w = Scrolling { track: &track, state: &s, position, seconds, rate: sr, tempo, by_freq, tint };
    if let Some(cache) = app.decks_view.waves.get_mut(d.index()) {
        deck_wave::scrolling(ui.painter(), wave, cache, &w);
    }
    if app.engine.deck_loading_on(d) {
        ui.painter().text(pos2(wave.max.x - 8.0, wave.min.y + 10.0), Align2::RIGHT_CENTER, "Analysing…", bold(11.0), t.accent);
    }
    // Dragging the waveform: a nudge while heard (left moves the music on), else the music
    // follows the pointer.
    let i = d.index();
    if resp.drag_started()
        && let Some(x) = app.decks_view.search.get_mut(i)
    {
        *x = Some((s.position, 0.0));
    }
    if !resp.dragged() {
        if let Some(x) = app.decks_view.search.get_mut(i) {
            *x = None;
        }
    } else if s.playing || s.previewing {
        let dx = -resp.drag_delta().x;
        let dt = ui.input(|inp| inp.stable_dt).clamp(1.0 / 240.0, 0.1);
        if dx.abs() >= 0.5 {
            app.decks_view.focus = i;
            run(app, "deck.jog", json!({"deck": d.id(), "delta": f64::from((dx / dt / NUDGE_PX_S).clamp(-1.0, 1.0)), "mode": "nudge"}));
        }
    } else if let Some((from, moved)) = app.decks_view.search.get(i).copied().flatten() {
        let moved = moved + resp.drag_delta().x;
        if let Some(x) = app.decks_view.search.get_mut(i) {
            *x = Some((from, moved));
        }
        if resp.drag_delta().x.abs() > 0.0 {
            let spp = seconds * sr * tempo / f64::from(wave.width().max(1.0));
            let at = (from - f64::from(moved) * spp).clamp(0.0, track.frames() as f64);
            app.decks_view.focus = i;
            run(app, "deck.seek", json!({"deck": d.id(), "at": at / sr}));
        }
    }
    if let Some(p) = dropped(ui, wave) {
        load(app, &p, Some(d), false);
    }
}

/// Where a deck's beat grid came from, in words.
fn grid_source(from: &str) -> &'static str {
    match from {
        "given" => "grid set by hand",
        "library" => "grid from mixforge's library",
        "mixforge" => "grid analysed by mixforge",
        "tag" => "grid from the BPM tag",
        _ => "beat grid",
    }
}

/// The time `secs` as `m:ss.t`.
fn clock_tenths(secs: f64) -> String {
    if !secs.is_finite() {
        return "–".into();
    }
    let neg = secs < 0.0;
    let tenths = (secs.abs() * 10.0).floor() as u64;
    format!("{}{}:{:02}.{}", if neg { "-" } else { "" }, tenths / 600, (tenths / 10) % 60, tenths % 10)
}

fn deck_panel(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, position: f64) {
    let t = Tokens::current();
    let i = d.index();
    let (s, track) = deck(app, d);
    let sr = app.engine.session().sample_rate.as_f64().max(1.0);
    let focus = app.decks_view.focus == i;
    let tint = tint(d);
    ui.painter().rect_filled(r, 4.0, t.panel_bg);
    if focus {
        ui.painter().rect_stroke(r, 4.0, Stroke::new(1.0, tint.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    }
    // A click anywhere on the panel focuses it (for Space).
    if ui.input(|inp| inp.pointer.any_pressed() && inp.pointer.interact_pos().is_some_and(|p| r.contains(p))) {
        app.decks_view.focus = i;
    }
    let inner = r.shrink(10.0);
    // Header: badge, title and artist; the tempo, pitch and key on the right.
    let badge = Rect::from_min_size(inner.min, vec2(34.0, 34.0));
    ui.painter().rect_filled(badge, 5.0, tint);
    ui.painter().text(badge.center(), Align2::CENTER_CENTER, name(d), bold(19.0), Color32::BLACK);
    let info_w = 190.0;
    let title_w = (inner.width() - badge.width() - info_w - 16.0).max(40.0);
    let tx = badge.max.x + 10.0;
    let (title, artist) = track.as_ref().map_or(("No track".to_string(), String::new()), |t| (t.title.clone(), t.artist.clone()));
    elided_text(
        ui.painter(),
        pos2(tx, inner.min.y - 1.0),
        Align2::LEFT_TOP,
        &title,
        bold(16.0),
        if track.is_some() { t.text } else { t.text_dim },
        title_w,
    );
    elided_text(ui.painter(), pos2(tx, inner.min.y + 19.0), Align2::LEFT_TOP, &artist, regular(12.0), t.text_dim, title_w);
    let right = inner.max.x;
    let bpm =
        if s.bpm > 0.0 { format!("{:.2}", s.bpm) } else { track.as_ref().and_then(|t| t.bpm).map_or_else(|| "–".into(), |b| format!("{b:.2}")) };
    let bpm_r = ui.painter().text(pos2(right, inner.min.y - 4.0), Align2::RIGHT_TOP, bpm, mono(26.0), if s.synced { tint } else { t.text });
    ui.painter().text(pos2(bpm_r.min.x - 6.0, bpm_r.max.y - 4.0), Align2::RIGHT_BOTTOM, "BPM", bold(9.0), t.text_dim);
    // Chips under the tempo: the pitch and its range, then the key.
    let info = app.decks_view.info.get(i).cloned().flatten();
    let key_src = info.as_ref().and_then(|v| v.get("key_source")).and_then(Value::as_str).map(library_browser::source_short).unwrap_or("");
    let key = track.as_ref().and_then(|t| t.key.clone());
    let chip_y = bpm_r.max.y + 3.0;
    let key_text = key.as_ref().map_or_else(|| "no key".to_string(), |k| if key_src.is_empty() { k.clone() } else { format!("{k} · {key_src}") });
    let key_r = chip(ui, pos2(right, chip_y), &key_text, if key.is_some() { t.accent } else { t.text_dim }, key.is_some());
    let pitch = format!("{:+.2} %  ±{:.0}", s.pitch * 100.0, s.range.max(PITCH_RANGES[0]) * 100.0);
    let off = s.pitch.abs() > 1e-6;
    chip(ui, pos2(key_r.min.x - 6.0, chip_y), &pitch, if off { t.text } else { t.text_dim }, false);
    // Times: elapsed bright, remaining dim, both large.
    let line = inner.min.y + 40.0;
    if let Some(tr) = &track {
        let total = tr.frames() as f64 / sr;
        let now = position / sr;
        let rate = if s.rate.is_finite() && s.rate > 0.05 { s.rate } else { 1.0 };
        let elapsed = ui.painter().text(pos2(inner.min.x, line), Align2::LEFT_TOP, clock_tenths(now), mono(19.0), t.text);
        let remain = format!("-{}", clock_tenths(((total - now) / rate).max(0.0)));
        let rem = ui.painter().text(pos2(elapsed.max.x + 16.0, line), Align2::LEFT_TOP, remain, mono(19.0), t.text_dim);
        // The grid's source, in words; a missing grid in the warning colour (no sync or loops).
        let (grid, colour) = tr.grid_from.map_or(("no beat grid: sync and loops off", t.rec), |g| (grid_source(g), t.text_dim));
        ui.painter().text(pos2(rem.max.x + 16.0, rem.center().y), Align2::LEFT_CENTER, grid, regular(10.5), colour);
    }
    let note_x = inner.min.x + 360.0;
    if app.engine.deck_loading_on(d) {
        ui.painter().text(pos2(note_x, line + 5.0), Align2::LEFT_TOP, "Analysing…", bold(11.0), t.accent);
    } else if let Some(e) = app.decks_view.load_error.get(i).cloned().flatten() {
        elided_text(ui.painter(), pos2(note_x, line + 5.0), Align2::LEFT_TOP, &e, regular(11.0), t.rec, (inner.max.x - note_x).max(20.0));
    }
    // The overview: click or drag to seek.
    let ov = Rect::from_min_size(pos2(inner.min.x, inner.min.y + 68.0), vec2(inner.width(), 40.0));
    let phrase = Rect::from_min_size(pos2(ov.min.x, ov.max.y + 2.0), vec2(ov.width(), 5.0));
    let ov_resp = ui.interact(ov, Id::new(("decks_overview", d.id())), Sense::click_and_drag());
    let ov_label = format!("Deck {} Overview", name(d));
    ov_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, &ov_label));
    if let Some(tr) = &track {
        let by_freq = app.waveform_bands();
        if let Some(cache) = app.decks_view.overviews.get_mut(i) {
            deck_wave::overview(ui.painter(), ov, cache, tr, &s, position, by_freq, tint);
        }
        deck_wave::phrase_bar(ui, phrase, tr, position);
        if (ov_resp.clicked() || ov_resp.dragged())
            && let Some(p) = ov_resp.interact_pointer_pos()
        {
            let frames = tr.frames() as f64;
            let mut at = (f64::from((p.x - ov.min.x) / ov.width().max(1.0))).clamp(0.0, 1.0) * frames;
            if app.engine.deck_status.quantise
                && let Some(g) = &tr.grid
            {
                at = g.pos_at(g.beat_at(at).round()).clamp(0.0, frames);
            }
            run(app, "deck.seek", json!({"deck": d.id(), "at": at / sr}));
        }
    } else {
        ui.painter().rect_filled(ov, 2.0, t.window_bg);
    }
    // Controls: pitch on the outer side, the jog on the inner.
    let ctl = Rect::from_min_max(pos2(inner.min.x, phrase.max.y + 10.0), inner.max);
    let pitch_w = 46.0;
    let jog_w = ctl.height().clamp(90.0, 170.0);
    let (pitch_r, main_r, jog_r) = if d == DeckId::A {
        let p = Rect::from_min_size(ctl.min, vec2(pitch_w, ctl.height()));
        let j = Rect::from_min_size(pos2(ctl.max.x - jog_w, ctl.min.y), vec2(jog_w, ctl.height()));
        (p, Rect::from_min_max(pos2(p.max.x + 8.0, ctl.min.y), pos2(j.min.x - 8.0, ctl.max.y)), j)
    } else {
        let j = Rect::from_min_size(ctl.min, vec2(jog_w, ctl.height()));
        let p = Rect::from_min_size(pos2(ctl.max.x - pitch_w, ctl.min.y), vec2(pitch_w, ctl.height()));
        (p, Rect::from_min_max(pos2(j.max.x + 8.0, ctl.min.y), pos2(p.min.x - 8.0, ctl.max.y)), j)
    };
    pitch_fader(app, ui, pitch_r, d, &s, track.as_deref());
    jog(app, ui, jog_r, d, &s, position, track.as_deref());
    transport(app, ui, main_r, d, &s, track.as_deref());
    if let Some(p) = dropped(ui, r) {
        load(app, &p, Some(d), false);
    }
}

fn transport(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, track: Option<&DeckTrack>) {
    let t = Tokens::current();
    let loaded = track.is_some();
    let i = d.index();
    let n = name(d);
    let gap = 6.0;
    // Row 1: Play, Cue | Sync, Master Tempo | Load.
    let row = Rect::from_min_size(r.min, vec2(r.width(), 44.0));
    let mut x = row.min.x;
    let mut cell = |w: f32, h: f32| {
        let c = Rect::from_min_size(pos2(x, row.min.y + (row.height() - h) / 2.0), vec2(w, h));
        x += w + gap;
        c
    };
    let play_r = cell(66.0, 44.0);
    let cue_r = cell(66.0, 44.0);
    let sync_r = cell(54.0, 30.0);
    let mt_r = cell(42.0, 30.0);
    let play = button(ui, play_r, "", s.playing, t.transport_play, &format!("Deck {n} Play"), 16.0);
    // The icon, drawn: a triangle, or two bars while playing.
    let (c, ink) = (play_r.center(), if s.playing { t.text_dark } else { t.text });
    if s.playing {
        for dx in [-5.0, 3.0] {
            ui.painter().rect_filled(Rect::from_min_size(pos2(c.x + dx, c.y - 8.0), vec2(4.0, 16.0)), 1.0, ink);
        }
    } else {
        ui.painter().add(egui::Shape::convex_polygon(
            vec![pos2(c.x - 6.0, c.y - 9.0), pos2(c.x + 9.0, c.y), pos2(c.x - 6.0, c.y + 9.0)],
            ink,
            Stroke::NONE,
        ));
    }
    if play.on_hover_text(if s.playing { "Pause (Space when this deck is focused)" } else { "Play (Space when this deck is focused)" }).clicked()
        && loaded
    {
        app.decks_view.focus = i;
        run(app, if s.playing { "deck.pause" } else { "deck.play" }, json!({"deck": d.id()}));
    }
    let cue = button(ui, cue_r, "CUE", s.previewing, t.accent, &format!("Deck {n} Cue"), 13.0).on_hover_text(format!(
        "Stopped: sets the cue here and plays while held. Playing: back to the cue, paused. Key: {}",
        if d == DeckId::A { "Q" } else { "W" }
    ));
    if loaded {
        pointer_hold(app, ui, &cue, d, Held::Cue, false);
    }
    if button(ui, sync_r, "SYNC", s.synced, tint(d), &format!("Deck {n} Sync"), 11.0)
        .on_hover_text("Beat sync to the other deck (tempo, then phase). Key: S on the focused deck")
        .clicked()
    {
        run(app, "deck.sync", json!({"deck": d.id(), "on": !s.synced}));
    }
    if button(ui, mt_r, "MT", s.key_lock, t.accent, &format!("Deck {n} Master Tempo"), 11.0).on_hover_text("Master Tempo (key lock)").clicked() {
        run(app, "deck.master_tempo", json!({"deck": d.id(), "on": !s.key_lock}));
    }
    let load_r = Rect::from_min_max(pos2((row.max.x - 70.0).max(x), row.min.y + 7.0), pos2(row.max.x, row.max.y - 7.0));
    // Load Lock: the deck is heard (playing, or a cue preview).
    let locked = s.playing || s.previewing;
    let load_resp =
        button(ui, load_r, if locked { "LOCKED" } else { "LOAD" }, false, t.accent, &format!("Deck {n} Load"), if locked { 9.5 } else { 11.0 });
    if locked {
        ui.painter().rect_stroke(load_r, 3.0, Stroke::new(2.0, t.rec), egui::StrokeKind::Inside);
    }
    // The deck-has-track light.
    ui.painter().circle_filled(pos2(load_r.min.x + 7.0, load_r.min.y + 7.0), 3.0, if loaded { t.meter_green } else { t.meter_bg });
    let tip = if locked {
        format!("Deck {n} is playing: Load Lock. Shift+click loads anyway (it fades out first)")
    } else {
        format!("Load the selected library track on Deck {n}")
    };
    if load_resp.on_hover_text(tip).clicked() {
        let force = ui.input(|i| i.modifiers.shift);
        if let Some(p) = app.decks_view.browser.selected_path() {
            load(app, &p, Some(d), force);
        } else {
            app.ui.status = "Pick a track in the library first".into();
        }
    }
    // Row 2: the hot cue pads.
    // The pads take the height the rows below leave, so the rows reach the panel's foot.
    let row_h = KEY_ROW;
    let pad_h = (r.height() - row.height() - 2.0 * row_h - 3.0 * 10.0).clamp(80.0, 148.0);
    let pads = Rect::from_min_size(pos2(r.min.x, row.max.y + 10.0), vec2(r.width(), pad_h));
    // A named list: A–D down the left, E–H down the right.
    let rows = HOT_CUES.div_ceil(2);
    let (cw, rh) = ((pads.width() - gap) / 2.0, ((pads.height() - 4.0 * (rows as f32 - 1.0)) / rows as f32).min(34.0));
    for slot in 0..HOT_CUES {
        let (col, row) = (slot / rows, slot % rows);
        let pr = Rect::from_min_size(pos2(pads.min.x + col as f32 * (cw + gap), pads.min.y + row as f32 * (rh + 4.0)), vec2(cw, rh));
        pad(app, ui, pr, d, s, slot, track);
    }
    // Row 3: beat loops.
    let lr = Rect::from_min_size(pos2(r.min.x, pads.max.y + 10.0), vec2(r.width(), row_h));
    let loop_beats = loop_length(app, d, s);
    let small = 8;
    let lw = (lr.width() - gap * (small as f32 - 1.0)) / small as f32;
    let cell_at = |k: usize, row: Rect| Rect::from_min_size(pos2(row.min.x + k as f32 * (lw + gap), row.min.y), vec2(lw, row.height()));
    let frac = cell_at(0, lr);
    let frac_on = loop_beats.is_some_and(|b| b < 0.99);
    let frac_text = match loop_beats {
        Some(b) if b < 0.3 => "¼ beat",
        Some(b) if b < 0.99 => "½ beat",
        _ => "¼ ½",
    };
    let fresp = button(ui, frac, frac_text, frac_on, t.accent, &format!("Deck {n} Short Loops"), 10.5).on_hover_text("A loop of ¼ or ½ beat");
    egui::Popup::menu(&fresp).show(|ui| {
        for (label, beats) in [("1/4 beat", "1/4"), ("1/2 beat", "1/2")] {
            if ui.button(label).clicked() {
                run(app, "deck.loop", json!({"deck": d.id(), "beats": beats}));
                ui.close();
            }
        }
    });
    for (k, beats) in LOOPS.iter().enumerate() {
        let on = loop_beats.is_some_and(|b| (b - beats).abs() < 0.01);
        let label = format!("{beats:.0}");
        if button(ui, cell_at(k + 1, lr), &label, on, t.accent, &format!("Deck {n} Loop {beats:.0}"), 11.0)
            .on_hover_text(format!("A {beats:.0}-beat loop"))
            .clicked()
        {
            if on {
                run(app, "deck.loop", json!({"deck": d.id(), "action": "exit"}));
            } else {
                run(app, "deck.loop", json!({"deck": d.id(), "beats": beats}));
            }
        }
    }
    let hd = cell_at(7, lr);
    let (h1, h2) = (Rect::from_min_max(hd.min, pos2(hd.center().x - 1.0, hd.max.y)), Rect::from_min_max(pos2(hd.center().x + 1.0, hd.min.y), hd.max));
    if button(ui, h1, "½", false, t.accent, &format!("Deck {n} Loop Halve"), 11.0).on_hover_text("Halve the loop").clicked() {
        run(app, "deck.loop", json!({"deck": d.id(), "action": "halve"}));
    }
    if button(ui, h2, "×2", false, t.accent, &format!("Deck {n} Loop Double"), 10.0).on_hover_text("Double the loop").clicked() {
        run(app, "deck.loop", json!({"deck": d.id(), "action": "double"}));
    }
    // Row 4: loop in/out, exit or reloop, beat jump.
    let r4 = Rect::from_min_size(pos2(r.min.x, lr.max.y + 10.0), vec2(r.width(), row_h));
    for (k, (text, action, tip)) in [("IN", "in", "Loop in here"), ("OUT", "out", "Loop out here: the loop starts")].into_iter().enumerate() {
        if button(ui, cell_at(k, r4), text, false, t.accent, &format!("Deck {n} Loop {text}"), 10.5).on_hover_text(tip).clicked() {
            run(app, "deck.loop", json!({"deck": d.id(), "action": action}));
        }
    }
    let ex = Rect::from_min_max(cell_at(2, r4).min, cell_at(3, r4).max);
    let (text, action) = if s.loop_on { ("EXIT", "exit") } else { ("RELOOP", "reloop") };
    if button(ui, ex, text, s.loop_on, Color32::from_rgb(255, 170, 40), &format!("Deck {n} Loop Exit"), 10.5)
        .on_hover_text("Leave the loop, or go back into the last one")
        .clicked()
    {
        run(app, "deck.loop", json!({"deck": d.id(), "action": action}));
    }
    let jump = jump_beats(app, d);
    let back = cell_at(5, r4);
    let size = cell_at(6, r4);
    let fwd = cell_at(7, r4);
    ui.painter().text(pos2(cell_at(4, r4).max.x, r4.center().y), Align2::RIGHT_CENTER, "JUMP", bold(9.5), t.text_dim);
    if button(ui, back, "◀", false, t.accent, &format!("Deck {n} Beat Jump Back"), 11.0)
        .on_hover_text(format!("Jump {jump:.0} beats back (key: comma)"))
        .clicked()
    {
        run(app, "deck.beatjump", json!({"deck": d.id(), "beats": -jump}));
    }
    if button(ui, size, &format!("{jump:.0}"), false, t.accent, &format!("Deck {n} Beat Jump Size"), 11.0)
        .on_hover_text("Beats a jump moves (click for the next size)")
        .clicked()
    {
        let k = app.decks_view.jump.get(i).copied().flatten().unwrap_or(DEFAULT_JUMP);
        if let Some(j) = app.decks_view.jump.get_mut(i) {
            *j = Some((k + 1) % JUMPS.len());
        }
    }
    if button(ui, fwd, "▶", false, t.accent, &format!("Deck {n} Beat Jump Forward"), 11.0)
        .on_hover_text(format!("Jump {jump:.0} beats on (key: full stop)"))
        .clicked()
    {
        run(app, "deck.beatjump", json!({"deck": d.id(), "beats": jump}));
    }
}

/// The loop's length in beats, while one is set.
fn loop_length(app: &SoundApp, d: DeckId, s: &DeckState) -> Option<f64> {
    let (a, b) = (s.loop_in?, s.loop_out?);
    if !s.loop_on {
        return None;
    }
    let g = app.engine.deck_tracks.get(d.index())?.as_ref()?.grid.as_ref()?;
    Some(g.beat_at(b) - g.beat_at(a))
}

/// The names of hot cues A–D when they come from a DJ set or mixforge.
const CUE_NAMES: [&str; 4] = ["Mix in", "Bass hit", "Mix out", "Bass gone"];

/// What hot cue `slot` is called: the set's or mixforge's name while it is still at the cue they
/// gave it, else the section starting there, else its time.
fn cue_name(slot: usize, s: &DeckState, track: Option<&DeckTrack>, sr: f64) -> Option<String> {
    let at = s.hot.get(slot).copied().flatten()?;
    let given = track.and_then(|t| t.cues.get(slot).copied().flatten());
    if let Some(n) = CUE_NAMES.get(slot).filter(|_| given.is_some_and(|g| (at - g).abs() < 1.0)) {
        return Some((*n).to_string());
    }
    let beat = track.and_then(|t| t.grid.as_ref()).map_or(0.25 * sr, |g| g.period_at(at) / 2.0);
    if let Some(x) = track.and_then(|t| deck_wave::section_at(t, at, beat)) {
        return Some(deck_wave::section_name(&x.label));
    }
    Some(library_browser::clock(at / sr))
}

/// A hot cue row: its letter in its colour, then its name (or a dash while empty). Click: jump
/// (held while stopped: plays from it) or set; Shift+click or right-click clears.
fn pad(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, slot: usize, track: Option<&DeckTrack>) {
    let t = Tokens::current();
    let set = s.hot.get(slot).copied().flatten().is_some();
    let colour = hot_cue_colour(slot);
    let letter = hot_cue_letter(slot);
    let label = format!("Deck {} Hot Cue {letter}", name(d));
    let resp = ui.interact(r, Id::new(("decks_pad", d.id(), slot)), Sense::click());
    let down = resp.is_pointer_button_down_on();
    let p = ui.painter();
    let bg = if down && set {
        colour.gamma_multiply(0.45)
    } else if resp.hovered() {
        t.button_hi
    } else {
        t.window_bg
    };
    p.rect_filled(r, 3.0, bg);
    let sq = Rect::from_min_size(r.min, vec2(r.height(), r.height()));
    if set {
        p.rect_filled(sq, 3.0, colour);
    } else {
        p.rect_stroke(sq.shrink(1.0), 3.0, Stroke::new(1.0, colour.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    }
    p.text(sq.center(), Align2::CENTER_CENTER, letter, bold(13.0), if set { Color32::BLACK } else { colour });
    let sr = app.engine.session().sample_rate.as_f64().max(1.0);
    let cue = cue_name(slot, s, track, sr);
    let text_r = r.max.x - sq.max.x - 12.0;
    elided_text(
        p,
        pos2(sq.max.x + 8.0, r.center().y),
        Align2::LEFT_CENTER,
        cue.as_deref().unwrap_or("—"),
        if set { bold(12.5) } else { regular(12.0) },
        if set { t.text } else { t.text_dim.gamma_multiply(0.6) },
        text_r.max(10.0),
    );
    let on = set;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, &label));
    let tip = match cue {
        Some(n) => format!("{n}: jump (held while stopped: plays from it). Shift+click or right-click clears. Key: {}", slot + 1),
        None => format!("Set a hot cue here. Key: {}", slot + 1),
    };
    let resp = resp.on_hover_text(tip);
    if track.is_none() {
        return;
    }
    let shift = ui.input(|inp| inp.modifiers.shift);
    pointer_hold(app, ui, &resp, d, Held::Pad { slot, release: set }, shift);
    if resp.secondary_clicked() && set {
        run(app, "deck.hotcue", json!({"deck": d.id(), "n": slot + 1, "action": "clear"}));
    }
}

/// A vertical fader's travel (0 bottom, 1 top) from a drag in `r`, else `None`. Double-click
/// gives `reset`.
fn vfader(ui: &mut Ui, r: Rect, travel: f32, id: Id, label: &str, reset: f32) -> (Option<f32>, Response) {
    let t = Tokens::current();
    let resp = ui.interact(r, id, Sense::click_and_drag());
    let slot = Rect::from_center_size(r.center(), vec2(6.0, r.height() - 16.0));
    ui.painter().rect_filled(slot, 3.0, t.fader_track);
    for k in 0..=10 {
        let y = slot.max.y - slot.height() * k as f32 / 10.0;
        let w = if k % 5 == 0 { 9.0 } else { 5.0 };
        ui.painter().hline(slot.min.x - w - 2.0..=slot.min.x - 3.0, y, Stroke::new(1.0, t.text_dim.gamma_multiply(0.6)));
    }
    let y = slot.max.y - slot.height() * travel.clamp(0.0, 1.0);
    let cap = Rect::from_center_size(pos2(r.center().x, y), vec2(r.width().min(34.0), 14.0));
    ui.painter().rect_filled(cap, 2.0, shade(t.button_hi, 0.15));
    ui.painter().hline(cap.x_range(), y, Stroke::new(2.0, Color32::WHITE));
    let label = label.to_string();
    let value = f64::from(travel);
    resp.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, &label);
        info.value = Some(value);
        info
    });
    let mut out = None;
    if resp.dragged()
        && let Some(p) = resp.interact_pointer_pos()
    {
        out = Some(((slot.max.y - p.y) / slot.height().max(1.0)).clamp(0.0, 1.0));
    }
    if resp.double_clicked() {
        out = Some(reset);
    }
    (out, resp)
}

/// The travel to show for control `id`: the one being dragged, else what the decks report.
fn shown_value(app: &SoundApp, id: Id, reported: f32) -> f32 {
    app.decks_view.held.get(&id).copied().unwrap_or(reported)
}

fn hold_value(app: &mut SoundApp, id: Id, resp: &Response, value: Option<f32>) {
    match value {
        Some(v) if resp.dragged() => {
            app.decks_view.held.insert(id, v);
        }
        _ if !resp.dragged() => {
            app.decks_view.held.remove(&id);
        }
        _ => {}
    }
}

/// The pitch fader's travel (0 bottom, 1 top) for `pitch` (a fraction) in ±`range`: up is
/// slower, as on a CDJ.
fn pitch_travel(pitch: f64, range: f64) -> f32 {
    (0.5 - pitch / (2.0 * range)).clamp(0.0, 1.0) as f32
}

/// The pitch (a fraction) at the fader's `travel`.
fn travel_pitch(travel: f32, range: f64) -> f64 {
    (0.5 - f64::from(travel)) * 2.0 * range
}

fn pitch_fader(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, track: Option<&DeckTrack>) {
    let t = Tokens::current();
    let range = if s.range > 0.0 { s.range } else { PITCH_RANGES[0] };
    let fr = Rect::from_min_max(pos2(r.min.x, r.min.y + 12.0), pos2(r.max.x, r.max.y - 44.0));
    let id = Id::new(("decks_pitch", d.id()));
    // Up is slower, as on a CDJ: the top (travel 1) is −range, the bottom +range.
    let reported = pitch_travel(s.pitch, range);
    let travel = shown_value(app, id, reported);
    ui.painter().text(pos2(fr.center().x, fr.min.y - 2.0), Align2::CENTER_BOTTOM, "−", bold(10.0), t.text_dim);
    ui.painter().text(pos2(fr.center().x, fr.max.y + 1.0), Align2::CENTER_TOP, "+", bold(10.0), t.text_dim);
    let (v, resp) = vfader(ui, fr, travel, id, &format!("Deck {} Pitch", name(d)), 0.5);
    // 0 %: a bright mark either side of the slot.
    let mid = fr.center().y;
    ui.painter().hline(fr.min.x + 1.0..=fr.min.x + 9.0, mid, Stroke::new(2.0, t.meter_green));
    ui.painter().hline(fr.max.x - 9.0..=fr.max.x - 1.0, mid, Stroke::new(2.0, t.meter_green));
    // Soft takeover: where the deck's tempo really is (sync, a nudge), when not where the fader is.
    if let Some(bpm) = track.and_then(|tr| tr.bpm).filter(|b| *b > 0.0 && s.bpm > 0.0) {
        let actual = s.bpm / bpm - 1.0;
        if (actual - s.pitch).abs() > 0.0005 {
            let at = pitch_travel(actual, range);
            let slot_h = fr.height() - 16.0;
            let y = fr.max.y - 8.0 - slot_h * at;
            ui.painter().hline(fr.min.x + 2.0..=fr.max.x - 2.0, y, Stroke::new(2.0, t.accent.gamma_multiply(0.8)));
        }
    }
    let resp = resp.on_hover_text(
        "Pitch (up is slower, as on a CDJ). The yellow line is the tempo now (sync): move the fader to it to take over. Double-click: 0 %",
    );
    hold_value(app, id, &resp, v);
    if let Some(v) = v
        && track.is_some()
    {
        let value = travel_pitch(v, range) * 100.0;
        // Drawn at the deck's tempo, so it applies at once (no soft takeover).
        run(app, "deck.pitch", json!({"deck": d.id(), "value": (value * 100.0).round() / 100.0, "takeover": false}));
    }
    let rr = Rect::from_min_max(pos2(r.min.x, r.max.y - KEY_ROW), r.max);
    let label = format!("±{:.0}", range * 100.0);
    if button(ui, rr, &label, false, t.accent, &format!("Deck {} Pitch Range", name(d)), 10.5)
        .on_hover_text("Pitch range: ±8, ±16 or ±50 %")
        .clicked()
        && track.is_some()
    {
        let k = PITCH_RANGES.iter().position(|x| (x - range).abs() < 1e-6).unwrap_or(0);
        let next = PITCH_RANGES.get((k + 1) % PITCH_RANGES.len()).copied().unwrap_or(0.08);
        run(app, "deck.pitch", json!({"deck": d.id(), "range": (next * 100.0).round()}));
    }
}

fn jog(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, position: f64, track: Option<&DeckTrack>) {
    let t = Tokens::current();
    let size = r.width().min(r.height() - 10.0);
    let c = pos2(r.center().x, r.min.y + size / 2.0);
    let jr = Rect::from_center_size(c, vec2(size, size));
    let resp = ui.interact(jr, Id::new(("decks_jog", d.id())), Sense::drag());
    let label = format!("Deck {} Jog", name(d));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, &label));
    let p = ui.painter();
    let rad = size / 2.0 - 2.0;
    let tint = tint(d);
    // The platter: a rim of ticks, the track's progress round it in the deck's colour, a disc.
    p.circle_filled(c, rad, t.window_bg);
    for k in 0..48 {
        let a = k as f32 / 48.0 * std::f32::consts::TAU;
        let dir = vec2(a.sin(), -a.cos());
        let long = k % 4 == 0;
        p.line_segment(
            [c + dir * (rad - if long { 7.0 } else { 4.0 }), c + dir * (rad - 1.0)],
            Stroke::new(1.0, t.text_dim.gamma_multiply(if long { 0.7 } else { 0.35 })),
        );
    }
    if let Some(tr) = track {
        let frac = (position / tr.frames().max(1) as f64).clamp(0.0, 1.0) as f32;
        let n = (frac * 96.0).ceil() as usize;
        let ring = rad - 11.0;
        let pts: Vec<egui::Pos2> = (0..=n)
            .map(|k| {
                let a = k as f32 / 96.0 * std::f32::consts::TAU;
                c + vec2(a.sin(), -a.cos()) * ring
            })
            .collect();
        if pts.len() > 1 {
            p.add(egui::Shape::line(pts, Stroke::new(3.0, tint.gamma_multiply(0.85))));
        }
    }
    // Inside the ring: a dark disc with the tempo, the pitch and its range; a dot runs round its
    // edge once a bar (once every two seconds without a grid).
    let disc = rad - 16.0;
    p.circle_filled(c, disc, shade(t.window_bg, if resp.dragged() { 0.1 } else { 0.0 }));
    p.circle_stroke(c, disc, Stroke::new(1.5, t.border));
    let turn = match track.and_then(|tr| tr.grid.as_ref()) {
        Some(g) => g.beat_at(position) / 4.0,
        None => position / app.engine.session().sample_rate.as_f64().max(1.0) / 2.0,
    };
    let a = (turn.fract() * std::f64::consts::TAU) as f32;
    let live = s.playing || s.previewing;
    p.circle_filled(c + vec2(a.sin(), -a.cos()) * (disc - 6.0), 4.0, if live { Color32::WHITE } else { t.text_dim });
    if track.is_some() {
        let bpm = if s.bpm > 0.0 { format!("{:.2}", s.bpm) } else { track.and_then(|tr| tr.bpm).map_or_else(|| "–".into(), |b| format!("{b:.2}")) };
        let size = (disc * 0.36).clamp(14.0, 26.0);
        p.text(c - vec2(0.0, size * 0.15), Align2::CENTER_CENTER, bpm, mono(size), if s.synced { tint } else { t.text });
        let pitch = format!("{:+.1} %  ±{:.0}", s.pitch * 100.0, s.range.max(PITCH_RANGES[0]) * 100.0);
        p.text(c + vec2(0.0, size * 0.75), Align2::CENTER_CENTER, pitch, mono((size * 0.45).max(9.0)), t.text_dim);
    }
    p.text(c + vec2(0.0, disc * 0.62), Align2::CENTER_CENTER, if live { "NUDGE" } else { "SEARCH" }, bold(8.0), t.text_dim.gamma_multiply(0.7));
    let resp = resp.on_hover_text("Drag sideways: nudge while playing, search while stopped");
    if track.is_some() {
        jog_drag(app, ui, &resp, d, s);
    }
}

fn mixer(app: &mut SoundApp, ui: &mut Ui, r: Rect) {
    let t = Tokens::current();
    ui.painter().rect_filled(r, 4.0, t.panel_bg2);
    let st = app.engine.deck_status;
    let centre_w = 76.0;
    let ch_w = (r.width() - centre_w) / 2.0;
    let xf_h = 54.0;
    let body = Rect::from_min_max(r.min + vec2(0.0, 6.0), pos2(r.max.x, r.max.y - xf_h));
    for d in DeckId::ALL {
        let x = if d == DeckId::A { r.min.x } else { r.max.x - ch_w };
        channel(app, ui, Rect::from_min_size(pos2(x, body.min.y), vec2(ch_w, body.height())), d);
    }
    // Centre: master volume and meter, the fader curve.
    let c = Rect::from_min_size(pos2(r.min.x + ch_w, body.min.y), vec2(centre_w, body.height()));
    ui.painter().text(pos2(c.center().x, c.min.y + 2.0), Align2::CENTER_TOP, "MASTER", bold(10.0), t.text_dim);
    let id = Id::new("decks_master");
    let reported = if st.master_db.is_finite() { (st.master_db / 40.0 + 1.0).clamp(0.0, 1.0) } else { 1.0 };
    let mut travel = shown_value(app, id, reported);
    let mut kui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_center_size(pos2(c.center().x, c.min.y + 36.0), vec2(40.0, 40.0))));
    let resp = knob::knob(&mut kui, 40.0, &mut travel, Knob { label: "Master Volume", default: 1.0, detent: None, origin: 0.0, colour: t.accent });
    let db = if travel <= 0.0 { -60.0 } else { ((travel - 1.0) * 40.0).max(-60.0) };
    let resp = resp.on_hover_text(format!("Master volume {db:.1} dB, into the limiter at −1 dBTP. Double-click: 0 dB"));
    hold_value(app, id, &resp, Some(travel));
    if resp.changed() {
        run(app, "deck.master_volume", json!({"db": (db * 10.0).round() / 10.0}));
    }
    let mr = Rect::from_min_size(pos2(c.center().x - 9.0, c.min.y + 66.0), vec2(18.0, (body.height() - 156.0 - PHONES_H).max(30.0)));
    let [l, rgt] = app.decks_view.meters[2];
    meter(ui, Rect::from_min_size(mr.min, vec2(8.0, mr.height())), l, l, l >= 1.0);
    meter(ui, Rect::from_min_size(pos2(mr.min.x + 10.0, mr.min.y), vec2(8.0, mr.height())), rgt, rgt, rgt >= 1.0);
    headphones(app, ui, Rect::from_min_size(pos2(c.min.x, mr.max.y + 4.0), vec2(centre_w, PHONES_H)));
    let curve = st.decks[0].fader_curve;
    let (next, text) = match curve {
        FaderCurve::Linear => (FaderCurve::Smooth, "CH: LIN"),
        FaderCurve::Smooth => (FaderCurve::Sharp, "CH: SMOOTH"),
        FaderCurve::Sharp => (FaderCurve::Linear, "CH: SHARP"),
    };
    let qr = Rect::from_center_size(pos2(c.center().x, body.max.y - 62.0), vec2(centre_w - 10.0, 20.0));
    let q = st.quantise;
    if button(ui, qr, "QUANTISE", q, t.accent, "Quantise", 9.0).on_hover_text("Quantise cues, hot cues and loops to the beat (both decks)").clicked()
    {
        run(app, "deck.quantise", json!({"on": !q}));
    }
    let cr = Rect::from_center_size(pos2(c.center().x, body.max.y - 38.0), vec2(centre_w - 10.0, 20.0));
    if button(ui, cr, text, false, t.accent, "Channel Fader Curve", 8.5).on_hover_text("Channel fader curve (both channels)").clicked() {
        for d in DeckId::ALL {
            run(app, "deck.fader", json!({"deck": d.id(), "curve": next.id()}));
        }
    }
    // The crossfader and its curve.
    let xr = Rect::from_min_max(pos2(r.min.x + 40.0, r.max.y - xf_h + 10.0), pos2(r.max.x - 40.0, r.max.y - 14.0));
    let id = Id::new("decks_xfader");
    let reported = if st.xfader.is_finite() { st.xfader * 0.5 + 0.5 } else { 0.5 };
    let travel = shown_value(app, id, reported);
    let resp = ui.interact(xr, id, Sense::click_and_drag());
    let slot = Rect::from_center_size(xr.center(), vec2(xr.width() - 20.0, 6.0));
    ui.painter().rect_filled(slot, 3.0, t.fader_track);
    ui.painter().vline(slot.center().x, xr.y_range(), Stroke::new(1.0, t.text_dim));
    let x = slot.min.x + slot.width() * travel;
    let cap = Rect::from_center_size(pos2(x, xr.center().y), vec2(14.0, xr.height().min(28.0)));
    ui.painter().rect_filled(cap, 2.0, shade(t.button_hi, 0.15));
    ui.painter().vline(x, cap.y_range(), Stroke::new(2.0, Color32::WHITE));
    ui.painter().text(pos2(xr.min.x - 8.0, xr.center().y), Align2::RIGHT_CENTER, "A", bold(11.0), crate::theme::studio::DECK_A);
    ui.painter().text(pos2(xr.max.x + 8.0, xr.center().y), Align2::LEFT_CENTER, "B", bold(11.0), crate::theme::studio::DECK_B);
    let value = f64::from(travel);
    resp.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, "Crossfader");
        info.value = Some(value);
        info
    });
    let mut new = None;
    if resp.dragged()
        && let Some(p) = resp.interact_pointer_pos()
    {
        new = Some(((p.x - slot.min.x) / slot.width().max(1.0)).clamp(0.0, 1.0));
    }
    if resp.double_clicked() {
        new = Some(0.5);
    }
    let resp = resp.on_hover_text("Crossfader. Double-click: centre");
    hold_value(app, id, &resp, new);
    if let Some(v) = new {
        run(app, "deck.xfader", json!({"value": ((v * 2.0 - 1.0) * 1000.0).round() / 1000.0}));
    }
    let xc = st.xfader_curve;
    let (next, text) = match xc {
        XfadeCurve::Smooth => (XfadeCurve::Sharp, "XF: SMOOTH"),
        XfadeCurve::Sharp => (XfadeCurve::Smooth, "XF: CUT"),
    };
    let cr = Rect::from_center_size(pos2(c.center().x, body.max.y - 14.0), vec2(centre_w - 10.0, 20.0));
    if button(ui, cr, text, false, t.accent, "Crossfader Curve", 8.5).on_hover_text("Crossfader curve: smooth (constant power) or cut").clicked() {
        run(app, "deck.xfader", json!({"curve": next.id()}));
    }
    // Where the headphones play.
    let status = headphone_status(app);
    let sr = Rect::from_min_max(pos2(r.min.x, r.max.y - 13.0), r.max);
    let g = ui.painter().layout_no_wrap(status.clone(), regular(9.0), t.text_dim);
    ui.painter().galley(pos2(sr.center().x - g.size().x / 2.0, sr.min.y), g, t.text_dim);
    ui.interact(sr, Id::new("decks_phones_status"), Sense::hover()).widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &status));
}

/// The headphones section's height, in the mixer's centre.
const PHONES_H: f32 = 84.0;

/// "Headphones: outputs 3-4", or why there are none.
pub fn headphone_status(app: &SoundApp) -> String {
    match app.player.as_ref() {
        None => "Headphones: no audio device".into(),
        Some(p) if p.silent => "Headphones: no output (no audio device)".into(),
        Some(p) => match p.headphones {
            soundcraft_playback::HeadphoneRoute::None if p.headphone_note.is_some() => p.headphone_note.clone().unwrap_or_default(),
            soundcraft_playback::HeadphoneRoute::None if p.device_channels <= 2 => {
                format!("Headphones: no output ({}-channel device; choose Split in Setup to cue in one ear)", p.device_channels)
            }
            soundcraft_playback::HeadphoneRoute::None => "Headphones: no output (Setup › Playback Engine)".into(),
            soundcraft_playback::HeadphoneRoute::Split => "Headphones: split (left phones, right master)".into(),
            r => format!("Headphones: {}", r.label()),
        },
    }
}

/// The headphone controls: MIX and LEVEL, MASTER CUE and the headphone meter.
fn headphones(app: &mut SoundApp, ui: &mut Ui, r: Rect) {
    let t = Tokens::current();
    let st = app.engine.deck_status;
    ui.painter().text(pos2(r.center().x, r.min.y), Align2::CENTER_TOP, "PHONES", bold(9.0), t.text_dim);
    let ks = 26.0;
    let ky = r.min.y + 12.0;
    let mix = if st.headphone_mix.is_finite() { st.headphone_mix.clamp(0.0, 1.0) } else { 0.0 };
    deck_knob(
        app,
        ui,
        Rect::from_min_size(pos2(r.center().x - ks - 3.0, ky), vec2(ks, ks)),
        "Headphone Mix",
        mix,
        Knob { label: "", default: 0.0, detent: Some(0.5), origin: 0.0, colour: t.accent },
        &format!("Headphone mix: {:.0} % master (left: cue only, right: master only). Double-click: cue only", mix * 100.0),
        |app, v| run(app, "deck.headphone_mix", json!({"value": (f64::from(v) * 1000.0).round() / 1000.0})),
    );
    // The level's knob spans −60..0 dB evenly; all the way down is off.
    let (lo, hi) = soundcraft_mix::HEADPHONE_DB;
    let to_travel = |db: f32| ((db - lo) / (hi - lo)).clamp(0.0, 1.0);
    let db = st.headphone_db;
    let travel = if db.is_finite() { to_travel(db) } else { to_travel(soundcraft_mix::HEADPHONE_DEFAULT_DB) };
    let shown = if db <= lo { "off".to_string() } else { format!("{db:.1} dB") };
    deck_knob(
        app,
        ui,
        Rect::from_min_size(pos2(r.center().x + 3.0, ky), vec2(ks, ks)),
        "Headphone Level",
        travel,
        Knob { label: "", default: to_travel(soundcraft_mix::HEADPHONE_DEFAULT_DB), detent: None, origin: 0.0, colour: t.text },
        &format!("Headphone level {shown}, into its own limiter at −1 dBTP. Double-click: {} dB", soundcraft_mix::HEADPHONE_DEFAULT_DB),
        |app, v| {
            let db = lo + v * (hi - lo);
            run(app, "deck.headphone_level", json!({"db": (f64::from(db) * 10.0).round() / 10.0}));
        },
    );
    let br = Rect::from_center_size(pos2(r.center().x, ky + ks + 14.0), vec2(r.width() - 10.0, 20.0));
    let on = st.master_cue;
    if button(ui, br, "MASTER CUE", on, t.accent, "Master Cue", 8.0).on_hover_text("Master cue: the master joins the headphones' cue bus").clicked() {
        run(app, "deck.master_cue", json!({"on": !on}));
    }
    let [l, rr] = app.decks_view.meters.get(3).copied().unwrap_or_default();
    let my = br.max.y + 4.0;
    let w = r.width() - 14.0;
    for (k, v) in [l, rr].into_iter().enumerate() {
        let bar = Rect::from_min_size(pos2(r.min.x + 7.0, my + k as f32 * 5.0), vec2(w, 4.0));
        ui.painter().rect_filled(bar, 1.0, t.fader_track);
        let lit = Rect::from_min_size(bar.min, vec2(w * v.clamp(0.0, 1.0), 4.0));
        ui.painter().rect_filled(lit, 1.0, if v >= 0.89 { t.rec } else { t.meter_green });
    }
    ui.interact(Rect::from_min_size(pos2(r.min.x + 7.0, my), vec2(w, 9.0)), Id::new("decks_phones_meter"), Sense::hover())
        .on_hover_text("Headphone output level (after its limiter)");
}

/// A knob for deck `d`'s control, sending `cmd` with what `to_params` makes of its travel.
#[allow(clippy::too_many_arguments)]
fn deck_knob(app: &mut SoundApp, ui: &mut Ui, at: Rect, label: &str, reported: f32, k: Knob<'_>, tip: &str, send: impl FnOnce(&mut SoundApp, f32)) {
    let id = Id::new(("decks_knob", label));
    let mut travel = shown_value(app, id, reported);
    let mut kui = ui.new_child(egui::UiBuilder::new().max_rect(at));
    let resp = knob::knob(&mut kui, at.width().min(at.height()), &mut travel, Knob { label, ..k });
    let resp = resp.on_hover_text(tip);
    hold_value(app, id, &resp, Some(travel));
    if resp.changed() {
        send(app, travel);
    }
}

fn channel(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId) {
    let t = Tokens::current();
    let s = app.engine.deck_status.decks.get(d.index()).copied().unwrap_or_default();
    let n = name(d);
    let tint = tint(d);
    let ks = 32.0;
    let row_h = 38.0;
    // The knobs sit on the outer side, their kills inside.
    let kx = if d == DeckId::A { r.min.x + 14.0 } else { r.max.x - 14.0 - ks };
    let killx = if d == DeckId::A { kx + ks + 6.0 } else { kx - 6.0 - 30.0 };
    let mut y = r.min.y + 2.0;
    ui.painter().text(pos2(kx + ks / 2.0, y), Align2::CENTER_TOP, format!("CH {n}"), bold(10.0), tint);
    y += 14.0;
    let trim = s.trim_db;
    let tr = Rect::from_min_size(pos2(kx, y), vec2(ks, ks));
    ui.painter().text(pos2(killx + 15.0, y + ks / 2.0), Align2::CENTER_CENTER, "TRIM", bold(9.0), t.text_dim);
    deck_knob(
        app,
        ui,
        tr,
        &format!("Deck {n} Trim"),
        knob::trim_travel(trim),
        Knob { label: "", default: 0.5, detent: Some(0.5), origin: 0.5, colour: t.text },
        &format!("Trim {trim:+.1} dB"),
        |app, v| run(app, "deck.trim", json!({"deck": d.id(), "db": (knob::trim_db(v) * 10.0).round() / 10.0})),
    );
    y += row_h;
    for (band, text, colour) in [
        (EqBand::High, "HI", crate::theme::studio::HIGH),
        (EqBand::Mid, "MID", crate::theme::studio::MID),
        (EqBand::Low, "LOW", crate::theme::studio::LOW),
    ] {
        let db = s.eq_db.get(band.index()).copied().unwrap_or(0.0);
        let killed = db <= soundcraft_mix::dj_strip::EQ_KILL_DB;
        let kr = Rect::from_min_size(pos2(kx, y), vec2(ks, ks));
        let band_id = match band {
            EqBand::Low => "low",
            EqBand::Mid => "mid",
            EqBand::High => "high",
        };
        deck_knob(
            app,
            ui,
            kr,
            &format!("Deck {n} EQ {text}"),
            knob::eq_travel(db),
            Knob { label: "", default: 0.5, detent: Some(0.5), origin: 0.5, colour },
            &format!("{text} {} (−26..+6 dB). Double-click: 0 dB", if killed { "killed".to_string() } else { format!("{db:+.1} dB") }),
            |app, v| run(app, "deck.eq", json!({"deck": d.id(), "band": band_id, "db": (knob::eq_db(v) * 10.0).round() / 10.0})),
        );
        let kb = Rect::from_min_size(pos2(killx, y + 6.0), vec2(30.0, 20.0));
        if button(ui, kb, text, killed, t.rec, &format!("Deck {n} EQ {text} Kill"), 9.0).on_hover_text(format!("Kill the {text} band")).clicked() {
            let p = if killed { json!({"deck": d.id(), "band": band_id, "db": 0}) } else { json!({"deck": d.id(), "band": band_id, "db": "kill"}) };
            run(app, "deck.eq", p);
        }
        y += row_h;
    }
    let fr = Rect::from_min_size(pos2(kx, y), vec2(ks, ks));
    ui.painter().text(pos2(killx + 15.0, y + ks / 2.0), Align2::CENTER_CENTER, "FILTER", bold(8.5), t.text_dim);
    let filter = s.filter;
    deck_knob(
        app,
        ui,
        fr,
        &format!("Deck {n} Filter"),
        if filter.is_finite() { filter * 0.5 + 0.5 } else { 0.5 },
        Knob { label: "", default: 0.5, detent: Some(0.5), origin: 0.5, colour: t.accent },
        "Filter: left low-pass, right high-pass, off in the centre",
        |app, v| run(app, "deck.filter", json!({"deck": d.id(), "value": ((v * 2.0 - 1.0) * 1000.0).round() / 1000.0})),
    );
    y += row_h + 2.0;
    // The channel fader and meter.
    let fh = (r.max.y - y - 50.0).max(40.0);
    let fx = if d == DeckId::A { r.min.x + 12.0 } else { r.max.x - 12.0 - 36.0 };
    let id = Id::new(("decks_fader", d.id()));
    let travel = shown_value(app, id, if s.fader.is_finite() { s.fader } else { 1.0 });
    let (v, resp) = vfader(ui, Rect::from_min_size(pos2(fx, y), vec2(36.0, fh)), travel, id, &format!("Deck {n} Fader"), 1.0);
    hold_value(app, id, &resp, v);
    if let Some(v) = v {
        run(app, "deck.fader", json!({"deck": d.id(), "value": (f64::from(v) * 1000.0).round() / 1000.0}));
    }
    let mx = if d == DeckId::A { fx + 44.0 } else { fx - 26.0 };
    let [l, rr] = app.decks_view.meters.get(d.index()).copied().unwrap_or_default();
    meter(ui, Rect::from_min_size(pos2(mx, y), vec2(8.0, fh)), l, l, l >= 1.0);
    meter(ui, Rect::from_min_size(pos2(mx + 10.0, y), vec2(8.0, fh)), rr, rr, rr >= 1.0);
    // The headphone CUE button, under the fader.
    let cr = Rect::from_min_size(pos2(fx - 2.0, y + fh + 3.0), vec2(40.0, 20.0));
    if button(ui, cr, "CUE", s.headphone_cue, t.accent, &format!("Deck {n} Headphone Cue"), 9.0)
        .on_hover_text("Headphone cue: this channel in the headphones, pre-fader")
        .clicked()
    {
        run(app, "deck.headphone_cue", json!({"deck": d.id(), "on": !s.headphone_cue}));
    }
    // Crossfader assign.
    let ay = r.max.y - 20.0;
    let aw = 26.0;
    let ax = if d == DeckId::A { r.min.x + 8.0 } else { r.max.x - 8.0 - 3.0 * aw - 4.0 };
    for (k, (side, text)) in [(XfadeSide::A, "A"), (XfadeSide::Thru, "THRU"), (XfadeSide::B, "B")].into_iter().enumerate() {
        let br = Rect::from_min_size(pos2(ax + k as f32 * (aw + 2.0), ay), vec2(aw, 18.0));
        if button(ui, br, text, s.side == side, tint, &format!("Deck {n} Assign {text}"), if text.len() > 1 { 7.5 } else { 9.5 })
            .on_hover_text("Which side of the crossfader this channel plays through (THRU: always heard)")
            .clicked()
        {
            run(app, "deck.assign", json!({"deck": d.id(), "side": side.id()}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_playhead_eases_between_published_states_and_snaps_on_a_jump() {
        let sr = 48_000.0;
        let mut c = Clock::default();
        let mut s = DeckState { load: 1, playing: true, rate: 1.0, position: 0.0, ..Default::default() };
        c.advance(&s, 0.0, sr);
        // Published every 10 ms, shown every 16.7 ms: the shown playhead moves steadily.
        let mut last = 0.0;
        let mut steps = Vec::new();
        for f in 1..120 {
            let now = f64::from(f) / 60.0;
            s.position = (now / 0.01).floor() * 0.01 * sr;
            let x = c.advance(&s, now, sr);
            steps.push(x - last);
            last = x;
        }
        let ideal = sr / 60.0;
        assert!(steps.iter().skip(20).all(|d| (d - ideal).abs() < ideal * 0.35), "{steps:?}");
        // A jump (a loop, a hot cue) is shown at once.
        s.position = 0.0;
        assert!(c.advance(&s, 2.01, sr) < 0.02 * sr, "snapped back");
        // Paused, it is where the deck is.
        s.playing = false;
        s.position = 1234.0;
        assert_eq!(c.advance(&s, 2.1, sr), 1234.0);
    }

    #[test]
    fn the_pitch_fader_is_slower_at_the_top() {
        // Up is slower, as on a CDJ.
        assert_eq!(pitch_travel(-0.08, 0.08), 1.0);
        assert_eq!(pitch_travel(0.08, 0.08), 0.0);
        assert_eq!(pitch_travel(0.0, 0.08), 0.5);
        assert!((travel_pitch(1.0, 0.08) + 0.08).abs() < 1e-9);
        assert!((travel_pitch(pitch_travel(0.031, 0.16), 0.16) - 0.031).abs() < 1e-6);
    }

    #[test]
    fn load_status_says_where_the_grid_came_from_and_any_fallback() {
        let v = json!({"deck": "a", "title": "Tidal", "bpm": 138.0, "grid_from": "tag", "key": "9A", "key_source": "mik", "analysis_error": "no mixforge"});
        let s = load_status(&v);
        assert!(
            s.contains("Deck A: loaded Tidal") && s.contains("grid from the BPM tag") && s.contains("key 9A (MIK)") && s.contains("no mixforge"),
            "{s}"
        );
        let none = load_status(&json!({"deck": "b", "title": "X", "bpm": null, "grid_from": null}));
        assert!(none.contains("no beat grid"), "{none}");
        assert_eq!(clock_tenths(83.45), "1:23.4");
    }
}
