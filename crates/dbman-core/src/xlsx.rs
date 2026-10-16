//! Reads the sheets of an Excel workbook (.xlsx, .xlsm) row by row: shared
//! strings and the date formats are read first, then the sheet is parsed on
//! its own thread so a large one never sits in memory whole.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};
use zip::ZipArchive;

mod write;
pub use write::Writer;

use crate::DbError;
use crate::DbResult;
use crate::import::{Field, RecordSource};

type Archive = ZipArchive<BufReader<File>>;

fn zip_err(e: zip::result::ZipError) -> DbError {
    DbError::new(format!("Not an Excel workbook (.xlsx): {e}"))
}

fn xml_err(e: quick_xml::Error) -> DbError {
    DbError::new(format!("The workbook is damaged: {e}"))
}

fn archive(path: &Path) -> DbResult<Archive> {
    let file = File::open(path).map_err(|e| DbError::new(format!("Could not read the file: {e}")))?;
    ZipArchive::new(BufReader::new(file)).map_err(zip_err)
}

/// A part of the workbook read whole; `None` when it is not there.
fn part(zip: &mut Archive, name: &str) -> DbResult<Option<Vec<u8>>> {
    let mut f = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(zip_err(e)),
    };
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| DbError::new(format!("Could not read the file: {e}")))?;
    Ok(Some(buf))
}

fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
}

/// `&amp;`, `&#10;` and the like.
fn reference(r: &BytesRef) -> String {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return c.to_string();
    }
    match r.as_ref() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => "",
    }
    .to_string()
}

/// A sheet's name and where its XML is.
struct Sheet {
    name: String,
    part: String,
}

struct Workbook {
    zip: Archive,
    sheets: Vec<Sheet>,
    date1904: bool,
}

fn workbook(path: &Path) -> DbResult<Workbook> {
    let mut zip = archive(path)?;
    let book = part(&mut zip, "xl/workbook.xml")?.ok_or_else(|| DbError::new("Not an Excel workbook (.xlsx)."))?;
    let rels = part(&mut zip, "xl/_rels/workbook.xml.rels")?.unwrap_or_default();
    let mut targets = HashMap::new();
    let mut r = Reader::from_reader(rels.as_slice());
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(xml_err)? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(t)) = (attr(&e, "Id"), attr(&e, "Target")) {
                    let t = match t.strip_prefix('/') {
                        Some(abs) => abs.to_string(),
                        None => format!("xl/{t}"),
                    };
                    targets.insert(id, t);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let mut sheets = Vec::new();
    let mut date1904 = false;
    let mut r = Reader::from_reader(book.as_slice());
    loop {
        match r.read_event_into(&mut buf).map_err(xml_err)? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "workbookPr" => date1904 = matches!(attr(&e, "date1904").as_deref(), Some("1" | "true")),
                "sheet" => {
                    let name = attr(&e, "name").unwrap_or_default();
                    // `r:id`: the local name is `id`.
                    if let Some(part) = attr(&e, "id").and_then(|id| targets.get(&id).cloned()) {
                        sheets.push(Sheet { name, part });
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(Workbook { zip, sheets, date1904 })
}

/// The workbook's sheet names, in order.
pub fn sheets(path: &Path) -> DbResult<Vec<String>> {
    Ok(workbook(path)?.sheets.into_iter().map(|s| s.name).collect())
}

fn shared_strings(zip: &mut Archive) -> DbResult<Vec<String>> {
    let Some(xml) = part(zip, "xl/sharedStrings.xml")? else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    let mut r = Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    let mut current = String::new();
    // Inside `<t>`, and inside phonetic hints (`<rPh>`), which are not the text.
    let (mut in_t, mut in_phonetic) = (false, false);
    loop {
        match r.read_event_into(&mut buf).map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                "si" => current.clear(),
                "t" => in_t = true,
                "rPh" => in_phonetic = true,
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == "si" => out.push(String::new()),
            Event::End(e) => match e.local_name().as_ref() {
                "si" => out.push(std::mem::take(&mut current)),
                "t" => in_t = false,
                "rPh" => in_phonetic = false,
                _ => {}
            },
            Event::Text(t) if in_t && !in_phonetic => current.push_str(&t.xml10_content()),
            Event::CData(t) if in_t && !in_phonetic => current.push_str(&t.xml10_content()),
            Event::GeneralRef(g) if in_t && !in_phonetic => current.push_str(&reference(&g)),
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// How a number in a cell style reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberKind {
    Plain,
    Date,
    DateTime,
    Time,
}

/// Built-in format ids that show dates or times.
fn builtin_kind(id: u32) -> NumberKind {
    match id {
        14..=17 | 27..=31 | 34..=36 | 50..=58 => NumberKind::Date,
        22 => NumberKind::DateTime,
        18..=21 | 32 | 33 | 45..=47 => NumberKind::Time,
        _ => NumberKind::Plain,
    }
}

/// Whether a custom format code shows a date or a time.
fn code_kind(code: &str) -> NumberKind {
    // Leave out quoted text, `[Red]`-like sections and escaped characters.
    let mut plain = String::new();
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                }
            }
            '[' => {
                let inner: String = chars.by_ref().take_while(|c| *c != ']').collect();
                // `[h]:mm` is an elapsed time.
                if matches!(inner.to_lowercase().as_str(), "h" | "hh" | "m" | "mm" | "s" | "ss") {
                    plain.push('h');
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            c => plain.push(c.to_ascii_lowercase()),
        }
    }
    let plain = plain.split(';').next().unwrap_or("");
    let date = plain.contains('y') || plain.contains('d');
    let time = plain.contains('h') || plain.contains('s');
    match (date, time) {
        (true, true) => NumberKind::DateTime,
        (true, false) => NumberKind::Date,
        (false, true) => NumberKind::Time,
        // `mmm` alone is a month.
        (false, false) if plain.contains("mmm") => NumberKind::Date,
        _ => NumberKind::Plain,
    }
}

/// What each cell style index shows numbers as.
fn styles(zip: &mut Archive) -> DbResult<Vec<NumberKind>> {
    let Some(xml) = part(zip, "xl/styles.xml")? else { return Ok(Vec::new()) };
    let mut custom = HashMap::new();
    let mut out = Vec::new();
    let mut in_xfs = false;
    let mut r = Reader::from_reader(xml.as_slice());
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).map_err(xml_err)? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "numFmt" => {
                    if let (Some(id), Some(code)) = (attr(&e, "numFmtId"), attr(&e, "formatCode"))
                        && let Ok(id) = id.parse::<u32>()
                    {
                        custom.insert(id, code_kind(&code));
                    }
                }
                "cellXfs" => in_xfs = true,
                "xf" if in_xfs => {
                    let id = attr(&e, "numFmtId").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                    out.push(custom.get(&id).copied().unwrap_or_else(|| builtin_kind(id)));
                }
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == "cellXfs" => in_xfs = false,
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// An Excel serial day number as a date, date and time, or time.
fn serial_text(v: f64, kind: NumberKind, date1904: bool) -> String {
    let days = v.floor() as i64;
    let mut secs = ((v - v.floor()) * 86_400.0).round() as i64;
    let mut days = days;
    if secs >= 86_400 {
        days += 1;
        secs -= 86_400;
    }
    let time = format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60);
    // Days since 1970-01-01. The 1900 system counts a 29 February 1900 that never was.
    let unix = if date1904 {
        days - 24_107
    } else if days < 61 {
        days - 25_568
    } else {
        days - 25_569
    };
    let date = crate::generate::date_from_days(unix);
    match kind {
        NumberKind::Date if secs == 0 => date,
        NumberKind::Time if days == 0 => time,
        _ => format!("{date} {time}"),
    }
}

/// `C7` is column 2 (from 0).
fn column_of(cell: &str) -> Option<usize> {
    let mut n = 0usize;
    let mut any = false;
    for c in cell.chars() {
        if c.is_ascii_alphabetic() {
            n = n * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
            any = true;
        } else {
            break;
        }
    }
    any.then(|| n - 1)
}

/// Counts the sheet's XML read, scaled to the file's size, for progress.
struct Scaled<R> {
    inner: R,
    done: u64,
    total: u64,
    file: u64,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Scaled<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.done += n as u64;
        if self.total > 0 {
            let part = (self.done as f64 / self.total as f64).min(1.0);
            self.read.store((part * self.file as f64) as u64, Ordering::Relaxed);
        }
        Ok(n)
    }
}

/// What a sheet's parser sends: a row, or the sheet's width first.
enum Message {
    Width(usize),
    Row(Vec<Field>),
}

struct Parser {
    strings: Vec<String>,
    styles: Vec<NumberKind>,
    date1904: bool,
}

impl Parser {
    fn run(&self, xml: impl Read, tx: &SyncSender<DbResult<Message>>) -> DbResult<()> {
        let mut r = Reader::from_reader(BufReader::with_capacity(256 * 1024, xml));
        let mut buf = Vec::new();
        let mut row: Vec<Field> = Vec::new();
        // The cell being read: its column, type and style.
        let mut col = 0usize;
        let mut kind = String::new();
        let mut style = 0usize;
        let mut value: Option<String> = None;
        let (mut in_v, mut in_t) = (false, false);
        loop {
            match r.read_event_into(&mut buf).map_err(xml_err)? {
                Event::Start(e) => match e.local_name().as_ref() {
                    "row" => row.clear(),
                    "c" => {
                        col = attr(&e, "r").and_then(|c| column_of(&c)).unwrap_or(row.len());
                        kind = attr(&e, "t").unwrap_or_default();
                        style = attr(&e, "s").and_then(|s| s.parse().ok()).unwrap_or(0);
                        value = None;
                    }
                    "v" => in_v = true,
                    "t" => in_t = true,
                    _ => {}
                },
                Event::Empty(e) => match e.local_name().as_ref() {
                    "dimension" => {
                        // `A1:D100`: four columns.
                        if let Some(w) =
                            attr(&e, "ref").and_then(|r| r.split(':').nth(1).and_then(column_of)).map(|c| c + 1)
                            && tx.send(Ok(Message::Width(w))).is_err()
                        {
                            return Ok(());
                        }
                    }
                    // An empty row sends nothing.
                    "row" => {}
                    _ => {}
                },
                Event::Text(t) if in_v || in_t => value.get_or_insert_default().push_str(&t.xml10_content()),
                Event::GeneralRef(g) if in_v || in_t => value.get_or_insert_default().push_str(&reference(&g)),
                Event::End(e) => match e.local_name().as_ref() {
                    "v" => in_v = false,
                    "t" => in_t = false,
                    "c" => {
                        let field = self.field(&kind, style, value.take());
                        if field.is_some() {
                            if row.len() <= col {
                                row.resize(col + 1, None);
                            }
                            row[col] = field;
                        }
                    }
                    "row"
                        if row.iter().any(Option::is_some)
                            && tx.send(Ok(Message::Row(std::mem::take(&mut row)))).is_err() =>
                    {
                        return Ok(());
                    }
                    _ => {}
                },
                Event::Eof => return Ok(()),
                _ => {}
            }
            buf.clear();
        }
    }

    fn field(&self, kind: &str, style: usize, value: Option<String>) -> Field {
        let v = value?;
        match kind {
            "s" => v.trim().parse::<usize>().ok().and_then(|i| self.strings.get(i).cloned()),
            "b" => Some(if v.trim() == "1" { "true".into() } else { "false".into() }),
            "e" => None,
            "str" | "inlineStr" | "d" => Some(v),
            _ => {
                let number_kind = self.styles.get(style).copied().unwrap_or(NumberKind::Plain);
                match (number_kind, v.trim().parse::<f64>()) {
                    (NumberKind::Plain, _) | (_, Err(_)) => Some(v.trim().to_string()),
                    (k, Ok(n)) => Some(serial_text(n, k, self.date1904)),
                }
            }
        }
    }
}

struct XlsxSource {
    rx: Receiver<DbResult<Message>>,
    columns: Vec<String>,
    /// The first row when it holds data, not names.
    pending: Option<Vec<Field>>,
}

/// Opens a sheet (the first when `sheet` is empty); `header` takes column names from its first row.
pub fn open(path: &Path, sheet: &str, header: bool, read: Arc<AtomicU64>) -> DbResult<Box<dyn RecordSource>> {
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let Workbook { mut zip, sheets, date1904 } = workbook(path)?;
    let chosen =
        if sheet.is_empty() { sheets.first() } else { sheets.iter().find(|s| s.name == sheet) }.ok_or_else(|| {
            match sheet {
                "" => DbError::new("The workbook has no sheets."),
                s => DbError::new(format!("The workbook has no sheet {s}.")),
            }
        })?;
    let part = chosen.part.clone();
    let parser = Parser { strings: shared_strings(&mut zip)?, styles: styles(&mut zip)?, date1904 };
    let (tx, rx) = sync_channel::<DbResult<Message>>(1024);
    std::thread::spawn(move || {
        let result = match zip.by_name(&part) {
            Ok(f) => {
                let total = f.size();
                parser.run(Scaled { inner: f, done: 0, total, file: file_size, read }, &tx)
            }
            Err(e) => Err(zip_err(e)),
        };
        if let Err(e) = result {
            let _ = tx.send(Err(e));
        }
    });

    let mut width = 0;
    let first = loop {
        match rx.recv() {
            Ok(Ok(Message::Width(w))) => width = w,
            Ok(Ok(Message::Row(r))) => break Some(r),
            Ok(Err(e)) => return Err(e),
            Err(_) => break None,
        }
    };
    let first = first.unwrap_or_default();
    let width = width.max(first.len());
    let (columns, pending) = if header {
        let mut names: Vec<String> = Vec::with_capacity(width);
        for i in 0..width {
            let name = first.get(i).cloned().flatten().map(|s| s.trim().to_string()).unwrap_or_default();
            let name = if name.is_empty() || names.contains(&name) { format!("column{}", i + 1) } else { name };
            names.push(name);
        }
        (names, None)
    } else {
        ((1..=width).map(|i| format!("column{i}")).collect(), Some(first).filter(|r| !r.is_empty()))
    };
    Ok(Box::new(XlsxSource { rx, columns, pending }))
}

impl RecordSource for XlsxSource {
    fn columns(&self) -> &[String] {
        &self.columns
    }

    fn next_record(&mut self) -> DbResult<Option<Vec<Field>>> {
        let row = match self.pending.take() {
            Some(r) => r,
            None => loop {
                match self.rx.recv() {
                    Ok(Ok(Message::Row(r))) => break r,
                    Ok(Ok(Message::Width(_))) => {}
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Ok(None),
                }
            },
        };
        let mut row = row;
        row.resize(self.columns.len(), None);
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_days() {
        assert_eq!(serial_text(45_000.0, NumberKind::Date, false), "2023-03-15");
        assert_eq!(serial_text(45_000.5, NumberKind::DateTime, false), "2023-03-15 12:00:00");
        assert_eq!(serial_text(0.25, NumberKind::Time, false), "06:00:00");
        assert_eq!(serial_text(1.0, NumberKind::Date, false), "1900-01-01");
        assert_eq!(serial_text(0.0, NumberKind::Date, true), "1904-01-01");
    }

    #[test]
    fn format_codes() {
        assert_eq!(code_kind("yyyy-mm-dd"), NumberKind::Date);
        assert_eq!(code_kind("dd.mm.yyyy hh:mm"), NumberKind::DateTime);
        assert_eq!(code_kind("[h]:mm:ss"), NumberKind::Time);
        assert_eq!(code_kind("#,##0.00\" days\""), NumberKind::Plain);
        assert_eq!(code_kind("[Red]0.00"), NumberKind::Plain);
        assert_eq!(column_of("AB12"), Some(27));
    }
}
