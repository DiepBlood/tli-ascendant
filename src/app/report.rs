//! Report data export (2026-11-05): packs one run's log into a zip the player can hand to the developers;
//! the player looks it over before it is saved. Nothing goes over the network.
//!
//! 1. Picking a run: Tartarus (`<app_dir>\runs`) and Vorax (`<app_dir>\vorax`) runs are both listed, the latest first.
//! 2. Suspicious moments come from log.jsonl, each with a reason (Tartarus: [`tartarus_moments`]; Vorax follows
//!    1a-1o of the Vorax report-data notes (2026-11-05, private): [`vorax_moments`]).
//! 3. Screenshots: about 5 s around each moment and one per move; Tartarus samples observe\ when observe mode was on.
//! 4. Images (the rules the user settled on 2026-11-05):
//!    - A Tartarus prison board the reader recognised (instrument pick, decision cards, treasure pick included), or a
//!      Vorax choice or result screen: the whole frame, shrunk to 1280 wide and re-encoded.
//!    - Every whole frame, before any cut, scaling or encoding, gets its corners blacked out in the pixels
//!      ([`mask_ids`]): the bottom right, where map and battle screens show the account UID, and the bottom
//!      left, where the Tartarus prison shows a faint server and id line. Neither covers anything the review needs.
//!    - The Vorax endgame battle: only the already cut frame-battle-*.jpg, never a whole frame.
//!    - Any other screen (read misses, unknown screens, town, map, riot...): cut to the board only; when no board
//!      shows even then, it stays out of the zip and report.txt says so.
//! 5. log.jsonl: [`super::share::sanitize`] first, then every `kind == "window"` line goes (it holds the game window's title);
//!    local paths become `<路徑>`, the Windows user name `<使用者>`, and the install id goes ([`clean_log`]); a run
//!    of 15 or more digits (the shape of the game's player ids) goes too ([`scrub_long_numbers`]), and crash.log's
//!    lines are cut before any game log text they quote ([`crash_message`]).
//! 6. The zip only holds log.jsonl, .jpg and report.txt, at most [`LIMIT`]; over it, [`LEVELS`] degrade in order:
//!    one image every two moves -> 3 s around a moment instead of 5 -> lower resolution -> lower JPEG quality; still over,
//!    the moments' images go from P3 up, and the moves' images last ([`fit`]).
//! 7. The export runs on a background thread and can be cancelled; its contents are listed first, and only "Save and
//!    open folder" writes the zip beside Ascendant.exe (or in [`super::settings::app_dir`] when that folder can't be
//!    written; see [`save_dir`]), then selects it in File Explorer.
//! 8. Times on screen are local (the folder names and the log are UTC, see [`run_time`]); report.txt stays in UTC.

use super::settle::{self, Outcome, Settled};
use super::theme::{self, ACCENT, MUTED};
use crate::lang::{self, Lang, tr, trf};
use crate::reader::{REF_H, REF_W, Rect, Rgba};
use eframe::egui::{self, RichText};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

/// The zip's size limit.
pub const LIMIT: u64 = 20 * 1024 * 1024;
/// The file names allowed in the zip.
pub const ALLOWED: [&str; 3] = ["log.jsonl", ".jpg", "report.txt"];
/// How far apart a moment's frames are taken (ms): one a second within N s either side.
const SAMPLE_MS: i64 = 1000;
/// How far a Vorax moment's line may be from the nearest frame when none falls in its window (ms), either way
/// (the Vorax spec). A Tartarus moment looks back [`STEP_BACK_MS`] (its frames are kept only when the board
/// changes) and [`NEAREST_MS`] ahead.
const NEAREST_MS: i64 = 5000;
/// How far back the frame of a move is looked for (ms).
const STEP_BACK_MS: i64 = 120_000;
/// An observe frame counts as a board only with read lines this close before and after it (ms).
const OBSERVE_NEAR_MS: i64 = 3000;
/// The Tartarus screens (a `read` line's screen) that are the prison board: sent whole.
const BOARD_SCREENS: [&str; 9] = ["Patrol", "Manage", "ChainTarget", "InstrumentPick", "TreasurePick", "PrisonStatus", "Interrogation", "CellPick", "DayBanner"];

/// The account UID block at the bottom right of map and battle screens, as fractions of the frame's height
/// (width, height), kept to the bottom right corner: the HUD scales with the height, so a 21:9 frame keeps it
/// as wide in pixels. Measured on trainingdata/vorax/frames/t132.00-battle.png (2560x1440): the four lines
/// (season, date, character name and number, UID) span x 0.818-0.983 and y 0.903-0.968 of the frame;
/// with a margin the block starts at x 0.79 and y 0.885 of a 16:9 frame.
pub const UID_RIGHT: (f32, f32) = (0.375, 0.115);
/// The chat box of map and battle screens, kept to the bottom left corner, in the same height fractions
/// (width, height), reaching the bottom edge (a smaller interface scale puts it nearer the corner). The panel
/// has no drawn frame: its lines wrap inside a fixed width. On trainingdata/vorax/frames/t132.00-battle.png and
/// frames2/t160.00-battle-wave3.png (2560x1440) its icons and lines span x 10-333 and y 1208-1418; a wrapped
/// line ends at most one character (about 14 px) short of the wrap width, so the panel ends by x 347. With 2% of
/// that as margin: 354 px wide (0.246 of the height), from y 0.80 of the height down.
pub const CHAT_LEFT: (f32, f32) = (0.246, 0.20);
/// The faint server and id line of the Tartarus prison (the server name `亞洲` over a GUID-like id): x 22-420 and
/// y 921-960 of 1078 on 2026-11-04 21-15-31 frame-0219 and 2026-11-03 21-12-52 frame-0019, so x 29-561 and y
/// 1230-1282 at 2560x1440. Measured again on 2026-11-05 (frame-0057, frame-0254): x 25-580 and y 1220-1290 at
/// 2560x1440. With a margin: 588 px wide (0.408 of the height, 8 px past 580), from y 0.845 of the height down to the
/// bottom.
/// Its right edge stays left of the first decision card of a three card day (x 594 at 2560x1440, measured on
/// 2026-11-04 21-15-31 frame-0221 and the Hunt day 2026-11-04 08-29-19 frame-0068).
pub const ID_LINE_LEFT: (f32, f32) = (0.408, 0.155);
/// The id line's measured right edge and the first decision card's left edge at 2560x1440 (see [`ID_LINE_LEFT`]).
pub const ID_LINE_TEXT_RIGHT: f32 = 580.0;
pub const FIRST_CARD_LEFT: f32 = 594.0;

/// The blocks [`mask_ids`] blacks out on a `w` x `h` frame: the bottom right UID, the bottom left chat box and
/// the bottom left id line, each reaching the frame's corner.
pub fn id_rects(w: usize, h: usize) -> [Rect; 3] {
    let (wf, hf) = (w as f32, h as f32);
    let corner = |(fw, fh): (f32, f32)| ((fw * hf).ceil().min(wf) as i32, (fh * hf).ceil().min(hf) as i32);
    let (rw, rh) = corner(UID_RIGHT);
    let (cw, ch) = corner(CHAT_LEFT);
    let (lw, lh) = corner(ID_LINE_LEFT);
    [Rect::new(w as i32 - rw, h as i32 - rh, rw, rh), Rect::new(0, h as i32 - ch, cw, ch), Rect::new(0, h as i32 - lh, lw, lh)]
}

/// Blacks out [`id_rects`] in the pixels of a whole frame. Every whole frame goes through this before it is
/// cut, scaled or encoded; there is no way to turn it off.
pub fn mask_ids(img: &mut Rgba) {
    for r in id_rects(img.w, img.h) {
        blacken(img, r);
    }
}

/// What a Tartarus "board only" cut keeps (reference 1918x1078, in [`crate::reader::layout`]'s layout):
/// everything above y 915 (days, titles, objective, instruments, the ring, the vault, every kind of card), and the
/// bottom middle (the destination cards' feet, Re-roll, Confirm, x 600-1500). The UID at the bottom left and Quick Action at the bottom right are left out.
pub const TARTARUS_KEEP: [Rect; 2] = [Rect::new(0, 0, REF_W as i32, 915), Rect::new(600, 915, 900, 90)];

/// What a Vorax choice screen "board only" cut keeps (reference 2560x1440, from the relative coordinates in
/// section 2 of the Vorax report-data notes (2026-11-05, private); the tanks 30 px wider each side so the outer tank
/// labels are whole): the left panel, the tanks and labels, the held instruments, the cards, the tooltip.
pub const VORAX_KEEP: [Rect; 5] = [
    Rect::new(13, 439, 302, 612),
    Rect::new(354, 86, 2160, 547),
    Rect::new(1997, 22, 486, 94),
    Rect::new(563, 893, 1690, 518),
    super::vorax_learn::TOOLTIP,
];

/// Which game mode a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Game {
    Tartarus,
    Vorax,
}

impl Game {
    pub fn name(self) -> &'static str {
        match self {
            Game::Tartarus => tr("Tartarus"),
            Game::Vorax => tr("Vorax"),
        }
    }
}

/// A run in the list.
#[derive(Clone, Debug, PartialEq)]
pub struct RunEntry {
    pub dir: PathBuf,
    pub name: String,
    pub game: Game,
    /// The size of every file in the folder, observe\ included.
    pub bytes: u64,
}

/// What is done with an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Treat {
    /// A board screen: the whole frame shrunk to the width limit (Tartarus' UID blacked out).
    Full(Game),
    /// A screen not known to be a board: cut to the board, left out when no board shows.
    Crop(Game),
    /// An image already cut (Vorax frame-battle, observe mode's region crops): only re-encoded.
    AsIs,
}

/// An image in a run's folder: its time (UTC ms), path relative to the folder and treatment.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    pub ms: i64,
    pub file: String,
    pub treat: Treat,
}

/// A suspicious moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Moment {
    pub ms: i64,
    /// The line's `at`.
    pub at: String,
    pub reason: String,
    /// 1 matters most; over the size limit, images go from 3 up.
    pub priority: u8,
    /// Images the line itself names (relative paths), always included.
    pub files: Vec<String>,
    /// A Vorax endgame battle moment: only frame-battle-*.jpg.
    pub battle: bool,
}

/// One degrade level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    /// One image every this many moves.
    pub step_every: usize,
    /// Seconds either side of a moment.
    pub window_s: i64,
    /// The images' width limit.
    pub width: usize,
    pub quality: u8,
}

/// The levels tried in order: one image every two moves -> 3 s instead of 5 -> lower resolution -> lower JPEG quality.
pub const LEVELS: [Level; 7] = [
    Level { step_every: 1, window_s: 5, width: 1280, quality: 80 },
    Level { step_every: 2, window_s: 5, width: 1280, quality: 80 },
    Level { step_every: 2, window_s: 3, width: 1280, quality: 80 },
    Level { step_every: 2, window_s: 3, width: 960, quality: 80 },
    Level { step_every: 2, window_s: 3, width: 720, quality: 80 },
    Level { step_every: 2, window_s: 3, width: 720, quality: 65 },
    Level { step_every: 2, window_s: 3, width: 720, quality: 50 },
];

/// Over the limit at the last level, images are dropped in steps: P3 moments', P2's, the moves', P1's.
/// `drop` is the step reached (0 = none dropped).
pub const DROPS: u8 = 4;

/// The (level, drop) combinations in the order they are tried: every level first, then each drop step at the
/// last level.
pub fn attempts() -> Vec<(usize, u8)> {
    let last = LEVELS.len() - 1;
    (0..LEVELS.len()).map(|k| (k, 0)).chain((1..=DROPS).map(|d| (last, d))).collect()
}

/// The first (level, drop) that fits, in [`attempts`] order. `size` gives the zip size of a combination.
/// None when nothing fits.
pub fn fit(limit: u64, mut size: impl FnMut(&Level, u8) -> u64) -> Option<(usize, u8)> {
    attempts().into_iter().find(|&(k, d)| size(&LEVELS[k], d) <= limit)
}

// ───────────────────────── Picking a run ─────────────────────────

/// A folder's size, subfolders included.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map_or(0, |m| m.len()),
        })
        .sum()
}

/// The finished Tartarus and Vorax runs under `app_dir`, newest first. Only folders holding a finished game count
/// (the same test as the run log's keep rule, [`super::runlog::finished`]): the folders from opening the app, the
/// wait before a game and the time after one, and games given up, are left out. Reads every log whole, so it runs
/// on a background thread ([`ReportUi::refresh`]).
pub fn list_runs(app_dir: &Path) -> Vec<RunEntry> {
    list_runs_known(app_dir, &Mutex::new(HashSet::new()))
}

/// [`list_runs`] with `known`, the folders already found finished: a finished game never changes, so only the
/// folders not in it have their log read (one listing after each game would otherwise read every log again, hundreds
/// of MB with "Keep all run logs" on); the folders found finished now are added. Kept in memory only.
pub fn list_runs_known(app_dir: &Path, known: &Mutex<HashSet<PathBuf>>) -> Vec<RunEntry> {
    let mut runs = Vec::new();
    for (sub, game) in [("runs", Game::Tartarus), ("vorax", Game::Vorax)] {
        let Ok(entries) = std::fs::read_dir(app_dir.join(sub)) else { continue };
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if !e.file_type().is_ok_and(|t| t.is_dir()) || !super::runlog::is_run_folder(&name) {
                continue;
            }
            let dir = e.path();
            let seen = known.lock().is_ok_and(|k| k.contains(&dir));
            if !seen {
                if !super::runlog::finished(&dir, game == Game::Vorax) {
                    continue;
                }
                if let Ok(mut k) = known.lock() {
                    k.insert(dir.clone());
                }
            }
            runs.push(RunEntry { bytes: dir_size(&e.path()), dir: e.path(), name, game });
        }
    }
    runs.sort_by(|a, b| b.name.cmp(&a.name));
    runs
}

impl RunEntry {
    /// The entry for the run folder `dir`, its game told by the folder it is in (`vorax` or else Tartarus).
    pub fn of(dir: PathBuf) -> RunEntry {
        let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let game = if dir.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "vorax") { Game::Vorax } else { Game::Tartarus };
        RunEntry { bytes: dir_size(&dir), dir, name, game }
    }

    /// How the run is named in a run list: "2026-11-04 21:15:31 · Tartarus · 12.3 MB".
    pub fn label(&self) -> String {
        format!("{} \u{b7} {}", self.title(), size_text(self.bytes))
    }

    /// The label without the size, "2026-11-04 21:15:31 · Tartarus": the Run tab's closed run picker, which stays one
    /// line at the narrowest window, as it does with "Latest run".
    pub fn title(&self) -> String {
        format!("{} \u{b7} {}", run_time(&self.name), self.game.name())
    }
}

/// "12.3 MB", "420 KB".
pub fn size_text(bytes: u64) -> String {
    if bytes >= 1 << 20 { format!("{:.1} MB", bytes as f64 / (1 << 20) as f64) } else { format!("{} KB", bytes.div_ceil(1024)) }
}

/// A run folder's name (UTC) "2026-11-04_13-15-31" in local time, "2026-11-04 21:15:31" at UTC+8 (the folder
/// names and the zip's file name stay UTC).
pub fn run_time(name: &str) -> String {
    let offset = folder_utc(name).map_or(0, local_offset);
    run_time_at(name, offset)
}

/// [`run_time`] at a fixed offset from UTC in seconds; a name that is not a run folder's is given back as it is.
pub fn run_time_at(name: &str, offset: i64) -> String {
    let Some(utc) = folder_utc(name) else { return name.to_string() };
    let local = super::runlog::folder_name(std::time::UNIX_EPOCH + std::time::Duration::from_secs((utc + offset).max(0) as u64));
    format!("{} {}", &local[..10], local[11..19].replace('-', ":"))
}

/// The seconds since 1970 (UTC) of a run folder's name "YYYY-MM-DD_HH-MM-SS" (a suffix after it is ignored).
pub fn folder_utc(name: &str) -> Option<i64> {
    let b = name.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'_' || b[13] != b'-' || b[16] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| name.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

/// The local time's offset from UTC (seconds) at `utc` (seconds since 1970), as Windows converts a file time to
/// local time; 0 when it can't tell.
pub fn local_offset(utc: i64) -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
    // File times count 100 ns from 1601-01-01.
    let ticks = (utc + 11_644_473_600) as u64 * 10_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let mut local = FILETIME::default();
    // SAFETY: both pointers are to FILETIMEs on this stack frame.
    if unsafe { FileTimeToLocalFileTime(&ft, &mut local) }.is_err() {
        return 0;
    }
    let local_ticks = (u64::from(local.dwHighDateTime) << 32) | u64::from(local.dwLowDateTime);
    (local_ticks as i64 - ticks as i64) / 10_000_000
}

/// A log time "2026-11-04T13:16:37.123Z" as the local clock "21:16:37" (UTC+8), for the screen.
fn local_clock(at: &str) -> String {
    let Some(ms) = super::latency::log_millis(at) else { return clock(at).to_string() };
    let secs = ms.div_euclid(1000);
    let local = secs + local_offset(secs);
    let local = super::runlog::folder_name(std::time::UNIX_EPOCH + std::time::Duration::from_secs(local.max(0) as u64));
    local[11..19].replace('-', ":")
}

// ───────────────────────── The log ─────────────────────────

/// A line of the log and its time.
struct Row {
    ms: i64,
    v: Value,
}

fn rows(text: &str) -> Vec<Row> {
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Some(Row { ms: super::latency::log_millis(v["at"].as_str()?)?, v }))
        .collect()
}

fn kind(v: &Value) -> &str {
    v["kind"].as_str().unwrap_or("")
}

fn at(v: &Value) -> String {
    v["at"].as_str().unwrap_or_default().to_string()
}

/// A JSON value as plain text: a string without its quotes, null as nothing, anything else as JSON.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Digits replaced by N, so a warning with other numbers counts as the same one.
fn pattern(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut digit = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !digit {
                out.push('N');
            }
            digit = true;
        } else {
            out.push(c);
            digit = false;
        }
    }
    out
}

/// Tartarus' other sources: the review (largest loss), the run check's issues, crash.log's crashes (UTC ms and first line).
#[derive(Clone, Debug, Default)]
pub struct Extra {
    pub worst: Option<settle::Step>,
    pub health: Vec<String>,
    pub crashes: Vec<(i64, String)>,
    /// Moves made against the advice: (read time, advice, move made).
    pub differs: Vec<(String, String, String)>,
}

// ───────────────────────── Tartarus: normal passing states (1.0.2) ─────────────────────────

/// A kind of Tartarus log line that is a normal passing state of the reader or the game, not a suspicious moment:
/// the session shows the last reading, or reads once more, until the screen settles. Such lines are only counted, in
/// one line of report.txt with no images ([`TartarusMoments::summary`]); a run with more of one kind than its
/// [`Calm::limit`] lists that kind once, P3, as unusually many.
pub struct Calm {
    /// Its name in the summary line (English, translated with [`tr`]).
    pub label: &'static str,
    /// Its lines' English sentences (`{}` stands for any text), matched in English and in Chinese ([`is_line`]).
    pub lines: &'static [&'static str],
    /// The Chinese older builds logged for its lines, since changed in the table (`{}` stands for any text): runs
    /// logged before the change are matched by it as it was.
    pub legacy: &'static [&'static str],
    /// Only on a read whose screen is Unknown (a board read between two steps); on a decision screen the same line
    /// is a failure and stays a moment.
    pub unknown_only: bool,
    /// The most of it a normal run has: the 95th percentile of its count over the 30 newest finished runs on
    /// 2026-11-07 (2026-11-04 to 11-06, tmp_png\v102\moments-stats.txt), at least [`CALM_FLOOR`].
    pub limit: usize,
}

/// No kind of normal passing state is unusual this few times in a run, whatever its percentile.
pub const CALM_FLOOR: usize = 3;

/// The normal passing states, each with why it is not suspicious. Up to 1.0.1 each was a moment of its own (a miss
/// P2, a read warning P3) with 4 to 7 images: in 2026-11-06_16-17-31 they were 15 of its 35 moments, and the
/// low-confidence warnings ([`LOW_CONFIDENCE`]) 9 more.
pub const CALM: [Calm; 13] = [
    // The session holds a changed board for one more read before it advises on it (session.rs CHANGED): every move's
    // result is read this way.
    Calm { label: "board changed, read again to confirm", lines: &["The board changed: reading it again to confirm."], legacy: &[], unknown_only: false, limit: 51 },
    // A game dialog over the prison (a reward, a confirmation): the last reading stays until the player closes it.
    Calm { label: "popup over the game", lines: &["A popup covers the game: showing the last reading."], legacy: &[], unknown_only: false, limit: 40 },
    // The day banner and the other screens between the prison's steps: nothing to read, the last reading stays.
    Calm { label: "prison board off screen", lines: &["The prison board is not on screen: showing the last reading."], legacy: &[], unknown_only: false, limit: 22 },
    // The bite's animation: the target screen catches up within session.rs BITE_SHOWS (1.5 s).
    Calm { label: "last bite not drawn yet", lines: &["The board does not show the last bite yet: showing the last reading."], legacy: &[], unknown_only: false, limit: 29 },
    // A Replace's flash and fade-in: the new inmate shows about 0.45 s after the click (session.rs REPLACE_SHOWS).
    Calm { label: "replaced inmate not drawn yet", lines: &["The new inmate is not on show yet: showing the last reading."], legacy: &[], unknown_only: false, limit: 19 },
    // The day-break effects: the board is held at most session.rs DAY_SHOWS (1 s).
    Calm { label: "new day not drawn yet", lines: &["The new day is not on the board yet: showing the last reading."], legacy: &[], unknown_only: false, limit: 3 },
    // The board, the decision cards or an offer fading in after a screen change (session.rs FADING, CARDS_FADING,
    // OFFER_PARTIAL); a short offer read the same for OFFER_STEADY stands.
    Calm {
        label: "screen still coming in",
        lines: &[
            "The board is still coming in: showing the last reading.",
            "The decision cards are still coming in: showing the last reading.",
            "The offer is still coming in: showing the last reading.",
        ],
        legacy: &[],
        unknown_only: false,
        limit: 2,
    },
    // A Warden's Treasure or instrument pick covers the board by design: its cells come from the last board read.
    // Until 2026-11-04 its Chinese was another (the backups' zh_tw.rs; git's history starts after it).
    Calm {
        label: "pick covering the board",
        lines: &["The pick covers the board: the cells are from the last board read (day {})."],
        legacy: &["選擇畫面蓋住了盤面：牢房取自上次讀到的盤面（第 {} 天）。"],
        unknown_only: false,
        limit: 144,
    },
    // The pick screens cover the days counter: the count from the last read stands.
    Calm { label: "days counter covered", lines: &["The days counter is hidden: {} day(s) until Riot kept from the last read."], legacy: &[], unknown_only: false, limit: 116 },
    // A board read between two steps (Unknown screen: the destination cards or decision buttons are not up yet), on
    // which no advice is given. On a patrol screen the same lines are a failure, so only on Unknown.
    Calm {
        label: "board read between steps",
        lines: &[
            "No destination cards or decision buttons on screen: take the screenshot when they show.",
            "Mark today's doors: tick them under \u{201c}Select a Destination\u{201d}, or select a cell and press Today's door.",
        ],
        legacy: &[],
        unknown_only: true,
        limit: 184,
    },
    // The mouse over a cell shows the game's tooltip: the cells under it keep the last board read.
    Calm { label: "tooltip over cells", lines: &["A tooltip covers {}: {} kept from the last board read."], legacy: &[], unknown_only: false, limit: 3 },
    // The eater still changes after its bite (up to 2.8 s, session.rs NO_ADVICE_NOTICE): the targets the game marks
    // stand in, and the board is read again once the screen stops.
    Calm {
        label: "eater still changing after its bite",
        lines: &[
            "The eater in cell {} still changes after its bite: taken as the {} the targets the game marks call for, until it reads.",
            "The eater in cell {} has nothing to Consume on the board read, but the game marks targets: it may still be changing after its bite, and the board is read again once the screen stops. If this stays, check the eater and the marked cells.",
        ],
        legacy: &[],
        unknown_only: false,
        limit: 9,
    },
    // Screens with nothing to solve: the run's theme scroll, and the wait after the run.
    Calm {
        label: "screen with nothing to solve",
        lines: &["The run's theme: {}.", "The run's theme: {}. Nothing to solve on this screen.", "The run is over: waiting for the next run."],
        legacy: &[],
        unknown_only: false,
        limit: 0,
    },
];

/// The reader's low-confidence warnings ("... (confidence 0.xx)"): the read went on with its best guess. One on a
/// read between steps (Unknown screen, no advice given) or on a single read is a frame caught mid-animation or a
/// cell in passing, and the next reads settle it. Such a warning is a moment only when it persists, [`LOW_STREAK`]
/// reads in a row on a decision screen, or when a very low one ([`VERY_LOW`]) is followed by another reading of its
/// cell on the same screen and day: the guess was wrong while it showed. The last one is the warden's.
const LOW_CONFIDENCE: [&str; 7] = [
    "Check cell {}: read as {} (confidence {}).",
    "Check cell {}: {} marker read with confidence {}.",
    "Check cell {}: the glow behind the locked door read as {} (confidence {}).",
    "Check cell {}: beast read as {} (confidence {}).",
    "Check cell {}: rarity read as {} (confidence {}).",
    "Check cell {}: read as an empty cell (confidence {}).",
    "Check the warden: read at cell {} (confidence {}).",
];
/// Reads in a row a low-confidence warning of the same cell must be in to persist.
const LOW_STREAK: usize = 2;
/// A confidence this low that the next read contradicts is a moment (the reader warns below 0.3; a quarter of the
/// warnings in the 30 runs were 0.05 or lower, half 0.16 or lower).
const VERY_LOW: f32 = 0.1;
/// The summary's name for the low-confidence warnings left out.
const LOW_LABEL: &str = "low-confidence cell reads (between steps, or not repeated)";

/// A low-confidence warning's sentence (index in [`LOW_CONFIDENCE`]) and cell (index in the model's cells; None
/// for the warden).
#[derive(Clone, Copy, Debug, PartialEq)]
struct LowKey {
    sentence: usize,
    cell: Option<usize>,
}

/// A read's low-confidence warning: its key, confidence and text.
type LowWarning = (LowKey, f32, String);

/// Whether `text` fits `template`: its fixed parts in order, the first at the start and the last at the end, any
/// text for each `{}` or `{N}` (templates here have no literal braces).
fn fits(text: &str, template: &str) -> bool {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else { break };
        parts.push(&rest[..open]);
        rest = &rest[open + close + 1..];
    }
    parts.push(rest);
    if parts.len() == 1 {
        return text == template;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if text.len() < first.len() + last.len() || !text.starts_with(first) || !text.ends_with(last) {
        return false;
    }
    let mut middle = &text[first.len()..text.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match middle.find(p) {
            Some(i) => middle = &middle[i + p.len()..],
            None => return false,
        }
    }
    true
}

/// Whether a logged line is the English sentence `en` (with its `{}` filled), in English or in Chinese: the log keeps
/// the language the window had then, so lines are told apart by their English key, never by the Chinese alone.
fn is_line(text: &str, en: &'static str) -> bool {
    Lang::ALL.iter().any(|&l| fits(text, lang::tr_in(l, en)))
}

/// The normal passing state (index in [`CALM`]) a miss line or read warning is, if any. `unknown`: on a read whose
/// screen is Unknown.
fn calm_kind(text: &str, unknown: bool) -> Option<usize> {
    CALM.iter().position(|c| (unknown || !c.unknown_only) && (c.lines.iter().any(|l| is_line(text, l)) || c.legacy.iter().any(|l| fits(text, l))))
}

/// A low-confidence warning's key and confidence (its last number), if `text` is one.
fn low_confidence(text: &str) -> Option<(LowKey, f32)> {
    let sentence = LOW_CONFIDENCE.iter().position(|t| is_line(text, t))?;
    let numbers: Vec<&str> = text.split(|c: char| !(c.is_ascii_digit() || c == '.')).map(|s| s.trim_matches('.')).filter(|s| !s.is_empty()).collect();
    let confidence = numbers.last()?.parse().ok()?;
    let cell = match sentence == LOW_CONFIDENCE.len() - 1 {
        true => None,
        false => Some(numbers.first()?.parse::<usize>().ok()?.checked_sub(1)?),
    };
    Some((LowKey { sentence, cell }, confidence))
}

/// Whether `next`, the read after `read`, on the same screen and day, has the key's cell (or the warden) otherwise.
fn flipped(read: &Value, next: Option<&Value>, key: LowKey) -> bool {
    let Some(next) = next else { return false };
    if next["screen"] != read["screen"] || next["model"]["day"] != read["model"]["day"] {
        return false;
    }
    let (a, b) = match key.cell {
        Some(n) => (&read["model"]["cells"][n], &next["model"]["cells"][n]),
        None => (&read["model"]["warden"], &next["model"]["warden"]),
    };
    !a.is_null() && !b.is_null() && a != b
}

/// Tartarus' suspicious moments, and the normal passing states left out of them.
#[derive(Clone, Debug, Default)]
pub struct TartarusMoments {
    pub moments: Vec<Moment>,
    /// The lines left out: the [`CALM`] kinds not unusually many, in its order, then the low-confidence warnings
    /// ([`LOW_LABEL`]); kinds with none are not there.
    pub quiet: Vec<Quiet>,
    /// Each [`CALM`] kind's count in the run, listed as unusual or not.
    pub counts: Vec<usize>,
}

/// A kind of line left out of the moments: its label, count, and the `at` of its first and last line (UTC), so
/// the developer can still find them in log.jsonl.
#[derive(Clone, Debug, PartialEq)]
pub struct Quiet {
    pub label: &'static str,
    pub n: usize,
    pub first: String,
    pub last: String,
}

impl TartarusMoments {
    /// report.txt's line for the lines left out, or None when there are none: each kind with its count and the
    /// times of its first and last line, as the moments list shows them.
    pub fn summary(&self) -> Option<String> {
        let total: usize = self.quiet.iter().map(|q| q.n).sum();
        let items: Vec<String> = self.quiet.iter().map(|q| trf!("{} {} times ({} to {})", tr(q.label), q.n, clock(&q.first), clock(&q.last))).collect();
        (total > 0).then(|| trf!("Also {} normal passing states not listed (no images): {}.", total, items.join(tr(", "))))
    }
}

/// Tartarus' suspicious moments (1.0.2):
/// - `edit` (cells, markers, instruments or theme corrected by hand): P1.
/// - `miss` (a read that failed on a screen other than Unknown): reported once per screen and line, with a count, P2.
///   Unknown means the reader saw no prison screen at all (town, loading), which is no failure.
/// - Read warnings (a `read`'s warnings and problems): the same text with other numbers counts once, with a count, P3.
/// - Misses and read warnings that are a normal passing state ([`CALM`]) are not moments: they are counted in
///   [`TartarusMoments::quiet`]; a kind over its [`Calm::limit`] in the run is listed once, P3, as unusually many.
/// - Low-confidence warnings ([`LOW_CONFIDENCE`]): P3 once per cell and sentence, only when they persist or a very
///   low one is contradicted by the next read; the others are counted in `quiet`.
/// - A move made against the advice (the move `audit` infers): P2.
/// - The review's largest loss, each issue of a failed run check, and crash.log's crashes during the run: P1.
pub fn tartarus_moments(text: &str, extra: &Extra) -> TartarusMoments {
    let rows = rows(text);
    let mut out = Vec::new();
    let moment = |r: &Row, reason: String, priority: u8| Moment { ms: r.ms, at: at(&r.v), reason, priority, files: Vec::new(), battle: false };
    // (screen, line) -> (index of the first line, count)
    let mut misses: Vec<(String, String, usize, usize)> = Vec::new();
    let mut warns: Vec<(String, String, usize, usize)> = Vec::new();
    // Each CALM kind: (count, index of its first line, that line's text, index of its last line).
    let mut calm: Vec<(usize, usize, String, usize)> = vec![(0, 0, String::new(), 0); CALM.len()];
    let note = |calm: &mut [(usize, usize, String, usize)], k: usize, i: usize, line: &str| {
        let c = &mut calm[k];
        if c.0 == 0 {
            (c.1, c.2) = (i, line.to_string());
        }
        c.0 += 1;
        c.3 = i;
    };
    // Each read line: its index and its low-confidence warnings (key, confidence, text).
    let mut reads: Vec<(usize, Vec<LowWarning>)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match kind(&r.v) {
            "edit" => {
                let v = &r.v;
                let reason = if let Some(cell) = v["cell"].as_u64() {
                    trf!("Manual correction: cell {}: {}", cell, v["edit"].as_str().unwrap_or(""))
                } else if let Some(tools) = v.get("tools") {
                    trf!("Manual correction: instruments set to {}", tools.as_array().map(|t| t.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default())
                } else {
                    trf!("Manual correction: theme set to {}", v["theme"].as_str().unwrap_or("-"))
                };
                out.push(moment(r, reason, 1));
            }
            "miss" => {
                let screen = r.v["screen"].as_str().unwrap_or("Unknown");
                if screen == "Unknown" {
                    continue;
                }
                let line = r.v["line"].as_str().unwrap_or("").to_string();
                if let Some(k) = calm_kind(&line, false) {
                    note(&mut calm, k, i, &line);
                    continue;
                }
                match misses.iter_mut().find(|(s, l, _, _)| s.as_str() == screen && l.as_str() == line) {
                    Some(m) => m.3 += 1,
                    None => misses.push((screen.to_string(), line, i, 1)),
                }
            }
            "read" => {
                let unknown = r.v["screen"].as_str().unwrap_or("Unknown") == "Unknown";
                let texts = r.v["warnings"].as_array().into_iter().flatten().chain(r.v["problems"].as_array().into_iter().flatten()).filter_map(|w| w.as_str());
                // A kind counts once per line (a read between steps has both of its sentences).
                let mut kinds: Vec<usize> = Vec::new();
                let mut lows = Vec::new();
                for w in texts {
                    if let Some(k) = calm_kind(w, unknown) {
                        if !kinds.contains(&k) {
                            kinds.push(k);
                            note(&mut calm, k, i, w);
                        }
                        continue;
                    }
                    if let Some((key, confidence)) = low_confidence(w) {
                        lows.push((key, confidence, w.to_string()));
                        continue;
                    }
                    let p = pattern(w);
                    match warns.iter_mut().find(|(k, _, _, _)| *k == p) {
                        Some(m) => m.3 += 1,
                        None => warns.push((p, w.to_string(), i, 1)),
                    }
                }
                reads.push((i, lows));
            }
            _ => {}
        }
    }
    for (screen, line, i, n) in misses {
        out.push(moment(&rows[i], trf!("Read failed ({}): {} ({} times this run)", screen, line, n), 2));
    }
    for (_, text, i, n) in warns {
        out.push(moment(&rows[i], trf!("Read warning: {} ({} times this run)", text, n), 3));
    }
    // Low-confidence warnings: (key, index of the line, text, why it is listed) for the keys listed, at their first
    // line that qualifies; the count is every warning of the key in the run.
    let mut listed: Vec<(LowKey, usize, String, String)> = Vec::new();
    for (j, (i, lows)) in reads.iter().enumerate() {
        let read = &rows[*i].v;
        let unknown = read["screen"].as_str().unwrap_or("Unknown") == "Unknown";
        for (key, confidence, w) in lows {
            let has = |k: usize| reads[k].1.iter().any(|l| l.0 == *key);
            let (mut a, mut b) = (j, j);
            while a > 0 && has(a - 1) {
                a -= 1;
            }
            while b + 1 < reads.len() && has(b + 1) {
                b += 1;
            }
            let why = if unknown {
                None
            } else if b - a + 1 >= LOW_STREAK {
                Some(trf!("in {} reads in a row", b - a + 1))
            } else if *confidence < VERY_LOW && flipped(read, reads.get(j + 1).map(|n| &rows[n.0].v), *key) {
                Some(tr("very low, and the next read differs").to_string())
            } else {
                None
            };
            if let Some(why) = why
                && !listed.iter().any(|l| l.0 == *key)
            {
                listed.push((*key, *i, w.clone(), why));
            }
        }
    }
    let all_lows = || reads.iter().flat_map(|r| r.1.iter());
    for (key, i, text, why) in &listed {
        let n = all_lows().filter(|l| l.0 == *key).count();
        out.push(moment(&rows[*i], trf!("Read warning: {} ({}; {} times this run)", text, why, n), 3));
    }
    // The low-confidence warnings left out: their reads (index of the line, once per warning).
    let low_quiet: Vec<usize> = reads.iter().flat_map(|(i, lows)| lows.iter().filter(|l| !listed.iter().any(|x| x.0 == l.0)).map(move |_| *i)).collect();
    let mut quiet = Vec::new();
    let span = |label: &'static str, n: usize, first: usize, last: usize| Quiet { label, n, first: at(&rows[first].v), last: at(&rows[last].v) };
    for (k, (n, i, first, last)) in calm.iter().enumerate() {
        let limit = CALM[k].limit.max(CALM_FLOOR);
        if *n > limit {
            out.push(moment(&rows[*i], trf!("Unusually many ({}): {} ({} times this run, normally at most {})", tr(CALM[k].label), first, n, limit), 3));
        } else if *n > 0 {
            quiet.push(span(CALM[k].label, *n, *i, *last));
        }
    }
    if let (Some(&first), Some(&last)) = (low_quiet.first(), low_quiet.last()) {
        quiet.push(span(LOW_LABEL, low_quiet.len(), first, last));
    }
    let row_at = |when: &str| super::latency::log_millis(when).map(|ms| Row { ms, v: serde_json::json!({ "at": when }) });
    for (read_at, advised, made) in &extra.differs {
        if let Some(r) = row_at(read_at) {
            out.push(moment(&r, trf!("Move made differs from the advice: advised {}, made {}", advised, made), 2));
        }
    }
    if let Some(w) = &extra.worst
        && let Some(r) = row_at(&w.read_at)
    {
        out.push(moment(&r, trf!("Largest loss in the run review: day {}, made {}, best {}, loss {}", w.day, w.made, w.best, format!("{:.2}", w.loss)), 1));
    }
    // The run check's issues: one with a day goes at that day's first board read, the others at the run's end.
    let end = rows.iter().rev().find(|r| kind(&r.v) == "run_end").or(rows.last());
    for issue in &extra.health {
        let day = day_of(issue);
        let r = day.and_then(|d| rows.iter().find(|r| kind(&r.v) == "read" && r.v["model"]["day"].as_i64() == Some(d))).or(end);
        if let Some(r) = r {
            out.push(moment(r, trf!("Run check: {}", issue), 1));
        }
    }
    let (first, last) = (rows.first().map(|r| r.ms), rows.last().map(|r| r.ms));
    for (ms, line) in &extra.crashes {
        if first.is_some_and(|f| *ms >= f) && last.is_some_and(|l| *ms <= l + 60_000) {
            let when = super::runlog::timestamp(std::time::UNIX_EPOCH + std::time::Duration::from_millis(*ms as u64));
            out.push(Moment { ms: *ms, at: when, reason: trf!("crash.log has a crash at this time: {}", line), priority: 1, files: Vec::new(), battle: false });
        }
    }
    out.sort_by_key(|m| m.ms);
    TartarusMoments { moments: out, quiet, counts: calm.iter().map(|c| c.0).collect() }
}

/// A run check issue's day: "Day 12: ..." or `第 12 天：...` (by the current language's sentence start).
fn day_of(issue: &str) -> Option<i64> {
    let template = tr("Day {}: clicked with no advice on show.");
    let head = template.split("{}").next().unwrap_or("");
    let rest = issue.strip_prefix(head)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// crash.log's entries: (UTC ms, the first line's message, see [`crash_message`]). Each starts "secs version
/// thread '...': message".
pub fn crashes(text: &str) -> Vec<(i64, String)> {
    text.lines()
        .filter_map(|l| {
            let (secs, rest) = l.split_once(' ')?;
            let secs: i64 = secs.parse().ok()?;
            let (_, msg) = rest.split_once(' ')?;
            msg.starts_with("thread '").then(|| (secs * 1000, crash_message(msg)))
        })
        .collect()
}

/// Markers of the game's own log (UE_game.log), which holds the player's ids (`SetGamePlayer`, `playerinfo`): a
/// crash message quoting such a line is cut before it.
const GAME_LOG_MARKS: [&str; 4] = ["setgameplayer", "playerinfo", "ue_game", "logtemp"];

/// A crash message as report.txt may quote it: at most 200 characters, cut before a quoted game log line (its time
/// stamp `[2026.11.05-13.12.57:123]` or one of [`GAME_LOG_MARKS`]), with long numbers removed.
pub fn crash_message(msg: &str) -> String {
    let low = msg.to_ascii_lowercase();
    let mut cut = GAME_LOG_MARKS.iter().filter_map(|m| low.find(m)).min().unwrap_or(msg.len());
    let b = msg.as_bytes();
    // A UE log time stamp: `[` four digits `.` two digits `.` two digits `-`.
    if let Some(i) = (0..b.len().saturating_sub(12)).find(|&i| {
        b[i] == b'[' && b[i + 1..i + 5].iter().all(u8::is_ascii_digit) && b[i + 5] == b'.' && b[i + 8] == b'.' && b[i + 11] == b'-'
    }) {
        cut = cut.min(i);
    }
    let mut kept = msg[..cut].trim_end().to_string();
    if cut < msg.len() {
        kept.push_str(" <\u{904a}\u{6232}\u{65e5}\u{8a8c}\u{5df2}\u{79fb}\u{9664}>");
    }
    let (kept, _) = scrub_long_numbers(&kept);
    kept.chars().take(200).collect()
}

/// Vorax suspicious moments, after section 1 (1a-1o) of the Vorax report-data notes (2026-11-05, private). Lines are
/// always parsed as JSON: `vorax_read`'s reading has a kind of its own (the screen kind).
pub fn vorax_moments(text: &str) -> Vec<Moment> {
    let rows = rows(text);
    let mut out: Vec<Moment> = Vec::new();
    // 1a-1e: boards (signatures) reported already; the others are deduplicated by event and key fields.
    let mut boards: HashSet<String> = HashSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut round: Option<i64> = None;
    // Moments with a 1a (index in `out`, round), and 1g's unmatched tooltip crops (the tooltip line's own round).
    let mut card_moments: Vec<(usize, Option<i64>)> = Vec::new();
    let mut tooltips: Vec<(Option<i64>, String)> = Vec::new();
    let mut wave: Option<i64> = None;
    for r in &rows {
        let v = &r.v;
        let mut push = |key: String, reason: String, priority: u8, files: Vec<String>, battle: bool| {
            if seen.insert(key) {
                out.push(Moment { ms: r.ms, at: at(v), reason, priority, files, battle });
            }
        };
        let frame = |name: &str| v[name].as_str().map(str::to_string);
        match kind(v) {
            "vorax_read" => {
                let reading = &v["reading"];
                if let Some(n) = reading["round"].as_i64() {
                    round = Some(n);
                }
                let choice = matches!(reading["kind"].as_str(), Some("potion" | "instrument" | "protocol"));
                if v["steady"].as_bool() != Some(true) || v["gate"].as_bool() != Some(false) || !choice {
                    continue;
                }
                let sig = v["signature"].as_str().unwrap_or("").to_string();
                // 1a-1e: one moment per board (signature), from the first line of it with something to report.
                if boards.contains(&sig) {
                    continue;
                }
                let mut found: Vec<(String, u8)> = Vec::new();
                let mut unknown_card = false;
                for (k, card) in reading["cards"].as_array().into_iter().flatten().enumerate() {
                    if card["en"].is_null() {
                        let raw = format!("{} [{}]", plain(&card["rawName"]), plain(&card["rawType"]));
                        found.push((trf!("Card {} not recognised: {} ({})", k + 1, raw, plain(&card["why"])), 1));
                        unknown_card = true;
                    }
                }
                for (i, tank) in reading["tanks"].as_array().into_iter().flatten().enumerate() {
                    let partial = ["archetype", "rarity", "activity", "quantity"].iter().any(|f| tank[*f].is_null());
                    if tank["empty"].as_bool() == Some(false) && partial {
                        found.push((trf!("Tank {} only partly read: {} {} {}", i + 1, plain(&tank["rawName"]), plain(&tank["rawValues"]), plain(&tank["rawTotal"])), 1));
                    }
                }
                for (j, held) in reading["held"].as_array().into_iter().flatten().enumerate() {
                    if held["id"].is_null() && held["opening"].as_bool() == Some(false) {
                        found.push((trf!("Held instrument icon {} not recognised", j + 1), 2));
                    }
                }
                if let Some(p) = reading["problems"].as_array().and_then(|p| p.first()).and_then(|p| p.as_str()) {
                    found.push((trf!("Read problem: {}", p), 2));
                }
                if reading["round"].is_null() {
                    found.push((tr("Round not read").to_string(), 3));
                }
                if found.is_empty() {
                    continue;
                }
                boards.insert(sig);
                let priority = found.iter().map(|f| f.1).min().unwrap_or(3);
                let reason = found.into_iter().map(|f| f.0).collect::<Vec<_>>().join("; ");
                if unknown_card {
                    card_moments.push((out.len(), round));
                }
                out.push(Moment { ms: r.ms, at: at(v), reason, priority, files: frame("frame").into_iter().collect(), battle: false });
            }
            "vorax_unknown" => {
                let files = ["frame", "crop"].iter().filter_map(|f| frame(f)).collect();
                let reason = trf!("Observe mode: unknown {}: {}", v["what"].as_str().unwrap_or(""), v["rawName"].as_str().unwrap_or(""));
                push(format!("1f|{}|{}", r.ms, reason), reason, 1, files, false);
            }
            "vorax_tooltip" if v["matched"].is_null() => {
                if let Some(crop) = frame("crop") {
                    tooltips.push((v["round"].as_i64(), crop));
                }
            }
            "vorax_pick_mismatch" => {
                let reason = trf!("Picked instrument does not match the card (card {}, icon id {})", plain(&v["picked"]), plain(&v["icon"]));
                push(format!("1h|{}", r.ms), reason, 1, Vec::new(), false);
            }
            "vorax_pending_stuck" => {
                let s = v["ms"].as_f64().unwrap_or(0.0) / 1000.0;
                push(format!("1i|{}", r.ms), trf!("Board did not settle for {} s", format!("{s:.1}")), 2, Vec::new(), false);
            }
            "vorax_round" if v["phase"].as_str() != Some("reroll") && v["chosen"]["card"].is_null() => {
                push(format!("1j|{}", r.ms), tr("Could not tell which card was picked").to_string(), 3, Vec::new(), false);
            }
            "vorax_end" => {
                let seen_rounds = match &v["roundsSeen"] {
                    Value::Array(a) => !a.is_empty(),
                    Value::Number(n) => n.as_f64().is_some_and(|n| n > 0.0),
                    Value::Null => false,
                    _ => true,
                };
                if v["ultimate"].is_null() && seen_rounds {
                    push(format!("1k|{}", r.ms), tr("Final activity on the result screen not read").to_string(), 2, Vec::new(), false);
                }
            }
            "vorax_battle_end" => {
                let files: Vec<String> = frame("frame").into_iter().collect();
                let reads = v["reads"].as_i64().unwrap_or(0);
                if reads == 0 {
                    push(format!("1l|{}", r.ms), tr("Endgame battle panel never read").to_string(), 1, files.clone(), true);
                }
                if let (Some(board), Some(activity)) = (v["board"].as_f64(), v["activity"].as_f64())
                    && activity < board * 0.95
                {
                    push(format!("1m|{}", r.ms), trf!("Battle activity {} is below the board's final {}", plain(&v["activity"]), plain(&v["board"])), 1, files.clone(), true);
                }
                let share_bad = v["bonusShare"].as_f64().is_some_and(|s| !(0.0..=1.0).contains(&s));
                if reads > 0 && (["bonus", "next", "percent"].iter().any(|f| v[*f].is_null()) || share_bad) {
                    push(format!("1n|{}", r.ms), tr("Battle end numbers incomplete").to_string(), 2, files, true);
                }
                wave = None;
            }
            "vorax_battle" => {
                if let Some(w) = v["wave"].as_i64() {
                    if let Some(before) = wave.filter(|&b| w < b) {
                        push(format!("1o|{}", r.ms), trf!("Battle wave went backwards ({} → {})", before, w), 3, Vec::new(), true);
                    }
                    wave = Some(w);
                }
            }
            _ => {}
        }
    }
    // 1g: never a moment of its own: an unmatched tooltip's crop goes with the 1a of its own round, two at most.
    for (r, crop) in tooltips {
        for &(k, round) in card_moments.iter().filter(|(_, round)| r.is_some() && *round == r) {
            let files = &mut out[k].files;
            if round == r && files.iter().filter(|f| f.contains("-crop")).count() < 2 && !files.contains(&crop) {
                files.push(crop.clone());
            }
        }
    }
    out.sort_by_key(|m| m.ms);
    out
}

// ───────────────────────── The frames ─────────────────────────

/// An observe frame's name obs-HHMMSSmmm.jpg (UTC time) in ms: on the run's start date, or the next day for a time
/// over an hour before the start (past midnight).
fn observe_ms(name: &str, start_ms: i64) -> Option<i64> {
    let digits = name.strip_prefix("obs-")?.strip_suffix(".jpg")?;
    if digits.len() != 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| digits[r].parse::<i64>().ok();
    let of_day = ((n(0..2)? * 60 + n(2..4)?) * 60 + n(4..6)?) * 1000 + n(6..9)?;
    let day = start_ms.div_euclid(86_400_000) * 86_400_000;
    let mut ms = day + of_day;
    if ms < start_ms - 3_600_000 {
        ms += 86_400_000;
    }
    Some(ms)
}

/// A Tartarus run's frames: frame-NNNN (whole for a read line on a board, cut otherwise) and observe\ (whole only
/// between board reads). By time.
fn tartarus_shots(dir: &Path, text: &str) -> Vec<Shot> {
    let rows = rows(text);
    let board = |v: &Value| kind(v) == "read" && v["screen"].as_str().is_some_and(|s| BOARD_SCREENS.contains(&s));
    let mut shots: Vec<Shot> = rows
        .iter()
        .filter(|r| matches!(kind(&r.v), "read" | "miss"))
        .filter_map(|r| {
            let file = r.v["frame"].as_str()?;
            dir.join(file).is_file().then(|| Shot { ms: r.ms, file: file.to_string(), treat: if board(&r.v) { Treat::Full(Game::Tartarus) } else { Treat::Crop(Game::Tartarus) } })
        })
        .collect();
    let reads: Vec<&Row> = rows.iter().filter(|r| matches!(kind(&r.v), "read" | "miss")).collect();
    if let (Some(start), Ok(entries)) = (rows.first().map(|r| r.ms), std::fs::read_dir(dir.join("observe"))) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(ms) = observe_ms(&name, start) else { continue };
            let k = reads.partition_point(|r| r.ms <= ms);
            let before = k.checked_sub(1).and_then(|k| reads.get(k)).filter(|r| ms - r.ms <= OBSERVE_NEAR_MS);
            let after = reads.get(k).filter(|r| r.ms - ms <= OBSERVE_NEAR_MS);
            let full = before.is_some_and(|r| board(&r.v)) && after.is_some_and(|r| board(&r.v));
            shots.push(Shot { ms, file: format!("observe/{name}"), treat: if full { Treat::Full(Game::Tartarus) } else { Treat::Crop(Game::Tartarus) } });
        }
    }
    shots.sort_by_key(|s| s.ms);
    shots
}

/// A Vorax run's frames: `vorax_read.frame` (choice and result screens whole, resolving animations cut, other screens
/// unused), frame-battle (already cut), observe mode's whole learn frames (by the nearest read line) and region crops.
/// observe\ is not used: its screen is unknown, and it may be the battle's map.
fn vorax_shots(dir: &Path, text: &str) -> Vec<Shot> {
    let rows = rows(text);
    let mut shots = Vec::new();
    for r in &rows {
        let v = &r.v;
        let exists = |f: &str| dir.join(f).is_file();
        match kind(v) {
            "vorax_read" => {
                let k = v["reading"]["kind"].as_str().unwrap_or("other").to_string();
                if let Some(f) = v["frame"].as_str().filter(|f| exists(f)) {
                    let treat = match k.as_str() {
                        "potion" | "instrument" | "protocol" | "result" => Some(Treat::Full(Game::Vorax)),
                        "resolving" => Some(Treat::Crop(Game::Vorax)),
                        _ => None,
                    };
                    if let Some(treat) = treat {
                        shots.push(Shot { ms: r.ms, file: f.to_string(), treat });
                    }
                }
            }
            // Private builds only: the English client's whole frames ([`SAMPLE_TREAT`]).
            #[cfg(feature = "dev-samples")]
            k if k == super::vorax_session::samples::EVENT => {
                if let Some(f) = v["file"].as_str().filter(|f| is_sample(f) && exists(f)) {
                    shots.push(Shot { ms: r.ms, file: f.to_string(), treat: SAMPLE_TREAT });
                }
            }
            "vorax_battle_frame" | "vorax_battle_end" => {
                if let Some(f) = v["frame"].as_str().filter(|f| f.starts_with("frame-battle-") && exists(f)) {
                    shots.push(Shot { ms: r.ms, file: f.to_string(), treat: Treat::AsIs });
                }
            }
            "vorax_unknown" | "vorax_tooltip" => {
                if let Some(f) = v["crop"].as_str().filter(|f| exists(f)) {
                    shots.push(Shot { ms: r.ms, file: f.to_string(), treat: Treat::AsIs });
                }
                // The line's own screen kind, never the read before it.
                let choice = matches!(v["screen"].as_str(), Some("potion" | "instrument" | "protocol" | "result"));
                if let Some(f) = v["frame"].as_str().filter(|f| exists(f)) {
                    shots.push(Shot { ms: r.ms, file: f.to_string(), treat: if choice { Treat::Full(Game::Vorax) } else { Treat::Crop(Game::Vorax) } });
                }
            }
            _ => {}
        }
    }
    shots.sort_by_key(|s| s.ms);
    shots
}

/// Private builds only (feature `dev-samples`): the English client's whole frames (`sample-NNNN.jpg`, see
/// `super::vorax_session::samples`) go in whole with [`mask_ids`] applied, as a Tartarus board frame does: they may
/// show any screen, the battle's map with its UID and chat included.
#[cfg(feature = "dev-samples")]
const SAMPLE_TREAT: Treat = Treat::Full(Game::Tartarus);

/// The interface scale a run's sample frames are masked for when its log has none ([`sample_scale`]): the
/// largest setting is about 1.5 times the one the id blocks were measured at.
#[cfg(feature = "dev-samples")]
const SAMPLE_SCALE_DEFAULT: f32 = 1.5;

/// The interface scale of a run's sample frames, for [`sample_rects`]: the largest `ui_scale` the log has, as a
/// multiple of the setting the id blocks were measured at (PageScale 0.667, see `crate::vorax::ui_scale`), never
/// under 1; [`SAMPLE_SCALE_DEFAULT`] without one.
#[cfg(feature = "dev-samples")]
fn sample_scale(text: &str) -> f32 {
    rows(text)
        .iter()
        .filter(|r| kind(&r.v) == "ui_scale")
        .filter_map(|r| r.v["scale"].as_f64())
        .map(|p| (p / 0.667) as f32)
        .fold(None, |m: Option<f32>, s| Some(m.map_or(s, |m| m.max(s))))
        .map_or(SAMPLE_SCALE_DEFAULT, |s| s.clamp(1.0, 2.0))
}

/// [`id_rects`] for a sample frame at interface scale `s`: each block `s` times as large from its corner. The HUD
/// grows with the interface (at the largest setting the UID starts 0.486 of the height from the right edge, past
/// the 0.375 measured at the original setting).
#[cfg(feature = "dev-samples")]
fn sample_rects(w: usize, h: usize, s: f32) -> [Rect; 3] {
    let (wf, hf) = (w as f32, h as f32);
    let corner = |(fw, fh): (f32, f32)| ((fw * s * hf).ceil().min(wf) as i32, (fh * s * hf).ceil().min(hf) as i32);
    let (rw, rh) = corner(UID_RIGHT);
    let (cw, ch) = corner(CHAT_LEFT);
    let (lw, lh) = corner(ID_LINE_LEFT);
    [Rect::new(w as i32 - rw, h as i32 - rh, rw, rh), Rect::new(0, h as i32 - ch, cw, ch), Rect::new(0, h as i32 - lh, lw, lh)]
}

/// The most sample frames one export takes: evenly spread over the run when there are more.
#[cfg(feature = "dev-samples")]
const SAMPLE_PICKS: usize = 80;

/// A sample frame's file name ([`SAMPLE_TREAT`]).
#[cfg(feature = "dev-samples")]
fn is_sample(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.strip_prefix(super::vorax_session::samples::PREFIX).is_some_and(|rest| rest.starts_with('-')) && name.ends_with(".jpg")
}

/// The picks of a Vorax export with the run's sample frames added (each once, at most [`SAMPLE_PICKS`], evenly
/// spread): they are what the run is for, so not only those near a moment.
#[cfg(feature = "dev-samples")]
fn with_samples(mut picks: Vec<Pick>, shots: &[Shot]) -> Vec<Pick> {
    let samples: Vec<&Shot> = shots.iter().filter(|s| is_sample(&s.file)).collect();
    let n = samples.len();
    let take = n.min(SAMPLE_PICKS);
    for i in 0..take {
        let shot = samples[i * n / take];
        if !picks.iter().any(|p| p.shot.file == shot.file) {
            picks.push(Pick { shot: shot.clone(), moments: Vec::new(), step: false });
        }
    }
    picks
}

/// The note of a zip with sample frames, shown in the listing before saving.
#[cfg(feature = "dev-samples")]
fn sample_note(n: usize) -> String {
    if lang::zh() {
        format!("含整張畫面 {n} 張（已遮 UID 與聊天）：英文客戶端樣本，只在私人測試版。")
    } else {
        format!("Includes {n} whole frames (UID and chat blacked out): English client samples, private test build only.")
    }
}

/// Each move's time: the read time of each Tartarus advice, each Vorax round (`vorax_round`).
fn steps(game: Game, text: &str) -> Vec<i64> {
    rows(text)
        .iter()
        .filter_map(|r| match (game, kind(&r.v)) {
            (Game::Tartarus, "advice") => r.v["read_at"].as_str().and_then(super::latency::log_millis),
            (Game::Vorax, "vorax_round") => Some(r.ms),
            _ => None,
        })
        .collect()
}

/// An image picked and who wants it: which moments, and whether it is a move's (it can be both).
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub shot: Shot,
    pub moments: Vec<usize>,
    pub step: bool,
}

/// Images picked at a level and drop step. A move takes the nearest frame before it (a whole one first); a moment the
/// nearest frame to each second within `window_s` either side, plus the line's own images; a battle moment only frame-battle. Each image once.
pub fn plan(game: Game, shots: &[Shot], steps: &[i64], moments: &[Moment], level: &Level, drop: u8) -> Vec<Pick> {
    let back = match game {
        Game::Tartarus => STEP_BACK_MS,
        Game::Vorax => NEAREST_MS,
    };
    let mut picks: Vec<Pick> = Vec::new();
    // File name -> index in picks.
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut add = |shot: &Shot, moment: Option<usize>, picks: &mut Vec<Pick>| {
        let k = *used.entry(shot.file.clone()).or_insert_with(|| {
            picks.push(Pick { shot: shot.clone(), moments: Vec::new(), step: false });
            picks.len() - 1
        });
        match moment {
            Some(m) if !picks[k].moments.contains(&m) => picks[k].moments.push(m),
            Some(_) => {}
            None => picks[k].step = true,
        }
    };
    // Drop steps: 1 = P3 moments' images, 2 = P2's too, 3 = the moves' too, 4 = every moment's.
    let keep_priority = |p: u8| match drop {
        0 => true,
        1 => p <= 2,
        2 | 3 => p <= 1,
        _ => false,
    };
    for (k, m) in moments.iter().enumerate().filter(|(_, m)| keep_priority(m.priority)) {
        for f in &m.files {
            if let Some(s) = shots.iter().find(|s| &s.file == f) {
                add(s, Some(k), &mut picks);
            }
        }
        let battle = |s: &Shot| s.file.starts_with("frame-battle-");
        if m.battle {
            // A battle moment with no image of its own: the run's nearest frame-battle.
            if m.files.iter().all(|f| !shots.iter().any(|s| &s.file == f))
                && let Some(s) = shots.iter().filter(|s| battle(s)).min_by_key(|s| (s.ms - m.ms).abs())
            {
                add(s, Some(k), &mut picks);
            }
            continue;
        }
        let pool: Vec<&Shot> = shots.iter().filter(|s| !battle(s) && s.treat != Treat::AsIs).collect();
        let w = level.window_s * 1000;
        let mut any = false;
        let mut t = m.ms - w;
        while t <= m.ms + w {
            if let Some(s) = pool.iter().filter(|s| (s.ms - t).abs() <= SAMPLE_MS / 2).min_by_key(|s| (s.ms - t).abs()) {
                add(s, Some(k), &mut picks);
                any = true;
            }
            t += SAMPLE_MS;
        }
        // No frame in the window (with observe mode off a frame is kept only when the screen changes): the nearest one
        // before the moment (Vorax within 5 s, Tartarus within 120 s), else within 5 s after it.
        if !any && m.files.is_empty() {
            let before = pool.iter().filter(|s| s.ms <= m.ms && m.ms - s.ms <= back).max_by_key(|s| s.ms);
            let after = pool.iter().filter(|s| s.ms > m.ms && s.ms - m.ms <= NEAREST_MS).min_by_key(|s| s.ms);
            if let Some(s) = before.or(after) {
                add(s, Some(k), &mut picks);
            }
        }
    }
    if drop < 3 {
        let frames: Vec<&Shot> = shots.iter().filter(|s| !s.file.starts_with("observe/") && s.treat != Treat::AsIs).collect();
        let pool = if frames.is_empty() { shots.iter().filter(|s| s.treat != Treat::AsIs).collect() } else { frames };
        for (i, &t) in steps.iter().enumerate() {
            if i % level.step_every.max(1) != 0 {
                continue;
            }
            let near = |full: bool| pool.iter().filter(|s| s.ms <= t + 500 && t - s.ms <= STEP_BACK_MS && matches!(s.treat, Treat::Full(_)) == full).max_by_key(|s| s.ms).copied();
            if let Some(s) = near(true).or_else(|| near(false)) {
                add(s, None, &mut picks);
            }
        }
    }
    picks.sort_by_key(|p| p.shot.ms);
    picks
}

// ───────────────────────── Images ─────────────────────────

/// Decodes a JPEG into RGBA.
pub fn decode_jpeg(bytes: &[u8]) -> Result<Rgba, String> {
    use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    let data = decoder.decode().map_err(|e| format!("{e:?}"))?;
    let info = decoder.info().ok_or("JPEG without a header")?;
    let (w, h) = (usize::from(info.width), usize::from(info.height));
    if data.len() != w * h * 4 {
        return Err(format!("JPEG decoded to {} bytes for {w}x{h}", data.len()));
    }
    Ok(Rgba { w, h, data })
}

/// Encodes RGBA as a JPEG.
pub fn encode_jpeg(img: &Rgba, quality: u8) -> Result<Vec<u8>, String> {
    let (w, h) = (u16::try_from(img.w).map_err(|e| e.to_string())?, u16::try_from(img.h).map_err(|e| e.to_string())?);
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, quality).encode(&img.data, w, h, jpeg_encoder::ColorType::Rgba).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Shrinks to `width` when wider, keeping the aspect.
fn shrink(img: Rgba, width: usize) -> Rgba {
    if img.w <= width || img.w == 0 {
        return img;
    }
    let h = (img.h * width).div_ceil(img.w).max(1);
    img.resize(width, h)
}

/// Blacks out `r`.
fn blacken(img: &mut Rgba, r: Rect) {
    let x0 = r.x.clamp(0, img.w as i32) as usize;
    let x1 = (r.x + r.w).clamp(0, img.w as i32) as usize;
    let y0 = r.y.clamp(0, img.h as i32) as usize;
    let y1 = (r.y + r.h).clamp(0, img.h as i32) as usize;
    for y in y0..y1 {
        for p in img.data[(y * img.w + x0) * 4..(y * img.w + x1) * 4].as_chunks_mut::<4>().0 {
            p.copy_from_slice(&[0, 0, 0, 255]);
        }
    }
}

/// Keeps only the pixels inside `keep` (the rest black), cut to their bounding box.
pub fn keep_only(img: &Rgba, keep: &[Rect]) -> Option<Rgba> {
    let inside = |r: &Rect| Rect::new(r.x.max(0), r.y.max(0), (r.x + r.w).min(img.w as i32) - r.x.max(0), (r.y + r.h).min(img.h as i32) - r.y.max(0));
    let keep: Vec<Rect> = keep.iter().map(inside).filter(|r| r.w > 0 && r.h > 0).collect();
    let x0 = keep.iter().map(|r| r.x).min()?;
    let y0 = keep.iter().map(|r| r.y).min()?;
    let x1 = keep.iter().map(|r| r.x + r.w).max()?;
    let y1 = keep.iter().map(|r| r.y + r.h).max()?;
    let mut out = Rgba { w: (x1 - x0) as usize, h: (y1 - y0) as usize, data: vec![0; ((x1 - x0) * (y1 - y0) * 4) as usize] };
    for p in out.data.as_chunks_mut::<4>().0 {
        p[3] = 255;
    }
    for r in &keep {
        for y in r.y..r.y + r.h {
            let src = &img.data[((y as usize) * img.w + r.x as usize) * 4..((y as usize) * img.w + (r.x + r.w) as usize) * 4];
            let at = (((y - y0) as usize) * out.w + (r.x - x0) as usize) * 4;
            out.data[at..at + src.len()].copy_from_slice(src);
        }
    }
    Some(out)
}

/// A Tartarus board-only cut: laid out as the reader does ([`crate::reader::layout::lay_out_found`], any aspect and
/// interface scale) on the reference frame, keeping [`TARTARUS_KEEP`]. No prison top bar (gate and back arrow): not a prison screen, None.
/// `hint` is the interface scale measured on the frame before. The output is no sharper than the frame.
pub fn tartarus_crop(img: &Rgba, hint: &mut Option<f32>) -> Option<Rgba> {
    let (board, _, u, found) = crate::reader::layout::lay_out_found(img, *hint);
    if !found {
        return None;
    }
    *hint = Some(u);
    let out = keep_only(&board, &TARTARUS_KEEP)?;
    // The reference frame is 1918 wide; a smaller frame (observe mode's 1280x720) shrinks back to its own scale.
    let scale = (img.h as f32 / REF_H as f32).min(1.0);
    let w = ((out.w as f32 * scale).round() as usize).max(1);
    Some(shrink(out, w))
}

/// A Vorax board-only cut: [`VORAX_KEEP`] moved to the frame by [`super::vorax_learn::frame_rect`].
pub fn vorax_crop(img: &Rgba) -> Option<Rgba> {
    let keep: Vec<Rect> = VORAX_KEEP.iter().map(|&r| super::vorax_learn::frame_rect(img.w, img.h, r)).collect();
    keep_only(img, &keep)
}

/// Whether a file is one the game's readers already cut to a panel (Vorax frame-battle-*, observe mode's
/// learn-*-crop*): the only images that may go in as they are.
pub fn already_cut(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.starts_with("frame-battle-") || (name.starts_with("learn-") && name.contains("-crop"))
}

/// Processes one image: None when no board shows even when cut (it stays out of the zip).
///
/// The hard rule: a whole frame never goes in as it is unless it is a card-play screen. Every Tartarus whole
/// frame and every frame to be cut gets [`mask_ids`] in its pixels first (see [`masked`]); a card-play frame
/// ([`Treat::Full`]) then goes in whole, any other one only through
/// [`tartarus_crop`] or [`vorax_crop`], which build a new image from the board regions alone. [`Treat::AsIs`]
/// is honoured only for [`already_cut`] files; anything else marked so is left out.
fn process(dir: &Path, shot: &Shot, width: usize, quality: u8, hint: &mut Option<f32>) -> Result<Option<Vec<u8>>, String> {
    if shot.treat == Treat::AsIs && !already_cut(&shot.file) {
        return Ok(None);
    }
    let path = dir.join(&shot.file);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", shot.file))?;
    let img = decode_jpeg(&bytes).map_err(|e| format!("{}: {e}", shot.file))?;
    // Private builds only: a sample frame goes in whole with the id blocks grown to its interface scale (`hint`, set
    // from the run's log by `prepare`), never through the treatments below.
    #[cfg(feature = "dev-samples")]
    if is_sample(&shot.file) {
        let mut img = img;
        for r in sample_rects(img.w, img.h, hint.unwrap_or(SAMPLE_SCALE_DEFAULT)) {
            blacken(&mut img, r);
        }
        return encode_jpeg(&shrink(img, width), quality).map(Some);
    }
    treat_image(img, shot.treat, hint).map(|img| encode_jpeg(&shrink(img, width), quality)).transpose()
}

/// Whether a whole frame of this treatment gets [`mask_ids`]: every one but a Vorax card-play screen (choice
/// or result). Those are full screen interfaces with no UID and no chat (checked by the Vorax session), and the
/// blocks would cover the bottom of the card tooltip (y 1152-1175 at 2560x1440) and the card area's corner.
pub fn masked(treat: Treat) -> bool {
    !matches!(treat, Treat::AsIs | Treat::Full(Game::Vorax))
}

/// [`process`] on a decoded image: the id corners blacked out (see [`masked`]), then kept whole, cut to the
/// board, or (an already cut panel) kept as it is.
pub fn treat_image(mut img: Rgba, treat: Treat, hint: &mut Option<f32>) -> Option<Rgba> {
    if masked(treat) {
        mask_ids(&mut img);
    }
    match treat {
        Treat::Full(_) | Treat::AsIs => Some(img),
        Treat::Crop(Game::Tartarus) => tartarus_crop(&img, hint),
        Treat::Crop(Game::Vorax) => vorax_crop(&img),
    }
}

// ───────────────────────── Cleaning the log ─────────────────────────

/// What cleaning looks for.
#[derive(Clone, Debug, Default)]
pub struct Scrub {
    /// Local folders as expanded (%USERPROFILE%, %APPDATA%, %LOCALAPPDATA%...), the longest first.
    pub dirs: Vec<String>,
    /// The Windows user name (%USERNAME%).
    pub user: Option<String>,
    /// This computer's install id.
    pub install_id: Option<String>,
}

impl Scrub {
    /// From the environment.
    pub fn from_env(install_id: &str) -> Scrub {
        let mut dirs: Vec<String> = ["USERPROFILE", "APPDATA", "LOCALAPPDATA", "TEMP", "TMP", "HOMEPATH"].iter().filter_map(|k| std::env::var(k).ok()).filter(|d| d.len() > 3).collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
        let user = std::env::var("USERNAME").ok().filter(|u| u.chars().count() >= 2);
        Scrub { dirs, user, install_id: Some(install_id.to_string()).filter(|s| !s.is_empty()) }
    }
}

/// How much cleaning replaced and removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub window_rows: usize,
    pub paths: usize,
    pub users: usize,
    pub install_ids: usize,
    /// Runs of 15 or more digits (the shape of player ids) removed.
    pub ids: usize,
}

/// A character folded for a case-blind comparison: its lowercase, character for character (a lowercase of
/// several characters, such as that of 'İ', compares as a whole).
fn fold(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase()
}

/// Replaces every `needle`, ignoring case, with `with`; the count. Compares character by character, so a
/// character whose lowercase has another byte length (İ, Ω, K) never puts an index off a character boundary.
fn replace_ci(text: &mut String, needle: &str, with: &str) -> usize {
    let wanted: Vec<char> = needle.chars().flat_map(fold).collect();
    if wanted.is_empty() {
        return 0;
    }
    let chars: Vec<char> = text.chars().collect();
    // Whether the folded characters from `i` start with `wanted`; how many characters of the text that took.
    let matches_at = |i: usize| -> Option<usize> {
        let mut k = 0;
        let mut j = i;
        while k < wanted.len() {
            let c = *chars.get(j)?;
            for f in fold(c) {
                if wanted.get(k) != Some(&f) {
                    return None;
                }
                k += 1;
            }
            j += 1;
        }
        Some(j - i)
    };
    let mut out = String::with_capacity(text.len());
    let (mut i, mut n) = (0, 0);
    while i < chars.len() {
        match matches_at(i) {
            Some(len) => {
                out.push_str(with);
                i += len;
                n += 1;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    *text = out;
    n
}

/// Local paths in a string (`X:\...`, `X:/...`, `\\server\...`) become `<路徑>`: from the drive letter to a quote,
/// bracket, comma, semicolon or line break (a path may hold spaces; better too much than too little). The count.
fn replace_drive_paths(text: &mut String) -> usize {
    const ENDS: &[char] = &['"', '\'', '<', '>', '|', '\r', '\n', '\t', ',', ';', '(', ')', '[', ']', '{', '}', '\u{ff09}', '\u{ff0c}', '\u{3002}', '\u{300d}'];
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut n) = (0, 0);
    while i < chars.len() {
        let drive = i + 2 < chars.len() && chars[i].is_ascii_alphabetic() && chars[i + 1] == ':' && matches!(chars[i + 2], '\\' | '/') && (i == 0 || !chars[i - 1].is_ascii_alphanumeric());
        let unc = i + 2 < chars.len() && chars[i] == '\\' && chars[i + 1] == '\\' && chars[i + 2].is_alphanumeric();
        if drive || unc {
            let mut j = i + 2;
            while j < chars.len() && !ENDS.contains(&chars[j]) {
                j += 1;
            }
            // A sentence's closing spaces and full stop are not the path's.
            while j > i + 3 && matches!(chars[j - 1], ' ' | '.') {
                j -= 1;
            }
            out.push_str("<路徑>");
            n += 1;
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    *text = out;
    n
}

/// The fewest digits in a row that count as an id: the game's player ids (UE_game.log's `SetGamePlayer`) are longer.
pub const LONG_NUMBER: usize = 15;

/// The first run of [`LONG_NUMBER`] or more digits in a row in `text`, if any (the privacy check of an export).
pub fn long_number(text: &str) -> Option<&str> {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let j = (i..b.len()).find(|&j| !b[j].is_ascii_digit()).unwrap_or(b.len());
            if j - i >= LONG_NUMBER {
                return Some(&text[i..j]);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Every run of [`LONG_NUMBER`] or more digits becomes `<數字>`; the count.
pub fn scrub_long_numbers(text: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let (mut n, mut rest) = (0, text);
    while let Some(hit) = long_number(rest) {
        let at = hit.as_ptr() as usize - rest.as_ptr() as usize;
        out.push_str(&rest[..at]);
        out.push_str("<\u{6578}\u{5b57}>");
        rest = &rest[at + hit.len()..];
        n += 1;
    }
    out.push_str(rest);
    (out, n)
}

/// A JSON number whose text holds [`LONG_NUMBER`] or more digits in a row: a whole number that long becomes the
/// string `<數字>` (an id), a fraction is rounded to 6 decimals (playout means and the like keep their meaning).
fn scrub_number(v: &mut Value, removed: &mut Removed) {
    let Value::Number(n) = v else { return };
    if long_number(&n.to_string()).is_none() {
        return;
    }
    match n.as_f64() {
        Some(f) if n.is_f64() && f.abs() < 1e14 => {
            let rounded = (f * 1e6).round() / 1e6;
            if let Some(r) = serde_json::Number::from_f64(rounded) {
                *v = Value::Number(r);
            }
        }
        _ => {
            *v = Value::String("<\u{6578}\u{5b57}>".to_string());
            removed.ids += 1;
        }
    }
}

/// Cleans a string: local folders and drive paths become `<路徑>`, the user name `<使用者>`, the install id goes,
/// and long numbers (ids, see [`LONG_NUMBER`]) become `<數字>`.
pub fn scrub_text(text: &str, s: &Scrub, removed: &mut Removed) -> String {
    let (mut t, ids) = scrub_long_numbers(text);
    removed.ids += ids;
    if let Some(id) = &s.install_id {
        removed.install_ids += replace_ci(&mut t, id, "");
    }
    for d in &s.dirs {
        removed.paths += replace_ci(&mut t, d, "<路徑>");
        let slashed = d.replace('\\', "/");
        if slashed != *d {
            removed.paths += replace_ci(&mut t, &slashed, "<路徑>");
        }
    }
    removed.paths += replace_drive_paths(&mut t);
    if let Some(u) = &s.user {
        removed.users += replace_ci(&mut t, u, "<使用者>");
    }
    t
}

fn scrub_value(v: &mut Value, s: &Scrub, removed: &mut Removed) {
    match v {
        Value::String(text) => *text = scrub_text(text, s, removed),
        Value::Array(a) => a.iter_mut().for_each(|x| scrub_value(x, s, removed)),
        Value::Number(_) => scrub_number(v, removed),
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for k in keys {
                let low = k.to_ascii_lowercase().replace(['_', '-'], "");
                if low == "installid" || low == "install" {
                    map.remove(&k);
                    removed.install_ids += 1;
                    continue;
                }
                let clean = scrub_text(&k, s, removed);
                let mut value = map.remove(&k).unwrap_or(Value::Null);
                scrub_value(&mut value, s, removed);
                map.insert(clean, value);
            }
        }
        _ => {}
    }
}

/// Cleans log.jsonl: after [`super::share::sanitize`] puts the window's size for its title, `kind == "window"` lines go
/// whole (section 3 of the Vorax spec; Tartarus too); every string of a line (keys too) is cleaned by [`scrub_text`], install id fields go.
/// A line that is not JSON is cleaned as a string.
pub fn clean_log(log: &str, s: &Scrub) -> (String, Removed) {
    let mut removed = Removed::default();
    let mut out = String::with_capacity(log.len());
    for line in super::share::sanitize(log).lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(mut v) => {
                if v["kind"] == "window" {
                    removed.window_rows += 1;
                    continue;
                }
                scrub_value(&mut v, s, &mut removed);
                out.push_str(&v.to_string());
            }
            Err(_) => out.push_str(&scrub_text(line, s, &mut removed)),
        }
        out.push('\n');
    }
    (out, removed)
}

// ───────────────────────── Packing ─────────────────────────

/// Whether a file name may go in the zip ([`ALLOWED`]).
pub fn allowed(name: &str) -> bool {
    name.is_ascii() && !name.contains(['/', '\\']) && (name == "log.jsonl" || name == "report.txt" || (name.ends_with(".jpg") && name.len() > 4))
}

/// Packs a zip: `files` are (name in the zip, contents); a name not allowed is an error.
pub fn zip_bytes(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    use zip::write::SimpleFileOptions;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in files {
        if !allowed(name) {
            return Err(format!("not allowed in the report: {name}"));
        }
        // JPEGs are compressed already, so they are stored; text is deflated.
        let method = if name.ends_with(".jpg") { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
        w.start_file(name.as_str(), SimpleFileOptions::default().compression_method(method)).map_err(|e| e.to_string())?;
        w.write_all(bytes).map_err(|e| e.to_string())?;
    }
    Ok(w.finish().map_err(|e| e.to_string())?.into_inner())
}

/// About how many bytes a file adds to a zip besides its contents (local and central headers, name).
const ZIP_OVERHEAD: u64 = 120;

/// Report data made and not yet saved.
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    pub run: String,
    pub game: Option<Game>,
    pub zip: Vec<u8>,
    /// (time, priority, reason, image count, whether images were left out as they may show personal information)
    pub moments: Vec<(String, u8, String, usize, bool)>,
    pub step_images: usize,
    pub images: usize,
    pub removed: Removed,
    /// The degrade level and drop step used.
    pub level: (usize, u8),
    /// Extra notes (a review skipped for another language...).
    pub notes: Vec<String>,
    /// Tartarus' normal passing states left out of the moments, in one line ([`TartarusMoments::summary`]).
    pub quiet: Option<String>,
}

/// What to export.
#[derive(Clone, Debug)]
pub struct Request {
    pub dir: PathBuf,
    pub game: Game,
    /// The review the Run tab already has (used for the same folder, not re-solved).
    pub settled: Option<Settled>,
    pub install_id: String,
    pub app_dir: Option<PathBuf>,
}

/// What the background thread sends.
pub enum Msg {
    Status(String),
    Done(Result<Prepared, String>),
}

/// The error string of a cancel.
const CANCELLED: &str = "cancelled";

/// Tartarus' other sources (review, run check, crash.log, moves made against the advice).
fn tartarus_extra(req: &Request, text: &str, cancel: &AtomicBool, status: &dyn Fn(String), notes: &mut Vec<String>) -> Extra {
    let mut extra = Extra::default();
    let name = req.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ui = lang::lang();
    // Advice in another language than the window's is not compared (audit switches the global language for a while: see settle.rs).
    let same_lang = settle::other_lang(text, ui).is_none();
    if !same_lang {
        notes.push(tr("The run was advised in another language: moves made against the advice and the run review were not checked.").to_string());
    }
    if same_lang && text.contains("\"kind\":\"advice\"") {
        status(tr("Comparing moves with the advice...").to_string());
        for g in crate::audit::games(&name, text) {
            for d in &g.decisions {
                if d.followed() == Some(false)
                    && let Ok(a) = &d.actual
                {
                    extra.differs.push((d.read_at.clone(), d.pick.clone(), d.label(a)));
                }
            }
        }
    }
    let cached = req.settled.as_ref().filter(|s| s.folder == name);
    extra.worst = match cached {
        Some(s) => s.worst.clone(),
        None if same_lang && text.contains("\"kind\":\"run_end\"") && !cancel.load(Ordering::Relaxed) => {
            status(tr("Reviewing the run for its largest loss...").to_string());
            let rollouts = settle::ROLLOUTS / 4;
            // The re-solve runs on a thread of its own at below normal priority. `audit::score` has no way to stop
            // part way, so a cancel stops waiting for it: the export ends at once and the thread finishes alone.
            // At most one such re-solve runs at a time (see [`settle_once`]): an export cancelled and started again
            // waits for the one still running, and takes its result when it is the same run.
            let out = settle_once(&name, text, ui, rollouts, cancel);
            match out {
                Some(Outcome::Settled(s)) => {
                    notes.push(trf!("Run review re-solved here with {} playouts per option.", rollouts));
                    s.worst
                }
                _ => None,
            }
        }
        None => None,
    };
    let crash = req.app_dir.as_ref().map(|d| d.join("crash.log"));
    if let Some(report) = super::health::check(&req.dir, crash.as_deref()) {
        extra.health = report.issues;
    }
    if let Some(text) = crash.and_then(|p| std::fs::read_to_string(p).ok()) {
        extra.crashes = crashes(&text);
    }
    extra
}

/// The export's run review re-solve: the run folder it is for and its result once done (None: failed).
type Running = (String, Arc<(std::sync::Mutex<Option<Option<Outcome>>>, std::sync::Condvar)>);

/// The one re-solve an export may have running (see [`settle_once`]).
fn running() -> &'static std::sync::Mutex<Option<Running>> {
    static RUNNING: std::sync::OnceLock<std::sync::Mutex<Option<Running>>> = std::sync::OnceLock::new();
    RUNNING.get_or_init(|| std::sync::Mutex::new(None))
}

/// Whether an export's run review re-solve is still running (the language is not changed meanwhile: `audit`
/// compares advice labels in the global language).
pub fn settle_running() -> bool {
    running().lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|(_, cell)| cell.0.lock().unwrap_or_else(|e| e.into_inner()).is_none())
}

/// The run review of `name` for an export, re-solved on its own thread at below normal priority, at most one at a
/// time. A re-solve of the same run still running, or done, is taken over instead of starting another; one of
/// another run is waited for first. `audit::score` cannot stop part way, so a cancel only stops the waiting:
/// the thread finishes alone, and the next export of that run takes its result. None when cancelled or failed.
fn settle_once(name: &str, text: &str, ui: Lang, rollouts: usize, cancel: &AtomicBool) -> Option<Outcome> {
    // Waits for `cell` to be filled, checking `cancel`.
    let wait = |cell: &Arc<(std::sync::Mutex<Option<Option<Outcome>>>, std::sync::Condvar)>| -> Option<Option<Outcome>> {
        let (lock, done) = &**cell;
        let mut got = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(out) = got.as_ref() {
                return Some(out.clone());
            }
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            got = done.wait_timeout(got, std::time::Duration::from_millis(200)).unwrap_or_else(|e| e.into_inner()).0;
        }
    };
    loop {
        let current = running().lock().unwrap_or_else(|e| e.into_inner()).clone();
        match current {
            Some((folder, cell)) if folder == name => return wait(&cell).flatten(),
            Some((_, cell)) if cell.0.lock().unwrap_or_else(|e| e.into_inner()).is_none() => {
                // Another run's re-solve: let it finish first.
                wait(&cell)?;
            }
            _ => {
                let cell = Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
                *running().lock().unwrap_or_else(|e| e.into_inner()) = Some((name.to_string(), cell.clone()));
                let (name2, text2, fill) = (name.to_string(), text.to_string(), cell.clone());
                std::thread::spawn(move || {
                    crate::solver::solve::set_priority(-1);
                    let threads = std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).max(1));
                    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).start_handler(|_| crate::solver::solve::set_priority(-1)).build();
                    let out = pool.ok().and_then(|pool| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pool.install(|| settle::settle(&name2, &text2, ui, rollouts)))).ok());
                    *fill.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
                    fill.1.notify_all();
                });
                return wait(&cell).flatten();
            }
        }
    }
}

/// The run's facts: game language, OCR language, resolution (from the window line, which is deleted later, and the reads).
fn run_facts(game: Game, text: &str) -> (String, String, String) {
    let rows = rows(text);
    let window = rows.iter().find(|r| kind(&r.v) == "window");
    let ocr = window.and_then(|r| r.v["ocr"].as_str().map(str::to_string)).or_else(crate::reader::ocr::language).unwrap_or_else(|| tr("none").to_string());
    let size = rows.iter().find_map(|r| {
        let s = r.v["size"].as_array()?;
        Some(format!("{} x {}", s.first()?.as_u64()?, s.get(1)?.as_u64()?))
    });
    let lang = match game {
        Game::Vorax => Lang::ZhTw,
        Game::Tartarus => rows.iter().find(|r| kind(&r.v) == "advice").and_then(|r| r.v["pick"].as_str()).map_or(lang::lang(), |p| if p.is_ascii() { Lang::En } else { Lang::ZhTw }),
    };
    (lang.native_name().to_string(), ocr, size.unwrap_or_else(|| "-".to_string()))
}

/// Makes one report (called on a background thread).
pub fn prepare(req: &Request, cancel: &AtomicBool, status: &dyn Fn(String)) -> Result<Prepared, String> {
    let check = || if cancel.load(Ordering::Relaxed) { Err(CANCELLED.to_string()) } else { Ok(()) };
    status(tr("Reading the run log...").to_string());
    let text = std::fs::read_to_string(req.dir.join("log.jsonl")).map_err(|e| e.to_string())?;
    let run = req.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut notes = Vec::new();
    status(tr("Picking the suspicious moments...").to_string());
    let (moments, quiet, shots) = match req.game {
        Game::Tartarus => {
            let extra = tartarus_extra(req, &text, cancel, status, &mut notes);
            let found = tartarus_moments(&text, &extra);
            let quiet = found.summary();
            (found.moments, quiet, tartarus_shots(&req.dir, &text))
        }
        Game::Vorax => (vorax_moments(&text), None, vorax_shots(&req.dir, &text)),
    };
    check()?;
    let steps = steps(req.game, &text);
    let scrub = Scrub::from_env(&req.install_id);
    let (log, removed) = clean_log(&text, &scrub);
    let facts = run_facts(req.game, &text);
    // Processed images: file -> contents (None: no board shows, left out), for the width and quality in `sized`.
    // A level of another width or quality empties it first, so only one size of the images is ever held.
    let mut cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();
    let mut sized: Option<(usize, u8)> = None;
    let mut hint = None;
    // A Vorax run's sample frames are masked for its interface scale (`process`); Vorax cuts do not use the hint.
    #[cfg(feature = "dev-samples")]
    if req.game == Game::Vorax {
        hint = Some(sample_scale(&text));
    }
    for (k, drop) in attempts() {
        let level = LEVELS[k];
        if sized != Some((level.width, level.quality)) {
            cache = HashMap::new();
            sized = Some((level.width, level.quality));
        }
        let picks = plan(req.game, &shots, &steps, &moments, &level, drop);
        #[cfg(feature = "dev-samples")]
        let picks = if req.game == Game::Vorax { with_samples(picks, &shots) } else { picks };
        let todo: Vec<&Pick> = picks.iter().filter(|p| !cache.contains_key(&p.shot.file)).collect();
        for (n, p) in todo.iter().enumerate() {
            check()?;
            if n % 5 == 0 {
                status(trf!("Processing images {}/{}...", n + 1, todo.len()));
            }
            cache.insert(p.shot.file.clone(), process(&req.dir, &p.shot, level.width, level.quality, &mut hint)?);
        }
        // A cheap estimate first (log.jsonl deflates to about a tenth; a fifth to be safe; report.txt 64 KB);
        // only a combination under it is packed, and the packed size decides.
        let images: u64 = picks.iter().filter_map(|p| cache.get(&p.shot.file).and_then(|b| b.as_ref())).map(|b| b.len() as u64 + ZIP_OVERHEAD).sum();
        if images + log.len() as u64 / 5 + 64 * 1024 + 2 * ZIP_OVERHEAD > LIMIT {
            continue;
        }
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut per_moment = vec![0usize; moments.len()];
        let mut skipped = vec![false; moments.len()];
        let mut step_images = 0;
        for p in &picks {
            let name = p.shot.file.rsplit('/').next().unwrap_or(&p.shot.file).to_string();
            match cache.get(&p.shot.file).cloned().flatten() {
                Some(bytes) => {
                    p.moments.iter().for_each(|&m| per_moment[m] += 1);
                    step_images += usize::from(p.step);
                    files.push((name, bytes));
                }
                None => p.moments.iter().for_each(|&m| skipped[m] = true),
            }
        }
        #[cfg(not(feature = "dev-samples"))]
        let notes = notes.clone();
        // Private builds only: a zip with sample frames says so first.
        #[cfg(feature = "dev-samples")]
        let notes = {
            let mut notes = notes.clone();
            let n = files.iter().filter(|(name, _)| is_sample(name)).count();
            if n > 0 {
                notes.insert(0, sample_note(n));
            }
            notes
        };
        let mut prepared = Prepared {
            run: run.clone(),
            game: Some(req.game),
            zip: Vec::new(),
            moments: moments.iter().enumerate().map(|(i, m)| (m.at.clone(), m.priority, m.reason.clone(), per_moment[i], skipped[i])).collect(),
            step_images,
            images: files.len(),
            removed: removed.clone(),
            level: (k, drop),
            notes,
            quiet: quiet.clone(),
        };
        let report = scrub_text(&report_text(&prepared, &level, &facts), &scrub, &mut Removed::default());
        files.insert(0, ("log.jsonl".to_string(), log.clone().into_bytes()));
        files.insert(0, ("report.txt".to_string(), report.into_bytes()));
        check()?;
        status(tr("Packing the zip...").to_string());
        prepared.zip = zip_bytes(&files)?;
        // The estimate was too low: the next combination.
        if prepared.zip.len() as u64 <= LIMIT {
            return Ok(prepared);
        }
    }
    Err(tr("The run is too large to fit in 20 MB.").to_string())
}

/// "21:16:37" (UTC).
fn clock(at: &str) -> &str {
    at.get(11..19).unwrap_or(at)
}

/// report.txt's text (in the window's language).
/// How report.txt and the export card describe the images: the same rule as [`masked`].
const IMAGES_RULE: &str = "- Images: card-play screens (the Tartarus prison, Vorax picks and results) are kept whole; other screens are cut to the board, or left out when none shows. Tartarus frames have the account UID at the bottom right and the chat and id line at the bottom left blacked out; Vorax pick and result screens are full-screen menus with no UID or chat and are kept as they are";

fn report_text(p: &Prepared, level: &Level, (game_lang, ocr, size): &(String, String, String)) -> String {
    let mut t = String::new();
    let mut line = |s: String| {
        t.push_str(&s);
        t.push_str("\r\n");
    };
    line(trf!("Ascendant report data {}", p.run));
    line(trf!("Version: {}", env!("CARGO_PKG_VERSION")));
    line(trf!("Game mode: {}", p.game.map_or("-", Game::name)));
    line(trf!("Game language: {}", game_lang));
    line(trf!("OCR language: {}", ocr));
    line(trf!("Resolution: {}", size));
    line(trf!("Times are UTC. Images: {} ({} for the moves, the rest for the moments).", p.images, p.step_images));
    line(trf!("Compression: one image every {} moves, {} s around each moment, width {}, JPEG quality {}.", level.step_every, level.window_s, level.width, level.quality));
    if p.level.1 > 0 {
        line(trf!("Over 20 MB: images of lower-priority moments were left out (step {}).", p.level.1));
    }
    line(String::new());
    line(trf!("Suspicious moments: {}", p.moments.len()));
    for (i, (at, priority, reason, n, skipped)) in p.moments.iter().enumerate() {
        line(format!("{}. {} [P{}] {} ({})", i + 1, clock(at), priority, reason, trf!("{} images", n)));
        if *skipped {
            line(format!("   {}", tr("Some images of this moment were left out: they may show personal information.")));
        }
    }
    if let Some(quiet) = &p.quiet {
        line(quiet.clone());
    }
    line(String::new());
    line(tr("Removed before packing:").to_string());
    line(trf!("- Window titles: {} window lines deleted", p.removed.window_rows));
    line(trf!("- Local paths: {} replaced with <路徑>", p.removed.paths));
    line(trf!("- Windows user name: {} replaced with <使用者>", p.removed.users));
    line(trf!("- Install id: {} removed", p.removed.install_ids));
    line(trf!("- Long numbers (ids): {} replaced with <數字>", p.removed.ids));
    line(tr(IMAGES_RULE).to_string());
    for n in &p.notes {
        line(n.clone());
    }
    t
}

// ───────────────────────── Saving ─────────────────────────

/// The folder to save in: `exe`'s folder when it can be written, else `fallback` (app_dir); `writable` tells.
pub fn save_dir(exe: Option<&Path>, fallback: Option<PathBuf>, writable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    exe.and_then(Path::parent).filter(|d| writable(d)).map(Path::to_path_buf).or(fallback.filter(|d| writable(d)))
}

/// Whether a folder can be written: a probe file made and deleted.
pub fn can_write(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".ascendant-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The zip's name, ASCII whatever the window's language (uploads, unzip tools and other computers take it as
/// it is): "Ascendant-report-<run time>.zip". A run folder name is ASCII; anything else in it becomes '_'.
pub fn zip_name(game: Game, run: &str) -> String {
    let run: String = run.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' }).collect();
    let mode = match game {
        Game::Tartarus => "tartarus",
        Game::Vorax => "vorax",
    };
    format!("Ascendant-report-{mode}-{run}.zip")
}

/// The `n`th name to try for a zip: the name itself, then "-2", "-3"... before ".zip".
pub fn numbered(name: &str, n: usize) -> String {
    match (n, name.strip_suffix(".zip")) {
        (0 | 1, _) => name.to_string(),
        (_, Some(stem)) => format!("{stem}-{n}.zip"),
        (_, None) => format!("{name}-{n}"),
    }
}

/// Writes `bytes` as a new file in `dir` named `name` (or "-2", "-3"... when taken): the name is claimed
/// with create_new, the bytes go to a temporary file, which is renamed over the claim once complete, so a
/// half written zip never carries the final name and no existing file is ever replaced. The path written.
pub fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let mut path = None;
    for n in 1..1000 {
        let candidate = dir.join(numbered(name, n));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(_) => {
                path = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", candidate.display())),
        }
    }
    let path = path.ok_or_else(|| format!("{}: every name is taken", dir.display()))?;
    let temp = dir.join(format!(".{}.partial-{}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), std::process::id()));
    // The rename is tried a few times: an antivirus scanning the new file can hold it for a moment.
    let rename = || {
        let mut last = std::fs::rename(&temp, &path);
        for _ in 0..5 {
            if last.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            last = std::fs::rename(&temp, &path);
        }
        last
    };
    let written = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp).and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all())).and_then(|()| rename());
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::remove_file(&path);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(path)
}

/// Removes what a save cut short left in `dir`: ".Ascendant-report-*.partial-*" files and empty
/// "Ascendant-report-*.zip" name claims. The count removed.
pub fn clean_leftovers(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let partial = name.starts_with(".Ascendant-report-") && name.contains(".partial-");
            let claim = name.starts_with("Ascendant-report-") && name.ends_with(".zip") && e.metadata().is_ok_and(|m| m.is_file() && m.len() == 0);
            (partial || claim) && std::fs::remove_file(e.path()).is_ok()
        })
        .count()
}

/// Saves beside Ascendant.exe, or in app_dir when that can't be written. The path saved to.
pub fn save(p: &Prepared, exe: Option<&Path>, app_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let dir = save_dir(exe, app_dir, can_write).ok_or_else(|| tr("No folder to save in: neither Ascendant's folder nor its data folder can be written.").to_string())?;
    clean_leftovers(&dir);
    write_new(&dir, &zip_name(p.game.unwrap_or(Game::Tartarus), &p.run), &p.zip)
}

/// Selects `path` in File Explorer: `explorer.exe /select,"<path>"`.
pub fn reveal(path: &Path) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
}

// ───────────────────────── In the background ─────────────────────────

/// An export in progress.
pub struct Export {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

impl Export {
    /// Starts on a background thread; `notify` (a repaint) runs with each message.
    pub fn start(req: Request, notify: impl Fn() + Send + Sync + 'static) -> Export {
        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            crate::solver::solve::set_priority(-1);
            let send = |m: Msg| {
                let _ = tx.send(m);
                notify();
            };
            // A panic in the export ends the export with a message, not the app. (A release build aborts on a
            // panic, see Cargo.toml's [profile.release]; there this cannot catch it.)
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepare(&req, &flag, &|s| send(Msg::Status(s)))));
            let result = result.unwrap_or_else(|e| {
                let why = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
                Err(trf!("internal error: {}", why))
            });
            send(Msg::Done(result));
        });
        Export { rx, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The messages sent, in order.
    pub fn poll(&self) -> Vec<Msg> {
        self.rx.try_iter().collect()
    }
}

// ───────────────────────── The window ─────────────────────────

/// The card's state.
#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Working(Export, String),
    Ready(Box<Prepared>),
    /// Being written on a background thread; the bool: select the zip in File Explorer once saved.
    Saving(Receiver<Result<PathBuf, String>>, Box<Prepared>, bool),
    Saved(PathBuf, Box<Prepared>),
    Failed(String),
}

/// What the export card exports: the run the Run tab shows (see [`super::run_review::RunReview::export_target`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A finished run: the one the Run tab shows.
    Run(PathBuf, Game),
    /// "Latest run" before any game ended this session, so the Run tab shows no run: the newest finished run, named on
    /// the card (its "time · game", [`RunEntry::title`]) since it is not on show above.
    Newest(PathBuf, Game, String),
    /// The run on show is not finished (it is not in the list of finished runs): nothing to export.
    Unfinished,
    /// Not known yet: the run list is being read, or the run on show is still being settled.
    Waiting,
    /// No finished run at all.
    Nothing,
}

/// The Run tab's "Export this run" card, and the list of finished runs the Run tab's run picker shows.
#[derive(Default)]
pub struct ReportUi {
    runs: Vec<RunEntry>,
    listed: bool,
    /// The run list being read on a background thread.
    listing: Option<Receiver<Vec<RunEntry>>>,
    /// The run folders found finished by a listing, so the next listing reads only the others' logs (see
    /// [`list_runs_known`]).
    known: Arc<Mutex<HashSet<PathBuf>>>,
    /// The run being prepared ("time · game"), named while it is: the run picker may have moved on meanwhile.
    exporting: String,
    /// The folder of the run being prepared or last prepared: a finished export ("Saved") or a failure belongs to
    /// it, and goes once another run is to be exported (see [`ReportUi::follow`]).
    exporting_dir: Option<PathBuf>,
    /// For previews (ASCENDANT_PREVIEW_MEASURE): the top of the Prepare button when last drawn, in window points.
    pub prepare_top: Option<f32>,
    phase: Phase,
    /// For tests (ASCENDANT_PREVIEW_REPORT_SAVE=1): saves as soon as the report is made.
    pub preview_save: bool,
}

/// What a press on the card does.
enum Act {
    Start,
    Cancel,
    Save,
    Discard,
    Reveal(PathBuf),
    Refresh,
}

impl ReportUi {
    /// Lists the runs again, on a background thread (a run folder can hold thousands of observe frames);
    /// `notify` repaints when the list is in.
    pub fn refresh(&mut self, notify: impl Fn() + Send + 'static) {
        let (tx, rx) = channel();
        let known = Arc::clone(&self.known);
        std::thread::spawn(move || {
            let _ = tx.send(super::settings::app_dir().map(|d| list_runs_known(&d, &known)).unwrap_or_default());
            notify();
        });
        self.listing = Some(rx);
        self.listed = true;
    }

    /// Takes the run list once the background thread has it.
    fn poll_list(&mut self) {
        let Some(rx) = &self.listing else { return };
        match rx.try_recv() {
            Ok(runs) => {
                self.runs = runs;
                self.listing = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.listing = None,
        }
    }

    /// The finished runs listed so far (the Run tab's run picker shows them); the first call starts the listing on a
    /// background thread, and the list is taken once it is in.
    pub fn runs(&mut self, ctx: &egui::Context) -> &[RunEntry] {
        if !self.listed {
            let ctx = ctx.clone();
            self.refresh(move || ctx.request_repaint());
        }
        self.poll_list();
        &self.runs
    }

    /// The finished runs as last listed, without reading them again.
    pub fn listed_runs(&self) -> &[RunEntry] {
        &self.runs
    }

    /// Whether the list is being read, or is to be read again before it is next shown.
    pub fn listing(&self) -> bool {
        self.listing.is_some() || !self.listed
    }

    /// Lists the runs again the next time they are asked for (a game just ended, so the list is out of date).
    pub fn relist(&mut self) {
        self.listed = false;
    }

    /// For tests: `runs` comes in as a listing does.
    #[cfg(test)]
    pub fn take_list(&mut self, runs: Vec<RunEntry>) {
        let (tx, rx) = channel();
        tx.send(runs).unwrap();
        self.listing = Some(rx);
        self.listed = true;
        self.poll_list();
    }

    /// Starts the export of the run `dir` (the preview entry point).
    pub fn start_dir(&mut self, dir: PathBuf, settled: Option<Settled>, install_id: &str, notify: impl Fn() + Send + Sync + 'static) {
        let entry = RunEntry::of(dir.clone());
        let game = entry.game;
        self.exporting = entry.title();
        self.exporting_dir = Some(dir.clone());
        self.phase = Phase::Working(Export::start(Request { dir, game, settled, install_id: install_id.to_string(), app_dir: super::settings::app_dir() }, notify), String::new());
    }

    /// The background's messages.
    pub fn poll(&mut self, exe: Option<&Path>, notify: impl Fn() + Send + 'static) {
        if let Phase::Saving(rx, ..) = &self.phase {
            match rx.try_recv() {
                Ok(result) => {
                    let Phase::Saving(_, p, open) = std::mem::take(&mut self.phase) else { return };
                    self.phase = match result {
                        Ok(path) => {
                            if open {
                                reveal(&path);
                            }
                            Phase::Saved(path, p)
                        }
                        Err(e) => Phase::Failed(e),
                    };
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.phase = Phase::Failed(tr("the save thread stopped").to_string()),
            }
            return;
        }
        let Phase::Working(export, status) = &mut self.phase else { return };
        let mut done = None;
        for m in export.poll() {
            match m {
                Msg::Status(s) => *status = s,
                Msg::Done(r) => done = Some(r),
            }
        }
        match done {
            // A preview saves by itself and never opens File Explorer (it would take the focus).
            Some(Ok(p)) if self.preview_save => self.start_save(Box::new(p), exe, false, notify),
            Some(Ok(p)) => self.phase = Phase::Ready(Box::new(p)),
            Some(Err(e)) if e == CANCELLED => self.phase = Phase::Idle,
            Some(Err(e)) => self.phase = Phase::Failed(e),
            None => {}
        }
    }

    /// Writes `p` on a background thread (20 MB and a flush can take a while on a hard disk); with `open`, selects
    /// the zip in File Explorer once written.
    fn start_save(&mut self, p: Box<Prepared>, exe: Option<&Path>, open: bool, notify: impl Fn() + Send + 'static) {
        let (tx, rx) = channel();
        let (zip, game, run, exe) = (p.zip.clone(), p.game, p.run.clone(), exe.map(Path::to_path_buf));
        std::thread::spawn(move || {
            let copy = Prepared { zip, game, run, ..Prepared::default() };
            let _ = tx.send(save(&copy, exe.as_deref(), super::settings::app_dir()));
            notify();
        });
        self.phase = Phase::Saving(rx, p, open);
    }

    /// The card follows the run to export: once another run (or an unfinished one, or none) is to be exported, an
    /// export of the run before goes, whether it is being prepared (cancelled; what its thread still sends is
    /// dropped with the channel), listed or finished ("Saved", or a failure), and the card offers "Prepare" for
    /// `target`. Only a zip being written is finished first, so no half-written file is left; once written, the next
    /// call finds "Saved" for the run before and goes back to "Prepare" too. `Waiting` (the list being read again)
    /// changes nothing.
    fn follow(&mut self, target: &Target) {
        let dir = match target {
            Target::Run(dir, _) | Target::Newest(dir, _, _) => Some(dir),
            Target::Unfinished | Target::Nothing => None,
            Target::Waiting => return,
        };
        if dir == self.exporting_dir.as_ref() || matches!(self.phase, Phase::Idle | Phase::Saving(..)) {
            return;
        }
        if let Phase::Working(export, _) = &self.phase {
            export.cancel();
        }
        self.phase = Phase::Idle;
    }

    /// Whether an export or its run review re-solve is running (the window keeps the language as it is meanwhile).
    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Working(..)) || settle_running()
    }

    /// Draws the card, which exports `target`, the run the Run tab shows. `settled` is the review the Run tab has,
    /// `exe` is Ascendant.exe.
    pub fn show(&mut self, ui: &mut egui::Ui, target: &Target, settled: Option<&Settled>, install_id: &str, exe: Option<&Path>) {
        let _ = self.runs(ui.ctx());
        let ctx = ui.ctx().clone();
        self.poll(exe, move || ctx.request_repaint());
        self.follow(target);
        theme::heading(ui, tr("Export this run"));
        let mut act = None;
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            let small = |ui: &mut egui::Ui, text: &str, colour| {
                ui.add(egui::Label::new(RichText::new(text).small().color(colour)).wrap());
            };
            match &self.phase {
                Phase::Idle | Phase::Failed(_) => {
                    small(ui, tr("Packs the log and board screenshots of the run picked above into a zip for a bug report. Nothing is sent: you look it over, and only \"Save and open folder\" writes it."), MUTED);
                    ui.horizontal(|ui| {
                        let prepare = ui.button(tr("Prepare")).on_hover_text(tr("Picks the suspicious moments and the screenshots in the background"));
                        self.prepare_top = Some(prepare.rect.top());
                        if prepare.clicked() {
                            act = Some(Act::Start);
                        }
                        // Greyed out only while the list is read; it lists the runs of the run picker above again.
                        if ui.add_enabled(self.listing.is_none(), egui::Button::new(tr("Refresh list")).small()).clicked() {
                            act = Some(Act::Refresh);
                        }
                    });
                    // Why nothing is exported, or which run when none is on show above: one line of fixed height, empty
                    // when the run on show is exported, so the buttons and everything below stay where they are.
                    let (text, colour, hover) = match target {
                        Target::Run(..) => (String::new(), MUTED, None),
                        Target::Newest(.., title) => (trf!("Exports: {}", title), MUTED, None),
                        Target::Unfinished => (tr("This run is not finished: pick one above").to_string(), theme::WARN, None),
                        Target::Waiting => (tr("Wait a moment: this run is still being read").to_string(), MUTED, None),
                        Target::Nothing => (tr("No finished runs yet").to_string(), MUTED, Some(tr("No finished runs yet: with Keep a run log on in Settings, each game is listed once it ends."))),
                    };
                    let line_h = ui.text_style_height(&egui::TextStyle::Small);
                    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), line_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.set_min_size(egui::vec2(ui.available_width(), line_h));
                        if !text.is_empty() {
                            let label = ui.add(egui::Label::new(RichText::new(text).small().color(colour)).truncate());
                            if let Some(hover) = hover {
                                label.on_hover_text(hover);
                            }
                        }
                    });
                    if let Phase::Failed(e) = &self.phase {
                        small(ui, &trf!("Export failed: {}", e), theme::ERROR);
                    }
                }
                Phase::Saving(..) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                        ui.label(RichText::new(tr("Saving...")).strong());
                    });
                }
                Phase::Working(_, status) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                        ui.add(egui::Label::new(RichText::new(trf!("Preparing the report: {}", self.exporting)).strong()).wrap());
                    });
                    small(ui, status, MUTED);
                    if ui.button(tr("Cancel")).clicked() {
                        act = Some(Act::Cancel);
                    }
                }
                Phase::Ready(p) => {
                    contents(ui, p);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(tr("Save and open folder")).strong()).on_hover_text(tr("Saves the zip in Ascendant's folder and selects it in File Explorer")).clicked() {
                            act = Some(Act::Save);
                        }
                        if ui.button(tr("Cancel")).clicked() {
                            act = Some(Act::Discard);
                        }
                    });
                }
                Phase::Saved(path, p) => {
                    ui.label(RichText::new(tr("Saved")).strong().color(theme::GOOD));
                    // The file name and a short line on where; the whole path in the hover.
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let beside_exe = exe.and_then(Path::parent).is_some_and(|d| Some(d) == path.parent());
                    let place = if beside_exe { tr("In the same folder as Ascendant.exe") } else { tr("In Ascendant's data folder (its own folder can't be written)") };
                    let full = path.display().to_string();
                    ui.add(egui::Label::new(RichText::new(&name).small().color(theme::TEXT)).truncate().show_tooltip_when_elided(false)).on_hover_text(&full);
                    ui.add(egui::Label::new(RichText::new(place).small().color(MUTED)).wrap()).on_hover_text(&full);
                    small(ui, &trf!("{} images, {}", p.images, size_text(p.zip.len() as u64)), MUTED);
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(tr("Done")).strong()).clicked() {
                            act = Some(Act::Discard);
                        }
                        if ui.small_button(tr("Open folder")).on_hover_text(tr("Shows the zip in File Explorer")).clicked() {
                            act = Some(Act::Reveal(path.clone()));
                        }
                    });
                }
            }
        });
        match act {
            Some(Act::Start) => {
                // Only a finished run is exported; otherwise the line under the buttons already says why.
                if let Target::Run(dir, game) | Target::Newest(dir, game, _) = target {
                    self.exporting = self.runs.iter().find(|r| r.dir == *dir).map_or_else(|| RunEntry::of(dir.clone()).title(), RunEntry::title);
                    self.exporting_dir = Some(dir.clone());
                    let ctx = ui.ctx().clone();
                    let req = Request { dir: dir.clone(), game: *game, settled: settled.cloned(), install_id: install_id.to_string(), app_dir: super::settings::app_dir() };
                    self.phase = Phase::Working(Export::start(req, move || ctx.request_repaint()), String::new());
                }
            }
            Some(Act::Cancel) => {
                if let Phase::Working(e, _) = &self.phase {
                    e.cancel();
                }
            }
            Some(Act::Save) => {
                if let Phase::Ready(p) = std::mem::take(&mut self.phase) {
                    let ctx = ui.ctx().clone();
                    self.start_save(p, exe, true, move || ctx.request_repaint());
                }
            }
            Some(Act::Discard) => self.phase = Phase::Idle,
            Some(Act::Reveal(path)) => reveal(&path),
            Some(Act::Refresh) => {
                let ctx = ui.ctx().clone();
                self.refresh(move || ctx.request_repaint());
            }
            None => {}
        }
    }
}

/// The contents before saving: moments and reasons, image count, total size, what was removed.
fn contents(ui: &mut egui::Ui, p: &Prepared) {
    let small = |ui: &mut egui::Ui, text: &str, colour| {
        ui.add(egui::Label::new(RichText::new(text).small().color(colour)).wrap());
    };
    // One line, cut with an ellipsis when narrow; the whole text in the hover, once: the label's own tooltip when
    // cut is turned off, so the two never show together.
    let one_line = |ui: &mut egui::Ui, text: &str, full: &str, colour| {
        ui.add(egui::Label::new(RichText::new(text).small().color(colour)).truncate().show_tooltip_when_elided(false)).on_hover_text(full);
    };
    ui.label(RichText::new(trf!("{} \u{b7} {}", run_time(&p.run), p.game.map_or("", Game::name))).strong());
    small(ui, &trf!("{} images, {} in all ({} for the moves)", p.images, size_text(p.zip.len() as u64), p.step_images), theme::TEXT);
    // Private builds only: a zip with the English client's whole frames says so before it is saved.
    #[cfg(feature = "dev-samples")]
    if let Some(n) = p.notes.first().filter(|n| p.images > 0 && (n.starts_with("含整張畫面") || n.starts_with("Includes"))) {
        small(ui, n, theme::WARN);
    }
    let (k, drop) = p.level;
    if k > 0 || drop > 0 {
        let l = LEVELS[k];
        let full = trf!("Shrunk to fit 20 MB: one image every {} moves, {} s around each moment, width {}, quality {}.", l.step_every, l.window_s, l.width, l.quality);
        one_line(ui, &trf!("Shrunk to fit 20 MB (width {})", l.width), &full, theme::WARN);
    }
    small(ui, &trf!("Suspicious moments: {}", p.moments.len()), MUTED);
    egui::ScrollArea::vertical().id_salt("report-moments").max_height(120.0).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        if p.moments.is_empty() {
            small(ui, tr("None found: only the moves' screenshots are included."), MUTED);
        }
        for (at, priority, reason, n, skipped) in &p.moments {
            let when = local_clock(at);
            let mut full = format!("{} P{} \u{b7} {} \u{b7} {}", when, priority, reason, trf!("{} images", n));
            if *skipped {
                full.push_str(&format!("\n{}", tr("Some images of this moment were left out: they may show personal information.")));
            }
            let colour = if *priority == 1 { theme::WARN } else { theme::TEXT };
            one_line(ui, &format!("{} P{} {}", when, priority, reason), &full, colour);
        }
    });
    // What was removed: a one line summary, the counts and the image rules in its hover.
    let r = &p.removed;
    let mut detail = vec![
        trf!("- Window titles: {} window lines deleted", r.window_rows),
        trf!("- Local paths: {} replaced with <路徑>", r.paths),
        trf!("- Windows user name: {} replaced with <使用者>", r.users),
        trf!("- Install id: {} removed", r.install_ids),
        trf!("- Long numbers (ids): {} replaced with <數字>", r.ids),
        tr(IMAGES_RULE).to_string(),
    ];
    detail.extend(p.notes.iter().cloned());
    let summary = tr("Removed: window titles, local paths, user name, install id; Tartarus UID corners blacked out");
    one_line(ui, summary, &format!("{summary}\n{}", detail.join("\n")), MUTED);
}

/// The preview entry point (ASCENDANT_PREVIEW_REPORT=run folder): exports that run when the app opens.
pub fn preview_dir() -> Option<PathBuf> {
    std::env::var_os("ASCENDANT_PREVIEW_REPORT").map(PathBuf::from).filter(|d| d.join("log.jsonl").is_file())
}

/// The Run tab's review as a [`Settled`], borrowed: it is cloned only when an export starts.
pub fn settled_of(out: Option<&(PathBuf, Outcome)>) -> Option<&Settled> {
    match out {
        Some((_, Outcome::Settled(s))) => Some(s.as_ref()),
        _ => None,
    }
}

/// Whether ASCENDANT_PREVIEW_REPORT_SAVE may save by itself: only in a debug build or one built with
/// ASCENDANT_PREVIEW_BUILD set (for screenshots); a release build for players ignores it.
pub fn preview_save_allowed() -> bool {
    cfg!(debug_assertions) || option_env!("ASCENDANT_PREVIEW_BUILD").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(v: Value) -> String {
        v.to_string()
    }

    /// Private builds only: a Vorax run's sample frames are shots treated whole with the id corners blacked out,
    /// every one is picked (evenly spread over the cap), and the listing's note says the zip has whole frames.
    #[cfg(feature = "dev-samples")]
    #[test]
    fn sample_frames_go_in_whole_and_masked() {
        let dir = std::env::temp_dir().join(format!("ascendant-report-samples-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        for k in 0..100 {
            let file = format!("sample-{:04}.jpg", k + 1);
            std::fs::write(dir.join(&file), b"x").unwrap();
            text.push_str(&line(json!({"at": t(&format!("00:{:02}.{:03}", k / 10, (k % 10) * 100)), "kind": "vorax_sample", "file": file, "why": "interval"})));
            text.push('\n');
        }
        // A sample row whose file is gone, and a row of another kind naming a sample file: neither is a shot.
        text.push_str(&line(json!({"at": t("00:20.000"), "kind": "vorax_sample", "file": "sample-0999.jpg"})));
        text.push('\n');
        text.push_str(&line(json!({"at": t("00:21.000"), "kind": "vorax_note", "file": "sample-0001.jpg"})));
        let shots = vorax_shots(&dir, &text);
        assert_eq!(shots.len(), 100);
        assert!(shots.iter().all(|s| s.treat == SAMPLE_TREAT && masked(s.treat)));
        let picks = with_samples(Vec::new(), &shots);
        assert_eq!(picks.len(), SAMPLE_PICKS);
        assert_eq!((picks[0].shot.file.as_str(), picks[79].shot.file.as_str()), ("sample-0001.jpg", "sample-0099.jpg"));
        // Already picked (near a moment): not twice.
        let again = with_samples(picks.clone(), &shots);
        assert_eq!(again.len(), SAMPLE_PICKS);
        // Even through the treatments, a sample frame is masked and kept whole.
        let img = Rgba { w: 2560, h: 1440, data: vec![255; 2560 * 1440 * 4] };
        let out = treat_image(img.clone(), SAMPLE_TREAT, &mut None).unwrap();
        assert_eq!((out.w, out.h), (2560, 1440));
        for r in id_rects(2560, 1440) {
            assert_eq!(out.rgb(r.x + r.w / 2, r.y + r.h / 2), [0, 0, 0], "{r:?}");
        }
        // The blocks grow with the interface scale: at scale 1 they are the usual ones; at the largest setting the
        // UID (0.486 of the height from the right edge) is covered.
        assert_eq!(sample_rects(2560, 1440, 1.0), id_rects(2560, 1440));
        let uid = sample_rects(2560, 1440, 1.5)[0];
        assert!(2560 - uid.x >= (0.486 * 1440.0) as i32, "{uid:?}");
        // process: whole, at its width, the id blocks of the run's scale blacked out (decoded back from JPEG).
        std::fs::write(dir.join("sample-0001.jpg"), encode_jpeg(&img, 90).unwrap()).unwrap();
        let shot = Shot { ms: 0, file: "sample-0001.jpg".into(), treat: SAMPLE_TREAT };
        let out = decode_jpeg(&process(&dir, &shot, 2560, 90, &mut Some(1.5)).unwrap().unwrap()).unwrap();
        assert_eq!((out.w, out.h), (2560, 1440));
        let at_uid = (2560 - (0.47 * 1440.0) as i32, 1440 - 40);
        assert!(out.rgb(at_uid.0, at_uid.1)[0] < 40, "UID at the largest scale");
        assert!(out.rgb(1280, 400)[0] > 215, "the board kept");
        let at_one = decode_jpeg(&process(&dir, &shot, 2560, 90, &mut Some(1.0)).unwrap().unwrap()).unwrap();
        assert!(at_one.rgb(at_uid.0, at_uid.1)[0] > 215, "scale 1 leaves that spot");
        // The run's scale: the largest ui_scale row over the measured setting, at least 1; 1.5 without one.
        let scaled = format!("{}
{}", line(json!({"at": t("00:00.000"), "kind": "ui_scale", "scale": 0.841017})), line(json!({"at": t("00:01.000"), "kind": "ui_scale", "scale": 1.0})));
        assert!((sample_scale(&scaled) - 1.0 / 0.667).abs() < 1e-3);
        assert!((sample_scale(&line(json!({"at": t("00:00.000"), "kind": "ui_scale", "scale": 0.5}))) - 1.0).abs() < 1e-6);
        assert_eq!(sample_scale(""), SAMPLE_SCALE_DEFAULT);
        assert!(is_sample("sample-0001.jpg") && !is_sample("samples.jpg") && !is_sample("frame-0001.jpg") && !is_sample("sample-0001.png"));
        assert!(sample_note(3).contains('3'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn t(s: &str) -> String {
        format!("2026-11-04T21:{s}Z")
    }

    #[test]
    fn tartarus_moments_cover_every_event() {
        crate::lang::set(Lang::En);
        let log = [
            line(json!({"at": t("15:31.863"), "kind": "window", "ocr": "zh-Hant-TW", "window": "\"Torchlight: Infinite\" (2560 x 1440)"})),
            line(json!({"at": t("15:32.229"), "kind": "miss", "screen": "Unknown", "line": "no board", "frame": null})),
            line(json!({"at": t("15:33.000"), "kind": "miss", "screen": "Patrol", "line": "doors not up yet", "frame": null})),
            line(json!({"at": t("15:34.000"), "kind": "miss", "screen": "Patrol", "line": "doors not up yet", "frame": null})),
            line(json!({"at": t("15:35.000"), "kind": "read", "screen": "Patrol", "warnings": ["Check cell 4: read 0.5"], "problems": [], "model": {"day": 3}})),
            line(json!({"at": t("15:36.000"), "kind": "read", "screen": "Patrol", "warnings": ["Check cell 9: read 0.7"], "problems": ["No doors"], "model": {"day": 3}})),
            line(json!({"at": t("15:37.000"), "kind": "edit", "cell": 5, "edit": "Sealed"})),
            line(json!({"at": t("15:38.000"), "kind": "edit", "tools": ["chains", "axe"]})),
            line(json!({"at": t("15:39.000"), "kind": "edit", "theme": "Sewer"})),
            // Normal passing states: counted, never a moment of their own.
            line(json!({"at": t("15:40.000"), "kind": "miss", "screen": "Patrol", "line": "The board changed: reading it again to confirm.", "frame": null})),
            line(json!({"at": t("15:41.000"), "kind": "read", "screen": "TreasurePick", "warnings": ["The pick covers the board: the cells are from the last board read (day 4)."], "problems": [], "model": {"day": 4}})),
            line(json!({"at": t("15:50.000"), "kind": "run_end", "vaulted": 3})),
        ]
        .join("\n");
        let worst = settle::Step { day: 3, read_at: t("15:35.000"), screen: "Patrol", made: "Go to 04".into(), best: "Go to 06".into(), advice: "Go to 04".into(), followed: Some(true), loss: 1.25, gap_se: 0.1, noise: false };
        let start = super::super::latency::log_millis(&t("15:31.863")).unwrap();
        let extra = Extra {
            worst: Some(worst),
            health: vec!["Day 3: clicked with no advice on show.".into(), "The result fell far below.".into()],
            crashes: vec![(start + 10_000, "thread 'main': boom".into()), (start - 999_999, "old".into())],
            differs: vec![(t("15:36.000"), "Go to 04".into(), "Go to 06".into())],
        };
        let found = tartarus_moments(&log, &extra);
        let m = &found.moments;
        let reasons: Vec<&str> = m.iter().map(|m| m.reason.as_str()).collect();
        let has = |s: &str| reasons.iter().filter(|r| r.contains(s)).count();
        // The normal passing states are only in the summary line.
        assert_eq!((has("board changed"), has("pick covers")), (0, 0), "{reasons:#?}");
        assert_eq!(
            found.summary().as_deref(),
            Some("Also 2 normal passing states not listed (no images): board changed, read again to confirm 1 times (21:15:40 to 21:15:40), pick covering the board 1 times (21:15:41 to 21:15:41).")
        );
        assert_eq!(m.len(), 11, "{reasons:#?}");
        // An Unknown screen's miss does not count; Patrol's line is reported once, with 2.
        assert_eq!(has("Read failed"), 1, "{reasons:#?}");
        assert!(reasons.iter().any(|r| r.starts_with("Read failed (Patrol): doors not up yet (2 times")));
        // Warnings with other numbers count as one; problems count too.
        assert_eq!(has("Read warning: Check cell"), 1);
        assert!(reasons.iter().any(|r| r.contains("Check cell 4: read 0.5 (2 times")));
        assert_eq!(has("Read warning: No doors"), 1);
        assert_eq!(has("Manual correction"), 3);
        assert!(reasons.contains(&"Manual correction: cell 5: Sealed") && reasons.contains(&"Manual correction: instruments set to chains, axe") && reasons.contains(&"Manual correction: theme set to Sewer"));
        assert_eq!(has("differs from the advice"), 1);
        assert_eq!(has("Largest loss"), 1);
        assert_eq!(has("Run check"), 2);
        assert_eq!(has("crash.log"), 1);
        // An issue with a day goes at that day's first board read; one without at the run's end.
        let check = |s: &str| m.iter().find(|x| x.reason.contains(s)).unwrap().at.clone();
        assert_eq!(check("Day 3"), t("15:35.000"));
        assert_eq!(check("fell far below"), t("15:50.000"));
        assert_eq!(check("Largest loss"), t("15:35.000"));
        // Priorities.
        let p = |s: &str| m.iter().find(|x| x.reason.contains(s)).unwrap().priority;
        assert_eq!((p("Manual"), p("Read failed"), p("Read warning"), p("differs"), p("Largest"), p("Run check"), p("crash")), (1, 2, 3, 2, 1, 1, 1));
        assert!(m.windows(2).all(|w| w[0].ms <= w[1].ms));
    }

    /// The index in CALM of the kind with this label.
    fn calm(label: &str) -> usize {
        CALM.iter().position(|c| c.label == label).unwrap()
    }

    /// Normal passing states are counted, in either language, and never listed; a between-steps line counts only on
    /// an Unknown screen, and once per read; an Unknown screen's miss is not counted at all.
    #[test]
    fn normal_passing_states_are_not_moments() {
        crate::lang::set(Lang::En);
        let miss = |s: &str, screen: &str, text: &str| line(json!({"at": t(s), "kind": "miss", "screen": screen, "line": text, "frame": null}));
        let read = |s: &str, screen: &str, warnings: Value, problems: Value| line(json!({"at": t(s), "kind": "read", "screen": screen, "warnings": warnings, "problems": problems, "model": {"day": 2}}));
        let between = "No destination cards or decision buttons on screen: take the screenshot when they show.";
        let doors = "Mark today's doors: tick them under \u{201c}Select a Destination\u{201d}, or select a cell and press Today's door.";
        let log = [
            miss("10:00.000", "Patrol", "The board changed: reading it again to confirm."),
            miss("10:01.000", "Manage", "盤面有變化：再讀一次確認。"),
            miss("10:02.000", "Unknown", "The board changed: reading it again to confirm."),
            miss("10:03.000", "Popup", "彈出視窗蓋住了遊戲：顯示上次的讀取。"),
            miss("10:04.000", "Theme", "本局主題：濫權。"),
            read("10:05.000", "Unknown", json!([between]), json!([doors])),
            read("10:06.000", "TreasurePick", json!(["選單蓋住盤面：牢房用上次讀到的（第 2 天）", "The days counter is hidden: 9 day(s) until Riot kept from the last read."]), json!([])),
            // On a patrol screen the doors not read are a failure.
            read("10:07.000", "Patrol", json!([between]), json!([])),
            // The pick's Chinese as builds up to 2026-11-04 logged it.
            read("10:08.000", "InstrumentPick", json!(["選擇畫面蓋住了盤面：牢房取自上次讀到的盤面（第 2 天）。"]), json!([])),
            line(json!({"at": t("10:30.000"), "kind": "run_end"})),
        ]
        .join("\n");
        let found = tartarus_moments(&log, &Extra::default());
        let reasons: Vec<&str> = found.moments.iter().map(|m| m.reason.as_str()).collect();
        assert_eq!(reasons, vec![format!("Read warning: {between} (1 times this run)").as_str()]);
        let n = |label: &str| found.counts[calm(label)];
        assert_eq!(
            (n("board changed, read again to confirm"), n("popup over the game"), n("screen with nothing to solve"), n("board read between steps"), n("pick covering the board"), n("days counter covered")),
            (2, 1, 1, 1, 2, 1)
        );
        assert_eq!(
            found.summary().as_deref(),
            Some(
                "Also 8 normal passing states not listed (no images): board changed, read again to confirm 2 times (21:10:00 to 21:10:01), popup over the game 1 times (21:10:03 to 21:10:03), pick covering the board 2 times (21:10:06 to 21:10:08), days counter covered 1 times (21:10:06 to 21:10:06), board read between steps 1 times (21:10:05 to 21:10:05), screen with nothing to solve 1 times (21:10:04 to 21:10:04)."
            )
        );
        // Nothing left out: no line.
        assert_eq!(TartarusMoments::default().summary(), None);
    }

    /// A kind over its limit in a run is listed once, P3, at its first line, and leaves the summary; at the limit it
    /// stays quiet.
    #[test]
    fn unusually_many_normal_states_are_listed_once() {
        crate::lang::set(Lang::En);
        let day = calm("new day not drawn yet");
        let popup = calm("popup over the game");
        let (day_limit, popup_limit) = (CALM[day].limit.max(CALM_FLOOR), CALM[popup].limit.max(CALM_FLOOR));
        let mut log = Vec::new();
        for k in 0..=day_limit {
            log.push(line(json!({"at": t(&format!("20:{:02}.000", k)), "kind": "miss", "screen": "Patrol", "line": "The new day is not on the board yet: showing the last reading."})));
        }
        for k in 0..popup_limit {
            log.push(line(json!({"at": t(&format!("21:{:02}.{:03}", k / 1000, k % 1000)), "kind": "miss", "screen": "Popup", "line": "A popup covers the game: showing the last reading."})));
        }
        let found = tartarus_moments(&log.join("\n"), &Extra::default());
        assert_eq!(found.moments.len(), 1, "{:#?}", found.moments);
        let m = &found.moments[0];
        assert_eq!(
            m.reason,
            format!("Unusually many (new day not drawn yet): The new day is not on the board yet: showing the last reading. ({} times this run, normally at most {day_limit})", day_limit + 1)
        );
        assert_eq!((m.priority, m.at.as_str()), (3, t("20:00.000").as_str()));
        assert_eq!(found.quiet, vec![Quiet { label: "popup over the game", n: popup_limit, first: t("21:00.000"), last: t(&format!("21:00.{:03}", popup_limit - 1)) }]);
        assert_eq!((found.counts[day], found.counts[popup]), (day_limit + 1, popup_limit));
    }

    /// Low-confidence warnings are moments only on a decision screen, when the same cell and sentence repeat in
    /// consecutive reads or a very low one is contradicted by the next read of the same screen and day.
    #[test]
    fn low_confidence_warnings_need_to_repeat_or_flip() {
        crate::lang::set(Lang::En);
        let read = |s: &str, screen: &str, warning: &str, cells: Value| line(json!({"at": t(s), "kind": "read", "screen": screen, "warnings": [warning], "problems": [], "model": {"day": 3, "cells": cells}}));
        let rat = |tier: i64| json!({"kind": "inmate", "beast": "rat", "tier": tier});
        let cells = |n: usize, cell: Value| {
            let mut c = vec![json!({"kind": "sealed"}); 16];
            c[n] = cell;
            Value::Array(c)
        };
        let log = [
            // Once, and the next read has another warning: quiet.
            read("30:00.000", "Patrol", "Check cell 05: beast read as Normal Lion (confidence 0.20).", cells(4, rat(0))),
            // Two reads in a row: listed once.
            read("30:01.000", "Patrol", "Check cell 07: read as open (confidence 0.25).", cells(6, rat(0))),
            read("30:02.000", "Patrol", "Check cell 07: read as open (confidence 0.22).", cells(6, rat(0))),
            // Very low, and the next read of the screen and day differs: listed.
            read("30:03.000", "Manage", "Check cell 09: rarity read as Magic Rat (confidence 0.05).", cells(8, rat(1))),
            // Very low, but the next read is another screen: quiet.
            read("30:04.000", "Manage", "Check cell 10: rarity read as Magic Rat (confidence 0.05).", cells(9, rat(1))),
            read("30:05.000", "Patrol", "Check cell 11: rarity read as Rare Rat (confidence 0.29).", cells(9, rat(2))),
            // Between steps (Unknown) in a row: quiet.
            read("30:06.000", "Unknown", "Check cell 03: read as locked (confidence 0.01).", cells(2, rat(0))),
            read("30:07.000", "Unknown", "Check cell 03: read as locked (confidence 0.02).", cells(2, rat(0))),
            // Logged in Chinese, in a row: listed.
            read("30:08.000", "Patrol", "檢查牢房 12：囚犯讀為普通狂獅（信心 0.10）。", cells(11, rat(0))),
            read("30:09.000", "Patrol", "檢查牢房 12：囚犯讀為普通狂獅（信心 0.12）。", cells(11, rat(0))),
        ]
        .join("\n");
        let found = tartarus_moments(&log, &Extra::default());
        let reasons: Vec<&str> = found.moments.iter().map(|m| m.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec![
                "Read warning: Check cell 07: read as open (confidence 0.25). (in 2 reads in a row; 2 times this run)",
                "Read warning: Check cell 09: rarity read as Magic Rat (confidence 0.05). (very low, and the next read differs; 1 times this run)",
                "Read warning: 檢查牢房 12：囚犯讀為普通狂獅（信心 0.10）。 (in 2 reads in a row; 2 times this run)",
            ]
        );
        assert!(found.moments.iter().all(|m| m.priority == 3));
        // Cells 05, 10, 11 and the two between steps.
        assert_eq!(found.quiet, vec![Quiet { label: LOW_LABEL, n: 5, first: t("30:00.000"), last: t("30:07.000") }]);
    }

    /// Every sentence the rules match has its Chinese, and a filled sentence fits its template in either language.
    #[test]
    fn calm_and_low_confidence_sentences_match_both_languages() {
        for en in CALM.iter().flat_map(|c| c.lines.iter()).chain(LOW_CONFIDENCE.iter()) {
            assert_ne!(lang::tr_in(Lang::ZhTw, en), *en, "{en} has no Chinese");
        }
        for label in CALM.iter().map(|c| c.label).chain([LOW_LABEL]) {
            assert_ne!(lang::tr_in(Lang::ZhTw, label), label, "{label} has no Chinese");
        }
        assert!(fits("Check cell 05: read as open (confidence 0.20).", LOW_CONFIDENCE[0]));
        assert!(!fits("Check cell 05: read as open (confidence 0.20)", LOW_CONFIDENCE[0]));
        assert!(fits("檢查典獄長：讀在 07 號（信心 0.15）。", lang::tr_in(Lang::ZhTw, LOW_CONFIDENCE[6])));
        assert_eq!(low_confidence("檢查典獄長：讀在 07 號（信心 0.15）。"), Some((LowKey { sentence: 6, cell: None }, 0.15)));
        assert_eq!(low_confidence("Check cell 12: beast read as Normal Lion (confidence 0.03)."), Some((LowKey { sentence: 3, cell: Some(11) }, 0.03)));
        // The rarity warning is not the beast one, though both end alike.
        assert_eq!(low_confidence("檢查牢房 02：稀有度讀為普通狂獅（信心 0.23）。").map(|l| l.0.sentence), Some(4));
        assert_eq!(calm_kind("暴動倒數被遮住：沿用上次讀到的距離暴動 16 天。", false), Some(calm("days counter covered")));
        assert_eq!(calm_kind("The prison board is not on screen: showing the last reading.", false), Some(calm("prison board off screen")));
        assert_eq!(calm_kind("Read 2 of the 3 instruments on offer.", false), None);
        // The legacy Chinese of the pick still matches; another sentence that starts alike does not.
        let pick = Some(calm("pick covering the board"));
        assert_eq!(calm_kind("選擇畫面蓋住了盤面：牢房取自上次讀到的盤面（第 19 天）。", false), pick);
        assert_eq!(calm_kind("選擇畫面蓋住了盤面，之前也沒讀過盤面：假設為開局。", false), None);
    }

    /// An export belongs to its run: once another run (or an unfinished one, or none) is to be exported, an export
    /// being prepared (cancelled), listed, saved or failed goes and the card is back to "Prepare"; the same run, or
    /// the list being read again, keeps it; only a zip being written is finished first.
    #[test]
    fn a_finished_export_goes_when_another_run_is_picked() {
        let (a, b) = (PathBuf::from("C:/app/vorax/2026-11-06_16-24-14"), PathBuf::from("C:/app/runs/2026-11-06_16-19-57"));
        let saved = || Phase::Saved(PathBuf::from("C:/out/a.zip"), Box::default());
        let mut ui = ReportUi { exporting_dir: Some(a.clone()), phase: saved(), ..ReportUi::default() };
        ui.follow(&Target::Run(a.clone(), Game::Vorax));
        ui.follow(&Target::Waiting);
        assert!(matches!(ui.phase, Phase::Saved(..)), "the same run, or the list read again: kept");
        ui.follow(&Target::Run(b.clone(), Game::Tartarus));
        assert!(matches!(ui.phase, Phase::Idle), "another run: Prepare again");

        for target in [Target::Newest(b.clone(), Game::Tartarus, String::new()), Target::Unfinished, Target::Nothing] {
            for phase in [Phase::Failed("x".into()), Phase::Ready(Box::default())] {
                let mut ui = ReportUi { exporting_dir: Some(a.clone()), phase, ..ReportUi::default() };
                ui.follow(&target);
                assert!(matches!(ui.phase, Phase::Idle), "{target:?}");
            }
        }
        let listed = |phase| ReportUi { exporting_dir: Some(a.clone()), phase, ..ReportUi::default() };
        let mut ui = listed(Phase::Ready(Box::default()));
        ui.follow(&Target::Run(a.clone(), Game::Vorax));
        assert!(matches!(ui.phase, Phase::Ready(..)), "listed, the same run: kept");
        ui.follow(&Target::Run(b.clone(), Game::Tartarus));
        assert!(matches!(ui.phase, Phase::Idle), "listed, another run: Prepare again");

        // Being prepared: cancelled and gone; the thread's later messages go nowhere.
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ui = listed(Phase::Working(Export { rx, cancel: cancel.clone() }, String::new()));
        ui.follow(&Target::Waiting);
        assert!(matches!(ui.phase, Phase::Working(..)) && !cancel.load(Ordering::Relaxed), "the list read again: carries on");
        ui.follow(&Target::Unfinished);
        assert!(matches!(ui.phase, Phase::Idle) && cancel.load(Ordering::Relaxed), "another run: cancelled");
        assert!(tx.send(Msg::Status(String::new())).is_err(), "nobody listens any more");

        // Being written: finished first, then back to Prepare once saved.
        let (save_tx, save_rx) = channel();
        let mut ui = listed(Phase::Saving(save_rx, Box::default(), false));
        ui.follow(&Target::Unfinished);
        assert!(matches!(ui.phase, Phase::Saving(..)), "being saved: carries on");
        save_tx.send(Ok(PathBuf::from("C:/out/a.zip"))).unwrap();
        ui.poll(None, || {});
        assert!(matches!(ui.phase, Phase::Saved(..)));
        ui.follow(&Target::Unfinished);
        assert!(matches!(ui.phase, Phase::Idle), "saved, the run changed meanwhile: Prepare");
    }

    #[test]
    fn crash_log_entries() {
        let text = "1762290000 1.0.0 thread 'main': panicked at src/x.rs:1:1\nstack backtrace:\n   0: foo\n1762290100 1.0.0 thread 'settle-0': oops\n";
        assert_eq!(crashes(text), vec![(1_762_290_000_000, "thread 'main': panicked at src/x.rs:1:1".to_string()), (1_762_290_100_000, "thread 'settle-0': oops".to_string())]);
    }

    #[test]
    fn vorax_moments_follow_the_spec() {
        crate::lang::set(Lang::En);
        let card = |en: Value| json!({"en": en, "rawName": "化骨油膏", "rawType": "藥劑", "why": "no match"});
        let read = |at: &str, sig: &str, round: Value, cards: Vec<Value>, extra: Value| {
            let mut reading = json!({"kind": "potion", "round": round, "cards": cards, "tanks": [], "held": [], "problems": []});
            for (k, v) in extra.as_object().unwrap() {
                reading[k] = v.clone();
            }
            line(json!({"at": t(at), "kind": "vorax_read", "steady": true, "gate": false, "signature": sig, "frame": "frame-0001.jpg", "reading": reading}))
        };
        let log = [
            // 1a-1e: one moment per board (signature), even when a later line of it reads something else.
            read("00:01.000", "A", json!(1), vec![card(Value::Null), card(json!("X"))], json!({})),
            read("00:02.000", "A", json!(1), vec![card(Value::Null), card(Value::Null)], json!({})),
            // Not steady or gated: no; a result screen (the reading's kind): no.
            line(json!({"at": t("00:02.500"), "kind": "vorax_read", "steady": false, "gate": false, "signature": "Z", "reading": {"kind": "potion", "round": null, "cards": [{"en": null}]}})),
            line(json!({"at": t("00:02.600"), "kind": "vorax_read", "steady": true, "gate": false, "signature": "Y", "reading": {"kind": "result", "round": null, "cards": [{"en": null}]}})),
            // 1b, 1c, 1d, 1e.
            read(
                "00:03.000",
                "B",
                Value::Null,
                vec![],
                json!({"tanks": [{"empty": false, "archetype": null, "rarity": "n", "activity": 1, "quantity": 1, "rawName": "蟲", "rawValues": "1x3", "rawTotal": 3}, {"empty": true, "archetype": null}], "held": [{"id": null, "opening": false}, {"id": null, "opening": true}], "problems": ["水箱讀不到"]}),
            ),
            // 1f.
            line(json!({"at": t("00:04.000"), "kind": "vorax_unknown", "what": "card", "rawName": "怪名", "frame": "learn-0002.jpg", "crop": "learn-0002-crop.jpg"})),
            // 1g: never a moment; its crop goes with the 1a of the tooltip line's own round (1), not another round's (2).
            line(json!({"at": t("00:04.500"), "kind": "vorax_tooltip", "matched": null, "raw": "提示", "round": 1, "crop": "learn-0003-crop.jpg"})),
            read("00:05.000", "C", json!(2), vec![card(json!("X"))], json!({})),
            line(json!({"at": t("00:05.500"), "kind": "vorax_tooltip", "matched": null, "raw": "別的", "round": 2, "crop": "learn-0004-crop.jpg"})),
            // 1h, 1i, 1j (a reroll is no), 1k.
            line(json!({"at": t("00:06.000"), "kind": "vorax_pick_mismatch", "card": 1, "icon": 7, "picked": "孵化囊"})),
            line(json!({"at": t("00:07.000"), "kind": "vorax_pending_stuck", "since": t("00:01.000"), "ms": 6000})),
            line(json!({"at": t("00:08.000"), "kind": "vorax_round", "phase": "potion", "chosen": {"card": null}})),
            line(json!({"at": t("00:08.500"), "kind": "vorax_round", "phase": "reroll", "chosen": {"card": null}})),
            line(json!({"at": t("00:09.000"), "kind": "vorax_end", "ultimate": null, "roundsSeen": [1, 2], "held": []})),
            // 1o, 1l, 1m, 1n.
            line(json!({"at": t("00:10.000"), "kind": "vorax_battle", "wave": 2})),
            line(json!({"at": t("00:11.000"), "kind": "vorax_battle", "wave": 1})),
            line(json!({"at": t("00:12.000"), "kind": "vorax_battle_end", "board": 1000, "reads": 0, "frame": "frame-battle-0009.jpg"})),
            line(json!({"at": t("00:13.000"), "kind": "vorax_battle_end", "board": 1000, "activity": 900, "reads": 3, "bonus": 1, "next": null, "percent": 5, "bonusShare": 0.5})),
            // Events that are not suspicious.
            line(json!({"at": t("00:14.000"), "kind": "vorax_early_dropped", "why": "x"})),
            line(json!({"at": t("00:14.100"), "kind": "vorax_resume"})),
            line(json!({"at": t("00:14.200"), "kind": "vorax_mode"})),
        ]
        .join("\n");
        let m = vorax_moments(&log);
        let reasons: Vec<&str> = m.iter().map(|m| m.reason.as_str()).collect();
        let count = |s: &str| reasons.iter().filter(|r| r.starts_with(s)).count();
        assert_eq!(count("Card 1 not recognised"), 1, "{reasons:#?}");
        assert_eq!(reasons.iter().filter(|r| r.contains("Card 2")).count(), 0);
        // Board B: its four findings in one moment, at the highest priority; 1b without JSON quotes.
        let b: Vec<&&str> = reasons.iter().filter(|r| r.starts_with("Tank 1")).collect();
        assert_eq!(b, [&"Tank 1 only partly read: 蟲 1x3 3; Held instrument icon 1 not recognised; Read problem: 水箱讀不到; Round not read"]);
        assert_eq!(count("Observe mode: unknown card: 怪名"), 1);
        assert_eq!(reasons.iter().filter(|r| r.contains("Tooltip")).count(), 0);
        let card1 = m.iter().find(|x| x.reason.starts_with("Card 1")).unwrap();
        assert_eq!(card1.files, ["frame-0001.jpg", "learn-0003-crop.jpg"]);
        assert!(m.iter().all(|x| !x.files.iter().any(|f| f == "learn-0004-crop.jpg")));
        assert_eq!(count("Picked instrument does not match"), 1);
        assert_eq!(count("Board did not settle for 6.0 s"), 1);
        assert_eq!(count("Could not tell which card was picked"), 1);
        assert_eq!(count("Final activity on the result screen not read"), 1);
        assert_eq!(count("Battle wave went backwards (2 → 1)"), 1);
        assert_eq!(count("Endgame battle panel never read"), 1);
        assert_eq!(count("Battle activity 900 is below the board's final 1000"), 1);
        assert_eq!(count("Battle end numbers incomplete"), 1);
        assert_eq!(m.len(), 11);
        let battle = m.iter().find(|x| x.reason.starts_with("Endgame")).unwrap();
        assert!(battle.battle && battle.files == ["frame-battle-0009.jpg"]);
        let unknown = m.iter().find(|x| x.reason.starts_with("Observe")).unwrap();
        assert_eq!(unknown.files, ["learn-0002.jpg", "learn-0002-crop.jpg"]);
        let p = |s: &str| m.iter().find(|x| x.reason.starts_with(s)).unwrap().priority;
        assert_eq!((p("Card"), p("Tank"), p("Could not"), p("Board did not")), (1, 1, 3, 2));
    }

    #[test]
    fn clean_log_removes_titles_paths_user_and_install_id() {
        let scrub = Scrub {
            dirs: vec![r"D:\Home\alice\AppData\Roaming".into(), r"D:\Home\alice".into()],
            user: Some("Alice".into()),
            install_id: Some("0f8fad5b-d9cb-469f-a165-70867728950e".into()),
        };
        let log = [
            line(json!({"at": t("00:00.000"), "kind": "window", "ocr": "zh-Hant-TW", "window": "\"Torchlight: Infinite - Alice\" (2560 x 1440)"})),
            line(json!({"at": t("00:01.000"), "kind": "miss", "line": r"failed to open D:\Home\alice\AppData\Roaming\Ascendant\x.png, then E:\x\home\runs", "screen": "Patrol"})),
            line(json!({"at": t("00:02.000"), "kind": "note", "text": "d:/home/ALICE/desktop/a b.txt and \\\\nas\\share\\f", "who": "alice logged in", "install_id": "0f8fad5b-d9cb-469f-a165-70867728950e", "id": "x 0f8fad5b-d9cb-469f-a165-70867728950e"})),
            "not json E:\\data\\Alice.log".to_string(),
        ]
        .join("\n");
        let (out, removed) = clean_log(&log, &scrub);
        for bad in ["Alice", "alice", "ALICE", "D:\\\\Home", "D:\\Home", "E:\\", "E:\\\\", "d:/home", "\\\\nas", "Torchlight", "0f8fad5b", "install_id", "\"window\""] {
            assert!(!out.contains(bad), "{bad} left in:\n{out}");
        }
        assert_eq!(removed.window_rows, 1);
        assert!(removed.paths >= 4, "{removed:?}");
        assert!(removed.users >= 1 && removed.install_ids == 2, "{removed:?}");
        assert!(out.contains("<路徑>") && out.contains("<使用者>"));
        assert_eq!(out.lines().count(), 3);
        // What is no path stays.
        let mut r = Removed::default();
        assert_eq!(scrub_text("Day 3: 10:15 at x:y", &Scrub::default(), &mut r), "Day 3: 10:15 at x:y");
        assert_eq!(r, Removed::default());
    }

    #[test]
    fn degrades_in_order() {
        // Each combination's size: smaller at later levels and with more dropped. Gives the result and the combinations tried.
        let sizes = [100, 90, 80, 70, 60, 50, 40];
        let f = |limit: u64| {
            let mut tried = Vec::new();
            let out = fit(limit, |l, d| {
                let k = LEVELS.iter().position(|x| x == l).unwrap();
                tried.push((k, d));
                sizes[k] - u64::from(d) * 9
            });
            (out, tried)
        };
        assert_eq!(f(100).0, Some((0, 0)));
        assert_eq!(f(85), (Some((2, 0)), vec![(0, 0), (1, 0), (2, 0)]));
        let (out, tried) = f(15);
        assert_eq!(out, Some((6, 3)));
        assert_eq!(tried.last(), Some(&(6, 3)));
        assert_eq!(tried.len(), 7 + 3);
        assert_eq!(f(1).0, None);
        // Order: every two moves -> 3 s -> resolution -> quality.
        assert_eq!((LEVELS[0].step_every, LEVELS[1].step_every), (1, 2));
        assert_eq!((LEVELS[1].window_s, LEVELS[2].window_s), (5, 3));
        assert!(LEVELS[3].width < LEVELS[2].width && LEVELS[3].quality == LEVELS[2].quality);
        assert!(LEVELS.windows(2).skip(4).all(|w| w[1].quality < w[0].quality && w[1].width == w[0].width));
    }

    /// A frame of noise (compresses badly, like a big whole frame).
    fn noisy(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        let mut data = vec![0u8; w * h * 4];
        for p in data.as_chunks_mut::<4>().0 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            p.copy_from_slice(&[x as u8, (x >> 8) as u8, (x >> 16) as u8, 255]);
        }
        encode_jpeg(&Rgba { w, h, data }, 92).unwrap()
    }

    /// Writes a Tartarus run folder: a board read (whole) and an advice per move.
    fn tartarus_run(name: &str, steps: usize) -> PathBuf {
        tartarus_run_sized(name, steps, 1280, 720)
    }

    /// [`tartarus_run`] with `w` x `h` frames.
    fn tartarus_run_sized(name: &str, steps: usize, w: usize, h: usize) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ascendant-report-{name}-{}", std::process::id())).join("runs").join("2026-11-04_21-15-31");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = vec![line(json!({"at": t("15:31.000"), "kind": "window", "ocr": "zh-Hant-TW", "window": "\"Torchlight: Infinite\" (1280 x 720)"}))];
        for k in 0..steps {
            let at = t(&format!("{:02}:{:02}.000", 16 + k / 6, (k % 6) * 10));
            let frame = format!("frame-{:04}.jpg", k + 1);
            std::fs::write(dir.join(&frame), noisy(w, h, k as u32 + 1)).unwrap();
            log.push(line(json!({"at": at, "kind": "read", "screen": "Patrol", "frame": frame, "warnings": [], "model": {"day": k + 1}})));
            log.push(line(json!({"at": at, "kind": "advice", "read_at": at, "pick": "去 04 號", "rounds": 10})));
        }
        std::fs::write(dir.join("log.jsonl"), log.join("\n")).unwrap();
        dir
    }

    #[test]
    fn full_frames_degrade_under_the_limit() {
        crate::lang::set(Lang::En);
        let dir = tartarus_run("degrade", 12);
        let text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
        let (shots, steps) = (tartarus_shots(&dir, &text), steps(Game::Tartarus, &text));
        assert_eq!(shots.len(), 12);
        assert!(shots.iter().all(|s| s.treat == Treat::Full(Game::Tartarus)));
        let mut hint = None;
        let mut size = |l: &Level, d: u8| -> u64 {
            plan(Game::Tartarus, &shots, &steps, &[], l, d).iter().map(|p| process(&dir, &p.shot, l.width, l.quality, &mut hint).unwrap().unwrap().len() as u64).sum()
        };
        let all = size(&LEVELS[0], 0);
        let half = size(&LEVELS[1], 0);
        assert!(half * 10 < all * 6, "{half} of {all}");
        // A limit between all whole frames and every second move's: level 1; a little smaller needs resolution or quality.
        assert_eq!(fit(all - 1, &mut size), Some((1, 0)));
        let small = size(&LEVELS[4], 0);
        let k = fit(small, &mut size).unwrap();
        assert!(k.0 >= 4 && size(&LEVELS[k.0], k.1) <= small, "{k:?}");
        // A 1280 wide whole frame is not enlarged; at 720 it shrinks.
        let one = process(&dir, &shots[0], 720, 80, &mut hint).unwrap().unwrap();
        assert_eq!(decode_jpeg(&one).unwrap().w, 720);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn whole_export_stays_under_the_limit_and_only_holds_allowed_files() {
        crate::lang::set(Lang::En);
        let dir = tartarus_run("export", 6);
        let req = Request { dir: dir.clone(), game: Game::Tartarus, settled: None, install_id: String::new(), app_dir: None };
        let p = prepare(&req, &AtomicBool::new(false), &|_| {}).unwrap();
        assert!(p.zip.len() as u64 <= LIMIT);
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(p.zip.clone())).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        assert!(names.iter().all(|n| allowed(n)), "{names:?}");
        assert!(names.contains(&"log.jsonl".to_string()) && names.contains(&"report.txt".to_string()));
        assert_eq!(names.iter().filter(|n| n.ends_with(".jpg")).count(), 6);
        let mut log = String::new();
        std::io::Read::read_to_string(&mut z.by_name("log.jsonl").unwrap(), &mut log).unwrap();
        assert!(!log.contains("Torchlight") && !log.contains("\"window\""));
        // Cancelled: cancelled.
        let cancelled = prepare(&req, &AtomicBool::new(true), &|_| {});
        assert_eq!(cancelled.err().as_deref(), Some(CANCELLED));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn folder_names_show_in_local_time() {
        // UTC+8: the run of 13:12 UTC shows as 21:12, and the date moves on past midnight.
        assert_eq!(run_time_at("2026-11-05_13-12-57", 8 * 3600), "2026-11-05 21:12:57");
        assert_eq!(run_time_at("2026-11-05_20-00-00", 8 * 3600), "2026-11-06 04:00:00");
        assert_eq!(run_time_at("2026-12-31_23-30-00", 3600), "2027-01-01 00:30:00");
        assert_eq!(run_time_at("2026-03-01_02-00-00", -5 * 3600), "2026-02-28 21:00:00");
        // A suffix after the time is ignored; a name that is no run folder is given back.
        assert_eq!(run_time_at("2026-11-05_13-12-57-2", 0), "2026-11-05 13:12:57");
        assert_eq!(run_time_at("notes", 8 * 3600), "notes");
        assert_eq!(folder_utc("1970-01-01_00-00-00"), Some(0));
        assert_eq!(folder_utc("2026-11-05_13-12-57"), Some(1_793_884_377));
        // This computer's offset is whole quarter hours within a day.
        let offset = local_offset(1_793_884_377);
        assert!(offset % 900 == 0 && offset.abs() <= 14 * 3600, "{offset}");
        assert_eq!(run_time("2026-11-05_13-12-57"), run_time_at("2026-11-05_13-12-57", offset));
    }

    #[test]
    fn the_id_line_mask_covers_its_text_and_clears_the_cards() {
        // At 2560x1440 the id line ends at x 580 and the first of three decision cards starts at x 594: the mask
        // reaches at least 4 px past the text and stays left of the card.
        for h in [1440usize, 1080, 720, 2160] {
            let (w, k) = (h * 16 / 9, h as f32 / 1440.0);
            let [_, _, left] = id_rects(w, h);
            let right = (left.x + left.w) as f32;
            assert!(right >= (ID_LINE_TEXT_RIGHT + 4.0) * k, "{h}: {right}");
            assert!(right < FIRST_CARD_LEFT * k, "{h}: {right}");
        }
        let [_, _, left] = id_rects(2560, 1440);
        assert_eq!(left.w, 588);
        // It reaches up past the text's top (y 1220).
        assert!(left.y <= 1220, "{left:?}");
    }

    #[test]
    fn long_numbers_never_leave_in_an_export() {
        crate::lang::set(Lang::En);
        assert_eq!(long_number("SetGamePlayer 76561198012345678 ok"), Some("76561198012345678"));
        assert_eq!(long_number("1793884466768 and 12345678901234"), None);
        assert_eq!(scrub_long_numbers("id 123456789012345, 9").0, "id <\u{6578}\u{5b57}>, 9");
        // A crash message quoting a game log line is cut before it.
        let msg = crash_message("thread 'main': bad line [2026.11.05-13.12.57:123][  0]LogTemp: SetGamePlayer 76561198012345678");
        assert!(!msg.contains("SetGamePlayer") && !msg.contains("2026.11.05") && long_number(&msg).is_none(), "{msg}");
        assert!(msg.starts_with("thread 'main': bad line"), "{msg}");
        // A whole export: a player id in a warning, a long whole number field and a float of 17 digits in the log,
        // and crash.log quoting the game log during the run.
        let dir = tartarus_run("privacy", 3);
        let app = dir.parent().unwrap().parent().unwrap().to_path_buf();
        let mut text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
        text.push('\n');
        text.push_str(&line(json!({"at": t("16:05.000"), "kind": "read", "screen": "Patrol", "warnings": ["SetGamePlayer 76561198012345678 joined"], "model": {"day": 2}})));
        text.push('\n');
        text.push_str(&line(json!({"at": t("16:06.000"), "kind": "note", "player": 76_561_198_012_345_678u64, "ev": 0.1f64 + 0.2})));
        std::fs::write(dir.join("log.jsonl"), text).unwrap();
        let first = crate::app::latency::log_millis(&t("15:31.000")).unwrap() / 1000;
        let crash = format!("{} 1.0.0 thread 'main': panicked: [2026.11.04-21.16.00:001][  0]LogTemp: SetGamePlayer 76561198012345678 playerinfo\n", first + 40);
        std::fs::write(app.join("crash.log"), crash).unwrap();
        let req = Request { dir: dir.clone(), game: Game::Tartarus, settled: None, install_id: String::new(), app_dir: Some(app.clone()) };
        let p = prepare(&req, &AtomicBool::new(false), &|_| {}).unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(p.zip.clone())).unwrap();
        for name in ["log.jsonl", "report.txt"] {
            let mut body = String::new();
            std::io::Read::read_to_string(&mut z.by_name(name).unwrap(), &mut body).unwrap();
            assert_eq!(long_number(&body), None, "{name}");
            assert!(!body.contains("76561198"), "{name}");
            if name == "report.txt" {
                // The crash is quoted, without the game log's text.
                let crash = body.lines().find(|l| l.contains("crash.log")).unwrap_or_default();
                assert!(crash.contains("thread 'main': panicked"), "{body}");
                assert!(!crash.contains("SetGamePlayer") && !crash.to_ascii_lowercase().contains("playerinfo") && !crash.contains("LogTemp"), "{body}");
            }
        }
        assert!(p.removed.ids >= 2, "{:?}", p.removed);
        let _ = std::fs::remove_dir_all(&app);
    }

    #[test]
    fn id_masks_black_out_exactly_their_blocks() {
        // 16:9 and 21:9 frames of random pixels (never pure black): inside the blocks every pixel is black,
        // outside every pixel is unchanged.
        for (w, h) in [(2560, 1440), (1280, 720), (3440, 1440), (2560, 1080)] {
            let data: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8 | 1, (i % 241) as u8 | 1, (i % 239) as u8 | 1, 255]).collect();
            let before = Rgba { w, h, data };
            let mut after = before.clone();
            mask_ids(&mut after);
            let rects = id_rects(w, h);
            let inside = |x: usize, y: usize| rects.iter().any(|r| (r.x as usize..(r.x + r.w) as usize).contains(&x) && (r.y as usize..(r.y + r.h) as usize).contains(&y));
            for y in 0..h {
                for x in 0..w {
                    if inside(x, y) {
                        assert_eq!(after.rgb(x as i32, y as i32), [0, 0, 0], "{w}x{h} at {x},{y}");
                    } else {
                        assert_eq!(after.rgb(x as i32, y as i32), before.rgb(x as i32, y as i32), "{w}x{h} at {x},{y}");
                    }
                }
            }
            // The UID block reaches the corner and covers the measured text (x 0.818-0.983, y 0.903-0.968 of a 16:9 frame).
            let [right, chat, left] = rects;
            assert_eq!((right.x + right.w, right.y + right.h), (w as i32, h as i32));
            let k = h as f32 / 1440.0;
            let text_w = (0.983 - 0.818) * 2560.0 * k;
            assert!(right.w as f32 >= text_w + (1.0 - 0.983) * 2560.0 * k, "{right:?}");
            assert!(((h - right.y as usize) as f32) >= (1.0 - 0.903) * h as f32, "{right:?}");
            // The id line at the bottom left: x 22-420, y 921-960 of 1078, to the bottom edge.
            assert!(left.x == 0 && left.x + left.w >= (420.0 * h as f32 / 1078.0) as i32 && left.y <= (921.0 * h as f32 / 1078.0) as i32 && left.y + left.h == h as i32, "{left:?}");
            // The chat box: x up to 347 and y from 1208 of 1440 (the wrap width), to the bottom edge.
            assert!(chat.x == 0 && chat.x + chat.w >= (347.0 * k) as i32 && chat.y <= (1208.0 * k) as i32 && chat.y + chat.h == h as i32, "{chat:?}");
            // Neither reaches the first decision card of a three card day (x 594 at 2560x1440, centred).
            let card = (w as f32 / 2.0 - (1280.0 - 594.0) * k) as i32;
            assert!(left.x + left.w < card && chat.x + chat.w < card, "{left:?} {chat:?} card {card}");
        }
    }

    #[test]
    fn replace_ci_survives_case_changes_of_other_lengths() {
        // 'İ' lowercases to two characters (3 bytes), 'Ω' (U+2126) to 'ω' (2 bytes), 'K' (U+212A) to 'k' (1 byte).
        let mut t = "xİstanbul and Ωmega, KELVIN, İSTANBUL".to_string();
        assert_eq!(replace_ci(&mut t, "i̇stanbul", "<c>"), 2);
        assert_eq!(t, "x<c> and Ωmega, KELVIN, <c>");
        assert_eq!(replace_ci(&mut t, "ωMEGA", "<o>"), 1);
        assert_eq!(replace_ci(&mut t, "kelvin", "<k>"), 1);
        assert_eq!(t, "x<c> and <o>, <k>, <c>");
        // Needles ending or starting on such characters, and no match at all.
        let mut u = "İİİ".to_string();
        assert_eq!(replace_ci(&mut u, "İ", "-"), 3);
        assert_eq!(u, "---");
        let mut v = "abc".to_string();
        assert_eq!((replace_ci(&mut v, "İ", "-"), replace_ci(&mut v, "", "-"), v.as_str()), (0, 0, "abc"));
    }

    #[test]
    fn saving_never_replaces_a_file() {
        let dir = std::env::temp_dir().join(format!("ascendant-report-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = zip_name(Game::Tartarus, "2026-11-04_21-15-31");
        let a = write_new(&dir, &name, b"one").unwrap();
        let b = write_new(&dir, &name, b"two").unwrap();
        let c = write_new(&dir, &name, b"three").unwrap();
        assert_eq!(a.file_name().unwrap(), "Ascendant-report-tartarus-2026-11-04_21-15-31.zip");
        assert_eq!(b.file_name().unwrap(), "Ascendant-report-tartarus-2026-11-04_21-15-31-2.zip");
        assert_eq!(c.file_name().unwrap(), "Ascendant-report-tartarus-2026-11-04_21-15-31-3.zip");
        assert_eq!((std::fs::read(&a).unwrap(), std::fs::read(&c).unwrap()), (b"one".to_vec(), b"three".to_vec()));
        // No temporary file is left.
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left.len(), 3, "{left:?}");
        assert_eq!(numbered("a.zip", 1), "a.zip");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The pixels of `img` inside `r` shrunk by `inset` on every side: all near black (JPEG keeps a little noise).
    fn black_inside(img: &Rgba, r: Rect, inset: i32) -> bool {
        (r.y + inset..r.y + r.h - inset).all(|y| (r.x + inset..r.x + r.w - inset).all(|x| img.rgb(x, y).iter().all(|&c| c <= 40)))
    }

    #[test]
    fn exported_frames_have_black_id_corners() {
        crate::lang::set(Lang::En);
        // 16:9, 21:9 (synthetic) and observe mode's 1280x720: Tartarus board frames go in whole, every block black.
        for (w, h) in [(1920, 1080), (2560, 1080), (1280, 720)] {
            let dir = tartarus_run_sized(&format!("corners-{w}"), 2, w, h);
            let req = Request { dir: dir.clone(), game: Game::Tartarus, settled: None, install_id: String::new(), app_dir: None };
            let p = prepare(&req, &AtomicBool::new(false), &|_| {}).unwrap();
            let mut z = zip::ZipArchive::new(std::io::Cursor::new(p.zip.clone())).unwrap();
            let mut seen = 0;
            for i in 0..z.len() {
                let mut f = z.by_index(i).unwrap();
                if !f.name().unwrap().ends_with(".jpg") {
                    continue;
                }
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut f, &mut bytes).unwrap();
                let img = decode_jpeg(&bytes).unwrap();
                // Shrunk to 1280 wide at most, same aspect.
                assert_eq!(img.w, w.min(1280));
                for r in id_rects(img.w, img.h) {
                    assert!(black_inside(&img, r, 8), "{w}x{h}: {r:?}");
                    // The block reaches the frame's bottom edge.
                    assert_eq!(r.y + r.h, img.h as i32);
                }
                // Outside the blocks the noise is still there.
                assert!(img.rgb(img.w as i32 / 2, img.h as i32 / 2).iter().any(|&c| c > 40) || img.rgb(img.w as i32 / 2 + 1, img.h as i32 / 2).iter().any(|&c| c > 40));
                seen += 1;
            }
            assert_eq!(seen, 2, "{w}x{h}");
            let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
        }
    }

    #[test]
    fn whole_frames_off_the_board_never_go_in_as_they_are() {
        crate::lang::set(Lang::En);
        // A Tartarus run: one board read, then off-board frames: a miss on an unknown screen, a miss on a known
        // screen, and observe frames far from any board read. Noise has no prison top bar: none of them may go in.
        let dir = tartarus_run_sized("offboard", 1, 1280, 720);
        let mut log = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
        for (k, screen) in [(2, "Unknown"), (3, "Popup")] {
            let frame = format!("frame-{k:04}.jpg");
            std::fs::write(dir.join(&frame), noisy(1280, 720, 90 + k)).unwrap();
            log.push_str(&format!("\n{}", line(json!({"at": t(&format!("17:0{k}.000")), "kind": "miss", "screen": screen, "line": "x", "frame": frame}))));
        }
        std::fs::create_dir_all(dir.join("observe")).unwrap();
        std::fs::write(dir.join("observe").join("obs-211730000.jpg"), noisy(1280, 720, 77)).unwrap();
        log.push_str(&format!("\n{}", line(json!({"at": t("17:31.000"), "kind": "miss", "screen": "Patrol", "line": "x", "frame": null}))));
        std::fs::write(dir.join("log.jsonl"), &log).unwrap();
        let text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
        let shots = tartarus_shots(&dir, &text);
        let treat = |f: &str| shots.iter().find(|s| s.file == f).map(|s| s.treat);
        assert_eq!(treat("frame-0001.jpg"), Some(Treat::Full(Game::Tartarus)));
        assert_eq!((treat("frame-0002.jpg"), treat("frame-0003.jpg"), treat("observe/obs-211730000.jpg")), (Some(Treat::Crop(Game::Tartarus)), Some(Treat::Crop(Game::Tartarus)), Some(Treat::Crop(Game::Tartarus))));
        let mut hint = None;
        for f in ["frame-0002.jpg", "frame-0003.jpg", "observe/obs-211730000.jpg"] {
            let shot = shots.iter().find(|s| s.file == f).unwrap();
            assert_eq!(process(&dir, shot, 1280, 80, &mut hint).unwrap(), None, "{f}");
        }
        // Marked "as is" without being an already cut panel: left out too.
        let fake = Shot { ms: 0, file: "frame-0002.jpg".into(), treat: Treat::AsIs };
        assert_eq!(process(&dir, &fake, 1280, 80, &mut hint).unwrap(), None);
        assert!(already_cut("frame-battle-0009.jpg") && already_cut("learn-0002-crop.jpg") && already_cut("learn-0002-crop-1.jpg"));
        assert!(!already_cut("learn-0002.jpg") && !already_cut("frame-0002.jpg") && !already_cut("observe/obs-1.jpg"));
        let req = Request { dir: dir.clone(), game: Game::Tartarus, settled: None, install_id: String::new(), app_dir: None };
        let p = prepare(&req, &AtomicBool::new(false), &|_| {}).unwrap();
        let z = zip::ZipArchive::new(std::io::Cursor::new(p.zip.clone())).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.name_for_index(i).unwrap().unwrap().to_string()).collect();
        assert!(names.contains(&"frame-0001.jpg".to_string()), "{names:?}");
        for f in ["frame-0002.jpg", "frame-0003.jpg", "obs-211730000.jpg"] {
            assert!(!names.contains(&f.to_string()), "{f} in {names:?}");
        }
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());

        // A Vorax whole frame of no known card-play screen is cut: everything outside the board regions is black,
        // so the frame never goes in as it is.
        let bytes = noisy(2560, 1440, 5);
        let img = decode_jpeg(&bytes).unwrap();
        let cut = treat_image(img.clone(), Treat::Crop(Game::Vorax), &mut None).unwrap();
        assert!((cut.w, cut.h) != (img.w, img.h));
        // Between the tanks (to y 633) and the cards (from y 893), right of the left panel: black.
        let (x0, y0) = (VORAX_KEEP.iter().map(|r| r.x).min().unwrap(), VORAX_KEEP.iter().map(|r| r.y).min().unwrap());
        for (x, y) in [(1000, 760), (1800, 800), (700, 700)] {
            assert_eq!(cut.rgb(x - x0, y - y0), [0, 0, 0], "{x},{y}");
        }
        // And the shots of a Vorax run: 'other' screens give none, a resolving one is cut.
        let vdir = std::env::temp_dir().join(format!("ascendant-report-vshots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&vdir);
        std::fs::create_dir_all(&vdir).unwrap();
        for f in ["frame-0001.jpg", "frame-0002.jpg", "frame-0003.jpg", "learn-0004.jpg"] {
            std::fs::write(vdir.join(f), b"x").unwrap();
        }
        let vlog = [
            line(json!({"at": t("00:01.000"), "kind": "vorax_read", "frame": "frame-0001.jpg", "reading": {"kind": "potion"}})),
            line(json!({"at": t("00:02.000"), "kind": "vorax_read", "frame": "frame-0002.jpg", "reading": {"kind": "other"}})),
            line(json!({"at": t("00:03.000"), "kind": "vorax_read", "frame": "frame-0003.jpg", "reading": {"kind": "resolving"}})),
            line(json!({"at": t("00:04.000"), "kind": "vorax_unknown", "frame": "learn-0004.jpg"})),
        ]
        .join("\n");
        let v = vorax_shots(&vdir, &vlog);
        let vt = |f: &str| v.iter().find(|s| s.file == f).map(|s| s.treat);
        assert_eq!((vt("frame-0001.jpg"), vt("frame-0002.jpg"), vt("frame-0003.jpg"), vt("learn-0004.jpg")), (Some(Treat::Full(Game::Vorax)), None, Some(Treat::Crop(Game::Vorax)), Some(Treat::Crop(Game::Vorax))));
        std::fs::remove_dir_all(&vdir).unwrap();
    }

    #[test]
    fn moment_frames_look_back_per_game() {
        // A moment with no frame in its window (3 s either side at this level): a frame 60 s before is taken for
        // Tartarus (120 s back), not for Vorax (5 s either way); one 4 s before is taken by both; after the moment
        // 4 s is taken and 6 s is not.
        let level = &LEVELS[2];
        assert_eq!(level.window_s, 3);
        let shot = |ms: i64| Shot { ms, file: format!("frame-{ms}.jpg"), treat: Treat::Full(Game::Tartarus) };
        let m = Moment { ms: 100_000, at: String::new(), reason: "x".into(), priority: 1, files: Vec::new(), battle: false };
        let picked = |game: Game, ms: i64| plan(game, &[shot(ms)], &[], std::slice::from_ref(&m), level, 0).len();
        assert_eq!((picked(Game::Tartarus, 40_000), picked(Game::Vorax, 40_000)), (1, 0));
        assert_eq!((picked(Game::Tartarus, 96_000), picked(Game::Vorax, 96_000)), (1, 1));
        assert_eq!((picked(Game::Tartarus, 104_000), picked(Game::Vorax, 104_000)), (1, 1));
        assert_eq!((picked(Game::Tartarus, 106_000), picked(Game::Vorax, 106_000)), (0, 0));
    }

    #[test]
    fn learn_frames_go_by_their_own_screen() {
        let dir = std::env::temp_dir().join(format!("ascendant-report-learn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["learn-0001.jpg", "learn-0002.jpg", "learn-0003.jpg"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        // The read before says potion, but each line's own screen decides.
        let log = [
            line(json!({"at": t("00:01.000"), "kind": "vorax_read", "frame": null, "reading": {"kind": "potion"}})),
            line(json!({"at": t("00:02.000"), "kind": "vorax_unknown", "screen": "other", "frame": "learn-0001.jpg"})),
            line(json!({"at": t("00:03.000"), "kind": "vorax_tooltip", "screen": "protocol", "frame": "learn-0002.jpg"})),
            line(json!({"at": t("00:04.000"), "kind": "vorax_unknown", "frame": "learn-0003.jpg"})),
        ]
        .join("\n");
        let v = vorax_shots(&dir, &log);
        let vt = |f: &str| v.iter().find(|s| s.file == f).map(|s| s.treat);
        assert_eq!((vt("learn-0001.jpg"), vt("learn-0002.jpg"), vt("learn-0003.jpg")), (Some(Treat::Crop(Game::Vorax)), Some(Treat::Full(Game::Vorax)), Some(Treat::Crop(Game::Vorax))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vorax_card_play_frames_are_not_masked() {
        // A Vorax choice or result frame keeps its corners (no UID or chat there, and the tooltip and cards reach
        // them); a Tartarus whole frame and any frame to be cut are masked.
        let img = Rgba { w: 1280, h: 720, data: [200u8, 200, 200, 255].repeat(1280 * 720) };
        let whole = treat_image(img.clone(), Treat::Full(Game::Vorax), &mut None).unwrap();
        for r in id_rects(1280, 720) {
            assert_eq!(whole.rgb(r.x + r.w / 2, r.y + r.h / 2), [200, 200, 200]);
        }
        let tartarus = treat_image(img.clone(), Treat::Full(Game::Tartarus), &mut None).unwrap();
        for r in id_rects(1280, 720) {
            assert_eq!(tartarus.rgb(r.x + r.w / 2, r.y + r.h / 2), [0, 0, 0]);
        }
        assert!(masked(Treat::Crop(Game::Vorax)) && masked(Treat::Crop(Game::Tartarus)) && !masked(Treat::AsIs));
        // report.txt says the same, in both languages: Tartarus blacked out, Vorax pick and result screens kept.
        let text = report_text(&Prepared::default(), &LEVELS[0], &(String::new(), String::new(), String::new()));
        assert!(text.contains(tr(IMAGES_RULE)), "{text}");
        let zh = crate::lang::tr_in(crate::lang::Lang::ZhTw, IMAGES_RULE);
        assert_ne!(zh, IMAGES_RULE, "translated");
        for rule in [IMAGES_RULE, zh] {
            assert!(!rule.contains("always") && !rule.contains("一律"), "no corner is blacked out on every image: {rule}");
            assert!((rule.contains("Tartarus") || rule.contains("塔羅斯")) && (rule.contains("kept as they are") || rule.contains("保留原樣")), "{rule}");
        }
    }

    #[test]
    fn leftovers_of_a_cut_short_save_are_removed() {
        let dir = std::env::temp_dir().join(format!("ascendant-report-left-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Ascendant-report-tartarus-2026-11-04_21-15-31.zip"), b"").unwrap();
        std::fs::write(dir.join(".Ascendant-report-tartarus-2026-11-04_21-15-31.zip.partial-77"), b"half").unwrap();
        std::fs::write(dir.join("Ascendant-report-vorax-2026-11-04_21-21-09.zip"), b"PK").unwrap();
        std::fs::write(dir.join("notes.zip"), b"").unwrap();
        assert_eq!(clean_leftovers(&dir), 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["Ascendant-report-vorax-2026-11-04_21-21-09.zip", "notes.zip"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zip_takes_only_allowed_names() {
        for ok in ["log.jsonl", "report.txt", "frame-0001.jpg", "obs-211637085.jpg", "learn-0002-crop.jpg"] {
            assert!(allowed(ok), "{ok}");
        }
        for bad in ["crash.log", "settings.json", "health.txt", "x.png", ".jpg", "observe/obs-1.jpg", "..\\a.jpg", "log.jsonl.bak", "frame.jpeg"] {
            assert!(!allowed(bad), "{bad}");
        }
        let ok = zip_bytes(&[("report.txt".into(), b"r".to_vec()), ("log.jsonl".into(), b"{}".to_vec()), ("a.jpg".into(), vec![1, 2])]).unwrap();
        let z = zip::ZipArchive::new(std::io::Cursor::new(ok)).unwrap();
        assert_eq!(z.len(), 3);
        assert!(zip_bytes(&[("settings.json".into(), b"{}".to_vec())]).is_err());
    }

    #[test]
    fn save_folder_falls_back_to_app_dir() {
        let exe = Path::new(r"C:\Program Files\Ascendant\Ascendant.exe");
        let app = PathBuf::from(r"D:\Home\x\AppData\Roaming\Ascendant");
        assert_eq!(save_dir(Some(exe), Some(app.clone()), |_| true), Some(PathBuf::from(r"C:\Program Files\Ascendant")));
        assert_eq!(save_dir(Some(exe), Some(app.clone()), |d| d == app.as_path()), Some(app.clone()));
        assert_eq!(save_dir(None, Some(app.clone()), |_| true), Some(app.clone()));
        assert_eq!(save_dir(Some(exe), Some(app), |_| false), None);
        // A real folder: the temp folder can be written.
        assert!(can_write(&std::env::temp_dir()));
        assert_eq!(zip_name(Game::Vorax, "2026-11-04_21-15-31"), "Ascendant-report-vorax-2026-11-04_21-15-31.zip");
    }

    #[test]
    fn zip_names_are_ascii_in_every_language() {
        // The name does not depend on the window's language at all: no global language change needed.
        {
            for run in ["2026-11-04_21-15-31", "2026-11-04_21-15-31-2", "回報 test"] {
                for game in [Game::Tartarus, Game::Vorax] {
                    let name = zip_name(game, run);
                    assert!(name.is_ascii() && name.starts_with("Ascendant-report-") && name.ends_with(".zip"), "{name}");
                }
            }
            assert_eq!(zip_name(Game::Tartarus, "2026-11-04_21-15-31"), "Ascendant-report-tartarus-2026-11-04_21-15-31.zip");
        }
        // Names inside the zip are ASCII too.
        assert!(!allowed("回報.jpg") && allowed("frame-0001.jpg"));
    }

    #[test]
    fn observe_names_to_times() {
        let start = super::super::latency::log_millis("2026-11-04T23:59:00.000Z").unwrap();
        assert_eq!(observe_ms("obs-235930123.jpg", start), super::super::latency::log_millis("2026-11-04T23:59:30.123Z"));
        // Past midnight.
        assert_eq!(observe_ms("obs-000001000.jpg", start), super::super::latency::log_millis("2026-11-05T00:00:01.000Z"));
        assert_eq!(observe_ms("obs-12.jpg", start), None);
    }

    #[test]
    fn crops_keep_only_the_board() {
        // A 2x2 grid of blocks: top left and bottom right kept, the rest black, cut to the bounding box.
        let img = Rgba { w: 4, h: 4, data: (0..64).map(|i| i as u8 | 1).collect() };
        let out = keep_only(&img, &[Rect::new(0, 0, 2, 2), Rect::new(2, 2, 2, 2)]).unwrap();
        assert_eq!((out.w, out.h), (4, 4));
        assert_eq!(&out.data[(2 * 4) * 4..(2 * 4) * 4 + 4], &[0, 0, 0, 255]);
        assert_eq!(out.rgb(3, 3), img.rgb(3, 3));
        assert!(keep_only(&img, &[Rect::new(10, 10, 2, 2)]).is_none());
        // The Vorax regions cover the tank labels (TANK_X, STRIP_Y) and the board besides the battle panel.
        let tanks = VORAX_KEEP[1];
        assert!(crate::vorax::read::TANK_X.iter().all(|&x| x - 150 >= tanks.x && x + 150 <= tanks.x + tanks.w));
        let (y0, y1) = super::super::vorax_learn::STRIP_Y;
        assert!(y0 >= tanks.y && y1 <= tanks.y + tanks.h);
        // The id corners: see `id_masks_black_out_exactly_their_blocks`.
    }

    /// Crop comparisons, run by hand (`cargo test --profile fast-test --lib report::tests::crop_samples -- --ignored`):
    /// ASCENDANT_CROP_LIST is a file of lines "name|treatment|frame path", the treatment full-t, crop-t,
    /// full-v, crop-v, uid (the UID corner before blacking) or battle (a whole battle png cut by read::BATTLE_PANEL, as frame-battle is);
    /// writes ASCENDANT_CROP_OUT/report-crop-<name>.png: the frame on the left, the result on the right (black when left out).
    #[test]
    #[ignore]
    fn crop_samples() {
        let list = std::fs::read_to_string(std::env::var("ASCENDANT_CROP_LIST").unwrap()).unwrap();
        let out = PathBuf::from(std::env::var("ASCENDANT_CROP_OUT").unwrap());
        std::fs::create_dir_all(&out).unwrap();
        for row in list.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = row.splitn(3, '|');
            let (name, how, path) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap().trim());
            let bytes = std::fs::read(path).unwrap();
            let img = if path.ends_with(".png") { crate::reader::decode_png_raw(&bytes).unwrap() } else { decode_jpeg(&bytes).unwrap() };
            let mut hint = None;
            let done = match how {
                "full-t" | "full-v" => {
                    let mut m = img.clone();
                    mask_ids(&mut m);
                    Some(m)
                }
                "crop-t" | "crop-v" => {
                    let mut m = img.clone();
                    mask_ids(&mut m);
                    if how == "crop-t" { tartarus_crop(&m, &mut hint) } else { vorax_crop(&m) }
                }
                "battle" => Some(img.crop(super::super::vorax_learn::frame_rect(img.w, img.h, crate::vorax::read::BATTLE_PANEL))),
                // The bottom left id line before blacking, enlarged to see what it is.
                "uid" => Some(img.crop(id_rects(img.w, img.h)[2])),
                _ => panic!("{how}"),
            };
            // As the export does: 1280 wide, quality 80, then decoded again.
            let done = done.map(|d| decode_jpeg(&encode_jpeg(&shrink(d, LEVELS[0].width), LEVELS[0].quality).unwrap()).unwrap());
            let h = 720usize;
            let shown = blur_private(&img);
            let left = shown.resize(img.w * h / img.h, h);
            let right = match &done {
                Some(d) => d.resize((d.w * h / d.h).max(1), h),
                None => Rgba { w: 400, h, data: [0, 0, 0, 255].repeat(400 * h) },
            };
            let gap = 16;
            let w = left.w + gap + right.w;
            let mut data = [40u8, 40, 40, 255].repeat(w * h);
            for y in 0..h {
                data[y * w * 4..(y * w + left.w) * 4].copy_from_slice(&left.data[y * left.w * 4..(y + 1) * left.w * 4]);
                let at = (y * w + left.w + gap) * 4;
                data[at..at + right.w * 4].copy_from_slice(&right.data[y * right.w * 4..(y + 1) * right.w * 4]);
            }
            let file = out.join(format!("report-crop-{name}.png"));
            crate::reader::save_png(&file, &Rgba { w, h, data }).unwrap();
            println!("{name}: {}x{} -> {}", img.w, img.h, done.as_ref().map_or("skipped".to_string(), |d| format!("{}x{}", d.w, d.h)));
        }
    }
    /// Run by hand: a report zip from a real run (cargo test --profile fast-test --lib report::tests::export_real -- --ignored).
    /// ASCENDANT_REPORT_RUN is the run folder, ASCENDANT_REPORT_OUT the folder for the zip; the window language is zh-TW.
    #[test]
    #[ignore]
    fn export_real() {
        crate::lang::set(Lang::ZhTw);
        let dir = PathBuf::from(std::env::var("ASCENDANT_REPORT_RUN").unwrap());
        let out = PathBuf::from(std::env::var("ASCENDANT_REPORT_OUT").unwrap());
        let game = if dir.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "vorax") { Game::Vorax } else { Game::Tartarus };
        let app_dir = dir.parent().and_then(Path::parent).map(Path::to_path_buf);
        let req = Request { dir, game, settled: None, install_id: String::new(), app_dir };
        let started = std::time::Instant::now();
        let p = prepare(&req, &AtomicBool::new(false), &|s| println!("{s}")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let path = out.join(zip_name(game, &p.run));
        std::fs::write(&path, &p.zip).unwrap();
        println!("{} {} bytes, {} images ({} for the moves), level {:?}, {:?}, {:.1} s", path.display(), p.zip.len(), p.images, p.step_images, p.level, p.removed, started.elapsed().as_secs_f32());
        crate::lang::set(Lang::En);
    }
    /// A comparison's own copy of a frame with the id blocks (UID, chat box, id line) each filled with its own mean
    /// colour, so the comparison image holds no UID, character name or chat itself.
    fn blur_private(img: &Rgba) -> Rgba {
        let mut out = img.clone();
        for r in id_rects(img.w, img.h) {
            let part = img.crop(r);
            let n = (part.w * part.h).max(1) as u64;
            let mean: Vec<u8> = (0..3).map(|c| (part.data.iter().skip(c).step_by(4).map(|&v| u64::from(v)).sum::<u64>() / n) as u8).collect();
            for y in r.y.max(0)..(r.y + r.h).min(img.h as i32) {
                for x in r.x.max(0)..(r.x + r.w).min(img.w as i32) {
                    let i = (y as usize * img.w + x as usize) * 4;
                    out.data[i..i + 3].copy_from_slice(&mean);
                }
            }
        }
        out
    }

    /// The id masks against real frames, run by hand (`... report::tests::uid_samples -- --ignored`):
    /// ASCENDANT_UID_LIST is a file of lines "name|frame path"; writes ASCENDANT_UID_OUT/report-uid3-<name>.png with
    /// four panels: the frame with the id blocks blurred away (so the comparison holds no UID itself), the frame
    /// as exported, and its bottom left and bottom right corners enlarged, to show nothing is left around the blocks
    /// and the decision cards are untouched.
    #[test]
    #[ignore]
    fn uid_samples() {
        let list = std::fs::read_to_string(std::env::var("ASCENDANT_UID_LIST").unwrap()).unwrap();
        let out = PathBuf::from(std::env::var("ASCENDANT_UID_OUT").unwrap());
        std::fs::create_dir_all(&out).unwrap();
        for row in list.lines().filter(|l| !l.trim().is_empty()) {
            // "name|path" or "name|path|v": v is a Vorax card-play frame, exported unmasked (see `masked`).
            let mut parts = row.split('|');
            let (name, path, vorax) = (parts.next().unwrap(), parts.next().unwrap(), parts.next() == Some("v"));
            let bytes = std::fs::read(path.trim()).unwrap();
            let img = if path.trim().ends_with(".png") { crate::reader::decode_png_raw(&bytes).unwrap() } else { decode_jpeg(&bytes).unwrap() };
            let blurred = blur_private(&img);
            let treat = if vorax { Treat::Full(Game::Vorax) } else { Treat::Full(Game::Tartarus) };
            let masked = treat_image(img.clone(), treat, &mut None).unwrap();
            let masked = decode_jpeg(&encode_jpeg(&shrink(masked, LEVELS[0].width), LEVELS[0].quality).unwrap()).unwrap();
            let right = masked.crop(Rect::new((masked.w as f32 * 0.6) as i32, (masked.h as f32 * 0.72) as i32, masked.w as i32, masked.h as i32));
            let left = masked.crop(Rect::new(0, (masked.h as f32 * 0.72) as i32, (masked.w as f32 * 0.4) as i32, masked.h as i32));
            let h = 480usize;
            let panels: Vec<Rgba> = [blurred, masked, left, right].iter().map(|p| p.resize((p.w * h / p.h).max(1), h)).collect();
            let gap = 12;
            let w: usize = panels.iter().map(|p| p.w).sum::<usize>() + gap * (panels.len() - 1);
            let mut data = [40u8, 40, 40, 255].repeat(w * h);
            let mut x0 = 0;
            for p in &panels {
                for y in 0..h {
                    let at = (y * w + x0) * 4;
                    data[at..at + p.w * 4].copy_from_slice(&p.data[y * p.w * 4..(y + 1) * p.w * 4]);
                }
                x0 += p.w + gap;
            }
            crate::reader::save_png(&out.join(format!("report-uid3-{name}.png")), &Rgba { w, h, data }).unwrap();
            println!("{name}: {}x{} blocks {:?}", img.w, img.h, id_rects(img.w, img.h));
        }
    }
}
