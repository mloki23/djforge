//! Deck tints on the DJ decks' clips and the transition markers in the Edit window.

use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Shape, epaint::ClippedShape, pos2, vec2};
use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_model::{
    ChannelFormat, Clip, ClipId, NudgeSource, Session, SourceId, Track, TrackId, TrackKind, Transition, TransitionStyle, WarpMap,
};
use soundcraft_ui_egui::dj_marks::{self, Deck, Decks, Mark, MarkKind};
use soundcraft_ui_egui::edit_window::{rect, x_of};
use soundcraft_ui_egui::theme::studio;
use soundcraft_ui_egui::{Services, SoundApp};

const BAR: i64 = 96_000; // 4/4 at 120 bpm, 48 kHz
const FAST_BAR: i64 = 90_000; // at 128 bpm

fn clip(s: &mut Session, track: TrackId, name: &str, start: i64, len: i64, locked: bool) -> ClipId {
    let id = s.new_clip_id();
    let mut c = Clip::audio(id, name, SourceId(900 + id.0), 0, start, len);
    if locked {
        c.warp = Some(WarpMap { beats: vec![0.0, 24_000.0], anchor: 0.0, lock: None });
    }
    if let Some(p) = s.track_mut(track).and_then(Track::playlist_mut) {
        p.clips.push(c);
    }
    id
}

fn transition(
    id: u64,
    (out_clip, out_track): (ClipId, TrackId),
    (in_clip, in_track): (ClipId, TrackId),
    entry_bar: i64,
    style: TransitionStyle,
) -> Transition {
    let (build_bars, out_bars) = style.default_bars();
    Transition {
        id,
        out_clip,
        in_clip,
        out_track,
        in_track,
        entry_bar,
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

/// A small set laid out as `dj.import_set` lays one out: "Sunrise" on Deck A into "Orbit" on
/// Deck B (a blend entering on bar 17) into "Lift" back on Deck A (a quick mix on bar 49), the
/// tempo stepping from 120 to 128 bpm on bar 33; a vocal track and a loose clip besides.
struct Set {
    engine: Engine,
    decks: [TrackId; 2],
    vocal: TrackId,
    clips: [ClipId; 3],
    loose: ClipId,
    vocal_clip: ClipId,
}

fn set() -> Set {
    let mut engine = Engine::default();
    let s = engine.session_mut();
    let a = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck A"));
    let b = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck B"));
    let vocal = s.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Vocal"));
    let sunrise = clip(s, a, "Sunrise", 0, 44 * BAR, true);
    let orbit = clip(s, b, "Orbit", 16 * BAR, 60 * BAR, true);
    let lift = clip(s, a, "Lift", 3_072_000 + 15 * FAST_BAR, 40 * BAR, true);
    let loose = clip(s, a, "Loose", 100 * BAR, BAR, false);
    let vocal_clip = clip(s, vocal, "Hook", 0, 8 * BAR, true);
    let bar33 = s.tempo.bar_start_tick(33);
    s.tempo.set_tempo(bar33, 128.0).unwrap();
    s.dj.transitions =
        vec![transition(1, (sunrise, a), (orbit, b), 17, TransitionStyle::Blend), transition(2, (orbit, b), (lift, a), 49, TransitionStyle::Quick)];
    s.edit.zoom.samples_per_px = 6_000.0;
    Set { engine, decks: [a, b], vocal, clips: [sunrise, orbit, lift], loose, vocal_clip }
}

#[test]
fn markers_sit_on_the_mix_in_hit_and_bass_gone_bars() {
    let set = set();
    let s = set.engine.session();
    let got: Vec<(u64, MarkKind, i64, i64)> = dj_marks::marks(s).iter().map(|m| (m.transition, m.kind, m.bar, m.at)).collect();
    let after = |bar: i64| 32 * BAR + (bar - 33) * FAST_BAR;
    assert_eq!(
        got,
        [
            (1, MarkKind::MixIn, 17, 16 * BAR),
            (1, MarkKind::Hit, 33, 32 * BAR),
            // Blend: the outgoing bass is gone 8 bars after the hit.
            (1, MarkKind::MixOut, 41, after(41)),
            (2, MarkKind::MixIn, 49, after(49)),
            (2, MarkKind::Hit, 57, after(57)),
            // Quick: 4 bars after the hit.
            (2, MarkKind::MixOut, 61, after(61)),
        ]
    );
    let m = dj_marks::marks(s);
    assert!(m.iter().all(|m| m.at == dj_marks::bar_sample(s, m.bar)));
    assert_eq!((m[0].out_track, m[0].in_track), (set.decks[0], set.decks[1]));
    assert!(dj_marks::marks(&Session::default()).is_empty());
}

#[test]
fn a_marker_tooltip_names_the_mix() {
    let set = set();
    let s = set.engine.session();
    let t = &s.dj.transitions;
    assert_eq!(dj_marks::hover_text(s, &t[0]), "Sunrise → Orbit · blend · 16-bar build · hit bar 33");
    assert_eq!(dj_marks::hover_text(s, &t[1]), "Orbit → Lift · quick · 8-bar build · hit bar 57");
    let out = dj_marks::marks(s).into_iter().find(|m| m.kind == MarkKind::MixOut).unwrap();
    assert_eq!(dj_marks::mark_text(s, &out), "Mix-out · bar 41\nSunrise → Orbit · blend · 16-bar build · hit bar 33");
}

#[test]
fn deck_tints_follow_the_deck_names_else_the_track_order() {
    let set = set();
    let s = set.engine.session();
    let d = Decks::of(s);
    let [a, b] = set.decks;
    let find = |c: ClipId| s.find_clip(c).unwrap();
    assert_eq!((d.track(a), d.track(b)), (Some(Deck::A), Some(Deck::B)));
    let tints: Vec<Option<Deck>> = set.clips.iter().map(|c| d.clip(find(*c).0, find(*c).1)).collect();
    assert_eq!(tints, [Some(Deck::A), Some(Deck::B), Some(Deck::A)]);
    // A loose (not beat-locked, not mixed) clip on a deck gets none.
    assert_eq!(d.clip(a, find(set.loose).1), None);
    // The vocal track holds a beat-locked clip, so it counts as a deck: the third, so A.
    assert_eq!((d.track(set.vocal), d.clip(set.vocal, find(set.vocal_clip).1)), (Some(Deck::A), Some(Deck::A)));

    // Named decks keep their letters when reordered; the unnamed one goes by its place.
    let mut s2 = s.clone();
    s2.tracks.swap(0, 2);
    let d2 = Decks::of(&s2);
    assert_eq!((d2.track(a), d2.track(b), d2.track(set.vocal)), (Some(Deck::A), Some(Deck::B), Some(Deck::A)));
    s2.tracks.swap(0, 1);
    assert_eq!(Decks::of(&s2).track(set.vocal), Some(Deck::B), "second among the decks");

    // Unnamed decks alternate A, B, A by track order.
    let mut s3 = Session::default();
    let ids: Vec<TrackId> = (0..3).map(|i| s3.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some(&format!("T{i}")))).collect();
    let midi = s3.add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
    for t in &ids {
        clip(&mut s3, *t, "x", 0, BAR, true);
    }
    let d3 = Decks::of(&s3);
    assert_eq!(ids.iter().map(|t| d3.track(*t)).collect::<Vec<_>>(), [Some(Deck::A), Some(Deck::B), Some(Deck::A)]);
    assert_eq!(d3.track(midi), None);

    // A session with no beat-locked clips and no transitions falls back to the names, but has
    // nothing to tint; other tracks are no decks.
    let mut s4 = Session::default();
    let (x, y, z) = (
        s4.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Deck B")),
        s4.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("deck a")),
        s4.add_track(TrackKind::Audio, ChannelFormat::Stereo, Some("Pads")),
    );
    let plain = clip(&mut s4, x, "p", 0, BAR, false);
    let d4 = Decks::of(&s4);
    assert_eq!((d4.track(x), d4.track(y), d4.track(z)), (Some(Deck::B), Some(Deck::A), None));
    assert_eq!(d4.clip(x, s4.find_clip(plain).unwrap().1), None);
    assert_eq!(Decks::of(&Session::default()), Decks::default());
}

struct Rig {
    ctx: egui::Context,
    app: SoundApp,
    shapes: Vec<ClippedShape>,
}

impl Rig {
    fn new(engine: Engine) -> Rig {
        let mut r = Rig { ctx: egui::Context::default(), app: SoundApp::new(engine, None, Services::default()), shapes: Vec::new() };
        // The palette is process-wide: say what this rig draws rather than depend on it.
        r.app.run("ui.waveform_bands", json!({"value": false})).unwrap();
        r.app.run("ui.deck_colours", json!({"value": true})).unwrap();
        r.frames(4);
        r
    }

    fn frame(&mut self, events: Vec<Event>) {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
        let mut out = self.ctx.run_ui(raw, |ui| {
            self.app.logic(ui.ctx());
            self.app.ui(ui);
        });
        out.textures_delta.clear();
        self.shapes = out.shapes;
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame(vec![]);
        }
    }

    /// Where the flag of `m` is drawn.
    fn flag(&self, m: &Mark) -> Pos2 {
        let tl = rect(self.app.edit_layout.timeline);
        pos2(x_of(self.app.engine.session(), tl, m.at), dj_marks::strip(tl).center().y)
    }

    fn click(&mut self, at: Pos2) {
        self.frame(vec![Event::PointerMoved(at)]);
        for pressed in [true, false] {
            self.frame(vec![Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]);
        }
        self.frames(2);
    }

    /// The filled shapes in `colour` outlined in the background colour: the marker flags.
    fn flags(&self, colour: egui::Color32) -> usize {
        self.shapes
            .iter()
            .filter(|c| matches!(&c.shape, Shape::Path(p) if p.fill == colour && p.stroke.color == egui::epaint::ColorMode::Solid(studio::BG)))
            .count()
    }

    /// The rectangles filled in `colour`.
    fn rects(&self, colour: egui::Color32) -> usize {
        self.shapes.iter().filter(|c| matches!(&c.shape, Shape::Rect(r) if r.fill == colour)).count()
    }
}

#[test]
fn the_edit_window_draws_one_flag_per_marker_and_tints_the_deck_clips() {
    let mut rig = Rig::new(set().engine);
    assert_eq!((rig.flags(studio::IN), rig.flags(studio::ACCENT), rig.flags(studio::OUT)), (2, 2, 2));
    // Sunrise, Lift and the beat-locked vocal hook are Deck A, Orbit Deck B; the loose clip
    // (off screen anyway) is not tinted.
    assert_eq!((rig.rects(studio::DECK_A), rig.rects(studio::DECK_B)), (3, 1));
    rig.app.run("ui.deck_colours", json!({"value": false})).unwrap();
    rig.frames(2);
    assert_eq!((rig.rects(studio::DECK_A), rig.rects(studio::DECK_B)), (0, 0), "the toggle turns them off");
    // Without transitions there are no markers.
    rig.app.engine.session_mut().dj.transitions.clear();
    rig.frames(2);
    assert_eq!((rig.flags(studio::IN), rig.flags(studio::ACCENT), rig.flags(studio::OUT)), (0, 0, 0));
}

#[test]
fn clicking_a_marker_selects_its_transition_and_locates_there() {
    let set = set();
    let lift = set.clips[2];
    let mut rig = Rig::new(set.engine);
    let marks = dj_marks::marks(rig.app.engine.session());
    let hit = marks.iter().find(|m| m.transition == 2 && m.kind == MarkKind::Hit).copied().unwrap();
    rig.click(rig.flag(&hit));
    let ed = &rig.app.engine.session().edit;
    assert_eq!(ed.selected_clips, [lift], "the transition into Lift is selected");
    assert_eq!(ed.selected_tracks, [set.decks[0]]);
    assert_eq!((ed.playhead, rig.app.position()), (hit.at, hit.at));
    // Clicking where bar 41's mix-out of the first transition is.
    let out = marks.iter().find(|m| m.transition == 1 && m.kind == MarkKind::MixOut).copied().unwrap();
    rig.click(rig.flag(&out));
    assert_eq!(rig.app.engine.session().edit.selected_clips, [set.clips[1]]);
    assert_eq!(rig.app.position(), out.at);
}
