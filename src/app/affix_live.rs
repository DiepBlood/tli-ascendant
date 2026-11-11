//! The affix frames live on the game (private builds from 2026-11-08, public since 1.2.0). While a map page
//! shows, map page frames the capture takes (`Event::MapShot`: when the page shows, after each Add or Reset, at least
//! once a second, and the ones asked for here) are read on a worker thread ([`crate::affix::read`], from the live
//! lossless frame), and the danger and warning entries are framed ([`AffixOverlay`]).
//!
//! When to read (the live test of 2026-11-08: the panel was not on screen 0.6 s after the page's log line, the map
//! still moving, and came in by about 1.5 s; frames once a second then left it unread for up to a second more):
//! - after the page shows, and after a click, a frame is asked for every [`QUICK_EVERY`] for at most [`QUICK_FOR`],
//!   and the panel is read once it is on screen (its text area dark, [`panel::on_screen`]: the bright map is never
//!   read) and at rest (two frames alike); then frames come once a second again;
//! - Add and Reset: the frames go at once (frames left up would mark other affixes as entries move; framing the wrong
//!   affix is worse than framing none). The button held down repeats every 14 to 42 ms in the log: the panel is
//!   looked at once no click came for [`RELEASED_AFTER`] (clicks until then, held or not, are one lot). When it is then
//!   at rest and the same as before the clicks (a button out of uses, a click that changed nothing), the frames from
//!   before go back up without reading; a single Add's panel may not be drawn yet, so one still the same is waited on
//!   until [`ADD_WAIT`] after the first click; after a Reset the old panel shows a while (460 ms, the new one by
//!   820 ms), so a panel still the same as before the clicks, or as at the last Reset, is waited on for up to
//!   [`RESET_WAIT`] from that Reset; otherwise the panel at rest is read, and after Adds the frames from before go
//!   back up at once moved down past the new entries at the top ([`moved_down`], rows from the bullets) until that
//!   read is done. An Add on a full panel ([`MAX_AFFIXES`]) changes nothing: the frames stay. Quick frames that end
//!   with nothing read are noted too, with how dark they came;
//! - outside those, a panel whose print changed is read, once more when it comes to rest after a read of a frame still
//!   moving, and a read that may read better (no entry, an entry ambiguous, short or a few edits off a sentence:
//!   [`worth_reading_again`]) is read again after 250 ms, 500 ms, then 1 s each, at most [`RETRY_READS`] times for the
//!   panel (as Vorax's choice screens since 1.1.0), counted across reads of the same entries (the half-clear panel's
//!   background moves its print).
//!
//! The newest read replaces the frames, but a read of the same entries as the last ([`alike`]) keeps what that one
//! found ([`keep_found`]): an entry at the fuzzy limit came and went as the background moved (2026-11-08 evening).
//! Frames up go when [`GONE_AFTER`] frames in a row do not show the panel. The visit remembers the entries it read whole, so the last entry cut by
//! the panel's bottom takes the grade of the entry it was when it showed whole (new affixes come in at the top: 100
//! additions checked in the lab), but only from an entry not shown whole on the same panel (two affixes of different
//! grades can start alike). The frames go as soon as the page is hidden or closed, the game is not in front, or no
//! frame came for [`STALE`] (the capture stops taking them when the log has said nothing of the page for 10 minutes,
//! so frames left up then would point at the wrong lines after a click). What happened is noted for the samples' log
//! ([`AffixLive::take_notes`]: each read's times and counts, frames put back, frames taken down).

use super::affix_overlay::AffixOverlay;
use super::gamelog::{MapAffixes, MapPage, PageState, Signal};
use crate::affix::dict::{key, levenshtein};
use crate::affix::frames::{self, FrameStyle};
use crate::affix::panel::Placement;
use crate::affix::{Dict, Grade, Grades, Learned, Lookup, Panel, panel};
use crate::reader::{Rect, Rgba, ocr};
use std::sync::{Arc, Mutex, PoisonError};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

/// A cut entry needs this many key characters to take a remembered entry's grade (as the table's start matching).
const MIN_CUT: usize = 20;
/// Frames are taken at least once a second while a page shows: none for this long and the frames go.
const STALE: Duration = Duration::from_secs(3);
/// A page shown again after this long without frames is a new visit (its closing line may never have come).
const NEW_VISIT_AFTER: Duration = Duration::from_secs(60);
/// How many times a panel read incomplete is read again while it stays the same (Vorax's RETRY_READS).
const RETRY_READS: u32 = 6;
/// After the page shows or a click, a frame this often until the panel at rest is read (see the module notes).
const QUICK_EVERY: Duration = Duration::from_millis(120);
/// The quick frames stop this long after they started, the panel read or not.
const QUICK_FOR: Duration = Duration::from_secs(3);
/// No click for this long after the last one: the button was let go (held, it repeats every 14 to 42 ms).
const RELEASED_AFTER: Duration = Duration::from_millis(100);
/// After a Reset, how long a panel still the same as before (or as at the Reset) is waited on, from the last Reset,
/// before it counts as unchanged.
const RESET_WAIT: Duration = Duration::from_millis(1500);
/// After an Add, how long from the first click a panel still the same as before is waited on before it counts as
/// unchanged: the game had drawn the added affix 360 ms after a click (the live test of 2026-11-08), not known sooner.
/// Held down, the button is let go well after this, so only a single click waits.
const ADD_WAIT: Duration = Duration::from_millis(400);
/// The most affixes a map takes: the Add button is greyed with 0 uses left at six (every panel of the live tests of
/// 2026-11-08, sample-0004 and 0005 of 18-03-54 and the user's screenshot 1.webp). An Add click then changes nothing.
const MAX_AFFIXES: usize = 6;
/// Frames in a row that do not show the panel (bright, [`panel::on_screen`]) before the frames go: the page's log
/// line says nothing when a popup takes the panel's place (the 哀思-守夜 popup of 2026-11-08, 16 s with frames up).
const GONE_AFTER: u32 = 2;
/// An entry not found is read again only when it may read better: its key shorter than this (a line read badly), or
/// within this many edits beyond the fuzzy limit of a sentence. An affix the table does not have reads the same every
/// time (five reads of each panel alike, 2026-11-09): reading it again only costs time.
const SHORT_KEY: usize = 6;
const NEAR_MISS: usize = 2;
/// Two reads are of the same panel when they have as many entries and each entry's key is within this share of its
/// length (at least [`ALIKE_MIN`] edits) of the other read's at the same place, its top within [`ALIKE_TOP`] frame
/// pixels per 1440 rows: the background behind the half-clear panel moves, and a letter or two of a line read
/// differently then made an entry come and go (5212008 on 18-02-24, one edit inside the fuzzy limit).
const ALIKE_SHARE: f64 = 0.25;
const ALIKE_MIN: usize = 3;
const ALIKE_TOP: f64 = 8.0;

/// The wait before the `n`th read again (from 1): 250 ms, 500 ms, then 1 s.
fn retry_wait(n: u32) -> Duration {
    Duration::from_millis(250 << n.saturating_sub(1).min(2))
}

/// Whether a read is worth reading again: no entry, a whole entry ambiguous, or a whole entry not found that may read
/// better (see [`SHORT_KEY`]). An entry cut by the panel's bottom stays cut whatever the frame, so it does not count.
fn worth_reading_again(panel: &Panel, dict: &Dict) -> bool {
    panel.entries.is_empty()
        || panel.entries.iter().any(|e| {
            !e.cut
                && match e.lookup {
                    Lookup::Found { .. } => false,
                    Lookup::Ambiguous => true,
                    Lookup::Unknown => {
                        let m = dict.margin(&e.text);
                        m.chars < SHORT_KEY || m.best.is_none_or(|b| b <= m.allowed + NEAR_MISS)
                    }
                }
        })
}

/// Whether two reads are of the same panel (see [`ALIKE_SHARE`]); two reads of no entry are.
fn alike(a: &Panel, b: &Panel, frame_h: usize) -> bool {
    let top = ALIKE_TOP * frame_h as f64 / 1440.0;
    a.entries.len() == b.entries.len()
        && a.entries.iter().zip(&b.entries).all(|(x, y)| {
            let (kx, ky) = (key(&x.text), key(&y.text));
            let n = kx.chars().count().max(ky.chars().count());
            let limit = ALIKE_MIN.max((n as f64 * ALIKE_SHARE).round() as usize);
            f64::from((x.top - y.top).abs()) <= top && levenshtein(&kx, &ky) <= limit
        })
}

/// A read of the same panel as `before` (see [`alike`]) keeps an entry `before` found when this read did not find it
/// but its text is still near that match (within [`NEAR_MISS`] edits beyond the fuzzy limit of the sentence's text):
/// an entry at the fuzzy limit no longer comes and goes as the background moves, and a match a read got wrong is not
/// kept against a later read that disagrees with it.
fn keep_found(panel: &mut Panel, before: &Panel, dict: &Dict) {
    for (e, was) in panel.entries.iter_mut().zip(&before.entries) {
        let Lookup::Found { index, .. } = was.lookup else { continue };
        if matches!(e.lookup, Lookup::Found { .. }) {
            continue;
        }
        let Some(s) = dict.sentences().get(index) else { continue };
        let k = key(&e.text);
        let near = [&s.en, &s.zh].iter().filter(|t| !t.is_empty()).map(|t| levenshtein(&k, &key(t))).min();
        if near.is_some_and(|d| d <= dict.margin(&e.text).allowed + NEAR_MISS) {
            e.lookup = was.lookup;
        }
    }
}

/// After an Add on a full panel, its frames are watched this long (a click's panel drawn by 360 ms): more bullets
/// than entries read take the frames down and read the panel.
const FULL_ADD_WATCH: Duration = Duration::from_millis(1000);
/// A panel whose print changes while its entries stand still is read again at most this often (see `frame`): the
/// background behind it moves; a map's other panel with entries at the very same rows still gets read within this.
const LAYOUT_RECHECK: Duration = Duration::from_secs(3);

/// Entry heights may differ this much (reference pixels, a third of a text line) for the bullets of a panel to be the
/// panel before an Add moved down ([`moved_down`]).
const HEIGHT_SLACK: f64 = 9.0;

/// The frames of the panel `before` an Add, moved to where its entries are now: new affixes come in at the top
/// (100 additions checked in the lab), so with `tops` (each entry's top on the new panel, from its bullets) showing
/// `k` more entries, the old entry `j` is now entry `j + k`; the new entries at the top are not framed until read.
/// None when the panel did not grow, has nothing framed, or an old entry's height on the new panel is not its height
/// before (a bullet found too many or too few would move every frame by one entry).
fn moved_down(before: &Panel, tops: &[i32], frame_h: usize, place: Option<Placement>) -> Option<Vec<(Rect, Grade)>> {
    let k = tops.len().checked_sub(before.entries.len()).filter(|&k| k > 0)?;
    let slack = HEIGHT_SLACK * frame_h as f64 / 1440.0;
    let heights_kept = before.entries.iter().enumerate().all(|(j, e)| {
        tops.get(j + k + 1).is_none_or(|next| f64::from((next - tops[j + k] - (e.bottom - e.top)).abs()) <= slack)
    });
    if !heights_kept {
        return None;
    }
    let entries = before
        .entries
        .iter()
        .enumerate()
        .map(|(j, e)| {
            let top = tops[j + k];
            let bottom = tops.get(j + k + 1).copied().unwrap_or(top + (e.bottom - e.top));
            crate::affix::Entry { top, bottom, ..e.clone() }
        })
        .collect();
    let boxes = frames::boxes_at(&Panel { entries, title: before.title }, frame_h, place);
    (!boxes.is_empty()).then_some(boxes)
}

/// The file of the user's grade changes, in the app's `affix` folder (beside the private builds' samples): written on
/// each change, samples taken or not.
pub const GRADES_FILE: &str = "grades.jsonl";

/// The fields of a grade change: the sentence's id, its grade before and after (null: the default).
pub fn grade_row(id: &str, from: Option<Grade>, to: Option<Grade>) -> serde_json::Value {
    let name = |g: Option<Grade>| g.map(|g| format!("{g:?}"));
    serde_json::json!({ "id": id, "from": name(from), "to": name(to) })
}

/// Appends a grade change to `file` (a JSON line: its time, `affix_grade`, the id, the grade before and after); errors
/// dropped, nothing without a file.
fn append_grade(file: Option<&std::path::Path>, id: &str, from: Option<Grade>, to: Option<Grade>) {
    use std::io::Write;
    let Some(file) = file else { return };
    let mut row = grade_row(id, from, to);
    row["at"] = serde_json::json!(super::runlog::timestamp(SystemTime::now()));
    row["kind"] = serde_json::json!("affix_grade");
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(f, "{row}");
    }
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// One entry on the map page now, top to bottom (the affix page lists them first, unknown ones too).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Found: the sentence's index in the table.
    Known(usize),
    /// Not found: the text as read; `ambiguous` when it reads like sentences of different grades (not framed), `cut`
    /// when the panel's bottom cuts it; `game`, the grade its game colour frames it with (none for an ambiguous one).
    Unknown { text: String, ambiguous: bool, cut: bool, game: Grade },
    /// Named by the game log on entering the map, not in the table (no words for it yet): its id.
    Logged(u32),
}

impl Seen {
    /// The table's sentence of game id `id`, else the id alone.
    fn of_id(dict: &Dict, id: u32) -> Seen {
        let id_text = id.to_string();
        dict.sentences().iter().position(|s| s.ids.contains(&id_text)).map_or(Seen::Logged(id), Seen::Known)
    }

    fn of(panel: &Panel, e: &crate::affix::Entry) -> Seen {
        match e.lookup {
            Lookup::Found { index, .. } => Seen::Known(index),
            other => Seen::Unknown {
                text: e.text.clone(),
                ambiguous: other == Lookup::Ambiguous,
                cut: e.cut,
                game: if frames::by_game_colour(panel, e) { frames::game_grade(e.colour) } else { Grade::None },
            },
        }
    }
}

/// The affixes shown on the affix page with no map page on show: the last read of the page closed, then from the map
/// entered next as the game log names them (the user, 2026-11-10: the map's affixes on show in the map, and the log's
/// when the page closed before it was read). Until a page shows or the hideout is entered.
#[derive(Clone, Debug)]
struct MapShown {
    seen: Vec<Seen>,
    /// The game log's ids of the map entered (None until it is entered).
    log: Option<Vec<u32>>,
    /// The list is the log's, not the read's (they differed, or nothing was read).
    from_log: bool,
    /// When the page closed, or the map was entered.
    at: SystemTime,
    closed: Instant,
}

/// A change of the interface scale measured larger than this is a change of the slider ([`AffixLive::scale_seen`]).
const SCALE_CHANGE: f64 = 0.01;
/// The game log's interface scale and the one measured may differ by this much (the log's 0.668 for the user's 0.667
/// is 0.0015; one pixel of the title bar's width 0.002).
const SCALE_DISAGREE: f64 = 0.02;

/// How long after the page closed the level entered is taken as its map: the game entered it 0.5 to 1.7 s after the
/// page closed in 78 of 87 visits of the logs of 2026-11-07; the others were cancelled.
pub const LEVEL_WITHIN: Duration = Duration::from_secs(15);

/// How long a map entered stays "this map" on the affix page with no hideout heard: a map rarely takes this long, so
/// past it the hideout was likely missed and the list is named by its time.
const MAP_FOR: Duration = Duration::from_secs(3600);

impl MapShown {
    /// Whose list the affix page shows: this map (entered, or the page just closed), else an earlier one with its time.
    fn title(&self) -> super::affix_page::MapTitle {
        use super::affix_page::MapTitle;
        let recent = if self.log.is_some() { self.at.elapsed().unwrap_or_default() <= MAP_FOR } else { self.closed.elapsed() <= LEVEL_WITHIN };
        if recent { MapTitle::This { from_log: self.from_log } } else { MapTitle::Earlier { at: self.at } }
    }
}

/// One entry of a read, for the samples' log: where it is (frame pixels) and its bullet's colour, the text as read
/// (game text: the panel's), and what it matched: the sentence's first id with its grade and whether exactly, or
/// ambiguous; the user's grade a near misread took; whether the panel's bottom cuts it. Lets a visit's log tell what
/// each frame stood for without the screenshots (2026-11-09: the reads' text took a diagnostic test to see).
#[derive(Clone, Debug, PartialEq)]
pub struct ReadEntry {
    pub top: i32,
    pub bottom: i32,
    pub colour: crate::affix::BulletColour,
    pub text: String,
    pub found: Option<(String, Grade, bool)>,
    pub ambiguous: bool,
    pub user: Option<Grade>,
    pub cut: bool,
}

/// What the live frames did, for the samples' log (`affix_samples`: private builds, observe mode and run logs on), so
/// a live test shows where the time went.
#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    /// The affix frames' switch and style: when a page shows, and when either changes.
    Setup { enabled: bool, style: FrameStyle },
    /// The panel scrolled (its bullets moved together by `dy` frame rows, no click): the frames went (`up`: there were
    /// frames) and the panel is read again once it rests.
    Scrolled { dy: i32, up: bool },
    /// The game came to the front or left it while a page shows (`boxes`: the frames there were to show): frames are
    /// shown only with the game in front, so a visit's log tells when they were hidden for it.
    Front { front: bool, boxes: usize },
    /// The user changed an affix's grade on the affix page (`id`: the sentence's id; None is the default grade).
    Grade { id: String, from: Option<Grade>, to: Option<Grade> },
    /// An Add (`reset` false) or Reset click took the frames down (`up`: there were frames).
    FramesDown { reset: bool, up: bool },
    /// A read came back. Its frame: what it was taken after (`open`, `add`, `revert`, `refresh`, `retry`) and when;
    /// the panel's grey and whether it was at rest. The read: how long it waited for the worker and took. What it
    /// found, and the frames up (each frame's grade and top, frame pixels); how long after the page showed (the first
    /// read since) and after the last click (the first frames since) the frames went up.
    Read {
        after: &'static str,
        frame_at: SystemTime,
        grey: f64,
        at_rest: bool,
        wait_ms: u64,
        read_ms: u64,
        entries: usize,
        found: usize,
        unknown: usize,
        ambiguous: usize,
        cut: usize,
        boxes: usize,
        frames: Vec<(Grade, i32)>,
        since_open_ms: Option<u64>,
        since_click_ms: Option<u64>,
        /// Each entry read ([`ReadEntry`]), whether the panel is of the measured interface scale (None: no bullet), and
        /// whether its title bar showed ([`crate::affix::panel::title_shows`]).
        items: Vec<ReadEntry>,
        scale: Option<bool>,
        /// The interface scale the panel was placed at (`affix::read::read_placed`; None: the measured one).
        ui_scale: Option<f64>,
        title: bool,
    },
    /// The panel after the clicks was the one before: its frames put back without a read (each frame's grade and top),
    /// this long after the last click.
    Restored { boxes: usize, frames: Vec<(Grade, i32)>, since_click_ms: u64 },
    /// The quick frames ended ([`QUICK_FOR`]) with no panel read: how many frames came, how many of them were dark
    /// enough to be the panel ([`panel::on_screen`]), and the darkest grey seen (a panel kept from reads by the grey
    /// threshold, a game brighter than measured, shows here).
    Unread { frames: u32, dark: u32, min_grey: Option<f64> },
    /// The map entered after the page closed, as the game log names its affixes, against the page's last read
    /// (`read`: its entries): the log's ids, those of them the read did not find (`missing`: not read, unknown, or
    /// read as another sentence) and the sentences the read found that the log does not name (`extra`, by their keys).
    /// Sentences are compared, so another id of one read sentence is no difference. The affix page shows the log's.
    LogCheck { level: Option<u32>, read: usize, log: Vec<u32>, missing: Vec<u32>, extra: Vec<String> },
    /// The interface scale measured on the panel (`measured`, 1 for the measured one) is not the game log's (`logged`),
    /// which came after the scale last changed: not framed, the page says another scale.
    ScaleMismatch { measured: f64, logged: f64 },
    /// The panel went from the screen with the page still open in the log (a popup in its place): the frames went
    /// (`up`: there were frames) this long after the first frame without it ([`GONE_AFTER`]).
    FramesGone { up: bool, after_ms: u64 },
}

/// The frames that came during the quick frames, for [`Note::Unread`].
#[derive(Default)]
struct QuickSeen {
    frames: u32,
    dark: u32,
    min_grey: Option<f64>,
}

/// A read the worker did: its number, the panel, the frame's size, and when it started and how long it took.
struct Done {
    seq: u64,
    panel: Panel,
    /// Whether the panel is of the interface scale the reader was measured at (None: no bullet showed).
    scale: Option<bool>,
    /// Where the panel was drawn when at another interface scale (`affix::read::read_placed`).
    place: Option<Placement>,
    /// The language the panel was read in (the one set in Ascendant when the read began).
    read_in: crate::lang::Lang,
    size: (usize, usize),
    started: Instant,
    read_ms: u64,
}

/// A read sent ahead while the page comes in: the panel was not known to be at rest yet (two frames alike), so its read
/// is shown only once the next frame is the same ([`AffixLive::frame`]); another frame drops it. The read then overlaps
/// the wait for that frame: the frames come up about one read sooner after the page shows (2026-11-09).
struct Ahead {
    seq: u64,
    print: Vec<u8>,
    confirmed: bool,
}

/// A frame sent to be read, until its read comes back: its print, its text print ([`panel::text_print`]) and what the
/// read's note needs.
struct Sent {
    seq: u64,
    print: Vec<u8>,
    text: Vec<u8>,
    after: &'static str,
    frame_at: SystemTime,
    at_rest: bool,
    sent: Instant,
}

/// The panel on show before a click (put back when the panel after the clicks is the same): the print of the frame it
/// was read from, its frames, entries, frame size and the read itself (its entries' rows, to move the frames down after
/// an Add, [`moved_down`]).
struct Before {
    print: Vec<u8>,
    boxes: Vec<(Rect, Grade)>,
    seen: Vec<Seen>,
    size: (usize, usize),
    read: Panel,
    /// The text print ([`panel::text_print`]) of the frame read, when known: the panel the same as this one is told by
    /// its words, whatever the background behind it did.
    text: Option<Vec<u8>>,
    /// When `read` came back: the frames put back are of that read, so a read of another panel landing among the clicks
    /// does not leave its time and words to the layout check ([`LAYOUT_RECHECK`]).
    read_at: Option<Instant>,
}

/// Clicks on Add or Reset not settled yet (the panel after them not looked at): when the last came (the button let go
/// [`RELEASED_AFTER`] later), when the first came, when the last Reset came and the print of the panel on show then
/// (the one it replaces), and the panel on show before the first.
struct Click {
    at: Instant,
    first: Instant,
    reset_at: Option<Instant>,
    reset_print: Option<Vec<u8>>,
    reset_text: Option<Vec<u8>>,
    before: Option<Before>,
}

/// The live frames.
pub struct AffixLive {
    overlay: Option<AffixOverlay>,
    jobs: Sender<(u64, Rgba)>,
    done: Receiver<Done>,
    /// Reads sent and the newest one back.
    sent: u64,
    shown_seq: u64,
    /// The frames sent whose reads have not come back (the newest few).
    in_flight: Vec<Sent>,
    /// The print of the frame the frames on show were read from (or put back from).
    shown_print: Option<Vec<u8>>,
    /// The page showing (None: no page, hidden or closed).
    page: Option<MapPage>,
    /// The visit is over (closed): the next page shown starts a new one.
    closed: bool,
    /// The print of the last frame sent to be read ([`panel::print`]).
    last_print: Option<Vec<u8>>,
    /// The print of the frame before (sent or not): a frame alike shows the panel at rest.
    prev_print: Option<Vec<u8>>,
    /// The text print ([`panel::text_print`]) of the frame before.
    prev_text: Option<Vec<u8>>,
    /// The last frame sent was of the panel at rest (see `frame`).
    sent_at_rest: bool,
    /// Reads again of the panel as it is ([`RETRY_READS`]), whether the next frame is read whatever it shows, and the
    /// frame to ask of the capture ([`AffixLive::wants_shot_in`]).
    retries: u32,
    read_again: bool,
    shot_in: Option<Duration>,
    /// Quick frames ([`QUICK_EVERY`]) since then: the page shown or a click, until the panel at rest is sent to be read
    /// or [`QUICK_FOR`] passed; and the frames seen since.
    quick: Option<Instant>,
    quick_seen: QuickSeen,
    /// Clicks not settled yet (see the module notes).
    click: Option<Click>,
    /// When the page showed and when the last click came, until the first read (or frames put back) since: the notes'
    /// times to frames.
    opened: Option<Instant>,
    last_click: Option<Instant>,
    /// What happened, for the samples' log ([`AffixLive::take_notes`]).
    notes: Vec<Note>,
    /// When the last map page frame came (or the page showed).
    last_frame: Option<Instant>,
    /// The frames of the newest read, in frame pixels, and that frame's size.
    boxes: Vec<(Rect, Grade)>,
    size: (usize, usize),
    /// Entries read whole this visit: their key and what they matched.
    memory: Vec<(String, Lookup)>,
    /// The entries of the newest read, top to bottom, found or not: the affix page lists them first.
    seen: Vec<Seen>,
    /// The newest read as shown (entries found in an earlier read of the same panel kept, [`keep_found`]); empty when
    /// the frames are not of a read of the panel on show.
    last_read: Panel,
    /// Frames in a row that did not show the panel ([`GONE_AFTER`]).
    gone: u32,
    /// When the first of those frames came.
    gone_since: Option<Instant>,
    /// Texts read exactly as one sentence one safe character from others, counted this page visit
    /// (`affix::read::ExactSeen`): such an entry is that sentence from its second read.
    exact_seen: crate::affix::read::ExactSeen,
    /// The panel last read is of another interface scale than the reader was measured at (`affix::read::read_scaled`):
    /// read as no entries, so not framed, and the affix page says so. A read with no bullet leaves it as it is.
    odd_scale: bool,
    /// The affixes of the map page last closed, shown on the affix page while no page shows ([`MapShown`]).
    map: Option<MapShown>,
    /// Where the panel last read was drawn at another interface scale (None: the measured one): the frames, and the
    /// bullets looked at between reads (a scroll, an Add), go through it.
    place: Option<Placement>,
    /// The interface scale last read (1 for the measured one) and when a read first showed it: a change of the scale is
    /// seen there (the slider moved in the hideout).
    scale_seen: Option<(f64, Instant)>,
    /// The scale the game log last gave (the HUD's `PageScale` on a level entered, as a multiple of the measured one,
    /// `vorax::ui_scale::from_micro`) and when it came: checked against the one measured only when it came after the
    /// last change seen (the log's is written on entering a level only: older, it may be the slider's place before).
    log_scale: Option<(f64, Instant)>,
    /// The panel's words read as the other client language than the one set in Ascendant, on two reads of the page
    /// on show (`affix::read::words_language`; one read could be a panel fading in): the affix page says so. A read
    /// in the set language clears it, one whose words tell nothing leaves it.
    client: Option<crate::lang::Lang>,
    /// The other language the last read that told one pointed to, for [`AffixLive::client`]'s second read.
    client_once: Option<crate::lang::Lang>,
    /// When the last Add on a full panel came, while its panel is watched for growing ([`FULL_ADD_WATCH`]).
    full_add: Option<Instant>,
    /// When [`AffixLive::last_read`] came back.
    last_read_at: Option<Instant>,
    /// The text print ([`panel::text_print`]) of the frame [`AffixLive::last_read`] was read from; None when not known.
    last_read_text: Option<Vec<u8>>,
    /// The read ahead of the page coming in, and its read when it came back before the frame that confirms it.
    ahead: Option<Ahead>,
    held: Option<Done>,
    style: FrameStyle,
    /// Whether the game was in front at the last poll while a page shows (None: not polled since the page showed).
    front: Option<bool>,
    /// The user's grades of the last [`AffixLive::configure`] (None before the first: the first sets them, no notes).
    grades: Option<Grades>,
    /// Where each grade change is appended ([`GRADES_FILE`] in the app's `affix` folder), whether samples are taken or
    /// not; none in unit tests.
    grades_file: Option<std::path::PathBuf>,
    /// The frames are on (the affix page's switch).
    enabled: bool,
    /// The grade table the worker reads with: the defaults with the user's grades.
    dict: Arc<Mutex<Dict>>,
}

impl AffixLive {
    /// The live frames with their worker thread; `wake` repaints the window when a read is done (the UI polls then).
    pub fn new(wake: impl Fn() + Send + 'static) -> AffixLive {
        let (jobs, inbox) = channel::<(u64, Rgba)>();
        let (outbox, done) = channel();
        let dict = Arc::new(Mutex::new(Dict::builtin()));
        let table = dict.clone();
        std::thread::spawn(move || {
            while let Ok((mut seq, mut frame)) = inbox.recv() {
                // only the newest frame waiting is worth reading
                while let Ok((s, f)) = inbox.try_recv() {
                    (seq, frame) = (s, f);
                }
                let started = Instant::now();
                // The OCR cache gives back an earlier read for a crop that looks alike, and one reverted line can look
                // alike: every panel is read afresh. This empties the other reads' cache too, a speed cost only (its
                // slots are keyed by region and size, so no other read ever gets the panel's text); the map page and the
                // boards are not on screen together.
                ocr::clear_cache();
                let dict = table.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let read_in = crate::lang::lang();
                let (panel, scale, place) = crate::affix::read::read_placed(&frame, &dict);
                let read_ms = ms(started.elapsed());
                if outbox.send(Done { seq, panel, scale, place, read_in, size: (frame.w, frame.h), started, read_ms }).is_err() {
                    break;
                }
                wake();
            }
        });
        AffixLive {
            overlay: None,
            jobs,
            done,
            sent: 0,
            shown_seq: 0,
            in_flight: Vec::new(),
            shown_print: None,
            page: None,
            closed: true,
            last_print: None,
            prev_print: None,
            prev_text: None,
            sent_at_rest: false,
            retries: 0,
            read_again: false,
            shot_in: None,
            quick: None,
            quick_seen: QuickSeen::default(),
            click: None,
            opened: None,
            last_click: None,
            notes: Vec::new(),
            last_frame: None,
            boxes: Vec::new(),
            size: (0, 0),
            memory: Vec::new(),
            seen: Vec::new(),
            last_read: Panel::default(),
            gone: 0,
            gone_since: None,
            exact_seen: crate::affix::read::ExactSeen::default(),
            odd_scale: false,
            client: None,
            client_once: None,
            map: None,
            place: None,
            scale_seen: None,
            log_scale: None,
            full_add: None,
            last_read_at: None,
            last_read_text: None,
            ahead: None,
            held: None,
            style: FrameStyle::default(),
            front: None,
            grades: None,
            grades_file: if cfg!(test) { None } else { super::settings::app_dir().map(|d| d.join("affix").join(GRADES_FILE)) },
            enabled: true,
            dict,
        }
    }

    /// The affix page's settings: the switch, the frame style and the user's grades, with the affixes learned from the
    /// game log ([`crate::affix::learned`]). A changed table reads the panel again on the next frame (at most a second
    /// away while a page shows); the switch off hides the frames, on again with a page showing reads it as the page
    /// coming in (quick frames; the capture's once-a-second frames come back with the switch, `Watcher::map_frames`).
    pub fn configure(&mut self, enabled: bool, style: FrameStyle, grades: &Grades, learned: &[Learned]) {
        if let Some(old) = self.grades.as_ref().filter(|old| *old != grades) {
            let ids: std::collections::BTreeSet<&String> = old.keys().chain(grades.keys()).collect();
            for id in ids {
                let (from, to) = (old.get(id).copied(), grades.get(id).copied());
                if from != to {
                    self.notes.push(Note::Grade { id: id.clone(), from, to });
                    append_grade(self.grades_file.as_deref(), id, from, to);
                }
            }
        }
        if self.grades.as_ref() != Some(grades) {
            self.grades = Some(grades.clone());
        }
        if (enabled, style) != (self.enabled, self.style) && self.page.is_some() {
            self.notes.push(Note::Setup { enabled, style });
        }
        self.style = style;
        if enabled != self.enabled {
            self.enabled = enabled;
            self.boxes.clear();
            self.shown_print = None;
            self.last_print = None;
            self.hide();
            if enabled && self.page.is_some() {
                // no frame came while off: not stale, the next frame read as the first of the panel
                let now = Instant::now();
                self.last_frame = Some(now);
                self.read_afresh();
                self.shown_seq = self.sent;
                (self.click, self.full_add) = (None, None);
                self.start_quick(now);
                self.ask(QUICK_EVERY);
            }
        }
        let table = Dict::builtin().with_learned(learned).with_grades(grades);
        let mut dict = self.dict.lock().unwrap_or_else(PoisonError::into_inner);
        // the same sentences in the same places (a grade picked changes no more), and the same grades too
        let same_list = dict.sentences().len() == table.sentences().len()
            && dict.sentences().iter().zip(table.sentences()).all(|(a, b)| a.en == b.en && a.zh == b.zh && a.ids == b.ids);
        let same = same_list && dict.sentences().iter().zip(table.sentences()).all(|(a, b)| a.grade == b.grade);
        if !same {
            *dict = table;
            // the map shown: the log's ids again in the new table (an affix of it just learned gets its words); a
            // read's list goes, its indices of the old table
            if !same_list && let Some(m) = &mut self.map {
                m.seen = m.log.as_ref().map(|ids| ids.iter().map(|&id| Seen::of_id(&dict, id)).collect()).unwrap_or_default();
            }
            drop(dict);
            self.read_afresh();
            // the memory holds matches graded the old way: the next frame reads them again. Frames to put back after a
            // click were of the old table too, and so are the frames on show: no later click puts them back.
            self.memory.clear();
            self.exact_seen.clear();
            // The sentences seen are indices into the table: a fold of learned affixes moves the indices after it, so
            // they go until the next read. A grade picked moves none, and the affix page shows each one's grade from
            // the grades themselves: they stay. Cleared, the page's list of this map went for the read after each grade
            // picked in it (0.2 to 0.6 s on 2026-11-10) and the whole page jumped up and back (the user, 2026-11-10).
            if !same_list {
                self.seen.clear();
            }
            self.shown_print = None;
            if let Some(c) = &mut self.click {
                c.before = None;
            }
            // reads still on the way were made with the old table: dropped, or they would bring the old indices and
            // grades back (into the memory, for the rest of the visit)
            self.shown_seq = self.sent;
        }
    }

    /// The entries on the map page now, top to bottom (found ones by table index); none when no page shows.
    /// Whether the panel last on screen was of another interface scale than the reader was measured at (nothing is
    /// read or framed then; the affix page says so).
    pub fn odd_scale(&self) -> bool {
        self.odd_scale
    }

    /// The game client language the panel on show reads as, when it is not the one set in Ascendant (the affix page
    /// says to set it: the OCR engine follows the setting).
    pub fn client_hint(&self) -> Option<crate::lang::Lang> {
        self.client
    }

    /// Whether frames are up on the game (the switch on, a page showing, frames to show, the game not known to be away
    /// from the front): the capture then looks at the page twice a second (`Watcher::map_watch`). With the game behind
    /// another window the frames are hidden, and the page is looked at once a second as before.
    pub fn frames_up(&self) -> bool {
        self.enabled && self.page.is_some() && !self.boxes.is_empty() && self.front != Some(false)
    }

    /// The entries on the map page now (found ones by table index); with no page on show, those of the map page last
    /// closed and the map entered from it ([`MapShown`]), else none.
    pub fn seen(&self) -> &[Seen] {
        match (&self.page, &self.map) {
            (Some(_), _) => &self.seen,
            (None, Some(m)) => &m.seen,
            (None, None) => &[],
        }
    }

    /// What the affix page says of the panel and of the list it shows: another interface scale, the other client
    /// language, and with no page on show whose list it is ([`MapShown::title`]).
    pub fn hints(&self) -> super::affix_page::Hints {
        let map = if self.page.is_none() { self.map.as_ref().map(MapShown::title) } else { None };
        super::affix_page::Hints { odd_scale: self.odd_scale(), client: self.client_hint(), map }
    }

    /// A level entered ([`Signal::MapAffixes`]): the hideout drops the map shown; a map's affixes as the game log
    /// names them replace it, checked against the page's last read when the page closed just before
    /// ([`LEVEL_WITHIN`], [`Note::LogCheck`]). A level whose block names no panel affix (other
    /// content) changes nothing.
    fn entered(&mut self, a: &MapAffixes) {
        if a.level.is_some_and(|l| super::gamelog::HIDEOUT_LEVELS.contains(&l)) {
            self.map = None;
            return;
        }
        if a.ids().is_empty() {
            return;
        }
        let dict = self.dict.lock().unwrap_or_else(PoisonError::into_inner);
        let logged: Vec<Seen> = a.ids().iter().map(|&id| Seen::of_id(&dict, id)).collect();
        // the read of the page closed just before, when the level is its map
        let read = self.map.take().filter(|m| m.log.is_none() && m.closed.elapsed() <= LEVEL_WITHIN).map(|m| m.seen);
        let known = |list: &[Seen]| list.iter().filter_map(|s| if let Seen::Known(i) = s { Some(*i) } else { None }).collect::<Vec<usize>>();
        let (read_known, log_known) = (known(read.as_deref().unwrap_or_default()), known(&logged));
        let missing: Vec<u32> = a.ids().iter().zip(&logged).filter(|(_, s)| !matches!(s, Seen::Known(i) if read_known.contains(i))).map(|(&id, _)| id).collect();
        let extra: Vec<String> = read_known.iter().filter(|i| !log_known.contains(i)).map(|&i| dict.sentences()[i].key().to_string()).collect();
        drop(dict);
        let agrees = read.as_ref().is_some_and(|r| missing.is_empty() && extra.is_empty() && r.len() == logged.len());
        if let Some(r) = &read {
            self.notes.push(Note::LogCheck { level: a.level, read: r.len(), log: a.ids().to_vec(), missing, extra });
        }
        // the log's list, unless the read found the same (then in the panel's order)
        let (seen, from_log) = match read {
            Some(r) if agrees => (r, false),
            _ => (logged, true),
        };
        self.map = Some(MapShown { seen, log: Some(a.ids().to_vec()), from_log, at: SystemTime::now(), closed: Instant::now() });
    }

    /// Whether the frames need looking after on this tick: a page shows and frames keep coming, or frames are up.
    pub fn active(&self) -> bool {
        (self.page.is_some() && !self.stale(Instant::now())) || self.overlay.as_ref().is_some_and(AffixOverlay::shown)
    }

    /// No map page frame for [`STALE`].
    fn stale(&self, now: Instant) -> bool {
        self.last_frame.is_none_or(|at| now.duration_since(at) > STALE)
    }

    /// What happened since the last call, for the samples' log.
    pub fn take_notes(&mut self) -> Vec<Note> {
        std::mem::take(&mut self.notes)
    }

    /// A map page signal from the game log.
    pub fn note(&mut self, signal: Signal) {
        let (page, state) = match signal {
            Signal::MapPage { page, state } => (page, state),
            Signal::AffixButton { revert } => {
                self.clicked(revert);
                return;
            }
            Signal::MapAffixes(a) => {
                self.entered(&a);
                return;
            }
            Signal::UiScale { micro } => {
                self.log_scale = Some((crate::vorax::ui_scale::from_micro(micro), Instant::now()));
                return;
            }
            _ => return,
        };
        match state {
            PageState::Shown => {
                let now = Instant::now();
                let long_gone = self.last_frame.is_none_or(|at| now.duration_since(at) > NEW_VISIT_AFTER);
                if self.closed || long_gone || self.page.is_some_and(|p| p != page) {
                    self.memory.clear();
                    self.exact_seen.clear();
                }
                self.closed = false;
                self.page = Some(page);
                // the page's own list from now on: the map shown before goes
                self.map = None;
                self.last_frame = Some(now);
                // read the next panel at rest whatever it shows; reads still on the way are of the page as it was before
                self.read_afresh();
                self.shown_seq = self.sent;
                self.boxes.clear();
                self.shown_print = None;
                self.click = None;
                self.gone = 0;
                self.full_add = None;
                self.opened = Some(now);
                self.start_quick(now);
                self.ask(QUICK_EVERY);
                self.notes.push(Note::Setup { enabled: self.enabled, style: self.style });
                self.front = None;
            }
            PageState::Hidden | PageState::Closed => {
                // the last read's entries stay on the affix page for the map entered next (or until a page shows)
                if state == PageState::Closed && self.page.is_some() {
                    self.map = Some(MapShown { seen: self.seen.clone(), log: None, from_log: false, at: SystemTime::now(), closed: Instant::now() });
                }
                self.page = None;
                // the hints are of the panel on show: none once the page goes
                self.odd_scale = false;
                (self.client, self.client_once) = (None, None);
                self.closed |= state == PageState::Closed;
                self.boxes.clear();
                self.shown_print = None;
                self.last_read = Panel::default();
                self.quick = None;
                self.click = None;
                self.full_add = None;
                self.hide();
            }
        }
    }

    /// An Add (`revert` false) or Reset click: the frames go down and the reads on the way (of the panel before) are
    /// dropped; a click while earlier ones are not settled yet (the button held down, or the panel after them not
    /// looked at yet) is part of the same clicks, which keep the panel on show before the first of them. A Reset also
    /// forgets the entries remembered whole: the cut last entry must not take the grade of an affix just reset away.
    /// An Add on a full panel ([`MAX_AFFIXES`] entries read on the panel on show, no clicks pending) changes nothing:
    /// the frames stay up and nothing is read.
    fn clicked(&mut self, revert: bool) {
        let now = Instant::now();
        if !revert && self.click.is_none() && self.shown_print.is_some() && self.last_read.entries.len() >= MAX_AFFIXES {
            // watched a while all the same: should the panel grow after all (a bullet too many read, a map that takes
            // more than six), the frames go then ([`AffixLive::full_add_watch`])
            self.full_add = Some(now);
            self.ask(QUICK_EVERY);
            return;
        }
        let mut click = match self.click.take() {
            Some(c) => Click { at: now, ..c },
            None => Click {
                at: now,
                first: now,
                reset_at: None,
                reset_print: None,
                reset_text: None,
                before: self.shown_print.clone().map(|print| Before {
                    print,
                    boxes: self.boxes.clone(),
                    seen: self.seen.clone(),
                    size: self.size,
                    read: self.last_read.clone(),
                    text: self.last_read_text.clone(),
                    read_at: self.last_read_at,
                }),
            },
        };
        if revert {
            // the panel on show now (the newest frame since the last click, else the one at an earlier Reset) is the
            // one this Reset replaces: waited out as the one before the clicks is
            click.reset_print = self.prev_print.clone().or(click.reset_print.take());
            click.reset_text = self.prev_text.clone().or(click.reset_text.take());
            click.reset_at = Some(now);
        }
        self.full_add = None;
        self.notes.push(Note::FramesDown { reset: revert, up: !self.boxes.is_empty() });
        // the frames on show are kept in `before` (the first click) or were down already: none to put back later
        self.boxes.clear();
        self.shown_print = None;
        self.hide();
        self.shown_seq = self.sent;
        self.read_afresh();
        if revert {
            self.memory.clear();
        }
        self.click = Some(click);
        self.last_click = Some(now);
        self.start_quick(now);
        self.ask(RELEASED_AFTER);
    }

    /// Quick frames from `now` (the page shown or a click), the frames seen counted afresh.
    fn start_quick(&mut self, now: Instant) {
        self.quick = Some(now);
        self.quick_seen = QuickSeen::default();
    }

    /// The next frame is read whatever it shows, as the first of a panel.
    fn read_afresh(&mut self) {
        self.last_print = None;
        self.prev_print = None;
        self.prev_text = None;
        self.sent_at_rest = false;
        self.retries = 0;
        self.read_again = false;
        self.shot_in = None;
        self.last_read = Panel::default();
        (self.ahead, self.held) = (None, None);
    }

    /// Asks the capture for a frame `after` from now (the soonest of what is asked).
    fn ask(&mut self, after: Duration) {
        self.shot_in = Some(self.shot_in.map_or(after, |s| s.min(after)));
    }

    /// A map page frame from the capture (`after`: what it was taken after; `at`: when). See the module notes for when
    /// it is read.
    pub fn frame(&mut self, frame: &Rgba, after: &'static str, at: SystemTime) {
        if self.page.is_none() || !self.enabled {
            return;
        }
        let now = Instant::now();
        self.last_frame = Some(now);
        let print = panel::print(frame);
        let at_rest = self.prev_print.as_ref().is_some_and(|prev| panel::same(prev, &print));
        self.prev_print = Some(print.clone());
        let text = panel::text_print(frame);
        self.prev_text = Some(text.clone());
        let dark = panel::on_screen(&print, frame.w, frame.h);
        if self.quick.is_some_and(|since| now.duration_since(since) >= QUICK_FOR) {
            // a panel at rest read (or frames put back) ends the quick frames before this: none was
            self.quick = None;
            let QuickSeen { frames, dark, min_grey } = std::mem::take(&mut self.quick_seen);
            self.notes.push(Note::Unread { frames, dark, min_grey });
        }
        let quick = self.quick.is_some();
        if quick {
            let seen = &mut self.quick_seen;
            let grey = panel::grey(&print);
            seen.frames += 1;
            seen.dark += u32::from(dark);
            seen.min_grey = Some(seen.min_grey.map_or(grey, |g| g.min(grey)));
        }
        // still clicking (the button held down): the panel changes with each click
        if let Some(c) = &self.click
            && now.duration_since(c.at) < RELEASED_AFTER
        {
            self.ask(RELEASED_AFTER - now.duration_since(c.at));
            return;
        }
        // the panel not on screen (the map before it comes in, another menu over it): nothing to read, and frames up go
        // once it stays away (a popup in its place says nothing in the log); the next frame looked at soon to see that
        if !dark {
            self.gone += 1;
            if self.gone == 1 {
                self.gone_since = Some(now);
            }
            if !self.boxes.is_empty() || self.shown_print.is_some() {
                if self.gone >= GONE_AFTER {
                    let since = self.gone_since.map_or(0, |t| ms(now.duration_since(t)));
                    self.notes.push(Note::FramesGone { up: !self.boxes.is_empty(), after_ms: since });
                    self.boxes.clear();
                    self.shown_print = None;
                    self.hide();
                    self.read_afresh();
                    // the panel coming back is read once at rest, as when the page shows (not while it fades in)
                    self.start_quick(now);
                    self.ask(QUICK_EVERY);
                } else {
                    self.ask(QUICK_EVERY);
                }
            }
            if quick {
                self.ask(QUICK_EVERY);
            }
            return;
        }
        self.gone = 0;
        if self.full_add.is_some() {
            let tops = crate::affix::read::entry_tops_at(frame, self.place);
            if self.full_add_watch(now, &tops, frame.h) {
                return;
            }
        }
        if let Some(c) = &self.click {
            if quick && !at_rest {
                self.ask(QUICK_EVERY);
                return;
            }
            // the same words as before the clicks (or at the Reset): the text print, as the grey print changes with
            // the background behind the panel (a Reset's old panel, read as a new one, was framed for 0.2 s)
            let same = c.before.as_ref().is_some_and(|b| b.text.as_ref().map_or_else(|| panel::same(&b.print, &print), |t| panel::same_text(t, &text)));
            let waiting = quick
                && match c.reset_at {
                    // the old panel shows a while after a Reset (the one before the clicks, or after an Add among
                    // them): wait for the new one
                    Some(at) => now.duration_since(at) < RESET_WAIT && (same || c.reset_text.as_ref().map_or_else(|| c.reset_print.as_ref().is_some_and(|p| panel::same(p, &print)), |t| panel::same_text(t, &text))),
                    // the game may not have drawn the added affix yet: a single click's panel still the same waits
                    None => same && now.duration_since(c.first) < ADD_WAIT,
                };
            if waiting {
                self.ask(QUICK_EVERY);
                return;
            }
            let c = self.click.take().expect("a click");
            if same && let Some(b) = c.before {
                // the panel is the one before the clicks: its frames go back up, no read needed
                let frames = b.boxes.iter().map(|(r, g)| (*g, r.y)).collect();
                self.notes.push(Note::Restored { boxes: b.boxes.len(), frames, since_click_ms: ms(now.duration_since(c.at)) });
                (self.boxes, self.seen, self.size, self.shown_print, self.last_read) = (b.boxes, b.seen, b.size, Some(b.print), b.read);
                (self.last_read_at, self.last_read_text) = (b.read_at, b.text);
                (self.last_print, self.sent_at_rest, self.quick, self.last_click) = (Some(print), true, None, None);
                return;
            }
            // Adds only: the frames from before go back up at once, moved down past the new entries at the top (their
            // rows from the bullets, no OCR), until the read of this panel replaces them
            if c.reset_at.is_none()
                && let Some(b) = &c.before
                && b.size == (frame.w, frame.h)
                && let Some(boxes) = moved_down(&b.read, &crate::affix::read::entry_tops_at(frame, self.place), frame.h, self.place)
            {
                (self.boxes, self.size) = (boxes, b.size);
            }
            self.send(frame, print, at_rest, after, at);
            return;
        }
        // a read ahead of the page coming in (see `Ahead`): this frame alike shows it was of the panel at rest, and its
        // read is shown (now, or when it comes); another frame drops it
        if let Some(a) = &self.ahead
            && !a.confirmed
        {
            if at_rest && panel::same(&a.print, &print) {
                let seq = a.seq;
                self.ahead.as_mut().expect("a read ahead").confirmed = true;
                // its frame was of the panel at rest after all (the samples' log says so)
                if let Some(s) = self.in_flight.iter_mut().find(|s| s.seq == seq) {
                    s.at_rest = true;
                }
                (self.quick, self.sent_at_rest, self.last_print) = (None, true, Some(print));
                if let Some(done) = self.held.take() {
                    self.take(done);
                }
                return;
            }
            self.shown_seq = self.shown_seq.max(a.seq);
            (self.ahead, self.held) = (None, None);
        }
        // the page coming in: the panel is read once at rest; the first frame of it is read ahead while the next one
        // shows whether it was at rest (its read is shown only then)
        if quick && !at_rest {
            if self.opened.is_some() && self.click.is_none() && self.last_read.entries.is_empty() {
                self.send(frame, print.clone(), false, after, at);
                self.ahead = Some(Ahead { seq: self.sent, print, confirmed: false });
            }
            self.ask(QUICK_EVERY);
            return;
        }
        let changed = !self.last_print.as_ref().is_some_and(|last| panel::same(last, &print));
        let tops = if changed { crate::affix::read::entry_tops_at(frame, self.place) } else { Vec::new() };
        // the panel scrolled (the user's wheel: no click, nothing in the log), or entries came in at its top as the page
        // filled (18-38-37 of 11-08): its bullets moved together, so the frames up stand on other entries now. They go
        // at once; this frame is read ahead, shown only if the next one is alike (the panel at rest, see `Ahead`), else
        // the quick frames follow the panel and it is read once it rests (the once-a-second frames saw a scroll up to a
        // second late, and the read of a frame mid-scroll framed a moving panel)
        if changed
            && !quick
            && (!self.boxes.is_empty() || self.shown_print.is_some())
            && let Some(dy) = self.scroll_shift(&tops, frame.h)
        {
            self.notes.push(Note::Scrolled { dy, up: !self.boxes.is_empty() });
            self.boxes.clear();
            self.shown_print = None;
            self.hide();
            self.read_afresh();
            // this frame stays the one the next is compared with: alike, the panel rests and the read ahead stands
            (self.prev_print, self.prev_text) = (Some(print.clone()), Some(text));
            self.start_quick(now);
            self.send(frame, print.clone(), false, after, at);
            self.ahead = Some(Ahead { seq: self.sent, print, confirmed: false });
            self.ask(QUICK_EVERY);
            return;
        }
        // the print changed but the entries stand where the last read found them, read a moment ago, with the same words
        // (its text print: a map's other panel with entries at the very same rows has other words): the background
        // behind the half-clear panel moved, not its entries; no read (3 in 4 reads of the live tests of 2026-11-08
        // found what the read before had). A read asked for again goes ahead.
        if changed
            && !self.read_again
            && self.last_read_at.is_some_and(|t| now.duration_since(t) < LAYOUT_RECHECK)
            && self.layout_unchanged(&tops, frame.h)
            && self.last_read_text.as_ref().is_some_and(|t| panel::same_text(t, &panel::text_print(frame)))
        {
            self.last_print = Some(print);
            self.sent_at_rest |= at_rest;
            return;
        }
        if changed {
            // read anyway; whether it is another panel (its own reads again) the read tells by its entries (`take`):
            // the background behind the panel moves the print too
            self.read_again = false;
        }
        let settled = at_rest && !self.sent_at_rest;
        let again = std::mem::take(&mut self.read_again);
        if !(changed || settled || again) {
            return;
        }
        self.send(frame, print, at_rest, after, at);
    }

    /// Whether a frame's bullets (`tops`, on a frame `frame_h` high) stand where the last read's entries do: as many,
    /// none moved by more than [`HEIGHT_SLACK`]. No bullet found, or no read, tells nothing (false).
    fn layout_unchanged(&self, tops: &[i32], frame_h: usize) -> bool {
        let read = &self.last_read.entries;
        let slack = HEIGHT_SLACK * frame_h as f64 / 1440.0;
        !tops.is_empty() && tops.len() == read.len() && tops.iter().zip(read).all(|(t, e)| f64::from((t - e.top).abs()) <= slack)
    }

    /// How far the panel scrolled: the rows (frame pixels) its bullets (`tops`, on a frame `frame_h` high) moved together
    /// from the last read's entries, when at least two of them (one, for a panel of one) stand that far from where they
    /// were, within [`HEIGHT_SLACK`], and they are at least half the bullets (entries scrolled in at an edge are new).
    /// None when they did not move, or not together (a popup over some bullets, an entry added).
    fn scroll_shift(&self, tops: &[i32], frame_h: usize) -> Option<i32> {
        let read: Vec<i32> = self.last_read.entries.iter().map(|e| e.top).collect();
        if tops.is_empty() || read.is_empty() {
            return None;
        }
        let slack = HEIGHT_SLACK * frame_h as f64 / 1440.0;
        let near = |a: i32, b: i32| f64::from((a - b).abs()) <= slack;
        let matches = |dy: i32| tops.iter().filter(|&&t| read.iter().any(|&r| near(t, r + dy))).count();
        let (dy, n) = tops.iter().flat_map(|&t| read.iter().map(move |&r| t - r)).filter(|&dy| !near(dy, 0)).map(|dy| (dy, matches(dy))).max_by_key(|&(dy, n)| (n, -dy.abs()))?;
        let enough = if read.len() == 1 && tops.len() == 1 { n == 1 } else { n >= 2 };
        (enough && n * 2 >= tops.len() && matches(0) < n).then_some(dy)
    }

    /// The watch after an Add on a full panel ([`FULL_ADD_WATCH`]), with the tops of a frame's bullets (`frame_h` rows):
    /// more bullets than the entries read, or as many with a top moved by more than [`HEIGHT_SLACK`] (a new affix at
    /// the top pushed the last one off the panel), take the frames down and read the panel at rest (true); else the
    /// watch goes on, a frame soon, until it is over. No bullet found tells nothing.
    fn full_add_watch(&mut self, now: Instant, tops: &[i32], frame_h: usize) -> bool {
        let Some(since) = self.full_add else { return false };
        let read = self.last_read.entries.len();
        let moved = tops.len() == read && !self.layout_unchanged(tops, frame_h);
        if tops.len() > read || moved {
            self.full_add = None;
            self.notes.push(Note::FramesDown { reset: false, up: !self.boxes.is_empty() });
            self.boxes.clear();
            self.shown_print = None;
            self.hide();
            self.read_afresh();
            self.start_quick(now);
            self.ask(QUICK_EVERY);
            return true;
        }
        if now.duration_since(since) >= FULL_ADD_WATCH {
            self.full_add = None;
        } else {
            self.ask(QUICK_EVERY);
        }
        false
    }

    /// Sends a frame to be read; a panel at rest read ends the quick frames.
    fn send(&mut self, frame: &Rgba, print: Vec<u8>, at_rest: bool, after: &'static str, at: SystemTime) {
        self.sent_at_rest = at_rest;
        self.last_print = Some(print.clone());
        if at_rest {
            self.quick = None;
        }
        self.sent += 1;
        self.in_flight.push(Sent { seq: self.sent, print, text: panel::text_print(frame), after, frame_at: at, at_rest, sent: Instant::now() });
        if self.in_flight.len() > 8 {
            self.in_flight.remove(0);
        }
        let _ = self.jobs.send((self.sent, frame.clone()));
    }

    /// The frame to ask of the capture now, if any (`Watcher::map_shot_in`).
    pub fn wants_shot_in(&mut self) -> Option<Duration> {
        self.shot_in.take()
    }

    /// Takes the reads done and shows or hides the frames: shown while a page shows and the game is in front
    /// (`game`: its client area in screen pixels, None when not in front).
    pub fn poll(&mut self, game: Option<[i32; 4]>) {
        while let Ok(done) = self.done.try_recv() {
            self.take(done);
        }
        if self.page.is_some() && self.enabled && self.front != Some(game.is_some()) {
            self.front = Some(game.is_some());
            self.notes.push(Note::Front { front: game.is_some(), boxes: self.boxes.len() });
        }
        if self.page.is_some() && self.stale(Instant::now()) {
            // frames stopped coming: the next frame reads the panel again
            self.boxes.clear();
            self.shown_print = None;
            self.read_afresh();
            self.quick = None;
        }
        match game {
            Some(game) if self.enabled && self.page.is_some() && !self.boxes.is_empty() => {
                if self.overlay.is_none() {
                    self.overlay = AffixOverlay::new();
                }
                if let Some(o) = &mut self.overlay {
                    o.show(&self.boxes, self.size, self.style, game);
                }
            }
            _ => self.hide(),
        }
    }

    /// One read done: its frames and the sentences seen replace the last ones; an incomplete read of the newest frame
    /// sent asks for it to be read again ([`RETRY_READS`]).
    fn take(&mut self, done: Done) {
        // a read ahead not confirmed yet waits for the frame that tells (see `Ahead`)
        if let Some(a) = &self.ahead
            && a.seq == done.seq
        {
            if !a.confirmed {
                self.held = Some(done);
                return;
            }
            self.ahead = None;
        }
        let sent = self.in_flight.iter().position(|s| s.seq == done.seq).map(|i| self.in_flight.remove(i));
        self.in_flight.retain(|s| s.seq > done.seq);
        // a read of a page left meanwhile, or older than one shown, is dropped
        if self.page.is_none() || done.seq <= self.shown_seq {
            return;
        }
        self.shown_seq = done.seq;
        // a panel of another interface scale is read as no entries (no frames): the page tells why
        if let Some(fits) = done.scale {
            self.odd_scale = !fits;
        }
        let mut done = done;
        if done.scale == Some(true) {
            let s = done.place.map_or(1.0, |p| p.s);
            let now = Instant::now();
            if self.scale_seen.is_none_or(|(seen, _)| (seen - s).abs() > SCALE_CHANGE) {
                self.scale_seen = Some((s, now));
            }
            // the log's scale, when it came after the change last seen, must be the one measured: else not framed
            let since = self.scale_seen.map(|(_, at)| at);
            if let Some((logged, at)) = self.log_scale
                && since.is_some_and(|c| at > c)
                && (logged - s).abs() > SCALE_DISAGREE
            {
                self.notes.push(Note::ScaleMismatch { measured: s, logged });
                self.odd_scale = true;
                (done.panel, done.scale, done.place) = (Panel::default(), Some(false), None);
            }
        }
        if done.scale.is_some() {
            self.place = done.place;
        }
        // words of the other client language than the one set: said on the affix page from the second such read
        match crate::affix::read::words_language(&done.panel, done.read_in) {
            Some(l) if l == done.read_in => (self.client, self.client_once) = (None, None),
            Some(l) => {
                if self.client_once == Some(l) {
                    self.client = Some(l);
                }
                self.client_once = Some(l);
            }
            None => {}
        }
        let mut panel = done.panel;
        // an entry unsure only for sentences one safe character from the one it equals: that one from its second read
        {
            let dict = self.dict.lock().unwrap_or_else(PoisonError::into_inner);
            self.exact_seen.confirm(&mut panel, &dict);
        }
        // the same panel as the last read (its background moved): what was found stays found, and its reads again go
        // on counting; another panel starts over
        if alike(&panel, &self.last_read, done.size.1) {
            let dict = self.dict.lock().unwrap_or_else(PoisonError::into_inner);
            keep_found(&mut panel, &self.last_read, &dict);
        } else {
            self.retries = 0;
        }
        remember_and_fill(&mut panel, &mut self.memory);
        self.seen = panel.entries.iter().map(|e| Seen::of(&panel, e)).collect();
        self.boxes = frames::boxes_at(&panel, done.size.1, self.place);
        self.size = done.size;
        // the frames on show now are of this read's frame: its print when it is still known (more than a few frames
        // sent meanwhile drop the oldest), else none, so no click puts these frames back for another panel
        self.shown_print = sent.as_ref().map(|s| s.print.clone());
        let text = sent.as_ref().map(|s| s.text.clone());
        if let Some(s) = sent {
            let now = Instant::now();
            let count = |f: &dyn Fn(&crate::affix::Entry) -> bool| panel.entries.iter().filter(|e| f(e)).count();
            let items = {
                let dict = self.dict.lock().unwrap_or_else(PoisonError::into_inner);
                panel
                    .entries
                    .iter()
                    .map(|e| ReadEntry {
                        top: e.top,
                        bottom: e.bottom,
                        colour: e.colour,
                        text: e.text.clone(),
                        found: match e.lookup {
                            Lookup::Found { index, grade, exact } => {
                                Some((dict.sentences().get(index).and_then(|s| s.ids.first().cloned()).unwrap_or_default(), grade, exact))
                            }
                            _ => None,
                        },
                        ambiguous: e.lookup == Lookup::Ambiguous,
                        user: e.user,
                        cut: e.cut,
                    })
                    .collect()
            };
            self.notes.push(Note::Read {
                after: s.after,
                frame_at: s.frame_at,
                grey: panel::grey(&s.print),
                at_rest: s.at_rest,
                wait_ms: ms(done.started.saturating_duration_since(s.sent)),
                read_ms: done.read_ms,
                entries: panel.entries.len(),
                found: count(&|e| matches!(e.lookup, Lookup::Found { .. })),
                unknown: count(&|e| e.lookup == Lookup::Unknown),
                ambiguous: count(&|e| e.lookup == Lookup::Ambiguous),
                cut: count(&|e| e.cut),
                boxes: self.boxes.len(),
                frames: self.boxes.iter().map(|(r, g)| (*g, r.y)).collect(),
                since_open_ms: self.opened.take().map(|t| ms(now.duration_since(t))),
                since_click_ms: self.last_click.take().map(|t| ms(now.duration_since(t))),
                items,
                scale: done.scale,
                ui_scale: done.place.map(|p| p.s),
                title: panel.title,
            });
        }
        // only the newest frame's read decides: a later frame already sent is read anyway
        let again = done.seq == self.sent && self.retries < RETRY_READS && {
            let dict = self.dict.lock().unwrap_or_else(PoisonError::into_inner);
            worth_reading_again(&panel, &dict)
        };
        if again {
            self.retries += 1;
            self.read_again = true;
            self.ask(retry_wait(self.retries));
        }
        self.last_read = panel;
        self.last_read_at = Some(Instant::now());
        self.last_read_text = text;
    }

    fn hide(&mut self) {
        if let Some(o) = &mut self.overlay {
            o.hide();
        }
    }
}

/// Remembers the entries of `panel` read whole and found, and gives a cut last entry that was not found the match of
/// the one remembered entry whose key starts with its key (at least [`MIN_CUT`] characters; several, or none, leave it
/// as it is). Entries shown whole on this same panel are not candidates: the cut one is the oldest affix, pushed down
/// past them, so it is not one of them, and an affix of another grade can start the same way (5521032, a warning, and
/// 5521031, not framed, share their first 60 characters).
fn remember_and_fill(panel: &mut Panel, memory: &mut Vec<(String, Lookup)>) {
    let on_panel: Vec<String> = panel.entries.iter().filter(|e| !e.cut).map(|e| key(&e.text)).collect();
    for (e, k) in panel.entries.iter().filter(|e| !e.cut).zip(&on_panel) {
        if matches!(e.lookup, Lookup::Found { .. }) && !memory.iter().any(|(m, _)| m == k) {
            memory.push((k.clone(), e.lookup));
        }
    }
    for e in panel.entries.iter_mut().filter(|e| e.cut && !matches!(e.lookup, Lookup::Found { .. })) {
        let k = key(&e.text);
        if k.chars().count() < MIN_CUT {
            continue;
        }
        let mut whole = memory.iter().filter(|(m, _)| m.starts_with(&k) && !on_panel.contains(m));
        if let (Some((_, lookup)), None) = (whole.next(), whole.next()) {
            e.lookup = *lookup;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::affix::{BulletColour, Entry};

    fn entry(text: &str, cut: bool, lookup: Lookup) -> Entry {
        Entry { top: 0, bottom: 10, colour: BulletColour::White, text: text.into(), cut, lookup, user: None }
    }

    /// A cut last entry takes the grade of the whole entry the visit read before; a short cut start, or one that
    /// starts several remembered entries, stays as it was.
    #[test]
    fn cut_entries_take_the_whole_entry_remembered() {
        let found = Lookup::Found { index: 3, grade: Grade::Warn, exact: true };
        let mut memory = Vec::new();
        let mut first = Panel { entries: vec![entry("At Full Life, you deal -9% additional damage every 3 s. Stacks up to 3 times.", false, found)], title: true };
        remember_and_fill(&mut first, &mut memory);
        assert_eq!(memory.len(), 1);
        let mut later = Panel { entries: vec![entry("At Full Life, you deal -9% additional damage", true, Lookup::Unknown), entry("At Full", true, Lookup::Unknown)], title: true };
        remember_and_fill(&mut later, &mut memory);
        assert_eq!(later.entries[0].lookup, found);
        assert_eq!(later.entries[1].lookup, Lookup::Unknown);
        // a second remembered entry with the same start makes it ambiguous: left as it was
        memory.push((key("At Full Life, you deal -9% additional damage for 5 s"), Lookup::Found { index: 4, grade: Grade::Danger, exact: true }));
        let mut again = Panel { entries: vec![entry("At Full Life, you deal -9% additional damage", true, Lookup::Unknown)], title: true };
        remember_and_fill(&mut again, &mut memory);
        assert_eq!(again.entries[0].lookup, Lookup::Unknown);
    }

    /// A cut entry does not take the grade of an entry shown whole on the same panel, even when it starts the same way:
    /// 5521032 (a warning) and 5521031 (not framed) share their first 60 characters.
    #[test]
    fn cut_entries_never_take_an_entry_on_the_same_panel() {
        let warn = Lookup::Found { index: 1, grade: Grade::Warn, exact: true };
        let whole = "When Rare monsters hit a target, they additionally increase the target's Life Regain by -5% for 5 s, up to 5 stacks";
        let cut = "When Rare monsters hit a target, they additionally increase the target's";
        let mut memory = Vec::new();
        let mut panel = Panel { entries: vec![entry(whole, false, warn), entry(cut, true, Lookup::Ambiguous)], title: true };
        remember_and_fill(&mut panel, &mut memory);
        assert_eq!(memory.len(), 1);
        assert_eq!(panel.entries[1].lookup, Lookup::Ambiguous);
    }

    /// Showing a page drops the reads still on the way (of the page as it was) and the frames of the last visit, and
    /// asks for the first quick frame.
    #[test]
    fn showing_a_page_drops_reads_on_the_way() {
        let mut live = AffixLive::new(|| {});
        live.sent = 7;
        live.boxes.push((Rect::new(0, 0, 1, 1), Grade::Danger));
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Shown });
        assert_eq!(live.shown_seq, 7);
        assert!(live.boxes.is_empty());
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY));
    }

    /// A small 16:9 frame of one grey. Dark (up to 80) is the panel on screen, bright the map before it.
    /// A dark one also has a line of "text" (bright pixels) at a row of its text area given by its grey, so two panels
    /// of different greys have different words too (the panel's sameness is told by its words, `panel::text_print`).
    /// The line (two rows) starts right of the bullets' column, as text does: a bright bar across it would be read as
    /// a bullet, moving with the grey as a scroll.
    fn plain(grey: u8) -> Rgba {
        let mut f = Rgba { w: 160, h: 90, data: vec![grey; 160 * 90 * 4] };
        if grey <= 80 {
            let area = panel::text_area(160, 90).expect("16:9");
            let y = area.y as usize + usize::from(grey) % area.h as usize;
            for (x, y) in ((area.x + area.w / 3) as usize..(area.x + area.w) as usize).flat_map(|x| [(x, y), (x, y + 1)]) {
                let i = (y * 160 + x) * 4;
                f.data[i..i + 3].copy_from_slice(&[230, 230, 230]);
            }
        }
        f
    }

    fn shown() -> AffixLive {
        let mut live = AffixLive::new(|| {});
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Shown });
        live
    }

    /// A frame of one grey, taken now; the reads sent so far.
    fn feed(live: &mut AffixLive, grey: u8) -> u64 {
        live.frame(&plain(grey), "open", SystemTime::now());
        live.sent
    }

    /// A read of the newest frame sent.
    fn done(live: &AffixLive, entries: Vec<Entry>) -> Done {
        Done { seq: live.sent, panel: Panel { entries, title: true }, scale: Some(true), place: None, read_in: crate::lang::Lang::En, size: (160, 90), started: Instant::now(), read_ms: 5 }
    }

    /// The clicks let go: as if the last click came RELEASED_AFTER ago (and the first click, and the last Reset, `wait`
    /// before that).
    fn let_go(live: &mut AffixLive, wait: Duration) {
        let c = live.click.as_mut().expect("clicks");
        c.at -= RELEASED_AFTER;
        c.first -= RELEASED_AFTER + wait;
        if let Some(at) = &mut c.reset_at {
            *at -= RELEASED_AFTER + wait;
        }
    }

    /// The page coming in: quick frames, the bright map never read, the panel's first frame read ahead and confirmed
    /// by the next one alike (no second read), then no more quick frames; later a changed panel is read at once and
    /// once more at rest, as before.
    #[test]
    fn the_page_coming_in_is_read_once_at_rest() {
        let mut live = shown();
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY));
        assert_eq!([feed(&mut live, 200), feed(&mut live, 200), feed(&mut live, 200)], [0, 0, 0], "the map: not read");
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY));
        assert_eq!(feed(&mut live, 10), 1, "the panel coming in: read ahead");
        assert_eq!(feed(&mut live, 10), 1, "the panel at rest: the read ahead stands, no second read");
        assert!(live.quick.is_none());
        let _ = live.wants_shot_in();
        assert_eq!(feed(&mut live, 10), 1);
        assert_eq!(live.wants_shot_in(), None, "no more quick frames");
        assert_eq!([feed(&mut live, 40), feed(&mut live, 40), feed(&mut live, 40)], [2, 3, 3], "another panel later");
    }

    /// A read ahead of the page coming in is shown once the next frame is alike: held when it comes back first, shown
    /// at once when it comes back after; a next frame not alike drops it (its read never shows) and reads that one
    /// ahead instead.
    #[test]
    fn a_read_ahead_shows_only_once_the_panel_is_at_rest() {
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        // back before the confirming frame: held, then shown
        let mut live = shown();
        assert_eq!(feed(&mut live, 10), 1);
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert!(live.boxes.is_empty() && live.held.is_some(), "held until the next frame tells");
        assert_eq!(feed(&mut live, 10), 1, "alike: no second read");
        assert_eq!(live.boxes.len(), 1, "shown");
        assert!(live.quick.is_none() && live.ahead.is_none() && live.held.is_none());
        // back after it: shown at once
        let mut live = shown();
        feed(&mut live, 10);
        feed(&mut live, 10);
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert_eq!(live.boxes.len(), 1);
        // the next frame not alike (still coming in): dropped, that frame read ahead
        let mut live = shown();
        feed(&mut live, 10);
        let early = done(&live, vec![entry("Rea", false, Lookup::Unknown)]);
        assert_eq!(feed(&mut live, 40), 2, "not alike: this one read ahead");
        live.take(early);
        assert!(live.boxes.is_empty() && live.held.is_none(), "the dropped read never shows");
        assert_eq!(feed(&mut live, 40), 2, "alike: confirmed");
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert_eq!(live.boxes.len(), 1);
        // a click meanwhile drops it too
        let mut live = shown();
        feed(&mut live, 10);
        let early = done(&live, vec![entry("Read whole", false, found)]);
        live.note(Signal::AffixButton { revert: false });
        live.take(early);
        assert!(live.boxes.is_empty() && live.ahead.is_none());
    }

    /// The quick frames stop QUICK_FOR after they started: a panel never at rest is then read as frames come.
    #[test]
    fn the_quick_frames_stop_after_a_while() {
        let mut live = shown();
        live.quick = Some(Instant::now() - QUICK_FOR);
        assert_eq!(feed(&mut live, 10), 1);
        assert!(live.quick.is_none());
    }

    /// An incomplete read (no entry, or a whole entry not found) of a panel at rest asks for frames after 250 ms,
    /// 500 ms, then 1 s each, at most RETRY_READS times, and the next frame is read though alike; a complete read asks
    /// for none; a cut entry not found does not count; another panel starts over.
    #[test]
    fn an_incomplete_read_is_read_again_a_few_times() {
        let mut live = shown();
        feed(&mut live, 0);
        assert_eq!(feed(&mut live, 0), 1, "the panel at rest");
        let _ = live.wants_shot_in();
        let mut waits = Vec::new();
        for _ in 0..8 {
            live.take(done(&live, vec![]));
            waits.push(live.wants_shot_in().map(|d| d.as_millis()));
            feed(&mut live, 0);
        }
        assert_eq!(waits, [Some(250), Some(500), Some(1000), Some(1000), Some(1000), Some(1000), None, None]);
        assert_eq!(live.sent, 1 + 6, "each read again is of the next frame, alike as it is");
        // another panel: read, and its own reads again (a line read badly: short)
        feed(&mut live, 40);
        let unknown = entry("Xy", false, Lookup::Unknown);
        live.take(done(&live, vec![unknown]));
        assert_eq!(live.wants_shot_in(), Some(Duration::from_millis(250)));
        // complete (a cut entry not found does not count): no read again, and the next frame alike is read only to
        // see the panel at rest
        let found = Lookup::Found { index: 0, grade: Grade::Danger, exact: true };
        feed(&mut live, 40);
        let sent = live.sent;
        live.take(done(&live, vec![entry("Read whole", false, found), entry("Cut at the bott", true, Lookup::Unknown)]));
        assert_eq!(live.wants_shot_in(), None);
        assert_eq!(feed(&mut live, 40), sent);
    }

    /// Every entry of a read is on the map page's list, top to bottom: found ones by their sentence, the others with
    /// their text, whether they read like affixes of different grades, and whether the panel's bottom cuts them.
    #[test]
    fn unknown_entries_are_seen_too() {
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let found = Lookup::Found { index: 7, grade: Grade::Warn, exact: true };
        let entries = vec![entry("Known", false, found), entry("Not in the table", false, Lookup::Unknown), entry("Like two", false, Lookup::Ambiguous), entry("Cut at the", true, Lookup::Unknown)];
        live.take(done(&live, entries));
        assert_eq!(
            live.seen(),
            [
                Seen::Known(7),
                Seen::Unknown { text: "Not in the table".into(), ambiguous: false, cut: false, game: Grade::None },
                Seen::Unknown { text: "Like two".into(), ambiguous: true, cut: false, game: Grade::None },
                Seen::Unknown { text: "Cut at the".into(), ambiguous: false, cut: true, game: Grade::None },
            ]
        );
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Hidden });
        assert!(live.seen().is_empty(), "no page: nothing on it");
    }

    /// A changed table (a grade picked, an affix learned) drops the reads still on the way, made with the old table, and
    /// the matches of the old one; the sentences seen on the map stay through a grade picked (their places in the table
    /// do not move; the affix page's list of the map would jump), not through an affix learned. The same table changes
    /// nothing.
    #[test]
    fn a_new_table_drops_reads_on_the_way() {
        let mut live = AffixLive::new(|| {});
        live.sent = 5;
        live.memory.push(("x".into(), Lookup::Unknown));
        live.seen = vec![Seen::Known(3)];
        live.configure(true, FrameStyle::Outline, &Grades::new(), &[]);
        assert_eq!((live.shown_seq, live.memory.len(), live.seen.len()), (0, 1, 1), "the same table");
        let (first, was) = live.dict.lock().map(|d| (d.sentences()[0].key().to_string(), d.sentences()[0].grade)).unwrap();
        let other = if was == Grade::Warn { Grade::Danger } else { Grade::Warn };
        live.configure(true, FrameStyle::Outline, &Grades::from([(first.clone(), other)]), &[]);
        assert_eq!((live.shown_seq, live.memory.len(), live.seen.len()), (5, 0, 1), "a grade picked: the sentences seen stay");
        // an affix learned: the table's sentences change, and the indices seen with them
        live.sent = 6;
        let learned = [Learned { id: "5888888".into(), en: "#% a new affix".into(), zh: String::new() }];
        live.configure(true, FrameStyle::Outline, &Grades::from([(first, other)]), &learned);
        assert_eq!((live.shown_seq, live.seen.len()), (6, 0), "an affix learned: the sentences seen go");
    }

    /// A click takes the frames down at once and drops the reads on the way (of the panel before); frames while the
    /// button is held are not read; once let go, the panel at rest is read when it differs from the one before. Reset
    /// forgets the entries remembered whole (the affix reset away may be one of them), Add keeps them.
    #[test]
    fn a_click_takes_the_frames_down_until_the_panel_rests() {
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert_eq!((live.memory.len(), live.boxes.len()), (1, 1), "the read was taken");
        // a frame of another panel sent before the click, still being read
        assert_eq!(feed(&mut live, 9), 2);
        let before_click = done(&live, vec![entry("Read whole", false, found)]);
        live.note(Signal::AffixButton { revert: false });
        assert!(live.boxes.is_empty(), "frames down at the click");
        assert_eq!(live.memory.len(), 1, "Add keeps the visit's memory");
        assert_eq!(live.wants_shot_in(), Some(RELEASED_AFTER), "a frame once the button is let go");
        live.take(before_click);
        assert!(live.boxes.is_empty() && live.shown_seq == live.sent, "the read of the panel before the click is dropped");
        assert_eq!(feed(&mut live, 9), 2, "the button still held: not read");
        let_go(&mut live, Duration::ZERO);
        assert_eq!(feed(&mut live, 9), 3, "let go, at rest, not the panel before: read");
        live.note(Signal::AffixButton { revert: true });
        assert!(live.memory.is_empty(), "Reset forgets the entries remembered whole");
    }

    /// Clicks that changed nothing (the button out of uses): once let go and the panel at rest is the one before, its
    /// frames go back up without a read, however many clicks there were.
    #[test]
    fn clicks_that_change_nothing_put_the_frames_back() {
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        let boxes = live.boxes.clone();
        for _ in 0..26 {
            live.note(Signal::AffixButton { revert: false });
        }
        // held down for 26 clicks: let go well after ADD_WAIT from the first
        let_go(&mut live, ADD_WAIT);
        assert_eq!(feed(&mut live, 0), 1, "not at rest yet");
        assert_eq!(feed(&mut live, 0), 1, "at rest and the panel before: no read");
        assert_eq!(live.boxes, boxes, "the frames from before are back");
        assert!(live.click.is_none() && live.quick.is_none());
        let notes = live.take_notes();
        assert_eq!(notes.iter().filter(|n| matches!(n, Note::FramesDown { .. })).count(), 26);
        assert!(matches!(notes.last(), Some(Note::Restored { boxes: 1, .. })), "{notes:?}");
    }

    /// The frames put back after clicks bring back their read's time and words too: a read of another panel landing
    /// among the clicks would else have the layout check take the panel for that one (its words) read a moment ago.
    #[test]
    fn frames_put_back_bring_back_their_read() {
        let mut live = read_one();
        let (at, text) = (live.last_read_at, live.last_read_text.clone());
        assert!(at.is_some() && text.is_some());
        live.note(Signal::AffixButton { revert: false });
        // a read of another panel, sent before the click, lands among the clicks
        (live.last_read_at, live.last_read_text) = (Some(Instant::now()), Some(vec![9; 4]));
        let_go(&mut live, ADD_WAIT);
        feed(&mut live, 0);
        feed(&mut live, 0);
        assert!(matches!(live.take_notes().last(), Some(Note::Restored { .. })));
        assert_eq!((live.last_read_at, live.last_read_text), (at, text), "the read of the frames put back");
    }

    /// After a Reset the old panel shows a while: a panel still the same is waited on up to RESET_WAIT, the new panel
    /// read as soon as it is at rest; still the same after RESET_WAIT, the frames from before go back up.
    #[test]
    fn a_reset_waits_for_the_new_panel() {
        let read_panel = || {
            let mut live = shown();
            feed(&mut live, 0);
            feed(&mut live, 0);
            let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
            live.take(done(&live, vec![entry("Read whole", false, found)]));
            live.note(Signal::AffixButton { revert: true });
            let_go(&mut live, Duration::ZERO);
            live
        };
        let mut live = read_panel();
        feed(&mut live, 0);
        assert_eq!(feed(&mut live, 0), 1, "the old panel: waited on");
        assert!(live.click.is_some() && live.boxes.is_empty());
        assert_eq!([feed(&mut live, 40), feed(&mut live, 40)], [1, 2], "the new panel, read once at rest");
        let mut live = read_panel();
        let_go(&mut live, RESET_WAIT);
        feed(&mut live, 0);
        assert_eq!(feed(&mut live, 0), 1, "still the same after RESET_WAIT");
        assert_eq!(live.boxes.len(), 1, "the frames from before are back");
    }

    /// The notes say where the time went: a read's frame, counts and the time since the page showed; a click's frames
    /// down; the first read after the clicks with the time since the last click.
    #[test]
    fn notes_say_where_the_time_went() {
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![entry("Read whole", false, found), entry("Not known", false, Lookup::Unknown)]));
        let notes = live.take_notes();
        let [Note::Setup { enabled: true, style: FrameStyle::Outline }, Note::Read { after, at_rest, entries, found, unknown, boxes, since_open_ms, since_click_ms, .. }] = notes.as_slice() else { panic!("{notes:?}") };
        assert_eq!((*after, *at_rest, *entries, *found, *unknown, *boxes), ("open", true, 2, 1, 1, 1));
        // each entry as read: its text and what it matched (the sentence's first id)
        let Note::Read { items, scale, .. } = &notes[1] else { unreachable!() };
        let id = live.dict.lock().unwrap().sentences()[7].ids[0].clone();
        assert_eq!(items.iter().map(|e| (e.text.as_str(), e.found.clone())).collect::<Vec<_>>(), [("Read whole", Some((id, Grade::Danger, true))), ("Not known", None)]);
        assert_eq!(*scale, Some(true));
        assert!(since_open_ms.is_some() && since_click_ms.is_none());
        live.note(Signal::AffixButton { revert: false });
        let_go(&mut live, Duration::ZERO);
        feed(&mut live, 40);
        feed(&mut live, 40);
        live.take(done(&live, vec![]));
        let notes = live.take_notes();
        assert!(matches!(notes.first(), Some(Note::FramesDown { reset: false, up: true })), "{notes:?}");
        assert!(matches!(notes.last(), Some(Note::Read { since_click_ms: Some(_), since_open_ms: None, .. })), "{notes:?}");
    }

    /// A panel read (grey 0, one frame up) on a page just shown.
    fn read_one() -> AffixLive {
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert_eq!(live.boxes.len(), 1);
        live
    }

    /// The switch turned off and on again with the page showing: the frames come back with no click, and the page is
    /// not stale while the frames come once a second.
    #[test]
    fn the_switch_on_again_reads_the_page_showing() {
        let mut live = read_one();
        live.configure(false, FrameStyle::Outline, &Grades::new(), &[]);
        assert!(live.boxes.is_empty());
        // off a while: no frames came
        live.last_frame = Some(Instant::now() - STALE * 2);
        let _ = live.wants_shot_in();
        live.configure(true, FrameStyle::Outline, &Grades::new(), &[]);
        assert!(live.active(), "not stale");
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY), "a frame asked at once");
        let sent = live.sent;
        feed(&mut live, 0);
        feed(&mut live, 0);
        assert!(live.sent > sent, "the panel read again");
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert_eq!(live.boxes.len(), 1, "framed with no click");
        // the once-a-second frames keep it from going stale
        for _ in 0..4 {
            live.last_frame = live.last_frame.map(|t| t - Duration::from_secs(1));
            feed(&mut live, 0);
            live.poll(None);
        }
        assert_eq!(live.boxes.len(), 1, "still framed after 4 s of frames");
    }

    /// The game leaving the front or coming back while a page shows is noted once each time, with the frames there
    /// were; the user's grade changes are noted (none for the first settings, which only set them).
    #[test]
    fn the_front_and_grade_changes_are_noted() {
        let mut live = read_one();
        let _ = live.take_notes();
        // frames keep coming meanwhile (a slow test run would else find the page stale and take the frames down)
        for game in [Some([0, 0, 160, 90]), Some([0, 0, 160, 90]), None, None, Some([0, 0, 160, 90])] {
            live.last_frame = Some(Instant::now());
            live.poll(game);
        }
        let fronts: Vec<Note> = live.take_notes().into_iter().filter(|n| matches!(n, Note::Front { .. })).collect();
        assert!(live.frames_up(), "in front with frames: looked at twice a second");
        live.last_frame = Some(Instant::now());
        live.poll(None);
        assert!(!live.frames_up(), "the game behind another window: frames hidden, looked at once a second");
        live.last_frame = Some(Instant::now());
        live.poll(Some([0, 0, 160, 90]));
        assert!(live.frames_up());
        let _ = live.take_notes();
        assert_eq!(fronts, [Note::Front { front: true, boxes: 1 }, Note::Front { front: false, boxes: 1 }, Note::Front { front: true, boxes: 1 }]);
        // the first settings set the grades: nothing noted
        live.configure(true, FrameStyle::Outline, &Grades::from([("5111001".to_string(), Grade::Warn)]), &[]);
        assert!(!live.take_notes().iter().any(|n| matches!(n, Note::Grade { .. })));
        live.configure(true, FrameStyle::Outline, &Grades::from([("5111001".to_string(), Grade::None), ("5212010".to_string(), Grade::Danger)]), &[]);
        live.configure(true, FrameStyle::Outline, &Grades::from([("5212010".to_string(), Grade::Danger)]), &[]);
        let grades: Vec<Note> = live.take_notes().into_iter().filter(|n| matches!(n, Note::Grade { .. })).collect();
        // and into the grades file, samples taken or not
        let file = std::env::temp_dir().join(format!("affix-grades-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&file);
        live.grades_file = Some(file.clone());
        live.configure(true, FrameStyle::Outline, &Grades::from([("5212010".to_string(), Grade::Warn)]), &[]);
        let written = std::fs::read_to_string(&file).expect("the grades file");
        assert!(written.contains("\"kind\":\"affix_grade\"") && written.contains("\"id\":\"5212010\"") && written.contains("\"from\":\"Danger\"") && written.contains("\"to\":\"Warn\""), "{written}");
        let _ = std::fs::remove_file(&file);
        let _ = live.take_notes();
        assert_eq!(
            grades,
            [
                Note::Grade { id: "5111001".into(), from: Some(Grade::Warn), to: Some(Grade::None) },
                Note::Grade { id: "5212010".into(), from: None, to: Some(Grade::Danger) },
                Note::Grade { id: "5111001".into(), from: Some(Grade::None), to: None },
            ]
        );
    }

    /// A scroll: the bullets moving together by a few rows tell it, with entries scrolled in or out at the edges; bullets
    /// that did not move, moved apart (a popup over some), or one alone of many do not.
    #[test]
    fn a_scroll_is_told_by_the_bullets_moving_together() {
        let mut live = shown();
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.last_read = Panel { entries: [500, 560, 640, 700].iter().map(|&t| Entry { top: t, bottom: t + 50, ..entry("an entry", false, found) }).collect(), title: true };
        assert_eq!(live.scroll_shift(&[500, 560, 640, 700], 1440), None, "not moved");
        assert_eq!(live.scroll_shift(&[503, 558, 641, 704], 1440), None, "moved within the slack");
        assert_eq!(live.scroll_shift(&[470, 530, 610, 670], 1440), Some(-30), "up by 30");
        assert_eq!(live.scroll_shift(&[564, 624, 704, 784], 1440), Some(64), "down by 64, one scrolled in at the top");
        assert_eq!(live.scroll_shift(&[436, 496, 576, 636, 720], 1440), Some(-64), "up by 64, one scrolled in at the bottom");
        assert_eq!(live.scroll_shift(&[500, 560], 1440), None, "two hidden by a popup: not moved");
        assert_eq!(live.scroll_shift(&[520, 640, 900], 1440), None, "not together");
        assert_eq!(live.scroll_shift(&[], 1440), None, "no bullet");
        // 1080 rows: the slack scales (the read's entries at 1080 rows too)
        let mut small = shown();
        small.last_read = Panel { entries: [375, 420, 480].iter().map(|&t| Entry { top: t, bottom: t + 38, ..entry("an entry", false, found) }).collect(), title: true };
        assert_eq!(small.scroll_shift(&[353, 398, 458], 1080), Some(-22));
        assert_eq!(small.scroll_shift(&[380, 416, 484], 1080), None, "within 6.75 rows");
        let mut one = shown();
        one.last_read = Panel { entries: vec![Entry { top: 500, bottom: 550, ..entry("an entry", false, found) }], title: true };
        assert_eq!(one.scroll_shift(&[470], 1440), Some(-30), "a panel of one");
    }

    /// A dark frame with a bright bar across the text area at its row `row` (read as one bullet there) and
    /// a line of text by `grey` (as [`plain`]): the panel's one entry, scrolled when the bar moves.
    fn barred(grey: u8, row: usize) -> Rgba {
        let mut f = plain(grey);
        let area = panel::text_area(160, 90).expect("16:9");
        for x in area.x as usize..(area.x + area.w) as usize {
            let i = ((area.y as usize + row) * 160 + x) * 4;
            f.data[i..i + 3].copy_from_slice(&[230, 230, 230]);
        }
        f
    }

    /// The user scrolls the panel (no click, nothing in the log): the first frame with its bullet moved takes the frames
    /// down at once, the quick frames follow the panel, and it is read once at rest; a frame with the background moved
    /// alone (the bullet where it was) is not a scroll.
    #[test]
    fn a_scroll_takes_the_frames_down_until_the_panel_rests() {
        let mut live = shown();
        let first = barred(10, 12);
        let top = crate::affix::read::entry_tops(&first)[0];
        live.frame(&first, "open", SystemTime::now());
        live.frame(&first, "open", SystemTime::now());
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![Entry { top, bottom: top + 8, ..entry("Read whole", false, found) }]));
        assert_eq!(live.boxes.len(), 1);
        let _ = live.take_notes();
        let _ = live.wants_shot_in();
        // the background behind the panel moves, the bullet stays: no scroll
        live.frame(&barred(12, 12), "refresh", SystemTime::now());
        assert!(!live.take_notes().iter().any(|n| matches!(n, Note::Scrolled { .. })), "the background moved, not the panel");
        // the panel scrolls up: its bullet two text area rows higher; the frame is read ahead
        let sent = live.sent;
        live.frame(&barred(14, 10), "refresh", SystemTime::now());
        assert!(live.boxes.is_empty(), "frames down at once");
        assert!(matches!(live.take_notes().as_slice(), [Note::Scrolled { dy, up: true }] if *dy < 0), "a scroll up noted");
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY), "quick frames follow the panel");
        assert_eq!(live.sent, sent + 1, "read ahead");
        // its read comes back while the panel still moves: held, then dropped, nothing framed
        live.take(done(&live, vec![Entry { top, bottom: top + 8, ..entry("Read whole", false, found) }]));
        assert!(live.boxes.is_empty(), "held until the next frame tells");
        live.frame(&barred(16, 9), "retry", SystemTime::now());
        assert!(live.boxes.is_empty() && live.ahead.is_none(), "still moving: the read ahead dropped");
        assert_eq!(live.sent, sent + 1, "not read mid-scroll");
        live.frame(&barred(16, 9), "retry", SystemTime::now());
        assert_eq!(live.sent, sent + 2, "read once at rest");
        // a scroll that stops at once: the read ahead confirmed by the next frame alike, no second read
        live.take(done(&live, vec![Entry { top: crate::affix::read::entry_tops(&barred(16, 9))[0], bottom: top, ..entry("Read whole", false, found) }]));
        assert_eq!(live.boxes.len(), 1);
        let _ = live.take_notes();
        let sent = live.sent;
        live.frame(&barred(18, 12), "refresh", SystemTime::now());
        assert!(live.boxes.is_empty() && live.sent == sent + 1, "frames down, read ahead");
        live.take(done(&live, vec![Entry { top, bottom: top + 8, ..entry("Read whole", false, found) }]));
        live.frame(&barred(18, 12), "retry", SystemTime::now());
        assert_eq!((live.boxes.len(), live.sent), (1, sent + 1), "at rest at once: the read ahead shown, no second read");
    }

    /// A panel of another interface scale is read as no entries: the frames go and the page is told, until a panel of
    /// the measured scale is read; a read with no bullet tells nothing.
    #[test]
    fn a_panel_of_another_interface_scale_is_not_framed() {
        let mut live = read_one();
        assert!(!live.odd_scale());
        feed(&mut live, 40);
        feed(&mut live, 40);
        let mut odd = done(&live, Vec::new());
        odd.scale = Some(false);
        live.take(odd);
        assert!(live.odd_scale() && live.boxes.is_empty(), "another scale: no frames, the page told");
        let mut none = done(&live, Vec::new());
        none.scale = None;
        live.take(none);
        assert!(live.odd_scale(), "no bullet: tells nothing");
        // a read of a page left meanwhile tells nothing either
        let mut stale = done(&live, Vec::new());
        stale.scale = Some(true);
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Hidden });
        live.take(stale);
        assert!(!live.odd_scale(), "the page left: no hint");
        live.odd_scale = true;
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Shown });
        feed(&mut live, 0);
        feed(&mut live, 0);
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        live.take(done(&live, vec![entry("Read whole", false, found)]));
        assert!(!live.odd_scale() && live.boxes.len() == 1, "the measured scale again: framed");
    }

    /// The map page closed: its last read stays on the affix page. The game log's affixes of the map entered replace
    /// it: the read kept when it found the same (no difference noted), the log's when it did not (noted, the ids not
    /// read and the sentences read but not named); an id the table lacks shows as such. The hideout drops the list, a
    /// level without panel affixes keeps it, and a page shown takes over.
    #[test]
    fn the_map_entered_keeps_its_affixes_on_the_page() {
        let (a, b) = {
            let d = Dict::builtin();
            let ids = |i: usize| d.sentences()[i].ids[0].parse::<u32>().unwrap();
            let pick: Vec<usize> = (0..d.sentences().len()).filter(|&i| d.sentences()[i].ids[0].parse::<u32>().is_ok()).take(2).collect();
            ((pick[0], ids(pick[0])), (pick[1], ids(pick[1])))
        };
        let read_and_close = |entries: Vec<Seen>| {
            let mut live = shown();
            live.seen = entries;
            live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Closed });
            live
        };
        let mut live = read_and_close(vec![Seen::Known(a.0), Seen::Known(b.0)]);
        assert_eq!(live.seen(), [Seen::Known(a.0), Seen::Known(b.0)], "the last read, the page closed");
        assert!(matches!(live.hints().map, Some(super::super::affix_page::MapTitle::This { from_log: false })));
        let _ = live.take_notes();
        live.note(Signal::MapAffixes(MapAffixes::of(Some(5308), &[b.1, a.1])));
        assert_eq!(live.seen(), [Seen::Known(a.0), Seen::Known(b.0)], "the same affixes: the read's list, its order");
        assert!(matches!(live.take_notes().as_slice(), [Note::LogCheck { read: 2, missing, extra, .. }] if missing.is_empty() && extra.is_empty()));
        // read one wrong, the log names another and one the table lacks
        let mut live = read_and_close(vec![Seen::Known(a.0), Seen::Unknown { text: "x".into(), ambiguous: false, cut: false, game: Grade::None }]);
        let _ = live.take_notes();
        live.note(Signal::MapAffixes(MapAffixes::of(Some(5308), &[b.1, 5_655_555])));
        assert_eq!(live.seen(), [Seen::Known(b.0), Seen::Logged(5_655_555)], "the log's list");
        assert!(matches!(live.hints().map, Some(super::super::affix_page::MapTitle::This { from_log: true })));
        let notes = live.take_notes();
        let [Note::LogCheck { missing, extra, .. }] = notes.as_slice() else { panic!("{notes:?}") };
        assert_eq!(missing, &[b.1, 5_655_555]);
        assert_eq!(extra.len(), 1, "the read's sentence the log does not name");
        // a level of other content keeps it; the hideout drops it
        live.note(Signal::MapAffixes(MapAffixes::of(Some(235_501), &[])));
        assert_eq!(live.seen().len(), 2);
        live.note(Signal::MapAffixes(MapAffixes::of(Some(110), &[a.1, b.1])));
        assert!(live.seen().is_empty() && live.hints().map.is_none(), "the hideout, whatever ids its block names");
        // nothing read before the page closed: the log's list
        let mut live = read_and_close(Vec::new());
        live.note(Signal::MapAffixes(MapAffixes::of(Some(5308), &[a.1])));
        assert_eq!(live.seen(), [Seen::Known(a.0)]);
        // a page shown takes over
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Shown });
        assert!(live.hints().map.is_none());
    }

    /// The game log's interface scale checks the one measured only when it came after the scale last changed: one
    /// written before (the slider moved in the hideout since the last level) does not stop the frames; a newer one that
    /// disagrees does (not framed, noted, the page told), and one that agrees does not.
    #[test]
    fn the_logs_interface_scale_checks_only_when_newer() {
        let place = Placement { s: 1.258, left: 148.0, top: 297.0 };
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        let read = |live: &mut AffixLive| {
            live.sent += 1;
            let d = Done { place: Some(place), ..done(live, vec![entry("Read whole", false, found)]) };
            live.take(d);
        };
        let mut live = read_one();
        // the log's last scale: the user's own, from before the slider moved
        live.note(Signal::UiScale { micro: 667_000 });
        read(&mut live);
        assert_eq!((live.boxes.len(), live.odd_scale()), (1, false), "an older log scale: framed at the scale measured");
        assert_eq!(live.place, Some(place));
        // a level entered since: the log's scale is newer than the change, and agrees
        live.note(Signal::UiScale { micro: 839_000 });
        let _ = live.take_notes();
        read(&mut live);
        assert_eq!(live.boxes.len(), 1, "a newer log scale that agrees");
        // a newer one that disagrees
        live.note(Signal::UiScale { micro: 1_000_000 });
        read(&mut live);
        assert!(live.boxes.is_empty() && live.odd_scale(), "a newer log scale that disagrees: not framed, the page told");
        assert!(live.take_notes().iter().any(|n| matches!(n, Note::ScaleMismatch { .. })));
    }

    /// Words of the other client language than the one set are said on the affix page from the second read that
    /// shows them (one could be a panel fading in); a read whose words tell nothing keeps the hint, one in the set
    /// language clears it, and so does the page going.
    #[test]
    fn words_of_the_other_client_language_are_told() {
        use crate::lang::Lang;
        let mut live = read_one();
        feed(&mut live, 40);
        feed(&mut live, 40);
        let run_together = "Rare monsters inflict a stackofSlow effect ontargetswithin 8 m ofthem for3severy2s.";
        // each read a newer one than the last taken (an older one is dropped)
        let read = |live: &mut AffixLive, text: &str, read_in: Lang| {
            live.sent += 1;
            Done { read_in, ..done(live, vec![entry(text, false, Lookup::Unknown)]) }
        };
        let r = read(&mut live, run_together, Lang::ZhTw);
        live.take(r);
        assert_eq!(live.client_hint(), None, "one read: not yet");
        let r = read(&mut live, "+30%", Lang::ZhTw);
        live.take(r);
        assert_eq!(live.client_hint(), None, "words that tell nothing");
        let r = read(&mut live, run_together, Lang::ZhTw);
        live.take(r);
        assert_eq!(live.client_hint(), Some(Lang::En), "the second read in a row that tells English");
        let r = read(&mut live, "+30%", Lang::ZhTw);
        live.take(r);
        assert_eq!(live.client_hint(), Some(Lang::En), "words that tell nothing keep it");
        let r = read(&mut live, "關 卡 內 新 增 1 組 攻 擊 加 速 的 災 燼 觸 槍", Lang::ZhTw);
        live.take(r);
        assert_eq!(live.client_hint(), None, "Chinese read in Chinese: as set");
        let r = read(&mut live, "", Lang::En);
        live.take(r);
        let r = read(&mut live, "", Lang::En);
        live.take(r);
        assert_eq!(live.client_hint(), Some(Lang::ZhTw), "no English words on the panel, twice");
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Closed });
        assert_eq!(live.client_hint(), None, "the page went");
    }

    /// A single Add: the panel still the same at rest soon after is waited on (the game may not have drawn the added
    /// affix yet), and read once it changes; still the same ADD_WAIT after the click, the frames from before go back up.
    #[test]
    fn a_single_add_waits_for_the_game_to_draw() {
        let mut live = read_one();
        live.note(Signal::AffixButton { revert: false });
        let_go(&mut live, Duration::ZERO);
        let _ = live.wants_shot_in();
        assert_eq!([feed(&mut live, 0), feed(&mut live, 0)], [1, 1], "the panel before, too soon: waited on");
        assert!(live.click.is_some() && live.boxes.is_empty(), "no frames put back");
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY));
        assert_eq!([feed(&mut live, 40), feed(&mut live, 40)], [1, 2], "the panel with the new affix, read at rest");
        let mut live = read_one();
        live.note(Signal::AffixButton { revert: false });
        let_go(&mut live, ADD_WAIT);
        assert_eq!([feed(&mut live, 0), feed(&mut live, 0)], [1, 1]);
        assert_eq!(live.boxes.len(), 1, "still the same after ADD_WAIT: the frames from before are back");
    }

    /// After a Reset, the old panel with the same words but the background behind it moved (its grey print another) is
    /// still waited on: told by its text print (2026-11-09, visit 15-49-33 of 11-08: the old panel read as the new one
    /// and framed for 0.2 s); a panel with other words is read.
    #[test]
    fn a_reset_waits_for_new_words_not_a_new_background() {
        let mut live = read_one();
        live.note(Signal::AffixButton { revert: true });
        let_go(&mut live, Duration::ZERO);
        // the panel before (its words at the row of grey 0) over a brighter background: other grey print, same words
        let mut moved = plain(0);
        let area = panel::text_area(160, 90).unwrap();
        for y in (area.y as usize + 10)..(area.y + area.h) as usize {
            for x in 0..160 {
                let i = (y * 160 + x) * 4;
                moved.data[i..i + 3].copy_from_slice(&[20, 20, 20]);
            }
        }
        assert!(!panel::same(&panel::print(&plain(0)), &panel::print(&moved)), "the grey print sees the background");
        live.frame(&moved, "revert", SystemTime::now());
        live.frame(&moved, "revert", SystemTime::now());
        assert_eq!(live.sent, 1, "the old words: waited on, not read");
        assert!(live.click.is_some() && live.boxes.is_empty());
        assert_eq!([feed(&mut live, 40), feed(&mut live, 40)], [1, 2], "new words: read at rest");
    }

    /// An Add then a Reset among the same clicks: the panel on show at the Reset (the one after the Add) is waited out
    /// as the one before the clicks is, RESET_WAIT from the Reset (not from the Add), and the new panel read.
    #[test]
    fn a_reset_after_an_add_waits_from_the_reset() {
        let mut live = read_one();
        live.note(Signal::AffixButton { revert: false });
        assert_eq!(feed(&mut live, 40), 1, "the panel after the Add, the button still held: not read");
        live.note(Signal::AffixButton { revert: true });
        // the Add came long before, the Reset just now
        live.click.as_mut().unwrap().first -= RESET_WAIT * 2;
        let_go(&mut live, Duration::ZERO);
        assert_eq!([feed(&mut live, 40), feed(&mut live, 40)], [1, 1], "the panel the Reset replaces: waited on");
        assert!(live.click.is_some());
        assert_eq!([feed(&mut live, 20), feed(&mut live, 20)], [1, 2], "the new panel, read at rest");
    }

    /// The print kept for putting frames back goes with the frames: a click, the page hidden, frames stopping, a new
    /// table, or a read whose frame is no longer known (too many sent since) leave none, so clicks after them read
    /// the panel instead of putting back frames of another panel.
    #[test]
    fn the_print_on_show_goes_with_the_frames() {
        let mut live = read_one();
        assert!(live.shown_print.is_some());
        live.note(Signal::AffixButton { revert: false });
        assert!(live.shown_print.is_none() && live.click.as_ref().is_some_and(|c| c.before.is_some()));
        // the panel after the Add sent to be read; another Add before that read comes back
        let_go(&mut live, ADD_WAIT);
        assert_eq!([feed(&mut live, 40), feed(&mut live, 40)], [1, 2]);
        live.note(Signal::AffixButton { revert: false });
        assert!(live.click.as_ref().is_some_and(|c| c.before.is_none()), "no frames up, none to put back");
        // the panel goes back to the one before the first Add: read, not frames put back
        let_go(&mut live, ADD_WAIT);
        assert_eq!([feed(&mut live, 0), feed(&mut live, 0)], [2, 3]);
        assert!(!live.take_notes().iter().any(|n| matches!(n, Note::Restored { .. })));
        // a read whose frame is no longer known
        let mut live = read_one();
        feed(&mut live, 40);
        feed(&mut live, 40);
        live.in_flight.clear();
        live.take(done(&live, vec![]));
        assert!(live.shown_print.is_none());
        // hidden, frames stopping, a new table
        for end in 0..3 {
            let mut live = read_one();
            match end {
                0 => live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Hidden }),
                1 => {
                    live.last_frame = Some(Instant::now() - STALE * 2);
                    live.poll(None);
                }
                _ => {
                    let (first, was) = live.dict.lock().map(|d| (d.sentences()[0].key().to_string(), d.sentences()[0].grade)).unwrap();
                    let other = if was == Grade::Warn { Grade::Danger } else { Grade::Warn };
                    live.configure(true, FrameStyle::Outline, &Grades::from([(first, other)]), &[]);
                }
            }
            assert!(live.shown_print.is_none(), "case {end}");
        }
    }

    /// Quick frames that end with no panel read leave a note: how many frames came, how many were dark, and the
    /// darkest grey seen; quick frames that end with a read leave none.
    #[test]
    fn quick_frames_never_read_are_noted() {
        let mut live = shown();
        feed(&mut live, 200);
        feed(&mut live, 150);
        feed(&mut live, 10);
        live.quick = live.quick.map(|q| q - QUICK_FOR);
        feed(&mut live, 200);
        let notes = live.take_notes();
        let [Note::Setup { .. }, Note::Unread { frames: 3, dark: 1, min_grey: Some(g) }] = notes.as_slice() else { panic!("{notes:?}") };
        assert!((g - panel::grey(&panel::print(&plain(10)))).abs() < 0.5, "{g}");
        let mut live = read_one();
        assert!(live.quick.is_none());
        feed(&mut live, 0);
        assert!(live.take_notes().iter().all(|n| !matches!(n, Note::Unread { .. })));
    }

    /// Frames go when the panel goes with the page still open in the log (a popup in its place): a frame without the
    /// panel asks for the next one soon, a second in a row takes the frames down, and the panel coming back is read
    /// again. One bright frame between dark ones changes nothing.
    #[test]
    fn frames_go_when_the_panel_goes() {
        let mut live = read_one();
        let _ = live.wants_shot_in();
        feed(&mut live, 200);
        assert_eq!(live.boxes.len(), 1, "one frame without the panel: not yet");
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY), "the next frame soon");
        feed(&mut live, 200);
        assert!(live.boxes.is_empty() && live.shown_print.is_none() && live.last_read.entries.is_empty());
        assert!(live.quick.is_some() && live.wants_shot_in() == Some(QUICK_EVERY), "quick frames for its return");
        assert_eq!(feed(&mut live, 0), 1, "the panel back, still coming in: not read");
        assert_eq!(feed(&mut live, 0), 2, "the panel back at rest: read again");
        let mut live = read_one();
        feed(&mut live, 200);
        feed(&mut live, 0);
        assert_eq!(live.boxes.len(), 1);
        assert_eq!(live.gone, 0);
    }

    /// An entry the table does not have, read whole, is not read again (it reads the same every time); a short one, or
    /// one a few edits beyond a sentence's fuzzy limit, may read better and is.
    #[test]
    fn affixes_not_in_the_table_are_not_read_again() {
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let _ = live.wants_shot_in();
        live.take(done(&live, vec![entry("Something never seen in the game, nothing like it at all", false, Lookup::Unknown)]));
        assert_eq!(live.wants_shot_in(), None, "not in the table: no read again");
        let en = live.dict.lock().unwrap().sentences().iter().find(|s| s.en.starts_with("When ")).expect("a sentence").en.clone();
        let near = format!("Qqqq{}", &en[4..]);
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        let _ = live.wants_shot_in();
        live.take(done(&live, vec![entry(&near, false, Lookup::Unknown)]));
        assert_eq!(live.wants_shot_in(), Some(retry_wait(1)), "four edits off a sentence: read again");
    }

    /// Six entries read: the Add button is out of uses, so an Add click leaves the frames up and reads nothing, only
    /// watching the panel a while; a Reset still takes them down, and an Add with fewer entries too. Should the panel
    /// show more bullets than entries read after all, the frames go and the panel is read at rest.
    #[test]
    fn an_add_on_a_full_panel_changes_nothing() {
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        let full = |n: usize| {
            let mut live = shown();
            feed(&mut live, 0);
            feed(&mut live, 0);
            let entries = (0..n).map(|i| Entry { top: i as i32 * 10, bottom: i as i32 * 10 + 10, ..entry(&format!("Entry number {i} read whole"), false, found) }).collect();
            live.take(done(&live, entries));
            let _ = live.take_notes();
            let _ = live.wants_shot_in();
            live
        };
        let mut live = full(MAX_AFFIXES);
        live.note(Signal::AffixButton { revert: false });
        assert_eq!(live.boxes.len(), MAX_AFFIXES, "the frames stay up");
        assert!(live.click.is_none() && live.take_notes().is_empty());
        assert_eq!(live.wants_shot_in(), Some(QUICK_EVERY), "watched");
        // a frame with no bullets found (or as many as read): the frames stay, the watch goes on until it is over
        let sent = feed(&mut live, 0);
        assert_eq!((live.boxes.len(), live.sent), (MAX_AFFIXES, sent));
        // the entries read start at 0, 10, ... 50 on a frame 1440 high
        let same: Vec<i32> = (0..MAX_AFFIXES as i32).map(|i| i * 10).collect();
        // the watch from now on (a slow debug build's frame above may take a good part of FULL_ADD_WATCH)
        live.full_add = Some(Instant::now());
        assert!(!live.full_add_watch(Instant::now(), &same, 1440) && live.full_add.is_some());
        assert!(!live.full_add_watch(Instant::now(), &[], 1440) && live.full_add.is_some(), "no bullet: tells nothing");
        assert!(!live.full_add_watch(Instant::now() + FULL_ADD_WATCH, &same, 1440) && live.full_add.is_none(), "over");
        live.note(Signal::AffixButton { revert: true });
        assert!(live.boxes.is_empty(), "a Reset takes them down");
        // more bullets than entries read: the panel grew after all
        let mut live = full(MAX_AFFIXES);
        live.note(Signal::AffixButton { revert: false });
        let more: Vec<i32> = (0..=MAX_AFFIXES as i32).map(|i| i * 10).collect();
        assert!(live.full_add_watch(Instant::now(), &more, 1440));
        assert!(live.boxes.is_empty() && live.shown_print.is_none() && live.quick.is_some() && live.full_add.is_none());
        // as many bullets, moved down (a new one at the top pushed the last off the panel)
        let mut live = full(MAX_AFFIXES);
        live.note(Signal::AffixButton { revert: false });
        let moved: Vec<i32> = (0..MAX_AFFIXES as i32).map(|i| i * 10 + 25).collect();
        assert!(live.full_add_watch(Instant::now(), &moved, 1440) && live.boxes.is_empty());
        let mut live = full(MAX_AFFIXES - 1);
        live.note(Signal::AffixButton { revert: false });
        assert!(live.boxes.is_empty() && live.click.is_some());
    }

    /// Bullets where the last read's entries are (a few pixels off at most) are its layout unchanged: a print changed by
    /// the background alone is then not read again. A moved, added or missing bullet, or none found, is not.
    #[test]
    fn a_still_layout_is_not_read_again() {
        let found = Lookup::Found { index: 7, grade: Grade::Danger, exact: true };
        let mut live = shown();
        live.last_read = Panel { entries: (0..3).map(|i| Entry { top: 400 + i * 60, bottom: 460 + i * 60, ..entry("an entry", false, found) }).collect(), title: true };
        assert!(live.layout_unchanged(&[400, 460, 520], 1440));
        assert!(live.layout_unchanged(&[403, 458, 521], 1440), "a few pixels off");
        assert!(!live.layout_unchanged(&[400, 460, 540], 1440), "one moved");
        assert!(!live.layout_unchanged(&[340, 400, 460, 520], 1440), "one added");
        assert!(!live.layout_unchanged(&[400, 460], 1440), "one missing");
        assert!(!live.layout_unchanged(&[], 1440), "no bullet found");
        // the frame path: a frame with no bullets found (as in these tests) is read as before
        let mut live = read_one();
        assert_eq!(feed(&mut live, 40), 2);
    }

    /// A read of the same panel (its background moved, a letter read otherwise) keeps the entries found before while
    /// its text stays near that match, so the frames do not come and go, and its reads again go on counting; a text
    /// that moved away from the match (the match was wrong) does not keep it, nor do another panel's entries.
    #[test]
    fn a_panel_read_again_keeps_what_it_found() {
        let (index, en, grade) = {
            let dict = Dict::builtin();
            let (i, s) = dict.sentences().iter().enumerate().find(|(_, s)| s.en.len() > 60 && s.grade != Grade::None).expect("a sentence");
            (i, s.en.clone(), s.grade)
        };
        let read_whole = || {
            let mut live = shown();
            feed(&mut live, 0);
            feed(&mut live, 0);
            live.take(done(&live, vec![entry(&en, false, Lookup::Found { index, grade, exact: true })]));
            assert_eq!(live.boxes.len(), 1);
            live
        };
        // four letters read otherwise: near the match, kept
        let near = format!("Qqqq{}", &en[4..]);
        let mut live = read_whole();
        assert_eq!(feed(&mut live, 40), 2, "the background moved: read");
        live.take(done(&live, vec![entry(&near, false, Lookup::Unknown)]));
        assert_eq!(live.boxes.len(), 1, "found before: still framed");
        assert_eq!(live.seen(), [Seen::Known(index)]);
        assert_eq!(feed(&mut live, 10), 3);
        live.take(done(&live, vec![entry("Another affix that reads nothing alike", false, Lookup::Unknown)]));
        assert!(live.boxes.is_empty(), "another panel: its own reads");
        // ten letters off: still the same panel by its entries, but no longer near the match: not kept
        let far = format!("Qqqqqqqqqq{}", &en[10..]);
        let mut live = read_whole();
        feed(&mut live, 40);
        live.take(done(&live, vec![entry(&far, false, Lookup::Unknown)]));
        assert!(live.boxes.is_empty(), "a match the new read disagrees with is not kept");
        // reads again of one panel are counted across background changes
        let mut live = shown();
        feed(&mut live, 0);
        feed(&mut live, 0);
        live.take(done(&live, vec![entry("Xy", false, Lookup::Unknown)]));
        assert_eq!(live.retries, 1);
        feed(&mut live, 40);
        live.take(done(&live, vec![entry("Xy", false, Lookup::Unknown)]));
        assert_eq!(live.retries, 2, "not started over");
    }

    /// After an Add the frames of the panel before move down past the new entries at the top, to the rows the bullets
    /// give: the old entry j is now entry j + k; the bottom of the last keeps its height. No growth, no frames; and no
    /// frames moved when an old entry's height on the new panel is not its height before (a bullet too many or too
    /// few would put every frame one entry off).
    #[test]
    fn frames_move_down_past_new_entries() {
        let warn = Lookup::Found { index: 1, grade: Grade::Warn, exact: true };
        let danger = Lookup::Found { index: 2, grade: Grade::Danger, exact: true };
        let before = Panel {
            title: true,
            entries: vec![
                Entry { top: 100, bottom: 140, ..entry("first", false, warn) },
                Entry { top: 140, bottom: 200, ..entry("second", false, danger) },
            ],
        };
        let boxes = moved_down(&before, &[60, 100, 140], 1440, None).expect("frames");
        let rows: Vec<(i32, i32, Grade)> = boxes.iter().map(|(r, g)| (r.y, r.y + r.h, *g)).collect();
        assert_eq!(rows, [(100, 140, Grade::Warn), (140, 200, Grade::Danger)]);
        // two new entries, a two-line one at the top
        let boxes = moved_down(&before, &[40, 100, 140, 180], 1440, None).expect("frames");
        assert_eq!(boxes.iter().map(|(r, _)| r.y).collect::<Vec<_>>(), [140, 180]);
        assert!(moved_down(&before, &[100, 140], 1440, None).is_none(), "the panel did not grow");
        assert!(moved_down(&before, &[], 1440, None).is_none());
        // a bullet too many (between the old entries): the first old entry would be 20 high, not 40
        assert!(moved_down(&before, &[60, 100, 120, 140], 1440, None).is_none(), "heights differ: not moved");
    }

    /// Replays saved map page visits through the live frames (2026-11-09, lab check, ignored): each visit's log rows
    /// (`affix_page`, `affix_click`, `affix_sample`) at their own times, the panel of the newest saved frame fed as
    /// the capture would (every QUICK_EVERY), the reads done on the worker for real; prints the frames on show each
    /// time they change, and counts frames that came back within a second of going with no click between (a flicker)
    /// and frames still up after the page closed. Real time: a visit takes as long as it did.
    ///
    /// `ASCENDANT_AFFIX_VISITS`: the samples' affix folder; `ASCENDANT_AFFIX_REPLAY`: visit folders in it, `;`
    /// between; `ASCENDANT_AFFIX_LEARNED` (optional): the learned list; `ASCENDANT_AFFIX_REPLAY_LANG`: en or zh-TW
    /// (default zh-TW). Run with `-- --ignored --nocapture replay_saved_visits`.
    #[test]
    #[ignore]
    fn replay_saved_visits() {
        use std::path::PathBuf;
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
        let root = PathBuf::from(var("ASCENDANT_AFFIX_VISITS"));
        let learned = std::env::var_os("ASCENDANT_AFFIX_LEARNED").and_then(|p| crate::affix::learned::load(&PathBuf::from(p))).unwrap_or_default();
        let zh = std::env::var("ASCENDANT_AFFIX_REPLAY_LANG").map_or(true, |l| l != "en");
        crate::lang::set(if zh { crate::lang::Lang::ZhTw } else { crate::lang::Lang::En });
        let ms_of = |at: &str| -> i64 {
            // "2026-11-08T18:34:07.374Z": the time of day in ms
            let t = &at[11..23];
            let n = |a: usize, b: usize| t[a..b].parse::<i64>().unwrap_or(0);
            ((n(0, 2) * 60 + n(3, 5)) * 60 + n(6, 8)) * 1000 + n(9, 12)
        };
        for visit in var("ASCENDANT_AFFIX_REPLAY").split(';').map(str::trim).filter(|v| !v.is_empty()) {
            let dir = root.join(visit);
            let rows: Vec<serde_json::Value> = std::fs::read_to_string(dir.join("log.jsonl"))
                .expect("a visit's log")
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let t0 = rows.iter().find_map(|r| r["at"].as_str()).map(ms_of).expect("rows");
            // visits sampled since quick frames are kept (`affix_samples` `quick`): those frames at their real times;
            // older visits: the newest saved frame every QUICK_EVERY
            let quick_rows = rows.iter().any(|r| r["kind"] == "affix_quick");
            let paste = |file: &str, area: [i64; 4]| -> Option<Rgba> {
                let crop = crate::reader::load_image_raw(&dir.join(file)).ok()?;
                let (w, h) = (2560usize, 1440usize);
                let mut f = Rgba { w, h, data: vec![0; w * h * 4] };
                for y in 0..crop.h.min(area[3] as usize) {
                    let n = crop.w.min(area[2] as usize);
                    let at = ((area[1] as usize + y) * w + area[0] as usize) * 4;
                    f.data[at..at + n * 4].copy_from_slice(&crop.data[y * crop.w * 4..(y * crop.w + n) * 4]);
                }
                Some(f)
            };
            let mut live = AffixLive::new(|| {});
            live.configure(true, FrameStyle::Outline, &Grades::new(), &learned);
            let start = Instant::now();
            let mut frame: Option<Rgba> = None;
            let (mut shown, mut flickers, mut up_after_close) = (Vec::new(), 0, 0);
            let mut dropped: Option<(Instant, usize)> = None;
            let mut closed = false;
            let mut clicked = false;
            // when the page last showed, and the first frames after it (ms of the replay)
            let (mut shown_at, mut first_frames): (Option<i64>, Vec<i64>) = (None, Vec::new());
            let mut last_feed = start;
            println!("\n== {visit}");
            let end = rows.iter().filter_map(|r| r["at"].as_str()).map(ms_of).max().unwrap_or(t0) - t0 + 1500;
            let mut i = 0;
            while (start.elapsed().as_millis() as i64) < end {
                let now_ms = start.elapsed().as_millis() as i64;
                while i < rows.len() && rows[i]["at"].as_str().map_or(0, ms_of) - t0 <= now_ms {
                    let r = &rows[i];
                    match r["kind"].as_str().unwrap_or("") {
                        "affix_page" => {
                            let state = match r["state"].as_str() {
                                Some("shown") => PageState::Shown,
                                Some("hidden") => PageState::Hidden,
                                _ => PageState::Closed,
                            };
                            closed = state != PageState::Shown;
                            shown_at = (state == PageState::Shown).then_some(now_ms);
                            live.note(Signal::MapPage { page: MapPage::Normal, state });
                            println!("{now_ms:>6} page {state:?}");
                        }
                        "affix_click" => {
                            live.note(Signal::AffixButton { revert: r["button"].as_str() == Some("revert") });
                            // frames a click takes down are no flicker (as the samples' health counts)
                            (dropped, clicked) = (None, true);
                        }
                        "affix_quick" => {
                            let area: Vec<i64> = r["area"].as_array().map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect()).unwrap_or_default();
                            if let (Some(file), [x, y, w, h]) = (r["file"].as_str(), area.as_slice())
                                && let Some(f) = paste(file, [*x, *y, *w, *h])
                            {
                                live.frame(&f, "retry", SystemTime::now());
                                frame = Some(f);
                            }
                        }
                        "affix_sample" => {
                            if let Some(png) = r["lossless"].as_str()
                                && let Ok(crop) = crate::reader::load_image_raw(&dir.join(png))
                            {
                                let (w, h) = (2560usize, 1440usize);
                                let area = panel::area(w, h).expect("16:9");
                                let mut f = Rgba { w, h, data: vec![0; w * h * 4] };
                                for y in 0..crop.h.min(area.h as usize) {
                                    let n = crop.w.min(area.w as usize);
                                    let at = ((area.y as usize + y) * w + area.x as usize) * 4;
                                    f.data[at..at + n * 4].copy_from_slice(&crop.data[y * crop.w * 4..(y * crop.w + n) * 4]);
                                }
                                frame = Some(f);
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
                if !closed
                    && !quick_rows
                    && let Some(f) = &frame
                    && last_feed.elapsed() >= QUICK_EVERY
                {
                    live.frame(f, "retry", SystemTime::now());
                    last_feed = Instant::now();
                }
                live.poll(None);
                let boxes: Vec<(Grade, i32)> = live.boxes.iter().map(|(r, g)| (*g, r.y)).collect();
                if boxes != shown {
                    if !boxes.is_empty()
                        && let Some(at) = shown_at.take()
                    {
                        first_frames.push(now_ms - at);
                    }
                    println!("{now_ms:>6} frames {boxes:?}");
                    if boxes.len() < shown.len() && !std::mem::take(&mut clicked) {
                        dropped = Some((Instant::now(), shown.len()));
                    } else if let Some((at, n)) = dropped
                        && boxes.len() >= n
                    {
                        if at.elapsed() <= Duration::from_secs(1) {
                            flickers += 1;
                        }
                        dropped = None;
                    }
                    if closed && !boxes.is_empty() {
                        up_after_close += 1;
                    }
                    shown = boxes;
                    clicked = false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            println!("{visit}: flickers {flickers}, frames up after the page closed {up_after_close}, page shown to first frames {first_frames:?} ms, frames at the end {shown:?}");
        }
    }

    /// The page's states: shown starts reading, hidden keeps the visit's memory, closed then shown starts a new visit.
    #[test]
    fn page_states() {
        let mut live = AffixLive::new(|| {});
        let shown = Signal::MapPage { page: MapPage::Normal, state: PageState::Shown };
        live.note(shown);
        assert!(live.active());
        live.memory.push(("x".into(), Lookup::Unknown));
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Hidden });
        assert!(!live.active());
        live.note(shown);
        assert_eq!(live.memory.len(), 1);
        live.note(Signal::MapPage { page: MapPage::Normal, state: PageState::Closed });
        live.note(shown);
        assert!(live.memory.is_empty());
    }
}
