//! Database explorer: the tree of connections, databases, schemas and objects.
//!
//! Children are loaded lazily: when a folder opens and its contents are not
//! loaded yet, the tree asks for them with an [`Action`].

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use dbman_core::{Connection, ConnectionConfig, DbKind, DbObject, ObjectKind, ObjectPath, TableDetails};
use egui::{Color32, RichText, Ui};
use shadcn_egui::{Theme, Tree, TreeUi, menu_item, menu_separator};
use uuid::Uuid;

use crate::backup_dialog::Scope;
use crate::i18n::{tr, trf};
use crate::icons::{self, Icon};

/// A value that is loaded in the background.
#[derive(Default)]
pub enum Load<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Load::Idle)
    }

    pub fn from_result(r: Result<T, impl std::fmt::Display>) -> Self {
        match r {
            Ok(v) => Load::Ready(v),
            Err(e) => Load::Failed(e.to_string()),
        }
    }
}

/// A saved connection and everything loaded for it.
pub struct ConnState {
    pub cfg: ConnectionConfig,
    pub session: Load<Arc<dyn Connection>>,
    pub version: String,
    pub databases: Load<Vec<String>>,
    pub schemas: HashMap<String, Load<Vec<String>>>,
    pub objects: HashMap<(String, String), Load<Vec<DbObject>>>,
    pub details: HashMap<ObjectPath, Load<TableDetails>>,
}

impl ConnState {
    pub fn new(cfg: ConnectionConfig) -> Self {
        Self {
            cfg,
            session: Load::Idle,
            version: String::new(),
            databases: Load::Idle,
            schemas: HashMap::new(),
            objects: HashMap::new(),
            details: HashMap::new(),
        }
    }

    pub fn session(&self) -> Option<Arc<dyn Connection>> {
        self.session.ready().cloned()
    }

    /// Forgets everything loaded, keeping the session.
    pub fn clear_cache(&mut self) {
        self.databases = Load::Idle;
        self.schemas.clear();
        self.objects.clear();
        self.details.clear();
    }

    pub fn disconnect(&mut self) {
        self.session = Load::Idle;
        self.version.clear();
        self.clear_cache();
    }

    pub fn has_schemas(&self) -> bool {
        dbman_core::Capabilities::for_kind(self.cfg.kind).schemas
    }

    /// Where unqualified names live in `db`: `public`, `dbo`, `main`, or the database itself.
    pub fn default_schema(&self, db: &str) -> String {
        if !self.has_schemas() {
            return db.to_string();
        }
        let preferred = match self.cfg.kind {
            DbKind::Postgres => "public",
            DbKind::MsSql => "dbo",
            _ => "main",
        };
        match self.schemas.get(db).and_then(Load::ready) {
            Some(list) if !list.iter().any(|s| s == preferred) => list.first().cloned().unwrap_or_default(),
            _ => preferred.to_string(),
        }
    }

    /// What autocomplete knows of `db`: the tables and views loaded so far, with their columns where loaded.
    pub fn catalog(&self, db: &str) -> dbman_core::complete::Catalog {
        use dbman_core::complete::{Catalog, CatalogTable};
        let mut tables = Vec::new();
        for ((d, schema), objects) in &self.objects {
            let Some(objects) = objects.ready().filter(|_| d == db) else { continue };
            for o in objects {
                let view = match o.kind {
                    ObjectKind::Table | ObjectKind::ForeignTable => false,
                    ObjectKind::View | ObjectKind::MaterializedView => true,
                    _ => continue,
                };
                let columns = self
                    .details
                    .get(&o.path)
                    .and_then(Load::ready)
                    .map(|t| t.columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect());
                tables.push(CatalogTable { schema: schema.clone(), name: o.path.name.clone(), view, columns });
            }
        }
        tables.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
        let schemas = if self.has_schemas() {
            self.schemas.get(db).and_then(Load::ready).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        Catalog { kind: Some(self.cfg.kind), schemas, default_schema: self.default_schema(db), tables }
    }

    /// Actions that load what autocomplete in `db` still lacks: the schemas,
    /// the default schema's objects, and anything `wants` names.
    pub fn catalog_loads(&self, db: &str, wants: &[(String, String)], want_schemas: &[String]) -> Vec<Action> {
        let id = self.cfg.id;
        let mut out = Vec::new();
        if self.session().is_none() {
            return out;
        }
        if self.has_schemas() && self.schemas.get(db).is_none_or(Load::is_idle) {
            out.push(Action::LoadSchemas(id, db.to_string()));
            return out;
        }
        let default = self.default_schema(db);
        for schema in std::iter::once(&default).chain(want_schemas) {
            if self.objects.get(&(db.to_string(), schema.clone())).is_none_or(Load::is_idle) {
                out.push(Action::LoadObjects(id, db.to_string(), schema.clone()));
            }
        }
        for (schema, name) in wants {
            let loaded = self.objects.get(&(db.to_string(), schema.clone())).and_then(Load::ready);
            let Some(path) = loaded.and_then(|l| l.iter().find(|o| o.path.name == *name)).map(|o| o.path.clone())
            else {
                continue;
            };
            if self.details.get(&path).is_none_or(Load::is_idle) {
                out.push(Action::LoadDetails(id, path));
            }
        }
        out
    }
}

/// A row of the explorer tree.
#[derive(Clone, PartialEq, Debug)]
pub enum Node {
    Conn(Uuid),
    Database(Uuid, String),
    Schema(Uuid, String, String),
    Group(Uuid, String, String, ObjectKind),
    Object(Uuid, DbObject),
    Column(Uuid, ObjectPath, String),
    /// Loading/error rows and sub-folders that do nothing when clicked.
    Info(String),
}

/// What the explorer asks the app to do.
#[derive(Debug, Clone)]
pub enum Action {
    Connect(Uuid),
    LoadDatabases(Uuid),
    LoadSchemas(Uuid, String),
    LoadObjects(Uuid, String, String),
    LoadDetails(Uuid, ObjectPath),
    OpenData(Uuid, DbObject),
    OpenObject(Uuid, DbObject),
    OpenConsole(Uuid, Option<String>),
    NewConnection,
    EditConnection(Uuid),
    DuplicateConnection(Uuid),
    DeleteConnection(Uuid),
    Disconnect(Uuid),
    Refresh(Uuid),
    RefreshObjects(Uuid, String, String),
    RefreshSchemas(Uuid, String),
    CopyText(String),
    NewTable(Uuid, String, String),
    EditTable(Uuid, ObjectPath),
    /// Opens a console with a CREATE template for this kind of object.
    NewFromTemplate(Uuid, String, String, ObjectKind),
    NewDatabase(Uuid),
    NewSchema(Uuid, String),
    DropDatabase(Uuid, String),
    DropSchema(Uuid, String, String),
    DropObject(Uuid, DbObject),
    RenameObject(Uuid, DbObject),
    Truncate(Uuid, DbObject),
    Backup(Uuid, Scope),
    Restore(Uuid, Scope),
    ExportData(Uuid, dbman_drivers::transfer::ExportSource),
    ImportData(Uuid, ObjectPath),
    /// Import a file into a new table in a database and schema.
    ImportNewTable(Uuid, String, String),
    /// Opens the test data generator for a table.
    GenerateData(Uuid, ObjectPath),
    /// Opens the ER diagram of a schema (database, schema).
    OpenDiagram(Uuid, String, String),
    /// Opens a comparison of a schema with another one (database, schema).
    OpenCompare(Uuid, String, String),
    /// Opens a copy of a schema's tables (or one table) to another connection.
    OpenCopy(Uuid, String, String, Option<String>),
    /// Opens a console in a database with a script in it.
    OpenConsoleWith(Uuid, Option<String>, String),
    StartJob(Box<crate::transfer_dialog::Start>),
    /// Opens a script file in a new console in a database.
    OpenScript(Uuid, Option<String>, std::path::PathBuf),
    /// A script file was opened or saved: it goes first in the recent list.
    RecentScript(std::path::PathBuf),
    /// A console ran a statement.
    RecordQuery(Box<crate::store::HistoryEntry>),
}

pub fn kind_color(kind: DbKind) -> Color32 {
    match kind {
        DbKind::Postgres => Color32::from_rgb(0x33, 0x67, 0x91),
        DbKind::MySql => Color32::from_rgb(0xe4, 0x8e, 0x00),
        DbKind::Sqlite => Color32::from_rgb(0x0f, 0x80, 0xcc),
        DbKind::DuckDb => Color32::from_rgb(0xb8, 0x9a, 0x00),
        DbKind::MsSql => Color32::from_rgb(0xa9, 0x1d, 0x22),
        DbKind::ClickHouse => Color32::from_rgb(0xc8, 0x9b, 0x00),
    }
}

/// A small colored tag with the engine's initials.
pub fn kind_badge(ui: &mut Ui, kind: DbKind) {
    let text = match kind {
        DbKind::Postgres => "PG",
        DbKind::MySql => "MY",
        DbKind::Sqlite => "SL",
        DbKind::DuckDb => "DK",
        DbKind::MsSql => "MS",
        DbKind::ClickHouse => "CH",
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 16.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, kind_color(kind));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(9.5),
        Color32::WHITE,
    );
}

pub fn object_icon(kind: ObjectKind) -> Icon {
    match kind {
        ObjectKind::Table | ObjectKind::ForeignTable => Icon::Table,
        ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
        ObjectKind::Sequence => Icon::Sequence,
        ObjectKind::Function | ObjectKind::Procedure => Icon::Function,
        ObjectKind::Trigger => Icon::Trigger,
        ObjectKind::Type => Icon::Type,
    }
}

/// Row content: a colored icon, the name and muted extra text.
fn row(ui: &mut Ui, icon: Option<Icon>, color: Color32, name: &str, extra: Option<&str>) {
    let t = Theme::get(ui.ctx());
    ui.spacing_mut().item_spacing.x = 6.0;
    if let Some(i) = icon {
        icons::icon(ui, i, color);
    }
    ui.add(egui::Label::new(RichText::new(name).color(t.foreground)).truncate().selectable(false));
    if let Some(e) = extra.filter(|e| !e.is_empty()) {
        ui.add(egui::Label::new(RichText::new(e).size(12.0).color(t.muted_foreground)).truncate().selectable(false));
    }
}

fn status_rows<T>(tree: &mut TreeUi<'_, Node>, load: &Load<T>, key: &str) {
    match load {
        Load::Idle | Load::Loading => {
            tree.leaf_ui(Node::Info(format!("{key}:loading")), "loading", |ui| {
                ui.add(shadcn_egui::Spinner::new().size(12.0));
                ui.label(RichText::new(tr("loading…")).color(Theme::get(ui.ctx()).muted_foreground));
            });
        }
        Load::Failed(e) => {
            let first = e.lines().next().unwrap_or_default().to_string();
            tree.leaf_ui(Node::Info(format!("{key}:error")), "error", |ui| {
                ui.label(RichText::new(first).color(Theme::get(ui.ctx()).destructive));
            })
            .on_hover_text(e.as_str());
        }
        Load::Ready(_) => {}
    }
}

/// Draws the explorer and collects what the user asked for into `actions`.
pub fn show(ui: &mut Ui, conns: &[ConnState], selected: &mut Option<Node>, actions: &mut Vec<Action>) {
    let t = Theme::get(ui.ctx());
    if conns.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(tr("No connections yet")).color(t.muted_foreground));
            ui.add_space(8.0);
            if ui.add(shadcn_egui::Button::outline(tr("+ New connection"))).clicked() {
                actions.push(Action::NewConnection);
            }
        });
        return;
    }
    Tree::new("explorer").icons(false).show(ui, selected, |tree| {
        for c in conns {
            connection(tree, c, actions);
        }
    });
}

fn connection(tree: &mut TreeUi<'_, Node>, c: &ConnState, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    let name = c.cfg.display_name();
    let kind = c.cfg.kind;
    let connected = c.session.ready().is_some();
    let r = tree.folder_ui(
        Node::Conn(id),
        &id.to_string(),
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            kind_badge(ui, kind);
            let t = Theme::get(ui.ctx());
            ui.add(egui::Label::new(RichText::new(&name).color(t.foreground)).truncate().selectable(false));
            if connected {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.0, Color32::from_rgb(0x22, 0xc5, 0x5e));
            }
        },
        |tree| match &c.session {
            Load::Idle => {
                actions.push(Action::Connect(id));
                status_rows::<()>(tree, &Load::Loading, "conn");
            }
            Load::Loading => status_rows::<()>(tree, &Load::Loading, "conn"),
            Load::Failed(e) => status_rows::<()>(tree, &Load::Failed(e.clone()), "conn"),
            Load::Ready(_) => databases(tree, c, actions),
        },
    );
    if r.double_clicked() {
        actions.push(Action::OpenConsole(id, None));
    }
    r.context_menu(|ui| {
        ui.set_min_width(200.0);
        if menu_item(ui, tr("New console"), Some("Ctrl+T")).clicked() {
            actions.push(Action::OpenConsole(id, None));
        }
        if connected {
            if !kind.is_file_based() && menu_item(ui, tr("New database…"), None).clicked() {
                actions.push(Action::NewDatabase(id));
            }
            if menu_item(ui, tr("Refresh"), None).clicked() {
                actions.push(Action::Refresh(id));
            }
            if kind.is_file_based() {
                let database = if kind == DbKind::Sqlite { "main".into() } else { String::new() };
                backup_items(ui, id, Scope { database, ..Default::default() }, actions);
            }
            if menu_item(ui, tr("Disconnect"), None).clicked() {
                actions.push(Action::Disconnect(id));
            }
        } else if menu_item(ui, tr("Connect"), None).clicked() {
            actions.push(Action::Connect(id));
        }
        menu_separator(ui);
        if menu_item(ui, tr("Properties…"), None).clicked() {
            actions.push(Action::EditConnection(id));
        }
        if menu_item(ui, tr("Duplicate"), None).clicked() {
            actions.push(Action::DuplicateConnection(id));
        }
        if menu_item(ui, tr("Delete"), None).clicked() {
            actions.push(Action::DeleteConnection(id));
        }
    });
}

fn databases(tree: &mut TreeUi<'_, Node>, c: &ConnState, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    let dbs = match &c.databases {
        Load::Ready(d) => d,
        other => {
            if other.is_idle() {
                actions.push(Action::LoadDatabases(id));
            }
            status_rows(tree, other, "dbs");
            return;
        }
    };
    let t = Theme::get(tree.ui().ctx());
    for db in dbs {
        let r = tree.folder_ui(
            Node::Database(id, db.clone()),
            db,
            |ui| row(ui, Some(Icon::Database), t.muted_foreground, db, None),
            |tree| {
                if c.has_schemas() {
                    schemas(tree, c, db, actions);
                } else {
                    objects(tree, c, db, db, actions);
                }
            },
        );
        r.context_menu(|ui| {
            ui.set_min_width(200.0);
            if menu_item(ui, tr("New console"), None).clicked() {
                actions.push(Action::OpenConsole(id, Some(db.clone())));
            }
            if c.has_schemas() {
                if menu_item(ui, tr("New schema…"), None).clicked() {
                    actions.push(Action::NewSchema(id, db.clone()));
                }
                if menu_item(ui, tr("Refresh"), None).clicked() {
                    actions.push(Action::RefreshSchemas(id, db.clone()));
                }
            } else {
                new_object_items(ui, c, db, db, actions);
                if menu_item(ui, tr("ER diagram"), None).clicked() {
                    actions.push(Action::OpenDiagram(id, db.clone(), db.clone()));
                }
                if menu_item(ui, tr("Compare with…"), None).clicked() {
                    actions.push(Action::OpenCompare(id, db.clone(), db.clone()));
                }
                if menu_item(ui, tr("Copy to another connection…"), None).clicked() {
                    actions.push(Action::OpenCopy(id, db.clone(), db.clone(), None));
                }
                if menu_item(ui, tr("Refresh"), None).clicked() {
                    actions.push(Action::RefreshObjects(id, db.clone(), db.clone()));
                }
            }
            if menu_item(ui, tr("Copy name"), None).clicked() {
                actions.push(Action::CopyText(db.clone()));
            }
            if !c.cfg.kind.is_file_based() || (c.cfg.kind == DbKind::Sqlite && db == "main") {
                menu_separator(ui);
                backup_items(ui, id, Scope { database: db.clone(), ..Default::default() }, actions);
            }
            if !c.cfg.kind.is_file_based() {
                menu_separator(ui);
                if menu_item(ui, tr("Drop database…"), None).clicked() {
                    actions.push(Action::DropDatabase(id, db.clone()));
                }
            }
        });
    }
}

fn schemas(tree: &mut TreeUi<'_, Node>, c: &ConnState, db: &str, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    let list = match c.schemas.get(db) {
        Some(Load::Ready(s)) => s,
        other => {
            let idle = Load::Idle;
            let other = other.unwrap_or(&idle);
            if other.is_idle() {
                actions.push(Action::LoadSchemas(id, db.to_string()));
            }
            status_rows(tree, other, "schemas");
            return;
        }
    };
    let t = Theme::get(tree.ui().ctx());
    for s in list {
        let r = tree.folder_ui(
            Node::Schema(id, db.to_string(), s.clone()),
            s,
            |ui| row(ui, Some(Icon::Schema), t.muted_foreground, s, None),
            |tree| objects(tree, c, db, s, actions),
        );
        r.context_menu(|ui| {
            ui.set_min_width(200.0);
            if menu_item(ui, tr("New console"), None).clicked() {
                actions.push(Action::OpenConsole(id, Some(db.to_string())));
            }
            new_object_items(ui, c, db, s, actions);
            if menu_item(ui, tr("ER diagram"), None).clicked() {
                actions.push(Action::OpenDiagram(id, db.to_string(), s.clone()));
            }
            if menu_item(ui, tr("Compare with…"), None).clicked() {
                actions.push(Action::OpenCompare(id, db.to_string(), s.clone()));
            }
            if menu_item(ui, tr("Copy to another connection…"), None).clicked() {
                actions.push(Action::OpenCopy(id, db.to_string(), s.clone(), None));
            }
            if menu_item(ui, tr("Refresh"), None).clicked() {
                actions.push(Action::RefreshObjects(id, db.to_string(), s.clone()));
            }
            if menu_item(ui, tr("Copy name"), None).clicked() {
                actions.push(Action::CopyText(s.clone()));
            }
            menu_separator(ui);
            // Only pg_dump can back up one schema.
            if c.cfg.kind == DbKind::Postgres && menu_item(ui, tr("Back up schema…"), None).clicked() {
                let scope = Scope { database: db.to_string(), schema: Some(s.clone()), table: None };
                actions.push(Action::Backup(id, scope));
            }
            if menu_item(ui, tr("Drop schema…"), None).clicked() {
                actions.push(Action::DropSchema(id, db.to_string(), s.clone()));
            }
        });
    }
}

/// "Back up… / Restore…" menu entries.
fn backup_items(ui: &mut Ui, id: Uuid, scope: Scope, actions: &mut Vec<Action>) {
    if menu_item(ui, tr("Back up…"), None).clicked() {
        actions.push(Action::Backup(id, scope.clone()));
    }
    if menu_item(ui, tr("Restore…"), None).clicked() {
        actions.push(Action::Restore(id, scope));
    }
}

/// "New table / view / routine…" menu entries for a schema.
fn new_object_items(ui: &mut Ui, c: &ConnState, db: &str, schema: &str, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    if menu_item(ui, tr("New table…"), None).clicked() {
        actions.push(Action::NewTable(id, db.to_string(), schema.to_string()));
    }
    if menu_item(ui, tr("New table from a file…"), None).clicked() {
        actions.push(Action::ImportNewTable(id, db.to_string(), schema.to_string()));
    }
    let kinds: &[(ObjectKind, &str)] = match c.cfg.kind {
        DbKind::Postgres => &[
            (ObjectKind::View, "New view…"),
            (ObjectKind::MaterializedView, "New materialized view…"),
            (ObjectKind::Function, "New function…"),
            (ObjectKind::Procedure, "New procedure…"),
            (ObjectKind::Trigger, "New trigger…"),
            (ObjectKind::Sequence, "New sequence…"),
            (ObjectKind::Type, "New type…"),
        ],
        DbKind::MySql => &[
            (ObjectKind::View, "New view…"),
            (ObjectKind::Function, "New function…"),
            (ObjectKind::Procedure, "New procedure…"),
            (ObjectKind::Trigger, "New trigger…"),
        ],
        DbKind::Sqlite => &[(ObjectKind::View, "New view…"), (ObjectKind::Trigger, "New trigger…")],
        DbKind::ClickHouse => &[
            (ObjectKind::View, "New view…"),
            (ObjectKind::MaterializedView, "New materialized view…"),
            (ObjectKind::Function, "New function…"),
        ],
        DbKind::MsSql => &[
            (ObjectKind::View, "New view…"),
            (ObjectKind::Function, "New function…"),
            (ObjectKind::Procedure, "New procedure…"),
            (ObjectKind::Trigger, "New trigger…"),
            (ObjectKind::Sequence, "New sequence…"),
            (ObjectKind::Type, "New type…"),
        ],
        DbKind::DuckDb => &[
            (ObjectKind::View, "New view…"),
            (ObjectKind::Function, "New macro…"),
            (ObjectKind::Sequence, "New sequence…"),
            (ObjectKind::Type, "New type…"),
        ],
    };
    for (k, text) in kinds {
        if menu_item(ui, tr(text), None).clicked() {
            actions.push(Action::NewFromTemplate(id, db.to_string(), schema.to_string(), *k));
        }
    }
    menu_separator(ui);
}

fn objects(tree: &mut TreeUi<'_, Node>, c: &ConnState, db: &str, schema: &str, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    let key = (db.to_string(), schema.to_string());
    let list = match c.objects.get(&key) {
        Some(Load::Ready(o)) => o,
        other => {
            let idle = Load::Idle;
            let other = other.unwrap_or(&idle);
            if other.is_idle() {
                actions.push(Action::LoadObjects(id, key.0, key.1));
            }
            status_rows(tree, other, "objects");
            return;
        }
    };
    let mut groups: BTreeMap<ObjectKind, Vec<&DbObject>> = BTreeMap::new();
    for o in list {
        let k = match o.kind {
            ObjectKind::Procedure => ObjectKind::Function,
            k => k,
        };
        groups.entry(k).or_default().push(o);
    }
    if groups.is_empty() {
        tree.leaf_ui(Node::Info("empty".into()), "empty", |ui| {
            ui.label(RichText::new(tr("no objects")).color(Theme::get(ui.ctx()).muted_foreground));
        });
    }
    let t = Theme::get(tree.ui().ctx());
    for (kind, items) in groups {
        let label = kind.group_label();
        let count = items.len().to_string();
        let r = tree.folder_ui(
            Node::Group(id, db.to_string(), schema.to_string(), kind),
            label,
            |ui| row(ui, None, t.muted_foreground, tr(label), Some(&count)),
            |tree| {
                for o in items {
                    object(tree, c, o, actions);
                }
            },
        );
        r.context_menu(|ui| {
            ui.set_min_width(200.0);
            new_object_items(ui, c, db, schema, actions);
            if menu_item(ui, tr("Refresh"), None).clicked() {
                actions.push(Action::RefreshObjects(id, db.to_string(), schema.to_string()));
            }
        });
    }
}

fn object(tree: &mut TreeUi<'_, Node>, c: &ConnState, o: &DbObject, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    let t = Theme::get(tree.ui().ctx());
    let color = match o.kind {
        ObjectKind::Table | ObjectKind::ForeignTable => Color32::from_rgb(0x3b, 0x82, 0xf6),
        ObjectKind::View | ObjectKind::MaterializedView => Color32::from_rgb(0x8b, 0x5c, 0xf6),
        ObjectKind::Function | ObjectKind::Procedure => Color32::from_rgb(0x10, 0xb9, 0x81),
        ObjectKind::Trigger => Color32::from_rgb(0xf5, 0x9e, 0x0b),
        _ => t.muted_foreground,
    };
    let glyph = Some(object_icon(o.kind));
    // Overloaded routines share a name, so the label includes the arguments.
    let label = match &o.detail {
        Some(d) => format!("{}({d})", o.path.name),
        None => o.path.name.clone(),
    };
    let detail = match o.kind {
        ObjectKind::Function | ObjectKind::Procedure if c.cfg.kind == DbKind::Postgres => {
            Some(format!("({})", o.detail.as_deref().unwrap_or("")))
        }
        ObjectKind::Trigger => o.detail.as_ref().map(|table| trf("on {table}", &[("table", table)])),
        ObjectKind::Type => o.detail.clone(),
        _ => None,
    };
    let node = Node::Object(id, o.clone());
    let r = if o.kind.has_columns() {
        tree.folder_ui(
            node,
            &label,
            |ui| row(ui, glyph, color, &o.path.name, o.comment.as_deref()),
            |tree| table_children(tree, c, o, actions),
        )
    } else {
        tree.leaf_ui(node, &label, |ui| row(ui, glyph, color, &o.path.name, detail.as_deref()))
    };
    if r.double_clicked() {
        if o.kind.has_data() {
            actions.push(Action::OpenData(id, o.clone()));
        } else {
            actions.push(Action::OpenObject(id, o.clone()));
        }
    }
    if let Some(c) = &o.comment {
        r.clone().on_hover_text(c.as_str());
    }
    r.context_menu(|ui| {
        ui.set_min_width(200.0);
        if o.kind.has_data() && menu_item(ui, tr("Open data"), Some("F4")).clicked() {
            actions.push(Action::OpenData(id, o.clone()));
        }
        if menu_item(ui, tr("Structure & DDL"), None).clicked() {
            actions.push(Action::OpenObject(id, o.clone()));
        }
        if o.kind == ObjectKind::Table && menu_item(ui, tr("Edit table…"), None).clicked() {
            actions.push(Action::EditTable(id, o.path.clone()));
        }
        menu_separator(ui);
        if menu_item(ui, tr("Copy name"), None).clicked() {
            actions.push(Action::CopyText(o.path.name.clone()));
        }
        if menu_item(ui, tr("Copy qualified name"), None).clicked() {
            actions.push(Action::CopyText(dbman_core::sql::Dialect(c.cfg.kind).qualified(&o.path)));
        }
        menu_separator(ui);
        if dbman_core::ddl::rename(c.cfg.kind, o, "x").is_some() && menu_item(ui, tr("Rename…"), None).clicked() {
            actions.push(Action::RenameObject(id, o.clone()));
        }
        if o.kind.has_data() && menu_item(ui, tr("Export data…"), None).clicked() {
            actions.push(Action::ExportData(id, dbman_drivers::transfer::ExportSource::Table(o.path.clone())));
        }
        if o.kind == ObjectKind::Table && menu_item(ui, tr("Import data…"), None).clicked() {
            actions.push(Action::ImportData(id, o.path.clone()));
        }
        if o.kind == ObjectKind::Table && menu_item(ui, tr("Generate test data…"), None).clicked() {
            actions.push(Action::GenerateData(id, o.path.clone()));
        }
        if o.kind == ObjectKind::Table && menu_item(ui, tr("Copy to…"), None).clicked() {
            let p = &o.path;
            actions.push(Action::OpenCopy(id, p.database.clone(), p.schema.clone(), Some(p.name.clone())));
        }
        if o.kind == ObjectKind::Table && menu_item(ui, tr("Truncate…"), None).clicked() {
            actions.push(Action::Truncate(id, o.clone()));
        }
        if o.kind == ObjectKind::Table
            && matches!(c.cfg.kind, DbKind::Postgres | DbKind::MySql | DbKind::ClickHouse)
            && menu_item(ui, tr("Back up table…"), None).clicked()
        {
            let table = match c.cfg.kind {
                DbKind::Postgres => format!("{}.{}", o.path.schema, o.path.name),
                _ => o.path.name.clone(),
            };
            let scope = Scope { database: o.path.database.clone(), schema: None, table: Some(table) };
            actions.push(Action::Backup(id, scope));
        }
        if menu_item(ui, tr("Drop…"), None).clicked() {
            actions.push(Action::DropObject(id, o.clone()));
        }
    });
}

fn table_children(tree: &mut TreeUi<'_, Node>, c: &ConnState, o: &DbObject, actions: &mut Vec<Action>) {
    let id = c.cfg.id;
    let d = match c.details.get(&o.path) {
        Some(Load::Ready(d)) => d,
        other => {
            let idle = Load::Idle;
            let other = other.unwrap_or(&idle);
            if other.is_idle() {
                actions.push(Action::LoadDetails(id, o.path.clone()));
            }
            status_rows(tree, other, "details");
            return;
        }
    };
    let t = Theme::get(tree.ui().ctx());
    let key_color = Color32::from_rgb(0xea, 0xb3, 0x08);
    for col in &d.columns {
        let glyph = Some(if col.primary_key { Icon::Key } else { Icon::Column });
        let mut ty = col.data_type.clone();
        if !col.nullable {
            ty.push_str(" not null");
        }
        let color = if col.primary_key { key_color } else { t.muted_foreground };
        let r = tree.leaf_ui(Node::Column(id, o.path.clone(), col.name.clone()), &format!("col:{}", col.name), |ui| {
            row(ui, glyph, color, &col.name, Some(&ty))
        });
        if let Some(cm) = &col.comment {
            r.on_hover_text(cm.as_str());
        }
    }
    if !d.indexes.is_empty() {
        tree.folder_ui(
            Node::Info(format!("{:?}:indexes", o.path)),
            "grp:indexes",
            |ui| row(ui, None, t.muted_foreground, tr("indexes"), Some(&d.indexes.len().to_string())),
            |tree| {
                for i in &d.indexes {
                    let extra = format!("({}){}", i.columns.join(", "), if i.unique { " unique" } else { "" });
                    tree.leaf_ui(
                        Node::Info(format!("{:?}:idx:{}", o.path, i.name)),
                        &format!("idx:{}", i.name),
                        |ui| {
                            row(
                                ui,
                                Some(if i.primary { Icon::Key } else { Icon::Index }),
                                key_color,
                                &i.name,
                                Some(&extra),
                            )
                        },
                    );
                }
            },
        );
    }
    if !d.foreign_keys.is_empty() {
        tree.folder_ui(
            Node::Info(format!("{:?}:fks", o.path)),
            "grp:fks",
            |ui| row(ui, None, t.muted_foreground, tr("foreign keys"), Some(&d.foreign_keys.len().to_string())),
            |tree| {
                for f in &d.foreign_keys {
                    let extra = format!("({}) -> {}({})", f.columns.join(", "), f.ref_table, f.ref_columns.join(", "));
                    tree.leaf_ui(Node::Info(format!("{:?}:fk:{}", o.path, f.name)), &format!("fk:{}", f.name), |ui| {
                        row(ui, Some(Icon::ForeignKey), t.muted_foreground, &f.name, Some(&extra))
                    });
                }
            },
        );
    }
}
