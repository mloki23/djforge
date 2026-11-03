//! MIDI slots on an instrument column: notes land on the launch sample and the bar lines of
//! every pass (under tempo steps too), notes crossing the seam are cut there, nothing is left
//! sounding after a stop, a switch, a transport stop or a locate, and the track's arrangement
//! notes are muted while a slot plays.
//!
//! The instrument is a recorder: it logs every note-on, note-off and all-notes-off at its
//! timeline sample.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_midi::{Note, Sequence};
use soundcraft_mix::{INSTRUMENT_SLOT, LiveCmd, MixEngine, PreparedInstance};
use soundcraft_model::{ChannelFormat, Clip, Insert, LiveColumn, MidiSlot, Session, TrackId, TrackKind};
use soundcraft_time::{Samples, TICKS_PER_QUARTER};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

const BLOCK: usize = 512;
const Q: i64 = TICKS_PER_QUARTER;
const KICK: u8 = 36;
const LONG: u8 = 60;
const CROSS: u8 = 50;
const ARR: u8 = 70;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ev {
    On(Samples, u8),
    Off(Samples, u8),
    AllOff(Samples),
}

/// Logs what the instrument is sent, at the timeline sample of the block it was sent for.
struct Recorder {
    inner: Box<dyn Plugin>,
    pos: Arc<AtomicI64>,
    /// Each event with the number of the block it was sent for (blocks rendered before it).
    log: Arc<Mutex<Vec<(u64, Ev)>>>,
    blocks: u64,
}

impl Recorder {
    fn push(&self, e: Ev) {
        self.log.lock().unwrap().push((self.blocks, e));
    }
}

impl Plugin for Recorder {
    fn info(&self) -> &'static soundcraft_dsp::PluginInfo {
        self.inner.info()
    }
    fn prepare(&mut self, sr: f32, mb: usize, ch: usize) {
        self.inner.prepare(sr, mb, ch);
    }
    fn reset(&mut self) {}
    fn set_param(&mut self, id: &str, v: f32) -> bool {
        self.inner.set_param(id, v)
    }
    fn param(&self, id: &str) -> Option<f32> {
        self.inner.param(id)
    }
    fn process(&mut self, _io: &mut [Vec<f32>], _frames: usize) {
        self.blocks += 1;
    }
    fn note_on(&mut self, offset: usize, note: u8, _velocity: u8) {
        let at = self.pos.load(Ordering::Relaxed) + offset as i64;
        self.push(Ev::On(at, note));
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        let at = self.pos.load(Ordering::Relaxed) + offset as i64;
        self.push(Ev::Off(at, note));
    }
    fn all_notes_off(&mut self) {
        let at = self.pos.load(Ordering::Relaxed);
        self.push(Ev::AllOff(at));
    }
}

fn note(pitch: u8, start: i64, length: i64) -> Note {
    Note { pitch, velocity: 100, release_velocity: 64, channel: 0, start, length }
}

/// A one-bar drum loop: a kick on every beat.
fn kicks() -> MidiSlot {
    let notes = (0..4).map(|b| note(KICK, b * Q, Q / 4)).collect();
    MidiSlot { name: "Kicks".into(), color: [0; 3], sequence: Sequence { notes, ctrls: Vec::new() }, length_beats: 4.0, gain_db: 0.0 }
}

/// A one-bar loop of long notes: one held three beats, one from beat 3.5 for two beats (past
/// the seam: cut there), and one starting past the loop's end (never played).
fn longs() -> MidiSlot {
    let notes = vec![note(LONG, 0, 3 * Q), note(CROSS, 7 * Q / 2, 2 * Q), note(KICK, 5 * Q, Q)];
    MidiSlot { name: "Longs".into(), color: [0; 3], sequence: Sequence { notes, ctrls: Vec::new() }, length_beats: 4.0, gain_db: 0.0 }
}

struct Rig {
    s: Session,
    eng: MixEngine,
    pos: Arc<AtomicI64>,
    log: Arc<Mutex<Vec<(u64, Ev)>>>,
    track: TrackId,
}

/// A 120 bpm session (stepped up by 2 bpm every beat from beat 8 when `ramp`) with an
/// instrument column holding [`kicks`] and [`longs`], and an engine whose instrument records.
fn rig(ramp: bool) -> Rig {
    let mut s = Session::default();
    s.tempo.set_tempo(0, 120.0).unwrap();
    if ramp {
        for b in 8..60 {
            s.tempo.set_tempo(b * Q, 120.0 + 2.0 * (b - 7) as f64).unwrap();
        }
    }
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.rec"));
    let mut col = LiveColumn::new(t);
    col.slots[0] = Some(kicks().into());
    col.slots[1] = Some(longs().into());
    s.live.columns.push(col);
    let pos = Arc::new(AtomicI64::new(0));
    let log = Arc::new(Mutex::new(Vec::with_capacity(100_000)));
    let rec = Recorder { inner: soundcraft_dsp::create("gain").unwrap(), pos: Arc::clone(&pos), log: Arc::clone(&log), blocks: 0 };
    let mut eng = MixEngine::new(48_000.0, BLOCK);
    eng.set_external_instances(true);
    eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_SLOT, id: "clap:test.rec".into(), ch: 2, plugin: Box::new(rec) }]);
    Rig { s, eng, pos, log, track: t }
}

impl Rig {
    fn bar(&self, n: i64) -> Samples {
        self.s.tempo.tick_to_samples(self.s.tempo.bar_start_tick(n), self.s.sample_rate)
    }

    /// The sample of beat `beat` (from 0) of bar `n`.
    fn beat(&self, n: i64, beat: i64) -> Samples {
        self.s.tempo.tick_to_samples(self.s.tempo.bar_start_tick(n) + beat * Q, self.s.sample_rate)
    }

    /// Renders `[from, to)` in blocks (the last one short), handing each command to the engine before the block
    /// that starts at or after its send time.
    fn run(&mut self, from: Samples, to: Samples, cmds: &[(Samples, LiveCmd)]) {
        let mut out = vec![vec![0.0; BLOCK]; 2];
        let mut pos = from;
        let mut sent = 0;
        while pos < to {
            while let Some((_, c)) = cmds.get(sent).filter(|(at, _)| *at <= pos) {
                self.eng.live(*c);
                sent += 1;
            }
            let n = BLOCK.min((to - pos) as usize);
            self.pos.store(pos, Ordering::Relaxed);
            self.eng.render(&self.s, pos, n, &mut out);
            pos += n as i64;
        }
    }

    fn events(&self) -> Vec<Ev> {
        self.log.lock().unwrap().iter().map(|(_, e)| *e).collect()
    }

    /// The pitches still sounding after everything sent so far (see [`sounding`]).
    fn sounding(&self) -> Vec<u8> {
        sounding(&self.log.lock().unwrap())
    }

    fn ons(&self, pitch: u8) -> Vec<Samples> {
        self.events().iter().filter(|e| matches!(e, Ev::On(_, p) if *p == pitch)).map(|e| at(*e)).collect()
    }
}

fn at(e: Ev) -> Samples {
    match e {
        Ev::On(t, _) | Ev::Off(t, _) | Ev::AllOff(t) => t,
    }
}

/// The pitches still sounding after `events`, played as an instrument plays them: block by
/// block, each block's events in sample order (a note-off before a note-on on the same sample,
/// as the slots send them).
fn sounding(events: &[(u64, Ev)]) -> Vec<u8> {
    let mut ev = events.to_vec();
    ev.sort_by_key(|(block, e)| (*block, at(*e), !matches!(e, Ev::AllOff(_) | Ev::Off(..))));
    let ev = ev.into_iter().map(|(_, e)| e);
    let mut on = [0u32; 128];
    for e in ev {
        match e {
            Ev::On(_, p) => on[usize::from(p)] += 1,
            Ev::Off(_, p) => on[usize::from(p)] = 0,
            Ev::AllOff(_) => on = [0; 128],
        }
    }
    (0..128u8).filter(|p| on[usize::from(*p)] > 0).collect()
}

/// A drum slot launched for bar 3: its first kick lands exactly on the bar's sample and every
/// kick of the next three passes on its beat, at a constant tempo and under tempo steps.
#[test]
fn a_drum_slot_lands_on_the_bar_and_loops_on_the_beats() {
    for ramp in [false, true] {
        let mut r = rig(ramp);
        let launch = r.bar(3);
        r.run(0, r.bar(7), &[(0, LiveCmd::Launch { column: 0, slot: 0, at: launch })]);
        let want: Vec<Samples> = (3..7).flat_map(|n| (0..4).map(move |b| (n, b))).map(|(n, b)| r.beat(n, b)).collect();
        assert_eq!(r.ons(KICK).first(), Some(&launch), "ramp {ramp}: the first kick is on the bar sample");
        assert_eq!(r.ons(KICK), want, "ramp {ramp}: four passes, every kick on its beat");
        assert!(r.eng.live_state()[0].playing.is_some());
    }
}

/// A note running past the loop's end is cut on the seam sample, before the next pass starts
/// there; a note starting past the loop's end never plays.
#[test]
fn notes_crossing_the_seam_end_on_it() {
    let mut r = rig(true);
    r.run(0, r.bar(6), &[(0, LiveCmd::Launch { column: 0, slot: 1, at: r.bar(2) })]);
    let ev = r.events();
    for n in 3..6 {
        let seam = r.bar(n);
        assert!(ev.contains(&Ev::Off(seam, CROSS)), "cut on the seam of bar {n}");
        assert!(ev.contains(&Ev::On(seam, LONG)), "the next pass starts on it");
    }
    assert!(r.ons(KICK).is_empty(), "beyond the loop: never played");
    assert_eq!(r.ons(LONG).len(), 4);
}

/// Switching slots, stopping a column, stopping the transport and locating leave nothing
/// sounding: every note-on gets its note-off (or all-notes-off) on the sample that ends it.
#[test]
fn no_note_is_left_sounding() {
    // Switch mid-note, then stop mid-note: released on their samples.
    let mut r = rig(false);
    let (b2, b4) = (r.bar(2), r.bar(4));
    let switch = r.beat(4, 1);
    r.run(
        0,
        r.bar(6),
        &[
            (0, LiveCmd::Launch { column: 0, slot: 1, at: b2 }),
            (b2 + 10_000, LiveCmd::Launch { column: 0, slot: 0, at: switch }),
            (switch + 10_000, LiveCmd::Launch { column: 0, slot: 1, at: r.bar(5) }),
            (r.bar(5) + 10_000, LiveCmd::Stop { column: 0, at: r.beat(5, 1) }),
        ],
    );
    let ev = r.events();
    assert!(ev.contains(&Ev::Off(switch, LONG)), "the switch releases the held note on its sample");
    assert!(ev.contains(&Ev::On(b4, LONG)) && ev.contains(&Ev::On(switch, KICK)));
    assert!(ev.contains(&Ev::Off(r.beat(5, 1), LONG)), "the stop releases it on its sample");
    assert_eq!(r.sounding(), Vec::<u8>::new());
    assert!(r.eng.live_state()[0].playing.is_none());

    // The transport stops mid-note (as playback stops: the live layer clears, the engine resets).
    let mut r = rig(false);
    r.run(0, r.bar(3) + 5_000, &[(0, LiveCmd::Launch { column: 0, slot: 1, at: r.bar(2) })]);
    assert_eq!(r.sounding(), [LONG]);
    r.eng.live_clear();
    r.eng.reset();
    assert_eq!(r.sounding(), Vec::<u8>::new());

    // A locate mid-note: the loop plays on, re-phased, and what sounded is released.
    let mut r = rig(false);
    let from = r.bar(3) + 5_120;
    r.run(0, from, &[(0, LiveCmd::Launch { column: 0, slot: 1, at: r.bar(2) })]);
    let to = r.bar(9) - 7_000;
    r.eng.live_locate(from, to, false);
    r.eng.reset();
    let before = r.events().len();
    r.run(to, r.bar(11), &[]);
    let after = &r.events()[before..];
    assert!(after.contains(&Ev::On(r.bar(9), LONG)) && after.contains(&Ev::On(r.bar(10), LONG)), "the loop goes on from the new position");
    assert!(!after.iter().any(|e| matches!(e, Ev::On(t, _) if *t < r.bar(9))), "nothing from before the new position starts");
    r.run(r.bar(11), r.bar(11) + 2 * BLOCK as i64, &[(r.bar(11), LiveCmd::Stop { column: 0, at: r.bar(11) + 100 })]);
    assert_eq!(r.sounding(), Vec::<u8>::new());

    // A transport loop wrap mid-note.
    let mut r = rig(false);
    let (l0, l1) = (r.bar(2), r.bar(3) + 6_000);
    r.run(0, l1, &[(0, LiveCmd::Launch { column: 0, slot: 1, at: l0 })]);
    r.eng.live_locate(l1, l0, true);
    r.run(l0, l0 + 4 * BLOCK as i64, &[]);
    assert!(r.events().contains(&Ev::On(l0, LONG)), "the wrap plays the pass at the loop start");
    r.eng.live_clear();
    r.eng.reset();
    assert_eq!(r.sounding(), Vec::<u8>::new());
}

/// While a slot plays, the track's arrangement notes are muted: the one sounding is released
/// on the launch sample, none starts until the slot stops, and they play again after it.
#[test]
fn a_playing_slot_mutes_the_tracks_arrangement_notes() {
    let mut r = rig(false);
    // An arrangement clip: a long note over bar 2's last beats into bar 3, then a note per beat.
    let mut notes = vec![note(ARR, 2 * Q, 4 * Q)];
    notes.extend((8..32).map(|b| note(ARR, b * Q, Q / 2)));
    let cid = r.s.new_clip_id();
    let len = r.bar(9) - r.bar(1);
    let t = r.track;
    r.s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "arr", 0, len, Sequence { notes, ctrls: Vec::new() }));
    let (launch, stop) = (r.bar(2), r.bar(5));
    r.run(0, r.bar(8), &[(0, LiveCmd::Launch { column: 0, slot: 0, at: launch }), (r.bar(4), LiveCmd::Stop { column: 0, at: stop })]);
    let ev = r.events();
    assert!(ev.contains(&Ev::Off(launch, ARR)), "the arrangement note sounding at the launch ends there");
    let arr = r.ons(ARR);
    assert!(arr.iter().all(|t| *t < launch || *t >= stop), "no arrangement note starts while the slot plays: {arr:?}");
    assert_eq!(arr.iter().filter(|t| **t >= stop).count(), 12, "they play again once it stops");
    assert!(r.ons(KICK).iter().all(|t| (launch..stop).contains(t)));
    r.eng.live_clear();
    r.eng.reset();
    assert_eq!(r.sounding(), Vec::<u8>::new());
}

/// A MIDI slot on an audio column, or an audio slot on an instrument column, never plays.
#[test]
fn a_slot_of_the_wrong_kind_does_not_play() {
    let mut r = rig(false);
    let t = r.track;
    let audio = r.s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    r.s.live.columns.push(LiveColumn::new(audio));
    r.s.live.columns[1].slots[0] = Some(kicks().into());
    r.s.live.columns[0].slots[2] = Some(soundcraft_model::Slot::one_shot("a", [0; 3], soundcraft_model::SourceId(5)).into());
    r.run(0, r.bar(3), &[(0, LiveCmd::Launch { column: 1, slot: 0, at: r.bar(2) }), (0, LiveCmd::Launch { column: 0, slot: 2, at: r.bar(2) })]);
    assert!(r.events().iter().all(|e| !matches!(e, Ev::On(..))));
    assert!(r.eng.live_state().iter().all(|c| c.playing.is_none()));
    assert_eq!(r.s.live.column_track(0, &r.s.tracks), Some(t));
}

/// A playing slot cleared (a new snapshot) stops its column, and what it left sounding is
/// released at the next block; the same notes in a new snapshot play on.
#[test]
fn a_playing_slot_cleared_by_a_snapshot_is_released() {
    let mut r = rig(false);
    let mid = r.bar(2) + 6_144;
    r.run(0, mid, &[(0, LiveCmd::Launch { column: 0, slot: 1, at: r.bar(2) })]);
    assert_eq!(r.sounding(), [LONG]);
    let old = r.s.clone();
    let mut renamed = old.clone();
    if let Some(soundcraft_model::SlotContent::Midi(m)) = &mut renamed.live.columns[0].slots[1] {
        m.name = "renamed".into();
    }
    r.eng.live_snapshot(&old, &renamed, mid);
    assert!(r.eng.live_state()[0].playing.is_some(), "the same notes play on");
    let mut cleared = old.clone();
    cleared.live.columns[0].slots[1] = None;
    r.eng.live_snapshot(&old, &cleared, mid);
    r.s = cleared;
    r.run(mid, mid + BLOCK as i64, &[]);
    assert!(r.events().contains(&Ev::Off(mid, LONG)));
    assert_eq!(r.sounding(), Vec::<u8>::new());
    assert!(r.eng.live_state()[0].playing.is_none());
}
