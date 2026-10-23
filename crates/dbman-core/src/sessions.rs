//! Server sessions: the statement that lists who is connected and what they
//! run on each engine, and the statements that cancel a query or end a session.

use crate::sql::Dialect;
use crate::{Cell, DbKind, Product, QueryResult};

/// One client session (or, on ClickHouse, one running query).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    /// What the kill statements take: a process id, or a ClickHouse query id.
    pub id: String,
    pub user: String,
    pub database: String,
    /// Client host and port.
    pub client: String,
    /// The program that connected, when the client tells.
    pub application: String,
    /// `active`, `idle`, `Sleep`, `running`… as the engine reports it.
    pub state: String,
    /// Seconds in the current state or query.
    pub seconds: Option<f64>,
    /// What the session is waiting for, if anything.
    pub wait: String,
    /// The current or last statement.
    pub query: String,
    /// The session that ran the listing itself.
    pub own: bool,
}

impl Session {
    /// Idle sessions have no statement running.
    pub fn is_idle(&self) -> bool {
        let s = self.state.to_ascii_lowercase();
        s.is_empty() || s == "idle" || s == "sleep" || s == "sleeping" || s == "dormant"
    }
}

/// Engines whose sessions can be listed.
pub fn supported(kind: DbKind) -> bool {
    !kind.is_file_based()
}

/// The statement that lists sessions, with the columns `Session` has, in its order.
pub fn list_sql(kind: DbKind, product: Product) -> Option<&'static str> {
    Some(match kind {
        // CockroachDB's pg_stat_activity is empty; its sessions are in crdb_internal.
        DbKind::Postgres if product == Product::CockroachDb => {
            "SELECT session_id, user_name, '', client_address, application_name, status, \
             EXTRACT(EPOCH FROM (now() - COALESCE(active_query_start, session_start)))::float8, '', \
             CASE WHEN active_queries <> '' THEN active_queries ELSE last_active_query END, \
             session_id = (SELECT session_id FROM [SHOW session_id]) \
             FROM crdb_internal.cluster_sessions WHERE status <> 'CLOSED' ORDER BY session_start"
        }
        DbKind::Postgres => {
            "SELECT pid::text, usename, datname, \
             COALESCE(host(client_addr) || ':' || client_port, 'local'), application_name, state, \
             EXTRACT(EPOCH FROM (now() - COALESCE(CASE WHEN state = 'active' THEN query_start ELSE state_change END, backend_start)))::float8, \
             COALESCE(wait_event_type || ': ' || wait_event, ''), query, pid = pg_backend_pid() \
             FROM pg_stat_activity WHERE backend_type = 'client backend' ORDER BY pid"
        }
        DbKind::MySql => {
            "SELECT ID, USER, DB, HOST, '', CASE WHEN STATE IS NULL OR STATE = '' THEN COMMAND \
             ELSE CONCAT(COMMAND, ': ', STATE) END, TIME, '', INFO, ID = CONNECTION_ID() \
             FROM information_schema.PROCESSLIST WHERE COMMAND <> 'Daemon' ORDER BY ID"
        }
        DbKind::MsSql => {
            "SELECT CAST(s.session_id AS nvarchar(10)), s.login_name, DB_NAME(s.database_id), \
             CONCAT(s.host_name, ' ', c.client_net_address), s.program_name, COALESCE(r.status, s.status), \
             DATEDIFF(second, COALESCE(r.start_time, s.last_request_start_time), GETDATE()), \
             COALESCE(r.wait_type, ''), t.text, CASE WHEN s.session_id = @@SPID THEN 1 ELSE 0 END \
             FROM sys.dm_exec_sessions s \
             LEFT JOIN sys.dm_exec_connections c ON c.session_id = s.session_id \
             LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             OUTER APPLY sys.dm_exec_sql_text(COALESCE(r.sql_handle, c.most_recent_sql_handle)) t \
             WHERE s.is_user_process = 1 ORDER BY s.session_id"
        }
        DbKind::ClickHouse => {
            "SELECT query_id, user, current_database, toString(address), client_name, 'running', elapsed, \
             '', query, query_id = queryID() FROM system.processes ORDER BY elapsed DESC"
        }
        DbKind::Sqlite | DbKind::DuckDb => return None,
    })
}

/// Reads the rows of [`list_sql`].
pub fn parse(result: &QueryResult) -> Vec<Session> {
    let text = |c: Option<&Cell>| c.and_then(Cell::as_text).map(|s| s.trim().to_string()).unwrap_or_default();
    result
        .rows
        .iter()
        .map(|r| Session {
            id: text(r.first()),
            user: text(r.get(1)),
            database: text(r.get(2)),
            client: text(r.get(3)),
            application: text(r.get(4)),
            state: text(r.get(5)),
            seconds: text(r.get(6)).parse().ok(),
            wait: text(r.get(7)),
            query: text(r.get(8)),
            own: matches!(text(r.get(9)).to_ascii_lowercase().as_str(), "1" | "t" | "true"),
        })
        .collect()
}

/// Whether the running statement of a session can be stopped while the session stays.
pub fn can_cancel(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::MySql | DbKind::ClickHouse)
}

/// Whether a session can be closed (ClickHouse has queries, not sessions).
pub fn can_terminate(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::MySql | DbKind::MsSql)
}

/// Stops the statement a session runs.
pub fn cancel_sql(kind: DbKind, product: Product, id: &str) -> Option<String> {
    Some(match kind {
        DbKind::Postgres if product == Product::CockroachDb => format!(
            "CANCEL QUERIES (SELECT query_id FROM crdb_internal.cluster_queries WHERE session_id = '{}')",
            hex(id)?
        ),
        DbKind::Postgres => format!("SELECT pg_cancel_backend({})", number(id)?),
        DbKind::MySql => format!("KILL QUERY {}", number(id)?),
        DbKind::ClickHouse => format!("KILL QUERY WHERE query_id = {}", Dialect(kind).string(id)),
        _ => return None,
    })
}

/// Ends a session; its open transaction rolls back.
pub fn terminate_sql(kind: DbKind, product: Product, id: &str) -> Option<String> {
    Some(match kind {
        DbKind::Postgres if product == Product::CockroachDb => format!("CANCEL SESSION '{}'", hex(id)?),
        DbKind::Postgres => format!("SELECT pg_terminate_backend({})", number(id)?),
        DbKind::MySql => format!("KILL CONNECTION {}", number(id)?),
        DbKind::MsSql => format!("KILL {}", number(id)?),
        _ => return None,
    })
}

/// Process ids go into the statement as they are, so they must be numbers.
fn number(id: &str) -> Option<u64> {
    id.trim().parse().ok()
}

/// CockroachDB's session ids are hexadecimal.
fn hex(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id)
}

/// Seconds as `850 ms`, `12 s`, `4 min 05 s` or `3 h 20 min`.
pub fn duration_label(seconds: f64) -> String {
    if seconds < 1.0 {
        return format!("{:.0} ms", seconds.max(0.0) * 1000.0);
    }
    let s = seconds as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min {:02} s", s / 60, s % 60),
        _ => format!("{} h {:02} min", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColumnMeta;

    #[test]
    fn statements_take_numbers_only() {
        assert_eq!(
            cancel_sql(DbKind::Postgres, Product::Native, "42").as_deref(),
            Some("SELECT pg_cancel_backend(42)")
        );
        assert_eq!(terminate_sql(DbKind::MySql, Product::Native, "7").as_deref(), Some("KILL CONNECTION 7"));
        assert_eq!(terminate_sql(DbKind::MsSql, Product::Native, "55").as_deref(), Some("KILL 55"));
        assert_eq!(cancel_sql(DbKind::Postgres, Product::Native, "1; DROP TABLE x"), None);
        assert_eq!(cancel_sql(DbKind::MsSql, Product::Native, "55"), None);
        assert_eq!(terminate_sql(DbKind::ClickHouse, Product::Native, "q"), None);
        assert_eq!(
            cancel_sql(DbKind::ClickHouse, Product::Native, "a'b").as_deref(),
            Some("KILL QUERY WHERE query_id = 'a''b'")
        );
        assert!(list_sql(DbKind::Sqlite, Product::Native).is_none());
    }

    #[test]
    fn parses_rows() {
        let t = |s: &str| Cell::Text(s.into());
        let result = QueryResult {
            columns: (0..10).map(|i| ColumnMeta::new(format!("c{i}"), "text")).collect(),
            rows: vec![vec![
                t("12"),
                t("app"),
                t("shop"),
                t("10.0.0.5:5050"),
                t("psql"),
                t("active"),
                t("3.5"),
                Cell::Null,
                t("SELECT 1"),
                t("t"),
            ]],
            truncated: false,
        };
        let s = &parse(&result)[0];
        assert_eq!(s.id, "12");
        assert_eq!(s.seconds, Some(3.5));
        assert_eq!(s.wait, "");
        assert!(s.own);
        assert!(!s.is_idle());
    }

    #[test]
    fn durations() {
        assert_eq!(duration_label(0.25), "250 ms");
        assert_eq!(duration_label(12.0), "12 s");
        assert_eq!(duration_label(245.0), "4 min 05 s");
        assert_eq!(duration_label(12000.0), "3 h 20 min");
    }
}
