//! Copy tab: tables of a schema (the source) with their rows, into a schema
//! on any connection (the target), also between different engines. Missing
//! tables are created with mapped types; the copy runs as a background job.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dbman_core::copy::target_def;
use dbman_core::{ConnectionConfig, DbKind, DbResult, ObjectKind, ObjectPath, TableDetails};
use dbman_drivers::copy::{CopyJob, Existing};
use egui::{RichText, Ui};
use shadcn_egui::{Badge, Button, ButtonSize, Checkbox, Empty, Select, Theme, alert_dialog};
use tokio::sync::OnceCell;

use crate::events::{Event, EventSender, TabEvent, TabId};
use crate::explorer::{Action, Load};
use crate::i18n::{tr, trf};
use crate::jobs::JobSpec;
use crate::tabs::{TabCtx, error_alert};
use crate::target_picker::{Session, TargetPicker};
use crate::transfer_dialog::Start;

/// Rows per INSERT.
const BATCH_ROWS: usize = 500;

pub enum CopyEvent {
    Tables(DbResult<Vec<String>>),
    Details(String, DbResult<TableDetails>),
    /// Tables already in the target, for the picker generation they answer.
    Target(u64, DbResult<Vec<String>>),
}

pub struct CopyRow {
    pub name: String,
    pub include: bool,
}

pub struct CopyView {
    pub database: String,
    pub schema: String,
    source_cfg: ConnectionConfig,
    source: Session,
    picker: TargetPicker,
    /// Chosen at first instead of every table.
    only: Option<String>,
    tables: Load<Vec<CopyRow>>,
    details: HashMap<String, Load<TableDetails>>,
    target_tables: Load<HashSet<String>>,
    target_for: Option<u64>,
    existing: Option<usize>,
    selected: Option<String>,
    confirm: bool,
}

impl CopyView {
    pub fn new(cfg: ConnectionConfig, database: String, schema: String, only: Option<String>) -> Self {
        let picker = TargetPicker::new(&database, &schema);
        Self {
            database,
            schema,
            source_cfg: cfg,
            source: Arc::new(OnceCell::new()),
            picker,
            selected: only.clone(),
            only,
            tables: Load::Idle,
            details: HashMap::new(),
            target_tables: Load::Idle,
            target_for: None,
            existing: Some(0),
            confirm: false,
        }
    }

    pub fn title(&self) -> String {
        let what = self.only.as_deref().unwrap_or(if self.schema.is_empty() { &self.database } else { &self.schema });
        trf("{name} (copy)", &[("name", &what)])
    }

    /// Switches the target to a saved connection on the next frame.
    pub fn set_target(&mut self, conn: uuid::Uuid) {
        self.picker.want(conn);
    }

    /// Chooses the target database and schema by name (tests).
    pub fn choose(&mut self, database: &str, schema: &str) {
        self.picker.choose(database, schema);
    }

    pub fn set_existing(&mut self, e: Existing) {
        self.existing = Existing::ALL.iter().position(|x| *x == e);
    }

    /// Shows the column mapping of a table (tests).
    pub fn select(&mut self, name: &str) {
        self.selected = Some(name.into());
    }

    /// The chosen tables.
    pub fn chosen(&self) -> Vec<&str> {
        self.tables.ready().map_or(Vec::new(), |r| r.iter().filter(|r| r.include).map(|r| r.name.as_str()).collect())
    }

    /// Tables the target already has, once listed.
    pub fn in_target(&self) -> Option<&HashSet<String>> {
        self.target_tables.ready()
    }

    fn existing(&self) -> Existing {
        self.existing.and_then(|i| Existing::ALL.get(i).copied()).unwrap_or(Existing::Append)
    }

    fn source_cfg(&self) -> ConnectionConfig {
        let mut cfg = self.source_cfg.clone();
        if cfg.kind == DbKind::MySql {
            cfg.database = self.database.clone();
        }
        cfg
    }

    fn send(tx: &EventSender, tab: TabId, ev: CopyEvent) {
        tx.send(Event::Tab { tab, event: TabEvent::Copy(Box::new(ev)) });
    }

    pub fn on_event(&mut self, ev: TabEvent) {
        let ev = match ev {
            TabEvent::Picker(e) => return self.picker.on_event(e),
            TabEvent::Copy(ev) => ev,
            _ => return,
        };
        match *ev {
            CopyEvent::Tables(r) => {
                let only = self.only.clone();
                self.tables = Load::from_result(r.map(|names| {
                    names
                        .into_iter()
                        .map(|name| CopyRow { include: only.as_ref().is_none_or(|o| *o == name), name })
                        .collect()
                }));
            }
            CopyEvent::Details(name, r) => {
                self.details.insert(name, Load::from_result(r));
            }
            CopyEvent::Target(g, r) if g == self.picker.generation() => {
                self.target_tables = Load::from_result(r.map(|v| v.into_iter().collect()));
            }
            CopyEvent::Target(..) => {}
        }
    }

    fn load_tables(&mut self, cx: &TabCtx<'_>, tab: TabId) {
        self.tables = Load::Loading;
        let (session, cfg, tx) = (self.source.clone(), self.source_cfg(), cx.tasks.sender());
        let (db, schema) = (self.database.clone(), self.schema.clone());
        cx.tasks.runtime().spawn(async move {
            let r = async {
                let conn = session.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?;
                let objects = conn.objects(&db, &schema).await?;
                Ok(objects.into_iter().filter(|o| o.kind == ObjectKind::Table).map(|o| o.path.name).collect())
            }
            .await;
            Self::send(&tx, tab, CopyEvent::Tables(r));
        });
    }

    fn load_details(&mut self, name: String, cx: &TabCtx<'_>, tab: TabId) {
        self.details.insert(name.clone(), Load::Loading);
        let (session, cfg, tx) = (self.source.clone(), self.source_cfg(), cx.tasks.sender());
        let path = ObjectPath::new(&self.database, &self.schema, &name);
        cx.tasks.runtime().spawn(async move {
            let r =
                async { session.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?.table_details(&path).await }
                    .await;
            Self::send(&tx, tab, CopyEvent::Details(name, r));
        });
    }

    fn load_target(&mut self, cx: &TabCtx<'_>, tab: TabId) {
        let generation = self.picker.generation();
        self.target_for = Some(generation);
        let (Some(cfg), Some(db), Some(schema)) =
            (self.picker.cfg(), self.picker.database().map(str::to_string), self.picker.schema().map(str::to_string))
        else {
            return;
        };
        self.target_tables = Load::Loading;
        let (session, tx) = (self.picker.session(), cx.tasks.sender());
        cx.tasks.runtime().spawn(async move {
            let r = async {
                let conn = session.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?;
                let objects = conn.objects(&db, &schema).await?;
                Ok(objects.into_iter().filter(|o| o.kind == ObjectKind::Table).map(|o| o.path.name).collect())
            }
            .await;
            Self::send(&tx, tab, CopyEvent::Target(generation, r));
        });
    }

    /// The target is the source itself.
    fn onto_itself(&self, own: uuid::Uuid) -> bool {
        self.picker.conn() == Some(own)
            && self.picker.database() == Some(self.database.as_str())
            && self.picker.schema().is_some_and(|s| s == self.schema || self.schema.is_empty())
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        self.picker.ensure(cx.tasks, tab, cx.conns, cx.conn_id, self.source_cfg.kind);
        if self.tables.is_idle() {
            self.load_tables(&cx, tab);
        }
        if self.picker.schema().is_some() && self.target_for != Some(self.picker.generation()) {
            self.load_target(&cx, tab);
        }
        if let Some(name) = self.selected.clone()
            && !self.details.contains_key(&name)
        {
            self.load_details(name, &cx, tab);
        }

        self.toolbar(ui, tab, &cx);
        ui.add_space(6.0);
        if let Load::Failed(e) = &self.tables {
            return error_alert(ui, tr("Could not list the tables"), e);
        }
        if self.tables.ready().is_none() {
            ui.add(shadcn_egui::Spinner::new());
            return;
        }
        egui::Panel::bottom(egui::Id::new(("copy-actions", tab)))
            .resizable(false)
            .show(ui, |ui| self.actions(ui, &mut cx));
        egui::Panel::left(egui::Id::new(("copy-list", tab)))
            .resizable(true)
            .default_size(320.0)
            .min_size(220.0)
            .show(ui, |ui| self.list(ui));
        ui.add_space(4.0);
        self.mapping(ui);
        if self.confirm {
            self.confirm_dialog(ui, cx);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, tab: TabId, cx: &TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        ui.horizontal_wrapped(|ui| {
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
            ui.label(RichText::new(tr("Existing tables")).color(t.muted_foreground));
            let labels: Vec<&str> = Existing::ALL.iter().map(|e| tr(e.label())).collect();
            ui.push_id("copy-existing", |ui| ui.add(Select::new(&mut self.existing, &labels).width(170.0)));
        });
    }

    fn list(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let in_target = self.target_tables.ready().cloned();
        let Load::Ready(rows) = &mut self.tables else { return };
        let chosen = rows.iter().filter(|r| r.include).count();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(trf("{chosen} of {total} tables", &[("chosen", &chosen), ("total", &rows.len())]))
                    .size(12.0)
                    .color(t.muted_foreground),
            );
            if ui.add(Button::ghost(tr("All")).size(ButtonSize::Sm)).clicked() {
                rows.iter_mut().for_each(|r| r.include = true);
            }
            if ui.add(Button::ghost(tr("None")).size(ButtonSize::Sm)).clicked() {
                rows.iter_mut().for_each(|r| r.include = false);
            }
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical().id_salt("copy-tables").auto_shrink([false, false]).show(ui, |ui| {
            if rows.is_empty() {
                ui.label(RichText::new(tr("The schema has no tables.")).color(t.muted_foreground));
            }
            for (i, row) in rows.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.push_id(i, |ui| ui.add(Checkbox::new(&mut row.include)));
                    let r = ui.selectable_label(self.selected.as_deref() == Some(row.name.as_str()), &row.name);
                    if r.clicked() {
                        self.selected = Some(row.name.clone());
                    }
                    if in_target.as_ref().is_some_and(|s| s.contains(&row.name)) {
                        ui.add(Badge::new(tr("in target")).outline());
                    }
                });
            }
        });
    }

    /// Source and target types of the chosen table's columns.
    fn mapping(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Some(name) = self.selected.clone() else {
            Empty::new(tr("Nothing selected"))
                .description(tr("Choose a table to see how its columns are created in the target."))
                .show(ui, |_| {});
            return;
        };
        ui.label(RichText::new(&name).strong().size(15.0));
        ui.add_space(6.0);
        let details = match self.details.get(&name) {
            Some(Load::Ready(d)) => d,
            Some(Load::Failed(e)) => return error_alert(ui, tr("Could not read the table"), e),
            _ => {
                ui.add(shadcn_egui::Spinner::new());
                return;
            }
        };
        let Some(to) = self.picker.kind() else { return };
        let exists = self.target_tables.ready().is_some_and(|s| s.contains(&name));
        if exists && self.existing() != Existing::Recreate {
            ui.label(
                RichText::new(tr("The target has this table; the rows go into its columns with the same names."))
                    .color(t.muted_foreground),
            );
            ui.add_space(4.0);
        }
        let chosen: HashSet<&str> = self.chosen().into_iter().collect();
        let target = ObjectPath::new(
            self.picker.database().unwrap_or_default(),
            self.picker.schema().unwrap_or_default(),
            &name,
        );
        let (def, notes) = target_def(self.source_cfg.kind, to, target, details, |r| chosen.contains(r));
        egui::ScrollArea::vertical().id_salt("copy-mapping").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("copy-mapping-grid").striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new(tr("Column")).strong());
                ui.label(RichText::new(trf("{db} type", &[("db", &self.source_cfg.kind.label())])).strong());
                ui.label(RichText::new(trf("{db} type", &[("db", &to.label())])).strong());
                ui.end_row();
                for (src, c) in details.columns.iter().zip(&def.columns) {
                    ui.label(&src.name);
                    ui.label(RichText::new(&src.data_type).monospace());
                    let changed = !src.data_type.eq_ignore_ascii_case(&c.col.data_type);
                    let text = RichText::new(&c.col.data_type).monospace();
                    ui.label(if changed { text.strong() } else { text.color(t.muted_foreground) });
                    ui.end_row();
                }
            });
            for n in notes {
                ui.add_space(4.0);
                ui.label(RichText::new(n).color(t.muted_foreground));
            }
        });
    }

    /// Chosen tables the target has, which Replace or Recreate change.
    fn overwritten(&self) -> Vec<String> {
        let Some(in_target) = self.target_tables.ready() else { return Vec::new() };
        self.chosen().into_iter().filter(|n| in_target.contains(*n)).map(str::to_string).collect()
    }

    fn actions(&mut self, ui: &mut Ui, cx: &mut TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        let n = self.chosen().len();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let itself = self.onto_itself(cx.conn_id);
            let ready = n > 0 && self.picker.schema().is_some() && !itself && self.target_tables.ready().is_some();
            let label = if n == 1 { tr("Copy 1 table").to_string() } else { trf("Copy {n} tables", &[("n", &n)]) };
            if ui.add_enabled(ready, Button::new(&label).size(ButtonSize::Sm)).clicked() {
                if self.existing() != Existing::Append && !self.overwritten().is_empty() {
                    self.confirm = true;
                } else {
                    self.start(cx);
                }
            }
            if itself {
                ui.label(RichText::new(tr("The target is the source itself.")).color(t.destructive));
            } else if let Load::Failed(e) = &self.target_tables {
                ui.label(RichText::new(tr("Could not list the target's tables")).color(t.destructive))
                    .on_hover_text(e.as_str());
            } else {
                ui.label(
                    RichText::new(tr("Missing tables are created; rows are copied in the background."))
                        .color(t.muted_foreground),
                );
            }
        });
        ui.add_space(4.0);
    }

    fn start(&mut self, cx: &mut TabCtx<'_>) {
        let (Some(conn), Some(cfg), Some(db), Some(schema)) = (
            self.picker.conn(),
            self.picker.cfg(),
            self.picker.database().map(str::to_string),
            self.picker.schema().map(str::to_string),
        ) else {
            return;
        };
        let tables: Vec<String> = self.chosen().into_iter().map(str::to_string).collect();
        let where_ = if schema == db { db.clone() } else { format!("{db}.{schema}") };
        let what = if tables.len() == 1 { tables[0].clone() } else { trf("{n} tables", &[("n", &tables.len())]) };
        let job = CopyJob {
            source_db: self.database.clone(),
            source_schema: self.schema.clone(),
            tables,
            target_db: db.clone(),
            target_schema: schema.clone(),
            existing: self.existing(),
            batch_rows: BATCH_ROWS,
        };
        cx.actions.push(Action::StartJob(Box::new(Start {
            title: trf(
                "Copy {what} to {where} on {conn}",
                &[("what", &what), ("where", &where_), ("conn", &cfg.display_name())],
            ),
            spec: JobSpec::Copy { source: Box::new(self.source_cfg()), target: Box::new(cfg), job },
            output: None,
            refresh: Some(Action::RefreshObjects(conn, db, schema)),
        })));
        // The target's tables change; list them again.
        self.target_for = None;
    }

    fn confirm_dialog(&mut self, ui: &mut Ui, mut cx: TabCtx<'_>) {
        let names = self.overwritten();
        let shown: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
        let more = if names.len() > shown.len() {
            trf(" and {n} more", &[("n", &(names.len() - shown.len()))])
        } else {
            String::new()
        };
        let tables = format!("{}{more}", shown.join(", "));
        let target = self.picker.display_name();
        let args: [(&str, &dyn std::fmt::Display); 2] = [("tables", &tables), ("target", &target)];
        let description = match self.existing() {
            Existing::Recreate => trf(
                "{tables} already in {target} will be dropped and created again; their current rows are lost.",
                &args,
            ),
            _ => trf("{tables} already in {target} will be emptied first; their current rows are lost.", &args),
        };
        match alert_dialog(ui.ctx(), "copy-overwrite", tr("Overwrite tables in the target?"), &description, tr("Copy"))
        {
            Some(true) => {
                self.confirm = false;
                self.start(&mut cx);
            }
            Some(false) => self.confirm = false,
            None => {}
        }
    }
}
