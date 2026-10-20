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
