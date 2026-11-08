//! Screen reading for Vorax (SS11 gameplay): screenshots of the Traditional Chinese client are read into a board
//! ([`read::Reading`]) and then converted into the input [`ScreenState`] of the solver (crates/vorax). The first
//! stage only does offline single screenshots (`ascendant-cli vorax-read`, `vorax-solve`).

mod digits;
mod icons;
pub mod learn;
pub mod names;
pub mod read;
pub mod scale;
mod templates;
// The English game client's digits (feature `en-vorax`, on by default since 1.1.0).
#[cfg(feature = "en-vorax")]
mod templates_en;
// The English client's effect-text words by card (the same feature).
#[cfg(feature = "en-vorax")]
mod templates_en_effects;
pub mod ui_scale;

use crate::lang;
use crate::reader::{Rect, Rgba, decode_png_raw};
use names::{Category, archetype_name, names, rarity_name};
use read::{Kind, Reading};
use serde::Serialize;
use std::fmt::Write;
use std::path::Path;
use vorax::advisor::{Advice, Best, ScreenMonster, ScreenPhase, ScreenState};
use vorax::data::Timing;

/// Read a PNG and convert it to the reference size (2560x1440).
pub fn load(path: &Path) -> Result<Rgba, String> {
    Ok(read::to_reference(&load_raw(path)?))
}

/// Read a PNG at its original size (the captured frame).
pub fn load_raw(path: &Path) -> Result<Rgba, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode_png_raw(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read one screenshot file.
pub fn read_file(path: &Path) -> Result<Reading, String> {
    let raw = load_raw(path)?;
    Ok(read::read_with_frame(&read::to_reference(&raw), Some(&raw)))
}

/// Features of the held instrument icons in a reference-size frame, left to right (20x13x3 per cell): used for the
/// observe mode key and for icon template candidates (same order as `held` of [`read::Reading`]).
pub fn icon_features(img: &Rgba) -> Vec<Vec<f32>> {
    icons::features(img)
}

pub use icons::{IconEval, VsrPick};

/// For evaluation (the #[ignore] evaluation in tests/vorax_read.rs): the held icon of each cell in a reference-size
/// frame, left to right, with the results of our own templates and of the VSR signatures separately.
#[doc(hidden)]
pub fn icon_eval(img: &Rgba) -> Vec<IconEval> {
    icons::evaluate(img)
}

/// For measurement: the held instruments of a reference-size frame (`held` of [`read::Reading`], without running
/// the other reads).
#[doc(hidden)]
pub fn read_held(img: &Rgba) -> Vec<read::Held> {
    icons::read_held(img)
}

/// Traditional Chinese names of the instruments revealed on the intro page (with an icon template but unknown to
/// the solver), for example `蒸骨坩堝`.
pub fn opening_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = icons::OPENING.iter().map(|(zh, _)| *zh).collect();
    // The same item may have templates for several positions (`水蛭罐`).
    v.dedup();
    v
}

/// The range (reference coordinates) of the `j`-th cell counted from the left when there are `n` icon cells in
/// total; None if beyond the cell count.
pub fn icon_rect(j: usize, n: usize) -> Option<Rect> {
    (j < n && n <= icons::SLOTS).then(|| icons::slot(n - 1 - j))
}

/// Manually specified held instruments: solver id and pick round (None means not given).
pub type HeldOverride = Vec<(u8, Option<i32>)>;

/// Value of `--instruments`: comma separated, each item is a solver id, an English name or a Traditional Chinese
/// name, optionally with an `@round` suffix.
pub fn parse_instruments(value: &str) -> Result<HeldOverride, String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|item| {
            let (name, round) = match item.split_once('@') {
                Some((n, r)) => (n.trim(), Some(r.trim().parse::<i32>().map_err(|_| format!("「{item}」的回合不是數字"))?)),
                None => (item, None),
            };
            let id = name.parse::<u8>().ok().filter(|id| vorax::data().instrument(*id).is_some()).or_else(|| {
                names().cards.iter().find(|c| c.category == Category::Instrument && (c.en.eq_ignore_ascii_case(name) || c.zh.as_deref() == Some(name))).and_then(|c| c.id)
            });
            id.map(|id| (id, round)).ok_or_else(|| format!("不認得的手術用具「{name}」"))
        })
        .collect()
}

/// Convert to solver input; when an "unknown" is read or things do not add up, return the reasons instead of a
/// state. `held` is the manually specified held instruments (replacing what the icons read).
pub fn to_screen(r: &Reading, held: Option<&HeldOverride>) -> Result<ScreenState, Vec<String>> {
    let mut why = Vec::new();
    let phase = match r.kind {
        Kind::Potion => ScreenPhase::Potion,
        Kind::Instrument => ScreenPhase::Instrument,
        Kind::Protocol => ScreenPhase::Protocol,
        _ => return Err(vec![zh_en("不是選擇畫面，沒有可以建議的選項。", "Not a choice screen: nothing to advise.").to_string()]),
    };
    let Some(shown_round) = r.round else { return Err(vec![zh_en("回合讀不到。", "The round could not be read.").into()]) };
    for (i, t) in r.tanks.iter().enumerate() {
        if !t.known() {
            why.push(trf2("水箱 {} 有未知的值（種族、稀有度或活性×數量）。", "Tank {} has unknown values (archetype, rarity or activity × quantity).", i + 1));
        }
    }
    if r.tanks.len() != 6 {
        why.push(zh_en("水箱沒讀全。", "Not every tank was read.").into());
    }
    if let (Some(ult), Some(sum)) = (r.ultimate, r.tank_sum)
        && ult != sum
        && !r.ultimate_pending
    {
        why.push(format!("{} {sum} ≠ {ult}", zh_en("各水箱總活性的和不等於最終活性：", "The tanks' totals don't add up to Ultimate Activity:")));
    }
    let hand: Vec<String> = r.cards.iter().map(|c| c.en.clone().unwrap_or_default()).collect();
    for (k, c) in r.cards.iter().enumerate() {
        // The solver never advises on protocols anyway (they do not affect the final activity): protocols
        // missing from the name table (`生命復甦術` in the 2026-11-03 ranked game, since named) do not count as unreadable.
        if c.en.is_none() && phase != ScreenPhase::Protocol {
            why.push(trf2("第 {} 張卡未知。", "Card {} is unknown.", k + 1));
        }
    }
    if r.cards.is_empty() {
        why.push(zh_en("沒有讀到選項卡。", "No cards were read.").into());
    }
    let (instruments, rounds): (Vec<String>, Vec<i32>) = match held {
        Some(list) => {
            let mut out = (Vec::new(), Vec::new());
            for &(id, round) in list {
                let def = vorax::data().instrument(id).expect("checked id");
                if matches!(def.timing, Timing::Periodic(_)) && round.is_none() {
                    why.push(format!("{}「{}」{}", zh_en("每 N 回合生效的手術用具", "The periodic instrument"), def.name, zh_en("要用「@回合」給取得回合。", " needs its pick round as @round.")));
                }
                out.0.push(def.name.clone());
                out.1.push(round.unwrap_or(1));
            }
            out
        }
        None => {
            let mut out = (Vec::new(), Vec::new());
            // The instruments revealed on the intro page only change the initial board; skip them, they do not
            // block advice.
            for (k, h) in r.held.iter().enumerate().filter(|(_, h)| !h.opening) {
                match h.id {
                    None => why.push(trf2("右上角第 {} 個手術用具圖示認不出（可用 --instruments 指定）。", "Held instrument icon {} is unknown (give them with --instruments).", k + 1)),
                    Some(id) => {
                        let def = vorax::data().instrument(id).expect("template id");
                        if matches!(def.timing, Timing::Periodic(_)) {
                            why.push(format!("{}「{}」{}", zh_en("每 N 回合生效的手術用具", "The periodic instrument"), def.name, zh_en("取得回合看不到，請用 --instruments id@回合 指定。", "'s pick round isn't on screen: give it with --instruments id@round.")));
                        }
                        out.0.push(def.name.clone());
                        out.1.push(1);
                    }
                }
            }
            out
        }
    };
    if phase != ScreenPhase::Protocol && r.rerolls.is_none() {
        why.push(zh_en("重擲次數讀不到。", "The reroll count could not be read.").into());
    }
    if !why.is_empty() {
        return Err(why);
    }
    // The left panel shows the round that just ended when choosing an instrument at 8k / 28k; the solver has
    // already advanced to the next round before the choice (end_round in crates/vorax game.rs). When choosing the
    // first instrument at the start it is round 1.
    let round = if phase == ScreenPhase::Instrument && !instruments.is_empty() { shown_round + 1 } else { shown_round };
    Ok(ScreenState {
        tanks: r
            .tanks
            .iter()
            .map(|t| {
                (!t.empty).then(|| ScreenMonster { archetype: t.archetype.expect("known"), rarity: t.rarity.expect("known"), activity: t.activity.expect("known"), quantity: t.quantity.expect("known") })
            })
            .collect(),
        hand,
        instruments,
        instrument_rounds: Some(rounds),
        phase,
        round: Some(round),
        // The other reroll count is not on this screen: left to the solver's default (0).
        potion_refreshes: if phase == ScreenPhase::Potion { r.rerolls } else { None },
        instrument_refreshes: if phase == ScreenPhase::Instrument { r.rerolls } else { None },
        shelley: None,
    })
}

fn zh_en(zh: &'static str, en: &'static str) -> &'static str {
    if lang::zh() { zh } else { en }
}

fn trf2(zh: &str, en: &str, n: usize) -> String {
    (if lang::zh() { zh } else { en }).replacen("{}", &n.to_string(), 1)
}

/// Name of the screen kind in the current language.
pub fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Potion => zh_en("選藥劑（含藥劑箱內的選擇）", "potion choice (or inside a potion kit)"),
        Kind::Instrument => zh_en("選手術用具", "instrument choice"),
        Kind::Protocol => zh_en("選手術方案", "protocol choice"),
        Kind::Resolving => zh_en("結算中（不可操作）", "resolving (no choice)"),
        Kind::Result => zh_en("結算畫面", "result screen"),
        Kind::Other => zh_en("非渴血手術盤面", "not a Vorax board"),
    }
}

/// The board as read, as text for humans (current language).
pub fn describe(r: &Reading) -> String {
    let unknown = zh_en("未知", "unknown");
    let opt = |v: Option<i64>| v.map_or(unknown.to_string(), |v| v.to_string());
    let mut s = String::new();
    let _ = writeln!(s, "{}：{}", zh_en("畫面", "Screen"), kind_name(r.kind));
    if r.kind == Kind::Result {
        let _ = writeln!(s, "{}：{}", zh_en("最終活性", "Ultimate Activity"), opt(r.ultimate));
        return s;
    }
    if r.kind == Kind::Other {
        return s;
    }
    let _ = writeln!(s, "{}：{}/13", zh_en("回合", "Round"), r.round.map_or(unknown.to_string(), |n| n.to_string()));
    let ult = match r.ultimate {
        Some(u) if r.ultimate_pending => format!("{u}（{}）", zh_en("數值還在跳", "still counting")),
        Some(u) => u.to_string(),
        None if r.panel_covered => zh_en("被提示框蓋住", "covered by a tooltip").to_string(),
        None if r.ultimate_pending => zh_en("數值正在變動，讀不到", "still counting, unreadable").to_string(),
        None => zh_en("讀不到", "unreadable").to_string(),
    };
    let _ = writeln!(s, "{}：{ult}　{}：{}　{}：{}", zh_en("最終活性", "Ultimate Activity"), zh_en("各格加總", "tank sum"), opt(r.tank_sum), zh_en("下檔獎勵", "next reward"), opt(r.next_reward));
    if r.kind.is_choice() && r.kind != Kind::Protocol {
        let _ = writeln!(s, "{}：{}", zh_en("剩餘重擲", "Rerolls left"), r.rerolls.map_or(unknown.to_string(), |n| n.to_string()));
    }
    let _ = writeln!(s, "{}", zh_en("水箱：", "Tanks:"));
    for (i, t) in r.tanks.iter().enumerate() {
        if t.empty {
            let _ = writeln!(s, "  {}. {}", i + 1, zh_en("空", "empty"));
            continue;
        }
        let check = if t.consistent() { "" } else { zh_en("　（對不起來）", "  (inconsistent)") };
        let _ = writeln!(
            s,
            "  {}. {}〔{}／{}〕{} × {} = {}{check}",
            i + 1,
            t.name.as_deref().unwrap_or(unknown),
            t.archetype.map_or(unknown, archetype_name),
            t.rarity.map_or(unknown, rarity_name),
            opt(t.activity),
            opt(t.quantity),
            opt(t.total),
        );
    }
    let held: Vec<String> = r
        .held
        .iter()
        .map(|h| match (h.opening, &h.zh) {
            (true, Some(zh)) => format!("{zh}{}", zh_en("（開局揭示，只影響初始盤面，略過）", " (opening reveal: only the starting board, skipped)")),
            _ => h.id.and_then(|_| shown_instrument(h)).unwrap_or_else(|| unknown.to_string()),
        })
        .collect();
    let _ = writeln!(s, "{}：{}", zh_en("已持有手術用具", "Held instruments"), if held.is_empty() { zh_en("無", "none").to_string() } else { held.join("、") });
    if r.kind.is_choice() {
        let _ = writeln!(s, "{}", zh_en("選項：", "Cards:"));
        for (k, c) in r.cards.iter().enumerate() {
            match c.name() {
                Some(n) => {
                    let how = match c.how {
                        Some(read::How::Partial) => zh_en("（卡名被遮，依可見部分）", " (name partly hidden)"),
                        Some(read::How::Tooltip) => zh_en("（依提示框）", " (from the tooltip)"),
                        Some(read::How::Effect) => zh_en("（卡名被遮，依效果文字開頭）", " (name hidden, from the effect text)"),
                        Some(read::How::Carried) => zh_en("（這次沒讀到，沿用同一手前一次讀到的卡名）", " (unread this time, name from this hand's earlier read)"),
                        _ => "",
                    };
                    let _ = writeln!(s, "  {}. {}［{}］{how}", k + 1, n.shown(), n.category.label());
                }
                None => {
                    let _ = writeln!(s, "  {}. {}（{}「{}」）", k + 1, unknown, zh_en("讀到", "read"), c.raw_name.replace(' ', ""));
                }
            }
        }
    }
    if !r.problems.is_empty() {
        let _ = writeln!(s, "{}", zh_en("注意：", "Notes:"));
        for p in &r.problems {
            let _ = writeln!(s, "  - {p}");
        }
    }
    s
}

fn shown_instrument(h: &read::Held) -> Option<String> {
    Some(if lang::zh() { h.zh.clone().or_else(|| h.en.clone())? } else { h.en.clone()? })
}

/// One row of the ranking table: option, expected score (the mean of the final activity at the endgame, capped at
/// 840k), probability of reaching 840k.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceRow {
    /// Name of the option (current language); a target-choosing potion carries `→ 水箱 2、5` (tanks 2 and 5); ones
    /// with no effect get a note.
    pub label: String,
    /// The option's name alone, without the target tanks: the card name (or `重擲`, reroll), with the no-effect note
    /// as in `label`. The window's ranking writes this and draws the tanks from `targets`.
    pub name: String,
    /// The tanks the card acts on (0-based, left to right, the order of `→ 水箱 2、5`); empty for the reroll and a
    /// card with no target.
    pub targets: Vec<u8>,
    pub mean: f64,
    /// Probability of reaching 840k (0 to 1); None when the solver does not give it.
    pub reached: Option<f64>,
    /// This row is the reroll.
    pub refresh: bool,
    /// This row is the advised option.
    pub best: bool,
}

/// The text of one advice for humans, shared by the CLI and the window ([`advice_rows`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceRows {
    /// `建議：蛻生皮溶液 → 水箱 2、5`, `建議：重擲`, or `這個選擇不影響分數。` (advice: pick this potion targeting tanks 2 and 5; advice:
    /// reroll; this choice does not change the score)
    pub headline: String,
    /// Notes from the solver (always take the Panacea and Holy Water), in the current language; each card the
    /// solver knows by name but cannot simulate ([`unsimulated`]) has a line here (`「子一代：寄生蠱卵」無法模擬。`,
    /// "... can't be simulated") instead of being in `unknown`.
    pub note: Option<String>,
    /// From the highest expected score to the lowest; the reroll is placed at its position.
    pub rows: Vec<AdviceRow>,
    /// A one-sentence description of how the expected score is computed.
    pub method: String,
    /// Names the solver does not recognize (the unsimulated cards are in `note`).
    pub unknown: Vec<String>,
}

/// Convert advice `a` of `x` into text for humans. Protocols have no ranking (the solver does not solve them).
pub fn advice_rows(x: &ScreenState, a: &Advice) -> AdviceRows {
    let category = if x.phase == ScreenPhase::Instrument { Category::Instrument } else { Category::Potion };
    let zh = lang::zh();
    let name_of = |index: usize| x.hand.get(index).map_or_else(|| zh_en("重擲", "Reroll").to_string(), |en| card_label(category, en));
    let label = |index: usize, targets: &[u8]| {
        let name = name_of(index);
        if targets.is_empty() {
            name
        } else {
            let ts: Vec<String> = targets.iter().map(|k| (k + 1).to_string()).collect();
            format!("{name} → {} {}", zh_en("水箱", "tank"), ts.join(if zh { "、" } else { "," }))
        }
    };
    let headline = match &a.best {
        Best::Card(r) => format!("{}：{}", zh_en("建議", "Pick"), label(r.index, &r.targets)),
        Best::Refresh => format!("{}：{}", zh_en("建議", "Pick"), zh_en("重擲", "Reroll")),
        Best::Nothing => zh_en("這個選擇不影響分數。", "The choice doesn't change the score.").to_string(),
    };
    let best_card = match &a.best {
        Best::Card(r) => Some((r.index, r.targets.clone())),
        _ => None,
    };
    let mut rows: Vec<AdviceRow> = a
        .ranking
        .iter()
        .map(|r| {
            let no_effect = if r.no_effect == Some(true) { zh_en("（無效果）", " (no effect)") } else { "" };
            AdviceRow {
                label: label(r.index, &r.targets) + no_effect,
                name: name_of(r.index) + no_effect,
                targets: r.targets.clone(),
                mean: r.end.map_or(r.value, |e| e.activity),
                reached: r.end.map(|e| e.reached),
                refresh: false,
                best: best_card.as_ref().is_some_and(|(i, t)| *i == r.index && *t == r.targets),
            }
        })
        .collect();
    // The reroll is placed by expected score (the solver lists it separately, not in the ranking).
    if let Some(e) = a.refresh_end {
        let at = rows.iter().position(|r| r.mean < e.activity).unwrap_or(rows.len());
        let reroll = zh_en("重擲", "Reroll").to_string();
        rows.insert(at, AdviceRow { label: reroll.clone(), name: reroll, targets: Vec::new(), mean: e.activity, reached: Some(e.reached), refresh: true, best: a.best == Best::Refresh });
    }
    let method = if a.plan {
        zh_en("藥劑：每個選項模擬到終局；期望分數是終局最終活性的平均（上限 840k）。", "Potions: each option played to the end; mean is the final Ultimate Activity, capped at 840k.")
    } else {
        zh_en("手術用具：每個選項模擬到終局；期望分數是終局最終活性的平均（上限 840k）。", "Instruments: each option played to the end; mean is the final Ultimate Activity, capped at 840k.")
    };
    let (cannot, unknown): (Vec<String>, Vec<String>) = a.unknown.iter().cloned().partition(|n| unsimulated(n));
    let mut lines: Vec<String> = a.note.iter().flat_map(|n| n.split('\n')).map(str::to_string).collect();
    for name in cannot {
        // A card the solver's note already speaks of keeps that line (it says more: not taken, rounds left).
        let told = lines.iter().any(|l| l.starts_with(&format!("{name}:")) || (vorax::advisor::is_xenopanacea(&name) && l.starts_with("A Xenopanacea:")));
        if !told {
            lines.push(format!("{name}: can\u{2019}t be simulated."));
        }
    }
    let note = (!lines.is_empty()).then(|| lines.join("\n")).map(|en| if zh { lang::vorax_note(&en) } else { en });
    AdviceRows { headline, note, rows, method: method.to_string(), unknown }
}

/// A potion the solver knows by name but cannot simulate: a chain card (`chain_steps_after`, the offspring
/// included) or an entry of the name table's `unsimulatedPotions` (a potion with no solver id). Such a card is
/// not "unknown to the solver" ([`AdviceRows::unknown`]) but "can't be simulated" ([`AdviceRows::note`]).
pub fn unsimulated(en: &str) -> bool {
    vorax::advisor::chain_steps_after(en).is_some() || names().cards.iter().any(|c| c.category == Category::Potion && c.id.is_none() && c.en == en)
}

/// Things on the game screen to box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum What {
    /// The k-th card (left to right, 0-based).
    Card(usize),
    /// The i-th tank (0-based).
    Tank(u8),
    /// The reroll button.
    Reroll,
}

/// A box: the center and half width and height on the reference screen (2560x1440), see [`read::to_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Target {
    pub what: What,
    pub x: f32,
    pub y: f32,
    pub half_w: f32,
    pub half_h: f32,
}

/// Center x of the tank glass (y 345, half width 150, half height 245; section 5.1 of the real-machine record).
const TANK_GLASS_X: [f32; 6] = [566.0, 906.0, 1246.0, 1590.0, 1934.0, 2280.0];

/// Center x of the five fan cards of the large potion kit (reference coordinates, measured without mouse hover in
/// the 2026-11-03 ranked game).
const FAN_X: [i32; 5] = [920, 1132, 1344, 1562, 1768];

/// Things to box for advice `a` (reference coordinates): the advised card and its target tank, or the reroll
/// button; when the score is not affected, nothing is boxed. The card box is fixed at the default position, and a
/// raised card or one pushed aside by a neighbor is still boxed (see below).
pub fn targets(r: &Reading, a: &Advice) -> Vec<Target> {
    match &a.best {
        Best::Card(rk) => {
            let mut out = Vec::new();
            // A raised card (chosen by the player or under the mouse) is still boxed: not boxing it when raised
            // would make the box flicker on and off. The box stays at the default position and size (the n-th card
            // by the order read), and does not follow the position read: when the mouse is on a card, neighbor
            // cards are pushed aside by about 47 and the raised card by about 40, so a box following them would
            // keep jumping (2026-11-04 ranked game, the same box changed position 6 times in 1.3 seconds);
            // pushed-aside and raised cards are still inside the fixed box. Three cards use CARD_X; the five fan
            // cards of the large potion kit use the positions measured without mouse hover (spacing about 212); the
            // four cards of a chain step use HAND4_X (measured, 200-226 apart); for other card counts (the single
            // card in the tutorial game) x is as read and y is fixed.
            if let Some(card) = r.cards.get(rk.index) {
                let fixed = match r.cards.len() {
                    3 => read::CARD_X.get(rk.index).map(|&x| (x, 110.0)),
                    4 => read::HAND4_X.get(rk.index).map(|&x| (x, 110.0)),
                    5 => FAN_X.get(rk.index).map(|&x| (x, 100.0)),
                    _ => None,
                };
                let target = match fixed {
                    Some((x, half_w)) => Target { what: What::Card(rk.index), x: x as f32, y: 1140.0, half_w, half_h: 185.0 },
                    None => {
                        let xs: Vec<i32> = r.cards.iter().map(|c| c.x).collect();
                        let gap = xs.windows(2).map(|w| (w[1] - w[0]).abs()).min().unwrap_or(500);
                        let half_w = (gap as f32 / 2.0 - 4.0).min(125.0);
                        Target { what: What::Card(rk.index), x: card.x as f32, y: 1140.0, half_w, half_h: 185.0 }
                    }
                };
                out.push(target);
            }
            out.extend(rk.targets.iter().filter_map(|&t| TANK_GLASS_X.get(usize::from(t)).map(|&x| Target { what: What::Tank(t), x, y: 345.0, half_w: 150.0, half_h: 245.0 })));
            out
        }
        Best::Refresh => {
            // On the instrument screen there is an extra Shirley portrait to the left of the reroll button.
            let (x, half_w) = if r.kind == Kind::Instrument { (1617.0, 88.0) } else { (1625.0, 75.0) };
            vec![Target { what: What::Reroll, x, y: 1362.0, half_w, half_h: 28.0 }]
        }
        Best::Nothing => vec![],
    }
}

/// The board signature: solve only when two consecutive reads are the same. It includes the screen kind, round,
/// tanks, hand, held icons and reroll count; it excludes the final activity, the tooltip and whether a card is
/// raised, so mouse hover does not make the advice recompute.
pub fn signature(r: &Reading) -> String {
    let mut s = format!("{:?}|{:?}|{:?}|", r.kind, r.round, r.rerolls);
    for t in &r.tanks {
        if t.empty {
            s.push_str("_;");
        } else {
            let _ = write!(s, "{:?},{:?},{:?},{:?};", t.archetype, t.rarity, t.activity, t.quantity);
        }
    }
    s.push('|');
    for c in &r.cards {
        let _ = write!(s, "{};", c.en.as_deref().unwrap_or("?"));
    }
    s.push('|');
    for h in &r.held {
        let _ = write!(s, "{:?}{};", h.id, if h.opening { "o" } else { "" });
    }
    s
}

/// Name of the card in the current language: the solver's English name converted back to the Traditional Chinese
/// name.
pub fn card_label(category: Category, en: &str) -> String {
    names().cards.iter().find(|c| c.category == category && c.en == en).map_or_else(|| en.to_string(), |c| c.shown().to_string())
}
