//! Opens files for import: Parquet through an in-memory DuckDB, every other
//! format through [`dbman_core::import`].

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};

use dbman_core::import::{self, CsvOptions, Field, ImportFormat, RecordSource};
use dbman_core::{DbError, DbResult};

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
