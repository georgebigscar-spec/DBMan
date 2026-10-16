//! Chooses where something goes: a saved connection, one of its databases
//! and (on engines with schemas) a schema. Lists load on the picker's own
//! session, which the tab can reuse for its work.

use std::sync::Arc;

use dbman_core::{Capabilities, Connection, ConnectionConfig, DbKind, DbResult};
use egui::{RichText, Ui};
use shadcn_egui::{Select, Theme};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::events::{Event, TabEvent, TabId, Tasks};
use crate::explorer::{ConnState, Load};

pub type Session = Arc<OnceCell<Arc<dyn Connection>>>;

pub enum PickerEvent {
    /// The generation it answers, so late answers for an older choice are dropped.
    Databases(u64, DbResult<Vec<String>>),
    Schemas(u64, DbResult<Vec<String>>),
}

pub struct TargetPicker {
    conn: Option<Uuid>,
    cfg: Option<ConnectionConfig>,
    session: Session,
    generation: u64,
    databases: Load<Vec<String>>,
    db: Option<usize>,
    schemas: Load<Vec<String>>,
    schema: Option<usize>,
    /// Names chosen when they are listed (the source's, at first).
    prefer_db: String,
    prefer_schema: String,
    /// A connection to switch to on the next frame.
    want: Option<Uuid>,
}

impl TargetPicker {
    pub fn new(prefer_db: &str, prefer_schema: &str) -> Self {
        Self {
            conn: None,
            cfg: None,
            session: Arc::new(OnceCell::new()),
            generation: 0,
            databases: Load::Idle,
            db: None,
            schemas: Load::Idle,
            schema: None,
            prefer_db: prefer_db.into(),
            prefer_schema: prefer_schema.into(),
            want: None,
        }
    }

    pub fn set_target(&mut self, conn: Uuid, cfg: ConnectionConfig, tasks: &Tasks, tab: TabId) {
        self.conn = Some(conn);
        self.cfg = Some(cfg);
        self.generation += 1;
        self.session = Arc::new(OnceCell::new());
        self.db = None;
        self.schema = None;
        self.schemas = Load::Idle;
        self.load_databases(tasks, tab);
    }

    /// Switches to a saved connection on the next frame.
    pub fn want(&mut self, conn: Uuid) {
        self.want = Some(conn);
    }

    /// Chooses a database and schema by name now, or once they are listed.
    pub fn choose(&mut self, database: &str, schema: &str) {
        self.prefer_db = database.into();
        self.prefer_schema = schema.into();
        if let Some(dbs) = self.databases.ready()
            && let Some(i) = dbs.iter().position(|d| d == database)
            && self.db != Some(i)
        {
            self.db = Some(i);
            self.schemas = Load::Idle;
        }
        if let Some(ss) = self.schemas.ready() {
            self.schema = ss.iter().position(|s| s == schema).or(self.schema);
        }
    }

    pub fn conn(&self) -> Option<Uuid> {
        self.conn
    }

    pub fn kind(&self) -> Option<DbKind> {
        self.cfg.as_ref().map(|c| c.kind)
    }

    /// Bumped whenever the choice changes.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn has_schemas(&self) -> bool {
        self.kind().is_some_and(|k| Capabilities::for_kind(k).schemas)
    }

    pub fn database(&self) -> Option<&str> {
        self.db.and_then(|i| self.databases.ready()?.get(i)).map(String::as_str)
    }

    /// The schema; on engines without schemas, the database.
    pub fn schema(&self) -> Option<&str> {
        if self.has_schemas() {
            self.schema.and_then(|i| self.schemas.ready()?.get(i)).map(String::as_str)
        } else {
            self.database()
        }
    }

    /// The connection settings for the chosen database (MySQL sessions are per database).
    pub fn cfg(&self) -> Option<ConnectionConfig> {
        let mut cfg = self.cfg.clone()?;
        if cfg.kind == DbKind::MySql
            && let Some(db) = self.database()
        {
            cfg.database = db.to_string();
        }
        Some(cfg)
    }

    pub fn display_name(&self) -> String {
        self.cfg.as_ref().map(|c| c.display_name()).unwrap_or_default()
    }

    /// The picker's session to the chosen target.
    pub fn session(&self) -> Session {
        self.session.clone()
    }

    fn send(tasks: &Tasks, tab: TabId, ev: PickerEvent) {
        tasks.sender().send(Event::Tab { tab, event: TabEvent::Picker(ev) });
    }

    fn load_databases(&mut self, tasks: &Tasks, tab: TabId) {
        let Some(cfg) = self.cfg.clone() else { return };
        self.databases = Load::Loading;
        let (session, generation, t) = (self.session.clone(), self.generation, tasks.clone());
        tasks.runtime().spawn(async move {
            let r = async { session.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?.databases().await }.await;
            Self::send(&t, tab, PickerEvent::Databases(generation, r));
        });
    }

    fn load_schemas(&mut self, tasks: &Tasks, tab: TabId) {
        let (Some(db), true) = (self.database().map(str::to_string), self.has_schemas()) else { return };
        let Some(cfg) = self.cfg() else { return };
        self.schemas = Load::Loading;
        self.schema = None;
        let (session, generation, t) = (self.session.clone(), self.generation, tasks.clone());
        tasks.runtime().spawn(async move {
            let r = async { session.get_or_try_init(|| dbman_drivers::connect(&cfg)).await?.schemas(&db).await }.await;
            Self::send(&t, tab, PickerEvent::Schemas(generation, r));
        });
    }

    pub fn on_event(&mut self, ev: PickerEvent) {
        match ev {
            PickerEvent::Databases(g, r) if g == self.generation => {
                self.databases = Load::from_result(r);
                if let Some(dbs) = self.databases.ready() {
                    self.db = dbs.iter().position(|d| *d == self.prefer_db).or((!dbs.is_empty()).then_some(0));
                }
                self.schemas = Load::Idle;
            }
            PickerEvent::Schemas(g, r) if g == self.generation => {
                self.schemas = Load::from_result(r);
                if let Some(ss) = self.schemas.ready() {
                    self.schema = ss.iter().position(|s| *s == self.prefer_schema).or((!ss.is_empty()).then_some(0));
                }
            }
            _ => {}
        }
    }

    /// Picks a first target (another connection of `kind`, else `own`) and
    /// keeps the schema list loaded. Call every frame.
    pub fn ensure(&mut self, tasks: &Tasks, tab: TabId, conns: &[ConnState], own: Uuid, kind: DbKind) {
        if let Some(id) = self.want.take()
            && let Some(c) = conns.iter().find(|c| c.cfg.id == id)
        {
            self.set_target(c.cfg.id, c.cfg.clone(), tasks, tab);
        }
        if self.conn.is_none() {
            let pick = conns
                .iter()
                .find(|c| c.cfg.id != own && c.cfg.kind == kind)
                .or_else(|| conns.iter().find(|c| c.cfg.id == own))
                .or_else(|| conns.first());
            if let Some(c) = pick {
                self.set_target(c.cfg.id, c.cfg.clone(), tasks, tab);
            }
        }
        if self.has_schemas() && self.schemas.is_idle() && self.database().is_some() {
            self.load_schemas(tasks, tab);
        }
    }

    /// The connection, database and schema selects, in a row.
    pub fn ui(&mut self, ui: &mut Ui, tasks: &Tasks, tab: TabId, conns: &[ConnState]) {
        let t = Theme::get(ui.ctx());
        let names: Vec<String> =
            conns.iter().map(|c| format!("{} ({})", c.cfg.display_name(), c.cfg.kind.label())).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut idx = self.conn.and_then(|id| conns.iter().position(|c| c.cfg.id == id));
        let before = idx;
        ui.push_id("target-conn", |ui| ui.add(Select::new(&mut idx, &refs).width(220.0)));
        if idx != before
            && let Some(c) = idx.and_then(|i| conns.get(i))
        {
            self.set_target(c.cfg.id, c.cfg.clone(), tasks, tab);
        }
        match &self.databases {
            Load::Ready(dbs) => {
                let refs: Vec<&str> = dbs.iter().map(String::as_str).collect();
                let before = self.db;
                ui.push_id("target-db", |ui| {
                    ui.add(Select::new(&mut self.db, &refs).placeholder("Database").width(160.0))
                });
                if before != self.db {
                    self.generation += 1;
                    if self.kind() == Some(DbKind::MySql) {
                        self.session = Arc::new(OnceCell::new());
                    }
                    self.schemas = Load::Idle;
                }
            }
            Load::Failed(e) => {
                ui.label(RichText::new("Could not connect").color(t.destructive)).on_hover_text(e.as_str());
            }
            _ => {
                ui.add(egui::Spinner::new());
            }
        }
        if self.has_schemas() {
            match &self.schemas {
                Load::Ready(ss) => {
                    let refs: Vec<&str> = ss.iter().map(String::as_str).collect();
                    let before = self.schema;
                    ui.push_id("target-schema", |ui| {
                        ui.add(Select::new(&mut self.schema, &refs).placeholder("Schema").width(140.0))
                    });
                    if before != self.schema {
                        self.generation += 1;
                    }
                }
                Load::Failed(e) => {
                    ui.label(RichText::new("No schemas").color(t.destructive)).on_hover_text(e.as_str());
                }
                _ => {}
            }
        }
    }
}
