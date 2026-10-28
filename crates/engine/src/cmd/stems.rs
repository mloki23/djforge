//! The stem cache: a source file split into vocals, drums, bass and other by Demucs (`htdemucs`,
//! run in WSL on Windows), stored once per file content as 24-bit FLAC at the source's own rate
//! and length, so the stems line up with it sample for sample.
//!
//! An entry is `<root>/<key>/{vocals,drums,bass,other}.flac` plus `stems.json`, built in a
//! `.tmp-<key>-<pid>-<n>` folder beside it and renamed into place, so a half-built entry never
//! looks finished. The root defaults to `%LOCALAPPDATA%\djforge\stems`, never inside the music
//! folders, and holds a `.djforge-stems` marker so [`clear`] only ever empties a stem cache.
//!
//! **Headroom.** Demucs' stems of a loud master can each peak above full scale, and a FLAC file
//! can't hold that. So Demucs is given the source at 2^-k of its level (k from its peak, one bit
//! spare) and runs with `--clip-mode none`; the stems are stored at that level, `stems.json`
//! records k (`scale_log2`), and playback multiplies them back by 2^k. Scaling by a power of two
//! is exact, so the stems still add up to the source.
//!
//! **File access.** The source is read under the automation roots like any file a command names.
//! The cache's own files (inside its root only) are the app's own folder: they are read and
//! written through [`cache_io`], outside the roots, so an MCP session confined to a music folder
//! can still split. So the root must never come from a command's arguments: it is
//! [`default_root`] (or a test's temporary folder), and the stem WAVs a runner returns are read
//! only from inside the split's own temporary folder.
//!
//! **Sessions.** `dj.split_stems` splits a clip's source on a worker thread ([`split_job`]) and
//! `dj.attach_stems` gives the clip, and every other unsplit clip of that source, the four stems
//! as sources referenced in place in the cache, in one undo step. The caches a session may read
//! stems from are [`default_root`] and every cache a split used in this process; a session names
//! a stem by its path, and only `<such a root>/<key>/<stem>.flac` is read outside the automation
//! roots ([`cached_stem`]), so a reopened session plays its stems wherever automation is
//! confined.

use crate::{Engine, EngineError, io};
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode, probe};
use soundcraft_model::{ClipId, STEMS, Session, SourceAudio, SourceId, StemSet};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The Demucs model the cache holds stems of; part of every key.
pub const MODEL: &str = "htdemucs";
/// The `stems.json` version this build writes and reads; older entries are split again.
pub const VERSION: u64 = 2;
/// The error a split stopped part-way gives.
pub const CANCELLED: &str = "cancelled";
/// Free space a split needs on the cache's drive by default.
pub const MIN_FREE: u64 = 2 << 30;
/// Most a stem's length may differ from the source's, in frames, to be padded or trimmed to it.
pub const LENGTH_SLACK: usize = 64;
/// A stored stem sample at or past this (at the scaled-down level) counts as clipping.
pub const CLIP: f32 = 0.999;
/// The file that marks a folder as a stem cache.
pub const MARKER: &str = ".djforge-stems";
/// Most headroom a split gives a source, in powers of two (the model's `StemSet::scale` limit).
const MAX_SCALE_LOG2: u8 = 30;
/// The entry's description, next to its stems.
const INDEX: &str = "stems.json";
/// How the temporary folders of splits under way are named.
const TMP_PREFIX: &str = ".tmp-";
/// The WSL python with Demucs when `DJFORGE_DEMUCS_PYTHON` is not set.
const DEFAULT_PYTHON: &str = "~/mixforge-wsl/venv/bin/python";
/// Shortest time Demucs is given over one file.
const DEMUCS_MIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long `timeout` waits after asking Demucs to stop before killing it.
const KILL_AFTER_SECS: u64 = 10;
/// Longest a helper process (free space, process checks, kills) may take.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a cached FLAC (or the input WAV) read to learn its rate and length.
const HEAD: usize = 64 << 10;

/// Separates the WAV at the first path into the four stems inside the output folder (the second
/// path), stopping when the check says so. Returns the four WAVs in [`STEMS`] order.
pub type StemRunner = Arc<dyn Fn(&Path, &Path, &dyn Fn() -> bool) -> Result<[PathBuf; 4], String> + Send + Sync>;

/// Free bytes on the drive holding a folder, `None` when that can't be learned.
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// Runs `f`, which touches only files inside a stem cache's root, outside the automation roots:
/// the cache is the app's own folder, not one a command names.
fn cache_io<T>(f: impl FnOnce() -> T) -> T {
    crate::file_access::unconfined(f)
}

/// Where stems are cached, and the free space a split needs there.
#[derive(Clone)]
pub struct StemCache {
    root: PathBuf,
    min_free: u64,
    free_space: FreeSpace,
}

impl std::fmt::Debug for StemCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StemCache").field("root", &self.root).field("min_free", &self.min_free).finish_non_exhaustive()
    }
}

impl StemCache {
    /// The cache in folder `root`, needing [`MIN_FREE`] bytes free on its drive for a split.
    /// The cache's files are read and written outside the automation roots, so `root` must be
    /// the app's own folder ([`default_root`]), never one taken from a command's arguments.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), min_free: MIN_FREE, free_space: Arc::new(free_space) }
    }

    /// The cache at [`default_root`].
    pub fn at_default() -> Result<Self, String> {
        default_root().map(Self::new)
    }

    /// This cache needing `min_free` bytes free for a split, as `free_space` reports them.
    pub fn with_free_space(mut self, min_free: u64, free_space: FreeSpace) -> Self {
        self.min_free = min_free;
        self.free_space = free_space;
        self
    }

    /// The folder holding the entries.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder of entry `key`.
    pub fn entry(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// The four stems of entry `key` and their `scale_log2`, if the entry is complete and intact:
    /// its `stems.json` is this build's [`VERSION`] and describes this key and model, and names
    /// each stem's FLAC, which has the recorded size, rate and length.
    fn hit(&self, key: &str) -> Option<([PathBuf; 4], u8)> {
        cache_io(|| {
            let dir = self.entry(key);
            let index: Value = serde_json::from_str(&io::read_text(dir.join(INDEX)).ok()?).ok()?;
            let (rate, frames) = (index.get("sample_rate")?.as_u64()?, index.get("frames")?.as_u64()?);
            if index.get("version")?.as_u64()? != VERSION || index.get("key")?.as_str()? != key || index.get("model")?.as_str()? != MODEL {
                return None;
            }
            let scale_log2 = u8::try_from(index.get("scale_log2")?.as_u64()?).ok().filter(|k| *k <= MAX_SCALE_LOG2)?;
            let stems = index.get("stems")?.as_array()?;
            if stems.len() != STEMS.len() {
                return None;
            }
            let mut paths: [PathBuf; 4] = Default::default();
            for ((name, stem), path) in STEMS.iter().zip(stems).zip(paths.iter_mut()) {
                let file = format!("{name}.flac");
                if stem.get("name")?.as_str()? != *name || stem.get("file")?.as_str()? != file {
                    return None;
                }
                let p = dir.join(&file);
                if io::file_len(&p)? != stem.get("bytes")?.as_u64()? {
                    return None;
                }
                let info = probe(&io::read_head(&p, HEAD).ok()?, Some("flac")).ok()?;
                if u64::from(info.sample_rate) != rate || info.frames != frames {
                    return None;
                }
                *path = p;
            }
            Some((paths, scale_log2))
        })
    }

    /// Removes entry `key` (one whose stems turned out not to load), so the next split of its
    /// file splits it again.
    pub fn remove(&self, key: &str) -> Result<(), String> {
        if !is_key(key) {
            return Err(format!("`{key}` is not a stem cache key"));
        }
        cache_io(|| io::remove_tree(self.entry(key))).map(|_| ()).map_err(|e| e.to_string())
    }

    /// Creates the root with its marker, and sweeps the temporary folders other processes left
    /// behind: only when their process is known to be gone. One whose process id was reused
    /// stays until [`clear`].
    fn prepare(&self) -> Result<(), String> {
        cache_io(|| {
            io::make_dir(&self.root).map_err(|e| e.to_string())?;
            let marker = self.root.join(MARKER);
            if !io::path_exists(&marker) {
                io::write_path(&marker, "djforge stem cache: Clear Stem Cache may empty this folder\n").map_err(|e| e.to_string())?;
            }
            for entry in io::list_entries(&self.root).map_err(|e| e.to_string())? {
                let Some(pid) = entry.file_name().and_then(|n| n.to_str()).and_then(tmp_pid) else { continue };
                // A live split's folder doesn't change while Demucs writes deep inside it, so its
                // age says nothing: only a gone process lets it go.
                if pid != std::process::id() && !pid_running(pid) {
                    let _ = io::remove_tree(&entry);
                }
            }
            Ok(())
        })
    }
}

/// A source's stems in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StemFiles {
    /// The cache entry: the source's content hash, size and the model.
    pub key: String,
    /// The FLACs in [`STEMS`] order.
    pub paths: [PathBuf; 4],
    /// The stems are stored at 2^-`scale_log2` of their level; they play at 2^`scale_log2`.
    pub scale_log2: u8,
    /// Whether they were already cached (nothing was run).
    pub hit: bool,
}

/// Where the stem cache lives by default: `%LOCALAPPDATA%\djforge\stems` on Windows, else
/// `$XDG_CACHE_HOME/djforge/stems` or `~/.cache/djforge/stems`.
pub fn default_root() -> Result<PathBuf, String> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|h| h.join(".cache"))).ok_or("neither XDG_CACHE_HOME nor HOME is set")?
    };
    Ok(base.join("djforge").join("stems"))
}

/// The cache key of a file holding `bytes`: its FNV-1a 64 hash, its size and the model.
pub fn key_of(bytes: &[u8]) -> String {
    format!("{:016x}-{}-{MODEL}", super::prep::fnv1a64(bytes), bytes.len())
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `name` is a cache entry's folder name (a key).
fn is_key(name: &str) -> bool {
    let mut parts = name.splitn(3, '-');
    let (Some(hash), Some(size), Some(model)) = (parts.next(), parts.next(), parts.next()) else { return false };
    hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && all_digits(size) && model == MODEL
}

/// The process id in `name` when it is a split's temporary folder name, `.tmp-<key>-<pid>-<n>`.
fn tmp_pid(name: &str) -> Option<u32> {
    let mut parts = name.strip_prefix(TMP_PREFIX)?.rsplitn(3, '-');
    let (n, pid, key) = (parts.next()?, parts.next()?, parts.next()?);
    if all_digits(n) && is_key(key) { pid.parse().ok().filter(|_| all_digits(pid)) } else { None }
}

/// Tells temporary folders of splits running at once in this process apart.
static TMP_COUNT: AtomicU64 = AtomicU64::new(0);

/// The temporary folders of this process's splits under way, which [`clear`] leaves alone.
static RUNNING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn running() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    RUNNING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A split's temporary folder, removed with everything in it when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(path: PathBuf) -> Self {
        running().push(path.clone());
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = cache_io(|| io::remove_tree(&self.0));
        running().retain(|p| p != &self.0);
    }
}

/// `path` for comparing: absolute with `.` and `..` resolved by name, without Windows' `\\?\`
/// prefix or a trailing separator, and in lower case on Windows.
fn path_key(path: &Path) -> String {
    let text = super::prep::normalise(path).to_string_lossy().into_owned();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).trim_end_matches(['/', '\\']);
    if cfg!(windows) { text.to_lowercase() } else { text.to_string() }
}

/// Whether `path` is inside folder `dir` (not `dir` itself), by name.
fn inside(path: &Path, dir: &Path) -> bool {
    let (p, d) = (path_key(path), path_key(dir));
    p.strip_prefix(&d).is_some_and(|rest| rest.starts_with(['/', '\\']))
}

/// The headroom, in powers of two, a source peaking at `peak` gets: enough to bring it to full
/// scale or under, and one more.
fn headroom(peak: f32) -> u8 {
    if !(peak.is_finite() && peak > 0.0) {
        return 1;
    }
    let bits = f64::from(peak).log2().ceil().max(0.0) + 1.0;
    if bits >= f64::from(MAX_SCALE_LOG2) { MAX_SCALE_LOG2 } else { bits as u8 }
}

/// Scales every sample of `buf` by 2^-`bits` (exact: a power of two).
fn scale_down(buf: &mut AudioBuffer, bits: u8) {
    let g = 0.5f32.powi(i32::from(bits));
    buf.channels.iter_mut().flatten().for_each(|x| *x *= g);
}

/// The stems of the audio file at `source_path` from the cache, splitting it with `runner` when
/// they are not there yet (or the entry is damaged or from an older build). `stop` cancels the
/// split ([`CANCELLED`]).
pub fn split(cache: &StemCache, source_path: &Path, runner: &StemRunner, stop: &dyn Fn() -> bool) -> Result<StemFiles, String> {
    let bytes = io::read_file(source_path).map_err(|e| e.to_string())?;
    let (key, size) = (key_of(&bytes), bytes.len());
    if let Some((paths, scale_log2)) = cache.hit(&key) {
        return Ok(StemFiles { key, paths, scale_log2, hit: true });
    }
    let cancelled = || if stop() { Err(CANCELLED.to_string()) } else { Ok(()) };
    cancelled()?;
    let shown = source_path.display();
    let ext = source_path.extension().and_then(|x| x.to_str());
    let (_, mut source) = decode(&bytes, ext).map_err(|e| format!("{shown}: {e}"))?;
    drop(bytes);
    let (frames, rate, channels) = (source.frames(), source.sample_rate, source.num_channels());
    if frames == 0 || channels == 0 || rate == 0 {
        return Err(format!("{shown}: holds no audio to split"));
    }
    if channels > 2 {
        return Err(format!("{shown}: has {channels} channels; stems can be split from mono or stereo files only"));
    }
    cache.prepare()?;
    check_free_space(cache)?;
    cancelled()?;
    let peak = source.channels.iter().flatten().filter(|x| x.is_finite()).fold(0.0f32, |m, x| m.max(x.abs()));
    let mut scale_log2 = headroom(peak);
    scale_down(&mut source, scale_log2);
    let n = TMP_COUNT.fetch_add(1, Ordering::Relaxed);
    let tmp = TempDir::new(cache.root.join(format!("{TMP_PREFIX}{key}-{}-{n}", std::process::id())));
    let (input, out) = (tmp.0.join("source.wav"), tmp.0.join("out"));
    let float_wav = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
    let mut retried = false;
    let stems = loop {
        cache_io(|| -> Result<(), String> {
            io::remove_tree(&out).map_err(|e| e.to_string())?;
            io::make_dir(&out).map_err(|e| e.to_string())?;
            let wav = encode(&source, &float_wav).map_err(|e| format!("could not write the WAV for Demucs: {e}"))?;
            io::write_path(&input, &wav).map_err(|e| e.to_string())
        })?;
        let wavs = runner(&input, &out, stop)?;
        cancelled()?;
        // The stems are read outside the automation roots, so only from this split's folder.
        if let Some(stray) = wavs.iter().find(|w| !inside(w, &tmp.0)) {
            return Err(format!("{}: not in the split's folder {}; the stems were not read", stray.display(), tmp.0.display()));
        }
        match cache_io(|| store_stems(&tmp.0, &wavs, rate, channels, frames, &cancelled))? {
            Some(stems) => break stems,
            None if !retried && scale_log2 < MAX_SCALE_LOG2 => {
                // A stem clips at this level: once more with another bit of headroom.
                retried = true;
                scale_log2 += 1;
                scale_down(&mut source, 1);
            }
            None => return Err(format!("{shown}: the stems clip even with headroom ({scale_log2} bits)")),
        }
    };
    drop(source);
    let index = json!({
        "version": VERSION,
        "source_path": shown.to_string(),
        "size": size,
        "key": key,
        "model": MODEL,
        "sample_rate": rate,
        "frames": frames,
        "scale_log2": scale_log2,
        "created": super::prep::utc_now(),
        "stems": stems,
    });
    let text = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
    cache_io(|| -> Result<(), String> {
        // Only the stems and their description go into the entry.
        for work in [&input, &out] {
            io::remove_tree(work).map_err(|e| e.to_string())?;
        }
        io::write_path(tmp.0.join(INDEX), text).map_err(|e| e.to_string())
    })?;
    cancelled()?;
    let dir = cache.entry(&key);
    // Another split of the same file that finished first keeps its entry.
    let theirs = |key: String| cache.hit(&key).map(|(paths, scale_log2)| StemFiles { key, paths, scale_log2, hit: false });
    if cache_io(|| io::path_exists(&dir)) {
        if let Some(files) = theirs(key.clone()) {
            return Ok(files);
        }
        // A damaged entry goes.
        cache_io(|| io::remove_tree(&dir)).map_err(|e| e.to_string())?;
    }
    if let Err(e) = cache_io(|| io::rename_path(&tmp.0, &dir)) {
        return theirs(key).ok_or_else(|| e.to_string());
    }
    let paths = STEMS.map(|name| dir.join(format!("{name}.flac")));
    Ok(StemFiles { key, paths, scale_log2, hit: false })
}

/// Encodes the runner's four WAVs `wavs` as FLACs in folder `dir`, each as `channels` channels
/// of exactly `frames` frames at `rate`. Returns their `stems.json` entries, or `None` when a
/// stem clips (reaches [`CLIP`]).
fn store_stems(
    dir: &Path,
    wavs: &[PathBuf; 4],
    rate: u32,
    channels: usize,
    frames: usize,
    cancelled: &dyn Fn() -> Result<(), String>,
) -> Result<Option<Vec<Value>>, String> {
    let mut stems = Vec::with_capacity(STEMS.len());
    for (name, wav) in STEMS.iter().zip(wavs) {
        cancelled()?;
        let stem = read_stem(name, wav, rate, channels, frames)?;
        if stem.channels.iter().flatten().any(|x| x.abs() >= CLIP) {
            return Ok(None);
        }
        let flac = encode(&stem, &EncodeOptions { format: FileFormat::Flac, bit_depth: BitDepth::Int24, dither: false, bwf: None })
            .map_err(|e| format!("could not encode stem {name}: {e}"))?;
        let file = format!("{name}.flac");
        io::write_path(dir.join(&file), &flac).map_err(|e| e.to_string())?;
        stems.push(json!({"name": name, "file": file, "bytes": flac.len()}));
    }
    Ok(Some(stems))
}

/// Stem `name` from the runner's WAV at `wav`, as `channels` channels of exactly `frames` frames
/// at `rate`.
fn read_stem(name: &str, wav: &Path, rate: u32, channels: usize, frames: usize) -> Result<AudioBuffer, String> {
    let bytes = io::read_file(wav).map_err(|e| e.to_string())?;
    let (_, mut stem) = decode(&bytes, Some("wav")).map_err(|e| format!("stem {name}: {e}"))?;
    drop(bytes);
    if stem.num_channels() != channels {
        // Demucs writes stereo; a mono source gets the average back.
        if channels != 1 || stem.num_channels() == 0 {
            return Err(format!("stem {name} has {} channels, the track {channels}", stem.num_channels()));
        }
        let n = stem.num_channels() as f32;
        let len = stem.frames();
        let mono = (0..len).map(|i| stem.channels.iter().map(|c| c.get(i).copied().unwrap_or(0.0)).sum::<f32>() / n).collect();
        stem.channels = vec![mono];
    }
    let mut stem = io::at_rate(stem, rate, io::MAX_IMPORT_SAMPLES).ok_or_else(|| format!("stem {name} is too long to resample"))?;
    let len = stem.frames();
    if len.abs_diff(frames) > LENGTH_SLACK {
        return Err(format!("stem {name} is {len} frames, the track is {frames}"));
    }
    for c in &mut stem.channels {
        c.resize(frames, 0.0);
    }
    Ok(stem)
}

/// Refuses a split when the cache's drive has less free space than the cache needs.
fn check_free_space(cache: &StemCache) -> Result<(), String> {
    // Unknown free space doesn't block a split; a full drive then fails the write itself.
    let Some(free) = (cache.free_space)(&cache.root) else { return Ok(()) };
    if free >= cache.min_free {
        return Ok(());
    }
    let gb = |b: u64| b as f64 / f64::from(1u32 << 30);
    Err(format!("need {} GB free on {} for stems; {:.1} GB free", gb(cache.min_free), drive_of(&cache.root), gb(free)))
}

/// The drive (Windows) or root folder holding `path`, for messages.
fn drive_of(path: &Path) -> String {
    match path.components().next() {
        Some(Component::Prefix(p)) => p.as_os_str().to_string_lossy().into_owned(),
        _ => path.display().to_string(),
    }
}

/// Free bytes on the drive holding folder `dir`: PowerShell's `DriveInfo` on Windows, `df`
/// elsewhere. `None` when the query fails.
pub fn free_space(dir: &Path) -> Option<u64> {
    let mut command = if cfg!(windows) {
        let drive = match dir.components().next()? {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => char::from(d),
                _ => return None,
            },
            _ => return None,
        };
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &format!("[System.IO.DriveInfo]::new('{drive}:\\').AvailableFreeSpace")]);
        c
    } else {
        let mut c = std::process::Command::new("df");
        c.arg("-Pk").arg(dir);
        c
    };
    let out = helper_output(&mut command)?;
    let text = String::from_utf8_lossy(&out);
    if cfg!(windows) {
        text.trim().parse().ok()
    } else {
        // `Filesystem 1024-blocks Used Available Capacity Mounted-on`, then the drive's line.
        let kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
        kb.checked_mul(1024)
    }
}

/// The standard output of a short helper command that succeeded, `None` otherwise.
fn helper_output(command: &mut std::process::Command) -> Option<Vec<u8>> {
    command.stdin(std::process::Stdio::null());
    hide_window(command);
    match super::dj::output_within(command, HELPER_TIMEOUT, &|| false) {
        Ok(super::dj::Sidecar::Ran(out)) if out.status.success() => Some(out.stdout),
        _ => None,
    }
}

/// Whether process `pid` is running; when that can't be learned, it counts as running.
pub(crate) fn pid_running(pid: u32) -> bool {
    if cfg!(windows) {
        let mut c = std::process::Command::new("tasklist");
        c.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
        helper_output(&mut c).is_none_or(|out| String::from_utf8_lossy(&out).contains(&format!(",\"{pid}\",")))
    } else {
        let mut c = std::process::Command::new("ps");
        c.args(["-p", &pid.to_string()]).stdin(std::process::Stdio::null());
        // `ps -p` exits 1 when there is no such process.
        !matches!(super::dj::output_within(&mut c, HELPER_TIMEOUT, &|| false), Ok(super::dj::Sidecar::Ran(out)) if out.status.code() == Some(1))
    }
}

/// Keeps a sidecar from flashing a console window over the app on Windows.
fn hide_window(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// `word` quoted for bash: in single quotes, each `'` written `'\''`. A leading `~/` stays
/// outside the quotes so bash still expands it.
fn bash_word(word: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    match word.strip_prefix("~/") {
        Some(rest) => format!("~/{}", quote(rest)),
        None => quote(word),
    }
}

/// `text` as a regular expression matching itself.
fn regex_literal(text: &str) -> String {
    text.chars().fold(String::new(), |mut out, c| {
        if r"\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// The python with Demucs: `DJFORGE_DEMUCS_PYTHON` (a WSL path on Windows) or
/// `~/mixforge-wsl/venv/bin/python`.
fn demucs_python() -> String {
    std::env::var("DJFORGE_DEMUCS_PYTHON").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| DEFAULT_PYTHON.to_string())
}

/// How long Demucs may take over `secs` seconds of audio: three times as long, and at least ten
/// minutes.
fn demucs_timeout(secs: f64) -> Duration {
    let three = if secs.is_finite() && secs > 0.0 { (secs * 3.0).ceil().min(1e9) as u64 } else { 0 };
    DEMUCS_MIN_TIMEOUT.max(Duration::from_secs(three))
}

/// The runner that separates with Demucs `htdemucs` (float32 WAVs, `--clip-mode none`), through
/// `wsl -e bash -lc` on Windows and the python itself elsewhere, under `timeout` (three times
/// the audio's length, at least ten minutes). When it is stopped or runs out of time, whatever
/// it left running is killed by the unique name of the split's folder.
pub fn demucs_runner() -> StemRunner {
    Arc::new(run_demucs)
}

fn run_demucs(input: &Path, out: &Path, stop: &dyn Fn() -> bool) -> Result<[PathBuf; 4], String> {
    let python = demucs_python();
    let secs = cache_io(|| io::read_head(input, HEAD).ok()).and_then(|h| probe(&h, Some("wav")).ok()).map_or(0.0, |i| i.duration_secs());
    let limit = demucs_timeout(secs);
    // The split's folder name is unique, and every process Demucs runs names it.
    let tag =
        input.parent().and_then(Path::file_name).and_then(|n| n.to_str()).ok_or_else(|| format!("{}: not in a split's folder", input.display()))?;
    let args = ["-m", "demucs", "-n", MODEL, "--float32", "--clip-mode", "none"];
    let mut command = if cfg!(windows) {
        let line = format!(
            "out=\"$(wslpath -u {})\" && in=\"$(wslpath -u {})\" && exec timeout --kill-after={KILL_AFTER_SECS} {} {} {} -o \"$out\" \"$in\"",
            bash_word(&for_wslpath(out)?),
            bash_word(&for_wslpath(input)?),
            limit.as_secs(),
            bash_word(&python),
            args.join(" "),
        );
        let mut c = std::process::Command::new("wsl");
        c.args(["-e", "bash", "-lc", &line]);
        c
    } else {
        let python = match python.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").map_or_else(|| PathBuf::from(&python), |h| PathBuf::from(h).join(rest)),
            None => PathBuf::from(&python),
        };
        let mut c = std::process::Command::new("timeout");
        c.arg(format!("--kill-after={KILL_AFTER_SECS}")).arg(limit.as_secs().to_string()).arg(&python);
        c.args(args).arg("-o").arg(out).arg(input);
        c
    };
    command.stdin(std::process::Stdio::null());
    hide_window(&mut command);
    let minutes = limit.as_secs().div_ceil(60);
    let grace = Duration::from_secs(KILL_AFTER_SECS + 30);
    let ran = match super::dj::output_within(&mut command, limit.saturating_add(grace), stop) {
        Ok(super::dj::Sidecar::Ran(ran)) => ran,
        other => {
            kill_leftovers(tag);
            return Err(match other {
                Ok(super::dj::Sidecar::Stopped) => CANCELLED.to_string(),
                Ok(super::dj::Sidecar::LeftHolding) => "Demucs left a process holding its output".to_string(),
                Err(err) => format!("could not run Demucs ({python}): {err}"),
                _ => format!("Demucs timed out after {minutes} minutes"),
            });
        }
    };
    let tail = || {
        let stderr = String::from_utf8_lossy(&ran.stderr);
        let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        lines.get(lines.len().saturating_sub(5)..).unwrap_or_default().join("\n")
    };
    if timed_out(ran.status.code()) {
        return Err(format!("Demucs timed out after {minutes} minutes"));
    }
    if !ran.status.success() {
        return Err(format!("Demucs failed ({}): {}", ran.status, tail()));
    }
    let stem_of = input.file_stem().ok_or_else(|| format!("{}: not a file name", input.display()))?;
    let dir = out.join(MODEL).join(stem_of);
    let paths = STEMS.map(|name| dir.join(format!("{name}.wav")));
    if let Some(missing) = paths.iter().find(|p| !cache_io(|| io::file_found(p))) {
        return Err(format!("Demucs wrote no {}: {}", missing.display(), tail()));
    }
    Ok(paths)
}

/// Whether an exit code is `timeout`'s for a command it stopped: 124 when Demucs ended on its
/// signal, 137 when it had to be killed.
fn timed_out(code: Option<i32>) -> bool {
    matches!(code, Some(124 | 137))
}

/// The Windows path `path` as `wslpath -u` takes it: without a `\\?\` prefix. A `\\?\UNC\`
/// network path is refused.
fn for_wslpath(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or_else(|| format!("{}: not a Unicode path", path.display()))?;
    if text.starts_with(r"\\?\UNC\") {
        return Err(format!("{}: Demucs in WSL can't reach a network path", path.display()));
    }
    Ok(text.strip_prefix(r"\\?\").unwrap_or(text).to_string())
}

/// Kills every process (in WSL on Windows) whose command line names the split folder `tag`;
/// twice, a moment apart, to catch one that was still starting.
fn kill_leftovers(tag: &str) {
    let pattern = regex_literal(tag);
    for pass in 0..2 {
        if pass > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        let mut command = if cfg!(windows) {
            let mut c = std::process::Command::new("wsl");
            c.args(["-e", "pkill", "-f", &pattern]);
            c
        } else {
            let mut c = std::process::Command::new("pkill");
            c.args(["-f", &pattern]);
            c
        };
        // pkill exits 1 when nothing was left to kill.
        let _ = helper_output(&mut command);
    }
}

// ─── Splitting clips ─────────────────────────────────────────────────────────────────────────

const SPLIT: &str = "dj.split_stems";
const ATTACH: &str = "dj.attach_stems";

/// What a split is doing, for its progress window.
pub const DECODING: &str = "decoding";
pub const SEPARATING: &str = "separating (Demucs)";
pub const ENCODING: &str = "encoding";
pub const LOADING: &str = "loading the stems";

pub fn specs() -> Vec<super::CommandSpec> {
    vec![
        crate::cmd!(
            "dj.split_stems",
            "Split into Stems",
            [],
            None,
            "{clip} — splits the audio file of the clip's source (it must be a readable file path; save a recorded take first) into vocals, drums, bass and other with Demucs `htdemucs`, cached by the file's content in the app's stem cache (a cached file is not split again), then gives the clip, and every other unsplit clip of the same source, the four stems in one undo step. Waits for Demucs (seconds to minutes when the file is not cached yet). Returns {clip, key, hit, scale_log2, clips: [ids given the stems], skipped: [{clip, key}: clips of the source already split from other stems], seconds}, or {clip, already: true, key} when the clip is split already. Through the running app (its control channel) it starts a background job instead and returns {clip, started: true}: poll the clip (inspect it) until it has stems before using them. Refused when no stem cache is set up",
            super::always,
            split_stems
        ),
        crate::cmd!(
            "dj.attach_stems",
            "Attach Stems",
            [],
            None,
            "{clip, key} — gives the clip, and every other unsplit clip of the same source, the stems of stem cache entry `key` (as dj.split_stems returns it; it must be the stems of the clip's audio file), referenced in place in the cache. Returns {clip, key, scale_log2, clips, skipped}, or {clip, already: true, key} when the clip is split already",
            super::always,
            attach_stems
        ),
        crate::cmd!(
            "clip.stem_mask",
            "Stem Mask",
            [],
            None,
            "{clip, mask: [\"vocals\" | \"drums\" | \"bass\" | \"other\", ...]} — the stems a split clip plays (at least one); the others are silent on this clip only. Returns {clip, mask}",
            super::always,
            stem_mask
        ),
        crate::cmd!(
            "dj.stem_copy",
            "Copy Stems to Track",
            [],
            None,
            "{clip, track, mask: [stem names]} — copies a split clip to audio track `track` at the same start, beat-locked like it, playing only the stems in `mask`; named with \" (acapella)\" (vocals only), \" (instrumental)\" (all but vocals) or \" (stems)\". Refused when it would overlap a clip on that track. Returns {clip: the copy, track, name, mask}",
            super::always,
            stem_copy
        ),
        crate::cmd!(
            "clip.unsplit_stems",
            "Remove Stems",
            [],
            None,
            "{clip} — the clip, and every clip of the same track split from the same stems that plays all four, play their own audio again; clips playing a stem mask (acapella, instrumental and other stem copies) keep their stems. The four stem files leave the session when nothing uses them any more (they stay in the stem cache). Returns {clips: [ids unsplit], kept: [ids of masked clips that keep their stems], sources: stem files removed}",
            super::always,
            unsplit_stems
        ),
        crate::cmd!(
            noundo "dj.clear_stem_cache",
            "Clear Stem Cache",
            [],
            None,
            "{} — not undoable; deletes all cached stems in the app's stem cache (nothing else there or anywhere). Refused while a split is under way, and while the open session uses cached stems (the refusal names what to do: Remove Stems on split clips, Clip List › Clear on stem files left in the Clip List, or change a Sampler's sample) or close the session. Returns {files, bytes}",
            super::always,
            clear_stem_cache
        ),
    ]
}

const MASK: &str = "clip.stem_mask";
/// Why a clip whose file changed can't be split.
const CHANGED: &str = "no longer plays what its file holds (the file changed since it was imported); import it again to split it";
const COPY: &str = "dj.stem_copy";
const UNSPLIT: &str = "clip.unsplit_stems";
const CLEAR: &str = "dj.clear_stem_cache";

/// `mask` from `p`: stem names, at least one.
fn mask_param(p: &Value, cmd: &str) -> crate::Result<[bool; 4]> {
    let mask =
        p.get("mask").ok_or_else(|| super::bad(cmd, "`mask` required: the stems to play, from \"vocals\", \"drums\", \"bass\" and \"other\""))?;
    let names = mask.as_array().ok_or_else(|| super::bad(cmd, "`mask` must be an array of stem names, such as [\"vocals\"]"))?;
    let mut mask = [false; 4];
    for name in names {
        let i = name.as_str().and_then(|n| STEMS.iter().position(|s| s.eq_ignore_ascii_case(n.trim())));
        let i = i.ok_or_else(|| super::bad(cmd, format!("{name} is not a stem: use \"vocals\", \"drums\", \"bass\" or \"other\"")))?;
        if let Some(on) = mask.get_mut(i) {
            *on = true;
        }
    }
    if !mask.contains(&true) {
        return Err(super::bad(cmd, "the mask is empty: name at least one stem"));
    }
    Ok(mask)
}

/// The names of the stems `mask` plays.
fn mask_names(mask: [bool; 4]) -> Vec<&'static str> {
    STEMS.iter().zip(mask).filter(|(_, on)| *on).map(|(n, _)| *n).collect()
}

/// The split clip `clip`: refused when it isn't split.
fn split_clip<'a>(s: &'a Session, clip: ClipId, cmd: &str) -> crate::Result<(soundcraft_model::TrackId, &'a soundcraft_model::Clip)> {
    let (t, c) = s.find_clip(clip).ok_or_else(|| super::bad(cmd, format!("no clip {}", clip.0)))?;
    if c.stems.is_none() {
        return Err(super::bad(cmd, format!("`{}` is not split: split the clip into stems first", c.name)));
    }
    Ok((t, c))
}

fn stem_mask(e: &mut Engine, p: &Value) -> crate::Result<Value> {
    let clip = super::clip_id(p, MASK)?;
    let mask = mask_param(p, MASK)?;
    // The same mask again changes nothing (and makes no undo step).
    if split_clip(e.session(), clip, MASK)?.1.stem_mask != mask
        && let Some(c) = e.session_mut().find_clip_mut(clip)
    {
        c.stem_mask = mask;
    }
    Ok(json!({"clip": clip.0, "mask": mask_names(mask)}))
}

fn stem_copy(e: &mut Engine, p: &Value) -> crate::Result<Value> {
    let clip = super::clip_id(p, COPY)?;
    let mask = mask_param(p, COPY)?;
    let track = super::track_param(e, COPY, p, "track")?.ok_or_else(|| super::bad(COPY, "`track` required: the audio track to copy to"))?;
    let s = e.session();
    let (_, c) = split_clip(s, clip, COPY)?;
    let t = s.track(track).ok_or_else(|| super::bad(COPY, format!("no track {}", track.0)))?;
    if t.kind != soundcraft_model::TrackKind::Audio {
        return Err(super::bad(COPY, format!("`{}` is not an audio track", t.name)));
    }
    if let Some(o) = t.clips().iter().find(|o| o.range().overlaps(&c.range())) {
        return Err(super::bad(COPY, format!("the copy would overlap `{}` on `{}`; move it or choose another track", o.name, t.name)));
    }
    let suffix = match mask {
        [true, false, false, false] => " (acapella)",
        [false, true, true, true] => " (instrumental)",
        _ => " (stems)",
    };
    let mut copy = c.clone();
    // A copy of a copy is named for its own mask only.
    let base = [" (acapella)", " (instrumental)", " (stems)"].iter().find_map(|s| c.name.strip_suffix(s)).unwrap_or(&c.name);
    copy.name = format!("{base}{suffix}");
    copy.stem_mask = mask;
    copy.group = None;
    copy.loop_id = None;
    let s = e.session_mut();
    copy.id = s.new_clip_id();
    let (id, name) = (copy.id, copy.name.clone());
    crate::edit::place_clip(s, track, copy);
    Ok(json!({"clip": id.0, "track": track.0, "name": name, "mask": mask_names(mask)}))
}

fn unsplit_stems(e: &mut Engine, p: &Value) -> crate::Result<Value> {
    let clip = super::clip_id(p, UNSPLIT)?;
    let s = e.session();
    let (_, c) = split_clip(s, clip, UNSPLIT)?;
    let (Some(src), Some(set)) = (c.source(), c.stems.clone()) else {
        return Err(super::bad(UNSPLIT, format!("clip {} is not an audio clip", clip.0)));
    };
    let shared = same_track(s, src);
    let ours = |c: &soundcraft_model::Clip| c.source().is_some_and(|x| shared.contains(&x)) && c.stems.as_ref() == Some(&set);
    let s = e.session_mut();
    let (mut clips, mut kept) = (Vec::new(), Vec::new());
    for c in s.tracks.iter_mut().flat_map(|t| t.playlists.iter_mut()).flat_map(|p| p.clips.iter_mut()) {
        if !ours(c) {
            continue;
        }
        // The clip asked for goes back to its audio; a mashup copy (a stem mask) elsewhere keeps
        // its stems.
        if c.id == clip || c.stem_mask == [true; 4] {
            c.unsplit();
            clips.push(c.id.0);
        } else {
            kept.push(c.id.0);
        }
    }
    // The stem files go when no clip, Session view slot or Sampler plays them any more.
    let used: Vec<SourceId> = s.used_sources().collect();
    let gone: Vec<SourceId> = set.sources.iter().copied().filter(|id| !used.contains(id)).collect();
    s.sources.retain(|x| !gone.contains(&x.id));
    for id in &gone {
        s.pool.remove(*id);
    }
    s.edit.selected_sources.retain(|x| !gone.contains(x));
    Ok(json!({"clips": clips, "kept": kept, "sources": gone.len()}))
}

fn clear_stem_cache(e: &mut Engine, _: &Value) -> crate::Result<Value> {
    let cache = setup(e, CLEAR)?.cache;
    clear_stem_cache_in(e, &cache)
}

/// `dj.clear_stem_cache` on `cache` (the app's own, or a test's): refused while the session
/// names a file in a known stem cache.
pub fn clear_stem_cache_in(e: &Engine, cache: &StemCache) -> crate::Result<Value> {
    if let Some(why) = clear_stem_cache_refusal(e, cache) {
        return Err(super::bad(CLEAR, why));
    }
    let (files, bytes) = clear(cache).map_err(EngineError::Io)?;
    Ok(json!({"files": files, "bytes": bytes}))
}

/// Why `dj.clear_stem_cache` on `cache` would be refused now, if it would: a split into it is
/// under way, or the session plays cached stems. Lets a confirm say so before asking.
pub fn clear_stem_cache_refusal(e: &Engine, cache: &StemCache) -> Option<String> {
    if live_split(cache.root()) {
        return Some("a split into stems is under way: wait for it to finish (or cancel it), then clear the stem cache".into());
    }
    cache_users(e.session()).map(|why| format!("the session uses cached stems: {why}; or close the session"))
}

/// What in `s` plays a file in a known stem cache, and what to do about it; `None` when nothing.
fn cache_users(s: &Session) -> Option<String> {
    let cached: Vec<SourceId> = s.sources.iter().filter(|x| cached_stem(Path::new(&x.path)).is_some()).map(|x| x.id).collect();
    if cached.is_empty() {
        return None;
    }
    let count = |n: usize, what: &str| if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") };
    let split = s.all_clips().filter(|c| c.stems.as_ref().is_some_and(|set| set.sources.iter().any(|x| cached.contains(x)))).count();
    let playing = s.all_clips().filter(|c| c.source().is_some_and(|x| cached.contains(&x))).count();
    let slots = s.live.slots().filter(|(_, _, slot)| slot.audio_sources().any(|x| cached.contains(&x))).count();
    let samplers: Vec<&str> = s
        .tracks
        .iter()
        .filter(|t| t.instrument.as_ref().and_then(|i| i.sample).is_some_and(|x| cached.contains(&x)))
        .map(|t| t.name.as_str())
        .collect();
    let mut why = Vec::new();
    if split > 0 {
        why.push(format!("{} — remove the stems from them first (Remove Stems)", count(split, "split clip")));
    }
    if playing > 0 {
        why.push(format!("{} playing a stem file — remove them first", count(playing, "clip")));
    }
    if slots > 0 {
        why.push(format!("{} in the Session view playing stems — clear them first", count(slots, "slot")));
    }
    for t in &samplers {
        why.push(format!("the Sampler on `{t}` plays a stem file — change the Sampler's sample"));
    }
    if why.is_empty() {
        why.push(format!("{} left in the Clip List — Clip List › Clear those files", count(cached.len(), "stem file")));
    }
    Some(why.join("; "))
}

/// The caches of the splits under way (one entry per split, from its start until its result is
/// applied or dropped).
static LIVE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn live() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    LIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether a split under way uses the cache at `root`.
fn live_split(root: &Path) -> bool {
    let key = path_key(root);
    live().iter().any(|r| path_key(r) == key)
}

/// Marks a split under way in a cache for as long as it lives: carried by the [`SplitJob`],
/// then by its [`Split`] until that is applied or dropped.
struct LiveSplit(PathBuf);

impl LiveSplit {
    fn new(root: &Path) -> Self {
        live().push(root.to_path_buf());
        Self(root.to_path_buf())
    }
}

impl Drop for LiveSplit {
    fn drop(&mut self) {
        let mut roots = live();
        if let Some(i) = roots.iter().position(|r| r == &self.0) {
            roots.remove(i);
        }
    }
}

/// The caches splits used in this process, most recent first: with [`default_root`], the only
/// folders sessions may read stems from outside the automation roots.
static USED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn used() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    USED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Notes that a split uses the cache at `root`.
pub(crate) fn use_root(root: &Path) {
    let key = path_key(root);
    let mut roots = used();
    roots.retain(|r| path_key(r) != key);
    roots.insert(0, root.to_path_buf());
}

/// The caches stems are read from: those set up on an engine ([`Engine::set_stems`]) or used by
/// a split here, most recent first. Never a cache nobody set up, not even [`default_root`].
fn known_roots() -> Vec<PathBuf> {
    used().clone()
}

/// The stem cache and Demucs runner an engine splits with ([`Engine::set_stems`]).
#[derive(Clone)]
pub struct StemSetup {
    pub cache: StemCache,
    pub runner: StemRunner,
}

/// Why a split or a clear can't run on an engine without a stem cache.
const NO_CACHE: &str = "no stem cache is set up";

/// The engine's stem cache and runner, or the refusal of `cmd` without them.
fn setup(e: &Engine, cmd: &str) -> crate::Result<StemSetup> {
    e.stems().cloned().ok_or_else(|| super::bad(cmd, NO_CACHE))
}

/// `path` with `.` and `..` resolved by name, when that names a stem in a known stem cache:
/// `<root>/<key>/<stem>.flac`. Only such a file (read at that resolved path) is read outside the
/// automation roots when a session names it.
pub(crate) fn cached_stem(path: &Path) -> Option<PathBuf> {
    let p = super::prep::normalise(path);
    let file = p.file_name()?.to_str()?;
    let entry = p.parent()?;
    let key = entry.file_name()?.to_str()?;
    let root = entry.parent()?;
    let known = is_key(key) && STEMS.iter().any(|s| file == format!("{s}.flac")) && known_roots().iter().any(|r| path_key(r) == path_key(root));
    known.then_some(p)
}

/// The cache holding complete entry `key`, its stems and their `scale_log2`.
fn cached(key: &str) -> Option<(StemCache, [PathBuf; 4], u8)> {
    known_roots().into_iter().map(StemCache::new).find_map(|c| c.hit(key).map(|(paths, k)| (c, paths, k)))
}

/// Four stems decoded at the session's rate, each with its file's own rate.
type Loaded = [(Arc<SourceAudio>, u32); 4];

/// The stems at `paths` decoded at `rate`.
fn load_stems(paths: &[PathBuf; 4], rate: u32) -> Result<Loaded, String> {
    let loaded = cache_io(|| paths.iter().map(|p| io::decode_exact_file(p, rate)).collect::<Result<Vec<_>, String>>())?;
    Loaded::try_from(loaded).map_err(|_| "not four stems".to_string())
}

/// Stems a finished split decoded already, handed to the `dj.attach_stems` it runs.
struct Prepared {
    /// The clip's audio when the split started.
    origin: Origin,
    /// The cache the split used.
    cache: StemCache,
    paths: [PathBuf; 4],
    scale_log2: u8,
    loaded: Loaded,
    /// `loaded`, each fitted on the worker to the length its clip's source had then (a stem
    /// resampled to the session's rate can end a few frames off), so applying doesn't copy it.
    fitted: Fitted,
}

/// Stems a split fitted to its source's length: `None` where a stem needed no fitting or
/// couldn't be fitted.
type Fitted = [Option<Arc<SourceAudio>>; 4];

/// `loaded` fitted to `frames` frames of `channels` channels, where a stem was resampled to
/// `rate` and ends within [`LENGTH_SLACK`] frames of that.
fn fit(loaded: &Loaded, rate: u32, frames: u64, channels: usize) -> Fitted {
    let fit_one = |(audio, file_rate): &(Arc<SourceAudio>, u32)| {
        let len = audio.buffer.frames() as u64;
        if *file_rate == rate || len == frames || len.abs_diff(frames) > LENGTH_SLACK as u64 || audio.buffer.num_channels() != channels {
            return None;
        }
        let mut buffer = audio.buffer.clone();
        let n = usize::try_from(frames).ok()?;
        buffer.channels.iter_mut().for_each(|c| c.resize(n, 0.0));
        Some(Arc::new(SourceAudio::new(buffer)))
    };
    let [a, b, c, d] = loaded;
    [fit_one(a), fit_one(b), fit_one(c), fit_one(d)]
}

thread_local! {
    static PREPARED: std::cell::RefCell<Option<Prepared>> = const { std::cell::RefCell::new(None) };
}

/// A clip's audio when its split started: its source, the audio playing (held weakly, so it is
/// never mistaken for audio allocated later in its place) and its file, with size and time.
#[derive(Clone)]
struct Origin {
    source: SourceId,
    audio: std::sync::Weak<SourceAudio>,
    file: PathBuf,
    stamp: (u64, u64),
}

impl Origin {
    fn of(s: &Session, source: SourceId, file: &Path) -> Self {
        let audio = s.pool.get(source).map(Arc::downgrade).unwrap_or_default();
        Self { source, audio, file: file.to_path_buf(), stamp: io::saved_stamp(file) }
    }

    /// Whether source `src` of `s` is still this audio, from this unchanged file.
    fn matches(&self, s: &Session, src: SourceId) -> bool {
        let same_audio = || self.audio.upgrade().is_some_and(|a| s.pool.get(src).is_some_and(|b| Arc::ptr_eq(&a, b)));
        let same_file = || source_file(s, src, ATTACH).is_ok_and(|f| path_key(&f) == path_key(&self.file) && io::saved_stamp(&f) == self.stamp);
        src == self.source && same_audio() && same_file()
    }
}

/// Whether source `id`'s audio is what its file holds now: a file referenced in place unchanged
/// since it was decoded or saved (or, changed, still decoding to the same audio). A copy (not
/// saved, or resampled) can't be compared and counts.
fn plays_its_file(s: &Session, id: SourceId) -> bool {
    match plays_its_file_now(s, id) {
        Ok(known) => known,
        Err((audio, file)) => io::file_holds(&audio, &file),
    }
}

/// [`plays_its_file`] when that is known without decoding the file; otherwise the audio and the
/// file to compare (by decoding, which can take a while).
fn plays_its_file_now(s: &Session, id: SourceId) -> Result<bool, (Arc<SourceAudio>, PathBuf)> {
    let Some(src) = s.source(id) else { return Ok(false) };
    if src.unsaved {
        return Ok(true);
    }
    let (Some(audio), Some(file)) = (s.pool.get(id), file_of(s, id)) else { return Ok(false) };
    if (src.file_len != 0 && io::saved_stamp(&file) == (src.file_len, src.file_modified_ms)) || io::decoded_unchanged(audio, &file) {
        return Ok(true);
    }
    Err((Arc::clone(audio), file))
}

/// The file source `id` of `s` plays, by name: absolute, or in the session's folder once saved.
fn file_of(s: &Session, id: SourceId) -> Option<PathBuf> {
    let src = s.source(id)?;
    let stored = Path::new(&src.path);
    if stored.is_absolute() {
        Some(stored.to_path_buf())
    } else if src.unsaved {
        None
    } else {
        s.folder.as_deref().map(|f| f.join(stored))
    }
}

/// A source of `s` already playing the stem at `path`, at `rate`, `frames` long and `channels`
/// wide: attaching the same stems again reuses it, so one file stays one source.
fn stem_source(s: &Session, path: &Path, rate: u32, frames: u64, channels: usize, scale_log2: u8) -> Option<SourceId> {
    let want = path_key(path);
    // Played at another level by a split clip: not the same stems.
    let other_scale = |id: SourceId| {
        let other =
            |set: &Option<soundcraft_model::StemSet>| set.as_ref().is_some_and(|set| set.sources.contains(&id) && set.scale_log2 != scale_log2);
        s.all_clips().any(|c| other(&c.stems)) || s.live.slots().any(|(_, _, slot)| other(&slot.stems))
    };
    s.sources.iter().filter(|x| !other_scale(x.id)).find_map(|x| {
        let audio = s.pool.get(x.id)?;
        let fits = audio.buffer.sample_rate == rate && audio.buffer.frames() as u64 == frames && audio.buffer.num_channels() == channels;
        let named = Path::new(&x.path).is_absolute() && path_key(Path::new(&x.path)) == want;
        // A stem referenced in place must still be what the file holds (an entry made again
        // holds new stems at the same path).
        (fits && named && (x.unsaved || io::decoded_unchanged(audio, path))).then_some(x.id)
    })
}

/// The audio file of source `id`, which must be there and readable.
fn source_file(s: &Session, id: SourceId, cmd: &str) -> crate::Result<PathBuf> {
    let src = s.source(id).ok_or_else(|| super::bad(cmd, "the clip's source is missing from the session"))?;
    let stored = Path::new(&src.path);
    let save_first = || super::bad(cmd, format!("`{}` is not in a file yet; save the session first", src.name));
    let file = if stored.is_absolute() {
        stored.to_path_buf()
    } else if src.unsaved {
        return Err(save_first());
    } else {
        s.folder.as_deref().map(|f| f.join(stored)).ok_or_else(save_first)?
    };
    crate::file_access::check_read(&file).map_err(EngineError::Io)?;
    if !io::file_found(&file) {
        return Err(EngineError::Io(format!("{}: the clip's audio file is not there; relink it first", file.display())));
    }
    Ok(file)
}

/// The audio clip `clip` of `s` and its source.
fn audio_clip<'a>(s: &'a Session, clip: ClipId, cmd: &str) -> crate::Result<(&'a soundcraft_model::Clip, SourceId)> {
    let (_, c) = s.find_clip(clip).ok_or_else(|| super::bad(cmd, format!("no clip {}", clip.0)))?;
    let src = c.source().ok_or_else(|| super::bad(cmd, format!("clip {} is not an audio clip", clip.0)))?;
    Ok((c, src))
}

fn already(clip: ClipId, key: &str) -> Value {
    json!({"clip": clip.0, "already": true, "key": key})
}

fn attach_stems(e: &mut Engine, p: &Value) -> crate::Result<Value> {
    let clip = super::clip_id(p, ATTACH)?;
    let key = super::str_param(p, "key").ok_or_else(|| super::bad(ATTACH, "`key` required: the stem cache entry dj.split_stems returned"))?;
    let prepared = PREPARED.with(|p| p.borrow_mut().take());
    attach(e, clip, key, prepared)
}

/// Gives `clip`, and every other unsplit clip of its source, the stems of cache entry `key`,
/// decoded (`prepared`, when a split loaded them already and their files are unchanged).
/// Stems that don't load are removed from the cache, so the next split makes them again.
fn attach(e: &mut Engine, clip: ClipId, key: &str, prepared: Option<Prepared>) -> crate::Result<Value> {
    let s = e.session();
    let (c, src) = audio_clip(s, clip, ATTACH)?;
    if let Some(set) = &c.stems {
        return Ok(already(clip, &set.key));
    }
    if !is_key(key) {
        return Err(super::bad(ATTACH, format!("`{key}` is not a stem cache key")));
    }
    // The cache the split used, else any this process knows.
    // A split's own stems, already loaded, still attach when their entry went meanwhile
    // (another app cleared the cache): saving then copies them into the session.
    let found = match &prepared {
        Some(x) => x.cache.hit(key).map(|(paths, k)| (x.cache.clone(), paths, k)).or_else(|| {
            let gone = x.paths.iter().all(|p| !cache_io(|| io::path_exists(p)));
            gone.then(|| (x.cache.clone(), x.paths.clone(), x.scale_log2))
        }),
        None => cached(key),
    };
    let (cache, paths, scale_log2) =
        found.ok_or_else(|| super::bad(ATTACH, format!("no stems `{key}` in the stem cache; split the clip with dj.split_stems")))?;
    let rate = s.sample_rate.hz();
    let source = s.source(src).ok_or_else(|| super::bad(ATTACH, "the clip's source is missing from the session"))?;
    let (frames, channels, name) = (source.frames, usize::from(source.channels), source.name.clone());
    // The stems must be this clip's file's: known when the clip still plays the audio its split
    // started from, else checked by hashing the file.
    if !plays_its_file(s, src) {
        return Err(super::bad(
            ATTACH,
            format!("`{name}` no longer plays what its file holds (the file changed since it was imported); import it again to split it"),
        ));
    }
    if !prepared.as_ref().is_some_and(|x| x.origin.matches(s, src)) {
        let file = source_file(s, src, ATTACH)?;
        if key_of(&io::read_file(&file)?) != key {
            return Err(super::bad(ATTACH, format!("`{key}` holds the stems of another file than {}", file.display())));
        }
    }
    // Stems the session plays already (another clip of the file split before) are reused.
    let reused: Vec<Option<SourceId>> = paths.iter().map(|p| stem_source(s, p, rate, frames, channels, scale_log2)).collect();
    // Stems the worker fitted to this length already, when they are the ones used below.
    let pre_fitted = prepared.as_ref().map(|x| x.fitted.clone());
    let mut from_prepared = false;
    let loaded = if reused.iter().all(Option::is_some) {
        None
    } else {
        let gone = paths.iter().all(|p| !cache_io(|| io::path_exists(p)));
        let fresh = |l: &Loaded| l.iter().zip(&paths).all(|((a, _), p)| a.buffer.sample_rate == rate && (gone || io::decoded_unchanged(a, p)));
        Some(match prepared.filter(|x| x.paths == paths && fresh(&x.loaded)) {
            Some(x) => {
                from_prepared = true;
                x.loaded
            }
            None => load_stems(&paths, rate).map_err(|m| {
                let removed = match cache.remove(key) {
                    Ok(()) => "they were removed from the stem cache, so splitting again makes them anew",
                    Err(_) => "they could not be removed from the stem cache",
                };
                EngineError::Io(format!("the cached stems would not load ({m}); {removed}"))
            })?,
        })
    };
    let mut fitted = Vec::with_capacity(STEMS.len());
    for (i, ((stem, path), reuse)) in STEMS.iter().zip(&paths).zip(&reused).enumerate() {
        if let Some(id) = reuse {
            fitted.push(Ok(*id));
            continue;
        }
        let Some((audio, file_rate)) = loaded.as_ref().and_then(|l| l.get(i)).cloned() else {
            return Err(EngineError::Internal(ATTACH.into(), "a stem was not loaded".into()));
        };
        let (len, width) = (audio.buffer.frames() as u64, audio.buffer.num_channels());
        if width != channels || (file_rate == rate && len != frames) || len.abs_diff(frames) > LENGTH_SLACK as u64 {
            return Err(EngineError::Io(format!(
                "stem {stem} of `{key}` is {len} frames, {width} channels; the clip's source is {frames} frames, {channels} channels"
            )));
        }
        // Resampled stems can end a few frames off the resampled source.
        let ready = pre_fitted.as_ref().filter(|_| from_prepared).and_then(|f| f.get(i).cloned().flatten());
        let audio = if len == frames {
            audio
        } else if let Some(ready) = ready.filter(|f| f.buffer.frames() as u64 == frames && f.buffer.num_channels() == channels) {
            ready
        } else {
            let mut buffer = audio.buffer.clone();
            let n = usize::try_from(frames).unwrap_or(usize::MAX);
            buffer.channels.iter_mut().for_each(|c| c.resize(n, 0.0));
            Arc::new(SourceAudio::new(buffer))
        };
        fitted.push(Err((format!("{name} ({stem})"), path.clone(), audio, file_rate)));
    }
    let s = e.session_mut();
    let ids: Vec<SourceId> = fitted
        .into_iter()
        .map(|f| match f {
            Ok(id) => id,
            Err((name, path, audio, file_rate)) => io::add_file_source(s, &name, &path, audio, FileFormat::Flac, file_rate),
        })
        .collect();
    let sources = <[SourceId; 4]>::try_from(ids).map_err(|_| EngineError::Internal(ATTACH.into(), "not four stems".into()))?;
    let set = StemSet { key: key.to_string(), sources, scale_log2 };
    let shared: Vec<SourceId> = same_track(s, src).into_iter().filter(|id| !sources.contains(id)).collect();
    // A clip joined by its file's name gets the stems only when its audio is the file split:
    // the same width and length, and what the file holds now.
    let fits = |id: SourceId| {
        id == src
            || s.source(id).is_some_and(|x| usize::from(x.channels) == channels && x.frames.abs_diff(frames) <= LENGTH_SLACK as u64)
                && plays_its_file(s, id)
    };
    let unfit: Vec<SourceId> = shared.iter().copied().filter(|id| !fits(*id)).collect();
    let (mut clips, mut skipped) = (Vec::new(), Vec::new());
    for c in s.tracks.iter_mut().flat_map(|t| t.playlists.iter_mut()).flat_map(|p| p.clips.iter_mut()) {
        if !c.source().is_some_and(|id| shared.contains(&id)) {
            continue;
        }
        match &c.stems {
            None if c.source().is_some_and(|id| unfit.contains(&id)) => {
                skipped.push(json!({"clip": c.id.0, "reason": "its audio is not what the split file holds now"}));
            }
            None => {
                c.stems = Some(set.clone());
                clips.push(c.id.0);
            }
            Some(other) if other.key != key => skipped.push(json!({"clip": c.id.0, "key": other.key})),
            Some(_) => {}
        }
    }
    Ok(json!({"clip": clip.0, "key": key, "scale_log2": scale_log2, "clips": clips, "skipped": skipped}))
}

/// The sources of `s` that are the same track as `src`: `src` itself, the same audio (the file
/// imported again in place), or the same file by name (imported again and resampled to the
/// session's rate).
fn same_track(s: &Session, src: SourceId) -> Vec<SourceId> {
    let audio = s.pool.get(src).cloned();
    let file = file_of(s, src).map(|f| path_key(&f));
    let same = |id: SourceId| {
        id == src
            || audio.as_ref().is_some_and(|a| s.pool.get(id).is_some_and(|b| Arc::ptr_eq(a, b)))
            || file.as_ref().is_some_and(|f| file_of(s, id).is_some_and(|g| path_key(&g) == *f))
    };
    s.sources.iter().map(|x| x.id).filter(|id| same(*id)).collect()
}

fn split_stems(e: &mut Engine, p: &Value) -> crate::Result<Value> {
    let clip = super::clip_id(p, SPLIT)?;
    if let Some(set) = audio_clip(e.session(), clip, SPLIT)?.0.stems.as_ref() {
        return Ok(already(clip, &set.key));
    }
    let StemSetup { cache, runner } = setup(e, SPLIT)?;
    let Some(job) = split_job_in(e, p, cache, runner)? else {
        // Checked above: the clip is not split.
        return Err(EngineError::Internal(SPLIT.into(), "the clip was split already".into()));
    };
    let split = job.wait()?;
    let prepared = split.prepared();
    let attached = attach(e, clip, &split.files.key, Some(prepared))?;
    Ok(split.report(attached))
}

/// How far a split got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitProgress {
    /// [`DECODING`], [`SEPARATING`], [`ENCODING`] or [`LOADING`].
    pub stage: &'static str,
}

/// What a running split and the job watching it share.
struct SplitShared {
    cancel: AtomicBool,
    stage: Mutex<&'static str>,
}

impl SplitShared {
    fn set(&self, stage: &'static str) {
        *self.stage.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = stage;
    }

    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// A finished split, not yet applied to the session: its stems, decoded.
pub struct Split {
    clip: ClipId,
    origin: Origin,
    /// The automation roots in force when the split started.
    roots: Option<Arc<crate::file_access::AutomationRoots>>,
    /// Marks the split under way until it is applied or dropped.
    live: Option<Arc<LiveSplit>>,
    cache: StemCache,
    files: StemFiles,
    loaded: Loaded,
    fitted: Fitted,
    started: std::time::Instant,
}

impl Split {
    /// The clip it splits.
    pub fn clip(&self) -> ClipId {
        self.clip
    }

    /// The stems in the cache.
    pub fn files(&self) -> &StemFiles {
        &self.files
    }

    /// Gives the clip (and the other unsplit clips of its source) the stems, as one undoable
    /// `dj.attach_stems`. Returns what `dj.split_stems` does.
    pub fn apply(self, e: &mut Engine) -> crate::Result<Value> {
        let params = json!({"clip": self.clip.0, "key": self.files.key});
        let prepared = self.prepared();
        PREPARED.with(|p| *p.borrow_mut() = Some(prepared));
        // Under the automation roots in force when the split started (when there were any), so
        // a split an agent started never reads more when the app applies it.
        let attached = crate::file_access::confine(self.roots.as_ref(), || e.execute(ATTACH, &params));
        PREPARED.with(|p| p.borrow_mut().take());
        Ok(self.report(attached?))
    }

    fn prepared(&self) -> Prepared {
        Prepared {
            origin: self.origin.clone(),
            cache: self.cache.clone(),
            paths: self.files.paths.clone(),
            scale_log2: self.files.scale_log2,
            loaded: self.loaded.clone(),
            fitted: self.fitted.clone(),
        }
    }

    /// `attached` (what `dj.attach_stems` returned) with whether the stems were cached and how
    /// long the split took.
    fn report(&self, mut attached: Value) -> Value {
        if let Some(o) = attached.as_object_mut().filter(|o| !o.contains_key("already")) {
            o.insert("hit".into(), json!(self.files.hit));
            o.insert("seconds".into(), json!(self.started.elapsed().as_secs_f64()));
        }
        attached
    }
}

/// A `dj.split_stems` split on its own thread: its stage, a cancel (which stops Demucs), and
/// the finished [`Split`] to apply.
pub struct SplitJob {
    shared: Arc<SplitShared>,
    clip: ClipId,
    /// Marks the split under way; handed to its [`Split`].
    live: Option<Arc<LiveSplit>>,
    state: SplitState,
}

enum SplitState {
    Running(std::thread::JoinHandle<Result<Split, String>>),
    Ended(Option<Box<Result<Split, String>>>),
}

/// Starts the split `dj.split_stems` makes for `p`, in the engine's stem cache with its runner
/// ([`Engine::set_stems`]; refused without them): see [`split_job_in`].
pub fn split_job(e: &Engine, p: &Value) -> crate::Result<Option<SplitJob>> {
    let StemSetup { cache, runner } = setup(e, SPLIT)?;
    split_job_in(e, p, cache, runner)
}

/// Starts the split `dj.split_stems` makes for `p` (`{clip}`) in `cache`, separating with
/// `runner`. The clip and its audio file are checked here; `None` when the clip is split
/// already. The split, and decoding the stems, happen on another thread; the session is not
/// touched until [`Split::apply`]. `cache` is the app's own folder (or a test's), never one a
/// command names.
pub fn split_job_in(e: &Engine, p: &Value, cache: StemCache, runner: StemRunner) -> crate::Result<Option<SplitJob>> {
    let clip = super::clip_id(p, SPLIT)?;
    let s = e.session();
    let (c, src) = audio_clip(s, clip, SPLIT)?;
    if c.stems.is_some() {
        return Ok(None);
    }
    let file = source_file(s, src, SPLIT)?;
    let changed = || super::bad(SPLIT, format!("`{}` {CHANGED}", c.name));
    // When only decoding the file tells, the worker does it.
    let check = match plays_its_file_now(s, src) {
        Ok(true) => None,
        Ok(false) => return Err(changed()),
        Err(compare) => Some(compare),
    };
    let name = c.name.clone();
    let origin = Origin::of(s, src, &file);
    // Held by the job, its result and the worker, so the cache counts as in use until all end.
    let live = Arc::new(LiveSplit::new(cache.root()));
    let worker_live = Arc::clone(&live);
    // The roots in force now also apply when the split is applied.
    let roots = crate::file_access::current();
    let rate = s.sample_rate.hz();
    let (frames, channels) = s.source(src).map_or((0, 0), |x| (x.frames, usize::from(x.channels)));
    use_root(cache.root());
    let shared = Arc::new(SplitShared { cancel: AtomicBool::new(false), stage: Mutex::new(DECODING) });
    let started = std::time::Instant::now();
    let work = {
        let shared = Arc::clone(&shared);
        move || {
            let _live = worker_live;
            if let Some((audio, file)) = check
                && !io::file_holds(&audio, &file)
            {
                return Err(format!("`{name}` {CHANGED}"));
            }
            separate(&cache, &file, &runner, &shared, rate).map(|(files, loaded)| Split {
                clip,
                origin,
                roots: roots.clone(),
                cache,
                files,
                fitted: fit(&loaded, rate, frames, channels),
                loaded,
                started,
                live: None,
            })
        }
    };
    let live = Some(live);
    if cfg!(target_arch = "wasm32") {
        return Ok(Some(SplitJob { shared: Arc::clone(&shared), clip, live, state: SplitState::Ended(Some(Box::new(work()))) }));
    }
    // The worker reads the source under the automation roots in force here.
    let roots = crate::file_access::current();
    let worker = move || match &roots {
        Some(r) => crate::file_access::confine(Some(r), work),
        None => crate::file_access::unconfined(work),
    };
    match std::thread::Builder::new().name("split-stems".into()).spawn(worker) {
        Ok(handle) => Ok(Some(SplitJob { shared: Arc::clone(&shared), clip, live, state: SplitState::Running(handle) })),
        Err(err) => Err(EngineError::Io(format!("could not start the split: {err}"))),
    }
}

/// Splits `file` into `cache` and decodes the stems at `rate`. An entry whose stems don't
/// decode counts as a miss: it is removed and the file split again, once.
fn separate(cache: &StemCache, file: &Path, runner: &StemRunner, shared: &Arc<SplitShared>, rate: u32) -> Result<(StemFiles, Loaded), String> {
    let staged: StemRunner = {
        let (shared, runner) = (Arc::clone(shared), Arc::clone(runner));
        Arc::new(move |input: &Path, out: &Path, stop: &dyn Fn() -> bool| {
            shared.set(SEPARATING);
            let r = runner(input, out, stop);
            shared.set(ENCODING);
            r
        })
    };
    let stop = || shared.stopped();
    let mut failed = None;
    for _ in 0..2 {
        shared.set(DECODING);
        let files = split(cache, file, &staged, &stop)?;
        if stop() {
            return Err(CANCELLED.to_string());
        }
        shared.set(LOADING);
        match load_stems(&files.paths, rate) {
            // A cancel while the stems loaded still stops the split.
            Ok(_) if stop() => return Err(CANCELLED.to_string()),
            Ok(loaded) => return Ok((files, loaded)),
            Err(m) => {
                cache.remove(&files.key)?;
                if !files.hit {
                    return Err(format!("the new stems would not load: {m}"));
                }
                failed = Some(m);
            }
        }
    }
    Err(format!("the stems would not load: {}", failed.unwrap_or_default()))
}

/// A thread's result, or its panic as an error.
fn joined(handle: std::thread::JoinHandle<Result<Split, String>>) -> Result<Split, String> {
    handle.join().unwrap_or_else(|p| Err(format!("the split stopped: {}", crate::panic_message(&*p))))
}

/// A split's error as the engine reports it.
fn failed(msg: String) -> EngineError {
    if msg == CANCELLED { EngineError::Io(CANCELLED.to_string()) } else { EngineError::Io(format!("{SPLIT}: {msg}")) }
}

impl Drop for SplitJob {
    /// A job dropped before it ended stops its split (Demucs is killed).
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }
}

impl SplitJob {
    /// The clip it splits.
    pub fn clip(&self) -> ClipId {
        self.clip
    }

    /// What it is doing.
    pub fn progress(&self) -> SplitProgress {
        SplitProgress { stage: *self.shared.stage.lock().unwrap_or_else(std::sync::PoisonError::into_inner) }
    }

    /// Stops the split (Demucs is killed); it then ends with [`CANCELLED`] and the session stays
    /// as it was.
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether it was cancelled.
    pub fn cancelling(&self) -> bool {
        self.shared.stopped()
    }

    /// The finished split once it ended, given once; `None` before.
    pub fn poll(&mut self) -> Option<crate::Result<Split>> {
        if let SplitState::Running(handle) = &self.state
            && !handle.is_finished()
        {
            return None;
        }
        let r = match std::mem::replace(&mut self.state, SplitState::Ended(None)) {
            SplitState::Running(handle) => Some(joined(handle)),
            SplitState::Ended(r) => r.map(|r| *r),
        };
        r.map(|r| self.unless_cancelled(r))
    }

    /// `r`, or [`CANCELLED`] when the job was cancelled before its result was taken: a split
    /// that finished as Cancel was pressed is not applied.
    fn unless_cancelled(&mut self, r: Result<Split, String>) -> crate::Result<Split> {
        match r {
            Ok(_) if self.cancelling() => Err(failed(CANCELLED.to_string())),
            Ok(mut split) => {
                split.live = self.live.take();
                Ok(split)
            }
            Err(m) => Err(failed(m)),
        }
    }

    /// Waits for the split to end and gives it.
    pub fn wait(mut self) -> crate::Result<Split> {
        let r = match std::mem::replace(&mut self.state, SplitState::Ended(None)) {
            SplitState::Running(handle) => joined(handle),
            SplitState::Ended(r) => r.map_or_else(|| Err("the result was taken already".to_string()), |r| *r),
        };
        self.unless_cancelled(r)
    }
}

/// Whether `path` is a drive's or the file system's root (or empty), which [`clear`] never empties.
fn is_top(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::Normal(_)))
}

/// The cached stems in `cache` (its entries, not the folders of splits under way): how many
/// files and their bytes, what [`clear`] would remove of them. `(0, 0)` when there is no cache.
pub fn stem_cache_size(cache: &StemCache) -> Result<(usize, u64), String> {
    cache_io(|| {
        if !io::path_exists(&cache.root) {
            return Ok((0, 0));
        }
        let (mut files, mut bytes) = (0, 0u64);
        for entry in io::list_entries(&cache.root).map_err(|e| e.to_string())? {
            if entry.file_name().and_then(|n| n.to_str()).is_some_and(is_key) {
                let (n, b) = io::tree_size(&entry).map_err(|e| e.to_string())?;
                files += n;
                bytes = bytes.saturating_add(b);
            }
        }
        Ok((files, bytes))
    })
}

/// Empties the stem cache: removes the entries (folders named like keys) and the temporary
/// folders of splits (`.tmp-<key>-<pid>-<n>`, except splits under way: this process's, and
/// another process's while it runs) directly
/// in its root, and nothing else. A folder without the cache's [`MARKER`], or a drive's root, is
/// refused. Links are removed, never followed. Returns the files removed and their bytes.
pub fn clear(cache: &StemCache) -> Result<(usize, u64), String> {
    let root = &cache.root;
    if is_top(root) {
        return Err(format!("{}: a drive's root is never cleared as a stem cache", root.display()));
    }
    cache_io(|| {
        if !io::path_exists(root) {
            return Ok((0, 0));
        }
        if !io::path_exists(root.join(MARKER)) {
            return Err(format!("{}: not a stem cache (it has no {MARKER} file); nothing was removed", root.display()));
        }
        let busy: Vec<String> = running().iter().map(|p| path_key(p)).collect();
        let (mut files, mut bytes, mut failed) = (0, 0u64, Vec::new());
        for entry in io::list_entries(root).map_err(|e| e.to_string())? {
            let Some(name) = entry.file_name().and_then(|n| n.to_str()) else { continue };
            let live = |pid: u32| if pid == std::process::id() { busy.contains(&path_key(&entry)) } else { pid_running(pid) };
            if !(is_key(name) || tmp_pid(name).is_some_and(|pid| !live(pid))) {
                continue;
            }
            match io::remove_tree(&entry) {
                Ok((n, b)) => {
                    files += n;
                    bytes = bytes.saturating_add(b);
                }
                Err(e) => failed.push(e.to_string()),
            }
        }
        if failed.is_empty() { Ok((files, bytes)) } else { Err(format!("removed {files} files; could not remove: {}", failed.join("; "))) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_the_hash_size_and_model() {
        let key = key_of(b"abc");
        assert_eq!(key, "e71fa2190541574b-3-htdemucs");
        assert!(is_key(&key));
        for not in
            ["e71fa2190541574b-3-other", "e71fa2190541574-3-htdemucs", "E71FA2190541574B-3-htdemucs", "e71fa2190541574b--htdemucs", "x", ".tmp-a"]
        {
            assert!(!is_key(not), "{not}");
        }
    }

    #[test]
    fn temporary_folders_name_a_key_a_process_and_a_count() {
        assert_eq!(tmp_pid(".tmp-e71fa2190541574b-3-htdemucs-1234-0"), Some(1234));
        for not in [
            ".tmp-e71fa2190541574b-3-htdemucs-1234",
            ".tmp-e71fa2190541574b-3-htdemucs--0",
            ".tmp-e71fa2190541574b-3-htdemucs-12x-0",
            ".tmp-notakey-1-0",
            ".tmp-",
            "e71fa2190541574b-3-htdemucs-1234-0",
            ".tmp-e71fa2190541574b-3-htdemucs-99999999999-0",
        ] {
            assert_eq!(tmp_pid(not), None, "{not}");
        }
    }

    #[test]
    fn headroom_brings_the_peak_a_bit_under_full_scale() {
        for (peak, bits) in [(0.0, 1), (0.3, 1), (1.0, 1), (1.01, 2), (2.0, 2), (2.5, 3), (f32::NAN, 1), (f32::INFINITY, 1), (1e30, 30)] {
            assert_eq!(headroom(peak), bits, "{peak}");
        }
        for peak in [0.3f32, 1.0, 1.5, 3.9, 100.0] {
            assert!(peak * 0.5f32.powi(i32::from(headroom(peak))) <= 0.5, "{peak}");
        }
    }

    #[test]
    fn demucs_gets_three_times_the_audio_and_at_least_ten_minutes() {
        assert_eq!(demucs_timeout(3.0), Duration::from_secs(600));
        assert_eq!(demucs_timeout(f64::NAN), Duration::from_secs(600));
        assert_eq!(demucs_timeout(-5.0), Duration::from_secs(600));
        assert_eq!(demucs_timeout(1_000.2), Duration::from_secs(3_001));
    }

    #[test]
    fn bash_words_are_quoted_but_keep_the_home_tilde() {
        assert_eq!(bash_word(r"C:\Users\it's here"), r"'C:\Users\it'\''s here'");
        assert_eq!(bash_word("~/venv/bin/python"), "~/'venv/bin/python'");
        assert_eq!(bash_word("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn timeout_exits_count_as_timed_out() {
        assert!(timed_out(Some(124)) && timed_out(Some(137)));
        assert!(!timed_out(Some(1)) && !timed_out(Some(0)) && !timed_out(None));
    }

    #[test]
    fn wslpath_gets_paths_without_the_verbatim_prefix() {
        assert_eq!(for_wslpath(Path::new(r"\\?\C:\t\x.wav")).unwrap(), r"C:\t\x.wav");
        assert_eq!(for_wslpath(Path::new(r"C:\it's\x.wav")).unwrap(), r"C:\it's\x.wav");
        assert!(for_wslpath(Path::new(r"\\?\UNC\server\share\x.wav")).is_err());
    }

    #[test]
    fn inside_compares_by_name() {
        let tmp = std::env::temp_dir().join("c").join(".tmp-x");
        assert!(inside(&tmp.join("out").join("a.wav"), &tmp));
        assert!(!inside(&tmp, &tmp));
        assert!(!inside(&tmp.join("..").join("y.wav"), &tmp));
        assert!(!inside(&std::env::temp_dir().join("c").join(".tmp-xy").join("a.wav"), &tmp));
        let mut trailing = tmp.clone().into_os_string();
        trailing.push(std::path::MAIN_SEPARATOR_STR);
        assert!(inside(&tmp.join("a.wav"), Path::new(&trailing)));
        if cfg!(windows) {
            let upper = PathBuf::from(tmp.display().to_string().to_uppercase());
            assert!(inside(&tmp.join("a.wav"), &upper));
            let verbatim = PathBuf::from(format!(r"\\?\{}", tmp.display()));
            assert!(inside(&verbatim.join("a.wav"), &tmp));
        }
    }

    #[test]
    fn kill_patterns_match_the_folder_name_literally() {
        assert_eq!(regex_literal(".tmp-0123456789abcdef-1-htdemucs-9-0"), r"\.tmp-0123456789abcdef-1-htdemucs-9-0");
        assert_eq!(regex_literal("a(b)*"), r"a\(b\)\*");
    }

    #[test]
    fn drive_roots_are_never_a_stem_cache() {
        let tops: &[&str] = if cfg!(windows) { &[r"C:\", "C:", r"\\?\C:\", r"\", ""] } else { &["/", ""] };
        for top in tops {
            assert!(is_top(Path::new(top)), "{top}");
            assert!(clear(&StemCache::new(*top)).unwrap_err().contains("never cleared"), "{top}");
        }
        assert!(!is_top(&std::env::temp_dir()));
    }

    #[test]
    fn the_free_space_of_the_temp_drive_is_known() {
        let free = free_space(&std::env::temp_dir());
        assert!(free.is_some_and(|f| f > 0), "{free:?}");
    }

    #[test]
    fn this_process_is_running() {
        assert!(pid_running(std::process::id()));
    }

    /// No test anywhere in the workspace builds the app's real stem cache: the incident of a
    /// test run emptying it must never repeat. Only the apps' entry points call `at_default`.
    #[test]
    fn no_test_reaches_the_real_stem_cache() {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() && !matches!(name.as_str(), "target" | ".git" | "node_modules") {
                    walk(&path, out);
                } else if name.ends_with(".rs") {
                    out.push(path);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let mut files = Vec::new();
        for top in ["crates", "apps"] {
            walk(&root.join(top), &mut files);
        }
        assert!(files.len() > 50, "{}", files.len());
        let forbidden = ["at_default(", "StemCache::new(default_root", "StemCache::new(stems::default_root", "default_stem_cache"];
        let mut offenders = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let in_tests_dir = f.components().any(|c| c.as_os_str() == "tests");
            let test_code = if in_tests_dir { text.as_str() } else { text.split_once("#[cfg(test)]").map_or("", |(_, t)| t) };
            for word in forbidden {
                if test_code.lines().any(|l| l.contains(word) && !l.trim_start().starts_with("//") && !l.contains("let forbidden")) {
                    offenders.push(format!("{}: {word}", f.display()));
                }
            }
        }
        assert!(offenders.is_empty(), "tests must use a temporary stem cache: {offenders:#?}");
    }

    #[test]
    fn the_default_root_is_outside_the_music_folders() {
        let root = default_root().unwrap();
        assert!(root.ends_with(Path::new("djforge").join("stems")), "{}", root.display());
        assert!(!root.components().any(|c| c.as_os_str().eq_ignore_ascii_case("Music")), "{}", root.display());
    }
}
