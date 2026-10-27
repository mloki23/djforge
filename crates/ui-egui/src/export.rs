//! Export Set runs `dj.export_set` on its own thread behind a progress window (like Prep Tracks),
//! so the app keeps drawing and the export can be cancelled while it preps.

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use serde_json::{Value, json};
use soundcraft_model::Session;

/// Whether a clip of the set (a clip of a DJ transition) is split, so stem files can be asked for.
pub(crate) fn set_is_split(s: &Session) -> bool {
    s.dj.transitions.iter().flat_map(|t| [t.out_clip, t.in_clip]).any(|id| s.find_clip(id).is_some_and(|(_, c)| c.stems.is_some()))
}

/// Starts the export `dj.export_set` runs for `params` (without `out`, into a folder in
/// `app.prep_root`, analysed by `app.prep_analyse`; Mixxx through the engine's Mixxx runner),
/// from the Export Set dialog or `app.run` (agents, scripts): its checks are made here, the
/// export runs on another thread and [`step`] collects the result. Returns `{root, started:
/// true}`; a refusal also goes to the status line.
pub fn start(app: &mut SoundApp, params: &Value) -> Result<Value, String> {
    let r = start_job(app, params);
    if let Err(e) = &r {
        app.ui.status.clone_from(e);
        log::warn!("dj.export_set: {e}");
    }
    r
}

fn start_job(app: &mut SoundApp, params: &Value) -> Result<Value, String> {
    if crate::prep::refuses_command(app, "dj.export_set") {
        return Err(app.ui.status.clone());
    }
    let analyse = std::sync::Arc::clone(&app.prep_analyse);
    let job = soundcraft_engine::cmd::export_job_in(&app.engine, params, app.prep_root.as_deref(), analyse).map_err(|e| e.to_string())?;
    let root = job.out().to_path_buf();
    app.export = Some(job);
    app.ui.status = format!("Exporting the set to {}", root.display());
    Ok(json!({"root": root, "started": true}))
}

/// Collects the result once the export has ended. Called from `logic`, which eframe keeps
/// calling while a repaint is requested, so the result arrives with the window hidden too. The
/// export works on a snapshot of the session taken when it started and changes nothing in it,
/// so its result stands whatever happened to the session meanwhile.
pub fn step(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &mut app.export else { return };
    let total = job.progress().total;
    let Some(result) = job.poll() else {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
    };
    app.ui.status = match result {
        Ok(v) => status(&v, total),
        Err(e) => e.to_string(),
    };
    app.export = None;
    crate::prep::quit_when_idle(app, ctx);
}

/// The status line after `dj.export_set` (of `total` tracks): what each target got, and the
/// export folder.
pub(crate) fn status(v: &Value, total: usize) -> String {
    let root = v["root"].as_str().unwrap_or_default();
    if v["cancelled"].as_bool().unwrap_or(false) {
        // A track cancelled part-way was not prepped.
        let done = v["prep"]["tracks"].as_array().map_or(0, |t| t.iter().filter(|t| t["status"] != "cancelled").count());
        return format!("Export cancelled after {done} of {total} track(s); nothing more was written to {root}");
    }
    let mut parts = Vec::new();
    match &v["rekordbox"] {
        Value::String(_) => parts.push("Rekordbox XML written".to_string()),
        Value::Null => {}
        other => parts.push(format!("Rekordbox: failed — {}", other["error"].as_str().unwrap_or("unknown error"))),
    }
    let mixxx = &v["mixxx"];
    match mixxx["skipped"].as_str() {
        Some("not requested") => {}
        Some(why) => parts.push(format!("Mixxx: skipped — {why}")),
        None if mixxx.is_object() => {
            let playlist = mixxx["playlist"].as_str().map_or_else(String::new, |p| format!(" in playlist \"{p}\""));
            parts.push(format!("Mixxx: {} track(s){playlist}", mixxx["tracks"].as_u64().unwrap_or(0)));
        }
        None => {}
    }
    match &v["stems"] {
        Value::Array(files) if !files.is_empty() => {
            let written = files.iter().filter(|f| f.get("stem_file").is_some()).count();
            let skipped = files.len().saturating_sub(written);
            let skipped = if skipped == 0 { String::new() } else { format!(" ({skipped} skipped)") };
            parts.push(format!("{written} stem file(s){skipped}"));
        }
        Value::Object(o) => parts.push(format!("stem files: skipped — {}", o.get("skipped").and_then(Value::as_str).unwrap_or("unknown reason"))),
        _ => {}
    }
    let tracks = v["tracks"].as_array().map_or(&[][..], Vec::as_slice);
    let doubts: usize = tracks.iter().map(|t| t["doubts"].as_array().map_or(0, Vec::len)).sum();
    let doubts = match doubts {
        0 => String::new(),
        1 => "; 1 doubt, see the report".into(),
        n => format!("; {n} doubts, see the report"),
    };
    format!("Exported {} track(s) to {root}: {}{doubts}", tracks.len(), parts.join("; "))
}

/// Shows how far the running export got, with Cancel (or Esc), which stops prep after its
/// current track (nothing more is written then).
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &app.export else { return };
    let p = job.progress();
    let stopping = job.cancelling();
    // Esc is taken here, so it doesn't also reach what's behind the window.
    let mut cancel = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    egui::Modal::new(egui::Id::new("export_progress")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.set_width(380.0);
        let title = match (p.stage, &p.current, stopping) {
            ("prep", Some(file), true) => format!("Stopping after {file}"),
            (_, _, true) => "Stopping".to_string(),
            ("prep", Some(file), false) => format!("Prepping {} of {}: {file}", p.done + 1, p.total),
            ("prep", None, false) => format!("Prepping {} of {}", p.done.min(p.total), p.total),
            ("rekordbox", _, _) => "Writing the Rekordbox XML".to_string(),
            ("mixxx", _, _) => "Writing the set into Mixxx".to_string(),
            ("stems", _, _) => "Writing stem files".to_string(),
            _ => "Finishing the export".to_string(),
        };
        ui.label(egui::RichText::new(title).font(bold(13.0)));
        let fraction = if p.stage != "prep" || p.total == 0 { 1.0 } else { p.done as f32 / p.total as f32 };
        ui.add(egui::ProgressBar::new(fraction));
        ui.horizontal(|ui| {
            cancel |= ui.add_enabled(!stopping, egui::Button::new("Cancel")).clicked();
            ui.label(egui::RichText::new(job.out().display().to_string()).color(Tokens::current().text_dim));
        });
    });
    if cancel && !stopping {
        job.cancel();
        app.ui.status = "Stopping the export after the current track".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_say_what_each_target_got() {
        let v = json!({
            "root": "C:/x/set",
            "rekordbox": "C:/x/set/set.rekordbox.xml",
            "mixxx": {"skipped": "Mixxx is running — close it and export again"},
            "stems": [{"file": "a.mp3", "stem_file": "C:/x/set/a.stem.mp4"}, {"file": "b.mp3", "stem_file": "C:/x/set/b.stem.mp4"}],
            "tracks": [{"doubts": []}, {"doubts": ["x"]}],
            "cancelled": false,
        });
        assert_eq!(
            status(&v, 2),
            "Exported 2 track(s) to C:/x/set: Rekordbox XML written; Mixxx: skipped — Mixxx is running — close it and export again; 2 stem file(s); 1 doubt, see the report"
        );
        let v = json!({
            "root": "/x", "rekordbox": {"error": "denied"}, "mixxx": {"playlist": "p", "tracks": 3},
            "stems": {"skipped": "ffmpeg not found"}, "tracks": [{}, {}, {}], "cancelled": false,
        });
        assert_eq!(
            status(&v, 3),
            "Exported 3 track(s) to /x: Rekordbox: failed — denied; Mixxx: 3 track(s) in playlist \"p\"; stem files: skipped — ffmpeg not found"
        );
        let v = json!({"root": "/x", "cancelled": true, "prep": {"tracks": [{"status": "ok"}, {"status": "cancelled"}]}});
        assert_eq!(status(&v, 3), "Export cancelled after 1 of 3 track(s); nothing more was written to /x");
    }
}
