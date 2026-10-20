//! Users, roles and privileges on every server: the rights check, then a role
//! and a user are created, the user joins the role, gets and loses a
//! privilege, is changed and dropped, and each step shows in the listings.
//! Needs `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL` and
//! `DBMAN_TEST_CLICKHOUSE`; each server is skipped with a note otherwise.

use std::sync::Arc;

use dbman_core::admin::{self, Edit, Grant, Membership, Principal, Scope, Target};
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

async fn rows(c: &Arc<dyn Connection>, sql: &str) -> dbman_core::QueryResult {
    match c.execute(None, sql, 10_000).await.unwrap_or_else(|e| panic!("{sql}: {e}")) {
        StatementOutcome::Rows(r) => r,
        other => panic!("{sql}: {other:?}"),
    }
}

/// The first of several alternative statements that works.
async fn first_working(c: &Arc<dyn Connection>, list: &[&str]) -> dbman_core::QueryResult {
    let mut errors = Vec::new();
    for sql in list {
        match c.execute(None, sql, 10_000).await {
            Ok(StatementOutcome::Rows(r)) => return r,
            Ok(other) => errors.push(format!("{sql}: {other:?}")),
            Err(e) => errors.push(format!("{sql}: {e}")),
        }
    }
    panic!("none of these work: {errors:#?}");
}

async fn principals(c: &Arc<dyn Connection>) -> Vec<Principal> {
    admin::parse_principals(&first_working(c, admin::principals_sql(c.kind())).await)
}

async fn memberships(c: &Arc<dyn Connection>) -> Vec<Membership> {
    admin::parse_memberships(&first_working(c, admin::memberships_sql(c.kind())).await)
}

async fn grants(c: &Arc<dyn Connection>, p: &Principal) -> Vec<Grant> {
    admin::parse_grants(&rows(c, &admin::grants_sql(c.kind(), p).unwrap()).await)
}

async fn run(c: &Arc<dyn Connection>, statements: Vec<String>) {
    for sql in statements {
        c.execute(None, &sql, 0).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

async fn find(c: &Arc<dyn Connection>, name: &str) -> Option<Principal> {
    principals(c).await.into_iter().find(|p| p.name == name)
}

/// `table` is the target of the grant: (database or schema, table), and it is created first.
async fn check(cfg: ConnectionConfig, password: &str, table: (&str, &str), create_table: &str) {
    let kind = cfg.kind;
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    assert!(admin::parse_check(&rows(&c, admin::check_sql(kind).unwrap()).await), "{kind:?}: no admin rights");
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let (role_name, user_name) = (format!("dbman_readers_{tag}"), format!("dbman_app_{tag}"));
    run(&c, vec![create_table.to_string()]).await;

    let mut e = Edit::new(kind, true);
    e.name = role_name.clone();
    run(&c, admin::create(kind, &e)).await;
    let mut e = Edit::new(kind, false);
    e.name = user_name.clone();
    e.password = password.into();
    run(&c, admin::create(kind, &e)).await;

    let role = find(&c, &role_name).await.unwrap_or_else(|| panic!("{kind:?}: the role is listed"));
    let user = find(&c, &user_name).await.unwrap_or_else(|| panic!("{kind:?}: the user is listed"));
    assert!(role.role, "{kind:?}: {role:?}");
    assert!(!user.role, "{kind:?}: {user:?}");
    if kind == DbKind::Postgres {
        assert!(user.has("LOGIN") && !role.has("LOGIN"));
    }

    run(&c, vec![admin::grant_role(kind, &role, &user, false)]).await;
    let joined = memberships(&c).await;
    assert!(joined.iter().any(|m| m.role == role.id() && m.member == user.id()), "{kind:?}: {joined:#?}");

    let scope = Scope::Table;
    let (database, schema) = match scope.fields(kind) {
        (true, _, _) => (table.0.to_string(), String::new()),
        _ => (String::new(), table.0.to_string()),
    };
    let target = admin::target_sql(kind, &Target { scope, database, schema, name: table.1.into() });
    run(&c, vec![admin::grant(kind, &["SELECT", "INSERT"], &target, &role, false)]).await;
    let held = grants(&c, &role).await;
    let select = held
        .iter()
        .find(|g| g.privilege == "SELECT")
        .unwrap_or_else(|| panic!("{kind:?}: the grant is listed: {held:#?}"));
    assert!(select.object.ends_with(table.1), "{kind:?}: {select:?}");
    // The listed target revokes it.
    run(&c, vec![admin::revoke(kind, &["SELECT"], &select.target, &role, select.grantable)]).await;
    let held = grants(&c, &role).await;
    assert!(!held.iter().any(|g| g.privilege == "SELECT"), "{kind:?}: {held:#?}");
    assert!(held.iter().any(|g| g.privilege == "INSERT"), "{kind:?}: {held:#?}");

    // Options, password and name in one edit.
    let mut e = Edit::of(&user);
    e.password = format!("{password}2");
    e.name = format!("{user_name}_x");
    if let Some(o) = admin::options(kind).first() {
        e.options.push(o.key.into());
    }
    if kind == DbKind::Postgres {
        e.options.push("CREATEDB".into());
        e.conn_limit = "3".into();
        e.valid_until = "2099-01-01".into();
    }
    run(&c, admin::alter(kind, &user, &e)).await;
    let changed = find(&c, &e.name).await.unwrap_or_else(|| panic!("{kind:?}: renamed"));
    if kind == DbKind::Postgres {
        assert!(changed.has("CREATEDB"), "{changed:?}");
        assert_eq!(changed.conn_limit, "3");
        assert!(changed.valid_until.starts_with("2099-01-01"), "{changed:?}");
    }
    if kind == DbKind::MsSql {
        assert!(changed.has("DISABLED"), "{changed:?}");
    }

    run(&c, vec![admin::revoke_role(kind, &role, &changed)]).await;
    assert!(!memberships(&c).await.iter().any(|m| m.role == role.id() && m.member == changed.id()));
    run(&c, vec![admin::revoke(kind, &["INSERT"], &target, &role, false)]).await;
    run(&c, vec![admin::drop(kind, &changed)]).await;
    run(&c, admin::drop_owned(kind, &role)).await;
    if kind == DbKind::MsSql {
        // The login stays with the dropped user.
        run(&c, vec![format!("DROP LOGIN [{user_name}]")]).await;
    }
    assert!(find(&c, &e.name).await.is_none() && find(&c, &role_name).await.is_none());
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    check(cfg, "secret", ("public", "dbman_admin_t"), "CREATE TABLE IF NOT EXISTS public.dbman_admin_t (id int)").await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let create = format!("CREATE TABLE IF NOT EXISTS `{db}`.dbman_admin_t (id int)");
    check(cfg, "secret", (&db, "dbman_admin_t"), &create).await;
}

#[tokio::test]
async fn mssql() {
    let Some(mut cfg) = from_url(DbKind::MsSql, "DBMAN_TEST_MSSQL") else {
        eprintln!("DBMAN_TEST_MSSQL not set, skipping");
        return;
    };
    // Users belong to a database: make sure it exists and work in it.
    let db = cfg.database.clone();
    let mut master = cfg.clone();
    master.database = "master".into();
    let m = dbman_drivers::connect(&master).await.unwrap();
    run(&m, vec![format!("IF DB_ID(N'{db}') IS NULL CREATE DATABASE [{db}]")]).await;
    cfg.database = db;
    check(
        cfg,
        "DbMan_Secret1",
        ("dbo", "dbman_admin_t"),
        "IF OBJECT_ID(N'dbo.dbman_admin_t') IS NULL CREATE TABLE dbo.dbman_admin_t (id int)",
    )
    .await;
}

#[tokio::test]
async fn clickhouse() {
    let Some(cfg) = from_url(DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE") else {
        eprintln!("DBMAN_TEST_CLICKHOUSE not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let create = format!("CREATE TABLE IF NOT EXISTS `{db}`.dbman_admin_t (id Int32) ENGINE = Memory");
    check(cfg, "secret", (&db, "dbman_admin_t"), &create).await;
}
