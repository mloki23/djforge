//! `dj.export_set` hands the set to Mixxx through the engine's Mixxx runner: mixforge's plan of
//! the prepped copies, with the grid and cues in file seconds. Every runner here is a fake; no
//! test runs mixforge or touches Mixxx's database.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions, encode};
use soundcraft_engine::Engine;
use soundcraft_engine::cmd::{MixxxRunner, PrepAnalyse, export_job_in};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-export-mixxx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// mixforge's analysis, faked: every track is 8A at 120 bpm (prep fills the MP3 copies' key).
fn analysis() -> PrepAnalyse {
    Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(json!({"camelot": "8A", "bpm": 120.0})))
}

const MP3: &str = "../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3";

/// A two-track set at 120 bpm in a session named "test set", from MP3s of about 2 s in `lib`,
/// each with a beat every 0.1 s from 0 (a bar every 0.4 s of the file):
/// - a.mp3 on bar 1;
/// - b.mp3 on bar 2.
///
/// A -> B enters on bar 3 (0.8 s into A, 0.4 s into B), hits on 4 and A's bass is gone on 5.
/// Returns the engine and B's clip.
fn set_b(lib: &Path) -> (Engine, u64) {
    std::fs::create_dir_all(lib).unwrap();
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR")).join(MP3);
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let mut clips = Vec::new();
    for (name, bar) in [("a.mp3", 1), ("b.mp3", 2)] {
        let path = lib.join(name);
        std::fs::copy(&mp3, &path).unwrap();
        let beats: Vec<f64> = (0..21).map(|k| k as f64 * 0.1).collect();
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        clips.push(r["clip"].as_u64().unwrap());
    }
    e.execute("dj.transition", &json!({"out_clip": clips[0], "in_clip": clips[1], "entry_bar": 3, "build_bars": 1, "out_bars": 1})).unwrap();
    (e, clips[1])
}

fn set(lib: &Path) -> Engine {
    set_b(lib).0
}

/// A runner that records each (plan, set name) it is given and answers `answer`.
fn fake(calls: &Arc<Mutex<Vec<(Value, String)>>>, answer: Result<Value, String>) -> MixxxRunner {
    let calls = Arc::clone(calls);
    Arc::new(move |plan: &Value, set: &str| {
        calls.lock().unwrap().push((plan.clone(), set.to_string()));
        answer.clone()
    })
}

fn export(e: &Engine, root: &Path) -> Value {
    export_job_in(e, &json!({"targets": ["rekordbox", "mixxx"]}), Some(root), analysis()).unwrap().wait().unwrap()
}

fn close(v: &Value, want: f64) -> bool {
    v.as_f64().is_some_and(|x| (x - want).abs() < 1e-6)
}

#[test]
fn mixxx_gets_mixforge_s_plan_of_the_prepped_copies() {
    let dir = scratch("plan");
    let mut e = set(&dir.join("lib"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let summary = json!({"backup": "C:/x/mixxxdb.sqlite.mixforge-backup-1", "playlist": "mixforge · test set", "tracks": [{}, {}]});
    e.set_mixxx_runner(fake(&calls, Ok(summary.clone())));
    let root = dir.join("prepped");
    let r = export(&e, &root);
    let out = root.join("test set");

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "{r}");
    let (plan, set) = &calls[0];
    assert_eq!(set, "test set");
    let tracks = plan["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2, "{plan}");
    // The same files the Rekordbox XML lists: the prepped copies, in set order.
    let paths: Vec<PathBuf> = tracks.iter().map(|t| PathBuf::from(t["path"].as_str().unwrap())).collect();
    let rows: Vec<PathBuf> = r["tracks"].as_array().unwrap().iter().map(|t| PathBuf::from(t["path"].as_str().unwrap())).collect();
    assert_eq!(paths, vec![out.join("a.mp3"), out.join("b.mp3")]);
    assert_eq!(paths, rows);
    for t in tracks {
        // A bar every 0.4 s: 600 quarter notes a minute; the first downbeat at the file start.
        assert!(close(&t["bpm"], 600.0) && close(&t["first_downbeat"], 0.0), "{t}");
        assert_eq!((&t["tempo_multiplier"], &t["sections"]), (&json!(1), &json!([])));
        // Prep filled 8A: mixforge's name for it.
        assert_eq!(t["key"], "Am");
        assert!(t["title"].is_string() && t["artist"].is_string(), "{t}");
    }
    // A: the set starts at its first downbeat; it mixes out on bar 3 (0.8 s), over 2 bars.
    assert!(close(&tracks[0]["cue_in"], 0.0) && close(&tracks[0]["out_start"], 0.8), "{}", tracks[0]);
    assert_eq!(tracks[0]["transition_bars"], 2);
    // B: mixed into on bar 3 (0.4 s); nothing after it.
    assert!(close(&tracks[1]["cue_in"], 0.4) && tracks[1]["out_start"].is_null(), "{}", tracks[1]);
    assert_eq!(tracks[1]["transition_bars"], 16);

    // What mixforge did is in the result and the report.
    assert_eq!(r["mixxx"]["playlist"], "mixforge · test set", "{r}");
    assert_eq!(r["mixxx"]["summary"], summary);
    let report = std::fs::read_to_string(out.join("test set.export.txt")).unwrap();
    assert!(report.contains("Mixxx: playlist \"mixforge · test set\""), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_running_mixxx_skips_mixxx_and_the_rest_still_completes() {
    let dir = scratch("running");
    let mut e = set(&dir.join("lib"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let running = "Close Mixxx first: it keeps the library open and would overwrite these changes.";
    e.set_mixxx_runner(fake(&calls, Err(running.into())));
    let root = dir.join("prepped");
    let r = export(&e, &root);
    assert_eq!(r["mixxx"], json!({"skipped": "Mixxx is running — close it and export again"}), "{r}");
    let out = root.join("test set");
    assert!(out.join("test set.rekordbox.xml").is_file(), "{r}");
    let report = std::fs::read_to_string(out.join("test set.export.txt")).unwrap();
    assert!(report.contains("Mixxx: Mixxx is running — close it and export again"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failing_runner_s_message_is_the_reason_mixxx_was_skipped() {
    let dir = scratch("failing");
    let mut e = set(&dir.join("lib"));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let why = "Mixxx library not found at C:/nowhere/mixxxdb.sqlite. Start Mixxx once, then close it.";
    e.set_mixxx_runner(fake(&calls, Err(why.into())));
    let root = dir.join("prepped");
    let r = export(&e, &root);
    assert_eq!(r["mixxx"], json!({"skipped": why}), "{r}");
    assert!(root.join("test set/test set.rekordbox.xml").is_file());

    // A runner that panics is a reason too.
    e.set_mixxx_runner(Arc::new(|_: &Value, _: &str| panic!("the runner broke")));
    let r = export(&e, &root);
    assert_eq!(r["mixxx"], json!({"skipped": "the runner broke"}), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_runner_mixxx_is_skipped() {
    let dir = scratch("no-runner");
    let e = set(&dir.join("lib"));
    let root = dir.join("prepped");
    let r = export(&e, &root);
    assert_eq!(r["mixxx"], json!({"skipped": "Mixxx export is not set up"}), "{r}");
    assert!(root.join("test set/test set.rekordbox.xml").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The two-track set plus c.wav on bar 4, B -> C on bar 5 (2 bars).
fn three(lib: &Path) -> Engine {
    let (mut e, b) = set_b(lib);
    let buf = AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.1; 30 * 44_100]] };
    let wav = lib.join("c.wav");
    std::fs::write(&wav, encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
    let beats: Vec<f64> = (0..60).map(|k| k as f64 * 0.5).collect();
    let c = e.execute("dj.load_warped", &json!({"path": wav, "beats": beats, "at_bar": 4})).unwrap()["clip"].as_u64().unwrap();
    e.execute("dj.transition", &json!({"out_clip": b, "in_clip": c, "entry_bar": 5, "build_bars": 1, "out_bars": 1})).unwrap();
    e
}

/// The analysis faked as 8A for every file but `keyless` (no key for those).
fn analysis_without(keyless: &'static [&'static str]) -> PrepAnalyse {
    Arc::new(move |path: &str, _: &dyn Fn() -> bool| {
        Ok(if keyless.iter().any(|k| path.ends_with(k)) { json!({"bpm": 120.0}) } else { json!({"camelot": "8A", "bpm": 120.0}) })
    })
}

/// The plan the runner got and the result, exporting `e` with `analyse`.
fn exported(e: &mut Engine, root: &Path, analyse: PrepAnalyse) -> (Value, Value) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    e.set_mixxx_runner(fake(&calls, Ok(json!({"playlist": "mixforge · test set"}))));
    let r = export_job_in(e, &json!({"targets": ["rekordbox", "mixxx"]}), Some(root), analyse).unwrap().wait().unwrap();
    let plan = calls.lock().unwrap().first().map(|c| c.0.clone()).unwrap_or(Value::Null);
    (plan, r)
}

fn names(plan: &Value) -> Vec<String> {
    plan["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| Path::new(t["path"].as_str().unwrap()).file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// A WAV copy has no key tag: its key comes from prep's analysis of the file.
#[test]
fn a_track_without_a_key_tag_takes_prep_s_analysis() {
    let dir = scratch("wav-key");
    let mut e = three(&dir.join("lib"));
    let (plan, _) = exported(&mut e, &dir.join("prepped"), analysis());
    assert_eq!(names(&plan), ["a.mp3", "b.mp3", "c.wav"]);
    assert_eq!(plan["tracks"][2]["key"], "Am");
    // B mixes out into C: its outro is that transition's 2 bars.
    assert_eq!(plan["tracks"][1]["transition_bars"], 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A track with no key at all is left out of Mixxx; the next track Mixxx gets says how that
/// shifts it: the first of the set, or an intro taken from the transition before.
#[test]
fn a_track_without_a_key_is_left_out_and_the_next_one_says_so() {
    let dir = scratch("no-key");
    let mut e = three(&dir.join("lib"));
    let (plan, r) = exported(&mut e, &dir.join("middle"), analysis_without(&["b.mp3"]));
    assert_eq!(names(&plan), ["a.mp3", "c.wav"]);
    assert!(r["tracks"][1]["doubts"].to_string().contains("left out of the Mixxx export: its key is not known"), "{r}");
    let c = r["tracks"][2]["doubts"].to_string();
    assert!(c.contains("its Mixxx intro may be off") && c.contains("b.mp3 was left out"), "{c}");

    let (plan, r) = exported(&mut e, &dir.join("first"), analysis_without(&["a.mp3"]));
    assert_eq!(names(&plan), ["b.mp3", "c.wav"]);
    let b = r["tracks"][1]["doubts"].to_string();
    assert!(b.contains("Mixxx takes it as the set's first track") && b.contains("a.mp3 was left out"), "{b}");
    assert!(!r["tracks"][2]["doubts"].to_string().contains("was left out"), "{r}");

    // Two left out in a row: the next track names both.
    let (plan, r) = exported(&mut e, &dir.join("both"), analysis_without(&["a.mp3", "b.mp3"]));
    assert_eq!(names(&plan), ["c.wav"]);
    let c = r["tracks"][2]["doubts"].to_string();
    assert!(c.contains("because a.mp3, b.mp3 were left out of the Mixxx export"), "{c}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// After one analysis times out, no other runs: neither prep's nor the export's own for keys,
/// so a stuck sidecar costs one timeout, not one per track.
#[test]
fn a_stuck_analysis_is_waited_on_once() {
    let dir = scratch("stuck");
    let mut e = three(&dir.join("lib"));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let analyse: PrepAnalyse = Arc::new(move |_: &str, _: &dyn Fn() -> bool| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err("timed out after 120 s".to_string())
    });
    let (_, r) = exported(&mut e, &dir.join("prepped"), analyse);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "{r}");
    // No track has a key, so none goes to Mixxx.
    assert_eq!(r["mixxx"]["skipped"], "no track could go to Mixxx; see the tracks' doubts", "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}
