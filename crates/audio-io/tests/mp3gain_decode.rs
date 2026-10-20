//! A global_gain step must scale the decoded audio by exactly 2^(steps/4): checked through a
//! real decode of every fixture.

use soundcraft_audio_io::{decode, mp3gain};

const FIXTURES: [(&str, &[u8]); 6] = [
    ("cbr128_44k_stereo", include_bytes!("fixtures/mp3/cbr128_44k_stereo.mp3")),
    ("vbr_48k_stereo", include_bytes!("fixtures/mp3/vbr_48k_stereo.mp3")),
    ("cbr64_44k_mono", include_bytes!("fixtures/mp3/cbr64_44k_mono.mp3")),
    ("mpeg2_22k_mono", include_bytes!("fixtures/mp3/mpeg2_22k_mono.mp3")),
    ("mpeg2_24k_stereo", include_bytes!("fixtures/mp3/mpeg2_24k_stereo.mp3")),
    ("mpeg25_11k_mono", include_bytes!("fixtures/mp3/mpeg25_11k_mono.mp3")),
];

#[test]
fn decoded_audio_scales_by_exact_quarter_powers_of_two() {
    for (name, bytes) in FIXTURES {
        let (_, x) = decode(bytes, Some("mp3")).unwrap();
        for steps in -3..=2 {
            let (_, y) = decode(&mp3gain::adjust(bytes, steps).unwrap(), Some("mp3")).unwrap();
            assert_eq!((y.num_channels(), y.frames()), (x.num_channels(), x.frames()), "{name} {steps}");
            let (x, y) = (x.interleaved(), y.interleaved());
            let (xy, xx) =
                x.iter().zip(&y).fold((0.0f64, 0.0f64), |(xy, xx), (&a, &b)| (xy + f64::from(a) * f64::from(b), xx + f64::from(a) * f64::from(a)));
            let ratio = xy / xx;
            let want = 2f64.powf(f64::from(steps) / 4.0);
            let off_db = 20.0 * (ratio / want).log10();
            assert!(off_db.abs() < 0.01, "{name} {steps}: ratio {ratio}, want {want} ({off_db} dB off)");
            assert!((20.0 * want.log10() - f64::from(steps) * mp3gain::STEP_DB).abs() < 1e-12);
            let err = x.iter().zip(&y).map(|(&a, &b)| (f64::from(b) - ratio * f64::from(a)).abs()).fold(0.0, f64::max);
            assert!(err < 1e-4, "{name} {steps}: max residual {err}");
        }
    }
}
