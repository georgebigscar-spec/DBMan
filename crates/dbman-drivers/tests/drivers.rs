//! Driver tests against real databases.
//!
//! SQLite always runs. PostgreSQL and MySQL run when their URL is set, e.g.
//! `DBMAN_TEST_POSTGRES=postgres://postgres:secret@127.0.0.1:5432/postgres` and
//! `DBMAN_TEST_MYSQL=mysql://root:secret@127.0.0.1:3306/dbman_test`.

use std::sync::Arc;

use dbman_core::ddl::{self, ColumnDef, TableDef};
use dbman_core::{
    Cell, ColumnInfo, Connection, ConnectionConfig, DbKind, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectPath,
    StatementOutcome, dml,
};

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

async fn exec(c: &Arc<dyn Connection>, db: &str, sql: &str) -> StatementOutcome {
    c.execute(Some(db), sql, 1000).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn rows(c: &Arc<dyn Connection>, db: &str, sql: &str) -> Vec<Vec<Cell>> {
    match exec(c, db, sql).await {
        StatementOutcome::Rows(r) => r.rows,
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

/// Runs the same checks on every engine: `setup` creates `items` (id pk, name,
/// parent fk -> items), index `items_name_idx` and view `item_names`.
async fn check_engine(c: Arc<dyn Connection>, db: &str, schema: &str, setup: &[&str]) {
    for s in setup {
        exec(&c, db, s).await;
    }
    assert!(!c.server_version().await.unwrap().is_empty());
    assert!(c.databases().await.unwrap().iter().any(|d| d == db));
    assert!(c.schemas(db).await.unwrap().iter().any(|s| s == schema));

    let objects = c.objects(db, schema).await.unwrap();
    let find = |name: &str| objects.iter().find(|o| o.path.name == name).unwrap_or_else(|| panic!("{name} not listed"));
    assert_eq!(find("items").kind, ObjectKind::Table);
    assert_eq!(find("item_names").kind, ObjectKind::View);

    let items = find("items").clone();
    let d = c.table_details(&items.path).await.unwrap();
    let names: Vec<_> = d.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "name", "parent"]);
    assert_eq!(d.primary_key(), ["id"]);
    assert!(!d.columns[1].nullable);
    assert!(d.indexes.iter().any(|i| i.name == "items_name_idx" && i.columns == ["name"]));
    assert_eq!(d.foreign_keys.len(), 1);
    assert_eq!(d.foreign_keys[0].ref_table, "items");
    assert_eq!(d.foreign_keys[0].ref_columns, ["id"]);

    let ddl = c.ddl(&items).await.unwrap();
    assert!(ddl.to_ascii_uppercase().contains("CREATE TABLE"), "{ddl}");
    let view_ddl = c.ddl(find("item_names")).await.unwrap();
    assert!(view_ddl.to_ascii_uppercase().contains("VIEW"), "{view_ddl}");

    let q = dbman_core::sql::Dialect(c.kind()).qualified(&items.path);
    match exec(&c, db, &format!("INSERT INTO {q} (id, name, parent) VALUES (1, 'root', NULL), (2, 'child', 1)")).await {
        StatementOutcome::Done { affected } => assert_eq!(affected, Some(2)),
        other => panic!("{other:?}"),
    }
    let r = rows(&c, db, &format!("SELECT id, name, parent FROM {q} ORDER BY id")).await;
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Cell::Text("1".into()));
    assert_eq!(r[1][1], Cell::Text("child".into()));
    assert_eq!(r[0][2], Cell::Null);

    match exec(&c, db, &format!("SELECT id FROM {q}")).await {
        StatementOutcome::Rows(r) => {
            assert!(r.columns[0].numeric, "{:?}", r.columns[0]);
        }
        other => panic!("{other:?}"),
    }
    match c.execute(Some(db), &format!("SELECT * FROM {q}"), 1).await.unwrap() {
        StatementOutcome::Rows(r) => assert!(r.truncated && r.rows.len() == 1),
        other => panic!("{other:?}"),
    }
    // Statements built for grid edits round-trip tricky values.
    let d = dbman_core::sql::Dialect(c.kind());
    let tricky = Cell::Text("O'Brien \\ \"q\" ;".into());
    let two = Cell::Text("2".into());
    let set = [dml::Value::new("name", &tricky, false)];
    let key = [dml::Value::new("id", &two, true)];
    match exec(&c, db, &dml::update(d, &items.path, &set, &key)).await {
        StatementOutcome::Done { affected } => assert_eq!(affected, Some(1)),
        other => panic!("{other:?}"),
    }
    let r = rows(&c, db, &format!("SELECT name FROM {q} WHERE id = 2")).await;
    assert_eq!(r[0][0], tricky);
    // An UPDATE that keeps the value still reports the matched row.
    match exec(&c, db, &dml::update(d, &items.path, &set, &key)).await {
        StatementOutcome::Done { affected } => assert_eq!(affected, Some(1)),
        other => panic!("{other:?}"),
    }
    // Rolled back deletes leave the row.
    exec(&c, db, dml::begin(c.kind())).await;
    exec(&c, db, &dml::delete(d, &items.path, &key)).await;
    exec(&c, db, "ROLLBACK").await;
    assert_eq!(rows(&c, db, &format!("SELECT id FROM {q} WHERE id = 2")).await.len(), 1);

    assert!(c.execute(Some(db), "SELECT * FROM no_such_table_xyz", 10).await.is_err());
    // The session still works after an error.
    assert_eq!(rows(&c, db, "SELECT 1").await.len(), 1);
}

async fn run_plan(c: &Arc<dyn Connection>, db: &str, plan: &ddl::Plan) {
    for s in &plan.statements {
        exec(c, db, s).await;
    }
}

/// Creates tables with the DDL builders, reads them back, alters them and
/// checks the result; also that an unchanged table yields no statements.
async fn check_ddl(c: Arc<dyn Connection>, db: &str, schema: &str) {
    let kind = c.kind();
    let path = |n: &str| ObjectPath::new(db, schema, n);
    let int = if kind == DbKind::Sqlite { "INTEGER" } else { "integer" };
    let col = |name: &str, ty: &str| ColumnDef {
        col: ColumnInfo { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() },
        original: None,
    };
    for t in ["ddl_members", "ddl_teams"] {
        let _ = c
            .execute(
                Some(db),
                &format!("DROP TABLE IF EXISTS {}", dbman_core::sql::Dialect(kind).qualified(&path(t))),
                0,
            )
            .await;
    }

    let mut teams = TableDef::new(path("ddl_teams"));
    let mut id = col("id", int);
    id.col.primary_key = true;
    id.col.nullable = false;
    id.col.auto_increment = true;
    teams.columns = vec![id.clone(), col("title", "varchar(40)")];
    run_plan(&c, db, &ddl::create_table(kind, &teams)).await;

    let mut members = TableDef::new(path("ddl_members"));
    let mut email = col("email", "varchar(100)");
    email.col.nullable = false;
    email.col.comment = Some("login".into());
    let mut team = col("team_id", int);
    team.col.default = Some("1".into());
    members.columns = vec![id, email, team];
    members.indexes.push(IndexInfo {
        name: "ddl_members_email".into(),
        columns: vec!["email".into()],
        unique: true,
        ..Default::default()
    });
    members.foreign_keys.push(ForeignKeyInfo {
        name: "ddl_members_team_fk".into(),
        columns: vec!["team_id".into()],
        ref_schema: schema.into(),
        ref_table: "ddl_teams".into(),
        ref_columns: vec!["id".into()],
        on_update: "NO ACTION".into(),
        on_delete: "CASCADE".into(),
    });
    run_plan(&c, db, &ddl::create_table(kind, &members)).await;
    let q = dbman_core::sql::Dialect(kind).qualified(&path("ddl_members"));
    exec(
        &c,
        db,
        &format!(
            "INSERT INTO {} (title) VALUES ('core')",
            dbman_core::sql::Dialect(kind).qualified(&path("ddl_teams"))
        ),
    )
    .await;
    exec(&c, db, &format!("INSERT INTO {q} (email) VALUES ('a@x.io')")).await;

    let details = c.table_details(&path("ddl_members")).await.unwrap();
    assert_eq!(details.primary_key(), ["id"]);
    assert!(details.columns[0].auto_increment, "{:?}", details.columns[0]);
    assert_eq!(details.columns[2].default.as_deref().map(|d| d.trim_matches('\'')), Some("1"));
    assert!(details.indexes.iter().any(|i| i.name == "ddl_members_email" && i.unique));
    assert_eq!(details.foreign_keys.len(), 1);
    assert_eq!(details.foreign_keys[0].on_delete.to_ascii_uppercase(), "CASCADE");

    let old = TableDef::from_details(path("ddl_members"), &details);
    let unchanged = ddl::alter_table(kind, &old, &old);
    assert!(unchanged.statements.is_empty(), "{:?}", unchanged.statements);

    // Rename a column, change a type, make one NOT NULL, add one, swap the index.
    let mut new = old.clone();
    new.columns[1].col.name = "mail".into();
    new.columns[1].col.data_type = "varchar(200)".into();
    new.columns[2].col.nullable = false;
    new.columns.push(col("nick", "varchar(20)"));
    new.indexes =
        vec![IndexInfo { name: "ddl_members_nick".into(), columns: vec!["nick".into()], ..Default::default() }];
    let plan = ddl::alter_table(kind, &old, &new);
    run_plan(&c, db, &plan).await;

    let after = c.table_details(&path("ddl_members")).await.unwrap();
    let names: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "mail", "team_id", "nick"], "{}", plan.script());
    assert!(after.columns[1].data_type.to_ascii_lowercase().contains("200"), "{:?}", after.columns[1]);
    assert!(!after.columns[2].nullable);
    assert!(after.indexes.iter().any(|i| i.name == "ddl_members_nick"));
    assert!(!after.indexes.iter().any(|i| i.name == "ddl_members_email"));
    assert_eq!(after.foreign_keys.len(), 1, "{}", plan.script());
    // The row survived (SQLite rebuilds the table).
    assert_eq!(rows(&c, db, &format!("SELECT mail FROM {q}")).await[0][0], Cell::Text("a@x.io".into()));
    let again = TableDef::from_details(path("ddl_members"), &after);
    assert!(ddl::alter_table(kind, &again, &again).statements.is_empty());

    let members_obj =
        dbman_core::DbObject { kind: ObjectKind::Table, path: path("ddl_members"), detail: None, comment: None };
    exec(&c, db, &ddl::rename(kind, &members_obj, "ddl_people").unwrap()).await;
    let people = dbman_core::DbObject { path: path("ddl_people"), ..members_obj };
    exec(&c, db, &ddl::truncate(kind, &people.path)).await;
    exec(&c, db, &ddl::drop_object(kind, &people, false)).await;
    let teams_obj =
        dbman_core::DbObject { kind: ObjectKind::Table, path: path("ddl_teams"), detail: None, comment: None };
    exec(&c, db, &ddl::drop_object(kind, &teams_obj, false)).await;
    let left = c.objects(db, schema).await.unwrap();
    assert!(!left.iter().any(|o| o.path.name.starts_with("ddl_")));
}

/// Starts a statement that runs for many seconds, cancels it, and checks it
/// stops quickly and that the session still works afterwards. (Whether a
/// killed statement reports an error differs: MySQL's SLEEP() just returns 1.)
async fn check_cancel(c: Arc<dyn Connection>, db: &str, slow: &str) {
    let started = std::time::Instant::now();
    let runner = {
        let (c, db, slow) = (c.clone(), db.to_string(), slow.to_string());
        tokio::spawn(async move { c.execute(Some(&db), &slow, 10).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    c.cancel().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), runner).await.expect("cancel took too long");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(rows(&c, db, "SELECT 1").await, vec![vec![Cell::Text("1".into())]]);
}

#[tokio::test]
async fn sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.database = dir.path().join("test.db").to_string_lossy().into_owned();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    check_ddl(c.clone(), "main", "main").await;
    check_cancel(
        c.clone(),
        "main",
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000000000) SELECT count(*) FROM n",
    )
    .await;
    check_engine(
        c,
        "main",
        "main",
        &[
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, parent INTEGER REFERENCES items(id))",
            "CREATE INDEX items_name_idx ON items(name)",
            "CREATE VIEW item_names AS SELECT name FROM items",
        ],
    )
    .await;
}

#[tokio::test]
async fn postgres() {
    let Some(cfg) = from_url(DbKind::Postgres, "DBMAN_TEST_POSTGRES") else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    check_engine(
        c.clone(),
        &db,
        "dbman_test",
        &[
            "DROP SCHEMA IF EXISTS dbman_test CASCADE",
            "CREATE SCHEMA dbman_test",
            "CREATE TABLE dbman_test.items (id int PRIMARY KEY, name varchar(50) NOT NULL, parent int REFERENCES dbman_test.items(id))",
            "CREATE INDEX items_name_idx ON dbman_test.items(name)",
            "CREATE VIEW dbman_test.item_names AS SELECT name FROM dbman_test.items",
            "CREATE FUNCTION dbman_test.add(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT a + b; $$",
            "CREATE TYPE dbman_test.mood AS ENUM ('ok', 'sad')",
            "CREATE SEQUENCE dbman_test.counter START 5",
        ],
    )
    .await;
    let objects = c.objects(&db, "dbman_test").await.unwrap();
    for o in &objects {
        let ddl = c.ddl(o).await.unwrap_or_else(|e| panic!("ddl of {:?}: {e}", o.path));
        assert!(ddl.contains("CREATE"), "{ddl}");
    }
    let f = objects.iter().find(|o| o.path.name == "add").unwrap();
    assert_eq!(f.kind, ObjectKind::Function);
    assert_eq!(f.detail.as_deref(), Some("a integer, b integer"));
    assert!(objects.iter().any(|o| o.kind == ObjectKind::Type && o.path.name == "mood"));
    assert!(objects.iter().any(|o| o.kind == ObjectKind::Sequence && o.path.name == "counter"));
    check_ddl(c.clone(), &db, "dbman_test").await;
    exec(&c, &db, "DROP SCHEMA dbman_test CASCADE").await;
    check_cancel(c, &db, "SELECT pg_sleep(30)").await;
}

#[tokio::test]
async fn mysql() {
    let Some(cfg) = from_url(DbKind::MySql, "DBMAN_TEST_MYSQL") else {
        eprintln!("DBMAN_TEST_MYSQL not set, skipping");
        return;
    };
    let db = cfg.database.clone();
    let c = dbman_drivers::connect(&cfg).await.unwrap();
    check_engine(
        c.clone(),
        &db,
        &db,
        &[
            "DROP VIEW IF EXISTS item_names",
            "DROP TABLE IF EXISTS items",
            "DROP FUNCTION IF EXISTS add_two",
            "CREATE TABLE items (id int PRIMARY KEY, name varchar(50) NOT NULL, parent int, \
             CONSTRAINT items_parent_fk FOREIGN KEY (parent) REFERENCES items(id))",
            "CREATE INDEX items_name_idx ON items(name)",
            "CREATE VIEW item_names AS SELECT name FROM items",
            "CREATE FUNCTION add_two(a int) RETURNS int DETERMINISTIC RETURN a + 2",
        ],
    )
    .await;
    let objects = c.objects(&db, &db).await.unwrap();
    for o in &objects {
        let ddl = c.ddl(o).await.unwrap_or_else(|e| panic!("ddl of {:?}: {e}", o.path));
        assert!(ddl.contains("CREATE"), "{ddl}");
    }
    assert!(objects.iter().any(|o| o.kind == ObjectKind::Function && o.path.name == "add_two"));
    check_ddl(c.clone(), &db, &db).await;
    let blob = rows(&c, &db, "SELECT CAST('ab' AS BINARY) AS b, 'x' AS t").await;
    assert_eq!(blob[0][0], Cell::Bytes(b"ab".to_vec()));
    assert_eq!(blob[0][1], Cell::Text("x".into()));
    check_cancel(c, &db, "SELECT SLEEP(30)").await;
}
