//! Search in a database: object names, the text of views and routines, and
//! values in table rows. The statements that look for a value, and the pieces
//! the search tab shows.

use crate::sql::Dialect;
use crate::{DbKind, ObjectKind, ObjectPath};

/// Where a hit was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitKind {
    /// The object's name.
    Name,
    /// The object's definition: a view's query, a routine's or trigger's body.
    Definition,
    /// Rows of a table or view.
    Data,
}

impl HitKind {
    pub const fn label(self) -> &'static str {
        match self {
            HitKind::Name => "Name",
            HitKind::Definition => "Definition",
            HitKind::Data => "Data",
        }
    }
}

/// Whether `name` contains `needle`, ignoring case.
pub fn name_matches(name: &str, needle: &str) -> bool {
    !needle.is_empty() && name.to_lowercase().contains(&needle.to_lowercase())
}

/// Objects whose definition is worth reading.
pub fn has_definition(kind: ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::View
            | ObjectKind::MaterializedView
            | ObjectKind::Function
            | ObjectKind::Procedure
            | ObjectKind::Trigger
    )
}

/// Objects with rows to look through.
pub fn has_rows(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Table | ObjectKind::ForeignTable)
}

/// The first line of `text` that contains `needle` (ignoring case), trimmed
/// to about `width` characters around the match, with its line number.
pub fn excerpt(text: &str, needle: &str, width: usize) -> Option<(usize, String)> {
    if needle.is_empty() {
        return None;
    }
    let low = needle.to_lowercase();
    text.lines().enumerate().find_map(|(n, line)| {
        let lower = line.to_lowercase();
        let at = lower.find(&low)?;
        let chars: Vec<char> = line.trim_end().chars().collect();
        // `at` is a byte offset into the lowercased line; count chars up to it.
        let start_char = lower[..at].chars().count().min(chars.len());
        let from = start_char.saturating_sub(width / 3);
        let to = (from + width).min(chars.len());
        let mut s: String = chars[from..to].iter().collect::<String>().trim().to_string();
        if from > 0 {
            s.insert(0, '…');
        }
        if to < chars.len() {
            s.push('…');
        }
        Some((n + 1, s))
    })
}

/// Column types whose values are not text to search: binary data.
pub fn searchable_type(data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    !["bytea", "blob", "binary", "varbinary", "image", "geometry", "geography"].iter().any(|b| t.contains(b))
}

/// A `WHERE` condition true for rows where any of `columns`, as text,
/// contains `needle`, ignoring case where the engine allows.
pub fn value_filter(kind: DbKind, columns: &[String], needle: &str) -> String {
    let d = Dialect(kind);
    // `!` escapes the wildcards of LIKE everywhere but ClickHouse, which gets a plain search.
    let pattern = format!("%{}%", needle.replace('!', "!!").replace('%', "!%").replace('_', "!_"));
    let parts: Vec<String> = columns
        .iter()
        .map(|c| {
            let col = d.ident(c);
            match kind {
                DbKind::Postgres => format!("CAST({col} AS text) ILIKE {} ESCAPE '!'", d.string(&pattern)),
                DbKind::DuckDb => format!("CAST({col} AS VARCHAR) ILIKE {} ESCAPE '!'", d.string(&pattern)),
                DbKind::MySql => format!("CAST({col} AS CHAR) LIKE {} ESCAPE '!'", d.string(&pattern)),
                DbKind::Sqlite | DbKind::Redis | DbKind::MongoDb => {
                    format!("CAST({col} AS TEXT) LIKE {} ESCAPE '!'", d.string(&pattern))
                }
                DbKind::MsSql => format!("CAST({col} AS NVARCHAR(MAX)) LIKE {} ESCAPE '!'", d.string(&pattern)),
                DbKind::ClickHouse => format!("positionCaseInsensitiveUTF8(toString({col}), {}) > 0", d.string(needle)),
            }
        })
        .collect();
    parts.join(" OR ")
}

/// Counts the rows of `path` that `filter` selects, stopping at `cap + 1`.
pub fn count_sql(kind: DbKind, path: &ObjectPath, filter: &str, cap: usize) -> String {
    let t = Dialect(kind).qualified(path);
    let n = cap + 1;
    match kind {
        DbKind::MsSql => format!("SELECT COUNT(*) FROM (SELECT TOP {n} 1 AS x FROM {t} WHERE {filter}) AS m"),
        _ => format!("SELECT COUNT(*) FROM (SELECT 1 AS x FROM {t} WHERE {filter} LIMIT {n}) AS m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts() {
        let body = "CREATE VIEW v AS\n  SELECT id, total\n  FROM orders WHERE status = 'paid'";
        assert_eq!(excerpt(body, "ORDERS", 80), Some((3, "FROM orders WHERE status = 'paid'".into())));
        assert_eq!(excerpt(body, "nothing", 80), None);
        let long = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let (_, s) = excerpt(&long, "needle", 30).unwrap();
        assert!(s.starts_with('…') && s.ends_with('…') && s.contains("needle"), "{s}");
        assert_eq!(excerpt("ÄÖÜ needle", "NEEDLE", 80), Some((1, "ÄÖÜ needle".into())));
    }

    #[test]
    fn filters_escape_wildcards() {
        let cols = vec!["name".to_string(), "note".to_string()];
        assert_eq!(
            value_filter(DbKind::Postgres, &cols, "50%_o'k"),
            "CAST(\"name\" AS text) ILIKE '%50!%!_o''k%' ESCAPE '!' OR CAST(\"note\" AS text) ILIKE '%50!%!_o''k%' ESCAPE '!'"
        );
        assert_eq!(
            value_filter(DbKind::MsSql, &cols[..1], "a"),
            "CAST([name] AS NVARCHAR(MAX)) LIKE N'%a%' ESCAPE '!'"
        );
        assert_eq!(
            value_filter(DbKind::ClickHouse, &cols[..1], "a%"),
            "positionCaseInsensitiveUTF8(toString(`name`), 'a%') > 0"
        );
    }

    #[test]
    fn counts_are_capped() {
        let p = ObjectPath::new("db", "dbo", "t");
        assert_eq!(
            count_sql(DbKind::MsSql, &p, "x = 1", 999),
            "SELECT COUNT(*) FROM (SELECT TOP 1000 1 AS x FROM [dbo].[t] WHERE x = 1) AS m"
        );
        let p = ObjectPath::new("main", "main", "t");
        assert!(count_sql(DbKind::Sqlite, &p, "x = 1", 9).ends_with("LIMIT 10) AS m"));
        assert!(searchable_type("varchar(20)") && !searchable_type("bytea") && !searchable_type("LONGBLOB"));
    }
}
