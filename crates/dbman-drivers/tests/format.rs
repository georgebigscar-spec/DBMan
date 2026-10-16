//! Formatted SQL means the same: every query runs as written and formatted on
//! each engine, and the results must match. SQLite and DuckDB always run; the
//! servers need `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL`
//! and `DBMAN_TEST_CLICKHOUSE`, and are skipped with a note otherwise.

use std::sync::Arc;

use dbman_core::format::format_sql;
use dbman_core::{Connection, ConnectionConfig, DbKind};

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

async fn outcome(c: &Arc<dyn Connection>, db: &str, sql: &str) -> String {
    let out = c.execute(Some(db), sql, 100).await.unwrap_or_else(|e| panic!("{sql}\n{e}"));
    match out {
        dbman_core::StatementOutcome::Rows(r) => format!("{:?}", r.rows),
        other => format!("{other:?}"),
    }
}

const QUERIES: &[&str] = &[
    "select a.id, b.v as \"bv\", case when a.n > 1 then 'big' else 'small' end k from fmt_t a join fmt_t b on b.id = a.id and b.n >= 0 where a.n between 0 and 5 or a.v is null order by a.id",
    "select v, count(*) c, sum(n) from fmt_t where id in (select id from fmt_t where n > 0) group by v having count(*) >= 1 order by v",
    "with x as (select id, n from fmt_t where n<>2), y as (select id from x) select x.id, -x.n, x.n*2+1 from x join y on y.id=x.id order by x.id",
    "select id from fmt_t where exists (select 1 from fmt_t t2 where t2.id = fmt_t.id) union all select id from fmt_t order by 1",
];

async fn check(cfg: ConnectionConfig, db: &str, create: &str, extra: &[&str]) {
    let kind = cfg.kind;
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    let _ = c.execute(Some(db), "DROP TABLE fmt_t", 0).await;
    outcome(&c, db, &format_sql(create, kind)).await;
    outcome(&c, db, &format_sql("insert into fmt_t (id, v, n) values (1, 'a', 1), (2, 'b', 2), (3, null, 3)", kind))
        .await;
    for sql in QUERIES.iter().chain(extra) {
        let formatted = format_sql(sql, kind);
        assert_ne!(&formatted, sql);
        assert_eq!(outcome(&c, db, &formatted).await, outcome(&c, db, sql).await, "{kind:?}\n{formatted}");
    }
    outcome(&c, db, "DROP TABLE fmt_t").await;
}

const CREATE: &str = "create table fmt_t (id integer primary key, v varchar(10), n integer not null default 0)";

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("x.db").to_string_lossy().into_owned();
    check(cfg, "main", CREATE, &["select id from fmt_t order by id limit 2 offset 1"]).await;
}

#[tokio::test]
async fn duckdb() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.database = dir.path().join("x.duckdb").to_string_lossy().into_owned();
    let extra = [
        "select id::varchar, row_number() over (partition by v is null order by id) from fmt_t qualify id > 1 order by 1",
    ];
    check(cfg, "x", CREATE, &extra).await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let extra = [
        "select id::text, e'a\\'b', 1e-2, json_build_object('k', v)->>'k' from fmt_t order by id limit 2",
        "select id, v from fmt_t where v is distinct from 'a' order by id",
    ];
    check(cfg, &db, CREATE, &extra).await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let extra = ["select `id`, \"s\", concat(v, 'x') from fmt_t # note\n where v like 'a%' order by id limit 1"];
    check(cfg, &db, CREATE, &extra).await;
}

#[tokio::test]
async fn mssql() {
    let Some(cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let extra = ["select top 2 [id], n'x' from dbo.fmt_t with (nolock) order by id"];
    check(cfg, &db, CREATE, &extra).await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let create = "create table fmt_t (id UInt32, v Nullable(String), n Int32 default 0) engine = MergeTree order by id";
    let extra =
        ["select id, count() from fmt_t prewhere n > 0 where id < 3 group by id order by id settings max_threads = 1"];
    check(cfg, &db, create, &extra).await;
}
