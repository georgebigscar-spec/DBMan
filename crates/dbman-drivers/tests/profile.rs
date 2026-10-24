//! Column profiles on every engine: counts, distinct values, the range and
//! average, the most frequent values and a histogram, under a filter. SQLite
//! and DuckDB always run; the servers need `DBMAN_TEST_POSTGRES`,
//! `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL` and `DBMAN_TEST_CLICKHOUSE`, and are
//! skipped otherwise.

use dbman_core::profile::{self, BUCKETS};
use dbman_core::{ConnectionConfig, DbKind, ObjectPath};

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

async fn check(cfg: ConnectionConfig, db: &str, schema: &str, create: &str) {
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let kind = c.kind();
    let path = ObjectPath::new(db, schema, "profile_t");
    let t = dbman_core::sql::Dialect(kind).qualified(&path);
    let _ = c.execute(Some(db), &format!("DROP TABLE {t}"), 0).await;
    c.execute(Some(db), &create.replace("{t}", &t), 0).await.unwrap();
    // Totals 1 to 30 and one NULL; names cycle through three cities.
    let cities = ["Berlin", "Paris", "Rome"];
    let mut values: Vec<String> = (1..=30).map(|i| format!("({i}, '{}', {i})", cities[i % 3])).collect();
    values.push("(31, 'Berlin', NULL)".into());
    let insert = format!("INSERT INTO {t} (id, name, total) VALUES {}", values.join(", "));
    c.execute(Some(db), &insert, 0).await.unwrap_or_else(|e| panic!("{insert}: {e}"));

    let p = profile::run(c.as_ref(), &path, "total", true, "").await.unwrap_or_else(|e| panic!("{kind:?}: {e}"));
    assert!(p.errors.is_empty(), "{kind:?}: {:?}", p.errors);
    assert_eq!((p.rows, p.non_null, p.distinct), (31, 30, Some(30)), "{kind:?}");
    let num = |s: &Option<String>| s.as_deref().and_then(|s| s.parse::<f64>().ok());
    assert_eq!((num(&p.min), num(&p.max)), (Some(1.0), Some(30.0)), "{kind:?}");
    assert!((p.avg.unwrap() - 15.5).abs() < 1e-6, "{kind:?}: {:?}", p.avg);
    assert_eq!(p.histogram.len(), BUCKETS, "{kind:?}");
    assert_eq!(p.histogram.iter().map(|b| b.2).sum::<i64>(), 30, "{kind:?}");
    assert_eq!(p.top.len(), 10, "{kind:?}");

    let p = profile::run(c.as_ref(), &path, "name", false, "id > 3").await.unwrap_or_else(|e| panic!("{kind:?}: {e}"));
    assert!(p.errors.is_empty(), "{kind:?}: {:?}", p.errors);
    assert_eq!((p.rows, p.distinct), (28, Some(3)), "{kind:?}");
    assert_eq!((p.min.as_deref(), p.max.as_deref()), (Some("Berlin"), Some("Rome")), "{kind:?}");
    assert_eq!(p.top[0], (Some("Berlin".to_string()), 10), "{kind:?}");
    assert!(p.histogram.is_empty() && p.avg.is_none(), "{kind:?}");
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
