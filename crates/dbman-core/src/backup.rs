//! Backup and restore options for every engine, and the command lines they
//! turn into. PostgreSQL and MySQL use the server's own dump tools
//! (`pg_dump` / `pg_restore` / `psql`, `mysqldump` / `mariadb-dump` /
//! `mysql`); SQLite copies pages with its online backup API. Running the jobs
//! lives in `dbman-drivers::backup`; this module only builds them, so it can
//! be tested without the tools installed.

use std::path::{Path, PathBuf};

use crate::{ConnectionConfig, DbKind, SslMode};

/// What a dump or restore contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Content {
    #[default]
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

impl Content {
    pub const ALL: [Content; 3] = [Content::SchemaAndData, Content::SchemaOnly, Content::DataOnly];

    pub const fn label(self) -> &'static str {
        match self {
            Content::SchemaAndData => "Schema and data",
            Content::SchemaOnly => "Schema only",
            Content::DataOnly => "Data only",
        }
    }
}

/// `pg_dump --format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PgFormat {
    /// A SQL script, restored with `psql`.
    Plain,
    /// Compressed archive for `pg_restore`; the usual choice.
    #[default]
    Custom,
    /// One file per table; the only format dumped in parallel.
    Directory,
    Tar,
}

impl PgFormat {
    pub const ALL: [PgFormat; 4] = [PgFormat::Custom, PgFormat::Plain, PgFormat::Directory, PgFormat::Tar];

    pub const fn label(self) -> &'static str {
        match self {
            PgFormat::Plain => "Plain SQL",
            PgFormat::Custom => "Custom archive",
            PgFormat::Directory => "Directory",
            PgFormat::Tar => "Tar",
        }
    }

    const fn flag(self) -> &'static str {
        match self {
            PgFormat::Plain => "p",
            PgFormat::Custom => "c",
            PgFormat::Directory => "d",
            PgFormat::Tar => "t",
        }
    }

    /// File extension, or `None` for a directory.
    pub const fn extension(self) -> Option<&'static str> {
        match self {
            PgFormat::Plain => Some("sql"),
            PgFormat::Custom => Some("dump"),
            PgFormat::Directory => None,
            PgFormat::Tar => Some("tar"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PgDumpOptions {
    pub format: PgFormat,
    pub content: Content,
    /// Only these schemas (`--schema`); empty for all.
    pub schemas: Vec<String>,
    /// Only these tables (`--table`, `schema.table`); empty for all.
    pub tables: Vec<String>,
    /// `--exclude-table-data`: keep the definition, skip the rows.
    pub exclude_data: Vec<String>,
    /// Emit `DROP` before every `CREATE` (plain format only; archives choose at restore).
    pub clean: bool,
    /// Include `CREATE DATABASE` (plain format only).
    pub create: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
    /// Rows as `INSERT` statements instead of `COPY` (plain format).
    pub inserts: bool,
    /// Parallel jobs (directory format).
    pub jobs: u32,
    /// Compression level 0–9; `None` keeps the tool's default.
    pub compress: Option<u8>,
}

impl Default for PgDumpOptions {
    fn default() -> Self {
        Self {
            format: PgFormat::Custom,
            content: Content::SchemaAndData,
            schemas: Vec::new(),
            tables: Vec::new(),
            exclude_data: Vec::new(),
            clean: false,
            create: false,
            no_owner: false,
            no_privileges: false,
            inserts: false,
            jobs: 1,
            compress: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PgRestoreOptions {
    pub content: Content,
    /// Drop objects before recreating them.
    pub clean: bool,
    /// Create the database first (connects to `postgres` and runs `CREATE DATABASE`).
    pub create: bool,
    pub no_owner: bool,
    pub no_privileges: bool,
    /// All or nothing: roll back everything on the first error.
    pub single_transaction: bool,
    /// Stop at the first error instead of reporting and going on.
    pub exit_on_error: bool,
    /// Parallel jobs (archives only, not with a single transaction).
    pub jobs: u32,
    pub schemas: Vec<String>,
    pub tables: Vec<String>,
}

impl Default for PgRestoreOptions {
    fn default() -> Self {
        Self {
            content: Content::SchemaAndData,
            clean: false,
            create: false,
            no_owner: true,
            no_privileges: false,
            single_transaction: false,
            exit_on_error: true,
            jobs: 1,
            schemas: Vec::new(),
            tables: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MyDumpOptions {
    pub content: Content,
    /// Only these tables; empty for the whole database.
    pub tables: Vec<String>,
    /// Consistent snapshot without locking InnoDB tables.
    pub single_transaction: bool,
    pub routines: bool,
    pub triggers: bool,
    pub events: bool,
    pub add_drop_table: bool,
    /// Include `CREATE DATABASE` and `USE`.
    pub create_database: bool,
    /// Binary columns as hex literals, safe for any client charset.
    pub hex_blob: bool,
    /// Many rows per `INSERT` (smaller and faster); off gives one row per line.
    pub extended_insert: bool,
}

impl Default for MyDumpOptions {
    fn default() -> Self {
        Self {
            content: Content::SchemaAndData,
            tables: Vec::new(),
            single_transaction: true,
            routines: true,
            triggers: true,
            events: true,
            add_drop_table: true,
            create_database: false,
            hex_blob: true,
            extended_insert: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MyRestoreOptions {
    /// Keep going after a failing statement.
    pub force: bool,
}

/// How to back up an SQLite file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqliteMethod {
    /// Page-by-page copy that other connections can keep using meanwhile.
    #[default]
    OnlineBackup,
    /// `VACUUM INTO`: a compacted copy without free pages.
    VacuumInto,
}

impl SqliteMethod {
    pub const ALL: [SqliteMethod; 2] = [SqliteMethod::OnlineBackup, SqliteMethod::VacuumInto];

    pub const fn label(self) -> &'static str {
        match self {
            SqliteMethod::OnlineBackup => "Online backup (page copy)",
            SqliteMethod::VacuumInto => "VACUUM INTO (compacted copy)",
        }
    }
}

/// The external programs DBMan drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    PgDump,
    PgRestore,
    Psql,
    MySqlDump,
    MariaDbDump,
    MySql,
    MariaDb,
}

impl Tool {
    pub const fn program(self) -> &'static str {
        match self {
            Tool::PgDump => "pg_dump",
            Tool::PgRestore => "pg_restore",
            Tool::Psql => "psql",
            Tool::MySqlDump => "mysqldump",
            Tool::MariaDbDump => "mariadb-dump",
            Tool::MySql => "mysql",
            Tool::MariaDb => "mariadb",
        }
    }

    const fn is_mariadb(self) -> bool {
        matches!(self, Tool::MariaDbDump | Tool::MariaDb)
    }
}

/// A command line to run, with secrets passed through the environment.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCommand {
    pub tool: Tool,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Feed this file to the tool's standard input.
    pub stdin: Option<PathBuf>,
    /// Write the tool's standard output to this file.
    pub stdout: Option<PathBuf>,
}

impl ToolCommand {
    fn new(tool: Tool) -> Self {
        Self { tool, args: Vec::new(), env: Vec::new(), stdin: None, stdout: None }
    }

    /// Points the command at another host and port (an SSH tunnel's local end).
    pub fn reroute(&mut self, host: &str, port: u16) {
        let mut next = None;
        for a in &mut self.args {
            match next.take() {
                Some("--host") => *a = host.to_string(),
                Some("--port") => *a = port.to_string(),
                _ => {}
            }
            if a == "--host" || a == "--port" {
                next = Some(if a == "--host" { "--host" } else { "--port" });
            } else if a.starts_with("--host=") {
                *a = format!("--host={host}");
            } else if a.starts_with("--port=") {
                *a = format!("--port={port}");
            }
        }
    }

    fn arg(&mut self, a: impl Into<String>) -> &mut Self {
        self.args.push(a.into());
        self
    }

    fn flag(&mut self, on: bool, a: &str) -> &mut Self {
        if on {
            self.args.push(a.to_string());
        }
        self
    }

    /// The command as a shell line, for the log. Passwords are never part of it.
    pub fn display(&self) -> String {
        let quote = |s: &str| {
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_=./:@,".contains(c)) {
                s.to_string()
            } else {
                format!("'{}'", s.replace('\'', r"'\''"))
            }
        };
        let mut line = self.tool.program().to_string();
        for a in &self.args {
            line.push(' ');
            line.push_str(&quote(a));
        }
        if let Some(p) = &self.stdin {
            line.push_str(&format!(" < {}", quote(&p.to_string_lossy())));
        }
        if let Some(p) = &self.stdout {
            line.push_str(&format!(" > {}", quote(&p.to_string_lossy())));
        }
        line
    }
}

fn pg_connection(cmd: &mut ToolCommand, cfg: &ConnectionConfig) {
    cmd.arg("--host").arg(&cfg.host).arg("--port").arg(cfg.port.to_string()).arg("--username").arg(&cfg.user);
    cmd.arg("--no-password");
    if !cfg.password.is_empty() {
        cmd.env.push(("PGPASSWORD".into(), cfg.password.clone()));
    }
    let mode = match cfg.ssl_mode {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
        SslMode::VerifyFull => "verify-full",
    };
    cmd.env.push(("PGSSLMODE".into(), mode.into()));
    cmd.env.push(("PGAPPNAME".into(), "DBMan".into()));
}

fn pg_content(cmd: &mut ToolCommand, content: Content) {
    match content {
        Content::SchemaAndData => {}
        Content::SchemaOnly => {
            cmd.arg("--schema-only");
        }
        Content::DataOnly => {
            cmd.arg("--data-only");
        }
    }
}

/// `pg_dump` of `database` into `target` (a file, or a directory for that format).
pub fn pg_dump(cfg: &ConnectionConfig, database: &str, o: &PgDumpOptions, target: &Path) -> ToolCommand {
    let mut c = ToolCommand::new(Tool::PgDump);
    pg_connection(&mut c, cfg);
    c.arg("--dbname").arg(database).arg("--format").arg(o.format.flag()).arg("--verbose");
    c.arg("--file").arg(target.to_string_lossy());
    pg_content(&mut c, o.content);
    for s in &o.schemas {
        c.arg("--schema").arg(s);
    }
    for t in &o.tables {
        c.arg("--table").arg(t);
    }
    for t in &o.exclude_data {
        c.arg("--exclude-table-data").arg(t);
    }
    let plain = o.format == PgFormat::Plain;
    c.flag(plain && o.clean, "--clean").flag(plain && o.clean, "--if-exists");
    c.flag(plain && o.create, "--create");
    c.flag(o.no_owner, "--no-owner").flag(o.no_privileges, "--no-privileges");
    c.flag(plain && o.inserts, "--column-inserts");
    if o.format == PgFormat::Directory && o.jobs > 1 {
        c.arg("--jobs").arg(o.jobs.to_string());
    }
    if let Some(level) = o.compress.filter(|_| o.format != PgFormat::Tar) {
        c.arg("--compress").arg(level.min(9).to_string());
    }
    c
}

/// Restores `source` into `database`: `psql` for a SQL script, `pg_restore` for archives.
pub fn pg_restore(
    cfg: &ConnectionConfig,
    database: &str,
    o: &PgRestoreOptions,
    source: &Path,
    format: PgFormat,
) -> ToolCommand {
    if format == PgFormat::Plain {
        let mut c = ToolCommand::new(Tool::Psql);
        pg_connection(&mut c, cfg);
        c.arg("--dbname").arg(database).arg("--file").arg(source.to_string_lossy());
        c.arg("--echo-errors").arg("--no-psqlrc");
        if o.exit_on_error {
            c.arg("--set").arg("ON_ERROR_STOP=1");
        }
        c.flag(o.single_transaction, "--single-transaction");
        return c;
    }
    let mut c = ToolCommand::new(Tool::PgRestore);
    pg_connection(&mut c, cfg);
    // With --create pg_restore connects to another database and creates the target from the archive.
    c.arg("--dbname").arg(if o.create { "postgres" } else { database }).arg("--verbose");
    pg_content(&mut c, o.content);
    c.flag(o.clean, "--clean").flag(o.clean, "--if-exists").flag(o.create, "--create");
    c.flag(o.no_owner, "--no-owner").flag(o.no_privileges, "--no-privileges");
    c.flag(o.single_transaction, "--single-transaction");
    c.flag(o.exit_on_error, "--exit-on-error");
    if o.jobs > 1 && !o.single_transaction {
        c.arg("--jobs").arg(o.jobs.to_string());
    }
    for s in &o.schemas {
        c.arg("--schema").arg(s);
    }
    for t in &o.tables {
        c.arg("--table").arg(t);
    }
    c.arg(source.to_string_lossy());
    c
}

fn my_connection(cmd: &mut ToolCommand, cfg: &ConnectionConfig) {
    cmd.arg(format!("--host={}", cfg.host)).arg(format!("--port={}", cfg.port)).arg(format!("--user={}", cfg.user));
    cmd.arg("--protocol=TCP");
    if !cfg.password.is_empty() {
        cmd.env.push(("MYSQL_PWD".into(), cfg.password.clone()));
    }
    // MySQL and MariaDB clients spell the TLS switches differently.
    let mariadb = cmd.tool.is_mariadb();
    match (cfg.ssl_mode, mariadb) {
        (SslMode::Prefer, _) => {}
        (SslMode::Disable, true) => {
            cmd.arg("--skip-ssl");
        }
        (SslMode::Disable, false) => {
            cmd.arg("--ssl-mode=DISABLED");
        }
        (SslMode::Require, true) => {
            cmd.arg("--ssl");
        }
        (SslMode::Require, false) => {
            cmd.arg("--ssl-mode=REQUIRED");
        }
        (SslMode::VerifyFull, true) => {
            cmd.arg("--ssl").arg("--ssl-verify-server-cert");
        }
        (SslMode::VerifyFull, false) => {
            cmd.arg("--ssl-mode=VERIFY_IDENTITY");
        }
    }
}

/// `mysqldump` (or `mariadb-dump`) of `database` into the file `target`.
/// `against_mariadb` tells whether the server is MariaDB: MySQL 8's mysqldump
/// then needs `--column-statistics=0`.
pub fn my_dump(
    cfg: &ConnectionConfig,
    tool: Tool,
    against_mariadb: bool,
    database: &str,
    o: &MyDumpOptions,
    target: &Path,
) -> ToolCommand {
    let mut c = ToolCommand::new(tool);
    my_connection(&mut c, cfg);
    c.arg("--verbose").arg("--default-character-set=utf8mb4");
    if tool == Tool::MySqlDump && against_mariadb {
        c.arg("--column-statistics=0");
    }
    match o.content {
        Content::SchemaAndData => {}
        Content::SchemaOnly => {
            c.arg("--no-data");
        }
        Content::DataOnly => {
            c.arg("--no-create-info");
        }
    }
    let with_schema = o.content != Content::DataOnly;
    c.flag(o.single_transaction, "--single-transaction");
    c.flag(with_schema && o.routines, "--routines");
    c.flag(with_schema && o.events, "--events");
    c.arg(if o.triggers && with_schema { "--triggers" } else { "--skip-triggers" });
    c.arg(if o.add_drop_table && with_schema { "--add-drop-table" } else { "--skip-add-drop-table" });
    c.flag(o.hex_blob, "--hex-blob");
    if !o.extended_insert {
        c.arg("--skip-extended-insert");
    }
    c.arg(format!("--result-file={}", target.to_string_lossy()));
    if o.create_database && o.tables.is_empty() {
        c.arg("--databases").arg(database);
    } else {
        c.arg(database);
        for t in &o.tables {
            c.arg(t);
        }
    }
    c
}

/// Runs the SQL script `source` in `database` with the `mysql` client.
pub fn my_restore(
    cfg: &ConnectionConfig,
    tool: Tool,
    database: &str,
    o: &MyRestoreOptions,
    source: &Path,
) -> ToolCommand {
    let mut c = ToolCommand::new(tool);
    my_connection(&mut c, cfg);
    c.arg("--default-character-set=utf8mb4").arg("--verbose");
    c.flag(o.force, "--force");
    if !database.is_empty() {
        c.arg(format!("--database={database}"));
    }
    c.stdin = Some(source.to_path_buf());
    c
}

/// Which PostgreSQL dump `path` holds, from its first bytes (or a directory).
pub fn detect_pg_format(path: &Path) -> std::io::Result<PgFormat> {
    if path.is_dir() {
        return Ok(PgFormat::Directory);
    }
    use std::io::Read;
    let mut head = [0u8; 512];
    let mut f = std::fs::File::open(path)?;
    let mut n = 0;
    while n < head.len() {
        let r = f.read(&mut head[n..])?;
        if r == 0 {
            break;
        }
        n += r;
    }
    let head = &head[..n];
    Ok(if head.starts_with(b"PGDMP") {
        PgFormat::Custom
    } else if head.len() >= 262 && &head[257..262] == b"ustar" {
        PgFormat::Tar
    } else {
        PgFormat::Plain
    })
}

/// How to back up a DuckDB database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuckMethod {
    /// `EXPORT DATABASE` to a folder: schema.sql, load.sql and a Parquet file per table.
    #[default]
    Parquet,
    /// `EXPORT DATABASE` with CSV files, readable by anything.
    Csv,
    /// A new database file: `ATTACH` it and `COPY FROM DATABASE`.
    DatabaseFile,
}

impl DuckMethod {
    pub const ALL: [DuckMethod; 3] = [DuckMethod::Parquet, DuckMethod::Csv, DuckMethod::DatabaseFile];

    pub const fn label(self) -> &'static str {
        match self {
            DuckMethod::Parquet => "Export folder (Parquet)",
            DuckMethod::Csv => "Export folder (CSV)",
            DuckMethod::DatabaseFile => "Database file copy",
        }
    }

    pub const fn writes_folder(self) -> bool {
        !matches!(self, DuckMethod::DatabaseFile)
    }
}

/// Alias of the backup or restore file while it is attached.
const DUCK_ALIAS: &str = "dbman_backup_file";

/// Statements that back up the DuckDB catalog `db` to `target`, run on a
/// session of that database. The last statement is cleanup that also runs
/// after a failure.
pub fn duck_backup(db: &str, method: DuckMethod, target: &Path) -> Vec<String> {
    let d = crate::sql::Dialect(DbKind::DuckDb);
    let to = d.string(&target.to_string_lossy());
    match method {
        DuckMethod::Parquet | DuckMethod::Csv => {
            let options = if method == DuckMethod::Parquet {
                "FORMAT parquet, COMPRESSION zstd"
            } else {
                "FORMAT csv, HEADER true"
            };
            vec![format!("USE {}", d.ident(db)), format!("EXPORT DATABASE {to} ({options})")]
        }
        DuckMethod::DatabaseFile => vec![
            format!("ATTACH {to} AS {DUCK_ALIAS}"),
            format!("COPY FROM DATABASE {} TO {DUCK_ALIAS}", d.ident(db)),
            format!("DETACH DATABASE IF EXISTS {DUCK_ALIAS}"),
        ],
    }
}

/// Statements that load the backup `source` (an export folder or a database
/// file) into the DuckDB catalog `db`. The last statement is cleanup.
pub fn duck_restore(db: &str, source: &Path) -> Vec<String> {
    let d = crate::sql::Dialect(DbKind::DuckDb);
    let from = d.string(&source.to_string_lossy());
    if source.is_dir() {
        vec![format!("USE {}", d.ident(db)), format!("IMPORT DATABASE {from}")]
    } else {
        vec![
            format!("ATTACH {from} AS {DUCK_ALIAS} (READ_ONLY)"),
            format!("COPY FROM DATABASE {DUCK_ALIAS} TO {}", d.ident(db)),
            format!("DETACH DATABASE IF EXISTS {DUCK_ALIAS}"),
        ]
    }
}

/// What a SQL Server backup contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MsBackupType {
    #[default]
    Full,
    /// Changes since the last full backup.
    Differential,
    /// The transaction log (full or bulk-logged recovery model).
    Log,
}

impl MsBackupType {
    pub const ALL: [MsBackupType; 3] = [MsBackupType::Full, MsBackupType::Differential, MsBackupType::Log];

    pub const fn label(self) -> &'static str {
        match self {
            MsBackupType::Full => "Full",
            MsBackupType::Differential => "Differential",
            MsBackupType::Log => "Transaction log",
        }
    }
}

/// `BACKUP DATABASE` options.
#[derive(Debug, Clone, PartialEq)]
pub struct MsBackupOptions {
    pub kind: MsBackupType,
    /// Leaves the backup chain alone, so scheduled backups are not affected.
    pub copy_only: bool,
    /// Not available in SQL Server Express.
    pub compression: bool,
    pub checksum: bool,
    /// Overwrite the backup sets already in the file instead of appending.
    pub init: bool,
    /// Run `RESTORE VERIFYONLY` afterwards.
    pub verify: bool,
}

impl Default for MsBackupOptions {
    fn default() -> Self {
        Self { kind: MsBackupType::Full, copy_only: true, compression: false, checksum: true, init: true, verify: true }
    }
}

/// `RESTORE DATABASE` options.
#[derive(Debug, Clone, PartialEq)]
pub struct MsRestoreOptions {
    /// Overwrite an existing database (`WITH REPLACE`).
    pub replace: bool,
    /// Disconnect other sessions first (`SET SINGLE_USER WITH ROLLBACK IMMEDIATE`).
    pub close_connections: bool,
    /// Put the data and log files in the server's default folders, named
    /// after the target database (needed to restore a copy next to the original).
    pub relocate: bool,
    /// Bring the database online; off to apply differential or log backups next.
    pub recovery: bool,
}

impl Default for MsRestoreOptions {
    fn default() -> Self {
        Self { replace: true, close_connections: true, relocate: false, recovery: true }
    }
}

/// `BACKUP` statements for database `db` to the file `path` on the server.
pub fn ms_backup(db: &str, o: &MsBackupOptions, path: &str) -> Vec<String> {
    let d = crate::sql::Dialect(DbKind::MsSql);
    let what = if o.kind == MsBackupType::Log { "LOG" } else { "DATABASE" };
    let mut with = Vec::new();
    if o.kind == MsBackupType::Differential {
        with.push("DIFFERENTIAL");
    } else if o.copy_only {
        with.push("COPY_ONLY");
    }
    with.push(if o.init { "INIT" } else { "NOINIT" });
    if o.compression {
        with.push("COMPRESSION");
    }
    if o.checksum {
        with.push("CHECKSUM");
    }
    with.push("STATS = 5");
    let file = d.string(path);
    let mut out = vec![format!("BACKUP {what} {} TO DISK = {file} WITH {}", d.ident(db), with.join(", "))];
    if o.verify {
        let check = if o.checksum { " WITH CHECKSUM" } else { "" };
        // The last backup set in the file is the one just written.
        out.push(format!(
            "DECLARE @pos int = (SELECT TOP 1 position FROM msdb.dbo.backupset WHERE database_name = {} ORDER BY backup_set_id DESC); RESTORE VERIFYONLY FROM DISK = {file} WITH FILE = @pos{}",
            d.string(db),
            check.replace(" WITH", ",")
        ));
    }
    out
}

/// `RESTORE` statements for database `db` from the file `path` on the server.
/// `moves` relocates files (`logical name`, `new physical path`). The second
/// value is cleanup to run even if the restore fails.
pub fn ms_restore(
    db: &str,
    o: &MsRestoreOptions,
    path: &str,
    moves: &[(String, String)],
) -> (Vec<String>, Option<String>) {
    let d = crate::sql::Dialect(DbKind::MsSql);
    let exists = format!("IF DB_ID({}) IS NOT NULL", d.string(db));
    let mut out = Vec::new();
    if o.close_connections {
        out.push(format!("{exists} ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE", d.ident(db)));
    }
    let mut with = Vec::new();
    if o.replace {
        with.push("REPLACE".to_string());
    }
    with.push(if o.recovery { "RECOVERY" } else { "NORECOVERY" }.to_string());
    for (logical, physical) in moves {
        with.push(format!("MOVE {} TO {}", d.string(logical), d.string(physical)));
    }
    with.push("STATS = 5".into());
    out.push(format!("RESTORE DATABASE {} FROM DISK = {} WITH {}", d.ident(db), d.string(path), with.join(", ")));
    // A database left restoring (NORECOVERY) cannot be altered.
    let cleanup = (o.close_connections && o.recovery).then(|| {
        format!(
            "{exists} AND DATABASEPROPERTYEX({}, 'Status') = 'ONLINE' ALTER DATABASE {} SET MULTI_USER",
            d.string(db),
            d.ident(db)
        )
    });
    (out, cleanup)
}

/// Where ClickHouse writes or reads a backup (on the server's side).
#[derive(Debug, Clone, PartialEq)]
pub enum ChDestination {
    /// A disk from the server's `storage_configuration`, allowed in `backups.allowed_disk`.
    Disk {
        disk: String,
        path: String,
    },
    /// A path under the server's `backups.allowed_path`.
    File {
        path: String,
    },
    S3 {
        url: String,
        access_key: String,
        secret: String,
    },
}

impl Default for ChDestination {
    fn default() -> Self {
        ChDestination::Disk { disk: "backups".into(), path: String::new() }
    }
}

impl ChDestination {
    pub const LABELS: [&'static str; 3] = ["Disk", "File", "S3"];

    pub fn index(&self) -> usize {
        match self {
            ChDestination::Disk { .. } => 0,
            ChDestination::File { .. } => 1,
            ChDestination::S3 { .. } => 2,
        }
    }

    /// The same destination with `name` as the file (or the last part of the S3 URL).
    pub fn with_name(&self, name: &str) -> Self {
        let swap = |p: &str| match p.rsplit_once('/') {
            Some((dir, _)) => format!("{dir}/{name}"),
            None => name.to_string(),
        };
        match self {
            ChDestination::Disk { disk, path } => ChDestination::Disk { disk: disk.clone(), path: swap(path) },
            ChDestination::File { path } => ChDestination::File { path: swap(path) },
            ChDestination::S3 { url, access_key, secret } => {
                ChDestination::S3 { url: swap(url), access_key: access_key.clone(), secret: secret.clone() }
            }
        }
    }

    /// `Disk('backups', 'x.zip')`; with `mask`, the S3 secret is hidden.
    pub fn sql(&self, mask: bool) -> String {
        let d = crate::sql::Dialect(DbKind::ClickHouse);
        match self {
            ChDestination::Disk { disk, path } => format!("Disk({}, {})", d.string(disk), d.string(path)),
            ChDestination::File { path } => format!("File({})", d.string(path)),
            ChDestination::S3 { url, access_key, .. } if access_key.is_empty() => format!("S3({})", d.string(url)),
            ChDestination::S3 { url, access_key, secret } => format!(
                "S3({}, {}, {})",
                d.string(url),
                d.string(access_key),
                if mask { "'******'".to_string() } else { d.string(secret) }
            ),
        }
    }
}

/// `BACKUP` options for ClickHouse.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChBackupOptions {
    pub destination: ChDestination,
    /// An earlier backup (file name at the same destination) to make an incremental one on.
    pub base_backup: String,
    /// Only the table definitions.
    pub structure_only: bool,
}

/// `RESTORE` options for ClickHouse.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChRestoreOptions {
    pub source: ChDestination,
    /// Restore under another name (`AS`); empty keeps the original one.
    pub rename_to: String,
    /// Add the rows to tables that already exist instead of failing.
    pub allow_non_empty_tables: bool,
    pub structure_only: bool,
}

/// What a ClickHouse backup covers: `` DATABASE `db` `` or `` TABLE `db`.`t` ``.
fn ch_target(database: &str, table: Option<&str>) -> String {
    let d = crate::sql::Dialect(DbKind::ClickHouse);
    match table {
        Some(t) => format!("TABLE {}.{}", d.ident(database), d.ident(t)),
        None => format!("DATABASE {}", d.ident(database)),
    }
}

/// `BACKUP … TO … ASYNC`; with `mask`, for showing (the S3 secret is hidden).
pub fn ch_backup(database: &str, table: Option<&str>, o: &ChBackupOptions, mask: bool) -> String {
    let mut settings = Vec::new();
    if !o.base_backup.trim().is_empty() {
        settings.push(format!("base_backup = {}", o.destination.with_name(o.base_backup.trim()).sql(mask)));
    }
    if o.structure_only {
        settings.push("structure_only = true".into());
    }
    let settings = if settings.is_empty() { String::new() } else { format!(" SETTINGS {}", settings.join(", ")) };
    format!("BACKUP {} TO {}{settings} ASYNC", ch_target(database, table), o.destination.sql(mask))
}

/// `RESTORE … FROM … ASYNC`.
pub fn ch_restore(database: &str, table: Option<&str>, o: &ChRestoreOptions, mask: bool) -> String {
    let d = crate::sql::Dialect(DbKind::ClickHouse);
    let rename = o.rename_to.trim();
    let alias = match (rename.is_empty(), table) {
        (true, _) => String::new(),
        (false, Some(_)) => format!(" AS {}.{}", d.ident(database), d.ident(rename)),
        (false, None) => format!(" AS {}", d.ident(rename)),
    };
    let mut settings = Vec::new();
    if o.allow_non_empty_tables {
        settings.push("allow_non_empty_tables = true");
    }
    if o.structure_only {
        settings.push("structure_only = true");
    }
    let settings = if settings.is_empty() { String::new() } else { format!(" SETTINGS {}", settings.join(", ")) };
    format!("RESTORE {}{alias} FROM {}{settings} ASYNC", ch_target(database, table), o.source.sql(mask))
}

/// The default file name for a backup: `<database>_<date>-<time>.<ext>`.
pub fn default_file_name(kind: DbKind, database: &str, stamp: &str, pg_format: PgFormat) -> String {
    let base: String = database
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(database)
        .trim_end_matches(".duckdb")
        .trim_end_matches(".sqlite3")
        .trim_end_matches(".sqlite")
        .trim_end_matches(".db")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let base = if base.is_empty() { "backup".to_string() } else { base };
    match kind {
        DbKind::Postgres => match pg_format.extension() {
            Some(ext) => format!("{base}_{stamp}.{ext}"),
            None => format!("{base}_{stamp}"),
        },
        DbKind::MySql => format!("{base}_{stamp}.sql"),
        DbKind::Sqlite => format!("{base}_{stamp}.db"),
        DbKind::DuckDb => format!("{base}_{stamp}"),
        DbKind::MsSql => format!("{base}_{stamp}.bak"),
        DbKind::ClickHouse => format!("{base}_{stamp}.zip"),
        DbKind::Redis => format!("{base}_{stamp}.rdb"),
        DbKind::MongoDb => format!("{base}_{stamp}.archive.gz"),
    }
}

/// [`default_file_name`] for a DuckDB backup made with `method`.
pub fn duck_file_name(database: &str, stamp: &str, method: DuckMethod) -> String {
    let name = default_file_name(DbKind::DuckDb, database, stamp, PgFormat::default());
    if method.writes_folder() { name } else { format!("{name}.duckdb") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reroute_moves_host_and_port() {
        let mut cmd = pg_dump(&pg(), "shop", &PgDumpOptions::default(), Path::new("/tmp/x.dump"));
        cmd.reroute("127.0.0.1", 40000);
        let line = cmd.display();
        assert!(line.contains("--host 127.0.0.1 --port 40000"), "{line}");
        let mut my = ConnectionConfig::new(DbKind::MySql);
        my.host = "db".into();
        let mut cmd = my_dump(&my, Tool::MySqlDump, false, "shop", &MyDumpOptions::default(), Path::new("/tmp/x.sql"));
        cmd.reroute("127.0.0.1", 40001);
        assert!(cmd.args.contains(&"--host=127.0.0.1".to_string()) && cmd.args.contains(&"--port=40001".to_string()));
    }

    fn pg() -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::Postgres);
        c.host = "db.local".into();
        c.port = 5433;
        c.user = "admin".into();
        c.password = "s3cret".into();
        c
    }

    #[test]
    fn pg_dump_command() {
        let o = PgDumpOptions {
            schemas: vec!["public".into()],
            exclude_data: vec!["public.logs".into()],
            no_owner: true,
            compress: Some(6),
            ..Default::default()
        };
        let c = pg_dump(&pg(), "shop", &o, Path::new("/tmp/shop.dump"));
        assert_eq!(
            c.display(),
            "pg_dump --host db.local --port 5433 --username admin --no-password --dbname shop --format c --verbose \
             --file /tmp/shop.dump --schema public --exclude-table-data public.logs --no-owner --compress 6"
        );
        assert!(c.env.contains(&("PGPASSWORD".into(), "s3cret".into())));
        assert!(!c.display().contains("s3cret"));
    }

    #[test]
    fn pg_plain_only_flags() {
        let o =
            PgDumpOptions { format: PgFormat::Plain, clean: true, create: true, inserts: true, ..Default::default() };
        let c = pg_dump(&pg(), "shop", &o, Path::new("a b.sql"));
        assert!(c.args.iter().any(|a| a == "--clean"));
        assert!(c.args.iter().any(|a| a == "--column-inserts"));
        assert!(c.display().contains("'a b.sql'"));
        let o = PgDumpOptions { format: PgFormat::Custom, clean: true, ..o };
        assert!(!pg_dump(&pg(), "shop", &o, Path::new("x")).args.iter().any(|a| a == "--clean"));
        let o = PgDumpOptions { format: PgFormat::Directory, jobs: 4, ..Default::default() };
        assert!(pg_dump(&pg(), "shop", &o, Path::new("x")).display().contains("--jobs 4"));
    }

    #[test]
    fn pg_restore_commands() {
        let o = PgRestoreOptions { clean: true, jobs: 3, ..Default::default() };
        let c = pg_restore(&pg(), "shop", &o, Path::new("/b/shop.dump"), PgFormat::Custom);
        assert_eq!(c.tool, Tool::PgRestore);
        assert!(c.display().ends_with("--clean --if-exists --no-owner --exit-on-error --jobs 3 /b/shop.dump"));
        let o = PgRestoreOptions { create: true, ..Default::default() };
        assert!(pg_restore(&pg(), "shop", &o, Path::new("x"), PgFormat::Tar).display().contains("--dbname postgres"));
        let c = pg_restore(&pg(), "shop", &PgRestoreOptions::default(), Path::new("/b/s.sql"), PgFormat::Plain);
        assert_eq!(c.tool, Tool::Psql);
        assert!(c.display().contains("--set ON_ERROR_STOP=1"));
    }

    #[test]
    fn mysql_commands() {
        let mut cfg = ConnectionConfig::new(DbKind::MySql);
        cfg.host = "127.0.0.1".into();
        cfg.port = 3306;
        cfg.user = "root".into();
        cfg.password = "pw".into();
        let o = MyDumpOptions { tables: vec!["orders".into()], ..Default::default() };
        let c = my_dump(&cfg, Tool::MySqlDump, true, "shop", &o, Path::new("/b/shop.sql"));
        assert_eq!(
            c.display(),
            "mysqldump --host=127.0.0.1 --port=3306 --user=root --protocol=TCP --verbose \
             --default-character-set=utf8mb4 --column-statistics=0 --single-transaction --routines --events \
             --triggers --add-drop-table --hex-blob --result-file=/b/shop.sql shop orders"
        );
        assert_eq!(c.env, vec![("MYSQL_PWD".into(), "pw".into())]);
        let o = MyDumpOptions { content: Content::DataOnly, create_database: true, ..Default::default() };
        let c = my_dump(&cfg, Tool::MariaDbDump, true, "shop", &o, Path::new("x"));
        assert!(c.display().ends_with("--databases shop"));
        assert!(c.args.iter().any(|a| a == "--skip-triggers"));
        assert!(!c.args.iter().any(|a| a == "--routines" || a.starts_with("--column-statistics")));
        let c = my_restore(&cfg, Tool::MySql, "shop", &MyRestoreOptions { force: true }, Path::new("/b/shop.sql"));
        assert!(c.display().ends_with("--force --database=shop < /b/shop.sql"));
        cfg.ssl_mode = SslMode::Require;
        assert!(
            my_restore(&cfg, Tool::MariaDb, "", &MyRestoreOptions::default(), Path::new("x"))
                .args
                .contains(&"--ssl".into())
        );
    }

    #[test]
    fn detects_dump_formats() {
        let dir = std::env::temp_dir().join(format!("dbman-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let custom = dir.join("a.dump");
        std::fs::write(&custom, b"PGDMP\x01\x0e\x00").unwrap();
        let plain = dir.join("a.sql");
        std::fs::write(&plain, b"--\n-- PostgreSQL database dump\n").unwrap();
        let mut tar = vec![0u8; 600];
        tar[257..262].copy_from_slice(b"ustar");
        let tar_path = dir.join("a.tar");
        std::fs::write(&tar_path, tar).unwrap();
        assert_eq!(detect_pg_format(&custom).unwrap(), PgFormat::Custom);
        assert_eq!(detect_pg_format(&plain).unwrap(), PgFormat::Plain);
        assert_eq!(detect_pg_format(&tar_path).unwrap(), PgFormat::Tar);
        assert_eq!(detect_pg_format(&dir).unwrap(), PgFormat::Directory);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_names() {
        assert_eq!(
            default_file_name(DbKind::Postgres, "shop", "20261015-1200", PgFormat::Custom),
            "shop_20261015-1200.dump"
        );
        assert_eq!(default_file_name(DbKind::Postgres, "shop", "s", PgFormat::Directory), "shop_s");
        assert_eq!(default_file_name(DbKind::Sqlite, "/home/me/my app.sqlite3", "s", PgFormat::Custom), "my_app_s.db");
        assert_eq!(default_file_name(DbKind::MySql, "", "s", PgFormat::Custom), "backup_s.sql");
    }

    #[test]
    fn duck_statements() {
        let b = duck_backup("shop", DuckMethod::Parquet, Path::new("/tmp/it's"));
        assert_eq!(b, ["USE \"shop\"", "EXPORT DATABASE '/tmp/it''s' (FORMAT parquet, COMPRESSION zstd)"]);
        let b = duck_backup("shop", DuckMethod::DatabaseFile, Path::new("/tmp/b.duckdb"));
        assert_eq!(b[1], "COPY FROM DATABASE \"shop\" TO dbman_backup_file");
        assert_eq!(duck_file_name("/data/shop.duckdb", "s", DuckMethod::DatabaseFile), "shop_s.duckdb");
        assert_eq!(duck_file_name("/data/shop.duckdb", "s", DuckMethod::Csv), "shop_s");
    }

    #[test]
    fn ms_statements() {
        let b = ms_backup("shop", &MsBackupOptions::default(), "shop.bak");
        assert_eq!(b[0], "BACKUP DATABASE [shop] TO DISK = N'shop.bak' WITH COPY_ONLY, INIT, CHECKSUM, STATS = 5");
        assert!(b[1].contains("RESTORE VERIFYONLY FROM DISK = N'shop.bak' WITH FILE = @pos, CHECKSUM"), "{}", b[1]);
        let o = MsBackupOptions { kind: MsBackupType::Differential, verify: false, ..Default::default() };
        assert_eq!(
            ms_backup("shop", &o, "d.bak"),
            ["BACKUP DATABASE [shop] TO DISK = N'd.bak' WITH DIFFERENTIAL, INIT, CHECKSUM, STATS = 5"]
        );
        let o = MsBackupOptions { kind: MsBackupType::Log, verify: false, ..Default::default() };
        assert!(ms_backup("shop", &o, "l.trn")[0].starts_with("BACKUP LOG [shop]"));

        let moves = [("shop".to_string(), "/var/opt/mssql/data/copy.mdf".to_string())];
        let (r, cleanup) = ms_restore("copy", &MsRestoreOptions::default(), "shop.bak", &moves);
        assert_eq!(r[0], "IF DB_ID(N'copy') IS NOT NULL ALTER DATABASE [copy] SET SINGLE_USER WITH ROLLBACK IMMEDIATE");
        assert_eq!(
            r[1],
            "RESTORE DATABASE [copy] FROM DISK = N'shop.bak' WITH REPLACE, RECOVERY, MOVE N'shop' TO N'/var/opt/mssql/data/copy.mdf', STATS = 5"
        );
        assert!(cleanup.unwrap().ends_with("ALTER DATABASE [copy] SET MULTI_USER"));
    }

    #[test]
    fn ch_statements() {
        let o = ChBackupOptions {
            destination: ChDestination::Disk { disk: "backups".into(), path: "shop_2.zip".into() },
            base_backup: "shop_1.zip".into(),
            structure_only: false,
        };
        assert_eq!(
            ch_backup("shop", None, &o, false),
            "BACKUP DATABASE `shop` TO Disk('backups', 'shop_2.zip') SETTINGS base_backup = Disk('backups', 'shop_1.zip') ASYNC"
        );
        let s3 = ChDestination::S3 {
            url: "https://b.s3.amazonaws.com/x/a.zip".into(),
            access_key: "K".into(),
            secret: "S".into(),
        };
        let o = ChBackupOptions { destination: s3.clone(), ..Default::default() };
        assert_eq!(
            ch_backup("shop", Some("orders"), &o, true),
            "BACKUP TABLE `shop`.`orders` TO S3('https://b.s3.amazonaws.com/x/a.zip', 'K', '******') ASYNC"
        );
        assert_eq!(s3.with_name("b.zip").sql(false), "S3('https://b.s3.amazonaws.com/x/b.zip', 'K', 'S')");
        let r = ChRestoreOptions {
            source: ChDestination::File { path: "/backups/a.zip".into() },
            rename_to: "shop_copy".into(),
            allow_non_empty_tables: true,
            structure_only: false,
        };
        assert_eq!(
            ch_restore("shop", None, &r, false),
            "RESTORE DATABASE `shop` AS `shop_copy` FROM File('/backups/a.zip') SETTINGS allow_non_empty_tables = true ASYNC"
        );
    }
}
