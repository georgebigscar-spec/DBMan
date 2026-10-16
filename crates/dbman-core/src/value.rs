//! A cell's value laid out for reading: JSON and XML indented, bytes as a hex
//! dump, and images recognized by their first bytes.

use crate::Cell;

/// What a value looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    Null,
    /// Indented JSON.
    Json(String),
    /// Indented XML.
    Xml(String),
    Text(String),
    /// An image in this format (`"PNG"`, `"JPEG"`…), with its hex dump.
    Image(&'static str, String),
    /// A hex dump.
    Bytes(String),
}

/// Bytes shown in a hex dump; the rest is left out.
pub const HEX_LIMIT: usize = 64 * 1024;

pub fn show(cell: &Cell) -> Shown {
    match cell {
        Cell::Null => Shown::Null,
        Cell::Text(s) => pretty_json(s)
            .map(Shown::Json)
            .or_else(|| pretty_xml(s).map(Shown::Xml))
            .unwrap_or_else(|| Shown::Text(s.clone())),
        Cell::Bytes(b) => match image_format(b) {
            Some(f) => Shown::Image(f, hex_dump(b, HEX_LIMIT)),
            None => Shown::Bytes(hex_dump(b, HEX_LIMIT)),
        },
    }
}

/// `s` indented, if it is a JSON object or array.
pub fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim();
    if !(t.starts_with('{') && t.ends_with('}') || t.starts_with('[') && t.ends_with(']')) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    serde_json::to_string_pretty(&v).ok()
}

/// `s` with one element per line, indented by depth, if it looks like XML
/// whose tags all close. Text, comments and CDATA are kept as they are.
pub fn pretty_xml(s: &str) -> Option<String> {
    let t = s.trim();
    if !t.starts_with('<') || !t.ends_with('>') {
        return None;
    }
    let mut out = String::new();
    let mut depth = 0usize;
    let mut rest = t;
    let line = |out: &mut String, depth: usize, text: &str| {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&"  ".repeat(depth));
        out.push_str(text);
    };
    while !rest.is_empty() {
        if let Some(body) = rest.strip_prefix('<') {
            let end = if body.starts_with("!--") {
                rest.find("-->")? + 3
            } else if body.starts_with("![CDATA[") {
                rest.find("]]>")? + 3
            } else {
                rest.find('>')? + 1
            };
            let tag = &rest[..end];
            if tag.starts_with("</") {
                depth = depth.checked_sub(1)?;
                // `<a>text</a>` stays on one line.
                if !out.is_empty() && !out.ends_with('>') {
                    out.push_str(tag);
                } else {
                    line(&mut out, depth, tag);
                }
            } else {
                line(&mut out, depth, tag);
                let opens = !(tag.ends_with("/>") || tag.starts_with("<?") || tag.starts_with("<!"));
                if opens {
                    depth += 1;
                }
            }
            rest = &rest[end..];
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = rest[..end].trim();
            if !text.is_empty() {
                // Text right after an opening tag joins its line.
                if out.ends_with('>') && !out.ends_with("/>") {
                    out.push_str(text);
                } else {
                    line(&mut out, depth, text);
                }
            }
            rest = &rest[end..];
        }
    }
    (depth == 0).then_some(out)
}

/// The image format of `b`, from its first bytes.
pub fn image_format(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("PNG")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("JPEG")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("GIF")
    } else if b.starts_with(b"BM") && b.len() > 14 {
        Some("BMP")
    } else if b.len() > 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("WebP")
    } else {
        None
    }
}

/// Offset, 16 bytes in hex and the same bytes as ASCII per line, for the
/// first `max` bytes.
pub fn hex_dump(b: &[u8], max: usize) -> String {
    let mut out = String::new();
    for (i, chunk) in b[..b.len().min(max)].chunks(16).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{:08x}  ", i * 16));
        for j in 0..16 {
            match chunk.get(j) {
                Some(x) => out.push_str(&format!("{x:02x} ")),
                None => out.push_str("   "),
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        out.extend(chunk.iter().map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '.' }));
        out.push('|');
    }
    if b.len() > max {
        out.push_str(&format!("\n… {} more bytes", b.len() - max));
    }
    out
}

/// "12 B", "3.4 KB", "1.2 MB".
pub fn size_label(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_indented() {
        assert_eq!(
            pretty_json(r#" {"a":1,"b":[true,null]} "#).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": [\n    true,\n    null\n  ]\n}"
        );
        assert_eq!(pretty_json("[1]").unwrap(), "[\n  1\n]");
        assert_eq!(pretty_json("{not json}"), None);
        assert_eq!(pretty_json("42"), None);
        assert_eq!(show(&Cell::Text("{\"k\":\"v\"}".into())), Shown::Json("{\n  \"k\": \"v\"\n}".into()));
    }

    #[test]
    fn xml_is_indented() {
        assert_eq!(
            pretty_xml("<?xml version=\"1.0\"?><a x=\"1\"><b>text</b><c/><!-- note --><d><e>1</e></d></a>").unwrap(),
            "<?xml version=\"1.0\"?>\n<a x=\"1\">\n  <b>text</b>\n  <c/>\n  <!-- note -->\n  <d>\n    <e>1</e>\n  </d>\n</a>"
        );
        assert_eq!(pretty_xml("<a><b></a>"), None);
        assert_eq!(pretty_xml("<a>"), None);
        assert_eq!(pretty_xml("a < b and c > d"), None);
    }

    #[test]
    fn bytes() {
        assert_eq!(
            hex_dump(b"Hello, world!\x00\x01\x02\xff", 1024),
            "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 01 02  |Hello, world!...|\n00000010  ff                                                |.|"
        );
        assert!(hex_dump(&[0; 40], 16).ends_with("\n… 24 more bytes"));
        assert_eq!(image_format(b"\x89PNG\r\n\x1a\n...."), Some("PNG"));
        assert_eq!(image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("JPEG"));
        assert_eq!(image_format(b"plain"), None);
        assert!(matches!(show(&Cell::Bytes(b"GIF89a....".to_vec())), Shown::Image("GIF", _)));
        assert_eq!(show(&Cell::Null), Shown::Null);
        assert_eq!(show(&Cell::Text("plain".into())), Shown::Text("plain".into()));
        assert_eq!(size_label(2048), "2.0 KB");
    }
}
