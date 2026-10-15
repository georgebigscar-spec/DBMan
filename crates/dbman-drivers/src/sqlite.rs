//! SQLite driver on `rusqlite` (bundled SQLite).
//!
//! rusqlite is synchronous, so every call runs on tokio's blocking pool.
//! Databases are `main` plus attached ones; each has a single implicit schema.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, ColumnInfo, ColumnMeta, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ForeignKeyInfo,
    IndexInfo, ObjectKind, ObjectPath, QueryResult, StatementOutcome, TableDetails,
};
use rusqlite::types::ValueRef;

pub struct SqliteConnection {
    conn: Arc<Mutex<rusqlite::Connection>>,
    /// Interrupts the running statement without taking the connection lock.
    interrupt: rusqlite::InterruptHandle,
}

impl SqliteConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let path = cfg.database.trim().to_string();
        if path.is_empty() {
            return Err(DbError::new("choose a database file"));
        }
        let conn = tokio::task::spawn_blocking(move || -> DbResult<rusqlite::Connection> {
            let c = rusqlite::Connection::open(&path).map_err(DbError::from_display)?;
            c.busy_timeout(std::time::Duration::from_secs(5)).map_err(DbError::from_display)?;
            c.execute_batch("PRAGMA foreign_keys = ON").map_err(DbError::from_display)?;
            Ok(c)
        })
        .await
        .map_err(DbError::from_display)??;
        let interrupt = conn.get_interrupt_handle();
        Ok(Self { conn: Arc::new(Mutex::new(conn)), interrupt })
    }

    /// The underlying connection, for the backup tools.
    pub fn raw(&self) -> Arc<Mutex<rusqlite::Connection>> {
        self.conn.clone()
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
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
    Dialect(DbKind::Sqlite).ident(s)
}

fn cell(v: ValueRef<'_>) -> Cell {
    match v {
        ValueRef::Null => Cell::Null,
        ValueRef::Integer(i) => Cell::Text(i.to_string()),
        ValueRef::Real(f) => Cell::Text(f.to_string()),
        ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Cell::Bytes(b.to_vec()),
    }
}

/// Runs one statement on a locked connection; shared with the backup tools.
pub(crate) fn run(c: &rusqlite::Connection, sql: &str, max_rows: usize) -> rusqlite::Result<StatementOutcome> {
    let mut stmt = c.prepare(sql)?;
    if stmt.column_count() == 0 {
        stmt.raw_execute()?;
        return Ok(StatementOutcome::Done { affected: Some(c.changes()) });
    }
    let columns: Vec<ColumnMeta> = stmt
        .columns()
        .iter()
        .map(|col| ColumnMeta::new(col.name(), col.decl_type().unwrap_or("").to_ascii_lowercase()))
        .collect();
    let n = columns.len();
    let mut out = QueryResult { columns, rows: Vec::new(), truncated: false };
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next()? {
        if out.rows.len() >= max_rows {
            out.truncated = true;
            break;
        }
        out.rows.push((0..n).map(|i| row.get_ref(i).map(cell).unwrap_or(Cell::Null)).collect());
    }
    // Expressions have no declared type; fall back to the type of the first non-null value.
    for (i, col) in out.columns.iter_mut().enumerate() {
        if col.type_name.is_empty() {
            col.numeric = out.rows.iter().map(|r| &r[i]).find(|c| !c.is_null()).is_some_and(|c| match c {
                Cell::Text(s) => s.parse::<f64>().is_ok(),
                _ => false,
            });
        }
    }
    Ok(StatementOutcome::Rows(out))
}

#[async_trait]
impl Connection for SqliteConnection {
    fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }

    async fn server_version(&self) -> DbResult<String> {
        self.with(|c| c.query_row("SELECT 'SQLite ' || sqlite_version()", [], |r| r.get(0))).await
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq")?;
            st.query_map([], |r| r.get(0))?.collect()
        })
        .await
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        Ok(vec![database.to_string()])
    }

    async fn objects(&self, database: &str, _schema: &str) -> DbResult<Vec<DbObject>> {
        let db = database.to_string();
        self.with(move |c| {
            let sql = format!(
                "SELECT type, name, tbl_name FROM {}.sqlite_master
                 WHERE type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite_%' ORDER BY name",
                q(&db)
            );
            let mut st = c.prepare(&sql)?;
            let rows =
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            let mut out = Vec::new();
            for r in rows {
                let (ty, name, tbl) = r?;
                let kind = match ty.as_str() {
                    "view" => ObjectKind::View,
                    "trigger" => ObjectKind::Trigger,
                    _ => ObjectKind::Table,
                };
                let detail = (kind == ObjectKind::Trigger).then_some(tbl);
                out.push(DbObject { kind, path: ObjectPath::new(&db, &db, name), detail, comment: None });
            }
            Ok(out)
        })
        .await
    }

    async fn table_details(&self, p: &ObjectPath) -> DbResult<TableDetails> {
        let p = p.clone();
        self.with(move |c| {
            let (db, t) = (q(&p.database), q(&p.name));
            let mut columns = Vec::new();
            let mut st = c.prepare(&format!("PRAGMA {db}.table_info({t})"))?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                columns.push(ColumnInfo {
                    name: r.get(1)?,
                    data_type: r.get(2)?,
                    nullable: r.get::<_, i64>(3)? == 0,
                    default: r.get(4)?,
                    primary_key: r.get::<_, i64>(5)? > 0,
                    auto_increment: false,
                    comment: None,
                });
            }
            // A single INTEGER PRIMARY KEY is an alias for the rowid, assigned automatically.
            let pks: Vec<usize> = columns.iter().enumerate().filter(|(_, c)| c.primary_key).map(|(i, _)| i).collect();
            if let [i] = pks[..]
                && columns[i].data_type.eq_ignore_ascii_case("integer")
            {
                columns[i].auto_increment = true;
                columns[i].nullable = false;
            }

            let mut indexes = Vec::new();
            let mut st = c.prepare(&format!("PRAGMA {db}.index_list({t})"))?;
            let list: Vec<(String, bool, String)> = st
                .query_map([], |r| Ok((r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)))?
                .collect::<Result<_, _>>()?;
            for (name, unique, origin) in list {
                let mut st = c.prepare(&format!("PRAGMA {db}.index_info({})", q(&name)))?;
                let cols: Vec<String> = st
                    .query_map([], |r| r.get::<_, Option<String>>(2))?
                    .map(|r| r.map(|c| c.unwrap_or_else(|| "<expr>".into())))
                    .collect::<Result<_, _>>()?;
                indexes.push(IndexInfo { name, columns: cols, unique, primary: origin == "pk", method: None });
            }

            let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
            let mut st = c.prepare(&format!("PRAGMA {db}.foreign_key_list({t})"))?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let name = format!("fk_{}_{id}", p.name);
                let from: String = r.get(3)?;
                let to: Option<String> = r.get(4)?;
                match foreign_keys.last_mut() {
                    Some(f) if f.name == name => {
                        f.columns.push(from);
                        f.ref_columns.push(to.unwrap_or_default());
                    }
                    _ => foreign_keys.push(ForeignKeyInfo {
                        name,
                        columns: vec![from],
                        ref_schema: p.database.clone(),
                        ref_table: r.get(2)?,
                        ref_columns: vec![to.unwrap_or_default()],
                        on_update: r.get(5)?,
                        on_delete: r.get(6)?,
                    }),
                }
            }
            Ok(TableDetails { columns, indexes, foreign_keys, comment: None })
        })
        .await
    }

    async fn ddl(&self, o: &DbObject) -> DbResult<String> {
        let p = o.path.clone();
        let is_table = o.kind == ObjectKind::Table;
        self.with(move |c| {
            let master = format!("{}.sqlite_master", q(&p.database));
            let mut out: String =
                c.query_row(&format!("SELECT sql FROM {master} WHERE name = ?1"), [&p.name], |r| r.get(0))?;
            out.push_str(";\n");
            if is_table {
                let mut st = c.prepare(&format!(
                    "SELECT sql FROM {master} WHERE type IN ('index', 'trigger') AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY type, name"
                ))?;
                for s in st.query_map([&p.name], |r| r.get::<_, String>(0))? {
                    out.push_str(&s?);
                    out.push_str(";\n");
                }
            }
            Ok(out)
        })
        .await
    }

    async fn execute(&self, _database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let sql = sql.to_string();
        self.with(move |c| run(c, &sql, max_rows)).await
    }

    async fn cancel(&self) -> DbResult<()> {
        self.interrupt.interrupt();
        Ok(())
    }
}
