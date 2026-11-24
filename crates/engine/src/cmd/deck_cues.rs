//! Hot cues A–D for a track loaded on a live deck outside a DJ set: its mix points from
//! mixforge's analysis.
//!
//! Ported from mixforge's planner (pass 3, `planner.py`: mix points), which is Marty's own code,
//! for a track in the middle of a set with the standard transition (blend: 16 bars in and out,
//! the bass building over 16 bars and going out over 8). Mix in is the first 8-bar phrase in the
//! first third whose transition bars are on the grid, moved to a section boundary or to the bar
//! of biggest change near it; Mix out is the last section boundary that leaves room for the whole
//! transition, avoiding breakdowns, else the last trusted 16- then 8-bar phrase. Bass hit and
//! Bass gone follow from them as djforge's transitions place them.

use serde_json::Value;

/// Bars of the standard transition, in and out.
const TRANSITION_BARS: i64 = 16;
/// Bars the incoming bass builds over (blend), and the outgoing goes out over.
const BUILD_BARS: i64 = 16;
const OUT_BARS: i64 = 8;
const PHRASE_BARS: i64 = 16;

/// The analysis fields the planner reads.
struct Track {
    first: f64,
    bar: f64,
    total: i64,
    blocks: Vec<bool>,
    change: Vec<f64>,
    /// Section boundaries (bar, label, bars when given).
    sections: Vec<(i64, String, Option<i64>)>,
}

impl Track {
    fn of(a: &Value) -> Option<Track> {
        let bpm = a.get("bpm")?.as_f64().filter(|b| b.is_finite() && (20.0..=400.0).contains(b))?;
        let first = a.get("first_downbeat")?.as_f64().filter(|f| f.is_finite() && *f >= 0.0)?;
        let end = a.get("audio_end").or_else(|| a.get("duration")).and_then(Value::as_f64).filter(|e| e.is_finite() && *e > first)?;
        let bar = 4.0 * 60.0 / bpm;
        let total = ((end - first) / bar).floor();
        if !(1.0..1e6).contains(&total) {
            return None;
        }
        let list = |k: &str| a.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        let blocks = list("grid_ok_4bar").iter().map(|v| v.as_bool().unwrap_or(false)).collect();
        let change = list("bar_change").iter().map(|v| v.as_f64().filter(|x| x.is_finite()).unwrap_or(0.0)).collect();
        let sections = list("sections")
            .iter()
            .filter_map(|s| {
                let b = s.get("bar")?.as_i64()?;
                let label = s.get("label").and_then(Value::as_str).unwrap_or("").to_string();
                Some((b, label, s.get("bars").and_then(Value::as_i64)))
            })
            .collect();
        Some(Track { first, bar, total: total as i64, blocks, change, sections })
    }

    /// All 4-bar blocks a span touches follow the track's grid (no blocks known: trusted).
    fn trusted(&self, first_bar: i64, length: i64) -> bool {
        if self.blocks.is_empty() || length == 0 {
            return true;
        }
        let from = first_bar.div_euclid(4).max(0);
        let to = (self.blocks.len() as i64).min(-(-(first_bar + length)).div_euclid(4));
        (from..to).all(|b| usize::try_from(b).ok().and_then(|b| self.blocks.get(b)).copied().unwrap_or(false))
    }

    fn change_at(&self, b: i64) -> f64 {
        usize::try_from(b).ok().and_then(|b| self.change.get(b)).copied().unwrap_or(0.0)
    }

    /// The nearby bar (±4) with the biggest section change: the real phrase start.
    fn snap(&self, bar: i64, lo: i64, hi: i64, length: i64) -> i64 {
        if self.change.is_empty() {
            return bar;
        }
        let n = self.change.len() as i64;
        let window: Vec<i64> = ((lo.max(bar - 4))..=(hi.min(bar + 4))).filter(|b| *b < n && *b >= 0 && self.trusted(*b, length)).collect();
        // The first of equal maxima, as Python's `max` picks.
        let Some(best) = window.iter().copied().fold(None, |m: Option<i64>, b| match m {
            Some(x) if self.change_at(x) >= self.change_at(b) => Some(x),
            _ => Some(b),
        }) else {
            return bar;
        };
        if self.change_at(best) > 1.3 * self.change_at(bar.min(n - 1)) { best } else { bar }
    }

    fn bounds(&self) -> Vec<i64> {
        self.sections.iter().map(|s| s.0).collect()
    }

    /// (start, end) bars of the breakdowns.
    fn breaks(&self) -> Vec<(i64, i64)> {
        let s = &self.sections;
        s.iter().enumerate().filter(|(_, x)| x.1 == "break").map(|(k, x)| (x.0, s.get(k + 1).map_or(x.0 + x.2.unwrap_or(16), |n| n.0))).collect()
    }
}

/// The hot cues A (Mix in), B (Bass hit), C (Mix out) and D (Bass gone), in file seconds on
/// mixforge's timeline, from mixforge analysis `a`; `None` without a usable bpm, first downbeat
/// and end.
pub(crate) fn planned(a: &Value) -> Option<[Option<f64>; 4]> {
    let t = Track::of(a)?;
    let (n_in, n) = (TRANSITION_BARS, TRANSITION_BARS);
    let third = t.total / 3;
    let bounds = t.bounds();
    let mut in_bar = (0..third.max(1)).step_by(8).find(|p| t.trusted(*p, n_in)).unwrap_or(0);
    if in_bar > 0 {
        let on_section = bounds.iter().copied().find(|b| in_bar <= *b && *b <= third && t.trusted(*b, n_in));
        in_bar = on_section.unwrap_or_else(|| t.snap(in_bar, 1, third, n_in));
    }
    let earliest = in_bar + n_in + 8;
    let last = t.total - n;
    let brk = t.breaks();
    let clear = |b: i64| !brk.iter().any(|(s, e)| *s < b + n && *e > b);
    let mut cands: Vec<i64> = bounds.iter().copied().filter(|b| earliest <= *b && *b <= last && t.trusted(*b, n) && clear(*b)).collect();
    if cands.is_empty() {
        // Every boundary overlaps a breakdown: that rather than an off-grid mix.
        cands = bounds.iter().copied().filter(|b| earliest <= *b && *b <= last && t.trusted(*b, n)).collect();
    }
    let mut out_bar = cands.iter().copied().max();
    if out_bar.is_none() {
        for step in [PHRASE_BARS, 8] {
            let top = last.div_euclid(step) * step;
            out_bar = (0..).map(|k| top - k * step).take_while(|p| *p >= earliest).find(|p| t.trusted(*p, n));
            if out_bar.is_some() {
                break;
            }
        }
    }
    if let Some(o) = out_bar.filter(|o| !bounds.contains(o)) {
        out_bar = Some(t.snap(o, earliest, last, n));
    }
    let out_bar = out_bar.unwrap_or_else(|| earliest.max(last));
    let at = |bar: i64| t.first + bar as f64 * t.bar;
    Some([Some(at(in_bar)), Some(at(in_bar + BUILD_BARS)), Some(at(out_bar)), Some(at(out_bar + BUILD_BARS + OUT_BARS))])
}

/// The track's sections from mixforge analysis `a`: (start, end, label) in file seconds on
/// mixforge's timeline, each ending where the next starts (the last at its own length, else the
/// audio's end), on the same bars [`planned`] places the cues on. Empty without sections or a
/// usable grid.
pub(crate) fn sections(a: &Value) -> Vec<(f64, f64, String)> {
    let Some(t) = Track::of(a) else { return Vec::new() };
    let at = |bar: i64| t.first + bar as f64 * t.bar;
    let mut s: Vec<&(i64, String, Option<i64>)> = t.sections.iter().filter(|s| s.0 >= 0 && s.0 < t.total).collect();
    s.sort_by_key(|s| s.0);
    s.iter()
        .enumerate()
        .map(|(i, (bar, label, bars))| {
            let next = s.get(i + 1).map_or(t.total, |n| n.0);
            let end = bars.map_or(next, |b| (bar + b.max(1)).min(next));
            (at(*bar), at(end), label.clone())
        })
        .filter(|(a, b, _)| b > a)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An analysis as mixforge's library holds it (`analysis.py`'s fields, `sections` from
    /// `structure.py`): 128 bpm, 120 bars.
    fn analysis() -> Value {
        json!({
            "bpm": 128.0, "first_downbeat": 0.5, "duration": 230.0, "audio_start": 0.3, "audio_end": 225.5,
            "bars": 120, "downbeat_confidence": 0.9, "constant_tempo": true, "lufs": -8.1, "key": "A minor", "camelot": "8A",
            "bar_energy_db": vec![-12.0; 120],
            "grid_ok_4bar": vec![true; 30],
            "bar_change": (0..120).map(|b| if b % 16 == 0 { 3.0 } else { 1.0 }).collect::<Vec<f64>>(),
            "sections": [
                {"bar": 0, "label": "intro"}, {"bar": 16, "label": "verse"}, {"bar": 48, "label": "break"},
                {"bar": 64, "label": "chorus"}, {"bar": 96, "label": "outro"}
            ]
        })
    }

    #[test]
    fn a_clean_track_mixes_in_on_its_first_downbeat_and_out_on_its_last_clear_boundary() {
        let c = planned(&analysis()).unwrap();
        let bar = 1.875;
        assert_eq!(c[0], Some(0.5), "Mix in: the first downbeat");
        assert_eq!(c[1], Some(0.5 + 16.0 * bar), "Bass hit 16 bars on");
        // Boundaries 48 (a breakdown, to 64), 64 and 96: the last clear one that leaves room for
        // the transition (it can start up to bar 104).
        assert_eq!(c[2], Some(0.5 + 96.0 * bar));
        assert_eq!(c[3], Some(0.5 + 120.0 * bar), "Bass gone 24 bars after Mix out");
    }

    #[test]
    fn sections_lie_on_the_bars_the_cues_do_and_end_where_the_next_starts() {
        let bar = 1.875;
        let s = sections(&analysis());
        assert_eq!(s.len(), 5);
        assert_eq!(s[0], (0.5, 0.5 + 16.0 * bar, "intro".to_string()));
        assert_eq!(s[2], (0.5 + 48.0 * bar, 0.5 + 64.0 * bar, "break".to_string()));
        assert!(s[4].2 == "outro" && s[4].1 > s[4].0);
        // A section's own length ends it early, never past the next.
        let mut a = analysis();
        a["sections"] = json!([{"bar": 80, "label": "break", "bars": 16}, {"bar": 82, "label": "drop"}, {"bar": 8, "label": "verse", "bars": 4}]);
        let s = sections(&a);
        assert_eq!(s[0], (0.5 + 8.0 * bar, 0.5 + 12.0 * bar, "verse".to_string()));
        assert_eq!(s[1].1, 0.5 + 82.0 * bar);
        assert!(sections(&json!({"sections": [{"bar": 0, "label": "intro"}]})).is_empty(), "no grid, no sections");
    }

    #[test]
    fn an_off_grid_intro_and_a_breakdown_move_the_mix_points() {
        let mut a = analysis();
        // The first 8 bars are off the grid: mix in on the first clean phrase, at a boundary.
        a["grid_ok_4bar"][0] = json!(false);
        a["grid_ok_4bar"][1] = json!(false);
        a["sections"] = json!([{"bar": 0, "label": "intro"}, {"bar": 8, "label": "verse"}, {"bar": 80, "label": "break", "bars": 16}, {"bar": 82, "label": "drop"}]);
        let c = planned(&a).unwrap();
        assert_eq!(c[0], Some(0.5 + 8.0 * 1.875));
        // 82 is clear of nothing (the break runs 80-82, which a mix at 82 doesn't overlap).
        assert_eq!(c[2], Some(0.5 + 82.0 * 1.875));
        // Without sections: the last trusted 16-bar phrase.
        let mut a = analysis();
        a["sections"] = json!([]);
        a["bar_change"] = json!([]);
        assert_eq!(planned(&a).unwrap()[2], Some(0.5 + 96.0 * 1.875));
        assert!(planned(&json!({"bpm": 128})).is_none());
    }
}
