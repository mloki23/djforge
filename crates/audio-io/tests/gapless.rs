//! An MP3's priming (what a gapless decode trims at its start) against ffmpeg's: LAME-tagged
//! files that ffmpeg makes, and a fixture without the tag. Needs ffmpeg and ffprobe; says so and
//! checks nothing without them.

use soundcraft_audio_io::mp3gain::gapless_priming;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tools() -> bool {
    let found = ["ffmpeg", "ffprobe"].iter().all(|t| Command::new(t).arg("-version").output().is_ok_and(|o| o.status.success()));
    if !found {
        println!("note: ffmpeg or ffprobe is not on the PATH; this test checks nothing");
    }
    found
}

/// ffprobe's start time of `file`'s audio, in samples at `rate`.
fn start_samples(file: &Path, rate: u32) -> u32 {
    let o = Command::new("ffprobe").args(["-v", "error", "-show_entries", "stream=start_time", "-of", "csv=p=0"]).arg(file).output().unwrap();
    let secs: f64 = String::from_utf8_lossy(&o.stdout).trim().parse().unwrap();
    (secs * f64::from(rate)).round() as u32
}

/// `file` decoded by ffmpeg (which trims the priming), left channel.
fn ffmpeg_decode(file: &Path) -> Vec<f32> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(file).args(["-ac", "1", "-f", "f32le", "-"]).output().unwrap();
    o.stdout.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

#[test]
fn the_priming_is_the_lame_tag_s_delay_and_symphonia_trims_it() {
    if !tools() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("sc-gapless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for rate in [48_000u32, 44_100] {
        let file: PathBuf = dir.join(format!("lame{rate}.mp3"));
        let src = format!("sine=f=440:d=2:sample_rate={rate}");
        let o = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", &src, "-c:a", "libmp3lame", "-b:a", "192k"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let bytes = std::fs::read(&file).unwrap();
        let (priming, at) = gapless_priming(&bytes).unwrap();
        assert_eq!(at, rate);
        assert_eq!(priming, start_samples(&file, rate));
        assert_eq!(priming, 1105, "{rate}");
        // Symphonia's gapless decode starts where ffmpeg's does: the priming is trimmed.
        let (_, ours) = soundcraft_audio_io::decode(&bytes, Some("mp3")).unwrap();
        let theirs = ffmpeg_decode(&file);
        let n = 4_000;
        let worst = (0..n).map(|i| (ours.channels[0][i] - theirs[i]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "{rate}: off by {worst}");
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Without a LAME tag nothing is trimmed.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    assert_eq!(gapless_priming(&std::fs::read(&fixture).unwrap()), Some((0, 44_100)));
    assert_eq!(start_samples(&fixture, 44_100), 0);
    assert_eq!(gapless_priming(b"not an mp3"), None);
}
