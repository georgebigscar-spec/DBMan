//! Data export to a file and import from a file, run as background jobs on
//! their own session: export pages through a table by its primary key, import
//! reads the file on a blocking thread and inserts batches of rows.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dbman_core::export::Format;
use dbman_core::import::{self, ColumnMap, CsvOptions, Field, ImportFormat};
use dbman_core::sql::Dialect;
use dbman_core::{Cell, Connection, ConnectionConfig, DbError, DbKind, DbResult, ObjectPath, StatementOutcome, dml};

use crate::backup::{Cancel, Progress, Report};
use crate::files::FileWriter;

/// Rows fetched per page when exporting a table with a primary key.
const PAGE: usize = 5000;

/// What to export.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportSource {
    Table(ObjectPath),
    /// The rows of a query, run in `database`.
    Query {
        database: Option<String>,
        sql: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportJob {
    pub source: ExportSource,
    pub format: Format,
    pub path: PathBuf,
    /// Target of SQL INSERT statements (defaults to the exported table).
    pub insert_into: Option<ObjectPath>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportJob {
    pub table: ObjectPath,
    pub path: PathBuf,
    pub format: ImportFormat,
    pub csv: CsvOptions,
    pub columns: Vec<ColumnMap>,
    pub empty_as_null: bool,
    /// Delete the table's rows first.
    pub clear_first: bool,
    /// All or nothing: roll back everything on an error or cancel.
    pub transaction: bool,
    pub batch_rows: usize,
}

pub(crate) fn cancelled() -> DbError {
    DbError::new("cancelled")
}

async fn wait_cancel(cancel: &Cancel) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Runs a statement; a cancel request stops it on the server.
pub(crate) async fn run(
    conn: &Arc<dyn Connection>,
    database: Option<&str>,
    sql: &str,
    max_rows: usize,
    cancel: &Cancel,
) -> DbResult<StatementOutcome> {
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    tokio::select! {
        r = conn.execute(database, sql, max_rows) => r,
        () = wait_cancel(cancel) => {
            let _ = conn.cancel().await;
            Err(cancelled())
        }
    }
}

pub(crate) fn rows_of(outcome: StatementOutcome) -> dbman_core::QueryResult {
    match outcome {
        StatementOutcome::Rows(r) => r,
        StatementOutcome::Done { .. } => Default::default(),
    }
}

/// Writes the rows of a table or query to a file. A partial file is removed
/// when the job fails or is cancelled.
pub async fn export(cfg: ConnectionConfig, job: ExportJob, report: Report, cancel: Cancel) -> DbResult<()> {
    let result = export_inner(&cfg, &job, &report, &cancel).await;
    if result.is_err() && job.path.is_file() {
        let _ = std::fs::remove_file(&job.path);
        report(Progress::Log(format!("Removed the incomplete {}", job.path.display())));
    }
    result
}

async fn export_inner(cfg: &ConnectionConfig, job: &ExportJob, report: &Report, cancel: &Cancel) -> DbResult<()> {
    let d = Dialect(cfg.kind);
    let conn = crate::connect(cfg).await?;
    let insert_into = job.insert_into.clone().or_else(|| match &job.source {
        ExportSource::Table(t) => Some(t.clone()),
        ExportSource::Query { .. } => None,
    });
    let mut writer = FileWriter::create(&job.path, job.format, d, insert_into)?;

    match &job.source {
        ExportSource::Query { database, sql } => {
            report(Progress::Log(format!("Running {sql}")));
            let r = rows_of(run(&conn, database.as_deref(), sql, usize::MAX / 2, cancel).await?);
            writer.begin(&r.columns)?;
            writer.rows(&r.rows)?;
        }
        ExportSource::Table(t) => {
            let db = Some(t.database.as_str()).filter(|s| !s.is_empty());
            let count_sql = format!("SELECT COUNT(*) FROM {}", d.qualified(t));
            let total = match run(&conn, db, &count_sql, 1, cancel).await {
                Ok(o) => rows_of(o).rows.first().and_then(|r| r.first()).and_then(|c| match c {
                    Cell::Text(s) => s.parse::<u64>().ok(),
                    _ => None,
                }),
                Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
                Err(_) => None,
            };
            if let Some(n) = total {
                report(Progress::Log(format!("{} has {n} rows", d.qualified(t))));
            }
            let details = conn.table_details(t).await?;
            let key = details.primary_key().iter().map(|c| d.ident(c)).collect::<Vec<_>>().join(", ");
            if key.is_empty() {
                // Without a key, pages could overlap or skip rows; read it in one go.
                report(Progress::Log("No primary key: reading every row in one query".into()));
                let sql = format!("SELECT * FROM {}", d.qualified(t));
                let r = rows_of(run(&conn, db, &sql, usize::MAX / 2, cancel).await?);
                writer.begin(&r.columns)?;
                writer.rows(&r.rows)?;
            } else {
                let mut offset = 0;
                loop {
                    let sql = d.select_page(t, "", &key, PAGE, offset);
                    let r = rows_of(run(&conn, db, &sql, PAGE, cancel).await?);
                    if offset == 0 {
                        writer.begin(&r.columns)?;
                    }
                    writer.rows(&r.rows)?;
                    offset += r.rows.len();
                    if let Some(b) = writer.bytes() {
                        report(Progress::Bytes(b));
                    }
                    if let Some(n) = total.filter(|n| *n > 0) {
                        report(Progress::Fraction(offset as f32 / n as f32));
                    }
                    if r.rows.len() < PAGE {
                        break;
                    }
                }
            }
        }
    }
    let rows = writer.written();
    if job.format == Format::Parquet {
        report(Progress::Log("Writing Parquet".into()));
    }
    writer.finish()?;
    let size = std::fs::metadata(&job.path).map(|m| m.len()).unwrap_or(0);
    report(Progress::Bytes(size));
    report(Progress::Log(format!("Wrote {rows} rows to {}", job.path.display())));
    Ok(())
}

/// Inserts the rows of a file into a table.
pub async fn import(cfg: ConnectionConfig, job: ImportJob, report: Report, cancel: Cancel) -> DbResult<()> {
    if job.columns.iter().all(|c| c.source.is_none()) {
        return Err(DbError::new("No column of the file is mapped to the table."));
    }
    let conn = crate::connect(&cfg).await?;
    let db = Some(job.table.database.clone()).filter(|s| !s.is_empty());
    let db = db.as_deref();
    // ClickHouse has no transactions.
    let transaction = job.transaction && cfg.kind != DbKind::ClickHouse;
    if transaction {
        run(&conn, db, dml::begin(cfg.kind), 0, &cancel).await?;
    }
    let result = import_rows(&conn, db, &cfg, &job, &report, &cancel).await;
    match result {
        Ok(n) => {
            if transaction {
                run(&conn, db, "COMMIT", 0, &cancel).await?;
            }
            report(Progress::Fraction(1.0));
            report(Progress::Log(format!("Imported {n} rows into {}", Dialect(cfg.kind).qualified(&job.table))));
            Ok(())
        }
        Err(e) => {
            if transaction {
                // The session may still be busy with a cancelled statement; a fresh flag lets ROLLBACK run.
                let idle: Cancel = Default::default();
                match run(&conn, db, "ROLLBACK", 0, &idle).await {
                    Ok(_) => report(Progress::Log("Rolled back; the table is unchanged".into())),
                    Err(r) => report(Progress::Log(format!("Rollback failed: {}", r.message))),
                }
            } else {
                report(Progress::Log("Rows inserted before the error stay in the table".into()));
            }
            Err(e)
        }
    }
}

async fn import_rows(
    conn: &Arc<dyn Connection>,
    db: Option<&str>,
    cfg: &ConnectionConfig,
    job: &ImportJob,
    report: &Report,
    cancel: &Cancel,
) -> DbResult<u64> {
    let d = Dialect(cfg.kind);
    if job.clear_first {
        let sql = match cfg.kind {
            DbKind::ClickHouse => format!("TRUNCATE TABLE {}", d.qualified(&job.table)),
            // DELETE stays inside the transaction and respects foreign keys.
            _ => format!("DELETE FROM {}", d.qualified(&job.table)),
        };
        report(Progress::Log(sql.clone()));
        run(conn, db, &sql, 0, cancel).await?;
    }
    let (before, after) = import::identity_statements(cfg.kind, &job.table, &job.columns);
    for sql in &before {
        report(Progress::Log(sql.clone()));
        run(conn, db, sql, 0, cancel).await?;
    }

    // The file is read and parsed on a blocking thread, a few batches ahead.
    let size = std::fs::metadata(&job.path).map(|m| m.len()).unwrap_or(0);
    let read = Arc::new(AtomicU64::new(0));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<DbResult<Vec<Vec<Field>>>>(4);
    let reader = {
        let (path, format, csv, batch, read) =
            (job.path.clone(), job.format, job.csv.clone(), job.batch_rows.max(1), read.clone());
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let mut source = match crate::files::open(&path, format, &csv, read) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    return;
                }
            };
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let rows = import::preview(source.as_mut(), batch);
                let last = matches!(&rows, Ok(r) if r.len() < batch) || rows.is_err();
                if matches!(&rows, Ok(r) if r.is_empty()) || tx.blocking_send(rows).is_err() || last {
                    return;
                }
            }
        })
    };

    let mut done: u64 = 0;
    let result = async {
        while let Some(batch) = rx.recv().await {
            let rows = batch?;
            let sql = import::insert_rows(d, &job.table, &job.columns, &rows, job.empty_as_null);
            run(conn, db, &sql, 0, cancel).await.map_err(|e| {
                if cancel.load(Ordering::Relaxed) {
                    e
                } else {
                    DbError::new(format!("Rows {}–{}: {}", done + 1, done + rows.len() as u64, e.message))
                }
            })?;
            done += rows.len() as u64;
            report(Progress::Log(format!("{done} rows")));
            if size > 0 {
                report(Progress::Fraction(read.load(Ordering::Relaxed) as f32 / size as f32));
            }
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        for sql in &after {
            report(Progress::Log(sql.clone()));
            run(conn, db, sql, 0, cancel).await?;
        }
        Ok(done)
    }
    .await;
    drop(rx);
    let _ = reader.await;
    result
}
