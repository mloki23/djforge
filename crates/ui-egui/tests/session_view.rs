//! View › Session: the Session window sets up its Live columns once, fills slots from the time
//! selection, a dropped file or the slot menu, launches by click and by key (queued, then playing
//! on the silent clock), stops, launches scenes, edits and clears slots, and its keys apply only
//! while it has focus.
//!
//! Files live in a temporary folder removed on drop; no analysis is set up (never mixforge).
#![allow(clippy::unwrap_used)]

use egui::{DroppedFile, Event, Key, Modifiers, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions};
use soundcraft_engine::Engine;
use soundcraft_model::{SlotMode, TrackId};
use soundcraft_ui_egui::{Services, SoundApp};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A temporary folder, removed with everything in it when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-session-view-{name}-{}", std::process::id()));
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

/// A stereo 48 kHz WAV of `secs` seconds in `dir`.
fn wav(dir: &Path, name: &str, secs: f64) -> PathBuf {
    let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; (secs * 48_000.0) as usize]; 2] };
    let path = dir.join(name);
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
    path
}

#[derive(Debug)]
struct Dropped(PathBuf);

impl DroppedFile for Dropped {
    fn path(&self) -> &Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Err("read by path".into())
    }
}

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

/// The app on the silent clock with a 30 s track at 120 bpm loaded beat-locked from bar 1.
fn harness(dir: &Path) -> (Harness<'static, SoundApp>, TrackId) {
    let mut e = Engine::default();
    let r = e.execute("dj.load_warped", &json!({"path": wav(dir, "track.wav", 30.0), "bpm": 120})).unwrap();
    let track = TrackId(r["track"].as_u64().unwrap());
    let player = soundcraft_playback::Player::silent(e.session_arc());
    let app = SoundApp::new(e, Some(player), Services::default());
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            let _ = app.take_dropped_files(&ctx);
            app.ui(ui);
        },
        app,
    );
    steps(&mut h, 4);
    (h, track)
}

/// Opens View › Session and gives it focus.
fn open(h: &mut Harness<'_, SoundApp>) {
    h.state_mut().run("window.session", json!({"value": true})).unwrap();
    steps(h, 4);
}

fn state(h: &mut Harness<'_, SoundApp>) -> Value {
    h.state_mut().engine.execute("live.state", &json!({})).unwrap()
}

/// Steps (in real time, for the silent clock) until `done` holds or 5 s pass; the last state.
fn until(h: &mut Harness<'_, SoundApp>, done: impl Fn(&Value) -> bool) -> Value {
    let start = std::time::Instant::now();
    loop {
        step(h);
        let s = state(h);
        if done(&s) || start.elapsed() > std::time::Duration::from_secs(5) {
            return s;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn press(h: &mut Harness<'_, SoundApp>, key: Key, modifiers: Modifiers) {
    let s = &mut h.state_mut().synthetic;
    s.push(Event::ModifiersChanged(modifiers));
    for pressed in [true, false] {
        s.push(Event::Key { key, physical_key: None, pressed, repeat: false, modifiers });
    }
    s.push(Event::ModifiersChanged(Modifiers::NONE));
    steps(h, 2);
}

/// Opens slot `label`'s menu and clicks `item`.
fn menu(h: &mut Harness<'_, SoundApp>, label: &str, item: &str) {
    h.get_by_label(label).click_secondary();
    steps(h, 3);
    h.get_by_label(item).click();
    steps(h, 3);
}

fn playing(s: &Value, column: usize) -> Option<u64> {
    s["columns"][column]["playing"]["slot"].as_u64()
}

#[test]
fn opening_the_view_sets_up_the_live_columns_once() {
    let dir = Scratch::new("open");
    let (mut h, _) = harness(&dir.0);
    let undo = h.state().engine.undo_history().len();
    open(&mut h);
    let s = h.state().engine.session();
    assert_eq!(s.live.columns.len(), 4);
    assert!(s.live.columns.iter().all(|c| s.track(c.track).is_some_and(|t| t.name.starts_with("Live "))));
    assert_eq!(h.state().engine.undo_history().len(), undo + 1, "one undo step");
    let _ = h.get_by_label("Launch Quantise");
    let _ = h.get_by_label("Stop All");
    let _ = h.get_by_label("Live 4 slot 8");
    // Closed and opened again: nothing more to set up.
    h.state_mut().run("window.session", json!({"value": false})).unwrap();
    open(&mut h);
    assert_eq!(h.state().engine.undo_history().len(), undo + 1);
}

#[test]
fn a_selection_fills_a_slot_that_launches_by_click_and_stops() {
    let dir = Scratch::new("click");
    let (mut h, track) = harness(&dir.0);
    open(&mut h);
    {
        let s = h.state_mut().engine.session_mut();
        s.edit.selected_tracks = vec![track];
        s.edit.selection = soundcraft_time::Range::new(0, 4 * 96_000);
    }
    menu(&mut h, "Live 1 slot 3", "Add Selection Here");
    let slot = h.state().engine.session().live.slot(0, 2).cloned().unwrap();
    assert_eq!((slot.length_beats, slot.mode), (16.0, SlotMode::Loop));
    // Quantise to a quarter note so the test waits half a second, not a bar.
    h.state_mut().run("live.quantise", json!({"value": "1/4"})).unwrap();
    h.get_by_label("Live 1 slot 3").click();
    let queued = until(&mut h, |s| s["columns"][0]["queued"].is_object() || playing(s, 0).is_some());
    assert!(h.state().is_playing(), "launching starts the transport");
    let s = until(&mut h, |s| playing(s, 0) == Some(2));
    assert_eq!(playing(&s, 0), Some(2), "queued {queued}, then {s}");
    // The ring follows the loop.
    assert!(s["columns"][0]["playing"]["loop_beat"].as_f64().is_some(), "{s}");
    h.get_by_label("■ Stop 1").click();
    let s = until(&mut h, |s| playing(s, 0).is_none());
    assert_eq!(playing(&s, 0), None, "{s}");
}

#[test]
fn keys_launch_stop_and_pick_columns_only_while_the_window_has_focus() {
    let dir = Scratch::new("keys");
    let (mut h, _) = harness(&dir.0);
    open(&mut h);
    let file = wav(&dir.0, "loop.wav", 4.0);
    for (c, r) in [(0, 0), (1, 1), (1, 3), (2, 3)] {
        h.state_mut().run("live.slot_from_file", json!({"column": c, "slot": r, "path": file, "bpm": 120})).unwrap();
    }
    h.state_mut().run("live.quantise", json!({"value": "none"})).unwrap();
    // Unfocused (a click elsewhere), the keys keep their arrangement meaning.
    h.state_mut().session_view.focused = false;
    press(&mut h, Key::ArrowRight, Modifiers::NONE);
    press(&mut h, Key::Num2, Modifiers::NONE);
    steps(&mut h, 5);
    assert_eq!(h.state().session_view.column, 0);
    assert!(!h.state().is_playing(), "2 launched nothing");
    // Focused: → selects Live 2, and 2 launches its slot 2.
    h.state_mut().session_view.focused = true;
    press(&mut h, Key::ArrowRight, Modifiers::NONE);
    assert_eq!(h.state().session_view.column, 1);
    press(&mut h, Key::Num2, Modifiers::NONE);
    let s = until(&mut h, |s| playing(s, 1) == Some(1));
    assert_eq!(playing(&s, 1), Some(1), "{s}");
    // An empty slot is refused, and says so.
    press(&mut h, Key::Num5, Modifiers::NONE);
    assert!(h.state().ui.status.contains("is empty"), "{}", h.state().ui.status);
    // Shift+4: scene 4 launches row 4 where a slot fills it (columns 2 and 3).
    press(&mut h, Key::Num4, Modifiers::SHIFT);
    let s = until(&mut h, |s| playing(s, 1) == Some(3) && playing(s, 2) == Some(3));
    assert_eq!((playing(&s, 0), playing(&s, 1), playing(&s, 2)), (None, Some(3), Some(3)), "{s}");
    // 0 stops the selected column; Esc stops everything.
    press(&mut h, Key::Num0, Modifiers::NONE);
    let s = until(&mut h, |s| playing(s, 1).is_none());
    assert_eq!((playing(&s, 1), playing(&s, 2)), (None, Some(3)), "{s}");
    press(&mut h, Key::Escape, Modifiers::NONE);
    let s = until(&mut h, |s| playing(s, 2).is_none());
    assert_eq!(playing(&s, 2), None, "{s}");
    // ← stops at the first column.
    for _ in 0..3 {
        press(&mut h, Key::ArrowLeft, Modifiers::NONE);
    }
    assert_eq!(h.state().session_view.column, 0);
}

#[test]
fn a_scene_button_launches_its_row_and_stop_all_stops_it() {
    let dir = Scratch::new("scene");
    let (mut h, _) = harness(&dir.0);
    open(&mut h);
    let file = wav(&dir.0, "loop.wav", 4.0);
    for c in [0, 3] {
        h.state_mut().run("live.slot_from_file", json!({"column": c, "slot": 6, "path": file, "bpm": 120})).unwrap();
    }
    h.state_mut().run("live.quantise", json!({"value": "none"})).unwrap();
    h.get_by_label("▶ 7").click();
    let s = until(&mut h, |s| playing(s, 0) == Some(6) && playing(s, 3) == Some(6));
    assert_eq!((playing(&s, 0), playing(&s, 1), playing(&s, 3)), (Some(6), None, Some(6)), "{s}");
    h.get_by_label("Stop All").click();
    let s = until(&mut h, |s| (0..4).all(|c| playing(s, c).is_none()));
    assert!((0..4).all(|c| playing(&s, c).is_none()), "{s}");
}

#[test]
fn a_dropped_file_loads_into_the_slot_under_the_pointer() {
    let dir = Scratch::new("drop");
    let (mut h, _) = harness(&dir.0);
    open(&mut h);
    let target = h.state().session_view.drop_targets.iter().find(|(_, c, r)| (*c, *r) == (2, 5)).map(|(rect, _, _)| *rect).unwrap();
    h.state_mut().synthetic.push(Event::PointerMoved(target.center()));
    steps(&mut h, 3);
    h.input_mut().dropped_files.push(Arc::new(Dropped(wav(&dir.0, "hit.wav", 0.5))));
    steps(&mut h, 3);
    let slot = h.state().engine.session().live.slot(2, 5).cloned().unwrap();
    assert_eq!((slot.name.as_str(), slot.mode), ("hit", SlotMode::OneShot));
    assert!(h.state().ui.status.contains("Live 3 slot 6"), "{}", h.state().ui.status);
}

#[test]
fn the_slot_menu_edits_and_clears() {
    let dir = Scratch::new("edit");
    let (mut h, _) = harness(&dir.0);
    open(&mut h);
    let file = wav(&dir.0, "loop.wav", 10.0);
    h.state_mut().run("live.slot_from_file", json!({"column": 1, "slot": 0, "path": file, "bpm": 120})).unwrap();
    menu(&mut h, "Live 2 slot 1", "Edit...");
    {
        let edit = h.state_mut().session_view.edit.as_mut().expect("the Edit Slot dialog is open");
        assert_eq!((edit.name.as_str(), edit.length_bars), ("loop", Some(5)));
        edit.name = "Drums".into();
        edit.length_bars = Some(2);
        edit.gain_db = -6.0;
    }
    steps(&mut h, 2);
    let undo = h.state().engine.undo_history().len();
    h.get_by_label("OK").click();
    steps(&mut h, 3);
    let slot = h.state().engine.session().live.slot(1, 0).cloned().unwrap();
    assert_eq!((slot.name.as_str(), slot.length_beats, slot.gain_db), ("Drums", 8.0, -6.0));
    assert_eq!(h.state().engine.undo_history().len(), undo + 1, "one undo step");
    assert!(h.state().session_view.edit.is_none());
    // A slot a file plays is mentioned when that file is cleared from the Clip List.
    let app = h.state_mut();
    assert!(app.dialogs.open_clear(&app.engine, vec![], vec![slot.source]));
    let Some(soundcraft_ui_egui::dialogs::Dialog::ClearClips { message, .. }) = &app.dialogs.open else { panic!("the Clear dialog is open") };
    assert!(message.contains("1 slot in the Session view (Live 2 slot 1) plays it"), "{message}");
    app.dialogs.open = None;
    steps(&mut h, 2);
    menu(&mut h, "Live 2 slot 1", "Clear");
    assert!(h.state().engine.session().live.slot(1, 0).is_none());
    assert!(h.state_mut().engine.undo());
    assert!(h.state().engine.session().live.slot(1, 0).is_some(), "clearing is undoable");
}
