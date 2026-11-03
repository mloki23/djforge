use soundcraft_midi::patterns::*;
use soundcraft_midi::{Key, PPQ};

const BEAT: i64 = PPQ as i64;
const SIXTEENTH: i64 = BEAT / 4;

fn spec(style: PatternStyle, bars: u32, key: Option<&str>) -> PatternSpec {
    PatternSpec { kind: style.kind(), style, bars, beats_per_bar: 4, key: key.map(|k| Key::parse(k).unwrap()), seed: 7 }
}

fn gen_ok(style: PatternStyle, bars: u32, key: Option<&str>) -> Generated {
    generate(&spec(style, bars, key)).unwrap()
}

#[test]
fn bass_and_synth_notes_stay_in_key() {
    for key in ["8A", "8B", "11A", "4B", "1A", "Ebm", "F#"] {
        let k = Key::parse(key).unwrap();
        for kind in [PatternKind::Bass, PatternKind::Synth] {
            for style in PatternStyle::of_kind(kind) {
                for bars in [1, 4, 8, 16] {
                    let g = gen_ok(style, bars, Some(key));
                    assert!(!g.sequence.notes.is_empty(), "{} {key}", style.name());
                    for n in &g.sequence.notes {
                        assert!(k.contains(n.pitch), "{} in {key}: pitch {} out of key", style.name(), n.pitch);
                    }
                }
            }
        }
    }
}

#[test]
fn four_floor_is_kick_on_every_beat_only() {
    for bars in [1, 4, 16, 32] {
        let g = gen_ok(PatternStyle::FourFloor, bars, None);
        let starts: Vec<i64> = g.sequence.notes.iter().map(|n| n.start).collect();
        let expected: Vec<i64> = (0..i64::from(bars) * 4).map(|b| b * BEAT).collect();
        assert_eq!(starts, expected);
        assert!(g.sequence.notes.iter().all(|n| n.pitch == KICK));
        assert!(g.automation.is_empty());
    }
}

#[test]
fn rolling_bass_never_shares_a_sixteenth_with_a_kick() {
    for bars in [1, 4, 16] {
        let kicks: Vec<i64> =
            gen_ok(PatternStyle::TranceFull, bars, None).sequence.notes.iter().filter(|n| n.pitch == KICK).map(|n| n.start / SIXTEENTH).collect();
        assert_eq!(kicks.len(), bars as usize * 4);
        let bass = gen_ok(PatternStyle::Rolling, bars, Some("8A"));
        // Three 16ths per beat.
        assert_eq!(bass.sequence.notes.len(), bars as usize * 12);
        for n in &bass.sequence.notes {
            assert!(!kicks.contains(&(n.start / SIXTEENTH)), "bass at {} under a kick", n.start);
            assert!(n.end() <= (n.start / SIXTEENTH + 1) * SIXTEENTH, "bass note spills into the next 16th");
        }
    }
}

#[test]
fn trance_full_layers() {
    let g = gen_ok(PatternStyle::TranceFull, 2, None);
    let at = |pitch: u8| g.sequence.notes.iter().filter(move |n| n.pitch == pitch).map(|n| n.start).collect::<Vec<_>>();
    assert_eq!(at(OPEN_HAT), (0..8).map(|b| b * BEAT + BEAT / 2).collect::<Vec<_>>());
    assert_eq!(at(CLAP), (0..4).map(|i| BEAT + i * 2 * BEAT).collect::<Vec<_>>());
    assert_eq!(at(CLOSED_HAT).len(), 2 * 12);
    assert!(g.sequence.notes.iter().all(|n| n.channel == 9));
}

#[test]
fn fill_ends_with_rising_snare_roll() {
    for bars in [1, 4] {
        let g = gen_ok(PatternStyle::Fill, bars, None);
        let snares: Vec<_> = g.sequence.notes.iter().filter(|n| n.pitch == SNARE).collect();
        assert_eq!(snares.len(), 16);
        let last_bar = i64::from(bars - 1) * 4 * BEAT;
        assert!(snares.iter().all(|n| n.start >= last_bar));
        assert!(snares.windows(2).all(|w| w[1].velocity > w[0].velocity));
        assert!(g.sequence.notes.iter().any(|n| n.pitch == KICK) && g.sequence.notes.iter().any(|n| n.pitch == OPEN_HAT));
    }
}

#[test]
fn riser_automation_is_monotonic_and_ends_at_one() {
    for bars in [1, 4, 16] {
        let g = gen_ok(PatternStyle::Riser, bars, None);
        assert_eq!(g.sequence.notes.len(), 1);
        let note = g.sequence.notes[0];
        assert_eq!((note.start, note.end(), note.pitch), (0, g.length_ticks, FX_PITCH));
        let params: Vec<_> = g.automation.iter().map(|c| c.param.name()).collect();
        assert_eq!(params, ["cutoff", "volume"]);
        for c in &g.automation {
            assert!(c.points.windows(2).all(|w| w[1].0 > w[0].0 && w[1].1 >= w[0].1), "{} not monotonic", c.param.name());
            assert!(c.points.iter().all(|p| (0.0..=1.0).contains(&p.1)));
            assert_eq!(c.points.last().copied(), Some((g.length_ticks, 1.0)));
            assert_eq!(c.points.first().map(|p| p.0), Some(0));
        }
        assert_eq!(g.automation[0].points[0].1, 0.05);
        assert_eq!(g.automation[1].points[0].1, 0.3);
    }
}

#[test]
fn downlifter_mirrors_riser() {
    let g = gen_ok(PatternStyle::Downlifter, 4, None);
    for c in &g.automation {
        assert!(c.points.windows(2).all(|w| w[1].1 <= w[0].1));
        assert_eq!(c.points.first().copied(), Some((0, 1.0)));
    }
}

#[test]
fn impact_is_one_beat_at_zero() {
    let g = gen_ok(PatternStyle::Impact, 4, None);
    assert_eq!(g.sequence.notes.len(), 1);
    assert_eq!((g.sequence.notes[0].start, g.sequence.notes[0].length), (0, BEAT));
}

#[test]
fn clip_length_is_exact_and_notes_fit() {
    for style in PatternStyle::ALL {
        for bars in [1u32, 4, 16, 32] {
            let g = gen_ok(style, bars, Some("8A"));
            assert_eq!(g.length_ticks, i64::from(bars) * 4 * 960, "{}", style.name());
            assert!(g.sequence.notes.iter().all(|n| n.start >= 0 && n.length > 0 && n.end() <= g.length_ticks), "{}", style.name());
        }
    }
}

#[test]
fn same_seed_same_output_and_seed_varies_details() {
    for style in PatternStyle::ALL {
        assert_eq!(gen_ok(style, 8, Some("5A")), gen_ok(style, 8, Some("5A")), "{}", style.name());
    }
    let mut other = spec(PatternStyle::TranceFull, 4, None);
    other.seed = 8;
    let a = gen_ok(PatternStyle::TranceFull, 4, None);
    let b = generate(&other).unwrap();
    assert_ne!(a, b);
    // Only velocities change with the seed.
    let timing = |g: &Generated| g.sequence.notes.iter().map(|n| (n.start, n.pitch, n.length)).collect::<Vec<_>>();
    assert_eq!(timing(&a), timing(&b));
}

#[test]
fn root_fifth_follows_progression() {
    // A minor: i–VI–III–VII = A F C G, one chord per bar under 8 bars, per two bars from 8.
    let roots = |bars: u32| {
        let g = gen_ok(PatternStyle::RootFifth, bars, Some("Am"));
        g.sequence.notes.iter().filter(|n| (n.start / BEAT) % 4 == 0).map(|n| n.pitch % 12).collect::<Vec<_>>()
    };
    assert_eq!(roots(4), [9, 5, 0, 7]);
    assert_eq!(roots(8), [9, 9, 5, 5, 0, 0, 7, 7]);
}

#[test]
fn key_required_where_needed() {
    for style in PatternStyle::ALL {
        let r = generate(&spec(style, 4, None));
        if style.needs_key() {
            assert_eq!(r, Err(PatternError::KeyRequired(style.name())));
        } else {
            assert!(r.is_ok(), "{}", style.name());
        }
    }
    assert!(PatternStyle::Rolling.needs_key() && PatternStyle::Arp.needs_key() && !PatternStyle::Riser.needs_key());
}

#[test]
fn rejects_bad_specs() {
    let mut s = spec(PatternStyle::FourFloor, 4, None);
    s.beats_per_bar = 3;
    assert_eq!(generate(&s), Err(PatternError::UnsupportedMeter(3)));
    let mut s = spec(PatternStyle::FourFloor, 0, None);
    assert!(matches!(generate(&s), Err(PatternError::BadBars { .. })));
    s.bars = MAX_BARS + 1;
    assert!(matches!(generate(&s), Err(PatternError::BadBars { .. })));
    let mut s = spec(PatternStyle::Rolling, 4, Some("8A"));
    s.kind = PatternKind::Drums;
    assert!(matches!(generate(&s), Err(PatternError::StyleMismatch { .. })));
}

#[test]
fn kind_and_style_names_round_trip() {
    for k in PatternKind::ALL {
        assert_eq!(PatternKind::parse(k.name()), Ok(k));
    }
    for s in PatternStyle::ALL {
        assert_eq!(PatternStyle::parse(s.name()), Ok(s));
    }
    assert_eq!(PatternStyle::parse("FOUR_FLOOR"), Ok(PatternStyle::FourFloor));
    assert!(PatternStyle::parse("dubstep").is_err());
    assert!(PatternKind::parse("vocals").is_err());
}
