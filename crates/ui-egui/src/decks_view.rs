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
//! Keys while the tab is shown: Space plays or pauses the focused deck (never the arrangement),
//! Left/Right focus Deck A/B, Up/Down pick a library row, Enter loads it on the first free
//! deck, Shift+Left/Right on Deck A/B. Other plain keys do nothing here; Cmd shortcuts work.

use crate::SoundApp;
use crate::deck_wave::{self, ColumnCache, OverviewCache, Scrolling, hot_cue_colour, hot_cue_letter};
use crate::knob::{self, Knob};
use crate::library_browser::{self, Browser, Dragged};
use crate::theme::{Tokens, bold, mono, regular};
use crate::widgets::{elided_text, key as key_face, key_fill, meter, shade};
use egui::{Align2, Color32, Id, Key, Modifiers, Rect, Response, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_mix::{DeckId, DeckState, DeckTrack, EqBand, FaderCurve, HOT_CUES, PITCH_RANGES, XfadeCurve, XfadeSide};
use std::collections::HashMap;
use std::sync::Arc;

/// Seconds of playback across a scrolling waveform, by zoom step.
const ZOOMS: [f64; 7] = [2.0, 4.0, 6.0, 8.0, 12.0, 16.0, 32.0];
const DEFAULT_ZOOM: usize = 3;
/// Beat jump sizes.
const JUMPS: [f64; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
const DEFAULT_JUMP: usize = 2;
/// Beat loop lengths with buttons of their own (1/4 and 1/2 are in a menu).
const LOOPS: [f64; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
/// A jog drag of this many points is one jog event at full deflection.
const JOG_PX: f32 = 40.0;
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
        if !(s.playing || s.previewing) {
            self.shown = s.position;
        } else {
            let target = self.published + (now - self.seen).clamp(0.0, 0.1) * rate * sr;
            let predicted = self.shown + (now - self.at).clamp(0.0, 0.1) * rate * sr;
            let gap = target - predicted;
            self.shown = if gap.abs() > SNAP_S * sr { target } else { predicted + gap * EASE };
        }
        self.at = now;
        self.shown
    }
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
    cue_held: [bool; 2],
    /// The pad held: (slot, whether its release is sent).
    pad_held: [Option<(usize, bool)>; 2],
    jump: [Option<usize>; 2],
    /// Each deck's last `deck.load` report, and its last refusal or failure.
    pub info: [Option<Value>; 2],
    pub load_error: [Option<String>; 2],
    pub browser: Browser,
    /// Meter levels shown: A, B, master (L, R), falling smoothly.
    meters: [[f32; 2]; 3],
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

/// Loads `path` on `deck`, or the first free deck (empty, else not playing); `force` loads onto
/// a playing deck. A refusal shows on the deck and in the status line.
pub fn load(app: &mut SoundApp, path: &str, deck: Option<DeckId>, force: bool) {
    let deck = deck.or_else(|| {
        DeckId::ALL.into_iter().find(|d| app.engine.deck_tracks.get(d.index()).is_none_or(Option::is_none) && !app.engine.deck_loading_on(*d))
    });
    let mut p = json!({"path": path});
    if let Some(d) = deck {
        p["deck"] = json!(d.id());
    }
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
            let d = deck.unwrap_or(DeckId::A);
            if let Some(slot) = app.decks_view.load_error.get_mut(d.index()) {
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
        (Some(b), Some(from)) => s.push_str(&format!(" — {b:.1} BPM (grid from {from})")),
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

/// The Decks tab's keys. True when the key is the tab's (or does nothing here).
pub fn key(app: &mut SoundApp, k: Key, m: Modifiers) -> bool {
    if m.command || m.ctrl || m.alt {
        return false;
    }
    match k {
        Key::Space => {
            let d = deck_of(app.decks_view.focus);
            let (s, track) = deck(app, d);
            if track.is_some() {
                run(app, if s.playing { "deck.pause" } else { "deck.play" }, json!({"deck": d.id()}));
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
        Key::ArrowUp | Key::ArrowDown => {
            let b = &mut app.decks_view.browser;
            let n = b.rows.len();
            if n > 0 {
                let i = b.selected.unwrap_or(0);
                b.selected = Some(if k == Key::ArrowUp { i.saturating_sub(1) } else { (i + 1).min(n - 1) });
            }
        }
        Key::Enter => {
            if let Some(p) = app.decks_view.browser.selected_path() {
                load(app, &p, None, false);
            }
        }
        _ => {}
    }
    true
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
    feed_meters(app, ui.input(|i| i.stable_dt).clamp(0.0, 0.25));
    let wave_h = (full.height() * 0.25).clamp(150.0, 260.0);
    let mid_h = (full.height() * 0.41).clamp(360.0, 430.0);
    let waves = Rect::from_min_size(full.min, vec2(full.width(), wave_h));
    let mid = Rect::from_min_size(pos2(full.min.x, waves.max.y + 4.0), vec2(full.width(), mid_h));
    let lib = Rect::from_min_max(pos2(full.min.x, mid.max.y + 4.0), full.max);
    let before = app.decks_view.columns_computed();
    for (i, d) in DeckId::ALL.into_iter().enumerate() {
        let h = (waves.height() - 6.0) / 2.0;
        let r = Rect::from_min_size(pos2(waves.min.x + 4.0, waves.min.y + 2.0 + i as f32 * (h + 2.0)), vec2(waves.width() - 8.0, h));
        scrolling_wave(app, ui, r, d, shown.get(i).copied().unwrap_or(0.0));
    }
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
}

fn feed_meters(app: &mut SoundApp, dt: f32) {
    let st = app.engine.deck_status;
    let fall = 10f32.powf(-26.0 * dt / 20.0);
    let levels = [st.decks[0].level, st.decks[1].level, st.master_level];
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

/// A press-and-release key: returns (pressed this frame, released this frame).
fn hold_edges(resp: &Response, held: bool, ui: &Ui) -> (bool, bool) {
    let down = resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_down());
    (down && !held, !down && held)
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

/// Jog from a drag: a nudge while playing, a search while stopped.
fn jog_drag(app: &mut SoundApp, resp: &Response, d: DeckId, s: &DeckState, invert: bool) {
    if !resp.dragged() {
        return;
    }
    let dx = resp.drag_delta().x * if invert { -1.0 } else { 1.0 };
    if dx.abs() < 0.5 {
        return;
    }
    let delta = f64::from((dx / JOG_PX).clamp(-1.0, 1.0));
    let mode = if s.playing { "nudge" } else { "search" };
    app.decks_view.focus = d.index();
    run(app, "deck.jog", json!({"deck": d.id(), "delta": delta, "mode": mode}));
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
    let w = Scrolling { track: &track, state: &s, position, seconds, rate: sr, by_freq, tint };
    if let Some(cache) = app.decks_view.waves.get_mut(d.index()) {
        deck_wave::scrolling(ui.painter(), wave, cache, &w);
    }
    if app.engine.deck_loading_on(d) {
        ui.painter().text(pos2(wave.max.x - 8.0, wave.min.y + 10.0), Align2::RIGHT_CENTER, "Analysing…", bold(11.0), t.accent);
    }
    // Dragging the waveform jogs it (dragging it left moves the music on).
    jog_drag(app, &resp, d, &s, true);
    if let Some(p) = dropped(ui, wave) {
        load(app, &p, Some(d), false);
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
    let inner = r.shrink(8.0);
    // Header: badge, title, artist; BPM, pitch and key on the right.
    let badge = Rect::from_min_size(inner.min, vec2(30.0, 30.0));
    ui.painter().rect_filled(badge, 4.0, tint);
    ui.painter().text(badge.center(), Align2::CENTER_CENTER, name(d), bold(17.0), Color32::BLACK);
    let info_w = 150.0;
    let title_w = (inner.width() - badge.width() - info_w - 16.0).max(40.0);
    let tx = badge.max.x + 8.0;
    let (title, artist) = track.as_ref().map_or(("No track".to_string(), String::new()), |t| (t.title.clone(), t.artist.clone()));
    elided_text(
        ui.painter(),
        pos2(tx, inner.min.y),
        Align2::LEFT_TOP,
        &title,
        bold(15.0),
        if track.is_some() { t.text } else { t.text_dim },
        title_w,
    );
    elided_text(ui.painter(), pos2(tx, inner.min.y + 19.0), Align2::LEFT_TOP, &artist, regular(12.0), t.text_dim, title_w);
    let right = inner.max.x;
    let bpm =
        if s.bpm > 0.0 { format!("{:.2}", s.bpm) } else { track.as_ref().and_then(|t| t.bpm).map_or_else(|| "–".into(), |b| format!("{b:.2}")) };
    ui.painter().text(pos2(right, inner.min.y - 2.0), Align2::RIGHT_TOP, bpm, mono(22.0), if s.synced { tint } else { t.text });
    let pitch = format!("{:+.2} %  ±{:.0}", s.pitch * 100.0, s.range.max(PITCH_RANGES[0]) * 100.0);
    ui.painter().text(pos2(right - 82.0, inner.min.y + 30.0), Align2::RIGHT_TOP, pitch, mono(11.0), t.text_dim);
    let info = app.decks_view.info.get(i).cloned().flatten();
    let key_src = info.as_ref().and_then(|v| v.get("key_source")).and_then(Value::as_str).map(library_browser::source_short).unwrap_or("");
    let key_text = track
        .as_ref()
        .and_then(|t| t.key.clone())
        .map_or_else(|| "key –".into(), |k| if key_src.is_empty() { k } else { format!("{k} · {key_src}") });
    ui.painter().text(pos2(right, inner.min.y + 30.0), Align2::RIGHT_TOP, key_text, bold(11.5), t.accent);
    // Times, state and errors.
    let line = inner.min.y + 36.0;
    if let Some(tr) = &track {
        let total = tr.frames() as f64 / sr;
        let now = position / sr;
        let rate = if s.rate.is_finite() && s.rate > 0.05 { s.rate } else { 1.0 };
        let times = format!("{}   -{}", clock_tenths(now), clock_tenths(((total - now) / rate).max(0.0)));
        ui.painter().text(pos2(tx, line), Align2::LEFT_TOP, times, mono(12.5), t.text);
        let grid = tr.grid_from.map_or_else(|| "no grid".to_string(), |g| format!("grid: {g}"));
        ui.painter().text(pos2(tx + 170.0, line + 1.0), Align2::LEFT_TOP, grid, regular(11.0), t.text_dim);
    }
    if app.engine.deck_loading_on(d) {
        ui.painter().text(pos2(tx + 260.0, line + 1.0), Align2::LEFT_TOP, "Analysing…", bold(11.0), t.accent);
    } else if let Some(e) = app.decks_view.load_error.get(i).cloned().flatten() {
        elided_text(ui.painter(), pos2(tx + 260.0, line + 1.0), Align2::LEFT_TOP, &e, regular(11.0), t.rec, (inner.max.x - tx - 260.0).max(20.0));
    }
    // The overview: click or drag to seek.
    let ov = Rect::from_min_size(pos2(inner.min.x, inner.min.y + 56.0), vec2(inner.width(), 42.0));
    let ov_resp = ui.interact(ov, Id::new(("decks_overview", d.id())), Sense::click_and_drag());
    let ov_label = format!("Deck {} Overview", name(d));
    ov_resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, &ov_label));
    if let Some(tr) = &track {
        let by_freq = app.waveform_bands();
        if let Some(cache) = app.decks_view.overviews.get_mut(i) {
            deck_wave::overview(ui.painter(), ov, cache, tr, &s, position, by_freq, tint);
        }
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
    let ctl = Rect::from_min_max(pos2(inner.min.x, ov.max.y + 8.0), inner.max);
    let pitch_w = 46.0;
    let jog_w = ctl.height().clamp(90.0, 150.0);
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
    transport(app, ui, main_r, d, &s, track.is_some());
    if let Some(p) = dropped(ui, r) {
        load(app, &p, Some(d), false);
    }
}

fn transport(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, loaded: bool) {
    let t = Tokens::current();
    let i = d.index();
    let n = name(d);
    let gap = 6.0;
    // Row 1: Play, Cue | Sync, Master Tempo, Quantise | Load.
    let row = Rect::from_min_size(r.min, vec2(r.width(), 40.0));
    let mut x = row.min.x;
    let mut cell = |w: f32, h: f32| {
        let c = Rect::from_min_size(pos2(x, row.min.y + (row.height() - h) / 2.0), vec2(w, h));
        x += w + gap;
        c
    };
    let play_r = cell(62.0, 40.0);
    let cue_r = cell(62.0, 40.0);
    let sync_r = cell(50.0, 28.0);
    let mt_r = cell(40.0, 28.0);
    let q_r = cell(30.0, 28.0);
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
    let cue = button(ui, cue_r, "CUE", s.previewing, t.accent, &format!("Deck {n} Cue"), 13.0)
        .on_hover_text("Stopped: sets the cue here and plays while held. Playing: back to the cue, paused");
    let held = app.decks_view.cue_held.get(i).copied().unwrap_or(false);
    let (press, release) = hold_edges(&cue, held, ui);
    if press && loaded {
        app.decks_view.focus = i;
        run(app, "deck.cue", json!({"deck": d.id(), "down": true}));
    }
    if release && loaded {
        run(app, "deck.cue", json!({"deck": d.id(), "down": false}));
    }
    if let Some(h) = app.decks_view.cue_held.get_mut(i) {
        *h = (*h || press) && !release;
    }
    if button(ui, sync_r, "SYNC", s.synced, tint(d), &format!("Deck {n} Sync"), 11.0)
        .on_hover_text("Beat sync to the other deck (tempo, then phase)")
        .clicked()
    {
        run(app, "deck.sync", json!({"deck": d.id(), "on": !s.synced}));
    }
    if button(ui, mt_r, "MT", s.key_lock, t.accent, &format!("Deck {n} Master Tempo"), 11.0).on_hover_text("Master Tempo (key lock)").clicked() {
        run(app, "deck.master_tempo", json!({"deck": d.id(), "on": !s.key_lock}));
    }
    let q = app.engine.deck_status.quantise;
    if button(ui, q_r, "Q", q, t.accent, &format!("Deck {n} Quantise"), 11.0)
        .on_hover_text("Quantise cues, hot cues and loops to the beat (both decks)")
        .clicked()
    {
        run(app, "deck.quantise", json!({"on": !q}));
    }
    let load_r = Rect::from_min_max(pos2((row.max.x - 64.0).max(x), row.min.y + 6.0), pos2(row.max.x, row.max.y - 6.0));
    let locked = s.playing;
    let load_resp = button(ui, load_r, "LOAD", false, t.accent, &format!("Deck {n} Load"), 11.0);
    // The deck-has-track light.
    ui.painter().circle_filled(pos2(load_r.min.x + 7.0, load_r.min.y + 7.0), 3.0, if loaded { t.meter_green } else { t.meter_bg });
    if locked {
        ui.painter().text(pos2(load_r.max.x - 4.0, load_r.min.y + 2.0), Align2::RIGHT_TOP, "LOCK", bold(7.0), t.rec);
    }
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
    let pad_h = (r.height() - 40.0 - 24.0 - 24.0 - 8.0 - 8.0 - 6.0 - 24.0).clamp(34.0, 64.0);
    let pads = Rect::from_min_size(pos2(r.min.x, row.max.y + 8.0), vec2(r.width(), pad_h));
    let pw = (pads.width() - gap * (HOT_CUES as f32 - 1.0)) / HOT_CUES as f32;
    for slot in 0..HOT_CUES {
        let pr = Rect::from_min_size(pos2(pads.min.x + slot as f32 * (pw + gap), pads.min.y), vec2(pw, pads.height()));
        pad(app, ui, pr, d, s, slot, loaded);
    }
    // Row 3: beat loops.
    let lr = Rect::from_min_size(pos2(r.min.x, pads.max.y + 8.0), vec2(r.width(), 26.0));
    let loop_beats = loop_length(app, d, s);
    let small = 8;
    let lw = (lr.width() - gap * (small as f32 - 1.0)) / small as f32;
    let cell_at = |k: usize, row: Rect| Rect::from_min_size(pos2(row.min.x + k as f32 * (lw + gap), row.min.y), vec2(lw, row.height()));
    let frac = cell_at(0, lr);
    let mut menu = ui.new_child(egui::UiBuilder::new().max_rect(frac));
    let frac_on = loop_beats.is_some_and(|b| b < 0.99);
    let fresp = menu.add_sized(frac.size(), egui::Button::new(egui::RichText::new("¼ ½").size(11.0)).selected(frac_on));
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
    let r4 = Rect::from_min_size(pos2(r.min.x, lr.max.y + 6.0), vec2(r.width(), 26.0));
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
    let jump = JUMPS.get(app.decks_view.jump.get(i).copied().flatten().unwrap_or(DEFAULT_JUMP)).copied().unwrap_or(4.0);
    let back = cell_at(5, r4);
    let size = cell_at(6, r4);
    let fwd = cell_at(7, r4);
    ui.painter().text(pos2(cell_at(4, r4).max.x, r4.center().y), Align2::RIGHT_CENTER, "JUMP", bold(9.5), t.text_dim);
    if button(ui, back, "◀", false, t.accent, &format!("Deck {n} Beat Jump Back"), 11.0).on_hover_text(format!("Jump {jump:.0} beats back")).clicked()
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
    if button(ui, fwd, "▶", false, t.accent, &format!("Deck {n} Beat Jump Forward"), 11.0).on_hover_text(format!("Jump {jump:.0} beats on")).clicked()
    {
        run(app, "deck.beatjump", json!({"deck": d.id(), "beats": jump}));
    }
    ui.painter().text(
        pos2(r.min.x, r4.max.y + 8.0),
        Align2::LEFT_TOP,
        "Pads: click sets an empty one, jumps to a set one; Shift+click or right-click clears",
        regular(10.0),
        t.text_dim,
    );
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

fn pad(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, slot: usize, loaded: bool) {
    let i = d.index();
    let set = s.hot.get(slot).copied().flatten().is_some();
    let colour = hot_cue_colour(slot);
    let letter = hot_cue_letter(slot);
    let label = format!("Deck {} Hot Cue {letter}", name(d));
    let resp = ui.interact(r, Id::new(("decks_pad", d.id(), slot)), Sense::click());
    let fill = if set { if resp.is_pointer_button_down_on() { shade(colour, 0.2) } else { colour } } else { key_fill(&resp) };
    key_face(ui.painter(), r, &resp, fill);
    if !set {
        ui.painter().rect_stroke(r.shrink(2.0), 2.0, Stroke::new(1.0, colour.gamma_multiply(0.5)), egui::StrokeKind::Inside);
    }
    ui.painter().text(r.center(), Align2::CENTER_CENTER, letter, bold(14.0), if set { Color32::BLACK } else { colour });
    let on = set;
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, &label));
    let resp =
        resp.on_hover_text(if set { "Jump (held while stopped: plays from it). Shift+click or right-click clears" } else { "Set a hot cue here" });
    if !loaded {
        return;
    }
    let held = app.decks_view.pad_held.get(i).copied().flatten();
    let down = resp.is_pointer_button_down_on() && ui.input(|inp| inp.pointer.primary_down());
    let seq = |action: &str| json!({"deck": d.id(), "n": slot + 1, "action": action});
    if down && held.is_none_or(|(k, _)| k != slot) {
        app.decks_view.focus = i;
        let shift = ui.input(|inp| inp.modifiers.shift);
        let release = if shift {
            run(app, "deck.hotcue", seq("clear"));
            false
        } else if set {
            run(app, "deck.hotcue", seq("jump"));
            true
        } else {
            run(app, "deck.hotcue", seq("set"));
            false
        };
        if let Some(h) = app.decks_view.pad_held.get_mut(i) {
            *h = Some((slot, release));
        }
    } else if !down && let Some((k, release)) = held.filter(|(k, _)| *k == slot) {
        if release {
            run(app, "deck.hotcue", seq("release"));
        }
        let _ = k;
        if let Some(h) = app.decks_view.pad_held.get_mut(i) {
            *h = None;
        }
    }
    if resp.secondary_clicked() && set {
        run(app, "deck.hotcue", seq("clear"));
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

fn pitch_fader(app: &mut SoundApp, ui: &mut Ui, r: Rect, d: DeckId, s: &DeckState, track: Option<&DeckTrack>) {
    let t = Tokens::current();
    let range = if s.range > 0.0 { s.range } else { PITCH_RANGES[0] };
    let fr = Rect::from_min_max(r.min, pos2(r.max.x, r.max.y - 26.0));
    let id = Id::new(("decks_pitch", d.id()));
    // Up is slower, as on a CDJ: the top is −range.
    let reported = (0.5 - s.pitch / (2.0 * range)) as f32;
    let travel = shown_value(app, id, reported);
    ui.painter().text(pos2(fr.center().x, fr.min.y - 2.0), Align2::CENTER_BOTTOM, "−", bold(10.0), t.text_dim);
    let (v, resp) = vfader(ui, fr, 1.0 - travel, id, &format!("Deck {} Pitch", name(d)), 0.5);
    // Soft takeover: where the deck's tempo really is (sync, a nudge), when not where the fader is.
    if let Some(bpm) = track.and_then(|tr| tr.bpm).filter(|b| *b > 0.0 && s.bpm > 0.0) {
        let actual = s.bpm / bpm - 1.0;
        if (actual - s.pitch).abs() > 0.0005 {
            let at = (0.5 - actual / (2.0 * range)).clamp(0.0, 1.0) as f32;
            let slot_h = fr.height() - 16.0;
            let y = fr.min.y + 8.0 + slot_h * at;
            ui.painter().hline(fr.min.x + 2.0..=fr.max.x - 2.0, y, Stroke::new(2.0, t.accent.gamma_multiply(0.8)));
        }
    }
    let resp = resp.on_hover_text(
        "Pitch (up is slower, as on a CDJ). The yellow line is the tempo now (sync): move the fader to it to take over. Double-click: 0 %",
    );
    let pitch = v.map(|v| 1.0 - v);
    hold_value(app, id, &resp, pitch);
    if let Some(p) = pitch
        && track.is_some()
    {
        let value = (0.5 - f64::from(p)) * 2.0 * range * 100.0;
        run(app, "deck.pitch", json!({"deck": d.id(), "value": (value * 100.0).round() / 100.0}));
    }
    let rr = Rect::from_min_max(pos2(r.min.x, r.max.y - 22.0), r.max);
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
    p.circle_filled(c, rad, shade(t.panel_bg2, if resp.dragged() { 0.1 } else { 0.0 }));
    p.circle_stroke(c, rad, Stroke::new(2.0, t.border));
    p.circle_stroke(c, rad * 0.62, Stroke::new(1.0, t.border));
    // A mark that turns once a bar (once every two seconds without a grid).
    let turn = match track.and_then(|tr| tr.grid.as_ref()) {
        Some(g) => g.beat_at(position) / 4.0,
        None => position / app.engine.session().sample_rate.as_f64().max(1.0) / 2.0,
    };
    let a = (turn.fract() * std::f64::consts::TAU) as f32 - std::f32::consts::FRAC_PI_2;
    let dir = vec2(a.cos(), a.sin());
    p.line_segment([c + dir * rad * 0.66, c + dir * (rad - 3.0)], Stroke::new(3.0, if s.playing { Color32::WHITE } else { t.text_dim }));
    p.text(c, Align2::CENTER_CENTER, if s.playing { "NUDGE" } else { "SEARCH" }, bold(9.5), t.text_dim);
    let resp = resp.on_hover_text("Drag sideways: nudge while playing, search while stopped");
    if track.is_some() {
        jog_drag(app, &resp, d, s, false);
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
    let mr = Rect::from_min_size(pos2(c.center().x - 9.0, c.min.y + 64.0), vec2(18.0, (body.height() - 104.0).max(40.0)));
    let [l, rgt] = app.decks_view.meters[2];
    meter(ui, Rect::from_min_size(mr.min, vec2(8.0, mr.height())), l, l, l >= 1.0);
    meter(ui, Rect::from_min_size(pos2(mr.min.x + 10.0, mr.min.y), vec2(8.0, mr.height())), rgt, rgt, rgt >= 1.0);
    let curve = st.decks[0].fader_curve;
    let (next, text) = match curve {
        FaderCurve::Linear => (FaderCurve::Smooth, "LIN"),
        FaderCurve::Smooth => (FaderCurve::Sharp, "SMOOTH"),
        FaderCurve::Sharp => (FaderCurve::Linear, "SHARP"),
    };
    let cr = Rect::from_center_size(pos2(c.center().x, body.max.y - 14.0), vec2(centre_w - 12.0, 20.0));
    if button(ui, cr, text, false, t.accent, "Channel Fader Curve", 9.5).on_hover_text("Channel fader curve (both channels)").clicked() {
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
        XfadeCurve::Smooth => (XfadeCurve::Sharp, "SMOOTH"),
        XfadeCurve::Sharp => (XfadeCurve::Smooth, "CUT"),
    };
    let cr = Rect::from_center_size(pos2(r.center().x, r.max.y - 6.0), vec2(60.0, 12.0));
    let resp = ui.interact(cr, Id::new("decks_xcurve"), Sense::click());
    ui.painter().text(cr.center(), Align2::CENTER_CENTER, format!("curve: {text}"), regular(9.5), if resp.hovered() { t.text } else { t.text_dim });
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Crossfader Curve"));
    if resp.on_hover_text("Crossfader curve: smooth (constant power) or cut").clicked() {
        run(app, "deck.xfader", json!({"curve": next.id()}));
    }
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
    let fh = (r.max.y - y - 26.0).max(40.0);
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
    fn load_status_says_where_the_grid_came_from_and_any_fallback() {
        let v = json!({"deck": "a", "title": "Tidal", "bpm": 138.0, "grid_from": "tag", "key": "9A", "key_source": "mik", "analysis_error": "no mixforge"});
        let s = load_status(&v);
        assert!(s.contains("Deck A: loaded Tidal") && s.contains("grid from tag") && s.contains("key 9A (MIK)") && s.contains("no mixforge"), "{s}");
        let none = load_status(&json!({"deck": "b", "title": "X", "bpm": null, "grid_from": null}));
        assert!(none.contains("no beat grid"), "{none}");
        assert_eq!(clock_tenths(83.45), "1:23.4");
    }
}
