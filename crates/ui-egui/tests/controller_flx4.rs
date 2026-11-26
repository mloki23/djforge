//! The built-in DDJ-FLX4 layout through the app, with a fake controller (a `FakeSource` input and
//! a `FakeOutputs` output, never a real port) and the silent clock: the controller plays a deck
//! and its lights follow, LOAD respects Load Lock, a replugged output gets every light again,
//! turning the layout off or quitting darkens it, a MIDI Learn mapping overrides a layout entry,
//! and the controller's messages never play the live instrument.
#![allow(clippy::unwrap_used)]

use egui::vec2;
use serde_json::json;
use soundcraft_midi_io::{FakeOutputs, FakeSource, MidiClock, MidiOutputs as _};
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_ui_egui::midi_input::MidiHost;
use soundcraft_ui_egui::{Services, SoundApp};
use std::time::{Duration, Instant};

const FLX4: &str = "DDJ-FLX4";

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A short wav to load on a deck.
fn track(tag: &str) -> (Scratch, String) {
    let dir = std::env::temp_dir().join(format!("djforge-ui-flx4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 8 * 48_000]; 2] };
    let path = dir.join("track.wav");
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
    (Scratch(dir), path.to_string_lossy().into_owned())
}

/// The test's UI clock and egui context.
struct Ui {
    ctx: egui::Context,
    t: f64,
}

impl Ui {
    fn new() -> Ui {
        Ui { ctx: egui::Context::default(), t: 1.0 }
    }

    /// One frame, `dt` UI seconds after the last.
    fn frame(&mut self, app: &mut SoundApp, dt: f64) {
        self.t += dt;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))),
            time: Some(self.t),
            ..Default::default()
        };
        self.ctx
            .run_ui(raw, |ui| {
                app.logic(ui.ctx());
                app.ui(ui);
            })
            .textures_delta
            .clear();
    }

    /// Frames (in real time too: the router thread delivers, the silent clock runs) until `done`.
    fn until(&mut self, app: &mut SoundApp, what: &str, done: impl Fn(&SoundApp) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(8), "never: {what} ({})", app.ui.status);
            self.frame(app, 0.02);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The app with a fake DDJ-FLX4 (input and output) enabled.
fn app_with_flx4(engine: soundcraft_engine::Engine) -> SoundApp {
    let player = soundcraft_playback::Player::silent(engine.session_arc());
    let mut app = SoundApp::new(engine, Some(player), Services::default());
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    src.plug(FLX4);
    let mut outs = FakeOutputs::new();
    outs.plug(FLX4);
    app.midi_host = Some(MidiHost::new(Box::new(src)).with_outputs(Box::new(outs)));
    app.run("midi.input_enable", json!({"name": FLX4, "enabled": true})).unwrap();
    app
}

fn send(app: &mut SoundApp, bytes: [u8; 3]) {
    let src = app.midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap();
    assert!(src.inject_bytes(FLX4, &bytes, 0));
}

fn outs(app: &mut SoundApp) -> &mut FakeOutputs {
    app.midi_host.as_mut().unwrap().outputs_mut().unwrap().as_fake_mut().unwrap()
}

/// What the controller was sent since the last call, as wire bytes.
fn sent(app: &mut SoundApp) -> Vec<[u8; 3]> {
    outs(app).take_sent().into_iter().map(|(_, m)| m.wire().0).collect()
}

fn layout(app: &mut SoundApp) -> serde_json::Value {
    let v = app.run("midi.inputs", json!({})).unwrap();
    v["ports"].as_array().unwrap().iter().find(|p| p["raw_name"] == FLX4).unwrap()["layout"].clone()
}

#[test]
fn the_flx4_plays_a_deck_lights_follow_load_lock_holds_and_it_goes_dark() {
    let mut ui = Ui::new();
    let mut app = app_with_flx4(soundcraft_engine::Engine::default());
    ui.until(&mut app, "the lights' output opens", |a| a.controllers.lit());
    ui.frame(&mut app, 0.02);
    let l = layout(&mut app);
    assert_eq!((l["name"].as_str(), l["on"].as_bool(), l["output"].as_str()), (Some("DDJ-FLX4"), Some(true), Some("open")), "{l}");
    // Opening sends every light: the hot cue mode LED on, the empty decks dark.
    let first = sent(&mut app);
    assert!(first.len() > 50, "{}", first.len());
    assert!(first.contains(&[0x90, 0x1B, 0x7F]) && first.contains(&[0x90, 0x0B, 0x00]) && first.contains(&[0xB0, 0x02, 0x00]), "{first:02X?}");
    ui.frame(&mut app, 0.02);
    assert!(sent(&mut app).is_empty(), "nothing changed, nothing sent");

    // A track on Deck A: PLAY on the controller plays it, and the PLAY LED stays lit.
    let (_dir, path) = track("play");
    app.run("deck.load", json!({"path": path, "deck": "a", "bpm": 120})).unwrap();
    ui.until(&mut app, "deck A loaded", |a| a.engine.deck_tracks[0].is_some() && a.engine.deck_status.decks[0].load != 0);
    ui.frame(&mut app, 0.02);
    let mut since = sent(&mut app);
    assert!(since.contains(&[0x9F, 0x00, 0x7F]), "the track-loaded flash");
    send(&mut app, [0x90, 0x0B, 0x7F]);
    send(&mut app, [0x90, 0x0B, 0x00]);
    ui.until(&mut app, "deck A plays", |a| a.engine.deck_status.decks[0].playing);
    ui.frame(&mut app, 0.02);
    since.extend(sent(&mut app));
    let play = |s: &[[u8; 3]]| s.iter().rev().find(|b| b[..2] == [0x90, 0x0B]).map(|b| b[2]);
    assert_eq!(play(&since), Some(0x7F), "PLAY lit: {since:02X?}");
    ui.frame(&mut app, 0.3);
    ui.frame(&mut app, 0.3);
    assert_eq!(play(&sent(&mut app)), None, "playing: no blinking");
    // The headphone CUE button toggles the deck's cue, and its LED follows.
    send(&mut app, [0x90, 0x54, 0x7F]);
    ui.until(&mut app, "deck A cued", |a| a.engine.deck_status.decks[0].headphone_cue);
    ui.frame(&mut app, 0.02);
    assert!(sent(&mut app).contains(&[0x90, 0x54, 0x7F]));

    // LOAD 1 with Deck A playing: Load Lock refuses, and says so.
    app.decks_view.browser.rows = vec![soundcraft_ui_egui::library_browser::Row { path: path.clone(), title: "track".into(), ..Default::default() }];
    app.decks_view.browser.selected = Some(0);
    let load = app.engine.deck_status.decks[0].load;
    send(&mut app, [0x96, 0x46, 0x7F]);
    ui.until(&mut app, "the refusal", |a| a.ui.status.contains("playing"));
    assert!(app.ui.status.contains("Deck A"), "{}", app.ui.status);
    assert_eq!(app.engine.deck_status.decks[0].load, load, "nothing loaded");
    assert!(!app.engine.deck_loading_on(soundcraft_mix::DeckId::A));
    // LOAD 2 loads the selected row on the empty Deck B.
    send(&mut app, [0x96, 0x47, 0x7F]);
    ui.until(&mut app, "deck B loaded", |a| a.engine.deck_tracks[1].is_some());

    // The output unplugged and plugged back in: every light is sent again.
    let backend = outs(&mut app).list()[0].backend_id.clone();
    outs(&mut app).unplug(FLX4);
    ui.frame(&mut app, 1.1);
    ui.frame(&mut app, 0.02);
    assert!(!app.controllers.lit(), "the output went away");
    outs(&mut app).plug_device(FLX4, &backend);
    sent(&mut app);
    ui.frame(&mut app, 1.1);
    assert!(app.controllers.lit());
    let again = sent(&mut app);
    assert!(again.len() > 50 && again.contains(&[0x90, 0x0B, 0x7F]) && again.contains(&[0x90, 0x54, 0x7F]), "{}", again.len());

    // The layout turned off: lights dark, its output closed, its buttons no longer play.
    app.run("midi.layout", json!({"name": FLX4, "on": false})).unwrap();
    ui.frame(&mut app, 0.02);
    let dark = sent(&mut app);
    assert!(dark.len() > 50 && dark.iter().all(|b| b[2] == 0), "{dark:02X?}");
    assert_eq!(layout(&mut app)["on"], false);
    assert!(!outs(&mut app).is_open(FLX4));
    send(&mut app, [0x90, 0x0B, 0x7F]);
    for _ in 0..20 {
        ui.frame(&mut app, 0.02);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(app.engine.deck_status.decks[0].playing, "PLAY does nothing with the layout off");
    // On again: every light again; quitting: every light off.
    app.run("midi.layout", json!({"name": FLX4})).unwrap();
    ui.until(&mut app, "the lights again", |a| a.controllers.lit());
    ui.frame(&mut app, 0.02);
    assert!(sent(&mut app).len() > 50);
    soundcraft_ui_egui::controller_layouts::shutdown(&mut app);
    let off = sent(&mut app);
    assert!(off.len() > 50 && off.iter().all(|b| b[2] == 0), "{off:02X?}");
    assert!(off.contains(&[0x90, 0x0B, 0x00]) && off.contains(&[0x90, 0x54, 0x00]) && off.contains(&[0xB0, 0x02, 0x00]));
    assert!(!outs(&mut app).is_open(FLX4));
    app.run("deck.pause", json!({"deck": "a"})).unwrap();
    ui.frame(&mut app, 0.02);
}

fn synth() -> (soundcraft_engine::Engine, soundcraft_model::TrackId) {
    let mut engine = soundcraft_engine::Engine::default();
    let t = {
        let s = engine.session_mut();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Synth"));
        let tr = s.track_mut(t).unwrap();
        tr.instrument = Some(Insert::new("subtractive_synth"));
        tr.mixer.record_arm = true;
        t
    };
    (engine, t)
}

fn peak(app: &SoundApp, t: soundcraft_model::TrackId) -> f32 {
    app.player.as_ref().unwrap().meters().tracks.get(&t).map_or(0.0, |m| m.peak[0].max(m.peak[1]))
}

#[test]
fn flx4_messages_never_play_the_instrument_and_midi_learn_overrides_the_layout() {
    let (engine, t) = synth();
    let mut ui = Ui::new();
    let mut app = app_with_flx4(engine);
    // MIDI Learn maps deck 1's PLAY to a command: the mapping wins over the layout.
    app.run(
        "midi.map",
        json!({"device": FLX4, "matcher": {"type": "note", "ch": 0, "note": 0x0B}, "target": {"kind": "command", "id": "window.transport"}}),
    )
    .unwrap();
    ui.until(&mut app, "the layout starts", |a| a.controllers.active());
    ui.frame(&mut app, 0.02);
    assert_eq!(app.player.as_ref().unwrap().midi_target(), Some(t));
    let shown = app.ui.show_transport;
    send(&mut app, [0x90, 0x0B, 0x7F]);
    ui.until(&mut app, "the mapped command", |a| a.ui.show_transport != shown);
    // Pads, the jog, a knob: claimed by the layout, never heard on the armed synth.
    for bytes in
        [[0x97, 0x00, 0x7F], [0x97, 0x00, 0x00], [0x90, 0x36, 0x7F], [0xB0, 0x21, 0x41], [0x90, 0x3F, 0x7F], [0x99, 0x24, 0x7F], [0xB6, 0x05, 0x40]]
    {
        send(&mut app, bytes);
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        ui.frame(&mut app, 0.02);
        assert!(peak(&app, t) < 1e-3, "a controller message played the synth");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_learned_14_bit_knob_drives_only_its_learned_target() {
    let (engine, _) = synth();
    let mut ui = Ui::new();
    let mut app = app_with_flx4(engine);
    ui.until(&mut app, "the layout starts", |a| a.controllers.active());
    // EQ HI turned fully up through the layout first: it has seen the knob's MSB.
    send(&mut app, [0xB0, 0x07, 0x7F]);
    send(&mut app, [0xB0, 0x27, 0x7F]);
    ui.until(&mut app, "EQ HI up", |a| a.engine.deck_status.decks[0].eq_db[2] > 3.0);
    let hi = app.engine.deck_status.decks[0].eq_db[2];
    // Then EQ HI learned (its MSB) to the synth's volume.
    app.run(
        "midi.map",
        json!({"device": FLX4, "matcher": {"type": "cc", "ch": 0, "cc": 0x07}, "target": {"kind": "track_volume", "track": "Synth"}, "pickup": false}),
    )
    .unwrap();
    ui.until(&mut app, "the layout starts", |a| a.controllers.active());
    let before = app.engine.session().track_by_name("Synth").unwrap().mixer.volume_db;
    // The knob turned down: MSB then LSB, then the unlearned EQ MID as a marker.
    for bytes in [[0xB0, 0x07, 0x10], [0xB0, 0x27, 0x00], [0xB0, 0x0B, 0x7F], [0xB0, 0x2B, 0x7F]] {
        send(&mut app, bytes);
    }
    ui.until(&mut app, "EQ MID moves (the layout still has the unlearned knob)", |a| a.engine.deck_status.decks[0].eq_db[1] > 0.5);
    let after = app.engine.session().track_by_name("Synth").unwrap().mixer.volume_db;
    assert!(after < before - 3.0, "the learned target moved: {before} → {after}");
    assert_eq!(app.engine.deck_status.decks[0].eq_db[2], hi, "the LSB of the learned knob didn't reach the layout's EQ HI");
}

/// Deck A with a track, paused, its CUE held from the controller (previewing).
fn hold_cue(ui: &mut Ui, app: &mut SoundApp, what: &str) {
    send(app, [0x90, 0x0C, 0x7F]);
    ui.until(app, &format!("the CUE preview ({what})"), |a| a.engine.deck_status.decks[0].previewing);
}

#[test]
fn a_held_cue_is_let_go_when_learn_starts_the_layout_stops_the_app_quits_or_the_controller_goes() {
    let mut ui = Ui::new();
    let mut app = app_with_flx4(soundcraft_engine::Engine::default());
    let (_dir, path) = track("release");
    app.run("deck.load", json!({"path": path, "deck": "a", "bpm": 120})).unwrap();
    ui.until(&mut app, "deck A loaded", |a| a.engine.deck_tracks[0].is_some() && a.engine.deck_status.decks[0].load != 0 && a.controllers.active());
    let stopped = |a: &SoundApp| !a.engine.deck_status.decks[0].previewing;
    // MIDI Learn starts: the release would never reach the layout.
    hold_cue(&mut ui, &mut app, "learn");
    app.run("midi.learn", json!({"target": {"kind": "command", "id": "window.transport"}})).unwrap();
    ui.until(&mut app, "let go when learning starts", stopped);
    app.run("midi.learn", json!({"cancel": true})).unwrap();
    // The layout turned off.
    hold_cue(&mut ui, &mut app, "layout off");
    app.run("midi.layout", json!({"name": FLX4, "on": false})).unwrap();
    ui.until(&mut app, "let go when the layout stops", stopped);
    app.run("midi.layout", json!({"name": FLX4, "on": true})).unwrap();
    ui.until(&mut app, "the layout again", |a| a.controllers.active());
    // The app quits.
    hold_cue(&mut ui, &mut app, "quit");
    soundcraft_ui_egui::controller_layouts::shutdown(&mut app);
    ui.until(&mut app, "let go at quit", stopped);
    // The controller unplugged: noticed at the next device refresh.
    ui.until(&mut app, "the layout again", |a| a.controllers.active());
    hold_cue(&mut ui, &mut app, "unplug");
    assert!(app.midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap().unplug(FLX4));
    ui.frame(&mut app, 1.1);
    ui.until(&mut app, "let go when the controller goes", stopped);
    assert!(!app.controllers.active());
}
