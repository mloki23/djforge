//! Production over a DJ set: "parts" are instrument tracks with a built-in instrument set to one
//! of the trance presets in [`super::prod_presets`] (`prod.new_part`, `prod.presets`). Any
//! hosted plugin can still go on a part through `mix.instrument` and the insert commands.

use super::prod_presets::{PRESETS, default_preset, preset};
use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_midi::patterns::PatternKind;
use soundcraft_model::{ChannelFormat, Session, TrackKind};
use std::collections::HashSet;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "prod.new_part",
            "New Part",
            [],
            None,
            "{kind: drums|bass|synth|fx, preset?: id (prod.presets; default trance_kit, rolling_bass, stab, noise_riser), name?} — an instrument track after the DJ decks (Deck A/B), named \"Drums 1\", \"Bass 2\"… unless `name` is given, with the preset's built-in instrument and parameters. One undo step. Returns {track, name, preset, instrument}",
            always,
            new_part
        ),
        cmd!(query "prod.presets", "Part Presets", [], None, "{kind?: drums|bass|synth|fx} — the presets prod.new_part takes: [{id, kind, instrument}]", always, presets),
    ]
}

/// Fields `prod.new_part` reads; anything else is refused rather than ignored.
const FIELDS: [&str; 3] = ["kind", "preset", "name"];

/// The part kind in `p`'s `kind`, if given.
fn kind_param(cmd: &str, p: &Value) -> Result<Option<PatternKind>> {
    match p.get("kind") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let k = v.as_str().ok_or_else(|| bad(cmd, "`kind` must be drums, bass, synth or fx"))?;
            PatternKind::parse(k).map(Some).map_err(|_| bad(cmd, format!("unknown kind `{k}`: drums, bass, synth or fx")))
        }
    }
}

/// What a part of `kind` is called before its number.
fn label(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::Drums => "Drums",
        PatternKind::Bass => "Bass",
        PatternKind::Synth => "Synth",
        PatternKind::Fx => "FX",
    }
}

/// "`base` n" with the first n from 1 no track is called.
fn numbered_name(s: &Session, base: &str) -> String {
    let taken = |n: &str| s.tracks.iter().any(|t| t.name == n);
    (1u64..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).unwrap_or_else(|| base.to_string())
}

/// The DJ deck tracks: those the set's transitions mix between and audio tracks holding
/// beat-locked clips; failing both, tracks named "Deck A" / "Deck B" (as `dj.import_set` names
/// them).
fn deck_tracks(s: &Session) -> Vec<TrackId> {
    let mixed: HashSet<TrackId> = s.dj.transitions.iter().flat_map(|t| [t.out_track, t.in_track]).collect();
    let decks: Vec<TrackId> = s
        .tracks
        .iter()
        .filter(|t| mixed.contains(&t.id) || (t.kind == TrackKind::Audio && t.clips().iter().any(|c| c.warp.is_some())))
        .map(|t| t.id)
        .collect();
    if !decks.is_empty() {
        return decks;
    }
    s.tracks.iter().filter(|t| t.kind == TrackKind::Audio && matches!(t.name.as_str(), "Deck A" | "Deck B")).map(|t| t.id).collect()
}

/// The outermost folder holding `t` that the session has, or `t` itself when it is in none.
fn outermost(s: &Session, t: TrackId) -> TrackId {
    let mut root = t;
    // The depth cap guards against a cyclic folder chain in a hostile session file; a folder id
    // naming no track ends the chain at the last folder that exists.
    for _ in 0..64 {
        match s.track(root).and_then(|tr| tr.folder).filter(|&parent| parent != t && s.track(parent).is_some()) {
            Some(parent) => root = parent,
            None => break,
        }
    }
    root
}

/// Where a new part goes: after the last DJ deck track (after the whole of its outermost folder
/// when it is in one, so the folder's members stay together below it) and after any instrument
/// tracks outside folders right after that (earlier parts), so parts stack in the order they were
/// made. With no decks, `None`: the end, before any master.
fn part_position(s: &Session) -> Option<usize> {
    let decks = deck_tracks(s);
    let last = s.tracks.iter().rposition(|t| decks.contains(&t.id))?;
    let root = outermost(s, s.tracks.get(last)?.id);
    let at = s.track_index(root)?;
    let block = s.tracks.iter().skip(at + 1).take_while(|t| super::track::in_folder(s, t.id, root)).count();
    let end = at + 1 + block;
    let parts = s.tracks.iter().skip(end).take_while(|t| t.kind == TrackKind::Instrument && t.folder.is_none()).count();
    Some(end + parts)
}

fn new_part(e: &mut Engine, p: &Value) -> Result<Value> {
    const CMD: &str = "prod.new_part";
    if let Some(k) = p.as_object().and_then(|o| o.keys().find(|k| !FIELDS.contains(&k.as_str()))) {
        return Err(bad(CMD, format!("unknown field `{k}`: a part takes kind, preset and name")));
    }
    let kind = kind_param(CMD, p)?.ok_or_else(|| bad(CMD, "`kind` required: drums, bass, synth or fx"))?;
    let id = match p.get("preset") {
        None | Some(Value::Null) => default_preset(kind),
        Some(v) => v.as_str().ok_or_else(|| bad(CMD, "`preset` must be a preset id (prod.presets)"))?,
    };
    let preset = preset(id).ok_or_else(|| bad(CMD, format!("unknown preset `{id}` (prod.presets lists them)")))?;
    if preset.kind != kind {
        return Err(bad(CMD, format!("`{id}` is a {} preset, not {}", preset.kind.name(), kind.name())));
    }
    let name = match p.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(CMD, "`name` must be a non-empty string"))?),
    };
    let info = soundcraft_dsp::plugin_info(preset.instrument).ok_or_else(|| bad(CMD, format!("the built-in `{}` is missing", preset.instrument)))?;
    let ins = super::instrument::plugin_with(info, preset.params.iter().copied());
    // The track is whole before it joins the session: nothing below can fail.
    let s = e.session_mut();
    let name = name.map_or_else(|| numbered_name(s, label(kind)), str::to_string);
    let mut track = s.make_track(TrackKind::Instrument, ChannelFormat::Stereo, Some(&name));
    track.instrument = Some(ins);
    let (t, name) = (track.id, track.name.clone());
    let at = part_position(s).unwrap_or_else(|| s.before_master()).min(s.tracks.len());
    s.tracks.insert(at, track);
    s.edit.selected_tracks = vec![t];
    Ok(json!({"track": t, "name": name, "preset": preset.id, "instrument": preset.instrument}))
}

fn presets(_: &mut Engine, p: &Value) -> Result<Value> {
    let kind = kind_param("prod.presets", p)?;
    let list: Vec<Value> = PRESETS
        .iter()
        .filter(|x| kind.is_none_or(|k| x.kind == k))
        .map(|x| json!({"id": x.id, "kind": x.kind.name(), "instrument": x.instrument}))
        .collect();
    Ok(Value::Array(list))
}

#[cfg(test)]
mod tests {
    use super::super::prod_presets::PRESETS;
    use crate::Engine;
    use serde_json::{Value, json};
    use soundcraft_midi::patterns::{CLAP, CLOSED_HAT, FX_PITCH, KICK, OPEN_HAT};
    use soundcraft_midi::{Note, Sequence};
    use soundcraft_model::{ChannelFormat, Clip, ClipId, SourceId, TrackId, TrackKind, Transition, WarpMap};
    use soundcraft_time::{Range, TICKS_PER_QUARTER};

    fn part(e: &mut Engine, p: Value) -> Value {
        e.execute("prod.new_part", &p).unwrap()
    }

    fn track_of(r: &Value) -> TrackId {
        TrackId(r["track"].as_u64().unwrap())
    }

    fn names(e: &Engine) -> Vec<String> {
        e.session().tracks.iter().map(|t| t.name.clone()).collect()
    }

    #[test]
    fn each_kind_gets_its_instrument_with_the_presets_parameters() {
        for x in PRESETS {
            let mut e = Engine::default();
            let r = part(&mut e, json!({"kind": x.kind.name(), "preset": x.id}));
            assert_eq!((r["preset"].as_str(), r["instrument"].as_str()), (Some(x.id), Some(x.instrument)));
            let tr = e.session().track(track_of(&r)).unwrap();
            assert_eq!(tr.kind, TrackKind::Instrument);
            let ins = tr.instrument.as_ref().unwrap();
            assert_eq!(ins.plugin, x.instrument);
            for (k, v) in x.params {
                assert_eq!(ins.params.get(*k).copied(), Some(*v), "{}: {k}", x.id);
            }
            let info = soundcraft_dsp::plugin_info(x.instrument).unwrap();
            assert_eq!(ins.params.len(), info.params.len(), "{}: the rest at their defaults", x.id);
            assert_eq!(e.session().edit.selected_tracks, vec![tr.id], "the new part is selected");
        }
    }

    #[test]
    fn a_part_without_a_preset_gets_its_kinds_default() {
        let mut e = Engine::default();
        for (kind, preset, plugin) in [
            ("drums", "trance_kit", "drum_synth"),
            ("bass", "rolling_bass", "subtractive_synth"),
            ("synth", "stab", "subtractive_synth"),
            ("fx", "noise_riser", "subtractive_synth"),
        ] {
            let r = part(&mut e, json!({"kind": kind}));
            assert_eq!(r["preset"], preset);
            assert_eq!(r["instrument"], plugin);
        }
    }

    #[test]
    fn parts_are_numbered_per_kind_and_a_given_name_is_kept_unique() {
        let mut e = Engine::default();
        let got: Vec<Value> = ["drums", "bass", "bass", "fx", "drums"].iter().map(|k| part(&mut e, json!({"kind": k}))["name"].clone()).collect();
        assert_eq!(got, ["Drums 1", "Bass 1", "Bass 2", "FX 1", "Drums 2"].map(Value::from));
        // A deleted part's number is taken again; a given name is used as is, made unique if taken.
        e.execute("track.delete", &json!({"tracks": ["Bass 1"]})).unwrap();
        assert_eq!(part(&mut e, json!({"kind": "bass"}))["name"], "Bass 1");
        assert_eq!(part(&mut e, json!({"kind": "synth", "name": "Hoover"}))["name"], "Hoover");
        let again = part(&mut e, json!({"kind": "synth", "name": "Hoover"}))["name"].as_str().unwrap().to_string();
        assert_ne!(again, "Hoover");
        let all = names(&e);
        assert_eq!(all.len(), all.iter().collect::<std::collections::HashSet<_>>().len(), "{all:?}");
    }

    #[test]
    fn parts_go_after_the_dj_decks_in_the_order_they_are_made() {
        let mut e = Engine::default();
        let s = e.session_mut();
        s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck A"));
        s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck B"));
        s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Vocal"));
        s.add_track(TrackKind::Master, ChannelFormat::Stereo, None);
        part(&mut e, json!({"kind": "drums"}));
        part(&mut e, json!({"kind": "bass"}));
        let all = names(&e);
        assert_eq!(&all[..5], ["Deck A", "Deck B", "Drums 1", "Bass 1", "Vocal"], "{all:?}");
        // With no decks, a part goes at the end (before the master).
        let mut e = Engine::default();
        e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Pad"));
        e.session_mut().add_track(TrackKind::Master, ChannelFormat::Stereo, None);
        part(&mut e, json!({"kind": "synth"}));
        let all = names(&e);
        assert_eq!(all[..2], ["Pad", "Synth 1"], "{all:?}");
        assert_eq!(e.session().tracks.last().map(|t| t.kind), Some(TrackKind::Master));
    }

    #[test]
    fn a_folder_id_naming_no_track_ends_the_folder_walk() {
        let mut e = Engine::default();
        let (a, _) = deck(&mut e, "Deck A", true);
        deck(&mut e, "Loose", false);
        if let Some(t) = e.session_mut().track_mut(a) {
            t.folder = Some(TrackId(9_999));
        }
        part(&mut e, json!({"kind": "bass"}));
        assert_eq!(names(&e), ["Deck A", "Bass 1", "Loose"]);
    }

    /// A noise riser part and an impact part, each holding a note over the first two seconds.
    fn noisy_session() -> Engine {
        let mut e = Engine::default();
        for (kind, preset) in [("fx", "noise_riser"), ("fx", "impact")] {
            let t = track_of(&part(&mut e, json!({"kind": kind, "preset": preset})));
            let mut seq = Sequence::default();
            seq.notes.push(Note { pitch: FX_PITCH, velocity: 120, release_velocity: 64, channel: 0, start: 0, length: 4 * TICKS_PER_QUARTER });
            let s = e.session_mut();
            let cid = s.new_clip_id();
            crate::edit::place_clip(s, t, Clip::midi(cid, preset, 0, 96_000, seq));
        }
        e
    }

    #[test]
    fn noise_bounces_repeat_and_a_normalised_bounce_stays_under_its_ceiling() {
        let dir = std::env::temp_dir().join(format!("sc-prod-bounce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bounce = |name: &str, normalize: bool| {
            let mut e = noisy_session();
            let path = dir.join(name);
            let r = e
                .execute(
                    "file.bounce_mix",
                    &json!({"path": path.to_string_lossy(), "bit_depth": "32f", "start": 0, "end": 96_000, "normalize": normalize}),
                )
                .unwrap();
            let (_, buf) = soundcraft_audio_io::decode(&std::fs::read(&path).unwrap(), Some("wav")).unwrap();
            (r, buf.channels)
        };
        let (_, first) = bounce("a.wav", false);
        let (_, second) = bounce("b.wav", false);
        assert!(first.iter().flatten().any(|x| x.abs() > 0.01), "the noise sounds");
        assert_eq!(first, second, "the same session bounces the same noise");
        // The second pass plays exactly what the first measured, so the peak lands on -0.1 dBFS.
        let (r, normalised) = bounce("n.wav", true);
        let peak = normalised.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
        let ceiling = soundcraft_dsp::db_to_gain(-0.1);
        assert!(peak <= ceiling * 1.000_01, "normalised peak {peak} over {ceiling} ({r})");
        assert!(peak >= ceiling * 0.999, "normalised peak {peak} short of {ceiling}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audio track `name` holding one clip, beat-locked when `warped`.
    fn deck(e: &mut Engine, name: &str, warped: bool) -> (TrackId, ClipId) {
        let s = e.session_mut();
        let t = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(name));
        let c = s.new_clip_id();
        let mut clip = Clip::audio(c, name, SourceId(90), 0, 0, 48_000);
        if warped {
            clip.warp = WarpMap::new(vec![0.0, 0.5, 1.0], 0.0);
            assert!(clip.warp.is_some());
        }
        crate::edit::place_clip(s, t, clip);
        (t, c)
    }

    #[test]
    fn decks_are_the_tracks_a_transition_mixes_whatever_they_are_called() {
        let mut e = Engine::default();
        let (left, lc) = deck(&mut e, "Left", false);
        let (right, rc) = deck(&mut e, "Right", false);
        deck(&mut e, "Deck A", false);
        let x: Transition =
            serde_json::from_value(json!({"id": 1, "out_clip": lc.0, "in_clip": rc.0, "out_track": left.0, "in_track": right.0})).unwrap();
        e.session_mut().dj.transitions.push(x);
        part(&mut e, json!({"kind": "bass"}));
        let all = names(&e);
        assert_eq!(all, ["Left", "Right", "Bass 1", "Deck A"], "a track called Deck A is not a deck when the set says otherwise");
    }

    #[test]
    fn a_track_with_a_beat_locked_clip_is_a_deck_without_a_transition() {
        let mut e = Engine::default();
        deck(&mut e, "Deck A", false);
        deck(&mut e, "Locked", true);
        deck(&mut e, "Loose", false);
        part(&mut e, json!({"kind": "fx"}));
        assert_eq!(names(&e), ["Deck A", "Locked", "FX 1", "Loose"]);
    }

    #[test]
    fn a_part_goes_below_the_whole_folder_its_last_deck_is_in() {
        let mut e = Engine::default();
        deck(&mut e, "Deck A", true);
        deck(&mut e, "Deck B", true);
        deck(&mut e, "After", false);
        e.execute("track.move_to_new_folder", &json!({"tracks": ["Deck A", "Deck B"], "name": "Set"})).unwrap();
        e.execute("track.new", &json!({"kind": "folder", "name": "Inner"})).unwrap();
        e.execute("track.move_to_folder", &json!({"tracks": ["Inner"], "folder": "Set"})).unwrap();
        let before = names(&e);
        assert_eq!(before, ["Set", "Deck A", "Deck B", "Inner", "After"]);
        part(&mut e, json!({"kind": "drums"}));
        part(&mut e, json!({"kind": "synth"}));
        assert_eq!(names(&e), ["Set", "Deck A", "Deck B", "Inner", "Drums 1", "Synth 1", "After"]);
        let s = e.session();
        assert!(s.tracks.iter().filter(|t| t.name.ends_with(" 1")).all(|t| t.folder.is_none()), "parts stay out of the folder");
    }

    #[test]
    fn refuses_unknown_kinds_presets_fields_and_a_preset_of_another_kind() {
        let mut e = Engine::default();
        for bad in [
            json!({}),
            json!({"kind": "vocals"}),
            json!({"kind": 3}),
            json!({"kind": "bass", "preset": "nope"}),
            json!({"kind": "bass", "preset": "trance_kit"}),
            json!({"kind": "drums", "preset": "stab"}),
            json!({"kind": "fx", "preset": 2}),
            json!({"kind": "bass", "name": ""}),
            json!({"kind": "bass", "cutoff": 200}),
        ] {
            assert!(e.execute("prod.new_part", &bad).is_err(), "{bad}");
        }
        assert!(e.session().tracks.is_empty(), "nothing was made");
        assert!(!e.can_undo(), "nothing was recorded");
        let err = e.execute("prod.new_part", &json!({"kind": "bass", "preset": "trance_kit"})).unwrap_err().to_string();
        assert!(err.contains("drums"), "{err}");
    }

    #[test]
    fn one_undo_removes_the_part() {
        let mut e = Engine::default();
        e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck A"));
        let before = names(&e);
        part(&mut e, json!({"kind": "drums"}));
        assert_eq!(e.session().tracks.len(), 2);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(names(&e), before);
        assert!(e.journal.iter().any(|(id, _)| id == "prod.new_part"), "journaled");
    }

    #[test]
    fn presets_list_every_preset_or_one_kinds() {
        let mut e = Engine::default();
        let all = e.execute("prod.presets", &json!({})).unwrap();
        assert_eq!(all.as_array().map(Vec::len), Some(PRESETS.len()));
        let fx = e.execute("prod.presets", &json!({"kind": "fx"})).unwrap();
        let ids: Vec<&str> = fx.as_array().unwrap().iter().filter_map(|x| x["id"].as_str()).collect();
        assert_eq!(ids, ["noise_riser", "downlifter", "impact"]);
        assert!(fx.as_array().unwrap().iter().all(|x| x["kind"] == "fx" && x["instrument"] == "subtractive_synth"));
        let drums = e.execute("prod.presets", &json!({"kind": "drums"})).unwrap();
        assert_eq!(drums, json!([{"id": "trance_kit", "kind": "drums", "instrument": "drum_synth"}]));
        assert!(e.execute("prod.presets", &json!({"kind": "vocals"})).is_err());
        assert!(!e.can_undo(), "a query");
    }

    /// `notes` (pitch, start and length in 8th notes) on a part with `preset` at the session's
    /// 120 bpm, a beat in, rendered for `secs` more: the peak over the render and the RMS while
    /// the notes are held, both in dBFS over both channels. The beat of silence first lets the
    /// instrument's level settle from its default, as it has long since in a playing session.
    fn render(kind: &str, preset: &str, notes: &[(u8, i64, i64)], secs: i64) -> (f32, f32) {
        let q = TICKS_PER_QUARTER;
        let mut e = Engine::default();
        let t = track_of(&e.execute("prod.new_part", &json!({"kind": kind, "preset": preset})).unwrap());
        let mut seq = Sequence::default();
        for &(pitch, start, eighths) in notes {
            seq.notes.push(Note { pitch, velocity: 110, release_velocity: 64, channel: 0, start: (start + 2) * q / 2, length: eighths * q / 2 });
        }
        let s = e.session_mut();
        let cid = s.new_clip_id();
        crate::edit::place_clip(s, t, Clip::midi(cid, "audition", 0, 24_000 + 48_000 * secs, seq));
        let out = soundcraft_mix::render_range(e.session(), Range::new(0, 24_000 + 48_000 * secs), 512);
        let peak = out.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
        // An 8th note is a quarter of a second at 120 bpm.
        let first = notes.iter().map(|n| n.1).min().unwrap_or(0) as usize * 12_000 + 24_000;
        let last = notes.iter().map(|n| n.1 + n.2).max().unwrap_or(0) as usize * 12_000 + 24_000;
        let during = || out.iter().flat_map(|c| c.iter().take(last).skip(first));
        let n = during().count().max(1) as f32;
        let rms = (during().map(|x| x * x).sum::<f32>() / n).sqrt();
        let db = |x: f32| 20.0 * x.max(1.0e-9).log10();
        (db(peak), db(rms))
    }

    #[test]
    fn every_preset_sounds_without_clipping() {
        // One bar of the trance groove for the kit; one note for the rest, held as its patterns
        // hold it (a beat of bass or synth, four bars of riser or downlifter, a beat of impact).
        let mut bar = Vec::new();
        for beat in 0..4 {
            bar.extend([(KICK, 2 * beat, 1), (CLOSED_HAT, 2 * beat, 1), (OPEN_HAT, 2 * beat + 1, 1)]);
        }
        bar.extend([(CLAP, 2, 1), (CLAP, 6, 1)]);
        let mut failed = Vec::new();
        for x in PRESETS {
            let (notes, secs): (Vec<(u8, i64, i64)>, i64) = match (x.kind.name(), x.id) {
                ("drums", _) => (bar.clone(), 2),
                ("bass", _) => (vec![(45, 0, 2)], 1),
                ("synth", _) => (vec![(69, 0, 2)], 1),
                (_, "impact") => (vec![(FX_PITCH, 0, 2)], 3),
                _ => (vec![(FX_PITCH, 0, 32)], 9),
            };
            let (peak, rms) = render(x.kind.name(), x.id, &notes, secs);
            println!("{:>13}: peak {peak:6.1} dBFS, rms {rms:6.1} dBFS", x.id);
            if peak >= 0.0 || !(-40.0..=-6.0).contains(&rms) {
                failed.push(x.id);
            }
        }
        assert!(failed.is_empty(), "clipped (peak >= 0 dBFS), nearly silent (rms < -40 dBFS) or too hot (rms > -6 dBFS): {failed:?}");
    }
}
