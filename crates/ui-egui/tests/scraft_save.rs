//! A session opened from a SoundCraft `.scraft` saves as the `.djforge` beside it: the app says
//! so, Open Recent offers the `.djforge` instead, the `.scraft` is never written, and when the
//! `.djforge` already exists Save asks before replacing it (also from the save-changes question).
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_ui_egui::{Services, SoundApp, menus};
use std::path::{Path, PathBuf};

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A folder holding `Song.scraft` (a session as SoundCraft saved it) and, if asked, an unrelated
/// `Song.djforge` beside it.
fn folder(tag: &str, with_djforge: bool) -> (Dir, PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("djforge-scraft-save-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    soundcraft_engine::Engine::default().execute("session.save_as", &json!({"path": dir.join("Song.djforge")})).unwrap();
    let old = dir.join("Song.scraft");
    std::fs::rename(dir.join("Song.djforge"), &old).unwrap();
    if with_djforge {
        soundcraft_engine::Engine::default().execute("session.save_as", &json!({"path": dir.join("Song.djforge")})).unwrap();
    }
    let bytes = std::fs::read(&old).unwrap();
    (Dir(dir), old, bytes)
}

fn harness(old: &Path) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.run("session.open", json!({"path": old})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    steps(&mut h);
    h
}

fn steps(h: &mut Harness<'_, SoundApp>) {
    for _ in 0..4 {
        h.step();
    }
}

fn change(h: &mut Harness<'_, SoundApp>) {
    let track = h.state_mut().run("track.new", json!({"name": "Deck A", "format": "stereo"})).unwrap()["tracks"][0].clone();
    h.state_mut().run("mix.mute", json!({"track": track})).unwrap();
}

#[test]
fn saving_a_scraft_says_where_it_went_and_recent_offers_the_djforge() {
    let (dir, old, original) = folder("status", false);
    let mut h = harness(&old);
    assert_eq!(h.state().ui.recent_sessions.first().map(String::as_str), Some(old.to_string_lossy().as_ref()));
    change(&mut h);
    menus::invoke_menu(h.state_mut(), "session.save", "File > Save");
    steps(&mut h);
    let new = dir.0.join("Song.djforge");
    assert!(new.is_file());
    assert_eq!(std::fs::read(&old).unwrap(), original, "the .scraft is unchanged");
    assert_eq!(h.state().engine.path.as_deref(), Some(new.to_string_lossy().as_ref()));
    assert_eq!(h.state().ui.status, "Saved as Song.djforge; Song.scraft is unchanged.");
    let recent = &h.state().ui.recent_sessions;
    assert_eq!(recent.first().map(String::as_str), Some(new.to_string_lossy().as_ref()));
    assert!(!recent.iter().any(|p| p.ends_with(".scraft")), "{recent:?}");
}

#[test]
fn save_asks_before_replacing_an_existing_djforge() {
    let (dir, old, original) = folder("replace", true);
    let other = std::fs::read(dir.0.join("Song.djforge")).unwrap();
    let mut h = harness(&old);
    change(&mut h);
    menus::invoke_menu(h.state_mut(), "session.save", "File > Save");
    steps(&mut h);
    // Not refused: Save As opens on the `.djforge`, asking to replace it.
    assert_eq!(h.state().dialogs.open_name(), Some("path"));
    h.get_by_label("Replace").click();
    steps(&mut h);
    let new = dir.0.join("Song.djforge");
    assert_ne!(std::fs::read(&new).unwrap(), other, "replaced once asked");
    assert_eq!(std::fs::read(&old).unwrap(), original);
    assert!(!h.state().engine.is_dirty());
    assert_eq!(h.state().engine.path.as_deref(), Some(new.to_string_lossy().as_ref()));
}

#[test]
fn save_from_the_save_changes_question_asks_too_and_then_closes() {
    let (dir, old, original) = folder("question", true);
    let mut h = harness(&old);
    change(&mut h);
    menus::invoke_menu(h.state_mut(), "session.close", "File > Close Session");
    steps(&mut h);
    assert_eq!(h.state().dialogs.open_name(), Some("save_changes"));
    h.get_by_label("Save").click();
    steps(&mut h);
    assert_eq!(h.state().dialogs.open_name(), Some("path"), "asks before replacing Song.djforge");
    h.get_by_label("Replace").click();
    steps(&mut h);
    assert_eq!(std::fs::read(&old).unwrap(), original);
    assert!(dir.0.join("Song.djforge").is_file());
    assert!(h.state().engine.path.is_none(), "closed after saving");
}
