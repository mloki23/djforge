//! The library the decks load from (`library.*`): mixforge's library, read only, and folders of
//! audio files.
//!
//! mixforge's `library.db` holds each analysed file's grid, key and length (see
//! `soundcraft_library`), and Mixed In Key's `MIKStore.db` its key and energy (MIK's win, as
//! mixforge has it). The app names them ([`Engine::set_library`], [`Engine::set_mik_library`]);
//! nothing else does, so no test reaches the real ones. They are read off the UI thread
//! ([`Engine::poll_library`]) and kept until their files change and then stand still for a
//! moment (mixforge may be writing); listings meanwhile give the last read's rows.
//!
//! A folder lists its audio files without decoding them: mixforge's analysis joins them by path,
//! and the tags of the files a page returns fill in what it lacks. The folders offered as roots
//! are the user's Music folder and the folders they add (kept in the app's preferences).
//!
//! `deck.load` asks [`analysis_for`] first: a fresh row gives the grid, key and mix points with
//! no sidecar run.
//!
//! A listing looks at the files it returns (are they there, changed, their tags) and a folder's
//! is read from disk: on a network drive or a sleeping disk that takes a while, so the app runs
//! them off the UI thread ([`Engine::start_library_query`]); the commands run them in place.

use super::*;
use crate::cmd;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The audio files a folder lists (by extension, any case).
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "aif", "aiff", "m4a", "ogg"];
/// Rows a page holds unless `limit` says otherwise, and at most.
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
/// Entries of one folder listed at most (of each kind: files, subfolders).
pub const MAX_FOLDER_ENTRIES: usize = 20_000;
/// How far a track's tempo may be from the deck's, as a fraction, to be compatible.
pub const BPM_TOLERANCE: f64 = 0.06;

const LIST: &str = "library.list";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "library.list",
            "Library List",
            [],
            None,
            "{source?, folder?, query?, bpm_min?, bpm_max?, keys?, energy_min?, energy_max?, status?, sort?, desc?, offset?, limit?} — tracks to load on the decks: mixforge's library (read only; what it analysed) or the audio files (mp3, wav, flac, aiff, m4a, ogg) directly in a folder, joined to mixforge's analysis by path. source?: mixforge|folder (default mixforge), folder? (for source folder), query? (words that must all be in the title, artist or file name, any case), bpm_min?/bpm_max? (the track's own tempo), keys? (Camelot keys, any of them: \"8A\" or [\"8A\", \"9A\"]), energy_min?/energy_max? (1–10), status?: any|analysed|unanalysed (a folder's files mixforge hasn't analysed; tracks lacking a value a filter asks about are left out), sort?: title|artist|bpm|key|energy|length|name (default title; key in Camelot order; unknowns last), desc?: bool, offset? (default 0), limit? (default 200, at most 1000). Returns {source, total, offset, rows: [{path, name, title, artist, bpm, key (Camelot), key_source: mik|mixforge|tag|null, energy (1–10), energy_source: mik|mixforge|tag|null, length_s, analysed, stale (the file changed since mixforge analysed it), outdated (analysed by another mixforge version: it analyses the file again), missing (no file there)}], from_copy (the library was busy: a copy was read), loading (the libraries are being read again off the UI thread: these rows are the last read's; ask again), library_error, mik_error}, and for a folder {folder, folders: [{path, name}] (its subfolders), truncated}. Key and energy: Mixed In Key's (when the app set it up), else mixforge's, else the MP3's tags (MIK's comment `8A - Energy 6`, EnergyLevel, TKEY). A folder's files aren't decoded: sort, search and the rest use the analysis and the file names, and the tags of the rows returned fill in their title, artist, bpm, key and energy. A file reached by another path than mixforge analysed it under (a mapped drive and its network path) is listed as not analysed",
            always,
            list
        ),
        cmd!(
            query "library.search",
            "Library Search",
            [],
            None,
            "{query, source?, folder?, sort?, desc?, offset?, limit?} — library.list with `query` required: the tracks whose title, artist or file name hold every word of it, any case",
            always,
            search
        ),
        cmd!(
            query "library.compatible",
            "Library Compatible Tracks",
            [],
            None,
            "{deck: a|b, source?, folder?, query?, sort?, desc?, offset?, limit?} — the tracks that mix with the deck's: key the same in Camelot, one step round the wheel (12A↔1A) or its relative (8A↔8B), and tempo within 6 % of the deck's now (its pitch included), or of half or double it. Without a key on the deck only the tempo counts. Rows (as library.list's) are best first (same key, then ±1, then relative; then the nearer tempo) unless `sort`, each with {why, key_match: same|adjacent|relative|null, bpm_ratio: 1|2|0.5 (what the track's tempo is played at), bpm_diff_pct}; also {deck: {key, bpm}}",
            always,
            compatible
        ),
        cmd!(
            query "library.folders",
            "Library Folders",
            [],
            None,
            "{} — the folders the library browser starts from: the Music folder and the ones added (library.add_folder), each {path, name, kind: music|added, found}, and mixforge's library {path, found} (null when the app hasn't set it up)",
            always,
            folders
        ),
        cmd!(
            query "library.health",
            "Library Health",
            [],
            None,
            "{folders?: bool (default true: also look through the browser's folders), limit?: 1..=10000 (default 100: entries listed per kind; the counts are complete)} — a background job (returns {job}; see job.status): what in the library needs looking after. Read only: it never changes a file or tag. Result {counts: {tracks, missing, stale, outdated, unanalysed, duplicates, key_disagreements, lacking}, missing (mixforge has a track whose file is gone), stale (the file changed since mixforge analysed it; a tag edit such as Mixed In Key's counts, and re-analysing settles it), outdated (analysed by another mixforge version), unanalysed (audio files in the folders mixforge hasn't analysed), duplicates ([{artist, title, paths}]: the same artist and title more than once), key_disagreements ([{path, title, mik, mixforge}]), lacking ([{path, title, lacks: [bpm|key|energy]}]), to_analyze (the stale, outdated and unanalysed files: give them to library.analyze), truncated (kinds with more than `limit`)}. Progress {stage, done, total}",
            always,
            health
        ),
        cmd!(
            noundo "library.add_folder",
            "Add Library Folder",
            [],
            None,
            "{path} — adds a folder to the library browser's roots (kept in the app's preferences, on this computer). It must exist",
            always,
            add_folder
        ),
        cmd!(
            noundo "library.remove_folder",
            "Remove Library Folder",
            [],
            None,
            "{path} — takes a folder added with library.add_folder off the library browser's roots (the folder itself is left alone)",
            always,
            remove_folder
        ),
    ]
}

// ---- the database --------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
mod db {
    pub use soundcraft_library::{ANALYSIS_VERSION, MikSong, Stamp, StructureRow, TrackRow, TrackSummary, path_key};
    use std::path::Path;

    pub fn stamp(db: &Path) -> Option<Stamp> {
        soundcraft_library::stamp_of(db)
    }

    pub fn summaries(db: &Path) -> Result<(Vec<TrackSummary>, bool), String> {
        soundcraft_library::Library::new(db).summaries().map(|r| (r.value, r.from_copy)).map_err(|e| e.to_string())
    }

    pub fn track(db: &Path, path: &str) -> Result<Option<(TrackRow, Option<StructureRow>)>, String> {
        soundcraft_library::Library::new(db).track(path).map(|r| r.value).map_err(|e| e.to_string())
    }

    pub fn mik(db: &Path) -> Result<Vec<MikSong>, String> {
        soundcraft_library::mik_songs(db).map(|r| r.value).map_err(|e| e.to_string())
    }
}

/// The web has no files: no library.
#[cfg(target_arch = "wasm32")]
mod db {
    use std::path::Path;

    pub const ANALYSIS_VERSION: i64 = 9;
    pub type Stamp = ();

    #[derive(Default)]
    pub struct TrackSummary {
        pub path: String,
        pub size: Option<i64>,
        pub mtime: Option<i64>,
        pub version: Option<i64>,
        pub title: Option<String>,
        pub artist: Option<String>,
        pub bpm: Option<f64>,
        pub camelot: Option<String>,
        pub key: Option<String>,
        pub energy: Option<f64>,
        pub duration: Option<f64>,
    }

    pub struct TrackRow {
        pub path: String,
        pub size: Option<i64>,
        pub mtime: Option<i64>,
        pub version: Option<i64>,
        pub data: String,
    }

    pub struct StructureRow {
        pub size: Option<i64>,
        pub mtime: Option<i64>,
        pub data: String,
    }

    pub struct MikSong {
        pub file: String,
        pub energy: Option<i64>,
        pub camelot: Option<String>,
    }

    pub fn path_key(path: &str) -> String {
        path.to_string()
    }

    pub fn stamp(_: &Path) -> Option<Stamp> {
        None
    }

    const NONE: &str = "libraries can't be read on the web";

    pub fn summaries(_: &Path) -> Result<(Vec<TrackSummary>, bool), String> {
        Err(NONE.into())
    }

    pub fn track(_: &Path, _: &str) -> Result<Option<(TrackRow, Option<StructureRow>)>, String> {
        Err(NONE.into())
    }

    pub fn mik(_: &Path) -> Result<Vec<MikSong>, String> {
        Err(NONE.into())
    }
}

pub(crate) use db::path_key;

/// How long the library's files must stand still (mixforge busy writing) before they are read
/// again.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(1000);
/// How often [`Engine::poll_library`] looks at the files.
const LOOK_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// A track as the library lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryTrack {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub bpm: Option<f64>,
    /// Camelot.
    pub key: Option<String>,
    pub energy: Option<f64>,
    pub length_s: Option<f64>,
    /// mixforge analysed it.
    pub analysed: bool,
    /// Where the key and energy came from: `mik` (Mixed In Key), `mixforge`, `tag`.
    pub key_source: Option<&'static str>,
    pub energy_source: Option<&'static str>,
    /// mixforge's own key (Camelot), whatever Mixed In Key says.
    mixforge_key: Option<String>,
    /// mixforge's analysis version.
    version: Option<i64>,
    /// The file's size and time when it was analysed.
    size: Option<i64>,
    mtime: Option<i64>,
}

impl LibraryTrack {
    /// A file nothing knows about yet: its name for a title.
    fn plain(path: String) -> Self {
        LibraryTrack {
            title: file_stem(&path),
            path,
            artist: String::new(),
            bpm: None,
            key: None,
            energy: None,
            length_s: None,
            analysed: false,
            key_source: None,
            energy_source: None,
            mixforge_key: None,
            version: None,
            size: None,
            mtime: None,
        }
    }

    /// Mixed In Key's key and energy, where it has them: MIK is the reference (mixforge prefers
    /// its key too).
    fn with_mik(mut self, m: Option<&Mik>) -> Self {
        if let Some(m) = m {
            if let Some(k) = m.camelot.as_deref().and_then(soundcraft_model::camelot_of) {
                (self.key, self.key_source) = (Some(k), Some("mik"));
            }
            if let Some(en) = m.energy {
                (self.energy, self.energy_source) = (Some(en as f64), Some("mik"));
            }
        }
        self
    }
}

/// What Mixed In Key has for a file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Mik {
    pub energy: Option<i64>,
    pub camelot: Option<String>,
}

/// What the libraries held when last read.
pub(crate) struct Snapshot {
    tracks: Arc<Vec<LibraryTrack>>,
    /// Index into `tracks` by [`path_key`].
    by_key: Arc<HashMap<String, usize>>,
    /// Mixed In Key's songs by [`path_key`].
    mik: Arc<HashMap<String, Mik>>,
    from_copy: bool,
    /// Why mixforge's library, or MIK's, couldn't be read.
    error: Option<String>,
    mik_error: Option<String>,
}

/// The stamps of mixforge's library and MIK's.
type Stamps = (Option<db::Stamp>, Option<db::Stamp>);

/// A read of the libraries running on a thread of its own.
struct LibraryJob {
    stamps: Stamps,
    handle: std::thread::JoinHandle<Snapshot>,
}

/// The library's setup and what it last read.
#[derive(Default)]
pub struct LibraryState {
    db: Option<PathBuf>,
    mik_db: Option<PathBuf>,
    snapshot: Option<Arc<Snapshot>>,
    /// The stamps the snapshot was read at.
    read_at: Option<Stamps>,
    /// The stamps last seen and since when.
    seen: Option<(Stamps, std::time::Instant)>,
    looked: Option<std::time::Instant>,
    job: Option<LibraryJob>,
    music: Option<PathBuf>,
    added: Vec<PathBuf>,
    /// Where the added folders are kept; `None` keeps them for this run only.
    prefs: Option<PathBuf>,
}

impl LibraryState {
    /// mixforge's library database, when the app set it up.
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub(crate) fn set_db(&mut self, db: PathBuf) {
        self.db = Some(db);
        self.forget();
    }

    pub(crate) fn set_mik(&mut self, db: PathBuf) {
        self.mik_db = Some(db);
        self.forget();
    }

    fn forget(&mut self) {
        (self.snapshot, self.read_at, self.seen) = (None, None, None);
    }

    /// The Music folder and where added folders are kept (whose folders are read now).
    pub(crate) fn set_folders(&mut self, music: Option<PathBuf>, prefs: Option<PathBuf>) {
        self.music = music;
        self.added = prefs.as_deref().map(read_folders).unwrap_or_default();
        self.prefs = prefs;
    }

    /// A read under way.
    pub(crate) fn loading(&self) -> bool {
        self.job.is_some()
    }

    /// Takes a finished read, and starts another when the files changed and then stood still
    /// for [`SETTLE`] (at once when nothing was read yet). `look` checks the files now rather
    /// than at most every [`LOOK_EVERY`].
    pub(crate) fn refresh(&mut self, look: bool) {
        if self.job.as_ref().is_some_and(|j| j.handle.is_finished())
            && let Some(job) = self.job.take()
        {
            match job.handle.join() {
                Ok(s) => {
                    self.snapshot = Some(Arc::new(s));
                    self.read_at = Some(job.stamps);
                }
                Err(_) => log::warn!("reading the library failed"),
            }
        }
        if self.db.is_none() && self.mik_db.is_none() {
            return;
        }
        let now = std::time::Instant::now();
        if !look && self.looked.is_some_and(|t| now.duration_since(t) < LOOK_EVERY) {
            return;
        }
        self.looked = Some(now);
        let stamps: Stamps = (self.db.as_deref().and_then(db::stamp), self.mik_db.as_deref().and_then(db::stamp));
        let since = match self.seen {
            Some((s, at)) if s == stamps => at,
            _ => {
                self.seen = Some((stamps, now));
                now
            }
        };
        let changed = self.read_at != Some(stamps);
        let settled = self.snapshot.is_none() || now.duration_since(since) >= SETTLE;
        if changed && settled && self.job.is_none() {
            self.start(stamps);
        }
    }

    fn start(&mut self, stamps: Stamps) {
        let (db, mik_db) = (self.db.clone(), self.mik_db.clone());
        let old = self.snapshot.clone();
        // Each library is read again only when its own files changed.
        let (db_changed, mik_changed) = match self.read_at {
            Some((d, m)) => (d != stamps.0, m != stamps.1),
            None => (true, true),
        };
        let work = move || read_snapshot(db.as_deref(), mik_db.as_deref(), old.as_deref(), db_changed, mik_changed);
        match std::thread::Builder::new().name("library-read".into()).spawn(work) {
            Ok(handle) => self.job = Some(LibraryJob { stamps, handle }),
            Err(e) => log::warn!("could not start reading the library: {e}"),
        }
    }
}

/// Reads the libraries (off the UI thread): mixforge's when `db_changed`, MIK's when
/// `mik_changed`, keeping the rest of `old`.
fn read_snapshot(db: Option<&Path>, mik_db: Option<&Path>, old: Option<&Snapshot>, db_changed: bool, mik_changed: bool) -> Snapshot {
    let (mik, mik_error) = match (mik_db, old) {
        (None, _) => (Arc::new(HashMap::new()), None),
        (Some(_), Some(o)) if !mik_changed => (Arc::clone(&o.mik), o.mik_error.clone()),
        (Some(p), _) => match db::mik(p) {
            Ok(songs) => (Arc::new(songs.into_iter().map(|s| (path_key(&s.file), Mik { energy: s.energy, camelot: s.camelot })).collect()), None),
            Err(e) => (Arc::new(HashMap::new()), Some(e)),
        },
    };
    let (rows, from_copy, error) = match (db, old) {
        (None, _) => (None, false, None),
        (Some(_), Some(o)) if !db_changed => (None, o.from_copy, o.error.clone()),
        (Some(p), _) => match db::summaries(p) {
            Ok((rows, copy)) => (Some(rows), copy, None),
            Err(e) => (Some(Vec::new()), false, Some(e)),
        },
    };
    let tracks: Vec<LibraryTrack> = match (rows, old) {
        (Some(rows), _) => rows
            .iter()
            .map(track_of)
            .map(|t| {
                let m = mik.get(&path_key(&t.path));
                t.with_mik(m)
            })
            .collect(),
        // MIK changed alone: its key and energy go onto the tracks read before.
        (None, Some(o)) if mik_changed => o.tracks.iter().map(|t| t.clone().with_mik(mik.get(&path_key(&t.path)))).collect(),
        (None, Some(o)) => return Snapshot { mik, mik_error, from_copy, error, tracks: Arc::clone(&o.tracks), by_key: Arc::clone(&o.by_key) },
        (None, None) => Vec::new(),
    };
    // One row per file: two spellings of one path (case, separators) keep the current analysis.
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<LibraryTrack> = Vec::with_capacity(tracks.len());
    for t in tracks {
        match by_key.get(&path_key(&t.path)).copied() {
            Some(i) => {
                if let Some(old) = kept.get_mut(i)
                    && old.version != Some(db::ANALYSIS_VERSION)
                    && t.version == Some(db::ANALYSIS_VERSION)
                {
                    *old = t;
                }
            }
            None => {
                by_key.insert(path_key(&t.path), kept.len());
                kept.push(t);
            }
        }
    }
    let tracks = kept;
    Snapshot { tracks: Arc::new(tracks), by_key: Arc::new(by_key), mik, from_copy, error, mik_error }
}

/// The folders kept in the preferences file at `path` (`{"folders": [...]}`); none when it
/// can't be read.
fn read_folders(path: &Path) -> Vec<PathBuf> {
    let text = crate::file_access::unconfined(|| crate::io::read_text(path)).ok();
    let v: Value = text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    v.get("folders").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default()
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

fn file_stem(path: &str) -> String {
    Path::new(path).file_stem().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

fn track_of(row: &db::TrackSummary) -> LibraryTrack {
    let key = [&row.camelot, &row.key].into_iter().flatten().find_map(|k| soundcraft_model::camelot_of(k));
    let energy = row.energy.filter(|e| e.is_finite());
    LibraryTrack {
        path: row.path.clone(),
        title: row.title.clone().unwrap_or_else(|| file_stem(&row.path)),
        artist: row.artist.clone().unwrap_or_default(),
        bpm: row.bpm.filter(|b| (20.0..=400.0).contains(b)),
        key_source: key.as_ref().map(|_| "mixforge"),
        key: key.clone(),
        energy_source: energy.map(|_| "mixforge"),
        energy,
        length_s: row.duration.filter(|d| *d >= 0.0),
        analysed: true,
        mixforge_key: key.clone(),
        version: row.version,
        size: row.size,
        mtime: row.mtime,
    }
}

/// What the libraries hold now (the last read while a new one runs), and whether a read is
/// under way. Refused when the app set no mixforge library up.
fn library(e: &mut Engine, cmd: &str) -> Result<(Arc<Snapshot>, bool)> {
    let lib = &mut e.library;
    if lib.db.is_none() {
        return Err(bad(cmd, "mixforge's library isn't set up: djforge reads it when it runs as the app"));
    }
    lib.refresh(true);
    let empty =
        || Arc::new(Snapshot { tracks: Arc::default(), by_key: Arc::default(), mik: Arc::default(), from_copy: false, error: None, mik_error: None });
    Ok((lib.snapshot.clone().unwrap_or_else(empty), lib.loading()))
}

/// What Mixed In Key has for the file at `path`, as last read.
pub(crate) fn mik_for(e: &Engine, path: &str) -> Option<Mik> {
    e.library.snapshot.as_ref().and_then(|s| s.mik.get(&path_key(path)).cloned())
}

/// Mixed In Key's Camelot key and energy in a comment it wrote (`8A - Energy 6`).
pub(crate) fn mik_comment(comment: &str) -> (Option<String>, Option<f64>) {
    let mut words = comment.split(|c: char| c.is_whitespace() || c == '-' || c == '/' || c == ',').filter(|w| !w.is_empty());
    let key = words.clone().next().and_then(|w| camelot(w).map(|(n, l)| format!("{n}{}", char::from(l))));
    let mut energy = None;
    while let Some(w) = words.next() {
        if w.eq_ignore_ascii_case("energy") {
            energy = words.next().and_then(|n| n.parse::<u8>().ok()).filter(|n| (1..=10).contains(n)).map(f64::from);
            break;
        }
    }
    (key, energy)
}

// ---- deck.load -----------------------------------------------------------------------------

/// What mixforge's library says about a file `deck.load` loads.
#[derive(Debug)]
pub(crate) enum FromLibrary {
    /// Its analysis (with `sections` from its structure row when that is fresh too).
    Fresh(Value),
    /// A row for an older state of the file.
    Stale,
    /// A row of another mixforge analysis version: mixforge analyses the file again.
    Outdated,
    Absent,
    Unreadable(String),
}

impl FromLibrary {
    pub(crate) fn id(&self) -> &'static str {
        match self {
            FromLibrary::Fresh(_) => "fresh",
            FromLibrary::Stale => "stale",
            FromLibrary::Outdated => "outdated",
            FromLibrary::Absent => "absent",
            FromLibrary::Unreadable(_) => "unreadable",
        }
    }
}

/// The analysis of the file at `path` in the library `db`, if mixforge made it of the file as it
/// is now (`stamp`: its size and time).
pub(crate) fn analysis_for(db: &Path, path: &str, stamp: Option<(u64, i64)>) -> FromLibrary {
    let (row, structure) = match db::track(db, path) {
        Ok(Some(r)) => r,
        Ok(None) => return FromLibrary::Absent,
        Err(why) => return FromLibrary::Unreadable(why),
    };
    let fresh =
        |size: Option<i64>, mtime: Option<i64>| stamp.is_some_and(|(s, m)| size.and_then(|x| u64::try_from(x).ok()) == Some(s) && mtime == Some(m));
    if !fresh(row.size, row.mtime) {
        return FromLibrary::Stale;
    }
    if row.version != Some(db::ANALYSIS_VERSION) {
        return FromLibrary::Outdated;
    }
    let Ok(mut a) = serde_json::from_str::<Value>(&row.data) else {
        return FromLibrary::Unreadable(format!("{path}: its analysis in mixforge's library isn't JSON"));
    };
    if a.get("sections").is_none_or(Value::is_null)
        && let Some(s) = structure.filter(|s| fresh(s.size, s.mtime))
        && let Ok(st) = serde_json::from_str::<Value>(&s.data)
        && let Some(sections) = sections_on_grid(&a, &st)
        && let Some(obj) = a.as_object_mut()
    {
        obj.insert("sections".into(), sections);
    }
    FromLibrary::Fresh(a)
}

/// all-in-one's section boundaries snapped to the nearest bar of mixforge's grid
/// (`structure.py`'s `sections_on_grid`, Marty's own code): `[{bar, label, bars}]`, leaving out
/// the start and end markers and any boundary not after the one before.
fn sections_on_grid(a: &Value, structure: &Value) -> Option<Value> {
    let (bpm, first) = super::dj::grid_of(a)?;
    let bar = 4.0 * 60.0 / bpm;
    let n_bars = a.get("bars").and_then(Value::as_f64).filter(|b| b.is_finite() && *b >= 0.0)?;
    let mut out: Vec<Value> = Vec::new();
    let mut last: Option<f64> = None;
    for seg in structure.get("segments").and_then(Value::as_array).into_iter().flatten() {
        let label = seg.get("label").and_then(Value::as_str).unwrap_or("");
        if matches!(label, "start" | "end") {
            continue;
        }
        let (Some(start), Some(end)) = (seg.get("start").and_then(Value::as_f64), seg.get("end").and_then(Value::as_f64)) else { continue };
        // Python's round: halves to even.
        let b = ((start - first) / bar).round_ties_even();
        if b.is_finite() && (0.0..=n_bars).contains(&b) && last.is_none_or(|l| b > l) {
            let bars = ((end - start) / bar).round_ties_even();
            out.push(json!({"bar": b as i64, "label": label, "bars": if bars.is_finite() { bars as i64 } else { 0 }}));
            last = Some(b);
        }
    }
    Some(Value::Array(out))
}

// ---- listing -------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Title,
    Artist,
    Bpm,
    Key,
    Energy,
    Length,
    Name,
}

struct Query {
    folder: Option<String>,
    words: Vec<String>,
    filter: Filter,
    sort: Option<Sort>,
    desc: bool,
    offset: usize,
    limit: usize,
}

fn query_of(cmd: &str, p: &Value) -> Result<Query> {
    let folder = match str_param(p, "source") {
        None | Some("mixforge") => None,
        Some("folder") => {
            Some(str_param(p, "folder").filter(|f| !f.is_empty()).ok_or_else(|| bad(cmd, "`folder` required for source folder"))?.to_string())
        }
        Some(_) => return Err(bad(cmd, "`source` must be mixforge or folder")),
    };
    let words = str_param(p, "query").unwrap_or("").split_whitespace().map(str::to_lowercase).collect();
    let sort = match str_param(p, "sort") {
        None => None,
        Some("title") => Some(Sort::Title),
        Some("artist") => Some(Sort::Artist),
        Some("bpm") => Some(Sort::Bpm),
        Some("key") => Some(Sort::Key),
        Some("energy") => Some(Sort::Energy),
        Some("length") => Some(Sort::Length),
        Some("name") => Some(Sort::Name),
        Some(_) => return Err(bad(cmd, "`sort` must be title, artist, bpm, key, energy, length or name")),
    };
    let desc = match p.get("desc") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(cmd, "`desc` must be true or false")),
    };
    let count = |key: &str, default: usize| -> Result<usize> {
        match p.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(v) => v.as_u64().and_then(|n| usize::try_from(n).ok()).ok_or_else(|| bad(cmd, format!("`{key}` must be a whole number 0 or more"))),
        }
    };
    let offset = count("offset", 0)?;
    let limit = count("limit", DEFAULT_LIMIT)?;
    if limit == 0 || limit > MAX_LIMIT {
        return Err(bad(cmd, format!("`limit` must be 1..={MAX_LIMIT}")));
    }
    Ok(Query { folder, words, filter: filter_of(cmd, p)?, sort, desc, offset, limit })
}

/// What `bpm_min`, `bpm_max`, `keys`, `energy_min`, `energy_max` and `status` keep.
#[derive(Default)]
struct Filter {
    bpm: Option<(f64, f64)>,
    keys: Vec<(u8, u8)>,
    energy: Option<(f64, f64)>,
    analysed: Option<bool>,
}

fn filter_of(cmd: &str, p: &Value) -> Result<Filter> {
    let n = |key: &str| super::decks::num(cmd, p, key, 0.0..=1000.0);
    let range = |lo: Option<f64>, hi: Option<f64>| match (lo, hi) {
        (None, None) => None,
        (lo, hi) => Some((lo.unwrap_or(f64::NEG_INFINITY), hi.unwrap_or(f64::INFINITY))),
    };
    let keys = match p.get("keys") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(k)) => vec![camelot(k).ok_or_else(|| bad(cmd, format!("`keys`: {k} isn't a Camelot key (1A–12B)")))?],
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_str().and_then(camelot).ok_or_else(|| bad(cmd, "`keys` must be Camelot keys (1A–12B)"))).collect::<Result<_>>()?
        }
        Some(_) => return Err(bad(cmd, "`keys` must be Camelot keys (1A–12B)")),
    };
    let analysed = match str_param(p, "status") {
        None | Some("any") => None,
        Some("analysed") => Some(true),
        Some("unanalysed") => Some(false),
        Some(_) => return Err(bad(cmd, "`status` must be any, analysed or unanalysed")),
    };
    Ok(Filter { bpm: range(n("bpm_min")?, n("bpm_max")?), keys, energy: range(n("energy_min")?, n("energy_max")?), analysed })
}

impl Filter {
    fn keeps(&self, t: &LibraryTrack) -> bool {
        let within = |v: Option<f64>, r: Option<(f64, f64)>| r.is_none_or(|(lo, hi)| v.is_some_and(|v| lo <= v && v <= hi));
        within(t.bpm, self.bpm)
            && within(t.energy, self.energy)
            && (self.keys.is_empty() || key_rank(&t.key).is_some_and(|k| self.keys.contains(&k)))
            && self.analysed.is_none_or(|a| a == t.analysed)
    }
}

fn matches(t: &LibraryTrack, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let hay = format!("{}\n{}\n{}", t.title, t.artist, file_name(&t.path)).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// A Camelot key's number (1–12) and letter (A minor, B major).
fn camelot(key: &str) -> Option<(u8, u8)> {
    let k = key.trim().to_ascii_uppercase();
    let l = k.chars().last().filter(|l| matches!(l, 'A' | 'B'))?;
    let n: u8 = k.strip_suffix(l)?.parse().ok().filter(|n| (1..=12).contains(n))?;
    Some((n, if l == 'A' { b'A' } else { b'B' }))
}

/// How track key `t` mixes with deck key `d`: `same`, `adjacent` (±1 round the wheel, the same
/// letter) or `relative` (the same number, the other letter).
fn key_match(d: (u8, u8), t: (u8, u8)) -> Option<&'static str> {
    let step = (i16::from(t.0) - i16::from(d.0)).rem_euclid(12);
    match (d.1 == t.1, step) {
        (true, 0) => Some("same"),
        (true, 1 | 11) => Some("adjacent"),
        (false, 0) => Some("relative"),
        _ => None,
    }
}

/// The ratio a track of `track` bpm plays at to meet `deck` bpm (1, 2: a half-time track, 0.5: a
/// double-time one) and how far off it then is (percent), when within the tolerance.
fn bpm_match(deck: f64, track: f64) -> Option<(f64, f64)> {
    [1.0, 2.0, 0.5]
        .into_iter()
        .map(|r| (r, (track * r - deck) / deck * 100.0))
        .filter(|(_, d)| d.is_finite() && d.abs() <= BPM_TOLERANCE * 100.0 + 1e-9)
        .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
}

fn key_rank(key: &Option<String>) -> Option<(u8, u8)> {
    key.as_deref().and_then(camelot)
}

/// Orders `rows` by `sort` (unknowns last whichever way), then title and path.
fn sort_rows(rows: &mut [LibraryTrack], sort: Sort, desc: bool) {
    use std::cmp::Ordering;
    fn opt<T: PartialOrd>(a: Option<T>, b: Option<T>, desc: bool) -> Ordering {
        match (a, b) {
            (Some(a), Some(b)) => {
                let o = a.partial_cmp(&b).unwrap_or(Ordering::Equal);
                if desc { o.reverse() } else { o }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
    let text = |s: &str| (!s.is_empty()).then(|| s.to_lowercase());
    rows.sort_by(|a, b| {
        let first = match sort {
            Sort::Title => opt(text(&a.title), text(&b.title), desc),
            Sort::Artist => opt(text(&a.artist), text(&b.artist), desc),
            Sort::Bpm => opt(a.bpm, b.bpm, desc),
            Sort::Key => opt(key_rank(&a.key), key_rank(&b.key), desc),
            Sort::Energy => opt(a.energy, b.energy, desc),
            Sort::Length => opt(a.length_s, b.length_s, desc),
            Sort::Name => opt(Some(file_name(&a.path).to_lowercase()), Some(file_name(&b.path).to_lowercase()), desc),
        };
        first.then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())).then_with(|| a.path.cmp(&b.path))
    });
}

fn is_audio(p: &Path) -> bool {
    let hidden = p.file_name().and_then(|n| n.to_str()).is_none_or(|n| n.starts_with('.'));
    !hidden && p.extension().and_then(|x| x.to_str()).is_some_and(|x| AUDIO_EXTENSIONS.iter().any(|a| x.eq_ignore_ascii_case(a)))
}

/// What a listing covers.
struct Listing {
    tracks: Vec<LibraryTrack>,
    from_copy: bool,
    /// For a folder: it, its subfolders and whether entries were left out.
    folder: Option<(String, Vec<PathBuf>, bool)>,
    /// Why mixforge's library, or Mixed In Key's, couldn't be read.
    library_error: Option<String>,
    mik_error: Option<String>,
    /// A read of the libraries is under way: what was read before is listed.
    loading: bool,
}

/// What a query needs of the engine, taken when it is asked; [`Prepared::run`] then does the
/// file reads, on any thread.
struct Prepared {
    q: Query,
    /// The libraries as last read (none: mixforge's isn't set up and a folder is listed), and
    /// whether a read is under way.
    snap: Option<Arc<Snapshot>>,
    loading: bool,
    /// `library.compatible`: the deck, its tempo and key.
    compatible: Option<(soundcraft_mix::DeckId, f64, Option<String>)>,
}

const SEARCH: &str = "library.search";
const COMPATIBLE: &str = "library.compatible";

/// Checks a `library.list`, `library.search` or `library.compatible` and takes what it needs.
fn prepare(e: &mut Engine, cmd: &str, p: &Value) -> Result<Prepared> {
    let compatible = if cmd == COMPATIBLE {
        let deck = str_param(p, "deck").and_then(soundcraft_mix::DeckId::parse).ok_or_else(|| bad(cmd, "`deck` must be a or b"))?;
        let Some(t) = e.deck_tracks.get(deck.index()).and_then(Option::as_ref) else {
            return Err(bad(cmd, format!("deck {} is empty: load a track first (deck.load)", deck.id().to_uppercase())));
        };
        let now = e.deck_status.decks.get(deck.index()).map(|s| s.bpm).filter(|b| b.is_finite() && *b > 0.0);
        let (bpm, key) = (now.or(t.bpm).filter(|b| b.is_finite() && *b > 0.0), t.key.clone());
        let Some(bpm) = bpm else {
            return Err(bad(cmd, format!("deck {}'s track has no tempo to match: load it with a bpm", deck.id().to_uppercase())));
        };
        Some((deck, bpm, key))
    } else {
        None
    };
    if cmd == SEARCH && str_param(p, "query").is_none_or(|q| q.trim().is_empty()) {
        return Err(bad(SEARCH, "`query` required: words to find in the title, artist or file name"));
    }
    let q = query_of(cmd, p)?;
    let (snap, loading) = if q.folder.is_none() {
        let (snap, loading) = library(e, cmd)?;
        (Some(snap), loading)
    } else {
        // Without mixforge's library a folder still lists (with MIK's key and energy when set up).
        e.library.refresh(true);
        (e.library.snapshot.clone(), e.library.loading())
    };
    Ok(Prepared { q, snap, loading, compatible })
}

impl Prepared {
    /// Lists, sorts and pages the query, looking at the files it returns.
    fn run(self) -> Result<Value> {
        let mut l = self.listing()?;
        let q = &self.q;
        let Some((deck, bpm, key)) = &self.compatible else {
            sort_rows(&mut l.tracks, q.sort.unwrap_or(Sort::Title), q.desc);
            let rows = page(&l.tracks, q);
            return Ok(listed(&l, q, rows));
        };
        compatible_rows(l, q, *deck, *bpm, key.clone())
    }

    fn listing(&self) -> Result<Listing> {
        let q = &self.q;
        let snap = self.snap.as_ref();
        let Some(folder) = &q.folder else {
            let tracks = snap.map(|s| s.tracks.iter().filter(|t| matches(t, &q.words) && q.filter.keeps(t)).cloned().collect()).unwrap_or_default();
            return Ok(Listing {
                tracks,
                from_copy: snap.is_some_and(|s| s.from_copy),
                folder: None,
                library_error: snap.and_then(|s| s.error.clone()),
                mik_error: snap.and_then(|s| s.mik_error.clone()),
                loading: self.loading,
            });
        };
        let (dirs, files, truncated) = crate::io::list_folder(folder, MAX_FOLDER_ENTRIES)?;
        let tracks = files
            .iter()
            .filter(|p| is_audio(p))
            .map(|p| {
                let path = p.to_string_lossy().into_owned();
                let key = path_key(&path);
                let row = snap.and_then(|s| s.by_key.get(&key).and_then(|i| s.tracks.get(*i)));
                match row {
                    Some(r) => LibraryTrack { path, ..r.clone() },
                    None => LibraryTrack::plain(path).with_mik(snap.and_then(|s| s.mik.get(&key))),
                }
            })
            .filter(|t| matches(t, &q.words) && q.filter.keeps(t))
            .collect();
        Ok(Listing {
            tracks,
            from_copy: snap.is_some_and(|s| s.from_copy),
            folder: Some((folder.clone(), dirs, truncated)),
            library_error: snap.and_then(|s| s.error.clone()),
            mik_error: snap.and_then(|s| s.mik_error.clone()),
            loading: self.loading,
        })
    }
}

/// A library query the app runs off the UI thread ([`Engine::start_library_query`]).
pub struct LibraryQuery(QueryState);

enum QueryState {
    Running(std::thread::JoinHandle<Result<Value>>),
    Done(Option<Result<Value>>),
}

impl std::fmt::Debug for LibraryQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.finished() { "LibraryQuery(finished)" } else { "LibraryQuery(running)" })
    }
}

impl LibraryQuery {
    /// Whether its answer is in.
    pub fn finished(&self) -> bool {
        match &self.0 {
            QueryState::Running(h) => h.is_finished(),
            QueryState::Done(_) => true,
        }
    }

    /// Its answer, what the command would have given (waits for it if it isn't in).
    pub fn take(self) -> Result<Value> {
        match self.0 {
            QueryState::Running(h) => h.join().unwrap_or_else(|_| Err(EngineError::Io("reading the library failed".into()))),
            QueryState::Done(r) => r.unwrap_or_else(|| Err(EngineError::Io("reading the library failed".into()))),
        }
    }
}

impl Engine {
    /// Starts `library.list`, `library.search` or `library.compatible` with `p`: checked and
    /// given what it needs from the engine now, its file reads (the folder, and each row's file
    /// and tags) on a thread of its own, so a slow disk never holds up the UI. Refused as the
    /// command would refuse it.
    pub fn start_library_query(&mut self, id: &str, p: &Value) -> Result<LibraryQuery> {
        let cmd: &'static str = match id {
            LIST => LIST,
            SEARCH => SEARCH,
            COMPATIBLE => COMPATIBLE,
            other => return Err(EngineError::UnknownCommand(other.to_string())),
        };
        let prepared = prepare(self, cmd, p)?;
        Ok(LibraryQuery(match std::thread::Builder::new().name("library-query".into()).spawn(move || prepared.run()) {
            Ok(h) => QueryState::Running(h),
            Err(e) => QueryState::Done(Some(Err(EngineError::Io(format!("could not start reading the library: {e}"))))),
        }))
    }
}

/// A listing's row, its file looked at now: missing, changed since analysed, of an outdated
/// analysis, and the tags of an MP3 filling what the analysis and Mixed In Key lack (MIK's
/// comment `8A - Energy 6`, its `EnergyLevel`, then `TKEY`).
fn row(t: &LibraryTrack) -> Value {
    let stamp = crate::io::size_and_mtime(&t.path);
    let stale = t.analysed && stamp.is_some_and(|(s, m)| t.size.and_then(|x| u64::try_from(x).ok()) != Some(s) || t.mtime != Some(m));
    let outdated = t.analysed && t.version != Some(db::ANALYSIS_VERSION);
    let mut t = t.clone();
    let is_mp3 = Path::new(&t.path).extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3"));
    let lacks = !t.analysed || t.artist.is_empty() || t.key.is_none() || t.energy.is_none();
    if stamp.is_some() && is_mp3 && lacks {
        let tags = super::prep::tag_head(Path::new(&t.path)).map(|h| soundcraft_audio_io::mp3tags::read_basic(&h)).unwrap_or_default();
        if t.artist.is_empty() {
            t.artist = tags.artist.clone().unwrap_or_default();
        }
        if !t.analysed {
            if let Some(title) = tags.title.clone() {
                t.title = title;
            }
            t.bpm = t.bpm.or(tags.bpm.filter(|b| (20.0..=400.0).contains(b)));
        }
        let (comment_key, comment_energy) = tags.comment.as_deref().map(mik_comment).unwrap_or_default();
        if t.key.is_none()
            && let Some(k) = comment_key.or_else(|| tags.key.as_deref().and_then(soundcraft_model::camelot_of))
        {
            (t.key, t.key_source) = (Some(k), Some("tag"));
        }
        if t.energy.is_none()
            && let Some(en) = comment_energy.or(tags.energy.map(f64::from))
        {
            (t.energy, t.energy_source) = (Some(en), Some("tag"));
        }
    }
    json!({"path": t.path, "name": file_name(&t.path), "title": t.title, "artist": t.artist, "bpm": t.bpm, "key": t.key,
        "key_source": t.key_source, "energy": t.energy, "energy_source": t.energy_source, "length_s": t.length_s,
        "analysed": t.analysed, "stale": stale, "outdated": outdated, "missing": stamp.is_none()})
}

fn page(rows: &[LibraryTrack], q: &Query) -> Vec<Value> {
    rows.iter().skip(q.offset).take(q.limit).map(row).collect()
}

fn listed(l: &Listing, q: &Query, rows: Vec<Value>) -> Value {
    let mut out = json!({"source": if l.folder.is_some() { "folder" } else { "mixforge" }, "total": l.tracks.len(), "offset": q.offset,
        "rows": rows, "from_copy": l.from_copy, "loading": l.loading, "library_error": l.library_error, "mik_error": l.mik_error});
    if let (Some((folder, dirs, truncated)), Some(o)) = (&l.folder, out.as_object_mut()) {
        let mut subs: Vec<(String, String)> = dirs
            .iter()
            .map(|d| (d.to_string_lossy().into_owned(), d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
            .collect();
        subs.sort_by_key(|(_, n)| n.to_lowercase());
        o.insert("folder".into(), json!(folder));
        o.insert("folders".into(), subs.into_iter().map(|(path, name)| json!({"path": path, "name": name})).collect());
        o.insert("truncated".into(), json!(truncated));
    }
    out
}

fn list(e: &mut Engine, p: &Value) -> Result<Value> {
    prepare(e, LIST, p)?.run()
}

fn search(e: &mut Engine, p: &Value) -> Result<Value> {
    prepare(e, SEARCH, p)?.run()
}

fn compatible(e: &mut Engine, p: &Value) -> Result<Value> {
    prepare(e, COMPATIBLE, p)?.run()
}

/// The tracks of `l` that mix with `deck`'s (`bpm`, `key`): ranked by key match then tempo, or
/// sorted by `q`'s sort.
fn compatible_rows(mut l: Listing, q: &Query, deck: soundcraft_mix::DeckId, bpm: f64, key: Option<String>) -> Result<Value> {
    let deck_key = key.as_deref().and_then(camelot);
    // (key rank, |tempo off|, why, key match, ratio, off).
    let mut found: Vec<(LibraryTrack, (u8, f64), Value)> = l
        .tracks
        .drain(..)
        .filter_map(|t| {
            let (ratio, off) = bpm_match(bpm, t.bpm?)?;
            let (rank, km) = match deck_key {
                None => (0, None),
                Some(d) => {
                    let m = key_match(d, key_rank(&t.key)?)?;
                    (
                        match m {
                            "same" => 0,
                            "adjacent" => 1,
                            _ => 2,
                        },
                        Some(m),
                    )
                }
            };
            let key_why = match (km, &t.key) {
                (Some(m), Some(k)) => format!("{k} ({m})"),
                _ => "key not matched".to_string(),
            };
            let tempo = t.bpm.unwrap_or(0.0);
            let bpm_why = if ratio == 1.0 {
                format!("{tempo:.1} BPM ({off:+.1} %)")
            } else {
                format!("{tempo:.1} BPM ×{ratio} = {:.1} ({off:+.1} %)", tempo * ratio)
            };
            let extra =
                json!({"why": format!("{key_why}, {bpm_why}"), "key_match": km, "bpm_ratio": ratio, "bpm_diff_pct": (off * 10.0).round() / 10.0});
            Some((t, (rank, off.abs()), extra))
        })
        .collect();
    match q.sort {
        Some(s) => {
            let mut tracks: Vec<LibraryTrack> = found.iter().map(|f| f.0.clone()).collect();
            sort_rows(&mut tracks, s, q.desc);
            let order: HashMap<&str, usize> = tracks.iter().enumerate().map(|(i, t)| (t.path.as_str(), i)).collect();
            let mut keyed: Vec<(usize, (LibraryTrack, (u8, f64), Value))> =
                found.drain(..).map(|f| (order.get(f.0.path.as_str()).copied().unwrap_or(usize::MAX), f)).collect();
            keyed.sort_by_key(|(i, _)| *i);
            found = keyed.into_iter().map(|(_, f)| f).collect();
        }
        None => found
            .sort_by(|a, b| a.1.0.cmp(&b.1.0).then(a.1.1.total_cmp(&b.1.1)).then_with(|| a.0.title.to_lowercase().cmp(&b.0.title.to_lowercase()))),
    }
    let rows: Vec<Value> = found
        .iter()
        .skip(q.offset)
        .take(q.limit)
        .map(|(t, _, extra)| {
            let mut r = row(t);
            if let (Some(o), Some(x)) = (r.as_object_mut(), extra.as_object()) {
                o.extend(x.clone());
            }
            r
        })
        .collect();
    l.tracks = found.into_iter().map(|f| f.0).collect();
    let mut out = listed(&l, q, rows);
    if let Some(o) = out.as_object_mut() {
        o.insert("deck".into(), json!({"deck": deck.id(), "key": key, "bpm": bpm}));
    }
    Ok(out)
}

// ---- health --------------------------------------------------------------------------------

/// Most audio files the health scan looks through in the folders.
const HEALTH_MAX_FILES: usize = 200_000;

fn health(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "library.health";
    let folders = super::decks::bool_param(CMD, p, "folders", Some(true))?;
    let limit = match p.get("limit") {
        None | Some(Value::Null) => 100,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=10_000).contains(n))
            .ok_or_else(|| bad(CMD, "`limit` must be 1..=10000"))?,
    };
    let (snap, loading) = library(e, CMD)?;
    if loading && e.library.read_at.is_none() {
        return Err(bad(CMD, "the libraries are being read for the first time: ask again in a moment"));
    }
    let roots: Vec<PathBuf> = if folders { e.library.music.iter().chain(e.library.added.iter()).cloned().collect() } else { Vec::new() };
    super::jobs::start(e, CMD, "health", move |ctx| health_scan(&snap, &roots, limit, ctx))
}

/// An artist or title for comparing: lower case, letters and digits, single spaces.
fn plain_words(s: &str) -> String {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The audio files in `roots` and below (at most [`HEALTH_MAX_FILES`]), and whether that cut it short.
fn audio_below(roots: &[PathBuf], ctx: &super::jobs::JobCtx) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut todo: Vec<PathBuf> = roots.to_vec();
    let mut seen = std::collections::HashSet::new();
    while let Some(dir) = todo.pop() {
        if ctx.cancelled() || out.len() >= HEALTH_MAX_FILES {
            return (out, true);
        }
        if !seen.insert(path_key(&dir.to_string_lossy())) {
            continue;
        }
        let Ok((dirs, files, _)) = crate::io::list_folder(&dir, MAX_FOLDER_ENTRIES) else { continue };
        out.extend(files.into_iter().filter(|f| is_audio(f)));
        todo.extend(dirs);
        ctx.progress(json!({"stage": "looking through the folders", "files": out.len()}));
    }
    (out, false)
}

fn health_scan(snap: &Snapshot, roots: &[PathBuf], limit: usize, ctx: &super::jobs::JobCtx) -> Result<Value> {
    const CMD: &str = "library.health";
    let stopped = || bad(CMD, "cancelled");
    let tracks = &snap.tracks;
    let (mut missing, mut stale, mut outdated, mut key_off, mut lacking) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut by_name: HashMap<String, Vec<&LibraryTrack>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        if i % 200 == 0 {
            if ctx.cancelled() {
                return Err(stopped());
            }
            ctx.progress(json!({"stage": "checking mixforge's tracks", "done": i, "total": tracks.len()}));
        }
        match crate::io::size_and_mtime(&t.path) {
            None => missing.push(json!(t.path)),
            Some((size, mtime)) => {
                if t.size.and_then(|x| u64::try_from(x).ok()) != Some(size) || t.mtime != Some(mtime) {
                    stale.push(json!(t.path));
                }
            }
        }
        if t.version != Some(db::ANALYSIS_VERSION) {
            outdated.push(json!(t.path));
        }
        if let (Some("mik"), Some(mine), Some(theirs)) = (t.key_source, &t.mixforge_key, &t.key)
            && mine != theirs
        {
            key_off.push(json!({"path": t.path, "title": t.title, "mik": theirs, "mixforge": mine}));
        }
        let lacks: Vec<&str> = [("bpm", t.bpm.is_none()), ("key", t.key.is_none()), ("energy", t.energy.is_none())]
            .into_iter()
            .filter_map(|(k, gone)| gone.then_some(k))
            .collect();
        if !lacks.is_empty() {
            lacking.push(json!({"path": t.path, "title": t.title, "lacks": lacks}));
        }
        let title = plain_words(&t.title);
        if !title.is_empty() {
            by_name.entry(format!("{}|{title}", plain_words(&t.artist))).or_default().push(t);
        }
    }
    let mut duplicates: Vec<Value> = by_name
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|g| json!({"artist": g.first().map(|t| t.artist.clone()), "title": g.first().map(|t| t.title.clone()), "paths": g.iter().map(|t| t.path.clone()).collect::<Vec<_>>()}))
        .collect();
    duplicates.sort_by_key(|d| d["title"].as_str().unwrap_or("").to_lowercase());
    let (files, cut_short) = audio_below(roots, ctx);
    if ctx.cancelled() {
        return Err(stopped());
    }
    let mut unanalysed: Vec<Value> = Vec::new();
    let mut listed = std::collections::HashSet::new();
    for f in files {
        let path = f.to_string_lossy().into_owned();
        let key = path_key(&path);
        if !snap.by_key.contains_key(&key) && listed.insert(key) {
            unanalysed.push(json!(path));
        }
    }
    let counts = json!({"tracks": tracks.len(), "missing": missing.len(), "stale": stale.len(), "outdated": outdated.len(),
        "unanalysed": unanalysed.len(), "duplicates": duplicates.len(), "key_disagreements": key_off.len(), "lacking": lacking.len()});
    let to_analyze: Vec<Value> = stale.iter().chain(outdated.iter()).chain(unanalysed.iter()).take(limit).cloned().collect();
    let mut truncated: Vec<&str> = Vec::new();
    let mut cap = |name: &'static str, mut v: Vec<Value>| {
        if v.len() > limit {
            v.truncate(limit);
            truncated.push(name);
        }
        v
    };
    let out = json!({
        "counts": counts,
        "missing": cap("missing", missing),
        "stale": cap("stale", stale),
        "outdated": cap("outdated", outdated),
        "unanalysed": cap("unanalysed", unanalysed),
        "duplicates": cap("duplicates", duplicates),
        "key_disagreements": cap("key_disagreements", key_off),
        "lacking": cap("lacking", lacking),
        "to_analyze": to_analyze,
        "folders_cut_short": cut_short,
        "library_error": snap.error,
        "mik_error": snap.mik_error,
    });
    let mut out = out;
    if let Some(o) = out.as_object_mut() {
        o.insert("truncated".into(), json!(truncated));
    }
    Ok(out)
}

// ---- folders -------------------------------------------------------------------------------

fn folders(e: &mut Engine, _: &Value) -> Result<Value> {
    let lib = &e.library;
    let root = |p: &Path, kind: &str| {
        let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        json!({"path": p.display().to_string(), "name": name, "kind": kind, "found": crate::io::dir_found(p)})
    };
    let roots: Vec<Value> = lib.music.iter().map(|m| root(m, "music")).chain(lib.added.iter().map(|a| root(a, "added"))).collect();
    let mixforge = lib.db.as_ref().map(|d| json!({"path": d.display().to_string(), "found": crate::io::size_and_mtime(d).is_some()}));
    Ok(json!({"roots": roots, "mixforge": mixforge, "kept": lib.prefs.is_some()}))
}

fn same_folder(a: &Path, b: &Path) -> bool {
    path_key(&a.to_string_lossy()).trim_end_matches(['\\', '/']) == path_key(&b.to_string_lossy()).trim_end_matches(['\\', '/'])
}

/// Saves the added folders, when the app keeps them.
fn save_folders(e: &mut Engine, cmd: &str) -> Result<()> {
    let Some(prefs) = e.library.prefs.clone() else { return Ok(()) };
    let folders: Vec<String> = e.library.added.iter().map(|p| p.display().to_string()).collect();
    let bytes = serde_json::to_vec_pretty(&json!({"folders": folders})).map_err(|err| bad(cmd, err.to_string()))?;
    crate::file_access::unconfined(|| {
        if let Some(dir) = prefs.parent() {
            crate::io::make_dir(dir)?;
        }
        crate::io::write_file_atomic(&prefs, &bytes)
    })
}

fn folder_param(cmd: &str, p: &Value) -> Result<PathBuf> {
    str_param(p, "path").filter(|s| !s.trim().is_empty()).map(PathBuf::from).ok_or_else(|| bad(cmd, "`path` required: a folder"))
}

fn add_folder(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "library.add_folder";
    let path = folder_param(CMD, p)?;
    if !crate::io::dir_found(&path) {
        return Err(bad(CMD, format!("{}: no such folder", path.display())));
    }
    let added = !e.library.added.iter().any(|a| same_folder(a, &path));
    if added {
        e.library.added.push(path.clone());
        save_folders(e, CMD)?;
    }
    Ok(json!({"path": path.display().to_string(), "added": added}))
}

fn remove_folder(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "library.remove_folder";
    let path = folder_param(CMD, p)?;
    let before = e.library.added.len();
    e.library.added.retain(|a| !same_folder(a, &path));
    let removed = e.library.added.len() != before;
    if removed {
        save_folders(e, CMD)?;
    }
    Ok(json!({"path": path.display().to_string(), "removed": removed}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No test anywhere in the workspace reads Marty's real mixforge library: tests build theirs
    /// in a temporary folder. Only the apps' entry points ask where mixforge keeps it.
    #[test]
    fn no_test_reaches_the_real_mixforge_library() {
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
        // Put together here, so this test's own source holds none of them.
        let forbidden: Vec<String> = [
            ["mixforge_library", "_db"],
            ["default", "_db("],
            ["Roaming\\\\", "mixforge"],
            ["Roaming/", "mixforge"],
            ["join(\"", "mixforge\")"],
            ["DJFORGE_", "MIXFORGE_HOME"],
            ["var(\"MIXFORGE", "_HOME\""],
            ["var_os(\"MIXFORGE", "_HOME\""],
            ["default_mik", "_db("],
            ["mik_library", "_db"],
            ["MIK", "Store"],
            ["hotcues", ".json"],
        ]
        .iter()
        .map(|w| w.concat())
        .collect();
        let mut offenders = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let in_tests_dir = f.components().any(|c| c.as_os_str() == "tests");
            let test_code = if in_tests_dir { text.as_str() } else { text.split_once("#[cfg(test)]").map_or("", |(_, t)| t) };
            let code = || test_code.lines().filter(|l| !l.trim_start().starts_with("//"));
            for word in &forbidden {
                if code().any(|l| l.contains(word.as_str())) {
                    offenders.push(format!("{}: {word}", f.display()));
                }
            }
            // %APPDATA% and mixforge together: the real library's folder.
            let app_data = ["APP", "DATA"].concat();
            if code().any(|l| l.contains(app_data.as_str()) && l.to_lowercase().contains("mixforge")) {
                offenders.push(format!("{}: {app_data} with mixforge", f.display()));
            }
        }
        assert!(offenders.is_empty(), "tests must use a library.db built in a temporary folder: {offenders:#?}");
    }

    #[test]
    fn mixed_in_keys_comment_gives_the_key_and_energy() {
        assert_eq!(mik_comment("8A - Energy 6"), (Some("8A".into()), Some(6.0)));
        assert_eq!(mik_comment("12b - energy 10 - 128 BPM"), (Some("12B".into()), Some(10.0)));
        assert_eq!(mik_comment("Energy 4"), (None, Some(4.0)));
        assert_eq!(mik_comment("great track"), (None, None));
        assert_eq!(mik_comment("8A - Energy 11"), (Some("8A".into()), None));
    }

    #[test]
    fn a_key_ending_in_a_wide_character_is_no_camelot_key() {
        assert_eq!([camelot("8é"), camelot("é"), camelot("ÄA")], [None; 3]);
        assert_eq!(camelot(" 10b "), Some((10, b'B')));
    }

    #[test]
    fn camelot_keys_mix_the_same_one_step_round_the_wheel_or_relative() {
        let k = |s| camelot(s).unwrap();
        assert_eq!(key_match(k("8A"), k("8A")), Some("same"));
        assert_eq!(key_match(k("8A"), k("9A")), Some("adjacent"));
        assert_eq!(key_match(k("8A"), k("7a")), Some("adjacent"));
        assert_eq!(key_match(k("12A"), k("1A")), Some("adjacent"), "the wheel wraps");
        assert_eq!(key_match(k("1B"), k("12B")), Some("adjacent"));
        assert_eq!(key_match(k("8A"), k("8B")), Some("relative"));
        assert_eq!(key_match(k("8A"), k("9B")), None);
        assert_eq!(key_match(k("8A"), k("10A")), None);
        assert_eq!([camelot("13A"), camelot("0B"), camelot("8C"), camelot(""), camelot("A")], [None; 5]);
    }

    #[test]
    fn tempos_match_within_six_percent_also_at_half_and_double() {
        assert_eq!(bpm_match(140.0, 140.0), Some((1.0, 0.0)));
        assert!(bpm_match(140.0, 148.0).is_some_and(|(r, d)| r == 1.0 && (d - 5.714).abs() < 0.01));
        assert_eq!(bpm_match(140.0, 150.0), None);
        assert!(bpm_match(140.0, 70.0).is_some_and(|(r, _)| r == 2.0), "a half-time track");
        assert!(bpm_match(70.0, 138.0).is_some_and(|(r, _)| r == 0.5), "a double-time one");
        assert_eq!(bpm_match(140.0, 100.0), None);
    }

    #[test]
    fn sections_snap_to_the_grids_bars_as_mixforge_does() {
        let a = json!({"bpm": 120.0, "first_downbeat": 1.0, "bars": 20});
        // A bar is 2 s.
        let st = json!({"segments": [
            {"start": 0.0, "end": 1.0, "label": "start"},
            {"start": 1.1, "end": 17.0, "label": "intro"},
            {"start": 17.0, "end": 33.0, "label": "drop"},
            {"start": 17.5, "end": 20.0, "label": "dup"},
            {"start": 80.0, "end": 90.0, "label": "past the end"},
            {"start": 33.0, "end": 41.0, "label": "end"}
        ]});
        assert_eq!(sections_on_grid(&a, &st).unwrap(), json!([{"bar": 0, "label": "intro", "bars": 8}, {"bar": 8, "label": "drop", "bars": 8}]));
        assert_eq!(sections_on_grid(&json!({"bpm": 120.0}), &st), None);
    }
}
