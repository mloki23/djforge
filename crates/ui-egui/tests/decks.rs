//! The live decks through the app on the silent clock: a deck that starts while the arrangement
//! plays at half speed stops it the app's way (speed reset, transport stopped), deck loads land
//! on the player and come back in `decks.state`, and the Search entries for the decks are valid.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_ui_egui::{Services, SoundApp};

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn app(tag: &str) -> (SoundApp, Scratch, String) {
    // Its own folder per test: tests run in parallel.
    let dir = std::env::temp_dir().join(format!("djforge-ui-decks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 8 * 48_000]; 2] };
    let path = dir.join("track.wav");
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
    let e = Engine::default();
    let player = soundcraft_playback::Player::silent(e.session_arc());
    (SoundApp::new(e, Some(player), Services::default()), Scratch(dir), path.to_string_lossy().into_owned())
}

/// Runs the app's transport handling until `done` holds (or 5 s pass), then returns `decks.state`.
fn until(app: &mut SoundApp, done: impl Fn(&SoundApp, &Value) -> bool) -> Value {
    let start = std::time::Instant::now();
    loop {
        app.handle_transport(0.01);
        let state = app.engine.execute("decks.state", &json!({})).unwrap();
        if done(app, &state) || start.elapsed() > std::time::Duration::from_secs(5) {
            return state;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn a_deck_starting_stops_the_half_speed_arrangement_and_plays_at_its_own_rate() {
    let (mut app, _dir, path) = app("half-speed");
    app.engine.execute("deck.load", &json!({"path": path, "deck": "a", "bpm": 120})).unwrap();
    let st = until(&mut app, |_, s| {
        s["decks"][0]["track"].is_object() && s["decks"][0]["position"].as_f64() == Some(0.0) && !s["decks"][0]["loading"].as_bool().unwrap()
    });
    assert_eq!(st["decks"][0]["track"]["title"], "track", "{st}");
    app.engine.execute("transport.half_speed", &json!({})).unwrap();
    until(&mut app, |a, _| a.is_playing());
    assert!(app.is_playing());
    app.engine.execute("deck.play", &json!({"deck": "a"})).unwrap();
    let st = until(&mut app, |a, s| !a.is_playing() && s["decks"][0]["playing"] == true && s["decks"][0]["position"].as_f64() > Some(0.5));
    assert!(!app.is_playing(), "the arrangement stopped");
    assert_eq!(app.player.as_ref().unwrap().speed(), 1.0, "half-speed was reset");
    assert_eq!(st["decks"][0]["playing"], true, "{st}");
    // Arrangement play stops the deck again.
    app.engine.execute("transport.play", &json!({})).unwrap();
    let st = until(&mut app, |a, s| a.is_playing() && s["decks"][0]["playing"] == false);
    assert_eq!(st["decks"][0]["playing"], false);
    app.engine.execute("transport.stop", &json!({})).unwrap();
    app.handle_transport(0.01);
}

#[test]
fn every_deck_search_entry_runs_on_loaded_decks() {
    let palette = soundcraft_ui_egui::decks::palette();
    for (label, id, params) in &palette {
        let spec = soundcraft_engine::find_command(id).unwrap_or_else(|| panic!("{label}: {id} is not a command"));
        assert!(spec.id.starts_with("deck.") || spec.id.starts_with("library."), "{label}");
        assert!(params.is_null() || params.is_object(), "{label}");
    }
    assert!(palette.iter().filter(|e| e.1 == "deck.jog").all(|e| e.2["mode"] == "search"), "no nudge from Search: it would bend for a moment only");
    assert!(palette.iter().any(|e| e.1 == "deck.cue" && e.2["tap"] == true), "the cue is a tap");
    assert!(!palette.iter().any(|e| e.1 == "deck.hotcue" && e.2["action"] == "jump"), "no jump: stopped, it would play until a release");
    // Each entry with parameters runs on loaded decks (the ones without open a dialog).
    let (mut app, _dir, path) = app("search");
    for d in ["a", "b"] {
        app.engine.execute("deck.load", &json!({"path": path, "deck": d, "bpm": 120})).unwrap();
    }
    until(&mut app, |_, s| (0..2).all(|i| s["decks"][i]["track"].is_object() && !s["decks"][i]["loading"].as_bool().unwrap_or(true)));
    for (label, id, params) in palette.iter().filter(|e| !e.2.is_null()) {
        if let Err(e) = app.run(id, params.clone()) {
            panic!("{label}: {id} {params}: {e}");
        }
        app.handle_transport(0.01);
    }
    let mut labels: Vec<&String> = palette.iter().map(|e| &e.0).collect();
    labels.sort();
    let n = labels.len();
    labels.dedup();
    assert_eq!(labels.len(), n, "every label once");
}
