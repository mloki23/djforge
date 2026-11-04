//! The Produce menu: new parts from the trance presets, the Starter Pattern dialog and Add
//! Instrument Column, for making parts over a DJ set.
//!
//! The Starter Pattern dialog runs `prod.pattern` on the selected part (over the timeline
//! selection, else `bars` from the playhead's bar) or into a Session view slot. Its key starts as
//! the key the set plays there (`dj.key_at`). A refusal shows in the dialog; one because the
//! pattern would replace something offers Replace, which reruns the refused parameters with
//! `replace: true`. Nothing is replaced without that click. Other commands that refuse to replace
//! (the slot menu's Copy Selected MIDI Clip Here) ask the same in a small window.

use crate::SoundApp;
use crate::theme::{Tokens, bold, regular};
use serde_json::{Value, json};
use soundcraft_midi::Key;
use soundcraft_midi::patterns::{PatternKind, PatternStyle};

/// Bars a pattern fills without a selection (into a slot: [`SLOT_BARS`]).
const TIMELINE_BARS: u32 = 8;
/// Bars a slot pattern starts at.
const SLOT_BARS: u32 = 4;
/// Most bars the dialog offers (`prod.pattern` writes at most 256 into a slot).
const MOST_BARS: u32 = 256;

/// The Produce menu's windows.
#[derive(Default)]
pub struct ProduceState {
    /// The Starter Pattern dialog, while open.
    pub pattern: Option<PatternDialog>,
    /// A command refused because it would replace something, asking whether to.
    pub replace: Option<ReplaceAsk>,
}

/// The Starter Pattern dialog's fields.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternDialog {
    pub kind: PatternKind,
    pub style: PatternStyle,
    /// Bars written into a slot, or from the playhead's bar when there is no selection.
    pub bars: u32,
    /// The key, as typed (bass and synth styles).
    pub key: String,
    /// A fixed seed; `None` lets `prod.pattern` pick one stable for the place.
    pub seed: Option<u64>,
    /// The Session view slot written into (column, slot) instead of the selected part.
    pub slot: Option<(usize, usize)>,
    /// Why the last Create was refused.
    pub error: Option<String>,
    /// The parameters last refused because they would replace something: Replace reruns them
    /// with `replace: true`.
    pub offer: Option<Value>,
}

/// A refusal to replace, offered again with `replace: true`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplaceAsk {
    pub id: String,
    pub params: Value,
    pub why: String,
}

/// What a part of `kind` is called.
pub fn kind_label(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::Drums => "Drums",
        PatternKind::Bass => "Bass",
        PatternKind::Synth => "Synth",
        PatternKind::Fx => "FX",
    }
}

/// "Trance Full" for `trance_full`.
pub fn title(id: &str) -> String {
    let word = |w: &str| {
        let mut c = w.chars();
        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
    };
    id.split('_').map(word).collect::<Vec<_>>().join(" ")
}

/// Whether a refusal is one that `replace: true` would get past.
fn wants_replace(why: &str) -> bool {
    why.contains("replace: true")
}

/// The Produce menu's contents.
pub fn menu(app: &mut SoundApp, ui: &mut egui::Ui) {
    for kind in PatternKind::ALL {
        ui.menu_button(format!("New {} Part", kind_label(kind)), |ui| {
            let presets = app.engine.execute("prod.presets", &json!({"kind": kind.name()})).unwrap_or_default();
            let ids: Vec<String> = presets.as_array().into_iter().flatten().filter_map(|p| p["id"].as_str().map(str::to_string)).collect();
            for (i, id) in ids.iter().enumerate() {
                // `prod.presets` lists each kind's default preset first.
                let label = if i == 0 { format!("{} (default)", title(id)) } else { title(id) };
                if ui.button(label).clicked() {
                    let _ = app.run("prod.new_part", json!({"kind": kind.name(), "preset": id}));
                    ui.close();
                }
            }
        });
    }
    ui.separator();
    if ui.button("Starter Pattern...").on_hover_text("A starter MIDI pattern on the selected part, in key").clicked() {
        open_pattern(app, None);
        ui.close();
    }
    if ui.button("Add Instrument Column").on_hover_text("A Session view column of MIDI slots").clicked() {
        add_instrument_column(app);
        ui.close();
    }
}

/// `live.add_column {kind: instrument}`, with the Session view shown (opening it first sets up
/// the Live columns of a session without any, as View › Session does).
pub fn add_instrument_column(app: &mut SoundApp) {
    if app.run("window.session", json!({"value": true})).is_ok() {
        let _ = app.run("live.add_column", json!({"kind": "instrument"}));
    }
}

/// Opens the Starter Pattern dialog for the selected part, or for Session view slot `slot`
/// (column, slot). Its key is the one the set plays at the selection start (a slot: the
/// playhead), as `dj.key_at` finds it.
pub fn open_pattern(app: &mut SoundApp, slot: Option<(usize, usize)>) {
    let s = app.engine.session();
    let at = if slot.is_none() && !s.edit.selection.is_empty() { s.edit.selection.start } else { s.edit.playhead };
    let key = app.engine.execute("dj.key_at", &json!({"at": at})).ok().and_then(|v| v["key"].as_str().map(str::to_string)).unwrap_or_default();
    let kind = PatternKind::Drums;
    let style = PatternStyle::of_kind(kind).next().unwrap_or(PatternStyle::FourFloor);
    app.produce.pattern = Some(PatternDialog {
        kind,
        style,
        bars: if slot.is_some() { SLOT_BARS } else { TIMELINE_BARS },
        key,
        seed: None,
        slot,
        error: None,
        offer: None,
    });
}

/// Runs `id`; when it refuses because it would replace something, asks whether to (Replace
/// reruns it with `replace: true`).
pub fn run_or_offer_replace(app: &mut SoundApp, id: &str, params: Value) {
    if let Err(why) = app.run(id, params.clone())
        && wants_replace(&why)
    {
        app.produce.replace = Some(ReplaceAsk { id: id.to_string(), params, why });
    }
}

/// The `prod.pattern` parameters for the dialog's fields: a key only for the styles that use one,
/// and bars into a slot or when there is no selection.
pub fn pattern_params(app: &SoundApp, d: &PatternDialog) -> Value {
    let mut p = json!({"kind": d.kind.name(), "style": d.style.name()});
    if d.style.needs_key() && !d.key.trim().is_empty() {
        p["key"] = json!(d.key.trim());
    }
    if let Some(seed) = d.seed {
        p["seed"] = json!(seed);
    }
    match d.slot {
        Some((column, slot)) => {
            p["slot"] = json!({"column": column, "slot": slot});
            p["bars"] = json!(d.bars);
        }
        None if app.engine.session().edit.selection.is_empty() => p["bars"] = json!(d.bars),
        None => {}
    }
    p
}

/// A seed other than `old`, from the clock.
fn shuffled(old: Option<u64>) -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let mut x = soundcraft_midi::XorShift::new(u64::from(nanos) ^ old.unwrap_or(0).rotate_left(17));
    // Kept under 2^32 so it reads back exactly wherever the number goes.
    let mut seed = x.next_u64() >> 32;
    if Some(seed) == old {
        seed = seed.wrapping_add(1) & 0xFFFF_FFFF;
    }
    seed
}

/// What the dialog writes on: the slot, else the selected part.
fn target(app: &SoundApp, d: &PatternDialog) -> String {
    let s = app.engine.session();
    match d.slot {
        Some((c, r)) => format!("Into Live {} slot {}", c + 1, r + 1),
        None => {
            let parts: Vec<&str> = s
                .tracks
                .iter()
                .filter(|t| s.edit.selected_tracks.contains(&t.id) && t.kind == soundcraft_model::TrackKind::Instrument)
                .map(|t| t.name.as_str())
                .collect();
            let over = if s.edit.selection.is_empty() { "from the playhead's bar" } else { "over the selection" };
            match parts.as_slice() {
                [] => "No part selected: select one (Produce › New Part makes one)".to_string(),
                [one] => format!("On {one}, {over}"),
                _ => "Several parts are selected: select one".to_string(),
            }
        }
    }
}

/// The key field's reading: "Am · 8A", or why it isn't one.
fn key_reading(text: &str, at_slot: bool) -> String {
    if text.trim().is_empty() {
        return if at_slot { "no key at the playhead" } else { "no key here" }.to_string();
    }
    match Key::parse(text) {
        Ok(k) => format!("{} · {}", k.name(), k.camelot()),
        Err(_) => "not a key (try Am, F#, 8A or 1m)".to_string(),
    }
}

pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    pattern_dialog(app, ctx);
    replace_window(app, ctx);
}

fn pattern_dialog(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(mut d) = app.produce.pattern.take() else { return };
    let t = Tokens::current();
    let selection = d.slot.is_none() && !app.engine.session().edit.selection.is_empty();
    let target = target(app, &d);
    let (mut keep, mut create, mut replace) = (true, false, false);
    egui::Modal::new(egui::Id::new("starter_pattern")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.label(egui::RichText::new("Starter Pattern").font(bold(13.0)));
        ui.label(egui::RichText::new(&target).font(regular(11.0)).color(t.text_dim));
        egui::Grid::new("starter_pattern_fields").num_columns(2).show(ui, |ui| {
            ui.label("Kind");
            let before = d.kind;
            egui::ComboBox::from_id_salt("pattern_kind").selected_text(kind_label(d.kind)).show_ui(ui, |ui| {
                for k in PatternKind::ALL {
                    ui.selectable_value(&mut d.kind, k, kind_label(k));
                }
            });
            if d.kind != before {
                d.style = PatternStyle::of_kind(d.kind).next().unwrap_or(d.style);
            }
            ui.end_row();
            ui.label("Style");
            egui::ComboBox::from_id_salt("pattern_style").selected_text(title(d.style.name())).show_ui(ui, |ui| {
                for s in PatternStyle::of_kind(d.kind) {
                    ui.selectable_value(&mut d.style, s, title(s.name()));
                }
            });
            ui.end_row();
            ui.label("Bars");
            ui.horizontal(|ui| {
                ui.add_enabled(!selection, egui::DragValue::new(&mut d.bars).range(1..=MOST_BARS));
                if selection {
                    ui.label(egui::RichText::new("the selection's bars").color(t.text_dim));
                }
            });
            ui.end_row();
            ui.label("Key");
            ui.horizontal(|ui| {
                let keyed = d.style.needs_key();
                ui.add_enabled(keyed, egui::TextEdit::singleline(&mut d.key).desired_width(60.0));
                let reading = if keyed { key_reading(&d.key, d.slot.is_some()) } else { "not used by this style".to_string() };
                ui.label(egui::RichText::new(reading).color(t.text_dim));
            });
            ui.end_row();
            ui.label("Seed");
            ui.horizontal(|ui| {
                let mut fixed = d.seed.is_some();
                if ui.checkbox(&mut fixed, "").on_hover_text("Fix the seed (otherwise one stable for the place)").changed() {
                    d.seed = fixed.then(|| shuffled(None));
                }
                match &mut d.seed {
                    Some(seed) => {
                        ui.add(egui::DragValue::new(seed));
                    }
                    None => {
                        ui.label(egui::RichText::new("auto").color(t.text_dim));
                    }
                }
                if ui.button("Shuffle").on_hover_text("A new seed: another variation").clicked() {
                    d.seed = Some(shuffled(d.seed));
                }
            });
            ui.end_row();
        });
        if let Some(why) = &d.error {
            ui.colored_label(t.accent, why);
        }
        ui.horizontal(|ui| {
            if ui.button("Create").clicked() {
                create = true;
            }
            if d.offer.is_some() && ui.button("Replace").on_hover_text("Replace what is there with the pattern").clicked() {
                replace = true;
            }
            if ui.button("Cancel").clicked() {
                keep = false;
            }
        });
    });
    let params = pattern_params(app, &d);
    // The fields changed since the refusal: what Replace would replace no longer stands.
    if d.offer.as_ref().is_some_and(|o| *o != params) {
        d.offer = None;
        d.error = None;
    }
    let run = if replace {
        d.offer.take().map(|mut p| {
            p["replace"] = json!(true);
            p
        })
    } else if create {
        Some(params.clone())
    } else {
        None
    };
    if let Some(p) = run {
        match app.run("prod.pattern", p) {
            Ok(v) => {
                keep = false;
                let what = v["clip"].as_u64().and_then(|c| app.engine.session().find_clip(soundcraft_model::ClipId(c)).map(|(_, c)| c.name.clone()));
                app.ui.status = match (what, d.slot) {
                    (Some(name), _) => format!("Wrote {name}"),
                    (None, Some((c, r))) => format!("Wrote a pattern into Live {} slot {}", c + 1, r + 1),
                    (None, None) => "Wrote the pattern".to_string(),
                };
            }
            Err(why) => {
                // Replace offers only to rerun what was refused for replacing, never anything else.
                d.offer = (!replace && wants_replace(&why)).then_some(params);
                d.error = Some(why);
            }
        }
    }
    if keep {
        app.produce.pattern = Some(d);
    }
}

fn replace_window(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(ask) = app.produce.replace.take() else { return };
    let (mut keep, mut replace) = (true, false);
    egui::Modal::new(egui::Id::new("replace_ask")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.label(egui::RichText::new("Replace?").font(bold(13.0)));
        ui.label(&ask.why);
        ui.horizontal(|ui| {
            replace = ui.button("Replace").clicked();
            if ui.button("Cancel").clicked() {
                keep = false;
            }
        });
    });
    if replace {
        let mut p = ask.params.clone();
        p["replace"] = json!(true);
        let _ = app.run(&ask.id, p);
        keep = false;
    }
    if keep {
        app.produce.replace = Some(ask);
    }
}
