//! Redis: command lines as redis-cli reads them, which commands only read,
//! replies shown as result tables, and keys grouped by their `:` prefixes.

use std::ops::Range;

use crate::{Cell, ColumnMeta, QueryResult};

/// A reply from the server, independent of the driver.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Nil,
    /// `OK` and other simple status lines.
    Status(String),
    Int(i64),
    Double(f64),
    Bool(bool),
    Text(String),
    /// A value that is not valid UTF-8.
    Bytes(Vec<u8>),
    Array(Vec<Reply>),
    Map(Vec<(Reply, Reply)>),
}

impl Reply {
    /// The reply as one line of text; arrays and maps as JSON-like lists.
    pub fn text(&self) -> String {
        match self {
            Reply::Nil => "(nil)".into(),
            Reply::Status(s) | Reply::Text(s) => s.clone(),
            Reply::Int(i) => i.to_string(),
            Reply::Double(d) => d.to_string(),
            Reply::Bool(b) => b.to_string(),
            Reply::Bytes(b) => crate::Cell::Bytes(b.clone()).display().into_owned(),
            Reply::Array(items) => format!("[{}]", items.iter().map(Reply::text).collect::<Vec<_>>().join(", ")),
            Reply::Map(pairs) => format!(
                "{{{}}}",
                pairs.iter().map(|(k, v)| format!("{}: {}", k.text(), v.text())).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    fn cell(&self) -> Cell {
        match self {
            Reply::Nil => Cell::Null,
            Reply::Bytes(b) => Cell::Bytes(b.clone()),
            r => Cell::Text(r.text()),
        }
    }

    fn is_scalar(&self) -> bool {
        !matches!(self, Reply::Array(_) | Reply::Map(_))
    }

    /// The items of an array (or a map's keys and values in turn), for replies read as lists.
    pub fn items(&self) -> Vec<&Reply> {
        match self {
            Reply::Array(items) => items.iter().collect(),
            Reply::Map(pairs) => pairs.iter().flat_map(|(k, v)| [k, v]).collect(),
            _ => Vec::new(),
        }
    }

    /// A text or status reply's text; numbers as text too.
    pub fn as_str(&self) -> Option<String> {
        match self {
            Reply::Text(s) | Reply::Status(s) => Some(s.clone()),
            Reply::Int(i) => Some(i.to_string()),
            Reply::Double(d) => Some(d.to_string()),
            Reply::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Reply::Int(i) => Some(*i),
            r => r.as_str()?.trim().parse().ok(),
        }
    }
}

/// Splits a command line into arguments as redis-cli does: spaces separate
/// them, `"…"` takes escapes (`\n`, `\t`, `\"`, `\\`, `\xHH`), `'…'` is literal
/// but for `\'`.
pub fn parse(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut arg = String::new();
        if first == '"' || first == '\'' {
            chars.next();
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '\\' if first == '"' => match chars.next() {
                        Some('n') => arg.push('\n'),
                        Some('r') => arg.push('\r'),
                        Some('t') => arg.push('\t'),
                        Some('b') => arg.push('\u{8}'),
                        Some('a') => arg.push('\u{7}'),
                        Some('x') => {
                            let hex: String = chars.by_ref().take(2).collect();
                            let b = u8::from_str_radix(&hex, 16).map_err(|_| format!("Bad escape \\x{hex}"))?;
                            arg.push(b as char);
                        }
                        Some(c) => arg.push(c),
                        None => break,
                    },
                    '\\' if chars.peek() == Some(&'\'') => {
                        chars.next();
                        arg.push('\'');
                    }
                    c if c == first => {
                        closed = true;
                        break;
                    }
                    c => arg.push(c),
                }
            }
            if !closed {
                return Err("Unbalanced quotes".into());
            }
            if chars.peek().is_some_and(|c| !c.is_whitespace()) {
                return Err("A closing quote must be followed by a space".into());
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
        }
        args.push(arg);
    }
    Ok(args)
}

/// An argument written so that [`parse`] reads it back: as is when it is a
/// plain word, otherwise in double quotes with escapes.
pub fn quote(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\'));
    if plain {
        return arg.to_string();
    }
    let mut s = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\x{:02x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

/// A command line from its arguments.
pub fn command(args: &[&str]) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

/// The commands of a script: one per line, skipping blank lines and lines
/// starting with `#` or `--`.
pub fn split(script: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in script.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let trimmed = body.trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with("--") {
            let start = at + (body.len() - trimmed.len());
            out.push(start..at + body.trim_end().len());
        }
        at += line.len();
    }
    out
}

/// Commands that only read. Everything else counts as a change, as for SQL.
const READS: &[&str] = &[
    "bitcount",
    "bitpos",
    "dbsize",
    "dump",
    "echo",
    "exists",
    "expiretime",
    "geodist",
    "geohash",
    "geopos",
    "georadius_ro",
    "georadiusbymember_ro",
    "geosearch",
    "get",
    "getbit",
    "getrange",
    "hexists",
    "hget",
    "hgetall",
    "hkeys",
    "hlen",
    "hmget",
    "hrandfield",
    "hscan",
    "hstrlen",
    "hvals",
    "info",
    "keys",
    "lastsave",
    "lcs",
    "lindex",
    "llen",
    "lolwut",
    "lpos",
    "lrange",
    "mget",
    "pexpiretime",
    "pfcount",
    "ping",
    "pttl",
    "randomkey",
    "role",
    "scan",
    "scard",
    "sdiff",
    "select",
    "sinter",
    "sintercard",
    "sismember",
    "smembers",
    "smismember",
    "srandmember",
    "sscan",
    "strlen",
    "substr",
    "sunion",
    "time",
    "ttl",
    "type",
    "xinfo",
    "xlen",
    "xpending",
    "xrange",
    "xread",
    "xrevrange",
    "zcard",
    "zcount",
    "zdiff",
    "zinter",
    "zintercard",
    "zlexcount",
    "zmscore",
    "zrandmember",
    "zrange",
    "zrangebylex",
    "zrangebyscore",
    "zrank",
    "zrevrange",
    "zrevrangebylex",
    "zrevrangebyscore",
    "zrevrank",
    "zscan",
    "zscore",
    "zunion",
    "json.get",
    "json.mget",
    "json.type",
    "json.strlen",
    "json.arrlen",
    "json.objkeys",
    "json.objlen",
    "ft.search",
    "ft.info",
    "ft._list",
    "ts.get",
    "ts.range",
    "ts.revrange",
    "ts.info",
];

/// Whether a command only reads. `CONFIG GET`, `CLIENT LIST`, `MEMORY USAGE`
/// and the like read too; their other subcommands change things.
pub fn is_read_only(args: &[String]) -> bool {
    let Some(name) = args.first().map(|a| a.to_ascii_lowercase()) else { return true };
    let sub = args.get(1).map(|a| a.to_ascii_lowercase()).unwrap_or_default();
    match name.as_str() {
        "config" => sub == "get",
        "client" => matches!(sub.as_str(), "list" | "info" | "getname" | "id"),
        "memory" => matches!(sub.as_str(), "usage" | "stats" | "doctor"),
        "object" => matches!(sub.as_str(), "encoding" | "freq" | "idletime" | "refcount"),
        "command" => true,
        "acl" => matches!(sub.as_str(), "whoami" | "list" | "users" | "cat"),
        "slowlog" => sub == "get" || sub == "len",
        "cluster" => matches!(sub.as_str(), "info" | "nodes" | "slots" | "shards"),
        n => READS.contains(&n),
    }
}

/// A reply as a result table:
/// - a scalar is one row of `value`;
/// - a map (a hash, `CONFIG GET` in RESP3) is `key`, `value` rows;
/// - an array of scalars is `#`, `value` rows;
/// - an array of arrays (`XRANGE`, `SCAN`) is a row per array, its items in columns.
pub fn result(reply: &Reply) -> QueryResult {
    let col = |name: &str, numeric: bool| ColumnMeta { name: name.into(), type_name: String::new(), numeric };
    let (columns, rows) = match reply {
        Reply::Map(pairs) => (
            vec![col("key", false), col("value", false)],
            pairs.iter().map(|(k, v)| vec![k.cell(), v.cell()]).collect(),
        ),
        Reply::Array(items) if items.iter().all(Reply::is_scalar) => (
            vec![col("#", true), col("value", false)],
            items.iter().enumerate().map(|(i, v)| vec![Cell::Text((i + 1).to_string()), v.cell()]).collect(),
        ),
        Reply::Array(items) => {
            let width = items.iter().map(|i| if i.is_scalar() { 1 } else { i.items().len() }).max().unwrap_or(1).max(1);
            let rows: Vec<Vec<Cell>> = items
                .iter()
                .map(|i| {
                    let mut row: Vec<Cell> =
                        if i.is_scalar() { vec![i.cell()] } else { i.items().into_iter().map(Reply::cell).collect() };
                    row.resize(width, Cell::Null);
                    row
                })
                .collect();
            ((1..=width).map(|n| col(&n.to_string(), false)).collect(), rows)
        }
        r => (vec![col("value", matches!(r, Reply::Int(_) | Reply::Double(_)))], vec![vec![r.cell()]]),
    };
    QueryResult { columns, rows, truncated: false }
}

/// The database number of a database name in the explorer (`db3` → 3).
pub fn db_index(name: &str) -> Option<i64> {
    name.strip_prefix("db").unwrap_or(name).parse().ok()
}

/// A node of the key tree: a group of keys sharing a prefix, or a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNode {
    /// The last part of the name (after the prefix of the parent group).
    pub label: String,
    /// The full key, for a key; a group has none.
    pub key: Option<String>,
    /// Keys under a group, counted through its subgroups.
    pub count: usize,
    pub children: Vec<KeyNode>,
}

/// Groups sorted keys by their `sep`-separated prefixes: `user:1` and `user:2`
/// go under `user`. A group with one key is not made; that key stays whole.
pub fn tree(keys: &[String], sep: &str) -> Vec<KeyNode> {
    fn build(keys: &[(&str, &str)], sep: &str) -> Vec<KeyNode> {
        // (rest of the name, whole key), sorted by rest.
        let mut out = Vec::new();
        let mut i = 0;
        while i < keys.len() {
            let (rest, key) = keys[i];
            match rest.split_once(sep).filter(|_| !sep.is_empty()) {
                Some((head, _)) => {
                    let prefix = format!("{head}{sep}");
                    let mut j = i;
                    while j < keys.len() && keys[j].0.starts_with(&prefix) {
                        j += 1;
                    }
                    if j - i == 1 {
                        out.push(KeyNode {
                            label: rest.to_string(),
                            key: Some(key.to_string()),
                            count: 1,
                            children: vec![],
                        });
                    } else {
                        let inner: Vec<(&str, &str)> =
                            keys[i..j].iter().map(|(r, k)| (&r[prefix.len()..], *k)).collect();
                        let children = build(&inner, sep);
                        out.push(KeyNode { label: head.to_string(), key: None, count: j - i, children });
                    }
                    i = j;
                }
                None => {
                    out.push(KeyNode {
                        label: rest.to_string(),
                        key: Some(key.to_string()),
                        count: 1,
                        children: vec![],
                    });
                    i += 1;
                }
            }
        }
        out
    }
    let mut sorted: Vec<&str> = keys.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let pairs: Vec<(&str, &str)> = sorted.iter().map(|k| (*k, *k)).collect();
    build(&pairs, sep)
}

/// "12s", "5m 3s", "2h 1m", "3d 4h" for a TTL in seconds; "no expiry" for -1.
pub fn ttl_label(seconds: i64) -> String {
    match seconds {
        s if s < 0 => "no expiry".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_like_redis_cli() {
        assert_eq!(parse("SET  user:1 \"Alice Smith\"").unwrap(), s(&["SET", "user:1", "Alice Smith"]));
        assert_eq!(parse(r#"set k "a\nb\x41\"" 'it\'s'"#).unwrap(), s(&["set", "k", "a\nbA\"", "it's"]));
        assert_eq!(parse("  ").unwrap(), Vec::<String>::new());
        assert_eq!(parse(r#"get "" x"#).unwrap(), s(&["get", "", "x"]));
        assert!(parse("get \"open").is_err());
        assert!(parse("get \"a\"b").is_err());
        for arg in ["plain", "with space", "q\"uote", "back\\slash", "", "line\nbreak", "tab\t", "'"] {
            assert_eq!(parse(&command(&["SET", arg])).unwrap(), s(&["SET", arg]), "{arg:?}");
        }
    }

    #[test]
    fn script_lines() {
        let script = "GET a\n\n  # note\n  SET b 1  \r\n-- x\nPING";
        let parts: Vec<&str> = split(script).into_iter().map(|r| &script[r]).collect();
        assert_eq!(parts, ["GET a", "SET b 1", "PING"]);
    }

    #[test]
    fn reads_and_changes() {
        assert!(is_read_only(&s(&["GET", "a"])));
        assert!(is_read_only(&s(&["hgetall", "h"])));
        assert!(is_read_only(&s(&["CONFIG", "GET", "maxmemory"])));
        assert!(!is_read_only(&s(&["CONFIG", "SET", "maxmemory", "1"])));
        assert!(!is_read_only(&s(&["SET", "a", "1"])));
        assert!(!is_read_only(&s(&["FLUSHALL"])));
        assert!(!is_read_only(&s(&["EVAL", "return 1", "0"])));
        assert!(is_read_only(&s(&["client", "list"])));
        assert!(!is_read_only(&s(&["client", "kill", "1"])));
    }

    #[test]
    fn replies_as_tables() {
        let r = result(&Reply::Text("hi".into()));
        assert_eq!((r.columns[0].name.as_str(), r.rows.clone()), ("value", vec![vec![Cell::Text("hi".into())]]));
        assert_eq!(result(&Reply::Nil).rows, vec![vec![Cell::Null]]);
        let r = result(&Reply::Map(vec![(Reply::Text("name".into()), Reply::Text("Alice".into()))]));
        assert_eq!(r.columns.len(), 2);
        assert_eq!(r.rows[0][1], Cell::Text("Alice".into()));
        let r = result(&Reply::Array(vec![Reply::Text("a".into()), Reply::Int(2)]));
        assert_eq!(r.rows[1], vec![Cell::Text("2".into()), Cell::Text("2".into())]);
        // XRANGE: [id, [field, value…]] per entry.
        let entry = Reply::Array(vec![
            Reply::Text("1-0".into()),
            Reply::Array(vec![Reply::Text("f".into()), Reply::Text("v".into())]),
        ]);
        let r = result(&Reply::Array(vec![entry]));
        assert_eq!(r.rows[0], vec![Cell::Text("1-0".into()), Cell::Text("[f, v]".into())]);
    }

    #[test]
    fn key_tree() {
        let keys = s(&["user:2", "user:1:name", "user:1:age", "session", "cart:9", "user:3"]);
        let t = tree(&keys, ":");
        let labels: Vec<(&str, usize)> = t.iter().map(|n| (n.label.as_str(), n.count)).collect();
        assert_eq!(labels, [("cart:9", 1), ("session", 1), ("user", 4)]);
        let user = &t[2];
        assert_eq!(user.children[0].label, "1");
        assert_eq!(user.children[0].children[0].key.as_deref(), Some("user:1:age"));
        assert_eq!(user.children[1].key.as_deref(), Some("user:2"));
        assert_eq!(ttl_label(-1), "no expiry");
        assert_eq!(ttl_label(3725), "1h 2m");
        assert_eq!(db_index("db3"), Some(3));
    }
}
