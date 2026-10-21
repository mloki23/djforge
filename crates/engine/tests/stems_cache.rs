//! The stem cache splits a source once, through a fake Demucs that writes known stems at 44.1 kHz,
//! stores them at the source's rate and length, and reuses or rebuilds entries.

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode};
use soundcraft_engine::cmd::stems::{self, CANCELLED, StemCache, StemFiles, StemRunner};
use soundcraft_engine::model::STEMS;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The rate Demucs writes whatever it is given.
const DEMUCS_RATE: u32 = 44_100;
/// Each stem of the synthetic source: (frequency in Hz, amplitude), in STEMS order.
const PARTS: [(f64, f64); 4] = [(440.0, 0.2), (3_000.0, 0.15), (60.0, 0.25), (1_000.0, 0.2)];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-stems-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Stem `i` of the source at time `t`, channel `ch` (the right channel is phase-shifted).
fn part(i: usize, ch: usize, t: f64) -> f32 {
    let (f, a) = PARTS[i];
    (a * (std::f64::consts::TAU * f * t + ch as f64 * 0.7).sin()) as f32
}

fn wav(buf: &AudioBuffer) -> Vec<u8> {
    encode(buf, &EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None }).unwrap()
}

/// A source WAV of the four parts summed, `secs` long at `rate` with `channels` channels.
fn source(dir: &Path, rate: u32, channels: usize, secs: f64) -> PathBuf {
    let frames = (secs * f64::from(rate)) as usize;
    let channels = (0..channels).map(|ch| (0..frames).map(|k| (0..4).map(|i| part(i, ch, k as f64 / f64::from(rate))).sum()).collect()).collect();
    let path = dir.join("track.wav");
    std::fs::write(&path, wav(&AudioBuffer { sample_rate: rate, channels })).unwrap();
    path
}

/// A fake Demucs: writes each part as a stereo WAV at 44.1 kHz, as long as the input plus
/// `extra` frames, where Demucs would, and counts its runs.
fn fake(calls: &Arc<AtomicUsize>, extra: i64) -> StemRunner {
    let calls = calls.clone();
    Arc::new(move |input: &Path, out: &Path, _stop: &dyn Fn() -> bool| {
        calls.fetch_add(1, Ordering::SeqCst);
        let (_, src) = decode(&std::fs::read(input).map_err(|e| e.to_string())?, Some("wav")).map_err(|e| e.to_string())?;
        let len = (src.frames() as f64 * f64::from(DEMUCS_RATE) / f64::from(src.sample_rate)).round() as i64 + extra;
        let dir = out.join("htdemucs").join(input.file_stem().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths: [PathBuf; 4] = Default::default();
        for (i, (name, path)) in STEMS.iter().zip(paths.iter_mut()).enumerate() {
            let channels = (0..2).map(|ch| (0..len).map(|k| part(i, ch, k as f64 / f64::from(DEMUCS_RATE))).collect()).collect();
            *path = dir.join(format!("{name}.wav"));
            std::fs::write(&*path, wav(&AudioBuffer { sample_rate: DEMUCS_RATE, channels })).unwrap();
        }
        Ok(paths)
    })
}

fn cache(dir: &Path) -> StemCache {
    StemCache::new(dir.join("cache")).with_free_space(stems::MIN_FREE, Arc::new(|_: &Path| Some(u64::MAX)))
}

fn never() -> bool {
    false
}

fn decoded(path: &Path) -> AudioBuffer {
    decode(&std::fs::read(path).unwrap(), None).unwrap().1
}

/// The names directly in `dir`.
fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

fn no_temp_folder(c: &StemCache) {
    if c.root().exists() {
        let left: Vec<String> = names(c.root()).into_iter().filter(|n| n.starts_with(".tmp-")).collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

#[test]
fn a_miss_stores_four_flacs_at_the_source_rate_that_sum_to_the_source() {
    let dir = scratch("miss");
    let src = source(&dir, 48_000, 2, 2.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let got: StemFiles = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    assert!(!got.hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let entry = c.root().join(&got.key);
    assert_eq!(names(&entry), ["bass.flac", "drums.flac", "other.flac", "stems.json", "vocals.flac"]);
    let index: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(entry.join("stems.json")).unwrap()).unwrap();
    let original = decoded(&src);
    let frames = original.frames();
    assert_eq!(index["sample_rate"], 48_000);
    assert_eq!(index["frames"], frames);
    assert_eq!(index["model"], "htdemucs");
    assert_eq!(index["key"], got.key.as_str());
    assert_eq!(index["size"], std::fs::metadata(&src).unwrap().len());
    assert_eq!(index["source_path"], src.display().to_string());
    // Each stem is at the source's rate and length, and they add back up to the source.
    let mut sum = vec![vec![0.0f32; frames]; 2];
    for (name, path) in STEMS.iter().zip(&got.paths) {
        assert_eq!(path, &entry.join(format!("{name}.flac")));
        let stem = decoded(path);
        assert_eq!((stem.sample_rate, stem.frames(), stem.num_channels()), (48_000, frames, 2), "{name}");
        for (s, c) in sum.iter_mut().zip(&stem.channels) {
            for (s, x) in s.iter_mut().zip(c) {
                *s += x;
            }
        }
    }
    // The resampler's error: over the whole file (its edges see silence beyond the ends), and
    // inside it.
    let edge = 256;
    let (mut worst, mut inner, mut sq) = (0.0f32, 0.0f32, 0.0f64);
    for (s, o) in sum.iter().zip(&original.channels) {
        for (k, (a, b)) in s.iter().zip(o).enumerate() {
            let d = (a - b).abs();
            worst = worst.max(d);
            sq += f64::from(d * d);
            if (edge..frames - edge).contains(&k) {
                inner = inner.max(d);
            }
        }
    }
    let rms = (sq / (2 * frames) as f64).sqrt();
    eprintln!("stem sum vs source: max {worst:.2e} (inside {inner:.2e}), rms {rms:.2e}");
    assert!(inner < 1e-4, "inside max {inner}");
    assert!(rms < 1e-3, "rms {rms}");
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mono_source_gets_mono_stems() {
    let dir = scratch("mono");
    let src = source(&dir, 44_100, 1, 0.5);
    let c = cache(&dir);
    let got = stems::split(&c, &src, &fake(&Arc::new(AtomicUsize::new(0)), 0), &never).unwrap();
    for path in &got.paths {
        let stem = decoded(path);
        assert_eq!((stem.sample_rate, stem.frames(), stem.num_channels()), (44_100, 22_050, 1));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_split_is_a_hit_and_runs_nothing() {
    let dir = scratch("hit");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let first = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    let modified = std::fs::metadata(&first.paths[0]).unwrap().modified().unwrap();
    let second = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    assert!(second.hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!((&second.key, &second.paths), (&first.key, &first.paths));
    assert_eq!(std::fs::metadata(&first.paths[0]).unwrap().modified().unwrap(), modified);
    // A copy of the same file elsewhere is the same entry.
    let copy = dir.join("copy.wav");
    std::fs::copy(&src, &copy).unwrap();
    assert!(stems::split(&c, &copy, &fake(&calls, 0), &never).unwrap().hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_damaged_entry_is_rebuilt() {
    let dir = scratch("damaged");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let first = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    let entry = c.root().join(&first.key);
    let good = std::fs::read(entry.join("stems.json")).unwrap();
    // A corrupt stems.json, a truncated stem, a missing stem: each is split again.
    let damage: [&dyn Fn(); 3] = [
        &|| std::fs::write(entry.join("stems.json"), "{not json").unwrap(),
        &|| {
            let bytes = std::fs::read(&first.paths[2]).unwrap();
            std::fs::write(&first.paths[2], &bytes[..bytes.len() / 2]).unwrap();
        },
        &|| std::fs::remove_file(&first.paths[3]).unwrap(),
    ];
    for (n, damage) in damage.iter().enumerate() {
        damage();
        let again = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
        assert!(!again.hit, "{n}");
        assert_eq!(calls.load(Ordering::SeqCst), n + 2);
        assert_eq!(again.paths, first.paths);
        assert_eq!(std::fs::read(entry.join("stems.json")).unwrap().len(), good.len(), "{n}");
        assert!(stems::split(&c, &src, &fake(&calls, 0), &never).unwrap().hit, "{n}");
        no_temp_folder(&c);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stem_of_another_length_fails_the_split() {
    let dir = scratch("length");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    // 200 frames over at 44.1 kHz is 218 at 48 kHz: too many.
    let err = stems::split(&c, &src, &fake(&calls, 200), &never).unwrap_err();
    assert_eq!(err, "stem vocals is 24218 frames, the track is 24000");
    assert!(names(c.root()).is_empty(), "{:?}", names(c.root()));
    // A few frames off is padded or trimmed to the exact length.
    for extra in [-30, 30] {
        let got = stems::split(&c, &src, &fake(&calls, extra), &never).unwrap();
        assert!(got.paths.iter().all(|p| decoded(p).frames() == 24_000), "{extra}");
        stems::clear(&c).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_runner_error_leaves_nothing_behind() {
    let dir = scratch("runner-error");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = cache(&dir);
    let failing: StemRunner = Arc::new(|_: &Path, out: &Path, _: &dyn Fn() -> bool| {
        std::fs::create_dir_all(out.join("htdemucs")).unwrap();
        Err("Demucs failed (exit code: 1): CUDA out of memory".to_string())
    });
    let err = stems::split(&c, &src, &failing, &never).unwrap_err();
    assert_eq!(err, "Demucs failed (exit code: 1): CUDA out of memory");
    assert!(names(c.root()).is_empty(), "{:?}", names(c.root()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stopping_cancels_and_leaves_nothing_behind() {
    let dir = scratch("stop");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    // Stopped while Demucs runs: the runner finishes, then the split gives up.
    let stopped = Arc::new(AtomicBool::new(false));
    let inner = fake(&calls, 0);
    let flag = stopped.clone();
    let stopping: StemRunner = Arc::new(move |i: &Path, o: &Path, s: &dyn Fn() -> bool| {
        flag.store(true, Ordering::SeqCst);
        inner(i, o, s)
    });
    let err = stems::split(&c, &src, &stopping, &|| stopped.load(Ordering::SeqCst)).unwrap_err();
    assert_eq!(err, CANCELLED);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(names(c.root()).is_empty(), "{:?}", names(c.root()));
    // Stopped before it starts: nothing runs.
    assert_eq!(stems::split(&c, &src, &fake(&calls, 0), &|| true).unwrap_err(), CANCELLED);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_full_drive_is_refused_before_anything_runs() {
    let dir = scratch("full");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = StemCache::new(dir.join("cache")).with_free_space(2 << 30, Arc::new(|_: &Path| Some(1 << 30)));
    let calls = Arc::new(AtomicUsize::new(0));
    let err = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap_err();
    assert!(err.starts_with("need 2 GB free on "), "{err}");
    assert!(err.ends_with(" for stems; 1.0 GB free"), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    no_temp_folder(&c);
    // Free space that can't be learned doesn't block the split.
    let unknown = StemCache::new(dir.join("cache")).with_free_space(2 << 30, Arc::new(|_: &Path| None));
    assert!(stems::split(&unknown, &src, &fake(&calls, 0), &never).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_removes_only_cache_entries() {
    let dir = scratch("clear");
    let src = source(&dir, 48_000, 2, 0.5);
    let c = cache(&dir);
    let got = stems::split(&c, &src, &fake(&Arc::new(AtomicUsize::new(0)), 0), &never).unwrap();
    let root = c.root();
    let entry_bytes: u64 = std::fs::read_dir(root.join(&got.key)).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    std::fs::create_dir_all(root.join(".tmp-0123456789abcdef-1-htdemucs-9-0/out")).unwrap();
    std::fs::write(root.join(".tmp-0123456789abcdef-1-htdemucs-9-0/out/x.wav"), [0u8; 10]).unwrap();
    std::fs::write(root.join("notes.txt"), "mine").unwrap();
    std::fs::create_dir_all(root.join("mine")).unwrap();
    std::fs::write(root.join("mine/a.flac"), "keep").unwrap();
    std::fs::create_dir_all(root.join("0123456789abcdef-1-other")).unwrap();
    assert_eq!(stems::clear(&c).unwrap(), (6, entry_bytes + 10));
    assert_eq!(names(root), ["0123456789abcdef-1-other", "mine", "notes.txt"]);
    assert_eq!(std::fs::read_to_string(root.join("mine/a.flac")).unwrap(), "keep");
    assert_eq!(stems::clear(&c).unwrap(), (0, 0));
    // A cache never used is empty.
    assert_eq!(stems::clear(&StemCache::new(dir.join("nowhere"))).unwrap(), (0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs the real Demucs (in WSL on Windows) over a 3 s synthetic track.
#[test]
#[ignore = "runs Demucs; needs ~/mixforge-wsl/venv (or DJFORGE_DEMUCS_PYTHON) with demucs installed"]
fn the_real_runner_splits_a_short_track() {
    let dir = scratch("real");
    let src = source(&dir, 48_000, 2, 3.0);
    let c = StemCache::new(dir.join("cache")).with_free_space(0, Arc::new(|_: &Path| None));
    let started = std::time::Instant::now();
    let got = stems::split(&c, &src, &stems::demucs_runner(), &never).unwrap();
    eprintln!("real Demucs split took {:.1} s", started.elapsed().as_secs_f64());
    assert!(!got.hit);
    let original = decoded(&src);
    let mut sum = vec![vec![0.0f32; original.frames()]; 2];
    for path in &got.paths {
        let stem = decoded(path);
        assert_eq!((stem.sample_rate, stem.frames(), stem.num_channels()), (48_000, 144_000, 2));
        for (s, c) in sum.iter_mut().zip(&stem.channels) {
            for (s, x) in s.iter_mut().zip(c) {
                *s += x;
            }
        }
    }
    let sq: f64 = sum.iter().zip(&original.channels).flat_map(|(s, o)| s.iter().zip(o).map(|(a, b)| f64::from((a - b) * (a - b)))).sum();
    let power: f64 = original.channels.iter().flatten().map(|x| f64::from(x * x)).sum();
    eprintln!("stem sum residual: {:.1} dB below the source", 10.0 * (power / sq.max(1e-30)).log10());
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}
