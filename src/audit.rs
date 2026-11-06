//! Per-run decision scoring: `ascendant-cli audit` reads the run log (runs\<UTC>\log.jsonl), re-solves every option
//! on the board read at the time of each advice, infers from the later reads and game signals which move the player
//! actually made, and splits the run's whole gap into decision loss and luck.
//!
//! - Only the four screens Patrol, Manage, Interrogation and instrument pick are re-solved; the other screens are only
//!   counted and skipped. The state is rebuilt as the session built it ([`rebuild`]): the logged board plus the run's
//!   theme (the read row's `theme`) and the Usurpation weights (the `Incident(252..=255)` signals logged before the
//!   read). The Warden's Treasure counts the session adds only matter on a treasure pick, which is not re-solved.
//! - The move actually made (marked "unknown" when it cannot be told, and then no loss is counted):
//!   - Patrol: the first Warden position read twice in a row on the same day after `Moved` that differs from the old
//!     one (skipping screens that cover the board, such as instrument pick, reward pick and cell pick); stale reads
//!     with no `Moved` after them do not count as a decision.
//!   - Manage: after the first `Bite` following `Consume`, the one cell among the consume options whose content
//!     changed; a lone `Decision` followed by a prey pick is a Hunt (the target is the new prey mark); a Replace that
//!     `rates` recognizes, or the Replace card read as used up twice in a row, is a Replace; the day number going up by
//!     1 (on the last day, `RunEnd` right after) is End.
//!   - Interrogation: a later read on the same day with 1 more mark is Instigate; a signal that an inmate was vaulted,
//!     or an unchanged mark count with the tier sum up by 1, is Bestow.
//!   - Instrument pick: a `Reroll` right after is a re-roll; otherwise see which instrument was added in the later
//!     reads (up to the next day).
//! - Decision loss = best option's mean - mean of the move made; a gap within 2x gap_se of the move made counts as
//!   noise, listed separately and not included in the loss total.
//! - The expected value at the start takes best_ev and sd from the first advice row of a run that has a round count
//!   (the session's full model); actual = the vaulted count from `run_end` + the last read board's value and objective
//!   reward (the `State::loot` scoring). Luck = actual - expected + loss total.

use crate::app::latency::log_millis;
use crate::board::{CellModel, ChoiceKind, Model, build_state};
use crate::engine::data::{Beast, Card, LOOT, RAINBOW, Tool};
use crate::engine::incidents::{USURPATION_I, usurped};
use crate::engine::{Action, ChoiceId, State};
use crate::lang::{self, Lang};
use crate::rates::{Event, Read, Run, SAME_CLICK_MS, Signal, last_stable, pad, parse_run, reads_after, replace_at, t, width};
use crate::solver::TUNED;
use crate::solver::solve::{SolveOption, candidates, solve};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Write;

/// Seed for the re-solve: the same for every decision, so results are reproducible.
const SEED: u32 = 12345;

/// Noise threshold: a gap within this many gap_se.
const NOISE_SE: f64 = 2.0;

/// The screens that are re-solved.
const SCREENS: [&str; 4] = ["Patrol", "Manage", "Interrogation", "InstrumentPick"];

/// Reasons why something cannot be told: English, Traditional Chinese.
pub type Why = (&'static str, &'static str);

/// Rows that audit additionally needs from the log: advice, original game signal names, run end.
#[derive(Deserialize)]
struct Row {
    at: String,
    kind: String,
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    pick: Option<String>,
    #[serde(default)]
    read_at: Option<String>,
    #[serde(default)]
    rounds: Option<u64>,
    #[serde(default)]
    ev: Option<f64>,
    #[serde(default)]
    best_ev: Option<f64>,
    #[serde(default)]
    sd: Option<f64>,
    #[serde(default)]
    vaulted: Option<u32>,
}

/// One advice row.
struct Advice {
    read_at: String,
    pick: String,
    rounds: u64,
    ev: Option<f64>,
    best_ev: Option<f64>,
    sd: Option<f64>,
}

/// One decision: the board at the time of the advice, the advice, the move actually made, and the re-solve result.
pub struct Decision {
    pub read_at: String,
    pub day: i64,
    pub screen: String,
    /// The advice in the log (label).
    pub pick: String,
    pub model: Model,
    /// The run's theme at the read (a theme id of the game data), see [`rebuild`].
    pub theme: Option<u32>,
    /// The last Usurpation stage seen before the read, see [`rebuild`].
    pub usurped: Option<u32>,
    /// The action the advice maps to; None when the label cannot be matched.
    pub advised: Option<Action>,
    /// The move actually made; the reason when it cannot be told.
    pub actual: Result<Action, Why>,
    /// Each action's label in the output language.
    pub labels: Vec<(Action, String)>,
    /// The re-solved ranking (only re-solved when the move made can be told).
    pub ranking: Vec<SolveOption>,
}

/// The score of one decision.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// The move made cannot be told.
    Unknown(Why),
    /// Cannot be re-solved: the board has problems, or the move made is not among the options.
    Unsolved(Why),
    /// The move made is the best one.
    Best,
    /// The gap is within noise.
    Noise { loss: f64, gap_se: f64 },
    Loss { loss: f64, gap_se: f64 },
}

impl Verdict {
    /// The loss that counts toward the loss total.
    pub fn loss(&self) -> f64 {
        if let Verdict::Loss { loss, .. } = self { *loss } else { 0.0 }
    }
}

impl Decision {
    pub fn label(&self, a: &Action) -> String {
        self.labels.iter().find(|(x, _)| x == a).map_or_else(|| format!("{a:?}"), |(_, l)| l.clone())
    }

    /// Whether the advice was followed; None when the move made or the advice cannot be told.
    pub fn followed(&self) -> Option<bool> {
        match (&self.actual, &self.advised) {
            (Ok(a), Some(b)) => Some(a == b),
            _ => None,
        }
    }

    pub fn verdict(&self) -> Verdict {
        let actual = match &self.actual {
            Ok(a) => a,
            Err(why) => return Verdict::Unknown(*why),
        };
        if self.ranking.is_empty() {
            return Verdict::Unsolved(("not re-solved", "沒有重算"));
        }
        let Some(chosen) = self.ranking.iter().find(|o| &o.action == actual) else {
            return Verdict::Unsolved(("the move made is not among the options", "實際所選不在選項裡"));
        };
        let best = &self.ranking[0];
        if self.ranking.len() == 1 || &best.action == actual {
            return Verdict::Best;
        }
        let loss = (best.mean - chosen.mean).max(0.0);
        if loss <= NOISE_SE * chosen.gap_se { Verdict::Noise { loss, gap_se: chosen.gap_se } } else { Verdict::Loss { loss, gap_se: chosen.gap_se } }
    }

    /// The re-solved best option.
    pub fn best(&self) -> Option<&SolveOption> {
        self.ranking.first()
    }
}

/// One run.
pub struct Game {
    /// Folder name.
    pub folder: String,
    /// Start time (`RunStart` or the folder's first row).
    pub start: String,
    /// End time (`run_end`); None when the run did not end.
    pub end: Option<String>,
    /// Expected value at the start and the single-run standard deviation.
    pub expected: Option<(f64, f64)>,
    pub vaulted: Option<u32>,
    /// The last read board's value and objective reward.
    pub board: Option<f64>,
    pub decisions: Vec<Decision>,
    /// Number of advice rows on other screens or with no read found (not re-solved).
    pub skipped: usize,
}

impl Game {
    /// The actual result: vaulted count + board.
    pub fn actual(&self) -> Option<f64> {
        self.vaulted.map(|v| f64::from(v) + self.board.unwrap_or(0.0))
    }

    pub fn z(&self) -> Option<f64> {
        let ((e, sd), actual) = (self.expected?, self.actual()?);
        (sd > 0.0).then(|| (actual - e) / sd)
    }

    /// Loss total (excluding noise).
    pub fn loss(&self) -> f64 {
        self.decisions.iter().map(|d| d.verdict().loss()).sum()
    }

    /// Sum and count of the gaps within noise.
    pub fn noise(&self) -> (f64, usize) {
        let v: Vec<f64> = self.decisions.iter().filter_map(|d| if let Verdict::Noise { loss, .. } = d.verdict() { Some(loss) } else { None }).collect();
        (v.iter().sum(), v.len())
    }

    /// Luck = actual - expected + loss total.
    pub fn luck(&self) -> Option<f64> {
        Some(self.actual()? - self.expected?.0 + self.loss())
    }
}

/// A run in progress.
struct Span {
    start: String,
    end: Option<String>,
    vaulted: Option<u32>,
    advice: Vec<Advice>,
}

impl Span {
    fn new(start: &str) -> Span {
        Span { start: start.to_string(), end: None, vaulted: None, advice: Vec::new() }
    }
}

/// Reads one folder's log.jsonl, cuts it into runs, and infers the move actually made for each decision (no re-solve).
pub fn games(folder: &str, text: &str) -> Vec<Game> {
    let run = parse_run(folder, text);
    // Original game signal names (`rates::Signal` folds RunStart, RunEnd and Reroll into Other).
    let mut raws: Vec<(i64, String)> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut closed = true;
    for line in text.lines() {
        if !line.contains("\"kind\":\"advice\"") && !line.contains("\"kind\":\"game\"") && !line.contains("\"kind\":\"run_end\"") {
            continue;
        }
        let Ok(r) = serde_json::from_str::<Row>(line) else { continue };
        match r.kind.as_str() {
            "game" => {
                let Some(s) = r.signal else { continue };
                if s == "RunStart" {
                    // A run with no advice yet: start from here instead.
                    match spans.last_mut() {
                        Some(sp) if !closed && sp.advice.is_empty() => sp.start = r.at.clone(),
                        _ => {
                            spans.push(Span::new(&r.at));
                            closed = false;
                        }
                    }
                }
                raws.push((log_millis(&r.at).unwrap_or(0), s));
            }
            "run_end" => {
                if closed {
                    spans.push(Span::new(&r.at));
                }
                if let Some(sp) = spans.last_mut() {
                    sp.end = Some(r.at.clone());
                    sp.vaulted = r.vaulted;
                }
                closed = true;
            }
            "advice" => {
                if closed {
                    spans.push(Span::new(&r.at));
                    closed = false;
                }
                let (Some(read_at), Some(pick)) = (r.read_at, r.pick) else { continue };
                if let Some(sp) = spans.last_mut() {
                    sp.advice.push(Advice { read_at, pick, rounds: r.rounds.unwrap_or(0), ev: r.ev, best_ev: r.best_ev, sd: r.sd });
                }
            }
            _ => {}
        }
    }
    let index: HashMap<&str, usize> = run.events.iter().enumerate().filter_map(|(i, e)| e.read().map(|r| (r.at.as_str(), i))).collect();
    spans.into_iter().filter(|sp| !sp.advice.is_empty()).map(|sp| game(&run, &raws, &index, sp)).collect()
}

fn game(run: &Run, raws: &[(i64, String)], index: &HashMap<&str, usize>, sp: Span) -> Game {
    let ev = &run.events;
    let from = log_millis(&sp.start).unwrap_or(i64::MIN);
    let until = sp.end.as_deref().and_then(log_millis).unwrap_or(i64::MAX);
    // Advice rows with a round count (rounds 0 is a single option; before the 08:47 build ev was recorded as 0).
    let usable: Vec<&Advice> = sp.advice.iter().filter(|a| a.rounds > 0 && a.ev.is_some() && a.best_ev.is_some()).collect();
    let expected = usable.iter().find_map(|a| Some((a.best_ev?, a.sd?)));
    // Of several advice rows for the same decision point keep only the last: same screen, same day, no board-changing
    // signal between.
    let mut kept: Vec<(&Advice, usize)> = Vec::new();
    let mut skipped = 0;
    for a in usable {
        let Some(&i) = index.get(a.read_at.as_str()) else {
            skipped += 1;
            continue;
        };
        let r = ev[i].read().expect("read");
        if let Some(&(_, j)) = kept.last() {
            let p = ev[j].read().expect("read");
            let quiet = j < i && !ev[j + 1..i].iter().any(|e| e.signal().is_some_and(|s| !s.passive()));
            if quiet && p.screen == r.screen && p.model.day == r.model.day {
                kept.pop();
            }
        }
        kept.push((a, i));
    }
    let mut decisions = Vec::new();
    for (a, i) in kept {
        let r = ev[i].read().expect("read");
        if !SCREENS.contains(&r.screen.as_str()) || (r.screen == "Patrol" && stale_patrol(ev, i)) {
            skipped += 1;
            continue;
        }
        decisions.push(decide(run, raws, i, from, until, a));
    }
    // End-of-run board: the board value that was the same in the last 2 consecutive reads; if none, take the last read.
    let board_vals: Vec<Option<f64>> = ev.iter().filter_map(Event::read).filter(|r| in_span(r, from, until)).map(|r| Some(board_loot(&r.model))).collect();
    let board = last_stable(&board_vals).map(|(v, _)| v).or_else(|| board_vals.last().copied().flatten());
    Game { folder: run.name.clone(), start: sp.start, end: sp.end, expected, vaulted: sp.vaulted, board, decisions, skipped }
}

fn in_span(r: &Read, from: i64, until: i64) -> bool {
    log_millis(&r.at).is_some_and(|t| t >= from && t <= until)
}

/// The value of the inmates on the board plus the reward for completing the objective (`State::loot` minus the vault).
fn board_loot(m: &Model) -> f64 {
    let (st, _) = build_state(m, 1);
    st.loot() - st.vault.len() as f64 * LOOT[RAINBOW as usize]
}

/// The theme id of a read row's `theme` (the data's English name, as the session logs it).
pub fn theme_id(name: Option<&str>) -> Option<u32> {
    let name = name?;
    crate::engine::data::data().themes.iter().find(|t| t.name == name).map(|t| t.id)
}

/// The last Usurpation stage (incident 252 to 255) the game's log named in `[from, to]`, as the session keeps it.
fn usurped_by(raws: &[(i64, String)], from: i64, to: i64) -> Option<u32> {
    raws.iter()
        .filter(|(ms, _)| (from..=to).contains(ms))
        .filter_map(|(_, s)| s.strip_prefix("Incident(")?.strip_suffix(')')?.parse::<u32>().ok())
        .filter(|id| (USURPATION_I..USURPATION_I + 4).contains(id))
        .max()
}

/// The state the session solved for a logged board: the board, the run's theme and the Usurpation stages so far.
pub fn rebuild(m: &Model, theme: Option<u32>, last_usurped: Option<u32>) -> (State, Vec<String>) {
    let (mut st, problems) = build_state(m, 1);
    st.theme = theme;
    if let Some(last) = last_usurped {
        usurped(&mut st, last);
    }
    (st, problems)
}

/// The decision for one advice row: board, the action the advice maps to, the move actually made.
fn decide(run: &Run, raws: &[(i64, String)], i: usize, from: i64, until: i64, a: &Advice) -> Decision {
    let r = run.events[i].read().expect("read");
    let m = &r.model;
    let theme = theme_id(r.theme.as_deref());
    let usurped = usurped_by(raws, from, log_millis(&r.at).unwrap_or(i64::MAX));
    let (st, problems) = rebuild(m, theme, usurped);
    let cands: Vec<Action> = candidates(&st).into_iter().map(|c| c.action).collect();
    let actual = if !problems.is_empty() {
        Err(("the board has problems", "盤面有問題"))
    } else {
        match r.screen.as_str() {
            "Patrol" => patrol(&run.events, raws, i, m),
            "Manage" => manage(run, raws, i, until, m, &cands),
            "Interrogation" => interrogation(&run.events, i, until, m),
            _ => instrument(&run.events, raws, i, until, m, &cands),
        }
    };
    // The move made must be one of the options, otherwise it does not count.
    let actual = actual.and_then(|x| if cands.contains(&x) { Ok(x) } else { Err(("the move made is not among the options", "實際所選不在選項裡")) });
    let labels = candidates(&st).into_iter().map(|c| (c.action, c.label)).collect();
    Decision {
        read_at: a.read_at.clone(),
        day: m.day,
        screen: r.screen.clone(),
        pick: a.pick.clone(),
        model: m.clone(),
        theme,
        usurped,
        advised: advised(&st, &a.pick),
        actual,
        labels,
        ranking: Vec::new(),
    }
}

/// The action an advice label maps to: regenerate the labels in the advice's language and compare; equal for the
/// whole string or up to ` (`.
fn advised(st: &State, pick: &str) -> Option<Action> {
    let want = if pick.is_ascii() { Lang::En } else { Lang::ZhTw };
    let was = lang::lang();
    if want != was {
        lang::set(want);
    }
    let cands = candidates(st);
    if want != was {
        lang::set(was);
    }
    let base = |s: &str| s.split(" (").next().unwrap_or(s).trim().to_string();
    if let Some(c) = cands.iter().find(|c| c.label == pick) {
        return Some(c.action.clone());
    }
    let hits: Vec<_> = cands.iter().filter(|c| base(&c.label) == base(pick)).collect();
    (hits.len() == 1).then(|| hits[0].action.clone())
}

/// The first value (non-None) that appears twice in a row.
fn first_stable<T: PartialEq + Clone>(vals: &[Option<T>]) -> Option<T> {
    vals.windows(2).find_map(|w| match (&w[0], &w[1]) {
        (Some(a), Some(b)) if a == b => Some(a.clone()),
        _ => None,
    })
}

/// The first signal after row `i` that changes the board.
fn next_signal(ev: &[Event], i: usize) -> Option<usize> {
    ev.iter().enumerate().skip(i + 1).find(|(_, e)| e.signal().is_some_and(|s| !s.passive())).map(|(k, _)| k)
}

/// The `sig` signal within `SAME_CLICK_MS` before or after row `k`.
fn near(ev: &[Event], k: usize, sig: Signal) -> Option<usize> {
    let t = ev[k].ms();
    ev.iter().enumerate().find(|(_, e)| e.signal() == Some(sig) && (e.ms() - t).abs() <= SAME_CLICK_MS).map(|(j, _)| j)
}

/// Reads after row `k`, before the `stop` signal (passive signals do not count), and no later than `until`.
fn window(ev: &[Event], k: usize, until: i64, stop: impl Fn(Signal) -> bool) -> Vec<&Read> {
    let mut out = Vec::new();
    for e in &ev[k + 1..] {
        if e.ms() > until {
            break;
        }
        match (e.read(), e.signal()) {
            (Some(r), _) => out.push(r),
            (None, Some(s)) if !s.passive() && stop(s) => break,
            _ => {}
        }
    }
    out
}

/// Whether a game signal with original name `name` falls within [from, to] ms.
fn raw_between(raws: &[(i64, String)], name: &str, from: i64, to: i64) -> bool {
    raws.iter().any(|(t, s)| s == name && *t >= from && *t <= to)
}

/// Reads that stay on the Patrol screen after a move was pressed (stale frames): no `Moved` after, but one within the
/// 1.5 s before.
fn stale_patrol(ev: &[Event], i: usize) -> bool {
    let moved_next = next_signal(ev, i).is_some_and(|k| ev[k].signal() == Some(Signal::Moved) || near(ev, k, Signal::Moved).is_some_and(|j| j > i));
    let t = ev[i].ms();
    !moved_next && ev[..i].iter().rev().take_while(|e| t - e.ms() <= 1_500).any(|e| e.signal() == Some(Signal::Moved))
}

/// Patrol: the first Warden position read twice in a row on the same day after `Moved` that differs from the old one
/// (skipping screens that cover the board).
fn patrol(ev: &[Event], raws: &[(i64, String)], i: usize, m: &Model) -> Result<Action, Why> {
    let k = next_signal(ev, i).ok_or(("no move after the advice", "建議之後沒有動作"))?;
    let k = match ev[k].signal() {
        Some(Signal::Moved) => k,
        Some(Signal::Decision) => near(ev, k, Signal::Moved).ok_or(("the next click is no move", "下一個動作不是移動"))?,
        _ => return Err(("the next click is no move", "下一個動作不是移動")),
    };
    let t0 = ev[k].ms();
    // A `Decision` arriving with `Moved`, events at the landing cell and instrument-trigger signals do not count as the
    // next action; only a cell pick (sewer), a re-roll or the run end does.
    let ends = |t: i64| ["CellPicked", "Reroll", "RunEnd", "RunStart"].iter().any(|n| raw_between(raws, n, t, t));
    let (reads, _) = reads_after(ev, k, |s, t| (s == Signal::Decision && (t - t0).abs() <= SAME_CLICK_MS) || s == Signal::Incident || (s == Signal::Other && !ends(t)));
    // Instrument pick, reward pick and cell pick cover the board, so the Warden position cannot be read reliably:
    // skip those reads entirely.
    let vals: Vec<Option<i64>> = reads
        .iter()
        .filter(|r| !matches!(r.screen.as_str(), "InstrumentPick" | "TreasurePick" | "CellPick"))
        .map(|r| (r.model.day == m.day && r.model.warden != m.warden).then_some(r.model.warden))
        .collect();
    let w = first_stable(&vals).ok_or(("the warden's new cell was not read twice", "典獄長的新位置沒有連續讀到 2 次"))?;
    usize::try_from(w).map(|cell| Action::Patrol { cell }).map_err(|_| ("bad warden cell", "典獄長位置不合理"))
}

/// Manage: consume, hunt, replace or end the day.
fn manage(run: &Run, raws: &[(i64, String)], i: usize, until: i64, m: &Model, cands: &[Action]) -> Result<Action, Why> {
    let ev = &run.events;
    let k = next_signal(ev, i).ok_or(("no move after the advice", "建議之後沒有動作"))?;
    let sig = ev[k].signal();
    let consume = match sig {
        Some(Signal::Consume) => Some(k),
        Some(Signal::Decision) => near(ev, k, Signal::Consume),
        _ => None,
    };
    if let Some(c) = consume {
        return devour_target(ev, c, until, m, cands);
    }
    if sig != Some(Signal::Decision) || [Signal::Moved, Signal::Bite, Signal::ChainEnd].iter().any(|&s| near(ev, k, s).is_some()) {
        return Err(("the next click is unclear", "看不出下一個動作"));
    }
    if let Some(hunt) = hunt(ev, k, until, m, cands) {
        return hunt;
    }
    if replace_at(run, k).is_some() || card_used(ev, k, until, m) {
        return Ok(Action::Swap);
    }
    if day_ended(ev, raws, k, until, m) {
        return Ok(Action::End);
    }
    Err(("the click changed nothing readable", "看不出按了什麼"))
}

/// Fallback way to infer a Replace: `rates` stops a Replace at an immediately following instrument signal (such as
/// the tongs), so here the original-name signals are skipped and we check whether the Manage screen of the same day
/// and cell reads the Replace card as used up twice in a row.
fn card_used(ev: &[Event], k: usize, until: i64, m: &Model) -> bool {
    if m.swap_used {
        return false;
    }
    let reads = window(ev, k, until, |s| s != Signal::Other);
    if reads.iter().any(|r| matches!(r.screen.as_str(), "CellPick" | "Interrogation" | "TreasurePick" | "InstrumentPick" | "ChainTarget")) {
        return false;
    }
    let vals: Vec<Option<bool>> = reads.iter().filter(|r| r.screen == "Manage" && r.model.day == m.day && r.model.warden == m.warden).map(|r| Some(r.model.swap_used)).collect();
    last_stable(&vals).is_some_and(|(v, _)| v)
}

/// The consume target: after the first `Bite` following `Consume`, the one cell among the consume options whose
/// content changed.
fn devour_target(ev: &[Event], c: usize, until: i64, m: &Model, cands: &[Action]) -> Result<Action, Why> {
    let opts: Vec<usize> = cands.iter().filter_map(|a| if let Action::Devour { to } = a { Some(*to) } else { None }).collect();
    if opts.is_empty() {
        return Err(("a Consume with no Consume option", "吃了，但選項裡沒有吃"));
    }
    let bite = ev[c + 1..]
        .iter()
        .position(|e| matches!(e.signal(), Some(Signal::Bite | Signal::Consume | Signal::Moved)))
        .map(|p| c + 1 + p)
        .filter(|&b| ev[b].signal() == Some(Signal::Bite));
    if let Some(b) = bite {
        // From the bite to the next action: vaulting, opening a treasure and other original-name signals do not count
        // (`until` blocks the next run).
        let mut reads = window(ev, b, until, |s| !matches!(s, Signal::Vaulted | Signal::TreasureOpen | Signal::Other));
        // A treasure's reward (opening a door, a tier-up) only takes effect after the pick: look only up to the end
        // of the pick screen.
        if let Some(p) = reads.iter().position(|r| r.screen == "TreasurePick")
            && let Some(q) = reads[p..].iter().position(|r| r.screen != "TreasurePick")
        {
            reads.truncate(p + q);
        }
        let hits: Vec<usize> = opts
            .iter()
            .copied()
            .filter(|&n| {
                let vals: Vec<Option<bool>> = reads.iter().filter(|r| r.model.day == m.day).map(|r| r.model.cells.get(n).map(|x| Some(x) != m.cells.get(n))).collect();
                last_stable(&vals).is_some_and(|(v, _)| v)
            })
            .collect();
        if let [n] = hits[..] {
            return Ok(Action::Devour { to: n });
        }
    }
    if let [n] = opts[..] {
        return Ok(Action::Devour { to: n });
    }
    Err(("the Consume target is unclear", "吃的對象看不出來"))
}

/// The cell the prey mark on the lion points to.
fn prey_cells(m: &Model) -> Vec<i64> {
    m.cells
        .iter()
        .filter_map(|c| match c {
            CellModel::Inmate { beast: Beast::Lion, prey: Some(p), .. } => p.cell,
            _ => None,
        })
        .collect()
}

/// Hunt: a prey pick screen appears after the Decision; the target is the newly appeared prey mark, or the only
/// target if there is just one.
fn hunt(ev: &[Event], k: usize, until: i64, m: &Model, cands: &[Action]) -> Option<Result<Action, Why>> {
    let reads = window(ev, k, until, |s| matches!(s, Signal::Moved | Signal::Consume | Signal::Bite));
    if !reads.iter().any(|r| r.screen == "CellPick" && r.model.choice.kind == ChoiceKind::Prey) {
        return None;
    }
    let opts: Vec<usize> = cands.iter().filter_map(|a| if let Action::Hunt { cell } = a { Some(*cell) } else { None }).collect();
    let pre = prey_cells(m);
    let vals: Vec<Option<i64>> = reads
        .iter()
        .map(|r| {
            let new: Vec<i64> = prey_cells(&r.model).into_iter().filter(|c| !pre.contains(c)).collect();
            if let [c] = new[..] { Some(c) } else { None }
        })
        .collect();
    let target = first_stable(&vals).and_then(|c| usize::try_from(c).ok()).filter(|c| opts.contains(c));
    Some(match (target, &opts[..]) {
        (Some(cell), _) | (None, &[cell]) => Ok(Action::Hunt { cell }),
        _ => Err(("the Hunt target is unclear", "獵殺的對象看不出來")),
    })
}

/// End the day: the next day is read twice in a row afterwards; on the last day, `RunEnd` follows immediately and the
/// Manage screen of the same day never returns.
fn day_ended(ev: &[Event], raws: &[(i64, String)], k: usize, until: i64, m: &Model) -> bool {
    let reads = window(ev, k, until, |s| matches!(s, Signal::Moved | Signal::Consume | Signal::Bite | Signal::Decision | Signal::ChainEnd));
    let vals: Vec<Option<bool>> = reads.iter().map(|r| Some(r.model.day > m.day)).collect();
    if last_stable(&vals).is_some_and(|(v, _)| v) {
        return true;
    }
    let t = ev[k].ms();
    m.day >= m.max_days && raw_between(raws, "RunEnd", t, t + 3_000) && !reads.iter().any(|r| r.screen == "Manage" && r.model.day == m.day)
}

/// Number of event marks on the board.
fn markers(m: &Model) -> usize {
    m.cells.iter().filter(|c| c.event().is_some()).count()
}

/// Tier sum: the inmate tiers in open cells, with each inmate in the vault counted as Chromatic.
fn tier_score(m: &Model) -> i64 {
    let board: i64 = m.cells.iter().filter_map(|c| if let CellModel::Inmate { tier, .. } = c { Some(*tier) } else { None }).sum();
    board + m.vault.len() as i64 * i64::from(RAINBOW)
}

fn card(c: Card) -> Action {
    Action::Choose { id: ChoiceId::Name(c.key().to_string()) }
}

/// Interrogation: a later read on the same day with 1 more mark is Instigate; a vaulting signal, or an unchanged mark
/// count with the tier sum up by 1, is Bestow.
fn interrogation(ev: &[Event], i: usize, until: i64, m: &Model) -> Result<Action, Why> {
    let k = next_signal(ev, i).ok_or(("no move after the advice", "建議之後沒有動作"))?;
    if ev[k].signal() != Some(Signal::Decision) {
        return Err(("the next click is unclear", "看不出下一個動作"));
    }
    let end = ev[k + 1..].iter().position(|e| matches!(e.signal(), Some(Signal::Moved | Signal::Consume | Signal::Bite)) || e.ms() > until).map_or(ev.len(), |p| k + 1 + p);
    // A Bestow raising an Ultimate to Chromatic: vaulted (a later treasure pick changes tiers, so reads are no longer
    // used).
    if ev[k + 1..end].iter().any(|e| e.signal() == Some(Signal::Vaulted)) {
        return Ok(card(Card::Bestow));
    }
    let same: Vec<&Read> = ev[k + 1..end].iter().filter_map(Event::read).filter(|r| r.model.day == m.day && r.screen != "TreasurePick").collect();
    if same.is_empty() {
        return Err(("no read the same day after the card", "選卡之後同一天沒有讀取"));
    }
    // The old board is still shown during the animation after a card pick: look at the change in (mark count, tier
    // sum) of the last 2 consecutive equal reads; with only 1 read, use that one.
    let (mk, sc) = (markers(m) as i64, tier_score(m));
    let vals: Vec<Option<(i64, i64)>> = same.iter().map(|r| Some((markers(&r.model) as i64 - mk, tier_score(&r.model) - sc))).collect();
    let change = last_stable(&vals).map(|(v, _)| v).or(if let [Some(v)] = vals[..] { Some(v) } else { None });
    match change {
        Some((1, 0)) => return Ok(card(Card::Instigate)),
        Some((0, 1)) => return Ok(card(Card::Bestow)),
        _ => {}
    }
    Err(("the card's effect is unclear", "看不出選了哪張卡"))
}

/// Instrument pick: a `Reroll` right after is a re-roll; otherwise see which instrument was added in the next 2
/// consecutive reads.
fn instrument(ev: &[Event], raws: &[(i64, String)], i: usize, until: i64, m: &Model, cands: &[Action]) -> Result<Action, Why> {
    if let Some(k) = next_signal(ev, i) {
        let t = ev[k].ms();
        if ev[k].signal() == Some(Signal::Other) && raw_between(raws, "Reroll", t, t) {
            return Ok(Action::Reroll);
        }
    }
    // Taking an instrument has no signal and the instrument bar is often read only the next day; a move does not change
    // instruments: look until a consume, a bite, another day or the next instrument pick screen (with another screen
    // between) appears (`until` blocks the next run).
    let all = window(ev, i, until, |s| matches!(s, Signal::Consume | Signal::Bite));
    let end = (0..all.len())
        .find(|&j| all[j].model.day > m.day + 1 || (j > 0 && all[j].screen == "InstrumentPick" && all[j - 1].screen != "InstrumentPick"))
        .unwrap_or(all.len());
    let reads = &all[..end];
    let vals: Vec<Option<Tool>> = reads
        .iter()
        .map(|r| {
            let new: Vec<Tool> = r.model.tools.iter().copied().filter(|x| !m.tools.contains(x)).collect();
            if let [x] = new[..] { (r.model.tools.len() == m.tools.len() + 1).then_some(x) } else { None }
        })
        .collect();
    let tool = first_stable(&vals).ok_or(("the instrument taken was not read twice", "拿到的刑具沒有連續讀到 2 次"))?;
    let a = Action::Choose { id: ChoiceId::Name(tool.key().to_string()) };
    if cands.contains(&a) { Ok(a) } else { Err(("the instrument taken was not offered", "拿到的刑具不在選項裡")) }
}

/// Re-solve every decision whose move made can be told (`rollouts` rounds), decisions in parallel.
pub fn score(games: &mut [Game], rollouts: usize) {
    use rayon::prelude::*;
    let mut jobs: Vec<&mut Decision> = games.iter_mut().flat_map(|g| g.decisions.iter_mut()).filter(|d| d.actual.is_ok()).collect();
    jobs.par_iter_mut().for_each(|d| {
        let (st, _) = rebuild(&d.model, d.theme, d.usurped);
        d.ranking = solve(&st, rollouts, SEED, &TUNED);
    });
}

fn screen_name(s: &str) -> &'static str {
    match s {
        "Patrol" => t("Patrol", "巡邏"),
        "Manage" => t("Manage", "管理"),
        "Interrogation" => t("Interrogation", "審訊"),
        "InstrumentPick" => t("Instrument pick", "選刑具"),
        _ => t("other", "其他"),
    }
}

fn why(w: Why) -> &'static str {
    t(w.0, w.1)
}

/// Hours:minutes:seconds of a time.
fn clock(at: &str) -> &str {
    at.get(11..19).unwrap_or(at)
}

fn day_label(d: i64) -> String {
    if lang::zh() { format!("第 {d} 天") } else { format!("day {d}") }
}

/// The report for one run.
fn game_report(o: &mut String, g: &Game, each: bool) {
    let span = match &g.end {
        Some(end) => format!("{} – {}", clock(&g.start), clock(end)),
        None => format!("{} – {}", clock(&g.start), t("not finished", "未結束")),
    };
    let _ = writeln!(o, "\n== {}  {} ==", g.folder, span);
    let expected = g.expected.map_or_else(|| "-".to_string(), |(e, sd)| format!("{e:.2} ± {sd:.2}"));
    let actual = match (g.actual(), g.vaulted) {
        (Some(a), Some(v)) => format!("{a:.2}（{} {v} + {} {:.2}）", t("vaulted", "收容"), t("board", "盤面"), g.board.unwrap_or(0.0)),
        _ => t("unknown (no run_end)", "不明（沒有 run_end）").to_string(),
    };
    let z = g.z().map_or_else(|| "-".to_string(), |z| format!("{z:+.2}"));
    let _ = writeln!(o, "{} {expected}; {} {actual}; z = {z}", t("expected at the start", "開局期望"), t("actual", "實際"));
    let n = g.decisions.len();
    let followed = g.decisions.iter().filter(|d| d.followed() == Some(true)).count();
    let not = g.decisions.iter().filter(|d| d.followed() == Some(false)).count();
    let unknown = g.decisions.iter().filter(|d| d.actual.is_err()).count();
    let unmatched = g.decisions.iter().filter(|d| d.actual.is_ok() && d.advised.is_none()).count();
    let _ = write!(
        o,
        "{} {n}: {} {followed}, {} {not}, {} {unknown}",
        t("decisions", "決策"),
        t("followed", "照建議"),
        t("not followed", "沒照"),
        t("unknown", "不明")
    );
    if unmatched > 0 {
        let _ = write!(o, ", {} {unmatched}", t("advice label not matched", "建議標籤對不上"));
    }
    let _ = writeln!(o, "; {} {}", t("other screens skipped", "其他畫面略過"), g.skipped);
    let (noise, noise_n) = g.noise();
    let losses = g.decisions.iter().filter(|d| matches!(d.verdict(), Verdict::Loss { .. })).count();
    let luck = g.luck().map_or_else(|| "-".to_string(), |l| format!("{l:+.2}"));
    let _ = writeln!(
        o,
        "{} {:.3}（{losses}）; {} {noise:.3}（{noise_n}）; {} {luck}",
        t("decision loss", "決策損失"),
        g.loss(),
        t("within noise", "雜訊內"),
        t("luck = actual - expected + loss", "運氣 = 實際 − 期望 + 損失")
    );
    // The 3 largest gaps (those within noise are marked).
    let mut top: Vec<(&Decision, f64, bool)> = g
        .decisions
        .iter()
        .filter_map(|d| match d.verdict() {
            Verdict::Loss { loss, .. } => Some((d, loss, false)),
            Verdict::Noise { loss, .. } if loss > 0.0 => Some((d, loss, true)),
            _ => None,
        })
        .collect();
    top.sort_by(|a, b| b.1.total_cmp(&a.1));
    if !top.is_empty() {
        let _ = writeln!(o, "{}", t("largest losses:", "最大損失："));
    }
    for (d, loss, noisy) in top.iter().take(3) {
        let actual = d.actual.as_ref().map(|a| d.label(a)).unwrap_or_default();
        let best = d.best().map(|b| d.label(&b.action)).unwrap_or_default();
        let _ = writeln!(
            o,
            "  {} {} {}: {} 「{actual}」, {} 「{best}」, {} {loss:.3}{}",
            day_label(d.day),
            clock(&d.read_at),
            screen_name(&d.screen),
            t("made", "實際"),
            t("best", "最佳"),
            t("loss", "損失"),
            if *noisy { t(" (within noise)", "（雜訊內）") } else { "" }
        );
    }
    if each {
        for d in &g.decisions {
            let _ = writeln!(o, "  {}", decision_line(d));
        }
    }
}

/// One line per decision (`--each`).
fn decision_line(d: &Decision) -> String {
    let head = format!("{} {} {}", pad(&day_label(d.day), 6), clock(&d.read_at), pad(screen_name(&d.screen), width(t("Instrument pick", "選刑具"))));
    let follow = match d.followed() {
        Some(true) => t("followed", "照建議"),
        Some(false) => t("not followed", "沒照"),
        None => "?",
    };
    let actual = match &d.actual {
        Ok(a) => d.label(a),
        Err(w) => return format!("{head} {} ({}); {} 「{}」", t("unknown", "不明"), why(*w), t("advice", "建議"), d.pick),
    };
    let best = d.best().map(|b| (d.label(&b.action), b.mean));
    let mean = |a: &Action| d.ranking.iter().find(|o| &o.action == a).map(|o| o.mean);
    let tail = match d.verdict() {
        Verdict::Best => format!("{} 0", t("loss", "損失")),
        Verdict::Noise { loss, gap_se } => format!("{} {loss:.3} ± {gap_se:.3} {}", t("loss", "損失"), t("(noise)", "（雜訊）")),
        Verdict::Loss { loss, gap_se } => format!("{} {loss:.3} ± {gap_se:.3}", t("loss", "損失")),
        Verdict::Unsolved(w) | Verdict::Unknown(w) => why(w).to_string(),
    };
    let best = best.map_or_else(String::new, |(l, m)| format!("; {} 「{l}」 {m:.3}", t("best", "最佳")));
    let mine = d.actual.as_ref().ok().and_then(mean).map_or_else(String::new, |m| format!(" {m:.3}"));
    format!("{head} {follow}: {} 「{actual}」{mine}{best}; {tail}", t("made", "實際"))
}

/// All reports; with several runs, a summary table is added.
pub fn report(games: &[Game], rollouts: usize, each: bool) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{} {}, {} {rollouts}", t("runs", "局數"), games.len(), t("playouts per option", "每個選項重算輪數"));
    for g in games {
        game_report(&mut o, g, each);
    }
    if games.len() > 1 {
        let _ = writeln!(o, "\n== {} ==", t("summary", "總表"));
        let name_w = games.iter().map(|g| width(&g.folder)).max().unwrap_or(0).max(width(t("run", "局")));
        let _ = writeln!(o, "{}  {:>6}  {:>5}  {:>6}  {:>6}  {:>6}  {:>6}", pad(t("run", "局"), name_w), "exp", "sd", "actual", "z", "loss", "luck");
        let (mut z2, mut nz) = (0.0, 0);
        for g in games {
            let f = |x: Option<f64>, p: usize| x.map_or_else(|| "-".to_string(), |v| format!("{v:.p$}"));
            let _ = writeln!(
                o,
                "{}  {:>6}  {:>5}  {:>6}  {:>6}  {:>6.3}  {:>6}",
                pad(&g.folder, name_w),
                f(g.expected.map(|e| e.0), 2),
                f(g.expected.map(|e| e.1), 2),
                f(g.actual(), 2),
                g.z().map_or_else(|| "-".to_string(), |z| format!("{z:+.2}")),
                g.loss(),
                g.luck().map_or_else(|| "-".to_string(), |l| format!("{l:+.2}"))
            );
            if let Some(z) = g.z() {
                z2 += z * z;
                nz += 1;
            }
        }
        let _ = writeln!(
            o,
            "{} {z2:.2}, {} {nz}{}",
            t("sum of z^2", "z² 總和"),
            t("runs", "局數"),
            t(" (with a well-sized spread the sum is about the number of runs)", "（模型散布合適時，z² 總和約等於局數）")
        );
    }
    let _ = writeln!(
        o,
        "\n{}",
        t(
            "Expected: the first advice row's best_ev and sd (the session's full model). Actual: run_end's vaulted plus the last board's loot and objective cache (State::loot). Loss: best mean minus the mean of the move made, re-solved on the logged board with the run's theme and Usurpation weights; within 2 gap_se of it is noise, listed apart and not summed. Only Patrol, Manage, Interrogation and Instrument pick are re-solved; a move that cannot be told from the log is unknown and not counted.",
            "期望：一局第一個建議列的 best_ev 與 sd（session 的完整模型）。實際：run_end 的收容數加上最後讀盤的盤面價值與目標獎勵（State::loot）。損失：在紀錄的盤面上重算（帶入本局主題與篡位權重），最佳平均減實際所選平均；在 2 倍 gap_se 內算雜訊，另列、不加總。只重算巡邏、管理、審訊、選刑具；從紀錄看不出實際所選的標「不明」，不計。"
        )
    );
    o
}
