//! Builds INSERT / UPDATE / DELETE statements for edits made in a data grid.

use crate::sql::Dialect;
use crate::{Cell, DbKind, ObjectPath};

/// A column value used in a statement.
#[derive(Debug, Clone, Copy)]
pub struct Value<'a> {
    pub column: &'a str,
    pub cell: &'a Cell,
    /// Numeric columns get bare numbers instead of quoted strings.
    pub numeric: bool,
}

impl<'a> Value<'a> {
    pub fn new(column: &'a str, cell: &'a Cell, numeric: bool) -> Self {
        Self { column, cell, numeric }
    }
}

/// A SQL literal for `cell`.
pub fn literal(d: Dialect, cell: &Cell, numeric: bool) -> String {
    match cell {
        Cell::Null => "NULL".into(),
        Cell::Text(s) if numeric && is_plain_number(s) => s.clone(),
        Cell::Text(s) => d.string(s),
        Cell::Bytes(b) => {
            let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
            match d.0 {
                DbKind::Postgres => format!("'\\x{hex}'::bytea"),
                DbKind::MySql | DbKind::Sqlite => format!("X'{hex}'"),
                DbKind::MsSql => format!("0x{hex}"),
                DbKind::DuckDb => {
                    let escaped: String = b.iter().map(|x| format!("\\x{x:02X}")).collect();
                    format!("'{escaped}'::BLOB")
                }
            }
        }
    }
}

fn is_plain_number(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.parse::<f64>().is_ok()
        && s.chars().all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
}

/// `a = 1 AND b IS NULL`
fn condition(d: Dialect, key: &[Value<'_>]) -> String {
    key.iter()
        .map(|v| match v.cell {
            Cell::Null => format!("{} IS NULL", d.ident(v.column)),
            c => format!("{} = {}", d.ident(v.column), literal(d, c, v.numeric)),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub fn update(d: Dialect, table: &ObjectPath, set: &[Value<'_>], key: &[Value<'_>]) -> String {
    let assignments = set
        .iter()
        .map(|v| format!("{} = {}", d.ident(v.column), literal(d, v.cell, v.numeric)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("UPDATE {} SET {assignments} WHERE {}", d.qualified(table), condition(d, key))
}

pub fn delete(d: Dialect, table: &ObjectPath, key: &[Value<'_>]) -> String {
    format!("DELETE FROM {} WHERE {}", d.qualified(table), condition(d, key))
}

/// An INSERT of the given values; with no values, a row of defaults.
pub fn insert(d: Dialect, table: &ObjectPath, values: &[Value<'_>]) -> String {
    if values.is_empty() {
        return match d.0 {
            DbKind::MySql => format!("INSERT INTO {} () VALUES ()", d.qualified(table)),
            _ => format!("INSERT INTO {} DEFAULT VALUES", d.qualified(table)),
        };
    }
    let cols = values.iter().map(|v| d.ident(v.column)).collect::<Vec<_>>().join(", ");
    let vals = values.iter().map(|v| literal(d, v.cell, v.numeric)).collect::<Vec<_>>().join(", ");
    format!("INSERT INTO {} ({cols}) VALUES ({vals})", d.qualified(table))
}

/// The statement that starts a transaction.
pub fn begin(kind: DbKind) -> &'static str {
    match kind {
        DbKind::MySql => "START TRANSACTION",
        DbKind::MsSql => "BEGIN TRANSACTION",
        _ => "BEGIN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> ObjectPath {
        ObjectPath::new("db", "public", "users")
    }

    #[test]
    fn update_with_key() {
        let d = Dialect(DbKind::Postgres);
        let name = Cell::Text("O'Hara".into());
        let id = Cell::Text("7".into());
        let null = Cell::Null;
        let sql = update(
            d,
            &t(),
            &[Value::new("name", &name, false)],
            &[Value::new("id", &id, true), Value::new("tenant", &null, false)],
        );
        assert_eq!(sql, "UPDATE \"public\".\"users\" SET \"name\" = 'O''Hara' WHERE \"id\" = 7 AND \"tenant\" IS NULL");
    }

    #[test]
    fn numbers_are_bare_only_when_plain() {
        let d = Dialect(DbKind::MySql);
        assert_eq!(literal(d, &Cell::Text("1.5".into()), true), "1.5");
        assert_eq!(literal(d, &Cell::Text("1; DROP".into()), true), "'1; DROP'");
        assert_eq!(literal(d, &Cell::Text("inf".into()), true), "'inf'");
        assert_eq!(literal(d, &Cell::Text("42".into()), false), "'42'");
        assert_eq!(literal(d, &Cell::Bytes(vec![1, 0xab]), false), "X'01AB'");
        assert_eq!(literal(Dialect(DbKind::Postgres), &Cell::Bytes(vec![1]), false), "'\\x01'::bytea");
    }

    #[test]
    fn insert_and_delete() {
        let d = Dialect(DbKind::Sqlite);
        let v = Cell::Text("a".into());
        assert_eq!(
            insert(d, &t(), &[Value::new("name", &v, false)]),
            "INSERT INTO \"public\".\"users\" (\"name\") VALUES ('a')"
        );
        assert_eq!(insert(d, &t(), &[]), "INSERT INTO \"public\".\"users\" DEFAULT VALUES");
        assert_eq!(insert(Dialect(DbKind::MySql), &t(), &[]), "INSERT INTO `public`.`users` () VALUES ()");
        assert_eq!(
            delete(d, &t(), &[Value::new("name", &v, false)]),
            "DELETE FROM \"public\".\"users\" WHERE \"name\" = 'a'"
        );
    }
}
