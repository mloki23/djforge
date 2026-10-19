//! DJ arrangement data: the transitions between beat-locked tracks of an imported set.

use crate::{ClipId, Track, TrackId};
use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer};
use soundcraft_time::Samples;

/// Fewest bars a transition's bass build or bass out may span.
pub const MIN_TRANSITION_BARS: i64 = 1;
/// Most bars a transition's bass build or bass out may span.
pub const MAX_TRANSITION_BARS: i64 = 256;
/// Latest bar a transition may enter on (far past any real set).
pub const MAX_TRANSITION_BAR: i64 = 100_000;
/// Largest nudge, in beats, either way.
pub const MAX_NUDGE_BEATS: f64 = 8.0;

/// DJ arrangement data: the transitions between beat-locked tracks.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DjState {
    /// A malformed record is dropped rather than failing the session load.
    #[serde(default, deserialize_with = "loadable_transitions")]
    pub transitions: Vec<Transition>,
}

/// How a transition mixes: a long blend or a quick cut-over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionStyle {
    #[default]
    Blend,
    Quick,
}

/// Where a transition's nudge came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NudgeSource {
    #[default]
    None,
    Plan,
    Suggested,
    Manual,
}

/// One mix from `out_clip` into `in_clip`. Positions are session bars (1-based, like the ruler).
///
/// Reading is lenient: a missing, null or unknown value takes its default (bars of 0 become the
/// style's in [`Transition::sanitize`]); only the id and the two clips are required.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Transition {
    pub id: u64,
    pub out_clip: ClipId,
    pub in_clip: ClipId,
    /// Re-bound to the track holding `out_clip` by [`DjState::sanitize`].
    #[serde(default, deserialize_with = "lenient")]
    pub out_track: TrackId,
    #[serde(default, deserialize_with = "lenient")]
    pub in_track: TrackId,
    /// Bar the incoming track enters on (its fade-in and bass build start here).
    #[serde(default, deserialize_with = "lenient")]
    pub entry_bar: i64,
    #[serde(default, deserialize_with = "lenient")]
    pub style: TransitionStyle,
    /// Bars the incoming bass builds over; it hits in full at `entry_bar + build_bars`.
    #[serde(default, deserialize_with = "lenient")]
    pub build_bars: i64,
    /// Bars the outgoing bass goes out over after the hit.
    #[serde(default, deserialize_with = "lenient")]
    pub out_bars: i64,
    #[serde(default, deserialize_with = "lenient")]
    pub nudge_beats: f64,
    #[serde(default, deserialize_with = "lenient")]
    pub nudge_source: NudgeSource,
    /// Last nudge suggestion, kept for the report and the UI.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<NudgeSuggestion>,
    /// Set when the transition's fades or automation were changed by hand: recipes don't
    /// overwrite it unless forced.
    #[serde(default, deserialize_with = "lenient")]
    pub edited: bool,
    /// Set when the tempo map (or sample rate) changed since the recipe was applied: its points
    /// and fades are no longer on its bars until it is applied again.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// What the recipe last wrote, to tell hand edits from it and to remove exactly that on
    /// re-apply.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    pub applied: Option<Applied>,
}

/// What `dj.suggest_nudge` proposed for a transition.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NudgeSuggestion {
    pub nudge_beats: f64,
    pub nudge_ms: f64,
    pub reliable: bool,
    pub ambiguity: f64,
}

/// What a transition's recipe wrote into the session, in timeline samples.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Applied {
    /// `tempo_fingerprint` of the tempo map and rate the positions were worked out under.
    pub tempo: u64,
    /// The bass breakpoints written.
    pub points: Vec<AppliedPoint>,
    /// The stretch of each deck's bass lane the transition owns.
    pub spans: Vec<OwnedSpan>,
    /// The fades written.
    pub fades: Vec<AppliedFade>,
}

/// A bass breakpoint a recipe wrote.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppliedPoint {
    pub track: TrackId,
    pub at: Samples,
    pub value: f32,
}

/// A stretch `[start, end]` of a deck's bass lane a transition owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OwnedSpan {
    pub track: TrackId,
    pub start: Samples,
    pub end: Samples,
}

impl OwnedSpan {
    /// Whether the spans share more than an end point on the same track.
    pub fn overlaps(&self, o: &OwnedSpan) -> bool {
        self.track == o.track && self.start < o.end && o.start < self.end
    }
}

/// A clip fade a recipe wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppliedFade {
    pub clip: ClipId,
    pub track: TrackId,
    /// The fade-in (else the fade-out).
    pub fade_in: bool,
    pub len: Samples,
}

/// A value of type `T`, or its default when missing, null or malformed.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

/// The transitions that read; malformed records are dropped.
fn loadable_transitions<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Transition>, D::Error> {
    let list = match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(a) => a,
        _ => Vec::new(),
    };
    Ok(list.into_iter().filter_map(|t| serde_json::from_value(t).ok()).collect())
}

impl TransitionStyle {
    /// Default (build, out) bars: a blend builds over 16 and goes out over 8, a quick mix 8 and 4.
    pub fn default_bars(self) -> (i64, i64) {
        match self {
            TransitionStyle::Blend => (16, 8),
            TransitionStyle::Quick => (8, 4),
        }
    }
}

impl DjState {
    pub fn is_empty(&self) -> bool {
        self.transitions.is_empty()
    }

    /// An id no transition uses yet (one past the largest).
    pub fn next_id(&self) -> u64 {
        self.transitions.iter().map(|t| t.id).max().unwrap_or(0).saturating_add(1)
    }

    pub fn transition(&self, id: u64) -> Option<&Transition> {
        self.transitions.iter().find(|t| t.id == id)
    }

    pub fn transition_mut(&mut self, id: u64) -> Option<&mut Transition> {
        self.transitions.iter_mut().find(|t| t.id == id)
    }

    /// Repair invariants after loading untrusted data: re-bind each transition to the tracks
    /// holding its audio clips now (on any playlist), drop those whose clips are gone everywhere,
    /// that mix a clip into itself, or that repeat an id or a clip pair (keeping the first), and
    /// bound the bar counts and nudges.
    pub fn sanitize(&mut self, tracks: &[Track]) {
        let holder =
            |clip: ClipId| tracks.iter().find(|t| t.playlists.iter().flat_map(|p| &p.clips).any(|c| c.id == clip && c.is_audio())).map(|t| t.id);
        let (mut ids, mut pairs) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
        self.transitions.retain_mut(|t| {
            let (Some(o), Some(i)) = (holder(t.out_clip), holder(t.in_clip)) else { return false };
            (t.out_track, t.in_track) = (o, i);
            t.out_clip != t.in_clip && ids.insert(t.id) && pairs.insert((t.out_clip, t.in_clip))
        });
        self.transitions.iter_mut().for_each(Transition::sanitize);
    }
}

impl Transition {
    /// Bar the incoming bass hits in full.
    pub fn hit_bar(&self) -> i64 {
        self.entry_bar.saturating_add(self.build_bars)
    }

    /// Bar the outgoing bass is gone by.
    pub fn gone_bar(&self) -> i64 {
        self.hit_bar().saturating_add(self.out_bars)
    }

    /// Bound the bars and nudge (bars of 0 or less, as a record without them reads, take the
    /// style's); a suggestion with a non-finite number is dropped.
    pub fn sanitize(&mut self) {
        let (build, out) = self.style.default_bars();
        if self.build_bars <= 0 {
            self.build_bars = build;
        }
        if self.out_bars <= 0 {
            self.out_bars = out;
        }
        self.entry_bar = self.entry_bar.clamp(1, MAX_TRANSITION_BAR);
        self.build_bars = self.build_bars.clamp(MIN_TRANSITION_BARS, MAX_TRANSITION_BARS);
        self.out_bars = self.out_bars.clamp(MIN_TRANSITION_BARS, MAX_TRANSITION_BARS);
        self.nudge_beats = bound_nudge(self.nudge_beats);
        if let Some(s) = &mut self.suggestion {
            if [s.nudge_beats, s.nudge_ms, s.ambiguity].iter().all(|v| v.is_finite()) {
                s.nudge_beats = bound_nudge(s.nudge_beats);
            } else {
                self.suggestion = None;
            }
        }
    }
}

fn bound_nudge(beats: f64) -> f64 {
    if beats.is_finite() { beats.clamp(-MAX_NUDGE_BEATS, MAX_NUDGE_BEATS) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelFormat, Clip, Session, SourceId, TrackKind};

    /// A session with two audio tracks holding one clip each, and a transition between them.
    fn mixed() -> (Session, Transition) {
        let mut s = Session::default();
        let a = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let b = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, None);
        let (ca, cb) = (s.new_clip_id(), s.new_clip_id());
        if let Some(p) = s.track_mut(a).and_then(Track::playlist_mut) {
            p.clips.push(Clip::audio(ca, "out", SourceId(90), 0, 0, 1_000_000));
        }
        if let Some(p) = s.track_mut(b).and_then(Track::playlist_mut) {
            p.clips.push(Clip::audio(cb, "in", SourceId(91), 0, 500_000, 1_000_000));
        }
        let (build_bars, out_bars) = TransitionStyle::Blend.default_bars();
        let t = Transition {
            id: 1,
            out_clip: ca,
            in_clip: cb,
            out_track: a,
            in_track: b,
            entry_bar: 33,
            style: TransitionStyle::Blend,
            build_bars,
            out_bars,
            nudge_beats: 0.25,
            nudge_source: NudgeSource::Suggested,
            suggestion: Some(NudgeSuggestion { nudge_beats: 0.25, nudge_ms: 117.2, reliable: true, ambiguity: 0.1 }),
            edited: true,
            stale: false,
            applied: Some(Applied {
                tempo: 7,
                points: vec![AppliedPoint { track: b, at: 10, value: -24.0 }],
                spans: vec![OwnedSpan { track: b, start: 10, end: 20 }],
                fades: vec![AppliedFade { clip: cb, track: b, fade_in: true, len: 10 }],
            }),
        };
        (s, t)
    }

    #[test]
    fn a_session_without_dj_data_loads_empty_and_writes_no_key() {
        let mut v = serde_json::to_value(Session::default()).unwrap();
        assert!(v.get("dj").is_none());
        v.as_object_mut().unwrap().remove("dj");
        assert!(Session::from_json(&v.to_string()).unwrap().dj.is_empty());
    }

    #[test]
    fn transitions_round_trip() {
        let (mut s, t) = mixed();
        s.dj.transitions.push(t.clone());
        let mut quick = t;
        quick.id = 2;
        quick.in_clip = quick.out_clip;
        quick.out_clip = s.dj.transitions[0].in_clip;
        (quick.out_track, quick.in_track) = (quick.in_track, quick.out_track);
        quick.style = TransitionStyle::Quick;
        quick.suggestion = None;
        quick.nudge_source = NudgeSource::None;
        quick.stale = true;
        s.dj.transitions.push(quick);
        let text = s.to_json().unwrap();
        assert!(text.contains("\"style\": \"quick\"") && text.contains("\"nudge_source\": \"suggested\""));
        let back = Session::from_json(&text).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.dj.next_id(), 3);
        assert_eq!((back.dj.transitions[0].hit_bar(), back.dj.transitions[0].gone_bar()), (49, 57));
    }

    #[test]
    fn sanitize_drops_orphans_and_bounds_hostile_values() {
        let (mut s, t) = mixed();
        let mut hostile = t.clone();
        hostile.id = 2;
        hostile.out_clip = t.in_clip;
        hostile.in_clip = t.out_clip;
        hostile.entry_bar = i64::MIN;
        hostile.build_bars = 0;
        hostile.out_bars = i64::MAX;
        hostile.nudge_beats = f64::NAN;
        hostile.suggestion = Some(NudgeSuggestion { nudge_beats: 1.0, nudge_ms: f64::INFINITY, reliable: false, ambiguity: 0.0 });
        let mut wrong_track = t.clone();
        wrong_track.id = 4;
        wrong_track.in_track = t.out_track;
        let mut gone = t.clone();
        gone.id = 5;
        gone.in_clip = ClipId(9_999);
        let mut itself = t.clone();
        itself.id = 6;
        itself.in_clip = t.out_clip;
        s.dj.transitions = vec![t.clone(), hostile, wrong_track, gone, itself];
        // Non-finite floats can't be written to JSON, so repair in memory.
        s.sanitize();
        let ids: Vec<u64> = s.dj.transitions.iter().map(|t| t.id).collect();
        // 4 repeats transition 1's clip pair; 5 lost a clip; 6 mixes a clip into itself.
        assert_eq!(ids, [1, 2]);
        let h = &s.dj.transitions[1];
        assert_eq!((h.entry_bar, h.build_bars, h.out_bars, h.nudge_beats, h.suggestion), (1, 16, 256, 0.0, None));
        assert_eq!(s.dj.transitions[0], t);

        // Deleting a clip orphans its transitions.
        for tr in &mut s.tracks {
            for p in &mut tr.playlists {
                p.clips.retain(|c| c.id != t.in_clip);
            }
        }
        s.sanitize();
        assert!(s.dj.is_empty());
    }

    #[test]
    fn sanitize_rebinds_a_moved_clip_and_keeps_inactive_playlists() {
        let (mut s, mut t) = mixed();
        t.in_track = t.out_track;
        s.dj.transitions = vec![t.clone()];
        s.sanitize();
        let b = s.tracks[1].id;
        assert_eq!(s.dj.transitions[0].in_track, b, "re-bound to the track holding the clip");
        // On an inactive playlist of that track the clip still counts.
        let tr = s.track_mut(b).unwrap();
        tr.playlists.push(crate::Playlist::default());
        tr.active_playlist = 1;
        s.sanitize();
        assert_eq!(s.dj.transitions.len(), 1);
    }

    #[test]
    fn duplicate_ids_keep_the_first() {
        let (mut s, t) = mixed();
        let mut dup = t.clone();
        dup.entry_bar = 65;
        s.dj.transitions = vec![t.clone(), dup];
        s.sanitize();
        assert_eq!(s.dj.transitions, [t]);
    }

    #[test]
    fn lenient_records_load_and_malformed_ones_are_dropped() {
        let (mut s, t) = mixed();
        s.dj.transitions = vec![t.clone()];
        let mut v = serde_json::to_value(&s).unwrap();
        let rec = &mut v["dj"]["transitions"][0];
        rec["nudge_beats"] = serde_json::Value::Null;
        rec["style"] = serde_json::json!("drop");
        rec["nudge_source"] = serde_json::json!(42);
        rec["build_bars"] = serde_json::Value::Null;
        if let Some(o) = rec.as_object_mut() {
            o.remove("out_bars");
            o.remove("entry_bar");
            o.remove("in_track");
        }
        let bad = serde_json::json!({"id": "two", "out_clip": 1});
        if let Some(a) = v["dj"]["transitions"].as_array_mut() {
            a.push(bad);
        }
        let back = Session::from_json(&v.to_string()).unwrap();
        let got = back.dj.transitions.first().unwrap();
        assert_eq!(back.dj.transitions.len(), 1);
        assert_eq!((got.nudge_beats, got.style, got.nudge_source), (0.0, TransitionStyle::Blend, NudgeSource::None));
        assert_eq!((got.entry_bar, got.build_bars, got.out_bars, got.in_track), (1, 16, 8, t.in_track));
    }
}
