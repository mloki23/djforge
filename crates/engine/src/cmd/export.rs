//! Exporting a DJ set for playing out: the set's tracks in order, each file's bar grid and its
//! cues in the file's own time ([`collect`]), and the set's Rekordbox XML ([`rekordbox`]).

// Used by `dj.export_set`, which comes with the job that runs the export.
#![cfg_attr(not(test), allow(dead_code))]

use super::dj::recipe::{bar_at_or_after, bar_sample};
use soundcraft_model::{Clip, ClipId, Session, Source, SourceId, StemSet, Transition, WarpMap};
use std::path::{Path, PathBuf};

#[path = "export_rekordbox.rs"]
pub(crate) mod rekordbox;

/// One track of an exported set: a source file with its bar grid and cues, in seconds of the
/// decoded file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExportTrack {
    pub source: SourceId,
    /// The file exported: the one the audio was imported from when it is still there.
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    /// File seconds of each bar's downbeat, strictly increasing, over the whole file.
    pub grid: Vec<f64>,
    pub cues: Vec<Cue>,
    pub split: Option<StemSet>,
    /// The earliest of the set's clips playing this file.
    pub first_clip: ClipId,
    /// What the export can't vouch for.
    pub doubts: Vec<String>,
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
/// Clips of one file whose bar lines are further apart than this (seconds) get a doubt.
const DISAGREE_SECS: f64 = 0.005;
/// Session beats in a bar: export needs 4/4 throughout.
const BEATS_PER_BAR: f64 = 4.0;
/// The doubt of a track exported from the session's own copy of its audio.
pub(crate) const SESSION_COPY: &str = "exported the session's copy (resampled); re-import the track to export its original";

/// The set in session `s`, from its DJ transitions: the files their clips play, one track per
/// file in the order its first clip starts, each with its bar grid and cues in file seconds.
///
/// A beat-locked clip plays its source-pool audio at [`WarpMap::source_at_time`] (its source
/// offset doesn't enter), and the pool holds the whole decoded file from its first sample at the
/// session rate, so a pool position over the session rate is a time in the file.
///
/// Grid: one clip governs each file, its longest (the first of equally long ones). Its first
/// session bar line plays some track beat; every fourth track beat from there, through the
/// clip's analysed beats ([`WarpMap::source_at`], nudges included), is a bar, over the whole
/// file. Other clips of the file whose bar lines are more than 5 ms off it get a doubt.
///
/// Cues, each through the transition's own clip: A "Mix in" on the downbeat of the entry bar
/// and B "Bass hit" on the hit bar of the transition into the track; C "Mix out" on the entry
/// bar and D "Bass gone" on the gone bar of the transition out of it; memory cues at A and C. A
/// file played twice gets its later mix points as memory cues. A cue outside the file is left
/// out with a doubt.
///
/// Refused: a session without transitions, with a meter other than 4/4, or a transition clip
/// that is missing or not beat-locked.
pub(crate) fn collect(s: &Session) -> Result<Vec<ExportTrack>, String> {
    let mut transitions: Vec<&Transition> = s.dj.transitions.iter().collect();
    if transitions.is_empty() {
        return Err("the session has no DJ transitions: import a set first (DJ > Import mixforge Set)".into());
    }
    if let Some(m) = s.tempo.meters().iter().find(|m| (m.numerator, m.denominator) != (4, 4)) {
        let bar = s.tempo.bar_beat_at_tick(m.tick).bar;
        return Err(format!(
            "bar {bar} is in {}/{}: exported grids need 4/4 throughout; set the meter to 4/4 and export again",
            m.numerator, m.denominator
        ));
    }
    transitions.sort_by_key(|t| (t.entry_bar, t.id));
    let sr = s.sample_rate.as_f64();

    // The files, each with its clips in timeline order.
    struct File<'a> {
        key: PathBuf,
        path: PathBuf,
        source: &'a Source,
        doubts: Vec<String>,
        clips: Vec<(&'a Clip, &'a WarpMap)>,
    }
    let mut files: Vec<File> = Vec::new();
    for id in transitions.iter().flat_map(|t| [t.out_clip, t.in_clip]) {
        let c = s.find_clip(id).map(|(_, c)| c).ok_or_else(|| format!("clip {} of a transition is not on its track's active playlist", id.0))?;
        let w = c.warp.as_ref().filter(|w| w.is_usable() && c.is_audio()).ok_or_else(|| format!("clip {} is not beat-locked", id.0))?;
        let src = c.source().and_then(|x| s.source(x)).ok_or_else(|| format!("clip {} has no source", id.0))?;
        let (path, doubt) = file_of(s, src);
        let key = crate::io::file_key(&path);
        match files.iter_mut().find(|f| f.key == key) {
            Some(f) if f.clips.iter().any(|(k, _)| k.id == id) => {}
            Some(f) => f.clips.push((c, w)),
            None => files.push(File { key, path, source: src, doubts: doubt.into_iter().collect(), clips: vec![(c, w)] }),
        }
    }
    for f in &mut files {
        f.clips.sort_by_key(|(c, _)| (c.start, c.id));
    }
    files.sort_by_key(|f| f.clips.first().map(|(c, _)| (c.start, c.id)));

    let mut out = Vec::with_capacity(files.len());
    for mut f in files {
        let Some(&(first, first_warp)) = f.clips.first() else { continue };
        let name = f.path.file_name().map_or_else(|| f.path.display().to_string(), |n| n.to_string_lossy().into_owned());
        // The governing clip: the longest, the first of equally long ones.
        let (mut gc, mut gw) = (first, first_warp);
        for &(c, w) in &f.clips {
            if c.length > gc.length {
                (gc, gw) = (c, w);
            }
        }
        let frames = f.source.frames as f64;
        let duration = frames / sr;
        let b0 = gw.track_beat_at(&s.tempo, s.sample_rate, gc.start, bar_sample(s, bar_at_or_after(s, gc.start)));
        let grid = grid(gw, b0, frames, sr);
        if grid.is_empty() {
            return Err(format!("clip {}'s bars all fall outside its file {name}", gc.id.0));
        }
        // How far the other clips' bar lines are from the governing grid.
        let worst = f
            .clips
            .iter()
            .filter(|(c, _)| c.id != gc.id)
            .filter_map(|&(c, w)| {
                let t = bar_sample(s, bar_at_or_after(s, c.start));
                (t < c.end()).then(|| {
                    let heard = w.source_at_time(&s.tempo, s.sample_rate, c.start, t);
                    let k = ((gw.beat_at_source(heard) - b0) / BEATS_PER_BAR).round();
                    (heard - gw.source_at(b0 + BEATS_PER_BAR * k)).abs() / sr
                })
            })
            .filter(|d| d.is_finite())
            .fold(0.0f64, f64::max);
        if worst > DISAGREE_SECS {
            f.doubts.push(format!("clips of {name} disagree by {:.0} ms; exported clip {}'s grid", worst * 1000.0, gc.id.0));
        }

        let mut cues: Vec<Cue> = Vec::new();
        let mut twice = false;
        for t in &transitions {
            for (clip, bar, cue, slot) in [
                (t.in_clip, t.entry_bar, "Mix in", 0),
                (t.in_clip, t.hit_bar(), "Bass hit", 1),
                (t.out_clip, t.entry_bar, "Mix out", 2),
                (t.out_clip, t.gone_bar(), "Bass gone", 3),
            ] {
                let Some(&(c, w)) = f.clips.iter().find(|(c, _)| c.id == clip) else { continue };
                let at = w.source_at_time(&s.tempo, s.sample_rate, c.start, bar_sample(s, bar)) / sr;
                if !(at.is_finite() && (0.0..duration).contains(&at)) {
                    f.doubts.push(format!("the {cue} cue (bar {bar}) falls outside the file; left out"));
                    continue;
                }
                if cues.iter().any(|x| x.slot == slot && !x.memory) {
                    twice = true;
                    cues.push(Cue { name: cue, slot, at, memory: true });
                    continue;
                }
                cues.push(Cue { name: cue, slot, at, memory: false });
                if slot == 0 || slot == 2 {
                    cues.push(Cue { name: cue, slot, at, memory: true });
                }
            }
        }
        if twice {
            f.doubts.push(format!("{name} plays more than once in the set; its later mix points are memory cues"));
        }
        cues.sort_by(|a, b| (a.slot, a.memory).cmp(&(b.slot, b.memory)).then(a.at.total_cmp(&b.at)));
        out.push(ExportTrack {
            source: f.source.id,
            path: f.path,
            title: String::new(),
            artist: String::new(),
            grid,
            cues,
            split: first.stems.clone(),
            first_clip: first.id,
            doubts: f.doubts,
        });
    }
    Ok(out)
}

/// The file to export for source `src`, and a doubt about it: the file it was imported from
/// when that is still there and readable; else its own file when that is a user's file
/// referenced in place; else the session's copy of it (with a doubt); else (with a doubt) the
/// path it should be at.
fn file_of(s: &Session, src: &Source) -> (PathBuf, Option<String>) {
    let exists = |p: &Path| crate::io::file_len(p).is_some();
    if let Some(origin) = src.origin.as_deref().map(PathBuf::from).filter(|p| p.is_absolute() && exists(p)) {
        return (origin, None);
    }
    let stored = Path::new(&src.path);
    let full = if stored.is_absolute() { stored.to_path_buf() } else { s.folder.as_deref().map_or_else(|| stored.to_path_buf(), |f| f.join(stored)) };
    if exists(&full) {
        let in_place = stored.is_absolute() && !crate::io::in_audio_files(stored);
        return (full, (!in_place).then(|| SESSION_COPY.to_string()));
    }
    let missing = format!("no audio file found for `{}` (at {}); relink it (File > Relink Audio) and export again", src.name, full.display());
    (src.origin.as_deref().map_or(full, PathBuf::from), Some(missing))
}

/// File seconds of every bar of a file of `frames` pool samples at rate `sr` whose clip plays
/// track beat `b0` on a bar line: every fourth track beat from `b0` through warp map `w`, both
/// ways, inside the file; finite and strictly increasing.
fn grid(w: &WarpMap, b0: f64, frames: f64, sr: f64) -> Vec<f64> {
    let lo = w.beat_at_source(0.0);
    if !(b0.is_finite() && lo.is_finite()) {
        return Vec::new();
    }
    // One bar of slack before the file start: the filter below decides.
    let first = ((lo - b0) / BEATS_PER_BAR).floor() - 1.0;
    let duration = frames / sr;
    let mut out = Vec::new();
    let mut last = f64::NEG_INFINITY;
    for i in 0..MAX_GRID_BARS.saturating_add(2) {
        let at = w.source_at(b0 + BEATS_PER_BAR * (first + i as f64)) / sr;
        if at >= duration || out.len() >= MAX_GRID_BARS {
            break;
        }
        if at.is_finite() && at >= 0.0 && at > last {
            out.push(at);
            last = at;
        }
    }
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
            // These sources have no files: the export names where they should be and says so.
            assert_eq!(t.path, PathBuf::from(&s.source(src).unwrap().path));
            assert!(t.doubts.len() == 1 && t.doubts[0].starts_with("no audio file found"), "{:?}", t.doubts);
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
        assert!(set[0].doubts.iter().all(|d| !d.contains("disagree")), "{:?}", set[0].doubts);

        // Nudged apart, the longer clip (A) governs: one grid, no jump where C plays, and a doubt.
        e.execute("dj.nudge", &json!({"clip": c.0, "beats": 0.25})).unwrap();
        let set = collect(e.session()).unwrap();
        assert_eq!(set[0].grid.len(), 53);
        assert!(close(set[0].grid[0], FIRST) && set[0].grid.windows(2).all(|w| close(w[1] - w[0], FILE_BAR)));
        let want = format!("disagree by 120 ms; exported clip {}'s grid", a.0);
        assert!(set[0].doubts.iter().any(|d| d.contains(&want)), "{:?}", set[0].doubts);
    }

    #[test]
    fn a_file_mixed_in_twice_keeps_its_later_mix_points_as_memory_cues() {
        let (mut e, [_, b, c]) = set();
        // C plays B's file: it is mixed into on bar 25 (B) and again on bar 49 (C).
        let s = e.session_mut();
        let file = |s: &Session, id: ClipId| s.find_clip(id).unwrap().1.source().unwrap();
        let (fb, fc) = (file(s, b), file(s, c));
        let path = s.source(fb).unwrap().path.clone();
        s.sources.iter_mut().find(|x| x.id == fc).unwrap().path = path;
        let set = collect(e.session()).unwrap();
        assert_eq!(set.len(), 2);
        let t = &set[1];
        for slot in 0..4 {
            assert!(t.cues.iter().filter(|c| c.slot == slot && !c.memory).count() <= 1, "{:?}", cues(t));
        }
        // B's mix-in and hit are the hot cues; C's are memory cues (C's hit is bar 57: 8 bars in).
        assert!(t.cues.iter().any(|c| c.slot == 1 && c.memory && close(c.at, bar_secs(8.0))), "{:?}", cues(t));
        assert!(t.doubts.iter().any(|d| d.contains("plays more than once")), "{:?}", t.doubts);
    }

    #[test]
    fn a_tempo_ramp_keeps_each_file_s_own_bar_lengths() {
        let (mut e, _) = set();
        e.execute("dj.tempo_ramp", &json!({"from_bar": 25, "bars": 16, "to_bpm": 136.0})).unwrap();
        for t in collect(e.session()).unwrap() {
            // Relocking the clips to the ramp rounds their starts to a sample (20 µs).
            assert!((t.grid[0] - FIRST).abs() < 1.0 / SR as f64, "{:?}", &t.grid[..3]);
            assert!(t.grid.windows(2).all(|w| (w[1] - w[0] - FILE_BAR).abs() < 1e-4), "{:?}", t.grid);
        }
    }

    #[test]
    fn a_track_off_the_bar_lines_takes_its_bars_from_the_session() {
        let (mut e, [_, b, _]) = set();
        // B's clip starts on track beat -2: its downbeats fall half a bar after the session's
        // bar lines, so the session's bars are its file beats 2, 6, 10...
        e.session_mut().find_clip_mut(b).unwrap().warp.as_mut().unwrap().anchor = -2.0;
        let t = &collect(e.session()).unwrap()[1];
        assert!(close(t.grid[0], FIRST + 2.0 * FILE_BEAT), "{:?}", &t.grid[..3]);
        assert!(t.grid.windows(2).all(|w| close(w[1] - w[0], FILE_BAR)));
    }

    #[test]
    fn a_meter_other_than_four_four_is_refused() {
        let (mut e, _) = set();
        let s = e.session_mut();
        let tick = s.tempo.bar_start_tick(100);
        s.tempo.set_meter(tick, 3, 4).unwrap();
        let err = collect(e.session()).unwrap_err();
        assert!(err.contains("bar 100 is in 3/4") && err.contains("4/4"), "{err}");
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
        let set = collect(e.session()).unwrap();
        // The file it was imported from, though the session plays a resampled copy.
        assert_eq!(crate::io::file_key(&set[0].path), crate::io::file_key(&dir.join("a.wav")));
        assert!(set[0].doubts.is_empty(), "{:?}", set[0].doubts);
        let grid = set[0].grid.clone();
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
