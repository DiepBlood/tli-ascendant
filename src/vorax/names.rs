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
    /// Name the English client prints, when it is not the solver's (`enClient` in the name file): the potion the
    /// solver calls `Purest Holy Water` is `The Holiest Water` on the English client's card and tooltip (2026-11-08,
    /// round 7 of 09-50-14, read as 至純聖水 once the client was switched to Traditional Chinese). `en` stays the
    /// solver's, which ties the card to its id.
    pub client_en: Option<String>,
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
            _ => self.printed_en(),
        }
    }

    /// The name the English client prints: [`Self::client_en`] when it has one, the solver's otherwise. What the
    /// English reader matches card names, tooltips and effect texts against, and what an English page shows.
    pub fn printed_en(&self) -> &str {
        self.client_en.as_deref().unwrap_or(&self.en)
    }
}

/// The name table.
pub struct Names {
    pub cards: Vec<CardName>,
    /// Traditional Chinese name and archetype of each monster (the English solver has no monster names).
    pub monsters: Vec<(String, Archetype)>,
    /// English name and archetype of each monster whose English name is known (the English client's tank names).
    pub monsters_en: Vec<(String, Archetype)>,
}

/// File name of the supplementary name file. Its format is a subset of the name file, for example
/// `{"potions": {"38": {"zh": "誘蟲劑"}}, "monsters": {"墮落者": {"zh": "墮落者", "archetype": "agito"}}}`:
/// the four card sections (potions, unsimulatedPotions, instruments, protocols) may only change `zh` and `enClient` of
/// entries the name file already has (keys as in the name file: ids for potions and instruments, English names for
/// protocols); `monsters` may add new ones.
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
    // The Traditional Chinese name of every card already in the table and whose it is: a name the supplementary
    // file gives another card is not applied (two cards with one name could not be told apart).
    let mut taken: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for section in CARDS {
        for (key, v) in base[section].as_object().into_iter().flatten() {
            if let Some(zh) = v["zh"].as_str().map(str::trim).filter(|z| !z.is_empty()) {
                taken.insert(zh.to_string(), format!("{section}.{key}"));
            }
        }
    }
    for (section, entries) in extra.as_object().into_iter().flatten() {
        let (Some(entries), Some(target)) = (entries.as_object(), base.get_mut(section).and_then(Value::as_object_mut)) else { continue };
        for (key, fields) in entries {
            match (target.get_mut(key).and_then(Value::as_object_mut), fields.as_object()) {
                (Some(entry), Some(fields)) => {
                    // Cards only change the Traditional Chinese name and the English client's name: changing the
                    // English name would no longer match the solver's id.
                    for (field, v) in fields.iter().filter(|(f, _)| !CARDS.contains(&section.as_str()) || matches!(f.as_str(), "zh" | "enClient")) {
                        let here = format!("{section}.{key}");
                        let card_zh = CARDS.contains(&section.as_str()) && field == "zh";
                        if card_zh
                            && let Some(zh) = v.as_str().map(str::trim).filter(|z| !z.is_empty())
                            && let Some(other) = taken.get(zh).filter(|o| **o != here)
                        {
                            changes.push(format!("{here}.zh：{v} 和 {other} 重名，未套用"));
                            continue;
                        }
                        if card_zh
                            && let Some(zh) = v.as_str().map(str::trim).filter(|z| !z.is_empty())
                        {
                            taken.insert(zh.to_string(), here.clone());
                        }
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
            // A protocol whose English name is not known has `en` empty and its Traditional Chinese name as the key:
            // the key stands in, so two such protocols are never the same card.
            let en = v["en"].as_str().filter(|s| !s.is_empty()).unwrap_or(key).to_string();
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
            let client_en = v["enClient"].as_str().map(str::trim).filter(|s| !s.is_empty() && *s != en).map(str::to_string);
            cards.push(CardName { category, id, en, client_en, zh: zh(v), rarity });
        }
    }
    let monsters = z["monsters"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, v)| Some((zh(v).unwrap_or_else(|| key.clone()), serde_json::from_value(v["archetype"].clone()).ok()?)))
        .collect();
    let monsters_en = z["monsters"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, v)| Some((v["en"].as_str().map(str::trim).filter(|e| !e.is_empty())?.to_string(), serde_json::from_value(v["archetype"].clone()).ok()?)))
        .collect();
    Names { cards, monsters, monsters_en }
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

/// The surgical instruments' one-line notes (data/vorax-instrument-notes.json, written in our own words from the card
/// text of real games and the simulation): the panel shows them when the mouse rests on an instrument, so the player
/// can match it against the game's icon. Kept apart from the name table, which the reader matches text against.
const NOTES: &str = include_str!("../../data/vorax-instrument-notes.json");

/// The notes by solver id: (Traditional Chinese, English).
fn notes() -> &'static std::collections::HashMap<u8, (String, String)> {
    static NOTES_BY_ID: OnceLock<std::collections::HashMap<u8, (String, String)>> = OnceLock::new();
    NOTES_BY_ID.get_or_init(|| {
        let v: Value = serde_json::from_str(NOTES).expect("vorax-instrument-notes.json");
        v["instruments"]
            .as_object()
            .expect("instruments")
            .iter()
            .map(|(id, n)| {
                let text = |k: &str| n[k].as_str().unwrap_or_default().to_string();
                (id.parse().expect("instrument id"), (text("zh"), text("en")))
            })
            .collect()
    })
}

/// Instrument `id`'s note in the window language; None when it has none.
pub fn instrument_note(id: u8) -> Option<&'static str> {
    notes().get(&id).map(|(zh, en)| if lang::zh() { zh.as_str() } else { en.as_str() }).filter(|t| !t.is_empty())
}

/// The solver id of the instrument named `name`: the solver's English name, the English client's or the Traditional
/// Chinese one (the panel's held instruments are shown by name).
pub fn instrument_id(name: &str) -> Option<u8> {
    names()
        .cards
        .iter()
        .find(|c| c.category == Category::Instrument && (c.en == name || c.client_en.as_deref() == Some(name) || c.zh.as_deref() == Some(name)))
        .and_then(|c| c.id)
}

#[cfg(test)]
mod tests {
    /// Every instrument of the game data has a note in both languages, and no note is for an instrument it lacks.
    #[test]
    fn every_instrument_has_its_notes() {
        let ids: Vec<u8> = vorax::data().instruments.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), 25);
        for &id in &ids {
            let (zh, en) = notes().get(&id).unwrap_or_else(|| panic!("no note for instrument {id}"));
            assert!(!zh.is_empty() && !en.is_empty(), "instrument {id}");
            let chinese = |t: &str| t.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
            assert!(chinese(zh) && !chinese(en), "instrument {id}: {zh} / {en}");
        }
        assert!(notes().keys().all(|id| ids.contains(id)));
        // By name as the panel shows it.
        let claws = names().cards.iter().find(|c| c.category == Category::Instrument && c.id == Some(6)).unwrap();
        assert_eq!(instrument_id(&claws.en), Some(6));
        assert_eq!(claws.zh.as_deref().and_then(instrument_id), Some(6));
    }

    use super::*;

    /// Every potion and instrument with an id matches the solver; Traditional Chinese names are not duplicated.
    #[test]
    fn every_card_maps_to_the_solver() {
        let n = names();
        assert_eq!(n.cards.iter().filter(|c| c.category == Category::Potion && c.id.is_some()).count(), 42);
        assert_eq!(n.cards.iter().filter(|c| c.category == Category::Instrument && c.id.is_some()).count(), 25);
        // 13 from the upstream name file (Yellow Bile Therapy is `黑膽汁療法`, 2026-11-08), and two seen in play that it
        // lacks (keyed by their Traditional Chinese name; their English names from the English client, 2026-11-08).
        assert_eq!(n.cards.iter().filter(|c| c.category == Category::Protocol).count(), 15);
        assert!(n.cards.iter().any(|c| c.en == "Yellow Bile Therapy" && c.zh.as_deref() == Some("黑膽汁療法")));
        for (en, zh) in [("Epidermal Grafting Experiment", "表皮移植實驗"), ("Cardiac Pacing Resuscitation", "生命復甦術")] {
            assert!(n.cards.iter().any(|c| c.category == Category::Protocol && c.en == en && c.zh.as_deref() == Some(zh)), "{zh}");
        }
        let mut zh: Vec<&str> = n.cards.iter().filter_map(|c| c.zh.as_deref()).collect();
        let all = zh.len();
        zh.sort_unstable();
        zh.dedup();
        assert_eq!(zh.len(), all);
        assert!(n.monsters.iter().any(|(m, a)| m == "苦行大教宗" && *a == Archetype::Agito));
        // The English monster names: every monster's, each once (禁典學者's, Taboo Scholar, seen in the English game of
        // 2026-11-08 13:05, the one name missing on either side, an Agito on both).
        assert_eq!(n.monsters_en.len(), n.monsters.len());
        assert!(n.monsters_en.iter().any(|(m, a)| m == "Taboo Scholar" && *a == Archetype::Agito));
        assert!(n.monsters.iter().any(|(m, a)| m == "禁典學者" && *a == Archetype::Agito));
        assert!(n.monsters_en.iter().any(|(m, a)| m == "Apostate" && *a == Archetype::Agito));
        let mut en: Vec<&str> = n.monsters_en.iter().map(|(m, _)| m.as_str()).collect();
        en.sort_unstable();
        en.dedup();
        assert_eq!(en.len(), n.monsters_en.len());
        // The English client's names of cards it prints otherwise: the solver's name and id stay.
        let holy = n.cards.iter().find(|c| c.en == "Purest Holy Water").unwrap();
        assert_eq!((holy.id.is_some(), holy.printed_en(), holy.zh.as_deref()), (true, "The Holiest Water", Some("至純聖水")));
        assert!(n.cards.iter().filter(|c| c.client_en.is_none()).all(|c| c.printed_en() == c.en));
        // No two cards print one English name.
        let mut printed: Vec<&str> = n.cards.iter().map(CardName::printed_en).collect();
        let all = printed.len();
        printed.sort_unstable();
        printed.dedup();
        assert_eq!(printed.len(), all);
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

    /// A name the supplementary file gives a card that another card already has is not applied (here Yellow Bile
    /// Therapy given Trepanation's `顱骨鑽孔術`); a card's own name again is.
    #[test]
    fn the_extra_file_cannot_give_two_cards_one_name() {
        let mut z: Value = serde_json::from_str(RAW).unwrap();
        let extra = serde_json::json!({ "protocols": { "Yellow Bile Therapy": { "zh": "顱骨鑽孔術" }, "Trepanation": { "zh": "顱骨鑽孔術" } } });
        let changes = merge(&mut z, &extra);
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert!(changes[0].contains("重名") && changes[0].contains("protocols.Trepanation"), "{changes:?}");
        let n = parse_value(z);
        assert!(n.cards.iter().any(|c| c.en == "Yellow Bile Therapy" && c.zh.as_deref() == Some("黑膽汁療法")));
    }
}
