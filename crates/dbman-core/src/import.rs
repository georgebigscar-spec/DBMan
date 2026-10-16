//! Reads CSV, TSV, JSON, JSON Lines and Excel files record by record and turns
//! records into multi-row INSERT statements for a table. Parquet files are
//! read by the drivers crate, which has DuckDB.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sql::Dialect;
use crate::{Cell, ColumnInfo, DbError, DbKind, DbResult, ObjectPath, dml, result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    /// Delimited text: CSV, TSV or any single-character separator.
    Delimited,
    /// An array of objects, or JSON Lines (one object per line).
    Json,
    /// An Excel workbook (.xlsx): one sheet.
    Excel,
    /// An Apache Parquet file.
    Parquet,
}

impl ImportFormat {
    pub const ALL: [ImportFormat; 4] =
        [ImportFormat::Delimited, ImportFormat::Json, ImportFormat::Excel, ImportFormat::Parquet];

    pub fn label(self) -> &'static str {
        match self {
            ImportFormat::Delimited => "CSV / TSV",
            ImportFormat::Json => "JSON / JSON Lines",
            ImportFormat::Excel => "Excel (.xlsx)",
            ImportFormat::Parquet => "Parquet",
        }
    }

    /// Whether the file names its columns whatever the options say.
    pub fn named(self, csv: &CsvOptions) -> bool {
        match self {
            ImportFormat::Delimited | ImportFormat::Excel => csv.header,
            ImportFormat::Json | ImportFormat::Parquet => true,
        }
    }
}

/// How a delimited file or a workbook is laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    pub delimiter: char,
    /// The first record holds column names.
    pub header: bool,
    /// The workbook's sheet; empty for the first.
    pub sheet: String,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self { delimiter: ',', header: true, sheet: String::new() }
    }
}

/// The format and delimiter a file's extension suggests.
pub fn detect(path: &Path) -> (ImportFormat, CsvOptions) {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "json" | "jsonl" | "ndjson" => (ImportFormat::Json, CsvOptions::default()),
        "tsv" | "tab" => (ImportFormat::Delimited, CsvOptions { delimiter: '\t', ..Default::default() }),
        "xlsx" | "xlsm" => (ImportFormat::Excel, CsvOptions::default()),
        "parquet" | "parq" => (ImportFormat::Parquet, CsvOptions::default()),
        _ => (ImportFormat::Delimited, CsvOptions::default()),
    }
}

/// A value read from a file: `None` is NULL.
pub type Field = Option<String>;

/// Records of a file, read one at a time.
pub trait RecordSource: Send {
    /// Column names: the header, the JSON keys, or `column1`, `column2`… .
    fn columns(&self) -> &[String];
    /// The next record with one field per column, `None` at the end.
    fn next_record(&mut self) -> DbResult<Option<Vec<Field>>>;
}

/// A reader that counts the bytes read, for progress.
struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

fn io_err(e: io::Error) -> DbError {
    DbError::new(format!("Could not read the file: {e}"))
}

/// Opens `path`; `read` counts the bytes consumed so far.
pub fn open(
    path: &Path,
    format: ImportFormat,
    csv: &CsvOptions,
    read: Arc<AtomicU64>,
) -> DbResult<Box<dyn RecordSource>> {
    if format == ImportFormat::Excel {
        return crate::xlsx::open(path, &csv.sheet, csv.header, read);
    }
    let file = File::open(path).map_err(io_err)?;
    let reader = BufReader::with_capacity(256 * 1024, Counting { inner: file, read });
    from_reader(reader, format, csv)
}

/// Like [`open`], for any reader (tests, previews).
pub fn from_reader(
    mut reader: impl BufRead + Send + 'static,
    format: ImportFormat,
    csv: &CsvOptions,
) -> DbResult<Box<dyn RecordSource>> {
    skip_bom(&mut reader).map_err(io_err)?;
    match format {
        ImportFormat::Delimited => Ok(Box::new(CsvSource::new(reader, csv)?)),
        ImportFormat::Json => Ok(Box::new(JsonSource::new(reader)?)),
        ImportFormat::Excel | ImportFormat::Parquet => {
            Err(DbError::new(format!("{} files are read from a path.", format.label())))
        }
    }
}

fn skip_bom(r: &mut impl BufRead) -> io::Result<()> {
    if r.fill_buf()?.starts_with(&[0xEF, 0xBB, 0xBF]) {
        r.consume(3);
    }
    Ok(())
}

/// Reads up to `limit` records for a preview.
pub fn preview(source: &mut dyn RecordSource, limit: usize) -> DbResult<Vec<Vec<Field>>> {
    let mut rows = Vec::new();
    while rows.len() < limit {
        match source.next_record()? {
            Some(r) => rows.push(r),
            None => break,
        }
    }
    Ok(rows)
}

/// RFC 4180 records: quoted fields may hold separators, quotes (doubled) and line breaks.
struct CsvSource<R> {
    reader: R,
    delimiter: char,
    columns: Vec<String>,
    /// The first record when the file has no header row.
    pending: Option<Vec<String>>,
    line: u64,
}

impl<R: BufRead + Send> CsvSource<R> {
    fn new(reader: R, o: &CsvOptions) -> DbResult<Self> {
        let mut me = Self { reader, delimiter: o.delimiter, columns: Vec::new(), pending: None, line: 0 };
        let first = me.read_record()?.unwrap_or_default();
        if o.header {
            me.columns = first.into_iter().map(|c| c.trim().to_string()).collect();
        } else {
            me.columns = (1..=first.len()).map(|i| format!("column{i}")).collect();
            me.pending = Some(first);
        }
        Ok(me)
    }

    fn read_record(&mut self) -> DbResult<Option<Vec<String>>> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line).map_err(io_err)? == 0 {
                return Ok(None);
            }
            self.line += 1;
            // Skip blank lines between records.
            if !line.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
        }
        let start = self.line;
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        loop {
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if quoted {
                    if c == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            field.push('"');
                        } else {
                            quoted = false;
                        }
                    } else {
                        field.push(c);
                    }
                } else if c == '"' && field.is_empty() {
                    quoted = true;
                } else if c == self.delimiter {
                    fields.push(std::mem::take(&mut field));
                } else if c == '\n' || (c == '\r' && chars.peek().is_none_or(|n| *n == '\n')) {
                    // End of the record.
                } else {
                    field.push(c);
                }
            }
            if !quoted {
                break;
            }
            // A quoted field goes on to the next line.
            line.clear();
            if self.reader.read_line(&mut line).map_err(io_err)? == 0 {
                return Err(DbError::new(format!("Line {start}: a quoted field is not closed.")));
            }
            self.line += 1;
        }
        fields.push(field);
        Ok(Some(fields))
    }
}

impl<R: BufRead + Send> RecordSource for CsvSource<R> {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> DbResult<Option<Vec<Field>>> {
        let record = match self.pending.take() {
            Some(r) => Some(r),
            None => self.read_record()?,
        };
        Ok(record.map(|mut r| {
            // Short records get NULLs; extra fields are dropped.
            r.resize(self.columns.len(), String::new());
            r.into_iter().map(Some).collect()
        }))
    }
}

/// An array of objects (read whole) or JSON Lines (read line by line).
struct JsonSource<R> {
    lines: Option<R>,
    array: std::vec::IntoIter<serde_json::Value>,
    /// Objects read ahead to find the column names.
    ahead: std::collections::VecDeque<serde_json::Map<String, serde_json::Value>>,
    columns: Vec<String>,
    line: u64,
}

/// Objects read before the column list is fixed; keys first seen later are ignored.
const SAMPLE: usize = 100;

impl<R: BufRead + Send> JsonSource<R> {
    fn new(mut reader: R) -> DbResult<Self> {
        let starts_array = loop {
            let buf = reader.fill_buf().map_err(io_err)?;
            match buf.iter().position(|b| !b.is_ascii_whitespace()) {
                Some(i) => {
                    let array = buf[i] == b'[';
                    reader.consume(i);
                    break array;
                }
                None if buf.is_empty() => break false,
                None => {
                    let n = buf.len();
                    reader.consume(n);
                }
            }
        };
        let mut me = if starts_array {
            let values: Vec<serde_json::Value> =
                serde_json::from_reader(reader).map_err(|e| DbError::new(format!("Not a valid JSON array: {e}")))?;
            Self { lines: None, array: values.into_iter(), ahead: Default::default(), columns: Vec::new(), line: 0 }
        } else {
            Self {
                lines: Some(reader),
                array: Vec::new().into_iter(),
                ahead: Default::default(),
                columns: Vec::new(),
                line: 0,
            }
        };
        while me.ahead.len() < SAMPLE {
            let Some(obj) = me.next_object()? else { break };
            for k in obj.keys() {
                if !me.columns.contains(k) {
                    me.columns.push(k.clone());
                }
            }
            me.ahead.push_back(obj);
        }
        Ok(me)
    }

    fn next_object(&mut self) -> DbResult<Option<serde_json::Map<String, serde_json::Value>>> {
        let value = match &mut self.lines {
            None => match self.array.next() {
                Some(v) => v,
                None => return Ok(None),
            },
            Some(r) => {
                let mut line = String::new();
                loop {
                    line.clear();
                    if r.read_line(&mut line).map_err(io_err)? == 0 {
                        return Ok(None);
                    }
                    self.line += 1;
                    if !line.trim().is_empty() {
                        break;
                    }
                }
                serde_json::from_str(&line).map_err(|e| DbError::new(format!("Line {}: {e}", self.line)))?
            }
        };
        match value {
            serde_json::Value::Object(o) => Ok(Some(o)),
            other => Err(DbError::new(format!("Expected an object for each row, found {other}"))),
        }
    }
}

impl<R: BufRead + Send> RecordSource for JsonSource<R> {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> DbResult<Option<Vec<Field>>> {
        let obj = match self.ahead.pop_front() {
            Some(o) => o,
            None => match self.next_object()? {
                Some(o) => o,
                None => return Ok(None),
            },
        };
        Ok(Some(
            self.columns
                .iter()
                .map(|c| match obj.get(c) {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    // Numbers and booleans as written; nested values as JSON text.
                    Some(v) => Some(v.to_string()),
                })
                .collect(),
        ))
    }
}

/// Where a table column's values come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMap {
    pub column: String,
    /// Index of the file's column; `None` leaves the column to its default.
    pub source: Option<usize>,
    pub numeric: bool,
    /// `0x…` hex text becomes bytes.
    pub binary: bool,
    pub auto_increment: bool,
}

/// Matches table columns to file columns by name (ignoring case, spaces and
/// underscores), or by position when the file has no names.
pub fn auto_map(table: &[ColumnInfo], file: &[String], file_has_names: bool) -> Vec<ColumnMap> {
    let norm = |s: &str| s.chars().filter(|c| !matches!(c, ' ' | '_' | '-')).collect::<String>().to_lowercase();
    table
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let source = if file_has_names {
                file.iter().position(|f| *f == c.name).or_else(|| file.iter().position(|f| norm(f) == norm(&c.name)))
            } else {
                (i < file.len()).then_some(i)
            };
            ColumnMap {
                column: c.name.clone(),
                source,
                numeric: result::is_numeric_type(&c.data_type),
                binary: is_binary_type(&c.data_type),
                auto_increment: c.auto_increment,
            }
        })
        .collect()
}

/// Binary column types of the supported engines.
pub fn is_binary_type(type_name: &str) -> bool {
    let t = type_name.trim().to_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    matches!(base, "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "image")
}

/// Turns file fields into cells: empty text is NULL when `empty_as_null`,
/// and `0x…` hex in binary columns becomes bytes.
pub fn cell(field: &Field, map: &ColumnMap, empty_as_null: bool) -> Cell {
    match field {
        None => Cell::Null,
        Some(s) if empty_as_null && s.is_empty() => Cell::Null,
        Some(s) if map.binary => match parse_hex(s) {
            Some(b) => Cell::Bytes(b),
            None => Cell::Text(s.clone()),
        },
        Some(s) => Cell::Text(s.clone()),
    }
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if h.len() % 2 != 0 {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

/// `INSERT INTO t (a, b) VALUES (…), (…)` for `rows`, which hold one field per
/// file column. Only mapped columns are inserted.
pub fn insert_rows(
    d: Dialect,
    table: &ObjectPath,
    columns: &[ColumnMap],
    rows: &[Vec<Field>],
    empty_as_null: bool,
) -> String {
    let mapped: Vec<(&ColumnMap, usize)> = columns.iter().filter_map(|c| c.source.map(|s| (c, s))).collect();
    let names = mapped.iter().map(|(c, _)| d.ident(&c.column)).collect::<Vec<_>>().join(", ");
    let mut sql = format!("INSERT INTO {} ({names}) VALUES", d.qualified(table));
    for (i, row) in rows.iter().enumerate() {
        sql.push_str(if i == 0 { "\n(" } else { ",\n(" });
        for (j, (c, src)) in mapped.iter().enumerate() {
            if j > 0 {
                sql.push_str(", ");
            }
            let field = row.get(*src).cloned().flatten();
            sql.push_str(&dml::literal(d, &cell(&field, c, empty_as_null), c.numeric));
        }
        sql.push(')');
    }
    sql
}

/// Statements to run before and after the inserts, when values go into
/// generated columns: SQL Server needs IDENTITY_INSERT, and PostgreSQL's
/// sequences must move past the imported values.
pub fn identity_statements(kind: DbKind, table: &ObjectPath, columns: &[ColumnMap]) -> (Vec<String>, Vec<String>) {
    let d = Dialect(kind);
    let generated: Vec<&ColumnMap> = columns.iter().filter(|c| c.source.is_some() && c.auto_increment).collect();
    if generated.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let q = d.qualified(table);
    match kind {
        DbKind::MsSql => (vec![format!("SET IDENTITY_INSERT {q} ON")], vec![format!("SET IDENTITY_INSERT {q} OFF")]),
        DbKind::Postgres => {
            let after = generated
                .iter()
                .map(|c| {
                    let col = d.ident(&c.column);
                    format!(
                        "SELECT setval(pg_get_serial_sequence({}, {}), COALESCE(MAX({col}), 1), MAX({col}) IS NOT NULL) FROM {q}",
                        d.string(&q),
                        d.string(&c.column)
                    )
                })
                .collect();
            (Vec::new(), after)
        }
        _ => (Vec::new(), Vec::new()),
    }
}

/// Rows per INSERT: SQL Server allows at most 1000 rows in a VALUES list.
pub fn default_batch(kind: DbKind) -> usize {
    match kind {
        DbKind::MsSql => 1000,
        DbKind::ClickHouse => 5000,
        _ => 500,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, format: ImportFormat, o: &CsvOptions) -> (Vec<String>, Vec<Vec<Field>>) {
        let mut s = from_reader(io::Cursor::new(text.as_bytes().to_vec()), format, o).unwrap();
        let rows = preview(s.as_mut(), usize::MAX).unwrap();
        (s.columns().to_vec(), rows)
    }

    fn f(s: &str) -> Field {
        Some(s.into())
    }

    #[test]
    fn csv_quotes_newlines_and_bom() {
        let text = "\u{feff}id,name,note\r\n1,\"a,\"\"b\"\"\",\"two\nlines\"\r\n\r\n2,,x\n3\n";
        let (cols, rows) = read(text, ImportFormat::Delimited, &CsvOptions::default());
        assert_eq!(cols, ["id", "name", "note"]);
        assert_eq!(rows[0], [f("1"), f("a,\"b\""), f("two\nlines")]);
        assert_eq!(rows[1], [f("2"), f(""), f("x")]);
        assert_eq!(rows[2], [f("3"), f(""), f("")]);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn tsv_without_header() {
        let o = CsvOptions { delimiter: '\t', header: false, ..Default::default() };
        let (cols, rows) = read("1\ta\n2\tb\n", ImportFormat::Delimited, &o);
        assert_eq!(cols, ["column1", "column2"]);
        assert_eq!(rows, [[f("1"), f("a")], [f("2"), f("b")]]);
    }

    #[test]
    fn unclosed_quote_is_an_error() {
        let mut s = from_reader(io::Cursor::new(b"a\n\"x\n".to_vec()), ImportFormat::Delimited, &CsvOptions::default())
            .unwrap();
        assert!(s.next_record().unwrap_err().message.contains("not closed"));
    }

    #[test]
    fn json_array_and_lines() {
        let o = CsvOptions::default();
        let (cols, rows) =
            read(r#" [{"id": 1, "name": "a"}, {"id": 2, "extra": {"k": true}}]"#, ImportFormat::Json, &o);
        assert_eq!(cols, ["id", "name", "extra"]);
        assert_eq!(rows[1], [f("2"), None, f(r#"{"k":true}"#)]);
        let (cols, rows) = read("{\"b\":null,\"a\":\"x\"}\n\n{\"a\":1.5}\n", ImportFormat::Json, &o);
        assert_eq!(cols, ["b", "a"]);
        assert_eq!(rows, [[None, f("x")], [None, f("1.5")]]);
    }

    #[test]
    fn maps_by_name_and_builds_inserts() {
        let table = vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), auto_increment: true, ..Default::default() },
            ColumnInfo { name: "full_name".into(), data_type: "text".into(), ..Default::default() },
            ColumnInfo { name: "photo".into(), data_type: "bytea".into(), ..Default::default() },
            ColumnInfo { name: "missing".into(), data_type: "text".into(), ..Default::default() },
        ];
        let file = vec!["Full Name".to_string(), "ID".to_string(), "photo".to_string()];
        let map = auto_map(&table, &file, true);
        assert_eq!(map.iter().map(|m| m.source).collect::<Vec<_>>(), [Some(1), Some(0), Some(2), None]);
        let t = ObjectPath::new("db", "public", "people");
        let rows = vec![vec![f("O'Hara"), f("7"), f("0x01ff")], vec![f(""), f("8"), None]];
        let sql = insert_rows(Dialect(DbKind::Postgres), &t, &map, &rows, true);
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"people\" (\"id\", \"full_name\", \"photo\") VALUES\n(7, 'O''Hara', '\\x01FF'::bytea),\n(8, NULL, NULL)"
        );
        let (before, after) = identity_statements(DbKind::Postgres, &t, &map);
        assert!(before.is_empty());
        assert_eq!(
            after,
            [
                "SELECT setval(pg_get_serial_sequence('\"public\".\"people\"', 'id'), COALESCE(MAX(\"id\"), 1), MAX(\"id\") IS NOT NULL) FROM \"public\".\"people\""
            ]
        );
        let (before, after) = identity_statements(DbKind::MsSql, &t, &map);
        assert_eq!(before, ["SET IDENTITY_INSERT [public].[people] ON"]);
        assert_eq!(after, ["SET IDENTITY_INSERT [public].[people] OFF"]);
    }

    #[test]
    fn positional_map_without_names() {
        let table = vec![
            ColumnInfo { name: "a".into(), data_type: "int".into(), ..Default::default() },
            ColumnInfo { name: "b".into(), data_type: "int".into(), ..Default::default() },
        ];
        let map = auto_map(&table, &["column1".into()], false);
        assert_eq!(map[0].source, Some(0));
        assert_eq!(map[1].source, None);
    }

    #[test]
    fn detects_by_extension() {
        assert_eq!(detect(Path::new("x.ndjson")).0, ImportFormat::Json);
        assert_eq!(detect(Path::new("x.TSV")).1.delimiter, '\t');
        assert_eq!(detect(Path::new("x.csv")).1.delimiter, ',');
    }
}
