//! Comparison tab: a schema (the source) against another schema on any
//! connection (the target). "Structure" lists the objects that are new,
//! changed or only in the target; "Data" matches the rows of each table by
//! primary key. Both build a script that makes the target match the source,
//! which can be copied, saved, opened in a console or run as a background job.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dbman_core::compare::{self, DataDiff, DataSpec, ObjectDiff, Snapshot, Status};
use dbman_core::ddl::Plan;
use dbman_core::{Connection, ConnectionConfig, DbError, DbKind, DbResult, ObjectPath, TableDetails};
use egui::{Color32, RichText, Ui};
use shadcn_egui::{Button, ButtonSize, Checkbox, DiffView, Empty, Theme, TooltipExt, alert_dialog, tabs, toast};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::events::{Event, EventSender, TabEvent, TabId};
use crate::explorer::{Action, Load};
use crate::files;
use crate::i18n::{tr, trf};
use crate::jobs::JobSpec;
use crate::tabs::{TabCtx, error_alert, toolbar_button};
use crate::target_picker::TargetPicker;
use crate::transfer_dialog::Start;

/// Script text shown in previews; Copy and Save give all of it.
const PREVIEW_CHARS: usize = 200_000;

type Session = crate::target_picker::Session;

pub enum CompareEvent {
    Progress(String),
    Schema(DbResult<Vec<ObjectDiff>>),
    DataTables(DbResult<Vec<DataRow>>),
    DataTable(usize, Result<DataDiff, String>),
    DataDone,
}

#[derive(Debug, Clone)]
pub enum DataState {
    /// Not compared, and why.
    Skipped(String),
    Pending,
    Done(DataDiff),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct DataRow {
    pub name: String,
    pub state: DataState,
    pub include: bool,
    /// Tables this one references in the target (for the script's order).
    pub references: Vec<String>,
    spec: Option<DataSpec>,
    source: ObjectPath,
}

struct SchemaRow {
    diff: ObjectDiff,
    include: bool,
}

/// What the results were compared against; the script goes there.
#[derive(Clone)]
struct Compared {
    conn: Uuid,
    cfg: ConnectionConfig,
    database: String,
    schema: String,
}

pub struct CompareView {
    pub database: String,
    pub schema: String,
    source_cfg: ConnectionConfig,
    source: Session,
    picker: TargetPicker,
    mode: usize,
    running: bool,
    cancel: Arc<AtomicBool>,
    progress: String,
    structure: Load<Vec<SchemaRow>>,
    data: Load<Vec<DataRow>>,
    compared: Option<Compared>,
    show_same: bool,
    selected: Option<usize>,
    show_script: bool,
    /// The script for the chosen rows, rebuilt when the choice changes.
    script: Option<(Plan, String)>,
    confirm: bool,
}

impl CompareView {
    pub fn new(cfg: ConnectionConfig, database: String, schema: String) -> Self {
        let picker = TargetPicker::new(&database, &schema);
        Self {
            database,
            schema,
            source_cfg: cfg,
            source: Arc::new(OnceCell::new()),
            picker,
            mode: 0,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: String::new(),
            structure: Load::Idle,
            data: Load::Idle,
            compared: None,
            show_same: false,
            selected: None,
            show_script: false,
            script: None,
            confirm: false,
        }
    }

    pub fn title(&self) -> String {
        if self.schema == self.database || self.schema.is_empty() {
            trf("{name} (compare)", &[("name", &self.database)])
        } else {
            trf("{name} (compare)", &[("name", &format!("{}.{}", self.database, self.schema))])
        }
    }

    /// Picks the target connection (tests).
    pub fn set_target(&mut self, conn: Uuid, cfg: ConnectionConfig, tasks: &crate::events::Tasks, tab: TabId) {
        self.picker.set_target(conn, cfg, tasks, tab);
    }

    /// Chooses the target database and schema by name (tests).
    pub fn choose(&mut self, database: &str, schema: &str) {
        self.picker.choose(database, schema);
    }

    pub fn target_database(&self) -> Option<&str> {
        self.picker.database()
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// The structure results (tests).
    pub fn structure(&self) -> Option<Vec<(&str, Status)>> {
        self.structure.ready().map(|rows| rows.iter().map(|r| (r.diff.name.as_str(), r.diff.status)).collect())
    }

    /// The data results (tests).
    pub fn data(&self) -> Option<&[DataRow]> {
        self.data.ready().map(Vec::as_slice)
    }

    /// The script for the chosen rows (tests).
    pub fn script_text(&mut self) -> String {
        self.ensure_script();
        self.script.as_ref().map(|(_, s)| s.clone()).unwrap_or_default()
    }

    /// Shows the details of the row with this name (tests).
    pub fn select(&mut self, name: &str) {
        self.show_script = false;
        self.selected = if self.mode == 0 {
            self.structure.ready().and_then(|rows| rows.iter().position(|r| r.diff.name == name))
        } else {
            self.data.ready().and_then(|rows| rows.iter().position(|r| r.name == name))
        };
    }

    pub fn set_mode(&mut self, data: bool) {
        self.mode = usize::from(data);
        self.selected = None;
        self.script = None;
    }

    fn source_cfg(&self) -> ConnectionConfig {
        let mut cfg = self.source_cfg.clone();
        if cfg.kind == DbKind::MySql {
            cfg.database = self.database.clone();
        }
        cfg
    }

    fn send(tx: &EventSender, tab: TabId, ev: CompareEvent) {
        tx.send(Event::Tab { tab, event: TabEvent::Compare(Box::new(ev)) });
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        let ev = match ev {
            TabEvent::Picker(e) => return self.picker.on_event(e),
            TabEvent::Compare(ev) => ev,
            _ => return,
        };
        match *ev {
            CompareEvent::Progress(p) => self.progress = p,
            CompareEvent::Schema(r) => {
                self.running = false;
                self.structure = Load::from_result(r.map(|diffs| {
                    diffs
                        .into_iter()
                        .map(|diff| {
                            // Dropping is never chosen by default.
                            let include = matches!(diff.status, Status::OnlySource | Status::Changed);
                            SchemaRow { diff, include }
                        })
                        .collect()
                }));
                self.script = None;
            }
            CompareEvent::DataTables(r) => {
                self.data = Load::from_result(r);
                self.script = None;
            }
            CompareEvent::DataTable(i, r) => {
                if let Load::Ready(rows) = &mut self.data
                    && let Some(row) = rows.get_mut(i)
                {
                    row.state = match r {
                        Ok(d) => {
                            row.include = d.differs();
                            DataState::Done(d)
                        }
                        Err(e) => DataState::Failed(e),
                    };
                }
                self.script = None;
            }
            CompareEvent::DataDone => {
                self.running = false;
                self.progress.clear();
                if let Load::Ready(rows) = &mut self.data {
                    for r in rows.iter_mut().filter(|r| matches!(r.state, DataState::Pending)) {
                        r.state = DataState::Skipped(tr("Cancelled").into());
                    }
                }
            }
        }
    }

    fn start(&mut self, cx: &TabCtx<'_>, tab: TabId) {
        let (Some(conn), Some(db), Some(schema), Some(target_cfg)) = (
            self.picker.conn(),
            self.picker.database().map(str::to_string),
            self.picker.schema().map(str::to_string),
            self.picker.cfg(),
        ) else {
            return;
        };
        self.compared = Some(Compared { conn, cfg: target_cfg.clone(), database: db.clone(), schema: schema.clone() });
        self.cancel = Arc::new(AtomicBool::new(false));
        self.running = true;
        self.selected = None;
        self.script = None;
        self.show_script = false;
        self.progress = tr("Connecting…").into();
        let (source, target) = (self.source.clone(), self.picker.session());
        let source_cfg = self.source_cfg();
        let (sdb, sschema) = (self.database.clone(), self.schema.clone());
        let cancel = self.cancel.clone();
        let tx = cx.tasks.sender();
        let data = self.mode == 1;
        if data {
            self.data = Load::Loading;
        } else {
            self.structure = Load::Loading;
        }
        cx.tasks.runtime().spawn(async move {
            let sessions = async {
                let s = source.get_or_try_init(|| dbman_drivers::connect(&source_cfg)).await?.clone();
                let t = target.get_or_try_init(|| dbman_drivers::connect(&target_cfg)).await?.clone();
                Ok::<_, DbError>((s, t))
            }
            .await;
            let (s, t) = match sessions {
                Ok(p) => p,
                Err(e) => {
                    let ev = if data { CompareEvent::DataTables(Err(e)) } else { CompareEvent::Schema(Err(e)) };
                    Self::send(&tx, tab, ev);
                    if data {
                        Self::send(&tx, tab, CompareEvent::DataDone);
                    }
                    return;
                }
            };
            let read = |which: &'static str, conn: Arc<dyn Connection>, db: String, schema: String, ddl: bool| {
                let (tx, cancel) = (tx.clone(), cancel.clone());
                async move {
                    let progress = |done: usize, total: usize| {
                        let text = if which == "source" {
                            trf("Reading the source {done}/{total}…", &[("done", &done), ("total", &total)])
                        } else {
                            trf("Reading the target {done}/{total}…", &[("done", &done), ("total", &total)])
                        };
                        Self::send(&tx, tab, CompareEvent::Progress(text));
                    };
                    dbman_drivers::compare::snapshot(&conn, &db, &schema, ddl, progress, &cancel).await
                }
            };
            if !data {
                let r = async {
                    if s.kind() != t.kind() {
                        return Err(DbError::new(trf(
                            "Structure can only be compared between databases of the same kind ({a} and {b} here). \
                             Data can be compared between any two.",
                            &[("a", &s.kind().label()), ("b", &t.kind().label())],
                        )));
                    }
                    let a = read("source", s.clone(), sdb, sschema, true).await?;
                    let b = read("target", t.clone(), db, schema, true).await?;
                    Ok(compare::compare(t.kind(), &a, &b))
                }
                .await;
                Self::send(&tx, tab, CompareEvent::Schema(r));
                return;
            }
            let tables = async {
                let a = read("source", s.clone(), sdb, sschema, false).await?;
                let b = read("target", t.clone(), db, schema, false).await?;
                Ok::<_, DbError>(data_rows(&a, &b))
            }
            .await;
            let rows = match tables {
                Ok(rows) => rows,
                Err(e) => {
                    Self::send(&tx, tab, CompareEvent::DataTables(Err(e)));
                    Self::send(&tx, tab, CompareEvent::DataDone);
                    return;
                }
            };
            Self::send(&tx, tab, CompareEvent::DataTables(Ok(rows.clone())));
            for (i, row) in rows.iter().enumerate() {
                let Some(spec) = row.spec.clone() else { continue };
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let name = row.name.clone();
                let progress = {
                    let tx = tx.clone();
                    move |n: u64| {
                        Self::send(
                            &tx,
                            tab,
                            CompareEvent::Progress(trf("{name}: {n} rows read…", &[("name", &name), ("n", &n)])),
                        )
                    }
                };
                let r = dbman_drivers::compare::compare_data(&s, &row.source, &t, spec, &progress, &cancel).await;
                if r.is_err() && cancel.load(Ordering::Relaxed) {
                    break;
                }
                Self::send(&tx, tab, CompareEvent::DataTable(i, r.map_err(|e| e.message)));
            }
            Self::send(&tx, tab, CompareEvent::DataDone);
        });
    }

    fn kind(&self) -> DbKind {
        self.compared.as_ref().map_or(self.source_cfg.kind, |c| c.cfg.kind)
    }

    fn ensure_script(&mut self) {
        if self.script.is_some() {
            return;
        }
        let kind = self.kind();
        let plan = if self.mode == 0 {
            let Some(rows) = self.structure.ready() else { return };
            let chosen: Vec<&ObjectDiff> =
                rows.iter().filter(|r| r.include && r.diff.status != Status::Same).map(|r| &r.diff).collect();
            compare::sync_plan(&chosen)
        } else {
            let Some(rows) = self.data.ready() else { return };
            let done: Vec<(&DataRow, &DataDiff)> = rows
                .iter()
                .filter(|r| r.include)
                .filter_map(|r| match &r.state {
                    DataState::Done(d) if d.differs() => Some((r, d)),
                    _ => None,
                })
                .collect();
            let order = compare::table_order(
                &done.iter().map(|(r, _)| (r.name.clone(), r.references.clone())).collect::<Vec<_>>(),
            );
            let ordered: Vec<&DataDiff> = order.iter().map(|&i| done[i].1).collect();
            compare::data_plan(&ordered)
        };
        let text = compare::script(kind, &plan);
        self.script = Some((plan, text));
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        self.picker.ensure(cx.tasks, tab, cx.conns, cx.conn_id, self.source_cfg.kind);
        self.toolbar(ui, tab, &cx);
        ui.add_space(6.0);

        let (load_failed, has_results) = match self.mode {
            0 => (
                if let Load::Failed(e) = &self.structure { Some(e.clone()) } else { None },
                self.structure.ready().is_some(),
            ),
            _ => (if let Load::Failed(e) = &self.data { Some(e.clone()) } else { None }, self.data.ready().is_some()),
        };
        if let Some(e) = load_failed {
            return error_alert(ui, tr("Could not compare"), &e);
        }
        if !has_results {
            let t = Theme::get(ui.ctx());
            if self.running {
                ui.horizontal(|ui| {
                    ui.add(shadcn_egui::Spinner::new());
                    ui.label(RichText::new(&self.progress).color(t.muted_foreground));
                });
            } else {
                let what = if self.mode == 0 {
                    tr(
                        "Compare finds the tables, views and routines that differ and writes the script that makes the target match the source.",
                    )
                } else {
                    tr(
                        "Compare matches the rows of each table by primary key and writes the INSERT, UPDATE and DELETE statements that make the target match the source.",
                    )
                };
                ui.label(RichText::new(what).color(t.muted_foreground));
            }
            return;
        }

        egui::Panel::bottom(egui::Id::new(("compare-actions", tab)))
            .resizable(false)
            .show(ui, |ui| self.actions(ui, &mut cx));
        egui::Panel::left(egui::Id::new(("compare-list", tab)))
            .resizable(true)
            .default_size(380.0)
            .min_size(260.0)
            .show(ui, |ui| {
                if self.mode == 0 {
                    self.structure_list(ui);
                } else {
                    self.data_list(ui);
                }
            });
        ui.add_space(4.0);
        if self.show_script {
            self.script_view(ui);
        } else if self.mode == 0 {
            self.structure_detail(ui);
        } else {
            self.data_detail(ui);
        }
        if self.confirm {
            self.confirm_dialog(ui, cx);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, tab: TabId, cx: &TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        ui.horizontal_wrapped(|ui| {
            let before = self.mode;
            tabs(ui, &mut self.mode, &[tr("Structure"), tr("Data")]);
            if before != self.mode {
                self.selected = None;
                self.script = None;
                self.show_script = false;
            }
            ui.add_space(12.0);
            ui.label(RichText::new(tr("Source")).color(t.muted_foreground));
            let source = if self.schema == self.database || self.schema.is_empty() {
                format!("{} · {}", self.source_cfg.display_name(), self.database)
            } else {
                format!("{} · {}.{}", self.source_cfg.display_name(), self.database, self.schema)
            };
            ui.label(RichText::new(source).strong());
            ui.add_space(8.0);
            ui.label(RichText::new(tr("Target")).color(t.muted_foreground));

            self.picker.ui(ui, cx.tasks, tab, cx.conns);
            ui.add_space(8.0);
            let ready = self.picker.schema().is_some() && !self.running;
            let label = if self.mode == 0 { tr("Compare structure") } else { tr("Compare data") };
            if ui.add_enabled(ready, Button::new(label).size(ButtonSize::Sm)).clicked() {
                self.start(cx, tab);
            }
            if self.running {
                if toolbar_button(ui, tr("Stop")).clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                ui.add(shadcn_egui::Spinner::new());
                ui.label(RichText::new(&self.progress).color(t.muted_foreground));
            }
        });
    }

    fn structure_list(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Load::Ready(rows) = &mut self.structure else { return };
        let count = |s: Status| rows.iter().filter(|r| r.diff.status == s).count();
        let (new, changed, extra, same) =
            (count(Status::OnlySource), count(Status::Changed), count(Status::OnlyTarget), count(Status::Same));
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(trf(
                    "{new} new · {changed} changed · {extra} only in target · {same} same",
                    &[("new", &new), ("changed", &changed), ("extra", &extra), ("same", &same)],
                ))
                .size(12.0)
                .color(t.muted_foreground),
            );
        });
        ui.add(Checkbox::new(&mut self.show_same).label(tr("Show identical objects")));
        ui.add_space(4.0);
        let mut changed_choice = false;
        egui::ScrollArea::vertical().id_salt("compare-structure").auto_shrink([false, false]).show(ui, |ui| {
            if rows.iter().all(|r| r.diff.status == Status::Same) {
                ui.label(RichText::new(tr("The schemas are the same.")).color(t.muted_foreground));
            }
            for (i, row) in rows.iter_mut().enumerate() {
                if row.diff.status == Status::Same && !self.show_same {
                    continue;
                }
                ui.horizontal(|ui| {
                    ui.push_id(i, |ui| {
                        let enabled = row.diff.status != Status::Same;
                        if ui.add_enabled(enabled, Checkbox::new(&mut row.include)).changed() {
                            changed_choice = true;
                        }
                    });
                    status_badge(ui, &t, row.diff.status);
                    let text = trf("{kind} {name}", &[("kind", &tr(row.diff.kind.label())), ("name", &row.diff.name)]);
                    let r = ui.selectable_label(self.selected == Some(i) && !self.show_script, text);
                    if r.clicked() {
                        self.selected = Some(i);
                        self.show_script = false;
                    }
                });
            }
        });
        if changed_choice {
            self.script = None;
        }
    }

    fn structure_detail(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let kind = self.kind();
        let Some(row) = self.selected.and_then(|i| self.structure.ready()?.get(i)) else {
            Empty::new(tr("Nothing selected"))
                .description(tr("Choose an object to see what differs, or Show script for all of them."))
                .show(ui, |_| {});
            return;
        };
        let d = &row.diff;
        egui::ScrollArea::vertical().id_salt("compare-detail").auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(trf("{kind} {name}", &[("kind", &tr(d.kind.label())), ("name", &d.name)]))
                        .strong()
                        .size(15.0),
                );
                status_badge(ui, &t, d.status);
            });
            ui.add_space(6.0);
            for c in &d.changes {
                ui.label(format!("• {c}"));
            }
            for n in &d.notes {
                ui.label(RichText::new(n).color(t.muted_foreground));
            }
            let plan = d.plan();
            if !plan.statements.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(tr("Statements")).strong());
                code(ui, &t, &compare::script(kind, &Plan { statements: plan.statements, notes: Vec::new() }), true);
            }
            ui.add_space(8.0);
            ui.columns(2, |cols| {
                cols[0].label(RichText::new(tr("Source")).strong());
                cols[1].label(RichText::new(tr("Target")).strong());
            });
            match (&d.source_ddl, &d.target_ddl) {
                // Both sides exist and differ: the changed lines are highlighted.
                (Some(source), Some(target)) if source != target => {
                    let diff = ddl_diff(source, target);
                    DiffView::new(("compare-ddl", &d.name), &diff).split(true).max_height(600.0).show(ui);
                }
                (source, target) => {
                    ui.columns(2, |cols| {
                        code(&mut cols[0], &t, source.as_deref().unwrap_or("-- not in the source"), true);
                        code(&mut cols[1], &t, target.as_deref().unwrap_or("-- not in the target"), true);
                    });
                }
            }
        });
    }

    fn data_list(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Load::Ready(rows) = &mut self.data else { return };
        let differing = rows.iter().filter(|r| matches!(&r.state, DataState::Done(d) if d.differs())).count();
        let done = rows.iter().filter(|r| matches!(r.state, DataState::Done(_))).count();
        ui.label(
            RichText::new(trf(
                "{done} of {total} tables compared · {differing} differ",
                &[("done", &done), ("total", &rows.len()), ("differing", &differing)],
            ))
            .size(12.0)
            .color(t.muted_foreground),
        );
        ui.add(Checkbox::new(&mut self.show_same).label(tr("Show identical tables")));
        ui.add_space(4.0);
        let mut changed_choice = false;
        egui::ScrollArea::vertical().id_salt("compare-data").auto_shrink([false, false]).show(ui, |ui| {
            for (i, row) in rows.iter_mut().enumerate() {
                let differs = matches!(&row.state, DataState::Done(d) if d.differs());
                if matches!(row.state, DataState::Done(_)) && !differs && !self.show_same {
                    continue;
                }
                ui.horizontal(|ui| {
                    ui.push_id(i, |ui| {
                        if ui.add_enabled(differs, Checkbox::new(&mut row.include)).changed() {
                            changed_choice = true;
                        }
                    });
                    let r = ui.selectable_label(self.selected == Some(i) && !self.show_script, &row.name);
                    if r.clicked() {
                        self.selected = Some(i);
                        self.show_script = false;
                    }
                    let (text, color) = match &row.state {
                        DataState::Done(d) if d.differs() => {
                            (format!("+{} ~{} −{}", d.inserted, d.updated, d.deleted), t.foreground)
                        }
                        DataState::Done(d) => (trf("{n} rows, same", &[("n", &d.same)]), t.muted_foreground),
                        DataState::Pending => ("…".into(), t.muted_foreground),
                        DataState::Skipped(why) => (why.clone(), t.muted_foreground),
                        DataState::Failed(_) => (tr("failed").into(), t.destructive),
                    };
                    ui.label(RichText::new(text).size(12.0).color(color))
                        .on_hover_text(tr("Rows to insert (+), update (~) and delete (−)"));
                });
            }
        });
        if changed_choice {
            self.script = None;
        }
    }

    fn data_detail(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Some(row) = self.selected.and_then(|i| self.data.ready()?.get(i)) else {
            Empty::new(tr("Nothing selected"))
                .description(tr("Choose a table to see its differing rows, or Show script for all of them."))
                .show(ui, |_| {});
            return;
        };
        ui.label(RichText::new(&row.name).strong().size(15.0));
        ui.add_space(6.0);
        let d = match &row.state {
            DataState::Done(d) => d,
            DataState::Failed(e) => return error_alert(ui, tr("Could not compare this table"), e),
            DataState::Skipped(why) => {
                ui.label(RichText::new(why).color(t.muted_foreground));
                return;
            }
            DataState::Pending => {
                ui.add(shadcn_egui::Spinner::new());
                return;
            }
        };
        ui.label(trf(
            "Source {source} rows · target {target} rows · {same} same · {insert} to insert · {update} to update · {delete} to delete",
            &[
                ("source", &d.source_rows),
                ("target", &d.target_rows),
                ("same", &d.same),
                ("insert", &d.inserted),
                ("update", &d.updated),
                ("delete", &d.deleted),
            ],
        ));
        if d.samples.is_empty() {
            return;
        }
        ui.add_space(8.0);
        let shown = d.samples.len() as u64;
        let total = d.inserted + d.updated + d.deleted;
        if shown < total {
            ui.label(
                RichText::new(trf(
                    "The first {shown} of {total} differing rows:",
                    &[("shown", &shown), ("total", &total)],
                ))
                .color(t.muted_foreground),
            );
        }
        egui::ScrollArea::vertical().id_salt("compare-rows").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("compare-rows-grid").striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new(tr("Key")).strong());
                ui.label(RichText::new(tr("Change")).strong());
                ui.label(RichText::new(tr("Columns")).strong());
                ui.end_row();
                for s in &d.samples {
                    ui.label(RichText::new(&s.key).monospace());
                    let what = match s.status {
                        Status::OnlySource => tr("insert"),
                        Status::Changed => tr("update"),
                        Status::OnlyTarget => tr("delete"),
                        Status::Same => "",
                    };
                    ui.label(what);
                    ui.label(s.columns.join(", "));
                    ui.end_row();
                }
            });
        });
    }

    fn script_view(&mut self, ui: &mut Ui) {
        self.ensure_script();
        let t = Theme::get(ui.ctx());
        let Some((plan, text)) = &self.script else { return };
        ui.label(RichText::new(trf("Sync script · {n} statements", &[("n", &plan.statements.len())])).strong());
        ui.add_space(4.0);
        egui::ScrollArea::both().id_salt("compare-script").auto_shrink([false, false]).show(ui, |ui| {
            if text.len() > PREVIEW_CHARS {
                let cut = text.char_indices().nth(PREVIEW_CHARS).map_or(text.len(), |(i, _)| i);
                code(ui, &t, &text[..cut], false);
                ui.label(
                    RichText::new(tr("… The preview stops here; Copy and Save give the whole script."))
                        .color(t.muted_foreground),
                );
            } else if text.is_empty() {
                ui.label(RichText::new(tr("Nothing is chosen.")).color(t.muted_foreground));
            } else {
                code(ui, &t, text, false);
            }
        });
    }

    fn actions(&mut self, ui: &mut Ui, cx: &mut TabCtx<'_>) {
        self.ensure_script();
        let Some((plan, text)) = self.script.clone() else { return };
        let n = plan.statements.len();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let label = if self.show_script { tr("Hide script") } else { tr("Show script") };
            if ui.add(Button::outline(label).size(ButtonSize::Sm)).clicked() {
                self.show_script = !self.show_script;
            }
            ui.label(RichText::new(trf("{n} statements", &[("n", &n)])).color(Theme::get(ui.ctx()).muted_foreground));
            let some = n > 0;
            if ui.add_enabled(some, Button::outline(tr("Copy")).size(ButtonSize::Sm)).clicked() {
                ui.ctx().copy_text(text.clone());
                toast(ui.ctx(), tr("Copied the sync script"), None);
            }
            if ui.add_enabled(some, Button::outline(tr("Save…")).size(ButtonSize::Sm)).clicked() {
                let name = self.compared.as_ref().map_or("sync".into(), |c| format!("sync_{}", c.schema));
                let dialog =
                    rfd::AsyncFileDialog::new().set_file_name(format!("{name}.sql")).add_filter("SQL", &["sql"]);
                let text = text.clone();
                files::save_file(ui.ctx(), dialog, move || text);
            }
            if let Some(c) = self.compared.clone() {
                if ui
                    .add_enabled(some, Button::outline(tr("Open in console")).size(ButtonSize::Sm))
                    .tooltip(tr("A console on the target, to read and run the script yourself"))
                    .clicked()
                {
                    cx.actions.push(Action::OpenConsoleWith(c.conn, Some(c.database.clone()), text.clone()));
                }
                if ui
                    .add_enabled(some, Button::destructive(tr("Apply to target")).size(ButtonSize::Sm))
                    .tooltip(tr("Runs the script on the target in the background"))
                    .clicked()
                {
                    self.confirm = true;
                }
            }
        });
        ui.add_space(4.0);
    }

    fn confirm_dialog(&mut self, ui: &mut Ui, cx: TabCtx<'_>) {
        let (Some(c), Some((plan, _))) = (self.compared.clone(), self.script.clone()) else {
            self.confirm = false;
            return;
        };
        let kind = c.cfg.kind;
        let transaction = if self.mode == 0 { compare::transactional_ddl(kind) } else { kind != DbKind::ClickHouse };
        let where_ = if c.schema == c.database { c.database.clone() } else { format!("{}.{}", c.database, c.schema) };
        let args: [(&str, &dyn std::fmt::Display); 3] =
            [("n", &plan.statements.len()), ("conn", &c.cfg.display_name()), ("where", &where_)];
        let description = if transaction {
            trf(
                "{n} statements will run on {conn} ({where}) in one transaction; an error rolls everything back.",
                &args,
            )
        } else {
            trf(
                "{n} statements will run on {conn} ({where}) one by one; an error stops the run, and what ran before it stays.",
                &args,
            )
        };
        match alert_dialog(ui.ctx(), "compare-apply", tr("Apply the sync script?"), &description, tr("Apply")) {
            Some(true) => {
                self.confirm = false;
                let args: [(&str, &dyn std::fmt::Display); 2] = [("where", &where_), ("conn", &c.cfg.display_name())];
                let title = if self.mode == 0 {
                    trf("Sync structure of {where} on {conn}", &args)
                } else {
                    trf("Sync data of {where} on {conn}", &args)
                };
                cx.actions.push(Action::StartJob(Box::new(Start {
                    title,
                    spec: JobSpec::Script {
                        cfg: Box::new(c.cfg.clone()),
                        database: Some(c.database.clone()),
                        statements: plan.statements,
                        transaction,
                    },
                    output: None,
                    refresh: Some(Action::RefreshObjects(c.conn, c.database, c.schema)),
                })));
            }
            Some(false) => self.confirm = false,
            None => {}
        }
    }
}

/// Tables of the source with the matching target table and how to compare them.
fn data_rows(source: &Snapshot, target: &Snapshot) -> Vec<DataRow> {
    let details = |s: &Snapshot, name: &str| -> Option<TableDetails> {
        s.objects.iter().find(|o| o.object.path.name == name).and_then(|o| o.details.clone())
    };
    source
        .objects
        .iter()
        .filter_map(|o| o.details.as_ref().map(|d| (o, d)))
        .map(|(o, sd)| {
            let name = o.object.path.name.clone();
            let (state, spec, references) = match details(target, &name) {
                None => (DataState::Skipped(tr("not in the target").into()), None, Vec::new()),
                Some(td) => {
                    let path = ObjectPath::new(&target.database, &target.schema, &name);
                    let refs = td.foreign_keys.iter().map(|f| f.ref_table.clone()).collect();
                    match DataSpec::new(path, sd, &td) {
                        Ok(spec) => (DataState::Pending, Some(spec), refs),
                        Err(why) => (DataState::Skipped(why.to_lowercase()), None, refs),
                    }
                }
            };
            DataRow { name, state, include: false, references, spec, source: o.object.path.clone() }
        })
        .collect()
}

fn status_badge(ui: &mut Ui, t: &Theme, status: Status) {
    let color = match status {
        Status::OnlySource => Color32::from_rgb(0x16, 0xa3, 0x4a),
        Status::Changed => Color32::from_rgb(0xd9, 0x77, 0x06),
        Status::OnlyTarget => t.destructive,
        Status::Same => t.muted_foreground,
    };
    // A fixed width keeps the names in one column.
    ui.allocate_ui_with_layout(egui::vec2(92.0, 18.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.set_min_width(92.0);
        ui.label(RichText::new(tr(status.label())).size(11.0).color(color));
    });
}

/// SQL in a muted box; `wrap` breaks long lines (side-by-side definitions).
/// The two DDL texts as a unified diff with all lines as context, for [`DiffView`].
fn ddl_diff(source: &str, target: &str) -> String {
    let (source, target) = (format!("{}\n", source.trim_end()), format!("{}\n", target.trim_end()));
    similar::TextDiff::from_lines(&source, &target).unified_diff().context_radius(100_000).to_string()
}

fn code(ui: &mut Ui, t: &Theme, text: &str, wrap: bool) {
    egui::Frame::new().fill(t.muted).corner_radius(t.radius_md()).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let label = egui::Label::new(RichText::new(text).monospace().size(12.0));
        ui.add(if wrap { label.wrap() } else { label.extend() });
    });
}

#[cfg(test)]
mod tests {
    use super::ddl_diff;

    #[test]
    fn ddl_diff_keeps_every_line() {
        let source = "CREATE TABLE t (\n  id int,\n  name text\n);";
        let target = "CREATE TABLE t (\n  id int\n);";
        let diff = ddl_diff(source, target);
        let body: Vec<&str> = diff.lines().filter(|l| !l.starts_with("@@")).collect();
        assert_eq!(body, [" CREATE TABLE t (", "-  id int,", "-  name text", "+  id int", " );"]);
    }
}
