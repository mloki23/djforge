//! Live MIDI input from the app: the on-screen keyboard's Play mode sounds the target
//! instrument, and enabled inputs persist in the preferences. No real MIDI port is opened: the
//! devices are a `FakeSource`.

use egui::{Event, Id, PointerButton, Pos2, RawInput, Rect, vec2};
use serde_json::json;
use soundcraft_midi_io::{FakeSource, MidiClock};
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_ui_egui::midi_input::{MidiHost, MidiInputPref};
use soundcraft_ui_egui::{Services, SoundApp, UiState};

fn frame(ctx: &egui::Context, app: &mut SoundApp, events: Vec<Event>) {
    let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
    ctx.run_ui(raw, |ui| {
        app.logic(ui.ctx());
        app.ui(ui);
    })
    .textures_delta
    .clear();
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Default::default() }
}

/// An app on a silent player (no audio device) with one record-armed synth track.
fn app() -> (SoundApp, soundcraft_model::TrackId) {
    let mut engine = soundcraft_engine::Engine::default();
    let t = {
        let s = engine.session_mut();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
        let tr = s.track_mut(t).unwrap();
        tr.instrument = Some(Insert::new("subtractive_synth"));
        tr.mixer.record_arm = true;
        t
    };
    let player = soundcraft_playback::Player::silent(engine.session_arc());
    (SoundApp::new(engine, Some(player), Services::default()), t)
}

fn peak(app: &SoundApp, t: soundcraft_model::TrackId) -> f32 {
    app.player.as_ref().unwrap().meters().tracks.get(&t).map_or(0.0, |m| m.peak[0].max(m.peak[1]))
}

#[test]
fn the_keyboard_play_mode_sounds_the_target_instrument() {
    let (mut app, t) = app();
    app.ui.show_midi_keyboard = true;
    app.ui.midi_keyboard_play = true;
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&ctx, &mut app, vec![]);
    }
    assert_eq!(app.player.as_ref().unwrap().midi_target(), Some(t), "the armed synth track is the target");
    let keys: Rect = ctx.data(|d| d.get_temp(Id::new("soundcraft-midi-keyboard"))).expect("the keyboard is drawn");
    // Middle of the first white key's lower half (C3, no black key over it).
    let at = Pos2::new(keys.min.x + 4.0, keys.max.y - 6.0);
    frame(&ctx, &mut app, vec![Event::PointerMoved(at)]);
    frame(&ctx, &mut app, vec![button(at, true)]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while peak(&app, t) < 1e-3 {
        assert!(std::time::Instant::now() < deadline, "the synth never sounded");
        frame(&ctx, &mut app, vec![]);
        std::thread::yield_now();
    }
    let held: Option<u8> = ctx.data(|d| d.get_temp(Id::new("soundcraft-midi-keyboard-held")));
    assert_eq!(held, Some(48));
    frame(&ctx, &mut app, vec![button(at, false)]);
    let held: Option<u8> = ctx.data(|d| d.get_temp(Id::new("soundcraft-midi-keyboard-held")));
    assert_eq!(held, None, "released with the button");
}

#[test]
fn an_enabled_input_persists_in_the_preferences_and_opens() {
    let (mut app, _) = app();
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    src.plug("Pads");
    src.plug("Keys");
    app.midi_host = Some(MidiHost::new(Box::new(src)));
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    let ports = app.run("midi.inputs", json!({})).unwrap();
    assert_eq!(ports["ports"].as_array().unwrap().len(), 2);
    assert!(ports["ports"].as_array().unwrap().iter().all(|p| p["open"] == false && p["enabled"] == false), "nothing set: nothing opens");
    let r = app.run("midi.input_enable", json!({"name": "Keys", "enabled": true})).unwrap();
    assert_eq!((r["enabled"].as_bool(), r["open"].as_bool()), (Some(true), Some(true)));
    assert!(app.run("midi.input_enable", json!({"name": "Nope", "enabled": true})).is_err());
    // Saved and read back like the app's preferences file, in a scratch folder.
    let dir = std::env::temp_dir().join(format!("djforge-midi-prefs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("prefs.json");
    std::fs::write(&path, serde_json::to_string(&app.ui).unwrap()).unwrap();
    let back: UiState = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(back.midi_inputs.len(), 1);
    assert_eq!(back.midi_inputs[0].raw_name, "Keys");
    // A new run with those preferences opens Keys (only) on its first frame.
    let (mut next, _) = app_with(back);
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    src.plug("Pads");
    src.plug("Keys");
    next.midi_host = Some(MidiHost::new(Box::new(src)));
    frame(&ctx, &mut next, vec![]);
    let src = next.midi_host.as_ref().unwrap().source();
    assert!(src.is_open("Keys") && !src.is_open("Pads"));
    // Disabling closes it and drops it from the preferences.
    next.run("midi.input_enable", json!({"name": "Keys", "enabled": false})).unwrap();
    assert!(!next.midi_host.as_ref().unwrap().source().is_open("Keys"));
    assert_eq!(next.ui.midi_inputs, Vec::<MidiInputPref>::new());
}

fn app_with(ui: UiState) -> (SoundApp, soundcraft_model::TrackId) {
    let (mut a, t) = app();
    a.ui = ui;
    (a, t)
}

#[test]
fn the_target_override_and_panic_commands() {
    let (mut app, t) = app();
    let audio = app.engine.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    assert!(app.run("midi.target", json!({"track": audio.0})).is_err(), "an audio track is no target");
    let r = app.run("midi.target", json!({"track": t.0})).unwrap();
    assert_eq!(r["target"], json!(t.0));
    let r = app.run("midi.target", json!({"track": null})).unwrap();
    assert_eq!((r["override"].clone(), r["target"].clone()), (json!(null), json!(t.0)), "cleared: the armed track");
    app.run("midi.panic", json!({})).unwrap();
}
