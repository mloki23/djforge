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
    platform_dir(platform, &env, ["djforge", "djforge"])
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
    /// Nothing to do: the djforge folder exists already, or there is no SoundCraft folder.
    None,
    /// The SoundCraft folder was copied: how many files, and the ones that couldn't be read
    /// (a file another program holds open, say), which were left out.
    Copied { files: usize, skipped: Vec<PathBuf> },
}

/// First run after the rebrand: when the settings folder `new` doesn't exist but SoundCraft's
/// `old` does, copy `old` into it once, recursively, leaving `old` untouched. The copy is built
/// beside `new` and renamed into place, so an interrupted copy leaves no half folder and the next
/// start tries again; once `new` exists this does nothing. Links are not followed.
pub fn migrate(new: &Path, old: &Path) -> Result<Migration, String> {
    if new.exists() || !std::fs::symlink_metadata(old).is_ok_and(|m| m.is_dir()) {
        return Ok(Migration::None);
    }
    let parent = new.parent().ok_or_else(|| format!("{}: no parent folder", new.display()))?;
    let name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let staging = parent.join(format!(".{name}.migrating-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let mut files = 0;
    let mut skipped = Vec::new();
    let copied = copy_tree(old, &staging, &mut files, &mut skipped).and_then(|()| std::fs::rename(&staging, new));
    if let Err(error) = copied {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("copying the SoundCraft settings {} to {} failed: {error}", old.display(), new.display()));
    }
    Ok(Migration::Copied { files, skipped })
}

fn copy_tree(from: &Path, to: &Path, files: &mut usize, skipped: &mut Vec<PathBuf>) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&source, &target, files, skipped)?;
        } else if kind.is_file() {
            match std::fs::copy(&source, &target) {
                Ok(_) => *files += 1,
                Err(_) => skipped.push(source),
            }
        }
    }
    Ok(())
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

    #[test]
    fn the_soundcraft_settings_are_copied_once_and_left_untouched() {
        let base = migration_dir("once");
        let (old, new) = (base.join("SoundCraft"), base.join("djforge"));
        std::fs::create_dir_all(old.join("Autosave/Song 1")).unwrap();
        std::fs::create_dir_all(old.join("Presets")).unwrap();
        std::fs::write(old.join("ui.json"), r#"{"ui_scale": 1.5}"#).unwrap();
        std::fs::write(old.join("Autosave/Song 1/session.scraft"), "{}").unwrap();
        std::fs::write(old.join("Presets/Lead.json"), "preset").unwrap();

        assert_eq!(migrate(&new, &old), Ok(Migration::Copied { files: 3, skipped: vec![] }));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), r#"{"ui_scale": 1.5}"#);
        assert_eq!(std::fs::read_to_string(new.join("Autosave/Song 1/session.scraft")).unwrap(), "{}");
        assert_eq!(std::fs::read_to_string(new.join("Presets/Lead.json")).unwrap(), "preset");
        assert_eq!(std::fs::read_to_string(old.join("ui.json")).unwrap(), r#"{"ui_scale": 1.5}"#, "the old folder stays");
        assert!(old.join("Autosave/Song 1/session.scraft").is_file());
        let left: Vec<_> = std::fs::read_dir(&base).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 2, "no staging folder left behind: {left:?}");

        // Idempotent: djforge's own changes are never overwritten by a second copy.
        std::fs::write(new.join("ui.json"), "{}").unwrap();
        std::fs::write(old.join("ui.json"), r#"{"ui_scale": 2.0}"#).unwrap();
        assert_eq!(migrate(&new, &old), Ok(Migration::None));
        assert_eq!(std::fs::read_to_string(new.join("ui.json")).unwrap(), "{}");
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
    fn config_override_preserves_non_unicode_paths() {
        use std::os::unix::ffi::OsStringExt as _;
        let value = OsString::from_vec(b"isolated/\xff".to_vec());
        assert_eq!(config_dir_with(Platform::Mac, |_| Some(value.clone())), Some(PathBuf::from(value)));
    }
}
