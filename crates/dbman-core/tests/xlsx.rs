//! Reads a workbook saved by openpyxl: shared strings, dates, booleans,
//! empty rows and cells, formulas without a saved value, and a second sheet.

use std::path::PathBuf;

use dbman_core::import::{self, CsvOptions, ImportFormat};

fn workbook() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/orders.xlsx")
}

fn s(v: &str) -> Option<String> {
    Some(v.into())
}

#[test]
fn reads_a_sheet() {
    let path = workbook();
    assert_eq!(dbman_core::xlsx::sheets(&path).unwrap(), ["Notes", "Orders"]);
    let (format, csv) = import::detect(&path);
    assert_eq!(format, ImportFormat::Excel);
    let csv = CsvOptions { sheet: "Orders".into(), ..csv };
    let mut source = import::open(&path, format, &csv, Default::default()).unwrap();
    assert_eq!(source.columns(), ["id", "customer", "placed", "paid at", "amount", "shipped", "comment", "column8"]);
    let rows = import::preview(source.as_mut(), 100).unwrap();
    assert_eq!(
        rows,
        [
            vec![
                s("1"),
                s("Анна Иванова"),
                s("2024-02-29"),
                s("2024-03-01 09:30:15"),
                s("1234.5"),
                s("true"),
                s("first\nline & <b>"),
                None
            ],
            vec![s("2"), s("Bob"), s("1999-12-31"), None, s("0.1"), s("false"), None, None],
            vec![s("3"), s("Carol"), None, s("2000-01-01 00:00:00"), s("-7"), None, None, None],
            vec![None, None, None, None, None, None, None, s("wide")],
            vec![s("4"), None, None, None, None, None, None, None],
        ]
    );
}

#[test]
fn first_sheet_without_names() {
    let csv = CsvOptions { header: false, ..Default::default() };
    let mut source = import::open(&workbook(), ImportFormat::Excel, &csv, Default::default()).unwrap();
    assert_eq!(source.columns(), ["column1"]);
    assert_eq!(import::preview(source.as_mut(), 10).unwrap(), [vec![s("just a note")]]);
}

#[test]
fn missing_sheet() {
    let csv = CsvOptions { sheet: "Nope".into(), ..Default::default() };
    let err = import::open(&workbook(), ImportFormat::Excel, &csv, Default::default()).err().unwrap();
    assert_eq!(err.message, "The workbook has no sheet Nope.");
}
