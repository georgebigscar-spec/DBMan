//! Cells copied from a spreadsheet: Excel, LibreOffice and Google Sheets put
//! a block of cells on the clipboard as tab-separated lines, quoting a cell
//! that holds a tab, a line break or a quote.

/// The rows and cells of copied text. A trailing line break ends the last row
/// rather than starting an empty one.
pub fn parse(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut chars = text.chars().peekable();
    // A quote opens a quoted cell only as the cell's first character.
    let mut at_start = true;
    while let Some(c) = chars.next() {
        match c {
            '"' if at_start => {
                at_start = false;
                while let Some(q) = chars.next() {
                    if q == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            cell.push('"');
                        } else {
                            break;
                        }
                    } else {
                        cell.push(q);
                    }
                }
            }
            '\t' => {
                row.push(std::mem::take(&mut cell));
                at_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
                at_start = true;
            }
            c => {
                at_start = false;
                cell.push(c);
            }
        }
    }
    if !at_start || !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excel_block() {
        assert_eq!(parse("1\tAlice\tBerlin\r\n2\tBob\t\r\n"), [vec!["1", "Alice", "Berlin"], vec!["2", "Bob", ""]]);
        assert_eq!(parse("one"), [vec!["one"]]);
        assert_eq!(parse(""), Vec::<Vec<String>>::new());
        assert_eq!(parse("\n"), [vec![""]]);
    }

    #[test]
    fn quoted_cells() {
        let text = "\"line 1\nline 2\"\t\"say \"\"hi\"\"\"\n5\"\t\"a\tb\"\n";
        assert_eq!(parse(text), [vec!["line 1\nline 2", "say \"hi\""], vec!["5\"", "a\tb"]]);
    }
}
