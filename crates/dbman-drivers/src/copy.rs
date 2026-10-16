//! Copies tables with their rows from one connection to another, also
//! between different engines (types are mapped by [`dbman_core::copy`]).
//! Missing tables are created in foreign key order; rows go in batches, in a
//! transaction per table where the target has transactions.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dbman_core::compare::table_order;
use dbman_core::copy::{self, CopyColumn};
use dbman_core::import::{self, ColumnMap};
use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, Connection, ConnectionConfig, DbError, DbKind, DbResult, ObjectKind, ObjectPath, TableDetails, ddl, dml,
};

use crate::backup::{Cancel, Progress, Report};
use crate::transfer::{cancelled, rows_of, run};

/// Rows read from the source per query.
const PAGE: usize = 5000;

/// What happens to a table that already exists in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Add the rows to the ones there.
    Append,
    /// Delete its rows first.
    Replace,
    /// Drop it and create it again from the source.
    Recreate,
}

impl Existing {
    pub const ALL: [Existing; 3] = [Existing::Append, Existing::Replace, Existing::Recreate];

    pub fn label(self) -> &'static str {
        match self {
            Existing::Append => "Add the rows",
            Existing::Replace => "Replace the rows",
            Existing::Recreate => "Drop and create again",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CopyJob {
    pub source_db: String,
    pub source_schema: String,
    pub tables: Vec<String>,
    pub target_db: String,
    pub target_schema: String,
    pub existing: Existing,
    /// Rows per INSERT.
    pub batch_rows: usize,
}

fn target_cfg(cfg: &ConnectionConfig, db: &str) -> ConnectionConfig {
    let mut cfg = cfg.clone();
    if cfg.kind == DbKind::MySql {
        cfg.database = db.to_string();
    }
    cfg
}

async fn count(conn: &Arc<dyn Connection>, path: &ObjectPath, cancel: &Cancel) -> Option<u64> {
    let d = Dialect(conn.kind());
    let db = Some(path.database.as_str()).filter(|s| !s.is_empty());
    let sql = format!("SELECT COUNT(*) FROM {}", d.qualified(path));
    let r = rows_of(run(conn, db, &sql, 1, cancel).await.ok()?);
    match r.rows.first()?.first()? {
        Cell::Text(s) => s.parse().ok(),
        _ => None,
    }
}

/// Copies the job's tables from `source` to `target`.
pub async fn copy(
    source: ConnectionConfig,
    target: ConnectionConfig,
    job: CopyJob,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    let s = crate::connect(&target_cfg(&source, &job.source_db)).await?;
    let t = crate::connect(&target_cfg(&target, &job.target_db)).await?;
    let (from, to) = (s.kind(), t.kind());
    let td = Dialect(to);
    let tdb = Some(job.target_db.as_str());
    let spath = |n: &str| ObjectPath::new(&job.source_db, &job.source_schema, n);
    let tpath = |n: &str| ObjectPath::new(&job.target_db, &job.target_schema, n);

    let mut tables: Vec<(String, TableDetails)> = Vec::new();
    for name in &job.tables {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        tables.push((name.clone(), s.table_details(&spath(name)).await?));
    }
    let order = table_order(
        &tables
            .iter()
            .map(|(n, d)| (n.clone(), d.foreign_keys.iter().map(|f| f.ref_table.clone()).collect()))
            .collect::<Vec<_>>(),
    );
    let tables: Vec<&(String, TableDetails)> = order.iter().map(|&i| &tables[i]).collect();
    let copied: HashSet<&str> = tables.iter().map(|(n, _)| n.as_str()).collect();
    let existing: HashSet<String> = t
        .objects(&job.target_db, &job.target_schema)
        .await?
        .into_iter()
        .filter(|o| o.kind == ObjectKind::Table)
        .map(|o| o.path.name)
        .collect();
    let exec = |sql: String| {
        let (t, cancel, report) = (t.clone(), cancel.clone(), report.clone());
        async move {
            report(Progress::Log(sql.lines().next().unwrap_or("").chars().take(160).collect()));
            run(&t, tdb, &sql, 0, &cancel).await.map_err(|e| DbError::new(format!("{sql}\n{}", e.message)))
        }
    };

    // Drops and deletes run children first, creates parents first.
    let recreate = job.existing == Existing::Recreate;
    if recreate {
        for (name, _) in tables.iter().rev().filter(|(n, _)| existing.contains(n)) {
            exec(format!("DROP TABLE {}", td.qualified(&tpath(name)))).await?;
        }
    }
    for (name, details) in &tables {
        if existing.contains(name) && !recreate {
            continue;
        }
        let (def, notes) = copy::target_def(from, to, tpath(name), details, |r| copied.contains(r));
        for n in notes {
            report(Progress::Log(n));
        }
        let plan = ddl::create_table(to, &def);
        for n in &plan.notes {
            report(Progress::Log(n.clone()));
        }
        for st in plan.statements {
            exec(st).await?;
        }
    }
    if job.existing == Existing::Replace {
        for (name, _) in tables.iter().rev().filter(|(n, _)| existing.contains(n)) {
            exec(match to {
                DbKind::ClickHouse => format!("TRUNCATE TABLE {}", td.qualified(&tpath(name))),
                _ => format!("DELETE FROM {}", td.qualified(&tpath(name))),
            })
            .await?;
        }
    }

    let mut total = 0;
    for (name, _) in &tables {
        total += count(&s, &spath(name), &cancel).await.unwrap_or(0);
    }
    report(Progress::Log(format!("{total} rows in {} tables", tables.len())));
    let mut done: u64 = 0;
    for (name, details) in &tables {
        let n =
            copy_table(&s, &t, &spath(name), &tpath(name), details, &job, &report, &cancel, &mut done, total).await?;
        report(Progress::Log(format!("{name}: {n} rows")));
    }
    report(Progress::Fraction(1.0));
    report(Progress::Log(format!("Copied {done} rows")));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn copy_table(
    s: &Arc<dyn Connection>,
    t: &Arc<dyn Connection>,
    source: &ObjectPath,
    target: &ObjectPath,
    details: &TableDetails,
    job: &CopyJob,
    report: &Report,
    cancel: &Cancel,
    done: &mut u64,
    total: u64,
) -> DbResult<u64> {
    let to = t.kind();
    let target_details = t.table_details(target).await?;
    let columns: Vec<CopyColumn> = details
        .columns
        .iter()
        .filter_map(|c| target_details.columns.iter().find(|tc| tc.name == c.name))
        .map(|c| CopyColumn::new(to, c))
        .collect();
    if columns.is_empty() {
        return Err(DbError::new(format!("{}: no column of the source is in the target table", target.name)));
    }
    let maps: Vec<ColumnMap> = columns
        .iter()
        .map(|c| ColumnMap {
            column: c.name.clone(),
            source: Some(0),
            numeric: c.numeric,
            binary: false,
            auto_increment: c.auto_increment,
        })
        .collect();
    let (before, after) = import::identity_statements(to, target, &maps);
    let tdb = Some(target.database.as_str());
    let transaction = to != DbKind::ClickHouse;
    if transaction {
        run(t, tdb, dml::begin(to), 0, cancel).await?;
    }
    let result = async {
        for st in &before {
            run(t, tdb, st, 0, cancel).await?;
        }
        let n = copy_rows(s, t, source, target, details, &columns, job.batch_rows.max(1), report, cancel, done, total)
            .await?;
        for st in &after {
            run(t, tdb, st, 0, cancel).await?;
        }
        Ok::<_, DbError>(n)
    }
    .await;
    match result {
        Ok(n) => {
            if transaction {
                run(t, tdb, "COMMIT", 0, cancel).await?;
            }
            Ok(n)
        }
        Err(e) => {
            if transaction {
                let idle: Cancel = Default::default();
                if run(t, tdb, "ROLLBACK", 0, &idle).await.is_ok() {
                    report(Progress::Log(format!("{}: rolled back", target.name)));
                }
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn copy_rows(
    s: &Arc<dyn Connection>,
    t: &Arc<dyn Connection>,
    source: &ObjectPath,
    target: &ObjectPath,
    details: &TableDetails,
    columns: &[CopyColumn],
    batch: usize,
    report: &Report,
    cancel: &Cancel,
    done: &mut u64,
    total: u64,
) -> DbResult<u64> {
    let sd = Dialect(s.kind());
    let sdb = Some(source.database.as_str()).filter(|d| !d.is_empty());
    let tdb = Some(target.database.as_str());
    let key = details.primary_key().iter().map(|c| sd.ident(c)).collect::<Vec<_>>().join(", ");
    let mut offset = 0;
    let mut copied: u64 = 0;
    loop {
        // Without a key, pages could overlap or skip rows; read it in one go.
        let (sql, page) = if key.is_empty() {
            (format!("SELECT * FROM {}", sd.qualified(source)), usize::MAX / 2)
        } else {
            (sd.select_page(source, "", &key, PAGE, offset), PAGE)
        };
        let r = rows_of(run(s, sdb, &sql, page, cancel).await?);
        let picks: Vec<usize> = columns
            .iter()
            .map(|c| {
                r.columns
                    .iter()
                    .position(|m| m.name == c.name)
                    .ok_or_else(|| DbError::new(format!("Column {} is missing in {}", c.name, source.name)))
            })
            .collect::<DbResult<_>>()?;
        let n = r.rows.len();
        let mut rows = r.rows.into_iter();
        loop {
            let chunk: Vec<Vec<Cell>> = rows
                .by_ref()
                .take(batch)
                .map(|mut row| picks.iter().map(|&i| std::mem::replace(&mut row[i], Cell::Null)).collect())
                .collect();
            if chunk.is_empty() {
                break;
            }
            let len = chunk.len();
            let sql = copy::insert_sql(t.kind(), target, columns, chunk);
            run(t, tdb, &sql, 0, cancel).await.map_err(|e| {
                if cancel.load(Ordering::Relaxed) {
                    e
                } else {
                    DbError::new(format!("{}, rows {}–{}: {}", target.name, copied + 1, copied + len as u64, e.message))
                }
            })?;
            copied += len as u64;
            *done += len as u64;
            if total > 0 {
                report(Progress::Fraction(*done as f32 / total as f32));
            }
        }
        offset += n;
        if key.is_empty() || n < PAGE {
            return Ok(copied);
        }
    }
}
