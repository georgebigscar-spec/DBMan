//! Builds INSERT / UPDATE / DELETE statements for edits made in a data grid,
//! and query templates for a table that a console opens with.

use crate::sql::Dialect;
use crate::{Cell, ColumnInfo, DbKind, ObjectPath};

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
                DbKind::ClickHouse => format!("unhex('{hex}')"),
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
pub fn condition(d: Dialect, key: &[Value<'_>]) -> String {
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

/// A query on one table to start from in a console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryTemplate {
    Select,
    Insert,
    Update,
    Delete,
}

impl QueryTemplate {
    pub const ALL: [QueryTemplate; 4] =
        [QueryTemplate::Select, QueryTemplate::Insert, QueryTemplate::Update, QueryTemplate::Delete];

    pub fn label(self) -> &'static str {
        match self {
            QueryTemplate::Select => "SELECT",
            QueryTemplate::Insert => "INSERT",
            QueryTemplate::Update => "UPDATE",
            QueryTemplate::Delete => "DELETE",
        }
    }

    pub fn changes_data(self) -> bool {
        self != QueryTemplate::Select
    }

    /// The query on `table`, formatted. Values are `:column` parameters, so
    /// the console asks for them when it runs. Without `columns` (they could
    /// not be read) the query has placeholders to fill in instead.
    pub fn sql(self, kind: DbKind, table: &ObjectPath, columns: &[ColumnInfo]) -> String {
        use crate::complete::ident;
        let t = if table.schema.is_empty() {
            ident(kind, &table.name)
        } else {
            format!("{}.{}", ident(kind, &table.schema), ident(kind, &table.name))
        };
        let names = |cols: &[&ColumnInfo]| cols.iter().map(|c| ident(kind, &c.name)).collect::<Vec<_>>().join(", ");
        let equals = |cols: &[&ColumnInfo], sep: &str| {
            cols.iter().map(|c| format!("{} = :{}", ident(kind, &c.name), param(&c.name))).collect::<Vec<_>>().join(sep)
        };
        let all: Vec<&ColumnInfo> = columns.iter().collect();
        let mut key: Vec<&ColumnInfo> = all.iter().copied().filter(|c| c.primary_key).collect();
        if key.is_empty() {
            key = all.iter().copied().take(1).collect();
        }
        let filter = if key.is_empty() { "column = :value".to_string() } else { equals(&key, " AND ") };
        let sql = match self {
            QueryTemplate::Select if all.is_empty() => format!("SELECT * FROM {t}"),
            QueryTemplate::Select => format!("SELECT {} FROM {t}", names(&all)),
            QueryTemplate::Insert => {
                let given: Vec<&ColumnInfo> = all.iter().copied().filter(|c| !c.auto_increment).collect();
                if given.is_empty() && !all.is_empty() {
                    match kind {
                        DbKind::MySql => format!("INSERT INTO {t} () VALUES ()"),
                        _ => format!("INSERT INTO {t} DEFAULT VALUES"),
                    }
                } else if given.is_empty() {
                    format!("INSERT INTO {t} (column) VALUES (:value)")
                } else {
                    let values = given.iter().map(|c| format!(":{}", param(&c.name))).collect::<Vec<_>>();
                    format!("INSERT INTO {t} ({}) VALUES ({})", names(&given), values.join(", "))
                }
            }
            QueryTemplate::Update => {
                let mut set: Vec<&ColumnInfo> = all
                    .iter()
                    .copied()
                    .filter(|c| !c.auto_increment && !key.iter().any(|k| k.name == c.name))
                    .collect();
                if set.is_empty() {
                    set = all.iter().copied().filter(|c| !c.auto_increment).collect();
                }
                let set = if set.is_empty() { "column = :value".to_string() } else { equals(&set, ", ") };
                match kind {
                    // ClickHouse changes rows with mutations.
                    DbKind::ClickHouse => format!("ALTER TABLE {t} UPDATE {set} WHERE {filter}"),
                    _ => format!("UPDATE {t} SET {set} WHERE {filter}"),
                }
            }
            QueryTemplate::Delete => match kind {
                DbKind::ClickHouse => format!("ALTER TABLE {t} DELETE WHERE {filter}"),
                _ => format!("DELETE FROM {t} WHERE {filter}"),
            },
        };
        crate::format::format_sql(&format!("{sql};"), kind) + "\n"
    }
}

/// A parameter name for a column: letters, digits and `_` only.
fn param(column: &str) -> String {
    let mut p: String = column.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    if !p.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        p.insert(0, '_');
    }
    p
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

    fn columns() -> Vec<ColumnInfo> {
        let col = |name: &str, pk: bool, auto: bool| ColumnInfo {
            name: name.into(),
            data_type: "int".into(),
            primary_key: pk,
            auto_increment: auto,
            ..Default::default()
        };
        vec![col("id", true, true), col("Name", false, false), col("unit price", false, false)]
    }

    #[test]
    fn query_templates() {
        let pg = |q: QueryTemplate| q.sql(DbKind::Postgres, &t(), &columns());
        assert_eq!(pg(QueryTemplate::Select), "SELECT id,\n    \"Name\",\n    \"unit price\"\nFROM public.users;\n");
        assert_eq!(
            pg(QueryTemplate::Insert),
            "INSERT INTO public.users (\"Name\", \"unit price\")\nVALUES (:Name, :unit_price);\n"
        );
        assert_eq!(
            pg(QueryTemplate::Update),
            "UPDATE public.users\nSET \"Name\" = :Name,\n    \"unit price\" = :unit_price\nWHERE id = :id;\n"
        );
        assert_eq!(pg(QueryTemplate::Delete), "DELETE FROM public.users\nWHERE id = :id;\n");
        let ch = QueryTemplate::Delete.sql(DbKind::ClickHouse, &ObjectPath::new("db", "", "events"), &columns());
        assert_eq!(ch, "ALTER TABLE events DELETE WHERE id = :id;\n");
        // Unknown columns leave placeholders.
        assert_eq!(QueryTemplate::Select.sql(DbKind::MySql, &t(), &[]), "SELECT *\nFROM public.users;\n");
        // Every value is a parameter the console asks for.
        let sql = QueryTemplate::Update.sql(DbKind::MsSql, &t(), &columns());
        let names = crate::params::names(&crate::params::find(DbKind::MsSql, &sql));
        assert_eq!(names, ["Name", "unit_price", "id"]);
    }
}
