//! A rotary knob for the DJ mixer (trim, EQ, filter, master): drag up or right to turn it,
//! Shift for fine steps, double-click to reset, and an optional centre detent. Its travel is
//! 0..1; the caller maps it to what it controls (dB, filter amount), so one knob serves every
//! control and, in 9b, MIDI mapping (the caller adds Learn MIDI to its right-click menu).

use crate::theme::Tokens;
use crate::widgets::{rounded_vgradient, shade};
use egui::{Color32, Rect, Response, Sense, Stroke, Ui, Vec2, vec2};

/// Points of drag for the whole travel.
const THROW_PX: f32 = 180.0;
/// Fine drag (Shift): this much slower.
const FINE: f32 = 8.0;
/// Within this much of a detent the knob rests on it.
const DETENT: f32 = 0.025;

/// How a knob behaves and reads.
#[derive(Debug, Clone, Copy)]
pub struct Knob<'a> {
    /// Its name (accessibility, tooltips): "Deck A EQ High".
    pub label: &'a str,
    /// Travel a double-click returns to.
    pub default: f32,
    /// Travel it rests on when turned close (the filter's off, an EQ's 0 dB).
    pub detent: Option<f32>,
    /// The arc lights from here (0.5 for a bipolar knob, 0 for a level).
    pub origin: f32,
    /// The arc's colour.
    pub colour: Color32,
}

/// Draws the knob in a `size`-point square and turns `travel` (0..1) by the drag. The response
/// is `changed()` when the travel moved.
pub fn knob(ui: &mut Ui, size: f32, travel: &mut f32, k: Knob<'_>) -> Response {
    let (r, mut resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    let before = *travel;
    if resp.dragged() {
        let d = resp.drag_delta();
        let fine = if ui.input(|i| i.modifiers.shift) { FINE } else { 1.0 };
        let mut v = (*travel + (d.x - d.y) / THROW_PX / fine).clamp(0.0, 1.0);
        if let Some(c) = k.detent
            && (v - c).abs() < DETENT
            && fine == 1.0
        {
            v = c;
        }
        *travel = v;
    }
    if resp.double_clicked() {
        *travel = k.default;
    }
    if !travel.is_finite() {
        *travel = k.default;
    }
    if *travel != before {
        resp.mark_changed();
    }
    paint(ui, r, *travel, &k, resp.hovered() || resp.dragged());
    let label = k.label.to_string();
    let value = f64::from(*travel);
    resp.widget_info(|| {
        let mut info = egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, &label);
        info.value = Some(value);
        info
    });
    resp
}

fn paint(ui: &Ui, r: Rect, travel: f32, k: &Knob<'_>, hot: bool) {
    let t = Tokens::current();
    let p = ui.painter();
    let c = r.center();
    let rad = r.width() * 0.36;
    let start = std::f32::consts::PI * 0.75;
    let sweep = std::f32::consts::PI * 1.5;
    let arc = |a: f32, b: f32| -> Vec<egui::Pos2> {
        let n = 20;
        (0..=n).map(|i| a + (b - a) * i as f32 / n as f32).map(|x| c + vec2(x.cos(), x.sin()) * (rad + 3.0)).collect()
    };
    let at = start + sweep * travel.clamp(0.0, 1.0);
    let from = start + sweep * k.origin.clamp(0.0, 1.0);
    p.add(egui::Shape::line(arc(start, start + sweep), Stroke::new(2.0, t.meter_bg)));
    if (at - from).abs() > 0.01 {
        p.add(egui::Shape::line(arc(from.min(at), from.max(at)), Stroke::new(2.0, k.colour)));
    }
    p.circle_filled(c + vec2(0.0, 1.0), rad, Color32::from_black_alpha(90));
    let face = if hot { shade(t.knob_bg, 0.08) } else { t.knob_bg };
    rounded_vgradient(p, Rect::from_center_size(c, Vec2::splat(2.0 * rad)), 2.0 * rad, shade(face, 0.14), shade(face, -0.2));
    p.circle_stroke(c, rad - 0.75, Stroke::new(1.5, t.knob_ring));
    let d = vec2(at.cos(), at.sin());
    p.line_segment([c + d * rad * 0.15, c + d * (rad - 2.0)], Stroke::new(2.0, t.knob_pointer));
}

/// An EQ knob's dB at `travel`: −26 dB at the left, 0 dB in the centre, +6 dB at the right.
pub fn eq_db(travel: f32) -> f32 {
    bipolar_db(travel, 26.0, 6.0)
}

/// The travel of an EQ knob at `db` (a kill sits at the left end).
pub fn eq_travel(db: f32) -> f32 {
    bipolar_travel(db, 26.0, 6.0)
}

/// A trim knob's dB at `travel`: −24 dB to +12 dB, 0 dB in the centre.
pub fn trim_db(travel: f32) -> f32 {
    bipolar_db(travel, 24.0, 12.0)
}

pub fn trim_travel(db: f32) -> f32 {
    bipolar_travel(db, 24.0, 12.0)
}

fn bipolar_db(travel: f32, cut: f32, boost: f32) -> f32 {
    let p = (travel.clamp(0.0, 1.0) - 0.5) * 2.0;
    if p < 0.0 { p * cut } else { p * boost }
}

fn bipolar_travel(db: f32, cut: f32, boost: f32) -> f32 {
    if !db.is_finite() {
        return 0.5;
    }
    let p = if db < 0.0 { (db / cut).max(-1.0) } else { (db / boost).min(1.0) };
    p * 0.5 + 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_and_trim_knobs_map_both_ways_with_zero_in_the_centre() {
        assert_eq!(eq_db(0.5), 0.0);
        assert_eq!(eq_db(0.0), -26.0);
        assert_eq!(eq_db(1.0), 6.0);
        assert_eq!(trim_db(1.0), 12.0);
        for db in [-26.0, -10.0, 0.0, 3.0, 6.0] {
            assert!((eq_db(eq_travel(db)) - db).abs() < 1e-4, "{db}");
        }
        assert_eq!(eq_travel(-80.0), 0.0, "a kill rests at the left end");
        assert_eq!(eq_travel(f32::NAN), 0.5);
    }
}
