//! Result grid: a shadcn `DataTable` over a `QueryResult`, with NULLs shown
//! muted. Columns are at least `MIN_COLUMN_WIDTH` wide and can be resized;
//! wide results scroll horizontally inside the table.

use dbman_core::export::{Format, export};
use dbman_core::sql::Dialect;
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
}

pub const MIN_COLUMN_WIDTH: f32 = 140.0;

impl Grid {
    pub fn new(result: QueryResult) -> Self {
        let text = result.rows.iter().map(|r| r.iter().map(|c| c.display().into_owned()).collect()).collect();
        Self { result, text, current: None }
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
        DataTable::new(id, columns)
            .filter(tr("Filter rows..."))
            .min_column_width(MIN_COLUMN_WIDTH)
            .resizable()
            .max_height(height)
            .row_height(28.0)
            .cell_ui(|ui, row, col| {
                let rect = ui.max_rect().expand2(egui::vec2(12.0, 0.0));
                let r = ui.interact(rect, id.with(("cell", row, col)), egui::Sense::click());
                if r.clicked() {
                    *current = Some((row, col));
                }
                if r.double_clicked() {
                    opened = true;
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
        opened
    }
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
