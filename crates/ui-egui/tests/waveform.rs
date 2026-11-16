use egui::{Color32, Pos2, RawInput, Rect, Shape, vec2};
use serde_json::json;
use soundcraft_audio_io::AudioBuffer;
use soundcraft_model::{Clip, ClipId, Session, SourceAudio, SourceId};
use soundcraft_time::SampleRate;
use soundcraft_ui_egui::{Services, SoundApp, waveform};

const WAVE: Color32 = Color32::from_rgb(12, 12, 14);

/// The lines and meshes `f` paints across a 400×100 band on a 2x screen.
fn paint(samples_per_px: f64, f: impl Fn(&egui::Painter, &Session, Rect)) -> Vec<Shape> {
    let mut s = Session::new("t", SampleRate::HZ_48000);
    s.edit.zoom.samples_per_px = samples_per_px;
    let band = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 100.0));
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(2.0);
    let mut out = ctx.run_ui(RawInput { screen_rect: Some(band), ..Default::default() }, |ui| f(ui.painter(), &s, band));
    out.textures_delta.clear();
    out.shapes.into_iter().map(|c| c.shape).filter(|s| matches!(s, Shape::Path(_) | Shape::Mesh(_))).collect()
}

fn draw(samples: Vec<f32>, samples_per_px: f64) -> Vec<Shape> {
    draw_coloured(samples, samples_per_px, false)
}

fn draw_coloured(samples: Vec<f32>, samples_per_px: f64, by_frequency: bool) -> Vec<Shape> {
    let clip = Clip::audio(ClipId(1), "c", SourceId(1), 0, 0, samples.len() as i64);
    let audio = SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![samples] });
    paint(samples_per_px, |p, s, band| waveform::draw(p, s, band, &clip, &audio, 1, band, WAVE, by_frequency))
}

fn sine(hz: f32, n: usize) -> Vec<f32> {
    (0..n).map(|i| (i as f32 * std::f32::consts::TAU * hz / 48_000.0).sin() * 0.5).collect()
}

/// The colours of a mesh's vertices that aren't transparent, in `[x0, x1)` and from `x2` on.
fn colours_outside(mesh: &egui::Mesh, x0: f32, x1: f32, x2: f32) -> (Vec<Color32>, Vec<Color32>) {
    let seen = |f: &dyn Fn(f32) -> bool| mesh.vertices.iter().filter(|v| v.color.a() > 0 && f(v.pos.x)).map(|v| v.color).collect();
    (seen(&|x| x >= x0 && x < x1), seen(&|x| x >= x2))
}

#[test]
fn pure_bands_take_the_band_colours() {
    use soundcraft_ui_egui::theme::studio::{HIGH, LOW, MID};
    assert_eq!(waveform::band_colour([0.5, 0.0, 0.0]), Some(LOW));
    assert_eq!(waveform::band_colour([0.0, 0.01, 0.0]), Some(MID));
    assert_eq!(waveform::band_colour([0.0, 0.0, 1.0]), Some(HIGH));
    assert_eq!(waveform::band_colour([0.0; 3]), None);
    assert_eq!(waveform::band_colour([f32::NAN, f32::INFINITY, 0.0]), None);
    // A mix sits between its bands and keeps their brightness: bass and hats make a violet.
    let mix = waveform::band_colour([0.3, 0.0, 0.3 / 3.2]).unwrap_or_default();
    assert!(mix.r() > 0xa0 && mix.b() > 0xa0 && mix.g() < mix.r().min(mix.b()), "{mix:?}");
    let dull = waveform::band_colour([0.3, 0.3 / 1.6, 0.3 / 3.2]).unwrap_or_default();
    // All three: a pale blue-grey as bright as the band colours (their brightest channels are
    // 0xc4 to 0xf7), not a dull grey.
    assert!(dull.r().max(dull.g()).max(dull.b()) >= 0xc4, "{dull:?}");
}

#[test]
fn coloured_by_frequency_kicks_are_red_and_hats_blue() {
    use soundcraft_ui_egui::theme::studio::{HIGH, LOW};
    // 1 s of 60 Hz, then 1 s of 8 kHz, at 240 samples per point: the switch at x = 200.
    let mut x = sine(60.0, 48_000);
    x.extend(sine(8000.0, 48_000));
    for (power, outlines) in [(false, false), (true, false), (false, true)] {
        let mut s = Session::new("t", SampleRate::HZ_48000);
        s.edit.zoom.samples_per_px = 240.0;
        s.edit.set_flag("waveform.power", power);
        s.edit.set_flag("waveform.outlines", outlines);
        let clip = Clip::audio(ClipId(1), "c", SourceId(1), 0, 0, x.len() as i64);
        let audio = SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![x.clone()] });
        let band = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 100.0));
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(2.0);
        let mut out = ctx.run_ui(RawInput { screen_rect: Some(band), ..Default::default() }, |ui| {
            waveform::draw(ui.painter(), &s, band, &clip, &audio, 1, band, WAVE, true);
        });
        out.textures_delta.clear();
        let meshes: Vec<_> = out.shapes.into_iter().filter_map(|c| if let Shape::Mesh(m) = c.shape { Some(m) } else { None }).collect();
        let [mesh] = meshes.as_slice() else { panic!("{meshes:?}") };
        // The first columns hold the filters' start.
        let (left, right) = colours_outside(mesh, 8.0, 190.0, 210.0);
        let (low, high) = if outlines { (LOW.gamma_multiply(0.55), HIGH.gamma_multiply(0.55)) } else { (LOW, HIGH) };
        // Within a few levels of the band colour: the filters' slopes leak a little of the other
        // bands.
        let near = |a: Color32, b: Color32| a.to_array().iter().zip(b.to_array()).all(|(x, y)| x.abs_diff(y) <= 6);
        let fill = |cs: &[Color32], full: Color32, lighter: Color32| cs.iter().all(|&c| near(c, full) || near(c, lighter));
        assert!(!left.is_empty() && fill(&left, LOW, low), "power {power}, outlines {outlines}: {:?}", &left[..left.len().min(8)]);
        assert!(!right.is_empty() && fill(&right, HIGH, high), "power {power}, outlines {outlines}: {:?}", &right[..right.len().min(8)]);
    }
    // Off, or zoomed to the samples where the line takes the colour of what's in view.
    let x = sine(60.0, 48_000);
    let clip = Clip::audio(ClipId(1), "c", SourceId(1), 0, 0, x.len() as i64);
    let audio = SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![x] });
    let shapes = paint(0.25, |p, s, band| {
        let mut s = s.clone();
        s.edit.zoom.scroll = 24_000;
        waveform::draw(p, &s, band, &clip, &audio, 1, band, WAVE, true);
    });
    let [Shape::Path(line)] = shapes.as_slice() else { panic!("{shapes:?}") };
    let egui::epaint::ColorMode::Solid(c) = line.stroke.color else { panic!("{:?}", line.stroke) };
    assert!(c.to_array().iter().zip(LOW.to_array()).all(|(x, y)| x.abs_diff(y) <= 6), "{c:?}");
    let shapes = draw_coloured(sine(60.0, 48_000), 240.0, false);
    let [Shape::Mesh(mesh)] = shapes.as_slice() else { panic!("{shapes:?}") };
    assert!(mesh.vertices.iter().all(|v| v.color == WAVE || v.color == Color32::TRANSPARENT));
}

#[test]
fn audio_without_bands_and_live_takes_stay_one_colour() {
    let clip = Clip::audio(ClipId(1), "c", SourceId(1), 0, 0, 48_000);
    // A rate too low for bands: the overview has none, so the waveform keeps its colour.
    let audio = SourceAudio::new(AudioBuffer { sample_rate: 4_000, channels: vec![sine(60.0, 48_000)] });
    let shapes = paint(240.0, |p, s, band| waveform::draw(p, s, band, &clip, &audio, 1, band, WAVE, true));
    let [Shape::Mesh(mesh)] = shapes.as_slice() else { panic!("{shapes:?}") };
    assert!(mesh.vertices.iter().all(|v| v.color == WAVE || v.color == Color32::TRANSPARENT));
    let blocks = vec![vec![(-0.5, 0.5); 100]];
    let shapes = paint(128.0, |p, s, band| waveform::draw_live(p, s, band, 1280.0, 128.0, &blocks, band, WAVE));
    let [Shape::Mesh(mesh)] = shapes.as_slice() else { panic!("{shapes:?}") };
    assert!(mesh.vertices.iter().all(|v| v.color == WAVE || v.color == Color32::TRANSPARENT));
}

#[test]
fn a_take_draws_its_overview_while_it_records() {
    let blocks = vec![vec![(-0.5, 0.5); 100]];
    let shapes = paint(128.0, |p, s, band| waveform::draw_live(p, s, band, 1280.0, 128.0, &blocks, band, WAVE));
    let [Shape::Mesh(mesh)] = shapes.as_slice() else { panic!("{shapes:?}") };
    let span = |f: fn(&Pos2) -> f32| mesh.vertices.iter().map(|v| f(&v.pos)).fold((f32::MAX, f32::MIN), |(a, b), v| (a.min(v), b.max(v)));
    let ((left, right), (top, bottom)) = (span(|p| p.x), span(|p| p.y));
    assert!((left - 10.0).abs() <= 1.0 && (right - 110.0).abs() <= 1.0, "drawn from {left} to {right}");
    assert!((top - 27.0).abs() <= 1.0 && (bottom - 73.0).abs() <= 1.0, "drawn from {top} to {bottom}");
}

#[test]
fn at_the_sample_level_the_waveform_is_a_line_through_the_samples() {
    let x: Vec<f32> = (0..200).map(|i| (i as f32 * 0.3).sin() * 0.8).collect();
    let shapes = draw(x.clone(), 0.25);
    let [Shape::Path(line)] = shapes.as_slice() else { panic!("{shapes:?}") };
    assert!(line.points.len() > 100);
    for p in &line.points {
        let i = (p.x / 4.0).round() as usize;
        assert!((p.x - i as f32 * 4.0).abs() < 1e-3 && (p.y - (50.0 - x[i] * 46.0)).abs() < 1e-3, "{p:?}");
    }
}

#[test]
fn zoomed_out_a_one_sample_spike_stays_a_point_wide() {
    let mut x = vec![0.0f32; 4800];
    x[1600] = 1.0;
    let shapes = draw(x, 8.0);
    let [Shape::Mesh(mesh)] = shapes.as_slice() else { panic!("{shapes:?}") };
    let tall: Vec<f32> = mesh.vertices.iter().filter(|v| v.pos.y < 40.0).map(|v| v.pos.x).collect();
    let (left, right) = tall.iter().fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    assert!(right - left <= 1.0 && (199.0..=201.0).contains(&left), "spike drawn from {left} to {right}");
    assert!(mesh.vertices.iter().any(|v| v.pos.y < 5.0 && v.color == WAVE));
}

/// Waveform meshes drawn in a track's row, from `below` its top, once the app has settled.
fn waveforms(ctx: &egui::Context, app: &mut SoundApp, name: &str, below: f32) -> usize {
    let mut shapes = Vec::new();
    for _ in 0..2 {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.textures_delta.clear();
        shapes = out.shapes;
    }
    let id = app.engine.session().track_by_name(name).unwrap().id.0;
    let row = app.edit_layout.rows.iter().find(|(i, _)| *i == id).unwrap().1;
    let [x0, _, x1, _] = app.edit_layout.timeline;
    let lane = Rect::from_min_max(egui::pos2(x0, row[1] + below), egui::pos2(x1, row[3]));
    shapes.iter().filter(|c| matches!(&c.shape, Shape::Mesh(m) if m.vertices.iter().any(|v| lane.contains(v.pos)))).count()
}

#[test]
fn blocks_hide_waveforms_and_playlist_lanes_show_them() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.engine.execute("track.playlist_duplicate", &json!({"track": "Bass"})).unwrap();
    app.engine.execute("track.view", &json!({"tracks": ["Bass"], "view": "playlists"})).unwrap();
    app.engine.execute("track.view", &json!({"tracks": ["Snare"], "view": "blocks"})).unwrap();
    let main = app.engine.session().track_by_name("Bass").unwrap().height.points();
    let ctx = egui::Context::default();
    assert!(waveforms(&ctx, &mut app, "Kick", 0.0) > 0);
    assert_eq!(waveforms(&ctx, &mut app, "Snare", 0.0), 0);
    assert!(waveforms(&ctx, &mut app, "Bass", main) > 0, "the alternate playlist's lane");
}

#[test]
fn overlapped_crossfades_show_the_outgoing_clip_too() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.engine.execute("edit.fades_create", &json!({"tracks": ["Bass"], "start": {"seconds": 18.2}, "end": {"seconds": 18.7}})).unwrap();
    app.engine.execute("view.zoom_set", &json!({"samples_per_px": 64})).unwrap();
    app.engine.execute("view.scroll", &json!({"to": {"seconds": 18.0}})).unwrap();
    let ctx = egui::Context::default();
    let plain = waveforms(&ctx, &mut app, "Bass", 0.0);
    app.engine.execute("view.waveform_overlapped_crossfades", &json!({"value": true})).unwrap();
    assert_eq!(waveforms(&ctx, &mut app, "Bass", 0.0), plain + 1);
}

/// Draw time for a 10-minute stereo clip, zoomed out to fit and zoomed in, in the dev profile:
/// `cargo test -p soundcraft-ui-egui --test waveform -- --ignored --nocapture draw_time`.
#[test]
#[ignore = "timing harness"]
fn draw_time_of_a_long_clip() {
    let n = 48_000 * 600;
    let x: Vec<f32> = (0..n).map(|i| ((i as f32 * 0.0079).sin() * 0.5 + (i as f32 * 0.37).sin() * 0.2) * ((i / 24_000) % 2) as f32).collect();
    let clip = Clip::audio(ClipId(1), "c", SourceId(1), 0, 0, n as i64);
    let audio = SourceAudio::new(AudioBuffer { sample_rate: 48_000, channels: vec![x.clone(), x] });
    let t = std::time::Instant::now();
    for _ in 0..20 {
        std::hint::black_box(paint(64.0, |_, _, _| {}));
    }
    println!("empty frame: {:.3} ms", t.elapsed().as_secs_f64() * 1e3 / 20.0);
    for spp in [n as f64 / 400.0, 64.0, 4.0] {
        let t = std::time::Instant::now();
        let runs = 20;
        for _ in 0..runs {
            std::hint::black_box(paint(spp, |p, s, band| waveform::draw(p, s, band, &clip, &audio, 2, band, WAVE, false)));
        }
        println!("draw at {spp:.0} samples/px: {:.3} ms per frame", t.elapsed().as_secs_f64() * 1e3 / f64::from(runs));
        let t = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(paint(spp, |p, s, band| waveform::draw(p, s, band, &clip, &audio, 2, band, WAVE, true)));
        }
        println!("  coloured by frequency: {:.3} ms per frame", t.elapsed().as_secs_f64() * 1e3 / f64::from(runs));
    }
}

/// The distinct opaque colours of the waveform meshes in a track's row.
fn lane_colours(ctx: &egui::Context, app: &mut SoundApp, name: &str) -> std::collections::BTreeSet<[u8; 4]> {
    let mut shapes = Vec::new();
    for _ in 0..2 {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), ..Default::default() };
        let mut out = ctx.run_ui(raw, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        out.textures_delta.clear();
        shapes = out.shapes;
    }
    let id = app.engine.session().track_by_name(name).unwrap().id.0;
    let row = app.edit_layout.rows.iter().find(|(i, _)| *i == id).unwrap().1;
    let [x0, _, x1, _] = app.edit_layout.timeline;
    let lane = Rect::from_min_max(egui::pos2(x0, row[1]), egui::pos2(x1, row[3]));
    shapes
        .iter()
        .filter_map(|c| if let Shape::Mesh(m) = &c.shape { Some(m) } else { None })
        .filter(|m| m.vertices.iter().any(|v| lane.contains(v.pos)))
        .flat_map(|m| m.vertices.iter().filter(|v| v.color.a() == 255).map(|v| v.color.to_array()))
        .collect()
}

#[test]
fn split_clips_keep_one_colour_when_waveforms_are_coloured_by_frequency() {
    let mut app = SoundApp::new(soundcraft_engine::demo::demo_engine(), None, Services::default());
    app.ui.waveform_bands = Some(true);
    let ctx = egui::Context::default();
    assert!(lane_colours(&ctx, &mut app, "Bass").len() > 1, "coloured by frequency");
    let tid = app.engine.session().track_by_name("Bass").unwrap().id;
    let ids: Vec<ClipId> = app.engine.session().track(tid).unwrap().clips().iter().map(|c| c.id).collect();
    for id in ids {
        let c = app.engine.session_mut().find_clip_mut(id).unwrap();
        let src = c.source().unwrap();
        c.stems = Some(soundcraft_model::StemSet { scale_log2: 0, key: "k".into(), sources: [src; 4] });
    }
    assert_eq!(lane_colours(&ctx, &mut app, "Bass").len(), 1, "split clips keep the clip's colour");
    app.ui.waveform_bands = Some(false);
    assert_eq!(lane_colours(&ctx, &mut app, "Kick").len(), 1, "off: one colour");
}
