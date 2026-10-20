//! DJ > Prep Tracks... asks for one path (a plan.json or a folder) and sends it as `source`; the
//! prep then runs on its own thread behind a progress window with Cancel, so the app keeps
//! drawing.

use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::{Value, json};
use soundcraft_engine::cmd::{PrepAnalyse, PrepJob, prep_start};
use soundcraft_ui_egui::dialogs::{Dialog, Dialogs};
use soundcraft_ui_egui::{Services, SoundApp, menus};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// In the order prep takes them (by name).
const FIXTURES: [&str; 3] = ["cbr128_44k_stereo.mp3", "cbr64_44k_mono.mp3", "vbr_48k_stereo.mp3"];

#[test]
fn prep_tracks_is_in_the_dj_menu_and_prompts_for_its_source() {
    assert!(menus::ui_aliases().iter().any(|(path, id)| *path == "DJ > Prep Tracks..." && *id == "prep.tracks"));
    let e = soundcraft_engine::Engine::new(soundcraft_engine::model::Session::default());
    let mut d = Dialogs::default();
    assert!(d.open_for_command(&e, "prep.tracks"));
    match d.open {
        Some(Dialog::PathPrompt { cmd, key, title, .. }) => {
            assert_eq!((cmd.as_str(), key.as_str()), ("prep.tracks", "source"));
            assert!(title.contains("plan.json or folder"), "{title}");
        }
        _ => panic!("expected a path prompt"),
    }
}

fn harness() -> Harness<'static, SoundApp> {
    let app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
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

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-ui-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Folder `dir/name` holding the three MP3 fixtures.
fn mp3_folder(dir: &Path, name: &str) -> PathBuf {
    let src = dir.join(name);
    std::fs::create_dir_all(&src).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3");
    for f in FIXTURES {
        std::fs::copy(fixtures.join(f), src.join(f)).unwrap();
    }
    src
}

/// Steps the app until its prep has ended.
fn step_until_prepped(h: &mut Harness<'_, SoundApp>) {
    let until = Instant::now() + Duration::from_secs(600);
    while h.state().prep.is_some() {
        assert!(Instant::now() < until, "the prep never ended");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    v.sort();
    v
}

#[test]
fn the_dialog_starts_a_prep_and_stepping_the_app_completes_it() {
    let dir = scratch("prep-dialog");
    let src = mp3_folder(&dir, "my set");
    let root = dir.join("prepped");
    let mut h = harness();
    let app = h.state_mut();
    // The prompt sends only `source`: the copies go to a folder named for it in the app's root.
    app.prep_root = Some(root.clone());
    let analysed = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&analysed);
    app.prep_analyse = Arc::new(move |path: &str, _: &dyn Fn() -> bool| {
        seen.lock().unwrap().push(path.to_string());
        Ok(json!({"camelot": "8A", "bpm": 124.0}))
    });
    assert!(app.dialogs.open_for_command(&app.engine, "prep.tracks"));
    if let Some(Dialog::PathPrompt { path, .. }) = &mut app.dialogs.open {
        *path = src.to_string_lossy().into_owned();
    }
    h.run_steps(2);
    h.get_by_label("OK").click();
    h.run_steps(1);
    assert!(h.state().prep.is_some(), "{}", h.state().ui.status);
    assert!(h.state().dialogs.open.is_none());
    let journaled = h.state().engine.journal.len();
    step_until_prepped(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.starts_with("Prepped 3 track(s) into"), "{status}");
    assert!(!status.contains("cancelled"), "{status}");
    let out = root.join("my set");
    let mut want: Vec<String> = FIXTURES.iter().map(|f| f.to_string()).chain(["prep-report.txt".into()]).collect();
    want.sort();
    assert_eq!(names_in(&out), want);
    // Every track was analysed by the app's analysis, not mixforge's sidecar.
    assert_eq!(analysed.lock().unwrap().len(), 3);
    // Like a bounce, a prep started from its dialog doesn't go through the command journal.
    assert_eq!(h.state().engine.journal.len(), journaled);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A prep of `src` into `out` whose analysis says which file it reached, then waits for a go-ahead.
fn gated_job(src: &Path, out: &Path) -> (PrepJob, mpsc::Receiver<String>, mpsc::Sender<()>) {
    let (reached_tx, reached) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let analyse: PrepAnalyse = Arc::new(move |path: &str, _: &dyn Fn() -> bool| {
        reached_tx.lock().unwrap().send(path.to_string()).unwrap();
        go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60)).unwrap();
        Err::<Value, String>("no analysis here".into())
    });
    let s = soundcraft_engine::model::Session::default();
    let job = prep_start(&s, src.to_str().unwrap(), Some(out.to_str().unwrap()), -14.0, -1.0, analyse).unwrap();
    (job, reached, go)
}

#[test]
fn the_progress_window_shows_the_track_and_cancel_stops_after_it() {
    let dir = scratch("prep-cancel");
    let src = mp3_folder(&dir, "set");
    let out = dir.join("out");
    let mut h = harness();
    let (job, reached, go) = gated_job(&src, &out);
    h.state_mut().prep = Some(job);
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains(&format!("Prepping 1 of 3: {}", FIXTURES[0])).is_some());
    // Another prep is refused while this one runs.
    soundcraft_ui_egui::prep::start(h.state_mut(), &json!({"source": src.to_string_lossy(), "out": dir.join("other").to_string_lossy()}));
    assert!(h.state().ui.status.contains("already running"), "{}", h.state().ui.status);
    assert!(!dir.join("other").exists());

    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.query_by_label_contains(&format!("Stopping after {}", FIXTURES[0])).is_some());
    go.send(()).unwrap();
    step_until_prepped(&mut h);
    let status = h.state().ui.status.clone();
    assert!(status.starts_with("Prepped 1 track(s) into"), "{status}");
    assert!(status.contains("cancelled after 1 of 3"), "{status}");
    assert!(reached.try_recv().is_err(), "a second track was started");
    assert_eq!(names_in(&out), [FIXTURES[0], "prep-report.txt"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn escape_cancels_and_the_session_cannot_be_replaced_while_it_preps() {
    let dir = scratch("prep-switch");
    let src = mp3_folder(&dir, "set");
    let mut h = harness();
    let tracks = h.state().engine.session().tracks.len();
    let (job, reached, go) = gated_job(&src, &dir.join("out"));
    h.state_mut().prep = Some(job);
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    for id in ["session.new", "session.open", "session.close", "session.revert"] {
        let err = h.state_mut().run(id, json!({"path": dir.join("Other.scraft").to_string_lossy()})).unwrap_err();
        assert!(err.contains("prep"), "{id}: {err}");
    }
    assert_eq!(h.state().engine.session().tracks.len(), tracks);
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().prep.as_ref().is_some_and(PrepJob::cancelling));
    go.send(()).unwrap();
    step_until_prepped(&mut h);
    assert!(h.state().ui.status.contains("cancelled after 1 of 3"), "{}", h.state().ui.status);
    assert!(h.state_mut().run("session.close", json!({})).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn files_in_the_prep_folder_cannot_be_imported_or_saved_to_while_it_runs() {
    let dir = scratch("prep-targets");
    let src = mp3_folder(&dir, "set");
    let out = dir.join("out");
    let mut h = harness();
    let (job, reached, go) = gated_job(&src, &out);
    h.state_mut().prep = Some(job);
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    let copy = out.join(".").join(FIXTURES[0]);
    for (id, params) in [
        ("file.import_audio", json!({"path": copy.to_string_lossy()})),
        ("session.save_as", json!({"path": out.join("Mine.scraft").to_string_lossy()})),
        ("dj.import_set", json!({"plan": out.join("plan.json").to_string_lossy()})),
    ] {
        let err = h.state_mut().run(id, params).unwrap_err();
        assert!(err.contains("the prep is writing"), "{id}: {err}");
    }
    // Elsewhere is fine (this one fails for its own reason).
    let err = h.state_mut().run("file.import_audio", json!({"path": dir.join("nothing.wav").to_string_lossy()})).unwrap_err();
    assert!(!err.contains("the prep is writing"), "{err}");
    go.send(()).unwrap();
    for _ in 0..2 {
        if reached.recv_timeout(Duration::from_secs(60)).is_ok() {
            go.send(()).unwrap();
        }
    }
    step_until_prepped(&mut h);
    assert!(h.state_mut().run("file.import_audio", json!({"path": out.join(FIXTURES[0]).to_string_lossy()})).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_quit_cancels_the_prep_and_waits_for_it_to_stop() {
    let dir = scratch("prep-quit");
    let src = mp3_folder(&dir, "set");
    let mut h = harness();
    assert!(!soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    let (job, reached, go) = gated_job(&src, &dir.join("out"));
    h.state_mut().prep = Some(job);
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    assert!(soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    assert!(h.state().prep.as_ref().is_some_and(PrepJob::cancelling));
    assert!(h.state().prep_quit);
    go.send(()).unwrap();
    step_until_prepped(&mut h);
    assert!(!h.state().prep_quit, "the quit was not asked again");
    assert!(h.state().ui.status.contains("cancelled after 1 of 3"), "{}", h.state().ui.status);
    assert!(!soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    let _ = std::fs::remove_dir_all(&dir);
}
