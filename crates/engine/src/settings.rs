//! Where djforge keeps its settings, and the decks' library as the app sets it up. Shared by the
//! app and `djforge-cli` (whose MCP server plans sets and looks after the library without the
//! app open).

use crate::Engine;
use std::ffi::OsString;
use std::path::PathBuf;

/// Which platform's settings folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
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

/// The platform this runs on.
pub fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Other
    }
}

/// [`config_dir`] on `platform` with the environment `env`.
pub fn config_dir_with(platform: Platform, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = env("SOUNDCRAFT_CONFIG_DIR") {
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
    platform_dir(platform, &env, [crate::APP_NAME, crate::APP_NAME])
}

/// The settings folder SoundCraft used on `platform` with the environment `env`; none when
/// `$SOUNDCRAFT_CONFIG_DIR` overrides the folder.
pub fn legacy_config_dir_with(platform: Platform, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
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

/// The control port the app opens when the user allows AI agents (DJ › Allow AI Agents), where
/// `djforge-cli mcp --auto` looks for it.
pub const AGENT_PORT: u16 = 47417;

/// Runs without preferences (`SOUNDCRAFT_NO_PREFS`, agents' test runs) neither read nor write them,
/// and keep out of the settings folder entirely.
pub fn prefs_enabled() -> bool {
    std::env::var_os("SOUNDCRAFT_NO_PREFS").is_none()
}

/// The decks' library: mixforge's `library.db` (only ever read) and the browser's folders (the
/// Music folder and the ones added, kept in `library-folders.json` in the settings folder), and
/// mixforge for agents ([`crate::cmd::mixforge_agent`]). Runs without preferences (agents' test
/// runs) leave the user's library, Music folder and settings alone (and keep no hot cues), unless
/// `DJFORGE_MIXFORGE_HOME` names a library on purpose. (mixforge's sidecar, which grids files
/// mixforge's library has no fresh row for, still reads mixforge's own library: it runs mixforge's
/// code.) Mixed In Key's library gives the key and energy where it has them; it too is left alone
/// without preferences.
pub fn set_up_library(engine: &mut Engine) {
    engine.set_agent(crate::cmd::mixforge_agent());
    let chosen = std::env::var_os("DJFORGE_MIXFORGE_HOME").is_some_and(|v| !v.is_empty());
    if prefs_enabled() || chosen {
        match crate::cmd::mixforge_library_db() {
            Ok(db) => engine.set_library(db),
            Err(e) => log::warn!("no mixforge library: {e}"),
        }
    }
    if !prefs_enabled() {
        engine.set_library_folders(None, None);
        return;
    }
    match crate::cmd::mik_library_db() {
        Ok(db) => engine.set_mik_library(db),
        Err(e) => log::warn!("no Mixed In Key library: {e}"),
    }
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let music = var("USERPROFILE").or_else(|| var("HOME")).map(|h| h.join("Music"));
    engine.set_library_folders(music, config_dir().map(|d| d.join("library-folders.json")));
    // The hot cues set on the decks, kept per track.
    if let Some(dir) = config_dir() {
        engine.set_hotcue_store(dir.join("hotcues.json"));
    }
}
