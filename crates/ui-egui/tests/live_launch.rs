//! Session view launches through the app on the silent clock (no audio device): a launch while
//! stopped starts playback and lands on the first bar line, one while playing is queued for its
//! boundary and plays from exactly there, scenes launch only filled slots, and stops stop.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_time::TICKS_PER_QUARTER;
use soundcraft_ui_egui::{Services, SoundApp};

/// A temporary folder, removed with everything in it when dropped.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An app on the silent clock with Live columns, a 4 s loop (120 bpm) in slots 0 and 2 of
/// column 0 and slot 2 of column 1, and launch quantise `quantise`.
fn app(name: &str, quantise: &str) -> (SoundApp, Scratch) {
    let dir = std::env::temp_dir().join(format!("djforge-live-launch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = Scratch(dir.clone());
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 4 * 48_000]; 2] };
    let path = dir.join("loop.wav");
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
    let mut e = Engine::default();
    e.execute("live.setup", &json!({})).unwrap();
    for (column, slot) in [(0, 0), (0, 2), (1, 2)] {
        e.execute("live.slot_from_file", &json!({"column": column, "slot": slot, "path": path, "bpm": 120})).unwrap();
    }
    e.execute("live.quantise", &json!({"value": quantise})).unwrap();
    let player = soundcraft_playback::Player::silent(e.session_arc());
    let app = SoundApp::new(e, Some(player), Services::default());
    (app, scratch)
}

/// Runs the app's transport handling until `done` holds (or 5 s pass), then returns `live.state`.
fn until(app: &mut SoundApp, done: impl Fn(&Value) -> bool) -> Value {
    let start = std::time::Instant::now();
    loop {
        app.handle_transport(0.01);
        let state = app.engine.execute("live.state", &json!({})).unwrap();
        if done(&state) || start.elapsed() > std::time::Duration::from_secs(5) {
            return state;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn on_grid(app: &SoundApp, at: i64, beats: i64) -> bool {
    let s = app.engine.session();
    let tick = s.tempo.samples_to_ticks_f(at, s.sample_rate);
    let k = (tick / (beats * TICKS_PER_QUARTER) as f64).round() as i64;
    s.tempo.tick_to_samples(k * beats * TICKS_PER_QUARTER, s.sample_rate) == at
}

#[test]
fn a_launch_while_stopped_starts_playback_and_lands_on_the_first_bar() {
    let (mut app, _dir) = app("stopped", "1");
    app.engine.execute("edit.select", &json!({"start": 1_000, "end": 1_000})).unwrap();
    let r = app.engine.execute("live.launch", &json!({"column": 0, "slot": 2})).unwrap();
    assert_eq!(r["starts_transport"], true);
    let state = until(&mut app, |s| s["columns"][0]["playing"].is_object());
    assert!(app.is_playing(), "launching starts the transport");
    // Bar 2: the first bar line at or after the play start (sample 1000).
    assert_eq!(state["columns"][0]["playing"]["slot"], 2, "{state}");
    assert_eq!(state["columns"][0]["playing"]["start"], 96_000, "{state}");
}

#[test]
fn a_launch_while_playing_is_queued_then_plays_from_its_boundary() {
    let (mut app, _dir) = app("playing", "1/4");
    app.engine.execute("transport.play", &json!({})).unwrap();
    until(&mut app, |s| s["transport"]["position"].as_i64().is_some_and(|p| p > 0));
    app.engine.execute("live.launch", &json!({"column": 0, "slot": 0})).unwrap();
    let queued = until(&mut app, |s| s["columns"][0]["queued"].is_object());
    let at = queued["columns"][0]["queued"]["at"].as_i64().unwrap();
    assert_eq!(queued["columns"][0]["queued"]["slot"], 0, "{queued}");
    assert!(on_grid(&app, at, 1), "the launch waits for a quarter note: {at}");
    let playing = until(&mut app, |s| s["columns"][0]["playing"].is_object());
    assert_eq!(playing["columns"][0]["playing"]["start"].as_i64(), Some(at), "it starts exactly on its boundary: {playing}");
    assert!(playing["columns"][0]["queued"].is_null(), "{playing}");
    let pos = playing["columns"][0]["playing"]["loop_beat"].as_f64().unwrap();
    assert!((0.0..16.0).contains(&pos), "{playing}");
    // A scene launches its row only where a slot fills it; Stop All stops them all.
    app.engine.execute("live.scene", &json!({"row": 2})).unwrap();
    let scene = until(&mut app, |s| (0..2).all(|c| s["columns"][c]["playing"]["slot"] == 2));
    assert!((2..4).all(|c| scene["columns"][c]["playing"].is_null()), "{scene}");
    assert_eq!(scene["columns"][0]["playing"]["start"], scene["columns"][1]["playing"]["start"], "together: {scene}");
    app.engine.execute("live.stop_all", &json!({})).unwrap();
    let stopped = until(&mut app, |s| (0..4).all(|c| s["columns"][c]["playing"].is_null()));
    assert!((0..4).all(|c| stopped["columns"][c]["playing"].is_null()), "{stopped}");
    // The transport stopping stops everything at once.
    app.engine.execute("live.launch", &json!({"column": 1, "slot": 2})).unwrap();
    until(&mut app, |s| s["columns"][1]["playing"].is_object());
    app.engine.execute("transport.stop", &json!({})).unwrap();
    let after = until(&mut app, |s| s["columns"][1]["playing"].is_null());
    assert!(after["columns"][1]["playing"].is_null(), "{after}");
}

#[test]
fn a_launch_while_stopped_over_a_selection_plays_on_past_its_end() {
    let (mut app, _dir) = app("selection", "1");
    app.engine.execute("edit.select", &json!({"start": 1_000, "end": 20_000})).unwrap();
    // Two launches in one frame: one start of the transport, one boundary.
    app.engine.execute("live.launch", &json!({"column": 0, "slot": 2})).unwrap();
    app.engine.execute("live.launch", &json!({"column": 1, "slot": 2})).unwrap();
    let state = until(&mut app, |s| (0..2).all(|c| s["columns"][c]["playing"].is_object()));
    // The first bar line at or after the selection start, not the selection's end or a pre-roll.
    assert_eq!(state["columns"][0]["playing"]["start"], 96_000, "{state}");
    assert_eq!(state["columns"][1]["playing"]["start"], 96_000, "together: {state}");
    assert!(app.is_playing() && app.engine.transport.position > 20_000, "open-ended: still playing past the selection");
}

#[test]
fn a_launch_past_the_transport_loop_lands_inside_it() {
    let (mut app, _dir) = app("loop", "1");
    // 240 bpm: a bar is 48 000 samples; loop a bar and a half (0..72 000).
    app.engine.execute("event.tempo", &json!({"bpm": 240.0, "at": 0})).unwrap();
    app.engine.execute("options.loop_playback", &json!({"value": true})).unwrap();
    app.engine.execute("edit.select", &json!({"start": 0, "end": 72_000})).unwrap();
    app.engine.execute("transport.play", &json!({})).unwrap();
    let past = until(&mut app, |s| s["transport"]["position"].as_i64().is_some_and(|p| p > 50_000));
    assert!(past["transport"]["position"].as_i64().is_some_and(|p| p < 64_000), "launch inside the last half bar: {past}");
    // The next bar line (96 000) is past the loop's end: the launch waits for the wrap and starts
    // on the loop's first bar line, not half a bar into it.
    app.engine.execute("live.launch", &json!({"column": 0, "slot": 0})).unwrap();
    let state = until(&mut app, |s| s["columns"][0]["playing"].is_object());
    assert_eq!(state["columns"][0]["playing"]["start"], 0, "{state}");
    // A quantise with no line inside the loop is refused, and says why.
    app.engine.execute("live.quantise", &json!({"value": 4})).unwrap();
    app.engine.execute("edit.select", &json!({"start": 48_000, "end": 144_000})).unwrap();
    app.engine.execute("transport.stop", &json!({})).unwrap();
    app.handle_transport(0.01);
    app.engine.execute("transport.play", &json!({})).unwrap();
    until(&mut app, |s| s["transport"]["position"].as_i64().is_some_and(|p| p > 50_000));
    app.engine.execute("live.launch", &json!({"column": 1, "slot": 2})).unwrap();
    app.handle_transport(0.01);
    assert!(app.ui.status.contains("inside the loop"), "{}", app.ui.status);
}
