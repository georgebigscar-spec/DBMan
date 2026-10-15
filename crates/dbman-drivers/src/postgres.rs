//! PostgreSQL driver on `tokio-postgres`.
//!
//! PostgreSQL connections are bound to one database, so a session keeps one
//! client per database it has touched and opens them on demand.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbman_core::sql::Dialect;
use dbman_core::{
    Cell, ColumnInfo, ColumnMeta, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ForeignKeyInfo,
    IndexInfo, ObjectKind, ObjectPath, QueryResult, SslMode, StatementOutcome, TableDetails,
};
use futures_util::{TryStreamExt, pin_mut};
use tokio::sync::Mutex;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};

pub struct PgConnection {
    cfg: ConnectionConfig,
    default_db: String,
    clients: Mutex<HashMap<String, Arc<Client>>>,
    /// Cancel tokens of the clients, reachable while a query holds a client.
    cancel_tokens: std::sync::Mutex<HashMap<String, CancelToken>>,
}

impl PgConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> DbResult<Self> {
        let default_db = if cfg.database.trim().is_empty() { "postgres".to_string() } else { cfg.database.clone() };
        let me = Self {
            cfg: cfg.clone(),
            default_db,
            clients: Mutex::new(HashMap::new()),
            cancel_tokens: Default::default(),
        };
        me.client(None).await?;
        Ok(me)
    }

    async fn client(&self, database: Option<&str>) -> DbResult<Arc<Client>> {
        let db = database.filter(|d| !d.is_empty()).unwrap_or(&self.default_db).to_string();
        let mut clients = self.clients.lock().await;
        if let Some(c) = clients.get(&db)
            && !c.is_closed()
        {
            return Ok(c.clone());
        }
        let client = Arc::new(open_client(&self.cfg, &db).await?);
        if let Ok(mut t) = self.cancel_tokens.lock() {
            t.insert(db.clone(), client.cancel_token());
        }
        clients.insert(db, client.clone());
        Ok(client)
    }

    async fn rows(
        &self,
        db: &str,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> DbResult<Vec<tokio_postgres::Row>> {
        self.client(Some(db)).await?.query(sql, params).await.map_err(pg_err)
    }
}

async fn open_client(cfg: &ConnectionConfig, db: &str) -> DbResult<Client> {
    let mut pg = tokio_postgres::Config::new();
    pg.host(&cfg.host)
        .port(cfg.port)
        .user(&cfg.user)
        .dbname(db)
        .application_name("DBMan")
        .connect_timeout(Duration::from_secs(10));
    if !cfg.password.is_empty() {
        pg.password(&cfg.password);
    }
    if cfg.ssl_mode == SslMode::Disable {
        let (client, conn) = pg.connect(tokio_postgres::NoTls).await.map_err(pg_err)?;
        tokio::spawn(conn);
        return Ok(client);
    }
    pg.ssl_mode(match cfg.ssl_mode {
        SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
        _ => tokio_postgres::config::SslMode::Require,
    });
    let (client, conn) = pg.connect(make_tls(cfg)?).await.map_err(pg_err)?;
    tokio::spawn(conn);
    Ok(client)
}

fn make_tls(cfg: &ConnectionConfig) -> DbResult<postgres_native_tls::MakeTlsConnector> {
    let verify = cfg.ssl_mode == SslMode::VerifyFull;
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(!verify)
        .danger_accept_invalid_hostnames(!verify)
        .build()
        .map_err(DbError::from_display)?;
    Ok(postgres_native_tls::MakeTlsConnector::new(tls))
}

/// The server's message, with detail and hint when present.
fn pg_err(e: tokio_postgres::Error) -> DbError {
    if let Some(db) = e.as_db_error() {
        let mut msg = format!("{}: {}", db.severity(), db.message());
        if let Some(d) = db.detail() {
            msg.push_str(&format!("\nDetail: {d}"));
        }
        if let Some(h) = db.hint() {
            msg.push_str(&format!("\nHint: {h}"));
        }
        if let Some(tokio_postgres::error::ErrorPosition::Original(p)) = db.position() {
            msg.push_str(&format!("\nPosition: {p}"));
        }
        return DbError::new(msg);
    }
    // Walk the source chain: the top-level message alone is often just "error connecting to server".
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(&format!(": {s}"));
        src = s.source();
    }
    DbError::new(msg)
}

fn fk_action(code: &str) -> String {
    match code {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION",
    }
    .to_string()
}

#[async_trait]
impl Connection for PgConnection {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    async fn server_version(&self) -> DbResult<String> {
        let rows = self.rows(&self.default_db, "SELECT version()", &[]).await?;
        Ok(rows.first().map(|r| r.get::<_, String>(0)).unwrap_or_default())
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        if self.cfg.only_default_database {
            return Ok(vec![self.default_db.clone()]);
        }
        let rows = self
            .rows(
                &self.default_db,
                "SELECT datname::text FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        let rows = self
            .rows(
                database,
                "SELECT nspname::text FROM pg_namespace
                 WHERE nspname NOT IN ('information_schema', 'pg_catalog', 'pg_toast')
                   AND nspname NOT LIKE 'pg_temp_%' AND nspname NOT LIKE 'pg_toast_temp_%'
                 ORDER BY nspname <> 'public', nspname",
                &[],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>> {
        let mut out = Vec::new();
        let path = |name: String| ObjectPath::new(database, schema, name);

        let rels = self
            .rows(
                database,
                "SELECT c.relname::text, c.relkind::text, obj_description(c.oid, 'pg_class')
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S')
                 ORDER BY 1",
                &[&schema],
            )
            .await?;
        for r in rels {
            let kind = match r.get::<_, String>(1).as_str() {
                "v" => ObjectKind::View,
                "m" => ObjectKind::MaterializedView,
                "f" => ObjectKind::ForeignTable,
                "S" => ObjectKind::Sequence,
                _ => ObjectKind::Table,
            };
            out.push(DbObject { kind, path: path(r.get(0)), detail: None, comment: r.get(2) });
        }

        let procs = self
            .rows(
                database,
                "SELECT p.proname::text, pg_get_function_identity_arguments(p.oid), p.prokind::text,
                        obj_description(p.oid, 'pg_proc')
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                 WHERE n.nspname = $1 AND p.prokind IN ('f', 'p')
                   AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e')
                 ORDER BY 1, 2",
                &[&schema],
            )
            .await?;
        for r in procs {
            let kind = if r.get::<_, String>(2) == "p" { ObjectKind::Procedure } else { ObjectKind::Function };
            out.push(DbObject { kind, path: path(r.get(0)), detail: Some(r.get(1)), comment: r.get(3) });
        }

        let triggers = self
            .rows(
                database,
                "SELECT t.tgname::text, c.relname::text
                 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND NOT t.tgisinternal ORDER BY 1",
                &[&schema],
            )
            .await?;
        for r in triggers {
            out.push(DbObject {
                kind: ObjectKind::Trigger,
                path: path(r.get(0)),
                detail: Some(r.get(1)),
                comment: None,
            });
        }

        let types = self
            .rows(
                database,
                "SELECT t.typname::text, t.typtype::text, obj_description(t.oid, 'pg_type')
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                 WHERE n.nspname = $1 AND (t.typtype IN ('e', 'd')
                       OR (t.typtype = 'c' AND (SELECT relkind FROM pg_class WHERE oid = t.typrelid) = 'c'))
                 ORDER BY 1",
                &[&schema],
            )
            .await?;
        for r in types {
            let detail = match r.get::<_, String>(1).as_str() {
                "e" => "enum",
                "d" => "domain",
                _ => "composite",
            };
            out.push(DbObject {
                kind: ObjectKind::Type,
                path: path(r.get(0)),
                detail: Some(detail.into()),
                comment: r.get(2),
            });
        }
        Ok(out)
    }

    async fn table_details(&self, p: &ObjectPath) -> DbResult<TableDetails> {
        let db = p.database.as_str();
        let args: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&p.schema, &p.name];
        let cols = self
            .rows(
                db,
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                        pg_get_expr(d.adbin, d.adrelid), col_description(c.oid, a.attnum),
                        a.attidentity <> '' OR coalesce(pg_get_expr(d.adbin, d.adrelid), '') LIKE 'nextval(%',
                        EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary
                                AND a.attnum = ANY (i.indkey))
                 FROM pg_attribute a
                 JOIN pg_class c ON c.oid = a.attrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                &args,
            )
            .await?;
        let columns = cols
            .iter()
            .map(|r| ColumnInfo {
                name: r.get(0),
                data_type: r.get(1),
                nullable: r.get(2),
                default: r.get(3),
                comment: r.get(4),
                auto_increment: r.get(5),
                primary_key: r.get(6),
            })
            .collect();

        let idx = self
            .rows(
                db,
                "SELECT ic.relname::text, i.indisunique, i.indisprimary, am.amname::text,
                        ARRAY(SELECT pg_get_indexdef(i.indexrelid, k, true) FROM generate_series(1, i.indnatts) k)
                 FROM pg_index i
                 JOIN pg_class ic ON ic.oid = i.indexrelid
                 JOIN pg_class c ON c.oid = i.indrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_am am ON am.oid = ic.relam
                 WHERE n.nspname = $1 AND c.relname = $2
                 ORDER BY 1",
                &args,
            )
            .await?;
        let indexes = idx
            .iter()
            .map(|r| IndexInfo {
                name: r.get(0),
                unique: r.get(1),
                primary: r.get(2),
                method: Some(r.get(3)),
                columns: r.get(4),
            })
            .collect();

        let fks = self
            .rows(
                db,
                "SELECT con.conname::text,
                        ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(n, o)
                              JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.n ORDER BY k.o),
                        fn.nspname::text, fc.relname::text,
                        ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(n, o)
                              JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.n ORDER BY k.o),
                        con.confupdtype::text, con.confdeltype::text
                 FROM pg_constraint con
                 JOIN pg_class c ON c.oid = con.conrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_class fc ON fc.oid = con.confrelid
                 JOIN pg_namespace fn ON fn.oid = fc.relnamespace
                 WHERE con.contype = 'f' AND n.nspname = $1 AND c.relname = $2
                 ORDER BY 1",
                &args,
            )
            .await?;
        let foreign_keys = fks
            .iter()
            .map(|r| ForeignKeyInfo {
                name: r.get(0),
                columns: r.get(1),
                ref_schema: r.get(2),
                ref_table: r.get(3),
                ref_columns: r.get(4),
                on_update: fk_action(&r.get::<_, String>(5)),
                on_delete: fk_action(&r.get::<_, String>(6)),
            })
            .collect();

        let comment = self
            .rows(
                db,
                "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2",
                &args,
            )
            .await?
            .first()
            .and_then(|r| r.get(0));
        Ok(TableDetails { columns, indexes, foreign_keys, comment })
    }

    async fn ddl(&self, o: &DbObject) -> DbResult<String> {
        let p = &o.path;
        let db = p.database.as_str();
        let d = Dialect(DbKind::Postgres);
        let q = d.qualified(p);
        let args: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&p.schema, &p.name];
        let one = |rows: Vec<tokio_postgres::Row>| -> DbResult<String> {
            rows.first().map(|r| r.get::<_, String>(0)).ok_or_else(|| DbError::new(format!("{} not found", q)))
        };
        match o.kind {
            ObjectKind::Table | ObjectKind::ForeignTable => {
                let details = self.table_details(p).await?;
                let mut lines: Vec<String> = details
                    .columns
                    .iter()
                    .map(|c| {
                        let mut l = format!("    {} {}", d.ident(&c.name), c.data_type);
                        if let Some(def) = &c.default {
                            l.push_str(&format!(" DEFAULT {def}"));
                        }
                        if !c.nullable {
                            l.push_str(" NOT NULL");
                        }
                        l
                    })
                    .collect();
                let cons = self
                    .rows(
                        db,
                        "SELECT con.conname::text, pg_get_constraintdef(con.oid, true)
                         FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid
                         JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = $1 AND c.relname = $2
                         ORDER BY CASE con.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, 1",
                        &args,
                    )
                    .await?;
                for r in cons {
                    lines.push(format!("    CONSTRAINT {} {}", d.ident(&r.get::<_, String>(0)), r.get::<_, String>(1)));
                }
                let mut out = format!("CREATE TABLE {q} (\n{}\n);\n", lines.join(",\n"));
                let idx = self
                    .rows(
                        db,
                        "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i
                         JOIN pg_class c ON c.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = $1 AND c.relname = $2
                           AND NOT EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conindid = i.indexrelid)
                         ORDER BY 1",
                        &args,
                    )
                    .await?;
                for r in idx {
                    out.push_str(&format!("{};\n", r.get::<_, String>(0)));
                }
                if let Some(c) = &details.comment {
                    out.push_str(&format!("COMMENT ON TABLE {q} IS {};\n", d.string(c)));
                }
                for c in &details.columns {
                    if let Some(cm) = &c.comment {
                        out.push_str(&format!("COMMENT ON COLUMN {q}.{} IS {};\n", d.ident(&c.name), d.string(cm)));
                    }
                }
                Ok(out)
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                let def = one(self
                    .rows(
                        db,
                        "SELECT pg_get_viewdef(c.oid, true) FROM pg_class c
                         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2",
                        &args,
                    )
                    .await?)?;
                let head =
                    if o.kind == ObjectKind::View { "CREATE OR REPLACE VIEW" } else { "CREATE MATERIALIZED VIEW" };
                Ok(format!("{head} {q} AS\n{}", def.trim_end()))
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let args_text = o.detail.clone().unwrap_or_default();
                let rows = self
                    .rows(
                        db,
                        "SELECT pg_get_functiondef(p.oid) FROM pg_proc p
                         JOIN pg_namespace n ON n.oid = p.pronamespace
                         WHERE n.nspname = $1 AND p.proname = $2 AND pg_get_function_identity_arguments(p.oid) = $3",
                        &[&p.schema, &p.name, &args_text],
                    )
                    .await?;
                one(rows)
            }
            ObjectKind::Trigger => {
                let rows = self
                    .rows(
                        db,
                        "SELECT pg_get_triggerdef(t.oid, true) FROM pg_trigger t
                         JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = $1 AND t.tgname = $2",
                        &args,
                    )
                    .await?;
                one(rows).map(|s| s + ";")
            }
            ObjectKind::Sequence => {
                let rows = self
                    .rows(
                        db,
                        "SELECT format('CREATE SEQUENCE %s\n    AS %s\n    INCREMENT BY %s\n    MINVALUE %s\n    MAXVALUE %s\n    START WITH %s\n    CACHE %s%s;',
                                       $3::text, data_type, increment_by, min_value, max_value, start_value, cache_size,
                                       CASE WHEN cycle THEN '\n    CYCLE' ELSE '' END)
                         FROM pg_sequences WHERE schemaname = $1 AND sequencename = $2",
                        &[&p.schema, &p.name, &q],
                    )
                    .await?;
                one(rows)
            }
            ObjectKind::Type => {
                let rows = self
                    .rows(
                        db,
                        "SELECT CASE t.typtype
                           WHEN 'e' THEN format('CREATE TYPE %s AS ENUM (%s);', $3::text,
                               (SELECT string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder)
                                FROM pg_enum e WHERE e.enumtypid = t.oid))
                           WHEN 'd' THEN format('CREATE DOMAIN %s AS %s%s%s;', $3::text,
                               format_type(t.typbasetype, t.typtypmod),
                               CASE WHEN t.typnotnull THEN ' NOT NULL' ELSE '' END,
                               coalesce((SELECT string_agg(' CONSTRAINT ' || quote_ident(c.conname) || ' ' || pg_get_constraintdef(c.oid), '')
                                         FROM pg_constraint c WHERE c.contypid = t.oid), ''))
                           ELSE format('CREATE TYPE %s AS (%s);', $3::text,
                               (SELECT string_agg(quote_ident(a.attname) || ' ' || format_type(a.atttypid, a.atttypmod), ', ' ORDER BY a.attnum)
                                FROM pg_attribute a WHERE a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped))
                         END
                         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                         WHERE n.nspname = $1 AND t.typname = $2",
                        &[&p.schema, &p.name, &q],
                    )
                    .await?;
                one(rows)
            }
        }
    }

    async fn cancel(&self) -> DbResult<()> {
        // A session runs one statement at a time, so cancelling every client stops it.
        let tokens: Vec<CancelToken> =
            self.cancel_tokens.lock().map(|t| t.values().cloned().collect()).unwrap_or_default();
        for token in tokens {
            if self.cfg.ssl_mode == SslMode::Disable {
                token.cancel_query(tokio_postgres::NoTls).await.map_err(pg_err)?;
            } else {
                token.cancel_query(make_tls(&self.cfg)?).await.map_err(pg_err)?;
            }
        }
        Ok(())
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        let client = self.client(database).await?;
        // The simple protocol returns every value as text, which is what the grid shows;
        // preparing the statement first gives the column types (for alignment and sorting).
        let types: Option<Vec<String>> = match client.prepare(sql).await {
            Ok(stmt) => Some(stmt.columns().iter().map(|c| c.type_().name().to_string()).collect()),
            Err(_) => None,
        };
        let stream = client.simple_query_raw(sql).await.map_err(pg_err)?;
        pin_mut!(stream);
        let mut result: Option<QueryResult> = None;
        let mut affected = None;
        while let Some(msg) = stream.try_next().await.map_err(pg_err)? {
            match msg {
                SimpleQueryMessage::RowDescription(cols) => {
                    let columns = cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| {
                            let t = types.as_ref().and_then(|t| t.get(i)).cloned().unwrap_or_default();
                            ColumnMeta::new(c.name(), t)
                        })
                        .collect();
                    result = Some(QueryResult { columns, rows: Vec::new(), truncated: false });
                }
                SimpleQueryMessage::Row(row) => {
                    let r = result.get_or_insert_with(QueryResult::default);
                    if r.rows.len() >= max_rows {
                        // Dropping the stream discards the remaining rows.
                        r.truncated = true;
                        break;
                    }
                    r.rows.push(
                        (0..row.len()).map(|i| row.get(i).map_or(Cell::Null, |s| Cell::Text(s.to_string()))).collect(),
                    );
                }
                SimpleQueryMessage::CommandComplete(n) => affected = Some(n),
                _ => {}
            }
        }
        Ok(match result {
            Some(r) => StatementOutcome::Rows(r),
            None => StatementOutcome::Done { affected },
        })
    }
}
