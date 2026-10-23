use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The database engines DBMan knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DbKind {
    Postgres,
    MySql,
    Sqlite,
    DuckDb,
    MsSql,
    ClickHouse,
}

impl DbKind {
    pub const ALL: [DbKind; 6] =
        [DbKind::Postgres, DbKind::MySql, DbKind::MsSql, DbKind::ClickHouse, DbKind::Sqlite, DbKind::DuckDb];

    pub const fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::MySql => "MySQL / MariaDB",
            DbKind::Sqlite => "SQLite",
            DbKind::DuckDb => "DuckDB",
            DbKind::MsSql => "SQL Server",
            DbKind::ClickHouse => "ClickHouse",
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            DbKind::Postgres => Some(5432),
            DbKind::MySql => Some(3306),
            DbKind::MsSql => Some(1433),
            // The HTTP interface (8443 with TLS).
            DbKind::ClickHouse => Some(8123),
            DbKind::Sqlite | DbKind::DuckDb => None,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "root",
            DbKind::MsSql => "sa",
            DbKind::ClickHouse => "default",
            DbKind::Sqlite | DbKind::DuckDb => "",
        }
    }

    /// True for engines that open a file instead of talking to a server.
    pub fn is_file_based(self) -> bool {
        matches!(self, DbKind::Sqlite | DbKind::DuckDb)
    }
}

/// A product that speaks an engine's protocol: it connects with that engine's
/// driver, and the connection dialog fills in its usual port, user and TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Product {
    /// The engine itself.
    #[default]
    Native,
    CockroachDb,
    YugabyteDb,
    TimescaleDb,
    Supabase,
    Neon,
    TiDb,
    PlanetScale,
    SingleStore,
}

impl Product {
    /// The choices of the connection dialog, each engine followed by the products on its protocol.
    pub const CHOICES: [(DbKind, Product); 14] = [
        (DbKind::Postgres, Product::Native),
        (DbKind::Postgres, Product::CockroachDb),
        (DbKind::Postgres, Product::YugabyteDb),
        (DbKind::Postgres, Product::TimescaleDb),
        (DbKind::Postgres, Product::Supabase),
        (DbKind::Postgres, Product::Neon),
        (DbKind::MySql, Product::Native),
        (DbKind::MySql, Product::TiDb),
        (DbKind::MySql, Product::PlanetScale),
        (DbKind::MySql, Product::SingleStore),
        (DbKind::MsSql, Product::Native),
        (DbKind::ClickHouse, Product::Native),
        (DbKind::Sqlite, Product::Native),
        (DbKind::DuckDb, Product::Native),
    ];

    /// The engine whose driver the product uses; `None` for `Native`.
    pub const fn engine(self) -> Option<DbKind> {
        match self {
            Product::Native => None,
            Product::CockroachDb | Product::YugabyteDb | Product::TimescaleDb | Product::Supabase | Product::Neon => {
                Some(DbKind::Postgres)
            }
            Product::TiDb | Product::PlanetScale | Product::SingleStore => Some(DbKind::MySql),
        }
    }

    pub const fn label(self, kind: DbKind) -> &'static str {
        match self {
            Product::Native => kind.label(),
            Product::CockroachDb => "CockroachDB",
            Product::YugabyteDb => "YugabyteDB",
            Product::TimescaleDb => "TimescaleDB",
            Product::Supabase => "Supabase",
            Product::Neon => "Neon",
            Product::TiDb => "TiDB",
            Product::PlanetScale => "PlanetScale",
            Product::SingleStore => "SingleStore",
        }
    }

    /// The short tag of the explorer's badge.
    pub const fn badge(self, kind: DbKind) -> &'static str {
        match self {
            Product::Native => match kind {
                DbKind::Postgres => "PG",
                DbKind::MySql => "MY",
                DbKind::Sqlite => "SL",
                DbKind::DuckDb => "DK",
                DbKind::MsSql => "MS",
                DbKind::ClickHouse => "CH",
            },
            Product::CockroachDb => "CR",
            Product::YugabyteDb => "YB",
            Product::TimescaleDb => "TS",
            Product::Supabase => "SB",
            Product::Neon => "NE",
            Product::TiDb => "TI",
            Product::PlanetScale => "PS",
            Product::SingleStore => "S2",
        }
    }

    pub fn default_port(self, kind: DbKind) -> Option<u16> {
        match self {
            Product::CockroachDb => Some(26257),
            Product::YugabyteDb => Some(5433),
            Product::TiDb => Some(4000),
            _ => kind.default_port(),
        }
    }

    pub fn default_user(self, kind: DbKind) -> &'static str {
        match self {
            Product::CockroachDb | Product::TiDb => "root",
            Product::YugabyteDb => "yugabyte",
            Product::TimescaleDb => "tsdbadmin",
            Product::Neon => "neondb_owner",
            Product::PlanetScale | Product::SingleStore => "",
            _ => kind.default_user(),
        }
    }

    /// The database to connect to when none is given.
    pub fn default_database(self) -> &'static str {
        match self {
            Product::CockroachDb => "defaultdb",
            Product::YugabyteDb => "yugabyte",
            Product::TimescaleDb => "tsdb",
            Product::Neon => "neondb",
            _ => "",
        }
    }

    /// Whether the engine's dump tools (`pg_dump`, `mysqldump`) work with it.
    /// CockroachDB backs up with its own `BACKUP` statement to cloud storage.
    pub fn supports_backup(self) -> bool {
        self != Product::CockroachDb
    }

    /// Hosted services take only TLS connections.
    pub fn default_ssl(self) -> SslMode {
        match self {
            Product::TimescaleDb | Product::Supabase | Product::Neon | Product::PlanetScale => SslMode::Require,
            _ => SslMode::default(),
        }
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

/// How to sign in to an SSH server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SshAuth {
    #[default]
    Password,
    /// A private key file, with its passphrase in `password` when it has one.
    Key,
}

/// An SSH server the connection goes through: the database's host and port
/// are reached from there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshTunnel {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// The password, or the key's passphrase. Stored only with `save_password`.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub key_file: String,
}

fn ssh_port() -> u16 {
    22
}

impl Default for SshTunnel {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: ssh_port(),
            user: String::new(),
            auth: SshAuth::default(),
            password: String::new(),
            key_file: String::new(),
        }
    }
}

/// A saved connection ("data source" in DataGrip terms).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub kind: DbKind,
    /// The product on `kind`'s protocol, such as CockroachDB on PostgreSQL's.
    #[serde(default)]
    pub product: Product,
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
    #[serde(default)]
    pub ssh: SshTunnel,
    /// Marks the connection's row and tabs, for example red for production.
    #[serde(default)]
    pub color: ConnColor,
    /// The explorer folder the connection sits in; empty for the top level.
    #[serde(default)]
    pub folder: String,
    /// Only statements that read run on this connection.
    #[serde(default)]
    pub read_only: bool,
    /// A console asks before running statements that change data or structure.
    #[serde(default)]
    pub confirm_changes: bool,
}

/// The color of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ConnColor {
    #[default]
    None,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl ConnColor {
    pub const ALL: [ConnColor; 7] = [
        ConnColor::None,
        ConnColor::Red,
        ConnColor::Orange,
        ConnColor::Yellow,
        ConnColor::Green,
        ConnColor::Blue,
        ConnColor::Purple,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            ConnColor::None => "No color",
            ConnColor::Red => "Red",
            ConnColor::Orange => "Orange",
            ConnColor::Yellow => "Yellow",
            ConnColor::Green => "Green",
            ConnColor::Blue => "Blue",
            ConnColor::Purple => "Purple",
        }
    }

    pub const fn rgb(self) -> Option<[u8; 3]> {
        Some(match self {
            ConnColor::None => return None,
            ConnColor::Red => [0xef, 0x44, 0x44],
            ConnColor::Orange => [0xf9, 0x73, 0x16],
            ConnColor::Yellow => [0xea, 0xb3, 0x08],
            ConnColor::Green => [0x22, 0xc5, 0x5e],
            ConnColor::Blue => [0x3b, 0x82, 0xf6],
            ConnColor::Purple => [0xa8, 0x55, 0xf7],
        })
    }
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
            product: Product::Native,
            host: if kind.is_file_based() { String::new() } else { "localhost".into() },
            port: kind.default_port().unwrap_or(0),
            user: kind.default_user().into(),
            password: String::new(),
            save_password: true,
            database: String::new(),
            ssl_mode: SslMode::default(),
            only_default_database: false,
            ssh: SshTunnel::default(),
            color: ConnColor::None,
            folder: String::new(),
            read_only: false,
            confirm_changes: false,
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

    /// A DuckDB connection to a folder of data files rather than a database file.
    pub fn is_folder(&self) -> bool {
        self.kind == DbKind::DuckDb && std::path::Path::new(self.database.trim()).is_dir()
    }

    /// Whether the connection goes through an SSH server.
    pub fn uses_ssh(&self) -> bool {
        self.ssh.enabled && !self.kind.is_file_based()
    }

    /// Switches the engine, resetting the port and user if they were the old defaults.
    pub fn set_kind(&mut self, kind: DbKind) {
        self.set_product(kind, Product::Native);
    }

    /// Switches the engine and product, resetting the port, user, database and
    /// TLS mode if they were the old product's defaults.
    pub fn set_product(&mut self, kind: DbKind, product: Product) {
        if self.kind == kind && self.product == product {
            return;
        }
        let (old_kind, old) = (self.kind, self.product);
        if self.port == old.default_port(old_kind).unwrap_or(0) {
            self.port = product.default_port(kind).unwrap_or(0);
        }
        if self.user == old.default_user(old_kind) {
            self.user = product.default_user(kind).into();
        }
        if self.database == old.default_database() {
            self.database = product.default_database().into();
        }
        if self.ssl_mode == old.default_ssl() {
            self.ssl_mode = product.default_ssl();
        }
        if !kind.is_file_based() && self.host.is_empty() {
            self.host = "localhost".into();
        }
        self.kind = kind;
        self.product = product;
    }

    /// The name of the engine or product, as the connection dialog lists it.
    pub fn type_label(&self) -> &'static str {
        self.product.label(self.kind)
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
    fn set_product_moves_defaults() {
        let mut c = ConnectionConfig::new(DbKind::Postgres);
        c.set_product(DbKind::Postgres, Product::CockroachDb);
        assert_eq!((c.port, c.user.as_str(), c.database.as_str()), (26257, "root", "defaultdb"));
        c.set_product(DbKind::MySql, Product::PlanetScale);
        assert_eq!((c.port, c.user.as_str(), c.database.as_str()), (3306, "", ""));
        assert_eq!(c.ssl_mode, SslMode::Require);
        c.user = "app".into();
        c.set_kind(DbKind::Postgres);
        assert_eq!((c.port, c.user.as_str(), c.ssl_mode), (5432, "app", SslMode::Prefer));
        assert_eq!(c.type_label(), "PostgreSQL");
        for (kind, product) in Product::CHOICES {
            assert!(product.engine().is_none_or(|k| k == kind));
        }
    }

    #[test]
    fn old_settings_have_no_product() {
        let mut v = serde_json::to_value(ConnectionConfig::new(DbKind::MySql)).unwrap();
        v.as_object_mut().unwrap().remove("product");
        assert_eq!(serde_json::from_value::<ConnectionConfig>(v).unwrap().product, Product::Native);
    }

    #[test]
    fn roundtrips_through_json() {
        let c = ConnectionConfig::new(DbKind::Sqlite);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<ConnectionConfig>(&json).unwrap(), c);
    }
}
