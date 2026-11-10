//! Recorded MIDI takes: the live MIDI played on a record-armed instrument track becomes a clip in
//! the arrangement (`midi.record_take`), and a Session view slot recording becomes a MIDI slot
//! (`live.record_slot` arms it, `live.slot_from_take` writes it).
//!
//! The host captures each live event as it played (its timeline sample and its MIDI 1.0 bytes)
//! and hands the whole take over in one command when recording stops. Note-ons pair with their
//! note-offs (a note-on of a pitch already held ends the held one there); a note still held at
//! the end is cut there. Control changes, pitch bend, pressure and program changes are kept as
//! the clip's controllers (a slot keeps none: slots play only notes).
//!
//! A take is played in passes: a loop wrap or a locate while recording starts a new one, and
//! notes pair only within a pass. Each pass becomes a clip over the time it was played.
//!
//! **Record modes.** Audio takes follow `edit.record_mode`, which is about audio files
//! (destructive, TrackPunch); MIDI follows MIDI Merge (`options.midi_merge`), Pro Tools' MIDI
//! overdub switch. With it on, a pass over one MIDI clip is merged into it (the clip grows to
//! cover it). With it off, a pass goes on the track as a new clip where nothing is; over another
//! clip, `on_overlap` decides: refused ([`would_replace`]), recorded over (`replace`, clearing
//! only what lies under its own time), or put on a new playlist made active (`new_playlist`,
//! what the app does when recording stops, as audio loop passes do): a copy of the active
//! playlist, so the rest of the track plays on, with the take placed on it, the playlist before
//! intact. So a take never silently replaces Marty's clips, and is never lost.

use super::live::{column_param, four_four, refuse_occupied, replace_param, slot_param};
use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_midi::{CtrlEvent, CtrlKind, Note, Sequence};
use soundcraft_model::{Clip, ClipContent, ClipId, MAX_SLOT_BARS, MidiSlot, Session, SlotKind, TrackId};
use soundcraft_time::{TICKS_PER_QUARTER, TempoMap};
use std::collections::BTreeMap;

const RECORD_TAKE: &str = "midi.record_take";
const RECORD_SLOT: &str = "live.record_slot";
const SLOT_FROM_TAKE: &str = "live.slot_from_take";

/// Most events one take may hold.
const MAX_EVENTS: usize = 1_000_000;

/// Beats in a slot's bar (slots count 4/4 bars).
const SLOT_BAR_BEATS: i64 = 4;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "midi.record_take",
            "Record MIDI",
            [],
            None,
            "{track, start, end (positions: where recording started and stopped), events: [{at (timeline sample it played on), status, data1?, data2?}] (MIDI 1.0 channel messages in the order played), passes?: [{start, end, events}] (instead of start/end/events, for a take with transport jumps: one per stretch played without a loop wrap or locate), quantize?: bool (default: Input Quantize's setting), on_overlap?: refuse|replace|new_playlist (default refuse; replace: true means replace), new_playlist?: bool (always on a new playlist)}; at most 1000000 events in all — each pass of the live MIDI recorded on an instrument or MIDI track becomes a MIDI clip over the time it was played (a note quantized past its end stretches it): note-ons paired with their note-offs within the pass (a repeated note-on ends the held note there), notes still held at its end cut there, events outside it left out; control changes, pitch bend, pressure and program changes kept as controllers. Input Quantize (event.input_quantize) moves the notes onto the session's grid when on; the played timing stays restorable (event.restore_performance). With MIDI Merge on (options.midi_merge) a pass over one MIDI clip is merged into it, the clip growing to cover it. Otherwise a pass over another clip is refused, records over what lies under its own time (replace), or goes on a new playlist made active: a copy of the active one (so the rest of the track plays on) with the take placed on it, clearing only what lies under its own time, the playlist before staying intact (new_playlist), as audio loop passes do. A take with neither notes nor controllers creates nothing. One undo step. Returns {clip (the last; null when nothing was recorded), takes: [{clip, playlist, new_playlist, notes, controllers, start, end, merged, held_cut}], notes, controllers, start, end, merged, quantized, held_cut, playlist, new_playlist, message}",
            always,
            record_take
        ),
        cmd!(
            noundo "live.record_slot",
            "Record into Slot",
            [],
            None,
            "{column, slot (0-based; an empty slot of an instrument column), bars (1..=256 whole 4/4 bars), replace?: bool (default false)} — arms a Session view slot recording: from the next launch-quantise boundary (the transport starts from the edit insertion if stopped) the live MIDI played through the column's instrument (the live MIDI target while armed) is recorded for `bars` bars, then written into the slot (live.slot_from_take), which launches right there and loops on seamlessly. Stop or Esc cancels. Refuses a filled slot unless `replace`, and an audio column. Needs the app's audio engine and 4/4 throughout",
            has_columns,
            record_slot
        ),
        cmd!(
            "live.slot_from_take",
            "Record Slot Take",
            [],
            None,
            "{column, slot (0-based; an instrument column), start (the sample the recording started on: tick 0 of the slot), bars (1..=256 whole 4/4 bars), events: [{at, status, data1?, data2?}] as midi.record_take takes them, quantize?: bool (default: Input Quantize's setting), replace?: bool (default false)} — writes a slot recording into the slot: the notes played from `start` for `bars` bars, in ticks from `start`, notes held at the end cut there (controllers are dropped: slots play only notes). Input Quantize applies when on, a note quantized onto the loop's end wrapping round to its start. Refuses a filled slot unless `replace`. A take without notes writes nothing. One undo step. Returns {column, slot, notes, bars, length_beats, start, end, quantized, held_cut}",
            has_columns,
            slot_from_take
        ),
    ]
}

fn has_columns(e: &Engine) -> std::result::Result<(), String> {
    if e.session().live.columns.is_empty() { Err("the session has no Live columns: open View › Session first (live.setup)".into()) } else { Ok(()) }
}

/// One event of a take: the timeline sample it played on and its MIDI 1.0 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TakeEvent {
    at: Samples,
    bytes: [u8; 3],
}

/// `p`'s `events`, in the order played (a missing list is empty).
fn events_param(cmd: &str, p: &Value) -> Result<Vec<TakeEvent>> {
    let list = match p.get("events") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(bad(cmd, "`events` must be a list of {at, status, data1?, data2?}")),
    };
    if list.len() > MAX_EVENTS {
        return Err(bad(cmd, format!("a take holds at most {MAX_EVENTS} events")));
    }
    let byte = |v: &Value, key: &str, i: usize| -> Result<u8> {
        match v.get(key) {
            None | Some(Value::Null) if key != "status" => Ok(0),
            Some(x) => x
                .as_u64()
                .and_then(|b| u8::try_from(b).ok())
                .filter(|b| key == "status" || *b < 0x80)
                .ok_or_else(|| bad(cmd, format!("event {i}: `{key}` must be a MIDI byte"))),
            None => Err(bad(cmd, format!("event {i}: `status` required"))),
        }
    };
    let mut out = Vec::with_capacity(list.len());
    for (i, v) in list.iter().enumerate() {
        let at = v
            .get("at")
            .and_then(|a| a.as_i64().or_else(|| a.as_f64().filter(|f| f.is_finite()).map(|f| f.round() as i64)))
            .ok_or_else(|| bad(cmd, format!("event {i}: `at` (a timeline sample) required")))?
            .clamp(-MAX_POSITION, MAX_POSITION);
        let status = byte(v, "status", i)?;
        if !(0x80..0xF0).contains(&status) {
            return Err(bad(cmd, format!("event {i}: status {status:#04x} is not a channel message")));
        }
        out.push(TakeEvent { at, bytes: [status, byte(v, "data1", i)?, byte(v, "data2", i)?] });
    }
    // In time order; events on one sample keep the order they played in.
    out.sort_by_key(|e| e.at);
    Ok(out)
}

/// A take's notes and controllers over ticks `from..to` (each event at `tick(at)`): note-ons
/// paired with note-offs, a repeated note-on ending the held note, notes held at `to` (or whose
/// note-off comes later) cut there. Returns the sequence and how many notes were cut at `to`.
fn take_sequence(events: &[TakeEvent], tick: impl Fn(Samples) -> i64, from: i64, to: i64) -> (Sequence, usize) {
    let mut seq = Sequence::default();
    let mut held: BTreeMap<(u8, u8), (i64, u8)> = BTreeMap::new();
    let mut cut = 0;
    let close = |seq: &mut Sequence, (ch, pitch): (u8, u8), (start, vel): (i64, u8), end: i64, release: u8| {
        seq.notes.push(Note { pitch, velocity: vel.clamp(1, 127), release_velocity: release, channel: ch, start, length: (end - start).max(1) });
    };
    for e in events {
        let t = tick(e.at);
        let [status, d1, d2] = e.bytes;
        let (kind, ch, d1, d2) = (status & 0xF0, status & 0x0F, d1 & 0x7F, d2 & 0x7F);
        match kind {
            0x90 if d2 > 0 => {
                if t < from || t >= to {
                    continue;
                }
                if let Some(prev) = held.insert((ch, d1), (t, d2)) {
                    close(&mut seq, (ch, d1), prev, t, 64);
                }
            }
            0x80 | 0x90 => {
                if let Some(on) = held.remove(&(ch, d1)) {
                    if t >= to {
                        cut += 1;
                    }
                    close(&mut seq, (ch, d1), on, t.min(to), if kind == 0x80 { d2 } else { 64 });
                }
            }
            _ if t < from || t >= to => {}
            0xB0 => seq.ctrls.push(CtrlEvent { tick: t, channel: ch, kind: CtrlKind::Cc(d1), value: i32::from(d2) }),
            0xE0 => {
                seq.ctrls.push(CtrlEvent { tick: t, channel: ch, kind: CtrlKind::PitchBend, value: ((i32::from(d2) << 7) | i32::from(d1)) - 8192 })
            }
            0xD0 => seq.ctrls.push(CtrlEvent { tick: t, channel: ch, kind: CtrlKind::ChannelPressure, value: i32::from(d1) }),
            0xA0 => seq.ctrls.push(CtrlEvent { tick: t, channel: ch, kind: CtrlKind::PolyPressure(d1), value: i32::from(d2) }),
            0xC0 => seq.ctrls.push(CtrlEvent { tick: t, channel: ch, kind: CtrlKind::Program, value: i32::from(d1) }),
            _ => {}
        }
    }
    for (key, on) in std::mem::take(&mut held) {
        cut += 1;
        close(&mut seq, key, on, to, 64);
    }
    seq.sort();
    (seq, cut)
}

/// The tick a sample falls on, rounded.
fn tick_of(tempo: &TempoMap, sr: soundcraft_time::SampleRate, at: Samples) -> i64 {
    let t = tempo.samples_to_ticks_f(at, sr).round();
    if t.is_finite() { t.clamp(-1e15, 1e15) as i64 } else { 0 }
}

/// `p`'s `quantize` (default: whether Input Quantize is on).
fn quantize_param(cmd: &str, s: &Session, p: &Value) -> Result<bool> {
    match p.get("quantize") {
        None | Some(Value::Null) => Ok(s.edit.flag("midi.input_quantize")),
        Some(v) => v.as_bool().ok_or_else(|| bad(cmd, "`quantize` must be true or false")),
    }
}

/// What `midi.record_take` does with a take over another clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnOverlap {
    Refuse,
    Replace,
    NewPlaylist,
}

/// `p`'s `on_overlap` (`replace: true` is `replace`; default `refuse`).
fn on_overlap_param(cmd: &str, p: &Value) -> Result<OnOverlap> {
    let replace = replace_param(cmd, p)?;
    match p.get("on_overlap").filter(|v| !v.is_null()) {
        None => Ok(if replace { OnOverlap::Replace } else { OnOverlap::Refuse }),
        Some(v) => match v.as_str() {
            Some("refuse") => Ok(OnOverlap::Refuse),
            Some("replace") => Ok(OnOverlap::Replace),
            Some("new_playlist") => Ok(OnOverlap::NewPlaylist),
            _ => Err(bad(cmd, "`on_overlap` must be refuse, replace or new_playlist")),
        },
    }
}

/// One pass of a take: timeline played without a jump, from `start` to `end`.
struct Pass {
    start: Samples,
    end: Samples,
    events: Vec<TakeEvent>,
}

/// `p`'s passes: its `passes` list, else one pass of its `start`, `end` and `events`.
fn passes_param(e: &Engine, cmd: &str, p: &Value) -> Result<Vec<Pass>> {
    let one = |v: &Value, i: Option<usize>| -> Result<Pass> {
        let (Some(start), Some(end)) = (position_param(e, cmd, v, "start")?, position_param(e, cmd, v, "end")?) else {
            return Err(bad(
                cmd,
                match i {
                    Some(i) => format!("pass {i}: `start` and `end` required"),
                    None => "`start` and `end` (where recording started and stopped) required".into(),
                },
            ));
        };
        if end <= start {
            return Err(bad(cmd, "`end` must come after `start`"));
        }
        Ok(Pass { start, end, events: events_param(cmd, v)? })
    };
    match p.get("passes") {
        None | Some(Value::Null) => Ok(vec![one(p, None)?]),
        Some(Value::Array(list)) if !list.is_empty() && list.len() <= 1_000 => list.iter().enumerate().map(|(i, v)| one(v, Some(i))).collect(),
        Some(_) => Err(bad(cmd, "`passes` must be a list of 1..=1000 {start, end, events}")),
    }
}

/// Where a pass goes: merged into a clip (grown to the range), a new clip on the active
/// playlist, or a new clip on a new playlist.
enum Place {
    Merge(ClipId, Range),
    Clip,
    NewPlaylist,
}

fn record_take(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = RECORD_TAKE;
    let track = track_param(e, id, p, "track")?.ok_or_else(|| bad(id, "`track` required"))?;
    let on_overlap = on_overlap_param(id, p)?;
    let force_playlist = p.get("new_playlist").and_then(Value::as_bool).unwrap_or(false);
    let passes = passes_param(e, id, p)?;
    let s = e.session();
    let tr = s.track(track).ok_or_else(|| bad(id, "no such track"))?;
    if !tr.kind.is_midi() {
        return Err(bad(id, format!("`{}` is not an instrument or MIDI track", tr.name)));
    }
    let name = tr.name.clone();
    let quantize = quantize_param(id, s, p)?;
    let opts = super::event_more::quantize_opts(s);
    let total: usize = passes.iter().map(|x| x.events.len()).sum();
    if total > MAX_EVENTS {
        return Err(bad(id, format!("the take holds {total} events over its passes: at most {MAX_EVENTS} go in one command (split it)")));
    }
    let mut takes: Vec<Value> = Vec::new();
    let (mut notes, mut controllers, mut held) = (0usize, 0usize, 0usize);
    for (i, pass) in passes.iter().enumerate() {
        let s = e.session();
        let (sr, tempo) = (s.sample_rate, &s.tempo);
        // Ticks from the take's own first sample, as the clip will play them.
        let base = tempo.samples_to_ticks(pass.start, sr);
        let t1 = tick_of(tempo, sr, pass.end) - base;
        let (mut seq, held_cut) = take_sequence(&pass.events, |at| tick_of(tempo, sr, at) - base, 0, t1.max(1));
        if seq.notes.is_empty() && seq.ctrls.is_empty() {
            continue;
        }
        let performance = seq.clone();
        if quantize && !seq.notes.is_empty() {
            // On the session's grid: in absolute ticks, back to the take's after.
            seq.notes.iter_mut().for_each(|n| n.start += base);
            soundcraft_midi::ops::quantize(&mut seq.notes, &opts);
            seq.notes.iter_mut().for_each(|n| n.start = (n.start - base).max(0));
            seq.sort();
        }
        // Its real extent: where it was played, and any note quantized past the end.
        let last = seq.notes.iter().map(|n| n.start.saturating_add(1)).max().unwrap_or(0);
        let end = pass.end.max(tempo.tick_to_samples(base.saturating_add(last), sr));
        let span = Range::new(pass.start, end);
        let tr = s.track(track).ok_or_else(|| bad(id, "no such track"))?;
        let under: Vec<&Clip> = tr.clips().iter().filter(|c| c.range().overlaps(&span)).collect();
        let merge_into = match under.as_slice() {
            [c] if s.edit.midi_merge && matches!(c.content, ClipContent::Midi { .. }) => {
                let grown = Range::new(c.start.min(span.start), c.end().max(span.end));
                let blocked = tr.clips().iter().any(|o| o.id != c.id && o.range().overlaps(&grown));
                (!blocked).then_some((c.id, grown))
            }
            _ => None,
        };
        let place = match (force_playlist, merge_into, under.is_empty(), on_overlap) {
            (true, _, _, _) => Place::NewPlaylist,
            (false, Some((cid, grown)), _, _) => Place::Merge(cid, grown),
            (false, None, true, _) | (false, None, false, OnOverlap::Replace) => Place::Clip,
            (false, None, false, OnOverlap::NewPlaylist) => Place::NewPlaylist,
            (false, None, false, OnOverlap::Refuse) => {
                let what = under.iter().map(|c| format!("`{}`", c.name)).collect::<Vec<_>>().join(", ");
                let which = if passes.len() > 1 { format!(" (pass {})", i + 1) } else { String::new() };
                let how = if s.edit.midi_merge {
                    "MIDI Merge overdubs into one MIDI clip with room to grow; pass on_overlap: new_playlist to keep both, or replace to record over them"
                } else {
                    "pass on_overlap: new_playlist to keep both, or replace to record over them, or turn on MIDI Merge (options.midi_merge) to overdub"
                };
                return Err(would_replace(id, format!("the take{which} on `{name}` overlaps {what}: {how}")));
            }
        };
        let (n, c) = (seq.notes.len(), seq.ctrls.len());
        let s = e.session_mut();
        let (clip, merged, span, new_playlist) = match place {
            Place::Merge(cid, grown) => {
                merge(s, cid, grown, base, seq, quantize.then_some(performance));
                (cid, true, grown, false)
            }
            Place::Clip | Place::NewPlaylist => {
                let fresh = matches!(place, Place::NewPlaylist);
                if fresh {
                    copy_playlist(s, track);
                }
                let cid = s.new_clip_id();
                // Placing it clears (and trims) only what lies under its own extent: refused
                // above unless `replace`.
                crate::edit::place_clip(s, track, Clip::midi(cid, name.clone(), span.start, span.len().max(1), seq));
                if quantize {
                    s.midi_originals.insert(cid.0, performance);
                }
                (cid, false, span, fresh)
            }
        };
        let playlist = e.session().track(track).and_then(|t| t.playlist()).map(|pl| pl.name.clone());
        (notes, controllers, held) = (notes + n, controllers + c, held + held_cut);
        takes.push(json!({"clip": clip, "playlist": playlist, "new_playlist": new_playlist, "notes": n, "controllers": c,
            "start": span.start, "end": span.end, "merged": merged, "held_cut": held_cut}));
    }
    let first = passes.first().map_or(0, |x| x.start);
    let last = passes.last().map_or(0, |x| x.end);
    let (Some(top), Some(bottom)) = (takes.last().cloned(), takes.first().cloned()) else {
        let msg = format!("Nothing recorded on `{name}`: nothing was played");
        e.message(msg.clone());
        return Ok(json!({"clip": null, "takes": [], "notes": 0, "controllers": 0, "start": first, "end": last, "merged": false,
            "quantized": false, "held_cut": 0, "playlist": null, "new_playlist": false, "message": msg}));
    };
    let playlist = e.session().track(track).and_then(|t| t.playlist()).map(|pl| pl.name.clone());
    let on_new = takes.iter().any(|t| t["new_playlist"] == true);
    let msg = match &playlist {
        Some(pl) if on_new => {
            format!("Recorded {notes} MIDI note(s) on `{name}`, on the new playlist `{pl}` (the clips under it stay on the playlist before)")
        }
        _ => format!("Recorded {notes} MIDI note(s) on `{name}`"),
    };
    e.message(msg.clone());
    Ok(json!({"clip": top["clip"], "takes": takes, "notes": notes, "controllers": controllers, "start": bottom["start"], "end": top["end"],
        "merged": takes.iter().any(|t| t["merged"] == true), "quantized": quantize, "held_cut": held, "playlist": playlist,
        "new_playlist": on_new, "message": msg}))
}

/// Makes a copy of `track`'s active playlist and makes it active: its clips under new ids,
/// with their played MIDI timing and clip effects, so the track plays on as before and the
/// take placed on it next clears only what lies under its own time. The playlist copied stays
/// as it was.
fn copy_playlist(s: &mut Session, track: TrackId) {
    let Some(clips) = s.track(track).map(|t| t.clips().to_vec()) else { return };
    let mut copied = Vec::with_capacity(clips.len());
    for mut c in clips {
        let (old, new) = (c.id, s.new_clip_id());
        c.id = new;
        if let Some(seq) = s.midi_originals.get(&old.0).cloned() {
            s.midi_originals.insert(new.0, seq);
        }
        let edit = &mut s.edit;
        let (from, bypass) = (format!("clip_fx.{}.", old.0), format!("clip_fx.bypass.{}", old.0));
        let values: Vec<(String, f64)> = edit
            .values
            .range(from.clone()..)
            .take_while(|(k, _)| k.starts_with(&from))
            .map(|(k, v)| (k.replacen(&from, &format!("clip_fx.{}.", new.0), 1), *v))
            .collect();
        edit.values.extend(values);
        if edit.flag(&bypass) {
            edit.set_flag(&format!("clip_fx.bypass.{}", new.0), true);
        }
        copied.push(c);
    }
    if let Some(t) = s.track_mut(track) {
        let mut pl = soundcraft_model::Playlist::new(format!("{}.{:02}", t.name, t.playlists.len() + 1));
        pl.clips = copied;
        pl.sort();
        t.playlists.push(pl);
        t.active_playlist = t.playlists.len() - 1;
    }
}

/// Merges a take (`seq`, in ticks from `base`; `performance`: its notes as played, when
/// quantized) into MIDI clip `cid`, which grows to `grown`.
fn merge(s: &mut Session, cid: ClipId, grown: Range, base: i64, seq: Sequence, performance: Option<Sequence>) {
    let (sr, new_base) = (s.sample_rate, s.tempo.samples_to_ticks(grown.start, s.sample_rate));
    let Some(old_base) = s.find_clip(cid).map(|(_, c)| s.tempo.samples_to_ticks(c.start, sr)) else { return };
    let (shift_old, shift_take) = (old_base - new_base, base - new_base);
    let shifted = |mut q: Sequence, by: i64| {
        q.notes.iter_mut().for_each(|n| n.start += by);
        q.ctrls.iter_mut().for_each(|c| c.tick += by);
        q
    };
    let join = |a: Sequence, b: Sequence| {
        let mut out = shifted(a, shift_old);
        let b = shifted(b, shift_take);
        out.notes.extend(b.notes);
        out.ctrls.extend(b.ctrls);
        out.sort();
        out
    };
    let had_original = s.midi_originals.remove(&cid.0);
    let Some(c) = s.find_clip_mut(cid) else { return };
    let ClipContent::Midi { sequence } = &mut c.content else { return };
    let current = std::mem::take(sequence);
    // The played timing stays restorable when either side was quantized.
    let original = match (had_original, performance) {
        (None, None) => None,
        (old, take) => Some(join(old.unwrap_or_else(|| current.clone()), take.unwrap_or_else(|| seq.clone()))),
    };
    *sequence = join(current, seq);
    c.start = grown.start;
    c.length = grown.len().max(1);
    if let Some(o) = original {
        s.midi_originals.insert(cid.0, o);
    }
}

/// `p`'s `bars`: whole 4/4 bars a slot holds.
fn bars_param(cmd: &str, p: &Value) -> Result<u32> {
    p.get("bars")
        .and_then(Value::as_u64)
        .and_then(|b| u32::try_from(b).ok())
        .filter(|b| (1..=MAX_SLOT_BARS).contains(b))
        .ok_or_else(|| bad(cmd, format!("`bars` must be 1..={MAX_SLOT_BARS} whole bars")))
}

/// The instrument column `p` names, with the slot (required) and track: refused for an audio
/// column, or one that lost its track.
fn instrument_slot(e: &Engine, cmd: &str, p: &Value) -> Result<(usize, usize, TrackId)> {
    let column = column_param(e, cmd, p)?;
    let row = slot_param(cmd, p)?.ok_or_else(|| bad(cmd, "`slot` required (0-based)"))?;
    let s = e.session();
    let track = s
        .live
        .column_track(column, &s.tracks)
        .ok_or_else(|| bad(cmd, format!("Live column {} has lost its track: open View › Session (live.setup) to give it a new one", column + 1)))?;
    if s.live.column_kind(column, &s.tracks) != Some(SlotKind::Midi) {
        return Err(bad(
            cmd,
            format!("Live column {} is an audio column: record MIDI into an instrument column (live.add_column kind: instrument)", column + 1),
        ));
    }
    Ok((column, row, track))
}

fn record_slot(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = RECORD_SLOT;
    let (column, slot, track) = instrument_slot(e, id, p)?;
    let bars = bars_param(id, p)?;
    let replace = replace_param(id, p)?;
    refuse_occupied(e, id, column, slot, replace)?;
    super::live::can_launch(e, id)?;
    let req = crate::LiveRequest::RecordSlot { column, slot, bars, replace };
    e.transport_requests.push(crate::TransportRequest::Live(req));
    Ok(json!({"column": column, "slot": slot, "bars": bars, "track": track, "quantise": e.session().live.quantise.id(),
        "starts_transport": !e.transport.playing}))
}

fn slot_from_take(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = SLOT_FROM_TAKE;
    let (column, slot, track) = instrument_slot(e, id, p)?;
    let bars = bars_param(id, p)?;
    let replace = replace_param(id, p)?;
    let start = position_param(e, id, p, "start")?.ok_or_else(|| bad(id, "`start` (the sample the recording started on) required"))?;
    let events = events_param(id, p)?;
    let s = e.session();
    four_four(id, s)?;
    refuse_occupied(e, id, column, slot, replace)?;
    let quantize = quantize_param(id, s, p)?;
    let (sr, tempo) = (s.sample_rate, &s.tempo);
    let t0 = tick_of(tempo, sr, start);
    let len = i64::from(bars) * SLOT_BAR_BEATS * TICKS_PER_QUARTER;
    let (mut seq, held_cut) = take_sequence(&events, |at| tick_of(tempo, sr, at) - t0, 0, len);
    let end = tempo.tick_to_samples(t0.saturating_add(len), sr);
    if seq.notes.is_empty() {
        let msg = format!("Nothing recorded into slot {} of Live column {}: no notes were played", slot + 1, column + 1);
        e.message(msg.clone());
        return Ok(json!({"column": column, "slot": slot, "notes": 0, "bars": bars, "start": start, "end": end, "quantized": false,
            "held_cut": 0, "message": msg}));
    }
    if quantize {
        soundcraft_midi::ops::quantize(&mut seq.notes, &super::event_more::quantize_opts(s));
        // A note pulled onto the loop's end plays on its downbeat: the loop goes round.
        for n in &mut seq.notes {
            n.start = n.start.rem_euclid(len);
            n.length = n.length.min(len - n.start).max(1);
        }
    }
    let tr = s.track(track);
    let (name, color) = (tr.map_or_else(|| "Take".to_string(), |t| format!("{} take", t.name)), tr.map_or([200, 60, 60], |t| t.color));
    let x = MidiSlot::new(name, color, seq, f64::from(bars) * SLOT_BAR_BEATS as f64, 0.0);
    let notes = x.sequence.notes.len();
    let length_beats = x.length_beats;
    super::live::put(e, column, slot, x);
    e.message(format!("Recorded {notes} MIDI note(s) into slot {} of Live column {}", slot + 1, column + 1));
    Ok(json!({"column": column, "slot": slot, "notes": notes, "bars": bars, "length_beats": length_beats, "start": start, "end": end,
        "quantized": quantize, "held_cut": held_cut}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ev(at: Samples, bytes: [u8; 3]) -> TakeEvent {
        TakeEvent { at, bytes }
    }

    #[test]
    fn notes_pair_and_repeats_and_held_notes_end_where_they_must() {
        let events = [
            ev(10, [0x90, 60, 100]),
            ev(20, [0x90, 60, 90]),   // repeated: ends the first
            ev(30, [0x80, 60, 40]),   // ends the second
            ev(40, [0x91, 62, 80]),   // held to the end
            ev(45, [0xB1, 1, 64]),    // a controller
            ev(50, [0x90, 64, 0]),    // a note-off nobody played
            ev(-5, [0x90, 65, 100]),  // before the take
            ev(200, [0x90, 66, 100]), // after it
        ];
        let (seq, cut) = take_sequence(&events, |at| at, 0, 100);
        let got: Vec<_> = seq.notes.iter().map(|n| (n.pitch, n.channel, n.start, n.length, n.velocity)).collect();
        assert_eq!(got, [(60, 0, 10, 10, 100), (60, 0, 20, 10, 90), (62, 1, 40, 60, 80)]);
        assert_eq!(seq.notes[1].release_velocity, 40);
        assert_eq!(cut, 1);
        assert_eq!(seq.ctrls, [CtrlEvent { tick: 45, channel: 1, kind: CtrlKind::Cc(1), value: 64 }]);
    }
}
