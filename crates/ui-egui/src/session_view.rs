//! View › Session: the Live columns' clip slots, launched over the playing set.
//!
//! A grid of 8 slots per Live column, a Scene column, a Stop button per column, Stop All and the
//! launch quantise. Each slot shows what `live.state` reports: empty, filled (name and colour),
//! queued (blinking until its boundary) or playing (a ring showing where its loop is). A slot's
//! right-click menu adds the time selection, loads a file, edits or clears it; a file dropped on
//! a slot loads into it.
//!
//! While the window has focus (it was clicked last), 1–8 launch the selected column's slots, 0
//! stops it, ←/→ select the column, Shift+1–8 launch a scene and Esc stops everything. Elsewhere
//! those keys keep their arrangement meaning.

use crate::SoundApp;
use crate::theme::{Tokens, bold, regular};
use egui::{Color32, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, vec2};
use serde_json::{Value, json};
use soundcraft_model::{LIVE_SLOTS, Quantise, SLOT_GAIN_DB, SlotMode};

/// Size of a slot cell.
const CELL: egui::Vec2 = vec2(116.0, 26.0);

/// The Session window's state.
#[derive(Debug, Default)]
pub struct SessionView {
    pub open: bool,
    /// The window was clicked last: its keys apply.
    pub focused: bool,
    /// The column 1–8 and 0 act on.
    pub column: usize,
    /// Where a dropped file loads into a slot this frame: (cell, column, row).
    pub drop_targets: Vec<(Rect, usize, usize)>,
    /// The slot being edited (Edit...).
    pub edit: Option<SlotEdit>,
}

/// The Edit Slot dialog's fields.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotEdit {
    pub column: usize,
    pub row: usize,
    pub name: String,
    pub color: [u8; 3],
    pub mode: SlotMode,
    /// `None` for a slot without a beat grid (it has no bars).
    pub length_bars: Option<u32>,
    pub gain_db: f32,
    /// The values when the dialog opened, to send only what changed.
    first: Box<Option<SlotEdit>>,
}

/// `window.session`: opens (or with `value`, shows or hides) the Session window. Opening it on a
/// session without Live columns sets them up (`live.setup`, one undo step).
pub fn command(app: &mut SoundApp, p: &Value) -> Result<Value, String> {
    let open = p.get("value").and_then(Value::as_bool).unwrap_or(!app.session_view.open);
    app.session_view.open = open;
    app.session_view.focused = open;
    if open && app.engine.session().live.columns.is_empty() {
        app.run("live.setup", json!({}))?;
    }
    Ok(json!({"value": open}))
}

/// What a column shows of `live.state`: the row playing with its loop position (0..1, `None`
/// for a one-shot) and the row queued (`Some(None)`: a stop).
#[derive(Debug, Clone, Copy, Default)]
struct ColumnState {
    playing: Option<(usize, Option<f64>)>,
    queued: Option<Option<usize>>,
}

fn states(app: &mut SoundApp) -> Vec<ColumnState> {
    let Ok(state) = app.engine.execute("live.state", &json!({})) else { return Vec::new() };
    let row = |v: &Value| v.as_u64().and_then(|r| usize::try_from(r).ok());
    state["columns"]
        .as_array()
        .map(|cols| {
            cols.iter()
                .map(|c| {
                    let p = &c["playing"];
                    let fraction = p["loop_beat"].as_f64().zip(p["length_beats"].as_f64()).map(|(b, l)| (b / l.max(1.0)).clamp(0.0, 1.0));
                    ColumnState {
                        playing: row(&p["slot"]).map(|r| (r, fraction)),
                        queued: c["queued"].is_object().then(|| row(&c["queued"]["slot"])),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Handles a key while the Session window has focus; true when it did.
pub fn key(app: &mut SoundApp, key: Key, m: Modifiers) -> bool {
    let view = &app.session_view;
    if !view.open || !view.focused || m.command || m.alt || m.ctrl {
        return false;
    }
    let columns = app.engine.session().live.columns.len();
    if columns == 0 {
        return false;
    }
    let column = view.column.min(columns - 1);
    let digit = match key {
        Key::Num0 => Some(0),
        Key::Num1 => Some(1),
        Key::Num2 => Some(2),
        Key::Num3 => Some(3),
        Key::Num4 => Some(4),
        Key::Num5 => Some(5),
        Key::Num6 => Some(6),
        Key::Num7 => Some(7),
        Key::Num8 => Some(8),
        _ => None,
    };
    match (key, digit) {
        (_, Some(0)) if !m.shift => {
            let _ = app.run("live.stop", json!({"column": column}));
        }
        (_, Some(n)) if n > 0 && m.shift => {
            let _ = app.run("live.scene", json!({"row": n - 1}));
        }
        (_, Some(n)) if n > 0 => {
            let _ = app.run("live.launch", json!({"column": column, "slot": n - 1}));
        }
        (Key::ArrowLeft, _) if !m.shift => app.session_view.column = column.saturating_sub(1),
        (Key::ArrowRight, _) if !m.shift => app.session_view.column = (column + 1).min(columns - 1),
        (Key::Escape, _) if !m.shift => {
            let _ = app.run("live.stop_all", json!({}));
        }
        _ => return false,
    }
    true
}

/// Loads the first audio file of `files` dropped at `pos` on a slot into it; returns the rest.
pub(crate) fn take_drop(app: &mut SoundApp, files: Vec<egui::DroppedFileHandle>, pos: Option<egui::Pos2>) -> Vec<egui::DroppedFileHandle> {
    let Some(&(_, column, row)) = pos.and_then(|p| app.session_view.drop_targets.iter().rev().find(|(r, _, _)| r.contains(p))) else {
        return files;
    };
    let mut rest = Vec::new();
    let mut claimed = false;
    for f in files {
        let path = f.path().to_string_lossy().into_owned();
        if claimed || !f.path().is_absolute() || !crate::sampler_window::is_audio_name(&path) {
            rest.push(f);
            continue;
        }
        claimed = true;
        if app.run("live.slot_from_file", json!({"column": column, "slot": row, "path": path})).is_ok() {
            app.ui.status = format!(
                "Loaded {} into Live {} slot {}",
                f.path().file_name().map_or(path.clone(), |n| n.to_string_lossy().into_owned()),
                column + 1,
                row + 1
            );
        }
    }
    rest
}

fn quantise_label(q: Quantise) -> String {
    match q.normalised() {
        Quantise::None => "None".into(),
        Quantise::Quarter => "1/4".into(),
        Quantise::Half => "1/2".into(),
        Quantise::Bar(1) => "1 Bar".into(),
        Quantise::Bar(n) => format!("{n} Bars"),
    }
}

pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    if !app.session_view.open {
        app.session_view.focused = false;
        return;
    }
    let states = states(app);
    let mut open = true;
    let t = Tokens::current();
    let now = ctx.input(|i| i.time);
    let shown = egui::Window::new(egui::RichText::new("Session").font(bold(13.0)))
        .id(egui::Id::new("session_view"))
        .open(&mut open)
        .default_size(vec2(5.0 * CELL.x + 80.0, 11.0 * CELL.y + 60.0))
        .resizable(true)
        .show(ctx, |ui| grid(app, ui, &states, now));
    if let Some(r) = &shown {
        let rect = r.response.rect;
        // Keys go to the Session window after a click in it, and back to the arrangement after
        // a click anywhere else.
        if let Some(p) = ctx.input(|i| i.pointer.any_pressed().then(|| i.pointer.interact_pos()).flatten()) {
            app.session_view.focused = rect.contains(p);
        }
        if app.session_view.focused {
            ui_outline(ctx, rect, t.accent);
        }
    }
    if !open {
        app.session_view.open = false;
        app.session_view.focused = false;
    }
    if states.iter().any(|c| c.playing.is_some() || c.queued.is_some()) {
        ctx.request_repaint_after(std::time::Duration::from_millis(60));
    }
    edit_dialog(app, ctx);
}

fn ui_outline(ctx: &egui::Context, rect: Rect, color: Color32) {
    ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("session_view_focus"))).rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, color.gamma_multiply(0.6)),
        StrokeKind::Outside,
    );
}

fn grid(app: &mut SoundApp, ui: &mut egui::Ui, states: &[ColumnState], now: f64) {
    let t = Tokens::current();
    let columns = app.engine.session().live.columns.len();
    if columns == 0 {
        ui.label(egui::RichText::new("This session has no Live columns.").font(regular(11.0)).color(t.text_dim));
        if ui.button("Set Up Live Columns").clicked() {
            let _ = app.run("live.setup", json!({}));
        }
        return;
    }
    app.session_view.column = app.session_view.column.min(columns - 1);
    ui.horizontal(|ui| {
        let current = app.engine.session().live.quantise;
        let mut chosen = current;
        egui::ComboBox::from_label("Launch Quantise").selected_text(quantise_label(current)).show_ui(ui, |ui| {
            for q in Quantise::ALL {
                ui.selectable_value(&mut chosen, q, quantise_label(q));
            }
        });
        if chosen != current {
            let _ = app.run("live.quantise", json!({"value": chosen.id()}));
        }
        if ui.button("Stop All").clicked() {
            let _ = app.run("live.stop_all", json!({}));
        }
    });
    ui.add_space(4.0);
    egui::Grid::new("session_grid").spacing(vec2(4.0, 4.0)).show(ui, |ui| {
        let s = app.engine.session();
        let names: Vec<String> = (0..columns)
            .map(|c| {
                s.live
                    .column_track(c, &s.tracks)
                    .and_then(|id| s.track(id))
                    .map_or_else(|| format!("Live {} (no track)", c + 1), |tr| tr.name.clone())
            })
            .collect();
        for (c, name) in names.iter().enumerate() {
            let selected = c == app.session_view.column;
            if ui.selectable_label(selected, egui::RichText::new(name).font(bold(11.0))).clicked() {
                app.session_view.column = c;
            }
        }
        ui.label(egui::RichText::new("Scene").font(bold(11.0)).color(t.text_dim));
        ui.end_row();
        for row in 0..LIVE_SLOTS {
            for (c, state) in (0..columns).map(|c| (c, states.get(c).copied().unwrap_or_default())) {
                slot(app, ui, c, row, state, now);
            }
            if ui
                .add_sized(vec2(60.0, CELL.y), egui::Button::new(format!("▶ {}", row + 1)))
                .on_hover_text("Launch this row in every column")
                .clicked()
            {
                let _ = app.run("live.scene", json!({"row": row}));
            }
            ui.end_row();
        }
        for c in 0..columns {
            if ui
                .add_sized(CELL, egui::Button::new(format!("■ Stop {}", c + 1)))
                .on_hover_text(format!("Stop Live {} at the next boundary", c + 1))
                .clicked()
            {
                app.session_view.column = c;
                let _ = app.run("live.stop", json!({"column": c}));
            }
        }
        ui.end_row();
    });
}

fn slot(app: &mut SoundApp, ui: &mut egui::Ui, column: usize, row: usize, state: ColumnState, now: f64) {
    let t = Tokens::current();
    let (rect, resp) = ui.allocate_exact_size(CELL, Sense::click());
    let s = app.engine.session();
    let filled = s.live.slot(column, row).map(|x| (x.name.clone(), x.color));
    let label = format!("Live {} slot {}", column + 1, row + 1);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
    app.session_view.drop_targets.push((rect, column, row));
    let playing = state.playing.filter(|(r, _)| *r == row);
    let queued = state.queued.is_some_and(|q| q == Some(row));
    let p = ui.painter_at(rect);
    let fill = match &filled {
        Some((_, [r, g, b])) => Color32::from_rgb(*r, *g, *b).gamma_multiply(if playing.is_some() { 1.0 } else { 0.65 }),
        None => t.panel_bg2,
    };
    // Queued: blinks until its boundary.
    let blink = queued && (now * 4.0).floor() as i64 % 2 == 0;
    p.rect_filled(rect, 3.0, if blink { fill.gamma_multiply(0.4) } else { fill });
    let over_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty() && i.pointer.latest_pos().is_some_and(|q| rect.contains(q)));
    let border = if over_files || queued || playing.is_some() { t.accent } else { t.border };
    p.rect_stroke(rect, 3.0, Stroke::new(if over_files { 2.0 } else { 1.0 }, border), StrokeKind::Inside);
    let icon = egui::pos2(rect.min.x + 12.0, rect.center().y);
    match (playing, &filled) {
        (Some((_, fraction)), _) => {
            // A ring around the play mark showing where the loop is.
            p.circle_stroke(icon, 7.0, Stroke::new(1.0, Color32::WHITE.gamma_multiply(0.35)));
            let f = fraction.unwrap_or(1.0) as f32;
            let n = 24;
            let pts: Vec<egui::Pos2> = (0..=n)
                .map(|i| {
                    let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * f * i as f32 / n as f32;
                    icon + 7.0 * vec2(a.cos(), a.sin())
                })
                .collect();
            p.add(egui::Shape::line(pts, Stroke::new(2.0, Color32::WHITE)));
            p.text(icon, egui::Align2::CENTER_CENTER, "▶", regular(8.0), Color32::WHITE);
        }
        (None, Some(_)) => {
            p.text(icon, egui::Align2::CENTER_CENTER, "▷", regular(10.0), t.text);
        }
        (None, None) => {}
    }
    if let Some((name, _)) = &filled {
        p.text(egui::pos2(rect.min.x + 24.0, rect.center().y), egui::Align2::LEFT_CENTER, name, regular(11.0), t.text);
    }
    if resp.clicked() {
        app.session_view.column = column;
        if filled.is_some() {
            let _ = app.run("live.launch", json!({"column": column, "slot": row}));
        }
    }
    resp.context_menu(|ui| {
        app.session_view.column = column;
        if ui.button("Add Selection Here").clicked() {
            let _ = app.run("live.slot_from_selection", json!({"column": column, "slot": row}));
            ui.close();
        }
        let can_pick = app.services.pick_open.is_some();
        let load = ui.add_enabled(can_pick, egui::Button::new("Load File..."));
        let load = if can_pick { load } else { load.on_disabled_hover_text("Drop an audio file on the slot") };
        if load.clicked() {
            ui.close();
            if let Some(path) = app.services.pick_open.as_ref().and_then(|pick| pick("live.slot_from_file", crate::sampler_window::AUDIO_EXTS)) {
                let _ = app.run("live.slot_from_file", json!({"column": column, "slot": row, "path": path}));
            }
        }
        if ui.add_enabled(filled.is_some(), egui::Button::new("Edit...")).clicked() {
            app.session_view.edit = edit_of(app, column, row);
            ui.close();
        }
        if ui.add_enabled(filled.is_some(), egui::Button::new("Clear")).clicked() {
            let _ = app.run("live.slot_clear", json!({"column": column, "slot": row}));
            ui.close();
        }
    });
}

fn edit_of(app: &SoundApp, column: usize, row: usize) -> Option<SlotEdit> {
    let x = app.engine.session().live.slot(column, row)?;
    let bars = x.warp.as_ref().map(|_| (x.length_beats / 4.0).round().max(1.0) as u32);
    let e =
        SlotEdit { column, row, name: x.name.clone(), color: x.color, mode: x.mode, length_bars: bars, gain_db: x.gain_db, first: Box::new(None) };
    Some(SlotEdit { first: Box::new(Some(e.clone())), ..e })
}

/// The `live.slot_set` parameters for what the dialog changed (`None`: nothing).
fn changes(e: &SlotEdit) -> Option<Value> {
    let first = e.first.as_ref().as_ref()?;
    let mut p = json!({"column": e.column, "slot": e.row});
    let o = p.as_object_mut()?;
    if e.name != first.name {
        o.insert("name".into(), json!(e.name));
    }
    if e.color != first.color {
        o.insert("color".into(), json!(e.color));
    }
    if e.mode != first.mode {
        o.insert("mode".into(), json!(e.mode.id()));
    }
    if e.length_bars != first.length_bars
        && let Some(b) = e.length_bars
    {
        o.insert("length_bars".into(), json!(b));
    }
    if e.gain_db != first.gain_db {
        o.insert("gain_db".into(), json!(e.gain_db));
    }
    (o.len() > 2).then_some(p)
}

fn edit_dialog(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(mut e) = app.session_view.edit.take() else { return };
    let mut keep = true;
    let mut apply = false;
    egui::Window::new(egui::RichText::new(format!("Edit Live {} Slot {}", e.column + 1, e.row + 1)).font(bold(13.0)))
        .id(egui::Id::new("session_slot_edit"))
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            egui::Grid::new("slot_edit_fields").num_columns(2).show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut e.name);
                ui.end_row();
                ui.label("Colour");
                ui.color_edit_button_srgb(&mut e.color);
                ui.end_row();
                ui.label("Mode");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(e.length_bars.is_some(), |ui| ui.radio_value(&mut e.mode, SlotMode::Loop, "Loop"));
                    ui.radio_value(&mut e.mode, SlotMode::OneShot, "One-shot");
                });
                ui.end_row();
                if let Some(bars) = &mut e.length_bars {
                    ui.label("Length (bars)");
                    ui.add(egui::DragValue::new(bars).range(1..=256));
                    ui.end_row();
                }
                ui.label("Gain (dB)");
                ui.add(egui::Slider::new(&mut e.gain_db, SLOT_GAIN_DB.0..=SLOT_GAIN_DB.1));
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    apply = true;
                    keep = false;
                }
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
        });
    if apply && let Some(p) = changes(&e) {
        let _ = app.run("live.slot_set", p);
    }
    if keep {
        app.session_view.edit = Some(e);
    }
}
