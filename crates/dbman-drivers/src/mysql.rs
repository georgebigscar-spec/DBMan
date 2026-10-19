//! MySQL and MariaDB driver on `mysql_async`.
//!
//! In MySQL a database and a schema are the same thing, so the explorer shows
//! databases and the schema of every object equals its database.

use std::time::Duration;

use async_trait::async_trait;
use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, ColumnInfo, ColumnMeta, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ForeignKeyInfo,
    IndexInfo, ObjectKind, ObjectPath, QueryResult, SslMode, StatementOutcome, TableDetails,
};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder, Row, SslOpts, Value};
use tokio::sync::Mutex;

struct Session {
    conn: Conn,
    /// The database selected with `USE`, to avoid repeating it.
    current_db: Option<String>,
}

pub struct MySqlConnection {
    cfg: ConnectionConfig,
    session: Mutex<Session>,
    /// Server thread id of the session, for `KILL QUERY` from a second connection.
    thread_id: u32,
}

/// System databases hidden from the explorer.
const HIDDEN: [&str; 3] = ["information_schema", "performance_schema", "sys"];

impl MySqlConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let conn = open_with_ssl(cfg).await?;
        let thread_id = conn.id();
        let current_db = Some(cfg.database.clone()).filter(|d| !d.is_empty());
        Ok(Self { cfg: cfg.clone(), session: Mutex::new(Session { conn, current_db }), thread_id })
    }

    async fn rows<P: Into<mysql_async::Params> + Send>(&self, sql: &str, params: P) -> DbResult<Vec<Row>> {
        let mut s = self.session.lock().await;
        s.conn.exec(sql, params).await.map_err(my_err)
    }
}

async fn open_with_ssl(cfg: &ConnectionConfig) -> DbResult<Conn> {
    match cfg.ssl_mode {
        SslMode::Disable => open(cfg, None).await,
        SslMode::Prefer => match open(cfg, Some(insecure_tls())).await {
            Ok(c) => Ok(c),
            Err(_) => open(cfg, None).await,
        },
        SslMode::Require => open(cfg, Some(insecure_tls())).await,
        SslMode::VerifyFull => open(cfg, Some(SslOpts::default())).await,
    }
}

fn insecure_tls() -> SslOpts {
    SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)
}

async fn open(cfg: &ConnectionConfig, ssl: Option<SslOpts>) -> DbResult<Conn> {
    let opts = OptsBuilder::default()
        .ip_or_hostname(cfg.host.clone())
        .tcp_port(cfg.port)
        .user(Some(cfg.user.clone()))
        .pass(Some(cfg.password.clone()).filter(|p| !p.is_empty()))
        .db_name(Some(cfg.database.clone()).filter(|d| !d.is_empty()))
        .prefer_socket(false)
        // Report matched rather than changed rows, so an UPDATE that keeps a value still counts.
        .client_found_rows(true)
        .init(if cfg.read_only { vec!["SET SESSION TRANSACTION READ ONLY"] } else { Vec::new() })
        .ssl_opts(ssl);
    match tokio::time::timeout(Duration::from_secs(10), Conn::new(opts)).await {
        Ok(r) => r.map_err(my_err),
        Err(_) => Err(DbError::new(format!("timed out connecting to {}:{}", cfg.host, cfg.port))),
    }
}

fn my_err(e: mysql_async::Error) -> DbError {
    DbError::from_display(e)
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

fn get_s(r: &Row, i: usize) -> String {
    r.as_ref(i).and_then(text).unwrap_or_default()
}

fn get_opt(r: &Row, i: usize) -> Option<String> {
    r.as_ref(i).and_then(text).filter(|s| !s.is_empty())
}

fn type_name(t: ColumnType) -> &'static str {
    use ColumnType::*;
    match t {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => "blob",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => "varchar",
        MYSQL_TYPE_STRING => "char",
        MYSQL_TYPE_NULL => "null",
        _ => "",
    }
}

/// Charset number 63 means binary data (BLOB, BINARY, VARBINARY).
const BINARY_CHARSET: u16 = 63;

#[async_trait]
impl Connection for MySqlConnection {
    fn kind(&self) -> DbKind {
        DbKind::MySql
    }

    async fn server_version(&self) -> DbResult<String> {
        let rows = self.rows("SELECT CONCAT(VERSION(), ' ', @@version_comment)", ()).await?;
        Ok(rows.first().map(|r| get_s(r, 0)).unwrap_or_default())
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        if self.cfg.only_default_database && !self.cfg.database.is_empty() {
            return Ok(vec![self.cfg.database.clone()]);
        }
        let rows = self.rows("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY 1", ()).await?;
        Ok(rows.iter().map(|r| get_s(r, 0)).filter(|d| !HIDDEN.contains(&d.to_ascii_lowercase().as_str())).collect())
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        Ok(vec![database.to_string()])
    }

    async fn objects(&self, database: &str, _schema: &str) -> DbResult<Vec<DbObject>> {
        let path = |name: String| ObjectPath::new(database, database, name);
        let mut out = Vec::new();
        for r in self
            .rows(
                "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT FROM information_schema.TABLES
                 WHERE TABLE_SCHEMA = ? ORDER BY 1",
                (database,),
            )
            .await?
        {
            let kind = match get_s(&r, 1).as_str() {
                "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
                "SEQUENCE" => ObjectKind::Sequence,
                _ => ObjectKind::Table,
            };
            out.push(DbObject { kind, path: path(get_s(&r, 0)), detail: None, comment: get_opt(&r, 2) });
        }
        for r in self
            .rows(
                "SELECT ROUTINE_NAME, ROUTINE_TYPE, ROUTINE_COMMENT FROM information_schema.ROUTINES
                 WHERE ROUTINE_SCHEMA = ? ORDER BY 1",
                (database,),
            )
            .await?
        {
            let kind = if get_s(&r, 1) == "PROCEDURE" { ObjectKind::Procedure } else { ObjectKind::Function };
            out.push(DbObject { kind, path: path(get_s(&r, 0)), detail: None, comment: get_opt(&r, 2) });
        }
        for r in self
            .rows(
                "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS
                 WHERE TRIGGER_SCHEMA = ? ORDER BY 1",
                (database,),
            )
            .await?
        {
            out.push(DbObject {
                kind: ObjectKind::Trigger,
                path: path(get_s(&r, 0)),
                detail: get_opt(&r, 1),
                comment: None,
            });
        }
        Ok(out)
    }

    async fn table_details(&self, p: &ObjectPath) -> DbResult<TableDetails> {
        let args = (p.database.as_str(), p.name.as_str());
        let columns = self
            .rows(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_KEY, EXTRA, COLUMN_COMMENT
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                 ORDER BY ORDINAL_POSITION",
                args,
            )
            .await?
            .iter()
            .map(|r| ColumnInfo {
                name: get_s(r, 0),
                data_type: get_s(r, 1),
                nullable: get_s(r, 2) == "YES",
                default: r.as_ref(3).and_then(text),
                primary_key: get_s(r, 4) == "PRI",
                auto_increment: get_s(r, 5).to_ascii_lowercase().contains("auto_increment"),
                comment: get_opt(r, 6),
            })
            .collect();

        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in self
            .rows(
                "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE FROM information_schema.STATISTICS
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                 ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
                args,
            )
            .await?
        {
            let name = get_s(&r, 0);
            let column = get_s(&r, 2);
            match indexes.last_mut() {
                Some(i) if i.name == name => i.columns.push(column),
                _ => indexes.push(IndexInfo {
                    primary: name == "PRIMARY",
                    unique: get_s(&r, 1) == "0",
                    method: get_opt(&r, 3).map(|m| m.to_ascii_lowercase()),
                    columns: vec![column],
                    name,
                }),
            }
        }

        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in self
            .rows(
                "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME,
                        k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE
                 FROM information_schema.KEY_COLUMN_USAGE k
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
                   ON rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME
                  AND rc.TABLE_NAME = k.TABLE_NAME
                 WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL
                 ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                args,
            )
            .await?
        {
            let name = get_s(&r, 0);
            match foreign_keys.last_mut() {
                Some(f) if f.name == name => {
                    f.columns.push(get_s(&r, 1));
                    f.ref_columns.push(get_s(&r, 4));
                }
                _ => foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns: vec![get_s(&r, 1)],
                    ref_schema: get_s(&r, 2),
                    ref_table: get_s(&r, 3),
                    ref_columns: vec![get_s(&r, 4)],
                    on_update: get_s(&r, 5),
                    on_delete: get_s(&r, 6),
                }),
            }
        }

        let comment = self
            .rows("SELECT TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?", args)
            .await?
            .first()
            .and_then(|r| get_opt(r, 0));
        Ok(TableDetails { columns, indexes, foreign_keys, comment })
    }

    async fn ddl(&self, o: &DbObject) -> DbResult<String> {
        let d = Dialect(DbKind::MySql);
        let q = d.qualified(&o.path);
        let (what, col) = match o.kind {
            ObjectKind::View => ("VIEW", 1),
            ObjectKind::Function => ("FUNCTION", 2),
            ObjectKind::Procedure => ("PROCEDURE", 2),
            ObjectKind::Trigger => ("TRIGGER", 2),
            ObjectKind::Sequence => ("SEQUENCE", 1),
            _ => ("TABLE", 1),
        };
        let rows = self.rows(&format!("SHOW CREATE {what} {q}"), ()).await?;
        let ddl = rows.first().map(|r| get_s(r, col)).unwrap_or_default();
        Ok(format!("{ddl};"))
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let mut s = self.session.lock().await;
        if let Some(db) = database.filter(|d| !d.is_empty())
            && s.current_db.as_deref() != Some(db)
        {
            s.conn.query_drop(format!("USE {}", Dialect(DbKind::MySql).ident(db))).await.map_err(my_err)?;
            s.current_db = Some(db.to_string());
        }
        let mut result = s.conn.query_iter(sql).await.map_err(my_err)?;
        let cols = result.columns();
        let out = match cols.filter(|c| !c.is_empty()) {
            Some(cols) => {
                let binary: Vec<bool> = cols
                    .iter()
                    .map(|c| {
                        c.character_set() == BINARY_CHARSET
                            && !c.flags().contains(ColumnFlags::NUM_FLAG)
                            && matches!(
                                c.column_type(),
                                ColumnType::MYSQL_TYPE_TINY_BLOB
                                    | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                                    | ColumnType::MYSQL_TYPE_LONG_BLOB
                                    | ColumnType::MYSQL_TYPE_BLOB
                                    | ColumnType::MYSQL_TYPE_STRING
                                    | ColumnType::MYSQL_TYPE_VAR_STRING
                                    | ColumnType::MYSQL_TYPE_VARCHAR
                                    | ColumnType::MYSQL_TYPE_GEOMETRY
                            )
                    })
                    .collect();
                let columns = cols
                    .iter()
                    .map(|c| ColumnMeta::new(c.name_str().into_owned(), type_name(c.column_type())))
                    .collect();
                let mut qr = QueryResult { columns, rows: Vec::new(), truncated: false };
                while let Some(row) = result.next().await.map_err(my_err)? {
                    if qr.rows.len() >= max_rows {
                        qr.truncated = true;
                        break;
                    }
                    let cells = (0..row.len())
                        .map(|i| match row.as_ref(i) {
                            None | Some(Value::NULL) => Cell::Null,
                            Some(Value::Bytes(b)) if binary[i] => Cell::Bytes(b.clone()),
                            Some(v) => Cell::Text(text(v).unwrap_or_default()),
                        })
                        .collect();
                    qr.rows.push(cells);
                }
                StatementOutcome::Rows(qr)
            }
            None => StatementOutcome::Done { affected: Some(result.affected_rows()) },
        };
        // Reads (and discards) whatever is left, including further result sets of a CALL.
        result.drop_result().await.map_err(my_err)?;
        if sql.trim_start().get(..4).is_some_and(|w| w.eq_ignore_ascii_case("use ")) {
            // The console switched databases itself; select ours again before the next statement.
            s.current_db = None;
        }
        Ok(out)
    }

    async fn cancel(&self) -> DbResult<()> {
        // The session is busy, so the KILL goes through a short-lived second connection.
        let mut killer = open_with_ssl(&self.cfg).await?;
        let r = killer.query_drop(format!("KILL QUERY {}", self.thread_id)).await.map_err(my_err);
        let _ = killer.disconnect().await;
        r
    }
}
