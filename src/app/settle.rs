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
//!   loss of 0.34 at 400 rollouts, but fell inside the noise at 1600).
//! - Language: `audit` temporarily switches the global language when it matches advice labels (`audit::advised`),
//!   and switching language from a background thread would flash the window into the other language; so the run's
//!   advice language must match the current UI language to be settled, otherwise nothing is settled and the reason
//!   is given.

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
    /// |luck| is greater than the decision loss.
    Luck,
    /// The decision loss is not less than |luck|.
    Decisions,
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
    let cause = luck.map(|l| if l.abs() > loss { Cause::Luck } else { Cause::Decisions });
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
        cause,
        worst,
        scored: g.decisions.iter().filter(|d| !d.ranking.is_empty()).count(),
        unknown: g.decisions.iter().filter(|d| d.actual.is_err()).count(),
        rollouts,
        kind: RunKind::Tartarus,
        reach: None,
    }
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
    audit::score(std::slice::from_mut(&mut game), rollouts);
    Outcome::Settled(Box::new(summarize(&game, rollouts)))
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
        assert_eq!(s.cause, Some(Cause::Luck));
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
        assert_eq!(s.cause, Some(Cause::Luck));
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
        // Loss greater than |luck|: mostly decisions.
        g.decisions[0].ranking[1].gap_se = 0.1;
        g.decisions[0].ranking[0].mean = 10.0;
        g.expected = Some((2.0, 2.0));
        let s = summarize(&g, 400);
        assert_eq!(s.cause, Some(Cause::Decisions));
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
