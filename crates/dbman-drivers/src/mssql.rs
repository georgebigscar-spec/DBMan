//! SQL Server driver on `tiberius`.
//!
//! One TDS connection per session; `USE` switches the database when a call
//! names another one. Catalog queries use three-part names
//! (`[db].sys.tables`), so browsing never changes the session's database.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbman_core::ddl::{self, TableDef};
use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, ColumnInfo, ColumnMeta, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ForeignKeyInfo,
    IndexInfo, ObjectKind, ObjectPath, QueryResult, SslMode, StatementOutcome, TableDetails,
};
use futures_util::TryStreamExt;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, QueryItem, Row};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub type MsClient = Client<Compat<TcpStream>>;

struct Session {
    client: MsClient,
    database: String,
}

pub struct MsSqlConnection {
    cfg: ConnectionConfig,
    session: Mutex<Session>,
    /// Wakes the running `execute` so it stops and sends an attention signal.
    cancel: Arc<Notify>,
}

pub(crate) fn ms_err(e: tiberius::error::Error) -> DbError {
    match e {
        tiberius::error::Error::Server(t) => {
            DbError::new(format!("{} (error {}, line {})", t.message(), t.code(), t.line()))
        }
        other => DbError::from_display(other),
    }
}

fn q(s: &str) -> String {
    Dialect(DbKind::MsSql).ident(s)
}

/// Opens a TDS connection for `cfg`, in `database` if given.
pub async fn open_client(cfg: &ConnectionConfig, database: Option<&str>) -> DbResult<MsClient> {
    let mut c = Config::new();
    c.host(&cfg.host);
    c.port(cfg.port);
    c.application_name("DBMan");
    if let Some(db) = database.filter(|d| !d.is_empty()) {
        c.database(db);
    }
    c.authentication(AuthMethod::sql_server(&cfg.user, &cfg.password));
    match cfg.ssl_mode {
        SslMode::Disable => c.encryption(EncryptionLevel::NotSupported),
        SslMode::Prefer => {
            c.encryption(EncryptionLevel::On);
            c.trust_cert();
        }
        SslMode::Require => {
            c.encryption(EncryptionLevel::Required);
            c.trust_cert();
        }
        SslMode::VerifyFull => c.encryption(EncryptionLevel::Required),
    }
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(c.get_addr()))
        .await
        .map_err(|_| DbError::new(format!("connection to {}:{} timed out", cfg.host, cfg.port)))?
        .map_err(|e| DbError::new(format!("{}:{}: {e}", cfg.host, cfg.port)))?;
    tcp.set_nodelay(true).map_err(DbError::from_display)?;
    Client::connect(c, tcp.compat_write()).await.map_err(ms_err)
}

/// Text of a value as SQL Server Management Studio shows it.
fn cell(data: &ColumnData<'static>) -> Cell {
    fn t(s: impl ToString) -> Cell {
        Cell::Text(s.to_string())
    }
    let fmt_dt = |d: chrono::NaiveDateTime| {
        let s = d.format("%Y-%m-%d %H:%M:%S%.f").to_string();
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    };
    match data {
        ColumnData::U8(Some(v)) => t(v),
        ColumnData::I16(Some(v)) => t(v),
        ColumnData::I32(Some(v)) => t(v),
        ColumnData::I64(Some(v)) => t(v),
        ColumnData::F32(Some(v)) => t(v),
        ColumnData::F64(Some(v)) => t(v),
        ColumnData::Bit(Some(v)) => t(if *v { "1" } else { "0" }),
        ColumnData::String(Some(s)) => t(s),
        ColumnData::Guid(Some(g)) => t(g.to_string().to_uppercase()),
        ColumnData::Binary(Some(b)) => Cell::Bytes(b.to_vec()),
        ColumnData::Numeric(Some(n)) => t(n),
        ColumnData::Xml(Some(x)) => t(x.as_ref()),
        ColumnData::DateTime(Some(_)) | ColumnData::SmallDateTime(Some(_)) | ColumnData::DateTime2(Some(_)) => {
            chrono::NaiveDateTime::from_sql(data).ok().flatten().map_or(Cell::Null, |d| t(fmt_dt(d)))
        }
        ColumnData::Date(Some(_)) => chrono::NaiveDate::from_sql(data).ok().flatten().map_or(Cell::Null, t),
        ColumnData::Time(Some(_)) => chrono::NaiveTime::from_sql(data).ok().flatten().map_or(Cell::Null, t),
        ColumnData::DateTimeOffset(Some(_)) => chrono::DateTime::<chrono::FixedOffset>::from_sql(data)
            .ok()
            .flatten()
            .map_or(Cell::Null, |d| t(format!("{} {}", fmt_dt(d.naive_local()), d.format("%:z")))),
        _ => Cell::Null,
    }
}

fn type_name(t: ColumnType) -> &'static str {
    use ColumnType as T;
    match t {
        T::Null => "null",
        T::Bit | T::Bitn => "bit",
        T::Int1 => "tinyint",
        T::Int2 => "smallint",
        T::Int4 | T::Intn => "int",
        T::Int8 => "bigint",
        T::Float4 => "real",
        T::Float8 | T::Floatn => "float",
        T::Money | T::Money4 => "money",
        T::Decimaln => "decimal",
        T::Numericn => "numeric",
        T::Guid => "uniqueidentifier",
        T::Datetime | T::Datetimen => "datetime",
        T::Datetime4 => "smalldatetime",
        T::Daten => "date",
        T::Timen => "time",
        T::Datetime2 => "datetime2",
        T::DatetimeOffsetn => "datetimeoffset",
        T::BigVarBin | T::BigBinary | T::Image => "varbinary",
        T::BigVarChar | T::BigChar | T::Text => "varchar",
        T::NVarchar | T::NChar | T::NText => "nvarchar",
        T::Xml => "xml",
        T::Udt => "udt",
        T::SSVariant => "sql_variant",
    }
}

/// True for statements whose row count is worth reporting.
fn is_dml(sql: &str) -> bool {
    let tokens = dbman_core::sql::tokenize(sql, DbKind::MsSql);
    let first = tokens
        .iter()
        .find(|t| !matches!(t.kind, dbman_core::sql::TokenKind::Whitespace | dbman_core::sql::TokenKind::Comment));
    first.is_some_and(|t| {
        let w = sql[t.range.clone()].to_ascii_lowercase();
        matches!(w.as_str(), "insert" | "update" | "delete" | "merge")
    })
}

const ROWCOUNT: &str = "__dbman_rowcount";

/// Runs a batch, keeping the first result set (or the row count of DML).
async fn run(client: &mut MsClient, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
    let dml = is_dml(sql);
    let batch = if dml { format!("{sql}\n;SELECT @@ROWCOUNT AS {ROWCOUNT}") } else { sql.to_string() };
    let mut stream = client.simple_query(batch).await.map_err(ms_err)?;
    let mut result: Option<QueryResult> = None;
    let mut affected = None;
    let mut index = None;
    while let Some(item) = stream.try_next().await.map_err(ms_err)? {
        match item {
            QueryItem::Metadata(m) => {
                let cols = m.columns();
                if dml && cols.len() == 1 && cols[0].name() == ROWCOUNT {
                    index = Some(m.result_index());
                    continue;
                }
                if result.is_none() {
                    index = Some(m.result_index());
                    let columns = cols.iter().map(|c| ColumnMeta::new(c.name(), type_name(c.column_type()))).collect();
                    result = Some(QueryResult { columns, rows: Vec::new(), truncated: false });
                } else {
                    index = None;
                }
            }
            QueryItem::Row(row) => {
                if row.columns().len() == 1 && row.columns()[0].name() == ROWCOUNT && dml {
                    affected = row.get::<i32, _>(0).map(|n| n as u64);
                    continue;
                }
                if index != Some(row.result_index()) {
                    continue;
                }
                if let Some(r) = result.as_mut() {
                    if r.rows.len() >= max_rows {
                        r.truncated = true;
                        continue;
                    }
                    r.rows.push(row.cells().map(|(_, d)| cell(d)).collect());
                }
            }
        }
    }
    Ok(match result {
        Some(r) => StatementOutcome::Rows(r),
        None => StatementOutcome::Done { affected },
    })
}

fn get_str(r: &Row, i: usize) -> String {
    r.get::<&str, _>(i).unwrap_or_default().to_string()
}

/// `((1))` -> `1`: SQL Server wraps default expressions in parentheses.
fn strip_parens(s: &str) -> String {
    let mut s = s.trim();
    while s.starts_with('(') && s.ends_with(')') && balanced(&s[1..s.len() - 1]) {
        s = s[1..s.len() - 1].trim();
    }
    s.to_string()
}

fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// The type as written in DDL from `sys.types` name, length, precision and scale.
fn full_type(name: &str, max_length: i16, precision: u8, scale: u8) -> String {
    match name {
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({})", max_length / 2)
            }
        }
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({max_length})")
            }
        }
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" if scale != 7 => format!("{name}({scale})"),
        _ => name.to_string(),
    }
}

impl MsSqlConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let client = open_client(cfg, Some(&cfg.database)).await?;
        let mut me = Session { client, database: String::new() };
        me.database = current_db(&mut me.client).await?;
        Ok(Self { cfg: cfg.clone(), session: Mutex::new(me), cancel: Arc::new(Notify::new()) })
    }

    /// Rows of a catalog query with `@P1…` parameters.
    async fn rows(&self, sql: &str, params: &[&dyn tiberius::ToSql]) -> DbResult<Vec<Row>> {
        let mut s = self.session.lock().await;
        let stream = s.client.query(sql, params).await.map_err(ms_err)?;
        stream.into_first_result().await.map_err(ms_err)
    }

    async fn strings(&self, sql: &str, params: &[&dyn tiberius::ToSql]) -> DbResult<Vec<String>> {
        Ok(self.rows(sql, params).await?.iter().map(|r| get_str(r, 0)).collect())
    }
}

async fn current_db(client: &mut MsClient) -> DbResult<String> {
    let row = client.simple_query("SELECT DB_NAME()").await.map_err(ms_err)?.into_row().await.map_err(ms_err)?;
    Ok(row.map(|r| get_str(&r, 0)).unwrap_or_default())
}

/// `sys.objects.type` codes and how the explorer shows them.
fn object_kind(code: &str) -> Option<ObjectKind> {
    Some(match code.trim() {
        "U" => ObjectKind::Table,
        "V" => ObjectKind::View,
        "P" | "PC" => ObjectKind::Procedure,
        "FN" | "IF" | "TF" | "FS" | "FT" => ObjectKind::Function,
        "TR" => ObjectKind::Trigger,
        "SO" => ObjectKind::Sequence,
        _ => return None,
    })
}

#[async_trait]
impl Connection for MsSqlConnection {
    fn kind(&self) -> DbKind {
        DbKind::MsSql
    }

    async fn server_version(&self) -> DbResult<String> {
        let v = self.strings("SELECT @@VERSION", &[]).await?;
        Ok(v.first().and_then(|s| s.lines().next()).unwrap_or_default().trim().to_string())
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        if self.cfg.only_default_database && !self.cfg.database.is_empty() {
            return Ok(vec![self.cfg.database.clone()]);
        }
        self.strings(
            "SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1
             ORDER BY CASE WHEN database_id > 4 THEN 0 ELSE 1 END, name",
            &[],
        )
        .await
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        let db = q(database);
        self.strings(
            &format!(
                "SELECT name FROM {db}.sys.schemas
                 WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest')
                 ORDER BY CASE name WHEN 'dbo' THEN 0 ELSE 1 END, name"
            ),
            &[],
        )
        .await
    }

    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>> {
        let db = q(database);
        let rows = self
            .rows(
                &format!(
                    "SELECT o.name, o.type, p.name,
                            CAST(ep.value AS nvarchar(4000))
                     FROM {db}.sys.objects o
                     JOIN {db}.sys.schemas s ON s.schema_id = o.schema_id
                     LEFT JOIN {db}.sys.objects p ON p.object_id = o.parent_object_id AND o.type = 'TR'
                     LEFT JOIN {db}.sys.extended_properties ep
                       ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description'
                     WHERE s.name = @P1 AND o.is_ms_shipped = 0
                       AND o.type IN ('U', 'V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT', 'TR', 'SO')
                     ORDER BY o.name"
                ),
                &[&schema],
            )
            .await?;
        let mut out: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let kind = object_kind(r.get::<&str, _>(1)?)?;
                Some(DbObject {
                    kind,
                    path: ObjectPath::new(database, schema, get_str(r, 0)),
                    detail: r.get::<&str, _>(2).map(String::from),
                    comment: r.get::<&str, _>(3).map(String::from).filter(|c| !c.is_empty()),
                })
            })
            .collect();
        let types = self
            .strings(
                &format!(
                    "SELECT t.name FROM {db}.sys.types t JOIN {db}.sys.schemas s ON s.schema_id = t.schema_id
                     WHERE t.is_user_defined = 1 AND s.name = @P1 ORDER BY t.name"
                ),
                &[&schema],
            )
            .await?;
        out.extend(types.into_iter().map(|name| DbObject {
            kind: ObjectKind::Type,
            path: ObjectPath::new(database, schema, name),
            detail: None,
            comment: None,
        }));
        Ok(out)
    }

    async fn table_details(&self, p: &ObjectPath) -> DbResult<TableDetails> {
        let db = q(&p.database);
        let full = format!("{db}.{}", Dialect(DbKind::MsSql).qualified(p));
        let args: [&dyn tiberius::ToSql; 1] = [&full];
        let cols = self
            .rows(
                &format!(
                    "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity,
                            dc.definition, CAST(ep.value AS nvarchar(4000)),
                            CASE WHEN EXISTS (
                                SELECT 1 FROM {db}.sys.indexes i
                                JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                                WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id
                            ) THEN 1 ELSE 0 END
                     FROM {db}.sys.columns c
                     JOIN {db}.sys.types t ON t.user_type_id = c.user_type_id
                     LEFT JOIN {db}.sys.default_constraints dc ON dc.object_id = c.default_object_id
                     LEFT JOIN {db}.sys.extended_properties ep
                       ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = 'MS_Description'
                     WHERE c.object_id = OBJECT_ID(@P1)
                     ORDER BY c.column_id"
                ),
                &args,
            )
            .await?;
        if cols.is_empty() {
            return Err(DbError::new(format!("{full} not found")));
        }
        let columns = cols
            .iter()
            .map(|r| ColumnInfo {
                name: get_str(r, 0),
                data_type: full_type(
                    r.get::<&str, _>(1).unwrap_or(""),
                    r.get::<i16, _>(2).unwrap_or(0),
                    r.get::<u8, _>(3).unwrap_or(0),
                    r.get::<u8, _>(4).unwrap_or(0),
                ),
                nullable: r.get::<bool, _>(5).unwrap_or(true),
                auto_increment: r.get::<bool, _>(6).unwrap_or(false),
                default: r.get::<&str, _>(7).map(strip_parens),
                comment: r.get::<&str, _>(8).map(String::from).filter(|c| !c.is_empty()),
                primary_key: r.get::<i32, _>(9) == Some(1),
            })
            .collect();

        let idx = self
            .rows(
                &format!(
                    "SELECT i.name, i.is_unique, i.is_primary_key, LOWER(i.type_desc), c.name
                     FROM {db}.sys.indexes i
                     JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                     JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                     WHERE i.object_id = OBJECT_ID(@P1) AND i.type > 0 AND ic.is_included_column = 0
                     ORDER BY i.name, ic.key_ordinal"
                ),
                &args,
            )
            .await?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in &idx {
            let name = get_str(r, 0);
            match indexes.last_mut() {
                Some(i) if i.name == name => i.columns.push(get_str(r, 4)),
                _ => indexes.push(IndexInfo {
                    name,
                    columns: vec![get_str(r, 4)],
                    unique: r.get::<bool, _>(1).unwrap_or(false),
                    primary: r.get::<bool, _>(2).unwrap_or(false),
                    method: r.get::<&str, _>(3).map(String::from),
                }),
            }
        }

        let fk = self
            .rows(
                &format!(
                    "SELECT f.name, pc.name, rs.name, rt.name, rc.name,
                            f.update_referential_action_desc, f.delete_referential_action_desc
                     FROM {db}.sys.foreign_keys f
                     JOIN {db}.sys.foreign_key_columns fc ON fc.constraint_object_id = f.object_id
                     JOIN {db}.sys.columns pc ON pc.object_id = fc.parent_object_id AND pc.column_id = fc.parent_column_id
                     JOIN {db}.sys.tables rt ON rt.object_id = fc.referenced_object_id
                     JOIN {db}.sys.schemas rs ON rs.schema_id = rt.schema_id
                     JOIN {db}.sys.columns rc ON rc.object_id = fc.referenced_object_id AND rc.column_id = fc.referenced_column_id
                     WHERE f.parent_object_id = OBJECT_ID(@P1)
                     ORDER BY f.name, fc.constraint_column_id"
                ),
                &args,
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        let action = |r: &Row, i: usize| get_str(r, i).replace('_', " ");
        for r in &fk {
            let name = get_str(r, 0);
            match foreign_keys.last_mut() {
                Some(f) if f.name == name => {
                    f.columns.push(get_str(r, 1));
                    f.ref_columns.push(get_str(r, 4));
                }
                _ => foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns: vec![get_str(r, 1)],
                    ref_schema: get_str(r, 2),
                    ref_table: get_str(r, 3),
                    ref_columns: vec![get_str(r, 4)],
                    on_update: action(r, 5),
                    on_delete: action(r, 6),
                }),
            }
        }
        let comment = self
            .strings(
                &format!(
                    "SELECT CAST(value AS nvarchar(4000)) FROM {db}.sys.extended_properties
                     WHERE class = 1 AND major_id = OBJECT_ID(@P1) AND minor_id = 0 AND name = 'MS_Description'"
                ),
                &args,
            )
            .await?
            .into_iter()
            .next()
            .filter(|c| !c.is_empty());
        Ok(TableDetails { columns, indexes, foreign_keys, comment })
    }

    async fn ddl(&self, o: &DbObject) -> DbResult<String> {
        let db = q(&o.path.database);
        let full = format!("{db}.{}", Dialect(DbKind::MsSql).qualified(&o.path));
        match o.kind {
            ObjectKind::Table => {
                let details = self.table_details(&o.path).await?;
                let mut def = TableDef::from_details(o.path.clone(), &details);
                def.path.database.clear();
                Ok(ddl::create_table(DbKind::MsSql, &def).script())
            }
            ObjectKind::Sequence => {
                let rows = self
                    .rows(
                        &format!(
                            "SELECT TYPE_NAME(user_type_id), CAST(start_value AS nvarchar(40)),
                                    CAST(increment AS nvarchar(40))
                             FROM {db}.sys.sequences WHERE object_id = OBJECT_ID(@P1)"
                        ),
                        &[&full],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| DbError::new(format!("{full} not found")))?;
                Ok(format!(
                    "CREATE SEQUENCE {} AS {} START WITH {} INCREMENT BY {};\n",
                    Dialect(DbKind::MsSql).qualified(&o.path),
                    get_str(r, 0),
                    get_str(r, 1),
                    get_str(r, 2)
                ))
            }
            ObjectKind::Type => {
                let rows = self
                    .rows(
                        &format!(
                            "SELECT bt.name, t.max_length, t.precision, t.scale, t.is_nullable
                             FROM {db}.sys.types t
                             JOIN {db}.sys.schemas s ON s.schema_id = t.schema_id
                             JOIN {db}.sys.types bt ON bt.user_type_id = t.system_type_id
                             WHERE s.name = @P1 AND t.name = @P2"
                        ),
                        &[&o.path.schema.as_str(), &o.path.name.as_str()],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| DbError::new(format!("{full} not found")))?;
                let ty = full_type(
                    r.get::<&str, _>(0).unwrap_or(""),
                    r.get::<i16, _>(1).unwrap_or(0),
                    r.get::<u8, _>(2).unwrap_or(0),
                    r.get::<u8, _>(3).unwrap_or(0),
                );
                let null = if r.get::<bool, _>(4).unwrap_or(true) { "" } else { " NOT NULL" };
                Ok(format!("CREATE TYPE {} FROM {ty}{null};\n", Dialect(DbKind::MsSql).qualified(&o.path)))
            }
            _ => {
                let defs = self
                    .strings(
                        &format!("SELECT definition FROM {db}.sys.sql_modules WHERE object_id = OBJECT_ID(@P1)"),
                        &[&full],
                    )
                    .await?;
                let def = defs.into_iter().next().ok_or_else(|| DbError::new(format!("no definition for {full}")))?;
                Ok(format!("{}\nGO\n", def.trim()))
            }
        }
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let mut s = self.session.lock().await;
        if let Some(db) = database.filter(|d| !d.is_empty() && *d != s.database) {
            s.client
                .simple_query(format!("USE {}", q(db)))
                .await
                .map_err(ms_err)?
                .into_results()
                .await
                .map_err(ms_err)?;
            s.database = db.to_string();
        }
        let cancel = self.cancel.clone();
        let outcome = {
            let client = &mut s.client;
            tokio::select! {
                r = run(client, sql, max_rows) => Some(r),
                _ = cancel.notified() => None,
            }
        };
        match outcome {
            Some(r) => {
                // `USE` inside the statement switches the session's database.
                if r.is_ok() && sql.trim_start().get(..4).is_some_and(|w| w.eq_ignore_ascii_case("use ")) {
                    s.database = current_db(&mut s.client).await.unwrap_or_default();
                }
                r
            }
            None => {
                // The query future is gone. tiberius cannot reliably resync a
                // connection after an attention signal, so the session starts
                // over on a new connection (closing the old one ends the
                // statement on the server and rolls back its transaction).
                let fresh = open_client(&self.cfg, Some(&s.database)).await?;
                let old = std::mem::replace(&mut s.client, fresh);
                drop(old);
                Err(DbError::new("cancelled; the session was reopened, so an open transaction was rolled back"))
            }
        }
    }

    async fn cancel(&self) -> DbResult<()> {
        // `notify_one` stores a permit when nothing waits, so only wake a running query.
        self.cancel.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(strip_parens("((1))"), "1");
        assert_eq!(strip_parens("(getdate())"), "getdate()");
        assert_eq!(strip_parens("(1)+(2)"), "(1)+(2)");
        assert_eq!(full_type("nvarchar", -1, 0, 0), "nvarchar(max)");
        assert_eq!(full_type("nvarchar", 200, 0, 0), "nvarchar(100)");
        assert_eq!(full_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(full_type("datetime2", 8, 27, 7), "datetime2");
        assert!(is_dml("  -- x\n UPDATE t SET a = 1"));
        assert!(!is_dml("SELECT 1"));
    }
}
