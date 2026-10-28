//! Session view launch commands and the boundary helper: where a launch lands for every launch
//! quantise (under a tempo ramp too), what the commands refuse, how a headless engine answers,
//! and what a scene launches. The audio side is in `soundcraft-ui-egui`'s `live_launch` tests.

use serde_json::json;
use soundcraft_engine::cmd::{live_cmds, live_launch_margin, live_next_boundary};
use soundcraft_engine::model::{Quantise, Session, Slot, SourceId};
use soundcraft_engine::{Engine, LiveRequest, LiveStatus, TransportRequest};
use soundcraft_mix::LiveCmd;
use soundcraft_time::{Samples, TICKS_PER_QUARTER};

/// Timeline sample of session beat `beat` (quarter notes from the start).
fn at_beat(s: &Session, beat: f64) -> Samples {
    s.tempo.tick_to_samples((beat * TICKS_PER_QUARTER as f64).round() as i64, s.sample_rate)
}

#[test]
fn the_next_boundary_follows_every_quantise() {
    let s = Session::default();
    // 120 bpm at 48 kHz: a beat every 24 000 samples, a bar every 96 000.
    let from = 100_000;
    for (q, want) in [
        (Quantise::None, 100_000 + 2_400),
        (Quantise::Quarter, 120_000),
        (Quantise::Half, 144_000),
        (Quantise::Bar(1), 192_000),
        (Quantise::Bar(2), 192_000),
        (Quantise::Bar(4), 384_000),
        (Quantise::Bar(8), 768_000),
    ] {
        assert_eq!(live_next_boundary(&s, from, q, 2_400), Ok(want), "{q:?}");
    }
    // A boundary exactly at playhead + margin is taken; one a sample short of it is not.
    assert_eq!(live_next_boundary(&s, 96_000 - 2_400, Quantise::Bar(1), 2_400), Ok(96_000));
    assert_eq!(live_next_boundary(&s, 96_000 - 2_399, Quantise::Bar(1), 2_400), Ok(192_000));
    // The margin is 50 ms, or one audio block when that is longer.
    assert_eq!((live_launch_margin(&s, 512), live_launch_margin(&s, 4_096)), (2_400, 4_096));
}

#[test]
fn the_next_boundary_follows_a_tempo_ramp() {
    let mut s = Session::default();
    // 120 → 180 bpm in a step per beat over bars 1..9, then 180.
    for b in 0..32 {
        s.tempo.set_tempo(b * TICKS_PER_QUARTER, 120.0 + 60.0 * b as f64 / 32.0).unwrap();
    }
    s.tempo.set_tempo(32 * TICKS_PER_QUARTER, 180.0).unwrap();
    for (from_beat, q, want_beat) in [
        (4.5, Quantise::Quarter, 5.0),
        (9.1, Quantise::Bar(1), 12.0),
        (13.0, Quantise::Bar(2), 16.0),
        (17.0, Quantise::Bar(8), 32.0),
        (33.2, Quantise::Half, 34.0),
    ] {
        let got = live_next_boundary(&s, at_beat(&s, from_beat), q, 0).unwrap();
        assert_eq!(got, at_beat(&s, want_beat), "{q:?} from beat {from_beat}");
    }
}

#[test]
fn a_session_not_in_four_four_has_no_boundaries() {
    let mut s = Session::default();
    s.tempo.set_meter(8 * TICKS_PER_QUARTER, 7, 8).unwrap();
    let why = live_next_boundary(&s, 0, Quantise::Bar(1), 0).unwrap_err();
    assert!(why.contains("bar 3 is in 7/8") && why.contains("4/4"), "{why}");
}

/// An engine with Live columns, slots in rows 0 (columns 0 and 2) and 3 (column 1), whose host
/// carries out launches when `audio`.
fn engine(audio: bool) -> Engine {
    let mut e = Engine::default();
    e.execute("live.setup", &json!({})).unwrap();
    let s = e.session_mut();
    for (c, r) in [(0, 0), (2, 0), (1, 3)] {
        s.live.columns[c].slots[r] = Some(Slot::one_shot(format!("s{c}{r}"), [0; 3], SourceId(1)));
    }
    e.live_audio = audio;
    e.transport_requests.clear();
    e
}

fn err(e: &mut Engine, cmd: &str, p: serde_json::Value) -> String {
    e.execute(cmd, &p).err().unwrap_or_else(|| panic!("{cmd} {p} should be refused")).to_string()
}

#[test]
fn launch_commands_queue_a_request_for_the_audio_engine_without_an_undo_step() {
    let mut e = engine(true);
    let (undo, revision) = (e.undo_history().len(), e.revision);
    let r = e.execute("live.launch", &json!({"column": 0, "slot": 0})).unwrap();
    assert_eq!((&r["quantise"], &r["starts_transport"]), (&json!("1"), &json!(true)), "{r}");
    e.execute("live.stop", &json!({"column": 2})).unwrap();
    e.execute("live.stop_all", &json!({})).unwrap();
    e.execute("live.scene", &json!({"row": 3})).unwrap();
    let want = [LiveRequest::Launch { column: 0, slot: 0 }, LiveRequest::Stop { column: 2 }, LiveRequest::StopAll, LiveRequest::Scene { row: 3 }]
        .map(TransportRequest::Live);
    assert_eq!(e.transport_requests, want);
    assert_eq!(e.undo_history().len(), undo, "launching is not an edit");
    assert_eq!(e.revision, revision, "the document is untouched");
    assert_eq!(e.journal.iter().filter(|(id, _)| id.starts_with("live.")).count(), 5, "journaled like transport commands");
}

#[test]
fn launch_commands_refuse_what_cannot_play() {
    let mut e = engine(true);
    for (cmd, p, why) in [
        ("live.launch", json!({"column": 0, "slot": 1}), "is empty"),
        ("live.launch", json!({"column": 0}), "`slot` required"),
        ("live.launch", json!({"column": 9, "slot": 0}), "`column` must be 0..4"),
        ("live.stop", json!({}), "`column` required"),
        ("live.scene", json!({"row": 1}), "no Live column has a slot in row 2"),
        ("live.scene", json!({"row": 8}), "`row` must be 0..8"),
    ] {
        assert!(err(&mut e, cmd, p.clone()).contains(why), "{cmd} {p}");
    }
    // A column that lost its track plays nothing.
    let gone = e.session().live.columns[2].track;
    e.execute("track.delete", &json!({"tracks": [gone.0]})).unwrap();
    assert!(err(&mut e, "live.launch", json!({"column": 2, "slot": 0})).contains("lost its track"));
    // Bars that aren't bars.
    e.session_mut().tempo.set_meter(0, 3, 4).unwrap();
    assert!(err(&mut e, "live.launch", json!({"column": 0, "slot": 0})).contains("4/4"));
    assert!(e.transport_requests.is_empty(), "nothing refused was queued");
}

#[test]
fn a_headless_engine_refuses_launches_but_answers_state() {
    let mut e = engine(false);
    for (cmd, p) in [
        ("live.launch", json!({"column": 0, "slot": 0})),
        ("live.stop", json!({"column": 0})),
        ("live.stop_all", json!({})),
        ("live.scene", json!({"row": 0})),
    ] {
        assert!(err(&mut e, cmd, p).contains("need the app's audio engine"), "{cmd}");
    }
    assert!(e.transport_requests.is_empty());
    let state = e.execute("live.state", &json!({})).unwrap();
    assert_eq!(state["audio"], false);
    assert!(state["columns"].as_array().unwrap().iter().all(|c| c["playing"].is_null() && c["queued"].is_null()), "{state}");
}

#[test]
fn a_scene_launches_its_row_only_where_a_slot_fills_it() {
    let mut e = engine(true);
    let gone = e.session().live.columns[2].track;
    e.execute("track.delete", &json!({"tracks": [gone.0]})).unwrap();
    let s = e.session();
    // Column 2 has a slot in row 0 but lost its track; columns 1 and 3 have none there.
    assert_eq!(live_cmds(s, &LiveRequest::Scene { row: 0 }, 480), [LiveCmd::Launch { column: 0, slot: 0, at: 480 }]);
    assert_eq!(live_cmds(s, &LiveRequest::Scene { row: 3 }, 7), [LiveCmd::Launch { column: 1, slot: 3, at: 7 }]);
    assert!(live_cmds(s, &LiveRequest::Scene { row: 5 }, 7).is_empty());
    assert_eq!(live_cmds(s, &LiveRequest::Stop { column: 1 }, 9), [LiveCmd::Stop { column: 1, at: 9 }]);
    assert_eq!(live_cmds(s, &LiveRequest::StopAll, 9), [LiveCmd::StopAll { at: 9 }]);
}

#[test]
fn state_reports_what_plays_queued_and_where_a_loop_is() {
    let dir = std::env::temp_dir().join(format!("djforge-live-state-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    struct Gone(std::path::PathBuf);
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _gone = Gone(dir.clone());
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 4 * 48_000]; 2] };
    let path = dir.join("loop.wav");
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
    let mut e = engine(true);
    // An 8-beat loop in column 3, row 4.
    e.execute("live.slot_from_file", &json!({"column": 3, "slot": 4, "path": path, "bpm": 120})).unwrap();
    e.execute("live.slot_set", &json!({"column": 3, "slot": 4, "length_bars": 2})).unwrap();
    // As the host reports it: launched at beat 4, now at beat 14.5; column 0 queued to stop.
    e.live_status[3] = LiveStatus { playing: Some((4, 96_000)), queued: None };
    e.live_status[0] = LiveStatus { playing: Some((0, 0)), queued: Some((None, 192_000)) };
    e.transport.playing = true;
    e.transport.position = 348_000;
    let state = e.execute("live.state", &json!({})).unwrap();
    let col = &state["columns"][3];
    assert_eq!(
        (&col["playing"]["slot"], &col["playing"]["start"], &col["playing"]["length_beats"]),
        (&json!(4), &json!(96_000), &json!(8.0)),
        "{state}"
    );
    assert!((col["playing"]["loop_beat"].as_f64().unwrap() - 2.5).abs() < 1e-9, "10.5 beats in, an 8-beat loop: {state}");
    assert_eq!(state["columns"][0]["queued"], json!({"slot": null, "at": 192_000, "stop": true}));
    assert!(state["columns"][0]["playing"]["loop_beat"].is_null(), "a one-shot has no loop position");
    assert_eq!(state["columns"][3]["track"], json!(e.session().live.columns[3].track.0));
}
