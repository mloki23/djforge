//! Recorded MIDI takes: `midi.record_take` turns the live MIDI captured on a track into a clip on
//! bar lines (under a tempo ramp), pairing notes, cutting the ones held at the end, applying
//! Input Quantize, overdubbing with MIDI Merge or refusing to replace, in one undo step; and
//! `live.slot_from_take` / `live.record_slot` for Session view slots. The capture itself, from a
//! fake MIDI source through the audio thread, is in `soundcraft-playback`'s `take_tests`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use soundcraft_engine::model::{ClipContent, ClipId, Session, SlotContent, TrackId};
use soundcraft_engine::{Engine, EngineError, LiveRequest, TransportRequest};
use soundcraft_time::{Samples, TICKS_PER_QUARTER};

const Q: i64 = TICKS_PER_QUARTER;

fn run(e: &mut Engine, cmd: &str, p: Value) -> Value {
    e.execute(cmd, &p).unwrap_or_else(|err| panic!("{cmd}: {err}"))
}

/// Timeline sample of session beat `beat`.
fn at_beat(s: &Session, beat: f64) -> Samples {
    s.tempo.tick_to_samples((beat * Q as f64).round() as i64, s.sample_rate)
}

/// An instrument track under a tempo ramp (120 → 150 bpm a step per beat over 8 bars, then 150).
fn engine() -> (Engine, TrackId) {
    let mut e = Engine::default();
    let r = run(&mut e, "track.new", json!({"kind": "instrument", "name": "Lead"}));
    let t = TrackId(r["tracks"][0].as_u64().or_else(|| r["track"].as_u64()).unwrap_or_else(|| e.session().tracks[0].id.0));
    let s = e.session_mut();
    for b in 0..32 {
        s.tempo.set_tempo(b * Q, 120.0 + 30.0 * b as f64 / 32.0).unwrap();
    }
    s.tempo.set_tempo(32 * Q, 150.0).unwrap();
    (e, t)
}

/// A note-on and its note-off at beats `on` and `off`.
fn note(s: &Session, pitch: u8, on: f64, off: f64) -> [Value; 2] {
    [
        json!({"at": at_beat(s, on), "status": 0x90, "data1": pitch, "data2": 100}),
        json!({"at": at_beat(s, off), "status": 0x80, "data1": pitch, "data2": 0}),
    ]
}

fn take(e: &Engine, t: TrackId, from: f64, to: f64, events: Vec<Value>) -> Value {
    let s = e.session();
    json!({"track": t.0, "start": at_beat(s, from), "end": at_beat(s, to), "events": events})
}

/// The clip's notes as (pitch, start tick, length).
fn notes(e: &Engine, clip: &Value) -> Vec<(u8, i64, i64)> {
    let id = ClipId(clip.as_u64().unwrap());
    match &e.session().find_clip(id).unwrap().1.content {
        ClipContent::Midi { sequence } => sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect(),
        _ => panic!("not MIDI"),
    }
}

fn clip_count(e: &Engine, t: TrackId) -> usize {
    e.session().track(t).unwrap().clips().len()
}

#[test]
fn a_take_under_a_tempo_ramp_becomes_a_clip_on_bar_lines_with_the_ticks_played() {
    let (mut e, t) = engine();
    let s = e.session();
    let mut ev: Vec<Value> = Vec::new();
    ev.extend(note(s, 60, 4.25, 4.75));
    ev.extend(note(s, 64, 5.5, 7.0));
    ev.push(json!({"at": at_beat(s, 6.0), "status": 0xB0, "data1": 1, "data2": 90}));
    ev.push(json!({"at": at_beat(s, 6.5), "status": 0xE0, "data1": 0, "data2": 0x50}));
    ev.extend(note(s, 67, 9.0, 9.25));
    let p = take(&e, t, 4.1, 9.3, ev);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!((r["notes"].as_u64(), r["controllers"].as_u64(), r["merged"].as_bool()), (Some(3), Some(2), Some(false)));
    let s = e.session();
    // Bar 2 (beat 4) to the bar line after beat 9.3 (beat 12).
    assert_eq!((r["start"].as_i64(), r["end"].as_i64()), (Some(at_beat(s, 4.0)), Some(at_beat(s, 12.0))));
    let c = s.find_clip(ClipId(r["clip"].as_u64().unwrap())).unwrap().1;
    assert_eq!((c.start, c.end()), (at_beat(s, 4.0), at_beat(s, 12.0)));
    assert_eq!(notes(&e, &r["clip"]), [(60, Q / 4, Q / 2), (64, 3 * Q / 2, 3 * Q / 2), (67, 5 * Q, Q / 4)]);
    let ClipContent::Midi { sequence } = &c.content else { panic!() };
    let ctrls: Vec<_> = sequence.ctrls.iter().map(|c| (c.tick, c.kind, c.value)).collect();
    assert_eq!(ctrls, [(2 * Q, soundcraft_midi::CtrlKind::Cc(1), 90), (5 * Q / 2, soundcraft_midi::CtrlKind::PitchBend, (0x50 << 7) - 8192)]);
}

#[test]
fn input_quantize_moves_the_notes_and_the_played_timing_restores() {
    let (mut e, t) = engine();
    run(&mut e, "event.input_quantize", json!({"enabled": true, "grid": "1/16"}));
    let s = e.session();
    let p = take(&e, t, 4.0, 6.0, [note(s, 60, 4.3, 4.6), note(s, 62, 5.05, 5.5)].concat());
    let r = run(&mut e, "midi.record_take", p.clone());
    assert_eq!(r["quantized"], true);
    let starts: Vec<i64> = notes(&e, &r["clip"]).iter().map(|n| n.1).collect();
    assert_eq!(starts, [Q / 4, Q]);
    run(&mut e, "edit.select", json!({"clips": [r["clip"]]}));
    run(&mut e, "event.restore_performance", json!({}));
    let starts: Vec<i64> = notes(&e, &r["clip"]).iter().map(|n| n.1).collect();
    assert_eq!(starts, [(0.3 * Q as f64).round() as i64, (1.05 * Q as f64).round() as i64]);
    // `quantize: false` keeps the playing as it was, whatever the setting.
    let (mut e, t) = engine();
    run(&mut e, "event.input_quantize", json!({"enabled": true, "grid": "1/16"}));
    let s = e.session();
    let mut p = take(&e, t, 4.0, 6.0, note(s, 60, 4.3, 4.6).to_vec());
    p["quantize"] = json!(false);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!((r["quantized"].as_bool(), notes(&e, &r["clip"])[0].1), (Some(false), (0.3 * Q as f64).round() as i64));
}

#[test]
fn a_note_held_at_stop_is_cut_at_the_end() {
    let (mut e, t) = engine();
    let s = e.session();
    let ev = vec![json!({"at": at_beat(s, 5.0), "status": 0x90, "data1": 60, "data2": 100})];
    let p = take(&e, t, 4.0, 6.5, ev);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["held_cut"], 1);
    assert_eq!(notes(&e, &r["clip"]), [(60, Q, 3 * Q / 2)], "cut at the stop, not at the bar line");
    // Its note-off coming after the stop cuts it there too.
    let (mut e, t) = engine();
    let s = e.session();
    let p = take(&e, t, 4.0, 6.5, note(s, 60, 5.0, 7.5).to_vec());
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!((r["held_cut"].as_u64(), notes(&e, &r["clip"])), (Some(1), vec![(60, Q, 3 * Q / 2)]));
}

#[test]
fn a_take_over_a_clip_is_refused_replaces_on_request_and_overdubs_with_midi_merge() {
    let (mut e, t) = engine();
    let s = e.session();
    let first = take(&e, t, 4.0, 8.0, note(s, 60, 4.0, 5.0).to_vec());
    let r1 = run(&mut e, "midi.record_take", first);
    let s = e.session();
    let second = take(&e, t, 6.0, 10.0, note(s, 64, 9.0, 9.5).to_vec());
    let err = e.execute("midi.record_take", &second).unwrap_err();
    assert!(matches!(err, EngineError::WouldReplace { .. }), "{err}");
    assert!(err.to_string().contains("replace: true") && err.to_string().contains("MIDI Merge"), "{err}");
    assert_eq!(clip_count(&e, t), 1, "nothing changed");
    // MIDI Merge: the take goes into the clip, which grows to its bars.
    run(&mut e, "options.midi_merge", json!({"value": true}));
    let r = run(&mut e, "midi.record_take", second.clone());
    assert_eq!((r["merged"].as_bool(), r["clip"].clone()), (Some(true), r1["clip"].clone()));
    assert_eq!(clip_count(&e, t), 1);
    let s = e.session();
    assert_eq!((r["start"].as_i64(), r["end"].as_i64()), (Some(at_beat(s, 4.0)), Some(at_beat(s, 12.0))));
    assert_eq!(notes(&e, &r1["clip"]), [(60, 0, Q), (64, 5 * Q, Q / 2)]);
    e.undo();
    // Replace: what was under the take goes.
    run(&mut e, "options.midi_merge", json!({"value": false}));
    let mut p = second;
    p["replace"] = json!(true);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["merged"], false);
    let s = e.session();
    let ids: Vec<u64> = s.track(t).unwrap().clips().iter().map(|c| c.id.0).collect();
    assert_eq!(ids, [r["clip"].as_u64().unwrap()], "the take's bars (2 and 3) covered the first clip");
    assert_eq!(notes(&e, &r["clip"]), [(64, 5 * Q, Q / 2)]);
}

#[test]
fn undo_removes_the_take_in_one_step() {
    let (mut e, t) = engine();
    let s = e.session();
    let steps = e.undo_history().len();
    let p = take(&e, t, 4.0, 8.0, [note(s, 60, 4.0, 5.0), note(s, 62, 5.0, 6.0)].concat());
    run(&mut e, "midi.record_take", p);
    assert_eq!((e.undo_history().len(), e.undo_label()), (steps + 1, Some("Record MIDI")));
    assert_eq!(e.journal.last().map(|j| j.0.as_str()), Some("midi.record_take"));
    assert!(e.undo());
    assert_eq!(clip_count(&e, t), 0);
}

#[test]
fn an_empty_take_creates_nothing_and_says_so() {
    let (mut e, t) = engine();
    let s = e.session();
    let steps = e.undo_history().len();
    let ev = vec![json!({"at": at_beat(s, 5.0), "status": 0xB0, "data1": 64, "data2": 127})];
    let p = take(&e, t, 4.0, 8.0, ev);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!((r["clip"].clone(), r["notes"].as_u64(), r["controllers"].as_u64()), (Value::Null, Some(0), Some(1)));
    assert!(r["message"].as_str().unwrap().contains("no notes"));
    assert!(e.messages.last().unwrap().contains("Nothing recorded"));
    assert_eq!((clip_count(&e, t), e.undo_history().len()), (0, steps));
    let p = take(&e, t, 4.0, 8.0, Vec::new());
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["clip"], Value::Null);
}

#[test]
fn bad_takes_are_refused_with_the_reason() {
    let (mut e, t) = engine();
    let audio = TrackId(run(&mut e, "track.new", json!({"kind": "audio"}))["tracks"][0].as_u64().unwrap_or(0));
    for (p, why) in [
        (json!({"track": t.0, "start": 10, "end": 5}), "after `start`"),
        (json!({"track": t.0, "end": 5}), "`start` and `end`"),
        (json!({"track": t.0, "start": 0, "end": 5, "events": [{"at": 1, "status": 0xF8}]}), "not a channel message"),
        (json!({"track": t.0, "start": 0, "end": 5, "events": [{"status": 0x90}]}), "`at`"),
        (json!({"track": t.0, "start": 0, "end": 5, "events": [{"at": 1, "status": 0x90, "data1": 200}]}), "MIDI byte"),
        (json!({"track": audio.0, "start": 0, "end": 5}), "not an instrument"),
    ] {
        let err = e.execute("midi.record_take", &p).unwrap_err().to_string();
        assert!(err.contains(why), "{p}: {err}");
    }
}

/// Four audio columns and an instrument column (4), with a host that launches.
fn live_engine() -> Engine {
    let (mut e, _) = engine();
    run(&mut e, "live.setup", json!({}));
    run(&mut e, "live.add_column", json!({"kind": "instrument"}));
    e.live_audio = true;
    e.transport_requests.clear();
    e
}

fn slot_notes(e: &Engine, column: usize, slot: usize) -> (Vec<(u8, i64, i64)>, f64) {
    match e.session().live.slot(column, slot) {
        Some(SlotContent::Midi(m)) => (m.sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect(), m.length_beats),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_slot_take_counts_ticks_from_its_boundary_for_its_bars() {
    let mut e = live_engine();
    let s = e.session();
    // Recorded from bar 3 (beat 8) for two bars: a note on the boundary, one held past the end.
    let ev: Vec<Value> = [note(s, 60, 8.0, 8.5), note(s, 64, 10.25, 11.0), note(s, 67, 15.5, 17.0)].concat();
    let p = json!({"column": 4, "slot": 2, "start": at_beat(s, 8.0), "bars": 2, "events": ev});
    let r = run(&mut e, "live.slot_from_take", p.clone());
    assert_eq!((r["notes"].as_u64(), r["length_beats"].as_f64(), r["held_cut"].as_u64()), (Some(3), Some(8.0), Some(1)));
    assert_eq!(r["end"].as_i64(), Some(at_beat(e.session(), 16.0)));
    assert_eq!(slot_notes(&e, 4, 2), (vec![(60, 0, Q / 2), (64, 9 * Q / 4, 3 * Q / 4), (67, 15 * Q / 2, Q / 2)], 8.0));
    assert_eq!(e.undo_label(), Some("Record Slot Take"));
    // Filled now: refused unless `replace`.
    let err = e.execute("live.slot_from_take", &p).unwrap_err();
    assert!(matches!(err, EngineError::WouldReplace { .. }), "{err}");
    let err = e.execute("live.record_slot", &json!({"column": 4, "slot": 2, "bars": 2})).unwrap_err();
    assert!(matches!(err, EngineError::WouldReplace { .. }), "{err}");
    // Audio columns take no MIDI.
    for cmd in ["live.slot_from_take", "live.record_slot"] {
        let err = e.execute(cmd, &json!({"column": 0, "slot": 0, "start": 0, "bars": 1})).unwrap_err().to_string();
        assert!(err.contains("audio column"), "{cmd}: {err}");
    }
    assert!(e.undo());
    assert!(e.session().live.slot(4, 2).is_none(), "undo empties it");
}

#[test]
fn record_slot_arms_through_the_host_and_quantize_wraps_onto_the_downbeat() {
    let mut e = live_engine();
    let r = run(&mut e, "live.record_slot", json!({"column": 4, "slot": 0, "bars": 1}));
    assert_eq!((r["bars"].as_u64(), r["slot"].as_u64()), (Some(1), Some(0)));
    assert_eq!(e.transport_requests, [TransportRequest::Live(LiveRequest::RecordSlot { column: 4, slot: 0, bars: 1, replace: false })]);
    assert!(e.session().live.slot(4, 0).is_none(), "arming writes nothing");
    for bad in [json!({"column": 4, "slot": 0, "bars": 0}), json!({"column": 4, "bars": 1}), json!({"column": 4, "slot": 0, "bars": 300})] {
        assert!(e.execute("live.record_slot", &bad).is_err(), "{bad}");
    }
    e.live_audio = false;
    let err = e.execute("live.record_slot", &json!({"column": 4, "slot": 0, "bars": 1})).unwrap_err().to_string();
    assert!(err.contains("audio engine"), "{err}");
    // A note played just before the loop comes round lands on its downbeat when quantized.
    run(&mut e, "event.input_quantize", json!({"enabled": true, "grid": "1/16"}));
    let s = e.session();
    let ev: Vec<Value> = [note(s, 60, 8.5, 9.0), note(s, 62, 11.98, 12.4)].concat();
    let p = json!({"column": 4, "slot": 0, "start": at_beat(s, 8.0), "bars": 1, "events": ev});
    let r = run(&mut e, "live.slot_from_take", p);
    assert_eq!(r["quantized"], true);
    let (n, _) = slot_notes(&e, 4, 0);
    assert_eq!(n.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(), [(62, 0), (60, Q / 2)]);
    // An empty slot take writes nothing.
    let s = e.session();
    let p = json!({"column": 4, "slot": 1, "start": at_beat(s, 8.0), "bars": 1, "events": []});
    let r = run(&mut e, "live.slot_from_take", p);
    assert_eq!(r["notes"], 0);
    assert!(e.session().live.slot(4, 1).is_none());
}
