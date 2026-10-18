//! mixforge's set plan (`plan.json`): the tracks of a set in play order, with their beat grids,
//! cue points and the transition into the next one. [`parse_plan`] reads and validates it.

use serde_json::{Map, Value};
use soundcraft_model::{MAX_TRANSITION_BARS, MIN_TRANSITION_BARS, TransitionStyle};

/// Most tracks a plan may hold.
pub(crate) const MAX_PLAN_TRACKS: usize = 500;
/// The plan format family this reader understands.
const FORMAT_PREFIX: &str = "mixforge-plan/";
/// Tempo range (bpm) a track or the master tempo may have.
const BPM_RANGE: std::ops::RangeInclusive<f64> = 20.0..=400.0;

/// A set plan. Times are seconds of each track's audio file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub format: String,
    pub master_bpm: f64,
    /// How the session tempo moves between tracks (`ramp`, ...), as mixforge wrote it.
    pub tempo_mode: String,
    pub target_lufs: Option<f64>,
    /// mixforge's warnings, then the reader's own.
    pub warnings: Vec<String>,
    pub tracks: Vec<PlanTrack>,
}

/// One track of a plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanTrack {
    pub path: String,
    pub title: String,
    pub artist: String,
    /// The track's own tempo.
    pub bpm: f64,
    /// The tempo it plays at in the set.
    pub play_bpm: f64,
    pub first_downbeat: f64,
    /// Where the mix brings it in.
    pub cue_in: f64,
    /// Where its clip starts.
    pub start: f64,
    /// Where the mix into the next track starts (`None` for the last track).
    pub out_start: Option<f64>,
    pub audio_end: f64,
    pub gain_db: f64,
    pub nudge_ms: f64,
    pub camelot: String,
    pub key: String,
    /// The transition into the next track.
    pub transition: Option<PlanTransition>,
    /// Effects mixforge planned on the track (not imported).
    pub fx_count: usize,
}

/// How a track mixes into the next one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PlanTransition {
    pub style: TransitionStyle,
    pub bars: i64,
    /// Whether the tempo ramps to the next track's over the transition.
    pub ramp: bool,
}

/// Reads one object's fields, naming `place` (`plan`, `track 3`) in its errors.
struct Fields<'a> {
    obj: &'a Map<String, Value>,
    place: String,
}

impl Fields<'_> {
    fn err(&self, key: &str, what: &str) -> String {
        format!("{}: `{key}` {what}", self.place)
    }

    /// The value at `key`, `None` when missing or null.
    fn get(&self, key: &str) -> Option<&Value> {
        self.obj.get(key).filter(|v| !v.is_null())
    }

    fn num(&self, key: &str) -> Result<Option<f64>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(v) => v.as_f64().filter(|x| x.is_finite()).map(Some).ok_or_else(|| self.err(key, "must be a finite number")),
        }
    }

    fn need_num(&self, key: &str) -> Result<f64, String> {
        self.num(key)?.ok_or_else(|| self.err(key, "is missing"))
    }

    fn bpm(&self, key: &str, default: Option<f64>) -> Result<f64, String> {
        let v = match default {
            Some(d) => self.num(key)?.unwrap_or(d),
            None => self.need_num(key)?,
        };
        if BPM_RANGE.contains(&v) { Ok(v) } else { Err(self.err(key, "must be 20..=400")) }
    }

    fn time(&self, key: &str) -> Result<Option<f64>, String> {
        match self.num(key)? {
            Some(t) if t < 0.0 => Err(self.err(key, "must not be negative")),
            t => Ok(t),
        }
    }

    fn need_time(&self, key: &str) -> Result<f64, String> {
        self.time(key)?.ok_or_else(|| self.err(key, "is missing"))
    }

    fn text(&self, key: &str) -> Result<String, String> {
        match self.get(key) {
            None => Ok(String::new()),
            Some(v) => v.as_str().map(str::to_string).ok_or_else(|| self.err(key, "must be a string")),
        }
    }
}

/// The plan in `json`, or the first problem found, naming the track (1-based) and field.
pub(crate) fn parse_plan(json: &str) -> Result<Plan, String> {
    let root: Value = serde_json::from_str(json).map_err(|err| format!("plan.json is not valid JSON: {err}"))?;
    let obj = root.as_object().ok_or("plan.json must hold an object")?;
    let f = Fields { obj, place: "plan".to_string() };
    let format = f.text("format")?;
    if !format.starts_with(FORMAT_PREFIX) {
        return Err(format!("plan: `format` must be {FORMAT_PREFIX}…, not {format:?}"));
    }
    let master_bpm = f.bpm("master_bpm", None)?;
    let tempo_mode = f.text("tempo_mode")?;
    let target_lufs = f.num("target_lufs")?;
    let mut warnings = match f.get("warnings") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .and_then(|a| a.iter().map(|w| w.as_str().map(str::to_string)).collect::<Option<Vec<String>>>())
            .ok_or_else(|| f.err("warnings", "must be a list of strings"))?,
    };
    let list = f.get("tracks").and_then(Value::as_array).ok_or_else(|| f.err("tracks", "must be a list"))?;
    if list.is_empty() || list.len() > MAX_PLAN_TRACKS {
        return Err(format!("plan: `tracks` must hold 1..={MAX_PLAN_TRACKS} tracks, not {}", list.len()));
    }
    let mut tracks = Vec::with_capacity(list.len());
    for (i, v) in list.iter().enumerate() {
        let n = i + 1;
        let obj = v.as_object().ok_or_else(|| format!("track {n}: must be an object"))?;
        let last = n == list.len();
        tracks.push(parse_track(&Fields { obj, place: format!("track {n}") }, last, &mut warnings)?);
    }
    Ok(Plan { format, master_bpm, tempo_mode, target_lufs, warnings, tracks })
}

/// One track of the plan; `last` when no track follows it. An unknown transition style becomes a
/// blend, with a warning added to `warnings`.
fn parse_track(f: &Fields, last: bool, warnings: &mut Vec<String>) -> Result<PlanTrack, String> {
    let path = f.text("path")?;
    if path.trim().is_empty() {
        return Err(f.err("path", "is missing"));
    }
    let bpm = f.bpm("bpm", None)?;
    let play_bpm = f.bpm("play_bpm", Some(bpm))?;
    let first_downbeat = f.need_time("first_downbeat")?;
    let audio_end = f.need_time("audio_end")?;
    let out_start = f.time("out_start")?;
    match out_start {
        Some(o) if o > audio_end => return Err(f.err("out_start", "must not be after `audio_end`")),
        None if !last => return Err(f.err("out_start", "is missing, and a track follows")),
        _ => {}
    }
    let title = f.text("title")?;
    let transition = match f.get("transition") {
        None => None,
        Some(v) => {
            let obj = v.as_object().ok_or_else(|| f.err("transition", "must be an object"))?;
            let t = Fields { obj, place: f.place.clone() };
            let style = match t.text("style")?.as_str() {
                "blend" => TransitionStyle::Blend,
                "quick" => TransitionStyle::Quick,
                other => {
                    let name = if title.is_empty() { &path } else { &title };
                    warnings.push(format!("{}: unknown transition style {other:?} for {name}; blending instead.", f.place));
                    TransitionStyle::Blend
                }
            };
            let bars = match t.get("bars").or_else(|| f.get("transition_bars")) {
                None => style.default_bars().0,
                Some(b) => b
                    .as_i64()
                    .filter(|b| (MIN_TRANSITION_BARS..=MAX_TRANSITION_BARS).contains(b))
                    .ok_or_else(|| f.err("transition.bars", "must be a whole number 1..=256"))?,
            };
            let ramp = match t.get("ramp") {
                None => false,
                Some(r) => r.as_bool().ok_or_else(|| f.err("transition.ramp", "must be true or false"))?,
            };
            Some(PlanTransition { style, bars, ramp })
        }
    };
    let fx_count = match f.get("fx") {
        None => 0,
        Some(v) => v.as_array().map(Vec::len).ok_or_else(|| f.err("fx", "must be a list"))?,
    };
    Ok(PlanTrack {
        artist: f.text("artist")?,
        bpm,
        play_bpm,
        first_downbeat,
        cue_in: f.time("cue_in")?.unwrap_or(first_downbeat),
        start: f.time("start")?.unwrap_or(0.0),
        out_start,
        audio_end,
        gain_db: f.num("gain_db")?.unwrap_or(0.0),
        nudge_ms: f.num("nudge_ms")?.unwrap_or(0.0),
        camelot: f.text("camelot")?,
        key: f.text("key")?,
        transition,
        fx_count,
        path,
        title,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    const SMALL: &str = include_str!("../../tests/fixtures/mixforge-plan-small.json");

    /// The small plan with `edit` applied to its JSON.
    fn edited(edit: impl FnOnce(&mut Value)) -> String {
        let mut v: Value = serde_json::from_str(SMALL).unwrap();
        edit(&mut v);
        v.to_string()
    }

    fn refused(json: &str, want: &str) {
        let err = parse_plan(json).unwrap_err();
        assert!(err.contains(want), "wanted {want:?}, got {err:?}");
    }

    #[test]
    fn the_small_plan_parses() {
        let p = parse_plan(SMALL).unwrap();
        assert_eq!((p.format.as_str(), p.master_bpm, p.tempo_mode.as_str(), p.target_lufs), ("mixforge-plan/2", 140.0, "ramp", Some(-10.0)));
        assert_eq!(p.tracks.len(), 3);
        assert!(p.warnings.first().unwrap().starts_with("Key clash: Quest - Accelerator"));
        let t = &p.tracks[0];
        assert_eq!((t.bpm, t.play_bpm, t.first_downbeat, t.cue_in, t.start), (132.006, 132.006, 1.5957, 1.5957, 0.0));
        assert_eq!((t.out_start, t.audio_end, t.gain_db, t.nudge_ms), (Some(496.1187), 512.464, 6.0, 0.0));
        assert_eq!((t.camelot.as_str(), t.key.as_str(), t.artist.as_str(), t.fx_count), ("5B", "D#", "GERMSGEMS", 0));
        assert!(t.path.ends_with("193 - Quest - Accelerator (Sunnyside Dub) (Acid Trance 2000).mp3"));
        assert_eq!(t.transition, Some(PlanTransition { style: TransitionStyle::Quick, bars: 8, ramp: true }));
        // An integer nudge reads as a number.
        let t = &p.tracks[1];
        assert_eq!((t.nudge_ms, t.start, t.transition.map(|x| (x.style, x.bars))), (0.0, 1.3157, Some((TransitionStyle::Blend, 16))));
    }

    #[test]
    fn missing_optional_fields_get_defaults() {
        let json = edited(|v| {
            let t = v["tracks"][2].as_object_mut().unwrap();
            for k in ["gain_db", "nudge_ms", "cue_in", "out_start", "transition", "play_bpm", "fx", "start"] {
                t.remove(k);
            }
            v["tracks"][0]["transition"]["style"] = json!("drop");
        });
        let p = parse_plan(&json).unwrap();
        let t = &p.tracks[2];
        assert_eq!((t.gain_db, t.nudge_ms, t.cue_in, t.out_start, t.transition, t.play_bpm, t.fx_count), (0.0, 0.0, 0.9357, None, None, 134.661, 0));
        // An unknown style blends, with a warning naming the track.
        assert_eq!(p.tracks[0].transition.map(|t| t.style), Some(TransitionStyle::Blend));
        let w = p.warnings.last().unwrap();
        assert!(w.contains("track 1") && w.contains("\"drop\"") && w.contains("Quest - Accelerator"), "{w}");
    }

    #[test]
    fn hostile_plans_are_refused_with_the_track_and_field() {
        refused(&edited(|v| v["format"] = json!("rekordbox/1")), "`format` must be mixforge-plan/");
        refused(&edited(|v| v["tracks"][2]["bpm"] = json!("NaN")), "track 3: `bpm` must be a finite number");
        refused(&edited(|v| v["tracks"][2]["bpm"] = json!(500.0)), "track 3: `bpm` must be 20..=400");
        refused(&edited(|v| v["tracks"][1]["play_bpm"] = json!(10)), "track 2: `play_bpm` must be 20..=400");
        refused(&SMALL.replacen("\"bpm\": 132.006", "\"bpm\": 1e999", 1), "not valid JSON");
        refused(&edited(|v| v["tracks"][0]["cue_in"] = json!(-0.5)), "track 1: `cue_in` must not be negative");
        refused(&edited(|v| v["tracks"][1]["out_start"] = json!(9_999.0)), "track 2: `out_start` must not be after `audio_end`");
        refused(&edited(|v| v["tracks"][0]["out_start"] = Value::Null), "track 1: `out_start` is missing");
        refused(&edited(|v| v["tracks"][1]["transition"]["bars"] = json!(0)), "track 2: `transition.bars` must be a whole number 1..=256");
        refused(&edited(|v| v["tracks"][1]["transition"]["bars"] = json!(300)), "track 2: `transition.bars`");
        refused(
            &edited(|v| {
                v["tracks"][0].as_object_mut().unwrap().remove("first_downbeat");
            }),
            "track 1: `first_downbeat` is missing",
        );
        refused(&edited(|v| v["tracks"] = json!([])), "1..=500 tracks, not 0");
        refused(&edited(|v| v["tracks"] = json!(vec![v["tracks"][0].clone(); 501])), "1..=500 tracks, not 501");
        refused(&edited(|v| v["master_bpm"] = json!(null)), "plan: `master_bpm` is missing");
        refused("[1, 2]", "must hold an object");
    }
}
