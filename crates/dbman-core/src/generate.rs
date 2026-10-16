//! Test data: a generator for each column, picked from its type and name,
//! and rows made from them. A fixed seed gives the same rows every time.

use std::collections::HashMap;

use crate::copy::{Generic, generic};
use crate::{Cell, ColumnInfo, DbKind, TableDetails};

/// How the values of a column are made.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// Not written: the server fills it (identity, default).
    Skip,
    /// 1, 2, 3… after `start`.
    Sequence {
        start: i64,
    },
    Integer {
        min: i64,
        max: i64,
    },
    Decimal {
        min: f64,
        max: f64,
        scale: u32,
    },
    Bool,
    Words {
        min: u32,
        max: u32,
    },
    FirstName,
    LastName,
    FullName,
    Email,
    Phone,
    City,
    Country,
    Company,
    Url,
    /// Days between two dates (`YYYY-MM-DD`).
    Date {
        from: String,
        to: String,
    },
    Timestamp {
        from: String,
        to: String,
    },
    Time,
    Uuid,
    Json,
    Bytes {
        len: u32,
    },
    /// One of these values.
    OneOf(Vec<String>),
    Constant(String),
    /// A value of the referenced column, read from the parent table.
    Reference {
        table: String,
        column: String,
    },
    Null,
}

impl Kind {
    /// Names for the generator picker, in order; [`Kind::with_name`] makes one.
    pub const NAMES: [&'static str; 25] = [
        "Skip (server default)",
        "Sequence",
        "Integer",
        "Decimal",
        "Boolean",
        "Words",
        "First name",
        "Last name",
        "Full name",
        "Email",
        "Phone",
        "City",
        "Country",
        "Company",
        "URL",
        "Date",
        "Timestamp",
        "Time",
        "UUID",
        "JSON",
        "Bytes",
        "One of",
        "Constant",
        "From parent table",
        "NULL",
    ];

    pub fn name(&self) -> &'static str {
        Self::NAMES[self.index()]
    }

    pub fn index(&self) -> usize {
        match self {
            Kind::Skip => 0,
            Kind::Sequence { .. } => 1,
            Kind::Integer { .. } => 2,
            Kind::Decimal { .. } => 3,
            Kind::Bool => 4,
            Kind::Words { .. } => 5,
            Kind::FirstName => 6,
            Kind::LastName => 7,
            Kind::FullName => 8,
            Kind::Email => 9,
            Kind::Phone => 10,
            Kind::City => 11,
            Kind::Country => 12,
            Kind::Company => 13,
            Kind::Url => 14,
            Kind::Date { .. } => 15,
            Kind::Timestamp { .. } => 16,
            Kind::Time => 17,
            Kind::Uuid => 18,
            Kind::Json => 19,
            Kind::Bytes { .. } => 20,
            Kind::OneOf(_) => 21,
            Kind::Constant(_) => 22,
            Kind::Reference { .. } => 23,
            Kind::Null => 24,
        }
    }

    /// The generator at `index` in [`Kind::NAMES`] with default settings.
    pub fn with_name(index: usize) -> Kind {
        match index {
            0 => Kind::Skip,
            1 => Kind::Sequence { start: 1 },
            2 => Kind::Integer { min: 1, max: 1000 },
            3 => Kind::Decimal { min: 0.0, max: 1000.0, scale: 2 },
            4 => Kind::Bool,
            5 => Kind::Words { min: 1, max: 4 },
            6 => Kind::FirstName,
            7 => Kind::LastName,
            8 => Kind::FullName,
            9 => Kind::Email,
            10 => Kind::Phone,
            11 => Kind::City,
            12 => Kind::Country,
            13 => Kind::Company,
            14 => Kind::Url,
            15 => Kind::Date { from: "2020-01-01".into(), to: "2025-12-31".into() },
            16 => Kind::Timestamp { from: "2020-01-01".into(), to: "2025-12-31".into() },
            17 => Kind::Time,
            18 => Kind::Uuid,
            19 => Kind::Json,
            20 => Kind::Bytes { len: 16 },
            21 => Kind::OneOf(vec!["a".into(), "b".into(), "c".into()]),
            22 => Kind::Constant(String::new()),
            23 => Kind::Reference { table: String::new(), column: String::new() },
            _ => Kind::Null,
        }
    }
}

impl Kind {
    /// The settings as one line of text, for editing: `1..100`,
    /// `2020-01-01..2025-12-31`, `a, b, c`, `customers.id`…
    pub fn params(&self) -> String {
        match self {
            Kind::Sequence { start } => start.to_string(),
            Kind::Integer { min, max } => format!("{min}..{max}"),
            Kind::Decimal { min, max, scale } => format!("{min:.*}..{max:.*}", *scale as usize, *scale as usize),
            Kind::Words { min, max } => format!("{min}..{max}"),
            Kind::Date { from, to } | Kind::Timestamp { from, to } => format!("{from}..{to}"),
            Kind::Bytes { len } => len.to_string(),
            Kind::OneOf(v) => v.join(", "),
            Kind::Constant(v) => v.clone(),
            Kind::Reference { table, column } => format!("{table}.{column}"),
            _ => String::new(),
        }
    }

    /// Whether the generator has settings to edit.
    pub fn has_params(index: usize) -> bool {
        matches!(index, 1 | 2 | 3 | 5 | 15 | 16 | 20 | 21 | 22 | 23)
    }

    /// The generator at `index` in [`Kind::NAMES`] with settings read from
    /// [`Kind::params`] text; `scale` is kept for decimals.
    pub fn parse(index: usize, text: &str, scale: u32) -> Result<Kind, String> {
        let text = text.trim();
        let pair = || -> Result<(&str, &str), String> {
            text.split_once("..").map(|(a, b)| (a.trim(), b.trim())).ok_or_else(|| "Write it as min..max".to_string())
        };
        let int = |s: &str| s.parse::<i64>().map_err(|_| format!("Not a whole number: {s}"));
        let date =
            |s: &str| days_from_date(s).map(|_| s.to_string()).ok_or_else(|| format!("Not a date (YYYY-MM-DD): {s}"));
        Ok(match index {
            1 => Kind::Sequence { start: int(text)? },
            2 => {
                let (a, b) = pair()?;
                Kind::Integer { min: int(a)?, max: int(b)? }
            }
            3 => {
                let (a, b) = pair()?;
                let num = |s: &str| s.parse::<f64>().map_err(|_| format!("Not a number: {s}"));
                Kind::Decimal { min: num(a)?, max: num(b)?, scale }
            }
            5 => {
                let (a, b) = pair()?;
                let (a, b) = (int(a)?, int(b)?);
                if a < 1 || b < a {
                    return Err("Words: at least 1, and max not below min".into());
                }
                Kind::Words { min: a as u32, max: b as u32 }
            }
            15 | 16 => {
                let (a, b) = pair()?;
                let (from, to) = (date(a)?, date(b)?);
                if index == 15 { Kind::Date { from, to } } else { Kind::Timestamp { from, to } }
            }
            20 => Kind::Bytes { len: int(text)?.clamp(0, 1 << 20) as u32 },
            21 => {
                let values: Vec<String> =
                    text.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
                if values.is_empty() {
                    return Err("List the values, separated by commas".into());
                }
                Kind::OneOf(values)
            }
            22 => Kind::Constant(text.to_string()),
            23 => {
                let (table, column) = text.rsplit_once('.').ok_or_else(|| "Write it as table.column".to_string())?;
                Kind::Reference { table: table.trim().into(), column: column.trim().into() }
            }
            i => Kind::with_name(i),
        })
    }
}

/// What to write into one column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnPlan {
    pub column: String,
    pub kind: Kind,
    /// Percent of rows that get NULL (nullable columns only).
    pub null_percent: u8,
    /// Values must not repeat (primary key or unique index).
    pub unique: bool,
    /// Longest text the column takes.
    pub max_len: Option<u32>,
    /// The column's type, for converting values.
    pub generic: Generic,
}

impl ColumnPlan {
    pub fn writes(&self) -> bool {
        self.kind != Kind::Skip
    }
}

const FIRST: &[&str] = &[
    "Alice", "Bob", "Carol", "David", "Emma", "Frank", "Grace", "Henry", "Irene", "Jack", "Kate", "Liam", "Maria",
    "Nick", "Olga", "Peter", "Quinn", "Rosa", "Sam", "Tina", "Umar", "Vera", "Will", "Yana", "Zoe", "Ivan", "Anna",
    "Dmitry", "Elena", "Sergey",
];
const LAST: &[&str] = &[
    "Smith",
    "Johnson",
    "Brown",
    "Taylor",
    "Miller",
    "Wilson",
    "Moore",
    "Clark",
    "Lewis",
    "Walker",
    "Hall",
    "Young",
    "King",
    "Wright",
    "Scott",
    "Green",
    "Baker",
    "Adams",
    "Nelson",
    "Hill",
    "Ivanov",
    "Petrova",
    "Sokolov",
    "Kuznetsova",
    "Popov",
    "Novak",
    "Garcia",
    "Muller",
    "Rossi",
    "Kowalski",
];
const CITIES: &[&str] = &[
    "London",
    "Paris",
    "Berlin",
    "Madrid",
    "Rome",
    "Vienna",
    "Prague",
    "Warsaw",
    "Lisbon",
    "Dublin",
    "Oslo",
    "Stockholm",
    "Helsinki",
    "Moscow",
    "Kazan",
    "Tokyo",
    "Seoul",
    "Toronto",
    "Chicago",
    "Boston",
    "Denver",
    "Austin",
    "Sydney",
    "Cairo",
];
const COUNTRIES: &[&str] = &[
    "Austria",
    "Brazil",
    "Canada",
    "Chile",
    "Denmark",
    "Egypt",
    "Finland",
    "France",
    "Germany",
    "India",
    "Italy",
    "Japan",
    "Kenya",
    "Mexico",
    "Norway",
    "Poland",
    "Portugal",
    "Russia",
    "Spain",
    "Sweden",
    "Turkey",
    "Ukraine",
    "United Kingdom",
    "United States",
];
const COMPANY: &[&str] = &[
    "Acme",
    "Globex",
    "Initech",
    "Umbrella",
    "Stark",
    "Wayne",
    "Hooli",
    "Vandelay",
    "Soylent",
    "Tyrell",
    "Cyberdyne",
    "Wonka",
    "Gringotts",
    "Monarch",
    "Aperture",
    "Black Mesa",
];
const SUFFIX: &[&str] = &["Inc", "Ltd", "Group", "Labs", "Systems", "Partners", "Trading", "Holdings"];
const WORDS: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
];

/// The values a MySQL `enum('a','b')` or ClickHouse `Enum8('a' = 1)` takes.
pub fn enum_values(data_type: &str) -> Option<Vec<String>> {
    let lower = data_type.to_ascii_lowercase();
    let inner = ["enum(", "enum8(", "enum16(", "set("]
        .iter()
        .find_map(|p| lower.strip_prefix(p).map(|_| &data_type[p.len()..]))?
        .strip_suffix(')')?;
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut s = String::new();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    s.push('\'');
                    continue;
                }
                break;
            }
            s.push(c);
        }
        out.push(s);
        // Skip `= 1` up to the next comma.
        for c in chars.by_ref() {
            if c == ',' {
                break;
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A generator that fits the column's name and type.
pub fn default_kind(kind: DbKind, c: &ColumnInfo, details: &TableDetails) -> Kind {
    if c.auto_increment {
        return Kind::Skip;
    }
    if let Some(fk) = details.foreign_keys.iter().find(|f| f.columns.len() == 1 && f.columns[0] == c.name) {
        return Kind::Reference { table: fk.ref_table.clone(), column: fk.ref_columns[0].clone() };
    }
    if let Some(values) = enum_values(&c.data_type) {
        return Kind::OneOf(values);
    }
    let g = generic(kind, &c.data_type);
    let name = c.name.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    match g {
        Generic::Bool => Kind::Bool,
        Generic::Int16 | Generic::Int32 | Generic::Int64 => {
            if c.primary_key {
                Kind::Sequence { start: 1 }
            } else if has(&["age"]) {
                Kind::Integer { min: 18, max: 90 }
            } else if has(&["year"]) {
                Kind::Integer { min: 1990, max: 2025 }
            } else {
                let max = if g == Generic::Int16 { 30_000 } else { 1000 };
                Kind::Integer { min: 0, max }
            }
        }
        Generic::Decimal(p) => {
            let scale = p.map_or(2, |(_, s)| s);
            let room = p.map_or(1e6, |(p, s)| 10f64.powi((p.saturating_sub(s)).min(9) as i32) - 1.0);
            Kind::Decimal { min: 0.0, max: room.min(10_000.0), scale }
        }
        Generic::Float32 | Generic::Float64 => Kind::Decimal { min: 0.0, max: 1000.0, scale: 3 },
        Generic::Date => Kind::Date { from: "2020-01-01".into(), to: "2025-12-31".into() },
        Generic::Timestamp | Generic::TimestampTz => {
            Kind::Timestamp { from: "2020-01-01".into(), to: "2025-12-31".into() }
        }
        Generic::Time => Kind::Time,
        Generic::Uuid => Kind::Uuid,
        Generic::Json => Kind::Json,
        Generic::Binary => Kind::Bytes { len: 16 },
        Generic::Text(_) => {
            if has(&["email", "e_mail", "mail"]) {
                Kind::Email
            } else if has(&["first_name", "firstname", "given"]) {
                Kind::FirstName
            } else if has(&["last_name", "lastname", "surname", "family"]) {
                Kind::LastName
            } else if has(&["phone", "tel", "mobile"]) {
                Kind::Phone
            } else if has(&["city", "town"]) {
                Kind::City
            } else if has(&["country"]) {
                Kind::Country
            } else if has(&["company", "organization", "organisation", "employer", "vendor"]) {
                Kind::Company
            } else if has(&["url", "site", "link", "homepage"]) {
                Kind::Url
            } else if has(&["name", "author", "owner", "customer", "user"]) {
                Kind::FullName
            } else if has(&["description", "comment", "note", "text", "body", "message"]) {
                Kind::Words { min: 3, max: 12 }
            } else if has(&["date"]) && c.data_type.to_ascii_lowercase().contains("char") {
                Kind::Date { from: "2020-01-01".into(), to: "2025-12-31".into() }
            } else {
                Kind::Words { min: 1, max: 3 }
            }
        }
    }
}

/// A plan for every column of the table.
pub fn default_plan(kind: DbKind, details: &TableDetails) -> Vec<ColumnPlan> {
    let unique_cols: Vec<&str> = details
        .indexes
        .iter()
        .filter(|i| (i.unique || i.primary) && i.columns.len() == 1)
        .map(|i| i.columns[0].as_str())
        .chain(details.primary_key())
        .collect();
    details
        .columns
        .iter()
        .map(|c| {
            let g = generic(kind, &c.data_type);
            ColumnPlan {
                column: c.name.clone(),
                kind: default_kind(kind, c, details),
                null_percent: if c.nullable && !c.primary_key { 10 } else { 0 },
                unique: unique_cols.contains(&c.name.as_str()),
                max_len: match g {
                    Generic::Text(n) => n,
                    _ => None,
                },
                generic: g,
            }
        })
        .collect()
}

/// xorshift64*: small, fast and the same everywhere.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// In `min..=max`.
    pub fn range(&mut self, min: i64, max: i64) -> i64 {
        if max <= min {
            return min;
        }
        let span = (max as i128 - min as i128 + 1) as u128;
        (min as i128 + (self.next_u64() as u128 % span) as i128) as i64
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.next_u64() as usize % items.len()]
    }
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date.
pub fn days_from_date(s: &str) -> Option<i64> {
    let mut it = s.trim().get(..10)?.split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// `YYYY-MM-DD` of days since 1970-01-01.
pub fn date_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Makes rows from a plan. `references` holds the parent values of each
/// [`Kind::Reference`] column; `starts` the first value of sequences.
pub struct Generator {
    plan: Vec<ColumnPlan>,
    rng: Rng,
    references: HashMap<String, Vec<Cell>>,
    row: u64,
}

impl Generator {
    pub fn new(plan: Vec<ColumnPlan>, seed: u64, references: HashMap<String, Vec<Cell>>) -> Self {
        Self { plan, rng: Rng::new(seed), references, row: 0 }
    }

    /// Numbers rows from `n` on, so unique values go on after the `n` rows
    /// a table already has.
    pub fn starting_at(mut self, n: u64) -> Self {
        self.row = n;
        self
    }

    /// The columns that are written, in order.
    pub fn columns(&self) -> Vec<&ColumnPlan> {
        self.plan.iter().filter(|p| p.writes()).collect()
    }

    /// The next row: a cell per written column.
    pub fn row(&mut self) -> Vec<Cell> {
        let i = self.row;
        self.row += 1;
        let plan = std::mem::take(&mut self.plan);
        let row = plan.iter().filter(|p| p.writes()).map(|p| self.cell(p, i)).collect();
        self.plan = plan;
        row
    }

    fn cell(&mut self, p: &ColumnPlan, i: u64) -> Cell {
        if p.null_percent > 0 && !p.unique && self.rng.range(1, 100) <= i64::from(p.null_percent) {
            return Cell::Null;
        }
        let rng = &mut self.rng;
        let text = |s: String| Cell::Text(fit(s, p.max_len, p.unique, i));
        match &p.kind {
            Kind::Skip | Kind::Null => Cell::Null,
            Kind::Sequence { start } => Cell::Text((start + i as i64).to_string()),
            Kind::Integer { min, max } => {
                if p.unique {
                    Cell::Text((min + i as i64).to_string())
                } else {
                    Cell::Text(rng.range(*min, *max).to_string())
                }
            }
            Kind::Decimal { min, max, scale } => {
                let v = if p.unique { min + i as f64 } else { min + rng.unit() * (max - min) };
                Cell::Text(format!("{v:.*}", *scale as usize))
            }
            Kind::Bool => Cell::Text((rng.next_u64() & 1 == 1).to_string()),
            Kind::Words { min, max } => {
                let n = rng.range(i64::from(*min), i64::from((*max).max(*min)));
                let words: Vec<&str> = (0..n.max(1)).map(|_| *rng.pick(WORDS)).collect();
                text(capitalize(&words.join(" ")))
            }
            Kind::FirstName => text(rng.pick(FIRST).to_string()),
            Kind::LastName => text(rng.pick(LAST).to_string()),
            Kind::FullName => text(format!("{} {}", rng.pick(FIRST), rng.pick(LAST))),
            Kind::Email => {
                let (f, l) = (rng.pick(FIRST).to_lowercase(), rng.pick(LAST).to_lowercase());
                // The row number keeps addresses apart.
                Cell::Text(fit_end(format!("{f}.{l}{}", i + 1), "@example.com", p.max_len))
            }
            Kind::Phone => {
                text(format!("+1 {:03} {:03} {:04}", rng.range(200, 999), rng.range(100, 999), rng.range(0, 9999)))
            }
            Kind::City => text(rng.pick(CITIES).to_string()),
            Kind::Country => text(rng.pick(COUNTRIES).to_string()),
            Kind::Company => text(format!("{} {}", rng.pick(COMPANY), rng.pick(SUFFIX))),
            Kind::Url => {
                let w = rng.pick(WORDS);
                Cell::Text(fit_end(
                    format!("https://{w}{}.example.com/", if p.unique { i + 1 } else { 0 }),
                    "",
                    p.max_len,
                ))
            }
            Kind::Date { from, to } => {
                let (a, b) = span(from, to);
                let d = if p.unique { a + i as i64 } else { rng.range(a, b) };
                Cell::Text(date_from_days(d))
            }
            Kind::Timestamp { from, to } => {
                let (a, b) = span(from, to);
                let secs = if p.unique { a * 86_400 + i as i64 } else { rng.range(a * 86_400, b * 86_400 + 86_399) };
                let (d, s) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
                Cell::Text(format!("{} {:02}:{:02}:{:02}", date_from_days(d), s / 3600, s / 60 % 60, s % 60))
            }
            Kind::Time => {
                let s = rng.range(0, 86_399);
                Cell::Text(format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60))
            }
            Kind::Uuid => {
                let (a, b) = (rng.next_u64(), rng.next_u64());
                let b = (b & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
                let a = (a & 0xFFFF_FFFF_FFFF_0FFF) | 0x4000;
                Cell::Text(format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    a >> 32,
                    (a >> 16) & 0xFFFF,
                    a & 0xFFFF,
                    b >> 48,
                    b & 0xFFFF_FFFF_FFFF
                ))
            }
            Kind::Json => Cell::Text(format!(
                "{{\"id\": {}, \"tag\": \"{}\", \"score\": {}}}",
                i + 1,
                rng.pick(WORDS),
                rng.range(0, 100)
            )),
            Kind::Bytes { len } => Cell::Bytes((0..*len).map(|_| rng.next_u64() as u8).collect()),
            Kind::OneOf(values) if values.is_empty() => Cell::Null,
            Kind::OneOf(values) => {
                let v = if p.unique { &values[i as usize % values.len()] } else { rng.pick(values) };
                Cell::Text(v.clone())
            }
            Kind::Constant(v) => Cell::Text(v.clone()),
            Kind::Reference { .. } => match self.references.get(&p.column) {
                Some(values) if !values.is_empty() => {
                    let v = if p.unique { &values[i as usize % values.len()] } else { rng.pick(values) };
                    v.clone()
                }
                _ => Cell::Null,
            },
        }
    }
}

fn span(from: &str, to: &str) -> (i64, i64) {
    let a = days_from_date(from).unwrap_or(18_262);
    let b = days_from_date(to).unwrap_or(a + 365);
    (a.min(b), a.max(b))
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Cuts text to the column's length; unique columns get the row number.
fn fit(s: String, max: Option<u32>, unique: bool, i: u64) -> String {
    if unique { fit_end(s, &format!(" {}", i + 1), max) } else { fit_end(s, "", max) }
}

/// `s` + `end`, cutting `s` so the whole fits in `max` characters.
fn fit_end(s: String, end: &str, max: Option<u32>) -> String {
    let Some(max) = max.map(|m| m as usize) else { return s + end };
    let room = max.saturating_sub(end.chars().count());
    let mut out: String = s.chars().take(room).collect();
    out.push_str(end);
    out.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ForeignKeyInfo, IndexInfo};

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    #[test]
    fn picks_generators_from_names_and_types() {
        let mut id = col("id", "integer");
        id.primary_key = true;
        id.auto_increment = true;
        let details = TableDetails {
            columns: vec![
                id,
                col("email", "character varying(40)"),
                col("customer_id", "integer"),
                col("status", "enum('new','paid')"),
                col("created_at", "timestamp"),
            ],
            indexes: vec![IndexInfo {
                name: "u".into(),
                columns: vec!["email".into()],
                unique: true,
                ..Default::default()
            }],
            foreign_keys: vec![ForeignKeyInfo {
                columns: vec!["customer_id".into()],
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                ..Default::default()
            }],
            comment: None,
        };
        let plan = default_plan(DbKind::MySql, &details);
        let kinds: Vec<&Kind> = plan.iter().map(|p| &p.kind).collect();
        assert_eq!(kinds[0], &Kind::Skip);
        assert_eq!(kinds[1], &Kind::Email);
        assert!(plan[1].unique);
        assert_eq!(kinds[2], &Kind::Reference { table: "customers".into(), column: "id".into() });
        assert_eq!(kinds[3], &Kind::OneOf(vec!["new".into(), "paid".into()]));
        assert!(matches!(kinds[4], Kind::Timestamp { .. }));

        let refs = HashMap::from([("customer_id".to_string(), vec![Cell::Text("7".into())])]);
        let mut g = Generator::new(plan.clone(), 1, refs.clone());
        let rows: Vec<Vec<Cell>> = (0..50).map(|_| g.row()).collect();
        assert_eq!(rows[0].len(), 4, "the identity column is left to the server");
        let emails: std::collections::HashSet<String> =
            rows.iter().map(|r| if let Cell::Text(s) = &r[0] { s.clone() } else { panic!() }).collect();
        assert_eq!(emails.len(), 50);
        assert!(emails.iter().all(|e| e.len() <= 40 && e.ends_with("@example.com")));
        assert!(rows.iter().all(|r| matches!(&r[1], Cell::Null) || r[1] == Cell::Text("7".into())));
        let again: Vec<Vec<Cell>> = {
            let mut g = Generator::new(plan, 1, refs);
            (0..50).map(|_| g.row()).collect()
        };
        assert_eq!(rows, again, "a seed gives the same rows");
    }

    #[test]
    fn settings_round_trip_as_text() {
        for k in [
            Kind::Sequence { start: 5 },
            Kind::Integer { min: -3, max: 9 },
            Kind::Decimal { min: 1.5, max: 20.25, scale: 2 },
            Kind::Words { min: 2, max: 5 },
            Kind::Date { from: "2021-01-01".into(), to: "2021-12-31".into() },
            Kind::OneOf(vec!["x".into(), "y z".into()]),
            Kind::Reference { table: "customers".into(), column: "id".into() },
        ] {
            assert_eq!(Kind::parse(k.index(), &k.params(), 2), Ok(k.clone()));
        }
        assert!(Kind::parse(2, "1-5", 0).is_err());
        assert!(Kind::parse(15, "2020-13-01..2021-01-01", 0).is_err());
    }

    #[test]
    fn dates_round_trip() {
        for d in ["1970-01-01", "2000-02-29", "2024-12-31", "1999-03-01"] {
            assert_eq!(date_from_days(days_from_date(d).unwrap()), d);
        }
        assert_eq!(days_from_date("1970-01-02"), Some(1));
        assert_eq!(enum_values("Enum8('a' = 1, 'it''s' = 2)"), Some(vec!["a".into(), "it's".into()]));
    }
}
