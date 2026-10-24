//! MongoDB: the mongo shell's syntax as typed in a console
//! (`db.users.find({age: {$gt: 30}}).sort({name: 1})`), its values as
//! Extended JSON, and documents as a table.

use std::ops::Range;

use serde_json::{Map, Number, Value, json};

use crate::{Cell, ColumnMeta, QueryResult, StatementOutcome};

/// One method call of a chain: `sort({name: 1})`.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub method: String,
    pub args: Vec<Value>,
}

/// A statement of the shell.
#[derive(Debug, Clone, PartialEq)]
pub enum Shell {
    /// `show dbs`, `show collections`.
    Show(String),
    /// A method of the database: `db.runCommand({...})`, `db.getCollectionNames()`.
    Db(Call),
    /// A method of a collection, with the cursor methods after it:
    /// `db.users.find({}).sort({a: 1}).limit(5)`.
    Collection { name: String, call: Call, chain: Vec<Call> },
}

/// What a statement gives back.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Documents, as relaxed Extended JSON; `more` when there were more than fetched.
    Documents { docs: Vec<Value>, more: bool },
    /// A single value: a count, a list of names, a command's reply.
    Value(Value),
    /// Documents written: inserted, matched and modified, or deleted.
    Written { affected: u64 },
}

// ---------------------------------------------------------------------------
// Values

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, i: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn err<T>(&self, msg: &str) -> Result<T, String> {
        let line = self.s[..self.i].matches('\n').count() + 1;
        let near: String = self.rest().chars().take(20).collect();
        if near.is_empty() {
            Err(format!("{msg} at the end (line {line})"))
        } else {
            Err(format!("{msg} at line {line}, near `{near}`"))
        }
    }

    /// Skips whitespace and comments.
    fn ws(&mut self) {
        loop {
            let r = self.rest();
            let t = r.trim_start();
            self.i += r.len() - t.len();
            if t.starts_with("//") {
                self.i += t.find('\n').unwrap_or(t.len());
            } else if let Some(body) = t.strip_prefix("/*") {
                self.i += body.find("*/").map_or(t.len(), |e| e + 4);
            } else {
                return;
            }
        }
    }

    fn eat(&mut self, c: char) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.eat(c) { Ok(()) } else { self.err(&format!("Expected `{c}`")) }
    }

    fn ident(&mut self) -> Option<&'a str> {
        self.ws();
        let r = self.rest();
        let end = r
            .char_indices()
            .find(|&(k, c)| !(c == '_' || c == '$' || c.is_alphanumeric()) || (k == 0 && c.is_ascii_digit()))
            .map_or(r.len(), |(k, _)| k);
        if end == 0 {
            return None;
        }
        self.i += end;
        Some(&r[..end])
    }

    fn string(&mut self) -> Result<String, String> {
        self.ws();
        let quote = match self.peek() {
            Some(q @ ('"' | '\'' | '`')) => q,
            _ => return self.err("Expected a string"),
        };
        self.i += 1;
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((k, c)) = chars.next() {
            match c {
                c if c == quote => {
                    self.i += k + 1;
                    return Ok(out);
                }
                '\\' => {
                    let Some((_, e)) = chars.next() else { break };
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex: String = (0..4).filter_map(|_| chars.next().map(|(_, h)| h)).collect();
                            let code = u32::from_str_radix(&hex, 16).map_err(|_| "Bad \\u escape".to_string())?;
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        '\n' => {}
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
        self.i = self.s.len();
        self.err("Unterminated string")
    }

    fn number(&mut self) -> Result<Value, String> {
        self.ws();
        let r = self.rest();
        let end = r
            .char_indices()
            .find(|&(k, c)| {
                !(c.is_ascii_digit()
                    || c == '.'
                    || c == 'e'
                    || c == 'E'
                    || ((c == '-' || c == '+') && (k == 0 || matches!(r.as_bytes()[k - 1], b'e' | b'E'))))
            })
            .map_or(r.len(), |(k, _)| k);
        let text = &r[..end];
        self.i += end;
        if let Ok(n) = text.parse::<i64>() {
            return Ok(Value::Number(n.into()));
        }
        match text.parse::<f64>().ok().and_then(Number::from_f64) {
            Some(n) => Ok(Value::Number(n)),
            None => self.err("Bad number"),
        }
    }

    /// `/pattern/flags`.
    fn regex(&mut self) -> Result<Value, String> {
        self.i += 1;
        let r = self.rest();
        let mut escaped = false;
        let mut in_class = false;
        let mut end = None;
        for (k, c) in r.char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '[' => in_class = true,
                ']' => in_class = false,
                '/' if !in_class => {
                    end = Some(k);
                    break;
                }
                '\n' => break,
                _ => {}
            }
        }
        let Some(end) = end else { return self.err("Unterminated regular expression") };
        let pattern = r[..end].to_string();
        self.i += end + 1;
        let flags = self.rest().chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>();
        self.i += flags.len();
        let mut flags: Vec<char> = flags.chars().collect();
        flags.sort_unstable();
        let options: String = flags.into_iter().collect();
        Ok(json!({"$regularExpression": {"pattern": pattern, "options": options}}))
    }

    fn args(&mut self) -> Result<Vec<Value>, String> {
        self.expect('(')?;
        let mut out = Vec::new();
        loop {
            if self.eat(')') {
                return Ok(out);
            }
            out.push(self.value()?);
            if !self.eat(',') {
                self.expect(')')?;
                return Ok(out);
            }
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.peek() {
            None => self.err("Expected a value"),
            Some('{') => {
                self.i += 1;
                let mut map = Map::new();
                loop {
                    if self.eat('}') {
                        return Ok(Value::Object(map));
                    }
                    self.ws();
                    let key = match self.peek() {
                        Some('"' | '\'' | '`') => self.string()?,
                        Some(c) if c.is_ascii_digit() || c == '-' => self.number()?.to_string(),
                        _ => match self.ident() {
                            Some(k) => k.to_string(),
                            None => return self.err("Expected a key"),
                        },
                    };
                    self.expect(':')?;
                    let v = self.value()?;
                    map.insert(key, v);
                    if !self.eat(',') {
                        self.expect('}')?;
                        return Ok(Value::Object(map));
                    }
                }
            }
            Some('[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    if self.eat(']') {
                        return Ok(Value::Array(items));
                    }
                    items.push(self.value()?);
                    if !self.eat(',') {
                        self.expect(']')?;
                        return Ok(Value::Array(items));
                    }
                }
            }
            Some('"' | '\'' | '`') => Ok(Value::String(self.string()?)),
            Some('/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.number(),
            Some(_) => {
                let start = self.i;
                let Some(mut word) = self.ident() else { return self.err("Unexpected character") };
                if word == "new" {
                    word = match self.ident() {
                        Some(w) => w,
                        None => return self.err("Expected a constructor after `new`"),
                    };
                }
                match word {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" | "undefined" => Ok(Value::Null),
                    "Infinity" => Ok(json!({"$numberDouble": "Infinity"})),
                    "NaN" => Ok(json!({"$numberDouble": "NaN"})),
                    "MinKey" | "MaxKey" => {
                        self.ws();
                        if self.peek() == Some('(') {
                            self.args()?;
                        }
                        Ok(if word == "MinKey" { json!({"$minKey": 1}) } else { json!({"$maxKey": 1}) })
                    }
                    _ => {
                        self.ws();
                        if self.peek() != Some('(') {
                            self.i = start;
                            return self.err("Unknown name");
                        }
                        let args = self.args()?;
                        constructor(word, &args).map_err(|e| format!("{word}(): {e}"))
                    }
                }
            }
        }
    }
}

fn text_arg(args: &[Value], i: usize) -> Option<String> {
    match args.get(i)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `ISODate("2024-05-01")` as the full form Extended JSON takes.
fn iso_date(s: &str) -> String {
    let s = s.trim();
    let s = if s.len() == 10 { format!("{s}T00:00:00Z") } else { s.replacen(' ', "T", 1) };
    let time = s.find('T').map_or("", |t| &s[t..]);
    if time.contains(['Z', '+', '-']) { s } else { format!("{s}Z") }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut n = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| *c != b'=' && !c.is_ascii_whitespace()) {
        n = n << 6 | B64.iter().position(|b| *b == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
        }
    }
    Some(out)
}

fn constructor(name: &str, args: &[Value]) -> Result<Value, String> {
    let need = |i: usize| text_arg(args, i).ok_or_else(|| "expects a value".to_string());
    Ok(match name {
        "ObjectId" | "ObjectID" => match text_arg(args, 0) {
            Some(hex) if hex.len() == 24 && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
                json!({"$oid": hex.to_lowercase()})
            }
            Some(_) => return Err("expects 24 hexadecimal digits".into()),
            // A new id: the driver makes it.
            None => json!({"$oid": "new"}),
        },
        "ISODate" | "Date" => match args.first() {
            None => json!({"$date": {"$numberLong": now_millis().to_string()}}),
            Some(Value::Number(n)) => json!({"$date": {"$numberLong": n.as_i64().unwrap_or_default().to_string()}}),
            Some(Value::String(s)) => json!({"$date": iso_date(s)}),
            Some(_) => return Err("expects a date".into()),
        },
        "NumberLong" | "Long" => json!({"$numberLong": need(0)?}),
        "NumberInt" | "Int32" => json!({"$numberInt": need(0)?}),
        "NumberDecimal" | "Decimal128" => json!({"$numberDecimal": need(0)?}),
        "Double" => json!({"$numberDouble": need(0)?}),
        "UUID" => {
            let hex: String = need(0)?.chars().filter(|c| *c != '-').collect();
            if hex.len() != 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err("expects 32 hexadecimal digits".into());
            }
            let bytes: Vec<u8> = (0..16).map(|k| u8::from_str_radix(&hex[k * 2..k * 2 + 2], 16).unwrap_or(0)).collect();
            json!({"$binary": {"base64": base64(&bytes), "subType": "04"}})
        }
        "BinData" => {
            let sub = args.first().and_then(Value::as_u64).ok_or("expects a subtype")?;
            json!({"$binary": {"base64": need(1)?, "subType": format!("{sub:02x}")}})
        }
        "Timestamp" => {
            let t = args.first().and_then(Value::as_u64).unwrap_or(0);
            let i = args.get(1).and_then(Value::as_u64).unwrap_or(0);
            json!({"$timestamp": {"t": t, "i": i}})
        }
        "RegExp" => {
            let mut flags: Vec<char> = text_arg(args, 1).unwrap_or_default().chars().collect();
            flags.sort_unstable();
            json!({"$regularExpression": {"pattern": need(0)?, "options": flags.into_iter().collect::<String>()}})
        }
        _ => return Err("is not known".into()),
    })
}

/// Parses one value as typed in the shell (a JavaScript literal, with
/// `ObjectId(...)`, `ISODate(...)` and the like) into Extended JSON.
pub fn parse_value(text: &str) -> Result<Value, String> {
    let mut p = Parser::new(text);
    let v = p.value()?;
    p.ws();
    if p.i < text.len() {
        return p.err("Unexpected text after the value");
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Statements

/// Parses one statement.
pub fn parse(text: &str) -> Result<Shell, String> {
    let text = text.trim().trim_end_matches(';').trim_end();
    let mut p = Parser::new(text);
    let first = p.ident().ok_or_else(|| "Expected `db.…` or `show …`".to_string())?;
    if first == "show" {
        let what = p.ident().ok_or_else(|| "Expected what to show: dbs or collections".to_string())?;
        return Ok(Shell::Show(what.to_lowercase()));
    }
    if first != "db" {
        return Err(format!("Unknown statement `{first}`: expected `db.…` or `show …`"));
    }
    // db.name.with.dots.method(...), db["name"].method(...), db.getCollection("name").method(...)
    let mut names: Vec<String> = Vec::new();
    let call = loop {
        if p.eat('[') {
            names.push(p.string()?);
            p.expect(']')?;
            continue;
        }
        p.expect('.')?;
        let name = p.ident().ok_or_else(|| "Expected a name after `.`".to_string())?;
        p.ws();
        if p.peek() == Some('(') {
            let args = p.args()?;
            if names.is_empty() && name == "getCollection" {
                match args.first() {
                    Some(Value::String(s)) => names.push(s.clone()),
                    _ => return Err("getCollection() expects a collection name".into()),
                }
                continue;
            }
            break Call { method: name.to_string(), args };
        }
        names.push(name.to_string());
    };
    let mut chain = Vec::new();
    while p.eat('.') {
        let method = p.ident().ok_or_else(|| "Expected a method after `.`".to_string())?.to_string();
        let args = p.args()?;
        chain.push(Call { method, args });
    }
    p.ws();
    if p.i < text.len() {
        return p.err("Unexpected text");
    }
    Ok(if names.is_empty() { Shell::Db(call) } else { Shell::Collection { name: names.join("."), call, chain } })
}

/// `db.getCollection("name")`, which takes any name.
pub fn collection_ref(name: &str) -> String {
    format!("db.getCollection({})", Value::String(name.to_string()))
}

/// Splits a script into statements: at `;`, or at a line end outside
/// brackets when the next line does not go on with `.method()`.
pub fn split(script: &str) -> Vec<Range<usize>> {
    let b = script.as_bytes();
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut end = 0;
    let mut depth = 0i32;
    let mut i = 0;
    let push = |out: &mut Vec<Range<usize>>, start: &mut Option<usize>, end: usize| {
        if let Some(s) = start.take() {
            out.push(s..end);
        }
    };
    while i < b.len() {
        let c = b[i];
        match c {
            b'"' | b'\'' | b'`' => {
                start.get_or_insert(i);
                i += 1;
                while i < b.len() && b[i] != c {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                end = i.min(b.len());
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = script[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
                continue;
            }
            b';' if depth <= 0 => {
                push(&mut out, &mut start, end);
                depth = 0;
            }
            b'\n' if depth <= 0 && start.is_some() => {
                // A statement goes on when the next line starts with `.`.
                let rest = &script[i + 1..];
                let mut next = rest.trim_start();
                while next.starts_with("//") {
                    next = next[next.find('\n').unwrap_or(next.len())..].trim_start();
                }
                if !next.starts_with('.') {
                    push(&mut out, &mut start, end);
                    depth = 0;
                }
            }
            c if c.is_ascii_whitespace() => {}
            _ => {
                match c {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    _ => {}
                }
                start.get_or_insert(i);
                end = i + 1;
            }
        }
        i += 1;
    }
    push(&mut out, &mut start, end);
    out
}

const READ_METHODS: &[&str] = &[
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "count",
    "distinct",
    "getIndexes",
    "stats",
    "explain",
    "dataSize",
    "storageSize",
    "totalIndexSize",
    "isCapped",
];

const READ_DB_METHODS: &[&str] = &[
    "getCollectionNames",
    "getCollectionInfos",
    "stats",
    "version",
    "serverStatus",
    "hostInfo",
    "getName",
    "listCommands",
];

const READ_COMMANDS: &[&str] = &[
    "find",
    "aggregate",
    "count",
    "distinct",
    "listCollections",
    "listIndexes",
    "listDatabases",
    "dbStats",
    "collStats",
    "serverStatus",
    "ping",
    "buildInfo",
    "hello",
    "isMaster",
    "ismaster",
    "connectionStatus",
    "hostInfo",
    "getParameter",
    "explain",
    "currentOp",
    "top",
    "usersInfo",
    "rolesInfo",
    "getCmdLineOpts",
    "listCommands",
];

/// Whether a pipeline writes (`$out`, `$merge`).
pub fn pipeline_writes(pipeline: &Value) -> bool {
    pipeline
        .as_array()
        .is_some_and(|stages| stages.iter().any(|s| s.get("$out").is_some() || s.get("$merge").is_some()))
}

/// Whether a statement only reads.
pub fn is_read_only(shell: &Shell) -> bool {
    match shell {
        Shell::Show(_) => true,
        Shell::Db(c) if c.method == "runCommand" || c.method == "adminCommand" => {
            let Some(Value::Object(cmd)) = c.args.first() else { return false };
            let Some(name) = cmd.keys().next() else { return false };
            READ_COMMANDS.contains(&name.as_str()) && !cmd.get("pipeline").is_some_and(pipeline_writes)
        }
        Shell::Db(c) => READ_DB_METHODS.contains(&c.method.as_str()),
        Shell::Collection { call, .. } => {
            READ_METHODS.contains(&call.method.as_str())
                && !(call.method == "aggregate" && call.args.first().is_some_and(pipeline_writes))
        }
    }
}

/// Whether a script's statements all only read; one that does not parse counts as writing.
pub fn script_is_read_only(script: &str) -> bool {
    split(script).into_iter().all(|r| parse(&script[r]).is_ok_and(|s| is_read_only(&s)))
}

// ---------------------------------------------------------------------------
// Showing values

/// The type of a value, as the column header shows it.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "double",
        Value::Number(_) => "int",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(o) => match o.keys().next().map(String::as_str) {
            Some("$oid") => "objectId",
            Some("$date") => "date",
            Some("$numberLong") => "long",
            Some("$numberDecimal") => "decimal",
            Some("$numberDouble") => "double",
            Some("$numberInt") => "int",
            Some("$binary") => "binData",
            Some("$timestamp") => "timestamp",
            Some("$regularExpression") => "regex",
            _ => "object",
        },
    }
}

fn numeric_type(t: &str) -> bool {
    matches!(t, "int" | "long" | "double" | "decimal")
}

/// A wrapped scalar (`{"$oid": …}`, `{"$date": …}`) as plain text.
pub(crate) fn special_text(o: &Map<String, Value>) -> Option<String> {
    if o.len() != 1 {
        return None;
    }
    let (k, v) = o.iter().next()?;
    match (k.as_str(), v) {
        ("$oid" | "$numberLong" | "$numberDecimal" | "$numberDouble" | "$numberInt", Value::String(s)) => {
            Some(s.clone())
        }
        ("$date", Value::String(s)) => Some(s.clone()),
        ("$date", Value::Object(m)) => m.get("$numberLong").and_then(Value::as_str).map(|ms| {
            let ms: i64 = ms.parse().unwrap_or(0);
            date_text(ms)
        }),
        _ => None,
    }
}

/// Milliseconds since 1970 as `2024-05-01T10:00:00.000Z`.
fn date_text(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A value as a grid cell: wrapped scalars plainly, documents and arrays as JSON.
pub fn cell(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::String(s) => Cell::Text(s.clone()),
        Value::Object(o) => Cell::Text(special_text(o).unwrap_or_else(|| v.to_string())),
        other => Cell::Text(other.to_string()),
    }
}

/// Documents as a table: a column per top-level field (`_id` first, the rest
/// as they first appear), typed by the values found.
pub fn documents_result(docs: &[Value], more: bool) -> QueryResult {
    let mut names: Vec<String> = Vec::new();
    let mut types: Vec<Option<&'static str>> = Vec::new();
    let mut plain = Vec::new();
    for d in docs {
        let Value::Object(o) = d else {
            plain.push(d);
            continue;
        };
        for (k, v) in o {
            let t = type_name(v);
            match names.iter().position(|n| n == k) {
                Some(i) => {
                    if t != "null" {
                        types[i] = match types[i] {
                            None => Some(t),
                            Some(old) if old == t => Some(t),
                            Some(old) if numeric_type(old) && numeric_type(t) => Some("double"),
                            Some(_) => Some("mixed"),
                        };
                    }
                }
                None => {
                    names.push(k.clone());
                    types.push((t != "null").then_some(t));
                }
            }
        }
    }
    if let Some(i) = names.iter().position(|n| n == "_id") {
        let n = names.remove(i);
        let t = types.remove(i);
        names.insert(0, n);
        types.insert(0, t);
    }
    if !plain.is_empty() && names.is_empty() {
        let rows = docs.iter().map(|d| vec![cell(d)]).collect();
        return QueryResult { columns: vec![ColumnMeta::new("value", "")], rows, truncated: more };
    }
    let columns = names
        .iter()
        .zip(&types)
        .map(|(n, t)| {
            let t = t.unwrap_or("null");
            ColumnMeta { name: n.clone(), type_name: t.into(), numeric: numeric_type(t) }
        })
        .collect();
    let rows = docs.iter().map(|d| names.iter().map(|n| d.get(n).map_or(Cell::Null, cell)).collect()).collect();
    QueryResult { columns, rows, truncated: more }
}

/// A reply as the console shows it.
pub fn outcome(reply: Reply) -> StatementOutcome {
    match reply {
        Reply::Documents { docs, more } => StatementOutcome::Rows(documents_result(&docs, more)),
        Reply::Written { affected } => StatementOutcome::Done { affected: Some(affected) },
        Reply::Value(Value::Array(items)) => StatementOutcome::Rows(documents_result(&items, false)),
        Reply::Value(v @ Value::Object(_)) => StatementOutcome::Rows(documents_result(&[v], false)),
        Reply::Value(v) => StatementOutcome::Rows(QueryResult {
            columns: vec![ColumnMeta {
                name: "result".into(),
                type_name: type_name(&v).into(),
                numeric: v.is_number(),
            }],
            rows: vec![vec![cell(&v)]],
            truncated: false,
        }),
    }
}

/// A value written back as the shell reads it, indented: `ObjectId("…")`,
/// `ISODate("…")` instead of their Extended JSON wrappers.
pub fn to_shell(v: &Value) -> String {
    let mut out = String::new();
    write_shell(v, 0, &mut out);
    out
}

fn write_shell(v: &Value, indent: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match v {
        Value::Object(o) => {
            if let Some(s) = shell_scalar(o) {
                out.push_str(&s);
                return;
            }
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (k, (key, val)) in o.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                write_shell(val, indent + 1, out);
                if k + 1 < o.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (k, val) in a.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                write_shell(val, indent + 1, out);
                if k + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn shell_scalar(o: &Map<String, Value>) -> Option<String> {
    if o.len() != 1 {
        return None;
    }
    let (k, v) = o.iter().next()?;
    let q = |s: &str| Value::String(s.to_string()).to_string();
    Some(match (k.as_str(), v) {
        ("$oid", Value::String(s)) => format!("ObjectId({})", q(s)),
        ("$date", Value::String(s)) => format!("ISODate({})", q(s)),
        ("$date", Value::Object(_)) => format!("ISODate({})", q(&special_text(o)?)),
        ("$numberLong", Value::String(s)) => format!("NumberLong({})", q(s)),
        ("$numberDecimal", Value::String(s)) => format!("NumberDecimal({})", q(s)),
        ("$numberInt", Value::String(s)) => format!("NumberInt({})", q(s)),
        ("$numberDouble", Value::String(s)) => format!("Double({})", q(s)),
        ("$binary", Value::Object(b)) => {
            let data = b.get("base64")?.as_str()?;
            let sub = b.get("subType")?.as_str()?;
            if sub == "04"
                && let Some(bytes) = unbase64(data).filter(|b| b.len() == 16)
            {
                let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                return Some(format!("UUID(\"{}-{}-{}-{}-{}\")", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]));
            }
            format!("BinData({}, {})", u8::from_str_radix(sub, 16).ok()?, q(data))
        }
        ("$timestamp", Value::Object(t)) => format!("Timestamp({}, {})", t.get("t")?, t.get("i")?),
        ("$regularExpression", Value::Object(r)) => {
            format!("/{}/{}", r.get("pattern")?.as_str()?, r.get("options")?.as_str()?)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_as_typed_in_the_shell() {
        let v = parse_value(
            "{name: 'Ann', 'a b': \"x\\ny\", age: {$gt: 30, $lte: 4.5e1}, tags: ['a', 'b',], // note\n\
             _id: ObjectId(\"64B1F0A2C3D4E5F601234567\"), at: ISODate('2024-05-01'), n: NumberLong(7),\
             d: new Date('2024-05-01T10:00:00'), re: /^an+/i, ok: true, none: null, neg: -3}",
        )
        .unwrap();
        assert_eq!(v["name"], "Ann");
        assert_eq!(v["a b"], "x\ny");
        assert_eq!(v["age"]["$gt"], 30);
        assert_eq!(v["age"]["$lte"], 45.0);
        assert_eq!(v["tags"], json!(["a", "b"]));
        assert_eq!(v["_id"], json!({"$oid": "64b1f0a2c3d4e5f601234567"}));
        assert_eq!(v["at"], json!({"$date": "2024-05-01T00:00:00Z"}));
        assert_eq!(v["d"], json!({"$date": "2024-05-01T10:00:00Z"}));
        assert_eq!(v["n"], json!({"$numberLong": "7"}));
        assert_eq!(v["re"], json!({"$regularExpression": {"pattern": "^an+", "options": "i"}}));
        assert_eq!((v["ok"].clone(), v["none"].clone(), v["neg"].clone()), (json!(true), Value::Null, json!(-3)));
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "name", "the order is kept");
        assert!(parse_value("{a: }").is_err());
        assert!(parse_value("{a: ObjectId('xyz')}").unwrap_err().contains("ObjectId"));
        assert!(parse_value("[1, 2").is_err());
    }

    #[test]
    fn statements() {
        let s = parse("db.users.find({age: {$gt: 30}}, {name: 1}).sort({name: -1}).limit(5);").unwrap();
        let Shell::Collection { name, call, chain } = s else { panic!() };
        assert_eq!(name, "users");
        assert_eq!(call.method, "find");
        assert_eq!(call.args.len(), 2);
        assert_eq!(chain.iter().map(|c| c.method.as_str()).collect::<Vec<_>>(), ["sort", "limit"]);
        let Shell::Collection { name, .. } = parse("db.system.profile.find()").unwrap() else { panic!() };
        assert_eq!(name, "system.profile");
        let Shell::Collection { name, .. } = parse("db.getCollection('my-logs').countDocuments({})").unwrap() else {
            panic!()
        };
        assert_eq!(name, "my-logs");
        let Shell::Collection { name, .. } = parse("db['a b'].find()").unwrap() else { panic!() };
        assert_eq!(name, "a b");
        assert_eq!(
            parse("db.runCommand({ping: 1})").unwrap(),
            Shell::Db(Call { method: "runCommand".into(), args: vec![json!({"ping": 1})] })
        );
        assert_eq!(parse("show collections").unwrap(), Shell::Show("collections".into()));
        assert!(parse("select 1").is_err());
        assert!(parse("db.users.find(").is_err());
    }

    #[test]
    fn splits_scripts() {
        let script =
            "db.a.find()\n  .limit(2)\n\ndb.b.insertOne({\n  x: 1;\n})\n// done\ndb.c.find({s: 'a;b'}); db.d.drop()";
        let parts: Vec<&str> = split(script).into_iter().map(|r| &script[r]).collect();
        assert_eq!(
            parts,
            ["db.a.find()\n  .limit(2)", "db.b.insertOne({\n  x: 1;\n})", "db.c.find({s: 'a;b'})", "db.d.drop()"]
        );
    }

    #[test]
    fn reads_and_writes() {
        let ro = |s: &str| is_read_only(&parse(s).unwrap());
        assert!(ro("db.a.find({})"));
        assert!(ro("db.a.aggregate([{$match: {}}])"));
        assert!(!ro("db.a.aggregate([{$match: {}}, {$out: 'b'}])"));
        assert!(!ro("db.a.insertOne({})"));
        assert!(!ro("db.a.drop()"));
        assert!(ro("db.runCommand({listCollections: 1})"));
        assert!(!ro("db.runCommand({drop: 'a'})"));
        assert!(ro("show dbs"));
        assert!(script_is_read_only("db.a.find()\ndb.b.countDocuments()"));
        assert!(!script_is_read_only("db.a.find()\ndb.b.deleteMany({})"));
    }

    #[test]
    fn documents_as_a_table() {
        let docs = vec![
            json!({"name": "Ann", "_id": {"$oid": "64b1f0a2c3d4e5f601234567"}, "age": 31, "tags": ["a"]}),
            json!({"_id": {"$oid": "64b1f0a2c3d4e5f601234568"}, "age": 2.5, "at": {"$date": "2024-05-01T00:00:00Z"}}),
        ];
        let r = documents_result(&docs, true);
        let cols: Vec<(&str, &str, bool)> =
            r.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str(), c.numeric)).collect();
        assert_eq!(
            cols,
            [
                ("_id", "objectId", false),
                ("name", "string", false),
                ("age", "double", true),
                ("tags", "array", false),
                ("at", "date", false)
            ]
        );
        assert_eq!(r.rows[0][0], Cell::Text("64b1f0a2c3d4e5f601234567".into()));
        assert_eq!(r.rows[0][3], Cell::Text("[\"a\"]".into()));
        assert_eq!(r.rows[1][1], Cell::Null);
        assert!(r.truncated);
        assert_eq!(
            cell(&json!({"$date": {"$numberLong": "1714557600000"}})),
            Cell::Text("2024-05-01T10:00:00.000Z".into())
        );
    }

    #[test]
    fn writes_values_back_for_the_shell() {
        let v = parse_value(
            "{_id: ObjectId('64b1f0a2c3d4e5f601234567'), at: ISODate('2024-05-01T00:00:00Z'), \
             u: UUID('0123456789abcdef0123456789abcdef'), re: /a\\/b/gi, n: NumberDecimal('1.50'), list: [1, {x: []}]}",
        )
        .unwrap();
        let text = to_shell(&v);
        assert!(text.contains("\"_id\": ObjectId(\"64b1f0a2c3d4e5f601234567\")"), "{text}");
        assert!(text.contains("UUID(\"01234567-89ab-cdef-0123-456789abcdef\")"), "{text}");
        assert!(text.contains("/a\\/b/gi"), "{text}");
        assert_eq!(parse_value(&text).unwrap(), v, "{text}");
    }
}
