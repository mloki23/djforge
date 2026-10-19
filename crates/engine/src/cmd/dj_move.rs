//! `dj.move_transition`: moves a transition's mix point by whole bars, rippling the rest of the
//! set (every later track and the tempo from the old entry on) so each track keeps its own
//! beats, then rebuilds the moved transitions' recipes.

use super::recipe::{Changes, bar_sample, build, intact};
use super::*;
use soundcraft_model::{MAX_TRANSITION_BAR, MAX_TRANSITION_BARS, Transition};

const CMD: &str = "dj.move_transition";
/// Bars an outgoing clip plays on after its transition's gone bar (as the set import trims it).
const TAIL_BARS: i64 = 16;

/// The transitions after `first` in play order: each one's outgoing clip is the previous one's
/// incoming clip. Indices into `s.dj.transitions`, `first` included.
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
    let id = p.get("transition").and_then(Value::as_u64).ok_or_else(|| bad(CMD, "`transition` required"))?;
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

    // The new tempo map: unchanged before the old entry, shifted by the move after it. Moving
    // later, the gap keeps the tempo playing before the entry (the outgoing track's); moving
    // earlier, the events the move overlaps go.
    let (old_tempo, sr) = (s.tempo.clone(), s.sample_rate);
    let from = old_tempo.bar_start_tick(t.entry_bar);
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

    // Moved clips go `shift` session beats later under the new map, locked there so the relock
    // pass leaves them; the others then follow their beats from the old map to the new one.
    let s = e.session_mut();
    s.tempo = tempo;
    let (old, new) = (Timebase::new(&old_tempo, sr), Timebase::new(&s.tempo, sr));
    let new_fp = new.fp;
    let mut placed = Vec::new();
    for c in &clips {
        let Some(clip) = s.find_clip(*c).map(|(_, c)| c) else { continue };
        let (b0, b1) = (old.beat(clip.start) + shift, old.beat(clip.end()) + shift);
        let start = new.sample(b0).clamp(-MAX_POSITION, MAX_POSITION);
        let length = new.sample(b1).saturating_sub(start).clamp(1, MAX_POSITION);
        placed.push((*c, start, length, new.beat(start) - b0));
    }
    let mut tracks = Vec::new();
    for (c, start, length, drift) in placed {
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
    for t in tracks {
        if let Some(pl) = s.track_mut(t).and_then(|t| t.playlist_mut()) {
            pl.sort();
        }
    }
    let r = relocks(s, &old_tempo, sr);
    apply_relocks(s, &r);

    // Each moved transition's outgoing clip ends TAIL_BARS after its new gone bar, within its audio.
    let mut skipped = Vec::new();
    let mut rebuilt = Vec::new();
    let mut moved_ts: Vec<Transition> = moved.iter().filter_map(|&i| s.dj.transitions.get(i).cloned()).collect();
    for m in &mut moved_ts {
        m.entry_bar = m.entry_bar.saturating_add(bars).clamp(1, MAX_TRANSITION_BAR);
        let tail = bar_sample(s, m.gone_bar().saturating_add(TAIL_BARS));
        if let Some(c) = s.find_clip(m.out_clip).map(|(_, c)| c) {
            let end = tail.min(audio_end(s, c));
            if end > c.start
                && end != c.end()
                && let Some(c) = s.find_clip_mut(m.out_clip)
            {
                c.trim_end_to(end);
            }
        }
    }
    let r = relocks(s, &s.tempo.clone(), sr);
    apply_relocks(s, &r);
    // The moved recipes' old points go first (but not one an unmoved transition also wrote), so
    // the rebuilt ones don't meet them.
    let moved_ids: Vec<u64> = moved_ts.iter().map(|m| m.id).collect();
    let lane = super::recipe::bass_lane();
    let keep: Vec<(TrackId, Samples)> =
        s.dj.transitions
            .iter()
            .filter(|o| !moved_ids.contains(&o.id) || o.edited)
            .filter_map(|o| o.applied.as_ref())
            .flat_map(|a| a.points.iter().map(|p| (p.track, p.at)))
            .collect();
    for m in moved_ts.iter().filter(|m| !m.edited) {
        for p in m.applied.iter().flat_map(|a| a.points.iter()) {
            if !keep.contains(&(p.track, p.at))
                && let Some(tr) = s.track_mut(p.track)
            {
                tr.lane_mut(&lane).points.retain(|q| q.at != p.at);
            }
        }
    }
    for m in &moved_ts {
        if let Some(slot) = s.dj.transition_mut(m.id) {
            slot.entry_bar = m.entry_bar;
            if !m.edited {
                slot.applied = None;
            }
        }
    }
    for m in moved_ts {
        if m.edited {
            skipped.push(json!({"transition": m.id, "reason": "edited by hand; rebuild it with dj.transition {transition, force: true}"}));
            continue;
        }
        let ch = Changes { entry_bar: Some(m.entry_bar), ..Changes::default() };
        let mut current = m.clone();
        current.applied = None;
        match build(e, CMD, Some(current), m.out_clip, m.in_clip, &ch) {
            Ok(b) => rebuilt.push(b.transition.id),
            Err(err) if m.id == id => return Err(err),
            Err(err) => skipped.push(json!({"transition": m.id, "reason": err.to_string()})),
        }
    }
    // Transitions the move didn't touch are where they were: their recipes hold under the new map
    // when what they wrote is still there; otherwise they are stale.
    let s = e.session_mut();
    let fp = soundcraft_model::tempo_fingerprint(&s.tempo, s.sample_rate);
    let untouched: Vec<(usize, bool)> =
        s.dj.transitions
            .iter()
            .enumerate()
            .filter(|(_, o)| !moved_ids.contains(&o.id) && !o.edited && !o.stale)
            .filter_map(|(i, o)| o.applied.as_ref().filter(|a| a.tempo != fp).map(|a| (i, intact(s, a))))
            .collect();
    for (i, ok) in untouched {
        if let Some(o) = s.dj.transitions.get_mut(i) {
            match o.applied.as_mut() {
                Some(a) if ok => a.tempo = fp,
                _ => o.stale = true,
            }
        }
    }
    Ok(json!({"transition": id, "bars": bars, "entry_bar": new_entry, "moved_clips": clips, "rebuilt": rebuilt, "skipped": skipped}))
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
}
