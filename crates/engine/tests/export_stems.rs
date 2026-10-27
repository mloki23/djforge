//! `dj.export_set` with `stems: true` writes an NI STEM file for each split track: five ALAC
//! tracks (the copy, then drums, bass, other and vocals) and the `stem` box at `moov/udta/stem`.
//! The stems come from the session (a fake split); no test touches the stem cache. Tests that
//! need ffmpeg say so and pass when it isn't there.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions, FileFormat, encode};
use soundcraft_engine::Engine;
use soundcraft_engine::cmd::{PrepAnalyse, export_job_in};
use soundcraft_model::{ClipId, STEMS, StemSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// A test's own folder, removed when the test ends, failed or not.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("djforge-export-stems-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

fn analysis() -> PrepAnalyse {
    Arc::new(|_: &str, _: &dyn Fn() -> bool| Ok(json!({"camelot": "8A", "bpm": 120.0})))
}

fn tool(name: &str) -> bool {
    let found = Command::new(name).arg("-version").output().is_ok_and(|o| o.status.success());
    if !found {
        println!("note: {name} is not on the PATH; this test checks nothing");
    }
    found
}

/// Each stem's frequency (Hz), in the session's stem order (vocals, drums, bass, other).
const HZ: [f64; 4] = [700.0, 100.0, 50.0, 300.0];

/// A two-track set at 120 bpm in a 48 kHz session named "test set": a.wav (30 s at 44.1 kHz)
/// on bars 1..16, then a second WAV on bar 8, A -> B on bar 9. With `split`, A's clip is split
/// into stems that are sines at [`HZ`] of amplitude 0.1 (the drums `drums`), stored at half
/// level (`scale_log2` 1).
fn set(lib: &Path, split: bool) -> Engine {
    set_with(lib, split, 0.1)
}

fn set_with(lib: &Path, split: bool, drums: f64) -> Engine {
    std::fs::create_dir_all(lib).unwrap();
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let mut clips = Vec::new();
    for (name, bar) in [("a.wav", 1), ("b.wav", 8)] {
        let path = lib.join(name);
        let buf = AudioBuffer { sample_rate: 44_100, channels: vec![vec![0.1; 30 * 44_100]] };
        std::fs::write(&path, encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
        let beats: Vec<f64> = (0..60).map(|k| k as f64 * 0.5).collect();
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        clips.push(ClipId(r["clip"].as_u64().unwrap()));
    }
    e.execute("dj.transition", &json!({"out_clip": clips[0].0, "in_clip": clips[1].0, "entry_bar": 9, "build_bars": 2, "out_bars": 2})).unwrap();
    if split {
        let s = e.session_mut();
        let src = s.find_clip(clips[0]).unwrap().1.source().unwrap();
        let frames = s.pool.get(src).unwrap().buffer.frames();
        let rate = f64::from(s.sample_rate.hz());
        let sources = HZ.map(|hz| {
            let level = if hz == HZ[1] { drums } else { 0.1 };
            let x: Vec<f32> = (0..frames).map(|i| (level / 2.0 * (std::f64::consts::TAU * hz * i as f64 / rate).sin()) as f32).collect();
            let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: vec![x.clone(), x] };
            soundcraft_engine::io::add_source(s, "stem", buf, None, FileFormat::Wav)
        });
        s.find_clip_mut(clips[0]).unwrap().stems = Some(StemSet { key: "fake".into(), sources, scale_log2: 1 });
    }
    e
}

fn export(e: &Engine, root: &Path) -> Value {
    export_job_in(e, &json!({"targets": ["rekordbox"], "stems": true}), Some(root), analysis()).unwrap().wait().unwrap()
}

/// Stream `n` of `file` decoded by ffmpeg: its left channel at the file's rate.
fn decoded(file: &Path, n: usize) -> Vec<f32> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(file).args(["-map", &format!("0:a:{n}"), "-f", "f32le", "-"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    o.stdout.as_chunks::<8>().0.iter().map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

#[test]
fn a_split_track_gets_an_ni_stem_file() {
    if !(tool("ffmpeg") && tool("ffprobe")) {
        return;
    }
    let dir = scratch("split");
    let e = set(&dir.join("lib"), true);
    let r = export(&e, &dir.join("prepped"));
    let out = dir.join("prepped/test set");
    let stems = r["stems"].as_array().unwrap_or_else(|| panic!("{r}"));
    assert_eq!(stems.len(), 1, "{r}");
    let file = PathBuf::from(stems[0]["stem_file"].as_str().unwrap_or_else(|| panic!("{r}")));
    assert_eq!(file, out.join("a.stem.mp4"));
    // No temporary files are left.
    let left: Vec<String> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(left.iter().all(|n| !n.starts_with(".djforge-stems")), "{left:?}");

    // Five ALAC streams.
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,channels,sample_rate", "-of", "csv=p=0"])
        .arg(&file)
        .output()
        .unwrap();
    let lines: Vec<String> = String::from_utf8_lossy(&probe.stdout).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert!(lines.iter().all(|l| l.starts_with("alac,44100,2")), "{lines:?}");

    // The stem box: valid JSON, NI's stem order.
    let bytes = std::fs::read(&file).unwrap();
    let paths: Vec<String> = soundcraft_audio_io::mp4box::walk(&bytes).unwrap().into_iter().map(|b| b.path).collect();
    assert!(paths.iter().any(|p| p == "moov/udta/stem"), "{paths:?}");
    let meta: Value = serde_json::from_slice(soundcraft_audio_io::mp4box::stem_payload(&bytes).unwrap()).unwrap();
    let names: Vec<&str> = meta["stems"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Drums", "Bass", "Other", "Vocals"]);

    // The mix is the copy (a.wav as it is); each stem is the session's, back at 44.1 kHz and at
    // its level (twice what is stored): stream 1 is the drums, a 100 Hz sine of amplitude 0.1.
    let mix = decoded(&file, 0);
    assert_eq!(mix.len(), 30 * 44_100);
    assert!(mix.iter().all(|x| (x - 0.1).abs() < 1e-4));
    for (stream, name) in [(1, "drums"), (2, "bass"), (3, "other"), (4, "vocals")] {
        let hz = HZ[STEMS.iter().position(|s| *s == name).unwrap()];
        let x = decoded(&file, stream);
        assert_eq!(x.len(), 30 * 44_100, "{name}");
        let worst = (44_100..29 * 44_100)
            .map(|i| (f64::from(x[i]) - 0.1 * (std::f64::consts::TAU * hz * i as f64 / 44_100.0).sin()).abs())
            .fold(0.0, f64::max);
        assert!(worst < 2e-3, "{name}: off by {worst}");
    }

    // Exported again, the stem file is replaced; a foreign file of its name never is.
    let again = export(&e, &dir.join("prepped"));
    assert!(again["stems"][0]["stem_file"].is_string(), "{again}");
    std::fs::write(&file, b"mine").unwrap();
    let r = export(&e, &dir.join("prepped"));
    assert!(r["stems"][0]["skipped"].as_str().unwrap().contains("not a stem file an export wrote"), "{r}");
    assert_eq!(std::fs::read(&file).unwrap(), b"mine");
}

#[test]
fn without_a_split_track_stems_are_skipped() {
    let dir = scratch("unsplit");
    let e = set(&dir.join("lib"), false);
    let r = export(&e, &dir.join("prepped"));
    assert_eq!(r["stems"], json!({"skipped": "no track of the set is split"}), "{r}");
    let report = std::fs::read_to_string(dir.join("prepped/test set/test set.export.txt")).unwrap();
    assert!(report.contains("Stem files: no track of the set is split"), "{report}");
}

/// A stem louder than full scale (drums peaking at 1.6) would clip in ALAC: all five tracks are
/// halved, so they still add up, and the file says it plays 6 dB quieter.
#[test]
fn a_stem_above_full_scale_halves_the_whole_file() {
    if !tool("ffmpeg") {
        return;
    }
    let dir = scratch("loud");
    let e = set_with(&dir.join("lib"), true, 1.6);
    let r = export(&e, &dir.join("prepped"));
    let stem = &r["stems"][0];
    let file = PathBuf::from(stem["stem_file"].as_str().unwrap_or_else(|| panic!("{r}")));
    assert_eq!(stem["doubts"], json!(["the stem file plays 6.0 dB quieter than the prepped copy so no stem clips"]), "{r}");
    let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    // The mix (0.1 throughout) and every stem are halved alike.
    let mix = decoded(&file, 0);
    assert!(mix.iter().all(|x| (x - 0.05).abs() < 1e-4));
    let drums = decoded(&file, 1);
    assert!((peak(&drums) - 0.8).abs() < 2e-3, "{}", peak(&drums));
    let worst = (44_100..29 * 44_100)
        .map(|i| (f64::from(drums[i]) - 0.8 * (std::f64::consts::TAU * HZ[1] * i as f64 / 44_100.0).sin()).abs())
        .fold(0.0, f64::max);
    assert!(worst < 2e-3, "drums off by {worst}");
    assert!((peak(&decoded(&file, 2)) - 0.05).abs() < 2e-3);
}

/// The stems of a track prepped at a gain add up to the stem file's mix sample for sample: an
/// MP3 of noise with a click, prepped louder (gain steps not 0), split into four parts of it.
/// The session runs at the file's rate, so nothing is resampled and any offset or gain error
/// shows.
#[test]
fn the_stems_add_up_to_the_prepped_mix() {
    if !tool("ffmpeg") {
        return;
    }
    let dir = scratch("sum");
    let lib = dir.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    // 8 s of quiet noise with a click at 1 s, as MP3s.
    let mut seed = 12_345u32;
    let mut noise: Vec<f32> = (0..8 * 44_100)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(seed >> 8) / f64::from(1u32 << 24) * 0.1 - 0.05) as f32
        })
        .collect();
    noise[44_100] = 0.3;
    let wav = lib.join("noise.wav");
    std::fs::write(&wav, encode(&AudioBuffer { sample_rate: 44_100, channels: vec![noise.clone(), noise] }, &EncodeOptions::default()).unwrap())
        .unwrap();
    let mut e = Engine::default();
    e.session_mut().name = "test set".into();
    e.session_mut().sample_rate = soundcraft_time::SampleRate::HZ_44100;
    e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
    let mut clips = Vec::new();
    for (name, bar) in [("a.mp3", 1), ("b.mp3", 2)] {
        let path = lib.join(name);
        let o = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-i"])
            .arg(&wav)
            .args(["-c:a", "libmp3lame", "-b:a", "256k"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let beats: Vec<f64> = (0..16).map(|k| k as f64 * 0.5).collect();
        let r = e.execute("dj.load_warped", &json!({"path": path, "beats": beats, "at_bar": bar})).unwrap();
        clips.push(ClipId(r["clip"].as_u64().unwrap()));
    }
    e.execute("dj.transition", &json!({"out_clip": clips[0].0, "in_clip": clips[1].0, "entry_bar": 3, "build_bars": 1, "out_bars": 1})).unwrap();
    // A's stems: a tenth, a fifth, three tenths and two fifths of its audio.
    let s = e.session_mut();
    let src = s.find_clip(clips[0]).unwrap().1.source().unwrap();
    let audio = s.pool.get(src).unwrap().buffer.clone();
    let sources = [0.1f32, 0.2, 0.3, 0.4].map(|w| {
        let part =
            AudioBuffer { sample_rate: audio.sample_rate, channels: audio.channels.iter().map(|c| c.iter().map(|x| x * w).collect()).collect() };
        soundcraft_engine::io::add_source(s, "stem", part, None, FileFormat::Wav)
    });
    s.find_clip_mut(clips[0]).unwrap().stems = Some(StemSet { key: "fake".into(), sources, scale_log2: 0 });

    let r = export(&e, &dir.join("prepped"));
    let steps = r["prep"]["tracks"][0]["steps"].as_i64().unwrap();
    assert_ne!(steps, 0, "{r}");
    let file = PathBuf::from(r["stems"][0]["stem_file"].as_str().unwrap_or_else(|| panic!("{r}")));
    let mix = decoded(&file, 0);
    let peak = mix.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(peak > 0.1, "{peak}");
    let parts: Vec<Vec<f32>> = (1..5).map(|n| decoded(&file, n)).collect();
    assert!(parts.iter().all(|p| p.len() == mix.len()));
    let worst = (0..mix.len()).map(|i| (parts.iter().map(|p| p[i]).sum::<f32>() - mix[i]).abs()).fold(0.0f32, f32::max);
    assert!(worst < 2e-3 * peak + 1e-4, "the stems are off the mix by {worst} (peak {peak}, steps {steps})");
}
