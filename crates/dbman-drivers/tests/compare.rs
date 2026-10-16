//! Schema and data comparison: two schemas that differ are compared, the sync
//! script runs on the target, and a second comparison finds nothing. SQLite and
//! DuckDB always run; the servers need the `DBMAN_TEST_*` variables (see
//! `transfer.rs`) and are skipped with a note otherwise.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dbman_core::compare::{self, DataSpec, Snapshot, Status};
use dbman_core::{Connection, ConnectionConfig, DbKind, ObjectPath, StatementOutcome};
use dbman_drivers::backup::Report;
use dbman_drivers::compare::{compare_data, run_script, snapshot};

fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn quiet() -> Report {
    Arc::new(|_| {})
}

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

/// One side of the comparison: a connection and a schema.
struct Side {
    cfg: ConnectionConfig,
    conn: Arc<dyn Connection>,
    db: String,
    schema: String,
}

impl Side {
    async fn new(cfg: ConnectionConfig, db: &str, schema: &str) -> Self {
        let conn = dbman_drivers::connect(&cfg).await.unwrap();
        Self { cfg, conn, db: db.into(), schema: schema.into() }
    }

    /// Runs statements with `{s}` replaced by the schema.
    async fn run(&self, statements: &[&str]) {
        for st in statements {
            exec(&self.conn, &self.db, &st.replace("{s}", &self.schema)).await;
        }
    }

    async fn snapshot(&self) -> Snapshot {
        snapshot(&self.conn, &self.db, &self.schema, true, |_, _| {}, &no_cancel()).await.unwrap()
    }

    fn path(&self, name: &str) -> ObjectPath {
        ObjectPath::new(&self.db, &self.schema, name)
    }
}

struct Sql<'a> {
    /// Source schema, tables first.
    source: &'a [&'a str],
    target: &'a [&'a str],
    /// Rows: `{s}`, then the id, name and email of each row.
    insert: &'a str,
}

async fn check(kind: DbKind, src: Side, dst: Side, sql: Sql<'_>) {
    src.run(sql.source).await;
    dst.run(sql.target).await;

    let diffs = compare::compare(kind, &src.snapshot().await, &dst.snapshot().await);
    let status = |n: &str| diffs.iter().find(|d| d.name == n).map(|d| d.status);
    assert_eq!(status("customers"), Some(Status::Changed), "{kind:?}: {diffs:#?}");
    assert_eq!(status("orders"), Some(Status::OnlySource), "{kind:?}");
    assert_eq!(status("legacy"), Some(Status::OnlyTarget), "{kind:?}");
    assert_eq!(status("v_orders"), Some(Status::OnlySource), "{kind:?}");

    let chosen: Vec<_> = diffs.iter().filter(|d| d.status != Status::Same).collect();
    let plan = compare::sync_plan(&chosen);
    run_script(
        dst.cfg.clone(),
        Some(dst.db.clone()),
        plan.statements.clone(),
        compare::transactional_ddl(kind),
        quiet(),
        no_cancel(),
    )
    .await
    .unwrap_or_else(|e| panic!("{kind:?} schema sync: {e}\n{}", compare::script(kind, &plan)));

    let again = compare::compare(kind, &src.snapshot().await, &dst.snapshot().await);
    let left: Vec<_> = again.iter().filter(|d| d.status != Status::Same).collect();
    assert!(left.is_empty(), "{kind:?} still differs after the sync:\n{left:#?}\n{}", compare::script(kind, &plan));

    // Rows: 50 in the source; the target has 40 of them without email and 5 others.
    for i in 1..=50 {
        let st = sql.insert.replace("{s}", &src.schema);
        exec(&src.conn, &src.db, &format!("{st} ({i}, 'name {i}', 'e{i}@x.org')")).await;
    }
    for i in (1..=40).chain(100..105) {
        let st = sql.insert.replace("{s}", &dst.schema);
        exec(&dst.conn, &dst.db, &format!("{st} ({i}, 'name {i}', NULL)")).await;
    }
    let (sp, tp) = (src.path("customers"), dst.path("customers"));
    let sd = src.conn.table_details(&sp).await.unwrap();
    let td = dst.conn.table_details(&tp).await.unwrap();
    let spec = DataSpec::new(tp.clone(), &sd, &td).unwrap();
    let diff = compare_data(&src.conn, &sp, &dst.conn, spec.clone(), &|_| {}, &no_cancel()).await.unwrap();
    assert_eq!((diff.inserted, diff.updated, diff.deleted, diff.same), (10, 40, 5, 0), "{kind:?}");
    let plan = compare::data_plan(&[&diff]);
    run_script(
        dst.cfg.clone(),
        Some(dst.db.clone()),
        plan.statements,
        kind != DbKind::ClickHouse,
        quiet(),
        no_cancel(),
    )
    .await
    .unwrap_or_else(|e| panic!("{kind:?} data sync: {e}"));
    let diff = compare_data(&src.conn, &sp, &dst.conn, spec, &|_| {}, &no_cancel()).await.unwrap();
    assert!(!diff.differs(), "{kind:?}: {diff:?}");
    assert_eq!(diff.same, 50, "{kind:?}");
}

const ANSI_SOURCE: &[&str] = &[
    "CREATE TABLE {s}.customers (id integer PRIMARY KEY, name varchar(100) NOT NULL, email varchar(200))",
    "CREATE TABLE {s}.orders (id integer PRIMARY KEY, customer_id integer NOT NULL REFERENCES {s}.customers (id), total numeric(10,2))",
    "CREATE INDEX orders_customer ON {s}.orders (customer_id)",
    "CREATE VIEW {s}.v_orders AS SELECT o.id, c.name FROM {s}.orders o JOIN {s}.customers c ON c.id = o.customer_id",
];

const ANSI_TARGET: &[&str] = &[
    "CREATE TABLE {s}.customers (id integer PRIMARY KEY, name varchar(100) NOT NULL)",
    "CREATE TABLE {s}.legacy (id integer PRIMARY KEY)",
];

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let side = |name: &str| {
        let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
        cfg.database = dir.path().join(name).to_string_lossy().into_owned();
        cfg
    };
    let source: Vec<String> = ANSI_SOURCE.iter().map(|s| s.replace("{s}.", "")).collect();
    let target: Vec<String> = ANSI_TARGET.iter().map(|s| s.replace("{s}.", "")).collect();
    let source: Vec<&str> = source.iter().map(String::as_str).collect();
    let target: Vec<&str> = target.iter().map(String::as_str).collect();
    check(
        DbKind::Sqlite,
        Side::new(side("a.db"), "main", "main").await,
        Side::new(side("b.db"), "main", "main").await,
        Sql { source: &source, target: &target, insert: "INSERT INTO customers (id, name, email) VALUES" },
    )
    .await;
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("x.duckdb").to_string_lossy().into_owned();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    exec(&c, "x", "CREATE SCHEMA cmp_src").await;
    exec(&c, "x", "CREATE SCHEMA cmp_dst").await;
    drop(c);
    check(
        DbKind::DuckDb,
        Side::new(cfg.clone(), "x", "cmp_src").await,
        Side::new(cfg, "x", "cmp_dst").await,
        Sql { source: ANSI_SOURCE, target: ANSI_TARGET, insert: "INSERT INTO {s}.customers (id, name, email) VALUES" },
    )
    .await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    for s in ["cmp_src", "cmp_dst"] {
        exec(&c, &db, &format!("DROP SCHEMA IF EXISTS {s} CASCADE")).await;
        exec(&c, &db, &format!("CREATE SCHEMA {s}")).await;
    }
    check(
        DbKind::Postgres,
        Side::new(cfg.clone(), &db, "cmp_src").await,
        Side::new(cfg, &db, "cmp_dst").await,
        Sql { source: ANSI_SOURCE, target: ANSI_TARGET, insert: "INSERT INTO {s}.customers (id, name, email) VALUES" },
    )
    .await;
}

#[tokio::test]
async fn mssql() {
    let Some(mut cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    cfg.ssl_mode = dbman_core::SslMode::Prefer;
    let db = cfg.database.clone();
    let mut master_cfg = cfg.clone();
    master_cfg.database = "master".into();
    let master = dbman_drivers::connect(&master_cfg).await.unwrap();
    exec(&master, "master", &format!("IF DB_ID(N'{db}') IS NULL CREATE DATABASE [{db}]")).await;
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    for s in ["cmp_src", "cmp_dst"] {
        for st in [
            format!("DROP VIEW IF EXISTS {s}.v_orders"),
            format!("DROP TABLE IF EXISTS {s}.orders"),
            format!("DROP TABLE IF EXISTS {s}.customers"),
            format!("DROP TABLE IF EXISTS {s}.legacy"),
            format!("IF SCHEMA_ID(N'{s}') IS NULL EXEC(N'CREATE SCHEMA {s}')"),
        ] {
            exec(&c, &db, &st).await;
        }
    }
    check(
        DbKind::MsSql,
        Side::new(cfg.clone(), &db, "cmp_src").await,
        Side::new(cfg, &db, "cmp_dst").await,
        Sql { source: ANSI_SOURCE, target: ANSI_TARGET, insert: "INSERT INTO {s}.customers (id, name, email) VALUES" },
    )
    .await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let (a, b) = (format!("{db}_cmp_src"), format!("{db}_cmp_dst"));
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    for s in [&a, &b] {
        exec(&c, &db, &format!("DROP DATABASE IF EXISTS `{s}`")).await;
        exec(&c, &db, &format!("CREATE DATABASE `{s}`")).await;
    }
    let side = |s: &str| {
        let mut cfg = cfg.clone();
        cfg.database = s.into();
        cfg
    };
    check(
        DbKind::MySql,
        Side::new(side(&a), &a, &a).await,
        Side::new(side(&b), &b, &b).await,
        Sql { source: ANSI_SOURCE, target: ANSI_TARGET, insert: "INSERT INTO {s}.customers (id, name, email) VALUES" },
    )
    .await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let (a, b) = ("cmp_src", "cmp_dst");
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    for s in [a, b] {
        exec(&c, &db, &format!("DROP DATABASE IF EXISTS `{s}`")).await;
        exec(&c, &db, &format!("CREATE DATABASE `{s}`")).await;
    }
    check(
        DbKind::ClickHouse,
        Side::new(cfg.clone(), a, a).await,
        Side::new(cfg, b, b).await,
        Sql {
            source: &[
                "CREATE TABLE {s}.customers (id UInt32, name String, email Nullable(String)) ENGINE = MergeTree ORDER BY id",
                "CREATE TABLE {s}.orders (id UInt32, customer_id UInt32, total Decimal(10,2)) ENGINE = MergeTree ORDER BY id",
                "CREATE VIEW {s}.v_orders AS SELECT o.id, c.name FROM {s}.orders o JOIN {s}.customers c ON c.id = o.customer_id",
            ],
            target: &[
                "CREATE TABLE {s}.customers (id UInt32, name String) ENGINE = MergeTree ORDER BY id",
                "CREATE TABLE {s}.legacy (id UInt32) ENGINE = MergeTree ORDER BY id",
            ],
            insert: "INSERT INTO {s}.customers (id, name, email) VALUES",
        },
    )
    .await;
}
