//! Name table of Vorax: the Traditional Chinese names of the card names, monster names, races and rarities in
//! data/vorax-names-zh-TW.json. Cards follow the English names of the solver (crates/vorax); reading the screen
//! first matches the Traditional Chinese names, then converts back to the English name for the solver. Cards whose
//! Traditional Chinese name was not copied (zh is an empty string) cannot be recognized from the screen and are
//! only reported as "unknown".
//!
//! The supplementary name file [`EXTRA_FILE`] (placed in the Ascendant folder, `settings::app_dir()`) is read on
//! the first use of the name table and overrides the one compiled into the program: adding names needs no
//! recompile, restarting Ascendant is enough.

use crate::lang;
use serde::Serialize;
use serde_json::Value;
use std::sync::OnceLock;
use vorax::data::PotionRarity;
use vorax::{Archetype, Rarity};

/// The name file, compiled into the program.
const RAW: &str = include_str!("../../data/vorax-names-zh-TW.json");

/// Card category: potion, instrument, protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Category {
    Potion,
    Instrument,
    Protocol,
}

impl Category {
    /// Category name in the current language.
    pub fn label(self) -> &'static str {
        match (self, lang::zh()) {
            (Category::Potion, true) => "藥劑",
            (Category::Instrument, true) => "手術用具",
            (Category::Protocol, true) => "手術方案",
            (Category::Potion, false) => "potion",
            (Category::Instrument, false) => "instrument",
            (Category::Protocol, false) => "protocol",
        }
    }
}

/// The names of one card.
#[derive(Clone, Debug)]
pub struct CardName {
    pub category: Category,
    /// Solver id (potions and instruments); protocols and potions that cannot be simulated have none.
    pub id: Option<u8>,
    /// English name of the solver.
    pub en: String,
    /// Name printed by the Traditional Chinese client; None if not copied.
    pub zh: Option<String>,
    /// Rarity of a potion (the card-bottom text `普通藥劑` / `魔法藥劑` / `稀有藥劑`); None for other categories.
    pub rarity: Option<PotionRarity>,
}

impl CardName {
    /// Name in the current language: English when Traditional Chinese is missing.
    pub fn shown(&self) -> &str {
        match (&self.zh, lang::zh()) {
            (Some(zh), true) => zh,
            _ => &self.en,
        }
    }
}

/// The name table.
pub struct Names {
    pub cards: Vec<CardName>,
    /// Traditional Chinese name and archetype of each monster (the English solver has no monster names).
    pub monsters: Vec<(String, Archetype)>,
}

/// File name of the supplementary name file. Its format is a subset of the name file, for example
/// `{"potions": {"38": {"zh": "誘蟲劑"}}, "monsters": {"墮落者": {"zh": "墮落者", "archetype": "agito"}}}`:
/// the four card sections (potions, unsimulatedPotions, instruments, protocols) may only change `zh` of entries the
/// name file already has (keys as in the name file: ids for potions and instruments, English names for protocols);
/// `monsters` may add new ones.
pub const EXTRA_FILE: &str = "vorax-names-extra.json";

/// The name table, parsed on first use (the built-in name file plus the supplementary name file).
pub fn names() -> &'static Names {
    static NAMES: OnceLock<Names> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut z: Value = serde_json::from_str(RAW).expect("vorax-names-zh-TW.json");
        if let Some(extra) = extra() {
            let changes = merge(&mut z, &extra);
            let _ = EXTRA_CHANGES.set(changes);
        }
        parse_value(z)
    })
}

static EXTRA_CHANGES: OnceLock<Vec<String>> = OnceLock::new();

/// What the supplementary name file changed (`section.key.field：old → new`, where the old value of an added entry
/// is `（新增）`); empty when there is no supplementary file or the name table has not been used yet. One row is
/// written to each game's log, so when investigating a misrecognition it shows whether the supplementary file
/// caused it.
pub fn extra_changes() -> &'static [String] {
    names();
    EXTRA_CHANGES.get().map_or(&[], Vec::as_slice)
}

/// Contents of the supplementary name file; None when there is no file or it cannot be parsed (the reason is
/// printed when it cannot be parsed, and the built-in is used as before).
fn extra() -> Option<Value> {
    // Unit tests do not read the user's supplementary file, so the result does not vary with the file.
    if cfg!(test) {
        return None;
    }
    let path = crate::app::settings::app_dir()?.join(EXTRA_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            None
        }
    }
}

/// Overlay the supplementary name file onto the name file: the card sections only change existing entries, while
/// `monsters` and `unmapped` may add new ones. Returns what was actually changed (see [`extra_changes`]).
fn merge(base: &mut Value, extra: &Value) -> Vec<String> {
    const CARDS: [&str; 4] = ["potions", "unsimulatedPotions", "instruments", "protocols"];
    let mut changes = Vec::new();
    for (section, entries) in extra.as_object().into_iter().flatten() {
        let (Some(entries), Some(target)) = (entries.as_object(), base.get_mut(section).and_then(Value::as_object_mut)) else { continue };
        for (key, fields) in entries {
            match (target.get_mut(key).and_then(Value::as_object_mut), fields.as_object()) {
                (Some(entry), Some(fields)) => {
                    // Cards only change the Traditional Chinese name: changing the English name would no longer
                    // match the solver's id.
                    for (field, v) in fields.iter().filter(|(f, _)| !CARDS.contains(&section.as_str()) || f.as_str() == "zh") {
                        let old = entry.insert(field.clone(), v.clone());
                        if old.as_ref() != Some(v) {
                            changes.push(format!("{section}.{key}.{field}：{} → {v}", old.map_or("（無）".to_string(), |o| o.to_string())));
                        }
                    }
                }
                (None, Some(_)) if !CARDS.contains(&section.as_str()) => {
                    target.insert(key.clone(), fields.clone());
                    changes.push(format!("{section}.{key}：（新增）→ {fields}"));
                }
                _ => {}
            }
        }
    }
    changes
}

fn parse_value(z: Value) -> Names {
    let zh = |v: &Value| v["zh"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let d = vorax::data();
    let mut cards = Vec::new();
    for (section, category) in [("potions", Category::Potion), ("unsimulatedPotions", Category::Potion), ("instruments", Category::Instrument), ("protocols", Category::Protocol)] {
        for (key, v) in z[section].as_object().into_iter().flatten() {
            let en = v["en"].as_str().unwrap_or(key).to_string();
            let id = match category {
                Category::Potion => d.potion_by_name(&en),
                Category::Instrument => d.instrument_by_name(&en),
                Category::Protocol => None,
            };
            let rarity = match (category, id) {
                (Category::Potion, Some(id)) => Some(d.potion(id).rarity),
                (Category::Potion, None) => Some(PotionRarity::Supreme),
                _ => None,
            };
            cards.push(CardName { category, id, en, zh: zh(v), rarity });
        }
    }
    let monsters = z["monsters"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, v)| Some((zh(v).unwrap_or_else(|| key.clone()), serde_json::from_value(v["archetype"].clone()).ok()?)))
        .collect();
    Names { cards, monsters }
}

/// Archetype name in the current language.
pub fn archetype_name(a: Archetype) -> &'static str {
    match (a, lang::zh()) {
        (Archetype::Pest, true) => "蠱蟲",
        (Archetype::Aemberon, true) => "異魔",
        (Archetype::BoneSentry, true) => "骨衛兵",
        (Archetype::Agito, true) => "覺醒者",
        (Archetype::Pest, false) => "Pest",
        (Archetype::Aemberon, false) => "Aemberon",
        (Archetype::BoneSentry, false) => "Bone Sentry",
        (Archetype::Agito, false) => "Agito",
    }
}

/// Traditional Chinese name of the archetype (the text printed in the card effect text).
pub fn archetype_zh(a: Archetype) -> &'static str {
    match a {
        Archetype::Pest => "蠱蟲",
        Archetype::Aemberon => "異魔",
        Archetype::BoneSentry => "骨衛兵",
        Archetype::Agito => "覺醒者",
    }
}

/// Rarity name in the current language.
pub fn rarity_name(r: Rarity) -> &'static str {
    match (r, lang::zh()) {
        (Rarity::Normal, true) => "普通",
        (Rarity::Magic, true) => "魔法",
        (Rarity::Rare, true) => "稀有",
        (Rarity::Boss, true) => "首領",
        (Rarity::Normal, false) => "Normal",
        (Rarity::Magic, false) => "Magic",
        (Rarity::Rare, false) => "Rare",
        (Rarity::Boss, false) => "Boss",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every potion and instrument with an id matches the solver; Traditional Chinese names are not duplicated.
    #[test]
    fn every_card_maps_to_the_solver() {
        let n = names();
        assert_eq!(n.cards.iter().filter(|c| c.category == Category::Potion && c.id.is_some()).count(), 42);
        assert_eq!(n.cards.iter().filter(|c| c.category == Category::Instrument && c.id.is_some()).count(), 25);
        assert_eq!(n.cards.iter().filter(|c| c.category == Category::Protocol).count(), 13);
        let mut zh: Vec<&str> = n.cards.iter().filter_map(|c| c.zh.as_deref()).collect();
        let all = zh.len();
        zh.sort_unstable();
        zh.dedup();
        assert_eq!(zh.len(), all);
        assert!(n.monsters.iter().any(|(m, a)| m == "苦行大教宗" && *a == Archetype::Agito));
    }

    /// Supplementary name file: cards only change existing entries (field by field), new card keys are not added;
    /// monsters may be added.
    #[test]
    fn the_extra_file_fills_in_names() {
        let mut z: Value = serde_json::from_str(RAW).unwrap();
        let extra = serde_json::json!({
            "potions": { "0": { "zh": "測試藥劑", "en": "Renamed" }, "999": { "en": "Not A Potion", "zh": "不存在" } },
            "monsters": { "測試怪": { "zh": "測試怪", "archetype": "pest" } },
            "nonsense": { "x": 1 }
        });
        let changes = merge(&mut z, &extra);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(changes.iter().any(|c| c.contains("potions.0.zh") && c.contains("測試藥劑")), "{changes:?}");
        let n = parse_value(z);
        let zero = n.cards.iter().find(|c| c.category == Category::Potion && c.id == Some(0)).unwrap();
        assert_eq!((zero.zh.as_deref(), zero.en.as_str()), (Some("測試藥劑"), "Probiotic Enzyme Solution"));
        assert!(!n.cards.iter().any(|c| c.en == "Not A Potion"));
        assert_eq!(n.cards.len(), names().cards.len());
        assert!(n.monsters.iter().any(|(m, a)| m == "測試怪" && *a == Archetype::Pest));
    }
}
