//! Exports rows as CSV, TSV, JSON, JSON Lines, SQL INSERT statements,
//! Markdown or an Excel workbook. Parquet is written by `dbman-drivers`.

use std::io::{self, Cursor, Seek, Write};

use crate::sql::Dialect;
use crate::{Cell, ColumnMeta, ObjectPath, QueryResult, dml, xlsx};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    /// One JSON object per line.
    JsonLines,
    /// INSERT statements into the given table.
    SqlInserts,
    Markdown,
    /// An Excel workbook (.xlsx).
    Excel,
    /// An Apache Parquet file, written by `dbman-drivers` through DuckDB.
    Parquet,
}

impl Format {
    pub const ALL: [Format; 8] = [
        Format::Csv,
        Format::Tsv,
        Format::Json,
        Format::JsonLines,
        Format::SqlInserts,
        Format::Markdown,
        Format::Excel,
        Format::Parquet,
    ];

    /// The formats that are text, for the clipboard.
    pub const TEXT: [Format; 6] =
        [Format::Csv, Format::Tsv, Format::Json, Format::JsonLines, Format::SqlInserts, Format::Markdown];

    pub fn is_text(self) -> bool {
        !matches!(self, Format::Excel | Format::Parquet)
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::JsonLines => "JSON Lines",
            Format::SqlInserts => "SQL INSERT statements",
            Format::Markdown => "Markdown table",
            Format::Excel => "Excel (.xlsx)",
            Format::Parquet => "Parquet",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::JsonLines => "jsonl",
            Format::SqlInserts => "sql",
            Format::Markdown => "md",
            Format::Excel => "xlsx",
            Format::Parquet => "parquet",
        }
    }
}

/// Renders `r` in a text format. `table` names the target of SQL inserts (`None`: `exported`).
pub fn export(r: &QueryResult, format: Format, dialect: Dialect, table: Option<&ObjectPath>) -> String {
    let mut w = Writer::new(Cursor::new(Vec::new()), format, dialect, table.cloned());
    // Writing to memory cannot fail.
    let _ = w.begin(&r.columns).and_then(|()| w.rows(&r.rows));
    String::from_utf8(w.finish().map(Cursor::into_inner).unwrap_or_default()).unwrap_or_default()
}

enum Out<W: Write + Seek> {
    Text(W),
    Excel(Box<xlsx::Writer<W>>),
}

/// Writes rows to `out` as they arrive, so a table of any size can be
/// exported page by page: [`begin`](Writer::begin) with the columns, then
/// [`rows`](Writer::rows) any number of times, then [`finish`](Writer::finish).
pub struct Writer<W: Write + Seek> {
    out: Out<W>,
    format: Format,
    dialect: Dialect,
    table: ObjectPath,
    columns: Vec<ColumnMeta>,
    rows: u64,
}

impl<W: Write + Seek> Writer<W> {
    /// `table` names the target of SQL inserts (`None`: `exported`) and the
    /// sheets of a workbook. Parquet is not written here: see `dbman-drivers`.
    pub fn new(out: W, format: Format, dialect: Dialect, table: Option<ObjectPath>) -> Self {
        let table = table.unwrap_or_else(|| ObjectPath::new("", "", "exported"));
        let out = match format {
            Format::Excel => Out::Excel(Box::new(xlsx::Writer::new(out, &table.name))),
            _ => Out::Text(out),
        };
        Self { out, format, dialect, table, columns: Vec::new(), rows: 0 }
    }

    /// The output being written.
    pub fn get_ref(&self) -> Option<&W> {
        match &self.out {
            Out::Text(w) => Some(w),
            Out::Excel(x) => x.get_ref(),
        }
    }

    /// Rows written so far.
    pub fn written(&self) -> u64 {
        self.rows
    }

    pub fn begin(&mut self, columns: &[ColumnMeta]) -> io::Result<()> {
        self.columns = columns.to_vec();
        let out = match &mut self.out {
            Out::Text(w) => w,
            Out::Excel(x) => return x.begin(columns),
        };
        match self.format {
            Format::Csv => {
                let line = delimited_line(columns.iter().map(|c| Some(c.name.as_str())), ',', true);
                out.write_all(line.as_bytes())
            }
            Format::Tsv => {
                let line = delimited_line(columns.iter().map(|c| Some(c.name.as_str())), '\t', false);
                out.write_all(line.as_bytes())
            }
            Format::Json => out.write_all(b"["),
            Format::JsonLines | Format::SqlInserts | Format::Excel => Ok(()),
            Format::Parquet => Err(io::Error::other("Parquet files are written through DuckDB.")),
            Format::Markdown => {
                let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
                let head = columns.iter().map(|c| esc(&c.name)).collect::<Vec<_>>().join(" | ");
                let align: String = columns.iter().map(|c| if c.numeric { " ---: |" } else { " --- |" }).collect();
                write!(out, "| {head} |\n|{align}\n")
            }
        }
    }

    pub fn rows(&mut self, rows: &[Vec<Cell>]) -> io::Result<()> {
        let out = match &mut self.out {
            Out::Text(w) => w,
            Out::Excel(x) => {
                x.rows(rows)?;
                self.rows += rows.len() as u64;
                return Ok(());
            }
        };
        for row in rows {
            match self.format {
                Format::Csv | Format::Tsv => {
                    let (sep, quote) = if self.format == Format::Csv { (',', true) } else { ('\t', false) };
                    let texts: Vec<Option<std::borrow::Cow<'_, str>>> = row.iter().map(Cell::as_text).collect();
                    // NULL becomes an empty field.
                    let line = delimited_line(texts.iter().map(|t| t.as_deref()), sep, quote);
                    out.write_all(line.as_bytes())?;
                }
                Format::Json | Format::JsonLines => {
                    let obj = json_object(&self.columns, row);
                    let text = serde_json::to_string(&obj).unwrap_or_default();
                    match self.format {
                        Format::Json if self.rows == 0 => write!(out, "\n  {text}")?,
                        Format::Json => write!(out, ",\n  {text}")?,
                        _ => writeln!(out, "{text}")?,
                    }
                }
                Format::SqlInserts => {
                    let values: Vec<dml::Value<'_>> = self
                        .columns
                        .iter()
                        .zip(row)
                        .map(|(c, cell)| dml::Value::new(&c.name, cell, c.numeric))
                        .collect();
                    writeln!(out, "{};", dml::insert(self.dialect, &self.table, &values))?;
                }
                Format::Excel | Format::Parquet => {}
                Format::Markdown => {
                    let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
                    let cells = row.iter().map(|c| esc(&c.display())).collect::<Vec<_>>().join(" | ");
                    writeln!(out, "| {cells} |")?;
                }
            }
            self.rows += 1;
        }
        Ok(())
    }

    /// Closes the document and flushes; returns the output.
    pub fn finish(self) -> io::Result<W> {
        match self.out {
            Out::Excel(x) => x.finish(),
            Out::Text(mut out) => {
                if self.format == Format::Json {
                    out.write_all(if self.rows == 0 { b"]\n" } else { b"\n]\n" })?;
                }
                out.flush()?;
                Ok(out)
            }
        }
    }
}

/// One CSV or TSV line; `None` is an empty field.
fn delimited_line<'a>(fields: impl Iterator<Item = Option<&'a str>>, sep: char, quote: bool) -> String {
    let field = |s: &str| -> String {
        if quote && (s.contains(sep) || s.contains('"') || s.contains('\n') || s.contains('\r')) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else if !quote {
            s.replace(['\t', '\n', '\r'], " ")
        } else {
            s.to_string()
        }
    };
    let mut line = fields.map(|f| f.map(field).unwrap_or_default()).collect::<Vec<_>>().join(&sep.to_string());
    line.push('\n');
    line
}

/// A row as a JSON object; numeric columns become JSON numbers when they parse.
fn json_object(columns: &[ColumnMeta], row: &[Cell]) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (c, cell) in columns.iter().zip(row) {
        let v = match cell {
            Cell::Null => serde_json::Value::Null,
            Cell::Text(s) if c.numeric => s
                .parse::<i64>()
                .map(serde_json::Value::from)
                .or_else(|_| s.parse::<f64>().map(serde_json::Value::from))
                .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
            other => serde_json::Value::String(other.as_text().unwrap_or_default().into_owned()),
        };
        obj.insert(c.name.clone(), v);
    }
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DbKind;

    fn sample() -> QueryResult {
        QueryResult {
            columns: vec![ColumnMeta::new("id", "int4"), ColumnMeta::new("name", "text")],
            rows: vec![
                vec![Cell::Text("1".into()), Cell::Text("a,\"b\"".into())],
                vec![Cell::Text("2".into()), Cell::Null],
            ],
            truncated: false,
        }
    }

    #[test]
    fn csv_quotes_when_needed() {
        let s = export(&sample(), Format::Csv, Dialect(DbKind::Postgres), None);
        assert_eq!(s, "id,name\n1,\"a,\"\"b\"\"\"\n2,\n");
    }

    #[test]
    fn json_keeps_numbers_and_nulls() {
        let s = export(&sample(), Format::Json, Dialect(DbKind::Postgres), None);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["id"], 1);
        assert_eq!(v[1]["name"], serde_json::Value::Null);
    }

    #[test]
    fn sql_inserts() {
        let t = ObjectPath::new("db", "public", "t");
        let s = export(&sample(), Format::SqlInserts, Dialect(DbKind::Postgres), Some(&t));
        assert_eq!(s.lines().next().unwrap(), "INSERT INTO \"public\".\"t\" (\"id\", \"name\") VALUES (1, 'a,\"b\"');");
        let s = export(&sample(), Format::SqlInserts, Dialect(DbKind::Postgres), None);
        assert!(s.starts_with("INSERT INTO \"exported\" ("), "{s}");
    }

    #[test]
    fn json_lines_and_empty_json() {
        let s = export(&sample(), Format::JsonLines, Dialect(DbKind::Postgres), None);
        assert_eq!(s, "{\"id\":1,\"name\":\"a,\\\"b\\\"\"}\n{\"id\":2,\"name\":null}\n");
        let empty = QueryResult { columns: sample().columns, ..Default::default() };
        assert_eq!(export(&empty, Format::Json, Dialect(DbKind::Postgres), None), "[]\n");
    }

    #[test]
    fn excel_reads_back() {
        let dir = std::env::temp_dir().join(format!("dbman-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.xlsx");
        let columns = vec![
            ColumnMeta::new("id", "int4"),
            ColumnMeta::new("name", "text"),
            ColumnMeta::new("born", "date"),
            ColumnMeta::new("seen", "timestamp"),
            ColumnMeta::new("ok", "bool"),
            ColumnMeta::new("data", "bytea"),
        ];
        let t = |s: &str| Cell::Text(s.into());
        let rows = vec![
            vec![
                t("1"),
                t("Анна & <Co>"),
                t("2024-02-29"),
                t("2024-03-01 09:30:15"),
                t("t"),
                Cell::Bytes(vec![1, 255]),
            ],
            vec![t("12345678901234567"), Cell::Null, t("1800-01-01"), Cell::Null, t("false"), Cell::Null],
        ];
        let file = std::fs::File::create(&path).unwrap();
        let mut w =
            Writer::new(file, Format::Excel, Dialect(crate::DbKind::Postgres), Some(ObjectPath::new("", "", "people")));
        w.begin(&columns).unwrap();
        w.rows(&rows).unwrap();
        assert_eq!(w.written(), 2);
        w.finish().unwrap();
        assert_eq!(xlsx::sheets(&path).unwrap(), ["people"]);
        let mut src = xlsx::open(&path, "", true, Default::default()).unwrap();
        assert_eq!(src.columns(), ["id", "name", "born", "seen", "ok", "data"]);
        let got = crate::import::preview(src.as_mut(), 10).unwrap();
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            got,
            [
                vec![s("1"), s("Анна & <Co>"), s("2024-02-29"), s("2024-03-01 09:30:15"), s("true"), s("0x01FF")],
                vec![s("12345678901234567"), None, s("1800-01-01"), None, s("false"), None],
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn markdown() {
        let s = export(&sample(), Format::Markdown, Dialect(DbKind::Postgres), None);
        assert!(s.starts_with("| id | name |\n| ---: | --- |\n| 1 | a,\"b\" |"), "{s}");
    }
}
