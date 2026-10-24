//! A JSON value as a tree: the path of each node, a search over keys and
//! values, and edits of single values that keep their type.

use std::collections::HashSet;

use serde_json::Value;

/// One step of a path: a key of an object or an index of an array.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Step {
    Key(String),
    Index(usize),
}

pub type Path = Vec<Step>;

/// `$.user.tags[0]`, with keys that are not plain names quoted: `$["a b"]`.
pub fn path_label(path: &[Step]) -> String {
    let mut s = String::from("$");
    for step in path {
        match step {
            Step::Index(i) => s.push_str(&format!("[{i}]")),
            Step::Key(k) if !k.is_empty() && k.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                s.push('.');
                s.push_str(k);
            }
            Step::Key(k) => s.push_str(&format!("[{}]", Value::String(k.clone()))),
        }
    }
    s
}

/// A scalar as shown in the tree: strings quoted, the rest as JSON.
pub fn scalar_label(v: &Value) -> String {
    match v {
        Value::Array(a) => format!("[{}]", a.len()),
        Value::Object(o) => format!("{{{}}}", o.len()),
        other => other.to_string(),
    }
}

/// A scalar as typed into an edit box: strings without their quotes.
pub fn edit_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The paths whose key or scalar value contains `needle` (ignoring case),
/// with every node above them, so the tree can open down to them.
pub fn search(root: &Value, needle: &str) -> HashSet<Path> {
    let needle = needle.trim().to_lowercase();
    let mut out = HashSet::new();
    if needle.is_empty() {
        return out;
    }
    fn walk(v: &Value, path: &mut Path, needle: &str, out: &mut HashSet<Path>) -> bool {
        let key_hit = match path.last() {
            Some(Step::Key(k)) => k.to_lowercase().contains(needle),
            _ => false,
        };
        let mut hit = key_hit;
        match v {
            Value::Object(o) => {
                for (k, child) in o {
                    path.push(Step::Key(k.clone()));
                    hit |= walk(child, path, needle, out);
                    path.pop();
                }
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    path.push(Step::Index(i));
                    hit |= walk(child, path, needle, out);
                    path.pop();
                }
            }
            other => hit |= edit_text(other).to_lowercase().contains(needle),
        }
        if hit {
            out.insert(path.clone());
        }
        hit
    }
    walk(root, &mut Vec::new(), &needle, &mut out);
    out
}

/// The node at `path`.
pub fn get<'a>(root: &'a Value, path: &[Step]) -> Option<&'a Value> {
    path.iter().try_fold(root, |v, step| match step {
        Step::Key(k) => v.get(k),
        Step::Index(i) => v.get(i),
    })
}

fn get_mut<'a>(root: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    path.iter().try_fold(root, |v, step| match step {
        Step::Key(k) => v.get_mut(k),
        Step::Index(i) => v.get_mut(i),
    })
}

/// Sets the scalar at `path` from typed text, keeping its type: a string
/// takes the text as it is, a number must parse, a boolean is `true` or
/// `false`, and `null` (or any JSON typed into it) becomes that JSON, or a
/// string when it does not parse.
pub fn set(root: &mut Value, path: &[Step], text: &str) -> Result<(), String> {
    let node = get_mut(root, path).ok_or_else(|| "The value is no longer there.".to_string())?;
    let new = match node {
        Value::String(_) => Value::String(text.to_string()),
        Value::Number(_) => {
            let n: serde_json::Number =
                serde_json::from_str(text.trim()).map_err(|_| format!("\"{text}\" is not a number."))?;
            Value::Number(n)
        }
        Value::Bool(_) => match text.trim() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err("A boolean is true or false.".into()),
        },
        Value::Null => serde_json::from_str(text.trim()).unwrap_or_else(|_| Value::String(text.to_string())),
        Value::Array(_) | Value::Object(_) => return Err("Only single values are edited.".into()),
    };
    *node = new;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({"user": {"name": "Alice", "age": 36, "tags": ["admin", "Berlin"], "active": true, "note": null},
               "a b": 1})
    }

    #[test]
    fn paths_and_labels() {
        let p = vec![Step::Key("user".into()), Step::Key("tags".into()), Step::Index(1)];
        assert_eq!(path_label(&p), "$.user.tags[1]");
        assert_eq!(path_label(&[Step::Key("a b".into())]), "$[\"a b\"]");
        assert_eq!(get(&doc(), &p), Some(&json!("Berlin")));
        assert_eq!(scalar_label(&json!("x")), "\"x\"");
        assert_eq!(scalar_label(&json!([1, 2])), "[2]");
    }

    #[test]
    fn finds_keys_and_values() {
        let hits = search(&doc(), "berlin");
        let tag = vec![Step::Key("user".into()), Step::Key("tags".into()), Step::Index(1)];
        assert!(hits.contains(&tag));
        assert!(hits.contains(&vec![Step::Key("user".into())]), "the parents open");
        assert!(!hits.contains(&vec![Step::Key("user".into()), Step::Key("name".into())]));
        assert!(search(&doc(), "AGE").contains(&vec![Step::Key("user".into()), Step::Key("age".into())]));
        assert!(search(&doc(), " ").is_empty());
    }

    #[test]
    fn edits_keep_types() {
        let mut d = doc();
        let at = |k: &str| vec![Step::Key("user".into()), Step::Key(k.into())];
        set(&mut d, &at("name"), "42").unwrap();
        assert_eq!(d["user"]["name"], json!("42"));
        set(&mut d, &at("age"), "37.5").unwrap();
        assert_eq!(d["user"]["age"], json!(37.5));
        assert!(set(&mut d, &at("age"), "old").is_err());
        assert!(set(&mut d, &at("active"), "yes").is_err());
        set(&mut d, &at("active"), "false").unwrap();
        set(&mut d, &at("note"), "[1]").unwrap();
        assert_eq!(d["user"]["note"], json!([1]));
        assert!(set(&mut d, &at("tags"), "x").is_err());
        assert!(set(&mut d, &at("missing"), "x").is_err());
    }
}
