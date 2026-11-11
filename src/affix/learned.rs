//! The affixes learned from the game log: each map entered writes its affixes' ids and texts (the AffixInfos block,
//! `app::gamelog::take_affix_texts`), in the client's language with the values filled in. Those not in the default
//! table, or in it without this language's text, are kept in a file of the user's (`affix-learned.tsv` in the app's
//! folder) and join the table ([`super::Dict::with_learned`]): a new affix shows on the affix page as not framed until
//! the user grades it, and an id seen once in each language's client gets both texts. Game text only, no player data.

use std::path::{Path, PathBuf};

/// One affix learned: its id and its text in each language seen (templates: `#` for a rolled number; empty when not
/// seen in that language yet).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Learned {
    pub id: String,
    pub en: String,
    pub zh: String,
}

/// The file's first line. `# v2` marks the files learned with each text paired to its id whatever the order of the
/// log's fields (2026-11-09); an older file may hold texts paired to the wrong id and is set aside ([`Store::open`]).
const HEADER: &str = "# v2 Map affixes learned from the game log (id<TAB>English<TAB>Chinese; # = a rolled number). Game text only.";
/// The version [`HEADER`] names. Files from 2 up pair each text to its own id; a later version reads as it is.
const VERSION: u32 = 2;

/// The version a file's first line names (`# v2 ...` or `# v2` alone: `# v`, digits, then a blank or the line's end);
/// None for a line that names none.
fn version(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("# v")?;
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let after = &rest[digits..];
    if digits == 0 || !(after.is_empty() || after.starts_with(char::is_whitespace)) {
        return None;
    }
    rest[..digits].parse().ok()
}

/// A log text as a template, the same steps as the lab made the default table with (its bilingual.py script), so
/// the texts compare equal: a value tag with a number
/// (`<p>+15</p>`, `<p> 0.5 </p>`) becomes `#`; every other tag `<...>` goes, its words kept (`<e id=507>Drop
/// Quantity</e>`; a `<` with no `>` after it stays); then every number with its sign becomes `#`. None for an
/// untranslated key (`affix_class|description|5`) or an empty text.
pub fn template(text: &str) -> Option<String> {
    if text.contains("affix_class|") || text.trim().is_empty() {
        return None;
    }
    let plain = strip_tags(&e_tags_to_words(&values_to_marks(text)));
    // numbers (with a sign right before them) to #
    let chars: Vec<char> = plain.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let signed = matches!(c, '+' | '-') && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        if signed || c.is_ascii_digit() {
            let mut j = if signed { i + 1 } else { i };
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < chars.len() && chars[j] == '.' && chars[j + 1].is_ascii_digit() {
                j += 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
            }
            out.push('#');
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    let t = out.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// `<p>` value tags holding a number (blanks around it allowed, an optional sign, digits and dots) as `#`
/// (bilingual.py: `<p>\s*([+\-]?[\d.]+)\s*</p>`); other `<p>` tags are left to [`strip_tags`].
fn values_to_marks(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("<p>") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 3..];
        let value = after.trim_start();
        let value = value.strip_prefix(['+', '-']).unwrap_or(value);
        let digits = value.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(value.len());
        match value[digits..].trim_start().strip_prefix("</p>") {
            Some(tail) if digits > 0 => {
                out.push('#');
                rest = tail;
            }
            _ => {
                out.push_str("<p>");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Keyword tags `<e id=N>words</e>` as their words (bilingual.py: `<e id=\d+>(.*?)</e>`, the nearest `</e>`), before
/// [`strip_tags`]: a stray `<` before one is then not paired with the tag's `>`.
fn e_tags_to_words(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("<e id=") {
        let after = &rest[at + 6..];
        let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        let words = (digits > 0 && after[digits..].starts_with('>')).then(|| &after[digits + 1..]).and_then(|inner| inner.find("</e>").map(|end| (inner, end)));
        match words {
            Some((inner, end)) => {
                out.push_str(&rest[..at]);
                out.push_str(&inner[..end]);
                rest = &inner[end + 4..];
            }
            None => {
                out.push_str(&rest[..=at]);
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every tag `<...>` with at least one character inside out, the words between tags kept (bilingual.py: `<[^>]+>`);
/// a `<` with no `>` after it, or `<>`, stays.
fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        match rest[at + 1..].find('>') {
            Some(len) if len > 0 => rest = &rest[at + 1 + len + 1..],
            _ => {
                out.push('<');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether a text is Chinese (any CJK character).
fn is_chinese(t: &str) -> bool {
    t.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

/// Adds what the log said of affix `id` (`text` as logged): a new affix, or this language's text of one known.
/// True when the list changed (the caller saves it).
pub fn add(list: &mut Vec<Learned>, id: u32, text: &str) -> bool {
    let Some(t) = template(text) else { return false };
    let id = id.to_string();
    let zh = is_chinese(&t);
    let entry = match list.iter_mut().find(|l| l.id == id) {
        Some(e) => e,
        None => {
            list.push(Learned { id, ..Learned::default() });
            list.last_mut().expect("just pushed")
        }
    };
    let slot = if zh { &mut entry.zh } else { &mut entry.en };
    // the first text of a language is kept: the same affix logs the same words each time, only the values differ
    if slot.is_empty() {
        *slot = t;
        true
    } else {
        false
    }
}

/// The learned list with the file it is kept in. The path is None when there is no app folder, or when the file is
/// there but did not read: the list then starts empty and is not saved this run, so a shorter list never replaces the
/// file the user has.
#[derive(Debug, Default)]
pub struct Store {
    pub list: Vec<Learned>,
    path: Option<PathBuf>,
}

impl Store {
    /// The list in its file. A file of an older version (its first line names no version or one below [`VERSION`]:
    /// texts may be paired to the wrong id, see `app::gamelog::AffixBlock`) is set aside, renamed `affix-learned.v1.tsv` (`.v1-2.tsv`, `.v1-3.tsv`...
    /// when that name is taken, never over another file), and the list starts empty: the look back at start learns it
    /// again from the game's logs. A file that cannot be set aside is neither read nor saved over this run.
    pub fn open(path: Option<PathBuf>) -> Self {
        let Some(path) = path else { return Self::default() };
        match first_line(&path) {
            Ok(Some(line)) if !matches!(version(&line), Some(v) if v >= VERSION) => match set_aside(&path) {
                Ok(_) => Self { list: Vec::new(), path: Some(path) },
                Err(_) => Self::default(),
            },
            Ok(_) => match load(&path) {
                Some(list) => Self { list, path: Some(path) },
                None => Self::default(),
            },
            Err(_) => Self::default(),
        }
    }

    /// Writes the list to its file (nothing without one, see [`Store`]).
    pub fn save(&self) -> std::io::Result<()> {
        self.path.as_deref().map_or(Ok(()), |p| save(p, &self.list))
    }
}

/// The first non-empty line of the file at `path` (a byte order mark skipped); None when there is no file or no such
/// line; an error when the file is there but does not read.
fn first_line(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).trim_start_matches('\u{feff}').lines().find(|l| !l.trim().is_empty()).map(str::to_string)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Renames the file at `path` (`<name>.tsv`) to `<name>.v1.tsv` beside it, or `<name>.v1-2.tsv`, `<name>.v1-3.tsv`...
/// when the name is taken; the name it got.
fn set_aside(path: &Path) -> std::io::Result<PathBuf> {
    let stem = path.file_stem().map_or_else(|| "affix-learned".into(), |s| s.to_string_lossy().into_owned());
    let to = (1..1000)
        .map(|n| path.with_file_name(if n == 1 { format!("{stem}.v1.tsv") } else { format!("{stem}.v1-{n}.tsv") }))
        .find(|p| !p.exists())
        .ok_or_else(|| std::io::Error::other("no free name to set the learned file aside"))?;
    std::fs::rename(path, &to)?;
    Ok(to)
}

/// The list in a file: empty when there is no file yet; None when the file is there but does not read (see
/// [`Store`]). Bytes that are not UTF-8 read as U+FFFD, the rest of the line kept; a byte order mark is skipped.
pub fn load(path: &Path) -> Option<Vec<Learned>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let text = String::from_utf8_lossy(&bytes);
    let list = text
        .trim_start_matches('\u{feff}')
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            // a line whose empty Chinese column lost its tab (an editor trimming trailing blanks) still reads
            let p: Vec<&str> = l.split('\t').collect();
            let field = |i: usize| p.get(i).map_or(String::new(), |s| s.to_string());
            ((2..=3).contains(&p.len()) && !p[0].is_empty()).then(|| Learned { id: field(0), en: field(1), zh: field(2) })
        })
        .collect();
    Some(list)
}

/// Writes the list (a temporary file first, then renamed over the old one; the folder made when missing).
pub fn save(path: &Path, list: &[Learned]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = String::from(HEADER);
    text.push('\n');
    for l in list {
        let clean = |t: &str| t.replace(['\t', '\n', '\r'], " ");
        text.push_str(&format!("{}\t{}\t{}\n", clean(&l.id), clean(&l.en), clean(&l.zh)));
    }
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Log texts to templates, as the lab's bilingual.py: tags out, numbers with their sign to #.
    #[test]
    fn log_texts_become_templates() {
        assert_eq!(template("<p>+15</p>% additional Compass <e id=507>Drop Quantity</e>").as_deref(), Some("#% additional Compass Drop Quantity"));
        assert_eq!(template("Monsters in the stage <p>+10</p>% additional Max Life").as_deref(), Some("Monsters in the stage #% additional Max Life"));
        assert_eq!(template("關卡中有<p>7</p>%機率出現<p>1</p>隻<e id=10000018>全頭哥·布林</e>").as_deref(), Some("關卡中有#%機率出現#隻全頭哥·布林"));
        assert_eq!(template("Interval: <p>0.5</p>s, Speed-up Test Subject II").as_deref(), Some("Interval: #s, Speed-up Test Subject II"));
        assert_eq!(template("affix_class|description|5"), None);
        assert_eq!(template("  "), None);
        // a value tag is one mark whatever its number (as bilingual.py), a value tag without a number keeps its words
        assert_eq!(template("Lasts <p> 1.5 </p>s, <p>+.5</p> more").as_deref(), Some("Lasts #s, # more"));
        assert_eq!(template("<p>many</p> monsters").as_deref(), Some("many monsters"));
        // a < with no > after it, and <>, stay
        assert_eq!(template("Life <p>5</p>% <> of max, < half").as_deref(), Some("Life #% <> of max, < half"));
        assert_eq!(template("<e id=1>Cold</e> <b").as_deref(), Some("Cold <b"));
        // a stray < before a keyword tag stays (the tag's words taken out first, as bilingual.py); a keyword tag with no
        // closing </e> is an ordinary tag
        assert_eq!(template("a < b <e id=1>c</e>").as_deref(), Some("a < b c"));
        assert_eq!(template("<e id=2>Fire <e id=3>Cold</e> and</e> x").as_deref(), Some("Fire Cold and x"));
        assert_eq!(template("<e id=x>y</e> <e id=4>z").as_deref(), Some("y z"));
    }

    /// A new id is added with its language's text; the other language fills in later; a repeat changes nothing.
    #[test]
    fn learning_fills_each_language_once() {
        let mut list = Vec::new();
        assert!(add(&mut list, 5112007, "<p>-20</p>% additional Area Damage for you"));
        assert!(!add(&mut list, 5112007, "<p>-25</p>% additional Area Damage for you"));
        assert!(add(&mut list, 5112007, "玩家額外 <p>-20</p>% 範圍傷害"));
        assert_eq!(list, vec![Learned { id: "5112007".into(), en: "#% additional Area Damage for you".into(), zh: "玩家額外 #% 範圍傷害".into() }]);
        assert!(!add(&mut list, 5112008, "affix_class|description|9"));
        assert_eq!(list.len(), 1);
    }

    /// The file round trip, tabs in a text made spaces.
    #[test]
    fn the_file_reads_back() {
        let dir = std::env::temp_dir().join(format!("affix-learned-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("affix-learned.tsv");
        let list = vec![Learned { id: "1".into(), en: "a\tb".into(), zh: "".into() }, Learned { id: "2".into(), en: "".into(), zh: "中文".into() }];
        save(&path, &list).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back, vec![Learned { id: "1".into(), en: "a b".into(), zh: "".into() }, list[1].clone()]);
        // a line without its trailing tab (empty Chinese) reads too
        std::fs::write(&path, "# header\n3\t#% English only\n").unwrap();
        assert_eq!(load(&path).unwrap(), vec![Learned { id: "3".into(), en: "#% English only".into(), zh: "".into() }]);
        // a byte order mark and a byte that is not UTF-8 lose nothing else
        std::fs::write(&path, b"\xef\xbb\xbf# header\n4\tbad \xff byte\t\n5\tfine\t\n").unwrap();
        assert_eq!(load(&path).unwrap().iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["4", "5"]);
        // no file yet: an empty list, saved later into a folder made then
        let deeper = dir.join("new").join("affix-learned.tsv");
        assert_eq!(load(&deeper), Some(Vec::new()));
        let mut store = Store::open(Some(deeper.clone()));
        store.list.push(list[1].clone());
        store.save().unwrap();
        assert_eq!(load(&deeper).unwrap(), vec![list[1].clone()]);
        // a path that is there but does not read (a folder): no list, and nothing saved over it
        let folder = dir.join("folder.tsv");
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(load(&folder), None);
        let mut store = Store::open(Some(folder.clone()));
        store.list.push(list[0].clone());
        store.save().unwrap();
        assert!(folder.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file of the older version (no `# v2` first line: its texts may be paired to the wrong id) is set aside, never
    /// over another file set aside before (`.v1.tsv`, then `.v1-2.tsv`), and the list starts empty to be learned again;
    /// a file of this version reads as it is.
    #[test]
    fn an_older_file_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("affix-learned-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("affix-learned.tsv");
        let old = "# Map affixes learned from the game log (id<TAB>English<TAB>Chinese; # = a rolled number). Game text only.\n5212008\t\t玩家受到傷害時， # 秒內額外 #% 護甲值\n";
        std::fs::write(&path, old).unwrap();
        let store = Store::open(Some(path.clone()));
        assert!(store.list.is_empty() && !path.exists());
        assert_eq!(std::fs::read_to_string(dir.join("affix-learned.v1.tsv")).unwrap(), old);
        // saved again, with this version's first line
        let mut store = store;
        store.list.push(Learned { id: "5311013".into(), en: String::new(), zh: "玩家受到傷害時， # 秒內額外 #% 護甲值".into() });
        store.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().next().and_then(version), Some(VERSION));
        assert_eq!(Store::open(Some(path.clone())).list, store.list, "this version reads as it is");
        // another older file: set aside beside the first one, which stays as it was
        std::fs::write(&path, "5111007\t\tolder still\n").unwrap();
        assert!(Store::open(Some(path.clone())).list.is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("affix-learned.v1-2.tsv")).unwrap(), "5111007\t\tolder still\n");
        assert_eq!(std::fs::read_to_string(dir.join("affix-learned.v1.tsv")).unwrap(), old);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only a file naming no version or one below 2 is set aside: a later version (`# v3`, `# v10`) reads as it is and
    /// keeps its name, so it is never renamed `.v1`, nor is `# v2` alone on its line; `# v1` and a line like `# v2x`
    /// (no version) are set aside.
    #[test]
    fn a_newer_file_is_not_set_aside() {
        assert_eq!(version(HEADER), Some(2));
        assert_eq!(version("# v10 later"), Some(10));
        assert_eq!(version("# v1 older"), Some(1));
        assert_eq!(version("# v2"), Some(2), "the number alone on its line");
        assert_eq!(version("# v2\t"), Some(2));
        assert_eq!(version("# v2x"), None);
        assert_eq!(version("# v 2 "), None);
        assert_eq!(version("# Map affixes"), None);
        let dir = std::env::temp_dir().join(format!("affix-learned-v3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("affix-learned.tsv");
        let newer = "# v3 a later version\n5111007\t#% Max Erosion Resistance for you\t\n";
        std::fs::write(&path, newer).unwrap();
        let store = Store::open(Some(path.clone()));
        assert_eq!(store.list, vec![Learned { id: "5111007".into(), en: "#% Max Erosion Resistance for you".into(), zh: String::new() }]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer, "kept where it is");
        assert!(!dir.join("affix-learned.v1.tsv").exists());
        // `# v2` alone on the first line (a hand-edited file): this version, read as it is
        let bare = "# v2\r\n5111007\t#% Max Erosion Resistance for you\t\r\n";
        std::fs::write(&path, bare).unwrap();
        assert_eq!(Store::open(Some(path.clone())).list.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bare, "kept where it is");
        assert!(!dir.join("affix-learned.v1.tsv").exists());
        std::fs::write(&path, "# v1 an older version\n5111007\t\tolder\n").unwrap();
        assert!(Store::open(Some(path.clone())).list.is_empty());
        assert!(dir.join("affix-learned.v1.tsv").exists() && !path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
