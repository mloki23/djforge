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

/// Refuses to replace the session while a prep runs: the prep refuses to write over the audio of
/// the session it started in (a snapshot of its source files), so a session opened meanwhile
/// could play a file it is replacing. True when refused; the status line says why.
pub fn refuses_session_switch(app: &mut SoundApp) -> bool {
    if app.prep.is_some() {
        app.ui.status = "Finish or cancel the prep first".into();
    }
    app.prep.is_some()
}

/// Refuses command `id` while a prep runs when it names a file or folder inside the folder the
/// prep writes (`path`, `plan`, `dir`, `source`, or the session's own folder when saving): an
/// import from there would put a file the prep may replace into the session after the prep took
/// its snapshot of the session's files, and a save there could be replaced too. True when
/// refused; the status line says why.
pub fn refuses_target(app: &mut SoundApp, id: &str, params: &Value) -> bool {
    let Some(job) = &app.prep else { return false };
    let out = job.out().to_path_buf();
    let mut targets: Vec<std::path::PathBuf> =
        ["path", "plan", "dir", "source"].iter().filter_map(|k| params.get(*k).and_then(Value::as_str)).map(std::path::PathBuf::from).collect();
    if id.starts_with("session.save")
        && let Some(folder) = &app.engine.session().folder
    {
        targets.push(folder.clone());
    }
    let Some(hit) = targets.into_iter().find(|t| inside(t, &out)) else { return false };
    app.ui.status = format!("{} is in the folder the prep is writing ({}); wait for the prep or cancel it", hit.display(), out.display());
    log::warn!("{id}: {}", app.ui.status);
    true
}

/// Whether `path` (made absolute, `.` and `..` resolved by name) is `dir` or inside it, without
/// regard to case on Windows.
fn inside(path: &std::path::Path, dir: &std::path::Path) -> bool {
    use std::path::Component;
    let resolved = |p: &std::path::Path| {
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let mut out: Vec<String> = Vec::new();
        for c in abs.components() {
            match c {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => {
                    let s = other.as_os_str().to_string_lossy();
                    out.push(if cfg!(windows) { s.to_lowercase() } else { s.into_owned() });
                }
            }
        }
        out
    };
    let (path, dir) = (resolved(path), resolved(dir));
    path.len() >= dir.len() && path.iter().zip(&dir).all(|(a, b)| a == b)
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
