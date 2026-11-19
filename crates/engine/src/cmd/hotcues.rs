//! Hot cues the user sets or clears on a deck, kept per track in djforge's own file (never in
//! the track or mixforge's library): one JSON file in the app's settings folder, set by the app
//! ([`Engine::set_hotcue_store`]; tests give a temporary one).
//!
//! A track is known by its path, size and modification time: a file that changed is a new
//! track, and what was kept for it before no longer applies. For each slot the file holds the
//! user's cue (seconds into the track as the deck decodes it) or `null` when they cleared it; a
//! slot they never touched is left out, and the computed cue (the set's, mixforge's) fills it.
//!
//! ```json
//! {"version": 1, "tracks": {"<path key>": {"path": "...", "size": 123, "mtime": 1700000000,
//!   "cues": {"1": 12.5, "3": null}}}}
//! ```

use super::*;
use serde_json::json;
use soundcraft_mix::HOT_CUES;
use std::path::Path;

/// What the user did to each hot cue of a track: untouched (`None`), cleared (`Some(None)`) or
/// set at seconds (`Some(Some(s))`).
pub(crate) type Stored = [Option<Option<f64>>; HOT_CUES];

const VERSION: u64 = 1;

fn read(store: &Path) -> Option<Value> {
    let text = crate::file_access::unconfined(|| crate::io::read_text(store)).ok()?;
    serde_json::from_str(&text).ok().filter(Value::is_object)
}

fn entry<'a>(v: &'a Value, path: &str, (size, mtime): (u64, i64)) -> Option<&'a Value> {
    let e = v.get("tracks")?.get(super::library::path_key(path))?;
    (e.get("size").and_then(Value::as_u64) == Some(size) && e.get("mtime").and_then(Value::as_i64) == Some(mtime)).then_some(e)
}

/// The hot cues the user kept for the file at `path` as it is now (`stamp`: its size and time).
pub(crate) fn stored(store: &Path, path: &str, stamp: (u64, i64)) -> Option<Stored> {
    let v = read(store)?;
    let cues = entry(&v, path, stamp)?.get("cues")?.as_object()?;
    let mut out: Stored = [None; HOT_CUES];
    for (slot, cue) in cues {
        let Some(i) = slot.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < HOT_CUES) else { continue };
        let value = match cue {
            Value::Null => Some(None),
            v => v.as_f64().filter(|s| s.is_finite() && *s >= 0.0).map(Some),
        };
        if let (Some(o), Some(v)) = (out.get_mut(i), value) {
            *o = Some(v);
        }
    }
    Some(out)
}

/// Keeps `changes` (slot, the cue in seconds or `None` for cleared) for the file at `path` as
/// it is now, written atomically. What was kept for an older state of the file goes; a store
/// that can't be read is moved aside, not overwritten.
pub(crate) fn save(store: &Path, path: &str, stamp: (u64, i64), changes: &[(usize, Option<f64>)]) -> Result<()> {
    crate::file_access::unconfined(|| {
        let mut v = match crate::io::read_text(store) {
            Ok(text) => match serde_json::from_str::<Value>(&text).ok().filter(Value::is_object) {
                Some(v) => v,
                None => {
                    crate::io::move_aside(store)?;
                    json!({})
                }
            },
            Err(_) => json!({}),
        };
        let key = super::library::path_key(path);
        let kept = entry(&v, path, stamp).and_then(|e| e.get("cues")).and_then(Value::as_object).cloned().unwrap_or_default();
        let mut cues = kept;
        for (slot, cue) in changes.iter().filter(|(s, _)| *s < HOT_CUES) {
            cues.insert((slot + 1).to_string(), cue.map_or(Value::Null, |s| json!(s)));
        }
        let Some(root) = v.as_object_mut() else { return Err(EngineError::Io(format!("{}: not an object", store.display()))) };
        root.insert("version".into(), json!(VERSION));
        let tracks = root.entry("tracks").or_insert_with(|| json!({}));
        if !tracks.is_object() {
            *tracks = json!({});
        }
        if let Some(t) = tracks.as_object_mut() {
            t.insert(key, json!({"path": path, "size": stamp.0, "mtime": stamp.1, "cues": cues}));
        }
        let bytes = serde_json::to_vec_pretty(&v).map_err(|err| EngineError::Io(err.to_string()))?;
        if let Some(dir) = store.parent().filter(|d| !d.as_os_str().is_empty()) {
            crate::io::make_dir(dir)?;
        }
        crate::io::write_file_atomic(store, &bytes)
    })
}

/// `hot` (source samples) with the user's kept cues in their slots, those inside `frames`; how
/// many came from the user.
pub(crate) fn apply(hot: &mut [Option<f64>; HOT_CUES], stored: &Stored, rate: f64, frames: f64) -> usize {
    let mut n = 0;
    for (h, s) in hot.iter_mut().zip(stored) {
        match s {
            Some(Some(secs)) => {
                let at = secs * rate;
                if at.is_finite() && (0.0..frames).contains(&at) {
                    *h = Some(at);
                    n += 1;
                }
            }
            Some(None) => {
                *h = None;
                n += 1;
            }
            None => {}
        }
    }
    n
}
