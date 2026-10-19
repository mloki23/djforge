//! Import, export, save/load, rendering helpers (bounce, consolidate, commit) and analysis.

use crate::{Engine, EngineError, Result};
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, EncodeOptions, FileFormat};
use soundcraft_model::{BusId, ChannelFormat, Clip, ClipContent, Session, Source, SourceAudio, SourceId, TrackId, TrackKind};
use soundcraft_time::{Range, Samples};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Largest import we accept (samples per channel × channels), ~4 GiB of f32.
pub(crate) const MAX_IMPORT_SAMPLES: usize = 1 << 30;

/// `buf` converted to `rate`, unless it would then hold more than `limit` samples: a file with a
/// tiny sample rate grows by orders of magnitude.
pub(crate) fn at_rate(mut buf: AudioBuffer, rate: u32, limit: usize) -> Option<AudioBuffer> {
    let from = buf.sample_rate.max(1);
    if buf.frames() as f64 * buf.num_channels() as f64 * f64::from(rate) / f64::from(from) > limit as f64 {
        return None;
    }
    if from != rate {
        buf.channels = soundcraft_dsp::offline::resample(&buf.channels, from, rate);
        buf.sample_rate = rate;
    }
    Some(buf)
}

// ─── Checked file access ──────────────────────────────────────────────────────────────────────
//
// Every file a command names by path is read and written through the helpers below, so the
// automation roots (`crate::file_access`, `soundcraft-cli mcp --automation-read-root` and
// `--automation-write-root`) are checked in one place. A test in `file_access` keeps the rest of
// the engine from calling `std::fs` itself. Audio is decoded into the session's pool the moment it
// enters it (import, open, sampler load), so playback and bounce read nothing from disk and
// need no check of their own.

/// The bytes of the file at `path`, which must be inside the automation read root when there is
/// one. Only a regular file qualifies: a device or a pipe named by a session or a command would
/// never end or never answer. Errors name the file.
pub fn read_file(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let path = path.as_ref();
    crate::file_access::check_read(path).map_err(EngineError::Io)?;
    let err = |err: std::io::Error| EngineError::Io(format!("{}: {err}", path.display()));
    if !std::fs::metadata(path).map_err(err)?.is_file() {
        return Err(EngineError::Io(format!("{}: not a regular file", path.display())));
    }
    std::fs::read(path).map_err(err)
}

/// [`read_file`] as UTF-8 text.
pub fn read_text(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    String::from_utf8(read_file(path)?).map_err(|err| EngineError::Io(format!("{}: {err}", path.display())))
}

/// Is there a file at `path` the automation read root lets us see? A path outside the root is
/// reported as not found, so a command can't probe for files beyond it.
pub fn file_found(path: impl AsRef<Path>) -> bool {
    let path = path.as_ref();
    crate::file_access::check_read(path).is_ok() && path.is_file()
}

/// The file a path stored in the session file `session` refers to, or why it is not opened.
///
/// Every path a session names (audio sources, movies) goes through here before it reaches the
/// file system. A relative path is taken from the session's folder, and only files on this
/// computer or on the network share the session itself is on are opened (`craft_docpath`).
/// Paths the person picks in this session (imports) are not stored paths and skip this.
pub fn stored_path(stored: &str, session: &Path) -> std::result::Result<PathBuf, String> {
    let session = std::path::absolute(session).unwrap_or_else(|_| session.to_path_buf());
    let path = craft_docpath::resolve(stored, Some(&session)).map_err(|why| format!("not opened: {why}"))?;
    // A path that is still relative would be taken from the working directory, which the session
    // does not choose.
    if path.is_relative() {
        return Err("not opened: the path is not absolute".into());
    }
    Ok(path)
}

// ─── Audio referenced in place ────────────────────────────────────────────────────────────────
//
// An imported file that plays as decoded is referenced where it is rather than copied into the
// session. The session may keep such a reference only while (a) the session file can reopen it
// (`stored_path`), (b) it is not audio another session or a recording owns (an `Audio Files`
// folder), and (c) the file is still the one the audio in memory was decoded from.

/// A file's size and modification time when its audio was decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn file_stamp(p: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(p).ok()?;
    Some(FileStamp { len: m.len(), modified: m.modified().ok() })
}

/// The size and modification time (ms since 1970) a session records for the file at `p`; zeros
/// when they are unknown.
fn saved_stamp(p: &Path) -> (u64, u64) {
    let Some(st) = file_stamp(p) else { return (0, 0) };
    let ms = st.modified.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    (st.len, ms)
}

/// Where decoded audio came from: keyed by the audio's address, checked against the live `Arc`.
type DecodedFrom = std::collections::BTreeMap<usize, (std::sync::Weak<SourceAudio>, PathBuf, FileStamp)>;
static DECODED_FROM: std::sync::Mutex<DecodedFrom> = std::sync::Mutex::new(std::collections::BTreeMap::new());

fn decoded_from() -> std::sync::MutexGuard<'static, DecodedFrom> {
    DECODED_FROM.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Remember that `audio` was decoded from the file at `p` as it is now.
pub(crate) fn remember_decoded(audio: &Arc<SourceAudio>, p: &Path) {
    let Some(stamp) = file_stamp(p) else { return };
    let key = file_key(p);
    let mut map = decoded_from();
    map.retain(|_, (weak, _, _)| weak.strong_count() > 0);
    map.insert(Arc::as_ptr(audio) as usize, (Arc::downgrade(audio), key, stamp));
}

/// The file `audio` was decoded from and its stamp then, if it is known.
fn decoded_source(audio: &Arc<SourceAudio>) -> Option<(PathBuf, FileStamp)> {
    let map = decoded_from();
    map.get(&(Arc::as_ptr(audio) as usize))
        .filter(|(weak, _, _)| weak.upgrade().is_some_and(|a| Arc::ptr_eq(&a, audio)))
        .map(|(_, key, stamp)| (key.clone(), *stamp))
}

/// True when `audio` was decoded from the file at `p` and the file has not changed since.
pub(crate) fn decoded_unchanged(audio: &Arc<SourceAudio>, p: &Path) -> bool {
    let (key, stamp) = (file_key(p), file_stamp(p));
    decoded_source(audio).is_some_and(|(k, s)| k == key && Some(s) == stamp)
}

/// [`decoded_unchanged`], or else, when `audio` was decoded from `p` but the file's size or
/// time changed (a retag, a copy back), true when `p` still decodes to exactly `audio`: the
/// stamp is then renewed.
fn decodes_unchanged(audio: &Arc<SourceAudio>, p: &Path) -> bool {
    let (key, stamp) = (file_key(p), file_stamp(p));
    let Some((k, s)) = decoded_source(audio) else { return false };
    if k != key {
        return false;
    }
    if Some(s) == stamp {
        return true;
    }
    let ext = p.extension().and_then(|x| x.to_str());
    let again = read_file(p).ok().and_then(|bytes| decode_source(&bytes, ext, audio.buffer.sample_rate).ok());
    let same = again.is_some_and(|a| {
        let (x, y) = (&a.buffer.channels, &audio.buffer.channels);
        x.len() == y.len() && x.iter().zip(y).all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(a, b)| a.to_bits() == b.to_bits()))
    });
    if same {
        remember_decoded(audio, p);
    }
    same
}

/// Refuse to write `out` when it is the file of one of `s`'s sources: a bounce or export must
/// never replace audio the session plays, least of all a library file referenced in place.
pub fn refuse_source_file(s: &Session, out: &Path) -> Result<()> {
    let out_key = file_key(out);
    let hit = s.sources.iter().find(|src| {
        let p = Path::new(&src.path);
        let file = if p.is_absolute() { p.to_path_buf() } else { s.folder.as_deref().map_or_else(|| p.to_path_buf(), |f| f.join(p)) };
        file_key(&file) == out_key
    });
    match hit {
        Some(src) => {
            Err(EngineError::Io(format!("{}: is the audio file of source `{}` in this session; choose another file", out.display(), src.name)))
        }
        None => Ok(()),
    }
}

/// True when `p` is inside a folder named `Audio Files` (any depth, any case).
pub(crate) fn in_audio_files(p: &Path) -> bool {
    p.ancestors().skip(1).any(|a| a.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("Audio Files")))
}

/// The file the session file `session` may keep referencing for the stored absolute path
/// `stored`, holding `audio`: see the section comment. `None` means the audio must be copied.
pub(crate) fn kept_reference(stored: &str, session: &Path, audio: &Arc<SourceAudio>) -> Option<PathBuf> {
    if !Path::new(stored).is_absolute() || in_audio_files(Path::new(stored)) {
        return None;
    }
    let file = stored_path(stored, session).ok()?;
    (file_found(&file) && !in_audio_files(&file) && decodes_unchanged(audio, &file)).then_some(file)
}

/// Create `dir` and its parents, inside the automation write root when there is one.
pub(crate) fn make_dir(dir: impl AsRef<Path>) -> Result<()> {
    let dir = dir.as_ref();
    crate::file_access::check_write(dir).map_err(EngineError::Io)?;
    std::fs::create_dir_all(dir).map_err(|err| EngineError::Io(format!("{}: {err}", dir.display())))
}

/// Write `bytes` to the file at `path`, inside the automation write root when there is one.
pub(crate) fn write_path(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    crate::file_access::check_write(path).map_err(EngineError::Io)?;
    std::fs::write(path, bytes).map_err(|err| EngineError::Io(format!("{}: {err}", path.display())))
}

/// A relative `Audio Files/` path no other source in `s` already uses.
fn unique_audio_rel(s: &Session, stem: &str) -> String {
    let stem = sanitize_name(stem);
    let (plain, prefix) = (format!("Audio Files/{stem}.wav"), format!("Audio Files/{stem}-"));
    let used: std::collections::HashSet<u64> = s
        .sources
        .iter()
        .filter_map(|src| {
            if src.path == plain {
                return Some(1);
            }
            let n = src.path.strip_prefix(&prefix)?.strip_suffix(".wav")?;
            n.parse().ok().filter(|i: &u64| *i > 1 && i.to_string() == n)
        })
        .collect();
    match (1..).find(|i| !used.contains(i)) {
        Some(n) if n > 1 => format!("{prefix}{n}.wav"),
        _ => plain,
    }
}

/// Register decoded audio as a new source. Resamples to the session rate if needed.
pub fn add_source(s: &mut Session, name: &str, mut buf: AudioBuffer, path: Option<&str>, format: FileFormat) -> SourceId {
    let sr = s.sample_rate.hz();
    if buf.sample_rate != sr && buf.sample_rate > 0 {
        buf.channels = soundcraft_dsp::offline::resample(&buf.channels, buf.sample_rate, sr);
        buf.sample_rate = sr;
    }
    let id = SourceId(s.alloc());
    let stem = Path::new(name).file_stem().and_then(|x| x.to_str()).unwrap_or(name).to_string();
    let stored = match path {
        Some(p) => p.to_string(),
        None => unique_audio_rel(s, &stem),
    };
    s.sources.push(Source {
        id,
        name: stem.clone(),
        path: stored,
        channels: u16::try_from(buf.num_channels()).unwrap_or(u16::MAX),
        frames: buf.frames() as u64,
        sample_rate: sr,
        format,
        time_reference: 0,
        unsaved: true,
        file_len: 0,
        file_modified_ms: 0,
    });
    s.pool.insert(id, Arc::new(SourceAudio::new(buf)));
    id
}

/// A BWF time stamp (samples at the file's rate) at the session's rate, as the audio is resampled.
fn stamp_at_rate(stamp: u64, file_rate: u32, session_rate: u32) -> u64 {
    if file_rate == 0 || file_rate == session_rate {
        return stamp;
    }
    let (from, to) = (u128::from(file_rate), u128::from(session_rate));
    u64::try_from((u128::from(stamp) * to + from / 2) / from).unwrap_or(u64::MAX)
}

/// Decode and place an audio file. With no target track, a new track of matching width is made.
pub fn import_audio_bytes(e: &mut Engine, name: &str, bytes: &[u8], path: Option<&str>, target: Option<TrackId>, at: Samples) -> Result<Value> {
    let ext = Path::new(name).extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let (info, buf) = soundcraft_audio_io::decode(bytes, ext.as_deref()).map_err(|err| EngineError::Io(format!("{name}: {err}")))?;
    let buf = at_rate(buf, e.session().sample_rate.hz(), MAX_IMPORT_SAMPLES)
        .ok_or_else(|| EngineError::Io(format!("{name}: file is too large to import")))?;
    if buf.frames() == 0 {
        return Err(EngineError::Io(format!("{name}: no audio")));
    }
    let channels = buf.num_channels();
    let file_rate = info.sample_rate;
    let s = e.session_mut();
    let session_rate = s.sample_rate.hz();
    let src = add_source(s, name, buf, path, info.format);
    // A file that plays as decoded (no resampling), from an absolute path whose extension is the
    // one it was decoded as, is referenced where it is: saving the session does not copy it.
    let ext_of = |p: &str| Path::new(p).extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase);
    let in_place = file_rate == session_rate && path.is_some_and(|p| Path::new(p).is_absolute() && ext_of(p) == ext);
    if let (true, Some(p)) = (in_place, path.map(Path::new)) {
        // The same file imported again shares the audio already decoded from it, so the two
        // sources stay one reference instead of differing audio under one path.
        let key = file_key(p);
        let earlier = s
            .sources
            .iter()
            .filter(|x| x.id != src && !x.unsaved && file_key(Path::new(&x.path)) == key)
            .find_map(|x| s.pool.get(x.id).filter(|a| decoded_unchanged(a, p)).cloned());
        match earlier {
            Some(audio) => s.pool.insert(src, audio),
            None => {
                if let Some(audio) = s.pool.get(src) {
                    remember_decoded(audio, p);
                }
            }
        }
    }
    if let Some(sx) = s.sources.last_mut() {
        sx.unsaved = !in_place;
        if let Some(bwf) = info.bwf.as_ref() {
            sx.time_reference = stamp_at_rate(bwf.time_reference, file_rate, session_rate);
        }
    }
    let frames = s.source(src).map_or(0, |x| i64::try_from(x.frames).unwrap_or(0));
    let track = match target {
        Some(t) => t,
        None => {
            let stem = Path::new(name).file_stem().and_then(|x| x.to_str()).unwrap_or("Audio").to_string();
            s.add_track(TrackKind::Audio, ChannelFormat::for_channels(channels), Some(&stem))
        }
    };
    let cid = s.new_clip_id();
    let stem = s.source(src).map(|x| x.name.clone()).unwrap_or_default();
    crate::edit::place_clip(s, track, Clip::audio(cid, stem, src, 0, at, frames));
    Ok(json!({"track": track, "clip": cid, "source": src, "frames": frames, "channels": channels}))
}

/// Loads an audio file's bytes into an instrument track's Sampler as one undo step ("Load
/// Sampler Sample"): for files that have no path, such as one dropped into the browser. The same
/// checks as `mix.sampler_load`; not journaled, since the bytes can be large.
pub fn load_sampler_bytes(e: &mut Engine, track: TrackId, name: &str, bytes: &[u8]) -> Result<Value> {
    let before = e.session_arc();
    let r = crate::cmd::sampler::attach(e, track, name, bytes, None, Vec::new())?;
    e.push_undo("Load Sampler Sample", before);
    Ok(r)
}

/// Import a movie: a Video track clip spanning the picture (the movie is referenced by `path`,
/// never copied) plus its audio on a new audio track when it has a usable audio stream. Fails
/// only when the file has neither a readable picture nor audio.
pub fn import_video_bytes(e: &mut Engine, path: &str, bytes: Vec<u8>, at: Samples) -> Result<Value> {
    let stem = Path::new(path).file_stem().and_then(|n| n.to_str()).unwrap_or("Video").to_string();
    let ext = Path::new(path).extension().and_then(|x| x.to_str()).unwrap_or("mp4").to_ascii_lowercase();
    let bytes: Arc<[u8]> = bytes.into();
    let movie = soundcraft_video::Movie::open(Arc::clone(&bytes)).map(|m| m.info().clone());
    let audio_name = format!("{stem} audio.{}", if ext == "mov" { "mp4" } else { ext.as_str() });
    let audio = import_audio_bytes(e, &audio_name, &bytes, Some(path), None, at);
    let info = match (movie, &audio) {
        (Ok(info), _) => info,
        (Err(verr), Ok(a)) => {
            e.message(format!("{stem}: imported the audio; no picture ({verr})"));
            return Ok(json!({"audio": a, "video": Value::Null, "video_error": verr.to_string()}));
        }
        (Err(verr), Err(aerr)) => return Err(EngineError::Io(format!("{path}: {verr}; no usable audio stream ({aerr})"))),
    };
    let abs = std::fs::canonicalize(path).map_or_else(|_| path.to_string(), |p| p.to_string_lossy().into_owned());
    let s = e.session_mut();
    let sr = s.sample_rate.as_f64();
    let len = (info.duration * sr).round();
    let len = if len.is_finite() { (len as Samples).clamp(1, sr as Samples * 3600 * 48) } else { 1 };
    let vid = SourceId(s.alloc());
    s.videos.push(soundcraft_model::VideoSource {
        id: vid,
        name: Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("movie").to_string(),
        path: abs,
        width: info.width,
        height: info.height,
        frame_rate: info.frame_rate,
        duration: info.duration,
        codec: info.codec.clone(),
        offline: None,
    });
    // Reuse the first Video track when the new clip does not overlap its clips; else add one at the top.
    let range = Range::new(at, at.saturating_add(len));
    let track = match s.tracks.iter().find(|t| t.kind == TrackKind::Video) {
        Some(t) if !t.clips().iter().any(|c| c.range().overlaps(&range)) => t.id,
        _ => {
            let id = s.add_track(TrackKind::Video, ChannelFormat::Mono, Some(&stem));
            if let Some(i) = s.track_index(id) {
                let t = s.tracks.remove(i);
                s.tracks.insert(0, t);
            }
            id
        }
    };
    let cid = s.new_clip_id();
    crate::edit::place_clip(s, track, Clip::video(cid, stem.clone(), vid, 0, at, len));
    if !info.decodable {
        e.message(format!("{stem}: {} pictures cannot be shown yet; the Video track shows the clip only", info.codec_detail));
    }
    if let Err(err) = &audio {
        e.message(format!("{stem}: no usable audio stream ({err})"));
    }
    Ok(json!({
        "video": {"track": track, "clip": cid, "source": vid, "length": len, "info": info},
        "audio": audio.as_ref().ok(),
        "audio_error": audio.as_ref().err().map(ToString::to_string),
    }))
}

/// Resolve each movie referenced by the session file `session` ([`stored_path`]): keep the
/// resolved path when its file exists, else look for a file of the same name next to the session
/// (moved projects) and relink. A movie whose stored path is refused and has no such copy is
/// marked offline. Returns the movies still missing.
pub fn relink_videos(s: &mut Session, session: &Path) -> Vec<String> {
    let session_dir = session.parent().unwrap_or(Path::new(""));
    let mut missing = Vec::new();
    for v in &mut s.videos {
        v.offline = None;
        let p = stored_path(&v.path, session);
        // The UI opens `path` as it is: keep the checked path, not one relative to the working
        // directory.
        if let Ok(found) = &p
            && file_found(found)
        {
            v.path = found.to_string_lossy().into_owned();
            continue;
        }
        let name = Path::new(&v.path).file_name().map(PathBuf::from).unwrap_or_default();
        let near = [session_dir.join(&name), session_dir.join("Video Files").join(&name)]
            .into_iter()
            .find(|c| !name.as_os_str().is_empty() && file_found(c));
        match (near, p) {
            (Some(c), _) => v.path = c.to_string_lossy().into_owned(),
            (None, Ok(p)) => missing.push(format!("{} (video)", p.display())),
            (None, Err(why)) => {
                missing.push(format!("{} (video): {why}", v.path));
                v.offline = Some(why);
            }
        }
    }
    missing
}

/// Most meter changes a MIDI file may bring into the tempo map; scores have a handful.
const MAX_MIDI_METERS: usize = 10_000;

/// Import a Standard MIDI File as MIDI tracks (one per SMF track).
pub fn import_midi_bytes(e: &mut Engine, bytes: &[u8], at: Samples, tempo_map: bool) -> Result<Vec<TrackId>> {
    let smf = soundcraft_midi::read_smf(bytes).map_err(|err| EngineError::Io(err.to_string()))?;
    let s = e.session_mut();
    if tempo_map {
        // Each meter is placed by walking the ones before it.
        if smf.meters.len() > MAX_MIDI_METERS {
            return Err(EngineError::Io(format!("MIDI file: more than {MAX_MIDI_METERS} meter changes")));
        }
        for (tick, bpm) in &smf.tempos {
            let _ = s.tempo.set_tempo(*tick, *bpm);
        }
        for (tick, n, d) in &smf.meters {
            let _ = s.tempo.set_meter(*tick, u32::from(*n), u32::from(*d));
        }
    }
    let base_tick = s.tempo.samples_to_ticks(at, s.sample_rate);
    let mut out = Vec::new();
    for (i, tr) in smf.tracks.iter().enumerate() {
        if tr.sequence.notes.is_empty() {
            continue;
        }
        let mut name = if tr.name.trim().is_empty() { format!("MIDI {}", i + 1) } else { tr.name.clone() };
        soundcraft_model::truncate_chars(&mut name, soundcraft_model::MAX_NAME_CHARS);
        let id = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some(&name));
        let drums = tr.sequence.notes.iter().all(|n| n.channel == 9);
        if let Some(t) = s.track_mut(id) {
            t.instrument = Some(soundcraft_model::Insert::new(if drums { "drum_synth" } else { "subtractive_synth" }));
        }
        let end_tick = base_tick.saturating_add(tr.sequence.end_tick().max(1));
        let len = s.tempo.tick_to_samples(end_tick, s.sample_rate).saturating_sub(at);
        if len > s.sample_rate.samples(86_400.0) {
            return Err(EngineError::Io(format!("MIDI track {}: longer than 24 hours", i + 1)));
        }
        let cid = s.new_clip_id();
        crate::edit::place_clip(s, id, Clip::midi(cid, name, at, len.max(1), tr.sequence.clone()));
        out.push(id);
    }
    Ok(out)
}

/// Export MIDI clips as a format-1 SMF. `only` limits the file to those tracks.
pub fn export_midi(e: &Engine, only: Option<&[TrackId]>) -> Result<Vec<u8>> {
    let s = e.session();
    let mut smf = soundcraft_midi::Smf {
        format: 1,
        tracks: Vec::new(),
        tempos: s.tempo.tempos().iter().map(|t| (t.tick, t.bpm)).collect(),
        meters: s.tempo.meters().iter().map(|m| (m.tick, u8::try_from(m.numerator).unwrap_or(4), u8::try_from(m.denominator).unwrap_or(4))).collect(),
        markers: s.markers.iter().map(|m| (s.tempo.samples_to_ticks(m.start, s.sample_rate), m.name.clone())).collect(),
        key_sigs: Vec::new(),
    };
    let chosen: Vec<_> = s.tracks.iter().filter(|t| t.kind.is_midi() && only.is_none_or(|ids| ids.contains(&t.id))).collect();
    if only.is_some() && chosen.is_empty() {
        return Err(EngineError::BadParams("file.export_midi".into(), "none of the requested tracks hold MIDI".into()));
    }
    for t in chosen {
        let mut seq = soundcraft_midi::Sequence::default();
        for c in t.clips() {
            if let ClipContent::Midi { sequence } = &c.content {
                let base = s.tempo.samples_to_ticks(c.start, s.sample_rate);
                for n in &sequence.notes {
                    let mut n = *n;
                    n.start = n.start.saturating_add(base);
                    seq.notes.push(n);
                }
                for ev in &sequence.ctrls {
                    let mut ev = *ev;
                    ev.tick = ev.tick.saturating_add(base);
                    seq.ctrls.push(ev);
                }
            }
        }
        seq.sort();
        smf.tracks.push(soundcraft_midi::SmfTrack { name: t.name.clone(), sequence: seq });
    }
    soundcraft_midi::write_smf(&smf).map_err(|err| EngineError::Io(err.to_string()))
}

/// A bounce of the main mix, or of one bus, over a range, written to its file block by block so no
/// bounce has to fit in memory, and rendered a slice at a time ([`Bounce::step`]) so a UI can show
/// progress. The file appears at its path only once complete; a bounce that fails, or is dropped
/// unfinished, leaves any older file there alone.
pub struct Bounce {
    session: Arc<Session>,
    path: PathBuf,
    part: PathBuf,
    range: Range,
    bus: Option<BusId>,
    fold: Option<ChannelFormat>,
    normalize: bool,
    /// Measuring the peak to normalize to, the first of two passes.
    measuring: bool,
    render: soundcraft_mix::RangeRender,
    block: Vec<Vec<f32>>,
    done: usize,
    gain: f32,
    peak: f32,
    meter: soundcraft_dsp::meter::LoudnessMeter,
    enc: Option<soundcraft_audio_io::FileEncoder<Out>>,
}

/// A finished bounce: the size of its file and its levels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounced {
    pub bytes: u64,
    pub peak_db: f32,
    pub lufs: f32,
    pub true_peak_db: f32,
}

impl Bounce {
    /// Starts a bounce of `s`'s main mix, or of `bus`, over `r` to `path`: every channel (SMPTE/WAV
    /// order, with the WAV speaker mask of its format), or an ITU stereo fold-down when
    /// `fold_stereo`, peak-normalized to -0.1 dBFS when `normalize` (which renders twice).
    pub fn new(
        s: Arc<Session>,
        path: &Path,
        r: Range,
        opts: &EncodeOptions,
        bus: Option<BusId>,
        normalize: bool,
        fold_stereo: bool,
    ) -> Result<Bounce> {
        if r.len() > s.sample_rate.samples(MAX_BOUNCE_SECS) {
            return Err(EngineError::BadParams("file.bounce_mix".into(), "bounces are limited to 4 hours".into()));
        }
        refuse_source_file(&s, path)?;
        let (fmt, channels, fold) = bounce_layout(&s, bus, fold_stereo);
        let name = path.file_name().map_or_else(|| "bounce".into(), |n| n.to_string_lossy().into_owned());
        let part = path.with_file_name(format!("{name}.part"));
        crate::file_access::check_write(path).map_err(EngineError::Io)?;
        crate::file_access::check_write(&part).map_err(EngineError::Io)?;
        let enc = (|| -> std::result::Result<_, soundcraft_audio_io::AudioError> {
            let enc = soundcraft_audio_io::FileEncoder::new(Out::create(&part)?, opts, channels, s.sample_rate.hz(), Some(fmt.channel_mask()))?;
            enc.check_room(u64::try_from(r.len()).unwrap_or(0))?;
            Ok(enc)
        })()
        .map_err(|err| {
            let _ = std::fs::remove_file(&part);
            EngineError::Io(format!("{}: {err}", path.display()))
        })?;
        let render = soundcraft_mix::RangeRender::new(&s, bus, r, 1024);
        let meter = soundcraft_dsp::meter::LoudnessMeter::new(s.sample_rate.as_f64() as f32, channels);
        Ok(Bounce {
            session: s,
            path: path.to_path_buf(),
            part,
            range: r,
            bus,
            fold,
            normalize,
            measuring: normalize,
            render,
            block: Vec::new(),
            done: 0,
            gain: 1.0,
            peak: 0.0,
            meter,
            enc: Some(enc),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn range(&self) -> Range {
        self.range
    }

    /// How far along the bounce is, from 0 to 1.
    pub fn progress(&self) -> f32 {
        let pass = (self.done as f64 / self.range.len().max(1) as f64).min(1.0) as f32;
        match (self.normalize, self.measuring) {
            (false, _) => pass,
            (true, true) => pass / 2.0,
            (true, false) => 0.5 + pass / 2.0,
        }
    }

    /// Renders blocks until `more` returns false (at least one per call). Returns the finished
    /// bounce once its file is complete.
    pub fn step(&mut self, mut more: impl FnMut() -> bool) -> Result<Option<Bounced>> {
        loop {
            match self.render.next_block(&self.session) {
                Some((chunk, n)) => {
                    fill(&mut self.block, chunk, n, self.fold, self.gain);
                    self.done += n;
                    self.peak = self.peak.max(soundcraft_dsp::offline::peak(&self.block));
                    if !self.measuring {
                        self.meter.process(&self.block, n);
                        if let Some(enc) = &mut self.enc {
                            enc.write(&self.block, n).map_err(|err| EngineError::Io(format!("{}: {err}", self.path.display())))?;
                        }
                    }
                }
                None if self.measuring => {
                    self.gain = if self.peak > 0.0 { soundcraft_dsp::db_to_gain(-0.1) / self.peak } else { 1.0 };
                    self.peak = 0.0;
                    self.done = 0;
                    self.measuring = false;
                    self.render = soundcraft_mix::RangeRender::new(&self.session, self.bus, self.range, 1024);
                }
                None => return self.finish().map(Some),
            }
            if !more() {
                return Ok(None);
            }
        }
    }

    /// Renders the whole bounce (see [`Bounce::step`]).
    pub fn run(mut self) -> Result<Bounced> {
        loop {
            if let Some(done) = self.step(|| true)? {
                return Ok(done);
            }
        }
    }

    fn finish(&mut self) -> Result<Bounced> {
        let bytes = match self.enc.take() {
            Some(enc) => enc.finish().map_err(|err| EngineError::Io(format!("{}: {err}", self.path.display())))?.complete(&self.part, &self.path)?,
            None => 0,
        };
        Ok(Bounced {
            bytes,
            peak_db: soundcraft_dsp::gain_to_db(self.peak),
            lufs: self.meter.integrated_lufs(),
            true_peak_db: self.meter.true_peak_db(),
        })
    }
}

impl Drop for Bounce {
    /// Removes the part file unless the bounce completed (which renamed it).
    fn drop(&mut self) {
        drop(self.enc.take());
        let _ = std::fs::remove_file(&self.part);
    }
}

/// Where a bounce is written: its part file, or memory in the browser, which has no file system.
enum Out {
    File(std::io::BufWriter<std::fs::File>),
    Memory(std::io::Cursor<Vec<u8>>),
}

impl Out {
    fn create(part: &Path) -> std::io::Result<Out> {
        if cfg!(target_arch = "wasm32") {
            return Ok(Out::Memory(std::io::Cursor::new(Vec::new())));
        }
        Ok(Out::File(std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(part)?)))
    }

    /// Puts the finished file at `path` and returns its size: the part file is synced and renamed
    /// over it; in the browser the bytes are delivered ([`write_file`]).
    fn complete(self, part: &Path, path: &Path) -> Result<u64> {
        match self {
            Out::File(out) => {
                let mut len = 0;
                let synced = out.into_inner().map_err(std::io::IntoInnerError::into_error).and_then(|f| {
                    f.sync_all()?;
                    len = f.metadata()?.len();
                    Ok(())
                });
                replace_with(part, path, synced).map(|()| len)
            }
            Out::Memory(m) => {
                let bytes = m.into_inner();
                write_file(&path.to_string_lossy(), &bytes)?;
                Ok(bytes.len() as u64)
            }
        }
    }
}

impl std::io::Write for Out {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Out::File(f) => f.write(buf),
            Out::Memory(m) => {
                // A long bounce may not fit in the browser's memory: fail instead of aborting.
                let end = usize::try_from(m.position()).unwrap_or(usize::MAX).saturating_add(buf.len());
                let v = m.get_mut();
                v.try_reserve(end.saturating_sub(v.len())).map_err(|_| std::io::ErrorKind::OutOfMemory)?;
                m.write(buf)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Out::File(f) => f.flush(),
            Out::Memory(_) => Ok(()),
        }
    }
}

impl std::io::Seek for Out {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            Out::File(f) => f.seek(pos),
            Out::Memory(m) => m.seek(pos),
        }
    }
}

const MAX_BOUNCE_SECS: f64 = 4.0 * 3600.0;

/// The format, channel count and fold-down source of a bounce of the main mix or `bus`: every
/// channel (a mono or stereo main mix as stereo), or an ITU stereo fold-down of a surround source
/// when `fold_stereo`.
fn bounce_layout(s: &Session, bus: Option<BusId>, fold_stereo: bool) -> (ChannelFormat, usize, Option<ChannelFormat>) {
    let nch = soundcraft_mix::source_channels(s, bus);
    let fmt = match bus {
        Some(id) => s.bus(id).map_or(ChannelFormat::Stereo, |b| b.format),
        None if nch <= 2 => ChannelFormat::Stereo,
        None => s.main_format(),
    };
    if fold_stereo && nch > 2 { (ChannelFormat::Stereo, 2, Some(fmt)) } else { (fmt, nch, None) }
}

/// The first `n` frames of `chunk`, folded down from `fold` when set and scaled by `gain`.
fn fill(block: &mut Vec<Vec<f32>>, chunk: &[Vec<f32>], n: usize, fold: Option<ChannelFormat>, gain: f32) {
    let folded = fold.map(|from| soundcraft_mix::fold_down_stereo(from, chunk));
    let src = folded.as_deref().unwrap_or(chunk);
    block.resize_with(src.len(), Vec::new);
    for (d, c) in block.iter_mut().zip(src) {
        d.clear();
        d.extend(c.iter().take(n).map(|v| v * gain));
    }
}

/// Bounce one file per audio/instrument track (each soloed, so its sends and auxes are included)
/// into `dir`. Returns the written paths.
pub fn bounce_stems(e: &Engine, dir: &str, r: Range, opts: &EncodeOptions) -> Result<Vec<String>> {
    bounce_stems_with(e, dir, r, opts, false)
}

/// [`bounce_stems`] in the main format, or folded down to stereo when `fold_stereo`.
pub fn bounce_stems_with(e: &Engine, dir: &str, r: Range, opts: &EncodeOptions, fold_stereo: bool) -> Result<Vec<String>> {
    let s = e.session();
    if r.len() > s.sample_rate.samples(MAX_BOUNCE_SECS) {
        return Err(EngineError::BadParams("file.bounce_stems".into(), "bounces are limited to 4 hours".into()));
    }
    make_dir(dir)?;
    let ext = match opts.format {
        FileFormat::Aiff => "aif",
        FileFormat::Flac => "flac",
        _ => "wav",
    };
    let mut out = Vec::new();
    let stems: Vec<(TrackId, String)> = s
        .tracks
        .iter()
        .filter(|t| matches!(t.kind, TrackKind::Audio | TrackKind::Instrument | TrackKind::Midi) && !t.inactive && !t.mixer.mute)
        .map(|t| (t.id, t.name.clone()))
        .collect();
    for (id, name) in stems {
        let mut solo = s.clone();
        for t in &mut solo.tracks {
            t.mixer.solo = t.id == id;
        }
        let path = Path::new(dir).join(format!("{} - {}.{ext}", sanitize_name(&s.name), sanitize_name(&name)));
        Bounce::new(Arc::new(solo), &path, r, opts, None, false, fold_stereo)?.run()?;
        out.push(path.to_string_lossy().into_owned());
    }
    Ok(out)
}

/// Limit a range used for rendering to the track's material (plus a second), so a huge
/// selection never renders hours of silence.
pub fn bounded_to_track(s: &Session, t: TrackId, r: Range) -> Range {
    let Some(tr) = s.track(t) else { return Range::point(r.start) };
    let end = tr.clips().iter().map(Clip::end).max().unwrap_or(0).saturating_add(s.sample_rate.samples(1.0));
    let start = r.start.max(0).min(end);
    Range::new(start, r.end.min(end).max(start))
}

/// Transient positions (absolute) in a track's clips within `r`.
pub fn transients_in(s: &Session, t: TrackId, r: Range, sens: f32) -> Vec<Samples> {
    let r = if r.is_empty() { Range::new(0, s.content_end()) } else { r };
    let r = bounded_to_track(s, t, r);
    let audio = soundcraft_mix::render_clips(s, t, r);
    soundcraft_dsp::offline::detect_transients(&audio, s.sample_rate.as_f64() as f32, sens).into_iter().map(|i| r.start + i as i64).collect()
}

/// Strip Silence: keep only the non-silent ranges of clips inside `r`.
pub fn strip_silence(s: &mut Session, t: TrackId, r: Range, thr: f32, min_len: Samples, pre: Samples, post: Samples) -> Result<usize> {
    let r = bounded_to_track(s, t, r);
    let keep = non_silent(s, t, r, thr, min_len, pre, post)?;
    let mut cut_points = vec![r.start];
    for (a, b) in &keep {
        cut_points.push(*a);
        cut_points.push(*b);
    }
    cut_points.push(r.end);
    for p in &cut_points {
        crate::edit::separate_at(s, t, *p);
    }
    // Remove pieces that fall in the gaps.
    let mut gaps = Vec::new();
    let mut prev = r.start;
    for (a, b) in &keep {
        if *a > prev {
            gaps.push(Range::new(prev, *a));
        }
        prev = *b;
    }
    if prev < r.end {
        gaps.push(Range::new(prev, r.end));
    }
    if let Some(pl) = s.track_mut(t).and_then(|x| x.playlist_mut()) {
        pl.clips.retain(|c| !gaps.iter().any(|g| c.start >= g.start && c.end() <= g.end));
    }
    Ok(keep.len())
}

/// Most samples (over all channels) Strip Silence renders at once.
const STRIP_WINDOW_SAMPLES: usize = 1 << 24;

/// The non-silent ranges of `t`'s clips in `r`. The audio is rendered a window at a time where
/// audio clips play, so all of `r` is scanned however long it is (one render stops at
/// `MAX_RENDER_SAMPLES`); at most a day of clips, Pro Tools' longest session.
fn non_silent(s: &Session, t: TrackId, r: Range, thr: f32, min_len: Samples, pre: Samples, post: Samples) -> Result<Vec<(Samples, Samples)>> {
    let Some(tr) = s.track(t) else { return Ok(Vec::new()) };
    let mut clips: Vec<Range> = tr.clips().iter().filter(|c| c.is_audio() && !c.muted).filter_map(|c| c.range().intersect(&r)).collect();
    clips.sort_by_key(|x| x.start);
    let mut spans: Vec<Range> = Vec::new();
    for x in clips {
        match spans.last_mut() {
            Some(last) if x.start <= last.end => last.end = last.end.max(x.end),
            _ => spans.push(x),
        }
    }
    if spans.iter().map(Range::len).fold(0, i64::saturating_add) > s.sample_rate.samples(86_400.0) {
        return Err(EngineError::BadParams("edit.strip_silence".into(), format!("`{}` has more than a day of clips to scan", tr.name)));
    }
    let window = i64::try_from(STRIP_WINDOW_SAMPLES / tr.channels().max(1)).unwrap_or(i64::MAX);
    let len = |n: Samples| u64::try_from(n).unwrap_or(0);
    let mut scan = soundcraft_dsp::offline::SilenceScan::new(thr, len(min_len));
    let mut at = r.start;
    for x in spans {
        scan.skip(len(x.start - at));
        at = x.start;
        while at < x.end {
            let end = at.saturating_add(window).min(x.end);
            scan.push(&soundcraft_mix::render_clips(s, t, Range::new(at, end)));
            at = end;
        }
    }
    scan.skip(len(r.end - at));
    Ok(scan.finish(len(pre), len(post)).into_iter().map(|(a, b)| (r.start.saturating_add_unsigned(a), r.start.saturating_add_unsigned(b))).collect())
}

/// Consolidate `r` on a track into one new clip backed by a rendered source. The clip covers the
/// whole range, silence included; a range too long to render is refused rather than shortened.
pub fn consolidate(e: &mut Engine, cmd: &str, t: TrackId, r: Range) -> Result<bool> {
    let s = e.session();
    let Some(tr) = s.track(t).filter(|tr| tr.kind.has_playlist() && !tr.kind.is_midi()) else { return Ok(false) };
    let start = r.start.max(0);
    let len = r.end.saturating_sub(start);
    if len <= 0 {
        return Ok(false);
    }
    let max = soundcraft_mix::MAX_RENDER_SAMPLES / tr.channels().max(1);
    if usize::try_from(len).map_or(true, |n| n > max) {
        let secs = max as f64 / s.sample_rate.as_f64();
        return Err(crate::cmd::bad(cmd, format!("selection is too long to consolidate on `{}` (at most {secs:.0} s)", tr.name)));
    }
    let r = Range::new(start, start + len);
    let audio = soundcraft_mix::render_clips(s, t, r);
    if audio.iter().any(|ch| ch.len() as i64 != len) {
        return Err(EngineError::Internal(cmd.to_string(), "consolidate rendered the wrong length".into()));
    }
    let name = format!("{}_consolidated", tr.name);
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
    let s = e.session_mut();
    let src = add_source(s, &name, buf, None, FileFormat::Wav);
    crate::edit::clear_range(s, t, r, false);
    let cid = s.new_clip_id();
    crate::edit::place_clip(s, t, Clip::audio(cid, name, src, 0, r.start, r.len()));
    Ok(true)
}

/// The most frames of `channels`-channel audio one offline render holds.
pub fn render_limit(channels: usize) -> usize {
    soundcraft_mix::MAX_RENDER_SAMPLES / channels.max(1)
}

/// Refuses `len` frames of `name` past `max`, where a render would cut the audio short.
pub fn refuse_longer(s: &Session, cmd: &str, name: &str, verb: &str, len: Samples, max: usize) -> Result<()> {
    if len <= Samples::try_from(max).unwrap_or(Samples::MAX) {
        return Ok(());
    }
    let secs = max as f64 / s.sample_rate.as_f64();
    Err(crate::cmd::bad(cmd, format!("`{name}` is too long to {verb} (at most {secs:.0} s)")))
}

/// Render clip gain into a new source and reset the clip's gain.
pub fn render_clip_gain(e: &mut Engine, cmd: &str, id: soundcraft_model::ClipId) -> Result<bool> {
    let s = e.session();
    let Some((t, c)) = s.find_clip(id).map(|(t, c)| (t, c.clone())) else { return Ok(false) };
    if !c.is_audio() {
        return Ok(false);
    }
    refuse_longer(s, cmd, &c.name, "render", c.length, render_limit(s.track(t).map_or(1, |x| x.channels())))?;
    let mut c2 = c.clone();
    c2.fade_in = Default::default();
    c2.fade_out = Default::default();
    let mut tmp = s.clone();
    if let Some(pl) = tmp.track_mut(t).and_then(|x| x.playlist_mut()) {
        pl.clips = vec![c2];
    }
    let audio = soundcraft_mix::render_clips(&tmp, t, c.range());
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
    let s = e.session_mut();
    let src = add_source(s, &format!("{}_gain", c.name), buf, None, FileFormat::Wav);
    if let Some(cl) = s.find_clip_mut(id) {
        cl.content = ClipContent::Audio { source: src, offset: 0 };
        cl.gain_db = 0.0;
        cl.gain_env.clear();
        // The new source is the clip as it already sounds, stretch included.
        // Keeping the old ratio would play that audio through the stretch again.
        cl.stretch = 1.0;
    }
    Ok(true)
}

/// Commit a track's inserts into a new audio track (Track › Commit).
pub fn commit_track(e: &mut Engine, t: TrackId) -> Result<Option<TrackId>> {
    let s = e.session();
    let Some(tr) = s.track(t) else { return Ok(None) };
    if !tr.kind.is_audio_path() || tr.kind == TrackKind::Master {
        return Ok(None);
    }
    let end = tr.clips().iter().map(Clip::end).max().unwrap_or(0);
    if end <= 0 {
        return Ok(None);
    }
    let range = Range::new(0, end + s.sample_rate.samples(2.0));
    // The track renders through a main mix of at least two channels.
    refuse_longer(s, "track.commit", &tr.name, "commit", range.len(), render_limit(tr.channels().max(2)))?;
    let audio = soundcraft_mix::render_track_pre_fader(s, t, range);
    let name = format!("{}.cm", tr.name);
    let fmt = ChannelFormat::for_channels(audio.len().clamp(1, 2));
    let mixer = tr.mixer.clone();
    let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
    let s = e.session_mut();
    let src = add_source(s, &name, buf, None, FileFormat::Wav);
    let nt = s.add_track(TrackKind::Audio, fmt, Some(&name));
    if let (Some(idx), Some(new_idx)) = (s.track_index(t), s.track_index(nt)) {
        let tr = s.tracks.remove(new_idx);
        s.tracks.insert((idx + 1).min(s.tracks.len()), tr);
    }
    let cid = s.new_clip_id();
    if let Some(ntr) = s.track_mut(nt) {
        ntr.mixer.volume_db = mixer.volume_db;
        ntr.mixer.output = mixer.output.clone();
        ntr.mixer.sends = mixer.sends.clone();
        if let Some(pl) = ntr.playlist_mut() {
            pl.clips.push(Clip::audio(cid, name, src, 0, 0, range.len()));
        }
    }
    if let Some(old) = s.track_mut(t) {
        old.inactive = true;
        old.hidden = true;
    }
    Ok(Some(nt))
}

pub fn export_clips(e: &Engine, ids: &[soundcraft_model::ClipId], dir: &str) -> Result<usize> {
    let s = e.session();
    make_dir(dir)?;
    let mut n = 0;
    for id in ids {
        let Some((t, c)) = s.find_clip(*id) else { continue };
        let audio = soundcraft_mix::render_clips(s, t, c.range());
        let buf = AudioBuffer { sample_rate: s.sample_rate.hz(), channels: audio };
        let bytes = soundcraft_audio_io::encode(&buf, &EncodeOptions::default()).map_err(|err| EngineError::Io(err.to_string()))?;
        let stem = sanitize_name(&c.name);
        let mut path = Path::new(dir).join(format!("{stem}.wav"));
        let mut k = 2u32;
        while path.exists() {
            path = Path::new(dir).join(format!("{stem}-{k}.wav"));
            k = k.saturating_add(1);
            if k > 10_000 {
                return Err(EngineError::Io(format!("{dir}: too many files named {stem}")));
            }
        }
        write_path(&path, bytes)?;
        n += 1;
    }
    Ok(n)
}

pub fn sanitize_name(n: &str) -> String {
    let s: String = n.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' }).collect();
    let s = s.trim().trim_start_matches('.').to_string();
    if s.is_empty() { "untitled".into() } else { s.chars().take(100).collect() }
}

/// Save the session to `path` (a `.scraft` file). Audio that only exists in memory is written
/// as 32-bit float WAV into `Audio Files/` next to it. Returns how many audio files were written.
///
/// In the browser there are no folders: the session and all its audio go out as one bundle file
/// (`<name>.scraftz`, see [`session_bundle`]) through the host's delivery hook ([`set_deliver`]).
pub fn save_session(e: &mut Engine, path: &str) -> Result<usize> {
    if cfg!(target_arch = "wasm32") {
        return save_session_bundle(e, path);
    }
    let (fresh, written, p) = write_session(e.session(), path, false, false)?;
    // Keep the in-memory document in sync (paths, unsaved flags) without an undo step.
    *e.session_mut() = fresh;
    e.path = Some(p.to_string_lossy().into_owned());
    e.mark_clean();
    Ok(written)
}

/// Write a copy of the session (and any audio it needs) without touching the open document —
/// used for autosave / crash recovery. Audio already written for a source is reused.
pub fn save_copy(e: &Engine, path: &str) -> Result<usize> {
    write_session(e.session(), path, true, false).map(|(_, n, _)| n)
}

/// File › Save Session Copy In: a normal session folder that does not rename, retarget or
/// clean the session that stays open. With `copy_audio` the copy is self-contained: every
/// source with audio is written into its `Audio Files`, files referenced in place included.
pub fn save_session_copy(e: &Engine, path: &str, copy_audio: bool) -> Result<usize> {
    write_session(e.session(), path, false, copy_audio).map(|(_, n, _)| n)
}

/// Write `session` to `path`; returns the session with updated media paths, the number of audio
/// files written and the final path. `copy_audio` copies every source with audio that is not
/// already in the new session's own folder.
fn write_session(session: &Session, path: &str, overwrite: bool, copy_audio: bool) -> Result<(Session, usize, PathBuf)> {
    use std::collections::{HashMap, HashSet};
    let p = session_file(path);
    // Before any audio is written: a session saved outside the write root changes nothing.
    crate::file_access::check_write(&p).map_err(EngineError::Io)?;
    refuse_source_file(session, &p)?;
    let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
    let audio_dir = dir.join("Audio Files");
    let mut s = session.clone();
    let from = s.folder.clone();
    let same_dir = from.as_deref().is_some_and(|f| same_path(f, &dir));
    let mut written = 0;
    // A file can stay put for all the sources that name it, unless their audio differs.
    let shared_paths: Vec<bool> = {
        let mut audio_by_path: HashMap<&str, HashSet<Option<*const SourceAudio>>> = HashMap::new();
        for src in &s.sources {
            audio_by_path.entry(&src.path).or_default().insert(s.pool.get(src.id).map(Arc::as_ptr));
        }
        s.sources.iter().map(|src| audio_by_path.get(src.path.as_str()).is_some_and(|a| a.len() > 1)).collect()
    };
    // Files written for audio during this save: sources playing the same audio share one.
    let mut files: HashMap<*const SourceAudio, String> = HashMap::new();
    for (i, src) in s.sources.iter_mut().enumerate() {
        if !same_dir
            && s.pool.get(src.id).is_none()
            && Path::new(&src.path).is_relative()
            && let Some(f) = &from
        {
            src.path = f.join(&src.path).to_string_lossy().into_owned();
        }
        let exists_rel = same_dir && !Path::new(&src.path).is_absolute() && dir.join(&src.path).exists();
        let shared = shared_paths.get(i).copied().unwrap_or(false);
        if exists_rel && !src.unsaved && !shared && (!copy_audio || src.path.starts_with("Audio Files/")) {
            continue;
        }
        let in_folder = Path::new(&src.path).strip_prefix(&audio_dir).ok().and_then(Path::to_str).map(str::to_string);
        if let Some(f) = in_folder.filter(|_| !src.unsaved && Path::new(&src.path).is_file()) {
            src.path = format!("Audio Files/{f}");
            continue;
        }
        let Some(audio) = s.pool.get(src.id) else { continue };
        if !src.unsaved && !shared && !copy_audio && kept_reference(&src.path, &p, audio).is_some() {
            continue;
        }
        if let Some(path) = files.get(&Arc::as_ptr(audio)) {
            src.path.clone_from(path);
            src.unsaved = false;
            src.format = FileFormat::Wav;
            continue;
        }
        make_dir(&audio_dir)?;
        let mut stem = sanitize_name(&src.name);
        if overwrite {
            // One file per source id so autosaves never collide or pile up.
            stem = format!("{stem}_{}", src.id.0);
            if audio_dir.join(format!("{stem}.wav")).exists() {
                src.path = format!("Audio Files/{stem}.wav");
                src.unsaved = false;
                files.insert(Arc::as_ptr(audio), src.path.clone());
                continue;
            }
        }
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: soundcraft_audio_io::BitDepth::Float32, dither: false, bwf: None };
        let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts).map_err(|err| EngineError::Io(err.to_string()))?;
        let (name, new) = store_wav(&audio_dir, &stem, &bytes)?;
        src.path = format!("Audio Files/{name}");
        src.unsaved = false;
        src.format = FileFormat::Wav;
        files.insert(Arc::as_ptr(audio), src.path.clone());
        written += usize::from(new);
    }
    if written > 0 {
        sync_dir(&audio_dir);
    }
    // What each file was like when saved, so opening can tell when one changed since.
    for src in &mut s.sources {
        let file = if Path::new(&src.path).is_absolute() { PathBuf::from(&src.path) } else { dir.join(&src.path) };
        (src.file_len, src.file_modified_ms) = saved_stamp(&file);
    }
    s.folder = Some(dir.clone());
    if !overwrite && let Some(stem) = p.file_stem().and_then(|x| x.to_str()) {
        s.name = stem.to_string();
    }
    let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
    make_dir(&dir)?;
    // Atomic-ish write: temp file then rename.
    let tmp = p.with_extension("scraft.tmp");
    if !overwrite && read_file(&p).is_ok_and(|old| old != text.as_bytes()) {
        back_up(&p);
    }
    replace_file(&tmp, &p, text.as_bytes())?;
    Ok((s, written, p))
}

/// Write `bytes` to `tmp`, flush it to disk and rename it over `file`, so `file` holds either the
/// old or the new contents, never a mix.
pub fn replace_file(tmp: &Path, file: &Path, bytes: &[u8]) -> Result<()> {
    crate::file_access::check_write(file).map_err(EngineError::Io)?;
    crate::file_access::check_write(tmp).map_err(EngineError::Io)?;
    replace_with(tmp, file, write_synced(tmp, bytes))
}

/// Renames `tmp` over `file` once it is `written`, or removes it when writing or renaming failed.
/// The caller has checked both paths ([`crate::file_access::check_write`]).
fn replace_with(tmp: &Path, file: &Path, written: std::io::Result<()>) -> Result<()> {
    if let Err(err) = written.and_then(|()| std::fs::rename(tmp, file)) {
        let _ = std::fs::remove_file(tmp);
        return Err(EngineError::Io(format!("{}: {err}", file.display())));
    }
    sync_dir(file.parent().unwrap_or(Path::new("")));
    Ok(())
}

fn sync_dir(dir: &Path) {
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Copy the session file about to be replaced to `Backups/<name> backup <n>.<ext>`, keeping the
/// newest ten.
fn back_up(file: &Path) {
    let (Some(dir), Some(stem), Some(ext)) = (file.parent(), file.file_stem().and_then(|x| x.to_str()), file.extension().and_then(|x| x.to_str()))
    else {
        return;
    };
    let backups = dir.join("Backups");
    if let Err(err) = crate::file_access::check_write(&backups) {
        log::warn!("no backup of {}: {err}", file.display());
        return;
    }
    let prefix = format!("{stem} backup ");
    let suffix = format!(".{ext}");
    let numbers: Vec<u32> = std::fs::read_dir(&backups)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| f.file_name().to_str()?.strip_prefix(&prefix)?.strip_suffix(&suffix)?.parse().ok())
        .collect();
    let next = numbers.iter().max().map_or(1, |n| n.saturating_add(1));
    let copied = std::fs::create_dir_all(&backups).and_then(|()| std::fs::copy(file, backups.join(format!("{prefix}{next}{suffix}"))));
    if let Err(err) = copied {
        log::warn!("no backup of {}: {err}", file.display());
        return;
    }
    for n in numbers.into_iter().filter(|n| n.saturating_add(10) <= next) {
        let _ = std::fs::remove_file(backups.join(format!("{prefix}{n}{suffix}")));
    }
}

/// The file a session saved to `path` goes to: `path`, with `.scraft` added when it has no
/// extension and isn't an existing file.
pub fn session_file(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    // Only a path the write root allows is probed, so a refused one can't be used to test for files.
    let is_file = crate::file_access::check_write(&p).is_ok() && p.is_file();
    if p.extension().is_none() && !is_file { p.with_extension(soundcraft_model::SESSION_EXTENSION) } else { p }
}

/// True when `command` would write a session over the existing `file`. Save, Save As and Save
/// Copy In may write the open session's own file; other commands never ask.
pub fn replaces(e: &Engine, command: &str, file: &Path) -> bool {
    match command {
        "session.save" | "session.save_as" | "session.save_copy" => {
            file.exists() && !e.path.as_deref().is_some_and(|own| same_path(Path::new(own), file))
        }
        "session.save_template" | "file.export_selected_tracks_as_session" => file.exists(),
        _ => false,
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let full = |p: &Path| if p.as_os_str().is_empty() { Path::new(".").canonicalize() } else { p.canonicalize() };
    a == b || matches!((full(a), full(b)), (Ok(x), Ok(y)) if x == y)
}

/// Store `bytes` as the first of `<stem>.wav`, `<stem>_01.wav`, … in `dir` that is free or already
/// holds exactly these bytes, so saving never replaces audio; returns the name and whether it was
/// written.
fn store_wav(dir: &Path, stem: &str, bytes: &[u8]) -> Result<(String, bool)> {
    use std::io::Write;
    for k in 0..10_000u32 {
        let name = if k == 0 { format!("{stem}.wav") } else { format!("{stem}_{k:02}.wav") };
        let path = dir.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                if let Err(err) = f.write_all(bytes).and_then(|()| f.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(EngineError::Io(format!("{}: {err}", path.display())));
                }
                return Ok((name, true));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) && std::fs::read(&path).is_ok_and(|b| b == bytes) {
                    return Ok((name, false));
                }
            }
            Err(err) => return Err(EngineError::Io(format!("{}: {err}", path.display()))),
        }
    }
    Err(EngineError::Io(format!("{}: no free file name for {stem}", dir.display())))
}

/// Open a `.scraft` session and decode its audio files (missing files are reported, not fatal).
pub fn open_session(e: &mut Engine, path: &str) -> Result<Vec<String>> {
    open_session_linked(e, path, Path::new(path))
}

/// [`open_session`], following the media paths in it as if they were stored in the session file
/// `links` (crash recovery: the session written beside the autosave, its paths from the original).
pub(crate) fn open_session_linked(e: &mut Engine, path: &str, links: &Path) -> Result<Vec<String>> {
    open_session_report(e, path, links).map(|r| r.missing)
}

/// What opening a session found wrong with its media.
#[derive(Debug, Default)]
pub struct OpenReport {
    /// Media that could not be loaded.
    pub missing: Vec<String>,
    /// Audio files that loaded with a different length or width than the session recorded:
    /// the file changed on disk since it was imported or saved.
    pub changed: Vec<String>,
    /// Information: audio files whose size or modification time differ from when the session
    /// was saved, though they decode to the recorded length (a retag, or an edit of the same
    /// length).
    pub modified: Vec<String>,
}

/// [`open_session`], reporting audio files that changed on disk as well as missing media.
pub fn open_session_checked(e: &mut Engine, path: &str) -> Result<OpenReport> {
    open_session_report(e, path, Path::new(path))
}

fn open_session_report(e: &mut Engine, path: &str, links: &Path) -> Result<OpenReport> {
    let text = read_text(path)?;
    let mut s = Session::from_json(&text).map_err(|err| EngineError::Io(err.to_string()))?;
    let dir = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    let mut modified = Vec::new();
    let mut files = DecodedFiles::new();
    for src in &s.sources {
        let decoded = stored_path(&src.path, links)
            .map_err(|why| format!("{}: {why}", src.path))
            .and_then(|p| decode_file(&mut files, &p, s.sample_rate.hz()).map(|a| (p, a)));
        match decoded {
            Ok((p, audio)) => {
                let (frames, channels) = (audio.buffer.frames() as u64, audio.buffer.num_channels());
                if frames != src.frames || channels != usize::from(src.channels) {
                    changed.push(format!(
                        "{}: changed on disk since it was imported: {frames} frames, {channels} channels vs {} frames, {} channels",
                        p.display(),
                        src.frames,
                        src.channels
                    ));
                } else if src.file_len != 0 && saved_stamp(&p) != (src.file_len, src.file_modified_ms) {
                    modified.push(format!("{}: modified since the session was saved (same length; tags or audio may differ)", p.display()));
                }
                s.pool.insert(src.id, audio);
            }
            Err(err) => missing.push(err),
        }
    }
    missing.extend(relink_videos(&mut s, links));
    s.folder = Some(dir);
    e.replace_session(s);
    e.path = Some(path.to_string());
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    for m in changed.iter().chain(&modified) {
        e.message(m.clone());
    }
    Ok(OpenReport { missing, changed, modified })
}

/// Decode one source's audio file at the session rate.
fn decode_source(bytes: &[u8], ext: Option<&str>, rate: u32) -> std::result::Result<Arc<SourceAudio>, String> {
    let (_, buf) = soundcraft_audio_io::decode(bytes, ext).map_err(|err| err.to_string())?;
    let buf = at_rate(buf, rate, MAX_IMPORT_SAMPLES).ok_or("too large to load")?;
    Ok(Arc::new(SourceAudio::new(buf)))
}

/// Audio files decoded so far, by path: a session may name one file thousands of times.
/// The one name of the file at `p` (links followed), so the same file named two ways is read once.
pub(crate) fn file_key(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

pub(crate) type DecodedFiles = std::collections::HashMap<PathBuf, std::result::Result<Arc<SourceAudio>, String>>;

/// The audio of the file at `p` at `rate`, decoded the first time it is asked for. The error
/// names the file.
/// A file named in a session is hostile input too: a path that leaves the read root is not read.
pub(crate) fn decode_file(files: &mut DecodedFiles, p: &Path, rate: u32) -> std::result::Result<Arc<SourceAudio>, String> {
    crate::file_access::check_read(p)?;
    files
        .entry(file_key(p))
        .or_insert_with(|| {
            let bytes = read_file(p).map_err(|_| p.display().to_string())?;
            let audio = decode_source(&bytes, p.extension().and_then(|x| x.to_str()), rate).map_err(|err| format!("{}: {err}", p.display()))?;
            remember_decoded(&audio, p);
            Ok(audio)
        })
        .clone()
}

// ─── Relinking moved audio ────────────────────────────────────────────────────────────────────

/// How deep [`relink_audio`] looks into the folder it is given, and how many files it looks at.
const RELINK_MAX_DEPTH: usize = 16;
const RELINK_MAX_FILES: usize = 200_000;

/// A Camelot key tag (`8A`, `12B`).
fn is_camelot(seg: &str) -> bool {
    let t = seg.trim().to_ascii_lowercase();
    t.len() <= 3 && t.strip_suffix(['a', 'b']).and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=12).contains(&n))
}

fn is_number(seg: &str) -> bool {
    let t = seg.trim();
    !t.is_empty() && t.len() <= 3 && t.bytes().all(|b| b.is_ascii_digit())
}

/// True when part `i` of `parts` is a tag Mixed In Key adds to a file name: a Camelot key, an
/// energy level (`Energy 6`), a tempo with its unit (`128 BPM`), or a bare number right next to a
/// Camelot key (`1A - 128 - Title`). A bare number on its own (a track number, `03 - Title`) is
/// part of the name.
fn is_name_tag(parts: &[&str], i: usize) -> bool {
    let Some(seg) = parts.get(i) else { return false };
    let t = seg.trim().to_ascii_lowercase();
    let energy = t.strip_prefix("energy ").is_some_and(is_number);
    let bpm = t.strip_suffix("bpm").is_some_and(is_number);
    let near_key = |j: Option<usize>| j.and_then(|j| parts.get(j)).is_some_and(|x| is_camelot(x));
    let tempo = is_number(&t) && (near_key(i.checked_sub(1)) || near_key(i.checked_add(1)));
    is_camelot(&t) || energy || bpm || tempo
}

/// `stem` without the key/energy/tempo tags Mixed In Key adds at its start or end
/// (`1A - 128 - Title`, `Title - 1A`), lowercased. At least one part of the name is kept.
fn untagged_stem(stem: &str) -> String {
    let parts: Vec<&str> = stem.split(" - ").collect();
    let mut first = 0;
    while first + 1 < parts.len() && is_name_tag(&parts, first) {
        first += 1;
    }
    let mut last = parts.len();
    while last > first + 1 && is_name_tag(&parts, last - 1) {
        last -= 1;
    }
    parts.get(first..last).unwrap_or_default().join(" - ").trim().to_lowercase()
}

fn lower_ext(p: &Path) -> String {
    p.extension().and_then(|x| x.to_str()).unwrap_or_default().to_ascii_lowercase()
}

/// The files under `folder` (subfolders included, links not followed), at most
/// [`RELINK_MAX_FILES`] of them, [`RELINK_MAX_DEPTH`] folders deep.
fn files_under(folder: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() && depth < RELINK_MAX_DEPTH {
                todo.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                out.push(entry.path());
                if out.len() >= RELINK_MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// The audio of `file` when it decodes at `rate` to the length and width `src` recorded.
fn matching_audio(file: &Path, src: &Source, rate: u32) -> Option<Arc<SourceAudio>> {
    // A map per file: a candidate that doesn't match is dropped at once.
    let audio = decode_file(&mut DecodedFiles::new(), file, rate).ok()?;
    (audio.buffer.frames() as u64 == src.frames && audio.buffer.num_channels() == usize::from(src.channels)).then_some(audio)
}

/// Point source `src` at `file`, whose decoded audio is `audio`.
fn relink_to(e: &mut Engine, src: SourceId, file: &Path, audio: Arc<SourceAudio>) {
    let s = e.session_mut();
    s.pool.insert(src, audio);
    if let Some(x) = s.sources.iter_mut().find(|x| x.id == src) {
        x.path = file.to_string_lossy().into_owned();
        x.unsaved = false;
    }
}

/// Relink missing audio. With `file`, source `only` is relinked to that file when it decodes to
/// the length and width the source recorded. Otherwise `dir` and its subfolders are searched for
/// the missing sources (only `only` when given): exact file-name matches first; when none
/// matches, names compared without the key/energy/tempo tags Mixed In Key adds. A source is
/// relinked only when exactly one candidate decodes to its recorded length and width; when
/// several do, it is reported as ambiguous with their paths and left alone (relink it with
/// `file`).
pub fn relink_audio(e: &mut Engine, only: Option<SourceId>, dir: Option<&Path>, file: Option<&Path>) -> Result<Value> {
    const CMD: &str = "file.relink_audio";
    if let Some(id) = only {
        let s = e.session();
        if s.source(id).is_none() {
            return Err(EngineError::BadParams(CMD.into(), format!("no source {}", id.0)));
        }
        if s.pool.get(id).is_some() {
            return Err(EngineError::BadParams(CMD.into(), format!("source {} is not missing", id.0)));
        }
    }
    let rate = e.session().sample_rate.hz();
    if let Some(file) = file {
        let id = only.ok_or_else(|| EngineError::BadParams(CMD.into(), "`file` needs `source`".into()))?;
        crate::file_access::check_read(file).map_err(EngineError::Io)?;
        let src = e.session().source(id).cloned().ok_or_else(|| EngineError::BadParams(CMD.into(), format!("no source {}", id.0)))?;
        let audio = matching_audio(file, &src, rate).ok_or_else(|| {
            EngineError::Io(format!("{}: does not decode to the {} frames, {} channels of `{}`", file.display(), src.frames, src.channels, src.name))
        })?;
        relink_to(e, id, file, audio);
        let done = json!({"source": id.0, "name": src.name, "from": src.path, "to": file});
        return Ok(json!({"relinked": [done], "missing": [], "ambiguous": []}));
    }
    let dir = dir.ok_or_else(|| EngineError::BadParams(CMD.into(), "`dir` or `file` required".into()))?;
    crate::file_access::check_read(dir).map_err(EngineError::Io)?;
    if !dir.is_dir() {
        return Err(EngineError::Io(format!("{}: not a folder", dir.display())));
    }
    let files: Vec<PathBuf> = files_under(dir).into_iter().filter(|f| crate::file_access::check_read(f).is_ok()).collect();
    let mut by_name: std::collections::HashMap<String, Vec<&PathBuf>> = std::collections::HashMap::new();
    let mut by_untagged: std::collections::HashMap<(String, String), Vec<&PathBuf>> = std::collections::HashMap::new();
    for f in &files {
        if let Some(n) = f.file_name().and_then(|n| n.to_str()) {
            by_name.entry(n.to_lowercase()).or_default().push(f);
        }
        if let Some(stem) = f.file_stem().and_then(|n| n.to_str()) {
            by_untagged.entry((untagged_stem(stem), lower_ext(f))).or_default().push(f);
        }
    }
    let wanted: Vec<Source> =
        e.session().sources.iter().filter(|x| only.is_none_or(|id| x.id == id) && e.session().pool.get(x.id).is_none()).cloned().collect();
    let (mut relinked, mut missing, mut ambiguous) = (Vec::new(), Vec::new(), Vec::new());
    for src in wanted {
        // The stored path may use either separator, whatever this computer uses.
        let stored = src.path.replace('\\', "/");
        let name = stored.rsplit('/').next().unwrap_or_default().to_string();
        let stem = Path::new(&name).file_stem().and_then(|n| n.to_str()).unwrap_or_default();
        let exact = by_name.get(&name.to_lowercase()).cloned().unwrap_or_default();
        let tagged: Vec<&PathBuf> = by_untagged
            .get(&(untagged_stem(stem), lower_ext(Path::new(&name))))
            .into_iter()
            .flatten()
            .copied()
            .filter(|f| !exact.contains(f))
            .collect();
        // The candidates of one tier that match; only the first one's audio is kept.
        let matches = |cands: &[&PathBuf]| {
            let mut first = None;
            let mut paths = Vec::new();
            for c in cands {
                if let Some(audio) = matching_audio(c, &src, rate) {
                    first.get_or_insert(audio);
                    paths.push((*c).clone());
                }
            }
            (first, paths)
        };
        let (mut audio, mut paths) = matches(&exact);
        if paths.is_empty() {
            (audio, paths) = matches(&tagged);
        }
        match (audio, paths.as_slice()) {
            (Some(audio), [file]) => {
                relink_to(e, src.id, file, audio);
                relinked.push(json!({"source": src.id.0, "name": src.name, "from": src.path, "to": file}));
            }
            (_, []) => missing.push(json!({"source": src.id.0, "name": src.name, "path": src.path})),
            _ => ambiguous.push(json!({"source": src.id.0, "name": src.name, "path": src.path, "candidates": paths})),
        }
    }
    Ok(json!({"relinked": relinked, "missing": missing, "ambiguous": ambiguous, "files_searched": files.len()}))
}

// ─── Hosts without a filesystem (the browser) ─────────────────────────────────────────────────

/// Extension of a session bundle: one zip file holding the `.scraft` session and its
/// `Audio Files/`, the same layout as a saved session folder. The browser has no folders, so it
/// saves and opens sessions as bundles.
pub const BUNDLE_EXTENSION: &str = "scraftz";

/// Most entries and bytes a bundle may unpack to (bundles are hostile input).
const MAX_BUNDLE_ENTRIES: usize = 10_000;
const MAX_BUNDLE_BYTES: u64 = 1 << 30;

/// Hands a finished file (name, bytes) to the person on a host without a filesystem: the web app
/// installs one that starts a download. Native builds write files and never install it.
pub type DeliverHook = fn(&str, &[u8]) -> std::result::Result<(), String>;

static DELIVER: std::sync::OnceLock<DeliverHook> = std::sync::OnceLock::new();

/// Install the delivery hook (once, at start-up of the web app).
pub fn set_deliver(h: DeliverHook) {
    let _ = DELIVER.set(h);
}

/// Write one output file: to `path` natively; in the browser, delivered under its file name.
/// Without this, every export on the web failed with «operation not supported on this platform».
/// [`write_file`] for a path the person chose while session `s` is open: refused when it is
/// the audio file of one of its sources ([`refuse_source_file`]).
pub fn write_user_file(s: &Session, path: &str, bytes: &[u8]) -> Result<()> {
    refuse_source_file(s, Path::new(path))?;
    write_file(path, bytes)
}

pub fn write_file(path: &str, bytes: &[u8]) -> Result<()> {
    if cfg!(target_arch = "wasm32") {
        let deliver = DELIVER.get().ok_or_else(|| EngineError::Io("saving files is not available in this build".into()))?;
        let name = Path::new(path).file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).unwrap_or("Untitled");
        return deliver(name, bytes).map_err(EngineError::Io);
    }
    crate::file_access::check_write(path).map_err(EngineError::Io)?;
    std::fs::write(path, bytes).map_err(|err| EngineError::Io(format!("{path}: {err}")))
}

/// The session as a bundle: `<name>.scraft` plus every source's audio as 32-bit float WAV under
/// `Audio Files/` (stored, not compressed: audio barely compresses). Returns the bytes and the
/// session with its source paths pointing into the bundle.
pub fn session_bundle(session: &Session) -> Result<(Vec<u8>, Session)> {
    use std::io::Write as _;
    let zerr = |err: zip::result::ZipError| EngineError::Io(format!("bundle: {err}"));
    let ioerr = |err: std::io::Error| EngineError::Io(format!("bundle: {err}"));
    let mut s = session.clone();
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
    let mut taken = std::collections::HashSet::new();
    for src in &mut s.sources {
        let Some(audio) = s.pool.get(src.id) else { continue };
        let base = sanitize_name(&src.name);
        let mut target = format!("Audio Files/{base}.wav");
        let mut k = 1;
        while taken.contains(&target) && k <= 9_999 {
            target = format!("Audio Files/{base}_{k:02}.wav");
            k += 1;
        }
        let opts_wav = EncodeOptions { format: FileFormat::Wav, bit_depth: soundcraft_audio_io::BitDepth::Float32, dither: false, bwf: None };
        let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts_wav).map_err(|err| EngineError::Io(err.to_string()))?;
        w.start_file(target.as_str(), opts).map_err(zerr)?;
        w.write_all(&bytes).map_err(ioerr)?;
        taken.insert(target.clone());
        src.path = target;
        src.unsaved = false;
        src.format = FileFormat::Wav;
    }
    let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
    w.start_file(format!("{}.{}", sanitize_name(&s.name), soundcraft_model::SESSION_EXTENSION), opts).map_err(zerr)?;
    w.write_all(text.as_bytes()).map_err(ioerr)?;
    let bytes = w.finish().map_err(zerr)?.into_inner();
    Ok((bytes, s))
}

/// Save as a bundle named after `path`'s file name, delivered through the host ([`write_file`]).
fn save_session_bundle(e: &mut Engine, path: &str) -> Result<usize> {
    let stem = Path::new(path).file_stem().and_then(|x| x.to_str()).filter(|x| !x.is_empty()).map(str::to_string);
    let mut s = e.session().clone();
    if let Some(stem) = stem {
        s.name = stem;
    }
    let (bytes, fresh) = session_bundle(&s)?;
    let file = format!("{}.{BUNDLE_EXTENSION}", sanitize_name(&fresh.name));
    write_file(&file, &bytes)?;
    let written = fresh.sources.iter().filter(|x| fresh.pool.get(x.id).is_some()).count();
    *e.session_mut() = fresh;
    e.path = Some(file);
    e.mark_clean();
    Ok(written)
}

/// Open a session from bytes (the browser has no paths): a bundle ([`session_bundle`]) or a bare
/// `.scraft`, whose audio is then reported missing. Returns the missing media, like
/// [`open_session`].
pub fn open_session_bytes(e: &mut Engine, name: &str, bytes: &[u8]) -> Result<Vec<String>> {
    let (text, mut files) = if bytes.starts_with(b"PK\x03\x04") {
        read_bundle(bytes)?
    } else {
        (
            String::from_utf8(bytes.to_vec()).map_err(|_| EngineError::Io(format!("{name}: not a SoundCraft session")))?,
            std::collections::HashMap::new(),
        )
    };
    let mut s = Session::from_json(&text).map_err(|err| EngineError::Io(format!("{name}: {err}")))?;
    let mut missing = Vec::new();
    for src in &s.sources {
        match files.remove(&src.path) {
            Some(audio) => match decode_source(&audio, Path::new(&src.path).extension().and_then(|x| x.to_str()), s.sample_rate.hz()) {
                Ok(audio) => s.pool.insert(src.id, audio),
                Err(err) => missing.push(format!("{}: {err}", src.path)),
            },
            None => missing.push(src.path.clone()),
        }
    }
    // Movies are referenced by path, never copied into a session: none can be found here.
    missing.extend(s.videos.iter().map(|v| v.path.clone()));
    let stem = Path::new(name).file_stem().and_then(|x| x.to_str()).filter(|x| !x.is_empty()).unwrap_or("Untitled").to_string();
    e.replace_session(s);
    e.path = Some(format!("{}.{BUNDLE_EXTENSION}", sanitize_name(&stem)));
    for m in &missing {
        e.message(format!("missing media: {m}"));
    }
    Ok(missing)
}

/// Unpack a bundle: the one `.scraft` at its top level and the files beside it, by relative path.
fn read_bundle(bytes: &[u8]) -> Result<(String, std::collections::HashMap<String, Vec<u8>>)> {
    use std::io::Read as _;
    let zerr = |err: zip::result::ZipError| EngineError::Io(format!("bundle: {err}"));
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(zerr)?;
    if z.len() > MAX_BUNDLE_ENTRIES {
        return Err(EngineError::Io("bundle: too many files".into()));
    }
    let (mut text, mut files, mut total) = (None, std::collections::HashMap::new(), 0u64);
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(zerr)?;
        if f.is_dir() {
            continue;
        }
        let rel = f.name().map_err(zerr)?.to_string();
        let size = f.size();
        total = total.saturating_add(size);
        if total > MAX_BUNDLE_BYTES {
            return Err(EngineError::Io("bundle: too large".into()));
        }
        let mut data = Vec::new();
        // The declared size is hostile too: never read past it.
        f.by_ref().take(size).read_to_end(&mut data).map_err(|err| EngineError::Io(format!("bundle: {rel}: {err}")))?;
        let is_session = !rel.contains('/') && Path::new(&rel).extension().and_then(|x| x.to_str()) == Some(soundcraft_model::SESSION_EXTENSION);
        if is_session && text.is_none() {
            text = Some(String::from_utf8(data).map_err(|_| EngineError::Io(format!("bundle: {rel} is not text")))?);
        } else {
            files.insert(rel, data);
        }
    }
    let text = text.ok_or_else(|| EngineError::Io("bundle: no .scraft session inside".into()))?;
    Ok((text, files))
}

/// The record-enabled audio tracks, each with the input channels it records (those its input
/// route names, "In 3" → 2, wrapped to the device's `input_channels`) and a new take file
/// `<track>_<NN>.wav` in the `Audio Files` of the session's folder (`unsaved_dir` until it is saved).
pub fn plan_takes(e: &Engine, unsaved_dir: &Path, input_channels: usize) -> Vec<(TrackId, Vec<usize>, PathBuf)> {
    let dir = e.path.as_deref().and_then(|p| Path::new(p).parent()).unwrap_or(unsaved_dir);
    let s = e.session();
    let take_no = s.sources.len() + 1;
    s.tracks
        .iter()
        .filter(|t| t.mixer.record_arm && t.kind == TrackKind::Audio && !t.inactive)
        .map(|t| {
            let first = match &t.mixer.input {
                soundcraft_model::Route::Hardware(h) => {
                    h.trim_start_matches("In ").split(['-', ' ']).next().and_then(|n| n.parse::<usize>().ok()).map_or(0, |n| n.saturating_sub(1))
                }
                _ => 0,
            };
            let inputs = (0..t.channels()).map(|k| first.saturating_add(k) % input_channels.max(1)).collect();
            let file = |n: usize| dir.join("Audio Files").join(format!("{}_{n:02}.wav", sanitize_name(&t.name)));
            let path = (take_no..take_no + 1_000).map(file).find(|p| !p.exists()).unwrap_or_else(|| file(take_no));
            (t.id, inputs, path)
        })
        .collect()
}

/// A recording's plan, made when it starts: each record-enabled track's take file, where the
/// takes start on the timeline, the loop length they split into when loop recording, and the
/// punch range they're trimmed to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Recording {
    pub start: Samples,
    pub pass: Option<Samples>,
    pub punch: Option<Range>,
    /// The input's sample rate, which the take files are written at.
    pub rate: u32,
    pub takes: Vec<Take>,
}

/// One track's take file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Take {
    pub track: TrackId,
    pub path: PathBuf,
    pub channels: usize,
}

/// Place recorded take files as `plan` says, as one undo step ("Record").
pub fn place_recording(e: &mut Engine, plan: &Recording, takes: Vec<(TrackId, PathBuf)>) -> Vec<soundcraft_model::ClipId> {
    let ids = add_recording(e, plan.start, takes, plan.pass);
    if let Some(p) = plan.punch {
        let clips: Vec<u64> = ids.iter().map(|c| c.0).collect();
        let _ = e.execute_merged("edit.trim_to_fill_selection", &json!({"clips": clips, "start": p.start, "end": p.end}), "record");
    }
    ids
}

/// Turn recorded take files into clips at `start`, as one undo step ("Record"); each file is the
/// source of its clips. With `pass` (loop recording), a take becomes one clip per pass (at most
/// 1000), each pass after the first on a new playlist.
pub fn add_recording(e: &mut Engine, start: Samples, takes: Vec<(TrackId, PathBuf)>, pass: Option<Samples>) -> Vec<soundcraft_model::ClipId> {
    let before = e.session_arc();
    let mut out = Vec::new();
    for (tid, path) in takes {
        let read =
            read_file(&path).and_then(|b| soundcraft_audio_io::decode(&b, None).map_err(|err| EngineError::Io(format!("{}: {err}", path.display()))));
        let Ok((_, buf)) = read.inspect_err(|err| e.message(err.to_string())) else { continue };
        let s = e.session_mut();
        if s.track(tid).is_none() {
            continue;
        }
        let resampled = buf.sample_rate != s.sample_rate.hz();
        let name = path.file_stem().and_then(|x| x.to_str()).unwrap_or("Take");
        let src = add_source(s, name, buf, Some(&path.to_string_lossy()), FileFormat::Wav);
        let Some(x) = s.sources.last_mut() else { continue };
        x.unsaved = resampled;
        let len = i64::try_from(x.frames).unwrap_or(0);
        let pass = pass.unwrap_or(len).max(1);
        for offset in (0..len).step_by(usize::try_from(pass).unwrap_or(usize::MAX)).take(1_000) {
            let n = pass.min(len - offset);
            if offset > 0 && n < pass / 8 {
                break;
            }
            if offset > 0
                && let Some(t) = s.track_mut(tid)
            {
                t.playlists.push(soundcraft_model::Playlist::new(format!("{}.{:02}", t.name, t.playlists.len() + 1)));
                t.active_playlist = t.playlists.len() - 1;
            }
            let cid = s.new_clip_id();
            crate::edit::place_clip(s, tid, Clip::audio(cid, name, src, offset, start, n));
            out.push(cid);
        }
    }
    if !out.is_empty() {
        e.push_undo("Record", before);
    }
    out
}

/// Where a take recording from `start` shows while it holds `len` samples, as it will be placed:
/// just the pass under way when loop recording in passes of `pass`, within the `punch` range.
/// Returns the timeline position of its first sample and the span it covers.
pub fn live_span(start: Samples, pass: Option<Samples>, punch: Option<Range>, len: Samples) -> Option<(Samples, Range)> {
    let origin = start.saturating_sub(pass.filter(|p| *p > 0).map_or(0, |p| len / p * p));
    let span = Range::new(start, origin.saturating_add(len));
    Some((origin, span.intersect(&punch.unwrap_or(span))?))
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use serde_json::json;

    fn armed(e: &mut Engine, name: &str, format: &str, input: &str) -> TrackId {
        let id = e.execute("track.new", &json!({"name": name, "format": format})).unwrap()["tracks"][0].as_u64().unwrap();
        e.execute("track.input", &json!({"tracks": [id], "input": input})).unwrap();
        e.execute("mix.record_arm", &json!({"tracks": [id], "value": true})).unwrap();
        TrackId(id)
    }

    fn write_take(path: &Path, frames: usize) {
        let mut bytes = soundcraft_audio_io::wav_float_header(1, 48_000, frames as u64).unwrap();
        bytes.extend((0..frames).flat_map(|i| (i as f32 / frames as f32 - 0.5).to_le_bytes()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn armed_tracks_get_new_take_files_used_in_place() {
        let dir = std::env::temp_dir().join(format!("soundcraft-takes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = Engine::default();
        let vox = armed(&mut e, "Vox", "mono", "In 2");
        let gtr = armed(&mut e, "Gtr", "stereo", "In 3-4");
        write_take(&dir.join("Audio Files/Vox_01.wav"), 10);
        let plan = plan_takes(&e, &dir, 4);
        assert_eq!(plan, vec![(vox, vec![1], dir.join("Audio Files/Vox_02.wav")), (gtr, vec![2, 3], dir.join("Audio Files/Gtr_01.wav"))]);
        assert_eq!(plan_takes(&e, &dir, 2)[1].1, vec![0, 1]);

        let session = dir.join("Song.scraft").to_string_lossy().into_owned();
        e.execute("session.save_as", &json!({"path": session})).unwrap();
        write_take(&plan[0].2, 4_800);
        let clips = add_recording(&mut e, 96_000, vec![(vox, plan[0].2.clone())], None);
        let clip = e.session().find_clip(clips[0]).unwrap().1.clone();
        assert_eq!((clip.start, clip.length), (96_000, 4_800));
        assert_eq!(save_session(&mut e, &session).unwrap(), 0);
        let src = e.session().sources.last().unwrap().clone();
        assert_eq!((src.name.as_str(), src.path.as_str(), src.unsaved, src.frames), ("Vox_02", "Audio Files/Vox_02.wav", false, 4_800));
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(e.session().find_clip(clips[0]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_loop_take_becomes_one_clip_per_pass_and_autosaves_in_place() {
        let dir = std::env::temp_dir().join(format!("soundcraft-loop-take-{}", std::process::id()));
        let path = dir.join("Audio Files/Vox_01.wav");
        write_take(&path, 1_050);
        let mut e = Engine::default();
        let vox = armed(&mut e, "Vox", "mono", "In 1");
        assert_eq!(add_recording(&mut e, 1_000, vec![(vox, path.clone())], Some(400)).len(), 3);
        assert_eq!(save_copy(&e, &dir.join("Untitled.scraft").to_string_lossy()).unwrap(), 0);
        let t = e.session().track(vox).unwrap();
        let passes: Vec<_> = t.playlists.iter().flat_map(|p| p.clips.iter().map(|c| (c.source_offset(), c.start, c.length))).collect();
        assert_eq!(passes, vec![(0, 1_000, 400), (400, 1_000, 400), (800, 1_000, 250)]);
        assert_eq!((t.active_playlist, t.playlists[2].name.as_str()), (2, "Vox.03"));
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(e.session().track(vox).unwrap().playlists.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_shows_the_pass_under_way_within_the_punch_while_it_records() {
        assert_eq!(live_span(1_000, None, None, 0), None);
        assert_eq!(live_span(1_000, None, None, 500), Some((1_000, Range::new(1_000, 1_500))));
        assert_eq!(live_span(1_000, Some(400), None, 900), Some((200, Range::new(1_000, 1_100))));
        assert_eq!(live_span(1_000, Some(400), None, 800), None);
        let punch = Some(Range::new(1_200, 1_400));
        assert_eq!(live_span(1_000, None, punch, 150), None);
        assert_eq!(live_span(1_000, None, punch, 500), Some((1_000, Range::new(1_200, 1_400))));
    }
}

#[cfg(test)]
mod save_tests {
    use super::*;

    #[test]
    fn autosave_copy_leaves_document_untouched_and_reuses_audio() {
        let mut e = crate::demo::demo_engine();
        e.execute("mix.volume", &serde_json::json!({"track": "Kick", "db": -7.0})).unwrap();
        let dir = std::env::temp_dir().join(format!("soundcraft-autosave-{}", std::process::id()));
        let path = dir.join("Midnight Groove.scraft");
        let p = path.to_string_lossy().into_owned();
        let n1 = save_copy(&e, &p).unwrap();
        let n2 = save_copy(&e, &p).unwrap();
        assert!(n1 >= 5);
        assert_eq!(n2, 0, "second autosave must not rewrite audio");
        assert!(e.is_dirty() && e.path.is_none());
        let mut r = Engine::default();
        open_session(&mut r, &p).unwrap();
        assert_eq!(r.session().track_by_name("Kick").unwrap().mixer.volume_db, -7.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn with_take(level: f32) -> Engine {
        let mut e = Engine::default();
        let s = e.session_mut();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let src = add_source(s, "Audio 1_02", AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; 4_800]] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::audio(id, "Audio 1_02", src, 0, 0, 4_800));
        e
    }

    fn reopen(path: &str) -> Engine {
        let mut r = Engine::default();
        assert_eq!(open_session(&mut r, path).unwrap(), Vec::<String>::new());
        r
    }

    #[test]
    fn saving_never_replaces_audio_of_another_session() {
        let dir = std::env::temp_dir().join(format!("soundcraft-no-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
        save_session(&mut with_take(0.5), &path("A/A.scraft")).unwrap();
        save_session(&mut with_take(-0.25), &path("A/A v2.scraft")).unwrap();
        save_session(&mut with_take(0.75), &path("B/B.scraft")).unwrap();
        save_session(&mut reopen(&path("B/B.scraft")), &path("A/B.scraft")).unwrap();
        let levels = ["A/A.scraft", "A/A v2.scraft", "B/B.scraft", "A/B.scraft"].map(|p| {
            let r = reopen(&path(p));
            r.session().pool.get(r.session().sources[0].id).unwrap().buffer.channels[0][0]
        });
        assert_eq!(levels, [0.5, -0.25, 0.75, 0.75]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consolidating_twice_keeps_both_renders() {
        let dir = std::env::temp_dir().join(format!("soundcraft-consolidate-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let mut e = crate::demo::demo_engine();
        let kick = e.session().track_by_name("Kick").unwrap().id;
        let end = e.session().content_end();
        let render = |e: &Engine| soundcraft_mix::render_clips(e.session(), kick, Range::new(0, end));
        e.execute("edit.consolidate", &json!({"tracks": [kick.0], "start": 500_000, "end": 560_000})).unwrap();
        save_session(&mut e, &path).unwrap();
        e.execute("edit.consolidate", &json!({"tracks": [kick.0], "start": 1_400_000, "end": 1_500_000})).unwrap();
        save_session(&mut e, &path).unwrap();
        assert!(render(&reopen(&path)) == render(&e));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_after_undo_reuses_the_saved_audio() {
        let dir = std::env::temp_dir().join(format!("soundcraft-undo-resave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let mut e = with_take(0.5);
        e.execute("track.new", &json!({})).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        assert_eq!(std::fs::read_dir(dir.join("Audio Files")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_audio_is_trusted_only_in_the_folder_it_was_saved_in() {
        let dir = std::env::temp_dir().join(format!("soundcraft-own-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
        let level = |r: &Engine| r.session().pool.get(r.session().sources[0].id).unwrap().buffer.channels[0][0];
        save_session(&mut with_take(-0.25), &path("B/Other.scraft")).unwrap();
        let mut e = with_take(0.5);
        save_session(&mut e, &path("A/A.scraft")).unwrap();
        e.execute("track.new", &json!({})).unwrap();
        save_session(&mut e, &path("B/B.scraft")).unwrap();
        e.execute("edit.undo", &json!({})).unwrap();
        e.execute("session.save", &json!({})).unwrap();
        assert_eq!(level(&reopen(&path("B/B.scraft"))), 0.5);
        e.execute("session.save_copy", &json!({"path": path("A/Copy.scraft")})).unwrap();
        e.execute("session.save", &json!({})).unwrap();
        assert_eq!(level(&reopen(&path("B/B.scraft"))), 0.5);
        std::fs::remove_file(dir.join("A/Audio Files/Audio 1_02.wav")).unwrap();
        let mut missing = Engine::default();
        open_session(&mut missing, &path("A/A.scraft")).unwrap();
        save_session(&mut missing, &path("B/Moved.scraft")).unwrap();
        let mut r = Engine::default();
        assert_eq!(
            open_session(&mut r, &path("B/Moved.scraft")).unwrap(),
            vec![dir.join("A").join("Audio Files/Audio 1_02.wav").display().to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_over_another_session_needs_replace() {
        let dir = std::env::temp_dir().join(format!("soundcraft-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        let mut first = with_take(0.5);
        first.execute("session.save_as", &json!({"path": path})).unwrap();
        first.execute("session.save", &json!({})).unwrap();
        first.execute("session.save_as", &json!({"path": dir.join("Song")})).unwrap();
        assert!(first.execute("session.save_template", &json!({"path": path})).is_err());
        let mut second = with_take(-0.25);
        let track = second.session().tracks[0].id.0;
        for id in ["session.save", "session.save_as", "session.save_copy", "session.save_template", "file.export_selected_tracks_as_session"] {
            let err = second.execute(id, &json!({"path": path, "tracks": [track]})).unwrap_err();
            assert!(err.to_string().contains("`replace: true`"), "{id}: {err}");
        }
        assert_eq!(std::fs::read_dir(dir.join("Audio Files")).unwrap().count(), 1);
        Engine::default().execute("session.save_as", &json!({"path": dir.join("New/New.scraft")})).unwrap();
        std::fs::copy(&path, dir.join("Song")).unwrap();
        let mut bare = Engine::default();
        open_session(&mut bare, &dir.join("Song").to_string_lossy()).unwrap();
        bare.execute("session.save", &json!({})).unwrap();
        second.execute("session.save_as", &json!({"path": path, "replace": true})).unwrap();
        let r = reopen(&path);
        assert_eq!(r.session().pool.get(r.session().sources[0].id).unwrap().buffer.channels[0][0], -0.25);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_keeps_the_last_ten_versions_as_backups() {
        let dir = std::env::temp_dir().join(format!("soundcraft-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Song.scraft");
        let mut e = Engine::default();
        let mut versions = Vec::new();
        for i in 0..12 {
            e.execute("track.new", &json!({"name": format!("T{i}")})).unwrap();
            save_session(&mut e, &path.to_string_lossy()).unwrap();
            save_session(&mut e, &path.to_string_lossy()).unwrap();
            versions.push(std::fs::read(&path).unwrap());
        }
        let backup = |n: usize| std::fs::read(dir.join(format!("Backups/Song backup {n}.scraft"))).ok();
        assert_eq!(std::fs::read_dir(dir.join("Backups")).unwrap().count(), 10);
        assert_eq!((backup(1), backup(2), backup(11)), (None, Some(versions[1].clone()), Some(versions[10].clone())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_gives_sources_that_share_a_path_their_own_files() {
        let mut s = Session::default();
        let loud = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 64]] };
        let quiet = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 64]] };
        add_source(&mut s, "tone", loud, Some("Audio Files/tone.wav"), FileFormat::Wav);
        add_source(&mut s, "tone", quiet, Some("Audio Files/tone.wav"), FileFormat::Wav);
        let mut e = Engine::new(s);
        let dir = std::env::temp_dir().join(format!("soundcraft-shared-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Shared.scraft");
        save_session(&mut e, &path.to_string_lossy()).unwrap();
        let paths: Vec<_> = e.session().sources.iter().map(|src| src.path.clone()).collect();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1], "{paths:?}");
        let a = std::fs::read(dir.join(&paths[0])).unwrap();
        let b = std::fs::read(dir.join(&paths[1])).unwrap();
        assert_ne!(a, b);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn a_tiny_sample_rate_is_refused_before_it_is_converted() {
        let buf = |rate| AudioBuffer { sample_rate: rate, channels: vec![vec![0.5; 100]; 2] };
        assert!(at_rate(buf(1), 48_000, 1_000_000).is_none());
        let ok = at_rate(buf(8_000), 48_000, 1_000_000).unwrap();
        assert_eq!((ok.sample_rate, ok.frames()), (48_000, 600));
    }
}

#[cfg(test)]
mod open_tests {
    use super::*;

    #[test]
    fn a_file_named_by_many_sources_is_decoded_once() {
        let dir = std::env::temp_dir().join(format!("soundcraft-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 480]] };
        std::fs::write(dir.join("k.wav"), soundcraft_audio_io::encode(&audio, &EncodeOptions::default()).unwrap()).unwrap();
        let mut s = Session::default();
        for _ in 0..3 {
            add_source(&mut s, "k", audio.clone(), Some("k.wav"), FileFormat::Wav);
        }
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        std::fs::write(&path, s.to_json().unwrap()).unwrap();
        let mut e = Engine::default();
        assert!(open_session(&mut e, &path).unwrap().is_empty());
        let (sources, pool) = (&e.session().sources, &e.session().pool);
        let first = pool.get(sources[0].id).unwrap();
        assert!(sources.iter().all(|src| pool.get(src.id).is_some_and(|a| Arc::ptr_eq(a, first))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sources_playing_one_file_share_it_when_saved() {
        let dir = std::env::temp_dir().join(format!("soundcraft-shared-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Audio Files")).unwrap();
        let audio = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 480]] };
        std::fs::write(dir.join("Audio Files/k.wav"), soundcraft_audio_io::encode(&audio, &EncodeOptions::default()).unwrap()).unwrap();
        let mut s = Session::default();
        for _ in 0..3 {
            add_source(&mut s, "k", audio.clone(), Some("Audio Files/k.wav"), FileFormat::Wav);
        }
        s.sources.iter_mut().for_each(|src| src.unsaved = false);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        std::fs::write(&path, s.to_json().unwrap()).unwrap();
        let mut e = Engine::default();
        open_session(&mut e, &path).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        assert!(e.session().sources.iter().all(|src| src.path == "Audio Files/k.wav"));
        assert_eq!(save_copy(&e, &dir.join("copy/Song.scraft").to_string_lossy()).unwrap(), 1);
        assert_eq!(std::fs::read_dir(dir.join("copy/Audio Files")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn stored_media_paths_are_followed_only_on_this_computer() {
        let dir = std::env::temp_dir().join(format!("soundcraft-stored-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let audio = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 480]] };
        std::fs::write(dir.join("k.wav"), soundcraft_audio_io::encode(&audio, &EncodeOptions::default()).unwrap()).unwrap();
        let mut saved = Session::default();
        let t = saved.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let src = add_source(&mut saved, "k", audio, Some("../k.wav"), FileFormat::Wav);
        let id = saved.new_clip_id();
        crate::edit::place_clip(&mut saved, t, Clip::audio(id, "k", src, 0, 0, 480));
        // In a folder of its own, so a movie named `k.wav` is not relinked to a file beside it.
        std::fs::create_dir_all(dir.join("s")).unwrap();
        let path = dir.join("s").join("Song.scraft").to_string_lossy().into_owned();
        let mut stored = vec![r"\\192.0.2.1\share\x.wav".to_string(), "file://192.0.2.1/s/x.wav".to_string()];
        // Linux reads `//tmp/…` as the local `/tmp/…`, so only the path check keeps it from being read.
        if cfg!(unix) {
            stored.push(format!("/{}", dir.join("k.wav").display()));
        }
        let refused = |m: &str| m.contains("not opened: the path is on another computer");
        for p in stored {
            let mut s = saved.clone();
            s.sources[0].path.clone_from(&p);
            let id = SourceId(s.alloc());
            s.videos.push(soundcraft_model::VideoSource {
                id,
                name: "x".into(),
                path: p.clone(),
                width: 0,
                height: 0,
                frame_rate: 0.0,
                duration: 0.0,
                codec: String::new(),
                offline: None,
            });
            std::fs::write(&path, s.to_json().unwrap()).unwrap();
            let mut r = Engine::default();
            let missing = open_session(&mut r, &path).unwrap();
            assert_eq!(missing.iter().filter(|m| refused(m)).count(), 2, "{p}: {missing:?}");
            assert!(r.session().pool.get(r.session().sources[0].id).is_none(), "{p}");
            assert!(r.session().videos[0].offline.is_some(), "{p}");
            let mut into = Engine::default();
            let imported = into.execute("file.import_session_data", &json!({"path": path})).unwrap();
            assert!(imported["missing"].as_array().unwrap().iter().any(|m| m.as_str().is_some_and(refused)), "{p}: {imported}");
            assert!(into.session().pool.get(into.session().sources[0].id).is_none(), "{p}");
        }
        // A path relative to the session opens.
        std::fs::write(&path, saved.to_json().unwrap()).unwrap();
        let mut r = Engine::default();
        assert!(open_session(&mut r, &path).unwrap().is_empty());
        assert!(r.session().pool.get(r.session().sources[0].id).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn a_midi_track_name_is_cut_to_a_length_that_can_be_drawn() {
        let note = soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 960 };
        let sequence = soundcraft_midi::Sequence { notes: vec![note], ..Default::default() };
        let smf = soundcraft_midi::Smf { tracks: vec![soundcraft_midi::SmfTrack { name: "n".repeat(1_000_000), sequence }], ..Default::default() };
        let mut e = Engine::default();
        let ids = import_midi_bytes(&mut e, &soundcraft_midi::write_smf(&smf).unwrap(), 0, false).unwrap();
        let track = e.session().track(ids[0]).unwrap();
        assert_eq!((track.name.len(), track.clips()[0].name.len()), (soundcraft_model::MAX_NAME_CHARS, soundcraft_model::MAX_NAME_CHARS));
    }

    #[test]
    fn midi_longer_than_a_day_is_refused() {
        let held = soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: (1 << 28) - 1 };
        let sequence = soundcraft_midi::Sequence { notes: vec![held], ..Default::default() };
        let smf = soundcraft_midi::Smf { tracks: vec![soundcraft_midi::SmfTrack { name: String::new(), sequence }], ..Default::default() };
        let err = import_midi_bytes(&mut Engine::default(), &soundcraft_midi::write_smf(&smf).unwrap(), 0, false).unwrap_err();
        assert!(err.to_string().contains("longer than 24 hours"), "{err}");
    }

    #[test]
    fn midi_with_more_meter_changes_than_scores_have_is_refused() {
        let note = soundcraft_midi::Note { pitch: 60, velocity: 100, release_velocity: 64, channel: 0, start: 0, length: 960 };
        let sequence = soundcraft_midi::Sequence { notes: vec![note], ..Default::default() };
        let meters = (0..=MAX_MIDI_METERS as i64).map(|bar| (bar * 3840, 4, 4)).collect();
        let smf = soundcraft_midi::Smf { tracks: vec![soundcraft_midi::SmfTrack { name: String::new(), sequence }], meters, ..Default::default() };
        let err = import_midi_bytes(&mut Engine::default(), &soundcraft_midi::write_smf(&smf).unwrap(), 0, true).unwrap_err();
        assert!(err.to_string().contains("meter changes"), "{err}");
    }

    #[test]
    fn new_sources_take_the_first_free_file_name() {
        let mut s = Session::default();
        let buf = || AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 4]] };
        add_source(&mut s, "k", buf(), Some("Audio Files/k-02.wav"), FileFormat::Wav);
        let mut path = || {
            let id = add_source(&mut s, "k", buf(), None, FileFormat::Wav);
            s.source(id).map(|x| x.path.clone()).unwrap_or_default()
        };
        assert_eq!([path(), path(), path()], ["Audio Files/k.wav", "Audio Files/k-2.wav", "Audio Files/k-3.wav"]);
    }
}

#[cfg(all(test, unix))]
mod pipe_tests {
    use super::*;

    #[test]
    fn a_pipe_named_as_media_is_refused_not_waited_on() {
        let dir = std::env::temp_dir().join(format!("soundcraft-pipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("pipe.wav");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let mut s = Session::default();
        let audio = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 10]] };
        add_source(&mut s, "pipe", audio, Some(&fifo.to_string_lossy()), FileFormat::Wav);
        let path = dir.join("Song.scraft").to_string_lossy().into_owned();
        std::fs::write(&path, s.to_json().unwrap()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let pipe = fifo.to_string_lossy().into_owned();
        std::thread::spawn(move || {
            let mut e = Engine::default();
            let imported = e.execute("file.import_audio", &json!({"path": pipe})).is_ok();
            let _ = tx.send((imported, open_session(&mut e, &path).map_err(|err| err.to_string())));
        });
        let (imported, opened) = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(!imported);
        assert_eq!(opened.unwrap(), [fifo.display().to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod stem_tests {
    #[test]
    fn stems_one_file_per_track() {
        let mut e = crate::demo::demo_engine();
        let dir = std::env::temp_dir().join(format!("soundcraft-stems-{}", std::process::id()));
        let r = e
            .execute("file.bounce_stems", &serde_json::json!({"dir": dir.to_string_lossy(), "start": {"seconds": 10.0}, "end": {"seconds": 11.0}}))
            .unwrap();
        let n = r["files"].as_array().map_or(0, Vec::len);
        assert_eq!(n, 7, "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod strip_tests {
    use super::*;

    /// A stereo track with `length`-long clips at `starts`, each playing a second of steady tone.
    fn tones(starts: &[Samples], length: Samples) -> (Engine, TrackId) {
        let mut e = Engine::default();
        e.execute("track.new", &json!({"format": "stereo"})).unwrap();
        let t = e.session().tracks[0].id;
        let s = e.session_mut();
        let tone = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 48_000]; 2] };
        let src = add_source(s, "tone", tone, None, FileFormat::Wav);
        for at in starts {
            let id = s.new_clip_id();
            crate::edit::place_clip(s, t, Clip::audio(id, "tone", src, 0, *at, length));
        }
        (e, t)
    }

    #[test]
    fn clips_past_what_one_render_holds_are_kept() {
        // 100 minutes in: a stereo render stops at 2^28 frames, about 93 minutes.
        let far = 48_000 * 6000;
        let (mut e, t) = tones(&[0, far], 48_000);
        let r = e.execute("edit.strip_silence", &json!({"track": t.0, "start": 0, "end": far + 96_000})).unwrap();
        assert_eq!(r["clips"], 2);
        assert!(e.session().track(t).unwrap().clips().iter().any(|c| c.start >= far));
    }

    #[test]
    fn more_than_a_day_of_clips_is_refused() {
        let (mut e, t) = tones(&[0], 48_000 * 86_401);
        assert!(e.execute("edit.strip_silence", &json!({"track": t.0, "start": 0, "end": 48_000i64 * 86_402})).is_err());
        assert_eq!(e.session().track(t).unwrap().clips().len(), 1);
    }
}

#[cfg(test)]
mod bundle_tests {
    use super::*;

    #[test]
    fn bundle_round_trips_session_and_audio() {
        let mut e = crate::demo::demo_engine();
        e.execute("mix.volume", &serde_json::json!({"track": "Kick", "db": -7.0})).unwrap();
        let (bytes, fresh) = session_bundle(e.session()).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        assert!(fresh.sources.iter().all(|s| s.path.starts_with("Audio Files/") && !s.unsaved));
        let mut r = Engine::default();
        let missing = open_session_bytes(&mut r, "Midnight Groove.scraftz", &bytes).unwrap();
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(r.session().track_by_name("Kick").unwrap().mixer.volume_db, -7.0);
        assert_eq!(r.session().tracks.len(), e.session().tracks.len());
        for src in &e.session().sources {
            let (a, b) = (e.session().pool.get(src.id).unwrap(), r.session().pool.get(src.id).unwrap());
            assert_eq!(a.buffer.channels, b.buffer.channels, "{}: float WAV keeps every sample", src.name);
        }
        assert!(!r.is_dirty());
        assert_eq!(r.path.as_deref(), Some("Midnight Groove.scraftz"));
    }

    #[test]
    fn same_named_sources_get_distinct_files() {
        let mut e = crate::demo::demo_engine();
        let name = e.session().sources[0].name.clone();
        let s = e.session_mut();
        let n = s.sources.len();
        s.sources[1].name = name;
        let (bytes, fresh) = session_bundle(e.session()).unwrap();
        let paths: std::collections::HashSet<_> = fresh.sources.iter().map(|s| s.path.clone()).collect();
        assert_eq!(paths.len(), n);
        let mut r = Engine::default();
        assert!(open_session_bytes(&mut r, "x.scraftz", &bytes).unwrap().is_empty());
    }

    #[test]
    fn bare_session_opens_with_its_audio_reported_missing() {
        let e = crate::demo::demo_engine();
        let (_, fresh) = session_bundle(e.session()).unwrap();
        let text = fresh.to_json().unwrap();
        let mut r = Engine::default();
        let missing = open_session_bytes(&mut r, "Song.scraft", text.as_bytes()).unwrap();
        assert_eq!(missing.len(), fresh.sources.len());
        assert_eq!(r.session().tracks.len(), fresh.tracks.len());
    }

    #[test]
    fn hostile_bundles_are_errors_not_panics() {
        let mut r = Engine::default();
        for bad in [&b"PK\x03\x04 not really a zip"[..], b"", b"{ not json", b"\xff\xfe\x00"] {
            assert!(open_session_bytes(&mut r, "bad.scraftz", bad).is_err());
        }
        // A valid zip without a session inside.
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file("Audio Files/a.wav", zip::write::SimpleFileOptions::default()).unwrap();
        let only_audio = w.finish().unwrap().into_inner();
        assert!(open_session_bytes(&mut r, "a.scraftz", &only_audio).is_err());
        assert_eq!(r.session().tracks.len(), 0, "a failed open leaves the document alone");
    }

    #[test]
    fn write_file_writes_to_disk_natively() {
        let p = std::env::temp_dir().join(format!("soundcraft-write-file-{}.txt", std::process::id()));
        write_file(&p.to_string_lossy(), b"hello").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod in_place_tests {
    use super::*;

    /// A fresh temp folder for one test.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("soundcraft-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A one-second mono 32-bit float WAV at `rate` in `dir`: its absolute path and samples.
    fn library_wav(dir: &Path, rate: u32) -> (String, Vec<f32>) {
        wav_named(dir, "track.wav", rate, rate as usize)
    }

    /// A mono 32-bit float WAV of `frames` frames at `rate` named `name` in `dir`.
    fn wav_named(dir: &Path, name: &str, rate: u32, frames: usize) -> (String, Vec<f32>) {
        let samples: Vec<f32> = (0..frames).map(|i| ((i % 100) as f32 / 100.0 - 0.5) * 0.5).collect();
        let buf = AudioBuffer { sample_rate: rate, channels: vec![samples.clone()] };
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: soundcraft_audio_io::BitDepth::Float32, dither: false, bwf: None };
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, soundcraft_audio_io::encode(&buf, &opts).unwrap()).unwrap();
        (path.to_string_lossy().into_owned(), samples)
    }

    /// A 44.1 kHz session with one referenced import of `wav`, saved to `dir/Set/Set.scraft`.
    fn saved_with(dir: &Path, wav: &str) -> (Engine, String) {
        let mut e = session_at(44_100);
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        (e, path)
    }

    fn session_at(rate: u32) -> Engine {
        let mut e = Engine::default();
        e.execute("session.new", &json!({"sample_rate": rate})).unwrap();
        e
    }

    fn stored(session: &str) -> Session {
        Session::from_json(&std::fs::read_to_string(session).unwrap()).unwrap()
    }

    fn reopen(path: &str) -> Engine {
        let mut r = Engine::default();
        assert_eq!(open_session(&mut r, path).unwrap(), Vec::<String>::new());
        r
    }

    fn first_audio(e: &Engine) -> Vec<f32> {
        let s = e.session();
        s.pool.get(s.sources[0].id).unwrap().buffer.channels[0].clone()
    }

    #[test]
    fn sessions_at_the_source_rate_keep_audio_referenced_in_place() {
        let dir = folder("in-place");
        let (wav, audio) = library_wav(&dir.join("Library"), 44_100);
        let mut e = session_at(44_100);
        e.execute("dj.load_warped", &json!({"path": wav, "bpm": 120, "first_beat": 0.0})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        assert!(!dir.join("Set/Audio Files").exists());
        let saved = stored(&path);
        assert_eq!((saved.sources[0].path.as_str(), saved.sources[0].unsaved), (wav.as_str(), false));
        assert!(Path::new(&saved.sources[0].path).is_absolute());
        let mut r = reopen(&path);
        assert_eq!(first_audio(&r), audio);
        // The absolute path still finds the audio from another folder, and is not copied there.
        let moved = dir.join("Moved/Moved.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut r, &moved).unwrap(), 0);
        assert!(!dir.join("Moved/Audio Files").exists());
        assert_eq!(first_audio(&reopen(&moved)), audio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resampled_imports_are_copied_into_the_session() {
        let dir = folder("in-place-48k");
        let (wav, _) = library_wav(&dir.join("Library"), 44_100);
        let mut e = session_at(48_000);
        e.execute("dj.load_warped", &json!({"path": wav, "bpm": 120, "first_beat": 0.0})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        assert_eq!(std::fs::read_dir(dir.join("Set/Audio Files")).unwrap().count(), 1);
        assert!(stored(&path).sources[0].path.starts_with("Audio Files/"));
        assert_eq!(first_audio(&reopen(&path)).len(), 48_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_of_referenced_audio_is_saved_into_the_session_and_leaves_the_file_alone() {
        let dir = folder("in-place-edit");
        let (wav, audio) = library_wav(&dir.join("Library"), 44_100);
        let original = std::fs::read(&wav).unwrap();
        let mut e = session_at(44_100);
        let r = e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        let clip = r["imported"][0]["clip"].as_u64().unwrap();
        e.execute("audiosuite.process", &json!({"process": "invert", "clips": [clip]})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        assert_eq!(std::fs::read_dir(dir.join("Set/Audio Files")).unwrap().count(), 1);
        assert_eq!(std::fs::read(&wav).unwrap(), original);
        let r = reopen(&path);
        let s = r.session();
        let src = s.tracks.iter().flat_map(|t| t.clips()).find_map(|c| c.source()).unwrap();
        assert!(s.source(src).unwrap().path.starts_with("Audio Files/"));
        let edited = &s.pool.get(src).unwrap().buffer.channels[0];
        assert_eq!(edited.len(), audio.len());
        assert!(edited.iter().zip(&audio).all(|(x, y)| (x + y).abs() < 1e-6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reference_the_session_could_not_reopen_is_copied() {
        let dir = folder("in-place-refused");
        // A lookalike separator: the file opens on import, but a stored path naming it is refused.
        let (wav, audio) = wav_named(&dir.join("Lib"), "AC\u{FF0F}DC.wav", 44_100, 44_100);
        let mut e = session_at(44_100);
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        assert!(stored(&path).sources[0].path.starts_with("Audio Files/"));
        assert_eq!(first_audio(&reopen(&path)), audio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_network_path_off_the_sessions_share_is_copied() {
        let dir = folder("in-place-unc");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        // The same file through the administrative share: a network path for a local session.
        let Some(rest) = wav.strip_prefix("C:\\").or_else(|| wav.strip_prefix("c:\\")) else { return };
        let unc = format!("\\\\localhost\\C$\\{rest}");
        if !Path::new(&unc).is_file() {
            eprintln!("skipped: {unc} is not reachable (no administrative share)");
            return;
        }
        let mut e = session_at(44_100);
        e.execute("file.import_audio", &json!({"path": unc})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        assert!(stored(&path).sources[0].path.starts_with("Audio Files/"));
        assert_eq!(first_audio(&reopen(&path)), audio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_referenced_file_that_changed_on_disk_is_not_trusted() {
        let dir = folder("in-place-changed");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        // Changed before the save: the session keeps the audio it was edited with.
        let mut e = session_at(44_100);
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        wav_named(&dir.join("Lib"), "track.wav", 44_100, 22_050);
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 1);
        assert_eq!(first_audio(&reopen(&path)), audio);
        // Changed after the save: opening says so instead of loading it silently.
        let (wav, _) = library_wav(&dir.join("Lib2"), 44_100);
        let (_, path) = saved_with(&dir.join("B"), &wav);
        wav_named(&dir.join("Lib2"), "track.wav", 44_100, 22_050);
        let mut r = Engine::default();
        let report = open_session_checked(&mut r, &path).unwrap();
        assert!(report.missing.is_empty());
        assert_eq!(report.changed.len(), 1);
        assert!(report.changed[0].contains("changed on disk") && report.changed[0].contains("22050 frames"), "{:?}", report.changed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rewrite_or_retag_that_keeps_the_audio_keeps_the_reference() {
        let dir = folder("in-place-retag");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        let (mut e, path) = saved_with(&dir, &wav);
        // The same bytes written again (a copy back from a backup).
        let bytes = std::fs::read(&wav).unwrap();
        std::fs::write(&wav, &bytes).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        // A tag added in a trailing chunk: the file grows, the audio is the same.
        let mut tagged = bytes.clone();
        tagged.extend_from_slice(b"JUNK\x04\x00\x00\x00tag!");
        let riff = u32::from_le_bytes([tagged[4], tagged[5], tagged[6], tagged[7]]) + 12;
        tagged[4..8].copy_from_slice(&riff.to_le_bytes());
        std::fs::write(&wav, &tagged).unwrap();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        assert!(!dir.join("Set/Audio Files").exists());
        assert_eq!(stored(&path).sources[0].path, wav);
        assert_eq!(first_audio(&reopen(&path)), audio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_retagged_since_the_save_is_reported_on_open() {
        let dir = folder("in-place-retag-open");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        let (_, path) = saved_with(&dir, &wav);
        let mut r = Engine::default();
        let report = open_session_checked(&mut r, &path).unwrap();
        assert!(report.missing.is_empty() && report.changed.is_empty() && report.modified.is_empty(), "{report:?}");
        let mut tagged = std::fs::read(&wav).unwrap();
        tagged.extend_from_slice(b"JUNK\x04\x00\x00\x00tag!");
        let riff = u32::from_le_bytes([tagged[4], tagged[5], tagged[6], tagged[7]]) + 12;
        tagged[4..8].copy_from_slice(&riff.to_le_bytes());
        std::fs::write(&wav, &tagged).unwrap();
        let mut r = Engine::default();
        let report = open_session_checked(&mut r, &path).unwrap();
        assert!(report.changed.is_empty(), "{report:?}");
        assert_eq!(report.modified.len(), 1, "{report:?}");
        assert!(report.modified[0].contains("modified since the session was saved"));
        assert_eq!(first_audio(&r), audio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bounce_never_replaces_a_referenced_file() {
        let dir = folder("in-place-bounce");
        let (wav, _) = library_wav(&dir.join("Lib"), 44_100);
        let before = std::fs::read(&wav).unwrap();
        let (mut e, _) = saved_with(&dir, &wav);
        let err = e.execute("file.bounce_mix", &json!({"path": wav})).unwrap_err().to_string();
        assert!(err.contains("audio file of source"), "{err}");
        // Every export that writes a path the person picks refuses it too.
        let track = e.session().tracks[0].id.0;
        let clip = e.session().tracks[0].clips()[0].id.0;
        e.session_mut().add_track(TrackKind::Midi, ChannelFormat::Mono, Some("Notes"));
        let exports = [
            ("file.export_midi", json!({"path": wav})),
            ("file.export_session_text", json!({"path": wav})),
            ("file.export_sibelius", json!({"path": wav})),
            ("track.save_preset", json!({"path": wav, "tracks": [track]})),
            ("file.export_clip_groups", json!({"path": wav, "clips": [clip]})),
            ("session.save_template", json!({"path": wav, "replace": true})),
            ("session.save_copy", json!({"path": wav, "replace": true})),
        ];
        for (id, p) in exports {
            match e.execute(id, &p) {
                Err(err) => assert!(err.to_string().contains("audio file of source"), "{id}: {err}"),
                Ok(v) => panic!("{id} wrote over the source: {v}"),
            }
        }
        assert_eq!(std::fs::read(&wav).unwrap(), before);
        let other = dir.join("mix.wav").to_string_lossy().into_owned();
        e.execute("file.bounce_mix", &json!({"path": other})).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_copy_can_take_all_its_audio_along() {
        let dir = folder("in-place-copy");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        let (mut e, _) = saved_with(&dir, &wav);
        let plain = dir.join("Plain/Plain.scraft").to_string_lossy().into_owned();
        let r = e.execute("session.save_copy", &json!({"path": plain})).unwrap();
        assert_eq!(r["audio_files_written"], 0);
        let full = dir.join("Full/Full.scraft").to_string_lossy().into_owned();
        let r = e.execute("session.save_copy", &json!({"path": full, "copy_audio": true})).unwrap();
        assert_eq!(r["audio_files_written"], 1);
        assert!(stored(&full).sources[0].path.starts_with("Audio Files/"));
        // The open session still references the file in place.
        assert_eq!(e.session().sources[0].path, wav);
        std::fs::remove_file(&wav).unwrap();
        assert_eq!(first_audio(&reopen(&full)), audio);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_file_imported_twice_stays_one_reference() {
        let dir = folder("in-place-twice");
        let (wav, _) = library_wav(&dir.join("Lib"), 44_100);
        let mut e = session_at(44_100);
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        let path = dir.join("Set/Set.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut e, &path).unwrap(), 0);
        assert!(stored(&path).sources.iter().all(|x| x.path == wav));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moved_audio_is_relinked_by_name_and_by_name_without_key_tags() {
        let dir = folder("in-place-relink");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        let (_, path) = saved_with(&dir, &wav);
        // Moved into a subfolder and renamed with Mixed In Key tags, next to a decoy of another
        // length whose name differs only by its tags.
        let moved = dir.join("New/Sorted");
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::rename(&wav, moved.join("1A - 128 - track.wav")).unwrap();
        wav_named(&dir.join("New"), "track - 5B.wav", 44_100, 1_000);
        let mut r = Engine::default();
        assert_eq!(open_session(&mut r, &path).unwrap().len(), 1);
        let out = r.execute("file.relink_audio", &json!({"dir": dir.join("New")})).unwrap();
        assert_eq!(out["relinked"].as_array().unwrap().len(), 1, "{out}");
        assert!(out["missing"].as_array().unwrap().is_empty());
        assert_eq!(Path::new(&r.session().sources[0].path), moved.join("1A - 128 - track.wav"));
        assert_eq!(first_audio(&r), audio);
        // Saved again, it references the file where it now is.
        let again = dir.join("Again/Again.scraft").to_string_lossy().into_owned();
        assert_eq!(save_session(&mut r, &again).unwrap(), 0);
        assert_eq!(first_audio(&reopen(&again)), audio);
        // Nothing to find: reported as still missing.
        std::fs::remove_dir_all(dir.join("New")).unwrap();
        let mut gone = Engine::default();
        open_session(&mut gone, &path).unwrap();
        let out = gone.execute("file.relink_audio", &json!({"dir": dir.join("Set")})).unwrap();
        assert_eq!(out["missing"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_tags_are_stripped_only_at_the_ends() {
        assert_eq!(untagged_stem("1A - 128 - Song - Remix"), "song - remix");
        assert_eq!(untagged_stem("Song - 12B"), "song");
        assert_eq!(untagged_stem("8A - Energy 6 - Song"), "song");
        assert_eq!(untagged_stem("128"), "128");
        assert_eq!(untagged_stem("Song - Part 2"), "song - part 2");
        assert_eq!(untagged_stem("Song - 1A - 128"), "song");
        assert_eq!(untagged_stem("Song - 126 BPM"), "song");
        // A bare number is a tempo only next to a key: a track number stays part of the name.
        assert_eq!(untagged_stem("03 - Song"), "03 - song");
        assert_eq!(untagged_stem("Song - 03"), "song - 03");
    }

    #[test]
    fn several_matching_files_are_never_guessed_between() {
        let dir = folder("in-place-ambiguous");
        let (wav, audio) = library_wav(&dir.join("Lib"), 44_100);
        let (_, path) = saved_with(&dir, &wav);
        std::fs::remove_file(&wav).unwrap();
        // Two edits of the same length under the same name.
        let (clean, _) = wav_named(&dir.join("New/Clean"), "track.wav", 44_100, 44_100);
        wav_named(&dir.join("New/Dirty"), "track.wav", 44_100, 44_100);
        let mut r = Engine::default();
        open_session(&mut r, &path).unwrap();
        let out = r.execute("file.relink_audio", &json!({"dir": dir.join("New")})).unwrap();
        assert!(out["relinked"].as_array().unwrap().is_empty(), "{out}");
        let amb = out["ambiguous"].as_array().unwrap();
        assert_eq!((amb.len(), amb[0]["candidates"].as_array().unwrap().len()), (1, 2), "{out}");
        assert!(r.session().pool.get(r.session().sources[0].id).is_none());
        // The person picks one.
        let src = r.session().sources[0].id.0;
        let out = r.execute("file.relink_audio", &json!({"source": src, "file": clean})).unwrap();
        assert_eq!(out["relinked"].as_array().unwrap().len(), 1);
        assert_eq!(r.session().sources[0].path, clean);
        assert_eq!(first_audio(&r), audio);
        // Now it is not missing.
        let err = r.execute("file.relink_audio", &json!({"source": src, "dir": dir.join("New")})).unwrap_err().to_string();
        assert!(err.contains("not missing"), "{err}");
        // A file of another length is refused.
        let (short, _) = wav_named(&dir.join("Short"), "track.wav", 44_100, 100);
        let mut again = Engine::default();
        open_session(&mut again, &path).unwrap();
        assert!(again.execute("file.relink_audio", &json!({"source": src, "file": short})).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_track_number_is_not_a_key_tag() {
        let dir = folder("in-place-track-number");
        let (wav, _) = wav_named(&dir.join("Lib"), "Song.wav", 44_100, 44_100);
        let (_, path) = saved_with(&dir, &wav);
        std::fs::remove_file(&wav).unwrap();
        wav_named(&dir.join("New"), "03 - Song.wav", 44_100, 44_100);
        let mut r = Engine::default();
        open_session(&mut r, &path).unwrap();
        let out = r.execute("file.relink_audio", &json!({"dir": dir.join("New")})).unwrap();
        assert_eq!(out["missing"].as_array().unwrap().len(), 1, "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
