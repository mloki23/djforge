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

// ─── Task 6: masks, stem copies, Remove Stems and the cache ─────────────────────────────────

/// A WAV like [`source_at`] at 48 kHz holding only the parts `mask` names.
fn parts(dir: &Path, name: &str, level: f64, mask: [bool; 4]) -> PathBuf {
    let frames = RATE as usize;
    let sum = |ch: usize, k: usize| (0..4).filter(|i| mask[*i]).map(|i| part(i, level, ch, k as f64 / f64::from(RATE))).sum();
    let channels = (0..2).map(|ch| (0..frames).map(|k| sum(ch, k)).collect()).collect();
    let path = dir.join("music").join(name);
    std::fs::write(&path, float_wav(&AudioBuffer { sample_rate: RATE, channels })).unwrap();
    path
}

/// A new audio track named `name` (or so, when the name is taken).
fn new_track(e: &mut Engine, name: &str) -> u64 {
    run(e, "track.new", json!({"name": name}));
    e.session().tracks.iter().map(|t| t.id.0).max().unwrap()
}

fn err(e: &mut Engine, cmd: &str, p: Value) -> String {
    e.execute(cmd, &p).unwrap_err().to_string()
}

/// A split clip's mask picks the stems it plays: vocals only sounds like the vocals alone (within
/// the resampler's error), and the refusals name what is wrong.
#[test]
fn a_stem_mask_plays_only_its_stems() {
    const LEVEL: f64 = 0.86;
    let dir = scratch("mask");
    let path = source(&dir, "track.wav", LEVEL);
    let mut e = Engine::default();
    let clip = ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    assert!(err(&mut e, "clip.stem_mask", json!({"clip": clip.0, "mask": ["vocals"]})).contains("split the clip into stems first"));
    split(&mut e, clip, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    assert!(err(&mut e, "clip.stem_mask", json!({"clip": clip.0, "mask": []})).contains("the mask is empty"));
    assert!(err(&mut e, "clip.stem_mask", json!({"clip": clip.0, "mask": ["piano"]})).contains("is not a stem"));
    assert!(err(&mut e, "clip.stem_mask", json!({"clip": clip.0})).contains("`mask` required"));
    let r = run(&mut e, "clip.stem_mask", json!({"clip": clip.0, "mask": ["Vocals"]}));
    assert_eq!(r, json!({"clip": clip.0, "mask": ["vocals"]}));
    assert_eq!(e.session().find_clip(clip).unwrap().1.stem_mask, [true, false, false, false]);
    let vocals = mix(&e);
    let mut want = Engine::default();
    run(&mut want, "file.import_audio", json!({"path": parts(&dir, "vocals.wav", LEVEL, [true, false, false, false])}));
    let (worst, peak) = mix_error(&vocals, &mix(&want));
    assert!(peak > 0.1 && worst < 2e-4, "off by {worst} of {peak}");
    run(&mut e, "edit.undo", json!({}));
    assert_eq!(e.session().find_clip(clip).unwrap().1.stem_mask, [true; 4]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stem copy lands on another audio track at the same start, beat-locked like the original
/// (same warp map), playing its mask, named for what it plays; overlaps and wrong tracks are
/// refused by name.
#[test]
fn stem_copies_are_named_for_their_mask_and_never_overlap() {
    const LEVEL: f64 = 0.85;
    let dir = scratch("copy");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, b) = two_decks(&path);
    let deck_b = e.session().find_clip(b).unwrap().0.0;
    assert!(err(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": deck_b, "mask": ["vocals"]})).contains("split the clip into stems first"));
    split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    let warp = soundcraft_engine::model::WarpMap::new((0..10).map(|k| k as f64 * 22_500.0).collect(), 0.0);
    e.session_mut().find_clip_mut(a).unwrap().warp = warp;
    let vocals = new_track(&mut e, "Vocals");
    let undo = e.undo_history().len();
    let r = run(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": vocals, "mask": ["vocals"]}));
    assert_eq!((r["name"].as_str(), r["track"].as_u64(), r["mask"].clone()), (Some("track (acapella)"), Some(vocals), json!(["vocals"])), "{r}");
    assert_eq!(e.undo_history().len(), undo + 1);
    let copy = e.session().find_clip(ClipId(r["clip"].as_u64().unwrap())).unwrap().1.clone();
    let original = e.session().find_clip(a).unwrap().1.clone();
    assert!(original.warp.as_ref().is_some_and(|w| w.lock.is_some()), "beat-locked");
    assert_ne!(copy.id, a);
    assert_eq!(
        (copy.start, copy.length, &copy.warp, &copy.stems, copy.source()),
        (original.start, original.length, &original.warp, &original.stems, original.source())
    );
    assert_eq!(copy.stem_mask, [true, false, false, false]);
    // The copy is on Vocals; Deck B holds the other import; the original's own track holds it.
    let overlap = err(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": vocals, "mask": ["drums"]}));
    assert!(overlap.contains("would overlap `track (acapella)`"), "{overlap}");
    assert!(err(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": deck_b, "mask": ["drums"]})).contains("would overlap"));
    assert!(err(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": 999_999, "mask": ["drums"]})).contains("no track"));
    run(&mut e, "track.new", json!({"name": "Keys", "kind": "midi"}));
    assert!(err(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": "Keys", "mask": ["drums"]})).contains("not an audio track"));
    assert!(err(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": vocals, "mask": []})).contains("the mask is empty"));
    let inst = new_track(&mut e, "Inst");
    let r = run(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": inst, "mask": ["drums", "bass", "other"]}));
    assert_eq!(r["name"], "track (instrumental)");
    let rhythm = new_track(&mut e, "Rhythm");
    let r = run(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": rhythm, "mask": ["drums", "bass"]}));
    assert_eq!((r["name"].as_str(), r["track"].as_u64()), (Some("track (stems)"), Some(rhythm)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Remove Stems unsplits the clip and every clip of the track split from the same stems that
/// plays all four; a mashup copy (a mask) keeps its stems and their files, until it is unsplit
/// itself. The cache can then be cleared, which it refuses while the session uses cached stems.
#[test]
fn remove_stems_keeps_mashup_copies_then_clear_the_cache() {
    const LEVEL: f64 = 0.84;
    let dir = scratch("unsplit");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, b) = two_decks(&path);
    let c = cache(&dir);
    let key = split(&mut e, a, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL))["key"].as_str().unwrap().to_string();
    let vocals = new_track(&mut e, "Vocals");
    let copy = ClipId(run(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": vocals, "mask": ["vocals"]}))["clip"].as_u64().unwrap());
    let refused = stems::clear_stem_cache_in(&e, &c).unwrap_err().to_string();
    assert!(refused.contains("3 split clips") && refused.contains("(Remove Stems)") && refused.contains("close the session"), "{refused}");
    assert!(c.root().join(&key).exists());
    assert!(err(&mut e, "clip.unsplit_stems", json!({"clip": 999_999})).contains("no clip"));
    let r = run(&mut e, "clip.unsplit_stems", json!({"clip": b.0}));
    assert_eq!((r["clips"].clone(), r["kept"].clone(), r["sources"].as_u64()), (json!([a.0, b.0]), json!([copy.0]), Some(0)), "{r}");
    assert_eq!(e.undo_history().last().map(String::as_str), Some("Remove Stems"));
    for id in [a, b] {
        let clip = e.session().all_clips().find(|x| x.id == id).unwrap().clone();
        assert_eq!((clip.stems, clip.stem_mask), (None, [true; 4]));
    }
    assert_eq!(stems_of(&e, copy).map(|s| s.key), Some(key.clone()), "the acapella keeps its stems");
    assert_eq!(stem_sources(&e), 4);
    assert!(stems::clear_stem_cache_in(&e, &c).unwrap_err().to_string().contains("1 split clip "));
    // Asked for itself, the copy goes back to its audio, and the stem files leave.
    let r = run(&mut e, "clip.unsplit_stems", json!({"clip": copy.0}));
    assert_eq!((r["clips"].clone(), r["kept"].clone(), r["sources"].as_u64()), (json!([copy.0]), json!([]), Some(4)), "{r}");
    assert_eq!(stem_sources(&e), 0);
    assert!(err(&mut e, "clip.unsplit_stems", json!({"clip": a.0})).contains("split the clip into stems first"));
    let cleared = stems::clear_stem_cache_in(&e, &c).unwrap();
    assert!(cleared["files"].as_u64().unwrap() >= 5 && cleared["bytes"].as_u64().unwrap() > 0, "{cleared}");
    assert!(!c.root().join(&key).exists());
    // Undo brings the stems back (the files are gone from the cache, the audio is in memory).
    run(&mut e, "edit.undo", json!({}));
    assert_eq!((stems_of(&e, copy).map(|s| s.key), stem_sources(&e)), (Some(key), 4));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A render that would replace a split clip's audio is refused; setting a stretch that renders
/// nothing (no Elastic mode, or no stretch to remove) is not.
#[test]
fn only_renders_are_refused_on_a_split_clip() {
    const LEVEL: f64 = 0.83;
    let dir = scratch("refuse-render");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    let track = e.session().find_clip(a).unwrap().0.0;
    run(&mut e, "clip.remove_pitch_shift", json!({"clips": [a.0]}));
    run(&mut e, "clip.elastic_properties", json!({"clips": [a.0], "ratio": 1.5}));
    assert_eq!(e.session().find_clip(a).unwrap().1.stretch, 1.5);
    assert!(stems_of(&e, a).is_some());
    assert!(err(&mut e, "clip.remove_pitch_shift", json!({"clips": [a.0]})).contains("Remove Stems"));
    run(&mut e, "track.elastic", json!({"tracks": [track], "algorithm": "polyphonic"}));
    assert!(err(&mut e, "clip.elastic_properties", json!({"clips": [a.0], "ratio": 2.0})).contains("Remove Stems"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Gives the file at `p` a modification time an hour ahead, so a rewrite within the file
/// system's time resolution still counts as a change.
fn touch_later(p: &Path) {
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    std::fs::File::options().write(true).open(p).and_then(|f| f.set_modified(later)).unwrap();
}

/// A file replaced on disk between two imports: the clip of the old audio shares the file's name
/// with the new one, but not its audio, so it doesn't get the new file's stems (it is reported)
/// and can't be split itself until it is imported again.
#[test]
fn a_clip_of_a_replaced_file_never_gets_the_new_files_stems() {
    const LEVEL: f64 = 0.82;
    let dir = scratch("replaced");
    let path = source(&dir, "track.wav", LEVEL);
    let mut e = Engine::default();
    let import = |e: &mut Engine| ClipId(run(e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    let old = import(&mut e);
    // Same name, same length, other audio.
    let replaced = source(&dir, "track.wav", 0.5);
    touch_later(&replaced);
    let new = import(&mut e);
    let c = cache(&dir);
    let r = split(&mut e, new, &c, &fake(&Arc::new(AtomicUsize::new(0)), 0.5));
    assert_eq!(r["clips"], json!([new.0]), "{r}");
    assert_eq!(r["skipped"], json!([{"clip": old.0, "reason": "its audio is not what the split file holds now"}]));
    assert!(stems_of(&e, old).is_none());
    // Only decoding the file tells, so the split's worker refuses.
    let job = stems::split_job_in(&e, &json!({"clip": old.0}), c.clone(), fake(&Arc::new(AtomicUsize::new(0)), 0.5)).unwrap().unwrap();
    let refused = job.wait().err().unwrap().to_string();
    assert!(refused.contains("no longer plays what its file holds"), "{refused}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Stems cached again at the same key (the entry cleared, Demucs giving other output) are new
/// files at the old paths: a later split loads them instead of reusing the session's older stems.
#[test]
fn stems_made_again_at_the_same_key_are_loaded_not_reused() {
    const LEVEL: f64 = 0.81;
    let dir = scratch("remade");
    let path = source(&dir, "track.wav", LEVEL);
    let mut e = Engine::default();
    let import = |e: &mut Engine| ClipId(run(e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    let first = import(&mut e);
    let c = cache(&dir);
    let key = split(&mut e, first, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL))["key"].as_str().unwrap().to_string();
    let second = import(&mut e);
    c.remove(&key).unwrap();
    // This Demucs puts all of the vocals in "other" and leaves the vocals silent.
    let calls = Arc::new(AtomicUsize::new(0));
    let swapped: StemRunner = {
        let inner = fake(&calls, LEVEL);
        Arc::new(move |input: &Path, out: &Path, stop: &dyn Fn() -> bool| {
            let paths = inner(input, out, stop)?;
            let read = |p: &Path| decode(&std::fs::read(p).unwrap(), Some("wav")).unwrap().1;
            let (vocals, mut other) = (read(&paths[0]), read(&paths[3]));
            for (o, v) in other.channels.iter_mut().zip(&vocals.channels) {
                o.iter_mut().zip(v).for_each(|(o, v)| *o += v);
            }
            let silent = AudioBuffer { sample_rate: vocals.sample_rate, channels: vec![vec![0.0; vocals.frames()]; vocals.num_channels()] };
            std::fs::write(&paths[0], float_wav(&silent)).unwrap();
            std::fs::write(&paths[3], float_wav(&other)).unwrap();
            Ok(paths)
        })
    };
    let r = split(&mut e, second, &c, &swapped);
    assert_eq!((r["key"].as_str(), r["hit"].as_bool(), calls.load(Ordering::SeqCst)), (Some(key.as_str()), Some(false), 1), "{r}");
    let (old, new) = (stems_of(&e, first).unwrap(), stems_of(&e, second).unwrap());
    assert_ne!(old.sources, new.sources, "new stem sources");
    let s = e.session();
    let peak = |id: SourceId| s.pool.get(id).unwrap().buffer.channels.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(peak(old.sources[0]) > 0.01 && peak(new.sources[0]) == 0.0, "the second clip plays the new, silent vocals");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A split clip copied, then unsplit (Remove Stems takes its stem files out of the session),
/// pastes as its own audio: stems that left the session can't play.
#[test]
fn a_paste_of_a_clip_whose_stems_left_plays_its_own_audio() {
    const LEVEL: f64 = 0.79;
    let dir = scratch("paste");
    let path = source(&dir, "track.wav", LEVEL);
    let mut e = Engine::default();
    let a = ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    let full = mix(&e);
    split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    let t = e.session().find_clip(a).unwrap().0.0;
    run(&mut e, "clip.stem_mask", json!({"clip": a.0, "mask": ["vocals"]}));
    run(&mut e, "edit.copy", json!({"tracks": [t], "start": 0, "end": RATE}));
    run(&mut e, "clip.unsplit_stems", json!({"clip": a.0}));
    run(&mut e, "edit.clear", json!({"tracks": [t], "start": 0, "end": RATE}));
    run(&mut e, "edit.paste", json!({"tracks": [t], "at": 0}));
    let pasted = e.session().track(soundcraft_engine::model::TrackId(t)).unwrap().clips()[0].clone();
    assert_eq!((pasted.stems, pasted.stem_mask), (None, [true; 4]));
    assert!(mix(&e) == full, "the full source plays");
    let _ = std::fs::remove_dir_all(&dir);
}

/// While a split is under way (until its result is applied or dropped), its cache is not
/// cleared; a finished split whose entry went meanwhile still attaches the stems it loaded.
#[test]
fn a_split_under_way_keeps_its_cache_and_survives_its_entry_going() {
    const LEVEL: f64 = 0.78;
    let dir = scratch("live");
    let path = source(&dir, "track.wav", LEVEL);
    let (mut e, a, _) = two_decks(&path);
    let c = cache(&dir);
    let job = stems::split_job_in(&e, &json!({"clip": a.0}), c.clone(), fake(&Arc::new(AtomicUsize::new(0)), LEVEL)).unwrap().unwrap();
    let under_way = |e: &Engine| stems::clear_stem_cache_in(e, &c).err().map(|x| x.to_string()).unwrap_or_default();
    assert!(under_way(&e).contains("a split into stems is under way"));
    let done = job.wait().unwrap();
    assert!(under_way(&e).contains("a split into stems is under way"), "until it is applied");
    // Gone from the cache (another app cleared it): the loaded stems still attach.
    c.remove(&done.files().key).unwrap();
    let r = done.apply(&mut e).unwrap();
    assert_eq!(r["clips"].as_array().map(Vec::len), Some(2), "{r}");
    let set = stems_of(&e, a).unwrap();
    assert!(set.sources.iter().all(|id| e.session().pool.get(*id).is_some_and(|x| x.frames() == RATE as usize)));
    assert!(!under_way(&e).contains("under way"));
    // Dropping a job ends it too.
    run(&mut e, "edit.undo", json!({}));
    let job = stems::split_job_in(&e, &json!({"clip": a.0}), c.clone(), fake(&Arc::new(AtomicUsize::new(0)), LEVEL)).unwrap().unwrap();
    drop(job);
    assert!(stems::clear_stem_cache_in(&e, &c).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A recorded take (a file the app wrote, its audio added as decoded, no size recorded) splits:
/// whether it still plays its file is learned by decoding it.
#[test]
fn a_recorded_take_splits() {
    const LEVEL: f64 = 0.77;
    let dir = scratch("take");
    let path = source(&dir, "take.wav", LEVEL);
    let mut e = Engine::default();
    let (_, buf) = decode(&std::fs::read(&path).unwrap(), Some("wav")).unwrap();
    let s = e.session_mut();
    let t = s.add_track(soundcraft_engine::model::TrackKind::Audio, soundcraft_engine::model::ChannelFormat::Stereo, Some("Take"));
    let src = soundcraft_engine::io::add_source(s, "take", buf, Some(&path.to_string_lossy()), soundcraft_audio_io::FileFormat::Wav);
    s.sources.iter_mut().for_each(|x| x.unsaved = false);
    let id = s.new_clip_id();
    soundcraft_engine::edit::place_clip(s, t, soundcraft_engine::model::Clip::audio(id, "take", src, 0, 0, i64::from(RATE)));
    let r = split(&mut e, id, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    assert_eq!(r["clips"], json!([id.0]), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The refusal says what to do about each thing that holds cached stems.
#[test]
fn clearing_the_cache_says_what_holds_it() {
    const LEVEL: f64 = 0.76;
    let dir = scratch("holders");
    let path = source(&dir, "track.wav", LEVEL);
    let mut e = Engine::default();
    let a = ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    let c = cache(&dir);
    split(&mut e, a, &c, &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    let stems = stems_of(&e, a).unwrap().sources;
    let s = e.session_mut();
    let sampler = s.add_track(soundcraft_engine::model::TrackKind::Instrument, soundcraft_engine::model::ChannelFormat::Stereo, Some("Pads"));
    let mut ins = soundcraft_engine::model::Insert::new("sampler");
    ins.sample = Some(stems[1]);
    s.track_mut(sampler).unwrap().instrument = Some(ins);
    let why = stems::clear_stem_cache_in(&e, &c).unwrap_err().to_string();
    assert!(why.contains("1 split clip — remove the stems from them first (Remove Stems)"), "{why}");
    assert!(why.contains("the Sampler on `Pads` plays a stem file — change the Sampler's sample"), "{why}");
    e.session_mut().track_mut(sampler).unwrap().instrument = None;
    // The clip unsplit, but its stem files kept in the Clip List.
    e.session_mut().find_clip_mut(a).unwrap().unsplit();
    let why = stems::clear_stem_cache_in(&e, &c).unwrap_err().to_string();
    assert!(why.contains("4 stem files left in the Clip List — Clip List › Clear those files"), "{why}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mask given as a word is refused plainly; the same mask again changes nothing; a copy of a
/// copy is named for its own mask once.
#[test]
fn masks_and_copy_names_are_tidy() {
    const LEVEL: f64 = 0.75;
    let dir = scratch("tidy");
    let path = source(&dir, "track.wav", LEVEL);
    let mut e = Engine::default();
    let a = ClipId(run(&mut e, "file.import_audio", json!({"path": path}))["imported"][0]["clip"].as_u64().unwrap());
    split(&mut e, a, &cache(&dir), &fake(&Arc::new(AtomicUsize::new(0)), LEVEL));
    assert!(err(&mut e, "clip.stem_mask", json!({"clip": a.0, "mask": "vocals"})).contains("`mask` must be an array of stem names"));
    let undo = e.undo_history().len();
    run(&mut e, "clip.stem_mask", json!({"clip": a.0, "mask": ["vocals", "drums", "bass", "other"]}));
    assert_eq!(e.undo_history().len(), undo, "no change, no undo step");
    let one = new_track(&mut e, "One");
    let first = ClipId(run(&mut e, "dj.stem_copy", json!({"clip": a.0, "track": one, "mask": ["vocals"]}))["clip"].as_u64().unwrap());
    let two = new_track(&mut e, "Two");
    let r = run(&mut e, "dj.stem_copy", json!({"clip": first.0, "track": two, "mask": ["drums", "bass", "other"]}));
    assert_eq!(r["name"], "track (instrumental)");
    let _ = std::fs::remove_dir_all(&dir);
}
