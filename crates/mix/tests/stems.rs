//! A split clip plays its four stems through the track's stem faders and the clip's stem mask.
//!
//! The stems are synthetic and in memory: A + B + C + D = S exactly (every value is on a 2^-16
//! grid, so the sums are exact in f32).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{MixEngine, render_clips, render_range};
use soundcraft_model::{
    AutoParam, AutomationLane, ChannelFormat, Clip, ClipId, Fade, FadeShape, Session, SourceAudio, SourceId, StemSet, TrackId, TrackKind, WarpMap,
};
use soundcraft_time::Range;
use std::sync::Arc;

const SR: i64 = 48_000;
/// Source length and render length.
const N: usize = 48_000 * 2;
const LEN: i64 = 48_000;
const S: SourceId = SourceId(900);
const STEM_IDS: [SourceId; 4] = [SourceId(901), SourceId(902), SourceId(903), SourceId(904)];
/// The bass stem's index (`STEMS` order: vocals, drums, bass, other).
const BASS: usize = 2;

fn q(x: f32) -> f32 {
    (x * 65_536.0).round() / 65_536.0
}

/// A deterministic noise in -1..1.
fn noise(seed: &mut u32) -> f32 {
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    (*seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
}

/// Two channels for each of the four stems (sines at different frequencies plus noise).
fn stems() -> [Vec<Vec<f32>>; 4] {
    let mut seed = 7u32;
    let tone = |hz: f32, amp: f32, ch: usize, i: usize, seed: &mut u32| {
        let t = i as f32 / SR as f32;
        q(amp * (2.0 * std::f32::consts::PI * hz * t + ch as f32).sin() + 0.02 * noise(seed))
    };
    let mut out: [Vec<Vec<f32>>; 4] = Default::default();
    for (k, (hz, amp)) in [(1_200.0, 0.2), (180.0, 0.15), (55.0, 0.25), (640.0, 0.1)].into_iter().enumerate() {
        out[k] = (0..2).map(|ch| (0..N).map(|i| tone(hz, amp, ch, i, &mut seed)).collect()).collect();
    }
    out
}

fn sum(parts: &[&Vec<Vec<f32>>]) -> Vec<Vec<f32>> {
    (0..2).map(|ch| (0..N).map(|i| parts.iter().map(|p| p[ch][i]).sum()).collect()).collect()
}

fn audio(ch: Vec<Vec<f32>>) -> Arc<SourceAudio> {
    Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR as u32, channels: ch }))
}

/// A 120 bpm warp map for the source (beats every half second), played at the session's 128 bpm.
fn warp() -> WarpMap {
    WarpMap::new((0..8).map(|k| 1_000.0 + k as f64 * 24_000.0).collect(), 0.0).unwrap()
}

/// A session with S, its stems at 901..904, and the given extra sources, and one stereo audio
/// track holding a clip of `source` (offset, fades and clip gain set), split when `split`.
fn session(source: SourceId, split: bool, warped: bool, extra: &[(SourceId, Vec<Vec<f32>>)]) -> (Session, TrackId, ClipId) {
    let mut s = Session::default();
    s.tempo.set_tempo(0, 128.0).unwrap();
    let st = stems();
    s.pool.insert(S, audio(sum(&[&st[0], &st[1], &st[2], &st[3]])));
    for (id, ch) in STEM_IDS.into_iter().zip(st) {
        s.pool.insert(id, audio(ch));
    }
    for (id, ch) in extra {
        s.pool.insert(*id, audio(ch.clone()));
    }
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
    let id = s.new_clip_id();
    let mut c = Clip::audio(id, "c", source, 2_000, 1_000, LEN);
    c.gain_db = -2.0;
    c.fade_in = Fade { len: 4_800, shape: FadeShape::default(), curvature: 0.0 };
    c.fade_out = Fade { len: 9_600, shape: FadeShape::default(), curvature: 0.0 };
    if warped {
        c.warp = Some(warp());
    }
    if split {
        c.stems = Some(StemSet { key: "k".into(), sources: STEM_IDS });
    }
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(c);
    (s, t, id)
}

fn render(s: &Session) -> Vec<Vec<f32>> {
    render_range(s, Range::new(0, LEN + 4_000), 512)
}

fn max_diff(a: &[Vec<f32>], b: &[Vec<f32>]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| (p - q).abs())).fold(0.0, f32::max)
}

fn peak(a: &[Vec<f32>]) -> f32 {
    a.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()))
}

/// Puts a stem fader automation lane with the given points on the track.
fn lane(s: &mut Session, t: TrackId, stem: u8, points: &[(i64, f32)]) {
    let mut l = AutomationLane::new(AutoParam::Stem(stem));
    for &(at, v) in points {
        l.set_point(at, v);
    }
    s.track_mut(t).unwrap().automation.push(l);
}

fn clip_mut(s: &mut Session, t: TrackId) -> &mut Clip {
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.first_mut().unwrap()
}

/// Samples of an unsplit render of S kept from before stems existed (plain and warped), at
/// every 4999th frame of both channels, as f32 bits.
const BEFORE_PLAIN: [u32; 20] = [
    0, 0, 1044713824, 1052325235, 3196067687, 1042821060, 3195085014, 3193136939, 1043689763, 1041938530, 3185622728, 1048722315, 3199113433,
    3190237197, 3173752802, 3178704733, 3176696353, 1036675462, 3182923605, 3169302246,
];
const BEFORE_WARPED: [u32; 20] = [
    0, 0, 1020166707, 1049906503, 3187167901, 3189732512, 1053429613, 1040590201, 1046247203, 1054962429, 3189539042, 1025238194, 1037860204,
    3159297584, 1046825801, 1049464553, 3165816895, 1044832342, 3174521270, 1026586753,
];

fn picks(out: &[Vec<f32>]) -> Vec<u32> {
    (0..10).flat_map(|k| out.iter().map(move |c| c[1_000 + k * 4_999].to_bits())).collect()
}

/// Unsplit clips ignore stem faders and lanes, and render as they did before stems existed.
#[test]
fn an_unsplit_clip_ignores_stem_faders_and_renders_as_before() {
    for warped in [false, true] {
        let (mut s, t, _) = session(S, false, warped, &[]);
        let before = render(&s);
        let kept = if warped { BEFORE_WARPED } else { BEFORE_PLAIN };
        if warped {
            // The stretcher is C++ built per platform: allow a last-bit difference elsewhere.
            for (got, want) in picks(&before).iter().zip(kept) {
                assert!((f32::from_bits(*got) - f32::from_bits(want)).abs() <= 1e-6, "warped render changed: {got:#x} vs {want:#x}");
            }
        } else {
            assert_eq!(picks(&before), kept, "plain render changed");
        }
        s.track_mut(t).unwrap().mixer.stem_gain_db = [-144.0, 6.0, -20.0, -144.0];
        lane(&mut s, t, BASS as u8, &[(0, 0.0), (20_000, -144.0)]);
        lane(&mut s, t, 0, &[(0, -30.0)]);
        clip_mut(&mut s, t).stem_mask = [false, true, false, true];
        assert_eq!(render(&s), before, "warped {warped}: an unsplit clip ignores the stem faders");
    }
}

/// The unsplit render of one stem (or any extra source) with the same clip.
fn render_of(ch: Vec<Vec<f32>>, warped: bool) -> Vec<Vec<f32>> {
    let id = SourceId(950);
    render(&session(id, false, warped, &[(id, ch)]).0)
}

/// The session at another tempo (the warp map's source is 120 bpm).
fn at_tempo(mut s: Session, bpm: f64) -> Session {
    s.tempo.set_tempo(0, bpm).unwrap();
    s
}

/// With the mask all on and the faders at 0 dB, a split clip sounds like its source: plain, and
/// warped at the source's own tempo (the same stretcher input after the same pre-roll).
#[test]
fn a_split_clip_at_unity_plays_its_source() {
    for (warped, bpm, tol) in [(false, 128.0, 1e-5), (true, 120.0, 1e-4)] {
        let whole = render(&at_tempo(session(S, false, warped, &[]).0, bpm));
        let split = render(&at_tempo(session(S, true, warped, &[]).0, bpm));
        assert!(peak(&whole) > 0.1);
        let d = max_diff(&split, &whole);
        assert!(d <= tol, "warped {warped}: split vs source {d}");
    }
}

/// Stretched to another tempo, each stem plays exactly as an unsplit clip of it would, and the
/// four stay sample-aligned. (The stretcher is a phase vocoder, which is not linear once it
/// stretches, so the stretched stems' sum is not the stretched source sample for sample.)
#[test]
fn stretched_stems_play_as_unsplit_clips_of_themselves() {
    let split = render(&session(S, true, true, &[]).0);
    let parts = stems().map(|ch| render_of(ch, true));
    let summed: Vec<Vec<f32>> = (0..2).map(|c| (0..split[c].len()).map(|i| parts.iter().map(|p| p[c][i]).sum()).collect()).collect();
    assert!(peak(&split) > 0.1);
    assert!(max_diff(&split, &summed) <= 1e-5);
}

/// The stem mask picks which stems play: vocals only is stem A only.
#[test]
fn the_mask_plays_only_the_chosen_stems() {
    for warped in [false, true] {
        let (mut s, t, _) = session(S, true, warped, &[]);
        clip_mut(&mut s, t).stem_mask = [true, false, false, false];
        let vocals = render_of(stems()[0].clone(), warped);
        let d = max_diff(&render(&s), &vocals);
        assert!(d <= 1e-6, "warped {warped}: vocals only vs A: {d}");
    }
}

/// −144 dB on the bass fader removes C, and +6 dB doubles it (+6.02 dB is exactly double).
#[test]
fn a_stem_fader_scales_its_stem() {
    let st = stems();
    let bass = render_of(st[2].clone(), false);
    let whole = render(&session(S, false, false, &[]).0);
    for (db, extra) in [(-144.0f32, -1.0f32), (6.0, 10f32.powf(6.0 / 20.0) - 1.0)] {
        let (mut s, t, _) = session(S, true, false, &[]);
        s.track_mut(t).unwrap().mixer.stem_gain_db[BASS] = db;
        let got = render(&s);
        let want: Vec<Vec<f32>> = (0..2).map(|c| whole[c].iter().zip(&bass[c]).map(|(w, b)| w + extra * b).collect()).collect();
        let d = max_diff(&got, &want);
        assert!(d <= 1e-5, "bass at {db} dB: {d}");
    }
    // Without the bass at all.
    let no_bass = render_of(sum(&[&st[0], &st[1], &st[3]]), false);
    let (mut s, t, _) = session(S, true, false, &[]);
    s.track_mut(t).unwrap().mixer.stem_gain_db[BASS] = -144.0;
    assert!(max_diff(&render(&s), &no_bass) <= 1e-5);
}

/// A `stem:bass` lane ramping 0 → −144 dB fades C out along the ramp.
#[test]
fn stem_automation_follows_the_ramp() {
    let st = stems();
    let bass = render_of(st[2].clone(), false);
    let whole = render(&session(S, false, false, &[]).0);
    let (mut s, t, _) = session(S, true, false, &[]);
    let (a, b) = (10_000i64, 30_000i64);
    lane(&mut s, t, BASS as u8, &[(a, 0.0), (b, -144.0)]);
    let got = render(&s);
    let gain = |x: i64| {
        let db = if x <= a {
            0.0
        } else if x >= b {
            -144.0
        } else {
            -144.0 * (x - a) as f32 / (b - a) as f32
        };
        if db <= -144.0 { 0.0 } else { 10f32.powf(db / 20.0) }
    };
    for c in 0..2 {
        for (i, g) in got[c].iter().enumerate() {
            let want = whole[c][i] - (1.0 - gain(i as i64)) * bass[c][i];
            // The lane is read every 64 samples and interpolated, so a breakpoint's corner is
            // rounded over the 64 samples around it.
            let corner = [a, b].iter().any(|p| (i as i64 - p).abs() < 64);
            let tol = if corner { 2e-3 } else { 2e-4 };
            assert!((g - want).abs() <= tol, "ch {c} at {i}: {g} vs {want}");
            if i as i64 <= a - 64 || i as i64 >= b {
                assert!((g - want).abs() <= 1e-5, "ch {c} at {i}: flat outside the ramp");
            }
        }
    }
}

/// Playback through the engine, a block at a time.
fn play(s: &Session, total: usize, block: usize) -> Vec<Vec<f32>> {
    let mut eng = MixEngine::new(SR as f32, block);
    let mut out = vec![vec![0.0f32; block]; 2];
    let mut played: Vec<Vec<f32>> = vec![Vec::new(); 2];
    let mut pos = 0usize;
    while pos < total {
        let n = block.min(total - pos);
        eng.render(s, pos as i64, n, &mut out);
        for (p, o) in played.iter_mut().zip(&out) {
            p.extend_from_slice(&o[..n]);
        }
        pos += n;
    }
    played
}

/// Playback and a bounce (`render_range`, 1024 frames) of a split clip with a mask, a fader
/// and a lane (silencing the bass for a while, then bringing it back) are the same audio. Plain
/// clips match at any callback size. A warped clip's stretcher output depends on how the
/// timeline is cut into blocks (unsplit clips too), so it matches at the bounce's block size.
#[test]
fn a_bounce_of_a_split_clip_equals_playback() {
    for (warped, block) in [(false, 128), (false, 1024), (true, 1024)] {
        let (mut s, t, _) = session(S, true, warped, &[]);
        clip_mut(&mut s, t).stem_mask = [true, true, true, false];
        s.track_mut(t).unwrap().mixer.stem_gain_db[1] = -6.0;
        lane(&mut s, t, BASS as u8, &[(8_000, 0.0), (20_000, -144.0), (30_000, -144.0), (40_000, 3.0)]);
        let total = (LEN + 4_000) as usize;
        let bounce = render_range(&s, Range::new(0, total as i64), 1024);
        assert!(peak(&bounce) > 0.1);
        assert_eq!(play(&s, total, block), bounce, "warped {warped}, playback blocks of {block}");
    }
}

/// A stem missing from the pool is skipped; the others still play.
#[test]
fn a_missing_stem_is_skipped() {
    let st = stems();
    let (mut s, _, _) = session(S, true, false, &[]);
    s.pool.remove(STEM_IDS[1]);
    let want = render_of(sum(&[&st[0], &st[2], &st[3]]), false);
    assert!(max_diff(&render(&s), &want) <= 1e-5);
}

/// Clip renders (`render_clips`: consolidate, clip gain render, AudioSuite) render the clip as it
/// sounds: its stem mask applies, the track's stem faders and lanes (like its volume) don't.
#[test]
fn clip_renders_apply_the_mask_but_not_the_stem_faders() {
    let (mut s, t, _) = session(S, true, false, &[]);
    clip_mut(&mut s, t).stem_mask = [false, true, true, true];
    s.track_mut(t).unwrap().mixer.stem_gain_db = [0.0, -144.0, 6.0, -20.0];
    lane(&mut s, t, 3, &[(0, -144.0)]);
    let r = Range::new(0, LEN + 4_000);
    let got = render_clips(&s, t, r);
    let st = stems();
    let id = SourceId(950);
    let (o, ot, _) = session(id, false, false, &[(id, sum(&[&st[1], &st[2], &st[3]]))]);
    let want = render_clips(&o, ot, r);
    assert!(peak(&want) > 0.1);
    assert!(max_diff(&got, &want) <= 1e-5);
}

/// A stem lane replaces the stem fader (the engine writes lanes as absolute fader values, like
/// volume): a −6 dB fader with a written dip plays −6 dB outside the dip, not −12. In Write mode
/// the fader plays, as for volume.
#[test]
fn a_stem_lane_replaces_the_fader() {
    let bass = render_of(stems()[2].clone(), false);
    let whole = render(&session(S, false, false, &[]).0);
    let at_db = |db: f32| -> Vec<Vec<f32>> {
        (0..2).map(|c| whole[c].iter().zip(&bass[c]).map(|(w, b)| w + (10f32.powf(db / 20.0) - 1.0) * b).collect()).collect()
    };
    let (mut s, t, _) = session(S, true, false, &[]);
    s.track_mut(t).unwrap().mixer.stem_gain_db[BASS] = -6.0;
    lane(&mut s, t, BASS as u8, &[(0, -6.0), (10_000, -6.0), (20_000, -30.0), (30_000, -6.0)]);
    let got = render(&s);
    let want = at_db(-6.0);
    for c in 0..2 {
        for i in (0..9_900).chain(30_100..got[c].len()) {
            assert!((got[c][i] - want[c][i]).abs() <= 1e-5, "ch {c} at {i}: {} vs {}", got[c][i], want[c][i]);
        }
    }
    // Write mode: the fader, not the lane.
    s.track_mut(t).unwrap().mixer.automation_mode = soundcraft_model::AutomationMode::Write;
    s.track_mut(t).unwrap().mixer.stem_gain_db[BASS] = -144.0;
    assert!(max_diff(&render(&s), &at_db(-144.0)) <= 1e-5);
}

/// A non-finite stem fader plays at 0 dB rather than poisoning the mix.
#[test]
fn a_non_finite_stem_fader_plays_at_unity() {
    let whole = render(&session(S, false, false, &[]).0);
    let (mut s, t, _) = session(S, true, false, &[]);
    s.track_mut(t).unwrap().mixer.stem_gain_db = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0];
    let got = render(&s);
    assert!(got.iter().flatten().all(|x| x.is_finite()));
    assert!(max_diff(&got, &whole) <= 1e-5);
}

/// A re-split clip (same clip, new stem audio) restarts its stem players: the next block sounds
/// exactly like a fresh start there, with nothing left over from the old stems.
#[test]
fn a_re_split_clip_restarts_its_stem_players() {
    let st = stems();
    let other: Vec<Vec<f32>> = st[3].iter().map(|c| c.iter().map(|x| -x).collect()).collect();
    let (mut s, t, _) = session(S, true, true, &[(SourceId(960), other)]);
    // Vocals only, the stem that changes, so only its player is compared.
    clip_mut(&mut s, t).stem_mask = [true, false, false, false];
    let mut resplit = s.clone();
    if let Some(set) = &mut clip_mut(&mut resplit, t).stems {
        set.sources[0] = SourceId(960);
    }
    let block = 1024;
    let mut eng = MixEngine::new(SR as f32, block);
    let mut out = vec![vec![0.0f32; block]; 2];
    for k in 0..8 {
        eng.render(&s, k * block as i64, block, &mut out);
    }
    eng.render(&resplit, 8 * block as i64, block, &mut out);
    let mut fresh = MixEngine::new(SR as f32, block);
    let mut want = vec![vec![0.0f32; block]; 2];
    fresh.render(&resplit, 8 * block as i64, block, &mut want);
    assert!(peak(&want) > 0.01);
    assert_eq!(out, want);
}
