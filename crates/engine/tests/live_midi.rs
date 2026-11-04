//! Instrument columns and MIDI slots: `live.add_column` makes either kind of column,
//! `live.slot_from_clip` and `prod.pattern {slot}` fill MIDI slots, each kind of slot goes only
//! in its kind of column, and `live.state` reports the kinds.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_engine::model::{ChannelFormat, Clip, SlotKind, TrackId, TrackKind};
use soundcraft_midi::{Note, Sequence};
use soundcraft_time::TICKS_PER_QUARTER;

const Q: i64 = TICKS_PER_QUARTER;

fn run(e: &mut Engine, cmd: &str, p: Value) -> Value {
    e.execute(cmd, &p).unwrap_or_else(|err| panic!("{cmd} {p}: {err}"))
}

fn err(e: &mut Engine, cmd: &str, p: Value) -> String {
    e.execute(cmd, &p).err().unwrap_or_else(|| panic!("{cmd} {p} should be refused")).to_string()
}

/// A session with the four audio columns and one instrument column (column 4).
fn engine() -> Engine {
    let mut e = Engine::default();
    run(&mut e, "live.setup", json!({}));
    run(&mut e, "live.add_column", json!({"kind": "instrument"}));
    e
}

fn note(pitch: u8, start: i64, length: i64) -> Note {
    Note { pitch, velocity: 100, release_velocity: 64, channel: 0, start, length }
}

/// A MIDI clip on a new instrument track, selected: `beats` long at 120 bpm, a note per beat
/// for 12 beats and a controller.
fn midi_clip(e: &mut Engine, beats: f64) -> u64 {
    let s = e.session_mut();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
    let mut seq = Sequence { notes: (0..12).map(|b| note(60 + b as u8, b * Q, Q / 2)).collect(), ctrls: Vec::new() };
    seq.ctrls.push(soundcraft_midi::CtrlEvent { tick: 0, channel: 0, kind: soundcraft_midi::CtrlKind::Cc(1), value: 64 });
    let id = s.new_clip_id();
    let len = (beats * 24_000.0) as i64;
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(id, "Lead", 48_000, len, seq));
    s.edit.selected_clips = vec![id];
    id.0
}

#[test]
fn add_column_makes_an_instrument_column_and_undoes() {
    let mut e = Engine::default();
    let r = run(&mut e, "live.add_column", json!({"kind": "instrument"}));
    assert_eq!((r["column"].as_u64(), r["kind"].as_str(), r["preset"].as_str()), (Some(0), Some("instrument"), Some("trance_kit")));
    let s = e.session();
    let t = s.track(TrackId(r["track"].as_u64().unwrap())).unwrap();
    assert_eq!((t.name.as_str(), t.kind), ("Live 1", TrackKind::Instrument));
    assert_eq!(t.instrument.as_ref().map(|i| i.plugin.as_str()), Some("drum_synth"));
    assert_eq!(s.live.column_kind(0, &s.tracks), Some(SlotKind::Midi));
    let r = run(&mut e, "live.add_column", json!({"kind": "instrument", "preset": "rolling_bass"}));
    assert_eq!((r["column"].as_u64(), r["preset"].as_str()), (Some(1), Some("rolling_bass")));
    let r = run(&mut e, "live.add_column", json!({}));
    assert_eq!((r["kind"].as_str(), r["preset"].as_str()), (Some("audio"), None));
    assert_eq!(e.session().live.column_kind(2, &e.session().tracks), Some(SlotKind::Audio));
    // Each is one undo step, taking its track with it.
    let tracks = e.session().tracks.len();
    assert!(e.undo());
    assert_eq!((e.session().live.columns.len(), e.session().tracks.len()), (2, tracks - 1));
    assert!(e.undo() && e.undo());
    assert!(e.session().live.columns.is_empty());
    assert!(err(&mut e, "live.add_column", json!({"kind": "midi"})).contains("audio or instrument"));
    assert!(err(&mut e, "live.add_column", json!({"kind": "audio", "preset": "trance_kit"})).contains("instrument columns"));
    assert!(err(&mut e, "live.add_column", json!({"kind": "instrument", "preset": "nope"})).contains("unknown preset"));
    for _ in 0..16 {
        run(&mut e, "live.add_column", json!({}));
    }
    assert!(err(&mut e, "live.add_column", json!({})).contains("most columns"));
}

#[test]
fn a_selected_midi_clip_becomes_a_slot_of_whole_bars() {
    let mut e = engine();
    // 13.6 beats: 3.4 bars, snapped to 3 (the notes from beat 12 on are left out).
    let clip = midi_clip(&mut e, 13.6);
    let r = run(&mut e, "live.slot_from_clip", json!({"column": 4}));
    assert_eq!((r["slot"].as_u64(), r["bars"].as_f64(), r["length_beats"].as_f64()), (Some(0), Some(3.0), Some(12.0)));
    assert_eq!((r["notes"].as_u64(), r["controllers_dropped"].as_u64(), r["clip"].as_u64()), (Some(12), Some(1), Some(clip)));
    let slot = e.session().live.midi_slot(4, 0).unwrap();
    assert_eq!((slot.name.as_str(), slot.length_beats, slot.sequence.notes.len()), ("Lead", 12.0, 12));
    // A shorter clip: one bar at least, and the notes past it are left out.
    let short = midi_clip(&mut e, 1.0);
    let r = run(&mut e, "live.slot_from_clip", json!({"column": 4, "slot": 3, "clip": short}));
    assert_eq!((r["bars"].as_f64(), r["notes"].as_u64()), (Some(1.0), Some(4)));
    assert!(e.undo());
    assert!(e.session().live.slot(4, 3).is_none());
    // Not into an audio column; nothing selected is refused.
    assert!(err(&mut e, "live.slot_from_clip", json!({"column": 0})).contains("audio column"));
    e.session_mut().edit.selected_clips.clear();
    assert!(err(&mut e, "live.slot_from_clip", json!({"column": 4})).contains("select a MIDI clip"));
}

#[test]
fn audio_goes_only_in_audio_columns() {
    let mut e = engine();
    assert!(err(&mut e, "live.slot_from_selection", json!({"column": 4})).contains("instrument column"));
    assert!(err(&mut e, "live.slot_from_file", json!({"column": 4, "path": "x.wav"})).contains("instrument column"));
    assert!(soundcraft_engine::cmd::slot_file_job(&e, &json!({"column": 4, "path": "x.wav"})).is_err());
}

#[test]
fn a_pattern_fills_a_slot_and_refuses_an_occupied_one() {
    let mut e = engine();
    let r = run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "trance_full", "slot": {"column": 4, "slot": 2}}));
    assert_eq!((r["column"].as_u64(), r["slot"].as_u64(), r["bars"].as_u64(), r["length_beats"].as_f64()), (Some(4), Some(2), Some(4), Some(16.0)));
    let slot = e.session().live.midi_slot(4, 2).unwrap().clone();
    assert_eq!(slot.name, "Trance Full");
    assert!(slot.sequence.notes.iter().any(|n| n.pitch == soundcraft_midi::patterns::KICK && n.start == 0), "a kick on the downbeat");
    assert!(slot.sequence.notes.iter().all(|n| n.start < 16 * Q));
    assert!(err(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 2}})).contains("replace"));
    let r = run(
        &mut e,
        "prod.pattern",
        json!({"kind": "bass", "style": "rolling", "key": "8A", "bars": 2, "replace": true, "slot": {"column": 4, "slot": 2}}),
    );
    assert_eq!((r["camelot"].as_str(), r["length_beats"].as_f64()), (Some("8A"), Some(8.0)));
    assert_eq!(e.session().live.midi_slot(4, 2).unwrap().name, "Rolling (8A)");
    assert!(e.undo());
    assert_eq!(e.session().live.midi_slot(4, 2), Some(&slot));
    // FX sweeps are left out, and said so.
    let r = run(&mut e, "prod.pattern", json!({"kind": "fx", "style": "riser", "slot": {"column": 4, "slot": 5}}));
    assert!(!r["automation_dropped"].as_array().unwrap().is_empty(), "{r}");
    // Keyed styles need a key: none plays at the playhead here.
    assert!(err(&mut e, "prod.pattern", json!({"kind": "bass", "style": "rolling", "slot": {"column": 4, "slot": 6}})).contains("no key"));
    assert!(err(&mut e, "prod.pattern", json!({"kind": "drums", "style": "fill", "slot": {"column": 0, "slot": 0}})).contains("audio column"));
    assert!(err(&mut e, "prod.pattern", json!({"kind": "drums", "style": "fill", "track": 1, "slot": {"column": 4, "slot": 0}})).contains("`track`"));
}

#[test]
fn slots_of_both_kinds_edit_clear_and_report_their_kind() {
    let mut e = engine();
    run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}}));
    let r = run(&mut e, "live.slot_set", json!({"column": 4, "slot": 0, "name": "Kicks", "gain_db": -6.0, "length_bars": 2, "replace": true}));
    assert_eq!(
        (r["kind"].as_str(), r["name"].as_str(), r["length_beats"].as_f64(), r["mode"].as_str(), r["notes_dropped"].as_u64()),
        (Some("midi"), Some("Kicks"), Some(8.0), Some("loop"), Some(8))
    );
    assert!(err(&mut e, "live.slot_set", json!({"column": 4, "slot": 0, "mode": "one_shot"})).contains("only loops"));
    let s = e.session_mut();
    s.live.columns[0].slots[1] = Some(soundcraft_engine::model::Slot::one_shot("Vox", [0; 3], soundcraft_engine::model::SourceId(77)).into());
    let state = run(&mut e, "live.state", json!({}));
    let cols = state["columns"].as_array().unwrap();
    assert_eq!((cols[0]["kind"].as_str(), cols[4]["kind"].as_str()), (Some("audio"), Some("instrument")));
    assert_eq!((cols[4]["slots"][0]["kind"].as_str(), cols[4]["slots"][0]["name"].as_str()), (Some("midi"), Some("Kicks")));
    assert_eq!(cols[0]["slots"][1]["kind"].as_str(), Some("audio"));
    assert!(cols[4]["slots"][1].is_null());
    run(&mut e, "live.slot_clear", json!({"column": 4, "slot": 0}));
    assert!(e.session().live.slot(4, 0).is_none());
    assert!(e.undo());
    assert_eq!(e.session().live.midi_slot(4, 0).map(|x| x.name.as_str()), Some("Kicks"));
}

/// A lost instrument column gets an instrument track back from `live.setup`, its MIDI slots
/// kept; a lost audio column an audio track.
#[test]
fn setup_gives_a_lost_instrument_column_an_instrument_track() {
    let mut e = engine();
    run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}}));
    let lost = e.session().live.columns[4].track;
    e.session_mut().tracks.retain(|t| t.id != lost);
    assert_eq!(e.session().live.lost_columns(&e.session().tracks), [4]);
    run(&mut e, "live.setup", json!({}));
    let s = e.session();
    assert_eq!(s.live.column_kind(4, &s.tracks), Some(SlotKind::Midi));
    assert!(s.live.midi_slot(4, 0).is_some());
    assert_eq!(s.track(s.live.columns[4].track).and_then(|t| t.instrument.as_ref()).map(|i| i.plugin.as_str()), Some("drum_synth"));
}

/// Slots hold at most 256 bars: a longer pattern or clip is refused, not cut.
#[test]
fn slots_hold_at_most_256_bars() {
    let mut e = engine();
    let r = run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "bars": 256, "slot": {"column": 4, "slot": 0}}));
    assert_eq!(r["length_beats"].as_f64(), Some(1024.0));
    assert!(
        err(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "bars": 257, "slot": {"column": 4, "slot": 1}}))
            .contains("at most 256")
    );
    // 257 bars at 120 bpm.
    midi_clip(&mut e, 257.0 * 4.0);
    assert!(err(&mut e, "live.slot_from_clip", json!({"column": 4, "slot": 1})).contains("at most 256"));
    assert!(e.session().live.slot(4, 1).is_none());
}

/// Copying a clip into a filled slot needs `replace`, as a pattern does.
#[test]
fn a_clip_replaces_a_filled_slot_only_when_asked() {
    let mut e = engine();
    run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}}));
    midi_clip(&mut e, 8.0);
    assert!(err(&mut e, "live.slot_from_clip", json!({"column": 4, "slot": 0})).contains("replace"));
    run(&mut e, "live.slot_from_clip", json!({"column": 4, "slot": 0, "replace": true}));
    assert_eq!(e.session().live.midi_slot(4, 0).map(|x| x.name.as_str()), Some("Lead"));
}

/// A lost column holding both kinds comes back as an instrument column: its audio slots, which
/// could not play there, are emptied and reported.
#[test]
fn setup_empties_the_audio_slots_of_a_lost_mixed_column() {
    let mut e = engine();
    run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}}));
    let lost = e.session().live.columns[4].track;
    let s = e.session_mut();
    s.tracks.retain(|t| t.id != lost);
    s.live.columns[4].slots[3] = Some(soundcraft_engine::model::Slot::one_shot("Vox", [0; 3], soundcraft_engine::model::SourceId(77)).into());
    let r = run(&mut e, "live.setup", json!({}));
    assert_eq!(r["emptied"], json!([{"column": 4, "slot": 3, "name": "Vox"}]));
    assert!(e.messages.iter().any(|m| m.contains("emptied `Vox` (Live 5 slot 4)")), "{:?}", e.messages);
    let s = e.session();
    assert_eq!(s.live.column_kind(4, &s.tracks), Some(SlotKind::Midi));
    assert!(s.live.slot(4, 3).is_none() && s.live.midi_slot(4, 0).is_some());
}

/// `live.slot_to_clip` mirrors `live.slot_from_clip`: the slot's notes (every field) become a
/// clip on the column's track from the bar holding `at`, as one undo step, refusing to replace a
/// clip there unless asked and refusing a slot without MIDI.
#[test]
fn a_midi_slot_copies_out_to_a_clip_on_its_columns_track() {
    let mut e = engine();
    let clip = midi_clip(&mut e, 16.0);
    {
        // Every note field survives the round trip, release velocity and channel included.
        let s = e.session_mut();
        let c = s.find_clip_mut(soundcraft_engine::model::ClipId(clip)).unwrap();
        if let soundcraft_engine::model::ClipContent::Midi { sequence } = &mut c.content {
            for (i, n) in sequence.notes.iter_mut().enumerate() {
                n.release_velocity = 20 + i as u8;
                n.channel = (i % 3) as u8;
                n.velocity = 90 + i as u8;
            }
        }
    }
    run(&mut e, "live.slot_from_clip", json!({"column": 4, "slot": 0, "clip": clip}));
    let slot = e.session().live.midi_slot(4, 0).unwrap().clone();
    let track = e.session().live.column_track(4, &e.session().tracks).unwrap();
    let undo = e.undo_history().len();
    // At 120 bpm a bar is 96 000 samples: 100 000 is in bar 2.
    let r = run(&mut e, "live.slot_to_clip", json!({"column": 4, "slot": 0, "at": 100_000}));
    assert_eq!((r["start"].as_i64(), r["end"].as_i64(), r["bars"].as_f64()), (Some(96_000), Some(96_000 * 5), Some(4.0)));
    assert_eq!(e.undo_history().len(), undo + 1, "one undo step");
    let s = e.session();
    let made = s.find_clip(soundcraft_engine::model::ClipId(r["clip"].as_u64().unwrap())).unwrap();
    assert_eq!(made.0, track);
    assert_eq!((made.1.name.as_str(), made.1.color), (slot.name.as_str(), Some(slot.color)));
    match &made.1.content {
        soundcraft_engine::model::ClipContent::Midi { sequence } => assert_eq!(sequence.notes, slot.sequence.notes),
        _ => panic!("not MIDI"),
    }
    // Overlapping it: refused (typed, so a UI can offer Replace), then replaced when asked.
    let refused = e.execute("live.slot_to_clip", &json!({"column": 4, "slot": 0, "at": 200_000})).unwrap_err();
    assert!(matches!(refused, soundcraft_engine::EngineError::WouldReplace { .. }), "{refused:?}");
    assert!(refused.to_string().starts_with("bad parameters for `live.slot_to_clip`: bar 3 on "), "{refused}");
    assert!(refused.to_string().ends_with("pass replace: true to replace it"), "{refused}");
    assert_eq!(e.session().track(track).unwrap().clips().len(), 1);
    run(&mut e, "live.slot_to_clip", json!({"column": 4, "slot": 0, "at": 200_000, "replace": true}));
    let starts: Vec<i64> = e.session().track(track).unwrap().clips().iter().map(|c| c.start).collect();
    assert_eq!(starts, [96_000, 192_000], "the first is trimmed to what wasn't under the new one");
    assert!(e.undo());
    assert_eq!(e.session().track(track).unwrap().clips().len(), 1);
    assert!(e.undo());
    assert!(e.session().track(track).unwrap().clips().is_empty());
    // An empty slot, an audio slot and a missing slot are refused.
    assert!(err(&mut e, "live.slot_to_clip", json!({"column": 4, "slot": 3})).contains("is empty"));
    assert!(err(&mut e, "live.slot_to_clip", json!({"column": 4})).contains("`slot` required"));
    let wav = std::env::temp_dir().join(format!("djforge-slot-to-clip-{}.wav", std::process::id()));
    let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 96_000]; 2] };
    std::fs::write(&wav, soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
    let loaded = e.execute("live.slot_from_file", &json!({"column": 0, "slot": 0, "path": wav, "bpm": 120}));
    let _ = std::fs::remove_file(&wav);
    loaded.unwrap();
    assert!(err(&mut e, "live.slot_to_clip", json!({"column": 0, "slot": 0})).contains("holds audio"));
}

/// Every refusal to replace is typed, reading as before.
#[test]
fn refusals_to_replace_are_typed() {
    let mut e = engine();
    run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}}));
    let r = e.execute("prod.pattern", &json!({"kind": "drums", "style": "fill", "slot": {"column": 4, "slot": 0}})).unwrap_err();
    assert!(matches!(r, soundcraft_engine::EngineError::WouldReplace { .. }));
    assert_eq!(r.to_string(), "bad parameters for `prod.pattern`: slot 1 of Live column 5 holds `Four Floor`: pass replace: true to replace it");
    let clip = midi_clip(&mut e, 4.0);
    let r = e.execute("live.slot_from_clip", &json!({"column": 4, "slot": 0, "clip": clip})).unwrap_err();
    assert!(matches!(r, soundcraft_engine::EngineError::WouldReplace { .. }), "{r:?}");
}

/// Shortening a MIDI slot so notes start past its new end needs `replace`; what it then holds is
/// what a saved session reloads (only the notes it plays, those running past the end cut there).
#[test]
fn shrinking_a_midi_slot_drops_notes_only_when_asked_and_reloads_as_held() {
    let mut e = engine();
    run(&mut e, "prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}}));
    // A note running from beat 6 over the 2-bar line.
    let held = Note { pitch: 49, velocity: 90, release_velocity: 64, channel: 9, start: 6 * Q, length: 4 * Q };
    e.session_mut().live.columns[4].slots[0] = {
        let mut x = e.session().live.midi_slot(4, 0).unwrap().clone();
        x.sequence.notes.push(held);
        x.sequence.sort();
        Some(x.into())
    };
    let before = e.session().live.clone();
    let undo = e.undo_history().len();
    let refused = e.execute("live.slot_set", &json!({"column": 4, "slot": 0, "length_bars": 2})).unwrap_err();
    assert!(matches!(refused, soundcraft_engine::EngineError::WouldReplace { .. }), "{refused:?}");
    assert_eq!(
        refused.to_string(),
        "bad parameters for `live.slot_set`: 2 bars would drop 8 notes of `Four Floor` (slot 1 of Live column 5) starting past the new end: pass replace: true to drop them"
    );
    assert_eq!((e.session().live == before, e.undo_history().len()), (true, undo), "a refusal changes nothing");
    // Lengthening, and a shrink that drops nothing, need no `replace`.
    assert_eq!(run(&mut e, "live.slot_set", json!({"column": 4, "slot": 0, "length_bars": 8}))["notes_dropped"], 0);
    let r = run(&mut e, "live.slot_set", json!({"column": 4, "slot": 0, "length_bars": 2, "replace": true}));
    assert_eq!((r["notes_dropped"].as_u64(), r["length_beats"].as_f64()), (Some(8), Some(8.0)));
    let x = e.session().live.midi_slot(4, 0).unwrap().clone();
    assert_eq!(x.sequence.notes.len(), 9, "8 kicks and the held note");
    assert!(x.sequence.notes.iter().any(|n| n.pitch == 49 && n.start == 6 * Q && n.length == 2 * Q), "the held note ends on the loop end");
    // Saved and reopened, it is the same.
    let dir = std::env::temp_dir().join(format!("djforge-slot-shrink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("shrink.scraft");
    soundcraft_engine::io::save_session(&mut e, file.to_str().unwrap()).unwrap();
    let mut back = Engine::default();
    let opened = soundcraft_engine::io::open_session(&mut back, file.to_str().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    opened.unwrap();
    assert_eq!(back.session().live, e.session().live);
}

/// A lost instrument column's new track plays what its slots hold: the drum kit for drums, the
/// bass otherwise.
#[test]
fn setup_picks_a_lost_instrument_columns_preset_from_its_notes() {
    for (kind, style, key, plugin) in [("drums", "four_floor", None, "drum_synth"), ("bass", "rolling", Some("Am"), "subtractive_synth")] {
        let mut e = engine();
        let mut p = json!({"kind": kind, "style": style, "slot": {"column": 4, "slot": 0}});
        if let Some(k) = key {
            p["key"] = json!(k);
        }
        run(&mut e, "prod.pattern", p);
        let lost = e.session().live.columns[4].track;
        e.session_mut().tracks.retain(|t| t.id != lost);
        run(&mut e, "live.setup", json!({}));
        let s = e.session();
        let ins = s.track(s.live.columns[4].track).and_then(|t| t.instrument.clone()).unwrap();
        assert_eq!(ins.plugin, plugin, "{kind}");
    }
}
