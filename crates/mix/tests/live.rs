//! Live slots launch at their sample, loop in session beats and switch without clicks.
//!
//! Sources are synthetic and in memory: clicks on the beats of a 125 bpm "drum loop", sines and
//! constant levels, played over a 120 bpm session (and under tempo steps).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{LiveCmd, MixEngine, render_range};
use soundcraft_model::{
    AutoParam, AutomationLane, ChannelFormat, LiveColumn, Session, Slot, SlotMode, SourceAudio, SourceId, StemSet, TrackId, TrackKind, WarpMap,
};
use soundcraft_time::{Range, Samples, TICKS_PER_QUARTER};
use std::sync::Arc;

const SR: f64 = 48_000.0;
const BLOCK: usize = 512;
/// Beat length of the click source (125 bpm).
const SRC_BEAT: f64 = 23_040.0;
/// Where the click source's first beat falls.
const SRC_FIRST: f64 = 2_000.0;
const LOOP: SourceId = SourceId(900);

fn audio(ch: Vec<Vec<f32>>) -> Arc<SourceAudio> {
    Arc::new(SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: ch }))
}

/// A decaying 2 kHz burst of `amp` at `at`.
fn burst(v: &mut [f32], at: usize, amp: f32) {
    for i in 0..200 {
        if let Some(x) = v.get_mut(at + i) {
            *x += amp * (i as f32 * std::f32::consts::TAU * 2_000.0 / 48_000.0).sin() * (-(i as f32) / 40.0).exp();
        }
    }
}

/// A 125 bpm loop: a loud click on every bar's first beat, soft ones on the others, 16 beats.
fn clicks() -> (Vec<f32>, WarpMap) {
    let mut v = vec![0.0f32; (SRC_FIRST + 17.0 * SRC_BEAT) as usize];
    for k in 0..16 {
        burst(&mut v, (SRC_FIRST + k as f64 * SRC_BEAT) as usize, if k % 4 == 0 { 0.9 } else { 0.25 });
    }
    let beats = (0..17).map(|k| SRC_FIRST + k as f64 * SRC_BEAT).collect();
    (v, WarpMap::new(beats, 0.0).unwrap())
}

fn loop_slot(source: SourceId, warp: WarpMap, beats: f64) -> Slot {
    Slot {
        name: "loop".into(),
        color: [0; 3],
        source,
        warp: Some(warp),
        start_beat: 0.0,
        length_beats: beats,
        stems: None,
        stem_mask: [true; 4],
        mode: SlotMode::Loop,
        gain_db: 0.0,
    }
}

/// A 120 bpm session (stepped up by 2 bpm every beat from bar 3 on when `ramp`) with one Live
/// column on a stereo audio track.
fn session(ramp: bool) -> (Session, TrackId) {
    let mut s = Session::default();
    s.tempo.set_tempo(0, 120.0).unwrap();
    if ramp {
        for b in 8..40 {
            s.tempo.set_tempo(b * TICKS_PER_QUARTER, 120.0 + 2.0 * (b - 7) as f64).unwrap();
        }
    }
    let (v, _) = clicks();
    s.pool.insert(LOOP, audio(vec![v]));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    s.live.columns.push(LiveColumn::new(t));
    (s, t)
}

fn put(s: &mut Session, row: usize, slot: Slot) {
    s.live.columns[0].slots[row] = Some(slot.into());
}

fn bar(s: &Session, n: i64) -> Samples {
    s.tempo.tick_to_samples(s.tempo.bar_start_tick(n), s.sample_rate)
}

/// Renders `[0, len)` in blocks, handing each command to the engine before the block that
/// starts at or after its send time. Returns the left channel.
fn run(eng: &mut MixEngine, s: &Session, len: Samples, cmds: &[(Samples, LiveCmd)]) -> Vec<f32> {
    let mut out = vec![vec![0.0; BLOCK]; 2];
    let mut left = Vec::with_capacity(len as usize);
    let mut pos = 0;
    let mut sent = 0;
    while pos < len {
        while let Some((_, c)) = cmds.get(sent).filter(|(at, _)| *at <= pos) {
            eng.live(*c);
            sent += 1;
        }
        eng.render(s, pos, BLOCK, &mut out);
        left.extend_from_slice(&out[0]);
        pos += BLOCK as i64;
    }
    left
}

fn engine() -> MixEngine {
    MixEngine::new(SR as f32, BLOCK)
}

/// The onset of the loud click near `expect` (the first sample above a third of its peak),
/// and its error in ms.
fn loud_click(out: &[f32], expect: Samples) -> f64 {
    let lo = (expect - 2_400) as usize;
    let seg = &out[lo..lo + 4_800];
    let peak = seg.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(peak > 0.5, "no loud click near {expect} (peak {peak})");
    let onset = seg.iter().position(|x| x.abs() > 0.3 * peak).unwrap();
    ((lo + onset) as f64 - expect as f64) / SR * 1_000.0
}

/// A loop launched for bar 3 is silent up to its sample and sounds from it; its loud click (the
/// loop's first beat) sounds on the bar line of bar 3 and of the next three passes, at a
/// constant tempo and under tempo steps.
#[test]
fn a_loop_starts_on_its_sample_and_every_pass_starts_on_a_bar_line() {
    for ramp in [false, true] {
        let (mut s, _) = session(ramp);
        let (_, warp) = clicks();
        put(&mut s, 0, loop_slot(LOOP, warp, 4.0));
        let at = bar(&s, 3);
        let mut eng = engine();
        let out = run(&mut eng, &s, bar(&s, 7) + 4_800, &[(0, LiveCmd::Launch { column: 0, slot: 0, at })]);
        let first = out.iter().position(|x| *x != 0.0).unwrap();
        assert_eq!(first as i64, at, "ramp {ramp}: sound starts exactly at the launch sample");
        let mut worst = 0.0f64;
        for n in 3..7 {
            let err = loud_click(&out, bar(&s, n));
            worst = worst.max(err.abs());
            assert!(err.abs() < 1.0, "ramp {ramp}, bar {n}: the pass starts {err:+.3} ms off the bar line");
        }
        // Soft clicks fall on the beats between.
        let beat = |b: i64| s.tempo.tick_to_samples(b * TICKS_PER_QUARTER, s.sample_rate);
        for b in [9, 13, 17, 22] {
            let lo = (beat(b) - 240) as usize;
            assert!(out[lo..lo + 1_200].iter().any(|x| x.abs() > 0.1), "ramp {ramp}: no click on beat {b}");
        }
        println!("ramp {ramp}: worst pass-start error {worst:.3} ms");
    }
}

/// The engine honours `at` to the sample, inside a block, with one-shots played straight.
#[test]
fn a_launch_lands_inside_the_block_on_its_sample() {
    let (mut s, _) = session(false);
    s.pool.insert(SourceId(901), audio(vec![vec![0.25; 48_000]]));
    put(&mut s, 1, Slot::one_shot("dc", [0; 3], SourceId(901)));
    for at in [1_000, 1_024, 5_000, 7_777] {
        let mut eng = engine();
        let out = run(&mut eng, &s, 12_000, &[(0, LiveCmd::Launch { column: 0, slot: 1, at })]);
        let at = at as usize;
        assert!(out[..at].iter().all(|x| *x == 0.0), "silent before {at}");
        assert!(out[at] > 0.0, "sounding at {at}");
        assert!(out[at + 200..at + 2_000].iter().all(|x| (x - 0.25).abs() < 1e-6), "full level after the fade-in");
    }
}

/// One slot per column: launching another switches at one sample: the new slot's first sample
/// is the launch sample, the old one is gone a crossfade later; a stop leaves silence a
/// crossfade after its sample; a one-shot ends with its source.
#[test]
fn switching_and_stopping_happen_at_their_sample() {
    let (mut s, _) = session(false);
    s.pool.insert(SourceId(901), audio(vec![vec![0.25; 48_000]]));
    s.pool.insert(SourceId(902), audio(vec![vec![0.5; 30_000]]));
    put(&mut s, 0, Slot::one_shot("a", [0; 3], SourceId(901)));
    put(&mut s, 1, Slot::one_shot("b", [0; 3], SourceId(902)));
    let xf = (soundcraft_mix::XFADE_S * SR).round() as usize;
    let (a, b, stop) = (2_000usize, 10_301usize, 20_000usize);
    let mut eng = engine();
    let cmds = [
        (0, LiveCmd::Launch { column: 0, slot: 0, at: a as i64 }),
        (4_096, LiveCmd::Launch { column: 0, slot: 1, at: b as i64 }),
        (12_288, LiveCmd::Stop { column: 0, at: stop as i64 }),
        (25_000, LiveCmd::Launch { column: 0, slot: 1, at: 30_000 }),
    ];
    let out = run(&mut eng, &s, 70_000, &cmds);
    assert!((out[b - 1] - 0.25).abs() < 1e-6, "a plays up to the switch");
    assert!(out[b] != 0.25, "b comes in at the switch sample");
    assert!(out[b + xf..stop].iter().all(|x| (x - 0.5).abs() < 1e-6), "only b after the crossfade");
    assert!(out[b..b + xf].iter().all(|x| *x > 0.0 && *x < 0.75), "a smooth hand-over");
    assert!((out[stop - 1] - 0.5).abs() < 1e-6, "b plays up to the stop");
    assert!(out[stop..stop + xf].windows(2).all(|w| w[1] <= w[0]), "the stop fades");
    assert!(out[stop + xf..30_000].iter().all(|x| *x == 0.0), "silent after the stop's fade");
    assert!(out[30_000 + 200..60_000].iter().all(|x| (x - 0.5).abs() < 1e-6));
    assert!(out[60_000..].iter().all(|x| *x == 0.0), "the one-shot ends with its source");
    assert!(eng.live_state()[0].playing.is_none());
}

/// The largest step between neighbouring samples in `x`.
fn max_step(x: &[f32]) -> f32 {
    x.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()))
}

/// A loop restart crossfades: around every seam of a sine loop that does not fit its length
/// (a hard restart would jump by up to twice its amplitude), no step is larger than the
/// sine's own slope allows.
#[test]
fn a_loop_restart_has_no_step() {
    let (mut s, _) = session(false);
    // 120 bpm source (identity rate), a 441 Hz sine: one beat is 220.5 cycles, so every restart
    // lands half a cycle off.
    let amp = 0.5f32;
    let n = 48_000 * 4;
    let sine: Vec<f32> = (0..n).map(|i| amp * (i as f32 * std::f32::consts::TAU * 441.0 / 48_000.0).sin()).collect();
    s.pool.insert(SourceId(903), audio(vec![sine]));
    let beats = (0..8).map(|k| 1_000.0 + k as f64 * 24_000.0).collect();
    put(&mut s, 0, loop_slot(SourceId(903), WarpMap::new(beats, 0.0).unwrap(), 1.0));
    let mut eng = engine();
    let at = bar(&s, 2);
    let out = run(&mut eng, &s, at + 24_000 * 5 + 1_000, &[(0, LiveCmd::Launch { column: 0, slot: 0, at })]);
    let slope = amp * std::f32::consts::TAU * 441.0 / 48_000.0;
    let steady = max_step(&out[(at + 5_000) as usize..(at + 20_000) as usize]);
    let mut worst = 0.0f32;
    for k in 1..5 {
        let seam = (at + k * 24_000) as usize;
        let step = max_step(&out[seam - 300..seam + 300]);
        worst = worst.max(step);
        assert!(step < 1.5 * slope, "seam {k}: step {step:.4} (sine slope {slope:.4}, steady {steady:.4})");
    }
    println!("seam: worst step {worst:.4}, sine slope {slope:.4}, steady {steady:.4}, hard cut would be up to {:.4}", 2.0 * amp);
}

/// A split slot plays the stems its mask lets through, at the column track's stem faders and the
/// set's scale; as a loop it plays through a player per stem.
#[test]
fn a_stem_slot_plays_its_masked_stems_through_the_columns_stem_faders() {
    let stems = [SourceId(911), SourceId(912), SourceId(913), SourceId(914)];
    let levels = [0.0625f32, 0.125, 0.25, 0.03125];
    let one_shot = |s: &mut Session, mask: [bool; 4]| {
        let mut sl = Slot::one_shot("split", [0; 3], SourceId(910));
        sl.stems = Some(StemSet { scale_log2: 1, key: "k".into(), sources: stems });
        sl.stem_mask = mask;
        put(s, 0, sl);
    };
    let (mut s, t) = session(false);
    s.pool.insert(SourceId(910), audio(vec![vec![0.9; 48_000]]));
    for (id, l) in stems.iter().zip(levels) {
        s.pool.insert(*id, audio(vec![vec![l; 48_000]]));
    }
    s.track_mut(t).unwrap().mixer.stem_gain_db = [0.0, 0.0, -6.0, 0.0];
    one_shot(&mut s, [true, false, true, false]);
    let mut eng = engine();
    let out = run(&mut eng, &s, 8_192, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: 1_000 })]);
    let want = 2.0 * (0.0625 + 0.25 * soundcraft_dsp::db_to_gain(-6.0));
    assert!((out[4_000] - want).abs() < 1e-5, "vocals and bass (at -6 dB), times the scale: {} vs {want}", out[4_000]);
    // A stem fader lane on the column track moves its stem.
    let mut lane = AutomationLane::new(AutoParam::Stem(0));
    lane.set_point(0, -144.0);
    s.track_mut(t).unwrap().automation.push(lane);
    let mut eng = engine();
    let out = run(&mut eng, &s, 8_192, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: 1_000 })]);
    assert!((out[4_000] - 2.0 * 0.25 * soundcraft_dsp::db_to_gain(-6.0)).abs() < 1e-5);

    // As a loop: one player per stem, and only the unmasked stem is heard, exactly as its own
    // audio played unsplit would be.
    let (v, warp) = clicks();
    let (mut split, _) = session(false);
    // The masked-off stems are loud: none of them may be heard.
    for (k, id) in stems.iter().enumerate() {
        split.pool.insert(*id, audio(vec![if k == 1 { v.clone() } else { vec![0.3; v.len()] }]));
    }
    let mut sl = loop_slot(LOOP, warp.clone(), 4.0);
    sl.stems = Some(StemSet { scale_log2: 0, key: "k".into(), sources: stems });
    sl.stem_mask = [false, true, false, false];
    put(&mut split, 0, sl);
    let (mut plain, _) = session(false);
    put(&mut plain, 0, loop_slot(LOOP, warp, 4.0));
    let at = bar(&plain, 2);
    let launch = [(0, LiveCmd::Launch { column: 0, slot: 0, at })];
    let mut e1 = engine();
    let a = run(&mut e1, &split, at + 200_000, &launch);
    let mut e2 = engine();
    let b = run(&mut e2, &plain, at + 200_000, &launch);
    assert_eq!(e1.slot_players().count(), 2, "players (two: playing and warming) are built only for the stems it plays");
    let diff = a.iter().zip(&b).fold(0.0f32, |m, (x, y)| m.max((x - y).abs()));
    assert!(diff < 1e-6, "the masked loop is its unmasked stem (diff {diff})");
    assert!(a.iter().any(|x| x.abs() > 0.5));
}

/// A stopped transport clears every slot (the playback crate calls `live_clear` on stop); a
/// locate keeps loops playing, re-phased: the loop's first beat still sounds on the bar lines
/// of the new position.
#[test]
fn stop_clears_and_locate_re_phases() {
    let (mut s, _) = session(false);
    let (_, warp) = clicks();
    put(&mut s, 0, loop_slot(LOOP, warp, 4.0));
    let mut eng = engine();
    let at = bar(&s, 2);
    let _ = run(
        &mut eng,
        &s,
        at + 50_000,
        &[(0, LiveCmd::Launch { column: 0, slot: 0, at }), (at + BLOCK as i64, LiveCmd::Stop { column: 0, at: bar(&s, 9) })],
    );
    assert!(eng.live_state()[0].playing.is_some() && eng.live_state()[0].queued.is_some());
    // Locate to before bar 11 (the loop is one bar): the queued stop is dropped, the loop goes
    // on, its first beat on the bar lines of the new position.
    let from = at + 50_176;
    let to = bar(&s, 11) - 30_000;
    eng.live_locate(from, to, false);
    assert!(eng.live_state()[0].queued.is_none());
    let mut out = vec![vec![0.0; BLOCK]; 2];
    let mut left = Vec::new();
    let mut pos = to;
    while pos < bar(&s, 13) + 4_800 {
        eng.render(&s, pos, BLOCK, &mut out);
        left.extend_from_slice(&out[0]);
        pos += BLOCK as i64;
    }
    for n in [11, 12, 13] {
        let err = loud_click(&left, bar(&s, n) - to);
        assert!(err.abs() < 1.0, "bar {n} after the locate: {err:+.3} ms");
    }
    eng.live_clear();
    assert!(eng.live_state().iter().all(|c| c.playing.is_none() && c.queued.is_none()));
    eng.render(&s, pos, BLOCK, &mut out);
    eng.render(&s, pos + BLOCK as i64, BLOCK, &mut out);
    assert!(out[0].iter().all(|x| *x == 0.0), "nothing plays after a stop");
}

/// Bounces and offline renders have no live state: slots never reach them.
#[test]
fn bounces_ignore_live_slots() {
    let (mut s, _) = session(false);
    let (_, warp) = clicks();
    put(&mut s, 0, loop_slot(LOOP, warp, 4.0));
    let out = render_range(&s, Range::new(0, 200_000), BLOCK);
    assert!(out.iter().flatten().all(|x| *x == 0.0));
}

/// A lost column (here a second column on the same track, or one on a bus-like aux track) plays
/// nothing; the column that owns the track plays.
#[test]
fn a_lost_column_plays_nothing() {
    let (mut s, t) = session(false);
    s.pool.insert(SourceId(901), audio(vec![vec![0.25; 48_000]]));
    let aux = s.add_track(TrackKind::Aux, ChannelFormat::Stereo, None);
    s.live.columns.push(LiveColumn::new(t));
    s.live.columns.push(LiveColumn::new(aux));
    for c in 1..3 {
        s.live.columns[c].slots[0] = Some(Slot::one_shot("dc", [0; 3], SourceId(901)).into());
    }
    let mut eng = engine();
    let cmds = [(0, LiveCmd::Launch { column: 1, slot: 0, at: 1_000 }), (0, LiveCmd::Launch { column: 2, slot: 0, at: 1_000 })];
    let out = run(&mut eng, &s, 8_192, &cmds);
    assert!(out.iter().all(|x| *x == 0.0));
    s.live.columns[0].slots[0] = Some(Slot::one_shot("dc", [0; 3], SourceId(901)).into());
    let mut eng = engine();
    let out = run(&mut eng, &s, 8_192, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: 1_000 })]);
    assert!((out[4_000] - 0.25).abs() < 1e-6, "the owning column plays");
}

/// The transport wrapping at the end of its loop range moves queued launches with it: a launch
/// for the range's end sample starts at its start. A locate drops them.
#[test]
fn a_loop_wrap_keeps_queued_launches() {
    let (mut s, _) = session(false);
    s.pool.insert(SourceId(901), audio(vec![vec![0.25; 48_000]]));
    put(&mut s, 0, Slot::one_shot("dc", [0; 3], SourceId(901)));
    let (l0, l1) = (10_240, 20_480);
    let mut eng = engine();
    let _ = run(&mut eng, &s, l1, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: l1 })]);
    eng.live_locate(l1, l0, true);
    assert_eq!(eng.live_state()[0].queued.map(|q| q.at), Some(l0));
    let mut out = vec![vec![0.0; BLOCK]; 2];
    eng.render(&s, l0, BLOCK, &mut out);
    assert_eq!(out[0][0], 0.25, "it starts on the wrap's first sample");
    eng.live(LiveCmd::Stop { column: 0, at: l1 });
    eng.live_locate(l0 + 512, 0, false);
    assert!(eng.live_state()[0].queued.is_none(), "a locate drops it");
}

/// Launches sent just before the transport starts stay queued; ones for before the play
/// position are dropped.
#[test]
fn launches_sent_before_play_stay_queued() {
    let mut eng = engine();
    eng.live(LiveCmd::Launch { column: 0, slot: 0, at: 4_800 });
    eng.live(LiveCmd::Launch { column: 1, slot: 0, at: 100 });
    eng.live_start(1_000);
    assert_eq!(eng.live_state()[0].queued.map(|q| q.at), Some(4_800));
    assert!(eng.live_state()[1].queued.is_none());
}

/// While a slot plays, the column track's own clips are muted (fading over the crossfade), and
/// they come back when the column stops.
#[test]
fn a_playing_slot_mutes_the_column_tracks_clips() {
    let (mut s, t) = session(false);
    s.pool.insert(SourceId(901), audio(vec![vec![0.25; 96_000]]));
    s.pool.insert(SourceId(905), audio(vec![vec![0.125; 96_000]]));
    let id = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(soundcraft_model::Clip::audio(id, "arr", SourceId(905), 0, 0, 96_000));
    put(&mut s, 0, Slot::one_shot("dc", [0; 3], SourceId(901)));
    let xf = (soundcraft_mix::XFADE_S * SR).round() as usize;
    let (on, off) = (10_000usize, 40_000usize);
    let mut eng = engine();
    let out = run(
        &mut eng,
        &s,
        60_000,
        &[(0, LiveCmd::Launch { column: 0, slot: 0, at: on as i64 }), (on as i64 + BLOCK as i64, LiveCmd::Stop { column: 0, at: off as i64 })],
    );
    assert!(out[..on].iter().all(|x| (x - 0.125).abs() < 1e-6), "the arrangement plays");
    assert!(out[on + xf..off].iter().all(|x| (x - 0.25).abs() < 1e-6), "only the slot while it plays");
    assert!(out[off + xf..].iter().all(|x| (x - 0.125).abs() < 1e-6), "the arrangement is back");
    // The clips fade under the slot's (hard) start and back in under its tail: no steps there.
    assert!(out[on] > 0.37 && out[on..on + xf + 1].windows(2).all(|w| w[1] <= w[0]), "the clips fade out");
    assert!(max_step(&out[off - 10..off + xf + 10]) < 0.01, "slot and clips cross over");
}

/// A playing slot that is cleared fades out (rather than cutting off); one whose audio is gone
/// stops its column; a column whose track is deleted forgets its state.
#[test]
fn clearing_a_playing_slot_fades_it_out_and_a_lost_column_forgets() {
    let (mut s, t) = session(false);
    s.pool.insert(SourceId(901), audio(vec![vec![0.5; 96_000]]));
    put(&mut s, 0, Slot::one_shot("dc", [0; 3], SourceId(901)));
    let xf = (soundcraft_mix::XFADE_S * SR).round() as usize;
    let mut eng = engine();
    let at = 8_192;
    let before = run(&mut eng, &s, at, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: 1_000 })]);
    assert!((before[at as usize - 1] - 0.5).abs() < 1e-6);
    let mut cleared = s.clone();
    cleared.live.columns[0].slots[0] = None;
    eng.live_snapshot(&s, &cleared, at);
    assert!(eng.live_state()[0].playing.is_none());
    let mut out = vec![vec![0.0; BLOCK]; 2];
    eng.render(&cleared, at, BLOCK, &mut out);
    let tail = &out[0];
    assert!(tail[0] > 0.45 && tail[..xf].windows(2).all(|w| w[1] <= w[0]), "it fades out");
    assert!(tail[xf..].iter().all(|x| *x == 0.0));

    // Its audio gone: the column stops.
    let mut eng = engine();
    let _ = run(&mut eng, &s, at, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: 1_000 })]);
    let mut gone = s.clone();
    gone.pool.remove(SourceId(901));
    eng.render(&gone, at, BLOCK, &mut out);
    assert!(eng.live_state()[0].playing.is_none());

    // The track deleted: the lost column forgets what it played and had queued.
    let mut eng = engine();
    let _ = run(&mut eng, &s, at, &[(0, LiveCmd::Launch { column: 0, slot: 0, at: 1_000 }), (2_048, LiveCmd::Stop { column: 0, at: 90_000 })]);
    let mut lost = s.clone();
    lost.tracks.retain(|x| x.id != t);
    eng.render(&lost, at, BLOCK, &mut out);
    assert_eq!(eng.live_state()[0], soundcraft_mix::ColumnPlay::default());
}

/// A launch exactly on a loop restart that falls on a block boundary takes over from the pass
/// that ends once: its tail is not rendered twice (no level bump).
#[test]
fn a_launch_on_a_restart_at_a_block_boundary_has_one_tail() {
    let (mut s, _) = session(false);
    let amp = 0.5f32;
    let sine: Vec<f32> = (0..48_000 * 4).map(|i| amp * (i as f32 * std::f32::consts::TAU * 441.0 / 48_000.0).sin()).collect();
    s.pool.insert(SourceId(903), audio(vec![sine]));
    let beats = (0..8).map(|k| 1_000.0 + k as f64 * 24_000.0).collect();
    put(&mut s, 0, loop_slot(SourceId(903), WarpMap::new(beats, 0.0).unwrap(), 1.0));
    // Restarts every 24 000 samples from bar 2 (96 000); the fourth falls on a block boundary.
    let at = bar(&s, 2);
    let target = at + 4 * 24_000;
    assert_eq!(target % BLOCK as i64, 0);
    let mut eng = engine();
    let out = run(
        &mut eng,
        &s,
        target + 4_000,
        &[(0, LiveCmd::Launch { column: 0, slot: 0, at }), (target - 10_000, LiveCmd::Launch { column: 0, slot: 0, at: target })],
    );
    let around = out[(target - 200) as usize..(target + 300) as usize].iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(around < 2.05 * amp, "peak {around} around the relaunch (one tail plus the new pass)");
}

/// Renders a transport loop over `[l0, l1)` from `l0`, `len` samples of output in all, wrapping
/// the way playback does (blocks end on `l1`, then `live_locate(l1, l0, true)`). Returns the left
/// channel as heard.
fn run_looped(eng: &mut MixEngine, s: &Session, (l0, l1): (Samples, Samples), len: usize, cmds: &[LiveCmd]) -> Vec<f32> {
    eng.set_live_loop(Some(Range::new(l0, l1)));
    for c in cmds {
        eng.live(*c);
    }
    let mut out = vec![vec![0.0; BLOCK]; 2];
    let mut heard = Vec::with_capacity(len);
    let mut pos = l0;
    while heard.len() < len {
        if pos >= l1 {
            eng.live_locate(pos, l0, true);
            pos = l0;
        }
        let n = BLOCK.min((l1 - pos) as usize);
        eng.render(s, pos, n, &mut out);
        heard.extend_from_slice(&out[0][..n]);
        pos += n as i64;
    }
    heard
}

/// A 1-bar loop under a 4-bar transport loop (its phase the same after the wrap) and a 3.5-bar one
/// (half a bar off): the wrap has no step, and the downbeat after it sounds whole on its sample.
#[test]
fn a_transport_loop_wrap_is_seamless() {
    let amp = 0.5f32;
    let slope = amp * std::f32::consts::TAU * 441.0 / 48_000.0;
    for bars in [4.0, 3.5] {
        let (mut s, _) = session(false);
        let l1 = (bars * 96_000.0) as i64;
        let sine: Vec<f32> = (0..48_000 * 12).map(|i| amp * (i as f32 * std::f32::consts::TAU * 441.0 / 48_000.0).sin()).collect();
        s.pool.insert(SourceId(903), audio(vec![sine]));
        let beats = (0..20).map(|k| 1_000.0 + k as f64 * 24_000.0).collect();
        put(&mut s, 0, loop_slot(SourceId(903), WarpMap::new(beats, 0.0).unwrap(), 4.0));
        let mut eng = engine();
        let out = run_looped(&mut eng, &s, (0, l1), 2 * l1 as usize + 20_000, &[LiveCmd::Launch { column: 0, slot: 0, at: 0 }]);
        for wrap in [l1 as usize, 2 * l1 as usize] {
            let step = max_step(&out[wrap - 300..wrap + 300]);
            println!("{bars} bars: step {step:.4} at the wrap (sine slope {slope:.4})");
            assert!(step < 1.5 * slope, "{bars}-bar loop, wrap at {wrap}: step {step:.4}");
        }

        let (mut s, _) = session(false);
        let (_, warp) = clicks();
        put(&mut s, 0, loop_slot(LOOP, warp, 4.0));
        let mut eng = engine();
        let out = run_looped(&mut eng, &s, (0, l1), 2 * l1 as usize + 20_000, &[LiveCmd::Launch { column: 0, slot: 0, at: 0 }]);
        for wrap in [l1, 2 * l1] {
            let err = loud_click(&out, wrap);
            assert!(err.abs() < 1.0, "{bars}-bar loop: the downbeat after the wrap is {err:+.3} ms off");
        }
    }
}

/// A launch the audio thread sees after its sample keeps the loop on the grid (its passes still
/// start on bar lines) and is counted as late.
#[test]
fn a_late_launch_stays_on_the_grid() {
    let (mut s, _) = session(false);
    let (_, warp) = clicks();
    put(&mut s, 0, loop_slot(LOOP, warp, 4.0));
    let at = bar(&s, 2);
    let mut eng = engine();
    let out = run(&mut eng, &s, bar(&s, 5) + 4_800, &[(at + 3 * BLOCK as i64, LiveCmd::Launch { column: 0, slot: 0, at })]);
    let p = eng.live_state()[0].playing.unwrap();
    assert_eq!((p.start, eng.live_late()), (at, 1));
    for n in 3..5 {
        let err = loud_click(&out, bar(&s, n));
        assert!(err.abs() < 1.0, "bar {n}: {err:+.3} ms off the grid");
    }
}

/// A loop's position counted from its `origin` (what the Session view's ring shows) follows the
/// audio across transport wraps that aren't a multiple of its length and across a locate: at
/// every downbeat heard it reads 0. Counted from its (moved) start it would drift.
#[test]
fn the_loop_position_from_its_origin_follows_the_audio() {
    let (mut s, _) = session(false);
    let (_, warp) = clicks();
    put(&mut s, 0, loop_slot(LOOP, warp, 4.0));
    let beat = |t: Samples| s.tempo.samples_to_ticks_f(t, s.sample_rate) / TICKS_PER_QUARTER as f64;
    let l1 = 336_000; // 3.5 bars
    let mut eng = engine();
    let _ = run_looped(&mut eng, &s, (0, l1), 3 * l1 as usize, &[LiveCmd::Launch { column: 0, slot: 0, at: 0 }]);
    let p = eng.live_state()[0].playing.unwrap();
    assert_ne!(p.start, p.origin, "three wraps moved the start");
    // After the wraps (the timeline back at 0) and then after a locate to bar 6, the downbeats
    // heard are on bar lines, where the position from the origin reads 0.
    let heard_at = |eng: &mut MixEngine, from: Samples, n: i64| {
        let mut out = vec![vec![0.0; BLOCK]; 2];
        let mut left = Vec::new();
        let mut pos = from;
        while pos < from + n {
            eng.render(&s, pos, BLOCK, &mut out);
            left.extend_from_slice(&out[0]);
            pos += BLOCK as i64;
        }
        left
    };
    eng.live_locate(l1, 0, true);
    let out = heard_at(&mut eng, 0, 200_000);
    for n in [2, 3] {
        let at = bar(&s, n);
        assert!(loud_click(&out, at).abs() < 1.0);
        let pos = (beat(at) - beat(p.origin)).rem_euclid(4.0);
        assert!(pos.min(4.0 - pos) < 1e-6, "bar {n} after the wraps: position {pos}");
    }
    eng.live_locate(200_000, bar(&s, 6) - 30_000, false);
    let from = bar(&s, 6) - 30_000;
    let out = heard_at(&mut eng, from, 150_000);
    let at = bar(&s, 7);
    assert!(loud_click(&out, at - from).abs() < 1.0);
    let pos = (beat(at) - beat(eng.live_state()[0].playing.unwrap().origin)).rem_euclid(4.0);
    assert!(pos.min(4.0 - pos) < 1e-6, "after the locate: position {pos}");
}
