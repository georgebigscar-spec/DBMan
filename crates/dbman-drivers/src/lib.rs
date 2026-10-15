//! Database drivers for DBMan.
//!
//! Each engine implements [`dbman_core::Connection`]. To add an engine, add a
//! [`DbKind`] variant in `dbman-core`, implement `Connection` in a new module
//! here and dispatch to it in [`connect`].

use std::sync::Arc;

use dbman_core::{Connection, ConnectionConfig, DbKind, DbResult};

pub mod backup;
pub mod duckdb;
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

/// Opens a new session for `cfg`. Must be called inside a tokio runtime.
pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Arc<dyn Connection>> {
    Ok(match cfg.kind {
        DbKind::Postgres => Arc::new(postgres::PgConnection::connect(cfg).await?),
        DbKind::MySql => Arc::new(mysql::MySqlConnection::connect(cfg).await?),
        DbKind::Sqlite => Arc::new(sqlite::SqliteConnection::connect(cfg).await?),
        DbKind::DuckDb => Arc::new(duckdb::DuckDbConnection::connect(cfg).await?),
        DbKind::MsSql => Arc::new(mssql::MsSqlConnection::connect(cfg).await?),
    })
}
