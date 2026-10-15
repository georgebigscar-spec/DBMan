//! Backup → damage → restore round trips. SQLite and DuckDB always run; PostgreSQL and
//! MySQL need `DBMAN_TEST_POSTGRES` / `DBMAN_TEST_MYSQL` and the dump tools on
//! `PATH`, SQL Server and ClickHouse need `DBMAN_TEST_MSSQL` / `DBMAN_TEST_CLICKHOUSE`
//! (the latter with a `backups` disk, see `ci/clickhouse-backups.xml`); skipped with a note otherwise.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use dbman_core::backup::{
    self, DuckMethod, MyDumpOptions, MyRestoreOptions, PgDumpOptions, PgFormat, PgRestoreOptions, SqliteMethod, Tool,
};
use dbman_core::{Cell, Connection, ConnectionConfig, DbKind, StatementOutcome};
use dbman_drivers::backup::{
    MsJob, Progress, Report, clickhouse_run, duckdb_run, find_tool, mssql_run, run_tool, sqlite_backup, sqlite_restore,
};

fn collect() -> (Report, Arc<Mutex<Vec<Progress>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (Arc::new(move |p| s.lock().unwrap().push(p)), seen)
}

fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

async fn count(c: &Arc<dyn Connection>, db: &str, sql: &str) -> String {
    match c.execute(Some(db), sql, 10).await.unwrap() {
        StatementOutcome::Rows(r) => match &r.rows[0][0] {
            Cell::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
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

#[tokio::test]
async fn sqlite_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("app.db");
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = db.to_string_lossy().into_owned();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    c.execute(None, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", 0).await.unwrap();
    c.execute(None, "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000) INSERT INTO t (v) SELECT printf('row %d', i) FROM n", 0)
        .await
        .unwrap();

    for method in SqliteMethod::ALL {
        let target = dir.path().join(format!("backup-{method:?}.db"));
        let (report, seen) = collect();
        sqlite_backup(db.clone(), target.clone(), method, report, no_cancel()).await.unwrap();
        assert!(seen.lock().unwrap().contains(&Progress::Log("quick_check: ok".into())));
        assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Fraction(f) if *f >= 1.0)));

        c.execute(None, "DELETE FROM t WHERE id > 10", 0).await.unwrap();
        assert_eq!(count(&c, "main", "SELECT count(*) FROM t").await, "10");
        let (report, _) = collect();
        sqlite_restore(target.clone(), db.clone(), report, no_cancel()).await.unwrap();
        // The open session sees the restored rows.
        assert_eq!(count(&c, "main", "SELECT count(*) FROM t").await, "5000");
    }

    let not_db = dir.path().join("notes.txt");
    std::fs::write(&not_db, "hello there, not a database").unwrap();
    let (report, _) = collect();
    let err = sqlite_restore(not_db, db.clone(), report, no_cancel()).await.unwrap_err();
    assert!(err.message.contains("not an SQLite database"), "{}", err.message);

    let (report, _) = collect();
    let target = dir.path().join("cancelled.db");
    let err = sqlite_backup(db, target.clone(), SqliteMethod::OnlineBackup, report, Arc::new(AtomicBool::new(true)))
        .await
        .unwrap_err();
    assert_eq!(err.message, "cancelled");
    assert!(!target.exists());
}

#[tokio::test]
async fn postgres_round_trip() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let (Some(dump), Some(restore), Some(psql)) =
        (find_tool(Tool::PgDump, None), find_tool(Tool::PgRestore, None), find_tool(Tool::Psql, None))
    else {
        eprintln!("PostgreSQL client tools not found, skipping");
        return;
    };
    let db = cfg.database.clone();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    for sql in [
        "DROP SCHEMA IF EXISTS dbman_bk CASCADE",
        "CREATE SCHEMA dbman_bk",
        "CREATE TABLE dbman_bk.items (id int PRIMARY KEY, name text)",
        "INSERT INTO dbman_bk.items SELECT g, 'item ' || g FROM generate_series(1, 1000) g",
    ] {
        c.execute(Some(&db), sql, 0).await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    for format in PgFormat::ALL {
        let target = dir.path().join(format!("dump-{format:?}"));
        let o = PgDumpOptions { format, schemas: vec!["dbman_bk".into()], ..Default::default() };
        let (report, seen) = collect();
        run_tool(&dump, &backup::pg_dump(&cfg, &db, &o, &target), Some(&target), report, no_cancel())
            .await
            .unwrap_or_else(|e| panic!("{format:?}: {}", e.message));
        assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Bytes(n) if *n > 0)));
        assert_eq!(backup::detect_pg_format(&target).unwrap(), format);

        c.execute(Some(&db), "DROP SCHEMA dbman_bk CASCADE", 0).await.unwrap();
        let ro = PgRestoreOptions { single_transaction: true, ..Default::default() };
        let cmd = backup::pg_restore(&cfg, &db, &ro, &target, format);
        let program = if format == PgFormat::Plain { &psql } else { &restore };
        let (report, _) = collect();
        run_tool(program, &cmd, None, report, no_cancel())
            .await
            .unwrap_or_else(|e| panic!("{format:?}: {}", e.message));
        assert_eq!(count(&c, &db, "SELECT count(*) FROM dbman_bk.items").await, "1000", "{format:?}");
    }

    // A failing restore reports the tool's own error.
    let bad = dir.path().join("bad.sql");
    std::fs::write(&bad, "SELECT * FROM no_such_table_here;\n").unwrap();
    let cmd = backup::pg_restore(&cfg, &db, &PgRestoreOptions::default(), &bad, PgFormat::Plain);
    let (report, _) = collect();
    let err = run_tool(&psql, &cmd, None, report, no_cancel()).await.unwrap_err();
    assert!(err.message.contains("no_such_table_here"), "{}", err.message);
    c.execute(Some(&db), "DROP SCHEMA dbman_bk CASCADE", 0).await.unwrap();
}

#[tokio::test]
async fn mysql_round_trip() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let mariadb = c.server_version().await.unwrap().contains("MariaDB");
    let dump_tool = if mariadb { [Tool::MariaDbDump, Tool::MySqlDump] } else { [Tool::MySqlDump, Tool::MariaDbDump] };
    let client_tool = if mariadb { [Tool::MariaDb, Tool::MySql] } else { [Tool::MySql, Tool::MariaDb] };
    let Some((dump_tool, dump)) = dump_tool.into_iter().find_map(|t| find_tool(t, None).map(|p| (t, p))) else {
        eprintln!("mysqldump not found, skipping");
        return;
    };
    let Some((client_tool, client)) = client_tool.into_iter().find_map(|t| find_tool(t, None).map(|p| (t, p))) else {
        eprintln!("mysql client not found, skipping");
        return;
    };
    let db = cfg.database.clone();
    for sql in [
        "DROP TABLE IF EXISTS bk_items",
        "CREATE TABLE bk_items (id int PRIMARY KEY, name varchar(50), data varbinary(8))",
        "INSERT INTO bk_items VALUES (1, 'один', 0x00ff), (2, 'two', NULL), (3, 'three''s', 0x41)",
    ] {
        c.execute(Some(&db), sql, 0).await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("dump.sql");
    let o = MyDumpOptions { tables: vec!["bk_items".into()], ..Default::default() };
    let (report, _) = collect();
    run_tool(&dump, &backup::my_dump(&cfg, dump_tool, mariadb, &db, &o, &target), Some(&target), report, no_cancel())
        .await
        .unwrap_or_else(|e| panic!("{}", e.message));

    c.execute(Some(&db), "DROP TABLE bk_items", 0).await.unwrap();
    let (report, seen) = collect();
    run_tool(
        &client,
        &backup::my_restore(&cfg, client_tool, &db, &MyRestoreOptions::default(), &target),
        None,
        report,
        no_cancel(),
    )
    .await
    .unwrap_or_else(|e| panic!("{}", e.message));
    assert!(seen.lock().unwrap().contains(&Progress::Fraction(1.0)));
    assert_eq!(count(&c, &db, "SELECT count(*) FROM bk_items").await, "3");
    assert_eq!(count(&c, &db, "SELECT name FROM bk_items WHERE id = 1").await, "один");
    assert_eq!(count(&c, &db, "SELECT hex(data) FROM bk_items WHERE id = 1").await, "00FF");
    c.execute(Some(&db), "DROP TABLE bk_items", 0).await.unwrap();
}

#[tokio::test]
async fn duckdb_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("shop.duckdb");
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = db.to_string_lossy().into_owned();
    // The session stays open during the backup: the job shares its database.
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    for sql in [
        "CREATE SCHEMA sales",
        "CREATE SEQUENCE sales.ids",
        "CREATE TABLE sales.orders (id INTEGER PRIMARY KEY DEFAULT nextval('sales.ids'), note VARCHAR, tags VARCHAR[])",
        "INSERT INTO sales.orders (note, tags) SELECT 'order ' || i, ['a', 'b'] FROM range(5000) t(i)",
        "CREATE VIEW sales.big AS SELECT * FROM sales.orders WHERE id > 100",
    ] {
        c.execute(Some("shop"), sql, 0).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    for method in DuckMethod::ALL {
        let target = dir.path().join(backup::duck_file_name("shop", "t", method) + &format!("-{method:?}"));
        let (report, seen) = collect();
        let statements = backup::duck_backup("shop", method, &target);
        duckdb_run(cfg.database.clone(), statements, Some(target.clone()), report, no_cancel())
            .await
            .unwrap_or_else(|e| panic!("{method:?}: {e}"));
        assert_eq!(target.is_dir(), method.writes_folder(), "{method:?}");
        assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Bytes(n) if *n > 0)));

        // Restore into a new, empty database file.
        let restored = dir.path().join(format!("restored{method:?}.duckdb"));
        let mut cfg2 = cfg.clone();
        cfg2.database = restored.to_string_lossy().into_owned();
        let c2 = dbman_drivers::connect(&cfg2).await.unwrap();
        let name = format!("restored{method:?}");
        let (report, _) = collect();
        duckdb_run(cfg2.database.clone(), backup::duck_restore(&name, &target), None, report, no_cancel())
            .await
            .unwrap_or_else(|e| panic!("{method:?}: {e}"));
        assert_eq!(count(&c2, &name, "SELECT count(*) FROM sales.big").await, "4900", "{method:?}");
        assert_eq!(count(&c2, &name, "SELECT tags[2] FROM sales.orders LIMIT 1").await, "b", "{method:?}");
    }
}

#[tokio::test]
async fn mssql_round_trip() {
    let Some(mut cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    cfg.ssl_mode = dbman_core::SslMode::Prefer;
    cfg.database = "master".into();
    let master = dbman_drivers::connect(&cfg).await.unwrap();
    let run = |sql: String| {
        let m = master.clone();
        async move { m.execute(Some("master"), &sql, 0).await.unwrap_or_else(|e| panic!("{sql}: {e}")) }
    };
    for db in ["dbman_bk", "dbman_bk_copy"] {
        run(format!(
            "IF DB_ID(N'{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END"
        ))
        .await;
    }
    run("CREATE DATABASE dbman_bk".into()).await;
    run("CREATE TABLE dbman_bk.dbo.t (id int IDENTITY PRIMARY KEY, v nvarchar(20))".into()).await;
    run("INSERT INTO dbman_bk.dbo.t (v) SELECT TOP 3000 N'row' FROM sys.all_objects a CROSS JOIN sys.all_objects b"
        .into())
    .await;

    // A relative path lands in the server's default backup folder.
    let file = "dbman_bk_test.bak".to_string();
    let (report, seen) = collect();
    let backup = MsJob::Backup { database: "dbman_bk".into(), options: Default::default(), path: file.clone() };
    mssql_run(cfg.clone(), backup, report, no_cancel()).await.unwrap();
    assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Bytes(n) if *n > 0)));
    assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Log(l) if l.contains("VERIFYONLY"))));

    run("DELETE FROM dbman_bk.dbo.t WHERE id > 10".into()).await;
    // A session inside the database is disconnected by the restore.
    let mut inside = cfg.clone();
    inside.database = "dbman_bk".into();
    let other = dbman_drivers::connect(&inside).await.unwrap();
    let (report, _) = collect();
    let restore = MsJob::Restore { database: "dbman_bk".into(), options: Default::default(), path: file.clone() };
    mssql_run(cfg.clone(), restore, report, no_cancel()).await.unwrap();
    assert_eq!(count(&master, "master", "SELECT count(*) FROM dbman_bk.dbo.t").await, "3000");
    drop(other);

    // A copy next to the original needs its files moved.
    let (report, seen) = collect();
    let options = dbman_core::backup::MsRestoreOptions { relocate: true, ..Default::default() };
    let copy = MsJob::Restore { database: "dbman_bk_copy".into(), options, path: file };
    mssql_run(cfg.clone(), copy, report, no_cancel()).await.unwrap();
    assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Log(l) if l.contains("dbman_bk_copy_log.ldf"))));
    assert_eq!(count(&master, "master", "SELECT count(*) FROM dbman_bk_copy.dbo.t").await, "3000");
    assert_eq!(
        count(&master, "master", "SELECT CAST(DATABASEPROPERTYEX(N'dbman_bk_copy', 'UserAccess') AS nvarchar(20))")
            .await,
        "MULTI_USER"
    );
}

#[tokio::test]
async fn clickhouse_round_trip() {
    use dbman_core::backup::{ChBackupOptions, ChDestination, ChRestoreOptions, ch_backup, ch_restore};
    let Some(mut cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let run = |sql: String| {
        let c = c.clone();
        async move { c.execute(None, &sql, 0).await.unwrap_or_else(|e| panic!("{sql}: {e}")) }
    };
    for db in ["dbman_chbk", "dbman_chbk_full", "dbman_chbk_incr"] {
        run(format!("DROP DATABASE IF EXISTS {db}")).await;
    }
    run("CREATE DATABASE dbman_chbk".into()).await;
    run("CREATE TABLE dbman_chbk.t (id UInt64, v String) ENGINE = MergeTree ORDER BY id".into()).await;
    run("INSERT INTO dbman_chbk.t SELECT number, toString(number) FROM numbers(5000)".into()).await;

    // The disk keeps old backups, so every run uses new names.
    let stamp = uuid::Uuid::new_v4().simple().to_string();
    let full = ChDestination::Disk { disk: "backups".into(), path: format!("full_{stamp}.zip") };
    let o = ChBackupOptions { destination: full.clone(), ..Default::default() };
    let (report, seen) = collect();
    clickhouse_run(
        cfg.clone(),
        ch_backup("dbman_chbk", None, &o, false),
        ch_backup("dbman_chbk", None, &o, true),
        report,
        no_cancel(),
    )
    .await
    .unwrap();
    assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Bytes(n) if *n > 0)));

    run("INSERT INTO dbman_chbk.t SELECT number, 'more' FROM numbers(5000, 100)".into()).await;
    let incr = full.with_name(&format!("incr_{stamp}.zip"));
    let o =
        ChBackupOptions { destination: incr.clone(), base_backup: format!("full_{stamp}.zip"), ..Default::default() };
    let (report, _) = collect();
    clickhouse_run(cfg.clone(), ch_backup("dbman_chbk", None, &o, false), String::new(), report, no_cancel())
        .await
        .unwrap();

    for (source, name, rows) in [(full, "dbman_chbk_full", "5000"), (incr, "dbman_chbk_incr", "5100")] {
        let r = ChRestoreOptions { source, rename_to: name.into(), ..Default::default() };
        let (report, seen) = collect();
        clickhouse_run(cfg.clone(), ch_restore("dbman_chbk", None, &r, false), String::new(), report, no_cancel())
            .await
            .unwrap();
        assert!(seen.lock().unwrap().iter().any(|p| matches!(p, Progress::Fraction(f) if *f >= 1.0)));
        assert_eq!(count(&c, name, &format!("SELECT count() FROM {name}.t")).await, rows);
    }

    // Restoring over existing tables fails unless rows may be added.
    let r = ChRestoreOptions {
        source: ChDestination::Disk { disk: "backups".into(), path: format!("full_{stamp}.zip") },
        ..Default::default()
    };
    let (report, _) = collect();
    let err =
        clickhouse_run(cfg.clone(), ch_restore("dbman_chbk", Some("t"), &r, false), String::new(), report, no_cancel())
            .await
            .unwrap_err();
    assert!(err.message.contains("already"), "{}", err.message);
}
