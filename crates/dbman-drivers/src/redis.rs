//! Redis (and servers on its protocol, such as Valkey and KeyDB). The explorer
//! lists the numbered databases, a keys tab browses them, and a console runs
//! one command per line, as redis-cli does.

use std::time::Duration;

use async_trait::async_trait;
use dbman_core::redis::{self as r, Reply};
use dbman_core::{
    Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ObjectPath, SslMode, StatementOutcome,
    TableDetails,
};
use redis::aio::MultiplexedConnection;
use redis::{ConnectionAddr, IntoConnectionInfo, ProtocolVersion, RedisConnectionInfo, Value};
use tokio::sync::Mutex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct RedisConnection {
    cfg: ConnectionConfig,
    /// The connection and the database it has selected.
    conn: Mutex<(MultiplexedConnection, i64)>,
}

fn err(e: impl std::fmt::Display) -> DbError {
    DbError::from_display(e)
}

impl RedisConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let host = if cfg.host.is_empty() { "127.0.0.1".to_string() } else { cfg.host.clone() };
        let port = if cfg.port == 0 { 6379 } else { cfg.port };
        // Redis serves TLS on a port of its own, so there is nothing to "prefer".
        let addr = match cfg.ssl_mode {
            SslMode::Disable | SslMode::Prefer => ConnectionAddr::Tcp(host, port),
            SslMode::Require => ConnectionAddr::TcpTls { host, port, insecure: true, tls_params: None },
            SslMode::VerifyFull => ConnectionAddr::TcpTls { host, port, insecure: false, tls_params: None },
        };
        let db = r::db_index(&cfg.database).unwrap_or(0);
        let mut settings = RedisConnectionInfo::default().set_db(db).set_protocol(ProtocolVersion::RESP3);
        if !cfg.user.is_empty() {
            settings = settings.set_username(&cfg.user);
        }
        if !cfg.password.is_empty() {
            settings = settings.set_password(&cfg.password);
        }
        let info = addr.into_connection_info().map_err(err)?.set_redis_settings(settings);
        let client = redis::Client::open(info).map_err(err)?;
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, client.get_multiplexed_async_connection())
            .await
            .map_err(|_| DbError::new("Timed out connecting to the Redis server."))?
            .map_err(err)?;
        Ok(Self { cfg: cfg.clone(), conn: Mutex::new((conn, db)) })
    }

    async fn info(&self, section: &str) -> DbResult<String> {
        let reply = self.command(None, &["INFO".into(), section.into()]).await?;
        Ok(reply.as_str().unwrap_or_default())
    }
}

/// A driver value as a reply.
fn reply(v: Value) -> Reply {
    match v {
        Value::Nil => Reply::Nil,
        Value::Int(i) => Reply::Int(i),
        Value::BulkString(b) => match String::from_utf8(b) {
            Ok(s) => Reply::Text(s),
            Err(e) => Reply::Bytes(e.into_bytes()),
        },
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            Reply::Array(items.into_iter().map(reply).collect())
        }
        Value::SimpleString(s) => Reply::Status(s),
        Value::Okay => Reply::Status("OK".into()),
        Value::Map(pairs) => Reply::Map(pairs.into_iter().map(|(k, v)| (reply(k), reply(v))).collect()),
        Value::Attribute { data, .. } => reply(*data),
        Value::Double(d) => Reply::Double(d),
        Value::Boolean(b) => Reply::Bool(b),
        Value::VerbatimString { text, .. } => Reply::Text(text),
        Value::BigNumber(n) => Reply::Text(String::from_utf8_lossy(&n).into_owned()),
        Value::ServerError(e) => Reply::Status(format!("ERR {e}")),
        #[allow(unreachable_patterns)]
        _ => Reply::Nil,
    }
}

/// `key:value` lines of an `INFO` reply.
fn info_field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    info.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')).map(str::trim)
}

#[async_trait]
impl Connection for RedisConnection {
    fn kind(&self) -> DbKind {
        DbKind::Redis
    }

    async fn server_version(&self) -> DbResult<String> {
        let info = self.info("server").await?;
        Ok(match (info_field(&info, "valkey_version"), info_field(&info, "redis_version")) {
            (Some(v), _) => format!("Valkey {v}"),
            (None, Some(v)) => format!("Redis {v}"),
            _ => "Redis".into(),
        })
    }

    /// `db0`, `db1`… as many as the server has (16 unless configured), or the
    /// ones holding keys when `CONFIG` is not allowed.
    async fn databases(&self) -> DbResult<Vec<String>> {
        let own = r::db_index(&self.cfg.database).unwrap_or(0);
        if self.cfg.only_default_database {
            return Ok(vec![format!("db{own}")]);
        }
        let config = self.command(None, &["CONFIG".into(), "GET".into(), "databases".into()]).await;
        let count = config.ok().and_then(|c| c.items().get(1).and_then(|v| v.as_int()));
        let mut dbs: Vec<i64> = match count {
            Some(n) if n > 0 => (0..n).collect(),
            _ => {
                let keyspace = self.info("keyspace").await.unwrap_or_default();
                keyspace.lines().filter_map(|l| l.strip_prefix("db")?.split(':').next()?.parse().ok()).collect()
            }
        };
        if !dbs.contains(&own) {
            dbs.push(own);
            dbs.sort_unstable();
        }
        Ok(dbs.into_iter().map(|d| format!("db{d}")).collect())
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        Ok(vec![database.to_string()])
    }

    /// Keys are browsed in their own tab, not listed as objects.
    async fn objects(&self, _database: &str, _schema: &str) -> DbResult<Vec<DbObject>> {
        Ok(Vec::new())
    }

    async fn table_details(&self, _path: &ObjectPath) -> DbResult<TableDetails> {
        Err(DbError::new("Redis keys have no table structure."))
    }

    async fn ddl(&self, _object: &DbObject) -> DbResult<String> {
        Err(DbError::new("Redis keys have no DDL."))
    }

    /// One command line; the reply as a table.
    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let args = r::parse(sql.trim()).map_err(DbError::new)?;
        if args.is_empty() {
            return Ok(StatementOutcome::Done { affected: None });
        }
        let reply = self.command(database, &args).await?;
        let mut result = r::result(&reply);
        if result.rows.len() > max_rows {
            result.rows.truncate(max_rows);
            result.truncated = true;
        }
        Ok(StatementOutcome::Rows(result))
    }

    /// Commands are short; a blocking one (`BLPOP`) ends at its own timeout.
    async fn cancel(&self) -> DbResult<()> {
        Ok(())
    }

    async fn command(&self, database: Option<&str>, args: &[String]) -> DbResult<Reply> {
        let Some(name) = args.first() else { return Ok(Reply::Nil) };
        let mut guard = self.conn.lock().await;
        let (conn, current) = &mut *guard;
        if let Some(db) = database.and_then(r::db_index)
            && db != *current
        {
            redis::cmd("SELECT").arg(db).query_async::<Value>(conn).await.map_err(err)?;
            *current = db;
        }
        let mut cmd = redis::cmd(name);
        for a in &args[1..] {
            cmd.arg(a.as_bytes());
        }
        let value: Value = cmd.query_async(conn).await.map_err(err)?;
        if name.eq_ignore_ascii_case("select")
            && let Some(db) = args.get(1).and_then(|d| d.parse().ok())
        {
            *current = db;
        }
        Ok(reply(value))
    }
}

/// A page of `SCAN`: the cursor to go on with (`0` at the end) and the keys.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    pub cursor: String,
    pub keys: Vec<String>,
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Keys matching `pattern` from `cursor` on, about `count` per call, until
/// `limit` keys are found or the scan ends.
pub async fn scan(c: &dyn Connection, db: &str, cursor: &str, pattern: &str, limit: usize) -> DbResult<Scan> {
    let pattern = if pattern.trim().is_empty() { "*" } else { pattern.trim() };
    let mut out = Scan { cursor: cursor.to_string(), keys: Vec::new() };
    loop {
        let reply = c.command(Some(db), &args(&["SCAN", &out.cursor, "MATCH", pattern, "COUNT", "1000"])).await?;
        let items = reply.items();
        out.cursor = items.first().and_then(|c| c.as_str()).unwrap_or_else(|| "0".into());
        if let Some(keys) = items.get(1) {
            out.keys.extend(keys.items().into_iter().filter_map(Reply::as_str));
        }
        if out.cursor == "0" || out.keys.len() >= limit {
            return Ok(out);
        }
    }
}

/// What a key holds, as far as it is shown.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    /// A string, or a JSON document.
    Text(String),
    Bytes(Vec<u8>),
    /// A hash, list, set, sorted set or stream: its first rows.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    /// A type DBMan does not read; the console can.
    Other,
}

/// A key: its type, time to live in seconds (-1 without expiry), length and value.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyInfo {
    pub key: String,
    /// `string`, `hash`, `list`, `set`, `zset`, `stream`, `ReJSON-RL`…; `none` when gone.
    pub kind: String,
    pub ttl: i64,
    /// Characters of a string, fields of a hash, items of a list, set, sorted set or stream.
    pub len: Option<i64>,
    pub value: KeyValue,
}

impl KeyInfo {
    pub fn exists(&self) -> bool {
        self.kind != "none"
    }
}

/// Reads a key, with at most `limit` items of a collection.
pub async fn load_key(c: &dyn Connection, db: &str, key: &str, limit: usize) -> DbResult<KeyInfo> {
    let run = |a: Vec<String>| async move { c.command(Some(db), &a).await };
    let kind = run(args(&["TYPE", key])).await?.as_str().unwrap_or_else(|| "none".into());
    let ttl = run(args(&["TTL", key])).await?.as_int().unwrap_or(-1);
    let n = limit.to_string();
    let last = (limit.max(1) - 1).to_string();
    let rows = |columns: &[&str], rows: Vec<Vec<String>>| KeyValue::Rows {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        rows,
    };
    // Items of a `*SCAN` over a collection, until `limit`.
    let scan_all = |command: &'static str| async move {
        let mut cursor = "0".to_string();
        let mut items: Vec<Reply> = Vec::new();
        loop {
            let reply = run(args(&[command, key, &cursor, "COUNT", "500"])).await?;
            let parts = reply.items();
            cursor = parts.first().and_then(|c| c.as_str()).unwrap_or_else(|| "0".into());
            if let Some(page) = parts.get(1) {
                items.extend(page.items().into_iter().cloned());
            }
            if cursor == "0" || items.len() >= limit * 2 {
                return Ok::<_, DbError>(items);
            }
        }
    };
    let text = |r: &Reply| r.as_str().unwrap_or_else(|| r.text());
    let (len, value) = match kind.as_str() {
        "string" => {
            let len = run(args(&["STRLEN", key])).await?.as_int();
            let value = match run(args(&["GET", key])).await? {
                Reply::Bytes(b) => KeyValue::Bytes(b),
                r => KeyValue::Text(r.as_str().unwrap_or_default()),
            };
            (len, value)
        }
        "hash" => {
            let len = run(args(&["HLEN", key])).await?.as_int();
            let items = scan_all("HSCAN").await?;
            let pairs = items.chunks(2).take(limit).map(|p| p.iter().map(text).collect()).collect();
            (len, rows(&["field", "value"], pairs))
        }
        "list" => {
            let len = run(args(&["LLEN", key])).await?.as_int();
            let items = run(args(&["LRANGE", key, "0", &last])).await?;
            let r = items.items().into_iter().enumerate().map(|(i, v)| vec![i.to_string(), text(v)]).collect();
            (len, rows(&["index", "value"], r))
        }
        "set" => {
            let len = run(args(&["SCARD", key])).await?.as_int();
            let mut members: Vec<String> = scan_all("SSCAN").await?.iter().map(text).collect();
            members.sort();
            members.truncate(limit);
            (len, rows(&["member"], members.into_iter().map(|m| vec![m]).collect()))
        }
        "zset" => {
            let len = run(args(&["ZCARD", key])).await?.as_int();
            let reply = run(args(&["ZRANGE", key, "0", &last, "WITHSCORES"])).await?;
            // RESP3 pairs each member with its score; RESP2 alternates them.
            let flat: Vec<&Reply> = reply
                .items()
                .into_iter()
                .flat_map(|i| if i.items().is_empty() { vec![i] } else { i.items() })
                .collect();
            let r = flat.chunks(2).map(|p| p.iter().map(|v| text(v)).collect()).collect();
            (len, rows(&["member", "score"], r))
        }
        "stream" => {
            let len = run(args(&["XLEN", key])).await?.as_int();
            let reply = run(args(&["XRANGE", key, "-", "+", "COUNT", &n])).await?;
            let r = reply
                .items()
                .into_iter()
                .map(|e| {
                    let parts = e.items();
                    let id = parts.first().map(|i| text(i)).unwrap_or_default();
                    let fields: Vec<String> = parts
                        .get(1)
                        .map(|f| f.items())
                        .unwrap_or_default()
                        .chunks(2)
                        .map(|p| format!("{}={}", text(p[0]), p.get(1).map(|v| text(v)).unwrap_or_default()))
                        .collect();
                    vec![id, fields.join(", ")]
                })
                .collect();
            (len, rows(&["id", "fields"], r))
        }
        "ReJSON-RL" => (None, KeyValue::Text(run(args(&["JSON.GET", key])).await?.as_str().unwrap_or_default())),
        _ => (None, KeyValue::Other),
    };
    Ok(KeyInfo { key: key.to_string(), kind, ttl, len, value })
}

/// Sets a string key, keeping its time to live.
pub async fn set_string(c: &dyn Connection, db: &str, key: &str, value: &str) -> DbResult<()> {
    c.command(Some(db), &args(&["SET", key, value, "KEEPTTL"])).await.map(drop)
}

/// Sets the time to live in seconds, or removes it with `None`.
pub async fn expire(c: &dyn Connection, db: &str, key: &str, seconds: Option<i64>) -> DbResult<()> {
    let a = match seconds {
        Some(s) => args(&["EXPIRE", key, &s.to_string()]),
        None => args(&["PERSIST", key]),
    };
    c.command(Some(db), &a).await.map(drop)
}

/// Deletes keys; returns how many existed.
pub async fn delete(c: &dyn Connection, db: &str, keys: &[String]) -> DbResult<i64> {
    let mut a = vec!["DEL".to_string()];
    a.extend(keys.iter().cloned());
    Ok(c.command(Some(db), &a).await?.as_int().unwrap_or(0))
}

/// The number of keys in a database.
pub async fn dbsize(c: &dyn Connection, db: &str) -> DbResult<i64> {
    Ok(c.command(Some(db), &args(&["DBSIZE"])).await?.as_int().unwrap_or(0))
}
