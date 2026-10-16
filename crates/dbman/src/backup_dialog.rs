//! Backup and restore dialogs: the options each engine's tools offer, the
//! file to write or read, and the exact command that will run. Starting a job
//! hands it to the background jobs panel and closes the dialog.

use std::path::{Path, PathBuf};

use dbman_core::backup::{
    self, ChBackupOptions, ChDestination, ChRestoreOptions, Content, DuckMethod, MsBackupOptions, MsBackupType,
    MsRestoreOptions, MyDumpOptions, MyRestoreOptions, PgDumpOptions, PgFormat, PgRestoreOptions, SqliteMethod, Tool,
};
use dbman_core::{ConnectionConfig, DbKind};
use dbman_drivers::backup::find_tool;
use egui::{Context, RichText, Ui};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Select, Theme, footer, label};
use uuid::Uuid;

use crate::explorer::Action;
use crate::files::{self, PendingPath, Pick};
use crate::i18n::{tr, trf};
use crate::jobs::JobSpec;
use dbman_drivers::backup::MsJob;

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
    ms_backup: MsBackupOptions,
    ms_restore: MsRestoreOptions,
    ch_backup: ChBackupOptions,
    ch_restore: ChRestoreOptions,
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
            ms_backup: MsBackupOptions::default(),
            ms_restore: MsRestoreOptions::default(),
            ch_backup: ChBackupOptions::default(),
            ch_restore: ChRestoreOptions::default(),
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
        if me.kind() == DbKind::ClickHouse {
            let name = me.file_name();
            me.ch_backup.destination = ChDestination::Disk { disk: "backups".into(), path: name };
            me.ch_restore.source = ChDestination::Disk { disk: "backups".into(), path: String::new() };
        }
        if mode == Mode::Backup {
            // SQL Server writes on its own machine; a bare name goes to its default backup folder.
            me.path = if me.kind() == DbKind::MsSql {
                me.file_name()
            } else {
                default_dir().join(me.file_name()).to_string_lossy().into_owned()
            };
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
        let backup = self.mode == Mode::Backup;
        match (&self.scope.table, &self.scope.schema) {
            (Some(t), _) if backup => trf("Back up table {name}", &[("name", t)]),
            (Some(t), _) => trf("Restore into table {name}", &[("name", t)]),
            (None, Some(s)) if backup => trf("Back up schema {name}", &[("name", s)]),
            (None, Some(s)) => trf("Restore into schema {name}", &[("name", s)]),
            _ if self.kind().is_file_based() => {
                let name = self.cfg.display_name();
                if backup {
                    trf("Back up {name}", &[("name", &name)])
                } else {
                    trf("Restore into {name}", &[("name", &name)])
                }
            }
            _ if backup => trf("Back up database {name}", &[("name", &self.scope.database)]),
            _ => trf("Restore into database {name}", &[("name", &self.scope.database)]),
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
            trf(
                "{tools} not found. Install the client tools or set the folder that contains them below.",
                &[("tools", &names.join(" / "))],
            )
        })
    }

    /// A ClickHouse BACKUP or RESTORE; the server reads and writes the files.
    fn ch_job(&self) -> Result<JobSpec, String> {
        let db = &self.scope.database;
        let table = self.scope.table.as_deref();
        let dest = if self.mode == Mode::Backup { &self.ch_backup.destination } else { &self.ch_restore.source };
        let missing = match dest {
            ChDestination::Disk { disk, path } => disk.trim().is_empty() || path.trim().is_empty(),
            ChDestination::File { path } => path.trim().is_empty(),
            ChDestination::S3 { url, .. } => url.trim().is_empty(),
        };
        if missing {
            return Err(tr("Fill in where the backup is.").into());
        }
        let (sql, shown) = match self.mode {
            Mode::Backup => (
                backup::ch_backup(db, table, &self.ch_backup, false),
                backup::ch_backup(db, table, &self.ch_backup, true),
            ),
            Mode::Restore => (
                backup::ch_restore(db, table, &self.ch_restore, false),
                backup::ch_restore(db, table, &self.ch_restore, true),
            ),
        };
        Ok(JobSpec::ClickHouse { cfg: Box::new(self.cfg.clone()), sql, shown })
    }

    /// The job to run, or why it cannot run yet.
    fn job(&self) -> Result<JobSpec, String> {
        if self.kind() == DbKind::ClickHouse {
            return self.ch_job();
        }
        let path = PathBuf::from(self.path.trim());
        if self.path.trim().is_empty() {
            return Err(tr("Choose a file.").into());
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
                let format = self.detected.ok_or(tr("Choose an existing dump file or directory."))?;
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
                    return Err(tr("Choose an existing SQL file.").into());
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
                    return Err(tr("The backup cannot overwrite the database itself.").into());
                }
                Ok(JobSpec::SqliteBackup { db: source, target: path, method: self.sqlite_method })
            }
            (DbKind::Sqlite, Mode::Restore) => {
                if !path.is_file() {
                    return Err(tr("Choose an existing backup file.").into());
                }
                Ok(JobSpec::SqliteRestore { source: path, db: PathBuf::from(self.cfg.database.trim()) })
            }
            (DbKind::ClickHouse, _) => self.ch_job(),
            (DbKind::MsSql, Mode::Backup) => Ok(JobSpec::MsSql {
                cfg: Box::new(self.cfg.clone()),
                job: MsJob::Backup {
                    database: db.clone(),
                    options: self.ms_backup.clone(),
                    path: self.path.trim().into(),
                },
            }),
            (DbKind::MsSql, Mode::Restore) => {
                if db.trim().is_empty() {
                    return Err(tr("Choose the database to restore into.").into());
                }
                Ok(JobSpec::MsSql {
                    cfg: Box::new(self.cfg.clone()),
                    job: MsJob::Restore {
                        database: db.clone(),
                        options: self.ms_restore.clone(),
                        path: self.path.trim().into(),
                    },
                })
            }
            (DbKind::DuckDb, Mode::Backup) => {
                if path.exists() {
                    let path = path.display();
                    return Err(if self.duck_method.writes_folder() {
                        trf("{path} already exists; DuckDB writes a new folder.", &[("path", &path)])
                    } else {
                        trf("{path} already exists; DuckDB writes a new file.", &[("path", &path)])
                    });
                }
                let statements = backup::duck_backup(&self.duck_database(), self.duck_method, &path);
                Ok(JobSpec::DuckDb { db: self.cfg.database.trim().to_string(), statements, watch: Some(path) })
            }
            (DbKind::DuckDb, Mode::Restore) => {
                if !path.exists() {
                    return Err(tr("Choose an export folder or a database file.").into());
                }
                let statements = backup::duck_restore(&self.duck_database(), &path);
                Ok(JobSpec::DuckDb { db: self.cfg.database.trim().to_string(), statements, watch: None })
            }
        }
    }

    fn preview(&self, job: &JobSpec) -> String {
        match job {
            JobSpec::Tool { cmd, .. } => cmd.display(),
            JobSpec::SqliteBackup { db, target, method: SqliteMethod::OnlineBackup } => trf(
                "online backup API: copy every page of {source} to {target}",
                &[("source", &db.display()), ("target", &target.display())],
            ),
            JobSpec::SqliteBackup { target, method: SqliteMethod::VacuumInto, .. } => {
                format!("VACUUM INTO '{}'", target.display())
            }
            JobSpec::SqliteRestore { source, db } => trf(
                "online backup API: copy every page of {source} over {target}",
                &[("source", &source.display()), ("target", &db.display())],
            ),
            JobSpec::ClickHouse { shown, .. } => format!("{shown};"),
            JobSpec::MsSql { job: MsJob::Backup { database, options, path }, .. } => {
                backup::ms_backup(database, options, path)
                    .iter()
                    .map(|s| format!("{s};"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            JobSpec::MsSql { job: MsJob::Restore { database, options, path }, .. } => {
                let moves: Vec<(String, String)> = if options.relocate {
                    vec![("<each file>".into(), format!("<default data or log folder>/{database}…"))]
                } else {
                    Vec::new()
                };
                let (mut lines, cleanup) = backup::ms_restore(database, options, path, &moves);
                lines.extend(cleanup);
                lines.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n")
            }
            JobSpec::DuckDb { statements, .. } => {
                statements.iter().map(|s| format!("{s};")).collect::<Vec<_>>().join("\n")
            }
            // Not made by this dialog.
            JobSpec::Export { .. } | JobSpec::Import { .. } | JobSpec::Script { .. } | JobSpec::Copy { .. } => {
                String::new()
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
                DbKind::Postgres => (tr("PostgreSQL dumps"), &["dump", "backup", "sql", "tar", "pgdump"]),
                DbKind::MySql => (tr("SQL scripts"), &["sql"]),
                DbKind::Sqlite => (tr("SQLite databases"), &["db", "sqlite", "sqlite3", "db3", "bak"]),
                DbKind::DuckDb => (tr("DuckDB databases"), &["duckdb", "ddb", "db"]),
                DbKind::MsSql => (tr("SQL Server backups"), &["bak", "trn"]),
                DbKind::ClickHouse => (tr("ClickHouse backups"), &["zip", "tar"]),
            };
            d = d.add_filter(name, exts).add_filter(tr("All files"), &["*"]);
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
            (Mode::Backup, DbKind::Sqlite) => tr("Copies the database file while it stays usable."),
            (Mode::Backup, DbKind::Postgres) => tr("Runs pg_dump in the background."),
            (Mode::Backup, DbKind::MySql) => tr("Runs mysqldump in the background."),
            (Mode::Restore, DbKind::Sqlite) => tr("Replaces the whole database with the backup."),
            (Mode::Restore, DbKind::Postgres) => tr("Runs pg_restore (or psql for SQL scripts) in the background."),
            (Mode::Restore, DbKind::MySql) => tr("Runs the SQL file with the mysql client in the background."),
            (Mode::Backup, DbKind::MsSql) => tr("Runs BACKUP on the server in the background."),
            (Mode::Restore, DbKind::MsSql) => tr("Runs RESTORE on the server in the background."),
            (Mode::Backup, DbKind::ClickHouse) => tr("Runs BACKUP on the server in the background."),
            (Mode::Restore, DbKind::ClickHouse) => tr("Runs RESTORE on the server in the background."),
            (Mode::Backup, DbKind::DuckDb) => tr("Exports the database in the background; it stays usable meanwhile."),
            (Mode::Restore, DbKind::DuckDb) => tr("Loads an export folder or a database file in the background."),
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
                        (DbKind::MsSql, Mode::Backup) => self.ms_backup_ui(ui),
                        (DbKind::MsSql, Mode::Restore) => self.ms_restore_ui(ui),
                        (DbKind::ClickHouse, Mode::Backup) => self.ch_backup_ui(ui),
                        (DbKind::ClickHouse, Mode::Restore) => self.ch_restore_ui(ui),
                        (DbKind::DuckDb, Mode::Backup) => self.duck_backup_ui(ui),
                        (DbKind::DuckDb, Mode::Restore) => self.duck_restore_ui(ui),
                    }
                    // SQL Server runs BACKUP itself; no client tools.
                    if !self.kind().is_file_based() && !matches!(self.kind(), DbKind::MsSql | DbKind::ClickHouse) {
                        ui.add_space(10.0);
                        label(ui, tr("Client tools folder"));
                        ui.add(
                            Input::new(&mut self.tools_dir)
                                .placeholder(tr("Found on PATH and in the usual install folders"))
                                .width(ui.available_width()),
                        );
                    }
                });
                let job = self.job();
                ui.add_space(10.0);
                let t = Theme::get(ui.ctx());
                match &job {
                    Ok(j) => {
                        label(ui, tr("Command"));
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
                        Alert::new(tr("Not ready")).icon("!").description(e.as_str()).show(ui);
                    }
                }
                if let Some(e) = &self.error {
                    ui.add_space(6.0);
                    Alert::new(tr("Failed")).icon("!").description(e.as_str()).destructive().show(ui);
                }
                ui.add_space(12.0);
                footer(ui, |ui| {
                    let (verb, destructive) = match self.mode {
                        Mode::Backup => (tr("Start backup"), false),
                        Mode::Restore => (tr("Start restore"), true),
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
                    if ui.add(Button::outline(tr("Cancel"))).clicked() {
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
        // ClickHouse's destination has its own fields.
        if self.kind() == DbKind::ClickHouse {
            return;
        }
        let dir_format = (self.kind() == DbKind::Postgres && self.pg_dump.format == PgFormat::Directory)
            || (self.kind() == DbKind::DuckDb && self.duck_method.writes_folder());
        label(
            ui,
            match self.mode {
                Mode::Backup if dir_format && self.kind() == DbKind::DuckDb => tr("Export folder (created by DuckDB)"),
                Mode::Backup if dir_format => tr("Backup folder (created by pg_dump)"),
                _ if self.kind() == DbKind::MsSql => tr("Backup file on the server"),
                Mode::Backup => tr("Backup file"),
                Mode::Restore => tr("Backup to restore"),
            },
        );
        ui.horizontal(|ui| {
            let folders = matches!(self.kind(), DbKind::Postgres | DbKind::DuckDb);
            let browse_width = if self.mode == Mode::Restore && folders { 200.0 } else { 100.0 };
            if self.kind() == DbKind::MsSql {
                // The path is on the server, so a local file picker would mislead.
                ui.add(
                    Input::new(&mut self.path)
                        .placeholder(tr("shop.bak, or a full path on the server"))
                        .width(ui.available_width()),
                );
                return;
            }
            ui.add(Input::new(&mut self.path).width(ui.available_width() - browse_width));
            let idle = self.browsing.is_none();
            match self.mode {
                Mode::Backup => {
                    if ui.add_enabled(idle, Button::outline(tr("Browse…"))).clicked() {
                        self.browse(ui.ctx(), Pick::SaveAs);
                    }
                }
                Mode::Restore => {
                    if ui.add_enabled(idle, Button::outline(tr("File…"))).clicked() {
                        self.browse(ui.ctx(), Pick::OpenFile);
                    }
                    if folders && ui.add_enabled(idle, Button::outline(tr("Folder…"))).clicked() {
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
            ui.label(
                RichText::new(trf("Format: {format}", &[("format", &tr(f.label()))]))
                    .size(12.0)
                    .color(t.muted_foreground),
            );
        }
    }

    fn content_select(ui: &mut Ui, content: &mut Content) {
        let labels: Vec<&str> = Content::ALL.iter().map(|c| tr(c.label())).collect();
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
                label(ui, tr("Format"));
                let labels: Vec<&str> = PgFormat::ALL.iter().map(|f| tr(f.label())).collect();
                let mut idx = PgFormat::ALL.iter().position(|f| *f == self.pg_dump.format);
                ui.add(Select::new(&mut idx, &labels).width(200.0));
                if let Some(i) = idx {
                    self.pg_dump.format = PgFormat::ALL[i];
                }
            });
            ui.vertical(|ui| {
                label(ui, tr("Content"));
                Self::content_select(ui, &mut self.pg_dump.content);
            });
            if self.pg_dump.format == PgFormat::Directory {
                ui.vertical(|ui| {
                    label(ui, tr("Parallel jobs"));
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
        label(ui, tr("Only these schemas (comma-separated, empty for all)"));
        ui.add(Input::new(&mut self.schemas_text).placeholder("public, sales").width(ui.available_width()));
        label(ui, tr("Only these tables"));
        ui.add(
            Input::new(&mut self.tables_text)
                .placeholder("public.orders, public.customers")
                .width(ui.available_width()),
        );
        label(ui, tr("Skip the rows of these tables (keep their definition)"));
        ui.add(Input::new(&mut self.exclude_data_text).placeholder("public.audit_log").width(ui.available_width()));
        ui.add_space(8.0);
        let plain = self.pg_dump.format == PgFormat::Plain;
        ui.horizontal_wrapped(|ui| {
            ui.add(Checkbox::new(&mut self.pg_dump.no_owner).label(tr("No owner")));
            ui.add(Checkbox::new(&mut self.pg_dump.no_privileges).label(tr("No privileges")));
            if plain {
                ui.add(Checkbox::new(&mut self.pg_dump.clean).label(tr("DROP before CREATE")));
                ui.add(Checkbox::new(&mut self.pg_dump.create).label(tr("CREATE DATABASE")));
                ui.add(Checkbox::new(&mut self.pg_dump.inserts).label(tr("INSERT instead of COPY")));
            }
        });
    }

    fn pg_restore_ui(&mut self, ui: &mut Ui) {
        let script = self.detected == Some(PgFormat::Plain);
        if !script {
            label(ui, tr("Content"));
            Self::content_select(ui, &mut self.pg_restore.content);
            ui.add_space(8.0);
            label(ui, tr("Only these schemas"));
            ui.add(Input::new(&mut self.schemas_text).placeholder(tr("all")).width(ui.available_width()));
            label(ui, tr("Only these tables"));
            ui.add(Input::new(&mut self.tables_text).placeholder(tr("all")).width(ui.available_width()));
            ui.add_space(8.0);
        }
        ui.horizontal_wrapped(|ui| {
            ui.add(Checkbox::new(&mut self.pg_restore.single_transaction).label(tr("Single transaction")));
            ui.add(Checkbox::new(&mut self.pg_restore.exit_on_error).label(tr("Stop at the first error")));
            if !script {
                ui.add(Checkbox::new(&mut self.pg_restore.clean).label(tr("Drop objects first")));
                ui.add(Checkbox::new(&mut self.pg_restore.create).label(tr("Create the database")));
                ui.add(Checkbox::new(&mut self.pg_restore.no_owner).label(tr("No owner")));
                ui.add(Checkbox::new(&mut self.pg_restore.no_privileges).label(tr("No privileges")));
            }
        });
        if !script && !self.pg_restore.single_transaction {
            ui.horizontal(|ui| {
                ui.label(tr("Parallel jobs"));
                let mut jobs = self.pg_restore.jobs as f32;
                ui.add(egui::DragValue::new(&mut jobs).range(1.0..=16.0).speed(0.1));
                self.pg_restore.jobs = jobs as u32;
            });
        }
    }

    fn my_dump_ui(&mut self, ui: &mut Ui) {
        label(ui, tr("Content"));
        Self::content_select(ui, &mut self.my_dump.content);
        ui.add_space(8.0);
        label(ui, tr("Only these tables (comma-separated, empty for all)"));
        ui.add(Input::new(&mut self.tables_text).placeholder("orders, customers").width(ui.available_width()));
        ui.add_space(8.0);
        let o = &mut self.my_dump;
        ui.horizontal_wrapped(|ui| {
            ui.add(Checkbox::new(&mut o.single_transaction).label(tr("Single transaction (InnoDB snapshot)")));
            ui.add(Checkbox::new(&mut o.routines).label(tr("Procedures and functions")));
            ui.add(Checkbox::new(&mut o.triggers).label(tr("Triggers")));
            ui.add(Checkbox::new(&mut o.events).label(tr("Events")));
            ui.add(Checkbox::new(&mut o.add_drop_table).label(tr("DROP TABLE before CREATE")));
            ui.add(Checkbox::new(&mut o.create_database).label(tr("CREATE DATABASE and USE")));
            ui.add(Checkbox::new(&mut o.hex_blob).label(tr("Binary as hex")));
            ui.add(Checkbox::new(&mut o.extended_insert).label(tr("Multi-row INSERT")));
        });
    }

    fn my_restore_ui(&mut self, ui: &mut Ui) {
        ui.add(Checkbox::new(&mut self.my_restore.force).label(tr("Keep going after errors")));
    }

    fn sqlite_backup_ui(&mut self, ui: &mut Ui) {
        label(ui, tr("Method"));
        let labels: Vec<&str> = SqliteMethod::ALL.iter().map(|m| tr(m.label())).collect();
        let mut idx = SqliteMethod::ALL.iter().position(|m| *m == self.sqlite_method);
        ui.add(Select::new(&mut idx, &labels).width(280.0));
        if let Some(i) = idx {
            self.sqlite_method = SqliteMethod::ALL[i];
        }
    }

    /// Disk / File / S3 fields of a ClickHouse backup location.
    fn ch_destination_ui(ui: &mut Ui, dest: &mut ChDestination, id: &str) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                label(ui, tr("Location"));
                let mut idx = Some(dest.index());
                let labels: Vec<&str> = ChDestination::LABELS.iter().map(|l| tr(l)).collect();
                ui.push_id(id, |ui| ui.add(Select::new(&mut idx, &labels).width(120.0)));
                if let Some(i) = idx.filter(|i| *i != dest.index()) {
                    let name = match &*dest {
                        ChDestination::Disk { path, .. } | ChDestination::File { path } => path.clone(),
                        ChDestination::S3 { url, .. } => url.rsplit('/').next().unwrap_or_default().to_string(),
                    };
                    *dest = match i {
                        0 => ChDestination::Disk { disk: "backups".into(), path: name },
                        1 => ChDestination::File { path: name },
                        _ => ChDestination::S3 {
                            url: format!("https://bucket.s3.amazonaws.com/backups/{name}"),
                            access_key: String::new(),
                            secret: String::new(),
                        },
                    };
                }
            });
            ui.vertical(|ui| match dest {
                ChDestination::Disk { disk, path } => {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            label(ui, tr("Disk"));
                            ui.add(Input::new(disk).width(120.0));
                        });
                        ui.vertical(|ui| {
                            label(ui, tr("File on the disk"));
                            ui.add(Input::new(path).width(ui.available_width()));
                        });
                    });
                }
                ChDestination::File { path } => {
                    label(ui, tr("Path on the server (under backups.allowed_path)"));
                    ui.add(Input::new(path).width(ui.available_width()));
                }
                ChDestination::S3 { url, access_key, secret } => {
                    label(ui, tr("S3 URL of the backup"));
                    ui.add(Input::new(url).width(ui.available_width()));
                    ui.horizontal(|ui| {
                        let half = (ui.available_width() - 8.0) / 2.0;
                        ui.add(
                            Input::new(access_key)
                                .placeholder(tr("Access key (empty for the server's own)"))
                                .width(half),
                        );
                        ui.add(Input::new(secret).placeholder(tr("Secret")).password().width(half));
                    });
                }
            });
        });
    }

    fn ch_backup_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        Self::ch_destination_ui(ui, &mut self.ch_backup.destination, "ch-backup-dest");
        ui.label(
            RichText::new(tr(
                "ClickHouse writes the backup itself; a disk or path must be allowed in the server's backups settings.",
            ))
            .size(12.0)
            .color(t.muted_foreground),
        );
        ui.add_space(8.0);
        label(ui, tr("Incremental on top of (an earlier backup at the same location, empty for a full one)"));
        ui.add(
            Input::new(&mut self.ch_backup.base_backup)
                .placeholder("shop_20240101-000000.zip")
                .width(ui.available_width()),
        );
        ui.add_space(6.0);
        ui.add(Checkbox::new(&mut self.ch_backup.structure_only).label(tr("Table definitions only, no data")));
    }

    fn ch_restore_ui(&mut self, ui: &mut Ui) {
        Self::ch_destination_ui(ui, &mut self.ch_restore.source, "ch-restore-src");
        ui.add_space(8.0);
        label(ui, tr("Restore as (empty keeps the original name)"));
        ui.add(
            Input::new(&mut self.ch_restore.rename_to)
                .placeholder(format!("{}_restored", self.scope.database))
                .width(280.0),
        );
        ui.add_space(6.0);
        let o = &mut self.ch_restore;
        ui.add(Checkbox::new(&mut o.allow_non_empty_tables).label(tr("Add rows to tables that already exist")));
        ui.add(Checkbox::new(&mut o.structure_only).label(tr("Table definitions only, no data")));
    }

    fn ms_backup_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        ui.label(
            RichText::new(
                tr("SQL Server writes the file itself, on the server's disk; a bare file name goes to its default backup folder."),
            )
            .size(12.0)
            .color(t.muted_foreground),
        );
        ui.add_space(8.0);
        label(ui, tr("Type"));
        let before = self.ms_backup.kind;
        let labels: Vec<&str> = MsBackupType::ALL.iter().map(|k| tr(k.label())).collect();
        let mut idx = MsBackupType::ALL.iter().position(|k| *k == self.ms_backup.kind);
        ui.add(Select::new(&mut idx, &labels).width(200.0));
        if let Some(i) = idx {
            self.ms_backup.kind = MsBackupType::ALL[i];
        }
        if self.ms_backup.kind != before {
            let ext = if self.ms_backup.kind == MsBackupType::Log { "trn" } else { "bak" };
            let p = Path::new(self.path.trim());
            self.path = p.with_extension(ext).to_string_lossy().into_owned();
        }
        ui.add_space(8.0);
        let o = &mut self.ms_backup;
        if o.kind != MsBackupType::Differential {
            ui.add(Checkbox::new(&mut o.copy_only).label(tr("Copy-only (does not affect scheduled backups)")));
        }
        ui.add(Checkbox::new(&mut o.compression).label(tr("Compress (not in Express edition)")));
        ui.add(Checkbox::new(&mut o.checksum).label(tr("Page checksums")));
        ui.add(Checkbox::new(&mut o.init).label(tr("Overwrite backup sets in the file")));
        ui.add(Checkbox::new(&mut o.verify).label(tr("Verify the backup afterwards")));
    }

    fn ms_restore_ui(&mut self, ui: &mut Ui) {
        Alert::new(tr("Overwrites the database"))
            .icon("!")
            .description(trf(
                "{database} is replaced by the backup; other sessions in it are disconnected. To restore a copy, type a new name below.",
                &[("database", &self.scope.database)],
            ))
            .destructive()
            .show(ui);
        ui.add_space(8.0);
        label(ui, tr("Restore as database"));
        ui.add(Input::new(&mut self.scope.database).width(280.0));
        ui.add_space(8.0);
        let o = &mut self.ms_restore;
        ui.add(Checkbox::new(&mut o.replace).label(tr("Replace an existing database (WITH REPLACE)")));
        ui.add(Checkbox::new(&mut o.close_connections).label(tr("Disconnect other sessions first")));
        ui.add(
            Checkbox::new(&mut o.relocate)
                .label(tr("Move files to the server's default folders, named after the database")),
        );
        ui.add(
            Checkbox::new(&mut o.recovery).label(tr("Bring online (off to restore differential or log backups next)")),
        );
    }

    fn duck_backup_ui(&mut self, ui: &mut Ui) {
        let before = self.duck_method;
        label(ui, tr("Method"));
        let labels: Vec<&str> = DuckMethod::ALL.iter().map(|m| tr(m.label())).collect();
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
                tr("The folder gets schema.sql, load.sql and one data file per table; IMPORT DATABASE reads it back.")
            }
            DuckMethod::DatabaseFile => tr("A new DuckDB file with the same schemas, tables, views and sequences."),
        };
        ui.label(RichText::new(note).size(12.0).color(t.muted_foreground));
    }

    fn duck_restore_ui(&mut self, ui: &mut Ui) {
        Alert::new(tr("Restore into an empty database"))
            .icon("!")
            .description(trf(
                "Objects of the backup are created in {database}; it fails if any of them already exist.",
                &[("database", &self.duck_database())],
            ))
            .show(ui);
    }

    fn sqlite_restore_ui(&mut self, ui: &mut Ui) {
        Alert::new(tr("Overwrites the database"))
            .icon("!")
            .description(trf(
                "Every table of {database} is replaced by the backup's content.",
                &[("database", &self.cfg.database)],
            ))
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
