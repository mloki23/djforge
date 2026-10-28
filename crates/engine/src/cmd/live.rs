//! Session view commands: the Live columns and what their clip slots hold. Launching is not here:
//! what plays is live state, never part of the document.

use super::dj::{Beats, grid, grid_of};
use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{
    ChannelFormat, LIVE_COLUMNS, LIVE_SLOTS, LiveColumn, MAX_LIVE_COLUMNS, MAX_SLOT_BEATS, Quantise, SLOT_GAIN_DB, Session, Slot, SlotMode, TrackKind,
};

const SETUP: &str = "live.setup";
const FROM_SELECTION: &str = "live.slot_from_selection";
const FROM_FILE: &str = "live.slot_from_file";
const SET: &str = "live.slot_set";
const CLEAR: &str = "live.slot_clear";
const QUANTISE: &str = "live.quantise";

/// Why a slot command is refused before the Session view is set up.
const NO_COLUMNS: &str = "the session has no Live columns: open View › Session first (live.setup)";

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "live.setup",
            "Set Up Session View",
            [],
            None,
            "{columns?: 4 (1..=16)} — makes the Live columns: one stereo audio track each (\"Live 1\"…) with 8 empty clip slots. Idempotent: existing columns stay, a column whose track was deleted gets a new one. Returns {columns: [track ids], created}",
            always,
            setup
        ),
        cmd!(
            "live.slot_from_selection",
            "Add Selection to Slot",
            [],
            None,
            "{column (0-based), slot? (0-based; default: the column's first empty slot), track? (default: the selected tracks)} — the time selection over one beat-locked clip becomes a looping slot: its source, beat grid, stems and stem mask, the selection in the clip's own track beats snapped to whole 4-beat bars (at least 1) inside what the clip plays. Needs 4/4 throughout. Returns {column, slot, start_beat, length_beats, bars}",
            has_live,
            from_selection
        ),
        cmd!(
            "live.slot_from_file",
            "Load File into Slot",
            [],
            None,
            "{column (0-based), path, slot? (0-based; default: the column's first empty slot), bpm? + first_beat? (seconds of a downbeat; default: mixforge's analysis when the app has it, else the file's BPM tag with the first downbeat at 0)} — imports the audio file as the slot's source: a whole-bar loop from its first downbeat when it has a beat grid and at least a bar after it, else a one-shot at its own rate. Needs 4/4 throughout. Returns {column, slot, source, mode, grid: given|mixforge|tag|null, length_beats, analysis_error (why mixforge gave no grid), grid_error (why a grid was not used)}",
            has_live,
            from_file
        ),
        cmd!(
            "live.slot_set",
            "Edit Slot",
            [],
            None,
            "{column, slot (0-based), name?, color?: [r, g, b], mode?: loop|one_shot, length_bars? (1..=256 whole 4-beat bars; needs 4/4 throughout), gain_db? (-144..=24)} — edits a filled slot; a slot without a beat grid plays only as a one-shot",
            has_live,
            set
        ),
        cmd!(
            "live.slot_clear",
            "Clear Slot",
            [],
            None,
            "{column, slot (0-based)} — empties a slot; its audio file stays in the Clip List",
            has_live,
            clear
        ),
        cmd!(
            "live.quantise",
            "Launch Quantise",
            [],
            None,
            "{value: none | 1/4 | 1/2 | 1 | 2 | 4 | 8 (bars)} — where launches in the Session view wait for: at once, the next quarter or half note, or the next bar line of every 1, 2, 4 or 8 bars",
            always,
            quantise
        ),
    ]
}

fn has_live(e: &Engine) -> std::result::Result<(), String> {
    if e.session().live.columns.is_empty() { Err(NO_COLUMNS.into()) } else { Ok(()) }
}

fn setup(e: &mut Engine, p: &Value) -> Result<Value> {
    let want = match p.get("columns") {
        None | Some(Value::Null) => LIVE_COLUMNS,
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| (1..=MAX_LIVE_COLUMNS).contains(n))
            .ok_or_else(|| bad(SETUP, format!("`columns` must be 1..={MAX_LIVE_COLUMNS}")))?,
    };
    let s = e.session();
    let lost = s.live.lost_columns(&s.tracks);
    let more = want.saturating_sub(s.live.columns.len());
    let mut created = 0usize;
    if !lost.is_empty() || more > 0 {
        let s = e.session_mut();
        let mut make = |s: &mut Session, i: usize| {
            created += 1;
            s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(&format!("Live {}", i + 1)))
        };
        // A column whose track was lost keeps its slots on a new track.
        for i in lost {
            let track = make(s, i);
            if let Some(c) = s.live.columns.get_mut(i) {
                c.track = track;
            }
        }
        for i in s.live.columns.len()..want {
            let track = make(s, i);
            s.live.columns.push(LiveColumn::new(track));
        }
    }
    let columns: Vec<u64> = e.session().live.columns.iter().map(|c| c.track.0).collect();
    Ok(json!({"columns": columns, "created": created}))
}

/// The 0-based column `p` names, which must exist.
fn column_param(e: &Engine, cmd: &str, p: &Value) -> Result<usize> {
    let n = e.session().live.columns.len();
    if n == 0 {
        return Err(bad(cmd, NO_COLUMNS));
    }
    let v = p.get("column").ok_or_else(|| bad(cmd, "`column` required (0-based)"))?;
    v.as_u64().and_then(|c| usize::try_from(c).ok()).filter(|c| *c < n).ok_or_else(|| bad(cmd, format!("`column` must be 0..{n}")))
}

/// The 0-based slot `p` names, if it names one.
fn slot_param(cmd: &str, p: &Value) -> Result<Option<usize>> {
    match p.get("slot") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|r| usize::try_from(r).ok())
            .filter(|r| *r < LIVE_SLOTS)
            .map(Some)
            .ok_or_else(|| bad(cmd, format!("`slot` must be 0..{LIVE_SLOTS}"))),
    }
}

/// The slot a new slot goes into: the one `p` names, else the column's first empty one.
fn target(e: &Engine, cmd: &str, p: &Value, column: usize) -> Result<usize> {
    if let Some(r) = slot_param(cmd, p)? {
        return Ok(r);
    }
    let col = e.session().live.columns.get(column).ok_or_else(|| bad(cmd, NO_COLUMNS))?;
    col.next_empty().ok_or_else(|| bad(cmd, format!("Live column {} is full: pass `slot` to replace one, or clear one", column + 1)))
}

/// Beats in a slot's bar. Slots count bars of four beats, so the session must be in 4/4
/// throughout (as for set exports): see [`four_four`].
const BAR: f64 = 4.0;

/// Refuses a session whose meter is not 4/4 throughout, where slot bars would not be bars.
fn four_four(cmd: &str, s: &Session) -> Result<()> {
    match s.tempo.meters().iter().find(|m| (m.numerator, m.denominator) != (4, 4)) {
        Some(m) => Err(bad(
            cmd,
            format!(
                "bar {} is in {}/{}: Session view slots need 4/4 throughout; set the meter to 4/4 first",
                s.tempo.bar_beat_at_tick(m.tick).bar,
                m.numerator,
                m.denominator
            ),
        )),
        None => Ok(()),
    }
}

/// The first bar line at or after track beat `beat` (allowing for rounding).
fn bar_at_or_after(beat: f64) -> f64 {
    (beat / BAR - 1e-6).ceil() * BAR
}

/// The last bar line at or before track beat `beat` (allowing for rounding).
fn bar_at_or_before(beat: f64) -> f64 {
    (beat / BAR + 1e-6).floor() * BAR
}

/// Puts `slot` at `column`, `row`.
fn put(e: &mut Engine, column: usize, row: usize, slot: Slot) {
    if let Some(cell) = e.session_mut().live.columns.get_mut(column).and_then(|c| c.slots.get_mut(row)) {
        *cell = Some(slot);
    }
}

/// A slot from the selection `sel` over the one beat-locked clip on `tracks`, and its bars.
fn slot_from_selection(s: &Session, tracks: &[TrackId], sel: Range) -> std::result::Result<(Slot, f64), String> {
    if sel.is_empty() {
        return Err("make a time selection over a beat-locked clip first".into());
    }
    let clips: Vec<(&soundcraft_model::Track, &soundcraft_model::Clip)> = s
        .tracks
        .iter()
        .filter(|t| tracks.contains(&t.id))
        .flat_map(|t| t.clips().iter().filter(|c| c.is_audio() && c.range().overlaps(&sel)).map(move |c| (t, c)))
        .collect();
    let (track, clip) = match clips.as_slice() {
        [one] => *one,
        [] => return Err("the selection covers no audio clip on the selected tracks".into()),
        many => return Err(format!("the selection covers {} clips: select part of one beat-locked clip", many.len())),
    };
    let (Some(source), Some(warp)) = (clip.source(), clip.warp.as_ref().filter(|w| w.is_usable())) else {
        return Err(format!("`{}` is not beat-locked: load it with Load Track Beat-Locked (dj.load_warped)", clip.name));
    };
    // The part of the clip selected, in the clip's own track beats, snapped to its bars and kept
    // inside what the clip plays: from its first downbeat (beat 0) and its start, to its end and
    // the end of its audio.
    let (a, b) = (sel.start.max(clip.start), sel.end.min(clip.end()));
    let at = |t: Samples| warp.track_beat_at(&s.tempo, s.sample_rate, clip.start, t);
    let frames = s.source(source).map_or(0.0, |x| x.frames as f64);
    let (lo, hi) = (at(clip.start).max(0.0), at(clip.end()).min(warp.beat_at_source(frames)));
    let snap = |beat: f64| (beat / BAR).round() * BAR;
    let (start, end) = (snap(at(a)).max(bar_at_or_after(lo)), snap(at(b)).min(bar_at_or_before(hi)));
    let length = end - start;
    if !(start.is_finite() && length.is_finite()) || length < BAR {
        return Err("the selection is under 1 bar: select at least one whole bar of the clip".into());
    }
    if length > MAX_SLOT_BEATS {
        return Err(format!("the selection is longer than a slot's {MAX_SLOT_BEATS} beats"));
    }
    let mut warp = warp.clone();
    warp.lock = None;
    let slot = Slot {
        name: clip.name.clone(),
        color: clip.color.unwrap_or(track.color),
        source,
        warp: Some(warp),
        start_beat: start,
        length_beats: length,
        stems: clip.stems.clone(),
        stem_mask: clip.stem_mask,
        mode: SlotMode::Loop,
        gain_db: clip.gain_db,
    };
    Ok((slot, length / BAR))
}

fn from_selection(e: &mut Engine, p: &Value) -> Result<Value> {
    let column = column_param(e, FROM_SELECTION, p)?;
    let row = target(e, FROM_SELECTION, p, column)?;
    let tracks = match track_param(e, FROM_SELECTION, p, "track")? {
        Some(t) => vec![t],
        None => e.session().edit.selected_tracks.clone(),
    };
    let s = e.session();
    four_four(FROM_SELECTION, s)?;
    let (slot, bars) = slot_from_selection(s, &tracks, s.edit.selection).map_err(|why| bad(FROM_SELECTION, why))?;
    let out = json!({"column": column, "slot": row, "start_beat": slot.start_beat, "length_beats": slot.length_beats, "bars": bars});
    put(e, column, row, slot);
    Ok(out)
}

fn from_file(e: &mut Engine, p: &Value) -> Result<Value> {
    let column = column_param(e, FROM_FILE, p)?;
    let path = str_param(p, "path").ok_or_else(|| bad(FROM_FILE, "`path` required"))?.to_string();
    let row = target(e, FROM_FILE, p, column)?;
    let bpm = match p.get("bpm") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64().filter(|b| b.is_finite() && (20.0..=400.0).contains(b)).ok_or_else(|| bad(FROM_FILE, "`bpm` must be a number 20..=400"))?,
        ),
    };
    let first_beat = match (p.get("first_beat"), bpm) {
        (None | Some(Value::Null), _) => 0.0,
        (Some(_), None) => return Err(bad(FROM_FILE, "`first_beat` needs a `bpm`")),
        (Some(v), Some(_)) => {
            v.as_f64().filter(|f| f.is_finite() && (0.0..=600.0).contains(f)).ok_or_else(|| bad(FROM_FILE, "`first_beat` must be 0..=600 seconds"))?
        }
    };
    four_four(FROM_FILE, e.session())?;
    // The file is imported first, so a file that won't read or decode is refused before mixforge
    // runs; a failure after this is rolled back with the import.
    let bytes = crate::io::read_file(&path)?;
    let name = std::path::Path::new(&path).file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string();
    let (source, frames, _) = crate::io::import_source_bytes(e, &name, &bytes, Some(&path))?;
    // The beat grid: given, else mixforge's analysis (when the app set it up), else the file's
    // BPM tag with the first downbeat at the file start, else none (a one-shot).
    let mut analysis_error = None;
    let (beats, how) = match bpm {
        Some(bpm) => (Some(Beats::Constant { bpm, first: first_beat }), Some("given")),
        None => match analysed(e, &path) {
            Ok(b) => (Some(b), Some("mixforge")),
            Err(why) => {
                analysis_error = Some(why);
                match tagged_bpm(&path) {
                    Some(bpm) => (Some(Beats::Constant { bpm, first: 0.0 }), Some("tag")),
                    None => (None, None),
                }
            }
        },
    };
    let s = e.session();
    let label = s.source(source).map(|x| x.name.clone()).unwrap_or_default();
    let color = s.live.column_track(column, &s.tracks).and_then(|t| s.track(t)).map_or([128, 128, 128], |t| t.color);
    let mut slot = Slot::one_shot(label, color, source);
    let rate = s.sample_rate.as_f64();
    let mut grid_error = None;
    if let Some(b) = beats {
        // From the first downbeat, whole bars of the file; a file with less than a bar from its
        // first downbeat (or a downbeat past its end) stays a one-shot.
        match grid(&b.secs(frames, rate), rate, frames, BAR as i64, Some(0.0)) {
            Some((warp, track_beats)) if track_beats >= BAR => {
                slot.length_beats = bar_at_or_before(track_beats).clamp(BAR, MAX_SLOT_BEATS);
                slot.warp = Some(warp);
                slot.mode = SlotMode::Loop;
            }
            Some(_) => grid_error = Some("the file holds less than a bar from its first downbeat, so it plays as a one-shot"),
            None => grid_error = Some("the beat grid is unusable, so it plays as a one-shot"),
        }
    }
    let how = how.filter(|_| slot.warp.is_some());
    let out = json!({"column": column, "slot": row, "source": source, "mode": slot.mode.id(), "grid": how, "length_beats": slot.length_beats,
        "analysis_error": analysis_error, "grid_error": grid_error});
    put(e, column, row, slot);
    Ok(out)
}

/// The beat grid mixforge's analysis gives the file at `path`, or why there is none. The command
/// runs it to the end: it has no cancel of its own, and the sidecar has its time limit.
fn analysed(e: &Engine, path: &str) -> std::result::Result<Beats, String> {
    let analyse = e.analyse().ok_or("no mixforge analysis is set up")?;
    let a = analyse(path, &|| false)?;
    let (bpm, first) = grid_of(&a).ok_or("mixforge's analysis has no usable bpm/first_downbeat")?;
    Ok(Beats::Constant { bpm, first })
}

/// The BPM in the ID3 tag of the MP3 at `path`, when it is a usable tempo.
fn tagged_bpm(path: &str) -> Option<f64> {
    let p = std::path::Path::new(path);
    if !p.extension().and_then(|x| x.to_str()).is_some_and(|x| x.eq_ignore_ascii_case("mp3")) {
        return None;
    }
    let head = super::prep::tag_head(p).ok()?;
    soundcraft_audio_io::mp3tags::read_basic(&head).bpm.filter(|b| b.is_finite() && (20.0..=400.0).contains(b))
}

/// The filled slot `p` names (`column` and `slot` both required).
fn filled(e: &Engine, cmd: &str, p: &Value) -> Result<(usize, usize, Slot)> {
    let column = column_param(e, cmd, p)?;
    let row = slot_param(cmd, p)?.ok_or_else(|| bad(cmd, "`slot` required (0-based)"))?;
    let slot =
        e.session().live.slot(column, row).cloned().ok_or_else(|| bad(cmd, format!("slot {} of Live column {} is empty", row + 1, column + 1)))?;
    Ok((column, row, slot))
}

fn set(e: &mut Engine, p: &Value) -> Result<Value> {
    let (column, row, old) = filled(e, SET, p)?;
    if !["name", "color", "mode", "length_bars", "gain_db"].iter().any(|k| p.get(k).is_some()) {
        return Err(bad(SET, "nothing to set: pass name, color, mode, length_bars or gain_db"));
    }
    let mut slot = old.clone();
    if let Some(v) = p.get("name") {
        let mut name = v.as_str().ok_or_else(|| bad(SET, "`name` must be text"))?.to_string();
        soundcraft_model::truncate_chars(&mut name, soundcraft_model::MAX_NAME_CHARS);
        slot.name = name;
    }
    if let Some(v) = p.get("color") {
        let rgb: Option<Vec<u8>> = v.as_array().and_then(|a| a.iter().map(|c| c.as_u64().and_then(|c| u8::try_from(c).ok())).collect());
        slot.color = rgb.and_then(|c| <[u8; 3]>::try_from(c).ok()).ok_or_else(|| bad(SET, "`color` must be [r, g, b], each 0..=255"))?;
    }
    if let Some(v) = p.get("mode") {
        slot.mode = v.as_str().and_then(SlotMode::from_id).ok_or_else(|| bad(SET, "`mode` must be loop or one_shot"))?;
        if slot.mode == SlotMode::Loop && slot.warp.is_none() {
            return Err(bad(SET, "this slot has no beat grid, so it plays only as a one-shot: load the file again with a `bpm`"));
        }
    }
    if let Some(v) = p.get("length_bars") {
        four_four(SET, e.session())?;
        let most = (MAX_SLOT_BEATS / BAR).floor();
        let bars = v
            .as_f64()
            .filter(|b| b.fract() == 0.0 && (1.0..=most).contains(b))
            .ok_or_else(|| bad(SET, format!("`length_bars` must be a whole number 1..={most}")))?;
        slot.length_beats = bars * BAR;
    }
    if let Some(v) = p.get("gain_db") {
        let (lo, hi) = SLOT_GAIN_DB;
        slot.gain_db =
            v.as_f64().map(|g| g as f32).filter(|g| (lo..=hi).contains(g)).ok_or_else(|| bad(SET, format!("`gain_db` must be {lo}..={hi}")))?;
    }
    let out = json!({"column": column, "slot": row, "name": slot.name, "color": slot.color, "mode": slot.mode.id(), "length_beats": slot.length_beats, "gain_db": slot.gain_db});
    if slot != old {
        put(e, column, row, slot);
    }
    Ok(out)
}

fn clear(e: &mut Engine, p: &Value) -> Result<Value> {
    let (column, row, _) = filled(e, CLEAR, p)?;
    if let Some(cell) = e.session_mut().live.columns.get_mut(column).and_then(|c| c.slots.get_mut(row)) {
        *cell = None;
    }
    Ok(json!({"column": column, "slot": row}))
}

fn quantise(e: &mut Engine, p: &Value) -> Result<Value> {
    const WANT: &str = "`value` must be none, 1/4, 1/2, 1, 2, 4 or 8 (bars)";
    let v = p.get("value").ok_or_else(|| bad(QUANTISE, WANT))?;
    let q = match (v.as_str(), v.as_f64()) {
        (Some(id), _) => Quantise::from_id(id),
        (_, Some(n)) => [(0.0, Quantise::None), (0.25, Quantise::Quarter), (0.5, Quantise::Half)]
            .into_iter()
            .find(|(x, _)| *x == n)
            .map(|(_, q)| q)
            .or_else(|| (n.fract() == 0.0 && (1.0..=8.0).contains(&n)).then_some(Quantise::Bar(n as u8)).filter(|q| q.normalised() == *q)),
        _ => None,
    }
    .ok_or_else(|| bad(QUANTISE, WANT))?;
    if e.session().live.quantise != q {
        e.session_mut().live.quantise = q;
    }
    Ok(json!({"quantise": q.id()}))
}
