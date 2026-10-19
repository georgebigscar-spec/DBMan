//! Parameters in console queries: `:name`, `${name}` and, where `?` is no
//! operator, positional `?`. They are found outside strings and comments and
//! replaced by the values the user types, as written.

use std::ops::Range;

use crate::DbKind;
use crate::sql::{TokenKind, tokenize};

/// One place a parameter appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// `id` for `:id` and `${id}`; `?1`, `?2`… for positional ones, counted per statement.
    pub name: String,
    /// The text it replaces, including `:`, `${ }` or `?`.
    pub range: Range<usize>,
}

/// The parameters of a statement, in order.
pub fn find(kind: DbKind, sql: &str) -> Vec<Param> {
    let tokens = tokenize(sql, kind);
    let text = |i: usize| tokens.get(i).map_or("", |t| &sql[t.range.clone()]);
    let adjacent = |a: usize, b: usize| tokens[a].range.end == tokens[b].range.start;
    let is_name = |i: usize| tokens.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Word | TokenKind::Keyword));
    // PostgreSQL uses `?` in JSON operators, and ClickHouse in the ternary operator.
    let positional = !matches!(kind, DbKind::Postgres | DbKind::ClickHouse);
    let mut out = Vec::new();
    let mut n = 0;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t.kind != TokenKind::Punct {
            i += 1;
            continue;
        }
        match text(i) {
            ":" if is_name(i + 1) && adjacent(i, i + 1) => {
                // Not `::type` casts, slices like `a[1:n]`, DuckDB's `{'k':v}`
                // or ClickHouse's `{id:UInt32}`.
                let after_value = i > 0
                    && adjacent(i - 1, i)
                    && (matches!(
                        tokens[i - 1].kind,
                        TokenKind::Word
                            | TokenKind::Keyword
                            | TokenKind::Number
                            | TokenKind::String
                            | TokenKind::QuotedIdent
                    ) || matches!(text(i - 1), ":" | "]" | ")"));
                if !after_value {
                    let end = tokens[i + 1].range.end;
                    out.push(Param { name: text(i + 1).to_string(), range: t.range.start..end });
                    i += 2;
                    continue;
                }
            }
            "$" if text(i + 1) == "{" && is_name(i + 2) && text(i + 3) == "}" && adjacent(i, i + 1) => {
                let end = tokens[i + 3].range.end;
                out.push(Param { name: text(i + 2).to_string(), range: t.range.start..end });
                i += 4;
                continue;
            }
            "?" if positional => {
                n += 1;
                out.push(Param { name: format!("?{n}"), range: t.range.clone() });
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Parameter names in the order they first appear.
pub fn names(params: &[Param]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in params {
        if !out.contains(&p.name) {
            out.push(p.name.clone());
        }
    }
    out
}

/// `sql` with each parameter replaced by its value from `value`.
pub fn substitute(kind: DbKind, sql: &str, value: impl Fn(&str) -> String) -> String {
    let mut out = sql.to_string();
    for p in find(kind, sql).into_iter().rev() {
        out.replace_range(p.range, &value(&p.name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(kind: DbKind, sql: &str) -> Vec<String> {
        find(kind, sql).into_iter().map(|p| p.name).collect()
    }

    #[test]
    fn finds_named_parameters() {
        let sql =
            "SELECT * FROM t WHERE id = :id AND city = ${city} -- :not_this\n AND note <> ':nor_this' AND id <> :id";
        assert_eq!(found(DbKind::Postgres, sql), ["id", "city", "id"]);
        assert_eq!(names(&find(DbKind::Postgres, sql)), ["id", "city"]);
        assert_eq!(found(DbKind::Postgres, "SELECT x::text, a[1:n], (:v)"), ["v"]);
        assert_eq!(found(DbKind::Postgres, "SELECT $1, $$ :x $$"), Vec::<String>::new());
        assert_eq!(found(DbKind::MySql, "SET @a := 1; SELECT :b"), ["b"]);
        assert_eq!(found(DbKind::DuckDb, "SELECT {'k':v}"), Vec::<String>::new());
        assert_eq!(found(DbKind::ClickHouse, "SELECT {id:UInt32}"), Vec::<String>::new());
    }

    #[test]
    fn positional_only_where_question_mark_is_free() {
        assert_eq!(found(DbKind::MySql, "SELECT * FROM t WHERE a = ? AND b = ?"), ["?1", "?2"]);
        assert_eq!(found(DbKind::Postgres, "SELECT data ? 'key' FROM t"), Vec::<String>::new());
        assert_eq!(found(DbKind::ClickHouse, "SELECT a > 1 ? 'x' : 'y'"), Vec::<String>::new());
    }

    #[test]
    fn substitutes_values_as_written() {
        let sql = "SELECT * FROM t WHERE id = :id OR parent = :id AND city = ${city}";
        let out = substitute(DbKind::Postgres, sql, |n| if n == "id" { "42".into() } else { "'Berlin'".into() });
        assert_eq!(out, "SELECT * FROM t WHERE id = 42 OR parent = 42 AND city = 'Berlin'");
        let out = substitute(DbKind::Sqlite, "VALUES (?, ?)", |n| n.trim_start_matches('?').to_string());
        assert_eq!(out, "VALUES (1, 2)");
    }
}
