//! DJ > Prep Tracks... asks for one path (a plan.json or a folder) and sends it as `source`.

use soundcraft_ui_egui::dialogs::{Dialog, Dialogs};
use soundcraft_ui_egui::menus;

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
