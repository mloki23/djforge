//! The mixforge sidecar against a fixture library (a `library.db` built in a temporary
//! MIXFORGE_HOME, never mixforge's own): it passes on the track's section boundaries from the
//! `structure` cache, read-only, for the deck cue planner. Needs DJFORGE_PYTHON (mixforge's
//! environment) and DJFORGE_MIXFORGE; says so and checks nothing without them.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The sidecar, as the engine runs it.
const SIDECAR: &str = include_str!("../src/cmd/mixforge_beats.py");

/// Builds the fixture library at `home`: the analysis of `track` and its all-in-one structure.
const FIXTURE: &str = r#"
import json, os, sqlite3, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from mixforge import ANALYSIS_VERSION
home, track = Path(sys.argv[2]), Path(sys.argv[3]).resolve()
st = os.stat(track)
info = {"path": str(track), "size": st.st_size, "mtime": int(st.st_mtime), "version": ANALYSIS_VERSION,
        "duration": 10.0, "audio_end": 10.0, "bpm": 128.0, "first_downbeat": 0.5, "bars": 5, "constant_tempo": True,
        "grid_ok_4bar": [True, True], "bar_change": [1.0] * 5, "camelot": "8A", "key": "A minor"}
structure = {"segments": [{"start": 0.0, "end": 0.5, "label": "start"}, {"start": 0.5, "end": 4.25, "label": "intro"},
                          {"start": 4.25, "end": 10.0, "label": "drop"}], "downbeats": [0.5, 2.375, 4.25]}
db = sqlite3.connect(home / "library.db")
db.execute("CREATE TABLE tracks (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, version INTEGER, data TEXT)")
db.execute("CREATE TABLE structure (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, data TEXT)")
db.execute("INSERT INTO tracks VALUES (?, ?, ?, ?, ?)", (str(track), st.st_size, int(st.st_mtime), ANALYSIS_VERSION, json.dumps(info)))
db.execute("INSERT INTO structure VALUES (?, ?, ?, ?)", (str(track), st.st_size, int(st.st_mtime), json.dumps(structure)))
db.commit()
db.close()
"#;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup() -> Option<(String, String)> {
    let var = |n: &str| std::env::var(n).ok().filter(|v| !v.trim().is_empty());
    let (Some(python), Some(mixforge)) = (var("DJFORGE_PYTHON"), var("DJFORGE_MIXFORGE")) else {
        println!("note: DJFORGE_PYTHON or DJFORGE_MIXFORGE is not set; this test checks nothing");
        return None;
    };
    let ok = Command::new(&python).args(["-I", "-c", "import sys; sys.path.insert(0, sys.argv[1]); import mixforge.library", &mixforge]).output();
    if !ok.is_ok_and(|o| o.status.success()) {
        println!("note: {python} can't import mixforge from {mixforge}; this test checks nothing");
        return None;
    }
    Some((python, mixforge))
}

fn wav(path: &Path) {
    let frames = 10 * 48_000;
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; frames]; 2] };
    std::fs::write(path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
}

#[test]
fn the_sidecar_passes_on_cached_sections_without_writing_the_library() {
    let Some((python, mixforge)) = setup() else { return };
    let home = std::env::temp_dir().join(format!("djforge-sidecar-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let _scratch = Scratch(home.clone());
    let track = home.join("track.wav");
    wav(&track);
    let built = Command::new(&python).args(["-I", "-c", FIXTURE, &mixforge]).arg(&home).arg(&track).output().unwrap();
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    let db = home.join("library.db");
    let before = std::fs::read(&db).unwrap();
    let out = Command::new(&python).args(["-I", "-c", SIDECAR, &mixforge]).arg(&track).env("MIXFORGE_HOME", &home).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["bpm"], 128.0, "{v}");
    // all-in-one's segments on mixforge's grid (1.875 s bars from 0.5 s), start/end left out.
    assert_eq!(v["sections"], serde_json::json!([{"bar": 0, "label": "intro", "bars": 2}, {"bar": 2, "label": "drop", "bars": 3}]), "{v}");
    assert_eq!(v["grid_ok_4bar"], serde_json::json!([true, true]));
    assert_eq!(std::fs::read(&db).unwrap(), before, "the library is only read");
}
