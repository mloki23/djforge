//! Recorded MIDI takes: `midi.record_take` turns the live MIDI captured on a track into clips over
//! the time played, a pass each (under a tempo ramp), pairing notes, cutting the ones held at the end, applying
//! Input Quantize, overdubbing with MIDI Merge, refusing, replacing or using new playlists, in one undo step; and
//! `live.slot_from_take` / `live.record_slot` for Session view slots. The capture itself, from a
//! fake MIDI source through the audio thread, is in `soundcraft-playback`'s `tests/take_e2e.rs`.
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
    // On any playlist (a take may be on one no longer active).
    let c = e.session().tracks.iter().flat_map(|t| &t.playlists).flat_map(|p| &p.clips).find(|c| c.id == id).unwrap();
    match &c.content {
        ClipContent::Midi { sequence } => sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect(),
        _ => panic!("not MIDI"),
    }
}

fn clip_count(e: &Engine, t: TrackId) -> usize {
    e.session().track(t).unwrap().clips().len()
}

/// The tick a sample is on, rounded as the takes count it.
fn tick(s: &Session, at: Samples) -> i64 {
    s.tempo.samples_to_ticks_f(at, s.sample_rate).round() as i64
}

#[test]
fn a_take_under_a_tempo_ramp_becomes_a_clip_over_the_time_played_with_the_ticks_played() {
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
    // Exactly where it was played: no rounding out to bar lines.
    let (start, end) = (at_beat(s, 4.1), at_beat(s, 9.3));
    assert_eq!((r["start"].as_i64(), r["end"].as_i64()), (Some(start), Some(end)));
    let c = s.find_clip(ClipId(r["clip"].as_u64().unwrap())).unwrap().1;
    assert_eq!((c.start, c.end()), (start, end));
    let base = s.tempo.samples_to_ticks(start, s.sample_rate);
    let at = |beat: f64| tick(s, at_beat(s, beat)) - base;
    assert_eq!(notes(&e, &r["clip"]), [(60, at(4.25), Q / 2), (64, at(5.5), 3 * Q / 2), (67, at(9.0), Q / 4)]);
    let ClipContent::Midi { sequence } = &c.content else { panic!() };
    let ctrls: Vec<_> = sequence.ctrls.iter().map(|c| (c.tick, c.kind, c.value)).collect();
    assert_eq!(ctrls, [(at(6.0), soundcraft_midi::CtrlKind::Cc(1), 90), (at(6.5), soundcraft_midi::CtrlKind::PitchBend, (0x50 << 7) - 8192)]);
}

#[test]
fn input_quantize_moves_the_notes_onto_the_session_grid_and_the_played_timing_restores() {
    let (mut e, t) = engine();
    run(&mut e, "event.input_quantize", json!({"enabled": true, "grid": "1/16"}));
    let s = e.session();
    // Starting off the grid: the notes still land on the session's sixteenths.
    let p = take(&e, t, 4.1, 6.0, [note(s, 60, 4.3, 4.6), note(s, 62, 5.05, 5.5)].concat());
    let r = run(&mut e, "midi.record_take", p.clone());
    assert_eq!(r["quantized"], true);
    let s = e.session();
    let base = s.tempo.samples_to_ticks(at_beat(s, 4.1), s.sample_rate);
    let starts: Vec<i64> = notes(&e, &r["clip"]).iter().map(|n| n.1 + base).collect();
    assert_eq!(starts, [17 * Q / 4, 5 * Q], "beats 4.25 and 5 of the session");
    run(&mut e, "edit.select", json!({"clips": [r["clip"]]}));
    run(&mut e, "event.restore_performance", json!({}));
    let s = e.session();
    let starts: Vec<i64> = notes(&e, &r["clip"]).iter().map(|n| n.1 + base).collect();
    assert_eq!(starts, [tick(s, at_beat(s, 4.3)), tick(s, at_beat(s, 5.05))]);
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
    assert_eq!(notes(&e, &r["clip"]), [(60, Q, 3 * Q / 2)], "cut at the stop");
    // Its note-off coming after the stop cuts it there too.
    let (mut e, t) = engine();
    let s = e.session();
    let p = take(&e, t, 4.0, 6.5, note(s, 60, 5.0, 7.5).to_vec());
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!((r["held_cut"].as_u64(), notes(&e, &r["clip"])), (Some(1), vec![(60, Q, 3 * Q / 2)]));
}

/// The track's playlists as (name, clip starts), and which is active.
fn playlists(e: &Engine, t: TrackId) -> (Vec<(String, Vec<Samples>)>, usize) {
    let tr = e.session().track(t).unwrap();
    (tr.playlists.iter().map(|p| (p.name.clone(), p.clips.iter().map(|c| c.start).collect())).collect(), tr.active_playlist)
}

#[test]
fn a_take_over_a_clip_is_refused_goes_on_a_new_playlist_replaces_or_overdubs() {
    let (mut e, t) = engine();
    let s = e.session();
    let first = take(&e, t, 4.0, 8.0, note(s, 60, 4.0, 5.0).to_vec());
    let r1 = run(&mut e, "midi.record_take", first);
    let s = e.session();
    let second = take(&e, t, 6.0, 10.0, note(s, 64, 9.0, 9.5).to_vec());
    let err = e.execute("midi.record_take", &second).unwrap_err();
    assert!(matches!(err, EngineError::WouldReplace { .. }), "{err}");
    assert!(err.to_string().contains("new_playlist") && err.to_string().contains("MIDI Merge"), "{err}");
    assert_eq!(clip_count(&e, t), 1, "nothing changed");
    // A new playlist: the take there, active; the first clip intact on the playlist before.
    let mut p = second.clone();
    p["on_overlap"] = json!("new_playlist");
    let r = run(&mut e, "midi.record_take", p);
    let s = e.session();
    assert_eq!(r["playlist"], "Lead.02");
    assert!(r["message"].as_str().unwrap().contains("new playlist `Lead.02`"), "{r}");
    // The new playlist is the old one with the take placed on it: the first clip plays on up to it.
    assert_eq!(playlists(&e, t), (vec![("Lead".into(), vec![at_beat(s, 4.0)]), ("Lead.02".into(), vec![at_beat(s, 4.0), at_beat(s, 6.0)])], 1));
    e.undo();
    assert_eq!(playlists(&e, t).0.len(), 1, "undo takes the playlist too");
    // MIDI Merge: the take goes into the clip, which grows to cover it.
    run(&mut e, "options.midi_merge", json!({"value": true}));
    let r = run(&mut e, "midi.record_take", second.clone());
    assert_eq!((r["merged"].as_bool(), r["clip"].clone()), (Some(true), r1["clip"].clone()));
    assert_eq!(clip_count(&e, t), 1);
    let s = e.session();
    assert_eq!((r["start"].as_i64(), r["end"].as_i64()), (Some(at_beat(s, 4.0)), Some(at_beat(s, 10.0))));
    let shift = tick(s, at_beat(s, 9.0)) - s.tempo.samples_to_ticks(at_beat(s, 4.0), s.sample_rate);
    assert_eq!(notes(&e, &r1["clip"]), [(60, 0, Q), (64, shift, Q / 2)]);
    e.undo();
    // Replace: only what lies under the take's own time goes.
    run(&mut e, "options.midi_merge", json!({"value": false}));
    let mut p = second;
    p["replace"] = json!(true);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["merged"], false);
    let s = e.session();
    let starts: Vec<(Samples, Samples)> = s.track(t).unwrap().clips().iter().map(|c| (c.start, c.end())).collect();
    assert_eq!(starts, [(at_beat(s, 4.0), at_beat(s, 6.0)), (at_beat(s, 6.0), at_beat(s, 10.0))], "the first clip trimmed back to the take");
}

#[test]
fn a_take_next_to_a_clip_in_the_same_bar_is_neither_refused_nor_clears_it() {
    let (mut e, t) = engine();
    let s = e.session();
    let p = take(&e, t, 4.0, 5.0, note(s, 60, 4.0, 4.5).to_vec());
    run(&mut e, "midi.record_take", p);
    let s = e.session();
    // Bar 2 again, from beat 6: no overlap with the clip that ends at beat 5.
    let p = take(&e, t, 6.0, 7.5, note(s, 62, 6.5, 7.0).to_vec());
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["new_playlist"], false);
    let s = e.session();
    let spans: Vec<(Samples, Samples)> = s.track(t).unwrap().clips().iter().map(|c| (c.start, c.end())).collect();
    assert_eq!(spans, [(at_beat(s, 4.0), at_beat(s, 5.0)), (at_beat(s, 6.0), at_beat(s, 7.5))]);
}

/// Pass `k` of a loop over beats 4..8: a note in each, at a different beat.
fn loop_pass(s: &Session, pitch: u8, beat: f64, held: bool) -> Value {
    let mut ev = note(s, pitch, beat, beat + 0.5).to_vec();
    if held {
        // Held across the wrap: cut at the pass's end, its note-off in the next pass ignored.
        ev = vec![json!({"at": at_beat(s, beat), "status": 0x90, "data1": pitch, "data2": 100})];
    }
    json!({"start": at_beat(s, 4.0), "end": at_beat(s, 8.0), "events": ev})
}

#[test]
fn loop_passes_go_on_playlists_one_each_or_merge_into_one_clip() {
    let (mut e, t) = engine();
    let s = e.session();
    let mut late_off = loop_pass(s, 64, 5.0, false);
    // The held note's note-off comes in the third pass: not paired with anything there.
    late_off["events"].as_array_mut().unwrap().insert(0, json!({"at": at_beat(s, 4.5), "status": 0x80, "data1": 62, "data2": 0}));
    let passes = vec![loop_pass(s, 60, 4.5, false), loop_pass(s, 62, 7.5, true), late_off];
    let p = json!({"track": t.0, "passes": passes.clone(), "on_overlap": "new_playlist"});
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["takes"].as_array().unwrap().len(), 3);
    let s = e.session();
    let at4 = at_beat(s, 4.0);
    assert_eq!(
        playlists(&e, t),
        (vec![("Lead".into(), vec![at4]), ("Lead.02".into(), vec![at4]), ("Lead.03".into(), vec![at4])], 2),
        "one take per pass, as audio loop passes"
    );
    let pitches: Vec<Vec<(u8, i64)>> =
        r["takes"].as_array().unwrap().iter().map(|x| notes(&e, &x["clip"]).iter().map(|n| (n.0, n.2)).collect()).collect();
    let held = tick(s, at_beat(s, 8.0)) - tick(s, at_beat(s, 7.5));
    assert_eq!(pitches, [vec![(60, Q / 2)], vec![(62, held)], vec![(64, Q / 2)]], "notes pair within their own pass");
    assert_eq!(r["held_cut"], 1);
    assert_eq!(e.undo_label(), Some("Record MIDI"), "one undo step");
    e.undo();
    // MIDI Merge: every pass into the clip.
    run(&mut e, "options.midi_merge", json!({"value": true}));
    let r = run(&mut e, "midi.record_take", json!({"track": t.0, "passes": passes}));
    assert_eq!((clip_count(&e, t), playlists(&e, t).0.len()), (1, 1));
    let got: Vec<u8> = notes(&e, &r["clip"]).iter().map(|n| n.0).collect();
    assert_eq!(got, [60, 64, 62]);
}

#[test]
fn a_seek_during_a_take_records_each_stretch_where_it_was_played() {
    let (mut e, t) = engine();
    let s = e.session();
    // Played from beat 4, then a locate to beat 20 and on from there.
    let passes = vec![
        json!({"start": at_beat(s, 4.0), "end": at_beat(s, 6.0), "events": note(s, 60, 5.0, 5.5).to_vec()}),
        json!({"start": at_beat(s, 20.0), "end": at_beat(s, 22.0), "events": note(s, 62, 21.0, 21.5).to_vec()}),
    ];
    let r = run(&mut e, "midi.record_take", json!({"track": t.0, "passes": passes, "on_overlap": "new_playlist"}));
    assert_eq!(r["new_playlist"], false, "nothing overlaps");
    let s = e.session();
    let spans: Vec<(Samples, Samples)> = s.track(t).unwrap().clips().iter().map(|c| (c.start, c.end())).collect();
    assert_eq!(spans, [(at_beat(s, 4.0), at_beat(s, 6.0)), (at_beat(s, 20.0), at_beat(s, 22.0))]);
    assert_eq!(r["notes"], 2);
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
fn a_take_of_controllers_is_kept_and_only_an_empty_one_creates_nothing() {
    let (mut e, t) = engine();
    let s = e.session();
    let ev = vec![json!({"at": at_beat(s, 5.0), "status": 0xB0, "data1": 64, "data2": 127})];
    let p = take(&e, t, 4.0, 8.0, ev);
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!((r["notes"].as_u64(), r["controllers"].as_u64(), clip_count(&e, t)), (Some(0), Some(1), 1), "a pedal or filter sweep is playing too");
    let steps = e.undo_history().len();
    let p = take(&e, t, 10.0, 12.0, Vec::new());
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["clip"], Value::Null);
    assert!(r["message"].as_str().unwrap().contains("nothing was played"));
    assert!(e.messages.last().unwrap().contains("Nothing recorded"));
    assert_eq!((clip_count(&e, t), e.undo_history().len()), (1, steps));
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
        (json!({"track": t.0, "start": 0, "end": 5, "on_overlap": "keep"}), "on_overlap"),
        (json!({"track": t.0, "passes": []}), "`passes`"),
        (json!({"track": t.0, "passes": [{"start": 0}]}), "pass 0"),
        (json!({"track": audio.0, "start": 0, "end": 5}), "not an instrument"),
    ] {
        let err = e.execute("midi.record_take", &p).unwrap_err().to_string();
        assert!(err.contains(why), "{p}: {err}");
    }
}

#[test]
fn an_undo_group_makes_one_step_whatever_merges_are_held() {
    let (mut e, t) = engine();
    let steps = e.undo_history().len();
    e.hold_merge(Some("automation_pass"));
    e.begin_undo_group("Record Slot Take");
    let s = e.session();
    let p = take(&e, t, 4.0, 5.0, note(s, 60, 4.0, 4.5).to_vec());
    e.execute_merged("midi.record_take", &p, "a").unwrap();
    let s = e.session();
    let p = take(&e, t, 6.0, 7.0, note(s, 62, 6.0, 6.5).to_vec());
    e.execute_merged("midi.record_take", &p, "b").unwrap();
    assert_eq!(e.undo_history().len(), steps, "nothing pushed while the group is open");
    e.commit_undo_group();
    assert_eq!((e.undo_history().len(), e.undo_label()), (steps + 1, Some("Record Slot Take")));
    // A merged gesture after the commit (the automation pass still held) is a step of its own.
    let s = e.session();
    let p = take(&e, t, 8.0, 9.0, note(s, 64, 8.0, 8.5).to_vec());
    e.execute_merged("midi.record_take", &p, "c").unwrap();
    assert_eq!(e.undo_history().len(), steps + 2, "the group's step is not merged into");
    e.hold_merge(None);
    assert!(e.undo());
    assert_eq!(clip_count(&e, t), 2, "only the later write undone");
    assert!(e.undo());
    assert_eq!(clip_count(&e, t), 0, "both writes undone together");
    // A group opened inside an open one joins it; the outermost commit closes it.
    let steps = e.undo_history().len();
    e.begin_undo_group("Outer");
    e.begin_undo_group("Inner");
    let s = e.session();
    let p = take(&e, t, 4.0, 5.0, note(s, 60, 4.0, 4.5).to_vec());
    e.execute("midi.record_take", &p).unwrap();
    e.commit_undo_group();
    assert!(e.in_undo_group(), "the inner commit leaves the outer group open");
    e.commit_undo_group();
    assert_eq!((e.undo_history().len(), e.undo_label(), e.in_undo_group()), (steps + 1, Some("Outer"), false));
    // A group in which nothing changed pushes nothing.
    let steps = e.undo_history().len();
    e.begin_undo_group("Nothing");
    e.commit_undo_group();
    assert_eq!(e.undo_history().len(), steps);
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

/// A punch-in over one clip of a long track, with the clips under it kept on a new playlist: the
/// new active playlist is the old one with the take placed on it (only its own time cleared), so
/// the rest of the track plays on; the old playlist is intact.
#[test]
fn a_take_on_a_new_playlist_keeps_the_rest_of_the_track_playing() {
    let (mut e, t) = engine();
    for (from, to, pitch) in [(0.0, 2.0, 50), (8.0, 12.0, 52), (20.0, 22.0, 54)] {
        let s = e.session();
        let p = take(&e, t, from, to, note(s, pitch, from, from + 0.5).to_vec());
        run(&mut e, "midi.record_take", p);
    }
    // The middle clip's played timing was kept (it was quantized).
    let middle = e.session().track(t).unwrap().clips()[1].id;
    e.session_mut().midi_originals.insert(middle.0, soundcraft_midi::Sequence::default());
    let before = e.session().track(t).unwrap().playlists[0].clone();
    let s = e.session();
    let p = json!({"track": t.0, "start": at_beat(s, 9.0), "end": at_beat(s, 10.0), "events": note(s, 60, 9.0, 9.5).to_vec(),
        "on_overlap": "new_playlist"});
    let r = run(&mut e, "midi.record_take", p);
    assert_eq!(r["new_playlist"], true);
    let s = e.session();
    let tr = s.track(t).unwrap();
    assert_eq!((tr.playlists.len(), tr.active_playlist), (2, 1));
    assert_eq!(tr.playlists[0], before, "the playlist before is intact");
    let spans: Vec<(Samples, Samples)> = tr.clips().iter().map(|c| (c.start, c.end())).collect();
    let b = |x: f64| at_beat(s, x);
    assert_eq!(spans, [(b(0.0), b(2.0)), (b(8.0), b(9.0)), (b(9.0), b(10.0)), (b(10.0), b(12.0)), (b(20.0), b(22.0))]);
    let old: Vec<u64> = before.clips.iter().map(|c| c.id.0).collect();
    assert!(tr.clips().iter().all(|c| !old.contains(&c.id.0)), "the copies have their own ids");
    let copy = tr.clips().iter().find(|c| c.start == b(8.0)).unwrap().id;
    assert!(s.midi_originals.contains_key(&copy.0), "a copy keeps its played timing");
    // Forced (recovering a take), even over nothing: the track still plays on.
    let p = json!({"track": t.0, "start": at_beat(s, 30.0), "end": at_beat(s, 31.0), "events": note(s, 62, 30.0, 30.5).to_vec(),
        "new_playlist": true});
    run(&mut e, "midi.record_take", p);
    let tr = e.session().track(t).unwrap();
    assert_eq!((tr.playlists.len(), tr.active_playlist, tr.clips().len()), (3, 2, 6));
}

#[test]
fn a_take_of_too_many_events_over_its_passes_is_refused() {
    let (mut e, t) = engine();
    let ev: Vec<Value> = (0..600_000).map(|i| json!({"at": i, "status": 0xB0, "data1": 1, "data2": 0})).collect();
    let pass = json!({"start": 0, "end": 1_000_000, "events": ev});
    let err = e.execute("midi.record_take", &json!({"track": t.0, "passes": [pass.clone(), pass]})).unwrap_err().to_string();
    assert!(err.contains("1200000 events") && err.contains("at most 1000000"), "{err}");
}
