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
