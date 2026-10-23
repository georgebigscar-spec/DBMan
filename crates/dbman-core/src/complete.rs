//! SQL autocomplete: what fits at the cursor (tables after `FROM`, columns
//! of the statement's tables, a table's columns after `alias.`, keywords),
//! from a catalog of the objects the explorer has loaded.

use std::ops::Range;

use crate::DbKind;
use crate::sql::{self, Dialect, TokenKind};

/// A table or view the completer knows of; `columns` once they are loaded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogTable {
    pub schema: String,
    pub name: String,
    pub view: bool,
    /// Names and types.
    pub columns: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub kind: Option<DbKind>,
    /// Schemas of the database; empty on engines without them.
    pub schemas: Vec<String>,
    /// Where unqualified names are looked up.
    pub default_schema: String,
    pub tables: Vec<CatalogTable>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemKind {
    /// A statement template; `$0` in its insert marks where the cursor goes.
    Snippet,
    Column,
    Table,
    View,
    Schema,
    Function,
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// Text that replaces the word being typed.
    pub insert: String,
    pub kind: ItemKind,
    /// A column's type, a table's schema.
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    /// Byte range of the word being typed.
    pub range: Range<usize>,
    pub items: Vec<Item>,
    /// Tables (schema, name) whose columns would be offered once loaded.
    pub wants: Vec<(String, String)>,
    /// Schemas whose tables would be offered once loaded.
    pub want_schemas: Vec<String>,
}

/// Statement templates offered at the start of a statement: (label, insert).
pub const SNIPPETS: &[(&str, &str)] = &[
    ("sel", "SELECT * FROM $0"),
    ("selw", "SELECT * FROM $0 WHERE "),
    ("cnt", "SELECT count(*) FROM $0"),
    ("ins", "INSERT INTO $0 () VALUES ()"),
    ("upd", "UPDATE $0 SET  WHERE "),
    ("del", "DELETE FROM $0 WHERE "),
];

/// The cursor marker in a snippet's insert.
pub const CURSOR: &str = "$0";

const FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "cast",
    "ceil",
    "coalesce",
    "concat",
    "count",
    "floor",
    "length",
    "lower",
    "max",
    "min",
    "nullif",
    "round",
    "substring",
    "sum",
    "trim",
    "upper",
];

/// Keywords after which a table name comes.
const TABLE_BEFORE: &[&str] = &["from", "join", "update", "into", "table", "describe", "desc", "truncate"];

/// Clause keywords: a comma after `FROM` lists tables, elsewhere columns.
const CLAUSES: &[&str] =
    &["select", "from", "where", "group", "order", "having", "set", "on", "values", "limit", "returning", "using"];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `name` as written in SQL: bare when that reads the same, quoted otherwise.
pub fn ident(kind: DbKind, name: &str) -> String {
    let mut chars = name.chars();
    let plain = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !sql::is_keyword(name)
        // PostgreSQL and DuckDB fold bare names to lower case.
        && !(matches!(kind, DbKind::Postgres) && name.chars().any(|c| c.is_ascii_uppercase()));
    if plain { name.to_string() } else { Dialect(kind).ident(name) }
}

/// The text of an identifier token, unquoted.
fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    match (b.first(), b.last()) {
        (Some(b'"'), Some(b'"')) | (Some(b'`'), Some(b'`')) | (Some(b'['), Some(b']')) if s.len() >= 2 => {
            s[1..s.len() - 1].to_string()
        }
        _ => s.to_string(),
    }
}

/// A table named in the statement, with its alias.
#[derive(Debug, Clone, PartialEq)]
struct TableRef {
    schema: Option<String>,
    name: String,
    alias: Option<String>,
}

/// Significant tokens of a statement: (kind, text lower-cased for keywords, original text).
fn significant(stmt: &str, kind: DbKind) -> Vec<(TokenKind, String)> {
    let mut out: Vec<(TokenKind, String)> = Vec::new();
    for t in sql::tokenize(stmt, kind) {
        let text = &stmt[t.range.clone()];
        match t.kind {
            TokenKind::Whitespace | TokenKind::Comment => {}
            // `[name]` comes out as punctuation and a word on SQL Server; join it back.
            TokenKind::Punct if text == "[" => out.push((TokenKind::QuotedIdent, text.to_string())),
            TokenKind::Punct
                if text == "]"
                    && out.last().is_some_and(|l| {
                        l.0 == TokenKind::QuotedIdent && l.1.starts_with('[') && !l.1.ends_with(']')
                    }) =>
            {
                out.last_mut().unwrap().1.push(']');
            }
            _ if out
                .last()
                .is_some_and(|l| l.0 == TokenKind::QuotedIdent && l.1.starts_with('[') && !l.1.ends_with(']')) =>
            {
                out.last_mut().unwrap().1.push_str(text);
            }
            k => out.push((k, text.to_string())),
        }
    }
    out
}

fn is_name(t: &(TokenKind, String)) -> bool {
    matches!(t.0, TokenKind::Word | TokenKind::QuotedIdent)
}

fn is_kw(t: &(TokenKind, String), words: &[&str]) -> bool {
    t.0 == TokenKind::Keyword && words.contains(&t.1.to_ascii_lowercase().as_str())
}

/// The tables after FROM, JOIN, UPDATE and INTO, with their aliases.
fn table_refs(tokens: &[(TokenKind, String)]) -> Vec<TableRef> {
    let mut refs = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !is_kw(&tokens[i], &["from", "join", "update", "into"]) {
            i += 1;
            continue;
        }
        let in_from = is_kw(&tokens[i], &["from"]);
        i += 1;
        while let Some(first) = tokens.get(i).filter(|t| is_name(t)) {
            let (mut schema, mut name) = (None, unquote(&first.1));
            i += 1;
            if tokens.get(i).is_some_and(|t| t.1 == ".") && tokens.get(i + 1).is_some_and(is_name) {
                schema = Some(name);
                name = unquote(&tokens[i + 1].1);
                i += 2;
            }
            if tokens.get(i).is_some_and(|t| is_kw(t, &["as"])) {
                i += 1;
            }
            let alias = tokens.get(i).filter(|t| is_name(t)).map(|t| unquote(&t.1));
            if alias.is_some() {
                i += 1;
            }
            refs.push(TableRef { schema, name, alias });
            if in_from && tokens.get(i).is_some_and(|t| t.1 == ",") {
                i += 1;
                continue;
            }
            break;
        }
    }
    refs
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl Catalog {
    fn has_schemas(&self) -> bool {
        !self.schemas.is_empty()
    }

    fn find(&self, schema: Option<&str>, name: &str) -> Option<&CatalogTable> {
        let schema = schema.unwrap_or(&self.default_schema);
        self.tables
            .iter()
            .find(|t| same(&t.name, name) && (!self.has_schemas() || same(&t.schema, schema)))
            .or_else(|| self.tables.iter().find(|t| same(&t.name, name)))
    }
}

/// Matches `prefix`: 0 for a prefix match, 1 for a match inside, `None` for none.
fn rank(label: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(0);
    }
    let (l, p) = (label.to_lowercase(), prefix.to_lowercase());
    if l.starts_with(&p) {
        Some(0)
    } else if p.chars().count() >= 2 && l.contains(&p) {
        Some(1)
    } else {
        None
    }
}

/// Gathers the items that match the word being typed.
struct Collect<'a> {
    prefix: &'a str,
    kind: DbKind,
    items: Vec<(u8, Item)>,
    wants: Vec<(String, String)>,
    want_schemas: Vec<String>,
}

impl Collect<'_> {
    fn push(&mut self, label: String, insert: String, kind: ItemKind, detail: String) {
        if let Some(r) = rank(&label, self.prefix) {
            self.items.push((r, Item { label, insert, kind, detail }));
        }
    }

    /// A table's columns, or a request to load them.
    fn columns(&mut self, t: &CatalogTable) {
        match &t.columns {
            Some(cols) => {
                for (name, ty) in cols {
                    let insert = ident(self.kind, name);
                    self.push(name.clone(), insert, ItemKind::Column, ty.clone());
                }
            }
            None => self.wants.push((t.schema.clone(), t.name.clone())),
        }
    }
}

/// What fits at byte `cursor` of `sql`. Without `explicit` (Ctrl+Space),
/// nothing is offered until a letter or a `.` is typed.
pub fn complete(sql: &str, cursor: usize, kind: DbKind, catalog: &Catalog, explicit: bool) -> Option<Completion> {
    if cursor > sql.len() || !sql.is_char_boundary(cursor) {
        return None;
    }
    // Not inside a string or a comment.
    let inside = sql::tokenize(sql, kind).into_iter().find(|t| t.range.start < cursor && cursor <= t.range.end);
    if let Some(t) = &inside {
        let text = &sql[t.range.clone()];
        let open_line_comment = t.kind == TokenKind::Comment && text.starts_with("--") && cursor == t.range.end;
        if matches!(t.kind, TokenKind::String | TokenKind::QuotedIdent)
            && !(cursor == t.range.end && text.len() >= 2 && text.ends_with(text.chars().next().unwrap_or(' ')))
            || (t.kind == TokenKind::Comment && (cursor < t.range.end || open_line_comment || !text.ends_with("*/")))
        {
            return None;
        }
    }
    let start =
        sql[..cursor].char_indices().rev().take_while(|(_, c)| is_ident_char(*c)).last().map_or(cursor, |(i, _)| i);
    let prefix = &sql[start..cursor];
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let before = &sql[..start];
    let qualifier = before.strip_suffix('.').map(|b| {
        let q_end = b.len();
        let q_start = match b.chars().last() {
            Some(c @ ('"' | '`' | ']')) => {
                let open = if c == ']' { '[' } else { c };
                b[..q_end - 1].rfind(open).unwrap_or(0)
            }
            _ => b.char_indices().rev().take_while(|(_, c)| is_ident_char(*c)).last().map_or(q_end, |(i, _)| i),
        };
        unquote(&b[q_start..q_end])
    });
    let qualifier = qualifier.filter(|q| !q.is_empty());
    if !explicit && prefix.is_empty() && qualifier.is_none() {
        return None;
    }

    let stmt_range = sql::statement_at(sql, kind, cursor).unwrap_or(0..sql.len());
    let stmt_range = stmt_range.start.min(cursor)..stmt_range.end.max(cursor);
    let tokens = significant(&sql[stmt_range.clone()], kind);
    let refs = table_refs(&tokens);
    // Significant tokens before the word being typed (and its qualifier).
    let head_end =
        if qualifier.is_some() { before.trim_end_matches('.').len() } else { start } - stmt_range.start.min(start);
    let head_end = head_end.min(cursor - stmt_range.start);
    let mut head = significant(&sql[stmt_range.start..stmt_range.start + head_end], kind);
    if qualifier.is_some() {
        head.pop();
    }
    let prev = head.last();
    let clause =
        head.iter().rev().find(|t| is_kw(t, CLAUSES) || is_kw(t, TABLE_BEFORE)).map(|t| t.1.to_ascii_lowercase());
    let wants_tables = prev.is_some_and(|p| is_kw(p, TABLE_BEFORE))
        || (prev.is_some_and(|p| p.1 == ",") && clause.as_deref() == Some("from"));
    // After a name or a value comes an alias or a keyword (`orders o`, `name FROM`): only keywords,
    // and not for a single letter, which is most likely an alias.
    let after_value = qualifier.is_none()
        && prev.is_some_and(|p| is_name(p) || matches!(p.0, TokenKind::Number | TokenKind::String) || p.1 == ")");
    if after_value && !explicit && prefix.chars().count() < 2 {
        return None;
    }

    let mut c = Collect { prefix, kind, items: Vec::new(), wants: Vec::new(), want_schemas: Vec::new() };
    let table_item = |t: &CatalogTable, qualified: bool| {
        let k = if t.view { ItemKind::View } else { ItemKind::Table };
        let insert = if qualified && catalog.has_schemas() && !same(&t.schema, &catalog.default_schema) {
            format!("{}.{}", ident(kind, &t.schema), ident(kind, &t.name))
        } else {
            ident(kind, &t.name)
        };
        (t.name.clone(), insert, k, if catalog.has_schemas() { t.schema.clone() } else { String::new() })
    };
    let in_default = |t: &&CatalogTable| !catalog.has_schemas() || same(&t.schema, &catalog.default_schema);

    if let Some(q) = &qualifier {
        let r = refs
            .iter()
            .find(|r| r.alias.as_deref().is_some_and(|a| same(a, q)) || (r.alias.is_none() && same(&r.name, q)));
        if let Some(t) = r.and_then(|r| catalog.find(r.schema.as_deref(), &r.name)) {
            c.columns(t);
        } else if let Some(schema) = catalog.schemas.iter().find(|s| same(s, q)) {
            let mut any = false;
            for t in catalog.tables.iter().filter(|t| same(&t.schema, schema)) {
                any = true;
                let (l, i, k, d) = table_item(t, false);
                c.push(l, i, k, d);
            }
            if !any {
                c.want_schemas.push(schema.clone());
            }
        } else if let Some(t) = catalog.find(None, q) {
            c.columns(t);
        } else if let Some(r) = r {
            c.wants.push((r.schema.clone().unwrap_or_else(|| catalog.default_schema.clone()), r.name.clone()));
        }
    } else if after_value {
        let lower = prefix.chars().all(|c| !c.is_ascii_uppercase());
        for k in sql::keywords() {
            let k = if lower { k.to_string() } else { k.to_uppercase() };
            c.push(k.clone(), k, ItemKind::Keyword, String::new());
        }
    } else if wants_tables {
        for t in catalog.tables.iter().filter(in_default) {
            let (l, i, k, d) = table_item(t, true);
            c.push(l, i, k, d);
        }
        for s in &catalog.schemas {
            c.push(s.clone(), ident(kind, s), ItemKind::Schema, String::new());
        }
    } else {
        let mut seen: Vec<&CatalogTable> = Vec::new();
        for r in &refs {
            if let Some(t) = catalog.find(r.schema.as_deref(), &r.name)
                && !seen.iter().any(|s| std::ptr::eq(*s, t))
            {
                seen.push(t);
                c.columns(t);
            }
        }
        if head.is_empty() && !prefix.is_empty() {
            for (label, insert) in SNIPPETS {
                let detail = insert.replace(CURSOR, "…").trim().to_string();
                c.push(label.to_string(), insert.to_string(), ItemKind::Snippet, detail);
            }
        }
        if !prefix.is_empty() || explicit {
            for t in catalog.tables.iter().filter(in_default) {
                let (l, i, k, d) = table_item(t, true);
                c.push(l, i, k, d);
            }
            let lower = prefix.chars().all(|c| !c.is_ascii_uppercase()) && !prefix.is_empty();
            for f in FUNCTIONS {
                let f = if lower { f.to_string() } else { f.to_uppercase() };
                c.push(f.clone(), format!("{f}("), ItemKind::Function, String::new());
            }
            for k in sql::keywords() {
                let k = if lower { k.to_string() } else { k.to_uppercase() };
                c.push(k.clone(), k, ItemKind::Keyword, String::new());
            }
        }
    }
    let mut out = Completion { range: start..cursor, items: Vec::new(), wants: c.wants, want_schemas: c.want_schemas };
    let mut items = c.items;
    items.sort_by_key(|(r, i)| (*r, i.kind, i.label.to_lowercase()));
    let mut labels = std::collections::HashSet::new();
    out.items = items
        .into_iter()
        .map(|(_, i)| i)
        .filter(|i| labels.insert((i.label.to_lowercase(), i.kind)))
        .take(200)
        .collect();
    out.wants.dedup();
    // A finished word needs no popup.
    if !explicit && out.items.len() == 1 && same(&out.items[0].label, prefix) {
        out.items.clear();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let cols = |c: &[&str]| Some(c.iter().map(|n| (n.to_string(), "integer".to_string())).collect());
        Catalog {
            kind: Some(DbKind::Postgres),
            schemas: vec!["public".into(), "sales".into()],
            default_schema: "public".into(),
            tables: vec![
                CatalogTable {
                    schema: "public".into(),
                    name: "customers".into(),
                    view: false,
                    columns: cols(&["id", "name", "email"]),
                },
                CatalogTable {
                    schema: "public".into(),
                    name: "orders".into(),
                    view: false,
                    columns: cols(&["id", "customer_id", "total"]),
                },
                CatalogTable { schema: "public".into(), name: "order lines".into(), view: false, columns: None },
                CatalogTable { schema: "sales".into(), name: "leads".into(), view: true, columns: None },
            ],
        }
    }

    /// Completes at `|`.
    fn at(sql: &str) -> Option<Completion> {
        let cursor = sql.find('|').unwrap();
        let sql = sql.replace('|', "");
        complete(&sql, cursor, DbKind::Postgres, &catalog(), false)
    }

    fn labels(c: &Option<Completion>) -> Vec<&str> {
        c.as_ref().unwrap().items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn tables_after_from() {
        let c = at("select * from or|");
        assert_eq!(labels(&c)[..2], ["order lines", "orders"]);
        assert_eq!(c.as_ref().unwrap().items[0].insert, "\"order lines\"");
        assert_eq!(c.unwrap().range, 14..16);
        let c = at("SELECT * FROM customers c JOIN o|");
        assert_eq!(labels(&c)[..2], ["order lines", "orders"]);
        let c = at("select * from customers, o|");
        assert_eq!(labels(&c)[..2], ["order lines", "orders"]);
        assert_eq!(labels(&at("select * from sa|")), ["sales"]);
    }

    #[test]
    fn columns_of_aliases_and_tables() {
        assert_eq!(labels(&at("select o.| from orders o")), ["customer_id", "id", "total"]);
        assert_eq!(labels(&at("select o.t| from orders as o")), ["total"]);
        assert_eq!(labels(&at("select * from orders where orders.c|")), ["customer_id"]);
        let c = at("select c.na| from customers c join orders o on o.customer_id = c.id");
        assert_eq!(labels(&c), ["name"]);
        // The statement's columns come before tables and keywords.
        let c = at("select na| from customers");
        assert_eq!(labels(&c)[0], "name");
        assert!(labels(&c).contains(&"NATURAL") || labels(&c).contains(&"natural"), "{:?}", labels(&c));
    }

    #[test]
    fn loads_what_is_missing() {
        let c = at("select l.| from sales.leads l").unwrap();
        assert!(c.items.is_empty());
        assert_eq!(c.wants, [("sales".to_string(), "leads".to_string())]);
        assert_eq!(labels(&at("select * from sales.|")), ["leads"]);
    }

    #[test]
    fn quiet_where_nothing_fits() {
        assert!(at("select 'or|'").is_none());
        assert!(at("-- from or|").is_none());
        assert!(at("select |").is_none());
        assert!(at("select 12|").is_none());
        // A finished word.
        assert!(at("select * from orders|").unwrap().items.is_empty());
        let c = complete("select ", 7, DbKind::Postgres, &catalog(), true).unwrap();
        assert!(c.items.iter().any(|i| i.label == "SELECT"));
    }

    #[test]
    fn keywords_after_a_name() {
        assert!(at("select * from orders o|").is_none());
        assert_eq!(labels(&at("select name fr|")), ["from"]);
        assert_eq!(labels(&at("select * from orders WHE|")), ["WHEN", "WHERE"]);
    }

    #[test]
    fn sql_server_brackets() {
        let mut cat = catalog();
        cat.schemas = vec!["dbo".into()];
        cat.default_schema = "dbo".into();
        for t in &mut cat.tables {
            t.schema = "dbo".into();
        }
        let sql = "select x. from [dbo].[orders] x";
        let c = complete(sql, 9, DbKind::MsSql, &cat, false).unwrap();
        assert_eq!(c.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["customer_id", "id", "total"]);
    }

    #[test]
    fn quoting() {
        assert_eq!(ident(DbKind::Postgres, "orders"), "orders");
        assert_eq!(ident(DbKind::Postgres, "Orders"), "\"Orders\"");
        assert_eq!(ident(DbKind::MySql, "Orders"), "Orders");
        assert_eq!(ident(DbKind::MySql, "select"), "`select`");
        assert_eq!(ident(DbKind::MsSql, "order lines"), "[order lines]");
    }

    #[test]
    fn snippets_start_a_statement() {
        let c = at("sel|").unwrap();
        assert_eq!(c.items[0].kind, ItemKind::Snippet);
        assert_eq!(c.items[0].insert, "SELECT * FROM $0");
        assert!(labels(&at("sel|")).contains(&"select"));
        let c = at("select 1; up|").unwrap();
        assert_eq!(c.items[0].label, "upd");
        // Not inside a statement.
        assert!(at("select * from orders where del|").unwrap().items.iter().all(|i| i.kind != ItemKind::Snippet));
    }
}
