//! Writes rows to an Excel workbook as they arrive: numbers, dates and
//! booleans keep their cell types, the header row is bold, frozen and has a
//! filter, and rows past Excel's limit go on to a new sheet.

use std::io::{self, Seek, Write};

use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::{Cell, ColumnMeta};

/// Rows of a sheet, the header included.
const MAX_ROWS: u64 = 1_048_576;
const MAX_COLUMNS: usize = 16_384;
/// Characters a cell holds.
const MAX_TEXT: usize = 32_767;

const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// Style indexes in `styles.xml`.
const HEADER: u8 = 1;
const DATE: u8 = 2;
const DATE_TIME: u8 = 3;

/// How a column's values are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Date,
    DateTime,
    Bool,
    Text,
}

fn kind_of(c: &ColumnMeta) -> Kind {
    let t = c.type_name.to_lowercase();
    let zoned = t.contains("tz") || t.contains("zone") || t.contains("offset");
    if t.contains("bool") || t == "bit" {
        Kind::Bool
    } else if t == "date" || t == "date32" {
        Kind::Date
    } else if (t.contains("timestamp") || t.contains("datetime")) && !zoned {
        Kind::DateTime
    } else if c.numeric {
        Kind::Number
    } else {
        Kind::Text
    }
}

/// `0` is `A`, `26` is `AA`.
fn column_name(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Text for XML, without the characters XML cannot hold.
fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\t' | '\n' => out.push(c),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
}

/// A number Excel keeps exactly: at most 15 significant digits.
fn number(s: &str) -> Option<&str> {
    let t = s.trim();
    let v: f64 = t.parse().ok()?;
    if !v.is_finite() || t.starts_with('+') || t.contains(['i', 'I', 'n', 'N']) {
        return None;
    }
    let mantissa = t.split(['e', 'E']).next().unwrap_or(t).trim_start_matches('-');
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{}", frac.trim_end_matches('0'));
    let digits = digits.trim_start_matches('0').len();
    (digits <= 15).then_some(t)
}

/// An Excel serial day number of `YYYY-MM-DD[ hh:mm:ss[.fff]]`, from 1 March 1900.
fn serial(s: &str, with_time: bool) -> Option<f64> {
    let s = s.trim();
    let days = crate::generate::days_from_date(s)?;
    let rest = &s[10..];
    let mut v = (days + 25_569) as f64;
    if v < 61.0 {
        return None;
    }
    if rest.is_empty() {
        return Some(v);
    }
    if !with_time {
        return None;
    }
    let time = rest.strip_prefix([' ', 'T'])?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut it = hms.split(':');
    let (h, m): (u32, u32) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    let sec: u32 = it.next().map_or(Some(0), |x| x.parse().ok())?;
    if it.next().is_some() || h > 23 || m > 59 || sec > 60 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let frac: f64 = if frac.is_empty() { 0.0 } else { format!("0.{frac}").parse().ok()? };
    v += (f64::from(h * 3600 + m * 60 + sec) + frac) / 86_400.0;
    Some(v)
}

fn boolean(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "t" | "1" | "yes" => Some(true),
        "false" | "f" | "0" | "no" => Some(false),
        _ => None,
    }
}

/// A sheet name Excel accepts: at most 31 characters, none of `[]:*?/\`.
fn sheet_name(base: &str, n: usize) -> String {
    let clean: String = base.chars().map(|c| if "[]:*?/\\".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim_matches('\'');
    let clean = if clean.is_empty() { "Data" } else { clean };
    let suffix = if n > 1 { format!(" ({n})") } else { String::new() };
    let keep = 31 - suffix.chars().count();
    format!("{}{suffix}", clean.chars().take(keep).collect::<String>())
}

/// Streams a workbook to `out`; see [`crate::export::Writer`].
pub struct Writer<W: Write + Seek> {
    zip: ZipWriter<W>,
    name: String,
    columns: Vec<ColumnMeta>,
    kinds: Vec<Kind>,
    letters: Vec<String>,
    sheets: Vec<String>,
    /// Rows on the current sheet, the header included.
    sheet_rows: u64,
    buf: String,
}

fn zip_io(e: zip::result::ZipError) -> io::Error {
    io::Error::other(e.to_string())
}

impl<W: Write + Seek> Writer<W> {
    /// `name` names the sheets.
    pub fn new(out: W, name: &str) -> Self {
        Self {
            zip: ZipWriter::new(out),
            name: name.to_string(),
            columns: Vec::new(),
            kinds: Vec::new(),
            letters: Vec::new(),
            sheets: Vec::new(),
            sheet_rows: 0,
            buf: String::new(),
        }
    }

    pub fn get_ref(&self) -> Option<&W> {
        self.zip.get_ref()
    }

    pub fn begin(&mut self, columns: &[ColumnMeta]) -> io::Result<()> {
        if columns.len() > MAX_COLUMNS {
            return Err(io::Error::other(format!("An Excel sheet holds at most {MAX_COLUMNS} columns.")));
        }
        self.columns = columns.to_vec();
        self.kinds = columns.iter().map(kind_of).collect();
        self.letters = (0..columns.len()).map(column_name).collect();
        self.start_sheet()
    }

    fn options() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
    }

    fn start_sheet(&mut self) -> io::Result<()> {
        if !self.sheets.is_empty() {
            self.end_sheet()?;
        }
        self.sheets.push(sheet_name(&self.name, self.sheets.len() + 1));
        let n = self.sheets.len();
        self.zip.start_file(format!("xl/worksheets/sheet{n}.xml"), Self::options()).map_err(zip_io)?;
        let mut s = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="{MAIN}" xmlns:r="{REL}"><sheetViews><sheetView workbookViewId="0""#
        );
        s.push_str(if n == 1 { r#" tabSelected="1">"# } else { ">" });
        if !self.columns.is_empty() {
            s.push_str(r#"<pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/>"#);
        }
        s.push_str("</sheetView></sheetViews>");
        if !self.columns.is_empty() {
            s.push_str("<cols>");
            for (i, (c, k)) in self.columns.iter().zip(&self.kinds).enumerate() {
                let fit = match k {
                    Kind::Date => 12,
                    Kind::DateTime => 20,
                    Kind::Number | Kind::Bool => 12,
                    Kind::Text => 18,
                };
                let width = (c.name.chars().count() + 3).max(fit).min(60);
                s.push_str(&format!(r#"<col min="{0}" max="{0}" width="{width}" customWidth="1"/>"#, i + 1));
            }
            s.push_str("</cols>");
        }
        s.push_str(r#"<sheetData><row r="1">"#);
        for (c, l) in self.columns.iter().zip(&self.letters) {
            s.push_str(&format!(r#"<c r="{l}1" s="{HEADER}" t="inlineStr"><is><t xml:space="preserve">"#));
            escape(&c.name, &mut s);
            s.push_str("</t></is></c>");
        }
        s.push_str("</row>");
        self.zip.write_all(s.as_bytes())?;
        self.sheet_rows = 1;
        Ok(())
    }

    fn end_sheet(&mut self) -> io::Result<()> {
        let mut s = String::from("</sheetData>");
        if let Some(last) = self.letters.last() {
            s.push_str(&format!(r#"<autoFilter ref="A1:{last}{}"/>"#, self.sheet_rows));
        }
        s.push_str("</worksheet>");
        self.zip.write_all(s.as_bytes())
    }

    pub fn rows(&mut self, rows: &[Vec<Cell>]) -> io::Result<()> {
        for row in rows {
            if self.sheet_rows == MAX_ROWS {
                self.start_sheet()?;
            }
            self.sheet_rows += 1;
            self.row(row);
            if self.buf.len() > 64 * 1024 {
                self.zip.write_all(self.buf.as_bytes())?;
                self.buf.clear();
            }
        }
        self.zip.write_all(self.buf.as_bytes())?;
        self.buf.clear();
        Ok(())
    }

    fn row(&mut self, row: &[Cell]) {
        let r = self.sheet_rows;
        let s = &mut self.buf;
        s.push_str(&format!(r#"<row r="{r}">"#));
        for ((cell, kind), l) in row.iter().zip(&self.kinds).zip(&self.letters) {
            let text = match cell {
                Cell::Null => continue,
                Cell::Text(t) => std::borrow::Cow::Borrowed(t.as_str()),
                Cell::Bytes(_) => cell.as_text().unwrap_or_default(),
            };
            let bytes = matches!(cell, Cell::Bytes(_));
            match kind {
                Kind::Number if !bytes => {
                    if let Some(n) = number(&text) {
                        s.push_str(&format!(r#"<c r="{l}{r}"><v>{n}</v></c>"#));
                        continue;
                    }
                }
                Kind::Date | Kind::DateTime if !bytes => {
                    if let Some(v) = serial(&text, *kind == Kind::DateTime) {
                        let style = if *kind == Kind::Date { DATE } else { DATE_TIME };
                        s.push_str(&format!(r#"<c r="{l}{r}" s="{style}"><v>{v}</v></c>"#));
                        continue;
                    }
                }
                Kind::Bool if !bytes => {
                    if let Some(b) = boolean(&text) {
                        s.push_str(&format!(r#"<c r="{l}{r}" t="b"><v>{}</v></c>"#, u8::from(b)));
                        continue;
                    }
                }
                _ => {}
            }
            s.push_str(&format!(r#"<c r="{l}{r}" t="inlineStr"><is><t xml:space="preserve">"#));
            match text.char_indices().nth(MAX_TEXT) {
                Some((end, _)) => escape(&text[..end], s),
                None => escape(&text, s),
            }
            s.push_str("</t></is></c>");
        }
        s.push_str("</row>");
    }

    /// Writes the workbook's other parts; returns the output.
    pub fn finish(mut self) -> io::Result<W> {
        if self.sheets.is_empty() {
            self.start_sheet()?;
        }
        self.end_sheet()?;
        let mut sheets = String::new();
        let mut rels = String::new();
        let mut types = String::new();
        for (i, name) in self.sheets.iter().enumerate() {
            let n = i + 1;
            let mut esc = String::new();
            escape(name, &mut esc);
            sheets.push_str(&format!(r#"<sheet name="{esc}" sheetId="{n}" r:id="rId{n}"/>"#));
            rels.push_str(&format!(
                r#"<Relationship Id="rId{n}" Type="{REL}/worksheet" Target="worksheets/sheet{n}.xml"/>"#
            ));
            types.push_str(&format!(
                r#"<Override PartName="/xl/worksheets/sheet{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
            ));
        }
        let styles_id = self.sheets.len() + 1;
        let head = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
        let parts = [
            (
                "xl/workbook.xml".to_string(),
                format!(r#"{head}<workbook xmlns="{MAIN}" xmlns:r="{REL}"><sheets>{sheets}</sheets></workbook>"#),
            ),
            (
                "xl/_rels/workbook.xml.rels".to_string(),
                format!(
                    r#"{head}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}<Relationship Id="rId{styles_id}" Type="{REL}/styles" Target="styles.xml"/></Relationships>"#
                ),
            ),
            ("xl/styles.xml".to_string(), format!("{head}{STYLES}")),
            (
                "_rels/.rels".to_string(),
                format!(
                    r#"{head}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
                ),
            ),
            (
                "[Content_Types].xml".to_string(),
                format!(
                    r#"{head}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>{types}</Types>"#
                ),
            ),
        ];
        for (name, body) in parts {
            self.zip.start_file(name, Self::options()).map_err(zip_io)?;
            self.zip.write_all(body.as_bytes())?;
        }
        let mut out = self.zip.finish().map_err(zip_io)?;
        out.flush()?;
        Ok(out)
    }
}

const STYLES: &str = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="2"><numFmt numFmtId="164" formatCode="yyyy\-mm\-dd"/><numFmt numFmtId="165" formatCode="yyyy\-mm\-dd\ hh:mm:ss"/></numFmts><fonts count="2"><font><sz val="11"/><name val="Calibri"/><family val="2"/></font><font><b/><sz val="11"/><name val="Calibri"/><family val="2"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="4"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/><xf numFmtId="164" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/><xf numFmtId="165" fontId="0" fillId="0" borderId="0" xfId="0" applyNumberFormat="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(column_name(0), "A");
        assert_eq!(column_name(25), "Z");
        assert_eq!(column_name(26), "AA");
        assert_eq!(column_name(701), "ZZ");
        assert_eq!(column_name(702), "AAA");
        assert_eq!(number("12.30"), Some("12.30"));
        assert_eq!(number("-0.000123"), Some("-0.000123"));
        assert_eq!(number("1234567890123456"), None);
        assert_eq!(number("123456789012345.000"), Some("123456789012345.000"));
        assert_eq!(number("NaN"), None);
        assert_eq!(number("$5"), None);
        assert_eq!(serial("1900-03-01", false), Some(61.0));
        assert_eq!(serial("2024-02-29", false), Some(45_351.0));
        assert_eq!(serial("2024-02-29 12:00:00", true), Some(45_351.5));
        assert_eq!(serial("2024-02-29 12:00:00", false), None);
        assert_eq!(serial("2024-02-29 12:00:00+03", true), None);
        assert_eq!(serial("1899-01-01", false), None);
        assert_eq!(sheet_name("a/b:c", 1), "a_b_c");
        assert_eq!(sheet_name(&"x".repeat(40), 2).chars().count(), 31);
        let mut s = String::new();
        escape("a<b>&\u{1}\"\r\n", &mut s);
        assert_eq!(s, "a&lt;b&gt;&amp;&quot;&#13;\n");
    }
}
