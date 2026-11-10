//! Recording live MIDI from the app, on the silent clock (no audio device, no MIDI port: notes
//! come from the on-screen keyboard's path into the audio thread): an arrangement take on an
//! armed instrument track becomes a clip when recording stops, without an audio input; a slot
//! recording targets its column, writes the slot and launches it where it ends; Esc cancels.

use egui::{Event, Key, Pos2, RawInput, Rect, vec2};
use serde_json::{Value, json};
use soundcraft_midi_io::MidiMessage;
use soundcraft_model::{ChannelFormat, ClipContent, Insert, TrackId, TrackKind};
use soundcraft_ui_egui::{Services, SoundApp};
use std::time::{Duration, Instant};

fn frame(ctx: &egui::Context, app: &mut SoundApp, events: Vec<Event>) {
    let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
    ctx.run_ui(raw, |ui| {
        app.logic(ui.ctx());
        app.ui(ui);
    })
    .textures_delta
    .clear();
}

/// Frames (in real time, for the silent clock) until `done` holds; fails after 10 s.
fn until(ctx: &egui::Context, app: &mut SoundApp, what: &str, done: impl Fn(&SoundApp) -> bool) {
    let start = Instant::now();
    while !done(app) {
        assert!(start.elapsed() < Duration::from_secs(10), "never: {what} ({})", app.ui.status);
        frame(ctx, app, vec![]);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An app on the silent clock at 240 bpm (a bar a second) with a record-armed synth track and a
/// Session view instrument column (column 4).
fn app() -> (SoundApp, TrackId) {
    let mut e = soundcraft_engine::Engine::default();
    e.session_mut().tempo.set_tempo(0, 240.0).unwrap();
    e.execute("live.setup", &json!({})).unwrap();
    e.execute("live.add_column", &json!({"kind": "instrument"})).unwrap();
    let s = e.session_mut();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Lead"));
    let tr = s.track_mut(t).unwrap();
    tr.instrument = Some(Insert::new("subtractive_synth"));
    tr.mixer.record_arm = true;
    let player = soundcraft_playback::Player::silent(e.session_arc());
    (SoundApp::new(e, Some(player), Services::default()), t)
}

fn play(app: &SoundApp, msg: MidiMessage) {
    app.player.as_ref().unwrap().play_midi(msg);
}

fn pos(app: &SoundApp) -> i64 {
    app.player.as_ref().unwrap().position()
}

#[test]
fn an_arrangement_take_on_an_armed_instrument_track_becomes_a_clip_at_stop() {
    let (mut app, t) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    app.run("transport.record", json!({})).unwrap();
    until(&ctx, &mut app, "recording", |a| a.engine.transport.recording && pos(a) > 10_000);
    assert!(app.midi_take.is_some(), "a MIDI take with no audio input: {}", app.ui.status);
    play(&app, MidiMessage::NoteOn { ch: 0, note: 60, vel: 100 });
    let on = pos(&app);
    until(&ctx, &mut app, "the note sounds a while", |a| pos(a) > on + 6_000);
    play(&app, MidiMessage::NoteOff { ch: 0, note: 60, vel: 0 });
    play(&app, MidiMessage::NoteOn { ch: 0, note: 64, vel: 90 });
    let held = pos(&app);
    until(&ctx, &mut app, "the second note sounds", |a| pos(a) > held + 6_000);
    app.run("transport.stop", json!({})).unwrap();
    frame(&ctx, &mut app, vec![]);
    assert!(!app.engine.transport.recording);
    let clips = app.engine.session().track(t).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 1, "{}", app.ui.status);
    assert!(app.ui.status.contains("Recorded 2 MIDI note(s)"), "{}", app.ui.status);
    let ClipContent::Midi { sequence } = &clips[0].content else { panic!() };
    assert_eq!(sequence.notes.iter().map(|n| n.pitch).collect::<Vec<_>>(), [60, 64]);
    assert_eq!(clips[0].start, 0, "from the bar line at or before the start");
    assert_eq!(app.engine.undo_label(), Some("Record MIDI"));
    assert!(app.engine.undo());
    assert!(app.engine.session().track(t).unwrap().clips().is_empty(), "undo removes the take");
}

#[test]
fn a_slot_recording_targets_its_column_and_launches_where_it_ends() {
    let (mut app, t) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    let column_track = app.engine.session().live.column_track(4, &app.engine.session().tracks).unwrap();
    let err = app.run("live.record_slot", json!({"column": 0, "slot": 0, "bars": 1})).unwrap_err();
    assert!(err.contains("audio column"), "{err}");
    // Stopped: the transport starts from the insertion, recording from bar 1.
    app.run("live.record_slot", json!({"column": 4, "slot": 1, "bars": 1})).unwrap();
    frame(&ctx, &mut app, vec![]);
    let r = app.slot_recording.clone().expect("armed");
    assert_eq!((r.start, r.end), (0, 48_000), "one bar at 240 bpm");
    until(&ctx, &mut app, "the column is the target", |a| a.player.as_ref().unwrap().midi_target() == Some(column_track));
    until(&ctx, &mut app, "recording", |a| pos(a) > 4_000);
    play(&app, MidiMessage::NoteOn { ch: 0, note: 36, vel: 100 });
    let on = pos(&app);
    until(&ctx, &mut app, "a beat later", |a| pos(a) > on + 6_000);
    play(&app, MidiMessage::NoteOff { ch: 0, note: 36, vel: 0 });
    until(&ctx, &mut app, "the recording ends", |a| a.slot_recording.is_none());
    assert!(app.ui.status.contains("Recorded 1 note(s) into slot 2 of Live 5"), "{}", app.ui.status);
    let slot = app.engine.session().live.slot(4, 1).and_then(|x| x.midi()).cloned().expect("written");
    assert_eq!((slot.length_beats, slot.sequence.notes.len()), (4.0, 1));
    until(&ctx, &mut app, "the slot plays", |a| a.player.as_ref().unwrap().live_state()[4].playing.is_some());
    assert_eq!(app.player.as_ref().unwrap().live_state()[4].playing.map(|p| p.0), Some(1));
    assert_eq!(app.player.as_ref().unwrap().live_state()[4].playing.map(|p| p.1), Some(48_000), "launched where it ended");
    assert_eq!(app.engine.undo_history().len(), 3, "live.setup, the column, and one step for the take");
    // The old target is back: the armed track.
    frame(&ctx, &mut app, vec![]);
    assert_eq!(app.player.as_ref().unwrap().midi_target(), Some(t));
}

#[test]
fn esc_or_stop_cancels_a_slot_recording() {
    let (mut app, _) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    app.run("live.quantise", json!({"value": "4"})).unwrap();
    app.run("live.record_slot", json!({"column": 4, "slot": 0, "bars": 1})).unwrap();
    frame(&ctx, &mut app, vec![]);
    assert!(app.slot_recording.is_some());
    let key = |pressed| Event::Key { key: Key::Escape, physical_key: None, pressed, repeat: false, modifiers: Default::default() };
    frame(&ctx, &mut app, vec![key(true), key(false)]);
    assert!(app.slot_recording.is_none(), "Esc cancels");
    assert_eq!(app.ui.status, "Slot recording cancelled");
    app.run("live.record_slot", json!({"column": 4, "slot": 0, "bars": 1})).unwrap();
    frame(&ctx, &mut app, vec![]);
    assert!(app.slot_recording.is_some());
    app.run("transport.stop", json!({})).unwrap();
    frame(&ctx, &mut app, vec![]);
    assert!(app.slot_recording.is_none(), "Stop cancels");
    assert!(app.engine.session().live.slot(4, 0).is_none(), "nothing written");
    let filled: Value = app.engine.execute("live.state", &json!({})).unwrap()["columns"][4]["slots"][0].clone();
    assert_eq!(filled, Value::Null);
}

/// Plays `pitch` for 6 000 samples.
fn tap(ctx: &egui::Context, app: &mut SoundApp, pitch: u8) {
    play(app, MidiMessage::NoteOn { ch: 0, note: pitch, vel: 100 });
    let on = pos(app);
    until(ctx, app, "the note sounds a while", |a| pos(a) > on + 6_000);
    play(app, MidiMessage::NoteOff { ch: 0, note: pitch, vel: 0 });
}

#[test]
fn a_take_over_a_clip_goes_on_a_new_playlist_and_the_last_take_recovers() {
    let (mut app, t) = app();
    let ctx = egui::Context::default();
    // A clip already where the take will go.
    app.run("midi.new_clip", json!({"track": t.0, "start": 0, "end": 200_000})).unwrap();
    frame(&ctx, &mut app, vec![]);
    app.run("transport.record", json!({})).unwrap();
    until(&ctx, &mut app, "recording", |a| a.engine.transport.recording && pos(a) > 10_000);
    tap(&ctx, &mut app, 60);
    let off = pos(&app);
    until(&ctx, &mut app, "the note-off is in", |a| pos(a) > off + 2_000);
    app.run("transport.stop", json!({})).unwrap();
    frame(&ctx, &mut app, vec![]);
    let tr = app.engine.session().track(t).unwrap().clone();
    assert_eq!((tr.playlists.len(), tr.active_playlist), (2, 1), "{}", app.ui.status);
    assert_eq!(tr.playlists[0].clips.len(), 1, "the clip under it is intact on the playlist before");
    assert!(app.ui.status.contains("new playlist `Lead.02`"), "{}", app.ui.status);
    // Recover Last MIDI Take places it again, on a new playlist.
    app.run("midi.recover_take", json!({})).unwrap();
    let tr = app.engine.session().track(t).unwrap();
    assert_eq!((tr.playlists.len(), tr.active_playlist, tr.clips().len()), (3, 2, 1));
    assert!(app.ui.status.contains("Lead.03"), "{}", app.ui.status);
    assert_eq!(app.engine.undo_label(), Some("Record MIDI"));
}

#[test]
fn a_slot_filled_during_its_recording_sends_the_take_to_the_next_empty_slot() {
    let (mut app, _) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    app.run("live.record_slot", json!({"column": 4, "slot": 1, "bars": 1})).unwrap();
    frame(&ctx, &mut app, vec![]);
    until(&ctx, &mut app, "recording", |a| pos(a) > 4_000);
    tap(&ctx, &mut app, 36);
    // Something else fills slot 2 meanwhile (a pattern, a copy).
    let p = json!({"column": 4, "slot": 1, "start": 0, "bars": 1, "events": [{"at": 10, "status": 0x90, "data1": 40, "data2": 90}]});
    app.engine.execute("live.slot_from_take", &p).unwrap();
    until(&ctx, &mut app, "the recording ends", |a| a.slot_recording.is_none());
    assert!(app.ui.status.contains("into slot 1 of Live 5 (slot 2 was filled meanwhile)"), "{}", app.ui.status);
    let s = app.engine.session();
    assert_eq!(s.live.slot(4, 0).and_then(|x| x.midi()).map(|m| m.sequence.notes[0].pitch), Some(36), "the take");
    assert_eq!(s.live.slot(4, 1).and_then(|x| x.midi()).map(|m| m.sequence.notes[0].pitch), Some(40), "what filled it stays");
}

#[test]
fn a_slot_take_with_no_empty_slot_left_is_kept_for_recovery() {
    let (mut app, t) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    app.run("live.record_slot", json!({"column": 4, "slot": 7, "bars": 1})).unwrap();
    frame(&ctx, &mut app, vec![]);
    until(&ctx, &mut app, "recording", |a| pos(a) > 4_000);
    tap(&ctx, &mut app, 36);
    for slot in 0..8 {
        let p = json!({"column": 4, "slot": slot, "start": 0, "bars": 1, "events": [{"at": 10, "status": 0x90, "data1": 40, "data2": 90}]});
        app.engine.execute("live.slot_from_take", &p).unwrap();
    }
    until(&ctx, &mut app, "the recording ends", |a| a.slot_recording.is_none());
    assert!(app.ui.status.contains("Recover Last MIDI Take"), "{}", app.ui.status);
    let column_track = app.engine.session().live.column_track(4, &app.engine.session().tracks).unwrap();
    assert_ne!(column_track, t);
    let r = app.run("midi.recover_take", json!({})).unwrap();
    assert_eq!(r["notes"], 1);
    let tr = app.engine.session().track(column_track).unwrap();
    assert_eq!(tr.clips().len(), 1, "on the column's track");
}

/// A first write that put nothing in the slot (nothing played yet) doesn't make the slot the
/// recording's: filled by something else before the notes played last come in, it is not
/// replaced, and the take goes to the next empty slot.
#[test]
fn an_empty_first_write_does_not_claim_the_slot() {
    let (mut app, _) = app();
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, vec![]);
    app.run("live.record_slot", json!({"column": 4, "slot": 2, "bars": 1})).unwrap();
    frame(&ctx, &mut app, vec![]);
    until(&ctx, &mut app, "the first write", |a| a.slot_recording.as_ref().is_some_and(|r| r.written.is_some()));
    assert!(!app.slot_recording.as_ref().unwrap().ours);
    assert!(app.engine.session().live.slot(4, 2).is_none(), "nothing to write yet");
    play(&app, MidiMessage::NoteOn { ch: 0, note: 38, vel: 100 });
    let p = json!({"column": 4, "slot": 2, "start": 0, "bars": 1, "events": [{"at": 10, "status": 0x90, "data1": 40, "data2": 90}]});
    app.engine.execute("live.slot_from_take", &p).unwrap();
    until(&ctx, &mut app, "the recording ends", |a| a.slot_recording.is_none());
    let s = app.engine.session();
    assert_eq!(s.live.slot(4, 2).and_then(|x| x.midi()).map(|m| m.sequence.notes[0].pitch), Some(40), "not replaced");
    assert_eq!(s.live.slot(4, 0).and_then(|x| x.midi()).map(|m| m.sequence.notes[0].pitch), Some(38), "{}", app.ui.status);
}
