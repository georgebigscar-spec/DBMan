//! Fills a table with test rows made by [`dbman_core::generate`]: parent
//! values for foreign keys are read first, sequences start after the rows
//! already there, and rows go in batches in one transaction where the engine
//! has them.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use dbman_core::copy::{self, CopyColumn};
use dbman_core::generate::{ColumnPlan, Generator, Kind};
use dbman_core::import::{self, ColumnMap};
use dbman_core::sql::Dialect;
use dbman_core::{Cell, ConnectionConfig, DbError, DbKind, DbResult, ObjectPath, dml};

use crate::backup::{Cancel, Progress, Report};
use crate::transfer::{cancelled, rows_of, run};

/// Parent values read per foreign key.
const PARENT_VALUES: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct GenerateJob {
    pub table: ObjectPath,
    pub rows: u64,
    pub plan: Vec<ColumnPlan>,
    pub seed: u64,
    /// Delete the table's rows first.
    pub clear_first: bool,
    /// Rows per INSERT.
    pub batch_rows: usize,
}

/// Inserts the job's rows.
pub async fn generate(cfg: ConnectionConfig, job: GenerateJob, report: Report, cancel: Cancel) -> DbResult<()> {
    let mut cfg = cfg;
    if cfg.kind == DbKind::MySql {
        cfg.database = job.table.database.clone();
    }
    let conn = crate::connect(&cfg).await?;
    let kind = conn.kind();
    let d = Dialect(kind);
    let db = Some(job.table.database.as_str()).filter(|s| !s.is_empty());
    let table = d.qualified(&job.table);
    let details = conn.table_details(&job.table).await?;
    let mut plan = job.plan.clone();
    if !plan.iter().any(ColumnPlan::writes) {
        return Err(DbError::new("Every column is skipped; choose a generator for at least one."));
    }

    // Values of the parent tables.
    let mut references: HashMap<String, Vec<Cell>> = HashMap::new();
    for p in &plan {
        let Kind::Reference { table: parent, column } = &p.kind else { continue };
        let schema = details
            .foreign_keys
            .iter()
            .find(|f| f.columns.len() == 1 && f.columns[0] == p.column && f.ref_table == *parent)
            .map_or(job.table.schema.clone(), |f| f.ref_schema.clone());
        let schema = if schema.is_empty() { job.table.schema.clone() } else { schema };
        let path = ObjectPath::new(&job.table.database, &schema, parent);
        let sql = format!("SELECT DISTINCT {} FROM {}", d.ident(column), d.qualified(&path));
        let r = rows_of(run(&conn, db, &sql, PARENT_VALUES, &cancel).await?);
        let values: Vec<Cell> = r.rows.into_iter().filter_map(|mut row| row.drain(..).next()).collect();
        if values.is_empty() && !details.columns.iter().any(|c| c.name == p.column && c.nullable) {
            return Err(DbError::new(format!("{parent} has no rows for {} to point at; fill it first.", p.column)));
        }
        report(Progress::Log(format!("{}: {} values from {parent}.{column}", p.column, values.len())));
        references.insert(p.column.clone(), values);
    }

    let transaction = kind != DbKind::ClickHouse;
    if transaction {
        run(&conn, db, dml::begin(kind), 0, &cancel).await?;
    }
    let result = async {
        if job.clear_first {
            let sql = match kind {
                DbKind::ClickHouse => format!("TRUNCATE TABLE {table}"),
                _ => format!("DELETE FROM {table}"),
            };
            report(Progress::Log(sql.clone()));
            run(&conn, db, &sql, 0, &cancel).await?;
        }
        // Sequences go on after the rows already in the table.
        for p in &mut plan {
            if let Kind::Sequence { start } = &mut p.kind {
                let sql = format!("SELECT MAX({}) FROM {table}", d.ident(&p.column));
                let r = rows_of(run(&conn, db, &sql, 1, &cancel).await?);
                if let Some(Cell::Text(max)) = r.rows.first().and_then(|r| r.first())
                    && let Ok(max) = max.trim().split('.').next().unwrap_or("").parse::<i64>()
                {
                    *start = (*start).max(max + 1);
                }
            }
        }
        let columns: Vec<CopyColumn> = plan
            .iter()
            .filter(|p| p.writes())
            .map(|p| {
                let c = details
                    .columns
                    .iter()
                    .find(|c| c.name == p.column)
                    .ok_or_else(|| DbError::new(format!("{} has no column {}", job.table.name, p.column)))?;
                Ok(CopyColumn::new(kind, c))
            })
            .collect::<DbResult<_>>()?;
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
        let (before, after) = import::identity_statements(kind, &job.table, &maps);
        for st in &before {
            run(&conn, db, st, 0, &cancel).await?;
        }
        // Unique values go on after the rows already there.
        let existing = if job.clear_first {
            0
        } else {
            let r = rows_of(run(&conn, db, &format!("SELECT COUNT(*) FROM {table}"), 1, &cancel).await?);
            match r.rows.first().and_then(|r| r.first()) {
                Some(Cell::Text(n)) => n.trim().parse().unwrap_or(0),
                _ => 0,
            }
        };
        let mut g = Generator::new(plan, job.seed ^ existing, references).starting_at(existing);
        let batch = job.batch_rows.max(1) as u64;
        let mut done = 0;
        while done < job.rows {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            let n = batch.min(job.rows - done);
            let rows: Vec<Vec<Cell>> = (0..n).map(|_| g.row()).collect();
            let sql = copy::insert_sql(kind, &job.table, &columns, rows);
            run(&conn, db, &sql, 0, &cancel).await.map_err(|e| {
                if cancel.load(Ordering::Relaxed) {
                    e
                } else {
                    DbError::new(format!("rows {}–{}: {}", done + 1, done + n, e.message))
                }
            })?;
            done += n;
            report(Progress::Fraction(done as f32 / job.rows.max(1) as f32));
        }
        for st in &after {
            run(&conn, db, st, 0, &cancel).await?;
        }
        Ok::<_, DbError>(done)
    }
    .await;
    match result {
        Ok(n) => {
            if transaction {
                run(&conn, db, "COMMIT", 0, &cancel).await?;
            }
            report(Progress::Log(format!("Inserted {n} rows into {}", job.table.name)));
            Ok(())
        }
        Err(e) => {
            if transaction {
                let idle: Cancel = Default::default();
                if run(&conn, db, "ROLLBACK", 0, &idle).await.is_ok() {
                    report(Progress::Log("Rolled back".into()));
                }
            }
            Err(e)
        }
    }
}
