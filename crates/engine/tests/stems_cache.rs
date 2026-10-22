//! The stem cache splits a source once, through a fake Demucs that writes known stems at 44.1 kHz,
//! stores them at the source's rate and length (with headroom), and reuses or rebuilds entries.

use soundcraft_audio_io::{AudioBuffer, decode, wav_float_header};
use soundcraft_engine::cmd::stems::{self, CANCELLED, MARKER, StemCache, StemFiles, StemRunner};
use soundcraft_engine::file_access::{AutomationRoots, confine};
use soundcraft_engine::model::STEMS;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The rate Demucs writes whatever it is given.
const DEMUCS_RATE: u32 = 44_100;
/// Each stem of the synthetic source at level 1: (frequency in Hz, amplitude), in STEMS order.
const PARTS: [(f64, f64); 4] = [(440.0, 0.2), (3_000.0, 0.15), (60.0, 0.25), (1_000.0, 0.2)];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-stems-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Stem `i` of the source at level `level` at time `t`, channel `ch` (the right channel is
/// phase-shifted).
fn part(i: usize, level: f64, ch: usize, t: f64) -> f32 {
    let (f, a) = PARTS[i];
    (level * a * (std::f64::consts::TAU * f * t + ch as f64 * 0.7).sin()) as f32
}

/// A 32-bit float WAV of `buf`, unclamped (a loud master can hold samples past full scale).
fn float_wav(buf: &AudioBuffer) -> Vec<u8> {
    let mut bytes = wav_float_header(buf.num_channels(), buf.sample_rate, buf.frames() as u64).unwrap();
    bytes.extend(buf.interleaved().iter().flat_map(|x| x.to_le_bytes()));
    bytes
}

/// A source WAV of the four parts summed at `level`, `secs` long at `rate` with `channels`.
fn source(dir: &Path, rate: u32, channels: usize, secs: f64, level: f64) -> PathBuf {
    let frames = (secs * f64::from(rate)) as usize;
    let channels =
        (0..channels).map(|ch| (0..frames).map(|k| (0..4).map(|i| part(i, level, ch, k as f64 / f64::from(rate))).sum()).collect()).collect();
    let path = dir.join("track.wav");
    std::fs::write(&path, float_wav(&AudioBuffer { sample_rate: rate, channels })).unwrap();
    path
}

fn peak(buf: &AudioBuffer) -> f32 {
    buf.channels.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()))
}

/// Writes the four parts at `level` (times `gain`) as stereo WAVs at 44.1 kHz, `extra` frames
/// longer than the input, where Demucs would put them.
fn write_parts(input: &Path, out: &Path, level: f64, gain: f64, extra: i64) -> [PathBuf; 4] {
    let (_, src) = decode(&std::fs::read(input).unwrap(), Some("wav")).unwrap();
    let len = (src.frames() as f64 * f64::from(DEMUCS_RATE) / f64::from(src.sample_rate)).round() as i64 + extra;
    let dir = out.join("htdemucs").join(input.file_stem().unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    let mut paths: [PathBuf; 4] = Default::default();
    for (i, (name, path)) in STEMS.iter().zip(paths.iter_mut()).enumerate() {
        let channels =
            (0..2).map(|ch| (0..len).map(|k| (gain * f64::from(part(i, level, ch, k as f64 / f64::from(DEMUCS_RATE)))) as f32).collect()).collect();
        *path = dir.join(format!("{name}.wav"));
        std::fs::write(&*path, float_wav(&AudioBuffer { sample_rate: DEMUCS_RATE, channels })).unwrap();
    }
    paths
}

/// The power of two the split scaled a source of `level` down by, read off its input WAV.
fn input_gain(input: &Path, level: f64) -> f64 {
    let (_, src) = decode(&std::fs::read(input).unwrap(), Some("wav")).unwrap();
    let rate = f64::from(src.sample_rate);
    let full = (0..src.frames()).map(|k| (0..4).map(|i| part(i, level, 0, k as f64 / rate)).sum::<f32>().abs()).fold(0.0f32, f32::max);
    let ch0 = src.channels[0].iter().fold(0.0f32, |m, x| m.max(x.abs()));
    2f64.powf((f64::from(ch0) / f64::from(full)).log2().round())
}

/// A fake Demucs for a source at `level`: the parts at the level of its input, `extra` frames
/// long. Counts its runs.
fn fake_at(calls: &Arc<AtomicUsize>, level: f64, extra: i64) -> StemRunner {
    let calls = calls.clone();
    Arc::new(move |input: &Path, out: &Path, _stop: &dyn Fn() -> bool| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(write_parts(input, out, level, input_gain(input, level), extra))
    })
}

fn fake(calls: &Arc<AtomicUsize>, extra: i64) -> StemRunner {
    fake_at(calls, 1.0, extra)
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

/// The names directly in `dir`, but the cache's marker.
fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n != MARKER).collect();
    v.sort();
    v
}

fn no_temp_folder(c: &StemCache) {
    if c.root().exists() {
        let left: Vec<String> = names(c.root()).into_iter().filter(|n| n.starts_with(".tmp-")).collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

/// The stems' sum, brought back up by their scale, against the source: the largest difference
/// inside the file (the resampler's edges see silence past the ends), and the RMS over all of it.
fn sum_error(got: &StemFiles, src: &Path) -> (f32, f64) {
    let original = decoded(src);
    let frames = original.frames();
    let scale = 2f32.powi(i32::from(got.scale_log2));
    let mut sum = vec![vec![0.0f32; frames]; original.num_channels()];
    for path in &got.paths {
        let stem = decoded(path);
        assert_eq!((stem.frames(), stem.num_channels()), (frames, original.num_channels()));
        for (s, c) in sum.iter_mut().zip(&stem.channels) {
            for (s, x) in s.iter_mut().zip(c) {
                *s += x * scale;
            }
        }
    }
    let edge = 256;
    let (mut inner, mut sq) = (0.0f32, 0.0f64);
    for (s, o) in sum.iter().zip(&original.channels) {
        for (k, (a, b)) in s.iter().zip(o).enumerate() {
            let d = (a - b).abs();
            sq += f64::from(d * d);
            if (edge..frames - edge).contains(&k) {
                inner = inner.max(d);
            }
        }
    }
    (inner, (sq / (sum.len() * frames) as f64).sqrt())
}

fn index(c: &StemCache, got: &StemFiles) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(c.root().join(&got.key).join("stems.json")).unwrap()).unwrap()
}

/// A process id that is never running: not a multiple of 4, so never a Windows process id (and
/// unlikely elsewhere).
const GONE: u32 = 2_000_000_003;
/// A process that always runs: System on Windows, init elsewhere.
const LIVE: u32 = if cfg!(windows) { 4 } else { 1 };

/// Sets the folder's modification time two hours back.
fn make_old(dir: &Path) {
    let ok = if cfg!(windows) {
        let script = format!("(Get-Item -LiteralPath '{}').LastWriteTime = (Get-Date).AddHours(-2)", dir.display());
        std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-Command", &script]).status().unwrap().success()
    } else {
        std::process::Command::new("touch").args(["-d", "2 hours ago"]).arg(dir).status().unwrap().success()
    };
    assert!(ok);
    let age = std::fs::metadata(dir).unwrap().modified().unwrap().elapsed().unwrap();
    assert!(age > std::time::Duration::from_secs(3_600), "{age:?}");
}

#[test]
fn a_miss_stores_four_flacs_at_the_source_rate_that_sum_to_the_source() {
    let dir = scratch("miss");
    let src = source(&dir, 48_000, 2, 2.0, 1.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let got: StemFiles = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    assert!(!got.hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let entry = c.root().join(&got.key);
    assert_eq!(names(&entry), ["bass.flac", "drums.flac", "other.flac", "stems.json", "vocals.flac"]);
    assert!(c.root().join(MARKER).is_file());
    let index = index(&c, &got);
    let frames = decoded(&src).frames();
    assert_eq!(index["version"], 2);
    assert_eq!(index["sample_rate"], 48_000);
    assert_eq!(index["frames"], frames);
    assert_eq!(index["model"], "htdemucs");
    assert_eq!(index["key"], got.key.as_str());
    assert_eq!(index["size"], std::fs::metadata(&src).unwrap().len());
    assert_eq!(index["source_path"], src.display().to_string());
    // The source peaks under 0.8: one bit of headroom.
    assert_eq!((index["scale_log2"].as_u64(), got.scale_log2), (Some(1), 1));
    for (name, path) in STEMS.iter().zip(&got.paths) {
        assert_eq!(path, &entry.join(format!("{name}.flac")));
        let stem = decoded(path);
        assert_eq!((stem.sample_rate, stem.frames(), stem.num_channels()), (48_000, frames, 2), "{name}");
    }
    let (inner, rms) = sum_error(&got, &src);
    eprintln!("stem sum vs source: inside max {inner:.2e}, rms {rms:.2e}");
    assert!(inner < 1e-4, "inside max {inner}");
    assert!(rms < 1e-3, "rms {rms}");
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A loud master (peaks over +12 dBFS, the bass stem alone at 1.5) is split at a lower level and
/// stored with its scale, so the stems brought back up still sum to it.
#[test]
fn loud_stems_keep_their_level_through_headroom() {
    let dir = scratch("loud");
    let level = 6.0;
    let src = source(&dir, 48_000, 2, 1.0, level);
    let source_peak = peak(&decoded(&src));
    assert!(source_peak > 4.0, "{source_peak}");
    let c = cache(&dir);
    let got = stems::split(&c, &src, &fake_at(&Arc::new(AtomicUsize::new(0)), level, 0), &never).unwrap();
    // ceil(log2(4.x)) = 3, plus one.
    assert_eq!(got.scale_log2, 4);
    assert_eq!(index(&c, &got)["scale_log2"], 4);
    let bass = decoded(&got.paths[2]);
    assert!((peak(&bass) * 16.0 - 1.5).abs() < 0.01, "{}", peak(&bass));
    let (inner, rms) = sum_error(&got, &src);
    eprintln!("loud stem sum vs source: inside max {inner:.2e}, rms {rms:.2e}");
    assert!(inner < 1e-3, "inside max {inner}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stems that clip at the first level are split once more with another bit of headroom; stems
/// that clip again fail the split.
#[test]
fn clipping_stems_get_one_more_bit_of_headroom() {
    let dir = scratch("clipping");
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    // The first run's stems come back too loud (as Demucs' can be); the second's are right.
    let n = calls.clone();
    let once: StemRunner = Arc::new(move |input: &Path, out: &Path, _: &dyn Fn() -> bool| {
        let first = n.fetch_add(1, Ordering::SeqCst) == 0;
        let gain = input_gain(input, 1.0);
        Ok(write_parts(input, out, 1.0, if first { gain * 10.0 } else { gain }, 0))
    });
    let got = stems::split(&c, &src, &once, &never).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(got.scale_log2, 2);
    assert!(sum_error(&got, &src).0 < 1e-4);
    stems::clear(&c).unwrap();
    // Always too loud: the split fails, after two runs, and leaves nothing.
    let n = Arc::new(AtomicUsize::new(0));
    let runs = n.clone();
    let loud: StemRunner = Arc::new(move |input: &Path, out: &Path, _: &dyn Fn() -> bool| {
        runs.fetch_add(1, Ordering::SeqCst);
        Ok(write_parts(input, out, 5.0, 1.0, 0))
    });
    let err = stems::split(&c, &src, &loud, &never).unwrap_err();
    assert!(err.ends_with("the stems clip even with headroom (2 bits)"), "{err}");
    assert_eq!(n.load(Ordering::SeqCst), 2);
    assert!(names(c.root()).is_empty(), "{:?}", names(c.root()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_mono_source_gets_mono_stems() {
    let dir = scratch("mono");
    let src = source(&dir, 44_100, 1, 0.5, 1.0);
    let c = cache(&dir);
    let got = stems::split(&c, &src, &fake(&Arc::new(AtomicUsize::new(0)), 0), &never).unwrap();
    for path in &got.paths {
        let stem = decoded(path);
        assert_eq!((stem.sample_rate, stem.frames(), stem.num_channels()), (44_100, 22_050, 1));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn more_than_two_channels_are_refused_before_demucs_runs() {
    let dir = scratch("surround");
    let src = source(&dir, 48_000, 3, 0.2, 1.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let err = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap_err();
    assert!(err.ends_with("has 3 channels; stems can be split from mono or stereo files only"), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_split_is_a_hit_and_runs_nothing() {
    let dir = scratch("hit");
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let first = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    let modified = std::fs::metadata(&first.paths[0]).unwrap().modified().unwrap();
    let second = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    assert!(second.hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!((&second.key, &second.paths, second.scale_log2), (&first.key, &first.paths, first.scale_log2));
    assert_eq!(std::fs::metadata(&first.paths[0]).unwrap().modified().unwrap(), modified);
    // A copy of the same file elsewhere is the same entry.
    let copy = dir.join("copy.wav");
    std::fs::copy(&src, &copy).unwrap();
    assert!(stems::split(&c, &copy, &fake(&calls, 0), &never).unwrap().hit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_damaged_or_older_entry_is_rebuilt() {
    let dir = scratch("damaged");
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let first = stems::split(&c, &src, &fake(&calls, 0), &never).unwrap();
    let entry = c.root().join(&first.key);
    let good = std::fs::read_to_string(entry.join("stems.json")).unwrap();
    assert!(good.contains("\"version\": 2"), "{good}");
    // A corrupt stems.json, one from before headroom, a truncated stem, a missing stem: each is
    // split again.
    let damage: [&dyn Fn(); 4] = [
        &|| std::fs::write(entry.join("stems.json"), "{not json").unwrap(),
        &|| std::fs::write(entry.join("stems.json"), good.replace("\"version\": 2", "\"version\": 1")).unwrap(),
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
        assert_eq!(std::fs::read_to_string(entry.join("stems.json")).unwrap().len(), good.len(), "{n}");
        assert!(stems::split(&c, &src, &fake(&calls, 0), &never).unwrap().hit, "{n}");
        no_temp_folder(&c);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stem_of_another_length_fails_the_split() {
    let dir = scratch("length");
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
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
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
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
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
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
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
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

/// Two splits of one file at once: the one that finishes second keeps the first one's entry.
#[test]
fn a_split_racing_another_keeps_the_first_entry() {
    let dir = scratch("race");
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = fake(&calls, 0);
    let (c2, src2, calls2) = (c.clone(), src.clone(), calls.clone());
    let racing: StemRunner = Arc::new(move |i: &Path, o: &Path, s: &dyn Fn() -> bool| {
        // The other split runs to the end while this one is in Demucs.
        let theirs = stems::split(&c2, &src2, &fake(&calls2, 0), &never).unwrap();
        assert!(!theirs.hit);
        inner(i, o, s)
    });
    let got = stems::split(&c, &src, &racing, &never).unwrap();
    assert!(!got.hit);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(stems::split(&c, &src, &fake(&calls, 0), &never).unwrap().hit);
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_removes_only_cache_entries() {
    let dir = scratch("clear");
    let src = source(&dir, 48_000, 2, 0.5, 1.0);
    let c = cache(&dir);
    let got = stems::split(&c, &src, &fake(&Arc::new(AtomicUsize::new(0)), 0), &never).unwrap();
    let root = c.root();
    let entry_bytes: u64 = std::fs::read_dir(root.join(&got.key)).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
    std::fs::create_dir_all(root.join(".tmp-0123456789abcdef-1-htdemucs-2000000003-0/out")).unwrap();
    std::fs::write(root.join(".tmp-0123456789abcdef-1-htdemucs-2000000003-0/out/x.wav"), [0u8; 10]).unwrap();
    std::fs::write(root.join("notes.txt"), "mine").unwrap();
    std::fs::create_dir_all(root.join("mine")).unwrap();
    std::fs::write(root.join("mine/a.flac"), "keep").unwrap();
    let live = format!(".tmp-0123456789abcdef-1-htdemucs-{LIVE}-0");
    for kept in ["0123456789abcdef-1-other", ".tmp-mine", ".tmp-0123456789abcdef-1-htdemucs-9", &live] {
        std::fs::create_dir_all(root.join(kept)).unwrap();
    }
    // The size counts the finished entries: what Clear Stem Cache frees of the stems.
    assert_eq!(stems::stem_cache_size(&c).unwrap(), (5, entry_bytes));
    assert_eq!(stems::clear(&c).unwrap(), (6, entry_bytes + 10));
    assert_eq!(stems::stem_cache_size(&c).unwrap(), (0, 0));
    let mut want = vec![".tmp-0123456789abcdef-1-htdemucs-9", &live, ".tmp-mine", "0123456789abcdef-1-other", "mine", "notes.txt"];
    want.sort_unstable();
    assert_eq!(names(root), want);
    assert_eq!(std::fs::read_to_string(root.join("mine/a.flac")).unwrap(), "keep");
    assert!(root.join(MARKER).is_file());
    assert_eq!(stems::clear(&c).unwrap(), (0, 0));
    // A cache never used is empty.
    assert_eq!(stems::clear(&StemCache::new(dir.join("nowhere"))).unwrap(), (0, 0));
    assert_eq!(stems::stem_cache_size(&StemCache::new(dir.join("nowhere"))).unwrap(), (0, 0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A folder that isn't a stem cache (no marker) is never cleared, even holding key-named folders.
#[test]
fn clear_refuses_a_folder_without_the_marker() {
    let dir = scratch("no-marker");
    let key = dir.join("0123456789abcdef-1-htdemucs");
    std::fs::create_dir_all(&key).unwrap();
    std::fs::write(key.join("song.flac"), "mine").unwrap();
    let err = stems::clear(&StemCache::new(&dir)).unwrap_err();
    assert!(err.contains("not a stem cache"), "{err}");
    assert!(key.join("song.flac").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A key-named link in the cache to a folder elsewhere is removed; what it points at stays.
#[test]
fn clear_removes_a_link_but_not_what_it_points_at() {
    let dir = scratch("link");
    let src = source(&dir, 48_000, 2, 0.2, 1.0);
    let c = cache(&dir);
    stems::split(&c, &src, &fake(&Arc::new(AtomicUsize::new(0)), 0), &never).unwrap();
    let target = dir.join("music");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("song.flac"), "precious").unwrap();
    let link = c.root().join("fedcba9876543210-1-htdemucs");
    #[cfg(windows)]
    {
        // A junction needs no privilege.
        let ok = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&target).output().unwrap().status.success();
        assert!(ok, "mklink /J failed");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(link.join("song.flac").is_file());
    stems::clear(&c).unwrap();
    assert!(std::fs::symlink_metadata(&link).is_err(), "the link goes");
    assert_eq!(std::fs::read_to_string(target.join("song.flac")).unwrap(), "precious");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The runner's stems must be inside the split's own folder: a path elsewhere is never read.
#[test]
fn stems_outside_the_split_folder_are_refused() {
    let dir = scratch("outside");
    let src = source(&dir, 48_000, 2, 0.2, 1.0);
    let c = cache(&dir);
    let elsewhere = dir.join("elsewhere");
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = fake(&calls, 0);
    let (e2, e3) = (elsewhere.clone(), elsewhere.clone());
    let straying: StemRunner = Arc::new(move |i: &Path, o: &Path, s: &dyn Fn() -> bool| {
        let paths = inner(i, o, s)?;
        std::fs::create_dir_all(&e2).unwrap();
        Ok(paths.map(|p| {
            let to = e2.join(p.file_name().unwrap());
            std::fs::copy(&p, &to).unwrap();
            to
        }))
    });
    let err = stems::split(&c, &src, &straying, &never).unwrap_err();
    assert!(err.contains("not in the split's folder"), "{err}");
    // `..` back out of the folder is outside too.
    let dotdot: StemRunner = Arc::new(move |i: &Path, o: &Path, s: &dyn Fn() -> bool| {
        fake(&Arc::new(AtomicUsize::new(0)), 0)(i, o, s)?;
        let back = o.join("..").join("..").join("..").join("elsewhere");
        Ok(STEMS.map(|n| back.join(format!("{n}.wav"))))
    });
    assert!(e3.join("vocals.wav").is_file());
    let err = stems::split(&c, &src, &dotdot, &never).unwrap_err();
    assert!(err.contains("not in the split's folder"), "{err}");
    assert!(names(c.root()).is_empty(), "{:?}", names(c.root()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Clearing the cache while a split runs leaves that split's temporary folder alone, so the
/// split still finishes.
#[test]
fn clear_skips_a_split_under_way() {
    let dir = scratch("clear-busy");
    let src = source(&dir, 48_000, 2, 0.2, 1.0);
    let c = cache(&dir);
    let inner = fake(&Arc::new(AtomicUsize::new(0)), 0);
    // The same root spelled another way: a trailing separator, and upper case on Windows.
    let mut spelled = c.root().display().to_string();
    if cfg!(windows) {
        spelled = spelled.to_uppercase();
    }
    spelled.push(std::path::MAIN_SEPARATOR);
    let c2 = StemCache::new(spelled);
    let clearing: StemRunner = Arc::new(move |i: &Path, o: &Path, s: &dyn Fn() -> bool| {
        assert_eq!(stems::clear(&c2).unwrap(), (0, 0));
        assert!(i.is_file());
        inner(i, o, s)
    });
    let got = stems::split(&c, &src, &clearing, &never).unwrap();
    assert!(got.paths.iter().all(|p| p.is_file()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A split's temporary folder left by a process that has gone is swept at the next split; this
/// process's are not.
#[test]
fn leftovers_of_a_gone_process_are_swept() {
    let dir = scratch("sweep");
    let src = source(&dir, 48_000, 2, 0.2, 1.0);
    let c = cache(&dir);
    let gone = c.root().join(format!(".tmp-0123456789abcdef-1-htdemucs-{GONE}-0"));
    let ours = c.root().join(format!(".tmp-0123456789abcdef-1-htdemucs-{}-999", std::process::id()));
    // Another process's split under way, started long ago (a long track): never swept.
    let live = c.root().join(format!(".tmp-0123456789abcdef-1-htdemucs-{LIVE}-0"));
    for d in [&gone, &ours, &live] {
        std::fs::create_dir_all(d.join("out")).unwrap();
    }
    make_old(&live);
    stems::split(&c, &src, &fake(&Arc::new(AtomicUsize::new(0)), 0), &never).unwrap();
    assert!(!gone.exists());
    assert!(ours.is_dir());
    assert!(live.is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Under automation roots the source must be inside the read root, but the cache (the app's
/// own folder) works outside them.
#[test]
fn the_cache_works_outside_the_automation_roots() {
    let dir = scratch("roots");
    let music = dir.join("music");
    std::fs::create_dir_all(&music).unwrap();
    let src = source(&music, 48_000, 2, 0.2, 1.0);
    let c = cache(&dir);
    let roots = AutomationRoots::new(Some(&music), Some(&music)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let got = confine(roots.as_ref(), || stems::split(&c, &src, &fake(&calls, 0), &never)).unwrap();
    assert!(!got.hit && got.paths.iter().all(|p| p.is_file()));
    assert!(confine(roots.as_ref(), || stems::split(&c, &src, &fake(&calls, 0), &never)).unwrap().hit);
    let elsewhere = scratch("roots-elsewhere");
    let outside = source(&elsewhere, 48_000, 2, 0.2, 1.0);
    assert!(confine(roots.as_ref(), || stems::split(&c, &outside, &fake(&calls, 0), &never)).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&elsewhere);
}

/// The sum of the stems brought back up, against the source, as dB of residual below it.
fn residual_db(got: &StemFiles, src: &Path) -> f64 {
    let original = decoded(src);
    let scale = 2f32.powi(i32::from(got.scale_log2));
    let mut sum = vec![vec![0.0f32; original.frames()]; 2];
    for path in &got.paths {
        let stem = decoded(path);
        assert_eq!((stem.sample_rate, stem.frames(), stem.num_channels()), (original.sample_rate, original.frames(), 2));
        for (s, c) in sum.iter_mut().zip(&stem.channels) {
            for (s, x) in s.iter_mut().zip(c) {
                *s += x * scale;
            }
        }
    }
    let sq: f64 = sum.iter().zip(&original.channels).flat_map(|(s, o)| s.iter().zip(o).map(|(a, b)| f64::from((a - b) * (a - b)))).sum();
    let power: f64 = original.channels.iter().flatten().map(|x| f64::from(x * x)).sum();
    10.0 * (power / sq.max(1e-30)).log10()
}

/// A loud master: a square and a saw, peaking at about +2 dBFS.
fn loud_source(dir: &Path, secs: f64) -> PathBuf {
    let rate = 44_100u32;
    let frames = (secs * f64::from(rate)) as usize;
    let wave = |k: usize, ch: usize| {
        let t = k as f64 / f64::from(rate);
        let square = if (t * 110.0 + ch as f64 * 0.1).fract() < 0.5 { 0.6 } else { -0.6 };
        let saw = 2.0 * (t * 330.0).fract() - 1.0;
        (square + 0.66 * saw) as f32
    };
    let channels = (0..2).map(|ch| (0..frames).map(|k| wave(k, ch)).collect()).collect();
    let path = dir.join("loud.wav");
    std::fs::write(&path, float_wav(&AudioBuffer { sample_rate: rate, channels })).unwrap();
    path
}

/// Runs the real Demucs (in WSL on Windows) over 3 s of a loud master.
#[test]
#[ignore = "runs Demucs; needs ~/mixforge-wsl/venv (or DJFORGE_DEMUCS_PYTHON) with demucs installed"]
fn the_real_runner_splits_a_short_track() {
    let dir = scratch("real");
    let src = loud_source(&dir, 3.0);
    eprintln!("source peak {:+.2} dBFS", 20.0 * f64::from(peak(&decoded(&src))).log10());
    let c = StemCache::new(dir.join("cache")).with_free_space(0, Arc::new(|_: &Path| None));
    let started = std::time::Instant::now();
    let got = stems::split(&c, &src, &stems::demucs_runner(), &never).unwrap();
    eprintln!("real Demucs split took {:.1} s, scale_log2 {}", started.elapsed().as_secs_f64(), got.scale_log2);
    assert!(!got.hit);
    for (name, path) in STEMS.iter().zip(&got.paths) {
        let at_level = f64::from(peak(&decoded(path))) * 2f64.powi(i32::from(got.scale_log2));
        eprintln!("{name} peak {:+.2} dBFS at its level", 20.0 * at_level.log10());
    }
    eprintln!("stem sum residual: {:.1} dB below the source", residual_db(&got, &src));
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cancels the real Demucs part-way through a 60 s track; nothing of it is left running in WSL.
#[test]
#[ignore = "runs Demucs in WSL and checks for leftovers with pgrep"]
fn cancelling_the_real_runner_leaves_no_demucs_running() {
    let dir = scratch("real-cancel");
    let src = loud_source(&dir, 60.0);
    let c = StemCache::new(dir.join("cache")).with_free_space(0, Arc::new(|_: &Path| None));
    let pgrep = || {
        let out = std::process::Command::new("wsl").args(["-e", "pgrep", "-af", "demucs"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let started = std::time::Instant::now();
    let stop_at = std::time::Duration::from_secs(12);
    // What runs at the moment of the stop: Demucs must have started for the check to mean much.
    let running = std::sync::OnceLock::new();
    let stop = || started.elapsed() > stop_at && !running.get_or_init(pgrep).is_empty();
    let err = stems::split(&c, &src, &stems::demucs_runner(), &stop).unwrap_err();
    eprintln!("stopped after {:.1} s: {err}", started.elapsed().as_secs_f64());
    eprintln!("running when stopped: {:?}", running.get());
    assert_eq!(err, CANCELLED);
    assert!(running.get().is_some_and(|r| r.contains("demucs")), "{running:?}");
    let mut left = pgrep();
    for _ in 0..10 {
        if left.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        left = pgrep();
    }
    eprintln!("pgrep -af demucs: {left:?}");
    assert!(left.is_empty(), "{left}");
    no_temp_folder(&c);
    let _ = std::fs::remove_dir_all(&dir);
}
