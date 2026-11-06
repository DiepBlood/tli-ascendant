//! The run log: per run, a folder of JSON lines (log.jsonl) with what the
//! session read, advised and was told, and the frames read (frame-NNNN.jpg),
//! so a run can be reviewed later. A writer thread does the file work, so a
//! JPEG encode never stalls the window.

use crate::board::Model;
use crate::reader::Rgba;
use crate::reader::panels::Screen;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Finished games kept under each log root (Tartarus `runs`, Vorax `vorax`) while the "Keep all run logs" setting is
/// off: every folder from the oldest of them on stays whole, older folders are deleted. See [`prune`].
const KEEP_GAMES: usize = 10;

/// The "Keep all run logs" setting as the writer threads see it. It starts true, so nothing is deleted before the
/// app has applied the player's settings (`app::run` sets it right after loading them); the settings page sets it
/// again when the switch is turned.
static KEEP_ALL: AtomicBool = AtomicBool::new(true);

/// Sets whether every run log is kept (true) or only the newest [`KEEP_GAMES`] finished games per log root.
pub fn set_keep_all(keep: bool) {
    KEEP_ALL.store(keep, Ordering::Relaxed);
}

/// Whether every run log is kept, see [`set_keep_all`].
pub fn keep_all() -> bool {
    KEEP_ALL.load(Ordering::Relaxed)
}

/// Reads that install nothing on one screen in a row save a frame this often,
/// so a board the reader misses still shows in the log.
const MISS_FRAME_EVERY: Duration = Duration::from_secs(60);

/// JPEG quality of the saved frames: a 1920 x 1080 frame takes about 420 KB,
/// against 2.3 MB as a PNG. The reader reads the captured frame, never these.
const FRAME_QUALITY: u8 = 92;

enum Job {
    /// A write into a run's folder.
    Write(PathBuf, Item),
    Flush(Sender<()>),
    /// Later writes go into this existing folder, which is taken as made: switching to it never thins the run
    /// folders ([`RunLog::continue_in`]).
    Adopt(PathBuf),
}

enum Item {
    Line(String),
    /// File name, frame and JPEG quality.
    Jpeg(String, Rgba, u8),
    /// An observe mode frame, a JPEG already.
    Observed(String, Vec<u8>),
}

/// The log of the run on show. A folder is made with its first line, so an
/// app opened and closed without a read leaves none.
pub struct RunLog {
    root: PathBuf,
    /// The run's folder.
    dir: PathBuf,
    /// Frames saved in `dir`.
    frames: usize,
    /// The screen and board of the last frame saved, and when; no board for
    /// a read that installed none.
    last: Option<(Screen, Option<Model>, Instant)>,
    /// The fields of the last "window" line, which every later run's folder
    /// starts with: the window is reported only when it changes. Kept without
    /// the capture's geometry, which `geometry` adds afresh to every window line.
    window: Option<Value>,
    /// Adds the capture's latest geometry to a window line's fields
    /// (capture::with_geometry; a stand-in in tests).
    geometry: fn(Value) -> Value,
    tx: Option<Sender<Job>>,
    writer: Option<JoinHandle<()>>,
}

impl RunLog {
    /// A log in a folder per run under `root`, the first one starting now.
    pub fn new(root: PathBuf) -> RunLog {
        let (tx, rx) = channel::<Job>();
        let keep_root = root.clone();
        let writer = std::thread::spawn(move || {
            let mut made: Option<PathBuf> = None;
            for job in rx {
                let (dir, item) = match job {
                    Job::Write(dir, item) => (dir, item),
                    Job::Flush(done) => {
                        let _ = done.send(());
                        continue;
                    }
                    Job::Adopt(dir) => {
                        let _ = std::fs::create_dir_all(&dir);
                        made = Some(dir);
                        continue;
                    }
                };
                if made.as_ref() != Some(&dir) {
                    let _ = std::fs::create_dir_all(&dir);
                    prune(&keep_root, keep_all(), KEEP_GAMES, &dir, SystemTime::now());
                    made = Some(dir.clone());
                }
                // Errors are dropped: the log must not get in the player's way.
                let _ = match item {
                    Item::Line(line) => append(&dir.join("log.jsonl"), &line),
                    Item::Jpeg(name, frame, quality) => save_jpeg(&dir.join(name), &frame, quality),
                    Item::Observed(name, jpeg) => save_observed(&dir.join("observe"), &name, &jpeg),
                };
            }
        });
        let dir = root.join(folder_name(SystemTime::now()));
        RunLog { root, dir, frames: 0, last: None, window: None, geometry: super::capture::with_geometry, tx: Some(tx), writer: Some(writer) }
    }

    /// A log under runs in [`super::settings::app_dir`]; None when APPDATA is not set.
    pub fn in_appdata() -> Option<RunLog> {
        super::settings::app_dir().map(|d| RunLog::new(d.join("runs")))
    }

    /// The folder of the run on show.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the "window" line, and again at the top of each later run's
    /// folder.
    pub fn window(&mut self, fields: Value) {
        // The capture's latest measured geometry (numbers only) goes in as `capture`, see capture::with_geometry; it
        // is taken again for each later folder's window line, so that one carries the geometry of its own time.
        self.window = Some(fields.clone());
        self.event("window", SystemTime::now(), (self.geometry)(fields));
    }

    /// Starts a new run's folder, named from now; a second one in the same
    /// second gets a suffix. It starts with the last window line.
    pub fn new_run(&mut self) {
        let name = folder_name(SystemTime::now());
        let mut dir = self.root.join(&name);
        let mut n = 2;
        while dir == self.dir || dir.is_dir() {
            dir = self.root.join(format!("{name}-{n}"));
            n += 1;
        }
        self.dir = dir;
        self.frames = 0;
        self.last = None;
        if let Some(fields) = self.window.clone() {
            self.event("window", SystemTime::now(), (self.geometry)(fields));
        }
    }

    /// Goes on writing in the existing run folder `dir` (a paused Vorax game taken back after a restart): later lines
    /// and frames go there, numbered after the highest number its files already use (`frame-0012.jpg`,
    /// `frame-s0069.jpg`, `frame-battle-0083.jpg`, `learn-0003-crop.jpg`), so none is overwritten. Taking the
    /// folder over does not prune the run folders ([`prune`] runs only when a new folder is first written), so it
    /// stays even when it is older than the finished games kept. Observe frames are named by their time
    /// (`observe\obs-HHMMSSmmm.jpg`), not numbered, so they need nothing.
    pub fn continue_in(&mut self, dir: PathBuf) {
        self.frames = highest_number(&dir);
        self.last = None;
        self.send(Job::Adopt(dir.clone()));
        self.dir = dir;
    }

    /// Writes one line: `at`, `kind`, then `fields` (a JSON object).
    pub fn event(&self, kind: &str, at: SystemTime, fields: Value) {
        self.event_with(kind, at, fields, super::capture::take_notes);
    }

    /// [`RunLog::event`] with the capture's queued rows (capture_geometry, capture_trim, capture_warning) from
    /// `notes`: they go in, each at the time it was asked for, just before the next line that is neither a window
    /// nor a view line, into whichever log writes it. So a folder from only opening and closing the app still holds
    /// window and view lines alone, which the report's run list leaves out (report.rs `has_play`), and a new run's
    /// folder still starts with its window line.
    fn event_with(&self, kind: &str, at: SystemTime, fields: Value, notes: impl FnOnce() -> Vec<(SystemTime, &'static str, Value)>) {
        if kind != "window" && kind != "view" {
            for (asked, note, note_fields) in notes() {
                self.line(note, asked, note_fields);
            }
        }
        self.line(kind, at, fields);
    }

    /// Writes one line as [`RunLog::event`] does, without taking the capture's rows.
    fn line(&self, kind: &str, at: SystemTime, fields: Value) {
        let mut line = Map::new();
        line.insert("at".into(), Value::String(timestamp(at)));
        line.insert("kind".into(), Value::String(kind.into()));
        if let Value::Object(fields) = fields {
            line.extend(fields);
        }
        self.send(Job::Write(self.dir.clone(), Item::Line(Value::Object(line).to_string())));
    }

    /// Saves `frame` when its screen or board differs from the last frame
    /// saved, and gives the file's name. A frame without a board (a read that
    /// installed none) differs by its screen, or by [`MISS_FRAME_EVERY`]
    /// since the last frame saved.
    pub fn frame(&mut self, screen: Screen, model: Option<&Model>, frame: Rgba) -> Option<String> {
        if self.last.as_ref().is_some_and(|(s, m, at)| (*s, m.as_ref()) == (screen, model) && (model.is_some() || at.elapsed() < MISS_FRAME_EVERY)) {
            return None;
        }
        self.last = Some((screen, model.cloned(), Instant::now()));
        self.frames += 1;
        let name = frame_name(self.frames);
        self.send(Job::Write(self.dir.clone(), Item::Jpeg(name.clone(), frame, FRAME_QUALITY)));
        Some(name)
    }

    /// Saves `frame` as `<prefix>-NNNN.jpg` (quality `quality`) whether or not it equals the previous one, and
    /// returns the file name. The numbering is shared with [`RunLog::frame`]. Used by the Vorax log (it has no
    /// Tartarus `Screen` or `Model`).
    pub fn jpeg(&mut self, prefix: &str, frame: Rgba, quality: u8) -> String {
        self.frames += 1;
        let name = format!("{prefix}-{:04}.jpg", self.frames);
        self.send(Job::Write(self.dir.clone(), Item::Jpeg(name.clone(), frame, quality)));
        name
    }

    /// Takes the next frame number (shared with [`RunLog::frame`] and [`RunLog::jpeg`]): for when observe mode builds
    /// its own file names (`learn-NNNN-crop.jpg`, `frame-sNNNN.jpg`).
    pub fn number(&mut self) -> usize {
        self.frames += 1;
        self.frames
    }

    /// Saves `frame` as `name` in this run's folder (quality `quality`), without a number.
    pub fn jpeg_as(&self, name: &str, frame: Rgba, quality: u8) {
        self.send(Job::Write(self.dir.clone(), Item::Jpeg(name.to_string(), frame, quality)));
    }

    /// Saves `jpeg`, observe mode's frame of the game as captured at `at`,
    /// as observe\obs-HHMMSS-mmm.jpg (UTC), for a review of how a run looked
    /// between the frames read.
    pub fn observed(&self, jpeg: Vec<u8>, at: SystemTime) {
        let stamp = timestamp(at);
        let name = format!("obs-{}.jpg", stamp.get(11..23).unwrap_or_default().replace([':', '.'], ""));
        self.send(Job::Write(self.dir.clone(), Item::Observed(name, jpeg)));
    }

    /// Blocks until everything sent so far is written.
    pub fn wait(&self) {
        let _ = self.flushed().recv();
    }

    /// A receiver that gets a message, or is dropped, once everything sent
    /// so far is written.
    pub fn flushed(&self) -> Receiver<()> {
        let (done, rx) = channel();
        self.send(Job::Flush(done));
        rx
    }

    fn send(&self, job: Job) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(job);
        }
    }
}

impl Drop for RunLog {
    /// Finishes the writes sent so far.
    fn drop(&mut self) {
        self.tx = None;
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

fn append(path: &Path, line: &str) -> Result<(), String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| e.to_string())?;
    writeln!(file, "{line}").map_err(|e| e.to_string())
}

fn save_jpeg(path: &Path, img: &Rgba, quality: u8) -> Result<(), String> {
    let (w, h) = (u16::try_from(img.w).map_err(|e| e.to_string())?, u16::try_from(img.h).map_err(|e| e.to_string())?);
    let encoder = jpeg_encoder::Encoder::new_file(path, quality).map_err(|e| e.to_string())?;
    encoder.encode(&img.data, w, h, jpeg_encoder::ColorType::Rgba).map_err(|e| e.to_string())
}

/// An observe mode frame in `dir`.
fn save_observed(dir: &Path, name: &str, jpeg: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(name), jpeg).map_err(|e| e.to_string())
}

/// The highest file number among the JPEGs in `dir` (0 when none): a part of the name, between dashes, of at
/// least four digits, optionally after an `s` (`frame-0012`, `frame-s0069`, `frame-battle-0083`, `learn-0003-crop`).
fn highest_number(dir: &Path) -> usize {
    let number = |part: &str| {
        let digits = part.strip_prefix('s').unwrap_or(part);
        if digits.len() >= 4 && digits.bytes().all(|b| b.is_ascii_digit()) { digits.parse::<usize>().ok() } else { None }
    };
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().to_str()?.strip_suffix(".jpg").and_then(|stem| stem.split('-').filter_map(number).max()))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// "frame-0007.jpg".
fn frame_name(n: usize) -> String {
    format!("frame-{n:04}.jpg")
}

/// The UTC date and time of `t`: (year, month, day, hour, minute, second, millisecond).
fn civil(t: SystemTime) -> (i64, u32, u32, u32, u32, u32, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400) as u32);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem / 60 % 60, rem % 60, d.subsec_millis())
}

/// A run folder's name, UTC, so names sort by time: "2026-10-18_01-23-45".
pub(super) fn folder_name(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s, _) = civil(t);
    format!("{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}")
}

/// A line's time, UTC: "2026-10-18T01:23:45.678Z".
pub fn timestamp(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s, ms) = civil(t);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

/// True for a name [`folder_name`] gives, with or without a suffix.
pub(super) fn is_run_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 19
        && b[..19].iter().enumerate().all(|(k, c)| match k {
            4 | 7 => *c == b'-',
            10 => *c == b'_',
            13 | 16 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// Whether the run folder `dir` holds a finished game, from its log.jsonl: for a Vorax log root (`vorax`) a
/// `vorax_battle_end` row, or a `vorax_end` row whose `roundsSeen` holds round 13 or whose `ultimateFrom` is the
/// result screen (`vorax_end` alone is also written for a game given up or a fragment of one); for a Tartarus log
/// root a `run_end` row.
pub(super) fn finished(dir: &Path, vorax: bool) -> bool {
    let Ok(text) = std::fs::read_to_string(dir.join("log.jsonl")) else { return false };
    if !vorax {
        return text.lines().any(|l| l.contains("\"kind\":\"run_end\""));
    }
    text.lines().any(|l| {
        if l.contains("\"kind\":\"vorax_battle_end\"") {
            return true;
        }
        if !l.contains("\"kind\":\"vorax_end\"") {
            return false;
        }
        let Ok(row) = serde_json::from_str::<Value>(l) else { return false };
        // Round 13 is Vorax's last round (`LAST_ROUND` in vorax_session.rs).
        let last_round = row["roundsSeen"].as_array().is_some_and(|r| r.iter().any(|n| n.as_i64() == Some(13)));
        last_round || row["ultimateFrom"].as_str().is_some_and(|f| f.starts_with("result"))
    })
}

/// True only for a name RunLog itself gives a run folder: [`folder_name`]'s "2026-10-18_01-23-45", or that with the
/// suffix [`RunLog::new_run`] adds for a second folder in the same second ("-2", "-3": a dash and digits only).
/// Stricter than [`is_run_folder`], for what [`prune`] may delete: a folder the player renamed ("... backup",
/// "...-important") is never deleted.
fn made_run_folder(name: &str) -> bool {
    is_run_folder(name) && name.get(19..).is_some_and(|rest| rest.is_empty() || rest.strip_prefix('-').is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

/// The run folders under `root` that hold a finished game ([`finished`]), and all its run folders, both by name
/// (oldest first). Only names RunLog gives count ([`made_run_folder`]), and only up to a day after `now`: a folder
/// named later (the clock was set wrong when it was made) is neither a game nor ever deleted. Which rows mark a
/// finished game follows the root's name: `vorax` for Vorax, else Tartarus.
fn finished_games(root: &Path, now: SystemTime) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let vorax = root.file_name().is_some_and(|n| n == "vorax");
    let latest = folder_name(now + Duration::from_secs(86_400));
    let ours = |name: &str| made_run_folder(name) && name[..19] <= *latest;
    let mut runs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name().to_str().is_some_and(ours))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    runs.sort();
    let games = runs.iter().filter(|d| finished(d, vorax)).cloned().collect();
    (games, runs)
}

/// Deletes the old run folders under `root`, unless `keep_all` (the "Keep all run logs" setting, on: nothing is
/// deleted, frames included). Otherwise, once there are at least `games` finished games ([`finished_games`]), every
/// run folder older than the `games`-th newest of them is deleted whole (log, frames, observe frames); that one and
/// every newer folder stay whole. Files in `root` (Vorax's learned.jsonl, opening-rerolls.json), folders not named
/// as RunLog names them ([`made_run_folder`]) or named more than a day after `now` are never touched, nor is
/// `writing`, the folder being written. Errors are dropped: the log must not get in the player's way.
fn prune(root: &Path, keep_all: bool, games: usize, writing: &Path, now: SystemTime) {
    if keep_all || games == 0 {
        return;
    }
    let (finished, runs) = finished_games(root, now);
    let Some(oldest_kept) = finished.len().checked_sub(games).and_then(|k| finished.get(k)) else { return };
    for dir in runs.iter().filter(|d| *d < oldest_kept && d.as_path() != writing) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ascendant-runlog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The time the prune tests run at: 2026-11-06 01:23:45 UTC, after every folder they make.
    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_928_225)
    }

    #[test]
    fn names_sort_by_time() {
        let at = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(folder_name(at(0)), "1970-01-01_00-00-00");
        assert_eq!(folder_name(at(1_792_286_625)), "2026-10-18_01-23-45");
        assert_eq!(timestamp(at(1_792_286_625) + Duration::from_millis(7)), "2026-10-18T01:23:45.007Z");
        assert_eq!(folder_name(at(951_782_400)), "2000-02-29_00-00-00");
        let times = [9, 10, 59, 3599, 86_399, 86_400, 31_535_999, 31_536_000, 1_792_286_625];
        let names: Vec<String> = times.iter().map(|&s| folder_name(at(s))).collect();
        assert!(names.windows(2).all(|w| w[0] < w[1]), "{names:?}");
        assert!(names.iter().all(|n| is_run_folder(n)));
        assert!(!is_run_folder("notes") && is_run_folder("2026-10-18_01-23-45-2"));
    }

    /// Every file a run folder can hold: its log (with `rows`), read frames, Vorax learn and full frames, observe
    /// frames.
    fn fill(dir: &Path, rows: &[&str]) {
        std::fs::create_dir_all(dir.join("observe")).unwrap();
        let text: String = rows.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(dir.join("log.jsonl"), text).unwrap();
        for name in ["frame-0001.jpg", "frame-s0002.jpg", "frame-battle-0003.jpg", "learn-0004.jpg", "learn-0004-crop.jpg", "full-other-0005.jpg"] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        std::fs::write(dir.join("observe").join("obs-000000000.jpg"), b"").unwrap();
    }

    /// Every file under `root`, as paths relative to it, sorted.
    fn tree(root: &Path) -> Vec<PathBuf> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
                if e.file_type().unwrap().is_dir() {
                    walk(&e.path(), root, out);
                } else {
                    out.push(e.path().strip_prefix(root).unwrap().to_path_buf());
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    const RUN_END: &str = r#"{"at":"2026-10-30T19:09:51.885Z","kind":"run_end","vaulted":10}"#;

    /// Twelve finished Tartarus games, each between a folder before it and one after it without a game, with other
    /// folders and files in the root.
    fn twelve_games(root: &Path) -> impl Fn(usize, u32) -> PathBuf + '_ {
        let day = move |k: usize, h: u32| root.join(format!("2026-10-{:02}_{h:02}-00-00", k + 1));
        for k in 0..12 {
            fill(&day(k, 0), &[r#"{"kind":"window"}"#]);
            fill(&day(k, 1), &[r#"{"kind":"read"}"#, RUN_END]);
            fill(&day(k, 2), &[r#"{"kind":"view"}"#]);
        }
        std::fs::create_dir_all(root.join("mine").join("2026-01-01_00-00-00")).unwrap();
        std::fs::write(root.join("mine").join("keep.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.join("2026-09-01_00-00")).unwrap();
        std::fs::write(root.join("learned.jsonl"), b"{}\n").unwrap();
        std::fs::write(root.join("opening-rerolls.json"), b"{}").unwrap();
        day
    }

    /// "Keep all run logs" on: nothing at all is deleted, whatever a folder holds and however many games there are.
    #[test]
    fn keeping_all_deletes_nothing() {
        let root = temp("keep-all");
        let day = twelve_games(&root);
        let vorax = root.join("vorax");
        for k in 0..12 {
            fill(&vorax.join(format!("2026-11-{:02}_00-00-00", k + 1)), &[r#"{"kind":"vorax_battle_end"}"#]);
        }
        let before = tree(&root);
        prune(&root, true, KEEP_GAMES, &day(11, 2), now());
        prune(&vorax, true, KEEP_GAMES, &root.join("none"), now());
        prune(&root, true, 1, &root.join("none"), now());
        assert_eq!(tree(&root), before, "every file stays");
        assert!(before.contains(&PathBuf::from("2026-10-01_00-00-00").join("observe").join("obs-000000000.jpg")));
        assert!(before.contains(&PathBuf::from("2026-10-01_00-00-00").join("full-other-0005.jpg")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// "Keep all run logs" off, twelve finished games: the folders older than the tenth newest game are deleted
    /// whole; that game's folder and every newer one stay with all their files, as do the root's files and the
    /// folders not named as run folders.
    #[test]
    fn the_newest_ten_games_are_kept_whole() {
        let root = temp("keep-ten");
        let day = twelve_games(&root);
        prune(&root, false, KEEP_GAMES, &day(11, 2), now());
        for k in 0..12 {
            for h in 0..3 {
                let kept = k > 2 || (k == 2 && h >= 1);
                assert_eq!(day(k, h).is_dir(), kept, "day {k} folder {h}");
            }
        }
        let tenth = day(2, 1);
        for name in ["log.jsonl", "frame-0001.jpg", "frame-battle-0003.jpg", "learn-0004-crop.jpg", "full-other-0005.jpg"] {
            assert!(tenth.join(name).is_file(), "{name}");
        }
        assert!(tenth.join("observe").join("obs-000000000.jpg").is_file());
        assert!(root.join("learned.jsonl").is_file() && root.join("opening-rerolls.json").is_file());
        assert!(root.join("mine").join("keep.txt").is_file() && root.join("mine").join("2026-01-01_00-00-00").is_dir());
        assert!(root.join("2026-09-01_00-00").is_dir(), "not a run folder's name");
        // Pruning again changes nothing: there are ten games.
        let before = tree(&root);
        prune(&root, false, KEEP_GAMES, &day(11, 2), now());
        assert_eq!(tree(&root), before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The folder being written is never deleted, even when it is older than the games kept.
    #[test]
    fn the_folder_written_stays() {
        let root = temp("keep-writing");
        let day = twelve_games(&root);
        prune(&root, false, KEEP_GAMES, &day(0, 1), now());
        assert!(day(0, 1).is_dir() && !day(0, 0).exists() && !day(1, 2).exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Fewer than ten finished games: nothing is deleted.
    #[test]
    fn fewer_than_ten_games_delete_nothing() {
        let root = temp("keep-few");
        let day = |k: usize, h: u32| root.join(format!("2026-10-{:02}_{h:02}-00-00", k + 1));
        for k in 0..9 {
            fill(&day(k, 0), &[r#"{"kind":"window"}"#]);
            fill(&day(k, 1), &[RUN_END]);
        }
        // Many folders without a game do not count.
        for h in 0..20 {
            fill(&root.join(format!("2026-10-20_{h:02}-00-00")), &[r#"{"kind":"read"}"#]);
        }
        let before = tree(&root);
        prune(&root, false, KEEP_GAMES, &root.join("none"), now());
        assert_eq!(tree(&root), before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Vorax: a `vorax_end` row alone (a game given up, or a fragment of one) is not a finished game; one with
    /// round 13 seen, one whose final activity came from the result screen, and a `vorax_battle_end` row are.
    #[test]
    fn vorax_games_finished_by_their_rows() {
        let app = temp("keep-vorax");
        let root = app.join("vorax");
        let given_up = r#"{"at":"2026-11-03T12:20:19.675Z","held":[],"kind":"vorax_end","roundsSeen":[1],"ultimate":null}"#;
        let last_round = r#"{"at":"2026-11-03T12:36:50.982Z","held":[[9,5]],"kind":"vorax_end","roundsSeen":[1,2,3,4,5,6,7,8,9,10,11,12,13],"ultimate":null}"#;
        let result = r#"{"at":"2026-11-03T12:36:50.982Z","held":[],"kind":"vorax_end","roundsSeen":[11,12],"ultimate":900000,"ultimateFrom":"result"}"#;
        let from_board = r#"{"at":"2026-11-03T12:36:50.982Z","held":[],"kind":"vorax_end","roundsSeen":[11,12],"ultimate":900000,"ultimateFrom":"board"}"#;
        let battle = r#"{"activity":914900.0,"at":"2026-11-05T13:28:54.409Z","kind":"vorax_battle_end"}"#;
        let cases = [(given_up, false), (last_round, true), (result, true), (from_board, false), (battle, true), (RUN_END, false)];
        for (k, (row, game)) in cases.iter().enumerate() {
            let dir = root.join(format!("2026-11-01_00-00-{k:02}"));
            fill(&dir, &[r#"{"kind":"window"}"#, row]);
            assert_eq!(finished(&dir, true), *game, "{row}");
        }
        assert!(finished(&root.join("2026-11-01_00-00-05"), false), "run_end is a Tartarus game");
        assert!(!finished(&root.join("2026-11-01_00-00-04"), false));
        assert_eq!(finished_games(&root, now()).0.len(), 3, "the root's name picks the Vorax rows");
        // Eleven given-up fragments then ten games: only the fragments before the tenth newest game go.
        for k in 0..11 {
            fill(&root.join(format!("2026-11-02_00-00-{k:02}")), &[given_up]);
        }
        for k in 0..7 {
            fill(&root.join(format!("2026-11-03_00-00-{k:02}")), &[battle]);
        }
        let (games, _) = finished_games(&root, now());
        assert_eq!(games.len(), 10);
        prune(&root, false, KEEP_GAMES, &root.join("none"), now());
        assert!(!root.join("2026-11-01_00-00-00").exists(), "a fragment older than the tenth game");
        assert!(root.join("2026-11-01_00-00-01").is_dir(), "the tenth newest game");
        assert!(root.join("2026-11-01_00-00-03").is_dir() && root.join("2026-11-02_00-00-10").is_dir());
        std::fs::remove_dir_all(&app).unwrap();
    }

    /// Only names RunLog gives (with its "-2" suffix for a second folder in a second) are games or deleted: folders
    /// the player renamed stay, even an old one holding a finished game, and do not count as one.
    #[test]
    fn renamed_folders_are_never_deleted() {
        assert!(made_run_folder("2026-10-18_01-23-45") && made_run_folder("2026-10-18_01-23-45-2") && made_run_folder("2026-10-18_01-23-45-13"));
        for name in ["2026-10-18_01-23-45 備份", "2026-10-18_01-23-45-重要", "2026-10-18_01-23-45-", "2026-10-18_01-23-45-2a", "2026-10-18_01-23-45_2", "2026-10-18_01-23"] {
            assert!(!made_run_folder(name), "{name}");
        }
        assert!(is_run_folder("2026-10-18_01-23-45 備份"), "the report's run list is unchanged");
        let root = temp("keep-renamed");
        let day = twelve_games(&root);
        let renamed = ["2026-09-01_00-00-00 備份", "2026-09-01_00-00-00-重要", "2026-11-04_00-00-00 備份"];
        for name in renamed {
            fill(&root.join(name), &[RUN_END]);
        }
        let second = root.join("2026-10-01_00-00-00-2");
        fill(&second, &[RUN_END]);
        // The renamed games, one of them newer than all, do not count: the tenth newest game is still day 3's.
        assert_eq!(finished_games(&root, now()).0.len(), 13);
        prune(&root, false, KEEP_GAMES, &root.join("none"), now());
        for name in renamed {
            assert!(root.join(name).join("log.jsonl").is_file(), "{name}");
        }
        assert!(!second.exists() && !day(2, 0).exists() && day(2, 1).is_dir(), "a suffixed folder is RunLog's own");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A folder named more than a day after now (the clock was wrong when it was made) is not a game and is never
    /// deleted; one within the day is RunLog's own.
    #[test]
    fn folders_from_the_future_are_never_deleted() {
        let root = temp("keep-future");
        let day = twelve_games(&root);
        let future = root.join("2027-01-01_00-00-00");
        let tomorrow = root.join("2026-11-07_00-00-00");
        fill(&future, &[RUN_END]);
        for k in 1..=10 {
            fill(&root.join(format!("2027-01-01_00-00-{k:02}")), &[RUN_END]);
        }
        fill(&tomorrow, &[RUN_END]);
        let (games, _) = finished_games(&root, now());
        assert_eq!(games.len(), 13, "the twelve and tomorrow's");
        prune(&root, false, KEEP_GAMES, &root.join("none"), now());
        assert!(future.is_dir() && root.join("2027-01-01_00-00-10").is_dir(), "never deleted");
        assert!(tomorrow.is_dir() && day(3, 1).is_dir() && !day(3, 0).exists(), "tomorrow's game is the newest kept");
        // Run a year later, the same folders are past: they count, and only the newest ten games stay.
        let later = now() + Duration::from_secs(365 * 86_400);
        prune(&root, false, KEEP_GAMES, &root.join("none"), later);
        assert!(!day(11, 1).exists() && !tomorrow.exists() && !future.exists() && root.join("2027-01-01_00-00-01").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Nothing is deleted before the app has applied the settings.
    #[test]
    fn keeping_all_until_the_settings_say_otherwise() {
        assert!(KEEP_ALL.load(Ordering::Relaxed), "starts true");
    }

    #[test]
    fn frames_are_numbered_once_per_board() {
        let root = temp("frames");
        let mut log = RunLog::new(root.clone());
        let pixel = || Rgba { w: 1, h: 1, data: vec![0, 0, 0, 255] };
        let board = Model::blank();
        let mut other = board.clone();
        other.day += 1;
        assert_eq!(log.frame(Screen::Patrol, Some(&board), pixel()).as_deref(), Some("frame-0001.jpg"));
        assert_eq!(log.frame(Screen::Patrol, Some(&board), pixel()), None);
        assert_eq!(log.frame(Screen::Manage, Some(&board), pixel()).as_deref(), Some("frame-0002.jpg"));
        assert_eq!(log.frame(Screen::Manage, Some(&other), pixel()).as_deref(), Some("frame-0003.jpg"));
        // A frame without a board is saved once per screen.
        assert_eq!(log.frame(Screen::Unknown, None, pixel()).as_deref(), Some("frame-0004.jpg"));
        assert_eq!(log.frame(Screen::Unknown, None, pixel()), None);
        assert_eq!(log.frame(Screen::Popup, None, pixel()).as_deref(), Some("frame-0005.jpg"));
        log.event("read", SystemTime::now(), serde_json::json!({ "screen": "Patrol" }));
        log.wait();
        let first = log.dir().to_path_buf();
        assert!(first.join("frame-0005.jpg").is_file() && first.join("log.jsonl").is_file());

        // A new run numbers from 1 again, in another folder, which starts with
        // the window line again.
        log.window(serde_json::json!({ "window": "w", "ocr": "en-US" }));
        log.new_run();
        assert_ne!(log.dir(), first);
        assert_eq!(log.frame(Screen::Manage, Some(&other), pixel()).as_deref(), Some("frame-0001.jpg"));
        log.wait();
        let text = std::fs::read_to_string(log.dir().join("log.jsonl")).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!((&line["kind"], &line["window"], &line["ocr"]), (&"window".into(), &"w".into(), &"en-US".into()));
        drop(log);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn jpegs_are_numbered_with_the_frames() {
        let root = temp("jpeg");
        let mut log = RunLog::new(root.clone());
        let pixel = || Rgba { w: 1, h: 1, data: vec![0, 0, 0, 255] };
        assert_eq!(log.frame(Screen::Unknown, None, pixel()).as_deref(), Some("frame-0001.jpg"));
        assert_eq!(log.jpeg("frame", pixel(), 80), "frame-0002.jpg");
        assert_eq!(log.jpeg("learn", pixel(), 98), "learn-0003.jpg");
        log.wait();
        assert!(log.dir().join("frame-0002.jpg").is_file() && log.dir().join("learn-0003.jpg").is_file());
        drop(log);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Going on in an older run folder: it stays although it is past the finished games kept (taking it over does
    /// not prune), and later files are numbered after the highest number it already uses, whatever the prefix.
    #[test]
    fn a_run_folder_taken_back_keeps_its_frames_and_numbers() {
        const FRAMES: usize = 20;
        let root = temp("continue");
        let back = root.join("2026-10-01_00-00-00");
        std::fs::create_dir_all(back.join("observe")).unwrap();
        std::fs::write(back.join("log.jsonl"), b"{}\n").unwrap();
        for f in 1..=FRAMES {
            std::fs::write(back.join(frame_name(f)), b"").unwrap();
        }
        for name in ["frame-s0069.jpg", "frame-battle-0083.jpg", "learn-0071-crop.jpg"] {
            std::fs::write(back.join(name), b"").unwrap();
        }
        std::fs::write(back.join("observe").join("obs-111655387.jpg"), b"").unwrap();
        // More finished games after it than are kept: a prune with "Keep all run logs" off would take it.
        for k in 0..KEEP_GAMES + 2 {
            fill(&root.join(format!("2026-10-02_{k:02}-00-00")), &[RUN_END]);
        }
        assert_eq!(highest_number(&back), 83);
        let mut log = RunLog::new(root.clone());
        log.continue_in(back.clone());
        assert_eq!(log.dir(), back.as_path());
        let pixel = || Rgba { w: 1, h: 1, data: vec![0, 0, 0, 255] };
        assert_eq!(log.jpeg("frame", pixel(), 80), "frame-0084.jpg");
        assert_eq!(log.number(), 85);
        log.event("vorax_resume", SystemTime::now(), serde_json::json!({}));
        log.wait();
        let frames = std::fs::read_dir(&back).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("frame-")).count();
        assert_eq!(frames, FRAMES + 3, "its frames, and the new one");
        assert!(back.join("observe").join("obs-111655387.jpg").is_file());
        assert_eq!(std::fs::read_to_string(back.join("log.jsonl")).unwrap().lines().count(), 2, "appended");
        // The same prune on a new folder's first write, with "Keep all run logs" off, would have deleted it: the
        // setup is past the limit.
        drop(log);
        prune(&root, false, KEEP_GAMES, &root.join("none"), now());
        assert!(!back.exists(), "a prune would take it");
        assert_eq!(highest_number(&root.join("none")), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn observed_frames_go_into_their_folder() {
        let root = temp("observe");
        let log = RunLog::new(root.clone());
        // 2026-10-30 11:16:55.387 UTC.
        let at = UNIX_EPOCH + Duration::from_millis(1_793_359_015_387);
        log.observed(vec![1, 2, 3], at);
        log.wait();
        let file = log.dir().join("observe").join("obs-111655387.jpg");
        assert_eq!(std::fs::read(&file).unwrap(), vec![1, 2, 3], "{}", file.display());
        drop(log);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The geometry the stand-in gives window lines, for `each_folder_starts_with_the_latest_geometry` only.
    static GEOMETRY: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

    fn stand_in_geometry(mut fields: Value) -> Value {
        if let (Value::Object(map), Some(geometry)) = (&mut fields, GEOMETRY.lock().unwrap().clone()) {
            map.insert("capture".into(), geometry);
        }
        fields
    }

    /// A later run's folder starts with a window line carrying the capture's geometry as it is then, not as it was
    /// when the window was first told of (on 2026-11-06 two folders began with a stale texture from the moment the
    /// game changed monitors).
    #[test]
    fn each_folder_starts_with_the_latest_geometry() {
        let root = temp("latest-geometry");
        let mut log = RunLog::new(root.clone());
        log.geometry = stand_in_geometry;
        *GEOMETRY.lock().unwrap() = Some(serde_json::json!({ "texture": [2560, 1440], "origin": "unmatched" }));
        log.window(serde_json::json!({ "window": "w", "ocr": "en-US" }));
        log.wait();
        let first_dir = log.dir().to_path_buf();
        *GEOMETRY.lock().unwrap() = Some(serde_json::json!({ "texture": [1922, 1112], "origin": "window" }));
        log.new_run();
        log.wait();
        let first_line = |dir: &Path| -> Value {
            let text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
            serde_json::from_str(text.lines().next().unwrap()).unwrap()
        };
        let (old, new) = (first_line(&first_dir), first_line(log.dir()));
        assert_eq!((&old["kind"], &old["capture"]["texture"]), (&"window".into(), &serde_json::json!([2560, 1440])));
        assert_eq!((&new["kind"], &new["window"]), (&"window".into(), &"w".into()));
        assert_eq!(new["capture"], serde_json::json!({ "texture": [1922, 1112], "origin": "window" }), "the geometry as it is now");
        drop(log);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The report's run list holds finished games only ([`finished`]): a folder from opening the app, one played
    /// but not finished (Tartarus without `run_end`, Vorax with only a given-up `vorax_end`) is left out; a
    /// Tartarus run with `run_end`, a Vorax game with `vorax_battle_end` or with round 13 seen is listed, newest
    /// first, each with its game.
    #[test]
    fn report_lists_finished_games_only() {
        use super::super::report::{Game, list_runs};
        let app = temp("report-list");
        let played = r#"{"at":"2026-11-01T00:00:01.000Z","kind":"read","screen":"Manage"}"#;
        let given_up = r#"{"at":"2026-11-03T12:20:19.675Z","held":[],"kind":"vorax_end","roundsSeen":[1],"ultimate":null}"#;
        let last_round = r#"{"at":"2026-11-03T12:36:50.982Z","held":[[9,5]],"kind":"vorax_end","roundsSeen":[1,2,3,4,5,6,7,8,9,10,11,12,13],"ultimate":null}"#;
        let battle = r#"{"activity":914900.0,"at":"2026-11-05T13:28:54.409Z","kind":"vorax_battle_end"}"#;
        let window = r#"{"kind":"window"}"#;
        // Played but not finished: nothing listed.
        fill(&app.join("runs").join("2026-11-01_00-00-00"), &[window]);
        fill(&app.join("runs").join("2026-11-01_00-00-01"), &[window, played]);
        fill(&app.join("vorax").join("2026-11-01_00-00-02"), &[window, given_up]);
        assert!(list_runs(&app).is_empty(), "played but not finished: not listed");
        // Finished: listed, newest first.
        fill(&app.join("runs").join("2026-11-01_00-00-03"), &[window, played, RUN_END]);
        fill(&app.join("vorax").join("2026-11-01_00-00-04"), &[window, battle]);
        fill(&app.join("vorax").join("2026-11-01_00-00-05"), &[window, last_round]);
        let listed: Vec<(String, Game)> = list_runs(&app).into_iter().map(|r| (r.name, r.game)).collect();
        assert_eq!(
            listed,
            [("2026-11-01_00-00-05".to_string(), Game::Vorax), ("2026-11-01_00-00-04".to_string(), Game::Vorax), ("2026-11-01_00-00-03".to_string(), Game::Tartarus)]
        );

        // The folders found finished are kept in memory: the next listing does not read their logs again (a
        // finished game never changes), only the others'.
        let known = std::sync::Mutex::new(std::collections::HashSet::new());
        assert_eq!(super::super::report::list_runs_known(&app, &known).len(), 3);
        assert_eq!(known.lock().unwrap().len(), 3, "only the finished folders are remembered");
        std::fs::write(app.join("runs").join("2026-11-01_00-00-03").join("log.jsonl"), "").unwrap();
        fill(&app.join("runs").join("2026-11-01_00-00-01"), &[window, played, RUN_END]);
        let names: Vec<String> = super::super::report::list_runs_known(&app, &known).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["2026-11-01_00-00-05", "2026-11-01_00-00-04", "2026-11-01_00-00-03", "2026-11-01_00-00-01"], "a known one not read again; one finished since read");
        assert_eq!(list_runs(&app).len(), 3, "without the memory the emptied log is read: not finished");
        std::fs::remove_dir_all(&app).unwrap();
    }

    /// The capture's queued rows wait for a line that is neither a window nor a view line: a folder from opening and
    /// closing the app (window and view lines only) is still left out of the report's run list, and once a played
    /// line comes they go in before it, at the time they were asked for. The rows come from a stand-in queue here,
    /// not the capture's shared one, so tests running alongside cannot take them.
    #[test]
    fn capture_rows_wait_for_a_played_line() {
        let app = temp("capture-rows");
        let mut log = RunLog::new(app.join("runs"));
        let asked = UNIX_EPOCH + Duration::from_millis(1_793_359_015_000);
        let queued = || vec![(asked, "capture_geometry", serde_json::json!({ "texture": [1936, 1119] }))];
        let fields = serde_json::json!({ "window": "\"T\" (1920 x 1080)", "ocr": "en-US" });
        log.event_with("window", SystemTime::now(), fields.clone(), || panic!("not taken for a window line"));
        log.event_with("view", SystemTime::now(), serde_json::json!({ "advice": null }), || panic!("not taken for a view line"));
        log.window = Some(fields);
        log.wait();
        assert!(super::super::report::list_runs(&app).is_empty(), "opened and closed: no run listed");

        let at = asked + Duration::from_millis(500);
        log.event_with("read", at, serde_json::json!({ "screen": "Manage" }), queued);
        log.wait();
        let text = std::fs::read_to_string(log.dir().join("log.jsonl")).unwrap();
        let kinds: Vec<String> = text.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()["kind"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["window", "view", "capture_geometry", "read"]);
        let row: Value = serde_json::from_str(text.lines().nth(2).unwrap()).unwrap();
        assert_eq!(row["at"], timestamp(asked), "the time it was asked for");
        assert!(super::super::report::list_runs(&app).is_empty(), "played but not finished: not listed");

        // A new run's folder still starts with the window line.
        log.new_run();
        log.wait();
        let first = std::fs::read_to_string(log.dir().join("log.jsonl")).unwrap();
        assert!(first.lines().next().unwrap().contains("\"kind\":\"window\""));
        drop(log);
        std::fs::remove_dir_all(&app).unwrap();
    }
}
