//! Exporting a DJ set for playing out (`dj.export_set`): the set's tracks in order, each file's
//! bar grid and cues in the file's own time ([`collect`]); prep's level-matched copies of the
//! files; the set's Rekordbox XML ([`rekordbox`]); the set in Mixxx's library through mixforge
//! ([`MixxxRunner`]); NI STEM files of its split tracks ([`stemfile`]); and a report.

use super::dj::recipe::{bar_at_or_after, bar_sample};
use serde_json::{Value, json};
use soundcraft_model::{Clip, Session, Source, SourceId, StemSet, Transition, WarpMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

#[path = "export_rekordbox.rs"]
pub(crate) mod rekordbox;
#[path = "export_stems.rs"]
pub(crate) mod stemfile;

/// One track of an exported set: a source file with its bar grid and cues, in seconds of the
/// decoded file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExportTrack {
    pub source: SourceId,
    /// The file exported: the one the audio was imported from when it is still there.
    pub path: PathBuf,
    /// Whether `path` is a file that may be read (else nothing reads it: it is not prepped and
    /// not sent to Mixxx).
    pub found: bool,
    /// File seconds of each bar's downbeat, strictly increasing, over the whole file.
    pub grid: Vec<f64>,
    pub cues: Vec<Cue>,
    /// Bars of the first transition out of the track, from its mix out to its bass gone.
    pub mix_out_bars: Option<i64>,
    pub split: Option<StemSet>,
    /// The earliest of the set's clips playing this file.
    #[cfg(test)]
    pub first_clip: soundcraft_model::ClipId,
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
pub(crate) const SESSION_COPY: &str = "exported the session's copy";
/// What the session's copy of an imported file is.
const RESAMPLED: &str = "(resampled when it was imported); re-import the track to export its original";
/// What the session's copy of audio with no file it was imported from is.
const SESSION_AUDIO: &str = "(the set plays audio made or edited in the session, such as a recording, a rendered effect or consolidated clips; or the session is older than exports)";
/// The doubt of an original that changed since it was imported.
const CHANGED: &str = "the original changed since import; exported the session's audio";
/// Extensions of movies: the audio of a movie has no audio file of its own to export.
const MOVIES: [&str; 9] = ["mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "mpg", "mpeg"];

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
/// Timeline: a clip's warp map holds mixforge's beat times, used as they are, so these file
/// seconds are on mixforge's timeline (its decoder, pedalboard, starts 1105 samples later than a
/// gapless decoder on Marty's MP3s): the timeline mixforge's Mixxx exporter expects. Nothing is
/// converted here; Rekordbox's timeline is checked against real files in the stage 5 gate.
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
        found: bool,
        doubts: Vec<String>,
        clips: Vec<(&'a Clip, &'a WarpMap)>,
    }
    let mut files: Vec<File> = Vec::new();
    for id in transitions.iter().flat_map(|t| [t.out_clip, t.in_clip]) {
        let c = s.find_clip(id).map(|(_, c)| c).ok_or_else(|| format!("clip {} of a transition is not on its track's active playlist", id.0))?;
        let w = c.warp.as_ref().filter(|w| w.is_usable() && c.is_audio()).ok_or_else(|| format!("clip {} is not beat-locked", id.0))?;
        let src = c.source().and_then(|x| s.source(x)).ok_or_else(|| format!("clip {} has no source", id.0))?;
        let (path, doubts, found) = file_of(s, src);
        let key = crate::io::file_key(&path);
        match files.iter_mut().find(|f| f.key == key) {
            Some(f) if f.clips.iter().any(|(k, _)| k.id == id) => {}
            Some(f) => f.clips.push((c, w)),
            None => files.push(File { key, path, source: src, found, doubts, clips: vec![(c, w)] }),
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
        // A session copy of an MP3 is its gapless decode: the grid and cues, on mixforge's
        // timeline, sit the MP3's priming later on it, so they move that much earlier.
        let mut grid = grid;
        if f.found {
            let shift = copy_shift(s, f.source, &f.path, &mut f.doubts);
            if shift > 0.0 {
                grid = grid.into_iter().map(|g| g - shift).filter(|g| *g >= 0.0).collect();
                let before = cues.len();
                cues = cues.into_iter().map(|c| Cue { at: c.at - shift, ..c }).filter(|c| c.at >= 0.0).collect();
                if cues.len() < before {
                    f.doubts.push("a cue fell before the start of the session's copy; left out".into());
                }
            }
        }
        let mix_out_bars =
            transitions.iter().find(|t| f.clips.iter().any(|(c, _)| c.id == t.out_clip)).map(|t| t.gone_bar().saturating_sub(t.entry_bar));
        out.push(ExportTrack {
            source: f.source.id,
            path: f.path,
            found: f.found,
            grid,
            cues,
            mix_out_bars,
            // Any split clip of the file gives its stems (the first in the timeline), as
            // [`set_is_split`] has it.
            split: f.clips.iter().find_map(|(c, _)| c.stems.clone()),
            #[cfg(test)]
            first_clip: first.id,
            doubts: f.doubts,
        });
    }
    Ok(out)
}

/// Whether stem files can be asked of the set in `s`: a clip of one of its DJ transitions is
/// split (its file's stems are then exported, whichever of the file's clips is split).
pub fn set_is_split(s: &Session) -> bool {
    s.dj.transitions.iter().flat_map(|t| [t.out_clip, t.in_clip]).any(|id| s.find_clip(id).is_some_and(|(_, c)| c.stems.is_some()))
}

/// Why the session `s` can't be exported as a set (no transitions, a meter other than 4/4, a
/// transition clip missing or not beat-locked), or `None` when it can.
pub fn set_refusal(s: &Session) -> Option<String> {
    collect(s).err()
}

/// The file to export for source `src`, the doubts about it, and whether it was found. Every path
/// the session names goes through [`crate::io::stored_path_in`] first (only local files or the
/// session's own share; relative paths only from a saved session's folder): a path refused there
/// is never read.
///
/// The file it was imported from when that is still there, unchanged since (its size and date
/// as recorded) and not a movie; else its own file (a user's file referenced in place, or the
/// session's copy, with a doubt); else (with a doubt) the path it should be at, unread.
fn file_of(s: &Session, src: &Source) -> (PathBuf, Vec<String>, bool) {
    let resolve = |p: &str| crate::io::stored_path_in(p, s.folder.as_deref());
    let mut doubts = Vec::new();
    let is_movie = |p: &str| Path::new(p).extension().and_then(|x| x.to_str()).is_some_and(|x| MOVIES.iter().any(|m| x.eq_ignore_ascii_case(m)));
    let origin = match src.origin.as_deref().filter(|o| !is_movie(o)) {
        None => None,
        Some(o) => match resolve(o) {
            Ok(p) => Some(p),
            Err(why) => {
                doubts.push(format!("the file it was imported from ({o}) is {why}"));
                None
            }
        },
    };
    let mut changed = false;
    if let Some(p) = origin.as_ref().filter(|p| crate::io::file_found(p)) {
        let (len, ms) = crate::io::saved_stamp(p);
        changed = (src.origin_len != 0 && len != src.origin_len) || (src.origin_modified_ms != 0 && ms != 0 && ms != src.origin_modified_ms);
        if !changed {
            return (p.clone(), doubts, true);
        }
    }
    let stored = Path::new(&src.path);
    match resolve(&src.path) {
        Ok(full) if crate::io::file_found(&full) => {
            let in_place = stored.is_absolute() && !crate::io::in_audio_files(stored);
            if changed {
                let same = origin.as_ref().is_some_and(|o| crate::io::file_key(o) == crate::io::file_key(&full));
                doubts.push(if same {
                    "the file changed since it was imported; its grid and cues may no longer fit".to_string()
                } else {
                    CHANGED.to_string()
                });
            } else if !in_place {
                doubts.push(format!("{SESSION_COPY} {}", if src.origin.is_some() { RESAMPLED } else { SESSION_AUDIO }));
            }
            (full, doubts, true)
        }
        resolved => {
            let at = match &resolved {
                Ok(full) => full.display().to_string(),
                Err(why) => format!("{}, {why}", src.path),
            };
            doubts.push(format!("no audio file found for `{}` (at {at}); relink it (File > Relink Audio) and export again", src.name));
            // A refused absolute path (another computer's share, a device) is not named as the
            // track's file; a relative one names no place, so it stays as the track's identity.
            let path = origin.or_else(|| resolved.ok()).unwrap_or_else(|| PathBuf::from(if stored.is_relative() { &src.path } else { &src.name }));
            (path, doubts, false)
        }
    }
}

/// How far (seconds) the grid and cues move earlier on `path` when it is the session's copy (not
/// an MP3) of a source imported from an MP3: the original's priming, which the copy (a gapless
/// decode) doesn't have. 0 otherwise; 0 with a doubt when the original can't be read for it.
fn copy_shift(s: &Session, src: &Source, path: &Path, doubts: &mut Vec<String>) -> f64 {
    let mp3 = |p: &str| Path::new(p).extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3"));
    let from_mp3 = src.origin.as_deref().map_or_else(|| mp3(&src.name), mp3);
    if !from_mp3 || path.to_str().is_some_and(mp3) {
        return 0.0;
    }
    let primed = src
        .origin
        .as_deref()
        .and_then(|o| crate::io::stored_path_in(o, s.folder.as_deref()).ok())
        .filter(|o| crate::io::file_found(o))
        .and_then(|o| crate::io::read_head(&o, 4 << 20).ok())
        .and_then(|head| soundcraft_audio_io::mp3gain::gapless_priming(&head));
    match primed {
        Some((n, rate)) if rate > 0 => f64::from(n) / f64::from(rate),
        _ => {
            doubts.push("grid and cues may sit ~23 ms late on this copy (the original MP3 isn't there to tell)".into());
            0.0
        }
    }
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

// ─── Mixxx ──────────────────────────────────────────────────────────────────────────────────────

/// What writes an exported set into Mixxx's library: given mixforge's plan (`{tracks: [..]}`)
/// and the set's name, it returns mixforge's summary or why it could not.
pub type MixxxRunner = Arc<dyn Fn(&Value, &str) -> std::result::Result<Value, String> + Send + Sync>;

/// The sidecar that runs `mixforge.mixxx.export`: prints its summary as JSON, or `{error}`.
const MIXFORGE_MIXXX: &str = include_str!("mixforge_mixxx.py");
/// Shortest time the Mixxx sidecar is given; a set of more than 12 tracks gets 10 s a track.
const MIXXX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How mixforge's refusal while Mixxx runs starts.
const MIXXX_RUNNING: &str = "Close Mixxx first";
/// Bars of a track's outro when no transition leaves it (mixforge's default).
const DEFAULT_TRANSITION_BARS: i64 = 16;
/// mixforge's note names, from C.
const PITCHES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// The runner that calls `mixforge.mixxx.export(plan, set)` through the python in
/// `DJFORGE_PYTHON` against the mixforge checkout in `DJFORGE_MIXFORGE`, the plan on its standard
/// input, killed after two minutes. mixforge refuses while Mixxx runs and backs up Mixxx's
/// database before writing it.
pub fn mixforge_mixxx_runner() -> MixxxRunner {
    Arc::new(run_mixforge_mixxx)
}

fn run_mixforge_mixxx(plan: &Value, set: &str) -> std::result::Result<Value, String> {
    let var = |name: &str, what: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty()).ok_or_else(|| format!("set {name} to {what}"));
    let python = var("DJFORGE_PYTHON", "the python.exe of mixforge's environment")?;
    let mixforge = var("DJFORGE_MIXFORGE", "the mixforge folder (the one holding the `mixforge` package)")?;
    let input = serde_json::to_vec(plan).map_err(|err| err.to_string())?;
    let mut command = std::process::Command::new(&python);
    command.args(["-I", "-c", MIXFORGE_MIXXX, &mixforge, set]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: the GUI must not flash a console.
        command.creation_flags(0x0800_0000);
    }
    let tracks = plan.get("tracks").and_then(Value::as_array).map_or(0, Vec::len);
    let limit = MIXXX_TIMEOUT.max(std::time::Duration::from_secs(10u64.saturating_mul(tracks as u64)));
    let out = match super::dj::output_fed(&mut command, Some(input), limit, &|| false) {
        Ok(super::dj::Sidecar::Ran(out)) => out,
        Ok(super::dj::Sidecar::TimedOut) => {
            return Err(format!("mixforge's Mixxx export timed out after {} s — Mixxx may have been written; check it", limit.as_secs()));
        }
        Ok(super::dj::Sidecar::LeftHolding) => return Err("the mixforge sidecar left a process holding its output".into()),
        Ok(super::dj::Sidecar::Stopped) => return Err("mixforge's Mixxx export was stopped".into()),
        Err(err) => return Err(format!("could not run DJFORGE_PYTHON ({python}): {err}")),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines.get(lines.len().saturating_sub(5)..).unwrap_or_default().join("\n");
        return Err(format!("mixforge printed nothing ({}): {tail}", out.status));
    }
    sidecar_result(&stdout)
}

/// What the Mixxx sidecar printed (`stdout`, not empty) as its result: the last line printed,
/// since mixforge may print notes before it, as a JSON object; its `{error}` as the error.
fn sidecar_result(stdout: &str) -> std::result::Result<Value, String> {
    let text = stdout.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or_default();
    let head = || text.chars().take(300).collect::<String>();
    let v: Value = serde_json::from_str(text).map_err(|_| format!("mixforge printed something that is not JSON: {}", head()))?;
    if let Some(msg) = v.get("error") {
        return Err(msg.as_str().map_or_else(|| msg.to_string(), str::to_string));
    }
    if !v.is_object() {
        return Err(format!("mixforge printed something that is not an object: {}", head()));
    }
    Ok(v)
}

/// Key `k` (Camelot or musical notation) as mixforge names it ('Am', 'F#'; sharps only): the
/// inverse of its Camelot code, as mixforge's `camelot_to_key`.
fn mixforge_key(k: &str) -> Option<String> {
    let camelot = super::prep::to_camelot(k)?;
    let (number, letter) = camelot.split_at(camelot.len().checked_sub(1)?);
    let n: usize = number.parse().ok()?;
    // Camelot follows the circle of fifths from C major = 8B; nA is the relative minor of nB.
    let major = (n + 4) * 7 % 12;
    match letter {
        "B" => PITCHES.get(major).map(|p| (*p).to_string()),
        _ => PITCHES.get((major + 9) % 12).map(|p| format!("{p}m")),
    }
}

/// mixforge's plan entry for track `t` exported as `path` with these tags: its quarter-note BPM
/// from the median bar of its grid, its first downbeat, cue in (A, else the first downbeat), out
/// start (C) and outro bars, in file seconds; its key the first of `keys` that is one. With a
/// doubt when its bars vary by more than 5 ms (Mixxx gets one tempo). Why it can't go to Mixxx
/// otherwise.
fn mixxx_track(
    t: &ExportTrack,
    path: &Path,
    title: &str,
    artist: Option<&str>,
    keys: &[Option<&str>],
) -> std::result::Result<(Value, Option<String>), String> {
    let path = path.to_str().ok_or("its path is not Unicode")?;
    let key = keys.iter().flatten().find_map(|k| mixforge_key(k)).ok_or("its key is not known (no key tag and no analysis)")?;
    let mut bars: Vec<f64> = t.grid.windows(2).filter_map(|w| if let [a, b] = w { Some(b - a) } else { None }).collect();
    bars.sort_by(f64::total_cmp);
    let bar = bars.get(bars.len() / 2).copied().filter(|b| b.is_finite() && *b > 0.0).ok_or("its grid has fewer than two bars")?;
    let first = t.grid.first().copied().ok_or("its grid has fewer than two bars")?;
    let spread = bars.last().zip(bars.first()).map_or(0.0, |(hi, lo)| hi - lo);
    let doubt = (spread > DISAGREE_SECS).then(|| {
        format!("Mixxx gets one tempo ({:.2} bpm); this track's bars vary by {:.0} ms, so its Mixxx grid drifts", 240.0 / bar, spread * 1000.0)
    });
    let hot = |slot: u8| t.cues.iter().find(|c| c.slot == slot && !c.memory).map(|c| c.at);
    let entry = json!({
        "path": path,
        "bpm": 4.0 * 60.0 / bar,
        "tempo_multiplier": 1,
        "first_downbeat": first,
        "cue_in": hot(0).unwrap_or(first),
        "out_start": hot(2),
        "transition_bars": t.mix_out_bars.unwrap_or(DEFAULT_TRANSITION_BARS),
        "key": key,
        "title": title,
        "artist": artist.unwrap_or_default(),
        "sections": [],
    });
    Ok((entry, doubt))
}

/// What `dj.export_set` reports for Mixxx after `run` was given `plan` for set `set`: what it
/// wrote, or why it was skipped (a running Mixxx said plainly).
fn mixxx_export(run: &MixxxRunner, plan: &Value, set: &str) -> Value {
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(plan, set))).unwrap_or_else(|p| Err(crate::panic_message(&*p)));
    match ran {
        Ok(summary) => json!({
            "playlist": summary.get("playlist"),
            "backup": summary.get("backup"),
            "tracks": plan.get("tracks").and_then(Value::as_array).map_or(0, Vec::len),
            "summary": summary,
        }),
        Err(m) if m.contains(MIXXX_RUNNING) => json!({"skipped": "Mixxx is running — close it and export again"}),
        Err(m) => json!({"skipped": m}),
    }
}

// ─── dj.export_set ──────────────────────────────────────────────────────────────────────────────

const CMD: &str = "dj.export_set";
/// How the export report starts: an existing one is replaced only when it starts so.
const REPORT_HEADER: &str = "djforge set export: ";
/// What an exported Rekordbox XML holds: an existing one is replaced only when it does.
const XML_MARK: &str = "<PRODUCT Name=\"djforge\"";
/// The targets an export can write.
const TARGETS: [&str; 2] = ["rekordbox", "mixxx"];
/// Prep's loudness target and true-peak ceiling for the copies.
const TARGET_LUFS: f64 = -14.0;
const CEILING: f64 = -1.0;
/// How to bring the XML into Rekordbox.
const REKORDBOX_STEPS: &str =
    "Rekordbox › Preferences › Advanced › rekordbox xml: choose this file; then drag the playlist into your collection; then export to USB.";

pub fn specs() -> Vec<super::CommandSpec> {
    vec![crate::cmd!(
        noundo "dj.export_set",
        "Export Set",
        [],
        None,
        "{targets?: [\"rekordbox\", \"mixxx\"] (default both), stems?: false, out?: folder path (default Music/djforge-prepped/<session name>)} — exports the session's DJ set (its transitions' tracks in timeline order) for playing out: preps level-matched copies of the files into `out` (as prep.tracks; originals are never written) and writes <session name>.rekordbox.xml there (one TEMPO per bar from the session's grid, nudges included; hot cues A mix in, B bass hit, C mix out, D bass gone; memory cues at A and C) and <session name>.export.txt; with \"mixxx\", writes the set (the copies, each with its grid at its median BPM, key and cues) into Mixxx's library through mixforge (skipped while Mixxx runs; mixforge backs up the database first). With stems: true, writes <file>.stem.mp4 for each split track: an NI STEM file (five ALAC tracks: the copy, then its drums, bass, other and vocals at the copy's gain, with NI's `stem` metadata) through ffmpeg (DJFORGE_FFMPEG or ffmpeg on the PATH; skipped without it); a file of that name an export didn't write is never replaced. A cancel stops prep before its next track, and the XML, the Mixxx write and each stem file before they start (a Mixxx write once started runs to its end); the result then has cancelled: true and what was written. Returns {root, rekordbox: path|null|{error}, report: path|null|{error}, mixxx, stems, tracks: [{file, path, title, prep, grid_bars, cues, doubts, found, prep_refused, in_mixxx}], prep, cancelled}",
        super::always,
        export_set
    )]
}

fn export_set(e: &mut crate::Engine, p: &Value) -> crate::Result<Value> {
    export_job(e, p)?.wait()
}

/// What `dj.export_set` was asked for.
struct Request {
    rekordbox: bool,
    mixxx: bool,
    stems: bool,
    out: Option<PathBuf>,
}

fn request(p: &Value) -> crate::Result<Request> {
    let targets: Vec<&str> = match p.get("targets") {
        None | Some(Value::Null) => TARGETS.to_vec(),
        Some(v) => v
            .as_array()
            .and_then(|a| a.iter().map(|t| t.as_str().filter(|t| TARGETS.contains(t))).collect::<Option<Vec<_>>>())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| super::bad(CMD, "`targets` must be a list of \"rekordbox\" and \"mixxx\", not empty"))?,
    };
    let stems = match p.get("stems") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| super::bad(CMD, "`stems` must be true or false"))?,
    };
    let out = match p.get("out") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().filter(|o| !o.is_empty()).ok_or_else(|| super::bad(CMD, "`out` must be a folder path"))?),
    };
    Ok(Request {
        rekordbox: targets.contains(&"rekordbox"),
        mixxx: targets.contains(&"mixxx"),
        stems,
        out: out.map(|o| super::prep::normalise(Path::new(o))),
    })
}

/// `name` made safe as a file name: characters Windows refuses dropped, ends trimmed of spaces
/// and dots, and `_` after a name Windows reserves for a device (`CON`, `nul.txt`); "set" when
/// nothing is left.
fn file_name(name: &str) -> String {
    let kept: String = name.chars().filter(|c| !c.is_control() && !"<>:\"/\\|?*".contains(*c)).collect();
    let kept = kept.trim_matches(|c: char| c == ' ' || c == '.');
    if kept.is_empty() {
        return "set".to_string();
    }
    let (base, rest) = kept.split_at(kept.find('.').unwrap_or(kept.len()));
    let upper = base.trim_end().to_ascii_uppercase();
    let numbered = |p: &str| upper.strip_prefix(p).is_some_and(|n| n.len() == 1 && n.bytes().all(|b| (b'1'..=b'9').contains(&b)));
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&upper.as_str()) || numbered("COM") || numbered("LPT") {
        format!("{base}_{rest}")
    } else {
        kept.to_string()
    }
}

/// Bytes of a file read to tell an earlier export: both marks are in its first lines.
const MARK_HEAD: usize = 4096;

/// Why the export can't replace `path`: it exists and is not an earlier export (its first 4 KB
/// have `mark`: at the start for the report, anywhere for the XML). Only that head is read.
fn foreign(path: &Path, mark: &str) -> Option<String> {
    let ours = || {
        crate::io::read_head(path, MARK_HEAD).is_ok_and(|h| {
            let t = String::from_utf8_lossy(&h);
            if mark == REPORT_HEADER { t.starts_with(mark) } else { t.contains(mark) }
        })
    };
    (crate::io::path_exists(path) && !ours())
        .then(|| format!("{}: exists and is not an earlier export; move it or choose another `out`", path.display()))
}

/// How far an export got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportProgress {
    /// "prep", then "rekordbox", then "mixxx" and "stems" (when asked for), then "done".
    pub stage: &'static str,
    /// Tracks prepped (during "prep").
    pub done: usize,
    pub total: usize,
    /// The file name of the track being prepped now.
    pub current: Option<String>,
}

/// What a running export and the job watching it share: prep's progress and cancel, and the
/// stage.
#[derive(Debug, Default)]
struct Shared {
    prep: super::prep::Shared,
    stage: Mutex<&'static str>,
}

impl Shared {
    fn stage(&self, stage: &'static str) {
        *self.lock_stage() = stage;
    }

    fn lock_stage(&self) -> std::sync::MutexGuard<'_, &'static str> {
        self.stage.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn cancelled(&self) -> bool {
        self.prep.cancel.load(Ordering::Relaxed)
    }

    /// Cancels, under the stage's lock, so a cancel and the start of the Mixxx write
    /// ([`Self::start_mixxx`]) never cross.
    fn cancel(&self) {
        let _stage = self.lock_stage();
        self.prep.cancel.store(true, Ordering::Relaxed);
    }

    /// Enters the "mixxx" stage unless the export was cancelled (false then): once entered,
    /// mixforge writes Mixxx's library to the end, a cancel or not.
    fn start_mixxx(&self) -> bool {
        let mut stage = self.lock_stage();
        if self.cancelled() {
            return false;
        }
        *stage = "mixxx";
        true
    }
}

/// One track of the export: its length, and its stems.
struct Item {
    track: ExportTrack,
    total_secs: f64,
    /// Its stems for a stem file, when stems are asked for and its clip is split; or why they
    /// can't be had.
    stems: Option<std::result::Result<stemfile::Stems, String>>,
}

/// An export whose quick checks passed (its own files are not foreign and may be written): prep's
/// plan (which may read earlier copies to tell prep's own) is made on the export's thread.
struct Export {
    name: String,
    out: PathBuf,
    xml: PathBuf,
    report: PathBuf,
    request: Request,
    /// The session as the export started, for prep's checks and refusals.
    session: Arc<Session>,
    /// The set's tracks, each with its length and stems (when asked for); their copies are
    /// known once prep's plan is made.
    items: Vec<Item>,
    session_files: crate::io::SourceFiles,
    /// What writes the set into Mixxx, when the engine has one.
    mixxx: Option<MixxxRunner>,
    /// Whether it runs confined to automation roots: mixforge writes Mixxx's library and its own
    /// outside them, so a confined export writes no Mixxx.
    confined: bool,
    /// The ffmpeg stem files are made with.
    ffmpeg: String,
}

/// A `dj.export_set` run on its own thread: how far it got, a cancel that stops prep before its
/// next track (nothing more is written then), and its result.
pub struct ExportJob {
    shared: Arc<Shared>,
    out: PathBuf,
    state: JobState,
}

enum JobState {
    Running(std::thread::JoinHandle<crate::Result<Value>>),
    Ended(Option<crate::Result<Value>>),
}

/// Starts the run `dj.export_set` does for `p`: the request, the set and the export's own files
/// are checked before this returns; prep's checks (which may read earlier copies), the folder,
/// prep and the writing happen on another thread, so a refusal of prep's is the job's result.
pub fn export_job(e: &crate::Engine, p: &Value) -> crate::Result<ExportJob> {
    export_job_in(e, p, None, super::prep::mixforge_analysis())
}

/// [`export_job`] into `<root>/<session name>` when `p` gives no `out` (`root` default: prep's
/// [`super::prep::default_root`]), with mixforge's analysis for prep given by `analyse`.
pub fn export_job_in(e: &crate::Engine, p: &Value, root: Option<&Path>, analyse: super::prep::Analyse) -> crate::Result<ExportJob> {
    let request = request(p)?;
    let s = e.session();
    let tracks = collect(s).map_err(|m| super::bad(CMD, m))?;
    let name = file_name(&s.name);
    let out = match &request.out {
        Some(o) => o.clone(),
        None => super::prep::normalise(&match root {
            Some(r) => r.join(&name),
            None => super::prep::default_root()?.join(&name),
        }),
    };
    let (xml, report) = (out.join(format!("{name}.rekordbox.xml")), out.join(format!("{name}.export.txt")));
    // The export's own files, before prep makes the folder: only earlier exports are replaced.
    for (path, mark) in [(&xml, XML_MARK), (&report, REPORT_HEADER)] {
        crate::io::refuse_source_file(s, path)?;
        crate::file_access::check_write(path).map_err(crate::EngineError::Io)?;
        if let Some(why) = foreign(path, mark) {
            return Err(super::bad(CMD, why));
        }
    }
    let sr = s.sample_rate.as_f64();
    let mut items = Vec::with_capacity(tracks.len());
    for track in tracks {
        let total_secs = s.source(track.source).map_or(0.0, |x| x.frames as f64 / sr);
        let stems = track.split.as_ref().filter(|_| request.stems).map(|set| {
            let audio: Option<Vec<_>> = set.sources.iter().map(|id| s.pool.get(*id).cloned()).collect();
            audio
                .and_then(|a| <[_; 4]>::try_from(a).ok())
                .map(|audio| stemfile::Stems { audio, scale_log2: set.scale_log2 })
                .ok_or_else(|| "its stems are not loaded; split the clip again".to_string())
        });
        items.push(Item { track, total_secs, stems });
    }
    let export = Export {
        name,
        out: out.clone(),
        xml,
        report,
        request,
        session: e.session_arc(),
        items,
        session_files: crate::io::SourceFiles::of(s),
        mixxx: e.mixxx_runner().cloned(),
        confined: crate::file_access::current().is_some(),
        ffmpeg: e.ffmpeg().map_or_else(stemfile::ffmpeg, |p| p.to_string_lossy().into_owned()),
    };
    Ok(spawn(export, analyse, out))
}

/// Runs `export` on another thread.
fn spawn(export: Export, analyse: super::prep::Analyse, out: PathBuf) -> ExportJob {
    let shared = Arc::new(Shared::default());
    shared.stage("prep");
    let found = export.items.iter().filter(|i| i.track.found).count();
    shared.prep.update(|p| p.total = found);
    let ended = |r| ExportJob { shared: Arc::clone(&shared), out: out.clone(), state: JobState::Ended(Some(r)) };
    if cfg!(target_arch = "wasm32") {
        return ended(export.run(&*analyse, &shared));
    }
    let export = Arc::new(export);
    let roots = crate::file_access::current();
    let (worker, worker_shared) = (Arc::clone(&export), Arc::clone(&shared));
    let work = move || {
        let run = || worker.run(&*analyse, &worker_shared);
        match &roots {
            Some(r) => crate::file_access::confine(Some(r), run),
            None => crate::file_access::unconfined(run),
        }
    };
    match std::thread::Builder::new().name("export-set".into()).spawn(work) {
        Ok(handle) => ExportJob { shared: Arc::clone(&shared), out: out.clone(), state: JobState::Running(handle) },
        Err(_) => ended(Err(crate::EngineError::Internal(CMD.into(), "no thread to export on".into()))),
    }
}

impl ExportJob {
    /// How far it got.
    pub fn progress(&self) -> ExportProgress {
        let stage = *self.shared.stage.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let p = self.shared.prep.progress.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        ExportProgress { stage, done: p.done, total: p.total, current: p.current }
    }

    /// Stops the export at the next point it can: prep before its next track; the Rekordbox XML
    /// and the Mixxx write before they start (a Mixxx write already started is never stopped:
    /// the stage is "mixxx" until it ends); the stem files before the next. The result says it
    /// was cancelled, and what was written.
    pub fn cancel(&self) {
        self.shared.cancel();
    }

    /// Whether it was cancelled.
    pub fn cancelling(&self) -> bool {
        self.shared.prep.cancel.load(Ordering::Relaxed)
    }

    /// The folder it writes to.
    pub fn out(&self) -> &Path {
        &self.out
    }

    /// The result (what `dj.export_set` returns) once the run ended, given once; `None` before.
    pub fn poll(&mut self) -> Option<crate::Result<Value>> {
        if let JobState::Running(handle) = &self.state
            && !handle.is_finished()
        {
            return None;
        }
        match std::mem::replace(&mut self.state, JobState::Ended(None)) {
            JobState::Running(handle) => Some(joined(handle)),
            JobState::Ended(r) => r,
        }
    }

    /// Waits for the run to end and gives its result.
    pub fn wait(mut self) -> crate::Result<Value> {
        match std::mem::replace(&mut self.state, JobState::Ended(None)) {
            JobState::Running(handle) => joined(handle),
            JobState::Ended(r) => r.unwrap_or_else(|| Err(crate::EngineError::Internal(CMD.into(), "the result was taken already".into()))),
        }
    }
}

fn joined(handle: std::thread::JoinHandle<crate::Result<Value>>) -> crate::Result<Value> {
    handle.join().unwrap_or_else(|p| Err(crate::EngineError::Internal(CMD.into(), crate::panic_message(&*p))))
}

/// How the doubt of a track prep refused to copy starts.
const PREP_REFUSED: &str = "prep refused it: ";

/// One exported track's outcome.
struct Done {
    file: String,
    path: PathBuf,
    prep: String,
    title: String,
    grid_bars: usize,
    cues: usize,
    doubts: Vec<String>,
    /// Prep's gain steps on the exported file (0 when it is the original).
    steps: i32,
    /// Whether its file was found (else it was neither prepped nor read).
    found: bool,
    /// Why prep refused to copy it, when it did.
    prep_refused: Option<String>,
    /// Whether it went to Mixxx (`None` when Mixxx was not asked for).
    in_mixxx: Option<bool>,
}

impl Export {
    /// Preps the files, then writes the Rekordbox XML, the set into Mixxx, and the report. A
    /// cancelled prep ends it with nothing more written.
    fn run(&self, analyse: &dyn Fn(&str, &dyn Fn() -> bool) -> std::result::Result<Value, String>, shared: &Shared) -> crate::Result<Value> {
        // What mixforge's analysis gave for each file analysed (the key, if any), by prep or by
        // the export: a key for Mixxx when the copy has no key tag. After one analysis timed out
        // no other runs, here or in prep, so a stuck sidecar costs one timeout.
        let analysed: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<String>>> = Default::default();
        let timed_out = std::cell::Cell::new(false);
        let recording = |path: &str, stop: &dyn Fn() -> bool| {
            if timed_out.get() {
                return Err("skipped after an earlier timeout".to_string());
            }
            let r = analyse(path, stop);
            if r.as_ref().is_err_and(|m| m.starts_with(super::dj::TIMED_OUT)) {
                timed_out.set(true);
            }
            let key = r.as_ref().ok().and_then(|v| v.get("camelot")).and_then(Value::as_str).map(str::to_string);
            analysed.borrow_mut().insert(crate::io::file_key(Path::new(path)), key);
            r
        };
        let s = &*self.session;
        // Only files that were found are prepped: nothing reads a path that is missing or refused.
        let files: Vec<PathBuf> = self.items.iter().filter(|i| i.track.found).map(|i| super::prep::normalise(&i.track.path)).collect();
        let (plan, refused) = super::prep::Plan::for_files_except(s, &format!("set {}", s.name), files, self.out.clone(), TARGET_LUFS, CEILING)?;
        // Prep's rows and copies are those of the files it kept, in order.
        let (mut file, mut row) = (0, 0);
        let copies: Vec<Result<(PathBuf, usize), String>> = self
            .items
            .iter()
            .map(|item| {
                if !item.track.found {
                    return Err("its file was not found".to_string());
                }
                let at = file;
                file += 1;
                match refused.iter().find(|(i, _)| *i == at) {
                    Some((_, why)) => Err(format!("{PREP_REFUSED}{why}")),
                    None => {
                        let r = row;
                        row += 1;
                        plan.dests().get(r).cloned().map(|d| (d, r)).ok_or_else(|| "prep has no copy for it".to_string())
                    }
                }
            })
            .collect();
        let prep = plan.prep(&recording, &shared.prep)?;
        // The tracks left out of Mixxx since the last it got: they shift mixforge's reading of the
        // next one it gets.
        let mut left_out: Vec<String> = Vec::new();
        let rows = prep.get("tracks").and_then(Value::as_array).cloned().unwrap_or_default();
        if prep.get("cancelled").and_then(Value::as_bool).unwrap_or(false) {
            shared.stage("done");
            return Ok(json!({"root": self.out, "cancelled": true, "rekordbox": null, "report": null, "prep": prep}));
        }
        shared.stage("rekordbox");
        let mut rb = Vec::with_capacity(self.items.len());
        let mut done = Vec::with_capacity(self.items.len());
        let mut mixxx_plan = Vec::with_capacity(self.items.len());
        for (item, item_copy) in self.items.iter().zip(&copies) {
            let t = &item.track;
            let mut doubts = t.doubts.clone();
            let (row, copy) = match item_copy {
                Ok((copy, r)) => (rows.get(*r), Some(copy)),
                Err(_) => (None, None),
            };
            let status = match item_copy {
                Err(_) => "not prepped".to_string(),
                Ok(_) => row.and_then(|r| r.get("status")).and_then(Value::as_str).unwrap_or("failed").to_string(),
            };
            // The prepped copy when prep wrote one; the original (never written) otherwise.
            let path = if let (Some(copy), true) = (copy, matches!(status.as_str(), "ok" | "not_adjusted")) {
                copy.clone()
            } else if let Err(why) = item_copy {
                if t.found {
                    doubts.push(format!("{why}; exported the original"));
                }
                t.path.clone()
            } else {
                let why: Vec<&str> = row
                    .and_then(|r| r.get("doubts"))
                    .and_then(Value::as_array)
                    .map(|d| d.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                doubts.push(format!("prep {status} ({}); exported the original", why.join("; ")));
                t.path.clone()
            };
            // Only a file that was found (or prep's copy) is read: never a missing or refused path.
            let readable = item_copy.is_ok() || t.found;
            let tags_of = |p: &Path| {
                if readable && p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3")) {
                    super::prep::tag_head(p).map(|h| soundcraft_audio_io::mp3tags::read_basic(&h)).unwrap_or_default()
                } else {
                    soundcraft_audio_io::mp3tags::TagBasics::default()
                }
            };
            let tags = tags_of(&path);
            let stem = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let title = tags.title.clone().unwrap_or_else(|| stem.clone());
            let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match rekordbox::file_url(&path) {
                Some(location) => rb.push(rekordbox::RbTrack {
                    path: path.clone(),
                    location,
                    name: title.clone(),
                    artist: tags.artist.clone().unwrap_or_default(),
                    album: tags.album.clone().unwrap_or_default(),
                    size: crate::io::file_len(&path).unwrap_or(0),
                    total_secs: item.total_secs,
                    tonality: tags.key.clone().unwrap_or_default(),
                    grid: t.grid.clone(),
                    cues: t.cues.clone(),
                }),
                None if self.request.rekordbox => doubts.push("its path has no file:// URL; left out of the Rekordbox XML".into()),
                None => {}
            }
            let mut in_mixxx = None;
            if self.request.mixxx {
                let entry = if t.found {
                    // The copy's key tag, else prep's analysis of the file, else the original's tag.
                    let source = super::prep::normalise(&t.path);
                    let earlier = analysed.borrow().get(&crate::io::file_key(&source)).cloned();
                    // Prep analyses MP3s only: another file without a key tag is analysed here.
                    let analysis = match (earlier, &tags.key, source.to_str()) {
                        (Some(k), _, _) => k,
                        (None, None, Some(p)) => {
                            let stop = || shared.prep.cancel.load(Ordering::Relaxed);
                            recording(p, &stop).ok().and_then(|v| v.get("camelot").and_then(Value::as_str).map(str::to_string))
                        }
                        _ => None,
                    };
                    let analysis = analysis.as_deref();
                    let original = if tags.key.is_none() && analysis.is_none() && path != t.path { tags_of(&t.path).key } else { None };
                    mixxx_track(t, &path, &title, tags.artist.as_deref(), &[tags.key.as_deref(), analysis, original.as_deref()])
                } else {
                    Err("its file was not found".to_string())
                };
                match entry {
                    Ok((entry, drift)) => {
                        in_mixxx = Some(true);
                        doubts.extend(drift);
                        if !left_out.is_empty() {
                            let gone = left_out.join(", ");
                            let were = if left_out.len() == 1 { "was" } else { "were" };
                            doubts.push(if mixxx_plan.is_empty() {
                                format!("Mixxx takes it as the set's first track (main cue on its first downbeat, no intro) because {gone} {were} left out of the Mixxx export")
                            } else {
                                format!("its Mixxx intro may be off (it takes the length of the transition before {gone}) because {gone} {were} left out of the Mixxx export")
                            });
                            left_out.clear();
                        }
                        mixxx_plan.push(entry);
                    }
                    Err(why) => {
                        in_mixxx = Some(false);
                        doubts.push(format!("left out of the Mixxx export: {why}"));
                        left_out.push(file.clone());
                    }
                }
            }
            let steps = if path == t.path {
                0
            } else {
                row.and_then(|r| r.get("steps")).and_then(Value::as_i64).and_then(|x| i32::try_from(x).ok()).unwrap_or(0)
            };
            let prep_refused = item_copy.as_ref().err().and_then(|w| w.strip_prefix(PREP_REFUSED)).map(str::to_string);
            done.push(Done {
                file,
                path,
                prep: status,
                title,
                grid_bars: t.grid.len(),
                cues: t.cues.len(),
                doubts,
                steps,
                found: t.found,
                prep_refused,
                in_mixxx,
            });
        }

        // A cancel after prep: nothing more is written, as after a cancel in prep. Each later
        // stage checks again.
        let skipped = || json!({"skipped": "cancelled"});
        if shared.cancelled() {
            shared.stage("done");
            return Ok(json!({"root": self.out, "cancelled": true, "rekordbox": null, "report": null, "prep": prep}));
        }
        let rekordbox = if self.request.rekordbox {
            let xml = rekordbox::rekordbox_xml(&self.name, &rb);
            // Checked again: a file may have taken its place since the export started.
            match foreign(&self.xml, XML_MARK) {
                Some(why) => json!({"error": why}),
                None => match crate::io::replace_file_refusing(&self.session_files, &self.xml, xml.as_bytes()) {
                    Ok(()) => json!(self.xml),
                    Err(err) => json!({"error": err.to_string()}),
                },
            }
        } else {
            Value::Null
        };
        let mixxx = match &self.mixxx {
            _ if !self.request.mixxx => json!({"skipped": "not requested"}),
            _ if self.confined => json!({"skipped": "Mixxx export isn't available to a confined automation session"}),
            None => json!({"skipped": "Mixxx export is not set up"}),
            Some(_) if mixxx_plan.is_empty() => json!({"skipped": "no track could go to Mixxx; see the tracks' doubts"}),
            // Checked under the stage's lock right before mixforge starts: it is never stopped
            // part-way.
            Some(run) if shared.start_mixxx() => mixxx_export(run, &json!({"tracks": mixxx_plan}), &self.name),
            Some(_) => skipped(),
        };
        let stems = if !self.request.stems {
            json!([])
        } else if shared.cancelled() {
            skipped()
        } else {
            shared.stage("stems");
            self.stem_files(&done, &shared.prep.cancel)
        };
        let cancelled = shared.cancelled();

        let text = self.report_text(&rekordbox, &mixxx, &stems, &done, cancelled);
        // What was written stands when the report can't be: the result says why.
        let report = match foreign(&self.report, REPORT_HEADER) {
            Some(why) => json!({"error": why}),
            None => match crate::io::replace_file_refusing(&self.session_files, &self.report, text.as_bytes()) {
                Ok(()) => json!(self.report),
                Err(err) => json!({"error": err.to_string()}),
            },
        };
        shared.stage("done");
        let tracks: Vec<Value> = done
            .iter()
            .map(|d| {
                json!({
                    "file": d.file,
                    "path": d.path,
                    "title": d.title,
                    "prep": d.prep,
                    "grid_bars": d.grid_bars,
                    "cues": d.cues,
                    "doubts": d.doubts,
                    "found": d.found,
                    "prep_refused": d.prep_refused,
                    "in_mixxx": d.in_mixxx,
                })
            })
            .collect();
        Ok(json!({
            "root": self.out,
            "rekordbox": rekordbox,
            "report": report,
            "mixxx": mixxx,
            "stems": stems,
            "tracks": tracks,
            "prep": prep,
            "cancelled": cancelled,
        }))
    }

    /// The stem files of the split tracks (`done` are the tracks' outcomes, in order): for each,
    /// `{file, stem_file}` or `{file, skipped}`; the whole part skipped when ffmpeg is missing or
    /// no track is split. A cancel skips the rest.
    fn stem_files(&self, done: &[Done], cancel: &std::sync::atomic::AtomicBool) -> Value {
        let split: Vec<(&Item, &Done)> = self.items.iter().zip(done).filter(|(i, _)| i.stems.is_some()).collect();
        if split.is_empty() {
            return json!({"skipped": "no track of the set is split"});
        }
        let ffmpeg = &self.ffmpeg;
        if !stemfile::ffmpeg_found(ffmpeg) {
            return json!({"skipped": "ffmpeg not found (install it, or set DJFORGE_FFMPEG to ffmpeg.exe)"});
        }
        let stop = || cancel.load(Ordering::Relaxed);
        stemfile::sweep(&self.out);
        // (stem file, the file it was made from) of each written.
        let mut written: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut out = Vec::with_capacity(split.len());
        for (k, (item, d)) in split.into_iter().enumerate() {
            let name = d.path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let dest = self.out.join(format!("{name}.stem.mp4"));
            let result = match &item.stems {
                _ if stop() => Err("cancelled".to_string()),
                _ if !item.track.found => Err("its file was not found".to_string()),
                _ if written.iter().any(|(_, mix)| crate::io::file_key(mix) == crate::io::file_key(&d.path)) => {
                    Err("written above: the track plays more than once in the set".to_string())
                }
                _ if written.iter().any(|(w, _)| crate::io::file_key(w) == crate::io::file_key(&dest)) => {
                    Err(format!("another track's stem file is named {name}.stem.mp4"))
                }
                Some(Ok(stems)) => {
                    let tmp = self.out.join(format!("{}{}-{k}", stemfile::TMP_PREFIX, std::process::id()));
                    let job = stemfile::Job {
                        ffmpeg,
                        mix: &d.path,
                        stems,
                        gain: 2f64.powf(f64::from(d.steps) / 4.0),
                        dest: &dest,
                        tmp: &tmp,
                        sources: &self.session_files,
                        stop: &stop,
                    };
                    stemfile::write(&job)
                }
                Some(Err(why)) => Err(why.clone()),
                None => Err("it is not split".to_string()),
            };
            out.push(match result {
                Ok((p, doubt)) => {
                    written.push((p.clone(), d.path.clone()));
                    json!({"file": d.file, "stem_file": p, "doubts": doubt.into_iter().collect::<Vec<_>>()})
                }
                Err(why) => json!({"file": d.file, "skipped": why}),
            });
        }
        Value::Array(out)
    }

    /// The export report: what was written for each target, a line per track with its doubts,
    /// and the Rekordbox import steps.
    fn report_text(&self, rekordbox: &Value, mixxx: &Value, stems: &Value, done: &[Done], cancelled: bool) -> String {
        let said = |v: &Value| match v {
            Value::Null => "not requested".to_string(),
            Value::String(p) => p.clone(),
            Value::Array(a) if a.is_empty() => "not requested".to_string(),
            Value::Array(a) => a
                .iter()
                .map(|x| match (x.get("stem_file").and_then(Value::as_str), x.get("skipped").and_then(Value::as_str)) {
                    (Some(p), _) => p.to_string(),
                    (None, why) => format!("{}: skipped ({})", x.get("file").and_then(Value::as_str).unwrap_or("?"), why.unwrap_or("?")),
                })
                .collect::<Vec<_>>()
                .join("; "),
            other if other.get("playlist").is_some_and(Value::is_string) => format!(
                "playlist {} ({} tracks) in Mixxx's library; its database was backed up to {}",
                other.get("playlist").map(Value::to_string).unwrap_or_default(),
                other.get("tracks").and_then(Value::as_u64).unwrap_or(0),
                other.get("backup").and_then(Value::as_str).unwrap_or("?"),
            ),
            other => other.get("skipped").or_else(|| other.get("error")).and_then(Value::as_str).map_or_else(|| other.to_string(), str::to_string),
        };
        let mut out = format!("{REPORT_HEADER}{}\n{}\nfolder: {}\n\n", self.name, super::prep::utc_now(), self.out.display());
        if cancelled {
            out.push_str("The export was cancelled: what was written before the cancel is listed below.\n\n");
        }
        out.push_str(&format!("Rekordbox: {}\nMixxx: {}\nStem files: {}\n\n", said(rekordbox), said(mixxx), said(stems)));
        for (i, d) in done.iter().enumerate() {
            let doubts = if d.doubts.is_empty() { String::new() } else { format!("  | {}", d.doubts.join("; ")) };
            let line = format!("{:>2}  {}  prep {}  {} bars  {} cues{doubts}\n", i + 1, d.file, d.prep, d.grid_bars, d.cues);
            out.extend(line.chars().filter(|c| *c == '\n' || !c.is_control()));
        }
        if rekordbox.is_string() {
            out.push_str(&format!("\nTo use the Rekordbox XML: {REKORDBOX_STEPS}\n"));
        }
        out
    }
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
    use soundcraft_model::{ChannelFormat, Clip, ClipId, TrackKind, WarpMap};

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
            assert!(t.split.is_none());
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
    fn folder(name: &str) -> Folder {
        let dir = std::env::temp_dir().join(format!("sc-export-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Folder(dir)
    }

    /// A test's folder, removed when the test ends, failed or not.
    pub(super) struct Folder(pub PathBuf);

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl std::ops::Deref for Folder {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
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
    }

    /// The file a source was imported from is a path the session names, so it goes through
    /// `stored_path` like every other: one on another computer's share, a relative one in a
    /// session never saved (not taken from the working directory) and a device are refused with
    /// a doubt, are not the track's file, and nothing reads them.
    #[test]
    fn origins_the_session_may_not_open_are_refused_with_a_doubt() {
        let (mut e, clips) = set();
        let origins = [r"\\attacker\share\a.mp3", "a.mp3", r"\\.\PhysicalDrive0"];
        let s = e.session_mut();
        assert!(s.folder.is_none());
        for (clip, origin) in clips.iter().zip(origins) {
            let id = s.find_clip(*clip).unwrap().1.source().unwrap();
            s.sources.iter_mut().find(|x| x.id == id).unwrap().origin = Some(origin.into());
        }
        for (t, origin) in collect(e.session()).unwrap().iter().zip(origins) {
            let doubt = format!("the file it was imported from ({origin}) is not opened");
            assert!(t.doubts.iter().any(|d| d.starts_with(&doubt)), "{:?}", t.doubts);
            assert!(!t.found && !t.path.to_string_lossy().contains(origin), "{}", t.path.display());
        }
    }

    /// The audio of a movie has no audio file of its own: a movie origin is passed over quietly.
    #[test]
    fn a_movie_is_not_exported_as_a_track() {
        let (mut e, [a, _, _]) = set();
        let s = e.session_mut();
        let id = s.find_clip(a).unwrap().1.source().unwrap();
        s.sources.iter_mut().find(|x| x.id == id).unwrap().origin = Some(r"C:\films\movie.mp4".into());
        let t = &collect(e.session()).unwrap()[0];
        assert!(!t.path.to_string_lossy().contains("movie") && t.doubts.iter().all(|d| !d.contains("movie")), "{:?}", t);
    }

    #[test]
    fn names_windows_reserves_get_an_underscore() {
        for (name, want) in [
            ("CON", "CON_"),
            ("nul.txt", "nul_.txt"),
            ("Com1", "Com1_"),
            ("LPT9.set", "LPT9_.set"),
            ("conin$", "conin$_"),
            ("CONOUT$.x", "CONOUT$_.x"),
            ("COM10", "COM10"),
            ("Console", "Console"),
            ("a<b>", "ab"),
            (" . ", "set"),
        ] {
            assert_eq!(file_name(name), want, "{name}");
        }
    }

    #[test]
    fn the_sidecar_s_result_is_its_last_line() {
        let v = sidecar_result("backing up the library...\n{\"playlist\": \"mixforge · s\"}\n\n").unwrap();
        assert_eq!(v["playlist"], "mixforge · s");
        assert_eq!(sidecar_result("note\n{\"error\": \"Close Mixxx first: ...\"}").unwrap_err(), "Close Mixxx first: ...");
        assert!(sidecar_result("{\"a\": 1}\nnot json").unwrap_err().contains("not JSON"));
        assert!(sidecar_result("[1]").unwrap_err().contains("not an object"));
    }

    #[test]
    fn keys_become_mixforge_s_names() {
        // mixforge's camelot_to_key: 8A -> Am, 8B -> C; sharps only.
        for (k, want) in
            [("8A", "Am"), ("8B", "C"), ("11A", "F#m"), ("1B", "B"), ("12B", "E"), ("3A", "A#m"), ("5B", "D#"), ("Bbm", "A#m"), ("F", "F")]
        {
            assert_eq!(mixforge_key(k).as_deref(), Some(want), "{k}");
        }
        // Every Camelot code round-trips through prep's reading of musical notation.
        for n in 1..=12 {
            for l in ['A', 'B'] {
                let name = mixforge_key(&format!("{n}{l}")).unwrap();
                assert_eq!(crate::cmd::prep::to_camelot(&name), Some(format!("{n}{l}")), "{name}");
            }
        }
        assert_eq!(mixforge_key(""), None);
        assert_eq!(mixforge_key("13A"), None);
    }

    #[test]
    fn the_plan_entry_is_the_track_in_file_seconds() {
        let (e, _) = set();
        let set = collect(e.session()).unwrap();
        // B: 1.92 s bars (125 bpm); in on 25, out on 49 for 12 bars (quick: 8 build + 4 out).
        let (v, drift) = mixxx_track(&set[1], Path::new("/x/b.mp3"), "B", None, &[None, Some("8A")]).unwrap();
        assert_eq!(drift, None);
        assert!((v["bpm"].as_f64().unwrap() - 125.0).abs() < 1e-9, "{v}");
        assert!(close(v["first_downbeat"].as_f64().unwrap(), FIRST) && close(v["cue_in"].as_f64().unwrap(), bar_secs(0.0)), "{v}");
        assert!(close(v["out_start"].as_f64().unwrap(), bar_secs(24.0)), "{v}");
        assert_eq!((&v["transition_bars"], &v["key"], &v["artist"]), (&json!(12), &json!("Am"), &json!("")));
        // A track without a key, or a grid of one bar, can't go.
        assert!(mixxx_track(&set[1], Path::new("/x/b.mp3"), "B", None, &[None, Some("H"), None]).unwrap_err().contains("key is not known"));
        // The first key that is one is taken.
        let (v, _) = mixxx_track(&set[1], Path::new("/x/b.mp3"), "B", None, &[Some("nope"), Some("4B"), Some("8A")]).unwrap();
        assert_eq!(v["key"], "G#");
        // Bars that vary by more than 5 ms: Mixxx's one tempo drifts.
        let mut wobbly = set[1].clone();
        wobbly.grid[3] += 0.01;
        let (_, drift) = mixxx_track(&wobbly, Path::new("/x/b.mp3"), "B", None, &[Some("8A")]).unwrap();
        assert!(drift.as_deref().is_some_and(|d| d.contains("vary by 20 ms") && d.contains("drifts")), "{drift:?}");
        let mut short = set[2].clone();
        short.grid.truncate(1);
        assert!(mixxx_track(&short, Path::new("/x/c.mp3"), "C", None, &[Some("8A")]).unwrap_err().contains("fewer than two bars"));
    }

    /// No test anywhere in the workspace runs the real Mixxx export: it writes Marty's Mixxx
    /// library. Only the apps' entry points call `mixforge_mixxx_runner`; tests give a fake.
    #[test]
    fn no_test_reaches_the_real_mixxx_library() {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() && !matches!(name.as_str(), "target" | ".git" | "node_modules") {
                    walk(&path, out);
                } else if name.ends_with(".rs") {
                    out.push(path);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
        let mut files = Vec::new();
        for top in ["crates", "apps"] {
            walk(&root.join(top), &mut files);
        }
        assert!(files.len() > 50, "{}", files.len());
        let forbidden = ["mixforge_mixxx_runner(", "run_mixforge_mixxx("];
        let mut offenders = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let in_tests_dir = f.components().any(|c| c.as_os_str() == "tests");
            // Test code: a tests folder, or from the first `#[cfg(test)]` at the start of a line (a
            // test module; an indented one marks a field or item inside code).
            let test_code = if in_tests_dir {
                text.as_str()
            } else {
                text.split_once(
                    "
#[cfg(test)]",
                )
                .map_or("", |(_, t)| t)
            };
            // The apps set the real runner, so their tests (which run the real binaries) must not
            // export a set at all: "dj.export_set" asks for Mixxx by default.
            let in_apps = f.strip_prefix(&root).is_ok_and(|r| r.starts_with("apps"));
            for word in forbidden.iter().chain(in_apps.then_some(&"dj.export_set")) {
                if test_code.lines().any(|l| l.contains(word) && !l.trim_start().starts_with("//") && !l.contains("let forbidden")) {
                    offenders.push(format!("{}: {word}", f.display()));
                }
            }
        }
        assert!(offenders.is_empty(), "tests must use a fake Mixxx runner, and the apps' tests must not export sets: {offenders:#?}");
    }
}
