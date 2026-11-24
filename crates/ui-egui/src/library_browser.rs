//! The Decks tab's library browser: mixforge's library, the folders (the Music folder and those
//! added) and the tracks that mix with a deck, searched, sorted and paged through `library.list`,
//! `library.search` and `library.compatible`. A track loads by dragging it onto a deck, by the
//! Load buttons, by double-click (the first free deck) or by Shift+Left/Right.
//!
//! Queries run only when what is asked changes, or when a read of the libraries ends; a read
//! under way keeps the last rows and says "library loading…".

use crate::SoundApp;
use crate::theme::{Tokens, bold, regular};
use crate::widgets::elided_text;
use egui::{Align2, Color32, Rect, Sense, Stroke, Ui, pos2, vec2};
use serde_json::{Value, json};
use soundcraft_mix::DeckId;

/// Rows a page holds.
pub const PAGE: usize = 100;
/// A row's height.
const ROW_H: f32 = 20.0;

/// Where the rows come from.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Source {
    /// mixforge's library.
    #[default]
    Library,
    /// A folder (a root or one below it).
    Folder { root: String, path: String },
    /// The tracks that mix with a deck's.
    Compatible(DeckId),
}

/// A library row, as `library.list` gives it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub path: String,
    pub title: String,
    pub artist: String,
    pub bpm: Option<f64>,
    pub key: Option<String>,
    pub key_source: Option<String>,
    pub energy: Option<f64>,
    pub length_s: Option<f64>,
    pub analysed: bool,
    pub stale: bool,
    pub outdated: bool,
    pub missing: bool,
    /// Why it mixes (compatible rows).
    pub why: Option<String>,
}

impl Row {
    fn of(v: &Value) -> Row {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let f = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
        let path = s("path").unwrap_or_default();
        Row {
            title: s("title").filter(|t| !t.trim().is_empty()).or_else(|| s("name")).unwrap_or_else(|| path.clone()),
            artist: s("artist").unwrap_or_default(),
            path,
            bpm: f("bpm"),
            key: s("key"),
            key_source: s("key_source"),
            energy: f("energy"),
            length_s: f("length_s"),
            analysed: b("analysed"),
            stale: b("stale"),
            outdated: b("outdated"),
            missing: b("missing"),
            why: s("why"),
        }
    }

    /// The row's state icon (colour, tooltip), if it isn't simply analysed.
    fn status(&self) -> Option<(Color32, &'static str)> {
        if self.missing {
            Some((Color32::from_rgb(0xe0, 0x34, 0x2c), "Missing: no file there"))
        } else if self.outdated {
            Some((Color32::from_rgb(0x5a, 0x9b, 0xff), "Outdated: analysed by another mixforge version (analysed again on load)"))
        } else if self.stale {
            Some((Color32::from_rgb(0xff, 0xa8, 0x2e), "Stale: the file changed since mixforge analysed it"))
        } else if !self.analysed {
            Some((Color32::from_gray(120), "Not analysed (analysed on load)"))
        } else {
            None
        }
    }
}

/// A root of the folder browser (`library.folders`).
#[derive(Debug, Clone, PartialEq)]
pub struct Root {
    pub path: String,
    pub name: String,
    pub added: bool,
    pub found: bool,
}

/// A track dragged from the browser.
#[derive(Debug, Clone)]
pub struct Dragged(pub String);

/// The browser's state between frames.
#[derive(Debug, Default)]
pub struct Browser {
    pub source: Source,
    pub query: String,
    pub sort: Option<&'static str>,
    pub desc: bool,
    pub offset: usize,
    pub rows: Vec<Row>,
    pub total: usize,
    /// A folder's subfolders (path, name).
    pub subfolders: Vec<(String, String)>,
    pub roots: Vec<Root>,
    roots_read: bool,
    pub selected: Option<usize>,
    /// The last query asked, and whether its rows were the last read's while another ran.
    asked: Option<String>,
    stale_rows: bool,
    pub error: Option<String>,
}

impl Browser {
    /// The selected row's path.
    pub fn selected_path(&self) -> Option<String> {
        self.selected.and_then(|i| self.rows.get(i)).map(|r| r.path.clone())
    }
}

/// The query for what the browser shows: (command, parameters), plus what else makes its
/// answer change (a compatible list follows the deck's track and tempo).
fn query(app: &SoundApp) -> (&'static str, Value, String) {
    let b = &app.decks_view.browser;
    let mut p = json!({"offset": b.offset, "limit": PAGE});
    if let Some(s) = b.sort {
        p["sort"] = json!(s);
        p["desc"] = json!(b.desc);
    }
    let q = b.query.trim();
    if !q.is_empty() {
        p["query"] = json!(q);
    }
    let mut extra = String::new();
    let id = match &b.source {
        Source::Library => {
            if q.is_empty() {
                "library.list"
            } else {
                "library.search"
            }
        }
        Source::Folder { path, .. } => {
            p["source"] = json!("folder");
            p["folder"] = json!(path);
            "library.list"
        }
        Source::Compatible(d) => {
            p["deck"] = json!(d.id());
            let s = app.engine.deck_status.decks.get(d.index()).copied().unwrap_or_default();
            let track = app.engine.deck_tracks.get(d.index()).and_then(Option::as_ref).map(|t| t.path.clone()).unwrap_or_default();
            extra = format!("{track} {:.1}", s.bpm);
            "library.compatible"
        }
    };
    (id, p, extra)
}

/// Runs the browser's query when what it asks changed or a read of the libraries ended.
pub fn refresh(app: &mut SoundApp) {
    if !app.decks_view.browser.roots_read {
        read_roots(app);
    }
    let (id, p, extra) = query(app);
    let key = format!("{id} {p} {extra}");
    let reread = app.decks_view.browser.stale_rows && !app.engine.library_loading();
    if app.decks_view.browser.asked.as_deref() == Some(key.as_str()) && !reread {
        return;
    }
    let empty_deck =
        matches!(app.decks_view.browser.source, Source::Compatible(d) if app.engine.deck_tracks.get(d.index()).is_none_or(Option::is_none));
    let b = &mut app.decks_view.browser;
    b.asked = Some(key);
    if empty_deck {
        b.rows.clear();
        b.total = 0;
        b.subfolders.clear();
        b.error = Some("Load a track on the deck to see what mixes with it".into());
        b.stale_rows = false;
        return;
    }
    let r = app.engine.execute(id, &p);
    let b = &mut app.decks_view.browser;
    match r {
        Ok(v) => {
            let before = b.selected_path();
            b.rows = v.get("rows").and_then(Value::as_array).map(|a| a.iter().map(Row::of).collect()).unwrap_or_default();
            b.total = v.get("total").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(b.rows.len());
            b.subfolders = v
                .get("folders")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|f| Some((f.get("path")?.as_str()?.to_string(), f.get("name")?.as_str()?.to_string()))).collect())
                .unwrap_or_default();
            b.stale_rows = v.get("loading").and_then(Value::as_bool).unwrap_or(false);
            b.error = v.get("library_error").and_then(Value::as_str).map(|e| format!("mixforge's library: {e}"));
            b.selected = before.and_then(|p| b.rows.iter().position(|r| r.path == p)).or(if b.rows.is_empty() { None } else { Some(0) });
        }
        Err(e) => {
            b.rows.clear();
            b.total = 0;
            b.subfolders.clear();
            b.stale_rows = false;
            // The engine's wording names the command; the browser says what it means here.
            let e = e.to_string();
            b.error = Some(e.split_once("`: ").map_or(e.clone(), |(_, why)| why.to_string()));
        }
    }
}

fn read_roots(app: &mut SoundApp) {
    let b = &mut app.decks_view.browser;
    b.roots_read = true;
    let Ok(v) = app.engine.execute("library.folders", &json!({})) else { return };
    let roots: Vec<Root> = v
        .get("folders")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(Root {
                        path: f.get("path")?.as_str()?.to_string(),
                        name: f.get("name").and_then(Value::as_str).unwrap_or("Folder").to_string(),
                        added: f.get("kind").and_then(Value::as_str) == Some("added"),
                        found: f.get("found").and_then(Value::as_bool).unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    app.decks_view.browser.roots = roots;
}

/// Forgets the folders read and the rows asked for, so both are read again.
pub fn reread(app: &mut SoundApp) {
    let b = &mut app.decks_view.browser;
    b.roots_read = false;
    b.asked = None;
}

fn tab(ui: &mut Ui, label: &str, on: bool, tip: &str) -> bool {
    let t = Tokens::current();
    let g = ui.painter().layout_no_wrap(label.to_string(), bold(11.0), if on { t.accent_text } else { t.text });
    let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 18.0, 22.0), Sense::click());
    ui.painter().rect_filled(
        r,
        3.0,
        if on {
            t.tab_on
        } else if resp.hovered() {
            t.button_hi
        } else {
            t.button
        },
    );
    ui.painter().galley(pos2(r.min.x + 9.0, r.center().y - g.size().y * 0.5), g, Color32::WHITE);
    let label = label.to_string();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, &label));
    resp.on_hover_text(tip).clicked()
}

/// What a click in the browser asked for.
enum Act {
    Source(Source),
    Load(String, Option<DeckId>),
    Sort(&'static str),
    Enter(String),
    Up,
    Page(isize),
    AddFolder,
    RemoveFolder(String),
}

/// Draws the browser in `ui`'s rect.
pub fn show(app: &mut SoundApp, ui: &mut Ui) {
    refresh(app);
    let t = Tokens::current();
    let mut act: Option<Act> = None;
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, t.panel_bg);
    ui.spacing_mut().item_spacing = vec2(4.0, 4.0);
    let b = &app.decks_view.browser;
    // Sources, search and paging.
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        if tab(ui, "mixforge library", b.source == Source::Library, "What mixforge analysed (read only)") {
            act = Some(Act::Source(Source::Library));
        }
        for r in &b.roots {
            let on = matches!(&b.source, Source::Folder { root, .. } if *root == r.path);
            let tip = if r.found { r.path.clone() } else { format!("{} (not found)", r.path) };
            if tab(ui, &r.name, on, &tip) {
                act = Some(Act::Source(Source::Folder { root: r.path.clone(), path: r.path.clone() }));
            }
        }
        for d in DeckId::ALL {
            let label = format!("Compatible with {}", d.id().to_uppercase());
            if tab(ui, &label, b.source == Source::Compatible(d), "Key ±1 or relative, tempo within 6 %") {
                act = Some(Act::Source(Source::Compatible(d)));
            }
        }
        if ui.small_button("+ Folder").on_hover_text("Add a folder to the browser").clicked() {
            act = Some(Act::AddFolder);
        }
        if let Source::Folder { root, .. } = &b.source
            && b.roots.iter().any(|r| r.path == *root && r.added)
            && ui.small_button("− Folder").on_hover_text("Take this folder off the browser (the folder itself is left alone)").clicked()
        {
            act = Some(Act::RemoveFolder(root.clone()));
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let search =
            ui.add(egui::TextEdit::singleline(&mut app.decks_view.browser.query).hint_text("Search title, artist, file…").desired_width(260.0));
        if search.changed() {
            app.decks_view.browser.offset = 0;
        }
        let b = &app.decks_view.browser;
        let (from, to) = (b.offset.min(b.total), (b.offset + b.rows.len()).min(b.total));
        ui.label(egui::RichText::new(format!("{}–{} of {}", if b.total == 0 { 0 } else { from + 1 }, to, b.total)).color(t.text_dim));
        if ui.add_enabled(b.offset > 0, egui::Button::new("◀").small()).on_hover_text("Previous page").clicked() {
            act = Some(Act::Page(-1));
        }
        if ui.add_enabled(to < b.total, egui::Button::new("▶").small()).on_hover_text("Next page").clicked() {
            act = Some(Act::Page(1));
        }
        if app.engine.library_loading() || b.stale_rows {
            ui.spinner();
            ui.label(egui::RichText::new("library loading…").color(t.text_dim));
        }
        if let Some(e) = &b.error {
            ui.label(egui::RichText::new(e).color(t.text_dim));
        }
        if let Source::Folder { root, path } = &b.source
            && root != path
            && ui.small_button("⬆ Up").clicked()
        {
            act = Some(Act::Up);
        }
    });
    // The table.
    let area = ui.available_rect_before_wrap();
    let cols = columns(area.width() - 12.0);
    let head = Rect::from_min_size(pos2(area.min.x + 6.0, area.min.y), vec2(area.width() - 12.0, ROW_H));
    ui.painter().rect_filled(head, 0.0, t.panel_bg2);
    let b = &app.decks_view.browser;
    for (i, (name, sort, x, w)) in cols.iter().enumerate() {
        let r = Rect::from_min_size(pos2(head.min.x + x, head.min.y), vec2(*w, ROW_H));
        let resp = ui.interact(r, ui.id().with(("lib_head", i)), if sort.is_some() { Sense::click() } else { Sense::hover() });
        let arrow = if sort.is_some() && b.sort == *sort { if b.desc { " ▼" } else { " ▲" } } else { "" };
        ui.painter().text(pos2(r.min.x + 4.0, r.center().y), Align2::LEFT_CENTER, format!("{name}{arrow}"), bold(10.5), t.text_dim);
        if let Some(s) = sort {
            let label = format!("Sort by {name}");
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
            if resp.clicked() {
                act = Some(Act::Sort(s));
            }
        }
    }
    let body = Rect::from_min_max(pos2(area.min.x, head.max.y), area.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
    let subs = b.subfolders.clone();
    let n = subs.len() + b.rows.len();
    egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("lib_rows").show_rows(&mut child, ROW_H, n, |ui, range| {
        for i in range {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click_and_drag());
            let r = Rect::from_min_max(pos2(r.min.x + 6.0, r.min.y), pos2(r.max.x - 6.0, r.max.y));
            if let Some((path, name)) = subs.get(i) {
                if resp.hovered() {
                    ui.painter().rect_filled(r, 0.0, t.button_hi);
                }
                ui.painter().text(pos2(r.min.x + 26.0, r.center().y), Align2::LEFT_CENTER, format!("▸ {name}"), regular(12.0), t.text);
                let label = format!("Folder {name}");
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                if resp.double_clicked() || resp.clicked() {
                    act = Some(Act::Enter(path.clone()));
                }
                continue;
            }
            let k = i - subs.len();
            let b = &app.decks_view.browser;
            let Some(row) = b.rows.get(k).cloned() else { continue };
            let row = &row;
            let sel = b.selected == Some(k);
            if sel {
                ui.painter().rect_filled(r, 0.0, t.row_selected);
            } else if resp.hovered() {
                ui.painter().rect_filled(r, 0.0, t.button_hi);
            } else if k % 2 == 1 {
                ui.painter().rect_filled(r, 0.0, t.panel_bg2.gamma_multiply(0.5));
            }
            draw_row(ui, r, row, &cols);
            let label = format!("Track {}", row.title);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
            let resp = match &row.why {
                Some(why) => resp.on_hover_text(why.clone()),
                None => resp,
            };
            resp.dnd_set_drag_payload(Dragged(row.path.clone()));
            if resp.double_clicked() {
                act = Some(Act::Load(row.path.clone(), None));
            } else if resp.clicked() || resp.drag_started() {
                app.decks_view.browser.selected = Some(k);
            }
            let path = row.path.clone();
            resp.context_menu(|ui| {
                for d in DeckId::ALL {
                    if ui.button(format!("Load to Deck {}", d.id().to_uppercase())).clicked() {
                        act = Some(Act::Load(path.clone(), Some(d)));
                        ui.close();
                    }
                }
            });
        }
    });
    if let Some(a) = act {
        apply(app, a);
    }
}

/// The columns: (name, sort key, x, width).
fn columns(w: f32) -> Vec<(&'static str, Option<&'static str>, f32, f32)> {
    let fixed = 22.0 + 60.0 + 60.0 + 54.0 + 60.0;
    let flex = (w - fixed).max(120.0);
    let (tw, aw) = (flex * 0.58, flex * 0.42);
    let mut x = 0.0;
    let mut out = Vec::new();
    for (name, sort, width) in [
        ("", None, 22.0),
        ("Title", Some("title"), tw),
        ("Artist", Some("artist"), aw),
        ("BPM", Some("bpm"), 60.0),
        ("Key", Some("key"), 60.0),
        ("Energy", Some("energy"), 54.0),
        ("Length", Some("length"), 60.0),
    ] {
        out.push((name, sort, x, width));
        x += width;
    }
    out
}

fn draw_row(ui: &Ui, r: Rect, row: &Row, cols: &[(&'static str, Option<&'static str>, f32, f32)]) {
    let t = Tokens::current();
    let p = ui.painter();
    let cell = |i: usize| cols.get(i).map(|c| (r.min.x + c.2, c.3)).unwrap_or((r.min.x, 0.0));
    let y = r.center().y;
    let font = regular(12.0);
    if let Some((c, tip)) = row.status() {
        let (x, _) = cell(0);
        let dot = pos2(x + 11.0, y);
        p.circle_filled(dot, 4.0, c);
        let hit = Rect::from_center_size(dot, vec2(14.0, 14.0));
        if ui.rect_contains_pointer(hit) {
            egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), ui.id().with(("status", &row.path)), egui::PopupAnchor::Pointer)
                .show(|ui| ui.label(tip));
        }
    }
    let text = |i: usize, s: &str, c: Color32| {
        let (x, w) = cell(i);
        elided_text(p, pos2(x + 4.0, y), Align2::LEFT_CENTER, s, font.clone(), c, (w - 8.0).max(1.0));
    };
    let dim = if row.missing { t.text_dim } else { t.text };
    text(1, &row.title, dim);
    text(2, &row.artist, t.text_dim);
    text(3, &row.bpm.map_or_else(|| "–".into(), |b| format!("{b:.1}")), dim);
    let key = match (&row.key, &row.key_source) {
        (Some(k), Some(s)) => format!("{k} · {}", source_short(s)),
        (Some(k), None) => k.clone(),
        _ => "–".into(),
    };
    text(4, &key, dim);
    text(5, &row.energy.map_or_else(|| "–".into(), |e| format!("{e:.0}")), dim);
    text(6, &row.length_s.map_or_else(|| "–".into(), clock), dim);
    p.hline(r.x_range(), r.max.y, Stroke::new(1.0, Color32::from_black_alpha(40)));
}

/// A key source in a word: MIK, mf (mixforge), tag.
pub fn source_short(s: &str) -> &str {
    match s {
        "mik" => "MIK",
        "mixforge" => "mf",
        other => other,
    }
}

/// `m:ss`.
pub fn clock(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "–".into();
    }
    let s = secs.floor() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn apply(app: &mut SoundApp, a: Act) {
    let b = &mut app.decks_view.browser;
    match a {
        Act::Source(s) => {
            if b.source != s {
                b.source = s;
                b.offset = 0;
                b.selected = None;
            }
        }
        Act::Load(path, deck) => crate::decks_view::load(app, &path, deck, false),
        Act::Sort(s) => {
            if b.sort == Some(s) {
                b.desc = !b.desc;
            } else {
                b.sort = Some(s);
                b.desc = false;
            }
            b.offset = 0;
        }
        Act::Enter(path) => {
            if let Source::Folder { root, .. } = &b.source {
                b.source = Source::Folder { root: root.clone(), path };
                b.offset = 0;
                b.selected = None;
            }
        }
        Act::Up => {
            if let Source::Folder { root, path } = &b.source {
                let up = std::path::Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).filter(|p| p.len() >= root.len());
                b.source = Source::Folder { root: root.clone(), path: up.unwrap_or_else(|| root.clone()) };
                b.offset = 0;
            }
        }
        Act::Page(d) => {
            b.offset = if d < 0 { b.offset.saturating_sub(PAGE) } else { b.offset.saturating_add(PAGE) };
            b.selected = None;
        }
        Act::AddFolder => {
            let _ = app.dialogs.open_for_command(&app.engine, "library.add_folder");
            reread(app);
        }
        Act::RemoveFolder(path) => {
            if app.run("library.remove_folder", json!({"path": path})).is_ok() {
                app.decks_view.browser.source = Source::Library;
            }
            reread(app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_read_what_the_library_gives_and_name_their_state() {
        let r = Row::of(&json!({"path": "/m/a.mp3", "name": "a.mp3", "title": "", "bpm": 138.0, "key": "8A", "key_source": "mik",
            "analysed": false, "stale": false, "outdated": false, "missing": false}));
        assert_eq!(r.title, "a.mp3");
        assert_eq!(r.bpm, Some(138.0));
        assert!(r.status().is_some_and(|s| s.1.starts_with("Not analysed")));
        let m = Row::of(&json!({"path": "/m/b.mp3", "analysed": true, "missing": true}));
        assert!(m.status().is_some_and(|s| s.1.starts_with("Missing")));
        assert_eq!(clock(125.9), "2:05");
        assert_eq!(clock(f64::NAN), "–");
    }
}
