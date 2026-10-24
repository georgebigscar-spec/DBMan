//! A pivot table of a console result: the columns to group rows by, spread
//! across and aggregate, picked above the table.

use dbman_core::QueryResult;
use dbman_core::pivot::{self, Agg, PivotSpec};
use egui::{RichText, Ui};
use shadcn_egui::{Column, DataTable, Select, Theme};

use crate::i18n::{tr, trf};

/// A select of the result's columns with "none" first; `None` is none.
fn column_select(ui: &mut Ui, names: &[&str], none: &str, value: &mut Option<usize>, width: f32) -> bool {
    let options: Vec<&str> = std::iter::once(none).chain(names.iter().copied()).collect();
    let mut idx = Some(value.map_or(0, |v| v + 1));
    ui.add(Select::new(&mut idx, &options).width(width));
    let picked = idx.and_then(|i| i.checked_sub(1));
    let changed = picked != *value;
    *value = picked;
    changed
}

/// The pivot's settings: rows (two levels), columns, value and aggregate.
pub fn controls(ui: &mut Ui, result: &QueryResult, spec: &mut PivotSpec) {
    let t = Theme::get(ui.ctx());
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr("Rows")).color(t.muted_foreground));
        let mut first = spec.rows.first().copied();
        let mut second = spec.rows.get(1).copied();
        column_select(ui, &names, tr("(none)"), &mut first, 150.0);
        if first.is_some() {
            column_select(ui, &names, tr("(none)"), &mut second, 150.0);
        }
        spec.rows = first.into_iter().chain(second.filter(|s| Some(*s) != first)).collect();
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Columns")).color(t.muted_foreground));
        column_select(ui, &names, tr("(none)"), &mut spec.column, 150.0);
        ui.add_space(8.0);
        ui.label(RichText::new(tr("Value")).color(t.muted_foreground));
        column_select(ui, &names, tr("(rows)"), &mut spec.value, 150.0);
        if spec.value.is_some() {
            let aggs: Vec<&str> = Agg::ALL.iter().map(|a| tr(a.label())).collect();
            let mut a = Agg::ALL.iter().position(|a| *a == spec.agg);
            ui.add(Select::new(&mut a, &aggs).width(150.0));
            if let Some(a) = a {
                spec.agg = Agg::ALL[a];
            }
        }
    });
}

/// Draws the pivot table of `result`.
pub fn show(ui: &mut Ui, id: egui::Id, result: &QueryResult, spec: &PivotSpec) {
    let t = Theme::get(ui.ctx());
    let mut table = pivot::compute(result, spec);
    if table.rows.is_empty() {
        ui.label(RichText::new(tr("No rows to group.")).color(t.muted_foreground));
        return;
    }
    if spec.column.is_some()
        && let Some(last) = table.header.last_mut()
    {
        *last = tr("Total").to_string();
    }
    if table.total_row
        && let Some(first) = table.rows.last_mut().and_then(|r| r.first_mut())
    {
        *first = tr("Total").to_string();
    }
    let mut notes = Vec::new();
    if result.truncated {
        notes.push(trf("Computed over the {n} rows fetched, not the whole result.", &[("n", &result.rows.len())]));
    }
    if table.columns_cut {
        notes.push(trf("Only the first {n} column values are shown.", &[("n", &pivot::MAX_COLUMNS)]));
    }
    if spec.value.is_some() && spec.agg.numeric() && spec.value.is_some_and(|v| !result.columns[v].numeric) {
        notes.push(tr("Values that are not numbers are skipped.").to_string());
    }
    for n in notes {
        ui.label(RichText::new(n).size(12.0).color(t.muted_foreground));
    }
    let columns: Vec<Column> = table
        .header
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let c = Column::new(h.clone()).sortable();
            if i >= table.label_columns { c.right() } else { c }
        })
        .collect();
    // A new layout is a new table: its sort and widths start over.
    let key = (spec.rows.clone(), spec.column, spec.value, spec.agg.label());
    DataTable::new(id.with(key), columns)
        .filter(tr("Filter rows..."))
        .row_height(28.0)
        .resizable()
        .max_height(ui.available_height() - 40.0)
        .show(ui, &table.rows);
}
