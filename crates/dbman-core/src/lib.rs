//! Database-agnostic core of DBMan: connection settings, the [`Connection`]
//! trait every driver implements, schema and result models, and SQL helpers.
//!
//! The crate has no GUI or driver dependencies, so new database engines are
//! added by implementing [`Connection`] in `dbman-drivers` (or any other crate).

mod config;
mod driver;
mod error;
mod result;
mod schema;
pub mod sql;

pub use config::{ConnectionConfig, DbKind, SslMode};
pub use driver::{Capabilities, Connection};
pub use error::{DbError, DbResult};
pub use result::{Cell, ColumnMeta, QueryResult, StatementOutcome};
pub use schema::{ColumnInfo, DbObject, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectPath, TableDetails};
