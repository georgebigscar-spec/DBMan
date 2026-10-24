//! The value panel next to a result grid: the whole value of the current
//! cell, with JSON and XML indented, bytes as a hex dump and images drawn.

use std::hash::{Hash, Hasher};

use std::collections::HashSet;

use dbman_core::json_tree::{self, Path, Step};
use dbman_core::value::{self, Shown};
use dbman_core::{Cell, ColumnMeta};
use egui::{Color32, RichText, Ui};
use serde_json::Value;
use shadcn_egui::{Badge, Button, ButtonSize, Input, Theme, TooltipExt, toast, toggle_group};

use crate::i18n::{tr, trf};

/// Text longer than this is cut, so a huge value does not stall the window.
const TEXT_LIMIT: usize = 1024 * 1024;
/// Rows of a JSON tree drawn at most.
const TREE_LIMIT: usize = 2000;

#[derive(Default)]
pub struct ValuePanel {
    pub open: bool,
    /// What the shown value was taken for ("JSON", "PNG image"…), for tests.
    pub kind: String,
    /// The decoded image of the shown value, by a hash of its bytes.
    image: Option<(u64, Option<egui::TextureHandle>)>,
    /// Values in the JSON tree can be edited (set by the grid before drawing).
    pub editable: bool,
    /// The cell's new text after an edit in the JSON tree, for the grid to take.
    pub edited: Option<String>,
    /// JSON shown as indented text instead of a tree.
    pub json_text: bool,
    /// Text searched for in the JSON tree's keys and values.
    pub search: String,
    /// The parsed JSON of the shown value, by a hash of its text.
    json: Option<(u64, Value)>,
    /// The tree value being edited: its path and the text typed.
    editing: Option<(Path, String)>,
    edit_error: Option<String>,
}

/// The current cell of a grid: its column, row number and value.
pub struct Current<'a> {
    pub column: &'a ColumnMeta,
    pub row: usize,
    pub cell: &'a Cell,
}

impl ValuePanel {
    /// Draws the panel at the right side of `ui`; call it before the grid.
    pub fn show(&mut self, ui: &mut Ui, id: egui::Id, current: Option<Current<'_>>) {
        if !self.open {
            return;
        }
        egui::Panel::right(id)
            .resizable(true)
            .default_size(380.0)
            .min_size(220.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, ..Default::default() }))
            .show(ui, |ui| self.contents(ui, id, current));
    }

    fn contents(&mut self, ui: &mut Ui, id: egui::Id, current: Option<Current<'_>>) {
        let t = Theme::get(ui.ctx());
        ui.horizontal(|ui| {
            match &current {
                Some(c) => {
                    ui.label(RichText::new(&c.column.name).strong().color(t.foreground));
                    if !c.column.type_name.is_empty() {
                        ui.label(RichText::new(&c.column.type_name).color(t.muted_foreground));
                    }
                    ui.label(RichText::new(trf("row {n}", &[("n", &(c.row + 1))])).color(t.muted_foreground));
                }
                None => {
                    ui.label(RichText::new(tr("Value")).strong().color(t.foreground));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(Button::ghost("×").size(ButtonSize::Sm)).tooltip(tr("Close")).clicked() {
                    self.open = false;
                }
            });
        });
        let Some(c) = current else {
            self.kind.clear();
            ui.add_space(8.0);
            ui.label(RichText::new(tr("Click a cell to see its whole value.")).color(t.muted_foreground));
            return;
        };
        let shown = value::show(c.cell);
        let (kind, size) = match (&shown, c.cell) {
            (Shown::Null, _) => ("NULL".to_string(), None),
            (Shown::Json(_), Cell::Text(s)) => ("JSON".into(), Some(s.len())),
            (Shown::Xml(_), Cell::Text(s)) => ("XML".into(), Some(s.len())),
            (Shown::Image(f, _), Cell::Bytes(b)) => (trf("{format} image", &[("format", f)]), Some(b.len())),
            (Shown::Bytes(_), Cell::Bytes(b)) => (tr("bytes").into(), Some(b.len())),
            (_, Cell::Text(s)) => (tr("text").into(), Some(s.len())),
            _ => (String::new(), None),
        };
        self.kind = kind.clone();
        ui.horizontal(|ui| {
            ui.add(Badge::new(kind).outline());
            if let Some(n) = size {
                ui.label(RichText::new(value::size_label(n)).color(t.muted_foreground));
            }
            if let Some(raw) = c.cell.as_text()
                && ui
                    .add(Button::outline(tr("Copy")).size(ButtonSize::Sm))
                    .tooltip(tr("Copy the value as it is"))
                    .clicked()
            {
                ui.ctx().copy_text(raw.into_owned());
                toast(ui.ctx(), tr("Copied"), None);
            }
        });
        ui.add_space(6.0);
        if let (Shown::Json(_), Some(raw)) = (&shown, c.cell.as_text()) {
            let mut mode = usize::from(self.json_text);
            if toggle_group(ui, &mut mode, &[tr("Tree"), tr("Text")]).changed() {
                self.json_text = mode == 1;
            }
            ui.add_space(4.0);
            if !self.json_text {
                self.json_tree(ui, id, &raw);
                return;
            }
        }
        let text = match &shown {
            Shown::Null => {
                ui.label(RichText::new("NULL").italics().color(t.muted_foreground));
                return;
            }
            Shown::Json(s) | Shown::Xml(s) | Shown::Text(s) | Shown::Bytes(s) => s.as_str(),
            Shown::Image(_, hex) => {
                if let Cell::Bytes(b) = c.cell {
                    self.image(ui, id, b);
                }
                hex.as_str()
            }
        };
        let cut = cut(text, TEXT_LIMIT);
        // Plain text wraps; JSON, XML and hex dumps keep their lines.
        let wrap = matches!(shown, Shown::Text(_));
        let color = t.foreground;
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let width = if wrap { wrap_width } else { f32::INFINITY };
            let job = egui::text::LayoutJob::simple(buf.as_str().to_owned(), font, color, width);
            ui.fonts_mut(|f| f.layout_job(job))
        };
        egui::ScrollArea::both().id_salt(id.with("scroll")).auto_shrink([false, false]).show(ui, |ui| {
            let mut view = cut;
            ui.add(
                egui::TextEdit::multiline(&mut view)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(if wrap { ui.available_width() } else { f32::INFINITY })
                    .frame(egui::Frame::NONE)
                    .layouter(&mut layouter),
            );
            if cut.len() < text.len() {
                ui.label(
                    RichText::new(trf("Showing the first {size}.", &[("size", &value::size_label(cut.len()))]))
                        .color(t.muted_foreground),
                );
            }
        });
    }

    /// The JSON value as a tree, with a search and, when editable, its values edited in place.
    fn json_tree(&mut self, ui: &mut Ui, id: egui::Id, raw: &str) {
        let t = Theme::get(ui.ctx());
        let mut h = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut h);
        let key = h.finish();
        if self.json.as_ref().is_none_or(|(k, _)| *k != key) {
            let Ok(v) = serde_json::from_str::<Value>(raw) else { return };
            self.json = Some((key, v));
            self.editing = None;
            self.edit_error = None;
        }
        ui.add(Input::new(&mut self.search).placeholder(tr("Find in keys and values…")).width(ui.available_width()));
        if let Some(e) = &self.edit_error {
            ui.label(RichText::new(e).size(12.0).color(t.destructive));
        }
        let Some((_, root)) = self.json.take() else { return };
        let hits = json_tree::search(&root, &self.search);
        let searching = !self.search.trim().is_empty();
        if searching && hits.is_empty() {
            ui.label(RichText::new(tr("Nothing found.")).color(t.muted_foreground));
        }
        let mut tree = TreeState {
            hits: &hits,
            searching,
            editable: self.editable,
            editing: &mut self.editing,
            commit: None,
            copy: None,
            drawn: 0,
        };
        egui::ScrollArea::both().id_salt(id.with("json")).auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            node(ui, &mut tree, id, None, &root, &mut Vec::new());
            if tree.drawn > TREE_LIMIT {
                let note = trf("The first {n} rows are shown; the text view has all of it.", &[("n", &TREE_LIMIT)]);
                ui.label(RichText::new(note).color(t.muted_foreground));
            }
        });
        let (commit, copy) = (tree.commit.take(), tree.copy.take());
        let mut root = root;
        if let Some((path, text)) = commit {
            match json_tree::set(&mut root, &path, &text) {
                Ok(()) => {
                    self.edit_error = None;
                    self.edited = Some(root.to_string());
                }
                Err(e) => self.edit_error = Some(e),
            }
        }
        if let Some(text) = copy {
            ui.ctx().copy_text(text);
            toast(ui.ctx(), tr("Copied"), None);
        }
        self.json = Some((key, root));
    }

    /// Draws `bytes` as an image, decoded once per value.
    fn image(&mut self, ui: &mut Ui, id: egui::Id, bytes: &[u8]) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        let key = h.finish();
        if self.image.as_ref().is_none_or(|(k, _)| *k != key) {
            let texture = image::load_from_memory(bytes).ok().map(|img| {
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                // Small images are scaled up, so keep their pixels sharp.
                let small = size[0].max(size[1]) < 64;
                let options = if small { egui::TextureOptions::NEAREST } else { egui::TextureOptions::LINEAR };
                ui.ctx().load_texture(format!("{id:?}-value"), color, options)
            });
            self.image = Some((key, texture));
        }
        let t = Theme::get(ui.ctx());
        match self.image.as_ref().and_then(|(_, tex)| tex.as_ref()) {
            Some(tex) => {
                let [w, h] = tex.size().map(|v| v as f32);
                // Fit the panel and 320 px of height; small images grow to 128 px.
                let grow = (128.0 / w.max(h)).max(1.0);
                let scale = (ui.available_width() / w).min(320.0 / h).min(grow);
                ui.add(egui::Image::new((tex.id(), egui::vec2(w * scale, h * scale))));
                ui.label(RichText::new(format!("{} × {}", w as u32, h as u32)).color(t.muted_foreground));
            }
            None => {
                ui.label(RichText::new(tr("The image could not be decoded.")).color(t.muted_foreground));
            }
        }
        ui.add_space(6.0);
    }
}

/// The first `max` bytes of `s`, cut at a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// What drawing the JSON tree reads and reports.
struct TreeState<'a> {
    hits: &'a HashSet<Path>,
    searching: bool,
    editable: bool,
    editing: &'a mut Option<(Path, String)>,
    /// A value typed and confirmed: its path and text.
    commit: Option<(Path, String)>,
    /// Text to put on the clipboard.
    copy: Option<String>,
    drawn: usize,
}

fn value_color(v: &Value, t: &Theme) -> Color32 {
    match v {
        Value::String(_) => Color32::from_rgb(0x22, 0xa0, 0x5a),
        Value::Number(_) => Color32::from_rgb(0x3b, 0x82, 0xf6),
        Value::Bool(_) | Value::Null => Color32::from_rgb(0xa8, 0x55, 0xf7),
        _ => t.muted_foreground,
    }
}

fn key_text(key: Option<&Step>) -> String {
    match key {
        Some(Step::Key(k)) => k.clone(),
        Some(Step::Index(i)) => i.to_string(),
        None => "$".into(),
    }
}

/// One node of the JSON tree and, for objects and arrays, its children.
fn node(ui: &mut Ui, s: &mut TreeState<'_>, id: egui::Id, key: Option<&Step>, v: &Value, path: &mut Path) {
    s.drawn += 1;
    if s.drawn > TREE_LIMIT {
        return;
    }
    let t = Theme::get(ui.ctx());
    let hit = s.searching && s.hits.contains(path);
    let name = key_text(key);
    let name_color = if hit { t.foreground } else { t.muted_foreground };
    match v {
        Value::Object(_) | Value::Array(_) => {
            let mut header = RichText::new(format!("{name}  {}", json_tree::scalar_label(v))).color(name_color);
            if hit {
                header = header.strong();
            }
            let mut c = egui::CollapsingHeader::new(header).id_salt(id.with(path.clone())).default_open(path.len() < 2);
            // A search opens the way to what it finds and leaves the rest as it was.
            if hit {
                c = c.open(Some(true));
            }
            let r = c.show(ui, |ui| {
                let children: Vec<(Step, &Value)> = match v {
                    Value::Object(o) => o.iter().map(|(k, c)| (Step::Key(k.clone()), c)).collect(),
                    Value::Array(a) => a.iter().enumerate().map(|(i, c)| (Step::Index(i), c)).collect(),
                    _ => Vec::new(),
                };
                for (step, child) in children {
                    path.push(step.clone());
                    node(ui, s, id, Some(&step), child, path);
                    path.pop();
                }
            });
            let label = json_tree::path_label(path);
            r.header_response.context_menu(|ui| {
                if ui.button(tr("Copy path")).clicked() {
                    s.copy = Some(label.clone());
                }
                if ui.button(tr("Copy value")).clicked() {
                    s.copy = Some(serde_json::to_string_pretty(v).unwrap_or_default());
                }
            });
        }
        _ => {
            let editing_here = s.editing.as_ref().is_some_and(|(p, _)| p == path);
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                let mut k = RichText::new(format!("{name}:")).color(name_color);
                if hit {
                    k = k.strong();
                }
                ui.add(egui::Label::new(k).selectable(false));
                if editing_here {
                    let Some((_, text)) = s.editing.as_mut() else { return };
                    let r = ui.add(
                        egui::TextEdit::singleline(text)
                            .id(id.with(("json-edit", path.clone())))
                            .font(egui::TextStyle::Monospace)
                            .desired_width(ui.available_width() - 8.0),
                    );
                    if !r.has_focus() && !r.lost_focus() {
                        r.request_focus();
                    }
                    if r.lost_focus() {
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let (p, text) = s.editing.take().expect("editing this node");
                        if enter {
                            s.commit = Some((p, text));
                        }
                    }
                    return;
                }
                let mut text = RichText::new(json_tree::scalar_label(v)).monospace().color(value_color(v, &t));
                if hit {
                    text = text.strong().background_color(Color32::from_rgba_unmultiplied(0xf5, 0x9e, 0x0b, 60));
                }
                let r = ui.add(egui::Label::new(text).truncate().sense(egui::Sense::click()));
                let r = if s.editable { r.on_hover_text(tr("Double-click to edit")) } else { r };
                if s.editable && r.double_clicked() {
                    *s.editing = Some((path.clone(), json_tree::edit_text(v)));
                }
                let label = json_tree::path_label(path);
                r.context_menu(|ui| {
                    if s.editable && ui.button(tr("Edit")).clicked() {
                        *s.editing = Some((path.clone(), json_tree::edit_text(v)));
                    }
                    if ui.button(tr("Copy path")).clicked() {
                        s.copy = Some(label.clone());
                    }
                    if ui.button(tr("Copy value")).clicked() {
                        s.copy = Some(json_tree::edit_text(v));
                    }
                });
            });
        }
    }
}
