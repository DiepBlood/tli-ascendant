//! The language Ascendant reads the game in and writes its window in: English,
//! or Traditional Chinese for the zh-TW game client. English is the default,
//! and the tests run in it. Sentences are looked up by their English text:
//! [`tr`] gives a fixed one in the set language and [`trf!`] fills one's
//! `{}` (or `{0}`, `{1}`... where the translation reorders them), so English
//! output stays exactly what `format!` gave.

mod zh_tw;

/// Vorax advice notes in Traditional Chinese (src/vorax/mod.rs translates `Advice::note` with it).
pub use zh_tw::vorax_note;

use std::collections::HashMap;
use std::fmt::{Display, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    ZhTw,
}

impl Lang {
    pub const ALL: [Lang; 2] = [Lang::En, Lang::ZhTw];

    /// The settings value: "en" or "zh-TW".
    pub fn key(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhTw => "zh-TW",
        }
    }

    /// The language of a settings value; English for any other.
    pub fn from_key(key: &str) -> Lang {
        if key.eq_ignore_ascii_case("zh-TW") { Lang::ZhTw } else { Lang::En }
    }

    /// Its name written in itself, for the language picker.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::ZhTw => "繁體中文",
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

/// The set language.
pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == Lang::ZhTw as u8 { Lang::ZhTw } else { Lang::En }
}

/// Sets the language the reader and the window use from now on.
pub fn set(lang: Lang) {
    LANG.store(lang as u8, Ordering::Relaxed);
}

/// The set language is Traditional Chinese.
pub fn zh() -> bool {
    lang() == Lang::ZhTw
}

/// `en` in the set language: its translation, or `en` itself when there is none.
pub fn tr(en: &'static str) -> &'static str {
    tr_in(lang(), en)
}

/// `en` in `lang`; see [`tr`].
pub fn tr_in(lang: Lang, en: &'static str) -> &'static str {
    match lang {
        Lang::En => en,
        Lang::ZhTw => {
            static TABLE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
            TABLE.get_or_init(|| zh_tw::TEXT.iter().copied().collect()).get(en).copied().unwrap_or(en)
        }
    }
}

/// The Chinese of `en`, a text not known at compile time (such as a word
/// [`crate::engine::text::plural`] is given), when the set language is
/// Chinese and the table has it.
pub fn tr_dyn(en: &str) -> Option<&'static str> {
    if !zh() {
        return None;
    }
    zh_tw::TEXT.iter().find(|(key, _)| *key == en).map(|(_, zh)| *zh)
}

/// `template` with each `{}` replaced by the next of `args` and each `{N}`
/// by the Nth; a placeholder with no argument is left out.
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let (mut next, mut rest) = (0, template);
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').filter(|&close| after[..close].chars().all(|c| c.is_ascii_digit())) {
            Some(close) => {
                let index = if close == 0 {
                    next += 1;
                    next - 1
                } else {
                    after[..close].parse().unwrap_or(usize::MAX)
                };
                if let Some(arg) = args.get(index) {
                    let _ = write!(out, "{arg}");
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `trf!("Day {} · {} to Riot", day, n)`: the sentence in the set language
/// with its placeholders filled; see [`fill`].
macro_rules! trf {
    ($en:literal $(, $arg:expr)* $(,)?) => {
        $crate::lang::fill($crate::lang::tr($en), &[$(&$arg as &dyn ::std::fmt::Display),*])
    };
}
pub(crate) use trf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_in_order_and_by_index() {
        assert_eq!(fill("Day {} · {} to Riot", &[&7, &13]), format!("Day {} · {} to Riot", 7, 13));
        assert_eq!(fill("{1} then {0}", &[&"a", &"b"]), "b then a");
        assert_eq!(fill("no args {}", &[]), "no args ");
        assert_eq!(fill("brace { stays", &[]), "brace { stays");
    }

    #[test]
    fn english_is_the_key() {
        assert_eq!(tr_in(Lang::En, "Board"), "Board");
        assert_eq!(tr_in(Lang::ZhTw, "Board"), "盤面");
        assert_eq!(tr_in(Lang::ZhTw, "not in the table"), "not in the table");
    }

    #[test]
    fn table_keys_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (en, _) in zh_tw::TEXT {
            assert!(seen.insert(en), "{en:?} is in the table twice");
        }
    }
}
