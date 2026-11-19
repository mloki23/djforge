//! The library the decks load from (`library.*`): mixforge's library, read only, and folders of
//! audio files.
//!
//! mixforge's `library.db` holds each analysed file's grid, key and length (see
//! `soundcraft_library`). The app names it ([`Engine::set_library`]); nothing else does, so no test
//! reaches the real one. What it holds is read once and kept until the database changes.
//!
//! A folder lists its audio files without decoding them: mixforge's analysis joins them by path,
//! and the tags of the files a page returns fill in what it lacks. The folders offered as roots
//! are the user's Music folder and the folders they add (kept in the app's preferences).
//!
//! `deck.load` asks [`analysis_for`] first: a fresh row gives the grid, key and mix points with
//! no sidecar run.

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
            "{source?, folder?, query?, sort?, desc?, offset?, limit?} — tracks to load on the decks: mixforge's library (read only; what it analysed) or the audio files (mp3, wav, flac, aiff, m4a, ogg) directly in a folder, joined to mixforge's analysis by path. source?: mixforge|folder (default mixforge), folder? (for source folder), query? (words that must all be in the title, artist or file name, any case), sort?: title|artist|bpm|key|energy|length|name (default title; key in Camelot order; unknowns last), desc?: bool, offset? (default 0), limit? (default 200, at most 1000). Returns {source, total, offset, rows: [{path, name, title, artist, bpm, key (Camelot), energy, length_s, analysed, stale (the file changed since mixforge analysed it), missing (no file there)}], from_copy (the library was busy: a copy was read)}, and for a folder {folder, folders: [{path, name}] (its subfolders), truncated}. A folder's files aren't decoded: sort, search and the rest use mixforge's analysis and the file names, and the tags of the rows returned fill in their title, artist, bpm and key",
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
    pub use soundcraft_library::{Stamp, StructureRow, TrackRow, path_key};
    use std::path::Path;

    pub fn stamp(db: &Path) -> Option<Stamp> {
        soundcraft_library::Library::new(db).stamp()
    }

    pub fn tracks(db: &Path) -> Result<(Vec<TrackRow>, bool), String> {
        soundcraft_library::Library::new(db).tracks().map(|r| (r.value, r.from_copy)).map_err(|e| e.to_string())
    }

    pub fn track(db: &Path, path: &str) -> Result<Option<(TrackRow, Option<StructureRow>)>, String> {
        soundcraft_library::Library::new(db).track(path).map(|r| r.value).map_err(|e| e.to_string())
    }
}

/// The web has no files: no library.
#[cfg(target_arch = "wasm32")]
mod db {
    use std::path::Path;

    pub type Stamp = ();

    pub struct TrackRow {
        pub path: String,
        pub size: Option<i64>,
        pub mtime: Option<i64>,
        pub data: String,
    }

    pub struct StructureRow {
        pub size: Option<i64>,
        pub mtime: Option<i64>,
        pub data: String,
    }

    pub fn path_key(path: &str) -> String {
        path.to_string()
    }

    pub fn stamp(_: &Path) -> Option<Stamp> {
        None
    }

    pub fn tracks(_: &Path) -> Result<(Vec<TrackRow>, bool), String> {
        Err("mixforge's library can't be read on the web".into())
    }

    pub fn track(_: &Path, _: &str) -> Result<Option<(TrackRow, Option<StructureRow>)>, String> {
        Err("mixforge's library can't be read on the web".into())
    }
}

pub(crate) use db::path_key;

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
    /// The file's size and time when it was analysed.
    size: Option<i64>,
    mtime: Option<i64>,
}

/// What `library.db` held when last read.
struct Cached {
    stamp: db::Stamp,
    tracks: Arc<Vec<LibraryTrack>>,
    /// Index into `tracks` by [`path_key`].
    by_key: Arc<HashMap<String, usize>>,
    from_copy: bool,
}

/// The library's setup and what it last read.
#[derive(Default)]
pub struct LibraryState {
    db: Option<PathBuf>,
    cache: Option<Cached>,
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
        self.cache = None;
    }

    /// The Music folder and where added folders are kept (whose folders are read now).
    pub(crate) fn set_folders(&mut self, music: Option<PathBuf>, prefs: Option<PathBuf>) {
        self.music = music;
        self.added = prefs.as_deref().map(read_folders).unwrap_or_default();
        self.prefs = prefs;
    }
}

/// The folders kept in the preferences file at `path` (`{"folders": [...]}`); none when it
/// can't be read.
fn read_folders(path: &Path) -> Vec<PathBuf> {
    let text = crate::file_access::unconfined(|| crate::io::read_text(path)).ok();
    let v: Value = text.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
    v.get("folders").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default()
}

/// Reads an analysis's list fields leniently: a value of another type is as good as missing.
#[derive(serde::Deserialize, Default)]
struct Summary {
    title: Option<Value>,
    artist: Option<Value>,
    bpm: Option<Value>,
    camelot: Option<Value>,
    key: Option<Value>,
    energy: Option<Value>,
    duration: Option<Value>,
}

fn text(v: &Option<Value>) -> Option<String> {
    v.as_ref().and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn number(v: &Option<Value>) -> Option<f64> {
    v.as_ref().and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

fn file_stem(path: &str) -> String {
    Path::new(path).file_stem().and_then(|n| n.to_str()).unwrap_or(path).to_string()
}

fn track_of(row: &db::TrackRow) -> LibraryTrack {
    let s: Summary = serde_json::from_str(&row.data).unwrap_or_default();
    let key = [&s.camelot, &s.key].into_iter().filter_map(text).find_map(|k| soundcraft_model::camelot_of(&k));
    LibraryTrack {
        path: row.path.clone(),
        title: text(&s.title).unwrap_or_else(|| file_stem(&row.path)),
        artist: text(&s.artist).unwrap_or_default(),
        bpm: number(&s.bpm).filter(|b| (20.0..=400.0).contains(b)),
        key,
        energy: number(&s.energy),
        length_s: number(&s.duration).filter(|d| *d >= 0.0),
        analysed: true,
        size: row.size,
        mtime: row.mtime,
    }
}

/// mixforge's library as it is now: read again when its files changed since the last read.
fn library(e: &mut Engine, cmd: &str) -> Result<(Arc<Vec<LibraryTrack>>, Arc<HashMap<String, usize>>, bool)> {
    let lib = &mut e.library;
    let Some(dbp) = lib.db.clone() else {
        return Err(bad(cmd, "mixforge's library isn't set up: djforge reads it when it runs as the app"));
    };
    let stamp = db::stamp(&dbp);
    if let Some(c) = &lib.cache
        && Some(c.stamp) == stamp
    {
        return Ok((Arc::clone(&c.tracks), Arc::clone(&c.by_key), c.from_copy));
    }
    let (rows, from_copy) = db::tracks(&dbp).map_err(EngineError::Io)?;
    let tracks: Vec<LibraryTrack> = rows.iter().map(track_of).collect();
    let by_key: HashMap<String, usize> = tracks.iter().enumerate().map(|(i, t)| (path_key(&t.path), i)).collect();
    let (tracks, by_key) = (Arc::new(tracks), Arc::new(by_key));
    // Kept only when the files stood still while they were read.
    if let Some(s) = stamp.filter(|s| Some(*s) == db::stamp(&dbp)) {
        lib.cache = Some(Cached { stamp: s, tracks: Arc::clone(&tracks), by_key: Arc::clone(&by_key), from_copy });
    }
    Ok((tracks, by_key, from_copy))
}

// ---- deck.load -----------------------------------------------------------------------------

/// What mixforge's library says about a file `deck.load` loads.
#[derive(Debug)]
pub(crate) enum FromLibrary {
    /// Its analysis (with `sections` from its structure row when that is fresh too).
    Fresh(Value),
    /// A row for an older state of the file.
    Stale,
    Absent,
    Unreadable(String),
}

impl FromLibrary {
    pub(crate) fn id(&self) -> &'static str {
        match self {
            FromLibrary::Fresh(_) => "fresh",
            FromLibrary::Stale => "stale",
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
    Ok(Query { folder, words, sort, desc, offset, limit })
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
    let (n, l) = k.split_at(k.len().checked_sub(1)?);
    let n: u8 = n.parse().ok().filter(|n| (1..=12).contains(n))?;
    let l = *l.as_bytes().first().filter(|l| matches!(l, b'A' | b'B'))?;
    Some((n, l))
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
    /// mixforge's library couldn't be joined to a folder: why.
    library_error: Option<String>,
}

fn listing(e: &mut Engine, cmd: &str, q: &Query) -> Result<Listing> {
    let Some(folder) = &q.folder else {
        let (tracks, _, from_copy) = library(e, cmd)?;
        return Ok(Listing {
            tracks: tracks.iter().filter(|t| matches(t, &q.words)).cloned().collect(),
            from_copy,
            folder: None,
            library_error: None,
        });
    };
    let (dirs, files, truncated) = crate::io::list_folder(folder, MAX_FOLDER_ENTRIES)?;
    let (joined, library_error, from_copy) = match e.library.db.is_some().then(|| library(e, cmd)) {
        Some(Ok((t, k, copy))) => (Some((t, k)), None, copy),
        Some(Err(err)) => (None, Some(err.to_string()), false),
        None => (None, None, false),
    };
    let tracks = files
        .iter()
        .filter(|p| is_audio(p))
        .map(|p| {
            let path = p.to_string_lossy().into_owned();
            let row = joined.as_ref().and_then(|(t, k)| k.get(&path_key(&path)).and_then(|i| t.get(*i)));
            match row {
                Some(r) => LibraryTrack { path, ..r.clone() },
                None => LibraryTrack {
                    title: file_stem(&path),
                    path,
                    artist: String::new(),
                    bpm: None,
                    key: None,
                    energy: None,
                    length_s: None,
                    analysed: false,
                    size: None,
                    mtime: None,
                },
            }
        })
        .filter(|t| matches(t, &q.words))
        .collect();
    Ok(Listing { tracks, from_copy, folder: Some((folder.clone(), dirs, truncated)), library_error })
}

/// A listing's row, its file looked at now: missing, changed since analysed, and the tags of an
/// MP3 filling what the analysis lacks.
fn row(t: &LibraryTrack) -> Value {
    let stamp = crate::io::size_and_mtime(&t.path);
    let stale = t.analysed && stamp.is_some_and(|(s, m)| t.size.and_then(|x| u64::try_from(x).ok()) != Some(s) || t.mtime != Some(m));
    let mut t = t.clone();
    let is_mp3 = Path::new(&t.path).extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3"));
    let lacks = !t.analysed || t.artist.is_empty();
    if stamp.is_some() && is_mp3 && lacks {
        let tags = super::prep::tag_head(Path::new(&t.path)).map(|h| soundcraft_audio_io::mp3tags::read_basic(&h)).unwrap_or_default();
        if t.artist.is_empty() {
            t.artist = tags.artist.unwrap_or_default();
        }
        if !t.analysed {
            if let Some(title) = tags.title {
                t.title = title;
            }
            t.bpm = t.bpm.or(tags.bpm.filter(|b| (20.0..=400.0).contains(b)));
            t.key = t.key.or_else(|| tags.key.as_deref().and_then(soundcraft_model::camelot_of));
        }
    }
    json!({"path": t.path, "name": file_name(&t.path), "title": t.title, "artist": t.artist, "bpm": t.bpm, "key": t.key,
        "energy": t.energy, "length_s": t.length_s, "analysed": t.analysed, "stale": stale, "missing": stamp.is_none()})
}

fn page(rows: &[LibraryTrack], q: &Query) -> Vec<Value> {
    rows.iter().skip(q.offset).take(q.limit).map(row).collect()
}

fn listed(l: &Listing, q: &Query, rows: Vec<Value>) -> Value {
    let mut out = json!({"source": if l.folder.is_some() { "folder" } else { "mixforge" }, "total": l.tracks.len(), "offset": q.offset,
        "rows": rows, "from_copy": l.from_copy});
    if let (Some((folder, dirs, truncated)), Some(o)) = (&l.folder, out.as_object_mut()) {
        let mut subs: Vec<(String, String)> = dirs
            .iter()
            .map(|d| (d.to_string_lossy().into_owned(), d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
            .collect();
        subs.sort_by_key(|(_, n)| n.to_lowercase());
        o.insert("folder".into(), json!(folder));
        o.insert("folders".into(), subs.into_iter().map(|(path, name)| json!({"path": path, "name": name})).collect());
        o.insert("truncated".into(), json!(truncated));
        o.insert("library_error".into(), json!(l.library_error));
    }
    out
}

fn list(e: &mut Engine, p: &Value) -> Result<Value> {
    list_as(e, p, LIST)
}

fn list_as(e: &mut Engine, p: &Value, cmd: &str) -> Result<Value> {
    let q = query_of(cmd, p)?;
    let mut l = listing(e, cmd, &q)?;
    sort_rows(&mut l.tracks, q.sort.unwrap_or(Sort::Title), q.desc);
    let rows = page(&l.tracks, &q);
    Ok(listed(&l, &q, rows))
}

fn search(e: &mut Engine, p: &Value) -> Result<Value> {
    const SEARCH: &str = "library.search";
    if str_param(p, "query").is_none_or(|q| q.trim().is_empty()) {
        return Err(bad(SEARCH, "`query` required: words to find in the title, artist or file name"));
    }
    list_as(e, p, SEARCH)
}

fn compatible(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "library.compatible";
    let deck = str_param(p, "deck").and_then(soundcraft_mix::DeckId::parse).ok_or_else(|| bad(CMD, "`deck` must be a or b"))?;
    let Some(t) = e.deck_tracks.get(deck.index()).and_then(Option::as_ref) else {
        return Err(bad(CMD, format!("deck {} is empty: load a track first (deck.load)", deck.id().to_uppercase())));
    };
    let now = e.deck_status.decks.get(deck.index()).map(|s| s.bpm).filter(|b| b.is_finite() && *b > 0.0);
    let (bpm, key) = (now.or(t.bpm).filter(|b| b.is_finite() && *b > 0.0), t.key.clone());
    let Some(bpm) = bpm else {
        return Err(bad(CMD, format!("deck {}'s track has no tempo to match: load it with a bpm", deck.id().to_uppercase())));
    };
    let deck_key = key.as_deref().and_then(camelot);
    let q = query_of(CMD, p)?;
    let mut l = listing(e, CMD, &q)?;
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
    let mut out = listed(&l, &q, rows);
    if let Some(o) = out.as_object_mut() {
        o.insert("deck".into(), json!({"deck": deck.id(), "key": key, "bpm": bpm}));
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
        ]
        .iter()
        .map(|w| w.concat())
        .collect();
        let mut offenders = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let in_tests_dir = f.components().any(|c| c.as_os_str() == "tests");
            let test_code = if in_tests_dir { text.as_str() } else { text.split_once("#[cfg(test)]").map_or("", |(_, t)| t) };
            for word in &forbidden {
                if test_code.lines().any(|l| l.contains(word.as_str()) && !l.trim_start().starts_with("//")) {
                    offenders.push(format!("{}: {word}", f.display()));
                }
            }
        }
        assert!(offenders.is_empty(), "tests must use a library.db built in a temporary folder: {offenders:#?}");
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
