//! Stems in the UI: the clip menu splits a clip on a background job (a fake Demucs, a scratch
//! stem cache), the Mix window shows stem faders on tracks with split clips only, Make Acapella
//! copies a split clip onto another track, and Clear Stem Cache asks before emptying the cache.
//! Nothing here runs Demucs or touches the app's real stem cache.

use egui::{Event, Modifiers, PointerButton, Pos2, pos2, vec2};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode};
use soundcraft_engine::cmd::stems::{StemCache, StemRunner};
use soundcraft_engine::model::{ClipId, STEMS, TrackId};
use soundcraft_ui_egui::dialogs::Dialog;
use soundcraft_ui_egui::{Services, SoundApp, menus};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-ui-stems-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("music")).unwrap();
    dir
}

fn float_wav(buf: &AudioBuffer) -> Vec<u8> {
    encode(buf, &EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None }).unwrap()
}

/// A stereo WAV `dir/music/<name>.wav` of a tone at `level`, half a second. Each test uses its
/// own levels, so no two tests share a stem cache key.
fn source(dir: &Path, name: &str, level: f32) -> PathBuf {
    let frames = RATE as usize / 2;
    let tone = |ch: usize| (0..frames).map(|k| level * (k as f32 * 0.05 + ch as f32).sin()).collect();
    let path = dir.join("music").join(format!("{name}.wav"));
    std::fs::write(&path, float_wav(&AudioBuffer { sample_rate: RATE, channels: vec![tone(0), tone(1)] })).unwrap();
    path
}

/// A fake Demucs: each stem is a quarter of its input, at the input's rate. Counts its runs.
fn fake(calls: &Arc<AtomicUsize>) -> StemRunner {
    let calls = Arc::clone(calls);
    Arc::new(move |input: &Path, out: &Path, _: &dyn Fn() -> bool| {
        calls.fetch_add(1, Ordering::SeqCst);
        let (_, mut buf) = decode(&std::fs::read(input).unwrap(), Some("wav")).unwrap();
        buf.channels.iter_mut().flatten().for_each(|x| *x *= 0.25);
        let dir = out.join("htdemucs").join("source");
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths: [PathBuf; 4] = Default::default();
        for (name, path) in STEMS.iter().zip(paths.iter_mut()) {
            *path = dir.join(format!("{name}.wav"));
            std::fs::write(&*path, float_wav(&buf)).unwrap();
        }
        Ok(paths)
    })
}

/// A fake Demucs that says it started, then waits until told to go on or stopped.
fn gated(started: mpsc::Sender<()>, go: mpsc::Receiver<()>) -> StemRunner {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = fake(&calls);
    let (started, go) = (Mutex::new(started), Mutex::new(go));
    Arc::new(move |input: &Path, out: &Path, stop: &dyn Fn() -> bool| {
        let _ = started.lock().unwrap().send(());
        let until = Instant::now() + Duration::from_secs(60);
        loop {
            if stop() {
                return Err("cancelled".into());
            }
            if go.lock().unwrap().try_recv().is_ok() {
                return inner(input, out, stop);
            }
            assert!(Instant::now() < until, "never told to go on");
            std::thread::sleep(Duration::from_millis(5));
        }
    })
}

fn cache(dir: &Path) -> StemCache {
    StemCache::new(dir.join("cache")).with_free_space(0, Arc::new(|_: &Path| Some(u64::MAX)))
}

/// Two decks: track `a` and track `b`, each holding its own file, in an app splitting into a
/// scratch cache with `runner`.
fn harness(dir: &Path, levels: (f32, f32), runner: StemRunner) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
    for (name, level) in [("a", levels.0), ("b", levels.1)] {
        app.run("file.import_audio", json!({"path": source(dir, name, level)})).unwrap();
    }
    app.engine.set_stems(cache(dir), runner);
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).with_max_steps(1_000).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(5);
    h
}

fn track(h: &Harness<'_, SoundApp>, name: &str) -> TrackId {
    h.state().engine.session().track_by_name(name).unwrap().id
}

fn clip_of(h: &Harness<'_, SoundApp>, name: &str) -> ClipId {
    h.state().engine.session().track_by_name(name).unwrap().clips()[0].id
}

fn split(h: &Harness<'_, SoundApp>, clip: ClipId) -> bool {
    h.state().engine.session().find_clip(clip).unwrap().1.stems.is_some()
}

/// Right-clicks the middle of `clip` in the Edit window, opening the clip menu.
fn clip_menu(h: &mut Harness<'_, SoundApp>, clip: ClipId) {
    let app = h.state();
    let s = app.engine.session();
    let (t, c) = s.find_clip(clip).unwrap();
    let [x0, y0, x1, y1] = app.edit_layout.timeline;
    let tl = egui::Rect::from_min_max(pos2(x0, y0), pos2(x1, y1));
    let x = soundcraft_ui_egui::edit_window::x_of(s, tl, c.start + c.range().len().min(i64::from(RATE) / 4));
    let row = app.edit_layout.rows.iter().find(|(id, _)| *id == t.0).unwrap().1;
    let pos = pos2(x.max(x0 + 4.0), (row[1] + row[3]) / 2.0);
    press(h, pos, PointerButton::Secondary);
    h.run_steps(3);
}

fn press(h: &mut Harness<'_, SoundApp>, pos: Pos2, button: PointerButton) {
    let s = &mut h.input_mut().events;
    s.push(Event::PointerMoved(pos));
    s.push(Event::PointerButton { pos, button, pressed: true, modifiers: Modifiers::NONE });
    s.push(Event::PointerButton { pos, button, pressed: false, modifiers: Modifiers::NONE });
}

fn step_until_split(h: &mut Harness<'_, SoundApp>) {
    let until = Instant::now() + Duration::from_secs(120);
    while h.state().split.is_some() {
        assert!(Instant::now() < until, "the split never ended");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Splits `clip` through the clip menu and waits for the job.
fn split_from_menu(h: &mut Harness<'_, SoundApp>, clip: ClipId) {
    clip_menu(h, clip);
    h.get_by_label("Split into Stems").click();
    h.run_steps(2);
    assert!(h.state().split.is_some(), "{}", h.state().ui.status);
    step_until_split(h);
    assert!(split(h, clip), "{}", h.state().ui.status);
}

#[test]
fn the_clip_menu_splits_a_clip_on_a_job_in_one_undo_step() {
    let dir = scratch("menu");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.31, 0.32), fake(&calls));
    let a = clip_of(&h, "a");
    // An unsplit clip offers only the split.
    clip_menu(&mut h, a);
    assert!(h.query_by_label("Remove Stems").is_none());
    h.get_by_label("Split into Stems").click();
    h.run_steps(2);
    step_until_split(&mut h);
    assert!(split(&h, a));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let status = h.state().ui.status.clone();
    assert!(status.starts_with("Split a into stems in"), "{status}");
    assert!(!split(&h, clip_of(&h, "b")), "the other deck's file was not split");
    // The stems went into the scratch cache.
    assert_eq!(std::fs::read_dir(dir.join("cache")).unwrap().filter(|e| e.as_ref().unwrap().path().is_dir()).count(), 1);
    assert_eq!(h.state().engine.undo_label(), Some("Attach Stems"));
    h.state_mut().run("edit.undo", json!({})).unwrap();
    assert!(!split(&h, a));
    // Splitting again comes from the cache: Demucs doesn't run.
    split_from_menu(&mut h, a);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(h.state().ui.status.contains("from the stem cache"), "{}", h.state().ui.status);
    // A split clip offers the stem items instead.
    h.run_steps(3);
    clip_menu(&mut h, a);
    for item in ["Make Acapella...", "Make Instrumental...", "Stem Mask...", "Remove Stems"] {
        assert!(h.query_by_label_contains(item).is_some(), "{item}");
    }
    assert!(h.query_by_label("Split into Stems").is_none());
    h.get_by_label("Remove Stems").click();
    h.run_steps(2);
    assert!(!split(&h, a));
    assert!(h.state().ui.status.starts_with("Removed stems from 1 clip"), "{}", h.state().ui.status);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn while_a_split_runs_file_commands_and_other_jobs_wait_and_escape_cancels_it() {
    let dir = scratch("busy");
    let (started_tx, started) = mpsc::channel();
    let (go, go_rx) = mpsc::channel();
    let mut h = harness(&dir, (0.33, 0.34), gated(started_tx, go_rx));
    let (a, b) = (clip_of(&h, "a"), clip_of(&h, "b"));
    clip_menu(&mut h, a);
    h.get_by_label("Split into Stems").click();
    h.run_steps(1);
    assert!(h.state().split.is_some(), "{}", h.state().ui.status);
    started.recv_timeout(Duration::from_secs(60)).unwrap();
    h.run_steps(2);
    assert!(h.query_by_label_contains("Splitting a: separating (Demucs)").is_some());
    for (id, params) in [
        ("session.new", json!({})),
        ("file.import_audio", json!({"path": dir.join("music/a.wav")})),
        ("dj.split_stems", json!({"clip": b.0})),
        ("dj.clear_stem_cache", json!({})),
        ("prep.tracks", json!({"source": dir.join("music").to_string_lossy()})),
    ] {
        assert_eq!(h.state_mut().run(id, params).unwrap_err(), "Wait for the stem split to finish (or cancel it)", "{id}");
    }
    let _ = soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": b.0}));
    assert_eq!(h.state().ui.status, "Wait for the stem split to finish (or cancel it)");
    // Editing goes on.
    assert!(h.state_mut().run("edit.select_none", json!({})).is_ok());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().split.as_ref().is_some_and(|j| j.cancelling()));
    step_until_split(&mut h);
    assert_eq!(h.state().ui.status, "Stem split cancelled");
    assert!(!split(&h, a));
    drop(go);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_quit_cancels_the_split_and_waits_for_it() {
    let dir = scratch("quit");
    let (started_tx, started) = mpsc::channel();
    let (_go, go_rx) = mpsc::channel();
    let mut h = harness(&dir, (0.35, 0.36), gated(started_tx, go_rx));
    let a = clip_of(&h, "a");
    assert!(!soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": a.0})).unwrap();
    started.recv_timeout(Duration::from_secs(60)).unwrap();
    assert!(soundcraft_ui_egui::prep::holds_quit(h.state_mut()));
    assert!(h.state().split.as_ref().is_some_and(|j| j.cancelling()));
    step_until_split(&mut h);
    assert!(!h.state().prep_quit, "the quit was not asked again");
    assert!(!split(&h, a));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stem_faders_show_on_split_tracks_only_and_set_the_stem_params() {
    let dir = scratch("faders");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.37, 0.38), fake(&calls));
    h.state_mut().run("window.mix", json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("a vocals stem").is_none(), "no faders before a split");
    let clip = clip_of(&h, "a");
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": clip.0})).unwrap();
    step_until_split(&mut h);
    h.run_steps(3);
    for stem in STEMS {
        assert!(h.query_by_label(&format!("a {stem} stem")).is_some(), "{stem}");
        assert!(h.query_by_label(&format!("b {stem} stem")).is_none(), "{stem} on the unsplit track");
    }
    let a = track(&h, "a");
    let gains = |h: &Harness<'_, SoundApp>| h.state().engine.session().track(a).unwrap().mixer.stem_gain_db;

    // Dragging the vocals fader down lowers `stem:vocals`, as one undo step.
    let r = h.get_by_label("a vocals stem").rect();
    let top = pos2(r.center().x, r.min.y + r.height() * 0.1);
    h.drag_at(top);
    h.run_steps(1);
    for k in 1..=4 {
        h.hover_at(pos2(top.x, top.y + 8.0 * k as f32));
        h.run_steps(1);
    }
    h.drop_at(pos2(top.x, top.y + 32.0));
    h.run_steps(2);
    let vocals = gains(&h)[0];
    assert!(vocals < -1.0, "{vocals}");
    assert_eq!(h.state().engine.undo_label(), Some("Set Stem Fader"));

    // Mute remembers the level and restores it.
    h.state_mut().run("mix.stem_gain", json!({"track": a.0, "stem": "drums", "db": -9.0})).unwrap();
    h.run_steps(2);
    h.get_by_label("a drums stem mute").click();
    h.run_steps(2);
    assert_eq!(gains(&h)[1], -144.0);
    h.get_by_label("a drums stem mute").click();
    h.run_steps(2);
    assert_eq!(gains(&h)[1], -9.0);
    // A stem muted elsewhere un-mutes to 0 dB.
    h.state_mut().run("mix.stem_gain", json!({"track": a.0, "stem": "bass", "db": -144.0})).unwrap();
    h.run_steps(2);
    h.get_by_label("a bass stem mute").click();
    h.run_steps(2);
    assert_eq!(gains(&h)[2], 0.0);

    // Double-click resets to 0 dB.
    let c = h.get_by_label("a vocals stem").rect().center();
    for _ in 0..2 {
        press(&mut h, c, PointerButton::Primary);
        h.run_steps(1);
    }
    h.run_steps(2);
    assert_eq!(gains(&h)[0], 0.0, "was {vocals}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn make_acapella_copies_the_split_clip_onto_the_chosen_track_with_vocals_only() {
    let dir = scratch("acapella");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.39, 0.40), fake(&calls));
    h.state_mut().run("track.new", json!({"kind": "audio", "name": "Mashup"})).unwrap();
    h.run_steps(3);
    let a = clip_of(&h, "a");
    split_from_menu(&mut h, a);
    h.run_steps(3);
    clip_menu(&mut h, a);
    h.get_by_label_contains("Make Acapella...").hover();
    h.run_steps(3);
    h.get_by_label("Mashup").click();
    h.run_steps(2);
    let mashup = h.state().engine.session().track_by_name("Mashup").unwrap();
    let copy = mashup.clips().first().unwrap_or_else(|| panic!("no copy: {}", h.state().ui.status));
    assert_eq!(copy.name, "a (acapella)");
    assert_eq!(copy.stem_mask, [true, false, false, false]);
    assert_eq!(copy.start, h.state().engine.session().find_clip(a).unwrap().1.start);
    assert!(copy.stems.is_some());
    assert_eq!(h.state().ui.status, "Made a (acapella) on Mashup");
    assert_eq!(h.state().engine.session().edit.selected_clips, vec![copy.id]);
    let copy = copy.id;
    // Removing the original's stems keeps the acapella playing its stems.
    soundcraft_ui_egui::stems::remove(h.state_mut(), a);
    assert_eq!(h.state().ui.status, "Removed stems from 1 clip; 1 masked copy keeps its stems");
    assert!(!split(&h, a) && split(&h, copy));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_stem_cache_asks_with_the_size_and_clears_only_on_confirm() {
    let dir = scratch("clear");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.41, 0.42), fake(&calls));
    assert!(menus::ui_aliases().iter().any(|(path, id)| *path == "DJ > Clear Stem Cache..." && *id == "dj.clear_stem_cache"));
    let a = clip_of(&h, "a");
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": a.0})).unwrap();
    step_until_split(&mut h);
    let entries = |dir: &Path| std::fs::read_dir(dir.join("cache")).unwrap().filter(|e| e.as_ref().unwrap().path().is_dir()).count();
    assert_eq!(entries(&dir), 1);

    let dialog = |h: &Harness<'_, SoundApp>| match &h.state().dialogs.open {
        Some(Dialog::ClearStemCache { message, refused }) => (message.clone(), *refused),
        other => panic!("{other:?}"),
    };
    // The session plays the cached stems: the dialog says why it can't clear, and offers no Clear.
    menus::invoke_menu(h.state_mut(), "dj.clear_stem_cache", "DJ > Clear Stem Cache...");
    let (message, refused) = dialog(&h);
    assert!(refused && message.contains("uses cached stems"), "{message}");
    h.run_steps(2);
    assert!(h.query_by_label("Clear").is_none());
    h.get_by_label("OK").click();
    h.run_steps(2);
    assert!(h.state().dialogs.open.is_none());
    assert_eq!(entries(&dir), 1);

    soundcraft_ui_egui::stems::remove(h.state_mut(), a);
    menus::invoke_menu(h.state_mut(), "dj.clear_stem_cache", "DJ > Clear Stem Cache...");
    let (message, refused) = dialog(&h);
    // Four FLACs and their stems.json.
    assert!(!refused && message.starts_with("Delete every cached stem: ") && message.contains(" in 5 file(s)?"), "{message}");
    h.run_steps(2);
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().dialogs.open.is_none());
    assert_eq!(entries(&dir), 1, "Cancel cleared the cache");

    menus::invoke_menu(h.state_mut(), "dj.clear_stem_cache", "DJ > Clear Stem Cache...");
    h.run_steps(2);
    h.get_by_label("Clear").click();
    h.run_steps(2);
    assert!(h.state().ui.status.starts_with("Cleared the stem cache: 5 file(s)"), "{}", h.state().ui.status);
    assert_eq!(entries(&dir), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agents_split_in_the_background_and_clear_the_apps_cache() {
    let dir = scratch("agent");
    let calls = Arc::new(AtomicUsize::new(0));
    // The app's cache and runner are scratch ones before anything runs.
    let mut h = harness(&dir, (0.43, 0.44), fake(&calls));
    let a = clip_of(&h, "a");
    let started = h.state_mut().run("dj.split_stems", json!({"clip": a.0})).unwrap();
    assert_eq!(started, json!({"clip": a.0, "started": true}));
    assert!(h.state().split.is_some(), "the split runs in the background");
    step_until_split(&mut h);
    assert!(split(&h, a));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the app's runner split it");
    let again = h.state_mut().run("dj.split_stems", json!({"clip": a.0})).unwrap();
    assert_eq!(again["already"], true);
    let err = h.state_mut().run("dj.clear_stem_cache", json!({})).unwrap_err();
    assert!(err.contains("uses cached stems"), "{err}");
    soundcraft_ui_egui::stems::remove(h.state_mut(), a);
    let cleared = h.state_mut().run("dj.clear_stem_cache", json!({})).unwrap();
    assert_eq!(cleared["files"], 5, "the app's scratch cache was cleared");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_split_whose_session_was_replaced_is_not_applied() {
    let dir = scratch("replaced");
    let (started_tx, started) = mpsc::channel();
    let (go, go_rx) = mpsc::channel();
    let mut h = harness(&dir, (0.45, 0.46), gated(started_tx, go_rx));
    let a = clip_of(&h, "a");
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": a.0})).unwrap();
    started.recv_timeout(Duration::from_secs(60)).unwrap();
    // Not through `run` (which refuses it): the session is replaced anyway.
    h.state_mut().engine.replace_session(soundcraft_engine::model::Session::default());
    h.run_steps(1);
    go.send(()).unwrap();
    step_until_split(&mut h);
    assert!(h.state().ui.status.contains("after its session was closed"), "{}", h.state().ui.status);
    assert!(h.state().engine.session().tracks.is_empty());
    assert!(!h.state().engine.can_undo());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_automated_stem_fader_shows_its_lane_and_touch_records_stem_moves() {
    let dir = scratch("automated");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.47, 0.48), fake(&calls));
    let clip = clip_of(&h, "a");
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": clip.0})).unwrap();
    step_until_split(&mut h);
    let a = track(&h, "a");
    let app = h.state_mut();
    app.run("window.mix", json!({})).unwrap();
    app.run("automation.set_point", json!({"track": a.0, "param": "stem:bass", "at": 0, "value": -20.0})).unwrap();
    h.run_steps(3);
    let shown = |h: &Harness<'_, SoundApp>, stem: &str| h.get_by_label(&format!("a {stem} stem")).accesskit_node().numeric_value();
    // The lane, not the fader (0 dB), sets the bass: the control shows the lane's value.
    assert_eq!(shown(&h, "bass"), Some(-20.0));
    assert_eq!(shown(&h, "drums"), Some(0.0));

    // Touch records a stem fader move into its lane while playing.
    h.state_mut().run("mix.automation_mode", json!({"track": a.0, "mode": "touch"})).unwrap();
    h.state_mut().run("transport.play", json!({})).unwrap();
    h.run_steps(2);
    let r = h.get_by_label("a vocals stem").rect();
    let top = pos2(r.center().x, r.min.y + r.height() * 0.1);
    h.drag_at(top);
    h.run_steps(1);
    for k in 1..=6 {
        h.hover_at(pos2(top.x, top.y + 6.0 * k as f32));
        h.state_mut().engine.transport.position += 4_800;
        h.run_steps(1);
    }
    h.drop_at(pos2(top.x, top.y + 36.0));
    h.run_steps(2);
    h.state_mut().run("transport.stop", json!({})).unwrap();
    h.run_steps(2);
    let s = h.state().engine.session();
    let points = s.track(a).unwrap().lane(&soundcraft_engine::model::AutoParam::Stem(0)).map_or(0, |l| l.points.len());
    assert!(points >= 2, "{points} stem:vocals points recorded");
    let drums = s.track(a).unwrap().lane(&soundcraft_engine::model::AutoParam::Stem(1));
    assert!(drums.is_none_or(|l| l.points.is_empty()), "an untouched stem is not written");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Write pass rides the volume but writes a stem lane only while its fader moves: the
/// hand-drawn bass lane survives, the moved vocals fader is recorded.
#[test]
fn write_mode_leaves_untouched_stem_lanes_alone() {
    let dir = scratch("write");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.53, 0.54), fake(&calls));
    let clip = clip_of(&h, "a");
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": clip.0})).unwrap();
    step_until_split(&mut h);
    let a = track(&h, "a");
    let app = h.state_mut();
    app.run("window.mix", json!({})).unwrap();
    for (at, v) in [(0, -20.0), (12_000, -6.0)] {
        app.run("automation.set_point", json!({"track": a.0, "param": "stem:bass", "at": at, "value": v})).unwrap();
    }
    let bass = |h: &Harness<'_, SoundApp>| h.state().engine.session().track(a).unwrap().lane(&soundcraft_engine::model::AutoParam::Stem(2)).cloned();
    let drawn = bass(&h);
    h.state_mut().run("mix.automation_mode", json!({"track": a.0, "mode": "write"})).unwrap();
    h.state_mut().run("transport.play", json!({})).unwrap();
    h.run_steps(2);
    let r = h.get_by_label("a vocals stem").rect();
    let top = pos2(r.center().x, r.min.y + r.height() * 0.1);
    h.drag_at(top);
    h.run_steps(1);
    for k in 1..=6 {
        h.hover_at(pos2(top.x, top.y + 6.0 * k as f32));
        h.state_mut().engine.transport.position += 4_800;
        h.run_steps(1);
    }
    h.drop_at(pos2(top.x, top.y + 36.0));
    h.run_steps(2);
    h.state_mut().engine.transport.position += 4_800;
    h.run_steps(2);
    h.state_mut().run("transport.stop", json!({})).unwrap();
    h.run_steps(2);
    assert_eq!(bass(&h), drawn, "the drawn bass lane is untouched");
    let s = h.state().engine.session();
    let t = s.track(a).unwrap();
    let points = |p: soundcraft_engine::model::AutoParam| t.lane(&p).map_or(0, |l| l.points.len());
    assert!(points(soundcraft_engine::model::AutoParam::Stem(0)) >= 2, "the moved vocals fader is written");
    assert!(points(soundcraft_engine::model::AutoParam::Volume) >= 2, "Write rides the volume");
    assert_eq!(points(soundcraft_engine::model::AutoParam::Stem(1)), 0, "drums untouched");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stem_unmuted_elsewhere_forgets_its_old_level() {
    let dir = scratch("forget");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.49, 0.50), fake(&calls));
    let clip = clip_of(&h, "a");
    soundcraft_ui_egui::stems::start(h.state_mut(), &json!({"clip": clip.0})).unwrap();
    step_until_split(&mut h);
    let a = track(&h, "a");
    h.state_mut().run("window.mix", json!({})).unwrap();
    let gain = |h: &mut Harness<'_, SoundApp>, db: f64| {
        h.state_mut().run("mix.stem_gain", json!({"track": a.0, "stem": "drums", "db": db})).unwrap();
        h.run_steps(2);
    };
    gain(&mut h, -9.0);
    h.get_by_label("a drums stem mute").click();
    h.run_steps(2);
    // Un-muted and muted again by commands: the -9 dB is stale.
    gain(&mut h, -3.0);
    gain(&mut h, -144.0);
    h.get_by_label("a drums stem mute").click();
    h.run_steps(2);
    assert_eq!(h.state().engine.session().track(a).unwrap().mixer.stem_gain_db[1], 0.0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_running_prep_does_not_hold_back_clear_stem_cache() {
    let dir = scratch("prep-clear");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut h = harness(&dir, (0.51, 0.52), fake(&calls));
    // A prep of one fixture that waits in its analysis until told to go on.
    let src = dir.join("set");
    std::fs::create_dir_all(&src).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr64_44k_mono.mp3");
    std::fs::copy(fixture, src.join("one.mp3")).unwrap();
    let (reached_tx, reached) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let (reached_tx, go_rx) = (Mutex::new(reached_tx), Mutex::new(go_rx));
    let analyse: soundcraft_engine::cmd::PrepAnalyse = Arc::new(move |_: &str, _: &dyn Fn() -> bool| {
        reached_tx.lock().unwrap().send(()).unwrap();
        go_rx.lock().unwrap().recv_timeout(Duration::from_secs(60)).unwrap();
        Err::<serde_json::Value, String>("no analysis here".into())
    });
    let s = soundcraft_engine::model::Session::default();
    let out = dir.join("out");
    let job = soundcraft_engine::cmd::prep_start(&s, src.to_str().unwrap(), Some(out.to_str().unwrap()), -14.0, -1.0, analyse).unwrap();
    h.state_mut().prep = Some(job);
    reached.recv_timeout(Duration::from_secs(60)).unwrap();
    // Splitting waits for the prep; clearing the (scratch) cache doesn't.
    let clip = clip_of(&h, "a");
    let err = h.state_mut().run("dj.split_stems", json!({"clip": clip.0})).unwrap_err();
    assert_eq!(err, "Wait for the prep to finish (or cancel it)");
    let cleared = h.state_mut().run("dj.clear_stem_cache", json!({})).unwrap();
    assert_eq!(cleared["files"], 0);
    menus::invoke_menu(h.state_mut(), "dj.clear_stem_cache", "DJ > Clear Stem Cache...");
    assert!(matches!(&h.state().dialogs.open, Some(Dialog::ClearStemCache { refused: false, .. })));
    go.send(()).unwrap();
    let until = Instant::now() + Duration::from_secs(120);
    while h.state().prep.is_some() {
        assert!(Instant::now() < until, "the prep never ended");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
