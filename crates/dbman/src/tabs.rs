//! Editor tabs: SQL consoles, table data and object structure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dbman_core::sql::{split_statements, statement_at};
use dbman_core::{Connection, ConnectionConfig, DbKind, DbObject, StatementOutcome, TableDetails};
use dbman_drivers::transfer::ExportSource;
use egui::{Key, Modifiers, RichText, Ui};
use shadcn_egui::{Alert, Button, ButtonSize, Column, DataTable, Select, Theme, TooltipExt, tabs};

use crate::compare_view::CompareView;
use crate::copy_view::CopyView;
use crate::data_view::DataView;
use crate::er_view::ErView;
use crate::events::{Event, TabEvent, TabId, Tasks, timed};
use crate::explorer::{Action, ConnState, Load};
use crate::grid::{self, Grid};
use crate::sql_editor;
use crate::table_editor::TableEditor;

pub struct Tab {
    pub id: TabId,
    pub conn: uuid::Uuid,
    pub kind: TabKind,
}

pub enum TabKind {
    Console(Console),
    Data(Box<DataView>),
    Object(ObjectView),
    TableEditor(Box<TableEditor>),
    Diagram(Box<ErView>),
    Compare(Box<CompareView>),
    Copy(Box<CopyView>),
}

/// What a tab can reach while drawing.
pub struct TabCtx<'a> {
    pub tasks: &'a Tasks,
    pub conn: Option<&'a ConnState>,
    pub actions: &'a mut Vec<Action>,
    pub page_size: usize,
    /// The connection the tab belongs to (even when it is not connected).
    pub conn_id: uuid::Uuid,
    /// Every saved connection (comparison targets).
    pub conns: &'a [ConnState],
}

impl Tab {
    pub fn title(&self) -> String {
        match &self.kind {
            TabKind::Console(c) => c.title.clone(),
            TabKind::Data(d) => d.object.path.name.clone(),
            TabKind::Object(o) => format!("{} ({})", o.object.path.name, o.object.kind.label()),
            TabKind::TableEditor(e) => e.title(),
            TabKind::Diagram(d) => d.title(),
            TabKind::Compare(c) => c.title(),
            TabKind::Copy(c) => c.title(),
        }
    }

    /// Applies a background result; may ask the app to do something (refresh the explorer).
    pub fn on_event(&mut self, ev: TabEvent, ctx: &egui::Context) -> Vec<Action> {
        match &mut self.kind {
            TabKind::Console(c) => c.on_event(ev),
            TabKind::Data(d) => d.on_event(ev, ctx),
            TabKind::Object(o) => o.on_event(ev),
            TabKind::TableEditor(e) => return e.on_event(ev, ctx),
            TabKind::Diagram(d) => d.on_event(ev),
            TabKind::Compare(c) => c.on_event(ev),
            TabKind::Copy(c) => c.on_event(ev),
        }
        Vec::new()
    }

    pub fn ui(&mut self, ui: &mut Ui, cx: TabCtx<'_>) {
        let id = self.id;
        match &mut self.kind {
            TabKind::Console(c) => c.ui(ui, id, cx),
            TabKind::Data(d) => d.ui(ui, id, cx),
            TabKind::Object(o) => o.ui(ui, id, cx),
            TabKind::TableEditor(e) => e.ui(ui, id, cx),
            TabKind::Diagram(d) => d.ui(ui, id, cx),
            TabKind::Compare(c) => c.ui(ui, id, cx),
            TabKind::Copy(c) => c.ui(ui, id, cx),
        }
    }
}

pub(crate) fn toolbar_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(Button::outline(text).size(ButtonSize::Sm))
}

pub(crate) fn error_alert(ui: &mut Ui, title: &str, e: &str) {
    Alert::new(title).icon("!").description(e).destructive().show(ui);
}

pub(crate) fn not_connected(ui: &mut Ui, cx: &mut TabCtx<'_>) {
    ui.add_space(16.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("Not connected").color(Theme::get(ui.ctx()).muted_foreground));
        if ui.add(Button::new("Connect")).clicked() {
            cx.actions.push(Action::Connect(cx.conn_id));
        }
    });
}

// ---------------------------------------------------------------------------

/// Result of one statement in a console.
pub struct Output {
    pub sql: String,
    pub summary: String,
    pub error: Option<String>,
    pub grid: Option<Grid>,
}

/// A SQL console with its own session (own transaction state), like DataGrip's.
pub struct Console {
    pub title: String,
    pub cfg: ConnectionConfig,
    pub database: Option<String>,
    pub sql: String,
    session: Load<Arc<dyn Connection>>,
    running: bool,
    /// When the current run started, for the timer next to Stop.
    started: Option<Instant>,
    /// Set by Stop: the run skips its remaining statements.
    stop: Arc<AtomicBool>,
    outputs: Vec<Output>,
    shown_output: usize,
    selection: Option<std::ops::Range<usize>>,
    /// Statements to run once the session is open.
    queued: Option<Vec<String>>,
}

impl Console {
    pub fn new(title: String, cfg: ConnectionConfig, database: Option<String>) -> Self {
        Self {
            title,
            cfg,
            database,
            sql: String::new(),
            session: Load::Idle,
            running: false,
            started: None,
            stop: Arc::new(AtomicBool::new(false)),
            outputs: Vec::new(),
            shown_output: 0,
            selection: None,
            queued: None,
        }
    }

    pub fn summaries(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.summary.clone()).collect()
    }

    fn on_event(&mut self, ev: TabEvent) {
        match ev {
            TabEvent::Session(r) => {
                self.session = Load::from_result(r);
                if let Load::Failed(e) = &self.session {
                    self.running = false;
                    self.started = None;
                    self.queued = None;
                    self.outputs = vec![Output {
                        sql: String::new(),
                        summary: "connection failed".into(),
                        error: Some(e.clone()),
                        grid: None,
                    }];
                }
            }
            TabEvent::Statement { sql, elapsed, result } => {
                let (summary, error, grid) = match result {
                    Ok(outcome) => {
                        let summary = outcome.summary(elapsed);
                        let grid = match outcome {
                            StatementOutcome::Rows(r) => Some(Grid::new(r)),
                            StatementOutcome::Done { .. } => None,
                        };
                        (summary, None, grid)
                    }
                    Err(e) if self.stop.load(Ordering::Relaxed) => ("cancelled".into(), Some(e.message), None),
                    Err(e) => ("failed".into(), Some(e.message), None),
                };
                let has_grid = grid.is_some();
                self.outputs.push(Output { sql, summary, error: error.clone(), grid });
                // Show the newest result set, or the error.
                if has_grid || error.is_some() {
                    self.shown_output = self.outputs.len() - 1;
                }
            }
            TabEvent::RunFinished => {
                self.running = false;
                self.started = None;
            }
            TabEvent::Cancelled(Some(e)) => {
                self.outputs.push(Output {
                    sql: String::new(),
                    summary: "cancel failed".into(),
                    error: Some(e),
                    grid: None,
                });
                self.shown_output = self.outputs.len() - 1;
            }
            _ => {}
        }
    }

    /// The selection's statements, the whole script, or the statement under the cursor.
    fn statements_to_run(&self, all: bool) -> Vec<String> {
        let kind = self.cfg.kind;
        let split = |text: &str| -> Vec<String> {
            split_statements(text, kind).into_iter().map(|r| text[r].to_string()).collect()
        };
        match &self.selection {
            Some(r) if !all && !r.is_empty() => split(&self.sql[r.clone()]),
            _ if all => split(&self.sql),
            sel => {
                let cursor = sel.as_ref().map_or(self.sql.len(), |r| r.start);
                statement_at(&self.sql, kind, cursor).map(|r| self.sql[r].to_string()).into_iter().collect()
            }
        }
    }

    fn run(&mut self, tasks: &Tasks, tab: TabId, all: bool, max_rows: usize) {
        let stmts = self.statements_to_run(all);
        if stmts.is_empty() || self.running {
            return;
        }
        self.outputs.clear();
        self.shown_output = 0;
        self.running = true;
        self.started = Some(Instant::now());
        self.stop = Arc::new(AtomicBool::new(false));
        match &self.session {
            Load::Ready(s) => {
                spawn_run(tasks, tab, s.clone(), self.database.clone(), stmts, max_rows, self.stop.clone())
            }
            Load::Loading => self.queued = Some(stmts),
            Load::Idle | Load::Failed(_) => {
                self.queued = Some(stmts);
                self.open_session(tasks, tab);
            }
        }
    }

    /// Stops the run: the server cancels the current statement and the rest are skipped.
    fn stop(&mut self, tasks: &Tasks, tab: TabId) {
        self.stop.store(true, Ordering::Relaxed);
        match &self.session {
            Load::Ready(s) => {
                let s = s.clone();
                tasks.spawn(async move {
                    match s.cancel().await {
                        Ok(()) => Event::Tab { tab, event: TabEvent::Cancelled(None) },
                        Err(e) => Event::Tab { tab, event: TabEvent::Cancelled(Some(e.message)) },
                    }
                });
            }
            // Still connecting: nothing runs yet, so just drop the queued statements.
            _ => {
                self.queued = None;
                self.running = false;
                self.started = None;
            }
        }
    }

    pub fn open_session(&mut self, tasks: &Tasks, tab: TabId) {
        self.session = Load::Loading;
        let mut cfg = self.cfg.clone();
        if let Some(db) = &self.database
            && cfg.kind == DbKind::MySql
        {
            cfg.database = db.clone();
        }
        tasks.spawn(async move { Event::Tab { tab, event: TabEvent::Session(dbman_drivers::connect(&cfg).await) } });
    }

    fn ui(&mut self, ui: &mut Ui, tab: TabId, cx: TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        if let (Load::Ready(s), Some(stmts)) = (&self.session, self.queued.take()) {
            spawn_run(cx.tasks, tab, s.clone(), self.database.clone(), stmts, cx.page_size, self.stop.clone());
        }

        // Ctrl+Enter runs the statement under the cursor (or the selection),
        // Ctrl+Shift+Enter the whole script. Consumed before the editor sees Enter.
        let editor_id = egui::Id::new(("console-editor", tab));
        if ui.memory(|m| m.has_focus(editor_id)) {
            let all = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter));
            let one = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
            if all || one {
                self.run(cx.tasks, tab, all, cx.page_size);
            }
        }

        ui.horizontal(|ui| {
            let run = ui
                .add_enabled(!self.running, Button::new("▶ Run").size(ButtonSize::Sm))
                .tooltip("Run the statement under the cursor or the selection (Ctrl+Enter)");
            if run.clicked() {
                self.run(cx.tasks, tab, false, cx.page_size);
            }
            let all = ui
                .add_enabled(!self.running, Button::outline("Run script").size(ButtonSize::Sm))
                .tooltip("Run every statement (Ctrl+Shift+Enter)");
            if all.clicked() {
                self.run(cx.tasks, tab, true, cx.page_size);
            }
            if self.running {
                if ui
                    .add(Button::destructive("Stop").size(ButtonSize::Sm))
                    .tooltip("Cancel the running statement on the server")
                    .clicked()
                {
                    self.stop(cx.tasks, tab);
                }
                ui.add(egui::Spinner::new().size(14.0));
                if let Some(t0) = self.started {
                    ui.label(RichText::new(format_elapsed(t0.elapsed())).color(t.muted_foreground));
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
            }
            if let Some(dbs) = cx.conn.and_then(|c| c.databases.ready())
                && dbs.len() > 1
            {
                ui.add_space(12.0);
                ui.label(RichText::new("Database").color(t.muted_foreground));
                let names: Vec<&str> = dbs.iter().map(String::as_str).collect();
                let mut idx = self.database.as_ref().and_then(|d| dbs.iter().position(|x| x == d));
                let before = idx;
                ui.add(Select::new(&mut idx, &names).width(180.0).placeholder("default"));
                if idx != before {
                    self.database = idx.map(|i| dbs[i].clone());
                }
            }
            if let Some(c) = cx.conn
                && c.databases.is_idle()
                && c.session.ready().is_some()
            {
                cx.actions.push(Action::LoadDatabases(c.cfg.id));
            }
        });
        ui.add_space(6.0);

        egui::Panel::top(egui::Id::new(("console-split", tab)))
            .resizable(true)
            .default_size(260.0)
            .min_size(80.0)
            .frame(egui::Frame::new())
            .show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt(("console-scroll", tab)).show(ui, |ui| {
                    let out = sql_editor::editor(ui, editor_id, &mut self.sql, self.cfg.kind, ui.available_height());
                    if out.selection.is_some() {
                        self.selection = out.selection;
                    }
                });
            });

        ui.add_space(8.0);
        self.results(ui, tab, cx.actions, cx.conn_id);
    }

    fn results(&mut self, ui: &mut Ui, tab: TabId, actions: &mut Vec<Action>, conn: uuid::Uuid) {
        let t = Theme::get(ui.ctx());
        if self.outputs.is_empty() {
            if !self.running {
                ui.label(RichText::new("Write SQL above and press Ctrl+Enter.").color(t.muted_foreground));
            }
            return;
        }
        if self.outputs.len() > 1 {
            let labels: Vec<String> = self
                .outputs
                .iter()
                .enumerate()
                .map(|(i, o)| if o.error.is_some() { format!("{} ×", i + 1) } else { format!("{}", i + 1) })
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            self.shown_output = self.shown_output.min(refs.len() - 1);
            tabs(ui, &mut self.shown_output, &refs);
            ui.add_space(6.0);
        }
        let Some(o) = self.outputs.get(self.shown_output) else { return };
        ui.horizontal(|ui| {
            let first_line = o.sql.lines().next().unwrap_or_default();
            ui.add(egui::Label::new(RichText::new(first_line).monospace().color(t.muted_foreground)).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(&o.summary).color(t.muted_foreground));
            });
        });
        if let Some(e) = &o.error {
            error_alert(ui, if o.summary == "cancelled" { "Statement cancelled" } else { "Statement failed" }, e);
        }
        if let Some(g) = &o.grid {
            ui.horizontal(|ui| {
                if grid::export_menu(ui, &g.result, self.cfg.kind, None, true) {
                    let source = ExportSource::Query { database: self.database.clone(), sql: o.sql.clone() };
                    actions.push(Action::ExportData(conn, source));
                }
            });
            ui.add_space(4.0);
            g.show(ui, egui::Id::new(("console-grid", tab, self.shown_output)));
        }
    }
}

fn spawn_run(
    tasks: &Tasks,
    tab: TabId,
    session: Arc<dyn Connection>,
    database: Option<String>,
    stmts: Vec<String>,
    max_rows: usize,
    stop: Arc<AtomicBool>,
) {
    let tx = tasks.sender();
    tasks.runtime().spawn(async move {
        for sql in stmts {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let (elapsed, result) = timed(session.execute(database.as_deref(), &sql, max_rows)).await;
            let failed = result.is_err();
            tx.send(Event::Tab { tab, event: TabEvent::Statement { sql, elapsed, result } });
            if failed {
                break;
            }
        }
        tx.send(Event::Tab { tab, event: TabEvent::RunFinished });
    });
}

/// "0.4 s", "12 s", "3 min 05 s" for the running-query timer.
pub(crate) fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 10 {
        format!("{:.1} s", d.as_secs_f64())
    } else if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{} min {:02} s", secs / 60, secs % 60)
    }
}

// ---------------------------------------------------------------------------

/// Columns, indexes, keys and DDL of an object.
pub struct ObjectView {
    pub object: DbObject,
    kind: DbKind,
    section: usize,
    details: Load<TableDetails>,
    ddl: Load<String>,
}

impl ObjectView {
    pub fn new(object: DbObject, kind: DbKind) -> Self {
        Self { object, kind, section: 0, details: Load::Idle, ddl: Load::Idle }
    }

    fn on_event(&mut self, ev: TabEvent) {
        match ev {
            TabEvent::Ddl(r) => self.ddl = Load::from_result(r),
            TabEvent::Details(r) => self.details = Load::from_result(r),
            _ => {}
        }
    }

    fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let Some(session) = cx.conn.and_then(ConnState::session) else {
            not_connected(ui, &mut cx);
            return;
        };
        let has_columns = self.object.kind.has_columns();
        if has_columns && self.details.is_idle() {
            self.details = Load::Loading;
            let (s, p) = (session.clone(), self.object.path.clone());
            cx.tasks.spawn(async move { Event::Tab { tab, event: TabEvent::Details(s.table_details(&p).await) } });
        }
        if self.ddl.is_idle() {
            self.ddl = Load::Loading;
            let (s, o) = (session.clone(), self.object.clone());
            cx.tasks.spawn(async move { Event::Tab { tab, event: TabEvent::Ddl(s.ddl(&o).await) } });
        }

        let sections: &[&str] = if has_columns { &["Columns", "Indexes", "Foreign keys", "DDL"] } else { &["DDL"] };
        ui.horizontal(|ui| {
            tabs(ui, &mut self.section, sections);
            ui.add_space(8.0);
            if toolbar_button(ui, "⟳ Refresh").clicked() {
                self.details = Load::Idle;
                self.ddl = Load::Idle;
            }
            if self.object.kind.has_data() && toolbar_button(ui, "Open data").clicked() {
                cx.actions.push(Action::OpenData(cx.conn_id, self.object.clone()));
            }
        });
        ui.add_space(8.0);
        let section = sections.get(self.section).copied().unwrap_or("DDL");
        if section == "DDL" {
            match &self.ddl {
                Load::Ready(d) => {
                    egui::ScrollArea::both().id_salt(("ddl", tab)).show(ui, |ui| {
                        sql_editor::viewer(ui, egui::Id::new(("ddl-view", tab)), d, self.kind);
                    });
                }
                Load::Failed(e) => error_alert(ui, "Could not get DDL", e),
                _ => {
                    ui.add(egui::Spinner::new());
                }
            }
            return;
        }
        let d = match &self.details {
            Load::Ready(d) => d,
            Load::Failed(e) => return error_alert(ui, "Could not read the structure", e),
            _ => {
                ui.add(egui::Spinner::new());
                return;
            }
        };
        let id = egui::Id::new(("object-table", tab, self.section));
        let (columns, rows): (Vec<Column>, Vec<Vec<String>>) = match section {
            "Columns" => (
                vec![
                    Column::new("Name").sortable().weight(1.2),
                    Column::new("Type").sortable().weight(1.2),
                    Column::new("Nullable").weight(0.6),
                    Column::new("Default"),
                    Column::new("Key").weight(0.5),
                    Column::new("Comment").weight(1.4),
                ],
                d.columns
                    .iter()
                    .map(|c| {
                        vec![
                            c.name.clone(),
                            c.data_type.clone(),
                            if c.nullable { "yes".into() } else { "no".into() },
                            c.default.clone().unwrap_or_default(),
                            if c.primary_key {
                                "PK".into()
                            } else if c.auto_increment {
                                "auto".into()
                            } else {
                                String::new()
                            },
                            c.comment.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            ),
            "Indexes" => (
                vec![
                    Column::new("Name").sortable(),
                    Column::new("Columns").weight(1.5),
                    Column::new("Unique").weight(0.5),
                    Column::new("Method").weight(0.5),
                ],
                d.indexes
                    .iter()
                    .map(|i| {
                        vec![
                            i.name.clone(),
                            i.columns.join(", "),
                            if i.primary {
                                "primary".into()
                            } else if i.unique {
                                "yes".into()
                            } else {
                                "no".into()
                            },
                            i.method.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            ),
            _ => (
                vec![
                    Column::new("Name").sortable(),
                    Column::new("Columns"),
                    Column::new("References").weight(1.5),
                    Column::new("On update").weight(0.6),
                    Column::new("On delete").weight(0.6),
                ],
                d.foreign_keys
                    .iter()
                    .map(|f| {
                        vec![
                            f.name.clone(),
                            f.columns.join(", "),
                            format!("{}.{}({})", f.ref_schema, f.ref_table, f.ref_columns.join(", ")),
                            f.on_update.clone(),
                            f.on_delete.clone(),
                        ]
                    })
                    .collect(),
            ),
        };
        if rows.is_empty() {
            ui.label(RichText::new("none").color(Theme::get(ui.ctx()).muted_foreground));
            return;
        }
        DataTable::new(id, columns)
            .row_height(32.0)
            .max_height((ui.available_height() - 60.0).max(100.0))
            .show(ui, &rows);
    }
}
