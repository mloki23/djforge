//! Production over a DJ set: "parts" are instrument tracks with a built-in instrument set to one
//! of the trance presets in [`super::prod_presets`] (`prod.new_part`, `prod.presets`). Any
//! hosted plugin can still go on a part through `mix.instrument` and the insert commands.
//! `prod.pattern` writes a starter pattern onto a part over whole bars, in the key the set plays
//! there, or into a MIDI slot of an instrument column (the Session view).

use super::dj::key::key_playing;
use super::dj::recipe::bar_at_or_after;
use super::prod_presets::{PRESETS, default_preset, preset};
use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_midi::Key;
use soundcraft_midi::patterns::{self, AutoParam as Sweep, MAX_BARS, PatternKind, PatternSpec, PatternStyle};
use soundcraft_model::{AutoParam, ChannelFormat, Clip, Session, TrackKind};
use soundcraft_time::{Range, Samples};
use std::collections::HashSet;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "prod.new_part",
            "New Part",
            [],
            None,
            "{kind: drums|bass|synth|fx, preset?: id (prod.presets; default trance_kit, rolling_bass, stab, noise_riser), name?} — an instrument track after the DJ decks (Deck A/B), named \"Drums 1\", \"Bass 2\"… unless `name` is given, with the preset's built-in instrument and parameters. One undo step. Returns {track, name, preset, instrument}",
            always,
            new_part
        ),
        cmd!(
            "prod.pattern",
            "Starter Pattern",
            [],
            None,
            "{kind: drums|bass|synth|fx, style (drums: four_floor, trance_full, rolling_perc, fill; bass: rolling, offbeat, root_fifth; synth: stabs, arp; fx: riser, downlifter, impact), track? (default: the selected part), start?, end? (default: the timeline selection), bars? (without a selection: this many bars from the one holding the playhead), key? (bass and synth; default: the key the set plays at the start, as dj.key_at), replace?: bool (default false), seed?, slot?: {column, slot} (0-based: an instrument column's slot instead of a clip)} — a new MIDI clip of the style over the range snapped outward to whole 4/4 bars, named \"<Style> (<camelot>)\". Refuses when the bars overlap a clip on the track, or when the pattern's sweep would overwrite instrument:cutoff or instrument:level points inside them, unless `replace` (which clears the clips under them, and the cutoff and level points inside them that the new pattern doesn't sweep, so an old sweep doesn't keep playing under a drum pattern or an impact). FX sweeps automate the instrument's cutoff (exponentially over its range) and level (as gain under the part's own level) over the bars, leaving the lanes as they were outside them. A sweep written onto an empty lane makes that param follow automation over the whole timeline: outside the bars it holds the part's setting at the time, and the knob no longer changes it (edit the lane instead). With the track's automation off, the sweep doesn't play. One undo step. Returns {clip, track, key, camelot, bars, notes, start, end, automation: [lane ids written], automation_cleared: [lane ids cleared]}. With `slot`, it writes a looping MIDI slot of `bars` (default 4, at most 256) bars instead, in the key at the playhead (or `key`), named the same; `track`, `start` and `end` don't apply, an occupied slot is refused unless `replace`, and FX sweeps are left out (slots play no automation). Returns {column, slot, key, camelot, bars, length_beats, notes, automation_dropped: [params]}",
            always,
            pattern
        ),
        cmd!(query "prod.presets", "Part Presets", [], None, "{kind?: drums|bass|synth|fx} — the presets prod.new_part takes: [{id, kind, instrument}]", always, presets),
    ]
}

/// Fields `prod.new_part` reads; anything else is refused rather than ignored.
const FIELDS: [&str; 3] = ["kind", "preset", "name"];

/// The part kind in `p`'s `kind`, if given.
fn kind_param(cmd: &str, p: &Value) -> Result<Option<PatternKind>> {
    match p.get("kind") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let k = v.as_str().ok_or_else(|| bad(cmd, "`kind` must be drums, bass, synth or fx"))?;
            PatternKind::parse(k).map(Some).map_err(|_| bad(cmd, format!("unknown kind `{k}`: drums, bass, synth or fx")))
        }
    }
}

/// "`base` n" with the first n from 1 no track is called.
fn numbered_name(s: &Session, base: &str) -> String {
    let taken = |n: &str| s.tracks.iter().any(|t| t.name == n);
    (1u64..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap_or_else(|| base.to_string())
}

/// The DJ deck tracks: those the set's transitions mix between and audio tracks holding
/// beat-locked clips; failing both, tracks named "Deck A" / "Deck B" (as `dj.import_set` names
/// them).
fn deck_tracks(s: &Session) -> Vec<TrackId> {
    let mixed: HashSet<TrackId> = s.dj.transitions.iter().flat_map(|t| [t.out_track, t.in_track]).collect();
    let decks: Vec<TrackId> = s
        .tracks
        .iter()
        .filter(|t| mixed.contains(&t.id) || (t.kind == TrackKind::Audio && t.clips().iter().any(|c| c.warp.is_some())))
        .map(|t| t.id)
        .collect();
    if !decks.is_empty() {
        return decks;
    }
    s.tracks.iter().filter(|t| t.kind == TrackKind::Audio && matches!(t.name.as_str(), "Deck A" | "Deck B")).map(|t| t.id).collect()
}

/// The outermost folder holding `t` that the session has, or `t` itself when it is in none.
fn outermost(s: &Session, t: TrackId) -> TrackId {
    let mut root = t;
    // The depth cap guards against a cyclic folder chain in a hostile session file; a folder id
    // naming no track ends the chain at the last folder that exists.
    for _ in 0..64 {
        match s.track(root).and_then(|tr| tr.folder).filter(|&parent| parent != t && s.track(parent).is_some()) {
            Some(parent) => root = parent,
            None => break,
        }
    }
    root
}

/// Where a new part goes: after the last DJ deck track (after the whole of its outermost folder
/// when it is in one, so the folder's members stay together below it) and after any instrument
/// tracks outside folders right after that (earlier parts), so parts stack in the order they were
/// made. With no decks, `None`: the end, before any master.
fn part_position(s: &Session) -> Option<usize> {
    let decks = deck_tracks(s);
    let last = s.tracks.iter().rposition(|t| decks.contains(&t.id))?;
    let root = outermost(s, s.tracks.get(last)?.id);
    let at = s.track_index(root)?;
    let block = s.tracks.iter().skip(at + 1).take_while(|t| super::track::in_folder(s, t.id, root)).count();
    let end = at + 1 + block;
    let parts = s.tracks.iter().skip(end).take_while(|t| t.kind == TrackKind::Instrument && t.folder.is_none()).count();
    Some(end + parts)
}

fn new_part(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "prod.new_part";
    if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| !FIELDS.contains(&k.as_str()))) {
        return Err(bad(CMD, format!("unknown field `{k}`: a part takes kind, preset and name")));
    }
    let kind = kind_param(CMD, p)?.ok_or_else(|| bad(CMD, "`kind` required: drums, bass, synth or fx"))?;
    let id = match p.get("preset") {
        None | Some(Value::Null) => default_preset(kind),
        Some(v) => v.as_str().ok_or_else(|| bad(CMD, "`preset` must be a preset id (prod.presets)"))?,
    };
    let preset = preset(id).ok_or_else(|| bad(CMD, format!("unknown preset `{id}` (prod.presets lists them)")))?;
    if preset.kind != kind {
        return Err(bad(CMD, format!("`{id}` is a {} preset, not {}", preset.kind.name(), kind.name())));
    }
    let name = match p.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(CMD, "`name` must be a non-empty string"))?),
    };
    let info = soundcraft_dsp::plugin_info(preset.instrument).ok_or_else(|| bad(CMD, format!("the built-in `{}` is missing", preset.instrument)))?;
    let ins = super::instrument::plugin_with(info, preset.params.iter().copied());
    // The track is whole before it joins the session: nothing below can fail.
    let s = e.session_mut();
    let name = name.map_or_else(|| numbered_name(s, kind.label()), str::to_string);
    let mut track = s.make_track(TrackKind::Instrument, ChannelFormat::Stereo, Some(&name));
    track.instrument = Some(ins);
    let (t, name) = (track.id, track.name.clone());
    let at = part_position(s).unwrap_or_else(|| s.before_master()).min(s.tracks.len());
    s.tracks.insert(at, track);
    s.edit.selected_tracks = vec![t];
    Ok(json!({"track": t, "name": name, "preset": preset.id, "instrument": preset.instrument}))
}

/// Fields `prod.pattern` reads; anything else is refused rather than ignored.
const PATTERN_FIELDS: [&str; 10] = ["kind", "style", "track", "start", "end", "bars", "key", "replace", "seed", "slot"];

/// Ticks in a 4/4 bar, the only meter the generators write.
const BAR_TICKS: i64 = 4 * soundcraft_time::TICKS_PER_QUARTER;

/// The bars a pattern covers: `bars` whole 4/4 bars from bar `first`, from `start` to `end` on
/// the timeline.
struct Bars {
    first: i64,
    bars: u32,
    start_tick: i64,
    start: Samples,
    end: Samples,
}

/// The part `prod.pattern` writes on: `track`, else the one selected instrument track.
fn pattern_track(e: &Engine, cmd: &str, p: &Value) -> Result<TrackId> {
    let s = e.session();
    let t = match track_param(e, cmd, p, "track")? {
        Some(t) => t,
        None => {
            let selected: Vec<&soundcraft_model::Track> = s.tracks.iter().filter(|t| s.edit.selected_tracks.contains(&t.id)).collect();
            let parts: Vec<TrackId> = selected.iter().filter(|t| t.kind == TrackKind::Instrument).map(|t| t.id).collect();
            match (parts.as_slice(), selected.first()) {
                ([one], _) => *one,
                // Refused below, naming the track.
                ([], Some(t)) => t.id,
                ([], None) => return Err(bad(cmd, "no track: pass `track` or select a part")),
                _ => return Err(bad(cmd, "several parts are selected: pass `track`")),
            }
        }
    };
    match s.track(t) {
        Some(tr) if tr.kind == TrackKind::Instrument => Ok(t),
        Some(tr) => Err(bad(cmd, format!("`{}` is not an instrument track: patterns go on parts (prod.new_part)", tr.name))),
        None => Err(bad(cmd, "no such track")),
    }
}

/// The range snapped outward to whole bars: the selection (or `start`/`end`), else `bars` from the
/// bar holding the playhead (or `start`). Only 4/4 bars are written.
fn pattern_bars(e: &Engine, cmd: &str, p: &Value, bars: Option<u32>) -> Result<Bars> {
    let sel = range_param(e, cmd, p)?;
    let s = e.session();
    let sr = s.sample_rate;
    let (first, last) = if !sel.is_empty() {
        let first = s.tempo.bar_beat_at(sel.start, sr).bar.max(1);
        (first, bar_at_or_after(s, sel.end).max(first.saturating_add(1)))
    } else if let Some(n) = bars {
        let at = if p.get("start").is_some() { sel.start } else { s.edit.playhead };
        let first = s.tempo.bar_beat_at(at, sr).bar.max(1);
        (first, first.saturating_add(i64::from(n)))
    } else {
        return Err(bad(cmd, "no range: select the bars to fill, or pass `bars` to fill from the bar holding the playhead"));
    };
    let count = u32::try_from(last.saturating_sub(first))
        .ok()
        .filter(|n| (1..=MAX_BARS).contains(n))
        .ok_or_else(|| bad(cmd, format!("a pattern is 1..={MAX_BARS} bars")))?;
    let (start_tick, end_tick) = (s.tempo.bar_start_tick(first), s.tempo.bar_start_tick(last));
    // The meter at the start and any change inside the range must all be 4/4.
    let inside = s.tempo.meters().iter().copied().filter(|m| m.tick > start_tick && m.tick < end_tick);
    for m in std::iter::once(s.tempo.meter_at_tick(start_tick)).chain(inside) {
        if (m.numerator, m.denominator) != (4, 4) {
            let bar = s.tempo.bar_beat_at_tick(m.tick.max(start_tick)).bar;
            return Err(bad(cmd, format!("only 4/4 is supported: bar {bar} is in {}/{}", m.numerator, m.denominator)));
        }
    }
    if end_tick.saturating_sub(start_tick) != i64::from(count) * BAR_TICKS {
        return Err(bad(cmd, "only 4/4 is supported over the range"));
    }
    Ok(Bars { first, bars: count, start_tick, start: s.tempo.tick_to_samples(start_tick, sr), end: s.tempo.tick_to_samples(end_tick, sr) })
}

/// A sweep's value (0..=1) as a value of the parameter it drives.
type SweepMap = Box<dyn Fn(f32) -> f32>;

/// Where a pattern's sweep lands on a built-in instrument: the parameter it drives and how a
/// curve value maps onto it. Cutoff runs exponentially over the parameter's whole range (closed
/// to open); volume is gain under the part's own `level`, so 1 is the level the part is set to
/// and 0.5 is 6 dB under it.
fn sweep_target(curve: Sweep, info: &'static soundcraft_dsp::PluginInfo, level: Option<f32>) -> Option<(&'static str, SweepMap)> {
    match curve {
        Sweep::Cutoff => {
            let pi = info.param("cutoff")?;
            let lo = pi.min.max(1.0);
            let ratio = (pi.max / lo).max(1.0);
            Some(("cutoff", Box::new(move |v: f32| pi.clamp(lo * ratio.powf(v.clamp(0.0, 1.0))))))
        }
        Sweep::Volume => {
            let pi = info.param("level")?;
            let base = level.unwrap_or(pi.default);
            Some(("level", Box::new(move |v: f32| if v > 0.0 { pi.clamp(base + 20.0 * v.min(1.0).log10()) } else { pi.min })))
        }
    }
}

fn pattern(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "prod.pattern";
    if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| !PATTERN_FIELDS.contains(&k.as_str()))) {
        return Err(bad(CMD, format!("unknown field `{k}`: a pattern takes {}", PATTERN_FIELDS.join(", "))));
    }
    let kind = kind_param(CMD, p)?.ok_or_else(|| bad(CMD, "`kind` required: drums, bass, synth or fx"))?;
    let styles = || PatternStyle::of_kind(kind).map(PatternStyle::name).collect::<Vec<_>>().join(", ");
    let style = match p.get("style") {
        None | Some(Value::Null) => return Err(bad(CMD, format!("`style` required: {}", styles()))),
        Some(v) => v
            .as_str()
            .and_then(|x| PatternStyle::parse(x).ok())
            .ok_or_else(|| bad(CMD, format!("unknown style {v}: {} has {}", kind.name(), styles())))?,
    };
    if style.kind() != kind {
        return Err(bad(CMD, format!("`{}` is a {} style: {} has {}", style.name(), style.kind().name(), kind.name(), styles())));
    }
    let bars = match p.get("bars") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=MAX_BARS).contains(n))
                .ok_or_else(|| bad(CMD, format!("`bars` must be 1..={MAX_BARS}")))?,
        ),
    };
    let replace = match p.get("replace") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, "`replace` must be true or false"))?,
    };
    let seed = match p.get("seed") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad(CMD, "`seed` must be a whole number"))?),
    };
    let given_key = match p.get("key") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let text = v.as_str().ok_or_else(|| bad(CMD, "`key` must be a key like Am, F#, 8A or 1m"))?;
            Some(Key::parse(text).map_err(|_| bad(CMD, format!("`{text}` is not a key: try Am, F#, 8A or 1m")))?)
        }
    };
    if let Some(target) = p.get("slot").filter(|v| !v.is_null()) {
        return pattern_slot(
            e,
            p,
            target,
            PatternSpec { kind, style, bars: bars.unwrap_or(SLOT_BARS), beats_per_bar: 4, key: given_key, seed: seed.unwrap_or(0) },
            seed,
            replace,
        );
    }
    let t = pattern_track(e, CMD, p)?;
    let range = pattern_bars(e, CMD, p, bars)?;
    let s = e.session();
    // Only keyed styles use a key: the one given, else the set's at the start.
    let key = match (style.needs_key(), given_key) {
        (false, _) => None,
        (true, Some(k)) => Some(k),
        (true, None) => Some(key_playing(s, range.start, None).ok_or_else(|| bad(CMD, "no key here: pass key or set the track key"))?),
    };
    let tr = s.track(t).ok_or_else(|| bad(CMD, "no such track"))?;
    let span = Range::new(range.start, range.end);
    let under: Vec<String> = tr.clips().iter().filter(|c| c.range().overlaps(&span)).map(|c| format!("`{}`", c.name)).collect();
    // Stable by default: the same part and bars give the same pattern.
    let seed = seed.unwrap_or_else(|| t.0.wrapping_mul(1_000_003).wrapping_add(range.first.unsigned_abs()));
    let spec = PatternSpec { kind, style, bars: range.bars, beats_per_bar: 4, key, seed };
    let g = patterns::generate(&spec).map_err(|err| bad(CMD, err.to_string()))?;
    let name = match key {
        Some(k) => format!("{} ({})", style.title(), k.camelot()),
        None => style.title(),
    };
    let notes = g.sequence.notes.len();
    // Each sweep point lands at its tick on the tempo map, so a ramp bends the curve with the bars.
    let at = |tick: i64| s.tempo.tick_to_samples(range.start_tick.saturating_add(tick), s.sample_rate);
    let ins = tr.instrument.as_ref();
    let info = ins.and_then(|i| soundcraft_dsp::plugin_info(&i.plugin));
    let stored = |k: &str| ins.and_then(|i| i.params.get(k).copied());
    // Lanes that were empty: from now on they, not the knobs, set those params everywhere.
    let mut pinned = Vec::new();
    let mut lanes = Vec::new();
    for curve in &g.automation {
        let Some((param, value)) = info.and_then(|info| sweep_target(curve.param, info, stored("level"))) else { continue };
        let points: Vec<(Samples, f32)> = curve.points.iter().map(|&(tick, v)| (at(tick), value(v))).collect();
        // Outside the range an empty lane holds the part's own setting.
        let base = stored(param).unwrap_or_else(|| value(1.0));
        if tr.lane(&AutoParam::instrument(param)).is_none_or(|l| l.points.is_empty()) {
            pinned.push(param);
        }
        lanes.push((param, points, base));
    }
    // Sweep lanes with points inside the bars: the pattern's own sweeps would overwrite them, and
    // `replace` clears the rest so an old sweep doesn't keep playing under the new pattern.
    let drawn: Vec<&'static str> =
        SWEEP_PARAMS.into_iter().filter(|param| tr.lane(&AutoParam::instrument(param)).is_some_and(|l| drawn_inside(l, span))).collect();
    let writes = |param: &str| lanes.iter().any(|(q, ..)| *q == param);
    let overwritten: Vec<String> = drawn.iter().filter(|p| writes(p)).map(|p| format!("instrument:{p}")).collect();
    let cleared: Vec<&'static str> = drawn.iter().copied().filter(|p| !writes(p)).collect();
    if !replace && (!under.is_empty() || !overwritten.is_empty()) {
        let mut what = Vec::new();
        if !under.is_empty() {
            what.push(format!("the bars overlap {} on `{}`", under.join(", "), tr.name));
        }
        if !overwritten.is_empty() {
            what.push(format!("the sweep would overwrite the {} points inside the bars on `{}`", overwritten.join(" and "), tr.name));
        }
        let also = if cleared.is_empty() {
            String::new()
        } else {
            format!(" (it also clears the instrument:{} points there)", cleared.join(" and instrument:"))
        };
        return Err(would_replace(CMD, format!("{}: pass replace: true to replace what is there{also}", what.join("; "))));
    }
    let cleared = if replace { cleared } else { Vec::new() };
    let mut notices = Vec::new();
    if !cleared.is_empty() {
        notices.push(format!("{}: cleared the {} sweep inside the bars", tr.name, cleared.join("/")));
    }
    // What a cleared lane holds where it has no points (it always has some outside the bars).
    let cleared: Vec<(&'static str, f32)> = cleared
        .into_iter()
        .map(|param| (param, stored(param).or_else(|| info.and_then(|i| i.param(param)).map(|pi| pi.default)).unwrap_or(0.0)))
        .collect();
    if !pinned.is_empty() {
        notices.push(format!("{}'s {} now follow automation; edit the lane to change them", tr.name, pinned.join("/")));
    }
    if !lanes.is_empty() && !tr.mixer.automation_mode.reads() {
        notices.push(format!("{}'s automation is off, so its {} sweep won't play until the track reads automation again", tr.name, style.title()));
    }
    if !g.automation.is_empty() && lanes.is_empty() {
        let what = ins.map_or("the part has no instrument", |i| i.plugin.as_str());
        e.message(format!("{}: {what} has no cutoff or level to sweep, so only the notes were written", style.title()));
    }
    for n in notices {
        e.message(n);
    }
    let s = e.session_mut();
    let cid = s.new_clip_id();
    // Placing the clip clears (and trims) whatever was under it: refused above unless `replace`.
    crate::edit::place_clip(s, t, Clip::midi(cid, name, range.start, range.end.saturating_sub(range.start), g.sequence));
    let mut automation = Vec::new();
    if let Some(tr) = s.track_mut(t) {
        for (param, points, base) in lanes {
            // The curve's last point sits on the range end (tick == length_ticks), so the span is
            // inclusive: the sweep peaks exactly on the bar line, and the anchor one sample later
            // returns to what was there before.
            tr.lane_mut(&AutoParam::instrument(param)).write_curve(range.start, range.end, &points, base);
            automation.push(format!("instrument:{param}"));
        }
        for &(param, base) in &cleared {
            // Nothing written in place of the old points: the curve joins what is either side.
            tr.lane_mut(&AutoParam::instrument(param)).write_curve(range.start, range.end, &[], base);
        }
    }
    Ok(json!({
        "clip": cid,
        "track": t,
        "key": key.map(|k| k.name()),
        "camelot": key.map(|k| k.camelot()),
        "bars": range.bars,
        "notes": notes,
        "start": range.start,
        "end": range.end,
        "automation": automation,
        "automation_cleared": cleared.iter().map(|(param, _)| format!("instrument:{param}")).collect::<Vec<_>>(),
    }))
}

/// The instrument params a pattern's sweeps drive (as [`sweep_target`] maps them).
const SWEEP_PARAMS: [&str; 2] = ["cutoff", "level"];

/// Whether `lane` has points strictly inside `span`, past the one-sample anchors a sweep on the
/// neighbouring bars leaves just inside its bar lines (those hold the curve either side, and a
/// sweep over `span` keeps them so).
fn drawn_inside(lane: &soundcraft_model::AutomationLane, span: Range) -> bool {
    let (from, to) = (span.start.saturating_add(1), span.end.saturating_sub(1));
    lane.points.iter().any(|p| p.at > from && p.at < to)
}

/// Bars a slot pattern fills when `bars` isn't given.
const SLOT_BARS: u32 = 4;

/// `prod.pattern` into a MIDI slot: `target` is `{column, slot}`, `spec` the pattern asked for
/// (its key the one given, if any; its seed `seed`, else one stable for the slot).
fn pattern_slot(e: &mut Engine, p: &Value, target: &Value, mut spec: PatternSpec, seed: Option<u64>, replace: bool) -> Result<Value> {
    const CMD: &str = "prod.pattern";
    if let Some(k) = ["track", "start", "end"].iter().find(|k| p.get(**k).is_some_and(|v| !v.is_null())) {
        return Err(bad(CMD, format!("`{k}` is for clips: a slot pattern takes `bars` (default {SLOT_BARS})")));
    }
    if spec.bars > soundcraft_model::MAX_SLOT_BARS {
        return Err(bad(CMD, format!("a slot holds at most {} bars", soundcraft_model::MAX_SLOT_BARS)));
    }
    let column = super::live::column_param(e, CMD, target)?;
    super::live::takes(e, CMD, column, soundcraft_model::SlotKind::Midi)?;
    let row = super::live::slot_param(CMD, target)?.ok_or_else(|| bad(CMD, "`slot` needs {column, slot} (0-based)"))?;
    let s = e.session();
    super::live::four_four(CMD, s)?;
    if let Some(x) = s.live.slot(column, row).filter(|_| !replace) {
        return Err(would_replace(
            CMD,
            format!("slot {} of Live column {} holds `{}`: pass replace: true to replace it", row + 1, column + 1, x.name()),
        ));
    }
    if spec.style.needs_key() && spec.key.is_none() {
        spec.key = Some(key_playing(s, s.edit.playhead, None).ok_or_else(|| bad(CMD, "no key at the playhead: pass key or set the track key"))?);
    }
    if !spec.style.needs_key() {
        spec.key = None;
    }
    let track = s.live.columns.get(column).map_or(0, |c| c.track.0);
    spec.seed = seed.unwrap_or_else(|| track.wrapping_mul(1_000_003).wrapping_add((column * 8 + row) as u64));
    let g = patterns::generate(&spec).map_err(|err| bad(CMD, err.to_string()))?;
    let dropped: Vec<&str> = g
        .automation
        .iter()
        .map(|c| match c.param {
            Sweep::Cutoff => "cutoff",
            Sweep::Volume => "level",
        })
        .collect();
    if !dropped.is_empty() {
        e.message(format!("{}: slots play no automation, so its {} sweep was left out", spec.style.title(), dropped.join("/")));
    }
    let name = match spec.key {
        Some(k) => format!("{} ({})", spec.style.title(), k.camelot()),
        None => spec.style.title(),
    };
    let color = e.session().live.column_track(column, &e.session().tracks).and_then(|t| e.session().track(t)).map_or([128, 128, 128], |t| t.color);
    let length_beats = f64::from(spec.bars) * 4.0;
    let slot = soundcraft_model::MidiSlot::new(name, color, g.sequence, length_beats, 0.0);
    let notes = slot.sequence.notes.len();
    super::live::put(e, column, row, slot);
    Ok(json!({
        "column": column,
        "slot": row,
        "key": spec.key.map(|k| k.name()),
        "camelot": spec.key.map(|k| k.camelot()),
        "bars": spec.bars,
        "length_beats": length_beats,
        "notes": notes,
        "automation_dropped": dropped,
    }))
}

fn presets(_: &mut Engine, p: &Value) -> Result<Value> {
    let kind = kind_param("prod.presets", p)?;
    let list: Vec<Value> = PRESETS
        .iter()
        .filter(|x| kind.is_none_or(|k| x.kind == k))
        .map(|x| json!({"id": x.id, "kind": x.kind.name(), "instrument": x.instrument}))
        .collect();
    Ok(Value::Array(list))
}

#[cfg(test)]
mod tests {
    use super::super::prod_presets::PRESETS;
    use crate::Engine;
    use serde_json::{Value, json};
    use soundcraft_midi::patterns::{CLAP, CLOSED_HAT, FX_PITCH, KICK, OPEN_HAT};
    use soundcraft_midi::{Note, Sequence};
    use soundcraft_model::{ChannelFormat, Clip, ClipId, SourceId, TrackId, TrackKind, Transition, WarpMap};
    use soundcraft_time::{Range, TICKS_PER_QUARTER};

    fn part(e: &mut Engine, p: Value) -> Value {
        e.execute("prod.new_part", &p).unwrap()
    }

    fn track_of(r: &Value) -> TrackId {
        TrackId(r["track"].as_u64().unwrap())
    }

    fn names(e: &Engine) -> Vec<String> {
        e.session().tracks.iter().map(|t| t.name.clone()).collect()
    }

    #[test]
    fn each_kind_gets_its_instrument_with_the_presets_parameters() {
        for x in PRESETS {
            let mut e = Engine::default();
            let r = part(&mut e, json!({"kind": x.kind.name(), "preset": x.id}));
            assert_eq!((r["preset"].as_str(), r["instrument"].as_str()), (Some(x.id), Some(x.instrument)));
            let tr = e.session().track(track_of(&r)).unwrap();
            assert_eq!(tr.kind, TrackKind::Instrument);
            let ins = tr.instrument.as_ref().unwrap();
            assert_eq!(ins.plugin, x.instrument);
            for (k, v) in x.params {
                assert_eq!(ins.params.get(*k).copied(), Some(*v), "{}: {k}", x.id);
            }
            let info = soundcraft_dsp::plugin_info(x.instrument).unwrap();
            assert_eq!(ins.params.len(), info.params.len(), "{}: the rest at their defaults", x.id);
            assert_eq!(e.session().edit.selected_tracks, vec![tr.id], "the new part is selected");
        }
    }

    #[test]
    fn a_part_without_a_preset_gets_its_kinds_default() {
        let mut e = Engine::default();
        for (kind, preset, plugin) in [
            ("drums", "trance_kit", "drum_synth"),
            ("bass", "rolling_bass", "subtractive_synth"),
            ("synth", "stab", "subtractive_synth"),
            ("fx", "noise_riser", "subtractive_synth"),
        ] {
            let r = part(&mut e, json!({"kind": kind}));
            assert_eq!(r["preset"], preset);
            assert_eq!(r["instrument"], plugin);
        }
    }

    #[test]
    fn parts_are_numbered_per_kind_and_a_given_name_is_kept_unique() {
        let mut e = Engine::default();
        let got: Vec<Value> = ["drums", "bass", "bass", "fx", "drums"].iter().map(|k| part(&mut e, json!({"kind": k}))["name"].clone()).collect();
        assert_eq!(got, ["Drums 1", "Bass 1", "Bass 2", "FX 1", "Drums 2"].map(Value::from));
        // A deleted part's number is taken again; a given name is used as is, made unique if taken.
        e.execute("track.delete", &json!({"tracks": ["Bass 1"]})).unwrap();
        assert_eq!(part(&mut e, json!({"kind": "bass"}))["name"], "Bass 1");
        assert_eq!(part(&mut e, json!({"kind": "synth", "name": "Hoover"}))["name"], "Hoover");
        let again = part(&mut e, json!({"kind": "synth", "name": "Hoover"}))["name"].as_str().unwrap().to_string();
        assert_ne!(again, "Hoover");
        let all = names(&e);
        assert_eq!(all.len(), all.iter().collect::<std::collections::HashSet<_>>().len(), "{all:?}");
    }

    #[test]
    fn parts_go_after_the_dj_decks_in_the_order_they_are_made() {
        let mut e = Engine::default();
        let s = e.session_mut();
        s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck A"));
        s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck B"));
        s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Vocal"));
        s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
        part(&mut e, json!({"kind": "drums"}));
        part(&mut e, json!({"kind": "bass"}));
        let all = names(&e);
        assert_eq!(&all[..5], ["Deck A", "Deck B", "Drums 1", "Bass 1", "Vocal"], "{all:?}");
        // With no decks, a part goes at the end (before the master).
        let mut e = Engine::default();
        e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Pad"));
        e.session_mut().add_track(TrackKind::Master, ChannelFormat::Stereo, None);
        part(&mut e, json!({"kind": "synth"}));
        let all = names(&e);
        assert_eq!(all[..2], ["Pad", "Synth 1"], "{all:?}");
        assert_eq!(e.session().tracks.last().map(|t| t.kind), Some(TrackKind::Master));
    }

    #[test]
    fn a_folder_id_naming_no_track_ends_the_folder_walk() {
        let mut e = Engine::default();
        let (a, _) = deck(&mut e, "Deck A", true);
        deck(&mut e, "Loose", false);
        if let Some(t) = e.session_mut().track_mut(a) {
            t.folder = Some(TrackId(9_999));
        }
        part(&mut e, json!({"kind": "bass"}));
        assert_eq!(names(&e), ["Deck A", "Bass 1", "Loose"]);
    }

    /// A noise riser part and an impact part, each holding a note over the first two seconds.
    fn noisy_session() -> Engine {
        let mut e = Engine::default();
        for (kind, preset) in [("fx", "noise_riser"), ("fx", "impact")] {
            let t = track_of(&part(&mut e, json!({"kind": kind, "preset": preset})));
            let mut seq = Sequence::default();
            seq.notes.push(Note { pitch: FX_PITCH, velocity: 120, release_velocity: 64, channel: 0, start: 0, length: 4 * TICKS_PER_QUARTER });
            let s = e.session_mut();
            let cid = s.new_clip_id();
            crate::edit::place_clip(s, t, Clip::midi(cid, preset, 0, 96_000, seq));
        }
        e
    }

    #[test]
    fn noise_bounces_repeat_and_a_normalised_bounce_stays_under_its_ceiling() {
        let dir = std::env::temp_dir().join(format!("sc-prod-bounce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bounce = |name: &str, normalize: bool| {
            let mut e = noisy_session();
            let path = dir.join(name);
            let r = e
                .execute(
                    "file.bounce_mix",
                    &json!({"path": path.to_string_lossy(), "bit_depth": "32f", "start": 0, "end": 96_000, "normalize": normalize}),
                )
                .unwrap();
            let (_, buf) = soundcraft_audio_io::decode(&std::fs::read(&path).unwrap(), Some("wav")).unwrap();
            (r, buf.channels)
        };
        let (_, first) = bounce("a.wav", false);
        let (_, second) = bounce("b.wav", false);
        assert!(first.iter().flatten().any(|x| x.abs() > 0.01), "the noise sounds");
        assert_eq!(first, second, "the same session bounces the same noise");
        // The second pass plays exactly what the first measured, so the peak lands on -0.1 dBFS.
        let (r, normalised) = bounce("n.wav", true);
        let peak = normalised.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
        let ceiling = soundcraft_dsp::db_to_gain(-0.1);
        assert!(peak <= ceiling * 1.000_01, "normalised peak {peak} over {ceiling} ({r})");
        assert!(peak >= ceiling * 0.999, "normalised peak {peak} short of {ceiling}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audio track `name` holding one clip, beat-locked when `warped`.
    fn deck(e: &mut Engine, name: &str, warped: bool) -> (TrackId, ClipId) {
        let s = e.session_mut();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
        let c = s.new_clip_id();
        let mut clip = Clip::audio(c, name, SourceId(90), 0, 0, 48_000);
        if warped {
            clip.warp = WarpMap::new(vec![0.0, 0.5, 1.0], 0.0);
            assert!(clip.warp.is_some());
        }
        crate::edit::place_clip(s, t, clip);
        (t, c)
    }

    #[test]
    fn decks_are_the_tracks_a_transition_mixes_whatever_they_are_called() {
        let mut e = Engine::default();
        let (left, lc) = deck(&mut e, "Left", false);
        let (right, rc) = deck(&mut e, "Right", false);
        deck(&mut e, "Deck A", false);
        let x: Transition =
            serde_json::from_value(json!({"id": 1, "out_clip": lc.0, "in_clip": rc.0, "out_track": left.0, "in_track": right.0})).unwrap();
        e.session_mut().dj.transitions.push(x);
        part(&mut e, json!({"kind": "bass"}));
        let all = names(&e);
        assert_eq!(all, ["Left", "Right", "Bass 1", "Deck A"], "a track called Deck A is not a deck when the set says otherwise");
    }

    #[test]
    fn a_track_with_a_beat_locked_clip_is_a_deck_without_a_transition() {
        let mut e = Engine::default();
        deck(&mut e, "Deck A", false);
        deck(&mut e, "Locked", true);
        deck(&mut e, "Loose", false);
        part(&mut e, json!({"kind": "fx"}));
        assert_eq!(names(&e), ["Deck A", "Locked", "FX 1", "Loose"]);
    }

    #[test]
    fn a_part_goes_below_the_whole_folder_its_last_deck_is_in() {
        let mut e = Engine::default();
        deck(&mut e, "Deck A", true);
        deck(&mut e, "Deck B", true);
        deck(&mut e, "After", false);
        e.execute("track.move_to_new_folder", &json!({"tracks": ["Deck A", "Deck B"], "name": "Set"})).unwrap();
        e.execute("track.new", &json!({"kind": "folder", "name": "Inner"})).unwrap();
        e.execute("track.move_to_folder", &json!({"tracks": ["Inner"], "folder": "Set"})).unwrap();
        let before = names(&e);
        assert_eq!(before, ["Set", "Deck A", "Deck B", "Inner", "After"]);
        part(&mut e, json!({"kind": "drums"}));
        part(&mut e, json!({"kind": "synth"}));
        assert_eq!(names(&e), ["Set", "Deck A", "Deck B", "Inner", "Drums 1", "Synth 1", "After"]);
        let s = e.session();
        assert!(s.tracks.iter().filter(|t| t.name.ends_with(" 1")).all(|t| t.folder.is_none()), "parts stay out of the folder");
    }

    #[test]
    fn refuses_unknown_kinds_presets_fields_and_a_preset_of_another_kind() {
        let mut e = Engine::default();
        for bad in [
            json!({}),
            json!({"kind": "vocals"}),
            json!({"kind": 3}),
            json!({"kind": "bass", "preset": "nope"}),
            json!({"kind": "bass", "preset": "trance_kit"}),
            json!({"kind": "drums", "preset": "stab"}),
            json!({"kind": "fx", "preset": 2}),
            json!({"kind": "bass", "name": ""}),
            json!({"kind": "bass", "cutoff": 200}),
        ] {
            assert!(e.execute("prod.new_part", &bad).is_err(), "{bad}");
        }
        assert!(e.session().tracks.is_empty(), "nothing was made");
        assert!(!e.can_undo(), "nothing was recorded");
        let err = e.execute("prod.new_part", &json!({"kind": "bass", "preset": "trance_kit"})).unwrap_err().to_string();
        assert!(err.contains("drums"), "{err}");
    }

    #[test]
    fn one_undo_removes_the_part() {
        let mut e = Engine::default();
        e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck A"));
        let before = names(&e);
        part(&mut e, json!({"kind": "drums"}));
        assert_eq!(e.session().tracks.len(), 2);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(names(&e), before);
        assert!(e.journal.iter().any(|(id, _)| id == "prod.new_part"), "journaled");
    }

    #[test]
    fn presets_list_every_preset_or_one_kinds() {
        let mut e = Engine::default();
        let all = e.execute("prod.presets", &json!({})).unwrap();
        assert_eq!(all.as_array().map(Vec::len), Some(PRESETS.len()));
        let fx = e.execute("prod.presets", &json!({"kind": "fx"})).unwrap();
        let ids: Vec<&str> = fx.as_array().unwrap().iter().filter_map(|x| x["id"].as_str()).collect();
        assert_eq!(ids, ["noise_riser", "downlifter", "impact"]);
        assert!(fx.as_array().unwrap().iter().all(|x| x["kind"] == "fx" && x["instrument"] == "subtractive_synth"));
        let drums = e.execute("prod.presets", &json!({"kind": "drums"})).unwrap();
        assert_eq!(drums, json!([{"id": "trance_kit", "kind": "drums", "instrument": "drum_synth"}]));
        assert!(e.execute("prod.presets", &json!({"kind": "vocals"})).is_err());
        assert!(!e.can_undo(), "a query");
    }

    /// `notes` (pitch, start and length in 8th notes) on a part with `preset` at the session's
    /// 120 bpm, a beat in, rendered for `secs` more: the peak over the render and the RMS while
    /// the notes are held, both in dBFS over both channels. The beat of silence first lets the
    /// instrument's level settle from its default, as it has long since in a playing session.
    fn render(kind: &str, preset: &str, notes: &[(u8, i64, i64)], secs: i64) -> (f32, f32) {
        let q = TICKS_PER_QUARTER;
        let mut e = Engine::default();
        let t = track_of(&e.execute("prod.new_part", &json!({"kind": kind, "preset": preset})).unwrap());
        let mut seq = Sequence::default();
        for &(pitch, start, eighths) in notes {
            seq.notes.push(Note { pitch, velocity: 110, release_velocity: 64, channel: 0, start: (start + 2) * q / 2, length: eighths * q / 2 });
        }
        let s = e.session_mut();
        let cid = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::midi(cid, "audition", 0, 24_000 + 48_000 * secs, seq));
        let out = soundcraft_mix::render_range(e.session(), Range::new(0, 24_000 + 48_000 * secs), 512);
        let peak = out.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
        // An 8th note is a quarter of a second at 120 bpm.
        let first = notes.iter().map(|n| n.1).min().unwrap_or(0) as usize * 12_000 + 24_000;
        let last = notes.iter().map(|n| n.1 + n.2).max().unwrap_or(0) as usize * 12_000 + 24_000;
        let during = || out.iter().flat_map(|c| c.iter().take(last).skip(first));
        let n = during().count().max(1) as f32;
        let rms = (during().map(|x| x * x).sum::<f32>() / n).sqrt();
        let db = |x: f32| 20.0 * x.max(1.0e-9).log10();
        (db(peak), db(rms))
    }

    #[test]
    fn every_preset_sounds_without_clipping() {
        // One bar of the trance groove for the kit; one note for the rest, held as its patterns
        // hold it (a beat of bass or synth, four bars of riser or downlifter, a beat of impact).
        let mut bar = Vec::new();
        for beat in 0..4 {
            bar.extend([(KICK, 2 * beat, 1), (CLOSED_HAT, 2 * beat, 1), (OPEN_HAT, 2 * beat + 1, 1)]);
        }
        bar.extend([(CLAP, 2, 1), (CLAP, 6, 1)]);
        let mut failed = Vec::new();
        for x in PRESETS {
            let (notes, secs): (Vec<(u8, i64, i64)>, i64) = match (x.kind.name(), x.id) {
                ("drums", _) => (bar.clone(), 2),
                ("bass", _) => (vec![(45, 0, 2)], 1),
                ("synth", _) => (vec![(69, 0, 2)], 1),
                (_, "impact") => (vec![(FX_PITCH, 0, 2)], 3),
                _ => (vec![(FX_PITCH, 0, 32)], 9),
            };
            let (peak, rms) = render(x.kind.name(), x.id, &notes, secs);
            println!("{:>13}: peak {peak:6.1} dBFS, rms {rms:6.1} dBFS", x.id);
            if peak >= 0.0 || !(-40.0..=-6.0).contains(&rms) {
                failed.push(x.id);
            }
        }
        assert!(failed.is_empty(), "clipped (peak >= 0 dBFS), nearly silent (rms < -40 dBFS) or too hot (rms > -6 dBFS): {failed:?}");
    }

    // ---- prod.pattern -------------------------------------------------------------------------

    use super::super::dj::recipe::bar_sample;
    use soundcraft_model::{AutoParam, ClipContent, Session};
    use soundcraft_time::Samples;

    fn bar(e: &Engine, n: i64) -> Samples {
        bar_sample(e.session(), n)
    }

    /// Selects from `from` to `to` on the timeline.
    fn select(e: &mut Engine, from: Samples, to: Samples) {
        e.session_mut().edit.selection = Range::new(from, to);
    }

    fn pattern(e: &mut Engine, p: Value) -> Value {
        e.execute("prod.pattern", &p).unwrap()
    }

    fn clip_of(e: &Engine, r: &Value) -> Clip {
        e.session().find_clip(ClipId(r["clip"].as_u64().unwrap())).unwrap().1.clone()
    }

    fn sequence(c: &Clip) -> &Sequence {
        match &c.content {
            ClipContent::Midi { sequence } => sequence,
            _ => panic!("not a MIDI clip"),
        }
    }

    #[test]
    fn a_pattern_over_a_selection_under_a_tempo_ramp_starts_and_ends_on_bar_lines() {
        let mut e = Engine::default();
        e.execute("dj.tempo_ramp", &json!({"from_bar": 1, "bars": 8, "from_bpm": 128.0, "to_bpm": 138.0})).unwrap();
        let t = track_of(&part(&mut e, json!({"kind": "drums"})));
        // Bar 2 beat 2 to bar 5 beat 3 snaps outward to bars 2..6.
        let beat = |e: &Engine, n: i64, b: i64| {
            let s = e.session();
            s.tempo.tick_to_samples(s.tempo.bar_start_tick(n) + (b - 1) * TICKS_PER_QUARTER, s.sample_rate)
        };
        let (from, to) = (beat(&e, 2, 2), beat(&e, 5, 3));
        select(&mut e, from, to);
        let r = pattern(&mut e, json!({"kind": "drums", "style": "trance_full"}));
        assert_eq!((r["start"].as_i64(), r["end"].as_i64(), r["bars"].as_u64()), (Some(bar(&e, 2)), Some(bar(&e, 6)), Some(4)), "{r}");
        assert_eq!((r["key"].clone(), r["camelot"].clone(), r["track"].as_u64()), (Value::Null, Value::Null, Some(t.0)));
        let c = clip_of(&e, &r);
        assert_eq!((c.name.as_str(), c.start, c.end()), ("Trance Full", bar(&e, 2), bar(&e, 6)));
        let s = e.session();
        assert_eq!(s.tempo.samples_to_ticks(c.end(), s.sample_rate) - s.tempo.samples_to_ticks(c.start, s.sample_rate), 4 * 3840);
        let seq = sequence(&c);
        assert_eq!(r["notes"].as_u64(), Some(seq.notes.len() as u64));
        assert!(seq.notes.iter().all(|n| n.start >= 0 && n.start + n.length <= 4 * 3840));
        assert!(seq.notes.iter().any(|n| n.start == 0) && seq.notes.iter().any(|n| n.start == 15 * 960), "a kick on every beat");
        // The same part and bars give the same pattern again.
        let first = seq.clone();
        e.execute("edit.undo", &json!({})).unwrap();
        let r = pattern(&mut e, json!({"kind": "drums", "style": "trance_full", "track": t.0}));
        let again = clip_of(&e, &r);
        assert_eq!(*sequence(&again), first);
    }

    #[test]
    fn without_a_selection_bars_count_from_the_bar_holding_the_playhead() {
        let mut e = Engine::default();
        part(&mut e, json!({"kind": "drums"}));
        e.session_mut().edit.playhead = bar(&e, 3) + 1_000;
        let r = pattern(&mut e, json!({"kind": "drums", "style": "four_floor", "bars": 2}));
        assert_eq!((r["start"].as_i64(), r["end"].as_i64()), (Some(bar(&e, 3)), Some(bar(&e, 5))));
        assert_eq!(r["notes"], 8);
    }

    /// A 128 bpm set: beat-locked deck A (bars 1..45, key `a`) and deck B (bars 25..65, key `b`)
    /// mixed by a transition with its hit on bar 41, as the `dj.key_at` tests build it.
    fn set(a: &str, b: &str) -> Engine {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        let mut clips = Vec::new();
        for (name, (from, to), key) in [("Deck A", (1, 45), a), ("Deck B", (25, 65), b)] {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some(name));
            let buf = soundcraft_audio_io::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.0; 80 * 48_000]] };
            let src = crate::io::add_source(&mut s, name, buf, None, soundcraft_audio_io::FileFormat::Wav);
            s.sources.last_mut().unwrap().set_key(key);
            let id = s.new_clip_id();
            let start = bar_sample(&s, from);
            let mut c = Clip::audio(id, name, src, 0, start, bar_sample(&s, to) - start);
            c.warp = WarpMap::new((0..160).map(|k| 4_800.0 + k as f64 * 22_500.0).collect(), 0.0);
            crate::edit::place_clip(&mut s, t, c);
            clips.push(id);
        }
        let mut e = Engine::new(s);
        e.execute("dj.transition", &json!({"out_clip": clips[0].0, "in_clip": clips[1].0})).unwrap();
        e
    }

    #[test]
    fn the_key_is_the_one_the_set_plays_at_the_start_across_a_transition() {
        let mut e = set("8A", "Em");
        let bass = track_of(&part(&mut e, json!({"kind": "bass"})));
        for (from, camelot, key) in [(30, "8A", "Am"), (37, "8A", "Am"), (41, "9A", "Em"), (50, "9A", "Em")] {
            let (a, b) = (bar(&e, from), bar(&e, from + 4));
            select(&mut e, a, b);
            let r = pattern(&mut e, json!({"kind": "bass", "style": "rolling", "track": bass.0}));
            assert_eq!((r["camelot"].as_str(), r["key"].as_str()), (Some(camelot), Some(key)), "bar {from}: {r}");
            assert_eq!(clip_of(&e, &r).name, format!("Rolling ({camelot})"));
        }
        // A given key wins over the set's; an Open Key name reads too.
        let (a, b) = (bar(&e, 55), bar(&e, 57));
        select(&mut e, a, b);
        let r = pattern(&mut e, json!({"kind": "synth", "style": "stabs", "track": bass.0, "key": "1d"}));
        assert_eq!((r["camelot"].as_str(), r["key"].as_str()), (Some("8B"), Some("C")));
    }

    /// Into a slot, a keyed pattern takes the key the set plays at the playhead.
    #[test]
    fn a_slot_pattern_takes_the_key_at_the_playhead_across_a_transition() {
        let mut e = set("8A", "Em");
        e.execute("live.add_column", &json!({"kind": "instrument"})).unwrap();
        for (at, camelot) in [(30, "8A"), (37, "8A"), (41, "9A"), (50, "9A")] {
            e.session_mut().edit.playhead = bar(&e, at) + 1_000;
            let r = pattern(&mut e, json!({"kind": "bass", "style": "rolling", "replace": true, "slot": {"column": 0, "slot": 0}}));
            assert_eq!(r["camelot"].as_str(), Some(camelot), "bar {at}: {r}");
            assert_eq!(e.session().live.midi_slot(0, 0).map(|x| x.name.clone()), Some(format!("Rolling ({camelot})")));
        }
    }

    #[test]
    fn refusals_leave_the_session_alone() {
        let mut e = Engine::default();
        let drums = track_of(&part(&mut e, json!({"kind": "drums"})));
        let bass = track_of(&part(&mut e, json!({"kind": "bass"})));
        let audio = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Vocal"));
        let (a, b) = (bar(&e, 1), bar(&e, 5));
        select(&mut e, a, b);
        pattern(&mut e, json!({"kind": "drums", "style": "four_floor", "track": drums.0}));
        let before = e.session().clone();
        let undo = e.can_undo();
        let journal = e.journal.len();
        let refuse = |e: &mut Engine, p: Value, says: &str| {
            let err = e.execute("prod.pattern", &p).unwrap_err().to_string();
            assert!(err.contains(says), "{p}: {err}");
        };
        refuse(&mut e, json!({"kind": "bass", "style": "rolling", "track": bass.0}), "no key here: pass key or set the track key");
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": drums.0}), "replace: true");
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": audio.0}), "not an instrument track");
        refuse(&mut e, json!({"kind": "bass", "style": "rolling", "track": bass.0, "key": "H minor"}), "not a key");
        refuse(&mut e, json!({"kind": "bass", "style": "stabs", "track": bass.0, "key": "Am"}), "synth style");
        refuse(&mut e, json!({"kind": "bass", "track": bass.0, "key": "Am"}), "`style` required");
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": bass.0, "bars": 0}), "`bars`");
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": bass.0, "pitch": 3}), "unknown field");
        // A selected non-instrument track is refused by name; no track at all asks for one.
        e.session_mut().edit.selected_tracks = vec![audio];
        refuse(&mut e, json!({"kind": "drums", "style": "fill"}), "`Vocal` is not an instrument track");
        e.session_mut().edit.selected_tracks.clear();
        refuse(&mut e, json!({"kind": "drums", "style": "fill"}), "no track");
        // No selection and no bars: no range.
        e.session_mut().edit.selection = Range::point(0);
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": bass.0}), "no range");
        // Only 4/4.
        e.session_mut().tempo.set_meter(0, 3, 4).unwrap();
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": bass.0, "bars": 2}), "only 4/4 is supported: bar 1 is in 3/4");
        e.session_mut().tempo.set_meter(0, 4, 4).unwrap();
        e.session_mut().tempo.set_meter(3 * 3840, 7, 8).unwrap();
        refuse(&mut e, json!({"kind": "drums", "style": "fill", "track": bass.0, "bars": 8}), "bar 4 is in 7/8");
        e.session_mut().tempo.remove_meter(3 * 3840);
        e.session_mut().edit.selection = Range::new(a, b);
        e.session_mut().edit.selected_tracks = before.edit.selected_tracks.clone();
        e.session_mut().edit.playhead = before.edit.playhead;
        assert!(*e.session() == before, "a refusal changed the session");
        assert_eq!((e.can_undo(), e.journal.len()), (undo, journal), "a refusal was recorded");
    }

    #[test]
    fn replace_clears_and_trims_what_is_under_the_bars() {
        let mut e = Engine::default();
        let t = track_of(&part(&mut e, json!({"kind": "drums"})));
        let (a, b) = (bar(&e, 1), bar(&e, 9));
        select(&mut e, a, b);
        let old = pattern(&mut e, json!({"kind": "drums", "style": "four_floor"}));
        let starts: Vec<i64> = sequence(&clip_of(&e, &old)).notes.iter().map(|n| n.start).collect();
        let (a, b) = (bar(&e, 3), bar(&e, 5));
        select(&mut e, a, b);
        let r = pattern(&mut e, json!({"kind": "drums", "style": "fill", "replace": true}));
        let clips: Vec<(String, Samples, Samples)> =
            e.session().track(t).unwrap().clips().iter().map(|c| (c.name.clone(), c.start, c.end())).collect();
        let want = [("Four Floor", bar(&e, 1), bar(&e, 3)), ("Fill", bar(&e, 3), bar(&e, 5)), ("Four Floor", bar(&e, 5), bar(&e, 9))];
        assert_eq!(clips, want.map(|(n, a, b)| (n.to_string(), a, b)), "{old} then {r}");
        // The kept pieces keep only their own kicks: 8 before, 16 after.
        let kicks: Vec<usize> = e.session().track(t).unwrap().clips().iter().map(|c| sequence(c).notes.len()).collect();
        assert_eq!((kicks[0], kicks[2]), (8, 16));
        // The right-hand piece starts on its first kick: its notes are the original's from bar 5
        // on, moved back by the 4 bars before it.
        let right = e.session().track(t).unwrap().clips()[2].clone();
        let got: Vec<i64> = sequence(&right).notes.iter().map(|n| n.start).collect();
        let want: Vec<i64> = starts.iter().filter(|&&x| x >= 4 * 3840).map(|x| x - 4 * 3840).collect();
        assert_eq!(got.first(), Some(&0));
        assert_eq!(got, want);
    }

    /// The level and cutoff lanes of `t`'s instrument.
    fn sweeps(e: &Engine, t: TrackId) -> (Option<soundcraft_model::AutomationLane>, Option<soundcraft_model::AutomationLane>) {
        let tr = e.session().track(t).unwrap();
        (tr.lane(&AutoParam::instrument("cutoff")).cloned(), tr.lane(&AutoParam::instrument("level")).cloned())
    }

    #[test]
    fn a_riser_sweeps_cutoff_and_level_up_to_the_range_end_leaving_the_rest_alone() {
        let mut e = Engine::default();
        let t = track_of(&part(&mut e, json!({"kind": "fx", "preset": "noise_riser"})));
        let ins = e.session().track(t).unwrap().instrument.clone().unwrap();
        let (level, cutoff) = (ins.params["level"], ins.params["cutoff"]);
        // A hand-drawn level dip on bar 1 and a point on bar 12 stay as they were.
        let (b1, b12) = (bar(&e, 1), bar(&e, 12));
        {
            let lane = e.session_mut().track_mut(t).unwrap().lane_mut(&AutoParam::instrument("level"));
            lane.set_point(b1, level - 20.0);
            lane.set_point(b12, level - 10.0);
        }
        let (a, b) = (bar(&e, 5), bar(&e, 9));
        select(&mut e, a, b);
        let r = pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
        assert_eq!(r["automation"], json!(["instrument:cutoff", "instrument:level"]));
        assert_eq!(clip_of(&e, &r).name, "Riser");
        // The level lane already had points; the cutoff lane was empty, so the knob stops working.
        assert!(e.messages.iter().any(|m| m == "FX 1's cutoff now follow automation; edit the lane to change them"), "{:?}", e.messages);
        let (c, l) = sweeps(&e, t);
        let (c, l) = (c.unwrap(), l.unwrap());
        for lane in [&c, &l] {
            let inside: Vec<f32> = lane.points.iter().filter(|p| (a..=b).contains(&p.at)).map(|p| p.value).collect();
            assert!(inside.len() > 8, "{inside:?}");
            assert!(inside.windows(2).all(|w| w[1] >= w[0]), "rising: {inside:?}");
            assert!(lane.points.iter().any(|p| p.at == b), "a point on the range end");
        }
        // Cutoff opens fully and the level reaches the part's own level at the end.
        assert_eq!(c.value_at(b, 0.0), 20_000.0);
        assert!((l.value_at(b, 0.0) - level).abs() < 1e-4);
        assert!(c.value_at(a, 0.0) < 100.0 && l.value_at(a, 0.0) < level - 6.0, "it starts low");
        // Outside: the hand-drawn points are kept and the curve before and after is unchanged.
        assert_eq!(l.value_at(b1, 0.0), level - 20.0);
        assert_eq!(l.value_at(b12, 0.0), level - 10.0);
        let was = |at: Samples| (level - 20.0) + 10.0 * ((at - b1) as f32 / (b12 - b1) as f32);
        assert!((l.value_at(a - 1, 0.0) - was(a - 1)).abs() < 1e-3);
        assert!((l.value_at(b + 1, 0.0) - was(b + 1)).abs() < 1e-3);
        assert_eq!((c.value_at(a - 1, 0.0), c.value_at(b + 1, 0.0)), (cutoff, cutoff), "an empty lane holds the part's setting");
    }

    #[test]
    fn a_sweep_refuses_to_overwrite_automation_drawn_inside_its_bars() {
        let mut e = Engine::default();
        let t = track_of(&part(&mut e, json!({"kind": "fx", "preset": "noise_riser"})));
        let (a, b) = (bar(&e, 5), bar(&e, 9));
        let mid = bar(&e, 7);
        e.session_mut().track_mut(t).unwrap().lane_mut(&AutoParam::instrument("cutoff")).set_point(mid, 500.0);
        select(&mut e, a, b);
        let before = e.session().clone();
        let err = e.execute("prod.pattern", &json!({"kind": "fx", "style": "riser"})).unwrap_err();
        assert!(matches!(err, crate::EngineError::WouldReplace { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("instrument:cutoff") && msg.contains("replace: true") && !msg.contains("instrument:level"), "{msg}");
        assert!(*e.session() == before, "the refusal changed nothing");
        // A pattern with no sweep leaves the drawn points alone without asking.
        let r = pattern(&mut e, json!({"kind": "fx", "style": "impact"}));
        assert_eq!(r["automation_cleared"], json!([]));
        assert_eq!(sweeps(&e, t).0.unwrap().value_at(mid, 0.0), 500.0);
        // A sweep on the neighbouring bars isn't refused by the first one's anchors on the bar line.
        let (c, d) = (bar(&e, 9), bar(&e, 11));
        select(&mut e, c, d);
        pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
        let (g, h) = (bar(&e, 11), bar(&e, 13));
        select(&mut e, g, h);
        pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
    }

    #[test]
    fn replace_clears_an_old_sweep_the_new_pattern_does_not_write_and_nothing_outside() {
        for (kind, style) in [("fx", "impact"), ("drums", "fill")] {
            let mut e = Engine::default();
            let t = track_of(&part(&mut e, json!({"kind": "fx", "preset": "noise_riser"})));
            let ins = e.session().track(t).unwrap().instrument.clone().unwrap();
            let (level, cutoff) = (ins.params["level"], ins.params["cutoff"]);
            // Hand-drawn level points either side of the bars stay as they were.
            let (b1, b12) = (bar(&e, 1), bar(&e, 12));
            {
                let lane = e.session_mut().track_mut(t).unwrap().lane_mut(&AutoParam::instrument("level"));
                lane.set_point(b1, level - 20.0);
                lane.set_point(b12, level - 10.0);
            }
            let (a, b) = (bar(&e, 5), bar(&e, 9));
            select(&mut e, a, b);
            pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
            let err = e.execute("prod.pattern", &json!({"kind": kind, "style": style})).unwrap_err();
            assert!(matches!(err, crate::EngineError::WouldReplace { .. }), "{err:?}");
            assert!(err.to_string().contains("clears the instrument:cutoff and instrument:level points"), "{err}");
            let r = pattern(&mut e, json!({"kind": kind, "style": style, "replace": true}));
            assert_eq!(r["automation"], json!([]), "{style}");
            assert_eq!(r["automation_cleared"], json!(["instrument:cutoff", "instrument:level"]), "{style}");
            assert!(e.messages.iter().any(|m| m == "FX 1: cleared the cutoff/level sweep inside the bars"), "{:?}", e.messages);
            let (c, l) = sweeps(&e, t);
            let (c, l) = (c.unwrap(), l.unwrap());
            for lane in [&c, &l] {
                assert!(lane.points.iter().all(|p| p.at < a || p.at > b), "{style}: no points left inside the bars");
            }
            // The old riser no longer plays: cutoff holds the part's setting over the bars and the
            // level runs along the hand-drawn line.
            let was = |at: Samples| (level - 20.0) + 10.0 * ((at - b1) as f32 / (b12 - b1) as f32);
            for at in [a, bar(&e, 7), b] {
                assert_eq!(c.value_at(at, 0.0), cutoff, "{style}");
                assert!((l.value_at(at, 0.0) - was(at)).abs() < 1e-3, "{style}: {} vs {}", l.value_at(at, 0.0), was(at));
            }
            assert_eq!((l.value_at(b1, 0.0), l.value_at(b12, 0.0)), (level - 20.0, level - 10.0), "{style}");
        }
    }

    #[test]
    fn a_downlifter_falls_and_a_sweep_drives_only_the_params_an_instrument_has() {
        let mut e = Engine::default();
        let fx = track_of(&part(&mut e, json!({"kind": "fx", "preset": "downlifter"})));
        let drums = track_of(&part(&mut e, json!({"kind": "drums"})));
        let (a, b) = (bar(&e, 1), bar(&e, 3));
        select(&mut e, a, b);
        pattern(&mut e, json!({"kind": "fx", "style": "downlifter", "track": fx.0}));
        let (c, _) = sweeps(&e, fx);
        let c = c.unwrap();
        assert!(c.value_at(a, 0.0) > c.value_at(b, 0.0));
        // The drum kit has a level but no cutoff; a hosted instrument has neither that is known.
        let r = pattern(&mut e, json!({"kind": "fx", "style": "riser", "track": drums.0}));
        assert_eq!(r["automation"], json!(["instrument:level"]));
        assert!(matches!(sweeps(&e, drums), (None, Some(_))));
        let hosted = e.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Hosted"));
        e.session_mut().track_mut(hosted).unwrap().instrument = Some(soundcraft_model::Insert::new("clap:test.keys"));
        let r = pattern(&mut e, json!({"kind": "fx", "style": "riser", "track": hosted.0}));
        assert_eq!((r["automation"].clone(), r["notes"].as_u64()), (json!([]), Some(1)));
        assert_eq!(sweeps(&e, hosted), (None, None));
        assert!(e.messages.iter().any(|m| m.contains("only the notes")), "{:?}", e.messages);
        let (a, b) = (bar(&e, 5), bar(&e, 6));
        let r = pattern(&mut e, json!({"kind": "drums", "style": "fill", "track": fx.0, "start": a, "end": b}));
        assert_eq!(r["automation"], json!([]));
    }

    #[test]
    fn a_sweep_says_when_it_pins_the_knobs_or_will_not_play() {
        let mut e = Engine::default();
        let t = track_of(&part(&mut e, json!({"kind": "fx"})));
        let (a, b) = (bar(&e, 1), bar(&e, 3));
        select(&mut e, a, b);
        pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
        assert!(e.messages.iter().any(|m| m == "FX 1's cutoff/level now follow automation; edit the lane to change them"), "{:?}", e.messages);
        e.messages.clear();
        e.session_mut().track_mut(t).unwrap().mixer.automation_mode = soundcraft_model::AutomationMode::Off;
        let (a, b) = (bar(&e, 5), bar(&e, 7));
        select(&mut e, a, b);
        pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
        assert_eq!(e.messages, ["FX 1's automation is off, so its Riser sweep won't play until the track reads automation again"]);
    }

    #[test]
    fn one_undo_removes_the_clip_and_the_automation() {
        let mut e = Engine::default();
        let t = track_of(&part(&mut e, json!({"kind": "fx"})));
        let (a, b) = (bar(&e, 2), bar(&e, 4));
        select(&mut e, a, b);
        let before = e.session().clone();
        pattern(&mut e, json!({"kind": "fx", "style": "riser"}));
        assert_eq!(e.session().track(t).unwrap().clips().len(), 1);
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(*e.session() == before, "one undo puts everything back");
        assert!(e.session().track(t).unwrap().clips().is_empty() && sweeps(&e, t) == (None, None));
        assert!(e.journal.iter().any(|(id, _)| id == "prod.pattern"), "journaled");
    }

    #[test]
    fn a_trance_groove_sounds_inside_its_clip_and_not_before_it() {
        let mut e = Engine::default();
        let t = track_of(&part(&mut e, json!({"kind": "drums"})));
        let (a, b) = (bar(&e, 3), bar(&e, 5));
        select(&mut e, a, b);
        pattern(&mut e, json!({"kind": "drums", "style": "trance_full", "track": t.0}));
        let out = soundcraft_mix::render_range(e.session(), Range::new(0, b), 512);
        let peak =
            |from: Samples, to: Samples| out.iter().flat_map(|c| c.iter().take(to as usize).skip(from as usize)).fold(0.0f32, |m, x| m.max(x.abs()));
        assert_eq!(peak(0, a), 0.0, "silent before the clip");
        assert!(peak(a, b) > 0.05, "the groove sounds: peak {}", peak(a, b));
    }
}
