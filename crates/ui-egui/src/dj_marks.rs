//! The DJ set in the Edit window: deck tints on the decks' clips, and each transition's mix-in,
//! bass-hit and mix-out markers.
//!
//! **Decks.** The deck tracks are those `prod.*` treats as decks: the tracks the transitions mix
//! between and the audio tracks holding beat-locked clips, or failing both, the audio tracks named
//! "Deck A" / "Deck B". A deck track named "Deck A" or "Deck B" (as `dj.import_set` names them)
//! is that deck, so reordering them keeps their colours; any other deck track takes A or B by its
//! place among the deck tracks in track order (first A, second B, third A…), so a set of two
//! unnamed decks is A then B. A clip is tinted when it is an audio clip on a deck track that is
//! beat-locked or mixed by a transition.
//!
//! **Markers.** Each transition shows where the incoming track enters (mix-in, `entry_bar`),
//! where its bass hits in full (`hit_bar`) and where the outgoing bass is gone (mix-out,
//! `gone_bar` = hit + `out_bars`: the end of the recipe's bass swap, after which the outgoing
//! track no longer carries the mix). Positions are bar starts on the tempo map, as the recipe
//! places its automation; the nudge moves the audio, not the bars.

use crate::SoundApp;
use crate::edit_window::{sample_at, x_of};
use crate::theme::studio;
use egui::{Color32, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};
use serde_json::json;
use soundcraft_model::{Clip, ClipId, Session, TrackId, TrackKind, Transition, TransitionStyle};
use soundcraft_time::Samples;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A DJ deck.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Deck {
    A,
    B,
}

impl Deck {
    /// The deck's tint ([`studio::DECK_A`], [`studio::DECK_B`]).
    pub const fn colour(self) -> Color32 {
        match self {
            Deck::A => studio::DECK_A,
            Deck::B => studio::DECK_B,
        }
    }

    /// The deck a track named "Deck A" / "Deck B" is, ignoring case and outer spaces.
    fn named(name: &str) -> Option<Deck> {
        let n = name.trim();
        if n.eq_ignore_ascii_case("deck a") {
            Some(Deck::A)
        } else if n.eq_ignore_ascii_case("deck b") {
            Some(Deck::B)
        } else {
            None
        }
    }
}

/// Which deck each deck track is, and the clips the transitions mix.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decks {
    tracks: HashMap<TrackId, Deck>,
    mixed: HashSet<ClipId>,
}

impl Decks {
    /// The decks of session `s` (see the module docs).
    pub fn of(s: &Session) -> Decks {
        let mixed_tracks: HashSet<TrackId> = s.dj.transitions.iter().flat_map(|t| [t.out_track, t.in_track]).collect();
        let mut decks: Vec<(TrackId, &str)> = s
            .tracks
            .iter()
            .filter(|t| mixed_tracks.contains(&t.id) || (t.kind == TrackKind::Audio && t.clips().iter().any(|c| c.warp.is_some())))
            .map(|t| (t.id, t.name.as_str()))
            .collect();
        if decks.is_empty() {
            decks =
                s.tracks.iter().filter(|t| t.kind == TrackKind::Audio && Deck::named(&t.name).is_some()).map(|t| (t.id, t.name.as_str())).collect();
        }
        let by_place = |i: usize| if i.is_multiple_of(2) { Deck::A } else { Deck::B };
        let tracks = decks.iter().enumerate().map(|(i, (id, name))| (*id, Deck::named(name).unwrap_or_else(|| by_place(i)))).collect();
        let mixed = s.dj.transitions.iter().flat_map(|t| [t.out_clip, t.in_clip]).collect();
        Decks { tracks, mixed }
    }

    /// The deck track `id` is, if it is one.
    pub fn track(&self, id: TrackId) -> Option<Deck> {
        self.tracks.get(&id).copied()
    }

    /// The deck tint of `clip` on `track`: an audio clip on a deck track, beat-locked or mixed by
    /// a transition.
    pub fn clip(&self, track: TrackId, clip: &Clip) -> Option<Deck> {
        let deck = self.track(track)?;
        (clip.is_audio() && (clip.warp.is_some() || self.mixed.contains(&clip.id))).then_some(deck)
    }
}

/// One of a transition's three markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkKind {
    /// The incoming track enters: `entry_bar`.
    MixIn,
    /// The incoming bass hits in full: `hit_bar()`.
    Hit,
    /// The outgoing bass is gone: `gone_bar()`.
    MixOut,
}

impl MarkKind {
    pub const fn colour(self) -> Color32 {
        match self {
            MarkKind::MixIn => studio::IN,
            MarkKind::Hit => studio::ACCENT,
            MarkKind::MixOut => studio::OUT,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            MarkKind::MixIn => "Mix-in",
            MarkKind::Hit => "Bass hit",
            MarkKind::MixOut => "Mix-out",
        }
    }

    /// Which of several markers on the same spot a click takes: a mix-in (the next transition)
    /// over a hit over a mix-out (the previous one ending there).
    const fn rank(self) -> u8 {
        match self {
            MarkKind::MixIn => 0,
            MarkKind::Hit => 1,
            MarkKind::MixOut => 2,
        }
    }
}

/// A transition marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    pub transition: u64,
    pub kind: MarkKind,
    pub bar: i64,
    /// Timeline sample of the start of `bar`.
    pub at: Samples,
    pub out_track: TrackId,
    pub in_track: TrackId,
}

/// Timeline sample of the start of `bar` (1-based), as the DJ recipe places it.
pub fn bar_sample(s: &Session, bar: i64) -> Samples {
    s.tempo.tick_to_samples(s.tempo.bar_start_tick(bar), s.sample_rate)
}

/// Every transition's markers, earliest first (a mix-in before a mix-out on the same sample).
pub fn marks(s: &Session) -> Vec<Mark> {
    let mut v: Vec<Mark> =
        s.dj.transitions
            .iter()
            .flat_map(|t| {
                [(MarkKind::MixIn, t.entry_bar), (MarkKind::Hit, t.hit_bar()), (MarkKind::MixOut, t.gone_bar())].map(|(kind, bar)| Mark {
                    transition: t.id,
                    kind,
                    bar,
                    at: bar_sample(s, bar),
                    out_track: t.out_track,
                    in_track: t.in_track,
                })
            })
            .collect();
    v.sort_by_key(|m| (m.at, m.kind.rank(), m.transition));
    v
}

/// The name of clip `id`, wherever it is.
fn clip_title(s: &Session, id: ClipId) -> String {
    s.tracks
        .iter()
        .flat_map(|t| t.playlists.iter().flat_map(|p| &p.clips))
        .find(|c| c.id == id)
        .map_or_else(|| format!("clip {}", id.0), |c| c.name.clone())
}

/// "Out title → In title · style · N-bar build · hit bar X".
pub fn hover_text(s: &Session, t: &Transition) -> String {
    let style = match t.style {
        TransitionStyle::Blend => "blend",
        TransitionStyle::Quick => "quick",
    };
    format!("{} → {} · {style} · {}-bar build · hit bar {}", clip_title(s, t.out_clip), clip_title(s, t.in_clip), t.build_bars, t.hit_bar())
}

/// A marker's tooltip: which marker, then the transition.
pub fn mark_text(s: &Session, m: &Mark) -> String {
    let what = s.dj.transition(m.transition).map(|t| hover_text(s, t)).unwrap_or_default();
    format!("{} · bar {}\n{what}", m.kind.label(), m.bar)
}

/// How near (points) the pointer must be to a marker to pick it.
pub const PICK_PX: f32 = 6.0;
/// Height of the marker strip at the foot of the rulers.
pub const FLAG_H: f32 = 9.0;

/// The marker under `x` among `marks` drawn at `xs` (the same order): the nearest within
/// [`PICK_PX`], a mix-in winning a tie.
pub fn mark_at(marks: &[Mark], xs: impl Fn(&Mark) -> f32, x: f32) -> Option<&Mark> {
    marks
        .iter()
        .map(|m| ((xs(m) - x).abs(), m))
        .filter(|(d, _)| *d <= PICK_PX)
        .min_by(|(da, a), (db, b)| da.round().total_cmp(&db.round()).then(a.kind.rank().cmp(&b.kind.rank())))
        .map(|(_, m)| m)
}

/// Selects the transition of `m` (its incoming clip, which the DJ commands take as the
/// selected transition, with its track) and locates the playhead and insertion on the marker.
pub fn pick(app: &mut SoundApp, m: &Mark) {
    let Some(t) = app.engine.session().dj.transition(m.transition) else { return };
    let (clip, at) = (t.in_clip, m.at.max(0));
    let _ = app.run("edit.select", json!({"clips": [clip.0], "start": at}));
    let _ = app.run("transport.locate", json!({"at": at}));
}

/// The decks and markers of one revision of the session.
#[derive(Debug, Default)]
pub struct DjView {
    pub decks: Decks,
    pub marks: Vec<Mark>,
}

/// The [`DjView`] of `s`, worked out once per revision.
pub fn view(ctx: &egui::Context, s: &Session, revision: u64) -> Arc<DjView> {
    let id = egui::Id::new("dj_view");
    let key = (revision, s.sample_rate.hz());
    if let Some((k, v)) = ctx.data(|d| d.get_temp::<((u64, u32), Arc<DjView>)>(id))
        && k == key
    {
        return v;
    }
    let v = Arc::new(DjView { decks: Decks::of(s), marks: marks(s) });
    ctx.data_mut(|d| d.insert_temp(id, (key, Arc::clone(&v))));
    v
}

/// The marker strip: the foot of the rulers, over the timeline.
pub fn strip(tl: Rect) -> Rect {
    Rect::from_min_max(pos2(tl.min.x, tl.min.y - FLAG_H - 1.0), pos2(tl.max.x, tl.min.y))
}

/// Draws the transition markers (flags in the strip at the foot of the rulers and faint lines
/// through the two decks' lanes) and handles hovering and clicking them. Draws nothing without
/// transitions; only the markers in view are drawn.
pub fn draw(app: &mut SoundApp, ui: &mut Ui, tl: Rect) {
    let s = app.engine.session_arc();
    if s.dj.is_empty() {
        return;
    }
    let v = view(ui.ctx(), &s, app.engine.revision);
    let strip = strip(tl);
    let (from, to) = (sample_at(&s, tl, tl.min.x - PICK_PX * 2.0), sample_at(&s, tl, tl.max.x + PICK_PX * 2.0));
    let first = v.marks.partition_point(|m| m.at < from);
    let shown: &[Mark] = v.marks.get(first..).unwrap_or_default();
    let shown = shown.get(..shown.partition_point(|m| m.at <= to)).unwrap_or_default();
    if shown.is_empty() {
        return;
    }
    let rows: HashMap<u64, [f32; 4]> = app.edit_layout.rows.iter().copied().collect();
    let flags = ui.painter().with_clip_rect(strip.expand2(vec2(0.0, 1.0)));
    let lines = ui.painter().with_clip_rect(tl);
    let halo = Stroke::new(3.0, Color32::from_black_alpha(110));
    for m in shown {
        let x = x_of(&s, tl, m.at).round() + 0.5;
        let col = m.kind.colour();
        for track in [m.out_track, m.in_track] {
            if let Some(r) = rows.get(&track.0) {
                let seg = [pos2(x, r[1].max(tl.min.y)), pos2(x, r[3])];
                lines.line_segment(seg, halo);
                lines.line_segment(seg, Stroke::new(1.0, col.gamma_multiply(0.7)));
            }
        }
        flags.add(flag(m.kind, x, strip));
    }
    // One hover target at a time: the marker under the pointer.
    let Some(p) = ui.input(|i| i.pointer.hover_pos()) else { return };
    if !strip.expand2(vec2(PICK_PX, 2.0)).contains(p) {
        return;
    }
    let Some(m) = mark_at(shown, |m| x_of(&s, tl, m.at), p.x).copied() else { return };
    let x = x_of(&s, tl, m.at);
    let target = Rect::from_min_max(pos2(x - PICK_PX, strip.min.y - 2.0), pos2(x + PICK_PX, strip.max.y + 2.0));
    let resp = ui.interact(target, ui.id().with(("dj_mark", m.transition, m.kind)), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(mark_text(&s, &m));
    if resp.clicked() {
        pick(app, &m);
    }
}

/// A marker's flag at `x`: a pennant pointing into the mix (right) for a mix-in, out of it
/// (left) for a mix-out, a diamond for the hit; outlined so it reads on any ruler.
fn flag(kind: MarkKind, x: f32, strip: Rect) -> Shape {
    let (top, bottom) = (strip.min.y + 0.5, strip.max.y - 0.5);
    let mid = (top + bottom) / 2.0;
    let outline = Stroke::new(1.0, studio::BG);
    let pts = match kind {
        MarkKind::MixIn => vec![pos2(x - 1.0, top), pos2(x + 6.0, mid), pos2(x - 1.0, bottom)],
        MarkKind::MixOut => vec![pos2(x + 1.0, top), pos2(x + 1.0, bottom), pos2(x - 6.0, mid)],
        MarkKind::Hit => vec![pos2(x, top), pos2(x + 4.5, mid), pos2(x, bottom), pos2(x - 4.5, mid)],
    };
    Shape::convex_polygon(pts, kind.colour(), outline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_is_picked_by_the_nearest_flag_and_a_mix_in_wins_a_tie() {
        let m = |transition, kind, at| Mark { transition, kind, bar: 0, at, out_track: TrackId(1), in_track: TrackId(2) };
        let marks = [m(1, MarkKind::MixOut, 100), m(2, MarkKind::MixIn, 100), m(2, MarkKind::Hit, 130)];
        let xs = |m: &Mark| m.at as f32;
        assert_eq!(mark_at(&marks, xs, 101.0).map(|m| (m.transition, m.kind)), Some((2, MarkKind::MixIn)));
        assert_eq!(mark_at(&marks, xs, 127.0).map(|m| m.kind), Some(MarkKind::Hit));
        assert_eq!(mark_at(&marks, xs, 115.0), None);
        assert_eq!(mark_at(&[], xs, 0.0), None);
    }
}
