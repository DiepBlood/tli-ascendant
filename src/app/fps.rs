//! The game's frame rate on the Tartarus board, whether Ascendant was solving or not (2026-11-11): a player saying the
//! game stutters with Ascendant open can be checked in the run's health.txt and its report data.
//!
//! Every game log line starts with its time and the engine's frame number, which wraps at 1000:
//! `[2026.11.10-20.22.51:365][978]`. The `TCP Ping Result` lines come about once a second, so the frames between two
//! of them over the time between give the frame rate of that second (lab script fps.py, 2026-11-10: 141 frames a
//! second with no solve, 140 with one). The log follower ([`super::gamelog::follow`]) hands each line to a
//! [`FrameClock`], which keeps the seconds while the board is open (`PageName = S16BoardMain` with `PageRunState`
//! Run, closed by Destory). The session notes each solve's start and end ([`solve_started`], [`solve_ended`]), and a
//! run log takes the seconds no longer waiting for a solve's end every [`NOTE_EVERY`] as an `fps` row
//! ([`take_note`]): `idle` the seconds no solve overlapped, `solving` the others. [`summary`] reads the rows back for
//! health.txt.
//!
//! The cost: two substring searches a log line, the log follower already reading every line; the work for a second
//! is a parse of one line's time and frame.

use crate::lang::trf;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// The game log line written about once a second.
const PING: &str = "TCP Ping Result";
/// The Tartarus board page's open and close lines.
const BOARD_PAGE: &str = "PageName = S16BoardMain ,";
/// Two pings this far apart, in ms, make a second (fps.py): the frame number wraps every 1000 frames, about 7 s at
/// 140 frames a second, and a gap far over a second (a hitch in the log, a loading screen) says little.
const PAIR_MS: (i64, i64) = (500, 1600);
/// How often a run log writes an `fps` row with the seconds measured (as often as the `load` rows).
pub const NOTE_EVERY: Duration = Duration::from_secs(30);
/// A second waits this long after its end before it goes into a row: the log comes 30 to 130 ms behind the clock,
/// and a solve begun in it must have been noted.
const SETTLE_MS: i64 = 2000;
/// Solves ended longer ago than this are forgotten.
const KEEP_MS: i64 = 120_000;

/// One measured second: its span in UTC ms and the frames a second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Second {
    pub from: i64,
    pub to: i64,
    pub fps: f64,
}

/// Follows the game log's lines for the frame rate while the board is open.
#[derive(Debug, Default)]
pub struct FrameClock {
    board: bool,
    last: Option<(i64, u32)>,
}

impl FrameClock {
    /// Takes one log line; gives the second it closes, if any.
    pub fn line(&mut self, line: &str) -> Option<Second> {
        if let Some(rest) = line.split_once(BOARD_PAGE).map(|(_, rest)| rest) {
            self.board = rest.split_once("PageRunState").is_some_and(|(_, state)| state.contains("Run"));
            self.last = None;
            return None;
        }
        if !self.board || !line.contains(PING) {
            return None;
        }
        let (at, frame) = (super::latency::game_millis(line)?, frame(line)?);
        let second = self.last.and_then(|(was, was_frame)| {
            let ms = at - was;
            (PAIR_MS.0 < ms && ms < PAIR_MS.1).then(|| Second { from: was, to: at, fps: f64::from((frame + 1000 - was_frame) % 1000) / (ms as f64 / 1000.0) })
        });
        self.last = Some((at, frame));
        second
    }
}

/// The frame number after a line's time: `[time][ 978]`.
fn frame(line: &str) -> Option<u32> {
    let rest = line.get(25..)?.strip_prefix('[')?;
    let (n, _) = rest.split_once(']')?;
    n.trim().parse().ok().filter(|&n| n < 1000)
}

/// The seconds measured and the solves, waiting for [`take_note`].
#[derive(Default)]
struct Pending {
    seconds: Vec<Second>,
    /// Solves by start and end (None while running), UTC ms.
    solves: Vec<(i64, Option<i64>)>,
    noted: Option<std::time::Instant>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending { seconds: Vec::new(), solves: Vec::new(), noted: None });

fn pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A second the log follower measured.
pub fn note_second(second: Second) {
    let mut p = pending();
    // Nothing takes them while no run log writes: keep the last few minutes only.
    if p.seconds.len() >= 600 {
        p.seconds.remove(0);
    }
    p.seconds.push(second);
}

/// A solve began now.
pub fn solve_started() {
    let mut p = pending();
    let now = now_ms();
    p.solves.retain(|&(_, end)| end.is_none_or(|e| now - e < KEEP_MS));
    // A solve replaced before its end ends now.
    for s in p.solves.iter_mut().filter(|s| s.1.is_none()) {
        s.1 = Some(now);
    }
    p.solves.push((now, None));
}

/// The running solve ended now (done or dropped).
pub fn solve_ended() {
    let now = now_ms();
    for s in pending().solves.iter_mut().filter(|s| s.1.is_none()) {
        s.1 = Some(now);
    }
}

/// Splits `seconds` into those no solve overlapped and the others, each fps to one decimal.
pub fn split(seconds: &[Second], solves: &[(i64, Option<i64>)], now: i64) -> (Vec<f64>, Vec<f64>) {
    let (mut idle, mut solving) = (Vec::new(), Vec::new());
    for s in seconds {
        let fps = (s.fps * 10.0).round() / 10.0;
        if solves.iter().any(|&(start, end)| start < s.to && end.unwrap_or(now) > s.from) {
            solving.push(fps);
        } else {
            idle.push(fps);
        }
    }
    (idle, solving)
}

/// The `fps` row a run log writes when [`NOTE_EVERY`] has passed since the last and seconds are ready: those ended
/// [`SETTLE_MS`] ago or more. The first call only starts the clock.
pub fn take_note() -> Option<(SystemTime, &'static str, Value)> {
    if cfg!(test) {
        return None;
    }
    let mut p = pending();
    let due = p.noted.is_none_or(|at| at.elapsed() >= NOTE_EVERY);
    if !due || p.seconds.is_empty() {
        p.noted.get_or_insert_with(std::time::Instant::now);
        return None;
    }
    p.noted = Some(std::time::Instant::now());
    let now = now_ms();
    let ready = p.seconds.iter().take_while(|s| s.to <= now - SETTLE_MS).count();
    let seconds: Vec<Second> = p.seconds.drain(..ready).collect();
    let (idle, solving) = split(&seconds, &p.solves, now);
    (!seconds.is_empty()).then(|| (SystemTime::now(), "fps", json!({ "idle": idle, "solving": solving })))
}

/// The board's frame rate over a run's `fps` rows: (median, seconds) with no solve, and while solving.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub idle: Option<(f64, usize)>,
    pub solving: Option<(f64, usize)>,
}

impl Summary {
    /// The health.txt line; None with no second measured.
    pub fn line(&self) -> Option<String> {
        let (median, n) = self.idle.or(self.solving)?;
        let board = trf!("Game FPS on the board: median {} ({} s)", format!("{median:.0}"), n);
        Some(match (self.idle, self.solving) {
            (Some(_), Some((m, k))) => trf!("{}; while Ascendant solved: median {} ({} s).", board, format!("{m:.0}"), k),
            (Some(_), None) => trf!("{}; no second measured while Ascendant solved.", board),
            _ => trf!("Game FPS on the board while Ascendant solved: median {} ({} s).", format!("{median:.0}"), n),
        })
    }
}

/// [`Summary`] of a run log's rows.
pub fn summary(rows: &[Value]) -> Summary {
    let (mut idle, mut solving) = (Vec::new(), Vec::new());
    for r in rows.iter().filter(|r| r["kind"] == "fps") {
        idle.extend(r["idle"].as_array().into_iter().flatten().filter_map(Value::as_f64));
        solving.extend(r["solving"].as_array().into_iter().flatten().filter_map(Value::as_f64));
    }
    Summary { idle: median(&mut idle), solving: median(&mut solving) }
}

/// The median and the count; None when empty.
fn median(v: &mut [f64]) -> Option<(f64, usize)> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some((if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping(time: &str, frame: &str) -> String {
        format!("[2026.11.10-{time}][{frame}]GameLog: Display: [Game] TCP Ping Result: 35ms")
    }
    const OPEN: &str = "[2026.11.10-20.22.50:000][100]GameLog: Display: [Game] UIMgr PageName = S16BoardMain , PageRunState = Run";
    const CLOSE: &str = "[2026.11.10-20.22.59:000][100]GameLog: Display: [Game] UIMgr PageName = S16BoardMain , PageRunState = Destory";

    /// Frames over seconds between two pings, the frame number wrapping from 999 to 0.
    #[test]
    fn a_second_counts_its_frames_across_the_wrap() {
        let mut c = FrameClock::default();
        assert_eq!(c.line(OPEN), None);
        assert_eq!(c.line(&ping("20.22.51:365", "978")), None);
        let s = c.line(&ping("20.22.52:369", "121")).expect("a second");
        assert!((s.fps - 143.0 / 1.004).abs() < 1e-9, "{}", s.fps);
        assert_eq!(s.to - s.from, 1004);
        // A padded frame number reads too.
        let s = c.line(&ping("20.22.53:369", " 61")).expect("a second");
        assert!((s.fps - 940.0).abs() < 1e-9, "{}", s.fps);
    }

    /// Pings too far apart (the frame number may have wrapped more than once) or too close make no second, and
    /// nothing counts while the board is closed.
    #[test]
    fn gaps_and_a_closed_board_make_no_second() {
        let mut c = FrameClock::default();
        assert_eq!(c.line(&ping("20.22.51:000", "100")), None);
        assert_eq!(c.line(&ping("20.22.52:000", "240")), None, "board not open yet");
        c.line(OPEN);
        assert_eq!(c.line(&ping("20.22.51:000", "100")), None);
        assert_eq!(c.line(&ping("20.22.58:000", "100")), None, "7 s apart");
        assert_eq!(c.line(&ping("20.22.58:300", "140")), None, "0.3 s apart");
        assert!(c.line(&ping("20.22.59:300", "280")).is_some());
        c.line(CLOSE);
        assert_eq!(c.line(&ping("20.23.00:300", "420")), None, "board closed");
        // Lines with no time or no frame number are skipped.
        c.line(OPEN);
        assert_eq!(c.line("TCP Ping Result"), None);
        assert_eq!(c.line("[2026.11.10-20.23.01:300]TCP Ping Result"), None);
    }

    /// The board page lines and the ping line are the same on the English and the Traditional Chinese client (the
    /// log is the engine's, in English); a Chinese text elsewhere in a line changes nothing.
    #[test]
    fn both_clients_parse() {
        let mut c = FrameClock::default();
        c.line("[2026.11.10-20.22.50:000][100]GameLog: Display: [Game] UIMgr PageName = S16BoardMain , PageRunState = Run 塔羅斯");
        c.line(&ping("20.22.51:000", "100"));
        assert!(c.line(&format!("{} 延遲", ping("20.22.52:000", "240"))).is_some());
    }

    /// A second goes to `solving` when any solve overlapped it, a running one included.
    #[test]
    fn seconds_split_by_the_solves() {
        let s = |from, to| Second { from, to, fps: 140.04 };
        let seconds = [s(0, 1000), s(1000, 2000), s(2000, 3000), s(3000, 4000)];
        let (idle, solving) = split(&seconds, &[(1500, Some(1600)), (3900, None)], 5000);
        assert_eq!((idle, solving), (vec![140.0, 140.0], vec![140.0, 140.0]));
    }

    /// The summary over the rows, and none without rows (no game log, or the board never open).
    #[test]
    fn the_summary_reads_the_rows() {
        assert_eq!(summary(&[]), Summary::default());
        assert_eq!(Summary::default().line(), None);
        let rows = [json!({"kind": "fps", "idle": [141.0, 139.0, 140.0], "solving": [130.0]}), json!({"kind": "load"}), json!({"kind": "fps", "idle": [140.0], "solving": []})];
        let s = summary(&rows);
        assert_eq!(s, Summary { idle: Some((140.0, 4)), solving: Some((130.0, 1)) });
        assert_eq!(s.line().unwrap(), "Game FPS on the board: median 140 (4 s); while Ascendant solved: median 130 (1 s).");
        let idle_only = Summary { solving: None, ..s };
        assert_eq!(idle_only.line().unwrap(), "Game FPS on the board: median 140 (4 s); no second measured while Ascendant solved.");
    }
}
