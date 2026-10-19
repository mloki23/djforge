//! Marty's transition recipe: the incoming track fades in while its bass builds up under a low
//! shelf, its bass hits in full on the hit bar, then the outgoing track's bass goes out and it
//! fades out to its end. [`recipe`] works out the positions; [`build`] (`dj.transition`) records
//! a transition and writes them into the session, remembering what it wrote ([`Applied`]) so
//! [`marks`] can tell hand edits and tempo changes from it.

use super::*;
use soundcraft_model::{Applied, AppliedFade, AppliedPoint, AutoParam, Fade, FadeShape, Insert, NudgeSource, OwnedSpan, Transition, TransitionStyle};
use soundcraft_model::{MAX_TRANSITION_BAR, MAX_TRANSITION_BARS, MIN_TRANSITION_BARS};

/// The insert slot (a) the recipe's EQ sits in.
pub(crate) const EQ_SLOT: usize = 0;
/// The built-in EQ the recipe automates.
pub(crate) const EQ_PLUGIN: &str = "eq_7band";
/// The EQ parameter the bass moves with (raw dB).
pub(crate) const BASS_PARAM: &str = "low_shelf_gain";
/// Low shelf corner a newly inserted EQ gets (Hz).
const SHELF_FREQ: f32 = 200.0;
/// Low shelf gain while a bass is out (dB).
pub(crate) const BASS_OUT_DB: f32 = -24.0;
/// Low shelf gain while a bass plays in full (dB).
pub(crate) const BASS_IN_DB: f32 = 0.0;
/// Commands that write transitions themselves, so their changes are not hand edits.
pub(crate) const WRITERS: [&str; 3] = ["dj.transition", "dj.import_set", "dj.move_transition"];

/// The automation lane the recipe writes on each deck.
pub(crate) fn bass_lane() -> AutoParam {
    AutoParam::Plugin { slot: EQ_SLOT as u8, param: BASS_PARAM.to_string() }
}

/// Where a transition's recipe puts things, in timeline samples.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recipe {
    /// Start of `entry_bar`: the incoming bass starts building.
    pub entry: Samples,
    /// Start of the hit bar: the incoming bass is in full, the outgoing one starts going.
    pub hit: Samples,
    /// Start of the gone bar: the outgoing bass is out.
    pub gone: Samples,
    /// Equal-power fade-in of the incoming clip: from its start to `hit`.
    pub in_fade: Samples,
    /// Equal-power fade-out of the outgoing clip: from `hit` to its end.
    pub out_fade: Samples,
    /// Low shelf gain breakpoints (track, at, dB) on [`bass_lane`].
    pub points: Vec<(TrackId, Samples, f32)>,
    /// The stretch of each deck's [`bass_lane`] the transition owns: the incoming deck
    /// `[entry, hit]`, the outgoing one `[hit, gone]`. Points strictly inside are cleared before
    /// the recipe's are written at the ends.
    pub spans: [OwnedSpan; 2],
}

/// The clip `id` on track `track`'s active playlist.
pub(crate) fn clip_on(s: &Session, track: TrackId, id: ClipId) -> Option<&Clip> {
    s.track(track)?.clips().iter().find(|c| c.id == id)
}

fn clip_on_mut(s: &mut Session, track: TrackId, id: ClipId) -> Option<&mut Clip> {
    s.track_mut(track)?.playlist_mut()?.clip_mut(id)
}

/// Why clip `id` can't be used from track `track`: missing, or only on an inactive playlist.
fn not_on(s: &Session, track: TrackId, id: ClipId) -> String {
    let inactive = s.track(track).is_some_and(|t| t.playlists.iter().any(|p| p.clips.iter().any(|c| c.id == id)));
    if inactive {
        format!("clip {} is on an inactive playlist of track {}; make that playlist active to build the transition", id.0, track.0)
    } else {
        format!("clip {} is not on track {}", id.0, track.0)
    }
}

/// The track whose active playlist holds clip `id`.
fn track_of(s: &Session, id: ClipId) -> Option<TrackId> {
    s.find_clip(id).map(|(t, _)| t)
}

/// Timeline sample of the start of `bar` (1-based).
pub(crate) fn bar_sample(s: &Session, bar: i64) -> Samples {
    s.tempo.tick_to_samples(s.tempo.bar_start_tick(bar), s.sample_rate)
}

/// The first bar starting at or after timeline sample `at`.
fn bar_at_or_after(s: &Session, at: Samples) -> i64 {
    let bb = s.tempo.bar_beat_at(at, s.sample_rate);
    if bar_sample(s, bb.bar) < at { bb.bar.saturating_add(1) } else { bb.bar }
}

/// Transition `t`'s recipe in session `s`, or why it can't be built.
///
/// Both clips must be on their tracks' active playlists. The incoming clip must start before the
/// hit bar (its fade-in runs from its start, even when that is before or after `entry_bar`, to the
/// hit) and last until it. The outgoing clip must hold the hit: one ending on or before it is
/// refused rather than given a clamped fade, which would cut the outgoing track before the
/// incoming bass arrives. A fade that would overlap the clip's other fade is refused too.
pub(crate) fn recipe(s: &Session, t: &Transition) -> std::result::Result<Recipe, String> {
    let find = |track: TrackId, id: ClipId| clip_on(s, track, id).ok_or_else(|| not_on(s, track, id));
    let (out_clip, in_clip) = (find(t.out_track, t.out_clip)?, find(t.in_track, t.in_clip)?);
    let (entry, hit, gone) = (bar_sample(s, t.entry_bar), bar_sample(s, t.hit_bar()), bar_sample(s, t.gone_bar()));
    if in_clip.start >= hit {
        return Err(format!("the incoming clip {} starts on or after the bass hit (bar {}); enter earlier", t.in_clip.0, t.hit_bar()));
    }
    if in_clip.end() < hit {
        return Err(format!("the incoming clip {} ends before the bass hit (bar {})", t.in_clip.0, t.hit_bar()));
    }
    if out_clip.start > hit {
        return Err(format!("the outgoing clip {} starts after the bass hit (bar {})", t.out_clip.0, t.hit_bar()));
    }
    if out_clip.end() <= hit {
        return Err(format!(
            "the outgoing clip {} ends on or before the bass hit (bar {}); enter earlier or build over fewer bars",
            t.out_clip.0,
            t.hit_bar()
        ));
    }
    let (in_fade, out_fade) = (hit - in_clip.start, out_clip.end() - hit);
    if in_fade.saturating_add(in_clip.fade_out.len) > in_clip.length {
        return Err(format!("conflict: the incoming clip {}'s fade-in to bar {} would overlap its fade-out", t.in_clip.0, t.hit_bar()));
    }
    if out_fade.saturating_add(out_clip.fade_in.len) > out_clip.length {
        return Err(format!("conflict: the outgoing clip {}'s fade-out from bar {} would overlap its fade-in", t.out_clip.0, t.hit_bar()));
    }
    Ok(Recipe {
        entry,
        hit,
        gone,
        in_fade,
        out_fade,
        points: vec![
            (t.in_track, entry, BASS_OUT_DB),
            (t.in_track, hit, BASS_IN_DB),
            (t.out_track, hit, BASS_IN_DB),
            (t.out_track, gone, BASS_OUT_DB),
        ],
        spans: [OwnedSpan { track: t.in_track, start: entry, end: hit }, OwnedSpan { track: t.out_track, start: hit, end: gone }],
    })
}

/// What applying recipe `r` of `t` writes, under the session's current tempo map.
fn applied(s: &Session, t: &Transition, r: &Recipe) -> Applied {
    Applied {
        tempo: soundcraft_model::tempo_fingerprint(&s.tempo, s.sample_rate),
        points: r.points.iter().map(|&(track, at, value)| AppliedPoint { track, at, value }).collect(),
        spans: r.spans.to_vec(),
        fades: vec![
            AppliedFade { clip: t.in_clip, track: t.in_track, fade_in: true, len: r.in_fade },
            AppliedFade { clip: t.out_clip, track: t.out_track, fade_in: false, len: r.out_fade },
        ],
    }
}

/// A flag the session's state calls for on a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    /// The tempo map or rate changed since its recipe was applied.
    Stale,
    /// Its points or fades were changed by hand.
    Edited,
}

/// Whether what `a` recorded is still in the session: every point at its sample with its value,
/// no other point strictly inside an owned span, and every fade at its length on its clip.
fn intact(s: &Session, a: &Applied) -> bool {
    let lane = bass_lane();
    let points = |track: TrackId| s.track(track).and_then(|t| t.lane(&lane)).map_or(&[][..], |l| l.points.as_slice());
    let kept = a.points.iter().all(|p| points(p.track).iter().any(|q| q.at == p.at && (q.value - p.value).abs() <= 1e-4));
    let clean = a.spans.iter().all(|sp| !points(sp.track).iter().any(|q| q.at > sp.start && q.at < sp.end));
    let fades = a.fades.iter().all(|f| clip_on(s, f.track, f.clip).is_some_and(|c| (if f.fade_in { c.fade_in } else { c.fade_out }).len == f.len));
    kept && clean && fades
}

/// The flags session `s` calls for (transition index, flag), without changing it: a transition
/// whose recipe was applied under another tempo map is stale (a tempo change moves clips and
/// rescales fades, which is not a hand edit); one that isn't, whose recorded writes are no longer
/// all there, was edited by hand. A consistent session (after an undo, say) calls for none.
pub(crate) fn marks(s: &Session) -> Vec<(usize, Mark)> {
    let fp = soundcraft_model::tempo_fingerprint(&s.tempo, s.sample_rate);
    let mut out = Vec::new();
    for (i, t) in s.dj.transitions.iter().enumerate() {
        let Some(a) = t.applied.as_ref().filter(|_| !t.stale) else { continue };
        if a.tempo != fp {
            out.push((i, Mark::Stale));
        } else if !t.edited && !intact(s, a) {
            out.push((i, Mark::Edited));
        }
    }
    out
}

/// Sets the flags [`marks`] found on the session it was computed from.
pub(crate) fn apply_marks(s: &mut Session, marks: &[(usize, Mark)]) {
    for &(i, m) in marks {
        if let Some(t) = s.dj.transitions.get_mut(i) {
            match m {
                Mark::Stale => t.stale = true,
                Mark::Edited => t.edited = true,
            }
        }
    }
}

/// `{transition: id}` or `{out_clip, in_clip}`'s existing transition (`None`: a new one between
/// those clips), with the clip pair.
fn target(s: &Session, p: &Value, cmd: &str) -> Result<(Option<Transition>, ClipId, ClipId)> {
    let clip = |key: &str| match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(|n| Some(ClipId(n))).ok_or_else(|| bad(cmd, format!("`{key}` must be a clip id"))),
    };
    let (out_id, in_id) = (clip("out_clip")?, clip("in_clip")?);
    match (p.get("transition").filter(|v| !v.is_null()), out_id, in_id) {
        (Some(v), None, None) => {
            let id = v.as_u64().ok_or_else(|| bad(cmd, "`transition` must be a transition id"))?;
            let t = s.dj.transition(id).cloned().ok_or_else(|| bad(cmd, format!("no transition {id}")))?;
            let (o, i) = (t.out_clip, t.in_clip);
            Ok((Some(t), o, i))
        }
        (None, Some(o), Some(i)) => {
            if o == i {
                return Err(bad(cmd, "`out_clip` and `in_clip` must be different clips"));
            }
            Ok((s.dj.transitions.iter().find(|t| t.out_clip == o && t.in_clip == i).cloned(), o, i))
        }
        _ => Err(bad(cmd, "give `transition`, or `out_clip` and `in_clip`")),
    }
}

/// Optional whole-number parameter `key` within `lo..=hi`.
fn int_in(p: &Value, key: &str, lo: i64, hi: i64, cmd: &str) -> Result<Option<i64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            v.as_i64().filter(|n| (lo..=hi).contains(n)).map(Some).ok_or_else(|| bad(cmd, format!("`{key}` must be a whole number {lo}..={hi}")))
        }
    }
}

/// `dj.transition`: creates or updates a transition, then writes its recipe into the session.
pub(super) fn transition(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "dj.transition";
    let s = e.session();
    let (existing, out_id, in_id) = target(s, p, CMD)?;
    let style: Option<TransitionStyle> = match p.get("style") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v.clone()).map_err(|_| bad(CMD, "`style` must be \"blend\" or \"quick\""))?),
    };
    let build_bars = int_in(p, "build_bars", MIN_TRANSITION_BARS, MAX_TRANSITION_BARS, CMD)?;
    let out_bars = int_in(p, "out_bars", MIN_TRANSITION_BARS, MAX_TRANSITION_BARS, CMD)?;
    let entry = int_in(p, "entry_bar", 1, MAX_TRANSITION_BAR, CMD)?;
    let changes = Changes { style, build_bars, out_bars, entry_bar: entry, force: bool_or(p, "force", false) };
    let Built { transition: t, recipe: r, eq_inserted } = build(e, CMD, existing, out_id, in_id, &changes)?;
    let points: Vec<Value> = r.points.iter().map(|(track, at, db)| json!({"track": track, "at": at, "value": db})).collect();
    let transition = serde_json::to_value(&t).map_err(|err| bad(CMD, err.to_string()))?;
    Ok(json!({
        "transition": transition,
        "hit_bar": t.hit_bar(),
        "gone_bar": t.gone_bar(),
        "recipe": {"entry": r.entry, "hit": r.hit, "gone": r.gone, "in_fade": r.in_fade, "out_fade": r.out_fade, "points": points},
        "eq_inserted": eq_inserted,
    }))
}

/// What to change on a transition before its recipe is (re)applied; `None` keeps the current
/// value (a new transition's defaults).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Changes {
    /// A style given without bars brings its own.
    pub style: Option<TransitionStyle>,
    pub build_bars: Option<i64>,
    pub out_bars: Option<i64>,
    pub entry_bar: Option<i64>,
    /// Rebuild a transition edited by hand.
    pub force: bool,
}

/// A transition [`build`] recorded, its recipe, and the decks it inserted the EQ on.
pub(crate) struct Built {
    pub transition: Transition,
    pub recipe: Recipe,
    pub eq_inserted: Vec<TrackId>,
}

/// Records transition `existing` (`None`: a new one) from `out_id` into `in_id` with `ch`
/// applied, and writes its recipe into the session: the points it wrote before are removed
/// (unless another transition wrote the same one), any other point strictly inside the spans it
/// now owns is cleared, and its points and fades are written. A span overlapping another
/// transition's on the same deck is refused as a conflict. Everything is checked before the
/// session changes; errors name command `cmd`. A stale transition is rebuilt on its bars under
/// the current tempo map.
pub(crate) fn build(e: &mut Engine, cmd: &str, existing: Option<Transition>, out_id: ClipId, in_id: ClipId, ch: &Changes) -> Result<Built> {
    let s = e.session();
    let Changes { style, build_bars: build, out_bars: out, entry_bar: entry, force } = *ch;
    if let Some(t) = existing.as_ref().filter(|t| t.edited && !force) {
        return Err(bad(cmd, format!("transition {} was edited by hand; pass force: true to rebuild it", t.id)));
    }
    // The decks are wherever the clips are now.
    let deck = |id: ClipId| {
        track_of(s, id).ok_or_else(|| match s.tracks.iter().find(|t| t.playlists.iter().any(|p| p.clips.iter().any(|c| c.id == id))) {
            Some(t) => bad(cmd, not_on(s, t.id, id)),
            None => bad(cmd, format!("no clip {}", id.0)),
        })
    };
    let (out_track, in_track) = (deck(out_id)?, deck(in_id)?);
    if out_track == in_track {
        return Err(bad(cmd, "the two clips must be on different tracks"));
    }
    for (track, id) in [(out_track, out_id), (in_track, in_id)] {
        if !clip_on(s, track, id).is_some_and(Clip::is_audio) {
            return Err(bad(cmd, format!("clip {} is not an audio clip", id.0)));
        }
    }
    let mut t = match existing.clone() {
        Some(t) => t,
        None => {
            let style = style.unwrap_or(TransitionStyle::Blend);
            let (build_bars, out_bars) = style.default_bars();
            let in_start = clip_on(s, in_track, in_id).map_or(0, |c| c.start);
            Transition {
                id: s.dj.next_id(),
                out_clip: out_id,
                in_clip: in_id,
                out_track,
                in_track,
                entry_bar: bar_at_or_after(s, in_start).clamp(1, MAX_TRANSITION_BAR),
                style,
                build_bars,
                out_bars,
                nudge_beats: 0.0,
                nudge_source: NudgeSource::None,
                suggestion: None,
                edited: false,
                stale: false,
                applied: None,
            }
        }
    };
    t.out_track = out_track;
    t.in_track = in_track;
    if let Some(style) = style {
        // A style given without bars brings its own.
        let (b, o) = style.default_bars();
        t.style = style;
        t.build_bars = b;
        t.out_bars = o;
    }
    t.build_bars = build.unwrap_or(t.build_bars);
    t.out_bars = out.unwrap_or(t.out_bars);
    t.entry_bar = entry.unwrap_or(t.entry_bar);
    let r = recipe(s, &t).map_err(|m| bad(cmd, m))?;
    let others: Vec<&Transition> = s.dj.transitions.iter().filter(|o| o.id != t.id).collect();
    for span in &r.spans {
        let clash = others.iter().find_map(|o| o.applied.as_ref()?.spans.iter().any(|x| x.overlaps(span)).then_some(o.id));
        if let Some(other) = clash {
            return Err(bad(
                cmd,
                format!(
                    "conflict: transition {}'s bass span on track {} overlaps transition {other}'s; move or shorten one of them",
                    t.id, span.track.0
                ),
            ));
        }
    }
    for track in [out_track, in_track] {
        let slot =
            s.track(track).and_then(|tr| tr.mixer.inserts.get(EQ_SLOT)).ok_or_else(|| bad(cmd, format!("track {} has no insert slot a", track.0)))?;
        if let Some(ins) = slot.as_ref().filter(|i| i.plugin != EQ_PLUGIN) {
            return Err(bad(
                cmd,
                format!(
                    "insert slot a of track {} holds `{}`; the transition needs EQ 7-Band there, so move or remove it first",
                    track.0, ins.plugin
                ),
            ));
        }
    }
    let info = super::super::clap::plugin_info(EQ_PLUGIN).ok_or_else(|| bad(cmd, "the EQ 7-Band plugin is missing"))?;
    // The points written before, except any another transition wrote too.
    let shared =
        |p: &AppliedPoint| others.iter().any(|o| o.applied.as_ref().is_some_and(|a| a.points.iter().any(|q| q.track == p.track && q.at == p.at)));
    let stale_points: Vec<AppliedPoint> =
        existing.as_ref().and_then(|o| o.applied.as_ref()).map_or_else(Vec::new, |a| a.points.iter().filter(|p| !shared(p)).copied().collect());
    t.applied = Some(applied(s, &t, &r));
    t.edited = false;
    t.stale = false;

    let s = e.session_mut();
    let mut inserted = Vec::new();
    for track in [out_track, in_track] {
        if let Some(slot) = s.track_mut(track).and_then(|tr| tr.mixer.inserts.get_mut(EQ_SLOT))
            && slot.is_none()
        {
            let mut ins = Insert::new(info.id);
            for pi in info.params {
                ins.params.insert(pi.id.to_string(), pi.default);
            }
            ins.params.insert("low_shelf_freq".to_string(), SHELF_FREQ);
            *slot = Some(ins);
            inserted.push(track);
        }
    }
    let lane = bass_lane();
    for p in &stale_points {
        if let Some(tr) = s.track_mut(p.track) {
            tr.lane_mut(&lane).points.retain(|q| q.at != p.at);
        }
    }
    for span in &r.spans {
        if let Some(tr) = s.track_mut(span.track) {
            tr.lane_mut(&lane).clear_range(span.start.saturating_add(1), span.end);
        }
    }
    for &(track, at, db) in &r.points {
        if let Some(tr) = s.track_mut(track) {
            tr.lane_mut(&lane).set_point(at, db);
        }
    }
    if let Some(c) = clip_on_mut(s, in_track, in_id) {
        c.fade_in = Fade { len: r.in_fade, shape: FadeShape::EqualPower, curvature: 0.0 };
    }
    if let Some(c) = clip_on_mut(s, out_track, out_id) {
        c.fade_out = Fade { len: r.out_fade, shape: FadeShape::EqualPower, curvature: 0.0 };
    }
    match s.dj.transition_mut(t.id) {
        Some(slot) => *slot = t.clone(),
        None => s.dj.transitions.push(t.clone()),
    }
    Ok(Built { transition: t, recipe: r, eq_inserted: inserted })
}

/// `dj.transitions`: every transition, with its hit and gone bars.
pub(super) fn transitions(e: &mut Engine, _: &Value) -> Result<Value> {
    let list = e.session().dj.transitions.iter().map(|t| {
        let mut v = serde_json::to_value(t).map_err(|err| bad("dj.transitions", err.to_string()))?;
        if let Some(o) = v.as_object_mut() {
            o.insert("hit_bar".to_string(), json!(t.hit_bar()));
            o.insert("gone_bar".to_string(), json!(t.gone_bar()));
        }
        Ok(v)
    });
    Ok(Value::Array(list.collect::<Result<Vec<Value>>>()?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::edit as ops;
    use soundcraft_audio_io::{AudioBuffer, FileFormat};
    use soundcraft_model::{ChannelFormat, TrackKind, WarpMap};

    const SR: i64 = 48_000;

    /// A session at 128 bpm (a bar is 1.875 s) with two beat-locked clips on their own tracks:
    /// A on bars 1..45, B on bars 25..65. Returns the engine, (A's track, A), (B's track, B).
    fn decks() -> (Engine, (TrackId, ClipId), (TrackId, ClipId)) {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        let mut out = Vec::new();
        for (from, to) in [(1, 45), (25, 65)] {
            let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, None);
            let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![0.1; 80 * SR as usize]] };
            let src = crate::io::add_source(&mut s, "deck", buf, None, FileFormat::Wav);
            let id = s.new_clip_id();
            let start = bar_sample(&s, from);
            let mut c = Clip::audio(id, "c", src, 0, start, bar_sample(&s, to) - start);
            c.warp = WarpMap::new((0..160).map(|k| 4_800.0 + k as f64 * 22_500.0).collect(), 0.0);
            ops::place_clip(&mut s, t, c);
            out.push((t, id));
        }
        (Engine::new(s), out[0], out[1])
    }

    fn apply(e: &mut Engine, p: Value) -> Result<Value> {
        e.execute("dj.transition", &p)
    }

    /// (at, value) of track `t`'s bass lane.
    fn points(e: &Engine, t: TrackId) -> Vec<(Samples, f32)> {
        e.session().track(t).unwrap().lane(&bass_lane()).map(|l| l.points.iter().map(|p| (p.at, p.value)).collect()).unwrap_or_default()
    }

    fn bar(e: &Engine, n: i64) -> Samples {
        bar_sample(e.session(), n)
    }

    fn eqs(e: &Engine, t: TrackId) -> usize {
        e.session().track(t).unwrap().mixer.inserts.iter().flatten().filter(|i| i.plugin == EQ_PLUGIN).count()
    }

    #[test]
    fn a_blend_builds_over_sixteen_bars_and_goes_out_over_eight() {
        let (mut e, (ta, a), (tb, b)) = decks();
        let r = apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        assert_eq!((r["transition"]["id"].as_u64(), r["transition"]["entry_bar"].as_i64()), (Some(1), Some(25)), "{r}");
        assert_eq!((r["hit_bar"].as_i64(), r["gone_bar"].as_i64()), (Some(41), Some(49)), "{r}");
        assert_eq!(points(&e, tb), vec![(bar(&e, 25), -24.0), (bar(&e, 41), 0.0)]);
        assert_eq!(points(&e, ta), vec![(bar(&e, 41), 0.0), (bar(&e, 49), -24.0)]);
        let s = e.session();
        let (ca, cb) = (s.find_clip(a).unwrap().1, s.find_clip(b).unwrap().1);
        assert_eq!(cb.fade_in, Fade { len: bar(&e, 41) - bar(&e, 25), shape: FadeShape::EqualPower, curvature: 0.0 });
        assert_eq!(ca.fade_out, Fade { len: bar(&e, 45) - bar(&e, 41), shape: FadeShape::EqualPower, curvature: 0.0 });
        assert_eq!(r["recipe"]["in_fade"].as_i64(), Some(cb.fade_in.len));
        for t in [ta, tb] {
            let ins = s.track(t).unwrap().mixer.inserts.first().unwrap().as_ref().unwrap();
            assert_eq!((ins.plugin.as_str(), ins.params.get("low_shelf_freq")), (EQ_PLUGIN, Some(&200.0)));
        }
        // Again, by id and by clip pair: one transition, one EQ per deck, the same points.
        let after = e.session().clone();
        let r = apply(&mut e, json!({"transition": 1})).unwrap();
        assert_eq!(r["eq_inserted"], json!([]));
        apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        assert!(*e.session() == after, "re-applying changed the session");
        assert_eq!((eqs(&e, ta), eqs(&e, tb)), (1, 1));
        let list = e.execute("dj.transitions", &json!({})).unwrap();
        assert_eq!(list.as_array().map(Vec::len), Some(1), "{list}");
        assert_eq!(list[0]["hit_bar"], 41);
    }

    #[test]
    fn the_bass_is_half_way_in_and_half_way_out_mid_transition() {
        let (mut e, (ta, a), (tb, b)) = decks();
        apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        let s = e.session();
        let value = |t: TrackId, at: Samples| s.track(t).unwrap().lane(&bass_lane()).unwrap().value_at(at, 0.0);
        assert!((value(tb, bar(&e, 33)) + 12.0).abs() < 1e-3, "mid-build {}", value(tb, bar(&e, 33)));
        assert!((value(ta, bar(&e, 45)) + 12.0).abs() < 1e-3, "mid-out {}", value(ta, bar(&e, 45)));
        // Before the hit the outgoing bass is untouched; the incoming one is out before entry.
        assert_eq!(value(ta, bar(&e, 30)), 0.0);
        assert_eq!(value(tb, bar(&e, 24)), -24.0);
    }

    #[test]
    fn quick_uses_eight_and_four_and_a_style_change_brings_its_bars() {
        let (mut e, (ta, a), (_, b)) = decks();
        let r = apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "style": "quick"})).unwrap();
        let t = &r["transition"];
        assert_eq!((t["style"].as_str(), t["build_bars"].as_i64(), t["out_bars"].as_i64()), (Some("quick"), Some(8), Some(4)), "{r}");
        assert_eq!(points(&e, ta), vec![(bar(&e, 33), 0.0), (bar(&e, 37), -24.0)]);
        let r = apply(&mut e, json!({"transition": 1, "style": "blend"})).unwrap();
        assert_eq!((r["transition"]["build_bars"].as_i64(), r["transition"]["out_bars"].as_i64()), (Some(16), Some(8)));
        let r = apply(&mut e, json!({"transition": 1, "style": "quick", "build_bars": 12})).unwrap();
        assert_eq!((r["transition"]["build_bars"].as_i64(), r["transition"]["out_bars"].as_i64()), (Some(12), Some(4)));
        // Bars alone keep the style.
        let r = apply(&mut e, json!({"transition": 1, "out_bars": 2})).unwrap();
        let t = &r["transition"];
        assert_eq!((t["style"].as_str(), t["build_bars"].as_i64(), t["out_bars"].as_i64()), (Some("quick"), Some(12), Some(2)));
    }

    #[test]
    fn re_applying_replaces_the_old_points() {
        let (mut e, (ta, a), (tb, b)) = decks();
        apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "build_bars": 8})).unwrap();
        apply(&mut e, json!({"transition": 1, "entry_bar": 29})).unwrap();
        assert_eq!(points(&e, tb), vec![(bar(&e, 29), -24.0), (bar(&e, 37), 0.0)]);
        assert_eq!(points(&e, ta), vec![(bar(&e, 37), 0.0), (bar(&e, 45), -24.0)]);
        // Points outside the recipe's spans are left alone.
        let at = bar(&e, 60);
        e.session_mut().track_mut(tb).unwrap().lane_mut(&bass_lane()).set_point(at, 3.0);
        apply(&mut e, json!({"transition": 1, "entry_bar": 25})).unwrap();
        assert_eq!(points(&e, tb), vec![(bar(&e, 25), -24.0), (bar(&e, 33), 0.0), (at, 3.0)]);
    }

    #[test]
    fn an_edited_transition_is_rebuilt_only_when_forced() {
        let (mut e, (ta, a), (_, b)) = decks();
        apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        let at = bar(&e, 45);
        e.session_mut().track_mut(ta).unwrap().lane_mut(&bass_lane()).set_point(at, -6.0);
        e.session_mut().dj.transition_mut(1).unwrap().edited = true;
        let before = e.session().clone();
        let err = apply(&mut e, json!({"transition": 1})).unwrap_err();
        assert!(err.to_string().contains("transition 1 was edited by hand; pass force: true to rebuild it"), "{err}");
        assert!(apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "style": "quick"})).is_err());
        assert!(*e.session() == before);
        let r = apply(&mut e, json!({"transition": 1, "force": true})).unwrap();
        assert_eq!(r["transition"]["edited"], false);
        assert!(!e.session().dj.transition(1).unwrap().edited);
        assert_eq!(points(&e, ta), vec![(bar(&e, 41), 0.0), (bar(&e, 49), -24.0)]);
    }

    #[test]
    fn bad_transitions_are_refused_and_change_nothing() {
        let (mut e, (ta, a), (_, b)) = decks();
        let before = e.session().clone();
        let refused = |e: &mut Engine, p: Value, want: &str| {
            let err = apply(e, p.clone()).unwrap_err().to_string();
            assert!(err.contains(want), "{p}: {err}");
        };
        refused(&mut e, json!({"out_clip": a.0, "in_clip": 9_999}), "no clip 9999");
        refused(&mut e, json!({"out_clip": a.0, "in_clip": a.0}), "different clips");
        refused(&mut e, json!({"out_clip": a.0}), "give `transition`");
        refused(&mut e, json!({"transition": 7}), "no transition 7");
        // Entering on bar 1 hits on bar 17, before B starts on bar 25.
        refused(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "entry_bar": 1}), "starts on or after the bass hit");
        // Entering on bar 33 hits on bar 49, after A ends on bar 45.
        refused(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "entry_bar": 33}), "ends on or before the bass hit");
        refused(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "style": "slow"}), "`style`");
        refused(&mut e, json!({"out_clip": a.0, "in_clip": b.0, "build_bars": 0}), "`build_bars`");
        assert!(*e.session() == before);
        e.execute("mix.insert", &json!({"track": ta.0, "slot": "a", "plugin": "gain"})).unwrap();
        let before = e.session().clone();
        refused(&mut e, json!({"out_clip": a.0, "in_clip": b.0}), "holds `gain`");
        assert!(*e.session() == before);
        assert!(e.session().dj.is_empty());
    }

    fn flags(e: &Engine, id: u64) -> (bool, bool) {
        let t = e.session().dj.transition(id).unwrap();
        (t.edited, t.stale)
    }

    fn set_bass(e: &mut Engine, t: TrackId, at: Samples, value: f64) {
        e.execute("automation.set_point", &json!({"track": t.0, "param": "plugin:a:low_shelf_gain", "at": at, "value": value})).unwrap();
    }

    #[test]
    fn hand_edits_inside_a_transition_mark_it_edited() {
        let (mut e, (ta, a), (tb, b)) = decks();
        apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        // A nudge and automation outside the owned spans are not edits of the transition.
        e.execute("dj.nudge", &json!({"clip": b.0, "beats": 0.25})).unwrap();
        let (outside_b, outside_a) = (bar(&e, 60), bar(&e, 10));
        set_bass(&mut e, tb, outside_b, 3.0);
        set_bass(&mut e, ta, outside_a, 3.0);
        assert_eq!(flags(&e, 1), (false, false));
        // A point added inside the bass build is.
        let before = e.session().clone();
        let inside = bar(&e, 33);
        set_bass(&mut e, tb, inside, -6.0);
        assert_eq!(flags(&e, 1), (true, false));
        // Undoing it leaves the transition as the recipe wrote it.
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(*e.session() == before);
        assert_eq!(flags(&e, 1), (false, false));
        // So is a recipe point changed, or the incoming fade changed.
        let hit = bar(&e, 41);
        set_bass(&mut e, ta, hit, -3.0);
        assert_eq!(flags(&e, 1), (true, false));
        e.execute("edit.undo", &json!({})).unwrap();
        e.execute("edit.fades_create", &json!({"clip": b.0, "side": "in", "length": SR, "shape": "linear"})).unwrap();
        assert_eq!(flags(&e, 1), (true, false));
    }

    #[test]
    fn a_tempo_change_makes_a_transition_stale_and_re_applying_puts_it_back_on_its_bars() {
        let (mut e, (ta, a), (tb, b)) = decks();
        apply(&mut e, json!({"out_clip": a.0, "in_clip": b.0})).unwrap();
        let old_hit = bar(&e, 41);
        e.execute("event.tempo", &json!({"bpm": 140.0})).unwrap();
        assert_eq!(flags(&e, 1), (false, true));
        assert_ne!(bar(&e, 41), old_hit);
        apply(&mut e, json!({"transition": 1})).unwrap();
        assert_eq!(flags(&e, 1), (false, false));
        assert_eq!(points(&e, tb), vec![(bar(&e, 25), -24.0), (bar(&e, 41), 0.0)]);
        assert_eq!(points(&e, ta), vec![(bar(&e, 41), 0.0), (bar(&e, 49), -24.0)]);
        let c = e.session().find_clip(b).unwrap().1;
        assert_eq!(c.fade_in.len, bar(&e, 41) - c.start);
    }

    /// Clips (deck, from bar, to bar) on `decks` mono decks at 128 bpm.
    fn arranged(decks: usize, clips: &[(usize, i64, i64)]) -> (Engine, Vec<TrackId>, Vec<ClipId>) {
        let mut s = Session::default();
        s.tempo.set_tempo(0, 128.0).unwrap();
        let tracks: Vec<TrackId> = (0..decks).map(|_| s.add_track(TrackKind::Audio, ChannelFormat::Mono, None)).collect();
        let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![0.1; 10 * SR as usize]] };
        let src = crate::io::add_source(&mut s, "deck", buf, None, FileFormat::Wav);
        let mut ids = Vec::new();
        for &(deck, from, to) in clips {
            let id = s.new_clip_id();
            let start = bar_sample(&s, from);
            let clip = Clip::audio(id, "c", src, 0, start, bar_sample(&s, to) - start);
            ops::place_clip(&mut s, tracks[deck], clip);
            ids.push(id);
        }
        (Engine::new(s), tracks, ids)
    }

    #[test]
    fn transitions_on_shared_decks_leave_each_others_points_alone() {
        // A -> B -> C on alternating decks; on deck 0, A's bass is gone on bar 49, where C's builds.
        let (mut e, decks, c) = arranged(2, &[(0, 1, 45), (1, 25, 69), (0, 49, 90)]);
        apply(&mut e, json!({"out_clip": c[0].0, "in_clip": c[1].0})).unwrap();
        apply(&mut e, json!({"out_clip": c[1].0, "in_clip": c[2].0, "entry_bar": 49})).unwrap();
        let lanes = |e: &Engine| (points(e, decks[0]), points(e, decks[1]));
        let want = lanes(&e);
        assert_eq!(want.0, vec![(bar(&e, 41), 0.0), (bar(&e, 49), -24.0), (bar(&e, 65), 0.0)]);
        assert_eq!(want.1, vec![(bar(&e, 25), -24.0), (bar(&e, 41), 0.0), (bar(&e, 65), 0.0), (bar(&e, 73), -24.0)]);
        for order in [[1, 2], [2, 1], [1, 1], [2, 2]] {
            for id in order {
                apply(&mut e, json!({"transition": id})).unwrap();
            }
            assert_eq!(lanes(&e), want, "after re-applying {order:?}");
        }
        // Moving one keeps the other's points, and a command elsewhere marks neither edited.
        apply(&mut e, json!({"transition": 2, "entry_bar": 50})).unwrap();
        assert!(points(&e, decks[0]).contains(&(bar(&e, 49), -24.0)), "transition 1's gone point went");
        apply(&mut e, json!({"transition": 2, "entry_bar": 49})).unwrap();
        assert_eq!(lanes(&e), want);
        let far = bar(&e, 100);
        set_bass(&mut e, decks[1], far, 0.0);
        assert_eq!((flags(&e, 1), flags(&e, 2)), ((false, false), (false, false)));
        // Building into transition 1's span on deck 0 is a conflict.
        let before = e.session().clone();
        let err = apply(&mut e, json!({"transition": 2, "entry_bar": 45})).unwrap_err().to_string();
        assert!(err.contains("conflict") && err.contains("transition 1"), "{err}");
        assert!(*e.session() == before);
    }

    #[test]
    fn overlapping_fades_and_inactive_playlists_are_refused() {
        let (mut e, decks, c) = arranged(2, &[(0, 1, 45), (1, 25, 69)]);
        let long = bar(&e, 69) - bar(&e, 25) - bar(&e, 2);
        e.execute("edit.fades_create", &json!({"clip": c[1].0, "side": "out", "length": long})).unwrap();
        let err = apply(&mut e, json!({"out_clip": c[0].0, "in_clip": c[1].0})).unwrap_err().to_string();
        assert!(err.contains("conflict") && err.contains("fade-out"), "{err}");
        e.execute("edit.fades_create", &json!({"clip": c[1].0, "side": "out", "length": 1})).unwrap();
        apply(&mut e, json!({"out_clip": c[0].0, "in_clip": c[1].0})).unwrap();
        // The incoming clip only on an inactive playlist of its deck.
        let tr = e.session_mut().track_mut(decks[1]).unwrap();
        tr.playlists.push(soundcraft_model::Playlist::default());
        tr.active_playlist = 1;
        let err = apply(&mut e, json!({"transition": 1})).unwrap_err().to_string();
        assert!(err.contains("inactive playlist"), "{err}");
    }
}
