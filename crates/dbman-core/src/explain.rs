//! Execution plans: the statements that ask each engine for the plan of a
//! query, and a tree read back from what it answers.

use serde_json::{Map, Value};

use crate::{Cell, DbKind, QueryResult};

/// One step of a plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanNode {
    /// What the step does: "Seq Scan", "HASH_JOIN", "Clustered Index Seek".
    pub operation: String,
    /// The table, index, conditions and other details, one per entry.
    pub details: Vec<String>,
    /// Rows the planner expects.
    pub rows: Option<f64>,
    /// The planner's cost, in the engine's own units.
    pub cost: Option<f64>,
    /// Rows the step produced (analyzed plans).
    pub actual_rows: Option<f64>,
    /// Milliseconds the step took (analyzed plans).
    pub actual_ms: Option<f64>,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub roots: Vec<PlanNode>,
    /// Totals such as planning and execution time.
    pub totals: Vec<(String, String)>,
    /// The engine's answer as text, for copying.
    pub raw: String,
}

impl Plan {
    /// Every node with its depth, in display order.
    pub fn rows(&self) -> Vec<(usize, &PlanNode)> {
        fn walk<'a>(n: &'a PlanNode, depth: usize, out: &mut Vec<(usize, &'a PlanNode)>) {
            out.push((depth, n));
            for c in &n.children {
                walk(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for r in &self.roots {
            walk(r, 0, &mut out);
        }
        out
    }
}

/// Engines whose plan can be measured by running the statement.
pub fn supports_analyze(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::DuckDb)
}

/// The statements to run, in order; the plan is in the last result set that has rows.
pub fn statements(kind: DbKind, sql: &str, analyze: bool) -> Vec<String> {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    let analyze = analyze && supports_analyze(kind);
    match kind {
        DbKind::Postgres if analyze => vec![format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {sql}")],
        DbKind::Postgres => vec![format!("EXPLAIN (FORMAT JSON) {sql}")],
        DbKind::DuckDb if analyze => vec![format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}")],
        DbKind::DuckDb => vec![format!("EXPLAIN (FORMAT JSON) {sql}")],
        DbKind::MySql => vec![format!("EXPLAIN {sql}")],
        DbKind::Sqlite => vec![format!("EXPLAIN QUERY PLAN {sql}")],
        DbKind::ClickHouse => vec![format!("EXPLAIN indexes = 1 {sql}")],
        // SHOWPLAN must be alone in its batch; the statement is then planned, not run.
        DbKind::MsSql => vec!["SET SHOWPLAN_ALL ON".into(), sql.to_string(), "SET SHOWPLAN_ALL OFF".into()],
    }
}

/// Reads the plan from the result set of [`statements`].
pub fn parse(kind: DbKind, r: &QueryResult) -> Result<Plan, String> {
    let mut plan = match kind {
        DbKind::Postgres => postgres(&json(r)?)?,
        DbKind::DuckDb => duckdb(r)?,
        DbKind::MySql => mysql(r),
        DbKind::Sqlite => sqlite(r),
        DbKind::ClickHouse => indented(&lines(r)),
        DbKind::MsSql => mssql(r),
    };
    if plan.raw.is_empty() {
        plan.raw = raw_text(r);
    }
    // No steps is an answer too: SQLite plans no work for `INSERT … VALUES`.
    Ok(plan)
}

fn text(c: &Cell) -> Option<&str> {
    match c {
        Cell::Text(s) => Some(s),
        _ => None,
    }
}

fn col(r: &QueryResult, name: &str) -> Option<usize> {
    r.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
}

fn cell<'a>(r: &QueryResult, row: &'a [Cell], name: &str) -> Option<&'a str> {
    col(r, name).and_then(|i| row.get(i)).and_then(text).filter(|s| !s.is_empty())
}

fn num(s: Option<&str>) -> Option<f64> {
    s.and_then(|s| s.trim().parse().ok())
}

fn raw_text(r: &QueryResult) -> String {
    let mut out = String::new();
    for row in &r.rows {
        let cells: Vec<String> = row.iter().map(|c| c.display().into_owned()).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
}

fn lines(r: &QueryResult) -> Vec<String> {
    r.rows.iter().filter_map(|row| row.first().and_then(text)).flat_map(|s| s.lines().map(String::from)).collect()
}

fn json(r: &QueryResult) -> Result<Value, String> {
    let s: String = r.rows.iter().filter_map(|row| row.first().and_then(text)).collect();
    serde_json::from_str(&s).map_err(|e| format!("Could not read the plan: {e}"))
}

fn json_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) => Some(a.iter().filter_map(json_str).collect::<Vec<_>>().join(", ")),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn json_num(o: &Map<String, Value>, key: &str) -> Option<f64> {
    match o.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

// --- PostgreSQL ---------------------------------------------------------------

/// Keys of a PostgreSQL plan node shown as details, with their labels.
const PG_DETAILS: &[&str] = &[
    "Join Type",
    "Strategy",
    "Index Name",
    "Index Cond",
    "Recheck Cond",
    "Hash Cond",
    "Merge Cond",
    "Join Filter",
    "Filter",
    "Rows Removed by Filter",
    "Sort Key",
    "Sort Method",
    "Group Key",
    "CTE Name",
    "Subplan Name",
    "Workers Planned",
];

fn postgres(v: &Value) -> Result<Plan, String> {
    let top = v.as_array().and_then(|a| a.first()).and_then(Value::as_object).ok_or("Unexpected plan format.")?;
    let root = top.get("Plan").and_then(Value::as_object).ok_or("Unexpected plan format.")?;
    let mut totals = Vec::new();
    for key in ["Planning Time", "Execution Time"] {
        if let Some(ms) = json_num(top, key) {
            totals.push((key.to_string(), format!("{ms:.3} ms")));
        }
    }
    Ok(Plan { roots: vec![pg_node(root)], totals, raw: serde_json::to_string_pretty(v).unwrap_or_default() })
}

fn pg_node(o: &Map<String, Value>) -> PlanNode {
    let mut operation = o.get("Node Type").and_then(json_str).unwrap_or_default();
    if let Some(rel) = o.get("Relation Name").and_then(json_str) {
        operation.push_str(" on ");
        operation.push_str(&rel);
        if let Some(alias) = o.get("Alias").and_then(json_str).filter(|a| *a != rel) {
            operation.push(' ');
            operation.push_str(&alias);
        }
    }
    let details = PG_DETAILS.iter().filter_map(|k| o.get(*k).and_then(json_str).map(|v| format!("{k}: {v}"))).collect();
    let loops = json_num(o, "Actual Loops").unwrap_or(1.0);
    PlanNode {
        operation,
        details,
        rows: json_num(o, "Plan Rows"),
        cost: json_num(o, "Total Cost"),
        actual_rows: json_num(o, "Actual Rows").map(|r| r * loops),
        actual_ms: json_num(o, "Actual Total Time").map(|t| t * loops),
        children: o
            .get("Plans")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_object).map(pg_node).collect())
            .unwrap_or_default(),
    }
}

// --- DuckDB -----------------------------------------------------------------

fn duckdb(r: &QueryResult) -> Result<Plan, String> {
    let i = col(r, "explain_value").unwrap_or(r.columns.len().saturating_sub(1));
    let s: String = r.rows.iter().filter_map(|row| row.get(i).and_then(text)).collect();
    let v: Value = serde_json::from_str(&s).map_err(|e| format!("Could not read the plan: {e}"))?;
    let mut totals = Vec::new();
    let nodes: Vec<&Value> = match &v {
        // EXPLAIN: a list of operators.
        Value::Array(a) => a.iter().collect(),
        // EXPLAIN ANALYZE: the query's profile; its first operator is EXPLAIN_ANALYZE itself.
        Value::Object(o) => {
            if let Some(s) = json_num(o, "latency") {
                totals.push(("Execution Time".to_string(), format!("{:.3} ms", s * 1000.0)));
            }
            o.get("children").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let mut roots: Vec<PlanNode> = nodes.into_iter().filter_map(Value::as_object).map(duck_node).collect();
    while roots.len() == 1 && roots[0].operation == "EXPLAIN_ANALYZE" {
        roots = std::mem::take(&mut roots[0].children);
    }
    Ok(Plan { roots, totals, raw: serde_json::to_string_pretty(&v).unwrap_or_default() })
}

fn duck_node(o: &Map<String, Value>) -> PlanNode {
    let operation = o.get("operator_name").or_else(|| o.get("name")).and_then(json_str).unwrap_or_default();
    let extra = o.get("extra_info").and_then(Value::as_object);
    let details = extra
        .map(|e| {
            e.iter()
                .filter(|(k, _)| k.as_str() != "Estimated Cardinality")
                .filter_map(|(k, v)| json_str(v).map(|v| format!("{k}: {}", v.trim())))
                .collect()
        })
        .unwrap_or_default();
    PlanNode {
        operation: operation.trim().to_string(),
        details,
        rows: extra.and_then(|e| json_num(e, "Estimated Cardinality")),
        cost: None,
        actual_rows: json_num(o, "operator_cardinality"),
        actual_ms: json_num(o, "operator_timing").map(|s| s * 1000.0),
        children: o
            .get("children")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_object).map(duck_node).collect())
            .unwrap_or_default(),
    }
}

// --- MySQL / MariaDB ----------------------------------------------------------

fn mysql(r: &QueryResult) -> Plan {
    let roots = r
        .rows
        .iter()
        .map(|row| {
            let get = |k: &str| cell(r, row, k).filter(|s| *s != "NULL");
            let access = match get("type") {
                Some("ALL") => "Full scan".to_string(),
                Some("index") => "Index scan".to_string(),
                Some(t) => t.to_string(),
                None => get("select_type").unwrap_or_default().to_string(),
            };
            let operation = match get("table") {
                Some(t) => format!("{access} on {t}"),
                None => access,
            };
            let mut details = Vec::new();
            for (k, label) in [
                ("select_type", "Select"),
                ("key", "Key"),
                ("ref", "Ref"),
                ("possible_keys", "Possible keys"),
                ("filtered", "Filtered %"),
                ("Extra", "Extra"),
            ] {
                if let Some(v) = get(k) {
                    details.push(format!("{label}: {v}"));
                }
            }
            PlanNode { operation, details, rows: num(get("rows")), ..Default::default() }
        })
        .collect();
    Plan { roots, ..Default::default() }
}

// --- SQLite -------------------------------------------------------------------

fn sqlite(r: &QueryResult) -> Plan {
    // (id, parent, step) in the order SQLite lists them; parents come first.
    let steps: Vec<(String, String, PlanNode)> = r
        .rows
        .iter()
        .map(|row| {
            let detail = cell(r, row, "detail").unwrap_or_default();
            let (op, rest) = detail.split_once(' ').unwrap_or((detail, ""));
            let node = PlanNode {
                operation: format!("{op} {}", rest.split(" USING ").next().unwrap_or(rest)).trim().to_string(),
                details: rest.split_once(" USING ").map(|(_, u)| vec![format!("USING {u}")]).unwrap_or_default(),
                ..Default::default()
            };
            (
                cell(r, row, "id").unwrap_or_default().to_string(),
                cell(r, row, "parent").unwrap_or_default().to_string(),
                node,
            )
        })
        .collect();
    Plan { roots: tree(steps, "0"), ..Default::default() }
}

/// Builds a tree from (id, parent id, node) in listing order.
fn tree(steps: Vec<(String, String, PlanNode)>, root: &str) -> Vec<PlanNode> {
    fn children(steps: &[(String, String, PlanNode)], parent: &str, used: &mut [bool]) -> Vec<PlanNode> {
        let mut out = Vec::new();
        for i in 0..steps.len() {
            if !used[i] && steps[i].1 == parent {
                used[i] = true;
                let mut n = steps[i].2.clone();
                n.children = children(steps, &steps[i].0, used);
                out.push(n);
            }
        }
        out
    }
    let mut used = vec![false; steps.len()];
    let mut roots = children(&steps, root, &mut used);
    // Steps whose parent is missing still show, at the top.
    for (i, s) in steps.iter().enumerate() {
        if !used[i] {
            roots.push(s.2.clone());
        }
    }
    roots
}

// --- ClickHouse ---------------------------------------------------------------

/// A plan printed as lines indented by two spaces per level.
fn indented(lines: &[String]) -> Plan {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots = Vec::new();
    fn close(stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>) {
        let (_, n) = stack.pop().expect("not empty");
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(n),
            None => roots.push(n),
        }
    }
    for line in lines {
        let depth = line.len() - line.trim_start().len();
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            close(&mut stack, &mut roots);
        }
        let (operation, details) = match text.split_once(" (") {
            // "Step (details)"; "Key: value" lines stay whole.
            Some((op, rest)) if text.ends_with(')') && !op.contains(':') => {
                (op.to_string(), vec![rest[..rest.len() - 1].to_string()])
            }
            _ => (text.to_string(), Vec::new()),
        };
        stack.push((depth, PlanNode { operation, details, ..Default::default() }));
    }
    while !stack.is_empty() {
        close(&mut stack, &mut roots);
    }
    Plan { roots, raw: lines.join("\n"), ..Default::default() }
}

// --- SQL Server ---------------------------------------------------------------

fn mssql(r: &QueryResult) -> Plan {
    let steps = r
        .rows
        .iter()
        .map(|row| {
            let get = |k: &str| cell(r, row, k);
            let stmt = get("StmtId").unwrap_or("1");
            let plan_row = get("Type") == Some("PLAN_ROW");
            let node = if plan_row {
                PlanNode {
                    operation: get("PhysicalOp").unwrap_or_default().to_string(),
                    details: get("Argument").map(|a| vec![a.to_string()]).unwrap_or_default(),
                    rows: num(get("EstimateRows")),
                    cost: num(get("TotalSubtreeCost")),
                    ..Default::default()
                }
            } else {
                // The statement itself.
                PlanNode {
                    operation: get("Type").unwrap_or("Statement").to_string(),
                    details: get("StmtText").map(|s| vec![s.trim().to_string()]).unwrap_or_default(),
                    rows: num(get("EstimateRows")),
                    cost: num(get("TotalSubtreeCost")),
                    ..Default::default()
                }
            };
            let id = format!("{stmt}:{}", get("NodeId").unwrap_or_default());
            let parent = format!("{stmt}:{}", get("Parent").unwrap_or("0"));
            (id, parent, node)
        })
        .collect::<Vec<_>>();
    // Each statement's root has parent 0.
    let mut roots = Vec::new();
    let stmts: Vec<String> = steps.iter().map(|(id, _, _)| id.split(':').next().unwrap_or("").to_string()).collect();
    let mut seen = Vec::new();
    for s in stmts {
        if !seen.contains(&s) {
            seen.push(s);
        }
    }
    let mut rest = steps;
    for s in seen {
        let (mine, others): (Vec<_>, Vec<_>) =
            rest.into_iter().partition(|(id, _, _)| id.starts_with(&format!("{s}:")));
        roots.extend(tree(mine, &format!("{s}:0")));
        rest = others;
    }
    Plan { roots, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnMeta;

    fn result(cols: &[&str], rows: &[&[&str]]) -> QueryResult {
        QueryResult {
            columns: cols
                .iter()
                .map(|c| ColumnMeta { name: c.to_string(), type_name: String::new(), numeric: false })
                .collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| if *c == "NULL" { Cell::Null } else { Cell::Text(c.to_string()) }).collect())
                .collect(),
            truncated: false,
        }
    }

    fn outline(p: &Plan) -> Vec<String> {
        p.rows().into_iter().map(|(d, n)| format!("{}{}", "  ".repeat(d), n.operation)).collect()
    }

    #[test]
    fn postgres_analyzed() {
        let json = r#"[{"Plan": {"Node Type": "Nested Loop", "Join Type": "Inner", "Total Cost": 50.9, "Plan Rows": 6,
            "Actual Total Time": 0.5, "Actual Rows": 2, "Actual Loops": 1,
            "Plans": [{"Node Type": "Seq Scan", "Relation Name": "t", "Alias": "a", "Total Cost": 25.88, "Plan Rows": 6,
              "Filter": "(v = 'x'::text)", "Actual Total Time": 0.1, "Actual Rows": 2, "Actual Loops": 1},
              {"Node Type": "Index Scan", "Relation Name": "t", "Alias": "b", "Index Name": "t_pkey",
               "Index Cond": "(id = a.id)", "Total Cost": 4.17, "Plan Rows": 1, "Actual Total Time": 0.01,
               "Actual Rows": 1, "Actual Loops": 2}]}, "Planning Time": 0.2, "Execution Time": 0.7}]"#;
        let p = parse(DbKind::Postgres, &result(&["QUERY PLAN"], &[&[json]])).unwrap();
        assert_eq!(outline(&p), ["Nested Loop", "  Seq Scan on t a", "  Index Scan on t b"]);
        let scan = &p.roots[0].children[1];
        assert_eq!(scan.details, ["Index Name: t_pkey", "Index Cond: (id = a.id)"]);
        assert_eq!((scan.actual_rows, scan.actual_ms), (Some(2.0), Some(0.02)));
        assert_eq!(p.totals[1], ("Execution Time".to_string(), "0.700 ms".to_string()));
    }

    #[test]
    fn duckdb_plain_and_analyzed() {
        let plain = r#"[{"name": "HASH_JOIN", "children": [{"name": "SEQ_SCAN ", "children": [],
            "extra_info": {"Table": "t", "Filters": "v='x'", "Estimated Cardinality": "1"}}],
            "extra_info": {"Join Type": "INNER", "Estimated Cardinality": "1"}}]"#;
        let p =
            parse(DbKind::DuckDb, &result(&["explain_key", "explain_value"], &[&["physical_plan", plain]])).unwrap();
        assert_eq!(outline(&p), ["HASH_JOIN", "  SEQ_SCAN"]);
        assert_eq!(p.roots[0].children[0].details, ["Table: t", "Filters: v='x'"]);
        assert_eq!(p.roots[0].rows, Some(1.0));
        let analyzed = r#"{"latency": 0.005, "children": [{"operator_name": "EXPLAIN_ANALYZE", "children": [
            {"operator_name": "PROJECTION", "operator_timing": 0.001, "operator_cardinality": 3, "children": []}]}]}"#;
        let p =
            parse(DbKind::DuckDb, &result(&["explain_key", "explain_value"], &[&["analyzed_plan", analyzed]])).unwrap();
        assert_eq!(outline(&p), ["PROJECTION"]);
        assert_eq!((p.roots[0].actual_rows, p.roots[0].actual_ms), (Some(3.0), Some(1.0)));
        assert_eq!(p.totals[0].1, "5.000 ms");
    }

    #[test]
    fn sqlite_tree() {
        let r = result(
            &["id", "parent", "notused", "detail"],
            &[
                &["3", "0", "0", "SCAN a"],
                &["5", "0", "0", "SEARCH b USING INTEGER PRIMARY KEY (rowid=?)"],
                &["9", "0", "0", "USE TEMP B-TREE FOR ORDER BY"],
                &["12", "5", "0", "CORRELATED SCALAR SUBQUERY 1"],
            ],
        );
        let p = parse(DbKind::Sqlite, &r).unwrap();
        assert_eq!(
            outline(&p),
            ["SCAN a", "SEARCH b", "  CORRELATED SCALAR SUBQUERY 1", "USE TEMP B-TREE FOR ORDER BY"]
        );
        assert_eq!(p.roots[1].details, ["USING INTEGER PRIMARY KEY (rowid=?)"]);
    }

    #[test]
    fn mysql_rows() {
        let r = result(
            &["id", "select_type", "table", "type", "possible_keys", "key", "key_len", "ref", "rows", "Extra"],
            &[
                &["1", "SIMPLE", "a", "ALL", "PRIMARY", "NULL", "NULL", "NULL", "10", "Using where"],
                &["1", "SIMPLE", "b", "eq_ref", "PRIMARY", "PRIMARY", "4", "db.a.id", "1", ""],
            ],
        );
        let p = parse(DbKind::MySql, &r).unwrap();
        assert_eq!(outline(&p), ["Full scan on a", "eq_ref on b"]);
        assert_eq!(p.roots[0].rows, Some(10.0));
        assert!(p.roots[1].details.contains(&"Key: PRIMARY".to_string()));
    }

    #[test]
    fn clickhouse_indented() {
        let text = "Expression ((Project names + Projection))\n  Expression\n    ReadFromMergeTree (db.t)\n    Indexes:\n      PrimaryKey\n        Condition: (id in [1, 1])\n";
        let p = parse(DbKind::ClickHouse, &result(&["explain"], &[&[text]])).unwrap();
        assert_eq!(
            outline(&p),
            [
                "Expression",
                "  Expression",
                "    ReadFromMergeTree",
                "    Indexes:",
                "      PrimaryKey",
                "        Condition: (id in [1, 1])"
            ]
        );
        assert_eq!(p.roots[0].details, ["(Project names + Projection)"]);
    }

    #[test]
    fn sql_server_showplan() {
        let cols = [
            "StmtText",
            "StmtId",
            "NodeId",
            "Parent",
            "PhysicalOp",
            "Argument",
            "EstimateRows",
            "TotalSubtreeCost",
            "Type",
        ];
        let r = result(
            &cols,
            &[
                &["select * from t", "1", "1", "0", "NULL", "1", "1", "0.0066", "SELECT"],
                &["  |--Nested Loops", "1", "2", "1", "Nested Loops", "Inner Join", "1.0", "0.0066", "PLAN_ROW"],
                &[
                    "  |--Scan",
                    "1",
                    "3",
                    "2",
                    "Clustered Index Scan",
                    "OBJECT:([t] AS [a])",
                    "1.0",
                    "0.0033",
                    "PLAN_ROW",
                ],
            ],
        );
        let p = parse(DbKind::MsSql, &r).unwrap();
        assert_eq!(outline(&p), ["SELECT", "  Nested Loops", "    Clustered Index Scan"]);
        assert_eq!(p.roots[0].details, ["select * from t"]);
        assert_eq!(p.roots[0].children[0].children[0].cost, Some(0.0033));
    }

    #[test]
    fn statements_per_engine() {
        assert_eq!(
            statements(DbKind::Postgres, "select 1;", true),
            ["EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) select 1"]
        );
        assert_eq!(statements(DbKind::MySql, "select 1", true), ["EXPLAIN select 1"]);
        assert_eq!(statements(DbKind::MsSql, "select 1", false).len(), 3);
    }
}
