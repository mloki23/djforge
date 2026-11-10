//! MIDI takes end to end: live MIDI from a fake port (no real device) or the on-screen
//! keyboard's path, played by the audio thread (driven by hand, `harness`), captured, split into
//! passes and written by the engine's `midi.record_take` / `live.slot_from_take`, checked
//! against where the instrument played each note.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use soundcraft_engine::Engine;
use soundcraft_midi_io::MidiMessage;
use soundcraft_mix::LiveCmd;
use soundcraft_model::{ChannelFormat, Clip, ClipContent, ClipId, LiveColumn, Session, TrackKind};
use soundcraft_playback::harness::{Played, Rig};
use soundcraft_playback::{Captured, MidiCapture, TakePass, midi_in, take_passes};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER as Q};

fn on(note: u8) -> MidiMessage {
    MidiMessage::NoteOn { ch: 0, note, vel: 100 }
}

fn off(note: u8) -> MidiMessage {
    MidiMessage::NoteOff { ch: 0, note, vel: 0 }
}

/// A rig on an instrument track of `session` (a new one when it has none).
fn rig(mut s: Session) -> Rig {
    let t = s.tracks.iter().find(|t| t.kind == TrackKind::Instrument).map(|t| t.id);
    let t = t.unwrap_or_else(|| s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Lead")));
    s.track_mut(t).unwrap().mixer.record_arm = true;
    Rig::new(s, t).expect("the fake MIDI source")
}

fn drained(c: &mut MidiCapture) -> Vec<Captured> {
    let mut v = Vec::new();
    c.drain_into(&mut v);
    v
}

/// The passes as `midi.record_take` takes them.
fn passes_json(passes: &[TakePass]) -> Value {
    Value::Array(
        passes
            .iter()
            .map(|p| {
                let events: Vec<Value> = p
                    .events
                    .iter()
                    .map(|(at, m)| {
                        let [status, data1, data2] = midi_in::raw(*m);
                        json!({"at": at, "status": status, "data1": data1, "data2": data2})
                    })
                    .collect();
                json!({"start": p.start, "end": p.end, "events": events})
            })
            .collect(),
    )
}

/// A clip on any of the track's playlists.
fn clip(e: &Engine, id: &Value) -> Clip {
    let id = ClipId(id.as_u64().unwrap());
    e.session().tracks.iter().flat_map(|t| &t.playlists).flat_map(|p| &p.clips).find(|c| c.id == id).cloned().unwrap()
}

fn notes(c: &Clip) -> Vec<(u8, i64, i64)> {
    match &c.content {
        ClipContent::Midi { sequence } => sequence.notes.iter().map(|n| (n.pitch, n.start, n.length)).collect(),
        _ => panic!("not MIDI"),
    }
}

/// A take played on the fake port under a tempo ramp becomes a clip over the time played whose
/// note ticks are where the instrument played them.
#[test]
fn a_take_played_under_a_tempo_ramp_becomes_a_clip_of_the_ticks_heard() {
    let mut s = Session::default();
    for b in 0..16 {
        s.tempo.set_tempo(b * Q, 128.0 + 2.0 * b as f64).unwrap();
    }
    let mut r = rig(s);
    let mut cap = r.capture(1024);
    r.recording(true);
    let start = 30_000;
    r.play(start, None);
    r.callback();
    let mut pitches = Vec::new();
    for (i, pitch) in [60u8, 62, 64, 65, 67].into_iter().enumerate() {
        for _ in 0..(37 + 11 * i) {
            r.callback();
        }
        assert!(r.port(on(pitch)));
        for _ in 0..(20 + 3 * i) {
            r.callback();
        }
        assert!(r.port(off(pitch)));
        pitches.push(pitch);
    }
    // The last note is still held at the stop.
    r.callback();
    assert!(r.port(on(72)));
    for _ in 0..30 {
        r.callback();
    }
    let end = r.pos();
    let passes = take_passes(start, end, &drained(&mut cap));
    assert_eq!((passes.len(), passes[0].events.len()), (1, 11));
    let mut e = Engine::new((*r.session()).clone());
    let res = e.execute("midi.record_take", &json!({"track": r.track.0, "passes": passes_json(&passes)})).unwrap();
    let s = e.session();
    let tick = |at: i64| s.tempo.samples_to_ticks_f(at, s.sample_rate).round() as i64;
    let c = clip(&e, &res["clip"]);
    assert_eq!((c.start, c.end()), (start, end), "over the time played");
    let base = s.tempo.samples_to_ticks(start, s.sample_rate);
    let mut want: Vec<(u8, i64, i64)> = pitches
        .iter()
        .map(|p| {
            let (a, b) = (r.played(Played::On(*p))[0], r.played(Played::Off(*p))[0]);
            (*p, tick(a) - base, tick(b) - tick(a))
        })
        .collect();
    let held = r.played(Played::On(72))[0];
    want.push((72, tick(held) - base, tick(end) - tick(held)));
    assert_eq!(notes(&c), want);
    assert_eq!(res["held_cut"], 1);
}

/// Loop recording over a bar: each pass is its own take, its notes paired within it (a note held
/// over the wrap is cut there), one playlist each; with MIDI Merge, all in one clip.
#[test]
fn loop_passes_are_kept_apart_one_take_each() {
    let mut r = rig(Session::default());
    let mut cap = r.capture(1024);
    r.recording(true);
    // 120 bpm: bar 1 is 0..96 000.
    let looped = Range::new(0, 96_000);
    r.play(0, Some(looped));
    // Renders until `to` samples of loop passes have gone by (counting the wraps).
    let (mut wrapped, mut last) = (0, 0);
    let mut step = |r: &mut Rig, to: Samples| {
        while wrapped * 96_000 + r.pos() < to {
            r.callback();
            if r.pos() < last {
                wrapped += 1;
            }
            last = r.pos();
        }
    };
    // Pass 1: a note at 12 000; held from 80 000 over the wrap, released at 6 000 of pass 2.
    step(&mut r, 12_000);
    r.keyboard(on(60));
    step(&mut r, 20_000);
    r.keyboard(off(60));
    step(&mut r, 80_000);
    r.keyboard(on(62));
    step(&mut r, 96_000 + 6_000);
    r.keyboard(off(62));
    // Pass 2: a note at 48 000.
    step(&mut r, 96_000 + 48_000);
    r.keyboard(on(64));
    step(&mut r, 96_000 + 52_000);
    r.keyboard(off(64));
    // Pass 3, stopped at 30 000.
    step(&mut r, 2 * 96_000 + 30_000);
    let end = r.pos();
    let got = drained(&mut cap);
    let passes = take_passes(0, end, &got);
    assert_eq!(passes.iter().map(|p| (p.start, p.end)).collect::<Vec<_>>(), [(0, 96_000), (0, 96_000), (0, end)]);
    let s = (*r.session()).clone();
    let mut e = Engine::new(s);
    let res = e.execute("midi.record_take", &json!({"track": r.track.0, "passes": passes_json(&passes), "on_overlap": "new_playlist"})).unwrap();
    let takes = res["takes"].as_array().unwrap();
    assert_eq!(takes.len(), 2, "the last pass played nothing");
    let first = clip(&e, &takes[0]["clip"]);
    let second = clip(&e, &takes[1]["clip"]);
    let pitches = |c: &Clip| notes(c).iter().map(|n| n.0).collect::<Vec<u8>>();
    assert_eq!((pitches(&first), pitches(&second)), (vec![60, 62], vec![64]), "the held note's release in pass 2 pairs with nothing");
    let held = notes(&first)[1];
    let s = e.session();
    assert_eq!(held.1 + held.2, s.tempo.samples_to_ticks(96_000, s.sample_rate), "cut at the wrap");
    let tr = s.track(r.track).unwrap();
    assert_eq!((tr.playlists.len(), tr.active_playlist), (2, 1), "one playlist a pass");
    // MIDI Merge: both passes in one clip.
    let mut e = Engine::new((*r.session()).clone());
    e.execute("options.midi_merge", &json!({"value": true})).unwrap();
    let res = e.execute("midi.record_take", &json!({"track": r.track.0, "passes": passes_json(&passes), "on_overlap": "new_playlist"})).unwrap();
    assert_eq!(pitches(&clip(&e, &res["clip"])), [60, 64, 62]);
    assert_eq!(e.session().track(r.track).unwrap().playlists.len(), 1);
}

/// A locate during a take: what was played before and after it lands where it was played.
#[test]
fn a_seek_during_a_take_keeps_each_stretch_where_it_played() {
    let mut r = rig(Session::default());
    let mut cap = r.capture(1024);
    r.recording(true);
    r.play(0, None);
    r.render_to(10_000);
    r.keyboard(on(60));
    r.render_to(20_000);
    r.locate(500_000);
    r.callback();
    r.keyboard(off(60));
    r.keyboard(on(62));
    r.render_to(510_000);
    r.keyboard(off(62));
    r.render_to(520_000);
    let end = r.pos();
    let passes = take_passes(0, end, &drained(&mut cap));
    assert_eq!(passes.len(), 2);
    assert_eq!((passes[1].start, passes[1].end), (500_000, end));
    let mut e = Engine::new((*r.session()).clone());
    let res = e.execute("midi.record_take", &json!({"track": r.track.0, "passes": passes_json(&passes)})).unwrap();
    let takes = res["takes"].as_array().unwrap();
    let spans: Vec<(Samples, Samples)> = takes.iter().map(|t| (t["start"].as_i64().unwrap(), t["end"].as_i64().unwrap())).collect();
    assert_eq!(spans, [(0, passes[0].end), (500_000, end)]);
    let (a, b) = (clip(&e, &takes[0]["clip"]), clip(&e, &takes[1]["clip"]));
    assert_eq!((notes(&a).len(), notes(&b).len()), (1, 1));
    assert_eq!(notes(&a)[0].1 + notes(&a)[0].2, e.session().tempo.samples_to_ticks(passes[0].end, e.session().sample_rate), "held to the seek");
    assert_eq!(notes(&b)[0].0, 62, "the note-off after the seek pairs with nothing");
}

/// A Session view slot recording, as the app runs it: capture from the boundary, write the slot
/// a launch margin before the end and launch it there, then write in the notes played after the
/// first write (one undo group). The slot's first note sounds exactly where the recording
/// ended, and the late write cuts nothing sounding.
#[test]
fn a_slot_recording_launches_where_it_ends_and_loops_on_seamlessly() {
    let mut s = Session::default();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Lead"));
    s.live.columns = vec![LiveColumn::new(t)];
    let mut r = Rig::new(s, t).unwrap();
    let mut cap = r.capture(1024);
    r.capture_while_playing(true);
    r.play(0, None);
    r.callback();
    // 120 bpm: bar 2 at 96 000, a one-bar recording ends at bar 3 (192 000).
    let (b, end) = (96_000, 192_000);
    r.render_to(b);
    r.keyboard(on(60));
    r.render_to(b + 12_000);
    r.keyboard(off(60));
    r.render_to(b + 48_000);
    assert!(r.port(on(62)));
    r.render_to(b + 60_000);
    assert!(r.port(off(62)));
    let mut e = Engine::new((*r.session()).clone());
    let margin = soundcraft_engine::cmd::live_launch_margin(e.session(), 256);
    r.render_to(end - 2 * margin);
    let steps = e.undo_history().len();
    e.begin_undo_group("Record Slot Take");
    let mut events: Vec<Captured> = drained(&mut cap);
    let write = |e: &mut Engine, ev: &[Captured], replace: bool| {
        let passes = take_passes(b, end, ev);
        let p = json!({"column": 0, "slot": 0, "start": b, "bars": 1, "events": passes_json(&passes)[0]["events"], "replace": replace});
        e.execute("live.slot_from_take", &p).unwrap()
    };
    let first = write(&mut e, &events, false);
    assert_eq!(first["notes"], 2);
    r.update(e.session_arc());
    r.live(LiveCmd::Launch { column: 0, slot: 0, at: end });
    // A note played in the last moments, after the first write.
    r.render_to(end - margin / 2);
    r.keyboard(on(64));
    r.render_to(end - margin / 4);
    r.keyboard(off(64));
    r.render_to(end + 512);
    events.extend(drained(&mut cap).into_iter().filter(|c| c.at < end));
    let second = write(&mut e, &events, true);
    assert_eq!(second["notes"], 3);
    e.commit_undo_group();
    assert_eq!(e.undo_history().len(), steps + 1, "one undo step");
    r.update(e.session_arc());
    r.render_to(end + 96_000 + 1_000);
    assert_eq!(r.played(Played::On(60)), [b, end, end + 96_000], "live on the boundary, then the slot from where the recording ended");
    // Released where it was played to (to the tick: 25 samples at 120 bpm), not cut early.
    let held = r.played(Played::Off(60))[0] - b;
    let off60: Vec<Samples> = r.played(Played::Off(60)).into_iter().filter(|x| *x > end).collect();
    assert_eq!(off60.len(), 1, "{off60:?}");
    assert!((off60[0] - end - held).abs() <= 25, "not cut by the late write: {off60:?}, held {held}");
    let on64 = r.played(Played::On(64));
    assert_eq!(on64.len(), 2, "{on64:?}");
    assert!((on64[1] - on64[0] - 96_000).abs() <= 25, "the late note loops on too: {on64:?}");
    assert_eq!(r.live_late(), 0, "the launch arrived in time");
    assert_eq!(r.live_playing(0), Some((0, end)));
}
