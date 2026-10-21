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

use crate::io;
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode, probe};
use soundcraft_model::STEMS;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicU64, Ordering};
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
fn pid_running(pid: u32) -> bool {
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

/// Whether `path` is a drive's or the file system's root (or empty), which [`clear`] never empties.
fn is_top(path: &Path) -> bool {
    !path.components().any(|c| matches!(c, Component::Normal(_)))
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

    #[test]
    fn the_default_root_is_outside_the_music_folders() {
        let root = default_root().unwrap();
        assert!(root.ends_with(Path::new("djforge").join("stems")), "{}", root.display());
        assert!(!root.components().any(|c| c.as_os_str().eq_ignore_ascii_case("Music")), "{}", root.display());
    }
}
