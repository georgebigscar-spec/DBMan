//! SQL editor: a monospace `TextEdit` with syntax highlighting.

use std::ops::Range;
use std::sync::Arc;

use dbman_core::DbKind;
use dbman_core::sql::{TokenKind, tokenize};
use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Color32, FontId, TextBuffer, Ui};
use shadcn_egui::Theme;

pub struct EditorOutput {
    /// Byte range of the selection (empty when there is only a cursor).
    pub selection: Option<Range<usize>>,
}

struct Palette {
    keyword: Color32,
    string: Color32,
    comment: Color32,
    number: Color32,
    ident: Color32,
    text: Color32,
}

fn palette(t: &Theme) -> Palette {
    if t.dark {
        Palette {
            keyword: Color32::from_rgb(0xcc, 0x7a, 0xff),
            string: Color32::from_rgb(0x8f, 0xd6, 0x8f),
            comment: Color32::from_rgb(0x80, 0x80, 0x80),
            number: Color32::from_rgb(0x6c, 0xb6, 0xff),
            ident: Color32::from_rgb(0xff, 0xc6, 0x6d),
            text: t.foreground,
        }
    } else {
        Palette {
            keyword: Color32::from_rgb(0x00, 0x33, 0xb3),
            string: Color32::from_rgb(0x06, 0x7d, 0x17),
            comment: Color32::from_rgb(0x8c, 0x8c, 0x8c),
            number: Color32::from_rgb(0x17, 0x50, 0xeb),
            ident: Color32::from_rgb(0x87, 0x10, 0x94),
            text: t.foreground,
        }
    }
}

pub const FONT_SIZE: f32 = 13.5;

/// Highlighted layout of `sql`.
pub fn highlight(ui: &Ui, sql: &str, kind: DbKind) -> LayoutJob {
    let p = palette(&Theme::get(ui.ctx()));
    let font = FontId::monospace(FONT_SIZE);
    let mut job = LayoutJob::default();
    for tok in tokenize(sql, kind) {
        let color = match tok.kind {
            TokenKind::Keyword => p.keyword,
            TokenKind::String => p.string,
            TokenKind::Comment => p.comment,
            TokenKind::Number => p.number,
            TokenKind::QuotedIdent => p.ident,
            _ => p.text,
        };
        job.append(&sql[tok.range], 0.0, TextFormat::simple(font.clone(), color));
    }
    job
}

fn byte_offset(text: &str, c: CCursor) -> usize {
    text.char_indices().nth(c.index.into()).map_or(text.len(), |(b, _)| b)
}

/// An editable SQL area filling the available width, `rows` lines tall at least.
pub fn editor(ui: &mut Ui, id: egui::Id, text: &mut String, kind: DbKind, min_height: f32) -> EditorOutput {
    let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, wrap_width: f32| -> Arc<egui::Galley> {
        let mut job = highlight(ui, buf.as_str(), kind);
        job.wrap.max_width = wrap_width;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let out = egui::TextEdit::multiline(text)
        .id(id)
        .code_editor()
        .desired_width(f32::INFINITY)
        .min_size(egui::vec2(0.0, min_height))
        .lock_focus(true)
        .layouter(&mut layouter)
        .show(ui);
    let selection = out.cursor_range.map(|r| {
        let a = byte_offset(text, r.primary);
        let b = byte_offset(text, r.secondary);
        a.min(b)..a.max(b)
    });
    EditorOutput { selection }
}

/// Read-only, selectable, highlighted SQL.
pub fn viewer(ui: &mut Ui, id: egui::Id, text: &str, kind: DbKind) {
    let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, wrap_width: f32| -> Arc<egui::Galley> {
        let mut job = highlight(ui, buf.as_str(), kind);
        job.wrap.max_width = wrap_width;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let mut s = text;
    egui::TextEdit::multiline(&mut s)
        .id(id)
        .code_editor()
        .desired_width(f32::INFINITY)
        .layouter(&mut layouter)
        .show(ui);
}
