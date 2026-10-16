//! Transactions in a console session: the statement that opens one, then a
//! rollback that undoes the rows and a commit that keeps them, and what other
//! sessions see meanwhile. SQLite and DuckDB always run; the servers need
//! `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL` and `DBMAN_TEST_MSSQL`, and are
//! skipped with a note otherwise. ClickHouse has no transactions.

use std::sync::Arc;

use dbman_core::sql::{begin_statement, transaction_effect};
use dbman_core::{Cell, Connection, ConnectionConfig, DbKind, StatementOutcome};

fn from_url(kind: DbKind, var: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(var).ok().filter(|u| !u.is_empty())?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, addr) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, db) = addr.split_once('/').unwrap_or((addr, ""));
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, ""));
    let mut cfg = ConnectionConfig::new(kind);
    cfg.host = host.into();
    if let Ok(p) = port.parse() {
        cfg.port = p;
    }
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.database = db.into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    Some(cfg)
}

async fn exec(c: &Arc<dyn Connection>, db: &str, sql: &str) -> StatementOutcome {
    c.execute(Some(db), sql, 100).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn count(c: &Arc<dyn Connection>, db: &str) -> String {
    match exec(c, db, "SELECT COUNT(*) FROM tx_t").await {
        StatementOutcome::Rows(r) => match &r.rows[0][0] {
            Cell::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn check(cfg: ConnectionConfig, db: &str) {
    let kind = cfg.kind;
    let console = dbman_drivers::connect(&cfg).await.unwrap();
    let other = dbman_drivers::connect(&cfg).await.unwrap();
    let _ = console.execute(Some(db), "DROP TABLE tx_t", 0).await;
    exec(&console, db, "CREATE TABLE tx_t (id integer PRIMARY KEY)").await;
    let begin = begin_statement(kind).unwrap();
    assert_eq!(transaction_effect(kind, begin), Some(true));

    exec(&console, db, begin).await;
    exec(&console, db, "INSERT INTO tx_t (id) VALUES (1)").await;
    assert_eq!(count(&console, db).await, "1", "{kind:?}");
    // SQL Server's other sessions would wait for the row lock.
    if kind != DbKind::MsSql {
        assert_eq!(count(&other, db).await, "0", "{kind:?}: uncommitted rows are not visible elsewhere");
    }
    assert_eq!(transaction_effect(kind, "ROLLBACK"), Some(false));
    exec(&console, db, "ROLLBACK").await;
    assert_eq!(count(&console, db).await, "0", "{kind:?}");

    exec(&console, db, begin).await;
    exec(&console, db, "INSERT INTO tx_t (id) VALUES (2)").await;
    exec(&console, db, "COMMIT").await;
    assert_eq!(count(&other, db).await, "1", "{kind:?}");
    exec(&console, db, "DROP TABLE tx_t").await;
}

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("x.db").to_string_lossy().into_owned();
    check(cfg, "main").await;
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("x.duckdb").to_string_lossy().into_owned();
    check(cfg, "x").await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db).await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db).await;
}

#[tokio::test]
async fn mssql() {
    let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db).await;
}

#[test]
fn clickhouse_has_none() {
    assert_eq!(begin_statement(DbKind::ClickHouse), None);
}
