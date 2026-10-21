//! Clip List commands.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{Session, SourceId};
use std::collections::BTreeSet;

const CLEAR: &str = "clip.clear";

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "clip.clear",
        "Clear Clips",
        [],
        Some("Cmd+Shift+B"),
        "{clips?: [id], sources?: [id], with_clips?: bool} — removes clips and audio files from the session (files stay on disk); default: the Clip List selection. A file still used on the timeline needs `with_clips: true`, which removes the clips that play it (and their stem files, when no other clip uses them); split clips that use it only as a stem stay and lose their stems. Returns {clips, sources, unsplit}",
        has_list_selection,
        clear
    )]
}

fn has_list_selection(e: &Engine) -> std::result::Result<(), String> {
    let ed = &e.session().edit;
    if ed.selected_clips.is_empty() && ed.selected_sources.is_empty() { Err("select clips or files in the Clip List".into()) } else { Ok(()) }
}

fn ids<'a, T: serde::Deserialize<'a>>(p: &'a Value, key: &str) -> Result<Option<T>> {
    p.get(key).map(|v| T::deserialize(v).map_err(|_| bad(CLEAR, format!("`{key}` must be an array of ids")))).transpose()
}

/// What clearing the files `sources` together with the clips `clips` does on the timeline.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClearEffect {
    /// Clips that go: those listed, and those playing a cleared file as their own audio.
    pub removed: BTreeSet<ClipId>,
    /// Split clips that only use a cleared file as a stem: they stay and lose their stems.
    pub unsplit: BTreeSet<ClipId>,
    /// Stem files of removed clips whose own file is cleared, once no other clip uses them: they
    /// go too.
    pub stem_files: BTreeSet<SourceId>,
}

impl ClearEffect {
    /// Clips besides `clips` that a cleared file takes off the timeline, and split clips it unsplits,
    /// counted for file `f` only.
    pub fn of_file(&self, s: &Session, clips: &BTreeSet<ClipId>, f: SourceId) -> (usize, usize) {
        let removed = s.all_clips().filter(|c| c.source() == Some(f) && !clips.contains(&c.id)).count();
        let unsplit = s.all_clips().filter(|c| self.unsplit.contains(&c.id) && c.audio_sources().skip(1).any(|x| x == f)).count();
        (removed, unsplit)
    }
}

/// What `clip.clear` with `clips` and `sources` does (see [`ClearEffect`]).
pub fn clear_effect(s: &Session, clips: &BTreeSet<ClipId>, sources: &BTreeSet<SourceId>) -> ClearEffect {
    let own_cleared = |c: &soundcraft_model::Clip| c.source().is_some_and(|f| sources.contains(&f));
    let removed: BTreeSet<ClipId> = s.all_clips().filter(|c| clips.contains(&c.id) || own_cleared(c)).map(|c| c.id).collect();
    let unsplit: BTreeSet<ClipId> =
        s.all_clips().filter(|c| !removed.contains(&c.id) && c.audio_sources().skip(1).any(|f| sources.contains(&f))).map(|c| c.id).collect();
    // What the clips left on the timeline still play: unsplit clips keep only their own file.
    let used: BTreeSet<SourceId> = s
        .all_clips()
        .filter(|c| !removed.contains(&c.id))
        .flat_map(|c| c.audio_sources().take(if unsplit.contains(&c.id) { 1 } else { usize::MAX }))
        .collect();
    let stem_files = s
        .all_clips()
        .filter(|c| removed.contains(&c.id) && own_cleared(c))
        .flat_map(|c| c.audio_sources().skip(1))
        .filter(|f| !sources.contains(f) && !used.contains(f) && s.source(*f).is_some())
        .collect();
    ClearEffect { removed, unsplit, stem_files }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

fn clear(e: &mut Engine, p: &Value) -> Result<Value> {
    let s = e.session();
    let (clips, sources): (BTreeSet<ClipId>, BTreeSet<SourceId>) = match (ids(p, "clips")?, ids(p, "sources")?) {
        (None, None) => (s.edit.selected_clips.iter().copied().collect(), s.edit.selected_sources.iter().copied().collect()),
        (c, f) => (c.unwrap_or_default(), f.unwrap_or_default()),
    };
    let known: BTreeSet<ClipId> = s.all_clips().map(|c| c.id).collect();
    if let Some(c) = clips.iter().find(|c| !known.contains(c)) {
        return Err(bad(CLEAR, format!("no clip with id {c}")));
    }
    let files = sources.iter().map(|f| s.source(*f).ok_or_else(|| bad(CLEAR, format!("no audio file with id {f}")))).collect::<Result<Vec<_>>>()?;
    if clips.is_empty() && files.is_empty() {
        return Err(bad(CLEAR, "nothing to clear: pass `clips` or `sources`, or select them in the Clip List"));
    }
    let effect = clear_effect(s, &clips, &sources);
    if !bool_or(p, "with_clips", false)
        && let Some((f, (removed, unsplit))) = files.iter().map(|f| (f, effect.of_file(s, &clips, f.id))).find(|(_, (r, u))| r + u > 0)
    {
        let mut what = Vec::new();
        if removed > 0 {
            what.push(format!("{} will be removed", plural(removed, "clip")));
        }
        if unsplit > 0 {
            what.push(format!("{} will lose {}", plural(unsplit, "split clip"), if unsplit == 1 { "its stems" } else { "their stems" }));
        }
        return Err(bad(
            CLEAR,
            format!(
                "`{}` is used by {} on the timeline: {}; pass `with_clips: true` to go ahead",
                f.name,
                plural(removed + unsplit, "clip"),
                what.join(" and ")
            ),
        ));
    }
    let all: BTreeSet<SourceId> = sources.iter().chain(&effect.stem_files).copied().collect();
    let s = e.session_mut();
    for pl in s.tracks.iter_mut().flat_map(|t| t.playlists.iter_mut()) {
        pl.clips.retain(|c| !effect.removed.contains(&c.id));
        pl.clips.iter_mut().filter(|c| effect.unsplit.contains(&c.id)).for_each(soundcraft_model::Clip::unsplit);
    }
    s.sources.retain(|f| !all.contains(&f.id));
    for f in &all {
        s.pool.remove(*f);
    }
    s.edit.selected_clips.retain(|c| !effect.removed.contains(c));
    s.edit.selected_sources.retain(|f| !all.contains(f));
    Ok(json!({"clips": effect.removed.len(), "sources": all.len(), "unsplit": effect.unsplit.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};

    fn kick(e: &Engine) -> (SourceId, Vec<ClipId>) {
        let s = e.session();
        let f = s.sources.iter().find(|x| x.name == "Kick").map(|x| x.id).unwrap();
        (f, s.clips_using(f).map(|c| c.id).collect())
    }

    #[test]
    fn clearing_a_clip_takes_it_off_the_timeline_and_keeps_its_file() {
        let mut e = crate::demo::demo_engine();
        let (f, clips) = kick(&e);
        let r = e.execute("clip.clear", &json!({"clips": [clips[0]]})).unwrap();
        assert_eq!(r, json!({"clips": 1, "sources": 0, "unsplit": 0}));
        assert!(e.session().find_clip(clips[0]).is_none());
        assert!(e.session().source(f).is_some());
        assert!(e.undo());
        assert!(e.session().find_clip(clips[0]).is_some());
    }

    #[test]
    fn a_file_in_use_stays_unless_its_clips_go_too() {
        let mut e = crate::demo::demo_engine();
        e.execute("track.playlist_duplicate", &json!({"track": "Kick"})).unwrap();
        let (f, clips) = kick(&e);
        assert_eq!(clips.len(), 6);
        let err = e.execute("clip.clear", &json!({"sources": [f]})).unwrap_err();
        assert!(err.to_string().contains("used by 6 clips"), "{err}");
        let r = e.execute("clip.clear", &json!({"sources": [f], "with_clips": true})).unwrap();
        assert_eq!(r, json!({"clips": 6, "sources": 1, "unsplit": 0}));
        let s = e.session();
        assert!(s.source(f).is_none() && !s.pool.contains(f));
        assert_eq!(s.clips_using(f).count(), 0);
        assert_eq!(e.undo_label(), Some("Clear Clips"));
        assert!(e.undo());
        assert!(e.session().pool.contains(f));
        assert_eq!(e.session().clips_using(f).count(), 6);
    }

    /// The demo with its first Kick clip split into four new stem files.
    fn split_kick(e: &mut Engine) -> (SourceId, ClipId, [SourceId; 4]) {
        let (f, clips) = kick(e);
        let s = e.session_mut();
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 4_800]] };
        let stems = soundcraft_model::STEMS.map(|n| crate::io::add_source(s, n, buf.clone(), None, FileFormat::Wav));
        let c = s.find_clip_mut(clips[0]).unwrap();
        c.stems = Some(soundcraft_model::StemSet { key: "k".into(), sources: stems });
        c.stem_mask = [true, false, false, false];
        (f, clips[0], stems)
    }

    #[test]
    fn clearing_a_stem_file_unsplits_its_clip_and_keeps_it() {
        let mut e = crate::demo::demo_engine();
        let (f, clip, stems) = split_kick(&mut e);
        let err = e.execute("clip.clear", &json!({"sources": [stems[2]]})).unwrap_err().to_string();
        assert!(err.contains("1 split clip will lose its stems") && !err.contains("removed"), "{err}");
        let r = e.execute("clip.clear", &json!({"sources": [stems[2]], "with_clips": true})).unwrap();
        assert_eq!(r, json!({"clips": 0, "sources": 1, "unsplit": 1}));
        let s = e.session();
        let c = s.find_clip(clip).unwrap().1;
        assert_eq!((&c.stems, c.stem_mask, c.source()), (&None, [true; 4], Some(f)));
        assert!(s.source(stems[2]).is_none() && !s.pool.contains(stems[2]));
        // The other stem files stay in the Clip List, like any file after its clip goes.
        assert!(s.source(stems[0]).is_some());
        assert!(e.undo());
        assert!(e.session().find_clip(clip).unwrap().1.stems.is_some());
    }

    #[test]
    fn clearing_a_split_clips_own_file_takes_its_unshared_stem_files_too() {
        let mut e = crate::demo::demo_engine();
        let (f, _, stems) = split_kick(&mut e);
        // Another clip (not using the kick) shares the drums stem.
        let other = e.session().all_clips().find(|c| c.is_audio() && c.source() != Some(f)).unwrap().id;
        let shared = soundcraft_model::StemSet { key: "k2".into(), sources: [stems[1]; 4] };
        e.session_mut().find_clip_mut(other).unwrap().stems = Some(shared);
        let err = e.execute("clip.clear", &json!({"sources": [f]})).unwrap_err().to_string();
        assert!(err.contains("3 clips will be removed"), "{err}");
        let r = e.execute("clip.clear", &json!({"sources": [f], "with_clips": true})).unwrap();
        assert_eq!(r, json!({"clips": 3, "sources": 4, "unsplit": 0}));
        let s = e.session();
        assert_eq!([0, 1, 2, 3].map(|i| s.source(stems[i]).is_some()), [false, true, false, false]);
        assert!(s.find_clip(other).unwrap().1.stems.is_some());
    }

    #[test]
    fn listing_a_files_clips_too_is_enough() {
        let mut e = crate::demo::demo_engine();
        let (f, clips) = kick(&e);
        let r = e.execute("clip.clear", &json!({"clips": clips, "sources": [f]})).unwrap();
        assert_eq!(r, json!({"clips": 3, "sources": 1, "unsplit": 0}));
    }

    #[test]
    fn the_clip_list_selection_is_the_default_target() {
        let mut e = crate::demo::demo_engine();
        let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 4_800]] };
        let f = crate::io::add_source(e.session_mut(), "Spare", buf, None, FileFormat::Wav);
        let (_, clips) = kick(&e);
        e.execute("edit.select", &json!({"clips": [clips[0]]})).unwrap();
        let r = e.execute("edit.select", &json!({"sources": [f, f, 999_999]})).unwrap();
        assert_eq!(r["sources"], json!([f]));
        assert_eq!(r["clips"], json!([]));
        let r = e.execute("edit.select", &json!({"clips": [clips[0]]})).unwrap();
        assert_eq!(r["sources"], json!([]));
        e.execute("clip.clear", &json!({})).unwrap();
        assert!(e.session().find_clip(clips[0]).is_none());
        assert!(e.session().edit.selected_clips.is_empty());
        e.execute("edit.select", &json!({"sources": [f]})).unwrap();
        e.execute("clip.clear", &json!({})).unwrap();
        assert!(e.session().source(f).is_none());
        assert!(e.session().edit.selected_sources.is_empty());
    }

    #[test]
    fn bad_targets_are_errors() {
        let mut e = crate::demo::demo_engine();
        for p in [
            json!({}),
            json!({"clips": []}),
            json!({"clips": [999_999]}),
            json!({"sources": [999_999]}),
            json!({"sources": "Kick"}),
            json!({"clips": [-1]}),
        ] {
            assert!(e.execute("clip.clear", &p).is_err(), "{p}");
        }
    }

    #[test]
    fn deselect_all_clears_the_clip_list_selection() {
        let mut e = crate::demo::demo_engine();
        let (f, _) = kick(&e);
        e.execute("edit.select", &json!({"sources": [f]})).unwrap();
        e.execute("edit.select_none", &json!({})).unwrap();
        assert!(e.session().edit.selected_sources.is_empty());
    }
}
