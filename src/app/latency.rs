//! Where the wait goes between a click in the game and Ascendant's advice: a
//! run log's reads and advice, joined with the game's own log
//! (`UE_game.log`), whose click and server reply lines carry the game's
//! times. `ascendant-cli latency` prints it; nothing else reads the game log.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

/// Days from 1970-01-01 to a civil date.
fn days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Milliseconds since 1970 of the first seven numbers in `s`: year, month,
/// day, hour, minute, second, millisecond.
fn millis(s: &str) -> Option<i64> {
    let n: Vec<i64> = s.split(|c: char| !c.is_ascii_digit()).filter(|f| !f.is_empty()).take(7).map(|f| f.parse().ok()).collect::<Option<_>>()?;
    let [y, mo, d, h, mi, sec, ms] = n[..] else { return None };
    Some((((days(y, mo, d) * 24 + h) * 60 + mi) * 60 + sec) * 1000 + ms)
}

/// A game log line's time, UTC: "[2026.10.28-05.26.29:955]...".
pub fn game_millis(line: &str) -> Option<i64> {
    (line.len() >= 25 && line.starts_with('[') && line.as_bytes()[24] == b']').then(|| millis(&line[1..24])).flatten()
}

/// A run log line's time, UTC: "2026-10-28T05:26:17.211Z".
pub fn log_millis(at: &str) -> Option<i64> {
    millis(at)
}

/// The prison clicks (button name, time) and server replies (times) of a game log.
#[derive(Debug, Default)]
pub struct GameEvents {
    pub clicks: Vec<(i64, String)>,
    pub replies: Vec<i64>,
}

const CLICK: &str = "normal-click Btn_S16";
const REPLY: &str = "RecvPushCombineMsg";

pub fn game_events<'a>(lines: impl IntoIterator<Item = &'a str>) -> GameEvents {
    let mut events = GameEvents::default();
    for line in lines {
        let Some(t) = game_millis(line) else { continue };
        if let Some(at) = line.find(CLICK) {
            let name = &line[at + "normal-click ".len()..];
            events.clicks.push((t, name.trim().to_string()));
        } else if line.contains(REPLY) {
            events.replies.push(t);
        }
    }
    events
}

/// The time of the first stamped line that starts at or after `offset`.
fn stamp_after(file: &mut File, offset: u64) -> std::io::Result<Option<i64>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    // The partial line the offset fell in.
    if offset > 0 {
        reader.read_until(b'\n', &mut buf)?;
    }
    for _ in 0..1000 {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            return Ok(None);
        }
        if let Some(t) = game_millis(&String::from_utf8_lossy(&buf)) {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

/// The click and reply lines of the game log between `from` and `to`
/// (milliseconds since 1970): a game log runs to gigabytes, so the start is
/// found by bisection on the line times.
pub fn game_lines(path: &Path, from: i64, to: i64) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let (mut lo, mut hi) = (0u64, file.metadata()?.len());
    while hi - lo > 1 << 16 {
        let mid = lo + (hi - lo) / 2;
        match stamp_after(&mut file, mid)? {
            Some(t) if t < from => lo = mid,
            _ => hi = mid,
        }
    }
    file.seek(SeekFrom::Start(lo))?;
    let mut reader = BufReader::new(file);
    let (mut lines, mut buf) = (Vec::new(), Vec::new());
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let Some(t) = game_millis(&line) else { continue };
        if t > to {
            break;
        }
        if t >= from && (line.contains(CLICK) || line.contains(REPLY)) {
            lines.push(line.trim_end().to_string());
        }
    }
    Ok(lines)
}

/// A read or miss of a run log.
struct Read {
    at: i64,
    screen: String,
    /// The screen and model of a board read (not Unknown), to tell a new one.
    key: Option<String>,
    trigger: Option<String>,
    moved_ms: Option<f64>,
    scan_ms: Option<f64>,
    ahead: bool,
    steps: Vec<f64>,
    drift: Option<f64>,
}

/// The reads and the advice (time, read time) of a run log, and its time span.
fn run_log(text: &str) -> (Vec<Read>, Vec<(i64, i64)>) {
    let (mut reads, mut advice) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else { continue };
        let Some(at) = e["at"].as_str().and_then(log_millis) else { continue };
        let kind = e["kind"].as_str().unwrap_or_default();
        if kind == "advice" {
            if let Some(read_at) = e["read_at"].as_str().and_then(log_millis) {
                advice.push((at, read_at));
            }
            continue;
        }
        if kind != "read" && kind != "miss" {
            continue;
        }
        let screen = e["screen"].as_str().unwrap_or_default().to_string();
        let key = (kind == "read" && screen != "Unknown").then(|| format!("{screen} {}", e["model"]));
        reads.push(Read {
            at,
            screen,
            key,
            trigger: e["trigger"].as_str().map(str::to_string),
            moved_ms: e["moved_ms"].as_f64(),
            scan_ms: e["scan_ms"].as_f64(),
            ahead: e["ahead"].as_bool() == Some(true),
            steps: e["steps"].as_array().map(|a| a.iter().filter_map(Value::as_f64).filter(|&s| s < f64::from(u16::MAX)).collect()).unwrap_or_default(),
            drift: e["drift"].as_f64(),
        });
    }
    (reads, advice)
}

/// The time span of a run log's reads.
pub fn run_span(text: &str) -> Option<(i64, i64)> {
    let (reads, _) = run_log(text);
    Some((reads.first()?.at, reads.last()?.at))
}

/// Median, 75th and 90th percentile.
fn spread(v: &mut [f64]) -> String {
    if v.is_empty() {
        return "     -".to_string();
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!("{:6.2} {:6.2} {:6.2}", at(0.5), at(0.75), at(0.9))
}

/// The report: the reads by trigger, and with the game's events each
/// click's wait for the next board read and its advice, by step.
pub fn report(text: &str, game: Option<&GameEvents>, each: bool) -> String {
    let (reads, advice) = run_log(text);
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    let boards = reads.iter().filter(|r| r.key.is_some()).count();
    line(format!("{} reads, {} of them boards", reads.len(), boards));
    line(format!("{:<10} {:>5}   moved s: median    p75    p90   scan s: median    p75    p90", "trigger", "reads"));
    let mut by: BTreeMap<&str, Vec<&Read>> = BTreeMap::new();
    for r in &reads {
        by.entry(r.trigger.as_deref().unwrap_or("-")).or_default().push(r);
    }
    for (reason, rs) in &by {
        let mut moved: Vec<f64> = rs.iter().filter_map(|r| r.moved_ms).map(|m| m / 1000.0).collect();
        let mut scan: Vec<f64> = rs.iter().filter_map(|r| r.scan_ms).map(|m| m / 1000.0).collect();
        line(format!("{reason:<10} {:>5}          {}           {}", rs.len(), spread(&mut moved), spread(&mut scan)));
    }
    let settled = by.get("settled").map_or(0, Vec::len);
    let ahead = reads.iter().filter(|r| r.ahead).count();
    line(format!("settled reads that took the frame read ahead: {ahead} of {settled}"));
    for reason in ["animated", "max wait"] {
        let Some(rs) = by.get(reason) else { continue };
        let mut top: Vec<f64> = rs.iter().filter_map(|r| r.steps.iter().copied().reduce(f64::max)).collect();
        let mut drift: Vec<f64> = rs.iter().filter_map(|r| r.drift).collect();
        if !top.is_empty() {
            line(format!("{reason} reads: largest step (blocks) {}, drift {}", spread(&mut top), spread(&mut drift)));
        }
    }
    let mut solve: Vec<f64> = advice.iter().map(|&(at, read)| (at - read) as f64 / 1000.0).collect();
    line(format!("read -> advice s           {}", spread(&mut solve)));

    let Some(game) = game else { return out };
    let (Some(first), Some(last)) = (reads.first(), reads.last()) else { return out };
    let clicks: Vec<&(i64, String)> = game.clicks.iter().filter(|(t, _)| (first.at..=last.at).contains(t)).collect();
    let secs = |ms: i64| ms as f64 / 1000.0;
    let mut cols: [Vec<f64>; 6] = Default::default();
    let mut steps: BTreeMap<(String, String), [Vec<f64>; 3]> = BTreeMap::new();
    let (mut answered, mut late) = (0, 0);
    if each {
        line(String::new());
        line("click                            reply   read  screen          trigger    scan  advice   next".to_string());
    }
    for (i, &&(t, ref button)) in clicks.iter().enumerate() {
        let next = clicks.get(i + 1).map_or(last.at, |c| c.0);
        let reply = game.replies.iter().copied().find(|&r| (t..=t + 2000).contains(&r));
        let before = reads.iter().rev().find(|r| r.at <= t && r.key.is_some()).and_then(|r| r.key.as_ref());
        // The first board read after the click that shows another board, up to just past the next click.
        let new = reads.iter().find(|r| r.at > t && r.at <= next + 600 && r.key.is_some() && r.key.as_ref() != before);
        let adv = new.and_then(|n| advice.iter().find(|a| a.1 == n.at)).map(|a| a.0);
        let button = button.trim_start_matches("Btn_S16Board_").trim_start_matches("Btn_S16");
        if each {
            let rel = |x: Option<i64>| x.map_or("     -".to_string(), |x| format!("{:6.2}", secs(x - t)));
            line(format!(
                "{:<32} {} {} {:<15} {:<9} {} {} {:6.2}",
                button,
                rel(reply),
                rel(new.map(|n| n.at)),
                new.map_or("", |n| &n.screen),
                new.and_then(|n| n.trigger.as_deref()).map_or(String::new(), |r| if new.is_some_and(|n| n.ahead) { format!("{r}*") } else { r.to_string() }),
                new.and_then(|n| n.scan_ms).map_or("     -".to_string(), |s| format!("{:6.2}", s / 1000.0)),
                rel(adv),
                secs(next - t),
            ));
        }
        if let Some(r) = reply {
            cols[0].push(secs(r - t));
        }
        let Some(new) = new else { continue };
        answered += 1;
        cols[1].push(secs(new.at - t));
        if let Some(s) = new.scan_ms {
            cols[3].push(s / 1000.0);
        }
        let step = steps.entry((button.to_string(), new.screen.clone())).or_default();
        step[0].push(secs(new.at - t));
        step[2].push(secs(next - t));
        if let Some(a) = adv {
            cols[2].push(secs(a - t));
            cols[4].push(secs(a - new.at));
            cols[5].push(secs(next - a));
            step[1].push(secs(a - t));
            // A gift or instrument picked is confirmed next: that click
            // answers no advice of its own.
            late += usize::from(next < a && !button.ends_with("OptCell"));
        }
    }
    line(String::new());
    line(format!("{} clicks in the game, {answered} answered by a new board read; the player clicked before the advice {late} times", clicks.len()));
    line(format!("{:<26} median    p75    p90", "seconds"));
    for (name, v) in ["click -> server reply", "click -> board read", "click -> advice", "  the read's scan", "  read -> advice", "advice -> next click"].iter().zip(&mut cols) {
        line(format!("{name:<26} {}", spread(v)));
    }
    line(String::new());
    line(format!("{:<34} {:>3}  read s  advice s  next click s (medians)", "click -> screen", "n"));
    let mut rows: Vec<_> = steps.into_iter().collect();
    rows.sort_by_key(|(_, v)| std::cmp::Reverse(v[0].len()));
    for ((button, screen), mut v) in rows {
        let n = v[0].len();
        let [a, b, c] = &mut v;
        let median = |v: &mut Vec<f64>| spread(v).split_whitespace().next().unwrap_or("-").to_string();
        line(format!("{:<34} {n:>3}  {:>6}  {:>8}  {:>12}", format!("{button} -> {screen}"), median(a), median(b), median(c)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_agree() {
        let game = game_millis("[2026.10.28-05.26.29:955][415]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_RouteCell").unwrap();
        let log = log_millis("2026-10-28T05:26:29.955Z").unwrap();
        assert_eq!(game, log);
        assert_eq!(log_millis("1970-01-01T00:00:01.000Z"), Some(1000));
        assert_eq!(log_millis("2000-03-01T00:00:00.000Z"), Some(951_868_800_000));
        assert_eq!(game_millis("|   +12 [3]"), None);
    }

    #[test]
    fn joins_clicks_with_reads() {
        let game = game_events([
            "[2026.10.28-05.26.29:955][415]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_RouteCell",
            "[2026.10.28-05.26.29:990][420]TLLua: Display: [Game] PushMgr:RecvPushCombineMsg ProtoId = 1056, SynId = 4383 ",
            "[2026.10.28-05.26.33:210][879]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_ActionCell",
        ]);
        assert_eq!(game.clicks.len(), 2);
        assert_eq!(game.replies.len(), 1);
        let log = [
            r#"{"at":"2026-10-28T05:26:29.000Z","kind":"read","screen":"Patrol","model":1,"trigger":"settled","moved_ms":300,"scan_ms":200}"#,
            r#"{"at":"2026-10-28T05:26:31.455Z","kind":"read","screen":"Manage","model":2,"trigger":"settled","moved_ms":900,"scan_ms":150,"ahead":true,"steps":[3,2,1,0],"drift":2}"#,
            r#"{"at":"2026-10-28T05:26:31.555Z","kind":"advice","read_at":"2026-10-28T05:26:31.455Z"}"#,
            r#"{"at":"2026-10-28T05:26:34.000Z","kind":"miss","screen":"Unknown","trigger":"max wait","moved_ms":1100,"scan_ms":250,"steps":[30,25,35,20],"drift":30}"#,
        ]
        .join("\n");
        let text = report(&log, Some(&game), true);
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for part in [
            "2 clicks in the game, 1 answered",
            "RouteCell -> Manage 1 1.50 1.60 3.25",
            "click -> server reply 0.04",
            "click -> board read 1.50",
            "click -> advice 1.60",
            "took the frame read ahead: 1 of 2",
            "max wait reads: largest step (blocks) 35.00",
        ] {
            assert!(flat.contains(part), "{part:?} in {text}");
        }
    }
}
