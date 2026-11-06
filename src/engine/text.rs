//! The text helpers of js/engine.js (an, plural, tierName, cellLabel,
//! describe, toolName) with the in-game names from the data, in the set
//! language (see [`crate::lang`]): a name the zh-TW table lacks stays English.

use super::data::{Beast, Card, Gift, RAINBOW, TERM_CACHE, TERM_INCIDENT, TERM_INSTRUMENT, TERM_TREASURE, Tool, data};
use super::state::Inmate;
use crate::lang;

/// The zh name when the set language is Chinese and the table has one, else `en`.
fn pick<'a>(zh: Option<&'a String>, en: &'a str) -> &'a str {
    match zh {
        Some(name) if lang::zh() => name,
        _ => en,
    }
}

/// an: "an" before a vowel, else "a"; Chinese has no article.
pub fn an(text: &str) -> String {
    if lang::zh() {
        return text.to_string();
    }
    let vowel = text.chars().next().is_some_and(|c| "AEIOUaeiou".contains(c));
    format!("{} {text}", if vowel { "an" } else { "a" })
}

/// "3 mornings"; in Chinese the count and the word's translation, which
/// carries its own measure word (`3 個早上`).
pub fn plural(n: usize, word: &str) -> String {
    if lang::zh() {
        return lang::fill(lang::tr_dyn(word).unwrap_or(word), &[&n]);
    }
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

pub fn tier_name(tier: i32) -> &'static str {
    let d = data();
    let t = tier.clamp(0, RAINBOW) as usize;
    pick(d.zh.tiers[t].as_ref(), &d.tier_names[t])
}

pub fn beast_name(beast: Beast) -> &'static str {
    let d = data();
    pick(d.zh.beasts[beast.index()].as_ref(), &d.beast_names[beast.index()])
}

/// An instrument's in-game name.
pub fn instrument(tool: Tool) -> &'static str {
    let d = data();
    pick(d.zh.tools[tool as usize].as_ref(), &d.tool_names[tool as usize])
}

/// cellLabel: cell 0 is "01".
pub fn cell_label(n: usize) -> String {
    format!("{:02}", n + 1)
}

/// "cell 04", "cells 04 and 08", "cells 04, 08 and 14", or "none".
pub fn cell_list(cells: &[usize]) -> String {
    let labels: Vec<String> = cells.iter().map(|&n| cell_label(n)).collect();
    match labels.as_slice() {
        [] => lang::tr("none").to_string(),
        [one] => lang::trf!("cell {}", one),
        [rest @ .., last] => lang::trf!("cells {} and {}", rest.join(", "), last),
    }
}

pub fn describe(inmate: &Inmate) -> String {
    inmate_name(inmate.tier, inmate.beast)
}

/// "Legendary Colossal Rat"; in Chinese `傳奇巨鼠` without the space, which
/// stays while either name has no zh-TW entry yet (`至臻 Colossal Rat`).
pub fn inmate_name(tier: i32, beast: Beast) -> String {
    let (tier, beast) = (tier_name(tier), beast_name(beast));
    if lang::zh() && !tier.is_ascii() && !beast.is_ascii() { format!("{tier}{beast}") } else { format!("{tier} {beast}") }
}

/// toolName: the name of whatever a pending choice offers by key (an
/// instrument, a gift or a card), or the key itself.
pub fn tool_name(key: &str) -> String {
    let d = data();
    if let Some(t) = Tool::from_key(key) {
        return instrument(t).to_string();
    }
    if let Some(g) = Gift::from_key(key) {
        return pick(d.zh.gifts[g as usize].as_ref(), &d.gift_names[g as usize]).to_string();
    }
    if let Some(c) = Card::from_key(key) {
        return pick(d.zh.cards[c as usize].as_ref(), &d.card_names[c as usize]).to_string();
    }
    key.to_string()
}

pub fn incident_name(id: u32) -> &'static str {
    let d = data();
    pick(d.zh.incidents.get(&id), &d.incident(id).name)
}

/// An objective's name without its "Objective: " prefix; None for an unknown id.
pub fn objective_name(id: u32) -> Option<&'static str> {
    let d = data();
    let en = d.objective(id)?.name.as_str();
    let name = pick(d.zh.objectives.get(&id), en);
    Some(name.trim_start_matches("Objective: ").trim_start_matches("目標：").trim_start_matches("目標:").trim())
}

/// A theme's name; None for no theme or an unknown id.
pub fn theme_name(theme: Option<u32>) -> Option<&'static str> {
    let d = data();
    let id = theme?;
    Some(pick(d.zh.themes.get(&id), d.theme_name(Some(id))?))
}

/// The Hunt action's name.
pub fn hunt_name() -> &'static str {
    let d = data();
    pick(d.zh.actions.get("hunt"), &d.hunt_name)
}

fn term(id: u32, en: &'static str) -> &'static str {
    pick(data().zh.glossary.get(&id), en)
}

/// "Prison Incident".
pub fn incident_term() -> &'static str {
    term(TERM_INCIDENT, &data().incident_term)
}

/// "Torture Instrument".
pub fn instrument_term() -> &'static str {
    term(TERM_INSTRUMENT, &data().instrument_name)
}

/// "Warden's Treasure".
pub fn treasure_term() -> &'static str {
    term(TERM_TREASURE, &data().treasure_name)
}

/// "Warden's Cache".
pub fn cache_term() -> &'static str {
    term(TERM_CACHE, "Warden's Cache")
}
