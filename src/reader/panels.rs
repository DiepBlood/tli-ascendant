//! The text panels of the S16 prison screen (docs/s16-screen.md): which
//! screen is up, the destination cards, the decision buttons, the days
//! counter and the instrument or treasure offers, read with the Windows OCR
//! engine from fixed regions of the 1918x1078 frame. The Chromatic Prisoner
//! slots and the instrument icons are read from pixels.

use super::instruments::read_held;
use super::ocr::{Line, ocr, ocr_all, ocr_tiled};
use super::scene::{beast_at, confirm_lit, pick_shown, target_jaws_shown};
use super::layout::Layout;
use super::{Rect, Rgba};
use crate::engine::data::{Beast, Gift, Tool, data};
use crate::engine::incidents::USURPATION_I;
use crate::engine::text::{incident_name, instrument, theme_name, tool_name};
use crate::lang;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// "Select a Destination" with the destination cards.
    Patrol,
    /// "Make Your Decision" with the Consume / Replace / End buttons.
    Manage,
    /// "Select an Inmate to Consume": a Consume target pick.
    ChainTarget,
    /// "Select a Torture Instrument".
    InstrumentPick,
    /// "Warden's Treasure obtained": the Chromatic reward pick.
    TreasurePick,
    /// View Prison Status over a pick: the board with the counter and the
    /// vault, a "Return to Selection" button and no decision on it.
    PrisonStatus,
    /// The Interrogation Room's Instigate and Bestow cards under the board.
    Interrogation,
    /// "Select an Objective" under the board: the Sewer Ratway's pick of the
    /// cell the warden moves to, or the Hunt's pick of the Lion's prey.
    CellPick,
    /// The "Day N" plaque over a dimmed board.
    DayBanner,
    /// The run theme scroll shown once in the lobby before a run, titled
    /// with the theme's name.
    Theme,
    /// A game dialog over the board: the network timeout or connecting
    /// notice, or a Prison Incident popup.
    Popup,
    /// "A Riot has broken out in Tartarus" with the Ignore and Suppress
    /// buttons: the run is over.
    RunEnd,
    /// None of the above, such as the board between moves.
    Unknown,
}

impl Screen {
    /// The screen name used by the labels in data/scan-truth-s16.json.
    pub fn label(self) -> &'static str {
        match self {
            Screen::Patrol | Screen::Manage | Screen::Unknown => "board",
            Screen::ChainTarget => "chainTarget",
            Screen::InstrumentPick => "instrumentPick",
            Screen::TreasurePick => "treasurePick",
            Screen::PrisonStatus => "prisonStatus",
            Screen::Interrogation => "interrogation",
            Screen::CellPick => "cellPick",
            Screen::DayBanner => "dayBanner",
            Screen::Theme => "theme",
            Screen::Popup => "popup",
            Screen::RunEnd => "runEnd",
        }
    }
}

/// A filled Chromatic Prisoner slot: the beast when its portrait matches a
/// board template, with the match confidence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VaultSlot {
    pub beast: Option<Beast>,
    pub conf: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Panels {
    pub screen: Screen,
    /// "N day(s) until Riot".
    pub days: Option<u32>,
    /// Destination cards in order, as screen cell numbers (1 = Go to
    /// Interrogation Room); None for a card whose text did not read.
    pub doors: Vec<Option<u8>>,
    /// Buttons on screen: "Consume", "Replace", "Hunt", "End", or "Cancel" on a
    /// target pick.
    pub decision: Vec<&'static str>,
    /// The instruments on an instrument pick, in card order.
    pub instruments: Vec<Tool>,
    /// The re-rolls left on an instrument pick ("Remaining attempts N" over
    /// the Re-roll button); 0 without the button and on every other screen.
    pub rerolls: u8,
    /// Filled Chromatic Prisoner slots.
    pub vault: Vec<VaultSlot>,
    /// The Torture Instrument panel shows at least one icon.
    pub tool_icons: bool,
    /// The theme the run theme scroll names; None on every other screen.
    pub theme: Option<u32>,
    /// The Prison Incident a popup names; None on every other screen.
    pub incident: Option<u32>,
    /// The run objective the objective panel names, as (id, progress); see
    /// [`objective`].
    pub objective: Option<(u32, u32)>,
    /// The instruments the panel icons name; None off the board screens or
    /// when an icon is unsure.
    pub held: Option<Vec<Tool>>,
    /// What can be clicked on this screen, with the layout it was read from
    /// and its centre on the reference frame. The board cells are not here:
    /// see [`super::scene::portrait`].
    pub spots: Vec<(Spot, Layout, (i32, i32))>,
    /// A treasure pick with a gift picked: its Confirm button lit, see
    /// [`super::scene::confirm_lit`]; false on every other screen.
    pub gift_picked: bool,
    /// Per decision card place ([`CARD_X`]), whether a card there shows
    /// Consume's red art; None where no card shows clearly. See [`card_red`].
    pub card_red: [Option<bool>; 7],
    /// [`check_doors`] changed the door numbers read (re-read, inferred or cleared): this row must be
    /// read identically several times in a row before it is adopted, see `DOORS_SETTLE` in the session.
    pub doors_fixed: bool,
    /// Decision buttons in `decision` whose names went unread, named only because the instrument icons read showed
    /// no Blood-Stained Trap (see [`cards_unread`]): the session checks them against the instruments it knows held.
    pub by_icons: Vec<&'static str>,
}

/// The centres of the decision cards, as their names' (see [`ZH_BUTTON_X`]):
/// one card at `[3]`, two at `[2]` and `[4]`, three at `[1]`, `[3]` and
/// `[5]`, four at `[0]`, `[2]`, `[4]` and `[6]`.
pub const CARD_X: [i32; 7] = ZH_BUTTON_X;

/// The places in [`CARD_X`] of `n` decision cards, left to right.
pub fn card_places(n: usize) -> &'static [usize] {
    match n {
        1 => &[3],
        2 => &[2, 4],
        3 => &[1, 3, 5],
        4 => &[0, 2, 4, 6],
        _ => &[],
    }
}

/// Where a decision card is clicked, at place `k` of [`CARD_X`].
pub fn card_spot(k: usize) -> (i32, i32) {
    (CARD_X[k], ZH_BUTTON_Y - BUTTON_ABOVE)
}

/// The least brightness (mean R + G + B) of a decision card's art for its
/// colour to tell: 0.25 s after the cards come up on 81 samples. The empty
/// floor behind reads 50 to 60.
const CARD_SHOWN: f64 = 90.0;
/// Consume's red art, as red over R + G + B: 0.45 to 0.53 once shown, where
/// Replace and End read 0.32 to 0.39 (294 frames of 2026-10-29).
const CARD_RED: f64 = 0.44;
const CARD_NOT_RED: f64 = 0.40;

/// Whether the decision card at place `k` of [`CARD_X`] shows Consume's red
/// art, by its art left and right of the icon, which shows its colour while
/// the cards still fade in and their names read too dark (2026-10-29: 7 reads
/// took a fading Replace as used); None while too dark or unclear. Replace
/// and Hunt look alike, so only red tells.
fn card_red(img: &Rgba, k: usize) -> Option<bool> {
    let x = CARD_X[k];
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for rect in [Rect::new(x - 150, 770, 90, 70), Rect::new(x + 60, 770, 90, 70)] {
        for p in img.crop(rect).data.as_chunks::<4>().0 {
            for c in 0..3 {
                sum[c] += u64::from(p[c]);
            }
            n += 1;
        }
    }
    let total = sum.iter().sum::<u64>() as f64 / n.max(1) as f64;
    if total < CARD_SHOWN {
        return None;
    }
    let red = sum[0] as f64 / n as f64 / total;
    if red >= CARD_RED {
        Some(true)
    } else if red < CARD_NOT_RED {
        Some(false)
    } else {
        None
    }
}

/// Something the player clicks on a decision screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spot {
    /// A destination card by the screen cell its door leads to (1 = the
    /// Interrogation Room).
    Door(u8),
    /// "Consume", "Replace", "Hunt", "End", "Cancel" or "Re-roll".
    Button(&'static str),
    /// A pick card by its key: an instrument's, "instigate" or "bestow".
    Card(String),
}

impl Panels {
    /// The prison board is not on screen to read: no decision, dialog or riot
    /// title was found and the days counter, which every board screen but the
    /// Consume target pick shows, did not read (a loading screen, the
    /// hideout), or a day banner hides the counter.
    pub fn off_board(&self) -> bool {
        matches!(self.screen, Screen::Unknown | Screen::DayBanner) && self.days.is_none()
    }
}

// Regions (x, y, w, h) at 1918x1078 and the upscale each reads best at.
const DAYS: (Rect, f32) = (Rect::new(800, 0, 320, 60), 2.0);
const TITLE: (Rect, f32) = (Rect::new(0, 70, 1918, 60), 1.5);
const HEADING: (Rect, f32) = (Rect::new(420, 680, 1100, 40), 2.0);
/// Wide enough for the four decision cards of a Hunt day, whose Consume and
/// End names sit at about x 440 and 1476 (hunt-run-0005).
const CARDS: (Rect, f32) = (Rect::new(360, 740, 1160, 200), 1.5);
const TARGET: (Rect, f32) = (Rect::new(700, 800, 520, 50), 2.0);
const CANCEL: (Rect, f32) = (Rect::new(1600, 740, 318, 140), 2.0);
const BANNER: (Rect, f32) = (Rect::new(760, 400, 400, 130), 2.0);
/// The card names on the Select a Torture Instrument screen, one row across
/// the cards at y 480-515 (rec1-t031, a small font for "Ferocious Beast's
/// Ravenous Chains"); the hovered card is raised, its name at about y 435-470.
/// The descriptions start below about y 550.
const NAMES: (Rect, f32) = (Rect::new(0, 420, 1918, 125), 1.0);
/// "Remaining attempts N" over the Re-roll button; read from the title layout (see layout's TITLE_ZONES).
const REROLLS: (Rect, f32) = (Rect::new(640, 880, 320, 45), 2.0);
// The game's dialogs: "Tip" over "Network request timeout", and the title
// of a Prison Incident popup.
const DIALOG: (Rect, f32) = (Rect::new(560, 340, 800, 260), 1.0);
const INCIDENT: (Rect, f32) = (Rect::new(1100, 190, 700, 70), 1.0);
/// The title band of the run theme scroll, inside the DIALOG region.
const THEME: (Rect, f32) = (Rect::new(660, 310, 600, 70), 1.0);
/// The objective panel at the top left of the board: the name, the task and
/// the progress ("0/2") or "Completed". A name on two lines starts higher.
const OBJECTIVE: (Rect, f32) = (Rect::new(30, 110, 370, 140), 1.5);
/// The "Return to Selection" button of View Prison Status, where a pick
/// shows "View Prison Status".
const STATUS: (Rect, f32) = (Rect::new(1540, 115, 300, 40), 2.0);
/// The title of the riot screen that ends a run.
const RIOT: (Rect, f32) = (Rect::new(400, 95, 1120, 80), 1.0);
/// Distance between the centres of two destination cards.
const CARD_PITCH: i32 = 345;
/// The middle of the destination cards' row, which spans y 750-920 with the
/// text at y 880 (rec1-t049).
const DOOR_Y: i32 = 835;
/// How far above its name a decision button's icon and name centre (the
/// names at y 880, the icons at y 810 on rec1-t055).
const BUTTON_ABOVE: i32 = 35;
/// The middle of the Select a Torture Instrument cards, y 187-870 with the
/// names at about y 500 (reroll-2000x837).
const PICK_Y: i32 = 528;
/// The Consume target pick's round Cancel button (rec1-t057).
const CANCEL_AT: (i32, i32) = (1724, 803);
/// The Re-roll button under "Remaining attempts N" (reroll-2000x837).
const REROLL_AT: (i32, i32) = (787, 959);

/// Chromatic Prisoner slots in fill order: two columns, top to bottom.
const SLOT_X: [i32; 2] = [1720, 1805];
const SLOT_Y: [i32; 5] = [240, 335, 430, 525, 620];
/// Mean luma over a slot's centre above which it holds a portrait: empty
/// slots measure 20 to 34 on the board.
const SLOT_FILLED: f32 = 60.0;
/// A darker slot down to this holds a portrait when its beast reads sure
/// (see [`SLOT_SURE`]): a dark Chromatic Wolf measures 46 (2026-10-28, whose
/// vault of 5 read as none), but a passing glow lifts empty slots to 47 too.
const SLOT_DIM: f32 = 40.0;
/// The beast confidence of a dim slot's portrait: portraits read 0.79 to 1,
/// the glowing empty slots of a 2026-10-28 frame 0.07 to 0.27 (and read as
/// a vault of 9).
const SLOT_SURE: f32 = 0.5;
/// The first instrument icon of the Torture Instrument panel.
const ICON: (i32, i32) = (122, 390);
/// Luma standard deviation over the first icon spot above which an icon is there.
const ICON_SPREAD: f32 = 30.0;

/// The fixed texts of the screen in the game's language, as the zh-TW client
/// writes them (the chinese/ screenshots); None where no screenshot shows the
/// text yet, so that screen is not told apart in Chinese.
struct Words {
    /// The instrument pick's titles: at the run start and as an objective's reward.
    instrument_pick: &'static [&'static str],
    /// The treasure pick's title, and a word of it that alone marks the pick.
    treasure: &'static str,
    treasure_word: &'static str,
    return_to_selection: Option<&'static str>,
    /// The "View Prison Status" button, which marks a Prison Incident popup
    /// once the picks above are ruled out; read only in Chinese, where most
    /// incident names are not known yet.
    view_status: Option<&'static str>,
    select_inmate: &'static str,
    cancel: &'static str,
    /// A dialog's "Tip" title, the network timeout and the connecting notice.
    tip: Option<&'static str>,
    timeout: Option<&'static str>,
    connecting: Option<&'static str>,
    select_objective: Option<&'static str>,
    select_destination: &'static str,
    make_decision: &'static str,
    riot: Option<&'static str>,
    instigate: &'static str,
    bestow: &'static str,
    /// The decision buttons in the order of [`BUTTONS`]; None for one not seen.
    buttons: [Option<&'static str>; 4],
    completed: &'static str,
    remaining_attempts: &'static str,
}

/// The decision buttons' names as the model and the click spots name them.
const BUTTONS: [&str; 4] = ["Consume", "Replace", "Hunt", "End"];

const EN: Words = Words {
    instrument_pick: &["Select a Torture Instrument"],
    treasure: "Warden's Treasure",
    treasure_word: "treasure",
    return_to_selection: Some("Return to Selection"),
    view_status: None,
    select_inmate: "Select an Inmate to Consume",
    cancel: "Cancel",
    tip: Some("Tip"),
    timeout: Some("Network request timeout"),
    connecting: Some("Connecting to server"),
    select_objective: Some("Select an Objective"),
    select_destination: "Select a Destination",
    make_decision: "Make Your Decision",
    riot: Some("A Riot has broken out in Tartarus"),
    instigate: "Instigate",
    bestow: "Bestow",
    buttons: [Some("Consume"), Some("Replace"), Some("Hunt"), Some("End")],
    completed: "Completed",
    remaining_attempts: "Remaining attempts",
};

const ZH_TW: Words = Words {
    instrument_pick: &["選擇一件刑具", "選擇刑具獎勵"],
    treasure: "獲得典獄長的珍藏",
    treasure_word: "珍藏",
    return_to_selection: Some("返回選擇"),
    view_status: Some("查看監獄狀態"),
    select_inmate: "請選擇要吃的囚犯",
    cancel: "取消",
    tip: None,
    timeout: None,
    connecting: None,
    select_objective: Some("請選擇目標"),
    select_destination: "請選擇目的地",
    make_decision: "請進行決策",
    riot: Some("塔羅斯發生暴動"),
    instigate: "煽動",
    bestow: "賞賜",
    buttons: [Some("吃"), Some("換"), Some("尋獵"), Some("結束")],
    completed: "已完成",
    remaining_attempts: "剩餘次數",
};

/// The screen's words in the set language.
fn words() -> &'static Words {
    if lang::zh() { &ZH_TW } else { &EN }
}

/// CJK ideographs, which the zh-TW game text is written in.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

/// Lowercase ASCII letters and digits, and CJK ideographs, only: spacing and
/// punctuation are the parts OCR gets wrong most. English OCR never gives
/// an ideograph, so English text squashes as it always did.
pub fn squash(s: &str) -> String {
    s.chars().filter(|&c| c.is_ascii_alphanumeric() || is_cjk(c)).map(|c| c.to_ascii_lowercase()).collect()
}

/// Levenshtein distance over chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != b[j])).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// 1 for the same text up to case, spacing and punctuation, falling with the
/// edit distance.
pub fn similarity(a: &str, b: &str) -> f32 {
    let (a, b) = (squash(a), squash(b));
    let n = a.chars().count().max(b.chars().count());
    if n == 0 {
        return 0.0;
    }
    1.0 - distance(&a, &b) as f32 / n as f32
}

/// The candidate closest to `text` and its similarity, when at least `min`.
pub fn best_match<S: AsRef<str>>(text: &str, candidates: &[S], min: f32) -> Option<(usize, f32)> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (i, similarity(text, c.as_ref())))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|&(_, s)| s >= min)
}

fn has(lines: &[Line], target: &str, min: f32) -> bool {
    lines.iter().any(|l| similarity(&l.text, target) >= min || squash(&l.text).contains(&squash(target)))
}

/// A number as OCR writes it: letters that look like digits count as digits.
fn number(token: &str) -> Option<u32> {
    let digits: String = token
        .chars()
        .map(|c| match c {
            'O' | 'o' | 'D' | 'Q' => '0',
            'I' | 'l' | '|' | 'i' => '1',
            'Z' | 'z' => '2',
            'S' | 's' => '5',
            'B' => '8',
            c => c,
        })
        .collect();
    digits.parse().ok()
}

/// "N day(s) until Riot": the number is what comes before "day", or before
/// the first space when "day" misreads. OCR can drop the space ("0day(s)")
/// and reads the red 0 as "()". On a frame scaled up from a short window the
/// number can come as its own line after the text, so the lines go left to
/// right.
fn days(lines: &[Line]) -> Option<u32> {
    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by_key(|l| l.rect.x);
    let text = joined(sorted);
    if lang::zh() {
        return days_zh(&text);
    }
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let (head, rest) = text.split_at(lower.find("day").or_else(|| lower.find(' '))?);
    let head = head.replace(' ', "").replace("()", "0");
    (similarity(rest, "day(s) until Riot") >= 0.6 || squash(rest).ends_with("riot")).then(|| number(&head))?
}

/// "距離暴動剩餘19天" (the zh-TW counter): the number before the last 天,
/// with the digit-like letters OCR gives for digits; the text must name the
/// Riot or the days left, so another line ending in 天 is not the counter.
fn days_zh(text: &str) -> Option<u32> {
    // OCR reads 天 as 夭 at some scales.
    let s: String = text.chars().filter(|c| !c.is_whitespace()).map(|c| if c == '夭' { '天' } else { c }).collect::<String>().replace("()", "0");
    // The red count of the last days can come without its 天 ("距離暴動剩餘2"):
    // then the count is what ends the line after 剩餘.
    let head = match s.rfind('天') {
        Some(at) => &s[..at],
        None if s.contains('餘') => s.as_str(),
        None => return None,
    };
    let digits: Vec<char> = head.chars().rev().take_while(|&c| c.is_ascii_digit() || matches!(c, 'O' | 'o' | 'I' | 'l' | '|' | 'i')).collect();
    let digits: String = digits.into_iter().rev().collect();
    ["距離", "暴動", "剩餘"].iter().any(|w| head.contains(w)).then(|| number(&digits))?
}

/// The zh-TW counter when OCR splits it after 剩餘 and drops its big thin
/// digits ("距離暴動剩餘" then "9 天" for 19, or nothing after it for 11):
/// the digits after 剩餘, read tiled twice, give "1 9 1 9", whose halves
/// agree. None when the counter reads as one line or the halves differ.
fn days_zh_split(lines: &[Line], img: &Rgba) -> Option<u32> {
    // A line that ends at 餘: its digits, if any, were dropped.
    let head = lines.iter().find(|l| squash(&l.text).ends_with('餘'))?;
    let x = head.rect.x + head.rect.w + 1;
    let [read] = ocr_tiled(img, &[(Rect::new(x, 2, 48, 46), 2.0)], 2);
    repeated_count(&read)
}

/// The count a tiled read of the counter's digits gives ("1 4 1 4"): the
/// digits and digit-like letters, when their two halves agree.
fn repeated_count(read: &[Line]) -> Option<u32> {
    let digits: String = joined(read).chars().filter(|&c| c.is_ascii_digit() || matches!(c, 'O' | 'o' | 'I' | 'l' | 'i')).collect();
    let n = digits.len();
    (n >= 2 && n.is_multiple_of(2) && digits[..n / 2] == digits[n / 2..]).then(|| number(&digits[..n / 2]))?
}

/// The zh-TW counter read whole but for its thin "1"s ("距離暴動剩餘4夭" for
/// 14): the digits left of 天, tiled, in three crops measured from the right
/// end of the line holding 天 (2026-10-28_04-36-58 frame 37: "14 14"). Taken
/// only when it ends in the digit the line `read` gives, so a crop that
/// catches 天 or 餘 instead of the count changes nothing.
fn days_zh_tiled(lines: &[Line], img: &Rgba, read: Option<u32>) -> Option<u32> {
    let line = lines.iter().rev().find(|l| l.text.contains('天') || l.text.contains('夭'))?;
    let right = line.rect.x + line.rect.w;
    let crops = [(64, 24), (52, 28), (56, 26)].map(|(w, back)| (Rect::new(right - back - w, 2, w, 46), 2.0));
    ocr_tiled(img, &crops, 2)
        .iter()
        .filter_map(|r| repeated_count(r))
        .find(|&n| n <= 30 && read.is_none_or(|m| n % 10 == m % 10))
}

/// A game dialog: the "Tip" box of a network timeout, the connecting notice,
/// or a Prison Incident popup titled with the incident's name.
fn popup(dialog: &[Line], incident: &[Line]) -> bool {
    let w = words();
    let names: Vec<&str> = data().incidents.iter().map(|e| incident_name(e.id)).collect();
    w.tip.is_some_and(|tip| dialog.iter().any(|l| squash(&l.text) == squash(tip)))
        || w.timeout.is_some_and(|t| has(dialog, t, 0.75))
        || w.connecting.is_some_and(|t| has(dialog, t, 0.75))
        || incident.iter().any(|l| best_match(&l.text, &names, 0.75).is_some())
}

/// The Prison Incident a popup's title names. The Usurpation stages differ
/// only in their numeral ("僭越·其三", "Usurpation III"), which picks the stage.
fn incident_named(title: &[Line]) -> Option<u32> {
    let incidents = &data().incidents;
    let names: Vec<&str> = incidents.iter().map(|e| incident_name(e.id)).collect();
    let (line, k) = title.iter().find_map(|l| best_match(&l.text, &names, 0.75).map(|(k, _)| (l, k)))?;
    let id = incidents[k].id;
    if !(USURPATION_I..USURPATION_I + 4).contains(&id) {
        return Some(id);
    }
    let text = squash(&line.text);
    let stage = if lang::zh() {
        let numeral = text.chars().last()?;
        ['一', '二', '三', '四'].iter().position(|&c| c == numeral)?
    } else {
        ["iv", "iii", "ii", "i"].iter().position(|n| text.ends_with(n)).map(|k| 3 - k)?
    };
    Some(USURPATION_I + stage as u32)
}

/// The theme a run theme scroll's title names.
fn theme(title: &[Line]) -> Option<u32> {
    data().themes.iter().find(|t| has(title, theme_name(Some(t.id)).unwrap_or(&t.name), 0.75)).map(|t| t.id)
}

/// The objective the panel names, as (id, progress). The name can wrap onto
/// two lines, so the panel's text joined also counts. "Completed" is the
/// objective's target (1 for a rating objective). When the name reads but the
/// progress does not, the progress is 0: the objective still steers the
/// solve, and a later read corrects the count.
fn objective(lines: &[Line]) -> Option<(u32, u32)> {
    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by_key(|l| l.rect.y);
    let text = squash(&joined(sorted.iter().copied()));
    let d = data();
    // The name as the game writes it: "Objective: Prison Walk", "目標：巡獄之路".
    let name = |id: u32, en: &'static str| if lang::zh() { d.zh.objectives.get(&id).map_or(en, String::as_str) } else { en };
    let goal = d.objectives.iter().find(|o| {
        let name = name(o.id, &o.name);
        has(lines, name, 0.75) || text.contains(&squash(name))
    })?;
    Some((goal.id, lines.iter().find_map(|l| progress(&l.text, goal.target)).unwrap_or(0)))
}

/// A progress line: "N/M" gives N, "Completed" gives `target`. "Completed"
/// ends the task's line ("Move 18 tiles Completed"), and OCR can glue it to
/// the word before ("UltimateCompleted"), so the last word or its tail counts.
/// OCR glues the count to the task too ("times0/2") and can read its 1 as an
/// l ("timesl/2"), so the digits and digit-like letters next to the slash
/// count first, then the whole word read as a number. No task's last word
/// ends in one of those letters. The count after the slash is the known
/// target, so only a count above it is refused.
fn progress(text: &str, target: u32) -> Option<u32> {
    let completed = words().completed;
    if similarity(text, completed) >= 0.75 || completed_tail(text, completed) {
        return Some(target);
    }
    let (head, _) = text.trim().rsplit_once('/')?;
    let head = head.split_whitespace().last()?;
    let count = head.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, 'O' | 'o' | 'I' | 'l' | '|' | 'i'));
    let done = number(if count.len() < head.len() { &head[count.len()..] } else { head })?;
    (done <= target).then_some(done)
}

/// Whether the line's last word, or its last letters (one fewer to one more
/// than `completed` has: 8 to 10 for "Completed"), reads as `completed`.
fn completed_tail(text: &str, completed: &str) -> bool {
    // Chinese OCR spaces every character apart ("格 已 完 成"), so a Chinese
    // word is the whole line's tail.
    let word = if completed.is_ascii() { text.split_whitespace().last() } else { Some(text) };
    let Some(word) = word else { return false };
    let chars: Vec<char> = squash(word).chars().collect();
    let len = completed.chars().count();
    (len - 1..=len + 1).filter(|&n| n <= chars.len()).any(|n| {
        let tail: String = chars[chars.len() - n..].iter().collect();
        similarity(&tail, completed) >= 0.75
    })
}

/// The re-rolls left on an instrument pick: the N of "Remaining attempts N".
/// OCR can glue the count to the word and read its 1 as an l
/// ("Remaining attemptsl"). The game shows the text only with re-rolls left,
/// so a text whose count does not read gives 1; no text gives 0.
fn rerolls(lines: &[Line]) -> u8 {
    lines
        .iter()
        .find_map(|l| {
            let text: String = l.text.chars().filter(|c| !c.is_whitespace()).collect();
            let words = text.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, 'O' | 'o' | 'I' | 'l' | '|' | 'i'));
            (similarity(words, self::words().remaining_attempts) >= 0.75).then(|| number(&text[words.len()..]).filter(|&n| n > 0).map_or(1, |n| u8::try_from(n).unwrap_or(u8::MAX)))
        })
        .unwrap_or(0)
}

/// One destination card's text: "Go to Cell N" or "Go to Interrogation Room".
fn door(text: &str) -> Option<u8> {
    if lang::zh() {
        return door_zh(text);
    }
    let s = squash(text);
    if s.contains("room") || similarity(text, "Go to Interrogation Room") >= 0.6 {
        return Some(1);
    }
    let tail: String = match s.rfind("cell") {
        // OCR reads the digits 1 and 0 as I, l and O ("Go to Cell IL").
        Some(at) => s[at + 4..].chars().map(|c| match c {
            'i' | 'l' => '1',
            'o' => '0',
            c => c,
        }).collect(),
        None => s.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect(),
    };
    tail.parse().ok().filter(|n| (2..=16).contains(n))
}

/// One zh-TW destination card's text: "去N號" or "去審訊室". OCR reads the
/// digits 1 and 0 as I, l and O.
fn door_zh(text: &str) -> Option<u8> {
    let s = squash(text);
    if s.contains("審訊") || s.contains("訊室") {
        return Some(1);
    }
    let digits: String = s
        .chars()
        .filter_map(|c| match c {
            '0'..='9' => Some(c),
            'i' | 'l' => Some('1'),
            'o' => Some('0'),
            _ => None,
        })
        .collect();
    digits.parse().ok().filter(|n| (2..=16).contains(n))
}

/// The cards sit in a centred row, as many as today's doors (two with Burning
/// Stake): lines whose centres are within half a card pitch are one card. OCR
/// can drop "Go" or put junk before it and garble "Cell" ("eGo to Cell 11",
/// "to celgl"), so "to" and "cel" also mark card text. A card whose text
/// doesn't parse goes to `reread` with its centre. Each card comes with the
/// centre of its first line.
fn doors(lines: &[Line], reread: impl Fn(i32) -> Option<u8>) -> Vec<(Option<u8>, i32)> {
    let mut sorted: Vec<&Line> = lines
        .iter()
        .filter(|l| {
            let s = squash(&l.text);
            let card = if lang::zh() { s.contains('去') || s.contains('號') } else { s.starts_with("go") || s.starts_with("to") || s.contains("cel") };
            card || door(&l.text).is_some()
        })
        .collect();
    sorted.sort_by_key(|l| l.rect.centre().0);
    let mut cards: Vec<Vec<&Line>> = Vec::new();
    for l in sorted {
        match cards.last_mut() {
            Some(card) if l.rect.centre().0 - card[0].rect.centre().0 < CARD_PITCH / 2 => card.push(l),
            _ => cards.push(vec![l]),
        }
    }
    cards
        .into_iter()
        .map(|mut card| {
            card.sort_by_key(|l| l.rect.y);
            let x = card[0].rect.centre().0;
            (door(&joined(card.iter().copied())).or_else(|| reread(x)), x)
        })
        .collect()
}

fn joined<'a>(lines: impl IntoIterator<Item = &'a Line>) -> String {
    lines.into_iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// The card centred near `x` read alone at larger scales: the card under the
/// mouse is raised and dimmed, and its "Cell 11" reads as "celV1" or "celgl"
/// at the row's 1.5x but as "Cell 11" at 2.5x or 3x.
fn reread(img: &Rgba, x: i32) -> Option<u8> {
    reread_if(img, x, |_| true)
}

/// [`reread`] taking only a door `ok` accepts.
fn reread_if(img: &Rgba, x: i32, ok: impl Fn(u8) -> bool) -> Option<u8> {
    let rect = Rect::new(x - CARD_PITCH / 2, 840, CARD_PITCH - 15, 80);
    [2.5, 3.0].iter().find_map(|&scale| door(&joined(&ocr(img, rect, scale))).filter(|&d| ok(d)))
}

/// The zh-TW card centred at `x` read alone: first as its pixels' smallest
/// channel, where a red or blue glow behind the white text goes dark ("去13號"
/// reads as "去1號" over a red glow, as "去13號" so), then as it is, then as
/// black text on white; each in three bands, as the card under the mouse is
/// raised and dimmed.
fn reread_zh(img: &Rgba, x: i32) -> Option<u8> {
    reread_zh_if(img, x, |_| true)
}

/// [`reread_zh`] taking only a door `ok` accepts: each way of reading is tried
/// until one gives such a door.
fn reread_zh_if(img: &Rgba, x: i32, ok: impl Fn(u8) -> bool) -> Option<u8> {
    [860, 850, 855].iter().find_map(|&top| {
        let crop = img.crop(Rect::new(x - CARD_PITCH / 2 + 7, top, CARD_PITCH - 15, 50));
        let gray = min_channel(&crop);
        let ink = text_ink(&crop);
        let whole = Rect::new(0, 0, crop.w as i32, crop.h as i32);
        [(&gray, 2.0), (&gray, 3.0), (&crop, 3.0), (&crop, 2.0), (&ink, 2.0)].iter().find_map(|&(im, scale)| door(&joined(&ocr(im, whole, scale))).filter(|&d| ok(d)))
    })
}

/// The smallest channel of white text and its glow's edge, above which a
/// pixel is text (see [`text_ink`]): 120 and 150 read "去12號" alike, 180
/// left nothing.
const TEXT_INK: u8 = 135;

/// White text as black on white: "去12號" under a portrait read "去1號" every
/// other way, 15 tries in all (2026-10-28).
fn text_ink(img: &Rgba) -> Rgba {
    let data = img.data.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| {
        let v = if r.min(g).min(b) >= TEXT_INK { 0 } else { 255 };
        [v, v, v, 255]
    });
    Rgba { w: img.w, h: img.h, data: data.collect() }
}

/// The zh-TW destination cards read one by one at the three- and two-card
/// places, for a destination screen whose row read gave none (every card
/// with a portrait, or the one under the mouse, can drop out of it).
fn doors_alone(img: &Rgba) -> Vec<(Option<u8>, i32)> {
    let read = |places: &[i32]| -> Vec<(Option<u8>, i32)> { places.iter().filter_map(|&c| reread_zh(img, c).map(|door| (Some(door), c))).collect() };
    let three = read(&THREE_CARDS);
    if three.is_empty() { read(&TWO_CARDS) } else { three }
}

/// Each pixel's smallest channel as gray.
fn min_channel(img: &Rgba) -> Rgba {
    let data = img.data.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| {
        let m = r.min(g).min(b);
        [m, m, m, 255]
    });
    Rgba { w: img.w, h: img.h, data: data.collect() }
}

/// The destination cards' centres: three cards, or two with one die fewer.
const THREE_CARDS: [i32; 3] = [614, 959, 1305];
const TWO_CARDS: [i32; 2] = [786, 1130];

/// Whether the two patrol door cards sit at the centred two-card positions: that is a complete row
/// (an instrument with one option fewer such as the Burning Stake, or one die fewer rolled that day),
/// with no third card to come. When three cards fade in, the two read first sit at the three-card
/// positions, 171 pixels or more away from the two-card positions. `xs` are the door card centres
/// (reference coordinates).
pub fn two_card_row(xs: &[i32]) -> bool {
    let mut xs = xs.to_vec();
    xs.sort_unstable();
    xs.len() == 2 && xs.iter().zip(TWO_CARDS).all(|(&x, c)| (x - c).abs() < CARD_PITCH / 4)
}

/// Check of the patrol door cards. Left to right, the door cards run from nearest to farthest from the
/// Warden (1-6 steps clockwise): of 11,415 three-door reads in the logged history, 11,380 were so, and
/// the rest were misreads. A card whose door number was read but is unreachable, or does not fit this
/// order, is read once more on its own, and only a door number that fits is accepted; if that still
/// fails and only one door number fits between the cards before and after it, that one is inferred;
/// otherwise it counts as not read (wait as before, warn as before). A position whose door number was
/// not read is neither re-read nor inferred: the card may not have flipped yet. On 2026-11-04 14-46-54,
/// day 6, the third card `去13號` was read every time as 7, where the Warden stands, so it was dropped
/// as unreachable and the advice looked at only two doors.
/// `warden` is the Warden's cell number (screen numbering, 1 = Interrogation Room).
pub fn check_doors(img: &Rgba, panels: &mut Panels, warden: u8) {
    if panels.screen != Screen::Patrol {
        return;
    }
    let near = |a: i32, b: i32| (a - b).abs() < CARD_PITCH / 4;
    let read: Vec<(u8, i32)> = panels.spots.iter().filter_map(|(spot, _, (x, _))| if let Spot::Door(d) = spot { Some((*d, *x)) } else { None }).collect();
    let xs: Vec<i32> = read.iter().map(|&(_, x)| x).collect();
    let layout: &[i32] = if xs.iter().any(|&x| THREE_CARDS.iter().any(|&c| near(c, x))) {
        &THREE_CARDS
    } else if xs.iter().any(|&x| TWO_CARDS.iter().any(|&c| near(c, x))) {
        &TWO_CARDS
    } else {
        return;
    };
    // The door number read at each position and its centre (None if there is no card or nothing was read).
    let found: Vec<Option<(u8, i32)>> = layout.iter().map(|&c| read.iter().find(|&&(_, x)| near(x, c)).copied()).collect();
    let mut slots: Vec<Option<u8>> = found.iter().map(|f| f.map(|(d, _)| d)).collect();
    let read_at: Vec<bool> = slots.iter().map(Option::is_some).collect();
    // All cards of the row are present (including those whose door number cannot be read): positions
    // not read stay None, and warn as before.
    let whole_row = panels.doors.len() == layout.len();
    let step = |d: u8| (u16::from(d) + 16 - u16::from(warden)) % 16;
    let reach = |d: u8| (1..=6).contains(&step(d));
    // Positions that fit: the largest set that is reachable and whose step counts increase from left
    // to right (when several sets are equally large, keep only the positions present in every set).
    let n = slots.len();
    let fits = |mask: usize, slots: &[Option<u8>]| {
        let mut last = 0;
        (0..n).filter(|i| mask & (1 << i) != 0).all(|i| match slots[i] {
            Some(d) if reach(d) && step(d) > last => {
                last = step(d);
                true
            }
            _ => false,
        })
    };
    let good = |slots: &[Option<u8>]| -> Vec<bool> {
        let best = (0..1usize << n).filter(|&m| fits(m, slots)).map(|m| m.count_ones()).max().unwrap_or(0);
        let tops: Vec<usize> = (0..1usize << n).filter(|&m| m.count_ones() == best && fits(m, slots)).collect();
        (0..n).map(|i| !tops.is_empty() && tops.iter().all(|m| m & (1 << i) != 0)).collect()
    };
    // The step range of position i (exclusive at both ends): the nearest fitting cards on each side,
    // minus the one step taken by each position between them and i (those positions also hold cards,
    // just not read yet or not flipped yet).
    let bounds = |i: usize, slots: &[Option<u8>], ok: &[bool]| {
        let left = (0..i).rev().find(|&j| ok[j]);
        let right = (i + 1..n).find(|&j| ok[j]);
        let lo = left.and_then(|j| slots[j]).map_or(0, step) + (i - left.map_or(0, |j| j + 1)) as u16;
        let hi = right.and_then(|j| slots[j]).map_or(7, step).saturating_sub((right.unwrap_or(n) - i - 1) as u16);
        (lo, hi)
    };
    let mut ok = good(&slots);
    if (0..n).all(|i| ok[i] || !read_at[i]) {
        return;
    }
    for i in 0..n {
        if ok[i] || !read_at[i] {
            continue;
        }
        let (lo, hi) = bounds(i, &slots, &ok);
        let fit = |d: u8| reach(d) && lo < step(d) && step(d) < hi;
        let again = if lang::zh() { reread_zh_if(img, layout[i], fit) } else { reread_if(img, layout[i], fit) };
        // If the re-read yields nothing, keep the original: once other positions are fixed it may fit.
        if again.is_some() {
            slots[i] = again;
            ok = good(&slots);
        }
    }
    let wrong: Vec<usize> = (0..n).filter(|&i| read_at[i] && !ok[i]).collect();
    if let [i] = wrong[..] {
        let (lo, hi) = bounds(i, &slots, &ok);
        if hi.checked_sub(lo) == Some(2) {
            let s = lo + 1;
            slots[i] = Some(((u16::from(warden) + s - 1) % 16 + 1) as u8);
        }
    }
    let ok = good(&slots);
    for i in 0..n {
        if !ok[i] {
            slots[i] = None;
        }
    }
    let doors: Vec<Option<u8>> = (0..n).filter(|&i| whole_row || read_at[i]).map(|i| slots[i]).collect();
    panels.doors_fixed = doors != panels.doors;
    panels.doors = doors;
    panels.spots.retain(|(spot, _, _)| !matches!(spot, Spot::Door(_)));
    let x = |i: usize| found[i].map_or(layout[i], |(_, x)| x);
    panels.spots.extend((0..n).filter_map(|i| Some((Spot::Door(slots[i]?), Layout::Board, (x(i), DOOR_Y)))));
}

/// The zh-TW cards with those the row's read missed read again alone at 3x
/// (a card with a portrait, "去7號", often reads as nothing in the row). The
/// layout comes from a card the row read; a card missed again is left out,
/// as a card fewer can be a die fewer.
fn doors_zh(found: Vec<(Option<u8>, i32)>, img: &Rgba) -> Vec<(Option<u8>, i32)> {
    let near = |a: i32, b: i32| (a - b).abs() < CARD_PITCH / 4;
    let Some(&(_, x)) = found.first() else { return found };
    let layout: &[i32] = if THREE_CARDS.iter().any(|&c| near(c, x)) {
        &THREE_CARDS
    } else if TWO_CARDS.iter().any(|&c| near(c, x)) {
        &TWO_CARDS
    } else {
        return found;
    };
    layout
        .iter()
        .filter_map(|&c| match found.iter().find(|f| near(f.1, c)) {
            Some(&card) => Some(card),
            None => reread_zh(img, c).map(|door| (Some(door), c)),
        })
        .collect()
}

/// Where a zh-TW decision button's name can sit: the centres of the two-,
/// three- and four-button rows, at name height.
const ZH_BUTTON_X: [i32; 7] = [440, 614, 786, 959, 1130, 1305, 1476];
const ZH_BUTTON_Y: i32 = 878;
/// The tops of the bands a zh-TW button's name is read in: the names of a
/// three-button row, and the one under the mouse, sit about 10 px higher.
const ZH_BUTTON_TOPS: [i32; 2] = [860, 845];
/// The brightness (99th percentile luma over both bands) under which a
/// button's name is grayed out, the button used today: measured 103 for a
/// used Replace, 134 to 140 for a name dimmed under the mouse and 176 to 196
/// for the others (run 2026-10-28_03-58-17).
const ZH_BUTTON_LIT: u32 = 120;
/// Consume's red name pressed reads dark (99th percentile luma 93, red 135,
/// red over green and blue 59) but is no used gray: a gray name has no more
/// red than green, and the card's red art behind it reaches red 75 (run
/// 2026-10-28_16-47-59, whose last day advised ending the run as the
/// player clicked Consume).
const ZH_BUTTON_RED: u32 = 125;
const ZH_BUTTON_REDDER: u32 = 40;

/// Whether the zh-TW button name at `x` is lit, not grayed out by use.
fn button_lit(img: &Rgba, x: i32) -> bool {
    let band = img.crop(Rect::new(x - 45, ZH_BUTTON_TOPS[1], 90, ZH_BUTTON_TOPS[0] + 36 - ZH_BUTTON_TOPS[1]));
    let pixels = band.data.as_chunks::<4>().0;
    if pixels.is_empty() {
        return false;
    }
    let p99 = |mut v: Vec<u32>| {
        v.sort_unstable();
        v[(v.len() - 1) * 99 / 100]
    };
    let luma = p99(pixels.iter().map(|&[r, g, b, _]| (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000).collect());
    let red = p99(pixels.iter().map(|&[r, _, _, _]| u32::from(r)).collect());
    let redder = p99(pixels.iter().map(|&[r, g, b, _]| u32::from(r).saturating_sub(u32::from(g.max(b)))).collect());
    luma >= ZH_BUTTON_LIT || (red >= ZH_BUTTON_RED && redder >= ZH_BUTTON_REDDER)
}

/// Decision buttons found, each with its name's centre.
type Buttons = Vec<(&'static str, (i32, i32))>;

/// The zh-TW decision buttons left to right. Their one-character names
/// ("吃", "換") read only tiled (see [`ocr_tiled`]), so each spot is read so.
/// Also the names [`cards_unread`] gave only because the instrument icons read showed no Blood-Stained Trap.
fn buttons_zh(img: &Rgba) -> (Buttons, Vec<Line>, Vec<&'static str>) {
    let known: Vec<(usize, &str)> = words().buttons.iter().enumerate().filter_map(|(i, b)| Some((i, (*b)?))).collect();
    let texts: Vec<&str> = known.iter().map(|k| k.1).collect();
    let spot = |i: usize| Rect::new(ZH_BUTTON_X[i % ZH_BUTTON_X.len()] - 45, ZH_BUTTON_TOPS[i / ZH_BUTTON_X.len()], 90, 36);
    let regions: [(Rect, f32); 14] = std::array::from_fn(|i| (spot(i), 2.0));
    let mut found: Vec<(&'static str, (i32, i32))> = Vec::new();
    // Each spot's first line, placed at the spot, for the Interrogation Room's cards.
    let mut names = Vec::new();
    for (i, lines) in ocr_tiled(img, &regions, 2).iter().enumerate() {
        let x = ZH_BUTTON_X[i % ZH_BUTTON_X.len()];
        let Some(first) = lines.first() else { continue };
        names.push(Line { text: first.text.clone(), rect: spot(i) });
        // A used button keeps its grayed name on screen.
        if let Some((i, _)) = best_match(&first.text, &texts, 0.65).filter(|_| button_lit(img, x)) {
            let name = BUTTONS[known[i].0];
            if !found.iter().any(|f| f.0 == name) {
                found.push((name, (x, ZH_BUTTON_Y)));
            }
        }
    }
    if let Some(at) = consume_unread(img, &found) {
        found.push(("Consume", at));
    }
    let (more, by_icons) = cards_unread_marked(img, &found, || read_held(img).map(|held| held.contains(&Tool::Trap)));
    found.extend(more);
    (found, names, by_icons)
}

/// [`cards_unread`], and the names it gave only because the instrument icons read (`trap`) showed no Blood-Stained
/// Trap: the icons are read only when Replace and Hunt both fit an unread card, so read with no Trap, the names given
/// rest on them. A row the cards read settle alone marks none.
fn cards_unread_marked(img: &Rgba, found: &Buttons, trap: impl FnOnce() -> Option<bool>) -> (Buttons, Vec<&'static str>) {
    let no_trap = std::cell::Cell::new(false);
    let more = cards_unread(img, found, || {
        let held = trap();
        no_trap.set(held == Some(false));
        held
    });
    let by_icons = if no_trap.get() { more.iter().map(|b| b.0).collect() } else { Vec::new() };
    (more, by_icons)
}

/// What a decision card can be, left to right: a row is these in this order,
/// each but End only when on offer, End always last.
const ROW_ORDER: [&str; 3] = ["Consume", "Replace", "Hunt"];

/// Replace's and Hunt's cards whose names went unread. The card under the
/// mouse rises and its name dims, and the tiled read then drops it: on
/// 2026-11-06 (run 06-08-47, frames 0146, 0151 and 0162, read 38 to 65 ms
/// after the game logged the Replace click) Replace's raised "換" read 99th
/// percentile luma 151 and red 132 in the name band, End's 184 and 184, and
/// each read took Replace as used. As in [`consume_unread`],
/// End places the row; a card in it with no name read, its art shown and not
/// red (see [`card_red`]) and its name lit (see [`button_lit`]) is named by
/// the rows the cards read allow. Between Consume and End either Replace or
/// Hunt fits, so such a card is Replace only when the instruments read show
/// no Blood-Stained Trap, whose Hunt card is the only one (`trap`, read only
/// then). A grayed name, the button used, stays unread, as does a card gone.
fn cards_unread(img: &Rgba, found: &Buttons, trap: impl FnOnce() -> Option<bool>) -> Buttons {
    let Some(end) = found.iter().find(|f| f.0 == "End").map(|f| f.1.0) else { return Vec::new() };
    // End at place 4, 5 or 6 closes a row of 2, 3 or 4 cards; at 3 it is alone.
    let Some(last) = CARD_X.iter().position(|&x| x == end).filter(|&p| p >= 4) else { return Vec::new() };
    let places = card_places(last - 2);
    let others = &places[..places.len() - 1];
    let named = |k: usize| found.iter().find(|f| f.1.0 == CARD_X[k]).map(|f| f.0);
    let red: Vec<Option<bool>> = others.iter().map(|&k| card_red(img, k)).collect();
    let unread: Vec<bool> = others.iter().zip(&red).map(|(&k, &r)| named(k).is_none() && r == Some(false) && button_lit(img, CARD_X[k])).collect();
    if !unread.contains(&true) {
        return Vec::new();
    }
    // The rows that fit what was read: the names in order, End left out.
    let fits = |row: &Vec<&'static str>| {
        others.iter().zip(row).zip(&red).all(|((&k, &name), &r)| {
            named(k).is_none_or(|n| n == name) && match r {
                Some(true) => name == "Consume",
                Some(false) => name != "Consume",
                None => true,
            }
        })
    };
    let mut rows: Vec<Vec<&'static str>> = (0u8..8)
        .map(|mask| ROW_ORDER.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, &n)| n).collect::<Vec<_>>())
        .filter(|row| row.len() == others.len() && fits(row))
        .collect();
    let name_at = |rows: &[Vec<&'static str>], i: usize| {
        let first = rows.first()?[i];
        rows.iter().all(|row| row[i] == first).then_some(first)
    };
    let sure = |rows: &[Vec<&'static str>]| (0..others.len()).all(|i| !unread[i] || name_at(rows, i).is_some());
    if !sure(&rows) && trap() == Some(false) {
        rows.retain(|row| !row.contains(&"Hunt"));
    }
    others
        .iter()
        .enumerate()
        .filter(|&(i, _)| unread[i])
        .filter_map(|(i, &k)| Some((name_at(&rows, i)?, (CARD_X[k], ZH_BUTTON_Y))))
        .filter(|(name, _)| !found.iter().any(|f| f.0 == *name))
        .collect()
}

/// Consume's card when its one-character name "吃" went unread. The tiled read
/// drops it now and then, the more so under the mouse, whose highlight raises
/// the card and dims its name: on 2026-11-06 day 20 the player rested the
/// mouse on Consume for 10 s, the card flickered between plain (name 99th
/// percentile luma 176, red 247) and raised (134, 191), every read found it
/// lit, and the name read in about half of them. Each miss took Consume as
/// used, and the advice swung between ending the run and Consume 12 times.
/// End is the last card, so it places the row; the row's first card showing
/// Consume's red art (see [`card_red`]) with its name lit (see
/// [`button_lit`]) is Consume. A grayed name, the button used, stays unread.
fn consume_unread(img: &Rgba, found: &Buttons) -> Option<(i32, i32)> {
    if found.iter().any(|f| f.0 == "Consume") {
        return None;
    }
    let end = found.iter().find(|f| f.0 == "End")?.1.0;
    // End at place 4, 5 or 6 closes a row of 2, 3 or 4 cards; at 3 it is alone.
    let last = CARD_X.iter().position(|&x| x == end).filter(|&p| p >= 4)?;
    let k = card_places(last - 2)[0];
    let x = CARD_X[k];
    let free = !found.iter().any(|f| f.1.0 == x);
    (free && card_red(img, k) == Some(true) && button_lit(img, x)).then_some((x, ZH_BUTTON_Y))
}

/// The zh-TW Interrogation Room's cards, which sit where two decision
/// buttons do. Their two-character names drop out of the row's read now and
/// then, and so do the tiled reads of `spots` (see [`buttons_zh`]), so either
/// read and either name will do: one card read places both.
fn interrogation_zh(row: &[Line], spots: &[Line]) -> Option<[i32; 2]> {
    let w = words();
    let near = |l: &Line, name: &str| similarity(&l.text, name) >= 0.75 || squash(&l.text).contains(&squash(name));
    let seen = |name: &str| row.iter().chain(spots).any(|l| near(l, name));
    (seen(w.instigate) || seen(w.bestow)).then_some(TWO_CARDS)
}

/// The Interrogation Room's cards: both titles, Instigate left of Bestow,
/// as the centre x of the two titles.
fn interrogation(lines: &[Line]) -> Option<[i32; 2]> {
    let at = |name: &str| lines.iter().find(|l| similarity(&l.text, name) >= 0.75).map(|l| l.rect.centre().0);
    match (at(words().instigate), at(words().bestow)) {
        (Some(a), Some(b)) if a < b => Some([a, b]),
        _ => None,
    }
}

/// The decision buttons left to right, each with its name's centre.
fn buttons(lines: &[Line]) -> Vec<(&'static str, (i32, i32))> {
    // The buttons the game's language has a word for, with their index in BUTTONS.
    let known: Vec<(usize, &str)> = words().buttons.iter().enumerate().filter_map(|(i, b)| Some((i, (*b)?))).collect();
    let texts: Vec<&str> = known.iter().map(|k| k.1).collect();
    let mut found: Vec<(&'static str, (i32, i32))> = Vec::new();
    for l in lines {
        if let Some((i, _)) = best_match(&l.text, &texts, 0.65) {
            let name = BUTTONS[known[i].0];
            if !found.iter().any(|f| f.0 == name) {
                found.push((name, l.rect.centre()));
            }
        }
    }
    found.sort_by_key(|f| f.1.0);
    found
}

/// The zh-TW "請選擇要吃的囚犯" banner, which OCR reads only in part over its
/// red ("一 一 • 」 蘀 要 吃 的 囚"): 吃 with 要 or 囚 beside it.
fn select_inmate_zh(lines: &[Line]) -> bool {
    lines.iter().map(|l| squash(&l.text)).any(|s| s.contains('吃') && (s.contains('要') || s.contains('囚')))
}

/// The big jaws of a zh-TW Consume target pick, whose red banner text often
/// fails to read: the pick always hides the days counter, while a Bestow's
/// red and blue bursts on the Interrogation Room screen, which can score as
/// jaws, come with the counter on show.
fn target_screen_jaws(p: &Panels, img: &Rgba) -> bool {
    p.days.is_none() && target_jaws_shown(img)
}

/// The "Day N" plaque, "第10日" in Chinese.
fn day_banner(squashed: &str) -> bool {
    if lang::zh() { squashed.starts_with('第') && squashed.contains('日') } else { squashed.starts_with("day") }
}

/// Card names on a pick screen: lines that match a name, left to right,
/// each with the centre x of its line.
fn offers(lines: &[Line], names: &[String]) -> Vec<(usize, i32)> {
    let mut found: Vec<(usize, i32)> = Vec::new();
    for l in &names_joined(lines) {
        if let Some((i, _)) = best_match(&l.text, names, 0.75)
            && !found.iter().any(|f| f.0 == i)
        {
            found.push((i, l.rect.centre().0));
        }
    }
    found.sort_by_key(|f| f.1);
    found
}

/// The most a gap inside one card's name spans, in reference px. Redrawn at
/// interface scales 0.65 to 0.95, "Fanged Guillotine: Alpha Wolf" read as
/// two lines split at the colon, 33 px apart, and neither named an
/// instrument: the pick got no advice. Names on two cards are over 250 px
/// apart.
const NAME_GAP: i32 = 80;

/// `lines` in reading order with the lines of one name joined: a line that
/// starts within [`NAME_GAP`] of the last one's end, on the same row.
fn names_joined(lines: &[Line]) -> Vec<Line> {
    let mut sorted: Vec<&Line> = lines.iter().collect();
    sorted.sort_by_key(|l| l.rect.x);
    let mut out: Vec<Line> = Vec::new();
    for l in sorted {
        if let Some(last) = out.last_mut()
            && l.rect.x - (last.rect.x + last.rect.w) <= NAME_GAP
            && l.rect.y < last.rect.y + last.rect.h
            && last.rect.y < l.rect.y + l.rect.h
        {
            let right = (last.rect.x + last.rect.w).max(l.rect.x + l.rect.w);
            let (top, bottom) = (last.rect.y.min(l.rect.y), (last.rect.y + last.rect.h).max(l.rect.y + l.rect.h));
            last.rect = Rect::new(last.rect.x, top, right - last.rect.x, bottom - top);
            last.text.push_str(&l.text);
        } else {
            out.push(Line { text: l.text.clone(), rect: l.rect });
        }
    }
    out
}

pub(crate) fn luma_stats(img: &Rgba, (cx, cy): (i32, i32), half: i32) -> (f32, f32) {
    let mut v = Vec::with_capacity(((2 * half) * (2 * half)) as usize);
    for y in cy - half..cy + half {
        for x in cx - half..cx + half {
            let [r, g, b] = img.rgb(x, y);
            v.push(0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32);
        }
    }
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f32>() / v.len() as f32;
    (mean, var.sqrt())
}

/// Mean luma over each Chromatic Prisoner slot, in fill order.
pub fn slot_scores(img: &Rgba) -> Vec<f32> {
    SLOT_Y.iter().flat_map(|&y| SLOT_X.map(|x| luma_stats(img, (x, y), 22).0)).collect()
}

/// Luma spread over the first instrument icon spot.
pub fn icon_score(img: &Rgba) -> f32 {
    luma_stats(img, ICON, 22).1
}

/// The panels of a frame whose two layouts are the same, as at interface
/// scale 1 (the tests' saved frames); see [`read_panels_titled`].
pub fn read_panels(img: &Rgba) -> Panels {
    read_panels_titled(img, img)
}

/// The panels of `img`, a frame in the board layout, with [`TITLE`],
/// [`INCIDENT`], [`RIOT`] and [`REROLLS`] read from `title`, the same frame in the title layout (see
/// [`super::layout::lay_out_scaled`]).
pub fn read_panels_titled(img: &Rgba, title: &Rgba) -> Panels {
    let d = data();
    // Every fixed region is read at once, whether the screen needs it or not.
    // Every fixed region in this order; zh-TW text reads at other scales on two.
    let regions = || {
        let mut regions = [DAYS, TITLE, HEADING, CARDS, TARGET, CANCEL, BANNER, DIALOG, INCIDENT, THEME, OBJECTIVE, STATUS, RIOT, NAMES];
        if lang::zh() {
            // The red Consume target banner reads only at 1x, an incident's title only at 2x.
            regions[4].1 = 1.0;
            regions[8].1 = 2.0;
            // The heading's taller characters lose their tops to the English band (y 680-720).
            regions[2].0 = Rect::new(HEADING.0.x, 675, HEADING.0.w, 50);
        }
        regions
    };
    let mut imgs = [img; 14];
    for i in [1, 8, 12] {
        imgs[i] = title;
    }
    let title_img = title;
    let [days_lines, title, heading, cards, target, cancel, banner, dialog, incident, theme_lines, objective_lines, status, riot, name_lines] =
        ocr_all(imgs, &regions());
    let mut p = Panels {
        screen: Screen::Unknown,
        days: if lang::zh() {
            days_zh_split(&days_lines, img).or_else(|| {
                // Only a thin "1" drops out, which leaves one digit of 10 to 30.
                let read = days(&days_lines);
                read.filter(|&n| n >= 10).or_else(|| days_zh_tiled(&days_lines, img, read).or(read))
            })
        } else {
            days(&days_lines)
        },
        doors: Vec::new(),
        decision: Vec::new(),
        instruments: Vec::new(),
        rerolls: 0,
        vault: Vec::new(),
        tool_icons: icon_score(img) >= ICON_SPREAD,
        theme: None,
        incident: None,
        objective: None,
        held: None,
        spots: Vec::new(),
        gift_picked: false,
        card_red: std::array::from_fn(|k| card_red(img, k)),
        doors_fixed: false,
        by_icons: Vec::new(),
    };
    let w = words();
    if w.instrument_pick.iter().any(|t| has(&title, t, 0.75)) {
        p.screen = Screen::InstrumentPick;
        let names: Vec<String> = d.tool_ids.iter().map(|&t| instrument(t).to_string()).collect();
        for (i, x) in offers(&name_lines, &names) {
            let tool = d.tool_ids[i];
            p.instruments.push(tool);
            p.spots.push((Spot::Card(tool.key().to_string()), Layout::Board, (x, PICK_Y)));
        }
        // Read on this screen alone, so no other screen pays for the region.
        p.rerolls = rerolls(&ocr(title_img, REROLLS.0, REROLLS.1));
        if p.rerolls > 0 {
            p.spots.push((Spot::Button("Re-roll"), Layout::Title, REROLL_AT));
        }
    } else if has(&title, w.treasure, 0.75) || title.iter().any(|l| squash(&l.text).contains(w.treasure_word)) || super::treasure_shown(&super::fingerprint(img)) {
        // The print alone names a treasure pick whose title still fades in, read early (see `treasure_shown`).
        p.screen = Screen::TreasurePick;
        p.gift_picked = confirm_lit(img);
        // The four cards' middles line up with the instrument cards' (rec1-t085).
        let gift_names: Vec<String> = Gift::ALL.iter().map(|g| tool_name(g.key())).collect();
        for (i, x) in offers(&name_lines, &gift_names) {
            p.spots.push((Spot::Card(Gift::ALL[i].key().to_string()), Layout::Board, (x, PICK_Y)));
        }
    } else if w.return_to_selection.is_some_and(|t| has(&status, t, 0.75)) {
        p.screen = Screen::PrisonStatus;
    } else if has(&target, w.select_inmate, 0.75) || has(&cancel, w.cancel, 0.8) || (lang::zh() && (select_inmate_zh(&target) || target_screen_jaws(&p, img))) {
        p.screen = Screen::ChainTarget;
        p.decision = vec!["Cancel"];
        p.spots.push((Spot::Button("Cancel"), Layout::Board, CANCEL_AT));
    } else if let Some(id) = theme(&theme_lines) {
        p.screen = Screen::Theme;
        p.theme = Some(id);
    } else if popup(&dialog, &incident) || w.view_status.is_some_and(|t| has(&status, t, 0.75)) {
        p.screen = Screen::Popup;
        p.incident = incident_named(&incident);
    } else {
        // The destination and cell picks show no buttons: their headings spare the 14 tiled reads.
        let no_buttons = [Some(w.select_destination), w.select_objective].into_iter().flatten().any(|t| has(&heading, t, 0.75));
        let (found, spots, by_icons) = match () {
            _ if !lang::zh() => (buttons(&cards), Vec::new(), Vec::new()),
            _ if no_buttons => (Vec::new(), Vec::new(), Vec::new()),
            _ => buttons_zh(img),
        };
        p.decision = found.iter().map(|b| b.0).collect();
        p.by_icons = by_icons;
        let doors = doors(&cards, |x| if lang::zh() { reread_zh(img, x) } else { reread(img, x) });
        let doors = if lang::zh() { doors_zh(doors, img) } else { doors };
        let doors = if lang::zh() && doors.is_empty() && has(&heading, w.select_destination, 0.75) { doors_alone(img) } else { doors };
        if w.select_objective.is_some_and(|t| has(&heading, t, 0.75)) {
            // Without brackets or a target glow nothing is picked: the board
            // shows between the pick and the next screen.
            if pick_shown(img) {
                p.screen = Screen::CellPick;
            }
        } else if let Some([instigate, bestow]) = interrogation(&cards).or_else(|| lang::zh().then(|| interrogation_zh(&cards, &spots)).flatten()) {
            // Its heading is "Make Your Decision", as on the manage step.
            p.screen = Screen::Interrogation;
            p.decision.clear();
            for (key, x) in [("instigate", instigate), ("bestow", bestow)] {
                p.spots.push((Spot::Card(key.to_string()), Layout::Board, (x, DOOR_Y)));
            }
        } else if has(&heading, w.select_destination, 0.75) || (!doors.is_empty() && p.days.is_some()) {
            // Door cards or decision buttons alone need the days counter:
            // text on the lobby's objective dialog or any other game screen
            // can read as one.
            p.screen = Screen::Patrol;
            p.doors = doors.iter().map(|d| d.0).collect();
            p.spots = doors.iter().filter_map(|&(door, x)| Some((Spot::Door(door?), Layout::Board, (x, DOOR_Y)))).collect();
            p.decision.clear();
        } else if has(&heading, w.make_decision, 0.75) || (!p.decision.is_empty() && p.days.is_some()) {
            p.screen = Screen::Manage;
            p.spots = found.into_iter().map(|(name, (x, y))| (Spot::Button(name), Layout::Board, (x, y - BUTTON_ABOVE))).collect();
        } else if banner.iter().any(|l| day_banner(&squash(&l.text))) {
            p.screen = Screen::DayBanner;
        } else if w.riot.is_some_and(|t| has(&riot, t, 0.75)) {
            p.screen = Screen::RunEnd;
        }
    }
    // The Consume target pick covers the panel.
    if matches!(p.screen, Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::Interrogation) {
        p.held = read_held(img);
    }
    let pick_or_end = matches!(p.screen, Screen::InstrumentPick | Screen::TreasurePick | Screen::RunEnd);
    if !pick_or_end && p.screen != Screen::Theme {
        p.objective = objective(&objective_lines);
    }
    // The riot screen shows no Chromatic Prisoner slots.
    if !pick_or_end {
        // Slots fill in order: the vault ends at the first slot without a portrait.
        for (k, s) in slot_scores(img).into_iter().enumerate() {
            if s < SLOT_DIM {
                break;
            }
            let (x, y) = (SLOT_X[k % 2], SLOT_Y[k / 2]);
            let (beast, conf) = beast_at(img, (x, y));
            if s < SLOT_FILLED && (beast.is_none() || conf < SLOT_SURE) {
                break;
            }
            p.vault.push(VaultSlot { beast, conf });
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_names() {
        let names = ["Ferocious Beast's Ravenous Chains", "Scapegoat's Guillotine", "Kin's Coin of Fate", "Forged Key"];
        assert_eq!(best_match("Ferocious Beast's Ravenous Chains", &names, 0.75), Some((0, 1.0)));
        assert_eq!(best_match("ferocious beasts ravenous chalns", &names, 0.75).map(|m| m.0), Some(0));
        assert_eq!(best_match("Scapegoat'sGuillotlne", &names, 0.75).map(|m| m.0), Some(1));
        assert_eq!(best_match("Kin 's Coin of Fale", &names, 0.75).map(|m| m.0), Some(2));
        assert_eq!(best_match("Randomly removes 2 other", &names, 0.75), None);
        assert_eq!(door("Go to Celi9."), Some(9));
        assert_eq!(door("Go to Imeycogation Room"), Some(1));
        assert_eq!(door("Go to Cell 16"), Some(16));
        assert_eq!(number("O"), Some(0));
    }

    /// A reference frame with the decision cards' art at `places` painted
    /// `art` and their names, in the name band, painted `name`.
    fn cards_frame(places: &[usize], art: [u8; 3], name: [u8; 3]) -> Rgba {
        let (w, h) = (1918usize, 1078usize);
        let mut img = Rgba { w, h, data: [30, 30, 30, 255].repeat(w * h) };
        let mut paint = |r: Rect, c: [u8; 3]| {
            for y in r.y..r.y + r.h {
                for x in r.x..r.x + r.w {
                    let i = (y as usize * w + x as usize) * 4;
                    img.data[i..i + 3].copy_from_slice(&c);
                }
            }
        };
        for &k in places {
            let x = CARD_X[k];
            paint(Rect::new(x - 160, 760, 320, 80), art);
            paint(Rect::new(x - 15, 868, 30, 20), name);
        }
        img
    }

    /// The red card a row's End places, its name lit but unread, is Consume
    /// (the hovered "吃" of 2026-11-06 day 20); a grayed name, a card not red,
    /// or no End to place the row leaves it out.
    #[test]
    fn consume_card_whose_name_went_unread() {
        const RED: [u8; 3] = [200, 90, 80];
        const PINK: [u8; 3] = [240, 150, 150];
        const DIM_RED: [u8; 3] = [190, 120, 120];
        let end = |k: usize| -> Buttons { vec![("End", (CARD_X[k], ZH_BUTTON_Y))] };
        // Two cards: Consume at place 2, End at 4; plain and raised (dimmed) names.
        for name in [PINK, DIM_RED] {
            let img = cards_frame(&[2, 4], RED, name);
            assert_eq!(consume_unread(&img, &end(4)), Some((CARD_X[2], ZH_BUTTON_Y)), "{name:?}");
        }
        // Three and four cards: Consume first, at place 1 or 0.
        assert_eq!(consume_unread(&cards_frame(&[1, 3, 5], RED, PINK), &end(5)), Some((CARD_X[1], ZH_BUTTON_Y)));
        assert_eq!(consume_unread(&cards_frame(&[0, 2, 4, 6], RED, PINK), &end(6)), Some((CARD_X[0], ZH_BUTTON_Y)));
        // Read already: nothing added.
        let both: Buttons = vec![("Consume", (CARD_X[2], ZH_BUTTON_Y)), ("End", (CARD_X[4], ZH_BUTTON_Y))];
        assert_eq!(consume_unread(&cards_frame(&[2, 4], RED, PINK), &both), None);
        // A used Consume's grayed name stays used.
        assert_eq!(consume_unread(&cards_frame(&[2, 4], RED, [95, 95, 95]), &end(4)), None);
        // Replace's gray art (Replace and End) is no Consume.
        assert_eq!(consume_unread(&cards_frame(&[2, 4], [110, 105, 100], [200, 200, 200]), &end(4)), None);
        // Art too dark to tell its colour (card_red None, mean R + G + B 75 under CARD_SHOWN; [40, 40, 40] would
        // sum to 120 and read as not red) is no Consume either.
        assert_eq!(consume_unread(&cards_frame(&[2, 4], [25, 25, 25], PINK), &end(4)), None);
        // No End read, or End alone: the row is not placed.
        assert_eq!(consume_unread(&cards_frame(&[2, 4], RED, PINK), &Vec::new()), None);
        assert_eq!(consume_unread(&cards_frame(&[3], RED, PINK), &end(3)), None);
        // A Replace read at Consume's place keeps it.
        let replace: Buttons = vec![("Replace", (CARD_X[2], ZH_BUTTON_Y)), ("End", (CARD_X[4], ZH_BUTTON_Y))];
        assert_eq!(consume_unread(&cards_frame(&[2, 4], RED, PINK), &replace), None);
    }

    /// A reference frame with a decision card at each place `k` of [`CARD_X`]: its art painted `art`, its name `name`.
    fn row_frame(cards: &[(usize, [u8; 3], [u8; 3])]) -> Rgba {
        let mut img = cards_frame(&[], [0; 3], [0; 3]);
        for &(k, art, name) in cards {
            let one = cards_frame(&[k], art, name);
            let x = CARD_X[k];
            for y in 760..900 {
                for px in (x - 160)..(x + 160) {
                    let i = (y as usize * img.w + px as usize) * 4;
                    img.data[i..i + 4].copy_from_slice(&one.data[i..i + 4]);
                }
            }
        }
        img
    }

    /// An unread card that is no Consume and has its name lit is Replace or
    /// Hunt as the row allows (the raised "換" of 2026-11-06_06-08-47 frames
    /// 0151 and 0162); where both fit, it is Replace only with no Blood-Stained
    /// Trap held. A grayed name, dark art or a card read leaves it out.
    #[test]
    fn replace_or_hunt_card_whose_name_went_unread() {
        const RED: [u8; 3] = [200, 90, 80];
        const GRAY: [u8; 3] = [110, 105, 100];
        const LIT: [u8; 3] = [200, 200, 200];
        // The raised name of frame-0151: 99th percentile luma 151, red 132.
        const RAISED: [u8; 3] = [132, 160, 140];
        const USED: [u8; 3] = [95, 95, 95];
        let at = |name: &'static str, k: usize| (name, (CARD_X[k], ZH_BUTTON_Y));
        let unsure = || -> Option<bool> { panic!("the instruments are read only when Replace and Hunt both fit") };
        // Consume, ?, End: Replace with no Trap held; with the Trap or the instruments unread it stays unread.
        let img = row_frame(&[(1, RED, LIT), (3, GRAY, RAISED), (5, GRAY, LIT)]);
        let found: Buttons = vec![at("End", 5), at("Consume", 1)];
        assert_eq!(cards_unread(&img, &found, || Some(false)), vec![at("Replace", 3)]);
        assert_eq!(cards_unread(&img, &found, || Some(true)), Vec::new());
        assert_eq!(cards_unread(&img, &found, || None), Vec::new());
        // Its Consume unread as well (consume_unread adds it before): Consume's red art still places it.
        assert_eq!(cards_unread(&img, &vec![at("End", 5)], || Some(false)), vec![at("Replace", 3)]);
        // ?, End: Replace or Hunt.
        let img = row_frame(&[(2, GRAY, RAISED), (4, GRAY, LIT)]);
        assert_eq!(cards_unread(&img, &vec![at("End", 4)], || Some(false)), vec![at("Replace", 2)]);
        assert_eq!(cards_unread(&img, &vec![at("End", 4)], || Some(true)), Vec::new());
        // Four cards: Consume, Replace, Hunt, End, with no instrument read needed.
        let img = row_frame(&[(0, RED, LIT), (2, GRAY, RAISED), (4, GRAY, RAISED), (6, GRAY, LIT)]);
        let found: Buttons = vec![at("Consume", 0), at("End", 6)];
        assert_eq!(cards_unread(&img, &found, unsure), vec![at("Replace", 2), at("Hunt", 4)]);
        let found: Buttons = vec![at("Consume", 0), at("Replace", 2), at("End", 6)];
        assert_eq!(cards_unread(&img, &found, unsure), vec![at("Hunt", 4)]);
        // Three cards with no Consume: Replace, Hunt, End.
        let img = row_frame(&[(1, GRAY, RAISED), (3, GRAY, LIT), (5, GRAY, LIT)]);
        let found: Buttons = vec![at("Hunt", 3), at("End", 5)];
        assert_eq!(cards_unread(&img, &found, unsure), vec![at("Replace", 1)]);
        // Consume, Hunt read, End: nothing left unread.
        let img = row_frame(&[(1, RED, LIT), (3, GRAY, RAISED), (5, GRAY, LIT)]);
        let found: Buttons = vec![at("Consume", 1), at("Hunt", 3), at("End", 5)];
        assert_eq!(cards_unread(&img, &found, unsure), Vec::new());
        // A grayed name, the button used, stays used.
        let img = row_frame(&[(1, RED, LIT), (3, GRAY, USED), (5, GRAY, LIT)]);
        assert_eq!(cards_unread(&img, &vec![at("Consume", 1), at("End", 5)], || Some(false)), Vec::new());
        // Art too dark to tell (fading in), or no card there (Replace gone: Consume and End close up).
        let img = row_frame(&[(1, RED, LIT), (3, [25, 25, 25], RAISED), (5, GRAY, LIT)]);
        assert_eq!(cards_unread(&img, &vec![at("Consume", 1), at("End", 5)], || Some(false)), Vec::new());
        let img = row_frame(&[(2, RED, LIT), (4, GRAY, LIT)]);
        assert_eq!(cards_unread(&img, &vec![at("Consume", 2), at("End", 4)], || Some(false)), Vec::new());
        // An unread red card is Consume's, left to consume_unread; no End read, or End alone, places no row.
        let img = row_frame(&[(1, RED, LIT), (3, GRAY, RAISED), (5, GRAY, LIT)]);
        assert_eq!(cards_unread(&img, &vec![at("Replace", 3), at("End", 5)], || Some(false)), Vec::new());
        assert_eq!(cards_unread(&img, &vec![at("Consume", 1)], || Some(false)), Vec::new());
        assert_eq!(cards_unread(&row_frame(&[(3, GRAY, LIT)]), &vec![at("End", 3)], || Some(false)), Vec::new());
    }

    /// Only a name the instrument icons decided is marked (see [`cards_unread_marked`]): Consume, ?, End named
    /// Replace because the icons showed no Trap is; a row of four, settled by the cards read alone, marks none, and
    /// neither does a card left unnamed.
    #[test]
    fn a_card_named_by_the_icons_is_marked() {
        const RED: [u8; 3] = [200, 90, 80];
        const GRAY: [u8; 3] = [110, 105, 100];
        const LIT: [u8; 3] = [200, 200, 200];
        const RAISED: [u8; 3] = [132, 160, 140];
        let at = |name: &'static str, k: usize| (name, (CARD_X[k], ZH_BUTTON_Y));
        let img = row_frame(&[(1, RED, LIT), (3, GRAY, RAISED), (5, GRAY, LIT)]);
        let found: Buttons = vec![at("End", 5), at("Consume", 1)];
        assert_eq!(cards_unread_marked(&img, &found, || Some(false)), (vec![at("Replace", 3)], vec!["Replace"]));
        assert_eq!(cards_unread_marked(&img, &found, || Some(true)), (Vec::new(), Vec::new()));
        assert_eq!(cards_unread_marked(&img, &found, || None), (Vec::new(), Vec::new()));
        // Four cards: Replace and Hunt by their places alone, the icons never read.
        let img = row_frame(&[(0, RED, LIT), (2, GRAY, RAISED), (4, GRAY, RAISED), (6, GRAY, LIT)]);
        let found: Buttons = vec![at("Consume", 0), at("End", 6)];
        let unread = || -> Option<bool> { panic!("the instruments are read only when Replace and Hunt both fit") };
        assert_eq!(cards_unread_marked(&img, &found, unread), (vec![at("Replace", 2), at("Hunt", 4)], Vec::new()));
        // Three cards with no Consume: Replace by its place.
        let img = row_frame(&[(1, GRAY, RAISED), (3, GRAY, LIT), (5, GRAY, LIT)]);
        let found: Buttons = vec![at("Hunt", 3), at("End", 5)];
        assert_eq!(cards_unread_marked(&img, &found, unread), (vec![at("Replace", 1)], Vec::new()));
    }

    #[test]
    fn objective_progress_parses() {
        assert_eq!(progress("0/2", 2), Some(0));
        assert_eq!(progress("times0/2", 2), Some(0));
        assert_eq!(progress("O/18", 18), Some(0));
        assert_eq!(progress("Trigger ! Prison Incident 2 times 1/2", 2), Some(1));
        assert_eq!(progress("Completed", 4), Some(4));
        assert_eq!(progress("Move 18 tiles Completed", 18), Some(18));
        assert_eq!(progress("UltimateCompleted", 1), Some(1));
        assert_eq!(progress("Move 18 tiles 7/18", 18), Some(7));
        assert_eq!(progress("timesl/2", 2), Some(1));
        assert_eq!(progress("tilesl0/18", 18), Some(10));
        assert_eq!(progress("times0/2", 2), Some(0));
        assert_eq!(progress("Complete 2 objectives 1/2", 2), Some(1));
        assert_eq!(progress("Consume 4 times", 4), None);
        assert_eq!(progress("3/2", 2), None);
    }

    fn numbers(cards: Vec<(Option<u8>, i32)>) -> Vec<Option<u8>> {
        cards.into_iter().map(|c| c.0).collect()
    }

    #[test]
    fn cards_by_position() {
        let line = |text: &str, x: i32, y: i32| Line { text: text.to_string(), rect: Rect::new(x, y, 160, 30) };
        // Three cards, the room on two lines.
        let three = [line("Go to Cell 7", 535, 860), line("Go to Interrogation", 880, 845), line("Room", 920, 875), line("Go to Cell 12", 1225, 860)];
        assert_eq!(numbers(doors(&three, |_| None)), [Some(7), Some(1), Some(12)]);
        // Two cards centred between the three-card columns (Burning Stake).
        let two = [line("Go to Cell 5", 1060, 860), line("Go to Cell 2", 710, 860)];
        assert_eq!(numbers(doors(&two, |_| None)), [Some(2), Some(5)]);
        // OCR of a live frame at the reader's scale: junk before "Go", 11 read as IL.
        let live = [line("so to Cell 6", 710, 869), line("eGo to Cell IL", 1057, 869)];
        assert_eq!(numbers(doors(&live, |_| None)), [Some(6), Some(11)]);
        // The card under the mouse: "Go" dropped and "Cell 11" garbled; it is read again.
        let hovered = [line("to Cell 8", 588, 869), line("to celgl", 927, 869), line("Go to Cell 13", 1234, 869)];
        assert_eq!(numbers(doors(&hovered, |x| (x == 1007).then_some(11))), [Some(8), Some(11), Some(13)]);
    }

    /// A name split at its colon reads as one, its click at the whole name's
    /// middle; names on other cards stay apart (where the zh-TW OCR of a
    /// pick redrawn at interface scale 0.8 put them).
    #[test]
    fn a_name_split_at_its_colon_is_joined() {
        let line = |text: &str, x: i32, right: i32| Line { text: text.to_string(), rect: Rect::new(x, 506, right - x, 28) };
        let lines = [line("Barbed Whip", 1372, 1489), line("Alpha Wolf", 1001, 1058), line("Executioner's Blacklist", 378, 584), line("Fanged Guillotine:", 853, 968)];
        let names: Vec<String> = data().tool_ids.iter().map(|&t| instrument(t).to_string()).collect();
        let tools: Vec<(Tool, i32)> = offers(&lines, &names).into_iter().map(|(i, x)| (data().tool_ids[i], x)).collect();
        assert_eq!(tools, [(Tool::Blacklist, 481), (Tool::Guillotine, 955), (Tool::Whip, 1430)]);
    }

    #[test]
    fn interrogation_cards() {
        let line = |text: &str, x: i32| Line { text: text.to_string(), rect: Rect::new(x, 862, 110, 30) };
        assert!(interrogation(&[line("Instigate", 738), line("Bestow", 1090)]).is_some());
        assert!(interrogation(&[line("lnstigate", 738), line("Bestaw", 1090)]).is_some());
        assert!(interrogation(&[line("Instigate", 738)]).is_none());
        assert!(interrogation(&[line("Consume", 600), line("Replace", 900), line("End", 1200)]).is_none());
    }

    #[test]
    fn reroll_count() {
        let read = |text: &str| rerolls(&[Line { text: text.to_string(), rect: Rect::new(693, 893, 199, 22) }]);
        // As Windows OCR read reroll-2000x837.
        assert_eq!(read("Remaining attemptsl"), 1);
        assert_eq!(read("Remaining attempts1"), 1);
        assert_eq!(read("Remaining attempts 2"), 2);
        assert_eq!(read("Remainingattempts2"), 2);
        // The text shows only with re-rolls left.
        assert_eq!(read("Remaining attempts"), 1);
        assert_eq!(read("Confirm"), 0);
        assert_eq!(rerolls(&[]), 0);
    }

    #[test]
    fn door_numbers() {
        assert_eq!(door("Go to Cell lO"), Some(10));
        assert_eq!(door("Go to Cell 16"), Some(16));
        // The "l" of "Cell" is not a 1.
        assert_eq!(door("Go to Cell"), None);
        assert_eq!(door("Go to Interrogation Room"), Some(1));
    }

    #[test]
    fn days_counter() {
        let read = |text: &str| days(&[Line { text: text.to_string(), rect: Rect::new(844, 11, 235, 33) }]);
        assert_eq!(read("12 day(s) until Riot"), Some(12));
        assert_eq!(read("1 day(s) until Riot"), Some(1));
        // The red 0 on the last day, as Windows OCR read it in a recording.
        assert_eq!(read("() day(S) untii Riot"), Some(0));
        assert_eq!(read("0day(s) until Riot"), Some(0));
        assert_eq!(read("Oday(s) until Riot"), Some(0));
        assert_eq!(read("O day(s) until Riot"), Some(0));
        assert_eq!(read("1 0 day(s) until Riot"), Some(10));
        assert_eq!(read("13 dav(s) until Riot"), Some(13));
        assert_eq!(read("day(s) until Riot"), None);
        assert_eq!(read("Tartarus Prison"), None);
        assert_eq!(days(&[]), None);
    }

    /// zh-TW text as Windows OCR read the chinese/ screenshots: a space
    /// between every character, 天 read as 夭, a stray 一 before a card.
    #[test]
    fn zh_tw_reads() {
        assert_eq!(days_zh("距 離 暴 動 剩 餘 4 夭"), Some(4));
        assert_eq!(days_zh("距 離 暴 動 剩 餘 19 天"), Some(19));
        assert_eq!(days_zh("距 離 暴 動 剩 餘"), None);
        // The red count of the last days, read without its 天.
        assert_eq!(days_zh("距 離 暴 動 剩 餘 2"), Some(2));
        assert_eq!(days_zh("累 計 移 動 18 天"), None);
        assert_eq!(door_zh("去 2 號"), Some(2));
        assert_eq!(door_zh("一 丶 去 16 號"), Some(16));
        assert_eq!(door_zh("一 去 審 訊 室"), Some(1));
        assert_eq!(door_zh("去 號"), None);
        assert!(completed_tail("累 計 移 動 18 格 已 完 成", "已完成"));
        assert!(!completed_tail("累 計 移 動 18 格 7 / 18", "已完成"));
        let line = |text: &str| Line { text: text.to_string(), rect: Rect::new(760, 815, 250, 25) };
        assert!(select_inmate_zh(&[line("一 一 • 」 蘀 要 吃 的 囚")]));
        assert!(!select_inmate_zh(&[line("一 主 月")]));
        assert_eq!(squash("目 標 ： 巡 獄 之 路"), "目標巡獄之路");
        assert_eq!(squash("Go to Cell 7"), "gotocell7");
    }

    #[test]
    fn dialogs() {
        let lines = |texts: &[&str]| texts.iter().map(|t| Line { text: t.to_string(), rect: Rect::new(560, 561, 800, 34) }).collect::<Vec<_>>();
        assert!(popup(&lines(&["Tip", "Network request timeout. Please try again later.(5684)"]), &[]));
        assert!(popup(&lines(&["Connecting to server..."]), &[]));
        assert!(popup(&[], &lines(&["Behind Closed Doors"])));
        assert!(!popup(&lines(&["Multiple"]), &lines(&["Warden"])));
        assert!(!popup(&[], &[]));
        assert_eq!(theme(&lines(&["Opulence"])), Some(302));
        assert_eq!(theme(&lines(&["Cage Fiqht"])), Some(304));
        assert_eq!(theme(&lines(&["Warden"])), None);
    }
}
