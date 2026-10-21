//! The stem cache: a source file split into vocals, drums, bass and other by Demucs (`htdemucs`,
//! run in WSL on Windows), stored once per file content as 24-bit FLAC at the source's own rate
//! and length, so the stems line up with it sample for sample.
//!
//! An entry is `<root>/<key>/{vocals,drums,bass,other}.flac` plus `stems.json`, built in a
//! `.tmp-*` folder beside it and renamed into place, so a half-built entry never looks finished.
//! The root defaults to `%LOCALAPPDATA%\djforge\stems`, never inside the music folders.

use crate::io;
use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode, probe};
use soundcraft_model::STEMS;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The Demucs model the cache holds stems of; part of every key.
pub const MODEL: &str = "htdemucs";
/// The error a split stopped part-way gives.
pub const CANCELLED: &str = "cancelled";
/// Free space a split needs on the cache's drive by default.
pub const MIN_FREE: u64 = 2 << 30;
/// Most a stem's length may differ from the source's, in frames, to be padded or trimmed to it.
pub const LENGTH_SLACK: usize = 64;
/// The entry's description, next to its stems.
const INDEX: &str = "stems.json";
/// How the temporary folders of splits under way are named.
const TMP_PREFIX: &str = ".tmp-";
/// The WSL python with Demucs when `DJFORGE_DEMUCS_PYTHON` is not set.
const DEFAULT_PYTHON: &str = "~/mixforge-wsl/venv/bin/python";
/// Longest Demucs may take over one file.
const DEMUCS_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Longest the free-space query may take.
const FREE_SPACE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a cached FLAC read to check its rate and length.
const HEAD: usize = 64 << 10;

/// Separates the WAV at the first path into the four stems inside the output folder (the second
/// path), stopping when the check says so. Returns the four WAVs in [`STEMS`] order.
pub type StemRunner = Arc<dyn Fn(&Path, &Path, &dyn Fn() -> bool) -> Result<[PathBuf; 4], String> + Send + Sync>;

/// Free bytes on the drive holding a folder, `None` when that can't be learned.
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

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

    /// The four stems of entry `key`, if it is complete and intact: its `stems.json` describes
    /// this key and model, and names each stem's FLAC, which has the recorded size, rate and
    /// length.
    fn hit(&self, key: &str) -> Option<[PathBuf; 4]> {
        let dir = self.entry(key);
        let index: Value = serde_json::from_str(&io::read_text(dir.join(INDEX)).ok()?).ok()?;
        let (rate, frames) = (index.get("sample_rate")?.as_u64()?, index.get("frames")?.as_u64()?);
        if index.get("key")?.as_str()? != key || index.get("model")?.as_str()? != MODEL {
            return None;
        }
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
        Some(paths)
    }
}

/// A source's stems in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StemFiles {
    /// The cache entry: the source's content hash, size and the model.
    pub key: String,
    /// The FLACs in [`STEMS`] order.
    pub paths: [PathBuf; 4],
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

/// Whether `name` is a cache entry's folder name (a key).
fn is_key(name: &str) -> bool {
    let mut parts = name.splitn(3, '-');
    let (Some(hash), Some(size), Some(model)) = (parts.next(), parts.next(), parts.next()) else { return false };
    hash.len() == 16
        && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !size.is_empty()
        && size.bytes().all(|b| b.is_ascii_digit())
        && model == MODEL
}

/// Tells temporary folders of splits running at once in this process apart.
static TMP_COUNT: AtomicU64 = AtomicU64::new(0);

/// A split's temporary folder, removed with everything in it when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = io::remove_tree(&self.0);
    }
}

/// The stems of the audio file at `source_path` from the cache, splitting it with `runner` when
/// they are not there yet (or the entry is damaged). `stop` cancels the split ([`CANCELLED`]).
pub fn split(cache: &StemCache, source_path: &Path, runner: &StemRunner, stop: &dyn Fn() -> bool) -> Result<StemFiles, String> {
    let bytes = io::read_file(source_path).map_err(|e| e.to_string())?;
    let (key, size) = (key_of(&bytes), bytes.len());
    if let Some(paths) = cache.hit(&key) {
        return Ok(StemFiles { key, paths, hit: true });
    }
    let cancelled = || if stop() { Err(CANCELLED.to_string()) } else { Ok(()) };
    cancelled()?;
    io::make_dir(&cache.root).map_err(|e| e.to_string())?;
    check_free_space(cache)?;
    let ext = source_path.extension().and_then(|x| x.to_str());
    let (_, source) = decode(&bytes, ext).map_err(|e| format!("{}: {e}", source_path.display()))?;
    drop(bytes);
    let (frames, rate, channels) = (source.frames(), source.sample_rate, source.num_channels());
    if frames == 0 || channels == 0 || rate == 0 {
        return Err(format!("{}: holds no audio to split", source_path.display()));
    }
    cancelled()?;
    let n = TMP_COUNT.fetch_add(1, Ordering::Relaxed);
    let tmp = TempDir(cache.root.join(format!("{TMP_PREFIX}{key}-{}-{n}", std::process::id())));
    io::make_dir(&tmp.0).map_err(|e| e.to_string())?;
    let input = tmp.0.join("source.wav");
    let wav = encode(&source, &EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None })
        .map_err(|e| format!("could not write the WAV for Demucs: {e}"))?;
    drop(source);
    io::write_path(&input, &wav).map_err(|e| e.to_string())?;
    drop(wav);
    let out = tmp.0.join("out");
    let wavs = runner(&input, &out, stop)?;
    cancelled()?;
    let mut stems = Vec::with_capacity(STEMS.len());
    for (name, wav) in STEMS.iter().zip(&wavs) {
        cancelled()?;
        let stem = read_stem(name, wav, rate, channels, frames)?;
        let flac = encode(&stem, &EncodeOptions { format: FileFormat::Flac, bit_depth: BitDepth::Int24, dither: false, bwf: None })
            .map_err(|e| format!("could not encode stem {name}: {e}"))?;
        let file = format!("{name}.flac");
        io::write_path(tmp.0.join(&file), &flac).map_err(|e| e.to_string())?;
        stems.push(json!({"name": name, "file": file, "bytes": flac.len()}));
    }
    // Only the stems and their description go into the entry.
    for work in [&input, &out] {
        io::remove_tree(work).map_err(|e| e.to_string())?;
    }
    let index = json!({
        "source_path": source_path.display().to_string(),
        "size": size,
        "key": key,
        "model": MODEL,
        "sample_rate": rate,
        "frames": frames,
        "created": super::prep::utc_now(),
        "stems": stems,
    });
    let text = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
    io::write_path(tmp.0.join(INDEX), text).map_err(|e| e.to_string())?;
    cancelled()?;
    let dir = cache.entry(&key);
    if io::path_exists(&dir) {
        // Another split of the same file finished first: keep its entry. A damaged one goes.
        if let Some(paths) = cache.hit(&key) {
            return Ok(StemFiles { key, paths, hit: false });
        }
        io::remove_tree(&dir).map_err(|e| e.to_string())?;
    }
    io::rename_path(&tmp.0, &dir).map_err(|e| e.to_string())?;
    let paths = STEMS.map(|name| dir.join(format!("{name}.flac")));
    Ok(StemFiles { key, paths, hit: false })
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
    command.stdin(std::process::Stdio::null());
    hide_window(&mut command);
    let out = match super::dj::output_within(&mut command, FREE_SPACE_TIMEOUT, &|| false) {
        Ok(super::dj::Sidecar::Ran(out)) if out.status.success() => out,
        _ => return None,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if cfg!(windows) {
        text.trim().parse().ok()
    } else {
        // `Filesystem 1024-blocks Used Available Capacity Mounted-on`, then the drive's line.
        let kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
        kb.checked_mul(1024)
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

/// The Windows path `path` as WSL sees it: `C:\a\b` is `/mnt/c/a/b`.
pub fn wsl_path(path: &Path) -> Result<String, String> {
    let shown = || path.display().to_string();
    let mut parts = path.components();
    let drive = match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => char::from(d).to_ascii_lowercase(),
            _ => return Err(format!("{}: WSL reaches only paths on a drive letter", shown())),
        },
        _ => return Err(format!("{}: not a full Windows path", shown())),
    };
    let mut names = Vec::new();
    for part in parts {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => names.push(name.to_str().ok_or_else(|| format!("{}: not a Unicode path", shown()))?),
            // `..` at the drive's root stays there.
            Component::ParentDir => {
                names.pop();
            }
            Component::Prefix(_) => return Err(format!("{}: not a full Windows path", shown())),
        }
    }
    Ok(names.iter().fold(format!("/mnt/{drive}"), |out, name| out + "/" + name))
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

/// The python with Demucs: `DJFORGE_DEMUCS_PYTHON` (a WSL path on Windows) or
/// `~/mixforge-wsl/venv/bin/python`.
fn demucs_python() -> String {
    std::env::var("DJFORGE_DEMUCS_PYTHON").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| DEFAULT_PYTHON.to_string())
}

/// The runner that separates with Demucs `htdemucs` (float32 WAVs), through
/// `wsl -e bash -lc` on Windows and the python itself elsewhere, killed after 10 minutes or when
/// asked to stop.
pub fn demucs_runner() -> StemRunner {
    Arc::new(run_demucs)
}

fn run_demucs(input: &Path, out: &Path, stop: &dyn Fn() -> bool) -> Result<[PathBuf; 4], String> {
    let python = demucs_python();
    let mut command = if cfg!(windows) {
        let line =
            format!("{} -m demucs -n {MODEL} --float32 -o {} {}", bash_word(&python), bash_word(&wsl_path(out)?), bash_word(&wsl_path(input)?));
        let mut c = std::process::Command::new("wsl");
        c.args(["-e", "bash", "-lc", &line]);
        c
    } else {
        let python = match python.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").map_or_else(|| PathBuf::from(&python), |h| PathBuf::from(h).join(rest)),
            None => PathBuf::from(&python),
        };
        let mut c = std::process::Command::new(python);
        c.args(["-m", "demucs", "-n", MODEL, "--float32", "-o"]).arg(out).arg(input);
        c
    };
    command.stdin(std::process::Stdio::null());
    hide_window(&mut command);
    let ran = match super::dj::output_within(&mut command, DEMUCS_TIMEOUT, stop) {
        Ok(super::dj::Sidecar::Ran(ran)) => ran,
        Ok(super::dj::Sidecar::Stopped) => return Err(CANCELLED.to_string()),
        Ok(super::dj::Sidecar::TimedOut) => return Err(format!("Demucs timed out after {} minutes", DEMUCS_TIMEOUT.as_secs() / 60)),
        Ok(super::dj::Sidecar::LeftHolding) => return Err("Demucs left a process holding its output".to_string()),
        Err(err) => return Err(format!("could not run Demucs ({python}): {err}")),
    };
    let tail = || {
        let stderr = String::from_utf8_lossy(&ran.stderr);
        let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
        lines.get(lines.len().saturating_sub(5)..).unwrap_or_default().join("\n")
    };
    if !ran.status.success() {
        return Err(format!("Demucs failed ({}): {}", ran.status, tail()));
    }
    let stem_of = input.file_stem().ok_or_else(|| format!("{}: not a file name", input.display()))?;
    let dir = out.join(MODEL).join(stem_of);
    let paths = STEMS.map(|name| dir.join(format!("{name}.wav")));
    if let Some(missing) = paths.iter().find(|p| !io::file_found(p)) {
        return Err(format!("Demucs wrote no {}: {}", missing.display(), tail()));
    }
    Ok(paths)
}

/// Empties the stem cache: removes the entries (folders named like keys) and leftover `.tmp-*`
/// folders directly in its root, and nothing else. Returns the files removed and their bytes.
pub fn clear(cache: &StemCache) -> Result<(usize, u64), String> {
    if !io::path_exists(&cache.root) {
        return Ok((0, 0));
    }
    let (mut files, mut bytes, mut failed) = (0, 0u64, Vec::new());
    for entry in io::list_entries(&cache.root).map_err(|e| e.to_string())? {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else { continue };
        if !(is_key(name) || name.starts_with(TMP_PREFIX)) {
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
    fn windows_paths_become_wsl_paths() {
        if !cfg!(windows) {
            return;
        }
        assert_eq!(wsl_path(Path::new(r"C:\Users\a b\x.wav")).unwrap(), "/mnt/c/Users/a b/x.wav");
        assert_eq!(wsl_path(Path::new(r"\\?\D:\t\x.wav")).unwrap(), "/mnt/d/t/x.wav");
        assert_eq!(wsl_path(Path::new(r"C:\a\..\b")).unwrap(), "/mnt/c/b");
        assert_eq!(wsl_path(Path::new(r"C:\..")).unwrap(), "/mnt/c");
        assert!(wsl_path(Path::new(r"\\server\share\x.wav")).is_err());
        assert!(wsl_path(Path::new(r"rel\x.wav")).is_err());
    }

    #[test]
    fn bash_words_are_quoted_but_keep_the_home_tilde() {
        assert_eq!(bash_word("/mnt/c/it's here"), r"'/mnt/c/it'\''s here'");
        assert_eq!(bash_word("~/venv/bin/python"), "~/'venv/bin/python'");
        assert_eq!(bash_word("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn the_free_space_of_the_temp_drive_is_known() {
        let free = free_space(&std::env::temp_dir());
        assert!(free.is_some_and(|f| f > 0), "{free:?}");
    }

    #[test]
    fn the_default_root_is_outside_the_music_folders() {
        let root = default_root().unwrap();
        assert!(root.ends_with(Path::new("djforge").join("stems")), "{}", root.display());
        assert!(!root.components().any(|c| c.as_os_str().eq_ignore_ascii_case("Music")), "{}", root.display());
    }
}
