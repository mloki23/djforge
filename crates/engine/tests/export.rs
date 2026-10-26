//! `dj.export_set` preps a set's files and writes a Rekordbox XML of them, leaving the originals
//! alone.

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions, encode};
use soundcraft_engine::Engine;
use soundcraft_engine::cmd::{PrepAnalyse, export_job_in};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-export-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// mixforge's analysis, faked: every track is 8A at 120 bpm.
fn analysis() -> PrepAnalyse {
    Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(json!({"camelot": "8A", "bpm": 120.0})))
}

/// A two-track set at 120 bpm (a bar every 2 s) in a 48 kHz session named "test set", from
/// `lib`:
/// - a.wav: 30 s at 44.1 kHz (so the session plays a resampled copy), beats every 0.5 s from 0,
///   on bars 1..16;
/// - b.mp3: an MP3 of about 2 s, its beat list a beat every 0.1 s, on bar 8.
///
/// A -> B enters on bar 9, hits on 11 and A's bass is gone on 13.
fn set(lib: &Path) -> Engine {
    std::fs::create_dir_all(lib).unwrap();
    let a = lib.join("a.wav");
    let buf = AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.1; 30 * 44_100]] };
    std::fs::write(&a, encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
    let b = lib.join("b.mp3");
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    std::fs::copy(mp3, &b).unwrap();
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let load = |e: &mut Engine, path: &Path, beats: Vec<f64>, bar: i64| {
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        r["clip"].as_u64().unwrap()
    };
    let ca = load(&mut e, &a, (0..60).map(|k| k as f64 * 0.5).collect(), 1);
    let cb = load(&mut e, &b, (0..21).map(|k| k as f64 * 0.1).collect(), 8);
    e.execute("dj.transition", &json!({"out_clip": ca, "in_clip": cb, "entry_bar": 9, "build_bars": 2, "out_bars": 2})).unwrap();
    e
}

/// A TRACK of the XML's collection: its attributes, TEMPO count and POSITION_MARKs.
#[derive(Debug, Default)]
struct XmlTrack {
    attrs: Vec<(String, String)>,
    tempos: usize,
    marks: Vec<Vec<(String, String)>>,
}

impl XmlTrack {
    fn attr(&self, key: &str) -> &str {
        attr(&self.attrs, key)
    }
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> &'a str {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("no {key} in {attrs:?}"))
}

/// The collection's tracks, parsed strictly.
fn collection(xml: &str) -> Vec<XmlTrack> {
    let mut r = quick_xml::Reader::from_str(xml);
    r.config_mut().check_end_names = true;
    let mut out: Vec<XmlTrack> = Vec::new();
    let mut in_collection = false;
    loop {
        let ev = r.read_event().unwrap();
        let (e, empty) = match &ev {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(e) if e.name().as_ref() == b"COLLECTION" => {
                in_collection = false;
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let attrs: Vec<(String, String)> = e
            .attributes()
            .map(|a| {
                let a = a.unwrap();
                (String::from_utf8(a.key.as_ref().to_vec()).unwrap(), a.normalized_value(XmlVersion::Explicit1_0).unwrap().into_owned())
            })
            .collect();
        match e.name().as_ref() {
            b"COLLECTION" => in_collection = !empty,
            b"TRACK" if in_collection => out.push(XmlTrack { attrs, ..XmlTrack::default() }),
            b"TEMPO" => out.last_mut().unwrap().tempos += 1,
            b"POSITION_MARK" => out.last_mut().unwrap().marks.push(attrs),
            _ => {}
        }
    }
    out
}

/// The path a `file://localhost/` URL names.
fn url_path(url: &str) -> PathBuf {
    let rest = url.strip_prefix("file://localhost/").unwrap();
    let bytes = rest.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap(), 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let p = String::from_utf8(out).unwrap();
    PathBuf::from(if cfg!(windows) { p } else { format!("/{p}") })
}

fn hot_cue(t: &XmlTrack, num: &str) -> f64 {
    let m = t.marks.iter().find(|m| attr(m, "Num") == num).unwrap_or_else(|| panic!("no hot cue {num}: {:?}", t.marks));
    attr(m, "Start").parse().unwrap()
}

fn export(e: &Engine, root: &Path, analyse: PrepAnalyse) -> Value {
    export_job_in(e, &json!({"targets": ["rekordbox"]}), Some(root), analyse).unwrap().wait().unwrap()
}

#[test]
fn a_set_exports_prepped_copies_and_a_rekordbox_xml() {
    let dir = scratch("set");
    let lib = dir.join("lib");
    let e = set(&lib);
    let originals: Vec<Vec<u8>> = ["a.wav", "b.mp3"].iter().map(|f| std::fs::read(lib.join(f)).unwrap()).collect();
    let root = dir.join("prepped");
    let r = export(&e, &root, analysis());
    let out = root.join("test set");
    assert_eq!(Path::new(r["root"].as_str().unwrap()), out);
    let xml_path = out.join("test set.rekordbox.xml");
    assert_eq!(Path::new(r["rekordbox"].as_str().unwrap()), xml_path, "{r}");
    assert_eq!(r["mixxx"]["skipped"], "not requested");

    // Every Location is a prepped copy in the export folder, in set order.
    let xml = std::fs::read_to_string(&xml_path).unwrap();
    let tracks = collection(&xml);
    assert_eq!(tracks.len(), 2);
    let locations: Vec<PathBuf> = tracks.iter().map(|t| url_path(t.attr("Location"))).collect();
    assert_eq!(locations, vec![out.join("a.wav"), out.join("b.mp3")]);
    assert!(locations.iter().all(|l| l.is_file()));
    let rows = r["tracks"].as_array().unwrap();
    assert_eq!(rows.iter().map(|t| t["prep"].as_str().unwrap()).collect::<Vec<_>>(), ["not_adjusted", "ok"], "{r}");
    // The WAV is copied as it is; the MP3 copy got the key prep filled.
    assert_eq!(std::fs::read(out.join("a.wav")).unwrap(), originals[0]);
    assert_eq!(tracks[1].attr("Tonality"), "8A");
    assert_eq!(tracks[0].attr("Kind"), "WAV File");
    assert_eq!(tracks[0].attr("TotalTime"), "30");

    // A TEMPO per bar of each file: A's 15 bars of 2 s; B's beats every 0.1 s, a bar every 0.4 s.
    for (t, row) in tracks.iter().zip(rows) {
        assert_eq!(t.tempos as u64, row["grid_bars"].as_u64().unwrap());
    }
    assert_eq!(tracks[0].tempos, 15);
    assert_eq!(tracks[0].attr("AverageBpm"), "120.00");
    assert_eq!(tracks[1].attr("AverageBpm"), "600.00");
    // Cues: A mixes out on bar 9 (16 s in) and its bass is gone on bar 13 (24 s); B (its first
    // beat on bar 8) enters on bar 9 (beat 4: 0.4 s) and hits on bar 11 (beat 12: 1.2 s).
    assert_eq!((hot_cue(&tracks[0], "2"), hot_cue(&tracks[0], "3")), (16.0, 24.0));
    assert_eq!((hot_cue(&tracks[1], "0"), hot_cue(&tracks[1], "1")), (0.4, 1.2));
    assert_eq!(tracks[0].marks.len(), 3);
    assert_eq!(tracks[1].marks.len(), 3);

    // The originals are untouched, and the report says how to import the XML.
    for (f, before) in ["a.wav", "b.mp3"].iter().zip(&originals) {
        assert_eq!(&std::fs::read(lib.join(f)).unwrap(), before, "{f} changed");
    }
    let report = std::fs::read_to_string(out.join("test set.export.txt")).unwrap();
    assert!(report.starts_with("djforge set export: test set") && report.contains("rekordbox xml: choose this file"), "{report}");

    // Exported again into the same folder, the copies are prepped again and the XML is the same.
    let again = export(&e, &root, analysis());
    assert_eq!(again["tracks"].as_array().unwrap().len(), 2, "{again}");
    assert_eq!(std::fs::read_to_string(&xml_path).unwrap(), xml);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_saved_and_reopened_set_still_exports_the_original_files() {
    let dir = scratch("reopened");
    let lib = dir.join("lib");
    let mut e = set(&lib);
    let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
    soundcraft_engine::io::save_session(&mut e, &path).unwrap();
    let mut r = Engine::default();
    soundcraft_engine::io::open_session(&mut r, &path).unwrap();
    // a.wav plays from the session's resampled copy now, but the original is exported.
    assert!(r.session().sources.iter().any(|s| s.path.starts_with("Audio Files/")));
    let out = export(&r, &dir.join("prepped"), analysis());
    let rows = out["tracks"].as_array().unwrap();
    assert!(rows[0]["doubts"].as_array().unwrap().is_empty(), "{out}");
    assert_eq!(std::fs::read(Path::new(rows[0]["path"].as_str().unwrap())).unwrap(), std::fs::read(lib.join("a.wav")).unwrap());

    // A session from before sources remembered their files exports its copy, and says so.
    for s in &mut r.session_mut().sources {
        s.origin = None;
    }
    let out = export(&r, &dir.join("old"), analysis());
    let doubts = out["tracks"][0]["doubts"].to_string();
    assert!(doubts.contains("exported the session's copy (resampled)"), "{doubts}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_cancelled_export_writes_no_xml() {
    let dir = scratch("cancel");
    let e = set(&dir.join("lib"));
    // The analysis runs until it is stopped, so the cancel lands while prep is running.
    let started = Arc::new(std::sync::Barrier::new(2));
    let seen = Arc::clone(&started);
    let analyse: PrepAnalyse = Arc::new(move |_: &str, stop: &dyn Fn() -> bool| {
        seen.wait();
        while !stop() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err("stopped".into())
    });
    let root = dir.join("prepped");
    let job = export_job_in(&e, &json!({}), Some(&root), analyse).unwrap();
    started.wait();
    assert_eq!(job.progress().stage, "prep");
    job.cancel();
    let r = job.wait().unwrap();
    assert_eq!(r["cancelled"], true, "{r}");
    assert!(!root.join("test set/test set.rekordbox.xml").exists());
    assert!(!root.join("test set/test set.export.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bad_requests_and_foreign_files_are_refused() {
    let dir = scratch("refused");
    let e = set(&dir.join("lib"));
    let root = dir.join("prepped");
    for p in [json!({"targets": ["traktor"]}), json!({"targets": "rekordbox"}), json!({"stems": "yes"}), json!({"out": 3})] {
        assert!(export_job_in(&e, &p, Some(&root), analysis()).is_err(), "{p}");
    }
    assert!(export_job_in(&Engine::default(), &json!({}), Some(&root), analysis()).is_err());
    // A file of the XML's name that no export wrote is never replaced.
    let out = root.join("test set");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("test set.rekordbox.xml"), "mine").unwrap();
    let err = export_job_in(&e, &json!({}), Some(&root), analysis()).err().unwrap().to_string();
    assert!(err.contains("is not an earlier export"), "{err}");
    assert_eq!(std::fs::read_to_string(out.join("test set.rekordbox.xml")).unwrap(), "mine");
    let _ = std::fs::remove_dir_all(&dir);
}
