//! Read-only access to mixforge's track library, `library.db`.
//!
//! mixforge (Marty's analysis app, Python) keeps one SQLite database: `tracks(path TEXT PRIMARY
//! KEY, size, mtime, version, data TEXT)` holds each file's analysis as JSON (bpm, first
//! downbeat, key, Camelot, bars, …) and `structure(path, size, mtime, data)` all-in-one's
//! sections. `size` and `mtime` (whole seconds) are the file's when it was analysed: a row whose
//! file has changed since is stale.
//!
//! djforge only ever reads it, and never takes a write lock:
//!
//! - It opens the database through a `file:` URI with `mode=ro` (a read-only connection: it can
//!   take SQLite's shared lock to read, never a reserved or exclusive one), with a short busy
//!   timeout so a commit in progress is waited for briefly.
//! - Not `immutable=1`: that tells SQLite the file can't change, so it skips locking altogether
//!   and could read a page mixforge is half-way through writing. mixforge may be running.
//! - When that read can't be done (still busy after the timeout, locked, or a write-ahead log or
//!   hot journal a read-only connection can't recover), it copies the database, its `-wal` and
//!   `-journal` (when there are) to a temporary folder, reads the copy and deletes it. The `-shm`
//!   index is left behind: SQLite rebuilds it from the log, while a copied one could disagree
//!   with the copied log. A copy taken while the files changed is taken again.
//!
//! The database is the caller's to name ([`Library::new`]); [`default_db`] is where mixforge
//! keeps it, for the app's entry points only (tests build their own in a temporary folder).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// How long a read waits for a commit in progress before it reads a copy instead.
pub const BUSY_TIMEOUT: Duration = Duration::from_millis(750);
/// Copies taken before giving up when the files keep changing under it.
const COPY_TRIES: usize = 3;

/// Why the library couldn't be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LibraryError {
    /// There is no database file at the path.
    #[error("no mixforge library at {0}")]
    Missing(String),
    /// It is there but couldn't be read (not a database, damaged, no access).
    #[error("mixforge's library at {path} couldn't be read: {why}")]
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
    /// The analysis, JSON.
    pub data: String,
}

/// One row of `structure`: a file's sections from all-in-one.
#[derive(Debug, Clone, PartialEq)]
pub struct StructureRow {
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    /// `{segments: [{start, end, label}], downbeats, …}`, JSON.
    pub data: String,
}

/// What a read gave, and whether it came from a copy of the database (it was busy).
#[derive(Debug, Clone, PartialEq)]
pub struct Read<T> {
    pub value: T,
    pub from_copy: bool,
}

/// What the database files look like now: when this changes, what was read may be out of date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    db: (u64, Option<SystemTime>),
    wal: Option<(u64, Option<SystemTime>)>,
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
        let of = |p: &Path| std::fs::metadata(p).ok().filter(|m| m.is_file()).map(|m| (m.len(), m.modified().ok()));
        Some(Stamp { db: of(&self.db)?, wal: of(&sidecar(&self.db, "-wal")) })
    }

    /// Every row of `tracks` (none when the table isn't there yet).
    pub fn tracks(&self) -> Result<Read<Vec<TrackRow>>, LibraryError> {
        self.read(|c| {
            if !has_table(c, "tracks")? {
                return Ok(Vec::new());
            }
            let mut q = c.prepare("SELECT path, size, mtime, version, data FROM tracks")?;
            let rows = q.query_map([], track_row)?;
            rows.filter_map(|r| r.transpose()).collect()
        })
    }

    /// The `tracks` and `structure` rows of the file at `path` (absolute), matched as
    /// [`path_key`] compares paths.
    pub fn track(&self, path: &str) -> Result<Read<Option<(TrackRow, Option<StructureRow>)>>, LibraryError> {
        let want = path_key(path);
        let plain = plain_path(path);
        self.read(|c| {
            if !has_table(c, "tracks")? {
                return Ok(None);
            }
            let cols = "path, size, mtime, version, data";
            let exact = c.query_row(&format!("SELECT {cols} FROM tracks WHERE path = ?1"), [&plain], track_row).optional()?.flatten();
            let row = match exact {
                Some(r) => Some(r),
                // Another spelling of the same path (case, separators on Windows).
                None => {
                    let mut q = c.prepare("SELECT path FROM tracks")?;
                    let paths: Vec<String> = q.query_map([], |r| r.get::<_, Option<String>>(0))?.filter_map(|r| r.ok().flatten()).collect();
                    match paths.into_iter().find(|p| path_key(p) == want) {
                        Some(p) => c.query_row(&format!("SELECT {cols} FROM tracks WHERE path = ?1"), [&p], track_row).optional()?.flatten(),
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

    /// Runs `f` on a read-only connection to the database, else (busy, locked, a log it can't
    /// recover) on a copy of it.
    fn read<T>(&self, f: impl Fn(&Connection) -> rusqlite::Result<T>) -> Result<Read<T>, LibraryError> {
        let shown = self.db.display().to_string();
        if !std::fs::metadata(&self.db).is_ok_and(|m| m.is_file()) {
            return Err(LibraryError::Missing(shown));
        }
        let unreadable = |e: rusqlite::Error| LibraryError::Unreadable { path: shown.clone(), why: e.to_string() };
        match open_read_only(&self.db, self.busy).and_then(|c| in_snapshot(&c, &f)) {
            Ok(value) => Ok(Read { value, from_copy: false }),
            Err(e) if wants_copy(&e) => self.read_copy(&f).map_err(|why| LibraryError::Unreadable { path: shown.clone(), why }),
            Err(e) => Err(unreadable(e)),
        }
    }

    fn read_copy<T>(&self, f: &impl Fn(&Connection) -> rusqlite::Result<T>) -> Result<Read<T>, String> {
        let mut last = String::from("the database kept changing while it was copied");
        for _ in 0..COPY_TRIES {
            let copy = Copy::of(&self.db)?;
            let Some(file) = copy.file.as_deref() else { continue };
            if !copy.steady {
                continue;
            }
            match Connection::open(file).and_then(|c| {
                c.busy_timeout(self.busy)?;
                in_snapshot(&c, f)
            }) {
                Ok(value) => return Ok(Read { value, from_copy: true }),
                Err(e) => last = format!("{e} (reading a copy, as the library was busy)"),
            }
        }
        Err(last)
    }
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

/// The key two spellings of one path share: on Windows separators become `\`, the `\\?\`
/// prefix goes and case is ignored, as the file system does; elsewhere the path itself.
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
    r.get::<_, Option<i64>>(i).ok().flatten()
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

/// `f` inside one read transaction, so its queries see one state of the database.
fn in_snapshot<T>(c: &Connection, f: &impl Fn(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let tx = c.unchecked_transaction()?;
    let out = f(c);
    // Ends the read (nothing was written).
    drop(tx);
    out
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

/// A copy of the database and its log in a temporary folder, deleted on drop.
struct Copy {
    dir: PathBuf,
    file: Option<PathBuf>,
    /// The files didn't change while they were copied.
    steady: bool,
}

impl Copy {
    fn of(db: &Path) -> Result<Copy, String> {
        let n = COPIES.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("djforge-mixforge-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut copy = Copy { dir, file: None, steady: false };
        let before = Library::new(db).stamp();
        let to = copy.dir.join("library.db");
        std::fs::copy(db, &to).map_err(|e| format!("copying {}: {e}", db.display()))?;
        for suffix in ["-wal", "-journal"] {
            let from = sidecar(db, suffix);
            if std::fs::metadata(&from).is_ok_and(|m| m.is_file()) {
                std::fs::copy(&from, sidecar(&to, suffix)).map_err(|e| format!("copying {}: {e}", from.display()))?;
            }
        }
        copy.steady = before.is_some() && Library::new(db).stamp() == before;
        copy.file = Some(to);
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
            let data = format!(r#"{{"path": {path:?}, "bpm": {bpm}, "title": "t"}}"#);
            c.execute("INSERT INTO tracks VALUES (?1, 100, 1700000000, 9, ?2)", rusqlite::params![path, data]).unwrap();
        }
        c.execute("INSERT INTO structure VALUES (?1, 100, 1700000000, ?2)", rusqlite::params![r"C:\Music\One.mp3", r#"{"segments": []}"#]).unwrap();
        db
    }

    fn snapshot(db: &Path) -> (Vec<u8>, Option<SystemTime>) {
        (std::fs::read(db).unwrap(), std::fs::metadata(db).unwrap().modified().ok())
    }

    #[test]
    fn reads_every_track_and_one_with_its_sections_leaving_the_file_untouched() {
        let dir = Scratch::new("read");
        let db = fixture(&dir.0);
        let before = snapshot(&db);
        let lib = Library::new(&db);
        let all = lib.tracks().unwrap();
        assert!(!all.from_copy);
        assert_eq!(all.value.len(), 2);
        let (row, structure) = lib.track(r"C:\Music\One.mp3").unwrap().value.unwrap();
        assert_eq!((row.size, row.mtime, row.version), (Some(100), Some(1_700_000_000), Some(9)));
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
        let r = lib.tracks().unwrap();
        assert_eq!((r.value.len(), r.from_copy), (2, false), "the uncommitted row isn't seen");
        // Committing (an exclusive lock): the read falls back on a copy of the committed state.
        writer.execute_batch("ROLLBACK; BEGIN EXCLUSIVE; DELETE FROM tracks;").unwrap();
        let r = lib.tracks().unwrap();
        assert_eq!((r.value.len(), r.from_copy), (2, true));
        assert!(lib.track(r"C:\Music\Two.flac").unwrap().from_copy);
        writer.execute_batch("ROLLBACK").unwrap();
        assert!(!lib.tracks().unwrap().from_copy);
    }

    #[test]
    fn reads_what_a_write_ahead_log_holds() {
        let dir = Scratch::new("wal");
        let db = fixture(&dir.0);
        let writer = Connection::open(&db).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer.execute("INSERT INTO tracks VALUES ('C:\\Music\\Wal.mp3', 1, 1, 9, '{}')", []).unwrap();
        // The writer stays open: the row is in the log only.
        let r = Library::new(&db).tracks().unwrap();
        assert_eq!(r.value.len(), 3, "the committed row in the log is read");
        drop(writer);
    }

    #[test]
    fn no_database_or_not_one_says_so() {
        let dir = Scratch::new("missing");
        let lib = Library::new(dir.0.join("library.db"));
        assert!(matches!(lib.tracks(), Err(LibraryError::Missing(_))));
        assert_eq!(lib.stamp(), None);
        std::fs::write(dir.0.join("library.db"), b"not a database, just some bytes that go on for a while........").unwrap();
        assert!(matches!(lib.tracks(), Err(LibraryError::Unreadable { .. })), "{:?}", lib.tracks());
        // An empty file is an empty database: no tables, no tracks.
        std::fs::write(dir.0.join("library.db"), b"").unwrap();
        assert_eq!(lib.tracks().unwrap().value, Vec::new());
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
            assert_eq!(Library::new(&db).tracks().unwrap().value.len(), 2);
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
