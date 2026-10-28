//! `dj.export_set` preps a set's files and writes a Rekordbox XML of them, leaving the originals
//! alone.

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions, encode};
use soundcraft_engine::Engine;
use soundcraft_engine::cmd::{PrepAnalyse, export_job_in};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// Each TEMPO's Inizio (seconds).
    inizio: Vec<f64>,
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
            b"TEMPO" => {
                let t = out.last_mut().unwrap();
                t.tempos += 1;
                t.inizio.push(attr(&attrs, "Inizio").parse().unwrap());
            }
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
    assert!(doubts.contains("exported the session's copy (the set plays audio made or edited in the session"), "{doubts}");
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
    for p in [json!({"targets": ["traktor"]}), json!({"targets": "rekordbox"}), json!({"targets": []}), json!({"stems": "yes"}), json!({"out": 3})] {
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

/// An original edited after it was imported is not the audio the set's grid was made on: the
/// session's own copy of it is exported, with a doubt.
#[test]
fn an_original_changed_since_import_exports_the_session_s_audio() {
    let dir = scratch("changed");
    let lib = dir.join("lib");
    let mut e = set(&lib);
    let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
    soundcraft_engine::io::save_session(&mut e, &path).unwrap();
    // Unchanged, the original is exported.
    let r = export(&e, &dir.join("before"), analysis());
    assert_eq!(Path::new(r["tracks"][0]["path"].as_str().unwrap()), dir.join("before/Set/a.wav"));
    assert!(r["tracks"][0]["doubts"].as_array().unwrap().is_empty(), "{r}");
    let r = export(&e, &dir.join("prepped"), analysis());
    let copy = PathBuf::from(r["tracks"][0]["path"].as_str().unwrap());
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(lib.join("a.wav")).unwrap());

    // Edited (a second longer): the session's copy in Set/Audio Files is what is prepped.
    let buf = AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.2; 31 * 44_100]] };
    std::fs::write(lib.join("a.wav"), encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
    let r = export(&e, &dir.join("after"), analysis());
    let doubts = r["tracks"][0]["doubts"].to_string();
    assert!(doubts.contains("the original changed since import; exported the session's audio"), "{doubts}");
    let copy = PathBuf::from(r["tracks"][0]["path"].as_str().unwrap());
    let session_copy = e.session().sources.iter().find(|s| s.path.starts_with("Audio Files/")).unwrap();
    assert_eq!(std::fs::read(&copy).unwrap(), std::fs::read(dir.join("Set").join(&session_copy.path)).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file prep refuses on its own doesn't stop the export: two tracks named alike in different
/// folders, or a foreign file of a track's name in the export folder. The first of the alike is
/// prepped; the refused ones are exported as their originals, never written, with the reason.
#[test]
fn a_file_prep_refuses_is_exported_as_its_original() {
    let dir = scratch("refused-file");
    let lib = dir.join("lib");
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let mut clips = Vec::new();
    for (folder, bar) in [("x", 1), ("y", 2)] {
        let path = lib.join(folder).join("Intro.mp3");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(&mp3, &path).unwrap();
        let beats: Vec<f64> = (0..21).map(|k| k as f64 * 0.1).collect();
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        clips.push(r["clip"].as_u64().unwrap());
    }
    e.execute("dj.transition", &json!({"out_clip": clips[0], "in_clip": clips[1], "entry_bar": 3, "build_bars": 1, "out_bars": 1})).unwrap();
    let root = dir.join("prepped");
    let r = export(&e, &root, analysis());
    let out = root.join("test set");
    let rows = r["tracks"].as_array().unwrap();
    assert_eq!(rows.iter().map(|t| t["prep"].as_str().unwrap()).collect::<Vec<_>>(), ["ok", "not prepped"], "{r}");
    assert_eq!(Path::new(rows[1]["path"].as_str().unwrap()), lib.join("y/Intro.mp3"));
    let doubts = rows[1]["doubts"].to_string();
    assert!(doubts.contains("prep refused it: another track of the set has the name `Intro.mp3`"), "{doubts}");
    // The result says so plainly too, for a status line.
    assert!(rows[1]["prep_refused"].as_str().unwrap().contains("another track of the set has the name `Intro.mp3`"), "{r}");
    assert!(rows[0]["prep_refused"].is_null() && rows[0]["found"] == true && rows[1]["found"] == true, "{r}");
    let xml = std::fs::read_to_string(out.join("test set.rekordbox.xml")).unwrap();
    let locations: Vec<PathBuf> = collection(&xml).iter().map(|t| url_path(t.attr("Location"))).collect();
    assert_eq!(locations, vec![out.join("Intro.mp3"), lib.join("y/Intro.mp3")]);
    assert_eq!(std::fs::read(lib.join("y/Intro.mp3")).unwrap(), std::fs::read(&mp3).unwrap());

    // A file of b's name that prep didn't write: b is exported as its original; a still preps.
    let dir2 = scratch("foreign-copy");
    let e = set(&dir2.join("lib"));
    let out = dir2.join("prepped/test set");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("b.mp3"), "mine").unwrap();
    let r = export(&e, &dir2.join("prepped"), analysis());
    let rows = r["tracks"].as_array().unwrap();
    assert_eq!(rows[0]["prep"], "not_adjusted", "{r}");
    assert_eq!(Path::new(rows[1]["path"].as_str().unwrap()), dir2.join("lib/b.mp3"));
    assert!(rows[1]["doubts"].to_string().contains("exists and is not a prep copy"), "{r}");
    assert_eq!(std::fs::read_to_string(out.join("b.mp3")).unwrap(), "mine");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// A file that takes the XML's or the report's name while the export runs is not replaced: the
/// result says why, and the rest of the result stands.
#[test]
fn files_that_appear_during_the_export_are_not_replaced() {
    let dir = scratch("appeared");
    let e = set(&dir.join("lib"));
    let started = Arc::new(std::sync::Barrier::new(2));
    let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (seen, going) = (Arc::clone(&started), Arc::clone(&go));
    let analyse: PrepAnalyse = Arc::new(move |_: &str, _: &dyn Fn() -> bool| {
        if !going.load(std::sync::atomic::Ordering::SeqCst) {
            seen.wait();
            while !going.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        Ok(json!({"camelot": "8A", "bpm": 120.0}))
    });
    let root = dir.join("prepped");
    let job = export_job_in(&e, &json!({"targets": ["rekordbox"]}), Some(&root), analyse).unwrap();
    started.wait();
    let out = root.join("test set");
    for f in ["test set.rekordbox.xml", "test set.export.txt"] {
        std::fs::write(out.join(f), "mine").unwrap();
    }
    go.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = job.wait().unwrap();
    for (key, f) in [("rekordbox", "test set.rekordbox.xml"), ("report", "test set.export.txt")] {
        assert!(r[key]["error"].as_str().unwrap().contains("is not an earlier export"), "{r}");
        assert_eq!(std::fs::read_to_string(out.join(f)).unwrap(), "mine");
    }
    assert_eq!(r["tracks"].as_array().unwrap().len(), 2, "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The audio of an imported movie has no audio file of its own: its source remembers none.
#[test]
fn a_movie_s_audio_remembers_no_original() {
    let dir = scratch("movie");
    let movie = dir.join("clip.mp4");
    let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 48_000]] };
    let bytes = encode(&buf, &EncodeOptions::default()).unwrap();
    std::fs::write(&movie, &bytes).unwrap();
    let mut e = Engine::default();
    let r = soundcraft_engine::io::import_video_bytes(&mut e, &movie.to_string_lossy(), bytes, 0).unwrap();
    assert!(r["audio"].is_object(), "{r}");
    let src = &e.session().sources[0];
    assert_eq!((src.origin.as_deref(), src.origin_len, src.origin_modified_ms), (None, 0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session copy of an MP3 is its gapless decode, without the priming a LAME-tagged MP3 starts
/// with (1105 samples: 25 ms at 44.1 kHz), and the set's grid and cues are on the MP3's timeline
/// with it: exported on the copy, they move that much earlier. With the original gone (priming
/// unknown), the doubt says they may sit late.
#[test]
fn a_session_copy_of_an_mp3_moves_its_grid_by_the_priming() {
    let ffmpeg = Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success());
    if !ffmpeg {
        println!("note: ffmpeg is not on the PATH; this test checks nothing");
        return;
    }
    let dir = scratch("priming");
    let lib = dir.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let mp3 = lib.join("a.mp3");
    let o = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=f=220:d=12:sample_rate=44100", "-c:a", "libmp3lame", "-b:a", "192k"])
        .arg(&mp3)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let b = lib.join("b.wav");
    std::fs::write(&b, encode(&AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.1; 30 * 44_100]] }, &EncodeOptions::default()).unwrap())
        .unwrap();
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let mut clips = Vec::new();
    for (path, bar) in [(&mp3, 1), (&b, 4)] {
        // Downbeats from 0.5 s, a bar every 2 s.
        let beats: Vec<f64> = (0..20).map(|k| 0.5 + k as f64 * 0.5).collect();
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        clips.push(r["clip"].as_u64().unwrap());
    }
    e.execute("dj.transition", &json!({"out_clip": clips[0], "in_clip": clips[1], "entry_bar": 5, "build_bars": 1, "out_bars": 1})).unwrap();
    // Saved: the session keeps a resampled WAV copy of a.mp3.
    let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
    soundcraft_engine::io::save_session(&mut e, &path).unwrap();

    let first = |root: &str| {
        let r = export(&e, &dir.join(root), analysis());
        let xml = std::fs::read_to_string(dir.join(root).join("Set/Set.rekordbox.xml")).unwrap();
        let t = collection(&xml).remove(0);
        (r, t.inizio[0], hot_cue(&t, "2"))
    };
    // The original MP3: its own timeline.
    let (r, bar, out) = first("original");
    assert!(r["tracks"][0]["path"].as_str().unwrap().ends_with("a.mp3"), "{r}");
    assert_eq!(bar, 0.5);

    // Edited after import: the session's copy is exported, its grid and cues 25 ms earlier.
    std::fs::OpenOptions::new().append(true).open(&mp3).unwrap().write_all(b"\0\0\0\0").unwrap();
    let (r, copy_bar, copy_out) = first("copy");
    assert!(r["tracks"][0]["path"].as_str().unwrap().ends_with(".wav"), "{r}");
    // 1105 / 44100 s = 25.06 ms; the XML has milliseconds.
    assert_eq!((copy_bar, copy_out), (0.475, out - 0.025), "{r}");

    // The original gone: the copy's grid stays, with a doubt.
    std::fs::remove_file(&mp3).unwrap();
    let (r, gone_bar, _) = first("gone");
    assert_eq!(gone_bar, 0.5);
    assert!(r["tracks"][0]["doubts"].to_string().contains("may sit ~23 ms late on this copy"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}
