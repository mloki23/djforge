//! The live decks through the whole mix engine: into the main bus, through the master fader and
//! the output stage, which keeps them under -1 dBTP whatever the master fader does.

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{BeatGrid, DeckCmd, DeckId, DeckTrack, HOT_CUES, MixEngine, PreparedDeck, XfadeSide};
use soundcraft_model::{ChannelFormat, Clip, Route, Session, SourceAudio, SourceId, TrackKind};
use std::sync::Arc;

const SR: f64 = 48_000.0;

fn loud(hz: f64) -> Box<PreparedDeck> {
    let n = (5.0 * SR) as usize;
    let ch: Vec<f32> = (0..n).map(|i| if (hz * i as f64 / SR).fract() < 0.5 { 0.9 } else { -0.9 }).collect();
    let source = Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR as u32, channels: vec![ch.clone(), ch] }));
    let track = DeckTrack {
        source,
        grid: BeatGrid::constant(0.0, 22_500.0, n as f64),
        bpm: Some(128.0),
        key: None,
        cues: [None; HOT_CUES],
        title: String::new(),
        artist: String::new(),
        path: String::new(),
        grid_from: None,
        sections: Vec::new(),
    };
    Box::new(PreparedDeck::new(Arc::new(track), SR as f32))
}

#[test]
fn a_boosted_master_fader_cannot_take_the_decks_over_minus_one_dbtp() {
    let mut s = Session::default();
    let m = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
    s.track_mut(m).unwrap().mixer.volume_db = 6.0;
    let mut eng = MixEngine::new(SR as f32, 512);
    eng.enable_decks();
    eng.monitor_only = true;
    for (id, hz) in [(DeckId::A, 211.0), (DeckId::B, 2_917.0)] {
        assert!(eng.deck_load(id, loud(hz)).is_none());
        eng.deck(DeckCmd::Assign { deck: id, side: XfadeSide::Thru });
        eng.deck(DeckCmd::Play(id));
    }
    let mut out = vec![vec![0.0f32; 512]; 2];
    let mut meter = soundcraft_dsp::meter::LoudnessMeter::new(SR as f32, 2);
    for _ in 0..300 {
        eng.render(&s, 0, 512, &mut out);
        meter.process(&out, 512);
    }
    let tp = meter.true_peak_db();
    assert!(tp <= -0.95 && tp > -4.0, "true peak {tp} dBTP");
    // Stopped and quiet, the output stage lets the arrangement through untouched.
    eng.deck(DeckCmd::StopAll);
    for _ in 0..100 {
        eng.render(&s, 0, 512, &mut out);
    }
    assert!(!eng.decks_awake());
}

/// A session with one track playing a 440 Hz sine (0.5) from sample 0 for 4 s.
fn sine_session() -> Session {
    let mut s = Session::default();
    let n = (4.0 * SR) as usize;
    let sine: Vec<f32> = (0..n).map(|i| (std::f64::consts::TAU * 440.0 * i as f64 / SR).sin() as f32 * 0.5).collect();
    s.pool.insert(SourceId(77), Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR as u32, channels: vec![sine.clone(), sine] })));
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Sine"));
    let clip = s.new_clip_id();
    s.track_mut(t).unwrap().playlist_mut().unwrap().clips.push(Clip::audio(clip, "sine", SourceId(77), 0, 0, n as i64));
    s.track_mut(t).unwrap().mixer.pan = vec![-1.0, 1.0];
    s.track_mut(t).unwrap().mixer.output = Route::Main;
    s
}

#[test]
fn arrangement_play_after_the_decks_hands_over_without_a_click() {
    let s = sine_session();
    let mut eng = MixEngine::new(SR as f32, 512);
    eng.enable_decks();
    // A quiet, slow deck: its own 2 ms fade is far gentler than the sine's slope.
    let n = (5.0 * SR) as usize;
    let ch: Vec<f32> = (0..n).map(|i| (std::f64::consts::TAU * 60.0 * i as f64 / SR).sin() as f32 * 0.05).collect();
    let source = Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR as u32, channels: vec![ch.clone(), ch] }));
    let track = DeckTrack {
        source,
        grid: BeatGrid::constant(0.0, 22_500.0, n as f64),
        bpm: Some(128.0),
        key: None,
        cues: [None; HOT_CUES],
        title: String::new(),
        artist: String::new(),
        path: String::new(),
        grid_from: None,
        sections: Vec::new(),
    };
    assert!(eng.deck_load(DeckId::A, Box::new(PreparedDeck::new(Arc::new(track), SR as f32))).is_none());
    eng.deck(DeckCmd::Play(DeckId::A));
    let mut out = vec![vec![0.0f32; 512]; 2];
    let mut heard = Vec::new();
    eng.monitor_only = true;
    for _ in 0..40 {
        eng.render(&s, 0, 512, &mut out);
        heard.extend_from_slice(&out[0]);
    }
    // Arrangement Play: the decks stop (as playback does) and the sine plays from sample 0, through
    // the decks' quiet tail and past it.
    eng.deck(DeckCmd::StopAll);
    eng.monitor_only = false;
    for b in 0..150 {
        eng.render(&s, b * 512, 512, &mut out);
        heard.extend_from_slice(&out[0]);
    }
    let slope = std::f64::consts::TAU * 440.0 / SR * 0.5;
    let worst = heard.windows(2).map(|w| f64::from((w[1] - w[0]).abs())).fold(0.0, f64::max);
    assert!(worst < slope * 1.05, "step {worst} vs the sine's slope {slope}");
    // And the sine plays undelayed and untouched once the output stage has let go.
    let tail = &heard[heard.len() - 512..];
    let at = (149 * 512) as f64;
    let want: Vec<f32> = (0..512).map(|i| (std::f64::consts::TAU * 440.0 * (at + i as f64) / SR).sin() as f32 * 0.5).collect();
    assert!(tail.iter().zip(&want).all(|(a, b)| (a - b).abs() < 1e-4));
}

/// A deck of one click (0.5) at 1 s, cued and on the master.
fn click() -> Box<PreparedDeck> {
    let n = (5.0 * SR) as usize;
    let ch: Vec<f32> = (0..n).map(|i| if i == 48_000 { 0.5 } else { 0.0 }).collect();
    let source = Arc::new(SourceAudio::new(AudioBuffer { sample_rate: SR as u32, channels: vec![ch.clone(), ch] }));
    let track = DeckTrack {
        source,
        grid: BeatGrid::constant(0.0, 22_500.0, n as f64),
        bpm: Some(128.0),
        key: None,
        cues: [None; HOT_CUES],
        title: String::new(),
        artist: String::new(),
        path: String::new(),
        grid_from: None,
        sections: Vec::new(),
    };
    Box::new(PreparedDeck::new(Arc::new(track), SR as f32))
}

/// The headphone output (left) of the click deck through the whole engine at headphone mix `mix`.
fn phones_at(s: &Session, mix: f32) -> Vec<f32> {
    let mut eng = MixEngine::new(SR as f32, 512);
    eng.enable_decks();
    eng.set_headphones(true);
    eng.monitor_only = true;
    assert!(eng.deck_load(DeckId::A, click()).is_none());
    eng.deck(DeckCmd::Assign { deck: DeckId::A, side: XfadeSide::Thru });
    eng.deck(DeckCmd::HeadphoneCue { deck: DeckId::A, on: true });
    eng.deck(DeckCmd::HeadphoneMix { value: mix });
    eng.deck(DeckCmd::HeadphoneLevel { db: 0.0 });
    eng.deck(DeckCmd::Play(DeckId::A));
    let mut out = vec![vec![0.0f32; 512]; 2];
    let mut heard = Vec::new();
    for b in 0..150 {
        eng.render(s, b * 512, 512, &mut out);
        heard.extend_from_slice(&eng.headphones()[0][..512]);
    }
    heard
}

fn loudest(x: &[f32]) -> (usize, f32) {
    x.iter().enumerate().map(|(i, v)| (i, v.abs())).fold((0, 0.0), |m, (i, v)| if v > m.1 { (i, v) } else { m })
}

#[test]
fn cue_and_master_line_up_in_the_headphones_through_a_lookahead_master_insert() {
    let mut s = Session::default();
    let m = s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
    let mut maximizer = soundcraft_model::Insert::new("maximizer");
    maximizer.params.insert("ceiling".into(), 0.0);
    s.track_mut(m).unwrap().mixer.inserts[0] = Some(maximizer);
    let (cue, master, both) = (phones_at(&s, 0.0), phones_at(&s, 1.0), phones_at(&s, 0.5));
    let (at_cue, _) = loudest(&cue);
    let (at_master, _) = loudest(&master);
    assert!(at_cue > 48_000, "the click came through: {at_cue}");
    assert_eq!(at_cue, at_master, "the cue bus is delayed as the master is");
    // Half way: the two coincide, so the sum is one click at √2 × each, not two at 0.7 each.
    let (at_both, peak) = loudest(&both);
    assert_eq!(at_both, at_cue);
    let one = loudest(&cue).1;
    assert!((peak / one - std::f32::consts::SQRT_2).abs() < 0.05, "{peak} vs {one}");
    let echo = both.iter().enumerate().filter(|(i, v)| i.abs_diff(at_both) > 8 && v.abs() > one * 0.3).count();
    assert_eq!(echo, 0, "no second click (comb)");
}
