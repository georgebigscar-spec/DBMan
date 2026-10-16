//! Runs backup and restore jobs: the engines' dump tools as child processes,
//! and SQLite's online backup API on a blocking thread. Jobs report log lines
//! and progress through a callback and stop when their cancel flag is set
//! (child processes are killed, SQLite copies stop between steps).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dbman_core::backup::{SqliteMethod, Tool, ToolCommand};
use dbman_core::{DbError, DbResult};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// What a running job tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Log(String),
    /// Bytes written to the backup so far.
    Bytes(u64),
    /// Share of the work done, 0.0–1.0, when it can be known.
    Fraction(f32),
}

pub type Report = Arc<dyn Fn(Progress) + Send + Sync>;

/// Set to stop a job; it then ends with a "cancelled" error.
pub type Cancel = Arc<AtomicBool>;

fn cancelled() -> DbError {
    DbError::new("cancelled")
}

/// Where to look for `tool`: `extra_dir` first, then `PATH`, then the places
/// installers usually put it (Homebrew, Postgres.app, Program Files, Debian's
/// versioned directories; newest version first).
pub fn find_tool(tool: Tool, extra_dir: Option<&Path>) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{}.exe", tool.program()) } else { tool.program().to_string() };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = extra_dir.filter(|d| !d.as_os_str().is_empty()) {
        dirs.push(d.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(known_dirs(tool));
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Subdirectories of `parent` whose names start with `prefix`, newest-looking first.
fn versioned(parent: &str, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(parent) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect();
    // Compare version numbers numerically ("16" after "9").
    names.sort_by_key(|n| {
        n[prefix.len()..].split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse::<u32>().ok()).collect::<Vec<_>>()
    });
    names.into_iter().rev().map(|n| Path::new(parent).join(n).join(suffix)).collect()
}

fn known_dirs(tool: Tool) -> Vec<PathBuf> {
    let pg = matches!(tool, Tool::PgDump | Tool::PgRestore | Tool::Psql);
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        if pg {
            dirs.push(PathBuf::from("/Applications/Postgres.app/Contents/Versions/latest/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/opt/libpq/bin"));
            dirs.push(PathBuf::from("/usr/local/opt/libpq/bin"));
            dirs.extend(versioned("/opt/homebrew/opt", "postgresql@", "bin"));
        } else {
            dirs.push(PathBuf::from("/opt/homebrew/opt/mysql-client/bin"));
            dirs.push(PathBuf::from("/usr/local/opt/mysql-client/bin"));
            dirs.push(PathBuf::from("/usr/local/mysql/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/opt/mariadb/bin"));
        }
    } else if cfg!(windows) {
        if pg {
            dirs.extend(versioned(r"C:\Program Files\PostgreSQL", "", "bin"));
        } else {
            dirs.extend(versioned(r"C:\Program Files\MySQL", "MySQL Server ", "bin"));
            dirs.extend(versioned(r"C:\Program Files", "MariaDB ", "bin"));
        }
    } else if pg {
        dirs.extend(versioned("/usr/lib/postgresql", "", "bin"));
    }
    dirs
}

/// Size of a file, or of every file under a directory.
fn size_of(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {
            std::fs::read_dir(path).map(|rd| rd.filter_map(|e| e.ok()).map(|e| size_of(&e.path())).sum()).unwrap_or(0)
        }
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Runs `cmd` with `program`, streaming its output to the log. `watch` is the
/// file or directory being written, reported as `Bytes`. A file given as
/// standard input is fed by DBMan, which reports how much of it was read.
pub async fn run_tool(
    program: &Path,
    cmd: &ToolCommand,
    watch: Option<&Path>,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    report(Progress::Log(format!("$ {}", cmd.display())));
    let mut child = tokio::process::Command::new(program);
    child.args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v))).kill_on_drop(true);
    child.stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    child.stderr(Stdio::piped());
    match &cmd.stdout {
        Some(p) => {
            let f = std::fs::File::create(p).map_err(|e| DbError::new(format!("{}: {e}", p.display())))?;
            child.stdout(Stdio::from(f));
        }
        None => {
            child.stdout(Stdio::piped());
        }
    }
    #[cfg(windows)]
    {
        // No console window flashing up for every dump.
        child.creation_flags(0x0800_0000);
    }
    let mut child = child.spawn().map_err(|e| DbError::new(format!("could not start {}: {e}", program.display())))?;

    // Errors are on stderr; keep the tail for the failure message.
    let tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let pump = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>, keep: bool| {
        let report = report.clone();
        let tail = tail.clone();
        async move {
            let Some(s) = stream else { return };
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if keep && let Ok(mut t) = tail.lock() {
                    t.push(line.clone());
                    if t.len() > 20 {
                        t.remove(0);
                    }
                }
                report(Progress::Log(line));
            }
        }
    };
    let stderr = pump(child.stderr.take().map(|s| Box::new(s) as _), true);
    let stdout = pump(child.stdout.take().map(|s| Box::new(s) as _), false);

    let feed = {
        let input = cmd.stdin.clone();
        let stdin = child.stdin.take();
        let report = report.clone();
        async move {
            let (Some(path), Some(mut stdin)) = (input, stdin) else { return Ok(()) };
            let mut file =
                tokio::fs::File::open(&path).await.map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
            let total = file.metadata().await.map(|m| m.len()).unwrap_or(0).max(1);
            let mut buf = vec![0u8; 256 * 1024];
            let mut done = 0u64;
            loop {
                let n = file.read(&mut buf).await.map_err(DbError::from_display)?;
                if n == 0 {
                    break;
                }
                // The tool may exit early on an error; its stderr says why.
                if stdin.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                done += n as u64;
                report(Progress::Fraction(done as f32 / total as f32));
            }
            drop(stdin);
            Ok::<(), DbError>(())
        }
    };

    let watcher = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(w) = watch {
                report(Progress::Bytes(size_of(w)));
            }
        }
    };

    let work = async {
        let (fed, _, _, status) = tokio::join!(feed, stderr, stdout, child.wait());
        fed?;
        status.map_err(DbError::from_display)
    };
    let status = tokio::select! {
        r = work => r?,
        _ = watcher => {
            let _ = child.kill().await;
            return Err(cancelled());
        }
    };
    if let Some(w) = watch {
        report(Progress::Bytes(size_of(w)));
    }
    if status.success() {
        return Ok(());
    }
    let tail = tail.lock().map(|t| t.join("\n")).unwrap_or_default();
    let code = status.code().map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
    Err(DbError::new(if tail.is_empty() {
        format!("{} exited with {code}", cmd.tool.program())
    } else {
        format!("{} exited with {code}:\n{tail}", cmd.tool.program())
    }))
}

/// First line of `program --version`, for the log and the dialog.
pub async fn tool_version(program: &Path) -> Option<String> {
    let out = tokio::process::Command::new(program).arg("--version").kill_on_drop(true).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next().map(|l| l.trim().to_string())
}

/// Copies the SQLite database `from` into `to` page by page with the online
/// backup API. Used both ways: database → backup file, and backup file →
/// database (restore). Other connections may keep using the database.
fn copy_pages(
    from: &rusqlite::Connection,
    to: &mut rusqlite::Connection,
    report: &Report,
    cancel: &Cancel,
) -> DbResult<()> {
    use rusqlite::backup::{Backup, StepResult};
    let backup = Backup::new(from, to).map_err(DbError::from_display)?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let step = backup.step(256).map_err(DbError::from_display)?;
        let p = backup.progress();
        if p.pagecount > 0 {
            report(Progress::Fraction((p.pagecount - p.remaining) as f32 / p.pagecount as f32));
        }
        match step {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            // Someone holds a lock: wait a little and go on.
            StepResult::Busy | StepResult::Locked => std::thread::sleep(Duration::from_millis(50)),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn open(path: &Path) -> DbResult<rusqlite::Connection> {
    let c = rusqlite::Connection::open(path).map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
    c.busy_timeout(Duration::from_secs(10)).map_err(DbError::from_display)?;
    Ok(c)
}

/// Backs up the SQLite database `db` into the new file `target`.
pub async fn sqlite_backup(
    db: PathBuf,
    target: PathBuf,
    method: SqliteMethod,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    tokio::task::spawn_blocking(move || {
        if target.exists() {
            std::fs::remove_file(&target).map_err(|e| DbError::new(format!("{}: {e}", target.display())))?;
        }
        let src = open(&db)?;
        match method {
            SqliteMethod::OnlineBackup => {
                report(Progress::Log(format!("Copying pages of {} to {}", db.display(), target.display())));
                let mut dst = open(&target)?;
                let r = copy_pages(&src, &mut dst, &report, &cancel);
                drop(dst);
                if r.is_err() {
                    let _ = std::fs::remove_file(&target);
                }
                r?;
            }
            SqliteMethod::VacuumInto => {
                report(Progress::Log(format!("VACUUM INTO {}", target.display())));
                // VACUUM has no steps to check between, so a watcher interrupts it.
                let interrupt = src.get_interrupt_handle();
                let done = Arc::new(AtomicBool::new(false));
                let watcher = {
                    let (done, cancel) = (done.clone(), cancel.clone());
                    std::thread::spawn(move || {
                        while !done.load(Ordering::Relaxed) {
                            if cancel.load(Ordering::Relaxed) {
                                interrupt.interrupt();
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    })
                };
                let r = src.execute("VACUUM INTO ?1", [target.to_string_lossy()]);
                done.store(true, Ordering::Relaxed);
                let _ = watcher.join();
                if r.is_err() {
                    let _ = std::fs::remove_file(&target);
                }
                if cancel.load(Ordering::Relaxed) {
                    return Err(cancelled());
                }
                r.map_err(DbError::from_display)?;
                report(Progress::Fraction(1.0));
            }
        }
        report(Progress::Bytes(size_of(&target)));
        let check: String =
            open(&target)?.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(DbError::from_display)?;
        report(Progress::Log(format!("quick_check: {check}")));
        Ok(())
    })
    .await
    .map_err(DbError::from_display)?
}

/// Replaces the content of the SQLite database `db` with the backup `source`.
pub async fn sqlite_restore(source: PathBuf, db: PathBuf, report: Report, cancel: Cancel) -> DbResult<()> {
    tokio::task::spawn_blocking(move || {
        let mut head = [0u8; 16];
        use std::io::Read;
        std::fs::File::open(&source)
            .and_then(|mut f| f.read_exact(&mut head))
            .map_err(|e| DbError::new(format!("{}: {e}", source.display())))?;
        if &head != b"SQLite format 3\0" {
            return Err(DbError::new(format!("{} is not an SQLite database", source.display())));
        }
        report(Progress::Log(format!("Copying pages of {} into {}", source.display(), db.display())));
        let src = rusqlite::Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(DbError::from_display)?;
        let mut dst = open(&db)?;
        copy_pages(&src, &mut dst, &report, &cancel)
    })
    .await
    .map_err(DbError::from_display)?
}

/// Runs DuckDB backup or restore `statements` (from `dbman_core::backup`) on a
/// new session of the database file `db`, sharing it with open sessions. The
/// last statement is cleanup and also runs after a failure. `watch` is the
/// file or folder being written.
pub async fn duckdb_run(
    db: String,
    statements: Vec<String>,
    watch: Option<PathBuf>,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    tokio::task::spawn_blocking(move || {
        if db == ":memory:" {
            return Err(DbError::new("an in-memory database cannot be backed up from another session"));
        }
        let shared = crate::duckdb::open_shared(&db)?;
        let conn = &shared.conn;
        // Statements have no steps to check between, so a watcher interrupts
        // them and reports how much has been written.
        let interrupt = conn.interrupt_handle();
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (done, cancel, report, watch) = (done.clone(), cancel.clone(), report.clone(), watch.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    if cancel.load(Ordering::Relaxed) {
                        interrupt.interrupt();
                        return;
                    }
                    if let Some(w) = &watch {
                        report(Progress::Bytes(size_of(w)));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
        };
        let (cleanup, work) = match statements.split_last() {
            Some((last, rest)) if last.starts_with("DETACH") => (Some(last), rest),
            _ => (None, &statements[..]),
        };
        let mut result = Ok(());
        for sql in work {
            report(Progress::Log(sql.clone()));
            if let Err(e) = conn.execute_batch(sql) {
                result = Err(if cancel.load(Ordering::Relaxed) { cancelled() } else { DbError::from_display(e) });
                break;
            }
        }
        if let Some(sql) = cleanup {
            report(Progress::Log(sql.clone()));
            let _ = conn.execute_batch(sql);
        }
        done.store(true, Ordering::Relaxed);
        let _ = watcher.join();
        if let Some(w) = &watch {
            report(Progress::Bytes(size_of(w)));
        }
        if result.is_ok() {
            report(Progress::Fraction(1.0));
        }
        result
    })
    .await
    .map_err(DbError::from_display)?
}

/// A SQL Server backup or restore; the file path is on the server.
#[derive(Debug, Clone)]
pub enum MsJob {
    Backup { database: String, options: dbman_core::backup::MsBackupOptions, path: String },
    Restore { database: String, options: dbman_core::backup::MsRestoreOptions, path: String },
}

async fn ms_exec(client: &mut crate::mssql::MsClient, sql: &str) -> DbResult<Vec<Vec<tiberius::Row>>> {
    let stream = client.simple_query(sql).await.map_err(crate::mssql::ms_err)?;
    stream.into_results().await.map_err(crate::mssql::ms_err)
}

/// Relocations for restoring `path` as `database`: every file goes to the
/// server's default data or log folder, named after the database.
async fn ms_moves(client: &mut crate::mssql::MsClient, database: &str, path: &str) -> DbResult<Vec<(String, String)>> {
    let d = dbman_core::sql::Dialect(dbman_core::DbKind::MsSql);
    let dirs = ms_exec(
        client,
        "SELECT CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(400)), \
         CAST(SERVERPROPERTY('InstanceDefaultLogPath') AS nvarchar(400))",
    )
    .await?;
    let row = dirs.first().and_then(|r| r.first()).ok_or_else(|| DbError::new("no default folders"))?;
    let data_dir = row.get::<&str, _>(0).unwrap_or_default().to_string();
    let log_dir = row.get::<&str, _>(1).map(String::from).unwrap_or_else(|| data_dir.clone());
    let files = ms_exec(client, &format!("RESTORE FILELISTONLY FROM DISK = {}", d.string(path))).await?;
    let mut moves = Vec::new();
    for (i, r) in files.first().into_iter().flatten().enumerate() {
        let logical = r.get::<&str, _>("LogicalName").unwrap_or_default().to_string();
        let physical = r.get::<&str, _>("PhysicalName").unwrap_or_default();
        let is_log = r.get::<&str, _>("Type") == Some("L");
        let ext = physical.rsplit_once('.').map(|(_, e)| e).unwrap_or(if is_log { "ldf" } else { "mdf" });
        let suffix = match (is_log, i) {
            (true, _) => "_log".to_string(),
            (false, 0) => String::new(),
            (false, n) => format!("_{n}"),
        };
        let dir = if is_log { &log_dir } else { &data_dir };
        let sep = if dir.ends_with(['/', '\\']) || dir.is_empty() {
            ""
        } else if dir.contains('\\') {
            "\\"
        } else {
            "/"
        };
        moves.push((logical, format!("{dir}{sep}{database}{suffix}.{ext}")));
    }
    Ok(moves)
}

/// Runs a SQL Server backup or restore on the server. A second session
/// reports `percent_complete` and, on cancel, ends the job with `KILL`.
pub async fn mssql_run(cfg: dbman_core::ConnectionConfig, job: MsJob, report: Report, cancel: Cancel) -> DbResult<()> {
    let (cfg, _tunnel) = crate::ssh::route(&cfg).await?;
    use dbman_core::backup::{ms_backup, ms_restore};
    let mut client = crate::mssql::open_client(&cfg, Some("master")).await?;
    let spid: i16 = ms_exec(&mut client, "SELECT CAST(@@SPID AS smallint)")
        .await?
        .first()
        .and_then(|r| r.first())
        .and_then(|r| r.get::<i16, _>(0))
        .unwrap_or(0);
    let (statements, cleanup, database) = match &job {
        MsJob::Backup { database, options, path } => (ms_backup(database, options, path), None, database.clone()),
        MsJob::Restore { database, options, path } => {
            let moves = if options.relocate { ms_moves(&mut client, database, path).await? } else { Vec::new() };
            let (s, c) = ms_restore(database, options, path, &moves);
            (s, c, database.clone())
        }
    };

    let done = Arc::new(AtomicBool::new(false));
    let monitor = {
        let (cfg, done, cancel, report) = (cfg.clone(), done.clone(), cancel.clone(), report.clone());
        tokio::spawn(async move {
            let Ok(mut watcher) = crate::mssql::open_client(&cfg, Some("master")).await else {
                return;
            };
            let poll = format!(
                "SELECT CAST(percent_complete AS float) FROM sys.dm_exec_requests WHERE session_id = {spid} \
                 AND command IN ('BACKUP DATABASE', 'BACKUP LOG', 'RESTORE DATABASE', 'RESTORE VERIFYON')"
            );
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    let _ = ms_exec(&mut watcher, &format!("KILL {spid}")).await;
                    return;
                }
                if let Ok(r) = ms_exec(&mut watcher, &poll).await
                    && let Some(p) = r.first().and_then(|r| r.first()).and_then(|r| r.get::<f64, _>(0))
                {
                    report(Progress::Fraction((p / 100.0) as f32));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    };

    let mut result = Ok(());
    for sql in &statements {
        report(Progress::Log(format!("{sql};")));
        if let Err(e) = ms_exec(&mut client, sql).await {
            result = Err(if cancel.load(Ordering::Relaxed) { cancelled() } else { e });
            break;
        }
    }
    done.store(true, Ordering::Relaxed);
    let _ = monitor.await;
    if let Some(sql) = cleanup {
        report(Progress::Log(format!("{sql};")));
        // The killed session cannot run it; use a new one.
        let cleaned = match ms_exec(&mut client, &sql).await {
            Ok(_) => Ok(()),
            Err(_) => match crate::mssql::open_client(&cfg, Some("master")).await {
                Ok(mut c) => ms_exec(&mut c, &sql).await.map(|_| ()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = cleaned {
            report(Progress::Log(format!("cleanup failed: {}", e.message)));
        }
    }
    result?;
    if matches!(job, MsJob::Backup { .. })
        && let Ok(sets) = ms_exec(
            &mut client,
            &format!(
                "SELECT TOP 1 CAST(compressed_backup_size AS bigint) FROM msdb.dbo.backupset \
                 WHERE database_name = {} ORDER BY backup_finish_date DESC",
                dbman_core::sql::Dialect(dbman_core::DbKind::MsSql).string(&database)
            ),
        )
        .await
        && let Some(n) = sets.first().and_then(|r| r.first()).and_then(|r| r.get::<i64, _>(0))
    {
        report(Progress::Bytes(n as u64));
    }
    report(Progress::Fraction(1.0));
    Ok(())
}

/// Runs a ClickHouse `BACKUP … ASYNC` or `RESTORE … ASYNC` statement and
/// follows it in `system.backups`; `shown` is the statement for the log (with
/// secrets hidden). Cancel kills the operation's query.
pub async fn clickhouse_run(
    cfg: dbman_core::ConnectionConfig,
    sql: String,
    shown: String,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    use crate::clickhouse::Client;
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let num = |v: &serde_json::Value| text(v).parse::<u64>().unwrap_or(0);
    let (cfg, _tunnel) = crate::ssh::route(&cfg).await?;
    let client = Client::new(&cfg)?;
    report(Progress::Log(format!("{shown};")));
    let started = client.rows(&sql, &[]).await?;
    let id = started.first().and_then(|r| r.first()).map(text).ok_or_else(|| DbError::new("no backup id returned"))?;
    report(Progress::Log(format!("id {id}")));
    let restore = sql.trim_start().starts_with("RESTORE");
    let mut killed = false;
    loop {
        let rows = client
            .rows(
                "SELECT status, error, num_files, total_size, files_read, query_id, compressed_size
                 FROM system.backups WHERE id = {id:String}",
                &[("param_id", id.as_str())],
            )
            .await?;
        let Some(r) = rows.first() else {
            return Err(DbError::new(format!("backup {id} is not in system.backups")));
        };
        let status = text(&r[0]);
        let (files, size, read) = (num(&r[2]), num(&r[3]), num(&r[4]));
        match status.as_str() {
            "BACKUP_CREATED" | "RESTORED" => {
                report(Progress::Log(format!("{status}: {files} files, {size} bytes")));
                if !restore {
                    report(Progress::Bytes(num(&r[6]).max(size)));
                }
                report(Progress::Fraction(1.0));
                return Ok(());
            }
            s if s.ends_with("_CANCELLED") => return Err(cancelled()),
            s if s.ends_with("_FAILED") => {
                if killed {
                    return Err(cancelled());
                }
                return Err(DbError::new(text(&r[1]).trim().to_string()));
            }
            _ => {}
        }
        if restore && files > 0 {
            report(Progress::Fraction(read as f32 / files as f32));
        } else if !restore {
            report(Progress::Bytes(size));
        }
        if cancel.load(Ordering::Relaxed) && !killed {
            killed = true;
            let kill = format!(
                "KILL QUERY WHERE query_id = {} ASYNC",
                dbman_core::sql::Dialect(dbman_core::DbKind::ClickHouse).string(&text(&r[5]))
            );
            client.exec(&kill, &[]).await?;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
