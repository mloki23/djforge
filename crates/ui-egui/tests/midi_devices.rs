//! Setup › MIDI › MIDI Input Devices (the MIDI Devices window), the toolbar's MIDI activity
//! light and the deprecated `setup.midi_input_devices`. The devices are a `FakeSource`: no real
//! MIDI port is opened.
#![allow(clippy::unwrap_used)]

use egui::accesskit::Toggled;
use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::{By, NodeT as _, Queryable as _};
use serde_json::json;
use soundcraft_midi_io::{FakeSource, MidiClock, MidiMessage};
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_ui_egui::midi_input::MidiHost;
use soundcraft_ui_egui::{Services, SoundApp, menus};
use std::time::{Duration, Instant};

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().raw_input_hook(&mut raw);
    *h.input_mut() = raw;
    h.step();
}

fn steps(h: &mut Harness<'_, SoundApp>, n: usize) {
    for _ in 0..n {
        step(h);
    }
}

/// Steps (each 0.25 s of UI time; the router thread delivers in real time) until `done`.
fn until(h: &mut Harness<'_, SoundApp>, what: &str, done: impl Fn(&Harness<'_, SoundApp>) -> bool) {
    let start = Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(5), "never: {what} ({})", h.state().ui.status);
        step(h);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The app with a silent player, an armed synth "Lead" and a fake source with `ports` plugged;
/// those in `enable` enabled for live input.
fn harness(ports: &[&str], enable: &[&str]) -> Harness<'static, SoundApp> {
    let mut engine = soundcraft_engine::Engine::default();
    {
        let s = engine.session_mut();
        for (name, armed) in [("Lead", true), ("Pad", false)] {
            let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
            let tr = s.track_mut(t).unwrap();
            tr.name = name.into();
            tr.instrument = Some(Insert::new("subtractive_synth"));
            tr.mixer.record_arm = armed;
        }
    }
    let player = soundcraft_playback::Player::silent(engine.session_arc());
    let mut app = SoundApp::new(engine, Some(player), Services::default());
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    for p in ports {
        src.plug(p);
    }
    app.midi_host = Some(MidiHost::new(Box::new(src)));
    for p in enable {
        app.run("midi.input_enable", json!({"name": p, "enabled": true})).unwrap();
    }
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    steps(&mut h, 3);
    h
}

fn fake<'a>(h: &'a mut Harness<'_, SoundApp>) -> &'a mut FakeSource {
    h.state_mut().midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap()
}

fn is_open(h: &Harness<'_, SoundApp>, name: &str) -> bool {
    h.state().midi_host.as_ref().unwrap().source().is_open(name)
}

fn toggled(h: &Harness<'_, SoundApp>, label: &str) -> bool {
    h.get_by_label(label).accesskit_node().toggled() == Some(Toggled::True)
}

fn has_label(h: &Harness<'_, SoundApp>, label: &str) -> bool {
    h.query_by_label(label).is_some()
}

fn has_label_containing(h: &Harness<'_, SoundApp>, text: &str) -> bool {
    h.query_by_label_contains(text).is_some()
}

/// Opens the window the way the menu does, through the old entry's path.
fn open_from_menu(h: &mut Harness<'_, SoundApp>) {
    let path = "Setup > MIDI > MIDI Input Devices...";
    let id = menus::command_for_path(path, &menus::ui_aliases()).unwrap();
    assert_eq!(id, "window.midi_devices", "the old menu entry opens MIDI Devices");
    menus::invoke_menu(h.state_mut(), &id, path);
    steps(h, 2);
}

#[test]
fn the_old_menu_entry_opens_the_window_which_lists_the_ports_and_enable_toggles_them() {
    let mut h = harness(&["DDJ", "Pads"], &[]);
    assert!(!has_label(&h, "MIDI Devices"));
    open_from_menu(&mut h);
    assert!(h.state().ui.show_midi_devices);
    assert!(has_label(&h, "MIDI Devices"), "the window is shown");
    assert!(has_label(&h, "DDJ") && has_label(&h, "Pads"), "one row per port");
    assert!(!toggled(&h, "Pads"));
    h.get_by_label("Pads").click();
    steps(&mut h, 2);
    assert!(is_open(&h, "Pads"), "enabling opens it");
    assert_eq!(h.state().ui.midi_inputs.len(), 1);
    assert!(toggled(&h, "Pads"));
    h.get_by_label("Pads").click();
    steps(&mut h, 2);
    assert!(!is_open(&h, "Pads") && h.state().ui.midi_inputs.is_empty(), "disabling closes it");
    // There is one MIDI devices entry: the minimal window's is gone.
    let setup = menus::tree().into_iter().find(|n| n.label == "Setup").unwrap();
    let midi = setup.children.iter().find(|n| n.label == "MIDI").unwrap();
    assert!(midi.children.iter().any(|n| n.label == "MIDI Input Devices..."));
    assert!(!midi.children.iter().any(|n| n.label.contains("Live MIDI Inputs")));
}

#[test]
fn the_activity_lights_flash_on_an_injected_event_and_go_out() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 2);
    assert!(!toggled(&h, "DDJ activity") && !toggled(&h, "MIDI activity"));
    assert!(fake(&mut h).inject("DDJ", MidiMessage::NoteOn { ch: 9, note: 36, vel: 100 }, 0));
    until(&mut h, "the light flashes", |h| toggled(h, "DDJ activity"));
    assert!(toggled(&h, "MIDI activity"), "the toolbar light too");
    // The monitor shows the message: port, type, channel (1-based) and data.
    assert!(has_label(&h, "Note On") && has_label(&h, "36 vel 100"));
    steps(&mut h, 2);
    assert!(!toggled(&h, "DDJ activity") && !toggled(&h, "MIDI activity"), "they go out");
}

#[test]
fn reconnect_closes_the_port_and_opens_it_again() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 1);
    // Unplugged and replugged between two listings: the connection is silent, but still open.
    let backend = h.state().ui.midi_inputs[0].backend_id.clone();
    let src = fake(&mut h);
    assert!(src.unplug("DDJ"));
    src.plug_device("DDJ", &backend);
    assert!(!src.inject("DDJ", MidiMessage::ControlChange { ch: 0, cc: 1, value: 1 }, 0), "silent");
    let r = h.state_mut().run("midi.reconnect", json!({"name": "DDJ"})).unwrap();
    assert_eq!((r["closed"].as_bool(), r["open"].as_bool()), (Some(true), Some(true)), "closed, then opened");
    assert!(fake(&mut h).inject("DDJ", MidiMessage::ControlChange { ch: 0, cc: 1, value: 2 }, 0), "live again");
    // The button does the same.
    h.get_by_label("Reconnect").click();
    steps(&mut h, 1);
    assert_eq!(h.state().ui.status, "DDJ reconnected");
    assert!(fake(&mut h).inject("DDJ", MidiMessage::ControlChange { ch: 0, cc: 1, value: 3 }, 0));
    assert!(h.state_mut().run("midi.reconnect", json!({"name": "Nope"})).is_err());
}

#[test]
fn an_unplug_shows_disconnected_and_a_replug_reopens() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 1);
    let backend = h.state().ui.midi_inputs[0].backend_id.clone();
    assert!(fake(&mut h).unplug("DDJ"));
    until(&mut h, "the unplug is noticed", |h| h.state().ui.status.contains("DDJ disconnected – notes released"));
    steps(&mut h, 1);
    assert!(has_label(&h, "disconnected"), "{}", h.state().ui.status);
    assert!(!is_open(&h, "DDJ"));
    fake(&mut h).plug_device("DDJ", &backend);
    until(&mut h, "the replug reopens it", |h| is_open(h, "DDJ"));
    assert_eq!(h.state().ui.status, "DDJ reconnected");
    steps(&mut h, 1);
    assert!(has_label(&h, "open") && !has_label(&h, "disconnected"));
}

#[test]
fn the_override_combo_picks_the_target_and_clear_clears_it() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 2);
    assert!(has_label(&h, "Plays Lead (armed)"));
    h.get(By::new().role(egui::accesskit::Role::ComboBox).value("Automatic")).click();
    steps(&mut h, 2);
    h.get_by_label("Pad").click();
    steps(&mut h, 2);
    let pad = h.state().engine.session().track_by_name("Pad").unwrap().id;
    assert_eq!(h.state().midi_target, Some(pad));
    assert!(has_label(&h, "Plays Pad (override)"));
    assert_eq!(h.state().player.as_ref().unwrap().midi_target(), Some(pad));
    h.get_by_label("Clear").click();
    steps(&mut h, 2);
    assert_eq!(h.state().midi_target, None);
    assert!(has_label(&h, "Plays Lead (armed)"));
}

#[test]
fn panic_and_the_stats_and_the_mappings_shortcut() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 2);
    assert!(has_label_containing(&h, "Latency L:") && has_label_containing(&h, "Hop p99:"));
    assert!(has_label_containing(&h, "Dropped: port 0, subscribers 0, player 0"));
    assert!(!has_label_containing(&h, "router has stopped"), "a healthy router: no warning");
    let st = h.state_mut().run("midi.inputs", json!({})).unwrap();
    assert_eq!(st["healthy"], true);
    assert!(st["hop_p99_ms"].is_number() && st["latency_ms"].is_number());
    assert_eq!(st["target"]["why"], "armed");
    h.get_by_label("Panic (all notes off)").click();
    steps(&mut h, 1);
    assert!(h.state().ui.status.starts_with("MIDI panic"), "{}", h.state().ui.status);
    h.get_by_label("MIDI Mappings...").click();
    steps(&mut h, 1);
    assert!(h.state().ui.show_midi_mappings);
}

#[test]
fn the_toolbar_light_shows_with_an_enabled_input_and_opens_the_window() {
    let mut h = harness(&["DDJ"], &[]);
    assert!(!has_label(&h, "MIDI activity"), "hidden with no input enabled");
    h.state_mut().run("midi.input_enable", json!({"name": "DDJ", "enabled": true})).unwrap();
    steps(&mut h, 1);
    assert!(has_label(&h, "MIDI activity"));
    h.get_by_label("MIDI activity").click();
    steps(&mut h, 2);
    assert!(h.state().ui.show_midi_devices && has_label(&h, "MIDI Devices"));
}

#[test]
fn setup_midi_input_devices_is_a_shim_for_the_window_and_the_preferences() {
    let mut h = harness(&["DDJ"], &[]);
    let r = h.state_mut().run("setup.midi_input_devices", json!({})).unwrap();
    assert!(r["deprecated"].is_string());
    assert!(h.state().ui.show_midi_devices, "with nothing to enable it opens MIDI Devices");
    h.state_mut().run("setup.midi_input_devices", json!({"enable": ["DDJ"]})).unwrap();
    assert!(is_open(&h, "DDJ") && h.state().ui.midi_inputs.len() == 1, "forwarded to midi.input_enable");
    h.state_mut().run("setup.midi_input_devices", json!({"disable": ["DDJ"]})).unwrap();
    assert!(h.state().ui.midi_inputs.is_empty());
    let flags = &h.state().engine.session().edit.flags;
    assert!(!flags.iter().any(|f| f.starts_with("midi.input.")), "no session flags");
    // Headless (replays, old journals): a no-op that takes its old params.
    let mut e = soundcraft_engine::Engine::default();
    let r = e.execute("setup.midi_input_devices", &json!({"enable": ["Keys"]})).unwrap();
    assert_eq!(r["enabled"], json!([]));
    assert!(!e.session().edit.flags.iter().any(|f| f.starts_with("midi.input.")));
}

#[test]
fn old_midi_input_flags_are_dropped_on_load() {
    let mut s = soundcraft_model::Session::default();
    s.edit.set_flag("midi.input.Keys", true);
    s.edit.set_flag("midi.filter.sysex", true);
    let back = soundcraft_model::Session::from_json(&s.to_json().unwrap()).unwrap();
    assert!(!back.edit.flag("midi.input.Keys"));
    assert!(back.edit.flag("midi.filter.sysex"), "other flags stay");
}

fn peak(h: &Harness<'_, SoundApp>, track: &str) -> f32 {
    let t = h.state().engine.session().track_by_name(track).unwrap().id;
    h.state().player.as_ref().unwrap().meters().tracks.get(&t).map_or(0.0, |m| m.peak[0].max(m.peak[1]))
}

/// Holds a note from the DDJ on the armed synth until it sounds.
fn hold_a_note(h: &mut Harness<'_, SoundApp>) {
    assert!(fake(h).inject("DDJ", MidiMessage::NoteOn { ch: 0, note: 60, vel: 110 }, 0));
    until(h, "the held note sounds", |h| peak(h, "Lead") > 1e-3);
}

/// Steps in real time until the synth is silent; false if it still sounds after `secs`.
fn falls_silent(h: &mut Harness<'_, SoundApp>, secs: f64) -> bool {
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        if peak(h, "Lead") < 1e-5 {
            return true;
        }
        step(h);
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn a_reconnect_or_a_disable_releases_the_notes_the_port_held() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    hold_a_note(&mut h);
    assert!(!falls_silent(&mut h, 0.6), "a held note keeps sounding");
    h.state_mut().run("midi.reconnect", json!({"name": "DDJ"})).unwrap();
    assert!(falls_silent(&mut h, 5.0), "the reconnect released it");
    hold_a_note(&mut h);
    h.state_mut().run("midi.input_enable", json!({"name": "DDJ", "enabled": false})).unwrap();
    assert!(falls_silent(&mut h, 5.0), "disabling released it");
}

#[test]
fn only_enabled_listed_ports_offer_reconnect() {
    let mut h = harness(&["DDJ", "Pads"], &["DDJ"]);
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 2);
    assert_eq!(h.get_all_by_label("Reconnect").count(), 1, "DDJ only");
    let err = h.state_mut().run("midi.reconnect", json!({"name": "Pads"})).unwrap_err();
    assert!(err.contains("not enabled"), "{err}");
    assert!(!is_open(&h, "Pads"));
}

#[test]
fn midi_wakes_the_ui_at_a_capped_rate_for_lights_and_at_once_for_mappings() {
    use soundcraft_ui_egui::midi_input::{WAKE_DISPLAY, WAKE_OFF, WAKE_URGENT};
    let wake = |h: &Harness<'_, SoundApp>| h.state().midi_learn.wake_wanted.load(std::sync::atomic::Ordering::Relaxed);
    let mut h = harness(&["DDJ"], &[]);
    assert_eq!(wake(&h), WAKE_OFF, "no light shown");
    h.state_mut().run("midi.input_enable", json!({"name": "DDJ", "enabled": true})).unwrap();
    steps(&mut h, 1);
    assert_eq!(wake(&h), WAKE_DISPLAY, "the Edit toolbar's light");
    h.state_mut().run("window.mix", json!({})).unwrap();
    steps(&mut h, 1);
    assert_eq!(wake(&h), WAKE_OFF, "the Mix window has no light");
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 1);
    assert_eq!(wake(&h), WAKE_DISPLAY, "MIDI Devices' lights");
    let m = json!({"device": "DDJ", "matcher": {"type": "cc", "ch": 0, "cc": 7}, "target": {"kind": "stop_all"}});
    h.state_mut().run("midi.map", m).unwrap();
    steps(&mut h, 1);
    assert_eq!(wake(&h), WAKE_URGENT, "a mapping acts at once");
}

/// A DDJ plugged into another USB socket (WinMM: a new interface path; ALSA: a new client id)
/// is the same device: its enable and its mappings move to the new backend id.
#[test]
fn a_replug_with_another_backend_id_moves_the_enable_and_the_mappings() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut()
        .run("midi.map", json!({"device": "DDJ", "matcher": {"type": "note", "ch": 0, "note": 36}, "target": {"kind": "stop_all"}}))
        .unwrap();
    let old = h.state().ui.midi_inputs[0].backend_id.clone();
    assert!(fake(&mut h).unplug("DDJ"));
    until(&mut h, "the unplug is noticed", |h| !is_open(h, "DDJ"));
    fake(&mut h).plug_device("DDJ", "usb:other-socket");
    until(&mut h, "found on the new port", |h| is_open(h, "DDJ"));
    assert!(h.state().ui.status.contains("DDJ found on a new USB port; settings moved"), "{}", h.state().ui.status);
    let a = h.state();
    assert_eq!(a.ui.midi_inputs.len(), 1);
    assert_eq!(a.ui.midi_inputs[0].backend_id, "usb:other-socket");
    assert_eq!(a.ui.midi_mappings[0].device.backend_id, "usb:other-socket", "the mapping followed");
    assert_ne!(old, "usb:other-socket");
    assert!(h.state_mut().take_recent_changed(), "saved");
}

/// Two listed ports with the DDJ's name and neither its backend id: nothing is guessed; MIDI
/// Devices offers each, and the choice moves the settings.
#[test]
fn a_replug_that_two_ports_could_be_is_not_guessed() {
    let mut h = harness(&["DDJ"], &["DDJ"]);
    h.state_mut()
        .run("midi.map", json!({"device": "DDJ", "matcher": {"type": "note", "ch": 0, "note": 36}, "target": {"kind": "stop_all"}}))
        .unwrap();
    let old = h.state().ui.midi_inputs[0].backend_id.clone();
    assert!(fake(&mut h).unplug("DDJ"));
    until(&mut h, "the unplug is noticed", |h| !is_open(h, "DDJ"));
    fake(&mut h).plug_device("DDJ", "usb:a");
    fake(&mut h).plug_device("DDJ", "usb:b");
    until(&mut h, "both are seen", |h| !h.state().midi_host.as_ref().unwrap().ambiguous().is_empty());
    assert_eq!(h.state().ui.midi_inputs[0].backend_id, old, "not guessed");
    assert_eq!(h.state().ui.midi_mappings[0].device.backend_id, old);
    assert!(!is_open(&h, "DDJ") && !is_open(&h, "DDJ #2"));
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 2);
    assert_eq!(h.query_all(By::new().label_contains("Use settings from DDJ")).count(), 2, "one offer per port");
    let r = h.state_mut().run("midi.move_settings", json!({"raw_name": "DDJ", "backend_id": old, "to": "DDJ #2"})).unwrap();
    assert_eq!(r["backend_id"], "usb:b");
    assert!(is_open(&h, "DDJ #2"));
    assert_eq!(h.state().ui.midi_mappings[0].device.backend_id, "usb:b");
    assert!(h.state().midi_host.as_ref().unwrap().ambiguous().is_empty());
}
