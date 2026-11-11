//! Save As over another session asks first, and Enter alone never replaces it.
use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, dialogs::Dialog};

#[test]
fn saving_over_another_session_takes_the_replace_button() {
    let dir = std::env::temp_dir().join(format!("soundcraft-ui-replace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Song.djforge");
    soundcraft_engine::Engine::default().execute("session.save_as", &json!({"path": path})).unwrap();
    let other = std::fs::read(&path).unwrap();
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.dialogs.open = Some(Dialog::PathPrompt {
        cmd: "session.save_as".into(),
        title: "Save Session As".into(),
        path: path.to_string_lossy().into(),
        key: "path".into(),
    });
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(std::fs::read(&path).unwrap(), other);
    h.get_by_label("Replace").click();
    h.run_steps(3);
    assert_ne!(std::fs::read(&path).unwrap(), other);
    assert_eq!(h.state().engine.path, Some(path.to_string_lossy().into_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_as_on_a_soundcraft_session_writes_the_djforge_beside_it() {
    let dir = std::env::temp_dir().join(format!("djforge-ui-scraft-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("Song.scraft");
    soundcraft_engine::Engine::default().execute("session.save_as", &json!({"path": dir.join("Song.djforge")})).unwrap();
    std::fs::rename(dir.join("Song.djforge"), &old).unwrap();
    let original = std::fs::read(&old).unwrap();
    let mut engine = soundcraft_engine::demo::demo_engine();
    engine.path = Some(old.to_string_lossy().into_owned());
    let mut app = SoundApp::new(engine, None, Services::default());
    app.dialogs.open = Some(Dialog::PathPrompt {
        cmd: "session.save_as".into(),
        title: "Save Session As".into(),
        path: old.to_string_lossy().into(),
        key: "path".into(),
    });
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    // The `.djforge` doesn't exist yet: nothing is replaced, so Enter saves.
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    let new = dir.join("Song.djforge");
    assert!(new.is_file());
    assert_eq!(std::fs::read(&old).unwrap(), original, "the .scraft is never written over");
    assert_eq!(h.state().engine.path, Some(new.to_string_lossy().into_owned()));
    let _ = std::fs::remove_dir_all(&dir);
}
