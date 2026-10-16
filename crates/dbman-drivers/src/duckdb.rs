//! DuckDB driver on the `duckdb` crate (bundled DuckDB).
//!
//! DuckDB is embedded and synchronous, so calls run on tokio's blocking pool.
//! A database file can be opened only once per process, so every session on
//! the same file shares one database instance and gets its own connection
//! from it (`try_clone`). Databases are the attached catalogs; each has schemas.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use async_trait::async_trait;
use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, ColumnInfo, ColumnMeta, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ForeignKeyInfo,
    IndexInfo, ObjectKind, ObjectPath, QueryResult, StatementOutcome, TableDetails,
};
use duckdb::types::{TimeUnit, Value};

type Root = Arc<Mutex<duckdb::Connection>>;

/// Open databases by canonical path; an entry lives while a session holds its root.
fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<Mutex<duckdb::Connection>>>> {
    static R: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<duckdb::Connection>>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// A connection to a database file that shares the instance with every other
/// session on the same file. Keep it whole: `root` keeps the instance findable.
pub struct Shared {
    pub conn: duckdb::Connection,
    _root: Option<Root>,
}

/// A new connection to the database file at `path` (`:memory:` for a private
/// in-memory database), sharing the instance with other sessions on it.
pub fn open_shared(path: &str) -> DbResult<Shared> {
    if path == ":memory:" {
        let conn = duckdb::Connection::open_in_memory().map_err(DbError::from_display)?;
        return Ok(Shared { conn, _root: None });
    }
    // Canonicalize so different spellings of one file share the instance.
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| {
        let p = Path::new(path);
        if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(p) }
    });
    let mut reg = registry().lock().map_err(|_| DbError::new("registry poisoned"))?;
    reg.retain(|_, w| w.strong_count() > 0);
    if let Some(root) = reg.get(&key).and_then(Weak::upgrade) {
        let conn = root.lock().map_err(|_| DbError::new("connection poisoned"))?.try_clone();
        return Ok(Shared { conn: conn.map_err(DbError::from_display)?, _root: Some(root) });
    }
    let first = duckdb::Connection::open(&key).map_err(|e| DbError::new(format!("{}: {e}", key.display())))?;
    let conn = first.try_clone().map_err(DbError::from_display)?;
    let root = Arc::new(Mutex::new(first));
    reg.insert(key, Arc::downgrade(&root));
    Ok(Shared { conn, _root: Some(root) })
}

pub struct DuckDbConnection {
    conn: Arc<Mutex<duckdb::Connection>>,
    interrupt: Arc<duckdb::InterruptHandle>,
    _root: Option<Root>,
}

impl DuckDbConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let path = cfg.database.trim().to_string();
        if path.is_empty() {
            return Err(DbError::new("choose a database file (or :memory:)"));
        }
        tokio::task::spawn_blocking(move || {
            let Shared { conn, _root } = open_shared(&path)?;
            let interrupt = conn.interrupt_handle();
            Ok(Self { conn: Arc::new(Mutex::new(conn)), interrupt, _root })
        })
        .await
        .map_err(DbError::from_display)?
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&duckdb::Connection) -> duckdb::Result<T> + Send + 'static,
    ) -> DbResult<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let c = conn.lock().map_err(|_| DbError::new("connection poisoned"))?;
            f(&c).map_err(DbError::from_display)
        })
        .await
        .map_err(DbError::from_display)?
    }
}

fn q(s: &str) -> String {
    Dialect(DbKind::DuckDb).ident(s)
}

/// `YYYY-MM-DD` for days since 1970-01-01.
fn date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn micros(unit: TimeUnit, v: i64) -> i64 {
    match unit {
        TimeUnit::Second => v * 1_000_000,
        TimeUnit::Millisecond => v * 1_000,
        TimeUnit::Microsecond => v,
        TimeUnit::Nanosecond => v / 1_000,
    }
}

/// `HH:MM:SS[.ffffff]` for microseconds since midnight.
fn time_of_day(us: i64) -> String {
    let (s, frac) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let base = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    if frac == 0 { base } else { format!("{base}.{frac:06}").trim_end_matches('0').to_string() }
}

/// DuckDB's own text form of a value, as `SELECT v::VARCHAR` would print it.
fn text(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(i) => i.to_string(),
        Value::SmallInt(i) => i.to_string(),
        Value::Int(i) => i.to_string(),
        Value::BigInt(i) => i.to_string(),
        Value::HugeInt(i) => i.to_string(),
        Value::UHugeInt(i) => i.to_string(),
        Value::UTinyInt(i) => i.to_string(),
        Value::USmallInt(i) => i.to_string(),
        Value::UInt(i) => i.to_string(),
        Value::UBigInt(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Timestamp(unit, t) => {
            let us = micros(*unit, *t);
            format!("{} {}", date(us.div_euclid(86_400_000_000)), time_of_day(us.rem_euclid(86_400_000_000)))
        }
        Value::Text(s) | Value::Enum(s) => s.clone(),
        Value::Blob(b) | Value::Geometry(b) => b.iter().map(|x| format!("\\x{x:02X}")).collect(),
        Value::Date32(d) => date(i64::from(*d)),
        Value::Time64(unit, t) => time_of_day(micros(*unit, *t)),
        Value::Interval { months, days, nanos } => {
            let mut parts = Vec::new();
            if *months != 0 {
                parts.push(format!("{months} months"));
            }
            if *days != 0 {
                parts.push(format!("{days} days"));
            }
            if *nanos != 0 || parts.is_empty() {
                parts.push(time_of_day(nanos / 1_000));
            }
            parts.join(" ")
        }
        Value::List(items) | Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(|i| text(i).unwrap_or_else(|| "NULL".into())).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Struct(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(k, v)| format!("'{k}': {}", text(v).map_or("NULL".into(), |t| quoted(v, t))))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Map(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{}={}", text(k).unwrap_or_default(), text(v).unwrap_or_else(|| "NULL".into())))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Union(inner) => return text(inner),
        other => format!("{other:?}"),
    })
}

fn quoted(v: &Value, t: String) -> String {
    if matches!(v, Value::Text(_) | Value::Enum(_)) { format!("'{t}'") } else { t }
}

fn cell(v: Value) -> Cell {
    match v {
        Value::Blob(b) => Cell::Bytes(b),
        other => text(&other).map_or(Cell::Null, Cell::Text),
    }
}

/// DuckDB's name for a result column's (Arrow) type.
fn type_name(t: &duckdb::arrow::datatypes::DataType) -> String {
    use duckdb::arrow::datatypes::DataType as T;
    match t {
        T::Boolean => "boolean",
        T::Int8 => "tinyint",
        T::Int16 => "smallint",
        T::Int32 => "integer",
        T::Int64 => "bigint",
        T::UInt8 => "utinyint",
        T::UInt16 => "usmallint",
        T::UInt32 => "uinteger",
        T::UInt64 => "ubigint",
        T::Float16 | T::Float32 => "float",
        T::Float64 => "double",
        T::Decimal128(p, s) | T::Decimal256(p, s) => return format!("decimal({p},{s})"),
        T::Utf8 | T::LargeUtf8 | T::Utf8View => "varchar",
        T::Binary | T::LargeBinary | T::BinaryView | T::FixedSizeBinary(_) => "blob",
        T::Date32 | T::Date64 => "date",
        T::Time32(_) | T::Time64(_) => "time",
        T::Timestamp(_, Some(_)) => "timestamptz",
        T::Timestamp(_, None) => "timestamp",
        T::Interval(_) | T::Duration(_) => "interval",
        T::List(_) | T::LargeList(_) | T::FixedSizeList(..) => "list",
        T::Struct(_) => "struct",
        T::Map(..) => "map",
        T::Union(..) => "union",
        T::Dictionary(..) => "enum",
        T::Null => "null",
        other => return other.to_string().to_ascii_lowercase(),
    }
    .into()
}

fn run(c: &duckdb::Connection, sql: &str, max_rows: usize) -> duckdb::Result<StatementOutcome> {
    let mut stmt = c.prepare(sql)?;
    let mut rows = stmt.query([])?;
    let Some(st) = rows.as_ref() else {
        return Ok(StatementOutcome::Done { affected: None });
    };
    let n = st.column_count();
    let columns: Vec<ColumnMeta> = (0..n)
        .map(|i| ColumnMeta::new(st.column_name(i).map_or("?", |s| s.as_str()), type_name(&st.column_type(i))))
        .collect();
    // Statements without a result set report a single "Count" column.
    let is_count = n == 1 && columns[0].name == "Count";
    let mut out = QueryResult { columns, rows: Vec::new(), truncated: false };
    while let Some(row) = rows.next()? {
        if out.rows.len() >= max_rows {
            out.truncated = true;
            break;
        }
        out.rows.push((0..n).map(|i| row.get::<_, Value>(i).map(cell).unwrap_or(Cell::Null)).collect());
    }
    if is_count && !sql.trim_start().to_ascii_lowercase().starts_with("select") && out.rows.len() == 1 {
        let affected = match &out.rows[0][0] {
            Cell::Text(t) => t.parse().ok(),
            _ => None,
        };
        return Ok(StatementOutcome::Done { affected });
    }
    Ok(StatementOutcome::Rows(out))
}

fn strings(c: &duckdb::Connection, sql: &str, params: &[&dyn duckdb::ToSql]) -> duckdb::Result<Vec<String>> {
    let mut st = c.prepare(sql)?;
    st.query_map(params, |r| r.get::<_, String>(0))?.collect()
}

/// A list column; `duckdb_indexes().expressions` is text like
/// `['"name"', '(lower("x"))', id]`. Quoted identifiers come back unquoted.
fn list(v: Value) -> Vec<String> {
    match v {
        Value::List(items) | Value::Array(items) => items.iter().filter_map(text).collect(),
        Value::Text(s) => split_list(&s).into_iter().map(|p| unquote(&p)).collect(),
        _ => Vec::new(),
    }
}

/// Top-level items of `[a, 'b, c', f(d, e)]`, with single-quoted ones unwrapped.
fn split_list(s: &str) -> Vec<String> {
    let inner = s.trim().strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(s);
    let (mut items, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, None::<char>);
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), _) if ch == q => {
                if chars.peek() == Some(&q) {
                    cur.push(q);
                    chars.next();
                } else {
                    quote = None;
                    if q == '"' {
                        cur.push('"');
                    }
                }
            }
            (Some(_), _) => cur.push(ch),
            (None, '\'') => quote = Some('\''),
            (None, '"') => {
                quote = Some('"');
                cur.push('"');
            }
            (None, '(') => {
                depth += 1;
                cur.push(ch);
            }
            (None, ')') => {
                depth -= 1;
                cur.push(ch);
            }
            (None, ',') if depth == 0 => items.push(std::mem::take(&mut cur)),
            (None, _) => cur.push(ch),
        }
    }
    items.push(cur);
    items.into_iter().map(|i| i.trim().to_string()).filter(|i| !i.is_empty()).collect()
}

/// `"a ""b"""` -> `a "b"`; anything that is not one quoted identifier is kept.
fn unquote(s: &str) -> String {
    match s.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) if !inner.replace("\"\"", "").contains('"') => inner.replace("\"\"", "\""),
        _ => s.to_string(),
    }
}

#[async_trait]
impl Connection for DuckDbConnection {
    fn kind(&self) -> DbKind {
        DbKind::DuckDb
    }

    async fn server_version(&self) -> DbResult<String> {
        self.with(|c| c.query_row("SELECT 'DuckDB ' || version()", [], |r| r.get(0))).await
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        self.with(|c| {
            strings(
                c,
                "SELECT database_name FROM duckdb_databases() WHERE NOT internal
                 ORDER BY database_name <> current_database(), database_name",
                &[],
            )
        })
        .await
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        let db = database.to_string();
        self.with(move |c| {
            strings(
                c,
                "SELECT schema_name FROM duckdb_schemas() WHERE database_name = ? AND (NOT internal OR schema_name = 'main')
                 ORDER BY schema_name <> 'main', schema_name",
                &[&db],
            )
        })
        .await
    }

    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>> {
        let (db, sc) = (database.to_string(), schema.to_string());
        self.with(move |c| {
            let sql = "
                SELECT 'table', table_name, NULL, comment FROM duckdb_tables()
                 WHERE database_name = $1 AND schema_name = $2 AND NOT internal
                UNION ALL
                SELECT 'view', view_name, NULL, comment FROM duckdb_views()
                 WHERE database_name = $1 AND schema_name = $2 AND NOT internal
                UNION ALL
                SELECT 'sequence', sequence_name, NULL, comment FROM duckdb_sequences()
                 WHERE database_name = $1 AND schema_name = $2
                UNION ALL
                SELECT DISTINCT 'macro', function_name, array_to_string(parameters, ', '), comment FROM duckdb_functions()
                 WHERE database_name = $1 AND schema_name = $2 AND NOT internal
                   AND function_type IN ('macro', 'table_macro')
                UNION ALL
                SELECT 'type', type_name, logical_type, comment FROM duckdb_types()
                 WHERE database_name = $1 AND schema_name = $2 AND NOT internal
                ORDER BY 2";
            let mut st = c.prepare(sql)?;
            let rows = st.query_map([&db, &sc], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (ty, name, detail, comment) = r?;
                let kind = match ty.as_str() {
                    "view" => ObjectKind::View,
                    "sequence" => ObjectKind::Sequence,
                    "macro" => ObjectKind::Function,
                    "type" => ObjectKind::Type,
                    _ => ObjectKind::Table,
                };
                out.push(DbObject {
                    kind,
                    path: ObjectPath::new(&db, &sc, name),
                    detail,
                    comment: comment.filter(|c| !c.is_empty()),
                });
            }
            Ok(out)
        })
        .await
    }

    async fn table_details(&self, p: &ObjectPath) -> DbResult<TableDetails> {
        let p = p.clone();
        self.with(move |c| {
            let args: [&dyn duckdb::ToSql; 3] = [&p.database, &p.schema, &p.name];
            let mut st = c.prepare(
                "SELECT column_name, data_type, is_nullable, column_default, comment FROM duckdb_columns()
                 WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY column_index",
            )?;
            let mut columns: Vec<ColumnInfo> = st
                .query_map(&args[..], |r| {
                    Ok(ColumnInfo {
                        name: r.get(0)?,
                        data_type: r.get(1)?,
                        nullable: r.get(2)?,
                        default: r.get(3)?,
                        primary_key: false,
                        auto_increment: false,
                        comment: r.get::<_, Option<String>>(4)?.filter(|c| !c.is_empty()),
                    })
                })?
                .collect::<Result<_, _>>()?;

            let mut st = c.prepare(
                "SELECT constraint_type, constraint_column_names, constraint_name,
                        referenced_table, referenced_column_names
                 FROM duckdb_constraints()
                 WHERE database_name = ? AND schema_name = ? AND table_name = ?
                   AND constraint_type IN ('PRIMARY KEY', 'FOREIGN KEY', 'UNIQUE')
                 ORDER BY constraint_index",
            )?;
            let mut foreign_keys = Vec::new();
            let mut indexes = Vec::new();
            let mut rows = st.query(&args[..])?;
            while let Some(r) = rows.next()? {
                let ty: String = r.get(0)?;
                let cols = list(r.get(1)?);
                let name: Option<String> = r.get(2)?;
                match ty.as_str() {
                    "PRIMARY KEY" => {
                        for col in columns.iter_mut().filter(|c| cols.contains(&c.name)) {
                            col.primary_key = true;
                            col.nullable = false;
                        }
                        indexes.push(IndexInfo {
                            name: name.unwrap_or_else(|| format!("{}_pkey", p.name)),
                            columns: cols,
                            unique: true,
                            primary: true,
                            method: None,
                        });
                    }
                    "UNIQUE" => indexes.push(IndexInfo {
                        name: name.unwrap_or_else(|| format!("{}_{}_key", p.name, cols.join("_"))),
                        columns: cols,
                        unique: true,
                        primary: false,
                        method: None,
                    }),
                    _ => foreign_keys.push(ForeignKeyInfo {
                        name: name.unwrap_or_else(|| format!("{}_{}_fkey", p.name, cols.join("_"))),
                        columns: cols,
                        ref_schema: p.schema.clone(),
                        ref_table: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        ref_columns: list(r.get(4)?),
                        on_update: String::new(),
                        on_delete: String::new(),
                    }),
                }
            }
            let mut st = c.prepare(
                "SELECT index_name, is_unique, expressions FROM duckdb_indexes()
                 WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY index_name",
            )?;
            let mut rows = st.query(&args[..])?;
            while let Some(r) = rows.next()? {
                indexes.push(IndexInfo {
                    name: r.get(0)?,
                    unique: r.get(1)?,
                    columns: list(r.get(2)?),
                    primary: false,
                    method: Some("art".into()),
                });
            }
            let comment: Option<String> = c
                .query_row(
                    "SELECT comment FROM duckdb_tables() WHERE database_name = ? AND schema_name = ? AND table_name = ?",
                    &args[..],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            Ok(TableDetails { columns, indexes, foreign_keys, comment: comment.filter(|c| !c.is_empty()) })
        })
        .await
    }

    async fn ddl(&self, o: &DbObject) -> DbResult<String> {
        let (p, kind) = (o.path.clone(), o.kind);
        self.with(move |c| {
            let args: [&dyn duckdb::ToSql; 3] = [&p.database, &p.schema, &p.name];
            let one = |sql: &str| -> duckdb::Result<String> {
                c.query_row(sql, &args[..], |r| r.get::<_, Option<String>>(0)).map(Option::unwrap_or_default)
            };
            let mut out = match kind {
                ObjectKind::View => one(
                    "SELECT sql FROM duckdb_views() WHERE database_name = ? AND schema_name = ? AND view_name = ?",
                )?,
                ObjectKind::Sequence => one(
                    "SELECT sql FROM duckdb_sequences() WHERE database_name = ? AND schema_name = ? AND sequence_name = ?",
                )?,
                ObjectKind::Function => one(
                    "SELECT 'CREATE MACRO ' || function_name || '(' || array_to_string(parameters, ', ') || ') AS '
                            || macro_definition
                     FROM duckdb_functions() WHERE database_name = ? AND schema_name = ? AND function_name = ? LIMIT 1",
                )?,
                ObjectKind::Type => one(
                    "SELECT 'CREATE TYPE ' || type_name || ' AS ' || logical_type
                     FROM duckdb_types() WHERE database_name = ? AND schema_name = ? AND type_name = ?",
                )?,
                _ => one("SELECT sql FROM duckdb_tables() WHERE database_name = ? AND schema_name = ? AND table_name = ?")?,
            };
            if !out.trim_end().ends_with(';') {
                out.push(';');
            }
            out.push('\n');
            if kind == ObjectKind::Table {
                for s in strings(
                    c,
                    "SELECT sql FROM duckdb_indexes()
                     WHERE database_name = ? AND schema_name = ? AND table_name = ? AND sql IS NOT NULL ORDER BY index_name",
                    &args,
                )? {
                    out.push_str(s.trim_end_matches(';'));
                    out.push_str(";\n");
                }
            }
            Ok(out)
        })
        .await
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let sql = sql.to_string();
        let db = database.filter(|d| !d.is_empty()).map(String::from);
        self.with(move |c| {
            // In an aborted transaction only ROLLBACK runs, so the lookup may fail; the statement goes on.
            if let Some(db) = db
                && let Ok(current) = c.query_row::<String, _, _>("SELECT current_database()", [], |r| r.get(0))
                && current != db
            {
                c.execute_batch(&format!("USE {}", q(&db)))?;
            }
            run(c, &sql, max_rows)
        })
        .await
    }

    async fn cancel(&self) -> DbResult<()> {
        self.interrupt.interrupt();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_expressions() {
        assert_eq!(list(Value::Text(r#"['"name"']"#.into())), ["name"]);
        assert_eq!(list(Value::Text(r#"['(lower("name"))', id]"#.into())), [r#"(lower("name"))"#, "id"]);
        assert_eq!(list(Value::Text(r#"['"a, ""b"""', 'f(x, y)']"#.into())), [r#"a, "b""#, "f(x, y)"]);
        assert!(list(Value::Text("[]".into())).is_empty());
    }
}
