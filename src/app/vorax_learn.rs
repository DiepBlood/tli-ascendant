//! Vorax observe mode (plan section 6): while playing, save whatever could not be read, so that `ascendant-cli
//! vorax-learn` (`crate::vorax::learn`) can later turn it into data that can be pasted back into the name file and
//! icon templates.
//!
//! - Unknown reads (6.1): card name not in the name table, held icon not recognized, tank values only partly read,
//!   monster name not in the table: save the whole frame (`learn-NNNN.jpg`, q92) and the region crop
//!   (`learn-NNNN-crop.jpg`, q98), and write one row `vorax_unknown`. The same key is saved at most once per game
//!   and at most twice across games (`<log root>\learned.jsonl`).
//! - Tooltip (6.2): when a tooltip is on the left, save the whole frame and a tooltip crop and write one row
//!   `vorax_tooltip`; after `Reveal` save one intro reveal page (one per game, `vorax_reveal`); when an icon newly
//!   added by a confirmed instrument is not recognized, write one row `vorax_icon_pair` with the chosen card (a
//!   candidate for the icon templates).
//! - Resolve strip (6.3): the first read within 1 second after `Tool` / `Gone` / `Changed` saves only the tank
//!   label strip (`frame-sNNNN.jpg`, q90) and writes one row `vorax_strip`, with no OCR. During the resolve
//!   animation the gate is closed and the frame is not OCRed, so the strip is saved before the read
//!   ([`Learn::before_read`]).
//! - Space (6.4): before the first jpg save of each game, check the free space on the disk holding the log folder;
//!   below 5 GB only the log row is written and no jpg is saved, and [`Learn::note`] gives the panel a one-line
//!   explanation.
//! - Whole frames of unread boards (HANDOFF 3c, 2026-11-06): a board or result screen read as `Kind::Other` saves
//!   the whole frame (`full-other-NNNN.jpg`, q92) and writes one row `vorax_full_frame`, so frames of other UI scales
//!   can be collected ([`Learn::full_frame`]). On when observe mode or the general observe mode (settings `observe`)
//!   is on; at most one every [`FULL_EVERY_MS`], [`FULL_MAX`] per game, none while the screen looks the same as the
//!   last one saved, and none below the free space threshold. The report export never picks these files (they may
//!   show the account UID): it takes only the files named by `vorax_read`, `vorax_battle_*`, `vorax_unknown` and
//!   `vorax_tooltip` rows.
//!
//! Platform independent and testable: the time (UTC milliseconds) and the paths are passed in as parameters; the
//! way free space is queried can be replaced ([`Learn::with_free_space`]). When observe mode is off (`learn ==
//! false`) nothing is done.

use super::runlog::RunLog;
use super::vorax_session::Cue;
use crate::reader::{Rect, Rgba};
use crate::vorax::names::names;
use crate::vorax::read::{self, Kind, Reading, TANK_X, norm};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many times the same key is saved across games at most.
pub const MAX_SAVES: u32 = 2;
/// The disk holding the log folder needs at least this much free space before a jpg is saved.
pub const MIN_FREE: u64 = 5 << 30;
/// JPEG quality of the whole frame, the region crop and the resolve strip.
const FRAME_QUALITY: u8 = 92;
const CROP_QUALITY: u8 = 98;
const STRIP_QUALITY: u8 = 90;
/// The first read within this long after a resolve cue saves the strip.
pub const STRIP_WITHIN_MS: i64 = 1000;
/// The first read at least this long after `Reveal` saves the intro reveal page (the reveal animation takes about
/// 0.5 seconds).
pub const REVEAL_AFTER_MS: i64 = 500;
/// Left tooltip (reference coordinates x 20..430, y 630..1175).
pub const TOOLTIP: Rect = Rect::new(20, 630, 410, 545);
/// y range of the tank label strip (reference coordinates, full width).
pub const STRIP_Y: (i32, i32) = (478, 590);
/// File name of the cross-game index (in the log root folder, not in the per-game folder, so `prune` does not touch
/// it).
pub const INDEX: &str = "learned.jsonl";
/// File name prefix of the whole frames of unread boards ([`Learn::full_frame`]): `full-other-NNNN.jpg`. The report
/// export picks files by the row that names them, never by a prefix or a folder listing, and no row it reads names
/// these.
pub const FULL_PREFIX: &str = "full-other";
/// At most one whole frame of an unread board this often (ms): a board under an unsupported UI scale reads as
/// `Kind::Other` on every read (several a second), and one frame every 2 s still catches each hand and resolve.
pub const FULL_EVERY_MS: i64 = 2000;
/// At most this many whole frames of unread boards per game: 40 frames of 0.3 to 0.6 MB at 2560x1440 is at most
/// about 24 MB per game, and 40 at 2 s apart cover more than the first 13 rounds of changing screens.
pub const FULL_MAX: usize = 40;
/// A whole frame of an unread board is saved only once the board has read as `Kind::Other` this many reads in a row
/// and for at least [`FULL_OTHER_MS`] (both; the result screen needs the reads alone, see [`Learn::full_frame`]): a
/// round change's animation fades the round number for one read (the
/// 2026-11-06 12:23 game's full-other-0036, 0058 and 0070; the next read 70 to 140 ms later was the board), and a
/// deal animation can last two reads 83 ms apart (the 13:46 game's full-other-0040, R6's three cards still piled up).
pub const FULL_OTHER_READS: u32 = 2;
pub const FULL_OTHER_MS: i64 = 500;
/// No whole frame within this long after a paused game is resumed (ms): skipping an animation shows the world map
/// for a moment before the page is drawn again (the 12:23 game's full-other-0074, 0078 and 0087, 0.07 to 0.1 s
/// after the resume; they show the character name and the chat). A board still unread after it is saved.
pub const FULL_RESUME_QUIET_MS: i64 = 1000;

/// What could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// Card name not in the name table (`en == None`).
    Card,
    /// Icon of a held instrument not recognized (not counting the ones from the intro reveal).
    Icon,
    /// Tank archetype, rarity or values only partly read.
    Tank,
    /// Monster name not in the name table.
    Monster,
}

impl What {
    pub fn name(self) -> &'static str {
        match self {
            What::Card => "card",
            What::Icon => "icon",
            What::Tank => "tank",
            What::Monster => "monster",
        }
    }
}

/// One thing that could not be read: key, OCR raw text, features and crop region (reference coordinates).
#[derive(Clone, Debug, PartialEq)]
pub struct Unknown {
    pub what: What,
    pub key: String,
    pub raw_name: String,
    pub raw_type: String,
    pub raw_effect: String,
    pub why: Option<String>,
    /// Icon features (20x13x3, rounded to 0..255); None for the others.
    pub feature: Option<Vec<u8>>,
    pub region: Rect,
}

/// Card key: the normalized card name plus the type text.
pub fn card_key(raw_name: &str, raw_type: &str) -> String {
    format!("card:{}|{}", norm(raw_name), norm(raw_type))
}

/// Tank and monster key: the normalized monster name.
pub fn tank_key(what: What, raw_name: &str) -> String {
    format!("{}:{}", what.name(), norm(raw_name))
}

/// Tooltip key: the normalized raw card name.
pub fn tooltip_key(raw: &str) -> String {
    format!("tooltip:{}", norm(raw))
}

/// Icon features rounded to 0..255.
pub fn feature_bytes(f: &[f32]) -> Vec<u8> {
    f.iter().map(|v| v.round().clamp(0.0, 255.0) as u8).collect()
}

/// Icon key: an FNV-1a hash of the features quantized in steps of 48 per cell (the difference of the same item at
/// different positions is mostly within one step).
pub fn icon_key(f: &[f32]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in f {
        h ^= u64::from((v.clamp(0.0, 255.0) / 48.0).round() as u8);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("icon:{h:016x}")
}

/// Card region (reference coordinates): from the card name (y~1043 when raised) to the type text at the card bottom
/// (y~1320). When the position is unreliable (`x == 0`, only some of the five potion kit cards found), crop the
/// whole card strip.
fn card_region(x: i32) -> Rect {
    if x == 0 { Rect::new(700, 1020, 1460, 300) } else { Rect::new(x - 130, 1020, 260, 300) }
}

/// Label of the i-th tank (name, total activity, activity x count).
fn tank_region(i: usize) -> Rect {
    Rect::new(TANK_X[i] - 120, STRIP_Y.0, 240, STRIP_Y.1 - STRIP_Y.0)
}

/// Things that could not be read in one read (only the choice screen is looked at: during the resolve animation the
/// tanks glow and the cards move, so unreadable is normal). `icons` gives the features of the held icons (left to
/// right, same as `crate::vorax::icon_features`), and is called only when there are unrecognized icons (the frame
/// has to be converted to the reference size).
pub fn unknowns(r: &Reading, icons: impl FnOnce() -> Vec<Vec<f32>>) -> Vec<Unknown> {
    let mut out = Vec::new();
    if !r.kind.is_choice() {
        return out;
    }
    for c in r.cards.iter().filter(|c| c.en.is_none()) {
        out.push(Unknown {
            what: What::Card,
            key: card_key(&c.raw_name, &c.raw_type),
            raw_name: c.raw_name.clone(),
            raw_type: c.raw_type.clone(),
            raw_effect: c.raw_effect.clone(),
            why: c.why.clone(),
            feature: None,
            region: card_region(c.x),
        });
    }
    let monsters = &names().monsters;
    for (i, t) in r.tanks.iter().enumerate().take(TANK_X.len()) {
        if t.empty {
            continue;
        }
        let named = t.name.as_ref().is_some_and(|n| monsters.iter().any(|(m, _)| m == n));
        let what = if !named {
            What::Monster
        } else if !t.known() {
            What::Tank
        } else {
            continue;
        };
        let why = match what {
            What::Monster => format!("怪物名「{}」對不上名稱表", t.raw_name),
            _ => format!("總活性讀到「{}」、活性 x 數量讀到「{}」", t.raw_total, t.raw_values),
        };
        out.push(Unknown {
            what,
            key: tank_key(what, &t.raw_name),
            raw_name: t.raw_name.clone(),
            raw_type: String::new(),
            raw_effect: format!("{} | {}", t.raw_total, t.raw_values),
            why: Some(why),
            feature: None,
            region: tank_region(i),
        });
    }
    let unknown_icons: Vec<usize> = r.held.iter().enumerate().filter(|(_, h)| h.id.is_none() && !h.opening).map(|(j, _)| j).collect();
    if !unknown_icons.is_empty() {
        let features = icons();
        let n = r.held.len();
        for j in unknown_icons {
            let Some(f) = features.get(j) else { continue };
            let Some(region) = crate::vorax::icon_rect(j, n) else { continue };
            out.push(Unknown {
                what: What::Icon,
                key: icon_key(f),
                raw_name: String::new(),
                raw_type: String::new(),
                raw_effect: String::new(),
                why: Some(format!("相關係數最高 {:.2}", r.held[j].score)),
                feature: Some(feature_bytes(f)),
                region,
            });
        }
    }
    out
}

/// Whether the normalized tooltip text `tip` ([`norm`]) holds the Traditional Chinese name `zh`. OCR reads the
/// hyphen of an archetype card's name (`細肢藥粉-骨衛兵`) as `一` (the 2026-11-06 12:23 game: `細肢藥粉一骨衛兵`,
/// `麻藥酊劑一覺醒者`, `脊髓溶液一異魔`), and [`norm`] drops the hyphen but keeps `一`: so a name with a hyphen also
/// matches with `一` at the hyphen's place, and only there (a `一` that is part of a name, as in `子一代`, is
/// compared as it is).
pub fn tooltip_has(tip: &str, zh: &str) -> bool {
    let z = norm(zh);
    !z.is_empty() && (tip.contains(&z) || (zh.contains('-') && tip.contains(&norm(&zh.replace('-', "一")))))
}

/// The card matched by the tooltip (English name): the one in the hand whose Traditional Chinese name appears in
/// the tooltip raw text ([`tooltip_has`]).
pub fn tooltip_match(r: &Reading) -> Option<String> {
    let tip = norm(&r.tooltip);
    if tip.is_empty() {
        return None;
    }
    r.cards.iter().find(|c| c.zh.as_deref().is_some_and(|z| tooltip_has(&tip, z))).and_then(|c| c.en.clone())
}

/// The tooltip raw text contains some Traditional Chinese name from the name table (card or monster,
/// [`tooltip_has`]). Cards whose zh is blank in the name file, and tooltips whose text could not be read (for
/// example the tooltip of a held instrument, where the card name is not at the `TOOLTIP_NAME` position) all fail
/// to match.
pub fn tooltip_named(raw: &str) -> bool {
    let tip = norm(raw);
    let n = names();
    !tip.is_empty() && n.cards.iter().filter_map(|c| c.zh.as_deref()).chain(n.monsters.iter().map(|(m, _)| m.as_str())).any(|z| tooltip_has(&tip, z))
}

/// Room (reference pixels) the tooltip crop leaves on its right and bottom past the scaled [`TOOLTIP`]: at s 1.26
/// the 12:23 game's protocol tooltip (learn-0083) had its right border's glow out to x 545 and its bottom border at
/// y 1291, 6 and 2 pixels inside the scaled rect (542, 1293).
const TOOLTIP_ROOM: i32 = 10;

/// The left tooltip's rect (reference coordinates) at interface scale `s` (None: the original setting): [`TOOLTIP`]
/// grows with the interface about the left edge's anchor (`crate::vorax::ui_scale::ui_box`), plus
/// [`TOOLTIP_ROOM`] on the right and at the bottom, clipped to the reference frame. The fixed rect cut the
/// tooltip's right side at s 1.26 (the 2026-11-06 12:23 game's learn-0006-crop and learn-0008-crop).
pub fn tooltip_rect(s: Option<f64>) -> Rect {
    let r = scaled_rect(TOOLTIP, s);
    let x1 = (r.x + r.w + TOOLTIP_ROOM).min(read::W as i32);
    let y1 = (r.y + r.h + TOOLTIP_ROOM).min(read::H as i32);
    Rect::new(r.x, r.y, x1 - r.x, y1 - r.y)
}

/// A reference rect at the original setting where interface scale `s` draws it, clipped to the reference frame.
fn scaled_rect(r: Rect, s: Option<f64>) -> Rect {
    let (cx, cy, hw, hh) = crate::vorax::ui_scale::ui_box((r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0, r.w as f32 / 2.0, r.h as f32 / 2.0), s);
    let x0 = ((cx - hw).round() as i32).max(0);
    let y0 = ((cy - hh).round() as i32).max(0);
    let x1 = ((cx + hw).round() as i32).min(read::W as i32);
    let y1 = ((cy + hh).round() as i32).min(read::H as i32);
    Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

/// The band of the left tooltip under its name (reference coordinates, original setting): a monster's tooltip
/// shows its archetype and `總活性` (total activity) there, a card's its effect text (learn-0006-crop and
/// learn-0008-crop of the 2026-11-06 12:23 game, measured back to the original setting).
const TOOLTIP_BELOW_NAME: Rect = Rect::new(40, 890, 370, 110);

/// Whether the left tooltip of `frame` (at its own size, drawn at interface scale `s`) is a monster's: `總活性` in
/// the band under its name ([`TOOLTIP_BELOW_NAME`], one OCR call). A monster's tooltip is not a card's and is not
/// saved as one (most of the 12:23 game's unmatched tooltips were monsters under the mouse on the tanks).
fn monster_tooltip(frame: &Rgba, s: Option<f64>) -> bool {
    let band = frame_rect(frame.w, frame.h, scaled_rect(TOOLTIP_BELOW_NAME, s));
    let lines = crate::reader::ocr::ocr(frame, band, 2.0);
    monster_text(&lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(""))
}

/// The OCR text of the band under a tooltip's name says `總活性` (spaces and punctuation ignored, `總活` enough).
fn monster_text(text: &str) -> bool {
    norm(text).contains("總活")
}

/// Convert a region in reference coordinates (2560x1440) to pixels of a `w`x`h` frame (same as `read::to_frame`),
/// clipped to the frame.
pub fn frame_rect(w: usize, h: usize, r: Rect) -> Rect {
    let (cx, cy) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0);
    let (x, y, hw, hh) = read::to_frame(w, h, (cx, cy, r.w as f32 / 2.0, r.h as f32 / 2.0));
    let x0 = ((x - hw).round() as i32).clamp(0, w as i32);
    let y0 = ((y - hh).round() as i32).clamp(0, h as i32);
    let x1 = ((x + hw).round() as i32).clamp(x0, w as i32);
    let y1 = ((y + hh).round() as i32).clamp(y0, h as i32);
    Rect::new(x0, y0, x1 - x0, y1 - y0)
}

/// Region of the tank label strip in a `w`x`h` frame: y scales with the height, full width.
pub fn strip_rect(w: usize, h: usize) -> Rect {
    let k = h as f32 / read::H as f32;
    let (y0, y1) = ((STRIP_Y.0 as f32 * k).round() as i32, (STRIP_Y.1 as f32 * k).round() as i32);
    Rect::new(0, y0.clamp(0, h as i32), w as i32, (y1 - y0).clamp(0, h as i32))
}

/// Convert UTC milliseconds to the time of a log row.
fn at_time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// Free bytes on the disk holding the log folder (`GetDiskFreeSpaceExW`); when the folder does not exist yet, look
/// at the nearest parent folder. None if it cannot be determined. The windows crate does not enable
/// `Win32_Storage_FileSystem` (Cargo.toml stays untouched), so this one kernel32 function is declared directly.
#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, available: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let dir = path.ancestors().find(|p| p.is_dir())?;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    // SAFETY: wide is 0-terminated and valid for the duration of the call; available is a valid u64; the other two
    // outputs may be null.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(available)
}

/// Non-Windows: free space cannot be determined (when the threshold is not 0, no jpg is saved).
#[cfg(not(windows))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

/// State of observe mode: one per `VoraxSession`.
pub struct Learn {
    /// Observe mode and the resolve strip are on.
    enabled: bool,
    strips: bool,
    /// Root folder of the log (the cross-game index lives here); None means nothing is saved.
    root: Option<PathBuf>,
    /// Cross-game index: how many times each key has been saved (the file is read on first use).
    counts: Option<HashMap<String, u32>>,
    /// Keys saved in this game.
    game_keys: HashSet<String>,
    /// Free space checked in this game: Some(true) means jpg can be saved.
    space: Option<bool>,
    note: Option<String>,
    /// Pending strip: which cue, and when.
    strip: Option<(Cue, i64)>,
    /// Number of strips saved in this game.
    strips_saved: usize,
    /// Time of `Reveal`; None after the reveal page is saved, with `reveal_done` true.
    reveal: Option<i64>,
    reveal_done: bool,
    free: fn(&Path) -> Option<u64>,
    /// Free space threshold (default [`MIN_FREE`]; replay tests set 0 so the disk is not looked at).
    min_free: u64,
    /// The general observe mode (settings `observe`): with it, or with observe mode, whole frames of unread boards
    /// are saved ([`Learn::full_frame`]).
    observe: bool,
    /// Whole frames of unread boards saved in this game.
    full_saved: usize,
    /// The time and fingerprint ([`crate::reader::fingerprint`]) of the last of them.
    full_last: Option<(i64, Vec<u8>)>,
    /// The current run of `Kind::Other` reads: when its first read was, and how many reads it has.
    other_run: Option<(i64, u32)>,
    /// When the paused game was last resumed ([`Learn::resumed`]).
    resumed_at: Option<i64>,
    /// The interface scale the current frame was read at ([`Learn::set_ui`]; None: the original setting).
    ui: Option<f64>,
}

impl Learn {
    pub fn new(root: Option<PathBuf>, enabled: bool, strips: bool) -> Learn {
        Learn {
            enabled,
            strips,
            root,
            counts: None,
            game_keys: HashSet::new(),
            space: None,
            note: None,
            strip: None,
            strips_saved: 0,
            reveal: None,
            reveal_done: false,
            free: free_space,
            min_free: MIN_FREE,
            observe: false,
            full_saved: 0,
            full_last: None,
            other_run: None,
            resumed_at: None,
            ui: None,
        }
    }

    /// The interface scale the next frames are read at (None: the original setting): the tooltip crop grows with it
    /// ([`tooltip_rect`]).
    pub fn set_ui(&mut self, s: Option<f64>) {
        self.ui = s;
    }

    /// The general observe mode (settings `observe`) is on or off: see [`Learn::full_frame`].
    pub fn set_observe(&mut self, observe: bool) {
        self.observe = observe;
    }

    /// Replace the free space threshold (bytes; 0 means do not look at the disk, always allow saving).
    pub fn with_min_free(mut self, min_free: u64) -> Learn {
        self.min_free = min_free;
        self
    }

    /// Replace the way free space is queried (for tests).
    pub fn with_free_space(mut self, free: fn(&Path) -> Option<u64>) -> Learn {
        self.free = free;
        self
    }

    /// Settings changed. If the root folder changed, reload the index.
    pub fn configure(&mut self, root: Option<PathBuf>, enabled: bool, strips: bool, min_free: u64) {
        if min_free != self.min_free {
            (self.min_free, self.space, self.note) = (min_free, None, None);
        }
        if root != self.root {
            (self.root, self.counts, self.space, self.note) = (root, None, None, None);
        }
        (self.enabled, self.strips) = (enabled, strips);
        if !strips {
            self.strip = None;
        }
    }

    /// New game: the per-game dedup, strip numbering, reveal page and space check start over.
    pub fn new_game(&mut self) {
        self.game_keys.clear();
        (self.space, self.note, self.strip, self.strips_saved, self.reveal, self.reveal_done) = (None, None, None, 0, None, false);
        (self.full_saved, self.full_last, self.other_run, self.resumed_at) = (0, None, None, None);
    }

    /// A paused game was resumed at `at`: no whole frame for [`FULL_RESUME_QUIET_MS`], and the run of unread
    /// reads starts over (the reads before the pause were another page).
    pub fn resumed(&mut self, at: i64) {
        (self.resumed_at, self.other_run) = (Some(at), None);
    }

    /// One-line explanation for the panel when space is insufficient.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// How many times this key has been saved across games (the index is read on the first call).
    pub fn saved(&mut self, key: &str) -> u32 {
        self.counts().get(key).copied().unwrap_or(0)
    }

    fn counts(&mut self) -> &mut HashMap<String, u32> {
        let root = self.root.clone();
        self.counts.get_or_insert_with(|| root.map(|r| read_index(&r.join(INDEX))).unwrap_or_default())
    }

    /// This key can still be saved in this game: not saved yet in this game, and fewer than [`MAX_SAVES`] times
    /// across games, whatever the reason to save it (a tooltip of a hand with an unread card once skipped the
    /// cross-game count, and `tooltip:孿生激素一蟲蟲`, matched every time, was saved in every game, over 25 times).
    fn wanted(&mut self, key: &str) -> bool {
        !self.game_keys.contains(key) && self.saved(key) < MAX_SAVES
    }

    /// A jpg can be saved: the free space is checked on the first call of each game; below the threshold (default 5
    /// GB) or if it cannot be determined, nothing is saved (only the log row is written, better to save nothing
    /// than to fill the disk); with threshold 0 it is not checked. When observe mode is off it is not checked
    /// either (original behavior, saving is allowed).
    pub fn jpegs_allowed(&mut self) -> bool {
        if !self.enabled {
            return true;
        }
        self.space_ok()
    }

    /// The free space check of [`Learn::jpegs_allowed`], whether or not observe mode is on: once per game, false
    /// below the threshold or when it cannot be determined, true with threshold 0.
    fn space_ok(&mut self) -> bool {
        if let Some(ok) = self.space {
            return ok;
        }
        let Some(root) = self.root.clone() else { return false };
        let free = if self.min_free == 0 { None } else { (self.free)(&root) };
        let ok = self.min_free == 0 || free.is_some_and(|b| b >= self.min_free);
        if !ok {
            let need = self.min_free as f64 / f64::from(1u32 << 30);
            self.note = Some(match free {
                Some(b) => format!("紀錄資料夾所在磁碟只剩 {:.1} GB（不到 {need:.0} GB）：觀察模式只寫紀錄，不存影格。", b as f64 / f64::from(1u32 << 30)),
                None => format!("查不到紀錄資料夾所在磁碟的可用空間（要 {need:.0} GB）：觀察模式只寫紀錄，不存影格。"),
            });
        }
        self.space = Some(ok);
        ok
    }

    /// A log cue (in Vorax mode): a resolve cue records a pending strip, `Reveal` records the reveal page.
    pub fn note_cue(&mut self, cue: Cue, at: i64) {
        if !self.enabled {
            return;
        }
        if self.strips && cue.resolves() {
            self.strip = Some((cue, at));
        }
        if cue == Cue::Reveal && !self.reveal_done && self.reveal.is_none() {
            self.reveal = Some(at);
        }
    }

    /// Drop the pending resolve strip: a paused game was resumed, so the next frame is the reopened page, not the
    /// resolve animation the cue was for (it was saved as a strip with round None).
    pub fn drop_strip(&mut self) {
        self.strip = None;
    }

    /// Whether a resolve strip is waiting for the next frame (tests).
    #[cfg(test)]
    pub fn strip_pending(&self) -> bool {
        self.strip.is_some()
    }

    /// Before a read (before the gate, no OCR): save the pending strip and the intro reveal page. `round` is the
    /// last round read on screen.
    pub fn before_read(&mut self, log: &mut RunLog, frame: &Rgba, at: i64, round: Option<i32>) {
        if !self.enabled || self.root.is_none() {
            return;
        }
        if let Some((cue, since)) = self.strip {
            if at - since > STRIP_WITHIN_MS {
                self.strip = None;
            } else if at >= since {
                self.strip = None;
                self.strips_saved += 1;
                let file = self.jpegs_allowed().then(|| {
                    let name = format!("frame-s{:04}.jpg", log.number());
                    log.jpeg_as(&name, frame.crop(strip_rect(frame.w, frame.h)), STRIP_QUALITY);
                    name
                });
                log.event("vorax_strip", at_time(at), json!({ "cue": format!("{cue:?}"), "index": self.strips_saved, "round": round, "frame": file, "cueAt": since }));
            }
        }
        if let Some(since) = self.reveal
            && at - since >= REVEAL_AFTER_MS
        {
            (self.reveal, self.reveal_done) = (None, true);
            let file = self.jpegs_allowed().then(|| {
                let name = format!("learn-{:04}.jpg", log.number());
                log.jpeg_as(&name, frame.clone(), FRAME_QUALITY);
                name
            });
            log.event("vorax_reveal", at_time(at), json!({ "cueAt": since, "frame": file }));
        }
    }

    /// After a read: save what could not be read (steady reads only) and the tooltip. `icons` as in [`unknowns`].
    pub fn after_read(&mut self, log: &mut RunLog, frame: &Rgba, r: &Reading, steady: bool, at: i64, icons: impl FnOnce() -> Vec<Vec<f32>>) {
        if !self.enabled || self.root.is_none() {
            return;
        }
        // Rows to write: kind, key, crop region (reference coordinates), fields.
        let mut rows: Vec<(&str, String, Rect, Value)> = Vec::new();
        if steady {
            for u in unknowns(r, icons) {
                if !self.wanted(&u.key) {
                    continue;
                }
                self.game_keys.insert(u.key.clone());
                rows.push((
                    "vorax_unknown",
                    u.key.clone(),
                    u.region,
                    json!({ "what": u.what.name(), "key": u.key, "rawName": u.raw_name, "rawType": u.raw_type, "rawEffect": u.raw_effect, "why": u.why, "feature": u.feature, "round": r.round, "screen": r.kind }),
                ));
            }
        }
        if r.panel_covered && r.round.is_some() {
            let key = tooltip_key(&r.tooltip);
            let matched = tooltip_match(r);
            // Saved at most twice across games like every other key, whether or not the hand has an unknown card or
            // the raw text names no card of the name table.
            if self.wanted(&key) {
                self.game_keys.insert(key.clone());
                // A monster's tooltip (the mouse on a tank) is looked at once per game and key, and not saved.
                if matched.is_some() || !monster_tooltip(frame, self.ui) {
                    rows.push(("vorax_tooltip", key.clone(), tooltip_rect(self.ui), json!({ "key": key, "raw": r.tooltip, "matched": matched, "round": r.round, "screen": r.kind })));
                }
            }
        }
        if rows.is_empty() {
            return;
        }
        let saving = self.jpegs_allowed();
        let n = saving.then(|| log.number());
        let whole = n.map(|n| {
            let name = format!("learn-{n:04}.jpg");
            log.jpeg_as(&name, frame.clone(), FRAME_QUALITY);
            name
        });
        for (k, (kind, key, region, mut fields)) in rows.into_iter().enumerate() {
            let crop = n.map(|n| {
                let name = if k == 0 { format!("learn-{n:04}-crop.jpg") } else { format!("learn-{n:04}-crop-{}.jpg", k + 1) };
                log.jpeg_as(&name, frame.crop(frame_rect(frame.w, frame.h, region)), CROP_QUALITY);
                name
            });
            fields["frame"] = json!(whole);
            fields["crop"] = json!(crop);
            fields["size"] = json!([frame.w, frame.h]);
            log.event(kind, at_time(at), fields);
            if let Some(file) = &crop {
                self.count(log, &key, kind, file, at);
            }
        }
    }

    /// A confirmed instrument's newly added icon is not recognized: pair it with the chosen card (`card` is `{en,
    /// zh, rawName}`), and write one row `vorax_icon_pair`. `(j, n)` is the position of the new icon (counted from
    /// the left) and the number of icon cells.
    pub fn icon_pair(&mut self, log: &mut RunLog, frame: &Rgba, card: Value, (j, n): (usize, usize), features: &[Vec<f32>], at: i64) {
        if !self.enabled || self.root.is_none() {
            return;
        }
        let Some(region) = crate::vorax::icon_rect(j, n) else { return };
        let Some(f) = features.get(j) else { return };
        let key = format!("pair:{}", icon_key(f));
        if self.game_keys.contains(&key) {
            return;
        }
        self.game_keys.insert(key.clone());
        let crop = self.jpegs_allowed().then(|| {
            let name = format!("learn-{:04}-crop.jpg", log.number());
            log.jpeg_as(&name, frame.crop(frame_rect(frame.w, frame.h, region)), CROP_QUALITY);
            name
        });
        log.event("vorax_icon_pair", at_time(at), json!({ "card": card, "key": icon_key(f), "feature": feature_bytes(f), "crop": crop, "size": [frame.w, frame.h] }));
    }

    /// A read of the board or the result screen (`mode`, as the row's `mode`) came back `Kind::Other`: save the whole
    /// frame as `full-other-NNNN.jpg` and write one `vorax_full_frame` row with its file and why, for frames of other
    /// UI scales (HANDOFF 3c). The caller leaves out the intro page (always `Other`) and the endgame battle. Only with
    /// observe mode or the general observe mode on and a log root; at most one every [`FULL_EVERY_MS`] (a clock gone
    /// back counts as time passed), [`FULL_MAX`] per game, none while the screen does not differ from the last one
    /// saved ([`super::session::differs`]), none below the free space threshold. The caller hands every read of the
    /// board and the result screen here, read or not: a frame is saved only once the reads have been `Other`
    /// [`FULL_OTHER_READS`] times in a row and for [`FULL_OTHER_MS`] since the first of them (on the result screen,
    /// `mode` "result", the reads in a row alone), and not within [`FULL_RESUME_QUIET_MS`] of a
    /// resume (a clock gone back counts as time passed). Returns the file saved.
    pub fn full_frame(&mut self, log: &mut RunLog, frame: &Rgba, r: &Reading, at: i64, mode: &str) -> Option<String> {
        if !(self.enabled || self.observe) || self.root.is_none() {
            return None;
        }
        if r.kind != Kind::Other {
            self.other_run = None;
            return None;
        }
        let (since, reads) = self.other_run.map_or((at, 1), |(since, n)| (since, n + 1));
        self.other_run = Some((since, reads));
        // A clock gone back counts as time passed. The result screen needs the reads alone: it is on show for about
        // 0.95 s (2026-11-06_14-06-38: 34.405 to 35.345), and its one saved frame there (full-other-0071, the second
        // unread read, 145 ms after the first) is the only one showing its final activity.
        let quiet = if mode == "result" { 0 } else { FULL_OTHER_MS };
        if reads < FULL_OTHER_READS || (0..quiet).contains(&(at - since)) {
            return None;
        }
        if self.resumed_at.is_some_and(|t| (0..FULL_RESUME_QUIET_MS).contains(&(at - t))) {
            return None;
        }
        if self.full_saved >= FULL_MAX {
            return None;
        }
        if self.full_last.as_ref().is_some_and(|(last, _)| (0..FULL_EVERY_MS).contains(&(at - last))) {
            return None;
        }
        let print = crate::reader::fingerprint(frame);
        if self.full_last.as_ref().is_some_and(|(_, last)| !super::session::differs(last, &print)) {
            return None;
        }
        if !self.space_ok() {
            return None;
        }
        let name = log.jpeg(FULL_PREFIX, frame.clone(), FRAME_QUALITY);
        self.full_saved += 1;
        self.full_last = Some((at, print));
        log.event(
            "vorax_full_frame",
            at_time(at),
            json!({ "file": name, "reason": "kind_other", "mode": mode, "index": self.full_saved, "problems": r.problems, "size": [frame.w, frame.h] }),
        );
        Some(name)
    }

    /// A key was saved: add one row to the cross-game index.
    fn count(&mut self, log: &RunLog, key: &str, kind: &str, file: &str, at: i64) {
        *self.counts().entry(key.to_string()).or_insert(0) += 1;
        let Some(root) = &self.root else { return };
        let run = log.dir().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let line = json!({ "at": super::runlog::timestamp(at_time(at)), "key": key, "kind": kind, "run": run, "file": file });
        let _ = std::fs::create_dir_all(root);
        let _ = append(&root.join(INDEX), &line.to_string());
    }
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

/// Read the cross-game index: how many times each key appears. Read row by row: non-UTF-8 bytes are replaced with
/// the replacement character, broken rows are skipped, one broken row does not affect the others; if the file
/// cannot be read the result is empty.
pub fn read_index(path: &Path) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    let bytes = std::fs::read(path).unwrap_or_default();
    for line in String::from_utf8_lossy(&bytes).lines() {
        if let Ok(v) = serde_json::from_str::<Value>(line)
            && let Some(key) = v["key"].as_str()
        {
            *counts.entry(key.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vorax::names::Category;
    use crate::vorax::read::{Card, Held, Kind, Tank};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ascendant-vorax-learn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn card(raw_name: &str, en: Option<&str>) -> Card {
        Card {
            category: Category::Potion,
            index: None,
            id: None,
            en: en.map(str::to_string),
            zh: None,
            how: None,
            raw_name: raw_name.into(),
            raw_type: "普通藥劑".into(),
            raw_effect: "選擇一個水箱".into(),
            why: Some("名稱表裡沒有相符的繁中名".into()),
            x: 1205,
            dy: 0,
            raised: false,
        }
    }

    /// A potion screen: card 1 is unknown, all others are read.
    fn reading() -> Reading {
        Reading {
            kind: Kind::Potion,
            round: Some(3),
            ultimate: None,
            ultimate_pending: false,
            panel_covered: false,
            next_reward: None,
            rerolls: Some(3),
            tanks: vec![Tank { empty: true, ..Tank::default() }; 6],
            held: vec![],
            cards: vec![card("怪 藥", None), card("好藥", Some("Good"))],
            tank_sum: None,
            problems: vec![],
            tooltip: String::new(),
        }
    }

    fn pixel_frame(w: usize, h: usize) -> Rgba {
        Rgba { w, h, data: vec![40; w * h * 4] }
    }

    fn rows(log: &RunLog) -> Vec<Value> {
        log.wait();
        std::fs::read_to_string(log.dir().join("log.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn plenty(_: &Path) -> Option<u64> {
        Some(100 << 30)
    }

    fn scarce(_: &Path) -> Option<u64> {
        Some(1 << 30)
    }

    #[test]
    fn keys_ignore_ocr_noise_and_keep_categories_apart() {
        assert_eq!(card_key("怪 藥！", "普通 藥劑"), "card:怪藥|普通藥劑");
        assert_eq!(card_key("怪藥", "普通藥劑"), card_key(" 怪 藥 ", "普通藥劑。"));
        assert_ne!(card_key("怪藥", "普通藥劑"), card_key("怪藥", "魔法藥劑"));
        assert_eq!(tank_key(What::Monster, "骨 衛兵"), "monster:骨衛兵");
        assert_ne!(tank_key(What::Tank, "骨衛兵"), tank_key(What::Monster, "骨衛兵"));
        assert_eq!(tooltip_key("［蒸骨坩堝］"), "tooltip:蒸骨坩堝");
        // The tooltip matches the name table (monster names count too); matched or not, it is saved up to 2 times
        // across games.
        let (m, _) = &names().monsters[0];
        assert!(tooltip_named(&format!("細肢藥粉{m}")) && !tooltip_named("好藥") && !tooltip_named(""));
        // Icons: noise within one step gives the same key, a different shape gives a different one.
        let f: Vec<f32> = (0..780).map(|k| (k * 7 % 256) as f32).collect();
        let noisy: Vec<f32> = f.iter().map(|v| (v + 3.0).min(255.0)).collect();
        let other: Vec<f32> = f.iter().map(|v| 255.0 - v).collect();
        assert!(icon_key(&f).starts_with("icon:"));
        let same = f.iter().zip(&noisy).filter(|(a, b)| (*a / 48.0).round() == (*b / 48.0).round()).count();
        assert_eq!(icon_key(&f) == icon_key(&noisy), same == f.len());
        assert_ne!(icon_key(&f), icon_key(&other));
        assert_eq!(feature_bytes(&[0.4, 254.6, 300.0, -2.0]), vec![0, 255, 255, 0]);
    }

    #[test]
    fn unknowns_list_cards_tanks_and_icons() {
        let mut r = reading();
        r.tanks[1] = Tank { empty: false, name: Some("讀錯的名字".into()), raw_name: "讀錯的名字".into(), ..Tank::default() };
        let (m, _) = names().monsters[0].clone();
        r.tanks[2] = Tank { empty: false, name: Some(m.clone()), raw_name: m.clone(), raw_total: "1?".into(), ..Tank::default() };
        r.held = vec![
            Held { id: Some(1), en: None, zh: None, opening: false, score: 0.99 },
            Held { id: None, en: None, zh: Some("蒸骨坩堝".into()), opening: true, score: 0.97 },
            Held { id: None, en: None, zh: None, opening: false, score: 0.7 },
        ];
        let mut asked = 0;
        let us = unknowns(&r, || {
            asked += 1;
            vec![vec![1.0; 780], vec![2.0; 780], vec![3.0; 780]]
        });
        assert_eq!(asked, 1);
        let whats: Vec<(What, &str)> = us.iter().map(|u| (u.what, u.key.as_str())).collect();
        assert_eq!(whats[0], (What::Card, "card:怪藥|普通藥劑"));
        assert_eq!(whats[1], (What::Monster, "monster:讀錯的名字"));
        assert_eq!(whats[2].0, What::Tank);
        assert_eq!(whats[2].1, format!("tank:{}", norm(&m)));
        // Ones from the opening reveal do not count; the unrecognized one is cell 3 (counted from the left).
        assert_eq!(whats[3].0, What::Icon);
        assert_eq!(us[3].feature.as_deref(), Some(&[3u8; 780][..]));
        assert_eq!(us.len(), 4);
        assert_eq!(us[3].region, crate::vorax::icon_rect(2, 3).unwrap());
        // Not looked at during the resolve animation; when all icons are recognized the frame is not converted to
        // the reference size.
        r.kind = Kind::Resolving;
        assert!(unknowns(&r, || panic!("not needed")).is_empty());
        let mut r = reading();
        r.cards.remove(0);
        assert!(unknowns(&r, || panic!("not needed")).is_empty());
    }

    /// The 2026-11-06 12:23 game's tooltips: a hyphen read as `一` still names the card (only at the hyphen's place),
    /// and a monster's tooltip is told by its `總活性`.
    #[test]
    fn tooltips_match_a_hyphen_read_as_one_and_tell_monsters() {
        let tip = |raw: &str| norm(raw);
        assert!(tooltip_has(&tip("細 肢 藥 粉 一 骨 衛 兵"), "細肢藥粉-骨衛兵"));
        assert!(tooltip_has(&tip("麻 藥 酊 劑 一 覺 醒 者"), "麻藥酊劑-覺醒者"));
        assert!(tooltip_has(&tip("脊 髓 溶 液 一 異 魔"), "脊髓溶液-異魔"));
        assert!(tooltip_has(&tip("細 肢 藥 粉 - 蠱 蟲"), "細肢藥粉-蠱蟲"), "the hyphen read as itself, as before");
        assert!(!tooltip_has(&tip("細 肢 藥 粉 一 骨 衛 兵"), "細肢藥粉-蠱蟲"));
        // A name without a hyphen is compared as it is: its own `一` must be read, and a `一` read elsewhere is not
        // dropped.
        assert!(tooltip_has(&tip("子 一 代 ： 寄 生 蠱 卵"), "子一代：寄生蠱卵"));
        assert!(!tooltip_has(&tip("子 代 ： 寄 生 蠱 卵"), "子一代：寄生蠱卵"));
        assert!(!tooltip_has(&tip("暴 飬 巨 一 蠕 蟲"), "暴飬巨蠕蟲"));
        assert!(!tooltip_has(&tip("好藥"), ""));
        assert!(tooltip_named("細 肢 藥 粉 一 骨 衛 兵"));
        let mut r = reading();
        r.cards[0].zh = Some("麻藥酊劑-覺醒者".into());
        r.cards[0].en = Some("Narcotic Tincture - Awakened".into());
        r.tooltip = "麻 藥 酊 劑 一 覺 醒 者".into();
        assert_eq!(tooltip_match(&r).as_deref(), Some("Narcotic Tincture - Awakened"));
        // The band under the name: a monster's archetype and total activity, a card's effect text.
        assert!(monster_text("骨 衛 兵 總 活 性 180"));
        assert!(monster_text("總活性"));
        assert!(!monster_text("添加1組 骨衛兵，使 +73 數量"));
        assert!(!monster_text("選擇1組怪物 +41 活性"));
        assert!(!monster_text(""));
    }

    /// The tooltip crop grows with the interface about the left edge (0, 720): at the original setting it is
    /// [`TOOLTIP`] with 10 pixels more on the right and at the bottom, at the 12:23 game's s 1.2596 it holds the
    /// whole tooltip (learn-0083's protocol tooltip: right glow to x 545, bottom border at y 1291), clipped to the
    /// frame at the largest.
    #[test]
    fn the_tooltip_crop_grows_with_the_interface() {
        let roomy = Rect::new(TOOLTIP.x, TOOLTIP.y, TOOLTIP.w + 10, TOOLTIP.h + 10);
        assert_eq!(tooltip_rect(None), roomy);
        assert_eq!(tooltip_rect(Some(1.0)), roomy);
        let s = crate::vorax::ui_scale::from_page_scale(0.840137);
        let r = tooltip_rect(Some(s));
        let at = |v: f64, a: f64| (a + (v - a) * s).round() as i32;
        let want = [at(20.0, 0.0), at(630.0, 720.0), at(430.0, 0.0) + 10, at(1175.0, 720.0) + 10];
        let got = [r.x, r.y, r.x + r.w, r.y + r.h];
        assert!(got.iter().zip(want).all(|(g, w)| (g - w).abs() <= 1), "{got:?} vs {want:?}");
        assert!(r.w > 500 && r.h > 680, "{r:?}");
        assert!(r.x + r.w >= 545 + 5 && r.y + r.h >= 1291 + 5, "learn-0083's tooltip with room: {r:?}");
        let big = tooltip_rect(Some(crate::vorax::ui_scale::from_page_scale(1.0)));
        assert!(big.y >= 0 && big.y + big.h <= read::H as i32 && big.w > r.w, "{big:?}");
    }

    #[test]
    fn regions_scale_with_the_frame() {
        assert_eq!(frame_rect(2560, 1440, TOOLTIP), TOOLTIP);
        // 1920x1080: scaled by 0.75 at the same ratio.
        assert_eq!(frame_rect(1920, 1080, TOOLTIP), Rect::new(15, 473, 308, 408));
        // 3440x1440: 440 extra pixels on each side.
        assert_eq!(frame_rect(3440, 1440, TOOLTIP), Rect::new(460, 630, 410, 545));
        // 1280x1024 (5:4): left and right are cropped, the part beyond the frame is clipped.
        let r = frame_rect(1280, 1024, TOOLTIP);
        assert!(r.x == 0 && r.w > 0 && r.x + r.w <= 1280, "{r:?}");
        assert_eq!(strip_rect(2560, 1440), Rect::new(0, 478, 2560, 112));
        assert_eq!(strip_rect(1920, 1080), Rect::new(0, 359, 1920, 84));
        assert_eq!(strip_rect(3440, 1440), Rect::new(0, 478, 3440, 112));
    }

    #[test]
    fn a_key_is_kept_twice_across_runs_and_the_index_reads_back() {
        let root = temp("index");
        let frame = pixel_frame(64, 36);
        for run in 0..3 {
            // On each program restart, a new Learn reads the counts back from the index.
            let mut learn = Learn::new(Some(root.clone()), true, true).with_free_space(plenty);
            learn.new_game();
            let mut log = RunLog::new(root.clone());
            log.new_run();
            // Two reads in the same game: saved only once.
            learn.after_read(&mut log, &frame, &reading(), true, 1000, Vec::new);
            learn.after_read(&mut log, &frame, &reading(), true, 2000, Vec::new);
            let unknown: Vec<Value> = rows(&log).into_iter().filter(|r| r["kind"] == "vorax_unknown").collect();
            assert_eq!(unknown.len(), usize::from(run < 2), "run {run}");
            assert_eq!(learn.saved("card:怪藥|普通藥劑"), (run + 1).min(2) as u32);
            if run == 0 {
                let u = &unknown[0];
                assert_eq!((u["what"].as_str(), u["key"].as_str(), u["rawName"].as_str()), (Some("card"), Some("card:怪藥|普通藥劑"), Some("怪 藥")));
                assert_eq!((u["frame"].as_str(), u["crop"].as_str()), (Some("learn-0001.jpg"), Some("learn-0001-crop.jpg")));
                assert!(log.dir().join("learn-0001.jpg").is_file() && log.dir().join("learn-0001-crop.jpg").is_file());
            }
            drop(log);
        }
        assert_eq!(read_index(&root.join(INDEX)).get("card:怪藥|普通藥劑"), Some(&2));
        // Unsteady reads are not saved.
        let mut learn = Learn::new(Some(root.clone()), true, true).with_free_space(plenty);
        let mut log = RunLog::new(root.join("other"));
        let mut r = reading();
        r.cards[0].raw_name = "別的藥".into();
        learn.after_read(&mut log, &frame, &r, false, 0, Vec::new);
        assert!(rows(&log).is_empty());
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every save keeps to [`MAX_SAVES`] across games, even a tooltip of a hand with an unread card (which was
    /// saved in every game: `tooltip:孿生激素一蟲蟲` over 25 times).
    #[test]
    fn tooltips_of_a_hand_with_an_unread_card_keep_the_cross_game_cap() {
        let root = temp("tooltip-cap");
        let mut learn = Learn::new(Some(root.clone()), true, false).with_free_space(plenty);
        let frame = pixel_frame(1920, 1080);
        let mut r = reading();
        r.panel_covered = true;
        r.tooltip = "孿 生 激 素 一 蠱 蟲".into();
        // The hand has an unread card.
        r.cards[0].en = None;
        assert!(r.cards.iter().any(|c| c.en.is_none()));
        let key = tooltip_key(&r.tooltip);
        let mut saved = Vec::new();
        let mut log = RunLog::new(root.clone());
        for game in 0..4 {
            learn.new_game();
            learn.after_read(&mut log, &frame, &r, false, 1_000 * game, Vec::new);
            learn.after_read(&mut log, &frame, &r, false, 1_000 * game + 500, Vec::new);
            saved.push(learn.saved(&key));
        }
        assert_eq!(saved, [1, 2, 2, 2]);
        assert_eq!(rows(&log).iter().filter(|x| x["kind"] == "vorax_tooltip").count(), 2);
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tooltips_strips_and_the_reveal() {
        let root = temp("tooltip");
        let mut learn = Learn::new(Some(root.clone()), true, true).with_free_space(plenty);
        let mut log = RunLog::new(root.clone());
        let frame = pixel_frame(1920, 1080);
        let mut r = reading();
        r.cards.remove(0);
        r.panel_covered = true;
        r.tooltip = "好藥".into();
        // Tooltip of a card in the hand (Traditional Chinese name in the raw text): up to 2 times across games,
        // once per game.
        r.cards[0].zh = Some("好藥".into());
        learn.after_read(&mut log, &frame, &r, true, 100, Vec::new);
        learn.after_read(&mut log, &frame, &r, false, 200, Vec::new);
        // Resolve strip: the first read within 1 second after Tool; after 1 second it is not saved.
        learn.note_cue(Cue::Tool, 1000);
        learn.before_read(&mut log, &frame, 1150, Some(3));
        learn.before_read(&mut log, &frame, 1300, Some(3));
        learn.note_cue(Cue::Gone, 2000);
        learn.before_read(&mut log, &frame, 3500, Some(3));
        // Intro reveal page: the first read 0.5 seconds after Reveal, one per game.
        learn.note_cue(Cue::Reveal, 4000);
        learn.before_read(&mut log, &frame, 4200, None);
        learn.before_read(&mut log, &frame, 4600, None);
        learn.note_cue(Cue::Reveal, 5000);
        learn.before_read(&mut log, &frame, 6000, None);
        let all = rows(&log);
        let of = |k: &str| all.iter().filter(|r| r["kind"] == k).collect::<Vec<_>>();
        let tips = of("vorax_tooltip");
        assert_eq!(tips.len(), 1);
        assert_eq!((tips[0]["matched"].as_str(), tips[0]["raw"].as_str(), &tips[0]["size"]), (Some("Good"), Some("好藥"), &json!([1920, 1080])));
        let strips = of("vorax_strip");
        assert_eq!(strips.len(), 1);
        assert_eq!((strips[0]["cue"].as_str(), strips[0]["index"].as_u64(), strips[0]["round"].as_i64()), (Some("Tool"), Some(1), Some(3)));
        let strip = strips[0]["frame"].as_str().unwrap();
        assert!(strip.starts_with("frame-s") && log.dir().join(strip).is_file(), "{strip}");
        assert_eq!(of("vorax_reveal").len(), 1);
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nothing_is_kept_when_learn_is_off() {
        let root = temp("off");
        let mut learn = Learn::new(Some(root.clone()), false, true).with_free_space(plenty);
        let mut log = RunLog::new(root.clone());
        let frame = pixel_frame(64, 36);
        let mut r = reading();
        r.panel_covered = true;
        learn.after_read(&mut log, &frame, &r, true, 0, || panic!("not asked"));
        learn.note_cue(Cue::Tool, 0);
        learn.note_cue(Cue::Reveal, 0);
        learn.before_read(&mut log, &frame, 600, None);
        assert!(rows(&log).is_empty());
        assert!(!root.join(INDEX).exists());
        // When off, free space is not checked (original behavior).
        assert!(learn.jpegs_allowed() && learn.note().is_none());
        // When the strip is off, only the strip is not saved.
        let mut learn = Learn::new(Some(root.clone()), true, false).with_free_space(plenty);
        learn.note_cue(Cue::Tool, 0);
        learn.before_read(&mut log, &frame, 100, None);
        assert!(rows(&log).is_empty());
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn low_disk_space_writes_rows_without_jpegs() {
        let root = temp("space");
        let mut learn = Learn::new(Some(root.clone()), true, true).with_free_space(scarce);
        let mut log = RunLog::new(root.clone());
        learn.after_read(&mut log, &pixel_frame(64, 36), &reading(), true, 0, Vec::new);
        let all = rows(&log);
        assert_eq!(all.len(), 1);
        assert_eq!((&all[0]["frame"], &all[0]["crop"]), (&Value::Null, &Value::Null));
        assert!(learn.note().is_some_and(|n| n.contains("5 GB")));
        // Not saved, so not counted in the index.
        assert_eq!(learn.saved("card:怪藥|普通藥劑"), 0);
        assert!(std::fs::read_dir(log.dir()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".jpg")));
        // With threshold 0 (replay tests) the same disk saves fine.
        let mut zero = Learn::new(Some(root.clone()), true, true).with_free_space(scarce).with_min_free(0);
        assert!(zero.jpegs_allowed() && zero.note().is_none());
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_disk_space_writes_rows_without_jpegs() {
        // Free space cannot be determined, threshold 5 GB: better not to save the jpg, the row is still written.
        let root = temp("unknown-space");
        let mut learn = Learn::new(Some(root.clone()), true, true).with_free_space(|_| None);
        let mut log = RunLog::new(root.clone());
        learn.after_read(&mut log, &pixel_frame(64, 36), &reading(), true, 0, Vec::new);
        let all = rows(&log);
        assert_eq!(all.len(), 1);
        assert_eq!((&all[0]["frame"], &all[0]["crop"]), (&Value::Null, &Value::Null));
        assert!(learn.note().is_some_and(|n| n.contains("查不到")));
        assert!(std::fs::read_dir(log.dir()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".jpg")));
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_index_line_does_not_hide_the_others() {
        let root = temp("broken-index");
        std::fs::create_dir_all(&root).unwrap();
        let mut bytes = b"{\"key\":\"card:a|\"}\n{not json\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
        bytes.extend_from_slice("{\"key\":\"card:a|\"}\r\n{\"key\":\"icon:01\"}\n{\"key\":\"tank:半".as_bytes());
        std::fs::write(root.join(INDEX), &bytes).unwrap();
        let counts = read_index(&root.join(INDEX));
        assert_eq!((counts.get("card:a|"), counts.get("icon:01"), counts.len()), (Some(&2), Some(&1), 2));
        assert!(read_index(&root.join("missing.jsonl")).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A board read as `Kind::Other`.
    fn other() -> Reading {
        Reading { kind: Kind::Other, round: None, cards: vec![], rerolls: None, problems: vec!["no round".into()], ..reading() }
    }

    /// One of two plainly different screens.
    fn screen(k: usize) -> Rgba {
        Rgba { w: 64, h: 36, data: vec![if k.is_multiple_of(2) { 40 } else { 200 }; 64 * 36 * 4] }
    }

    fn full_rows(log: &RunLog) -> Vec<Value> {
        rows(log).into_iter().filter(|r| r["kind"] == "vorax_full_frame").collect()
    }

    #[test]
    fn full_frames_only_with_an_observe_mode_on() {
        let root = temp("full-off");
        // Both off: nothing, and the disk is not looked at.
        let mut learn = Learn::new(Some(root.clone()), false, false).with_free_space(|_| panic!("not looked at"));
        let mut log = RunLog::new(root.clone());
        assert_eq!(learn.full_frame(&mut log, &screen(0), &other(), 0, "board"), None);
        assert!(full_rows(&log).is_empty());
        // The general observe mode alone saves; so does observe mode alone.
        learn = learn.with_free_space(plenty);
        learn.set_observe(true);
        // The first unread read only starts the run (FULL_OTHER_READS, FULL_OTHER_MS).
        assert_eq!(learn.full_frame(&mut log, &screen(0), &other(), -500, "board"), None);
        assert_eq!(learn.full_frame(&mut log, &screen(0), &other(), 0, "board").as_deref(), Some("full-other-0001.jpg"));
        let mut vorax_only = Learn::new(Some(root.clone()), true, false).with_free_space(plenty);
        assert_eq!(vorax_only.full_frame(&mut log, &screen(1), &other(), -500, "result"), None);
        assert!(vorax_only.full_frame(&mut log, &screen(1), &other(), 0, "result").is_some_and(|f| f.starts_with("full-other-")));
        // A read that is not Other, or no log root: nothing.
        let mut learn = Learn::new(Some(root.clone()), true, false).with_free_space(plenty);
        assert_eq!(learn.full_frame(&mut log, &screen(0), &reading(), 0, "board"), None);
        let mut rootless = Learn::new(None, true, false).with_free_space(plenty);
        assert_eq!(rootless.full_frame(&mut log, &screen(0), &other(), 0, "board"), None);
        let rows = full_rows(&log);
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!((r["file"].as_str(), r["reason"].as_str(), r["mode"].as_str(), r["index"].as_u64()), (Some("full-other-0001.jpg"), Some("kind_other"), Some("board"), Some(1)));
        assert_eq!((&r["problems"], &r["size"]), (&json!(["no round"]), &json!([64, 36])));
        assert!(log.dir().join("full-other-0001.jpg").is_file());
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn full_frames_are_spaced_capped_and_deduplicated() {
        let root = temp("full-limits");
        let mut learn = Learn::new(Some(root.clone()), true, false).with_free_space(plenty);
        let mut log = RunLog::new(root.clone());
        let mut save = |learn: &mut Learn, k: usize, at: i64| learn.full_frame(&mut log, &screen(k), &other(), at, "board").is_some();
        assert!(!save(&mut learn, 0, 9_500), "the first unread read only starts the run");
        assert!(save(&mut learn, 0, 10_000));
        // Within 2 s: not even a different screen.
        assert!(!save(&mut learn, 1, 11_999));
        // 2 s later, but the same screen as the last saved: not saved.
        assert!(!save(&mut learn, 0, 12_000));
        assert!(save(&mut learn, 1, 12_000));
        // A clock gone back is time passed.
        assert!(save(&mut learn, 0, 5_000));
        // The cap: FULL_MAX per game, whatever comes after.
        let mut saved = 3;
        for k in 1..100 {
            saved += usize::from(save(&mut learn, k, 20_000 + 2_000 * k as i64));
        }
        assert_eq!(saved, FULL_MAX);
        // A new game starts over.
        learn.new_game();
        assert!(!save(&mut learn, 0, 999_500));
        assert!(save(&mut learn, 0, 1_000_000));
        assert_eq!(full_rows(&log).len(), FULL_MAX + 1);
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The 2026-11-06 12:23 game: a round change's animation reads as Other for one read and is not saved, nor are
    /// two in a row 83 ms apart (the 13:46 game's deal animation), only a run of two or more over 500 ms; right after a resume the world map is not saved, but a board still unread over a second after the
    /// resume is; the 2 s spacing still holds.
    #[test]
    fn full_frames_wait_for_a_run_of_unread_reads_and_skip_a_resume() {
        let root = temp("full-run");
        let mut learn = Learn::new(Some(root.clone()), true, false).with_free_space(plenty);
        let mut log = RunLog::new(root.clone());
        // Each read on a screen different from the one before (a frame equal to the last saved one is never saved).
        let mut save = |learn: &mut Learn, r: &Reading, at: i64| learn.full_frame(&mut log, &screen((at / 1_000) as usize), r, at, "board").is_some();
        // One unread read between two boards (full-other-0036, 0058, 0070): never saved.
        for at in [1_000, 3_000, 5_000] {
            assert!(!save(&mut learn, &other(), at));
            assert!(!save(&mut learn, &reading(), at + 100));
        }
        // Two in a row 83 ms apart (a deal animation, the 13:46 game's full-other-0040): not saved; still unread
        // 500 ms after the first: saved.
        assert!(!save(&mut learn, &other(), 10_000));
        assert!(!save(&mut learn, &other(), 10_083));
        assert!(!save(&mut learn, &other(), 10_400));
        assert!(save(&mut learn, &other(), 10_500));
        // A resume: the world map in the first second (full-other-0074, 0078, 0087, 0.07 to 0.1 s after) is not
        // saved, however many reads.
        learn.resumed(20_000);
        for at in [20_080, 20_200, 20_500, 20_999] {
            assert!(!save(&mut learn, &other(), at));
        }
        // Still unread over a second after the resume: saved.
        assert!(save(&mut learn, &other(), 21_100));
        // A resume read as the board again after 0.5 s: the later single unread read is not saved either.
        learn.resumed(30_000);
        assert!(!save(&mut learn, &other(), 30_080));
        assert!(!save(&mut learn, &reading(), 30_500));
        assert!(!save(&mut learn, &other(), 31_500));
        assert!(!save(&mut learn, &reading(), 31_600));
        // The spacing is unchanged: a run 1 s after the last save waits for the 2 s.
        assert!(!save(&mut learn, &other(), 40_000));
        assert!(!save(&mut learn, &other(), 40_100));
        assert!(save(&mut learn, &other(), 40_500));
        assert!(!save(&mut learn, &other(), 41_000));
        assert!(save(&mut learn, &other(), 43_100));
        // Two reads 600 ms apart: both conditions hold on the second.
        assert!(!save(&mut learn, &reading(), 52_000));
        assert!(!save(&mut learn, &other(), 52_000));
        assert!(save(&mut learn, &other(), 52_600));
        assert_eq!(full_rows(&log).len(), 5);
        // The result screen (on show for about 0.95 s): two reads in a row 145 ms apart are enough (14-06-38's
        // full-other-0071); one alone is not.
        let mut on_result = |learn: &mut Learn, r: &Reading, at: i64| learn.full_frame(&mut log, &screen((at / 1_000) as usize), r, at, "result").is_some();
        assert!(!on_result(&mut learn, &reading(), 60_000));
        assert!(!on_result(&mut learn, &other(), 61_000));
        assert!(on_result(&mut learn, &other(), 61_145));
        assert!(!on_result(&mut learn, &reading(), 64_000));
        assert!(!on_result(&mut learn, &other(), 65_000));
        assert!(!on_result(&mut learn, &reading(), 65_100));
        assert_eq!(full_rows(&log).len(), 6);
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn full_frames_keep_the_free_space_threshold() {
        let root = temp("full-space");
        // The general observe mode alone still keeps the threshold (observe mode's jpegs_allowed does not look at the
        // disk when observe mode is off).
        let mut learn = Learn::new(Some(root.clone()), false, false).with_free_space(scarce);
        learn.set_observe(true);
        let mut log = RunLog::new(root.clone());
        // Each after a first read that only starts the run of unread reads.
        for at in [-500, 0] {
            assert_eq!(learn.full_frame(&mut log, &screen(0), &other(), at, "board"), None);
        }
        assert!(full_rows(&log).is_empty());
        let mut unknown = Learn::new(Some(root.clone()), true, false).with_free_space(|_| None);
        for at in [-500, 0] {
            assert_eq!(unknown.full_frame(&mut log, &screen(0), &other(), at, "board"), None);
        }
        let mut zero = Learn::new(Some(root.clone()), true, false).with_free_space(scarce).with_min_free(0);
        assert_eq!(zero.full_frame(&mut log, &screen(0), &other(), -500, "board"), None);
        assert!(zero.full_frame(&mut log, &screen(0), &other(), 0, "board").is_some());
        log.wait();
        assert!(std::fs::read_dir(log.dir()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".jpg")).count() == 1);
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The report export never picks the whole frames of unread boards: neither as a moment's file nor as a shot.
    #[test]
    fn the_report_leaves_full_frames_out() {
        use super::super::report::{Game, Request, encode_jpeg, prepare, vorax_moments};
        crate::lang::set(crate::lang::Lang::En);
        let dir = temp("full-report");
        std::fs::create_dir_all(&dir).unwrap();
        let img = Rgba { w: 1280, h: 720, data: (0..1280 * 720 * 4).map(|k| (k * 31 % 251) as u8).collect() };
        let jpeg = encode_jpeg(&img, 80).unwrap();
        for f in ["frame-0001.jpg", "full-other-0002.jpg", "full-other-0003.jpg"] {
            std::fs::write(dir.join(f), &jpeg).unwrap();
        }
        let at = |s: &str| format!("2026-11-06T10:00:{s}Z");
        let lines = [
            json!({"at": at("01.000"), "kind": "vorax_read", "steady": true, "gate": false, "signature": "a", "frame": "frame-0001.jpg", "reading": {"kind": "potion", "round": null}}),
            json!({"at": at("01.500"), "kind": "vorax_read", "steady": false, "gate": false, "signature": "b", "frame": null, "reading": {"kind": "other"}}),
            json!({"at": at("01.500"), "kind": "vorax_full_frame", "file": "full-other-0002.jpg", "reason": "kind_other", "mode": "board", "index": 1}),
            json!({"at": at("02.000"), "kind": "vorax_round", "phase": "potion", "chosen": {"card": 0}}),
            json!({"at": at("04.000"), "kind": "vorax_full_frame", "file": "full-other-0003.jpg", "reason": "kind_other", "mode": "result", "index": 2}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(dir.join("log.jsonl"), &text).unwrap();
        assert!(vorax_moments(&text).iter().all(|m| m.files.iter().all(|f| !f.starts_with(FULL_PREFIX))));
        let req = Request { dir: dir.clone(), game: Game::Vorax, settled: None, install_id: String::new(), app_dir: None };
        let p = prepare(&req, &std::sync::atomic::AtomicBool::new(false), &|_| {}).unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(p.zip.clone())).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
        assert!(names.iter().any(|n| n == "frame-0001.jpg"), "{names:?}");
        assert!(names.iter().all(|n| !n.starts_with(FULL_PREFIX)), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn free_space_reads_the_disk() {
        let free = free_space(&std::env::temp_dir().join("no-such-folder-here").join("deeper"));
        assert!(free.is_some_and(|b| b > 0), "{free:?}");
    }
}
