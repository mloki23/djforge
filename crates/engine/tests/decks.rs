//! The live deck commands: `deck.load` reads files on its own thread and grids them (given,
//! mixforge, BPM tag), and the transport and mixer commands queue the audio engine's
//! commands, refusing what can't be done. Files live in a temporary folder removed on drop;
//! mixforge's analysis is always a fake.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, wav_float_header};
use soundcraft_engine::{DeckRequest, Engine, TransportRequest};
use soundcraft_mix::{DeckCmd, DeckId, EqBand, HotCueAction, LoopAction};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const RATE: u32 = 48_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-decks-{name}-{}", std::process::id()));
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

/// Waits for the loads under way and returns what they reported.
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

fn deck_cmds(e: &mut Engine) -> Vec<DeckCmd> {
    std::mem::take(&mut e.transport_requests)
        .into_iter()
        .filter_map(|r| match r {
            TransportRequest::Deck(DeckRequest(c)) => Some(c),
            _ => None,
        })
        .collect()
}

#[test]
fn deck_load_runs_off_the_ui_thread_and_falls_back_to_the_bpm_tag() {
    let dir = Scratch::new("tag");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    let fill = soundcraft_audio_io::mp3tags::TagFill { camelot: Some("8A"), bpm: Some(124.0), prep_note: "" };
    let (tagged, _) = soundcraft_audio_io::mp3tags::fill(&std::fs::read(fixture).unwrap(), &fill).unwrap();
    let path = dir.0.join("tagged.mp3");
    std::fs::write(&path, tagged).unwrap();
    let mut e = engine();
    let seen = Arc::new(Mutex::new(None));
    let s = Arc::clone(&seen);
    e.set_analyse(Arc::new(move |_: &str, _: &dyn Fn() -> bool| {
        *s.lock().unwrap() = Some(std::thread::current().id());
        Err("not analysed by mixforge".to_string())
    }));
    let r = run(&mut e, "deck.load", json!({"path": path}));
    assert_eq!((r["deck"].as_str(), r["loading"].as_bool()), (Some("a"), Some(true)), "returns at once");
    let done = wait(&mut e);
    let ran_on = seen.lock().unwrap().expect("the analysis ran");
    assert_ne!(ran_on, std::thread::current().id(), "the load ran on a thread of its own");
    assert_eq!(done.len(), 1);
    let v = &done[0];
    assert_eq!(
        (v["grid"].as_str(), v["analysis_error"].as_str(), v["key"].as_str()),
        (Some("tag"), Some("not analysed by mixforge"), Some("8A")),
        "{v}"
    );
    assert!((v["bpm"].as_f64().unwrap() - 124.0).abs() < 1e-6);
    let t = e.deck_tracks[0].clone().unwrap();
    assert_eq!(t.grid.as_ref().unwrap().first(), 0.0);
    assert_eq!(e.deck_loads.len(), 1, "the prepared deck waits for the host");
    assert_eq!(e.deck_loads[0].0, DeckId::A);
    assert!(e.messages.iter().any(|m| m.starts_with("Deck A: loaded")));
}

#[test]
fn mixforge_gives_the_grid_key_and_first_hot_cues() {
    let dir = Scratch::new("mixforge");
    let path = wav(&dir.0, "track.wav", 12.0);
    let mut e = engine();
    e.set_analyse(Arc::new(|_: &str, _: &dyn Fn() -> bool| {
        Ok(json!({"bpm": 128.0, "first_downbeat": 0.25, "camelot": "5A", "cues": [1.0, {"time": 2.5}, -1, "x", 4.0, 5.0, 6.0]}))
    }));
    run(&mut e, "deck.load", json!({"path": path, "deck": "b"}));
    let v = wait(&mut e).remove(0);
    assert_eq!(
        (v["deck"].as_str(), v["grid"].as_str(), v["key"].as_str(), v["title"].as_str()),
        (Some("b"), Some("mixforge"), Some("5A"), Some("track"))
    );
    let t = e.deck_tracks[1].clone().unwrap();
    assert_eq!(t.grid.as_ref().unwrap().first(), 0.25 * 48_000.0);
    assert_eq!(t.cues[..5], [Some(48_000.0), Some(120_000.0), Some(192_000.0), Some(240_000.0), None], "four cues, A–D");
    // Given bpm wins over the analysis.
    run(&mut e, "deck.load", json!({"path": path, "deck": "a", "bpm": 140, "first_beat": 0.1}));
    let v = wait(&mut e).remove(0);
    assert_eq!(v["grid"], "given");
    // A file that won't load says why, and leaves the deck as it was.
    run(&mut e, "deck.load", json!({"path": dir.0.join("missing.wav"), "deck": "a"}));
    let v = wait(&mut e).remove(0);
    assert!(v["error"].as_str().unwrap().contains("missing.wav"), "{v}");
    assert!((e.deck_tracks[0].as_ref().unwrap().bpm.unwrap() - 140.0).abs() < 1e-6);
}

#[test]
fn deck_commands_queue_the_audio_engines_and_refuse_what_cant_be_done() {
    let dir = Scratch::new("cmds");
    let path = wav(&dir.0, "a.wav", 6.0);
    let mut headless = Engine::default();
    assert!(err(&mut headless, "deck.load", json!({"path": path})).contains("audio engine"));
    let mut e = engine();
    assert!(err(&mut e, "deck.play", json!({"deck": "a"})).contains("deck A is empty"));
    assert!(err(&mut e, "deck.play", json!({"deck": "c"})).contains("`deck`"));
    run(&mut e, "deck.load", json!({"path": path, "deck": "a", "bpm": 120}));
    run(&mut e, "deck.load", json!({"path": path, "deck": "b"}));
    wait(&mut e);
    e.transport_requests.clear();
    let a = DeckId::A;
    run(&mut e, "deck.play", json!({"deck": "a"}));
    run(&mut e, "deck.cue", json!({"deck": "a", "down": false}));
    run(&mut e, "deck.seek", json!({"deck": "a", "at": 1.5}));
    run(&mut e, "deck.pitch", json!({"deck": "a", "value": 4, "range": 16}));
    run(&mut e, "deck.hotcue", json!({"deck": "a", "n": 2, "action": "set"}));
    run(&mut e, "deck.loop", json!({"deck": "a", "beats": "1/4"}));
    run(&mut e, "deck.loop", json!({"deck": "a", "action": "halve"}));
    run(&mut e, "deck.beatjump", json!({"deck": "a", "beats": -4}));
    run(&mut e, "deck.eq", json!({"deck": "a", "band": "low", "db": "kill"}));
    run(&mut e, "deck.xfader", json!({"curve": "sharp"}));
    let cmds = deck_cmds(&mut e);
    let want = [
        DeckCmd::Play(a),
        DeckCmd::Cue { deck: a, down: false },
        DeckCmd::Seek { deck: a, at: 72_000.0 },
        DeckCmd::Pitch { deck: a, value: 0.04, range: Some(0.16) },
        DeckCmd::HotCue { deck: a, n: 1, action: HotCueAction::Set },
        DeckCmd::Loop { deck: a, action: LoopAction::Beats(0.25) },
        DeckCmd::Loop { deck: a, action: LoopAction::Halve },
        DeckCmd::BeatJump { deck: a, beats: -4.0 },
        DeckCmd::Eq { deck: a, band: EqBand::Low, db: -80.0 },
        DeckCmd::Crossfader { value: f32::NAN, curve: Some(soundcraft_mix::XfadeCurve::Sharp) },
    ];
    assert_eq!(cmds[..9], want[..9]);
    assert!(matches!(cmds.get(9), Some(DeckCmd::Crossfader { value, curve: Some(soundcraft_mix::XfadeCurve::Sharp) }) if value.is_nan()), "{cmds:?}");
    assert_eq!(cmds.len(), 10);
    // Deck B has no grid: no sync, no beat loops.
    assert!(err(&mut e, "deck.sync", json!({"deck": "a", "on": true})).contains("beat grid"));
    assert!(err(&mut e, "deck.loop", json!({"deck": "b", "beats": 4})).contains("beat grid"));
    for (cmd, p, why) in [
        ("deck.hotcue", json!({"deck": "a", "n": 9}), "`n`"),
        ("deck.loop", json!({"deck": "a", "beats": 64}), "`beats`"),
        ("deck.loop", json!({"deck": "a"}), "one of"),
        ("deck.pitch", json!({"deck": "a", "range": 10}), "`range`"),
        ("deck.eq", json!({"deck": "a", "band": "low", "db": 7}), "`db`"),
        ("deck.beatjump", json!({"deck": "a", "beats": 0}), "`beats`"),
        ("deck.jog", json!({"deck": "a", "delta": 0.5, "mode": "scratch"}), "`mode`"),
    ] {
        assert!(err(&mut e, cmd, p.clone()).contains(why), "{cmd} {p}");
    }
    assert!(deck_cmds(&mut e).is_empty(), "refusals queue nothing");
    let st = run(&mut e, "decks.state", json!({}));
    assert_eq!(st["decks"][0]["track"]["title"], "a");
    assert_eq!(st["decks"][1]["track"]["grid"], Value::Null);
    assert!(e.undo_history().is_empty(), "nothing here is undoable");
}

#[test]
fn arrangement_play_and_deck_play_say_that_the_other_stops() {
    let dir = Scratch::new("rule");
    let path = wav(&dir.0, "a.wav", 4.0);
    let mut e = engine();
    run(&mut e, "deck.load", json!({"path": path, "bpm": 120}));
    wait(&mut e);
    e.deck_status.decks[0].playing = true;
    run(&mut e, "transport.play", json!({}));
    assert!(e.messages.iter().any(|m| m == "The decks stopped: the arrangement plays"));
    e.deck_status.decks[0].playing = false;
    e.transport.playing = true;
    run(&mut e, "deck.play", json!({"deck": "a"}));
    assert!(e.messages.iter().any(|m| m == "The arrangement stopped: a deck plays"));
    // The idle deck is the default for a load.
    e.deck_status.decks[0].playing = true;
    assert_eq!(run(&mut e, "deck.load", json!({"path": path}))["deck"], "b");
    wait(&mut e);
}
