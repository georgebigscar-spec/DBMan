//! SQL text helpers: a small tokenizer (for highlighting and splitting
//! scripts into statements) and identifier/literal quoting per dialect.

use std::ops::Range;

use crate::{DbKind, ObjectPath};

/// Lexical class of a [`Token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Whitespace,
    Comment,
    /// A string literal, including dollar-quoted bodies.
    String,
    /// `"ident"` or `` `ident` ``.
    QuotedIdent,
    Number,
    Keyword,
    Word,
    Semicolon,
    /// A `GO` line in SQL Server scripts; it ends a batch.
    BatchSeparator,
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

/// Splits `sql` into tokens covering the whole text.
pub fn tokenize(sql: &str, kind: DbKind) -> Vec<Token> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mysql = kind == DbKind::MySql;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let tk = if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            TokenKind::Whitespace
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') || mysql && c == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            TokenKind::Comment
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else if matches!(kind, DbKind::Postgres | DbKind::DuckDb)
                    && b[i] == b'/'
                    && b.get(i + 1) == Some(&b'*')
                {
                    depth += 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            TokenKind::Comment
        } else if c == b'\'' {
            let backslash = mysql || (start > 0 && matches!(b[start - 1], b'e' | b'E'));
            i = skip_quoted(b, i, b'\'', backslash);
            TokenKind::String
        } else if c == b'"' {
            i = skip_quoted(b, i, b'"', mysql);
            if mysql { TokenKind::String } else { TokenKind::QuotedIdent }
        } else if c == b'`' {
            i = skip_quoted(b, i, b'`', false);
            TokenKind::QuotedIdent
        } else if c == b'[' && matches!(kind, DbKind::Sqlite | DbKind::MsSql) {
            i += 1;
            while i < b.len() {
                if b[i] == b']' {
                    // `]]` is an escaped bracket in SQL Server.
                    if kind == DbKind::MsSql && b.get(i + 1) == Some(&b']') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            TokenKind::QuotedIdent
        } else if c == b'$' && matches!(kind, DbKind::Postgres | DbKind::DuckDb) && dollar_tag(b, i).is_some() {
            let tag_end = dollar_tag(b, i).unwrap();
            let tag = &b[i..tag_end];
            i = tag_end;
            loop {
                if i >= b.len() {
                    break;
                }
                if b[i..].starts_with(tag) {
                    i += tag.len();
                    break;
                }
                i += 1;
            }
            TokenKind::String
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            TokenKind::Number
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            let word = &sql[start..i];
            if kind == DbKind::MsSql && word.eq_ignore_ascii_case("go") && alone_on_line(b, start, i) {
                TokenKind::BatchSeparator
            } else if is_keyword(word) {
                TokenKind::Keyword
            } else {
                TokenKind::Word
            }
        } else if c == b';' {
            i += 1;
            TokenKind::Semicolon
        } else {
            i += 1;
            while i < b.len() && !sql.is_char_boundary(i) {
                i += 1;
            }
            TokenKind::Punct
        };
        out.push(Token { kind: tk, range: start..i });
    }
    out
}

/// Only whitespace around `start..end` on its line.
fn alone_on_line(b: &[u8], start: usize, end: usize) -> bool {
    let before = b[..start].iter().rev().take_while(|c| **c != b'\n').all(u8::is_ascii_whitespace);
    let after = b[end..].iter().take_while(|c| **c != b'\n').all(u8::is_ascii_whitespace);
    before && after
}

fn skip_quoted(b: &[u8], mut i: usize, q: u8, backslash: bool) -> usize {
    i += 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// End of a `$tag$` opener starting at `i`, if there is one.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    // `$1` is a parameter, not a tag.
    if b.get(j) == Some(&b'$') && !b.get(i + 1).is_some_and(u8::is_ascii_digit) { Some(j + 1) } else { None }
}

const KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "analyze",
    "and",
    "any",
    "as",
    "asc",
    "auto_increment",
    "autoincrement",
    "begin",
    "between",
    "by",
    "cascade",
    "case",
    "cast",
    "check",
    "column",
    "commit",
    "constraint",
    "create",
    "cross",
    "current_date",
    "current_timestamp",
    "database",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "explain",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "full",
    "function",
    "grant",
    "group",
    "having",
    "if",
    "ilike",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "language",
    "left",
    "like",
    "limit",
    "materialized",
    "natural",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "over",
    "partition",
    "primary",
    "procedure",
    "references",
    "rename",
    "replace",
    "returning",
    "returns",
    "revoke",
    "right",
    "rollback",
    "schema",
    "select",
    "sequence",
    "set",
    "show",
    "table",
    "temporary",
    "then",
    "to",
    "transaction",
    "trigger",
    "true",
    "truncate",
    "union",
    "unique",
    "update",
    "use",
    "using",
    "vacuum",
    "values",
    "view",
    "when",
    "where",
    "window",
    "with",
];

pub fn is_keyword(word: &str) -> bool {
    word.len() <= 17 && KEYWORDS.binary_search(&word.to_ascii_lowercase().as_str()).is_ok()
}

/// Byte ranges of the statements in a script, trimmed, without the trailing `;`.
/// Empty statements (only whitespace and comments) are skipped.
///
/// A SQL Server script with `GO` lines is split only at them, since procedure
/// bodies contain `;`.
pub fn split_statements(sql: &str, kind: DbKind) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut first: Option<usize> = None;
    let mut last = 0;
    let tokens = tokenize(sql, kind);
    let batches = tokens.iter().any(|t| t.kind == TokenKind::BatchSeparator);
    for t in tokens {
        match t.kind {
            TokenKind::Semicolon if batches => {
                first.get_or_insert(t.range.start);
                last = t.range.end;
            }
            TokenKind::Semicolon | TokenKind::BatchSeparator => {
                if let Some(f) = first.take() {
                    out.push(f..last);
                }
            }
            TokenKind::Whitespace | TokenKind::Comment => {}
            _ => {
                first.get_or_insert(t.range.start);
                last = t.range.end;
            }
        }
    }
    if let Some(f) = first {
        out.push(f..last);
    }
    out
}

/// The statement under the cursor (byte offset), or the closest one before it.
pub fn statement_at(sql: &str, kind: DbKind, cursor: usize) -> Option<Range<usize>> {
    let stmts = split_statements(sql, kind);
    // A cursor right after `;` or in the blank lines after a statement still means that statement.
    stmts
        .iter()
        .find(|r| cursor >= r.start && cursor <= r.end)
        .or_else(|| stmts.iter().rev().find(|r| r.end <= cursor))
        .or_else(|| stmts.first())
        .cloned()
}

/// Quoting rules of one engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect(pub DbKind);

impl Dialect {
    /// Quotes an identifier.
    pub fn ident(self, name: &str) -> String {
        match self.0 {
            DbKind::MySql => format!("`{}`", name.replace('`', "``")),
            DbKind::MsSql => format!("[{}]", name.replace(']', "]]")),
            DbKind::Postgres | DbKind::Sqlite | DbKind::DuckDb => format!("\"{}\"", name.replace('"', "\"\"")),
        }
    }

    /// `schema.name`, quoted, as used in queries (just `name` without a schema).
    pub fn qualified(self, path: &ObjectPath) -> String {
        if path.schema.is_empty() {
            return self.ident(&path.name);
        }
        format!("{}.{}", self.ident(&path.schema), self.ident(&path.name))
    }

    /// A string literal.
    pub fn string(self, value: &str) -> String {
        match self.0 {
            DbKind::MySql => format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''")),
            DbKind::MsSql => format!("N'{}'", value.replace('\'', "''")),
            DbKind::Postgres | DbKind::Sqlite | DbKind::DuckDb => format!("'{}'", value.replace('\'', "''")),
        }
    }

    /// `SELECT * FROM … LIMIT … OFFSET …` with optional filter and ordering.
    pub fn select_page(self, path: &ObjectPath, filter: &str, order_by: &str, limit: usize, offset: usize) -> String {
        let mut sql = format!("SELECT * FROM {}", self.qualified(path));
        if !filter.trim().is_empty() {
            sql.push_str(&format!(" WHERE {}", filter.trim()));
        }
        if !order_by.trim().is_empty() {
            sql.push_str(&format!(" ORDER BY {}", order_by.trim()));
        } else if self.0 == DbKind::MsSql {
            // OFFSET … FETCH needs an ORDER BY.
            sql.push_str(" ORDER BY (SELECT NULL)");
        }
        if self.0 == DbKind::MsSql {
            sql.push_str(&format!(" OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"));
        } else {
            sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
        }
        sql
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmts(sql: &str, kind: DbKind) -> Vec<&str> {
        split_statements(sql, kind).into_iter().map(|r| &sql[r]).collect()
    }

    #[test]
    fn keyword_list_is_sorted() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn splits_simple_script() {
        let sql = "select 1;\n\n-- note; here\nselect 'a;b' ; /* x; */ ;  update t set a = 2";
        assert_eq!(stmts(sql, DbKind::Postgres), ["select 1", "select 'a;b'", "update t set a = 2"]);
    }

    #[test]
    fn dollar_quotes_keep_semicolons() {
        let sql = "create function f() returns int as $body$ begin; return 1; end $body$ language plpgsql; select $1";
        let s = stmts(sql, DbKind::Postgres);
        assert_eq!(s.len(), 2);
        assert!(s[0].ends_with("plpgsql"));
        assert_eq!(s[1], "select $1");
    }

    #[test]
    fn mysql_quoting_and_hash_comments() {
        let sql = "select `a;b`, \"x;\\\"y\" from t # c;\n; select 'it\\'s;'";
        assert_eq!(stmts(sql, DbKind::MySql).len(), 2);
    }

    #[test]
    fn statement_under_cursor() {
        let sql = "select 1;\nselect 2;\n\nselect 3";
        let at = |c| &sql[statement_at(sql, DbKind::Sqlite, c).unwrap()];
        assert_eq!(at(0), "select 1");
        assert_eq!(at(12), "select 2");
        assert_eq!(at(20), "select 2");
        assert_eq!(at(sql.len()), "select 3");
    }

    #[test]
    fn quoting() {
        let pg = Dialect(DbKind::Postgres);
        assert_eq!(pg.ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect(DbKind::MySql).ident("a`b"), "`a``b`");
        assert_eq!(Dialect(DbKind::MySql).string("it's \\"), "'it''s \\\\'");
        let p = ObjectPath::new("db", "public", "users");
        assert_eq!(
            pg.select_page(&p, "id > 1", "", 10, 20),
            "SELECT * FROM \"public\".\"users\" WHERE id > 1 LIMIT 10 OFFSET 20"
        );
    }

    #[test]
    fn tokens_cover_text() {
        let sql = "SELECT ñ, \"q\" FROM t WHERE x = 'é' -- end";
        let toks = tokenize(sql, DbKind::Postgres);
        assert_eq!(toks.first().unwrap().range.start, 0);
        assert_eq!(toks.last().unwrap().range.end, sql.len());
        assert!(toks.windows(2).all(|w| w[0].range.end == w[1].range.start));
        assert_eq!(toks[0].kind, TokenKind::Keyword);
    }

    #[test]
    fn mssql_batches_and_quoting() {
        let sql = "CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END\nGO\nEXEC p;\n  go  \nSELECT [a]]b] FROM t";
        assert_eq!(
            stmts(sql, DbKind::MsSql),
            ["CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END", "EXEC p;", "SELECT [a]]b] FROM t"]
        );
        assert_eq!(stmts("SELECT 1; SELECT go FROM t", DbKind::MsSql), ["SELECT 1", "SELECT go FROM t"]);
        let d = Dialect(DbKind::MsSql);
        assert_eq!(d.ident("a]b"), "[a]]b]");
        assert_eq!(d.string("it's"), "N'it''s'");
        assert_eq!(
            d.select_page(&ObjectPath::new("db", "dbo", "t"), "", "", 10, 20),
            "SELECT * FROM [dbo].[t] ORDER BY (SELECT NULL) OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
    }
}
