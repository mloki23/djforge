//! Hot cues the user sets or clears on a deck, kept per track in djforge's own file (never in
//! the track or mixforge's library): one JSON file in the app's settings folder, set by the app
//! ([`Engine::set_hotcue_store`]; tests give a temporary one).
//!
//! A track is its audio: an entry holds the file's path, its size and time when last seen, and
//! its audio print (see [`crate::io::audio_print`]: a hash of all of the decoded audio, so tags
//! don't count, its length and its second-by-second loudness, `env`). Loading a file finds its
//! entry
//!
//! - at its path, when the hash is the same (a retag, which changes the size and time, keeps the
//!   cues; the size and time are brought up to date); or when the hash differs but the length is
//!   within 0.05 s and the envelope matches (another decoder's last bits: the hash is brought up
//!   to date); or, for an entry made before fingerprints, when the size and time are the same or
//!   the length within 0.05 s;
//! - else anywhere by hash: the file was moved or renamed, and the entry follows it, or (the
//!   file it was for still being there) a copy of it was made, and the copy gets its own entry.
//!
//! Nothing is ever dropped: an entry at a path whose audio changed goes to that path's
//! `previous` list when the new audio's cues are first kept, and `deck.load` reports the
//! previous cues so they can be restored.
//!
//! For each slot an entry holds the user's cue (seconds into the track as the deck decodes it)
//! or `null` when they cleared it; a slot they never touched is left out, and the computed cue
//! (the set's, mixforge's) fills it.
//!
//! ```json
//! {"version": 2, "tracks": {"<path key>": {"path": "...", "size": 123, "mtime": 1700000000,
//!   "fp": "0123456789abcdef", "seconds": 312.5, "env": [-14, -12, …], "cues": {"1": 12.5, "3": null},
//!   "previous": [{"size": 120, "mtime": 1600000000, "fp": "...", "seconds": 300.0,
//!                 "cues": {"1": 8.0}, "until": 1700000500}]}}}
//! ```

use super::*;
use serde_json::{Map, json};
use soundcraft_mix::HOT_CUES;
use std::path::Path;

/// What the user did to each hot cue of a track: untouched (`None`), cleared (`Some(None)`) or
/// set at seconds (`Some(Some(s))`).
pub(crate) type Stored = [Option<Option<f64>>; HOT_CUES];

const VERSION: u64 = 2;
/// How near two lengths are for an entry without a fingerprint to be the same audio.
const SAME_LENGTH_S: f64 = 0.05;

/// A track as the store knows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TrackId {
    pub path: String,
    /// The file's size and time now.
    pub stamp: (u64, i64),
    pub fp: u64,
    /// Its length in seconds.
    pub seconds: f64,
    /// Its loudness second by second (see [`crate::io::AudioPrint`]).
    pub envelope: Vec<i8>,
}

impl TrackId {
    fn fp_text(&self) -> String {
        format!("{:016x}", self.fp)
    }
}

/// What the store has for a track being loaded.
#[derive(Debug, Default)]
pub(crate) struct Recalled {
    pub cues: Option<Stored>,
    /// Cues kept for audio the file at this path held before (newest last): `{cues, until?, …}`.
    pub previous: Vec<Value>,
    /// The entry was found by fingerprint, or its size and time are old: it is to be brought up
    /// to date ([`relink`]).
    pub relink: bool,
}

fn read(store: &Path) -> Option<Value> {
    let text = crate::file_access::unconfined(|| crate::io::read_text(store)).ok()?;
    serde_json::from_str(&text).ok().filter(Value::is_object)
}

fn stamp_of(e: &Value) -> Option<(u64, i64)> {
    Some((e.get("size")?.as_u64()?, e.get("mtime")?.as_i64()?))
}

/// Whether two envelopes are of the same audio: their lengths within a second, and each second
/// within 1 dB (a step's rounding).
fn same_envelope(e: &Value, envelope: &[i8]) -> bool {
    let Some(stored) = e.get("env").and_then(Value::as_array) else { return false };
    stored.len().abs_diff(envelope.len()) <= 1
        && !envelope.is_empty()
        && stored.iter().zip(envelope).all(|(a, b)| a.as_i64().is_some_and(|a| a.abs_diff(i64::from(*b)) <= 1))
}

/// Whether the entry `e` (at the track's own path) is of the track's audio.
fn same_audio(e: &Value, id: &TrackId) -> bool {
    let same_length = e.get("seconds").and_then(Value::as_f64).is_some_and(|s| (s - id.seconds).abs() <= SAME_LENGTH_S);
    match e.get("fp").and_then(Value::as_str) {
        // Another hash of the same length and loudness: another decoder of the same audio.
        Some(fp) => fp == id.fp_text() || (same_length && same_envelope(e, &id.envelope)),
        // Made before fingerprints: the same size and time, or the same length.
        None => stamp_of(e) == Some(id.stamp) || same_length,
    }
}

/// Whether the file the entry `e` was kept for is still there.
fn still_there(e: &Value) -> bool {
    e.get("path").and_then(Value::as_str).is_some_and(crate::io::file_found)
}

/// The key of the entry for the track: at its path when of its audio, else any of its
/// fingerprint.
fn find(tracks: &Map<String, Value>, id: &TrackId) -> Option<String> {
    let here = super::library::path_key(&id.path);
    if tracks.get(&here).is_some_and(|e| same_audio(e, id)) {
        return Some(here);
    }
    let fp = id.fp_text();
    tracks.iter().find(|(_, e)| e.get("fp").and_then(Value::as_str) == Some(fp.as_str())).map(|(k, _)| k.clone())
}

fn cues_of(e: &Value) -> Stored {
    let mut out: Stored = [None; HOT_CUES];
    for (slot, cue) in e.get("cues").and_then(Value::as_object).into_iter().flatten() {
        let Some(i) = slot.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < HOT_CUES) else { continue };
        let value = match cue {
            Value::Null => Some(None),
            v => v.as_f64().filter(|s| s.is_finite() && *s >= 0.0).map(Some),
        };
        if let (Some(o), Some(v)) = (out.get_mut(i), value) {
            *o = Some(v);
        }
    }
    out
}

/// What the store has for the track `id`.
pub(crate) fn recall(store: &Path, id: &TrackId) -> Recalled {
    let Some(v) = read(store) else { return Recalled::default() };
    let Some(tracks) = v.get("tracks").and_then(Value::as_object) else { return Recalled::default() };
    let here = super::library::path_key(&id.path);
    let found = find(tracks, id);
    let at_path = tracks.get(&here);
    let mut previous: Vec<Value> = at_path.and_then(|e| e.get("previous")).and_then(Value::as_array).cloned().unwrap_or_default();
    // The entry at the path is of other audio: it becomes previous once this track's cues are kept.
    if let Some(e) = at_path.filter(|_| found.as_deref() != Some(here.as_str())) {
        let mut old = e.clone();
        if let Some(o) = old.as_object_mut() {
            o.remove("previous");
        }
        previous.push(old);
    }
    let Some(e) = found.as_ref().and_then(|k| tracks.get(k)) else { return Recalled { cues: None, previous, relink: false } };
    let relink = found.as_deref() != Some(here.as_str())
        || stamp_of(e) != Some(id.stamp)
        || e.get("fp").and_then(Value::as_str) != Some(id.fp_text().as_str());
    Recalled { cues: Some(cues_of(e)), previous, relink }
}

/// Brings the track's entry up to date (its path, size, time and fingerprint) without changing
/// its cues.
pub(crate) fn relink(store: &Path, id: &TrackId) -> Result<()> {
    save(store, id, &[])
}

/// Keeps `changes` (slot, the cue in seconds or `None` for cleared) for the track `id`, written
/// atomically. Its entry is found as [`recall`] finds it and brought up to date; an entry at its
/// path for other audio goes to the path's `previous` list. A store that can't be read is moved
/// aside, not overwritten.
pub(crate) fn save(store: &Path, id: &TrackId, changes: &[(usize, Option<f64>)]) -> Result<()> {
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
        let Some(root) = v.as_object_mut() else { return Err(EngineError::Io(format!("{}: not an object", store.display()))) };
        root.insert("version".into(), json!(VERSION));
        let tracks = root.entry("tracks").or_insert_with(|| json!({}));
        if !tracks.is_object() {
            *tracks = json!({});
        }
        let Some(tracks) = tracks.as_object_mut() else { return Err(EngineError::Io(format!("{}: no tracks", store.display()))) };
        let here = super::library::path_key(&id.path);
        let found = find(tracks, id);
        // Found elsewhere by its hash: moved there, unless its file is still where it was (this
        // is a copy, which gets an entry of its own).
        let mut entry = match &found {
            Some(k) if *k != here && tracks.get(k).is_some_and(still_there) => {
                let mut copy = tracks.get(k).cloned().unwrap_or_else(|| json!({}));
                if let Some(o) = copy.as_object_mut() {
                    o.remove("previous");
                }
                copy
            }
            Some(k) => tracks.remove(k).unwrap_or_else(|| json!({})),
            None => json!({}),
        };
        // What the path held for other audio is kept, as previous.
        let mut previous: Vec<Value> = entry.get("previous").and_then(Value::as_array).cloned().unwrap_or_default();
        if found.as_deref() != Some(here.as_str())
            && let Some(mut old) = tracks.remove(&here)
        {
            previous.extend(old.get("previous").and_then(Value::as_array).cloned().unwrap_or_default());
            if let Some(o) = old.as_object_mut() {
                o.remove("previous");
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                o.insert("until".into(), json!(now));
            }
            previous.push(old);
        }
        let mut cues = entry.get("cues").and_then(Value::as_object).cloned().unwrap_or_default();
        for (slot, cue) in changes.iter().filter(|(s, _)| *s < HOT_CUES) {
            cues.insert((slot + 1).to_string(), cue.map_or(Value::Null, |s| json!(s)));
        }
        if !entry.is_object() {
            entry = json!({});
        }
        if let Some(o) = entry.as_object_mut() {
            o.insert("path".into(), json!(id.path));
            o.insert("size".into(), json!(id.stamp.0));
            o.insert("mtime".into(), json!(id.stamp.1));
            o.insert("fp".into(), json!(id.fp_text()));
            o.insert("seconds".into(), json!(id.seconds));
            o.insert("env".into(), json!(id.envelope));
            o.insert("cues".into(), Value::Object(cues));
            if previous.is_empty() {
                o.remove("previous");
            } else {
                o.insert("previous".into(), Value::Array(previous));
            }
        }
        tracks.insert(here, entry);
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

/// A hot cue set or cleared on a deck, waiting for the deck to report it applied the edit (a
/// set cue snaps to the beat on the audio thread, which may be late to report).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingCue {
    pub deck: usize,
    /// The deck's load the edit was made on.
    pub load: u64,
    pub slot: usize,
    /// The edit's number, as sent with it.
    pub seq: u64,
}

impl PendingCue {
    /// The edit's result once the deck reports it applied it (`hot_seq` at or past the edit's):
    /// the slot as it shows then (`Some(None)` for empty); `None` before.
    pub(crate) fn landed(&self, hot: &[Option<f64>; HOT_CUES], hot_seq: u64) -> Option<Option<f64>> {
        (hot_seq >= self.seq).then(|| hot.get(self.slot).copied().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> TrackId {
        TrackId { path: "/m/a.mp3".into(), stamp: (100, 5), fp: 0xabc, seconds: 300.0, envelope: vec![-20, -12, -60] }
    }

    #[test]
    fn an_entry_from_before_fingerprints_matches_by_its_stamp_or_its_length() {
        assert!(same_audio(&json!({"size": 100, "mtime": 5}), &id()));
        assert!(same_audio(&json!({"size": 99, "mtime": 4, "seconds": 300.04}), &id()));
        assert!(!same_audio(&json!({"size": 99, "mtime": 4, "seconds": 300.1}), &id()));
        assert!(!same_audio(&json!({"size": 99, "mtime": 4}), &id()));
        // With a fingerprint, only the fingerprint counts.
        assert!(same_audio(&json!({"size": 1, "mtime": 1, "fp": "0000000000000abc"}), &id()));
        assert!(!same_audio(&json!({"size": 100, "mtime": 5, "fp": "0000000000000abd"}), &id()));
        // Another hash, the same length and loudness (within a step): another decoder.
        assert!(same_audio(&json!({"fp": "0000000000000abd", "seconds": 300.02, "env": [-21, -12, -60]}), &id()));
        assert!(!same_audio(&json!({"fp": "0000000000000abd", "seconds": 300.02, "env": [-23, -12, -60]}), &id()));
        assert!(!same_audio(&json!({"fp": "0000000000000abd", "seconds": 300.5, "env": [-20, -12, -60]}), &id()));
    }

    #[test]
    fn an_edit_lands_when_the_deck_says_it_applied_it() {
        let mut hot = [None; HOT_CUES];
        hot[0] = Some(5.0);
        let p = PendingCue { deck: 0, load: 1, slot: 0, seq: 7 };
        assert_eq!(p.landed(&hot, 6), None, "not applied yet, whatever the slot shows");
        assert_eq!(p.landed(&hot, 7), Some(Some(5.0)), "applied: set where it already was");
        assert_eq!(p.landed(&[None; HOT_CUES], 9), Some(None));
    }
}
