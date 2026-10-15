use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The database engines DBMan knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DbKind {
    Postgres,
    MySql,
    Sqlite,
}

impl DbKind {
    pub const ALL: [DbKind; 3] = [DbKind::Postgres, DbKind::MySql, DbKind::Sqlite];

    pub const fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::MySql => "MySQL / MariaDB",
            DbKind::Sqlite => "SQLite",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Postgres => Some(5432),
            DbKind::MySql => Some(3306),
            DbKind::Sqlite => None,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "root",
            DbKind::Sqlite => "",
        }
    }

    /// True for engines that open a file instead of talking to a server.
    pub fn is_file_based(self) -> bool {
        matches!(self, DbKind::Sqlite)
    }
}

/// How to use TLS for server connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SslMode {
    /// Plain TCP.
    Disable,
    /// TLS if the server offers it, without certificate checks.
    #[default]
    Prefer,
    /// TLS required, without certificate checks (like libpq's `require`).
    Require,
    /// TLS required and the certificate and host name are verified.
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyFull];

    pub const fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

/// A saved connection ("data source" in DataGrip terms).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub kind: DbKind,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// Stored only when `save_password` is set.
    #[serde(default)]
    pub password: String,
    #[serde(default = "yes")]
    pub save_password: bool,
    /// Initial database; for SQLite, the path of the database file.
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Show only `database` in the explorer instead of every database on the server.
    #[serde(default)]
    pub only_default_database: bool,
}

fn yes() -> bool {
    true
}

impl ConnectionConfig {
    pub fn new(kind: DbKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            kind,
            host: if kind.is_file_based() { String::new() } else { "localhost".into() },
            port: kind.default_port().unwrap_or(0),
            user: kind.default_user().into(),
            password: String::new(),
            save_password: true,
            database: String::new(),
            ssl_mode: SslMode::default(),
            only_default_database: false,
        }
    }

    /// The name shown in the explorer: the given name, or one built from the address.
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.clone();
        }
        if self.kind.is_file_based() {
            return std::path::Path::new(&self.database)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "SQLite".into());
        }
        let db = if self.database.is_empty() { String::new() } else { format!("/{}", self.database) };
        format!("{}@{}{}", self.user, self.host, db)
    }

    /// Switches the engine, resetting the port and user if they were the old defaults.
    pub fn set_kind(&mut self, kind: DbKind) {
        if self.kind == kind {
            return;
        }
        if self.port == self.kind.default_port().unwrap_or(0) {
            self.port = kind.default_port().unwrap_or(0);
        }
        if self.user == self.kind.default_user() {
            self.user = kind.default_user().into();
        }
        if !kind.is_file_based() && self.host.is_empty() {
            self.host = "localhost".into();
        }
        self.kind = kind;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_kind_moves_defaults() {
        let mut c = ConnectionConfig::new(DbKind::Postgres);
        c.set_kind(DbKind::MySql);
        assert_eq!(c.port, 3306);
        assert_eq!(c.user, "root");
        c.user = "app".into();
        c.set_kind(DbKind::Postgres);
        assert_eq!(c.port, 5432);
        assert_eq!(c.user, "app");
    }

    #[test]
    fn roundtrips_through_json() {
        let c = ConnectionConfig::new(DbKind::Sqlite);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<ConnectionConfig>(&json).unwrap(), c);
    }
}
