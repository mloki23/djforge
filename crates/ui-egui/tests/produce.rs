//! The Produce menu: New Part's preset submenus, the Starter Pattern dialog (key from the set,
//! Replace only on a click), Add Instrument Column, the MIDI Editor's scale shading and drum grid,
//! and the Session view's MIDI slot menu and notes window.
//!
//! A 120 bpm WAV in a temporary folder (removed on drop) plays beat-locked from bar 1 as the set,
//! its source keyed Am; no analysis is set up (never mixforge).
#![allow(clippy::unwrap_used)]

use egui::{Modifiers, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use serde_json::json;
use soundcraft_audio_io::{AudioBuffer, EncodeOptions};
use soundcraft_engine::Engine;
use soundcraft_midi::Key;
use soundcraft_midi::patterns::{PatternKind, PatternStyle};
use soundcraft_model::{Clip, ClipContent, ClipId, TrackId};
use soundcraft_ui_egui::{Services, SoundApp};
use std::path::PathBuf;

/// A temporary folder, removed with everything in it when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("djforge-produce-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn step(h: &mut Harness<'_, SoundApp>) {
    let mut raw = std::mem::take(h.input_mut());
    h.state_mut().raw_input_hook(&mut raw);
    *h.input_mut() = raw;
    h.step();
}

fn steps(h: &mut Harness<'_, SoundApp>, n: usize) {
    for _ in 0..n {
        step(h);
    }
}

/// The app with a 30 s set at 120 bpm in Am, beat-locked from bar 1.
fn harness(dir: &Scratch) -> Harness<'static, SoundApp> {
    let buf = AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.1; 30 * 48_000]; 2] };
    let path = dir.0.join("set.wav");
    std::fs::write(&path, soundcraft_audio_io::encode(&buf, &EncodeOptions::default()).unwrap()).unwrap();
    let mut e = Engine::default();
    e.execute("dj.load_warped", &json!({"path": path, "bpm": 120})).unwrap();
    assert!(e.session_mut().sources.last_mut().unwrap().set_key("Am"));
    let app = SoundApp::new(e, None, Services::default());
    // Short frames, so two clicks a frame apart are a double-click.
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).with_step_dt(1.0 / 30.0).build_ui_state(
        |ui, app: &mut SoundApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app,
    );
    steps(&mut h, 4);
    h
}

/// Opens the Produce menu and clicks `item` (in `submenu` first, when given).
fn produce(h: &mut Harness<'_, SoundApp>, submenu: Option<&str>, item: &str) {
    h.get_by_label("Produce").click();
    steps(h, 3);
    if let Some(sub) = submenu {
        let sub = format!("{sub} ⏵");
        h.get_by_label(&sub).hover();
        steps(h, 3);
        h.get_by_label(&sub).click();
        steps(h, 3);
    }
    h.get_by_label(item).click();
    steps(h, 4);
}

/// Opens `label`'s context menu and clicks `item`.
fn menu(h: &mut Harness<'_, SoundApp>, label: &str, item: &str) {
    h.get_by_label(label).click_secondary();
    steps(h, 3);
    h.get_by_label(item).click();
    steps(h, 4);
}

fn track(h: &Harness<'_, SoundApp>, name: &str) -> TrackId {
    h.state().engine.session().track_by_name(name).unwrap().id
}

fn clips(h: &Harness<'_, SoundApp>, t: TrackId) -> Vec<Clip> {
    h.state().engine.session().track(t).unwrap().clips().to_vec()
}

fn undo_steps(h: &Harness<'_, SoundApp>) -> usize {
    h.state().engine.undo_history().len()
}

#[test]
fn the_produce_menu_makes_a_part_with_the_chosen_preset() {
    let dir = Scratch::new("part");
    let mut h = harness(&dir);
    produce(&mut h, Some("New Bass Part"), "Offbeat Bass");
    let made = h.state().engine.session().track_by_name("Bass 1").unwrap().instrument.clone().unwrap();
    let mut reference = Engine::default();
    let want = |e: &mut Engine, preset: &str| {
        let t = e.execute("prod.new_part", &json!({"kind": "bass", "preset": preset})).unwrap();
        e.session().track(TrackId(t["track"].as_u64().unwrap())).unwrap().instrument.clone().unwrap()
    };
    assert_eq!(made, want(&mut reference, "offbeat_bass"));
    assert_ne!(made, want(&mut reference, "rolling_bass"));
    // Each kind's default comes first and says so.
    produce(&mut h, Some("New Drums Part"), "Trance Kit (default)");
    assert!(h.state().engine.session().track_by_name("Drums 1").is_some());
}

#[test]
fn the_dj_menu_no_longer_carries_the_production_items() {
    let dir = Scratch::new("dj");
    let mut h = harness(&dir);
    h.get_by_label("DJ").click();
    steps(&mut h, 3);
    let _ = h.get_by_label("Prep Tracks... ");
    for gone in ["New Drums Part", "New Bass Part", "Trance Drums over Selection", "Starter Pattern..."] {
        assert!(h.query_by_label_contains(gone).is_none(), "{gone} is in a menu");
    }
}

#[test]
fn the_dialog_prefills_the_key_from_the_set_and_creates_a_pattern() {
    let dir = Scratch::new("dialog");
    let mut h = harness(&dir);
    h.state_mut().run("prod.new_part", json!({"kind": "bass"})).unwrap();
    let bass = track(&h, "Bass 1");
    produce(&mut h, None, "Starter Pattern...");
    let d = h.state().produce.pattern.clone().unwrap();
    assert_eq!((d.key.as_str(), d.slot, d.seed), ("Am", None, None));
    let _ = h.get_by_label("On Bass 1, from the playhead's bar");
    {
        let d = h.state_mut().produce.pattern.as_mut().unwrap();
        d.kind = PatternKind::Bass;
        d.style = PatternStyle::Rolling;
        d.bars = 4;
    }
    steps(&mut h, 2);
    let _ = h.get_by_label("Am · 8A");
    let before = undo_steps(&h);
    h.get_by_label("Shuffle").click();
    steps(&mut h, 2);
    assert!(h.state().produce.pattern.as_ref().unwrap().seed.is_some(), "Shuffle fixes a new seed");
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    assert!(h.state().produce.pattern.is_none(), "the dialog closes");
    let made = clips(&h, bass);
    assert_eq!(made.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Rolling (8A)"]);
    assert_eq!(undo_steps(&h), before + 1, "one undo step");
}

#[test]
fn an_overlap_offers_replace_and_nothing_is_replaced_without_the_click() {
    let dir = Scratch::new("replace");
    let mut h = harness(&dir);
    h.state_mut().run("prod.new_part", json!({"kind": "drums"})).unwrap();
    let drums = track(&h, "Drums 1");
    h.state_mut().run("prod.pattern", json!({"kind": "drums", "style": "four_floor", "bars": 4})).unwrap();
    let first = clips(&h, drums);
    produce(&mut h, None, "Starter Pattern...");
    h.state_mut().produce.pattern.as_mut().unwrap().bars = 4;
    steps(&mut h, 2);
    let before = undo_steps(&h);
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    let d = h.state().produce.pattern.clone().unwrap();
    assert!(d.error.as_deref().is_some_and(|e| e.contains("overlap")), "{d:?}");
    let _ = h.get_by_label("Replace");
    assert_eq!(clips(&h, drums), first, "refused: nothing replaced");
    assert_eq!(undo_steps(&h), before);
    // Another field changed: Replace no longer stands for what was refused.
    h.state_mut().produce.pattern.as_mut().unwrap().bars = 2;
    steps(&mut h, 2);
    assert!(h.query_by_label("Replace").is_none());
    h.state_mut().produce.pattern.as_mut().unwrap().bars = 4;
    steps(&mut h, 2);
    assert!(h.query_by_label("Replace").is_none(), "only a new refusal offers it again");
    // Cancel leaves it all as it was.
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    h.get_by_label("Cancel").click();
    steps(&mut h, 2);
    assert!(h.state().produce.pattern.is_none());
    assert_eq!(clips(&h, drums), first);
    // Replace replaces, as one undo step.
    produce(&mut h, None, "Starter Pattern...");
    {
        let d = h.state_mut().produce.pattern.as_mut().unwrap();
        d.bars = 4;
        d.style = PatternStyle::Fill;
    }
    steps(&mut h, 2);
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    h.get_by_label("Replace").click();
    steps(&mut h, 3);
    assert!(h.state().produce.pattern.is_none());
    assert_eq!(clips(&h, drums).iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Fill"]);
    assert_eq!(undo_steps(&h), before + 1);
}

/// Shows clip `clip` in the MIDI Editor.
fn edit_clip(h: &mut Harness<'_, SoundApp>, clip: ClipId) {
    h.state_mut().run("edit.select", json!({"clips": [clip.0]})).unwrap();
    h.state_mut().ui.show_midi_editor = true;
    steps(h, 4);
}

fn notes(h: &Harness<'_, SoundApp>, clip: ClipId) -> Vec<soundcraft_midi::Note> {
    match &h.state().engine.session().find_clip(clip).unwrap().1.content {
        ClipContent::Midi { sequence } => sequence.notes.clone(),
        _ => panic!("not MIDI"),
    }
}

#[test]
fn a_drum_grid_cell_toggles_a_16th_and_shift_click_accents_it() {
    let dir = Scratch::new("grid");
    let mut h = harness(&dir);
    h.state_mut().run("prod.new_part", json!({"kind": "drums"})).unwrap();
    let r = h.state_mut().run("prod.pattern", json!({"kind": "drums", "style": "four_floor", "bars": 4})).unwrap();
    let clip = ClipId(r["clip"].as_u64().unwrap());
    edit_clip(&mut h, clip);
    h.get_by_label("Grid").click();
    steps(&mut h, 3);
    let clap = |n: &soundcraft_midi::Note| n.pitch == 39 && n.start == 240;
    assert!(!notes(&h, clip).iter().any(clap));
    let cell = "Hand Clap bar 1 step 2";
    let before = undo_steps(&h);
    h.get_by_label(cell).click();
    steps(&mut h, 2);
    let added: Vec<_> = notes(&h, clip).into_iter().filter(clap).collect();
    assert_eq!(added.iter().map(|n| (n.velocity, n.length)).collect::<Vec<_>>(), [(96, 240)]);
    assert_eq!(undo_steps(&h), before + 1, "one undo step a click");
    h.get_by_label(cell).click_modifiers(Modifiers::SHIFT);
    steps(&mut h, 2);
    assert_eq!(notes(&h, clip).iter().filter(|n| clap(n)).map(|n| n.velocity).collect::<Vec<_>>(), [127]);
    h.get_by_label(cell).click_modifiers(Modifiers::SHIFT);
    steps(&mut h, 2);
    assert_eq!(notes(&h, clip).iter().filter(|n| clap(n)).map(|n| n.velocity).collect::<Vec<_>>(), [96]);
    h.get_by_label(cell).click();
    steps(&mut h, 2);
    assert!(!notes(&h, clip).iter().any(clap), "a second click removes it");
    assert_eq!(undo_steps(&h), before + 4);
    // Four bars page by two.
    assert!(h.query_by_label("Bass Drum 1 bar 3 step 1").is_none());
    h.get_by_label("▶").click();
    steps(&mut h, 3);
    let _ = h.get_by_label("Bass Drum 1 bar 3 step 1");
}

#[test]
fn the_rows_shade_the_sets_key_and_the_override_edits_nothing() {
    let dir = Scratch::new("scale");
    let mut h = harness(&dir);
    h.state_mut().run("prod.new_part", json!({"kind": "bass"})).unwrap();
    let r = h.state_mut().run("prod.pattern", json!({"kind": "bass", "style": "rolling", "bars": 2})).unwrap();
    let clip = ClipId(r["clip"].as_u64().unwrap());
    edit_clip(&mut h, clip);
    assert_eq!(h.state().midi.scale, Key::parse("Am").ok());
    let _ = h.get_by_label("Key: Am (8A)");
    assert!(h.query_by_label("Grid").is_none(), "a bass line is no drum clip");
    let session = h.state().engine.session().clone();
    let (steps_before, dirty) = (undo_steps(&h), h.state().engine.is_dirty());
    h.get_by_label("Scale").click();
    steps(&mut h, 3);
    let _ = h.get_by_label("Set's key (Am)");
    h.get_by_label("Em (9A)").click();
    steps(&mut h, 3);
    assert_eq!(h.state().midi.scale, Key::parse("Em").ok());
    let _ = h.get_by_label("Key: Em (9A)");
    assert!(*h.state().engine.session() == session, "the override is a view setting");
    assert_eq!((undo_steps(&h), h.state().engine.is_dirty()), (steps_before, dirty));
}

#[test]
fn the_slot_menu_writes_a_starter_pattern_and_offers_replace() {
    let dir = Scratch::new("slot");
    let mut h = harness(&dir);
    // On a session without Live columns, it sets them up (as View › Session does) and adds one.
    produce(&mut h, None, "Add Instrument Column");
    assert!(h.state().session_view.open);
    let s = h.state().engine.session();
    assert_eq!(s.live.column_kind(4, &s.tracks), Some(soundcraft_model::SlotKind::Midi));
    let name = s.track(s.live.column_track(4, &s.tracks).unwrap()).unwrap().name.clone();
    let audio = s.track(s.live.column_track(0, &s.tracks).unwrap()).unwrap().name.clone();
    // The instrument column's name is marked; an audio column's isn't.
    let _ = h.get_by_label(&format!("♪ {name}"));
    let _ = h.get_by_label(&audio);
    menu(&mut h, "Live 5 slot 1", "New Starter Pattern Here...");
    let d = h.state().produce.pattern.clone().unwrap();
    assert_eq!((d.slot, d.bars), (Some((4, 0)), 4));
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    let slot = h.state().engine.session().live.midi_slot(4, 0).cloned().unwrap();
    assert_eq!((slot.name.as_str(), slot.length_beats), ("Four Floor", 16.0));
    // A filled slot: refused, then Replace.
    menu(&mut h, "Live 5 slot 1", "New Starter Pattern Here...");
    h.state_mut().produce.pattern.as_mut().unwrap().style = PatternStyle::TranceFull;
    steps(&mut h, 2);
    h.get_by_label("Create").click();
    steps(&mut h, 3);
    assert_eq!(h.state().engine.session().live.midi_slot(4, 0).map(|x| x.name.as_str()), Some("Four Floor"));
    h.get_by_label("Replace").click();
    steps(&mut h, 3);
    assert_eq!(h.state().engine.session().live.midi_slot(4, 0).map(|x| x.name.as_str()), Some("Trance Full"));
    // Copy Selected MIDI Clip Here asks before replacing too.
    h.state_mut().run("prod.new_part", json!({"kind": "synth"})).unwrap();
    let r = h.state_mut().run("prod.pattern", json!({"kind": "synth", "style": "stabs", "bars": 2})).unwrap();
    h.state_mut().run("edit.select", json!({"clips": [r["clip"]]})).unwrap();
    steps(&mut h, 2);
    menu(&mut h, "Live 5 slot 1", "Copy Selected MIDI Clip Here");
    assert!(h.state().produce.replace.is_some());
    assert_eq!(h.state().engine.session().live.midi_slot(4, 0).map(|x| x.name.as_str()), Some("Trance Full"));
    h.get_by_label("Replace").click();
    steps(&mut h, 3);
    assert!(h.state().produce.replace.is_none());
    assert_eq!(h.state().engine.session().live.midi_slot(4, 0).map(|x| x.name.as_str()), Some("Stabs (8A)"));
}

#[test]
fn double_clicking_a_midi_slot_shows_its_notes_to_copy_out_for_editing() {
    let dir = Scratch::new("notes");
    let mut h = harness(&dir);
    h.state_mut().run("window.session", json!({"value": true})).unwrap();
    h.state_mut().run("live.add_column", json!({"kind": "instrument"})).unwrap();
    h.state_mut().run("prod.pattern", json!({"kind": "drums", "style": "four_floor", "slot": {"column": 4, "slot": 0}})).unwrap();
    steps(&mut h, 3);
    double_click(&mut h, "Live 5 slot 1");
    steps(&mut h, 3);
    assert_eq!(h.state().session_view.notes.as_ref().map(|n| (n.0, n.1)), Some((4, 0)));
    let _ = h.get_by_label("16 notes · 4 bars · read-only");
    let before = undo_steps(&h);
    h.get_by_label("Copy to Arrangement at Playhead").click();
    steps(&mut h, 3);
    let s = h.state().engine.session();
    let column_track = s.live.column_track(4, &s.tracks).unwrap();
    let made = clips(&h, column_track);
    assert_eq!(made.iter().map(|c| (c.name.as_str(), c.start)).collect::<Vec<_>>(), [("Four Floor", 0)]);
    let slot_notes = h.state().engine.session().live.midi_slot(4, 0).unwrap().sequence.notes.clone();
    assert_eq!(notes(&h, made[0].id), slot_notes);
    assert_eq!(undo_steps(&h), before + 1, "one undo step");
    assert!(h.state().ui.show_midi_editor);
    assert_eq!(h.state().engine.session().edit.selected_clips, vec![made[0].id]);
    assert!(h.state().session_view.notes.is_none());
    // Where a clip already is, nothing is replaced.
    double_click(&mut h, "Live 5 slot 1");
    steps(&mut h, 3);
    h.get_by_label("Copy to Arrangement at Playhead").click();
    steps(&mut h, 3);
    assert_eq!(clips(&h, column_track).len(), 1);
    assert!(h.state().session_view.notes.as_ref().is_some_and(|n| n.2.as_deref().is_some_and(|e| e.contains("Four Floor"))));
}

/// Double-clicks `label`: two clicks a frame apart, after a pause (egui counts a click within
/// 0.6 s of the one before last as a triple-click).
fn double_click(h: &mut Harness<'_, SoundApp>, label: &str) {
    steps(h, 30);
    for _ in 0..2 {
        h.get_by_label(label).click();
        step(h);
    }
}
