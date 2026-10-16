//! Result grid: a shadcn `DataTable` over a `QueryResult`, with NULLs shown
//! muted and a horizontal scroll bar for wide results.

use dbman_core::export::{Format, export};
use dbman_core::sql::Dialect;
use dbman_core::{Cell, DbKind, ObjectPath, QueryResult};
use egui::{RichText, Ui};
use shadcn_egui::{
    Button, ButtonSize, Column, DataTable, Theme, dropdown_menu, menu_item, menu_label, menu_separator, toast,
};

use crate::files;
use crate::i18n::{tr, trf};

pub struct Grid {
    pub result: QueryResult,
    text: Vec<Vec<String>>,
}

pub const MIN_COLUMN_WIDTH: f32 = 140.0;

impl Grid {
    pub fn new(result: QueryResult) -> Self {
        let text = result.rows.iter().map(|r| r.iter().map(|c| c.display().into_owned()).collect()).collect();
        Self { result, text }
    }

    pub fn show(&self, ui: &mut Ui, id: egui::Id) {
        if self.result.columns.is_empty() {
            return;
        }
        let t = Theme::get(ui.ctx());
        let width = (self.result.columns.len() as f32 * MIN_COLUMN_WIDTH).max(ui.available_width());
        let height = (ui.available_height() - 110.0).max(120.0);
        let rows = &self.result.rows;
        egui::ScrollArea::horizontal().id_salt(id.with("h")).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_width(width);
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
                .max_height(height)
                .row_height(28.0)
                .cell_ui(|ui, row, col| {
                    if rows.get(row).and_then(|r| r.get(col)).is_some_and(Cell::is_null) {
                        ui.label(RichText::new("NULL").italics().color(t.muted_foreground));
                        true
                    } else {
                        false
                    }
                })
                .show(ui, &self.text);
        });
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
        for f in Format::ALL {
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
                files::save_file(ui.ctx(), dialog, move || export(&result, f, Dialect(kind), table.as_ref()));
            }
        }
    });
    export_all
}
