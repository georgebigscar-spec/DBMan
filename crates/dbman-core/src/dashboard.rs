//! The server dashboard: what to ask each engine for its connections,
//! throughput, long-running queries and size, how to read the answers, and
//! the largest tables of the current database.

use std::collections::HashMap;

use crate::{Cell, DbKind, Product, QueryResult};

/// A query counts as long-running after this many seconds.
pub const SLOW_SECONDS: u32 = 5;

/// One reading of the server. Counters (`total_queries`) only ever grow; the
/// dashboard turns two readings into a rate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub connections: Option<f64>,
    /// Sessions running a statement right now.
    pub active: Option<f64>,
    /// The engine's counter of transactions, statements or commands.
    pub total_queries: Option<f64>,
    /// Statements running longer than [`SLOW_SECONDS`].
    pub slow: Option<f64>,
    /// Bytes of the current database (memory in use for Redis).
    pub size: Option<f64>,
}

/// Whether the dashboard can read this server.
pub fn supported(kind: DbKind, product: Product) -> bool {
    !queries(kind, product).is_empty()
}

/// What the throughput counter counts, for the chart's title.
pub fn throughput_label(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "Transactions per second",
        DbKind::MsSql => "Batches per second",
        DbKind::Redis => "Commands per second",
        DbKind::MongoDb => "Operations per second",
        _ => "Queries per second",
    }
}

/// The statements of one reading. Each answers with one row of named
/// columns, or with name/value rows (`SHOW GLOBAL STATUS`, Redis `INFO`).
pub fn queries(kind: DbKind, product: Product) -> Vec<String> {
    let s = SLOW_SECONDS;
    let q: Vec<String> = match kind {
        // CockroachDB has no pg_stat_database.
        DbKind::Postgres if product == Product::CockroachDb => vec![],
        DbKind::Postgres => vec![format!(
            "SELECT \
               (SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend') AS connections, \
               (SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend' AND state = 'active' \
                  AND pid <> pg_backend_pid()) AS active, \
               (SELECT sum(xact_commit + xact_rollback) FROM pg_stat_database) AS queries, \
               (SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend' AND state = 'active' \
                  AND pid <> pg_backend_pid() AND now() - query_start > interval '{s} seconds') AS slow, \
               pg_database_size(current_database()) AS size"
        )],
        DbKind::MySql => vec![
            "SHOW GLOBAL STATUS WHERE Variable_name IN ('Threads_connected', 'Threads_running', 'Questions')".into(),
            format!(
                "SELECT (SELECT COUNT(*) FROM information_schema.PROCESSLIST \
                   WHERE COMMAND NOT IN ('Sleep', 'Daemon', 'Binlog Dump') AND TIME > {s}) AS slow, \
                 (SELECT SUM(data_length + index_length) FROM information_schema.TABLES \
                   WHERE table_schema = DATABASE()) AS size"
            ),
        ],
        DbKind::MsSql => vec![format!(
            "SELECT \
               (SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE is_user_process = 1) AS connections, \
               (SELECT COUNT(*) FROM sys.dm_exec_requests r JOIN sys.dm_exec_sessions x ON x.session_id = r.session_id \
                  WHERE x.is_user_process = 1 AND r.session_id <> @@SPID) AS active, \
               (SELECT SUM(cntr_value) FROM sys.dm_os_performance_counters \
                  WHERE counter_name = 'Batch Requests/sec') AS queries, \
               (SELECT COUNT(*) FROM sys.dm_exec_requests r JOIN sys.dm_exec_sessions x ON x.session_id = r.session_id \
                  WHERE x.is_user_process = 1 AND r.session_id <> @@SPID AND r.total_elapsed_time > {}) AS slow, \
               (SELECT SUM(CAST(size AS bigint)) * 8192 FROM sys.database_files) AS size",
            s * 1000
        )],
        DbKind::ClickHouse => vec![format!(
            "SELECT \
               (SELECT sum(value) FROM system.metrics WHERE metric IN ('TCPConnection', 'HTTPConnection', \
                  'MySQLConnection', 'PostgreSQLConnection')) AS connections, \
               (SELECT count() FROM system.processes WHERE query_id <> queryID()) AS active, \
               (SELECT sum(value) FROM system.events WHERE event = 'Query') AS queries, \
               (SELECT count() FROM system.processes WHERE query_id <> queryID() AND elapsed > {s}) AS slow, \
               (SELECT sum(bytes_on_disk) FROM system.parts WHERE active AND database = currentDatabase()) AS size"
        )],
        DbKind::Redis => vec!["INFO".into()],
        DbKind::MongoDb => vec![
            "db.adminCommand({serverStatus: 1})".into(),
            format!("db.adminCommand({{currentOp: 1, active: true, secs_running: {{$gte: {s}}}}})"),
            "db.stats()".into(),
        ],
        DbKind::Sqlite | DbKind::DuckDb => vec![],
    };
    q
}

/// Names in the answers, lowercased, mapped to their values.
fn values(results: &[QueryResult]) -> HashMap<String, f64> {
    let num = |c: &Cell| c.display().trim().parse::<f64>().ok();
    let mut out = HashMap::new();
    for r in results {
        // Redis INFO: one text of `name:value` lines.
        if let [row] = r.rows.as_slice()
            && let [Cell::Text(text)] = row.as_slice()
            && text.contains("\n")
        {
            for line in text.lines() {
                if let Some((k, v)) = line.trim().split_once(':')
                    && let Ok(v) = v.trim().parse::<f64>()
                {
                    out.insert(k.to_lowercase(), v);
                }
            }
            continue;
        }
        let name_value = r.columns.len() == 2
            && r.columns[0].name.to_lowercase().contains("name")
            && r.columns[1].name.to_lowercase().contains("value");
        if name_value {
            for row in &r.rows {
                if let (Some(k), Some(v)) = (row.first(), row.get(1).and_then(num)) {
                    out.insert(k.display().to_lowercase(), v);
                }
            }
        } else if let Some(row) = r.rows.first() {
            for (c, cell) in r.columns.iter().zip(row) {
                if let Some(v) = num(cell) {
                    out.insert(c.name.to_lowercase(), v);
                }
            }
        }
    }
    out
}

/// A sample from the answers to [`queries`].
pub fn read(kind: DbKind, results: &[QueryResult]) -> Sample {
    if kind == DbKind::MongoDb {
        return mongo_sample(results);
    }
    let v = values(results);
    let get = |names: &[&str]| names.iter().find_map(|n| v.get(*n).copied());
    match kind {
        DbKind::MySql => Sample {
            connections: get(&["threads_connected"]),
            // The dashboard's own statement is one of them.
            active: get(&["threads_running"]).map(|n| (n - 1.0).max(0.0)),
            total_queries: get(&["questions"]),
            slow: get(&["slow"]),
            size: get(&["size"]),
        },
        DbKind::Redis => Sample {
            connections: get(&["connected_clients"]),
            active: get(&["blocked_clients"]),
            total_queries: get(&["total_commands_processed"]),
            slow: None,
            size: get(&["used_memory"]),
        },
        _ => Sample {
            connections: get(&["connections"]),
            active: get(&["active"]),
            total_queries: get(&["queries"]),
            slow: get(&["slow"]),
            size: get(&["size"]),
        },
    }
}

/// MongoDB: `serverStatus`, the operations running long and `dbStats`, each
/// one row whose documents are JSON text.
fn mongo_sample(results: &[QueryResult]) -> Sample {
    let field = |r: usize, name: &str| -> Option<serde_json::Value> {
        let r = results.get(r)?;
        let i = r.columns.iter().position(|c| c.name == name)?;
        let text = r.rows.first()?.get(i)?.as_text()?.into_owned();
        Some(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
    };
    let num = |v: Option<&serde_json::Value>| -> Option<f64> {
        let v = v?;
        v.as_f64().or_else(|| v.as_str()?.parse().ok()).or_else(|| v.get("$numberLong")?.as_str()?.parse().ok())
    };
    let conns = field(0, "connections");
    let ops = field(0, "opcounters");
    let total = ops.as_ref().and_then(|o| o.as_object()).map(|o| o.values().filter_map(|v| num(Some(v))).sum());
    let size = [field(2, "storageSize"), field(2, "indexSize")].iter().map(|v| num(v.as_ref())).sum::<Option<f64>>();
    Sample {
        connections: num(conns.as_ref().and_then(|c| c.get("current"))),
        // The dashboard's own command is one of them.
        active: num(conns.as_ref().and_then(|c| c.get("active"))).map(|n| (n - 1.0).max(0.0)),
        total_queries: total,
        slow: field(1, "inprog").map(|p| p.as_array().map_or(0, Vec::len) as f64),
        size,
    }
}

/// The ten largest tables of the current database: name, bytes, rows.
pub fn tables_sql(kind: DbKind, product: Product) -> Option<&'static str> {
    Some(match kind {
        DbKind::Postgres if product == Product::CockroachDb => return None,
        DbKind::Postgres => {
            "SELECT n.nspname || '.' || c.relname AS name, pg_total_relation_size(c.oid) AS bytes, \
               GREATEST(c.reltuples, 0)::bigint AS row_count \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'm', 'p') AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
               AND n.nspname NOT LIKE 'pg_toast%' AND NOT c.relispartition \
             ORDER BY 2 DESC LIMIT 10"
        }
        DbKind::MySql => {
            "SELECT table_name AS name, data_length + index_length AS bytes, table_rows AS row_count \
             FROM information_schema.TABLES WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE' \
             ORDER BY 2 DESC LIMIT 10"
        }
        DbKind::MsSql => {
            "SELECT TOP 10 s.name + '.' + t.name AS name, SUM(p.reserved_page_count) * 8192 AS bytes, \
               SUM(CASE WHEN p.index_id IN (0, 1) THEN p.row_count ELSE 0 END) AS row_count \
             FROM sys.dm_db_partition_stats p JOIN sys.tables t ON t.object_id = p.object_id \
             JOIN sys.schemas s ON s.schema_id = t.schema_id \
             GROUP BY s.name, t.name ORDER BY bytes DESC"
        }
        DbKind::ClickHouse => {
            "SELECT table AS name, sum(bytes_on_disk) AS bytes, sum(rows) AS row_count FROM system.parts \
             WHERE active AND database = currentDatabase() GROUP BY table ORDER BY bytes DESC LIMIT 10"
        }
        DbKind::Redis | DbKind::MongoDb | DbKind::Sqlite | DbKind::DuckDb => return None,
    })
}

/// "512 B", "3.4 MB", "1.2 GB".
pub fn bytes_label(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n.max(0.0);
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{v:.0} {}", UNITS[0]) } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// Per-second rate between two readings of a counter `seconds` apart; none
/// when the counter went back (a restart) or no time passed.
pub fn rate(before: Option<f64>, after: Option<f64>, seconds: f64) -> Option<f64> {
    let (b, a) = (before?, after?);
    (a >= b && seconds > 0.0).then(|| (a - b) / seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnMeta;

    fn result(cols: &[&str], rows: &[&[&str]]) -> QueryResult {
        QueryResult {
            columns: cols.iter().map(|c| ColumnMeta::new(*c, "")).collect(),
            rows: rows.iter().map(|r| r.iter().map(|v| Cell::Text(v.to_string())).collect()).collect(),
            truncated: false,
        }
    }

    #[test]
    fn reads_each_engine() {
        let pg = result(&["connections", "active", "queries", "slow", "size"], &[&["12", "3", "1000", "1", "8192"]]);
        assert_eq!(
            read(DbKind::Postgres, &[pg]),
            Sample {
                connections: Some(12.0),
                active: Some(3.0),
                total_queries: Some(1000.0),
                slow: Some(1.0),
                size: Some(8192.0)
            }
        );
        let status = result(
            &["Variable_name", "Value"],
            &[&["Threads_connected", "7"], &["Threads_running", "2"], &["Questions", "500"]],
        );
        let rest = result(&["slow", "size"], &[&["0", "NULL"]]);
        let my = read(DbKind::MySql, &[status, rest]);
        assert_eq!((my.connections, my.active, my.total_queries, my.size), (Some(7.0), Some(1.0), Some(500.0), None));
        let info = result(
            &["reply"],
            &[&["# Clients\r\nconnected_clients:4\r\nused_memory:1048576\r\ntotal_commands_processed:99\r\n"]],
        );
        let r = read(DbKind::Redis, &[info]);
        assert_eq!((r.connections, r.total_queries, r.size, r.slow), (Some(4.0), Some(99.0), Some(1048576.0), None));
    }

    #[test]
    fn support_and_helpers() {
        assert!(supported(DbKind::Postgres, Product::Native));
        assert!(!supported(DbKind::Postgres, Product::CockroachDb));
        assert!(!supported(DbKind::Sqlite, Product::Native));
        assert_eq!(rate(Some(100.0), Some(160.0), 2.0), Some(30.0));
        assert_eq!(rate(Some(100.0), Some(10.0), 2.0), None);
        assert_eq!(bytes_label(512.0), "512 B");
        assert_eq!(bytes_label(3.5 * 1024.0 * 1024.0), "3.5 MB");
    }
}
