//! Stems: Split into Stems runs Demucs on its own thread behind a progress window (like Prep
//! Tracks), the clip menu's stem items, the mixer strip's four stem faders and Clear Stem Cache.

use crate::SoundApp;
use crate::theme::{Tokens, bold};
use crate::widgets::db_text;
use egui::{Align2, Color32, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_engine::cmd::stems;
use soundcraft_model::{ClipId, STEMS, Session, Track, TrackId, TrackKind};

/// The mask of Make Acapella: vocals only.
const ACAPELLA: [bool; 4] = [true, false, false, false];
/// The mask of Make Instrumental: all but vocals.
const INSTRUMENTAL: [bool; 4] = [false, true, true, true];
/// The faders' labels, in [`STEMS`] order.
const LETTERS: [&str; 4] = ["V", "D", "B", "O"];
/// A stem fader at or below this is muted (−inf).
const SILENT_DB: f32 = -143.9;
/// The stem faders' range, as `AutoParam::Stem` has it.
const MIN_DB: f32 = -144.0;
const MAX_DB: f32 = 6.0;
/// Height of the mixer strip's stem section (kept on every strip once any track has a split
/// clip, so the strips' rows line up).
pub(crate) const FADERS_H: f32 = 104.0;

/// Starts `dj.split_stems` for `params` (`{clip}`) in the engine's stem cache with its runner
/// (`Engine::set_stems`: the desktop app sets the real ones, tests a scratch cache),
/// from the clip menu or `app.run` (agents, scripts): the checks are made here, the split runs
/// on another thread and [`step`] applies it. Returns `{clip, started: true}`, or
/// `{clip, already: true, key}` when the clip is split already; a refusal also goes to the
/// status line.
pub fn start(app: &mut SoundApp, params: &Value) -> Result<Value, String> {
    let r = start_job(app, params);
    if let Err(e) = &r {
        app.ui.status.clone_from(e);
        log::warn!("dj.split_stems: {e}");
    }
    r
}

fn start_job(app: &mut SoundApp, params: &Value) -> Result<Value, String> {
    if crate::prep::refuses_command(app, "dj.split_stems") {
        return Err(app.ui.status.clone());
    }
    match stems::split_job(&app.engine, params).map_err(|e| e.to_string())? {
        Some(job) => {
            let clip = job.clip();
            app.split = Some(job);
            app.split_generation = app.session_generation;
            Ok(json!({"clip": clip.0, "started": true}))
        }
        None => {
            let clip = params.get("clip").and_then(Value::as_u64).map(ClipId);
            let s = app.engine.session();
            let key = clip.and_then(|c| s.find_clip(c)).and_then(|(_, c)| c.stems.as_ref()).map(|set| set.key.clone());
            app.ui.status = format!("{} is split already", clip.map_or_else(String::new, |c| clip_name(s, c)));
            Ok(json!({"clip": clip.map(|c| c.0), "already": true, "key": key}))
        }
    }
}

/// Applies the split once it has ended (as one undoable `dj.attach_stems`), unless the session
/// it started in was replaced meanwhile. Called from `logic`, which eframe keeps calling while a
/// repaint is requested.
pub fn step(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &mut app.split else { return };
    let Some(result) = job.poll() else {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
    };
    let name = clip_name(app.engine.session(), job.clip());
    app.split = None;
    let same_session = app.split_generation == app.session_generation;
    app.ui.status = match result {
        Ok(_) if !same_session => "The stem split ended after its session was closed: nothing was changed".into(),
        Ok(split) => match split.apply(&mut app.engine) {
            Ok(v) => status(&v, &name),
            Err(e) => e.to_string(),
        },
        Err(e) if e.to_string() == stems::CANCELLED => "Stem split cancelled".into(),
        Err(e) => e.to_string(),
    };
    crate::prep::quit_when_idle(app, ctx);
}

/// The status line after a split of clip `name` (what `dj.split_stems` returns).
pub(crate) fn status(v: &Value, name: &str) -> String {
    if v["already"].as_bool().unwrap_or(false) {
        return format!("{name} is split already");
    }
    let others = v["clips"].as_array().map_or(0, |c| c.len().saturating_sub(1));
    let others = match others {
        0 => String::new(),
        1 => " and 1 other clip of its audio".into(),
        n => format!(" and {n} other clips of its audio"),
    };
    let how = if v["hit"].as_bool().unwrap_or(false) {
        "from the stem cache".to_string()
    } else {
        format!("in {:.0} s", v["seconds"].as_f64().unwrap_or(0.0))
    };
    format!("Split {name}{others} into stems {how}")
}

fn clip_name(s: &Session, clip: ClipId) -> String {
    s.find_clip(clip).map_or_else(|| format!("clip {}", clip.0), |(_, c)| c.name.clone())
}

/// Shows what the running split is doing, with Cancel (or Esc), which stops Demucs.
pub fn show(app: &mut SoundApp, ctx: &egui::Context) {
    let Some(job) = &app.split else { return };
    let stage = job.progress().stage;
    let stopping = job.cancelling();
    let name = clip_name(app.engine.session(), job.clip());
    // Esc is taken here, so it doesn't also reach what's behind the window.
    let mut cancel = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    egui::Modal::new(egui::Id::new("split_progress")).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        ui.set_width(360.0);
        let title = if stopping { format!("Stopping the split of {name}") } else { format!("Splitting {name}: {stage}") };
        ui.label(egui::RichText::new(title).font(bold(13.0)));
        ui.horizontal(|ui| {
            ui.spinner();
            cancel |= ui.add_enabled(!stopping, egui::Button::new("Cancel")).clicked();
            ui.label(egui::RichText::new("vocals, drums, bass and other").color(Tokens::current().text_dim));
        });
    });
    if cancel && !stopping {
        job.cancel();
        app.ui.status = "Stopping the stem split".into();
    }
}

/// The clip menu's stem items for audio clip `clip`: Split into Stems, or on a split clip Make
/// Acapella / Make Instrumental (onto another audio track), Stem Mask and Remove Stems.
pub(crate) fn clip_menu(app: &mut SoundApp, ui: &mut Ui, clip: ClipId) {
    let s = app.engine.session_arc();
    let Some((own, c)) = s.find_clip(clip) else { return };
    if c.source().is_none() {
        return;
    }
    ui.separator();
    if c.stems.is_none() {
        let busy = app.prep.is_some() || app.split.is_some();
        let resp = ui.add_enabled(!busy, egui::Button::new("Split into Stems"));
        if resp.clicked() {
            let _ = start(app, &json!({"clip": clip.0}));
        }
        resp.on_disabled_hover_text("Wait for the running job to finish");
        return;
    }
    let targets: Vec<(TrackId, &str)> =
        s.tracks.iter().filter(|t| t.kind == TrackKind::Audio && t.id != own).map(|t| (t.id, t.name.as_str())).collect();
    for (label, mask) in [("Make Acapella...", ACAPELLA), ("Make Instrumental...", INSTRUMENTAL)] {
        ui.menu_button(label, |ui| {
            if targets.is_empty() {
                ui.label(egui::RichText::new("No other audio track").color(Tokens::current().text_dim));
            }
            for (track, name) in &targets {
                if ui.button(*name).clicked() {
                    copy(app, clip, *track, mask);
                }
            }
        });
    }
    ui.menu_button("Stem Mask...", |ui| {
        let playing = c.stem_mask.iter().filter(|on| **on).count();
        for (i, (name, on)) in STEMS.iter().zip(c.stem_mask).enumerate() {
            let mut checked = on;
            // The last stem playing stays: a clip plays at least one.
            if ui.add_enabled(!(on && playing == 1), egui::Checkbox::new(&mut checked, title(name))).changed() {
                let mut mask = c.stem_mask;
                if let Some(m) = mask.get_mut(i) {
                    *m = checked;
                }
                let _ = app.run("clip.stem_mask", json!({"clip": clip.0, "mask": names(mask)}));
            }
        }
    });
    if ui.button("Remove Stems").clicked() {
        remove(app, clip);
    }
}

/// `dj.stem_copy`: copies split clip `clip` to `track` playing the stems in `mask`, and selects
/// the copy.
pub fn copy(app: &mut SoundApp, clip: ClipId, track: TrackId, mask: [bool; 4]) {
    if let Ok(v) = app.run("dj.stem_copy", json!({"clip": clip.0, "track": track.0, "mask": names(mask)})) {
        let track = app.engine.session().track(track).map(|t| t.name.clone()).unwrap_or_default();
        app.ui.status = format!("Made {} on {track}", v["name"].as_str().unwrap_or("the copy"));
        let _ = app.run("edit.select", json!({"clips": [v["clip"]]}));
    }
}

/// `clip.unsplit_stems` on `clip`; the status line says how many clips play their own audio again.
pub fn remove(app: &mut SoundApp, clip: ClipId) {
    if let Ok(v) = app.run("clip.unsplit_stems", json!({"clip": clip.0})) {
        app.ui.status = remove_status(&v);
    }
}

/// The status line after `clip.unsplit_stems`.
pub(crate) fn remove_status(v: &Value) -> String {
    let count = |key: &str| v[key].as_array().map_or(0, Vec::len);
    let clips = match count("clips") {
        1 => "1 clip".to_string(),
        n => format!("{n} clips"),
    };
    let kept = match count("kept") {
        0 => String::new(),
        1 => "; 1 masked copy keeps its stems".into(),
        n => format!("; {n} masked copies keep their stems"),
    };
    format!("Removed stems from {clips}{kept}")
}

/// The stem names `mask` plays.
fn names(mask: [bool; 4]) -> Vec<&'static str> {
    STEMS.iter().zip(mask).filter(|(_, on)| *on).map(|(n, _)| *n).collect()
}

/// "vocals" → "Vocals".
fn title(name: &str) -> String {
    let mut c = name.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// Whether `track` is an audio track with a split clip in its playing playlist: only those
/// show stem faders.
pub fn has_split(track: &Track) -> bool {
    track.kind == TrackKind::Audio && track.clips().iter().any(|c| c.stems.is_some())
}

/// Whether any track shows stem faders.
pub(crate) fn any_split(s: &Session) -> bool {
    s.tracks.iter().any(has_split)
}

/// Sets stem `i` of `track` to `db` (`mix.stem_gain`), merged into one undo step per drag.
pub fn set_gain(app: &mut SoundApp, track: TrackId, i: usize, db: f32) {
    let start_db = app.engine.session().track(track).and_then(|t| t.mixer.stem_gain_db.get(i).copied()).unwrap_or(0.0);
    // A gesture, so Touch/Latch/Write record the move into the `stem:*` lane while playing.
    app.gesture = Some(crate::Gesture::Fader { track, start_db, stem: u8::try_from(i).ok() });
    let params = json!({"track": track.0, "stem": STEMS.get(i).copied().unwrap_or_default(), "db": db});
    let _ = app.engine.execute_merged("mix.stem_gain", &params, &format!("stem:{}:{i}", track.0));
}

/// The stem fader's mute button: mutes stem `i` of `track` (−inf), remembering its level, or
/// un-mutes it back to that level (0 dB when none is remembered). Its own undo step.
pub fn toggle_mute(app: &mut SoundApp, track: TrackId, i: usize) {
    let Some(db) = app.engine.session().track(track).and_then(|t| t.mixer.stem_gain_db.get(i).copied()) else { return };
    let to = if db <= SILENT_DB {
        app.stem_unmute.remove(&(track, i)).filter(|v| *v > SILENT_DB).unwrap_or(0.0)
    } else {
        app.stem_unmute.insert((track, i), db);
        MIN_DB
    };
    app.engine.end_merge();
    let stem = STEMS.get(i).copied().unwrap_or_default();
    let _ = app.run("mix.stem_gain", json!({"track": track.0, "stem": stem, "db": to}));
}

/// Fader position (0..1) of `db`, with +6 dB at the top.
fn pos_of(db: f32) -> f32 {
    let top = soundcraft_model::fader_db_to_pos(MAX_DB).max(f32::EPSILON);
    (soundcraft_model::fader_db_to_pos(db) / top).clamp(0.0, 1.0)
}

fn db_of(pos: f32) -> f32 {
    let top = soundcraft_model::fader_db_to_pos(MAX_DB);
    soundcraft_model::fader_pos_to_db(pos.clamp(0.0, 1.0) * top).clamp(MIN_DB, MAX_DB)
}

/// The mixer strip's stem section for `track` in `sec`: four small faders V, D, B and O
/// (−inf to +6 dB; double-click resets to 0 dB), each with a mute button.
pub(crate) fn faders(app: &mut SoundApp, ui: &mut Ui, track: &Track, sec: Rect) {
    let t = Tokens::current();
    crate::widgets::inset(ui.painter(), sec, t.strip_section);
    let id = track.id;
    let cw = (sec.width() - 4.0) / 4.0;
    for (i, ((letter, name), db)) in LETTERS.iter().zip(STEMS).zip(track.mixer.stem_gain_db).enumerate() {
        let col = Rect::from_min_size(pos2(sec.min.x + 2.0 + i as f32 * cw, sec.min.y), vec2(cw, sec.height()));
        let p = ui.painter().clone();
        p.text(pos2(col.center().x, col.min.y + 7.0), Align2::CENTER_CENTER, *letter, bold(9.0), t.text_dim);
        let area = Rect::from_min_max(pos2(col.min.x, col.min.y + 14.0), pos2(col.max.x, col.max.y - 21.0));
        let slot = Rect::from_center_size(area.center(), vec2(3.0, area.height() - 6.0));
        p.rect_filled(slot, 1.0, t.fader_track);
        let zero = slot.max.y - slot.height() * pos_of(0.0);
        p.hline(slot.min.x - 4.0..=slot.max.x + 4.0, zero, Stroke::new(1.0, t.text_dim.gamma_multiply(0.6)));
        let muted = db <= SILENT_DB;
        if !muted {
            // Un-muted (here or anywhere else): no level is waiting to be restored.
            app.stem_unmute.remove(&(id, i));
        }
        // A `stem:*` lane the track reads overrides the fader: the cap shows the lane's value
        // at the playhead, dimmed, with an "A".
        let automated = soundcraft_mix::stem_lane(track, i).is_some();
        let shown = if automated { soundcraft_mix::stem_db_at(track, i, app.position()) } else { db };
        let y = slot.max.y - slot.height() * pos_of(shown);
        let cap = Rect::from_center_size(pos2(slot.center().x, y), vec2((cw - 6.0).clamp(6.0, 16.0), 6.0));
        let face = match (automated, muted) {
            (true, _) => Color32::from_gray(130).gamma_multiply(0.8),
            (false, true) => Color32::from_gray(90),
            (false, false) => Color32::from_gray(210),
        };
        p.rect_filled(cap, 1.5, face);
        p.hline(cap.x_range(), y, Stroke::new(1.0, Color32::from_gray(30)));
        if automated {
            p.text(pos2(col.max.x - 1.0, col.min.y + 1.0), Align2::RIGHT_TOP, "A", bold(8.0), t.auto_read);
        }
        let resp = ui.interact(area, ui.id().with(("stem_fader", id.0, i)), Sense::click_and_drag());
        let label = format!("{} {name} stem", track.name);
        resp.widget_info(|| egui::WidgetInfo::slider(true, f64::from(shown), &label));
        let hover = if automated {
            format!("{} {}: {} dB, automated by the stem:{name} lane (the fader is {} dB)", track.name, title(name), db_text(shown), db_text(db))
        } else {
            format!("{} {}: {} dB (double-click: 0 dB)", track.name, title(name), db_text(db))
        };
        let resp = resp.on_hover_text(hover);
        if resp.double_clicked() {
            set_gain(app, id, i, 0.0);
        } else if resp.dragged() {
            let fine = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
            let dy = resp.drag_delta().y / slot.height().max(1.0) * if fine { 0.1 } else { 1.0 };
            set_gain(app, id, i, db_of(pos_of(db) - dy));
        }
        if resp.drag_stopped() {
            app.engine.end_merge();
        }
        let mr = Rect::from_center_size(pos2(col.center().x, col.max.y - 10.0), vec2((cw - 4.0).min(18.0), 14.0));
        let mut c = ui.new_child(egui::UiBuilder::new().id_salt(("stem_mute", id.0, i)).max_rect(mr));
        let mute = crate::widgets::text_toggle(&mut c, mr.size(), "M", muted, t.mute, &format!("Mute {name} (click again for its level)"));
        let mute_label = format!("{} {name} stem mute", track.name);
        mute.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, muted, &mute_label));
        if mute.clicked() {
            toggle_mute(app, id, i);
        }
    }
}

/// DJ › Clear Stem Cache...: asks first, saying what the cache holds, or says why it can't
/// be cleared now (a split into it runs, or the session plays its stems).
pub fn ask_clear(app: &mut SoundApp) {
    if crate::prep::refuses_command(app, "dj.clear_stem_cache") {
        return;
    }
    let (message, refused) = match app.engine.stems().map(|x| &x.cache) {
        Some(cache) => match stems::clear_stem_cache_refusal(&app.engine, cache) {
            Some(why) => (format!("The stem cache can't be cleared now: {why}."), true),
            None => (clear_message(stems::stem_cache_size(cache)), false),
        },
        None => {
            app.ui.status = "No stem cache is set up".into();
            return;
        }
    };
    app.dialogs.open = Some(crate::dialogs::Dialog::ClearStemCache { message, refused });
}

/// The Clear Stem Cache question for a cache of `size` (files, bytes).
pub(crate) fn clear_message(size: Result<(usize, u64), String>) -> String {
    let what = match size {
        Ok((0, _)) => return "The stem cache is empty.".into(),
        Ok((files, bytes)) => format!("{} in {files} file(s)", bytes_text(bytes)),
        Err(e) => format!("an unknown amount ({e})"),
    };
    format!("Delete every cached stem: {what}? Clips split from them would have to be split again. This can't be undone.")
}

/// `bytes` as KB, MB or GB.
pub(crate) fn bytes_text(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

/// Runs Clear Stem Cache (`dj.clear_stem_cache`) on the engine's stem cache: after the confirm, or
/// from `app.run`. Returns `{files, bytes}`; the status line says what was freed, or why not.
pub fn clear(app: &mut SoundApp) -> Result<Value, String> {
    if crate::prep::refuses_command(app, "dj.clear_stem_cache") {
        return Err(app.ui.status.clone());
    }
    let result = app.engine.execute("dj.clear_stem_cache", &json!({})).map_err(|e| e.to_string());
    app.ui.status = match &result {
        Ok(v) => format!("Cleared the stem cache: {} file(s), {}", v["files"], bytes_text(v["bytes"].as_u64().unwrap_or(0))),
        Err(e) => {
            log::warn!("dj.clear_stem_cache: {e}");
            e.clone()
        }
    };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_faders_reach_plus_six_at_the_top_and_silence_at_the_bottom() {
        assert_eq!(pos_of(MAX_DB), 1.0);
        assert_eq!(pos_of(MIN_DB), 0.0);
        assert!((db_of(pos_of(0.0))).abs() < 0.01);
        assert!((db_of(pos_of(-12.0)) + 12.0).abs() < 0.05);
        assert_eq!(db_of(2.0), MAX_DB);
        assert_eq!(db_of(-1.0), MIN_DB);
    }

    #[test]
    fn status_lines_say_what_happened() {
        assert_eq!(status(&json!({"clips": [1], "hit": true}), "Song"), "Split Song into stems from the stem cache");
        assert_eq!(
            status(&json!({"clips": [1, 2], "hit": false, "seconds": 12.4}), "Song"),
            "Split Song and 1 other clip of its audio into stems in 12 s"
        );
        assert_eq!(status(&json!({"already": true}), "Song"), "Song is split already");
        assert_eq!(remove_status(&json!({"clips": [1, 2], "kept": [3]})), "Removed stems from 2 clips; 1 masked copy keeps its stems");
        assert_eq!(remove_status(&json!({"clips": [1]})), "Removed stems from 1 clip");
        assert_eq!(clear_message(Ok((0, 0))), "The stem cache is empty.");
        assert!(clear_message(Ok((8, 1_500_000_000))).contains("1.5 GB in 8 file(s)"));
    }
}
