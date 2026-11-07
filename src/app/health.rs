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
/// A door clicked again within this long of the last, on the same day with
/// nothing in between, is the same move: the game logs every click on a door
/// card still on show (its click sound and `RouteCell` click), but moves the
/// Warden once. Five runs to 2026-11-07 had such a second click 0.15 s to
/// 0.52 s after the first, each listed as a click with no advice; a move's
/// own decision cards come up 0.22 s to 0.77 s on and end the window.
const DOOR_AGAIN: f64 = 1.0;

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

    /// The status bar's line: [`Report::summary`], except that with issues it says a click opens them (the run
    /// check window, see `ui.rs`), which health.txt cannot.
    pub fn status_line(&self) -> String {
        match self.issues.len() {
            0 => self.summary(),
            n => trf!("Run check: {} to look at, click here to see them.", n),
        }
    }

    /// The run check window's first line: [`Report::summary`], except that with issues it names no other place to
    /// look, the issues being listed under it.
    pub fn window_summary(&self) -> String {
        match self.issues.len() {
            0 => self.summary(),
            n => trf!("Run check: {} to look at.", n),
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
    // The last door click (day and log time) while nothing has happened since, and whether the click logged now
    // repeats it (see DOOR_AGAIN): its Decision is then not counted.
    let mut door: Option<(i64, f64)> = None;
    let mut door_again = false;
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
                match name {
                    "Moved" => {
                        door_again = door.is_some_and(|(d, t)| d == day && stamp - t <= DOOR_AGAIN);
                        door = Some((day, stamp));
                    }
                    // The door click's own RouteCell click, logged with its sound.
                    "Decision" if std::mem::take(&mut door_again) => continue,
                    "Decision" => {}
                    // Anything else the game does ends the move.
                    _ => (door, door_again) = (None, false),
                }
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
                // The advice was ready even where its mark was held back only because the game was
                // behind another window: an English run's day 9 (2026-11-07) had its advice 0.33 s
                // after the cards but its mark 11.4 s on, once the game was back in front.
                if mark || r["hidden"] == "behind" {
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
        report.issues.push(match slow_cards.len() {
            1 => trf!("Once the advice came over {} s after the decision cards ({} s).", CARDS_SLOW, format!("{worst:.2}")),
            n => trf!("{} times the advice came over {} s after the decision cards (slowest {} s).", n, CARDS_SLOW, format!("{worst:.2}")),
        });
    }
    if !slow_bites.is_empty() {
        let worst = slow_bites.iter().copied().fold(0.0, f64::max);
        report.issues.push(match slow_bites.len() {
            1 => trf!("Once the advice came over {} s after a bite ({} s).", BITE_SLOW, format!("{worst:.2}")),
            n => trf!("{} times the advice came over {} s after a bite (slowest {} s).", n, BITE_SLOW, format!("{worst:.2}")),
        });
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

    /// Run 2026-11-07_13-46-05 day 9: the advice came 0.33 s after the cards while the game was behind another
    /// window, so its mark showed only 11.4 s on, once the game was back in front. That is not slow advice; one
    /// slow time reads "Once", not "1 times".
    #[test]
    fn advice_ready_behind_another_window_is_not_slow() {
        let r = run("behind", &[
            r#"{"at":"2026-11-07T13:49:30.933Z","kind":"game","signal":"CardsUp(2, 0)","late_ms":15}"#,
            r#"{"at":"2026-11-07T13:49:31.181Z","kind":"view","advice":null,"mark":false,"screen":"Manage"}"#,
            r#"{"at":"2026-11-07T13:49:31.265Z","kind":"view","advice":"End the day","hidden":"behind","mark":false,"screen":"Manage"}"#,
            r#"{"at":"2026-11-07T13:49:42.657Z","kind":"view","advice":"End the day","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-11-07T13:49:43.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            END,
        ]);
        assert!(r.issues.is_empty(), "{:?}", r.issues);
        assert!(r.cards_median.is_some_and(|s| s < 0.5), "{:?}", r.cards_median);
        // Hidden for another reason (no game rect, clicked) still waits for the mark.
        let r = run("clicked", &[
            r#"{"at":"2026-11-07T13:49:30.933Z","kind":"game","signal":"CardsUp(2, 0)","late_ms":15}"#,
            r#"{"at":"2026-11-07T13:49:31.265Z","kind":"view","advice":"End the day","hidden":"rect","mark":false,"screen":"Manage"}"#,
            r#"{"at":"2026-11-07T13:49:42.657Z","kind":"view","advice":"End the day","mark":true,"screen":"Manage"}"#,
            r#"{"at":"2026-11-07T13:49:43.000Z","kind":"game","signal":"Decision","late_ms":30}"#,
            END,
        ]);
        assert_eq!(r.issues.len(), 1, "{:?}", r.issues);
        assert!(r.issues[0].starts_with("Once ") && r.issues[0].contains("11.74"), "{:?}", r.issues);
    }

    /// Run 2026-11-07_11-25-08 day 15: the door card was clicked again 0.52 s after the first click, the Warden moved
    /// once, and the game logged both clicks. The second is the same move, not a click with no advice; a door
    /// clicked after the window, or on another day, still counts.
    #[test]
    fn a_door_clicked_twice_is_one_move() {
        let read = |at: &str, day: u32| format!(r#"{{"at":"{at}","kind":"read","screen":"Patrol","model":{{"day":{day},"cells":[]}}}}"#);
        let game = |at: &str, signal: &str| format!(r#"{{"at":"{at}","kind":"game","signal":"{signal}","late_ms":40}}"#);
        let advice = |at: &str| format!(r#"{{"at":"{at}","kind":"view","advice":"Go to cell 15 (a locked door)","mark":true,"screen":"Patrol"}}"#);
        let lines = [
            read("2026-11-07T11:33:49.223Z", 15),
            advice("2026-11-07T11:33:49.253Z"),
            game("2026-11-07T11:33:49.724Z", "Moved"),
            game("2026-11-07T11:33:49.724Z", "Decision"),
            game("2026-11-07T11:33:50.241Z", "Moved"),
            game("2026-11-07T11:33:50.241Z", "Decision"),
            game("2026-11-07T11:33:50.342Z", "CardsUp(2, 1794051230316)"),
            // Day 16: a door clicked with no advice, then again 1.5 s on: both listed.
            read("2026-11-07T11:34:00.000Z", 16),
            game("2026-11-07T11:34:01.000Z", "Moved"),
            game("2026-11-07T11:34:01.000Z", "Decision"),
            game("2026-11-07T11:34:02.500Z", "Moved"),
            game("2026-11-07T11:34:02.500Z", "Decision"),
            END.to_string(),
        ];
        let r = run("door-twice", &lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(r.issues.len(), 2, "day 15's second click is the same move: {:?}", r.issues);
        assert!(r.issues.iter().all(|i| i.contains("16")), "{:?}", r.issues);
        assert_eq!(r.decisions, 3);
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
