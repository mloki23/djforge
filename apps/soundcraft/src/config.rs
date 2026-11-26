//! Per-user settings and automation profile directories.

#[cfg(test)]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(test)]
use soundcraft_engine::settings::Platform;
use soundcraft_engine::settings::{config_dir_with, legacy_config_dir_with, platform};

/// djforge's settings folder ([`soundcraft_engine::settings::config_dir`]).
pub fn config_dir() -> Option<PathBuf> {
    config_dir_with(platform(), |name| std::env::var_os(name))
}

/// The settings folder SoundCraft used, which djforge's first run copies across ([`migrate`]).
/// None with `$SOUNDCRAFT_CONFIG_DIR` set: an overridden folder has nothing to migrate.
pub fn legacy_config_dir() -> Option<PathBuf> {
    legacy_config_dir_with(platform(), |name| std::env::var_os(name))
}

/// What [`migrate`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to do: the settings were migrated before, there is no SoundCraft folder, or
    /// another djforge is migrating them right now.
    None,
    /// SoundCraft's settings were copied: how many files, the ones that couldn't be read (a file
    /// another program holds open, a folder without access), which were left out, and the links,
    /// which are never followed. `finished` is false when a settings file at the top level was
    /// left out: no [`MIGRATED_MARKER`] is written, and the next start copies just those files
    /// again. After [`MIGRATION_TRIES`] starts it gives up on them: `abandoned` lists them, and
    /// the migration is marked done. A file still to copy that djforge has meanwhile saved its
    /// own of (it saves `ui.json` on exit) is never replaced: it is abandoned at once and listed
    /// in `kept` too.
    Copied { files: usize, skipped: Vec<PathBuf>, links: Vec<PathBuf>, finished: bool, abandoned: Vec<PathBuf>, kept: Vec<PathBuf> },
}

/// Marks a settings folder whose SoundCraft settings were copied (or found nothing to copy).
pub const MIGRATED_MARKER: &str = "migrated-from-soundcraft.txt";
/// The top-level settings files an unfinished migration still has to copy, and how many starts
/// have tried (JSON, [`Pending`]).
const PENDING: &str = "migration-pending.json";
/// Held locked for the whole migration, so a second djforge starting meanwhile leaves it alone.
const LOCK: &str = ".migrating.lock";
/// How many starts try to copy a settings file that can't be read before giving up on it.
pub const MIGRATION_TRIES: u32 = 5;
/// SoundCraft's autosave folder: only folders holding a session with unsaved work are copied.
const AUTOSAVE: &str = "Autosave";
/// Folders copied whole.
const FOLDERS: &[&str] = &["Presets"];
/// How often a step another program briefly holds (antivirus, the search indexer) is tried.
const ATTEMPTS: u32 = 4;

/// [`PENDING`]: the starts that tried so far, and the top-level file names still to copy.
#[derive(Debug, Default, PartialEq, Eq)]
struct Pending {
    tries: u32,
    files: Vec<String>,
}

/// First run after the rebrand: copy what matters of SoundCraft's settings folder `old` into
/// djforge's `new`, once, leaving `old` untouched. That is the settings files at its top level
/// (`ui.json` and the like; not the logs), `Presets/`, and the autosave folders that still hold
/// a session (unsaved work to recover), with their modification times, so recovery compares
/// them as SoundCraft would. Old logs are not copied.
///
/// Done is recorded by [`MIGRATED_MARKER`] in `new`, not by `new` existing: a log or autosave
/// folder made first doesn't stop the copy. The copy merges into `new` and never replaces a file
/// there. A top-level settings file that couldn't be read is retried on the next start, alone
/// (so presets and autosaves deleted meanwhile don't come back), up to [`MIGRATION_TRIES`]
/// starts. Every copy is complete and on disk before the marker is written, and what an
/// interrupted copy left (temporary files, a half-written file) is removed first. Folders a
/// staging copy of an earlier version left behind are removed too.
pub fn migrate(new: &Path, old: &Path) -> Result<Migration, String> {
    migrate_with(new, old, true)
}

/// [`migrate`]; `hard_links` false copies as on a volume without hard links (tests).
fn migrate_with(new: &Path, old: &Path, hard_links: bool) -> Result<Migration, String> {
    remove_staging(new);
    if new.join(MIGRATED_MARKER).exists() || !std::fs::symlink_metadata(old).is_ok_and(|m| m.is_dir()) {
        return Ok(Migration::None);
    }
    let failed = |error: std::io::Error| format!("copying the SoundCraft settings {} to {} failed: {error}", old.display(), new.display());
    std::fs::create_dir_all(new).map_err(failed)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(new.join(LOCK)).map_err(failed)?;
    match lock.try_lock() {
        Ok(()) => {}
        // Another djforge is migrating: it finishes the job.
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Migration::None),
        // A volume that can't take the lock: the migration goes ahead unlocked. A racing
        // instance is then possible, which is safe, as the copy never replaces a file.
        Err(std::fs::TryLockError::Error(_)) => {}
    }
    // The instance that held the lock may have finished meanwhile.
    if new.join(MIGRATED_MARKER).exists() {
        return Ok(Migration::None);
    }
    remove_copy_temps(new);
    let pending = read_pending(new);
    let mut copy = Copy { files: 0, skipped: Vec::new(), links: Vec::new(), hard_links };
    // Top-level settings files left out this time, by name.
    let mut unread: Vec<String> = Vec::new();
    // Pending files djforge has saved its own of since: kept, never replaced.
    let mut kept: Vec<PathBuf> = Vec::new();
    match &pending {
        Some(pending) => {
            for name in &pending.files {
                let (source, target) = (old.join(name), new.join(name));
                // It wasn't there when this file was listed, so djforge wrote it since.
                if std::fs::symlink_metadata(&target).is_ok() {
                    kept.push(source);
                    continue;
                }
                // A file gone from the SoundCraft folder (or now a link) has nothing to copy.
                if std::fs::symlink_metadata(&source).is_ok_and(|m| m.is_file()) && !copy.file(&source, &target) {
                    unread.push(name.clone());
                }
            }
        }
        None => {
            let entries = std::fs::read_dir(old).map_err(failed)?;
            for entry in entries.flatten() {
                let (name, source, target) = (entry.file_name(), entry.path(), new.join(entry.file_name()));
                let lossy = name.to_string_lossy();
                let Ok(kind) = entry.file_type() else {
                    copy.skipped.push(source);
                    unread.push(lossy.into_owned());
                    continue;
                };
                if kind.is_symlink() {
                    copy.links.push(source);
                } else if kind.is_file() {
                    if !is_log(&lossy) && !copy.file(&source, &target) {
                        unread.push(lossy.into_owned());
                    }
                } else if FOLDERS.contains(&lossy.as_ref()) {
                    copy.tree(&source, &target);
                } else if lossy == AUTOSAVE {
                    copy.autosaves(&source, &target);
                }
            }
        }
    }
    let tries = pending.map_or(0, |p| p.tries) + 1;
    let Copy { files, skipped, links, .. } = copy;
    if !unread.is_empty() && tries < MIGRATION_TRIES {
        write_pending(new, &Pending { tries, files: unread }).map_err(failed)?;
        return Ok(Migration::Copied { files, skipped, links, finished: false, abandoned: kept.clone(), kept });
    }
    let abandoned = kept.iter().cloned().chain(unread.iter().map(|name| old.join(name))).collect();
    let note = format!("SoundCraft settings copied from {}\n", old.display());
    // `create_new`: a racing instance's marker counts as done too.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(new.join(MIGRATED_MARKER)) {
        Ok(mut f) => {
            std::io::Write::write_all(&mut f, note.as_bytes()).map_err(failed)?;
            f.sync_all().map_err(failed)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failed(error)),
    }
    let _ = std::fs::remove_file(new.join(PENDING));
    Ok(Migration::Copied { files, skipped, links, finished: true, abandoned, kept })
}

/// The [`PENDING`] list an unfinished migration left in `new`, if any. An unreadable list starts
/// the migration over, which only fills in what is missing.
fn read_pending(new: &Path) -> Option<Pending> {
    let bytes = std::fs::read(new.join(PENDING)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let tries = u32::try_from(value.get("tries")?.as_u64()?).ok()?;
    let files = value.get("files")?.as_array()?.iter().map(|f| f.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?;
    Some(Pending { tries, files })
}

/// Writes [`PENDING`] whole: to a temporary file first, then renamed over the previous list.
fn write_pending(new: &Path, pending: &Pending) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(&serde_json::json!({"tries": pending.tries, "files": pending.files})).map_err(std::io::Error::other)?;
    let tmp = new.join(format!(".{PENDING}.copy-{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, new.join(PENDING))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

fn is_log(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".log") || lower.ends_with(".tmp")
}

/// Removes `.<name>.migrating-*` folders beside `new`, left by an interrupted earlier copy.
fn remove_staging(new: &Path) {
    let (Some(parent), Some(name)) = (new.parent(), new.file_name()) else { return };
    let prefix = format!(".{}.migrating-", name.to_string_lossy());
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Removes, anywhere under `dir`, what an interrupted copy left behind: `.<name>.copy-<pid>`
/// temporaries, and for each `.<name>.partial-<pid>` sentinel the half-written `<name>` it
/// guards, with the sentinel. Safe because the migration runs, locked, before djforge writes
/// anything there. Links are not followed.
fn remove_copy_temps(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => remove_copy_temps(&entry.path()),
            Ok(kind) if kind.is_file() && temp_of(&name, ".copy-").is_some() => {
                let _ = std::fs::remove_file(entry.path());
            }
            Ok(kind) if kind.is_file() => {
                if let Some(target) = temp_of(&name, ".partial-") {
                    let _ = std::fs::remove_file(dir.join(target));
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            _ => {}
        }
    }
}

/// The `<name>` of a `.<name><kind><digits>` file a copy makes beside `<name>`.
fn temp_of<'a>(file: &'a str, kind: &str) -> Option<&'a str> {
    let (name, pid) = file.strip_prefix('.')?.rsplit_once(kind)?;
    (!name.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())).then_some(name)
}

/// The sentinel [`copy_new`] keeps beside `to` while writing it, or the temporary [`copy_file`]
/// links into place: `.<name><kind><pid>`.
fn temp_beside(to: &Path, kind: &str) -> PathBuf {
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    to.with_file_name(format!(".{name}{kind}{}", std::process::id()))
}

struct Copy {
    files: usize,
    skipped: Vec<PathBuf>,
    links: Vec<PathBuf>,
    /// Whether to place copies with hard links; false once the volume refuses one.
    hard_links: bool,
}

impl Copy {
    /// Every file under `from`, merged into `to`.
    fn tree(&mut self, from: &Path, to: &Path) {
        let entries = match std::fs::read_dir(from) {
            Ok(entries) => entries,
            Err(_) => {
                self.skipped.push(from.to_path_buf());
                return;
            }
        };
        if std::fs::create_dir_all(to).is_err() {
            self.skipped.push(from.to_path_buf());
            return;
        }
        for entry in entries {
            let Ok(entry) = entry else {
                self.skipped.push(from.to_path_buf());
                continue;
            };
            let (source, target) = (entry.path(), to.join(entry.file_name()));
            match entry.file_type() {
                Ok(kind) if kind.is_symlink() => self.links.push(source),
                Ok(kind) if kind.is_dir() => self.tree(&source, &target),
                Ok(kind) if kind.is_file() => {
                    self.file(&source, &target);
                }
                Ok(_) => {}
                Err(_) => self.skipped.push(source),
            }
        }
    }

    /// The autosave folders under `from` that hold a session: unsaved work, not history.
    fn autosaves(&mut self, from: &Path, to: &Path) {
        let Ok(entries) = std::fs::read_dir(from) else {
            self.skipped.push(from.to_path_buf());
            return;
        };
        for entry in entries.flatten() {
            let folder = entry.path();
            let holds_session = ["session.scraft", "session.djforge"].iter().any(|f| folder.join(f).is_file());
            if entry.file_type().is_ok_and(|t| t.is_dir()) && holds_session {
                self.tree(&folder, &to.join(entry.file_name()));
            }
        }
    }

    /// Copies `from` to `to` unless `to` exists, keeping its modification time ([`copy_file`]).
    /// False if it couldn't be copied (it is then in `skipped`).
    fn file(&mut self, from: &Path, to: &Path) -> bool {
        if std::fs::symlink_metadata(to).is_ok() {
            return true;
        }
        match copy_file(from, to, &mut self.hard_links) {
            Ok(copied) => {
                self.files += usize::from(copied);
                true
            }
            Err(_) => {
                self.skipped.push(from.to_path_buf());
                false
            }
        }
    }
}

/// Whether `from` was copied to `to` (false: `to` appeared meanwhile and was kept). Never
/// replaces a file, and the copy is on disk when this returns. With `hard_links`, the bytes go
/// to a temporary file that is then linked into place, so `to` appears whole or not at all. A
/// volume that refuses the link itself (a network share, FAT) clears `hard_links` for the rest of
/// the run, and the copy is written straight into a new `to` instead ([`copy_new`]).
fn copy_file(from: &Path, to: &Path, hard_links: &mut bool) -> std::io::Result<bool> {
    let modified = std::fs::metadata(from)?.modified()?;
    if *hard_links {
        let tmp = temp_beside(to, ".copy-");
        let staged = (|| {
            retry(|| std::fs::copy(from, &tmp).map(|_| ()))?;
            let file = std::fs::File::options().write(true).open(&tmp)?;
            file.set_modified(modified)?;
            file.sync_all()
        })();
        if let Err(e) = staged {
            // Reading or writing the copy failed, not the link: hard links may still work.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        // Not retried on permission denied: that is how some shares refuse hard links.
        let linked = retry_if(|| std::fs::hard_link(&tmp, to), sharing_violation);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            // The temporary file vanished: nothing says the volume can't link.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(e),
            Err(_) => *hard_links = false,
        }
    }
    copy_new(from, to, modified)
}

/// Copies `from` into a new file `to` with modification time `modified`; false if `to` exists.
/// A `.<name>.partial-<pid>` sentinel stands beside `to` until it is complete and on disk, so
/// a copy killed partway is recognised and removed by the next start ([`remove_copy_temps`]);
/// one that fails here is removed at once.
fn copy_new(from: &Path, to: &Path, modified: std::time::SystemTime) -> std::io::Result<bool> {
    let sentinel = temp_beside(to, ".partial-");
    std::fs::File::create(&sentinel)?.sync_all()?;
    let mut target = match std::fs::OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(file) => file,
        Err(e) => {
            let _ = std::fs::remove_file(&sentinel);
            return if e.kind() == std::io::ErrorKind::AlreadyExists { Ok(false) } else { Err(e) };
        }
    };
    let written = (|| {
        let mut source = None;
        retry(|| std::fs::File::open(from).map(|f| source = Some(f)))?;
        let Some(mut source) = source else { return Err(std::io::Error::other("the source was not opened")) };
        std::io::copy(&mut source, &mut target)?;
        target.set_modified(modified)?;
        target.sync_all()
    })();
    drop(target);
    if written.is_err() {
        let _ = std::fs::remove_file(to);
    }
    let _ = std::fs::remove_file(&sentinel);
    written.map(|()| true)
}

/// Runs `step`, again after a short wait while another program briefly holds the file
/// (permission denied, sharing violations), up to [`ATTEMPTS`] times.
fn retry(step: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    retry_if(step, transient)
}

/// [`retry`], waiting only on the errors `again` accepts.
fn retry_if(mut step: impl FnMut() -> std::io::Result<()>, again: fn(&std::io::Error) -> bool) -> std::io::Result<()> {
    let mut wait = std::time::Duration::from_millis(50);
    let mut attempt = 1;
    loop {
        match step() {
            Err(e) if attempt < ATTEMPTS && again(&e) => {
                std::thread::sleep(wait);
                wait *= 2;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// An error another program holding the file causes for a moment (Windows sharing and lock
/// violations are raw OS errors 32 and 33).
fn transient(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || sharing_violation(e)
}

/// A Windows sharing or lock violation (raw OS errors 32 and 33).
fn sharing_violation(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33))
}

/// Read bounded settings, recovering unreadable docking metadata independently of other settings.
pub fn load_ui(path: &std::path::Path) -> Result<Option<soundcraft_ui_egui::UiState>, String> {
    read_ui(path, true)
}

fn read_ui(path: &std::path::Path, recover_docking: bool) -> Result<Option<soundcraft_ui_egui::UiState>, String> {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Settings could not be read: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_UI_BYTES + 1).read_to_end(&mut bytes).map_err(|error| format!("Settings could not be read: {error}"))?;
    if bytes.len() as u64 > MAX_UI_BYTES {
        return Err("Settings file exceeds the 4 MiB limit".into());
    }
    let mut saved: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| format!("Settings could not be restored: {error}"))?;
    if recover_docking && let Some(fields) = saved.as_object_mut() {
        for (field, mix) in [("docking", false), ("mix_docking", true)] {
            let unreadable =
                fields.get(field).is_some_and(|value| match serde_json::from_value::<soundcraft_ui_egui::docking::Workspace>(value.clone()) {
                    Ok(workspace) => if mix { workspace.validate_mix() } else { workspace.validate() }.is_err(),
                    Err(_) => true,
                });
            if unreadable {
                fields.remove(field);
            }
        }
    }
    let state: soundcraft_ui_egui::UiState = serde_json::from_value(saved).map_err(|error| format!("Settings could not be restored: {error}"))?;
    state.docking.validate()?;
    state.mix_docking.validate_mix()?;
    Ok(Some(state))
}

const MAX_UI_BYTES: u64 = 4 * 1024 * 1024;

/// Write settings atomically. Preserve an unreadable or malformed existing file for recovery.
pub fn save_ui(path: &std::path::Path, ui: &soundcraft_ui_egui::UiState) -> Result<(), String> {
    use std::io::Write as _;
    // Loading can recover a future layout for this session, but saving must preserve its source.
    read_ui(path, false)?;
    ui.docking.validate()?;
    ui.mix_docking.validate_mix()?;
    let mut saved = ui.clone();
    saved.plugin_windows.clear();
    saved.status.clear();
    let bytes = serde_json::to_vec_pretty(&saved).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_UI_BYTES {
        return Err("Settings exceed the 4 MiB limit".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_nanos();
    let temporary = path.with_extension(format!("json.{}.{nonce}.tmp", std::process::id()));
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(|error| format!("Settings could not be saved: {error}"))?;
    let written = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written.map_err(|error| format!("Settings could not be saved: {error}"))
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn native_file_reload_preserves_edit_mix_and_hidden_floating_panels() {
        use soundcraft_ui_egui::{MainWindow, Services, SoundApp, docking::Panel};
        let directory = std::env::temp_dir().join(format!("soundcraft-dock-prefs-{}", std::process::id()));
        let path = directory.join("ui.json");
        let _ = std::fs::remove_dir_all(&directory);
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.run("ui.dock", serde_json::json!({"operation":"float","panel":"tracks","rect":[80,90,260,400]})).unwrap();
        app.run("ui.dock", serde_json::json!({"operation":"close","panel":"tracks"})).unwrap();
        app.ui.theme = soundcraft_ui_egui::theme::ThemeMode::Light;
        let edit = serde_json::to_value(&app.ui.docking).unwrap();
        app.ui.window = MainWindow::Mix;
        app.run("ui.dock", serde_json::json!({"operation":"open","panel":"tracks"})).unwrap();
        app.run("ui.dock", serde_json::json!({"operation":"float","panel":"tracks","rect":[140,150,280,420]})).unwrap();
        save_ui(&path, &app.ui).unwrap();
        app.ui = load_ui(&path).unwrap().unwrap();
        assert_eq!(app.ui.theme, soundcraft_ui_egui::theme::ThemeMode::Light);
        assert_eq!(serde_json::to_value(&app.ui.docking).unwrap(), edit);
        assert_eq!(app.ui.mix_docking.layout.floating.iter().find(|g| g.panels.contains(&Panel::Tracks)).unwrap().rect, [140.0, 150.0, 280.0, 420.0]);
        app.ui.window = MainWindow::Edit;
        app.run("ui.dock", serde_json::json!({"operation":"open","panel":"tracks"})).unwrap();
        assert_eq!(app.ui.docking.layout.floating.iter().find(|g| g.panels.contains(&Panel::Tracks)).unwrap().rect, [80.0, 90.0, 260.0, 400.0]);
        let mut invalid_mix = app.ui.clone();
        invalid_mix.mix_docking = soundcraft_ui_egui::docking::Workspace::default();
        let invalid_bytes = serde_json::to_vec(&invalid_mix).unwrap();
        std::fs::write(&path, &invalid_bytes).unwrap();
        let recovered = load_ui(&path).unwrap().unwrap();
        assert_eq!(recovered.theme, app.ui.theme);
        assert_eq!(serde_json::to_value(recovered.docking).unwrap(), serde_json::to_value(&app.ui.docking).unwrap());
        recovered.mix_docking.validate_mix().unwrap();
        assert!(save_ui(&path, &app.ui).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), invalid_bytes);
        std::fs::write(&path, b"damaged settings").unwrap();
        assert!(load_ui(&path).is_err());
        assert!(save_ui(&path, &app.ui).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"damaged settings");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn future_edit_and_mix_layouts_preserve_other_preferences_and_the_original_file() {
        use soundcraft_ui_egui::{Services, SoundApp};
        let directory = std::env::temp_dir().join(format!("soundcraft-future-dock-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ui.json");
        let mut app = SoundApp::new(soundcraft_engine::Engine::default(), None, Services::default());
        app.run("ui.dock", serde_json::json!({"operation":"float","panel":"tracks","rect":[80,90,260,400]})).unwrap();
        app.ui.theme = soundcraft_ui_egui::theme::ThemeMode::Light;
        app.ui.narrow_mix = true;
        for field in ["docking", "mix_docking"] {
            for invalid in [serde_json::json!({"layout":{"root":{"FutureLayout":{}}}}), serde_json::json!(false)] {
                let mut saved = serde_json::to_value(&app.ui).unwrap();
                saved[field] = invalid;
                assert!(serde_json::from_value::<soundcraft_ui_egui::UiState>(saved.clone()).is_err());
                let bytes = serde_json::to_vec(&saved).unwrap();
                std::fs::write(&path, &bytes).unwrap();
                let recovered = load_ui(&path).unwrap().unwrap();
                assert_eq!(recovered.theme, app.ui.theme);
                assert!(recovered.narrow_mix);
                recovered.docking.validate().unwrap();
                recovered.mix_docking.validate_mix().unwrap();
                let other = if field == "docking" { "mix_docking" } else { "docking" };
                assert_eq!(serde_json::to_value(&recovered).unwrap()[other], saved[other]);
                assert!(save_ui(&path, &recovered).is_err());
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// MIDI mappings are preferences: saved and loaded with the settings file, every field kept.
    #[test]
    fn midi_mappings_survive_saving_and_loading_the_settings() {
        use soundcraft_ui_egui::midi_learn::{DeviceKey, Mapping, Matcher, Mode, Target, TrackRef};
        let directory = std::env::temp_dir().join(format!("soundcraft-midi-map-prefs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let path = directory.join("ui.json");
        let mut ui = soundcraft_ui_egui::UiState::default();
        let device = DeviceKey { backend_id: "fake:0".into(), raw_name: "DDJ".into(), name: "DDJ".into() };
        ui.midi_mappings = vec![
            Mapping {
                device: device.clone(),
                matcher: Matcher::Cc { ch: 0, cc: 7 },
                target: Target::TrackVolume { track: TrackRef { id: 3, name: "Kick".into() } },
                mode: Mode::Absolute,
                sensitivity: 0.01,
                pickup: false,
            },
            Mapping {
                device: device.clone(),
                matcher: Matcher::Cc { ch: 1, cc: 33 },
                target: Target::InsertParam { track: TrackRef { id: 4, name: "Bass".into() }, slot: 2, param: "cutoff".into() },
                mode: Mode::RelTwos,
                sensitivity: 0.05,
                pickup: true,
            },
            Mapping {
                device,
                matcher: Matcher::Note { ch: 9, note: 36 },
                target: Target::Command { id: "transport.play".into(), params: serde_json::json!({}) },
                mode: Mode::Momentary,
                ..Mapping::default()
            },
        ];
        save_ui(&path, &ui).unwrap();
        let back = load_ui(&path).unwrap().unwrap();
        std::fs::remove_dir_all(&directory).unwrap();
        assert_eq!(back.midi_mappings, ui.midi_mappings);
    }

    #[test]
    fn oversized_settings_are_rejected_and_preserved() {
        let directory = std::env::temp_dir().join(format!("soundcraft-bounded-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("ui.json");
        let bytes = vec![b' '; MAX_UI_BYTES as usize + 1];
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_ui(&path).is_err());
        assert!(save_ui(&path, &soundcraft_ui_egui::UiState::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn resolve(platform: Platform, vars: &[(&str, &str)]) -> Option<PathBuf> {
        config_dir_with(platform, |name| vars.iter().find_map(|(key, value)| (*key == name).then(|| OsString::from(value))))
    }

    #[test]
    fn config_override_wins_without_reading_user_profile() {
        for platform in [Platform::Mac, Platform::Windows, Platform::Other] {
            let actual = config_dir_with(platform, |name| {
                assert_eq!(name, "SOUNDCRAFT_CONFIG_DIR");
                Some(OsString::from("isolated/settings"))
            });
            assert_eq!(actual, Some(PathBuf::from("isolated/settings")));
        }
    }

    #[test]
    fn config_empty_override_disables_profile_without_reading_defaults() {
        for platform in [Platform::Mac, Platform::Windows, Platform::Other] {
            assert_eq!(
                config_dir_with(platform, |name| {
                    assert_eq!(name, "SOUNDCRAFT_CONFIG_DIR");
                    Some(OsString::new())
                }),
                None
            );
        }
    }

    #[test]
    fn config_unset_override_keeps_platform_defaults() {
        let vars = [("HOME", "/user"), ("APPDATA", "/roaming"), ("XDG_CONFIG_HOME", "/xdg")];
        assert_eq!(resolve(Platform::Mac, &vars), Some(PathBuf::from("/user/Library/Application Support/djforge")));
        assert_eq!(resolve(Platform::Windows, &vars), Some(PathBuf::from("/roaming/djforge")));
        assert_eq!(resolve(Platform::Other, &vars), Some(PathBuf::from("/xdg/djforge")));
        assert_eq!(resolve(Platform::Other, &[("HOME", "/user")]), Some(PathBuf::from("/user/.config/djforge")));
        for platform in [Platform::Mac, Platform::Windows, Platform::Other] {
            assert_eq!(resolve(platform, &[]), None);
        }
    }

    #[test]
    fn the_soundcraft_folder_is_where_soundcraft_kept_its_settings() {
        let legacy = |platform, vars: &[(&str, &str)]| {
            legacy_config_dir_with(platform, |name| vars.iter().find_map(|(key, value)| (*key == name).then(|| OsString::from(value))))
        };
        let vars = [("HOME", "/user"), ("APPDATA", "/roaming"), ("XDG_CONFIG_HOME", "/xdg")];
        assert_eq!(legacy(Platform::Mac, &vars), Some(PathBuf::from("/user/Library/Application Support/SoundCraft")));
        assert_eq!(legacy(Platform::Windows, &vars), Some(PathBuf::from("/roaming/SoundCraft")));
        assert_eq!(legacy(Platform::Other, &vars), Some(PathBuf::from("/xdg/soundcraft")));
        let overridden = [("SOUNDCRAFT_CONFIG_DIR", "/isolated"), ("APPDATA", "/roaming")];
        assert_eq!(legacy(Platform::Windows, &overridden), None, "an overridden folder migrates nothing");
    }

    fn migration_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("djforge-migrate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_soundcraft_settings_are_copied_once_and_left_untouched() {
        let base = migration_dir("once");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), r#"{"ui_scale": 1.5}"#);
        write(&old.join("Presets/EQ/Lead.json"), "preset");
        write(&old.join("Autosave/Song 1/session.scraft"), "{}");
        write(&old.join("Autosave/Song 1/lock"), "");
        write(&old.join("Autosave/Song 1/Audio Files/Take.wav"), "wav");
        write(&old.join("Autosave/Empty/origin"), "history without a session");
        write(&old.join("logs/soundcraft.log"), "old log");
        write(&old.join("soundcraft.log"), "stray log");
        // A log or autosave folder made before the migration ran doesn't stop it.
        write(&new.join("logs/djforge.log"), "new log");
        let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(old.join("Autosave/Song 1/session.scraft")).unwrap().set_modified(stamp).unwrap();
        // An earlier version's interrupted staging copy is cleared away.
        std::fs::create_dir_all(base.join(".djforge.migrating-123/Presets")).unwrap();

        assert_eq!(
            migrate(&new, &old),
            Ok(Migration::Copied { files: 5, skipped: vec![], links: vec![], finished: true, abandoned: vec![], kept: vec![] })
        );
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), r#"{"ui_scale": 1.5}"#);
        assert_eq!(std::fs::read_to_string(new.join("Presets/EQ/Lead.json")).unwrap(), "preset");
        assert_eq!(std::fs::read_to_string(new.join("Autosave/Song 1/Audio Files/Take.wav")).unwrap(), "wav");
        assert!(new.join("Autosave/Song 1/lock").is_file());
        let modified = std::fs::metadata(new.join("Autosave/Song 1/session.scraft")).unwrap().modified().unwrap();
        assert_eq!(modified, stamp, "copies keep their modification time");
        assert!(!new.join("Autosave/Empty").exists(), "autosave history isn't copied");
        assert!(!new.join("logs/soundcraft.log").exists() && !new.join("soundcraft.log").exists(), "old logs aren't copied");
        assert_eq!(std::fs::read_to_string(new.join("logs/djforge.log")).unwrap(), "new log");
        assert!(new.join(MIGRATED_MARKER).is_file());
        assert!(!base.join(".djforge.migrating-123").exists());
        assert_eq!(std::fs::read_to_string(old.join("ui.json")).unwrap(), r#"{"ui_scale": 1.5}"#, "the old folder stays");
        assert!(old.join("logs/soundcraft.log").is_file() && old.join("Autosave/Song 1/session.scraft").is_file());

        // Idempotent: djforge's own changes are never overwritten by a second copy.
        std::fs::write(new.join("ui.json"), "{}").unwrap();
        std::fs::write(old.join("ui.json"), r#"{"ui_scale": 2.0}"#).unwrap();
        assert_eq!(migrate(&new, &old), Ok(Migration::None));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_unfinished_copy_resumes_without_replacing_what_is_there() {
        let base = migration_dir("resume");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "old settings");
        write(&old.join("Presets/EQ/Lead.json"), "preset");
        // Another instance (or an interrupted run) already wrote ui.json, but no marker.
        write(&new.join("ui.json"), "newer settings");
        assert_eq!(
            migrate(&new, &old),
            Ok(Migration::Copied { files: 1, skipped: vec![], links: vec![], finished: true, abandoned: vec![], kept: vec![] })
        );
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "newer settings");
        assert_eq!(std::fs::read_to_string(new.join("Presets/EQ/Lead.json")).unwrap(), "preset");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn without_hard_links_files_are_copied_whole_and_never_replace_one() {
        let base = migration_dir("no-links");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "old settings");
        write(&old.join("midi.json"), "mappings");
        write(&old.join("Presets/EQ/Lead.json"), "preset");
        write(&new.join("ui.json"), "newer settings");
        let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(old.join("midi.json")).unwrap().set_modified(stamp).unwrap();
        assert_eq!(
            migrate_with(&new, &old, false),
            Ok(Migration::Copied { files: 2, skipped: vec![], links: vec![], finished: true, abandoned: vec![], kept: vec![] })
        );
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "newer settings", "never replaced");
        assert_eq!(std::fs::read_to_string(new.join("midi.json")).unwrap(), "mappings");
        assert_eq!(std::fs::metadata(new.join("midi.json")).unwrap().modified().unwrap(), stamp);
        assert_eq!(std::fs::read_to_string(new.join("Presets/EQ/Lead.json")).unwrap(), "preset");
        assert!(new.join(MIGRATED_MARKER).is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn temporary_copies_an_interrupted_run_left_are_removed() {
        let base = migration_dir("temps");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "{}");
        write(&new.join(".ui.json.copy-4242"), "half a copy");
        write(&new.join("Presets/EQ/.Lead.json.copy-17"), "half a preset");
        write(&new.join("Presets/.notes.copy-me"), "a user's file");
        assert!(matches!(migrate(&new, &old), Ok(Migration::Copied { finished: true, .. })));
        assert!(!new.join(".ui.json.copy-4242").exists() && !new.join("Presets/EQ/.Lead.json.copy-17").exists());
        assert!(new.join("Presets/.notes.copy-me").is_file(), "only copy temporaries are removed");
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn a_settings_file_that_cannot_be_read_is_tried_again_next_start() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let base = migration_dir("locked");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "settings");
        write(&old.join("midi.json"), "mappings");
        // Another program holds ui.json open without sharing it.
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(old.join("ui.json")).unwrap();
        let Ok(Migration::Copied { files: 1, skipped, finished: false, .. }) = migrate(&new, &old) else { panic!("partly copied") };
        assert_eq!(skipped, vec![old.join("ui.json")]);
        assert!(!new.join(MIGRATED_MARKER).exists(), "no marker: the next start tries again");
        assert!(!new.join("ui.json").exists());
        drop(held);
        assert_eq!(
            migrate(&new, &old),
            Ok(Migration::Copied { files: 1, skipped: vec![], links: vec![], finished: true, abandoned: vec![], kept: vec![] })
        );
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "settings");
        assert!(new.join(MIGRATED_MARKER).is_file());
        assert!(!new.join(PENDING).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn a_retry_copies_only_the_files_left_out_so_deleted_ones_stay_deleted() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let base = migration_dir("retry-only");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "settings");
        write(&old.join("Presets/EQ/Lead.json"), "preset");
        write(&old.join("Autosave/Song/session.scraft"), "{}");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(old.join("ui.json")).unwrap();
        assert!(matches!(migrate(&new, &old), Ok(Migration::Copied { files: 2, finished: false, .. })));
        drop(held);
        // djforge ran meanwhile, and its user deleted the preset and the autosave.
        std::fs::remove_dir_all(new.join("Presets")).unwrap();
        std::fs::remove_dir_all(new.join("Autosave")).unwrap();
        assert!(matches!(migrate(&new, &old), Ok(Migration::Copied { files: 1, finished: true, .. })));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "settings");
        assert!(!new.join("Presets").exists() && !new.join("Autosave").exists(), "deleted files don't come back");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn a_settings_file_djforge_saved_itself_is_kept_and_not_reported_as_copied() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let base = migration_dir("saved-own");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "soundcraft settings");
        write(&old.join("midi.json"), "soundcraft mappings");
        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(old.join("ui.json")).unwrap();
        let held_midi = std::fs::OpenOptions::new().read(true).share_mode(0).open(old.join("midi.json")).unwrap();
        assert!(matches!(migrate(&new, &old), Ok(Migration::Copied { files: 0, finished: false, .. })));
        // djforge ran and saved its own ui.json on exit; midi.json is still held.
        std::fs::write(new.join("ui.json"), "djforge settings").unwrap();
        drop(held);
        let Ok(Migration::Copied { files: 0, finished: false, abandoned, kept, .. }) = migrate(&new, &old) else { panic!("kept") };
        assert_eq!((abandoned, kept), (vec![old.join("ui.json")], vec![old.join("ui.json")]));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "djforge settings", "never replaced");
        drop(held_midi);
        // Only midi.json was still pending.
        assert_eq!(
            migrate(&new, &old),
            Ok(Migration::Copied { files: 1, skipped: vec![], links: vec![], finished: true, abandoned: vec![], kept: vec![] })
        );
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "djforge settings");
        assert_eq!(std::fs::read_to_string(new.join("midi.json")).unwrap(), "soundcraft mappings");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn a_settings_file_that_never_reads_is_given_up_after_five_starts() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let base = migration_dir("give-up");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "settings");
        let _held = std::fs::OpenOptions::new().read(true).share_mode(0).open(old.join("ui.json")).unwrap();
        for start in 1..MIGRATION_TRIES {
            assert!(matches!(migrate(&new, &old), Ok(Migration::Copied { finished: false, .. })), "start {start}");
            assert!(!new.join(MIGRATED_MARKER).exists());
        }
        let Ok(Migration::Copied { finished: true, abandoned, .. }) = migrate(&new, &old) else { panic!("given up") };
        assert_eq!(abandoned, vec![old.join("ui.json")]);
        assert!(new.join(MIGRATED_MARKER).is_file() && !new.join(PENDING).exists());
        assert_eq!(migrate(&new, &old), Ok(Migration::None));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_copy_killed_partway_is_removed_and_copied_again() {
        let base = migration_dir("partial");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), r#"{"ui_scale": 1.5}"#);
        write(&old.join("Presets/EQ/Lead.json"), "preset");
        // A run without hard links was killed while writing these: truncated, sentinels beside.
        write(&new.join("ui.json"), "");
        write(&new.join(".ui.json.partial-4242"), "");
        write(&new.join("Presets/EQ/Lead.json"), "pre");
        write(&new.join("Presets/EQ/.Lead.json.partial-17"), "");
        assert!(matches!(migrate_with(&new, &old, false), Ok(Migration::Copied { files: 2, finished: true, .. })));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), r#"{"ui_scale": 1.5}"#);
        assert_eq!(std::fs::read_to_string(new.join("Presets/EQ/Lead.json")).unwrap(), "preset");
        assert!(!new.join(".ui.json.partial-4242").exists() && !new.join("Presets/EQ/.Lead.json.partial-17").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_second_instance_leaves_a_running_migration_alone() {
        let base = migration_dir("lock");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&old.join("ui.json"), "{}");
        std::fs::create_dir_all(&new).unwrap();
        let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(new.join(LOCK)).unwrap();
        held.try_lock().unwrap();
        assert_eq!(migrate(&new, &old), Ok(Migration::None));
        assert!(!new.join("ui.json").exists() && !new.join(MIGRATED_MARKER).exists());
        drop(held);
        assert!(matches!(migrate(&new, &old), Ok(Migration::Copied { files: 1, finished: true, .. })));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_failed_read_does_not_turn_hard_links_off() {
        let base = migration_dir("links-kept");
        let mut hard_links = true;
        assert!(copy_file(&base.join("missing.json"), &base.join("copy.json"), &mut hard_links).is_err());
        assert!(hard_links, "only a refused hard link turns them off");
        write(&base.join("ui.json"), "{}");
        assert_eq!(copy_file(&base.join("ui.json"), &base.join("copy.json"), &mut hard_links).ok(), Some(true));
        assert_eq!(copy_file(&base.join("ui.json"), &base.join("copy.json"), &mut hard_links).ok(), Some(false), "never replaced");
        assert!(hard_links);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn nothing_is_migrated_without_a_soundcraft_folder() {
        let base = migration_dir("none");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        assert_eq!(migrate(&new, &old), Ok(Migration::None));
        assert!(!new.exists(), "no empty djforge folder is made");
        std::fs::write(&old, "a file, not a folder").unwrap();
        assert_eq!(migrate(&new, &old), Ok(Migration::None));
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_reported_and_not_followed() {
        let base = migration_dir("links");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        write(&base.join("elsewhere/secret.json"), "not ours");
        write(&old.join("ui.json"), "{}");
        std::os::unix::fs::symlink(base.join("elsewhere/secret.json"), old.join("Presets.json")).unwrap();
        let Ok(Migration::Copied { links, .. }) = migrate(&new, &old) else { panic!("copied") };
        assert_eq!(links, vec![old.join("Presets.json")]);
        assert!(!new.join("Presets.json").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn config_override_preserves_non_unicode_paths() {
        use std::os::unix::ffi::OsStringExt as _;
        let value = OsString::from_vec(b"isolated/\xff".to_vec());
        assert_eq!(config_dir_with(Platform::Mac, |_| Some(value.clone())), Some(PathBuf::from(value)));
    }
}
