//! `prep.tracks` writes level-matched, tagged copies of MP3s and leaves the originals alone.

use serde_json::{Value, json};
use soundcraft_audio_io::{decode, mp3gain::STEP_DB, mp3tags};
use soundcraft_engine::cmd::prep_run;
use soundcraft_engine::model::Session;
use std::path::{Path, PathBuf};

const FIXTURES: [&str; 3] = ["cbr128_44k_stereo.mp3", "vbr_48k_stereo.mp3", "cbr64_44k_mono.mp3"];

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3").join(name)).unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-prep-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A folder holding the three fixtures and a text file.
fn source_folder(dir: &Path) -> PathBuf {
    let src = dir.join("set");
    std::fs::create_dir_all(&src).unwrap();
    for f in FIXTURES {
        std::fs::write(src.join(f), fixture(f)).unwrap();
    }
    std::fs::write(src.join("notes.txt"), "not audio").unwrap();
    src
}

fn no_analysis(_: &str) -> Result<Value, String> {
    Err("DJFORGE_PYTHON is not set".into())
}

fn run(source: &Path, out: &Path, analyse: &dyn Fn(&str) -> Result<Value, String>) -> soundcraft_engine::Result<Value> {
    prep_run(&Session::default(), source.to_str().unwrap(), Some(out.to_str().unwrap()), -14.0, -1.0, analyse)
}

/// Every file in `dir` with its bytes and modification time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            let m = std::fs::metadata(&p).unwrap().modified().unwrap();
            (p.clone(), std::fs::read(&p).unwrap(), m)
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn tracks(r: &Value) -> &Vec<Value> {
    r["tracks"].as_array().unwrap()
}

fn track<'a>(r: &'a Value, file: &str) -> &'a Value {
    tracks(r).iter().find(|t| t["file"] == file).unwrap_or_else(|| panic!("{file} not in {r}"))
}

#[test]
fn a_folder_preps_every_mp3_to_the_target_without_touching_the_originals() {
    let dir = scratch("folder");
    let src = source_folder(&dir);
    let out = dir.join("out");
    let before = snapshot(&src);
    let r = run(&src, &out, &no_analysis).unwrap();
    assert_eq!(snapshot(&src), before, "the originals changed");

    let mut written: Vec<String> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    written.sort();
    let mut want: Vec<String> = FIXTURES.iter().map(|s| s.to_string()).chain(["prep-report.txt".to_string()]).collect();
    want.sort();
    assert_eq!(written, want);
    assert_eq!(tracks(&r).len(), 3);

    let report = std::fs::read_to_string(out.join("prep-report.txt")).unwrap();
    for t in tracks(&r) {
        let file = t["file"].as_str().unwrap();
        assert_eq!(t["status"], "ok", "{t}");
        let (lufs, steps) = (t["lufs_before"].as_f64().unwrap(), t["steps"].as_i64().unwrap());
        let expected = lufs + steps as f64 * STEP_DB;
        let after = t["lufs_after"].as_f64().unwrap();
        assert!((after - expected).abs() <= 0.76, "{t}");
        assert!(t["tp_after"].as_f64().unwrap() <= -1.0 + 0.05, "{t}");
        assert!(t["doubts"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().starts_with("no mixforge analysis")), "{t}");
        assert!(report.contains(file), "{report}");
        // The copy decodes to the same length, at the gain reported.
        let (_, x) = decode(&fixture(file), Some("mp3")).unwrap();
        let (_, y) = decode(&std::fs::read(out.join(file)).unwrap(), Some("mp3")).unwrap();
        assert_eq!((x.num_channels(), x.frames()), (y.num_channels(), y.frames()));
    }
    assert_eq!(r["totals"]["ok"], 3);
    assert_eq!(r["totals"]["over_ceiling"], 0);
    // At least one track really moved.
    assert!(tracks(&r).iter().any(|t| t["steps"].as_i64() != Some(0)), "{r}");
}

#[test]
fn out_inside_the_source_folder_is_refused_before_anything_is_written() {
    let dir = scratch("refuse-out");
    let src = source_folder(&dir);
    let before = snapshot(&src);
    let err = run(&src, &src, &no_analysis).unwrap_err().to_string();
    assert!(err.contains("source"), "{err}");
    assert_eq!(snapshot(&src), before);
}

/// A plan.json from the small fixture plan whose tracks are `paths`.
fn plan_at(dir: &Path, paths: &[PathBuf]) -> PathBuf {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixforge-plan-small.json")).unwrap();
    let mut plan: Value = serde_json::from_str(&text).unwrap();
    let list = plan["tracks"].as_array_mut().unwrap();
    list.truncate(paths.len());
    for (t, p) in list.iter_mut().zip(paths) {
        t["path"] = json!(p.to_str().unwrap());
    }
    if let Some(last) = list.last_mut() {
        last.as_object_mut().unwrap().remove("out_start");
    }
    let set = dir.join("my set");
    std::fs::create_dir_all(&set).unwrap();
    let path = set.join("plan.json");
    std::fs::write(&path, serde_json::to_string(&plan).unwrap()).unwrap();
    path
}

#[test]
fn a_plan_preps_its_tracks() {
    let dir = scratch("plan");
    let src = source_folder(&dir);
    let plan = plan_at(&dir, &[src.join(FIXTURES[0]), src.join(FIXTURES[2])]);
    let out = dir.join("out");
    let r = run(&plan, &out, &no_analysis).unwrap();
    assert_eq!(tracks(&r).len(), 2);
    assert!(out.join(FIXTURES[0]).is_file() && out.join(FIXTURES[2]).is_file() && !out.join(FIXTURES[1]).exists());
    assert!(out.join("prep-report.txt").is_file());
}

#[test]
fn two_tracks_with_one_name_are_refused() {
    let dir = scratch("clash");
    let src = source_folder(&dir);
    let other = dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(FIXTURES[0].to_uppercase()), fixture(FIXTURES[0])).unwrap();
    let plan = plan_at(&dir, &[src.join(FIXTURES[0]), other.join(FIXTURES[0].to_uppercase())]);
    let out = dir.join("out");
    let err = run(&plan, &out, &no_analysis).unwrap_err().to_string();
    assert!(err.contains("name"), "{err}");
    assert!(!out.exists());
}

#[test]
fn a_truncated_file_preps_and_a_corrupt_one_is_copied_unadjusted() {
    let dir = scratch("damaged");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let whole = fixture(FIXTURES[0]);
    // Cut part-way through the last frame.
    std::fs::write(src.join("cut.mp3"), &whole[..whole.len() - 150]).unwrap();
    // Junk in the middle of a frame half-way through.
    let mut junk = whole.clone();
    let mid = junk.len() / 2;
    junk.splice(mid..mid, std::iter::repeat_n(0x55u8, 300));
    std::fs::write(src.join("junk.mp3"), &junk).unwrap();
    let out = dir.join("out");
    let r = run(&src, &out, &no_analysis).unwrap();

    let cut = track(&r, "cut.mp3");
    assert_eq!(cut["status"], "ok", "{cut}");
    assert_ne!(cut["steps"], 0, "{cut}");

    let bad = track(&r, "junk.mp3");
    assert_eq!(bad["status"], "not_adjusted", "{bad}");
    assert_eq!(bad["steps"], 0);
    assert!(bad["doubts"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().contains("frame sync lost")), "{bad}");
    // Only the tag differs: the audio after it is the original's.
    let copy = std::fs::read(out.join("junk.mp3")).unwrap();
    let audio = |b: &[u8]| b[10 + soundcraft_size(b)..].to_vec();
    assert_eq!(audio(&copy), audio(&junk));
}

/// The ID3v2 tag body size of `b` (syncsafe).
fn soundcraft_size(b: &[u8]) -> usize {
    assert_eq!(&b[..3], b"ID3");
    b[6..10].iter().fold(0usize, |n, &x| (n << 7) | usize::from(x & 0x7F))
}

#[test]
fn mixforge_analysis_fills_missing_key_and_bpm() {
    let dir = scratch("tags");
    let src = source_folder(&dir);
    let out = dir.join("out");
    let analyse = |_: &str| Ok(json!({"camelot": "4B", "bpm": 120.0}));
    let r = run(&src, &out, &analyse).unwrap();
    for t in tracks(&r) {
        let file = t["file"].as_str().unwrap();
        assert_eq!(t["filled"], json!(["key", "bpm"]), "{t}");
        // Reading the copy's tag back: the frames are there now.
        let (_, rep) = mp3tags::fill(&std::fs::read(out.join(file)).unwrap(), &mp3tags::TagFill { camelot: None, bpm: None, prep_note: "" }).unwrap();
        assert_eq!((rep.existing_key.as_deref(), rep.existing_bpm.as_deref()), (Some("4B"), Some("120")), "{file}");
        let doubts: Vec<&str> = t["doubts"].as_array().unwrap().iter().map(|d| d.as_str().unwrap()).collect();
        assert!(doubts.contains(&"no energy tag") && doubts.contains(&"no album"), "{doubts:?}");
    }
    // A second prep of the copies sees the tags and reports a disagreeing analysis.
    let out2 = dir.join("out2");
    let analyse = |_: &str| Ok(json!({"camelot": "9A", "bpm": 60.2}));
    let r = run(&out, &out2, &analyse).unwrap();
    for t in tracks(&r) {
        assert_eq!(t["filled"], json!([]), "{t}");
        let doubts: Vec<&str> = t["doubts"].as_array().unwrap().iter().map(|d| d.as_str().unwrap()).collect();
        assert!(doubts.contains(&"key: file 4B, mixforge 9A (file kept)"), "{doubts:?}");
        assert!(doubts.iter().any(|d| d.starts_with("bpm: file 120, mixforge 60.2") && d.contains("half/double")), "{doubts:?}");
    }
}

#[test]
fn the_ceiling_holds_a_boost_short_of_the_target_and_says_so() {
    let dir = scratch("ceiling");
    let src = source_folder(&dir);
    let out = dir.join("out");
    let r = prep_run(&Session::default(), src.to_str().unwrap(), Some(out.to_str().unwrap()), -14.0, -18.0, &no_analysis).unwrap();
    for t in tracks(&r) {
        assert_eq!(t["status"], "ok", "{t}");
        assert_eq!(t["limited_by"], "peaks", "{t}");
        assert!(t["tp_after"].as_f64().unwrap() <= -18.0 + 0.05, "{t}");
        assert!(t["lufs_after"].as_f64().unwrap() < -14.0 - 0.75, "{t}");
        assert!(t["doubts"][0].as_str().unwrap().starts_with("peaks: reaches"), "{t}");
    }
    assert_eq!(r["totals"]["over_ceiling"], 0);
}

#[test]
fn a_malformed_tag_is_kept_as_it_is_on_the_adjusted_copy() {
    let dir = scratch("bad-tag");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut bytes = fixture(FIXTURES[0]);
    // The first frame (TSSE) claims more bytes than the tag holds.
    assert_eq!(&bytes[10..14], b"TSSE");
    bytes[14..18].copy_from_slice(&0x0FFF_FFFFu32.to_be_bytes());
    std::fs::write(src.join("bad.mp3"), &bytes).unwrap();
    let out = dir.join("out");
    let r = run(&src, &out, &|_: &str| Ok(json!({"camelot": "4B", "bpm": 120.0}))).unwrap();
    let t = track(&r, "bad.mp3");
    assert_eq!(t["status"], "ok", "{t}");
    assert_ne!(t["steps"], 0, "{t}");
    assert_eq!(t["filled"], json!([]), "{t}");
    assert!(t["doubts"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().starts_with("tags left as they are: malformed")), "{t}");
    let copy = std::fs::read(out.join("bad.mp3")).unwrap();
    let tag = 10 + soundcraft_size(&bytes);
    assert_eq!(copy[..tag], bytes[..tag], "the tag changed");
    assert_ne!(copy, bytes, "the gain did not change");
}

fn hot_master() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hot_master.mp3")).unwrap()
}

/// A clipped master (pink noise, ffmpeg: -4.9 LUFS, +5.1 dBTP; it decodes past 0 dBFS).
fn hot_folder(dir: &Path) -> PathBuf {
    let src = dir.join("hot");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("master.mp3"), hot_master()).unwrap();
    src
}

#[test]
fn a_clipped_master_is_cut_to_the_target_and_measured_honestly() {
    let dir = scratch("hot");
    let src = hot_folder(&dir);
    let out = dir.join("out");
    let r = run(&src, &out, &no_analysis).unwrap();
    let t = track(&r, "master.mp3");
    assert_eq!(t["status"], "ok", "{t}");
    let steps = t["steps"].as_i64().unwrap();
    assert_eq!(steps, -6, "{t}");
    // ffmpeg's ebur128 of the original: -4.9 LUFS, +5.1 dBTP.
    assert!((t["lufs_before"].as_f64().unwrap() + 4.9).abs() < 0.3, "{t}");
    assert!((t["tp_before"].as_f64().unwrap() - 5.1).abs() < 0.3, "{t}");
    assert!((t["lufs_after"].as_f64().unwrap() + 14.0).abs() <= 0.76, "{t}");
    assert!(t["tp_after"].as_f64().unwrap() <= -1.0 + 0.05, "{t}");
    assert!(t["doubts"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().starts_with("original clips at 0 dBFS")), "{t}");
    assert_eq!(r["totals"]["over_ceiling"], 0);
}

#[test]
fn a_clipped_master_is_cut_below_a_hot_target_until_its_peaks_clear_the_ceiling() {
    let dir = scratch("hot-target");
    let src = hot_folder(&dir);
    let out = dir.join("out");
    let r = prep_run(&Session::default(), src.to_str().unwrap(), Some(out.to_str().unwrap()), -6.0, -1.0, &no_analysis).unwrap();
    let t = track(&r, "master.mp3");
    assert_eq!(t["status"], "ok", "{t}");
    assert_eq!(t["limited_by"], "peaks", "{t}");
    assert!(t["steps"].as_i64().unwrap() <= -5, "{t}");
    assert!(t["tp_after"].as_f64().unwrap() <= -1.0 + 0.05, "{t}");
}

/// Re-runs into an `out` that holds files: only earlier prep copies (and an earlier report) may
/// be replaced.
#[test]
fn files_in_out_that_prep_did_not_write_are_never_replaced() {
    let dir = scratch("guard");
    let src = source_folder(&dir);
    let out = dir.join("out");
    run(&src, &out, &no_analysis).unwrap();
    // A second run over its own copies and report is fine.
    run(&src, &out, &no_analysis).unwrap();

    // An original-looking file (no prep note) where a copy would go.
    let foreign = out.join(FIXTURES[1]);
    std::fs::write(&foreign, fixture(FIXTURES[1])).unwrap();
    let before = snapshot(&out);
    let err = run(&src, &out, &no_analysis).unwrap_err().to_string();
    assert!(err.contains(FIXTURES[1]) && err.contains("not a prep copy"), "{err}");
    assert_eq!(snapshot(&out), before, "something was written");

    // A hard link to the original where a copy would go.
    std::fs::remove_file(&foreign).unwrap();
    std::fs::hard_link(src.join(FIXTURES[1]), &foreign).unwrap();
    let err = run(&src, &out, &no_analysis).unwrap_err().to_string();
    assert!(err.contains("not a prep copy"), "{err}");
    assert_eq!(std::fs::read(src.join(FIXTURES[1])).unwrap(), fixture(FIXTURES[1]));
    std::fs::remove_file(&foreign).unwrap();

    // A report that isn't prep's.
    std::fs::write(out.join("prep-report.txt"), "my notes").unwrap();
    let err = run(&src, &out, &no_analysis).unwrap_err().to_string();
    assert!(err.contains("prep-report.txt"), "{err}");
    assert_eq!(std::fs::read_to_string(out.join("prep-report.txt")).unwrap(), "my notes");
}

#[test]
fn a_copy_replaces_a_link_rather_than_writing_through_it() {
    let dir = scratch("link");
    let src = source_folder(&dir);
    let out = dir.join("out");
    run(&src, &out, &no_analysis).unwrap();
    // An earlier prep copy hard-linked elsewhere: the next run must not change the other name.
    let elsewhere = dir.join("kept.mp3");
    std::fs::hard_link(out.join(FIXTURES[0]), &elsewhere).unwrap();
    let kept = std::fs::read(&elsewhere).unwrap();
    run(&src, &out, &|_: &str| Ok(json!({"camelot": "4B"}))).unwrap();
    assert_eq!(std::fs::read(&elsewhere).unwrap(), kept);
    assert_ne!(std::fs::read(out.join(FIXTURES[0])).unwrap(), kept);
}

#[test]
fn out_spelled_with_dots_is_still_the_source_folder() {
    let dir = scratch("dots");
    let src = source_folder(&dir);
    let sneaky = src.join("missing").join("..");
    let err = run(&src, &sneaky, &no_analysis).unwrap_err().to_string();
    assert!(err.contains("source"), "{err}");
    assert!(!src.join("missing").exists());
}

#[test]
fn a_plan_listing_a_file_twice_is_refused() {
    let dir = scratch("twice");
    let src = source_folder(&dir);
    let plan = plan_at(&dir, &[src.join(FIXTURES[0]), src.join(".").join(FIXTURES[0])]);
    let err = run(&plan, &dir.join("out"), &no_analysis).unwrap_err().to_string();
    assert!(err.contains("listed twice"), "{err}");
}

#[test]
fn non_mpeg_bytes_are_copied_without_new_tags() {
    let dir = scratch("not-mpeg");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let junk = vec![0x42u8; 5000];
    std::fs::write(src.join("fake.mp3"), &junk).unwrap();
    let out = dir.join("out");
    let r = run(&src, &out, &|_: &str| Ok(json!({"camelot": "4B", "bpm": 120.0}))).unwrap();
    let t = track(&r, "fake.mp3");
    assert_eq!(t["status"], "not_adjusted", "{t}");
    assert!(t["doubts"].as_array().unwrap().iter().any(|d| d == "not MPEG audio"), "{t}");
    assert_eq!(std::fs::read(out.join("fake.mp3")).unwrap(), junk);
    assert!(r["report"].as_str().unwrap().ends_with(&format!("out{}prep-report.txt", std::path::MAIN_SEPARATOR)), "{r}");
}

/// `audio` (a fixture, its own tag dropped) under an ID3v2.3 tag of Latin-1 text frames.
fn with_tag(audio: &[u8], frames: &[(&str, &str)]) -> Vec<u8> {
    let audio = &audio[10 + soundcraft_size(audio)..];
    let mut body = Vec::new();
    for (id, text) in frames {
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(&(text.len() as u32 + 1).to_be_bytes());
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(text.as_bytes());
    }
    let n = body.len();
    let mut out = b"ID3\x03\x00\x00".to_vec();
    out.extend([(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
    out.extend(body);
    out.extend_from_slice(audio);
    out
}

#[test]
fn file_keys_in_musical_notation_are_compared_as_camelot() {
    let dir = scratch("keys");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let audio = fixture(FIXTURES[0]);
    for (name, key, bpm) in [("a.mp3", "Am", "120"), ("b.mp3", "04B", "120"), ("c.mp3", "F#m", "fast")] {
        std::fs::write(src.join(name), with_tag(&audio, &[("TKEY", key), ("TBPM", bpm)])).unwrap();
    }
    let out = dir.join("out");
    let r = run(&src, &out, &|_: &str| Ok(json!({"camelot": "8A", "bpm": 120.0}))).unwrap();
    let doubts = |f: &str| -> Vec<String> { track(&r, f)["doubts"].as_array().unwrap().iter().map(|d| d.as_str().unwrap().to_string()).collect() };
    assert!(!doubts("a.mp3").iter().any(|d| d.starts_with("key:")), "{:?}", doubts("a.mp3"));
    assert!(doubts("b.mp3").contains(&"key: file 04B (4B), mixforge 8A (file kept)".to_string()), "{:?}", doubts("b.mp3"));
    assert!(doubts("c.mp3").contains(&"key: file F#m (11A), mixforge 8A (file kept)".to_string()), "{:?}", doubts("c.mp3"));
    assert!(doubts("c.mp3").iter().any(|d| d.starts_with("bpm: file `fast` is not a number")), "{:?}", doubts("c.mp3"));
}

/// The bad-tag fixture: its TSSE frame claims more bytes than the tag holds.
fn bad_tag() -> Vec<u8> {
    let mut bytes = fixture(FIXTURES[0]);
    bytes[14..18].copy_from_slice(&0x0FFF_FFFFu32.to_be_bytes());
    bytes
}

#[test]
fn a_re_run_accepts_its_own_copies_even_without_a_prep_note() {
    let dir = scratch("rerun-untagged");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("bad.mp3"), bad_tag()).unwrap();
    std::fs::write(src.join("fake.mp3"), vec![0x42u8; 5000]).unwrap();
    let out = dir.join("out");
    let first = run(&src, &out, &no_analysis).unwrap();
    assert!(track(&first, "bad.mp3")["doubts"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().starts_with("tags left as they are")));
    let again = run(&src, &out, &no_analysis).unwrap();
    assert_eq!(track(&again, "bad.mp3")["status"], "ok", "{again}");
    assert_eq!(track(&again, "fake.mp3")["status"], "not_adjusted", "{again}");
}

#[test]
fn a_planted_temp_file_is_not_written_through() {
    let dir = scratch("planted");
    let src = source_folder(&dir);
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, "keep me").unwrap();
    std::fs::hard_link(&victim, out.join(format!(".{}.djforge-tmp", FIXTURES[0]))).unwrap();
    let r = run(&src, &out, &no_analysis).unwrap();
    assert_eq!(track(&r, FIXTURES[0])["status"], "ok", "{r}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    assert!(!out.join(format!(".{}.djforge-tmp", FIXTURES[0])).exists());
}

#[test]
fn a_destination_that_cannot_be_read_is_refused() {
    let dir = scratch("unreadable");
    let src = source_folder(&dir);
    let out = dir.join("out");
    std::fs::create_dir_all(out.join(FIXTURES[0])).unwrap();
    let err = run(&src, &out, &no_analysis).unwrap_err().to_string();
    assert!(err.contains(FIXTURES[0]), "{err}");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1, "something was written");
}

#[test]
fn after_one_analysis_times_out_the_rest_are_skipped() {
    let dir = scratch("timeout");
    let src = source_folder(&dir);
    let calls = std::cell::Cell::new(0);
    let slow = |_: &str| {
        calls.set(calls.get() + 1);
        Err("timed out after 120 s".to_string())
    };
    let r = run(&src, &dir.join("out"), &slow).unwrap();
    assert_eq!(calls.get(), 1);
    let skipped = tracks(&r)
        .iter()
        .filter(|t| t["doubts"].as_array().unwrap().iter().any(|d| d == "no mixforge analysis: skipped after an earlier timeout"))
        .count();
    assert_eq!(skipped, 2, "{r}");
}

#[cfg(windows)]
#[test]
fn an_out_that_is_a_junction_to_the_source_folder_is_refused() {
    let dir = scratch("junction");
    let src = source_folder(&dir);
    let out = dir.join("out");
    let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&out).arg(&src).output().unwrap();
    assert!(made.status.success(), "{made:?}");
    let before = snapshot(&src);
    let err = run(&src, &out, &no_analysis).unwrap_err().to_string();
    assert!(err.contains("source"), "{err}");
    assert_eq!(snapshot(&src), before);
}

/// `audio` (a fixture, its own tag dropped) under an ID3v2.3 tag with header `flags` holding the
/// raw frame bodies `frames`.
fn with_raw_tag(audio: &[u8], flags: u8, frames: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let audio = &audio[10 + soundcraft_size(audio)..];
    let body: Vec<u8> = frames.iter().flat_map(|(id, b)| [&id[..], &(b.len() as u32).to_be_bytes(), &[0, 0], b].concat()).collect();
    let n = body.len();
    let size = [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F];
    [&b"ID3"[..], &[3, 0, flags], &size, &body, audio].concat()
}

#[test]
fn an_earlier_copy_is_recognised_by_its_note_even_when_id3_cannot_read_its_tag() {
    let dir = scratch("odd-copy");
    let src = source_folder(&dir);
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    // An earlier copy whose tag also holds a TXXX in a text encoding that doesn't exist.
    let note = [&[0u8][..], &b"djforge_prep"[..], &[0], &b"gain +0.0 dB"[..]].concat();
    let copy = with_raw_tag(&fixture(FIXTURES[0]), 0, &[(b"TXXX", vec![7, b'x', 0, b'y']), (b"TXXX", note)]);
    std::fs::write(out.join(FIXTURES[0]), &copy).unwrap();
    let r = run(&src, &out, &no_analysis).unwrap();
    assert_eq!(track(&r, FIXTURES[0])["status"], "ok", "{r}");
}

#[test]
fn a_v23_tag_with_a_stray_footer_flag_preps() {
    let dir = scratch("stray-footer");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let tss = [&[0u8][..], &b"Lavf"[..]].concat();
    std::fs::write(src.join("flag.mp3"), with_raw_tag(&fixture(FIXTURES[0]), 0x10, &[(b"TSSE", tss)])).unwrap();
    let r = run(&src, &dir.join("out"), &no_analysis).unwrap();
    let t = track(&r, "flag.mp3");
    assert_eq!(t["status"], "ok", "{t}");
    assert_ne!(t["steps"], 0, "{t}");
}

#[test]
fn a_track_that_fails_on_a_re_run_says_its_earlier_copy_was_kept() {
    let dir = scratch("kept-copy");
    let src = source_folder(&dir);
    let plan = plan_at(&dir, &[src.join(FIXTURES[0]), src.join(FIXTURES[2])]);
    let out = dir.join("out");
    run(&plan, &out, &no_analysis).unwrap();
    let earlier = std::fs::read(out.join(FIXTURES[0])).unwrap();
    std::fs::remove_file(src.join(FIXTURES[0])).unwrap();
    let r = run(&plan, &out, &no_analysis).unwrap();
    let t = track(&r, FIXTURES[0]);
    assert_eq!(t["status"], "failed", "{t}");
    assert!(t["doubts"].as_array().unwrap().iter().any(|d| d == "the earlier copy in the output folder was left as it was"), "{t}");
    assert_eq!(std::fs::read(out.join(FIXTURES[0])).unwrap(), earlier);
    assert!(!track(&r, FIXTURES[2])["doubts"].as_array().unwrap().iter().any(|d| d.as_str().unwrap().contains("earlier copy")));
}
