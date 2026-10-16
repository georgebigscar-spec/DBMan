//! Drives the whole window against a sample SQLite database.
//!
//! With `DBMAN_SCREENSHOTS=1` the steps are also rendered to
//! `tests/snapshots/*.png` (needs a wgpu adapter; on a machine without a GPU
//! install a software Vulkan driver such as `mesa-vulkan-drivers`).

use dbman::app::DbManApp;
use dbman::backup_dialog::Scope;
use dbman::explorer::Action;
use dbman::jobs::JobState;
use dbman::store::Settings;
use dbman_core::{Cell, ConnectionConfig, DbKind, DbObject, ObjectKind, ObjectPath};
use dbman_drivers::transfer::ExportSource;
use egui::{Key, Modifiers};
use egui_kittest::{Harness, kittest::Queryable as _};

fn sample_db(dir: &std::path::Path) -> String {
    let path = dir.join("shop.db");
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, city TEXT);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id),
                              total NUMERIC(10,2) NOT NULL, created_at TEXT DEFAULT CURRENT_TIMESTAMP, note TEXT);
         CREATE INDEX orders_customer_idx ON orders(customer_id);
         CREATE VIEW big_orders AS SELECT * FROM orders WHERE total > 100;
         CREATE TRIGGER orders_touch AFTER UPDATE ON orders BEGIN SELECT 1; END;
         INSERT INTO customers (name, email, city) VALUES
           ('Alice', 'alice@example.com', 'Berlin'), ('Bob', 'bob@example.com', NULL),
           ('Carol', 'carol@example.com', 'Lisbon'), ('Dmitry', 'dmitry@example.com', 'Moscow');
         INSERT INTO orders (customer_id, total, created_at, note) VALUES
           (1, 120.50, '2026-10-01 10:00:00', 'gift'), (1, 15.00, '2026-10-02 11:30:00', NULL),
           (2, 310.00, '2026-10-03 09:15:00', 'express'), (4, 42.42, '2026-10-04 18:45:00', NULL);",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

fn settle(h: &mut Harness<'_, DbManApp>) {
    for _ in 0..30 {
        h.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Clicks the first node with this label (a tree row also contains its label text).
fn click(h: &mut Harness<'_, DbManApp>, label: &str) {
    h.get_all_by_label(label).next().unwrap_or_else(|| panic!("no node {label}")).click();
    settle(h);
}

fn snapshot(h: &mut Harness<'_, DbManApp>, name: &str) {
    if std::env::var("DBMAN_SCREENSHOTS").is_ok_and(|v| v == "1") {
        h.snapshot(name);
    }
}

#[test]
fn browse_sqlite_database() {
    let dir = tempfile::tempdir().unwrap();
    // Keep the test away from the user's real settings.
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    cfg.database = sample_db(dir.path());
    let db_path = cfg.database.clone();
    let conn_id = cfg.id;
    let settings = Settings {
        connections: vec![cfg],
        dark: true,
        page_size: 500,
        tools_dir: String::new(),
        language: String::new(),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 860.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    snapshot(&mut h, "welcome");

    // Open the connection, the main database and the tables group.
    click(&mut h, &conn_id.to_string());
    click(&mut h, "main");
    click(&mut h, "tables");
    click(&mut h, "orders");

    let orders = DbObject {
        kind: ObjectKind::Table,
        path: ObjectPath::new("main", "main", "orders"),
        detail: None,
        comment: None,
    };
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenData(conn_id, orders.clone()), &ctx);
    settle(&mut h);
    snapshot(&mut h, "table_data");
    assert_eq!(h.state().tab_titles(), ["orders"]);

    // Edit: change two cells, delete a row, add a row, then submit.
    {
        let dv = h.state_mut().active_data_view().expect("data tab");
        dv.set_cell(0, 2, Cell::Text("130.00".into()));
        dv.set_cell(1, 4, Cell::Text("updated note".into()));
        dv.delete_rows(&[3]);
        dv.add_row();
        dv.set_cell(4, 1, Cell::Text("3".into()));
        dv.set_cell(4, 2, Cell::Text("99.99".into()));
        assert_eq!(dv.statements().len(), 4, "{:?}", dv.statements());
    }
    settle(&mut h);
    snapshot(&mut h, "data_editing");
    h.get_by_label("Submit (4)").click();
    settle(&mut h);
    let check = rusqlite::Connection::open(&db_path).unwrap();
    let rows: Vec<(i64, i64, f64, Option<String>)> = check
        .prepare("SELECT id, customer_id, total, note FROM orders ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            (1, 1, 130.0, Some("gift".into())),
            (2, 1, 15.0, Some("updated note".into())),
            (3, 2, 310.0, Some("express".into())),
            (4, 3, 99.99, None),
        ]
    );

    h.state_mut().handle(Action::OpenObject(conn_id, orders), &ctx);
    settle(&mut h);
    snapshot(&mut h, "object_structure");

    h.state_mut().handle(Action::OpenConsole(conn_id, None), &ctx);
    settle(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text(
        "-- customers with their order totals\nSELECT c.name, c.city, count(o.id) AS orders, sum(o.total) AS total\nFROM customers c LEFT JOIN orders o ON o.customer_id = c.id\nGROUP BY c.id ORDER BY total DESC;",
    );
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    snapshot(&mut h, "console");
    assert_eq!(h.state().tab_titles().len(), 3);
    assert!(
        h.state().console_summaries().iter().any(|s| s.starts_with("4 rows")),
        "{:?}",
        h.state().console_summaries()
    );

    // A long query keeps the window responsive and Stop cancels it on the server.
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text(
        "\nWITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000000000) SELECT count(*) FROM n;",
    );
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    let started = std::time::Instant::now();
    settle(&mut h);
    assert!(
        h.query_by_label("Stop").is_some(),
        "the query should still be running: {:?}",
        h.state().console_summaries()
    );
    click(&mut h, "Stop");
    settle(&mut h);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(h.state().console_summaries(), vec!["cancelled".to_string()]);
    assert!(h.query_by_label("Stop").is_none());

    // Back up the database in the background, damage it, restore it.
    let backup_file = dir.path().join("shop-backup.db");
    h.state_mut().handle(Action::Backup(conn_id, Scope { database: "main".into(), ..Default::default() }), &ctx);
    settle(&mut h);
    h.state_mut().backup_dialog().expect("backup dialog").set_path(&backup_file);
    settle(&mut h);
    snapshot(&mut h, "backup_dialog");
    click(&mut h, "Start backup");
    settle(&mut h);
    assert!(!h.state().backup_dialog_open());
    assert_eq!(h.state().jobs.list.len(), 1);
    assert_eq!(h.state().jobs.list[0].state, JobState::Done, "{:?}", h.state().jobs.list[0].log);
    assert!(backup_file.is_file());
    snapshot(&mut h, "background_tasks");

    check.execute("DELETE FROM orders", []).unwrap();
    h.state_mut().handle(Action::Restore(conn_id, Scope { database: "main".into(), ..Default::default() }), &ctx);
    settle(&mut h);
    h.state_mut().backup_dialog().expect("restore dialog").set_path(&backup_file);
    settle(&mut h);
    click(&mut h, "Start restore");
    settle(&mut h);
    assert_eq!(h.state().jobs.list[1].state, JobState::Done, "{:?}", h.state().jobs.list[1].log);
    let n: i64 = check.query_row("SELECT count(*) FROM orders", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 4);

    // Export a table to CSV and load the file into another table, both in the background.
    let orders = ObjectPath::new("main", "", "orders");
    let csv = dir.path().join("orders.csv");
    h.state_mut().handle(Action::ExportData(conn_id, ExportSource::Table(orders.clone())), &ctx);
    settle(&mut h);
    h.state_mut().export_dialog().expect("export dialog").set_path(&csv);
    settle(&mut h);
    snapshot(&mut h, "export_dialog");
    click(&mut h, "Start export");
    settle(&mut h);
    assert_eq!(h.state().jobs.list[2].state, JobState::Done, "{:?}", h.state().jobs.list[2].log);
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("id,customer_id,total,created_at,note\n1,1,"), "{text}");
    assert_eq!(text.lines().count(), 5, "{text}");

    check.execute("CREATE TABLE orders_copy AS SELECT * FROM orders WHERE 0", []).unwrap();
    h.state_mut().handle(Action::RefreshObjects(conn_id, "main".into(), "main".into()), &ctx);
    let copy = ObjectPath::new("main", "", "orders_copy");
    h.state_mut().handle(Action::ImportData(conn_id, copy), &ctx);
    settle(&mut h);
    h.state_mut().import_dialog().expect("import dialog").set_path(&csv);
    for _ in 0..50 {
        settle(&mut h);
        if h.state_mut().import_dialog().is_some_and(|d| d.preview_ready()) {
            break;
        }
    }
    settle(&mut h);
    snapshot(&mut h, "import_dialog");
    click(&mut h, "Start import");
    settle(&mut h);
    assert_eq!(h.state().jobs.list[3].state, JobState::Done, "{:?}", h.state().jobs.list[3].log);
    let same: i64 = check
        .query_row("SELECT count(*) FROM (SELECT * FROM orders EXCEPT SELECT * FROM orders_copy)", [], |r| r.get(0))
        .unwrap();
    let n: i64 = check.query_row("SELECT count(*) FROM orders_copy", [], |r| r.get(0)).unwrap();
    assert_eq!((n, same), (4, 0));

    // The schema's ER diagram: tables as boxes, foreign keys as links.
    h.state_mut().handle(Action::OpenDiagram(conn_id, "main".into(), "main".into()), &ctx);
    settle(&mut h);
    settle(&mut h);
    let d = h.state_mut().active_diagram().and_then(|v| v.diagram().cloned()).expect("diagram loaded");
    let names: Vec<&str> = d.entities.iter().map(|e| e.path.name.as_str()).collect();
    assert!(names.contains(&"customers") && names.contains(&"orders") && names.contains(&"orders_copy"), "{names:?}");
    assert_eq!(d.links.len(), 1);
    assert_eq!(d.entities[d.links[0].to].path.name, "customers");
    assert!(h.query_by_label("ER diagram").is_none());
    snapshot(&mut h, "er_diagram");

    // Compare the schema with a staging copy that lags behind, and sync it.
    let staging = dir.path().join("staging.db");
    rusqlite::Connection::open(&staging)
        .unwrap()
        .execute_batch(
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE);
             CREATE TABLE legacy (id INTEGER PRIMARY KEY);
             INSERT INTO customers (id, name, email) VALUES
               (1, 'Alice', 'alice@example.com'), (2, 'Bob', 'bob@old.example.com'), (9, 'Zed', NULL);",
        )
        .unwrap();
    let mut staging_cfg = ConnectionConfig::new(DbKind::Sqlite);
    staging_cfg.name = "staging".into();
    staging_cfg.database = staging.to_string_lossy().into_owned();
    h.state_mut().add_connection(staging_cfg);
    h.state_mut().handle(Action::OpenCompare(conn_id, "main".into(), "main".into()), &ctx);
    settle(&mut h);
    let wait_compare = |h: &mut Harness<'_, DbManApp>| {
        for _ in 0..50 {
            settle(h);
            if !h.state_mut().active_compare().unwrap().is_running() {
                break;
            }
        }
    };
    assert_eq!(h.state_mut().active_compare().unwrap().target_database(), Some("main"));
    click(&mut h, "Compare structure");
    wait_compare(&mut h);
    let rows = h
        .state_mut()
        .active_compare()
        .unwrap()
        .structure()
        .map(|r| r.into_iter().map(|(n, s)| (n.to_string(), s)).collect::<std::collections::HashMap<_, _>>());
    let rows = rows.expect("structure compared");
    use dbman_core::compare::Status;
    assert_eq!(rows.get("customers"), Some(&Status::Changed), "{rows:?}");
    assert_eq!(rows.get("orders"), Some(&Status::OnlySource));
    assert_eq!(rows.get("big_orders"), Some(&Status::OnlySource));
    assert_eq!(rows.get("legacy"), Some(&Status::OnlyTarget));
    let script = h.state_mut().active_compare().unwrap().script_text();
    assert!(script.contains("ALTER TABLE \"main\".\"customers\" ADD COLUMN \"city\" TEXT"), "{script}");
    assert!(!script.contains("DROP TABLE"), "dropping is opt-in: {script}");
    h.state_mut().active_compare().unwrap().select("customers");
    settle(&mut h);
    snapshot(&mut h, "compare_structure");
    let jobs = h.state().jobs.list.len();
    click(&mut h, "Apply to target");
    click(&mut h, "Apply");
    settle(&mut h);
    let job = &h.state().jobs.list[jobs];
    assert_eq!(job.state, JobState::Done, "{:?}", job.log);
    let target = rusqlite::Connection::open(&staging).unwrap();
    let n: i64 = target
        .query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('orders', 'big_orders')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);

    // Rows: the new city column and Bob's email differ, Carol and Dmitry are missing, Zed is extra.
    h.state_mut().active_compare().unwrap().set_mode(true);
    settle(&mut h);
    click(&mut h, "Compare data");
    wait_compare(&mut h);
    let data = h.state_mut().active_compare().unwrap().data().unwrap().to_vec();
    let customers = data.iter().find(|r| r.name == "customers").unwrap();
    let dbman::compare_view::DataState::Done(d) = &customers.state else { panic!("{customers:?}") };
    assert_eq!((d.same, d.updated, d.inserted, d.deleted), (0, 2, 2, 1), "{d:?}");
    h.state_mut().active_compare().unwrap().select("customers");
    settle(&mut h);
    snapshot(&mut h, "compare_data");
    let jobs = h.state().jobs.list.len();
    click(&mut h, "Apply to target");
    click(&mut h, "Apply");
    settle(&mut h);
    let job = &h.state().jobs.list[jobs];
    assert_eq!(job.state, JobState::Done, "{:?}", job.log);
    target.execute("ATTACH DATABASE ?1 AS src", [&db_path]).unwrap();
    for t in ["customers", "orders"] {
        for (a, b) in [("src", "main"), ("main", "src")] {
            let sql = format!("SELECT count(*) FROM (SELECT * FROM {a}.{t} EXCEPT SELECT * FROM {b}.{t})");
            let diff: i64 = target.query_row(&sql, [], |r| r.get(0)).unwrap();
            assert_eq!(diff, 0, "{t}: rows of {a} missing in {b}");
        }
    }

    // Copy the tables with their rows into an empty DuckDB file, then one table again in place of its rows.
    let archive = dir.path().join("archive.duckdb");
    let mut archive_cfg = ConnectionConfig::new(DbKind::DuckDb);
    archive_cfg.name = "archive".into();
    archive_cfg.database = archive.to_string_lossy().into_owned();
    let archive_id = archive_cfg.id;
    h.state_mut().add_connection(archive_cfg);
    h.state_mut().handle(Action::OpenCopy(conn_id, "main".into(), "main".into(), None), &ctx);
    h.state_mut().active_copy().unwrap().set_target(archive_id);
    let wait_target = |h: &mut Harness<'_, DbManApp>| {
        for _ in 0..50 {
            settle(h);
            if h.state_mut().active_copy().unwrap().in_target().is_some() {
                break;
            }
        }
    };
    wait_target(&mut h);
    let chosen: Vec<String> = h.state_mut().active_copy().unwrap().chosen().iter().map(|s| s.to_string()).collect();
    assert!(chosen.contains(&"customers".to_string()) && chosen.contains(&"orders".to_string()), "{chosen:?}");
    h.state_mut().active_copy().unwrap().select("customers");
    settle(&mut h);
    snapshot(&mut h, "copy_tables");
    let jobs = h.state().jobs.list.len();
    click(&mut h, &format!("Copy {} tables", chosen.len()));
    settle(&mut h);
    let job = &h.state().jobs.list[jobs];
    assert_eq!(job.state, JobState::Done, "{:?}", job.log);
    let count = |table: &str| -> i64 {
        let duck = duckdb::Connection::open(&archive).unwrap();
        duck.query_row(&format!("SELECT count(*) FROM main.{table}"), [], |r| r.get(0)).unwrap()
    };
    let source = rusqlite::Connection::open(&db_path).unwrap();
    let customers: i64 = source.query_row("SELECT count(*) FROM customers", [], |r| r.get(0)).unwrap();
    assert_eq!(count("customers"), customers);

    h.state_mut().handle(Action::OpenCopy(conn_id, "main".into(), "main".into(), Some("orders".into())), &ctx);
    h.state_mut().active_copy().unwrap().set_target(archive_id);
    h.state_mut().active_copy().unwrap().set_existing(dbman_drivers::copy::Existing::Replace);
    wait_target(&mut h);
    assert!(h.state_mut().active_copy().unwrap().in_target().unwrap().contains("orders"));
    let jobs = h.state().jobs.list.len();
    click(&mut h, "Copy 1 table");
    click(&mut h, "Copy");
    settle(&mut h);
    let job = &h.state().jobs.list[jobs];
    assert_eq!(job.state, JobState::Done, "{:?}", job.log);
    let orders: i64 = source.query_row("SELECT count(*) FROM orders", [], |r| r.get(0)).unwrap();
    assert_eq!(count("orders"), orders, "replaced, not added");

    // Test rows for orders: customer ids come from customers.
    let orders_path = dbman_core::ObjectPath::new("main", "main", "orders");
    h.state_mut().handle(Action::GenerateData(conn_id, orders_path), &ctx);
    settle(&mut h);
    let plan = h.state_mut().generate_dialog().expect("generator open").plan();
    let customer_id = plan.iter().find(|p| p.column == "customer_id").unwrap();
    assert!(matches!(&customer_id.kind, dbman_core::generate::Kind::Reference { table, .. } if table == "customers"));
    h.state_mut().generate_dialog().unwrap().set_count(200);
    settle(&mut h);
    snapshot(&mut h, "generate_dialog");
    let before: i64 = source.query_row("SELECT count(*) FROM orders", [], |r| r.get(0)).unwrap();
    let jobs = h.state().jobs.list.len();
    click(&mut h, "Generate");
    settle(&mut h);
    let job = &h.state().jobs.list[jobs];
    assert_eq!(job.state, JobState::Done, "{:?}", job.log);
    let after: i64 = source.query_row("SELECT count(*) FROM orders", [], |r| r.get(0)).unwrap();
    assert_eq!(after - before, 200);
    let orphans: i64 = source
        .query_row("SELECT count(*) FROM orders WHERE customer_id NOT IN (SELECT id FROM customers)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(orphans, 0);

    // The same window in Russian.
    h.state_mut().set_language(dbman::i18n::Lang::Ru);
    h.state_mut().active_copy().unwrap().select("orders");
    settle(&mut h);
    assert!(h.query_by_label("Копировать 1 таблицу").is_some());
    snapshot(&mut h, "russian");
    h.state_mut().set_language(dbman::i18n::Lang::En);
    settle(&mut h);

    h.state_mut().handle(Action::NewConnection, &ctx);
    settle(&mut h);
    snapshot(&mut h, "connection_dialog");
}
