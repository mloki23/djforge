//! `dj.split_stems` splits a clip on a worker thread, through a fake Demucs and a temporary stem
//! cache, and `dj.attach_stems` gives the clip and every other clip of its audio the stems in one
//! undo step. The stems play at their level, survive save and reopen (also where automation is
//! confined away from the cache), and a stem that won't load is split again.
//!
//! Each test splits a source of its own level, so no two share a cache key: a split's cache is
//! known to the whole process, and tests run at once.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, decode, wav_float_header};
use soundcraft_engine::Engine;
use soundcraft_engine::cmd::stems::{self, CANCELLED, DECODING, SEPARATING, StemCache, StemRunner};
use soundcraft_engine::file_access::{AutomationRoots, confine};
use soundcraft_engine::model::{ClipId, STEMS, SourceId, StemSet};
use soundcraft_time::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The rate Demucs writes whatever it is given.
const DEMUCS_RATE: u32 = 44_100;
/// Each stem of the synthetic source at level 1: (frequency in Hz, amplitude), in STEMS order.
const PARTS: [(f64, f64); 4] = [(440.0, 0.2), (3_000.0, 0.15), (60.0, 0.25), (1_000.0, 0.2)];
const RATE: u32 = 48_000;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("djforge-split-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("music")).unwrap();
    dir
}

fn part(i: usize, level: f64, ch: usize, t: f64) -> f32 {
    let (f, a) = PARTS[i];
    (level * a * (std::f64::consts::TAU * f * t + ch as f64 * 0.7).sin()) as f32
}

fn float_wav(buf: &AudioBuffer) -> Vec<u8> {
    let mut bytes = wav_float_header(buf.num_channels(), buf.sample_rate, buf.frames() as u64).unwrap();
    bytes.extend(buf.interleaved().iter().flat_map(|x| x.to_le_bytes()));
    bytes
}

/// A stereo source WAV `name` in `dir/music` of the four parts summed at `level`, 1 s at 48 kHz.
fn source(dir: &Path, name: &str, level: f64) -> PathBuf {
    source_at(dir, name, level, RATE)
}

/// [`source`] at `rate`.
fn source_at(dir: &Path, name: &str, level: f64, rate: u32) -> PathBuf {
    let frames = rate as usize;
    let channels = (0..2).map(|ch| (0..frames).map(|k| (0..4).map(|i| part(i, level, ch, k as f64 / f64::from(rate))).sum()).collect()).collect();
    let path = dir.join("music").join(name);
    std::fs::write(&path, float_wav(&AudioBuffer { sample_rate: rate, channels })).unwrap();
    path
}

/// The power of two the split scaled a source of `level` down by, read off its input WAV.
fn input_gain(input: &Path, level: f64) -> f64 {
    let (_, src) = decode(&std::fs::read(input).unwrap(), Some("wav")).unwrap();
    let rate = f64::from(src.sample_rate);
    let full = (0..src.frames()).map(|k| (0..4).map(|i| part(i, level, 0, k as f64 / rate)).sum::<f32>().abs()).fold(0.0f32, f32::max);
    let ch0 = src.channels[0].iter().fold(0.0f32, |m, x| m.max(x.abs()));
    2f64.powf((f64::from(ch0) / f64::from(full)).log2().round())
}

/// A fake Demucs for a source at `level`: the four parts at its input's level, at 44.1 kHz.
fn fake(calls: &Arc<AtomicUsize>, level: f64) -> StemRunner {
    let calls = calls.clone();
    Arc::new(move |input: &Path, out: &Path, _: &dyn Fn() -> bool| {
        calls.fetch_add(1, Ordering::SeqCst);
        let (_, src) = decode(&std::fs::read(input).unwrap(), Some("wav")).unwrap();
        let len = (src.frames() as f64 * f64::from(DEMUCS_RATE) / f64::from(src.sample_rate)).round() as usize;
        let gain = input_gain(input, level);
        let dir = out.join("htdemucs").join(input.file_stem().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let mut paths: [PathBuf; 4] = Default::default();
        for (i, (name, path)) in STEMS.iter().zip(paths.iter_mut()).enumerate() {
            let channels = (0..2)
                .map(|ch| (0..len).map(|k| (gain * f64::from(part(i, level, ch, k as f64 / f64::from(DEMUCS_RATE)))) as f32).collect())
                .collect();
            *path = dir.join(format!("{name}.wav"));
            std::fs::write(&*path, float_wav(&AudioBuffer { sample_rate: DEMUCS_RATE, channels })).unwrap();
        }
        Ok(paths)
    })
}

fn cache(dir: &Path) -> StemCache {
    StemCache::new(dir.join("cache")).with_free_space(stems::MIN_FREE, Arc::new(|_: &Path| Some(u64::MAX)))
}

fn run(e: &mut Engine, cmd: &str, p: Value) -> Value {
    e.execute(cmd, &p).unwrap_or_else(|err| panic!("{cmd}: {err}"))
}

/// An engine holding `path` imported twice (two tracks, one clip each): the clip ids.
fn two_decks(path: &Path) -> (Engine, ClipId, ClipId) {
    let mut e = Engine::default();
    let mut clip = || ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    let (a, b) = (clip(), clip());
    (e, a, b)
}

/// Splits `clip` through a job in `cache` with `runner`, and applies it.
fn split(e: &mut Engine, clip: ClipId, cache: &StemCache, runner: &StemRunner) -> Value {
    let job = stems::split_job_in(e, &json!({"clip": clip.0}), cache.clone(), runner.clone()).unwrap().expect("a job");
    job.wait().unwrap().apply(e).unwrap()
}

fn stems_of(e: &Engine, clip: ClipId) -> Option<StemSet> {
    e.session().all_clips().find(|c| c.id == clip).and_then(|c| c.stems.clone())
}

/// The main mix of the first second.
fn mix(e: &Engine) -> Vec<Vec<f32>> {
    soundcraft_mix::render_range(e.session(), Range::new(0, i64::from(RATE)), 1024)
}

/// The largest difference between two mixes inside the second (the resampler's edges see
/// silence past the ends), and the larger mix's peak.
fn mix_error(a: &[Vec<f32>], b: &[Vec<f32>]) -> (f32, f32) {
    let edge = 512;
    let mut worst = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        for (k, (p, q)) in x.iter().zip(y).enumerate() {
            if (edge..x.len() - edge).contains(&k) {
                worst = worst.max((p - q).abs());
            }
        }
    }
    let peak = a.iter().chain(b).flatten().fold(0.0f32, |m, x| m.max(x.abs()));
    (worst, peak)
}

#[test]
fn splitting_gives_every_clip_of_the_audio_its_stems_in_one_undo_step() {
    const LEVEL: f64 = 0.9;
    let dir = scratch("attach");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, b) = two_decks(&path);
    let sources = e.session().sources.len();
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = fake(&calls, LEVEL);
    let undo = e.undo_history().len();
    let r = split(&mut e, a, &c, &runner);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!((r["clip"].as_u64(), r["hit"].as_bool(), r["scale_log2"].as_u64()), (Some(a.0), Some(false), Some(1)), "{r}");
    assert_eq!(r["clips"], json!([a.0, b.0]), "{r}");
    assert_eq!(r["skipped"], json!([]));
    assert!(r["seconds"].as_f64().is_some_and(|s| s >= 0.0), "{r}");
    let key = r["key"].as_str().unwrap().to_string();
    let set = stems_of(&e, a).unwrap();
    assert_eq!((set.key.as_str(), set.scale_log2), (key.as_str(), 1));
    assert_eq!(stems_of(&e, b), Some(set.clone()), "the other clip shares the stems");
    // Four sources, referenced in place in the cache, with their size and time recorded.
    assert_eq!(e.session().sources.len(), sources + 4);
    for (name, id) in STEMS.iter().zip(set.sources) {
        let src = e.session().source(id).unwrap();
        let file = c.root().join(&key).join(format!("{name}.flac"));
        assert_eq!(Path::new(&src.path), file, "{name}");
        assert!(!src.unsaved, "{name}: referenced in place");
        assert_eq!(src.file_len, std::fs::metadata(&file).unwrap().len(), "{name}");
        assert!(src.file_modified_ms > 0 && src.name.ends_with(&format!("({name})")), "{src:?}");
        assert_eq!(e.session().pool.get(id).unwrap().frames(), RATE as usize);
    }
    assert_eq!(e.undo_history().len(), undo + 1, "one undo step");
    run(&mut e, "edit.undo", json!({}));
    assert_eq!((stems_of(&e, a), stems_of(&e, b)), (None, None));
    assert_eq!(e.session().sources.len(), sources, "undo drops the stem sources");
    // Split again: the cache answers, Demucs isn't run.
    let r = split(&mut e, b, &c, &runner);
    assert_eq!((r["hit"].as_bool(), r["key"].as_str(), r["clips"].clone()), (Some(true), Some(key.as_str()), json!([a.0, b.0])));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // A split clip isn't split again.
    assert!(stems::split_job_in(&e, &json!({"clip": a.0}), c.clone(), runner.clone()).unwrap().is_none());
    let before = e.session_arc();
    let r = run(&mut e, "dj.split_stems", json!({"clip": a.0}));
    assert_eq!(r, json!({"clip": a.0, "already": true, "key": key}));
    assert!(Arc::ptr_eq(&before, &e.session_arc()), "nothing changed");
    let r = run(&mut e, "dj.attach_stems", json!({"clip": a.0, "key": key}));
    assert_eq!(r["already"], true);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clips_split_from_other_stems_are_left_alone_and_reported() {
    const LEVEL: f64 = 0.91;
    let dir = scratch("skipped");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, b) = two_decks(&path);
    let other =
        StemSet { key: "0123456789abcdef-1-htdemucs".into(), sources: [SourceId(901), SourceId(902), SourceId(903), SourceId(904)], scale_log2: 0 };
    e.session_mut().find_clip_mut(b).unwrap().stems = Some(other.clone());
    let r = split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    assert_eq!(r["clips"], json!([a.0]));
    assert_eq!(r["skipped"], json!([{"clip": b.0, "key": other.key}]));
    assert_eq!(stems_of(&e, b), Some(other));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A loud master (peaks near +12 dBFS) is stored with headroom; the clip's stems carry the scale,
/// so the split clip plays at the original's level: its mix at 0 dB stem faders matches the
/// unsplit clip's within the resampler's error.
#[test]
fn a_split_clip_renders_like_the_original_even_when_loud() {
    for level in [1.0, 6.0] {
        let dir = scratch(&format!("render-{level}"));
        let path = source(&dir, "track.wav", level);
        let mut e = Engine::default();
        let clip = ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
        let original = mix(&e);
        let r = split(&mut e, clip, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), level));
        let want = if level > 1.0 { 4 } else { 1 };
        assert_eq!(r["scale_log2"], want, "{r}");
        assert_eq!(stems_of(&e, clip).unwrap().scale_log2, want);
        let split_mix = mix(&e);
        let (worst, peak) = mix_error(&original, &split_mix);
        eprintln!("level {level}: split vs original mix: max {worst:.2e} of peak {peak:.2}");
        assert!(peak > 0.5 * level as f32, "{peak}");
        assert!(worst < 2e-4 * level as f32, "level {level}: off by {worst}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Saved and reopened, the stems are still referenced in the cache and play: also when
/// automation is confined to the music folder (the cache is the app's own folder). Only
/// `<cache>/<key>/<stem>.flac` gets that trust: the same file elsewhere stays out of reach.
#[test]
fn a_reopened_session_keeps_its_stems_playable() {
    const LEVEL: f64 = 0.92;
    let dir = scratch("reopen");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, b) = two_decks(&path);
    let c = cache(&dir);
    let key = split(&mut e, a, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL))["key"].as_str().unwrap().to_string();
    let played = mix(&e);
    let set_path = dir.join("music").join("Set.scraft");
    run(&mut e, "session.save_as", json!({"path": set_path}));
    assert!(!dir.join("music").join("Audio Files").exists(), "nothing was copied into the session");
    let roots = AutomationRoots::new(Some(&dir.join("music")), Some(&dir.join("music"))).unwrap();
    for confined in [None, roots.as_ref()] {
        let mut r = Engine::default();
        let opened = confine(confined, || r.execute("session.open", &json!({"path": set_path}))).unwrap();
        assert_eq!(opened["missing"], json!([]), "confined: {}: {opened}", confined.is_some());
        let set = stems_of(&r, a).unwrap();
        assert_eq!((set.key.as_str(), stems_of(&r, b)), (key.as_str(), Some(set.clone())));
        for id in set.sources {
            assert!(r.session().pool.get(id).is_some(), "stem {id:?} loaded");
        }
        assert_eq!(mix(&r), played);
        // Saving again keeps the references to the cache.
        confine(confined, || r.execute("session.save", &json!({}))).unwrap();
        assert!(!dir.join("music").join("Audio Files").exists());
    }
    // The same stem outside a known cache is not read past the roots.
    let elsewhere = dir.join("elsewhere").join(&key);
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::copy(c.root().join(&key).join("vocals.flac"), elsewhere.join("vocals.flac")).unwrap();
    let text = std::fs::read_to_string(&set_path).unwrap();
    let cached = c.root().join(&key).join("vocals.flac").to_string_lossy().replace('\\', "\\\\");
    let moved = elsewhere.join("vocals.flac").to_string_lossy().replace('\\', "\\\\");
    assert!(text.contains(&cached));
    std::fs::write(&set_path, text.replace(&cached, &moved)).unwrap();
    let mut r = Engine::default();
    let opened = confine(roots.as_ref(), || r.execute("session.open", &json!({"path": set_path}))).unwrap();
    let missing = opened["missing"].to_string();
    assert!(missing.contains("outside the read root") && missing.contains("vocals.flac"), "{opened}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stem_file_gone_on_reopen_shows_in_the_health_report() {
    const LEVEL: f64 = 0.93;
    let dir = scratch("health");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    let c = cache(&dir);
    let key = split(&mut e, a, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL))["key"].as_str().unwrap().to_string();
    let set_path = dir.join("music").join("Set.scraft");
    run(&mut e, "session.save_as", json!({"path": set_path}));
    std::fs::remove_file(c.root().join(&key).join("drums.flac")).unwrap();
    let mut r = Engine::default();
    let opened = run(&mut r, "session.open", json!({"path": set_path}));
    assert!(opened["missing"].to_string().contains("drums.flac"), "{opened}");
    let health = run(&mut r, "session.audio_health", json!({}));
    let issues: Vec<&Value> = health["issues"].as_array().unwrap().iter().filter(|i| i["stem"] == "drums").collect();
    assert!(!issues.is_empty() && issues.iter().all(|i| i["code"] == "unavailable_audio"), "{health}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cancelling stops the fake Demucs while it runs (through the split's stop check); the session
/// is untouched and nothing is left in the cache.
#[test]
fn a_cancelled_split_leaves_the_session_as_it_was() {
    const LEVEL: f64 = 0.94;
    let dir = scratch("cancel");
    let path = source(&dir, "track.wav", LEVEL);
    let (e, a, _) = two_decks(&path);
    let before = e.session_arc();
    let c = cache(&dir);
    let stopped = Arc::new(AtomicUsize::new(0));
    let seen = stopped.clone();
    let waits: StemRunner = Arc::new(move |_: &Path, _: &Path, stop: &dyn Fn() -> bool| {
        let started = std::time::Instant::now();
        while !stop() {
            assert!(started.elapsed().as_secs() < 30, "never cancelled");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        seen.fetch_add(1, Ordering::SeqCst);
        Err(CANCELLED.to_string())
    });
    let job = stems::split_job_in(&e, &json!({"clip": a.0}), c.clone(), waits).unwrap().unwrap();
    assert_eq!(job.clip(), a);
    let mut stages = vec![job.progress().stage];
    while job.progress().stage != SEPARATING {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    stages.push(job.progress().stage);
    job.cancel();
    assert!(job.cancelling());
    let err = job.wait().err().unwrap();
    assert_eq!(err.to_string(), CANCELLED);
    assert_eq!(stopped.load(Ordering::SeqCst), 1, "Demucs saw the stop");
    assert_eq!(stages, [DECODING, SEPARATING]);
    assert!(Arc::ptr_eq(&before, &e.session_arc()));
    let left: Vec<_> = std::fs::read_dir(c.root()).unwrap().map(|x| x.unwrap().file_name()).filter(|n| n != stems::MARKER).collect();
    assert!(left.is_empty(), "{left:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cached stem that no longer decodes (its size and header intact, its audio damaged) is a
/// miss: the split removes the entry and runs Demucs again. Asked to attach it directly, the
/// entry is removed too.
#[test]
fn a_cached_stem_that_will_not_load_is_split_again() {
    const LEVEL: f64 = 0.95;
    let dir = scratch("damaged");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    let c = cache(&dir);
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = fake(&calls, LEVEL);
    let key = split(&mut e, a, &c, &runner)["key"].as_str().unwrap().to_string();
    run(&mut e, "edit.undo", json!({}));
    let damage = || {
        let vocals = c.root().join(&key).join("vocals.flac");
        let mut bytes = std::fs::read(&vocals).unwrap();
        let n = bytes.len();
        bytes[n / 4..].fill(0xFF);
        std::fs::write(&vocals, bytes).unwrap();
    };
    damage();
    let r = split(&mut e, a, &c, &runner);
    assert_eq!((r["hit"].as_bool(), calls.load(Ordering::SeqCst)), (Some(false), 2), "{r}");
    assert!(stems_of(&e, a).is_some());
    run(&mut e, "edit.undo", json!({}));
    damage();
    let err = e.execute("dj.attach_stems", &json!({"clip": a.0, "key": key})).unwrap_err().to_string();
    assert!(err.contains("would not load") && err.contains("removed from the stem cache"), "{err}");
    assert!(!c.root().join(&key).exists());
    assert!(stems_of(&e, a).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attach_refuses_keys_that_are_not_this_clips_stems() {
    const LEVEL: f64 = 0.96;
    let dir = scratch("refuse");
    let one = source(&dir, "one.wav", LEVEL);
    let two = source(&dir, "two.wav", 0.5);
    let mut e = Engine::default();
    let mut clip = |p: &Path| ClipId(run(&mut e, "file.import_audio", json!({"path": p}))["imported"][0]["clip"].as_u64().unwrap());
    let (a, b) = (clip(&one), clip(&two));
    let key = split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL))["key"].as_str().unwrap().to_string();
    let attach = |e: &mut Engine, key: &str| e.execute("dj.attach_stems", &json!({"clip": b.0, "key": key})).unwrap_err().to_string();
    assert!(attach(&mut e, &key).contains("stems of another file"));
    assert!(attach(&mut e, "../../x").contains("not a stem cache key"));
    assert!(attach(&mut e, "0123456789abcdef-1-htdemucs").contains("no stems"));
    assert!(e.execute("dj.attach_stems", &json!({"clip": b.0})).unwrap_err().to_string().contains("`key` required"));
    assert!(e.execute("dj.split_stems", &json!({"clip": 999_999})).unwrap_err().to_string().contains("no clip"));
    assert!(stems_of(&e, b).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The stem sources of the session (named "<track> (<stem>)").
fn stem_sources(e: &Engine) -> usize {
    e.session().sources.iter().filter(|s| STEMS.iter().any(|n| s.name.ends_with(&format!("({n})")))).count()
}

/// `path` as the session file's JSON writes it.
fn in_json(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap().trim_matches('"').to_string()
}

/// Splitting a second import of an already split file reuses the four stem sources, so saving
/// copies nothing into the session.
#[test]
fn a_file_split_again_reuses_its_stem_sources() {
    const LEVEL: f64 = 0.97;
    let dir = scratch("reuse");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    let c = cache(&dir);
    let runner = fake(&Arc::new(AtomicUsize::new(0)), LEVEL);
    split(&mut e, a, &c, &runner);
    let again = ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    let r = split(&mut e, again, &c, &runner);
    assert_eq!((r["hit"].as_bool(), r["clips"].clone()), (Some(true), json!([again.0])), "{r}");
    assert_eq!(stem_sources(&e), 4);
    assert_eq!(stems_of(&e, again), stems_of(&e, a));
    run(&mut e, "session.save_as", json!({"path": dir.join("music").join("Set.scraft")}));
    assert!(!dir.join("music").join("Audio Files").exists(), "nothing was copied into the session");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A split that finished for a clip id that, after an undo and another import, names a clip of
/// another file never gives that clip the first file's stems.
#[test]
fn a_stale_split_never_attaches_to_a_clip_that_reuses_its_id() {
    const LEVEL: f64 = 0.98;
    let dir = scratch("stale");
    let first = source(&dir, "first.wav", LEVEL);
    let other = source(&dir, "other.wav", 0.4);
    let mut e = Engine::default();
    let import = |e: &mut Engine, p: &Path| ClipId(run(e, "file.import_audio", json!({"path": p}))["imported"][0]["clip"].as_u64().unwrap());
    let a = import(&mut e, &first);
    let first_source = e.session().all_clips().find(|c| c.id == a).unwrap().source();
    let job = stems::split_job_in(&e, &json!({"clip": a.0}), cache(&dir), fake(&Arc::new(AtomicUsize::new(0)), LEVEL)).unwrap().unwrap();
    let done = job.wait().unwrap();
    run(&mut e, "edit.undo", json!({}));
    let b = import(&mut e, &other);
    assert_eq!(b, a, "the clip id is reused");
    assert_eq!(e.session().all_clips().find(|c| c.id == b).unwrap().source(), first_source, "and the source id");
    let err = done.apply(&mut e).unwrap_err().to_string();
    assert!(err.contains("stems of another file"), "{err}");
    assert!(stems_of(&e, b).is_none() && stem_sources(&e) == 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cancel is honoured however late it comes: on a cache hit the split may be done already,
/// and still nothing is applied.
#[test]
fn a_cancel_on_a_cache_hit_still_stops_the_split() {
    const LEVEL: f64 = 0.99;
    let dir = scratch("late-cancel");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    let c = cache(&dir);
    let runner = fake(&Arc::new(AtomicUsize::new(0)), LEVEL);
    split(&mut e, a, &c, &runner);
    run(&mut e, "edit.undo", json!({}));
    for poll in [false, true] {
        let mut job = stems::split_job_in(&e, &json!({"clip": a.0}), c.clone(), runner.clone()).unwrap().unwrap();
        // Let it finish first: the cancel comes after the stems loaded.
        while job.progress().stage != stems::LOADING {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        job.cancel();
        let r = if poll {
            loop {
                if let Some(r) = job.poll() {
                    break r;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        } else {
            job.wait()
        };
        assert_eq!(r.err().map(|x| x.to_string()).as_deref(), Some(CANCELLED), "poll: {poll}");
    }
    assert!(stems_of(&e, a).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file at another rate than the session's (44.1 kHz in 48 kHz) is resampled on each import,
/// so its clips hold different audio of the same file: they still share one split. The stems are
/// resampled too, so they are copied into the session when it is saved, and the saved session
/// names no file in the cache.
#[test]
fn a_file_at_another_rate_shares_its_split_and_copies_its_stems() {
    const LEVEL: f64 = 0.89;
    let dir = scratch("rate");
    let path = source_at(&dir, "track.wav", LEVEL, 44_100);
    let (mut e, a, b) = two_decks(&path);
    let s = e.session();
    let (sa, sb) = (s.all_clips().find(|c| c.id == a).unwrap().source().unwrap(), s.all_clips().find(|c| c.id == b).unwrap().source().unwrap());
    assert!(!Arc::ptr_eq(s.pool.get(sa).unwrap(), s.pool.get(sb).unwrap()), "two resampled copies");
    let c = cache(&dir);
    let r = split(&mut e, a, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    assert_eq!(r["clips"], json!([a.0, b.0]), "{r}");
    let set = stems_of(&e, a).unwrap();
    for id in set.sources {
        let src = e.session().source(id).unwrap();
        assert!(src.unsaved, "{src:?}: resampled, so not referenced in place");
        assert_eq!(e.session().pool.get(id).unwrap().buffer.sample_rate, RATE);
    }
    let played = mix(&e);
    let set_path = dir.join("music").join("Set.scraft");
    run(&mut e, "session.save_as", json!({"path": set_path}));
    let text = std::fs::read_to_string(&set_path).unwrap();
    assert!(!text.contains(&in_json(c.root())), "the saved session names the cache");
    let copied = std::fs::read_dir(dir.join("music").join("Audio Files")).unwrap().count();
    assert!(copied >= 4, "{copied}");
    let mut r = Engine::default();
    assert_eq!(run(&mut r, "session.open", json!({"path": set_path}))["missing"], json!([]));
    assert_eq!(mix(&r), played);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Under confinement a session's stem paths are matched after `.` and `..` are resolved (and
/// without regard to case on Windows): a detour that ends in the cache is read there, one that
/// ends outside it is not read at all.
#[test]
fn crafted_stem_paths_are_matched_by_where_they_lead() {
    const LEVEL: f64 = 0.88;
    let dir = scratch("crafted");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    let c = cache(&dir);
    let key = split(&mut e, a, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL))["key"].as_str().unwrap().to_string();
    let set_path = dir.join("music").join("Set.scraft");
    run(&mut e, "session.save_as", json!({"path": set_path}));
    let entry = c.root().join(&key);
    let elsewhere = dir.join("elsewhere").join(&key);
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::copy(entry.join("bass.flac"), elsewhere.join("bass.flac")).unwrap();
    let detour = entry.join("..").join(&key).join("vocals.flac");
    let through_music = dir.join("music").join("..").join("cache").join(&key).join("other.flac");
    let out = entry.join("..").join("..").join("elsewhere").join(&key).join("bass.flac");
    let mut text = std::fs::read_to_string(&set_path).unwrap();
    for (stem, to) in [("vocals", &detour), ("other", &through_music), ("bass", &out)] {
        let from = in_json(&entry.join(format!("{stem}.flac")));
        assert!(text.contains(&from), "{stem}");
        text = text.replace(&from, &in_json(to));
    }
    if cfg!(windows) {
        let from = in_json(&entry.join("drums.flac"));
        let root = in_json(c.root());
        text = text.replace(&from, &from.replacen(&root, &root.to_uppercase(), 1));
    }
    std::fs::write(&set_path, text).unwrap();
    let roots = AutomationRoots::new(Some(&dir.join("music")), Some(&dir.join("music"))).unwrap();
    let mut r = Engine::default();
    let opened = confine(roots.as_ref(), || r.execute("session.open", &json!({"path": set_path}))).unwrap();
    let missing = opened["missing"].as_array().unwrap();
    assert_eq!(missing.len(), 1, "{opened}");
    assert!(missing[0].as_str().unwrap().contains("outside the read root") && missing[0].as_str().unwrap().contains("elsewhere"), "{opened}");
    let set = stems_of(&r, a).unwrap();
    let loaded: Vec<bool> = set.sources.iter().map(|id| r.session().pool.get(*id).is_some()).collect();
    assert_eq!(loaded, [true, true, false, true], "vocals, drums, bass, other");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Importing a split clip's track from another session brings its stems along, read in the
/// cache even where automation is confined to the music folder.
#[test]
fn imported_session_data_brings_the_stems_where_automation_is_confined() {
    const LEVEL: f64 = 0.87;
    let dir = scratch("import-data");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    let set_path = dir.join("music").join("Set.scraft");
    run(&mut e, "session.save_as", json!({"path": set_path}));
    let roots = AutomationRoots::new(Some(&dir.join("music")), Some(&dir.join("music"))).unwrap();
    let mut r = Engine::default();
    let got = confine(roots.as_ref(), || r.execute("file.import_session_data", &json!({"path": set_path}))).unwrap();
    assert_eq!(got["missing"], json!([]), "{got}");
    let split_clips: Vec<StemSet> = r.session().all_clips().filter_map(|c| c.stems.clone()).collect();
    assert_eq!(split_clips.len(), 2, "{got}");
    for id in split_clips[0].sources {
        assert!(r.session().pool.get(id).is_some());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
