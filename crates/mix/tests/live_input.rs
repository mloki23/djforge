//! Live MIDI input on the target track's instrument, on top of its arrangement notes and its
//! Session view slot: nothing cuts another source's note, nothing is left sounding after a
//! target change, a device going away, a panic or a stop, and a stopped transport still plays
//! the target.
//!
//! The instrument is a recorder: it logs every note-on, note-off, all-notes-off and controller
//! message at its timeline sample, in the order sent.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_dsp::Plugin;
use soundcraft_midi::{Note, Sequence};
use soundcraft_mix::{INSTRUMENT_INSTANCE, LIVE_INBOX, LiveCmd, LiveEvent, MixEngine, PreparedInstance};
use soundcraft_model::{ChannelFormat, Clip, Insert, LiveColumn, MidiSlot, Session, TrackId, TrackKind};
use soundcraft_time::{Samples, TICKS_PER_QUARTER};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

const BLOCK: usize = 512;
const Q: i64 = TICKS_PER_QUARTER;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Ev {
    On(Samples, u8),
    Off(Samples, u8),
    AllOff(Samples),
    Midi(Samples, [u8; 3]),
}

struct Recorder {
    inner: Box<dyn Plugin>,
    pos: Arc<AtomicI64>,
    log: Arc<Mutex<Vec<Ev>>>,
    blocks: Arc<AtomicI64>,
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
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }
    fn note_on(&mut self, offset: usize, note: u8, _velocity: u8) {
        let at = self.pos.load(Ordering::Relaxed) + offset as i64;
        self.log.lock().unwrap().push(Ev::On(at, note));
    }
    fn note_off(&mut self, offset: usize, note: u8) {
        let at = self.pos.load(Ordering::Relaxed) + offset as i64;
        self.log.lock().unwrap().push(Ev::Off(at, note));
    }
    fn all_notes_off(&mut self) {
        let at = self.pos.load(Ordering::Relaxed);
        self.log.lock().unwrap().push(Ev::AllOff(at));
    }
    fn midi(&mut self, offset: usize, msg: [u8; 3]) {
        let at = self.pos.load(Ordering::Relaxed) + offset as i64;
        self.log.lock().unwrap().push(Ev::Midi(at, msg));
    }
}

struct Rig {
    s: Session,
    eng: MixEngine,
    pos: Arc<AtomicI64>,
    log: Arc<Mutex<Vec<Ev>>>,
    blocks: Arc<AtomicI64>,
    track: TrackId,
}

fn note(pitch: u8, start: i64, length: i64) -> Note {
    Note { pitch, velocity: 100, release_velocity: 64, channel: 0, start, length }
}

/// A 120 bpm session with one instrument track (a recorder) and, with `notes`, an arrangement
/// MIDI clip of them over the first 8 bars.
fn rig(notes: Vec<Note>) -> Rig {
    let mut s = Session::default();
    s.tempo.set_tempo(0, 120.0).unwrap();
    let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, None);
    s.track_mut(t).unwrap().instrument = Some(Insert::new("clap:test.rec"));
    if !notes.is_empty() {
        let cid = s.new_clip_id();
        let len = s.tempo.tick_to_samples(s.tempo.bar_start_tick(9), s.sample_rate);
        s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::midi(cid, "arr", 0, len, Sequence { notes, ctrls: Vec::new() }));
    }
    let pos = Arc::new(AtomicI64::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));
    let blocks = Arc::new(AtomicI64::new(0));
    let rec = Recorder { inner: soundcraft_dsp::create("gain").unwrap(), pos: Arc::clone(&pos), log: Arc::clone(&log), blocks: Arc::clone(&blocks) };
    let mut eng = MixEngine::new(48_000.0, BLOCK);
    eng.set_external_instances(true);
    eng.adopt(vec![PreparedInstance { track: t, slot: INSTRUMENT_INSTANCE, id: "clap:test.rec".into(), ch: 2, plugin: Box::new(rec) }]);
    eng.set_live_target(Some(t));
    Rig { s, eng, pos, log, blocks, track: t }
}

fn on(source: u32, pitch: u8, offset: usize) -> LiveEvent {
    LiveEvent { source, offset, msg: [0x90, pitch, 100] }
}

fn off(source: u32, pitch: u8, offset: usize) -> LiveEvent {
    LiveEvent { source, offset, msg: [0x80, pitch, 0] }
}

impl Rig {
    fn sample(&self, tick: i64) -> Samples {
        self.s.tempo.tick_to_samples(tick, self.s.sample_rate)
    }

    /// Renders the block at `pos` with `events` queued before it.
    fn block(&mut self, pos: Samples, events: &[LiveEvent]) {
        for e in events {
            assert!(self.eng.live_input(*e));
        }
        let mut out = vec![vec![0.0; BLOCK]; 2];
        self.pos.store(pos, Ordering::Relaxed);
        self.eng.render(&self.s, pos, BLOCK, &mut out);
    }

    /// Renders blocks from `from` while `pos < to`, the events of each block given by `at(pos)`.
    fn run(&mut self, from: Samples, to: Samples, at: impl Fn(Samples) -> Vec<LiveEvent>) {
        let mut pos = from;
        while pos < to {
            let ev = at(pos);
            self.block(pos, &ev);
            pos += BLOCK as i64;
        }
    }

    fn events(&self) -> Vec<Ev> {
        self.log.lock().unwrap().clone()
    }

    /// The notes sent (no controllers).
    fn notes(&self) -> Vec<Ev> {
        self.events().into_iter().filter(|e| matches!(e, Ev::On(..) | Ev::Off(..))).collect()
    }

    fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    /// Pitches still sounding, applying events in the order sent within each sample, like the
    /// built-in instruments: a note-on adds a voice, a note-off ends every voice of its pitch.
    fn sounding(&self) -> Vec<u8> {
        let mut ev = self.events();
        let at = |e: &Ev| match *e {
            Ev::On(t, _) | Ev::Off(t, _) | Ev::AllOff(t) | Ev::Midi(t, _) => t,
        };
        ev.sort_by_key(at);
        let mut on = [false; 128];
        for e in ev {
            match e {
                Ev::On(_, p) => on[usize::from(p)] = true,
                Ev::Off(_, p) => on[usize::from(p)] = false,
                Ev::AllOff(_) => on = [false; 128],
                Ev::Midi(..) => {}
            }
        }
        (0..128u8).filter(|p| on[usize::from(*p)]).collect()
    }
}

#[test]
fn a_live_note_plays_at_its_offset_in_the_block() {
    let mut r = rig(Vec::new());
    r.block(0, &[]);
    r.block(512, &[on(1, 60, 100), off(1, 60, 400)]);
    assert_eq!(r.notes(), [Ev::On(612, 60), Ev::Off(912, 60)]);
    // An offset past the block plays on its last sample.
    r.clear();
    r.block(1024, &[on(1, 61, 9_999)]);
    assert_eq!(r.notes(), [Ev::On(1024 + 511, 61)]);
}

/// The arrangement holds a pitch while a live key plays it too: whichever lets go first, the
/// pitch sounds until the other does. On a sample both send on, the arrangement's release comes
/// first, so a live note-on there is not cut.
#[test]
fn live_and_arrangement_notes_never_cut_each_other() {
    // The arrangement plays 60 for beats 0..2 and 62 from beat 1 for a beat.
    let mut r = rig(vec![note(60, 0, 2 * Q), note(62, Q, Q)]);
    let (b1, b2) = (r.sample(Q), r.sample(2 * Q));
    // Live: 60 pressed at sample 1000 (inside the arrangement's) and held past its end; 62
    // pressed exactly where the arrangement's 62 ends and released at beat 3.
    let (b2_block, b2_off) = (b2 / 512 * 512, (b2 % 512) as usize);
    let b3 = r.sample(3 * Q);
    r.run(0, r.sample(4 * Q), |pos| {
        let mut v = Vec::new();
        if pos == 512 {
            v.push(on(1, 60, 1000 - 512));
        }
        if pos == b2_block {
            v.push(on(1, 62, b2_off));
        }
        if pos == b3 / 512 * 512 {
            v.push(off(1, 60, (b3 % 512) as usize));
            v.push(off(1, 62, (b3 % 512) as usize));
        }
        v
    });
    let ev = r.events();
    let first = |e: Ev| ev.iter().position(|x| *x == e).unwrap_or_else(|| panic!("{e:?} not sent: {ev:?}"));
    // The arrangement's note-off of 60 at beat 2 is held back: the live key still holds it.
    assert!(!ev.contains(&Ev::Off(b2, 60)), "{ev:?}");
    // The arrangement's 62 ends at beat 2, then the live 62 starts on that sample.
    assert!(first(Ev::Off(b2, 62)) < first(Ev::On(b2, 62)));
    assert!(ev.contains(&Ev::On(b1, 62)));
    // Both released by the live keys at beat 3.
    assert!(ev.contains(&Ev::Off(b3, 60)) && ev.contains(&Ev::Off(b3, 62)), "{ev:?}");
    assert_eq!(r.sounding(), Vec::<u8>::new());
    // The other way round: a live key let go while the arrangement holds the pitch waits for it.
    let mut r = rig(vec![note(64, 0, 2 * Q)]);
    r.run(0, r.sample(3 * Q), |pos| match pos {
        512 => vec![on(1, 64, 0)],
        1024 => vec![off(1, 64, 0)],
        _ => Vec::new(),
    });
    let ev = r.events();
    assert!(!ev.contains(&Ev::Off(1024, 64)), "the arrangement still holds 64: {ev:?}");
    assert!(ev.contains(&Ev::Off(b2, 64)), "{ev:?}");
    assert_eq!(r.sounding(), Vec::<u8>::new());
}

/// A playing MIDI slot keeps playing its notes with live notes on top, and its notes don't cut
/// a live key on the same pitch.
#[test]
fn a_playing_slot_and_live_notes_play_together() {
    let mut r = rig(Vec::new());
    let notes = (0..4).map(|b| note(36, b * Q, Q / 4)).collect();
    let mut col = LiveColumn::new(r.track);
    col.slots[0] =
        Some(MidiSlot { name: "k".into(), color: [0; 3], sequence: Sequence { notes, ctrls: Vec::new() }, length_beats: 4.0, gain_db: 0.0 }.into());
    r.s.live.columns.push(col);
    r.eng.live(LiveCmd::Launch { column: 0, slot: 0, at: 0 });
    let beat = r.sample(Q);
    r.run(0, r.sample(4 * Q), |pos| match pos {
        // A live 36 held across the slot's second kick, and a 72 alongside.
        p if p == beat / 512 * 512 - 512 => vec![on(1, 36, 0), on(1, 72, 0)],
        p if p == r_end(beat) => vec![off(1, 36, 0), off(1, 72, 0)],
        _ => Vec::new(),
    });
    let ev = r.events();
    let kicks: Vec<Samples> = ev.iter().filter_map(|e| if let Ev::On(t, 36) = e { Some(*t) } else { None }).collect();
    assert!(kicks.contains(&(beat / 512 * 512 - 512)), "the live one: {ev:?}");
    for b in 0..4 {
        assert!(kicks.contains(&r.sample(b * Q)), "kick {b} plays: {kicks:?}");
    }
    // The slot's release of its second kick waits for the live 36.
    let kick_off = r.sample(Q + Q / 4);
    assert!(!ev.contains(&Ev::Off(kick_off, 36)), "{ev:?}");
    assert!(ev.contains(&Ev::On(beat / 512 * 512 - 512, 72)));
    // Released with the live keys (the next pass's first kick is just starting as the run ends).
    let end = r_end(beat);
    assert!(ev.contains(&Ev::Off(end, 36)) && ev.contains(&Ev::Off(end, 72)), "{ev:?}");
    assert!(!r.sounding().contains(&72));
}

/// Where the live keys of [`a_playing_slot_and_live_notes_play_together`] are let go: the block
/// after the slot's second kick ends.
fn r_end(beat: Samples) -> Samples {
    (beat + beat / 4) / 512 * 512 + 512
}

#[test]
fn nothing_live_is_left_sounding_after_a_target_change_a_closed_device_a_panic_or_a_stop() {
    let mut r = rig(Vec::new());
    r.block(0, &[on(1, 60, 0), on(2, 62, 0), LiveEvent { source: 2, offset: 0, msg: [0xB0, 64, 127] }]);
    // Port 1's device goes: only its key is released.
    r.eng.live_release(1);
    r.block(512, &[]);
    assert_eq!(r.sounding(), [62]);
    // A target change releases what the old target holds, and lifts its pedal.
    r.eng.set_live_target(None);
    r.block(1024, &[]);
    assert_eq!(r.sounding(), Vec::<u8>::new());
    // (Released between blocks: the recorder stamps them with the block before.)
    assert!(r.events().contains(&Ev::Midi(512, [0xB0, 64, 0])), "{:?}", r.events());
    // Panic: released, and the controllers reset.
    r.eng.set_live_target(Some(r.track));
    r.block(1536, &[on(1, 64, 0)]);
    r.eng.live_panic();
    r.block(2048, &[]);
    assert_eq!(r.sounding(), Vec::<u8>::new());
    assert!(r.events().ends_with(&[Ev::Off(1536, 64), Ev::Midi(1536, [0xB0, 64, 0]), Ev::Midi(1536, [0xB0, 121, 0])]), "{:?}", r.events());
    // A stop.
    r.block(2560, &[on(1, 65, 0)]);
    r.eng.live_release_all();
    r.eng.reset();
    r.block(3072, &[]);
    assert_eq!(r.sounding(), Vec::<u8>::new());
}

/// With the transport stopped the target's instrument plays live input (and nothing else
/// renders it); the standing position is not taken for a seek every block.
#[test]
fn a_stopped_transport_plays_the_target() {
    let mut r = rig(vec![note(70, 0, Q)]);
    r.eng.monitor_only = true;
    r.block(0, &[on(1, 60, 10)]);
    r.block(0, &[]);
    r.block(0, &[off(1, 60, 20)]);
    assert_eq!(r.events(), [Ev::On(10, 60), Ev::Off(20, 60)], "no arrangement note, no all-notes-off");
    assert_eq!(r.blocks.load(Ordering::Relaxed), 3, "the instrument rendered every block");
    // No target, nothing ringing: the instrument rests.
    r.eng.set_live_target(None);
    let mut quiet = rig(Vec::new());
    quiet.eng.set_live_target(None);
    quiet.eng.monitor_only = true;
    quiet.block(0, &[]);
    assert_eq!(quiet.blocks.load(Ordering::Relaxed), 0);
    assert!(!quiet.eng.live_awake());
}

/// A seek (a loop wrap, a play from stopped) releases the arrangement's notes but not a live
/// key's.
#[test]
fn a_seek_keeps_live_keys_sounding() {
    let mut r = rig(vec![note(70, 0, 4 * Q)]);
    r.block(0, &[on(1, 60, 0)]);
    r.block(512, &[]);
    let before = r.events().len();
    // Jump back to 0 (a loop wrap) and play on.
    r.block(0, &[]);
    assert_eq!(r.events().get(before), Some(&Ev::Off(0, 70)), "the arrangement's note released: {:?}", r.events());
    // A play from stopped (a reset) too.
    r.eng.reset();
    r.block(0, &[]);
    assert!(!r.events().iter().any(|e| matches!(e, Ev::AllOff(_))), "{:?}", r.events());
    assert!(r.sounding().contains(&60), "{:?}", r.events());
}

/// An instrument replaced takes its notes with it: the live keys are forgotten, so their later
/// releases send nothing to the new one.
#[test]
fn a_replaced_instrument_forgets_the_live_keys() {
    let mut r = rig(Vec::new());
    r.block(0, &[on(1, 60, 0)]);
    let rec =
        Recorder { inner: soundcraft_dsp::create("gain").unwrap(), pos: Arc::clone(&r.pos), log: Arc::clone(&r.log), blocks: Arc::clone(&r.blocks) };
    r.eng.adopt(vec![PreparedInstance { track: r.track, slot: INSTRUMENT_INSTANCE, id: "clap:test.rec".into(), ch: 2, plugin: Box::new(rec) }]);
    r.clear();
    r.block(512, &[off(1, 60, 0)]);
    assert_eq!(r.events(), Vec::<Ev>::new());
    assert_eq!(r.eng.take_retired().len(), 1);
}

/// The inbox never grows: a burst past it is refused and counted.
#[test]
fn a_burst_past_the_inbox_is_refused_and_counted() {
    let mut r = rig(Vec::new());
    let mut refused = 0;
    for i in 0..LIVE_INBOX + 100 {
        if !r.eng.live_input(LiveEvent { source: 1, offset: i % 512, msg: [0xB0, 1, (i % 128) as u8] }) {
            refused += 1;
        }
    }
    assert_eq!((refused, r.eng.live_dropped()), (100, 100));
    r.block(0, &[]);
    assert_eq!(r.events().iter().filter(|e| matches!(e, Ev::Midi(_, [0xB0, 1, _]))).count(), LIVE_INBOX);
}
