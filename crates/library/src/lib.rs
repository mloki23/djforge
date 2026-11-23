//! Read-only access to mixforge's track library, `library.db`, and to Mixed In Key's
//! `MIKStore.db` (its key and energy).
//!
//! mixforge (Marty's analysis app, Python) keeps one SQLite database: `tracks(path TEXT PRIMARY
//! KEY, size, mtime, version, data TEXT)` holds each file's analysis as JSON (bpm, first
//! downbeat, key, Camelot, bars, …) and `structure(path, size, mtime, data)` all-in-one's
//! sections. `size` and `mtime` (whole seconds) are the file's when it was analysed: a row whose
//! file has changed since is stale, and one of another analysis `version` than
//! [`ANALYSIS_VERSION`] is outdated.
//!
//! djforge only ever reads it, never takes a write lock, and keeps out of mixforge's way (its
//! commits wait at most 5 s for readers):
//!
//! - It opens the database through a `file:` URI with `mode=ro` (a read-only connection: it can
//!   take SQLite's shared lock to read, never a reserved or exclusive one), with a short busy
//!   timeout so a commit in progress is waited for briefly.
//! - Not `immutable=1`: that tells SQLite the file can't change, so it skips locking altogether
//!   and could read a page mixforge is half-way through writing. mixforge may be running.
//! - A listing ([`Library::summaries`]) reads [`CHUNK`] rows at a time, each chunk one short
//!   statement of its own, and only the fields a listing shows (`json_extract`), so it never holds
//!   the shared lock for long. A whole analysis is read for one file only ([`Library::track`]).
//! - When a read can't be done (still busy after the timeout, locked, or a write-ahead log or
//!   hot journal a read-only connection can't recover), it copies the database, its `-wal` and
//!   `-journal` (when there are) to a temporary folder, reads the copy and deletes it. The files
//!   are read as any reader opens them (sharing reading, writing and deleting, taking no lock),
//!   so mixforge can go on writing. The `-shm` index is left behind: SQLite rebuilds it from the
//!   log, while a copied one could disagree with the copied log. A copy taken while the files
//!   changed is taken again.
//!
//! Mixed In Key's database is only ever read from such a copy ([`mik_songs`]), as mixforge reads
//! it: MIK may hold it open.
//!
//! The databases are the caller's to name ([`Library::new`]); [`default_db`] and
//! [`default_mik_db`] are where mixforge and MIK keep them, for the app's entry points only
//! (tests build their own in a temporary folder).
//!
//! Paths are matched as [`path_key`] spells them. The same file reached two ways (a mapped drive
//! and its `\\server\share` path, a link) has two spellings: a file mixforge analysed under one
//! is listed as not analysed under the other.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// How long a read waits for a commit in progress before it reads a copy instead.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(750);
/// Rows a listing reads per statement.
pub const CHUNK: usize = 256;
/// mixforge's analysis version (`ANALYSIS_VERSION` in `mixforge/__init__.py`): a row of another
/// is outdated, and mixforge analyses the file again.
pub const ANALYSIS_VERSION: i64 = 9;
/// Copies taken before giving up when the files keep changing under it.
const COPY_TRIES: usize = 3;

/// Why the library couldn't be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LibraryError {
    /// There is no database file at the path.
    #[error("no database at {0}")]
    Missing(String),
    /// It is there but couldn't be read (not a database, damaged, no access).
    #[error("the database at {path} couldn't be read: {why}")]
    Unreadable { path: String, why: String },
}

/// One row of `tracks`: a file's analysis.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackRow {
    /// The file's absolute path, as mixforge resolved it.
    pub path: String,
    /// The file's size and modification time (whole seconds) when it was analysed.
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    /// mixforge's analysis version.
    pub version: Option<i64>,
    /// The analysis, JSON (as SQLite's `json()` spells it: `NaN` becomes `null`).
    pub data: String,
}

/// What a listing shows of a `tracks` row: the fields it reads out of the analysis.
#[derive(Debug, Clone, PartialEq, Default)]
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

/// One row of `structure`: a file's sections from all-in-one.
#[derive(Debug, Clone, PartialEq)]
pub struct StructureRow {
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    /// `{segments: [{start, end, label}], downbeats, …}`, JSON.
    pub data: String,
}

/// A song Mixed In Key analysed: its file, energy (1–10) and key (Camelot), when it has them.
#[derive(Debug, Clone, PartialEq)]
pub struct MikSong {
    pub file: String,
    pub energy: Option<i64>,
    pub camelot: Option<String>,
}

/// What a read gave, and whether it came from a copy of the database (it was busy).
#[derive(Debug, Clone, PartialEq)]
pub struct Read<T> {
    pub value: T,
    pub from_copy: bool,
}

/// What a database's files look like now: when this changes, what was read may be out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    db: (u64, Option<SystemTime>),
    wal: Option<(u64, Option<SystemTime>)>,
    journal: Option<(u64, Option<SystemTime>)>,
}

/// The stamp of the database at `db`, `None` without one.
pub fn stamp_of(db: &Path) -> Option<Stamp> {
    let of = |p: &Path| std::fs::metadata(p).ok().filter(|m| m.is_file()).map(|m| (m.len(), m.modified().ok()));
    Some(Stamp { db: of(db)?, wal: of(&sidecar(db, "-wal")), journal: of(&sidecar(db, "-journal")) })
}

/// mixforge's library at a path, read on demand (no connection is held between reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    db: PathBuf,
    busy: Duration,
}

impl Library {
    /// The library in the database file `db`.
    pub fn new(db: impl Into<PathBuf>) -> Self {
        Library { db: db.into(), busy: BUSY_TIMEOUT }
    }

    /// The same, waiting `busy` for a commit before reading a copy.
    pub fn with_busy_timeout(mut self, busy: Duration) -> Self {
        self.busy = busy;
        self
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.db
    }

    /// The database files' sizes and times now, `None` without a database.
    pub fn stamp(&self) -> Option<Stamp> {
        stamp_of(&self.db)
    }

    /// Every row of `tracks`, as a listing shows it (none when the table isn't there yet), read
    /// [`CHUNK`] rows per statement.
    pub fn summaries(&self) -> Result<Read<Vec<TrackSummary>>, LibraryError> {
        read(&self.db, self.busy, false, |c| {
            if !has_table(c, "tracks")? {
                return Ok(Vec::new());
            }
            // `json_valid(data, 2)`: JSON5 too, as Python writes NaN; a row that isn't JSON gives
            // only its path and stamp.
            let field = |f: &str| format!("CASE WHEN json_valid(data, 2) THEN json_extract(data, '$.{f}') END");
            let sql = format!(
                "SELECT rowid, path, size, mtime, version, {}, {}, {}, {}, {}, {}, {} FROM tracks WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
                field("title"),
                field("artist"),
                field("bpm"),
                field("camelot"),
                field("key"),
                field("energy"),
                field("duration")
            );
            let mut q = c.prepare(&sql)?;
            let (mut out, mut after) = (Vec::new(), i64::MIN);
            loop {
                // One statement per chunk: its read lock ends with it.
                let chunk: Vec<(i64, Option<TrackSummary>)> = q
                    .query_map(rusqlite::params![after, i64::try_from(CHUNK).unwrap_or(256)], |r| {
                        let summary = r.get::<_, Option<String>>(1)?.map(|path| TrackSummary {
                            path,
                            size: int(r, 2),
                            mtime: int(r, 3),
                            version: int(r, 4),
                            title: text(r, 5),
                            artist: text(r, 6),
                            bpm: real(r, 7),
                            camelot: text(r, 8),
                            key: text(r, 9),
                            energy: real(r, 10),
                            duration: real(r, 11),
                        });
                        Ok((r.get::<_, i64>(0)?, summary))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                let Some(last) = chunk.last().map(|(id, _)| *id) else { break };
                let full = chunk.len() == CHUNK;
                out.extend(chunk.into_iter().filter_map(|(_, s)| s));
                if !full {
                    break;
                }
                after = last;
            }
            Ok(out)
        })
    }

    /// The `tracks` and `structure` rows of the file at `path` (absolute), matched as
    /// [`path_key`] compares paths.
    pub fn track(&self, path: &str) -> Result<Read<Option<(TrackRow, Option<StructureRow>)>>, LibraryError> {
        let want = path_key(path);
        let plain = plain_path(path);
        read(&self.db, self.busy, true, |c| {
            if !has_table(c, "tracks")? {
                return Ok(None);
            }
            let select = "SELECT path, size, mtime, version, CASE WHEN json_valid(data, 2) THEN json(data) ELSE data END FROM tracks WHERE path = ?1";
            let exact = c.query_row(select, [&plain], track_row).optional()?.flatten();
            let row = match exact {
                Some(r) => Some(r),
                // Another spelling of the same path (case, separators on Windows).
                None => {
                    let mut q = c.prepare("SELECT path FROM tracks")?;
                    let paths: Vec<String> = q.query_map([], |r| r.get::<_, Option<String>>(0))?.filter_map(|r| r.ok().flatten()).collect();
                    match paths.into_iter().find(|p| path_key(p) == want) {
                        Some(p) => c.query_row(select, [&p], track_row).optional()?.flatten(),
                        None => None,
                    }
                }
            };
            let Some(row) = row else { return Ok(None) };
            let structure = if has_table(c, "structure")? {
                c.query_row("SELECT size, mtime, data FROM structure WHERE path = ?1", [&row.path], |r| {
                    Ok(r.get::<_, Option<String>>(2)?.map(|data| StructureRow { size: int(r, 0), mtime: int(r, 1), data }))
                })
                .optional()?
                .flatten()
            } else {
                None
            };
            Ok(Some((row, structure)))
        })
    }
}

/// The songs Mixed In Key analysed, from its database `db` (`MIKStore.db`), read from a copy as
/// mixforge's `mik.py` does: MIK may hold the file open. A key that isn't Camelot is left out.
pub fn mik_songs(db: &Path) -> Result<Read<Vec<MikSong>>, LibraryError> {
    let shown = db.display().to_string();
    if !std::fs::metadata(db).is_ok_and(|m| m.is_file()) {
        return Err(LibraryError::Missing(shown));
    }
    let query = |c: &Connection| -> rusqlite::Result<Vec<MikSong>> {
        let mut q = c.prepare("SELECT File, OverallEnergy, MainKey FROM Song WHERE IsAnalyzed = 1")?;
        let rows = q.query_map([], |r| {
            let camelot = text(r, 2).filter(|k| k.strip_suffix(['A', 'B']).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())));
            let energy = r.get_ref(1).ok().and_then(|v| match v {
                ValueRef::Integer(i) => Some(i),
                ValueRef::Real(x) if x.is_finite() => Some(x as i64),
                _ => None,
            });
            Ok(text(r, 0).map(|file| MikSong { file, energy: energy.filter(|e| *e > 0), camelot }))
        })?;
        rows.filter_map(|r| r.transpose()).collect()
    };
    read_copy(db, BUSY_TIMEOUT, false, &query).map_err(|why| LibraryError::Unreadable { path: shown, why })
}

/// Where mixforge keeps its library: `DJFORGE_MIXFORGE_HOME`, else mixforge's own
/// `MIXFORGE_HOME`, else `%APPDATA%\mixforge` (`~/mixforge` without `APPDATA`, as mixforge has
/// it), then `library.db`. Only the app's entry points call this.
pub fn default_db() -> Result<PathBuf, String> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = match var("DJFORGE_MIXFORGE_HOME").or_else(|| var("MIXFORGE_HOME")) {
        Some(h) => h,
        None => var("APPDATA").or_else(|| var("USERPROFILE")).or_else(|| var("HOME")).ok_or("neither APPDATA nor HOME is set")?.join("mixforge"),
    };
    Ok(home.join("library.db"))
}

/// Where Mixed In Key 10 keeps its library: `%LOCALAPPDATA%\Mixed In Key\Mixed In Key\10.0\
/// MIKStore.db` (as mixforge's `mik.py` reads it). Only the app's entry points call this.
pub fn default_mik_db() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from).ok_or("LOCALAPPDATA is not set")?;
    Ok(base.join("Mixed In Key").join("Mixed In Key").join("10.0").join("MIKStore.db"))
}

/// The key two spellings of one path share: on Windows separators become `\`, the `\\?\`
/// prefix goes and case is ignored, as the file system does (and mixforge's `normcase`);
/// elsewhere the path itself.
pub fn path_key(path: &str) -> String {
    if cfg!(windows) { plain_path(path).to_lowercase() } else { path.to_string() }
}

/// `path` without Windows' `\\?\` prefix and with `\` separators (on Windows).
fn plain_path(path: &str) -> String {
    if !cfg!(windows) {
        return path.to_string();
    }
    let p = path.replace('/', "\\");
    match p.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => p.strip_prefix(r"\\?\").map_or(p.clone(), str::to_string),
    }
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn int(r: &rusqlite::Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(n) => Some(n),
        ValueRef::Real(x) if x.is_finite() && x.fract() == 0.0 => Some(x as i64),
        _ => None,
    }
}

fn real(r: &rusqlite::Row, i: usize) -> Option<f64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(n) => Some(n as f64),
        ValueRef::Real(x) => Some(x).filter(|x| x.is_finite()),
        _ => None,
    }
}

fn text(r: &rusqlite::Row, i: usize) -> Option<String> {
    match r.get_ref(i).ok()? {
        ValueRef::Text(t) => std::str::from_utf8(t).ok().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string),
        _ => None,
    }
}

/// A `tracks` row; `None` for one without a path or data.
fn track_row(r: &rusqlite::Row) -> rusqlite::Result<Option<TrackRow>> {
    let (Some(path), Some(data)) = (r.get::<_, Option<String>>(0).ok().flatten(), r.get::<_, Option<String>>(4).ok().flatten()) else {
        return Ok(None);
    };
    Ok(Some(TrackRow { path, size: int(r, 1), mtime: int(r, 2), version: int(r, 3), data }))
}

fn has_table(c: &Connection, name: &str) -> rusqlite::Result<bool> {
    c.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1", [name], |r| r.get::<_, i64>(0)).map(|n| n > 0)
}

/// `f` inside one read transaction when `snapshot` (its queries see one state of the database),
/// else statement by statement.
fn run<T>(c: &Connection, snapshot: bool, f: &impl Fn(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    if !snapshot {
        return f(c);
    }
    let tx = c.unchecked_transaction()?;
    let out = f(c);
    // Ends the read (nothing was written).
    drop(tx);
    out
}

/// Runs `f` on a read-only connection to the database `db`, else (busy, locked, a log it can't
/// recover) on a copy of it.
fn read<T>(db: &Path, busy: Duration, snapshot: bool, f: impl Fn(&Connection) -> rusqlite::Result<T>) -> Result<Read<T>, LibraryError> {
    let shown = db.display().to_string();
    if !std::fs::metadata(db).is_ok_and(|m| m.is_file()) {
        return Err(LibraryError::Missing(shown));
    }
    match open_read_only(db, busy).and_then(|c| run(&c, snapshot, &f)) {
        Ok(value) => Ok(Read { value, from_copy: false }),
        Err(e) if wants_copy(&e) => read_copy(db, busy, snapshot, &f).map_err(|why| LibraryError::Unreadable { path: shown.clone(), why }),
        Err(e) => Err(LibraryError::Unreadable { path: shown, why: e.to_string() }),
    }
}

/// Runs `f` on a copy of the database `db`: read-only, else (a log the copy must recover)
/// read-write, the copy being djforge's own.
fn read_copy<T>(db: &Path, busy: Duration, snapshot: bool, f: &impl Fn(&Connection) -> rusqlite::Result<T>) -> Result<Read<T>, String> {
    let mut last = String::from("the database kept changing while it was copied");
    for _ in 0..COPY_TRIES {
        let copy = Copy::of(db)?;
        if !copy.steady {
            continue;
        }
        let attempt = |c: rusqlite::Result<Connection>| {
            c.and_then(|c| {
                c.busy_timeout(busy)?;
                run(&c, snapshot, f)
            })
        };
        let result = match attempt(open_read_only(&copy.file, busy)) {
            Err(e) if wants_copy(&e) => attempt(Connection::open(&copy.file)),
            other => other,
        };
        match result {
            Ok(value) => return Ok(Read { value, from_copy: true }),
            Err(e) => last = format!("{e} (reading a copy, as the database was busy)"),
        }
    }
    Err(last)
}

/// A read-only connection: `file:<path>?mode=ro`.
fn open_read_only(db: &Path, busy: Duration) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let c = match uri_of(db) {
        Some(uri) => Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)?,
        // A network path (\\server\share) has no plain `file:` form: read-only by the flag.
        None => Connection::open_with_flags(db, flags)?,
    };
    c.busy_timeout(busy)?;
    Ok(c)
}

/// `file:` URI of `db` opening it read-only; `None` for a network path.
fn uri_of(db: &Path) -> Option<String> {
    let s = plain_path(&db.to_string_lossy());
    if s.starts_with(r"\\") || s.starts_with("//") {
        return None;
    }
    let s = s.replace('\\', "/");
    let mut out: Vec<u8> = if s.starts_with('/') { b"file://".to_vec() } else { b"file:///".to_vec() };
    for b in s.bytes() {
        match b {
            b'%' | b'?' | b'#' | b' ' | 0..=0x1F | 0x7F => out.extend(format!("%{b:02X}").bytes()),
            _ => out.push(b),
        }
    }
    out.extend(b"?mode=ro");
    String::from_utf8(out).ok()
}

/// Whether a read that failed so may work on a copy: the database is busy or locked, or its
/// log or journal needs a recovery a read-only connection can't do.
fn wants_copy(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked | ErrorCode::CannotOpen | ErrorCode::ReadOnly))
}

static COPIES: AtomicU64 = AtomicU64::new(0);

/// A copy of a database and its log in a temporary folder, deleted on drop.
struct Copy {
    dir: PathBuf,
    file: PathBuf,
    /// The files didn't change while they were copied.
    steady: bool,
}

/// Copies `from` to `to`, opening `from` as any reader does (`File::open` shares reading,
/// writing and deleting on Windows and takes no lock), so its owner can go on writing.
fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    let err = |e: std::io::Error| format!("copying {}: {e}", from.display());
    let mut src = std::fs::File::open(from).map_err(err)?;
    let mut dst = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    std::io::copy(&mut src, &mut dst).map_err(err)?;
    Ok(())
}

/// SQLite's file change counter (header bytes 24..28, big-endian): every commit in rollback
/// mode bumps it. `None` for a file too short to have a header (an empty database).
fn change_counter(db: &Path) -> Option<u32> {
    use std::io::{Read as _, Seek as _};
    let mut f = std::fs::File::open(db).ok()?;
    f.seek(std::io::SeekFrom::Start(24)).ok()?;
    let mut b = [0u8; 4];
    f.read_exact(&mut b).ok()?;
    Some(u32::from_be_bytes(b))
}

impl Copy {
    fn of(db: &Path) -> Result<Copy, String> {
        let n = COPIES.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("djforge-dbcopy-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let file = dir.join(db.file_name().unwrap_or(std::ffi::OsStr::new("copy.db")));
        let mut copy = Copy { dir, file, steady: false };
        let (before, counter) = (stamp_of(db), change_counter(db));
        copy_file(db, &copy.file)?;
        for suffix in ["-wal", "-journal"] {
            let from = sidecar(db, suffix);
            if std::fs::metadata(&from).is_ok_and(|m| m.is_file()) {
                copy_file(&from, &sidecar(&copy.file, suffix))?;
            }
        }
        // Steady: the files' sizes and times, and the change counter of the live file before and
        // after, all the copy's (a commit within one timestamp tick still shows).
        copy.steady = before.is_some() && stamp_of(db) == before && change_counter(db) == counter && change_counter(&copy.file) == counter;
        Ok(copy)
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary folder, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!("djforge-library-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A library as mixforge makes it (`library.py`'s `_connect`), with two tracks and one
    /// structure row.
    fn fixture(dir: &Path) -> PathBuf {
        let db = dir.join("library.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS tracks (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, version INTEGER, data TEXT);
             CREATE TABLE IF NOT EXISTS structure (path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, data TEXT);",
        )
        .unwrap();
        for (path, bpm) in [(r"C:\Music\One.mp3", 138.0), (r"C:\Music\Two.flac", 140.0)] {
            let data = format!(r#"{{"path": {path:?}, "bpm": {bpm}, "title": "t", "camelot": "8A", "duration": 300.5, "lufs": NaN}}"#);
            c.execute("INSERT INTO tracks VALUES (?1, 100, 1700000000, 9, ?2)", rusqlite::params![path, data]).unwrap();
        }
        c.execute("INSERT INTO structure VALUES (?1, 100, 1700000000, ?2)", rusqlite::params![r"C:\Music\One.mp3", r#"{"segments": []}"#]).unwrap();
        db
    }

    fn snapshot(db: &Path) -> (Vec<u8>, Option<SystemTime>) {
        (std::fs::read(db).unwrap(), std::fs::metadata(db).unwrap().modified().ok())
    }

    #[test]
    fn lists_what_a_listing_shows_and_reads_one_track_whole_leaving_the_file_untouched() {
        let dir = Scratch::new("read");
        let db = fixture(&dir.0);
        let before = snapshot(&db);
        let lib = Library::new(&db);
        let all = lib.summaries().unwrap();
        assert!(!all.from_copy);
        assert_eq!(all.value.len(), 2);
        let one = &all.value[0];
        assert_eq!((one.title.as_deref(), one.bpm, one.camelot.as_deref(), one.duration), (Some("t"), Some(138.0), Some("8A"), Some(300.5)));
        assert_eq!((one.artist.as_deref(), one.energy, one.version), (None, None, Some(9)));
        let (row, structure) = lib.track(r"C:\Music\One.mp3").unwrap().value.unwrap();
        assert_eq!((row.size, row.mtime, row.version), (Some(100), Some(1_700_000_000), Some(9)));
        assert!(row.data.contains(r#""lufs":null"#), "Python's NaN, made JSON: {}", row.data);
        assert_eq!(structure.unwrap().data, r#"{"segments": []}"#);
        assert_eq!(lib.track(r"C:\Music\Two.flac").unwrap().value.unwrap().1, None, "no sections");
        assert_eq!(lib.track(r"C:\Music\Three.mp3").unwrap().value, None);
        if cfg!(windows) {
            assert!(lib.track("c:/music/one.MP3").unwrap().value.is_some(), "another spelling of the path");
        }
        assert_eq!(snapshot(&db), before, "the database is never written");
        assert!(!sidecar(&db, "-journal").exists() && !sidecar(&db, "-wal").exists());
    }

    #[test]
    fn a_listing_reads_in_chunks_and_a_row_that_isnt_json_keeps_its_path() {
        let dir = Scratch::new("chunks");
        let db = fixture(&dir.0);
        let c = Connection::open(&db).unwrap();
        c.execute_batch("BEGIN").unwrap();
        for i in 0..(2 * CHUNK + 7) {
            c.execute("INSERT INTO tracks VALUES (?1, 1, 1, 9, ?2)", rusqlite::params![format!("C:\\Many\\{i}.mp3"), format!(r#"{{"bpm": {i}}}"#)])
                .unwrap();
        }
        c.execute("INSERT INTO tracks VALUES ('C:\\Bad.mp3', 1, 1, 8, 'not json')", []).unwrap();
        c.execute_batch("COMMIT").unwrap();
        let all = Library::new(&db).summaries().unwrap().value;
        assert_eq!(all.len(), 2 * CHUNK + 10);
        let bad = all.iter().find(|s| s.path == r"C:\Bad.mp3").unwrap();
        assert_eq!((bad.bpm, bad.title.as_deref(), bad.version), (None, None, Some(8)));
    }

    #[test]
    fn the_analysis_version_is_mixforges() {
        // mixforge's source, when it is on this computer: a bump there must reach djforge.
        // MIXFORGE_SRC: mixforge's checkout (holding mixforge/__init__.py).
        let Some(src) = std::env::var_os("MIXFORGE_SRC").filter(|v| !v.is_empty()) else {
            println!("note: MIXFORGE_SRC is not set; this test checks nothing");
            return;
        };
        let text = std::fs::read_to_string(Path::new(&src).join("mixforge/__init__.py")).unwrap();
        let line = text.lines().find(|l| l.trim_start().starts_with("ANALYSIS_VERSION")).unwrap();
        let v: i64 = line.split('=').nth(1).unwrap().trim().parse().unwrap();
        assert_eq!(v, ANALYSIS_VERSION, "mixforge's ANALYSIS_VERSION changed: update soundcraft_library::ANALYSIS_VERSION");
    }

    #[test]
    fn a_read_only_connection_cannot_write() {
        let dir = Scratch::new("ro");
        let db = fixture(&dir.0);
        let c = open_read_only(&db, BUSY_TIMEOUT).unwrap();
        let e = c.execute("DELETE FROM tracks", []).unwrap_err();
        assert_eq!(e.sqlite_error_code(), Some(ErrorCode::ReadOnly), "{e}");
    }

    #[test]
    fn reads_while_mixforge_holds_a_write_transaction() {
        let dir = Scratch::new("busy");
        let db = fixture(&dir.0);
        let lib = Library::new(&db).with_busy_timeout(Duration::from_millis(50));
        // mixforge writing (a reserved lock, as Python's sqlite3 takes for an INSERT): readers go on.
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO tracks VALUES ('C:\\Music\\New.mp3', 1, 1, 9, '{}');").unwrap();
        let r = lib.summaries().unwrap();
        assert_eq!((r.value.len(), r.from_copy), (2, false), "the uncommitted row isn't seen");
        // Committing (an exclusive lock): the read falls back on a copy of the committed state.
        writer.execute_batch("ROLLBACK; BEGIN EXCLUSIVE; DELETE FROM tracks;").unwrap();
        let r = lib.summaries().unwrap();
        assert_eq!((r.value.len(), r.from_copy), (2, true));
        assert!(lib.track(r"C:\Music\Two.flac").unwrap().from_copy);
        writer.execute_batch("ROLLBACK").unwrap();
        assert!(!lib.summaries().unwrap().from_copy);
    }

    /// mixforge commits again and again (each waiting at most 100 ms for readers) while djforge
    /// lists a big library: none of its commits fails.
    #[test]
    fn a_listing_never_keeps_mixforge_from_committing() {
        let dir = Scratch::new("writer");
        let db = fixture(&dir.0);
        let c = Connection::open(&db).unwrap();
        c.execute_batch("BEGIN").unwrap();
        let bars = format!("[{}]", vec!["0.5"; 400].join(", "));
        for i in 0..4000 {
            let data = format!(r#"{{"title": "Track {i}", "bpm": 140, "bar_change": {bars}, "bar_energy_db": {bars}}}"#);
            c.execute("INSERT INTO tracks VALUES (?1, 1, 1, 9, ?2)", rusqlite::params![format!("C:\\Big\\{i}.mp3"), data]).unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
        drop(c);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (flag, path) = (std::sync::Arc::clone(&stop), db.clone());
        let writer = std::thread::spawn(move || {
            let w = Connection::open(&path).unwrap();
            w.busy_timeout(Duration::from_millis(100)).unwrap();
            let (mut commits, mut failures) = (0, Vec::new());
            while !flag.load(Ordering::Relaxed) {
                match w.execute("INSERT OR REPLACE INTO tracks VALUES ('C:\\Big\\new.mp3', 2, 2, 9, '{}')", []) {
                    Ok(_) => commits += 1,
                    Err(e) => failures.push(e.to_string()),
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            (commits, failures)
        });
        let lib = Library::new(&db);
        let started = std::time::Instant::now();
        let mut reads = 0;
        while started.elapsed() < Duration::from_secs(2) || reads < 3 {
            assert!(lib.summaries().unwrap().value.len() >= 4002);
            reads += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let (commits, failures) = writer.join().unwrap();
        assert!(failures.is_empty(), "mixforge's commits failed while djforge listed: {failures:?}");
        assert!(commits > 10, "{commits} commits");
    }

    #[test]
    fn reads_what_a_write_ahead_log_holds() {
        let dir = Scratch::new("wal");
        let db = fixture(&dir.0);
        let writer = Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.execute("INSERT INTO tracks VALUES ('C:\\Music\\Wal.mp3', 1, 1, 9, '{}')", []).unwrap();
        // The writer stays open: the row is in the log only.
        let r = Library::new(&db).summaries().unwrap();
        assert_eq!(r.value.len(), 3, "the committed row in the log is read");
        drop(writer);
    }

    #[test]
    fn no_database_or_not_one_says_so() {
        let dir = Scratch::new("missing");
        let lib = Library::new(dir.0.join("library.db"));
        assert!(matches!(lib.summaries(), Err(LibraryError::Missing(_))));
        assert_eq!(lib.stamp(), None);
        std::fs::write(dir.0.join("library.db"), b"not a database, just some bytes that go on for a while........").unwrap();
        assert!(matches!(lib.summaries(), Err(LibraryError::Unreadable { .. })), "{:?}", lib.summaries());
        // An empty file is an empty database: no tables, no tracks.
        std::fs::write(dir.0.join("library.db"), b"").unwrap();
        assert_eq!(lib.summaries().unwrap().value, Vec::new());
    }

    #[test]
    fn a_copy_is_steady_only_when_the_change_counter_stood_still() {
        let dir = Scratch::new("counter");
        let db = fixture(&dir.0);
        let before = change_counter(&db).unwrap();
        let copy = Copy::of(&db).unwrap();
        assert!(copy.steady);
        assert_eq!(change_counter(&copy.file), Some(before));
        Connection::open(&db).unwrap().execute("DELETE FROM structure", []).unwrap();
        assert_ne!(change_counter(&db), Some(before), "a commit bumps it");
        assert_eq!(change_counter(&dir.0.join("none.db")), None);
    }

    #[test]
    fn the_stamp_sees_a_journal_come_and_go() {
        let dir = Scratch::new("stamp");
        let db = fixture(&dir.0);
        let before = stamp_of(&db).unwrap();
        std::fs::write(sidecar(&db, "-journal"), b"x").unwrap();
        assert_ne!(stamp_of(&db), Some(before));
        std::fs::remove_file(sidecar(&db, "-journal")).unwrap();
        assert_eq!(stamp_of(&db), Some(before));
    }

    /// Mixed In Key's `Song` table, as mixforge's `mik.py` reads it.
    #[test]
    fn mixed_in_keys_songs_are_read_from_a_copy() {
        let dir = Scratch::new("mik");
        let db = dir.0.join("MIK.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE Song (File TEXT, OverallEnergy INTEGER, MainKey TEXT, MainKeyConfidence REAL, Tempo REAL, IsAnalyzed INTEGER);
             INSERT INTO Song VALUES ('C:\\Music\\One.mp3', 6, '8A', 0.9, 138.0, 1);
             INSERT INTO Song VALUES ('C:\\Music\\Two.mp3', 0, 'Am', 0.5, 140.0, 1);
             INSERT INTO Song VALUES ('C:\\Music\\Three.mp3', 7, '3B', 0.9, 140.0, 0);
             INSERT INTO Song VALUES (NULL, 7, '3B', 0.9, 140.0, 1);",
        )
        .unwrap();
        // MIK holds it open (a write transaction under way): the copy is read regardless.
        c.execute_batch("BEGIN IMMEDIATE; UPDATE Song SET OverallEnergy = 1;").unwrap();
        let before = snapshot(&db);
        let songs = mik_songs(&db).unwrap();
        assert!(songs.from_copy);
        assert_eq!(
            songs.value,
            [
                MikSong { file: r"C:\Music\One.mp3".into(), energy: Some(6), camelot: Some("8A".into()) },
                MikSong { file: r"C:\Music\Two.mp3".into(), energy: None, camelot: None },
            ]
        );
        assert_eq!(snapshot(&db), before);
        c.execute_batch("ROLLBACK").unwrap();
        assert!(matches!(mik_songs(&dir.0.join("none.db")), Err(LibraryError::Missing(_))));
    }

    #[test]
    fn the_uri_escapes_what_sqlite_would_read_as_its_own() {
        if cfg!(windows) {
            assert_eq!(uri_of(Path::new(r"C:\A b\x#1?%\é.db")).unwrap(), "file:///C:/A%20b/x%231%3F%25/é.db?mode=ro");
            assert_eq!(uri_of(Path::new(r"\\server\share\library.db")), None);
        } else {
            assert_eq!(uri_of(Path::new("/a b/x.db")).unwrap(), "file:///a%20b/x.db?mode=ro");
        }
        let dir = Scratch::new("odd name #1");
        let sub = dir.0.join("a b%20?");
        if std::fs::create_dir_all(&sub).is_ok() {
            let db = fixture(&sub);
            assert_eq!(Library::new(&db).summaries().unwrap().value.len(), 2);
        }
    }

    #[test]
    fn paths_compare_as_the_file_system_does() {
        if cfg!(windows) {
            assert_eq!(path_key(r"\\?\C:\Music\A.mp3"), path_key("c:/music/a.MP3"));
            assert_eq!(plain_path(r"\\?\UNC\srv\s\a.mp3"), r"\\srv\s\a.mp3");
        } else {
            assert_ne!(path_key("/m/A.mp3"), path_key("/m/a.mp3"));
        }
    }
}
