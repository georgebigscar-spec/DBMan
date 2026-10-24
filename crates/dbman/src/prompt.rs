//! Small confirm-and-run dialogs: create or drop databases and schemas, drop,
//! rename or truncate objects. Each shows the exact SQL before running it.

use dbman_core::{DbKind, DbObject, ObjectKind, ddl};
use egui::{Context, RichText};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Theme, footer, label};
use uuid::Uuid;

use crate::explorer::Action;
use crate::i18n::{tr, trf};
use crate::sql_editor;

pub enum PromptKind {
    CreateDatabase,
    CreateSchema { db: String },
    DropDatabase { db: String },
    DropSchema { db: String, schema: String },
    DropObject(DbObject),
    Rename(DbObject),
    Truncate(DbObject),
}

pub struct Prompt {
    pub conn: Uuid,
    pub kind: PromptKind,
    pub db_kind: DbKind,
    name: String,
    cascade: bool,
    force: bool,
    pub running: bool,
    pub error: Option<String>,
}

pub enum PromptOutcome {
    /// Run these statements in this database.
    Run {
        db: String,
        statements: Vec<String>,
    },
    Close,
}

impl Prompt {
    pub fn new(conn: Uuid, db_kind: DbKind, kind: PromptKind) -> Self {
        let name = match &kind {
            PromptKind::Rename(o) => o.path.name.clone(),
            _ => String::new(),
        };
        Self { conn, kind, db_kind, name, cascade: false, force: false, running: false, error: None }
    }

    fn title(&self) -> String {
        match &self.kind {
            PromptKind::CreateDatabase => tr("New database").into(),
            PromptKind::CreateSchema { .. } => tr("New schema").into(),
            PromptKind::DropDatabase { db } => trf("Drop database {db}?", &[("db", db)]),
            PromptKind::DropSchema { schema, .. } => trf("Drop schema {schema}?", &[("schema", schema)]),
            PromptKind::DropObject(o) => {
                trf("Drop {kind} {name}?", &[("kind", &tr(o.kind.label())), ("name", &o.path.name)])
            }
            PromptKind::Rename(o) => {
                trf("Rename {kind} {name}", &[("kind", &tr(o.kind.label())), ("name", &o.path.name)])
            }
            PromptKind::Truncate(o) => trf("Delete all rows of {name}?", &[("name", &o.path.name)]),
        }
    }

    fn description(&self) -> &'static str {
        match &self.kind {
            PromptKind::CreateDatabase | PromptKind::CreateSchema { .. } => tr("Enter a name."),
            PromptKind::Rename(_) => tr("Enter the new name."),
            PromptKind::Truncate(_) => tr("Every row is deleted. This cannot be undone."),
            _ => tr("This cannot be undone."),
        }
    }

    fn needs_name(&self) -> bool {
        matches!(self.kind, PromptKind::CreateDatabase | PromptKind::CreateSchema { .. } | PromptKind::Rename(_))
    }

    fn destructive(&self) -> bool {
        !self.needs_name()
    }

    /// The database to run the statements in ("" for the connection's default).
    fn database(&self) -> String {
        match &self.kind {
            // MongoDB drops the database a statement runs in.
            PromptKind::DropDatabase { db } if self.db_kind == DbKind::MongoDb => db.clone(),
            PromptKind::CreateDatabase | PromptKind::DropDatabase { .. } => String::new(),
            PromptKind::CreateSchema { db } | PromptKind::DropSchema { db, .. } => db.clone(),
            PromptKind::DropObject(o) | PromptKind::Rename(o) | PromptKind::Truncate(o) => o.path.database.clone(),
        }
    }

    fn statements(&self) -> Result<Vec<String>, String> {
        let k = self.db_kind;
        let name = self.name.trim();
        if self.needs_name() && name.is_empty() {
            return Err(tr("Enter a name.").into());
        }
        Ok(vec![match &self.kind {
            PromptKind::CreateDatabase => ddl::create_database(k, name),
            PromptKind::CreateSchema { .. } => ddl::create_schema(k, name),
            PromptKind::DropDatabase { db } => {
                let mut s = ddl::drop_database(k, db);
                if self.force && k == DbKind::Postgres {
                    s.push_str(" WITH (FORCE)");
                }
                s
            }
            PromptKind::DropSchema { schema, .. } => ddl::drop_schema(k, schema, self.cascade),
            PromptKind::DropObject(o) => ddl::drop_object(k, o, self.cascade),
            PromptKind::Rename(o) => ddl::rename(k, o, name).ok_or_else(|| {
                trf("{engine} cannot rename a {kind}.", &[("engine", &k.label()), ("kind", &tr(o.kind.label()))])
            })?,
            PromptKind::Truncate(o) => ddl::truncate(k, &o.path),
        }])
    }

    /// What to reload in the explorer after the statements ran.
    pub fn refresh(&self) -> Action {
        match &self.kind {
            PromptKind::CreateDatabase | PromptKind::DropDatabase { .. } => Action::Refresh(self.conn),
            PromptKind::CreateSchema { db } | PromptKind::DropSchema { db, .. } => {
                Action::RefreshSchemas(self.conn, db.clone())
            }
            PromptKind::DropObject(o) | PromptKind::Rename(o) | PromptKind::Truncate(o) => {
                Action::RefreshObjects(self.conn, o.path.database.clone(), o.path.schema.clone())
            }
        }
    }

    pub fn done_message(&self) -> String {
        match &self.kind {
            PromptKind::CreateDatabase => trf("Database {name} created", &[("name", &self.name.trim())]),
            PromptKind::CreateSchema { .. } => trf("Schema {name} created", &[("name", &self.name.trim())]),
            PromptKind::DropDatabase { db } => trf("Database {db} dropped", &[("db", db)]),
            PromptKind::DropSchema { schema, .. } => trf("Schema {schema} dropped", &[("schema", schema)]),
            PromptKind::DropObject(o) => trf("{name} dropped", &[("name", &o.path.name)]),
            PromptKind::Rename(o) => {
                trf("{name} renamed to {new_name}", &[("name", &o.path.name), ("new_name", &self.name.trim())])
            }
            PromptKind::Truncate(o) => trf("{name} emptied", &[("name", &o.path.name)]),
        }
    }

    pub fn show(&mut self, ctx: &Context) -> Option<PromptOutcome> {
        let mut outcome = None;
        let title = self.title();
        let r = Dialog::new("sql-prompt", title).description(self.description()).width(520.0).show(ctx, |ui| {
            let t = Theme::get(ui.ctx());
            if self.needs_name() {
                label(ui, tr("Name"));
                let r = ui.add(Input::new(&mut self.name).width(ui.available_width()));
                if !r.has_focus() && self.name.is_empty() {
                    r.request_focus();
                }
                ui.add_space(8.0);
            }
            let pg = self.db_kind == DbKind::Postgres;
            let cascade_applies = pg
                && match &self.kind {
                    PromptKind::DropSchema { .. } => true,
                    PromptKind::DropObject(o) => o.kind != ObjectKind::Trigger,
                    _ => false,
                };
            if cascade_applies {
                ui.add(Checkbox::new(&mut self.cascade).label(tr("Cascade (also drop objects that depend on it)")));
            }
            if pg && matches!(self.kind, PromptKind::DropDatabase { .. }) {
                ui.add(Checkbox::new(&mut self.force).label(tr("Force (disconnect other sessions first)")));
            }
            let statements = self.statements();
            ui.add_space(8.0);
            match &statements {
                Ok(s) => sql_editor::viewer(ui, egui::Id::new("sql-prompt-sql"), &(s.join(";\n") + ";"), self.db_kind),
                Err(e) => {
                    ui.label(RichText::new(e.as_str()).color(t.muted_foreground));
                }
            }
            if let Some(e) = &self.error {
                ui.add_space(8.0);
                Alert::new(tr("Failed")).icon("!").description(e.as_str()).destructive().show(ui);
            }
            ui.add_space(12.0);
            footer(ui, |ui| {
                let verb = match &self.kind {
                    PromptKind::Rename(_) => tr("Rename"),
                    PromptKind::Truncate(_) => tr("Delete rows"),
                    _ if self.destructive() => tr("Drop"),
                    _ => tr("Create"),
                };
                let button = if self.destructive() { Button::destructive(verb) } else { Button::new(verb) };
                let enter = self.needs_name() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.add_enabled(statements.is_ok() && !self.running, button).clicked()
                    || (enter && statements.is_ok()))
                    && let Ok(s) = statements.clone()
                {
                    outcome = Some(PromptOutcome::Run { db: self.database(), statements: s });
                }
                if ui.add(Button::outline(tr("Cancel"))).clicked() {
                    outcome = Some(PromptOutcome::Close);
                }
                if self.running {
                    ui.add(shadcn_egui::Spinner::new());
                }
            });
        });
        if r.should_close && outcome.is_none() {
            outcome = Some(PromptOutcome::Close);
        }
        outcome
    }
}
