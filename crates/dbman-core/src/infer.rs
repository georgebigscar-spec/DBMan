//! Column types for a new table, guessed from the values of a file: whole
//! numbers, decimals, floats, booleans, dates, timestamps, UUIDs, `0x…`
//! bytes, and text otherwise. Types come out as [`Generic`], which
//! [`crate::copy::render`] writes for each engine.

use crate::copy::Generic;
use crate::ddl::{ColumnDef, TableDef};
use crate::import::Field;
use crate::{ColumnInfo, DbKind, ObjectPath};

/// What one column's values allow so far.
#[derive(Debug, Clone)]
struct Column {
    seen: bool,
    int32: bool,
    int64: bool,
    decimal: bool,
    float: bool,
    boolean: bool,
    date: bool,
    timestamp: bool,
    uuid: bool,
    hex: bool,
    /// Digits before and after the point, at most.
    whole: u32,
    scale: u32,
    /// Characters, at most.
    len: usize,
}

impl Default for Column {
    fn default() -> Self {
        Self {
            seen: false,
            int32: true,
            int64: true,
            decimal: true,
            float: true,
            boolean: true,
            date: true,
            timestamp: true,
            uuid: true,
            hex: true,
            whole: 0,
            scale: 0,
            len: 0,
        }
    }
}

/// `-?digits[.digits]` without leading zeros: digits before and after the point.
fn decimal_parts(s: &str) -> Option<(u32, u32)> {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || (s.contains('.') && !digits(frac)) {
        return None;
    }
    // `007` is a code, not a number.
    if int.len() > 1 && int.starts_with('0') {
        return None;
    }
    let whole = if int == "0" { 0 } else { int.len() as u32 };
    Some((whole, frac.len() as u32))
}

fn is_float(s: &str) -> bool {
    let t = s.strip_prefix('-').unwrap_or(s);
    t.contains(['e', 'E'])
        && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'-' | b'+'))
        && !t.starts_with(['e', 'E'])
        && s.parse::<f64>().is_ok_and(f64::is_finite)
}

/// `hh:mm:ss[.fff]`.
fn is_time(s: &str) -> bool {
    let (hms, frac) = s.split_once('.').unwrap_or((s, ""));
    let parts: Vec<&str> = hms.split(':').collect();
    parts.len() == 3
        && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
        && parts[0] < "24"
        && parts[1] < "60"
        && parts[2] < "61"
        && frac.bytes().all(|b| b.is_ascii_digit())
        && (s.contains('.') == !frac.is_empty())
}

/// `YYYY-MM-DD` that is a real day.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let Some(days) = crate::generate::days_from_date(s) else { return false };
    crate::generate::date_from_days(days) == s
}

fn is_timestamp(s: &str) -> bool {
    match (s.get(..10), s.get(11..)) {
        (Some(date), Some(time)) => is_date(date) && matches!(s.as_bytes()[10], b' ' | b'T') && is_time(time),
        _ => false,
    }
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter()
            .enumerate()
            .all(|(i, c)| if [8, 13, 18, 23].contains(&i) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

fn is_hex(s: &str) -> bool {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .is_some_and(|h| h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl Column {
    fn add(&mut self, v: &str) {
        self.seen = true;
        self.len = self.len.max(v.chars().count());
        match decimal_parts(v) {
            Some((whole, scale)) => {
                self.whole = self.whole.max(whole);
                self.scale = self.scale.max(scale);
                let int = if scale == 0 && !v.contains('.') { v.parse::<i64>().ok() } else { None };
                self.int64 &= int.is_some();
                self.int32 &= int.is_some_and(|n| i32::try_from(n).is_ok());
            }
            None => {
                self.int32 = false;
                self.int64 = false;
                self.decimal = false;
                self.float &= is_float(v);
            }
        }
        self.boolean &= v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false");
        self.date &= is_date(v);
        self.timestamp &= is_timestamp(v) || is_date(v);
        self.uuid &= is_uuid(v);
        self.hex &= is_hex(v);
    }

    fn generic(&self, kind: DbKind) -> Generic {
        let text = || if self.len <= 255 { Generic::Text(Some(255)) } else { Generic::Text(None) };
        if !self.seen {
            return Generic::Text(Some(255));
        }
        // MySQL and SQLite would take `true` as text into a number column.
        let real_bool = matches!(kind, DbKind::Postgres | DbKind::DuckDb | DbKind::ClickHouse | DbKind::MsSql);
        if self.int32 {
            Generic::Int32
        } else if self.int64 {
            Generic::Int64
        } else if self.decimal && self.whole + self.scale <= 38 {
            Generic::Decimal(Some(((self.whole + self.scale).max(1), self.scale)))
        } else if self.decimal || self.float {
            Generic::Float64
        } else if self.boolean && real_bool {
            Generic::Bool
        } else if self.date {
            Generic::Date
        } else if self.timestamp {
            Generic::Timestamp
        } else if self.uuid {
            Generic::Uuid
        } else if self.hex {
            Generic::Binary
        } else {
            text()
        }
    }
}

/// Collects the values of a file's rows; see [`Guess::types`].
#[derive(Debug, Clone, Default)]
pub struct Guess {
    columns: Vec<Column>,
    rows: u64,
}

impl Guess {
    pub fn new(width: usize) -> Self {
        Self { columns: vec![Column::default(); width], rows: 0 }
    }

    /// Takes a row into account; empty fields count as NULL.
    pub fn add(&mut self, row: &[Field]) {
        for (c, f) in self.columns.iter_mut().zip(row) {
            if let Some(v) = f.as_deref().filter(|v| !v.is_empty()) {
                c.add(v);
            }
        }
        self.rows += 1;
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The types every value seen so far fits.
    pub fn types(&self, kind: DbKind) -> Vec<Generic> {
        self.columns.iter().map(|c| c.generic(kind)).collect()
    }
}

/// Column names for a new table from a file's: trimmed, never empty, and
/// different from each other ignoring case.
pub fn column_names(file: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, name) in file.iter().enumerate() {
        let base = name.trim();
        let base = if base.is_empty() { format!("column{}", i + 1) } else { base.to_string() };
        let mut name = base.clone();
        let mut n = 1;
        while out.iter().any(|o| o.eq_ignore_ascii_case(&name)) {
            n += 1;
            name = format!("{base}_{n}");
        }
        out.push(name);
    }
    out
}

/// A table of nullable columns with these names and types.
pub fn table_def(path: ObjectPath, columns: &[(String, String)]) -> TableDef {
    let mut def = TableDef::new(path);
    def.columns = columns
        .iter()
        .map(|(name, ty)| ColumnDef {
            col: ColumnInfo { name: name.clone(), data_type: ty.clone(), nullable: true, ..Default::default() },
            original: None,
        })
        .collect();
    def
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::render;

    fn guess(values: &[&str], kind: DbKind) -> String {
        let mut g = Guess::new(1);
        for v in values {
            g.add(&[Some(v.to_string())]);
        }
        render(kind, g.types(kind)[0], false)
    }

    #[test]
    fn types() {
        let pg = DbKind::Postgres;
        assert_eq!(guess(&["1", "-20", ""], pg), "integer");
        assert_eq!(guess(&["1", "3000000000"], pg), "bigint");
        assert_eq!(guess(&["1.5", "-123.25", "7"], pg), "numeric(5,2)");
        assert_eq!(guess(&["0.001"], pg), "numeric(3,3)");
        assert_eq!(guess(&["1e10", "2.5"], pg), "double precision");
        assert_eq!(guess(&["007", "12"], pg), "varchar(255)");
        assert_eq!(guess(&["true", "FALSE"], pg), "boolean");
        assert_eq!(guess(&["true", "false"], DbKind::MySql), "varchar(255)");
        assert_eq!(guess(&["2024-02-29", "1999-12-31"], pg), "date");
        assert_eq!(guess(&["2024-02-30"], pg), "varchar(255)");
        assert_eq!(guess(&["2024-02-29 10:00:00", "2024-03-01", "2024-03-01T09:30:15.123"], pg), "timestamp");
        assert_eq!(guess(&["2024-02-29 10:00:00+03"], pg), "varchar(255)");
        assert_eq!(guess(&["café au lait", "Ünïcödé text"], pg), "varchar(255)");
        assert_eq!(guess(&["0b2c6f2e-6d5c-4a8e-9a3f-1c2d3e4f5a6b"], pg), "uuid");
        assert_eq!(guess(&["0x01FF", "0x"], pg), "bytea");
        assert_eq!(guess(&[&"x".repeat(300)], pg), "text");
        assert_eq!(guess(&[], pg), "varchar(255)");
        assert_eq!(guess(&["1", "2"], DbKind::ClickHouse), "Int32");
    }

    #[test]
    fn names() {
        let file: Vec<String> = ["id", " Name ", "", "ID", "name"].iter().map(|s| s.to_string()).collect();
        assert_eq!(column_names(&file), ["id", "Name", "column3", "ID_2", "name_2"]);
    }
}
