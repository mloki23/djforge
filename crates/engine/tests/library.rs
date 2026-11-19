//! The library the decks load from: mixforge's `library.db` (always a fixture built here, in a
//! temporary folder, with mixforge's schema) and folders of audio files. Listing, sorting,
//! searching, paging, the compatible filter, the missing and stale flags, reading only, and
//! `deck.load` taking its grid from a fresh row.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, wav_float_header};
use soundcraft_engine::Engine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const RATE: u32 = 48_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-library-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wav(dir: &Path, name: &str, secs: f64) -> String {
    let frames = (secs * f64::from(RATE)) as usize;
    let channels = (0..2).map(|_| (0..frames).map(|k| (0.2 * (k as f64 * 0.05).sin()) as f32).collect()).collect();
    let buf = AudioBuffer { sample_rate: RATE, channels };
    let mut bytes = wav_float_header(2, RATE, frames as u64).unwrap();
    bytes.extend(buf.interleaved().iter().flat_map(|x| x.to_le_bytes()));
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

/// A file's size and modification time as mixforge stores them (`int(st_mtime)`).
fn stamp(path: &str) -> (i64, i64) {
    let m = std::fs::metadata(path).unwrap();
    (m.len() as i64, m.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64)
}

/// mixforge's schema (`library.py`'s `_connect`).
fn new_db(dir: &Path) -> PathBuf {
    let db = dir.join("library.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, version INTEGER, data TEXT);
         CREATE TABLE IF NOT EXISTS structure (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, data TEXT);",
    )
    .unwrap();
    db
}

/// Stores an analysis of `path` as mixforge's `store` does; `(size, mtime)` the file's then.
fn store(db: &Path, path: &str, at: (i64, i64), data: Value) {
    let c = rusqlite::Connection::open(db).unwrap();
    let mut data = data;
    data["path"] = json!(path);
    c.execute("INSERT OR REPLACE INTO tracks VALUES (?1, ?2, ?3, 9, ?4)", rusqlite::params![path, at.0, at.1, data.to_string()]).unwrap();
}

fn store_structure(db: &Path, path: &str, at: (i64, i64), data: Value) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute("INSERT OR REPLACE INTO structure VALUES (?1, ?2, ?3, ?4)", rusqlite::params![path, at.0, at.1, data.to_string()]).unwrap();
}

fn track(title: &str, artist: Option<&str>, bpm: f64, camelot: &str, duration: f64) -> Value {
    json!({"title": title, "artist": artist, "bpm": bpm, "first_downbeat": 0.5, "duration": duration, "audio_end": duration,
        "camelot": camelot, "key": "C minor", "bars": (duration / (240.0 / bpm)) as i64, "constant_tempo": true})
}

fn engine() -> Engine {
    let mut e = Engine::default();
    e.live_audio = true;
    e
}

fn run(e: &mut Engine, cmd: &str, p: Value) -> Value {
    e.execute(cmd, &p).unwrap_or_else(|err| panic!("{cmd} {p}: {err}"))
}

fn err(e: &mut Engine, cmd: &str, p: Value) -> String {
    e.execute(cmd, &p).err().unwrap_or_else(|| panic!("{cmd} {p} should be refused")).to_string()
}

fn wait(e: &mut Engine) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..2000 {
        out.extend(e.poll_deck_loads().into_iter().map(|r| r.unwrap_or_else(|err| json!({"error": err.to_string()}))));
        if !e.deck_loading() {
            return out;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the deck load never finished");
}

fn titles(v: &Value) -> Vec<String> {
    v["rows"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap().to_string()).collect()
}

/// A library of five tracks (files on disk) and one whose file is gone.
fn library(dir: &Path) -> (PathBuf, Vec<String>) {
    let db = new_db(dir);
    let specs = [
        ("Ocean Drive", Some("Arty"), 138.0, "8A", 2.0),
        ("Silent Hour", Some("Above"), 140.0, "9A", 3.0),
        ("Amber Sky", None, 70.0, "12A", 1.0),
        ("Night Ocean", Some("Bryan"), 132.0, "1A", 4.0),
        ("Zenith", Some("Above"), 150.0, "12B", 2.5),
    ];
    let mut paths = Vec::new();
    for (i, (title, artist, bpm, key, secs)) in specs.into_iter().enumerate() {
        let path = wav(dir, &format!("track{i}.wav"), secs);
        store(&db, &path, stamp(&path), track(title, artist, bpm, key, secs));
        paths.push(path);
    }
    let gone = dir.join("gone.wav").to_string_lossy().into_owned();
    store(&db, &gone, (10, 10), track("Gone Track", Some("Nobody"), 141.0, "8B", 1.0));
    (db, paths)
}

#[test]
fn the_library_lists_sorts_searches_and_pages() {
    let dir = Scratch::new("list");
    let (db, _) = library(&dir.0);
    let mut e = engine();
    assert!(err(&mut e, "library.list", json!({})).contains("isn't set up"), "no library until the app sets one");
    e.set_library(db);
    let v = run(&mut e, "library.list", json!({}));
    assert_eq!(v["total"], 6);
    assert_eq!(titles(&v), ["Amber Sky", "Gone Track", "Night Ocean", "Ocean Drive", "Silent Hour", "Zenith"], "by title");
    let amber = &v["rows"][0];
    assert_eq!((amber["artist"].as_str(), amber["key"].as_str(), amber["analysed"].as_bool()), (Some(""), Some("12A"), Some(true)));
    assert_eq!((amber["missing"].as_bool(), amber["stale"].as_bool(), amber["length_s"].as_f64()), (Some(false), Some(false), Some(1.0)));
    assert_eq!(v["rows"][1]["missing"], true, "its file is gone");
    let v = run(&mut e, "library.list", json!({"sort": "bpm", "desc": true}));
    assert_eq!(titles(&v)[..3], ["Zenith", "Gone Track", "Silent Hour"]);
    let v = run(&mut e, "library.list", json!({"sort": "key"}));
    assert_eq!(titles(&v), ["Night Ocean", "Ocean Drive", "Gone Track", "Silent Hour", "Amber Sky", "Zenith"], "Camelot order");
    let v = run(&mut e, "library.list", json!({"sort": "artist"}));
    assert_eq!(titles(&v).last().map(String::as_str), Some("Amber Sky"), "no artist goes last");
    // Paging: the total is the whole listing's.
    let v = run(&mut e, "library.list", json!({"offset": 2, "limit": 2}));
    assert_eq!((v["total"].as_u64(), titles(&v)), (Some(6), vec!["Night Ocean".to_string(), "Ocean Drive".to_string()]));
    assert_eq!(run(&mut e, "library.list", json!({"offset": 10}))["rows"], json!([]));
    // Search: every word, in the title, artist or file name, any case.
    let v = run(&mut e, "library.search", json!({"query": "OCEAN"}));
    assert_eq!(titles(&v), ["Night Ocean", "Ocean Drive"]);
    assert_eq!(titles(&run(&mut e, "library.search", json!({"query": "above zen"}))), ["Zenith"]);
    assert_eq!(titles(&run(&mut e, "library.search", json!({"query": "track2"}))), ["Amber Sky"], "the file name");
    assert!(err(&mut e, "library.search", json!({"query": " "})).contains("`query` required"));
    for bad in [
        json!({"sort": "colour"}),
        json!({"limit": 0}),
        json!({"limit": 5000}),
        json!({"source": "itunes"}),
        json!({"source": "folder"}),
        json!({"offset": -1}),
    ] {
        err(&mut e, "library.list", bad);
    }
}

#[test]
fn compatible_tracks_match_the_decks_key_and_tempo() {
    let dir = Scratch::new("compatible");
    let db = new_db(&dir.0);
    // The deck's track: 12A at 140.
    let deck = wav(&dir.0, "deck.wav", 4.0);
    store(&db, &deck, stamp(&deck), track("Deck Track", None, 140.0, "12A", 4.0));
    let rows = [
        ("Wrap", 140.0, "1A"),     // one step round the wheel past 12
        ("Relative", 70.0, "12B"), // the relative key, half time
        ("Below", 148.0, "11A"),   // +5.7 %
        ("Too Fast", 150.0, "12A"),
        ("Wrong Key", 140.0, "5A"),
        ("Double", 280.0, "12A"),
        ("Diagonal", 140.0, "1B"),
    ];
    for (i, (title, bpm, key)) in rows.into_iter().enumerate() {
        let p = wav(&dir.0, &format!("c{i}.wav"), 1.0);
        store(&db, &p, stamp(&p), track(title, None, bpm, key, 1.0));
    }
    let mut e = engine();
    e.set_library(db);
    assert!(err(&mut e, "library.compatible", json!({"deck": "a"})).contains("load a track first"));
    run(&mut e, "deck.load", json!({"path": deck, "deck": "a"}));
    let loaded = wait(&mut e).remove(0);
    assert_eq!((loaded["grid_from"].as_str(), loaded["key"].as_str()), (Some("library"), Some("12A")), "{loaded}");
    let v = run(&mut e, "library.compatible", json!({"deck": "a"}));
    assert_eq!(titles(&v), ["Deck Track", "Double", "Wrap", "Below", "Relative"], "same key, then ±1, then relative; nearer tempo first");
    assert_eq!(v["deck"]["key"], "12A");
    let by = |t: &str| v["rows"].as_array().unwrap().iter().find(|r| r["title"] == t).unwrap().clone();
    assert_eq!((by("Wrap")["key_match"].as_str(), by("Wrap")["bpm_ratio"].as_f64()), (Some("adjacent"), Some(1.0)));
    assert_eq!((by("Relative")["key_match"].as_str(), by("Relative")["bpm_ratio"].as_f64()), (Some("relative"), Some(2.0)));
    assert_eq!(by("Double")["bpm_ratio"], 0.5);
    assert_eq!(by("Below")["bpm_diff_pct"], 5.7);
    assert!(by("Relative")["why"].as_str().unwrap().contains("12B (relative)"), "{}", by("Relative")["why"]);
    // The deck's tempo now counts (its pitch): at 150 only "Too Fast" (same key) and "Below" fit.
    let mut st = e.deck_status;
    st.decks[0].bpm = 150.0;
    e.set_deck_status(st);
    let v = run(&mut e, "library.compatible", json!({"deck": "a", "sort": "title"}));
    assert_eq!(titles(&v), ["Below", "Too Fast"], "{v}");
}

#[test]
fn deck_load_takes_a_fresh_library_row_and_falls_back_on_a_stale_one() {
    let dir = Scratch::new("load");
    let db = new_db(&dir.0);
    let path = wav(&dir.0, "track.wav", 75.0);
    let at = stamp(&path);
    let mut a = track("From Library", Some("Arty"), 128.0, "5A", 75.0);
    a["grid_ok_4bar"] = json!(vec![true; 10]);
    a["bar_change"] = json!(vec![1.0; 40]);
    store(&db, &path, at, a);
    // all-in-one's sections, on the grid's bars (1.875 s): a drop at bar 24.
    let drop = 0.5 + 24.0 * 1.875;
    store_structure(
        &db,
        &path,
        at,
        json!({"segments": [
        {"start": 0.5, "end": drop - 15.0, "label": "intro"}, {"start": drop - 15.0, "end": drop, "label": "build"},
        {"start": drop, "end": 75.0, "label": "drop"}]}),
    );
    let before = (std::fs::read(&db).unwrap(), std::fs::metadata(&db).unwrap().modified().unwrap());
    let mut e = engine();
    e.set_library(db.clone());
    let sidecar = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&sidecar);
    e.set_analyse(Arc::new(move |_: &str, _: &dyn Fn() -> bool| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"bpm": 130.0, "first_downbeat": 0.25, "duration": 75.0, "camelot": "6A"}))
    }));
    run(&mut e, "deck.load", json!({"path": path, "deck": "a"}));
    let v = wait(&mut e).remove(0);
    assert_eq!(
        (v["grid_from"].as_str(), v["library"].as_str(), v["key"].as_str(), v["cues_from"].as_str()),
        (Some("library"), Some("fresh"), Some("5A"), Some("mixforge")),
        "{v}"
    );
    assert_eq!(sidecar.load(Ordering::SeqCst), 0, "no sidecar run");
    let t = e.deck_tracks[0].clone().unwrap();
    assert_eq!(t.grid.as_ref().unwrap().first(), 0.5 * 48_000.0);
    let bar = 1.875 * 48_000.0;
    assert_eq!(t.cues[2], Some(24_000.0 + 24.0 * bar), "Mix out on the drop, from the structure row");
    // The file changed since mixforge analysed it: the row is stale and the sidecar grids it.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    wav(&dir.0, "track.wav", 74.0);
    run(&mut e, "deck.load", json!({"path": path, "deck": "a"}));
    let v = wait(&mut e).remove(0);
    assert_eq!((v["grid_from"].as_str(), v["library"].as_str(), v["key"].as_str()), (Some("mixforge"), Some("stale"), Some("6A")), "{v}");
    assert_eq!(sidecar.load(Ordering::SeqCst), 1);
    assert_eq!(run(&mut e, "library.list", json!({}))["rows"][0]["stale"], true);
    // A file mixforge never saw goes to the sidecar too.
    let other = wav(&dir.0, "other.wav", 5.0);
    run(&mut e, "deck.load", json!({"path": other, "deck": "b"}));
    assert_eq!(wait(&mut e).remove(0)["library"], "absent");
    assert_eq!((std::fs::read(&db).unwrap(), std::fs::metadata(&db).unwrap().modified().unwrap()), before, "the library is never written");
}

#[test]
fn reading_never_writes_and_goes_on_while_mixforge_writes() {
    let dir = Scratch::new("readonly");
    let (db, paths) = library(&dir.0);
    let before = (std::fs::read(&db).unwrap(), std::fs::metadata(&db).unwrap().modified().unwrap());
    let mut e = engine();
    e.set_library(db.clone());
    assert_eq!(run(&mut e, "library.list", json!({}))["total"], 6);
    run(&mut e, "deck.load", json!({"path": paths[0], "deck": "a"}));
    assert_eq!(wait(&mut e)[0]["library"], "fresh");
    // mixforge committing (an exclusive lock) while djforge lists: a copy is read.
    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE; DELETE FROM tracks;").unwrap();
    // A new engine (nothing read yet) reads it now.
    let mut fresh = engine();
    fresh.set_library(db.clone());
    let v = run(&mut fresh, "library.list", json!({}));
    assert_eq!((v["total"].as_u64(), v["from_copy"].as_bool()), (Some(6), Some(true)), "{v}");
    run(&mut fresh, "deck.load", json!({"path": paths[1], "deck": "a"}));
    assert_eq!(wait(&mut fresh)[0]["grid_from"], "library", "a load reads the copy too");
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);
    assert_eq!((std::fs::read(&db).unwrap(), std::fs::metadata(&db).unwrap().modified().unwrap()), before, "nothing was written");
}

#[test]
fn a_folder_lists_its_audio_files_joined_to_the_library() {
    let dir = Scratch::new("folder");
    let music = dir.0.join("Music");
    std::fs::create_dir_all(music.join("Trance")).unwrap();
    std::fs::create_dir_all(music.join("ambient")).unwrap();
    let analysed = wav(&music, "analysed.wav", 2.0);
    wav(&music, "plain.WAV", 1.0);
    std::fs::write(music.join("notes.txt"), "not audio").unwrap();
    std::fs::write(music.join(".hidden.mp3"), "not listed").unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    let fill = soundcraft_audio_io::mp3tags::TagFill { camelot: Some("4B"), bpm: Some(136.0), prep_note: "" };
    let (tagged, _) = soundcraft_audio_io::mp3tags::fill(&std::fs::read(fixture).unwrap(), &fill).unwrap();
    std::fs::write(music.join("tagged.mp3"), tagged).unwrap();
    let db = new_db(&dir.0);
    store(&db, &analysed, stamp(&analysed), track("Analysed One", Some("Arty"), 138.0, "8A", 2.0));
    let mut e = engine();
    let folder = music.to_string_lossy().into_owned();
    // Without mixforge's library a folder still lists.
    let v = run(&mut e, "library.list", json!({"source": "folder", "folder": folder}));
    assert_eq!(v["total"], 3);
    assert_eq!(v["library_error"], Value::Null);
    e.set_library(db);
    let v = run(&mut e, "library.list", json!({"source": "folder", "folder": folder, "sort": "name"}));
    assert_eq!((v["source"].as_str(), v["total"].as_u64(), v["truncated"].as_bool()), (Some("folder"), Some(3), Some(false)));
    let names: Vec<&str> = v["rows"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["analysed.wav", "plain.WAV", "tagged.mp3"]);
    let subs: Vec<&str> = v["folders"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(subs, ["ambient", "Trance"]);
    let r = &v["rows"];
    assert_eq!((r[0]["title"].as_str(), r[0]["analysed"].as_bool(), r[0]["bpm"].as_f64()), (Some("Analysed One"), Some(true), Some(138.0)));
    assert_eq!((r[1]["title"].as_str(), r[1]["analysed"].as_bool(), r[1]["bpm"].as_f64()), (Some("plain"), Some(false), None));
    assert_eq!((r[2]["key"].as_str(), r[2]["bpm"].as_f64()), (Some("4B"), Some(136.0)), "the MP3's tags fill in its row");
    assert_eq!(titles(&run(&mut e, "library.search", json!({"source": "folder", "folder": folder, "query": "plain"}))), ["plain"]);
    assert!(err(&mut e, "library.list", json!({"source": "folder", "folder": dir.0.join("nope")})).contains("nope"));
}

#[test]
fn folders_added_are_kept_in_the_preferences() {
    let dir = Scratch::new("roots");
    let (music, added) = (dir.0.join("Music"), dir.0.join("Sets"));
    std::fs::create_dir_all(&music).unwrap();
    std::fs::create_dir_all(&added).unwrap();
    let prefs = dir.0.join("config").join("library-folders.json");
    let mut e = engine();
    e.set_library_folders(Some(music.clone()), Some(prefs.clone()));
    let v = run(&mut e, "library.folders", json!({}));
    assert_eq!(v["roots"].as_array().unwrap().len(), 1);
    assert_eq!((v["roots"][0]["kind"].as_str(), v["roots"][0]["found"].as_bool()), (Some("music"), Some(true)));
    assert_eq!(v["mixforge"], Value::Null);
    assert_eq!(run(&mut e, "library.add_folder", json!({"path": added}))["added"], true);
    assert_eq!(run(&mut e, "library.add_folder", json!({"path": added}))["added"], false, "once");
    assert!(err(&mut e, "library.add_folder", json!({"path": dir.0.join("nope")})).contains("no such folder"));
    // Another run reads them back.
    let mut again = engine();
    again.set_library_folders(Some(music), Some(prefs.clone()));
    let v = run(&mut again, "library.folders", json!({}));
    assert_eq!(v["roots"][1]["kind"], "added");
    assert_eq!(v["roots"][1]["path"], added.display().to_string());
    assert_eq!(run(&mut again, "library.remove_folder", json!({"path": added}))["removed"], true);
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&prefs).unwrap()).unwrap();
    assert_eq!(kept["folders"], json!([]));
}
