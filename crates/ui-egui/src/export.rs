//! Export Set runs `dj.export_set` on its own thread behind a progress window (like Prep Tracks),
//! so the app keeps drawing and the export can be cancelled; when it ends, the status line and a
//! result window say what each target and each track got.

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What a finished export wrote, for its result window.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportResult {
    /// The status line it ended with.
    pub summary: String,
    /// The export folder.
    pub root: PathBuf,
    /// Each track's file and its doubts, in set order.
    pub tracks: Vec<(String, Vec<String>)>,
}

/// Starts the export `dj.export_set` runs for `params` (without `out`, into a folder in
/// `app.prep_root`, analysed by `app.prep_analyse`; Mixxx through the engine's Mixxx runner),
/// from the Export Set dialog or `app.run` (agents, scripts): its quick checks are made here, the
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
    app.export_result = None;
    app.ui.status = format!("Exporting the set to {}", root.display());
    Ok(json!({"root": root, "started": true}))
}

/// Collects the result once the export has ended: the status line, and the result window when
/// it got past prep. Called from `logic`, which eframe keeps calling while a repaint is
/// requested, so the result arrives with the window hidden too. The export works on a snapshot
/// of the session taken when it started and changes nothing in it, so its result stands
/// whatever happened to the session meanwhile.
pub fn step(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &mut app.export else { return };
    let total = job.progress().total;
    let Some(result) = job.poll() else {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
    };
    match result {
        Ok(v) => {
            let summary = status(&v, total);
            app.ui.status.clone_from(&summary);
            app.export_result = v["tracks"].as_array().map(|tracks| ExportResult {
                summary,
                root: PathBuf::from(v["root"].as_str().unwrap_or_default()),
                tracks: tracks
                    .iter()
                    .map(|t| {
                        let doubts =
                            t["doubts"].as_array().map_or_else(Vec::new, |d| d.iter().filter_map(Value::as_str).map(str::to_string).collect());
                        (t["file"].as_str().unwrap_or_default().to_string(), doubts)
                    })
                    .collect(),
            });
        }
        Err(e) => app.ui.status = e.to_string(),
    }
    app.export = None;
    crate::prep::quit_when_idle(app, ctx);
}

/// `names` as "a.mp3, b.mp3, c.mp3, …" (the first three).
fn first_names(names: &[&str]) -> String {
    let mut s = names.iter().take(3).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 3 {
        s.push_str(", …");
    }
    s
}

/// The files of `tracks` for which `pick` holds.
fn files_where(tracks: &[Value], pick: impl Fn(&Value) -> bool) -> Vec<&str> {
    tracks.iter().filter(|t| pick(t)).map(|t| t["file"].as_str().unwrap_or("?")).collect()
}

/// "N <what> (a.mp3, …)", or nothing for no files.
fn counted(files: &[&str], what: &str) -> Option<String> {
    (!files.is_empty()).then(|| format!("{} {what} ({})", files.len(), first_names(files)))
}

/// The status line after `dj.export_set` (of `total` tracks to prep): what each target got, the
/// tracks it couldn't do everything for, the report and the export folder.
pub(crate) fn status(v: &Value, total: usize) -> String {
    let root = v["root"].as_str().unwrap_or_default();
    let cancelled = v["cancelled"].as_bool().unwrap_or(false);
    let Some(tracks) = v["tracks"].as_array() else {
        // Cancelled before anything but prep's copies was written.
        let rows = v["prep"]["tracks"].as_array().map_or(&[][..], Vec::as_slice);
        // A track cancelled part-way was not prepped.
        let done = rows.iter().filter(|t| t["status"] != "cancelled").count();
        return if v["prep"]["cancelled"].as_bool().unwrap_or(false) || !cancelled {
            format!("Export cancelled after {done} of {total} track(s); nothing more was written to {root}")
        } else {
            format!("Export cancelled after prep ({done} of {total} track(s) prepped into {root}); no XML, Mixxx or stem files were written")
        };
    };
    let mut parts = Vec::new();
    parts.extend(counted(&files_where(tracks, |t| t["found"] == false), "not found"));
    parts.extend(counted(&files_where(tracks, |t| t["prep_refused"].is_string()), "refused by prep"));
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
            parts.extend(counted(&files_where(tracks, |t| t["in_mixxx"] == false), "left out of Mixxx"));
        }
        None => {}
    }
    match &v["stems"] {
        Value::Array(files) if !files.is_empty() => {
            let written = files.iter().filter(|f| f.get("stem_file").is_some()).count();
            let mut line = format!("{written} stem file(s)");
            let skipped: Vec<String> =
                files.iter().filter_map(|f| f["skipped"].as_str().map(|why| format!("{}: {why}", f["file"].as_str().unwrap_or("?")))).collect();
            if !skipped.is_empty() {
                let more = if skipped.len() > 2 { "; …" } else { "" };
                line.push_str(&format!(", {} skipped ({}{more})", skipped.len(), skipped.iter().take(2).cloned().collect::<Vec<_>>().join("; ")));
            }
            parts.push(line);
        }
        Value::Object(o) => parts.push(format!("stem files: skipped — {}", o.get("skipped").and_then(Value::as_str).unwrap_or("unknown reason"))),
        _ => {}
    }
    let doubts: usize = tracks.iter().map(|t| t["doubts"].as_array().map_or(0, Vec::len)).sum();
    match &v["report"] {
        Value::String(p) => {
            let name = Path::new(p).file_name().map_or_else(|| p.clone(), |n| n.to_string_lossy().into_owned());
            let doubts = match doubts {
                0 => String::new(),
                1 => "1 doubt; ".into(),
                n => format!("{n} doubts; "),
            };
            parts.push(format!("{doubts}see {name}"));
        }
        Value::Null => {}
        other => parts.push(format!("report not written: {}", other["error"].as_str().unwrap_or("unknown error"))),
    }
    let head = if cancelled { "Export cancelled; written before the cancel: " } else { "" };
    format!("{head}Exported {} track(s) to {root}: {}", tracks.len(), parts.join("; "))
}

/// Shows how far the running export got, with Cancel (or Esc), which stops it at the next point
/// it can; while mixforge writes Mixxx's library it can't be stopped, and the window says so.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &app.export else { return };
    let p = job.progress();
    let stopping = job.cancelling();
    let in_mixxx = p.stage == "mixxx";
    // Esc is taken here, so it doesn't also reach what's behind the window.
    let mut cancel = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) && !in_mixxx;
    egui::Modal::new(egui::Id::new("export_progress")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.set_width(400.0);
        let title = match (p.stage, &p.current, stopping) {
            ("mixxx", _, _) => "Writing the set into Mixxx — can't stop now".to_string(),
            ("prep", Some(file), true) => format!("Stopping after {file}"),
            (_, _, true) => "Stopping".to_string(),
            ("prep", Some(file), false) => format!("Prepping {} of {}: {file}", p.done + 1, p.total),
            ("prep", None, false) => format!("Prepping {} of {}", p.done.min(p.total), p.total),
            ("rekordbox", _, _) => "Writing the Rekordbox XML".to_string(),
            ("stems", _, _) => "Writing stem files".to_string(),
            _ => "Finishing the export".to_string(),
        };
        ui.label(egui::RichText::new(title).font(bold(13.0)));
        if p.stage == "prep" {
            ui.add(egui::ProgressBar::new(if p.total == 0 { 0.0 } else { p.done as f32 / p.total as f32 }));
        }
        ui.horizontal(|ui| {
            if p.stage != "prep" {
                // No count to show: the stage takes as long as it takes.
                ui.spinner();
            }
            cancel |= ui.add_enabled(!stopping && !in_mixxx, egui::Button::new("Cancel")).clicked();
            ui.label(egui::RichText::new(job.out().display().to_string()).color(Tokens::current().text_dim));
        });
    });
    if cancel && !stopping {
        job.cancel();
        app.ui.status = "Stopping the export".into();
    }
}

/// The window a finished export leaves: what it did, the folder (with Open folder) and each
/// track's doubts.
pub fn show_result(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(r) = &app.export_result else { return };
    let mut open = true;
    let mut close = false;
    let mut opened: Option<Result<(), String>> = None;
    egui::Window::new(egui::RichText::new("Export Finished").font(bold(13.0)))
        .collapsible(false)
        .resizable(true)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -60.0))
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
            ui.set_max_width(560.0);
            ui.label(r.summary.as_str());
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Folder:");
                ui.label(egui::RichText::new(r.root.display().to_string()).monospace());
            });
            let doubtful: Vec<&(String, Vec<String>)> = r.tracks.iter().filter(|(_, d)| !d.is_empty()).collect();
            if doubtful.is_empty() {
                ui.label(egui::RichText::new("No doubts about any track.").color(Tokens::current().text_dim));
            } else {
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for (file, doubts) in doubtful {
                        ui.label(egui::RichText::new(file.as_str()).strong());
                        for d in doubts {
                            ui.label(format!("  • {d}"));
                        }
                    }
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Open folder").clicked() {
                    opened = Some(open_folder(&r.root));
                }
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
        });
    if let Some(Err(e)) = opened {
        log::warn!("open folder: {e}");
        app.ui.status = e;
    }
    if close || !open {
        app.export_result = None;
    }
}

/// Opens `folder` in the system's file browser (Explorer, Finder, or `xdg-open`'s), without
/// waiting for it.
fn open_folder(folder: &Path) -> Result<(), String> {
    if cfg!(target_arch = "wasm32") {
        return Err("Opening a folder isn't possible here".into());
    }
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program).arg(folder).spawn().map_err(|e| format!("Couldn't open {}: {e}", folder.display()))?;
    // Reaped on a thread of its own, so it never lingers.
    let _ = std::thread::Builder::new().name("open-folder".into()).spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_say_what_each_target_and_track_got() {
        let v = json!({
            "root": "C:/x/set",
            "rekordbox": "C:/x/set/set.rekordbox.xml",
            "report": "C:/x/set/set.export.txt",
            "mixxx": {"skipped": "Mixxx is running — close it and export again"},
            "stems": [
                {"file": "a.mp3", "stem_file": "C:/x/set/a.stem.mp4"},
                {"file": "b.mp3", "skipped": "its stems are not loaded; split the clip again"},
            ],
            "tracks": [
                {"file": "a.mp3", "found": true, "doubts": []},
                {"file": "b.mp3", "found": true, "prep_refused": "another track has its name", "doubts": ["x"]},
            ],
            "cancelled": false,
        });
        assert_eq!(
            status(&v, 2),
            "Exported 2 track(s) to C:/x/set: 1 refused by prep (b.mp3); Rekordbox XML written; Mixxx: skipped — Mixxx is running — close it and export again; 1 stem file(s), 1 skipped (b.mp3: its stems are not loaded; split the clip again); 1 doubt; see set.export.txt"
        );
        let five: Vec<Value> =
            ["a", "b", "c", "d", "e"].iter().map(|f| json!({"file": format!("{f}.mp3"), "found": false, "in_mixxx": false})).collect();
        let v = json!({
            "root": "/x", "rekordbox": {"error": "denied"}, "mixxx": {"playlist": "p", "tracks": 3},
            "stems": {"skipped": "ffmpeg not found"}, "report": {"error": "disk full"}, "cancelled": false,
            "tracks": five,
        });
        assert_eq!(
            status(&v, 5),
            "Exported 5 track(s) to /x: 5 not found (a.mp3, b.mp3, c.mp3, …); Rekordbox: failed — denied; Mixxx: 3 track(s) in playlist \"p\"; 5 left out of Mixxx (a.mp3, b.mp3, c.mp3, …); stem files: skipped — ffmpeg not found; report not written: disk full"
        );
        let v = json!({"root": "/x", "cancelled": true, "prep": {"cancelled": true, "tracks": [{"status": "ok"}, {"status": "cancelled"}]}});
        assert_eq!(status(&v, 3), "Export cancelled after 1 of 3 track(s); nothing more was written to /x");
        let v = json!({"root": "/x", "cancelled": true, "prep": {"cancelled": false, "tracks": [{"status": "ok"}, {"status": "ok"}]}});
        assert_eq!(status(&v, 2), "Export cancelled after prep (2 of 2 track(s) prepped into /x); no XML, Mixxx or stem files were written");
    }
}
