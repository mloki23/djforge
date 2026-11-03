//! `dj.key_at`: the key playing at a point of the set, from the keys its tracks' sources carry
//! (the plan's, mixforge's analysis or the file's `TKEY`), for production that follows the set.

use super::recipe::bar_sample;
use super::*;
use soundcraft_midi::Key;

const CMD: &str = "dj.key_at";

/// The beat-locked audio clip playing at `at` (on `track` only, when given), as (track, clip).
/// Where a transition's two clips both play there, the outgoing one owns `at` before the
/// transition's hit bar and the incoming one from it; otherwise the latest-started clip does.
pub(crate) fn owner_at(s: &Session, at: Samples, track: Option<TrackId>) -> Option<ClipId> {
    let under: Vec<&Clip> = s
        .tracks
        .iter()
        .filter(|t| track.is_none_or(|id| t.id == id))
        .flat_map(|t| t.clips())
        .filter(|c| c.warp.is_some() && c.source().is_some() && c.start <= at && at < c.end())
        .collect();
    let mut pick = under.iter().max_by_key(|c| (c.start, c.id))?.id;
    // Back through the transitions into the pick while their hit is still to come: each step
    // goes to an earlier clip, so a chain of them ends.
    for _ in 0..s.dj.transitions.len() {
        let back =
            s.dj.transitions
                .iter()
                .find(|t| t.in_clip == pick && t.out_clip != pick && under.iter().any(|c| c.id == t.out_clip) && at < bar_sample(s, t.hit_bar()));
        match back {
            Some(t) => pick = t.out_clip,
            None => break,
        }
    }
    Some(pick)
}

/// `dj.key_at {at?, track?}`.
pub(super) fn key_at(e: &mut Engine, p: &Value) -> Result<Value> {
    let at = position_param(e, CMD, p, "at")?.unwrap_or(e.session().edit.playhead);
    let track = track_param(e, CMD, p, "track")?;
    let s = e.session();
    let Some(clip) = owner_at(s, at, track) else {
        return Ok(json!({"key": null, "camelot": null, "source": null, "clip": null}));
    };
    let source = s.find_clip(clip).and_then(|(_, c)| c.source());
    // A stored key that doesn't read (a hand-edited session) counts as none.
    let key = source.and_then(|id| s.source(id)).and_then(|x| x.key.as_deref()).and_then(|k| Key::parse(k).ok());
    Ok(json!({
        "key": key.map(|k| k.name()),
        "camelot": key.map(|k| k.camelot()),
        "source": source,
        "clip": clip,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::edit as ops;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, TrackKind, WarpMap};

    const SR: i64 = 48_000;

    /// A session at 128 bpm with beat-locked clips A (bars 1..45, key `a`) and B (bars 25..65,
    /// key `b`) on their own tracks, mixed by a blend entering on bar 25 (its hit on bar 41).
    /// Returns the engine and (A's track, A), (B's track, B).
    fn mixed(a: Option<&str>, b: Option<&str>) -> (Engine, (TrackId, ClipId), (TrackId, ClipId)) {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        let mut out = Vec::new();
        for ((from, to), key) in [((1, 45), a), ((25, 65), b)] {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![0.1; 80 * SR as usize]] };
            let src = crate::io::add_source(&mut s, "deck", buf, None, FileFormat::Wav);
            if let Some(k) = key {
                s.sources.last_mut().unwrap().set_key(k);
            }
            let id = s.new_clip_id();
            let start = bar_sample(&s, from);
            let mut c = Clip::audio(id, "c", src, 0, start, bar_sample(&s, to) - start);
            c.warp = WarpMap::new((0..160).map(|k| 4_800.0 + k as f64 * 22_500.0).collect(), 0.0);
            ops::place_clip(&mut s, t, c);
            out.push((t, id));
        }
        let mut e = Engine::new(s);
        let (a, b) = (out[0].1, out[1].1);
        e.execute("dj.transition", &json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        (e, out[0], out[1])
    }

    fn bar(e: &Engine, n: i64) -> Samples {
        bar_sample(e.session(), n)
    }

    /// `dj.key_at` at `at` (on `track` when given).
    fn key_at(e: &mut Engine, at: Samples, track: Option<TrackId>) -> Value {
        let p = match track {
            Some(t) => json!({"at": at, "track": t.0}),
            None => json!({"at": at}),
        };
        e.execute(CMD, &p).unwrap()
    }

    /// `dj.key_at` at the start of bar `n` (on `track` when given).
    fn key_on(e: &mut Engine, n: i64, track: Option<TrackId>) -> Value {
        let at = bar(e, n);
        key_at(e, at, track)
    }

    #[test]
    fn the_outgoing_key_plays_until_the_hit_bar_and_the_incoming_one_from_it() {
        let (mut e, (_, a), (_, b)) = mixed(Some("8A"), Some("Em"));
        let before = e.session().clone();
        for (at, camelot, key, clip) in [
            (bar(&e, 10), "8A", "Am", a),
            (bar(&e, 30), "8A", "Am", a),
            (bar(&e, 41) - 1, "8A", "Am", a),
            (bar(&e, 41), "9A", "Em", b),
            (bar(&e, 44), "9A", "Em", b),
            (bar(&e, 50), "9A", "Em", b),
        ] {
            let r = key_at(&mut e, at, None);
            assert_eq!((r["camelot"].as_str(), r["key"].as_str(), r["clip"].as_u64()), (Some(camelot), Some(key), Some(clip.0)), "at {at}: {r}");
        }
        assert!(*e.session() == before, "dj.key_at changed the session");
        // Restricted to the incoming deck, its key plays from its start.
        let deck_b = e.session().find_clip(b).unwrap().0;
        let r = key_on(&mut e, 30, Some(deck_b));
        assert_eq!(r["camelot"], "9A", "{r}");
        // The playhead by default.
        e.session_mut().edit.playhead = bar(&e, 42);
        assert_eq!(e.execute(CMD, &json!({})).unwrap()["camelot"], "9A");
    }

    #[test]
    fn without_a_transition_the_latest_started_clip_owns_the_overlap() {
        let (mut e, _, (_, b)) = mixed(Some("8A"), Some("9A"));
        e.session_mut().dj.transitions.clear();
        let r = key_on(&mut e, 30, None);
        assert_eq!((r["camelot"].as_str(), r["clip"].as_u64()), (Some("9A"), Some(b.0)), "{r}");
    }

    #[test]
    fn an_unkeyed_clip_or_none_at_all_gives_no_key() {
        let (mut e, (_, a), _) = mixed(None, Some("9A"));
        let r = key_on(&mut e, 10, None);
        assert_eq!((r["key"].clone(), r["camelot"].clone(), r["clip"].as_u64()), (Value::Null, Value::Null, Some(a.0)), "{r}");
        let r = key_on(&mut e, 200, None);
        assert_eq!(r, json!({"key": null, "camelot": null, "source": null, "clip": null}));
        // A stored key that doesn't read counts as none.
        let src = e.session().sources.last().unwrap().id;
        e.session_mut().sources.iter_mut().find(|x| x.id == src).unwrap().key = Some("nonsense".into());
        assert_eq!(key_on(&mut e, 50, None)["key"], Value::Null);
        assert!(e.execute(CMD, &json!({"track": "nope"})).is_err());
    }
}
