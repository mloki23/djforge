//! Clip waveforms, drawn per physical pixel, or as a line through the samples at the sample level.
//!
//! Coloured by frequency ([`band_colour`]), each column mixes the low, mid and high bands the
//! overview kept for its span (`Peaks::build_with_bands`): red for kicks and bass, green for
//! vocals and leads, blue for hats. Drawing only reads those bands; nothing is filtered per frame.

use egui::{Color32, Mesh, Painter, Rect, Shape, Stroke, pos2, vec2};
use soundcraft_model::{Clip, Session, SourceAudio};
use soundcraft_time::Samples;

/// Draw a clip's audio across `body`, one band per channel, `channels` of them at most: in
/// `wave`, or with `by_frequency` coloured by frequency ([`band_colour`]) where the overview has
/// bands, `wave` elsewhere.
pub fn draw(
    painter: &Painter,
    s: &Session,
    tl: Rect,
    clip: &Clip,
    audio: &SourceAudio,
    channels: usize,
    body: Rect,
    wave: Color32,
    by_frequency: bool,
) {
    for (ch, band) in bands(body, channels.min(audio.peaks.len()).max(1)).enumerate() {
        channel(painter, s, tl, clip, audio, ch, band, wave, by_frequency);
    }
}

/// How much each band counts in [`band_colour`], by RMS: music has less energy the higher it
/// goes, so mids and highs are lifted (about +4 and +10 dB) to show up beside the bass.
const BAND_WEIGHT: [f32; 3] = [1.0, 1.6, 3.2];

/// The colour of a span whose low, mid and high bands have these RMS levels: the band colours
/// ([`crate::theme::studio`]'s LOW, MID and HIGH) mixed by the bands' weighted energy, then
/// brought back to the brightness of the colours mixed, so a mix never dulls to grey. `None`
/// for silence.
pub fn band_colour(rms: [f32; 3]) -> Option<Color32> {
    use crate::theme::studio::{HIGH, LOW, MID};
    let w = std::array::from_fn::<f32, 3, _>(|k| {
        let v = rms.get(k).copied().unwrap_or(0.0) * BAND_WEIGHT.get(k).copied().unwrap_or(1.0);
        if v.is_finite() { v * v } else { 0.0 }
    });
    let total: f32 = w.iter().sum();
    if !(total > 0.0 && total.is_finite()) {
        return None;
    }
    let colours = [LOW, MID, HIGH];
    let channel = |f: fn(Color32) -> u8| colours.iter().zip(&w).map(|(c, w)| f32::from(f(*c)) * w).sum::<f32>() / total;
    let mix = [channel(|c| c.r()), channel(|c| c.g()), channel(|c| c.b())];
    let peak = |c: Color32| f32::from(c.r().max(c.g()).max(c.b()));
    let bright = colours.iter().zip(&w).map(|(c, w)| peak(*c) * w).sum::<f32>() / total;
    let top = mix.iter().copied().fold(0.0f32, f32::max);
    let k = if top > 0.0 { bright / top } else { 1.0 };
    let [r, g, b] = mix.map(|v| (v * k).round().clamp(0.0, 255.0) as u8);
    Some(Color32::from_rgb(r, g, b))
}

fn bands(body: Rect, n: usize) -> impl Iterator<Item = Rect> {
    let h = body.height() / n as f32;
    (0..n).map(move |k| Rect::from_min_size(pos2(body.min.x, body.min.y + h * k as f32), vec2(body.width(), h)))
}

fn channel(painter: &Painter, s: &Session, tl: Rect, clip: &Clip, audio: &SourceAudio, ch: usize, band: Rect, wave: Color32, by_frequency: bool) {
    let (Some(samples), Some(peaks)) = (audio.channel(ch), audio.peaks.get(ch)) else { return };
    let by_frequency = by_frequency && peaks.has_bands();
    let Some(g) = Grid::new(painter, s, tl, band) else { return };
    let (rectified, power, outlines) = (s.edit.flag("waveform.rectified"), s.edit.flag("waveform.power"), s.edit.flag("waveform.outlines"));
    let origin = clip.start.saturating_sub(clip.source_offset()) as f64;
    let gain = |pos: f64| clip.gain_at((pos as Samples).saturating_sub(clip.start).min(clip.length).max(0)) * s.edit.zoom.waveform_zoom;
    if g.step <= 1.0 {
        let first = (g.pos_at(g.vis.min.x) - origin).floor().max(0.0) as usize;
        let last = (g.pos_at(g.vis.max.x) - origin).ceil() as usize;
        let from = first.saturating_sub(1);
        let points = samples.get(from..samples.len().min(last.saturating_add(2))).unwrap_or_default().iter().zip(from..).map(|(&v, i)| {
            let pos = origin + i as f64;
            pos2(g.x_at(pos), g.cy - level(v * gain(pos)) * g.half)
        });
        // One colour along the line: the bands of what is in view.
        let colour = if by_frequency { peaks.bands(first, last.saturating_add(1)).and_then(band_colour).unwrap_or(wave) } else { wave };
        painter.add(Shape::line(points.collect(), Stroke::new(g.weight, colour)));
        return;
    }
    let (start, overlap) = (g.start() - origin, g.overlap());
    let spans = if power {
        peaks.rms_spans(samples, start, g.step, overlap, g.cols).into_iter().map(|r| r.map(|r| (-r, r))).collect()
    } else {
        peaks.spans(samples, start, g.step, overlap, g.cols)
    };
    let base = g.cy + g.half;
    let ys: Vec<Option<(f32, f32)>> = spans
        .into_iter()
        .enumerate()
        .map(|(c, span)| {
            let (lo, hi) = span?;
            let k = gain((g.k0 + c as f64 + 0.5) * g.step);
            Some(if rectified {
                (base - level(lo.abs().max(hi.abs()) * k) * g.half * 2.0, base)
            } else {
                (g.cy - level(hi * k) * g.half, g.cy - level(lo * k) * g.half)
            })
        })
        .collect();
    let colours: Vec<Color32> = if by_frequency {
        peaks.band_spans(start, g.step, overlap, g.cols).into_iter().map(|b| b.and_then(band_colour).unwrap_or(wave)).collect()
    } else {
        Vec::new()
    };
    g.paint(painter, &ys, Colours { wave, each: &colours }, outlines, rectified);
}

/// Each column's colour, `wave` for columns `each` doesn't name.
#[derive(Clone, Copy)]
struct Colours<'a> {
    wave: Color32,
    each: &'a [Color32],
}

impl Colours<'_> {
    fn at(&self, c: usize) -> Color32 {
        self.each.get(c).copied().unwrap_or(self.wave)
    }
}

/// Draw a take while it records across `body`, one band per channel: `blocks` holds each channel's
/// (min, max) per block of input frames, the blocks `block_len` timeline samples long from
/// timeline position `origin`.
pub fn draw_live(painter: &Painter, s: &Session, tl: Rect, origin: f64, block_len: f64, blocks: &[Vec<(f32, f32)>], body: Rect, wave: Color32) {
    let zoom = s.edit.zoom.waveform_zoom;
    for (blocks, band) in blocks.iter().zip(bands(body, blocks.len())) {
        let Some(g) = Grid::new(painter, s, tl, band) else { continue };
        let ys: Vec<Option<(f32, f32)>> = (0..g.cols)
            .map(|c| {
                let a = g.start() + c as f64 * g.step - origin;
                let (i, j) = ((a / block_len).floor().max(0.0) as usize, ((a + g.step + g.overlap()) / block_len).ceil().max(0.0) as usize);
                let (lo, hi) =
                    blocks.get(i..j.min(blocks.len()))?.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), b| (lo.min(b.0), hi.max(b.1)));
                (lo <= hi).then(|| (g.cy - level(hi * zoom) * g.half, g.cy - level(lo * zoom) * g.half))
            })
            .collect();
        g.paint(painter, &ys, Colours { wave, each: &[] }, false, false);
    }
}

fn level(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
}

/// One column per physical pixel across the visible part of a channel's band, each summing up a
/// one-point window of timeline samples anchored to absolute positions, so scrolling doesn't
/// change what a column shows.
struct Grid {
    vis: Rect,
    tl_x: f32,
    scroll: f64,
    /// Timeline samples per point, and per column.
    spp: f64,
    step: f64,
    px: f32,
    /// A one-point line, in whole physical pixels.
    weight: f32,
    cy: f32,
    half: f32,
    /// The first column's physical pixel and absolute window index, and the column count.
    p0: f32,
    k0: f64,
    cols: usize,
}

impl Grid {
    fn new(painter: &Painter, s: &Session, tl: Rect, band: Rect) -> Option<Grid> {
        let vis = painter.clip_rect().intersect(band);
        if !vis.is_positive() {
            return None;
        }
        let ppp = painter.pixels_per_point();
        let px = 1.0 / ppp;
        let weight = ppp.round().max(1.0) * px;
        let spp = s.edit.zoom.samples_per_px.max(0.01);
        let step = spp * f64::from(px);
        let scroll = s.edit.zoom.scroll as f64;
        let p0 = (vis.min.x * ppp).floor();
        let k0 = ((scroll + f64::from((p0 + 0.5) * px - tl.min.x) * spp) / step).floor();
        let cy = ((band.center().y - weight * 0.5) * ppp).round() * px + weight * 0.5;
        let cols = ((vis.max.x * ppp).ceil() - p0).max(0.0) as usize;
        Some(Grid { vis, tl_x: tl.min.x, scroll, spp, step, px, weight, cy, half: band.height() * 0.46, p0, k0, cols })
    }

    fn pos_at(&self, x: f32) -> f64 {
        self.scroll + f64::from(x - self.tl_x) * self.spp
    }

    fn x_at(&self, pos: f64) -> f32 {
        self.tl_x + ((pos - self.scroll) / self.spp) as f32
    }

    /// How far each column's window reaches into its neighbours': a column is `step` apart but sums
    /// up a whole point.
    fn overlap(&self) -> f64 {
        (self.spp - self.step).max(0.0)
    }

    /// The timeline position where the first column's window starts.
    fn start(&self) -> f64 {
        self.k0 * self.step - self.overlap() * 0.5
    }

    /// Fill the columns' (top, bottom) spans in their colours; with `outlines`, edge them in those
    /// colours around a lighter fill, along the top only when `rectified`.
    fn paint(&self, painter: &Painter, ys: &[Option<(f32, f32)>], colours: Colours<'_>, outlines: bool, rectified: bool) {
        let mut mesh = Mesh::default();
        self.strip(&mut mesh, ys, colours, if outlines { 0.55 } else { 1.0 });
        let edges: &[fn((f32, f32)) -> f32] = match (outlines, rectified) {
            (false, _) => &[],
            (true, true) => &[|y| y.0],
            (true, false) => &[|y| y.0, |y| y.1],
        };
        for edge in edges {
            let line: Vec<Option<(f32, f32)>> = ys
                .iter()
                .enumerate()
                .map(|(c, y)| {
                    let y = edge((*y)?);
                    let near = |i: Option<usize>| i.and_then(|i| ys.get(i).copied().flatten()).map_or(y, |n| (edge(n) + y) * 0.5);
                    let (a, b) = (near(c.checked_sub(1)), near(Some(c + 1)));
                    Some((y.min(a).min(b) - self.weight * 0.5, y.max(a).max(b) + self.weight * 0.5))
                })
                .collect();
            self.strip(&mut mesh, &line, colours, 1.0);
        }
        painter.add(Shape::mesh(mesh));
    }

    /// Fill each column's (top, bottom) span, at least `weight` tall, in its colour times `fill`,
    /// joined to its neighbours in one triangle strip and anti-aliased at both ends. Runs of equal
    /// spans in one colour share vertices.
    fn strip(&self, mesh: &mut Mesh, spans: &[Option<(f32, f32)>], colours: Colours<'_>, fill: f32) {
        let half = self.px * 0.5;
        let thick = |(top, bot): (f32, f32)| {
            if bot - top < self.weight { ((top + bot - self.weight) * 0.5, (top + bot + self.weight) * 0.5) } else { (top, bot) }
        };
        let push = |mesh: &mut Mesh, prev: &mut Option<u32>, x: f32, ((top, bot), color): ((f32, f32), Color32)| {
            let i = mesh.vertices.len() as u32;
            mesh.colored_vertex(pos2(x, top - half), Color32::TRANSPARENT);
            mesh.colored_vertex(pos2(x, top + half), color);
            mesh.colored_vertex(pos2(x, bot - half), color);
            mesh.colored_vertex(pos2(x, bot + half), Color32::TRANSPARENT);
            if let Some(j) = *prev {
                for k in 0..3 {
                    mesh.add_triangle(j + k, j + k + 1, i + k);
                    mesh.add_triangle(j + k + 1, i + k + 1, i + k);
                }
            }
            *prev = Some(i);
        };
        let tint = |c: usize| if fill < 1.0 { colours.at(c).gamma_multiply(fill) } else { colours.at(c) };
        let at = |c: Option<usize>| c.and_then(|c| spans.get(c).copied().flatten().map(|s| (thick(s), tint(c))));
        let mut prev = None;
        for c in 0..spans.len() {
            let Some(span) = at(Some(c)) else {
                prev = None;
                continue;
            };
            let x = (self.p0 + c as f32 + 0.5) * self.px;
            let (before, after) = (at(c.checked_sub(1)), at(Some(c + 1)));
            if before.is_none() {
                push(mesh, &mut prev, x - half, span);
            }
            if before != Some(span) || after != Some(span) {
                push(mesh, &mut prev, x, span);
            }
            if after.is_none() {
                push(mesh, &mut prev, x + half, span);
            }
        }
    }
}
