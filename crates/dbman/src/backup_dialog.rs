//! Backup and restore dialogs: the options each engine's tools offer, the
//! file to write or read, and the exact command that will run. Starting a job
//! hands it to the background jobs panel and closes the dialog.

use std::path::{Path, PathBuf};

use dbman_core::backup::{
    self, Content, DuckMethod, MyDumpOptions, MyRestoreOptions, PgDumpOptions, PgFormat, PgRestoreOptions,
    SqliteMethod, Tool,
};
use dbman_core::{ConnectionConfig, DbKind};
use dbman_drivers::backup::find_tool;
use egui::{Context, RichText, Ui};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Select, Theme, footer, label};
use uuid::Uuid;

use crate::explorer::Action;
use crate::files::{self, PendingPath, Pick};
use crate::jobs::JobSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Backup,
    Restore,
}

/// What to back up or restore into.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scope {
    pub database: String,
    /// One schema (PostgreSQL).
    pub schema: Option<String>,
    /// One table, `schema.table` in PostgreSQL.
    pub table: Option<String>,
}

pub struct StartJob {
    pub title: String,
    pub spec: JobSpec,
    pub output: Option<PathBuf>,
    pub refresh: Option<Action>,
    pub tools_dir: String,
}

pub enum BackupOutcome {
    Start(Box<StartJob>),
    Close,
}

pub struct BackupDialog {
    conn: Uuid,
    cfg: ConnectionConfig,
    mode: Mode,
    scope: Scope,
    mariadb: bool,
    pg_dump: PgDumpOptions,
    pg_restore: PgRestoreOptions,
    my_dump: MyDumpOptions,
    my_restore: MyRestoreOptions,
    sqlite_method: SqliteMethod,
    duck_method: DuckMethod,
    path: String,
    /// Format of the PostgreSQL file chosen for restore.
    detected: Option<PgFormat>,
    detected_for: String,
    schemas_text: String,
    tables_text: String,
    exclude_data_text: String,
    tools_dir: String,
    browsing: Option<PendingPath>,
    error: Option<String>,
}

/// Comma- or newline-separated names.
fn names(text: &str) -> Vec<String> {
    text.split([',', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// `YYYYMMDD-HHMMSS` in UTC, for default file names.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

fn default_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf).or_else(|| Some(u.home_dir().to_path_buf())))
        .unwrap_or_else(std::env::temp_dir)
}

impl BackupDialog {
    /// `version` is the server's version string (tells MariaDB from MySQL).
    pub fn new(conn: Uuid, cfg: ConnectionConfig, version: &str, mode: Mode, scope: Scope, tools_dir: String) -> Self {
        let mut pg_dump = PgDumpOptions::default();
        let mut my_dump = MyDumpOptions::default();
        let mut tables_text = String::new();
        let mut schemas_text = String::new();
        if let Some(s) = &scope.schema {
            schemas_text = s.clone();
        }
        if let Some(t) = &scope.table {
            tables_text = t.clone();
            pg_dump.tables = vec![t.clone()];
            my_dump.tables = vec![t.clone()];
        }
        let mut me = Self {
            conn,
            mariadb: version.contains("MariaDB"),
            mode,
            pg_dump,
            pg_restore: PgRestoreOptions::default(),
            my_dump,
            my_restore: MyRestoreOptions::default(),
            sqlite_method: SqliteMethod::default(),
            duck_method: DuckMethod::default(),
            path: String::new(),
            detected: None,
            detected_for: String::new(),
            schemas_text,
            tables_text,
            exclude_data_text: String::new(),
            tools_dir,
            browsing: None,
            error: None,
            cfg,
            scope,
        };
        if mode == Mode::Backup {
            me.path = default_dir().join(me.file_name()).to_string_lossy().into_owned();
        }
        me
    }

    /// Sets the file to write or read (tests; the dialog has a Browse button).
    pub fn set_path(&mut self, path: &Path) {
        self.path = path.to_string_lossy().into_owned();
    }

    fn kind(&self) -> DbKind {
        self.cfg.kind
    }

    fn file_name(&self) -> String {
        let base = match (&self.scope.table, &self.scope.schema) {
            (Some(t), _) => format!("{}_{}", self.scope.database, t.rsplit('.').next().unwrap_or(t)),
            (None, Some(s)) => format!("{}_{s}", self.scope.database),
            _ if self.kind().is_file_based() => self.cfg.database.clone(),
            _ => self.scope.database.clone(),
        };
        if self.kind() == DbKind::DuckDb {
            return backup::duck_file_name(&base, &timestamp(), self.duck_method);
        }
        backup::default_file_name(self.kind(), &base, &timestamp(), self.pg_dump.format)
    }

    fn title(&self) -> String {
        let what = match (&self.scope.table, &self.scope.schema) {
            (Some(t), _) => format!("table {t}"),
            (None, Some(s)) => format!("schema {s}"),
            _ if self.kind().is_file_based() => self.cfg.display_name(),
            _ => format!("database {}", self.scope.database),
        };
        match self.mode {
            Mode::Backup => format!("Back up {what}"),
            Mode::Restore => format!("Restore into {what}"),
        }
    }

    /// The DuckDB catalog to back up: the one chosen, else the file's own
    /// (DuckDB names it after the file).
    fn duck_database(&self) -> String {
        if !self.scope.database.is_empty() {
            return self.scope.database.clone();
        }
        Path::new(self.cfg.database.trim()).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn tool_dir(&self) -> Option<&Path> {
        Some(Path::new(self.tools_dir.trim())).filter(|p| !p.as_os_str().is_empty())
    }

    /// The tools to use, MariaDB's own ones first against a MariaDB server.
    fn pick_tool(&self, candidates: &[Tool]) -> Result<(Tool, PathBuf), String> {
        candidates.iter().find_map(|t| find_tool(*t, self.tool_dir()).map(|p| (*t, p))).ok_or_else(|| {
            let names: Vec<&str> = candidates.iter().map(|t| t.program()).collect();
            format!(
                "{} not found. Install the client tools or set the folder that contains them below.",
                names.join(" / ")
            )
        })
    }

    /// The job to run, or why it cannot run yet.
    fn job(&self) -> Result<JobSpec, String> {
        let path = PathBuf::from(self.path.trim());
        if self.path.trim().is_empty() {
            return Err("Choose a file.".into());
        }
        let db = &self.scope.database;
        match (self.kind(), self.mode) {
            (DbKind::Postgres, Mode::Backup) => {
                let (_, program) = self.pick_tool(&[Tool::PgDump])?;
                let mut o = self.pg_dump.clone();
                o.schemas = names(&self.schemas_text);
                o.tables = names(&self.tables_text);
                o.exclude_data = names(&self.exclude_data_text);
                let cmd = backup::pg_dump(&self.cfg, db, &o, &path);
                Ok(JobSpec::Tool { program, cmd, watch: Some(path) })
            }
            (DbKind::Postgres, Mode::Restore) => {
                let format = self.detected.ok_or("Choose an existing dump file or directory.")?;
                let tool = if format == PgFormat::Plain { Tool::Psql } else { Tool::PgRestore };
                let (_, program) = self.pick_tool(&[tool])?;
                let mut o = self.pg_restore.clone();
                o.schemas = names(&self.schemas_text);
                o.tables = names(&self.tables_text);
                let cmd = backup::pg_restore(&self.cfg, db, &o, &path, format);
                Ok(JobSpec::Tool { program, cmd, watch: None })
            }
            (DbKind::MySql, Mode::Backup) => {
                let order: &[Tool] = if self.mariadb {
                    &[Tool::MariaDbDump, Tool::MySqlDump]
                } else {
                    &[Tool::MySqlDump, Tool::MariaDbDump]
                };
                let (tool, program) = self.pick_tool(order)?;
                let mut o = self.my_dump.clone();
                o.tables = names(&self.tables_text);
                let cmd = backup::my_dump(&self.cfg, tool, self.mariadb, db, &o, &path);
                Ok(JobSpec::Tool { program, cmd, watch: Some(path) })
            }
            (DbKind::MySql, Mode::Restore) => {
                if !path.is_file() {
                    return Err("Choose an existing SQL file.".into());
                }
                let order: &[Tool] =
                    if self.mariadb { &[Tool::MariaDb, Tool::MySql] } else { &[Tool::MySql, Tool::MariaDb] };
                let (tool, program) = self.pick_tool(order)?;
                let cmd = backup::my_restore(&self.cfg, tool, db, &self.my_restore, &path);
                Ok(JobSpec::Tool { program, cmd, watch: None })
            }
            (DbKind::Sqlite, Mode::Backup) => {
                let source = PathBuf::from(self.cfg.database.trim());
                if source == path {
                    return Err("The backup cannot overwrite the database itself.".into());
                }
                Ok(JobSpec::SqliteBackup { db: source, target: path, method: self.sqlite_method })
            }
            (DbKind::Sqlite, Mode::Restore) => {
                if !path.is_file() {
                    return Err("Choose an existing backup file.".into());
                }
                Ok(JobSpec::SqliteRestore { source: path, db: PathBuf::from(self.cfg.database.trim()) })
            }
            (DbKind::DuckDb, Mode::Backup) => {
                if path.exists() {
                    return Err(format!(
                        "{} already exists; DuckDB writes a new {}.",
                        path.display(),
                        if self.duck_method.writes_folder() { "folder" } else { "file" }
                    ));
                }
                let statements = backup::duck_backup(&self.duck_database(), self.duck_method, &path);
                Ok(JobSpec::DuckDb { db: self.cfg.database.trim().to_string(), statements, watch: Some(path) })
            }
            (DbKind::DuckDb, Mode::Restore) => {
                if !path.exists() {
                    return Err("Choose an export folder or a database file.".into());
                }
                let statements = backup::duck_restore(&self.duck_database(), &path);
                Ok(JobSpec::DuckDb { db: self.cfg.database.trim().to_string(), statements, watch: None })
            }
        }
    }

    fn preview(&self, job: &JobSpec) -> String {
        match job {
            JobSpec::Tool { cmd, .. } => cmd.display(),
            JobSpec::SqliteBackup { db, target, method: SqliteMethod::OnlineBackup } => {
                format!("online backup API: copy every page of {} to {}", db.display(), target.display())
            }
            JobSpec::SqliteBackup { target, method: SqliteMethod::VacuumInto, .. } => {
                format!("VACUUM INTO '{}'", target.display())
            }
            JobSpec::SqliteRestore { source, db } => {
                format!("online backup API: copy every page of {} over {}", source.display(), db.display())
            }
            JobSpec::DuckDb { statements, .. } => {
                statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n")
            }
        }
    }

    fn refresh_detected(&mut self) {
        if self.kind() != DbKind::Postgres || self.mode != Mode::Restore || self.detected_for == self.path {
            return;
        }
        self.detected_for = self.path.clone();
        let p = Path::new(self.path.trim());
        self.detected = if p.exists() { backup::detect_pg_format(p).ok() } else { None };
    }

    fn browse(&mut self, ctx: &Context, kind: Pick) {
        let mut d = rfd::AsyncFileDialog::new();
        let current = Path::new(self.path.trim());
        if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
            d = d.set_directory(dir);
        }
        if kind == Pick::SaveAs {
            d = d.set_file_name(
                current.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| self.file_name()),
            );
        } else if kind == Pick::OpenFile {
            let (name, exts): (&str, &[&str]) = match self.kind() {
                DbKind::Postgres => ("PostgreSQL dumps", &["dump", "backup", "sql", "tar", "pgdump"]),
                DbKind::MySql => ("SQL scripts", &["sql"]),
                DbKind::Sqlite => ("SQLite databases", &["db", "sqlite", "sqlite3", "db3", "bak"]),
                DbKind::DuckDb => ("DuckDB databases", &["duckdb", "ddb", "db"]),
            };
            d = d.add_filter(name, exts).add_filter("All files", &["*"]);
        }
        self.browsing = Some(files::pick(ctx, d, kind));
    }

    pub fn show(&mut self, ctx: &Context) -> Option<BackupOutcome> {
        if let Some(answer) = self.browsing.as_ref().and_then(PendingPath::poll) {
            self.browsing = None;
            if let Some(p) = answer {
                self.path = p.to_string_lossy().into_owned();
            }
        }
        self.refresh_detected();
        let mut outcome = None;
        let description = match (self.mode, self.kind()) {
            (Mode::Backup, DbKind::Sqlite) => "Copies the database file while it stays usable.",
            (Mode::Backup, DbKind::Postgres) => "Runs pg_dump in the background.",
            (Mode::Backup, DbKind::MySql) => "Runs mysqldump in the background.",
            (Mode::Restore, DbKind::Sqlite) => "Replaces the whole database with the backup.",
            (Mode::Restore, DbKind::Postgres) => "Runs pg_restore (or psql for SQL scripts) in the background.",
            (Mode::Restore, DbKind::MySql) => "Runs the SQL file with the mysql client in the background.",
            (Mode::Backup, DbKind::DuckDb) => "Exports the database in the background; it stays usable meanwhile.",
            (Mode::Restore, DbKind::DuckDb) => "Loads an export folder or a database file in the background.",
        };
        let r = Dialog::new(("backup-dialog", self.conn), self.title()).description(description).width(620.0).show(
            ctx,
            |ui| {
                egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.7).show(ui, |ui| {
                    self.file_row(ui);
                    ui.add_space(10.0);
                    match (self.kind(), self.mode) {
                        (DbKind::Postgres, Mode::Backup) => self.pg_dump_ui(ui),
                        (DbKind::Postgres, Mode::Restore) => self.pg_restore_ui(ui),
                        (DbKind::MySql, Mode::Backup) => self.my_dump_ui(ui),
                        (DbKind::MySql, Mode::Restore) => self.my_restore_ui(ui),
                        (DbKind::Sqlite, Mode::Backup) => self.sqlite_backup_ui(ui),
                        (DbKind::Sqlite, Mode::Restore) => self.sqlite_restore_ui(ui),
                        (DbKind::DuckDb, Mode::Backup) => self.duck_backup_ui(ui),
                        (DbKind::DuckDb, Mode::Restore) => self.duck_restore_ui(ui),
                    }
                    if !self.kind().is_file_based() {
                        ui.add_space(10.0);
                        label(ui, "Client tools folder");
                        ui.add(
                            Input::new(&mut self.tools_dir)
                                .placeholder("Found on PATH and in the usual install folders")
                                .width(ui.available_width()),
                        );
                    }
                });
                let job = self.job();
                ui.add_space(10.0);
                let t = Theme::get(ui.ctx());
                match &job {
                    Ok(j) => {
                        label(ui, "Command");
                        egui::Frame::new()
                            .fill(t.muted)
                            .corner_radius(t.radius_md())
                            .inner_margin(egui::Margin::same(8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.add(egui::Label::new(RichText::new(self.preview(j)).monospace().size(12.0)).wrap());
                            });
                    }
                    Err(e) => {
                        Alert::new("Not ready").icon("!").description(e.as_str()).show(ui);
                    }
                }
                if let Some(e) = &self.error {
                    ui.add_space(6.0);
                    Alert::new("Failed").icon("!").description(e.as_str()).destructive().show(ui);
                }
                ui.add_space(12.0);
                footer(ui, |ui| {
                    let (verb, destructive) = match self.mode {
                        Mode::Backup => ("Start backup", false),
                        Mode::Restore => ("Start restore", true),
                    };
                    let button = if destructive { Button::destructive(verb) } else { Button::new(verb) };
                    if ui.add_enabled(job.is_ok(), button).clicked()
                        && let Ok(spec) = job
                    {
                        let output = (self.mode == Mode::Backup).then(|| PathBuf::from(self.path.trim()));
                        let refresh = (self.mode == Mode::Restore).then(|| match self.kind() {
                            DbKind::Postgres => Action::RefreshSchemas(self.conn, self.scope.database.clone()),
                            DbKind::DuckDb => Action::RefreshSchemas(self.conn, self.duck_database()),
                            _ => Action::RefreshObjects(
                                self.conn,
                                self.scope.database.clone(),
                                self.scope.database.clone(),
                            ),
                        });
                        outcome = Some(BackupOutcome::Start(Box::new(StartJob {
                            title: self.title(),
                            spec,
                            output,
                            refresh,
                            tools_dir: self.tools_dir.trim().to_string(),
                        })));
                    }
                    if ui.add(Button::outline("Cancel")).clicked() {
                        outcome = Some(BackupOutcome::Close);
                    }
                });
            },
        );
        if r.should_close && outcome.is_none() {
            outcome = Some(BackupOutcome::Close);
        }
        outcome
    }

    fn file_row(&mut self, ui: &mut Ui) {
        let dir_format = (self.kind() == DbKind::Postgres && self.pg_dump.format == PgFormat::Directory)
            || (self.kind() == DbKind::DuckDb && self.duck_method.writes_folder());
        label(
            ui,
            match self.mode {
                Mode::Backup if dir_format && self.kind() == DbKind::DuckDb => "Export folder (created by DuckDB)",
                Mode::Backup if dir_format => "Backup folder (created by pg_dump)",
                Mode::Backup => "Backup file",
                Mode::Restore => "Backup to restore",
            },
        );
        ui.horizontal(|ui| {
            let folders = matches!(self.kind(), DbKind::Postgres | DbKind::DuckDb);
            let browse_width = if self.mode == Mode::Restore && folders { 200.0 } else { 100.0 };
            ui.add(Input::new(&mut self.path).width(ui.available_width() - browse_width));
            let idle = self.browsing.is_none();
            match self.mode {
                Mode::Backup => {
                    if ui.add_enabled(idle, Button::outline("Browse…")).clicked() {
                        self.browse(ui.ctx(), Pick::SaveAs);
                    }
                }
                Mode::Restore => {
                    if ui.add_enabled(idle, Button::outline("File…")).clicked() {
                        self.browse(ui.ctx(), Pick::OpenFile);
                    }
                    if folders && ui.add_enabled(idle, Button::outline("Folder…")).clicked() {
                        self.browse(ui.ctx(), Pick::Folder);
                    }
                }
            }
        });
        if self.mode == Mode::Restore
            && self.kind() == DbKind::Postgres
            && let Some(f) = self.detected
        {
            let t = Theme::get(ui.ctx());
            ui.label(RichText::new(format!("Format: {}", f.label())).size(12.0).color(t.muted_foreground));
        }
    }

    fn content_select(ui: &mut Ui, content: &mut Content) {
        let labels: Vec<&str> = Content::ALL.iter().map(|c| c.label()).collect();
        let mut idx = Content::ALL.iter().position(|c| c == content);
        ui.add(Select::new(&mut idx, &labels).width(200.0));
        if let Some(i) = idx {
            *content = Content::ALL[i];
        }
    }

    fn pg_dump_ui(&mut self, ui: &mut Ui) {
        let before = self.pg_dump.format;
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                label(ui, "Format");
                let labels: Vec<&str> = PgFormat::ALL.iter().map(|f| f.label()).collect();
                let mut idx = PgFormat::ALL.iter().position(|f| *f == self.pg_dump.format);
                ui.add(Select::new(&mut idx, &labels).width(200.0));
                if let Some(i) = idx {
                    self.pg_dump.format = PgFormat::ALL[i];
                }
            });
            ui.vertical(|ui| {
                label(ui, "Content");
                Self::content_select(ui, &mut self.pg_dump.content);
            });
            if self.pg_dump.format == PgFormat::Directory {
                ui.vertical(|ui| {
                    label(ui, "Parallel jobs");
                    let mut jobs = self.pg_dump.jobs as f32;
                    ui.add(egui::DragValue::new(&mut jobs).range(1.0..=16.0).speed(0.1));
                    self.pg_dump.jobs = jobs as u32;
                });
            }
        });
        if self.pg_dump.format != before {
            // Keep the chosen folder, switch the extension.
            let p = PathBuf::from(self.path.trim());
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let new = match self.pg_dump.format.extension() {
                Some(ext) => p.with_file_name(format!("{stem}.{ext}")),
                None => p.with_file_name(stem),
            };
            self.path = new.to_string_lossy().into_owned();
        }
        ui.add_space(8.0);
        label(ui, "Only these schemas (comma-separated, empty for all)");
        ui.add(Input::new(&mut self.schemas_text).placeholder("public, sales").width(ui.available_width()));
        label(ui, "Only these tables");
        ui.add(
            Input::new(&mut self.tables_text)
                .placeholder("public.orders, public.customers")
                .width(ui.available_width()),
        );
        label(ui, "Skip the rows of these tables (keep their definition)");
        ui.add(Input::new(&mut self.exclude_data_text).placeholder("public.audit_log").width(ui.available_width()));
        ui.add_space(8.0);
        let plain = self.pg_dump.format == PgFormat::Plain;
        ui.horizontal_wrapped(|ui| {
            ui.add(Checkbox::new(&mut self.pg_dump.no_owner).label("No owner"));
            ui.add(Checkbox::new(&mut self.pg_dump.no_privileges).label("No privileges"));
            if plain {
                ui.add(Checkbox::new(&mut self.pg_dump.clean).label("DROP before CREATE"));
                ui.add(Checkbox::new(&mut self.pg_dump.create).label("CREATE DATABASE"));
                ui.add(Checkbox::new(&mut self.pg_dump.inserts).label("INSERT instead of COPY"));
            }
        });
    }

    fn pg_restore_ui(&mut self, ui: &mut Ui) {
        let script = self.detected == Some(PgFormat::Plain);
        if !script {
            label(ui, "Content");
            Self::content_select(ui, &mut self.pg_restore.content);
            ui.add_space(8.0);
            label(ui, "Only these schemas");
            ui.add(Input::new(&mut self.schemas_text).placeholder("all").width(ui.available_width()));
            label(ui, "Only these tables");
            ui.add(Input::new(&mut self.tables_text).placeholder("all").width(ui.available_width()));
            ui.add_space(8.0);
        }
        ui.horizontal_wrapped(|ui| {
            ui.add(Checkbox::new(&mut self.pg_restore.single_transaction).label("Single transaction"));
            ui.add(Checkbox::new(&mut self.pg_restore.exit_on_error).label("Stop at the first error"));
            if !script {
                ui.add(Checkbox::new(&mut self.pg_restore.clean).label("Drop objects first"));
                ui.add(Checkbox::new(&mut self.pg_restore.create).label("Create the database"));
                ui.add(Checkbox::new(&mut self.pg_restore.no_owner).label("No owner"));
                ui.add(Checkbox::new(&mut self.pg_restore.no_privileges).label("No privileges"));
            }
        });
        if !script && !self.pg_restore.single_transaction {
            ui.horizontal(|ui| {
                ui.label("Parallel jobs");
                let mut jobs = self.pg_restore.jobs as f32;
                ui.add(egui::DragValue::new(&mut jobs).range(1.0..=16.0).speed(0.1));
                self.pg_restore.jobs = jobs as u32;
            });
        }
    }

    fn my_dump_ui(&mut self, ui: &mut Ui) {
        label(ui, "Content");
        Self::content_select(ui, &mut self.my_dump.content);
        ui.add_space(8.0);
        label(ui, "Only these tables (comma-separated, empty for all)");
        ui.add(Input::new(&mut self.tables_text).placeholder("orders, customers").width(ui.available_width()));
        ui.add_space(8.0);
        let o = &mut self.my_dump;
        ui.horizontal_wrapped(|ui| {
            ui.add(Checkbox::new(&mut o.single_transaction).label("Single transaction (InnoDB snapshot)"));
            ui.add(Checkbox::new(&mut o.routines).label("Procedures and functions"));
            ui.add(Checkbox::new(&mut o.triggers).label("Triggers"));
            ui.add(Checkbox::new(&mut o.events).label("Events"));
            ui.add(Checkbox::new(&mut o.add_drop_table).label("DROP TABLE before CREATE"));
            ui.add(Checkbox::new(&mut o.create_database).label("CREATE DATABASE and USE"));
            ui.add(Checkbox::new(&mut o.hex_blob).label("Binary as hex"));
            ui.add(Checkbox::new(&mut o.extended_insert).label("Multi-row INSERT"));
        });
    }

    fn my_restore_ui(&mut self, ui: &mut Ui) {
        ui.add(Checkbox::new(&mut self.my_restore.force).label("Keep going after errors"));
    }

    fn sqlite_backup_ui(&mut self, ui: &mut Ui) {
        label(ui, "Method");
        let labels: Vec<&str> = SqliteMethod::ALL.iter().map(|m| m.label()).collect();
        let mut idx = SqliteMethod::ALL.iter().position(|m| *m == self.sqlite_method);
        ui.add(Select::new(&mut idx, &labels).width(280.0));
        if let Some(i) = idx {
            self.sqlite_method = SqliteMethod::ALL[i];
        }
    }

    fn duck_backup_ui(&mut self, ui: &mut Ui) {
        let before = self.duck_method;
        label(ui, "Method");
        let labels: Vec<&str> = DuckMethod::ALL.iter().map(|m| m.label()).collect();
        let mut idx = DuckMethod::ALL.iter().position(|m| *m == self.duck_method);
        ui.add(Select::new(&mut idx, &labels).width(280.0));
        if let Some(i) = idx {
            self.duck_method = DuckMethod::ALL[i];
        }
        if self.duck_method.writes_folder() != before.writes_folder() {
            // Keep the chosen folder, switch between a folder and a .duckdb file.
            let p = PathBuf::from(self.path.trim());
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let name = if self.duck_method.writes_folder() { stem } else { format!("{stem}.duckdb") };
            self.path = p.with_file_name(name).to_string_lossy().into_owned();
        }
        let t = Theme::get(ui.ctx());
        let note = match self.duck_method {
            DuckMethod::Parquet | DuckMethod::Csv => {
                "The folder gets schema.sql, load.sql and one data file per table; IMPORT DATABASE reads it back."
            }
            DuckMethod::DatabaseFile => "A new DuckDB file with the same schemas, tables, views and sequences.",
        };
        ui.label(RichText::new(note).size(12.0).color(t.muted_foreground));
    }

    fn duck_restore_ui(&mut self, ui: &mut Ui) {
        Alert::new("Restore into an empty database")
            .icon("!")
            .description(format!(
                "Objects of the backup are created in {}; it fails if any of them already exist.",
                self.duck_database()
            ))
            .show(ui);
    }

    fn sqlite_restore_ui(&mut self, ui: &mut Ui) {
        Alert::new("Overwrites the database")
            .icon("!")
            .description(format!("Every table of {} is replaced by the backup's content.", self.cfg.database))
            .destructive()
            .show(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_shape() {
        let s = timestamp();
        assert_eq!(s.len(), 15, "{s}");
        assert_eq!(&s[8..9], "-");
        assert!(s.starts_with("20"));
    }

    #[test]
    fn splits_names() {
        assert_eq!(names(" a, b ,\nc,, "), vec!["a", "b", "c"]);
    }
}
