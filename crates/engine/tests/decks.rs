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

/// An analysis as mixforge's library holds it (`analysis.py`, sections from `structure.py`):
/// 128 bpm from 0.5 s, 40 bars.
fn analysis() -> Value {
    json!({
        "bpm": 128.0, "first_downbeat": 0.5, "duration": 75.0, "audio_end": 75.5, "camelot": "5A", "key": "C minor",
        "constant_tempo": true, "downbeat_confidence": 0.9, "lufs": -8.0,
        "grid_ok_4bar": vec![true; 10],
        "bar_change": vec![1.0; 40],
        "sections": [{"bar": 0, "label": "intro"}, {"bar": 16, "label": "build"}, {"bar": 24, "label": "drop"}]
    })
}

#[test]
fn mixforge_gives_the_grid_key_and_its_mix_points_as_hot_cues() {
    let dir = Scratch::new("mixforge");
    let path = wav(&dir.0, "track.wav", 75.0);
    let mut e = engine();
    e.set_analyse(Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(analysis())));
    run(&mut e, "deck.load", json!({"path": path, "deck": "b"}));
    let v = wait(&mut e).remove(0);
    assert_eq!(
        (v["deck"].as_str(), v["grid"].as_str(), v["key"].as_str(), v["title"].as_str(), v["cues_from"].as_str()),
        (Some("b"), Some("mixforge"), Some("5A"), Some("track"), Some("mixforge")),
        "{v}"
    );
    assert_eq!(v["priming_ms"], 0.0, "not an MP3");
    let t = e.deck_tracks[1].clone().unwrap();
    assert_eq!(t.grid.as_ref().unwrap().first(), 0.5 * 48_000.0);
    // Mix in on the first downbeat, the bass 16 bars on; Mix out on the drop (bar 24), the last
    // boundary leaving room; Bass gone 24 bars after it falls past the end.
    let bar = 1.875 * 48_000.0;
    assert_eq!(t.cues[..5], [Some(24_000.0), Some(24_000.0 + 16.0 * bar), Some(24_000.0 + 24.0 * bar), None, None]);
    // Given bpm wins over the analysis's grid.
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
fn a_track_in_the_open_set_takes_the_sets_cues() {
    let dir = Scratch::new("set");
    let (a, b) = (wav(&dir.0, "a.wav", 40.0), wav(&dir.0, "b.wav", 40.0));
    let mut e = engine();
    let ra = run(&mut e, "dj.load_warped", json!({"path": a, "bpm": 120, "first_beat": 0.0, "at_bar": 1}));
    let rb = run(&mut e, "dj.load_warped", json!({"path": b, "bpm": 120, "first_beat": 0.0, "at_bar": 9}));
    run(&mut e, "dj.transition", json!({"out_clip": ra["clip"], "in_clip": rb["clip"], "build_bars": 4, "out_bars": 4}));
    e.set_analyse(Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(analysis())));
    run(&mut e, "deck.load", json!({"path": b, "deck": "a"}));
    let v = wait(&mut e).remove(0);
    assert_eq!(v["cues_from"], "session", "{v}");
    let t = e.deck_tracks[0].clone().unwrap();
    // B comes in on bar 9 (its first beat) and its bass hits 4 bars (8 s at 120 bpm) later; it
    // goes out nowhere in this set.
    let [mix_in, hit, out, gone] = [0, 1, 2, 3].map(|i| t.cues[i]);
    assert!(mix_in.unwrap().abs() < 1.0 && (hit.unwrap() - 8.0 * 48_000.0).abs() < 1.0, "{mix_in:?} {hit:?}");
    assert_eq!((out, gone), (None, None));
    // A plays out of it: Mix out on the same bar, 8 bars (16 s) into A; Bass gone 8 bars on.
    run(&mut e, "deck.load", json!({"path": a, "deck": "b"}));
    wait(&mut e);
    let t = e.deck_tracks[1].clone().unwrap();
    assert!((t.cues[2].unwrap() - 16.0 * 48_000.0).abs() < 1.0 && (t.cues[3].unwrap() - 32.0 * 48_000.0).abs() < 1.0, "{:?}", t.cues);
}

#[test]
fn a_playing_deck_refuses_a_load_unless_forced_even_if_it_started_while_decoding() {
    let dir = Scratch::new("lock");
    let path = wav(&dir.0, "a.wav", 4.0);
    let mut e = engine();
    e.deck_status.decks[0].playing = true;
    assert!(err(&mut e, "deck.load", json!({"path": path, "deck": "a"})).contains("Deck A is playing: pause it first"));
    run(&mut e, "deck.load", json!({"path": path, "deck": "a", "force": true, "bpm": 120}));
    assert!(wait(&mut e)[0]["error"].is_null());
    assert_eq!(e.deck_loads.len(), 1);
    // Issued while stopped, started before the file was ready: refused at the finish.
    run(&mut e, "deck.load", json!({"path": path, "deck": "b"}));
    e.deck_status.decks[1].playing = true;
    let v = wait(&mut e).remove(0);
    assert!(v["error"].as_str().unwrap().contains("Deck B is playing"), "{v}");
    assert_eq!(e.deck_loads.len(), 1, "nothing handed on");
}

#[test]
fn the_last_load_issued_wins_and_the_one_it_replaces_stops_early() {
    let dir = Scratch::new("gen");
    let (slow, fast) = (wav(&dir.0, "slow.wav", 4.0), wav(&dir.0, "fast.wav", 4.0));
    let mut e = engine();
    let stopped = Arc::new(Mutex::new(false));
    let s = Arc::clone(&stopped);
    e.set_analyse(Arc::new(move |_: &str, stop: &dyn Fn() -> bool| {
        for _ in 0..2000 {
            if stop() {
                *s.lock().unwrap() = true;
                return Err("cancelled".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(json!({"bpm": 128.0, "first_downbeat": 0.0}))
    }));
    run(&mut e, "deck.load", json!({"path": slow, "deck": "a"}));
    run(&mut e, "deck.load", json!({"path": fast, "deck": "a", "bpm": 124}));
    let done = wait(&mut e);
    assert_eq!(done.len(), 1, "{done:?}");
    assert!(done[0]["path"].as_str().unwrap().ends_with("fast.wav"));
    assert!(*stopped.lock().unwrap(), "the superseded analysis was stopped");
    assert_eq!(e.deck_loads.len(), 1);
    assert!(e.deck_tracks[0].as_ref().unwrap().path.ends_with("fast.wav"));
}

#[test]
fn a_rate_change_while_loading_refuses_and_a_lost_track_is_forgotten() {
    let dir = Scratch::new("rate");
    let path = wav(&dir.0, "a.wav", 4.0);
    let mut e = engine();
    run(&mut e, "deck.load", json!({"path": path, "deck": "a", "bpm": 120}));
    e.session_mut().sample_rate = soundcraft_time::SampleRate::new(96_000).unwrap();
    assert!(wait(&mut e)[0]["error"].as_str().unwrap().contains("sample rate changed"));
    e.session_mut().sample_rate = soundcraft_time::SampleRate::new(48_000).unwrap();
    run(&mut e, "deck.load", json!({"path": path, "deck": "a", "bpm": 120}));
    wait(&mut e);
    let load = e.deck_loads[0].1.load();
    let mut st = e.deck_status;
    st.decks[0].load = load;
    e.set_deck_status(st);
    assert!(e.deck_tracks[0].is_some());
    // The audio thread's engine was replaced: the deck is empty, and so is decks.state.
    st.decks[0].load = 0;
    e.set_deck_status(st);
    assert!(e.deck_tracks[0].is_none());
    assert_eq!(run(&mut e, "decks.state", json!({}))["decks"][0]["track"], Value::Null);
    // Another session (perhaps at another rate) waits for the decks to stop.
    st.decks[1].playing = true;
    e.set_deck_status(st);
    assert!(err(&mut e, "session.new", json!({})).contains("pause the decks first"));
}

#[test]
fn an_mp3s_mixforge_grid_and_cues_move_by_its_priming() {
    let ok = std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success());
    if !ok {
        println!("note: ffmpeg is not on the PATH; this test checks nothing");
        return;
    }
    let dir = Scratch::new("priming");
    let path = dir.0.join("lame.mp3");
    let o = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=f=440:d=80:sample_rate=48000", "-c:a", "libmp3lame", "-b:a", "128k"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let primed = soundcraft_audio_io::mp3gain::gapless_priming(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(primed, (1105, 48_000), "a LAME gapless tag");
    let mut e = engine();
    e.set_analyse(Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(analysis())));
    run(&mut e, "deck.load", json!({"path": path, "deck": "a"}));
    let v = wait(&mut e).remove(0);
    assert!((v["priming_ms"].as_f64().unwrap() - 1105.0 / 48.0).abs() < 1e-6, "{v}");
    let t = e.deck_tracks[0].clone().unwrap();
    assert!((t.grid.as_ref().unwrap().first() - (24_000.0 - 1105.0)).abs() < 1e-6);
    assert!((t.cues[0].unwrap() - (24_000.0 - 1105.0)).abs() < 1e-6, "{:?}", t.cues);
    // A given grid is djforge's own: it stays.
    run(&mut e, "deck.load", json!({"path": path, "deck": "a", "bpm": 128, "first_beat": 0.5}));
    wait(&mut e);
    assert_eq!(e.deck_tracks[0].as_ref().unwrap().grid.as_ref().unwrap().first(), 24_000.0);
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

#[test]
fn hot_cues_the_user_sets_or_clears_are_kept_per_track_and_win_their_slots() {
    let dir = Scratch::new("hotcues");
    let path = wav(&dir.0, "track.wav", 75.0);
    let store = dir.0.join("settings").join("hotcues.json");
    let setup = |e: &mut Engine| {
        e.set_analyse(Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(analysis())));
        e.set_hotcue_store(store.clone());
    };
    let mut e = engine();
    setup(&mut e);
    run(&mut e, "deck.load", json!({"path": path, "deck": "a"}));
    assert_eq!(wait(&mut e)[0]["user_cues"], 0);
    let computed = e.deck_tracks[0].clone().unwrap().cues;
    let mut st = e.deck_status;
    st.decks[0].load = e.deck_loads[0].1.load();
    st.decks[0].hot = computed;
    e.set_deck_status(st);
    assert!(!store.exists(), "the deck reporting its track's own cues keeps nothing");
    // The user sets A at 10 s and F at 5 s, and clears C.
    let sr = f64::from(RATE);
    st.decks[0].hot[0] = Some(10.0 * sr);
    st.decks[0].hot[2] = None;
    st.decks[0].hot[5] = Some(5.0 * sr);
    e.set_deck_status(st);
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    let entry = kept["tracks"].as_object().unwrap().values().next().unwrap().clone();
    assert_eq!(entry["cues"], json!({"1": 10.0, "3": null, "6": 5.0}), "{kept}");
    assert_eq!(entry["path"], json!(path));
    e.set_deck_status(st);
    // Loaded again (another run): the user's cues win their slots, the computed ones fill the rest.
    let mut again = engine();
    setup(&mut again);
    run(&mut again, "deck.load", json!({"path": path, "deck": "b"}));
    assert_eq!(wait(&mut again)[0]["user_cues"], 3);
    let cues = again.deck_tracks[1].clone().unwrap().cues;
    assert_eq!(cues[..6], [Some(10.0 * sr), computed[1], None, computed[3], computed[4], Some(5.0 * sr)]);
    // B moved later: kept beside the others.
    let mut st = again.deck_status;
    st.decks[1].load = again.deck_loads[0].1.load();
    st.decks[1].hot = cues;
    again.set_deck_status(st);
    st.decks[1].hot[1] = Some(20.0 * sr);
    again.set_deck_status(st);
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(kept["tracks"].as_object().unwrap().values().next().unwrap()["cues"], json!({"1": 10.0, "2": 20.0, "3": null, "6": 5.0}));
    // The file changed: it is a new track, and the cues kept for the old one don't apply.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    wav(&dir.0, "track.wav", 74.0);
    run(&mut again, "deck.load", json!({"path": path, "deck": "a"}));
    assert_eq!(wait(&mut again)[0]["user_cues"], 0);
    // A store that isn't JSON is moved aside, not lost, and a new one written.
    std::fs::write(&store, "not json").unwrap();
    let load = again.deck_loads.last().unwrap().1.load();
    let mut st = again.deck_status;
    st.decks[0].load = load;
    st.decks[0].hot = again.deck_tracks[0].clone().unwrap().cues;
    again.set_deck_status(st);
    st.decks[0].hot[7] = Some(sr);
    again.set_deck_status(st);
    assert!(std::fs::read_to_string(&store).unwrap().contains("\"8\": 1.0"));
    assert!(std::fs::read_dir(store.parent().unwrap()).unwrap().flatten().any(|f| f.file_name().to_string_lossy().contains("corrupt")));
}
