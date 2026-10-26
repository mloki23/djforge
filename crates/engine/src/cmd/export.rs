//! Exporting a DJ set for playing out: the set's tracks in order, each file's bar grid and its
//! cues in the file's own time ([`collect`]).

// Used by `dj.export_set`, which comes with the job that runs the export.
#![cfg_attr(not(test), allow(dead_code))]

use super::dj::recipe::{bar_at_or_after, bar_sample};
use soundcraft_model::{Clip, ClipId, Session, SourceId, StemSet, Transition, WarpMap};
use soundcraft_time::Samples;
use std::path::PathBuf;

/// One track of an exported set: a source file with its bar grid and cues, in seconds of the
/// decoded file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExportTrack {
    pub source: SourceId,
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    /// File seconds of each bar's downbeat, strictly increasing, over the whole file.
    pub grid: Vec<f64>,
    pub cues: Vec<Cue>,
    pub split: Option<StemSet>,
    /// The earliest of the set's clips playing this file.
    pub first_clip: ClipId,
}

/// A cue of an exported track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Cue {
    /// "Mix in", "Bass hit", "Mix out" or "Bass gone".
    pub name: &'static str,
    /// Hot cue A..D as 0..3.
    pub slot: u8,
    /// File seconds.
    pub at: f64,
    /// A memory cue (else a hot cue).
    pub memory: bool,
}

/// Most bars a file's grid holds (a 30-minute track at 60 bpm has 450).
const MAX_GRID_BARS: usize = 100_000;

/// The set in session `s`, from its DJ transitions: the files their clips play, one track per
/// file in the order its first clip starts, each with its bar grid and cues in file seconds.
///
/// A beat-locked clip plays its source-pool audio at [`WarpMap::source_at_time`] (its source
/// offset doesn't enter), and the pool holds the whole decoded file from its first sample at the
/// session rate, so a pool position over the session rate is a time in the file. The grid is the
/// file time heard on each session bar line inside the file's clips (nudges included), extended
/// at the edge bar lengths over the whole file. Cues: A mix-in on the entry bar and B the bass
/// hit of the transition into the track, C mix-out on the entry bar and D the bass gone of the
/// transition out of it, each through that transition's clip; memory cues at A and C. Cues
/// outside the file are left out.
pub(crate) fn collect(s: &Session) -> Result<Vec<ExportTrack>, String> {
    let mut transitions: Vec<&Transition> = s.dj.transitions.iter().collect();
    if transitions.is_empty() {
        return Err("the session has no DJ transitions: import a set first (DJ > Import mixforge Set)".into());
    }
    transitions.sort_by_key(|t| (t.entry_bar, t.id));
    let sr = s.sample_rate.as_f64();

    // The files, each with its clips in timeline order.
    struct File<'a> {
        source: SourceId,
        path: &'a str,
        frames: u64,
        clips: Vec<(&'a Clip, &'a WarpMap)>,
    }
    let mut files: Vec<File> = Vec::new();
    for id in transitions.iter().flat_map(|t| [t.out_clip, t.in_clip]) {
        let c = s.find_clip(id).map(|(_, c)| c).ok_or_else(|| format!("clip {} of a transition is not on its track's active playlist", id.0))?;
        let w = c.warp.as_ref().filter(|w| w.is_usable() && c.is_audio()).ok_or_else(|| format!("clip {} is not beat-locked", id.0))?;
        let src = c.source().and_then(|x| s.source(x)).ok_or_else(|| format!("clip {} has no source file", id.0))?;
        match files.iter_mut().find(|f| f.path == src.path) {
            Some(f) if f.clips.iter().any(|(k, _)| k.id == id) => {}
            Some(f) => f.clips.push((c, w)),
            None => files.push(File { source: src.id, path: &src.path, frames: src.frames, clips: vec![(c, w)] }),
        }
    }
    for f in &mut files {
        f.clips.sort_by_key(|(c, _)| (c.start, c.id));
    }
    files.sort_by_key(|f| f.clips.first().map(|(c, _)| (c.start, c.id)));

    let at = |c: &Clip, w: &WarpMap, t: Samples| w.source_at_time(&s.tempo, s.sample_rate, c.start, t) / sr;
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let Some(&(first, _)) = f.clips.first() else { continue };
        let duration = f.frames as f64 / sr;
        let grid = grid(s, &f.clips, duration);
        if grid.is_empty() {
            return Err(format!("clip {}'s bars all fall outside its file", first.id.0));
        }
        let mut cues = Vec::new();
        for t in &transitions {
            let mut cue = |clip: ClipId, bar: i64, name: &'static str, slot: u8| {
                let Some(&(c, w)) = f.clips.iter().find(|(c, _)| c.id == clip) else { return };
                let secs = at(c, w, bar_sample(s, bar));
                if !(secs.is_finite() && (0.0..duration).contains(&secs)) || cues.iter().any(|x: &Cue| x.slot == slot) {
                    return;
                }
                cues.push(Cue { name, slot, at: secs, memory: false });
            };
            cue(t.in_clip, t.entry_bar, "Mix in", 0);
            cue(t.in_clip, t.hit_bar(), "Bass hit", 1);
            cue(t.out_clip, t.entry_bar, "Mix out", 2);
            cue(t.out_clip, t.gone_bar(), "Bass gone", 3);
        }
        let memory: Vec<Cue> = cues.iter().filter(|c| c.slot == 0 || c.slot == 2).map(|c| Cue { memory: true, ..*c }).collect();
        cues.extend(memory);
        cues.sort_by_key(|c| (c.slot, c.memory));
        out.push(ExportTrack {
            source: f.source,
            path: PathBuf::from(f.path),
            title: String::new(),
            artist: String::new(),
            grid,
            cues,
            split: first.stems.clone(),
            first_clip: first.id,
        });
    }
    Ok(out)
}

/// File seconds of each bar's downbeat for a file of `duration` seconds played by beat-locked
/// `clips` (in timeline order): the file time heard on each session bar line inside a clip,
/// extended before the first and after the last at the edge bar lengths over the whole file;
/// only finite, strictly increasing times inside the file are kept.
fn grid(s: &Session, clips: &[(&Clip, &WarpMap)], duration: f64) -> Vec<f64> {
    let sr = s.sample_rate.as_f64();
    let at = |c: &Clip, w: &WarpMap, bar: i64| w.source_at_time(&s.tempo, s.sample_rate, c.start, bar_sample(s, bar)) / sr;
    // The bars heard inside the clips, and the bar lengths at either end.
    let mut heard = Vec::new();
    let (mut before, mut after) = (f64::NAN, f64::NAN);
    for (i, &(c, w)) in clips.iter().enumerate() {
        let first = bar_at_or_after(s, c.start);
        let mut bar = first;
        while bar_sample(s, bar) < c.end() && heard.len() < MAX_GRID_BARS {
            heard.push(at(c, w, bar));
            bar = bar.saturating_add(1);
        }
        let last = bar.saturating_sub(1).max(first);
        if i == 0 {
            before = at(c, w, first.saturating_add(1)) - at(c, w, first);
        }
        if i + 1 == clips.len() {
            after = at(c, w, last) - at(c, w, last.saturating_sub(1));
            if heard.is_empty() {
                heard.push(at(c, w, first));
            }
        }
    }
    let usable = |d: f64| d.is_finite() && d > 0.0;
    let mut out = Vec::new();
    if let (Some(&p0), true) = (heard.first(), usable(before)) {
        let n = (p0 / before).floor().clamp(0.0, MAX_GRID_BARS as f64) as usize;
        out.extend((1..=n).rev().map(|k| p0 - k as f64 * before));
    }
    out.extend_from_slice(&heard);
    if let (Some(&pn), true) = (heard.last(), usable(after)) {
        let n = ((duration - pn) / after).ceil().clamp(0.0, MAX_GRID_BARS as f64) as usize;
        out.extend((1..=n).map(|k| pn + k as f64 * after));
    }
    let mut last = f64::NEG_INFINITY;
    out.retain(|&v| {
        let keep = v.is_finite() && (0.0..duration).contains(&v) && v > last;
        if keep {
            last = v;
        }
        keep
    });
    out.truncate(MAX_GRID_BARS);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::cmd::dj::recipe::bar_sample;
    use crate::edit as ops;
    use serde_json::json;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, Clip, TrackKind, WarpMap};

    const SR: i64 = 48_000;
    /// Each test file: 100 s with a 125 bpm beat list from 0.1 s (a beat every 0.48 s, a bar
    /// every 1.92 s of the file).
    const FILE_SECS: f64 = 100.0;
    const FIRST: f64 = 0.1;
    const FILE_BEAT: f64 = 0.48;
    const FILE_BAR: f64 = 4.0 * FILE_BEAT;

    /// A file's beat-locked clip on `track` from bar `from` to bar `to`, starting on track beat
    /// `anchor`.
    fn clip(s: &mut Session, track: soundcraft_model::TrackId, name: &str, from: i64, to: i64, anchor: f64) -> ClipId {
        let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![0.0; (FILE_SECS * SR as f64) as usize]] };
        let src = crate::io::add_source(s, name, buf, None, FileFormat::Wav);
        let id = s.new_clip_id();
        let start = bar_sample(s, from);
        let mut c = Clip::audio(id, name, src, 0, start, bar_sample(s, to) - start);
        c.warp = WarpMap::new((0..220).map(|k| (FIRST + k as f64 * FILE_BEAT) * SR as f64).collect(), anchor);
        ops::place_clip(s, track, c);
        id
    }

    /// A three-track set at 128 bpm on two decks: A on bars 1..45, B on 25..69, C on 48..90
    /// (its downbeat a bar in, on bar 49). A -> B blends in on bar 25 (hit 41, gone 49), B -> C
    /// is quick on bar 49 (hit 57, gone 61). C is added first and B -> C built first, so the
    /// order comes from the timeline. Returns the engine and [A, B, C].
    fn set() -> (Engine, [ClipId; 3]) {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        let decks: Vec<_> = (0..2).map(|_| s.add_track(TrackKind::Audio, ChannelFormat::Mono, None)).collect();
        let c = clip(&mut s, decks[0], "c", 48, 90, -4.0);
        let a = clip(&mut s, decks[0], "a", 1, 45, 0.0);
        let b = clip(&mut s, decks[1], "b", 25, 69, 0.0);
        let mut e = Engine::new(s);
        e.execute("dj.transition", &json!({"out_clip": b.0, "in_clip": c.0, "entry_bar": 49, "style": "quick"})).unwrap();
        e.execute("dj.transition", &json!({"out_clip": a.0, "in_clip": b.0, "entry_bar": 25})).unwrap();
        (e, [a, b, c])
    }

    /// File seconds of the downbeat `bars` bars after the first.
    fn bar_secs(bars: f64) -> f64 {
        FIRST + bars * FILE_BAR
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    /// (slot, at, memory) of a track's cues.
    fn cues(t: &ExportTrack) -> Vec<(u8, f64, bool)> {
        t.cues.iter().map(|c| (c.slot, c.at, c.memory)).collect()
    }

    fn assert_cues(t: &ExportTrack, want: &[(u8, f64, bool)]) {
        let got = cues(t);
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, w) in got.iter().zip(want) {
            assert!(g.0 == w.0 && g.2 == w.2 && close(g.1, w.1), "{got:?} != {want:?}");
        }
    }

    #[test]
    fn tracks_come_in_timeline_order_with_their_files() {
        let (e, [a, b, c]) = set();
        let set = collect(e.session()).unwrap();
        assert_eq!(set.iter().map(|t| t.first_clip).collect::<Vec<_>>(), vec![a, b, c]);
        let s = e.session();
        for t in &set {
            let src = s.find_clip(t.first_clip).unwrap().1.source().unwrap();
            assert_eq!(t.source, src);
            assert_eq!(t.path, PathBuf::from(&s.source(src).unwrap().path));
            assert!(t.title.is_empty() && t.artist.is_empty() && t.split.is_none());
        }
    }

    #[test]
    fn the_grid_is_every_bar_of_the_file_from_the_first_downbeat() {
        let (e, _) = set();
        for t in collect(e.session()).unwrap() {
            // C starts a bar before its downbeat (anchor -4): its first bar is still the downbeat.
            assert!(close(t.grid[0], FIRST), "{:?}", &t.grid[..3]);
            // Every bar of the file, a file bar apart: 0.1 + 52 * 1.92 = 99.94 s is the last.
            assert_eq!(t.grid.len(), 53);
            assert!(t.grid.windows(2).all(|w| close(w[1] - w[0], FILE_BAR)));
        }
    }

    #[test]
    fn a_nudge_shifts_the_grid_and_cues_by_its_beats() {
        let (mut e, [_, b, _]) = set();
        let before = collect(e.session()).unwrap();
        e.execute("dj.nudge", &json!({"clip": b.0, "beats": 0.25})).unwrap();
        let after = collect(e.session()).unwrap();
        // B's music plays a quarter beat later: the file position on each bar line is a quarter
        // beat earlier. Its first downbeat (0.1 - 0.12 s) falls before the file start.
        let shift = 0.25 * FILE_BEAT;
        let (old, new) = (&before[1].grid, &after[1].grid);
        assert!(close(new[0], FIRST - shift + FILE_BAR), "{:?}", &new[..2]);
        assert_eq!(new.len(), old.len() - 1);
        assert!(new.iter().zip(&old[1..]).all(|(n, o)| close(*n, o - shift)));
        // Its mix-in cue falls before the file start too; the others move with it.
        let hit = |t: &ExportTrack| t.cues.iter().find(|c| c.slot == 1).unwrap().at;
        assert!(close(hit(&after[1]), hit(&before[1]) - shift));
        assert!(after[1].cues.iter().all(|c| c.slot != 0));
        // The other tracks are untouched.
        assert_eq!((&after[0], &after[2]), (&before[0], &before[2]));
    }

    #[test]
    fn cues_are_the_transitions_bars_in_file_seconds() {
        let (e, _) = set();
        let set = collect(e.session()).unwrap();
        // A (bars 1..): no mix-in; it mixes out at B's entry (bar 25) and its bass is gone on 49.
        assert_cues(&set[0], &[(2, bar_secs(24.0), false), (2, bar_secs(24.0), true), (3, bar_secs(48.0), false)]);
        // B (bars 25..): enters on 25, hits on 41; C enters on 49 and B's bass is gone on 61.
        assert_cues(
            &set[1],
            &[
                (0, bar_secs(0.0), false),
                (0, bar_secs(0.0), true),
                (1, bar_secs(16.0), false),
                (2, bar_secs(24.0), false),
                (2, bar_secs(24.0), true),
                (3, bar_secs(36.0), false),
            ],
        );
        // C (downbeat on bar 49): enters on 49, hits on 57; nothing after it.
        assert_cues(&set[2], &[(0, bar_secs(0.0), false), (0, bar_secs(0.0), true), (1, bar_secs(8.0), false)]);
        let names: Vec<&str> = set[1].cues.iter().filter(|c| !c.memory).map(|c| c.name).collect();
        assert_eq!(names, ["Mix in", "Bass hit", "Mix out", "Bass gone"]);
    }

    #[test]
    fn clips_of_one_file_make_one_track() {
        let (mut e, [a, b, c]) = set();
        let s = e.session_mut();
        let file = |s: &Session, id: ClipId| s.find_clip(id).unwrap().1.source().unwrap();
        let (fa, fc) = (file(s, a), file(s, c));
        let path = s.source(fa).unwrap().path.clone();
        s.sources.iter_mut().find(|x| x.id == fc).unwrap().path = path;
        let set = collect(e.session()).unwrap();
        assert_eq!(set.iter().map(|t| t.first_clip).collect::<Vec<_>>(), vec![a, b]);
        // C's clip plays A's file again from a bar before its downbeat: the grid stays one.
        assert_eq!(set[0].grid.len(), 53);
        assert!(set[0].grid.windows(2).all(|w| close(w[1] - w[0], FILE_BAR)));
        // Its cues come from both clips: mixed out of on bar 25, mixed into on bar 49.
        let slots: Vec<u8> = set[0].cues.iter().filter(|c| !c.memory).map(|c| c.slot).collect();
        assert_eq!(slots, [0, 1, 2, 3]);
        assert!(close(set[0].cues[0].at, bar_secs(0.0)) && close(set[0].cues.last().unwrap().at, bar_secs(48.0)));
    }

    #[test]
    fn a_session_without_transitions_or_with_an_unlocked_clip_is_refused() {
        let err = collect(&Session::default()).unwrap_err();
        assert!(err.contains("import a set first (DJ > Import mixforge Set)"), "{err}");
        let (mut e, [_, b, _]) = set();
        e.session_mut().find_clip_mut(b).unwrap().warp = None;
        let err = collect(e.session()).unwrap_err();
        assert!(err.contains(&format!("clip {}", b.0)) && err.contains("beat-locked"), "{err}");
    }

    /// A folder of its own for one test's files.
    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-export-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The pool holds each file from its first sample at the session rate, so a pool position over
    /// the session rate is a time in the file, resampled or not; and a beat-locked clip plays pool
    /// positions whatever its source offset, so trimming a clip leaves the file grid alone.
    #[test]
    fn pool_positions_over_the_session_rate_are_file_seconds() {
        const FILE_RATE: u32 = 44_100;
        let dir = folder("file-time");
        // 30 s at 44.1 kHz, a click at 1.25 s, beats every 0.5 s from 0.25 s (120 bpm).
        let mut x = vec![0.0f32; 30 * FILE_RATE as usize];
        x[(1.25 * f64::from(FILE_RATE)) as usize] = 1.0;
        let buf = AudioBuffer { sample_rate: FILE_RATE, channels: vec![x] };
        let bytes = soundcraft_audio_io::encode(&buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap();
        let beats: Vec<f64> = (0..60).map(|k| 0.25 + k as f64 * 0.5).collect();
        let mut e = Engine::default();
        assert_eq!(e.session().sample_rate.hz(), SR as u32);
        e.session_mut().tempo.set_tempo(0, 120.0).unwrap();
        let mut clips = Vec::new();
        for (name, bar) in [("a.wav", 1), ("b.wav", 9)] {
            let path = dir.join(name);
            std::fs::write(&path, &bytes).unwrap();
            let r = e.execute("dj.load_warped", &json!({"path": path.to_string_lossy(), "beats": beats, "at_bar": bar})).unwrap();
            clips.push(ClipId(r["clip"].as_u64().unwrap()));
        }
        e.execute("dj.transition", &json!({"out_clip": clips[0].0, "in_clip": clips[1].0, "entry_bar": 10, "build_bars": 2, "out_bars": 2})).unwrap();

        // The click sits at 1.25 s of the pool audio, resampled to 48 kHz.
        let s = e.session();
        let src = s.find_clip(clips[0]).unwrap().1.source().unwrap();
        assert_eq!(s.source(src).unwrap().sample_rate, SR as u32);
        let pool = &s.pool.get(src).unwrap().buffer.channels[0];
        let peak = (0..pool.len()).max_by(|&i, &j| pool[i].abs().total_cmp(&pool[j].abs())).unwrap();
        assert!((peak as f64 / SR as f64 - 1.25).abs() <= 1.0 / SR as f64, "click at pool sample {peak}");

        // The grid is the analysed downbeats in file seconds: 0.25 s, then every 2 s.
        let on_downbeats = |g: &[f64]| g.iter().enumerate().all(|(k, v)| (v - (0.25 + 2.0 * k as f64)).abs() < 1e-6);
        let grid = collect(e.session()).unwrap()[0].grid.clone();
        assert_eq!(grid.len(), 15, "{grid:?}");
        assert!(on_downbeats(&grid), "{grid:?}");

        // Trimmed (a source offset) the clip plays the same file positions on the same bars.
        let start = bar_sample(e.session(), 3);
        e.execute("edit.trim_clip", &json!({"clip": clips[0].0, "start": start})).unwrap();
        let c = e.session().find_clip(clips[0]).unwrap().1;
        assert!(c.source_offset() > 0 && c.start == start);
        let trimmed = collect(e.session()).unwrap()[0].grid.clone();
        assert_eq!(trimmed.len(), grid.len());
        assert!(on_downbeats(&trimmed), "{trimmed:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
