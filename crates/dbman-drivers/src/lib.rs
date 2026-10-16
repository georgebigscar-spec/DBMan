//! Database drivers for DBMan.
//!
//! Each engine implements [`dbman_core::Connection`]. To add an engine, add a
//! [`DbKind`] variant in `dbman-core`, implement `Connection` in a new module
//! here and dispatch to it in [`connect`].

use std::sync::Arc;

use dbman_core::{Connection, ConnectionConfig, DbKind, DbResult};

pub mod backup;
pub mod clickhouse;
pub mod compare;
pub mod copy;
pub mod duckdb;
pub mod files;
pub mod generate;
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;
pub mod transfer;

/// Opens a new session for `cfg`. Must be called inside a tokio runtime.
pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Arc<dyn Connection>> {
    Ok(match cfg.kind {
        DbKind::Postgres => Arc::new(postgres::PgConnection::connect(cfg).await?),
        DbKind::MySql => Arc::new(mysql::MySqlConnection::connect(cfg).await?),
        DbKind::Sqlite => Arc::new(sqlite::SqliteConnection::connect(cfg).await?),
        DbKind::DuckDb => Arc::new(duckdb::DuckDbConnection::connect(cfg).await?),
        DbKind::MsSql => Arc::new(mssql::MsSqlConnection::connect(cfg).await?),
        DbKind::ClickHouse => Arc::new(clickhouse::ClickHouseConnection::connect(cfg).await?),
    })
}

/// The execution plan of a statement, asked on `conn`'s session. With `analyze`
/// (where the engine supports it) the statement runs and the plan has the
/// actual times and rows.
pub async fn explain(
    conn: &Arc<dyn Connection>,
    database: Option<&str>,
    kind: DbKind,
    sql: &str,
    analyze: bool,
) -> DbResult<dbman_core::explain::Plan> {
    let mut plan = None;
    let mut failed = None;
    let stmts = dbman_core::explain::statements(kind, sql, analyze);
    let last = stmts.len() - 1;
    for (i, s) in stmts.iter().enumerate() {
        // After an error only the last statement runs: it switches SQL Server's SHOWPLAN off again.
        if failed.is_some() && i != last {
            continue;
        }
        match conn.execute(database, s, usize::MAX / 2).await {
            Ok(dbman_core::StatementOutcome::Rows(r)) if !r.columns.is_empty() => plan = Some(r),
            Ok(_) => {}
            Err(e) => {
                failed.get_or_insert(e);
            }
        }
    }
    if let Some(e) = failed {
        return Err(e);
    }
    let r = plan.ok_or_else(|| dbman_core::DbError::new("The server returned no plan."))?;
    dbman_core::explain::parse(kind, &r).map_err(dbman_core::DbError::new)
}
