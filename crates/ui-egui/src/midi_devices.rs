//! Setup › MIDI › MIDI Input Devices (the MIDI Devices window) and the toolbar's MIDI activity
//! light.
//!
//! The window lists every input port with an Enable checkbox, its state (open, closed, or
//! disconnected: enabled but unplugged) and an activity light, and a Reconnect button that closes
//! and reopens the port (WinMM can miss an unplug and replug between two listings, leaving a
//! silent connection). Below: the last few messages, the live target and why it was chosen with
//! an override, Panic, the latency and drop figures, and a way to MIDI Mappings.
//!
//! Listing ports creates a backend client, so the window lists on opening, on Refresh and when
//! the host's device list changes ([`MidiHost::generation`]); the rest is read every frame.

use crate::SoundApp;
use crate::midi_input::{FLASH_S, MidiHost};
use crate::theme::Tokens;
use egui::{Sense, vec2};
use serde_json::{Value, json};
use soundcraft_model::TrackId;

const CACHE: &str = "soundcraft-midi-devices";

/// Runs a UI command, putting a failure on the status line.
fn run(app: &mut SoundApp, id: &str, p: Value) {
    if let Err(e) = app.run(id, p) {
        app.ui.status = e;
    }
}

/// A small round light, announced as `label` and toggled while lit.
fn light(ui: &mut egui::Ui, lit: bool, label: &str) -> egui::Response {
    let t = Tokens::current();
    let (r, resp) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    let fill = if lit { t.meter_green } else { t.text_dim.gamma_multiply(0.35) };
    ui.painter().circle_filled(r.center(), 4.5, fill);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Other, true, lit, label));
    resp
}

/// The MIDI Devices window, while shown.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let mut open = app.ui.show_midi_devices;
    let cache = egui::Id::new(CACHE);
    if !open {
        return;
    }
    let now = ctx.input(|i| i.time);
    let flash = crate::midi_input::flash_s(ctx);
    let t = Tokens::current();
    egui::Window::new("MIDI Devices").id(egui::Id::new("midi_devices")).open(&mut open).default_size(vec2(560.0, 440.0)).show(ctx, |ui| {
        let generation = app.midi_host.as_ref().map(MidiHost::generation);
        let cached: Option<(Option<u64>, Vec<Value>)> = ui.ctx().data(|d| d.get_temp(cache));
        let refresh = ui
            .horizontal(|ui| {
                ui.label(egui::RichText::new("Inputs").strong());
                ui.button("Refresh").on_hover_text("List the devices again").clicked()
            })
            .inner;
        let ports = match cached {
            Some((g, p)) if g == generation && !refresh => p,
            _ => match app.run("midi.inputs", json!({})) {
                Ok(v) => v["ports"].as_array().cloned().unwrap_or_default(),
                Err(e) => {
                    app.ui.status = e;
                    Vec::new()
                }
            },
        };
        if app.midi_host.is_none() {
            ui.label("No MIDI system on this machine.");
        } else if ports.is_empty() {
            ui.label("No MIDI input devices.");
        }
        let mut stale = false;
        egui::Grid::new("midi_devices_ports").num_columns(4).striped(true).show(ui, |ui| {
            for h in ["", "Enable", "State", ""] {
                ui.label(egui::RichText::new(h).strong());
            }
            ui.end_row();
            for port in &ports {
                let name = port["name"].as_str().unwrap_or_default().to_string();
                let raw = port["raw_name"].as_str().unwrap_or_default();
                let backend = port["backend_id"].as_str().unwrap_or_default();
                let state = port["state"].as_str().unwrap_or("closed");
                let lit = crate::midi_input::port_lit(app, raw, backend, now, flash);
                light(ui, lit, &format!("{name} activity")).on_hover_text("Lights on each message from this port");
                let mut on = port["enabled"].as_bool().unwrap_or(false);
                let tip = format!("{raw}\nbackend: {backend}");
                if ui.checkbox(&mut on, &name).on_hover_text(tip).changed() {
                    if state == "disconnected" && !on {
                        // Not listed, so not by name: drop it from the preferences directly.
                        app.ui.midi_inputs.retain(|p| !(p.raw_name == raw && p.backend_id == backend));
                        app.mark_prefs_changed();
                        stale = true;
                    } else {
                        run(app, "midi.input_enable", json!({"name": name, "enabled": on}));
                    }
                }
                let colour = match state {
                    "open" => t.meter_green,
                    "disconnected" => t.meter_red,
                    _ => t.text_dim,
                };
                ui.label(egui::RichText::new(state).color(colour));
                let can = state != "disconnected";
                if ui
                    .add_enabled(can, egui::Button::new("Reconnect"))
                    .on_hover_text("Close the port and open it again (for a device replugged without being noticed)")
                    .clicked()
                {
                    run(app, "midi.reconnect", json!({"name": name}));
                }
                ui.end_row();
            }
        });
        let gen_now = app.midi_host.as_ref().map(MidiHost::generation);
        if gen_now == generation && !stale {
            ui.ctx().data_mut(|d| d.insert_temp(cache, (generation, ports.clone())));
        } else {
            ui.ctx().data_mut(|d| d.remove_temp::<(Option<u64>, Vec<Value>)>(cache));
        }
        let st = crate::midi_input::status(app, now);

        ui.separator();
        ui.label(egui::RichText::new("Last messages").strong());
        let recent = st["recent"].as_array().cloned().unwrap_or_default();
        if recent.is_empty() {
            ui.label(egui::RichText::new("Nothing received yet.").color(t.text_dim));
        } else {
            egui::Grid::new("midi_devices_recent").num_columns(4).striped(true).show(ui, |ui| {
                for h in ["Port", "Type", "Ch", "Data"] {
                    ui.label(egui::RichText::new(h).strong());
                }
                ui.end_row();
                for m in recent.iter().rev() {
                    ui.label(m["port"].as_str().unwrap_or("?"));
                    ui.label(m["type"].as_str().unwrap_or("?"));
                    ui.label(m["ch"].to_string());
                    ui.label(egui::RichText::new(m["data"].as_str().unwrap_or("")).monospace());
                    ui.end_row();
                }
            });
        }

        ui.separator();
        ui.label(egui::RichText::new("Live target").strong());
        let plays = match st["target"].as_object() {
            Some(o) => {
                format!("Plays {} ({})", o.get("name").and_then(Value::as_str).unwrap_or("?"), o.get("why").and_then(Value::as_str).unwrap_or("?"))
            }
            None => "Plays nothing: record-arm an instrument track, or choose one below.".into(),
        };
        ui.label(plays);
        let choices = crate::midi_input::target_choices(app.engine.session());
        let current = app.midi_target.and_then(|id| choices.iter().find(|c| c.0 == id)).map_or("Automatic", |c| c.1.as_str()).to_string();
        let mut pick: Option<Option<TrackId>> = None;
        ui.horizontal(|ui| {
            ui.label("Override");
            egui::ComboBox::from_id_salt("midi_devices_target").selected_text(current).show_ui(ui, |ui| {
                if ui.selectable_label(app.midi_target.is_none(), "Automatic").clicked() {
                    pick = Some(None);
                }
                for (id, name) in &choices {
                    if ui.selectable_label(app.midi_target == Some(*id), name).clicked() {
                        pick = Some(Some(*id));
                    }
                }
            });
            if ui.add_enabled(app.midi_target.is_some(), egui::Button::new("Clear")).on_hover_text("Back to the armed or selected track").clicked() {
                pick = Some(None);
            }
        });
        if let Some(p) = pick {
            run(app, "midi.target", json!({"track": p.map(|t| t.0)}));
        }
        if ui.button("Panic (all notes off)").clicked() {
            run(app, "midi.panic", json!({}));
        }

        ui.separator();
        ui.label(egui::RichText::new("Status").strong());
        let ms = |v: &Value| v.as_f64().map_or_else(|| "–".to_string(), |x| format!("{x:.2} ms"));
        ui.label(format!("Latency L: {}   Hop p99: {}", ms(&st["latency_ms"]), ms(&st["hop_p99_ms"])))
            .on_hover_text("L: the longest recent audio callback plus 2 ms. Hop: the MIDI router's delay, p99 over the last second.");
        let n = |v: &Value| v.as_u64().map_or_else(|| "–".to_string(), |x| x.to_string());
        ui.label(format!(
            "Dropped: port {}, subscribers {}, player {}",
            n(&st["dropped_port"]),
            n(&st["dropped_subscribers"]),
            n(&st["dropped_player"])
        ));
        if st["healthy"] == false {
            ui.label(egui::RichText::new("⚠ The MIDI router has stopped: no more input will arrive. Restart the app.").color(t.meter_red));
        }
        ui.separator();
        if ui.button("MIDI Mappings...").clicked() {
            app.ui.show_midi_mappings = true;
        }
    });
    if !open {
        ctx.data_mut(|d| d.remove_temp::<(Option<u64>, Vec<Value>)>(cache));
    }
    app.ui.show_midi_devices = open;
    // The figures move while shown; a lit light goes out.
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(if crate::midi_input::any_lit(app, now, flash) { FLASH_S } else { 0.5 }));
}

/// The toolbar's MIDI activity light: hidden with no input enabled. Hover shows the live
/// target; a click opens MIDI Devices.
pub fn toolbar_light(app: &mut SoundApp, ui: &mut egui::Ui) {
    if app.ui.midi_inputs.is_empty() {
        return;
    }
    let t = Tokens::current();
    let now = ui.input(|i| i.time);
    let lit = crate::midi_input::any_lit(app, now, crate::midi_input::flash_s(ui.ctx()));
    let (r, resp) = ui.allocate_exact_size(vec2(34.0, 22.0), Sense::click());
    let painter = ui.painter();
    painter.circle_filled(egui::pos2(r.min.x + 6.0, r.center().y), 4.0, if lit { t.meter_green } else { t.text_dim.gamma_multiply(0.35) });
    painter.text(egui::pos2(r.min.x + 13.0, r.center().y), egui::Align2::LEFT_CENTER, "MIDI", crate::theme::bold(9.5), t.text_dim);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, lit, "MIDI activity"));
    let tip = match crate::midi_input::target(app) {
        Some((_, name, why)) => format!("MIDI in plays {name} ({})\nClick for MIDI Devices", why.as_str()),
        None => "MIDI in plays nothing: record-arm an instrument track\nClick for MIDI Devices".into(),
    };
    if resp.on_hover_text(tip).clicked() {
        app.ui.show_midi_devices = true;
    }
    if lit {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(FLASH_S));
    }
}
