//! The dashboard's statements on every server: a reading has connections,
//! a throughput counter and a size, and the largest tables list the one made
//! here. The servers need `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`,
//! `DBMAN_TEST_MSSQL`, `DBMAN_TEST_CLICKHOUSE` and `DBMAN_TEST_REDIS`, and
//! are skipped otherwise.

use dbman_core::dashboard;
use dbman_core::{ConnectionConfig, DbKind, Product, StatementOutcome};

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

async fn check(cfg: ConnectionConfig, create: Option<&str>) {
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let kind = c.kind();
    if let Some(create) = create {
        let _ = c.execute(None, "DROP TABLE dashboard_t", 0).await;
        c.execute(None, create, 0).await.unwrap_or_else(|e| panic!("{create}: {e}"));
    }
    let mut results = Vec::new();
    for q in dashboard::queries(kind, Product::Native) {
        match c.execute(None, &q, 100).await.unwrap_or_else(|e| panic!("{kind:?}: {q}: {e}")) {
            StatementOutcome::Rows(r) => results.push(r),
            other => panic!("{other:?}"),
        }
    }
    let s = dashboard::read(kind, &results);
    assert!(s.connections.is_some_and(|n| n >= 1.0), "{kind:?}: {s:?}");
    assert!(s.active.is_some(), "{kind:?}: {s:?}");
    assert!(s.total_queries.is_some_and(|n| n > 0.0), "{kind:?}: {s:?}");
    assert!(s.size.is_some_and(|n| n > 0.0), "{kind:?}: {s:?}");
    if kind != DbKind::Redis {
        assert_eq!(s.slow, Some(0.0), "{kind:?}");
    }
    if let Some(sql) = dashboard::tables_sql(kind, Product::Native) {
        let StatementOutcome::Rows(r) = c.execute(None, sql, 10).await.unwrap_or_else(|e| panic!("{kind:?}: {e}"))
        else {
            panic!()
        };
        assert!(r.rows.iter().any(|row| row[0].display().ends_with("dashboard_t")), "{kind:?}: {:?}", r.rows);
    }
    if create.is_some() {
        c.execute(None, "DROP TABLE dashboard_t", 0).await.unwrap();
    }
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else { return };
    check(cfg, Some("CREATE TABLE dashboard_t AS SELECT g AS id FROM generate_series(1, 1000) g")).await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else { return };
    check(cfg, Some("CREATE TABLE dashboard_t (id int PRIMARY KEY)")).await;
}

#[tokio::test]
async fn mssql() {
    let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else { return };
    check(cfg, Some("CREATE TABLE dashboard_t (id int PRIMARY KEY)")).await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else { return };
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let _ = c.execute(None, "DROP TABLE IF EXISTS dashboard_t", 0).await;
    c.execute(None, "CREATE TABLE dashboard_t (id UInt32) ENGINE = MergeTree ORDER BY id", 0).await.unwrap();
    c.execute(None, "INSERT INTO dashboard_t SELECT number FROM numbers(1000)", 0).await.unwrap();
    check(cfg, None).await;
    c.execute(None, "DROP TABLE dashboard_t", 0).await.unwrap();
}

#[tokio::test]
async fn redis() {
    let Some(mut cfg) = from_url(DbKind::Redis, "DBMAN_TEST_REDIS") else { return };
    cfg.database = "db0".into();
    if cfg.port == 0 {
        cfg.port = 6379;
    }
    check(cfg, None).await;
}
