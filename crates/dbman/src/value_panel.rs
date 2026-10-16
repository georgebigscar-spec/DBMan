//! The value panel next to a result grid: the whole value of the current
//! cell, with JSON and XML indented, bytes as a hex dump and images drawn.

use std::hash::{Hash, Hasher};

use dbman_core::value::{self, Shown};
use dbman_core::{Cell, ColumnMeta};
use egui::{RichText, Ui};
use shadcn_egui::{Badge, Button, ButtonSize, Theme, TooltipExt, toast};

use crate::i18n::{tr, trf};

/// Text longer than this is cut, so a huge value does not stall the window.
const TEXT_LIMIT: usize = 1024 * 1024;

#[derive(Default)]
pub struct ValuePanel {
    pub open: bool,
    /// What the shown value was taken for ("JSON", "PNG image"…), for tests.
    pub kind: String,
    /// The decoded image of the shown value, by a hash of its bytes.
    image: Option<(u64, Option<egui::TextureHandle>)>,
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
