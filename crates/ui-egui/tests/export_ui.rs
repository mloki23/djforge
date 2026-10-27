//! DJ > Export Set... asks which targets to write (Rekordbox, Mixxx, and stem files when a track
//! of the set is split), then runs the export on its own thread behind a progress window with
//! Cancel, so the app keeps drawing. Every export here goes into a scratch folder, with a fake
//! analysis and a fake Mixxx runner: nothing runs mixforge, touches Mixxx, Rekordbox, Marty's
//! folders or the real stem cache, and no stem file is written (no ffmpeg needed).

use egui::vec2;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_engine::cmd::{ExportJob, MixxxRunner, PrepAnalyse};
use soundcraft_engine::model::{ClipId, StemSet};
use soundcraft_ui_egui::dialogs::Dialog;
use soundcraft_ui_egui::{Services, SoundApp, menus};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const MP3: &str = "../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3";
const WAIT: &str = "Wait for the export to finish (or cancel it)";

/// A test's own scratch folder, removed when the test ends (passed or failed).
struct Scratch(PathBuf);

impl Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("djforge-ui-export-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

/// A two-track set at 120 bpm in a session named "test set", from MP3 copies in `dir/lib`: a.mp3
/// on bar 1 mixing into b.mp3 (on bar 2) on bar 3. Returns the engine and the clips.
fn set(dir: &Path) -> (Engine, [ClipId; 2]) {
    let lib = dir.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR")).join(MP3);
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let mut clips = Vec::new();
    for (name, bar) in [("a.mp3", 1), ("b.mp3", 2)] {
        let path = lib.join(name);
        std::fs::copy(&mp3, &path).unwrap();
        let beats: Vec<f64> = (0..21).map(|k| k as f64 * 0.1).collect();
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        clips.push(ClipId(r["clip"].as_u64().unwrap()));
    }
    e.execute("dj.transition", &json!({"out_clip": clips[0].0, "in_clip": clips[1].0, "entry_bar": 3, "build_bars": 1, "out_bars": 1})).unwrap();
    (e, [clips[0], clips[1]])
}

/// A Mixxx runner that records the set names it is given and says it wrote the playlist.
fn fake_mixxx(calls: &Arc<Mutex<Vec<String>>>) -> MixxxRunner {
    let calls = Arc::clone(calls);
    Arc::new(move |plan: &Value, set: &str| {
        calls.lock().unwrap().push(set.to_string());
        Ok(json!({"playlist": format!("mixforge · {set}"), "tracks": plan["tracks"]}))
    })
}

fn analysis() -> PrepAnalyse {
    Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(json!({"camelot": "8A", "bpm": 120.0})))
}

/// An analysis that says when it is reached, then waits for a go-ahead.
fn gated() -> (PrepAnalyse, mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (reached_tx, reached) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let analyse: PrepAnalyse = Arc::new(move |_: &str, _: &dyn Fn() -> bool| {
        let _ = reached_tx.lock().unwrap().send(());
        let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60));
        Ok(json!({"camelot": "8A", "bpm": 120.0}))
    });
    (analyse, reached, go)
}

/// The app on set `e`, exporting into `dir/prepped` with `analyse` and a fake Mixxx runner whose
/// calls go into `calls`.
fn harness(mut e: Engine, dir: &Path, analyse: PrepAnalyse, calls: &Arc<Mutex<Vec<String>>>) -> Harness<'static, SoundApp> {
    e.set_mixxx_runner(fake_mixxx(calls));
    let mut app = SoundApp::new(e, None, Services::default());
    app.prep_root = Some(dir.join("prepped"));
    app.prep_analyse = analyse;
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).with_max_steps(1_000).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(5);
    h
}

/// Steps the app until its export has ended.
fn step_until_exported(h: &mut Harness<'_, SoundApp>) {
    let until = Instant::now() + Duration::from_secs(600);
    while h.state().export.is_some() {
        assert!(Instant::now() < until, "the export never ended");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_dialog(h: &mut Harness<'_, SoundApp>) {
    menus::invoke_menu(h.state_mut(), "dj.export_set", "DJ > Export Set...");
    // The window sizes itself over its first frames, so its buttons settle before a click.
    h.run_steps(6);
}

fn disabled(h: &Harness<'_, SoundApp>, label: &str) -> bool {
    h.get_by_label(label).accesskit_node().is_disabled()
}

const STEMS_LABEL: &str = "Include stem files (.stem.mp4)";

#[test]
fn the_menu_opens_the_dialog_and_export_writes_rekordbox_and_mixxx() {
    assert!(menus::ui_aliases().iter().any(|(path, id)| *path == "DJ > Export Set..." && *id == "dj.export_set"));
    let dir = scratch("dialog");
    let (e, _) = set(&dir);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut h = harness(e, &dir, analysis(), &calls);
    open_dialog(&mut h);
    match &h.state().dialogs.open {
        Some(Dialog::ExportSet { rekordbox: true, mixxx: true, stems: false, split: false, refused: None }) => {}
        other => panic!("expected the export dialog with both targets ticked, got {other:?}"),
    }
    assert!(h.query_by_label_contains("Mixxx must be closed").is_some());
    // No track of the set is split: no stem files to ask for.
    assert!(disabled(&h, STEMS_LABEL));
    h.get_by_label("Export").click();
    h.run_steps(1);
    assert!(h.state().export.is_some(), "{}", h.state().ui.status);
    assert!(h.state().dialogs.open.is_none());
    step_until_exported(&mut h);

    let out = dir.join("prepped").join("test set");
    let status = h.state().ui.status.clone();
    assert!(status.contains("Rekordbox XML written"), "{status}");
    assert!(status.contains("Mixxx: 2 track(s) in playlist \"mixforge · test set\""), "{status}");
    assert!(status.contains(&out.display().to_string()), "{status}");
    assert!(!status.contains("stem"), "stems weren't asked for: {status}");
    let xml = std::fs::read_to_string(out.join("test set.rekordbox.xml")).unwrap();
    assert!(xml.contains("<PRODUCT Name=\"djforge\""), "{xml}");
    assert!(out.join("a.mp3").is_file() && out.join("b.mp3").is_file() && out.join("test set.export.txt").is_file());
    assert_eq!(*calls.lock().unwrap(), ["test set"]);
    assert!(status.contains("see test set.export.txt"), "{status}");

    // A window says what happened to each track, with the folder and a way to open it.
    h.run_steps(6);
    assert!(h.query_by_label_contains("Export Finished").is_some());
    assert!(h.query_by_label(&out.display().to_string()).is_some());
    assert!(h.query_by_label("Open folder").is_some());
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert!(h.state().export_result.is_none());
    assert!(h.query_by_label_contains("Export Finished").is_none());
}

#[test]
fn export_needs_a_target_and_mixxx_can_be_left_out() {
    let dir = scratch("targets");
    let (e, _) = set(&dir);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut h = harness(e, &dir, analysis(), &calls);
    open_dialog(&mut h);
    h.get_by_label("Rekordbox").click();
    h.get_by_label("Mixxx").click();
    h.run_steps(2);
    assert!(disabled(&h, "Export"));
    h.get_by_label("Export").click();
    h.key_press(egui::Key::Enter);
    h.run_steps(2);
    assert!(h.state().export.is_none());
    assert!(matches!(h.state().dialogs.open, Some(Dialog::ExportSet { rekordbox: false, mixxx: false, .. })));

    h.get_by_label("Rekordbox").click();
    h.run_steps(2);
    assert!(!disabled(&h, "Export"));
    h.get_by_label("Export").click();
    h.run_steps(1);
    step_until_exported(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.contains("Rekordbox XML written") && !status.contains("Mixxx"), "{status}");
    assert!(calls.lock().unwrap().is_empty(), "Mixxx was not asked for");
    assert!(dir.join("prepped").join("test set").join("test set.rekordbox.xml").is_file());
}

#[test]
fn the_stems_checkbox_is_enabled_once_a_track_of_the_set_is_split() {
    let dir = scratch("stems");
    let (mut e, clips) = set(&dir);
    let s = e.session_mut();
    let src = s.find_clip(clips[1]).unwrap().1.source().unwrap();
    s.find_clip_mut(clips[1]).unwrap().stems = Some(StemSet { key: "fake".into(), sources: [src; 4], scale_log2: 0 });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut h = harness(e, &dir, analysis(), &calls);
    open_dialog(&mut h);
    assert!(matches!(h.state().dialogs.open, Some(Dialog::ExportSet { split: true, stems: false, .. })));
    assert!(!disabled(&h, STEMS_LABEL));
    h.get_by_label(STEMS_LABEL).click();
    h.run_steps(2);
    assert!(matches!(h.state().dialogs.open, Some(Dialog::ExportSet { stems: true, .. })));
}

/// Stems asked for with no ffmpeg to make them: the rest is exported, and the status line says
/// why there are no stem files. (The engine's ffmpeg is a path that doesn't exist.)
#[test]
fn stem_files_without_ffmpeg_are_skipped_and_the_status_line_says_why() {
    let dir = scratch("no-ffmpeg");
    let (mut e, clips) = set(&dir);
    let s = e.session_mut();
    // Only the clip of the file playing later in the set is split: its stems are still found.
    let src = s.find_clip(clips[1]).unwrap().1.source().unwrap();
    s.find_clip_mut(clips[1]).unwrap().stems = Some(StemSet { key: "fake".into(), sources: [src; 4], scale_log2: 0 });
    e.set_ffmpeg(dir.join("missing").join("ffmpeg.exe"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut h = harness(e, &dir, analysis(), &calls);
    open_dialog(&mut h);
    h.get_by_label(STEMS_LABEL).click();
    h.run_steps(2);
    h.get_by_label("Export").click();
    h.run_steps(1);
    step_until_exported(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.contains("Rekordbox XML written") && status.contains("stem files: skipped — ffmpeg not found"), "{status}");
}

#[test]
fn a_session_that_can_t_be_exported_says_why_in_the_dialog() {
    let dir = scratch("refused");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut h = harness(Engine::default(), &dir, analysis(), &calls);
    open_dialog(&mut h);
    assert!(h.query_by_label_contains("the session has no DJ transitions").is_some());
    assert!(disabled(&h, "Export"));
    h.key_press(egui::Key::Enter);
    h.run_steps(2);
    assert!(h.state().export.is_none() && !dir.join("prepped").exists());
}

#[test]
fn the_status_line_names_missing_files_and_tracks_left_out_of_mixxx() {
    let dir = scratch("missing");
    let (e, _) = set(&dir);
    std::fs::remove_file(dir.join("lib").join("b.mp3")).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut h = harness(e, &dir, analysis(), &calls);
    h.state_mut().run("dj.export_set", json!({})).unwrap();
    step_until_exported(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.contains("1 not found (b.mp3)"), "{status}");
    assert!(status.contains("1 left out of Mixxx (b.mp3)"), "{status}");
    assert!(status.contains("Mixxx: 1 track(s)"), "{status}");
    // The window lists the track's doubts.
    h.run_steps(6);
    assert!(h.query_by_label_contains("its file was not found").is_some());
}

/// Once mixforge is writing Mixxx's library the export can't stop: the window says so, Cancel
/// and Esc are off, and a quit waits for it to finish; what came after (the report) still says
/// the export was cancelled.
#[test]
fn the_mixxx_write_can_t_be_stopped_and_a_quit_waits_for_it() {
    let dir = scratch("in-mixxx");
    let (e, _) = set(&dir);
    let (reached_tx, reached) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let inner = fake_mixxx(&calls);
    let runner: MixxxRunner = Arc::new(move |plan: &Value, set: &str| {
        let _ = reached_tx.lock().unwrap().send(());
        let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60));
        inner(plan, set)
    });
    let mut h = harness(e, &dir, analysis(), &Arc::new(Mutex::new(Vec::new())));
    h.state_mut().engine.set_mixxx_runner(runner);
    h.state_mut().run("dj.export_set", json!({})).unwrap();
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Writing the set into Mixxx — can't stop now").is_some());
    assert!(disabled(&h, "Cancel"));
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(!h.state().export.as_ref().is_some_and(ExportJob::cancelling), "Esc can't stop the Mixxx write");
    assert!(soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    go.send(()).unwrap();
    step_until_exported(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.contains("Export cancelled") && status.contains("Mixxx: 2 track(s)"), "{status}");
    assert_eq!(*calls.lock().unwrap(), ["test set"]);
    assert!(!h.state().prep_quit);
}

#[test]
fn an_agent_s_export_runs_in_the_background_and_jobs_wait_for_each_other() {
    let dir = scratch("agent");
    let (e, _) = set(&dir);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (analyse, reached, go) = gated();
    let mut h = harness(e, &dir, analyse, &calls);
    let tracks = h.state().engine.session().tracks.len();
    let journaled = h.state().engine.journal.len();
    let started = h.state_mut().run("dj.export_set", json!({"targets": ["rekordbox", "mixxx"]})).unwrap();
    let out = dir.join("prepped").join("test set");
    assert_eq!(started["started"], true, "{started}");
    assert_eq!(PathBuf::from(started["root"].as_str().unwrap()), out);
    assert!(h.state().export.is_some());
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Prepping 1 of 2: a.mp3").is_some());

    // While it runs: no other job, no file or session command, and no second export.
    let copy = out.join("a.mp3").to_string_lossy().into_owned();
    for (id, params) in [
        ("dj.export_set", json!({})),
        ("prep.tracks", json!({"source": dir.join("lib").to_string_lossy(), "out": dir.join("other").to_string_lossy()})),
        ("dj.split_stems", json!({"clip": 1})),
        ("dj.import_set", json!({"plan": dir.join("plan.json").to_string_lossy()})),
        ("file.import_audio", json!({"paths": [copy]})),
        ("session.save_as", json!({"path": dir.join("Mine.scraft").to_string_lossy()})),
        ("session.new", json!({})),
        ("session.close", json!({})),
    ] {
        assert_eq!(h.state_mut().run(id, params).unwrap_err(), WAIT, "{id}");
    }
    assert!(!dir.join("other").exists() && !dir.join("Mine.scraft").exists());
    assert_eq!(h.state().engine.session().tracks.len(), tracks);
    soundcraft_ui_egui::prep::start(h.state_mut(), &json!({"source": dir.join("lib").to_string_lossy()}));
    assert!(h.state().prep.is_none(), "{}", h.state().ui.status);
    // Read-only queries, editing and playback go on.
    for id in ["session.inspect", "edit.select_none", "transport.play", "transport.stop"] {
        assert!(h.state_mut().run(id, json!({})).is_ok(), "{id}: {}", h.state().ui.status);
    }

    // Esc cancels: prep stops after its track, and nothing more is written.
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().export.as_ref().is_some_and(ExportJob::cancelling));
    assert!(h.query_by_label_contains("Stopping after a.mp3").is_some());
    go.send(()).unwrap();
    step_until_exported(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.contains("Export cancelled after 1 of 2"), "{status}");
    assert!(!out.join("test set.rekordbox.xml").exists());
    assert!(calls.lock().unwrap().is_empty());
    assert!(reached.try_recv().is_err(), "a second track was started");
    // Like a prep, a background export doesn't go through the command journal.
    assert!(h.state().engine.journal[journaled..].iter().all(|(id, _)| id != "dj.export_set"));
    assert!(h.state_mut().run("session.inspect", json!({})).is_ok());
}

#[test]
fn an_export_waits_for_a_running_prep() {
    let dir = scratch("after-prep");
    let (e, _) = set(&dir);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (analyse, reached, go) = gated();
    let mut h = harness(e, &dir, analysis(), &calls);
    let s = soundcraft_engine::model::Session::default();
    let lib = dir.join("lib");
    let job = soundcraft_engine::cmd::prep_start(&s, lib.to_str().unwrap(), Some(dir.join("out").to_str().unwrap()), -14.0, -1.0, analyse).unwrap();
    h.state_mut().prep = Some(job);
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    let err = h.state_mut().run("dj.export_set", json!({})).unwrap_err();
    assert_eq!(err, "Wait for the prep to finish (or cancel it)");
    open_dialog(&mut h);
    h.get_by_label("Export").click();
    h.run_steps(1);
    assert!(h.state().export.is_none());
    assert_eq!(h.state().ui.status, "Wait for the prep to finish (or cancel it)");
    assert!(!dir.join("prepped").exists());
    h.state().prep.as_ref().unwrap().cancel();
    go.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(600);
    while h.state().prep.is_some() {
        assert!(Instant::now() < until, "the prep never ended");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_quit_cancels_the_export_and_waits_for_it_to_stop() {
    let dir = scratch("quit");
    let (e, _) = set(&dir);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (analyse, reached, go) = gated();
    let mut h = harness(e, &dir, analyse, &calls);
    assert!(!soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    h.state_mut().run("dj.export_set", json!({"targets": ["rekordbox"]})).unwrap();
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    assert!(soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    assert!(h.state().export.as_ref().is_some_and(ExportJob::cancelling));
    assert!(h.state().prep_quit);
    assert!(h.state().ui.status.contains("export"), "{}", h.state().ui.status);
    go.send(()).unwrap();
    step_until_exported(&mut h);
    assert!(!h.state().prep_quit, "the quit was not asked again");
    assert!(h.state().ui.status.contains("Export cancelled"), "{}", h.state().ui.status);
    assert!(!soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
}
