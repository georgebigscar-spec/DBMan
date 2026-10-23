//! SQL editor: a monospace `TextEdit` with syntax highlighting, an
//! autocomplete popup (tables, columns, keywords, snippets) that opens as you
//! type a name or a `.`, or with Ctrl+Space, and a find and replace bar.

use std::ops::Range;
use std::sync::Arc;

use dbman_core::DbKind;
use dbman_core::complete::{self, Catalog, Completion, ItemKind};
use dbman_core::find::{self, Query};
use dbman_core::sql::{TokenKind, tokenize};
use egui::text::{CCursor, CCursorRange, LayoutJob, LayoutSection, TextFormat};
use egui::{Color32, FontId, Key, Modifiers, RichText, TextBuffer, Ui};
use shadcn_egui::{Button, ButtonSize, Input, Theme, Toggle};

use crate::i18n::{tr, trf};

pub struct EditorOutput {
    /// Byte range of the selection (empty when there is only a cursor).
    pub selection: Option<Range<usize>>,
    /// Tables (schema, name) whose columns the popup would show once loaded.
    pub wants: Vec<(String, String)>,
    /// Schemas whose tables the popup would show once loaded.
    pub want_schemas: Vec<String>,
}

/// The autocomplete popup of one editor.
#[derive(Default)]
pub struct Autocomplete {
    open: bool,
    /// Opened with Ctrl+Space: shows everything, even without a typed letter.
    explicit: bool,
    selected: usize,
    shown: Option<Completion>,
}

impl Autocomplete {
    pub fn is_open(&self) -> bool {
        self.open && self.shown.as_ref().is_some_and(|c| !c.items.is_empty())
    }

    /// The labels the popup shows (tests).
    pub fn labels(&self) -> Vec<String> {
        match (&self.shown, self.open) {
            (Some(c), true) => c.items.iter().map(|i| i.label.clone()).collect(),
            _ => Vec::new(),
        }
    }
}

/// Rows the popup shows at once.
const VISIBLE: usize = 10;

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

/// Replaces the word being typed with `insert` and puts the cursor after it,
/// or for a snippet where its `$0` is.
fn accept(ui: &Ui, id: egui::Id, text: &mut String, range: Range<usize>, insert: &str) {
    let range = range.start.min(text.len())..range.end.min(text.len());
    let (insert, cursor) = match insert.find(complete::CURSOR) {
        Some(i) => (insert.replacen(complete::CURSOR, "", 1), i),
        None => (insert.to_string(), insert.len()),
    };
    text.replace_range(range.clone(), &insert);
    let at = text[..range.start + cursor].chars().count();
    place_cursor(ui.ctx(), id, at);
}

/// Focuses an editor with the cursor before character `at`.
pub fn place_cursor(ctx: &egui::Context, id: egui::Id, at: usize) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(at))));
    state.store(ctx, id);
    ctx.memory_mut(|m| m.request_focus(id));
}

fn kind_tag(k: ItemKind) -> &'static str {
    match k {
        ItemKind::Snippet => "snip",
        ItemKind::Column => "col",
        ItemKind::Table => "table",
        ItemKind::View => "view",
        ItemKind::Schema => "schema",
        ItemKind::Function => "fn",
        ItemKind::Keyword => "kw",
    }
}

/// [`editor`] with autocomplete from `catalog`, which is only built while the popup is open.
#[allow(clippy::too_many_arguments)]
pub fn editor_with_completion(
    ui: &mut Ui,
    id: egui::Id,
    text: &mut String,
    kind: DbKind,
    min_height: f32,
    ac: &mut Autocomplete,
    catalog: &dyn Fn() -> Catalog,
    marks: &Marks,
) -> EditorOutput {
    let focused = ui.memory(|m| m.has_focus(id));
    if !focused {
        ac.open = false;
    }
    // Keys the popup takes before the text edit sees them.
    let items = ac.shown.as_ref().map_or(0, |c| c.items.len());
    if focused && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Space)) {
        ac.open = true;
        ac.explicit = true;
        ac.selected = 0;
    }
    if ac.open && items > 0 && focused {
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
            ac.selected = (ac.selected + 1) % items;
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
            ac.selected = (ac.selected + items - 1) % items;
        }
        if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            ac.open = false;
        }
        let enter =
            ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab));
        if enter
            && ac.open
            && let Some(c) = ac.shown.take()
        {
            let item = &c.items[ac.selected.min(c.items.len() - 1)];
            accept(ui, id, text, c.range.clone(), &item.insert);
            ac.open = false;
        }
    }

    let before = text.len();
    let out = show(ui, id, text, kind, min_height, marks);
    let changed = text.len() != before || out.response.changed();
    let cursor = out.cursor_range.filter(|r| r.primary == r.secondary).map(|r| byte_offset(text, r.primary));
    if changed && let Some(at) = cursor {
        // Typing a name or a `.` opens the popup; anything else closes it.
        let last = text[..at].chars().last();
        let typed = last.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        if typed && !ac.open {
            ac.open = true;
            ac.explicit = false;
            ac.selected = 0;
        } else if !typed {
            ac.open = false;
        }
    }
    let mut result = EditorOutput { selection: out.selection.clone(), wants: Vec::new(), want_schemas: Vec::new() };
    let (Some(at), true) = (cursor, ac.open) else {
        ac.shown = None;
        return result;
    };
    let Some(c) = complete::complete(text, at, kind, &catalog(), ac.explicit) else {
        ac.open = false;
        ac.shown = None;
        return result;
    };
    result.wants = c.wants.clone();
    result.want_schemas = c.want_schemas.clone();
    if c.items.is_empty() {
        ac.shown = Some(c);
        return result;
    }
    ac.selected = ac.selected.min(c.items.len() - 1);

    // The popup, under the word being typed.
    let t = Theme::get(ui.ctx());
    let char_at = text[..c.range.start].chars().count();
    let rect = out.galley.pos_from_cursor(CCursor::new(char_at)).translate(out.galley_pos.to_vec2());
    let mut picked = None;
    egui::Area::new(id.with("autocomplete"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.left_bottom() + egui::vec2(0.0, 2.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).fill(t.popover).stroke(egui::Stroke::new(1.0, t.border)).show(ui, |ui| {
                ui.set_min_width(260.0);
                let row_h = 20.0;
                egui::ScrollArea::vertical().max_height(row_h * VISIBLE as f32).show(ui, |ui| {
                    for (i, item) in c.items.iter().enumerate() {
                        let selected = i == ac.selected;
                        let bg = ui.painter().add(egui::Shape::Noop);
                        let r = ui
                            .horizontal(|ui| {
                                ui.set_min_height(row_h);
                                let tag = RichText::new(kind_tag(item.kind)).size(11.0).color(t.muted_foreground);
                                ui.add_sized([40.0, row_h], egui::Label::new(tag));
                                ui.label(RichText::new(&item.label).monospace().color(t.foreground));
                                if !item.detail.is_empty() {
                                    ui.label(RichText::new(&item.detail).size(11.0).color(t.muted_foreground));
                                }
                            })
                            .response
                            .interact(egui::Sense::click());
                        if selected {
                            let shape = egui::Shape::rect_filled(r.rect, 4.0, t.accent);
                            ui.painter().set(bg, shape);
                            r.scroll_to_me(None);
                        }
                        if r.clicked() {
                            picked = Some(i);
                        }
                    }
                });
            });
        });
    if let Some(i) = picked {
        accept(ui, id, text, c.range.clone(), &c.items[i].insert);
        ac.open = false;
        ac.shown = None;
    } else {
        ac.shown = Some(c);
    }
    result
}

struct Shown {
    response: egui::Response,
    galley: Arc<egui::Galley>,
    galley_pos: egui::Pos2,
    cursor_range: Option<CCursorRange>,
    selection: Option<Range<usize>>,
}

fn show(ui: &mut Ui, id: egui::Id, text: &mut String, kind: DbKind, min_height: f32, marks: &Marks) -> Shown {
    let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, wrap_width: f32| -> Arc<egui::Galley> {
        let mut job = highlight(ui, buf.as_str(), kind);
        mark(&mut job, marks, Theme::get(ui.ctx()).dark);
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
    if marks.reveal
        && let Some(m) = marks.current.and_then(|i| marks.all.get(i))
        && m.end <= text.len()
    {
        let (a, b) = (text[..m.start].chars().count(), text[..m.end].chars().count());
        let rect = out.galley.pos_from_cursor(CCursor::new(a)).union(out.galley.pos_from_cursor(CCursor::new(b)));
        ui.scroll_to_rect(rect.translate(out.galley_pos.to_vec2()).expand(24.0), None);
    }
    let selection = out.cursor_range.map(|r| {
        let a = byte_offset(text, r.primary);
        let b = byte_offset(text, r.secondary);
        a.min(b)..a.max(b)
    });
    Shown {
        response: out.response.response,
        galley: out.galley,
        galley_pos: out.galley_pos,
        cursor_range: out.cursor_range,
        selection,
    }
}

/// An editable SQL area filling the available width, `rows` lines tall at least.
pub fn editor(ui: &mut Ui, id: egui::Id, text: &mut String, kind: DbKind, min_height: f32) -> EditorOutput {
    let out = show(ui, id, text, kind, min_height, &Marks::default());
    EditorOutput { selection: out.selection, wants: Vec::new(), want_schemas: Vec::new() }
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

/// The matches of the find bar that the editor highlights.
#[derive(Default)]
pub struct Marks {
    pub all: Vec<Range<usize>>,
    /// Index of the current match, drawn stronger.
    pub current: Option<usize>,
    /// Scroll the editor to the current match.
    pub reveal: bool,
}

/// Gives the text of `marks` a background, splitting the sections of `job` at their edges.
fn mark(job: &mut LayoutJob, marks: &Marks, dark: bool) {
    if marks.all.is_empty() {
        return;
    }
    let (all, current) = if dark {
        (Color32::from_rgba_unmultiplied(255, 200, 0, 60), Color32::from_rgba_unmultiplied(255, 140, 0, 150))
    } else {
        (Color32::from_rgba_unmultiplied(255, 213, 0, 110), Color32::from_rgba_unmultiplied(255, 150, 0, 170))
    };
    let mut sections = Vec::with_capacity(job.sections.len() + marks.all.len() * 2);
    for sec in std::mem::take(&mut job.sections) {
        let r: Range<usize> = sec.byte_range.start.into()..sec.byte_range.end.into();
        let mut cuts = vec![r.start, r.end];
        for m in marks.all.iter().filter(|m| m.start < r.end && m.end > r.start) {
            cuts.extend([m.start.max(r.start), m.end.min(r.end)]);
        }
        cuts.sort_unstable();
        cuts.dedup();
        for (i, w) in cuts.windows(2).enumerate() {
            let mut format = sec.format.clone();
            if let Some(k) = marks.all.iter().position(|m| m.start <= w[0] && w[1] <= m.end) {
                format.background = if marks.current == Some(k) { current } else { all };
            }
            let leading_space = if i == 0 { sec.leading_space } else { 0.0 };
            sections.push(LayoutSection { leading_space, byte_range: w[0].into()..w[1].into(), format });
        }
    }
    job.sections = sections;
}

/// Find and replace in an editor: Ctrl+F finds, Ctrl+R (or Ctrl+H) also replaces.
#[derive(Default)]
pub struct FindBar {
    pub open: bool,
    /// The replace row is shown.
    pub replace: bool,
    pub query: Query,
    pub replacement: String,
    /// Index of the current match.
    pub current: Option<usize>,
    /// Focus the find input on the next frame.
    focus: bool,
    reveal: bool,
}

impl FindBar {
    /// Opens the bar, looking for `selected` when it is a piece of one line.
    pub fn show(&mut self, replace: bool, selected: Option<&str>) {
        self.open = true;
        self.replace = replace;
        if let Some(s) = selected.filter(|s| !s.trim().is_empty() && !s.contains('\n')) {
            self.query.text = s.to_string();
        }
        self.focus = true;
        self.current = None;
    }

    pub fn matches(&self, text: &str) -> Vec<Range<usize>> {
        find::find_all(text, &self.query)
    }

    /// Goes to the next (or previous) match from byte `from`.
    pub fn step(&mut self, text: &str, from: usize, backwards: bool) {
        self.current = find::next_match(&self.matches(text), from, backwards);
        self.reveal = true;
    }

    /// Goes to the match after (or before) the current one.
    fn step_from_current(&mut self, text: &str, cursor: usize, backwards: bool) {
        let all = self.matches(text);
        let from = match self.current.and_then(|i| all.get(i)) {
            Some(m) if backwards => m.start,
            Some(m) => m.end.max(m.start + 1),
            None => cursor,
        };
        self.step(text, from, backwards);
    }

    /// Replaces the current match (or goes to the first one) and moves to the next.
    pub fn replace_current(&mut self, text: &mut String, cursor: usize) {
        let all = self.matches(text);
        let Some(m) = self.current.and_then(|i| all.get(i)).cloned() else {
            self.step(text, cursor, false);
            return;
        };
        text.replace_range(m.clone(), &self.replacement);
        self.step(text, m.start + self.replacement.len(), false);
    }

    /// Replaces every match; returns how many there were.
    pub fn replace_all(&mut self, text: &mut String) -> usize {
        let (out, n) = find::replace_all(text, &self.query, &self.replacement);
        if n > 0 {
            *text = out;
        }
        self.current = None;
        n
    }

    /// Closes the bar and puts the editor's cursor on the current match.
    pub fn close(&mut self, ctx: &egui::Context, editor: egui::Id, text: &str) {
        self.open = false;
        match self.current.and_then(|i| self.matches(text).get(i).cloned()) {
            Some(m) => place_cursor(ctx, editor, text[..m.start].chars().count()),
            None => ctx.memory_mut(|m| m.request_focus(editor)),
        }
    }

    /// Draws the bar above an editor whose cursor is at byte `cursor`, and
    /// returns what the editor highlights. Replacing changes `text`.
    pub fn ui(&mut self, ui: &mut Ui, editor: egui::Id, text: &mut String, cursor: usize) -> Marks {
        if !self.open {
            return Marks::default();
        }
        let t = Theme::get(ui.ctx());
        let all = self.matches(text);
        if self.current.is_some_and(|i| i >= all.len()) {
            self.current = None;
        }
        let mut close = false;
        ui.horizontal(|ui| {
            let r = ui.add(Input::new(&mut self.query.text).placeholder(tr("Find")).width(240.0));
            if std::mem::take(&mut self.focus) {
                r.request_focus();
            }
            let mut changed = r.changed();
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if enter {
                let back = ui.input(|i| i.modifiers.shift);
                self.step_from_current(text, cursor, back);
                r.request_focus();
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            changed |= ui
                .add(Toggle::new(&mut self.query.match_case, "Aa").outline())
                .on_hover_text(tr("Match case"))
                .changed();
            changed |= ui
                .add(Toggle::new(&mut self.query.whole_word, "W").outline())
                .on_hover_text(tr("Whole words"))
                .changed();
            if changed {
                self.step(text, cursor, false);
            }
            let all = self.matches(text);
            let count = match (self.current, all.len()) {
                (_, 0) if !self.query.text.is_empty() => tr("No matches").to_string(),
                (_, 0) => String::new(),
                (Some(i), n) => trf("{i} of {n}", &[("i", &(i + 1)), ("n", &n)]),
                (None, n) => trf("{n} matches", &[("n", &n)]),
            };
            ui.label(RichText::new(count).color(t.muted_foreground));
            let prev = ui.add_enabled(!all.is_empty(), Button::outline("⏶").size(ButtonSize::Sm));
            if prev.on_hover_text(tr("Previous match (Shift+Enter)")).clicked() {
                self.step_from_current(text, cursor, true);
            }
            let next = ui.add_enabled(!all.is_empty(), Button::outline("⏷").size(ButtonSize::Sm));
            if next.on_hover_text(tr("Next match (Enter)")).clicked() {
                self.step_from_current(text, cursor, false);
            }
            let more = if self.replace { "⏶" } else { "⏷" };
            if ui.add(Button::ghost(more).size(ButtonSize::Sm)).on_hover_text(tr("Replace (Ctrl+R)")).clicked() {
                self.replace = !self.replace;
            }
            if ui.add(Button::ghost("×").size(ButtonSize::Sm)).on_hover_text(tr("Close (Escape)")).clicked() {
                close = true;
            }
        });
        if self.replace {
            ui.horizontal(|ui| {
                let r = ui.add(Input::new(&mut self.replacement).placeholder(tr("Replace with")).width(240.0));
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.replace_current(text, cursor);
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
                let any = !self.matches(text).is_empty();
                if ui.add_enabled(any, Button::outline(tr("Replace")).size(ButtonSize::Sm)).clicked() {
                    self.replace_current(text, cursor);
                }
                if ui.add_enabled(any, Button::outline(tr("Replace all")).size(ButtonSize::Sm)).clicked() {
                    let n = self.replace_all(text);
                    shadcn_egui::toast(ui.ctx(), trf("{n} replaced", &[("n", &n)]), None);
                }
            });
        }
        ui.add_space(4.0);
        if close {
            self.close(ui.ctx(), editor, text);
            return Marks::default();
        }
        Marks { all: self.matches(text), current: self.current, reveal: std::mem::take(&mut self.reveal) }
    }
}
