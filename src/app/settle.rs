//! Run review (the settlement on the Ascendant Run tab): after a run ends, re-solves every decision of that run in the
//! background with [`crate::audit`] and works out the vaulted count, the opening expectation, the decision loss, the
//! luck, how many standard deviations the result is off, and the single costliest step, so the player does not have to
//! run `ascendant-cli audit` separately to learn "was this run mostly luck or decisions".
//!
//! - Pure logic, no egui: the screen is the Run tab in ui.rs.
//! - Trigger: ui.rs notices that the run log moved to a new folder; the old one is the run that just ended. It is
//!   read only after the log is fully written ([`super::runlog::RunLog::flushed`]).
//! - The re-solving runs in its own small rayon pool: a quarter of the cores, below normal priority, so the game and
//!   the live advice carry on as usual; [`ROLLOUTS`] rollouts per option, same as the audit default: at 400
//!   rollouts the line between noise and loss falls elsewhere (the step on day 8 of 2026-11-04 14-49-39 was judged a
//!   loss of 0.34 at 400 rollouts, but fell inside the noise at 1600). The racing drops a trailing move early and
//!   freezes its values where its gap is already past the racing's threshold, so a move made that was dropped always
//!   read as a loss of that checkpoint's noise; after the re-solve, every such move is re-solved once more together
//!   with the best one to the full round count ([`audit::top_up`], 2026-11-10).
//! - The verdict (Tartarus, 2026-11-09): the opening expectation is a floor (the playouts follow the plain strategy,
//!   the player the advice), so the actual came out above it in almost every run and "mostly luck" showed nearly
//!   always. The audit's drift correction (the run's [`audit::Game::drift_at_start`], by the version of the playouts
//!   that solved it, and [`audit::SPREAD_AT_START`]) is taken out first.
//!   A decision loss of [`LOSS_SHARE`] of the widened spread or more is the decisions, whatever the result: luck that
//!   made up for it must not hide the costliest step (2026-11-08 10:39, a loss of 6.26 within one standard deviation).
//!   Otherwise, within [`WITHIN_Z`] drift-corrected standard deviations the run is within the expected range; beyond
//!   it, a loss not less than the drift-corrected luck is the decisions, else luck ran high or low.
//! - Language: `audit` temporarily switches the global language when it matches advice labels (`audit::advised`),
//!   and switching language from a background thread would flash the window into the other language; so the run's
//!   advice language must match the current UI language to be settled, otherwise nothing is settled and the reason
//!   is given.

use super::gamelog::RiotEnd;
use crate::audit::{self, Decision, Game, Verdict};
use crate::lang::{Lang, tr};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

/// Rollouts per option when settling.
pub const ROLLOUTS: usize = 1600;

/// Windows' THREAD_PRIORITY_BELOW_NORMAL.
const BELOW_NORMAL: i32 = -1;

/// Which game a settlement is for: it sets the unit of `actual`, `expected`, `loss` and `luck` (Tartarus: Chromatics;
/// Vorax: final activity counted up to 840,000) and what `Step::day` means (Tartarus: the day; Vorax: the round).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Tartarus,
    Vorax,
}

/// Vorax only: the chance of reaching the target at the start, and whether the game reached it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reach {
    pub target: i64,
    pub chance: f64,
    pub reached: bool,
    /// The final activity came from the battle panel and is too close to the target to tell ([`super::vorax_settle::Arrival::Unsure`]);
    /// `reached` is false then. (`Reach` is never serialized, so no serde default is needed.)
    pub unsure: bool,
}

/// Where the run's gap mostly comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// Tartarus: the result is within [`WITHIN_Z`] drift-corrected standard deviations of the expectation.
    Within,
    /// Luck ran above (`high`) or below the expectation, more than the decisions lost.
    Luck { high: bool },
    /// The decisions lost noticeably (see `cause`).
    Decisions,
}

/// A Tartarus run within this many drift-corrected standard deviations ([`audit::Game::z_net`]) is within the expected
/// range.
pub const WITHIN_Z: f64 = 1.0;

/// A decision loss of this share of the widened spread ([`audit::Game::spread_net`]) or more is noticeable.
pub const LOSS_SHARE: f64 = 0.5;

/// The verdict of a run from its decision loss, its luck and, for Tartarus, the drift-corrected (z, luck, spread):
/// see the module notes. Without the drift correction (Vorax, or the share of days to come unknown) the rule before
/// it: |luck| greater than the loss is luck.
pub(crate) fn cause(loss: f64, luck: Option<f64>, net: Option<(f64, f64, f64)>) -> Option<Cause> {
    match net {
        Some((_, _, spread)) if loss >= LOSS_SHARE * spread => Some(Cause::Decisions),
        Some((z_net, _, _)) if z_net.abs() < WITHIN_Z => Some(Cause::Within),
        Some((_, luck_net, _)) if loss >= luck_net.abs() => Some(Cause::Decisions),
        Some((_, luck_net, _)) => Some(Cause::Luck { high: luck_net > 0.0 }),
        None => luck.map(|l| if l.abs() > loss { Cause::Luck { high: l > 0.0 } } else { Cause::Decisions }),
    }
}

/// The costliest step.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub day: i64,
    /// The step's read time (the log's `read_at`), for the report export's screenshots.
    pub read_at: String,
    /// The screen's name (in the current UI language).
    pub screen: &'static str,
    /// What was actually picked, the best option from the re-solve, and the advice the log held at the time (labels).
    pub made: String,
    pub best: String,
    pub advice: String,
    /// Whether the advice was followed; None when that cannot be told.
    pub followed: Option<bool>,
    /// Best mean minus the mean of what was picked, and the paired standard error of the pick against the top option.
    pub loss: f64,
    pub gap_se: f64,
    /// The gap is within 2 times gap_se (noise, not counted in the decision loss).
    pub noise: bool,
}

/// The settlement of one run.
#[derive(Clone, Debug, PartialEq)]
pub struct Settled {
    /// The run log folder's name.
    pub folder: String,
    /// The vaulted count (`run_end`).
    pub vaulted: Option<u32>,
    /// The actual result: the vaulted count plus the value of the final board and the target reward.
    pub actual: Option<f64>,
    /// The opening expectation and the single-run standard deviation.
    pub expected: Option<(f64, f64)>,
    /// The decision loss (noise excluded).
    pub loss: f64,
    /// The sum and count of the gaps that fall within the noise.
    pub noise: (f64, usize),
    /// Luck = actual - expected + decision loss.
    pub luck: Option<f64>,
    /// How many standard deviations the actual result is off the expectation.
    pub z: Option<f64>,
    /// Tartarus: the expectation plus the usual drift ([`audit::Game::drift`]), and how many widened standard
    /// deviations the actual is off it ([`audit::Game::z_net`]); None for Vorax or when the drift is unknown.
    pub expected_net: Option<f64>,
    pub z_net: Option<f64>,
    pub cause: Option<Cause>,
    pub worst: Option<Step>,
    /// How many steps were re-solved, and how many of them could not tell what was actually picked.
    pub scored: usize,
    pub unknown: usize,
    pub rollouts: usize,
    /// Which game this is (and so the units), see [`RunKind`].
    pub kind: RunKind,
    /// Vorax only: reaching the target, see [`Reach`]; None for Tartarus.
    pub reach: Option<Reach>,
    /// Tartarus only: how the riot battle after the run went (the log's `riot` row, see [`riot_of`]); None until it
    /// is over (it ends about a minute after the settlement starts, and the run review then puts it in, see
    /// `RunReview::riot_done`), for a run logged before the row existed, and for Vorax.
    pub riot: Option<RiotEnd>,
}

/// The result of settling, or the reason nothing was settled.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Settled(Box<Settled>),
    /// The log has no run end (`run_end`): the previous run was not finished.
    Unfinished,
    /// The run's advice language differs from the UI language.
    Language { log: Lang, ui: Lang },
    /// The run finished, but there is no advice to re-solve.
    Empty,
    /// An error such as the log being unreadable.
    Failed(String),
}

/// The language of an advice label: the same test as `audit::advised` (all ASCII means English).
fn pick_lang(pick: &str) -> Lang {
    if pick.is_ascii() { Lang::En } else { Lang::ZhTw }
}

/// The language of the first advice in the log whose language differs from `ui`.
pub(crate) fn other_lang(text: &str, ui: Lang) -> Option<Lang> {
    text.lines()
        .filter(|l| l.contains("\"kind\":\"advice\""))
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("pick").and_then(|p| p.as_str()).map(pick_lang))
        .find(|&l| l != ui)
}

fn screen_name(screen: &str) -> &'static str {
    match screen {
        "Patrol" => tr("Patrol"),
        "Manage" => tr("Manage"),
        "Interrogation" => tr("Interrogation"),
        "InstrumentPick" => tr("Instrument pick"),
        _ => tr("Other"),
    }
}

/// One decision's gap: (loss, gap_se, is noise); None when it was the best, could not be re-solved, or the pick cannot
/// be told.
fn gap(d: &Decision) -> Option<(f64, f64, bool)> {
    match d.verdict() {
        Verdict::Loss { loss, gap_se } => Some((loss, gap_se, false)),
        Verdict::Noise { loss, gap_se } if loss > 0.0 => Some((loss, gap_se, true)),
        _ => None,
    }
}

/// Turns a re-solved run into a settlement.
pub(crate) fn summarize(g: &Game, rollouts: usize) -> Settled {
    let loss = g.loss();
    let luck = g.luck();
    let net = g.z_net().zip(g.luck_net()).zip(g.spread_net()).map(|((z, l), s)| (z, l, s));
    let cause = cause(loss, luck, net);
    let worst = g
        .decisions
        .iter()
        .filter_map(|d| gap(d).map(|x| (d, x)))
        .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
        .map(|(d, (loss, gap_se, noise))| Step {
            day: d.day,
            read_at: d.read_at.clone(),
            screen: screen_name(&d.screen),
            made: d.actual.as_ref().map(|a| d.label(a)).unwrap_or_default(),
            best: d.best().map(|b| d.label(&b.action)).unwrap_or_default(),
            advice: d.pick.clone(),
            followed: d.followed(),
            loss,
            gap_se,
            noise,
        });
    Settled {
        folder: g.folder.clone(),
        vaulted: g.vaulted,
        actual: g.actual(),
        expected: g.expected,
        loss,
        noise: g.noise(),
        luck,
        z: g.z(),
        expected_net: g.expected.zip(g.drift()).map(|((e, _), d)| e + d),
        z_net: g.z_net(),
        cause,
        worst,
        scored: g.decisions.iter().filter(|d| !d.ranking.is_empty()).count(),
        unknown: g.decisions.iter().filter(|d| d.actual.is_err()).count(),
        rollouts,
        kind: RunKind::Tartarus,
        reach: None,
        riot: None,
    }
}

/// The riot row (`riot`, written by `Session::log_riot_end`) after the last run_end of a run log's `text`, if any.
pub fn riot_of(text: &str) -> Option<RiotEnd> {
    let after = text.rfind("\"kind\":\"run_end\"").map_or(text, |k| &text[k..]);
    let row: serde_json::Value = after.lines().filter(|l| l.contains("\"kind\":\"riot\"")).filter_map(|l| serde_json::from_str(l).ok()).next_back()?;
    let flag = |k: &str| row[k].as_bool().unwrap_or(false);
    let count = |k: &str| row[k].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0);
    Some(RiotEnd { fought: flag("fought"), boss: flag("boss"), boss_killed: flag("boss_killed"), red: count("red"), color: count("color"), medium: count("medium") })
}

/// Settles the log `text` of the folder `folder`: `ui` is the current UI language and must equal the global language
/// (`audit` builds its labels in the global language). The re-solving uses the caller's current rayon pool.
pub fn settle(folder: &str, text: &str, ui: Lang, rollouts: usize) -> Outcome {
    if !text.contains("\"kind\":\"run_end\"") {
        return Outcome::Unfinished;
    }
    // Check the language before calling `audit::games`: when they differ, it would temporarily change the global
    // language.
    if let Some(log) = other_lang(text, ui) {
        return Outcome::Language { log, ui };
    }
    // The last finished run in the folder.
    let Some(mut game) = audit::games(folder, text).into_iter().rev().find(|g| g.end.is_some()) else {
        return Outcome::Empty;
    };
    if game.decisions.is_empty() {
        return Outcome::Empty;
    }
    audit::score_topped(std::slice::from_mut(&mut game), rollouts);
    Outcome::Settled(Box::new(Settled { riot: riot_of(text), ..summarize(&game, rollouts) }))
}

/// Settles the folder `dir` (reads its log.jsonl); no log file means no advice to settle.
pub fn settle_dir(dir: &Path, ui: Lang, rollouts: usize) -> Outcome {
    let name = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
    match std::fs::read_to_string(dir.join("log.jsonl")) {
        Ok(text) => settle(&name, &text, ui, rollouts),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Outcome::Empty,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// Settles `dir` in the background: waits for `written` (the log is fully written) before reading; the re-solving
/// runs on a small pool of a quarter of the cores at below normal priority, [`ROLLOUTS`] rollouts. The result comes
/// through the returned channel, and `notify` (repaint the window) is called after it is sent. If the thread dies the
/// channel disconnects, and the caller treats that as a failure.
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
            .thread_name(|i| format!("settle-{i}"))
            .start_handler(|_| crate::solver::solve::set_priority(BELOW_NORMAL))
            .build();
        let out = match pool {
            // The language is taken only here: it is the same one `audit` used to build the labels.
            Ok(pool) => pool.install(|| settle_dir(&dir, crate::lang::lang(), ROLLOUTS)),
            Err(e) => Outcome::Failed(e.to_string()),
        };
        let _ = tx.send(out);
        notify();
    });
    rx
}

#[cfg(test)]
mod riot_tests {
    use super::*;

    /// The riot row after the run's run_end is taken (the last when there are two); one before it, or none, is not.
    #[test]
    fn the_riot_row_after_the_run_end() {
        let end = r#"{"at":"2026-11-09T20:16:20.405Z","kind":"run_end","vaulted":6}"#;
        let riot = |red: u32, fought: bool| format!(r#"{{"at":"2026-11-09T20:16:54.200Z","kind":"riot","fought":{fought},"boss":false,"boss_killed":false,"red":{red},"color":3,"medium":7}}"#);
        let fought = RiotEnd { fought: true, red: 1, color: 3, medium: 7, ..RiotEnd::default() };
        assert_eq!(riot_of(&format!("{end}\n{}\n", riot(1, true))), Some(fought));
        assert_eq!(riot_of(&format!("{end}\n{}\n{}\n", riot(9, true), riot(1, true))), Some(fought));
        assert_eq!(riot_of(&format!("{}\n{end}\n", riot(1, true))), None);
        assert_eq!(riot_of(end), None);
        let abandoned = RiotEnd { red: 0, color: 3, medium: 7, ..RiotEnd::default() };
        assert_eq!(riot_of(&format!("{end}\n{}\n", riot(0, false))), Some(abandoned));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Action;
    use crate::solver::solve::SolveOption;
    use serde_json::{Value, json};

    // The hand-made log follows the format in tests/audit.rs.

    /// A 16-cell board: cell 0 is the Interrogation Room, the rest are sealed.
    fn board() -> Vec<Value> {
        (0..16).map(|n| if n == 0 { json!({"kind": "office"}) } else { json!({"kind": "sealed", "event": null}) }).collect()
    }

    fn time(at: &str) -> String {
        format!("2026-11-03T10:{at}Z")
    }

    fn read(at: &str, screen: &str, day: i64, warden: i64, extra: Value) -> String {
        let mut model = json!({
            "v": 1, "day": day, "maxDays": 20, "step": "manage", "warden": warden, "dest": [],
            "devourUsed": false, "swapUsed": false, "huntUsed": true, "eventKind": "chain",
            "choice": {"kind": "tool", "options": []}, "chainArmed": false, "tools": [], "vault": [],
            "cells": board(),
        });
        if let (Some(m), Some(x)) = (model.as_object_mut(), extra.as_object()) {
            for (k, v) in x {
                m.insert(k.clone(), v.clone());
            }
        }
        json!({"at": time(at), "kind": "read", "screen": screen, "theme": null, "model": model}).to_string()
    }

    fn game(at: &str, signal: &str) -> String {
        json!({"at": time(at), "kind": "game", "signal": signal}).to_string()
    }

    fn advice(at: &str, read_at: &str, pick: &str) -> String {
        json!({"at": time(at), "kind": "advice", "pick": pick, "read_at": time(read_at), "rounds": 100, "ev": 6.0, "best_ev": 6.0, "sd": 2.0, "also_good": []}).to_string()
    }

    /// A patrol from the Interrogation Room to cell 04 (then 2 consecutive reads of the Warden at cell 04), 1 vaulted;
    /// `end` decides whether there is a run end.
    fn patrol_log(pick: &str, end: bool) -> String {
        let patrol = json!({"step": "patrol", "dest": [3, 5]});
        let mut log = vec![
            game("00:00.500", "RunStart"),
            read("00:01.000", "Patrol", 2, 0, patrol),
            advice("00:01.100", "00:01.000", pick),
            game("00:02.000", "Moved"),
            game("00:02.000", "Decision"),
            read("00:02.500", "Manage", 2, 3, json!({})),
            read("00:02.600", "Manage", 2, 3, json!({})),
        ];
        if end {
            log.push(json!({"at": time("00:05.000"), "kind": "run_end", "vaulted": 1}).to_string());
        }
        log.join("\n")
    }

    /// Writes a log.jsonl in a temp folder and returns the folder.
    fn temp_run(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ascendant-settle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.jsonl"), text).unwrap();
        dir
    }

    fn option(action: Action, mean: f64, gap_se: f64) -> SolveOption {
        SolveOption { label: String::new(), action, mean, se: 0.01, gap_se, n: 100, best: 0.0, policy: false }
    }

    #[test]
    fn a_finished_run_from_its_log_file() {
        let dir = temp_run("finished", &patrol_log("Go to cell 04 (a locked door)", true));
        let out = settle_dir(&dir, Lang::En, 40);
        let _ = std::fs::remove_dir_all(&dir);
        let Outcome::Settled(s) = out else { panic!("{out:?}") };
        assert_eq!(s.vaulted, Some(1));
        assert_eq!(s.actual, Some(1.0));
        assert_eq!(s.expected, Some((6.0, 2.0)));
        assert_eq!(s.z, Some(-2.5));
        assert!(s.loss >= 0.0);
        // Luck = actual - expected + loss; |-5 + loss| is far greater than this step's loss: mostly luck.
        let luck = s.luck.unwrap();
        assert!((luck - (1.0 - 6.0 + s.loss)).abs() < 1e-9, "{luck}");
        // Day 2 of 20: 0.9 of the days to come; a log without the playouts' version takes the old playouts' drift.
        assert!((s.expected_net.unwrap() - (6.0 + audit::DRIFT_AT_START_V1 * 0.9)).abs() < 1e-9);
        assert!(s.z_net.unwrap() < -1.0, "{:?}", s.z_net);
        assert!(matches!(s.cause, Some(Cause::Luck { high: false } | Cause::Decisions)), "{:?}", s.cause);
        assert_eq!((s.scored, s.unknown, s.rollouts), (1, 0, 40));
        assert!(s.folder.starts_with("ascendant-settle-"));
    }

    #[test]
    fn the_largest_step_and_noise() {
        let text = patrol_log("Go to cell 04 (a locked door)", true);
        let mut g = audit::games("r", &text).remove(0);
        // 04 was actually taken; the re-solve says 06 averages 1.0 more, with gap_se 0.1: beyond the noise, so it
        // counts as loss.
        g.decisions[0].ranking = vec![option(Action::Patrol { cell: 5 }, 6.0, 0.0), option(Action::Patrol { cell: 3 }, 5.0, 0.1)];
        let s = summarize(&g, 400);
        assert!((s.loss - 1.0).abs() < 1e-9);
        assert!((s.luck.unwrap() - (1.0 - 6.0 + 1.0)).abs() < 1e-9);
        // A loss of 1 is less than half the widened spread (sqrt(2^2 + (2.16 x 0.9)^2) = 2.79) and than |luck| net.
        assert_eq!(s.cause, Some(Cause::Luck { high: false }));
        let w = s.worst.clone().unwrap();
        assert_eq!((w.day, w.screen, w.noise), (2, "Patrol", false));
        assert!((w.loss - 1.0).abs() < 1e-9 && (w.gap_se - 0.1).abs() < 1e-9);
        assert!(w.made.contains("04") && w.best.contains("06"), "{w:?}");
        assert_eq!(w.advice, "Go to cell 04 (a locked door)");
        assert_eq!(w.followed, Some(true));
        // gap_se 0.6: a gap of 1.0 is within 2 times that, so it is noise: not counted as loss, but still the largest
        // step.
        g.decisions[0].ranking[1].gap_se = 0.6;
        let s = summarize(&g, 400);
        assert_eq!(s.loss, 0.0);
        assert_eq!(s.noise.1, 1);
        assert!(s.worst.unwrap().noise);
        // A loss of 5, beyond half the spread: decisions.
        g.decisions[0].ranking[1].gap_se = 0.1;
        g.decisions[0].ranking[0].mean = 10.0;
        let s = summarize(&g, 400);
        assert_eq!(s.cause, Some(Cause::Decisions));
    }

    /// The three verdicts with the drift taken out: within one widened sd, decisions, and luck high or low.
    #[test]
    fn the_verdict_takes_the_usual_drift_out() {
        let text = patrol_log("Go to cell 04 (a locked door)", true);
        let mut g = audit::games("r", &text).remove(0);
        assert_eq!(g.start_left, Some(0.9));
        let drift = g.drift_at_start() * 0.9;
        // 04 taken and best: no loss.
        g.decisions[0].ranking = vec![option(Action::Patrol { cell: 3 }, 6.0, 0.0), option(Action::Patrol { cell: 5 }, 5.9, 0.5)];
        // The actual 1 a little above the expectation plus the drift: the old rule called it luck (|luck| > 0).
        g.expected = Some((1.0 - drift - 0.5, 2.0));
        let s = summarize(&g, 400);
        assert!(s.z_net.unwrap().abs() < WITHIN_Z && s.luck.unwrap() > 0.0, "{s:?}");
        assert_eq!(s.cause, Some(Cause::Within));
        // Far above the expectation with no loss: luck ran high.
        g.expected = Some((-6.0, 2.0));
        assert_eq!(summarize(&g, 400).cause, Some(Cause::Luck { high: true }));
        // Far below with no loss: luck ran low.
        g.expected = Some((6.0, 2.0));
        assert_eq!(summarize(&g, 400).cause, Some(Cause::Luck { high: false }));
        // Far below with 04 costing 3 (beyond the noise, more than half the spread of 2.79): the decisions.
        g.decisions[0].ranking = vec![option(Action::Patrol { cell: 5 }, 9.0, 0.0), option(Action::Patrol { cell: 3 }, 6.0, 0.1)];
        let s = summarize(&g, 400);
        assert!((s.loss - 3.0).abs() < 1e-9, "{s:?}");
        assert_eq!(s.cause, Some(Cause::Decisions));
        // A loss of 3 with luck that made up for it, the actual within one sd of the expectation plus the drift: the
        // decisions all the same (2026-11-08 10:39 lost 6.26 within the range).
        g.expected = Some((1.0 - drift + 3.0 - 0.5, 2.0));
        let s = summarize(&g, 400);
        assert!(s.z_net.unwrap().abs() < WITHIN_Z, "{s:?}");
        assert_eq!(s.cause, Some(Cause::Decisions));
        // A loss under half the spread within the range: within.
        g.decisions[0].ranking[0].mean = 7.0;
        let s = summarize(&g, 400);
        assert!((s.loss - 1.0).abs() < 1e-9 && s.z_net.unwrap().abs() < WITHIN_Z, "{s:?}");
        assert_eq!(s.cause, Some(Cause::Within));
        // Unknown drift (no read for the advice row): the rule before it, |luck| against the loss.
        g.start_left = None;
        let s = summarize(&g, 400);
        assert_eq!((s.z_net, s.expected_net), (None, None));
        assert_eq!(s.cause, cause(s.loss, s.luck, None));
    }

    /// The settle page takes the drift of the playouts that solved the run: the same run with its advice rows naming
    /// version 2 expects DRIFT_AT_START over the expectation, without the field DRIFT_AT_START_V1.
    #[test]
    fn the_drift_follows_the_logged_playouts_version() {
        let old = patrol_log("Go to cell 04 (a locked door)", true);
        let new = old.replace("\"rounds\":100", "\"playout\":2,\"rounds\":100");
        assert_ne!(old, new, "the advice rows name the version");
        let expected_net = |name: &str, text: &str| {
            let dir = temp_run(name, text);
            let out = settle_dir(&dir, Lang::En, 40);
            let _ = std::fs::remove_dir_all(&dir);
            let Outcome::Settled(s) = out else { panic!("{out:?}") };
            s.expected_net.unwrap()
        };
        assert!((expected_net("playout-old", &old) - (6.0 + audit::DRIFT_AT_START_V1 * 0.9)).abs() < 1e-9);
        assert!((expected_net("playout-new", &new) - (6.0 + audit::DRIFT_AT_START * 0.9)).abs() < 1e-9);
    }

    #[test]
    fn an_unfinished_run_is_not_settled() {
        let dir = temp_run("unfinished", &patrol_log("Go to cell 04 (a locked door)", false));
        let out = settle_dir(&dir, Lang::En, 40);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out, Outcome::Unfinished);
    }

    #[test]
    fn another_language_is_not_settled() {
        // English advice with a Chinese UI (and the other way round): audit is not called, the global language is left
        // alone.
        let dir = temp_run("language", &patrol_log("Go to cell 04 (a locked door)", true));
        let out = settle_dir(&dir, Lang::ZhTw, 40);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out, Outcome::Language { log: Lang::En, ui: Lang::ZhTw });
        assert_eq!(settle("r", &patrol_log("去 04 號 (未開啟的牢房)", true), Lang::En, 40), Outcome::Language { log: Lang::ZhTw, ui: Lang::En });
        assert_eq!(crate::lang::lang(), Lang::En);
    }

    #[test]
    fn a_missing_log_has_nothing_to_settle() {
        let dir = std::env::temp_dir().join(format!("ascendant-settle-{}-none", std::process::id()));
        assert_eq!(settle_dir(&dir, Lang::En, 40), Outcome::Empty);
    }
}
