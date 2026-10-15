use std::borrow::Cow;
use std::time::Duration;

/// One value of a result set.
///
/// Values are kept as text (as the server formats them), which is what a
/// client shows and edits; binary values stay as bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Null,
    Text(String),
    Bytes(Vec<u8>),
}

impl Cell {
    pub fn is_null(&self) -> bool {
        matches!(self, Cell::Null)
    }

    /// Text for a grid cell: `NULL`, the value, or bytes as `0x…` hex (shortened).
    pub fn display(&self) -> Cow<'_, str> {
        match self {
            Cell::Null => Cow::Borrowed("NULL"),
            Cell::Text(s) => Cow::Borrowed(s),
            Cell::Bytes(b) => Cow::Owned(hex_preview(b, 64)),
        }
    }

    /// The value as text, `None` for NULL; bytes as full `0x…` hex.
    pub fn as_text(&self) -> Option<Cow<'_, str>> {
        match self {
            Cell::Null => None,
            Cell::Text(s) => Some(Cow::Borrowed(s)),
            Cell::Bytes(b) => Some(Cow::Owned(hex_preview(b, usize::MAX))),
        }
    }
}

fn hex_preview(bytes: &[u8], max: usize) -> String {
    let mut s = String::with_capacity(2 + bytes.len().min(max) * 2);
    s.push_str("0x");
    for b in bytes.iter().take(max) {
        s.push_str(&format!("{b:02X}"));
    }
    if bytes.len() > max {
        s.push('…');
    }
    s
}

/// A column of a result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    /// The server's type name, lower case (`int4`, `varchar`, `INTEGER`…); empty if unknown.
    pub type_name: String,
    /// Numbers are right-aligned and sorted by value.
    pub numeric: bool,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        let type_name = type_name.into();
        let numeric = is_numeric_type(&type_name);
        Self { name: name.into(), type_name, numeric }
    }
}

/// Recognizes numeric type names of all supported engines.
pub fn is_numeric_type(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(
        base,
        "int"
            | "int2"
            | "int4"
            | "int8"
            | "integer"
            | "smallint"
            | "bigint"
            | "tinyint"
            | "mediumint"
            | "serial"
            | "bigserial"
            | "smallserial"
            | "decimal"
            | "numeric"
            | "real"
            | "double"
            | "float"
            | "float4"
            | "float8"
            | "money"
            | "oid"
            | "year"
            | "longlong"
            | "long"
            | "short"
            | "tiny"
            | "int24"
            | "newdecimal"
            | "hugeint"
            | "uhugeint"
            | "utinyint"
            | "usmallint"
            | "uinteger"
            | "ubigint"
    )
}

/// Rows returned by a statement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Cell>>,
    /// More rows were available than the requested maximum.
    pub truncated: bool,
}

/// What one executed statement produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementOutcome {
    Rows(QueryResult),
    /// A statement without a result set; the affected row count if the server reported one.
    Done {
        affected: Option<u64>,
    },
}

impl StatementOutcome {
    pub fn summary(&self, elapsed: Duration) -> String {
        let ms = elapsed.as_secs_f64() * 1000.0;
        match self {
            StatementOutcome::Rows(r) => {
                let more = if r.truncated { "+" } else { "" };
                format!("{}{} rows in {ms:.0} ms", r.rows.len(), more)
            }
            StatementOutcome::Done { affected: Some(n) } => format!("{n} rows affected in {ms:.0} ms"),
            StatementOutcome::Done { affected: None } => format!("completed in {ms:.0} ms"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_types() {
        assert!(is_numeric_type("int4"));
        assert!(is_numeric_type("NUMERIC(10,2)"));
        assert!(is_numeric_type("double precision"));
        assert!(!is_numeric_type("varchar(20)"));
        assert!(!is_numeric_type(""));
    }

    #[test]
    fn bytes_display_as_hex() {
        assert_eq!(Cell::Bytes(vec![0xde, 0xad]).display(), "0xDEAD");
        assert_eq!(Cell::Null.display(), "NULL");
    }
}
