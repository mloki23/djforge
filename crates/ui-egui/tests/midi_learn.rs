//! MIDI learn from the app, with the devices a `FakeSource` (never a real MIDI port) and the
//! silent clock: Learn MIDI on a mixer fader binds the next controller, which then rides the
//! fader once picked up; Learn MIDI on a Session view slot binds a pad that then launches the
//! slot through `live.launch`, quantised; a mapped note doesn't play the live instrument.
#![allow(clippy::unwrap_used)]

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use serde_json::json;
use soundcraft_midi_io::{FakeSource, MidiClock, MidiMessage};
use soundcraft_model::{ChannelFormat, Insert, TrackKind};
use soundcraft_ui_egui::midi_input::MidiHost;
use soundcraft_ui_egui::midi_learn::{Matcher, Target};
use soundcraft_ui_egui::{Services, SoundApp};
use std::time::{Duration, Instant};

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().raw_input_hook(&mut raw);
    *h.input_mut() = raw;
    h.step();
}

fn steps(h: &mut Harness<'_, SoundApp>, n: usize) {
    for _ in 0..n {
        step(h);
    }
}

/// Steps (in real time: the router thread delivers, the silent clock runs) until `done`; fails
/// after 5 s.
fn until(h: &mut Harness<'_, SoundApp>, what: &str, done: impl Fn(&SoundApp) -> bool) {
    let start = Instant::now();
    while !done(h.state()) {
        assert!(start.elapsed() < Duration::from_secs(5), "never: {what} ({})", h.state().ui.status);
        step(h);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The app on `engine` with a silent player and a fake "DDJ" enabled for live input.
fn harness(engine: soundcraft_engine::Engine) -> Harness<'static, SoundApp> {
    let player = soundcraft_playback::Player::silent(engine.session_arc());
    let mut app = SoundApp::new(engine, Some(player), Services::default());
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    src.plug("DDJ");
    app.midi_host = Some(MidiHost::new(Box::new(src)));
    app.run("midi.input_enable", json!({"name": "DDJ", "enabled": true})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    steps(&mut h, 4);
    h
}

fn send(h: &mut Harness<'_, SoundApp>, msg: MidiMessage) {
    let src = h.state_mut().midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap();
    assert!(src.inject("DDJ", msg, 0));
}

fn cc(cc: u8, value: u8) -> MidiMessage {
    MidiMessage::ControlChange { ch: 0, cc, value }
}

fn db(app: &SoundApp, name: &str) -> f32 {
    app.engine.session().track_by_name(name).unwrap().mixer.volume_db
}

#[test]
fn learn_from_a_mixer_fader_binds_the_next_controller_which_rides_the_fader() {
    let mut h = harness(soundcraft_engine::demo::demo_engine());
    h.state_mut().run("window.mix", json!({})).unwrap();
    h.state_mut().run("mix.volume", json!({"tracks": ["Kick"], "db": 0.0})).unwrap();
    steps(&mut h, 3);
    h.get_by_label("Kick volume").click_secondary();
    steps(&mut h, 3);
    h.get_by_label("Learn MIDI").click();
    steps(&mut h, 2);
    assert!(h.state().midi_learn.learning.is_some());
    let _ = h.get_by_label("Move a control…");
    send(&mut h, cc(7, 20));
    until(&mut h, "learned", |a| !a.ui.midi_mappings.is_empty());
    let m = h.state().ui.midi_mappings[0].clone();
    assert_eq!(m.matcher, Matcher::Cc { ch: 0, cc: 7 });
    assert!(matches!(&m.target, Target::TrackVolume { track } if track.name == "Kick"), "{:?}", m.target);
    let _ = h.get_by_label("Mapped to DDJ CC 7");
    // Below 0 dB (CC 95): picked up only once it crosses.
    send(&mut h, cc(7, 40));
    steps(&mut h, 5);
    assert_eq!(db(h.state(), "Kick"), 0.0, "not picked up");
    let undo = h.state().engine.undo_history().len();
    for v in [90, 96, 100, 104] {
        send(&mut h, cc(7, v));
    }
    let want = soundcraft_model::fader_pos_to_db(104.0 / 127.0);
    until(&mut h, "the fader follows", |a| (db(a, "Kick") - want).abs() < 1e-4);
    assert_eq!(h.state().engine.undo_history().len(), undo + 1, "one move, one undo step, across frames");
    // Esc cancels learning.
    h.state_mut().run("midi.learn", json!({"target": {"kind": "track_pan", "track": "Kick"}})).unwrap();
    h.key_press(egui::Key::Escape);
    steps(&mut h, 2);
    assert!(h.state().midi_learn.learning.is_none());
}

/// A Session view instrument column with a 1-bar MIDI slot, at 240 bpm (a bar a second).
fn live_engine() -> soundcraft_engine::Engine {
    let mut e = soundcraft_engine::Engine::default();
    e.session_mut().tempo.set_tempo(0, 240.0).unwrap();
    e.execute("live.setup", &json!({})).unwrap();
    e.execute("live.add_column", &json!({"kind": "instrument"})).unwrap();
    let s = e.session_mut();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
    let notes =
        (0..4).map(|b| soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: b * 960, length: 480 }).collect();
    let id = s.new_clip_id();
    let clip = soundcraft_model::Clip::midi(id, "Lead", 0, 48_000, soundcraft_midi::Sequence { notes, ctrls: Vec::new() });
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(clip);
    e.execute("live.slot_from_clip", &json!({"column": 4, "slot": 0, "clip": id.0})).unwrap();
    e
}

#[test]
fn learn_from_a_session_slot_binds_a_pad_that_launches_it_quantised() {
    let mut h = harness(live_engine());
    h.state_mut().run("window.session", json!({"value": true})).unwrap();
    h.state_mut().run("live.quantise", json!({"value": "1"})).unwrap();
    steps(&mut h, 4);
    h.get_by_label("Live 5 slot 1").click_secondary();
    steps(&mut h, 3);
    h.get_by_label("Learn MIDI").click();
    steps(&mut h, 2);
    send(&mut h, MidiMessage::NoteOn { ch: 9, note: 36, vel: 100 });
    send(&mut h, MidiMessage::NoteOff { ch: 9, note: 36, vel: 0 });
    until(&mut h, "learned", |a| !a.ui.midi_mappings.is_empty());
    let m = h.state().ui.midi_mappings[0].clone();
    assert_eq!((m.matcher, m.target.clone()), (Matcher::Note { ch: 9, note: 36 }, Target::SlotLaunch { column: 4, slot: 0 }));
    steps(&mut h, 3);
    assert!(!h.state().is_playing(), "learning launches nothing");
    // Playing, a little into the bar: the launch waits for the next bar line.
    h.state_mut().run("transport.play", json!({})).unwrap();
    until(&mut h, "into the bar", |a| a.player.as_ref().unwrap().position() > 2_000);
    send(&mut h, MidiMessage::NoteOn { ch: 9, note: 36, vel: 100 });
    let state = |a: &mut SoundApp| a.engine.execute("live.state", &json!({})).unwrap();
    let start = Instant::now();
    let queued = loop {
        step(&mut h);
        let s = state(h.state_mut());
        if s["columns"][4]["queued"].is_object() || s["columns"][4]["playing"]["slot"].as_u64().is_some() {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "never launched: {}", h.state().ui.status);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(queued["columns"][4]["queued"].is_object(), "waits for the bar line, as a click on the slot does: {queued}");
    let start = Instant::now();
    while state(h.state_mut())["columns"][4]["playing"]["slot"].as_u64() != Some(0) {
        assert!(start.elapsed() < Duration::from_secs(5), "never played");
        step(&mut h);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An app on the silent clock whose record-armed synth is the live target.
fn synth() -> (SoundApp, soundcraft_model::TrackId) {
    let mut engine = soundcraft_engine::Engine::default();
    let t = {
        let s = engine.session_mut();
        let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Synth"));
        let tr = s.track_mut(t).unwrap();
        tr.instrument = Some(Insert::new("subtractive_synth"));
        tr.mixer.record_arm = true;
        t
    };
    let player = soundcraft_playback::Player::silent(engine.session_arc());
    (SoundApp::new(engine, Some(player), Services::default()), t)
}

fn frame(ctx: &egui::Context, app: &mut SoundApp) {
    let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1600.0, 1000.0))), ..Default::default() };
    ctx.run_ui(raw, |ui| {
        app.logic(ui.ctx());
        app.ui(ui);
    })
    .textures_delta
    .clear();
}

fn peak(app: &SoundApp, t: soundcraft_model::TrackId) -> f32 {
    app.player.as_ref().unwrap().meters().tracks.get(&t).map_or(0.0, |m| m.peak[0].max(m.peak[1]))
}

#[test]
fn a_mapped_note_does_not_play_the_instrument_until_it_is_unmapped() {
    let (mut app, t) = synth();
    let mut src = FakeSource::new(MidiClock::new()).unwrap();
    src.plug("Pads");
    app.midi_host = Some(MidiHost::new(Box::new(src)));
    app.run("midi.input_enable", json!({"name": "Pads", "enabled": true})).unwrap();
    app.run(
        "midi.map",
        json!({"device": "Pads", "matcher": {"type": "note", "ch": 0, "note": 60}, "target": {"kind": "command", "id": "window.transport"}}),
    )
    .unwrap();
    let ctx = egui::Context::default();
    for _ in 0..3 {
        frame(&ctx, &mut app);
    }
    assert_eq!(app.player.as_ref().unwrap().midi_target(), Some(t));
    let inject = |app: &mut SoundApp, msg: MidiMessage| {
        assert!(app.midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap().inject("Pads", msg, 0));
    };
    inject(&mut app, MidiMessage::NoteOn { ch: 0, note: 60, vel: 127 });
    let start = Instant::now();
    while !app.ui.show_transport {
        assert!(start.elapsed() < Duration::from_secs(5), "the pad never ran its command");
        frame(&ctx, &mut app);
        std::thread::yield_now();
    }
    // Long enough for a played note to be heard on the silent clock.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        frame(&ctx, &mut app);
        assert!(peak(&app, t) < 1e-3, "a mapped note played the synth");
        std::thread::sleep(Duration::from_millis(5));
    }
    inject(&mut app, MidiMessage::NoteOff { ch: 0, note: 60, vel: 0 });
    app.run("midi.unmap", json!({"all": true})).unwrap();
    frame(&ctx, &mut app);
    inject(&mut app, MidiMessage::NoteOn { ch: 0, note: 60, vel: 127 });
    let start = Instant::now();
    while peak(&app, t) < 1e-3 {
        assert!(start.elapsed() < Duration::from_secs(5), "unmapped, the note never played");
        frame(&ctx, &mut app);
        std::thread::yield_now();
    }
}
