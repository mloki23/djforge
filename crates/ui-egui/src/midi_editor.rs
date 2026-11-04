//! The MIDI Editor: a piano roll with a velocity lane, docked under the Edit window.
//!
//! The rows of the pitches in the key's scale are shaded: the key the set plays at the clip's
//! start (`dj.key_at`), or one picked in the header for the view only (never a session edit).
//! A drum clip (on the built-in Drum Synth, or all GM drum notes) can show as a step grid
//! instead: a row per kit piece, 16 steps a bar, a click adds or removes a 16th and Shift+click
//! accents it, each one undo step.

use crate::SoundApp;
use crate::theme::{Tokens, bold, regular};
use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_midi::{Key, Sequence, gm_drum_name, note_name};
use soundcraft_model::{ClipContent, ClipId, Tool, TrackId};
use soundcraft_time::TICKS_PER_QUARTER;

const KEY_W: f32 = 48.0;
/// A header row's height, the clip operations' width and the view controls' width.
const HEADER_H: f32 = 22.0;
const OPS_W: f32 = 230.0;
const VIEW_W: f32 = 340.0;
/// Narrower than this, the header takes two rows.
const ONE_ROW_HEADER: f32 = 1000.0;
const ROW_H: f32 = 9.0;
const VEL_H: f32 = 64.0;
/// A drum grid step: a 16th note.
const STEP: i64 = TICKS_PER_QUARTER / 4;
/// A 4/4 bar.
const BAR: i64 = TICKS_PER_QUARTER * 4;
/// The drum grid's kit rows, top down: kick, clap, snare, closed hat, open hat and two percs.
const KIT_ROWS: [u8; 7] = [36, 39, 38, 42, 46, 37, 75];
/// Velocities of a grid step and an accented one, and the least that reads as accented (the
/// generated patterns accent kicks at 120).
const NORMAL: u8 = 96;
const ACCENT: u8 = 127;
const ACCENTED: u8 = 112;
/// Width of the drum grid's row names, and the narrowest step that still shows two bars a page.
const NAME_W: f32 = 104.0;
const MIN_STEP_W: f32 = 18.0;
/// The built-in Drum Synth's registry id.
const DRUM_SYNTH: &str = "drum_synth";

#[derive(Debug, Clone, Default)]
pub struct MidiEditorState {
    pub selected: Vec<usize>,
    pub drag: Option<NoteDrag>,
    pub scroll: f32,
    pub clip: Option<ClipId>,
    /// Rubber-band selection start (screen position).
    pub band: Option<egui::Pos2>,
    /// The key whose scale the rows show in place of the set's: a view setting, never a session
    /// edit. Cleared when another clip is edited.
    pub key_override: Option<Key>,
    /// The key whose scale the rows were last shaded in (kept for tests to read; nothing else
    /// uses it).
    pub scale: Option<Key>,
    /// Drum clips show as a step grid.
    pub drum_grid: bool,
    /// The drum grid's page.
    pub grid_page: usize,
}

#[derive(Debug, Clone)]
pub struct NoteDrag {
    pub index: usize,
    pub resize: bool,
    pub start: egui::Pos2,
    pub dx_ticks: i64,
    pub dy_semi: i64,
}

/// The MIDI clip being edited: the first selected MIDI clip, else a MIDI clip under the insertion
/// point on a selected track.
pub fn target_clip(app: &SoundApp) -> Option<ClipId> {
    let s = app.engine.session();
    for id in &s.edit.selected_clips {
        if let Some((_, c)) = s.find_clip(*id)
            && matches!(c.content, ClipContent::Midi { .. })
        {
            return Some(*id);
        }
    }
    let at = s.edit.selection.start;
    s.tracks
        .iter()
        .filter(|t| s.edit.selected_tracks.contains(&t.id) && t.kind.is_midi())
        .flat_map(|t| t.clips().iter())
        .find(|c| c.range().contains(at) || c.range().overlaps(&s.edit.selection))
        .or_else(|| s.tracks.iter().filter(|t| s.edit.selected_tracks.contains(&t.id) && t.kind.is_midi()).flat_map(|t| t.clips().iter()).next())
        .map(|c| c.id)
}

fn is_black(p: i32) -> bool {
    matches!(p.rem_euclid(12), 1 | 3 | 6 | 8 | 10)
}

pub fn show(app: &mut SoundApp, ui: &mut Ui) {
    // Painter-only contents do not advance the layout. Claim the resized
    // space, including the empty state, so the dock keeps its chosen height.
    ui.take_available_space();
    let t = Tokens::current();
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, t.panel_bg);
    // On a narrow panel the view controls (key, scale, grid) take a second header row, so the
    // clip's name keeps its room and nothing runs off the edge.
    let narrow = full.width() < ONE_ROW_HEADER;
    let header = Rect::from_min_size(full.min, vec2(full.width(), if narrow { 2.0 * HEADER_H } else { HEADER_H }));
    ui.painter().rect_filled(header, 0.0, t.panel_bg2);
    let row1 = Rect::from_min_size(full.min, vec2(full.width(), HEADER_H));
    ui.painter().text(pos2(row1.min.x + 8.0, row1.center().y), Align2::LEFT_CENTER, "MIDI EDITOR", bold(11.5), t.header_text);
    let Some(cid) = target_clip(app) else {
        let track = {
            let s = app.engine.session();
            s.tracks.iter().find(|t| t.kind.is_midi() && s.edit.selected_tracks.contains(&t.id)).map(|t| (t.id, t.name.clone()))
        };
        let msg = match &track {
            Some((_, name)) => format!("Click to add a MIDI clip to {name} at the insertion point."),
            None => "Select a MIDI clip (or a MIDI track) to edit its notes.".into(),
        };
        ui.painter().text(full.center(), Align2::CENTER_CENTER, msg, regular(13.0), t.text_dim);
        let body = Rect::from_min_max(pos2(full.min.x, header.max.y), full.max);
        if let Some((id, _)) = track
            && ui.interact(body, ui.id().with("new_clip"), Sense::click()).clicked()
        {
            let sel = app.engine.session().edit.selection;
            if let Ok(v) = app.run("midi.new_clip", json!({"track": id.0, "start": sel.start, "end": sel.end})) {
                let _ = app.run("edit.select", json!({"clips": [v["clip"]]}));
            }
        }
        return;
    };
    let Some((track, clip)) = app.engine.session().find_clip(cid).map(|(t, c)| (t, c.clone())) else { return };
    let ClipContent::Midi { sequence } = &clip.content else { return };
    let track_color = app.engine.session().track(track).map_or([120, 120, 200], |t| t.color);
    if app.midi.clip != Some(cid) {
        app.midi.key_override = None;
        app.midi.grid_page = 0;
    }
    let drums = is_drum_clip(app, track, sequence);
    let set_key = app.engine.execute("dj.key_at", &json!({"at": clip.start})).ok().and_then(|v| v["key"].as_str().and_then(|k| Key::parse(k).ok()));
    let key = app.midi.key_override.or(set_key);
    app.midi.scale = key;
    // Header toolbars: whole-clip MIDI operations at the right of the first row; the key, scale
    // and drum grid beside them, or along the second row on a narrow panel.
    let ops = Rect::from_min_max(pos2(row1.max.x - OPS_W - 6.0, row1.min.y + 1.0), pos2(row1.max.x - 6.0, row1.max.y - 1.0));
    let view = if narrow {
        Rect::from_min_max(pos2(header.min.x + 8.0, row1.max.y + 1.0), pos2(header.max.x - 6.0, header.max.y - 1.0))
    } else {
        Rect::from_min_max(pos2(ops.min.x - VIEW_W, row1.min.y + 1.0), pos2(ops.min.x - 4.0, row1.max.y - 1.0))
    };
    let info_end = if narrow { ops.min.x } else { view.min.x } - 6.0;
    ui.painter().with_clip_rect(Rect::from_min_max(row1.min, pos2(info_end, row1.max.y))).text(
        pos2(row1.min.x + 110.0, row1.center().y),
        Align2::LEFT_CENTER,
        format!(
            "{} · {} notes · {} selected · Pencil: add · drag: move/select · arrows: move · Delete: remove",
            clip.name,
            sequence.notes.len(),
            app.midi.selected.len()
        ),
        regular(11.0),
        t.text_dim,
    );
    let grid_ticks = match app.engine.session().edit.grid {
        soundcraft_time::GridValue::Note { value, dotted, triplet } => {
            let mut g = value.ticks();
            if dotted {
                g = g * 3 / 2;
            }
            if triplet {
                g = g * 2 / 3;
            }
            g.max(1)
        }
        _ => TICKS_PER_QUARTER / 4,
    };
    let mut vb = ui.new_child(egui::UiBuilder::new().max_rect(view).layout(egui::Layout::right_to_left(egui::Align::Center)));
    vb.set_clip_rect(view);
    vb.spacing_mut().item_spacing.x = 4.0;
    key_picker(app, &mut vb, set_key);
    vb.label(
        egui::RichText::new(key.map_or_else(|| "Key: none".to_string(), |k| format!("Key: {} ({})", k.name(), k.camelot()))).font(regular(11.0)),
    );
    if drums {
        vb.separator();
        vb.toggle_value(&mut app.midi.drum_grid, "Grid").on_hover_text("Show the drums as a 16-step grid");
    }
    let mut tb = ui.new_child(egui::UiBuilder::new().max_rect(ops).layout(egui::Layout::right_to_left(egui::Align::Center)));
    tb.spacing_mut().item_spacing.x = 4.0;
    let mut op: Option<(&str, serde_json::Value)> = None;
    if tb.small_button("Legato").clicked() {
        op = Some(("event.change_duration", json!({"legato": 0})));
    }
    if tb.small_button("Vel −10").clicked() {
        op = Some(("event.change_velocity", json!({"add": -10})));
    }
    if tb.small_button("Vel +10").clicked() {
        op = Some(("event.change_velocity", json!({"add": 10})));
    }
    if tb.small_button("Quantize").on_hover_text("Quantize to the grid").clicked() {
        op = Some(("event.quantize", json!({"grid": grid_ticks, "strength": 100})));
    }
    if let Some((id, mut p)) = op {
        p["clips"] = json!([cid.0]);
        let _ = app.run(id, p);
    }
    let s = app.engine.session_arc();
    let sr = s.sample_rate;
    let base_tick = s.tempo.samples_to_ticks(clip.start, sr);
    let len_ticks = (s.tempo.samples_to_ticks(clip.end(), sr) - base_tick).max(1);
    let base = egui::Color32::from_rgb(track_color[0], track_color[1], track_color[2]);
    if drums && app.midi.drum_grid {
        app.midi.clip = Some(cid);
        // The grid counts the song's bars: the clip's start is this far into its bar.
        let bar = s.tempo.bar_beat_at_tick(base_tick).bar;
        let place = GridPlace { first_bar: bar, offset: base_tick - s.tempo.bar_start_tick(bar), len: len_ticks };
        drum_grid(app, ui, cid, sequence, place, base, Rect::from_min_max(pos2(full.min.x, header.max.y), full.max));
        return;
    }
    let roll = Rect::from_min_max(pos2(full.min.x + KEY_W, header.max.y), pos2(full.max.x - 4.0, full.max.y - VEL_H - 4.0));
    let keys = Rect::from_min_max(pos2(full.min.x, roll.min.y), pos2(roll.min.x, roll.max.y));
    let vel = Rect::from_min_max(pos2(roll.min.x, roll.max.y + 4.0), pos2(roll.max.x, full.max.y - 2.0));
    // Vertical range: centre on the notes the first time.
    let rows = (roll.height() / ROW_H).floor() as i32;
    if app.midi.clip != Some(cid) {
        let hi = sequence.notes.iter().map(|n| i32::from(n.pitch)).max().unwrap_or(72);
        let lo = sequence.notes.iter().map(|n| i32::from(n.pitch)).min().unwrap_or(48);
        app.midi.scroll = (127 - (hi + lo) / 2 - rows / 2) as f32 * ROW_H;
        app.midi.clip = Some(cid);
    }
    if ui.rect_contains_pointer(roll) {
        app.midi.scroll -= ui.input(|i| i.smooth_scroll_delta.y);
    }
    app.midi.scroll = app.midi.scroll.clamp(0.0, (128.0 * ROW_H - roll.height()).max(0.0));
    let scroll = app.midi.scroll;
    let px_per_tick = roll.width() / len_ticks as f32;
    let x_of = |tick: i64| roll.min.x + tick as f32 * px_per_tick;
    let y_of = |pitch: i32| roll.min.y + (127 - pitch) as f32 * ROW_H - scroll;
    let pitch_at = |y: f32| 127 - ((y - roll.min.y + scroll) / ROW_H).floor() as i32;
    let painter = ui.painter().with_clip_rect(roll.union(keys));
    // Background rows and keys.
    let top = pitch_at(roll.min.y);
    for r in 0..=rows + 1 {
        let pitch = top - r;
        let y = y_of(pitch);
        let row = Rect::from_min_size(pos2(roll.min.x, y), vec2(roll.width(), ROW_H));
        painter.rect_filled(row, 0.0, row_color(pitch, key));
        let key = Rect::from_min_size(pos2(keys.min.x, y), vec2(KEY_W - 2.0, ROW_H - 1.0));
        painter.rect_filled(key, 1.0, if is_black(pitch) { Color32::from_rgb(20, 20, 20) } else { Color32::from_rgb(225, 225, 225) });
        if pitch.rem_euclid(12) == 0 && (0..=127).contains(&pitch) {
            painter.text(
                pos2(key.max.x - 3.0, key.center().y),
                Align2::RIGHT_CENTER,
                soundcraft_midi::note_name(pitch as u8),
                regular(8.0),
                Color32::from_rgb(40, 40, 40),
            );
            painter.line_segment([pos2(roll.min.x, y + ROW_H), pos2(roll.max.x, y + ROW_H)], Stroke::new(1.0, Color32::from_rgb(64, 64, 68)));
        }
    }
    // Grid.
    let mut tick = 0;
    while tick <= len_ticks {
        let bar = tick % (TICKS_PER_QUARTER * 4) == 0;
        let x = x_of(tick);
        if grid_ticks as f32 * px_per_tick > 4.0 || bar {
            painter.line_segment(
                [pos2(x, roll.min.y), pos2(x, roll.max.y)],
                Stroke::new(1.0, if bar { Color32::from_rgb(80, 80, 86) } else { Color32::from_rgb(50, 50, 54) }),
            );
        }
        tick += grid_ticks;
    }
    // Notes.
    let drag = app.midi.drag.clone();
    for (i, n) in sequence.notes.iter().enumerate() {
        let (mut st, mut ln, mut p) = (n.start, n.length, i32::from(n.pitch));
        if let Some(d) = &drag
            && (d.index == i || (!d.resize && app.midi.selected.contains(&i) && app.midi.selected.contains(&d.index)))
        {
            if d.resize {
                if d.index == i {
                    ln = (ln + d.dx_ticks).max(grid_ticks.min(ln).max(1));
                }
            } else {
                st = (st + d.dx_ticks).max(0);
                p = (p + d.dy_semi as i32).clamp(0, 127);
            }
        }
        let r = Rect::from_min_max(pos2(x_of(st), y_of(p) + 1.0), pos2(x_of(st + ln).max(x_of(st) + 3.0), y_of(p) + ROW_H - 1.0));
        let sel = app.midi.selected.contains(&i);
        let k = 0.45 + 0.55 * f32::from(n.velocity) / 127.0;
        let fill = if sel {
            Color32::WHITE
        } else {
            Color32::from_rgb((f32::from(base.r()) * k) as u8, (f32::from(base.g()) * k) as u8, (f32::from(base.b()) * k) as u8)
        };
        painter.rect(r, 2.0, fill, Stroke::new(1.0, Color32::BLACK), StrokeKind::Inside);
    }
    // Interaction.
    let resp = ui.interact(roll, ui.id().with(("roll", cid.0)), Sense::click_and_drag());
    let hit = |pos: egui::Pos2| -> Option<(usize, bool)> {
        sequence.notes.iter().enumerate().rev().find_map(|(i, n)| {
            let r = Rect::from_min_max(
                pos2(x_of(n.start), y_of(i32::from(n.pitch))),
                pos2(x_of(n.start + n.length).max(x_of(n.start) + 3.0), y_of(i32::from(n.pitch)) + ROW_H),
            );
            r.contains(pos).then_some((i, pos.x > r.max.x - 5.0))
        })
    };
    let snap = |tk: f32| -> i64 { ((tk / grid_ticks as f32).round() as i64) * grid_ticks };
    if resp.drag_started()
        && let Some(p) = resp.interact_pointer_pos()
    {
        if let Some((i, edge)) = hit(p) {
            if !app.midi.selected.contains(&i) {
                app.midi.selected = vec![i];
            }
            app.midi.drag = Some(NoteDrag { index: i, resize: edge, start: p, dx_ticks: 0, dy_semi: 0 });
        } else {
            app.midi.band = Some(p);
        }
    }
    // Rubber-band selection on empty space.
    if let (Some(a), Some(b)) = (app.midi.band, ui.ctx().pointer_latest_pos()) {
        let band = Rect::from_two_pos(a, b);
        ui.painter().rect(
            band,
            0.0,
            Color32::from_rgba_unmultiplied(120, 170, 255, 40),
            Stroke::new(1.0, Color32::from_rgb(120, 170, 255)),
            StrokeKind::Inside,
        );
        if resp.drag_stopped() {
            app.midi.selected = sequence
                .notes
                .iter()
                .enumerate()
                .filter(|(_, n)| {
                    let r = Rect::from_min_max(
                        pos2(x_of(n.start), y_of(i32::from(n.pitch))),
                        pos2(x_of(n.start + n.length), y_of(i32::from(n.pitch)) + ROW_H),
                    );
                    r.intersects(band)
                })
                .map(|(i, _)| i)
                .collect();
            app.midi.band = None;
        }
    }
    if resp.dragged()
        && let (Some(d), Some(p)) = (&mut app.midi.drag, resp.interact_pointer_pos())
    {
        d.dx_ticks = snap((p.x - d.start.x) / px_per_tick);
        d.dy_semi = -((p.y - d.start.y) / ROW_H).round() as i64;
    }
    if resp.drag_stopped()
        && let Some(d) = app.midi.drag.take()
    {
        if d.resize {
            if let Some(n) = sequence.notes.get(d.index) {
                let _ = app.run("midi.note_edit", json!({"clip": cid.0, "index": d.index, "length_ticks": (n.length + d.dx_ticks).max(1)}));
            }
        } else if d.dx_ticks != 0 || d.dy_semi != 0 {
            let _ = app.run("midi.notes_move", json!({"clip": cid.0, "indices": app.midi.selected, "ticks": d.dx_ticks, "semitones": d.dy_semi}));
            app.midi.selected.clear();
        }
    }
    if resp.clicked()
        && let Some(p) = resp.interact_pointer_pos()
    {
        match hit(p) {
            Some((i, _)) => {
                if ui.input(|x| x.modifiers.shift) {
                    if !app.midi.selected.contains(&i) {
                        app.midi.selected.push(i);
                    }
                } else {
                    app.midi.selected = vec![i];
                }
                // Audition on the instrument would go here (needs a live MIDI input path).
            }
            None if s.edit.tool == Tool::Pencil => {
                let pitch = pitch_at(p.y).clamp(0, 127);
                let start = (((p.x - roll.min.x) / px_per_tick) as i64 / grid_ticks) * grid_ticks;
                let _ = app.run(
                    "midi.note_add",
                    json!({"clip": cid.0, "pitch": pitch, "start_ticks": start.max(0), "length_ticks": grid_ticks, "velocity": 100}),
                );
                app.midi.selected.clear();
            }
            None => app.midi.selected.clear(),
        }
    }
    // Keyboard: Cmd+A select all; arrows transpose (Shift = octave) and move by the grid.
    if ui.rect_contains_pointer(roll) && !ui.ctx().egui_wants_keyboard_input() {
        let (all, up, down, left, right, shift) = ui.input(|i| {
            let arrows = !i.modifiers.alt && !i.modifiers.command && !i.modifiers.ctrl;
            (
                i.modifiers.command && i.key_pressed(egui::Key::A),
                arrows && i.key_pressed(egui::Key::ArrowUp),
                arrows && i.key_pressed(egui::Key::ArrowDown),
                arrows && !i.modifiers.shift && i.key_pressed(egui::Key::ArrowLeft),
                arrows && !i.modifiers.shift && i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.shift,
            )
        });
        if all {
            app.midi.selected = (0..sequence.notes.len()).collect();
        }
        let semi = if up {
            1
        } else if down {
            -1
        } else {
            0
        } * if shift { 12 } else { 1 };
        let ticks = if right {
            grid_ticks
        } else if left {
            -grid_ticks
        } else {
            0
        };
        if (semi != 0 || ticks != 0) && !app.midi.selected.is_empty() {
            let _ = app.run("midi.notes_move", json!({"clip": cid.0, "indices": app.midi.selected, "ticks": ticks, "semitones": semi}));
        }
    }
    if !app.midi.selected.is_empty()
        && !ui.ctx().egui_wants_keyboard_input()
        && ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace))
    {
        let _ = app.run("midi.note_delete", json!({"clip": cid.0, "indices": app.midi.selected}));
        app.midi.selected.clear();
    }
    // Velocity lane.
    ui.painter().rect_filled(vel, 0.0, Color32::from_rgb(28, 28, 30));
    ui.painter().text(pos2(full.min.x + 6.0, vel.center().y), Align2::LEFT_CENTER, "Velocity", regular(10.0), t.text_dim);
    let vresp = ui.interact(vel, ui.id().with(("vel", cid.0)), Sense::click_and_drag());
    for (i, n) in sequence.notes.iter().enumerate() {
        let x = x_of(n.start);
        let h = (vel.height() - 4.0) * f32::from(n.velocity) / 127.0;
        let col = if app.midi.selected.contains(&i) { Color32::WHITE } else { base };
        ui.painter().line_segment([pos2(x + 1.0, vel.max.y - 2.0), pos2(x + 1.0, vel.max.y - 2.0 - h)], Stroke::new(3.0, col));
        ui.painter().circle_filled(pos2(x + 1.0, vel.max.y - 2.0 - h), 2.5, col);
    }
    if (vresp.clicked() || vresp.dragged())
        && let Some(p) = vresp.interact_pointer_pos()
        && let Some((i, n)) = sequence.notes.iter().enumerate().min_by_key(|(_, n)| ((x_of(n.start) - p.x).abs() * 10.0) as i64)
        && (x_of(n.start) - p.x).abs() < 8.0
    {
        let v = (((vel.max.y - 2.0 - p.y) / (vel.height() - 4.0)) * 127.0).round().clamp(1.0, 127.0) as i64;
        let _ = app.engine.execute_merged("midi.note_edit", &json!({"clip": cid.0, "index": i, "velocity": v}), &format!("vel:{}", cid.0));
    }
}

/// A piano roll row's colour: shaded when the pitch is in the key's scale, more so on the tonic.
fn row_color(pitch: i32, key: Option<Key>) -> Color32 {
    let degree = key.filter(|k| (0..=127).contains(&pitch) && k.contains(pitch as u8)).map(|k| i32::from(k.tonic) == pitch.rem_euclid(12));
    match (is_black(pitch), degree) {
        (false, None) => Color32::from_rgb(40, 40, 42),
        (true, None) => Color32::from_rgb(30, 30, 32),
        (false, Some(false)) => Color32::from_rgb(44, 50, 60),
        (true, Some(false)) => Color32::from_rgb(34, 40, 50),
        (false, Some(true)) => Color32::from_rgb(50, 60, 76),
        (true, Some(true)) => Color32::from_rgb(40, 50, 66),
    }
}

/// The header's key menu: the set's key, or any other for the view only (the session is
/// untouched).
fn key_picker(app: &mut SoundApp, ui: &mut Ui, set_key: Option<Key>) {
    let mut chosen = app.midi.key_override;
    let set = set_key.map_or_else(|| "Set's key (none)".to_string(), |k| format!("Set's key ({})", k.name()));
    egui::ComboBox::from_label("Scale")
        .width(110.0)
        .selected_text(chosen.map_or_else(|| set.clone(), |k| format!("{} ({})", k.name(), k.camelot())))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut chosen, None, set);
            for minor in [true, false] {
                for tonic in 0..12 {
                    let k = Key::new(tonic, minor);
                    ui.selectable_value(&mut chosen, Some(k), format!("{} ({})", k.name(), k.camelot()));
                }
            }
        })
        .response
        .on_hover_text("The scale the rows show (view only)");
    app.midi.key_override = chosen;
}

/// Whether a clip reads as drums: its track plays the built-in Drum Synth, or every note is a
/// GM drum on a track that doesn't play another built-in instrument (a bass line's notes are
/// often all in the GM drum range too).
pub fn is_drum_clip(app: &SoundApp, track: TrackId, sequence: &Sequence) -> bool {
    let instrument = app.engine.session().track(track).and_then(|t| t.instrument.as_ref()).map(|i| i.plugin.as_str());
    match instrument {
        Some(DRUM_SYNTH) => true,
        Some(id) if soundcraft_dsp::plugin_info(id).is_some() => false,
        _ => !sequence.notes.is_empty() && sequence.notes.iter().all(|n| gm_drum_name(n.pitch).is_some()),
    }
}

/// The drum grid's rows: the kit, then any other pitch the clip plays, low to high.
pub fn grid_rows(sequence: &Sequence) -> Vec<u8> {
    let mut extra: Vec<u8> = sequence.notes.iter().map(|n| n.pitch).filter(|p| !KIT_ROWS.contains(p)).collect();
    extra.sort_unstable();
    extra.dedup();
    KIT_ROWS.iter().copied().chain(extra).collect()
}

/// What a drum grid row is called: its GM drum name, else the note.
fn row_name(pitch: u8) -> String {
    gm_drum_name(pitch).map_or_else(|| note_name(pitch), str::to_string)
}

/// Where a clip sits on the song's bars, for the drum grid.
#[derive(Debug, Clone, Copy)]
struct GridPlace {
    /// The song bar the clip starts in.
    first_bar: i64,
    /// Ticks from that bar's start to the clip's.
    offset: i64,
    /// The clip's length in ticks.
    len: i64,
}

/// The notes of `pitch` a step at clip tick `tick` holds: those starting within half a step of
/// it, so a note played a little early or late still shows on its step.
fn step_hits(sequence: &Sequence, pitch: u8, tick: i64) -> Vec<usize> {
    let (from, to) = (tick - STEP / 2, tick + STEP / 2);
    sequence.notes.iter().enumerate().filter(|(_, n)| n.pitch == pitch && n.start >= from && n.start < to).map(|(i, _)| i).collect()
}

/// The drum grid: a row per kit piece and 16 steps to each of the song's bars, a page of bars at
/// a time. A click adds or removes the step's 16th (`midi.note_add` / `midi.note_delete`);
/// Shift+click accents every note on the step, or takes the accent off (`midi.note_edit`). Each
/// click is one undo step.
fn drum_grid(app: &mut SoundApp, ui: &mut Ui, cid: ClipId, sequence: &Sequence, place: GridPlace, color: Color32, area: Rect) {
    let t = Tokens::current();
    let end = place.offset.saturating_add(place.len);
    let bars = usize::try_from(u64::try_from(end).unwrap_or(0).div_ceil(BAR as u64)).unwrap_or(1).max(1);
    // Two bars a page when 32 steps fit comfortably, else one.
    let page_bars = if bars > 1 && (area.width() - NAME_W - 8.0) / 32.0 >= MIN_STEP_W { 2 } else { 1 };
    let pages = bars.div_ceil(page_bars);
    app.midi.grid_page = app.midi.grid_page.min(pages - 1);
    let first = app.midi.grid_page * page_bars;
    let last = (first + page_bars).min(bars);
    let song_bar = |i: usize| place.first_bar.saturating_add(i64::try_from(i).unwrap_or(0));
    let strip = Rect::from_min_max(area.min, pos2(area.max.x, area.min.y + 24.0));
    let mut nav =
        ui.new_child(egui::UiBuilder::new().max_rect(strip.shrink2(vec2(6.0, 2.0))).layout(egui::Layout::left_to_right(egui::Align::Center)));
    if pages > 1 {
        if nav.add_enabled(app.midi.grid_page > 0, egui::Button::new("◀")).on_hover_text("Earlier bars").clicked() {
            app.midi.grid_page -= 1;
        }
        nav.label(format!("Bars {}–{} of {}–{}", song_bar(first), song_bar(last - 1), song_bar(0), song_bar(bars - 1)));
        if nav.add_enabled(app.midi.grid_page + 1 < pages, egui::Button::new("▶")).on_hover_text("Later bars").clicked() {
            app.midi.grid_page += 1;
        }
    } else if bars == 1 {
        nav.label(format!("Bar {}", song_bar(0)));
    } else {
        nav.label(format!("Bars {}–{}", song_bar(0), song_bar(bars - 1)));
    }
    nav.label(egui::RichText::new("Click: add or remove a 16th · Shift+click: accent").font(regular(10.5)).color(t.text_dim));
    let rows = grid_rows(sequence);
    let grid = Rect::from_min_max(pos2(area.min.x + NAME_W, strip.max.y + 2.0), pos2(area.max.x - 8.0, area.max.y - 4.0));
    let row_h = (grid.height() / rows.len().max(1) as f32).min(22.0);
    let step_w = grid.width() / (page_bars * 16) as f32;
    let painter = ui.painter().with_clip_rect(area);
    let mut clicked: Option<(u8, i64, Vec<usize>)> = None;
    for (r, &pitch) in rows.iter().enumerate() {
        let y = grid.min.y + r as f32 * row_h;
        let name = row_name(pitch);
        painter.text(pos2(area.min.x + 6.0, y + row_h / 2.0), Align2::LEFT_CENTER, &name, regular(10.5), t.text);
        for s in 0..(last - first) * 16 {
            let step = first * 16 + s;
            // The step's tick in the clip (song bars count from the bar the clip starts in).
            let tick = i64::try_from(step).unwrap_or(i64::MAX).saturating_mul(STEP) - place.offset;
            // Steps before the clip (its first bar's lead-in) or past its end aren't the clip's.
            if tick + STEP / 2 <= 0 || tick >= place.len {
                continue;
            }
            let cell = Rect::from_min_size(pos2(grid.min.x + s as f32 * step_w, y), vec2(step_w, row_h)).shrink(1.0);
            let hits = step_hits(sequence, pitch, tick);
            let velocity = hits.iter().filter_map(|&i| sequence.notes.get(i)).map(|n| n.velocity).max();
            let fill = match velocity {
                Some(v) if v >= ACCENTED => color,
                Some(v) => color.gamma_multiply(0.35 + 0.4 * f32::from(v) / 127.0),
                // Beats alternate, so the bar reads in fours.
                None if (s / 4) % 2 == 0 => Color32::from_rgb(46, 46, 50),
                None => Color32::from_rgb(36, 36, 40),
            };
            painter.rect_filled(cell, 2.0, fill);
            if velocity.is_some_and(|v| v >= ACCENTED) {
                painter.rect_stroke(cell, 2.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
            }
            let label = format!("{name} bar {} step {}", song_bar(step / 16), step % 16 + 1);
            let resp = ui.interact(cell, ui.id().with(("drum_cell", cid.0, pitch, step)), Sense::click());
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, velocity.is_some(), &label));
            if resp.clicked() {
                clicked = Some((pitch, tick, hits));
            }
        }
    }
    let Some((pitch, tick, hits)) = clicked else { return };
    // The click's own modifiers count too: Shift may be up again by the end of the frame.
    let shift =
        ui.input(|i| i.modifiers.shift || i.events.iter().any(|e| matches!(e, egui::Event::PointerButton { modifiers, .. } if modifiers.shift)));
    app.midi.selected.clear();
    if hits.is_empty() {
        let velocity = if shift { ACCENT } else { NORMAL };
        let _ =
            app.run("midi.note_add", json!({"clip": cid.0, "pitch": pitch, "start_ticks": tick.max(0), "length_ticks": STEP, "velocity": velocity}));
    } else if !shift {
        let _ = app.run("midi.note_delete", json!({"clip": cid.0, "indices": hits}));
    } else {
        // Accented (any note at ACCENTED or more): every note goes back to NORMAL; else every
        // note goes to ACCENT. Several notes on a step are one undo step.
        let accented = hits.iter().filter_map(|&i| sequence.notes.get(i)).any(|n| n.velocity >= ACCENTED);
        let velocity = if accented { NORMAL } else { ACCENT };
        let key = format!("drum_accent:{}:{pitch}:{tick}:{}", cid.0, app.engine.revision);
        app.engine.end_merge();
        // Each edit re-sorts the notes, so the indices are found again after every one: the next
        // note on the step still off the velocity. At most one edit per note found at the click.
        for _ in 0..hits.len() {
            let next = match app.engine.session().find_clip(cid).map(|(_, c)| &c.content) {
                Some(ClipContent::Midi { sequence }) => {
                    step_hits(sequence, pitch, tick).into_iter().find(|&i| sequence.notes.get(i).is_some_and(|n| n.velocity != velocity))
                }
                _ => None,
            };
            let Some(i) = next else { break };
            if let Err(e) = app.engine.execute_merged("midi.note_edit", &json!({"clip": cid.0, "index": i, "velocity": velocity}), &key) {
                app.ui.status = e.to_string();
                break;
            }
        }
        app.engine.end_merge();
    }
}
