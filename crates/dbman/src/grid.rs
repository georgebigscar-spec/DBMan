//! Result grid: a shadcn `DataTable` over a `QueryResult`, with NULLs shown
//! muted. Columns are at least `MIN_COLUMN_WIDTH` wide and can be resized;
//! wide results scroll horizontally inside the table.

use std::collections::BTreeSet;
use std::sync::Arc;

use dbman_core::export::{Format, export};
use dbman_core::sql::Dialect;
use dbman_core::value::Stats;
use dbman_core::{Cell, DbKind, ObjectPath, QueryResult};
use egui::{RichText, Ui};
use shadcn_egui::{
    Button, ButtonSize, Column, DataTable, Theme, TooltipExt, dropdown_menu, menu_item, menu_label, menu_separator,
    toast,
};

use crate::files;
use crate::i18n::{tr, trf};
use crate::value_panel::Current;

pub struct Grid {
    pub result: QueryResult,
    text: Vec<Vec<String>>,
    /// The clicked cell (row, column), shown in the value panel.
    pub current: Option<(usize, usize)>,
    pub selection: CellSelection,
}

pub const MIN_COLUMN_WIDTH: f32 = 140.0;

impl Grid {
    pub fn new(result: QueryResult) -> Self {
        let text = result.rows.iter().map(|r| r.iter().map(|c| c.display().into_owned()).collect()).collect();
        Self { result, text, current: None, selection: CellSelection::default() }
    }

    /// The current cell for the value panel.
    pub fn current_value(&self) -> Option<Current<'_>> {
        let (row, col) = self.current?;
        Some(Current { column: self.result.columns.get(col)?, row, cell: self.result.rows.get(row)?.get(col)? })
    }

    /// Draws the grid; true when a cell was double-clicked.
    pub fn show(&mut self, ui: &mut Ui, id: egui::Id) -> bool {
        if self.result.columns.is_empty() {
            return false;
        }
        let t = Theme::get(ui.ctx());
        let height = (ui.available_height() - 110.0).max(120.0);
        let rows = &self.result.rows;
        let text = &self.text;
        let current = &mut self.current;
        let selection = &mut self.selection;
        let mut clicked = None;
        let mut opened = false;
        let columns = self
            .result
            .columns
            .iter()
            .map(|c| {
                let col = Column::new(c.name.clone()).sortable();
                if c.numeric { col.right() } else { col }
            })
            .collect();
        let shown = DataTable::new(id, columns)
            .filter(tr("Filter rows..."))
            .min_column_width(MIN_COLUMN_WIDTH)
            .resizable()
            .max_height(height)
            .row_height(28.0)
            .cell_ui(|ui, row, col| {
                let rect = ui.max_rect().expand2(egui::vec2(12.0, 0.0));
                let r = ui.interact(rect, id.with(("cell", row, col)), egui::Sense::click());
                if r.clicked() {
                    clicked = Some((row, col, ui.input(|i| i.modifiers)));
                }
                if r.double_clicked() {
                    opened = true;
                }
                if selection.contains(row, col) {
                    selection_fill(ui, rect, &t);
                }
                if *current == Some((row, col)) {
                    current_outline(ui, rect, &t);
                }
                if rows.get(row).and_then(|r| r.get(col)).is_some_and(Cell::is_null) {
                    ui.label(RichText::new("NULL").italics().color(t.muted_foreground));
                } else {
                    let s = text.get(row).and_then(|r| r.get(col)).map(String::as_str).unwrap_or("");
                    ui.add(egui::Label::new(RichText::new(s).color(t.foreground)).truncate().selectable(false));
                }
                true
            })
            .show(ui, text);
        self.selection.set_order(shown.visible);
        if let Some((row, col, modifiers)) = clicked {
            self.current = Some((row, col));
            self.selection.click(row, col, modifiers);
        }
        let result = &self.result;
        self.selection.footer(ui, result.columns.len(), |row, col| result.rows.get(row)?.get(col).cloned());
        opened
    }
}

/// Cells picked in a grid with click, Ctrl+click and Shift+click. Shift
/// takes the rectangle between the last clicked cell and this one, in the
/// order the rows are shown.
#[derive(Default)]
pub struct CellSelection {
    pub cells: BTreeSet<(usize, usize)>,
    anchor: Option<(usize, usize)>,
    /// Rows in the order the table showed them last frame.
    order: Arc<Vec<usize>>,
}

impl CellSelection {
    pub fn contains(&self, row: usize, col: usize) -> bool {
        self.cells.len() > 1 && self.cells.contains(&(row, col))
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.anchor = None;
    }

    /// Sets the rows in the order the table shows them.
    pub fn set_order(&mut self, order: Arc<Vec<usize>>) {
        self.order = order;
    }

    pub fn click(&mut self, row: usize, col: usize, modifiers: egui::Modifiers) {
        match self.anchor {
            Some((arow, acol)) if modifiers.shift => {
                let pos = |r: usize| self.order.iter().position(|&x| x == r);
                let rows: Vec<usize> = match (pos(arow), pos(row)) {
                    (Some(a), Some(b)) => self.order[a.min(b)..=a.max(b)].to_vec(),
                    _ => (arow.min(row)..=arow.max(row)).collect(),
                };
                if !modifiers.command {
                    self.cells.clear();
                }
                for r in rows {
                    for c in acol.min(col)..=acol.max(col) {
                        self.cells.insert((r, c));
                    }
                }
            }
            _ if modifiers.command => {
                if !self.cells.remove(&(row, col)) {
                    self.cells.insert((row, col));
                }
                self.anchor = Some((row, col));
            }
            _ => {
                self.cells = BTreeSet::from([(row, col)]);
                self.anchor = Some((row, col));
            }
        }
    }

    /// The selected cells as tab-separated text: one line per row, in the
    /// order shown, with only the selected columns.
    pub fn tsv(&self, cell: impl Fn(usize, usize) -> Option<Cell>) -> String {
        let cols: BTreeSet<usize> = self.cells.iter().map(|&(_, c)| c).collect();
        let mut rows: Vec<usize> = self.cells.iter().map(|&(r, _)| r).collect();
        rows.dedup();
        let pos = |r: &usize| self.order.iter().position(|x| x == r).unwrap_or(usize::MAX);
        rows.sort_by_key(|r| (pos(r), *r));
        rows.dedup();
        rows.iter()
            .map(|&r| {
                cols.iter()
                    .map(|&c| match cell(r, c).filter(|_| self.cells.contains(&(r, c))) {
                        Some(v) => v.as_text().map(|t| t.replace(['\t', '\n'], " ")).unwrap_or_default(),
                        None => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The count, sum, average, smallest and largest of the selected cells
    /// under the grid, when more than one cell is selected; Ctrl+C copies them.
    pub fn footer(&mut self, ui: &mut Ui, columns: usize, cell: impl Fn(usize, usize) -> Option<Cell> + Copy) {
        self.cells.retain(|&(r, c)| c < columns && cell(r, c).is_some());
        if self.cells.len() < 2 {
            return;
        }
        let copy = ui.memory(|m| m.focused().is_none()) && ui.input(|i| i.events.contains(&egui::Event::Copy));
        if copy {
            ui.ctx().copy_text(self.tsv(cell));
            toast(ui.ctx(), trf("Copied {n} cells", &[("n", &self.cells.len())]), None);
        }
        let cells: Vec<Cell> = self.cells.iter().filter_map(|&(r, c)| cell(r, c)).collect();
        let stats = Stats::of(&cells);
        let t = Theme::get(ui.ctx());
        ui.horizontal(|ui| {
            let part = |ui: &mut Ui, name: String, value: String| {
                ui.label(RichText::new(name).color(t.muted_foreground));
                ui.label(RichText::new(value).strong().color(t.foreground));
                ui.add_space(8.0);
            };
            part(ui, tr("Count").into(), stats.count.to_string());
            if stats.nulls > 0 {
                part(ui, "NULL".into(), stats.nulls.to_string());
            }
            if let Some([sum, avg, min, max]) = stats.numbers() {
                part(ui, tr("Sum").into(), sum);
                part(ui, tr("Average").into(), avg);
                part(ui, tr("Min").into(), min);
                part(ui, tr("Max").into(), max);
            }
            let copy = Button::ghost(tr("Copy")).size(ButtonSize::Sm);
            if ui.add(copy).tooltip(tr("Copy the selected cells as tab-separated text (Ctrl+C)")).clicked() {
                ui.ctx().copy_text(self.tsv(cell));
                toast(ui.ctx(), trf("Copied {n} cells", &[("n", &self.cells.len())]), None);
            }
        });
    }
}

/// The fill behind a selected cell.
pub fn selection_fill(ui: &Ui, rect: egui::Rect, t: &Theme) {
    ui.painter().rect_filled(rect, 0.0, t.primary.gamma_multiply(if t.dark { 0.22 } else { 0.12 }));
}

/// The ring around the current cell.
pub fn current_outline(ui: &Ui, rect: egui::Rect, t: &Theme) {
    ui.painter().rect_stroke(rect.shrink(1.0), 3.0, egui::Stroke::new(1.5, t.ring), egui::StrokeKind::Inside);
}

/// The "Value" toggle above a grid.
pub fn value_button(ui: &mut Ui, open: &mut bool) {
    let b = if *open { Button::secondary(tr("Value")) } else { Button::outline(tr("Value")) };
    if ui.add(b.size(ButtonSize::Sm)).tooltip(tr("Show the whole value of the clicked cell")).clicked() {
        *open = !*open;
    }
}

/// "Export" button with a menu: copy the rows to the clipboard or save them to
/// a file as CSV, TSV, JSON, SQL INSERTs or Markdown. `table` is the target of
/// the INSERT statements. With `all_rows`, the menu also offers to export every
/// row (not just the ones shown); returns true when that was chosen.
pub fn export_menu(
    ui: &mut Ui,
    result: &QueryResult,
    kind: DbKind,
    table: Option<&ObjectPath>,
    all_rows: bool,
) -> bool {
    let trigger = ui.add(Button::outline(tr("Export")).size(ButtonSize::Sm));
    let render = |f: Format| export(result, f, Dialect(kind), table);
    let mut export_all = false;
    dropdown_menu(&trigger, 240.0, |ui| {
        if all_rows {
            if menu_item(ui, tr("All rows to a file…"), None).clicked() {
                export_all = true;
            }
            menu_separator(ui);
        }
        menu_label(ui, tr("Copy to clipboard"));
        for f in Format::TEXT {
            if menu_item(ui, tr(f.label()), None).clicked() {
                ui.ctx().copy_text(render(f));
                toast(
                    ui.ctx(),
                    trf("Copied {n} rows as {format}", &[("n", &result.rows.len()), ("format", &tr(f.label()))]),
                    None,
                );
            }
        }
        menu_separator(ui);
        menu_label(ui, tr("Save to file"));
        for f in Format::ALL {
            if menu_item(ui, &format!("{}…", tr(f.label())), None).clicked() {
                let name = format!("{}.{}", table.map_or("result", |t| t.name.as_str()), f.extension());
                let dialog =
                    rfd::AsyncFileDialog::new().set_file_name(name).add_filter(tr(f.label()), &[f.extension()]);
                let (result, table) = (result.clone(), table.cloned());
                files::save_to(ui.ctx(), dialog, move |path| {
                    dbman_drivers::files::write_result(&result, path, f, Dialect(kind), table.as_ref())
                        .map_err(|e| e.message)
                });
            }
        }
    });
    export_all
}
