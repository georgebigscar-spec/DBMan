//! Read-only connections: statements that change data or structure are
//! refused before they reach the server.

use std::sync::Arc;

use async_trait::async_trait;
use dbman_core::{
    Capabilities, Connection, DbError, DbKind, DbObject, DbResult, ObjectPath, StatementOutcome, TableDetails, sql,
};

pub struct ReadOnly {
    pub inner: Arc<dyn Connection>,
}

/// The error a refused statement gets.
pub fn refused() -> DbError {
    DbError::new(
        "The connection is read-only: only statements that read data can run. Turn \"Read-only\" off in the connection's properties to change data.",
    )
}

#[async_trait]
impl Connection for ReadOnly {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn server_version(&self) -> DbResult<String> {
        self.inner.server_version().await
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        self.inner.databases().await
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        self.inner.schemas(database).await
    }

    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>> {
        self.inner.objects(database, schema).await
    }

    async fn table_details(&self, path: &ObjectPath) -> DbResult<TableDetails> {
        self.inner.table_details(path).await
    }

    async fn ddl(&self, object: &DbObject) -> DbResult<String> {
        self.inner.ddl(object).await
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        if !sql::is_read_only(self.kind(), sql) {
            return Err(refused());
        }
        self.inner.execute(database, sql, max_rows).await
    }

    async fn cancel(&self) -> DbResult<()> {
        self.inner.cancel().await
    }
}
