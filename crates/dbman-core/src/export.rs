//! Exports a result set as CSV, TSV, JSON or SQL INSERT statements.

use crate::sql::Dialect;
use crate::{Cell, ObjectPath, QueryResult, dml};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Tsv,
    Json,
    /// INSERT statements into the given table.
    SqlInserts,
    Markdown,
}

impl Format {
    pub const ALL: [Format; 5] = [Format::Csv, Format::Tsv, Format::Json, Format::SqlInserts, Format::Markdown];

    pub fn label(self) -> &'static str {
        match self {
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
            Format::Json => "JSON",
            Format::SqlInserts => "SQL INSERT statements",
            Format::Markdown => "Markdown table",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Json => "json",
            Format::SqlInserts => "sql",
            Format::Markdown => "md",
        }
    }
}

/// Renders `r`. `table` names the target of SQL inserts (`None`: `exported`).
pub fn export(r: &QueryResult, format: Format, dialect: Dialect, table: Option<&ObjectPath>) -> String {
    match format {
        Format::Csv => delimited(r, ',', true),
        Format::Tsv => delimited(r, '\t', false),
        Format::Json => json(r),
        Format::SqlInserts => {
            let target = table.cloned().unwrap_or_else(|| ObjectPath::new("", "", "exported"));
            let mut out = String::new();
            for row in &r.rows {
                let values: Vec<dml::Value<'_>> =
                    r.columns.iter().zip(row).map(|(c, cell)| dml::Value::new(&c.name, cell, c.numeric)).collect();
                out.push_str(&dml::insert(dialect, &target, &values));
                out.push_str(";\n");
            }
            out
        }
        Format::Markdown => {
            let esc = |s: &str| s.replace('|', "\\|").replace('\n', " ");
            let mut out = format!("| {} |\n", r.columns.iter().map(|c| esc(&c.name)).collect::<Vec<_>>().join(" | "));
            out.push_str(&format!(
                "|{}\n",
                r.columns.iter().map(|c| if c.numeric { " ---: |" } else { " --- |" }).collect::<String>()
            ));
            for row in &r.rows {
                out.push_str(&format!(
                    "| {} |\n",
                    row.iter().map(|c| esc(&c.display())).collect::<Vec<_>>().join(" | ")
                ));
            }
            out
        }
    }
}

fn delimited(r: &QueryResult, sep: char, quote: bool) -> String {
    let field = |s: &str| -> String {
        if quote && (s.contains(sep) || s.contains('"') || s.contains('\n') || s.contains('\r')) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else if !quote {
            s.replace(['\t', '\n', '\r'], " ")
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    let line = |cells: Vec<String>| cells.join(&sep.to_string()) + "\n";
    out.push_str(&line(r.columns.iter().map(|c| field(&c.name)).collect()));
    for row in &r.rows {
        // NULL becomes an empty field.
        out.push_str(&line(row.iter().map(|c| c.as_text().map(|t| field(&t)).unwrap_or_default()).collect()));
    }
    out
}

fn json(r: &QueryResult) -> String {
    let rows: Vec<serde_json::Value> = r
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (c, cell) in r.columns.iter().zip(row) {
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
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnMeta, DbKind};

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
    fn markdown() {
        let s = export(&sample(), Format::Markdown, Dialect(DbKind::Postgres), None);
        assert!(s.starts_with("| id | name |\n| ---: | --- |\n| 1 | a,\"b\" |"), "{s}");
    }
}
