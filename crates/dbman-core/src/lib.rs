//! Database-agnostic core of DBMan: connection settings, the [`Connection`]
//! trait every driver implements, schema and result models, and SQL helpers.
//!
//! The crate has no GUI or driver dependencies, so new database engines are
//! added by implementing [`Connection`] in `dbman-drivers` (or any other crate).

pub mod backup;
pub mod compare;
pub mod complete;
mod config;
pub mod copy;
pub mod ddl;
pub mod dml;
mod driver;
pub mod er;
mod error;
pub mod explain;
pub mod export;
pub mod generate;
pub mod import;
pub mod infer;
mod result;
mod schema;
pub mod sql;
pub mod xlsx;

pub use config::{ConnectionConfig, DbKind, SslMode};
pub use driver::{Capabilities, Connection};
pub use error::{DbError, DbResult};
pub use result::{Cell, ColumnMeta, QueryResult, StatementOutcome};
pub use schema::{ColumnInfo, DbObject, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectPath, TableDetails};
