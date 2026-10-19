//! `dj.move_transition`: moves a transition's mix point by whole bars, rippling the rest of the
//! set (the later tracks, every clip, automation point, marker and tempo event from the old entry
//! on) so each track keeps its own beats, then rebuilds the moved transitions' recipes.

use super::recipe::{Changes, bar_sample, build, clips_in_place, intact};
use super::*;
use soundcraft_model::{MAX_TRANSITION_BAR, MAX_TRANSITION_BARS, Transition};

const CMD: &str = "dj.move_transition";
/// Bars an outgoing clip plays on after its transition's gone bar (as the set import trims it).
const TAIL_BARS: i64 = 16;

/// The transitions from `first` on in play order, followed as a linear chain: each one's
/// outgoing clip is the previous one's incoming clip (the first such, if several; a loop ends
/// it). Indices into `s.dj.transitions`, `first` included.
fn chain(s: &Session, first: usize) -> Vec<usize> {
    let mut out = vec![first];
    let mut seen = std::collections::BTreeSet::from([first]);
    while let Some(next) =
        out.last().and_then(|&i| s.dj.transitions.get(i)).and_then(|t| s.dj.transitions.iter().position(|n| n.out_clip == t.in_clip))
    {
        if !seen.insert(next) {
            break;
        }
        out.push(next);
    }
    out
}

/// End (timeline sample) of beat-locked clip `c`'s audio under the session's tempo map, or its
/// own end when that can't be told.
fn audio_end(s: &Session, c: &Clip) -> Samples {
    let (Some(w), Some(frames)) = (c.warp.as_ref(), c.source().and_then(|src| s.source(src)).map(|x| x.frames as f64)) else {
        return c.end();
    };
    let at_start = beat_at(&s.tempo, s.sample_rate, c.start);
    sample_at_beat(&s.tempo, s.sample_rate, at_start + w.beat_at_source(frames) - w.anchor)
}

/// `dj.move_transition {transition, bars}`.
pub(super) fn move_transition(e: &mut Engine, p: &Value) -> Result<Value> {
    let id = match p.get("transition") {
        Some(v) => v.as_u64().ok_or_else(|| bad(CMD, "`transition` must be a transition id"))?,
        None => super::selected_one(e.session())
            .and_then(|c| e.session().dj.transitions.iter().find(|t| t.in_clip == c).map(|t| t.id))
            .ok_or_else(|| bad(CMD, "`transition` required (or select the clip a transition mixes into)"))?,
    };
    let bars = p
        .get("bars")
        .and_then(Value::as_i64)
        .filter(|b| *b != 0 && b.abs() <= MAX_TRANSITION_BARS)
        .ok_or_else(|| bad(CMD, format!("`bars` must be a non-zero whole number -{MAX_TRANSITION_BARS}..={MAX_TRANSITION_BARS}")))?;
    let s = e.session();
    let first = s.dj.transitions.iter().position(|t| t.id == id).ok_or_else(|| bad(CMD, format!("no transition {id}")))?;
    let moved = chain(s, first);
    let t = s.dj.transitions.get(first).cloned().ok_or_else(|| bad(CMD, format!("no transition {id}")))?;
    let new_entry = t.entry_bar.saturating_add(bars);
    if !(1..=MAX_TRANSITION_BAR).contains(&new_entry) {
        return Err(bad(CMD, format!("transition {id} would enter on bar {new_entry}")));
    }
    // An earlier mix point must stay inside the outgoing track and after its own mix in.
    let out_clip = s.find_clip(t.out_clip).map(|(_, c)| c).ok_or_else(|| bad(CMD, format!("clip {} is not on an active playlist", t.out_clip.0)))?;
    if bar_sample(s, new_entry) < out_clip.start {
        return Err(bad(CMD, format!("transition {id} would enter on bar {new_entry}, before its outgoing track starts")));
    }
    if let Some(prev) = s.dj.transitions.iter().find(|p| p.in_clip == t.out_clip)
        && new_entry < prev.gone_bar()
    {
        return Err(bad(
            CMD,
            format!("transition {id} would enter on bar {new_entry}, before transition {}'s bass is gone (bar {})", prev.id, prev.gone_bar()),
        ));
    }
    // What moves: the chain's incoming clips, and every clip starting at or after the old entry.
    // Clips straddling it (the outgoing track) stay.
    let (old_tempo, sr) = (s.tempo.clone(), s.sample_rate);
    let from = old_tempo.bar_start_tick(t.entry_bar);
    let from_sample = old_tempo.tick_to_samples(from, sr);
    let mut clips: Vec<ClipId> = Vec::new();
    for &i in &moved {
        if let Some(m) = s.dj.transitions.get(i)
            && !clips.contains(&m.in_clip)
        {
            clips.push(m.in_clip);
        }
    }
    for c in &clips {
        if s.find_clip(*c).is_none() {
            return Err(bad(CMD, format!("clip {} is not on an active playlist", c.0)));
        }
    }
    for c in s.tracks.iter().flat_map(|t| t.clips()) {
        if c.start >= from_sample && !clips.contains(&c.id) {
            clips.push(c.id);
        }
    }

    // The new tempo map: unchanged before the old entry, shifted by the move after it. Moving
    // later, the gap keeps the tempo playing before the entry (the outgoing track's); moving
    // earlier, the events the move overlaps go.
    let delta = old_tempo.bar_start_tick(new_entry) - from;
    let keep_before = from.min(from + delta);
    let events: Vec<(i64, f64)> = old_tempo
        .tempos()
        .iter()
        .filter(|ev| ev.tick < keep_before)
        .map(|ev| (ev.tick, ev.bpm))
        .chain(old_tempo.tempos().iter().filter(|ev| ev.tick >= from).map(|ev| (ev.tick + delta, ev.bpm)))
        .collect();
    let mut tempo = old_tempo.clone();
    let ticks: Vec<i64> = tempo.tempos().iter().map(|ev| ev.tick).filter(|&k| k != 0).collect();
    for k in ticks {
        tempo.remove_tempo(k);
    }
    for (tick, bpm) in events {
        tempo.set_tempo(tick, bpm).map_err(|err| bad(CMD, err.to_string()))?;
    }
    let shift = delta as f64 / TICKS_PER_QUARTER as f64;
    // A timeline position at or after the old entry, rippled: through ticks of the old map, moved
    // by the delta, back to samples under the new one.
    let ripple = |tempo: &TempoMap, at: Samples| -> Samples {
        let tick = old_tempo.samples_to_ticks_f(at, sr) + delta as f64;
        sample_at_beat(tempo, sr, tick / TICKS_PER_QUARTER as f64)
    };

    let moved_ids: Vec<u64> = moved.iter().filter_map(|&i| s.dj.transitions.get(i)).map(|m| m.id).collect();
    let lane = super::recipe::bass_lane();
    // Points unmoved transitions wrote stay, wherever they are.
    let keep: Vec<(TrackId, Samples)> =
        s.dj.transitions
            .iter()
            .filter(|o| !moved_ids.contains(&o.id))
            .filter_map(|o| o.applied.as_ref())
            .flat_map(|a| a.points.iter().map(|p| (p.track, p.at)))
            .collect();
    let rebuilt_points: Vec<(TrackId, Samples)> =
        s.dj.transitions
            .iter()
            .filter(|o| moved_ids.contains(&o.id) && !o.edited)
            .filter_map(|o| o.applied.as_ref())
            .flat_map(|a| a.points.iter().map(|p| (p.track, p.at)))
            .filter(|p| !keep.contains(p))
            .collect();

    let s = e.session_mut();
    s.tempo = tempo;
    let (old, new) = (Timebase::new(&old_tempo, sr), Timebase::new(&s.tempo, sr));
    let new_fp = new.fp;
    // Automation and markers from the old entry on ripple with the set; the recipes about to be
    // rebuilt lose their old points first.
    for tr in &mut s.tracks {
        let id = tr.id;
        for l in &mut tr.automation {
            let is_bass = l.param == lane;
            let mut moved_points = Vec::new();
            l.points.retain(|p| {
                if is_bass && rebuilt_points.contains(&(id, p.at)) {
                    return false;
                }
                if p.at >= from_sample && !(is_bass && keep.contains(&(id, p.at))) {
                    moved_points.push(*p);
                    return false;
                }
                true
            });
            for p in moved_points {
                l.set_point(ripple(&s.tempo, p.at), p.value);
            }
        }
    }
    for m in &mut s.markers {
        if m.start >= from_sample {
            let len = m.end.saturating_sub(m.start);
            m.start = ripple(&s.tempo, m.start);
            m.end = m.start.saturating_add(len);
        }
    }
    // Beat-locked clips go `shift` session beats later under the new map, locked there so the
    // relock pass leaves them; others move by ticks, keeping their length in samples. The clips
    // that stay then follow their beats from the old map to the new one.
    let mut placed = Vec::new();
    for c in &clips {
        let Some(clip) = s.find_clip(*c).map(|(_, c)| c) else { continue };
        if clip.warp.is_some() {
            let (b0, b1) = (old.beat(clip.start) + shift, old.beat(clip.end()) + shift);
            let start = new.sample(b0).clamp(-MAX_POSITION, MAX_POSITION);
            let length = new.sample(b1).saturating_sub(start).clamp(1, MAX_POSITION);
            placed.push((*c, start, length, new.beat(start) - b0));
        } else {
            placed.push((*c, ripple(&s.tempo, clip.start).clamp(-MAX_POSITION, MAX_POSITION), clip.length, 0.0));
        }
    }
    let mut tracks = Vec::new();
    for &(c, start, length, drift) in &placed {
        let Some(track) = s.find_clip(c).map(|(t, _)| t) else { continue };
        if let Some(clip) = s.find_clip_mut(c) {
            if clip.length != length {
                clip.rescale_to(length);
            }
            clip.start = start;
            let offset = clip.source_offset();
            if let Some(w) = clip.warp.as_mut() {
                w.anchor += drift;
                w.lock = Some(WarpLock { start, length, offset, tempo: new_fp });
            }
        }
        if !tracks.contains(&track) {
            tracks.push(track);
        }
    }
    for &t in &tracks {
        if let Some(pl) = s.track_mut(t).and_then(|t| t.playlist_mut()) {
            pl.sort();
        }
    }
    let r = relocks(s, &old_tempo, sr);
    apply_relocks(s, &r);

    // The moved transitions' new bars.
    let mut moved_ts: Vec<Transition> = moved.iter().filter_map(|&i| s.dj.transitions.get(i).cloned()).collect();
    for m in &mut moved_ts {
        m.entry_bar = m.entry_bar.saturating_add(bars).clamp(1, MAX_TRANSITION_BAR);
    }
    let gone_of = |s: &Session, clip: ClipId| -> Option<(u64, i64)> {
        let o = moved_ts.iter().find(|m| m.out_clip == clip).or_else(|| s.dj.transitions.iter().find(|o| o.out_clip == clip))?;
        Some((o.id, o.gone_bar()))
    };
    // A moved clip that now overlaps one staying on its deck (moving earlier) cuts that one where
    // it starts, when that is after the staying clip's own transition's bass is gone.
    let (mut cut, mut cut_clips) = (Vec::new(), Vec::new());
    for &(c, start, _, _) in &placed {
        let Some(track) = s.find_clip(c).map(|(t, _)| t) else { continue };
        let staying: Vec<ClipId> = s
            .track(track)
            .map(|t| t.clips().iter().filter(|o| !clips.contains(&o.id) && o.start < start && o.end() > start).map(|o| o.id).collect())
            .unwrap_or_default();
        for other in staying {
            if let Some((tid, gone)) = gone_of(s, other)
                && bar_sample(s, gone) > start
            {
                return Err(bad(
                    CMD,
                    format!("clip {} would start before transition {tid}'s bass is gone (bar {gone}) on the same deck; move it less", c.0),
                ));
            }
            if let Some(o) = s.find_clip_mut(other) {
                o.trim_end_to(start);
                cut.push(json!({"clip": other, "ends": start}));
                cut_clips.push(other);
            }
        }
    }
    // Each moved transition's outgoing clip ends TAIL_BARS after its new gone bar: cut back, or
    // extended no further than its audio or the next clip on its deck.
    for m in &moved_ts {
        let tail = bar_sample(s, m.gone_bar().saturating_add(TAIL_BARS));
        let Some((track, c)) = s.find_clip(m.out_clip) else { continue };
        let next = s.track(track).and_then(|t| t.clips().iter().filter(|o| o.id != c.id && o.start >= c.start).map(|o| o.start).min());
        let end = if tail < c.end() { tail } else { tail.min(audio_end(s, c).max(c.end())).min(next.unwrap_or(MAX_POSITION).max(c.end())) };
        if end > c.start
            && end != c.end()
            && let Some(c) = s.find_clip_mut(m.out_clip)
        {
            c.trim_end_to(end);
        }
    }
    let r = relocks(s, &s.tempo.clone(), sr);
    apply_relocks(s, &r);

    // Edited transitions keep their (rippled) points and stay edited; their record follows. The
    // others are rebuilt on their new bars.
    let fp = soundcraft_model::tempo_fingerprint(&s.tempo, s.sample_rate);
    for m in &moved_ts {
        let (s_tempo, edited) = (s.tempo.clone(), m.edited);
        let clip_edges: Vec<(ClipId, Samples, Samples)> =
            m.applied.iter().flat_map(|a| a.fades.iter()).filter_map(|f| s.find_clip(f.clip).map(|(_, c)| (f.clip, c.start, c.end()))).collect();
        let Some(slot) = s.dj.transition_mut(m.id) else { continue };
        slot.entry_bar = m.entry_bar;
        if !edited {
            slot.applied = None;
            continue;
        }
        if let Some(a) = slot.applied.as_mut() {
            for p in &mut a.points {
                if p.at >= from_sample {
                    p.at = ripple(&s_tempo, p.at);
                }
            }
            for sp in &mut a.spans {
                if sp.start >= from_sample {
                    sp.start = ripple(&s_tempo, sp.start);
                }
                if sp.end >= from_sample {
                    sp.end = ripple(&s_tempo, sp.end);
                }
            }
            for f in &mut a.fades {
                if let Some(&(_, start, end)) = clip_edges.iter().find(|x| x.0 == f.clip) {
                    (f.start, f.end) = (start, end);
                }
            }
            a.tempo = fp;
        }
    }
    let (mut rebuilt, mut skipped) = (Vec::new(), Vec::new());
    for m in moved_ts {
        if m.edited {
            skipped.push(json!({"transition": m.id, "reason": "edited by hand: moved with the set but not rebuilt; rebuild it with dj.transition {transition, force: true}"}));
            continue;
        }
        let ch = Changes { entry_bar: Some(m.entry_bar), ..Changes::default() };
        let mut current = m.clone();
        current.applied = None;
        match build(e, CMD, Some(current), m.out_clip, m.in_clip, &ch) {
            Ok(b) => rebuilt.push(b.transition.id),
            Err(err) if m.id == id => return Err(err),
            Err(err) => {
                if let Some(slot) = e.session_mut().dj.transition_mut(m.id) {
                    slot.stale = true;
                }
                skipped.push(json!({"transition": m.id, "reason": format!("rebuild failed: {err}")}));
            }
        }
    }
    // A transition whose outgoing clip the move cut gets its fade-out to the new end.
    let recut: Vec<Transition> =
        e.session().dj.transitions.iter().filter(|o| !moved_ids.contains(&o.id) && !o.edited && cut_clips.contains(&o.out_clip)).cloned().collect();
    for o in recut {
        match build(e, CMD, Some(o.clone()), o.out_clip, o.in_clip, &Changes::default()) {
            Ok(b) => rebuilt.push(b.transition.id),
            Err(err) => {
                if let Some(slot) = e.session_mut().dj.transition_mut(o.id) {
                    slot.stale = true;
                }
                skipped.push(json!({"transition": o.id, "reason": format!("rebuild failed: {err}")}));
            }
        }
    }
    // Transitions the move didn't touch are where they were: their recipes hold under the new map
    // when what they wrote is still there; otherwise they are stale.
    let s = e.session_mut();
    let untouched: Vec<(usize, bool)> =
        s.dj.transitions
            .iter()
            .enumerate()
            .filter(|(_, o)| !moved_ids.contains(&o.id) && !o.edited && !o.stale)
            .filter_map(|(i, o)| o.applied.as_ref().map(|a| (i, intact(s, a, true) && clips_in_place(s, a))))
            .collect();
    for (i, ok) in untouched {
        if let Some(o) = s.dj.transitions.get_mut(i) {
            match o.applied.as_mut() {
                Some(a) if ok => a.tempo = fp,
                _ => o.stale = true,
            }
        }
    }
    Ok(json!({"transition": id, "bars": bars, "entry_bar": new_entry, "moved_clips": clips, "cut": cut, "rebuilt": rebuilt, "skipped": skipped}))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::edit as ops;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, TrackKind, WarpMap};

    const SR: i64 = 48_000;

    /// A set at 128 bpm (132 from bar 45) on two decks: A (deck 0, bars 1..61) into B (deck 1,
    /// bars 25..101) entering on bar 25, then B into C (deck 0, bars 73..140) on bar 73. Every
    /// clip is beat-locked to a 128 bpm grid over one 200 s source.
    fn set() -> (Engine, Vec<ClipId>) {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        let decks = [s.add_track(TrackKind::Audio, ChannelFormat::Mono, None), s.add_track(TrackKind::Audio, ChannelFormat::Mono, None)];
        let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![0.0; 200 * SR as usize]] };
        let src = crate::io::add_source(&mut s, "deck", buf, None, FileFormat::Wav);
        let mut ids = Vec::new();
        for (deck, from, to) in [(0, 1, 61), (1, 25, 101), (0, 73, 140)] {
            let id = s.new_clip_id();
            let start = bar_sample(&s, from);
            let mut c = Clip::audio(id, "c", src, 0, start, bar_sample(&s, to) - start);
            c.warp = WarpMap::new((0..420).map(|k| k as f64 * 22_500.0).collect(), 0.0);
            ops::place_clip(&mut s, decks[deck], c);
            ids.push(id);
        }
        let mut e = Engine::new(s);
        e.execute("event.tempo", &json!({"bpm": 132.0, "at": "bars_beats:45|1|000"})).unwrap();
        e.execute("dj.transition", &json!({"out_clip": ids[0].0, "in_clip": ids[1].0, "entry_bar": 25})).unwrap();
        e.execute("dj.transition", &json!({"out_clip": ids[1].0, "in_clip": ids[2].0, "entry_bar": 73})).unwrap();
        (e, ids)
    }

    /// Track beat clip `c` plays at the start of `bar`.
    fn beat_heard(e: &Engine, c: ClipId, bar: i64) -> f64 {
        let s = e.session();
        let clip = s.find_clip(c).unwrap().1;
        clip.warp.as_ref().unwrap().track_beat_at(&s.tempo, s.sample_rate, clip.start, bar_sample(s, bar))
    }

    fn entries(e: &Engine) -> Vec<i64> {
        e.session().dj.transitions.iter().map(|t| t.entry_bar).collect()
    }

    fn mv(e: &mut Engine, id: u64, bars: i64) -> Result<Value> {
        e.execute("dj.move_transition", &json!({"transition": id, "bars": bars}))
    }

    #[test]
    fn moving_a_transition_later_ripples_the_rest_of_the_set() {
        let (mut e, c) = set();
        let before = e.session().clone();
        let heard =
            [beat_heard(&e, c[0], 10), beat_heard(&e, c[0], 40), beat_heard(&e, c[1], 30), beat_heard(&e, c[1], 90), beat_heard(&e, c[2], 80)];
        let tempo_bar_45 = e.session().tempo.bar_start_tick(45);
        let r = mv(&mut e, 1, 4).unwrap();
        assert_eq!(r["rebuilt"], json!([1, 2]), "{r}");
        assert_eq!(entries(&e), vec![29, 77]);
        let s = e.session();
        // The incoming clips start 4 bars later; A, before the entry, doesn't move.
        let start = |i: usize| s.find_clip(c[i]).unwrap().1.start;
        assert_eq!((start(0), start(1), start(2)), (bar_sample(s, 1), bar_sample(s, 29), bar_sample(s, 77)));
        // Every track plays the same beats at its own (moved) bars.
        let now = [beat_heard(&e, c[0], 10), beat_heard(&e, c[0], 40), beat_heard(&e, c[1], 34), beat_heard(&e, c[1], 94), beat_heard(&e, c[2], 84)];
        for (a, b) in heard.iter().zip(&now) {
            // Within a sample's rounding (1e-4 beats is 0.05 ms).
            assert!((a - b).abs() < 1e-4, "{heard:?} vs {now:?}");
        }
        // The tempo change on bar 45 is now on bar 49; 128 bpm fills the gap.
        assert!(s.tempo.tempos().iter().any(|t| t.tick == s.tempo.bar_start_tick(49) && t.bpm == 132.0));
        assert!(!s.tempo.tempos().iter().any(|t| t.tick == tempo_bar_45));
        // A now ends 16 bars after transition 1's gone bar (53); B after transition 2's (101).
        assert_eq!(s.find_clip(c[0]).unwrap().1.end(), bar_sample(s, 69));
        assert_eq!(s.find_clip(c[1]).unwrap().1.end(), bar_sample(s, 117));
        // The recipes are rebuilt on the new bars, and nothing reads as edited or stale.
        let t = &s.dj.transitions;
        assert!(t.iter().all(|t| !t.edited && !t.stale && t.applied.is_some()));
        let pts: Vec<Samples> = t[1].applied.as_ref().unwrap().points.iter().map(|p| p.at).collect();
        assert_eq!(pts, vec![bar_sample(s, 77), bar_sample(s, 93), bar_sample(s, 93), bar_sample(s, 101)]);
        // One undo puts everything back.
        assert!(e.undo());
        assert!(*e.session() == before);
    }

    #[test]
    fn moving_earlier_drops_the_overlapped_tempo_and_keeps_the_beats() {
        let (mut e, c) = set();
        let heard = [beat_heard(&e, c[1], 30), beat_heard(&e, c[2], 80)];
        mv(&mut e, 2, -8).unwrap();
        assert_eq!(entries(&e), vec![25, 65]);
        assert!((beat_heard(&e, c[1], 30) - heard[0]).abs() < 1e-4, "B, before the entry, keeps its beats");
        assert!((beat_heard(&e, c[2], 72) - heard[1]).abs() < 1e-4);
        assert!(e.session().dj.transitions.iter().all(|t| !t.edited && !t.stale));
    }

    #[test]
    fn an_edited_later_transition_is_moved_but_not_rebuilt() {
        let (mut e, _) = set();
        e.session_mut().dj.transition_mut(2).unwrap().edited = true;
        let r = mv(&mut e, 1, 4).unwrap();
        assert_eq!(r["rebuilt"], json!([1]));
        assert_eq!(r["skipped"][0]["transition"], 2, "{r}");
        assert!(r["skipped"][0]["reason"].as_str().unwrap().contains("edited by hand"));
        assert_eq!(entries(&e), vec![29, 77]);
    }

    #[test]
    fn a_move_too_early_is_refused_and_changes_nothing() {
        let (mut e, _) = set();
        let before = e.session().clone();
        for (id, bars, want) in [
            (1, -25, "would enter on bar 0"),
            (2, -60, "before its outgoing track starts"),
            (2, -30, "before transition 1's bass is gone"),
            (1, 0, "`bars`"),
            (9, 4, "no transition 9"),
        ] {
            let err = mv(&mut e, id, bars).unwrap_err().to_string();
            assert!(err.contains(want), "{id} by {bars}: {err}");
            assert!(*e.session() == before);
        }
    }

    /// Clip `c`'s deck.
    fn deck(e: &Engine, c: ClipId) -> TrackId {
        e.session().find_clip(c).unwrap().0
    }

    #[test]
    fn moving_ripples_automation_markers_and_other_clips_after_the_entry() {
        let (mut e, c) = set();
        let b_deck = deck(&e, c[1]);
        let (at20, at90, at100) = (bar_sample(e.session(), 20), bar_sample(e.session(), 90), bar_sample(e.session(), 100));
        for at in [at20, at90] {
            e.execute("automation.set_point", &json!({"track": b_deck.0, "param": "volume", "at": at, "value": -6.0})).unwrap();
        }
        e.execute("markers.add", &json!({"name": "drop", "at": at100})).unwrap();
        // A clip on another track, not part of any transition, on bar 95.
        let s = e.session_mut();
        let src = s.find_clip(c[0]).unwrap().1.source().unwrap();
        let other = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
        let id = s.new_clip_id();
        let start = bar_sample(s, 95);
        ops::place_clip(s, other, Clip::audio(id, "fx", src, 0, start, SR));
        mv(&mut e, 1, 4).unwrap();
        let s = e.session();
        let near = |a: Samples, bar: i64| (a - bar_sample(s, bar)).abs() <= 1;
        let vol: Vec<Samples> = s.track(b_deck).unwrap().lane(&soundcraft_model::AutoParam::Volume).unwrap().points.iter().map(|p| p.at).collect();
        assert!(vol.len() == 2 && vol[0] == at20 && near(vol[1], 94), "{vol:?}");
        assert!(near(s.markers[0].start, 104), "marker at {}", s.markers[0].start);
        assert!(near(s.find_clip(id).unwrap().1.start, 99));
        assert_eq!(s.find_clip(id).unwrap().1.length, SR, "a clip that isn't beat-locked keeps its length");
    }

    #[test]
    fn moving_earlier_cuts_the_clip_it_now_overlaps_after_its_mix() {
        let (mut e, c) = set();
        // C goes from bar 73 to bar 49 on deck 0, where A plays until bar 61; A's mix is gone by 49.
        let r = mv(&mut e, 2, -24).unwrap();
        let s = e.session();
        assert_eq!(s.find_clip(c[2]).unwrap().1.start, bar_sample(s, 49));
        assert_eq!(s.find_clip(c[0]).unwrap().1.end(), bar_sample(s, 49));
        assert_eq!(r["cut"][0]["clip"], c[0].0, "{r}");
        assert!(s.dj.transitions.iter().all(|t| !t.edited && !t.stale), "{:?}", s.dj.transitions);
    }

    #[test]
    fn an_edited_transition_keeps_its_points_in_step_with_its_music() {
        let (mut e, c) = set();
        let c_deck = deck(&e, c[2]);
        let at80 = bar_sample(e.session(), 80);
        e.execute("automation.set_point", &json!({"track": c_deck.0, "param": "plugin:a:low_shelf_gain", "at": at80, "value": -6.0})).unwrap();
        assert!(e.session().dj.transition(2).unwrap().edited);
        mv(&mut e, 1, 4).unwrap();
        // Another command: transition 2 is still edited, not stale, and its record matches.
        e.execute("automation.set_point", &json!({"track": c_deck.0, "param": "volume", "at": 0, "value": 0.0})).unwrap();
        let s = e.session();
        let t = s.dj.transition(2).unwrap();
        assert!(t.edited && !t.stale && t.entry_bar == 77, "{t:?}");
        let lane = super::super::recipe::bass_lane();
        let deck_points: Vec<Samples> = s.track(c_deck).unwrap().lane(&lane).unwrap().points.iter().map(|p| p.at).collect();
        let near = |a: Samples, bar: i64| (a - bar_sample(s, bar)).abs() <= 1;
        // Transition 1's out points (bars 45, 53 after the move), then 2's build with the hand point.
        assert!(deck_points.iter().any(|&a| near(a, 84)), "the hand point moved with the music: {deck_points:?}");
        let applied: Vec<Samples> = t.applied.as_ref().unwrap().points.iter().map(|p| p.at).collect();
        for (a, bar) in applied.iter().zip([77, 93, 93, 101]) {
            assert!(near(*a, bar), "{applied:?}");
        }
    }
}
