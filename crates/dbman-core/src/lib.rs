//! Database-agnostic core of DBMan: connection settings, the [`Connection`]
//! trait every driver implements, schema and result models, and SQL helpers.
//!
//! The crate has no GUI or driver dependencies, so new database engines are
//! added by implementing [`Connection`] in `dbman-drivers` (or any other crate).

pub mod admin;
pub mod backup;
pub mod chart;
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
pub mod find;
pub mod format;
pub mod generate;
pub mod import;
pub mod infer;
pub mod params;
mod result;
mod schema;
pub mod search;
pub mod sessions;
pub mod sql;
pub mod value;
pub mod xlsx;

pub use config::{ConnColor, ConnectionConfig, DbKind, Product, SshAuth, SshTunnel, SslMode};
pub use driver::{Capabilities, Connection};
pub use error::{DbError, DbResult};
pub use result::{Cell, ColumnMeta, QueryResult, StatementOutcome};
pub use schema::{ColumnInfo, DbObject, ForeignKeyInfo, IndexInfo, ObjectKind, ObjectPath, TableDetails};
