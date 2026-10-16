//! Opens files for import and writes files for export: Parquet through
//! DuckDB, every other format through [`dbman_core::import`] and
//! [`dbman_core::export`].

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};

use dbman_core::export::{Format, Writer};
use dbman_core::import::{self, CsvOptions, Field, ImportFormat, RecordSource};
use dbman_core::sql::Dialect;
use dbman_core::{Cell, ColumnMeta, DbError, DbResult, ObjectPath, QueryResult};

/// Opens `path`; `read` follows how much of the file has been read.
pub fn open(
    path: &Path,
    format: ImportFormat,
    csv: &CsvOptions,
    read: Arc<AtomicU64>,
) -> DbResult<Box<dyn RecordSource>> {
    match format {
        ImportFormat::Parquet => parquet(path, read),
        _ => import::open(path, format, csv, read),
    }
}

/// The sheet names of a workbook, for the import dialog.
pub fn sheets(path: &Path) -> DbResult<Vec<String>> {
    dbman_core::xlsx::sheets(path)
}

fn duck_err(e: duckdb::Error) -> DbError {
    DbError::new(format!("Could not read the Parquet file: {e}"))
}

struct ParquetSource {
    columns: Vec<String>,
    rx: Receiver<DbResult<Vec<Field>>>,
}

fn parquet(path: &Path, read: Arc<AtomicU64>) -> DbResult<Box<dyn RecordSource>> {
    let size = std::fs::metadata(path).map_err(|e| DbError::new(format!("Could not read the file: {e}")))?.len();
    let db = duckdb::Connection::open_in_memory().map_err(duck_err)?;
    let source = format!("read_parquet('{}')", path.to_string_lossy().replace('\'', "''"));
    let mut columns = Vec::new();
    let mut select = Vec::new();
    {
        let mut st = db.prepare(&format!("DESCRIBE SELECT * FROM {source}")).map_err(duck_err)?;
        let mut rows = st.query([]).map_err(duck_err)?;
        while let Some(row) = rows.next().map_err(duck_err)? {
            let name: String = row.get(0).map_err(duck_err)?;
            let ty: String = row.get(1).map_err(duck_err)?;
            let id = format!("\"{}\"", name.replace('"', "\"\""));
            // Bytes as `0x…` hex, which binary columns take; the rest as text.
            select.push(if ty == "BLOB" { format!("'0x' || hex({id})") } else { format!("CAST({id} AS VARCHAR)") });
            columns.push(name);
        }
    }
    if columns.is_empty() {
        return Err(DbError::new("The Parquet file has no columns."));
    }
    let total: i64 = db.query_row(&format!("SELECT COUNT(*) FROM {source}"), [], |r| r.get(0)).map_err(duck_err)?;
    let sql = format!("SELECT {} FROM {source}", select.join(", "));
    let width = columns.len();
    let (tx, rx) = sync_channel::<DbResult<Vec<Field>>>(1024);
    std::thread::spawn(move || {
        let result = (|| {
            let mut st = db.prepare(&sql).map_err(duck_err)?;
            let mut rows = st.query([]).map_err(duck_err)?;
            let mut done: i64 = 0;
            while let Some(row) = rows.next().map_err(duck_err)? {
                let fields = (0..width).map(|i| row.get::<_, Option<String>>(i)).collect::<Result<Vec<_>, _>>();
                if tx.send(fields.map_err(duck_err)).is_err() {
                    return Ok(());
                }
                done += 1;
                if total > 0 && done % 1024 == 0 {
                    read.store((done as f64 / total as f64 * size as f64) as u64, Ordering::Relaxed);
                }
            }
            read.store(size, Ordering::Relaxed);
            Ok(())
        })();
        if let Err(e) = result {
            let _ = tx.send(Err(e));
        }
    });
    Ok(Box::new(ParquetSource { columns, rx }))
}

impl RecordSource for ParquetSource {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> DbResult<Option<Vec<Field>>> {
        match self.rx.recv() {
            Ok(r) => r.map(Some),
            Err(_) => Ok(None),
        }
    }
}

/// Writes exported rows to a file in any [`Format`]: [`begin`](Self::begin)
/// with the columns, [`rows`](Self::rows) any number of times, then
/// [`finish`](Self::finish).
pub enum FileWriter {
    Core(Writer<BufWriter<File>>, PathBuf),
    Parquet(ParquetWriter),
}

fn write_err(path: &Path) -> impl Fn(std::io::Error) -> DbError + '_ {
    move |e| DbError::new(format!("Could not write {}: {e}", path.display()))
}

impl FileWriter {
    /// `table` names the target of SQL inserts and the sheets of a workbook.
    pub fn create(path: &Path, format: Format, dialect: Dialect, table: Option<ObjectPath>) -> DbResult<Self> {
        if format == Format::Parquet {
            return ParquetWriter::new(path).map(FileWriter::Parquet);
        }
        let file = File::create(path).map_err(|e| DbError::new(format!("Could not create {}: {e}", path.display())))?;
        let out = BufWriter::with_capacity(256 * 1024, file);
        Ok(FileWriter::Core(Writer::new(out, format, dialect, table), path.to_path_buf()))
    }

    pub fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        match self {
            FileWriter::Core(w, path) => w.begin(columns).map_err(write_err(path)),
            FileWriter::Parquet(w) => w.begin(columns),
        }
    }

    pub fn rows(&mut self, rows: &[Vec<Cell>]) -> DbResult<()> {
        match self {
            FileWriter::Core(w, path) => w.rows(rows).map_err(write_err(path)),
            FileWriter::Parquet(w) => w.rows(rows),
        }
    }

    /// Rows written so far.
    pub fn written(&self) -> u64 {
        match self {
            FileWriter::Core(w, _) => w.written(),
            FileWriter::Parquet(w) => w.rows,
        }
    }

    /// Bytes in the file so far, where that is known.
    pub fn bytes(&self) -> Option<u64> {
        match self {
            FileWriter::Core(w, _) => w.get_ref()?.get_ref().metadata().ok().map(|m| m.len()),
            FileWriter::Parquet(_) => None,
        }
    }

    pub fn finish(self) -> DbResult<()> {
        match self {
            FileWriter::Core(w, path) => w.finish().map(drop).map_err(write_err(&path)),
            FileWriter::Parquet(w) => w.finish(),
        }
    }
}

/// Writes a result set to `path`, as a grid's "Save to file" does.
pub fn write_result(
    r: &QueryResult,
    path: &Path,
    format: Format,
    dialect: Dialect,
    table: Option<&ObjectPath>,
) -> DbResult<()> {
    let mut w = FileWriter::create(path, format, dialect, table.cloned())?;
    w.begin(&r.columns)?;
    w.rows(&r.rows)?;
    w.finish()
}

fn duck_write_err(e: duckdb::Error) -> DbError {
    DbError::new(format!("Could not write the Parquet file: {e}"))
}

/// Gathers rows in a DuckDB file in the temporary folder, as text, and at
/// [`finish`](Self::finish) copies them to Parquet with the types the
/// source columns had, where every value fits that type.
pub struct ParquetWriter {
    db: duckdb::Connection,
    stage: PathBuf,
    path: PathBuf,
    columns: Vec<ColumnMeta>,
    /// Whether each column holds bytes, from its first value.
    blob: Vec<Option<bool>>,
    rows: u64,
}

impl Drop for ParquetWriter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.stage);
        let _ = std::fs::remove_file(self.stage.with_extension("duckdb.wal"));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The DuckDB types to try for a column, best first; text is the fallback.
fn candidates(c: &ColumnMeta) -> Vec<String> {
    let t = c.type_name.to_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    let zoned = t.contains("tz") || t.contains("zone") || t.contains("offset");
    if t.contains("bool") || base == "bit" {
        vec!["BOOLEAN".into()]
    } else if base == "date" || base == "date32" {
        vec!["DATE".into()]
    } else if t.contains("timestamp") || t.contains("datetime") {
        vec![if zoned { "TIMESTAMPTZ" } else { "TIMESTAMP" }.into()]
    } else if base == "time" || t == "time without time zone" {
        vec!["TIME".into()]
    } else if base == "uuid" || base == "uniqueidentifier" {
        vec!["UUID".into()]
    } else if t.contains("int") && !t.contains("interval") && !t.contains("point") {
        vec!["BIGINT".into(), "HUGEINT".into(), "DOUBLE".into()]
    } else if t.contains("float") || t.contains("double") || t.contains("real") {
        vec!["DOUBLE".into()]
    } else if c.numeric {
        // DECIMAL(38, s) with the largest scale seen, in `column_type`.
        vec!["DECIMAL".into(), "DOUBLE".into()]
    } else {
        Vec::new()
    }
}

impl ParquetWriter {
    fn new(path: &Path) -> DbResult<Self> {
        let stage = std::env::temp_dir().join(format!("dbman-export-{}.duckdb", uuid::Uuid::new_v4()));
        let db = duckdb::Connection::open(&stage).map_err(duck_write_err)?;
        Ok(Self { db, stage, path: path.to_path_buf(), columns: Vec::new(), blob: Vec::new(), rows: 0 })
    }

    fn begin(&mut self, columns: &[ColumnMeta]) -> DbResult<()> {
        if columns.is_empty() {
            return Err(DbError::new("A Parquet file needs at least one column."));
        }
        self.columns = columns.to_vec();
        self.blob = vec![None; columns.len()];
        let defs = (0..columns.len()).map(|i| format!("c{i} VARCHAR")).collect::<Vec<_>>().join(", ");
        self.db.execute_batch(&format!("CREATE TABLE stage ({defs})")).map_err(duck_write_err)
    }

    fn rows(&mut self, rows: &[Vec<Cell>]) -> DbResult<()> {
        let mut app = self.db.appender("stage").map_err(duck_write_err)?;
        for row in rows {
            let values = row.iter().zip(self.blob.iter_mut()).map(|(cell, blob)| {
                if blob.is_none() && !cell.is_null() {
                    *blob = Some(matches!(cell, Cell::Bytes(_)));
                }
                match (cell, *blob) {
                    (Cell::Null, _) => None,
                    (Cell::Bytes(b), Some(true)) => Some(hex(b)),
                    (Cell::Text(t), Some(true)) => Some(hex(t.as_bytes())),
                    (other, _) => other.as_text().map(|t| t.into_owned()),
                }
            });
            app.append_row(duckdb::appender_params_from_iter(values)).map_err(duck_write_err)?;
            self.rows += 1;
        }
        app.flush().map_err(duck_write_err)
    }

    /// The type the column is written as, and the expression that makes it.
    fn column_type(&self, i: usize) -> DbResult<String> {
        let c = format!("c{i}");
        if self.blob[i] == Some(true) {
            return Ok(format!("unhex({c})"));
        }
        for ty in candidates(&self.columns[i]) {
            let ty = if ty == "DECIMAL" {
                let scale: Option<i64> = self
                    .db
                    .query_row(
                        &format!("SELECT max(length(split_part(trim({c}), '.', 2))) FROM stage WHERE {c} LIKE '%.%'"),
                        [],
                        |r| r.get(0),
                    )
                    .map_err(duck_write_err)?;
                match scale.unwrap_or(0) {
                    s @ 0..=18 => format!("DECIMAL(38, {s})"),
                    _ => continue,
                }
            } else {
                ty
            };
            let misfits: i64 = self
                .db
                .query_row(
                    &format!("SELECT count(*) FROM stage WHERE {c} IS NOT NULL AND TRY_CAST({c} AS {ty}) IS NULL"),
                    [],
                    |r| r.get(0),
                )
                .map_err(duck_write_err)?;
            if misfits == 0 {
                return Ok(format!("CAST({c} AS {ty})"));
            }
        }
        Ok(c)
    }

    fn finish(self) -> DbResult<()> {
        let mut seen = std::collections::HashSet::new();
        let mut select = Vec::new();
        for (i, col) in self.columns.iter().enumerate() {
            let mut name = col.name.clone();
            let mut n = 1;
            while !seen.insert(name.to_lowercase()) {
                n += 1;
                name = format!("{}_{n}", col.name);
            }
            select.push(format!("{} AS \"{}\"", self.column_type(i)?, name.replace('"', "\"\"")));
        }
        let sql = format!(
            "COPY (SELECT {} FROM stage) TO {} (FORMAT parquet, COMPRESSION zstd)",
            select.join(", "),
            quote(&self.path.to_string_lossy())
        );
        self.db.execute_batch(&sql).map_err(duck_write_err)
    }
}
