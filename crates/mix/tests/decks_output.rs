//! The live decks through the whole mix engine: into the main bus, through the master fader and
//! the output stage, which keeps them under -1 dBTP whatever the master fader does.

use soundcraft_audio_io::AudioBuffer;
use soundcraft_mix::{BeatGrid, DeckCmd, DeckId, DeckTrack, HOT_CUES, MixEngine, PreparedDeck, XfadeSide};
use soundcraft_model::{ChannelFormat, Session, SourceAudio, TrackKind};
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
