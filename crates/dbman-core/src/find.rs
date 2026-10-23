//! Find and replace in a script: the matches of a search, and the text with
//! them replaced.

use std::ops::Range;

/// What to look for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub match_case: bool,
    /// Only matches that are not part of a longer name.
    pub whole_word: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte ranges of the matches in `haystack`, in order and not overlapping.
pub fn find_all(haystack: &str, q: &Query) -> Vec<Range<usize>> {
    if q.text.is_empty() {
        return Vec::new();
    }
    let needle = if q.match_case { q.text.clone() } else { q.text.to_lowercase() };
    let mut out = Vec::new();
    let mut at = 0;
    while at < haystack.len() {
        // Compare char by char from `at`, folding case per char so byte offsets stay those of `haystack`.
        let Some(end) = match_at(haystack, at, &needle, q.match_case) else {
            at += haystack[at..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let before = haystack[..at].chars().next_back();
        let after = haystack[end..].chars().next();
        let bounded = !q.whole_word || (!before.is_some_and(is_word) && !after.is_some_and(is_word));
        if bounded && end > at {
            out.push(at..end);
            at = end;
        } else {
            at += haystack[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

/// The end of a match of `needle` (already folded) starting at byte `at`.
fn match_at(haystack: &str, at: usize, needle: &str, match_case: bool) -> Option<usize> {
    let mut rest = haystack[at..].char_indices();
    let mut want = needle.chars().peekable();
    let mut end = at;
    while want.peek().is_some() {
        let (i, c) = rest.next()?;
        let folded: Vec<char> = if match_case { vec![c] } else { c.to_lowercase().collect() };
        for f in folded {
            if want.next() != Some(f) {
                return None;
            }
        }
        end = at + i + c.len_utf8();
    }
    Some(end)
}

/// `haystack` with every match replaced by `with`, and how many there were.
pub fn replace_all(haystack: &str, q: &Query, with: &str) -> (String, usize) {
    let matches = find_all(haystack, q);
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0;
    for m in &matches {
        out.push_str(&haystack[last..m.start]);
        out.push_str(with);
        last = m.end;
    }
    out.push_str(&haystack[last..]);
    (out, matches.len())
}

/// The match to go to from byte `from`: the first one starting at or after it
/// (or, backwards, the last one starting before it), wrapping around.
pub fn next_match(matches: &[Range<usize>], from: usize, backwards: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(if backwards {
        matches.iter().rposition(|m| m.start < from).unwrap_or(matches.len() - 1)
    } else {
        matches.iter().position(|m| m.start >= from).unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, match_case: bool, whole_word: bool) -> Query {
        Query { text: text.into(), match_case, whole_word }
    }

    #[test]
    fn finds_ignoring_case() {
        let s = "select id from Orders; SELECT * FROM orders_old";
        assert_eq!(find_all(s, &q("orders", false, false)), vec![15..21, 37..43]);
        assert_eq!(find_all(s, &q("orders", true, false)), vec![37..43]);
        assert_eq!(find_all(s, &q("", false, false)), Vec::<Range<usize>>::new());
    }

    #[test]
    fn whole_words_only() {
        let s = "id, order_id, id2, (id)";
        assert_eq!(find_all(s, &q("id", false, true)), vec![0..2, 20..22]);
    }

    #[test]
    fn non_ascii_offsets() {
        let s = "Привет, ПРИВЕТ мир";
        let m = find_all(s, &q("привет", false, false));
        assert_eq!(m.len(), 2);
        assert_eq!(&s[m[1].clone()], "ПРИВЕТ");
    }

    #[test]
    fn replaces_every_match() {
        let (out, n) = replace_all("a.id = b.ID", &q("id", false, true), "key");
        assert_eq!((out.as_str(), n), ("a.key = b.key", 2));
        let (out, n) = replace_all("aaa", &q("a", true, false), "aa");
        assert_eq!((out.as_str(), n), ("aaaaaa", 3));
    }

    #[test]
    fn steps_around() {
        let m = vec![0..2, 10..12, 20..22];
        assert_eq!(next_match(&m, 0, false), Some(0));
        assert_eq!(next_match(&m, 3, false), Some(1));
        assert_eq!(next_match(&m, 21, false), Some(0));
        assert_eq!(next_match(&m, 10, true), Some(0));
        assert_eq!(next_match(&m, 0, true), Some(2));
        assert_eq!(next_match(&[], 0, true), None);
    }
}
