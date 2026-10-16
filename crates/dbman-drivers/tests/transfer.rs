//! Export → import round trips: a generated CSV is imported into a table, the
//! table is exported in each format and imported into a second table, whose
//! CSV export must match the first one. SQLite and DuckDB always run; the
//! servers need `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL`
//! and `DBMAN_TEST_CLICKHOUSE`, and are skipped with a note otherwise.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use dbman_core::export::Format;
use dbman_core::import::{self, ImportFormat};
use dbman_core::{Cell, Connection, ConnectionConfig, DbKind, ObjectPath, StatementOutcome};
use dbman_drivers::backup::{Progress, Report};
use dbman_drivers::transfer::{ExportJob, ExportSource, ImportJob, export, import};

const ROWS: usize = 12_345;

fn collect() -> (Report, Arc<Mutex<Vec<Progress>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (Arc::new(move |p| s.lock().unwrap().push(p)), seen)
}

fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
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

async fn scalar(c: &Arc<dyn Connection>, db: &str, sql: &str) -> String {
    match exec(c, db, sql).await {
        StatementOutcome::Rows(r) => match &r.rows[0][0] {
            Cell::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Rows with quotes, separators, line breaks, non-ASCII text, NULLs and bytes.
fn source_csv(path: &Path) {
    let mut s = String::from("id,name,amount,note,data\n");
    for i in 1..=ROWS {
        let note = match i % 4 {
            0 => String::new(),
            1 => format!("\"line one\nline {i}\""),
            2 => "\"a, \"\"quoted\"\" value\"".into(),
            _ => "ünïcödé текст".into(),
        };
        let data = if i % 3 == 0 { String::new() } else { format!("0x{:04X}FF", i) };
        s.push_str(&format!("{i},name {i},{}.{:02},{note},{data}\n", i % 1000, i % 100));
    }
    std::fs::write(path, s).unwrap();
}

fn import_job(table: &ObjectPath, path: &Path, details: &dbman_core::TableDetails, kind: DbKind) -> ImportJob {
    let (format, csv) = import::detect(path);
    let mut source = import::open(path, format, &csv, Default::default()).unwrap();
    let columns = import::auto_map(&details.columns, source.columns(), format == ImportFormat::Json || csv.header);
    let _ = source.next_record();
    ImportJob {
        table: table.clone(),
        path: path.to_path_buf(),
        format,
        csv,
        columns,
        empty_as_null: true,
        clear_first: true,
        transaction: true,
        batch_rows: import::default_batch(kind),
    }
}

async fn export_to(cfg: &ConnectionConfig, table: &ObjectPath, format: Format, path: &Path) {
    let job =
        ExportJob { source: ExportSource::Table(table.clone()), format, path: path.to_path_buf(), insert_into: None };
    let (report, seen) = collect();
    export(cfg.clone(), job, report, no_cancel()).await.unwrap_or_else(|e| panic!("export {format:?}: {e}"));
    let seen = seen.lock().unwrap();
    assert!(seen.contains(&Progress::Log(format!("Wrote {ROWS} rows to {}", path.display()))), "{seen:?}");
}

async fn round_trip(cfg: ConnectionConfig, db: &str, schema: &str, create: &str) {
    let kind = cfg.kind;
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let src = ObjectPath::new(db, schema, "xfer_src");
    let dst = ObjectPath::new(db, schema, "xfer_dst");
    let d = dbman_core::sql::Dialect(kind);
    for t in [&src, &dst] {
        exec(&c, db, &format!("DROP TABLE IF EXISTS {}", d.qualified(t))).await;
        exec(&c, db, &create.replace("{t}", &d.qualified(t))).await;
    }
    let details = c.table_details(&src).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    // CSV → table.
    let input = dir.path().join("input.csv");
    source_csv(&input);
    let (report, _) = collect();
    import(cfg.clone(), import_job(&src, &input, &details, kind), report, no_cancel())
        .await
        .unwrap_or_else(|e| panic!("{kind:?} import: {e}"));
    assert_eq!(scalar(&c, db, &format!("SELECT COUNT(*) FROM {}", d.qualified(&src))).await, ROWS.to_string());
    if matches!(kind, DbKind::Postgres | DbKind::MsSql | DbKind::MySql) {
        // The identity / sequence moved past the imported ids.
        exec(&c, db, &format!("INSERT INTO {} (name) VALUES ('next')", d.qualified(&src))).await;
        exec(&c, db, &format!("DELETE FROM {} WHERE name = 'next'", d.qualified(&src))).await;
    }

    let expected = dir.path().join("expected.csv");
    export_to(&cfg, &src, Format::Csv, &expected).await;
    let expected_text = std::fs::read_to_string(&expected).unwrap();
    assert!(expected_text.contains("\"line one\nline 1\""), "{kind:?}");

    for format in [Format::Csv, Format::Tsv, Format::Json, Format::JsonLines] {
        let file = dir.path().join(format!("out.{}", format.extension()));
        export_to(&cfg, &src, format, &file).await;
        let (report, _) = collect();
        import(cfg.clone(), import_job(&dst, &file, &details, kind), report, no_cancel())
            .await
            .unwrap_or_else(|e| panic!("{kind:?} import {format:?}: {e}"));
        let again = dir.path().join(format!("again-{}.csv", format.extension()));
        export_to(&cfg, &dst, Format::Csv, &again).await;
        let got = std::fs::read_to_string(&again).unwrap();
        if format == Format::Tsv {
            // TSV has no quoting: line breaks inside values become spaces.
            assert_eq!(got.lines().count(), ROWS + 1, "{kind:?}");
            assert!(got.contains(",line one line 1,"), "{kind:?}");
        } else {
            assert!(got == expected_text, "{kind:?} {format:?} differs");
        }
    }

    // A failing batch rolls the whole import back.
    let bad = dir.path().join("bad.csv");
    std::fs::write(&bad, "id,name\n900001,ok\n900002,ok\nnot a number,x\n").unwrap();
    let mut job = import_job(&dst, &bad, &details, kind);
    job.clear_first = false;
    job.batch_rows = 2;
    let (report, _) = collect();
    let err = import(cfg.clone(), job, report, no_cancel()).await.unwrap_err();
    assert!(err.message.starts_with("Rows 3–3: "), "{kind:?}: {}", err.message);
    let left = scalar(&c, db, &format!("SELECT COUNT(*) FROM {}", d.qualified(&dst))).await;
    if kind == DbKind::ClickHouse {
        assert_eq!(left, (ROWS + 2).to_string());
    } else {
        assert_eq!(left, ROWS.to_string(), "{kind:?}");
    }

    // A query export, cancelled before it starts, leaves no file.
    let file = dir.path().join("cancelled.csv");
    let job = ExportJob {
        source: ExportSource::Query { database: Some(db.into()), sql: format!("SELECT * FROM {}", d.qualified(&src)) },
        format: Format::Csv,
        path: file.clone(),
        insert_into: None,
    };
    let (report, _) = collect();
    assert!(export(cfg.clone(), job, report, Arc::new(AtomicBool::new(true))).await.is_err());
    assert!(!file.exists());

    for t in [&src, &dst] {
        exec(&c, db, &format!("DROP TABLE {}", d.qualified(t))).await;
    }
}

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("x.db").to_string_lossy().into_owned();
    round_trip(
        cfg,
        "main",
        "",
        "CREATE TABLE {t} (id INTEGER PRIMARY KEY, name TEXT NOT NULL, amount NUMERIC, note TEXT, data BLOB)",
    )
    .await;
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("x.duckdb").to_string_lossy().into_owned();
    round_trip(
        cfg,
        "x",
        "main",
        "CREATE TABLE {t} (id INTEGER PRIMARY KEY, name VARCHAR NOT NULL, amount DECIMAL(12,2), note VARCHAR, data BLOB)",
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
    round_trip(
        cfg,
        &db,
        "public",
        "CREATE TABLE {t} (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text NOT NULL, \
         amount numeric(12,2), note text, data bytea)",
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
    round_trip(
        cfg,
        &db,
        &db,
        "CREATE TABLE {t} (id int AUTO_INCREMENT PRIMARY KEY, name varchar(100) NOT NULL, amount decimal(12,2), \
         note text, data blob)",
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
    round_trip(
        cfg,
        &db,
        "dbo",
        "CREATE TABLE {t} (id int IDENTITY(1,1) PRIMARY KEY, name nvarchar(100) NOT NULL, amount decimal(12,2), \
         note nvarchar(max), data varbinary(max))",
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
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    exec(&c, &db, &format!("CREATE DATABASE IF NOT EXISTS `{db}`")).await;
    round_trip(
        cfg,
        &db,
        &db,
        "CREATE TABLE {t} (id UInt32, name String, amount Decimal(12,2), note Nullable(String), \
         data Nullable(String)) ENGINE = MergeTree ORDER BY id",
    )
    .await;
}
