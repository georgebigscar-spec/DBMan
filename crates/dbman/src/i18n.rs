//! Interface language. Text in the code is English, which is the default;
//! `locales/<lang>.json` maps each English text to its translation, the way
//! gettext catalogs do. Text a catalog lacks stays English.
//!
//! `tr("Refresh")` translates a fixed text; `trf("{n} rows", &[("n", &n)])`
//! translates a template and then fills in its `{name}` placeholders, so a
//! translation may move them around.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Ru,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::Ru];

    /// The code stored in the settings.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        }
    }

    pub fn from_code(code: &str) -> Lang {
        Lang::ALL.into_iter().find(|l| l.code() == code).unwrap_or_default()
    }

    /// The language's name in itself, for the switcher.
    pub fn name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
        }
    }

    fn catalog(self) -> Option<&'static HashMap<String, String>> {
        match self {
            Lang::En => None,
            Lang::Ru => Some(&RU),
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

static RU: LazyLock<HashMap<String, String>> = LazyLock::new(|| parse(include_str!("../locales/ru.json")));

/// Reads a catalog; keys starting with `//` are comments.
pub fn parse(json: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> =
        serde_json::from_str(json).expect("a locale catalog is a JSON object of strings");
    map.retain(|k, _| !k.starts_with("//"));
    map
}

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Ru,
        _ => Lang::En,
    }
}

/// The text in the current language.
pub fn tr(text: &str) -> &str {
    current().catalog().and_then(|c| c.get(text)).map_or(text, String::as_str)
}

/// The template in the current language with `{name}` replaced by its value.
pub fn trf(template: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut s = tr(template).to_string();
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), &value.to_string());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_english_and_fills_placeholders() {
        set(Lang::En);
        assert_eq!(tr("Refresh"), "Refresh");
        assert_eq!(trf("{n} rows", &[("n", &3)]), "3 rows");
        set(Lang::Ru);
        assert_eq!(tr("Refresh"), "Обновить");
        assert_eq!(tr("No such text anywhere"), "No such text anywhere");
        set(Lang::En);
    }
}
