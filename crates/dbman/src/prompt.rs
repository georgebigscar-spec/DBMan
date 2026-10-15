//! Small confirm-and-run dialogs: create or drop databases and schemas, drop,
//! rename or truncate objects. Each shows the exact SQL before running it.

use dbman_core::{DbKind, DbObject, ObjectKind, ddl};
use egui::{Context, RichText};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Theme, footer, label};
use uuid::Uuid;

use crate::explorer::Action;
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
            PromptKind::CreateDatabase => "New database".into(),
            PromptKind::CreateSchema { .. } => "New schema".into(),
            PromptKind::DropDatabase { db } => format!("Drop database {db}?"),
            PromptKind::DropSchema { schema, .. } => format!("Drop schema {schema}?"),
            PromptKind::DropObject(o) => format!("Drop {} {}?", o.kind.label(), o.path.name),
            PromptKind::Rename(o) => format!("Rename {} {}", o.kind.label(), o.path.name),
            PromptKind::Truncate(o) => format!("Delete all rows of {}?", o.path.name),
        }
    }

    fn description(&self) -> &'static str {
        match &self.kind {
            PromptKind::CreateDatabase | PromptKind::CreateSchema { .. } => "Enter a name.",
            PromptKind::Rename(_) => "Enter the new name.",
            PromptKind::Truncate(_) => "Every row is deleted. This cannot be undone.",
            _ => "This cannot be undone.",
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
            PromptKind::CreateDatabase | PromptKind::DropDatabase { .. } => String::new(),
            PromptKind::CreateSchema { db } | PromptKind::DropSchema { db, .. } => db.clone(),
            PromptKind::DropObject(o) | PromptKind::Rename(o) | PromptKind::Truncate(o) => o.path.database.clone(),
        }
    }

    fn statements(&self) -> Result<Vec<String>, String> {
        let k = self.db_kind;
        let name = self.name.trim();
        if self.needs_name() && name.is_empty() {
            return Err("Enter a name.".into());
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
            PromptKind::Rename(o) => {
                ddl::rename(k, o, name).ok_or_else(|| format!("{} cannot rename a {}.", k.label(), o.kind.label()))?
            }
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
            PromptKind::CreateDatabase => format!("Database {} created", self.name.trim()),
            PromptKind::CreateSchema { .. } => format!("Schema {} created", self.name.trim()),
            PromptKind::DropDatabase { db } => format!("Database {db} dropped"),
            PromptKind::DropSchema { schema, .. } => format!("Schema {schema} dropped"),
            PromptKind::DropObject(o) => format!("{} dropped", o.path.name),
            PromptKind::Rename(o) => format!("{} renamed to {}", o.path.name, self.name.trim()),
            PromptKind::Truncate(o) => format!("{} emptied", o.path.name),
        }
    }

    pub fn show(&mut self, ctx: &Context) -> Option<PromptOutcome> {
        let mut outcome = None;
        let title = self.title();
        let r = Dialog::new("sql-prompt", title).description(self.description()).width(520.0).show(ctx, |ui| {
            let t = Theme::get(ui.ctx());
            if self.needs_name() {
                label(ui, "Name");
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
                ui.add(Checkbox::new(&mut self.cascade).label("Cascade (also drop objects that depend on it)"));
            }
            if pg && matches!(self.kind, PromptKind::DropDatabase { .. }) {
                ui.add(Checkbox::new(&mut self.force).label("Force (disconnect other sessions first)"));
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
                Alert::new("Failed").icon("!").description(e.as_str()).destructive().show(ui);
            }
            ui.add_space(12.0);
            footer(ui, |ui| {
                let verb = match &self.kind {
                    PromptKind::Rename(_) => "Rename",
                    PromptKind::Truncate(_) => "Delete rows",
                    _ if self.destructive() => "Drop",
                    _ => "Create",
                };
                let button = if self.destructive() { Button::destructive(verb) } else { Button::new(verb) };
                let enter = self.needs_name() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.add_enabled(statements.is_ok() && !self.running, button).clicked()
                    || (enter && statements.is_ok()))
                    && let Ok(s) = statements.clone()
                {
                    outcome = Some(PromptOutcome::Run { db: self.database(), statements: s });
                }
                if ui.add(Button::outline("Cancel")).clicked() {
                    outcome = Some(PromptOutcome::Close);
                }
                if self.running {
                    ui.add(egui::Spinner::new());
                }
            });
        });
        if r.should_close && outcome.is_none() {
            outcome = Some(PromptOutcome::Close);
        }
        outcome
    }
}
