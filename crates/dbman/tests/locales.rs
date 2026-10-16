//! Every text the code passes to `tr` or `trf` has a Russian translation, with
//! the same placeholders.

use std::collections::BTreeSet;

/// String literals passed as the first argument of `tr(` or `trf(`.
fn calls(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !src.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &src[i..];
        let len = if rest.starts_with("trf(") {
            4
        } else if rest.starts_with("tr(") {
            3
        } else {
            0
        };
        let ident_before = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'.');
        if len == 0 || ident_before {
            i += 1;
            continue;
        }
        let mut j = i + len;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != b'"' {
            i += len;
            continue;
        }
        j += 1;
        let mut s = String::new();
        let mut chars = src[j..].chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => match chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('\n') => {
                        // A line continuation skips the next line's indentation.
                        let rest: String = chars.clone().collect();
                        let trimmed = rest.trim_start();
                        let skip = rest.chars().count() - trimmed.chars().count();
                        for _ in 0..skip {
                            chars.next();
                        }
                    }
                    Some(o) => s.push(o),
                    None => break,
                },
                c => s.push(c),
            }
        }
        out.push(s);
        i = j;
        // `j` sits after the opening quote; move on past the literal.
        while !src.is_char_boundary(i) {
            i += 1;
        }
    }
    out
}

fn placeholders(s: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let mut rest = s;
    while let Some(a) = rest.find('{') {
        let Some(b) = rest[a..].find('}') else { break };
        let name = &rest[a + 1..a + b];
        if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            set.insert(name.to_string());
        }
        rest = &rest[a + b + 1..];
    }
    set
}

#[test]
fn russian_catalog_is_complete() {
    let ru = dbman::i18n::parse(include_str!("../locales/ru.json"));
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut missing = BTreeSet::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("i18n.rs") {
            for s in calls(&std::fs::read_to_string(&path).unwrap()) {
                if !ru.contains_key(&s) {
                    missing.insert(format!("{}: {s:?}", path.file_name().unwrap().to_string_lossy()));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "texts without a Russian translation:\n{}",
        missing.into_iter().collect::<Vec<_>>().join("\n")
    );
    let wrong: Vec<&String> = ru.iter().filter(|(k, v)| placeholders(k) != placeholders(v)).map(|(k, _)| k).collect();
    assert!(wrong.is_empty(), "translations that change the placeholders: {wrong:?}");
}
