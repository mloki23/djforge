//! Prep Tracks runs on its own thread behind a progress window, so the app keeps drawing and the
//! prep can be cancelled between tracks.

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use serde_json::Value;

/// Starts the prep `prep.tracks` runs for `params` (without `out`, into a folder in
/// `app.prep_root`, analysed by `app.prep_analyse`): its checks are made here (a refusal goes to
/// the status line), the tracks are prepped on another thread and [`step`] collects the result.
pub fn start(app: &mut SoundApp, params: &Value) {
    if app.prep.is_some() {
        app.ui.status = "A prep is already running".into();
        return;
    }
    let analyse = std::sync::Arc::clone(&app.prep_analyse);
    match soundcraft_engine::cmd::prep_job_in(&app.engine, params, app.prep_root.as_deref(), analyse) {
        Ok(job) => app.prep = Some(job),
        Err(e) => app.ui.status = e.to_string(),
    }
}

/// Collects the result once the prep has ended. Called from `logic`, which eframe keeps calling
/// while a repaint is requested, so the result arrives with the window hidden too.
pub fn step(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &mut app.prep else { return };
    let total = job.progress().total;
    let Some(result) = job.poll() else {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
    };
    app.ui.status = match result {
        Ok(v) => status(&v, total),
        Err(e) => e.to_string(),
    };
    app.prep = None;
}

/// Refuses to replace the session while a prep runs: the prep refuses to write over the audio of
/// the session it started in, and a session opened meanwhile could play a file it is replacing.
/// True when refused; the status line says why.
pub fn refuses_session_switch(app: &mut SoundApp) -> bool {
    if app.prep.is_some() {
        app.ui.status = "Finish or cancel the prep first".into();
    }
    app.prep.is_some()
}

/// The status line after `prep.tracks` (of `total` tracks): where the copies went and the totals.
pub(crate) fn status(v: &Value, total: usize) -> String {
    let t = &v["totals"];
    let n = |k: &str| t[k].as_u64().unwrap_or(0);
    let done = v["tracks"].as_array().map_or(0, Vec::len);
    let cancelled = if v["cancelled"].as_bool().unwrap_or(false) { format!("; cancelled after {done} of {total}") } else { String::new() };
    format!(
        "Prepped {done} track(s) into {}: {} ok, {} not adjusted, {} failed; {} of {} on target{cancelled}; see prep-report.txt",
        v["out"].as_str().unwrap_or_default(),
        n("ok"),
        n("not_adjusted"),
        n("failed"),
        n("on_target"),
        n("measured")
    )
}

/// Shows which track the running prep is on, with Cancel (or Esc), which stops it after that
/// track.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &app.prep else { return };
    let p = job.progress();
    let stopping = job.cancelling();
    let mut cancel = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    egui::Modal::new(egui::Id::new("prep_progress")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.set_width(360.0);
        let title = match (&p.current, stopping) {
            (Some(file), true) => format!("Stopping after {file}"),
            (Some(file), false) => format!("Prepping {} of {}: {file}", p.done + 1, p.total),
            (None, true) => "Stopping".to_string(),
            (None, false) => format!("Prepping {} of {}", p.done.min(p.total), p.total),
        };
        ui.label(egui::RichText::new(title).font(bold(13.0)));
        ui.add(egui::ProgressBar::new(if p.total == 0 { 1.0 } else { p.done as f32 / p.total as f32 }));
        ui.horizontal(|ui| {
            cancel |= ui.add_enabled(!stopping, egui::Button::new("Cancel")).clicked();
            ui.label(egui::RichText::new(format!("{} of {} done", p.done, p.total)).color(Tokens::current().text_dim));
        });
    });
    if cancel && !stopping {
        job.cancel();
        app.ui.status = "Stopping the prep after the current track".into();
    }
}
