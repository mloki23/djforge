//! The app is called djforge everywhere people see its name, and About credits SoundCraft.
use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use soundcraft_ui_egui::{Services, SoundApp, menus, panels};

#[test]
fn the_app_is_djforge_and_about_credits_soundcraft() {
    assert_eq!(soundcraft_engine::APP_NAME, "djforge");
    assert_eq!(panels::about_title(), "About djforge");
    assert_eq!(panels::about_credit(), "djforge — a DJ-first DAW. Based on SoundCraft by the ArtCraft team (MIT/Apache-2.0)");
    let about = menus::UI_COMMANDS.iter().find(|c| c.0 == "window.about").map(|c| c.1);
    assert_eq!(about, Some("About djforge"));
}

#[test]
fn the_about_window_shows_the_credit_and_the_licence() {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    app.ui.show_about = true;
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(3);
    h.get_by_label("About djforge");
    h.get_by_label(&panels::about_credit());
    for line in panels::ABOUT_NOTICES {
        h.get_by_label(line);
    }
    h.get_by_label("Copyright (c) 2026 ArtCraft Team and the SoundCraft contributors.");
    assert!(h.query_by_label("About SoundCraft").is_none());
    assert!(h.query_by_label_contains("Discord").is_none() && h.query_by_label_contains("getartcraft").is_none(), "no ArtCraft links");
}
