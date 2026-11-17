//! Audio sources (files in the session's Audio Files folder) and their decoded data, and the
//! movies video clips reference.

use crate::SourceId;
use soundcraft_audio_io::{AudioBuffer, FileFormat, bands::Bands, peaks::Peaks};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Metadata for an audio file used by the session (serialised).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub name: String,
    /// Path relative to the session folder (or absolute for referenced media).
    pub path: String,
    pub channels: u16,
    pub frames: u64,
    pub sample_rate: u32,
    pub format: FileFormat,
    /// Original BWF time stamp (samples), if any.
    #[serde(default)]
    pub time_reference: u64,
    /// True when the file has not been written to disk yet (recorded/rendered this session).
    #[serde(default)]
    pub unsaved: bool,
    /// Size in bytes of the file when the session was last saved (0 = unknown).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_len: u64,
    /// The file's modification time then, in milliseconds since 1970 (0 = unknown).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_modified_ms: u64,
    /// The absolute path of the user file it was imported from, kept when the session saves a
    /// copy of it (`None`: made in the session, a stem, or from a session older than this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Size in bytes of `origin` when it was imported or relinked (0 = unknown).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub origin_len: u64,
    /// `origin`'s modification time then, in milliseconds since 1970 (0 = unknown).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub origin_modified_ms: u64,
    /// The track's key as a Camelot code (`8A`), from the set plan, mixforge's analysis or the
    /// file's `TKEY` tag (`None`: not known, or from a session older than this).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl Source {
    /// Records the key written `raw` (Camelot, Open Key or a key name) as its Camelot code; a
    /// blank or unrecognised one leaves the key as it was. True when it was recognised.
    pub fn set_key(&mut self, raw: &str) -> bool {
        match camelot_of(raw) {
            Some(k) => {
                self.key = Some(k);
                true
            }
            None => false,
        }
    }
}

/// The Camelot code (`8A`) of the key written `raw` (Camelot, Open Key or a key name such as
/// `Am`), or `None` when it is not one.
pub fn camelot_of(raw: &str) -> Option<String> {
    soundcraft_midi::Key::parse(raw).ok().map(|k| k.camelot())
}

fn is_zero(x: &u64) -> bool {
    *x == 0
}

/// A movie used by Video track clips (serialised). The pictures themselves are never stored in
/// the session: the UI opens the movie from `path` and decodes frames on demand.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoSource {
    pub id: SourceId,
    pub name: String,
    /// Absolute path of the movie (referenced, never copied into the session folder).
    pub path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Frames per second (0 = unknown).
    #[serde(default)]
    pub frame_rate: f64,
    /// Picture length in seconds.
    #[serde(default)]
    pub duration: f64,
    /// Codec id (`h264`, `prores`, `mjpeg`, `hevc`, …).
    #[serde(default)]
    pub codec: String,
    /// Why the movie is not opened, when its stored path was refused on opening the session.
    #[serde(skip)]
    pub offline: Option<String>,
}

impl VideoSource {
    /// Display aspect ratio (width / height), 16:9 when unknown.
    pub fn aspect(&self) -> f64 {
        if self.width == 0 || self.height == 0 { 16.0 / 9.0 } else { f64::from(self.width) / f64::from(self.height) }
    }
}

/// Decoded audio plus waveform overviews (not serialised). Each channel's frequency bands, for
/// waveforms coloured by frequency, are filled in later by whoever claims the job
/// ([`SourceAudio::claim_bands`], [`SourceAudio::build_bands_while`]), so loading never waits
/// for them.
#[derive(Debug)]
pub struct SourceAudio {
    pub buffer: AudioBuffer,
    pub peaks: Vec<Peaks>,
    bands: Vec<OnceLock<Bands>>,
    bands_claimed: AtomicBool,
}

impl SourceAudio {
    /// The audio and its min/max overviews; no bands yet.
    pub fn new(buffer: AudioBuffer) -> Self {
        let peaks = buffer.channels.iter().map(|c| Peaks::build(c)).collect();
        let bands = buffer.channels.iter().map(|_| OnceLock::new()).collect();
        SourceAudio { buffer, peaks, bands, bands_claimed: AtomicBool::new(false) }
    }

    /// Channel `ch`'s frequency bands, once built.
    pub fn bands(&self, ch: usize) -> Option<&Bands> {
        self.bands.get(ch).and_then(OnceLock::get)
    }

    /// Whether every channel's bands are built.
    pub fn bands_ready(&self) -> bool {
        self.bands.iter().all(|b| b.get().is_some())
    }

    /// Claim the job of building the bands: true for the first caller only, so a source is
    /// queued for building once.
    pub fn claim_bands(&self) -> bool {
        !self.bands_ready() && !self.bands_claimed.swap(true, Ordering::AcqRel)
    }

    /// Whether the bands still need building and nobody has claimed the job.
    pub fn bands_pending(&self) -> bool {
        !self.bands_ready() && !self.bands_claimed.load(Ordering::Acquire)
    }

    /// Give the job back unfinished (the build was stopped, or never reached a worker), so the
    /// source is claimed and queued again later.
    pub fn unclaim_bands(&self) {
        self.bands_claimed.store(false, Ordering::Release);
    }

    /// Build the channels' missing bands, giving up when `wanted` says they are no longer needed
    /// (it is asked now and then). Returns whether every channel's bands are built.
    pub fn build_bands_while(&self, wanted: impl Fn() -> bool) -> bool {
        for (slot, samples) in self.bands.iter().zip(&self.buffer.channels) {
            if slot.get().is_some() {
                continue;
            }
            let Some(bands) = Bands::build_while(samples, self.buffer.sample_rate, &wanted) else { return false };
            let _ = slot.set(bands);
        }
        self.bands_ready()
    }
    pub fn frames(&self) -> usize {
        self.buffer.frames()
    }
    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.buffer.channels.get(ch).map(Vec::as_slice)
    }
}

/// Shared decoded audio, keyed by source id.
#[derive(Debug, Clone, Default)]
pub struct SourcePool {
    map: BTreeMap<SourceId, Arc<SourceAudio>>,
}

impl SourcePool {
    pub fn insert(&mut self, id: SourceId, audio: Arc<SourceAudio>) {
        self.map.insert(id, audio);
    }
    pub fn get(&self, id: SourceId) -> Option<&Arc<SourceAudio>> {
        self.map.get(&id)
    }
    pub fn remove(&mut self, id: SourceId) -> Option<Arc<SourceAudio>> {
        self.map.remove(&id)
    }
    pub fn contains(&self, id: SourceId) -> bool {
        self.map.contains_key(&id)
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn ids(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.map.keys().copied()
    }
    /// The decoded audio of every source.
    pub fn audio(&self) -> impl Iterator<Item = &Arc<SourceAudio>> + '_ {
        self.map.values()
    }
}

impl PartialEq for SourcePool {
    fn eq(&self, other: &Self) -> bool {
        self.map.len() == other.map.len() && self.map.iter().zip(other.map.iter()).all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tone(channels: usize, n: usize) -> SourceAudio {
        let c: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![c; channels] })
    }

    #[test]
    fn bands_come_later_and_are_built_once() {
        let a = tone(2, 4800);
        assert!(a.bands(0).is_none() && !a.bands_ready(), "new audio has no bands yet");
        assert!(a.claim_bands());
        assert!(!a.claim_bands(), "claimed once");
        assert!(!a.build_bands_while(|| false), "given up");
        assert!(a.bands(0).is_none());
        assert!(a.build_bands_while(|| true));
        assert!(a.bands(0).is_some_and(|b| !b.is_empty()) && a.bands(1).is_some() && a.bands(2).is_none());
        assert!(!a.claim_bands() && !a.bands_pending(), "nothing left to build");
        let b = tone(1, 4800);
        assert!(b.bands_pending() && b.claim_bands() && !b.bands_pending());
        b.unclaim_bands();
        assert!(b.bands_pending() && b.claim_bands(), "claimed again once given back");
        // No channels: ready at once.
        let none = SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![] });
        assert!(none.bands_ready() && !none.claim_bands());
    }

    fn old_source() -> serde_json::Value {
        serde_json::json!({"id": 3, "name": "a", "path": "a.wav", "channels": 2, "frames": 10, "sample_rate": 48000, "format": "Wav"})
    }

    #[test]
    fn a_source_from_before_keys_loads_without_one_and_saves_without_one() {
        let s: Source = serde_json::from_value(old_source()).unwrap();
        assert_eq!(s.key, None);
        assert!(serde_json::to_value(&s).unwrap().get("key").is_none());
    }

    #[test]
    fn keys_are_kept_as_camelot_and_unrecognised_ones_ignored() {
        let mut s: Source = serde_json::from_value(old_source()).unwrap();
        assert!(s.set_key("Am"));
        assert_eq!(s.key.as_deref(), Some("8A"));
        for bad in ["", "  ", "H", "13A", "Am7"] {
            assert!(!s.set_key(bad), "{bad}");
        }
        assert_eq!(s.key.as_deref(), Some("8A"), "an unrecognised key leaves the known one");
        assert!(s.set_key(" 04b "));
        assert_eq!(s.key.as_deref(), Some("4B"));
        assert_eq!(camelot_of("1m").as_deref(), Some("8A"));
        let back: Source = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back.key.as_deref(), Some("4B"));
    }
}
