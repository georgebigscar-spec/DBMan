//! Result grid: a shadcn `DataTable` over a `QueryResult`, with NULLs shown
//! muted and a horizontal scroll bar for wide results.

use dbman_core::{Cell, QueryResult};
use egui::{RichText, Ui};
use shadcn_egui::{Column, DataTable, Theme};

pub struct Grid {
    pub result: QueryResult,
    text: Vec<Vec<String>>,
}

const MIN_COLUMN_WIDTH: f32 = 140.0;

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
                .filter("Filter rows...")
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
