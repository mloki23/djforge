//! The stage 7b gate: the whole MIDI hardware chain through the app, scripted end to end. Two
//! fake devices (a `FakeSource`: no real MIDI port is ever opened) on the silent clock: "Keys"
//! plays and records a 4-bar bassline into a `prod.new_part` bass part, "DDJ" learns a stem
//! fader on a split drums clip and a Session view slot launch, then is unplugged and replugged;
//! a mapped pad never sounds on the bass, through an audio device change and a replug into
//! another USB socket (a new backend id).
//! Stems are split by a fake Demucs into a scratch stem cache, never the app's own.
#![allow(clippy::unwrap_used)]

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode};
use soundcraft_engine::cmd::stems::{StemCache, StemRunner};
use soundcraft_engine::model::{STEMS, TrackId};
use soundcraft_midi_io::{FakeSource, MidiClock, MidiMessage};
use soundcraft_model::ClipContent;
use soundcraft_playback::CaptureEvent;
use soundcraft_ui_egui::midi_input::MidiHost;
use soundcraft_ui_egui::midi_learn::{Matcher, Mode, Target};
use soundcraft_ui_egui::{Services, SoundApp};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;
/// 240 bpm: a beat is 12 000 samples, a bar 48 000 (a second), a tick 12.5 samples.
const BPM: f64 = 240.0;
const SAMPLES_PER_TICK: f64 = RATE as f64 * 60.0 / BPM / 960.0;
/// A sixteenth at 240 bpm, in seconds.
const SIXTEENTH_S: f64 = 60.0 / BPM / 4.0;

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

/// Steps in real time (the router thread delivers, the silent clock runs) until `done`.
fn until(h: &mut Harness<'_, SoundApp>, what: &str, done: impl Fn(&SoundApp) -> bool) {
    let start = Instant::now();
    while !done(h.state()) {
        assert!(start.elapsed() < Duration::from_secs(10), "never: {what} ({})", h.state().ui.status);
        step(h);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-stage7b-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("music")).unwrap();
    dir
}

fn float_wav(buf: &AudioBuffer) -> Vec<u8> {
    encode(buf, &EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None }).unwrap()
}

/// A half-second stereo "drum loop" in the scratch folder.
fn drum_loop(dir: &Path) -> PathBuf {
    let frames = RATE as usize / 2;
    let tone = |ch: usize| (0..frames).map(|k| 0.37 * (k as f32 * 0.07 + ch as f32).sin()).collect();
    let path = dir.join("music").join("drums.wav");
    std::fs::write(&path, float_wav(&AudioBuffer { sample_rate: RATE, channels: vec![tone(0), tone(1)] })).unwrap();
    path
}

/// A fake Demucs: each stem a quarter of its input.
fn fake_demucs() -> StemRunner {
    Arc::new(move |input: &Path, out: &Path, _: &dyn Fn() -> bool| {
        let (_, mut buf) = decode(&std::fs::read(input).unwrap(), Some("wav")).unwrap();
        buf.channels.iter_mut().flatten().for_each(|x| *x *= 0.25);
        let dir = out.join("htdemucs").join("source");
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths: [PathBuf; 4] = Default::default();
        for (name, path) in STEMS.iter().zip(paths.iter_mut()) {
            *path = dir.join(format!("{name}.wav"));
            std::fs::write(&*path, float_wav(&buf)).unwrap();
        }
        Ok(paths)
    })
}

struct Rig {
    h: Harness<'static, SoundApp>,
    clock: MidiClock,
    bass: TrackId,
    drums: TrackId,
    dir: PathBuf,
}

/// The session: a Session view instrument column (column 4), a split drums clip, and a
/// `rolling_bass` part; a silent player; fake "DDJ" and "Keys" plugged and enabled.
fn rig() -> Rig {
    let dir = scratch();
    let mut e = soundcraft_engine::Engine::default();
    e.session_mut().tempo.set_tempo(0, BPM).unwrap();
    e.execute("live.setup", &json!({})).unwrap();
    e.execute("live.add_column", &json!({"kind": "instrument"})).unwrap();
    let before: Vec<TrackId> = e.session().tracks.iter().map(|t| t.id).collect();
    e.execute("file.import_audio", &json!({"path": drum_loop(&dir)})).unwrap();
    let drums = e.session().tracks.iter().map(|t| t.id).find(|id| !before.contains(id)).expect("the drums track");
    let clip = e.session().track(drums).unwrap().clips()[0].id;
    e.set_stems(StemCache::new(dir.join("cache")).with_free_space(0, Arc::new(|_: &Path| Some(u64::MAX))), fake_demucs());
    e.execute("dj.split_stems", &json!({"clip": clip.0})).unwrap();
    assert!(e.session().find_clip(clip).unwrap().1.stems.is_some(), "the drums clip is split");
    let part = e.execute("prod.new_part", &json!({"kind": "bass", "preset": "rolling_bass"})).unwrap();
    assert_eq!(part["preset"], "rolling_bass");
    let bass = TrackId(part["track"].as_u64().unwrap());

    let player = soundcraft_playback::Player::silent(e.session_arc());
    let mut app = SoundApp::new(e, Some(player), Services::default());
    let clock = MidiClock::new();
    let mut src = FakeSource::new(MidiClock::from_origin(clock.origin())).unwrap();
    src.plug("DDJ");
    src.plug("Keys");
    app.midi_host = Some(MidiHost::new(Box::new(src)));
    for p in ["DDJ", "Keys"] {
        app.run("midi.input_enable", json!({"name": p, "enabled": true})).unwrap();
    }
    let h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    let mut rig = Rig { h, clock, bass, drums, dir };
    steps(&mut rig.h, 4);
    rig
}

fn fake<'a>(h: &'a mut Harness<'_, SoundApp>) -> &'a mut FakeSource {
    h.state_mut().midi_host.as_mut().unwrap().source_mut().as_fake_mut().unwrap()
}

fn send(h: &mut Harness<'_, SoundApp>, port: &str, msg: MidiMessage, ns: u64) {
    assert!(fake(h).inject(port, msg, ns), "{port} is not live");
}

fn pos(app: &SoundApp) -> i64 {
    app.player.as_ref().unwrap().position()
}

fn peak(app: &SoundApp, t: TrackId) -> f32 {
    app.player.as_ref().unwrap().meters().tracks.get(&t).map_or(0.0, |m| m.peak[0].max(m.peak[1]))
}

fn loudest(app: &SoundApp) -> f32 {
    app.player.as_ref().unwrap().meters().tracks.values().map(|m| m.peak[0].max(m.peak[1])).fold(0.0, f32::max)
}

fn is_open(app: &SoundApp, name: &str) -> bool {
    app.midi_host.as_ref().unwrap().source().is_open(name)
}

/// The bassline: four bars of rolling sixteenths (the three after each beat), A, A, F, G.
/// `(sixteenth index from the start, pitch, velocity)`.
fn bassline() -> Vec<(u32, u8, u8)> {
    let roots = [33, 33, 29, 31];
    let mut notes = Vec::new();
    for (bar, root) in roots.into_iter().enumerate() {
        for beat in 0..4u32 {
            for sub in 1..4u32 {
                let at = bar as u32 * 16 + beat * 4 + sub;
                notes.push((at, root + if sub == 3 { 12 } else { 0 }, if sub == 1 { 104 } else { 84 }));
            }
        }
    }
    notes
}

/// Presses the mapped pad (DDJ note 37 ch 10, mapped to Window › Transport): its command runs,
/// and for 400 ms nothing sounds on the bass.
fn pad_is_silent(h: &mut Harness<'_, SoundApp>, clock: &MidiClock, bass: TrackId, what: &str) {
    let shown = h.state().ui.show_transport;
    send(h, "DDJ", MidiMessage::NoteOn { ch: 9, note: 37, vel: 120 }, clock.now_ns());
    until(h, &format!("{what} runs its command"), |a| a.ui.show_transport != shown);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        step(h);
        assert!(peak(h.state(), bass) < 1e-5, "{what}: the mapped note sounded on the bass");
        std::thread::sleep(Duration::from_millis(5));
    }
    send(h, "DDJ", MidiMessage::NoteOff { ch: 9, note: 37, vel: 0 }, clock.now_ns());
    steps(h, 2);
}

#[test]
fn stage_7b_end_to_end_with_fake_devices() {
    let Rig { mut h, clock, bass, drums, dir } = rig();
    assert_eq!(h.state().ui.midi_inputs.len(), 2, "both enabled");
    assert!(is_open(h.state(), "DDJ") && is_open(h.state(), "Keys"));

    // --- Arm the bass part and record a 4-bar bassline from Keys. ---
    h.state_mut().run("mix.record_arm", json!({"tracks": [bass.0], "value": true})).unwrap();
    steps(&mut h, 2);
    assert_eq!(h.state().player.as_ref().unwrap().midi_target(), Some(bass), "the armed bass part plays");
    h.state_mut().run("transport.record", json!({})).unwrap();
    until(&mut h, "recording", |a| a.engine.transport.recording && pos(a) > 10_000);
    assert!(h.state().midi_take.is_some(), "a MIDI take: {}", h.state().ui.status);
    let line = bassline();
    // Played like a keyboard: each message stamped when it is sent, gates at 2/3 of a sixteenth.
    let t0 = Instant::now() + Duration::from_millis(100);
    let mut sched: Vec<(Instant, MidiMessage)> = Vec::new();
    for &(at, pitch, vel) in &line {
        let on = t0 + Duration::from_secs_f64(f64::from(at) * SIXTEENTH_S);
        sched.push((on, MidiMessage::NoteOn { ch: 0, note: pitch, vel }));
        sched.push((on + Duration::from_secs_f64(SIXTEENTH_S * 2.0 / 3.0), MidiMessage::NoteOff { ch: 0, note: pitch, vel: 0 }));
    }
    sched.sort_by_key(|(t, _)| *t);
    for (when, msg) in sched {
        while Instant::now() < when {
            std::thread::sleep(Duration::from_micros(500));
        }
        send(&mut h, "Keys", msg, clock.now_ns());
    }
    let midi_events = |a: &SoundApp| a.midi_take.as_ref().map_or(0, |t| t.events.iter().filter(|e| e.msg().is_some()).count());
    until(&mut h, "every message is captured", |a| midi_events(a) >= line.len() * 2);
    let captured: Vec<_> = h.state().midi_take.as_ref().unwrap().events.clone();
    assert!(!captured.iter().any(|e| matches!(e.event, CaptureEvent::Jump { .. })), "one pass");
    h.state_mut().run("transport.stop", json!({})).unwrap();
    steps(&mut h, 2);
    assert!(!h.state().engine.transport.recording);
    let status = h.state().ui.status.clone();
    assert!(status.contains(&format!("Recorded {} MIDI note(s)", line.len())), "{status}");
    assert!(!status.contains("dropped"), "{status}");
    assert_eq!(h.state().engine.undo_label(), Some("Record MIDI"));

    // What the player played, from its capture: (on sample, off sample, pitch, velocity).
    let mut played = Vec::new();
    let mut open: Option<(i64, u8, u8)> = None;
    for e in &captured {
        match e.msg() {
            Some(MidiMessage::NoteOn { note, vel, .. }) if vel > 0 => {
                assert!(open.is_none(), "the line is monophonic");
                open = Some((e.at, note, vel));
            }
            Some(MidiMessage::NoteOff { note, .. } | MidiMessage::NoteOn { note, vel: 0, .. }) => {
                let (on, p, v) = open.take().expect("an off after its on");
                assert_eq!(p, note);
                played.push((on, e.at, p, v));
            }
            _ => {}
        }
    }
    assert_eq!(played.len(), line.len());
    // The played line is the one sent: pitches and velocities exactly, the timing as played live
    // (stamped on arrival, so within a few ms of the schedule; relative to the first note).
    for (i, (&(at, pitch, vel), &(on, _, p, v))) in line.iter().zip(&played).enumerate() {
        assert_eq!((p, v), (pitch, vel), "note {i}");
        let want = f64::from(at - line[0].0) * SIXTEENTH_S * f64::from(RATE);
        let got = (on - played[0].0) as f64;
        assert!((got - want).abs() < 0.025 * f64::from(RATE), "note {i} {} ms off the schedule", (got - want) / 48.0);
    }
    // The take is a clip whose notes are what was played, within one tick.
    let clips = h.state().engine.session().track(bass).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 1, "{status}");
    let take = &clips[0];
    assert_eq!(take.start, 0, "from the bar line at or before the start");
    let ClipContent::Midi { sequence } = &take.content else { panic!("not a MIDI clip") };
    assert_eq!(sequence.notes.len(), played.len());
    for (i, (n, &(on, off, p, v))) in sequence.notes.iter().zip(&played).enumerate() {
        assert_eq!((n.pitch, n.velocity), (p, v), "note {i}");
        let start = (on - take.start) as f64 / SAMPLES_PER_TICK;
        let len = (off - on) as f64 / SAMPLES_PER_TICK;
        assert!((n.start as f64 - start).abs() <= 1.0, "note {i} start {} vs played {start:.2} ticks", n.start);
        assert!((n.length as f64 - len).abs() <= 1.0, "note {i} length {} vs played {len:.2} ticks", n.length);
    }

    // --- Learn a CC from the DDJ onto the drums clip's drums stem fader, and ride it. ---
    let stem = Target::StemFader {
        track: soundcraft_ui_egui::midi_learn::TrackRef::of(h.state().engine.session().track(drums).unwrap()),
        stem: "drums".into(),
    };
    let drums_name = h.state().engine.session().track(drums).unwrap().name.clone();
    h.state_mut().run("midi.learn", json!({"target": {"kind": "stem_fader", "track": drums_name, "stem": "drums"}})).unwrap();
    steps(&mut h, 1);
    assert!(h.state().midi_learn.learning.is_some());
    let cc = |value| MidiMessage::ControlChange { ch: 0, cc: 7, value };
    send(&mut h, "DDJ", cc(20), clock.now_ns());
    until(&mut h, "learned", |a| !a.ui.midi_mappings.is_empty());
    let m = h.state().ui.midi_mappings[0].clone();
    assert_eq!((m.device.name.as_str(), m.matcher, &m.target), ("DDJ", Matcher::Cc { ch: 0, cc: 7 }, &stem));
    // The first messages tell a knob from an encoder and move nothing.
    send(&mut h, "DDJ", cc(24), clock.now_ns());
    send(&mut h, "DDJ", cc(29), clock.now_ns());
    until(&mut h, "detected", |a| a.midi_learn.detect.is_none());
    assert_eq!(h.state().ui.midi_mappings[0].mode, Mode::Absolute);
    let stem_db = |a: &SoundApp| a.engine.session().track(drums).unwrap().mixer.stem_gain_db[1];
    assert_eq!(stem_db(h.state()), 0.0);
    // The stem fader's taper: +6 dB at the top. 0 dB sits at `unity` of the travel: picked up
    // once the controller crosses it.
    let top = soundcraft_model::fader_db_to_pos(6.0);
    let db_of = |v: u8| soundcraft_model::fader_pos_to_db(f32::from(v) / 127.0 * top).clamp(-144.0, 6.0);
    let unity = (soundcraft_model::fader_db_to_pos(0.0) / top * 127.0) as u8;
    let undo = h.state().engine.undo_history().len();
    let gesture: Vec<u8> = (unity - 12..=unity + 6).step_by(3).collect();
    for &v in &gesture {
        send(&mut h, "DDJ", cc(v), clock.now_ns());
        step(&mut h);
    }
    let last = *gesture.last().unwrap();
    until(&mut h, "the stem fader follows", |a| (stem_db(a) - db_of(last)).abs() < 1e-3);
    assert!(stem_db(h.state()) > 0.0, "above unity: {}", stem_db(h.state()));
    assert_eq!(h.state().engine.undo_history().len(), undo + 1, "one gesture, one undo step");
    assert_eq!(h.state().engine.undo_label(), Some("Set Stem Fader"));
    assert!(h.state_mut().engine.undo());
    assert_eq!(stem_db(h.state()), 0.0, "undone");
    assert_eq!(h.state().engine.undo_history().len(), undo);

    // --- Learn a pad from the DDJ onto a Session view slot holding the take; press it. ---
    h.state_mut().engine.execute("live.slot_from_clip", &json!({"column": 4, "slot": 0, "clip": take.id.0})).unwrap();
    h.state_mut().run("live.quantise", json!({"value": "1"})).unwrap();
    h.state_mut().run("midi.learn", json!({"target": {"kind": "slot_launch", "column": 4, "slot": 0}})).unwrap();
    steps(&mut h, 1);
    send(&mut h, "DDJ", MidiMessage::NoteOn { ch: 9, note: 36, vel: 100 }, clock.now_ns());
    send(&mut h, "DDJ", MidiMessage::NoteOff { ch: 9, note: 36, vel: 0 }, clock.now_ns());
    until(&mut h, "the pad is learned", |a| a.ui.midi_mappings.len() == 2);
    let pad = h.state().ui.midi_mappings[1].clone();
    assert_eq!((pad.matcher, pad.target), (Matcher::Note { ch: 9, note: 36 }, Target::SlotLaunch { column: 4, slot: 0 }));
    steps(&mut h, 3);
    assert!(h.state().player.as_ref().unwrap().live_state()[4].playing.is_none(), "learning launches nothing");
    h.state_mut().run("transport.play", json!({})).unwrap();
    until(&mut h, "into a bar", |a| a.is_playing() && pos(a) % 48_000 > 6_000 && pos(a) % 48_000 < 24_000);
    let pressed = pos(h.state());
    send(&mut h, "DDJ", MidiMessage::NoteOn { ch: 9, note: 36, vel: 100 }, clock.now_ns());
    let live = |a: &SoundApp| a.player.as_ref().unwrap().live_state()[4];
    until(&mut h, "the launch is queued", |a| live(a).queued.is_some() || live(a).playing.is_some());
    let q = live(h.state());
    let (row, at) = q.queued.expect("queued for the bar line, not launched at once");
    assert_eq!(row, Some(0));
    assert_eq!(at % 48_000, 0, "on a bar line");
    assert!(at > pressed, "the next one");
    send(&mut h, "DDJ", MidiMessage::NoteOff { ch: 9, note: 36, vel: 0 }, clock.now_ns());
    until(&mut h, "the slot plays", |a| live(a).playing.is_some());
    assert_eq!(live(h.state()).playing, Some((0, at)), "launched on the bar line it was queued for");
    assert_eq!(live(h.state()).late, 0);
    h.state_mut().run("transport.stop", json!({})).unwrap();
    until(&mut h, "stopped", |a| !a.is_playing() && live(a).playing.is_none());

    // --- Unplug the DDJ while it holds a note: disconnected, the note released; replug. ---
    h.state_mut().run("window.midi_devices", json!({"value": true})).unwrap();
    steps(&mut h, 2);
    until(&mut h, "the transport is quiet", |a| loudest(a) < 1e-5);
    let backend = h.state().ui.midi_inputs.iter().find(|p| p.raw_name == "DDJ").unwrap().backend_id.clone();
    send(&mut h, "DDJ", MidiMessage::NoteOn { ch: 0, note: 45, vel: 110 }, clock.now_ns());
    until(&mut h, "the held note sounds on the bass", |a| peak(a, bass) > 1e-3);
    assert!(fake(&mut h).unplug("DDJ"));
    until(&mut h, "the unplug is noticed", |a| a.ui.status.contains("DDJ disconnected – notes released"));
    steps(&mut h, 1);
    assert!(h.query_by_label("disconnected").is_some(), "shown as disconnected");
    assert!(!is_open(h.state(), "DDJ"));
    until(&mut h, "the held note is released", |a| peak(a, bass) < 1e-5);
    assert!(is_open(h.state(), "Keys"), "the other device is untouched");
    fake(&mut h).plug_device("DDJ", &backend);
    until(&mut h, "the replug reopens it", |a| is_open(a, "DDJ"));
    assert_eq!(h.state().ui.status, "DDJ reconnected");
    steps(&mut h, 1);
    assert!(h.query_by_label("disconnected").is_none());
    assert_eq!(h.state().ui.midi_mappings.len(), 2, "the mappings survive the replug");
    send(&mut h, "DDJ", cc(5), clock.now_ns());

    // --- A mapped pad is a control, not music: it never sounds on the bass (the live target). ---
    h.state_mut()
        .run(
            "midi.map",
            json!({"device": "DDJ", "matcher": {"type": "note", "ch": 9, "note": 37}, "target": {"kind": "command", "id": "window.transport"}}),
        )
        .unwrap();
    steps(&mut h, 2);
    until(&mut h, "quiet", |a| peak(a, bass) < 1e-5);
    pad_is_silent(&mut h, &clock, bass, "the pad");

    // --- An audio device change: a new player, which still filters the mapped pad. ---
    let old = h.state().player.as_ref().unwrap().id();
    let fresh = soundcraft_playback::Player::silent(h.state().engine.session_arc());
    h.state_mut().player = Some(fresh);
    steps(&mut h, 3);
    assert_ne!(h.state().player.as_ref().unwrap().id(), old);
    assert_eq!(h.state().player.as_ref().unwrap().midi_target(), Some(bass), "the new player plays the bass");
    pad_is_silent(&mut h, &clock, bass, "the pad after the device change");
    // An unmapped key still plays the bass on the new player.
    send(&mut h, "DDJ", MidiMessage::NoteOn { ch: 0, note: 45, vel: 110 }, clock.now_ns());
    until(&mut h, "an unmapped key sounds on the new player", |a| peak(a, bass) > 1e-3);
    send(&mut h, "DDJ", MidiMessage::NoteOff { ch: 0, note: 45, vel: 0 }, clock.now_ns());
    until(&mut h, "released", |a| peak(a, bass) < 1e-5);

    // --- The DDJ plugged into another USB socket: a new backend id, the same device. ---
    assert!(fake(&mut h).unplug("DDJ"));
    until(&mut h, "the unplug is noticed", |a| !is_open(a, "DDJ"));
    fake(&mut h).plug_device("DDJ", "usb:another-socket");
    until(&mut h, "found on the new port", |a| is_open(a, "DDJ"));
    assert!(h.state().ui.status.contains("DDJ found on a new USB port; settings moved"), "{}", h.state().ui.status);
    let ddj: Vec<_> = h.state().ui.midi_inputs.iter().filter(|p| p.raw_name == "DDJ").cloned().collect();
    assert_eq!((ddj.len(), ddj[0].backend_id.as_str()), (1, "usb:another-socket"));
    assert!(h.state().ui.midi_mappings.iter().all(|m| m.device.backend_id == "usb:another-socket"), "every mapping followed");
    pad_is_silent(&mut h, &clock, bass, "the pad on the new socket");

    // --- Nothing is left sounding. ---
    steps(&mut h, 2);
    until(&mut h, "silence", |a| loudest(a) < 1e-5);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        step(&mut h);
        assert!(loudest(h.state()) < 1e-5, "something sounds again");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!h.state().is_playing());
    assert!(h.state().player.as_ref().unwrap().live_state().iter().all(|c| c.playing.is_none() && c.queued.is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}
