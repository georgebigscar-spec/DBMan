//! Drives the whole window against a sample SQLite database.
//!
//! With `DBMAN_SCREENSHOTS=1` the steps are also rendered to
//! `tests/snapshots/*.png` (needs a wgpu adapter; on a machine without a GPU
//! install a software Vulkan driver such as `mesa-vulkan-drivers`).

use dbman::app::DbManApp;
use dbman::explorer::Action;
use dbman::store::Settings;
use dbman_core::{ConnectionConfig, DbKind, DbObject, ObjectKind, ObjectPath};
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
    let conn_id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500 };

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

    h.state_mut().handle(Action::NewConnection, &ctx);
    settle(&mut h);
    snapshot(&mut h, "connection_dialog");
}
