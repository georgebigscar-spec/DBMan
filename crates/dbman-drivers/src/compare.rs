//! Reads what [`dbman_core::compare`] compares: the objects of a schema, and
//! the rows of a table pair page by page; and runs sync scripts.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use dbman_core::compare::{DataDiff, DataDiffer, DataSpec, SchemaObject, Snapshot};
use dbman_core::sql::Dialect;
use dbman_core::{Cell, Connection, ConnectionConfig, DbError, DbKind, DbResult, ObjectKind, ObjectPath, dml};

use crate::backup::{Cancel, Progress, Report};
use crate::transfer::{cancelled, rows_of, run};

/// Rows fetched per page.
const PAGE: usize = 5000;

/// Reads every object of a schema: tables with their structure, and every
/// object's definition when `with_ddl`. `progress(done, total)` follows the objects.
pub async fn snapshot(
    conn: &Arc<dyn Connection>,
    database: &str,
    schema: &str,
    with_ddl: bool,
    progress: impl Fn(usize, usize),
    cancel: &Cancel,
) -> DbResult<Snapshot> {
    let objects = conn.objects(database, schema).await?;
    let objects: Vec<_> =
        if with_ddl { objects } else { objects.into_iter().filter(|o| o.kind == ObjectKind::Table).collect() };
    let total = objects.len();
    let mut out = Vec::with_capacity(total);
    for (i, object) in objects.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        progress(i, total);
        let details = match object.kind {
            ObjectKind::Table => Some(conn.table_details(&object.path).await?),
            _ => None,
        };
        let ddl = if with_ddl { conn.ddl(&object).await.map_err(|e| e.message) } else { Ok(String::new()) };
        out.push(SchemaObject { object, details, ddl });
    }
    Ok(Snapshot { database: database.to_string(), schema: schema.to_string(), objects: out })
}

/// Reads all rows of `path`, picking the spec's columns by name, page by page in key order.
async fn read_rows(
    conn: &Arc<dyn Connection>,
    path: &ObjectPath,
    spec: &DataSpec,
    mut each: impl FnMut(Vec<Cell>),
    progress: &(impl Fn(u64) + ?Sized),
    cancel: &Cancel,
) -> DbResult<()> {
    let d = Dialect(conn.kind());
    let db = Some(path.database.as_str()).filter(|s| !s.is_empty());
    let order = spec.key.iter().map(|&k| d.ident(&spec.columns[k].column)).collect::<Vec<_>>().join(", ");
    let mut offset = 0;
    loop {
        let sql = d.select_page(path, "", &order, PAGE, offset);
        let r = rows_of(run(conn, db, &sql, PAGE, cancel).await?);
        let picks: Vec<usize> = spec
            .columns
            .iter()
            .map(|c| {
                r.columns
                    .iter()
                    .position(|m| m.name == c.column)
                    .ok_or_else(|| DbError::new(format!("Column {} is missing in {}", c.column, path.name)))
            })
            .collect::<DbResult<_>>()?;
        let n = r.rows.len();
        for mut row in r.rows {
            each(picks.iter().map(|&i| std::mem::replace(&mut row[i], Cell::Null)).collect());
        }
        offset += n;
        progress(offset as u64);
        if n < PAGE {
            return Ok(());
        }
    }
}

/// Compares the rows of a source table with the target table the spec names;
/// statements are written for the target. `progress(rows)` counts rows read.
pub async fn compare_data(
    source: &Arc<dyn Connection>,
    source_table: &ObjectPath,
    target: &Arc<dyn Connection>,
    spec: DataSpec,
    progress: &(dyn Fn(u64) + Send + Sync),
    cancel: &Cancel,
) -> DbResult<DataDiff> {
    let target_table = spec.table.clone();
    let mut differ = DataDiffer::new(target.kind(), spec.clone());
    read_rows(target, &target_table, &spec, |r| differ.add_target(r), progress, cancel).await?;
    let read = differ.rows_read();
    let after_target = move |n: u64| progress(read + n);
    read_rows(source, source_table, &spec, |r| differ.add_source(r), &after_target, cancel).await?;
    Ok(differ.finish())
}

/// Runs statements in order on a new session, in one transaction when asked;
/// stops at the first error (rolling back).
pub async fn run_script(
    cfg: ConnectionConfig,
    database: Option<String>,
    statements: Vec<String>,
    transaction: bool,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    let conn = crate::connect(&cfg).await?;
    let db = database.as_deref();
    let transaction = transaction && cfg.kind != DbKind::ClickHouse;
    if transaction {
        run(&conn, db, dml::begin(cfg.kind), 0, &cancel).await?;
    }
    let total = statements.len();
    let mut result = Ok(());
    for (i, sql) in statements.iter().enumerate() {
        let first = sql.lines().next().unwrap_or("");
        let shown: String = first.chars().take(160).collect();
        if total <= 200 || i % 100 == 0 {
            report(Progress::Log(if shown.len() < sql.len() { format!("{shown} …") } else { shown }));
        }
        if let Err(e) = run(&conn, db, sql, 0, &cancel).await {
            result = Err(if cancel.load(Ordering::Relaxed) {
                e
            } else {
                DbError::new(format!("Statement {} of {total}: {}", i + 1, e.message))
            });
            break;
        }
        report(Progress::Fraction((i + 1) as f32 / total.max(1) as f32));
    }
    match result {
        Ok(()) => {
            if transaction {
                run(&conn, db, "COMMIT", 0, &cancel).await?;
            }
            report(Progress::Log(format!("Ran {total} statements")));
            Ok(())
        }
        Err(e) => {
            if transaction {
                let idle: Cancel = Default::default();
                match run(&conn, db, "ROLLBACK", 0, &idle).await {
                    Ok(_) => report(Progress::Log("Rolled back; the target is unchanged".into())),
                    Err(r) => report(Progress::Log(format!("Rollback failed: {}", r.message))),
                }
            } else {
                report(Progress::Log("Statements before the error stay applied".into()));
            }
            Err(e)
        }
    }
}
