//! Editor tabs: SQL consoles, table data and object structure.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dbman_core::sql::{self, split_statements, statement_at};
use dbman_core::{Connection, ConnectionConfig, DbKind, DbObject, Product, StatementOutcome, TableDetails};
use dbman_drivers::transfer::ExportSource;
use egui::{Key, Modifiers, RichText, Ui};
use shadcn_egui::{
    Alert, Badge, Button, ButtonSize, Column, DataTable, Select, Theme, TooltipExt, dropdown_menu, menu_item,
    menu_label, menu_separator, tabs, toast, toast_error, toggle_group,
};

use crate::admin_view::AdminView;
use crate::chart_view;
use crate::compare_view::CompareView;
use crate::copy_view::CopyView;
use crate::data_view::DataView;
use crate::er_view::ErView;
use crate::events::{Event, EventSender, TabEvent, TabId, Tasks, timed};
use crate::explorer::{Action, ConnState, Load};
use crate::files::{self, PendingPath, Pick};
use crate::grid::{self, Grid};
use crate::i18n::{tr, trf};
use crate::keys_view::KeysView;
use crate::pivot_view;
use crate::search_view::SearchView;
use crate::sessions_view::SessionsView;
use crate::sql_editor;
use crate::store::{History, HistoryEntry};
use crate::table_editor::TableEditor;
use crate::value_panel::ValuePanel;
use dbman_core::chart::{self, ChartSpec};
use dbman_core::explain::{self, Plan, PlanNode};
use dbman_core::params;
use dbman_core::pivot::{self, PivotSpec};

pub struct Tab {
    pub id: TabId,
    pub conn: uuid::Uuid,
    pub kind: TabKind,
}

pub enum TabKind {
    Console(Box<Console>),
    Data(Box<DataView>),
    Object(Box<ObjectView>),
    TableEditor(Box<TableEditor>),
    Diagram(Box<ErView>),
    Compare(Box<CompareView>),
    Copy(Box<CopyView>),
    Sessions(Box<SessionsView>),
    Admin(Box<AdminView>),
    Search(Box<SearchView>),
    Keys(Box<KeysView>),
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
    /// Statements run in consoles.
    pub history: &'a History,
    /// Script files opened or saved lately, newest first.
    pub recent_scripts: &'a [PathBuf],
    /// Values given to query parameters before, by name.
    pub params: &'a std::collections::BTreeMap<String, String>,
}

impl Tab {
    pub fn title(&self) -> String {
        match &self.kind {
            TabKind::Console(c) => c.title(),
            TabKind::Data(d) => d.object.path.name.clone(),
            TabKind::Object(o) => format!("{} ({})", o.object.path.name, tr(o.object.kind.label())),
            TabKind::TableEditor(e) => e.title(),
            TabKind::Diagram(d) => d.title(),
            TabKind::Compare(c) => c.title(),
            TabKind::Copy(c) => c.title(),
            TabKind::Sessions(s) => s.title(),
            TabKind::Admin(a) => a.title(),
            TabKind::Search(s) => s.title(),
            TabKind::Keys(k) => k.title(),
        }
    }

    /// The database the tab works in, if it is tied to one.
    pub fn database(&self) -> Option<&str> {
        let db = match &self.kind {
            TabKind::Console(c) => c.database.as_deref().unwrap_or(&c.cfg.database),
            TabKind::Data(d) => &d.object.path.database,
            TabKind::Object(o) => &o.object.path.database,
            TabKind::TableEditor(e) => e.database(),
            TabKind::Diagram(d) => &d.database,
            TabKind::Compare(c) => &c.database,
            TabKind::Copy(c) => &c.database,
            TabKind::Sessions(_) => "",
            TabKind::Admin(a) => &a.database,
            TabKind::Search(s) => &s.database,
            TabKind::Keys(k) => &k.database,
        };
        (!db.is_empty()).then_some(db)
    }

    /// The table, view or script the tab shows, for the window title.
    pub fn entity(&self) -> Option<String> {
        match &self.kind {
            TabKind::Console(c) => Some(c.title()),
            TabKind::Data(d) => Some(d.object.path.name.clone()),
            TabKind::Object(o) => Some(o.object.path.name.clone()),
            TabKind::TableEditor(e) => e.table().map(str::to_owned),
            TabKind::Diagram(_)
            | TabKind::Compare(_)
            | TabKind::Sessions(_)
            | TabKind::Admin(_)
            | TabKind::Search(_)
            | TabKind::Keys(_) => None,
            TabKind::Copy(c) => c.only().map(str::to_owned),
        }
    }

    /// Applies a background result; may ask the app to do something (refresh the explorer).
    pub fn on_event(&mut self, ev: TabEvent, ctx: &egui::Context) -> Vec<Action> {
        match &mut self.kind {
            TabKind::Console(c) => return c.on_event(ev),
            TabKind::Data(d) => d.on_event(ev, ctx),
            TabKind::Object(o) => o.on_event(ev),
            TabKind::TableEditor(e) => return e.on_event(ev, ctx),
            TabKind::Diagram(d) => d.on_event(ev),
            TabKind::Compare(c) => c.on_event(ev),
            TabKind::Copy(c) => c.on_event(ev),
            TabKind::Sessions(s) => s.on_event(ev, ctx),
            TabKind::Admin(a) => a.on_event(ev, ctx),
            TabKind::Search(s) => s.on_event(ev),
            TabKind::Keys(k) => return k.on_event(ev),
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
            TabKind::Sessions(s) => s.ui(ui, id, cx),
            TabKind::Admin(a) => a.ui(ui, id, cx),
            TabKind::Search(s) => s.ui(ui, id, cx),
            TabKind::Keys(k) => k.ui(ui, id, cx),
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
        ui.label(RichText::new(tr("Not connected")).color(Theme::get(ui.ctx()).muted_foreground));
        if ui.add(Button::new(tr("Connect"))).clicked() {
            cx.actions.push(Action::Connect(cx.conn_id));
        }
    });
}

// ---------------------------------------------------------------------------

/// Result of one statement in a console.
pub struct Output {
    pub sql: String,
    pub summary: String,
    /// Stopped by the user (the summary is translated, so it cannot tell).
    pub cancelled: bool,
    pub error: Option<String>,
    pub grid: Option<Grid>,
    pub plan: Option<Plan>,
    /// The result shown as a chart instead of the table.
    pub chart: Option<ChartSpec>,
    /// The result shown as a pivot table instead.
    pub pivot: Option<PivotSpec>,
}

/// Work a console starts once its session is open.
/// The statements of a console run, after the one that opens a transaction, if any.
struct Batch {
    begin: Option<String>,
    stmts: Vec<String>,
}

enum Queued {
    Run(Batch),
    /// The plan of a statement, measured or not.
    Explain(String, bool, Option<String>),
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
    queued: Option<Queued>,
    autocomplete: sql_editor::Autocomplete,
    /// The find and replace bar over the editor.
    pub find: sql_editor::FindBar,
    /// The editor had the focus in the last frame.
    editor_focused: bool,
    /// The script file the console was opened from or saved to.
    pub file: Option<PathBuf>,
    /// The text as it was last opened or saved, to tell unsaved changes.
    saved_sql: String,
    pending_open: Option<PendingPath>,
    pending_save: Option<PendingPath>,
    pub show_history: bool,
    history_search: String,
    /// Statements wait for Commit instead of committing on their own.
    pub manual: bool,
    /// A transaction is open in the session.
    pub in_tx: bool,
    pub value: ValuePanel,
    /// Statements that change something, waiting for the user to confirm the run.
    confirm: Option<Vec<String>>,
    /// Statements with parameters, waiting for their values.
    params: Option<ParamPrompt>,
}

/// The values of a run's parameters, while the user types them.
struct ParamPrompt {
    stmts: Vec<String>,
    names: Vec<String>,
    values: Vec<String>,
    /// Explain the statement (`Some(analyze)`) instead of running it.
    explain: Option<bool>,
    /// The first field takes the focus once, when the dialog opens.
    focus: bool,
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
            autocomplete: Default::default(),
            find: Default::default(),
            editor_focused: false,
            file: None,
            saved_sql: String::new(),
            pending_open: None,
            pending_save: None,
            show_history: false,
            history_search: String::new(),
            manual: false,
            in_tx: false,
            value: ValuePanel::default(),
            confirm: None,
            params: None,
        }
    }

    /// The script's file name (with a dot when it has unsaved changes), or the console's name.
    pub fn title(&self) -> String {
        match self.file.as_ref().and_then(|p| p.file_name()) {
            Some(name) if self.sql != self.saved_sql => format!("{} •", name.to_string_lossy()),
            Some(name) => name.to_string_lossy().into_owned(),
            None => self.title.clone(),
        }
    }

    /// Shows a script file's text, as saved.
    pub fn set_file(&mut self, path: PathBuf, sql: String) {
        self.sql = sql.clone();
        self.saved_sql = sql;
        self.file = Some(path);
    }

    /// Writes the script to its file, or asks where when it has none.
    fn save(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(path) = self.file.clone() else {
            self.save_as(ctx);
            return;
        };
        match std::fs::write(&path, &self.sql) {
            Ok(()) => {
                self.saved_sql = self.sql.clone();
                toast(ctx, tr("Saved"), Some(&path.to_string_lossy()));
                actions.push(Action::RecentScript(path));
            }
            Err(e) => toast_error(ctx, tr("Could not save the file"), Some(&e.to_string())),
        }
    }

    fn save_as(&mut self, ctx: &egui::Context) {
        let name = self.file.as_ref().and_then(|p| p.file_name()).map_or("script.sql".into(), |n| n.to_string_lossy());
        let dialog = rfd::AsyncFileDialog::new().set_file_name(name).add_filter("SQL", &["sql"]);
        self.pending_save = Some(files::pick(ctx, dialog, Pick::SaveAs));
    }

    /// Opens a script here when the console is empty, otherwise in a new console.
    fn open(&mut self, path: PathBuf, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if self.file.is_some() || !self.sql.trim().is_empty() {
            actions.push(Action::OpenScript(self.cfg.id, self.database.clone(), path));
            return;
        }
        match read_script(&path) {
            Ok(sql) => {
                self.set_file(path.clone(), sql);
                actions.push(Action::RecentScript(path));
            }
            Err(e) => toast_error(ctx, tr("Could not open the file"), Some(&e)),
        }
    }

    /// Answers of the open and save dialogs.
    fn poll_dialogs(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if let Some(answer) = self.pending_open.as_ref().and_then(PendingPath::poll) {
            self.pending_open = None;
            if let Some(path) = answer {
                self.open(path, ctx, actions);
            }
        }
        if let Some(answer) = self.pending_save.as_ref().and_then(PendingPath::poll) {
            self.pending_save = None;
            if let Some(path) = answer {
                self.file = Some(path);
                self.save(ctx, actions);
            }
        }
    }

    /// Adds a statement from the history at the end of the script.
    fn insert(&mut self, ctx: &egui::Context, editor_id: egui::Id, sql: &str) {
        let sql = sql.trim_end();
        if !self.sql.trim().is_empty() {
            let trimmed = self.sql.trim_end().len();
            self.sql.truncate(trimmed);
            if !self.sql.ends_with(';') {
                self.sql.push(';');
            }
            self.sql.push('\n');
        }
        self.sql.push_str(sql);
        sql_editor::place_cursor(ctx, editor_id, self.sql.chars().count());
    }

    /// Formats the selection, or the whole script when nothing is selected.
    pub fn format(&mut self, ctx: &egui::Context, editor_id: egui::Id) {
        let range = match &self.selection {
            Some(r) if !r.is_empty() && r.end <= self.sql.len() => r.clone(),
            _ => 0..self.sql.len(),
        };
        let formatted = dbman_core::format::format_sql(&self.sql[range.clone()], self.cfg.kind);
        let end = range.start + formatted.len();
        self.sql.replace_range(range, &formatted);
        self.selection = None;
        sql_editor::place_cursor(ctx, editor_id, self.sql[..end].chars().count());
    }

    /// The shown plan's steps, indented by depth (tests).
    pub fn plan_outline(&self) -> Vec<String> {
        let plan = self.outputs.get(self.shown_output).and_then(|o| o.plan.as_ref());
        plan.map(|p| p.rows().into_iter().map(|(d, n)| format!("{}{}", "  ".repeat(d), n.operation)).collect())
            .unwrap_or_default()
    }

    /// The history panel's statements (tests).
    pub fn history_shown<'a>(&self, history: &'a History) -> Vec<&'a str> {
        history.find(self.cfg.id, &self.history_search).take(HISTORY_SHOWN).map(|e| e.sql.as_str()).collect()
    }

    /// The rows of the shown result, if it has any.
    pub fn shown_result(&self) -> Option<&dbman_core::QueryResult> {
        Some(&self.outputs.get(self.shown_output)?.grid.as_ref()?.result)
    }

    /// The pivot table of the shown result, when it is shown as one.
    pub fn pivot_mut(&mut self) -> Option<&mut PivotSpec> {
        self.outputs.get_mut(self.shown_output)?.pivot.as_mut()
    }

    /// The chart of the shown result, when it is shown as one.
    pub fn chart_mut(&mut self) -> Option<&mut ChartSpec> {
        self.outputs.get_mut(self.shown_output)?.chart.as_mut()
    }

    /// What the autocomplete popup offers (tests).
    pub fn completions(&self) -> Vec<String> {
        self.autocomplete.labels()
    }

    pub fn summaries(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.summary.clone()).collect()
    }

    fn on_event(&mut self, ev: TabEvent) -> Vec<Action> {
        let mut actions = Vec::new();
        match ev {
            TabEvent::Session(r) => {
                self.session = Load::from_result(r);
                self.in_tx = false;
                if let Load::Failed(e) = &self.session {
                    self.running = false;
                    self.started = None;
                    self.queued = None;
                    self.outputs = vec![Output {
                        sql: String::new(),
                        summary: tr("connection failed").into(),
                        cancelled: false,
                        error: Some(e.clone()),
                        grid: None,
                        plan: None,
                        chart: None,
                        pivot: None,
                    }];
                }
            }
            TabEvent::Transaction(open) => self.in_tx = open,
            TabEvent::Statement { sql, elapsed, result } => {
                if result.is_ok()
                    && let Some(open) = sql::transaction_effect(self.cfg.kind, &sql)
                {
                    self.in_tx = open;
                }
                let (summary, cancelled, error, grid) = match result {
                    Ok(outcome) => {
                        let summary = outcome_summary(&outcome, elapsed);
                        let grid = match outcome {
                            StatementOutcome::Rows(r) => Some(Grid::new(r)),
                            StatementOutcome::Done { .. } => None,
                        };
                        (summary, false, None, grid)
                    }
                    Err(e) if self.stop.load(Ordering::Relaxed) => {
                        (tr("cancelled").into(), true, Some(e.message), None)
                    }
                    Err(e) => (tr("failed").into(), false, Some(e.message), None),
                };
                let has_grid = grid.is_some();
                let at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                actions.push(Action::RecordQuery(Box::new(HistoryEntry {
                    conn: self.cfg.id,
                    database: self.database.clone(),
                    sql: sql.clone(),
                    at,
                    millis: elapsed.as_millis() as u64,
                    summary: summary.clone(),
                    ok: error.is_none(),
                })));
                self.outputs.push(Output {
                    sql,
                    summary,
                    cancelled,
                    error: error.clone(),
                    grid,
                    plan: None,
                    chart: None,
                    pivot: None,
                });
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
                    summary: tr("cancel failed").into(),
                    cancelled: false,
                    error: Some(e),
                    grid: None,
                    plan: None,
                    chart: None,
                    pivot: None,
                });
                self.shown_output = self.outputs.len() - 1;
            }
            TabEvent::Plan { sql, elapsed, result } => {
                let cancelled = result.is_err() && self.stop.load(Ordering::Relaxed);
                let (summary, error, plan) = match result {
                    Ok(p) => {
                        let steps = p.rows().len();
                        (
                            trf("plan, {n} steps, {time}", &[("n", &steps), ("time", &format_elapsed(elapsed))]),
                            None,
                            Some(p),
                        )
                    }
                    Err(e) if cancelled => (tr("cancelled").into(), Some(e.message), None),
                    Err(e) => (tr("failed").into(), Some(e.message), None),
                };
                self.outputs.push(Output {
                    sql,
                    summary,
                    cancelled,
                    error,
                    grid: None,
                    plan,
                    chart: None,
                    pivot: None,
                });
                self.shown_output = self.outputs.len() - 1;
            }
            _ => {}
        }
        actions
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

    fn run(&mut self, cx: &TabCtx<'_>, tab: TabId, all: bool) {
        let stmts = self.statements_to_run(all);
        if !self.ask_params(&stmts, None, cx.params) {
            self.run_ready(cx.tasks, tab, stmts, cx.page_size);
        }
    }

    /// Opens the parameters dialog when the statements have parameters; false when they have none.
    fn ask_params(
        &mut self,
        stmts: &[String],
        explain: Option<bool>,
        remembered: &std::collections::BTreeMap<String, String>,
    ) -> bool {
        let kind = self.cfg.kind;
        let mut names: Vec<String> = Vec::new();
        for s in stmts {
            for n in params::names(&params::find(kind, s)) {
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        if names.is_empty() {
            return false;
        }
        let values = names.iter().map(|n| remembered.get(n).cloned().unwrap_or_default()).collect();
        self.params = Some(ParamPrompt { stmts: stmts.to_vec(), names, values, explain, focus: true });
        true
    }

    /// Statements whose parameters have values: they run, after the confirmation if one is due.
    fn run_ready(&mut self, tasks: &Tasks, tab: TabId, stmts: Vec<String>, max_rows: usize) {
        if self.cfg.confirm_changes && stmts.iter().any(|s| !sql::is_read_only(self.cfg.kind, s)) {
            self.confirm = Some(stmts);
            return;
        }
        let begin = self.begin();
        self.start_run(tasks, tab, stmts, begin, max_rows);
    }

    /// The dialog that asks for parameter values before a run.
    fn params_ui(&mut self, ctx: &egui::Context, tab: TabId, cx: &mut TabCtx<'_>) {
        let Some(p) = &mut self.params else { return };
        let t = Theme::get(ctx);
        let mut go = false;
        let mut close = false;
        let r = shadcn_egui::Dialog::new(("query-params", tab), tr("Query parameters"))
            .description(tr("Values go into the statement as written: put text in quotes, like 'Berlin'."))
            .width(460.0)
            .show(ctx, |ui| {
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                for (i, (name, value)) in p.names.iter().zip(p.values.iter_mut()).enumerate() {
                    shadcn_egui::label(ui, name);
                    let r = ui.add(
                        shadcn_egui::Input::new(value).placeholder("NULL, 42, 'text'…").width(ui.available_width()),
                    );
                    if i == 0 && p.focus {
                        r.request_focus();
                        p.focus = false;
                    }
                    ui.add_space(4.0);
                }
                let shown: Vec<String> = p
                    .stmts
                    .iter()
                    .map(|s| {
                        params::substitute(self.cfg.kind, s, |n| {
                            p.names.iter().position(|x| x == n).map(|i| p.values[i].clone()).unwrap_or_default()
                        })
                    })
                    .collect();
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                    sql_editor::viewer(ui, egui::Id::new(("params-preview", tab)), &shown.join(";\n"), self.cfg.kind);
                });
                let missing = p.values.iter().any(|v| v.trim().is_empty());
                if missing {
                    ui.label(
                        RichText::new(tr("Every parameter needs a value (NULL for none)."))
                            .size(12.0)
                            .color(t.muted_foreground),
                    );
                }
                shadcn_egui::footer(ui, |ui| {
                    let label = if p.explain.is_some() { tr("Explain") } else { tr("Run") };
                    if ui.add_enabled(!missing, Button::new(label)).clicked() || (enter && !missing) {
                        go = true;
                    }
                    if ui.add(Button::outline(tr("Cancel"))).clicked() {
                        close = true;
                    }
                });
            });
        if r.should_close || close {
            self.params = None;
            return;
        }
        if !go {
            return;
        }
        let Some(p) = self.params.take() else { return };
        let value =
            |n: &str| p.names.iter().position(|x| x == n).map(|i| p.values[i].trim().to_string()).unwrap_or_default();
        let stmts: Vec<String> = p.stmts.iter().map(|s| params::substitute(self.cfg.kind, s, value)).collect();
        cx.actions.push(Action::RememberParams(
            p.names.iter().cloned().zip(p.values.iter().map(|v| v.trim().to_string())).collect(),
        ));
        match p.explain {
            Some(analyze) => {
                if let Some(sql) = stmts.into_iter().next() {
                    self.explain_sql(cx.tasks, tab, sql, analyze);
                }
            }
            None => self.run_ready(cx.tasks, tab, stmts, cx.page_size),
        }
    }

    /// Asks before a run that changes something, on connections that want it.
    fn confirm_ui(&mut self, ctx: &egui::Context, tasks: &Tasks, tab: TabId, max_rows: usize) {
        let Some(stmts) = &self.confirm else { return };
        let kind = self.cfg.kind;
        let changing: Vec<&String> = stmts.iter().filter(|s| !sql::is_read_only(kind, s)).collect();
        let first: String =
            changing.first().map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let first =
            if first.chars().count() > 200 { first.chars().take(200).collect::<String>() + "…" } else { first };
        let description = trf(
            "{n} of the statements change data or structure on {name}:\n\n{first}",
            &[("n", &changing.len()), ("name", &self.cfg.display_name()), ("first", &first)],
        );
        match shadcn_egui::alert_dialog(ctx, ("confirm-changes", tab), tr("Run changes?"), &description, tr("Run")) {
            Some(true) => {
                let stmts = self.confirm.take().unwrap_or_default();
                let begin = self.begin();
                self.start_run(tasks, tab, stmts, begin, max_rows);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    /// The statement to open a transaction with before the next run, in manual mode.
    fn begin(&self) -> Option<String> {
        if self.manual && !self.in_tx { sql::begin_statement(self.cfg.kind).map(str::to_string) } else { None }
    }

    /// Commits or rolls back the open transaction.
    pub fn finish(&mut self, tasks: &Tasks, tab: TabId, commit: bool, max_rows: usize) {
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        self.start_run(tasks, tab, vec![sql.to_string()], None, max_rows);
    }

    /// Rolls back an open transaction before the console closes.
    pub fn roll_back_on_close(&self, tasks: &Tasks) {
        if let (true, Load::Ready(s)) = (self.in_tx, &self.session) {
            let (s, db) = (s.clone(), self.database.clone());
            tasks.runtime().spawn(async move {
                let _ = s.execute(db.as_deref(), "ROLLBACK", 0).await;
            });
        }
    }

    fn start_run(&mut self, tasks: &Tasks, tab: TabId, stmts: Vec<String>, begin: Option<String>, max_rows: usize) {
        if stmts.is_empty() || self.running {
            return;
        }
        self.outputs.clear();
        self.shown_output = 0;
        self.running = true;
        self.started = Some(Instant::now());
        self.stop = Arc::new(AtomicBool::new(false));
        match &self.session {
            Load::Ready(s) => spawn_run(
                tasks,
                tab,
                s.clone(),
                self.database.clone(),
                Batch { begin, stmts },
                max_rows,
                self.stop.clone(),
            ),
            Load::Loading => self.queued = Some(Queued::Run(Batch { begin, stmts })),
            Load::Idle | Load::Failed(_) => {
                self.queued = Some(Queued::Run(Batch { begin, stmts }));
                self.open_session(tasks, tab);
            }
        }
    }

    /// Shows the plan of the statement under the cursor (or the selection's first one).
    fn explain(&mut self, cx: &TabCtx<'_>, tab: TabId, analyze: bool) {
        let Some(sql) = self.statements_to_run(false).into_iter().next() else { return };
        if !self.ask_params(std::slice::from_ref(&sql), Some(analyze), cx.params) {
            self.explain_sql(cx.tasks, tab, sql, analyze);
        }
    }

    fn explain_sql(&mut self, tasks: &Tasks, tab: TabId, sql: String, analyze: bool) {
        if self.running {
            return;
        }
        // An analyzed plan runs the statement, so in manual mode it waits for Commit too.
        let begin = if analyze { self.begin() } else { None };
        self.outputs.clear();
        self.shown_output = 0;
        self.running = true;
        self.started = Some(Instant::now());
        self.stop = Arc::new(AtomicBool::new(false));
        match &self.session {
            Load::Ready(s) => spawn_explain(
                tasks,
                tab,
                s.clone(),
                self.database.clone(),
                self.cfg.kind,
                self.cfg.product,
                sql,
                analyze,
                begin,
            ),
            Load::Loading => self.queued = Some(Queued::Explain(sql, analyze, begin)),
            Load::Idle | Load::Failed(_) => {
                self.queued = Some(Queued::Explain(sql, analyze, begin));
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

    fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        let ctx = ui.ctx().clone();
        self.poll_dialogs(&ctx, cx.actions);
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            self.save(&ctx, cx.actions);
        }
        self.confirm_ui(&ctx, cx.tasks, tab, cx.page_size);
        self.params_ui(&ctx, tab, &mut cx);
        if let (Load::Ready(s), Some(work)) = (&self.session, self.queued.take()) {
            let (s, db) = (s.clone(), self.database.clone());
            match work {
                Queued::Run(batch) => spawn_run(cx.tasks, tab, s, db, batch, cx.page_size, self.stop.clone()),
                Queued::Explain(sql, analyze, begin) => {
                    spawn_explain(cx.tasks, tab, s, db, self.cfg.kind, self.cfg.product, sql, analyze, begin)
                }
            }
        }

        // Ctrl+Enter runs the statement under the cursor (or the selection),
        // Ctrl+Shift+Enter the whole script. Consumed before the editor sees Enter.
        let editor_id = egui::Id::new(("console-editor", tab));
        if ui.memory(|m| m.has_focus(editor_id)) {
            let all = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter));
            let one = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
            if all || one {
                self.run(&cx, tab, all);
            }
            if self.cfg.kind.is_sql() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::ALT, Key::L)) {
                self.format(&ctx, editor_id);
            }
        }
        // egui drops the focus on Escape before the editor sees it, so look at the last frame's.
        if self.find.open
            && self.editor_focused
            && !self.autocomplete.is_open()
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            self.find.close(&ctx, editor_id, &self.sql);
        }
        // Ctrl+F finds in the script, Ctrl+R (or Ctrl+H) also replaces.
        let find = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F));
        let replace =
            ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::R) || i.consume_key(Modifiers::COMMAND, Key::H));
        if find || replace {
            let selected = self.selection.clone().and_then(|r| self.sql.get(r));
            self.find.show(replace, selected);
        }

        ui.horizontal(|ui| {
            let run = ui
                .add_enabled(!self.running, Button::new(tr("▶ Run")).size(ButtonSize::Sm))
                .tooltip(tr("Run the statement under the cursor or the selection (Ctrl+Enter)"));
            if run.clicked() {
                self.run(&cx, tab, false);
            }
            let all = ui
                .add_enabled(!self.running, Button::outline(tr("Run script")).size(ButtonSize::Sm))
                .tooltip(tr("Run every statement (Ctrl+Shift+Enter)"));
            if all.clicked() {
                self.run(&cx, tab, true);
            }
            // Redis runs commands: nothing to explain or format.
            if self.cfg.kind.is_sql() {
                let explain = ui
                    .add_enabled(!self.running, Button::outline(tr("Explain")).size(ButtonSize::Sm))
                    .tooltip(tr("Show the plan of the statement under the cursor without running it"));
                if explain.clicked() {
                    self.explain(&cx, tab, false);
                }
                if explain::supports_analyze(self.cfg.kind) {
                    let analyze = ui
                        .add_enabled(!self.running, Button::outline(tr("Explain analyze")).size(ButtonSize::Sm))
                        .tooltip(tr("Run the statement and show the plan with the actual times and rows"));
                    if analyze.clicked() {
                        self.explain(&cx, tab, true);
                    }
                }
                let format = ui
                    .add(Button::outline(tr("Format SQL")).size(ButtonSize::Sm))
                    .tooltip(tr("Lay out the selection or the whole script, one clause per line (Ctrl+Alt+L)"));
                if format.clicked() {
                    self.format(&ctx, editor_id);
                }
            }
            if self.running {
                if ui
                    .add(Button::destructive(tr("Stop")).size(ButtonSize::Sm))
                    .tooltip(tr("Cancel the running statement on the server"))
                    .clicked()
                {
                    self.stop(cx.tasks, tab);
                }
                ui.add(shadcn_egui::Spinner::new().size(14.0));
                if let Some(t0) = self.started {
                    ui.label(RichText::new(format_elapsed(t0.elapsed())).color(t.muted_foreground));
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
            }
            if sql::begin_statement(self.cfg.kind).is_some() {
                ui.add_space(12.0);
                let modes = [tr("Auto-commit"), tr("Manual commit")];
                let mut mode = Some(usize::from(self.manual));
                ui.add_enabled(!self.in_tx && !self.running, Select::new(&mut mode, &modes).width(150.0)).tooltip(
                    if self.in_tx {
                        tr("Commit or roll back the open transaction first")
                    } else {
                        tr("Manual: statements wait for Commit, and Rollback undoes them")
                    },
                );
                self.manual = mode == Some(1);
                if self.in_tx {
                    ui.add(Badge::new(tr("transaction open")).outline());
                    let commit = ui
                        .add_enabled(!self.running, Button::new(tr("Commit")).size(ButtonSize::Sm))
                        .tooltip(tr("Make the transaction's changes permanent"));
                    if commit.clicked() {
                        self.finish(cx.tasks, tab, true, cx.page_size);
                    }
                    let rollback = ui
                        .add_enabled(!self.running, Button::outline(tr("Rollback")).size(ButtonSize::Sm))
                        .tooltip(tr("Undo the transaction's changes"));
                    if rollback.clicked() {
                        self.finish(cx.tasks, tab, false, cx.page_size);
                    }
                }
            }
            if let Some(dbs) = cx.conn.and_then(|c| c.databases.ready())
                && dbs.len() > 1
            {
                ui.add_space(12.0);
                ui.label(RichText::new(tr("Database")).color(t.muted_foreground));
                let names: Vec<&str> = dbs.iter().map(String::as_str).collect();
                let mut idx = self.database.as_ref().and_then(|d| dbs.iter().position(|x| x == d));
                let before = idx;
                // The transaction belongs to the session of the current database.
                ui.add_enabled(!self.in_tx, Select::new(&mut idx, &names).width(180.0).placeholder(tr("default")));
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
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let history = if self.show_history {
                    ui.add(Button::secondary(tr("History")).size(ButtonSize::Sm))
                } else {
                    toolbar_button(ui, tr("History"))
                };
                if history.tooltip(tr("Statements run on this connection")).clicked() {
                    self.show_history = !self.show_history;
                }
                self.script_menu(ui, &ctx, cx.recent_scripts, cx.actions);
            });
        });
        ui.add_space(6.0);

        if self.show_history {
            self.history_panel(ui, tab, editor_id, cx.history);
        }

        egui::Panel::top(egui::Id::new(("console-split", tab)))
            .resizable(true)
            .default_size(260.0)
            .min_size(80.0)
            .frame(egui::Frame::new())
            .show(ui, |ui| {
                let cursor = self.selection.as_ref().map_or(0, |r| r.start).min(self.sql.len());
                let marks = self.find.ui(ui, editor_id, &mut self.sql, cursor);
                egui::ScrollArea::vertical().id_salt(("console-scroll", tab)).show(ui, |ui| {
                    let conn = cx.conn;
                    let db = conn.and_then(|c| catalog_database(c, self.database.as_deref()));
                    let catalog = || match (conn, &db) {
                        (Some(c), Some(db)) => c.catalog(db),
                        _ => Default::default(),
                    };
                    let height = ui.available_height();
                    let out = sql_editor::editor_with_completion(
                        ui,
                        editor_id,
                        &mut self.sql,
                        self.cfg.kind,
                        height,
                        &mut self.autocomplete,
                        &catalog,
                        &marks,
                    );
                    if out.selection.is_some() {
                        self.selection = out.selection.clone();
                    }
                    self.editor_focused = ui.memory(|m| m.has_focus(editor_id));
                    // Load the tables and columns the popup could offer.
                    if let (Some(c), Some(db)) = (conn, &db)
                        && ui.memory(|m| m.has_focus(editor_id))
                    {
                        cx.actions.extend(c.catalog_loads(db, &out.wants, &out.want_schemas));
                    }
                });
            });

        ui.add_space(8.0);
        self.results(ui, tab, cx.actions, cx.conn_id);
    }

    fn script_menu(&mut self, ui: &mut Ui, ctx: &egui::Context, recent: &[PathBuf], actions: &mut Vec<Action>) {
        let trigger = toolbar_button(ui, tr("Script"));
        dropdown_menu(&trigger, 260.0, |ui| {
            if menu_item(ui, tr("Open…"), None).clicked() {
                let dialog = rfd::AsyncFileDialog::new().add_filter("SQL", &["sql"]);
                self.pending_open = Some(files::pick_file(ctx, dialog));
            }
            if menu_item(ui, tr("Save"), Some("Ctrl+S")).clicked() {
                self.save(ctx, actions);
            }
            if menu_item(ui, tr("Save as…"), None).clicked() {
                self.save_as(ctx);
            }
            if !recent.is_empty() {
                menu_separator(ui);
                menu_label(ui, tr("Recent scripts"));
                for path in recent {
                    let name = path.file_name().map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy());
                    if menu_item(ui, &name, None).on_hover_text(path.to_string_lossy()).clicked() {
                        self.open(path.clone(), ctx, actions);
                    }
                }
            }
        });
    }

    /// The statements run on this connection, newest first; a click adds one to the script.
    fn history_panel(&mut self, ui: &mut Ui, tab: TabId, editor_id: egui::Id, history: &History) {
        let t = Theme::get(ui.ctx());
        let mut picked = None;
        egui::Panel::right(egui::Id::new(("console-history", tab)))
            .resizable(true)
            .default_size(340.0)
            .min_size(220.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, ..Default::default() }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr("Query history")).strong().color(t.foreground));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(Button::ghost("×").size(ButtonSize::Sm)).clicked() {
                            self.show_history = false;
                        }
                    });
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.history_search)
                        .hint_text(tr("Search"))
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                let now = chrono::Local::now();
                egui::ScrollArea::vertical().id_salt(("console-history-scroll", tab)).show(ui, |ui| {
                    let mut any = false;
                    for (i, e) in history.find(self.cfg.id, &self.history_search).take(HISTORY_SHOWN).enumerate() {
                        any = true;
                        let bg = ui.painter().add(egui::Shape::Noop);
                        let inner = egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.spacing_mut().interact_size.y = 0.0;
                            ui.horizontal(|ui| {
                                let meta = format!("{} · {}", history_time(e.at, &now), e.summary);
                                let color = if e.ok { t.muted_foreground } else { t.destructive };
                                ui.label(RichText::new(meta).size(11.0).color(color));
                                if let Some(db) = &e.database {
                                    ui.label(RichText::new(db).size(11.0).color(t.muted_foreground));
                                }
                            });
                            let preview: Vec<&str> =
                                e.sql.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).collect();
                            for line in preview {
                                ui.add(
                                    egui::Label::new(RichText::new(line).monospace().size(12.0).color(t.foreground))
                                        .truncate()
                                        .selectable(false),
                                );
                            }
                        });
                        let r = ui
                            .interact(
                                inner.response.rect,
                                egui::Id::new(("history-entry", tab, i)),
                                egui::Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .tooltip(tr("Click to add it to the script"));
                        if r.hovered() {
                            ui.painter().set(bg, egui::Shape::rect_filled(inner.response.rect, 6.0, t.accent));
                        }
                        if r.clicked() {
                            picked = Some(e.sql.clone());
                        }
                        r.context_menu(|ui| {
                            if ui.button(tr("Add to the script")).clicked() {
                                picked = Some(e.sql.clone());
                            }
                            if ui.button(tr("Copy")).clicked() {
                                ui.ctx().copy_text(e.sql.clone());
                            }
                        });
                    }
                    if !any {
                        ui.label(RichText::new(tr("Nothing here yet")).color(t.muted_foreground));
                    }
                });
            });
        if let Some(sql) = picked {
            self.insert(ui.ctx(), editor_id, &sql);
        }
    }

    fn results(&mut self, ui: &mut Ui, tab: TabId, actions: &mut Vec<Action>, conn: uuid::Uuid) {
        let t = Theme::get(ui.ctx());
        if self.outputs.is_empty() {
            if !self.running {
                ui.label(RichText::new(tr("Write SQL above and press Ctrl+Enter.")).color(t.muted_foreground));
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
        let Some(o) = self.outputs.get_mut(self.shown_output) else { return };
        ui.horizontal(|ui| {
            // The summary first, so the statement is cut to the width left.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(&o.summary).color(t.muted_foreground));
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let first_line = o.sql.lines().next().unwrap_or_default();
                    let label = RichText::new(first_line).monospace().color(t.muted_foreground);
                    ui.add(egui::Label::new(label).truncate());
                });
            });
        });
        if let Some(e) = &o.error {
            error_alert(ui, if o.cancelled { tr("Statement cancelled") } else { tr("Statement failed") }, e);
        }
        if let Some(p) = &o.plan {
            plan_view(ui, tab, p);
        }
        if let Some(g) = &mut o.grid {
            let chartable = chart::default_spec(&g.result);
            if chartable.is_none() {
                o.chart = None;
            }
            ui.horizontal(|ui| {
                // Table, Chart (when there are numbers to plot) and Pivot.
                let mut modes = vec![tr("Table")];
                if chartable.is_some() {
                    modes.push(tr("Chart"));
                }
                modes.push(tr("Pivot"));
                let pivot_mode = modes.len() - 1;
                let mut view = if o.pivot.is_some() { pivot_mode } else { usize::from(o.chart.is_some()) };
                if toggle_group(ui, &mut view, &modes).changed() {
                    o.chart = chartable.clone().filter(|_| view == 1 && view != pivot_mode);
                    o.pivot = (view == pivot_mode).then(|| pivot::default_spec(&g.result));
                }
                ui.add_space(8.0);
                if grid::export_menu(ui, &g.result, self.cfg.kind, None, true) {
                    let source = ExportSource::Query { database: self.database.clone(), sql: o.sql.clone() };
                    actions.push(Action::ExportData(conn, source));
                }
                if o.chart.is_none() && o.pivot.is_none() {
                    grid::value_button(ui, &mut self.value.open);
                }
            });
            ui.add_space(4.0);
            if let Some(spec) = &mut o.pivot {
                pivot_view::controls(ui, &g.result, spec);
                ui.add_space(6.0);
                pivot_view::show(ui, egui::Id::new(("console-pivot", tab, self.shown_output)), &g.result, spec);
                return;
            }
            if let Some(spec) = &mut o.chart {
                chart_view::controls(ui, &g.result, spec);
                ui.add_space(6.0);
                chart_view::show(ui, &g.result, spec, ui.available_height() - 8.0);
                return;
            }
            self.value.show(ui, egui::Id::new(("console-value", tab)), g.current_value());
            if g.show(ui, egui::Id::new(("console-grid", tab, self.shown_output))) {
                self.value.open = true;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_explain(
    tasks: &Tasks,
    tab: TabId,
    session: Arc<dyn Connection>,
    database: Option<String>,
    kind: DbKind,
    product: Product,
    sql: String,
    analyze: bool,
    begin: Option<String>,
) {
    let tx = tasks.sender();
    tasks.runtime().spawn(async move {
        if !open_transaction(&tx, tab, &session, database.as_deref(), begin).await {
            return;
        }
        let (elapsed, result) =
            timed(dbman_drivers::explain(&session, database.as_deref(), kind, product, &sql, analyze)).await;
        tx.send(Event::Tab { tab, event: TabEvent::Plan { sql, elapsed, result } });
        tx.send(Event::Tab { tab, event: TabEvent::RunFinished });
    });
}

/// "1234", "0.25", "1.2e9" for plan numbers.
fn plan_number(v: f64) -> String {
    if v.abs() >= 1e9 {
        format!("{v:.2e}")
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else if v.abs() >= 100.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.3}").trim_end_matches('0').to_string()
    }
}

/// A number of a plan step.
type Metric = fn(&PlanNode) -> Option<f64>;

/// A plan as an indented tree with its numbers, and a bar for each step's
/// share of the time (or, without times, of the cost).
fn plan_view(ui: &mut Ui, tab: TabId, plan: &Plan) {
    let t = Theme::get(ui.ctx());
    ui.horizontal(|ui| {
        if toolbar_button(ui, tr("Copy plan")).clicked() {
            ui.ctx().copy_text(plan.raw.clone());
            toast(ui.ctx(), tr("Copied"), None);
        }
        for (k, v) in &plan.totals {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("{}: {v}", tr(k))).color(t.muted_foreground));
        }
    });
    ui.add_space(4.0);
    let rows = plan.rows();
    if rows.is_empty() {
        ui.label(RichText::new(tr("The engine reports no plan steps for this statement.")).color(t.muted_foreground));
        return;
    }
    let has = |f: Metric| rows.iter().any(|(_, n)| f(n).is_some());
    let mut columns: Vec<(&str, Metric)> = Vec::new();
    if has(|n| n.rows) {
        columns.push((tr("Rows (est.)"), |n| n.rows));
    }
    if has(|n| n.cost) {
        columns.push((tr("Cost"), |n| n.cost));
    }
    if has(|n| n.actual_rows) {
        columns.push((tr("Rows"), |n| n.actual_rows));
    }
    if has(|n| n.actual_ms) {
        columns.push((tr("Time, ms"), |n| n.actual_ms));
    }
    let share: Option<Metric> = if has(|n| n.actual_ms) {
        Some(|n| n.actual_ms)
    } else if has(|n| n.cost) {
        Some(|n| n.cost)
    } else {
        None
    };
    let max = share.map_or(0.0, |f| rows.iter().filter_map(|(_, n)| f(n)).fold(0.0, f64::max));

    egui::ScrollArea::both().id_salt(("console-plan", tab)).auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new(("plan-grid", tab)).spacing([20.0, 8.0]).show(ui, |ui| {
            ui.label(RichText::new(tr("Step")).strong().color(t.muted_foreground));
            for (name, _) in &columns {
                ui.label(RichText::new(*name).strong().color(t.muted_foreground));
            }
            if share.is_some() {
                ui.label("");
            }
            ui.end_row();
            for (depth, n) in &rows {
                ui.horizontal_top(|ui| {
                    ui.add_space(*depth as f32 * 18.0);
                    let glyph = if *depth == 0 { "" } else { "└ " };
                    ui.vertical(|ui| {
                        ui.set_max_width(640.0);
                        let op = RichText::new(format!("{glyph}{}", n.operation)).monospace().color(t.foreground);
                        ui.label(op);
                        for d in &n.details {
                            let indent = if glyph.is_empty() { "" } else { "  " };
                            let text = RichText::new(format!("{indent}{d}")).size(11.5).color(t.muted_foreground);
                            ui.add(egui::Label::new(text).wrap());
                        }
                    });
                });
                for (_, f) in &columns {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        ui.label(RichText::new(f(n).map(plan_number).unwrap_or_default()).monospace());
                    });
                }
                if let Some(f) = share {
                    let frac = f(n).filter(|_| max > 0.0).map_or(0.0, |v| (v / max) as f32);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(90.0, 8.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 3.0, t.muted);
                    let mut fill = rect;
                    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
                    ui.painter().rect_filled(fill, 3.0, t.primary);
                }
                ui.end_row();
            }
        });
    });
}

/// History entries the panel shows at most.
const HISTORY_SHOWN: usize = 300;

/// "14:03:27" today, "2026-10-12 09:41" on other days, in local time.
fn history_time(at: i64, now: &chrono::DateTime<chrono::Local>) -> String {
    let Some(t) = chrono::DateTime::from_timestamp(at, 0).map(|t| t.with_timezone(&chrono::Local)) else {
        return String::new();
    };
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M:%S").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Reads a script file as text.
pub(crate) fn read_script(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    // A byte order mark from Windows editors is not part of the SQL.
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf".as_slice()).unwrap_or(&bytes);
    String::from_utf8(bytes.to_vec()).map_err(|_| tr("The file is not UTF-8 text").to_string())
}

fn spawn_run(
    tasks: &Tasks,
    tab: TabId,
    session: Arc<dyn Connection>,
    database: Option<String>,
    batch: Batch,
    max_rows: usize,
    stop: Arc<AtomicBool>,
) {
    let tx = tasks.sender();
    tasks.runtime().spawn(async move {
        if !open_transaction(&tx, tab, &session, database.as_deref(), batch.begin).await {
            return;
        }
        for sql in batch.stmts {
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

/// Runs `begin`, if any, before a console run; on failure the run is over.
async fn open_transaction(
    tx: &EventSender,
    tab: TabId,
    session: &Arc<dyn Connection>,
    database: Option<&str>,
    begin: Option<String>,
) -> bool {
    let Some(sql) = begin else { return true };
    let (elapsed, result) = timed(session.execute(database, &sql, 0)).await;
    let ok = result.is_ok();
    tx.send(Event::Tab { tab, event: TabEvent::Transaction(ok) });
    if !ok {
        tx.send(Event::Tab { tab, event: TabEvent::Statement { sql, elapsed, result } });
        tx.send(Event::Tab { tab, event: TabEvent::RunFinished });
    }
    ok
}

/// "0.4 s", "12 s", "3 min 05 s" for the running-query timer.
pub(crate) fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 10 {
        trf("{s} s", &[("s", &format!("{:.1}", d.as_secs_f64()))])
    } else if secs < 60 {
        trf("{s} s", &[("s", &secs)])
    } else {
        trf("{m} min {s} s", &[("m", &(secs / 60)), ("s", &format!("{:02}", secs % 60))])
    }
}

/// "4 rows in 12 ms" and the like, translated (`StatementOutcome::summary` is English only).
pub(crate) fn outcome_summary(outcome: &StatementOutcome, elapsed: Duration) -> String {
    let ms = format!("{:.0}", elapsed.as_secs_f64() * 1000.0);
    match outcome {
        StatementOutcome::Rows(r) => {
            let n = format!("{}{}", r.rows.len(), if r.truncated { "+" } else { "" });
            trf("{n} rows in {ms} ms", &[("n", &n), ("ms", &ms)])
        }
        StatementOutcome::Done { affected: Some(n) } => trf("{n} rows affected in {ms} ms", &[("n", n), ("ms", &ms)]),
        StatementOutcome::Done { affected: None } => trf("completed in {ms} ms", &[("ms", &ms)]),
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
        let labels: Vec<&str> = sections.iter().map(|s| tr(s)).collect();
        ui.horizontal(|ui| {
            tabs(ui, &mut self.section, &labels);
            ui.add_space(8.0);
            if toolbar_button(ui, tr("⟳ Refresh")).clicked() {
                self.details = Load::Idle;
                self.ddl = Load::Idle;
            }
            if self.object.kind.has_data() && toolbar_button(ui, tr("Open data")).clicked() {
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
                Load::Failed(e) => error_alert(ui, tr("Could not get DDL"), e),
                _ => {
                    ui.add(shadcn_egui::Spinner::new());
                }
            }
            return;
        }
        let d = match &self.details {
            Load::Ready(d) => d,
            Load::Failed(e) => return error_alert(ui, tr("Could not read the structure"), e),
            _ => {
                ui.add(shadcn_egui::Spinner::new());
                return;
            }
        };
        let id = egui::Id::new(("object-table", tab, self.section));
        let (columns, rows): (Vec<Column>, Vec<Vec<String>>) = match section {
            "Columns" => (
                vec![
                    Column::new(tr("Name")).sortable().weight(1.2),
                    Column::new(tr("Type")).sortable().weight(1.2),
                    Column::new(tr("Nullable")).weight(0.6),
                    Column::new(tr("Default")),
                    Column::new(tr("Key")).weight(0.5),
                    Column::new(tr("Comment")).weight(1.4),
                ],
                d.columns
                    .iter()
                    .map(|c| {
                        vec![
                            c.name.clone(),
                            c.data_type.clone(),
                            if c.nullable { tr("yes").into() } else { tr("no").into() },
                            c.default.clone().unwrap_or_default(),
                            if c.primary_key {
                                "PK".into()
                            } else if c.auto_increment {
                                tr("auto").into()
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
                    Column::new(tr("Name")).sortable(),
                    Column::new(tr("Columns")).weight(1.5),
                    Column::new(tr("Unique")).weight(0.5),
                    Column::new(tr("Method")).weight(0.5),
                ],
                d.indexes
                    .iter()
                    .map(|i| {
                        vec![
                            i.name.clone(),
                            i.columns.join(", "),
                            if i.primary {
                                tr("primary").into()
                            } else if i.unique {
                                tr("yes").into()
                            } else {
                                tr("no").into()
                            },
                            i.method.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            ),
            _ => (
                vec![
                    Column::new(tr("Name")).sortable(),
                    Column::new(tr("Columns")),
                    Column::new(tr("References")).weight(1.5),
                    Column::new(tr("On update")).weight(0.6),
                    Column::new(tr("On delete")).weight(0.6),
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
            ui.label(RichText::new(tr("none")).color(Theme::get(ui.ctx()).muted_foreground));
            return;
        }
        DataTable::new(id, columns)
            .row_height(32.0)
            .max_height((ui.available_height() - 60.0).max(100.0))
            .show(ui, &rows);
    }
}

/// The database a console's names are looked up in: the one picked, the
/// connection's own, or the only or first one listed.
fn catalog_database(c: &ConnState, picked: Option<&str>) -> Option<String> {
    let dbs = c.databases.ready()?;
    picked
        .filter(|p| dbs.iter().any(|d| d == p))
        .or_else(|| Some(c.cfg.database.as_str()).filter(|d| dbs.iter().any(|x| x == d)))
        .or_else(|| dbs.first().map(String::as_str))
        .map(String::from)
}
