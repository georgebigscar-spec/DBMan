//! Execution plans from every engine: a join of a table with itself is
//! explained, the plan must name the table, and an explained INSERT must not
//! run unless the plan is analyzed. SQLite and DuckDB always run; the servers
//! need `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL` and
//! `DBMAN_TEST_CLICKHOUSE`, `DBMAN_TEST_COCKROACH` and `DBMAN_TEST_TIDB`, and
//! are skipped with a note otherwise.

use std::sync::Arc;

use dbman_core::explain::{Plan, supports_analyze};
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

async fn count(c: &Arc<dyn Connection>, db: &str, table: &str) -> String {
    match exec(c, db, &format!("SELECT COUNT(*) FROM {table}")).await {
        StatementOutcome::Rows(r) => match &r.rows[0][0] {
            Cell::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn mentions(p: &Plan, word: &str) -> bool {
    p.rows().iter().any(|(_, n)| {
        n.operation.to_lowercase().contains(word) || n.details.iter().any(|d| d.to_lowercase().contains(word))
    })
}

async fn check(cfg: ConnectionConfig, db: &str, create: &str) {
    let (kind, product) = (cfg.kind, cfg.product);
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let _ = c.execute(Some(db), "DROP TABLE plan_t", 0).await;
    exec(&c, db, create).await;
    exec(&c, db, "INSERT INTO plan_t (id, v) VALUES (1, 'x'), (2, 'y')").await;

    let plan =
        dbman_drivers::explain(&c, Some(db), kind, product, "SELECT * FROM plan_t WHERE v = 'x'", false).await.unwrap();
    assert!(mentions(&plan, "plan_t"), "{kind:?}: {plan:#?}");
    assert!(!plan.raw.is_empty());
    let sql = "SELECT a.id, b.v FROM plan_t a JOIN plan_t b ON a.id = b.id WHERE a.v = 'x';";
    let plan = dbman_drivers::explain(&c, Some(db), kind, product, sql, false).await.unwrap();
    assert!(plan.rows().len() >= 2, "{kind:?}: {plan:#?}");

    // Explaining does not run the statement.
    let insert = "INSERT INTO plan_t (id, v) VALUES (3, 'z')";
    if kind != DbKind::ClickHouse {
        dbman_drivers::explain(&c, Some(db), kind, product, insert, false).await.unwrap();
        assert_eq!(count(&c, db, "plan_t").await, "2", "{kind:?}");
    }
    // A failed plan leaves the session usable (SHOWPLAN is off again on SQL Server).
    assert!(dbman_drivers::explain(&c, Some(db), kind, product, "SELECT * FROM no_such_table", false).await.is_err());
    assert_eq!(count(&c, db, "plan_t").await, "2", "{kind:?}");

    if supports_analyze(kind) {
        let plan = dbman_drivers::explain(&c, Some(db), kind, product, sql, true).await.unwrap();
        assert!(plan.rows().iter().any(|(_, n)| n.actual_ms.is_some()), "{kind:?}: {plan:#?}");
        assert!(plan.rows().iter().any(|(_, n)| n.actual_rows.is_some()), "{kind:?}: {plan:#?}");
        assert!(!plan.totals.is_empty(), "{kind:?}");
    }
    exec(&c, db, "DROP TABLE plan_t").await;
}

const CREATE: &str = "CREATE TABLE plan_t (id integer PRIMARY KEY, v varchar(10))";

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("x.db").to_string_lossy().into_owned();
    check(cfg, "main", CREATE).await;
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("x.duckdb").to_string_lossy().into_owned();
    check(cfg, "x", CREATE).await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, CREATE).await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, CREATE).await;
}

#[tokio::test]
async fn cockroach() {
    let Some(mut cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_COCKROACH") else {
        eprintln!("DBMAN_TEST_COCKROACH not set, skipping");
        return;
    };
    cfg.product = dbman_core::Product::CockroachDb;
    let db = cfg.database.clone();
    check(cfg, &db, CREATE).await;
}

#[tokio::test]
async fn tidb() {
    let Some(mut cfg) = from_url(DbKind::MySql, "DBMAN_TEST_TIDB") else {
        eprintln!("DBMAN_TEST_TIDB not set, skipping");
        return;
    };
    cfg.product = dbman_core::Product::TiDb;
    let db = cfg.database.clone();
    check(cfg, &db, CREATE).await;
}

#[tokio::test]
async fn mssql() {
    let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, CREATE).await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, "CREATE TABLE plan_t (id UInt32, v String) ENGINE = MergeTree ORDER BY id").await;
}
