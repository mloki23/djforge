use egui::accesskit::Role;
use egui::{Event, Modifiers, PointerButton, Pos2, Vec2, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use soundcraft_ui_egui::{Services, SoundApp};

const WINDOW_PX: Vec2 = vec2(1600.0, 1000.0);

fn frame(h: &mut Harness<'_, SoundApp>, px: Pos2, pressed: Option<bool>) {
    let zoom = h.ctx.zoom_factor();
    h.set_size(WINDOW_PX / zoom);
    let pos = (px.to_vec2() / zoom).to_pos2();
    h.input_mut().events.push(Event::PointerMoved(pos));
    if let Some(pressed) = pressed {
        h.input_mut().events.push(Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
    }
    h.step();
}

#[test]
fn interface_scale_slider_follows_the_pointer() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.extra.show_ui_customization = true;
    let mut h = Harness::builder().with_size(WINDOW_PX).with_pixels_per_point(1.0).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(6);
    let rail = h.get_by_role(Role::Slider).rect();
    let start = pos2(rail.left() + rail.width() / 3.0, rail.center().y);
    let end = pos2(rail.right() + 40.0, rail.center().y);
    frame(&mut h, start, Some(true));
    let mut scales = vec![];
    for i in 1..=20 {
        frame(&mut h, start.lerp(end, i as f32 / 20.0), None);
        scales.push(h.state().extra.ui_scale);
    }
    assert!(scales.windows(2).all(|w| w[0] <= w[1]), "{scales:?}");
    frame(&mut h, end, Some(false));
    frame(&mut h, end, None);
    assert_eq!(h.state().extra.ui_scale, 1.5);
    assert_eq!(h.ctx.zoom_factor(), 1.5);
}

#[test]
fn the_theme_menu_offers_every_palette_and_classic_applies() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.extra.show_ui_customization = true;
    let mut h = Harness::builder().with_size(WINDOW_PX).with_pixels_per_point(1.0).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    let combo = h.get_all_by_role(Role::ComboBox).find(|n| n.value().as_deref() == Some("djforge Dark")).expect("the theme menu");
    combo.click();
    h.run_steps(3);
    for label in ["Light", "System", "SoundCraft Classic"] {
        assert!(h.query_by_label(label).is_some(), "the theme menu lacks {label}");
    }
    // A real pointer click: move, press and release in separate frames, as a mouse does.
    let option = h.get_by_label("SoundCraft Classic").rect().center();
    frame(&mut h, option, None);
    frame(&mut h, option, Some(true));
    frame(&mut h, option, Some(false));
    h.run_steps(3);
    assert_eq!(h.state().ui.theme, soundcraft_ui_egui::theme::ThemeMode::Classic);
    assert_eq!(h.ctx.global_style().visuals.panel_fill, soundcraft_ui_egui::theme::Tokens::CLASSIC.panel_bg);
}
