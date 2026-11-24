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
