//! Copying tables between connections, also between different engines:
//! column types are mapped through a common set, and values are adjusted to
//! what the target accepts (booleans, bytes in text columns and the reverse).

use crate::ddl::TableDef;
use crate::sql::Dialect;
use crate::{Cell, ColumnInfo, DbKind, ObjectPath, TableDetails, dml};

/// Column types every engine has some form of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generic {
    Bool,
    Int16,
    Int32,
    Int64,
    /// Precision and scale; `None` for an unbounded NUMERIC.
    Decimal(Option<(u32, u32)>),
    Float32,
    Float64,
    /// Maximum length, if any.
    Text(Option<u32>),
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    Binary,
}

fn args(t: &str) -> Vec<u32> {
    let Some(start) = t.find('(') else { return Vec::new() };
    let end = t[start..].find(')').map_or(t.len(), |e| start + e);
    t[start + 1..end].split(',').filter_map(|a| a.trim().parse().ok()).collect()
}

/// Unwraps ClickHouse's `Nullable(…)` and `LowCardinality(…)`.
fn unwrap_ch(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let lower = t.to_ascii_lowercase();
        let inner = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|p| lower.starts_with(p).then(|| t[p.len()..].strip_suffix(')').unwrap_or(&t[p.len()..])));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

/// The common type of a column type as an engine names it; unknown types are text.
pub fn generic(kind: DbKind, data_type: &str) -> Generic {
    let raw = unwrap_ch(data_type);
    let t = raw.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("").trim();
    let a = args(&t);
    let decimal = || match a[..] {
        [p, s] => Generic::Decimal(Some((p, s))),
        [p] => Generic::Decimal(Some((p, 0))),
        _ => Generic::Decimal(None),
    };
    let text = || Generic::Text(a.first().copied().filter(|n| *n > 0));
    match base {
        "bool" | "boolean" => Generic::Bool,
        "bit" if kind == DbKind::MsSql || a.first().is_none_or(|n| *n == 1) => Generic::Bool,
        "tinyint" if kind == DbKind::MySql && a.first() == Some(&1) => Generic::Bool,
        "tinyint" | "smallint" | "int2" | "int16" | "uint8" | "utinyint" | "year" => Generic::Int16,
        "int8" if kind == DbKind::ClickHouse => Generic::Int16,
        "int8" | "bigint" | "bigserial" | "serial8" | "int64" | "uint32" | "uinteger" => Generic::Int64,
        "int" | "integer" | "int4" | "serial" | "serial4" | "mediumint" | "int32" | "uint16" | "usmallint" => {
            if kind == DbKind::Sqlite { Generic::Int64 } else { Generic::Int32 }
        }
        "uint64" | "ubigint" | "hugeint" | "uhugeint" | "int128" | "uint128" | "int256" | "uint256" => {
            Generic::Decimal(Some((38, 0)))
        }
        "decimal" | "numeric" | "number" | "money" | "smallmoney" | "dec" => decimal(),
        "decimal32" | "decimal64" | "decimal128" | "decimal256" => {
            Generic::Decimal(Some((38, a.first().copied().unwrap_or(0))))
        }
        "real" | "float4" | "float32" => Generic::Float32,
        "float" if kind == DbKind::MySql && a.first().is_none_or(|p| *p <= 24) => Generic::Float32,
        "double" | "float8" | "float64" | "float" => Generic::Float64,
        "date" | "date32" => Generic::Date,
        "time" | "timetz" => Generic::Time,
        "datetimeoffset" | "timestamptz" => Generic::TimestampTz,
        "timestamp" if t.contains("with time zone") => Generic::TimestampTz,
        "datetime64" | "datetime" if t.contains('\'') => Generic::TimestampTz,
        "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "datetime64" | "timestamp_s" | "timestamp_ms"
        | "timestamp_ns" => Generic::Timestamp,
        "uuid" | "uniqueidentifier" => Generic::Uuid,
        "json" | "jsonb" => Generic::Json,
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "image" | "bytes" => {
            Generic::Binary
        }
        "varchar" | "nvarchar" | "char" | "nchar" | "character" | "fixedstring" | "bpchar" | "string" => text(),
        "integer64" => Generic::Int64,
        _ if t.contains("char") => text(),
        _ if kind == DbKind::Sqlite && t.contains("int") => Generic::Int64,
        _ => Generic::Text(None),
    }
}

/// How an engine writes a common type. `keyed` columns (in a key or index)
/// get a bounded text type where the engine cannot index unbounded text.
pub fn render(kind: DbKind, g: Generic, keyed: bool) -> String {
    let dec = |d: Option<(u32, u32)>, max: u32, unbounded: &str| match d {
        Some((p, s)) => {
            let p = p.clamp(1, max);
            format!("({p},{})", s.min(p))
        }
        None => unbounded.to_string(),
    };
    match kind {
        DbKind::Postgres => match g {
            Generic::Bool => "boolean".into(),
            Generic::Int16 => "smallint".into(),
            Generic::Int32 => "integer".into(),
            Generic::Int64 => "bigint".into(),
            Generic::Decimal(d) => format!("numeric{}", dec(d, 1000, "")),
            Generic::Float32 => "real".into(),
            Generic::Float64 => "double precision".into(),
            Generic::Text(Some(n)) => format!("varchar({n})"),
            Generic::Text(None) => "text".into(),
            Generic::Date => "date".into(),
            Generic::Time => "time".into(),
            Generic::Timestamp => "timestamp".into(),
            Generic::TimestampTz => "timestamptz".into(),
            Generic::Uuid => "uuid".into(),
            Generic::Json => "jsonb".into(),
            Generic::Binary => "bytea".into(),
        },
        DbKind::MySql => match g {
            Generic::Bool => "tinyint(1)".into(),
            Generic::Int16 => "smallint".into(),
            Generic::Int32 => "int".into(),
            Generic::Int64 => "bigint".into(),
            Generic::Decimal(d) => format!("decimal{}", dec(d, 65, "(38,10)")),
            Generic::Float32 => "float".into(),
            Generic::Float64 => "double".into(),
            Generic::Text(Some(n)) if n <= 16_000 => format!("varchar({n})"),
            Generic::Text(_) if keyed => "varchar(255)".into(),
            Generic::Text(_) => "longtext".into(),
            Generic::Date => "date".into(),
            Generic::Time => "time(6)".into(),
            Generic::Timestamp | Generic::TimestampTz => "datetime(6)".into(),
            Generic::Uuid => "char(36)".into(),
            Generic::Json if keyed => "varchar(255)".into(),
            Generic::Json => "json".into(),
            Generic::Binary if keyed => "varbinary(255)".into(),
            Generic::Binary => "longblob".into(),
        },
        DbKind::Sqlite | DbKind::Redis | DbKind::MongoDb => match g {
            Generic::Bool | Generic::Int16 | Generic::Int32 | Generic::Int64 => "INTEGER".into(),
            Generic::Decimal(_) => "NUMERIC".into(),
            Generic::Float32 | Generic::Float64 => "REAL".into(),
            Generic::Binary => "BLOB".into(),
            _ => "TEXT".into(),
        },
        DbKind::DuckDb => match g {
            Generic::Bool => "BOOLEAN".into(),
            Generic::Int16 => "SMALLINT".into(),
            Generic::Int32 => "INTEGER".into(),
            Generic::Int64 => "BIGINT".into(),
            Generic::Decimal(d) => format!("DECIMAL{}", dec(d, 38, "(38,10)")),
            Generic::Float32 => "REAL".into(),
            Generic::Float64 => "DOUBLE".into(),
            Generic::Text(_) => "VARCHAR".into(),
            Generic::Date => "DATE".into(),
            Generic::Time => "TIME".into(),
            Generic::Timestamp => "TIMESTAMP".into(),
            Generic::TimestampTz => "TIMESTAMPTZ".into(),
            Generic::Uuid => "UUID".into(),
            Generic::Json => "JSON".into(),
            Generic::Binary => "BLOB".into(),
        },
        DbKind::MsSql => match g {
            Generic::Bool => "bit".into(),
            Generic::Int16 => "smallint".into(),
            Generic::Int32 => "int".into(),
            Generic::Int64 => "bigint".into(),
            Generic::Decimal(d) => format!("decimal{}", dec(d, 38, "(38,10)")),
            Generic::Float32 => "real".into(),
            Generic::Float64 => "float".into(),
            Generic::Text(Some(n)) if n <= 4000 => format!("nvarchar({n})"),
            Generic::Text(_) | Generic::Json if keyed => "nvarchar(450)".into(),
            Generic::Text(_) | Generic::Json => "nvarchar(max)".into(),
            Generic::Date => "date".into(),
            Generic::Time => "time".into(),
            Generic::Timestamp => "datetime2".into(),
            Generic::TimestampTz => "datetimeoffset".into(),
            Generic::Uuid => "uniqueidentifier".into(),
            Generic::Binary if keyed => "varbinary(900)".into(),
            Generic::Binary => "varbinary(max)".into(),
        },
        DbKind::ClickHouse => match g {
            Generic::Bool => "Bool".into(),
            Generic::Int16 => "Int16".into(),
            Generic::Int32 => "Int32".into(),
            Generic::Int64 => "Int64".into(),
            Generic::Decimal(d) => format!("Decimal{}", dec(d, 76, "(38,10)")),
            Generic::Float32 => "Float32".into(),
            Generic::Float64 => "Float64".into(),
            Generic::Date => "Date32".into(),
            Generic::Timestamp => "DateTime64(6)".into(),
            Generic::TimestampTz => "DateTime64(6, 'UTC')".into(),
            Generic::Uuid => "UUID".into(),
            _ => "String".into(),
        },
    }
}

/// A plain literal default (number, string, boolean) that any engine reads the same.
fn portable_default(d: &str) -> bool {
    let d = d.trim();
    let quoted = d.len() >= 2 && d.starts_with('\'') && d.ends_with('\'') && !d[1..d.len() - 1].contains('\'');
    quoted || d.parse::<f64>().is_ok() || matches!(d.to_ascii_lowercase().as_str(), "true" | "false" | "null")
}

/// The target table for a source table: types mapped (kept when both
/// engines are the same), server-made defaults dropped, foreign keys kept
/// only when `keep_fk(table)` (the referenced table is copied too), pointing
/// at the target schema.
pub fn target_def(
    from: DbKind,
    to: DbKind,
    target: ObjectPath,
    source: &TableDetails,
    keep_fk: impl Fn(&str) -> bool,
) -> (TableDef, Vec<String>) {
    let mut notes = Vec::new();
    let mut def = TableDef::from_details(target.clone(), source);
    let keyed: Vec<&str> = source
        .indexes
        .iter()
        .flat_map(|i| i.columns.iter().map(String::as_str))
        .chain(source.primary_key())
        .chain(source.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)))
        .collect();
    for c in &mut def.columns {
        c.original = None;
        let col = &mut c.col;
        if from != to {
            col.data_type = render(to, generic(from, &col.data_type), keyed.contains(&col.name.as_str()));
        }
        if col.default.as_deref().is_some_and(|d| !portable_default(d) || from != to && col.auto_increment) {
            col.default = None;
        }
        if to == DbKind::ClickHouse && col.primary_key {
            // Sorting keys cannot be Nullable.
            col.nullable = false;
        }
    }
    let before = def.foreign_keys.len();
    def.foreign_keys.retain(|f| keep_fk(&f.ref_table));
    for f in &mut def.foreign_keys {
        f.ref_schema = target.schema.clone();
    }
    if def.foreign_keys.len() < before {
        notes.push(format!("{}: foreign keys to tables that are not copied are left out", target.name));
    }
    if to == DbKind::ClickHouse {
        def.foreign_keys.clear();
        def.indexes.clear();
    }
    (def, notes)
}

/// A source value as the target column takes it.
pub fn convert_cell(to: DbKind, g: Generic, cell: Cell) -> Cell {
    match (g, cell) {
        (_, Cell::Null) => Cell::Null,
        (Generic::Bool, Cell::Text(s)) => {
            let on = matches!(s.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "y" | "yes" | "on");
            Cell::Text(match to {
                DbKind::Postgres | DbKind::DuckDb | DbKind::ClickHouse => if on { "true" } else { "false" }.into(),
                _ => if on { "1" } else { "0" }.into(),
            })
        }
        (Generic::Binary, Cell::Text(s)) if to != DbKind::ClickHouse => Cell::Bytes(s.into_bytes()),
        // ClickHouse keeps bytes in String columns; SQLite stores any type.
        (Generic::Text(_) | Generic::Json, Cell::Bytes(b)) if to != DbKind::ClickHouse && to != DbKind::Sqlite => {
            match String::from_utf8(b) {
                Ok(s) => Cell::Text(s),
                Err(e) => Cell::Bytes(e.into_bytes()),
            }
        }
        (Generic::TimestampTz, Cell::Text(s)) => Cell::Text(offset_with_minutes(&s)),
        (_, c) => c,
    }
}

/// `…+03` → `…+03:00`, which every engine parses.
fn offset_with_minutes(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() > 3 && matches!(b[b.len() - 3], b'+' | b'-') && b[b.len() - 2..].iter().all(u8::is_ascii_digit) {
        format!("{s}:00")
    } else {
        s.to_string()
    }
}

/// Whether values of this type are written as bare numbers.
pub fn numeric(to: DbKind, g: Generic) -> bool {
    match g {
        Generic::Bool => !matches!(to, DbKind::Postgres | DbKind::DuckDb | DbKind::ClickHouse),
        Generic::Int16
        | Generic::Int32
        | Generic::Int64
        | Generic::Decimal(_)
        | Generic::Float32
        | Generic::Float64 => true,
        _ => false,
    }
}

/// A target column the copy writes, with how to convert its values.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyColumn {
    pub name: String,
    pub generic: Generic,
    pub numeric: bool,
    pub auto_increment: bool,
}

impl CopyColumn {
    pub fn new(to: DbKind, c: &ColumnInfo) -> Self {
        let generic = generic(to, &c.data_type);
        Self { name: c.name.clone(), generic, numeric: numeric(to, generic), auto_increment: c.auto_increment }
    }
}

/// `INSERT INTO t (a, b) VALUES (…), (…)` with converted values; `rows` hold
/// one cell per column.
pub fn insert_sql(to: DbKind, table: &ObjectPath, columns: &[CopyColumn], rows: Vec<Vec<Cell>>) -> String {
    let d = Dialect(to);
    let names = columns.iter().map(|c| d.ident(&c.name)).collect::<Vec<_>>().join(", ");
    let mut sql = format!("INSERT INTO {} ({names}) VALUES", d.qualified(table));
    for (i, row) in rows.into_iter().enumerate() {
        sql.push_str(if i == 0 { "\n(" } else { ",\n(" });
        for (j, (cell, c)) in row.into_iter().zip(columns).enumerate() {
            if j > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&dml::literal(d, &convert_cell(to, c.generic, cell), c.numeric));
        }
        sql.push(')');
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_map_between_engines() {
        let pg = |t: &str| generic(DbKind::Postgres, t);
        assert_eq!(pg("character varying(20)"), Generic::Text(Some(20)));
        assert_eq!(pg("numeric(12,2)"), Generic::Decimal(Some((12, 2))));
        assert_eq!(pg("int8"), Generic::Int64);
        assert_eq!(pg("timestamp with time zone"), Generic::TimestampTz);
        assert_eq!(pg("timestamp without time zone"), Generic::Timestamp);
        assert_eq!(generic(DbKind::ClickHouse, "Nullable(Int8)"), Generic::Int16);
        assert_eq!(generic(DbKind::ClickHouse, "LowCardinality(Nullable(String))"), Generic::Text(None));
        assert_eq!(generic(DbKind::ClickHouse, "DateTime64(3, 'UTC')"), Generic::TimestampTz);
        assert_eq!(generic(DbKind::MySql, "tinyint(1)"), Generic::Bool);
        assert_eq!(generic(DbKind::MsSql, "bit"), Generic::Bool);
        assert_eq!(generic(DbKind::MsSql, "nvarchar(max)"), Generic::Text(None));
        assert_eq!(generic(DbKind::Sqlite, "INTEGER"), Generic::Int64);
        assert_eq!(generic(DbKind::Sqlite, ""), Generic::Text(None));
        assert_eq!(render(DbKind::MySql, Generic::Text(None), true), "varchar(255)");
        assert_eq!(render(DbKind::MsSql, Generic::Decimal(Some((80, 2))), false), "decimal(38,2)");
        assert_eq!(render(DbKind::ClickHouse, Generic::Decimal(None), false), "Decimal(38,10)");
    }

    #[test]
    fn values_follow_the_target() {
        let b = |to, s: &str| convert_cell(to, Generic::Bool, Cell::Text(s.into()));
        assert_eq!(b(DbKind::Postgres, "1"), Cell::Text("true".into()));
        assert_eq!(b(DbKind::MsSql, "t"), Cell::Text("1".into()));
        assert_eq!(convert_cell(DbKind::MsSql, Generic::Binary, Cell::Text("ab".into())), Cell::Bytes(b"ab".to_vec()));
        assert_eq!(
            convert_cell(DbKind::MsSql, Generic::TimestampTz, Cell::Text("2026-01-02 03:04:05+03".into())),
            Cell::Text("2026-01-02 03:04:05+03:00".into())
        );
        let cols = vec![
            CopyColumn { name: "id".into(), generic: Generic::Int32, numeric: true, auto_increment: false },
            CopyColumn { name: "ok".into(), generic: Generic::Bool, numeric: false, auto_increment: false },
        ];
        let sql = insert_sql(
            DbKind::Postgres,
            &ObjectPath::new("d", "s", "t"),
            &cols,
            vec![vec![Cell::Text("1".into()), Cell::Text("1".into())]],
        );
        assert_eq!(sql, "INSERT INTO \"s\".\"t\" (\"id\", \"ok\") VALUES\n(1, 'true')");
    }

    #[test]
    fn target_tables_drop_engine_specific_defaults() {
        let source = TableDetails {
            columns: vec![
                ColumnInfo {
                    name: "id".into(),
                    data_type: "integer".into(),
                    primary_key: true,
                    auto_increment: true,
                    default: Some("nextval('t_id_seq'::regclass)".into()),
                    ..Default::default()
                },
                ColumnInfo {
                    name: "state".into(),
                    data_type: "character varying(10)".into(),
                    nullable: true,
                    default: Some("'new'".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (def, _) = target_def(DbKind::Postgres, DbKind::MsSql, ObjectPath::new("d", "dbo", "t"), &source, |_| true);
        assert_eq!(def.columns[0].col.data_type, "int");
        assert_eq!(def.columns[0].col.default, None);
        assert_eq!(def.columns[1].col.data_type, "nvarchar(10)");
        assert_eq!(def.columns[1].col.default.as_deref(), Some("'new'"));
    }
}
