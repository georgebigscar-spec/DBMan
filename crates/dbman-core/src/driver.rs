use async_trait::async_trait;

use crate::{DbKind, DbObject, DbResult, ObjectPath, StatementOutcome, TableDetails};

/// What the explorer should show for an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Databases contain schemas (PostgreSQL). Otherwise a database *is* the
    /// schema (MySQL) or there is one implicit schema per database (SQLite),
    /// and the explorer skips the schema level.
    pub schemas: bool,
}

impl Capabilities {
    pub fn for_kind(kind: DbKind) -> Self {
        Self { schemas: matches!(kind, DbKind::Postgres | DbKind::DuckDb | DbKind::MsSql) }
    }
}

/// An open session to a database server or file.
///
/// Each SQL console opens its own `Connection`, so transactions and session
/// settings stay per console, as in DataGrip. Implementations must be cheap to
/// share across tasks (`Arc<dyn Connection>`); calls may run concurrently.
#[async_trait]
pub trait Connection: Send + Sync {
    fn kind(&self) -> DbKind;

    fn capabilities(&self) -> Capabilities {
        Capabilities::for_kind(self.kind())
    }

    /// Server product and version, for the status line.
    async fn server_version(&self) -> DbResult<String>;

    /// Databases on the server (for SQLite: `main` and attached databases).
    async fn databases(&self) -> DbResult<Vec<String>>;

    /// Schemas of `database`. Engines without schemas return `[database]`.
    async fn schemas(&self, database: &str) -> DbResult<Vec<String>>;

    /// Tables, views, routines and other objects of a schema.
    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>>;

    /// Columns, indexes and foreign keys of a table or view.
    async fn table_details(&self, path: &ObjectPath) -> DbResult<TableDetails>;

    /// The `CREATE` statement of an object, as the server reports it or rebuilt from the catalog.
    async fn ddl(&self, object: &DbObject) -> DbResult<String>;

    /// Runs one SQL statement in `database` (the current one if `None`) and
    /// returns at most `max_rows` rows.
    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome>;

    /// Asks the server to stop the statement this session is running right now.
    /// Returns once the request is sent; the running `execute` then ends early,
    /// usually with an error. Does nothing when the session is idle.
    async fn cancel(&self) -> DbResult<()>;

    /// Runs one Redis command, its arguments as typed, in `database` (`db3`)
    /// or the current one. Only Redis connections take commands.
    async fn command(&self, _database: Option<&str>, _args: &[String]) -> DbResult<crate::redis::Reply> {
        Err(crate::DbError::new("This connection does not take Redis commands."))
    }

    /// Runs one mongo shell statement in `database` (the connection's own if
    /// `None`), fetching at most `max_docs` documents. Only MongoDB
    /// connections take them.
    async fn mongo(
        &self,
        _database: Option<&str>,
        _statement: &crate::mongo::Shell,
        _max_docs: usize,
    ) -> DbResult<crate::mongo::Reply> {
        Err(crate::DbError::new("This connection does not take MongoDB statements."))
    }
}
