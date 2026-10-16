//! Exports rows as CSV, TSV, JSON, JSON Lines, SQL INSERT statements or Markdown.

use std::io::{self, Write};

use crate::sql::Dialect;
use crate::{Cell, ColumnMeta, ObjectPath, QueryResult, dml};

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
}

impl Format {
    pub const ALL: [Format; 6] =
        [Format::Csv, Format::Tsv, Format::Json, Format::JsonLines, Format::SqlInserts, Format::Markdown];

    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::JsonLines => "JSON Lines",
            Format::SqlInserts => "SQL INSERT statements",
            Format::Markdown => "Markdown table",
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
        }
    }
}

/// Renders `r`. `table` names the target of SQL inserts (`None`: `exported`).
pub fn export(r: &QueryResult, format: Format, dialect: Dialect, table: Option<&ObjectPath>) -> String {
    let mut w = Writer::new(Vec::new(), format, dialect, table.cloned());
    // Writing to memory cannot fail.
    let _ = w.begin(&r.columns).and_then(|()| w.rows(&r.rows));
    String::from_utf8(w.finish().unwrap_or_default()).unwrap_or_default()
}

/// Writes rows to `out` as they arrive, so a table of any size can be
/// exported page by page: [`begin`](Writer::begin) with the columns, then
/// [`rows`](Writer::rows) any number of times, then [`finish`](Writer::finish).
pub struct Writer<W: Write> {
    out: W,
    format: Format,
    dialect: Dialect,
    table: ObjectPath,
    columns: Vec<ColumnMeta>,
    rows: u64,
}

impl<W: Write> Writer<W> {
    /// `table` names the target of SQL inserts (`None`: `exported`).
    pub fn new(out: W, format: Format, dialect: Dialect, table: Option<ObjectPath>) -> Self {
        let table = table.unwrap_or_else(|| ObjectPath::new("", "", "exported"));
        Self { out, format, dialect, table, columns: Vec::new(), rows: 0 }
    }

    /// The output being written.
    pub fn get_ref(&self) -> &W {
        &self.out
    }

    /// Rows written so far.
    pub fn written(&self) -> u64 {
        self.rows
    }

    pub fn begin(&mut self, columns: &[ColumnMeta]) -> io::Result<()> {
        self.columns = columns.to_vec();
        match self.format {
            Format::Csv => {
                let line = delimited_line(columns.iter().map(|c| Some(c.name.as_str())), ',', true);
                self.out.write_all(line.as_bytes())
            }
            Format::Tsv => {
                let line = delimited_line(columns.iter().map(|c| Some(c.name.as_str())), '\t', false);
                self.out.write_all(line.as_bytes())
            }
            Format::Json => self.out.write_all(b"["),
            Format::JsonLines | Format::SqlInserts => Ok(()),
            Format::Markdown => {
                let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
                let head = columns.iter().map(|c| esc(&c.name)).collect::<Vec<_>>().join(" | ");
                let align: String = columns.iter().map(|c| if c.numeric { " ---: |" } else { " --- |" }).collect();
                write!(self.out, "| {head} |\n|{align}\n")
            }
        }
    }

    pub fn rows(&mut self, rows: &[Vec<Cell>]) -> io::Result<()> {
        for row in rows {
            match self.format {
                Format::Csv | Format::Tsv => {
                    let (sep, quote) = if self.format == Format::Csv { (',', true) } else { ('\t', false) };
                    let texts: Vec<Option<std::borrow::Cow<'_, str>>> = row.iter().map(Cell::as_text).collect();
                    // NULL becomes an empty field.
                    let line = delimited_line(texts.iter().map(|t| t.as_deref()), sep, quote);
                    self.out.write_all(line.as_bytes())?;
                }
                Format::Json | Format::JsonLines => {
                    let obj = json_object(&self.columns, row);
                    let text = serde_json::to_string(&obj).unwrap_or_default();
                    match self.format {
                        Format::Json if self.rows == 0 => write!(self.out, "\n  {text}")?,
                        Format::Json => write!(self.out, ",\n  {text}")?,
                        _ => writeln!(self.out, "{text}")?,
                    }
                }
                Format::SqlInserts => {
                    let values: Vec<dml::Value<'_>> = self
                        .columns
                        .iter()
                        .zip(row)
                        .map(|(c, cell)| dml::Value::new(&c.name, cell, c.numeric))
                        .collect();
                    writeln!(self.out, "{};", dml::insert(self.dialect, &self.table, &values))?;
                }
                Format::Markdown => {
                    let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
                    let cells = row.iter().map(|c| esc(&c.display())).collect::<Vec<_>>().join(" | ");
                    writeln!(self.out, "| {cells} |")?;
                }
            }
            self.rows += 1;
        }
        Ok(())
    }

    /// Closes the document and flushes; returns the output.
    pub fn finish(mut self) -> io::Result<W> {
        if self.format == Format::Json {
            self.out.write_all(if self.rows == 0 { b"]\n" } else { b"\n]\n" })?;
        }
        self.out.flush()?;
        Ok(self.out)
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
    fn markdown() {
        let s = export(&sample(), Format::Markdown, Dialect(DbKind::Postgres), None);
        assert!(s.starts_with("| id | name |\n| ---: | --- |\n| 1 | a,\"b\" |"), "{s}");
    }
}
