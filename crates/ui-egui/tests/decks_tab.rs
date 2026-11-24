//! Window › Decks through the real UI on the silent clock: the tab opens, the library browser
//! lists a fixture mixforge library (built here, in a temporary folder; never the real one), a
//! double-click and Shift+Right load its tracks, and the deck controls drive the decks: Play,
//! the CDJ cue held and released, a hot cue pad set then jumped to, a beat loop, Sync and an EQ
//! knob. Space plays the focused deck and never the arrangement.
#![allow(clippy::unwrap_used)]

use egui::{Key, Modifiers, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions};
use soundcraft_engine::Engine;
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};
use std::path::{Path, PathBuf};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-decks-tab-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A stereo 48 kHz WAV of `secs` seconds: a kick-like pulse on every beat at `bpm` over a tone.
fn wav(dir: &Path, name: &str, secs: f64, bpm: f64) -> String {
    let n = (secs * 48_000.0) as usize;
    let beat = 60.0 / bpm * 48_000.0;
    let c: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64;
            let ph = (t % beat) / beat;
            let kick = (-(ph * 30.0)).exp() * (t * 0.012).sin();
            (0.5 * kick + 0.1 * (t * 0.09).sin()) as f32
        })
        .collect();
    let buf = AudioBuffer { sample_rate: 48_000, channels: vec![c.clone(), c] };
    let path = dir.join(name);
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
    path.to_string_lossy().into_owned()
}

/// mixforge's schema with a fresh row for each track (title, bpm, key).
fn library(dir: &Path, tracks: &[(&str, &str, f64, &str, f64)]) -> PathBuf {
    let db = dir.join("library.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, version INTEGER, data TEXT);
         CREATE TABLE IF NOT EXISTS structure (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, data TEXT);",
    )
    .unwrap();
    for (path, title, bpm, camelot, secs) in tracks {
        let m = std::fs::metadata(path).unwrap();
        let mtime = m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let data = json!({"path": path, "title": title, "artist": "Test Artist", "bpm": bpm, "first_downbeat": 0.0, "duration": secs,
            "audio_end": secs, "camelot": camelot, "key": "A minor", "bars": (secs / (240.0 / bpm)) as i64, "constant_tempo": true});
        c.execute("INSERT OR REPLACE INTO tracks VALUES (?1, ?2, ?3, 9, ?4)", rusqlite::params![path, m.len() as i64, mtime, data.to_string()])
            .unwrap();
    }
    db
}

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().raw_input_hook(&mut raw);
    *h.input_mut() = raw;
    h.step();
    // The silent clock runs in real time.
    std::thread::sleep(std::time::Duration::from_millis(4));
}

fn steps(h: &mut Harness<'_, SoundApp>, n: usize) {
    for _ in 0..n {
        step(h);
    }
}

fn state(h: &mut Harness<'_, SoundApp>) -> Value {
    h.state_mut().engine.execute("decks.state", &json!({})).unwrap()
}

/// Steps until `done` holds for `decks.state` (or 8 s pass); returns it.
fn until(h: &mut Harness<'_, SoundApp>, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let start = std::time::Instant::now();
    loop {
        step(h);
        let s = state(h);
        if done(&s) {
            return s;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "{what}: {s}; status: {}; selected {:?}",
            h.state().ui.status,
            h.state().decks_view.browser.selected
        );
    }
}

fn harness(dir: &Path, secs: f64) -> Harness<'static, SoundApp> {
    let a = wav(dir, "alpha.wav", secs, 120.0);
    let b = wav(dir, "beta.wav", secs, 126.0);
    let db = library(dir, &[(&a, "Alpha", 120.0, "8A", secs), (&b, "Beta", 126.0, "9A", secs)]);
    let mut e = Engine::default();
    e.set_library(db);
    let player = soundcraft_playback::Player::silent(e.session_arc());
    let app = SoundApp::new(e, Some(player), Services::default());
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).with_step_dt(1.0 / 60.0).with_max_steps(1_000_000).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    steps(&mut h, 3);
    h.state_mut().run("window.decks", json!({})).unwrap();
    steps(&mut h, 3);
    h
}

/// Steps until the library browser lists `title`.
fn wait_row(h: &mut Harness<'_, SoundApp>, title: &str) {
    let start = std::time::Instant::now();
    while h.query_by_label(&format!("Track {title}")).is_none() {
        assert!(start.elapsed() < std::time::Duration::from_secs(8), "no {title} row");
        step(h);
    }
}

fn press(h: &mut Harness<'_, SoundApp>, label: &str) {
    let c = h.get_by_label(label).rect().center();
    h.drag_at(c);
    steps(h, 2);
}

fn release(h: &mut Harness<'_, SoundApp>, label: &str) {
    let c = h.get_by_label(label).rect().center();
    h.drop_at(c);
    steps(h, 2);
}

fn double_click(h: &mut Harness<'_, SoundApp>, label: &str) {
    let at = h.get_by_label(label).rect().center();
    for pressed in [true, false, true, false] {
        h.event(egui::Event::PointerMoved(at));
        h.event(egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
        step(h);
    }
    steps(h, 2);
}

fn click(h: &mut Harness<'_, SoundApp>, label: &str) {
    h.get_by_label(label).click();
    steps(h, 2);
}

#[test]
fn the_decks_tab_loads_from_the_library_and_its_controls_drive_the_decks() {
    let dir = Scratch::new("controls");
    let mut h = harness(&dir.0, 40.0);
    assert_eq!(h.state().ui.window, MainWindow::Decks);
    // The library browser lists the fixture's rows; a double-click loads the first free deck.
    wait_row(&mut h, "Alpha");
    wait_row(&mut h, "Beta");
    double_click(&mut h, "Track Alpha");
    let st = until(&mut h, "Alpha on Deck A", |s| s["decks"][0]["track"]["title"] == "alpha" && s["decks"][0]["loading"] == false);
    assert_eq!(st["decks"][0]["track"]["grid"], "library", "{st}");
    assert!(h.state().ui.status.contains("Deck A: loaded alpha"), "{}", h.state().ui.status);
    // Shift+Right loads the selected row on Deck B.
    h.get_by_label("Track Beta").click();
    steps(&mut h, 2);
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowRight);
    until(&mut h, "Beta on Deck B", |s| s["decks"][1]["track"]["title"] == "beta" && s["decks"][1]["loading"] == false);

    // Play, then Space pauses the focused deck; the arrangement never starts.
    click(&mut h, "Deck A Play");
    until(&mut h, "A plays", |s| s["decks"][0]["playing"] == true);
    h.key_press(Key::Space);
    until(&mut h, "Space paused A", |s| s["decks"][0]["playing"] == false);
    assert!(!h.state().is_playing(), "Space didn't start the arrangement");
    h.key_press(Key::Space);
    until(&mut h, "Space played A", |s| s["decks"][0]["playing"] == true);
    assert!(!h.state().is_playing());
    h.key_press(Key::Space);
    until(&mut h, "paused", |s| s["decks"][0]["playing"] == false);

    // The CDJ cue: held while stopped it previews from the cue point; released it goes back.
    h.state_mut().run("deck.seek", json!({"deck": "a", "at": 8.0})).unwrap();
    until(&mut h, "seeked", |s| (s["decks"][0]["position"].as_f64().unwrap() - 8.0).abs() < 0.01);
    press(&mut h, "Deck A Cue");
    let st = until(&mut h, "cue previews", |s| s["decks"][0]["previewing"] == true && s["decks"][0]["position"].as_f64().unwrap() > 8.05);
    let cue = st["decks"][0]["cue"].as_f64().unwrap();
    assert!((cue - 8.0).abs() < 0.01, "the cue was set at the playhead (on the beat): {st}");
    release(&mut h, "Deck A Cue");
    until(&mut h, "back to the cue", |s| {
        s["decks"][0]["previewing"] == false && s["decks"][0]["playing"] == false && (s["decks"][0]["position"].as_f64().unwrap() - cue).abs() < 0.01
    });

    // A hot cue pad: an empty one (H; A–D hold mixforge's mix points) sets it here; once set, a
    // press jumps there.
    assert!(state(&mut h)["decks"][0]["hot_cues"][7].is_null());
    click(&mut h, "Deck A Hot Cue H");
    let st = until(&mut h, "hot cue H set", |s| s["decks"][0]["hot_cues"][7].is_number());
    let hot = st["decks"][0]["hot_cues"][7].as_f64().unwrap();
    h.state_mut().run("deck.seek", json!({"deck": "a", "at": 20.0})).unwrap();
    until(&mut h, "moved on", |s| (s["decks"][0]["position"].as_f64().unwrap() - 20.0).abs() < 0.01);
    click(&mut h, "Deck A Hot Cue H");
    until(&mut h, "jumped to hot cue H", |s| {
        (s["decks"][0]["position"].as_f64().unwrap() - hot).abs() < 0.05 && s["decks"][0]["previewing"] == false
    });

    // A beat loop, then Sync, then an EQ knob.
    click(&mut h, "Deck A Play");
    until(&mut h, "plays again", |s| s["decks"][0]["playing"] == true);
    click(&mut h, "Deck A Loop 4");
    let st = until(&mut h, "a 4-beat loop", |s| s["decks"][0]["loop"]["on"] == true);
    let len = st["decks"][0]["loop"]["out"].as_f64().unwrap() - st["decks"][0]["loop"]["in"].as_f64().unwrap();
    assert!((len - 2.0).abs() < 0.01, "4 beats at 120 bpm: {st}");
    click(&mut h, "Deck B Sync");
    until(&mut h, "B synced", |s| s["decks"][1]["synced"] == true);
    let knob = h.get_by_label("Deck A EQ HI").rect().center();
    h.drag_at(knob);
    step(&mut h);
    h.hover_at(knob + vec2(0.0, 60.0));
    step(&mut h);
    h.drop_at(knob + vec2(0.0, 60.0));
    let st = until(&mut h, "EQ high cut", |s| s["decks"][0]["channel"]["eq_db"][2].as_f64().unwrap() < -3.0);
    assert_eq!(st["decks"][0]["channel"]["eq_db"][0].as_f64(), Some(0.0), "only the high band moved: {st}");
    click(&mut h, "Deck A EQ LOW Kill");
    until(&mut h, "low killed", |s| s["decks"][0]["channel"]["eq_db"][0].as_f64().unwrap() <= -80.0);
    assert!(!h.state().is_playing(), "the arrangement stayed stopped throughout");
}

#[test]
fn stray_chords_never_stop_the_decks_and_a_held_cue_always_lets_go() {
    let dir = Scratch::new("safety");
    let mut h = harness(&dir.0, 40.0);
    wait_row(&mut h, "Alpha");
    for (name, deck) in [("alpha.wav", "a"), ("beta.wav", "b")] {
        let path = dir.0.join(name).to_string_lossy().into_owned();
        h.state_mut().run("deck.load", json!({"path": path, "deck": deck})).unwrap();
    }
    until(&mut h, "both loaded", |s| s["decks"][0]["track"].is_object() && s["decks"][1]["track"].is_object() && s["decks"][1]["loading"] == false);

    // Enter (and a double-click) with both decks loaded is refused: Deck B's track stays.
    h.get_by_label("Track Alpha").click();
    steps(&mut h, 2);
    h.key_press(Key::Enter);
    steps(&mut h, 4);
    assert!(h.state().ui.status.contains("Both decks have a track"), "{}", h.state().ui.status);
    assert_eq!(state(&mut h)["decks"][1]["track"]["title"], "beta");

    // The pitch fader: dragging it up slows the deck down, as on a CDJ.
    let fader = h.get_by_label("Deck A Pitch").rect();
    h.drag_at(fader.center());
    step(&mut h);
    h.hover_at(fader.center() - vec2(0.0, fader.height() * 0.25));
    step(&mut h);
    h.drop_at(fader.center() - vec2(0.0, fader.height() * 0.25));
    let st = until(&mut h, "pitched down", |s| s["decks"][0]["pitch"].as_f64().unwrap() < -1.0);
    assert!(st["decks"][0]["rate"].as_f64().unwrap() < 1.0 || st["decks"][0]["playing"] == false, "{st}");

    // With Deck A playing, chords that would start the arrangement do nothing in the Decks tab.
    click(&mut h, "Deck A Play");
    until(&mut h, "A plays", |s| s["decks"][0]["playing"] == true);
    h.key_press_modifiers(Modifiers::ALT, Key::OpenBracket);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Space);
    h.key_press(Key::F12);
    steps(&mut h, 6);
    assert!(!h.state().is_playing(), "the arrangement stayed stopped");
    assert_eq!(state(&mut h)["decks"][0]["playing"], true, "Deck A still plays");

    // W holds Deck B's CUE: a preview while held, back to the cue on release.
    h.key_down(Key::W);
    until(&mut h, "W previews B", |s| s["decks"][1]["previewing"] == true);
    h.key_up(Key::W);
    until(&mut h, "W let go", |s| s["decks"][1]["previewing"] == false && s["decks"][1]["playing"] == false);

    // CUE held with the pointer, then the window changes (a menu, Cmd+=): the hold lets go.
    press(&mut h, "Deck B Cue");
    until(&mut h, "pointer previews B", |s| s["decks"][1]["previewing"] == true);
    h.state_mut().run("window.edit", json!({})).unwrap();
    until(&mut h, "the hold let go off the tab", |s| s["decks"][1]["previewing"] == false);
    assert_eq!(state(&mut h)["decks"][0]["playing"], true, "Deck A played on throughout");
}

/// The Decks tab's cost per frame with two 10-minute tracks playing (run with
/// `cargo test -p soundcraft-ui-egui --test decks_tab -- --ignored --nocapture`).
#[test]
#[ignore]
fn decks_tab_frame_cost_with_two_ten_minute_tracks_playing() {
    let dir = Scratch::new("cost");
    let mut h = harness(&dir.0, 600.0);
    wait_row(&mut h, "Alpha");
    for (title, deck) in [("Alpha", 0), ("Beta", 1)] {
        let path = dir.0.join(if deck == 0 { "alpha.wav" } else { "beta.wav" }).to_string_lossy().into_owned();
        h.state_mut().run("deck.load", json!({"path": path, "deck": if deck == 0 { "a" } else { "b" }})).unwrap();
        until(&mut h, title, |s| s["decks"][deck]["track"].is_object() && s["decks"][deck]["loading"] == false);
    }
    // Wait for the frequency bands so the coloured path is what is timed.
    let start = std::time::Instant::now();
    while !h.state().engine.deck_tracks.iter().flatten().all(|t| t.source.bands_ready()) && start.elapsed().as_secs() < 120 {
        step(&mut h);
    }
    h.state_mut().run("deck.play", json!({"deck": "a"})).unwrap();
    h.state_mut().run("deck.play", json!({"deck": "b"})).unwrap();
    h.state_mut().run("deck.sync", json!({"deck": "b", "on": true})).unwrap();
    steps(&mut h, 30);
    let frames = 300;
    let mut times = Vec::with_capacity(frames);
    let before = h.state().decks_view.columns_computed();
    for _ in 0..frames {
        let mut raw = std::mem::take(h.input_mut());
        h.state_mut().raw_input_hook(&mut raw);
        *h.input_mut() = raw;
        let t = std::time::Instant::now();
        h.step();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        std::thread::sleep(std::time::Duration::from_millis(12));
    }
    let cols = h.state().decks_view.columns_computed() - before;
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / frames as f64;
    let p95 = times[frames * 95 / 100];
    println!(
        "Decks tab, two 10-min tracks playing: mean {mean:.2} ms/frame, median {:.2}, p95 {p95:.2}, max {:.2}; {:.1} waveform columns computed per frame",
        times[frames / 2],
        times[frames - 1],
        cols as f64 / frames as f64
    );
}
