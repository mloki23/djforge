//! The Session view's model commands: `live.setup` makes the Live columns, and the slot commands
//! fill, edit and clear clip slots as undoable edits. Slot sources count as used wherever the
//! session lists its sources (Clear, Remove Stems, health, save and reopen).
//!
//! Files live in a temporary folder removed on drop; mixforge's analysis is always a fake.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, wav_float_header};
use soundcraft_engine::Engine;
use soundcraft_engine::model::{ClipId, Quantise, SlotMode, SourceId, StemSet, TrackId, TrackKind};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const RATE: u32 = 48_000;

/// A temporary folder, removed with everything in it when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-live-{name}-{}", std::process::id()));
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

/// A stereo 48 kHz float WAV `name` in `dir`, `secs` long, of a quiet tone.
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

fn run(e: &mut Engine, cmd: &str, p: Value) -> Value {
    e.execute(cmd, &p).unwrap_or_else(|err| panic!("{cmd} {p}: {err}"))
}

fn err(e: &mut Engine, cmd: &str, p: Value) -> String {
    e.execute(cmd, &p).err().unwrap_or_else(|| panic!("{cmd} {p} should be refused")).to_string()
}

/// Timeline sample of session bar `bar` (1-based) plus `beats` beats.
fn at(e: &Engine, bar: i64, beats: f64) -> Samples {
    let s = e.session();
    let tick = s.tempo.bar_start_tick(bar) + (beats * TICKS_PER_QUARTER as f64) as i64;
    s.tempo.tick_to_samples(tick, s.sample_rate)
}

/// An engine with the Live columns set up and a 30 s track at 120 bpm (first downbeat at the file
/// start) loaded beat-locked from bar `bar`, its track selected.
fn deck(dir: &Path, bar: i64) -> (Engine, TrackId, ClipId) {
    let path = wav(dir, "track.wav", 30.0);
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    let r = run(&mut e, "dj.load_warped", json!({"path": path, "bpm": 120, "first_beat": 0.0, "at_bar": bar}));
    let (t, c) = (TrackId(r["track"].as_u64().unwrap()), ClipId(r["clip"].as_u64().unwrap()));
    e.session_mut().edit.selected_tracks = vec![t];
    (e, t, c)
}

fn select(e: &mut Engine, from: Samples, to: Samples) {
    e.session_mut().edit.selection = Range::new(from, to);
}

/// Four stem sources (in the pool) for `clip`, which then plays them with only the vocals on.
fn split(e: &mut Engine, clip: ClipId) -> [SourceId; 4] {
    let s = e.session_mut();
    let stems = [0, 1, 2, 3].map(|i| {
        let buf = AudioBuffer { sample_rate: RATE, channels: vec![vec![0.01; 1_000]; 2] };
        soundcraft_engine::io::add_source(s, &format!("stem{i}.wav"), buf, None, soundcraft_audio_io::FileFormat::Wav)
    });
    let c = s.find_clip_mut(clip).unwrap();
    c.stems = Some(StemSet { key: "k".into(), sources: stems, scale_log2: 1 });
    c.stem_mask = [true, false, false, false];
    stems
}

#[test]
fn setup_makes_four_stereo_live_tracks_once_and_rebinds_a_deleted_one() {
    let mut e = Engine::default();
    assert!(err(&mut e, "live.setup", json!({"columns": 0})).contains("1..=16"));
    assert!(err(&mut e, "live.setup", json!({"columns": 17})).contains("1..=16"));
    let r = run(&mut e, "live.setup", json!({}));
    assert_eq!(r["created"], 4);
    let s = e.session();
    let names: Vec<_> = s.live.columns.iter().map(|c| s.track(c.track).map(|t| (t.name.clone(), t.kind, t.format.label()))).collect();
    let want: Vec<_> = (1..=4).map(|i| Some((format!("Live {i}"), TrackKind::Audio, "Stereo"))).collect();
    assert_eq!(names, want);
    assert_eq!(s.live.quantise, Quantise::Bar(1));

    // Again: nothing to do, and no undo step.
    let undo = e.undo_history().len();
    assert_eq!(run(&mut e, "live.setup", json!({}))["created"], 0);
    assert_eq!(e.undo_history().len(), undo);

    // A deleted column track comes back as a new track, the column keeping its slots.
    let dir = Scratch::new("setup");
    run(&mut e, "live.slot_from_file", json!({"column": 1, "path": wav(&dir.0, "hit.wav", 0.5)}));
    let gone = e.session().live.columns[1].track;
    run(&mut e, "track.delete", json!({"tracks": [gone.0]}));
    // Saved and opened again meanwhile, the column stays (lost, slots and all) and is reported.
    let file = dir.0.join("lost.scraft");
    soundcraft_engine::io::save_session(&mut e, file.to_str().unwrap()).unwrap();
    let mut e = Engine::default();
    let missing = soundcraft_engine::io::open_session(&mut e, file.to_str().unwrap()).unwrap();
    assert!(missing.iter().any(|m| m.contains("Live column 2") && m.contains("live.setup")), "{missing:?}");
    assert_eq!(e.session().live.lost_columns(&e.session().tracks), [1]);
    // A column on a track another column plays into, or on a track that isn't audio, is lost too.
    let aux = TrackId(run(&mut e, "track.new", json!({"kind": "aux"}))["tracks"][0].as_u64().unwrap());
    let s = e.session_mut();
    s.live.columns[3].track = s.live.columns[2].track;
    s.live.columns[0].track = aux;
    assert_eq!(e.session().live.lost_columns(&e.session().tracks), [0, 1, 3]);
    assert_eq!(run(&mut e, "live.setup", json!({"columns": 6}))["created"], 5);
    let s = e.session();
    assert!(s.live.lost_columns(&s.tracks).is_empty());
    assert_eq!(s.live.columns.len(), 6);
    assert!(s.track(s.live.columns[1].track).is_some_and(|t| t.name == "Live 2"));
    assert!(s.live.slot(1, 0).is_some(), "the lost column kept its slot");
    // Undo takes the whole setup back.
    assert!(e.undo());
    assert_eq!(e.session().live.columns.len(), 4);
    assert_eq!(e.session().live.lost_columns(&e.session().tracks), [0, 1, 3]);
}

#[test]
fn slot_commands_refuse_before_the_session_view_is_set_up() {
    let mut e = Engine::default();
    for (cmd, p) in [
        ("live.slot_from_selection", json!({"column": 0})),
        ("live.slot_from_file", json!({"column": 0, "path": "x.wav"})),
        ("live.slot_set", json!({"column": 0, "slot": 0, "name": "x"})),
        ("live.slot_clear", json!({"column": 0, "slot": 0})),
    ] {
        assert!(err(&mut e, cmd, p).contains("View › Session"), "{cmd}");
    }
    // The launch quantise is global and needs no columns.
    assert_eq!(run(&mut e, "live.quantise", json!({"value": "1/2"}))["quantise"], "1/2");
}

#[test]
fn a_selection_becomes_a_loop_of_the_clips_own_bars() {
    let dir = Scratch::new("selection");
    // The clip starts on bar 3, so its track beat 0 plays there: session bar 5 is its beat 8.
    let (mut e, t, clip) = deck(&dir.0, 3);
    let stems = split(&mut e, clip);
    // A selection a little off the bar lines snaps to them.
    let (a, b) = (at(&e, 5, 0.0) + 300, at(&e, 7, 0.0) - 200);
    select(&mut e, a, b);
    let r = run(&mut e, "live.slot_from_selection", json!({"column": 2}));
    assert_eq!(r, json!({"column": 2, "slot": 0, "start_beat": 8.0, "length_beats": 8.0, "bars": 2.0}));
    let s = e.session();
    let c = s.find_clip(clip).unwrap().1;
    let slot = s.live.slot(2, 0).unwrap();
    assert_eq!((slot.source, slot.mode, slot.stems.as_ref().map(|x| x.sources)), (c.source().unwrap(), SlotMode::Loop, Some(stems)));
    assert_eq!((slot.stem_mask, slot.name.as_str(), slot.color), (c.stem_mask, c.name.as_str(), s.track(t).unwrap().color));
    let w = slot.warp.as_ref().unwrap();
    assert_eq!((&w.beats, w.lock), (&c.warp.as_ref().unwrap().beats, None), "same grid, no lock");
    // The next one goes in the next empty slot; a chosen slot is replaced.
    let (a, b) = (at(&e, 3, 0.0), at(&e, 4, 0.0));
    select(&mut e, a, b);
    assert_eq!(run(&mut e, "live.slot_from_selection", json!({"column": 2}))["slot"], 1);
    assert_eq!(run(&mut e, "live.slot_from_selection", json!({"column": 2, "slot": 0}))["start_beat"], 0.0);
    // Each is an undo step.
    assert!(e.undo());
    assert_eq!(e.session().live.slot(2, 0).unwrap().start_beat, 8.0);
    assert!(e.undo());
    assert!(e.undo());
    assert!(e.session().live.slot(2, 0).is_none());
}

#[test]
fn a_selection_under_a_tempo_ramp_counts_the_clips_beats() {
    let dir = Scratch::new("ramp");
    let (mut e, _, _) = deck(&dir.0, 1);
    // 120 → 180 bpm over bars 1..9: by bar 9 a constant 120 bpm would count under 7 bars.
    run(&mut e, "dj.tempo_ramp", json!({"from_bar": 1, "bars": 8, "to_bpm": 180.0}));
    let (a, b) = (at(&e, 5, 0.0), at(&e, 9, 0.0));
    select(&mut e, a, b);
    let r = run(&mut e, "live.slot_from_selection", json!({"column": 0}));
    assert_eq!((r["start_beat"].as_f64(), r["length_beats"].as_f64()), (Some(16.0), Some(16.0)), "{r}");
}

#[test]
fn a_selection_must_be_a_bar_or_more_of_one_beat_locked_clip() {
    let dir = Scratch::new("refusals");
    let (mut e, t, _) = deck(&dir.0, 1);
    let refused = |e: &mut Engine, from: Samples, to: Samples| {
        select(e, from, to);
        err(e, "live.slot_from_selection", json!({"column": 0}))
    };
    let (a, b) = (0, 0);
    assert!(refused(&mut e, a, b).contains("make a time selection"));
    let (a, b) = (at(&e, 2, 0.0), at(&e, 2, 1.0));
    assert!(refused(&mut e, a, b).contains("under 1 bar"));
    // Two clips under the selection.
    run(&mut e, "dj.load_warped", json!({"path": wav(&dir.0, "b.wav", 10.0), "bpm": 120, "at_bar": 20, "track": t.0}));
    let (a, b) = (at(&e, 2, 0.0), at(&e, 22, 0.0));
    assert!(refused(&mut e, a, b).contains("covers 2 clips"));
    // A clip that is not beat-locked.
    let plain = wav(&dir.0, "plain.wav", 10.0);
    let r = run(&mut e, "file.import_audio", json!({"path": plain}));
    let other = TrackId(r["imported"][0]["track"].as_u64().unwrap());
    e.session_mut().edit.selected_tracks = vec![other];
    let (a, b) = (0, at(&e, 3, 0.0));
    assert!(refused(&mut e, a, b).contains("not beat-locked"));
    e.session_mut().edit.selected_tracks = vec![];
    let (a, b) = (0, at(&e, 3, 0.0));
    assert!(refused(&mut e, a, b).contains("no audio clip"));
    // A full column.
    e.session_mut().edit.selected_tracks = vec![t];
    let (a, b) = (0, at(&e, 2, 0.0));
    select(&mut e, a, b);
    for _ in 0..8 {
        run(&mut e, "live.slot_from_selection", json!({"column": 3}));
    }
    assert!(err(&mut e, "live.slot_from_selection", json!({"column": 3})).contains("full"));
    assert!(err(&mut e, "live.slot_from_selection", json!({"column": 4})).contains("`column` must be 0..4"));
    assert!(err(&mut e, "live.slot_from_selection", json!({"column": 0, "slot": 8})).contains("`slot` must be 0..8"));
}

/// A fake mixforge analysis that answers `answer` and records the paths it was asked about.
fn fake_analysis(answer: Result<Value, String>) -> (soundcraft_engine::cmd::PrepAnalyse, Arc<Mutex<Vec<String>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    (
        Arc::new(move |path: &str, _: &dyn Fn() -> bool| {
            log.lock().unwrap().push(path.to_string());
            answer.clone()
        }),
        asked,
    )
}

#[test]
fn a_file_loads_as_a_bar_loop_from_its_grid_else_as_a_one_shot() {
    let dir = Scratch::new("file");
    let path = wav(&dir.0, "loop.wav", 10.0);
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));

    // A given tempo: 10 s at 120 bpm from the file start is 20 beats, five whole bars.
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "bpm": 120}));
    assert_eq!((r["mode"].as_str(), r["grid"].as_str(), r["length_beats"].as_f64()), (Some("loop"), Some("given"), Some(20.0)), "{r}");
    let src = SourceId(r["source"].as_u64().unwrap());
    let s = e.session();
    let source = s.source(src).unwrap();
    // Imported like File › Import › Audio: referenced in place, its origin and stamps recorded.
    assert!(!source.unsaved && source.path == path && source.origin.is_some() && source.origin_len > 0, "{source:?}");
    assert!(s.pool.contains(src));
    assert!(s.tracks.iter().all(|t| t.clips().is_empty()), "a slot places no clip");
    assert_eq!(s.live.slot(0, 0).unwrap().start_beat, 0.0);

    // mixforge's analysis (a fake here): the loop starts on its first downbeat, 0.5 s in.
    let (fake, asked) = fake_analysis(Ok(json!({"bpm": 120.0, "first_downbeat": 0.5})));
    e.set_analyse(fake);
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path}));
    assert_eq!((r["grid"].as_str(), r["length_beats"].as_f64()), (Some("mixforge"), Some(16.0)), "{r}");
    assert_eq!(*asked.lock().unwrap(), std::slice::from_ref(&path));
    let slot = e.session().live.slot(0, 1).unwrap();
    assert_eq!(slot.warp.as_ref().unwrap().beats[0], 24_000.0, "beat 0 is the first downbeat");

    // No usable analysis and no BPM tag: a one-shot at its own rate, which can't loop.
    let (fake, _) = fake_analysis(Err("not analysed".into()));
    e.set_analyse(fake);
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path}));
    assert_eq!((r["mode"].as_str(), &r["grid"]), (Some("one_shot"), &Value::Null), "{r}");
    assert!(e.session().live.slot(0, 2).unwrap().warp.is_none());
    assert!(err(&mut e, "live.slot_set", json!({"column": 0, "slot": 2, "mode": "loop"})).contains("only as a one-shot"));
    let mut plain = Engine::default();
    run(&mut plain, "live.setup", json!({}));
    assert_eq!(run(&mut plain, "live.slot_from_file", json!({"column": 0, "path": path}))["mode"], "one_shot", "no analysis set up");

    // Refusals leave nothing behind.
    let sources = e.session().sources.len();
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 0, "path": dir.0.join("missing.wav")})).contains("missing.wav"));
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "bpm": 1000})).contains("`bpm`"));
    for _ in 3..8 {
        run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "bpm": 120}));
    }
    let sources_full = e.session().sources.len();
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 0, "path": path})).contains("full"));
    assert_eq!(e.session().sources.len(), sources_full, "a full column imports nothing");
    assert_eq!(sources_full, sources + 5);
}

#[test]
fn an_mp3_without_analysis_takes_its_bpm_tag() {
    let dir = Scratch::new("tag");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    let fill = soundcraft_audio_io::mp3tags::TagFill { camelot: None, bpm: Some(124.0), prep_note: "" };
    let (tagged, _) = soundcraft_audio_io::mp3tags::fill(&std::fs::read(fixture).unwrap(), &fill).unwrap();
    let path = dir.0.join("tagged.mp3");
    std::fs::write(&path, tagged).unwrap();
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    let r = run(&mut e, "live.slot_from_file", json!({"column": 1, "path": path}));
    assert_eq!((r["mode"].as_str(), r["grid"].as_str()), (Some("loop"), Some("tag")), "{r}");
    // 124 bpm: a beat every 60/124 s, from the file start.
    let w = e.session().live.slot(1, 0).unwrap().warp.clone().unwrap();
    assert!((w.beats[1] - w.beats[0] - 48_000.0 * 60.0 / 124.0).abs() < 1e-6 && w.beats[0] == 0.0, "{:?}", &w.beats[..2]);
}

#[test]
fn a_slot_is_edited_and_cleared_as_undoable_steps() {
    let dir = Scratch::new("edit");
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    run(&mut e, "live.slot_from_file", json!({"column": 0, "path": wav(&dir.0, "a.wav", 10.0), "bpm": 120}));
    let r = run(
        &mut e,
        "live.slot_set",
        json!({"column": 0, "slot": 0, "name": "Kick loop", "color": [1, 2, 3], "length_bars": 2, "gain_db": -6.0, "mode": "one_shot"}),
    );
    assert_eq!(r, json!({"column": 0, "slot": 0, "name": "Kick loop", "color": [1, 2, 3], "mode": "one_shot", "length_beats": 8.0, "gain_db": -6.0}));
    let undo = e.undo_history().len();
    run(&mut e, "live.slot_set", json!({"column": 0, "slot": 0, "name": "Kick loop"}));
    assert_eq!(e.undo_history().len(), undo, "no change, no undo step");
    for (p, why) in [
        (json!({"column": 0, "slot": 0}), "nothing to set"),
        (json!({"column": 0, "slot": 0, "color": [1, 2, 300]}), "`color`"),
        (json!({"column": 0, "slot": 0, "length_bars": 0}), "`length_bars`"),
        (json!({"column": 0, "slot": 0, "length_bars": 1.5}), "`length_bars`"),
        (json!({"column": 0, "slot": 0, "gain_db": 99}), "`gain_db`"),
        (json!({"column": 0, "slot": 0, "mode": "twice"}), "`mode`"),
        (json!({"column": 0, "slot": 1, "name": "x"}), "empty"),
        (json!({"column": 0, "name": "x"}), "`slot` required"),
    ] {
        assert!(err(&mut e, "live.slot_set", p.clone()).contains(why), "{p}");
    }
    run(&mut e, "live.slot_clear", json!({"column": 0, "slot": 0}));
    assert!(e.session().live.slot(0, 0).is_none());
    assert!(err(&mut e, "live.slot_clear", json!({"column": 0, "slot": 0})).contains("empty"));
    assert!(e.undo());
    assert_eq!(e.session().live.slot(0, 0).unwrap().name, "Kick loop");
    assert!(e.undo());
    let slot = e.session().live.slot(0, 0).unwrap();
    assert_eq!((slot.name.as_str(), slot.mode, slot.length_beats), ("a", SlotMode::Loop, 20.0));
}

#[test]
fn launch_quantise_takes_a_bar_count_or_a_note() {
    let mut e = Engine::default();
    for (v, q) in [(json!("none"), "none"), (json!(0.25), "1/4"), (json!("1/2"), "1/2"), (json!(2), "2"), (json!("8 bars"), "8"), (json!(1), "1")] {
        assert_eq!(run(&mut e, "live.quantise", json!({"value": v}))["quantise"], q, "{v}");
    }
    for v in [json!(3), json!("3/4"), json!(16), json!(null)] {
        assert!(err(&mut e, "live.quantise", json!({"value": v})).contains("`value`"), "{v}");
    }
    run(&mut e, "live.quantise", json!({"value": 4}));
    assert_eq!(e.session().live.quantise, Quantise::Bar(4));
    assert!(e.undo());
    assert_eq!(e.session().live.quantise, Quantise::Bar(1));
}

#[test]
fn slots_and_their_stems_survive_save_and_reopen() {
    let dir = Scratch::new("save");
    let (mut e, _, clip) = deck(&dir.0, 1);
    split(&mut e, clip);
    let (a, b) = (0, at(&e, 5, 0.0));
    select(&mut e, a, b);
    run(&mut e, "live.slot_from_selection", json!({"column": 0, "slot": 3}));
    run(&mut e, "live.slot_from_file", json!({"column": 1, "path": wav(&dir.0, "hit.wav", 0.5)}));
    run(&mut e, "live.quantise", json!({"value": 2}));
    let file = dir.0.join("set").join("set.scraft");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    soundcraft_engine::io::save_session(&mut e, file.to_str().unwrap()).unwrap();
    let mut back = Engine::default();
    let missing = soundcraft_engine::io::open_session(&mut back, file.to_str().unwrap()).unwrap();
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(back.session().live, e.session().live);
    let slot = back.session().live.slot(0, 3).unwrap();
    assert!(slot.audio_sources().all(|x| back.session().pool.contains(x)), "the slot's source and stems load");

    // A stem file gone from disk: the slot plays its own audio, and says so.
    let stem = back.session().source(slot.stems.as_ref().unwrap().sources[2]).unwrap().path.clone();
    std::fs::remove_file(dir.0.join("set").join(stem)).unwrap();
    let mut again = Engine::default();
    let missing = soundcraft_engine::io::open_session(&mut again, file.to_str().unwrap()).unwrap();
    assert!(missing.iter().any(|m| m.contains("Live 1 slot 4") && m.contains("stems missing")), "{missing:?}");
    let slot = again.session().live.slot(0, 3).unwrap();
    assert_eq!((&slot.stems, slot.stem_mask), (&None, [true; 4]));
}

#[test]
fn a_file_a_slot_plays_is_not_cleared_or_swept_with_stems() {
    let dir = Scratch::new("clear");
    let (mut e, _, clip) = deck(&dir.0, 1);
    let stems = split(&mut e, clip);
    let (a, b) = (0, at(&e, 3, 0.0));
    select(&mut e, a, b);
    run(&mut e, "live.slot_from_selection", json!({"column": 0}));
    let own = e.session().find_clip(clip).unwrap().1.source().unwrap();
    // Remove Stems: the clip unsplits, but the slot still plays the stem files, so they stay.
    run(&mut e, "clip.unsplit_stems", json!({"clip": clip.0}));
    assert!(stems.iter().all(|x| e.session().source(*x).is_some() && e.session().pool.contains(*x)));
    // Clearing a file a slot plays is refused, even with its clips, until the slot is cleared.
    for f in [own, stems[1]] {
        let why = err(&mut e, "clip.clear", json!({"sources": [f.0], "with_clips": true}));
        assert!(why.contains("Live 1 slot 1") && why.contains("live.slot_clear"), "{why}");
    }
    assert_eq!(soundcraft_engine::cmd::clear_effect(e.session(), &Default::default(), &[stems[0]].into()).slots, [(0, 0)].into());
    run(&mut e, "live.slot_clear", json!({"column": 0, "slot": 0}));
    let r = run(&mut e, "clip.clear", json!({"sources": [stems[1].0]}));
    assert_eq!(r["sources"], 1);
    run(&mut e, "clip.clear", json!({"sources": [own.0], "with_clips": true}));
    assert!(e.session().source(own).is_none());
}

#[test]
fn health_checks_slot_audio_too() {
    let dir = Scratch::new("health");
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    let r = run(&mut e, "live.slot_from_file", json!({"column": 2, "slot": 5, "path": wav(&dir.0, "a.wav", 1.0)}));
    let healthy = run(&mut e, "session.audio_health", json!({}));
    assert_eq!((healthy["healthy"].as_bool(), healthy["slots"].as_u64()), (Some(true), Some(1)), "{healthy}");
    e.session_mut().pool.remove(SourceId(r["source"].as_u64().unwrap()));
    let sick = run(&mut e, "session.audio_health", json!({}));
    let issue = &sick["issues"][0];
    assert_eq!(
        (issue["code"].as_str(), &issue["slot"]["column"], &issue["slot"]["row"]),
        (Some("unavailable_audio"), &json!(2), &json!(5)),
        "{sick}"
    );
    assert_eq!(issue["track"], "Live 3");
}

#[test]
fn a_selection_snaps_inside_what_the_clip_plays() {
    let dir = Scratch::new("inside");
    // 30.25 s at 120 bpm with the first downbeat 0.5 s in, from bar 3: the clip starts on a bar of
    // lead-in (its beat -4) and its audio ends at beat 59.5, nearer bar line 60 than 56.
    let path = wav(&dir.0, "short.wav", 30.25);
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    let r = run(&mut e, "dj.load_warped", json!({"path": path, "bpm": 120, "first_beat": 0.5, "at_bar": 3}));
    e.session_mut().edit.selected_tracks = vec![TrackId(r["track"].as_u64().unwrap())];
    let clip = e.session().find_clip(ClipId(r["clip"].as_u64().unwrap())).unwrap().1.clone();
    // From the clip start: the lead-in is not part of the loop, which starts on the downbeat.
    let (a, b) = (clip.start, at(&e, 5, 0.0));
    select(&mut e, a, b);
    let r = run(&mut e, "live.slot_from_selection", json!({"column": 0}));
    assert_eq!((r["start_beat"].as_f64(), r["length_beats"].as_f64()), (Some(0.0), Some(4.0)), "{r}");
    // To the clip end: the last bar line the audio reaches, not the one nearest the end.
    let (a, b) = (at(&e, 15, 0.0), clip.end());
    select(&mut e, a, b);
    let r = run(&mut e, "live.slot_from_selection", json!({"column": 0}));
    assert_eq!((r["start_beat"].as_f64(), r["length_beats"].as_f64()), (Some(44.0), Some(12.0)), "{r}");
}

#[test]
fn slot_bars_need_four_four_throughout() {
    let dir = Scratch::new("meter");
    let (mut e, _, _) = deck(&dir.0, 1);
    let path = wav(&dir.0, "a.wav", 4.0);
    run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "bpm": 120}));
    e.session_mut().tempo.set_meter(TICKS_PER_QUARTER * 32, 3, 4).unwrap();
    let (a, b) = (0, at(&e, 3, 0.0));
    select(&mut e, a, b);
    for (cmd, p) in [
        ("live.slot_from_selection", json!({"column": 1})),
        ("live.slot_from_file", json!({"column": 1, "path": path})),
        ("live.slot_set", json!({"column": 0, "slot": 0, "length_bars": 2})),
    ] {
        let why = err(&mut e, cmd, p);
        assert!(why.contains("bar 9 is in 3/4") && why.contains("4/4"), "{cmd}: {why}");
    }
    run(&mut e, "live.slot_set", json!({"column": 0, "slot": 0, "name": "no bars here"}));
}

#[test]
fn a_file_under_a_bar_from_its_downbeat_is_a_one_shot() {
    let dir = Scratch::new("short");
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    // A 0.3 s kick tagged 128 bpm: the fixture's 10-byte ID3 header and 34-byte tag, then about
    // 0.3 s of its 128 kbit/s frames.
    let fixture = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../audio-io/tests/fixtures/mp3/cbr128_44k_stereo.mp3")).unwrap();
    let kick = &fixture[..44 + 4_800];
    let fill = soundcraft_audio_io::mp3tags::TagFill { camelot: None, bpm: Some(128.0), prep_note: "" };
    let (tagged, _) = soundcraft_audio_io::mp3tags::fill(kick, &fill).unwrap();
    let path = dir.0.join("kick.mp3");
    std::fs::write(&path, tagged).unwrap();
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path}));
    assert_eq!((r["mode"].as_str(), &r["grid"]), (Some("one_shot"), &Value::Null), "{r}");
    assert!(r["grid_error"].as_str().unwrap().contains("less than a bar"), "{r}");
    let src = SourceId(r["source"].as_u64().unwrap());
    let frames = e.session().source(src).unwrap().frames;
    assert!((10_000..20_000).contains(&frames), "about 0.3 s: {frames}");
    assert!(e.session().live.slot(0, 0).unwrap().warp.is_none());
    // A first downbeat past the end of a 2 s file.
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": wav(&dir.0, "b.wav", 2.0), "bpm": 120, "first_beat": 5.0}));
    assert_eq!((r["mode"].as_str(), &r["grid"]), (Some("one_shot"), &Value::Null), "{r}");
    assert!(e.session().live.slot(0, 1).unwrap().warp.is_none());
}

#[test]
fn a_file_is_imported_before_it_is_analysed_and_says_why_analysis_gave_no_grid() {
    let dir = Scratch::new("order");
    let mut e = Engine::default();
    assert!(e.analyse().is_none(), "an engine reaches no mixforge until the app sets it up");
    run(&mut e, "live.setup", json!({}));
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": wav(&dir.0, "a.wav", 3.0)}));
    assert_eq!(r["analysis_error"], "no mixforge analysis is set up", "{r}");
    let (fake, asked) = fake_analysis(Err("not analysed by mixforge".into()));
    e.set_analyse(fake);
    // A file that won't decode is refused before mixforge runs, and leaves nothing behind.
    let junk = dir.0.join("junk.wav");
    std::fs::write(&junk, "not audio").unwrap();
    let sources = e.session().sources.len();
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 0, "path": junk})).contains("junk.wav"));
    assert!(asked.lock().unwrap().is_empty());
    assert_eq!(e.session().sources.len(), sources);
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": wav(&dir.0, "b.wav", 3.0)}));
    assert_eq!((r["analysis_error"].as_str(), r["mode"].as_str()), (Some("not analysed by mixforge"), Some("one_shot")), "{r}");
    let (fake, _) = fake_analysis(Ok(json!({"bpm": "fast"})));
    e.set_analyse(fake);
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": wav(&dir.0, "c.wav", 3.0)}));
    assert!(r["analysis_error"].as_str().unwrap().contains("no usable bpm"), "{r}");
    // A given grid needs no analysis; parameters of the wrong type are refused.
    let (fake, asked) = fake_analysis(Ok(json!({})));
    e.set_analyse(fake);
    let path = wav(&dir.0, "d.wav", 3.0);
    let r = run(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "bpm": 120}));
    assert_eq!((&r["analysis_error"], asked.lock().unwrap().len()), (&Value::Null, 0), "{r}");
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "bpm": "120"})).contains("`bpm` must be a number"));
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 0, "path": path, "first_beat": 0.5})).contains("`first_beat` needs a `bpm`"));
}

#[test]
fn a_template_keeps_the_live_columns_but_not_their_slots() {
    let dir = Scratch::new("template");
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    run(&mut e, "live.slot_from_file", json!({"column": 0, "path": wav(&dir.0, "a.wav", 1.0)}));
    let file = dir.0.join("t.scraft");
    run(&mut e, "session.save_template", json!({"path": file}));
    let mut t = Engine::default();
    soundcraft_engine::io::open_session(&mut t, file.to_str().unwrap()).unwrap();
    let live = &t.session().live;
    assert_eq!((live.columns.len(), live.slots().count()), (4, 0));
    assert!(live.lost_columns(&t.session().tracks).is_empty());
}
