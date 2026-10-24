//! Table data tab: rows page by page, with a WHERE filter and ORDER BY,
//! in-place editing (change cells, add and delete rows, paste cells copied
//! from a spreadsheet) written back in one transaction, and a record view
//! that shows one row as a column of fields.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, Connection, ConnectionConfig, DbKind, DbObject, ObjectKind, QueryResult, StatementOutcome, TableDetails, dml,
    paste,
};
use egui::{Color32, Key, RichText, Sense, Ui};
use shadcn_egui::{
    Alert, Button, ButtonSize, Column, DataTable, Dialog, Input, Theme, TooltipExt, menu_item, menu_separator, toast,
};

use crate::events::{Event, TabEvent, TabId, Tasks, timed};
use crate::exec;
use crate::explorer::{Action, ConnState, Load};
use crate::grid;
use crate::i18n::{tr, trf};
use crate::profile_view::ProfileDialog;
use crate::sql_editor;
use crate::tabs::{TabCtx, error_alert, format_elapsed, not_connected, outcome_summary, toolbar_button};
use crate::value_panel::{Current, ValuePanel};
use dbman_drivers::transfer::ExportSource;
use tokio::sync::OnceCell;

/// Pending changes to the rows on screen.
#[derive(Default)]
struct Edits {
    /// New values of existing rows, by (row, column).
    changed: BTreeMap<(usize, usize), Cell>,
    deleted: BTreeSet<usize>,
    /// New rows; `None` leaves the column to its default.
    inserted: Vec<Vec<Option<Cell>>>,
}

impl Edits {
    fn count(&self) -> usize {
        let rows: BTreeSet<usize> =
            self.changed.keys().map(|(r, _)| *r).filter(|r| !self.deleted.contains(r)).collect();
        rows.len() + self.deleted.len() + self.inserted.len()
    }

    fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

struct Editing {
    row: usize,
    col: usize,
    text: String,
    focused: bool,
}

/// What the grid asked for while drawing.
enum GridCmd {
    StartEdit(usize, usize),
    Commit,
    Cancel,
    SetNull(usize, usize),
    Revert(usize, usize),
    Delete(usize),
    Copy(String),
    /// A cell was clicked: show it in the value panel.
    Current(usize, usize),
    Profile(usize),
}

pub struct DataView {
    pub object: DbObject,
    kind: DbKind,
    /// The connection is read-only: rows cannot be edited.
    pub read_only: bool,
    filter: String,
    order_by: String,
    page: usize,
    rows: Load<QueryResult>,
    /// The tab's own session, so a slow page never holds up the explorer and
    /// Stop can cancel it without touching other queries. Opened on first load.
    session: Arc<OnceCell<Arc<dyn Connection>>>,
    /// Numbers page loads, so a stopped or superseded load's late answer is dropped.
    page_seq: u64,
    load_started: Option<Instant>,
    summary: String,
    loaded_once: bool,
    details: Load<TableDetails>,
    edits: Edits,
    /// Text of every row on screen (loaded and inserted), rebuilt after edits.
    display: Vec<Vec<String>>,
    version: u64,
    selected: HashSet<usize>,
    editing: Option<Editing>,
    submitting: bool,
    submit_error: Option<String>,
    preview: Option<String>,
    /// The clicked cell (row, column).
    current: Option<(usize, usize)>,
    cells: grid::CellSelection,
    value_panel: ValuePanel,
    /// One row at a time, its columns down the screen.
    pub record: bool,
    pub profile: Option<ProfileDialog>,
    /// "Profile column" was picked in a cell's menu: the column to profile.
    profile_asked: Option<usize>,
}

impl DataView {
    /// Sets the `WHERE` filter, as typed in the toolbar, before the first load.
    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
    }

    /// The `WHERE` filter.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn new(object: DbObject, kind: DbKind) -> Self {
        Self {
            object,
            kind,
            read_only: false,
            filter: String::new(),
            order_by: String::new(),
            page: 0,
            rows: Load::Idle,
            session: Arc::new(OnceCell::new()),
            page_seq: 0,
            load_started: None,
            summary: String::new(),
            loaded_once: false,
            details: Load::Idle,
            edits: Edits::default(),
            display: Vec::new(),
            version: 0,
            selected: HashSet::new(),
            editing: None,
            submitting: false,
            submit_error: None,
            preview: None,
            current: None,
            cells: grid::CellSelection::default(),
            value_panel: ValuePanel::default(),
            record: false,
            profile: None,
            profile_asked: None,
        }
    }

    pub fn on_event(&mut self, ev: TabEvent, ctx: &egui::Context) {
        match ev {
            TabEvent::Page { seq, elapsed, result } => {
                if seq != self.page_seq {
                    return;
                }
                self.load_started = None;
                match result {
                    Ok(StatementOutcome::Rows(r)) => {
                        self.summary = outcome_summary(&StatementOutcome::Rows(r.clone()), elapsed);
                        self.rows = Load::Ready(r);
                    }
                    Ok(other) => {
                        self.rows =
                            Load::Failed(trf("unexpected result: {result}", &[("result", &format!("{other:?}"))]))
                    }
                    Err(e) => self.rows = Load::Failed(e.message),
                }
                self.edits = Edits::default();
                self.selected.clear();
                self.editing = None;
                self.current = None;
                self.cells.clear();
                self.rebuild();
            }
            TabEvent::Details(r) => self.details = Load::from_result(r),
            TabEvent::Profile(column, r) => {
                if let Some(p) = self.profile.as_mut().filter(|p| p.column == column) {
                    p.result = Load::from_result(r);
                }
            }
            TabEvent::Submitted(r) => {
                self.submitting = false;
                match r {
                    Ok(n) => {
                        self.submit_error = None;
                        toast(ctx, tr("Changes saved"), Some(&trf("Statements committed: {n}", &[("n", &n)])));
                        self.loaded_once = false; // reload on the next frame
                    }
                    Err(e) => self.submit_error = Some(e.message),
                }
            }
            _ => {}
        }
    }

    /// Opens the profile of column `col`, counted over the rows the filter selects.
    pub fn open_profile(&mut self, tasks: &Tasks, tab: TabId, session: Arc<dyn Connection>, col: usize) {
        let Some(meta) = self.rows.ready().and_then(|r| r.columns.get(col)) else { return };
        let (column, numeric) = (meta.name.clone(), meta.numeric);
        self.profile = Some(ProfileDialog::new(column.clone()));
        let (path, filter) = (self.object.path.clone(), self.filter.clone());
        tasks.spawn(async move {
            let result = dbman_core::profile::run(session.as_ref(), &path, &column, numeric, &filter).await;
            Event::Tab { tab, event: TabEvent::Profile(column, result) }
        });
    }

    /// The whole table, or the rows the filter and ordering select.
    fn export_source(&self) -> ExportSource {
        let path = &self.object.path;
        if self.filter.trim().is_empty() && self.order_by.trim().is_empty() {
            return ExportSource::Table(path.clone());
        }
        let d = Dialect(self.kind);
        let mut sql = format!("SELECT * FROM {}", d.qualified(path));
        if !self.filter.trim().is_empty() {
            sql.push_str(&format!(" WHERE {}", self.filter.trim()));
        }
        if !self.order_by.trim().is_empty() {
            sql.push_str(&format!(" ORDER BY {}", self.order_by.trim()));
        }
        ExportSource::Query { database: Some(path.database.clone()).filter(|d| !d.is_empty()), sql }
    }

    /// Rows can be edited in tables whose structure is known. ClickHouse has
    /// no transactions to apply edits safely; change its data in a console.
    fn editable(&self) -> bool {
        self.kind != DbKind::ClickHouse
            && !self.read_only
            && self.object.kind == ObjectKind::Table
            && self.details.ready().is_some()
            && self.rows.ready().is_some()
    }

    fn base_len(&self) -> usize {
        self.rows.ready().map_or(0, |r| r.rows.len())
    }

    /// The current value of a cell, with edits applied; `None` for a default in a new row.
    fn value(&self, row: usize, col: usize) -> Option<Cell> {
        let base = self.base_len();
        if row < base {
            self.edits.changed.get(&(row, col)).cloned().or_else(|| self.rows.ready().map(|r| r.rows[row][col].clone()))
        } else {
            self.edits.inserted.get(row - base).and_then(|r| r[col].clone())
        }
    }

    fn rebuild(&mut self) {
        let Some(r) = self.rows.ready() else {
            self.display.clear();
            return;
        };
        let ncols = r.columns.len();
        let total = r.rows.len() + self.edits.inserted.len();
        self.display = (0..total)
            .map(|row| {
                (0..ncols).map(|c| self.value(row, c).map(|v| v.display().into_owned()).unwrap_or_default()).collect()
            })
            .collect();
        self.version += 1;
    }

    /// Key columns (indices into the result) used to find a row: the primary
    /// key, or every column if the table has none.
    fn key_columns(&self) -> (Vec<usize>, bool) {
        let (Some(r), Some(d)) = (self.rows.ready(), self.details.ready()) else { return (Vec::new(), false) };
        let pk = d.primary_key();
        let idx: Vec<usize> = pk.iter().filter_map(|k| r.columns.iter().position(|c| c.name == *k)).collect();
        if !pk.is_empty() && idx.len() == pk.len() { (idx, true) } else { ((0..r.columns.len()).collect(), false) }
    }

    /// Changes a cell (by index into the rows on screen, new rows after loaded ones).
    pub fn set_cell(&mut self, row: usize, col: usize, value: Cell) {
        self.set(row, col, value);
        self.rebuild();
    }

    /// Adds an empty row whose columns keep their defaults until edited.
    pub fn add_row(&mut self) {
        let n = self.rows.ready().map_or(0, |r| r.columns.len());
        self.edits.inserted.push(vec![None; n]);
        self.rebuild();
    }

    /// Writes cells copied from a spreadsheet (tab-separated lines) from the
    /// clicked cell to the right and down, adding rows past the last one; with
    /// no clicked cell, the lines become new rows. `NULL` sets NULL, and an
    /// empty cell leaves a new row's column to its default. Returns the rows written.
    pub fn paste(&mut self, text: &str) -> usize {
        let Some(ncols) = self.rows.ready().map(|r| r.columns.len()) else { return 0 };
        let lines = paste::parse(text);
        let base = self.base_len();
        let total = base + self.edits.inserted.len();
        let (row0, col0) = self.current.filter(|(r, c)| *r < total && *c < ncols).unwrap_or((total, 0));
        for (i, line) in lines.iter().enumerate() {
            let row = row0 + i;
            while row >= base + self.edits.inserted.len() {
                self.edits.inserted.push(vec![None; ncols]);
            }
            for (j, v) in line.iter().enumerate().take(ncols - col0) {
                let col = col0 + j;
                match v.as_str() {
                    "" if row >= base => self.edits.inserted[row - base][col] = None,
                    "NULL" => self.set(row, col, Cell::Null),
                    v => self.set(row, col, Cell::Text(v.to_string())),
                }
            }
        }
        self.rebuild();
        lines.len()
    }

    /// The row the record view shows.
    pub fn record_row(&self) -> usize {
        self.current.map_or(0, |(r, _)| r)
    }

    /// Marks rows for deletion (or restores rows already marked).
    pub fn delete_rows(&mut self, rows: &[usize]) {
        self.toggle_deleted(rows);
        self.rebuild();
    }

    /// The statements that write the pending edits.
    pub fn statements(&self) -> Vec<String> {
        let Some(r) = self.rows.ready() else { return Vec::new() };
        let d = Dialect(self.kind);
        let t = &self.object.path;
        let (keys, _) = self.key_columns();
        let key = |row: usize| -> Vec<dml::Value<'_>> {
            keys.iter().map(|&c| dml::Value::new(&r.columns[c].name, &r.rows[row][c], r.columns[c].numeric)).collect()
        };
        let mut out = Vec::new();
        for &row in &self.edits.deleted {
            out.push(dml::delete(d, t, &key(row)));
        }
        let mut by_row: BTreeMap<usize, Vec<(usize, &Cell)>> = BTreeMap::new();
        for ((row, col), cell) in &self.edits.changed {
            if !self.edits.deleted.contains(row) {
                by_row.entry(*row).or_default().push((*col, cell));
            }
        }
        for (row, cells) in by_row {
            let set: Vec<dml::Value<'_>> = cells
                .iter()
                .map(|(c, cell)| dml::Value::new(&r.columns[*c].name, cell, r.columns[*c].numeric))
                .collect();
            out.push(dml::update(d, t, &set, &key(row)));
        }
        for new in &self.edits.inserted {
            let values: Vec<dml::Value<'_>> = new
                .iter()
                .enumerate()
                .filter_map(|(c, v)| {
                    v.as_ref().map(|cell| dml::Value::new(&r.columns[c].name, cell, r.columns[c].numeric))
                })
                .collect();
            out.push(dml::insert(d, t, &values));
        }
        out
    }

    fn load(
        &mut self,
        tasks: &Tasks,
        tab: TabId,
        cfg: &ConnectionConfig,
        session: Arc<dyn Connection>,
        page_size: usize,
    ) {
        // One row more than a page tells whether there is a next page.
        let sql = Dialect(self.kind).select_page(
            &self.object.path,
            &self.filter,
            &self.order_by,
            page_size + 1,
            self.page * page_size,
        );
        let db = self.object.path.database.clone();
        self.rows = Load::Loading;
        self.loaded_once = true;
        self.page_seq += 1;
        self.load_started = Some(Instant::now());
        let seq = self.page_seq;
        let own = self.session.clone();
        let mut cfg = cfg.clone();
        if cfg.kind == DbKind::MySql {
            cfg.database = db.clone();
        }
        tasks.spawn(async move {
            let (elapsed, result) = timed(async {
                let s = own.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?;
                s.execute(Some(&db), &sql, page_size).await
            })
            .await;
            Event::Tab { tab, event: TabEvent::Page { seq, elapsed, result } }
        });
        if self.object.kind == ObjectKind::Table && !matches!(self.details, Load::Ready(_) | Load::Loading) {
            self.details = Load::Loading;
            let path = self.object.path.clone();
            let s = session.clone();
            tasks.spawn(async move { Event::Tab { tab, event: TabEvent::Details(s.table_details(&path).await) } });
        }
    }

    /// Cancels the page being loaded; the grid shows the previous page state as failed.
    fn stop(&mut self, tasks: &Tasks) {
        self.page_seq += 1;
        self.load_started = None;
        self.rows = Load::Failed(tr("Cancelled.").into());
        if let Some(s) = self.session.get().cloned() {
            // Best effort: the load's own answer is ignored because page_seq moved on.
            tasks.runtime().spawn(async move {
                let _ = s.cancel().await;
            });
        }
    }

    fn submit(&mut self, tasks: &Tasks, tab: TabId, cfg: ConnectionConfig) {
        let stmts = self.statements();
        if stmts.is_empty() {
            return;
        }
        self.submitting = true;
        self.submit_error = None;
        let db = self.object.path.database.clone();
        tasks.spawn(async move {
            let result = exec::run(cfg, db, stmts, exec::Mode::RowEdits).await;
            Event::Tab { tab, event: TabEvent::Submitted(result) }
        });
    }

    fn apply(&mut self, cmd: GridCmd, ctx: &egui::Context) {
        let base = self.base_len();
        match cmd {
            GridCmd::Current(row, col) => self.current = Some((row, col)),
            GridCmd::StartEdit(row, col) => {
                let text = self.value(row, col).and_then(|v| v.as_text().map(|t| t.into_owned())).unwrap_or_default();
                self.editing = Some(Editing { row, col, text, focused: false });
            }
            GridCmd::Cancel => self.editing = None,
            GridCmd::Commit => {
                if let Some(e) = self.editing.take() {
                    let current = self.value(e.row, e.col);
                    let unchanged =
                        current.as_ref().and_then(|c| c.as_text().map(|t| t.into_owned())) == Some(e.text.clone());
                    if !unchanged {
                        self.set(e.row, e.col, Cell::Text(e.text));
                    }
                }
            }
            GridCmd::SetNull(row, col) => self.set(row, col, Cell::Null),
            GridCmd::Revert(row, col) => {
                if row < base {
                    self.edits.changed.remove(&(row, col));
                } else if let Some(r) = self.edits.inserted.get_mut(row - base) {
                    r[col] = None;
                }
            }
            GridCmd::Delete(row) => self.toggle_deleted(&[row]),
            GridCmd::Copy(text) => ctx.copy_text(text),
            GridCmd::Profile(col) => self.profile_asked = Some(col),
        }
        self.rebuild();
    }

    fn set(&mut self, row: usize, col: usize, value: Cell) {
        let base = self.base_len();
        if row < base {
            let original = self.rows.ready().map(|r| &r.rows[row][col]);
            if original == Some(&value) {
                self.edits.changed.remove(&(row, col));
            } else {
                self.edits.changed.insert((row, col), value);
            }
        } else if let Some(r) = self.edits.inserted.get_mut(row - base) {
            r[col] = Some(value);
        }
    }

    fn toggle_deleted(&mut self, rows: &[usize]) {
        let base = self.base_len();
        let mut new_rows: Vec<usize> = rows.iter().filter(|r| **r >= base).map(|r| r - base).collect();
        new_rows.sort_unstable_by(|a, b| b.cmp(a));
        for i in new_rows {
            if i < self.edits.inserted.len() {
                self.edits.inserted.remove(i);
            }
        }
        for &r in rows.iter().filter(|r| **r < base) {
            if !self.edits.deleted.insert(r) {
                // Deleting a deleted row restores it.
                self.edits.deleted.remove(&r);
            }
        }
        self.selected.clear();
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        let Some(session) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        let cfg = cx.conn.map(|c| c.cfg.clone()).expect("a session implies a connection");
        if !self.loaded_once {
            self.load(cx.tasks, tab, &cfg, session.clone(), cx.page_size);
        }
        let dirty = !self.edits.is_empty();
        let mut reload = false;
        let mut profile = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(!dirty, Button::outline(tr("⟳ Refresh")).size(ButtonSize::Sm)).clicked() {
                reload = true;
            }
            let has_next = self.rows.ready().is_some_and(|r| r.truncated);
            if ui.add_enabled(self.page > 0 && !dirty, Button::outline("‹").size(ButtonSize::Sm)).clicked() {
                self.page -= 1;
                reload = true;
            }
            ui.label(RichText::new(trf("page {n}", &[("n", &(self.page + 1))])).color(t.muted_foreground));
            if ui.add_enabled(has_next && !dirty, Button::outline("›").size(ButtonSize::Sm)).clicked() {
                self.page += 1;
                reload = true;
            }
            ui.add_space(8.0);
            ui.label(RichText::new("WHERE").monospace().color(t.muted_foreground));
            let f = ui.add(Input::new(&mut self.filter).placeholder("id > 10").width(220.0));
            ui.label(RichText::new("ORDER BY").monospace().color(t.muted_foreground));
            let o = ui.add(Input::new(&mut self.order_by).placeholder("id DESC").width(150.0));
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            if (f.lost_focus() || o.lost_focus()) && enter && !dirty {
                self.page = 0;
                reload = true;
            }
            if let Some(r) = self.rows.ready()
                && grid::export_menu(ui, r, self.kind, Some(&self.object.path), true)
            {
                cx.actions.push(Action::ExportData(cx.conn_id, self.export_source()));
            }
            if self.rows.ready().is_some() {
                let b = if self.record { Button::secondary(tr("Record")) } else { Button::outline(tr("Record")) };
                if ui.add(b.size(ButtonSize::Sm)).tooltip(tr("Show one row with its columns down the screen")).clicked()
                {
                    self.record = !self.record;
                }
                if !self.record {
                    grid::value_button(ui, &mut self.value_panel.open);
                }
                let col = self.current.map(|(_, c)| c);
                if ui
                    .add_enabled(col.is_some(), Button::outline(tr("Profile")).size(ButtonSize::Sm))
                    .tooltip(tr(
                        "Nulls, distinct values, range and the most frequent values of the clicked cell's column",
                    ))
                    .clicked()
                    && let Some(col) = col
                {
                    profile = Some(col);
                }
            }
            // The summary gives way to the buttons when the window is narrow.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let summary = egui::Label::new(RichText::new(&self.summary).color(t.muted_foreground));
                ui.add(summary.truncate().selectable(false));
            });
        });

        let editable = self.editable();
        // Ctrl+V with no text field focused pastes copied cells into the table.
        let typing = ui.memory(|m| m.focused()).is_some_and(|f| egui::TextEdit::load_state(ui.ctx(), f).is_some());
        if editable && self.editing.is_none() && !typing {
            let pasted = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Paste(text) => Some(text.clone()),
                    _ => None,
                })
            });
            if let Some(text) = pasted {
                let n = self.paste(&text);
                if n > 0 {
                    toast(ui.ctx(), trf("Pasted {n} rows", &[("n", &n)]), Some(tr("Submit writes them to the table.")));
                }
            }
        }
        if editable {
            ui.add_space(4.0);
            let mut submit = false;
            ui.horizontal(|ui| {
                if toolbar_button(ui, tr("+ Row")).tooltip(tr("Add a row")).clicked() {
                    self.add_row();
                }
                let selected: Vec<usize> = self.selected.iter().copied().collect();
                if ui
                    .add_enabled(!selected.is_empty(), Button::outline(tr("− Delete")).size(ButtonSize::Sm))
                    .tooltip(tr("Mark the checked rows for deletion"))
                    .clicked()
                {
                    self.delete_rows(&selected);
                }
                if dirty {
                    ui.add_space(8.0);
                    let n = self.edits.count();
                    if ui
                        .add_enabled(
                            !self.submitting,
                            Button::new(trf("Submit ({n})", &[("n", &n)])).size(ButtonSize::Sm),
                        )
                        .tooltip(tr("Write the changes in one transaction"))
                        .clicked()
                    {
                        submit = true;
                    }
                    if toolbar_button(ui, tr("Preview SQL")).clicked() {
                        self.preview = Some(self.statements().join(";\n") + ";");
                    }
                    if ui.add_enabled(!self.submitting, Button::ghost(tr("Revert")).size(ButtonSize::Sm)).clicked() {
                        self.edits = Edits::default();
                        self.editing = None;
                        self.submit_error = None;
                        self.rebuild();
                    }
                    if self.submitting {
                        ui.add(shadcn_egui::Spinner::new().size(14.0));
                    }
                }
                let (_, has_pk) = self.key_columns();
                if !has_pk {
                    ui.label(
                        RichText::new(tr("No primary key: rows are matched by all their values."))
                            .size(12.0)
                            .color(t.muted_foreground),
                    );
                } else if !dirty {
                    ui.label(
                        RichText::new(tr("Double-click a cell to edit it; Ctrl+V pastes cells copied from Excel."))
                            .size(12.0)
                            .color(t.muted_foreground),
                    );
                }
            });
            if submit && let Some(c) = cx.conn {
                self.submit(cx.tasks, tab, c.cfg.clone());
            }
        }
        ui.add_space(6.0);
        if let Some(e) = &self.submit_error {
            Alert::new(tr("Changes were not saved")).icon("!").description(e.as_str()).destructive().show(ui);
            ui.add_space(6.0);
        }
        if reload {
            self.load(cx.tasks, tab, &cfg, session.clone(), cx.page_size);
        }
        let mut stop = false;
        match &self.rows {
            Load::Idle | Load::Loading => {
                ui.horizontal(|ui| {
                    ui.add(shadcn_egui::Spinner::new());
                    if let Some(t0) = self.load_started {
                        ui.label(RichText::new(format_elapsed(t0.elapsed())).color(t.muted_foreground));
                        ui.ctx().request_repaint_after(Duration::from_millis(100));
                    }
                    if ui.add(Button::destructive(tr("Stop")).size(ButtonSize::Sm)).clicked() {
                        stop = true;
                    }
                });
            }
            Load::Failed(e) => error_alert(ui, tr("Query failed"), e),
            Load::Ready(_) if self.record => self.record_view(ui, tab, editable),
            Load::Ready(_) => self.grid(ui, tab, editable),
        }
        if stop {
            self.stop(cx.tasks);
        }
        if let Some(col) = profile.or(self.profile_asked.take()) {
            self.open_profile(cx.tasks, tab, session, col);
        }
        if self.profile.as_ref().is_some_and(|p| !p.show(ui.ctx())) {
            self.profile = None;
        }
        self.preview_dialog(ui.ctx());
    }

    fn grid(&mut self, ui: &mut Ui, tab: TabId, editable: bool) {
        let Some(result) = self.rows.ready() else { return };
        let t = Theme::get(ui.ctx());
        let ctx = ui.ctx().clone();
        let base = result.rows.len();
        let ncols = result.columns.len();
        let id = egui::Id::new(("data-grid", tab));
        let mut cmds: Vec<GridCmd> = Vec::new();
        let mut editing = self.editing.take();
        let edits = &self.edits;
        let changed_bg = if t.dark {
            Color32::from_rgba_unmultiplied(59, 130, 246, 50)
        } else {
            Color32::from_rgb(0xdb, 0xea, 0xfe)
        };
        let new_bg =
            if t.dark { Color32::from_rgba_unmultiplied(34, 197, 94, 40) } else { Color32::from_rgb(0xdc, 0xfc, 0xe7) };
        let deleted_bg =
            if t.dark { Color32::from_rgba_unmultiplied(239, 68, 68, 40) } else { Color32::from_rgb(0xfe, 0xe2, 0xe2) };

        let columns: Vec<Column> = result
            .columns
            .iter()
            .map(|c| {
                let col = Column::new(c.name.clone()).sortable();
                if c.numeric { col.right() } else { col }
            })
            .collect();
        let mut panel = std::mem::take(&mut self.value_panel);
        let current = self.current;
        let value = current.and_then(|(row, col)| Some((row, col, self.value(row, col)?)));
        let shown = value
            .as_ref()
            .and_then(|(row, col, cell)| Some(Current { column: result.columns.get(*col)?, row: *row, cell }));
        panel.show(ui, egui::Id::new(("data-value", tab)), shown);
        self.value_panel = panel;
        let height = (ui.available_height() - 110.0).max(120.0);
        let display = &self.display;
        let selected = &mut self.selected;
        let cells = &self.cells;
        let mut clicked = None;
        let mut table = DataTable::new(id, columns)
            .filter(tr("Filter rows..."))
            .min_column_width(grid::MIN_COLUMN_WIDTH)
            .resizable()
            .max_height(height)
            .row_height(30.0)
            .version(self.version)
            .cell_ui(|ui, row, col| {
                let cell_rect = ui.max_rect().expand2(egui::vec2(12.0, 0.0));
                if let Some(e) = editing.as_mut().filter(|e| e.row == row && e.col == col) {
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut e.text)
                            .id(id.with(("edit", row, col)))
                            .desired_width(cell_rect.width() - 8.0)
                            .font(egui::TextStyle::Body),
                    );
                    if !e.focused {
                        te.request_focus();
                        e.focused = true;
                    } else if te.lost_focus() {
                        let cancel = ui.input(|i| i.key_pressed(Key::Escape));
                        cmds.push(if cancel { GridCmd::Cancel } else { GridCmd::Commit });
                    }
                    return true;
                }
                let is_new = row >= base;
                let deleted = edits.deleted.contains(&row);
                let changed = !is_new && edits.changed.contains_key(&(row, col));
                let value: Option<&Cell> = if is_new {
                    edits.inserted.get(row - base).and_then(|r| r[col].as_ref())
                } else {
                    edits.changed.get(&(row, col)).or_else(|| result.rows.get(row).and_then(|r| r.get(col)))
                };
                let bg = if deleted {
                    Some(deleted_bg)
                } else if is_new {
                    Some(new_bg)
                } else if changed {
                    Some(changed_bg)
                } else {
                    None
                };
                if let Some(bg) = bg {
                    ui.painter().rect_filled(cell_rect, 0.0, bg);
                }
                let r = ui.interact(cell_rect, id.with(("cell", row, col)), Sense::click());
                if r.clicked() {
                    cmds.push(GridCmd::Current(row, col));
                    clicked = Some((row, col, ui.input(|i| i.modifiers)));
                }
                if cells.contains(row, col) {
                    grid::selection_fill(ui, cell_rect, &t);
                }
                if current == Some((row, col)) {
                    grid::current_outline(ui, cell_rect, &t);
                }
                let text = match value {
                    None if is_new => RichText::new(tr("<default>")).italics().color(t.muted_foreground),
                    None | Some(Cell::Null) => RichText::new("NULL").italics().color(t.muted_foreground),
                    Some(v) => RichText::new(
                        display.get(row).and_then(|d| d.get(col)).cloned().unwrap_or_else(|| v.display().into_owned()),
                    )
                    .color(t.foreground),
                };
                let text = if deleted { text.strikethrough() } else { text };
                ui.add(egui::Label::new(text).truncate().selectable(false));
                if editable && !deleted && r.double_clicked() {
                    cmds.push(GridCmd::StartEdit(row, col));
                }
                r.context_menu(|ui| {
                    ui.set_min_width(180.0);
                    if let Some(v) = value.and_then(|v| v.as_text())
                        && menu_item(ui, tr("Copy value"), None).clicked()
                    {
                        cmds.push(GridCmd::Copy(v.into_owned()));
                    }
                    if menu_item(ui, tr("Profile column"), None).clicked() {
                        cmds.push(GridCmd::Profile(col));
                    }
                    if editable && !deleted {
                        menu_separator(ui);
                        if menu_item(ui, tr("Edit"), None).clicked() {
                            cmds.push(GridCmd::StartEdit(row, col));
                        }
                        if menu_item(ui, tr("Set NULL"), None).clicked() {
                            cmds.push(GridCmd::SetNull(row, col));
                        }
                        if (changed || (is_new && value.is_some())) && menu_item(ui, tr("Revert cell"), None).clicked()
                        {
                            cmds.push(GridCmd::Revert(row, col));
                        }
                    }
                    if editable {
                        let label = if deleted { tr("Restore row") } else { tr("Delete row") };
                        if menu_item(ui, label, None).clicked() {
                            cmds.push(GridCmd::Delete(row));
                        }
                    }
                });
                true
            });
        if editable {
            table = table.selection(selected);
        }
        let shown = table.show(ui, display);
        self.editing = editing;
        for c in cmds {
            self.apply(c, &ctx);
        }
        self.cells.set_order(shown.visible);
        if let Some((row, col, modifiers)) = clicked {
            self.cells.click(row, col, modifiers);
        }
        let mut cells = std::mem::take(&mut self.cells);
        cells.footer(ui, ncols, |row, col| self.value(row, col));
        self.cells = cells;
    }

    /// One row as a table of its columns: name, value and type. Values are edited
    /// as in the grid; the arrows step through the rows.
    fn record_view(&mut self, ui: &mut Ui, tab: TabId, editable: bool) {
        let Some(result) = self.rows.ready() else { return };
        let t = Theme::get(ui.ctx());
        let ctx = ui.ctx().clone();
        let base = result.rows.len();
        let total = base + self.edits.inserted.len();
        if total == 0 {
            ui.label(RichText::new(tr("No rows.")).color(t.muted_foreground));
            return;
        }
        let row = self.record_row().min(total - 1);
        let col = self.current.map_or(0, |(_, c)| c);
        ui.horizontal(|ui| {
            if ui.add_enabled(row > 0, Button::outline("‹").size(ButtonSize::Sm)).clicked() {
                self.current = Some((row - 1, col));
            }
            let label = trf("Row {n} of {total}", &[("n", &(row + 1)), ("total", &total)]);
            ui.label(RichText::new(label).color(t.muted_foreground));
            if ui.add_enabled(row + 1 < total, Button::outline("›").size(ButtonSize::Sm)).clicked() {
                self.current = Some((row + 1, col));
            }
            if self.edits.deleted.contains(&row) {
                ui.label(RichText::new(tr("marked for deletion")).color(t.destructive));
            } else if row >= base {
                ui.label(RichText::new(tr("new row")).color(t.muted_foreground));
            }
        });
        ui.add_space(4.0);
        let Some(result) = self.rows.ready() else { return };
        let row = self.record_row().min(total - 1);
        let id = egui::Id::new(("data-record", tab));
        let types: Vec<String> = result
            .columns
            .iter()
            .map(|c| {
                let declared = self.details.ready().and_then(|d| d.columns.iter().find(|x| x.name == c.name));
                declared.map_or_else(|| c.type_name.clone(), |d| d.data_type.clone())
            })
            .collect();
        let texts: Vec<Vec<String>> = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let v = self.display.get(row).and_then(|d| d.get(i)).cloned().unwrap_or_default();
                vec![c.name.clone(), v, types[i].clone()]
            })
            .collect();
        let deleted = self.edits.deleted.contains(&row);
        let values: Vec<Option<Cell>> = (0..result.columns.len()).map(|c| self.value(row, c)).collect();
        let changed: Vec<bool> = (0..result.columns.len())
            .map(|c| if row < base { self.edits.changed.contains_key(&(row, c)) } else { values[c].is_some() })
            .collect();
        let mut editing = self.editing.take();
        let mut cmds = Vec::new();
        let columns = vec![
            Column::new(tr("Column")).weight(1.0),
            Column::new(tr("Value")).weight(3.0),
            Column::new(tr("Type")).weight(1.0),
        ];
        DataTable::new(id, columns)
            .filter(tr("Filter columns…"))
            .row_height(30.0)
            .version(self.version + row as u64 * 1_000_003)
            .max_height((ui.available_height() - 8.0).max(120.0))
            .cell_ui(|ui, field, col| {
                if col != 1 {
                    return false;
                }
                let cell_rect = ui.max_rect().expand2(egui::vec2(12.0, 0.0));
                if let Some(e) = editing.as_mut().filter(|e| e.row == row && e.col == field) {
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut e.text)
                            .id(id.with(("edit", row, field)))
                            .desired_width(cell_rect.width() - 8.0),
                    );
                    if !e.focused {
                        te.request_focus();
                        e.focused = true;
                    } else if te.lost_focus() {
                        let cancel = ui.input(|i| i.key_pressed(Key::Escape));
                        cmds.push(if cancel { GridCmd::Cancel } else { GridCmd::Commit });
                    }
                    return true;
                }
                if changed[field] && !deleted {
                    let bg = if t.dark {
                        Color32::from_rgba_unmultiplied(59, 130, 246, 50)
                    } else {
                        Color32::from_rgb(0xdb, 0xea, 0xfe)
                    };
                    ui.painter().rect_filled(cell_rect, 0.0, bg);
                }
                let text = match &values[field] {
                    None if row >= base => RichText::new(tr("<default>")).italics().color(t.muted_foreground),
                    None | Some(Cell::Null) => RichText::new("NULL").italics().color(t.muted_foreground),
                    Some(_) => RichText::new(&texts[field][1]).color(t.foreground),
                };
                let text = if deleted { text.strikethrough() } else { text };
                let r = ui.interact(cell_rect, id.with(("cell", field)), Sense::click());
                ui.add(egui::Label::new(text).truncate().selectable(false));
                if r.clicked() {
                    cmds.push(GridCmd::Current(row, field));
                }
                if editable && !deleted && r.double_clicked() {
                    cmds.push(GridCmd::StartEdit(row, field));
                }
                r.context_menu(|ui| {
                    ui.set_min_width(180.0);
                    if let Some(v) = values[field].as_ref().and_then(|v| v.as_text())
                        && menu_item(ui, tr("Copy value"), None).clicked()
                    {
                        cmds.push(GridCmd::Copy(v.into_owned()));
                    }
                    if menu_item(ui, tr("Profile column"), None).clicked() {
                        cmds.push(GridCmd::Profile(field));
                    }
                    if editable && !deleted {
                        menu_separator(ui);
                        if menu_item(ui, tr("Edit"), None).clicked() {
                            cmds.push(GridCmd::StartEdit(row, field));
                        }
                        if menu_item(ui, tr("Set NULL"), None).clicked() {
                            cmds.push(GridCmd::SetNull(row, field));
                        }
                        if changed[field] && menu_item(ui, tr("Revert cell"), None).clicked() {
                            cmds.push(GridCmd::Revert(row, field));
                        }
                    }
                });
                true
            })
            .show(ui, &texts);
        self.editing = editing;
        for c in cmds {
            self.apply(c, &ctx);
        }
    }

    fn preview_dialog(&mut self, ctx: &egui::Context) {
        let Some(sql) = &self.preview else { return };
        let kind = self.kind;
        let r = Dialog::new("dml-preview", tr("Pending changes"))
            .description(tr("These statements run in one transaction."))
            .width(720.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    sql_editor::viewer(ui, egui::Id::new("dml-preview-text"), sql, kind);
                });
                ui.add_space(8.0);
                shadcn_egui::footer(ui, |ui| ui.add(Button::outline(tr("Close"))).clicked())
            });
        if r.should_close || r.inner {
            self.preview = None;
        }
    }
}
