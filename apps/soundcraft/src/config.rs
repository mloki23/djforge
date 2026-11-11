//! Per-user settings and automation profile directories.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum Platform {
    Mac,
    Windows,
    Other,
}

/// djforge's settings folder. A nonempty `$SOUNDCRAFT_CONFIG_DIR` overrides the platform default.
/// The override is the complete directory; no application-name suffix is appended. An explicitly
/// empty value disables the settings directory instead of falling back to a personal profile.
pub fn config_dir() -> Option<PathBuf> {
    config_dir_with(platform(), |name| std::env::var_os(name))
}

/// The settings folder SoundCraft used, which djforge's first run copies across ([`migrate`]).
/// None with `$SOUNDCRAFT_CONFIG_DIR` set: an overridden folder has nothing to migrate.
pub fn legacy_config_dir() -> Option<PathBuf> {
    legacy_config_dir_with(platform(), |name| std::env::var_os(name))
}

fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Other
    }
}

fn config_dir_with(platform: Platform, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = env("SOUNDCRAFT_CONFIG_DIR") {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    platform_dir(platform, &env, [soundcraft_engine::APP_NAME, soundcraft_engine::APP_NAME])
}

fn legacy_config_dir_with(platform: Platform, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if env("SOUNDCRAFT_CONFIG_DIR").is_some() {
        return None;
    }
    platform_dir(platform, &env, ["SoundCraft", "soundcraft"])
}

/// The platform's per-user settings folder named `name` (macOS and Windows) or `unix` (elsewhere).
fn platform_dir(platform: Platform, env: &impl Fn(&str) -> Option<OsString>, [name, unix]: [&str; 2]) -> Option<PathBuf> {
    match platform {
        Platform::Mac => env("HOME").map(|h| PathBuf::from(h).join("Library/Application Support").join(name)),
        Platform::Windows => env("APPDATA").map(|a| PathBuf::from(a).join(name)),
        Platform::Other => {
            env("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| env("HOME").map(|h| PathBuf::from(h).join(".config"))).map(|c| c.join(unix))
        }
    }
}

/// What [`migrate`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Migration {
    /// Nothing to do: the settings were migrated before, or there is no SoundCraft folder.
    None,
    /// SoundCraft's settings were copied: how many files, the ones that couldn't be read (a file
    /// another program holds open, a folder without access), which were left out, and the links,
    /// which are never followed.
    Copied { files: usize, skipped: Vec<PathBuf>, links: Vec<PathBuf> },
}

/// Marks a settings folder whose SoundCraft settings were copied (or found nothing to copy).
pub const MIGRATED_MARKER: &str = "migrated-from-soundcraft.txt";
/// SoundCraft's autosave folder: only folders holding a session with unsaved work are copied.
const AUTOSAVE: &str = "Autosave";
/// Folders copied whole.
const FOLDERS: &[&str] = &["Presets"];
/// How often a step another program briefly holds (antivirus, the search indexer) is tried.
const ATTEMPTS: u32 = 4;

/// First run after the rebrand: copy what matters of SoundCraft's settings folder `old` into
/// djforge's `new`, once, leaving `old` untouched. That is the settings files at its top level
/// (`ui.json` and the like; not the logs), `Presets/`, and the autosave folders that still hold
/// a session (unsaved work to recover), with their modification times, so recovery compares
/// them as SoundCraft would. Old logs are not copied.
///
/// Done is recorded by [`MIGRATED_MARKER`] in `new`, not by `new` existing: a log or autosave
/// folder made first doesn't stop the copy. The copy merges into `new` and never replaces a file
/// there, so a second instance racing this one, or a retry after an interrupted copy, only fills
/// in what is missing. Folders a staging copy of an earlier version left behind are removed.
pub fn migrate(new: &Path, old: &Path) -> Result<Migration, String> {
    remove_staging(new);
    if new.join(MIGRATED_MARKER).exists() || !std::fs::symlink_metadata(old).is_ok_and(|m| m.is_dir()) {
        return Ok(Migration::None);
    }
    let failed = |error: std::io::Error| format!("copying the SoundCraft settings {} to {} failed: {error}", old.display(), new.display());
    std::fs::create_dir_all(new).map_err(failed)?;
    let mut copy = Copy { files: 0, skipped: Vec::new(), links: Vec::new() };
    match std::fs::read_dir(old) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let (name, source, target) = (entry.file_name(), entry.path(), new.join(entry.file_name()));
                let Ok(kind) = entry.file_type() else {
                    copy.skipped.push(source);
                    continue;
                };
                let name = name.to_string_lossy();
                if kind.is_symlink() {
                    copy.links.push(source);
                } else if kind.is_file() {
                    if !is_log(&name) {
                        copy.file(&source, &target);
                    }
                } else if FOLDERS.contains(&name.as_ref()) {
                    copy.tree(&source, &target);
                } else if name == AUTOSAVE {
                    copy.autosaves(&source, &target);
                }
            }
        }
        Err(error) => return Err(failed(error)),
    }
    let note = format!("SoundCraft settings copied from {}\n", old.display());
    // `create_new`: a racing instance's marker counts as done too.
    match std::fs::OpenOptions::new().write(true).create_new(true).open(new.join(MIGRATED_MARKER)) {
        Ok(mut f) => std::io::Write::write_all(&mut f, note.as_bytes()).map_err(failed)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failed(error)),
    }
    Ok(Migration::Copied { files: copy.files, skipped: copy.skipped, links: copy.links })
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

struct Copy {
    files: usize,
    skipped: Vec<PathBuf>,
    links: Vec<PathBuf>,
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
                Ok(kind) if kind.is_file() => self.file(&source, &target),
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

    /// Copies `from` to `to` unless `to` exists, keeping its modification time. The bytes go to
    /// a temporary file that is then linked into place, which fails rather than replace a file
    /// that appeared meanwhile.
    fn file(&mut self, from: &Path, to: &Path) {
        if std::fs::symlink_metadata(to).is_ok() {
            return;
        }
        match copy_file(from, to) {
            Ok(true) => self.files += 1,
            Ok(false) => {}
            Err(_) => self.skipped.push(from.to_path_buf()),
        }
    }
}

/// Whether `from` was copied to `to` (false: `to` appeared meanwhile and was kept).
fn copy_file(from: &Path, to: &Path) -> std::io::Result<bool> {
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = to.with_file_name(format!(".{name}.copy-{}", std::process::id()));
    let result = (|| {
        let modified = std::fs::metadata(from)?.modified()?;
        retry(|| std::fs::copy(from, &tmp).map(|_| ()))?;
        std::fs::File::options().write(true).open(&tmp)?.set_modified(modified)?;
        match retry(|| std::fs::hard_link(&tmp, to)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            // No hard links on this volume: rename, still never over a file that exists.
            Err(_) if !to.exists() => retry(|| std::fs::rename(&tmp, to)).map(|()| true),
            Err(e) => Err(e),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Runs `step`, again after a short wait while another program briefly holds the file
/// (permission denied, sharing violations), up to [`ATTEMPTS`] times.
fn retry(mut step: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut wait = std::time::Duration::from_millis(50);
    let mut attempt = 1;
    loop {
        match step() {
            Err(e) if attempt < ATTEMPTS && transient(&e) => {
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
    e.kind() == std::io::ErrorKind::PermissionDenied || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
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

        assert_eq!(migrate(&new, &old), Ok(Migration::Copied { files: 5, skipped: vec![], links: vec![] }));
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
        assert_eq!(migrate(&new, &old), Ok(Migration::Copied { files: 1, skipped: vec![], links: vec![] }));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "newer settings");
        assert_eq!(std::fs::read_to_string(new.join("Presets/EQ/Lead.json")).unwrap(), "preset");
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
