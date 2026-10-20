//! `prep.tracks`: level-matched, tagged copies of a set's or a folder's MP3s.

use super::*;
use soundcraft_audio_io::mp3gain::STEP_DB;

pub fn specs() -> Vec<CommandSpec> {
    vec![]
}

/// The gain step picked for a track, and what held it back from the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Used by `prep.tracks` (next).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Pick {
    pub steps: i32,
    /// `"peaks"` (the ceiling), `"global_gain"` (the frames' 0..=255 room) or `"unmeasurable"`.
    pub limited_by: Option<&'static str>,
}

/// Steps of [`STEP_DB`] that bring integrated loudness `lufs` nearest `target` without taking
/// true peak `tp` above `ceiling` (a track already above it is cut, even below the target), within
/// the frames' step `range` (most negative, most positive).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn pick_steps(lufs: f64, tp: f64, target: f64, ceiling: f64, range: (i32, i32)) -> Pick {
    if ![lufs, tp, target, ceiling].iter().all(|v| v.is_finite()) {
        return Pick { steps: 0, limited_by: Some("unmeasurable") };
    }
    // Finite inputs can still give steps far outside any frame's room; `as` saturates them.
    let want = ((target - lufs) / STEP_DB).round().clamp(-1e6, 1e6) as i32;
    let peak_max = ((ceiling - tp) / STEP_DB).floor().clamp(-1e6, 1e6) as i32;
    let (mut steps, mut limited_by) = if peak_max < want { (peak_max, Some("peaks")) } else { (want, None) };
    // Not `clamp`: that panics on a reversed range.
    let clamped = steps.max(range.0).min(range.1);
    if clamped != steps {
        (steps, limited_by) = (clamped, Some("global_gain"));
    }
    Pick { steps, limited_by }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boost_stops_at_the_ceiling() {
        assert_eq!(pick_steps(-17.0, -4.0, -14.0, -1.0, (-50, 50)), Pick { steps: 1, limited_by: Some("peaks") });
    }

    #[test]
    fn a_cut_reaches_the_nearest_step() {
        assert_eq!(pick_steps(-10.9, -2.7, -14.0, -1.0, (-50, 50)), Pick { steps: -2, limited_by: None });
    }

    #[test]
    fn a_hot_track_is_cut_below_the_target_to_clear_the_ceiling() {
        assert_eq!(pick_steps(-12.0, 2.0, -14.0, -1.0, (-50, 50)), Pick { steps: -2, limited_by: Some("peaks") });
    }

    #[test]
    fn the_frames_room_clamps_the_step() {
        assert_eq!(pick_steps(-10.9, -2.7, -14.0, -1.0, (-1, 5)), Pick { steps: -1, limited_by: Some("global_gain") });
        assert_eq!(pick_steps(-20.0, -15.0, -14.0, -1.0, (-5, 2)), Pick { steps: 2, limited_by: Some("global_gain") });
    }

    #[test]
    fn unmeasurable_input_gives_no_step() {
        let none = Pick { steps: 0, limited_by: Some("unmeasurable") };
        assert_eq!(pick_steps(f64::NAN, -2.7, -14.0, -1.0, (-50, 50)), none);
        assert_eq!(pick_steps(-10.9, f64::INFINITY, -14.0, -1.0, (-50, 50)), none);
        assert_eq!(pick_steps(-10.9, -2.7, f64::NAN, -1.0, (-50, 50)), none);
    }
}
