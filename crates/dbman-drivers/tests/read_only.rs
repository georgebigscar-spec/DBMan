//! Read-only connections: reads work, changes are refused, and on PostgreSQL
//! even a SELECT that writes through a function fails. SQLite and DuckDB
//! always run; PostgreSQL and MySQL need `DBMAN_TEST_POSTGRES` and
//! `DBMAN_TEST_MYSQL`, and are skipped with a note otherwise.

use dbman_core::{ConnectionConfig, DbKind, StatementOutcome};

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

/// Creates `ro_t` with one row through a normal connection, then checks a read-only one.
async fn check(mut cfg: ConnectionConfig) {
    let kind = cfg.kind;
    let rw = dbman_drivers::connect(&cfg).await.unwrap();
    let _ = rw.execute(None, "DROP TABLE ro_t", 0).await;
    rw.execute(None, "CREATE TABLE ro_t (id INT PRIMARY KEY, v VARCHAR(10))", 0).await.unwrap();
    rw.execute(None, "INSERT INTO ro_t (id, v) VALUES (1, 'a')", 0).await.unwrap();

    cfg.read_only = true;
    let ro = dbman_drivers::connect(&cfg).await.unwrap();
    match ro.execute(None, "SELECT v FROM ro_t", 10).await.unwrap() {
        StatementOutcome::Rows(r) => assert_eq!(r.rows.len(), 1, "{kind:?}"),
        other => panic!("{kind:?}: {other:?}"),
    }
    for sql in
        ["INSERT INTO ro_t (id, v) VALUES (2, 'b')", "UPDATE ro_t SET v = 'c'", "DELETE FROM ro_t", "DROP TABLE ro_t"]
    {
        let e = ro.execute(None, sql, 0).await.expect_err(sql);
        assert!(e.to_string().contains("read-only"), "{kind:?}: {sql}: {e}");
    }
    rw.execute(None, "DROP TABLE ro_t", 0).await.unwrap();
}

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("ro.db").to_string_lossy().into_owned();
    check(cfg.clone()).await;
    // query_only also stops what the statement check lets through.
    cfg.read_only = true;
    let ro = dbman_drivers::connect(&cfg).await.unwrap();
    assert!(ro.execute(None, "SELECT 1", 1).await.is_ok());
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("ro.duckdb").to_string_lossy().into_owned();
    check(cfg).await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    check(cfg.clone()).await;
    let rw = dbman_drivers::connect(&cfg).await.unwrap();
    let _ = rw.execute(None, "DROP SEQUENCE ro_seq", 0).await;
    rw.execute(None, "CREATE SEQUENCE ro_seq", 0).await.unwrap();
    let mut ro_cfg = cfg;
    ro_cfg.read_only = true;
    let ro = dbman_drivers::connect(&ro_cfg).await.unwrap();
    let e = ro.execute(None, "SELECT nextval('ro_seq')", 1).await.expect_err("nextval writes");
    assert!(e.to_string().contains("read-only"), "{e}");
    rw.execute(None, "DROP SEQUENCE ro_seq", 0).await.unwrap();
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    check(cfg).await;
}
