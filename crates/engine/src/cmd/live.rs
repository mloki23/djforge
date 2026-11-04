//! Session view commands: the Live columns and what their clip slots hold. Launching is not here:
//! what plays is live state, never part of the document.

use super::dj::{Beats, analysed_key, grid, grid_of};
use super::*;
use crate::{TransportRequest, cmd};
use serde_json::json;
use soundcraft_model::{
    ChannelFormat, ClipContent, LIVE_COLUMNS, LIVE_SLOTS, LiveColumn, MAX_LIVE_COLUMNS, MAX_SLOT_BARS, MAX_SLOT_BEATS, MidiSlot, Quantise,
    SLOT_GAIN_DB, Session, Slot, SlotContent, SlotKind, SlotMode, TrackKind,
};

const SETUP: &str = "live.setup";
const ADD_COLUMN: &str = "live.add_column";
const FROM_CLIP: &str = "live.slot_from_clip";
const TO_CLIP: &str = "live.slot_to_clip";
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
            "{columns?: 4 (1..=16)} — makes the Live columns: one stereo audio track each (\"Live 1\"…) with 8 empty clip slots. Idempotent: existing columns stay, a column whose track was deleted gets a new one (an instrument track when it holds MIDI slots, its audio slots emptied: the trance_kit preset when every note is a General MIDI drum on channel 10, else rolling_bass). Returns {columns: [track ids], created, emptied: [{column, slot, name}]}",
            always,
            setup
        ),
        cmd!(
            "live.add_column",
            "Add Live Column",
            [],
            None,
            "{kind?: audio|instrument (default audio), preset?: id (prod.presets; instrument columns, default trance_kit)} — adds a Live column after the others (\"Live N\", at most 16): a stereo audio track whose slots hold audio, or an instrument track with the preset's built-in instrument whose slots hold MIDI. One undo step. Returns {column, track, kind, preset}",
            always,
            add_column
        ),
        cmd!(
            "live.slot_from_clip",
            "Copy MIDI Clip to Slot",
            [],
            None,
            "{column (0-based; an instrument column), slot? (0-based; default: the column's first empty slot), clip? (id; default: the one selected MIDI clip), replace?: bool (default false)} — copies the MIDI clip's notes into a looping MIDI slot, its length snapped to whole 4/4 bars (1..=256); notes starting past that end are left out (those running past it end there); controllers, the notes' MIDI real-time properties and the track's MIDI offset are not carried over (the slot plays the notes as they are stored). Refuses an occupied slot unless `replace`. Needs 4/4 throughout. Returns {column, slot, clip, name, bars, length_beats, notes, controllers_dropped}",
            has_live,
            from_clip
        ),
        cmd!(
            "live.slot_to_clip",
            "Copy MIDI Slot to Arrangement",
            [],
            None,
            "{column, slot (0-based; a MIDI slot), at? (position; default: the playhead), replace?: bool (default false)} — the mirror of live.slot_from_clip: the slot's notes (all of them, as they are) become a MIDI clip named and coloured after the slot on the column's track, from the start of the bar holding `at` for the slot's length, to edit in the MIDI Editor. Refuses when that overlaps a clip on the track unless `replace` (which clears what is under it). Needs 4/4 throughout. One undo step. Returns {clip, track, name, start, end, bars, notes}",
            has_live,
            to_clip
        ),
        cmd!(
            "live.slot_from_selection",
            "Add Selection to Slot",
            [],
            None,
            "{column (0-based), slot? (0-based; default: the column's first empty slot), track? (default: the selected tracks), replace?: bool (default false)} — the time selection over one beat-locked clip becomes a looping slot: its source, beat grid, stems and stem mask, the selection in the clip's own track beats snapped to whole 4-beat bars (at least 1) inside what the clip plays. Refuses an occupied `slot` unless `replace`. Needs 4/4 throughout. Returns {column, slot, start_beat, length_beats, bars}",
            has_live,
            from_selection
        ),
        cmd!(
            "live.slot_from_file",
            "Load File into Slot",
            [],
            None,
            "{column (0-based), path, slot? (0-based; default: the column's first empty slot), replace?: bool (default false), bpm? + first_beat? (seconds of a downbeat; default: mixforge's analysis when the app has it, else the file's BPM tag with the first downbeat at 0)} — imports the audio file as the slot's source: a whole-bar loop from its first downbeat when it has a beat grid and at least a bar after it, else a one-shot at its own rate. Refuses an occupied `slot` unless `replace`. Needs 4/4 throughout. Returns {column, slot, source, mode, grid: given|mixforge|tag|null, length_beats, analysis_error (why mixforge gave no grid), grid_error (why a grid was not used)}",
            has_live,
            from_file
        ),
        cmd!(
            "live.slot_set",
            "Edit Slot",
            [],
            None,
            "{column, slot (0-based), name?, color?: [r, g, b], mode?: loop|one_shot, length_bars? (1..=256 whole 4-beat bars; needs 4/4 throughout), gain_db? (-144..=24), replace?: bool (default false)} — edits a filled slot, audio or MIDI; a slot without a beat grid plays only as a one-shot, and a MIDI slot only loops (its gain scales its velocities). Shortening a MIDI slot so notes start past its new end is refused unless `replace`, which drops them (and cuts notes running past the end there), as a saved session reloads it. Returns {column, slot, kind, name, color, mode, length_beats, gain_db, notes_dropped}",
            has_live,
            set
        ),
        cmd!(
            "live.slot_clear",
            "Clear Slot",
            [],
            None,
            "{column, slot (0-based)} — empties a slot, audio or MIDI; an audio slot's file stays in the Clip List",
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
        cmd!(
            noundo "live.launch",
            "Launch Slot",
            [],
            None,
            "{column, slot (0-based)} — launches a filled slot at the next launch-quantise boundary (at least 50 ms ahead), stopping what its column plays there; with the transport stopped it starts playback from the edit insertion and launches on the first boundary from there. Needs the app's audio engine (refused headless) and 4/4 throughout",
            has_live,
            launch
        ),
        cmd!(noundo "live.stop", "Stop Column", [], None, "{column (0-based)} — stops the column at the next launch-quantise boundary", has_live, stop),
        cmd!(noundo "live.stop_all", "Stop All Slots", [], None, "{} — stops every column at the next launch-quantise boundary", has_live, stop_all),
        cmd!(
            noundo "live.scene",
            "Launch Scene",
            [],
            None,
            "{row (0-based)} — launches row `row` of every column that has a slot there, together on the next boundary; columns without one keep playing",
            has_live,
            scene
        ),
        cmd!(
            query "live.state",
            "Session View State",
            [],
            None,
            "{} — per column: its track, its kind (audio|instrument, null when its track is lost), each slot's kind (audio|midi) and name, the slot playing (with where it is in its loop, in beats) and the launch or stop queued, as the audio engine last reported; plus the transport and the launch quantise",
            always,
            state
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
    let mut emptied = Vec::new();
    let mut names = Vec::new();
    if !lost.is_empty() || more > 0 {
        let s = e.session_mut();
        let mut make = |s: &mut Session, i: usize, kind: SlotKind, preset: Option<&'static super::prod_presets::Preset>| {
            created += 1;
            column_track(s, i, kind, preset)
        };
        // A column whose track was lost keeps its slots on a new track of the kind they play.
        // A column holding both kinds becomes an instrument column, and its audio slots (which
        // could not play there) are emptied and reported.
        for i in lost {
            let midi = s.live.columns.get(i).is_some_and(|c| c.slots.iter().flatten().any(|x| x.kind() == SlotKind::Midi));
            let preset = s.live.columns.get(i).filter(|_| midi).and_then(|c| super::prod_presets::preset(lost_column_preset(c)));
            let track = make(s, i, if midi { SlotKind::Midi } else { SlotKind::Audio }, preset);
            if let Some(c) = s.live.columns.get_mut(i) {
                c.track = track;
                for (row, cell) in c.slots.iter_mut().enumerate().filter(|_| midi) {
                    if let Some(x) = cell.take_if(|x| x.kind() == SlotKind::Audio) {
                        names.push(format!("`{}` (Live {} slot {})", x.name(), i + 1, row + 1));
                        emptied.push(json!({"column": i, "slot": row, "name": x.name()}));
                    }
                }
            }
        }
        for i in s.live.columns.len()..want {
            let track = make(s, i, SlotKind::Audio, None);
            s.live.columns.push(LiveColumn::new(track));
        }
    }
    if !names.is_empty() {
        e.message(format!(
            "Session view: emptied {}: audio slots an instrument column can't play (their files stay in the Clip List)",
            names.join(", ")
        ));
    }
    let columns: Vec<u64> = e.session().live.columns.iter().map(|c| c.track.0).collect();
    Ok(json!({"columns": columns, "created": created, "emptied": emptied}))
}

/// The preset a lost instrument column's new track gets, from what its MIDI slots hold: the drum
/// kit when every note is a General MIDI drum (35..=81 on channel 10), else the bass. The channel
/// counts as well as the pitch: a bass line's notes are often all in the drum range too (a rolling
/// bass in A is all 45, a GM tom), and the starter patterns put drums, and only drums, on 10.
fn lost_column_preset(c: &LiveColumn) -> &'static str {
    /// MIDI channel 10, 0-based.
    const DRUM_CHANNEL: u8 = 9;
    let mut notes = c.slots.iter().flatten().filter_map(|x| match x {
        SlotContent::Midi(m) => Some(m.sequence.notes.iter()),
        SlotContent::Audio(_) => None,
    });
    let drums = notes.all(|mut slot| slot.all(|n| n.channel == DRUM_CHANNEL && (35..=81).contains(&n.pitch)));
    if drums { DEFAULT_PRESET } else { "rolling_bass" }
}

/// The track for column `i`: "Live {i + 1}", a stereo audio track, or for MIDI an instrument track
/// with `preset`'s built-in instrument (the drums default, `trance_kit`, when `None`).
fn column_track(s: &mut Session, i: usize, kind: SlotKind, preset: Option<&'static super::prod_presets::Preset>) -> TrackId {
    let name = format!("Live {}", i + 1);
    match kind {
        SlotKind::Audio => s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(&name)),
        SlotKind::Midi => {
            let t = s.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some(&name));
            let preset = preset.or_else(|| super::prod_presets::preset(DEFAULT_PRESET));
            let ins = preset
                .and_then(|x| soundcraft_dsp::plugin_info(x.instrument).map(|info| super::instrument::plugin_with(info, x.params.iter().copied())));
            if let Some(tr) = s.track_mut(t) {
                tr.instrument = ins;
            }
            t
        }
    }
}

/// The preset an instrument column's track starts with.
const DEFAULT_PRESET: &str = "trance_kit";

fn add_column(e: &mut Engine, p: &Value) -> Result<Value> {
    let kind = match p.get("kind") {
        None | Some(Value::Null) => SlotKind::Audio,
        Some(v) => match v.as_str() {
            Some("audio") => SlotKind::Audio,
            Some("instrument") => SlotKind::Midi,
            _ => return Err(bad(ADD_COLUMN, "`kind` must be audio or instrument")),
        },
    };
    let preset = match (p.get("preset"), kind) {
        (None | Some(Value::Null), _) => None,
        (Some(_), SlotKind::Audio) => return Err(bad(ADD_COLUMN, "`preset` is for instrument columns")),
        (Some(v), SlotKind::Midi) => {
            let id = v.as_str().ok_or_else(|| bad(ADD_COLUMN, "`preset` must be a preset id (prod.presets)"))?;
            Some(super::prod_presets::preset(id).ok_or_else(|| bad(ADD_COLUMN, format!("unknown preset `{id}` (prod.presets lists them)")))?)
        }
    };
    let column = e.session().live.columns.len();
    if column >= MAX_LIVE_COLUMNS {
        return Err(bad(ADD_COLUMN, format!("the Session view has its most columns ({MAX_LIVE_COLUMNS})")));
    }
    let s = e.session_mut();
    let track = column_track(s, column, kind, preset);
    s.live.columns.push(LiveColumn::new(track));
    let preset = (kind == SlotKind::Midi).then(|| preset.map_or(DEFAULT_PRESET, |x| x.id));
    let kind = if kind == SlotKind::Midi { "instrument" } else { "audio" };
    Ok(json!({"column": column, "track": track, "kind": kind, "preset": preset}))
}

/// Refuses a slot of `kind` for column `column`: MIDI goes only in an instrument column, audio
/// only in an audio track's. A lost column takes either (its track is made when it is set up).
pub(super) fn takes(e: &Engine, cmd: &str, column: usize, kind: SlotKind) -> Result<()> {
    let s = e.session();
    match s.live.column_kind(column, &s.tracks) {
        Some(k) if k != kind => Err(bad(
            cmd,
            match kind {
                SlotKind::Midi => {
                    format!("Live column {} is an audio column: MIDI goes in an instrument column (live.add_column kind: instrument)", column + 1)
                }
                SlotKind::Audio => format!("Live column {} is an instrument column: it holds MIDI slots, not audio", column + 1),
            },
        )),
        _ => Ok(()),
    }
}

/// The 0-based column `p` names, which must exist.
pub(super) fn column_param(e: &Engine, cmd: &str, p: &Value) -> Result<usize> {
    let n = e.session().live.columns.len();
    if n == 0 {
        return Err(bad(cmd, NO_COLUMNS));
    }
    let v = p.get("column").ok_or_else(|| bad(cmd, "`column` required (0-based)"))?;
    v.as_u64().and_then(|c| usize::try_from(c).ok()).filter(|c| *c < n).ok_or_else(|| bad(cmd, format!("`column` must be 0..{n}")))
}

/// The 0-based slot `p` names, if it names one.
pub(super) fn slot_param(cmd: &str, p: &Value) -> Result<Option<usize>> {
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
pub(super) fn target(e: &Engine, cmd: &str, p: &Value, column: usize) -> Result<usize> {
    if let Some(r) = slot_param(cmd, p)? {
        return Ok(r);
    }
    let col = e.session().live.columns.get(column).ok_or_else(|| bad(cmd, NO_COLUMNS))?;
    col.next_empty().ok_or_else(|| bad(cmd, format!("Live column {} is full: pass `slot` to replace one, or clear one", column + 1)))
}

/// `p`'s `replace` (default false).
pub(super) fn replace_param(cmd: &str, p: &Value) -> Result<bool> {
    match p.get("replace") {
        None | Some(Value::Null) => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| bad(cmd, "`replace` must be true or false")),
    }
}

/// Refuses filling slot `row` of `column` while it holds something, unless `replace`.
pub(super) fn refuse_occupied(e: &Engine, cmd: &str, column: usize, row: usize, replace: bool) -> Result<()> {
    match e.session().live.slot(column, row).filter(|_| !replace) {
        Some(x) => {
            Err(would_replace(cmd, format!("slot {} of Live column {} holds `{}`: pass replace: true to replace it", row + 1, column + 1, x.name())))
        }
        None => Ok(()),
    }
}

/// Beats in a slot's bar. Slots count bars of four beats, so the session must be in 4/4
/// throughout (as for set exports): see [`four_four`].
const BAR: f64 = 4.0;

/// Why slot bars would not be bars in `s`: a meter other than 4/4 somewhere.
fn not_four_four(s: &Session) -> Option<String> {
    s.tempo.meters().iter().find(|m| (m.numerator, m.denominator) != (4, 4)).map(|m| {
        format!(
            "bar {} is in {}/{}: Session view slots need 4/4 throughout; set the meter to 4/4 first",
            s.tempo.bar_beat_at_tick(m.tick).bar,
            m.numerator,
            m.denominator
        )
    })
}

/// Refuses a session whose meter is not 4/4 throughout, where slot bars would not be bars.
pub(super) fn four_four(cmd: &str, s: &Session) -> Result<()> {
    not_four_four(s).map_or(Ok(()), |why| Err(bad(cmd, why)))
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
pub(super) fn put(e: &mut Engine, column: usize, row: usize, slot: impl Into<SlotContent>) {
    if let Some(cell) = e.session_mut().live.columns.get_mut(column).and_then(|c| c.slots.get_mut(row)) {
        *cell = Some(slot.into());
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
    takes(e, FROM_SELECTION, column, SlotKind::Audio)?;
    let row = target(e, FROM_SELECTION, p, column)?;
    refuse_occupied(e, FROM_SELECTION, column, row, replace_param(FROM_SELECTION, p)?)?;
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

fn from_clip(e: &mut Engine, p: &Value) -> Result<Value> {
    let column = column_param(e, FROM_CLIP, p)?;
    takes(e, FROM_CLIP, column, SlotKind::Midi)?;
    let row = target(e, FROM_CLIP, p, column)?;
    let replace = replace_param(FROM_CLIP, p)?;
    four_four(FROM_CLIP, e.session())?;
    refuse_occupied(e, FROM_CLIP, column, row, replace)?;
    let s = e.session();
    let id = match p.get("clip") {
        None | Some(Value::Null) => {
            let midi: Vec<ClipId> = s
                .edit
                .selected_clips
                .iter()
                .copied()
                .filter(|c| s.find_clip(*c).is_some_and(|(_, x)| matches!(x.content, ClipContent::Midi { .. })))
                .collect();
            match midi.as_slice() {
                [one] => *one,
                [] => return Err(bad(FROM_CLIP, "select a MIDI clip first (or pass `clip`)")),
                _ => return Err(bad(FROM_CLIP, "several MIDI clips are selected: select one (or pass `clip`)")),
            }
        }
        Some(v) => ClipId(v.as_u64().ok_or_else(|| bad(FROM_CLIP, "`clip` must be a clip id"))?),
    };
    let (track, clip) = s.find_clip(id).ok_or_else(|| bad(FROM_CLIP, format!("no clip with id {}", id.0)))?;
    let ClipContent::Midi { sequence } = &clip.content else {
        return Err(bad(FROM_CLIP, format!("`{}` is not a MIDI clip", clip.name)));
    };
    // The clip's length in session beats, snapped to whole bars.
    let ticks = |t: Samples| s.tempo.samples_to_ticks_f(t, s.sample_rate);
    let beats = (ticks(clip.end()) - ticks(clip.start)) / soundcraft_time::TICKS_PER_QUARTER as f64;
    let bars = (beats / BAR).round().max(1.0);
    if bars > f64::from(MAX_SLOT_BARS) {
        return Err(bad(FROM_CLIP, format!("`{}` is {bars} bars: a slot holds at most {MAX_SLOT_BARS}", clip.name)));
    }
    let length_beats = bars * BAR;
    // What a start trim hid (before tick 0) and what starts past the loop never play: the slot
    // keeps only what it plays (see `MidiSlot::sanitize`).
    let controllers_dropped = sequence.ctrls.len();
    let color = clip.color.or_else(|| s.track(track).map(|t| t.color)).unwrap_or([128, 128, 128]);
    let slot = MidiSlot::new(clip.name.clone(), color, sequence.clone(), length_beats, 0.0);
    let out = json!({"column": column, "slot": row, "clip": id, "name": slot.name, "bars": bars, "length_beats": length_beats,
        "notes": slot.sequence.notes.len(), "controllers_dropped": controllers_dropped});
    put(e, column, row, slot);
    Ok(out)
}

fn to_clip(e: &mut Engine, p: &Value) -> Result<Value> {
    let column = column_param(e, TO_CLIP, p)?;
    let row = slot_param(TO_CLIP, p)?.ok_or_else(|| bad(TO_CLIP, "`slot` required (0-based)"))?;
    let replace = replace_param(TO_CLIP, p)?;
    let at = position_param(e, TO_CLIP, p, "at")?;
    let s = e.session();
    four_four(TO_CLIP, s)?;
    let x = match s.live.slot(column, row) {
        Some(SlotContent::Midi(x)) => x.clone(),
        Some(SlotContent::Audio(_)) => return Err(bad(TO_CLIP, format!("slot {} of Live column {} holds audio, not MIDI", row + 1, column + 1))),
        None => return Err(bad(TO_CLIP, format!("slot {} of Live column {} is empty", row + 1, column + 1))),
    };
    let track = s.live.column_track(column, &s.tracks).ok_or_else(|| bad(TO_CLIP, format!("Live column {} has no track", column + 1)))?;
    let sr = s.sample_rate;
    let bar = s.tempo.bar_beat_at(at.unwrap_or(s.edit.playhead), sr).bar.max(1);
    let start_tick = s.tempo.bar_start_tick(bar);
    let len_ticks = ((x.length_beats * soundcraft_time::TICKS_PER_QUARTER as f64).round() as i64).max(1);
    let span = Range::new(s.tempo.tick_to_samples(start_tick, sr), s.tempo.tick_to_samples(start_tick.saturating_add(len_ticks), sr));
    let tr = s.track(track).ok_or_else(|| bad(TO_CLIP, "no such track"))?;
    let under: Vec<String> = tr.clips().iter().filter(|c| c.range().overlaps(&span)).map(|c| format!("`{}`", c.name)).collect();
    if !under.is_empty() && !replace {
        return Err(would_replace(TO_CLIP, format!("bar {bar} on `{}` holds {}: pass replace: true to replace it", tr.name, under.join(", "))));
    }
    let s = e.session_mut();
    let id = s.new_clip_id();
    let mut clip = soundcraft_model::Clip::midi(id, x.name.clone(), span.start, span.len(), x.sequence.clone());
    clip.color = Some(x.color);
    // Placing it clears (and trims) whatever was under it: refused above unless `replace`.
    crate::edit::place_clip(s, track, clip);
    Ok(json!({"clip": id, "track": track, "name": x.name, "start": span.start, "end": span.end, "bars": x.length_beats / BAR,
        "notes": x.sequence.notes.len()}))
}

/// What `live.slot_from_file` was asked for, checked.
#[derive(Debug, Clone)]
struct FileParams {
    column: usize,
    row: usize,
    path: String,
    bpm: Option<f64>,
    first_beat: f64,
}

fn file_params(e: &Engine, p: &Value) -> Result<FileParams> {
    let column = column_param(e, FROM_FILE, p)?;
    takes(e, FROM_FILE, column, SlotKind::Audio)?;
    let path = str_param(p, "path").ok_or_else(|| bad(FROM_FILE, "`path` required"))?.to_string();
    let row = target(e, FROM_FILE, p, column)?;
    refuse_occupied(e, FROM_FILE, column, row, replace_param(FROM_FILE, p)?)?;
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
    Ok(FileParams { column, row, path, bpm, first_beat })
}

/// A file read, decoded and gridded for a slot: the slow part of `live.slot_from_file`, which a
/// [`SlotFileJob`] does off the UI thread.
#[derive(Debug)]
struct Loaded {
    path: String,
    decoded: crate::io::DecodedImport,
    beats: Option<Beats>,
    how: Option<&'static str>,
    analysis_error: Option<String>,
    /// The Camelot key mixforge's analysis gave, if any.
    key: Option<String>,
}

/// Reads and decodes the file (so one that won't decode is refused before mixforge runs), then
/// finds its beat grid: given, else mixforge's analysis (`analyse`, when the app set it up),
/// else the file's BPM tag with the first downbeat at the file start, else none (a one-shot).
fn load(f: &FileParams, rate: u32, analyse: Option<&super::PrepAnalyse>) -> Result<Loaded> {
    let bytes = crate::io::read_file(&f.path)?;
    let decoded = crate::io::decode_import(&file_name(&f.path), &bytes, rate)?;
    let (mut analysis_error, mut key) = (None, None);
    let (beats, how) = match f.bpm {
        Some(bpm) => (Some(Beats::Constant { bpm, first: f.first_beat }), Some("given")),
        None => match analysed(analyse, &f.path, &mut key) {
            Ok(b) => (Some(b), Some("mixforge")),
            Err(why) => {
                analysis_error = Some(why);
                match tagged_bpm(&f.path) {
                    Some(bpm) => (Some(Beats::Constant { bpm, first: 0.0 }), Some("tag")),
                    None => (None, None),
                }
            }
        },
    };
    Ok(Loaded { path: f.path.clone(), decoded, beats, how, analysis_error, key })
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("Audio").to_string()
}

thread_local! {
    /// The file a [`SlotFile`] loaded, for the `live.slot_from_file` it runs to apply it.
    static LOADED: std::cell::RefCell<Option<Loaded>> = const { std::cell::RefCell::new(None) };
}

fn from_file(e: &mut Engine, p: &Value) -> Result<Value> {
    let f = file_params(e, p)?;
    let ready = LOADED.with(|l| l.borrow_mut().take()).filter(|l| l.path == f.path);
    let loaded = match ready {
        Some(l) => l,
        None => {
            let analyse = e.analyse().cloned();
            load(&f, e.session().sample_rate.hz(), analyse.as_ref())?
        }
    };
    let Loaded { decoded, beats, how, analysis_error, key, .. } = loaded;
    let (column, row) = (f.column, f.row);
    // A failure from here on is rolled back with the import.
    let (source, frames, _) = crate::io::add_import(e, &file_name(&f.path), decoded, Some(&f.path))?;
    // The analysed key over any the file's tag gave.
    if let Some(k) = key
        && let Some(x) = e.session_mut().sources.iter_mut().find(|x| x.id == source)
    {
        x.key = Some(k);
    }
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

/// The beat grid mixforge's analysis (`analyse`) gives the file at `path`, or why there is none;
/// the key it gives goes into `key` either way. It runs to the end: there is no cancel, and the
/// sidecar has its time limit.
fn analysed(analyse: Option<&super::PrepAnalyse>, path: &str, key: &mut Option<String>) -> std::result::Result<Beats, String> {
    let analyse = analyse.ok_or("no mixforge analysis is set up")?;
    let a = analyse(path, &|| false)?;
    *key = analysed_key(&a);
    let (bpm, first) = grid_of(&a).ok_or("mixforge's analysis has no usable bpm/first_downbeat")?;
    Ok(Beats::Constant { bpm, first })
}

/// `live.slot_from_file` with the file read, decoded and analysed on another thread, so the app
/// stays responsive: [`slot_file_job`] checks the parameters and starts it, [`SlotFileJob::poll`]
/// gives the [`SlotFile`] once ready, and [`SlotFile::apply`] fills the slot as one undo step.
pub struct SlotFileJob {
    column: usize,
    row: usize,
    params: Value,
    state: JobState,
}

enum JobState {
    Running(std::thread::JoinHandle<Result<Loaded>>),
    Ended(Option<Box<Result<Loaded>>>),
}

/// A loaded file waiting to fill its slot.
pub struct SlotFile {
    params: Value,
    loaded: Loaded,
}

/// Starts loading the file `p` (`live.slot_from_file`'s parameters) names, for its slot: every
/// check is made before this returns (refusals as `live.slot_from_file` makes them).
pub fn slot_file_job(e: &Engine, p: &Value) -> Result<SlotFileJob> {
    if e.session().live.columns.is_empty() {
        return Err(bad(FROM_FILE, NO_COLUMNS));
    }
    let f = file_params(e, p)?;
    let mut params = p.clone();
    if let Some(o) = params.as_object_mut() {
        o.insert("slot".into(), json!(f.row));
    }
    let rate = e.session().sample_rate.hz();
    let analyse = e.analyse().cloned();
    let roots = crate::file_access::current();
    let (column, row) = (f.column, f.row);
    let work = move || {
        let run = || load(&f, rate, analyse.as_ref());
        match &roots {
            Some(r) => crate::file_access::confine(Some(r), run),
            None => crate::file_access::unconfined(run),
        }
    };
    let state = match std::thread::Builder::new().name("slot-file".into()).spawn(work) {
        Ok(handle) => JobState::Running(handle),
        Err(err) => return Err(EngineError::Io(format!("could not start loading the file: {err}"))),
    };
    Ok(SlotFileJob { column, row, params, state })
}

impl SlotFileJob {
    /// The slot it loads into (column, row).
    pub fn target(&self) -> (usize, usize) {
        (self.column, self.row)
    }

    /// The loaded file once ready (once), or why it could not be loaded.
    pub fn poll(&mut self) -> Option<Result<SlotFile>> {
        if let JobState::Running(handle) = &self.state
            && !handle.is_finished()
        {
            return None;
        }
        let r = match std::mem::replace(&mut self.state, JobState::Ended(None)) {
            JobState::Running(handle) => Some(handle.join().unwrap_or_else(|_| Err(EngineError::Io("loading the file failed".into())))),
            JobState::Ended(r) => r.map(|r| *r),
        };
        r.map(|r| r.map(|loaded| SlotFile { params: self.params.clone(), loaded }))
    }
}

impl SlotFile {
    /// Fills the slot with the loaded file: `live.slot_from_file` (one undo step), refused as it
    /// would be now (the slot's column gone, the meter changed…).
    pub fn apply(self, e: &mut Engine) -> Result<Value> {
        LOADED.with(|l| *l.borrow_mut() = Some(self.loaded));
        let r = e.execute(FROM_FILE, &self.params);
        LOADED.with(|l| l.borrow_mut().take());
        r
    }
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
fn filled(e: &Engine, cmd: &str, p: &Value) -> Result<(usize, usize, SlotContent)> {
    let column = column_param(e, cmd, p)?;
    let row = slot_param(cmd, p)?.ok_or_else(|| bad(cmd, "`slot` required (0-based)"))?;
    let slot =
        e.session().live.slot(column, row).cloned().ok_or_else(|| bad(cmd, format!("slot {} of Live column {} is empty", row + 1, column + 1)))?;
    Ok((column, row, slot))
}

fn set(e: &mut Engine, p: &Value) -> Result<Value> {
    let (column, row, old) = filled(e, SET, p)?;
    let replace = replace_param(SET, p)?;
    if !["name", "color", "mode", "length_bars", "gain_db"].iter().any(|k| p.get(k).is_some()) {
        return Err(bad(SET, "nothing to set: pass name, color, mode, length_bars or gain_db"));
    }
    let mut slot = old.clone();
    // The fields both kinds have.
    let (name, color, length_beats, gain_db) = match &mut slot {
        SlotContent::Audio(x) => (&mut x.name, &mut x.color, &mut x.length_beats, &mut x.gain_db),
        SlotContent::Midi(x) => (&mut x.name, &mut x.color, &mut x.length_beats, &mut x.gain_db),
    };
    if let Some(v) = p.get("name") {
        let mut text = v.as_str().ok_or_else(|| bad(SET, "`name` must be text"))?.to_string();
        soundcraft_model::truncate_chars(&mut text, soundcraft_model::MAX_NAME_CHARS);
        *name = text;
    }
    if let Some(v) = p.get("color") {
        let rgb: Option<Vec<u8>> = v.as_array().and_then(|a| a.iter().map(|c| c.as_u64().and_then(|c| u8::try_from(c).ok())).collect());
        *color = rgb.and_then(|c| <[u8; 3]>::try_from(c).ok()).ok_or_else(|| bad(SET, "`color` must be [r, g, b], each 0..=255"))?;
    }
    if let Some(v) = p.get("length_bars") {
        four_four(SET, e.session())?;
        let most = (MAX_SLOT_BEATS / BAR).floor();
        let bars = v
            .as_f64()
            .filter(|b| b.fract() == 0.0 && (1.0..=most).contains(b))
            .ok_or_else(|| bad(SET, format!("`length_bars` must be a whole number 1..={most}")))?;
        *length_beats = bars * BAR;
    }
    if let Some(v) = p.get("gain_db") {
        let (lo, hi) = SLOT_GAIN_DB;
        *gain_db =
            v.as_f64().map(|g| g as f32).filter(|g| (lo..=hi).contains(g)).ok_or_else(|| bad(SET, format!("`gain_db` must be {lo}..={hi}")))?;
    }
    if let Some(v) = p.get("mode") {
        let mode = v.as_str().and_then(SlotMode::from_id).ok_or_else(|| bad(SET, "`mode` must be loop or one_shot"))?;
        match &mut slot {
            SlotContent::Audio(x) if mode == SlotMode::Loop && x.warp.is_none() => {
                return Err(bad(SET, "this slot has no beat grid, so it plays only as a one-shot: load the file again with a `bpm`"));
            }
            SlotContent::Audio(x) => x.mode = mode,
            SlotContent::Midi(_) if mode != SlotMode::Loop => return Err(bad(SET, "a MIDI slot only loops")),
            SlotContent::Midi(_) => {}
        }
    }
    // A MIDI slot holds only what it plays (as it reloads): notes starting past a shortened loop
    // go, and those running past its end are cut there. Dropping notes needs `replace`.
    let mut notes_dropped = 0usize;
    if let SlotContent::Midi(x) = &mut slot {
        let before = x.sequence.notes.len();
        x.sanitize();
        notes_dropped = before.saturating_sub(x.sequence.notes.len());
    }
    if notes_dropped > 0 && !replace {
        let bars = slot.length_beats() / BAR;
        return Err(would_replace(
            SET,
            format!(
                "{bars} bars would drop {notes_dropped} note{} of `{}` (slot {} of Live column {}) starting past the new end: pass replace: true to drop them",
                if notes_dropped == 1 { "" } else { "s" },
                old.name(),
                row + 1,
                column + 1
            ),
        ));
    }
    let out = json!({"column": column, "slot": row, "kind": slot.kind().id(), "name": slot.name(), "color": slot.color(), "mode": slot.mode().id(),
        "length_beats": slot.length_beats(), "gain_db": slot.gain_db(), "notes_dropped": notes_dropped});
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

// ---- launching -----------------------------------------------------------------------------

const LAUNCH: &str = "live.launch";
const STOP: &str = "live.stop";
const STOP_ALL: &str = "live.stop_all";
const SCENE: &str = "live.scene";

/// The least time a launch is sent ahead of the boundary it waits for, in seconds.
pub const LAUNCH_MARGIN_S: f64 = 0.070;

/// Samples beyond one audio callback a launch is sent ahead: the audio thread renders a block of
/// up to this many on top of the callback in progress.
pub const LAUNCH_BLOCK: Samples = 512;

/// How far ahead of the playhead a launch must land, in session samples: at least
/// [`LAUNCH_MARGIN_S`], and the largest audio callback (`callback`, in session samples) plus
/// [`LAUNCH_BLOCK`], so the audio thread has the launch before its sample comes.
pub fn launch_margin(s: &Session, callback: Samples) -> Samples {
    s.sample_rate.samples(LAUNCH_MARGIN_S).max(callback.max(0).saturating_add(LAUNCH_BLOCK))
}

/// Where a launch meant for `at` lands while the transport loops over `looped` and is at `pos`:
/// `at` itself inside the loop, else the loop's first boundary of `quantise` (at or after its
/// start), as many loop lengths ahead as the transport needs to reach it at least `margin` from
/// now. The audio engine moves what is queued back one loop length at each wrap, so it plays
/// there. Refused when no boundary falls inside the loop (it is shorter than the quantise).
pub fn fold_into_loop(
    s: &Session,
    at: Samples,
    pos: Samples,
    looped: soundcraft_time::Range,
    quantise: Quantise,
    margin: Samples,
) -> std::result::Result<Samples, String> {
    let len = looped.len();
    if len <= 0 || at < looped.end {
        return Ok(at);
    }
    let first = next_boundary(s, looped.start, quantise, 0)?;
    if first >= looped.end {
        return Err(format!(
            "no {} boundary falls inside the loop: launch with a smaller launch quantise, or loop a longer selection",
            quantise.id()
        ));
    }
    let behind = pos.saturating_add(margin.max(0)).saturating_sub(first);
    let wraps = (behind.max(1).saturating_add(len - 1) / len).max(1);
    Ok(first.saturating_add(wraps.saturating_mul(len)))
}

/// The first launch boundary of `quantise` at or after `playhead + margin`: that sample itself
/// for [`Quantise::None`], else the first quarter note, half note or line of every `n` bars
/// (counted from bar 1) there or later, on the session tempo map (ramps included). Bars are four
/// beats, so a session not in 4/4 throughout is refused, as the slot commands refuse it.
pub fn next_boundary(s: &Session, playhead: Samples, quantise: Quantise, margin: Samples) -> std::result::Result<Samples, String> {
    if let Some(why) = not_four_four(s) {
        return Err(why);
    }
    let t0 = playhead.saturating_add(margin.max(0));
    let beats: i64 = match quantise.normalised() {
        Quantise::None => return Ok(t0),
        Quantise::Quarter => 1,
        Quantise::Half => 2,
        Quantise::Bar(n) => 4 * i64::from(n),
    };
    let grid = beats.saturating_mul(soundcraft_time::TICKS_PER_QUARTER).max(1);
    let tick0 = s.tempo.samples_to_ticks_f(t0, s.sample_rate);
    if !tick0.is_finite() {
        return Err("the playhead is outside the session".into());
    }
    let mut k = ((tick0 / grid as f64) - 1e-9).ceil() as i64;
    // The boundary's tick becomes its nearest sample, which may fall just before `t0`.
    for _ in 0..3 {
        let at = s.tempo.tick_to_samples(k.saturating_mul(grid), s.sample_rate);
        if at >= t0 {
            return Ok(at);
        }
        k = k.saturating_add(1);
    }
    Ok(s.tempo.tick_to_samples(k.saturating_mul(grid), s.sample_rate))
}

/// The audio engine's commands for `req` at sample `at`. A scene launches its row in every
/// column that plays into a track and has a slot there (none, when no column has one).
pub fn live_cmds(s: &Session, req: &crate::LiveRequest, at: Samples) -> Vec<soundcraft_mix::LiveCmd> {
    use crate::LiveRequest as R;
    use soundcraft_mix::LiveCmd as C;
    match *req {
        R::Launch { column, slot } => vec![C::Launch { column, slot, at }],
        R::Stop { column } => vec![C::Stop { column, at }],
        R::StopAll => vec![C::StopAll { at }],
        R::Scene { row } => (0..s.live.columns.len())
            .filter(|&c| s.live.slot(c, row).is_some() && s.live.column_track(c, &s.tracks).is_some())
            .map(|column| C::Launch { column, slot: row, at })
            .collect(),
    }
}

/// Refuses a launch or stop the host can't carry out: no audio engine, or bars that aren't bars.
fn can_launch(e: &Engine, cmd: &str) -> Result<()> {
    if !e.live_audio {
        return Err(bad(
            cmd,
            "Session view launches need the app's audio engine: run this in SoundCraft (or `soundcraft --control` and `soundcraft-cli mcp --connect`)",
        ));
    }
    four_four(cmd, e.session())
}

/// The column `p` names, which must play into a track.
fn playing_column(e: &Engine, cmd: &str, p: &Value) -> Result<usize> {
    let column = column_param(e, cmd, p)?;
    let s = e.session();
    if s.live.column_track(column, &s.tracks).is_none() {
        return Err(bad(cmd, format!("Live column {} has lost its track: open View › Session (live.setup) to give it a new one", column + 1)));
    }
    Ok(column)
}

fn queue(e: &mut Engine, req: crate::LiveRequest) -> Value {
    e.transport_requests.push(TransportRequest::Live(req));
    json!({"request": req, "quantise": e.session().live.quantise.id(), "starts_transport": !e.transport.playing})
}

fn launch(e: &mut Engine, p: &Value) -> Result<Value> {
    let column = playing_column(e, LAUNCH, p)?;
    let row = slot_param(LAUNCH, p)?.ok_or_else(|| bad(LAUNCH, "`slot` required (0-based)"))?;
    if e.session().live.slot(column, row).is_none() {
        return Err(bad(LAUNCH, format!("slot {} of Live column {} is empty", row + 1, column + 1)));
    }
    can_launch(e, LAUNCH)?;
    Ok(queue(e, crate::LiveRequest::Launch { column, slot: row }))
}

fn stop(e: &mut Engine, p: &Value) -> Result<Value> {
    let column = column_param(e, STOP, p)?;
    can_launch(e, STOP)?;
    Ok(queue(e, crate::LiveRequest::Stop { column }))
}

fn stop_all(e: &mut Engine, _: &Value) -> Result<Value> {
    can_launch(e, STOP_ALL)?;
    Ok(queue(e, crate::LiveRequest::StopAll))
}

fn scene(e: &mut Engine, p: &Value) -> Result<Value> {
    let row = p
        .get("row")
        .and_then(Value::as_u64)
        .and_then(|r| usize::try_from(r).ok())
        .filter(|r| *r < LIVE_SLOTS)
        .ok_or_else(|| bad(SCENE, format!("`row` must be 0..{LIVE_SLOTS}")))?;
    if live_cmds(e.session(), &crate::LiveRequest::Scene { row }, 0).is_empty() {
        return Err(bad(SCENE, format!("no Live column has a slot in row {}", row + 1)));
    }
    can_launch(e, SCENE)?;
    Ok(queue(e, crate::LiveRequest::Scene { row }))
}

fn state(e: &mut Engine, _: &Value) -> Result<Value> {
    let s = e.session();
    let now = e.transport.position;
    let beat = |t: Samples| s.tempo.samples_to_ticks_f(t, s.sample_rate) / soundcraft_time::TICKS_PER_QUARTER as f64;
    let columns: Vec<Value> = (0..s.live.columns.len())
        .map(|c| {
            let status = e.live_status.get(c).copied().unwrap_or_default();
            let playing = status.playing.map(|(row, start)| {
                let slot = s.live.slot(c, row);
                // Where a loop is: session beats since its launch beat, wrapped to its length, as
                // the mixer plays it (counted from `origin`, which transport jumps don't move).
                let looping = slot.filter(|x| x.midi().is_some() || x.audio().is_some_and(soundcraft_mix::slot_loops));
                let position = looping.map(|x| (beat(now) - beat(status.origin)).rem_euclid(x.length_beats().max(1.0)));
                json!({"slot": row, "name": slot.map(SlotContent::name), "kind": slot.map(|x| x.kind().id()), "start": start, "loop_beat": position,
                    "length_beats": looping.map(SlotContent::length_beats)})
            });
            let queued = status.queued.map(|(row, at)| json!({"slot": row, "at": at, "stop": row.is_none()}));
            let kind = s.live.column_kind(c, &s.tracks).map(|k| if k == SlotKind::Midi { "instrument" } else { "audio" });
            let slots: Vec<Value> = (0..LIVE_SLOTS)
                .map(|r| {
                    s.live.slot(c, r).map_or(Value::Null, |x| json!({"name": x.name(), "kind": x.kind().id(), "length_beats": x.length_beats()}))
                })
                .collect();
            json!({"column": c, "track": s.live.column_track(c, &s.tracks), "kind": kind, "slots": slots, "playing": playing, "queued": queued})
        })
        .collect();
    Ok(json!({"transport": {"playing": e.transport.playing, "position": now}, "quantise": s.live.quantise.id(),
        "audio": e.live_audio, "columns": columns}))
}
