//! Crash-recovery copies of open sessions, one folder each, written in the background, and the
//! recovery of the ones left behind with unsaved work.

use crate::io::{Recording, open_session_linked, sanitize_name};
use crate::{Engine, EngineError, Result};
use soundcraft_audio_io::{BitDepth, EncodeOptions, FileFormat};
use soundcraft_model::{Session, SourceAudio, SourceId};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

const SESSION: &str = "session.djforge";
/// The session file of folders SoundCraft wrote (a settings folder copied across keeps them).
const LEGACY_SESSION: &str = "session.scraft";
const ORIGIN: &str = "origin";
/// What the session file the folder came from held when the folder was written (see
/// [`file_state`]). Folders SoundCraft and earlier djforge builds wrote don't have it.
const ORIGIN_STATE: &str = "origin-state";
const LOCK: &str = "lock";
/// The plan of a recording still running when the folder was written.
const RECORDING: &str = "recording.json";
/// While changes keep coming, the newest is written at most this often.
const INTERVAL: Duration = Duration::from_secs(1);
const STOPPED: &str = "the autosave thread stopped";

/// Writes autosaves on a thread of its own, so the UI never waits on the disk.
pub struct Autosaver {
    jobs: Option<Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
}

enum Job {
    Write(Arc<Session>, Option<String>, Option<Recording>),
    /// Done with the session's folder: keep it for the next launch, or delete it.
    Close(bool),
    Flush(Sender<std::result::Result<(), String>>),
}

impl Autosaver {
    /// A writer for autosave folders under `root`.
    pub fn new(root: PathBuf) -> Autosaver {
        let (tx, rx) = channel();
        let failure = Arc::new(Mutex::new(None));
        let writer = Writer { root, failure: Arc::clone(&failure), folder: None, pending: None, last: None, written: Ok(()) };
        let thread = std::thread::Builder::new().name("soundcraft-autosave".into()).spawn(move || writer.run(&rx));
        if let Err(e) = &thread {
            report(&failure, format!("{STOPPED}: {e}"));
        }
        Autosaver { jobs: Some(tx), thread: thread.ok(), failure }
    }

    /// Autosave `session`, opened from the session file `origin`, soon, with the plan of the
    /// recording running in it.
    pub fn write(&self, session: Arc<Session>, origin: Option<String>, recording: Option<Recording>) {
        self.send(Job::Write(session, origin, recording));
    }

    /// Done with the open session's folder: keep it for the next launch, or delete it.
    pub fn close(&self, keep: bool) {
        self.send(Job::Close(keep));
    }

    /// Wait until everything handed over so far is done. A failure returned here isn't reported
    /// again by `failure`.
    pub fn flush(&self) -> Result<()> {
        let (tx, rx) = channel();
        self.send(Job::Flush(tx));
        let done = rx.recv().unwrap_or_else(|_| Err(STOPPED.into()));
        if done.is_err() {
            self.failure();
        }
        done.map_err(EngineError::Io)
    }

    /// The newest failure since the last call.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    fn send(&self, job: Job) {
        if self.jobs.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
            report(&self.failure, STOPPED.into());
        }
    }
}

impl Drop for Autosaver {
    /// What was handed over is written, and an open folder stays for the next launch.
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The writer thread.
struct Writer {
    root: PathBuf,
    failure: Arc<Mutex<Option<String>>>,
    /// The open session's folder.
    folder: Option<Autosave>,
    /// The newest session not written yet.
    pending: Option<(Arc<Session>, Option<String>, Option<Recording>)>,
    last: Option<Instant>,
    /// How the newest write into the open folder went, for `Flush`.
    written: std::result::Result<(), String>,
}

impl Writer {
    fn run(mut self, jobs: &Receiver<Job>) {
        loop {
            let wait = match (&self.pending, self.last) {
                (None, _) => Duration::MAX,
                (Some(_), last) => last.map_or(Duration::ZERO, |t| INTERVAL.saturating_sub(t.elapsed())),
            };
            match jobs.recv_timeout(wait) {
                Ok(Job::Write(session, origin, recording)) => self.pending = Some((session, origin, recording)),
                Ok(Job::Flush(reply)) => {
                    self.write_pending();
                    let _ = reply.send(self.written.clone());
                }
                Ok(Job::Close(keep)) => {
                    if keep {
                        self.write_pending();
                    }
                    self.pending = None;
                    self.written = Ok(());
                    if let Some(a) = self.folder.take()
                        && !keep
                    {
                        a.discard();
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.write_pending(),
                Err(RecvTimeoutError::Disconnected) => {
                    self.write_pending();
                    return;
                }
            }
        }
    }

    fn write_pending(&mut self) {
        let Some((session, origin, recording)) = self.pending.take() else { return };
        self.last = Some(Instant::now());
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            if self.folder.is_none() {
                self.folder = Some(Autosave::create(&self.root, &session.name)?);
            }
            self.folder.as_mut().map_or(Ok(()), |a| a.write(&session, origin.as_deref(), recording.as_ref()))
        }));
        self.written = match written {
            Ok(written) => written.map_err(|e| e.to_string()),
            Err(_) => Err("internal error (see the log)".into()),
        };
        if let Err(e) = &self.written {
            report(&self.failure, e.clone());
        }
    }
}

fn report(failure: &Mutex<Option<String>>, e: String) {
    log::warn!("autosave failed: {e}");
    *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
}

/// An open session's autosave folder, locked while the session is open so a later launch can tell
/// it from one a crash left behind.
struct Autosave {
    dir: PathBuf,
    lock: Option<std::fs::File>,
    copies: BTreeMap<SourceId, (Weak<SourceAudio>, String)>,
    next: u64,
    /// A recording's plan is in the folder.
    recording: bool,
}

impl Autosave {
    /// A new folder under `root` for the session called `name`.
    fn create(root: &Path, name: &str) -> Result<Autosave> {
        let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();
        let dir = root.join(format!("{} {stamp}-{}", sanitize_name(name), std::process::id()));
        let lock = std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(dir.join(LOCK))).map_err(|err| io_error(&dir, err))?;
        lock.try_lock().map_err(|err| EngineError::Io(format!("{}: {err}", dir.display())))?;
        Ok(Autosave { dir, lock: Some(lock), copies: BTreeMap::new(), next: 1, recording: false })
    }

    /// Write `session`, opened from the session file `origin`, and the plan of the `recording`
    /// running in it. Audio saved with it is referenced where it is; audio that exists only in
    /// memory is copied, once per recording or render.
    fn write(&mut self, session: &Session, origin: Option<&str>, recording: Option<&Recording>) -> Result<()> {
        let base = origin.and_then(|p| Path::new(p).parent()).and_then(|p| std::path::absolute(p).ok());
        let audio_dir = self.dir.join("Audio Files");
        let mut s = session.clone();
        let mut copies = BTreeMap::new();
        // Recovery follows the paths as if stored in the original session file, or in this one.
        let links = origin.map_or_else(|| self.dir.join(SESSION), PathBuf::from);
        for src in &mut s.sources {
            let absolute = Path::new(&src.path).is_absolute();
            let file = if absolute { Some(PathBuf::from(&src.path)) } else { base.as_ref().map(|b| b.join(&src.path)) };
            let audio = s.pool.get(src.id);
            // A file referenced in place (outside every `Audio Files`) is kept only while recovery
            // can reopen it and it is still the file the audio was decoded from.
            let kept = |f: &Path, a: &Arc<SourceAudio>| {
                !absolute || crate::io::in_audio_files(f) || crate::io::kept_reference(&src.path, &links, a).is_some()
            };
            if let Some(f) = &file
                && (audio.is_none() || !src.unsaved && f.is_file() && audio.is_some_and(|a| kept(f, a)))
            {
                src.path = f.to_string_lossy().into_owned();
                continue;
            }
            let Some(audio) = audio else { continue };
            let name = match self.copies.remove(&src.id) {
                Some((copy, name)) if copy.upgrade().is_some_and(|a| Arc::ptr_eq(&a, audio)) => name,
                _ => {
                    let name = format!("{} {}.wav", sanitize_name(&src.name), self.next);
                    self.next += 1;
                    let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
                    let bytes = soundcraft_audio_io::encode(&audio.buffer, &opts).map_err(|err| EngineError::Io(err.to_string()))?;
                    std::fs::create_dir_all(&audio_dir)
                        .and_then(|()| write_synced(&audio_dir.join(&name), &bytes))
                        .map_err(|err| io_error(&audio_dir, err))?;
                    name
                }
            };
            src.path = audio_dir.join(&name).to_string_lossy().into_owned();
            src.unsaved = true;
            copies.insert(src.id, (Arc::downgrade(audio), name));
        }
        let text = s.to_json().map_err(|err| EngineError::Io(err.to_string()))?;
        write_atomically(&self.dir.join(ORIGIN), origin.unwrap_or_default().as_bytes())?;
        // Read now, after the session handed over was taken: a save after this one changes it.
        write_atomically(&self.dir.join(ORIGIN_STATE), origin.map_or_else(|| NO_FILE.into(), file_state).as_bytes())?;
        write_atomically(&self.dir.join(SESSION), text.as_bytes())?;
        for (_, name) in std::mem::replace(&mut self.copies, copies).into_values() {
            let _ = std::fs::remove_file(audio_dir.join(name));
        }
        if let Some(r) = recording {
            let plan = serde_json::to_string(r).map_err(|err| EngineError::Io(err.to_string()))?;
            write_atomically(&self.dir.join(RECORDING), plan.as_bytes())?;
        } else if self.recording {
            let _ = std::fs::remove_file(self.dir.join(RECORDING));
        }
        self.recording = recording.is_some();
        Ok(())
    }

    /// Delete the folder: the session closed with nothing left to recover.
    fn discard(mut self) {
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The session file in the autosave folder `dir`: `session.djforge`, or the `session.scraft` of a
/// folder SoundCraft wrote.
fn session_in(dir: &Path) -> PathBuf {
    let legacy = dir.join(LEGACY_SESSION);
    if !dir.join(SESSION).exists() && legacy.is_file() { legacy } else { dir.join(SESSION) }
}

/// Unsaved work left behind in an autosave folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Leftover {
    pub dir: PathBuf,
    /// The session's name.
    pub name: String,
    /// The session file it was autosaved from, if it had one.
    pub origin: Option<String>,
    pub saved: SystemTime,
    /// A recording was still running in it.
    pub recording: bool,
}

/// How many times [`leftovers`] tries a folder's lock, and how long apart.
const LOCK_TRIES: usize = 3;
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(20);

/// [`ORIGIN_STATE`] when there was no session file, or it couldn't be read.
const NO_FILE: &str = "none";

/// What the file at `path` holds: its length and FNV-1a hash, or [`NO_FILE`].
fn file_state(path: &str) -> String {
    std::fs::read(path).map_or_else(|_| NO_FILE.into(), |b| format!("{} {:016x}", b.len(), crate::cmd::prep::fnv1a64(&b)))
}

/// The session file `origin` the autosave in `dir` (its session file `file`, written at `saved`)
/// came from was saved since with exactly what the autosave holds. Only a sure sign counts;
/// anything else (a rewrite by another program, a sync tool or a second instance) keeps the
/// autosave. Modification times can't tell: a save and the next change's autosave can land
/// within the file system's time resolution.
fn superseded(dir: &Path, file: &Path, origin: Option<&str>, saved: SystemTime) -> bool {
    let Some(origin) = origin else { return false };
    let Ok(modified) = std::fs::metadata(origin).and_then(|m| m.modified()) else { return false };
    match std::fs::read_to_string(dir.join(ORIGIN_STATE)) {
        // Rewritten since the autosave was written, with the autosave's session in it.
        Ok(then) => then != file_state(origin) && same_session(file, Path::new(origin)),
        // A folder SoundCraft or an earlier djforge build wrote: only a session file saved
        // strictly later says so.
        Err(_) => modified > saved,
    }
}

/// The session file `origin` holds the session the autosave session file `autosave` holds. Only
/// what saving rewrites may differ: where each source's audio is stored (the same file, or a
/// file of the same bytes) and the stamps of that file. Anything unreadable is not the same.
fn same_session(autosave: &Path, origin: &Path) -> bool {
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let (Some(mut a), Some(mut o)) = (read(autosave), read(origin)) else { return false };
    // Both resolve relative paths against the session file's folder.
    let base = origin.parent().unwrap_or(Path::new(""));
    let (Some(a_sources), Some(o_sources)) = (take_sources(&mut a, base), take_sources(&mut o, base)) else { return false };
    a == o && a_sources.len() == o_sources.len() && a_sources.iter().zip(&o_sources).all(|((af, a), (of, o))| a == o && same_audio(af, of))
}

/// Take the sources out of the session `v`: each without what saving rewrites, beside the file
/// its audio is in, relative paths resolved against `base`.
fn take_sources(v: &mut serde_json::Value, base: &Path) -> Option<Vec<(PathBuf, serde_json::Value)>> {
    let serde_json::Value::Array(sources) = v.as_object_mut()?.remove("sources")? else { return None };
    sources
        .into_iter()
        .map(|mut src| {
            let fields = src.as_object_mut()?;
            let path = PathBuf::from(fields.remove("path")?.as_str()?);
            for rewritten in ["unsaved", "file_len", "file_modified_ms"] {
                fields.remove(rewritten);
            }
            Some((if path.is_absolute() { path } else { base.join(path) }, src))
        })
        .collect()
}

/// The largest audio file [`same_audio`] reads through to compare with another. A larger copy
/// (a long take) counts as different, which keeps its autosave folder (the safe direction)
/// instead of stalling the start reading gigabytes.
const SAME_AUDIO_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// `a` and `b` name the same file, or files of the same bytes (copies of at most
/// [`SAME_AUDIO_MAX_BYTES`]).
fn same_audio(a: &Path, b: &Path) -> bool {
    same_audio_within(a, b, SAME_AUDIO_MAX_BYTES)
}

/// [`same_audio`], reading through copies of at most `max_bytes` only.
fn same_audio_within(a: &Path, b: &Path, max_bytes: u64) -> bool {
    if a == b || std::fs::canonicalize(a).is_ok_and(|a| std::fs::canonicalize(b).is_ok_and(|b| a == b)) {
        return true;
    }
    let open = |p: &Path| std::fs::File::open(p).and_then(|f| Ok((f.metadata()?.len(), f)));
    let (Ok((a_len, a)), Ok((b_len, b))) = (open(a), open(b)) else { return false };
    if a_len != b_len || a_len > max_bytes {
        return false;
    }
    let (mut a, mut b) = (std::io::BufReader::new(a), std::io::BufReader::new(b));
    loop {
        let (Ok(x), Ok(y)) = (std::io::BufRead::fill_buf(&mut a), std::io::BufRead::fill_buf(&mut b)) else { return false };
        let n = x.len().min(y.len());
        if n == 0 {
            return x.is_empty() && y.is_empty();
        }
        if x[..n] != y[..n] {
            return false;
        }
        std::io::BufRead::consume(&mut a, n);
        std::io::BufRead::consume(&mut b, n);
    }
}

/// Leftover autosaves under `root`, newest first: folders no open session holds, unless the
/// session file they came from was saved with exactly their changes since (see [`superseded`]). Empty
/// and outdated ones are deleted; a folder still locked is left alone.
pub fn leftovers(root: &Path) -> Vec<Leftover> {
    let mut out = Vec::new();
    for dir in std::fs::read_dir(root).into_iter().flatten().flatten().map(|d| d.path()) {
        let Ok(lock) = std::fs::OpenOptions::new().write(true).open(dir.join(LOCK)) else { continue };
        // A lock just released can still look taken for a moment (a child process another part of
        // the app started may hold a copy of its handle until it execs): ask a few times.
        let free = (0..LOCK_TRIES).any(|k| {
            if k > 0 {
                std::thread::sleep(LOCK_RETRY);
            }
            lock.try_lock().is_ok()
        });
        if !free {
            continue;
        }
        let origin = std::fs::read_to_string(dir.join(ORIGIN)).ok().filter(|o| !o.is_empty());
        let file = session_in(&dir);
        let saved = std::fs::metadata(&file).and_then(|m| m.modified());
        let recording = dir.join(RECORDING).is_file();
        drop(lock);
        match saved {
            Ok(saved) if recording || !superseded(&dir, &file, origin.as_deref(), saved) => {
                let text = std::fs::read_to_string(&file).unwrap_or_default();
                let named = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("name")?.as_str().map(str::to_string));
                let name = named.unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into_owned());
                out.push(Leftover { dir, name, origin, saved, recording });
            }
            _ => {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
    out.sort_by_key(|l| std::cmp::Reverse(l.saved));
    out
}

/// Open `left` as an unsaved version of the session it came from, so Save writes it back there,
/// with the takes of a recording that never finished placed as Stop would have. Returns the
/// media that couldn't be loaded.
pub fn recover(e: &mut Engine, left: &Leftover) -> Result<Vec<String>> {
    recover_checked(e, left).map(|r| r.missing)
}

/// [`recover`], also reporting audio files that changed or were modified on disk since.
pub fn recover_checked(e: &mut Engine, left: &Leftover) -> Result<crate::io::OpenReport> {
    let mut r = Engine::default();
    let session = session_in(&left.dir);
    let links = left.origin.as_deref().map_or(session.as_path(), Path::new);
    let mut report = open_session_linked(&mut r, &session.to_string_lossy(), links)?;
    let mut s = r.session().clone();
    let unfinished = unfinished_takes(left, &s, &mut report.missing);
    if let Some(home) = left.origin.as_deref().and_then(|o| Path::new(o).parent()) {
        for src in s.sources.iter_mut().filter(|src| !src.unsaved) {
            if let Ok(rel) = Path::new(&src.path).strip_prefix(home) {
                src.path = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            }
        }
    }
    e.replace_session(Session::default());
    *e.session_mut() = s;
    e.path = left.origin.clone();
    if let Some((plan, takes)) = unfinished {
        crate::io::place_recording(e, &plan, takes);
    }
    Ok(report)
}

/// The takes of the recording `left` holds the plan of, repaired, minus those already in `s`.
/// A take the plan names that isn't a take file SoundCraft recorded, in a folder it records into,
/// is left untouched and added to `skipped`.
fn unfinished_takes(left: &Leftover, s: &Session, skipped: &mut Vec<String>) -> Option<(Recording, Vec<(soundcraft_model::TrackId, PathBuf)>)> {
    let plan: Recording = serde_json::from_str(&std::fs::read_to_string(left.dir.join(RECORDING)).ok()?).ok()?;
    let known: Vec<PathBuf> =
        s.sources.iter().filter(|src| s.pool.get(src.id).is_some()).filter_map(|src| std::fs::canonicalize(&src.path).ok()).collect();
    let folders = take_folders(left);
    let mut takes = Vec::new();
    for t in &plan.takes {
        if std::fs::canonicalize(&t.path).is_ok_and(|p| known.contains(&p)) {
            continue;
        }
        match repair_take(&t.path, &folders, t.channels, plan.rate) {
            Ok(frames) if frames > 0 => takes.push((t.track, t.path.clone())),
            Ok(_) => {}
            Err(err) => {
                log::warn!("recovery skipped the take {}: {err}", t.path.display());
                skipped.push(format!("{}: {err}", t.path.display()));
            }
        }
    }
    Some((plan, takes))
}

/// The folders a recording in `left`'s session writes its takes into: the `Audio Files` next to
/// the session file, or, never saved, the one in its folder under the autosave root.
fn take_folders(left: &Leftover) -> Vec<PathBuf> {
    let saved = left.origin.as_deref().and_then(|o| Path::new(o).parent()).map(|d| d.join("Audio Files"));
    let unsaved = left.dir.parent().map(|root| root.join(sanitize_name(&left.name)).join("Audio Files"));
    saved.into_iter().chain(unsaved).filter_map(|d| std::fs::canonicalize(d).ok()).collect()
}

/// Make a take file's header cover all the audio in it; returns its frames. The recorder
/// rewrites the header only every second, so a crash leaves up to a second uncovered. Only a
/// regular file in one of `folders` that starts with the header the recorder writes is touched.
fn repair_take(path: &Path, folders: &[PathBuf], channels: usize, rate: u32) -> std::io::Result<u64> {
    let refuse = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string());
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(refuse("not a regular file"));
    }
    let folder = path.parent().and_then(|d| std::fs::canonicalize(d).ok());
    if !folder.is_some_and(|d| folders.contains(&d)) {
        return Err(refuse(&format!("not in a folder {} records into", crate::APP_NAME)));
    }
    let header = |frames| soundcraft_audio_io::wav_float_header(channels, rate, frames).map_err(std::io::Error::other);
    let expected = header(0)?;
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut head = vec![0u8; expected.len()];
    if std::io::Read::read_exact(&mut f, &mut head).is_err() || !is_take_header(&head, &expected) {
        return Err(refuse(&format!("not a take {} recorded", crate::APP_NAME)));
    }
    let audio = f.metadata()?.len().saturating_sub(expected.len() as u64);
    let frames = audio / (channels.max(1) as u64 * 4);
    std::io::Seek::rewind(&mut f)?;
    f.write_all(&header(frames)?)?;
    f.sync_all()?;
    Ok(frames)
}

/// `head` is a take header like `expected` (the recorder's header for no frames yet): RIFF or
/// RF64 WAVE, the same format chunk at the same place, and the data chunk right after it all.
/// Only the sizes and frame counts may differ.
fn is_take_header(head: &[u8], expected: &[u8]) -> bool {
    let magic = matches!(head.get(..4), Some(b"RIFF" | b"RF64")) && head.get(8..12) == Some(b"WAVE");
    let fmt = expected.windows(4).position(|w| w == b"fmt ");
    let fmt_len = fmt.and_then(|at| expected.get(at + 4..at + 8)).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes);
    let same_fmt = match (fmt, fmt_len) {
        (Some(at), Some(n)) => {
            let end = at.saturating_add(8).saturating_add(n as usize);
            expected.get(at..end).is_some_and(|chunk| head.get(at..end) == Some(chunk))
        }
        _ => false,
    };
    let data = expected.len().checked_sub(8).and_then(|at| head.get(at..at + 4)) == Some(b"data");
    magic && same_fmt && data
}

/// Delete a leftover autosave.
pub fn discard(left: &Leftover) {
    let _ = std::fs::remove_dir_all(&left.dir);
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    write_synced(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        io_error(path, err)
    })
}

fn io_error(path: &Path, err: std::io::Error) -> EngineError {
    EngineError::Io(format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::open_session;
    use serde_json::json;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{ChannelFormat, Clip, TrackKind};

    #[test]
    fn copies_over_the_size_cap_count_as_different_audio() {
        let dir = folder("same-audio-cap");
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.wav"), dir.join("b.wav"));
        std::fs::write(&a, [7u8; 4096]).unwrap();
        std::fs::write(&b, [7u8; 4096]).unwrap();
        assert!(same_audio_within(&a, &b, 4096), "equal copies within the cap");
        assert!(!same_audio_within(&a, &b, 4095), "a copy over the cap is never read through");
        // One file is the same file whatever its size: no bytes are read.
        assert!(same_audio_within(&a, &a, 0));
        std::fs::write(&b, [8u8; 4096]).unwrap();
        assert!(!same_audio_within(&a, &b, 4096));
        const { assert!(SAME_AUDIO_MAX_BYTES >= 100 * 1024 * 1024) };
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("soundcraft-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn record(e: &mut Engine, level: f32) {
        let before = e.session_arc();
        let s = e.session_mut();
        let t = match s.tracks.first() {
            Some(t) => t.id,
            None => s.add_track(TrackKind::Audio, ChannelFormat::Mono, None),
        };
        let src =
            crate::io::add_source(s, "Audio 1_01", AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; 4_800]] }, None, FileFormat::Wav);
        let id = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::audio(id, "Audio 1_01", src, 0, 0, 4_800));
        e.push_undo("Record", before);
    }

    fn levels(e: &Engine) -> Vec<f32> {
        let s = e.session();
        s.sources.iter().map(|src| s.pool.get(src.id).map_or(f32::NAN, |a| a.buffer.channels[0][0])).collect()
    }

    #[test]
    fn a_referenced_file_whose_audio_changed_is_decoded_once_across_autosaves() {
        let dir = folder("autosave-changed");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("track.wav");
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
        let write = |level: f32| {
            let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![level; 4_800]] };
            std::fs::write(&wav, soundcraft_audio_io::encode(&buf, &opts).unwrap()).unwrap();
        };
        write(0.5);
        let mut e = Engine::default();
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        // Same length, other audio.
        write(0.25);
        crate::io::touch_later(&wav);
        let mut a = Autosave::create(&dir.join("Autosave"), "Untitled").unwrap();
        let before = crate::io::REDECODES.with(std::cell::Cell::get);
        a.write(e.session(), None, None).unwrap();
        a.write(e.session(), None, None).unwrap();
        assert_eq!(crate::io::REDECODES.with(std::cell::Cell::get) - before, 1);
        // The audio in memory was copied, not the changed file referenced.
        assert_eq!(std::fs::read_dir(a.dir.join("Audio Files")).unwrap().count(), 1);
        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_referenced_in_place_is_not_copied_and_recovers_from_its_file() {
        let dir = folder("autosave-in-place");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("track.wav");
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.5; 4_800]] };
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
        std::fs::write(&wav, soundcraft_audio_io::encode(&buf, &opts).unwrap()).unwrap();
        let mut e = Engine::default();
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        let mut a = Autosave::create(&dir.join("Autosave"), "Untitled").unwrap();
        a.write(e.session(), None, None).unwrap();
        assert!(!a.dir.join("Audio Files").exists());
        drop(a);
        let left = leftovers(&dir.join("Autosave"));
        let mut r = Engine::default();
        assert_eq!(recover(&mut r, &left[0]).unwrap(), Vec::<String>::new());
        let src = &r.session().sources[0];
        assert_eq!((Path::new(&src.path), src.unsaved), (wav.as_path(), false));
        assert_eq!(levels(&r), [0.5]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_audio_is_referenced_and_takes_are_copied_once() {
        let dir = folder("autosave-copies");
        let path = dir.join("Song/Song.djforge");
        let mut e = Engine::default();
        record(&mut e, 0.5);
        crate::io::save_session(&mut e, &path.to_string_lossy()).unwrap();
        record(&mut e, -0.25);
        let mut a = Autosave::create(&dir.join("Autosave"), "Song").unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        assert_eq!(std::fs::read_dir(a.dir.join("Audio Files")).unwrap().count(), 1);
        drop(a);
        let left = leftovers(&dir.join("Autosave"));
        let mut r = Engine::default();
        assert_eq!(recover(&mut r, &left[0]).unwrap(), Vec::<String>::new());
        let paths: Vec<_> = r.session().sources.iter().map(|s| (s.path.starts_with("Audio Files/"), s.unsaved)).collect();
        assert_eq!(paths, [(true, false), (false, true)]);
        assert_eq!(levels(&r), [0.5, -0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_recorded_after_undo_gets_its_own_copy() {
        let dir = folder("autosave-undo");
        let mut e = Engine::default();
        record(&mut e, 0.5);
        let mut a = Autosave::create(&dir, "Untitled").unwrap();
        a.write(e.session(), None, None).unwrap();
        e.execute("edit.undo", &json!({})).unwrap();
        record(&mut e, -0.25);
        a.write(e.session(), None, None).unwrap();
        drop(a);
        let mut r = Engine::default();
        recover(&mut r, &leftovers(&dir)[0]).unwrap();
        assert_eq!(levels(&r), [-0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_writer_saves_the_newest_change_and_drops_the_folder_when_done() {
        let root = folder("autosave-writer");
        let w = Autosaver::new(root.clone());
        let mut e = Engine::default();
        for _ in 0..5 {
            record(&mut e, 0.5);
            w.write(e.session_arc(), None, None);
        }
        w.flush().unwrap();
        let file = std::fs::read_dir(&root).unwrap().flatten().map(|d| d.path().join(SESSION)).collect::<Vec<_>>();
        let mut r = Engine::default();
        open_session(&mut r, &file[0].to_string_lossy()).unwrap();
        assert_eq!((file.len(), r.session().sources.len()), (1, 5));
        w.close(false);
        w.flush().unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        record(&mut e, 0.5);
        w.write(e.session_arc(), None, None);
        drop(w);
        let mut r = Engine::default();
        recover(&mut r, &leftovers(&root)[0]).unwrap();
        assert_eq!(r.session().sources.len(), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_autosave_soundcraft_left_is_still_recovered() {
        let root = folder("autosave-legacy");
        let w = Autosaver::new(root.clone());
        let mut e = Engine::default();
        record(&mut e, 0.5);
        w.write(e.session_arc(), None, None);
        w.flush().unwrap();
        drop(w);
        // The folder as SoundCraft wrote it (a settings folder copied across keeps such folders).
        let dir = std::fs::read_dir(&root).unwrap().flatten().map(|d| d.path()).next().unwrap();
        std::fs::rename(dir.join(SESSION), dir.join(LEGACY_SESSION)).unwrap();
        let left = leftovers(&root);
        assert_eq!(left.len(), 1);
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert_eq!(r.session().sources.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_take_still_recording_at_a_crash_is_placed_on_recovery() {
        use crate::io::Take;
        let dir = folder("autosave-take");
        let root = dir.join("Autosave");
        let mut e = Engine::default();
        let track =
            soundcraft_model::TrackId(e.execute("track.new", &json!({"name": "Vox", "format": "mono"})).unwrap()["tracks"][0].as_u64().unwrap());
        let take = root.join(sanitize_name(&e.session().name)).join("Audio Files/Vox_01.wav");
        std::fs::create_dir_all(take.parent().unwrap()).unwrap();
        let audio: Vec<u8> = (0..480).flat_map(|i| (i as f32 / 1000.0).to_le_bytes()).collect();
        std::fs::write(&take, [soundcraft_audio_io::wav_float_header(1, 48_000, 100).unwrap(), audio].concat()).unwrap();
        let plan = Recording { start: 48_000, pass: None, punch: None, rate: 48_000, takes: vec![Take { track, path: take, channels: 1 }] };
        let clips = |e: &Engine| e.session().track(track).unwrap().clips().iter().map(|c| (c.start, c.length)).collect::<Vec<_>>();
        let w = Autosaver::new(root.clone());
        w.write(e.session_arc(), None, Some(plan.clone()));
        drop(w);
        let left = leftovers(&root);
        assert!(left[0].recording);
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert_eq!(clips(&r), [(48_000, 480)], "all of the take, its last second included");
        discard(&left[0]);
        let w = Autosaver::new(root.clone());
        w.write(r.session_arc(), None, Some(plan));
        drop(w);
        let mut again = Engine::default();
        recover(&mut again, &leftovers(&root)[0]).unwrap();
        assert_eq!((clips(&again), again.session().sources.len()), (vec![(48_000, 480)], 1), "placed once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovery_never_writes_a_file_that_isnt_a_take_in_a_take_folder() {
        use crate::io::Take;
        let dir = folder("autosave-hostile-take");
        let root = dir.join("Autosave");
        let mut e = Engine::default();
        let track =
            soundcraft_model::TrackId(e.execute("track.new", &json!({"name": "Vox", "format": "mono"})).unwrap()["tracks"][0].as_u64().unwrap());
        let takes = root.join(sanitize_name(&e.session().name)).join("Audio Files");
        std::fs::create_dir_all(&takes).unwrap();
        let wav = [soundcraft_audio_io::wav_float_header(1, 48_000, 0).unwrap(), vec![0; 4_000]].concat();
        let text = b"not audio at all, but long enough to be mistaken for a header if nobody looked at it ......".repeat(4);
        let files =
            [(takes.join("notes.wav"), text.clone()), (dir.join("Elsewhere/Vox_01.wav"), wav.clone()), (takes.join("../Vox_02.wav"), wav.clone())];
        for (path, bytes) in &files {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        // Only the symlink case below adds to it, and symlinks are a Unix-only part of the test.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
        #[cfg(unix)]
        {
            let link = takes.join("Vox_03.wav");
            std::os::unix::fs::symlink(&files[1].0, &link).unwrap();
            paths.push(link);
        }
        let plan = Recording {
            start: 0,
            pass: None,
            punch: None,
            rate: 48_000,
            takes: paths.iter().map(|p| Take { track, path: p.clone(), channels: 1 }).collect(),
        };
        let w = Autosaver::new(root.clone());
        w.write(e.session_arc(), None, Some(plan));
        drop(w);
        let mut r = Engine::default();
        let skipped = recover(&mut r, &leftovers(&root)[0]).unwrap();
        assert_eq!(skipped.len(), paths.len(), "{skipped:?}");
        assert!(r.session().sources.is_empty());
        for (path, bytes) in &files {
            assert_eq!(&std::fs::read(path).unwrap(), bytes, "{} untouched", path.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsaved_work_is_offered_once_its_session_closes() {
        let dir = folder("autosave-leftovers");
        let path = dir.join("Song.djforge").to_string_lossy().into_owned();
        let root = dir.join("Autosave");
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::default();
        crate::io::save_session(&mut e, &path).unwrap();
        record(&mut e, 0.5);
        let mut a = Autosave::create(&root, "Song").unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        assert!(leftovers(&root).is_empty());
        drop(a);
        let left = leftovers(&root);
        assert_eq!((left.len(), left[0].name.as_str(), left[0].origin.as_deref()), (1, "Song", Some(path.as_str())));
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert!(r.is_dirty());
        r.execute("session.save", &json!({})).unwrap();
        assert!(leftovers(&root).is_empty());
        assert!(!left[0].dir.exists());
        let mut reopened = Engine::default();
        open_session(&mut reopened, &path).unwrap();
        assert_eq!(levels(&reopened), [0.5]);
        Autosave::create(&root, "Song").unwrap().discard();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session saved, then changed within the file system's time resolution: the autosave of
    /// the change, left by a crash, and the session file have the same modification time.
    /// Returns the test's folder, the session file, the autosave root and the engine with the change.
    fn saved_then_changed(name: &str) -> (PathBuf, PathBuf, PathBuf, Engine) {
        let dir = folder(name);
        let path = dir.join("Song.djforge");
        let root = dir.join("Autosave");
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::default();
        // Every kind of source: a saved take, a file referenced in place and a new take.
        record(&mut e, 0.5);
        crate::io::save_session(&mut e, &path.to_string_lossy()).unwrap();
        let wav = dir.join("Elsewhere/track.wav");
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.75; 4_800]] };
        std::fs::write(&wav, soundcraft_audio_io::encode(&buf, &opts).unwrap()).unwrap();
        e.execute("file.import_audio", &json!({"path": wav})).unwrap();
        record(&mut e, -0.25);
        let mut a = Autosave::create(&root, "Song").unwrap();
        a.write(e.session(), e.path.as_deref(), None).unwrap();
        drop(a);
        (dir, path, root, e)
    }

    /// The autosave folders under `root`.
    fn autosaves(root: &Path) -> usize {
        std::fs::read_dir(root).map_or(0, Iterator::count)
    }

    /// Give the file at `p` the modification time of the autosave under `root`.
    fn same_time_as_the_autosave(p: &Path, root: &Path) {
        let autosave = std::fs::read_dir(root).unwrap().flatten().map(|d| d.path().join(SESSION)).next().unwrap();
        let t = std::fs::metadata(autosave).unwrap().modified().unwrap();
        std::fs::File::options().write(true).open(p).and_then(|f| f.set_modified(t)).unwrap();
        assert_eq!(std::fs::metadata(p).unwrap().modified().unwrap(), t);
    }

    #[test]
    fn an_autosave_as_old_as_its_session_file_is_still_offered() {
        let (dir, path, root, _) = saved_then_changed("autosave-same-time");
        same_time_as_the_autosave(&path, &root);
        let left = leftovers(&root);
        assert_eq!(left.len(), 1, "the unsaved change is offered, not deleted");
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert_eq!(levels(&r), [0.5, 0.75, -0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_autosave_is_dropped_once_its_session_is_saved_with_its_changes() {
        let (dir, path, root, mut e) = saved_then_changed("autosave-superseded");
        crate::io::save_session(&mut e, &path.to_string_lossy()).unwrap();
        // Even at the very same time as the autosave: the session file holds its changes.
        same_time_as_the_autosave(&path, &root);
        assert!(leftovers(&root).is_empty());
        assert_eq!(autosaves(&root), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_autosave_is_kept_when_something_else_rewrites_its_session_file() {
        let (dir, path, root, _) = saved_then_changed("autosave-rewritten");
        // Another program, a sync tool or a second instance writes other contents there.
        let mut other = Engine::default();
        other.execute("track.new", &json!({"name": "Other"})).unwrap();
        crate::io::save_session(&mut other, &path.to_string_lossy()).unwrap();
        same_time_as_the_autosave(&path, &root);
        assert_eq!(leftovers(&root).len(), 1, "at the same time");
        crate::io::touch_later(&path);
        let left = leftovers(&root);
        assert_eq!(left.len(), 1, "later");
        let mut r = Engine::default();
        recover(&mut r, &left[0]).unwrap();
        assert_eq!(levels(&r), [0.5, 0.75, -0.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_autosave_is_kept_when_its_session_is_saved_with_other_audio() {
        let (dir, path, root, mut e) = saved_then_changed("autosave-other-audio");
        crate::io::save_session(&mut e, &path.to_string_lossy()).unwrap();
        // The take saved with it is not the one autosaved: same length, other samples.
        for take in std::fs::read_dir(dir.join("Audio Files")).unwrap().flatten().map(|d| d.path()) {
            let mut bytes = std::fs::read(&take).unwrap();
            *bytes.last_mut().unwrap() ^= 0x40;
            std::fs::write(&take, bytes).unwrap();
        }
        assert_eq!(leftovers(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_autosave_from_an_earlier_build_is_kept_unless_its_session_was_saved_later() {
        let (dir, path, root, _) = saved_then_changed("autosave-earlier-build");
        // Folders SoundCraft and earlier djforge builds wrote don't say what the session file held.
        let folder = std::fs::read_dir(&root).unwrap().flatten().map(|d| d.path()).next().unwrap();
        let _ = std::fs::remove_file(folder.join(ORIGIN_STATE));
        same_time_as_the_autosave(&path, &root);
        assert_eq!(leftovers(&root).len(), 1, "the same time is no sign the session was saved since");
        crate::io::touch_later(&path);
        assert!(leftovers(&root).is_empty());
        assert!(!folder.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
