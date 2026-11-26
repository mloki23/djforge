//! MIDI Learn on the Decks tab's controls, with fake MIDI devices (a `FakeSource`, never a real
//! port) and the silent clock: right-click a deck, mixer or library control, Learn MIDI, move a
//! control, and the mapping is there; a learned CUE and hot cue play the deck as a controller's
//! do, and what they hold is let go when learning starts, the app quits or the device goes; a
//! learned message overrides the DDJ-FLX4's layout; a mapped message never plays the live
//! instrument.
#![allow(clippy::unwrap_used)]

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use serde_json::{Value, json};
use soundcraft_midi_io::{FakeOutputs, FakeSource, MidiClock, MidiMessage};
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_ui_egui::midi_input::MidiHost;
use soundcraft_ui_egui::midi_learn::{Matcher, Target};
use soundcraft_ui_egui::{Services, SoundApp};
use std::time::{Duration, Instant};

const PADS: &str = "Pads";
const FLX4: &str = "DDJ-FLX4";

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A short wav to load on a deck, in a temporary folder of its own.
fn track(tag: &str) -> (Scratch, String) {
    let dir = std::env::temp_dir().join(format!("djforge-deck-learn-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 8 * 48_000]; 2] };
    let path = dir.join("track.wav");
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
    (Scratch(dir), path.to_string_lossy().into_owned())
}

/// An engine whose record-armed synth is the live MIDI target.
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

/// The app on `engine` with a silent player and fake "Pads" and DDJ-FLX4 inputs enabled (the
/// FLX4 with its output, for its layout).
fn app(engine: soundcraft_engine::Engine) -> SoundApp {
    let player = soundcraft_playback::Player::silent(engine.session_arc());
    let mut app = SoundApp::new(engine, Some(player), Services::default());
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    src.plug(PADS);
    src.plug(FLX4);
    let mut outs = FakeOutputs::new();
    outs.plug(FLX4);
    app.midi_host = Some(MidiHost::new(Box::new(src)).with_outputs(Box::new(outs)));
    for name in [PADS, FLX4] {
        app.run("midi.input_enable", json!({"name": name, "enabled": true})).unwrap();
    }
    app
}

fn inject(app: &mut SoundApp, device: &str, msg: MidiMessage) {
    let src = app.midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap();
    assert!(src.inject(device, msg, 0), "{device} is plugged in");
}

fn on(note: u8) -> MidiMessage {
    MidiMessage::NoteOn { ch: 0, note, vel: 100 }
}

fn off(note: u8) -> MidiMessage {
    MidiMessage::NoteOff { ch: 0, note, vel: 0 }
}

fn cc(cc: u8, value: u8) -> MidiMessage {
    MidiMessage::ControlChange { ch: 0, cc, value }
}

// ---- Through the Decks tab (the kittest harness) ----

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

fn until(h: &mut Harness<'_, SoundApp>, what: &str, done: impl Fn(&SoundApp) -> bool) {
    let start = Instant::now();
    while !done(h.state()) {
        assert!(start.elapsed() < Duration::from_secs(8), "never: {what} ({})", h.state().ui.status);
        step(h);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn target(v: Value) -> Target {
    serde_json::from_value(v).unwrap()
}

#[test]
fn each_kind_of_deck_mixer_and_library_control_learns_from_the_decks_tab() {
    let (engine, _) = synth();
    // 20 frames a second: a learned knob is told from an encoder in a few frames.
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).with_step_dt(1.0 / 20.0).with_max_steps(1_000_000).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app(engine),
    );
    steps(&mut h, 3);
    h.state_mut().run("window.decks", json!({})).unwrap();
    steps(&mut h, 20);
    // A library row to right-click (the library has none here).
    h.state_mut().decks_view.browser.rows =
        vec![soundcraft_ui_egui::library_browser::Row { path: "nowhere.wav".into(), title: "track".into(), ..Default::default() }];
    steps(&mut h, 3);
    // (control, menu path, target, messages: the first binds, the rest tell a knob's kind).
    let cases: Vec<(&str, Vec<&str>, Value, Vec<MidiMessage>)> = vec![
        ("Deck A Cue", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "a", "control": "cue"}), vec![on(36), off(36)]),
        ("Deck B Play", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "b", "control": "play"}), vec![on(37)]),
        (
            "Deck A Hot Cue C",
            vec!["Learn MIDI for Clear Hot Cue C"],
            json!({"kind": "deck", "deck": "a", "control": "hot_cue_clear", "n": 3}),
            vec![on(38)],
        ),
        ("Deck B Hot Cue H", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "b", "control": "hot_cue", "n": 8}), vec![on(39)]),
        ("Deck A Loop Exit", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "a", "control": "reloop"}), vec![on(40)]),
        ("Deck B Beat Jump Forward", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "b", "control": "jump_forward"}), vec![on(41)]),
        ("Deck A Pitch", vec!["Learn MIDI for Pitch Bend −"], json!({"kind": "deck", "deck": "a", "control": "pitch_bend_down"}), vec![on(42)]),
        ("Deck B Pitch", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "b", "control": "pitch"}), vec![cc(1, 20), cc(1, 25), cc(1, 31)]),
        ("Deck A Jog", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "a", "control": "jog"}), vec![cc(2, 65); 4]),
        ("Deck B EQ LOW", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "b", "control": "eq_low"}), vec![cc(3, 60), cc(3, 70)]),
        ("Deck A Fader", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "a", "control": "fader"}), vec![cc(4, 90), cc(4, 100)]),
        ("Deck B Headphone Cue", vec!["Learn MIDI"], json!({"kind": "deck", "deck": "b", "control": "headphone_cue"}), vec![on(43)]),
        ("Crossfader", vec!["Learn MIDI"], json!({"kind": "mixer", "control": "crossfader"}), vec![cc(5, 10), cc(5, 30)]),
        ("Headphone Mix", vec!["Learn MIDI"], json!({"kind": "mixer", "control": "headphone_mix"}), vec![cc(6, 10), cc(6, 30)]),
        ("Master Cue", vec!["Learn MIDI"], json!({"kind": "mixer", "control": "master_cue"}), vec![on(44)]),
        ("Track track", vec!["Learn MIDI for Browse"], json!({"kind": "library", "control": "browse"}), vec![cc(8, 1); 4]),
    ];
    for (control, menu, want, msgs) in cases {
        let want = target(want);
        h.get_by_label(control).click_secondary();
        steps(&mut h, 3);
        for item in &menu {
            h.get_by_label(item).click();
            steps(&mut h, 3);
        }
        assert_eq!(h.state().midi_learn.learning.as_ref(), Some(&want), "{control}: armed");
        let n = h.state().ui.midi_mappings.len();
        for m in msgs {
            inject(h.state_mut(), PADS, m);
        }
        until(&mut h, control, |a| a.ui.midi_mappings.len() > n && a.midi_learn.detect.is_none());
        let m = h.state().ui.midi_mappings.last().unwrap().clone();
        assert_eq!(m.target, want, "{control}");
        assert_eq!(m.device.name, PADS);
        steps(&mut h, 2);
    }
    let mode = |want: Value| {
        let t = target(want);
        h.state().ui.midi_mappings.iter().find(|m| m.target == t).map(|m| m.mode).unwrap()
    };
    use soundcraft_ui_egui::midi_learn::Mode;
    assert_eq!(mode(json!({"kind": "deck", "deck": "a", "control": "jog"})), Mode::RelOffset, "the jog: an encoder");
    assert_eq!(mode(json!({"kind": "deck", "deck": "b", "control": "pitch"})), Mode::Absolute, "the pitch: a fader");
    assert_eq!(mode(json!({"kind": "library", "control": "browse"})), Mode::RelTwos);
    // Remove MIDI Mapping forgets it.
    h.get_by_label("Deck A Cue").click_secondary();
    steps(&mut h, 3);
    h.get_by_label("Remove MIDI Mapping").click();
    steps(&mut h, 3);
    assert!(!h.state().ui.midi_mappings.iter().any(|m| m.target == target(json!({"kind": "deck", "deck": "a", "control": "cue"}))));
    assert!(h.state().midi_learn.learning.is_none(), "nothing left armed");
}

// ---- Through the app's frames (the fake devices and the silent clock in real time) ----

struct Ui {
    ctx: egui::Context,
    t: f64,
}

impl Ui {
    fn new() -> Ui {
        Ui { ctx: egui::Context::default(), t: 1.0 }
    }

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

    fn until(&mut self, app: &mut SoundApp, what: &str, done: impl Fn(&SoundApp) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(8), "never: {what} ({})", app.ui.status);
            self.frame(app, 0.02);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn peak(app: &SoundApp, t: soundcraft_model::TrackId) -> f32 {
    app.player.as_ref().unwrap().meters().tracks.get(&t).map_or(0.0, |m| m.peak[0].max(m.peak[1]))
}

fn map(app: &mut SoundApp, device: &str, matcher: Value, target: Value) {
    app.run("midi.map", json!({"device": device, "matcher": matcher, "target": target})).unwrap();
}

#[test]
fn learned_cue_and_hot_cue_play_the_deck_and_let_go_on_learn_quit_and_unplug() {
    let (engine, synth_track) = synth();
    let mut ui = Ui::new();
    let mut app = app(engine);
    let (_dir, path) = track("hold");
    for d in ["a", "b"] {
        app.run("deck.load", json!({"path": path, "deck": d, "bpm": 120})).unwrap();
    }
    ui.until(&mut app, "both decks loaded", |a| (0..2).all(|i| a.engine.deck_tracks[i].is_some() && a.engine.deck_status.decks[i].load != 0));
    map(&mut app, PADS, json!({"type": "note", "ch": 0, "note": 36}), json!({"kind": "deck", "deck": "a", "control": "cue"}));
    map(&mut app, PADS, json!({"type": "note", "ch": 0, "note": 37}), json!({"kind": "deck", "deck": "a", "control": "hot_cue", "n": 1}));
    ui.frame(&mut app, 0.02);
    assert_eq!(app.player.as_ref().unwrap().midi_target(), Some(synth_track));
    let previewing = |a: &SoundApp| a.engine.deck_status.decks[0].previewing;
    let stopped = |a: &SoundApp| !a.engine.deck_status.decks[0].previewing;
    // CUE: plays while held, back and paused on the release.
    inject(&mut app, PADS, on(36));
    ui.until(&mut app, "the CUE preview", previewing);
    inject(&mut app, PADS, off(36));
    ui.until(&mut app, "the CUE released", stopped);
    // The hot cue: an empty pad sets it; pressed again it plays from it while held.
    inject(&mut app, PADS, on(37));
    inject(&mut app, PADS, off(37));
    ui.until(&mut app, "hot cue A set", |a| a.engine.deck_status.decks[0].hot[0].is_some());
    inject(&mut app, PADS, on(37));
    ui.until(&mut app, "the hot cue preview", previewing);
    inject(&mut app, PADS, off(37));
    ui.until(&mut app, "the hot cue released", stopped);
    // Mapped notes never played the armed synth.
    assert!(peak(&app, synth_track) < 1e-3, "a mapped note played the synth");
    // Held, then learning starts: let go (the release would teach nothing).
    inject(&mut app, PADS, on(36));
    ui.until(&mut app, "held (learn)", previewing);
    app.run("midi.learn", json!({"target": {"kind": "deck", "deck": "b", "control": "play"}})).unwrap();
    ui.until(&mut app, "let go when learning starts", stopped);
    app.run("midi.learn", json!({"cancel": true})).unwrap();
    inject(&mut app, PADS, off(36));
    // Held, then the app quits.
    inject(&mut app, PADS, on(37));
    ui.until(&mut app, "held (quit)", previewing);
    soundcraft_ui_egui::controller_layouts::shutdown(&mut app);
    ui.until(&mut app, "let go at quit", stopped);
    inject(&mut app, PADS, off(37));
    // Held, then the device is unplugged (noticed at the next device refresh): its release
    // never comes, so the hold is let go; Deck B's CUE, held from the other device, stays.
    map(&mut app, FLX4, json!({"type": "note", "ch": 1, "note": 0x0C}), json!({"kind": "deck", "deck": "b", "control": "cue"}));
    inject(&mut app, PADS, on(36));
    inject(&mut app, FLX4, MidiMessage::NoteOn { ch: 1, note: 0x0C, vel: 0x7F });
    ui.until(&mut app, "held (unplug)", |a| previewing(a) && a.engine.deck_status.decks[1].previewing);
    assert!(app.midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap().unplug(PADS));
    ui.frame(&mut app, 1.1);
    ui.until(&mut app, "let go when the device goes", stopped);
    for _ in 0..10 {
        ui.frame(&mut app, 0.02);
    }
    assert!(app.engine.deck_status.decks[1].previewing, "the other device's hold stays");
    // Removing its mapping lets go of what it holds.
    app.run("midi.unmap", json!({"target": {"kind": "deck", "deck": "b", "control": "cue"}})).unwrap();
    ui.until(&mut app, "let go when its mapping goes", |a| !a.engine.deck_status.decks[1].previewing);
}

#[test]
fn a_learned_deck_control_overrides_the_flx4_layout_on_the_same_message() {
    let (engine, synth_track) = synth();
    let mut ui = Ui::new();
    let mut app = app(engine);
    let (_dir, path) = track("flx4");
    app.run("deck.load", json!({"path": path, "deck": "b", "bpm": 120})).unwrap();
    ui.until(&mut app, "deck B loaded and the layout up", |a| {
        a.engine.deck_tracks[1].is_some() && a.engine.deck_status.decks[1].load != 0 && a.controllers.active()
    });
    // The FLX4's deck 1 CUE (note 0x0C), learned as Deck B's CUE.
    map(&mut app, FLX4, json!({"type": "note", "ch": 0, "note": 0x0C}), json!({"kind": "deck", "deck": "b", "control": "cue"}));
    inject(&mut app, FLX4, on(0x0C));
    ui.until(&mut app, "deck B previews", |a| a.engine.deck_status.decks[1].previewing);
    assert!(!app.engine.deck_status.decks[0].previewing && app.engine.deck_tracks[0].is_none(), "the layout's deck A got nothing");
    inject(&mut app, FLX4, off(0x0C));
    ui.until(&mut app, "deck B let go", |a| !a.engine.deck_status.decks[1].previewing);
    // A learned 14-bit fader: Learn MIDI on Deck B's fader, the FLX4's deck 1 fader moved (MSB
    // then LSB): both halves, absolute.
    app.run("midi.learn", json!({"target": {"kind": "deck", "deck": "b", "control": "fader"}})).unwrap();
    inject(&mut app, FLX4, cc(0x13, 0x40));
    inject(&mut app, FLX4, cc(0x33, 0x00));
    ui.until(&mut app, "learned", |a| a.ui.midi_mappings.len() == 2 && a.midi_learn.detect.is_none());
    let m = app.ui.midi_mappings.last().unwrap().clone();
    assert_eq!((m.matcher, m.lsb), (Matcher::Cc { ch: 0, cc: 0x13 }, Some(0x33)));
    app.ui.midi_mappings.last_mut().unwrap().pickup = false;
    let a_fader = app.engine.deck_status.decks[0].fader;
    inject(&mut app, FLX4, cc(0x13, 0x20));
    inject(&mut app, FLX4, cc(0x33, 0x00));
    ui.until(&mut app, "deck B's fader at a quarter", |a| (a.engine.deck_status.decks[1].fader - 0.25).abs() < 0.01);
    assert_eq!(app.engine.deck_status.decks[0].fader, a_fader, "deck A's fader (the layout's) didn't move");
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(200) {
        ui.frame(&mut app, 0.02);
        assert!(peak(&app, synth_track) < 1e-3, "a controller message played the synth");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_cue_held_by_a_key_is_let_go_by_the_logic_alone_with_the_decks_tab_not_drawn() {
    let mut ui = Ui::new();
    let mut app = app(soundcraft_engine::Engine::default());
    let (_dir, path) = track("keys");
    app.run("deck.load", json!({"path": path, "deck": "a", "bpm": 120})).unwrap();
    ui.until(&mut app, "deck A loaded", |a| a.engine.deck_tracks[0].is_some() && a.engine.deck_status.decks[0].load != 0);
    app.run("window.decks", json!({})).unwrap();
    let frame = |ui: &mut Ui, app: &mut SoundApp, events: Vec<egui::Event>, draw: bool| {
        ui.t += 0.02;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))),
            time: Some(ui.t),
            events,
            ..Default::default()
        };
        ui.ctx
            .run_ui(raw, |u| {
                app.logic(u.ctx());
                if draw {
                    app.ui(u);
                }
            })
            .textures_delta
            .clear();
    };
    let key = |pressed: bool| egui::Event::Key {
        key: egui::Key::Q,
        physical_key: Some(egui::Key::Q),
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    frame(&mut ui, &mut app, vec![key(true)], true);
    ui.until(&mut app, "Q holds Deck A's CUE", |a| a.engine.deck_status.decks[0].previewing);
    // Q comes up while only the logic runs (a covered window, or the tab not drawn).
    frame(&mut ui, &mut app, vec![key(false)], false);
    let start = Instant::now();
    while app.engine.deck_status.decks[0].previewing {
        assert!(start.elapsed() < Duration::from_secs(8), "never let go");
        frame(&mut ui, &mut app, vec![], false);
        std::thread::sleep(Duration::from_millis(5));
    }
}
