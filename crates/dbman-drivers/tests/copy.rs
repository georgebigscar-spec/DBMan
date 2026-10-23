//! Copies a table with every common kind of value from each engine to each
//! engine and checks the rows that arrive. SQLite and DuckDB always run; the
//! servers need the `DBMAN_TEST_*` variables (see `transfer.rs`).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use dbman_core::copy::{Generic, generic};
use dbman_core::sql::Dialect;
use dbman_core::{Cell, Connection, ConnectionConfig, DbKind, ObjectPath, StatementOutcome, dml};
use dbman_drivers::copy::{CopyJob, Existing, copy};

const ROWS: usize = 300;

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
    cfg.ssl_mode = if kind == DbKind::MsSql { dbman_core::SslMode::Prefer } else { dbman_core::SslMode::Disable };
    Some(cfg)
}

async fn exec(c: &Arc<dyn Connection>, db: &str, sql: &str) -> StatementOutcome {
    c.execute(Some(db), sql, usize::MAX / 2).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Where an engine keeps the source table and receives copies.
struct Place {
    kind: DbKind,
    cfg: ConnectionConfig,
    target_cfg: ConnectionConfig,
    db: String,
    src: String,
    dst: String,
    create: &'static str,
}

fn create_sql(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Redis => unreachable!("Redis has no tables"),
        DbKind::Postgres => {
            "CREATE TABLE {t} (id integer PRIMARY KEY, name varchar(50) NOT NULL, amount numeric(12,2), flag boolean, \
             created timestamp, note text, data bytea)"
        }
        DbKind::MySql => {
            "CREATE TABLE {t} (id int PRIMARY KEY, name varchar(50) NOT NULL, amount decimal(12,2), flag tinyint(1), \
             created datetime, note text, data blob)"
        }
        DbKind::Sqlite => {
            "CREATE TABLE {t} (id INTEGER PRIMARY KEY, name VARCHAR(50) NOT NULL, amount NUMERIC(12,2), flag BOOLEAN, \
             created TIMESTAMP, note TEXT, data BLOB)"
        }
        DbKind::DuckDb => {
            "CREATE TABLE {t} (id INTEGER PRIMARY KEY, name VARCHAR(50) NOT NULL, amount DECIMAL(12,2), flag BOOLEAN, \
             created TIMESTAMP, note VARCHAR, data BLOB)"
        }
        DbKind::MsSql => {
            "CREATE TABLE {t} (id int PRIMARY KEY, name nvarchar(50) NOT NULL, amount decimal(12,2), flag bit, \
             created datetime2, note nvarchar(max), data varbinary(max))"
        }
        DbKind::ClickHouse => {
            "CREATE TABLE {t} (id Int32, name String, amount Nullable(Decimal(12,2)), flag Nullable(Bool), \
             created Nullable(DateTime64(6)), note Nullable(String), data Nullable(String)) ENGINE = MergeTree ORDER BY id"
        }
    }
}

async fn places(dir: &std::path::Path) -> Vec<Place> {
    let mut out = Vec::new();
    let file = |kind: DbKind, name: &str| {
        let mut cfg = ConnectionConfig::new(kind);
        cfg.database = dir.join(name).to_string_lossy().into_owned();
        cfg
    };
    out.push(Place {
        kind: DbKind::Sqlite,
        cfg: file(DbKind::Sqlite, "src.db"),
        target_cfg: file(DbKind::Sqlite, "dst.db"),
        db: "main".into(),
        src: "main".into(),
        dst: "main".into(),
        create: create_sql(DbKind::Sqlite),
    });
    let duck = file(DbKind::DuckDb, "x.duckdb");
    let c = dbman_drivers::connect(&duck).await.unwrap();
    exec(&c, "x", "CREATE SCHEMA copy_src").await;
    exec(&c, "x", "CREATE SCHEMA copy_dst").await;
    drop(c);
    out.push(Place {
        kind: DbKind::DuckDb,
        cfg: duck.clone(),
        target_cfg: duck,
        db: "x".into(),
        src: "copy_src".into(),
        dst: "copy_dst".into(),
        create: create_sql(DbKind::DuckDb),
    });
    if let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") {
        let db = cfg.database.clone();
        let c = dbman_drivers::connect(&cfg).await.unwrap();
        for s in ["copy_src", "copy_dst"] {
            exec(&c, &db, &format!("DROP SCHEMA IF EXISTS {s} CASCADE")).await;
            exec(&c, &db, &format!("CREATE SCHEMA {s}")).await;
        }
        out.push(Place {
            kind: DbKind::Postgres,
            cfg: cfg.clone(),
            target_cfg: cfg,
            db,
            src: "copy_src".into(),
            dst: "copy_dst".into(),
            create: create_sql(DbKind::Postgres),
        });
    }
    if let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") {
        let base = cfg.database.clone();
        let c = dbman_drivers::connect(&cfg).await.unwrap();
        let (a, b) = (format!("{base}_copy_src"), format!("{base}_copy_dst"));
        for s in [&a, &b] {
            exec(&c, &base, &format!("DROP DATABASE IF EXISTS `{s}`")).await;
            exec(&c, &base, &format!("CREATE DATABASE `{s}`")).await;
        }
        let mut src = cfg.clone();
        src.database = a.clone();
        let mut dst = cfg;
        dst.database = b.clone();
        out.push(Place {
            kind: DbKind::MySql,
            cfg: src,
            target_cfg: dst,
            db: a.clone(),
            src: a,
            dst: b,
            create: create_sql(DbKind::MySql),
        });
    }
    if let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") {
        let db = cfg.database.clone();
        let mut master = cfg.clone();
        master.database = "master".into();
        let m = dbman_drivers::connect(&master).await.unwrap();
        exec(&m, "master", &format!("IF DB_ID(N'{db}') IS NULL CREATE DATABASE [{db}]")).await;
        let c = dbman_drivers::connect(&cfg).await.unwrap();
        for s in ["copy_src", "copy_dst"] {
            exec(&c, &db, &format!("DROP TABLE IF EXISTS {s}.copy_t")).await;
            exec(&c, &db, &format!("IF SCHEMA_ID(N'{s}') IS NULL EXEC(N'CREATE SCHEMA {s}')")).await;
        }
        out.push(Place {
            kind: DbKind::MsSql,
            cfg: cfg.clone(),
            target_cfg: cfg,
            db,
            src: "copy_src".into(),
            dst: "copy_dst".into(),
            create: create_sql(DbKind::MsSql),
        });
    }
    if let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") {
        let base = cfg.database.clone();
        let c = dbman_drivers::connect(&cfg).await.unwrap();
        for s in ["copy_src", "copy_dst"] {
            exec(&c, &base, &format!("DROP DATABASE IF EXISTS `{s}`")).await;
            exec(&c, &base, &format!("CREATE DATABASE `{s}`")).await;
        }
        out.push(Place {
            kind: DbKind::ClickHouse,
            cfg: cfg.clone(),
            target_cfg: cfg,
            db: "copy_src".into(),
            src: "copy_src".into(),
            dst: "copy_dst".into(),
            create: create_sql(DbKind::ClickHouse),
        });
    }
    out
}

/// Fills the source table with values of every kind, NULLs included.
async fn fill(p: &Place) -> Arc<dyn Connection> {
    let c = dbman_drivers::connect(&p.cfg).await.unwrap();
    let d = Dialect(p.kind);
    let t = d.qualified(&ObjectPath::new(&p.db, &p.src, "copy_t"));
    exec(&c, &p.db, &p.create.replace("{t}", &t)).await;
    let bool_lit = |b: bool| match p.kind {
        DbKind::Postgres | DbKind::DuckDb | DbKind::ClickHouse => b.to_string(),
        _ => u8::from(b).to_string(),
    };
    for chunk in (1..=ROWS).collect::<Vec<_>>().chunks(100) {
        let rows: Vec<String> = chunk
            .iter()
            .map(|&i| {
                let null = i % 7 == 0;
                let opt = |v: String| if null { "NULL".to_string() } else { v };
                format!(
                    "({i}, {}, {}, {}, {}, {}, {})",
                    d.string(&format!("name {i}")),
                    opt(format!("{}.{:02}", i * 3, i % 100)),
                    opt(bool_lit(i % 2 == 0)),
                    opt(format!("'2026-0{}-{:02} 0{}:15:30'", i % 9 + 1, i % 28 + 1, i % 10)),
                    opt(d.string(&format!("it's ünïcödé {i}"))),
                    opt(dml::literal(d, &Cell::Bytes(format!("ab{i}").into_bytes()), false)),
                )
            })
            .collect();
        exec(
            &c,
            &p.db,
            &format!("INSERT INTO {t} (id, name, amount, flag, created, note, data) VALUES {}", rows.join(", ")),
        )
        .await;
    }
    c
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Values made comparable across engines.
fn normalize(g: Generic, c: &Cell) -> String {
    let text = match c {
        Cell::Null => return "NULL".into(),
        Cell::Text(s) => s.clone(),
        Cell::Bytes(b) => return hex(b),
    };
    match g {
        Generic::Bool => {
            let on = matches!(text.to_ascii_lowercase().as_str(), "t" | "true" | "1");
            u8::from(on).to_string()
        }
        Generic::Int16
        | Generic::Int32
        | Generic::Int64
        | Generic::Decimal(_)
        | Generic::Float32
        | Generic::Float64 => {
            format!("{:.2}", text.parse::<f64>().unwrap_or(f64::NAN))
        }
        Generic::Timestamp | Generic::TimestampTz => text.replace('T', " ").chars().take(19).collect(),
        Generic::Binary => hex(text.as_bytes()),
        _ => text,
    }
}

/// The table's rows ordered by id, normalized by the source's column types.
async fn read(c: &Arc<dyn Connection>, db: &str, path: &ObjectPath, types: &[Generic]) -> Vec<Vec<String>> {
    let d = Dialect(c.kind());
    let sql = format!("SELECT id, name, amount, flag, created, note, data FROM {} ORDER BY id", d.qualified(path));
    let StatementOutcome::Rows(r) = exec(c, db, &sql).await else { panic!("no rows") };
    r.rows.iter().map(|row| row.iter().zip(types).map(|(c, g)| normalize(*g, c)).collect()).collect()
}

#[tokio::test]
async fn every_pair() {
    let dir = tempfile::tempdir().unwrap();
    let places = places(dir.path()).await;
    eprintln!("engines: {:?}", places.iter().map(|p| p.kind).collect::<Vec<_>>());
    for p in &places {
        let c = fill(p).await;
        let src = ObjectPath::new(&p.db, &p.src, "copy_t");
        let details = c.table_details(&src).await.unwrap();
        let types: Vec<Generic> = details.columns.iter().map(|c| generic(p.kind, &c.data_type)).collect();
        let expected = read(&c, &p.db, &src, &types).await;
        assert_eq!(expected.len(), ROWS);
        for q in &places {
            let target_db =
                if q.kind == DbKind::MySql || q.kind == DbKind::ClickHouse { q.dst.clone() } else { q.db.clone() };
            let job = CopyJob {
                source_db: p.db.clone(),
                source_schema: p.src.clone(),
                tables: vec!["copy_t".into()],
                target_db: target_db.clone(),
                target_schema: q.dst.clone(),
                existing: Existing::Recreate,
                batch_rows: 70,
            };
            let log = Arc::new(std::sync::Mutex::new(Vec::new()));
            let l = log.clone();
            let report: dbman_drivers::backup::Report = Arc::new(move |p| l.lock().unwrap().push(format!("{p:?}")));
            copy(p.cfg.clone(), q.target_cfg.clone(), job, report, Arc::new(AtomicBool::new(false)))
                .await
                .unwrap_or_else(|e| panic!("{:?} → {:?}: {e}\n{:#?}", p.kind, q.kind, log.lock().unwrap()));
            let t = dbman_drivers::connect(&q.target_cfg).await.unwrap();
            let got = read(&t, &target_db, &ObjectPath::new(&target_db, &q.dst, "copy_t"), &types).await;
            assert_eq!(got.len(), ROWS, "{:?} → {:?}", p.kind, q.kind);
            for (e, g) in expected.iter().zip(&got) {
                assert_eq!(e, g, "{:?} → {:?}", p.kind, q.kind);
            }
        }
        eprintln!("{:?} copied to every engine", p.kind);
    }
}

#[tokio::test]
async fn existing_tables_keep_or_replace_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("a.db").to_string_lossy().into_owned();
    let mut target = ConnectionConfig::new(DbKind::Sqlite);
    target.database = dir.path().join("b.db").to_string_lossy().into_owned();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    exec(&c, "main", "CREATE TABLE parent (id INTEGER PRIMARY KEY)").await;
    exec(&c, "main", "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id))").await;
    exec(&c, "main", "INSERT INTO parent VALUES (1), (2)").await;
    exec(&c, "main", "INSERT INTO child VALUES (10, 1), (11, 2)").await;
    let job = |existing| CopyJob {
        source_db: "main".into(),
        source_schema: "main".into(),
        // Children first: the copy orders them after their parents.
        tables: vec!["child".into(), "parent".into()],
        target_db: "main".into(),
        target_schema: "main".into(),
        existing,
        batch_rows: 500,
    };
    let quiet: dbman_drivers::backup::Report = Arc::new(|_| {});
    let no = || Arc::new(AtomicBool::new(false));
    copy(cfg.clone(), target.clone(), job(Existing::Append), quiet.clone(), no()).await.unwrap();
    let t = dbman_drivers::connect(&target).await.unwrap();
    let ddl = |sql: &str| sql.to_string();
    let StatementOutcome::Rows(r) = exec(&t, "main", &ddl("SELECT sql FROM sqlite_master WHERE name = 'child'")).await
    else {
        panic!()
    };
    assert!(format!("{:?}", r.rows).contains("REFERENCES"), "{:?}", r.rows);
    let count = |t: Arc<dyn Connection>| async move {
        let StatementOutcome::Rows(r) = exec(&t, "main", "SELECT count(*) FROM child").await else { panic!() };
        r.rows[0][0].clone()
    };
    assert!(
        copy(cfg.clone(), target.clone(), job(Existing::Append), quiet.clone(), no()).await.is_err(),
        "duplicate keys"
    );
    assert_eq!(count(t.clone()).await, Cell::Text("2".into()), "the failed table rolled back");
    copy(cfg.clone(), target.clone(), job(Existing::Replace), quiet.clone(), no()).await.unwrap();
    assert_eq!(count(t.clone()).await, Cell::Text("2".into()));
    copy(cfg, target, job(Existing::Recreate), quiet, no()).await.unwrap();
    assert_eq!(count(t).await, Cell::Text("2".into()));
}
