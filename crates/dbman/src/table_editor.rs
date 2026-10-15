//! Table editor: create a table or change an existing one through a form,
//! with the generated DDL shown live before it runs.

use dbman_core::ddl::{self, ColumnDef, Plan, TableDef};
use dbman_core::{
    ColumnInfo, ConnectionConfig, DbKind, DbObject, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectPath, TableDetails,
};
use egui::{RichText, Ui};
use shadcn_egui::{
    Alert, Button, ButtonSize, Checkbox, Input, Select, Theme, TooltipExt, dropdown_menu, label, menu_item, toast,
};

use crate::events::{Event, TabEvent, TabId, Tasks};
use crate::exec;
use crate::explorer::{Action, Load};
use crate::sql_editor;
use crate::tabs::{TabCtx, error_alert, not_connected};

const ACTIONS: [&str; 5] = ["NO ACTION", "RESTRICT", "CASCADE", "SET NULL", "SET DEFAULT"];

/// Editable index row; columns are typed as a comma-separated list.
#[derive(Clone, Default)]
struct IndexRow {
    name: String,
    columns: String,
    unique: bool,
}

#[derive(Clone)]
struct FkRow {
    name: String,
    columns: String,
    /// `table` or `schema.table`.
    target: String,
    ref_columns: String,
    on_delete: Option<usize>,
    on_update: Option<usize>,
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
}

fn action_index(a: &str) -> Option<usize> {
    ACTIONS.iter().position(|x| x.eq_ignore_ascii_case(a)).or(Some(0))
}

pub struct TableEditor {
    pub conn: uuid::Uuid,
    kind: DbKind,
    /// Where the table lives; the name is edited in `name`.
    path: ObjectPath,
    /// `None` when creating a new table.
    existing: Option<Load<TableDef>>,
    name: String,
    comment: String,
    columns: Vec<ColumnDef>,
    indexes: Vec<IndexRow>,
    fks: Vec<FkRow>,
    running: bool,
    error: Option<String>,
}

impl TableEditor {
    pub fn create(conn: uuid::Uuid, kind: DbKind, database: &str, schema: &str) -> Self {
        let int = if kind == DbKind::Sqlite { "INTEGER" } else { "integer" };
        let id = ColumnInfo {
            name: "id".into(),
            data_type: int.into(),
            nullable: false,
            primary_key: true,
            // DuckDB has no auto-increment columns.
            auto_increment: kind != DbKind::DuckDb,
            ..Default::default()
        };
        Self {
            conn,
            kind,
            path: ObjectPath::new(database, schema, ""),
            existing: None,
            name: "new_table".into(),
            comment: String::new(),
            columns: vec![ColumnDef { col: id, original: None }],
            indexes: Vec::new(),
            fks: Vec::new(),
            running: false,
            error: None,
        }
    }

    pub fn edit(conn: uuid::Uuid, kind: DbKind, path: ObjectPath) -> Self {
        let mut e = Self::create(conn, kind, &path.database, &path.schema);
        e.name = path.name.clone();
        e.path = path;
        e.columns.clear();
        e.existing = Some(Load::Idle);
        e
    }

    pub fn title(&self) -> String {
        match self.existing {
            None => format!("new table in {}", self.path.schema),
            Some(_) => format!("edit {}", self.path.name),
        }
    }

    fn load_from(&mut self, def: &TableDef) {
        self.columns = def.columns.clone();
        self.comment = def.comment.clone().unwrap_or_default();
        self.indexes = def
            .indexes
            .iter()
            .map(|i| IndexRow { name: i.name.clone(), columns: i.columns.join(", "), unique: i.unique })
            .collect();
        self.fks = def
            .foreign_keys
            .iter()
            .map(|f| FkRow {
                name: f.name.clone(),
                columns: f.columns.join(", "),
                target: if f.ref_schema.is_empty() || f.ref_schema == self.path.schema {
                    f.ref_table.clone()
                } else {
                    format!("{}.{}", f.ref_schema, f.ref_table)
                },
                ref_columns: f.ref_columns.join(", "),
                on_delete: action_index(&f.on_delete),
                on_update: action_index(&f.on_update),
            })
            .collect();
    }

    pub fn on_event(&mut self, ev: TabEvent, ctx: &egui::Context) -> Vec<Action> {
        match ev {
            TabEvent::Details(r) => {
                match r {
                    Ok(d) => {
                        let def = TableDef::from_details(self.path.clone(), &d);
                        self.load_from(&def);
                        self.existing = Some(Load::Ready(def));
                    }
                    Err(e) => self.existing = Some(Load::Failed(e.message)),
                }
                Vec::new()
            }
            TabEvent::Executed(r) => {
                self.running = false;
                match r {
                    Ok(_) => {
                        self.error = None;
                        let created = self.existing.is_none();
                        toast(ctx, if created { "Table created" } else { "Table altered" }, Some(&self.name));
                        // Continue editing the table as it now is on the server.
                        self.path.name = self.name.clone();
                        self.existing = Some(Load::Idle);
                        vec![Action::RefreshObjects(self.conn, self.path.database.clone(), self.path.schema.clone())]
                    }
                    Err(e) => {
                        self.error = Some(e.message);
                        Vec::new()
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    /// The table as the form describes it.
    fn definition(&self) -> TableDef {
        let mut path = self.path.clone();
        if let Some(Load::Ready(old)) = &self.existing {
            path.name = old.path.name.clone();
        } else {
            path.name = self.name.trim().to_string();
        }
        TableDef {
            path,
            columns: self.columns.clone(),
            indexes: self
                .indexes
                .iter()
                .map(|i| IndexInfo {
                    name: i.name.trim().into(),
                    columns: split_list(&i.columns),
                    unique: i.unique,
                    ..Default::default()
                })
                .collect(),
            foreign_keys: self
                .fks
                .iter()
                .map(|f| {
                    let (schema, table) = match f.target.trim().split_once('.') {
                        Some((s, t)) => (s.to_string(), t.to_string()),
                        None => (self.path.schema.clone(), f.target.trim().to_string()),
                    };
                    ForeignKeyInfo {
                        name: f.name.trim().into(),
                        columns: split_list(&f.columns),
                        ref_schema: schema,
                        ref_table: table,
                        ref_columns: split_list(&f.ref_columns),
                        on_delete: ACTIONS[f.on_delete.unwrap_or(0)].into(),
                        on_update: ACTIONS[f.on_update.unwrap_or(0)].into(),
                    }
                })
                .collect(),
            comment: Some(self.comment.trim().to_string()).filter(|c| !c.is_empty()),
            pk_name: match &self.existing {
                Some(Load::Ready(old)) => old.pk_name.clone(),
                _ => None,
            },
        }
    }

    fn plan(&self) -> Result<Plan, String> {
        let def = self.definition();
        if self.name.trim().is_empty() {
            return Err("The table needs a name.".into());
        }
        if def.columns.is_empty() {
            return Err("Add at least one column.".into());
        }
        if let Some(c) = def.columns.iter().find(|c| c.col.name.trim().is_empty() || c.col.data_type.trim().is_empty())
        {
            let which = if c.col.name.trim().is_empty() { "a name" } else { "a type" };
            return Err(format!("Every column needs {which}."));
        }
        match &self.existing {
            None => Ok(ddl::create_table(self.kind, &def)),
            Some(Load::Ready(old)) => {
                let mut plan = ddl::alter_table(self.kind, old, &def);
                if self.name.trim() != old.path.name {
                    let obj = DbObject { kind: ObjectKind::Table, path: old.path.clone(), detail: None, comment: None };
                    match ddl::rename(self.kind, &obj, self.name.trim()) {
                        Some(sql) => plan.statements.push(sql),
                        None => return Err("This database cannot rename tables.".into()),
                    }
                }
                Ok(plan)
            }
            Some(_) => Err("Loading the table…".into()),
        }
    }

    fn execute(&mut self, tasks: &Tasks, tab: TabId, cfg: ConnectionConfig, plan: Plan) {
        self.running = true;
        self.error = None;
        let db = self.path.database.clone();
        tasks.spawn(async move {
            let result = exec::run(cfg, db, plan.statements, exec::Mode::Transaction).await;
            Event::Tab { tab, event: TabEvent::Executed(result) }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, tab: TabId, mut cx: TabCtx<'_>) {
        let t = Theme::get(ui.ctx());
        let Some(session) = cx.conn.and_then(|c| c.session()) else {
            not_connected(ui, &mut cx);
            return;
        };
        if let Some(Load::Idle) = self.existing {
            self.existing = Some(Load::Loading);
            let path = self.path.clone();
            cx.tasks
                .spawn(async move { Event::Tab { tab, event: TabEvent::Details(session.table_details(&path).await) } });
        }
        match &self.existing {
            Some(Load::Loading | Load::Idle) => {
                ui.add(egui::Spinner::new());
                return;
            }
            Some(Load::Failed(e)) => {
                let e = e.clone();
                error_alert(ui, "Could not read the table", &e);
                return;
            }
            _ => {}
        }

        egui::ScrollArea::vertical().id_salt(("table-editor", tab)).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    label(ui, "Table name");
                    ui.add(Input::new(&mut self.name).width(260.0));
                });
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    label(ui, "Comment");
                    ui.add(Input::new(&mut self.comment).width(420.0));
                });
            });
            ui.add_space(12.0);
            self.columns_ui(ui, &t);
            ui.add_space(12.0);
            self.indexes_ui(ui, &t);
            ui.add_space(12.0);
            self.fks_ui(ui, &t);
            ui.add_space(16.0);

            let plan = self.plan();
            ui.horizontal(|ui| {
                label(ui, "SQL");
                ui.add_space(8.0);
                let ready = matches!(&plan, Ok(p) if !p.statements.is_empty());
                let verb = if self.existing.is_none() { "Create table" } else { "Apply changes" };
                if ui.add_enabled(ready && !self.running, Button::new(verb).size(ButtonSize::Sm)).clicked()
                    && let (Ok(p), Some(c)) = (&plan, cx.conn)
                {
                    self.execute(cx.tasks, tab, c.cfg.clone(), p.clone());
                }
                if self.existing.is_some()
                    && ui
                        .add_enabled(!self.running, Button::ghost("Reload").size(ButtonSize::Sm))
                        .tooltip("Discard edits")
                        .clicked()
                {
                    self.existing = Some(Load::Idle);
                }
                if self.running {
                    ui.add(egui::Spinner::new().size(14.0));
                }
            });
            ui.add_space(6.0);
            if let Some(e) = &self.error {
                Alert::new("The statements failed").icon("!").description(e.as_str()).destructive().show(ui);
                ui.add_space(6.0);
            }
            match plan {
                Ok(p) if p.statements.is_empty() => {
                    ui.label(RichText::new("No changes.").color(t.muted_foreground));
                }
                Ok(p) => sql_editor::viewer(ui, egui::Id::new(("table-editor-sql", tab)), &p.script(), self.kind),
                Err(e) => {
                    ui.label(RichText::new(e).color(t.muted_foreground));
                }
            }
        });
    }

    fn section_header(ui: &mut Ui, title: &str, add: &str) -> bool {
        let mut clicked = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong().size(15.0));
            ui.add_space(8.0);
            clicked = ui.add(Button::outline(add).size(ButtonSize::Sm)).clicked();
        });
        ui.add_space(4.0);
        clicked
    }

    fn columns_ui(&mut self, ui: &mut Ui, t: &Theme) {
        if Self::section_header(ui, "Columns", "+ Column") {
            let n = self.columns.len() + 1;
            let ty = ddl::common_types(self.kind)[match self.kind {
                DbKind::Sqlite => 2,
                DbKind::DuckDb => 8,
                _ => 7,
            }];
            self.columns.push(ColumnDef {
                col: ColumnInfo {
                    name: format!("column_{n}"),
                    data_type: ty.into(),
                    nullable: true,
                    ..Default::default()
                },
                original: None,
            });
        }
        let kind = self.kind;
        let mut remove = None;
        let mut swap = None;
        egui::Grid::new("columns-grid").num_columns(9).spacing([8.0, 6.0]).show(ui, |ui| {
            for h in ["Name", "Type", "", "Not null", "Default", "PK", "Auto", "Comment", ""] {
                ui.label(RichText::new(h).size(12.0).color(t.muted_foreground));
            }
            ui.end_row();
            let count = self.columns.len();
            for (i, c) in self.columns.iter_mut().enumerate() {
                ui.add(Input::new(&mut c.col.name).width(170.0));
                ui.add(Input::new(&mut c.col.data_type).width(170.0));
                let pick = ui.add(Button::ghost("⏷").size(ButtonSize::Icon)).tooltip("Common types");
                dropdown_menu(&pick, 200.0, |ui| {
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for ty in ddl::common_types(kind) {
                            if menu_item(ui, ty, None).clicked() {
                                c.col.data_type = ty.to_string();
                            }
                        }
                    });
                });
                let mut not_null = !c.col.nullable;
                ui.add(Checkbox::new(&mut not_null));
                c.col.nullable = !not_null;
                let mut default = c.col.default.clone().unwrap_or_default();
                ui.add(Input::new(&mut default).placeholder("expression").width(150.0))
                    .on_hover_text("An SQL expression: 'text' needs quotes, numbers and functions like now() do not.");
                c.col.default = Some(default).filter(|d| !d.trim().is_empty());
                ui.add(Checkbox::new(&mut c.col.primary_key));
                ui.add(Checkbox::new(&mut c.col.auto_increment));
                let mut comment = c.col.comment.clone().unwrap_or_default();
                ui.add(Input::new(&mut comment).width(200.0));
                c.col.comment = Some(comment).filter(|d| !d.trim().is_empty());
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, Button::ghost("⏶").size(ButtonSize::Icon)).tooltip("Move up").clicked() {
                        swap = Some((i - 1, i));
                    }
                    if ui
                        .add_enabled(i + 1 < count, Button::ghost("⏷").size(ButtonSize::Icon))
                        .tooltip("Move down")
                        .clicked()
                    {
                        swap = Some((i, i + 1));
                    }
                    if ui.add(Button::ghost("×").size(ButtonSize::Icon)).tooltip("Remove column").clicked() {
                        remove = Some(i);
                    }
                });
                ui.end_row();
            }
        });
        if let Some((a, b)) = swap {
            self.columns.swap(a, b);
        }
        if let Some(i) = remove {
            self.columns.remove(i);
        }
        if self.existing.is_some() && self.kind != DbKind::Sqlite {
            ui.label(
                RichText::new("Column order only applies to new columns; existing ones keep their position.")
                    .size(12.0)
                    .color(t.muted_foreground),
            );
        }
    }

    fn indexes_ui(&mut self, ui: &mut Ui, t: &Theme) {
        if Self::section_header(ui, "Indexes", "+ Index") {
            let n = self.indexes.len() + 1;
            self.indexes.push(IndexRow { name: format!("{}_idx_{n}", self.name.trim()), ..Default::default() });
        }
        if self.indexes.is_empty() {
            return;
        }
        let mut remove = None;
        egui::Grid::new("indexes-grid").num_columns(4).spacing([8.0, 6.0]).show(ui, |ui| {
            for h in ["Name", "Columns", "Unique", ""] {
                ui.label(RichText::new(h).size(12.0).color(t.muted_foreground));
            }
            ui.end_row();
            for (i, ix) in self.indexes.iter_mut().enumerate() {
                ui.add(Input::new(&mut ix.name).width(220.0));
                ui.add(Input::new(&mut ix.columns).placeholder("col_a, col_b").width(300.0));
                ui.add(Checkbox::new(&mut ix.unique));
                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).tooltip("Remove index").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.indexes.remove(i);
        }
    }

    fn fks_ui(&mut self, ui: &mut Ui, t: &Theme) {
        if Self::section_header(ui, "Foreign keys", "+ Foreign key") {
            let n = self.fks.len() + 1;
            self.fks.push(FkRow {
                name: format!("{}_fk_{n}", self.name.trim()),
                columns: String::new(),
                target: String::new(),
                ref_columns: "id".into(),
                on_delete: Some(0),
                on_update: Some(0),
            });
        }
        if self.fks.is_empty() {
            return;
        }
        let mut remove = None;
        egui::Grid::new("fks-grid").num_columns(7).spacing([8.0, 6.0]).show(ui, |ui| {
            for h in ["Name", "Columns", "References table", "Columns", "On delete", "On update", ""] {
                ui.label(RichText::new(h).size(12.0).color(t.muted_foreground));
            }
            ui.end_row();
            for (i, f) in self.fks.iter_mut().enumerate() {
                ui.add(Input::new(&mut f.name).width(190.0));
                ui.add(Input::new(&mut f.columns).placeholder("customer_id").width(150.0));
                ui.add(Input::new(&mut f.target).placeholder("customers").width(170.0));
                ui.add(Input::new(&mut f.ref_columns).placeholder("id").width(120.0));
                ui.add(Select::new(&mut f.on_delete, &ACTIONS).width(130.0));
                ui.add(Select::new(&mut f.on_update, &ACTIONS).width(130.0));
                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).tooltip("Remove foreign key").clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
        if let Some(i) = remove {
            self.fks.remove(i);
        }
    }

    /// Fills the form from loaded details (used by tests and when reopening).
    pub fn set_details(&mut self, d: &TableDetails) {
        let def = TableDef::from_details(self.path.clone(), d);
        self.load_from(&def);
        self.existing = Some(Load::Ready(def));
    }
}
