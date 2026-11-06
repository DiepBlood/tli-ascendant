//! Offline digest of observe mode (`ascendant-cli vorax-learn`, plan section 6.5): scans the `vorax_unknown`,
//! `vorax_tooltip`, `vorax_icon_pair`, `vorax_round` and `vorax_read` rows of `<vorax folder>\*\log.jsonl`, re-runs
//! OCR on the saved crops (two scales plus two thresholds, taking the card name with the most votes), groups by the
//! normalized card name, compares against the entries whose zh is blank in the name file, and outputs a table for
//! humans, JSON that can be pasted directly, icon template candidates and the read success rate per resolution.
//! **It does not write the data files or icons.rs**: a human confirms before pasting.
//!
//! Evidence used for matching: (a) the category and rarity of the type text at the card bottom agree with
//! game.json; (b) the effect text `選擇` (choose) maps to potions that need a target, and `隨機` (random) and `添加`
//! (add) map to those that apply automatically. (c) "re-simulate from the effect" is **not done**: the output says
//! so honestly as well.

use super::names::{Category, names};
use super::read::{inked, norm, type_of};
use crate::reader::ocr::{Line, ocr_all};
use crate::reader::panels::similarity;
use crate::reader::{Rect, Rgba};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use vorax::data::{PotionRarity, Targeting};

/// The name file (the same one as names.rs, compiled into the program; its keys are needed here).
const RAW: &str = include_str!("../../data/vorax-names-zh-TW.json");

/// Card section and category in the name file.
const SECTIONS: [(&str, Category); 4] = [("potions", Category::Potion), ("unsimulatedPotions", Category::Potion), ("instruments", Category::Instrument), ("protocols", Category::Protocol)];

/// A card whose zh is blank in the name file: a candidate for matching.
#[derive(Clone, Debug)]
pub struct Blank {
    pub section: &'static str,
    pub key: String,
    pub en: String,
    pub category: Category,
    /// Rarity in game.json (an unsimulatable potion is Supreme: the rarity at the card bottom is not fixed).
    pub rarity: Option<PotionRarity>,
    /// Whether the potion needs a target to be chosen.
    pub targeting: Option<Targeting>,
}

/// Cards whose zh is blank in the name file.
pub fn blanks() -> Vec<Blank> {
    let z: Value = serde_json::from_str(RAW).unwrap_or(Value::Null);
    let d = vorax::data();
    let mut out = Vec::new();
    for (section, category) in SECTIONS {
        for (key, v) in z[section].as_object().into_iter().flatten() {
            if v["zh"].as_str().is_some_and(|s| !s.trim().is_empty()) {
                continue;
            }
            let en = v["en"].as_str().unwrap_or(key).to_string();
            let id = (category == Category::Potion).then(|| d.potion_by_name(&en)).flatten();
            let (rarity, targeting) = match (category, id) {
                (Category::Potion, Some(id)) => (Some(d.potion(id).rarity), Some(d.potion(id).targeting)),
                (Category::Potion, None) => (Some(PotionRarity::Supreme), None),
                _ => (None, None),
            };
            out.push(Blank { section, key: key.clone(), en, category, rarity, targeting });
        }
    }
    out
}

/// The evidence for one candidate.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub blank: Blank,
    /// (a) Type text: +1 rarity matches, -1 mismatch, 0 cannot tell.
    pub a: i32,
    /// (b) Effect text: +1 matches, -1 mismatch, 0 cannot tell.
    pub b: i32,
}

impl Candidate {
    pub fn score(&self) -> i32 {
        self.a + self.b
    }
}

/// Score the candidates by type text and effect text ((c) is not done). Candidates of a different category than the
/// type text says are not listed.
pub fn candidates(category: Option<Category>, type_text: &str, effect: &str, blanks: &[Blank]) -> Vec<Candidate> {
    let (seen_cat, seen_rarity) = type_of(type_text);
    let category = seen_cat.or(category);
    let e = norm(effect);
    let wants = if e.contains("選擇") {
        Some(true)
    } else if e.contains("隨機") || e.contains("添加") {
        Some(false)
    } else {
        None
    };
    let mut out: Vec<Candidate> = blanks
        .iter()
        .filter(|b| category.is_none_or(|c| c == b.category))
        .map(|b| {
            let a = match (seen_rarity, b.rarity) {
                (Some(_), Some(PotionRarity::Supreme)) | (None, _) | (_, None) => 0,
                (Some(s), Some(r)) => if s == r { 1 } else { -1 },
            };
            let select = b.targeting.map(|t| matches!(t, Targeting::Select1 | Targeting::Select1To2));
            let kit = matches!(b.targeting, Some(Targeting::Box(_)));
            let b_score = match (wants, select) {
                _ if kit => 0,
                (Some(w), Some(s)) => if w == s { 1 } else { -1 },
                _ => 0,
            };
            Candidate { blank: b.clone(), a, b: b_score }
        })
        .collect();
    out.sort_by_key(|c| -c.score());
    out
}

/// Samples of one group (the same normalized card name).
#[derive(Clone, Debug, Default)]
pub struct Group {
    /// Kind of row: "card", "tooltip", "tank", "monster".
    pub what: String,
    pub category: Option<Category>,
    /// The card name with the most votes (normalized) with its vote count and the total number of readings.
    pub name: String,
    pub votes: usize,
    pub tries: usize,
    /// Number of rows (vorax_unknown and vorax_tooltip).
    pub rows: usize,
    pub type_text: String,
    pub effect: String,
    /// Matches a Traditional Chinese name already in the name table (OCR noise, not a missing name).
    pub known: Option<String>,
    /// Sample files (folder / crop file name).
    pub files: Vec<String>,
}

/// One icon template candidate.
#[derive(Clone, Debug)]
pub struct IconCandidate {
    pub key: String,
    /// The card it is paired with: solver id and name; None when there is no pairing.
    pub id: Option<u8>,
    pub card: String,
    pub feature: Vec<u8>,
    pub samples: usize,
}

/// The result of the digest.
#[derive(Debug, Default)]
pub struct Learned {
    pub runs: usize,
    pub unknown_rows: usize,
    pub tooltip_rows: usize,
    pub round_rows: usize,
    pub strip_rows: usize,
    pub groups: Vec<Group>,
    pub icons: Vec<IconCandidate>,
    /// Resolution -> (reads, choice screen reads, those among them without problems).
    pub rates: BTreeMap<String, (usize, usize, usize)>,
    /// Number of crop files that could not be read.
    pub missing: usize,
}

/// One row and the folder it is in.
struct Row {
    dir: PathBuf,
    v: Value,
}

fn rows(root: &Path) -> Result<(usize, Vec<Row>), String> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("log.jsonl").is_file()).collect();
    dirs.sort();
    let mut out = Vec::new();
    for dir in &dirs {
        let text = std::fs::read_to_string(dir.join("log.jsonl")).map_err(|e| format!("{}: {e}", dir.display()))?;
        out.extend(text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|v| Row { dir: dir.clone(), v }));
    }
    Ok((dirs.len(), out))
}

fn kind_category(screen: &str) -> Option<Category> {
    match screen {
        "potion" => Some(Category::Potion),
        "instrument" => Some(Category::Instrument),
        "protocol" => Some(Category::Protocol),
        _ => None,
    }
}

/// Re-run OCR on a crop: the original at 2x and 3x, and the black-on-white version at 2x with each of two
/// thresholds ([`inked`] 85, 90), 4 readings in total.
fn ocr_tries(img: &Rgba) -> [Vec<Line>; 4] {
    let full = Rect::new(0, 0, img.w as i32, img.h as i32);
    let (q85, q90) = (inked(img, full, 85), inked(img, full, 90));
    ocr_all([img, img, &q85, &q90], &[(full, 2.0), (full, 3.0), (full, 2.0), (full, 2.0)])
}

/// The card name, type text and effect text from one reading. `name_band` is the y range where the card name is (in
/// crop pixels), when None take the topmost line with two or more characters.
fn split(lines: &[Line], name_band: Option<(i32, i32)>) -> (String, String, String) {
    let mut ls: Vec<&Line> = lines.iter().filter(|l| !norm(&l.text).is_empty()).collect();
    ls.sort_by_key(|l| (l.rect.y, l.rect.x));
    // The type text is the bottom line (card bottom), and it can name a category and has few characters.
    let type_at = ls.len().checked_sub(1).filter(|&k| type_of(&ls[k].text).0.is_some() && norm(&ls[k].text).chars().count() <= 6);
    let type_line = type_at.map(|k| ls[k].text.clone()).unwrap_or_default();
    let name_at = match name_band {
        Some((y0, y1)) => ls.iter().position(|l| (y0..=y1).contains(&(l.rect.y + l.rect.h / 2))),
        None => (0..ls.len()).find(|&k| Some(k) != type_at && norm(&ls[k].text).chars().count() >= 2),
    };
    let name = name_at.map(|k| ls[k].text.clone()).unwrap_or_default();
    let effect = (0..ls.len()).filter(|&k| Some(k) != name_at && Some(k) != type_at && name_at.is_none_or(|n| k > n)).map(|k| ls[k].text.as_str()).collect::<Vec<_>>().join(" ");
    (name, type_line, effect)
}

/// The Traditional Chinese name in the name table (cards, monsters and the opening-reveal instruments) most similar
/// to `name`: first look for an exact match, then similarity of 0.75 or more, and finally mutual containment (three
/// or more characters).
fn known_name(name: &str) -> Option<String> {
    if name.chars().count() < 2 {
        return None;
    }
    let n = names();
    let all: Vec<String> = n
        .cards
        .iter()
        .filter_map(|c| c.zh.clone())
        .chain(n.monsters.iter().map(|(m, _)| m.clone()))
        .chain(super::opening_names().into_iter().map(str::to_string))
        .filter(|z| !norm(z).is_empty())
        .collect();
    let find = |ok: &dyn Fn(&str) -> bool| all.iter().find(|z| ok(&norm(z))).cloned();
    find(&|z| z == name)
        .or_else(|| find(&|z| similarity(z, name) >= 0.75))
        .or_else(|| find(&|z| name.contains(z) || (z.contains(name) && name.chars().count() >= 3)))
}

/// JPEG decoding (WinRT BitmapDecoder; the project has no crate for JPEG decoding, and Cargo.toml stays untouched).
#[cfg(windows)]
pub fn decode_jpeg(bytes: &[u8]) -> Result<Rgba, String> {
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode, ExifOrientationMode};
    use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
    let go = || -> windows::core::Result<Rgba> {
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream.GetOutputStreamAt(0)?)?;
        writer.WriteBytes(bytes)?;
        writer.StoreAsync()?.join()?;
        writer.FlushAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        let decoder = BitmapDecoder::CreateAsync(&stream)?.join()?;
        let (w, h) = (decoder.PixelWidth()? as usize, decoder.PixelHeight()? as usize);
        let pixels = decoder
            .GetPixelDataTransformedAsync(BitmapPixelFormat::Rgba8, BitmapAlphaMode::Ignore, &BitmapTransform::new()?, ExifOrientationMode::IgnoreExifOrientation, ColorManagementMode::DoNotColorManage)?
            .join()?;
        Ok(Rgba { w, h, data: pixels.DetachPixelData()?.to_vec() })
    };
    go().map_err(|e| e.to_string())
}

#[cfg(not(windows))]
pub fn decode_jpeg(_bytes: &[u8]) -> Result<Rgba, String> {
    Err("JPEG decoding needs Windows".into())
}

/// Scan the logs of every game under `root` (`home\vorax`) and digest them.
pub fn scan(root: &Path) -> Result<Learned, String> {
    let (runs, all) = rows(root)?;
    let mut out = Learned { runs, ..Learned::default() };
    // Grouping: (category, card name) -> group.
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();
    let mut votes: HashMap<(String, String), BTreeMap<String, usize>> = HashMap::new();
    let mut icons: BTreeMap<String, IconCandidate> = BTreeMap::new();
    for row in &all {
        let v = &row.v;
        match v["kind"].as_str().unwrap_or_default() {
            "vorax_round" => out.round_rows += 1,
            "vorax_strip" => out.strip_rows += 1,
            "vorax_read" => {
                let size = match v["size"].as_array() {
                    Some(s) if s.len() == 2 => format!("{}x{}", s[0], s[1]),
                    _ => "未記錄".into(),
                };
                let e = out.rates.entry(size).or_default();
                e.0 += 1;
                if kind_category(v["reading"]["kind"].as_str().unwrap_or_default()).is_some() {
                    e.1 += 1;
                    e.2 += usize::from(v["reading"]["problems"].as_array().is_some_and(|p| p.is_empty()));
                }
            }
            kind @ ("vorax_unknown" | "vorax_tooltip") => {
                let tooltip = kind == "vorax_tooltip";
                if tooltip {
                    out.tooltip_rows += 1;
                } else {
                    out.unknown_rows += 1;
                }
                let what = v["what"].as_str().unwrap_or("tooltip");
                if what == "icon" {
                    let key = v["key"].as_str().unwrap_or_default().to_string();
                    let feature: Vec<u8> = v["feature"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|n| n as u8)).collect();
                    let e = icons.entry(key.clone()).or_insert(IconCandidate { key, id: None, card: String::new(), feature, samples: 0 });
                    e.samples += 1;
                    continue;
                }
                if what != "card" && !tooltip {
                    // Tanks and monsters: only counted (monster names are listed in the table, and are not matched
                    // against cards).
                    let name = norm(v["rawName"].as_str().unwrap_or_default());
                    let g = groups.entry((what.to_string(), name.clone())).or_insert_with(|| Group { what: what.to_string(), name: name.clone(), known: known_name(&name), ..Group::default() });
                    g.rows += 1;
                    g.effect = v["why"].as_str().unwrap_or_default().to_string();
                    if let Some(c) = v["crop"].as_str() {
                        g.files.push(format!("{}\\{c}", row.dir.file_name().unwrap_or_default().to_string_lossy()));
                    }
                    continue;
                }
                let screen = kind_category(v["screen"].as_str().unwrap_or_default());
                let (mut name, mut type_text, mut effect) = if tooltip {
                    (v["raw"].as_str().unwrap_or_default().to_string(), String::new(), String::new())
                } else {
                    (v["rawName"].as_str().unwrap_or_default().to_string(), v["rawType"].as_str().unwrap_or_default().to_string(), v["rawEffect"].as_str().unwrap_or_default().to_string())
                };
                // Re-run OCR: each reading casts one vote.
                let mut ballot: Vec<String> = Vec::new();
                match v["crop"].as_str().map(|c| row.dir.join(c)) {
                    Some(path) => match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| decode_jpeg(&b)) {
                        Ok(img) => {
                            // The tooltip card name is at y 845..900 (reference coordinates, the crop starts at
                            // 630), converted by the crop height.
                            let k = img.h as f32 / 545.0;
                            let band = tooltip.then(|| (((845 - 630) as f32 * k) as i32 - 10, ((900 - 630) as f32 * k) as i32 + 10));
                            for lines in ocr_tries(&img) {
                                let (n, t, e) = split(&lines, band);
                                if !norm(&n).is_empty() {
                                    ballot.push(norm(&n));
                                }
                                if type_text.is_empty() && !t.is_empty() {
                                    type_text = t;
                                }
                                if norm(&e).chars().count() > norm(&effect).chars().count() {
                                    effect = e;
                                }
                            }
                        }
                        Err(_) => out.missing += 1,
                    },
                    None => out.missing += 1,
                }
                if ballot.is_empty() && !norm(&name).is_empty() {
                    ballot.push(norm(&name));
                }
                let mut tally: BTreeMap<String, usize> = BTreeMap::new();
                for b in &ballot {
                    *tally.entry(b.clone()).or_default() += 1;
                }
                if let Some((best, _)) = tally.iter().max_by_key(|(n, c)| (**c, n.chars().count())) {
                    name = best.clone();
                }
                let category = type_of(&type_text).0.or(screen);
                let cat = category.map_or("?".to_string(), |c| format!("{c:?}"));
                let gkey = (cat, norm(&name));
                let g = groups.entry(gkey.clone()).or_insert_with(|| Group { what: what.to_string(), category, name: norm(&name), known: known_name(&norm(&name)), ..Group::default() });
                g.rows += 1;
                g.tries += ballot.len();
                if g.type_text.is_empty() {
                    g.type_text = type_text;
                }
                if norm(&effect).chars().count() > norm(&g.effect).chars().count() {
                    g.effect = effect;
                }
                if let Some(c) = v["crop"].as_str() {
                    g.files.push(format!("{}\\{c}", row.dir.file_name().unwrap_or_default().to_string_lossy()));
                }
                let t = votes.entry(gkey).or_default();
                for b in ballot {
                    *t.entry(b).or_default() += 1;
                }
            }
            "vorax_icon_pair" => {
                let key = v["key"].as_str().unwrap_or_default().to_string();
                let en = v["card"]["en"].as_str().map(str::to_string);
                let id = en.as_deref().and_then(|en| names().cards.iter().find(|c| c.category == Category::Instrument && c.en == en)).and_then(|c| c.id);
                let card = en.or_else(|| v["card"]["rawName"].as_str().map(str::to_string)).unwrap_or_default();
                let feature: Vec<u8> = v["feature"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|n| n as u8)).collect();
                let e = icons.entry(key.clone()).or_insert(IconCandidate { key, id: None, card: String::new(), feature, samples: 0 });
                (e.id, e.card) = (id.or(e.id), card);
                e.samples += 1;
            }
            _ => {}
        }
    }
    for (k, g) in groups.iter_mut() {
        g.votes = votes.get(k).and_then(|t| t.get(&g.name)).copied().unwrap_or(0);
    }
    out.groups = groups.into_values().collect();
    out.icons = icons.into_values().collect();
    Ok(out)
}

/// Kind of the card-name group (first column of the table).
fn group_kind(g: &Group) -> &'static str {
    match (g.what.as_str(), g.category) {
        ("tank", _) => "水箱",
        ("monster", _) => "怪物",
        (_, Some(Category::Potion)) => "藥劑",
        (_, Some(Category::Instrument)) => "手術用具",
        (_, Some(Category::Protocol)) => "手術方案",
        ("tooltip", None) => "提示框",
        _ => "?",
    }
}

/// JSON that can be pasted directly: `{section: {key: {en, zh}}}`, taking only groups whose top score is unique and
/// greater than 0 (a group with only one candidate and no evidence is not taken, and is only listed in the table).
pub fn paste(learned: &Learned) -> Value {
    let blanks = blanks();
    let mut out = Map::new();
    for g in learned.groups.iter().filter(|g| g.known.is_none() && g.category.is_some() && g.name.chars().count() >= 2) {
        let cs = candidates(g.category, &g.type_text, &g.effect, &blanks);
        let unique = match cs.as_slice() {
            [one] => Some(one),
            [a, b, ..] if a.score() > b.score() => Some(a),
            _ => None,
        };
        if let Some(c) = unique.filter(|c| c.score() > 0) {
            let section = out.entry(c.blank.section.to_string()).or_insert_with(|| json!({}));
            section[c.blank.key.as_str()] = json!({ "en": c.blank.en, "zh": g.name });
        }
    }
    Value::Object(out)
}

/// The report for humans (Traditional Chinese).
pub fn report(learned: &Learned, root: &Path) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "渴血觀察模式整理：{}", root.display());
    let _ = writeln!(
        s,
        "{} 局；vorax_unknown {} 列、vorax_tooltip {} 列、vorax_round {} 列、vorax_strip {} 列；讀不到的裁切 {} 個",
        learned.runs, learned.unknown_rows, learned.tooltip_rows, learned.round_rows, learned.strip_rows, learned.missing
    );
    let _ = writeln!(s, "證據：(a) 類型字的類別與稀有度、(b) 效果文字的選擇／隨機；(c) 依效果重新模擬：未做。");
    let _ = writeln!(s);
    let _ = writeln!(s, "== 卡名（重跑 OCR 4 種讀法，取票數最多）==");
    let _ = writeln!(s, "{:<8} {:<16} {:>6} {:>4}  {:<10} 判斷", "類別", "卡名", "票數", "列數", "類型字");
    let blanks = blanks();
    for g in &learned.groups {
        let verdict = if let Some(k) = &g.known {
            format!("已知：名稱表有「{k}」")
        } else if g.category.is_none() || g.name.chars().count() < 2 {
            g.effect.chars().take(40).collect()
        } else {
            let cs = candidates(g.category, &g.type_text, &g.effect, &blanks);
            let top: Vec<String> = cs.iter().take(3).map(|c| format!("{}（a{:+} b{:+}）", c.blank.en, c.a, c.b)).collect();
            if top.is_empty() { "名稱檔沒有 zh 空白的同類條目".into() } else { format!("候選 {}", top.join("、")) }
        };
        let name = if g.name.is_empty() { "（讀不到）".to_string() } else { g.name.clone() };
        let _ = writeln!(s, "{:<8} {:<16} {:>3}/{:<2} {:>4}  {:<10} {}", group_kind(g), name, g.votes, g.tries, g.rows, norm(&g.type_text), verdict);
        if let Some(f) = g.files.first() {
            let _ = writeln!(s, "         樣本 {f}{}", if g.files.len() > 1 { format!(" 等 {} 個", g.files.len()) } else { String::new() });
        }
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "== 可直接貼的 JSON（data/vorax-names-zh-TW.json；人確認後再貼）==");
    let _ = writeln!(s, "{}", serde_json::to_string_pretty(&paste(learned)).unwrap_or_default());
    let _ = writeln!(s);
    let _ = writeln!(s, "== 圖示模板候選（src/vorax/icons.rs 的 TEMPLATES；人確認後再貼）==");
    if learned.icons.is_empty() {
        let _ = writeln!(s, "（沒有認不出的圖示）");
    }
    for ic in &learned.icons {
        let who = match (ic.id, ic.card.is_empty()) {
            (Some(id), _) => format!("id {id}（{}）", ic.card),
            (None, false) => format!("配上「{}」，求解器沒有這件", ic.card),
            (None, true) => "沒有配對（只有 vorax_unknown）".into(),
        };
        let _ = writeln!(s, "// {} {who}，{} 個樣本", ic.key, ic.samples);
        let bytes = ic.feature.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
        let _ = writeln!(s, "({}, [{bytes}]),", ic.id.map_or("?".to_string(), |id| id.to_string()));
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "== 各解析度的讀取成功率（選擇畫面沒有讀不到的地方）==");
    for (size, (all, choice, ok)) in &learned.rates {
        let rate = if *choice > 0 { format!("{:.0}%", *ok as f64 * 100.0 / *choice as f64) } else { "-".into() };
        let _ = writeln!(s, "{size:<12} 讀取 {all:>5}  選擇畫面 {choice:>5}  讀全 {ok:>5}  {rate}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_are_the_entries_without_zh() {
        let b = blanks();
        assert!(b.iter().any(|x| x.section == "protocols" && x.category == Category::Protocol));
        assert!(b.iter().all(|x| names().cards.iter().any(|c| c.en == x.en && c.zh.is_none())));
    }

    #[test]
    fn evidence_a_and_b() {
        let b = blanks();
        // A choice-type normal potion: the candidate with the same rarity that needs a target scores highest.
        let cs = candidates(Some(Category::Potion), "普通藥劑", "選擇1組怪物", &b);
        assert!(cs.iter().all(|c| c.blank.category == Category::Potion));
        if let Some(top) = cs.first().filter(|c| c.blank.rarity == Some(PotionRarity::Normal)) {
            assert!(matches!(top.blank.targeting, Some(Targeting::Select1 | Targeting::Select1To2)) || top.b == 0, "{top:?}");
        }
        // The type text says protocol: only protocols remain.
        let cs = candidates(Some(Category::Potion), "稀有手術方案", "", &b);
        assert!(!cs.is_empty() && cs.iter().all(|c| c.blank.category == Category::Protocol));
    }

    #[test]
    fn split_finds_name_type_and_effect() {
        let line = |t: &str, y: i32| Line { text: t.into(), rect: Rect::new(10, y, 100, 20) };
        let lines = [line("選擇1組怪物", 80), line("怪藥", 30), line("普通藥劑", 260), line("復甦", 110)];
        let (n, t, e) = split(&lines, None);
        assert_eq!((n.as_str(), t.as_str(), e.as_str()), ("怪藥", "普通藥劑", "選擇1組怪物 復甦"));
        let (n, _, _) = split(&lines, Some((70, 95)));
        assert_eq!(n, "選擇1組怪物");
    }
}
