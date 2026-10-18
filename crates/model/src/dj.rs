//! DJ arrangement data: the transitions between beat-locked tracks of an imported set.

use crate::{ClipId, Track, TrackId};

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
    #[serde(default)]
    pub transitions: Vec<Transition>,
}

/// How a transition mixes: a long blend or a quick cut-over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionStyle {
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Transition {
    pub id: u64,
    pub out_clip: ClipId,
    pub in_clip: ClipId,
    pub out_track: TrackId,
    pub in_track: TrackId,
    /// Bar the incoming track enters on (its fade-in and bass build start here).
    pub entry_bar: i64,
    pub style: TransitionStyle,
    /// Bars the incoming bass builds over; it hits in full at `entry_bar + build_bars`.
    pub build_bars: i64,
    /// Bars the outgoing bass goes out over after the hit.
    pub out_bars: i64,
    #[serde(default)]
    pub nudge_beats: f64,
    #[serde(default)]
    pub nudge_source: NudgeSource,
    /// Last nudge suggestion, kept for the report and the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<NudgeSuggestion>,
    /// Set when the transition's fades or automation were changed by hand: recipes don't
    /// overwrite it unless forced.
    #[serde(default)]
    pub edited: bool,
}

/// What `dj.suggest_nudge` proposed for a transition.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NudgeSuggestion {
    pub nudge_beats: f64,
    pub nudge_ms: f64,
    pub reliable: bool,
    pub ambiguity: f64,
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

    /// Repair invariants after loading untrusted data: drop transitions whose clips are gone (or
    /// aren't audio clips on the tracks named) and repeated ids (keeping the first), and bound
    /// the bar counts and nudges.
    pub fn sanitize(&mut self, tracks: &[Track]) {
        let holds = |track: TrackId, clip: ClipId| {
            tracks.iter().filter(|t| t.id == track).flat_map(|t| t.playlists.iter().flat_map(|p| &p.clips)).any(|c| c.id == clip && c.is_audio())
        };
        let mut seen = std::collections::BTreeSet::new();
        self.transitions.retain(|t| holds(t.out_track, t.out_clip) && holds(t.in_track, t.in_clip) && seen.insert(t.id));
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

    /// Bound the bars and nudge; a suggestion with a non-finite number is dropped.
    pub fn sanitize(&mut self) {
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
        quick.style = TransitionStyle::Quick;
        quick.suggestion = None;
        quick.nudge_source = NudgeSource::None;
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
        hostile.entry_bar = i64::MIN;
        hostile.build_bars = 0;
        hostile.out_bars = i64::MAX;
        hostile.nudge_beats = f64::NAN;
        hostile.suggestion = Some(NudgeSuggestion { nudge_beats: 1.0, nudge_ms: f64::INFINITY, reliable: false, ambiguity: 0.0 });
        let mut far = t.clone();
        far.id = 3;
        far.entry_bar = i64::MAX;
        far.nudge_beats = -100.0;
        let mut wrong_track = t.clone();
        wrong_track.id = 4;
        wrong_track.in_track = t.out_track;
        let mut gone = t.clone();
        gone.id = 5;
        gone.in_clip = ClipId(9_999);
        s.dj.transitions = vec![t.clone(), hostile, far, wrong_track, gone];
        // Non-finite floats can't be written to JSON, so repair in memory.
        s.sanitize();
        let back = s;
        let ids: Vec<u64> = back.dj.transitions.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1, 2, 3]);
        let h = &back.dj.transitions[1];
        assert_eq!((h.entry_bar, h.build_bars, h.out_bars, h.nudge_beats, h.suggestion), (1, 1, 256, 0.0, None));
        let f = &back.dj.transitions[2];
        assert_eq!((f.entry_bar, f.nudge_beats), (MAX_TRANSITION_BAR, -8.0));
        assert_eq!(back.dj.transitions[0], t);

        // Deleting a clip orphans its transition.
        let mut s = back;
        for tr in &mut s.tracks {
            for p in &mut tr.playlists {
                p.clips.retain(|c| c.id != t.in_clip);
            }
        }
        s.sanitize();
        assert!(s.dj.is_empty());
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
}
