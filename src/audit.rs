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
//!     one (skipping screens that cover the board, such as instrument pick, reward pick and cell pick); without such a
//!     pair, the advice's move when the player clicked its mark and the game logged `Moved` within `SAME_CLICK_MS`
//!     (the view row's `clicked`), else the one position read; stale reads with no `Moved` after them do not count as
//!     a decision.
//!   - Manage: after the first `Bite` following `Consume`, the one cell among the consume options whose content
//!     changed; a lone `Decision` followed by a prey pick is a Hunt (the target is the new prey mark); a Replace that
//!     `rates` recognizes, or the Replace card read as used up twice in a row, or with the Severed Iron Maiden held an
//!     `Instrument` signal with the `Decision` and the Warden's cell changed, is a Replace; the day number going up by
//!     1 (on the last day, `RunEnd` right after) is End.
//!   - Interrogation: a later read on the same day with 1 more mark is Instigate; a signal that an inmate was vaulted,
//!     or an unchanged mark count with the tier sum up by 1, is Bestow.
//!   - Instrument pick: a `Reroll` right after is a re-roll; otherwise see which instrument was added in the later
//!     reads (up to the next day).
//! - Decision loss = best option's mean - mean of the move made; a gap within 2x gap_se of the move made counts as
//!   noise, listed separately and not included in the loss total. The means are the solver's as the advice ranks
//!   them. The rows of the 2026-11-09 afternoon builds carry an instrument pick's Blacklist correction in `ev` and
//!   `best_ev` (`tool_bias`, since removed) and the playouts' own means in `ev_raw` and `best_ev_raw`.
//! - The expected value at the start takes best_ev and sd from the first advice row of a run that has a round count
//!   (the session's full model; best_ev_raw instead when the row has one, the playouts' own prediction); actual = the vaulted count from `run_end` + the last read board's value and objective
//!   reward (the `State::loot` scoring). Luck = actual - expected + loss total.
//! - The expected value is a floor: the playouts follow the plain strategy, the player the advice, which gains a little
//!   at every step, so the actual comes out above it on average (the calibration of 2026-11-09). Beside luck and z,
//!   drift-corrected ones take that out: [`DRIFT_AT_START`] and [`SPREAD_AT_START`], times the share of the run's days
//!   still to come at the first advice row (`luck_net`, `z_net`).

use crate::app::latency::log_millis;
use crate::board::{CellModel, ChoiceKind, Model, build_state};
use crate::engine::data::{Beast, Card, LOOT, RAINBOW, Tool};
use crate::engine::incidents::{USURPATION_I, usurped};
use crate::engine::{Action, ChoiceId, State};
use crate::lang::{self, Lang};
use crate::rates::{Event, Read, Run, SAME_CLICK_MS, Signal, last_stable, pad, parse_run, reads_after, replace_at, t, width};
use crate::solver::TUNED;
use crate::solver::solve::{SolveOption, candidates, solve, solve_with};
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
    /// `ev` and `best_ev` without an instrument pick's Blacklist correction: the rows of the 2026-11-09 afternoon
    /// builds have them (`tool_bias`, since removed), written
    /// only when it was not 0: the playouts' own prediction, which the expected value compares with the run.
    #[serde(default)]
    ev_raw: Option<f64>,
    #[serde(default)]
    best_ev_raw: Option<f64>,
    #[serde(default)]
    sd: Option<f64>,
    #[serde(default)]
    vaulted: Option<u32>,
    /// The playouts' version (`solver::PLAYOUT_VERSION`); rows before the field have none (version 1).
    #[serde(default)]
    playout: Option<u32>,
    /// A view row's advice on show and why its mark is hidden (`clicked`: the player clicked the mark).
    #[serde(default)]
    advice: Option<String>,
    #[serde(default)]
    hidden: Option<String>,
}

/// One advice row.
struct Advice {
    read_at: String,
    pick: String,
    rounds: u64,
    ev: Option<f64>,
    best_ev: Option<f64>,
    sd: Option<f64>,
    playout: Option<u32>,
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

/// The upward drift of a run's actual over the expected value of its first advice row, at day 1 (times the share of
/// the days still to come there), actual - expected fitted on that share through 0. Fitted again on 2026-11-09 evening
/// for the redesigned playouts (the Blacklist potential, the Hunt walk, TOOL_RANK measured again, `w_ahead`): the start
/// boards of 200 runs of 2026-10-30..11-09 (all but 3 began at day 1) solved again with them, whose expected values
/// came out 0.91 above the logged ones on average; 2.04 against the logged values, 1.08 against the new ones (1.10
/// and 1.05 on either half). Runs logged before that build carry the old playouts' expected values, about 0.9 lower,
/// so their luck reads about that much low unless they take [`DRIFT_AT_START_V1`]: an advice row of these playouts
/// names their version (`playout` 2, `solver::PLAYOUT_VERSION`), the rows before have none. See [`Game::drift_at_start`].
pub const DRIFT_AT_START: f64 = 1.08;

/// [`DRIFT_AT_START`] for the runs logged before the playouts' version was written (version 1, no `playout` field),
/// against the expected values they logged: 2.04 on the same 200 runs of the 2026-11-09 evening refit (the first fit,
/// on 192 runs of 2026-10-30..11-08, gave 1.96).
pub const DRIFT_AT_START_V1: f64 = 2.04;

/// The spread a run's actual has beyond the playouts' sd at day 1 (times the same share), added in quadrature: fitted so
/// the mean of `z_net`^2 over the runs is 1. On the 192 runs of 2026-10-30..11-08: sum of z^2 654 -> 192, |z| > 2 48 ->
/// 6 runs (9 expected at 4.6%). Unchanged by the refit of [`DRIFT_AT_START`] (2.16 again; |z_net| > 2 6 of 200 runs;
/// fitted on half of the runs and checked on the other half, mean z^2 0.75 and 1.44).
pub const SPREAD_AT_START: f64 = 2.16;

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
    /// The share of the run's days still to come at the advice row `expected` comes from (1 at day 1); None when its
    /// read is not in the log.
    pub start_left: Option<f64>,
    /// The playouts' version of that advice row (`solver::PLAYOUT_VERSION`); None for the rows before the field.
    pub playout: Option<u32>,
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

    /// The drift at day 1 for the playouts that solved the run: [`DRIFT_AT_START`] for version 2 and later (a later
    /// version takes the latest fit until it has its own), [`DRIFT_AT_START_V1`] for the rows without a version.
    pub fn drift_at_start(&self) -> f64 {
        match self.playout {
            Some(v) if v >= 2 => DRIFT_AT_START,
            _ => DRIFT_AT_START_V1,
        }
    }

    /// The drift expected above the start's expected value ([`Game::drift_at_start`] times the share of days to come).
    pub fn drift(&self) -> Option<f64> {
        Some(self.drift_at_start() * self.start_left?)
    }

    /// [`Game::luck`] less the drift: what is luck once the advice's usual gain over the playouts is taken out.
    pub fn luck_net(&self) -> Option<f64> {
        Some(self.luck()? - self.drift()?)
    }

    /// The single-run sd widened by [`SPREAD_AT_START`] (times the share of days to come), what [`Game::z_net`] divides by.
    pub fn spread_net(&self) -> Option<f64> {
        let ((_, sd), left) = (self.expected?, self.start_left?);
        Some((sd * sd + (SPREAD_AT_START * left).powi(2)).sqrt())
    }

    /// [`Game::z`] less the drift, over the spread widened by [`SPREAD_AT_START`] (times the share of days to come).
    pub fn z_net(&self) -> Option<f64> {
        let ((e, _), actual, drift, spread) = (self.expected?, self.actual()?, self.drift()?, self.spread_net()?);
        (spread > 0.0).then(|| (actual - e - drift) / spread)
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
    let mut clicks: Vec<(i64, String)> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut closed = true;
    for line in text.lines() {
        let click = line.contains("\"hidden\":\"clicked\"");
        if !click && !line.contains("\"kind\":\"advice\"") && !line.contains("\"kind\":\"game\"") && !line.contains("\"kind\":\"run_end\"") {
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
            "view" => {
                if let (Some("clicked"), Some(advice)) = (r.hidden.as_deref(), r.advice) {
                    clicks.push((log_millis(&r.at).unwrap_or(0), advice));
                }
            }
            "advice" => {
                if closed {
                    spans.push(Span::new(&r.at));
                    closed = false;
                }
                let (Some(read_at), Some(pick)) = (r.read_at, r.pick) else { continue };
                if let Some(sp) = spans.last_mut() {
                    sp.advice.push(Advice { read_at, pick, rounds: r.rounds.unwrap_or(0), ev: r.ev_raw.or(r.ev), best_ev: r.best_ev_raw.or(r.best_ev), sd: r.sd, playout: r.playout });
                }
            }
            _ => {}
        }
    }
    let index: HashMap<&str, usize> = run.events.iter().enumerate().filter_map(|(i, e)| e.read().map(|r| (r.at.as_str(), i))).collect();
    spans.into_iter().filter(|sp| !sp.advice.is_empty()).map(|sp| game(&run, &raws, &clicks, &index, sp)).collect()
}

fn game(run: &Run, raws: &[(i64, String)], clicks: &[(i64, String)], index: &HashMap<&str, usize>, sp: Span) -> Game {
    let ev = &run.events;
    let from = log_millis(&sp.start).unwrap_or(i64::MIN);
    let until = sp.end.as_deref().and_then(log_millis).unwrap_or(i64::MAX);
    // Advice rows with a round count (rounds 0 is a single option; before the 08:47 build ev was recorded as 0).
    let usable: Vec<&Advice> = sp.advice.iter().filter(|a| a.rounds > 0 && a.ev.is_some() && a.best_ev.is_some()).collect();
    let first = usable.iter().find(|a| a.best_ev.is_some() && a.sd.is_some());
    let expected = first.and_then(|a| Some((a.best_ev?, a.sd?)));
    let playout = first.and_then(|a| a.playout);
    // the share of the days to come at that advice row, from the read it was solved on
    let start_left = first.and_then(|a| index.get(a.read_at.as_str())).and_then(|&i| ev[i].read()).and_then(|r| {
        let m = &r.model;
        (m.max_days > 0).then(|| (m.max_days - m.day).clamp(0, m.max_days) as f64 / m.max_days as f64)
    });
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
        decisions.push(decide(run, raws, clicks, i, from, until, a));
    }
    // End-of-run board: the board value that was the same in the last 2 consecutive reads; if none, take the last read.
    let board_vals: Vec<Option<f64>> = ev.iter().filter_map(Event::read).filter(|r| in_span(r, from, until)).map(|r| Some(board_loot(&r.model))).collect();
    let board = last_stable(&board_vals).map(|(v, _)| v).or_else(|| board_vals.last().copied().flatten());
    Game { folder: run.name.clone(), start: sp.start, end: sp.end, expected, start_left, playout, vaulted: sp.vaulted, board, decisions, skipped }
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
fn decide(run: &Run, raws: &[(i64, String)], clicks: &[(i64, String)], i: usize, from: i64, until: i64, a: &Advice) -> Decision {
    let r = run.events[i].read().expect("read");
    let m = &r.model;
    let theme = theme_id(r.theme.as_deref());
    // The stage the read row holds (the popup read, since 2026-11-11) or the game's log named, whichever is later.
    let usurped = usurped_by(raws, from, log_millis(&r.at).unwrap_or(i64::MAX)).max(r.usurped);
    let (st, problems) = rebuild(m, theme, usurped);
    let cands: Vec<Action> = candidates(&st).into_iter().map(|c| c.action).collect();
    let advised = advised(&st, &a.pick);
    // When the player clicked this advice's mark, after the read and within the run.
    let clicked: Vec<i64> = clicks.iter().filter(|(t, pick)| *t >= run.events[i].ms() && *t <= until && *pick == a.pick).map(|(t, _)| *t).collect();
    let actual = if !problems.is_empty() {
        Err(("the board has problems", "盤面有問題"))
    } else {
        match r.screen.as_str() {
            "Patrol" => patrol(&run.events, raws, i, m, advised.clone().filter(|_| !clicked.is_empty()), &clicked),
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
        advised,
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

/// The first value read twice in a row ([`first_stable`]), or else the one value read when every read that has one
/// agrees: the screen after a move is often covered (an instrument or treasure pick, the next Manage advice) after a
/// single read of the new position, and that read was right in every such case checked (2026-11-08_18-24-48 and
/// 18-55-08, five Patrol moves: each single read named the advised cell). A move that is not among the options is
/// still dropped by the caller.
fn agreed<T: PartialEq + Clone>(vals: &[Option<T>]) -> Option<T> {
    first_stable(vals).or_else(|| {
        let mut seen = vals.iter().flatten();
        let first = seen.next()?;
        seen.all(|v| v == first).then(|| first.clone())
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
/// (skipping screens that cover the board); without such a pair, the advice's move when the player clicked its mark
/// (`clicked`, the times of the view rows) and the game logged the move within [`SAME_CLICK_MS`] after; else the one
/// position read ([`agreed`]).
///
/// A mark is hidden as `clicked` on a click on its target or the box around it (`mark::on_mark`), and the game logs
/// the move it plays 0.02 to 0.12 s after the click. A single read after the move can be wrong: on 2026-11-11
/// 19:16:57 the player clicked "Go to cell 14 (a Prison Incident marker)", the move completed the objective, the
/// board was read once mid-animation with the Warden on cell 15 and the instrument pick that followed covered it, so
/// cell 15 was taken, a false loss of 2.4. The click is not certain either, so a position read twice wins over it:
/// at 19:16:41 of the same run "Go to cell 08" was clicked, and the Warden was read on cell 06 five times, with the
/// next advice "Replace the Normal Berserk Lion in 06" (a click on the box's edge over the next cell).
fn patrol(ev: &[Event], raws: &[(i64, String)], i: usize, m: &Model, advised: Option<Action>, clicked: &[i64]) -> Result<Action, Why> {
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
    if first_stable(&vals).is_none()
        && let Some(go @ Action::Patrol { .. }) = advised
        && clicked.iter().any(|&c| c <= t0 && t0 - c <= SAME_CLICK_MS)
    {
        return Ok(go);
    }
    let w = agreed(&vals).ok_or(("the warden's new cell was not read, or read differently", "典獄長的新位置沒有讀到，或讀到的不一致"))?;
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
    if replace_at(run, k).is_some() || card_used(ev, k, until, m) || maiden_replace(ev, raws, k, until, m) {
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

/// A Replace with the Severed Iron Maiden held: the game logs `Instrument` with the `Decision` (the Maiden raising the
/// new inmate), which ends [`replace_at`]'s look at the reads, and the Replace card is not read as used, so the day
/// going up made it End (2026-11-11 17:53:29 and 17:57:10, a false loss of 0.38 and 0.46 each, both clicked on the
/// "Replace" advice). It counts when the Warden's cell, on the same day and screen as Manage or a passing one, is read
/// changed twice in a row before the next action; an End with an instrument triggering leaves the cell as it was.
fn maiden_replace(ev: &[Event], raws: &[(i64, String)], k: usize, until: i64, m: &Model) -> bool {
    let t = ev[k].ms();
    if m.swap_used || !m.tools.contains(&Tool::Maiden) || !raw_between(raws, "Instrument", t, t + SAME_CLICK_MS) {
        return false;
    }
    let Ok(w) = usize::try_from(m.warden) else { return false };
    let reads = window(ev, k, until, |s| s != Signal::Other);
    let vals: Vec<Option<bool>> = reads
        .iter()
        .filter(|r| matches!(r.screen.as_str(), "Manage" | "Unknown") && r.model.day == m.day && r.model.warden == m.warden)
        .map(|r| r.model.cells.get(w).map(|c| Some(c) != m.cells.get(w)))
        .collect();
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
    let target = agreed(&vals).and_then(|c| usize::try_from(c).ok()).filter(|c| opts.contains(c));
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

/// [`score`], then [`top_up`]: the re-solve the run review and `ascendant-cli audit` use.
pub fn score_topped(games: &mut [Game], rollouts: usize) -> usize {
    score(games, rollouts);
    top_up(games, rollouts)
}

/// Whether a scored decision needs the top-up: the move made is not the re-solve's best and the racing dropped it
/// before the full round count.
fn needs_top_up(d: &Decision, rollouts: usize) -> bool {
    let Ok(actual) = &d.actual else { return false };
    let Some(chosen) = d.ranking.iter().find(|o| &o.action == actual) else { return false };
    d.ranking.len() > 1 && &d.ranking[0].action != actual && chosen.n < rollouts
}

/// Re-solves, for every scored decision whose move made trailed and was dropped early by the racing, the move made
/// and the best move together on the same futures to the full round count (no racing), and puts those values into the
/// ranking. A dropped move keeps the values of the checkpoint that dropped it, and its gap there is past the racing's
/// 2.2 paired standard errors by construction, so the audit (2 gap_se) judged every such move a loss of that
/// checkpoint's noise: on 25 runs the losses it reported summed to 1.74 times what 6400-round solves without racing
/// measured on the same decisions, and on Replace-or-End-the-day decisions 2.7 times (2026-11-10). With the top-up the
/// audit matches re-solving every option without racing, at about 1.17 times the time of the plain re-solve.
/// Returns how many decisions were topped up. Decisions in parallel on the caller's rayon pool.
pub fn top_up(games: &mut [Game], rollouts: usize) -> usize {
    use rayon::prelude::*;
    let mut jobs: Vec<&mut Decision> = games.iter_mut().flat_map(|g| g.decisions.iter_mut()).filter(|d| needs_top_up(d, rollouts)).collect();
    let n = jobs.len();
    jobs.par_iter_mut().for_each(|d| {
        let Ok(actual) = d.actual.clone() else { return };
        let best = d.ranking[0].action.clone();
        let (st, _) = rebuild(&d.model, d.theme, d.usurped);
        let pair = solve_with(&st, rollouts, SEED, &TUNED, Some(&[best, actual]), false);
        merge_pair(&mut d.ranking, &pair, rollouts);
    });
    n
}

/// Puts the values of `pair` (two options re-solved together, the leader first) into `ranking`: the two options take
/// their new mean, se, best and round count, and the ranking is ordered again as `solve` orders it, the options that
/// played every round first (the pair, and any that was never dropped), then by mean, so an option the racing froze
/// cannot come out ahead on its checkpoint's noise. The leader's gap_se is 0 and the other's is their paired one; the
/// other options keep their values and their gap_se against the former leader.
fn merge_pair(ranking: &mut [SolveOption], pair: &[SolveOption], rollouts: usize) {
    let [lead, other] = pair else { return };
    for o in ranking.iter_mut() {
        let Some(p) = pair.iter().find(|p| p.action == o.action) else { continue };
        (o.mean, o.se, o.best, o.n) = (p.mean, p.se, p.best, p.n);
        o.gap_se = if o.action == lead.action { 0.0 } else { other.gap_se };
    }
    ranking.sort_by(|a, b| (b.n >= rollouts).cmp(&(a.n >= rollouts)).then(b.mean.partial_cmp(&a.mean).unwrap_or(std::cmp::Ordering::Equal)));
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
    if let (Some(drift), Some(z_net)) = (g.drift(), g.z_net()) {
        let _ = writeln!(o, "{} {drift:+.2}: z = {z_net:+.2}", t("less the usual drift", "扣掉一般漂移"));
    }
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
    let luck = match g.luck_net() {
        Some(net) => format!("{luck}（{} {net:+.2}）", t("less drift", "扣漂移")),
        None => luck,
    };
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
        let _ = writeln!(o, "{}  {:>6}  {:>5}  {:>6}  {:>6}  {:>6}  {:>6}  {:>6}  {:>6}", pad(t("run", "局"), name_w), "exp", "sd", "actual", "z", "loss", "luck", "z_net", "luck_n");
        let (mut z2, mut nz, mut z2_net, mut n_net) = (0.0, 0, 0.0, 0);
        for g in games {
            let f = |x: Option<f64>, p: usize| x.map_or_else(|| "-".to_string(), |v| format!("{v:.p$}"));
            let _ = writeln!(
                o,
                "{}  {:>6}  {:>5}  {:>6}  {:>6}  {:>6.3}  {:>6}  {:>6}  {:>6}",
                pad(&g.folder, name_w),
                f(g.expected.map(|e| e.0), 2),
                f(g.expected.map(|e| e.1), 2),
                f(g.actual(), 2),
                g.z().map_or_else(|| "-".to_string(), |z| format!("{z:+.2}")),
                g.loss(),
                g.luck().map_or_else(|| "-".to_string(), |l| format!("{l:+.2}")),
                g.z_net().map_or_else(|| "-".to_string(), |z| format!("{z:+.2}")),
                g.luck_net().map_or_else(|| "-".to_string(), |l| format!("{l:+.2}"))
            );
            if let Some(z) = g.z() {
                z2 += z * z;
                nz += 1;
            }
            if let Some(z) = g.z_net() {
                z2_net += z * z;
                n_net += 1;
            }
        }
        let _ = writeln!(
            o,
            "{} {z2:.2}, {} {nz}{}",
            t("sum of z^2", "z² 總和"),
            t("runs", "局數"),
            t(" (with a well-sized spread the sum is about the number of runs)", "（模型散布合適時，z² 總和約等於局數）")
        );
        let _ = writeln!(o, "{} {z2_net:.2}, {} {n_net}", t("sum of z_net^2 (less the usual drift)", "扣漂移後的 z² 總和"), t("runs", "局數"));
    }
    let _ = writeln!(
        o,
        "\n{}",
        t(
            "Expected: the first advice row's best_ev and sd (the session's full model). Actual: run_end's vaulted plus the last board's loot and objective cache (State::loot). Loss: best mean minus the mean of the move made, re-solved on the logged board with the run's theme and Usurpation weights; within 2 gap_se of it is noise, listed apart and not summed. Only Patrol, Manage, Interrogation and Instrument pick are re-solved; a move that cannot be told from the log is unknown and not counted.",
            "期望：一局第一個建議列的 best_ev 與 sd（session 的完整模型）。實際：run_end 的收容數加上最後讀盤的盤面價值與目標獎勵（State::loot）。損失：在紀錄的盤面上重算（帶入本局主題與僭越權重），最佳平均減實際所選平均；在 2 倍 gap_se 內算雜訊，另列、不加總。只重算巡邏、管理、審訊、選刑具；從紀錄看不出實際所選的標「不明」，不計。"
        )
    );
    o
}

#[cfg(test)]
mod tests {
    use super::{Decision, SolveOption, Verdict, agreed};
    use crate::engine::Action;

    /// A value read twice in a row wins; else one value read alone, or several reads that all agree, is taken; reads
    /// that disagree with no two in a row, or no read at all, give nothing.
    #[test]
    fn a_position_read_once_or_in_agreement_is_taken() {
        assert_eq!(agreed(&[Some(3), Some(4), Some(4)]), Some(4), "twice in a row");
        assert_eq!(agreed(&[None, Some(13), None]), Some(13), "read once, the screen covered after");
        assert_eq!(agreed(&[Some(7), None, Some(7)]), Some(7), "agreeing reads apart");
        assert_eq!(agreed(&[Some(3), None, Some(5)]), None, "disagreeing reads");
        assert_eq!(agreed::<i64>(&[None, None]), None);
        assert_eq!(agreed::<i64>(&[]), None);
    }

    /// Luck and z less the usual drift: the drift and the wider spread scale with the share of the days to come at the
    /// first advice row; a run whose start is not known has none; the original luck and z stay as they were.
    #[test]
    fn drift_corrected_luck_and_z() {
        let game = |left: Option<f64>| super::Game {
            folder: "t".into(),
            start: String::new(),
            end: None,
            expected: Some((6.0, 2.0)),
            start_left: left,
            playout: Some(crate::solver::PLAYOUT_VERSION),
            vaulted: Some(10),
            board: Some(0.0),
            decisions: Vec::new(),
            skipped: 0,
        };
        let g = game(Some(1.0));
        assert_eq!((g.z(), g.luck()), (Some(2.0), Some(4.0)), "the original fields unchanged");
        assert!((g.luck_net().unwrap() - (4.0 - super::DRIFT_AT_START)).abs() < 1e-12);
        let spread = (4.0 + super::SPREAD_AT_START * super::SPREAD_AT_START).sqrt();
        assert!((g.z_net().unwrap() - (4.0 - super::DRIFT_AT_START) / spread).abs() < 1e-12);
        // half the run to come: half the drift and of the extra spread
        let half = game(Some(0.5));
        assert!((half.drift().unwrap() - super::DRIFT_AT_START / 2.0).abs() < 1e-12);
        let spread = (4.0 + (super::SPREAD_AT_START / 2.0).powi(2)).sqrt();
        assert!((half.z_net().unwrap() - (4.0 - super::DRIFT_AT_START / 2.0) / spread).abs() < 1e-12);
        let unknown = game(None);
        assert_eq!((unknown.z_net(), unknown.luck_net(), unknown.z()), (None, None, Some(2.0)));
        // a run logged before the playouts' version: the old playouts' drift
        let old = super::Game { playout: None, ..game(Some(1.0)) };
        assert!((old.luck_net().unwrap() - (4.0 - super::DRIFT_AT_START_V1)).abs() < 1e-12);
        assert!((old.z_net().unwrap() - (4.0 - super::DRIFT_AT_START_V1) / spread_one()).abs() < 1e-12);
    }

    fn option(action: Action, mean: f64, gap_se: f64, n: usize) -> SolveOption {
        SolveOption { label: format!("{action:?}"), action, mean, se: 0.05, gap_se, n, best: 1.0, policy: false }
    }

    /// A patrol decision from a hand-made log (the Warden moved from the Interrogation Room to cell 04), whose ranking
    /// the tests set by hand.
    fn patrol_decision() -> Decision {
        let read = |at: &str, screen: &str, warden: i64, step: &str| {
            format!(
                "{{\"at\":\"2026-11-03T10:{at}Z\",\"kind\":\"read\",\"screen\":\"{screen}\",\"theme\":null,\"model\":{{\"v\":1,\"day\":2,\"maxDays\":20,\"step\":\"{step}\",\"warden\":{warden},\"dest\":[3,5],\"devourUsed\":false,\"swapUsed\":false,\"huntUsed\":true,\"eventKind\":\"chain\",\"choice\":{{\"kind\":\"tool\",\"options\":[]}},\"chainArmed\":false,\"tools\":[],\"vault\":[],\"cells\":[{{\"kind\":\"office\"}}{}]}}}}",
                ",{\"kind\":\"sealed\",\"event\":null}".repeat(15)
            )
        };
        let log = [
            "{\"at\":\"2026-11-03T10:00:00.500Z\",\"kind\":\"game\",\"signal\":\"RunStart\"}".to_string(),
            read("00:01.000", "Patrol", 0, "patrol"),
            "{\"at\":\"2026-11-03T10:00:01.100Z\",\"kind\":\"advice\",\"pick\":\"Go to cell 04 (a locked door)\",\"read_at\":\"2026-11-03T10:00:01.000Z\",\"rounds\":100,\"ev\":6.0,\"best_ev\":6.0,\"sd\":2.0}".to_string(),
            "{\"at\":\"2026-11-03T10:00:02.000Z\",\"kind\":\"game\",\"signal\":\"Moved\"}".to_string(),
            read("00:02.500", "Manage", 3, "manage"),
            read("00:02.600", "Manage", 3, "manage"),
            "{\"at\":\"2026-11-03T10:00:05.000Z\",\"kind\":\"run_end\",\"vaulted\":1}".to_string(),
        ];
        let mut g = super::games("t", &log.join("\n"));
        let d = g.remove(0).decisions.remove(0);
        assert_eq!(d.actual, Ok(Action::Patrol { cell: 3 }));
        d
    }

    /// A hand-made run: a read row (day, screen, Warden, tools, the inmate in cell 04 as JSON or a sealed cell), with
    /// the other rows given as is.
    fn hand_run(rows: &[String]) -> Vec<Decision> {
        super::games("t", &rows.join("\n")).remove(0).decisions
    }

    fn hand_read(at: &str, screen: &str, day: i64, warden: i64, tools: &str, cell4: &str) -> String {
        let step = if screen == "Manage" { "manage" } else { "patrol" };
        let rest = ",{\"kind\":\"sealed\",\"event\":null}".repeat(11);
        format!(
            "{{\"at\":\"2026-11-03T10:{at}Z\",\"kind\":\"read\",\"screen\":\"{screen}\",\"theme\":null,\"model\":{{\"v\":1,\"day\":{day},\"maxDays\":20,\"step\":\"{step}\",\"warden\":{warden},\"dest\":[3,5],\"devourUsed\":true,\"swapUsed\":false,\"huntUsed\":true,\"eventKind\":\"chain\",\"choice\":{{\"kind\":\"tool\",\"options\":[]}},\"chainArmed\":false,\"tools\":[{tools}],\"vault\":[],\"cells\":[{{\"kind\":\"office\"}},{{\"kind\":\"sealed\",\"event\":null}},{{\"kind\":\"sealed\",\"event\":null}},{cell4},{{\"kind\":\"sealed\",\"event\":null}}{rest}]}}}}"
        )
    }

    fn hand_row(at: &str, rest: &str) -> String {
        format!("{{\"at\":\"2026-11-03T10:{at}Z\",{rest}}}")
    }

    /// A clicked Patrol advice is the move made when the board read the new cell only once (2026-11-11 19:16:57: the
    /// move to an incident marker completed the objective, the Warden was read once on the next cell mid-animation and
    /// the instrument pick covered the board). Without the click, with the click on other advice, or with the new cell
    /// read twice in a row, the board decides.
    #[test]
    fn a_clicked_move_is_the_advice() {
        let sealed = "{\"kind\":\"sealed\",\"event\":null}";
        let run = |click: Option<&str>, reads: usize| {
            let mut rows = vec![
                hand_row("00:00.500", "\"kind\":\"game\",\"signal\":\"RunStart\""),
                hand_read("00:01.000", "Patrol", 2, 0, "", sealed),
                hand_row("00:01.100", "\"kind\":\"advice\",\"pick\":\"Go to cell 04 (a locked door)\",\"read_at\":\"2026-11-03T10:00:01.000Z\",\"rounds\":100,\"ev\":6.0,\"best_ev\":6.0,\"sd\":2.0"),
            ];
            if let Some(advice) = click {
                rows.push(hand_row("00:01.950", &format!("\"kind\":\"view\",\"advice\":\"{advice}\",\"hidden\":\"clicked\"")));
            }
            rows.extend([
                hand_row("00:02.000", "\"kind\":\"game\",\"signal\":\"Moved\""),
                hand_row("00:02.100", "\"kind\":\"game\",\"signal\":\"ObjectiveDone\""),
                hand_read("00:02.500", "Unknown", 2, 5, "", sealed),
                hand_read("00:02.550", "InstrumentPick", 2, 5, "", sealed),
            ]);
            if reads == 2 {
                rows.push(hand_read("00:02.600", "Unknown", 2, 5, "", sealed));
            }
            rows.push(hand_row("00:05.000", "\"kind\":\"run_end\",\"vaulted\":1"));
            hand_run(&rows).remove(0).actual
        };
        assert_eq!(run(Some("Go to cell 04 (a locked door)"), 1), Ok(Action::Patrol { cell: 3 }));
        assert_eq!(run(None, 1), Ok(Action::Patrol { cell: 5 }));
        // The new cell read twice in a row wins over the click (19:16:41 of the same run).
        assert_eq!(run(Some("Go to cell 04 (a locked door)"), 2), Ok(Action::Patrol { cell: 5 }));
        assert_eq!(run(Some("Go to cell 06 (a locked door)"), 1), Ok(Action::Patrol { cell: 5 }));
    }

    /// With the Severed Iron Maiden held, a `Decision` with `Instrument` and the Warden's cell read changed is a
    /// Replace, though the day goes up after it (2026-11-11 17:53:29 and 17:57:10 were taken as End). The same signals
    /// with the cell unchanged, or without the Maiden, stay End.
    #[test]
    fn a_replace_with_the_maiden_is_a_replace() {
        let wolf = "{\"beast\":\"wolf\",\"event\":null,\"ivory\":false,\"kind\":\"inmate\",\"prey\":null,\"tier\":1}";
        let lion = "{\"beast\":\"lion\",\"event\":null,\"ivory\":false,\"kind\":\"inmate\",\"prey\":null,\"tier\":4}";
        let run = |tools: &str, after: &str| {
            let rows = [
                hand_row("00:00.500", "\"kind\":\"game\",\"signal\":\"RunStart\""),
                hand_read("00:01.000", "Manage", 2, 3, tools, wolf),
                hand_read("00:01.050", "Manage", 2, 3, tools, wolf),
                hand_row("00:01.100", "\"kind\":\"advice\",\"pick\":\"x\",\"read_at\":\"2026-11-03T10:00:01.050Z\",\"rounds\":100,\"ev\":6.0,\"best_ev\":6.0,\"sd\":2.0"),
                hand_row("00:02.000", "\"kind\":\"game\",\"signal\":\"Decision\""),
                hand_row("00:02.050", "\"kind\":\"game\",\"signal\":\"Instrument\""),
                hand_read("00:02.200", "Manage", 2, 3, tools, wolf),
                hand_read("00:02.500", "Unknown", 2, 3, tools, after),
                hand_read("00:02.900", "Unknown", 2, 3, tools, after),
                hand_read("00:03.300", "Patrol", 3, 3, tools, after),
                hand_read("00:03.400", "Patrol", 3, 3, tools, after),
                hand_row("00:05.000", "\"kind\":\"run_end\",\"vaulted\":1"),
            ];
            hand_run(&rows).remove(0).actual
        };
        assert_eq!(run("\"maiden\"", lion), Ok(Action::Swap));
        assert_eq!(run("\"maiden\"", wolf), Ok(Action::End));
        assert_eq!(run("", lion), Ok(Action::End));
    }

    /// The top-up is for a move made that is not the best and was dropped by the racing (fewer rounds than asked):
    /// the best move, a move that played every round, a single option and a move that cannot be told need none.
    #[test]
    fn the_top_up_is_for_dropped_moves_that_were_not_best() {
        let (made, other) = (Action::Patrol { cell: 3 }, Action::Patrol { cell: 5 });
        let mut d = patrol_decision();
        d.ranking = vec![option(other.clone(), 6.5, 0.0, 1600), option(made.clone(), 6.0, 0.2, 200)];
        assert!(super::needs_top_up(&d, 1600), "dropped at 200 and not the best");
        d.ranking[1].n = 1600;
        assert!(!super::needs_top_up(&d, 1600), "played every round: its gap is measured already");
        d.ranking = vec![option(made.clone(), 6.5, 0.0, 1600), option(other.clone(), 6.0, 0.2, 200)];
        assert!(!super::needs_top_up(&d, 1600), "the move made is the best");
        d.ranking = vec![option(made.clone(), 6.5, 0.0, 1600)];
        assert!(!super::needs_top_up(&d, 1600), "a single option");
        d.ranking = vec![option(other, 6.5, 0.0, 1600), option(made, 6.0, 0.2, 200)];
        d.actual = Err(("x", "x"));
        assert!(!super::needs_top_up(&d, 1600), "the move made cannot be told");
    }

    /// The pair's values replace the two options' and the ranking is ordered by mean again: the leader of the pair
    /// takes gap_se 0 and the other the pair's gap_se, the rest keep theirs; a move made that comes out ahead becomes
    /// the best (and so the verdict Best).
    #[test]
    fn the_pair_values_go_into_the_ranking() {
        let (made, other, third) = (Action::Patrol { cell: 3 }, Action::Patrol { cell: 5 }, Action::End);
        let mut d = patrol_decision();
        d.ranking = vec![option(other.clone(), 6.5, 0.0, 1600), option(third.clone(), 6.2, 0.15, 1600), option(made.clone(), 6.0, 0.2, 200)];
        assert!(matches!(d.verdict(), Verdict::Loss { .. }));
        // The move made still trails, by less: a gap within the noise now.
        super::merge_pair(&mut d.ranking, &[option(other.clone(), 6.45, 0.0, 1600), option(made.clone(), 6.38, 0.06, 1600)], 1600);
        assert_eq!(d.ranking.iter().map(|o| (o.action.clone(), o.n)).collect::<Vec<_>>(), vec![(other.clone(), 1600), (made.clone(), 1600), (third.clone(), 1600)]);
        assert_eq!((d.ranking[0].mean, d.ranking[0].gap_se), (6.45, 0.0));
        assert_eq!((d.ranking[1].mean, d.ranking[1].gap_se), (6.38, 0.06));
        assert_eq!((d.ranking[2].mean, d.ranking[2].gap_se), (6.2, 0.15), "the third option keeps its values");
        assert!(matches!(d.verdict(), Verdict::Noise { .. }), "{:?}", d.verdict());
        // The move made comes out ahead: it is the best.
        super::merge_pair(&mut d.ranking, &[option(made.clone(), 6.5, 0.0, 1600), option(other.clone(), 6.4, 0.05, 1600)], 1600);
        assert_eq!(d.ranking[0].action, made);
        assert_eq!((d.ranking[0].gap_se, d.ranking[1].gap_se), (0.0, 0.05));
        assert_eq!(d.verdict(), Verdict::Best);
        // A frozen option (fewer rounds) with a higher checkpoint mean does not come out ahead of the pair.
        d.ranking = vec![option(other.clone(), 6.3, 0.0, 1400), option(third.clone(), 6.27, 0.3, 200), option(made.clone(), 6.0, 0.2, 200)];
        super::merge_pair(&mut d.ranking, &[option(other.clone(), 6.25, 0.0, 1600), option(made.clone(), 6.1, 0.06, 1600)], 1600);
        assert_eq!(d.ranking.iter().map(|o| o.action.clone()).collect::<Vec<_>>(), vec![other.clone(), made.clone(), third]);
        // A pair that is not two options changes nothing.
        let before: Vec<f64> = d.ranking.iter().map(|o| o.mean).collect();
        super::merge_pair(&mut d.ranking, &[option(other, 1.0, 0.0, 1600)], 1600);
        assert_eq!(d.ranking.iter().map(|o| o.mean).collect::<Vec<_>>(), before);
    }

    /// The single-run sd 2 widened at day 1.
    fn spread_one() -> f64 {
        (4.0 + super::SPREAD_AT_START * super::SPREAD_AT_START).sqrt()
    }

    /// The drift follows the first advice row's playouts version: version 2 takes DRIFT_AT_START, a row without the
    /// field (the logs before it) DRIFT_AT_START_V1.
    #[test]
    fn the_drift_follows_the_playouts_version() {
        let log = |extra: &str| {
            format!(
                "{{\"at\":\"2026-11-09T10:00:00.000Z\",\"kind\":\"game\",\"signal\":\"RunStart\"}}
{{\"at\":\"2026-11-09T10:00:01.000Z\",\"kind\":\"advice\",\"read_at\":\"2026-11-09T10:00:00.500Z\",\"pick\":\"Go to cell 04\",\"rounds\":1600,\"ev\":6.0,\"best_ev\":6.0,\"sd\":2.0{extra}}}"
            )
        };
        let new = super::games("t", &log(",\"playout\":2")).remove(0);
        assert_eq!(new.playout, Some(2));
        assert_eq!(new.drift_at_start(), super::DRIFT_AT_START);
        let old = super::games("t", &log("")).remove(0);
        assert_eq!(old.playout, None);
        assert_eq!(old.drift_at_start(), super::DRIFT_AT_START_V1);
        // a later version takes the latest fit until it has its own
        let later = super::Game { playout: Some(3), ..super::games("t", &log("")).remove(0) };
        assert_eq!(later.drift_at_start(), super::DRIFT_AT_START);
    }

    /// The expected value comes from the first advice row's playouts: on an opening instrument pick whose Blacklist
    /// carried its correction, `best_ev_raw` rather than the `best_ev` shown; a row without it gives `best_ev`.
    #[test]
    fn the_expected_value_leaves_the_blacklist_bias_out() {
        let row = |extra: &str| {
            format!(
                "{{\"at\":\"2026-11-09T10:00:00.000Z\",\"kind\":\"game\",\"signal\":\"RunStart\"}}
{{\"at\":\"2026-11-09T10:00:01.000Z\",\"kind\":\"advice\",\"read_at\":\"2026-11-09T10:00:00.500Z\",\"pick\":\"Take Executioner's Blacklist\",\"rounds\":1600,\"ev\":9.14,\"best_ev\":9.14,\"sd\":2.0{extra}}}"
            )
        };
        let expected = |text: &str| super::games("t", text).first().and_then(|g| g.expected);
        assert_eq!(expected(&row(",\"ev_raw\":8.0,\"best_ev_raw\":8.0,\"tool_bias\":1.14")), Some((8.0, 2.0)));
        assert_eq!(expected(&row("")), Some((9.14, 2.0)));
    }
}
