//! Backup → damage → restore round trips. SQLite always runs; PostgreSQL and
//! MySQL need `DBMAN_TEST_POSTGRES` / `DBMAN_TEST_MYSQL` and the dump tools on
//! `PATH` (skipped with a note otherwise).

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use dbman_core::backup::{
    self, MyDumpOptions, MyRestoreOptions, PgDumpOptions, PgFormat, PgRestoreOptions, SqliteMethod, Tool,
};
use dbman_core::{Cell, Connection, ConnectionConfig, DbKind, StatementOutcome};
use dbman_drivers::backup::{Progress, Report, find_tool, run_tool, sqlite_backup, sqlite_restore};

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
