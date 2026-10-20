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
        log::warn!("prep.tracks: {}", app.ui.status);
        return;
    }
    let analyse = std::sync::Arc::clone(&app.prep_analyse);
    match soundcraft_engine::cmd::prep_job_in(&app.engine, params, app.prep_root.as_deref(), analyse) {
        Ok(job) => app.prep = Some(job),
        Err(e) => {
            app.ui.status = e.to_string();
            log::warn!("prep.tracks: {e}");
        }
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
    if std::mem::take(&mut app.prep_quit) && !app.quit_requested {
        // The window's close waited for the prep: ask again, now that it has stopped.
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

/// Holds a quit while a prep runs: cancels it, and the quit goes ahead once the prep has stopped
/// after its current track (the window is closed again then; a quit asked by command stays
/// asked). True when held; the status line says why.
pub fn holds_quit(app: &mut SoundApp) -> bool {
    let Some(job) = &app.prep else { return false };
    job.cancel();
    app.prep_quit = true;
    app.ui.status = "Quitting once the prep stops after the current track".into();
    true
}

/// Refuses command `id` while a prep runs when it reads or writes files or replaces the session:
/// every `file.` and `session.` command but the read-only ones, `dj.import_set` and another
/// `prep.tracks`. The prep refuses to write over the audio of the session it started in (a
/// snapshot of its source files), so neither a file it may replace nor another session may come
/// in meanwhile. Playback, editing and mixing go on. True when refused; the status line says why.
pub fn refuses_command(app: &mut SoundApp, id: &str) -> bool {
    if app.prep.is_none() {
        return false;
    }
    let read_only = soundcraft_engine::find_command(id).is_some_and(|c| !c.journal && !c.undoable && !c.view);
    let refused = ((id.starts_with("file.") || id.starts_with("session.")) && !read_only) || matches!(id, "dj.import_set" | "prep.tracks");
    if refused {
        app.ui.status = "Wait for the prep to finish (or cancel it)".into();
        log::warn!("{id}: {}", app.ui.status);
    }
    refused
}

/// The status line after `prep.tracks` (of `total` tracks): where the copies went and the totals.
pub(crate) fn status(v: &Value, total: usize) -> String {
    let t = &v["totals"];
    let n = |k: &str| t[k].as_u64().unwrap_or(0);
    // A track cancelled part-way was not prepped.
    let done = v["tracks"].as_array().map_or(0, |t| t.iter().filter(|t| t["status"] != "cancelled").count());
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
    // Esc is taken here, so it doesn't also reach what's behind the window.
    let mut cancel = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
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
