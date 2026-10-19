//! `dj.import_set`: builds a beat-locked arrangement from mixforge's set plan. Every track goes on
//! one of two decks with its first downbeat on the bar its transition enters on, the session tempo
//! follows the plan (ramps or steps), each transition gets Marty's recipe, and each incoming track
//! is nudged by the plan or by a reliable suggestion.

use super::plan::{Plan, PlanTrack, parse_plan};
use super::recipe::{Changes, bar_sample, build};
use super::*;
use soundcraft_model::{ChannelFormat, Insert, NudgeSource, NudgeSuggestion, TrackKind, Transition, TransitionStyle};

const CMD: &str = "dj.import_set";
/// The limiter the master fader gets.
const MAXIMIZER: &str = "maximizer";

/// Where the plan puts each track and how the tempo moves, all in session bars and beats (so it
/// holds under any tempo map): worked out from the plan alone before anything is imported.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    /// Bar each track's clip starts on: its mix-in point when mixforge starts it past its first
    /// downbeat, else its lead-in (if any) before its first downbeat.
    at_bar: Vec<i64>,
    /// Bar each track's first downbeat falls on (before `at_bar` when the clip starts on a cue).
    downbeat_bar: Vec<i64>,
    /// Bar each track comes in on (1 for the first).
    entry_bar: Vec<i64>,
    /// Track beat each clip starts on, when it starts on a cue (see [`cue_bars`]).
    from_beat: Vec<Option<f64>>,
    /// Recipe bars (build, out) of each transition: the style's, halved until the hit comes
    /// before the outgoing track's `audio_end`.
    bars: Vec<(i64, i64)>,
    /// Tempo events (tick, bpm), in order, the first at tick 0.
    tempo: Vec<(i64, f64)>,
}

/// Bars a trimmed outgoing clip plays on after its transition's gone bar.
const TAIL_BARS: i64 = 16;

/// Whole bars of lead-in before a track's first downbeat, as [`grid`] gives a clip starting on a
/// bar line (none when the downbeat is within the pre-roll of the file start).
fn lead_in_bars(t: &PlanTrack, per_bar: i64) -> i64 {
    if t.first_downbeat <= PRE_ROLL_SECS {
        return 0;
    }
    let bar_secs = 60.0 / t.bpm * per_bar as f64;
    (t.first_downbeat / bar_secs).ceil().clamp(0.0, 1e6) as i64
}

/// Where mixforge brings track `t` in, as whole bars of its own grid after its first downbeat:
/// it plays the track from `start`, on one of its bar lines ("mixing in at bar N", past an intro
/// that is off its beat grid). `None` when `start` is at or before the first downbeat (within the
/// pre-roll), so the track plays from its start.
fn cue_bars(t: &PlanTrack, per_bar: i64) -> Option<i64> {
    if t.start <= t.first_downbeat + PRE_ROLL_SECS {
        return None;
    }
    let bars = ((t.start - t.first_downbeat) * t.bpm / 60.0 / per_bar as f64).round();
    (bars.is_finite() && bars >= 1.0).then(|| bars.min(1e6) as i64)
}

/// Where track `t` goes to come in on bar `entry`: (clip start bar, first downbeat bar, track
/// beat at the clip start). On a cue the clip starts there, its cue bar on `entry`; otherwise it
/// starts with its lead-in, its first downbeat on `entry` (the first track: on bar 1 and after
/// its lead-in).
fn placement(t: &PlanTrack, entry: i64, per_bar: i64, first: bool) -> (i64, i64, Option<f64>) {
    match cue_bars(t, per_bar) {
        Some(cue) => (entry, entry.saturating_sub(cue), Some((cue * per_bar) as f64)),
        None if first => (entry, entry.saturating_add(lead_in_bars(t, per_bar)), None),
        None => (entry.saturating_sub(lead_in_bars(t, per_bar)), entry, None),
    }
}

/// Session bar (1-based) whose start is nearest tick `tick`.
fn nearest_bar(tempo: &TempoMap, tick: i64) -> i64 {
    let bar = tempo.bar_beat_at_tick(tick).bar;
    let next = bar.saturating_add(1);
    if (tempo.bar_start_tick(next) - tick).abs() < (tick - tempo.bar_start_tick(bar)).abs() { next } else { bar }
}

/// Recipe bars (build, out) of a transition in `style` entering on bar `entry`: the style's,
/// halved (both, down to 1) until the hit bar starts before session beat `end` (where the
/// outgoing track's audio ends).
fn recipe_bars(style: TransitionStyle, tempo: &TempoMap, entry: i64, end: f64) -> (i64, i64) {
    let (mut b, mut o) = style.default_bars();
    while (b, o) != (1, 1) && tempo.bar_start_tick(entry.saturating_add(b)) as f64 >= end * TICKS_PER_QUARTER as f64 {
        (b, o) = ((b / 2).max(1), (o / 2).max(1));
    }
    (b, o)
}

/// The [`Layout`] of `plan` under the meter of `tempo` (4/4 by default: a session beat is a quarter).
///
/// Track 0 starts on bar 1. Track i+1 comes in on the bar line nearest where track i reaches its
/// `out_start` (that track beat, counted from track i's first downbeat, in session beats), with
/// its mix-in point (or, without one, its first downbeat) there: see [`placement`].
/// The tempo starts at track 0's `play_bpm`; each transition ramps to the next `play_bpm` one step
/// per beat over its recipe's bars (shortened to fit, see [`recipe_bars`]) from the entry bar (`ramp`), or steps to it on the hit bar,
/// and holds it after. A later transition entering before an earlier ramp ends takes over there.
fn layout(plan: &Plan, tempo: &TempoMap) -> std::result::Result<Layout, String> {
    let per_bar = i64::from(tempo.meter_at_tick(0).numerator.max(1));
    let beat_ticks = tempo.meter_at_tick(0).ticks_per_beat().max(1);
    let (mut at_bar, mut downbeat_bar, mut bars) = (Vec::new(), Vec::new(), Vec::new());
    let first = plan.tracks.first().ok_or("the plan has no tracks")?;
    let mut events: Vec<(i64, f64)> = vec![(0, first.play_bpm)];
    let (at, downbeat, from) = placement(first, 1, per_bar, true);
    at_bar.push(at);
    downbeat_bar.push(downbeat);
    let (mut entry_bar, mut from_beat) = (vec![1], vec![from]);
    for (i, pair) in plan.tracks.windows(2).enumerate() {
        let (Some(out), Some(inc)) = (pair.first(), pair.get(1)) else { continue };
        let n = i + 2;
        let out_start = out.out_start.ok_or_else(|| format!("track {}: `out_start` is missing", i + 1))?;
        let (Some(&out_bar), Some(&out_downbeat)) = (at_bar.last(), downbeat_bar.last()) else { continue };
        // Session beat of track i's first downbeat, then of its out point.
        let downbeat_beat = (tempo.bar_start_tick(out_downbeat) as f64) / TICKS_PER_QUARTER as f64;
        let out_beat = downbeat_beat + (out_start - out.first_downbeat) * out.bpm / 60.0;
        if !out_beat.is_finite() || out_beat.abs() > 1e9 {
            return Err(format!("track {}: `out_start` is out of range", i + 1));
        }
        let entry = nearest_bar(tempo, (out_beat * TICKS_PER_QUARTER as f64).round() as i64);
        let (at, downbeat, from) = placement(inc, entry, per_bar, false);
        if at < out_bar {
            return Err(format!("track {n} would start before track {} (its entry, bar {entry}, is too early)", i + 1));
        }
        at_bar.push(at);
        downbeat_bar.push(downbeat);
        entry_bar.push(entry);
        from_beat.push(from);
        let style = out.transition.map_or(TransitionStyle::Blend, |t| t.style);
        let end = downbeat_beat + (out.audio_end - out.first_downbeat) * out.bpm / 60.0;
        let (build_bars, out_bars) = recipe_bars(style, tempo, entry, end);
        bars.push((build_bars, out_bars));
        let ramp = out.transition.is_some_and(|t| t.ramp);
        let (from, to) = (out.play_bpm, inc.play_bpm);
        let hit = tempo.bar_start_tick(entry.saturating_add(build_bars));
        if ramp {
            let start = tempo.bar_start_tick(entry);
            let end = tempo.bar_start_tick(entry.saturating_add(build_bars + out_bars));
            events.retain(|&(tick, _)| tick < start);
            let steps = ((end - start) / beat_ticks).max(1);
            for k in 0..steps {
                // Each beat plays at the tempo halfway through it, as dj.tempo_ramp does.
                events.push((start + k * beat_ticks, from + (to - from) * (k as f64 + 0.5) / steps as f64));
            }
            events.push((end, to));
        } else {
            events.retain(|&(tick, _)| tick < hit);
            events.push((hit, to));
        }
    }
    Ok(Layout { at_bar, downbeat_bar, entry_bar, from_beat, bars, tempo: events })
}

/// Camelot wheel distance between keys like `5B` and `7A` (`None` when either isn't one): steps
/// round the wheel, plus one to cross between minor (A) and major (B).
fn camelot_distance(a: &str, b: &str) -> Option<u32> {
    let parse = |k: &str| {
        let k = k.trim().to_ascii_uppercase();
        let (num, letter) = k.split_at(k.len().checked_sub(1)?);
        let n: u32 = num.parse().ok().filter(|n| (1..=12).contains(n))?;
        matches!(letter, "A" | "B").then_some((n, letter == "B"))
    };
    let ((na, ma), (nb, mb)) = (parse(a)?, parse(b)?);
    let d = na.abs_diff(nb);
    Some(d.min(12 - d) + u32::from(ma != mb))
}

/// `m:ss` of `secs`.
fn clock(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Display name of a plan track.
fn title(t: &PlanTrack) -> String {
    if t.title.is_empty() {
        std::path::Path::new(&t.path).file_stem().and_then(|n| n.to_str()).unwrap_or(&t.path).to_string()
    } else {
        t.title.clone()
    }
}

/// The nudge a transition gets: the plan's when it has one, else a reliable suggestion.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Nudge {
    beats: f64,
    source: NudgeSource,
}

/// Picks the nudge from the plan's `plan_ms` (milliseconds at `beat_ms` per beat) and the
/// suggestion, if one was made: the plan wins, then a reliable suggestion, else none.
fn choose_nudge(plan_ms: f64, beat_ms: f64, suggestion: Option<&NudgeSuggestion>) -> Nudge {
    if plan_ms != 0.0 && beat_ms > 0.0 {
        let beats = (plan_ms / beat_ms).clamp(-soundcraft_model::MAX_NUDGE_BEATS, soundcraft_model::MAX_NUDGE_BEATS);
        return Nudge { beats, source: NudgeSource::Plan };
    }
    match suggestion {
        Some(s) if s.reliable => Nudge { beats: s.nudge_beats, source: NudgeSource::Suggested },
        _ => Nudge { beats: 0.0, source: NudgeSource::None },
    }
}

/// `dj.import_set {plan, report?}`.
pub(super) fn import_set(e: &mut Engine, p: &Value) -> Result<Value> {
    let plan_path = str_param(p, "plan").ok_or_else(|| bad(CMD, "`plan` required: the path of mixforge's plan.json"))?;
    let report_path = str_param(p, "report").map(str::to_string);
    // Everything is read and checked before the session changes.
    let plan = parse_plan(&crate::io::read_text(plan_path)?).map_err(|m| bad(CMD, m))?;
    for (i, t) in plan.tracks.iter().enumerate() {
        crate::file_access::check_read(&t.path).map_err(EngineError::Io)?;
        if !crate::io::file_found(&t.path) {
            return Err(bad(CMD, format!("track {}: no audio file at {}", i + 1, t.path)));
        }
    }
    if let Some(r) = &report_path {
        crate::file_access::check_write(r).map_err(EngineError::Io)?;
    }
    if e.session().all_clips().any(|c| c.warp.is_some()) {
        return Err(bad(CMD, "the session already holds beat-locked clips; import into an empty session (session.new)"));
    }
    let lay = layout(&plan, &e.session().tempo).map_err(|m| bad(CMD, m))?;
    let maximizer = super::super::clap::plugin_info(MAXIMIZER).ok_or_else(|| bad(CMD, "the Maximizer plugin is missing"))?;
    let mut warnings = plan.warnings.clone();

    // The whole tempo map first, so every clip is placed (in samples) under the final map.
    let s = e.session_mut();
    let old: Vec<i64> = s.tempo.tempos().iter().map(|t| t.tick).filter(|&t| t != 0).collect();
    for tick in old {
        s.tempo.remove_tempo(tick);
    }
    for &(tick, bpm) in &lay.tempo {
        s.tempo.set_tempo(tick, bpm).map_err(|err| bad(CMD, err.to_string()))?;
    }
    let decks =
        [s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck A")), s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck B"))];
    let master = match s.master().map(|m| m.id) {
        Some(m) => m,
        None => s.add_track(TrackKind::Master, ChannelFormat::Stereo, None),
    };
    let mut max = Insert::new(maximizer.id);
    for pi in maximizer.params {
        max.params.insert(pi.id.to_string(), pi.default);
    }
    max.params.insert("ceiling".to_string(), -1.0);
    max.params.insert("threshold".to_string(), 0.0);
    match s.track_mut(master).and_then(|m| m.mixer.inserts.iter_mut().find(|i| i.as_ref().is_none_or(|i| i.plugin == MAXIMIZER))) {
        Some(slot) => *slot = Some(max),
        None => warnings.push("the master fader has no free insert slot; no Maximizer was added".to_string()),
    }

    // The clips.
    let mut clips = Vec::new();
    let mut track_doubts: Vec<Vec<String>> = Vec::new();
    for (i, t) in plan.tracks.iter().enumerate() {
        let deck = decks.get(i % 2).copied();
        let at_bar = lay.at_bar.get(i).copied().unwrap_or(1);
        let from_beat = lay.from_beat.get(i).copied().flatten();
        let placed = place_warped(e, CMD, &t.path, Beats::Constant { bpm: t.bpm, first: t.first_downbeat }, at_bar, deck, from_beat)?;
        let mut doubts = Vec::new();
        // A track that mixes out ends TAIL_BARS after its transition's gone bar (or at its own
        // end), so its fade-out doesn't run on long past the mix. Trimmed now, before the track
        // after next goes on the same deck: placing that one clears what it overlaps, and could
        // otherwise split this one and leave a stray piece after it.
        if let (Some(&entry), Some(&(b, o))) = (lay.entry_bar.get(i + 1), lay.bars.get(i)) {
            let tail_bar = entry.saturating_add(b + o + TAIL_BARS);
            let tail = bar_sample(e.session(), tail_bar);
            if let Some(c) = e.session_mut().find_clip_mut(placed.clip)
                && tail > c.start
                && tail < c.end()
            {
                c.trim_end_to(tail);
            }
            let end = e.session().find_clip(placed.clip).map_or(0, |(_, c)| c.end());
            if let Some(&next) = lay.at_bar.get(i + 2)
                && bar_sample(e.session(), next) < end
            {
                doubts.push(format!(
                    "{}: track {} starts on the same deck on bar {next}, before this track's planned end (bar {tail_bar}), and cuts it there",
                    title(t),
                    i + 3
                ));
            }
        }
        if Some(&placed.downbeat_bar) != lay.downbeat_bar.get(i) {
            doubts.push(format!(
                "{}: its first downbeat landed on bar {} instead of {}",
                title(t),
                placed.downbeat_bar,
                lay.downbeat_bar.get(i).copied().unwrap_or(0)
            ));
        }
        let s = e.session_mut();
        if let Some(c) = s.find_clip_mut(placed.clip) {
            c.gain_db = (t.gain_db as f32).clamp(-144.0, 36.0);
        }
        let copied = s.find_clip(placed.clip).and_then(|(_, c)| c.source()).and_then(|src| s.source(src)).is_some_and(|src| src.unsaved);
        if copied {
            doubts.push(format!(
                "{}: not at the session's sample rate ({} Hz), so its audio is copied into the session",
                title(t),
                s.sample_rate.hz()
            ));
        }
        if t.fx_count > 0 {
            doubts.push(format!("{}: {} planned effect(s) not imported", title(t), t.fx_count));
        }
        clips.push(placed.clip);
        track_doubts.push(doubts);
    }

    // The plan's `start` within a track's first bar (past silence or a pre-roll; one on a later
    // bar line placed the clip on that cue already): the clip start is trimmed there when that
    // falls after it and before the track's bass hit; the locks recorded first keep its music on
    // its beats through the trim.
    super::record_locks(e.session_mut());
    for (i, t) in plan.tracks.iter().enumerate() {
        let (Some(&clip), true) = (clips.get(i), t.start > 0.0) else { continue };
        let s = e.session();
        let Some((_, c)) = s.find_clip(clip) else { continue };
        let Some(w) = c.warp.as_ref() else { continue };
        // A track placed on its cue already starts there.
        if lay.from_beat.get(i).copied().flatten().is_some() {
            continue;
        }
        let limit = match (i.checked_sub(1).and_then(|k| lay.bars.get(k)), lay.entry_bar.get(i)) {
            (Some(&(build_bars, _)), Some(&entry)) => bar_sample(s, entry.saturating_add(build_bars)),
            _ => c.end(),
        };
        let track_beat = w.beat_at_source(t.start * s.sample_rate.as_f64());
        let at = sample_at_beat(&s.tempo, s.sample_rate, beat_at(&s.tempo, s.sample_rate, c.start) + track_beat - w.anchor);
        if at > c.start
            && at < limit
            && let Some(c) = e.session_mut().find_clip_mut(clip)
        {
            c.trim_start_to(at);
        }
    }
    super::record_locks(e.session_mut());

    // The transitions, in play order: each nudge moves the clip the next one is measured against.
    let mut rows = Vec::new();
    for (i, pair) in plan.tracks.windows(2).enumerate() {
        let (Some(out), Some(inc)) = (pair.first(), pair.get(1)) else { continue };
        let (Some(&out_clip), Some(&in_clip), Some(&entry_bar)) = (clips.get(i), clips.get(i + 1), lay.entry_bar.get(i + 1)) else { continue };
        let mut doubts: Vec<String> = Vec::new();
        if i == 0 {
            doubts.extend(track_doubts.first().cloned().unwrap_or_default());
        }
        doubts.extend(track_doubts.get(i + 1).cloned().unwrap_or_default());
        let style = out.transition.map_or(TransitionStyle::Blend, |t| t.style);
        let (b, o) = lay.bars.get(i).copied().unwrap_or_else(|| style.default_bars());
        if let Some(d) = camelot_distance(&out.camelot, &inc.camelot).filter(|d| *d > 1) {
            doubts.push(format!("key clash: {} -> {} ({d} steps on the wheel)", out.camelot, inc.camelot));
        }
        // The recipe's bars, shortened (halving both) when the clips don't hold them.
        let mut built = None;
        let mut last_err = String::new();
        let (mut bb, mut ob) = (b, o);
        loop {
            let ch = Changes { style: Some(style), build_bars: Some(bb), out_bars: Some(ob), entry_bar: Some(entry_bar), force: false };
            match build(e, CMD, None, out_clip, in_clip, &ch) {
                Ok(x) => {
                    built = Some(x.transition);
                    break;
                }
                Err(err) => last_err = err.to_string(),
            }
            if bb == 1 && ob == 1 {
                break;
            }
            (bb, ob) = ((bb / 2).max(1), (ob / 2).max(1));
        }
        let mut t = match built {
            Some(t) => {
                if (t.build_bars, t.out_bars) != style.default_bars() {
                    doubts.push(format!("recipe shortened to {}/{} bars to fit the clips", t.build_bars, t.out_bars));
                }
                t
            }
            None => {
                doubts.push(format!("needs manual attention: no recipe applied ({last_err})"));
                let s = e.session();
                let deck_of = |c: ClipId| s.find_clip(c).map_or(TrackId(0), |(t, _)| t);
                let t = Transition {
                    id: s.dj.next_id(),
                    out_clip,
                    in_clip,
                    out_track: deck_of(out_clip),
                    in_track: deck_of(in_clip),
                    entry_bar: entry_bar.clamp(1, soundcraft_model::MAX_TRANSITION_BAR),
                    style,
                    build_bars: b,
                    out_bars: o,
                    nudge_beats: 0.0,
                    nudge_source: NudgeSource::None,
                    suggestion: None,
                    edited: false,
                    stale: false,
                    applied: None,
                };
                e.session_mut().dj.transitions.push(t.clone());
                t
            }
        };

        // The nudge.
        let s = e.session();
        let in_start = s.find_clip(in_clip).map_or(0, |(_, c)| c.start);
        let beat_ms = 60_000.0 / s.tempo.tempo_at_tick(s.tempo.samples_to_ticks(in_start, s.sample_rate));
        let mut suggestion = None;
        if inc.nudge_ms == 0.0 {
            match super::align::suggest_nudge(e, &json!({"out_clip": out_clip.0, "in_clip": in_clip.0})) {
                Ok(v) => {
                    let num = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                    let sug = NudgeSuggestion {
                        nudge_beats: num("nudge_beats"),
                        nudge_ms: num("nudge_ms"),
                        reliable: v.get("reliable").and_then(Value::as_bool).unwrap_or(false),
                        ambiguity: num("ambiguity"),
                    };
                    if !sug.reliable {
                        let note = v.get("note").and_then(Value::as_str).unwrap_or("unreliable");
                        doubts.push(format!("nudge left at 0: the suggestion is {note}; the alternative is {:+.1} ms", sug.nudge_ms));
                    }
                    suggestion = Some(sug);
                }
                Err(err) => doubts.push(format!("no nudge suggestion: {err}")),
            }
        }
        let nudge = choose_nudge(inc.nudge_ms, beat_ms, suggestion.as_ref());
        let s = e.session_mut();
        if nudge.beats != 0.0
            && let Some(w) = s.find_clip_mut(in_clip).and_then(|c| c.warp.as_mut())
        {
            w.anchor -= nudge.beats;
        }
        t.nudge_beats = nudge.beats;
        t.nudge_source = nudge.source;
        t.suggestion = suggestion;
        if let Some(slot) = s.dj.transition_mut(t.id) {
            *slot = t.clone();
        }
        let entry_secs = s.tempo.tick_to_samples(s.tempo.bar_start_tick(t.entry_bar), s.sample_rate) as f64 / s.sample_rate.as_f64();
        rows.push(json!({
            "index": i + 1,
            "transition": t.id,
            "out_title": title(out),
            "in_title": title(inc),
            "out_camelot": out.camelot,
            "in_camelot": inc.camelot,
            "style": style,
            "mixforge_bars": out.transition.map(|x| x.bars),
            "entry_bar": t.entry_bar,
            "entry_time": clock(entry_secs),
            "hit_bar": t.hit_bar(),
            "nudge_beats": nudge.beats,
            "nudge_ms": nudge.beats * beat_ms,
            "nudge_source": nudge.source,
            "reliable": suggestion.map(|x| x.reliable),
            "ambiguity": suggestion.map(|x| x.ambiguity),
            "alt_ms": suggestion.filter(|x| !x.reliable).map(|x| x.nudge_ms),
            "doubts": doubts,
        }));
    }
    if plan.tracks.len() == 1 {
        warnings.extend(track_doubts.into_iter().flatten());
    }
    let ignored_fx: usize = plan.tracks.iter().map(|t| t.fx_count).sum();
    let list: Vec<Value> = plan
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({
                "title": title(t), "artist": t.artist, "key": t.key, "camelot": t.camelot, "bpm": t.bpm, "play_bpm": t.play_bpm,
                "deck": decks.get(i % 2), "clip": clips.get(i), "at_bar": lay.at_bar.get(i), "downbeat_bar": lay.downbeat_bar.get(i), "entry_bar": lay.entry_bar.get(i),
                "cue_in": t.cue_in, "start": t.start, "audio_end": t.audio_end, "gain_db": t.gain_db,
            })
        })
        .collect();
    let report = json!({
        "format": plan.format,
        "master_bpm": plan.master_bpm,
        "tempo_mode": plan.tempo_mode,
        "target_lufs": plan.target_lufs,
        "tracks": plan.tracks.len(),
        "track_list": list,
        "sample_rate": e.session().sample_rate.hz(),
        "decks": decks,
        "master": master,
        "transitions": rows,
        "warnings": warnings,
        "ignored_fx": ignored_fx,
    });
    if let Some(path) = report_path {
        crate::io::refuse_source_file(e.session(), std::path::Path::new(&path))?;
        crate::io::write_path(&path, text_report(&report))?;
    }
    Ok(report)
}

/// The plain-text report: one aligned line per transition, then every doubt, then the warnings.
fn text_report(r: &Value) -> String {
    let rows = r.get("transitions").and_then(Value::as_array).cloned().unwrap_or_default();
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let int = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    let mut out = format!("mixforge set import: {} tracks, {} transitions\n\n", int(r, "tracks"), rows.len());
    for v in &rows {
        out.push_str(&format!(
            "{:>3}  {:<32.32} -> {:<32.32}  {:>3} -> {:<3}  {:<5}  bar {:>4} ({:>6})  hit {:>4}  nudge {:>+7.1} ms {:<9}{}\n",
            int(v, "index"),
            text(v, "out_title"),
            text(v, "in_title"),
            text(v, "out_camelot"),
            text(v, "in_camelot"),
            text(v, "style"),
            int(v, "entry_bar"),
            text(v, "entry_time"),
            int(v, "hit_bar"),
            v.get("nudge_ms").and_then(Value::as_f64).unwrap_or(0.0),
            text(v, "nudge_source"),
            if v.get("doubts").and_then(Value::as_array).is_some_and(|d| !d.is_empty()) { "  (see doubts)" } else { "" },
        ));
    }
    let doubts: Vec<String> = rows
        .iter()
        .flat_map(|v| {
            let i = int(v, "index");
            v.get("doubts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(move |d| d.as_str().map(|d| format!("  {i:>3}: {d}")))
        })
        .collect();
    if !doubts.is_empty() {
        out.push_str("\nDoubts:\n");
        for d in doubts {
            out.push_str(&d);
            out.push('\n');
        }
    }
    let warnings = r.get("warnings").and_then(Value::as_array).cloned().unwrap_or_default();
    if !warnings.is_empty() {
        out.push_str("\nmixforge warnings:\n");
        for w in warnings.iter().filter_map(Value::as_str) {
            out.push_str("  ");
            out.push_str(w);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use soundcraft_audio_io::AudioBuffer;

    const SR: i64 = 48_000;

    /// `secs` of mono drums at `bpm` from a first beat at `first` seconds whose sound comes `late`
    /// seconds after the grid: a kick and a 1.5 kHz click on every beat, a noise hat between.
    fn drums(bpm: f64, first: f64, late: f64, secs: usize) -> AudioBuffer {
        let (n, sr) = (secs * SR as usize, SR as f64);
        let tau = std::f64::consts::TAU;
        let mut x = vec![0.0f32; n];
        let mut seed: u32 = 1;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        };
        let half = 30.0 / bpm;
        for k in 0.. {
            let at = ((first + k as f64 * half + late) * sr).round() as usize;
            if at >= n {
                break;
            }
            for j in 0..(0.3 * sr) as usize {
                let (t, Some(y)) = (j as f64 / sr, x.get_mut(at + j)) else { break };
                let v = if k % 2 == 0 {
                    0.5 * (tau * 60.0 * t).sin() * (-t / 0.08).exp() + 0.3 * (tau * 1_500.0 * t).sin() * (-t / 0.005).exp()
                } else {
                    0.3 * f64::from(noise()) * (-t / 0.007).exp()
                };
                *y += v as f32;
            }
        }
        AudioBuffer { sample_rate: SR as u32, channels: vec![x] }
    }

    /// A fresh folder for one test's files.
    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-dj-import-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_wav(dir: &std::path::Path, name: &str, buf: &AudioBuffer) -> String {
        let path = dir.join(name);
        std::fs::write(&path, soundcraft_audio_io::encode(buf, &soundcraft_audio_io::EncodeOptions::default()).unwrap()).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// Three 90 s tracks at 120, 124 and 128 bpm. B enters on bar 17 (a ramping blend, its first
    /// downbeat 0.5 s in, so it starts a bar earlier, and a plan nudge of 10 ms); C enters on
    /// bar 49 (a quick mix that steps the tempo, its sound 30 ms late, in a clashing key).
    fn set(name: &str) -> (std::path::PathBuf, String) {
        let dir = folder(name);
        let a = write_wav(&dir, "a.wav", &drums(120.0, 0.0, 0.0, 90));
        let b = write_wav(&dir, "b.wav", &drums(124.0, 0.5, 0.0, 90));
        let c = write_wav(&dir, "c.wav", &drums(128.0, 0.0, 0.030, 90));
        // A's beat 64 is bar 17; B's downbeat is on bar 17, so its beat 128 is bar 49.
        let plan = json!({
            "format": "mixforge-plan/2", "master_bpm": 124.0, "tempo_mode": "ramp", "target_lufs": -10.0,
            "warnings": ["a warning from mixforge"],
            "tracks": [
                {"path": a, "title": "A", "bpm": 120.0, "play_bpm": 120.0, "first_downbeat": 0.0, "audio_end": 90.0, "out_start": 32.0,
                 "gain_db": 6.0, "nudge_ms": 0, "camelot": "8A", "transition": {"style": "blend", "bars": 16, "ramp": true}, "fx": [{"kind": "echo"}]},
                {"path": b, "title": "B", "bpm": 124.0, "play_bpm": 124.0, "first_downbeat": 0.5, "audio_end": 90.0,
                 "out_start": 0.5 + 128.0 * 60.0 / 124.0, "gain_db": -2.0, "nudge_ms": 10.0, "camelot": "9A",
                 "transition": {"style": "quick", "bars": 8, "ramp": false}, "fx": []},
                {"path": c, "title": "C", "bpm": 128.0, "play_bpm": 128.0, "first_downbeat": 0.0, "audio_end": 90.0, "out_start": null,
                 "gain_db": 0.0, "nudge_ms": 0.0, "camelot": "3B", "transition": null, "fx": []},
            ],
        });
        let path = dir.join("plan.json");
        std::fs::write(&path, plan.to_string()).unwrap();
        (dir, path.to_string_lossy().into_owned())
    }

    fn bar(e: &Engine, n: i64) -> Samples {
        let s = e.session();
        s.tempo.tick_to_samples(s.tempo.bar_start_tick(n), s.sample_rate)
    }

    fn tick(e: &Engine, n: i64) -> i64 {
        e.session().tempo.bar_start_tick(n)
    }

    #[test]
    fn a_set_imports_as_a_beat_locked_arrangement() {
        let (dir, plan) = set("full");
        let mut e = Engine::default();
        let before = e.session().clone();
        let report = dir.join("report.txt").to_string_lossy().into_owned();
        let r = e.execute("dj.import_set", &json!({"plan": plan, "report": report})).unwrap();
        let s = e.session();
        assert_eq!(s.sample_rate.hz(), SR as u32);

        // Clips on alternating decks, downbeats on the entry bars, gains set.
        let decks: Vec<TrackId> = serde_json::from_value(r["decks"].clone()).unwrap();
        let clips: Vec<ClipId> = r["track_list"].as_array().unwrap().iter().map(|t| ClipId(t["clip"].as_u64().unwrap())).collect();
        let starts: Vec<Samples> = clips.iter().map(|c| s.find_clip(*c).unwrap().1.start).collect();
        assert_eq!(starts, vec![bar(&e, 1), bar(&e, 16), bar(&e, 49)]);
        let on: Vec<TrackId> = clips.iter().map(|c| s.find_clip(*c).unwrap().0).collect();
        assert_eq!(on, vec![decks[0], decks[1], decks[0]]);
        let gains: Vec<f32> = clips.iter().map(|c| s.find_clip(*c).unwrap().1.gain_db).collect();
        assert_eq!(gains, vec![6.0, -2.0, 0.0]);
        assert_eq!(r["track_list"][1]["downbeat_bar"], 17);

        // Tempo: a ramp from bar 17 to 41 (blend: 16 + 8 bars), then a step on bar 57 (quick: hit).
        let tempo = &s.tempo;
        assert_eq!(tempo.tempo_at_tick(0), 120.0);
        let ramp: Vec<f64> = tempo.tempos().iter().filter(|t| t.tick >= tick(&e, 17) && t.tick < tick(&e, 41)).map(|t| t.bpm).collect();
        assert_eq!(ramp.len(), 96, "one step per beat");
        assert!(ramp.windows(2).all(|w| w[0] < w[1]) && ramp[0] > 120.0 && ramp[95] < 124.0);
        assert_eq!(tempo.tempo_at_tick(tick(&e, 41)), 124.0);
        assert!(!tempo.tempos().iter().any(|t| t.tick > tick(&e, 41) && t.tick < tick(&e, 57)));
        assert_eq!(tempo.tempo_at_tick(tick(&e, 57) - 1), 124.0);
        assert_eq!(tempo.tempo_at_tick(tick(&e, 57)), 128.0);

        // Transitions with their recipes, EQ on both decks, the Maximizer on the master.
        let t: Vec<&Transition> = s.dj.transitions.iter().collect();
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].entry_bar, t[0].style, t[0].hit_bar()), (17, TransitionStyle::Blend, 33));
        assert_eq!((t[1].entry_bar, t[1].style, t[1].hit_bar()), (49, TransitionStyle::Quick, 57));
        let lane = super::super::recipe::bass_lane();
        let pts = |d: TrackId| s.track(d).unwrap().lane(&lane).unwrap().points.iter().map(|p| p.at).collect::<Vec<_>>();
        assert_eq!(pts(decks[1]), vec![bar(&e, 17), bar(&e, 33), bar(&e, 57), bar(&e, 61)]);
        assert_eq!(pts(decks[0]), vec![bar(&e, 33), bar(&e, 41), bar(&e, 49), bar(&e, 57)]);
        for d in &decks {
            assert_eq!(s.track(*d).unwrap().mixer.inserts[0].as_ref().unwrap().plugin, "eq_7band");
        }
        let max = s.master().unwrap().mixer.inserts.iter().flatten().find(|i| i.plugin == MAXIMIZER).unwrap();
        assert_eq!((max.params.get("ceiling"), max.params.get("threshold")), (Some(&-1.0), Some(&0.0)));

        // The plan's nudge (10 ms at B's start tempo, 120 bpm: 0.02 beats) and a reliable suggestion.
        assert_eq!(t[0].nudge_source, NudgeSource::Plan);
        assert!((t[0].nudge_beats - 0.02).abs() < 1e-9);
        let b_anchor = s.find_clip(clips[1]).unwrap().1.warp.as_ref().unwrap().anchor;
        assert!((b_anchor + 4.02).abs() < 1e-9, "{b_anchor}");
        let row = &r["transitions"][1];
        assert_eq!(t[1].nudge_source, NudgeSource::Suggested, "{row}");
        let ms = row["nudge_ms"].as_f64().unwrap();
        // C is 0.064 beats late, B 0.02 beats later than its grid: 0.044 beats at 124 bpm.
        assert!((ms + 0.044 * 60_000.0 / 124.0).abs() < 2.0, "{row}");
        assert!(t[1].suggestion.is_some_and(|x| x.reliable));

        // The report and its doubts.
        assert_eq!(r["tracks"], 3);
        assert_eq!(r["ignored_fx"], 1);
        assert_eq!(r["warnings"], json!(["a warning from mixforge"]));
        assert_eq!(r["transitions"][0]["mixforge_bars"], 16);
        assert_eq!(r["transitions"][0]["entry_time"], "0:32");
        let doubts = |i: usize| r["transitions"][i]["doubts"].to_string();
        assert!(doubts(0).contains("1 planned effect(s) not imported"), "{}", doubts(0));
        assert!(doubts(1).contains("key clash: 9A -> 3B"), "{}", doubts(1));
        let text = std::fs::read_to_string(&report).unwrap();
        assert!(text.contains("Doubts:") && text.contains("key clash") && text.lines().filter(|l| l.contains(" bar ")).count() == 2, "{text}");

        // One undo step takes the whole import away; a second import is refused.
        let after = e.session().clone();
        let err = e.execute("dj.import_set", &json!({"plan": plan})).unwrap_err();
        assert!(err.to_string().contains("import into an empty session"), "{err}");
        assert!(*e.session() == after);
        assert!(e.undo());
        assert!(*e.session() == before, "undo left part of the import");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_changes_nothing() {
        let (dir, plan) = set("missing");
        std::fs::remove_file(dir.join("c.wav")).unwrap();
        let mut e = Engine::default();
        let before = e.session().clone();
        let err = e.execute("dj.import_set", &json!({"plan": plan})).unwrap_err();
        assert!(err.to_string().contains("track 3: no audio file"), "{err}");
        assert!(*e.session() == before);
        let err = e.execute("dj.import_set", &json!({"plan": dir.join("nope.json").to_string_lossy()})).unwrap_err();
        assert!(*e.session() == before, "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plan_nudge_wins_then_a_reliable_suggestion() {
        let sug = |reliable| NudgeSuggestion { nudge_beats: -0.05, nudge_ms: -25.0, reliable, ambiguity: 0.2 };
        assert_eq!(choose_nudge(10.0, 500.0, Some(&sug(true))), Nudge { beats: 0.02, source: NudgeSource::Plan });
        assert_eq!(choose_nudge(0.0, 500.0, Some(&sug(true))), Nudge { beats: -0.05, source: NudgeSource::Suggested });
        assert_eq!(choose_nudge(0.0, 500.0, Some(&sug(false))), Nudge { beats: 0.0, source: NudgeSource::None });
        assert_eq!(choose_nudge(0.0, 500.0, None), Nudge { beats: 0.0, source: NudgeSource::None });
    }

    #[test]
    fn camelot_distance_counts_wheel_steps() {
        assert_eq!(camelot_distance("5B", "5B"), Some(0));
        assert_eq!(camelot_distance("5B", "5A"), Some(1));
        assert_eq!(camelot_distance("12A", "1A"), Some(1));
        assert_eq!(camelot_distance("5B", "7A"), Some(3));
        assert_eq!(camelot_distance("8a", "3b"), Some(6));
        assert_eq!(camelot_distance("", "5A"), None);
        assert_eq!(camelot_distance("13A", "5A"), None);
    }

    /// A two-track plan: A (120 bpm, downbeat at 0, `a_end` s long) mixes from 32 s (bar 17) into
    /// B (124 bpm, downbeat 0.5 s in, 90 s long), a blend that ramps when `ramp`.
    fn pair(a: &str, b: &str, a_end: f64, b_start: f64, ramp: bool) -> Value {
        json!({
            "format": "mixforge-plan/2", "master_bpm": 124.0,
            "tracks": [
                {"path": a, "bpm": 120.0, "first_downbeat": 0.0, "audio_end": a_end, "out_start": 32.0,
                 "transition": {"style": "blend", "bars": 16, "ramp": ramp}},
                {"path": b, "bpm": 124.0, "first_downbeat": 0.5, "audio_end": 90.0, "start": b_start},
            ],
        })
    }

    #[test]
    fn a_short_outgoing_track_shortens_the_recipe_and_its_ramp() {
        let tempo = Session::default().tempo;
        // A ends on beat 140 (bar 36): a 16-bar build would hit on bar 33 (beat 128), in time.
        let lay = layout(&parse_plan(&pair("a", "b", 70.0, 0.0, true).to_string()).unwrap(), &tempo).unwrap();
        assert_eq!(lay.bars, vec![(16, 8)]);
        // A ends on beat 120 (bar 31): halved to 8/4 (hit on bar 25), and the ramp to bar 29.
        let lay = layout(&parse_plan(&pair("a", "b", 60.0, 0.0, true).to_string()).unwrap(), &tempo).unwrap();
        assert_eq!(lay.bars, vec![(8, 4)]);
        let (start, end) = (tempo.bar_start_tick(17), tempo.bar_start_tick(29));
        assert_eq!(lay.tempo.iter().filter(|(t, _)| *t >= start && *t < end).count(), 48);
        assert_eq!(lay.tempo.last(), Some(&(end, 124.0)));
    }

    #[test]
    fn imported_tracks_are_trimmed_to_their_mix_and_their_plan_start() {
        let dir = folder("trim");
        let a = write_wav(&dir, "a.wav", &drums(120.0, 0.0, 0.0, 150));
        let b = write_wav(&dir, "b.wav", &drums(124.0, 0.5, 0.0, 90));
        let plan = dir.join("plan.json");
        std::fs::write(&plan, pair(&a, &b, 150.0, 1.0, false).to_string()).unwrap();
        let mut e = Engine::default();
        let r = e.execute("dj.import_set", &json!({"plan": plan.to_string_lossy()})).unwrap();
        let s = e.session();
        let clip = |i: usize| s.find_clip(ClipId(r["track_list"][i]["clip"].as_u64().unwrap())).unwrap().1;
        // A (75 bars long) ends 16 bars after the gone bar (41), its fade-out from the hit (33).
        assert_eq!(clip(0).end(), bar(&e, 57));
        assert_eq!(clip(0).fade_out.len, bar(&e, 57) - bar(&e, 33));
        // B starts 1 s into its file, after its downbeat, which stays on bar 17.
        let c = clip(1);
        let w = c.warp.as_ref().unwrap();
        let heard = w.source_at_time(&s.tempo, s.sample_rate, c.start, c.start);
        assert!(c.start > bar(&e, 17) && (heard - SR as f64).abs() < 8.0, "starts at {} playing source sample {heard}", c.start);
        let downbeat = beat_at(&s.tempo, s.sample_rate, c.start) - w.anchor;
        // Less the (tiny) nudge the suggestion applied.
        let nudge = s.dj.transitions.first().unwrap().nudge_beats;
        assert!((downbeat - nudge - 64.0).abs() < 1e-6, "B's downbeat on session beat {downbeat}, nudged {nudge}");
        assert_eq!(c.fade_in.len, bar(&e, 33) - c.start);
        assert!(r["transitions"][0]["doubts"].as_array().unwrap().is_empty(), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_track_is_trimmed_before_the_track_after_next_lands_on_its_deck() {
        // A (150 s, to bar 76) into a short B on bar 17; B into C on bar 61, back on A's deck,
        // inside A's file but after A's planned end (bar 57). C is 10 s: it ends inside A's file.
        let dir = folder("deck");
        let a = write_wav(&dir, "a.wav", &drums(120.0, 0.0, 0.0, 150));
        let b = write_wav(&dir, "b.wav", &drums(124.0, 0.5, 0.0, 100));
        let c = write_wav(&dir, "c.wav", &drums(128.0, 0.0, 0.0, 10));
        let plan = json!({
            "format": "mixforge-plan/2", "master_bpm": 124.0,
            "tracks": [
                {"path": a, "bpm": 120.0, "first_downbeat": 0.0, "audio_end": 150.0, "out_start": 32.0,
                 "transition": {"style": "blend", "bars": 16, "ramp": false}},
                {"path": b, "bpm": 124.0, "first_downbeat": 0.5, "audio_end": 100.0, "out_start": 0.5 + 176.0 * 60.0 / 124.0,
                 "transition": {"style": "quick", "bars": 8, "ramp": false}},
                {"path": c, "bpm": 128.0, "first_downbeat": 0.0, "audio_end": 10.0},
            ],
        });
        let path = dir.join("plan.json");
        std::fs::write(&path, plan.to_string()).unwrap();
        let mut e = Engine::default();
        let r = e.execute("dj.import_set", &json!({"plan": path.to_string_lossy()})).unwrap();
        assert_eq!(r["track_list"][2]["at_bar"], 61, "{r}");
        let deck: TrackId = serde_json::from_value(r["decks"][0].clone()).unwrap();
        let s = e.session();
        let on_deck: Vec<(Samples, Samples)> = s.track(deck).unwrap().clips().iter().map(|c| (c.start, c.end())).collect();
        assert_eq!(on_deck.len(), 2, "a stray piece of A is left: {on_deck:?}");
        assert_eq!(on_deck[0], (bar(&e, 1), bar(&e, 57)));
        // Nothing reads as edited or stale once another command has run.
        e.execute("automation.set_point", &json!({"track": deck.0, "param": "volume", "at": bar(&e, 200), "value": 0.0})).unwrap();
        let t = &e.session().dj.transitions;
        assert!(t.iter().all(|t| !t.edited && !t.stale && t.applied.is_some()), "{t:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_track_mixed_in_past_its_intro_starts_on_its_cue() {
        // B's plan starts it at its bar 9 (8 bars after its downbeat at 0.5 s), as mixforge does
        // past an intro that is off its grid: that bar plays on the entry, nothing before it.
        let dir = folder("cue");
        let a = write_wav(&dir, "a.wav", &drums(120.0, 0.0, 0.0, 90));
        let b = write_wav(&dir, "b.wav", &drums(124.0, 0.5, 0.0, 90));
        let c = write_wav(&dir, "c.wav", &drums(128.0, 0.0, 0.0, 60));
        let beat_b = 60.0 / 124.0;
        let plan = json!({
            "format": "mixforge-plan/2", "master_bpm": 124.0,
            "tracks": [
                {"path": a, "bpm": 120.0, "first_downbeat": 0.0, "audio_end": 90.0, "out_start": 32.0,
                 "transition": {"style": "blend", "bars": 16, "ramp": false}},
                // B's out point is its beat 128: 96 beats after its cue.
                {"path": b, "bpm": 124.0, "first_downbeat": 0.5, "audio_end": 90.0, "start": 0.5 + 32.0 * beat_b,
                 "out_start": 0.5 + 128.0 * beat_b, "transition": {"style": "quick", "bars": 8, "ramp": false}},
                {"path": c, "bpm": 128.0, "first_downbeat": 0.0, "audio_end": 60.0},
            ],
        });
        let path = dir.join("plan.json");
        std::fs::write(&path, plan.to_string()).unwrap();
        let mut e = Engine::default();
        let r = e.execute("dj.import_set", &json!({"plan": path.to_string_lossy()})).unwrap();
        let s = e.session();
        let clip = s.find_clip(ClipId(r["track_list"][1]["clip"].as_u64().unwrap())).unwrap().1;
        assert_eq!(clip.start, bar(&e, 17), "B starts on its entry");
        let w = clip.warp.as_ref().unwrap();
        let heard = w.source_at_time(&s.tempo, s.sample_rate, clip.start, clip.start) / SR as f64;
        assert!((heard - (0.5 + 32.0 * beat_b)).abs() < 1e-3, "B starts {heard} s into its file");
        assert_eq!(r["track_list"][1]["downbeat_bar"], 9);
        // C comes in 96 beats (24 bars) after B's cue: bar 41, not 49 as if B's intro played.
        assert_eq!(r["transitions"][1]["entry_bar"], 41, "{r}");
        assert_eq!(clip.fade_in.len, bar(&e, 33) - bar(&e, 17));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
