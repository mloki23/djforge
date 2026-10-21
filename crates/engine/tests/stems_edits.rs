//! Every edit keeps a split clip's stems and stem mask: the clips an edit makes from a split clip
//! (moved, trimmed, split, pasted, duplicated, nudged, rippled, undone, saved and reopened) play
//! the same four stems through the same mask. Edits that give a clip new audio leave it unsplit.

use serde_json::{Value, json};
use soundcraft_audio_io::{AudioBuffer, FileFormat};
use soundcraft_engine::Engine;
use soundcraft_model::{ChannelFormat, Clip, ClipId, Session, SourceId, StemSet, TrackId, TrackKind, WarpMap};
use soundcraft_time::Samples;

const SR: usize = 48_000;
/// One beat at 128 bpm.
const BEAT: f64 = 22_500.0;
const MASK: [bool; 4] = [false, true, true, true];

fn bar(s: &Session, bar: i64) -> Samples {
    s.tempo.tick_to_samples(s.tempo.bar_start_tick(bar), s.sample_rate)
}

/// Four stem sources as long as `src`. With `shared`, they play its audio (cheap for long
/// sources); otherwise each is its own constant level, so a reopened session shows which is which.
fn stems_of(s: &mut Session, src: SourceId, shared: bool) -> StemSet {
    let sources = std::array::from_fn(|i| {
        let audio = s.pool.get(src).unwrap().clone();
        if shared {
            let id = SourceId(s.alloc());
            let meta = s.source(src).unwrap().clone();
            s.sources.push(soundcraft_model::Source { id, name: format!("{} ({})", meta.name, soundcraft_model::STEMS[i]), ..meta });
            s.pool.insert(id, audio);
            id
        } else {
            let level = (i + 1) as f32 / 10.0;
            let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![level; audio.frames()]] };
            soundcraft_engine::io::add_source(s, soundcraft_model::STEMS[i], buf, None, FileFormat::Wav)
        }
    });
    StemSet { key: "0123456789abcdef-1000-htdemucs".into(), sources }
}

/// A 128 bpm session with one deck: a 20 s source and a beat-locked clip of it over bars 2..10,
/// split into stems with vocals off.
fn split_deck() -> (Engine, TrackId, ClipId, StemSet) {
    let mut s = Session::default();
    s.tempo.set_tempo(0, 128.0).unwrap();
    let t = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Deck"));
    let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![(0..20 * SR).map(|i| (i % 100) as f32 / 100.0).collect()] };
    let src = soundcraft_engine::io::add_source(&mut s, "track", buf, None, FileFormat::Wav);
    let set = stems_of(&mut s, src, false);
    let id = s.new_clip_id();
    let start = bar(&s, 2);
    let mut c = Clip::audio(id, "track", src, 0, start, bar(&s, 10) - start);
    c.warp = WarpMap::new((0..42).map(|k| k as f64 * BEAT).collect(), 0.0);
    c.stems = Some(set.clone());
    c.stem_mask = MASK;
    soundcraft_engine::edit::place_clip(&mut s, t, c);
    (Engine::new(s), t, id, set)
}

/// The clips (any track, any playlist) that play the split clip's source.
fn from_source(e: &Engine, set: &StemSet) -> Vec<Clip> {
    let src = set.sources[0];
    let s = e.session();
    let main = s.all_clips().find(|c| c.stems.as_ref().is_some_and(|x| x.sources[0] == src)).and_then(Clip::source);
    s.all_clips().filter(|c| c.source().is_some() && c.source() == main).cloned().collect()
}

/// Every clip made from the split clip keeps its stems and mask; there are `n` of them.
fn assert_kept(e: &Engine, set: &StemSet, n: usize, what: &str) {
    let clips = from_source(e, set);
    assert_eq!(clips.len(), n, "{what}: clips from the split clip");
    for c in &clips {
        assert_eq!(c.stems.as_ref(), Some(set), "{what}: clip {} keeps its stems", c.id.0);
        assert_eq!(c.stem_mask, MASK, "{what}: clip {} keeps its mask", c.id.0);
    }
}

fn run(e: &mut Engine, cmd: &str, p: Value) -> Value {
    e.execute(cmd, &p).unwrap_or_else(|err| panic!("{cmd}: {err}"))
}

#[test]
fn moving_and_trimming_both_sides_keep_the_stems() {
    let (mut e, _, id, set) = split_deck();
    run(&mut e, "edit.move_clips", json!({"clips": [id.0], "by": SR}));
    assert_kept(&e, &set, 1, "move");
    let c = e.session().find_clip(id).unwrap().1.clone();
    run(&mut e, "edit.trim_clip", json!({"clip": id.0, "start": c.start + SR as i64}));
    assert_kept(&e, &set, 1, "trim start");
    run(&mut e, "edit.trim_clip", json!({"clip": id.0, "end": c.end() - SR as i64}));
    assert_kept(&e, &set, 1, "trim end");
    let c2 = e.session().find_clip(id).unwrap().1;
    assert_eq!((c2.start, c2.end()), (c.start + SR as i64, c.end() - SR as i64), "the trims happened");
}

#[test]
fn both_halves_of_a_split_keep_the_stems_and_undo_redo_keep_them_too() {
    let (mut e, t, id, set) = split_deck();
    let mid = bar(e.session(), 6);
    run(&mut e, "edit.separate", json!({"tracks": [t.0], "at": mid}));
    assert_kept(&e, &set, 2, "separate");
    assert!(e.undo());
    assert_kept(&e, &set, 1, "undo");
    assert!(e.session().find_clip(id).is_some());
    assert!(e.redo());
    assert_kept(&e, &set, 2, "redo");
    // Healing the halves gives back one split clip.
    let r = run(&mut e, "edit.heal", json!({"tracks": [t.0], "start": mid - 1, "end": mid + 1}));
    assert_kept(&e, &set, 1, &format!("heal {r}"));
}

#[test]
fn a_split_half_does_not_heal_into_an_unsplit_one() {
    let (mut e, t, _, set) = split_deck();
    let mid = bar(e.session(), 6);
    run(&mut e, "edit.separate", json!({"tracks": [t.0], "at": mid}));
    let right = from_source(&e, &set).into_iter().find(|c| c.start == mid).unwrap().id;
    e.session_mut().find_clip_mut(right).unwrap().stems = None;
    run(&mut e, "edit.heal", json!({"tracks": [t.0], "start": mid - 1, "end": mid + 1}));
    let clips = e.session().track(t).unwrap().clips().to_vec();
    assert_eq!(clips.len(), 2, "different audio stays separate");
    assert_eq!(clips.iter().map(|c| c.stems.is_some()).collect::<Vec<_>>(), [true, false]);
}

#[test]
fn copy_paste_and_duplicate_keep_the_stems() {
    let (mut e, t, _, set) = split_deck();
    let (a, b) = (bar(e.session(), 3), bar(e.session(), 5));
    run(&mut e, "edit.copy", json!({"tracks": [t.0], "start": a, "end": b}));
    let at = bar(e.session(), 20);
    run(&mut e, "edit.paste", json!({"tracks": [t.0], "at": at}));
    assert_kept(&e, &set, 2, "paste");
    let pasted = e.session().track(t).unwrap().clips().iter().find(|c| c.start == at).unwrap().clone();
    assert_eq!(pasted.length, b - a, "the copied part was pasted");
    run(&mut e, "edit.duplicate", json!({"tracks": [t.0], "start": a, "end": b}));
    assert!(from_source(&e, &set).len() > 2, "duplicate added a clip");
    let n = from_source(&e, &set).len();
    assert_kept(&e, &set, n, "duplicate");
}

#[test]
fn a_nudge_keeps_the_stems() {
    let (mut e, _, id, set) = split_deck();
    let before = e.session().find_clip(id).unwrap().1.warp.as_ref().unwrap().anchor;
    run(&mut e, "dj.nudge", json!({"clip": id.0, "beats": 1.0}));
    assert_kept(&e, &set, 1, "nudge");
    assert_eq!(e.session().find_clip(id).unwrap().1.warp.as_ref().unwrap().anchor, before - 1.0);
}

#[test]
fn a_session_saved_and_reopened_keeps_the_stems_and_their_audio() {
    let dir = std::env::temp_dir().join(format!("djforge-stems-edits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("Set").join("Set.scraft").to_string_lossy().into_owned();
    let (mut e, _, id, set) = split_deck();
    e.session_mut().tracks[0].mixer.stem_gain_db = [0.0, -3.0, -144.0, 6.0];
    run(&mut e, "session.save_as", json!({"path": path}));
    let mut opened = Engine::default();
    let r = run(&mut opened, "session.open", json!({"path": path}));
    assert_eq!(r["missing"], json!([]), "{r}");
    let s = opened.session();
    let c = s.find_clip(id).unwrap().1;
    let got = c.stems.as_ref().unwrap();
    assert_eq!((got.key.as_str(), c.stem_mask), (set.key.as_str(), MASK));
    for (i, src) in got.sources.iter().enumerate() {
        let audio = s.pool.get(*src).unwrap_or_else(|| panic!("stem {i} is loaded"));
        assert_eq!(audio.frames(), 20 * SR, "stem {i} is as long as the source");
        assert_eq!(audio.buffer.channels[0][0], (i + 1) as f32 / 10.0, "stem {i} plays its own audio");
    }
    assert_eq!(s.tracks[0].mixer.stem_gain_db, [0.0, -3.0, -144.0, 6.0]);
    let health = run(&mut opened, "session.audio_health", json!({}));
    assert_eq!(health["healthy"], true, "{health}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_tempo_change_relocks_a_split_clip_exactly_like_an_unsplit_one() {
    let (mut e, t, id, set) = split_deck();
    // The same clip, unsplit, on a second deck.
    let s = e.session_mut();
    let other = s.add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Twin"));
    let twin = s.new_clip_id();
    let mut c = s.find_clip(id).unwrap().1.clone();
    c.id = twin;
    c.stems = None;
    c.stem_mask = [true; 4];
    soundcraft_engine::edit::place_clip(s, other, c);
    let before = e.session().find_clip(id).unwrap().1.clone();
    run(&mut e, "event.tempo", json!({"bpm": 132.0, "at": "bars_beats:4|1|000"}));
    let s = e.session();
    let (split, plain) = (s.find_clip(id).unwrap().1, s.find_clip(twin).unwrap().1);
    assert_ne!((split.length, split.warp.as_ref().unwrap().lock), (before.length, before.warp.as_ref().unwrap().lock), "the clip relocked");
    assert_eq!((split.start, split.length, &split.warp), (plain.start, plain.length, &plain.warp));
    assert_eq!((split.stems.as_ref(), split.stem_mask), (Some(&set), MASK));
    assert_eq!(e.session().track(t).unwrap().clips().len(), 1);
}

#[test]
fn rendering_a_split_clip_into_new_audio_leaves_it_unsplit() {
    let (mut e, _, id, _) = split_deck();
    e.session_mut().find_clip_mut(id).unwrap().gain_db = -6.0;
    run(&mut e, "clip.gain_render", json!({"clips": [id.0]}));
    let c = e.session().find_clip(id).unwrap().1;
    assert!(c.stems.is_none() && c.stem_mask == [true; 4], "the stems were split from the old audio");
}

/// The stage 2 set (dj_move's fixture): 128 bpm, 132 from bar 45, on two decks: A (deck 0, bars
/// 1..61) into B (deck 1, bars 25..101) entering on bar 25, then B into C (deck 0, bars 73..140)
/// on bar 73. Every clip is beat-locked over one 200 s source, here split into stems.
fn stage2_set() -> (Engine, Vec<ClipId>, StemSet) {
    let mut s = Session::default();
    s.tempo.set_tempo(0, 128.0).unwrap();
    let decks = [s.add_track(TrackKind::Audio, ChannelFormat::Mono, None), s.add_track(TrackKind::Audio, ChannelFormat::Mono, None)];
    let buf = AudioBuffer { sample_rate: SR as u32, channels: vec![vec![0.0; 200 * SR]] };
    let src = soundcraft_engine::io::add_source(&mut s, "deck", buf, None, FileFormat::Wav);
    let set = stems_of(&mut s, src, true);
    let mut ids = Vec::new();
    for (deck, from, to) in [(0, 1, 61), (1, 25, 101), (0, 73, 140)] {
        let id = s.new_clip_id();
        let start = bar(&s, from);
        let mut c = Clip::audio(id, "c", src, 0, start, bar(&s, to) - start);
        c.warp = WarpMap::new((0..420).map(|k| k as f64 * BEAT).collect(), 0.0);
        c.stems = Some(set.clone());
        c.stem_mask = MASK;
        soundcraft_engine::edit::place_clip(&mut s, decks[deck], c);
        ids.push(id);
    }
    let mut e = Engine::new(s);
    run(&mut e, "event.tempo", json!({"bpm": 132.0, "at": "bars_beats:45|1|000"}));
    run(&mut e, "dj.transition", json!({"out_clip": ids[0].0, "in_clip": ids[1].0, "entry_bar": 25}));
    run(&mut e, "dj.transition", json!({"out_clip": ids[1].0, "in_clip": ids[2].0, "entry_bar": 73}));
    (e, ids, set)
}

#[test]
fn moving_a_transition_keeps_every_rippled_clips_stems() {
    let (mut e, ids, set) = stage2_set();
    assert_kept(&e, &set, 3, "the set");
    let before: Vec<Samples> = ids.iter().map(|id| e.session().find_clip(*id).unwrap().1.start).collect();
    for bars in [4, -8] {
        run(&mut e, "dj.move_transition", json!({"transition": 1, "bars": bars}));
        assert_kept(&e, &set, 3, &format!("move_transition {bars}"));
    }
    let after: Vec<Samples> = ids.iter().map(|id| e.session().find_clip(*id).unwrap().1.start).collect();
    assert_ne!(before, after, "the incoming clips moved");
    assert!(e.undo() && e.undo());
    assert_kept(&e, &set, 3, "undo");
}

#[test]
fn stretch_renders_refuse_a_split_clip_and_change_nothing() {
    // They render the clip's own audio, so a masked clip (an acapella) would become the full mix.
    let (mut e, t, id, _) = split_deck();
    e.session_mut().track_mut(t).unwrap().elastic = Some("polyphonic".into());
    e.session_mut().find_clip_mut(id).unwrap().stretch = 1.5;
    let c = e.session().find_clip(id).unwrap().1.clone();
    let before = e.session().clone();
    for (cmd, p) in [
        ("clip.elastic_properties", json!({"clips": [id.0], "ratio": 1.5})),
        ("clip.remove_pitch_shift", json!({"clips": [id.0]})),
        ("edit.tce_to_timeline", json!({"clips": [id.0], "start": c.start, "end": c.end() + SR as i64})),
    ] {
        let err = e.execute(cmd, &p).unwrap_err().to_string();
        assert!(err.contains("remove the stems first (Remove Stems), then try again"), "{cmd}: {err}");
        assert!(*e.session() == before, "{cmd} changed the session");
    }
}
