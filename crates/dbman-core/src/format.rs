//! A SQL formatter in the style of DataGrip's defaults: one clause per line,
//! select lists and `AND` / `OR` conditions on lines of their own, subqueries
//! indented, and keywords in upper case.
//!
//! Only queries, DML, `CREATE TABLE` and `CREATE VIEW` are laid out again.
//! Anything else (procedure bodies, triggers, `GRANT`, …) is kept as written,
//! and so are strings and comments. The tokens never change apart from the
//! case of keywords, so the formatted script means the same.

use std::ops::Range;

use crate::DbKind;
use crate::sql::{TokenKind, tokenize};

const INDENT: &str = "    ";

/// Operators written with a space on both sides.
const SPACED: &[&str] = &[
    "=", "<", ">", "<=", ">=", "<>", "!=", "+", "-", "*", "/", "%", "||", "->", "->>", "#>", "#>>", ":=", "=>", "<=>",
    "@>", "<@", "&&", "~", "~*", "!~", "!~*", "|", "&", "^",
];

/// Characters that make up operators.
const OP_CHARS: &str = "=<>!+-*/%|&^~:#@?";

/// Words that are not in the highlighter's keyword list but are written in upper case.
const EXTRA_UPPER: &[&str] = &[
    "except",
    "recursive",
    "qualify",
    "lateral",
    "minus",
    "prewhere",
    "top",
    "exclude",
    "conflict",
    "do",
    "nothing",
    "duplicate",
    "nulls",
    "rows",
    "only",
    "ties",
];

/// Words that start a clause on a new line.
const CLAUSES: &[&str] = &[
    "where",
    "having",
    "limit",
    "offset",
    "fetch",
    "returning",
    "window",
    "qualify",
    "prewhere",
    "values",
    "settings",
    "engine",
];

/// Words that may stand between a join's start and `JOIN`.
const JOIN_WORDS: &[&str] = &[
    "inner", "left", "right", "full", "cross", "natural", "outer", "global", "any", "all", "asof", "semi", "anti",
    "array",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Comment,
    String,
    Ident,
    Number,
    Keyword,
    Word,
    Semicolon,
    Go,
    /// One or more operator characters.
    Op,
    Punct,
}

#[derive(Debug, Clone)]
struct Tok<'a> {
    kind: Kind,
    text: &'a str,
    range: Range<usize>,
    /// No whitespace between this token and the one before.
    glued: bool,
    /// A line break in the whitespace before.
    newline: bool,
}

impl Tok<'_> {
    fn lower(&self) -> String {
        if matches!(self.kind, Kind::Keyword | Kind::Word) { self.text.to_ascii_lowercase() } else { String::new() }
    }

    fn is(&self, text: &str) -> bool {
        self.kind == Kind::Punct && self.text == text
    }

    fn line_comment(&self) -> bool {
        self.kind == Kind::Comment && !self.text.starts_with("/*")
    }
}

fn lex(sql: &str, kind: DbKind) -> Vec<Tok<'_>> {
    let mut out: Vec<Tok> = Vec::new();
    let (mut glued, mut newline) = (true, false);
    for t in tokenize(sql, kind) {
        let text = &sql[t.range.clone()];
        let k = match t.kind {
            TokenKind::Whitespace => {
                glued = false;
                newline |= text.contains('\n');
                continue;
            }
            TokenKind::Comment => Kind::Comment,
            TokenKind::String => Kind::String,
            TokenKind::QuotedIdent => Kind::Ident,
            TokenKind::Number => Kind::Number,
            TokenKind::Keyword => Kind::Keyword,
            TokenKind::Word => Kind::Word,
            TokenKind::Semicolon => Kind::Semicolon,
            TokenKind::BatchSeparator => Kind::Go,
            TokenKind::Punct if text.chars().all(|c| OP_CHARS.contains(c)) => Kind::Op,
            TokenKind::Punct => Kind::Punct,
        };
        let tok = Tok { kind: k, text, range: t.range, glued: glued && !out.is_empty(), newline };
        (glued, newline) = (true, false);
        if let Some(prev) = out.last_mut() {
            // `>=`, `::`, `->>` and the like come as one character each.
            let op_run = prev.kind == Kind::Op && k == Kind::Op && tok.glued;
            // `1e-5` comes as `1e`, `-`, `5`.
            let exponent = tok.glued
                && ((prev.kind == Kind::Number && prev.text.ends_with(['e', 'E']) && matches!(text, "-" | "+"))
                    || (k == Kind::Number && prev.kind == Kind::Op && exponent_sign(&out)));
            if op_run || exponent {
                let prev = out.last_mut().unwrap();
                prev.range.end = tok.range.end;
                prev.text = &sql[prev.range.clone()];
                if exponent {
                    prev.kind = Kind::Number;
                }
                continue;
            }
        }
        out.push(tok);
    }
    out
}

/// Whether the last token is the sign of an exponent, as in `1e-`.
fn exponent_sign(out: &[Tok]) -> bool {
    out.len() >= 2 && {
        let (n, sign) = (&out[out.len() - 2], &out[out.len() - 1]);
        n.kind == Kind::Number && n.text.ends_with(['e', 'E']) && sign.glued && matches!(sign.text, "-" | "+")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Query,
    CreateTable,
    Verbatim,
}

fn mode_of(toks: &[Tok]) -> Mode {
    let mut words = toks.iter().filter(|t| t.kind != Kind::Comment);
    let Some(first) = words.next() else { return Mode::Verbatim };
    if first.is("(") {
        return Mode::Query;
    }
    match first.lower().as_str() {
        "select" | "with" | "insert" | "update" | "delete" | "values" | "explain" => Mode::Query,
        "create" => {
            for t in words {
                match t.lower().as_str() {
                    "or" | "replace" | "temporary" | "temp" | "unlogged" | "materialized" | "global" | "local"
                    | "if" | "not" | "exists" | "recursive" => {}
                    "table" => return Mode::CreateTable,
                    "view" => return Mode::Query,
                    _ => return Mode::Verbatim,
                }
            }
            Mode::Verbatim
        }
        _ => Mode::Verbatim,
    }
}

/// One statement's tokens, and how it ended.
struct Statement<'a> {
    toks: Vec<Tok<'a>>,
    semicolon: bool,
    go: bool,
}

fn statements<'a>(toks: Vec<Tok<'a>>) -> Vec<Statement<'a>> {
    let mut out = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    // `BEGIN … END` blocks of routines and triggers hold `;` that do not end the statement.
    let mut block = 0usize;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].clone();
        match t.kind {
            Kind::Semicolon if block == 0 => {
                out.push(Statement { toks: std::mem::take(&mut cur), semicolon: true, go: false });
            }
            Kind::Go => {
                block = 0;
                out.push(Statement { toks: std::mem::take(&mut cur), semicolon: false, go: true });
            }
            _ => {
                if mode_of(&cur) == Mode::Verbatim && cur.first().is_some_and(|f| f.lower() == "create") {
                    match t.lower().as_str() {
                        "begin" | "case" => block += 1,
                        "end" => {
                            let next = toks.get(i + 1).map(Tok::lower).unwrap_or_default();
                            if !matches!(next.as_str(), "if" | "loop" | "while" | "repeat") {
                                block = block.saturating_sub(1);
                            }
                        }
                        _ => {}
                    }
                }
                cur.push(t);
            }
        }
        i += 1;
    }
    if !cur.is_empty() {
        out.push(Statement { toks: cur, semicolon: false, go: false });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// The statement itself, or a subquery in parentheses: clauses break lines.
    Block,
    /// The column list of `CREATE TABLE`: one item per line.
    List,
    /// Any other parentheses: kept on one line.
    Inline,
}

struct Level {
    frame: Frame,
    /// Indent of the clauses.
    base: usize,
    /// Indent of the closing parenthesis.
    close: usize,
    clause: &'static str,
    started: bool,
    case_depth: usize,
    between: bool,
}

impl Level {
    fn new(frame: Frame, base: usize, close: usize) -> Self {
        Level { frame, base, close, clause: "", started: false, case_depth: 0, between: false }
    }
}

struct Out {
    s: String,
    indent: usize,
    line_start: bool,
    /// A line comment was written, so the next token starts a line.
    pending: bool,
}

impl Out {
    fn newline(&mut self, indent: usize) {
        let trimmed = self.s.trim_end_matches(' ').len();
        self.s.truncate(trimmed);
        if !self.s.is_empty() {
            self.s.push('\n');
        }
        for _ in 0..indent {
            self.s.push_str(INDENT);
        }
        self.indent = indent;
        self.line_start = true;
        self.pending = false;
    }

    fn push(&mut self, text: &str, space: bool) {
        if self.pending {
            let indent = self.indent;
            self.newline(indent);
        }
        if space && !self.line_start {
            self.s.push(' ');
        }
        self.s.push_str(text);
        self.line_start = false;
    }
}

/// Whether a space goes between `prev` and `cur`.
fn space(prev: Option<&Tok>, cur: &Tok, unary: bool) -> bool {
    let Some(prev) = prev else { return false };
    if [",", ")", ".", "]"].iter().any(|p| cur.is(p)) || cur.kind == Kind::Semicolon {
        return false;
    }
    if prev.is("(") || prev.is(".") || prev.is("[") || unary {
        return false;
    }
    if prev.is(",") {
        return true;
    }
    let spaced = |t: &Tok| t.kind == Kind::Op && SPACED.contains(&t.text);
    if prev.text == "::" || cur.text == "::" {
        return false;
    }
    if spaced(prev) || spaced(cur) {
        return true;
    }
    !cur.glued
}

/// Whether `tok`, an operator after `prev`, is a sign rather than a binary operator.
fn is_unary(prev: Option<&Tok>, tok: &Tok) -> bool {
    if !matches!(tok.text, "-" | "+" | "~") {
        return false;
    }
    match prev {
        None => true,
        Some(p) => match p.kind {
            Kind::Op | Kind::Semicolon => true,
            Kind::Punct => p.is("(") || p.is(","),
            Kind::Keyword => {
                !matches!(p.lower().as_str(), "null" | "true" | "false" | "end" | "current_date" | "current_timestamp")
            }
            _ => false,
        },
    }
}

fn upper(tok: &Tok, prev: Option<&Tok>, next: Option<&Tok>, kind: DbKind) -> String {
    let dotted = prev.is_some_and(|p| p.is(".")) || next.is_some_and(|n| n.is(".") && n.glued);
    let word = tok.kind == Kind::Keyword || (tok.kind == Kind::Word && EXTRA_UPPER.contains(&tok.lower().as_str()));
    // ClickHouse names are case-sensitive, and some of them look like keywords.
    if word && !dotted && kind != DbKind::ClickHouse { tok.text.to_ascii_uppercase() } else { tok.text.to_string() }
}

fn layout(toks: &[Tok], kind: DbKind, mode: Mode) -> String {
    let mut out = Out { s: String::new(), indent: 0, line_start: true, pending: false };
    let mut levels = vec![Level::new(Frame::Block, 0, 0)];
    let first = toks.iter().find(|t| t.kind != Kind::Comment).map(Tok::lower).unwrap_or_default();
    levels[0].clause = if first == "with" { "with" } else { "" };
    let mut list_done = false;
    let mut prev: Option<&Tok> = None;
    let mut prev_word = String::new();
    let mut unary = false;
    let significant = |from: usize| toks[from..].iter().find(|t| t.kind != Kind::Comment);

    for (i, tok) in toks.iter().enumerate() {
        let next = significant(i + 1);
        let lower = if prev.is_some_and(|p| p.is(".")) { String::new() } else { tok.lower() };
        let lv = levels.last_mut().unwrap();
        let breaks = lv.frame == Frame::Block && lv.case_depth == 0;

        if tok.kind == Kind::Comment {
            if tok.newline && !out.line_start {
                let indent = if lv.frame == Frame::Inline { out.indent } else { lv.base };
                out.newline(indent);
            }
            out.push(tok.text, true);
            out.pending = tok.line_comment();
            continue;
        }

        // A new line before clauses.
        let clause_break = |out: &mut Out, lv: &mut Level, clause: &'static str| {
            if lv.started {
                out.newline(lv.base);
            }
            lv.clause = clause;
        };
        match lower.as_str() {
            "case" => lv.case_depth += 1,
            "end" => lv.case_depth = lv.case_depth.saturating_sub(1),
            "between" => lv.between = true,
            _ => {}
        }
        if breaks {
            let next_lower = next.map(Tok::lower).unwrap_or_default();
            match lower.as_str() {
                "select" => clause_break(&mut out, lv, "select"),
                "from" if !matches!(prev_word.as_str(), "delete" | "distinct") => clause_break(&mut out, lv, "from"),
                "values" if prev_word == "default" => {}
                w if CLAUSES.contains(&w) => clause_break(&mut out, lv, "other"),
                "group" | "order" | "partition" if next_lower == "by" => clause_break(&mut out, lv, "other"),
                "union" | "intersect" | "except" | "minus" => clause_break(&mut out, lv, "other"),
                "set" if first == "update" || prev_word == "update" => clause_break(&mut out, lv, "set"),
                "join" if !JOIN_WORDS.contains(&prev_word.as_str()) => clause_break(&mut out, lv, "join"),
                w if JOIN_WORDS.contains(&w) && !JOIN_WORDS.contains(&prev_word.as_str()) && leads_to_join(toks, i) => {
                    clause_break(&mut out, lv, "join")
                }
                "on" if matches!(next_lower.as_str(), "conflict" | "duplicate") => clause_break(&mut out, lv, "other"),
                "on" if lv.clause == "join" => lv.clause = "on",
                "and" if lv.between => lv.between = false,
                "and" | "or" if matches!(lv.clause, "where" | "on" | "having") => out.newline(lv.base + 1),
                _ => {}
            }
            if lower == "where" || lower == "having" {
                lv.clause = if lower == "where" { "where" } else { "having" };
            }
            if lower == "values" && prev_word != "default" {
                lv.clause = "values";
            }
        } else if lower == "and" && lv.between {
            lv.between = false;
        }

        let text = upper(tok, prev, toks.get(i + 1), kind);
        if tok.is("(") {
            out.push(&text, space(prev, tok, unary));
            let next_lower = next.map(Tok::lower).unwrap_or_default();
            let indent = out.indent;
            let lv = levels.last_mut().unwrap();
            lv.started = true;
            let frame = if matches!(next_lower.as_str(), "select" | "with") {
                Frame::Block
            } else if mode == Mode::CreateTable && !list_done && levels.len() == 1 {
                list_done = true;
                Frame::List
            } else {
                Frame::Inline
            };
            levels.push(Level::new(frame, indent + 1, indent));
            if frame != Frame::Inline && !next.is_some_and(|n| n.is(")")) {
                out.newline(indent + 1);
            }
        } else if tok.is(")") {
            if levels.len() > 1 {
                let lv = levels.pop().unwrap();
                if lv.frame != Frame::Inline && lv.started {
                    out.newline(lv.close);
                }
            }
            out.push(&text, false);
            levels.last_mut().unwrap().started = true;
        } else {
            // A comma after a line comment starts its line, and that is break enough.
            let own_line = out.pending || out.line_start;
            out.push(&text, space(prev, tok, unary));
            let lv = levels.last_mut().unwrap();
            lv.started = true;
            if tok.is(",") && lv.case_depth == 0 && !own_line {
                match (lv.frame, lv.clause) {
                    (Frame::List, _) => out.newline(lv.base),
                    (Frame::Block, "select" | "set" | "values") => out.newline(lv.base + 1),
                    (Frame::Block, "with") => out.newline(lv.base),
                    _ => {}
                }
            }
        }
        unary = tok.kind == Kind::Op && is_unary(prev, tok);
        if matches!(tok.kind, Kind::Keyword | Kind::Word) {
            prev_word = lower;
        } else {
            prev_word.clear();
        }
        prev = Some(tok);
    }
    out.s.trim_end().to_string()
}

/// Whether the join words starting at `i` end in `JOIN`.
fn leads_to_join(toks: &[Tok], i: usize) -> bool {
    for t in &toks[i..] {
        let w = t.lower();
        if w == "join" {
            return true;
        }
        if !JOIN_WORDS.contains(&w.as_str()) {
            return false;
        }
    }
    false
}

/// `sql` laid out again; see the module documentation.
pub fn format_sql(sql: &str, kind: DbKind) -> String {
    let mut out = String::new();
    for stmt in statements(lex(sql, kind)) {
        let mut toks = stmt.toks.as_slice();
        // A comment on the line of the previous statement's `;` stays there.
        while let Some(t) = toks.first().filter(|t| t.kind == Kind::Comment && !t.newline && !out.is_empty()) {
            out.push(' ');
            out.push_str(t.text);
            toks = &toks[1..];
        }
        if !toks.is_empty() {
            let mode = mode_of(toks);
            let text = match mode {
                Mode::Verbatim => sql[toks[0].range.start..toks[toks.len() - 1].range.end].to_string(),
                _ => layout(toks, kind, mode),
            };
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
            // A line comment at the end would swallow the `;`.
            if stmt.semicolon && toks.last().is_some_and(Tok::line_comment) {
                out.push('\n');
            }
        }
        if stmt.semicolon {
            out.push(';');
        }
        if stmt.go {
            out.push_str("\nGO");
        }
    }
    if sql.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(sql: &str) -> String {
        format_sql(sql, DbKind::Postgres)
    }

    /// The tokens other than whitespace, with keywords in lower case.
    fn words(sql: &str, kind: DbKind) -> Vec<String> {
        tokenize(sql, kind)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| {
                let s = &sql[t.range];
                if t.kind == TokenKind::Keyword { s.to_ascii_lowercase() } else { s.to_string() }
            })
            .collect()
    }

    const SAMPLES: &[(DbKind, &str)] = &[
        (
            DbKind::Postgres,
            "select a, b as \"B\", count(*) from t join u on u.id=t.uid and u.x>=1 where a between 1 and 2 or b in (select id from v where w is distinct from 3) group by a, b having count(*)>1 order by 1 desc limit 10 offset 5",
        ),
        (
            DbKind::Postgres,
            "with recursive r as (select 1 n union all select n+1 from r where n<10), s as (select * from r) select x::int, e'a\\'b', $1, -1, 1e-5, a->>'k' from s",
        ),
        (
            DbKind::Postgres,
            "insert into t (a, b) values (1, 'x'), (2, 'y') on conflict (a) do update set b = excluded.b returning *",
        ),
        (
            DbKind::Postgres,
            "create table t (id serial primary key, name text not null default 'x', price numeric(10, 2), unique (name)); create or replace function f() returns int as $$ select 1; $$ language sql",
        ),
        (
            DbKind::MySql,
            "update t set a = 1, b = b+1 where id = @id; select `a`, \"s\" from t left outer join u using (id) # note\n where x like 'a%'",
        ),
        (
            DbKind::MsSql,
            "select top 10 [a], n'x' from dbo.t with (nolock) where a = @p\nGO\ncreate procedure p as begin select 1; select 2; end\nGO",
        ),
        (
            DbKind::Sqlite,
            "create trigger tr after insert on t begin update u set n = n + 1; end; select case when a > 0 then 'p' else 'n' end from t -- tail",
        ),
        (
            DbKind::ClickHouse,
            "select key, count() from t array join arr prewhere x = 1 where y = 2 group by key settings max_threads = 2",
        ),
        (
            DbKind::DuckDb,
            "select * exclude (a) from read_parquet('x.parquet') qualify row_number() over (partition by a order by b) = 1",
        ),
    ];

    #[test]
    fn keeps_every_token() {
        for (kind, sql) in SAMPLES {
            let f = format_sql(sql, *kind);
            let lower = |v: Vec<String>| v.into_iter().map(|s| s.to_ascii_lowercase()).collect::<Vec<_>>();
            assert_eq!(lower(words(&f, *kind)), lower(words(sql, *kind)), "{kind:?}\n{f}");
        }
    }

    #[test]
    fn formatting_twice_changes_nothing() {
        for (kind, sql) in SAMPLES {
            let f = format_sql(sql, *kind);
            assert_eq!(format_sql(&f, *kind), f, "{kind:?}");
        }
    }

    #[test]
    fn query_layout() {
        assert_eq!(
            pg("select a, b from t join u on u.id=t.uid and u.x>=1 where a between 1 and 2 or b is null order by a"),
            "SELECT a,\n    b\nFROM t\nJOIN u ON u.id = t.uid\n    AND u.x >= 1\nWHERE a BETWEEN 1 AND 2\n    OR b IS NULL\nORDER BY a"
        );
    }

    #[test]
    fn subqueries_are_indented() {
        assert_eq!(
            pg("select * from t where id in (select uid from u where x = 1) and exists(select 1)"),
            "SELECT *\nFROM t\nWHERE id IN (\n    SELECT uid\n    FROM u\n    WHERE x = 1\n)\n    AND EXISTS(\n        SELECT 1\n    )"
        );
        assert_eq!(
            pg("with a as (select 1), b as (select 2) select * from a, b"),
            "WITH a AS (\n    SELECT 1\n),\nb AS (\n    SELECT 2\n)\nSELECT *\nFROM a, b"
        );
    }

    #[test]
    fn dml_layout() {
        assert_eq!(pg("insert into t(a,b) values (1,2),(3,4)"), "INSERT INTO t(a, b)\nVALUES (1, 2),\n    (3, 4)");
        assert_eq!(pg("update t set a=1,b=2 where id=3"), "UPDATE t\nSET a = 1,\n    b = 2\nWHERE id = 3");
        assert_eq!(pg("delete from t where id=3"), "DELETE FROM t\nWHERE id = 3");
        assert_eq!(
            pg("insert into t values (1) on conflict (a) do update set b = excluded.b returning *"),
            "INSERT INTO t\nVALUES (1)\nON CONFLICT (a) DO UPDATE\nSET b = excluded.b\nRETURNING *"
        );
    }

    #[test]
    fn create_table_has_a_column_per_line() {
        assert_eq!(
            pg("create table t (id int primary key, n numeric(10,2) not null)"),
            "CREATE TABLE t (\n    id int PRIMARY KEY,\n    n numeric(10, 2) NOT NULL\n)"
        );
    }

    #[test]
    fn functions_and_operators() {
        assert_eq!(
            pg("select count( * ), -1, x::text, a || b, t.*, $1"),
            "SELECT count(*),\n    -1,\n    x::text,\n    a || b,\n    t.*,\n    $1"
        );
        assert_eq!(pg("select 1e-5, a->>'k', e'x'"), "SELECT 1e-5,\n    a ->> 'k',\n    e'x'");
        assert_eq!(
            pg("select case when a>0 then 1 else -1 end, b"),
            "SELECT CASE WHEN a > 0 THEN 1 ELSE -1 END,\n    b"
        );
    }

    #[test]
    fn other_statements_are_kept() {
        let body = "create function f() returns int as $$\n  select 1;\n$$ language sql";
        assert_eq!(pg(body), body);
        let trigger = "create trigger tr after insert on t begin\n  update u set n = n + 1;\nend";
        assert_eq!(format_sql(&format!("{trigger};"), DbKind::Sqlite), format!("{trigger};"));
        assert_eq!(pg("grant  select on t to bob"), "grant  select on t to bob");
    }

    #[test]
    fn statements_and_comments() {
        assert_eq!(pg("select 1; select 2;\n"), "SELECT 1;\n\nSELECT 2;\n");
        assert_eq!(pg("select 1; -- one\nselect 2"), "SELECT 1; -- one\n\nSELECT 2");
        assert_eq!(pg("select a -- first\n, b from t"), "SELECT a -- first\n, b\nFROM t");
        assert_eq!(pg("select 1 -- end\n;"), "SELECT 1 -- end\n;");
        assert_eq!(format_sql("select 1\nGO\nselect 2", DbKind::MsSql), "SELECT 1\nGO\n\nSELECT 2");
    }

    #[test]
    fn clickhouse_keeps_case() {
        assert_eq!(format_sql("select key from t where x = 1", DbKind::ClickHouse), "select key\nfrom t\nwhere x = 1");
    }
}
