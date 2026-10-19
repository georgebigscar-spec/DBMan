//! The sessions monitor on every server: a second session runs a slow query,
//! the listing finds it (and marks its own session), and the query is
//! cancelled or the session ended. Needs `DBMAN_TEST_POSTGRES`,
//! `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL` and `DBMAN_TEST_CLICKHOUSE`; each
//! server is skipped with a note otherwise.

use std::sync::Arc;
use std::time::Duration;

use dbman_core::sessions::{self, Session};
use dbman_core::{Connection, ConnectionConfig, DbKind, StatementOutcome};

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

async fn list(c: &Arc<dyn Connection>) -> Vec<Session> {
    let sql = sessions::list_sql(c.kind()).unwrap();
    match c.execute(None, sql, 10_000).await.unwrap_or_else(|e| panic!("{sql}: {e}")) {
        StatementOutcome::Rows(r) => sessions::parse(&r),
        other => panic!("{other:?}"),
    }
}

/// Runs `slow` in a second session, finds it in the list and stops it.
async fn check(cfg: ConnectionConfig, slow: &str, terminate: bool) {
    let kind = cfg.kind;
    let monitor = dbman_drivers::connect(&cfg).await.unwrap();
    let worker = dbman_drivers::connect(&cfg).await.unwrap();
    let slow_sql = slow.to_string();
    let running = tokio::spawn(async move { worker.execute(None, &slow_sql, 10).await });

    let mut found = None;
    for _ in 0..50 {
        let all = list(&monitor).await;
        assert_eq!(all.iter().filter(|s| s.own).count(), 1, "{kind:?}: own session once: {all:#?}");
        found = all
            .into_iter()
            .find(|s| !s.own && (s.query.to_lowercase().contains("sleep") || s.query.contains("WAITFOR")));
        if found.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let s = found.unwrap_or_else(|| panic!("{kind:?}: the slow query is not listed"));
    assert!(!s.is_idle(), "{kind:?}: {s:#?}");
    assert!(!s.id.is_empty());

    let sql = if terminate { sessions::terminate_sql(kind, &s.id) } else { sessions::cancel_sql(kind, &s.id) };
    let sql = sql.unwrap();
    monitor.execute(None, &sql, 1).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    let result = tokio::time::timeout(Duration::from_secs(15), running)
        .await
        .unwrap_or_else(|_| panic!("{kind:?}: the query still runs after {sql}"))
        .unwrap();
    assert!(result.is_err(), "{kind:?}: the stopped query finished: {result:?}");
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    check(cfg.clone(), "SELECT pg_sleep(60)", false).await;
    check(cfg, "SELECT pg_sleep(60)", true).await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    // KILL QUERY makes SLEEP() return 1 rather than fail, so only the kill is checked.
    check(cfg, "SELECT SLEEP(60)", true).await;
}

#[tokio::test]
async fn mssql() {
    let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    check(cfg, "WAITFOR DELAY '00:01:00'", true).await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    check(cfg, "SELECT sleepEachRow(1) FROM numbers(60) SETTINGS max_block_size = 1", false).await;
}
