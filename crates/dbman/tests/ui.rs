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
use dbman_core::{Cell, ConnColor, ConnectionConfig, DbKind, DbObject, ObjectKind, ObjectPath};
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

/// A cell of a test workbook.
enum Value {
    Number(String),
    Text(String),
    Empty,
}

/// A workbook with a "Summary" sheet and a sheet of rows (numbers as numbers, text inline).
fn workbook(path: &std::path::Path, columns: &[&str], rows: &[Vec<Value>]) {
    use std::io::Write;
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let text = |r: String, s: &str| format!(r#"<c r="{r}" t="inlineStr"><is><t>{}</t></is></c>"#, esc(s));
    let sheet = |body: String| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{body}</sheetData></worksheet>"#
        )
    };
    let col = |j: usize| ((b'A' + j as u8) as char).to_string();
    let mut body = String::from(r#"<row r="1">"#);
    for (j, c) in columns.iter().enumerate() {
        body.push_str(&text(format!("{}1", col(j)), c));
    }
    body.push_str("</row>");
    for (i, row) in rows.iter().enumerate() {
        let n = i + 2;
        body.push_str(&format!(r#"<row r="{n}">"#));
        for (j, cell) in row.iter().enumerate() {
            let r = format!("{}{n}", col(j));
            match cell {
                Value::Number(v) => body.push_str(&format!(r#"<c r="{r}"><v>{v}</v></c>"#)),
                Value::Text(v) => body.push_str(&text(r, v)),
                Value::Empty => {}
            }
        }
        body.push_str("</row>");
    }
    let parts = [
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Summary" sheetId="1" r:id="rId1"/><sheet name="Orders" sheetId="2" r:id="rId2"/></sheets></workbook>"#.to_string(),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="worksheets/sheet2.xml"/></Relationships>"#.to_string(),
        ),
        ("xl/worksheets/sheet1.xml", sheet(format!(r#"<row r="1">{}</row>"#, text("A1".into(), "Orders by month")))),
        ("xl/worksheets/sheet2.xml", sheet(body)),
    ];
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, part) in parts {
        zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(part.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

fn settle(h: &mut Harness<'_, DbManApp>) {
    for _ in 0..30 {
        h.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Settles until background job `i` is no longer running, so slow CI runners
/// don't see it mid-way.
fn wait_job(h: &mut Harness<'_, DbManApp>, i: usize) {
    for _ in 0..50 {
        settle(h);
        if h.state().jobs.list.get(i).is_some_and(|j| j.state != JobState::Running) {
            return;
        }
    }
}

/// Clicks the first node with this label (a tree row also contains its label text).
fn click(h: &mut Harness<'_, DbManApp>, label: &str) {
    h.get_all_by_label(label).next().unwrap_or_else(|| panic!("no node {label}")).click();
    settle(h);
}

/// Clicks like a mouse does: move, press and release in separate frames.
fn press_and_release(h: &mut Harness<'_, DbManApp>, label: &str) {
    let pos = h.get_all_by_label(label).next().unwrap_or_else(|| panic!("no node {label}")).rect().center();
    h.event(egui::Event::PointerMoved(pos));
    h.run_steps(2);
    for pressed in [true, false] {
        let button = egui::PointerButton::Primary;
        h.event(egui::Event::PointerButton { pos, button, pressed, modifiers: Modifiers::NONE });
        h.run_steps(2);
    }
    settle(h);
}

/// Clicks the last node with this label, such as a dialog's button over a toolbar's.
fn click_last(h: &mut Harness<'_, DbManApp>, label: &str) {
    h.get_all_by_label(label).last().unwrap_or_else(|| panic!("no node {label}")).click();
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
    // And away from the system keychain.
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
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
        recent_scripts: Vec::new(),
        ..Default::default()
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

    // The command palette finds a loaded table and opens it, then closes the tab.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    settle(&mut h);
    h.event(egui::Event::Text("custo".into()));
    settle(&mut h);
    snapshot(&mut h, "command_palette");
    click(&mut h, "main.customers");
    assert_eq!(h.state().tab_titles(), ["customers"]);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::K);
    settle(&mut h);
    h.event(egui::Event::Text("close".into()));
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    assert!(h.state().tab_titles().is_empty(), "{:?}", h.state().tab_titles());
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(h.state().window_title(), format!("DBMan {version}"));

    // The tab's × closes it; the window title names the database and the table meanwhile.
    let customers = DbObject {
        kind: ObjectKind::Table,
        path: ObjectPath::new("main", "main", "customers"),
        detail: None,
        comment: None,
    };
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenData(conn_id, customers), &ctx);
    settle(&mut h);
    assert_eq!(h.state().window_title(), format!("DBMan {version} - shop.db - customers"));
    press_and_release(&mut h, "×");
    assert!(h.state().tab_titles().is_empty(), "{:?}", h.state().tab_titles());

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

    // Autocomplete: tables after FROM, an alias's columns after `c.`; Tab takes the selected one.
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("\nSELECT * FROM cust");
    for _ in 0..10 {
        h.run_steps(2);
        settle(&mut h);
        if !h.state().console_completion().0.is_empty() {
            break;
        }
    }
    assert_eq!(h.state().console_completion().0, ["customers"], "{:?}", h.state().console_completion().1);
    snapshot(&mut h, "autocomplete");
    h.key_press(Key::Tab);
    h.run_steps(2);
    assert!(
        h.state().console_completion().1.ends_with("SELECT * FROM customers"),
        "{}",
        h.state().console_completion().1
    );
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text(" c WHERE c.ci");
    for _ in 0..10 {
        h.run_steps(2);
        settle(&mut h);
        if !h.state().console_completion().0.is_empty() {
            break;
        }
    }
    assert_eq!(h.state().console_completion().0, ["city"]);
    h.key_press(Key::Enter);
    h.run_steps(2);
    let (items, sql) = h.state().console_completion();
    assert!(sql.ends_with("FROM customers c WHERE c.city"), "{sql}");
    assert!(items.is_empty());

    // Query history: the statements run so far, newest first; a click adds one to the script.
    click(&mut h, "History");
    let history = h.state().console_history();
    assert_eq!(history.len(), 2, "{history:?}");
    assert!(history[0].starts_with("WITH RECURSIVE"), "{history:?}");
    snapshot(&mut h, "query_history");
    h.get_all_by_label("FROM customers c LEFT JOIN orders o ON o.customer_id = c.id").last().unwrap().click();
    settle(&mut h);
    let (_, sql) = h.state().console_completion();
    assert!(sql.ends_with(&format!("c.city;\n{}", history[1].trim_end())), "{sql}");
    click(&mut h, "History");
    assert!(h.query_by_label("Query history").is_none());

    // The plan of the statement under the cursor (the one just added), without running it.
    let runs = h.state().console_history().len();
    click(&mut h, "Explain");
    for _ in 0..20 {
        settle(&mut h);
        if !h.state().console_plan().is_empty() {
            break;
        }
    }
    let plan = h.state().console_plan();
    assert!(plan.iter().any(|s| s.trim() == "SCAN c"), "{plan:?}");
    assert!(plan.iter().any(|s| s.trim().starts_with("SEARCH o") || s.trim().starts_with("SCAN o")), "{plan:?}");
    assert_eq!(h.state().console_history().len(), runs, "explaining is not a run");
    snapshot(&mut h, "explain_plan");

    // Formatting lays the script out one clause per line.
    click(&mut h, "Format SQL");
    settle(&mut h);
    let (_, sql) = h.state().console_completion();
    assert!(sql.contains("\nFROM customers c\nLEFT JOIN orders o ON o.customer_id = c.id"), "{sql}");
    assert_eq!(dbman_core::format::format_sql(&sql, DbKind::Sqlite), sql, "formatting again changes nothing");
    snapshot(&mut h, "format_sql");

    // A script file opens in a new console and saves back with Ctrl+S.
    let script = dir.path().join("report.sql");
    std::fs::write(&script, "SELECT 1;\n").unwrap();
    h.state_mut().handle(Action::OpenScript(conn_id, None, script.clone()), &ctx);
    settle(&mut h);
    let (title, file, sql) = h.state().active_console().expect("script console");
    assert_eq!((title.as_str(), file.as_ref(), sql.as_str()), ("report.sql", Some(&script), "SELECT 1;\n"));
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().focus();
    h.run_steps(2);
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().type_text("SELECT 2;");
    settle(&mut h);
    assert_eq!(h.state().active_console().unwrap().0, "report.sql •");
    h.key_press_modifiers(Modifiers::COMMAND, Key::S);
    settle(&mut h);
    assert_eq!(h.state().active_console().unwrap().0, "report.sql");
    let saved = std::fs::read_to_string(&script).unwrap();
    assert!(saved.starts_with("SELECT 1;") && saved.contains("SELECT 2;"), "{saved}");
    assert_eq!(h.state().recent_scripts(), [script]);

    // The value panel shows the clicked cell whole: JSON indented, an image drawn.
    let png = "89504E470D0A1A0A0000000D4948445200000004000000040802000000269309290000001449444154789C63F8CFC0C0D0F01F423210C7010000B213F18E391A340000000049454E44AE426082";
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().focus();
    h.run_steps(2);
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .last()
        .unwrap()
        .type_text(&format!("\nSELECT '{{\"id\":7,\"tags\":[\"a\",\"b\"]}}' AS doc, X'{png}' AS picture;"));
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    // The last node is the cell's label; the first is the whole row.
    h.get_all_by_label(r#"{"id":7,"tags":["a","b"]}"#).last().unwrap().click();
    settle(&mut h);
    click(&mut h, "Value");
    assert_eq!(h.state().console_value_kind().as_deref(), Some("JSON"));
    let bytes: Vec<u8> = (0..png.len()).step_by(2).map(|i| u8::from_str_radix(&png[i..i + 2], 16).unwrap()).collect();
    h.get_all_by_label(&Cell::Bytes(bytes).display()).last().unwrap().click();
    settle(&mut h);
    assert_eq!(h.state().console_value_kind().as_deref(), Some("PNG image"));
    assert!(h.query_by_label("4 × 4").is_some(), "the image is decoded");
    snapshot(&mut h, "value_panel");
    click(&mut h, "Value");
    assert_eq!(h.state().console_value_kind(), None);

    // Shift+click selects a range of cells; the line under the grid sums it.
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().focus();
    h.run_steps(2);
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().type_text(
        "\nSELECT 1.5 AS price, 'a' AS name UNION ALL SELECT 2.25, 'b' UNION ALL SELECT NULL, 'c' UNION ALL SELECT 10, 'd';",
    );
    settle(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    h.get_all_by_label("1.5").last().unwrap().click();
    settle(&mut h);
    h.get_all_by_label("10").last().unwrap().click_modifiers(Modifiers::SHIFT);
    settle(&mut h);
    for label in ["13.75", "4.58"] {
        assert!(h.query_by_label(label).is_some(), "{label} in the stats line");
    }
    h.get_all_by_label("d").last().unwrap().click_modifiers(Modifiers::COMMAND);
    settle(&mut h);
    snapshot(&mut h, "selection_stats");
    h.get_by_label("Copy").click();
    h.step();
    let copied = h.output().platform_output.commands.iter().find_map(|c| match c {
        egui::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    });
    assert_eq!(copied.as_deref(), Some("1.5\t\n2.25\t\n\t\n10\td"));
    settle(&mut h);

    // Manual commit: the run opens a transaction that Rollback undoes.
    let orders_count =
        |c: &rusqlite::Connection| c.query_row("SELECT count(*) FROM orders", [], |r| r.get::<_, i64>(0)).unwrap();
    click(&mut h, "Auto-commit");
    click(&mut h, "Manual commit");
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().focus();
    h.run_steps(2);
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).last().unwrap().type_text("\nDELETE FROM orders;");
    settle(&mut h);
    click(&mut h, "Run script");
    settle(&mut h);
    assert_eq!(h.state().console_in_tx(), Some(true), "{:?}", h.state().console_summaries());
    assert_eq!(orders_count(&check), 4, "not committed yet");
    snapshot(&mut h, "transaction");
    click(&mut h, "Rollback");
    assert_eq!(h.state().console_in_tx(), Some(false));
    assert_eq!(orders_count(&check), 4);
    // Closing a console with an open transaction asks first, then rolls it back.
    click(&mut h, "Run script");
    assert_eq!(h.state().console_in_tx(), Some(true));
    h.key_press_modifiers(Modifiers::COMMAND, Key::W);
    settle(&mut h);
    assert!(h.query_by_label("Close the console?").is_some());
    click(&mut h, "Roll back and close");
    assert!(!h.state().tab_titles().iter().any(|t| t.starts_with("report.sql")), "{:?}", h.state().tab_titles());
    check.execute("UPDATE orders SET note = note", []).expect("the transaction was rolled back");
    assert_eq!(orders_count(&check), 4);

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

    // The same rows from a sheet of an Excel workbook, replacing the copy's rows.
    let xlsx = dir.path().join("orders.xlsx");
    let mut st = check.prepare("SELECT id, customer_id, total, created_at, note FROM orders").unwrap();
    let rows: Vec<Vec<Value>> = st
        .query_map([], |r| {
            Ok((0..5)
                .map(|i| match r.get_ref(i).unwrap() {
                    rusqlite::types::ValueRef::Integer(v) => Value::Number(v.to_string()),
                    rusqlite::types::ValueRef::Real(v) => Value::Number(v.to_string()),
                    rusqlite::types::ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into_owned()),
                    _ => Value::Empty,
                })
                .collect())
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    drop(st);
    workbook(&xlsx, &["id", "customer_id", "total", "created_at", "note"], &rows);
    let copy = ObjectPath::new("main", "", "orders_copy");
    h.state_mut().handle(Action::ImportData(conn_id, copy), &ctx);
    settle(&mut h);
    let wait = |h: &mut Harness<'_, DbManApp>| {
        for _ in 0..50 {
            settle(h);
            if h.state_mut().import_dialog().is_some_and(|d| d.preview_ready()) {
                break;
            }
        }
    };
    h.state_mut().import_dialog().expect("import dialog").set_path(&xlsx);
    wait(&mut h);
    h.state_mut().import_dialog().unwrap().set_sheet("Orders");
    h.state_mut().import_dialog().unwrap().set_clear_first(true);
    wait(&mut h);
    settle(&mut h);
    snapshot(&mut h, "import_excel");
    click(&mut h, "Start import");
    settle(&mut h);
    assert_eq!(h.state().jobs.list[4].state, JobState::Done, "{:?}", h.state().jobs.list[4].log);
    let same: i64 = check
        .query_row("SELECT count(*) FROM (SELECT * FROM orders EXCEPT SELECT * FROM orders_copy)", [], |r| r.get(0))
        .unwrap();
    let n: i64 = check.query_row("SELECT count(*) FROM orders_copy", [], |r| r.get(0)).unwrap();
    assert_eq!((n, same), (4, 0));

    // A new table from the CSV file, its column types guessed from the values.
    h.state_mut().handle(Action::ImportNewTable(conn_id, "main".into(), "main".into()), &ctx);
    settle(&mut h);
    h.state_mut().import_dialog().expect("import dialog").set_path(&csv);
    wait(&mut h);
    h.state_mut().import_dialog().unwrap().set_table_name("orders_imported");
    settle(&mut h);
    let types: Vec<(String, String)> = h.state_mut().import_dialog().unwrap().new_columns().unwrap().to_vec();
    assert_eq!(
        types,
        [
            ("id".to_string(), "INTEGER".to_string()),
            ("customer_id".into(), "INTEGER".into()),
            ("total".into(), "NUMERIC".into()),
            ("created_at".into(), "TEXT".into()),
            ("note".into(), "TEXT".into()),
        ]
    );
    snapshot(&mut h, "import_new_table");
    click(&mut h, "Start import");
    settle(&mut h);
    assert_eq!(h.state().jobs.list[5].state, JobState::Done, "{:?}", h.state().jobs.list[5].log);
    let same: i64 = check
        .query_row("SELECT count(*) FROM (SELECT * FROM orders EXCEPT SELECT * FROM orders_imported)", [], |r| r.get(0))
        .unwrap();
    let n: i64 = check.query_row("SELECT count(*) FROM orders_imported", [], |r| r.get(0)).unwrap();
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
    wait_job(&mut h, jobs);
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
    wait_job(&mut h, jobs);
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
    wait_job(&mut h, jobs);
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
    wait_job(&mut h, jobs);
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
    wait_job(&mut h, jobs);
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

    // A database on PostgreSQL's protocol gets its own port, user and database.
    click(&mut h, "PostgreSQL");
    click(&mut h, "CockroachDB");
    let d = h.state_mut().connection_dialog().expect("dialog open");
    assert_eq!(d.cfg.kind, DbKind::Postgres);
    assert_eq!(d.cfg.product, dbman_core::Product::CockroachDb);
    assert_eq!((d.port(), d.cfg.user.as_str(), d.cfg.database.as_str()), ("26257", "root", "defaultdb"));
    snapshot(&mut h, "compatible_databases");
    click(&mut h, "CockroachDB");
    click(&mut h, "PostgreSQL");
    let d = h.state_mut().connection_dialog().unwrap();
    assert_eq!((d.port(), d.cfg.user.as_str(), d.cfg.database.as_str()), ("5432", "postgres", ""));

    // The SSH tab: a tunnel through a bastion.
    click(&mut h, "SSH");
    click(&mut h, "Connect through an SSH server");
    assert!(h.query_by_label("SSH (on)").is_some(), "the tab shows that SSH is on");
    assert!(h.query_by_label("SSH host").is_some());
    click(&mut h, "Password");
    click(&mut h, "Private key file");
    assert!(h.query_by_label("Passphrase").is_some());
    snapshot(&mut h, "ssh_tunnel");
}

/// The sessions tab of a PostgreSQL server (needs `DBMAN_TEST_POSTGRES`): a
/// second session sleeps, shows up in the list and is ended from its menu.
#[test]
fn sessions_monitor() {
    let Some(url) = std::env::var("DBMAN_TEST_POSTGRES").ok().filter(|u| !u.is_empty()) else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, addr) = rest.rsplit_once('@').unwrap();
    let (user, password) = auth.split_once(':').unwrap();
    let (hostport, db) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Postgres);
    cfg.name = "local".into();
    cfg.host = host.into();
    cfg.port = port.parse().unwrap();
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.database = db.into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    let conn_id = cfg.id;
    let settings = Settings {
        connections: vec![cfg.clone()],
        dark: true,
        page_size: 500,
        tools_dir: String::new(),
        language: String::new(),
        recent_scripts: Vec::new(),
        ..Default::default()
    };

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    // A session that sleeps until it is ended, on a runtime of its own; the
    // marker tells it from sleepers left by an earlier failed run.
    let marker = uuid::Uuid::new_v4().to_string();
    let other = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let sleeper = other.spawn({
        let (cfg, marker) = (cfg.clone(), marker.clone());
        async move {
            let c = dbman_drivers::connect(&cfg).await.unwrap();
            c.execute(None, &format!("SELECT pg_sleep(120) /* nightly report {marker} */"), 1).await
        }
    });
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 700.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &conn_id.to_string());
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenSessions(conn_id), &ctx);
    settle(&mut h);
    assert_eq!(h.state().tab_titles(), ["Sessions"]);
    let mut sleeping = None;
    for _ in 0..20 {
        let view = h.state_mut().active_sessions().unwrap();
        sleeping = view.sessions().iter().find(|s| s.query.contains(&marker)).map(|s| s.id.clone());
        if sleeping.is_some() {
            break;
        }
        settle(&mut h);
    }
    let sleeping = sleeping.expect("the sleeping session is listed");
    h.state_mut().active_sessions().unwrap().pick(&sleeping);
    settle(&mut h);
    assert!(h.query_by_label("Kill session…").is_some());
    snapshot(&mut h, "sessions");

    click(&mut h, "Kill session…");
    assert!(h.query_by_label("Kill session").is_some(), "asks first");
    press_and_release(&mut h, "Kill session");
    let ended = other.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(10), sleeper).await });
    assert!(ended.expect("the session ends").unwrap().is_err());
}

/// Connection options on SQLite: folders in the explorer, a colored
/// read-only connection that refuses changes, and one that asks first.
#[test]
fn connection_options() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let db = sample_db(dir.path());
    let mut prod = ConnectionConfig::new(DbKind::Sqlite);
    prod.name = "shop (production)".into();
    prod.database = db.clone();
    prod.folder = "Production".into();
    prod.color = ConnColor::Red;
    prod.read_only = true;
    let mut stage = ConnectionConfig::new(DbKind::Sqlite);
    stage.name = "shop (staging)".into();
    stage.database = db.clone();
    stage.folder = "Staging".into();
    stage.color = ConnColor::Yellow;
    stage.confirm_changes = true;
    let mut local = ConnectionConfig::new(DbKind::Sqlite);
    local.name = "scratch".into();
    local.database = dir.path().join("scratch.db").to_string_lossy().into_owned();
    let (prod_id, stage_id) = (prod.id, stage.id);
    let settings = Settings {
        connections: vec![prod, stage, local],
        dark: true,
        page_size: 500,
        tools_dir: String::new(),
        language: String::new(),
        recent_scripts: Vec::new(),
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 700.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    let count = || {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.query_row("SELECT count(*) FROM orders", [], |r| r.get::<_, i64>(0)).unwrap()
    };

    // Folders come first; a folder opens to its connections.
    assert!(h.query_by_label("Production").is_some());
    assert!(h.query_by_label("Staging").is_some());
    click(&mut h, "Production");
    click(&mut h, &prod_id.to_string());
    assert!(h.query_by_label("read-only").is_some());

    // The read-only connection runs reads and refuses changes.
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenConsole(prod_id, None), &ctx);
    settle(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text("SELECT count(*) FROM orders;\nDELETE FROM orders;");
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter);
    settle(&mut h);
    let summaries = h.state().console_summaries();
    assert!(summaries[0].starts_with("1 rows"), "{summaries:?}");
    assert_eq!(summaries[1], "failed");
    assert!(h.query_by_label_contains("The connection is read-only").is_some(), "the error says why");
    assert_eq!(count(), 4);
    snapshot(&mut h, "read_only");
    // Actions that change a read-only connection are refused too.
    let orders = DbObject {
        kind: ObjectKind::Table,
        path: ObjectPath::new("main", "main", "orders"),
        detail: None,
        comment: None,
    };
    h.state_mut().handle(Action::Truncate(prod_id, orders), &ctx);
    settle(&mut h);
    assert!(h.query_by_label_contains("Delete all rows").is_none(), "no truncate dialog");

    // The staging connection asks before a change, and runs nothing when cancelled.
    h.state_mut().handle(Action::OpenConsole(stage_id, None), &ctx);
    settle(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("DELETE FROM orders WHERE id = 1");
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert!(h.query_by_label("Run changes?").is_some());
    snapshot(&mut h, "confirm_changes");
    press_and_release(&mut h, "Cancel");
    assert_eq!(count(), 4);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    press_and_release(&mut h, "Run");
    assert_eq!(count(), 3);
    // Reads run without asking.
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("SELECT 1");
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert!(h.query_by_label("Run changes?").is_none());

    // The options tab of the connection dialog.
    h.state_mut().handle(Action::EditConnection(prod_id), &ctx);
    settle(&mut h);
    click(&mut h, "Options");
    assert!(h.query_by_label("Read-only").is_some());
    snapshot(&mut h, "connection_options");
}

/// Query parameters on SQLite: the console asks for `:city` and `${min}`
/// before running, and offers the same values the next time.
#[test]
fn query_parameters() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    cfg.database = sample_db(dir.path());
    let conn_id = cfg.id;
    let settings = Settings { connections: vec![cfg], ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 700.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenConsole(conn_id, None), &ctx);
    settle(&mut h);
    let editor = || egui::accesskit::Role::MultilineTextInput;
    h.get_by_role(editor()).focus();
    h.run_steps(2);
    h.get_by_role(editor())
        .type_text("SELECT name FROM customers\nWHERE city = :city AND id >= ${min_id} -- not :this\nORDER BY name");
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert!(h.query_by_label("Query parameters").is_some(), "asks for the values");
    assert!(h.query_by_label("city").is_some());
    assert!(h.query_by_label("min_id").is_some());
    assert!(h.query_by_label("this").is_none(), "comments have no parameters");
    assert_eq!(h.get_all_by_role(egui::accesskit::Role::TextInput).count(), 2);
    for (i, value) in ["'Berlin'", "1"].into_iter().enumerate() {
        h.get_all_by_role(egui::accesskit::Role::TextInput).nth(i).unwrap().focus();
        h.run_steps(2);
        h.get_all_by_role(egui::accesskit::Role::TextInput).nth(i).unwrap().type_text(value);
        h.run_steps(2);
    }
    settle(&mut h);
    snapshot(&mut h, "query_parameters");
    click_last(&mut h, "Run");
    let summaries = h.state().console_summaries();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].starts_with("1 rows"), "{summaries:?}");
    assert_eq!(h.state().remembered_param("city"), Some("'Berlin'"));

    // The next run offers the same values.
    h.get_by_role(editor()).focus();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert!(h.query_by_label("Query parameters").is_some());
    click_last(&mut h, "Run");
    assert!(h.state().console_summaries()[0].starts_with("1 rows"));
}

/// Tabs come back after a restart: a console with unsaved text, a table's
/// data and the object structure, with the same tab active.
#[test]
fn restores_tabs() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    cfg.database = sample_db(dir.path());
    let conn_id = cfg.id;
    let settings = Settings { connections: vec![cfg], ..Default::default() };
    let orders = DbObject {
        kind: ObjectKind::Table,
        path: ObjectPath::new("main", "main", "orders"),
        detail: None,
        comment: None,
    };
    let workspace = dir.path().join("workspace.json");
    let start = |settings: Settings| {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let ctx = egui::Context::default();
        let app = DbManApp::with_settings(&ctx, runtime, settings).keep_workspace(workspace.clone());
        let mut h = Harness::builder()
            .with_size(egui::vec2(1400.0, 700.0))
            .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
        settle(&mut h);
        h
    };

    let mut h = start(settings.clone());
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenData(conn_id, orders.clone()), &ctx);
    h.state_mut().handle(Action::OpenObject(conn_id, orders.clone()), &ctx);
    h.state_mut().handle(Action::OpenConsole(conn_id, None), &ctx);
    settle(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("SELECT count(*) FROM orders -- draft");
    // The data tab is the active one when the app closes.
    click(&mut h, "orders");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    settle(&mut h);
    let before = h.state().tab_titles();
    assert_eq!(before.len(), 3, "{before:?}");
    drop(h);

    let mut h = start(settings);
    assert_eq!(h.state().tab_titles(), before);
    assert_eq!(h.state().workspace().active, Some(0));
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenConsole(conn_id, None), &ctx);
    let w = h.state().workspace();
    match &w.tabs[2] {
        dbman::store::SavedTab::Console { sql, .. } => assert_eq!(sql, "SELECT count(*) FROM orders -- draft"),
        other => panic!("{other:?}"),
    }
    // The restored console runs its text.
    h.state_mut().select_tab(2);
    settle(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert!(
        h.state().console_summaries().iter().any(|s| s.starts_with("1 rows")),
        "{:?}",
        h.state().console_summaries()
    );
}

/// The users and roles tab of a PostgreSQL server (needs `DBMAN_TEST_POSTGRES`):
/// the explorer offers it to a superuser, a role is created from the editor,
/// gets a privilege and is dropped again.
#[test]
fn users_and_roles() {
    let Some(url) = std::env::var("DBMAN_TEST_POSTGRES").ok().filter(|u| !u.is_empty()) else {
        eprintln!("DBMAN_TEST_POSTGRES not set, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, addr) = rest.rsplit_once('@').unwrap();
    let (user, password) = auth.split_once(':').unwrap();
    let (hostport, db) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::Postgres);
    cfg.name = "local".into();
    cfg.host = host.into();
    cfg.port = port.parse().unwrap();
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.database = db.into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    let conn_id = cfg.id;
    let settings = Settings { connections: vec![cfg.clone()], ..Default::default() };

    // A role to look at, with a privilege, made beforehand on a runtime of its own.
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let (readers, made) = (format!("readers_{tag}"), format!("auditors_{tag}"));
    let other = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let setup = other.block_on({
        let (cfg, readers) = (cfg.clone(), readers.clone());
        async move {
            let c = dbman_drivers::connect(&cfg).await?;
            for sql in [
                "CREATE TABLE IF NOT EXISTS public.dbman_ui_admin (id int)".to_string(),
                format!("CREATE ROLE {readers}"),
                format!("GRANT SELECT ON public.dbman_ui_admin TO {readers}"),
                format!("GRANT {readers} TO CURRENT_USER"),
            ] {
                c.execute(None, &sql, 0).await?;
            }
            Ok::<_, dbman_core::DbError>(c)
        }
    });
    let setup = setup.unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 760.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &conn_id.to_string());
    for _ in 0..20 {
        if h.state().conn(conn_id).is_some_and(|c| c.admin) {
            break;
        }
        settle(&mut h);
    }
    assert!(h.state().conn(conn_id).is_some_and(|c| c.admin), "a superuser may manage roles");
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenAdmin(conn_id), &ctx);
    settle(&mut h);
    assert_eq!(h.state().tab_titles(), ["Users and roles"]);
    let listed = |h: &mut Harness<'_, DbManApp>, name: &str| {
        h.state_mut().active_admin().unwrap().principals().iter().any(|p| p.name == name)
    };
    for _ in 0..20 {
        if listed(&mut h, &readers) {
            break;
        }
        settle(&mut h);
    }
    assert!(listed(&mut h, &readers), "the role is listed");
    h.state_mut().active_admin().unwrap().pick(&readers);
    for _ in 0..20 {
        if !h.state_mut().active_admin().unwrap().grants().is_empty() {
            break;
        }
        settle(&mut h);
    }
    let grants = h.state_mut().active_admin().unwrap().grants().to_vec();
    assert!(grants.iter().any(|g| g.privilege == "SELECT" && g.object.ends_with("dbman_ui_admin")), "{grants:?}");
    assert!(h.query_by_label("Grant privileges…").is_some());
    snapshot(&mut h, "users_and_roles");
    // INSERT on the table, picked from the loaded list.
    click(&mut h, "Grant privileges…");
    assert!(h.query_by_label("With grant option (may grant it to others)").is_some());
    click_last(&mut h, "Select…");
    click_last(&mut h, "dbman_ui_admin");
    click_last(&mut h, "INSERT");
    snapshot(&mut h, "grant_privileges");
    click_last(&mut h, "Grant");
    for _ in 0..20 {
        if h.state_mut().active_admin().unwrap().grants().iter().any(|g| g.privilege == "INSERT") {
            break;
        }
        settle(&mut h);
    }
    let grants = h.state_mut().active_admin().unwrap().grants().to_vec();
    assert!(grants.iter().any(|g| g.privilege == "INSERT" && g.object.ends_with("dbman_ui_admin")), "{grants:?}");

    // A new role from the editor; the name field has the focus.
    click(&mut h, "New role…");
    h.event(egui::Event::Text(made.clone()));
    settle(&mut h);
    assert!(h.query_by_label("Create").is_some());
    click_last(&mut h, "Create");
    for _ in 0..20 {
        if listed(&mut h, &made) {
            break;
        }
        settle(&mut h);
    }
    assert!(listed(&mut h, &made), "the new role is listed");

    // And dropped again, after a confirmation.
    h.state_mut().active_admin().unwrap().pick(&made);
    settle(&mut h);
    click(&mut h, "Drop…");
    click_last(&mut h, "Drop");
    for _ in 0..20 {
        if !listed(&mut h, &made) {
            break;
        }
        settle(&mut h);
    }
    assert!(!listed(&mut h, &made), "the role is dropped");

    other.block_on(async {
        let _ = setup.execute(None, &format!("REVOKE {readers} FROM CURRENT_USER"), 0).await;
        let _ = setup.execute(None, &format!("DROP OWNED BY {readers}"), 0).await;
        let _ = setup.execute(None, &format!("DROP ROLE {readers}"), 0).await;
    });
}

#[test]
fn folder_of_files() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let root = dir.path().join("sales");
    std::fs::create_dir_all(root.join("archive")).unwrap();
    std::fs::write(root.join("orders.csv"), "id,customer,total\n1,Ada,9.5\n2,Linus,20\n3,Grace,7.25\n").unwrap();
    std::fs::write(root.join("customers.json"), r#"[{"name": "Ada", "city": "London"}]"#).unwrap();
    std::fs::write(root.join("archive").join("orders_2023.tsv"), "id\ttotal\n7\t1.5\n").unwrap();
    let mut cfg = ConnectionConfig::new(DbKind::DuckDb);
    cfg.name = "sales files".into();
    cfg.database = root.to_string_lossy().into_owned();
    let id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 640.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);

    // The folder is a database, a subfolder a schema, and every file a view.
    click(&mut h, &id.to_string());
    click(&mut h, "sales");
    assert!(h.query_all_by_label("archive").next().is_some());
    click(&mut h, "main");
    click(&mut h, "views");
    assert!(h.query_all_by_label("customers").next().is_some());
    let orders = DbObject {
        kind: ObjectKind::View,
        path: ObjectPath::new("sales", "main", "orders"),
        detail: None,
        comment: None,
    };
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenData(id, orders), &ctx);
    settle(&mut h);
    assert!(h.query_all_by_label("Grace").next().is_some(), "the CSV rows are shown");
    snapshot(&mut h, "folder_of_files");
}

#[test]
fn search_in_database() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    cfg.database = sample_db(dir.path());
    let id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 640.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &id.to_string());
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenSearch(id, "main".into(), None), &ctx);
    settle(&mut h);
    assert_eq!(h.state().tab_titles(), ["Search in main"]);

    // Names and definitions: the tables, the view and the trigger that mention orders.
    h.state_mut().active_search().unwrap().text = "orders".into();
    click(&mut h, "Search");
    for _ in 0..20 {
        if !h.state_mut().active_search().unwrap().is_running() {
            break;
        }
        settle(&mut h);
    }
    let found = |h: &mut Harness<'_, DbManApp>| -> Vec<String> {
        let mut v: Vec<String> = h
            .state_mut()
            .active_search()
            .unwrap()
            .hits
            .iter()
            .map(|x| format!("{} {}", x.kind.label(), x.object.path.name))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        found(&mut h),
        ["Definition big_orders", "Definition orders_touch", "Name big_orders", "Name orders", "Name orders_touch"]
    );
    snapshot(&mut h, "search");

    // Data: the customer in Berlin, opened as the table filtered to that row.
    {
        let s = h.state_mut().active_search().unwrap();
        s.text = "berlin".into();
        s.data = true;
    }
    click(&mut h, "Search");
    for _ in 0..20 {
        if !h.state_mut().active_search().unwrap().is_running() {
            break;
        }
        settle(&mut h);
    }
    assert_eq!(found(&mut h), ["Data customers"]);
    let hit = h.state_mut().active_search().unwrap().hits[0].clone();
    assert_eq!(hit.detail, "Matching rows: 1");
    h.state_mut().handle(Action::OpenDataFiltered(id, hit.object, hit.filter.unwrap()), &ctx);
    settle(&mut h);
    assert_eq!(h.state().tab_titles(), ["Search in main", "customers"]);
    assert!(h.query_all_by_label("Alice").next().is_some());
    assert!(h.query_all_by_label("Bob").next().is_none(), "only the matching row");
}

#[test]
fn find_replace_and_snippets_in_console() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    cfg.database = sample_db(dir.path());
    let id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 640.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &id.to_string());
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenConsole(id, None), &ctx);
    settle(&mut h);
    let editor = |h: &mut Harness<'_, DbManApp>| {
        h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
        h.run_steps(2);
    };

    // A snippet at the start of a statement: `sel` becomes `SELECT * FROM ` with the cursor at its end.
    editor(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("sel");
    for _ in 0..10 {
        settle(&mut h);
        if !h.state().console_completion().0.is_empty() {
            break;
        }
    }
    assert_eq!(h.state().console_completion().0.first().map(String::as_str), Some("sel"));
    h.key_press(Key::Enter);
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text("orders WHERE total > 10;");
    settle(&mut h);
    assert_eq!(h.state().console_completion().1, "SELECT * FROM orders WHERE total > 10;");
    h.get_by_role(egui::accesskit::Role::MultilineTextInput)
        .type_text("\nSELECT count(*) FROM Orders o JOIN order_lines l ON l.order_id = o.id;");
    settle(&mut h);

    // Ctrl+F opens the bar with the focus in its input; Enter steps through the matches.
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    settle(&mut h);
    h.event(egui::Event::Text("orders".into()));
    settle(&mut h);
    let find = |h: &mut Harness<'_, DbManApp>| {
        let c = h.state_mut().active_console_mut().unwrap();
        (c.find.matches(&c.sql).len(), c.find.current)
    };
    assert_eq!(find(&mut h), (2, Some(0)));
    assert!(h.query_by_label("1 of 2").is_some());
    h.key_press(Key::Enter);
    settle(&mut h);
    assert_eq!(find(&mut h), (2, Some(1)));
    h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
    settle(&mut h);
    assert_eq!(find(&mut h), (2, Some(0)));

    // Whole words skips `order_lines`; match case only the capitalised one.
    h.state_mut().active_console_mut().unwrap().find.query.whole_word = true;
    h.state_mut().active_console_mut().unwrap().find.query.text = "order".into();
    settle(&mut h);
    assert_eq!(find(&mut h).0, 0);
    assert!(h.query_by_label("No matches").is_some());
    h.state_mut().active_console_mut().unwrap().find.query = Default::default();
    h.state_mut().active_console_mut().unwrap().find.query.text = "Orders".into();
    h.state_mut().active_console_mut().unwrap().find.query.match_case = true;
    settle(&mut h);
    assert_eq!(find(&mut h).0, 1);

    // Replace all, then Escape closes the bar.
    h.state_mut().active_console_mut().unwrap().find.query.match_case = false;
    h.key_press_modifiers(Modifiers::COMMAND, Key::R);
    settle(&mut h);
    h.state_mut().active_console_mut().unwrap().find.replacement = "orders_archive".into();
    settle(&mut h);
    snapshot(&mut h, "find_replace");
    click(&mut h, "Replace all");
    let sql = h.state().console_completion().1;
    assert_eq!(
        sql,
        "SELECT * FROM orders_archive WHERE total > 10;\nSELECT count(*) FROM orders_archive o JOIN order_lines l ON l.order_id = o.id;"
    );
    editor(&mut h);
    h.key_press(Key::Escape);
    settle(&mut h);
    assert!(!h.state_mut().active_console_mut().unwrap().find.open);

    // Escape in the find input closes the bar too.
    editor(&mut h);
    h.key_press_modifiers(Modifiers::COMMAND, Key::F);
    settle(&mut h);
    assert!(h.state_mut().active_console_mut().unwrap().find.open);
    h.key_press(Key::Escape);
    settle(&mut h);
    assert!(!h.state_mut().active_console_mut().unwrap().find.open);
}

#[test]
fn chart_of_a_result() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    cfg.database = sample_db(dir.path());
    let id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 1000.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &id.to_string());
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenConsole(id, None), &ctx);
    settle(&mut h);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
    h.run_steps(2);
    h.get_by_role(egui::accesskit::Role::MultilineTextInput).type_text(
        "SELECT c.name, count(o.id) AS orders, sum(o.total) AS total\nFROM customers c LEFT JOIN orders o ON o.customer_id = c.id\nGROUP BY c.id ORDER BY total DESC;",
    );
    h.run_steps(2);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Enter);
    settle(&mut h);
    assert!(h.state_mut().active_console_mut().unwrap().chart_mut().is_none());

    // The names on the X axis, both number columns plotted.
    click(&mut h, "Chart");
    let spec = h.state_mut().active_console_mut().unwrap().chart_mut().cloned().unwrap();
    assert_eq!((spec.kind, spec.x, spec.ys.clone()), (dbman_core::chart::ChartKind::Bar, 0, vec![1, 2]));
    // Only the totals.
    click(&mut h, "orders");
    assert_eq!(h.state_mut().active_console_mut().unwrap().chart_mut().unwrap().ys, [2]);
    h.remove_cursor();
    settle(&mut h);
    snapshot(&mut h, "chart");

    h.state_mut().active_console_mut().unwrap().chart_mut().unwrap().kind = dbman_core::chart::ChartKind::Pie;
    settle(&mut h);
    snapshot(&mut h, "chart_pie");
    h.state_mut().active_console_mut().unwrap().chart_mut().unwrap().kind = dbman_core::chart::ChartKind::Line;
    settle(&mut h);

    // Back to the table.
    click(&mut h, "Table");
    assert!(h.state_mut().active_console_mut().unwrap().chart_mut().is_none());
}

#[test]
fn paste_from_a_spreadsheet_and_record_view() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    let db_path = sample_db(dir.path());
    cfg.database = db_path.clone();
    let id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 700.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &id.to_string());
    let customers = DbObject {
        kind: ObjectKind::Table,
        path: ObjectPath::new("main", "main", "customers"),
        detail: None,
        comment: None,
    };
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenData(id, customers), &ctx);
    settle(&mut h);

    // Copied cells with no clicked cell become new rows; an empty id keeps its default.
    h.event(egui::Event::Paste("\tEva\teva@example.com\tOslo\r\n\tFrank\tNULL\t\"Rome\"\r\n".into()));
    settle(&mut h);
    assert_eq!(h.state_mut().active_data_view().unwrap().statements().len(), 2);
    // From the clicked cell (Alice's name) to the right: name and email of the first row.
    click(&mut h, "Alice");
    h.event(egui::Event::Paste("Alicia\talicia@example.com".into()));
    settle(&mut h);
    let stmts = h.state_mut().active_data_view().unwrap().statements();
    assert_eq!(stmts.len(), 3, "{stmts:?}");
    click(&mut h, "Submit (3)");
    let check = rusqlite::Connection::open(&db_path).unwrap();
    let rows: Vec<(String, Option<String>, Option<String>)> = check
        .prepare("SELECT name, email, city FROM customers ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows[0], ("Alicia".into(), Some("alicia@example.com".into()), Some("Berlin".into())));
    assert_eq!(rows[4], ("Eva".into(), Some("eva@example.com".into()), Some("Oslo".into())));
    assert_eq!(rows[5], ("Frank".into(), None, Some("Rome".into())));

    // The record view: one row down the screen, the arrows step through the rows.
    click(&mut h, "Bob");
    click(&mut h, "Record");
    assert!(h.state_mut().active_data_view().unwrap().record);
    assert_eq!(h.state_mut().active_data_view().unwrap().record_row(), 1);
    assert!(h.query_by_label("Row 2 of 6").is_some());
    h.remove_cursor();
    settle(&mut h);
    snapshot(&mut h, "record_view");
    // The last one: the first steps through pages.
    click_last(&mut h, "›");
    assert_eq!(h.state_mut().active_data_view().unwrap().record_row(), 2);
    assert!(h.query_by_label("carol@example.com").is_some());
    click(&mut h, "Record");
    assert!(!h.state_mut().active_data_view().unwrap().record);
}

/// The keys tab of a Redis database (needs `DBMAN_TEST_REDIS`): keys grouped
/// by prefix, a hash's fields, a string edited and saved, and a key deleted.
#[test]
fn redis_keys() {
    let Some(url) = std::env::var("DBMAN_TEST_REDIS").ok().filter(|u| !u.is_empty()) else {
        eprintln!("DBMAN_TEST_REDIS not set, skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, addr) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = addr.trim_end_matches('/').split_once(':').unwrap_or((addr, "6379"));
    let mut cfg = ConnectionConfig::new(DbKind::Redis);
    cfg.name = "cache".into();
    cfg.host = host.into();
    cfg.port = port.parse().unwrap();
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    let id = cfg.id;

    // The test's own connection, on a runtime of its own.
    let other = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let c = other.block_on(dbman_drivers::connect(&cfg)).unwrap();
    let run = |line: &str| other.block_on(c.execute(Some("db8"), line, 100)).unwrap();
    for line in [
        "FLUSHDB",
        "SET greeting \"Hello, world\"",
        "HSET user:1 name Alice city Berlin",
        "HSET user:2 name Bob city Paris",
        "RPUSH jobs:queue send-mail resize-image",
        "ZADD jobs:done 3 report 7 backup",
        "SET session:9f2c token EX 3600",
    ] {
        run(line);
    }

    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 640.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &id.to_string());
    assert!(h.query_all_by_label("db8").next().is_some(), "the databases are listed");
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenKeys(id, "db8".into()), &ctx);
    let wait = |h: &mut Harness<'_, DbManApp>| {
        for _ in 0..20 {
            settle(h);
            if !h.state_mut().active_keys().unwrap().is_busy() {
                break;
            }
        }
    };
    wait(&mut h);
    assert_eq!(h.state().tab_titles(), ["Keys in db8"]);
    assert_eq!(h.state_mut().active_keys().unwrap().keys.len(), 6);

    // A group of keys opens to its keys; a hash shows its fields.
    click(&mut h, "user");
    click(&mut h, "1");
    wait(&mut h);
    let key = h.state_mut().active_keys().unwrap().key.clone().unwrap();
    assert_eq!((key.key.as_str(), key.kind.as_str(), key.len), ("user:1", "hash", Some(2)));
    let dbman_drivers::redis::KeyValue::Rows { rows, .. } = &key.value else { panic!("{:?}", key.value) };
    assert!(rows.contains(&vec!["city".to_string(), "Berlin".to_string()]), "{rows:?}");
    click(&mut h, "jobs");
    snapshot(&mut h, "redis_keys");

    // A string is edited and saved.
    click(&mut h, "greeting");
    wait(&mut h);
    assert_eq!(h.state_mut().active_keys().unwrap().edit, "Hello, world");
    h.state_mut().active_keys().unwrap().edit = "Hello, Redis".into();
    click(&mut h, "Save");
    wait(&mut h);
    let dbman_core::StatementOutcome::Rows(r) = run("GET greeting") else { panic!() };
    assert_eq!(r.rows, [[Cell::Text("Hello, Redis".into())]]);

    // Deleting asks first, then drops the key from the list.
    click(&mut h, "Delete key");
    press_and_release(&mut h, "Delete");
    wait(&mut h);
    let keys = &h.state_mut().active_keys().unwrap().keys;
    assert_eq!(keys.len(), 5);
    assert!(!keys.contains(&"greeting".to_string()));
}

/// The profile of a column: counts, range, the most frequent values and a
/// histogram, over the rows the WHERE filter selects.
#[test]
fn column_profile() {
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("DBMAN_CONFIG", dir.path().join("settings.json")) };
    unsafe { std::env::set_var("DBMAN_KEYCHAIN", "off") };
    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
    cfg.name = "shop".into();
    let db_path = sample_db(dir.path());
    // 200 more orders with the even totals from 0 to 398, in a shuffled order.
    let c = rusqlite::Connection::open(&db_path).unwrap();
    for i in 0..200 {
        let total = (i * 37 % 200) * 2;
        c.execute("INSERT INTO orders (customer_id, total) VALUES (?1, ?2)", (i % 4 + 1, total)).unwrap();
    }
    drop(c);
    cfg.database = db_path;
    let id = cfg.id;
    let settings = Settings { connections: vec![cfg], dark: true, page_size: 500, ..Default::default() };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let ctx = egui::Context::default();
    let app = DbManApp::with_settings(&ctx, runtime, settings);
    let mut h = Harness::builder()
        .with_size(egui::vec2(1400.0, 800.0))
        .build_ui_state(|ui, app: &mut DbManApp| app.ui(ui), app);
    settle(&mut h);
    click(&mut h, &id.to_string());
    let table = |name: &str| DbObject {
        kind: ObjectKind::Table,
        path: ObjectPath::new("main", "main", name),
        detail: None,
        comment: None,
    };
    let ctx = h.ctx.clone();
    h.state_mut().handle(Action::OpenData(id, table("customers")), &ctx);
    settle(&mut h);

    // A text column: one NULL, every value distinct, no histogram.
    click(&mut h, "Berlin");
    press_and_release(&mut h, "Profile");
    settle(&mut h);
    let p = h.state_mut().active_data_view().unwrap().profile.as_ref().unwrap().result.ready().cloned().unwrap();
    assert_eq!((p.rows, p.non_null, p.distinct), (4, 3, Some(3)));
    assert_eq!((p.min.as_deref(), p.max.as_deref()), (Some("Berlin"), Some("Moscow")));
    assert_eq!(p.top.len(), 4);
    assert!(p.histogram.is_empty() && p.avg.is_none());
    click_last(&mut h, "Close");
    assert!(h.state_mut().active_data_view().unwrap().profile.is_none());

    // A numeric column: average and histogram over all 204 rows.
    h.state_mut().handle(Action::OpenData(id, table("orders")), &ctx);
    settle(&mut h);
    click(&mut h, "120.5");
    press_and_release(&mut h, "Profile");
    settle(&mut h);
    let p = match &h.state_mut().active_data_view().unwrap().profile.as_ref().unwrap().result {
        dbman::explorer::Load::Ready(p) => p.clone(),
        dbman::explorer::Load::Failed(e) => panic!("{e}"),
        _ => panic!("still loading"),
    };
    assert_eq!((p.rows, p.non_null), (204, 204));
    assert_eq!(p.max.as_deref(), Some("398"));
    assert_eq!(p.histogram.len(), dbman_core::profile::BUCKETS);
    assert_eq!(p.histogram.iter().map(|b| b.2).sum::<i64>(), 204);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    h.remove_cursor();
    settle(&mut h);
    snapshot(&mut h, "column_profile");
    click_last(&mut h, "Close");

    // The WHERE filter narrows the rows profiled.
    h.state_mut().active_data_view().unwrap().set_filter("total > 300".into());
    click(&mut h, "⟳ Refresh");
    click(&mut h, "310");
    press_and_release(&mut h, "Profile");
    settle(&mut h);
    let p = h.state_mut().active_data_view().unwrap().profile.as_ref().unwrap().result.ready().cloned().unwrap();
    assert_eq!(p.rows, 1 + 49);
}
