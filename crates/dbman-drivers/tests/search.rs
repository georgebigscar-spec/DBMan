//! Searching values in rows on every engine: the filter matches text and
//! numbers, ignores case where the engine can, treats `%` and `_` as plain
//! characters, and the count stops at its cap. SQLite and DuckDB always run;
//! the servers need `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`,
//! `DBMAN_TEST_MSSQL` and `DBMAN_TEST_CLICKHOUSE`, and are skipped otherwise.

use std::sync::Arc;

use dbman_core::search::{count_sql, value_filter};
use dbman_core::{Cell, Connection, ConnectionConfig, DbKind, ObjectPath, StatementOutcome};

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

async fn count(c: &Arc<dyn Connection>, db: &str, path: &ObjectPath, needle: &str, cap: usize) -> usize {
    let filter = value_filter(c.kind(), &["name".into(), "total".into()], needle);
    let sql = count_sql(c.kind(), path, &filter, cap);
    match c.execute(Some(db), &sql, 1).await.unwrap_or_else(|e| panic!("{sql}: {e}")) {
        StatementOutcome::Rows(r) => match &r.rows[0][0] {
            Cell::Text(t) => t.parse().unwrap(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

async fn check(cfg: ConnectionConfig, db: &str, schema: &str, create: &str) {
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let kind = c.kind();
    let path = ObjectPath::new(db, schema, "search_t");
    let t = dbman_core::sql::Dialect(kind).qualified(&path);
    let _ = c.execute(Some(db), &format!("DROP TABLE {t}"), 0).await;
    c.execute(Some(db), &create.replace("{t}", &t), 0).await.unwrap();
    let insert = format!(
        "INSERT INTO {t} (id, name, total) VALUES (1, 'Ada Lovelace', 12.5), (2, '50% off', 7), \
         (3, 'a_b', 125), (4, 'Grace', NULL)"
    );
    c.execute(Some(db), &insert, 0).await.unwrap_or_else(|e| panic!("{insert}: {e}"));

    assert_eq!(count(&c, db, &path, "lovelace", 100).await, 1, "{kind:?}: case is ignored");
    assert_eq!(count(&c, db, &path, "12", 100).await, 2, "{kind:?}: numbers as text");
    assert_eq!(count(&c, db, &path, "50%", 100).await, 1, "{kind:?}: % is a character");
    assert_eq!(count(&c, db, &path, "a_b", 100).await, 1, "{kind:?}: _ is a character");
    assert_eq!(count(&c, db, &path, "zzz", 100).await, 0, "{kind:?}");
    assert_eq!(count(&c, db, &path, "a", 1).await, 2, "{kind:?}: the count stops past the cap");
    c.execute(Some(db), &format!("DROP TABLE {t}"), 0).await.unwrap();
}

const CREATE: &str = "CREATE TABLE {t} (id integer PRIMARY KEY, name varchar(50), total numeric(10, 2))";

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("s.db").to_string_lossy().into_owned();
    check(cfg, "main", "main", "CREATE TABLE {t} (id INTEGER PRIMARY KEY, name TEXT, total REAL)").await;
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("s.duckdb").to_string_lossy().into_owned();
    check(cfg, "s", "main", CREATE).await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, "public", CREATE).await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, &db, CREATE).await;
}

#[tokio::test]
async fn mssql() {
    let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(cfg, &db, "dbo", "CREATE TABLE {t} (id int PRIMARY KEY, name nvarchar(50), total decimal(10, 2))").await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    check(
        cfg,
        &db,
        &db,
        "CREATE TABLE {t} (id UInt32, name String, total Nullable(Decimal(10, 2))) ENGINE = MergeTree ORDER BY id",
    )
    .await;
}
