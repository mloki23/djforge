//! Media availability checks without decoding files or changing the session.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{ClipContent, STEMS};
use std::collections::BTreeMap;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "session.audio_health", "Session Audio Health", [], None, "{}; checks all audio clips, including inactive playlists; no filesystem or signal analysis", always, report),
    ]
}

/// The advice for any problem with a stem file.
const AGAIN: &str = "Remove the stems (Remove Stems) and split the clip again.";

fn report(e: &mut Engine, _: &Value) -> Result<Value> {
    let s = e.session();
    let sources: BTreeMap<_, _> = s.sources.iter().map(|source| (source.id, source)).collect();
    let mut issues = Vec::new();
    if s.sample_rate.hz() == 0 {
        issues.push(json!({"code": "invalid_sample_rate", "track": "Session", "message": "Session sample rate is zero. Set a valid rate in Session Setup before playback.", "active": true}));
    }
    let mut clips = 0usize;
    let mut active_end = 0i64;
    for track in &s.tracks {
        for (index, playlist) in track.playlists.iter().enumerate() {
            let active = index == track.active_playlist;
            for clip in &playlist.clips {
                let ClipContent::Audio { source: own, offset } = clip.content else { continue };
                clips += 1;
                if active {
                    active_end = active_end.max(clip.end());
                }
                // A split clip plays its stems, each read like its source, so each is checked too.
                let stems = clip.stems.iter().flat_map(|set| STEMS.iter().zip(set.sources).map(|(name, id)| (Some(*name), id)));
                for (stem, source) in std::iter::once((None, own)).chain(stems) {
                    let metadata = sources.get(&source);
                    let audio = s.pool.get(source);
                    // Stem files come from the stem cache: the fix is to split again, not reimport.
                    let mut add = |code: &str, own: &str, of_stem: &str| {
                        let message = if stem.is_some() { format!("{of_stem} {AGAIN}") } else { own.to_string() };
                        issues.push(json!({"code": code, "message": message, "track": track.name, "track_id": track.id.0,
                            "playlist": playlist.name, "active": active, "clip": clip.name, "clip_id": clip.id.0,
                            "source_id": source.0, "stem": stem, "path": metadata.map(|m| m.path.as_str())}));
                    };
                    if metadata.is_none() {
                        add(
                            "missing_source",
                            "Source metadata is missing. Reimport the audio and replace this clip.",
                            "Stem file metadata is missing.",
                        );
                    }
                    let Some(audio) = audio else {
                        add(
                            "unavailable_audio",
                            "Audio is not loaded; this clip will be silent. Restore the media and reopen the session, or reimport it.",
                            "Stem audio is not loaded; this stem will be silent.",
                        );
                        continue;
                    };
                    if audio.buffer.sample_rate != s.sample_rate.hz() {
                        add(
                            "sample_rate_mismatch",
                            "Loaded audio differs from the session sample rate. Reimport it to convert to the session rate.",
                            "Stem audio differs from the session sample rate.",
                        );
                    }
                    if audio.buffer.channels.is_empty() || audio.frames() == 0 {
                        add("empty_audio", "Loaded audio has no playable frames. Reimport a valid audio file.", "Stem audio has no playable frames.");
                        continue;
                    }
                    if audio.buffer.channels.iter().any(|channel| channel.len() != audio.frames()) {
                        add(
                            "uneven_channels",
                            "Loaded audio channels have different lengths. Reimport a valid audio file; bounds below use the shortest channel.",
                            "Stem audio channels have different lengths; bounds below use the shortest channel.",
                        );
                    }
                    // The clip itself is reported once, with its own source.
                    if stem.is_none() && (!clip.stretch.is_finite() || clip.stretch <= 0.0 || clip.length <= 0) {
                        add("invalid_clip", "Clip length or stretch is invalid. Replace this clip.", "");
                    }
                    let stretch = if clip.stretch.is_finite() && clip.stretch > 0.0 { clip.stretch } else { 1.0 };
                    // Match the mixer's last requested source sample, including elastic playback.
                    let last = offset as f64 + clip.length.saturating_sub(1).max(0) as f64 / stretch;
                    if offset < 0 || last >= audio.frames() as f64 {
                        add(
                            "source_bounds",
                            "Clip extends outside the loaded audio; part will be silent. Trim the clip or replace its source.",
                            "Clip extends outside the stem's loaded audio; part of this stem will be silent.",
                        );
                    }
                }
            }
        }
    }
    Ok(json!({"audio_clips": clips, "active_audio_end_samples": active_end,
        "active_audio_end_seconds": (s.sample_rate.hz() != 0).then(|| active_end as f64 / f64::from(s.sample_rate.hz())),
        "issue_count": issues.len(), "healthy": issues.is_empty(), "issues": issues,
        "scope": "All playlists, the stems of split clips included; loaded audio availability and clip bounds only. Does not check files on disk, signal levels, plugins or routing."}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;
    use soundcraft_model::{Clip, ClipId, SourceAudio, SourceId};
    use std::sync::Arc;

    fn check(e: &mut Engine) -> Value {
        e.execute("session.audio_health", &json!({})).unwrap()
    }

    #[test]
    fn empty_and_demo_are_healthy_and_query_is_read_only() {
        for mut e in [Engine::default(), crate::demo::demo_engine()] {
            let before = e.session_arc();
            let revision = e.revision;
            let dirty = e.is_dirty();
            assert_eq!(check(&mut e)["healthy"], true);
            assert_eq!(*before, *e.session());
            assert_eq!(e.revision, revision);
            assert_eq!(e.is_dirty(), dirty);
            assert!(e.journal.is_empty());
            assert!(!e.can_undo());
        }
    }

    #[test]
    fn reports_missing_media_in_inactive_takes() {
        let mut e = crate::demo::demo_engine();
        let t = e.session_mut().tracks.first_mut().unwrap();
        let mut p = soundcraft_model::Playlist::new("Alternate take");
        p.clips.push(Clip::audio(ClipId(999), "Lost take", SourceId(999), 0, 0, 10));
        t.playlists.push(p);
        let r = check(&mut e);
        assert_eq!(r["issue_count"], 2);
        assert_eq!(r["issues"][0]["code"], "missing_source");
        assert_eq!(r["issues"][1]["code"], "unavailable_audio");
        assert_eq!(r["issues"][0]["active"], false);
    }

    #[test]
    fn a_split_clip_reports_a_stem_that_is_missing_or_too_short() {
        let mut e = crate::demo::demo_engine();
        let s = e.session();
        let clip = s.all_clips().find(|c| c.is_audio()).unwrap().clone();
        let source = s.source(clip.source().unwrap()).unwrap().clone();
        let audio = s.pool.get(source.id).unwrap().clone();
        let s = e.session_mut();
        let ids: Vec<SourceId> = (0..4).map(|_| SourceId(s.alloc())).collect();
        for (i, id) in ids.iter().enumerate().take(3) {
            s.sources.push(soundcraft_model::Source { id: *id, name: format!("{} ({})", source.name, STEMS[i]), ..source.clone() });
        }
        // Vocals and drums play; bass is one frame short; `other` has no file at all.
        s.pool.insert(ids[0], audio.clone());
        s.pool.insert(ids[1], audio.clone());
        let short = AudioBuffer {
            sample_rate: audio.buffer.sample_rate,
            channels: audio.buffer.channels.iter().map(|c| c[..c.len() - 1].to_vec()).collect(),
        };
        s.pool.insert(ids[2], Arc::new(SourceAudio::new(short)));
        let set = soundcraft_model::StemSet { key: "k".into(), sources: [ids[0], ids[1], ids[2], ids[3]] };
        let id = clip.id;
        s.find_clip_mut(id).unwrap().stems = Some(set);
        // The clip reaches its source's last frame, so a stem one frame short is out of bounds.
        let frames = i64::try_from(audio.frames()).unwrap();
        let c = s.find_clip_mut(id).unwrap();
        c.content = ClipContent::Audio { source: source.id, offset: 0 };
        c.length = frames;
        c.stretch = 1.0;
        let r = check(&mut e);
        let mine: Vec<(String, Value)> = r["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["clip_id"] == id.0)
            .map(|i| (i["code"].as_str().unwrap().to_string(), i["stem"].clone()))
            .collect();
        assert_eq!(
            mine,
            [
                ("source_bounds".to_string(), json!("bass")),
                ("missing_source".to_string(), json!("other")),
                ("unavailable_audio".to_string(), json!("other"))
            ],
            "{r}"
        );
        for issue in r["issues"].as_array().unwrap().iter().filter(|i| i["clip_id"] == id.0) {
            let message = issue["message"].as_str().unwrap();
            assert!(message.ends_with("split the clip again.") && !message.contains("eimport"), "{issue}");
        }
        assert_eq!(r["audio_clips"], e.session().all_clips().filter(|c| c.is_audio()).count());
    }

    #[test]
    fn checks_loaded_bounds_and_stretch_not_stale_metadata() {
        let mut e = crate::demo::demo_engine();
        let id = e.session().sources.first().unwrap().id;
        let sr = e.session().sample_rate.hz();
        e.session_mut().pool.insert(id, Arc::new(SourceAudio::new(AudioBuffer::new(sr, 1, 10))));
        let t = e.session_mut().tracks.first_mut().unwrap();
        t.playlists.clear();
        let mut p = soundcraft_model::Playlist::new("Bounds");
        let mut c = Clip::audio(ClipId(999), "Stretch", id, 0, 0, 20);
        c.stretch = 2.0;
        p.clips.push(c);
        t.playlists.push(p);
        t.active_playlist = 0;
        assert_eq!(check(&mut e)["healthy"], true);
        e.session_mut().tracks.first_mut().unwrap().playlists[0].clips[0].length = 21;
        assert_eq!(check(&mut e)["issues"][0]["code"], "source_bounds");
        let clip = &mut e.session_mut().tracks.first_mut().unwrap().playlists[0].clips[0];
        clip.length = i64::MAX;
        clip.stretch = f64::NAN;
        clip.content = ClipContent::Audio { source: id, offset: i64::MAX };
        let r = check(&mut e);
        assert_eq!(r["issues"][0]["code"], "invalid_clip");
        assert_eq!(r["issues"][1]["code"], "source_bounds");
    }

    #[test]
    fn malformed_and_empty_audio_are_reported_without_panicking() {
        let mut e = crate::demo::demo_engine();
        let id = e.session().sources.first().unwrap().id;
        e.session_mut().pool.insert(id, Arc::new(SourceAudio::new(AudioBuffer::new(1, 0, 0))));
        let r = check(&mut e);
        assert!(r["issues"].as_array().unwrap().iter().any(|i| i["code"] == "empty_audio"));
        assert!(r["issues"].as_array().unwrap().iter().any(|i| i["code"] == "sample_rate_mismatch"));
    }

    #[test]
    fn zero_rate_has_unknown_duration_and_uneven_channels_use_shortest() {
        let mut e = crate::demo::demo_engine();
        let id = e.session().sources.first().unwrap().id;
        let sr = e.session().sample_rate.hz();
        let audio = AudioBuffer { sample_rate: sr, channels: vec![vec![0.0; 10], vec![0.0; 5]] };
        e.session_mut().pool.insert(id, Arc::new(SourceAudio::new(audio)));
        let r = check(&mut e);
        assert!(r["issues"].as_array().unwrap().iter().any(|i| i["code"] == "uneven_channels"));
        assert!(r["issues"].as_array().unwrap().iter().any(|i| i["code"] == "source_bounds"));
        e.session_mut().sample_rate = serde_json::from_value(json!(0)).unwrap();
        let r = check(&mut e);
        assert!(r["active_audio_end_seconds"].is_null());
        assert_eq!(r["issues"][0]["code"], "invalid_sample_rate");
    }
}
