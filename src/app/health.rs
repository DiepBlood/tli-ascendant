//! The run's check: when a run ends, its log is read for what went wrong on
//! the player's side, the things a review looked for by hand (2026-10-30):
//! advice that changed under its mark, clicks with no advice on show, slow
//! advice, a result far below the prediction, and a crash. The lines go to
//! the window, onto the game with the run's end, and to the run's folder as
//! health.txt.

use crate::engine::data::{LOOT, OBJECTIVE_CACHE, RAINBOW};
use crate::lang::{tr, trf};
use serde_json::Value;
use std::path::Path;

/// How long a mark has to have been on show for its advice changing to
/// count: a held advice left over from the screen before showed for 8 ms
/// (2026-10-30), which no player sees.
const SHOWN_MS: f64 = 100.0;
/// Advice later than this after the decision cards came up counts as slow:
/// a median run took 0.45 s.
const CARDS_SLOW: f64 = 1.0;
/// Advice later than this after a bite counts as slow: a median run took 0.6 s.
const BITE_SLOW: f64 = 1.2;
/// A result this many spreads below the prediction counts: results run
/// above it, as the playouts play worse than the advice (2026-10-30).
const BELOW: f64 = 2.5;

/// What the check found in one run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Decisions played: doors, cards and bites.
    pub decisions: usize,
    /// The median time from the decision cards coming up to advice, in s.
    pub cards_median: Option<f64>,
    /// One line per thing to look at.
    pub issues: Vec<String>,
}

impl Report {
    /// The one line for the window and the game.
    pub fn summary(&self) -> String {
        match (self.issues.len(), self.cards_median) {
            (0, Some(s)) => trf!("Run check: all clear ({} decisions, advice {} s after the cards).", self.decisions, format!("{s:.2}")),
            (0, None) => trf!("Run check: all clear ({} decisions).", self.decisions),
            (n, _) => trf!("Run check: {} to look at, listed in the Ascendant window.", n),
        }
    }

    /// The summary and every issue, a line each, for health.txt.
    pub fn text(&self) -> String {
        std::iter::once(self.summary()).chain(self.issues.iter().map(|i| format!("- {i}"))).collect::<Vec<_>>().join("\n") + "\n"
    }
}

/// Seconds since the epoch of a log time, "2026-10-30T20:09:50.650Z".
fn secs(at: &str) -> Option<f64> {
    super::latency::log_millis(at).map(|ms| ms as f64 / 1000.0)
}

/// A move's label without its reason: "Go to cell 06 (a locked door)" and
/// "Go to cell 06 (Chain Reaction opens 1)" are the same move.
fn base(label: &str) -> &str {
    label.split(" (").next().unwrap_or(label)
}

/// The Riot loot the run left: vaulted Chromatics, the last board read's
/// inmates by `LOOT`, and the objective's cache once done.
fn result(rows: &[Value]) -> Option<f64> {
    let vaulted = rows.iter().rev().find(|r| r["kind"] == "run_end")?["vaulted"].as_u64()? as f64;
    let board = rows.iter().rev().find(|r| r["kind"] == "read" && r["model"]["cells"].is_array())?;
    let loot: f64 = board["model"]["cells"]
        .as_array()?
        .iter()
        .filter(|c| c["kind"] == "inmate")
        .filter_map(|c| c["tier"].as_i64())
        .map(|t| LOOT[t.clamp(0, i64::from(RAINBOW)) as usize])
        .sum();
    let done = rows.iter().any(|r| r["kind"] == "game" && r["signal"] == "ObjectiveDone");
    Some(vaulted * LOOT[RAINBOW as usize] + loot + if done { OBJECTIVE_CACHE } else { 0.0 })
}

/// Checks the run logged in `dir`; None when it has no log or the run was
/// not played to its end. `crash_log`, when it changed after the run began,
/// is a crash.
pub fn check(dir: &Path, crash_log: Option<&Path>) -> Option<Report> {
    let text = std::fs::read_to_string(dir.join("log.jsonl")).ok()?;
    let rows: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    rows.iter().find(|r| r["kind"] == "run_end")?;
    let mut report = Report::default();
    let mut day = 0;
    // The advice on show: label, screen, since when its mark has shown, and
    // the longest it showed unbroken before (a held advice can flash its
    // mark back for a few ms after a bite).
    let mut shown: Option<(String, String, Option<f64>, f64)> = None;
    let mut advised = false;
    // A Cage Fight's chain badges on show: the bites they number are
    // clicked in a row, each with no new advice (2026-10-31: 27 of 33 lines).
    let mut chain_on = false;
    // The advice on show at the last click: still on show after it, it is
    // the click's, not advice for what comes next.
    let (mut last_advice, mut clicked): (Option<String>, Option<String>) = (None, None);
    let mut cards_up: Option<f64> = None;
    let mut bite_at: Option<f64> = None;
    let (mut cards, mut slow_cards, mut slow_bites) = (Vec::new(), Vec::new(), Vec::new());
    for r in &rows {
        let Some(at) = r["at"].as_str().and_then(secs) else { continue };
        match r["kind"].as_str().unwrap_or("") {
            "read" => {
                if let Some(d) = r["model"]["day"].as_i64() {
                    day = d;
                }
            }
            "game" => {
                let signal = r["signal"].as_str().unwrap_or("");
                let stamp = at - r["late_ms"].as_f64().unwrap_or(0.0) / 1000.0;
                let name = signal.split('(').next().unwrap_or("");
                if matches!(name, "Decision" | "Bite") {
                    report.decisions += 1;
                    if !advised && !(name == "Bite" && chain_on) {
                        report.issues.push(trf!("Day {}: clicked with no advice on show.", day));
                    }
                    advised = false;
                    clicked = last_advice.clone();
                }
                // A cell clicked (a Hunt's prey, an incident's cell) changes the board too.
                if matches!(name, "Decision" | "Bite" | "Moved" | "Consume" | "Reroll" | "TreasureOpen" | "ChainEnd" | "CellPicked") {
                    shown = None;
                }
                match name {
                    "CardsUp" => cards_up = Some(stamp),
                    "Bite" => bite_at = Some(stamp),
                    // The bite made a Chromatic: its vaulting and the treasure
                    // cover the board, whose chain advice took 1.34 s (2026-10-31).
                    "TreasureOpen" => bite_at = None,
                    "Decision" => (cards_up, bite_at) = (None, None),
                    _ => {}
                }
            }
            "view" => {
                let screen = r["screen"].as_str().unwrap_or("").to_string();
                let mark = r["mark"].as_bool().unwrap_or(false);
                chain_on = r["chain"].as_array().is_some_and(|c| !c.is_empty());
                let Some(advice) = r["advice"].as_str() else {
                    match &mut shown {
                        Some((_, on, since, longest)) if *on == screen => {
                            if let Some(s) = since.take() {
                                *longest = longest.max(at - s);
                            }
                        }
                        _ => shown = None,
                    }
                    continue;
                };
                last_advice = Some(advice.to_string());
                // Advice text counts once it is new since the click, its mark
                // or not: a single move's often shows without one.
                if mark || clicked.as_deref() != Some(advice) {
                    advised = true;
                }
                if mark {
                    if let Some(up) = cards_up.filter(|_| matches!(screen.as_str(), "Manage" | "Interrogation")) {
                        let wait = at - up;
                        cards.push(wait);
                        if wait > CARDS_SLOW {
                            slow_cards.push(wait);
                        }
                        cards_up = None;
                    }
                    if let Some(bite) = bite_at.filter(|_| screen == "ChainTarget") {
                        let wait = at - bite;
                        if wait > BITE_SLOW {
                            slow_bites.push(wait);
                        }
                        bite_at = None;
                    }
                }
                match &mut shown {
                    Some((was, on, since, longest)) if *on == screen && base(was) != base(advice) => {
                        let lit = since.map_or(*longest, |s| longest.max(at - s));
                        // A treasure's picks in a row each advise a gift.
                        if lit * 1000.0 >= SHOWN_MS && screen != "TreasurePick" {
                            report.issues.push(trf!("Day {}: the advice changed from \"{}\" to \"{}\" with its mark on.", day, was.clone(), advice));
                        }
                        shown = Some((advice.to_string(), screen, mark.then_some(at), 0.0));
                    }
                    Some((was, on, since, longest)) if *on == screen => {
                        match (mark, *since) {
                            (true, None) => *since = Some(at),
                            (false, Some(s)) => {
                                *longest = longest.max(at - s);
                                *since = None;
                            }
                            _ => {}
                        }
                        *was = advice.to_string();
                    }
                    _ => shown = Some((advice.to_string(), screen, mark.then_some(at), 0.0)),
                }
            }
            _ => {}
        }
    }
    if !slow_cards.is_empty() {
        let worst = slow_cards.iter().copied().fold(0.0, f64::max);
        report.issues.push(trf!("{} times the advice came over {} s after the decision cards (slowest {} s).", slow_cards.len(), CARDS_SLOW, format!("{worst:.2}")));
    }
    if !slow_bites.is_empty() {
        let worst = slow_bites.iter().copied().fold(0.0, f64::max);
        report.issues.push(trf!("{} times the advice came over {} s after a bite (slowest {} s).", slow_bites.len(), BITE_SLOW, format!("{worst:.2}")));
    }
    cards.sort_by(f64::total_cmp);
    report.cards_median = cards.get(cards.len() / 2).copied();
    // The prediction: day 1's patrol advice, before the run's luck. A log
    // begun mid-run (Ascendant restarted on day 12) has none: its first patrol
    // was taken for day 1's on 2026-10-31.
    let patrol: Vec<&str> = rows.iter().filter(|r| r["kind"] == "read" && r["screen"] == "Patrol" && r["model"]["day"] == 1).filter_map(|r| r["at"].as_str()).collect();
    let first = rows.iter().find(|r| r["kind"] == "advice" && r["sd"].as_f64().is_some_and(|sd| sd > 0.0) && r["read_at"].as_str().is_some_and(|at| patrol.contains(&at)));
    if let (Some(first), Some(got)) = (first, result(&rows)) {
        let (ev, sd) = (first["best_ev"].as_f64().unwrap_or(0.0), first["sd"].as_f64().unwrap_or(1.0));
        if got < ev - BELOW * sd {
            report.issues.push(trf!("The result, {} Chromatics' worth, fell far below the {} predicted on day 1.", format!("{got:.1}"), format!("{ev:.1}")));
        }
    }
    let began = rows.first().and_then(|r| r["at"].as_str()).map(str::to_string);
    if let (Some(path), Some(began)) = (crash_log, began)
        && let Ok(changed) = std::fs::metadata(path).and_then(|m| m.modified())
        && super::runlog::timestamp(changed) > began
    {
        report.issues.push(tr("Ascendant crashed during the run: see crash.log.").to_string());
    }
    Some(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, lines: &[&str]) -> Report {
        let dir = std::env::temp_dir().join(format!("ascendant-health-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.jsonl"), lines.join("\n")).unwrap();
        let report = check(&dir, None).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        report
    }

    const END: &str = r#"{"at":"2026-10-30T20:12:00.000Z","kind":"run_end","vaulted":5}"#;

    #[test]
    fn advice_changing_under_its_mark_is_listed() {
        let r = run("change", &[
            r#"{"at":"2026-10-30T20:00:00.000Z","kind":"read","screen":"Manage","model":{"day":3,"cells":[]}}"#,
            r#"{"at":"2026-10-30T20:00:00.100Z","kind":"view","advice":"End the day","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-10-30T20:00:00.400Z","kind":"view","advice":"Replace the Rat in 04","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-10-30T20:00:01.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            END,
        ]);
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert!(r.issues[0].contains("End the day") && r.issues[0].contains('3'));
    }

    #[test]
    fn a_new_reason_a_click_or_a_blink_is_no_change() {
        let r = run("same", &[
            r#"{"at":"2026-10-30T20:00:00.100Z","kind":"view","advice":"Go to cell 06 (a locked door)","mark":true,"screen":"Patrol"}"#,
            r#"{"at":"2026-10-30T20:00:00.300Z","kind":"view","advice":"Go to cell 06 (an Incident)","mark":true,"screen":"Patrol"}"#,
            r#"{"at":"2026-10-30T20:00:01.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:01.100Z","kind":"view","advice":"Replace the Rat in 04","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-10-30T20:00:01.108Z","kind":"view","advice":"End the day","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-10-30T20:00:02.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            END,
        ]);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert_eq!(r.decisions, 2);
    }

    #[test]
    fn a_click_before_any_advice_and_slow_cards_are_listed() {
        let r = run("slow", &[
            r#"{"at":"2026-10-30T20:00:00.030Z","kind":"game","signal":"CardsUp(3, 0)","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:01.500Z","kind":"view","advice":"End the day","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-10-30T20:00:02.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:03.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            END,
        ]);
        assert_eq!(r.issues.len(), 2, "{:?}", r.issues);
        assert_eq!(r.cards_median, Some(1.5));
    }

    #[test]
    fn a_bite_that_opens_a_treasure_is_not_slow() {
        let r = run("treasure", &[
            r#"{"at":"2026-10-30T19:59:59.500Z","kind":"view","advice":"Chain on into 07","mark":true,"screen":"ChainTarget"}"#,
            r#"{"at":"2026-10-30T20:00:00.000Z","kind":"game","signal":"Bite","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:01.300Z","kind":"game","signal":"TreasureOpen","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:01.340Z","kind":"view","advice":"Chain on into 03","mark":true,"screen":"ChainTarget"}"#,
            r#"{"at":"2026-10-30T20:00:02.000Z","kind":"game","signal":"Bite","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:03.500Z","kind":"view","advice":"Chain on into 05","mark":true,"screen":"ChainTarget"}"#,
            r#"{"at":"2026-10-30T20:00:04.000Z","kind":"game","signal":"Bite","late_ms":30}"#,
            END,
        ]);
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert!(r.issues[0].contains("1.53"), "{:?}", r.issues);
    }

    #[test]
    fn bites_clicked_by_the_chain_badges_are_advised() {
        let r = run("badges", &[
            r#"{"at":"2026-10-30T20:00:00.100Z","kind":"view","advice":null,"mark":false,"screen":"ChainTarget","chain":[9,6,10]}"#,
            r#"{"at":"2026-10-30T20:00:00.800Z","kind":"game","signal":"Bite","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:01.500Z","kind":"game","signal":"Bite","late_ms":30}"#,
            r#"{"at":"2026-10-30T20:00:01.600Z","kind":"view","advice":null,"mark":false,"screen":"ChainTarget","chain":null}"#,
            r#"{"at":"2026-10-30T20:00:02.300Z","kind":"game","signal":"Bite","late_ms":30}"#,
            END,
        ]);
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
    }

    #[test]
    fn a_run_not_played_to_its_end_is_not_checked() {
        let dir = std::env::temp_dir().join(format!("ascendant-health-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.jsonl"), r#"{"at":"2026-10-30T20:00:00.000Z","kind":"view","advice":null}"#).unwrap();
        assert_eq!(check(&dir, None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
