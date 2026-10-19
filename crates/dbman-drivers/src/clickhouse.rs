//! ClickHouse driver over the HTTP interface (`reqwest`).
//!
//! Each statement is one POST. A console session keeps a `session_id`, so
//! `SET` and temporary tables last between statements; catalog queries go
//! without it, since a ClickHouse session runs one query at a time. Results
//! come as `JSONCompactEachRowWithNamesAndTypes` and are read line by line,
//! so a huge result is cut off after `max_rows` without being downloaded.

use std::time::Duration;

use async_trait::async_trait;
use dbman_core::sql::{Dialect, TokenKind};
use dbman_core::{
    Cell, ColumnInfo, ColumnMeta, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, IndexInfo,
    ObjectKind, ObjectPath, QueryResult, SslMode, StatementOutcome, TableDetails,
};
use futures_util::StreamExt;
use serde_json::Value;

pub struct ClickHouseConnection {
    cfg: ConnectionConfig,
    client: Client,
    session: String,
    /// `query_id` of the statement `execute` is running, for `KILL QUERY`.
    running: std::sync::Mutex<Option<String>>,
}

/// An HTTP client bound to one server and user; used by the driver and by backups.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
}

impl Client {
    pub fn new(cfg: &ConnectionConfig) -> DbResult<Self> {
        let tls = matches!(cfg.ssl_mode, SslMode::Require | SslMode::VerifyFull);
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(cfg.ssl_mode == SslMode::Require)
            .build()
            .map_err(DbError::from_display)?;
        let scheme = if tls { "https" } else { "http" };
        Ok(Self {
            http,
            base: format!("{scheme}://{}:{}/", cfg.host, cfg.port),
            user: cfg.user.clone(),
            password: cfg.password.clone(),
        })
    }

    /// Sends `sql` with URL `params`; a non-200 answer becomes the server's error.
    pub async fn post(&self, sql: &str, params: &[(&str, &str)]) -> DbResult<reqwest::Response> {
        let resp = self
            .http
            .post(&self.base)
            .query(params)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql.to_string())
            .send()
            .await
            .map_err(|e| DbError::new(format!("{}: {e}", self.base)))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        Err(server_error(&text))
    }

    /// Every row of a query as JSON arrays.
    pub async fn rows(&self, sql: &str, params: &[(&str, &str)]) -> DbResult<Vec<Vec<Value>>> {
        let mut all = vec![("default_format", "JSONCompactEachRow")];
        all.extend_from_slice(params);
        let text = self.post(sql, &all).await?.text().await.map_err(DbError::from_display)?;
        text.lines().filter(|l| !l.trim().is_empty()).map(parse_line).collect()
    }

    /// Runs a statement that returns nothing.
    pub async fn exec(&self, sql: &str, params: &[(&str, &str)]) -> DbResult<()> {
        self.post(sql, params).await?.bytes().await.map_err(DbError::from_display)?;
        Ok(())
    }
}

/// `Code: 60. DB::Exception: Table x doesn't exist. (UNKNOWN_TABLE) (version 24.8…)` without the version.
fn server_error(text: &str) -> DbError {
    let t = text.trim();
    let t = t.rsplit_once(" (version ").map_or(t, |(m, _)| m);
    DbError::new(t.to_string())
}

fn parse_line(line: &str) -> DbResult<Vec<Value>> {
    if line.starts_with("Code: ") {
        return Err(server_error(line));
    }
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Array(v)) => Ok(v),
        Ok(other) => Ok(vec![other]),
        Err(e) => Err(DbError::new(format!("unexpected reply ({e}): {}", line.chars().take(200).collect::<String>()))),
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn cell(v: Value) -> Cell {
    text(&v).map_or(Cell::Null, Cell::Text)
}

fn s(row: &[Value], i: usize) -> String {
    row.get(i).and_then(text).unwrap_or_default()
}

/// Statements that produce rows and are streamed; others wait for the end.
fn returns_rows(sql: &str) -> bool {
    let tokens = dbman_core::sql::tokenize(sql, DbKind::ClickHouse);
    let first = tokens.iter().find(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment));
    first.is_some_and(|t| {
        matches!(
            sql[t.range.clone()].to_ascii_lowercase().as_str(),
            "select" | "with" | "show" | "describe" | "desc" | "explain" | "exists" | "check" | "("
        )
    })
}

/// `Nullable(T)` -> (`T`, true).
fn unwrap_nullable(ty: &str) -> (String, bool) {
    match ty.strip_prefix("Nullable(").and_then(|r| r.strip_suffix(')')) {
        Some(inner) => (inner.to_string(), true),
        None => (ty.to_string(), false),
    }
}

impl ClickHouseConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let me = Self {
            cfg: cfg.clone(),
            client: Client::new(cfg)?,
            session: uuid::Uuid::new_v4().to_string(),
            running: std::sync::Mutex::new(None),
        };
        me.client.rows("SELECT 1", &[]).await?;
        Ok(me)
    }

    fn database<'a>(&'a self, database: Option<&'a str>) -> Option<&'a str> {
        database.filter(|d| !d.is_empty()).or(Some(self.cfg.database.as_str()).filter(|d| !d.is_empty()))
    }

    async fn run(
        &self,
        database: Option<&str>,
        sql: &str,
        max_rows: usize,
        query_id: &str,
    ) -> DbResult<StatementOutcome> {
        let streamed = returns_rows(sql);
        let mut params = vec![
            ("default_format", "JSONCompactEachRowWithNamesAndTypes"),
            ("session_id", self.session.as_str()),
            ("session_timeout", "3600"),
            ("query_id", query_id),
            ("cancel_http_readonly_queries_on_client_close", "1"),
        ];
        if !streamed {
            // The summary header then carries the final row counts.
            params.push(("wait_end_of_query", "1"));
        }
        if let Some(db) = self.database(database) {
            params.push(("database", db));
        }
        if self.cfg.read_only {
            // Reads only, though session settings may still change.
            params.push(("readonly", "2"));
        }
        // A query cut off or cancelled just before may still hold the session.
        let mut tries = 0;
        let resp = loop {
            match self.client.post(sql, &params).await {
                Err(e) if e.message.contains("SESSION_IS_LOCKED") && tries < 50 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                other => break other?,
            }
        };
        let written = resp
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| serde_json::from_str::<Value>(h).ok())
            .and_then(|v| v.get("written_rows").and_then(text))
            .and_then(|n| n.parse::<u64>().ok());

        let mut body = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut result: Option<QueryResult> = None;
        let take = |line: &str, result: &mut Option<QueryResult>, lines: &mut Vec<String>| -> DbResult<bool> {
            if line.trim().is_empty() {
                return Ok(true);
            }
            if result.is_none() {
                lines.push(line.to_string());
                if lines.len() == 2 {
                    let names = parse_line(&lines[0])?;
                    let types = parse_line(&lines[1])?;
                    let columns = names
                        .iter()
                        .zip(types.iter())
                        .map(|(n, t)| ColumnMeta::new(text(n).unwrap_or_default(), text(t).unwrap_or_default()))
                        .collect();
                    *result = Some(QueryResult { columns, rows: Vec::new(), truncated: false });
                }
                return Ok(true);
            }
            let r = result.as_mut().unwrap();
            if r.rows.len() >= max_rows {
                r.truncated = true;
                return Ok(false);
            }
            r.rows.push(parse_line(line)?.into_iter().map(cell).collect());
            Ok(true)
        };
        'read: while let Some(chunk) = body.next().await {
            buf.extend_from_slice(&chunk.map_err(DbError::from_display)?);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                if !take(&line, &mut result, &mut lines)? {
                    break 'read;
                }
            }
        }
        if !buf.is_empty() && result.as_ref().is_none_or(|r| !r.truncated) {
            let line = String::from_utf8_lossy(&buf).into_owned();
            take(&line, &mut result, &mut lines)?;
        }
        if result.as_ref().is_some_and(|r| r.truncated) {
            // Stop the rest of the query instead of waiting for the server to notice.
            drop(body);
            let kill = format!("KILL QUERY WHERE query_id = {} SYNC", Dialect(DbKind::ClickHouse).string(query_id));
            let _ = self.client.exec(&kill, &[]).await;
        }
        Ok(match result {
            Some(r) => StatementOutcome::Rows(r),
            None => StatementOutcome::Done { affected: written.filter(|n| *n > 0) },
        })
    }
}

#[async_trait]
impl Connection for ClickHouseConnection {
    fn kind(&self) -> DbKind {
        DbKind::ClickHouse
    }

    async fn server_version(&self) -> DbResult<String> {
        let r = self.client.rows("SELECT 'ClickHouse ' || version()", &[]).await?;
        Ok(r.first().map(|r| s(r, 0)).unwrap_or_default())
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        if self.cfg.only_default_database && !self.cfg.database.is_empty() {
            return Ok(vec![self.cfg.database.clone()]);
        }
        let r = self
            .client
            .rows(
                "SELECT name FROM system.databases
                 ORDER BY name IN ('system', 'INFORMATION_SCHEMA', 'information_schema'), name",
                &[],
            )
            .await?;
        Ok(r.iter().map(|r| s(r, 0)).collect())
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        Ok(vec![database.to_string()])
    }

    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>> {
        let r = self
            .client
            .rows(
                "SELECT name, engine, comment FROM system.tables
                 WHERE database = {db:String} AND NOT is_temporary AND NOT startsWith(name, '.inner')
                 ORDER BY name",
                &[("param_db", database)],
            )
            .await?;
        Ok(r.iter()
            .map(|row| {
                let engine = s(row, 1);
                let kind = match engine.as_str() {
                    "View" => ObjectKind::View,
                    "MaterializedView" => ObjectKind::MaterializedView,
                    _ => ObjectKind::Table,
                };
                DbObject {
                    kind,
                    path: ObjectPath::new(database, schema, s(row, 0)),
                    detail: (kind == ObjectKind::Table).then_some(engine),
                    comment: Some(s(row, 2)).filter(|c| !c.is_empty()),
                }
            })
            .collect())
    }

    async fn table_details(&self, p: &ObjectPath) -> DbResult<TableDetails> {
        let args = [("param_db", p.database.as_str()), ("param_t", p.name.as_str())];
        let cols = self
            .client
            .rows(
                "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key
                 FROM system.columns WHERE database = {db:String} AND table = {t:String} ORDER BY position",
                &args,
            )
            .await?;
        if cols.is_empty() {
            return Err(DbError::new(format!("{}.{} not found", p.database, p.name)));
        }
        let columns = cols
            .iter()
            .map(|r| {
                let (data_type, nullable) = unwrap_nullable(&s(r, 1));
                ColumnInfo {
                    name: s(r, 0),
                    data_type,
                    nullable,
                    // MATERIALIZED and ALIAS columns are not plain defaults.
                    default: (s(r, 2) == "DEFAULT").then(|| s(r, 3)),
                    comment: Some(s(r, 4)).filter(|c| !c.is_empty()),
                    primary_key: s(r, 5) == "1",
                    auto_increment: false,
                }
            })
            .collect();
        let table = self
            .client
            .rows(
                "SELECT primary_key, comment FROM system.tables WHERE database = {db:String} AND name = {t:String}",
                &args,
            )
            .await?;
        let mut indexes = Vec::new();
        let pk = table.first().map(|r| s(r, 0)).unwrap_or_default();
        if !pk.is_empty() {
            indexes.push(IndexInfo {
                name: "PRIMARY KEY".into(),
                columns: pk.split(", ").map(String::from).collect(),
                unique: false,
                primary: true,
                method: None,
            });
        }
        let skipping = self
            .client
            .rows(
                "SELECT name, expr, type FROM system.data_skipping_indices
                 WHERE database = {db:String} AND table = {t:String} ORDER BY name",
                &args,
            )
            .await?;
        indexes.extend(skipping.iter().map(|r| IndexInfo {
            name: s(r, 0),
            columns: vec![s(r, 1)],
            unique: false,
            primary: false,
            method: Some(s(r, 2)),
        }));
        let comment = table.first().map(|r| s(r, 1)).filter(|c| !c.is_empty());
        Ok(TableDetails { columns, indexes, foreign_keys: Vec::new(), comment })
    }

    async fn ddl(&self, o: &DbObject) -> DbResult<String> {
        let what = if o.detail.as_deref() == Some("Dictionary") { "DICTIONARY" } else { "TABLE" };
        let q = Dialect(DbKind::ClickHouse).qualified(&o.path);
        let r = self.client.rows(&format!("SHOW CREATE {what} {q}"), &[]).await?;
        Ok(format!("{};\n", r.first().map(|r| s(r, 0)).unwrap_or_default().trim_end()))
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let query_id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut r) = self.running.lock() {
            *r = Some(query_id.clone());
        }
        let result = self.run(database, sql, max_rows, &query_id).await;
        if let Ok(mut r) = self.running.lock() {
            *r = None;
        }
        result
    }

    async fn cancel(&self) -> DbResult<()> {
        let Some(id) = self.running.lock().ok().and_then(|r| r.clone()) else {
            return Ok(());
        };
        let sql = format!("KILL QUERY WHERE query_id = {} ASYNC", Dialect(DbKind::ClickHouse).string(&id));
        self.client.exec(&sql, &[]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(unwrap_nullable("Nullable(String)"), ("String".into(), true));
        assert_eq!(unwrap_nullable("Array(Nullable(String))"), ("Array(Nullable(String))".into(), false));
        assert!(returns_rows(" -- x\nselect 1"));
        assert!(!returns_rows("INSERT INTO t VALUES (1)"));
        assert_eq!(
            server_error("Code: 60. DB::Exception: Unknown table. (UNKNOWN_TABLE) (version 24.8.1.1)\n").message,
            "Code: 60. DB::Exception: Unknown table. (UNKNOWN_TABLE)"
        );
        assert_eq!(parse_line(r#"[1,"a",null,[1,2]]"#).unwrap().len(), 4);
        assert_eq!(cell(serde_json::json!([1, 2])), Cell::Text("[1,2]".into()));
    }
}
