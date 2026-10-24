//! Database-agnostic core of DBMan: connection settings, the [`Connection`]
//! trait every driver implements, schema and result models, and SQL helpers.
//!
//! The crate has no GUI or driver dependencies, so new database engines are
//! added by implementing [`Connection`] in `dbman-drivers` (or any other crate).

pub mod admin;
pub mod aggregation;
pub mod backup;
pub mod chart;
pub mod compare;
pub mod complete;
mod config;
pub mod copy;
pub mod dashboard;
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
pub mod json_tree;
pub mod mongo;
pub mod params;
pub mod paste;
pub mod pivot;
pub mod profile;
pub mod query_builder;
pub mod redis;
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
