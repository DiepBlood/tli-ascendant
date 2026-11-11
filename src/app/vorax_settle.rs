//! Vorax game review (the settlement of a finished Vorax game): re-solves every potion and instrument choice of the
//! game from its log and works out the opening expectation, the decision loss, the luck, how many standard deviations
//! the result is off, and the single costliest step, like [`super::settle`] does for Tartarus runs.
//!
//! - Decisions: each `vorax_round` row of phase potion, instrument or reroll (protocols and resolving are skipped:
//!   protocols do not change Ultimate Activity). Its board is the latest `vorax_advice` row before it with the same
//!   phase (a reroll takes the advice's phase), the same round (an instrument advice's solver round may be the shown
//!   round + 1, see `crate::vorax::to_screen`) and, when the row lists the whole hand, the same hand. A round row with
//!   no such advice is not a decision.
//! - The move made: a reroll row is the reroll; otherwise `chosen.card` is the card's position on screen (null: the
//!   move is unknown). A potion that takes target tanks: first the tanks under the cursor at the tank clicks
//!   (`chosen.tanks`, see `clicked`); without them (older logs, or a click whose tank is unknown), the one option of that card (with as many targets as tank
//!   clicks, when any option has that many) whose targets all lie in the tanks that changed between the `before` and
//!   `after` signatures; anything else is unknown. Checked on the 2026-11-03..05 logs: when the card was the advised
//!   one, this agreed with the advised targets 61 times out of 61; a looser test (tanks whose archetype or rarity
//!   changed) disagreed about 1 time in 4, because the end of the round changes tanks too, so it is not used.
//! - Re-solve: [`score_options`] plays every option on the same futures; loss = best mean - mean of the move made,
//!   gap_se = the paired standard error of the two; a gap within 2 gap_se is noise (as in `crate::audit`).
//! - Expected: the first re-solved decision's best mean and the standard deviation of that option's single futures.
//!   Actual: the final activity (`vorax_end.ultimate`, else `vorax_battle_end.board`, else the last read's Ultimate
//!   Activity) capped at 840k, the same goal the solver plays for. Luck = actual - expected + decision loss.
//! - Labels are built from the name table in the language passed in, so no global language is switched.

use super::settle::{Outcome, Reach, RunKind, Settled, Step};
use crate::audit::{Verdict, Why};
use crate::lang::Lang;
use crate::vorax::names::{Category, names};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use vorax::advisor::{ChoiceKey, ScreenPhase, ScreenState, Scored, all_option_keys, paired_se, score_options, sd};
use vorax::value::TARGET;

/// Futures per option when settling (the same as the Tartarus settlement).
pub const ROLLOUTS: usize = super::settle::ROLLOUTS;

/// Windows' THREAD_PRIORITY_BELOW_NORMAL.
const BELOW_NORMAL: i32 = -1;

/// Noise threshold: a gap within this many gap_se.
const NOISE_SE: f64 = 2.0;

/// One decision: the board of the advice, the move made, the logged advice and the re-solve.
struct Decision {
    /// The round of the `vorax_round` row.
    round: i64,
    /// The advice's read time (`readAt`).
    read_at: String,
    state: ScreenState,
    /// The simulator can play this screen: not a hand the solver takes a card of without simulating (a Xenopanacea,
    /// or a Holy Water or Dissection whose chain can still be finished). A hand whose chain card can no longer be
    /// finished, or that holds an offspring card, is played without that card and is solvable.
    solvable: bool,
    /// The card picked is one the solver knows by name but cannot simulate (`crate::vorax::unsimulated`: a chain
    /// card it does not take, an offspring card): it is not among the options it plays, and is no unknown move.
    picked_unsimulated: bool,
    made: Result<ChoiceKey, Why>,
    /// The logged advice's best; None when it cannot be read.
    advice: Option<ChoiceKey>,
    scored: Option<Scored>,
}

impl Decision {
    fn verdict(&self) -> Verdict {
        if !self.solvable {
            return Verdict::Unsolved(("the simulator cannot play this hand", "模擬器無法模擬這手牌"));
        }
        if self.picked_unsimulated {
            return Verdict::Unsolved(("the card picked can't be simulated", "所選的卡無法模擬"));
        }
        let made = match &self.made {
            Ok(k) => k,
            Err(why) => return Verdict::Unknown(*why),
        };
        let Some(sc) = &self.scored else {
            return Verdict::Unsolved(("not re-solved", "沒有重算"));
        };
        let (Some(best), Some(mine)) = (sc.best(), sc.find(made)) else {
            return Verdict::Unsolved(("the move made is not among the options", "實際所選不在選項裡"));
        };
        let loss = (sc.mean[best] - sc.mean[mine]).max(0.0);
        if best == mine || loss <= 0.0 {
            return Verdict::Best;
        }
        let gap_se = paired_se(&sc.values[best], &sc.values[mine]);
        if loss <= NOISE_SE * gap_se { Verdict::Noise { loss, gap_se } } else { Verdict::Loss { loss, gap_se } }
    }
}

/// Every row of the log that parses as JSON.
fn rows(text: &str) -> Vec<Value> {
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect()
}

/// The row's top-level `kind` (a nested reading has its own `kind`, which is not looked at).
fn kind(row: &Value) -> &str {
    row.get("kind").and_then(Value::as_str).unwrap_or("")
}

/// A whole number that may have been written as a float.
fn num(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64))
}

/// Rows of kind `k`, last first.
fn last_of<'a>(rows: &'a [Value], k: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    rows.iter().rev().filter(move |r| kind(r) == k)
}

/// Whether advice state `x` is the board of round row `row` (phase `phase`, round `round`).
fn fits(x: &ScreenState, phase: &str, round: i64, row: &Value) -> bool {
    let phase_ok = match phase {
        "potion" => x.phase == ScreenPhase::Potion,
        "instrument" => x.phase == ScreenPhase::Instrument,
        // A reroll: potion or instrument, whichever the advice was.
        _ => x.phase != ScreenPhase::Protocol,
    };
    let round_ok = match x.round {
        None => true,
        Some(r) => {
            let r = i64::from(r);
            r == round || (x.phase == ScreenPhase::Instrument && r == round + 1)
        }
    };
    // The hand at the pick, when every card in it was read.
    let hand: Option<Vec<String>> = row.pointer("/chosen/hand").and_then(Value::as_array).and_then(|h| h.iter().map(|c| c.as_str().map(str::to_string)).collect());
    let hand_ok = match hand {
        Some(h) if !h.is_empty() => h == x.hand,
        _ => true,
    };
    phase_ok && round_ok && hand_ok
}

/// The six tank fields of a board signature (`crate::vorax::signature`); None when it is missing or not six tanks.
fn tanks(sig: Option<&Value>) -> Option<Vec<&str>> {
    let part = sig?.as_str()?.split('|').nth(3)?;
    let t: Vec<&str> = part.split(';').filter(|s| !s.is_empty()).collect();
    (t.len() == 6).then_some(t)
}

/// The option among `mine` (one card's options) whose targets are the tanks under the cursor at the tank clicks
/// (`chosen.tanks`, logged since 2026-11-06): the same tanks in the same order, else the same set of tanks. None
/// when the row has no such list (older logs), it is empty, a click's tank is unknown (null), or no single option
/// fits (a changed mind clicks more tanks than the potion takes): the targets are then inferred from the boards.
fn clicked(row: &Value, mine: &[&ChoiceKey]) -> Option<ChoiceKey> {
    let tanks: Vec<u8> = row.pointer("/chosen/tanks")?.as_array()?.iter().map(|t| t.as_u64().and_then(|t| u8::try_from(t).ok())).collect::<Option<_>>()?;
    if tanks.is_empty() {
        return None;
    }
    let exact: Vec<&ChoiceKey> = mine.iter().copied().filter(|k| k.2 == tanks).collect();
    if let [k] = exact[..] {
        return Some(k.clone());
    }
    let set = |t: &[u8]| {
        let mut t = t.to_vec();
        t.sort_unstable();
        t.dedup();
        t
    };
    let clicked = set(&tanks);
    let same: Vec<&ChoiceKey> = mine.iter().copied().filter(|k| set(&k.2) == clicked).collect();
    match same[..] {
        [k] => Some(k.clone()),
        _ => None,
    }
}

/// The card's position on screen in round row `row` (`chosen.card`), None when not recorded.
fn picked(row: &Value) -> Option<usize> {
    row.pointer("/chosen/card").and_then(Value::as_u64).map(|c| c as usize)
}

/// The move made in round row `row`, among the options `keys`.
fn made(row: &Value, keys: &[ChoiceKey]) -> Result<ChoiceKey, Why> {
    if row.get("phase").and_then(Value::as_str) == Some("reroll") {
        return Ok((1, 0, Vec::new()));
    }
    let Some(card) = row.pointer("/chosen/card").and_then(Value::as_u64) else {
        return Err(("the card picked was not recorded", "沒有記到選了哪張卡"));
    };
    let card = card as usize;
    let mine: Vec<&ChoiceKey> = keys.iter().filter(|k| k.0 == 0 && k.1 == card).collect();
    match mine[..] {
        [] => return Err(("the card picked is not among the options", "選的卡不在選項裡")),
        // A potion with no target, a potion kit, an instrument, or a potion with only one tank to target.
        [k] => return Ok(k.clone()),
        _ => {}
    }
    if let Some(k) = clicked(row, &mine) {
        return Ok(k);
    }
    let (Some(before), Some(after)) = (tanks(row.get("before")), tanks(row.get("after"))) else {
        return Err(("no board was read after the pick", "選完之後沒有讀到盤面"));
    };
    let changed: Vec<u8> = (0..6).filter(|&i| before[i] != after[i]).map(|i| i as u8).collect();
    let clicks = row.pointer("/chosen/tankClicks").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut pool = mine;
    if pool.iter().any(|k| k.2.len() == clicks) {
        pool.retain(|k| k.2.len() == clicks);
    }
    let hits: Vec<&ChoiceKey> = pool.into_iter().filter(|k| k.2.iter().all(|t| changed.contains(t))).collect();
    match hits[..] {
        [k] => Ok(k.clone()),
        _ => Err(("the potion's target tank is unclear", "看不出藥劑用在哪個水箱")),
    }
}

/// The logged advice's best as a `ChoiceKey` (`"refresh"`, a ranked card, or null for "no effect").
fn advice_key(best: Option<&Value>) -> Option<ChoiceKey> {
    match best? {
        Value::String(s) if s == "refresh" => Some((1, 0, Vec::new())),
        Value::Null => Some((2, 0, Vec::new())),
        b @ Value::Object(_) => {
            let index = b.get("index")?.as_u64()? as usize;
            let targets: Vec<u8> = b.get("targets").and_then(Value::as_array).map(|t| t.iter().filter_map(|x| x.as_u64().map(|x| x as u8)).collect()).unwrap_or_default();
            Some((0, index, targets))
        }
        _ => None,
    }
}

/// The decisions of the log, with the move made (not re-solved yet).
fn decisions(rows: &[Value]) -> Vec<Decision> {
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if kind(row) != "vorax_round" {
            continue;
        }
        let phase = row.get("phase").and_then(Value::as_str).unwrap_or("");
        if !matches!(phase, "potion" | "instrument" | "reroll") {
            continue;
        }
        // A row whose board or hand was not read (`missing`, see `VoraxSession::synced`) cannot be matched to its
        // advice: an empty hand fits any advice of the round, often the previous hand's.
        let lacks = |what: &str| row.get("missing").and_then(Value::as_array).is_some_and(|m| m.iter().any(|x| x.as_str() == Some(what)));
        if lacks("before") || lacks("hand") {
            continue;
        }
        let Some(round) = row.get("round").and_then(Value::as_i64) else { continue };
        let found = rows[..i].iter().rev().filter(|r| kind(r) == "vorax_advice").find_map(|a| {
            let x: ScreenState = serde_json::from_value(a.get("state")?.clone()).ok()?;
            fits(&x, phase, round, row).then_some((a, x))
        });
        let Some((adv, state)) = found else { continue };
        let keys = all_option_keys(&state);
        let made = made(row, keys.as_deref().unwrap_or(&[]));
        let read_at = adv.get("readAt").or_else(|| adv.get("at")).and_then(Value::as_str).unwrap_or("").to_string();
        let picked_unsimulated = phase == "potion" && picked(row).and_then(|c| state.hand.get(c)).is_some_and(|n| crate::vorax::unsimulated(n));
        out.push(Decision { round, read_at, state, solvable: keys.is_some(), picked_unsimulated, made, advice: advice_key(adv.pointer("/advice/best")), scored: None });
    }
    out
}

/// Re-solves the decisions whose move made is known, plus the first one (the expectation comes from it), decisions in
/// parallel on the caller's rayon pool.
fn score(ds: &mut [Decision], rollouts: usize) {
    use rayon::prelude::*;
    let futures = u32::try_from(rollouts).unwrap_or(u32::MAX);
    let first = ds.iter().position(|d| d.solvable);
    ds.par_iter_mut().enumerate().filter(|(i, d)| d.solvable && (d.made.is_ok() || Some(*i) == first)).for_each(|(_, d)| {
        d.scored = score_options(&d.state, futures, true);
    });
}

/// Whether the game reached its target, as far as the final activity can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    Reached,
    Missed,
    /// The final activity is the battle panel's (`ultimateFrom` "battle") and just over the target, within its
    /// tolerance ([`panel_tolerance`]): the board's own final activity may be just under it.
    Unsure,
}

/// The battle panel shows activity in steps of 0.1k (`836.4K`, see `crate::vorax::read::read_battle`) below 1M.
const PANEL_STEP: i64 = 100;
/// From 1M up the panel shows M with one decimal (`1.0M`), steps of 100k: every game that reached 1M showed it that
/// way in its battle rows' raw text (the 2026-11-06 03:00, 04:43 and 12:23 games, e.g. `1.0M + 482.1K / 999.9M`,
/// OCR'd as `1. OM` / `1 · OM`), and all logged an activity of exactly 1,000,000 from the panel.
const PANEL_STEP_M: i64 = 100_000;
/// The first value the panel shows in M.
const PANEL_M: i64 = 1_000_000;
/// How far the panel's last activity may run over the board's final activity, as a share of it: the ranked replay
/// (2026-11-03) ended its battle at 836.4k for a final activity of 833,372 (+0.36%); with that one sample, 0.5%.
const PANEL_OVER: f64 = 0.005;

/// The tolerance of a final activity `value` read from the battle panel: one display step at that value
/// ([`PANEL_STEP`] below [`PANEL_M`], [`PANEL_STEP_M`] from it) plus [`PANEL_OVER`].
pub fn panel_tolerance(value: i64) -> i64 {
    let step = if value >= PANEL_M { PANEL_STEP_M } else { PANEL_STEP };
    step + (value as f64 * PANEL_OVER).ceil() as i64
}

/// Whether a final activity `value` reached `target`. A value from the battle panel (`approx`) runs over the
/// board's, so under the target it was missed, and over it by less than [`panel_tolerance`] it is unsure.
pub fn arrival(value: i64, approx: bool, target: i64) -> Arrival {
    if value < target {
        Arrival::Missed
    } else if approx && value < target + panel_tolerance(value) {
        Arrival::Unsure
    } else {
        Arrival::Reached
    }
}

/// Whether the final activity ([`final_activity`]) is the battle panel's: the `vorax_end` row that gives it says
/// `ultimateFrom` "battle". Older logs have no such value, and are taken as exact.
fn approximate(rows: &[Value]) -> bool {
    last_of(rows, "vorax_end")
        .find(|r| r.get("ultimate").and_then(num).is_some())
        .is_some_and(|r| r.get("ultimateFrom").and_then(Value::as_str) == Some("battle"))
}

/// Whether the game in the Vorax log `text` reached the solver's target (840k), for the board page: None when no
/// final activity was logged. [`Reach::reached`] is true only for [`Arrival::Reached`].
pub fn arrival_of_log(text: &str) -> Option<Arrival> {
    let rows = rows(text);
    final_activity(&rows).map(|f| arrival(f, approximate(&rows), TARGET))
}

/// The final activity: `vorax_end.ultimate`, else `vorax_battle_end.board`, else the last read's Ultimate Activity.
fn final_activity(rows: &[Value]) -> Option<i64> {
    last_of(rows, "vorax_end")
        .find_map(|r| num(r.get("ultimate")?))
        .or_else(|| last_of(rows, "vorax_battle_end").find_map(|r| num(r.get("board")?)))
        .or_else(|| last_of(rows, "vorax_read").find_map(|r| num(r.pointer("/reading/ultimate")?)))
}

fn screen_name(ui: Lang, x: &ScreenState) -> &'static str {
    match (x.phase == ScreenPhase::Instrument, ui == Lang::ZhTw) {
        (true, true) => "手術用具",
        (true, false) => "Instrument",
        (false, true) => "藥劑",
        (false, false) => "Potion",
    }
}

/// The label of option `k` on screen `x` in language `ui`: the card name (Traditional Chinese when the name table has
/// it), a potion's targets as `→ 水箱 2、5` / `→ tank 2,5` (as `crate::vorax::advice_rows`), or the reroll.
fn label(ui: Lang, x: &ScreenState, k: &ChoiceKey) -> String {
    let zh = ui == Lang::ZhTw;
    match k.0 {
        1 => (if zh { "重擲" } else { "Reroll" }).to_string(),
        2 => (if zh { "不影響分數" } else { "No effect" }).to_string(),
        _ => {
            let category = if x.phase == ScreenPhase::Instrument { Category::Instrument } else { Category::Potion };
            let en = x.hand.get(k.1).map_or("?", String::as_str);
            let name = names()
                .cards
                .iter()
                .find(|c| c.category == category && c.en == en)
                .and_then(|c| if zh { c.zh.clone() } else { None })
                .unwrap_or_else(|| en.to_string());
            if k.2.is_empty() {
                name
            } else {
                let ts: Vec<String> = k.2.iter().map(|t| (t + 1).to_string()).collect();
                format!("{name} → {} {}", if zh { "水箱" } else { "tank" }, ts.join(if zh { "、" } else { "," }))
            }
        }
    }
}

fn step(d: &Decision, ui: Lang, loss: f64, gap_se: f64, noise: bool) -> Step {
    Step {
        day: d.round,
        read_at: d.read_at.clone(),
        screen: screen_name(ui, &d.state),
        made: d.made.as_ref().map(|k| label(ui, &d.state, k)).unwrap_or_default(),
        best: d.scored.as_ref().and_then(|s| s.best().map(|b| label(ui, &d.state, &s.keys[b]))).unwrap_or_default(),
        advice: d.advice.as_ref().map(|k| label(ui, &d.state, k)).unwrap_or_default(),
        followed: match (&d.made, &d.advice) {
            (Ok(m), Some(a)) => Some(m == a),
            _ => None,
        },
        loss,
        gap_se,
        noise,
    }
}

/// Turns re-solved decisions into a settlement; `fin` is the final activity.
fn summarize(folder: &str, ds: &[Decision], fin: Option<i64>, approx: bool, ui: Lang, rollouts: usize) -> Settled {
    let verdicts: Vec<Verdict> = ds.iter().map(Decision::verdict).collect();
    let loss: f64 = verdicts.iter().map(Verdict::loss).sum();
    let noisy: Vec<f64> = verdicts.iter().filter_map(|v| if let Verdict::Noise { loss, .. } = v { Some(*loss) } else { None }).collect();
    let noise: (f64, usize) = (noisy.iter().sum(), noisy.len());
    // The first re-solved decision: (best mean, its single-future standard deviation, its chance of reaching 840k).
    let first: Option<(f64, f64, f64)> = ds.iter().find_map(|d| {
        let sc = d.scored.as_ref()?;
        let b = sc.best()?;
        Some((sc.mean[b], sd(&sc.values[b]), sc.reached[b]))
    });
    let expected = first.map(|(m, s, _)| (m, s));
    let actual = fin.map(|f| f.min(TARGET) as f64);
    let luck = match (actual, expected) {
        (Some(a), Some((e, _))) => Some(a - e + loss),
        _ => None,
    };
    let z = match (actual, expected) {
        (Some(a), Some((e, s))) if s > 0.0 => Some((a - e) / s),
        _ => None,
    };
    // No drift correction for Vorax: |luck| against the loss, as before.
    let cause = super::settle::cause(loss, luck, None);
    let worst = ds
        .iter()
        .zip(&verdicts)
        .filter_map(|(d, v)| match v {
            Verdict::Loss { loss, gap_se } => Some((d, *loss, *gap_se, false)),
            Verdict::Noise { loss, gap_se } if *loss > 0.0 => Some((d, *loss, *gap_se, true)),
            _ => None,
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(d, loss, gap_se, noise)| step(d, ui, loss, gap_se, noise));
    let reach = match (first, fin) {
        // A battle panel value just over the target is not counted as reached ([`Arrival::Unsure`]).
        (Some((_, _, chance)), Some(f)) => Some(Reach { target: TARGET, chance, reached: arrival(f, approx, TARGET) == Arrival::Reached, unsure: arrival(f, approx, TARGET) == Arrival::Unsure }),
        _ => None,
    };
    Settled {
        folder: folder.to_string(),
        vaulted: None,
        actual,
        expected,
        loss,
        noise,
        luck,
        z,
        expected_net: None,
        z_net: None,
        cause,
        worst,
        scored: ds.iter().filter(|d| d.scored.is_some()).count(),
        unknown: verdicts.iter().filter(|v| matches!(v, Verdict::Unknown(_))).count(),
        rollouts,
        kind: RunKind::Vorax,
        reach,
        riot: None,
    }
}

/// Settles the Vorax log `text` of the folder `folder`; labels are in `ui`. The re-solving uses the caller's current
/// rayon pool.
pub fn settle(folder: &str, text: &str, ui: Lang, rollouts: usize) -> Outcome {
    let rows = rows(text);
    if !rows.iter().any(|r| kind(r) == "vorax_end") {
        return Outcome::Unfinished;
    }
    let mut ds = decisions(&rows);
    if ds.is_empty() {
        return Outcome::Empty;
    }
    score(&mut ds, rollouts);
    Outcome::Settled(Box::new(summarize(folder, &ds, final_activity(&rows), approximate(&rows), ui, rollouts)))
}

/// Settles the folder `dir` (reads its log.jsonl); no log file means nothing to settle.
pub fn settle_dir(dir: &Path, ui: Lang, rollouts: usize) -> Outcome {
    let name = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
    match std::fs::read_to_string(dir.join("log.jsonl")) {
        Ok(text) => settle(&name, &text, ui, rollouts),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::Empty,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// Settles `dir` in the background, as [`super::settle::spawn`]: waits for `written` (the log is fully written), then
/// re-solves on a small pool of a quarter of the cores at below normal priority, [`ROLLOUTS`] futures per option. The
/// result comes through the returned channel, and `notify` is called after it is sent. If the thread dies the channel
/// disconnects, and the caller treats that as a failure.
pub fn spawn(dir: PathBuf, written: Option<Receiver<()>>, notify: impl Fn() + Send + 'static) -> Receiver<Outcome> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        crate::solver::solve::set_priority(BELOW_NORMAL);
        if let Some(w) = written {
            // Either a message arriving or the writing thread ending (channel disconnected) means writing is done.
            let _ = w.recv();
        }
        let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).max(1));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("vorax-settle-{i}"))
            .start_handler(|_| crate::solver::solve::set_priority(BELOW_NORMAL))
            .build();
        let out = match pool {
            Ok(pool) => pool.install(|| settle_dir(&dir, crate::lang::lang(), ROLLOUTS)),
            Err(e) => Outcome::Failed(e.to_string()),
        };
        let _ = tx.send(out);
        notify();
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HAND: [&str; 3] = ["Fresh Spinal Powder", "Twin Hormone - Bone Sentry", "Potion Kit (Large)"];
    const OTHER: [&str; 3] = ["Fresh Spinal Powder", "Osteogenic Powder", "Xeno-Hormone"];

    /// A screen state as `vorax_advice` logs it: three monsters in tanks 1 to 3.
    fn state(phase: &str, round: i32, hand: &[&str]) -> Value {
        json!({
            "tanks": [
                {"archetype": "agito", "rarity": "normal", "activity": 1, "quantity": 36},
                {"archetype": "aemberon", "rarity": "boss", "activity": 300, "quantity": 1},
                {"archetype": "agito", "rarity": "rare", "activity": 15, "quantity": 12},
                null, null, null
            ],
            "hand": hand, "instruments": [], "instrumentRounds": [], "phase": phase, "round": round,
            "potionRefreshes": 3, "instrumentRefreshes": 2
        })
    }

    fn advice(at: &str, phase: &str, round: i32, hand: &[&str], best: Value) -> String {
        json!({"at": at, "kind": "vorax_advice", "readAt": at, "state": state(phase, round, hand), "advice": {"best": best}, "rollouts": 64}).to_string()
    }

    /// A board signature with the given tank fields (only the tanks are looked at).
    fn sig(tanks: &str) -> String {
        format!("Potion|Some(1)|Some(3)|{tanks}|a;b;c;|")
    }

    fn round_row(at: &str, round: i64, phase: &str, card: Value, hand: &[&str], clicks: u64, after: Option<&str>) -> String {
        json!({
            "at": at, "kind": "vorax_round", "round": round, "phase": phase,
            "chosen": {"card": card, "how": "raised", "hand": hand, "tankClicks": clicks},
            "before": sig("A;B;C;_;_;_;"), "after": after.map(sig),
        })
        .to_string()
    }

    fn end(ultimate: Value) -> String {
        json!({"at": "2026-11-05T10:00:09.000Z", "kind": "vorax_end", "ultimate": ultimate, "roundsSeen": [1]}).to_string()
    }

    fn row(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn screen(phase: &str, round: i32, hand: &[&str]) -> ScreenState {
        serde_json::from_value(state(phase, round, hand)).unwrap()
    }

    #[test]
    fn the_move_made_is_read_from_the_round_row() {
        let x = screen("potion", 1, &HAND);
        let keys = all_option_keys(&x).unwrap();
        // Card 2 targets one of tanks 1 to 3; cards 1 and 3 take no target.
        assert!(keys.contains(&(0, 0, vec![])) && keys.contains(&(0, 2, vec![])), "{keys:?}");
        assert!((0..3).all(|t| keys.contains(&(0, 1, vec![t]))), "{keys:?}");
        let at = "2026-11-05T10:00:01.000Z";
        let card = |c: Value, clicks: u64, after: Option<&str>| made(&row(&round_row(at, 1, "potion", c, &HAND, clicks, after)), &keys);
        assert_eq!(card(json!(0), 0, Some("A;B;C;_;_;_;")), Ok((0, 0, vec![])));
        assert_eq!(card(json!(2), 0, None), Ok((0, 2, vec![])));
        // Only tank 2 changed: the target is tank 2.
        assert_eq!(card(json!(1), 1, Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
        // Tanks 2 and 3 changed (the end of the round changes tanks too): unclear.
        assert!(card(json!(1), 1, Some("A;X;Y;_;_;_;")).is_err());
        // No board after the pick, or no card recorded: unknown.
        assert!(card(json!(1), 1, None).is_err());
        assert!(card(Value::Null, 0, Some("A;X;C;_;_;_;")).is_err());
        // A card that is not in the hand.
        assert!(card(json!(5), 0, Some("A;X;C;_;_;_;")).is_err());
        // A reroll is the reroll, whatever card the tooltip pointed at.
        assert_eq!(made(&row(&round_row(at, 1, "reroll", json!(2), &HAND, 0, None)), &keys), Ok((1, 0, vec![])));
    }

    /// The tanks under the cursor at the tank clicks (`chosen.tanks`) come first; without them, or when one is
    /// unknown or fits no option, the targets are inferred from the boards as before.
    #[test]
    fn the_clicked_tank_is_taken_first() {
        let x = screen("potion", 1, &HAND);
        let keys = all_option_keys(&x).unwrap();
        let at = "2026-11-05T10:00:01.000Z";
        let pick = |tanks: Option<Value>, after: Option<&str>| {
            let mut r = row(&round_row(at, 1, "potion", json!(1), &HAND, 1, after));
            if let Some(t) = tanks {
                r["chosen"]["tanks"] = t;
            }
            made(&r, &keys)
        };
        // No board after the pick: the clicked tank alone tells the target.
        assert_eq!(pick(Some(json!([2])), None), Ok((0, 1, vec![2])));
        // The click wins over the boards (tanks 2 and 3 changed, which alone is unclear).
        assert_eq!(pick(Some(json!([0])), Some("A;X;Y;_;_;_;")), Ok((0, 1, vec![0])));
        assert_eq!(pick(Some(json!([1])), Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
        // Older logs (no list), an empty list, an unknown tank, or a tank no option targets: inferred as before.
        assert_eq!(pick(None, Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
        assert_eq!(pick(Some(json!([])), Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
        assert_eq!(pick(Some(json!([null])), Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
        assert_eq!(pick(Some(json!([5])), Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
        assert!(pick(Some(json!([null])), None).is_err());
        // Two clicks for a one-target potion (a changed mind): no single option fits, so the boards decide.
        assert_eq!(pick(Some(json!([0, 1])), Some("A;X;C;_;_;_;")), Ok((0, 1, vec![1])));
    }

    /// A chain card the solver does not take (an offspring card, or a rite that can no longer be finished) is
    /// picked: the move is "can't be simulated", not unknown and not "not among the options"; picking one of the
    /// simulated cards of the same hand is judged as usual.
    #[test]
    fn a_picked_chain_card_is_unsimulated() {
        for (round, hand) in [(6, ["F1 Gen: Parasite Egg", "Fresh Spinal Powder", "Osteogenic Powder"]), (10, ["First Rite Holy Water: Confession", "Fresh Spinal Powder", "Osteogenic Powder"])] {
            let at = |s: u32| format!("2026-11-05T10:00:0{s}.000Z");
            let log = [
                advice(&at(1), "potion", round, &hand, json!({"index": 1, "value": 1.0})),
                round_row(&at(2), i64::from(round), "potion", json!(0), &hand, 0, Some("A;B;C;_;_;_;")),
                advice(&at(3), "potion", round, &hand, json!({"index": 1, "value": 1.0})),
                round_row(&at(4), i64::from(round), "potion", json!(2), &hand, 0, Some("A;B;C;_;_;_;")),
            ]
            .join("\n");
            let ds = decisions(&rows(&log));
            assert_eq!(ds.len(), 2, "{hand:?}");
            assert!(ds.iter().all(|d| d.solvable), "{hand:?}: the hand is played without the chain card");
            assert!(ds[0].picked_unsimulated && ds[0].made.is_err());
            assert!(matches!(ds[0].verdict(), Verdict::Unsolved((_, zh)) if zh == "所選的卡無法模擬"), "{hand:?}");
            assert!(!ds[1].picked_unsimulated && ds[1].made == Ok((0, 2, vec![])), "{hand:?}");
        }
    }

    #[test]
    fn signatures_and_advice_keys() {
        let v = json!(sig("A;B;C;_;_;_;"));
        assert_eq!(tanks(Some(&v)), Some(vec!["A", "B", "C", "_", "_", "_"]));
        assert_eq!(tanks(Some(&Value::Null)), None);
        assert_eq!(tanks(Some(&json!("Potion|Some(1)|Some(3)|A;B;|x;|"))), None);
        assert_eq!(advice_key(Some(&json!("refresh"))), Some((1, 0, vec![])));
        assert_eq!(advice_key(Some(&json!({"index": 1, "targets": [4], "value": 1.0}))), Some((0, 1, vec![4])));
        assert_eq!(advice_key(Some(&Value::Null)), Some((2, 0, vec![])));
        assert_eq!(advice_key(None), None);
    }

    #[test]
    fn rounds_find_their_advice() {
        let x = screen("instrument", 2, &HAND);
        let r = row(&round_row("t", 1, "instrument", json!(0), &HAND, 0, None));
        // The instrument advice's solver round is the shown round + 1.
        assert!(fits(&x, "instrument", 1, &r));
        assert!(fits(&x, "reroll", 1, &r));
        assert!(!fits(&x, "potion", 1, &r));
        assert!(!fits(&x, "instrument", 3, &r));
        // Another hand on the same round is another screen.
        let y = screen("instrument", 2, &OTHER);
        assert!(!fits(&y, "instrument", 1, &r));
    }

    #[test]
    fn labels_in_both_languages() {
        let x = screen("potion", 1, &HAND);
        assert_eq!(label(Lang::En, &x, &(0, 1, vec![1])), "Twin Hormone - Bone Sentry → tank 2");
        assert_eq!(label(Lang::En, &x, &(0, 0, vec![])), "Fresh Spinal Powder");
        assert_eq!(label(Lang::En, &x, &(1, 0, vec![])), "Reroll");
        assert_eq!(label(Lang::ZhTw, &x, &(1, 0, vec![])), "重擲");
        assert!(label(Lang::ZhTw, &x, &(0, 1, vec![0, 2])).ends_with(" → 水箱 1、3"));
        assert_eq!(screen_name(Lang::ZhTw, &x), "藥劑");
        assert_eq!(screen_name(Lang::En, &screen("instrument", 1, &HAND)), "Instrument");
    }

    /// A reroll of hand OTHER (advised), then card 2 of hand HAND on tank 2, then a protocol.
    fn game_log(end_row: Option<String>) -> String {
        let mut log = vec![
            advice("2026-11-05T10:00:01.000Z", "potion", 1, &OTHER, json!("refresh")),
            round_row("2026-11-05T10:00:02.000Z", 1, "reroll", json!(0), &OTHER, 0, Some("A;B;C;_;_;_;")),
            advice("2026-11-05T10:00:03.000Z", "potion", 1, &HAND, json!({"index": 1, "targets": [1], "value": 1.0})),
            round_row("2026-11-05T10:00:04.000Z", 1, "potion", json!(1), &HAND, 1, Some("A;X;C;_;_;_;")),
            round_row("2026-11-05T10:00:05.000Z", 11, "protocol", json!(0), &["Laser Amputation"], 0, None),
        ];
        log.extend(end_row);
        log.join("\n")
    }

    #[test]
    fn decisions_of_a_hand_made_log() {
        let rows = rows(&game_log(Some(end(json!(500_000)))));
        let ds = decisions(&rows);
        assert_eq!(ds.len(), 2, "the protocol is skipped");
        assert_eq!(ds[0].state.hand, OTHER);
        assert_eq!((ds[0].made.clone(), ds[0].advice.clone()), (Ok((1, 0, vec![])), Some((1, 0, vec![]))));
        assert_eq!(ds[1].state.hand, HAND);
        assert_eq!(ds[1].made, Ok((0, 1, vec![1])));
        assert_eq!(ds[1].read_at, "2026-11-05T10:00:03.000Z");
        assert_eq!(final_activity(&rows), Some(500_000));
    }

    /// A round row that says its board or hand is missing is not a decision: its empty hand would fit the previous
    /// hand's advice of the same round. A row lacking only its card is still one (its move is unknown).
    #[test]
    fn a_row_missing_its_board_is_not_a_decision() {
        let mut log = vec![advice("2026-11-05T10:00:01.000Z", "potion", 1, &OTHER, json!("refresh"))];
        for (k, missing) in [json!(["before", "hand"]), json!(["hand"]), json!(["before"])].into_iter().enumerate() {
            let mut r = row(&round_row(&format!("2026-11-05T10:00:0{}.000Z", k + 2), 1, "reroll", Value::Null, &[], 0, None));
            r["before"] = Value::Null;
            r["missing"] = missing;
            log.push(r.to_string());
        }
        let mut card = row(&round_row("2026-11-05T10:00:06.000Z", 1, "potion", Value::Null, &OTHER, 0, None));
        card["missing"] = json!(["card"]);
        log.push(card.to_string());
        let mut plain = row(&round_row("2026-11-05T10:00:07.000Z", 1, "reroll", Value::Null, &OTHER, 0, None));
        plain["missing"] = json!([]);
        log.push(plain.to_string());
        let ds = decisions(&rows(&log.join("\n")));
        assert_eq!(ds.len(), 2, "only the rows with their board and hand");
        assert!(ds[0].made.is_err(), "the card is unknown");
        assert_eq!(ds[1].made, Ok((1, 0, vec![])));
    }

    #[test]
    fn a_finished_game_is_settled() {
        let out = settle("g", &game_log(Some(end(json!(900_000)))), Lang::En, 64);
        let Outcome::Settled(s) = out else { panic!("{out:?}") };
        assert!(matches!(s.kind, RunKind::Vorax));
        assert_eq!(s.vaulted, None);
        assert_eq!(s.actual, Some(840_000.0), "capped at 840k");
        assert_eq!((s.scored, s.unknown, s.rollouts), (2, 0, 64));
        let (e, _) = s.expected.unwrap();
        assert!(e > 0.0 && e <= 840_000.0, "{e}");
        assert!((s.luck.unwrap() - (840_000.0 - e + s.loss)).abs() < 1e-6);
        let reach = s.reach.unwrap();
        assert!(reach.reached && (0.0..=1.0).contains(&reach.chance) && reach.target == 840_000);
        if let Some(w) = &s.worst {
            assert_eq!(w.screen, "Potion");
            assert_eq!(w.day, 1);
        }
    }

    #[test]
    fn unfinished_and_empty_games() {
        assert_eq!(settle("g", &game_log(None), Lang::En, 16), Outcome::Unfinished);
        // Only a protocol and a round with no advice: nothing to re-solve.
        let log = [
            round_row("2026-11-05T10:00:04.000Z", 1, "potion", json!(1), &HAND, 1, Some("A;X;C;_;_;_;")),
            round_row("2026-11-05T10:00:05.000Z", 11, "protocol", json!(0), &["Laser Amputation"], 0, None),
            end(Value::Null),
        ]
        .join("\n");
        assert_eq!(settle("g", &log, Lang::En, 16), Outcome::Empty);
        let dir = std::env::temp_dir().join(format!("ascendant-vorax-settle-{}-none", std::process::id()));
        assert_eq!(settle_dir(&dir, Lang::En, 16), Outcome::Empty);
    }

    #[test]
    fn a_real_game_is_settled() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax").join("2026-11-05_13-26-29");
        if !dir.join("log.jsonl").exists() {
            println!("skipped: {} is missing", dir.display());
            return;
        }
        let out = settle_dir(&dir, Lang::En, 256);
        let Outcome::Settled(s) = out else { panic!("{out:?}") };
        assert!(s.scored > 0, "{s:?}");
        // vorax_end.ultimate is null in this log: the board before the battle is used.
        assert_eq!(s.actual, Some(830_720.0));
        assert!(s.expected.is_some() && s.luck.is_some() && s.reach.is_some(), "{s:?}");
        println!("{s:#?}");
    }

    /// A final activity from the battle panel just over the target is unsure, not reached. The ranked replay's panel
    /// ended at 836.4k for a final activity of 833,372: against a target at that true value it is unsure, and against
    /// 840k it is missed. A value well over, or an exact one (result screen, settled board, older logs), is reached.
    #[test]
    fn a_battle_panel_value_near_the_target_is_unsure() {
        assert_eq!(panel_tolerance(836_400), 100 + 4_182);
        assert_eq!(arrival(836_400, true, 840_000), Arrival::Missed);
        assert_eq!(arrival(836_400, true, 833_372), Arrival::Unsure, "the ranked game's own final activity");
        assert_eq!(arrival(836_400, true, 833_373), Arrival::Unsure, "truly missed by 1: never reached");
        assert_eq!(arrival(843_500, true, 840_000), Arrival::Unsure);
        assert_eq!(arrival(845_000, true, 840_000), Arrival::Reached);
        assert_eq!(arrival(843_500, false, 840_000), Arrival::Reached);
        assert_eq!(arrival(839_999, false, 840_000), Arrival::Missed);
        let end_from = |u: i64, from: Option<&str>| json!({"at": "2026-11-05T10:00:09.000Z", "kind": "vorax_end", "ultimate": u, "ultimateFrom": from, "roundsSeen": [13]}).to_string();
        assert_eq!(arrival_of_log(&game_log(Some(end_from(842_000, Some("battle"))))), Some(Arrival::Unsure));
        assert_eq!(arrival_of_log(&game_log(Some(end_from(842_000, Some("board"))))), Some(Arrival::Reached));
        // Older logs: no ultimateFrom.
        assert_eq!(arrival_of_log(&game_log(Some(end(json!(842_000))))), Some(Arrival::Reached));
        assert_eq!(arrival_of_log(&game_log(None)), None);
        let out = settle("g", &game_log(Some(end_from(842_000, Some("battle")))), Lang::En, 16);
        let Outcome::Settled(s) = out else { panic!("{out:?}") };
        assert!(!s.reach.unwrap().reached, "unsure is not reached");
        let out = settle("g", &game_log(Some(end_from(842_000, None))), Lang::En, 16);
        let Outcome::Settled(s) = out else { panic!("{out:?}") };
        assert!(s.reach.unwrap().reached);
    }

    /// From 1M up the battle panel shows M with one decimal (`1.0M`, as in every logged game that reached 1M), so one
    /// display step is 100k, not 0.1k: a panel value of `1.0M` is within 105k of a target.
    #[test]
    fn a_battle_panel_value_in_m_has_a_100k_step() {
        assert_eq!(panel_tolerance(999_900), 100 + 5_000);
        assert_eq!(panel_tolerance(1_000_000), 100_000 + 5_000);
        assert_eq!(panel_tolerance(1_120_000), 100_000 + 5_600);
        assert_eq!(arrival(1_000_000, true, 1_000_000), Arrival::Unsure);
        assert_eq!(arrival(1_000_000, true, 990_000), Arrival::Unsure, "10k under: within one M step and the overrun");
        assert_eq!(arrival(1_000_000, true, 900_000), Arrival::Unsure, "100k under: still within one M step");
        assert_eq!(arrival(1_000_000, true, 890_000), Arrival::Reached, "more than one M step and the overrun under");
        assert_eq!(arrival(1_000_000, false, 1_000_000), Arrival::Reached);
        assert_eq!(arrival(1_000_000, true, 840_000), Arrival::Reached, "the 2026-11-06 games against 840k");
    }
}
