//! Redis: commands through the console path, every value type read by the
//! keys tab, databases kept apart, and a read-only connection refusing writes.
//! Needs `DBMAN_TEST_REDIS` (`redis://[:password@]host:port`); skipped otherwise.
//! Works in database 9, which it empties.

use dbman_core::{Cell, ConnectionConfig, DbKind, StatementOutcome};
use dbman_drivers::redis::{self, KeyValue};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBMAN_TEST_REDIS").ok().filter(|u| !u.is_empty())?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, addr) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = addr.split_once(':').unwrap_or((addr, "6379"));
    let mut cfg = ConnectionConfig::new(DbKind::Redis);
    cfg.host = host.into();
    cfg.port = port.trim_end_matches('/').parse().unwrap_or(6379);
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.database = "db0".into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    Some(cfg)
}

fn rows(o: StatementOutcome) -> Vec<Vec<Cell>> {
    match o {
        StatementOutcome::Rows(r) => r.rows,
        other => panic!("{other:?}"),
    }
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

#[tokio::test]
async fn redis() {
    let Some(cfg) = config() else { return };
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    assert!(
        c.server_version().await.unwrap().starts_with("Redis")
            || c.server_version().await.unwrap().starts_with("Valkey")
    );
    let dbs = c.databases().await.unwrap();
    assert!(dbs.contains(&"db0".to_string()) && dbs.contains(&"db9".to_string()), "{dbs:?}");

    let db = Some("db9");
    let run = |line: &'static str| {
        let c = c.clone();
        async move { c.execute(db, line, 1000).await.unwrap_or_else(|e| panic!("{line}: {e}")) }
    };
    run("FLUSHDB").await;
    assert_eq!(rows(run(r#"SET "user:1:name" "Alice Smith""#).await), [[text("OK")]]);
    assert_eq!(rows(run("GET user:1:name").await), [[text("Alice Smith")]]);
    assert_eq!(rows(run("GET missing").await), [[Cell::Null]]);
    run("HSET user:1 name Alice city Berlin").await;
    let mut hash = rows(run("HGETALL user:1").await);
    hash.sort_by_key(|r| r[0].display().into_owned());
    assert_eq!(hash, [[text("city"), text("Berlin")], [text("name"), text("Alice")]]);
    run("RPUSH queue a b c").await;
    assert_eq!(rows(run("LRANGE queue 0 -1").await).len(), 3);
    run("SADD tags red green").await;
    run("ZADD scores 10 alice 20 bob").await;
    run("XADD events 1-1 kind login").await;
    run("SET temp 1 EX 100").await;
    let e = c.execute(db, "NOSUCHCOMMAND", 10).await.unwrap_err();
    assert!(e.to_string().to_lowercase().contains("unknown command"), "{e}");
    assert!(c.execute(db, "GET \"open", 10).await.is_err());

    // The keys tab.
    let conn = c.as_ref();
    let mut keys = redis::scan(conn, "db9", "0", "*", 100).await.unwrap().keys;
    keys.sort();
    assert_eq!(keys, ["events", "queue", "scores", "tags", "temp", "user:1", "user:1:name"]);
    assert_eq!(redis::scan(conn, "db9", "0", "user:*", 100).await.unwrap().keys.len(), 2);
    assert_eq!(redis::dbsize(conn, "db9").await.unwrap(), 7);
    let k = redis::load_key(conn, "db9", "user:1:name", 100).await.unwrap();
    assert_eq!(
        (k.kind.as_str(), k.ttl, k.len, k.value),
        ("string", -1, Some(11), KeyValue::Text("Alice Smith".into()))
    );
    let k = redis::load_key(conn, "db9", "user:1", 100).await.unwrap();
    assert_eq!((k.kind.as_str(), k.len), ("hash", Some(2)));
    let k = redis::load_key(conn, "db9", "queue", 2).await.unwrap();
    let KeyValue::Rows { rows: r, .. } = k.value else { panic!() };
    assert_eq!((k.len, r), (Some(3), vec![vec!["0".to_string(), "a".into()], vec!["1".into(), "b".into()]]));
    let k = redis::load_key(conn, "db9", "tags", 100).await.unwrap();
    assert_eq!(
        k.value,
        KeyValue::Rows { columns: vec!["member".into()], rows: vec![vec!["green".into()], vec!["red".into()]] }
    );
    let k = redis::load_key(conn, "db9", "scores", 100).await.unwrap();
    let KeyValue::Rows { rows: r, .. } = k.value else { panic!() };
    assert_eq!(r, [["alice", "10"], ["bob", "20"]]);
    let k = redis::load_key(conn, "db9", "events", 100).await.unwrap();
    let KeyValue::Rows { rows: r, .. } = k.value else { panic!() };
    assert_eq!(r, [["1-1", "kind=login"]]);
    let k = redis::load_key(conn, "db9", "temp", 100).await.unwrap();
    assert!(k.ttl > 0 && k.ttl <= 100, "{}", k.ttl);
    redis::expire(conn, "db9", "temp", None).await.unwrap();
    assert_eq!(redis::load_key(conn, "db9", "temp", 1).await.unwrap().ttl, -1);
    redis::set_string(conn, "db9", "temp", "2").await.unwrap();
    assert_eq!(rows(run("GET temp").await), [[text("2")]]);
    assert_eq!(redis::delete(conn, "db9", &["temp".into(), "nope".into()]).await.unwrap(), 1);
    assert!(!redis::load_key(conn, "db9", "temp", 1).await.unwrap().exists());

    // Databases are apart; a console's SELECT moves only that console.
    assert_eq!(rows(c.execute(Some("db0"), "EXISTS user:1", 1).await.unwrap()), [[text("0")]]);
    assert_eq!(rows(c.execute(Some("db9"), "EXISTS user:1", 1).await.unwrap()), [[text("1")]]);

    // Read-only: reads go through, writes are refused before the server.
    let mut ro = cfg.clone();
    ro.read_only = true;
    let r = dbman_drivers::connect(&ro).await.unwrap();
    assert_eq!(rows(r.execute(db, "GET user:1:name", 1).await.unwrap()), [[text("Alice Smith")]]);
    assert!(r.execute(db, "SET user:1:name Bob", 1).await.is_err());
    assert!(redis::delete(r.as_ref(), "db9", &["user:1".into()]).await.is_err());
    assert_eq!(redis::dbsize(r.as_ref(), "db9").await.unwrap(), 6);
    run("FLUSHDB").await;
}
