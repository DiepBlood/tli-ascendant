//! State machine for live Vorax (S13) advice: cues from the game log ([`Cue`]) switch the mode, frames
//! are read into a board (`crate::vorax::read`), and only when the same board is read twice in a row is it
//! handed to the solver (crates/vorax); the result goes to the window ([`View`]) and is also written to
//! its own run log (`<app_dir>\vorax\<UTC>\`, separate from Tartarus's `runs\`).
//!
//! Platform independent and replayable: time is always passed in as a parameter (UTC milliseconds, the same
//! clock as [`super::latency::game_millis`]); the system clock is never read. When `Config::inline` is true,
//! reading and solving both finish on the caller's thread, so replay tests are deterministic.
//!
//! # Contract with the window (`vorax_view.rs`)
//! - [`VoraxSession::new`]: build one at program start; there is exactly one per program. When settings
//!   change, use [`VoraxSession::configure`].
//! - [`VoraxSession::on_progress`]: called when a background read or solve finishes (the UI uses it to
//!   request a repaint).
//! - [`VoraxSession::note_cue`]: hand it every `Signal::Vorax(cue)`, together with the log line's time
//!   (UTC milliseconds; `None` means it was found by looking back at program start, see [`under_way`]).
//!   Returns a [`ModeChange`]: on `Entered` the caller pauses the Tartarus Session, on `Left` it resumes it.
//!   VoraxSession never touches the Tartarus Session.
//! - [`VoraxSession::active`]: in Vorax mode (intro page, board, result screen). While true, frames are
//!   given to [`VoraxSession::read`], not to Tartarus.
//! - [`VoraxSession::busy`]: a read or solve is in progress (the window shows a spinner).
//! - [`VoraxSession::read`]: feed one captured frame (the game client area at its original size), with the
//!   capture time and reason ("auto", "ahead", "hotkey", "scheduled"). Does nothing when not in the mode.
//!   Reads run in the background, one at a time; frames arriving during a read keep only the latest one.
//! - [`VoraxSession::poll`]: called once per repaint with the current time; collects background results and
//!   handles timeouts. Returns true when the [`View`] changed.
//! - [`VoraxSession::view`]: the display state. `boxes` are the boxes to draw on the game, in **frame
//!   pixels** (center x, center y, half width, half height), the same convention as `Session::mark()`,
//!   converted by `crate::vorax::read::to_frame` from 2560x1440 reference coordinates; screen coordinates,
//!   `unit` and `clip` are converted by the window following `mark_spot`. When `boxes` is empty, everything
//!   is hidden.
//! - [`VoraxSession::take_read_next`]: when it returns true once, the UI calls `Watcher::read_next()` to
//!   read one more sample (to confirm the board).
//! - [`VoraxSession::take_news`]: after each completed read, `Some((news, off_board))`, to be handed to
//!   `Watcher::report`.
//! - [`VoraxSession::log_window`]: call when the capture window changes (`Event::Window`).
//! - [`VoraxSession::log_view`]: call when the boxes drawn by the window change; writes one `vorax_view`
//!   row; `fields` is a JSON object (for example `{"marks": [{"what": "card 2", "at": [x, y, w, h]}]}`,
//!   where `at` is `Mark::at()`), and VoraxSession adds `best`, `solving`, `screen`, `round` and `boxes`.
//! - [`VoraxSession::note_other_signal`]: hand it any log signal other than `Signal::Vorax` received while
//!   in Vorax mode (only Tartarus has those); returns `Left` to mean it is treated as having left Vorax
//!   (deadlock guard).
// Private builds only: whole frames of the English client as samples (roadmap item 17, `vorax_samples.rs`).
#[cfg(feature = "dev-samples")]
#[path = "vorax_samples.rs"]
pub(crate) mod samples;

use super::runlog::{RunLog, timestamp};
use super::settings::Settings;
use super::vorax_learn::Learn;
use crate::reader::Rgba;
use crate::vorax::read::{self, Kind, Reading};
use crate::vorax::{AdviceRows, HeldOverride, What, advice_rows, describe, signature, targets, to_screen};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicBool, Ordering};
use vorax::advisor::{Advice, Prepared, Race, ScreenPhase, ScreenState, conclude, play_raced_until, play_work, prepare, same_n};
use vorax::bot::Tally;

// ---------------------------------------------------------------------------- log cues

/// The Vorax pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Intro page (`S13GamePlay`).
    Intro,
    /// Board (`S13GamePlayMain`).
    Main,
    /// Result screen (`S13GamePlayReward`).
    Reward,
}

/// Page state (the game spells it Destory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageState {
    Run,
    Hide,
    Destory,
}

/// What one Vorax log line says (plan section 2.1). Most Vorax sounds are
/// `UDataTable::FindRow ... requested row 'Play_UI_S13_KYZ_Card_…'` warning lines, and buttons are also
/// FindRow lines; `AudioMgr@PlayAudio` lines are not used (the same sound would be counted twice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    /// Page switch: `PageName = S13GamePlay(Main|Reward)? , PageRunState = X`.
    Page(Page, PageState),
    /// `ProtoId = 5619`: on every confirm or reroll, the board changed.
    Synced,
    /// `ProtoId = 5621`: a game ended (may appear as 2 lines for the same game).
    Over,
    /// A potion was used (`Agentia_Use`).
    PotionUsed,
    /// Selection confirmed (`Selected_Confirm`): the instrument and the protocol.
    Picked,
    /// Protocol took effect (`Precept_Surgery`).
    Protocol,
    /// Reroll button: `RefreshBtn2` is the instrument, `RefreshBtn` is the potion.
    Reroll { instrument: bool },
    /// New hand (`Figure_Refresh`; 2 lines in the same millisecond, merged by [`VoraxSession::note_cue`]).
    Dealt,
    /// Card animation (`Selected_Shuffle`).
    Shuffle,
    /// Instrument triggered (`Tool_Surgery`).
    Tool,
    /// Monster disappeared (`Monster_DisAppear`).
    Gone,
    /// Monster changed (`Monster_change`).
    Changed,
    /// Card clicked (`S13GamePlayCard_C_SelectBtn`).
    CardClick,
    /// Tank clicked (`S13GamePlayMonsterItem_C_SelectBtn`). `tank` is the tank (0-based, left to right) under the
    /// mouse cursor when the log line was read, filled in by capture.rs ([`tank_at`]); [`cue`] always gives None,
    /// and None also means the cursor was not over a tank (between two tanks, outside the game, or unreadable).
    TankClick { tank: Option<u8> },
    /// Clicked an already held instrument (`RelicBtn`).
    Relic,
    /// The intro page reveals the instrument (`Show_Surgery`).
    Reveal,
    /// Confirm button (`S13GamePlayMain_C_ComfirmBtn`).
    Confirm,
    /// Start-surgery button on the result screen (`FightBtn`).
    Fight,
    /// End-surgery button on the result screen (`GiveUpBtn`).
    GiveUp,
    /// Entered a level other than the endgame battle map (`LevelMgr:_EnterLevelSuccess() N`, where N is not
    /// [`BATTLE_LEVEL`]): the endgame battle is over, back to town or the map.
    LevelLeft,
    /// Entered the endgame battle map (`LevelMgr:_EnterLevelSuccess() 230043`): the endgame battle started, even
    /// when the start-surgery button ([`Cue::Fight`]) was missed (the board page was closed to skip an animation).
    BattleEntered,
    /// A new Vorax game was paid for: the Vorax charges (`ConsumMgr:ResreshConsumData Coin_826 Value N`) went
    /// down, by 12 in all 4 games logged, about 35 ms after the intro page's start button and before the board
    /// opens (every other change was +1, from a map run). Not from one log line alone: capture.rs compares each
    /// value with the one before ([`spent`]); [`cue`] never gives it.
    NewGame,
    /// The game's UI scale: the HUD's `PageScale` in millionths (`gamelog::Signal::UiScale`, sent on by capture.rs as
    /// a Vorax cue so it never reaches Tartarus nor counts as leaving Vorax). Not a move: [`VoraxSession::note_cue`]
    /// records it (the reader reads the frames at that scale, see `crate::vorax::ui_scale`) and writes a `ui_scale`
    /// row; [`cue`] never gives it.
    UiScale { micro: u32 },
}

/// Whether a Vorax charges value (`Coin_826`, `gamelog::VORAX_COIN`) going from `before` to `now` means a new game
/// was paid for: it went down. Unknown before (the first value seen) is not.
pub fn spent(before: Option<u32>, now: u32) -> bool {
    before.is_some_and(|b| now < b)
}

/// The endgame battle map (`LevelMgr:_EnterLevelSuccess() 230043`, the Mad Doctor Shelley's operating room).
pub const BATTLE_LEVEL: &str = "230043";

impl Cue {
    /// New hand dealt or card animation: capture.rs schedules a read after this (tank monsters keep moving).
    pub fn deals(self) -> bool {
        matches!(self, Cue::Dealt | Cue::Shuffle)
    }

    /// Sounds of the resolve process: observe mode saves the resolve strips after this.
    pub fn resolves(self) -> bool {
        matches!(self, Cue::Tool | Cue::Gone | Cue::Changed)
    }

    /// The cue's name in a `vorax_round` row's `cues`: the variant name only, so a tank click stays `TankClick`
    /// as in older logs (its tank goes to `chosen.tanks`).
    fn name(self) -> String {
        match self {
            Cue::TankClick { .. } => "TankClick".to_string(),
            c => format!("{c:?}"),
        }
    }
}

/// Half width (reference pixels) of a tank's click area around its [`read::TANK_X`]. Neighbouring tanks are at
/// least 347 apart, so 165 leaves a dead band of at least 17 between two areas: a cursor there is recorded as
/// unknown rather than guessed. 165 covers the glass of every tank but the outer 22 px of the first one (glass
/// centers differ from the label centers by up to 42, see `crate::vorax` TANK_GLASS_X).
const TANK_HALF_W: f32 = 165.0;
/// Vertical extent (reference pixels) of the tanks' click areas: from the top of the glass (y 100) to the bottom
/// of the label strips (about 580), with a little margin.
const TANK_TOP: f32 = 90.0;
const TANK_BOTTOM: f32 = 600.0;

/// The tank (0-based, left to right) under the cursor: `rect` is the game's client area (left, top, width, height,
/// see `capture::game_rect`) and `cursor` the cursor position, both in the same screen coordinates (physical
/// pixels). The point is mapped to the 2560x1440 reference like [`read::to_reference`] maps a frame (scale by
/// height, center horizontally), so every resolution and aspect gives the same tank. None when the cursor is
/// outside the game or not over exactly one tank.
pub fn tank_at(rect: [i32; 4], cursor: (i32, i32)) -> Option<u8> {
    let [left, top, w, h] = rect;
    if w <= 0 || h <= 0 {
        return None;
    }
    let (dx, dy) = (cursor.0 - left, cursor.1 - top);
    if !(0..w).contains(&dx) || !(0..h).contains(&dy) {
        return None;
    }
    let k = read::H as f32 / h as f32;
    // The same rounding as to_reference and to_frame.
    let scaled_w = (w as f32 * read::H as f32 / h as f32).round();
    let shift = ((scaled_w - read::W as f32) / 2.0).trunc();
    let (x, y) = (dx as f32 * k - shift, dy as f32 * k);
    if !(TANK_TOP..=TANK_BOTTOM).contains(&y) {
        return None;
    }
    let hits: Vec<usize> = read::TANK_X.iter().enumerate().filter(|(_, c)| (x - **c as f32).abs() <= TANK_HALF_W).map(|(i, _)| i).collect();
    match hits[..] {
        [i] => u8::try_from(i).ok(),
        _ => None,
    }
}

/// The cue for each FindRow row name.
const ROWS: [(&str, Cue); 17] = [
    ("Play_UI_S13_KYZ_Card_Agentia_Use", Cue::PotionUsed),
    ("Play_UI_S13_KYZ_Card_Selected_Confirm", Cue::Picked),
    ("Play_UI_S13_KYZ_Card_Precept_Surgery", Cue::Protocol),
    ("S13GamePlayMain_C_RefreshBtn2", Cue::Reroll { instrument: true }),
    ("S13GamePlayMain_C_RefreshBtn", Cue::Reroll { instrument: false }),
    ("Play_UI_S13_KYZ_Card_Figure_Refresh", Cue::Dealt),
    ("Play_UI_S13_KYZ_Card_Selected_Shuffle", Cue::Shuffle),
    ("Play_UI_S13_KYZ_Card_Tool_Surgery", Cue::Tool),
    ("Play_UI_S13_KYZ_Card_Monster_DisAppear", Cue::Gone),
    ("Play_UI_S13_KYZ_Card_Monster_change", Cue::Changed),
    ("S13GamePlayCard_C_SelectBtn", Cue::CardClick),
    ("S13GamePlayMonsterItem_C_SelectBtn", Cue::TankClick { tank: None }),
    ("S13GamePlayRelic_C_RelicBtn", Cue::Relic),
    ("Play_UI_S13_KYZ_Card_Show_Surgery", Cue::Reveal),
    ("S13GamePlayMain_C_ComfirmBtn", Cue::Confirm),
    ("S13GamePlayReward_C_FightBtn", Cue::Fight),
    ("S13GamePlayReward_C_GiveUpBtn", Cue::GiveUp),
];

/// The Vorax cue for one log line. The line must contain `S13` or `ProtoId = 56` before matching goes any
/// further, so each Tartarus line costs only two extra `contains` calls.
pub fn cue(line: &str) -> Option<Cue> {
    if let Some(rest) = line.split("LevelMgr:_EnterLevelSuccess() ").nth(1) {
        let level = rest.split(|c: char| !c.is_ascii_digit()).next().unwrap_or_default();
        if level.is_empty() {
            return None;
        }
        return Some(if level == BATTLE_LEVEL { Cue::BattleEntered } else { Cue::LevelLeft });
    }
    if line.contains("ProtoId = 56") {
        let id = line.split("ProtoId = ").nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?;
        return match id {
            "5619" if line.contains("RecvPushCombineMsg") => Some(Cue::Synced),
            "5621" => Some(Cue::Over),
            _ => None,
        };
    }
    if !line.contains("S13") {
        return None;
    }
    if let Some(rest) = line.split("DispatchPageRunChange PageName = ").nth(1) {
        let (name, state) = rest.split_once(" , PageRunState = ")?;
        let page = match name.trim() {
            "S13GamePlay" => Page::Intro,
            "S13GamePlayMain" => Page::Main,
            "S13GamePlayReward" => Page::Reward,
            _ => return None,
        };
        let state = match state.trim() {
            "Run" => PageState::Run,
            "Hide" => PageState::Hide,
            "Destory" => PageState::Destory,
            _ => return None,
        };
        return Some(Cue::Page(page, state));
    }
    if !line.contains("UDataTable::FindRow") {
        return None;
    }
    // Match the whole row name: only then can RefreshBtn and RefreshBtn2, Selected and Selected_Confirm be told apart.
    let row = line.split("requested row '").nth(1)?.split('\'').next()?;
    ROWS.iter().find(|(r, _)| *r == row).map(|&(_, c)| c)
}

/// Maximum look-back: only the last 8 MB.
const LOOKBACK: u64 = 8 << 20;

/// When the program starts mid-game, whether a Vorax run is in progress: look back over the last 8 MB of
/// the first `end` bytes of `file`, find the last S13 page state line; if it is the board's Run, return
/// that cue. Never relies on reading the screen. A Run of the result screen does not count: no advice is
/// given when the program starts during the result screen, and a lone result page (the player opening
/// Vorax's reward page elsewhere, once for about 4 seconds at 2026-11-03 06:36:39) is not a game.
pub fn under_way<F: Read + Seek>(file: &mut F, end: u64) -> Option<Cue> {
    let from = end.saturating_sub(LOOKBACK);
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(from)).ok()?;
    file.take(end - from).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let last = text.lines().rev().filter(|l| l.contains("DispatchPageRunChange PageName = S13")).find_map(cue)?;
    (last == Cue::Page(Page::Main, PageState::Run)).then_some(last)
}

// ---------------------------------------------------------------------------- state

/// Vorax mode (the state diagram in plan section 2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Not in Vorax: Tartarus runs as usual.
    #[default]
    Off,
    /// Intro page.
    Intro,
    /// Board (choosing potions, instruments, protocols, and the resolve animation).
    Board,
    /// Result screen.
    Result,
    /// Endgame battle (after pressing `開始手術` on the result screen, until leaving the battle map): only the
    /// left panel is read and the bonuses recorded; no advice is given.
    Battle,
}

impl Mode {
    pub fn active(self) -> bool {
        self != Mode::Off
    }
}

/// Whether the mode changed after [`VoraxSession::note_cue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeChange {
    None,
    /// Entered Vorax mode from Off: the caller pauses Tartarus.
    Entered,
    /// Back to Off: the caller resumes Tartarus.
    Left,
}

/// Status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Not in the mode, or the board has not been read yet.
    #[default]
    Idle,
    /// Intro page.
    Intro,
    /// This board was read for the first time; a confirming extra read is in progress.
    Confirming,
    /// Solving.
    Solving,
    /// Advice available.
    Advised,
    /// Resolve animation in progress (or just confirmed, waiting for the new hand); no advice is given.
    Resolving,
    /// Part of the board cannot be read (the reason is in `View::lines`).
    Unreadable,
    /// Protocol: the solver does not solve it.
    Protocol,
    /// Result screen.
    Result,
    /// Endgame battle: only the left panel is read ([`View::battle`]).
    Battle,
}

/// Box style (corresponds to `Style` in mark.rs): cards and reroll buttons are Plain, tanks are Loud.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BoxStyle {
    Plain,
    Loud,
}

/// One box on the game, in frame pixels.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkBox {
    /// What the box surrounds: "card 2", "tank 5", "reroll" (1-based, for the log).
    pub what: String,
    /// Frame pixels: center x, center y, half width, half height.
    pub at: (f32, f32, f32, f32),
    pub style: BoxStyle,
}

/// A Vorax game's last round (the panel's `N/13`).
const LAST_ROUND: i32 = 13;

/// Where a game's final activity in [`GameSummary`] and `vorax_end` came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BoardFrom {
    /// The result screen.
    ResultScreen,
    /// The last board read with a settled final activity (the result screen was left before it was read).
    LastBoard,
    /// The endgame battle panel's activity at its last read (neither the result screen nor a settled board was
    /// read, as when the board page is closed after each round to skip the animation).
    Battle,
}

impl BoardFrom {
    /// The `ultimateFrom` value of `vorax_end` (older logs have only "result" and "board", or no field).
    fn tag(self) -> &'static str {
        match self {
            BoardFrom::ResultScreen => "result",
            BoardFrom::LastBoard => "board",
            BoardFrom::Battle => "battle",
        }
    }
}

/// The last finished Vorax game, kept for the board page after the mode has ended (see
/// [`VoraxSession::last_game`]) until a new Vorax game starts or Tartarus is played.
#[derive(Clone, Debug, PartialEq)]
pub struct GameSummary {
    /// The game's run log folder (for the settlement); None when logging is off.
    pub log_dir: Option<PathBuf>,
    /// When the game ended (UTC ms): the end of the endgame battle, or of the board without one. It also tells
    /// one game from the next.
    pub ended_at: i64,
    /// The board's final activity, and where it was read.
    pub board: Option<i64>,
    pub board_from: Option<BoardFrom>,
    /// How many rounds were seen.
    pub rounds: usize,
    /// The instruments held, in pick order, by their names in the current language.
    pub instruments: Vec<String>,
    /// The endgame battle's last panel read; None when there was no battle or the panel was never read.
    pub battle: Option<read::Battle>,
}

impl GameSummary {
    /// The battle's total: its activity plus its bonus.
    pub fn total(&self) -> Option<f64> {
        let b = self.battle.as_ref()?;
        Some(b.activity? + b.bonus?)
    }
}

/// Everything the window needs to display.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub mode: Mode,
    pub status: Status,
    /// Status description; the first line is the status line (current language); when unreadable, the reason
    /// follows (at most 4 entries).
    pub lines: Vec<String>,
    /// The kind of screen read most recently.
    pub kind: Option<Kind>,
    pub round: Option<i32>,
    pub ultimate: Option<i64>,
    /// The two remaining reroll counts tracked for this game (None until seen).
    pub potion_rerolls: Option<i32>,
    pub instrument_rerolls: Option<i32>,
    /// The advice text; when `stale`, it is the last usable advice for the same round and same hand (greyed out).
    pub advice: Option<AdviceRows>,
    pub stale: bool,
    /// Number of rollouts solved and the time taken.
    pub rollouts: u32,
    pub solve_ms: Option<u64>,
    /// Boxes to draw on the game (frame pixels); when empty, everything is hidden.
    pub boxes: Vec<MarkBox>,
    /// Just confirmed or rerolled, waiting for the new hand (gate): boxes are not shown.
    pub gated: bool,
    /// A choice inside the potion kit.
    pub in_kit: bool,
    /// The board that was read (`crate::vorax::describe`, expandable).
    pub board: Option<String>,
    /// The six tanks and the held instruments on the last steady selection screen (the panel's board
    /// picture). A held item is (name, whether it was revealed at the start); an unrecognized icon's name is "?".
    pub tanks: Vec<read::Tank>,
    pub held: Vec<(String, bool)>,
    /// What the left panel of the endgame battle showed when last read (only in [`Mode::Battle`]).
    pub battle: Option<read::Battle>,
    /// The endgame battle's Give Up button to box, with the line that giving up settles at once: every reward tier
    /// taken on [`GIVE_UP_READS`] reads in a row, with the setting on ([`Config::give_up_hint`]); frame pixels.
    pub give_up: Option<MarkBox>,
    /// Frame size of the last read.
    pub frame_size: Option<(usize, usize)>,
    /// Why Vorax is open but did not enter the mode (the game client is not Traditional Chinese): one line is
    /// shown at the very top of the window, and Tartarus runs as usual. May also be set while the mode is Off.
    pub blocked: Option<String>,
    /// Observe mode note (the log folder's disk has under 5 GB, so only the log is written, no frames); normally None.
    pub learn_note: Option<String>,
    /// Incremented every time the content changes.
    pub version: u64,
}

/// Settings (from [`Settings`]).
#[derive(Clone, Debug)]
pub struct Config {
    /// When Vorax mode is switched off entirely, cues are still received but the mode is not entered.
    pub enabled: bool,
    /// Number of rollouts per option.
    pub rollouts: u32,
    /// Observe mode (step 7).
    pub learn: bool,
    /// Resolve strips (step 7).
    pub strips: bool,
    /// Root folder of the log (`<app_dir>\vorax`); None writes no log.
    pub log_root: Option<PathBuf>,
    /// Reading and solving both run on the caller's thread (replay tests).
    pub inline: bool,
    /// Free space (bytes) observe mode needs to save jpg: from settings it is 5 GB, replay tests use 0.
    pub min_free_bytes: u64,
    /// The general observe mode (settings `observe`): with it, or with `learn`, a board read as `Kind::Other` saves
    /// its whole frame ([`Learn::full_frame`]).
    pub observe: bool,
    /// The endgame battle's give-up hint (settings `vorax_give_up_hint`, off by default).
    pub give_up_hint: bool,
}

impl Config {
    pub fn from_settings(s: &Settings) -> Config {
        Config {
            enabled: s.vorax_enabled,
            rollouts: s.vorax_rollouts,
            learn: s.vorax_learn,
            strips: s.vorax_strips,
            log_root: s.run_log.then(super::settings::app_dir).flatten().map(|d| d.join("vorax")),
            inline: false,
            min_free_bytes: super::vorax_learn::MIN_FREE,
            observe: s.observe,
            give_up_hint: s.vorax_give_up_hint,
        }
    }
}

/// The current UTC milliseconds (for the UI; VoraxSession itself does not read the clock).
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Convert UTC milliseconds to the time of a log row.
fn at_time(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64)
}

/// After receiving Synced (confirm or reroll), wait for the new hand: until Dealt/Shuffle arrives or this
/// much time passes, frames are not OCRed (resolve animation; reading would give no advice anyway). In the
/// tutorial game after a 28k reroll the same 3 cards just change order, with no Figure_Refresh; it is
/// released by Shuffle or the timeout.
pub const GATE_MS: i64 = 2500;
/// When the instrument reroll count has not been seen yet: 2 at the start (the solver's default is 0, which underestimates the value of rerolls).
const INSTRUMENT_REROLLS: i32 = 2;
/// The file in the Vorax log root that remembers the instrument rerolls seen at the start of recent games: the pact
/// spirit Intern Shelley adds 1 or 2 by rank, so the opening count is learned instead of assumed
/// ([`VoraxSession::opening_instrument_rerolls`]).
const OPENINGS_FILE: &str = "opening-rerolls.json";
/// How many recent games the opening instrument reroll count is learned from.
const OPENINGS_KEPT: usize = 5;

/// The instrument reroll counts remembered in `root` ([`OPENINGS_FILE`]), oldest first; empty when there are none.
fn load_openings(root: &std::path::Path) -> Vec<i32> {
    std::fs::read_to_string(root.join(OPENINGS_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v["instrument"].as_array().map(|a| a.iter().filter_map(|n| n.as_i64()).map(|n| n as i32).collect()))
        .unwrap_or_default()
}

/// The opening count to assume: the most frequent of the remembered ones, the newest on a tie; None when none.
fn opening_default(seen: &[i32]) -> Option<i32> {
    seen.iter().copied().max_by_key(|n| seen.iter().filter(|m| **m == *n).count())
}
/// When the potion reroll count has not been seen yet: 3 at the start of both games (plan section 10, point 2).
const POTION_REROLLS: i32 = 3;
/// Number of reasons listed when the board is unreadable.
const WHY_SHOWN: usize = 4;
/// JPEG quality of the frame saved for a new steady board (one 2560x1440 frame is about 0.3 to 0.6 MB).
const FRAME_QUALITY: u8 = 92;

/// The lossless PNGs a game saves with the general observe mode on (the user, 2026-11-09), each with the read it goes
/// with in a `vorax_lossless` row (`screen`, `file`, and the read's time, signature and reading or panel): the JPEG
/// frames lose the fine strokes a misread turns on (2026-11-09: no older reader read the logged values back off them),
/// so only these can check whether a read was right. Reports leave them out (only `.jpg` goes in), and the folder goes
/// with its game when run logs are pruned.
/// - Every selection screen (pick, reroll, protocol): the parts its reads use ([`read::evidence`], about 330 KB) of the
///   read the advice was solved on (`role` "advised"), and of the last steady read before the hand ended (`role`
///   "confirmed", the one likeliest clear of effects), the second only when it read otherwise (`evidence-NNNN.png`,
///   see [`VoraxSession::save_evidence`]). Reads with a problem (incomplete, mid animation) are not saved for these;
///   a steady one is saved once a round as `role` "unreadable", the misread to look into.
/// - Once a game each ([`LOSSLESS`], `lossless-<screen>-NNNN.png`): the whole frame of the first steady pick and of
///   the steady result screen (about 1.8 MB each; their `vorax_read` rows name it as `png`), and the battle panel read
///   the same twice in a row.
const LOSSLESS: [&str; 3] = ["pick", "result", "battle"];

/// A frame kept for its evidence image: the read's signature, time and round, the frame, and the interface scale it
/// was read at.
#[derive(Clone)]
struct Shot {
    sig: String,
    at: i64,
    round: Option<i32>,
    frame: Rgba,
    ui: Option<f64>,
}
/// Windows THREAD_PRIORITY_BELOW_NORMAL: the solver thread runs below normal priority so the game keeps its frame rate.
const BELOW_NORMAL: i32 = -1;
/// The note at the top of the window when the game client is one the reader does not read (neither Traditional Chinese nor, with the feature `en-vorax`, English; both languages are written).
const NOT_ZH: &str = "Vorax advice reads the 繁體中文 and English game clients only; Tartarus goes on as usual. 渴血手術的即時建議只讀繁中與英文客戶端，塔羅斯照常。";

fn t(zh: &'static str, en: &'static str) -> &'static str {
    if crate::lang::zh() { zh } else { en }
}

/// The result of one read.
struct Done {
    reading: Reading,
    frame: Rgba,
    at: i64,
    trigger: String,
    read_ms: u64,
    /// The interface scale the frame was read at (`ui_scale::Scaled::s`): None for the original setting.
    s: Option<f64>,
    /// Features of the held icons (left to right): computed only when there are unrecognized icons (not counting the ones revealed at the start), for observe mode.
    icons: Option<Vec<Vec<f32>>>,
    /// Diagnostics: the client the reader read as ([`read::english`]), the global language and the number of reader
    /// switches ([`read::english_sets`]) when the read started.
    client: Value,
}

/// Diagnostics of a read: `english` (the reader read the English client), `lang` (the global language) and `sets`
/// (reader switches so far, 0 without the feature `en-vorax`).
fn read_client() -> Value {
    #[cfg(feature = "en-vorax")]
    let sets = read::english_sets();
    #[cfg(not(feature = "en-vorax"))]
    let sets = 0;
    json!({ "english": read::english(), "lang": crate::lang::lang().key(), "sets": sets })
}

/// Read one frame (converted to the reference size, then read) of a game drawn at interface scale `ui` (see
/// [`VoraxSession::ui`]; at the original setting or None it is read as before). When there are unrecognized held
/// icons, the icon features are computed as well (on the read thread, not occupying the main thread), from the
/// reference the reading used.
fn read_frame(frame: Rgba, at: i64, trigger: String, ui: Option<f64>) -> Done {
    let clock = Instant::now();
    let client = read_client();
    let reference = read::to_reference(&frame);
    let scaled = crate::vorax::ui_scale::read(&reference, Some(&frame), ui);
    let reading = scaled.reading;
    let read_ms = clock.elapsed().as_millis() as u64;
    let icons = reading.held.iter().any(|h| h.id.is_none() && !h.opening).then(|| crate::vorax::icon_features(scaled.unscaled.as_ref().unwrap_or(&reference)));
    Done { reading, frame, at, trigger, read_ms, s: scaled.s, icons, client }
}

/// The result of one solve.
struct Solved {
    sig: String,
    x: ScreenState,
    advice: Advice,
    ms: u64,
    read_at: i64,
    rounds_known: bool,
    rollouts: u32,
    /// A solve started while the final activity was still ticking (`ultimate_pending`) (solving while it ticks).
    early: bool,
    /// [`VoraxSession::hand_moves`] when the solve started.
    moves: u32,
}

/// Whether two solver inputs are the same (`ScreenState` has no `PartialEq`, so compare the JSON).
fn same_state(a: &ScreenState, b: &ScreenState) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// A solve is split into several slices, checking `cancel` between slices: 1024 futures per slice (4096 is 4 slices; with 512 per slice it is about 5% slower than doing it in one go).
/// Used only for plain (`RACE_Z` is None); each batch of racing is a slice in itself.
const SOLVE_SLICE: u32 = 1024;

/// The racing (early stopping) threshold Z: options whose gap is more than Z standard errors are eliminated. None = plain (every option plays the full `rollouts`).
/// 2.2: over 105 real-game boards x 3 seeds, agreement and maximum loss match 4096 plain, and the median time is about -79%
/// (Vorax solver experiment (2026-11-04) section 3; consistent with the 2.2 Tartarus uses).
const RACE_Z: Option<f64> = Some(2.2);
/// First batch of racing: every option first plays 512 futures (256 is faster, but costs one more case where plain is right and racing is wrong).
const RACE_FIRST: u32 = 512;
/// After that, racing checks every 256 futures (also the interval at which `cancel` is checked).
const RACE_STEP: u32 = 256;

/// Solve: `rollouts` is the maximum number of futures per option. With racing (`RACE_Z`) it stops early; with plain the result is
/// exactly the same as `advise_plan(x, Some(rollouts), true)`. Both run batch by batch, and if `cancel` is true between batches
/// it gives up (returns None), so a new read does not have to queue behind an old solve.
fn solve_now(pool: Option<&rayon::ThreadPool>, x: &ScreenState, rollouts: u32, cancel: &AtomicBool) -> Option<(Advice, u64)> {
    solve_with(pool, x, rollouts, RACE_Z, cancel)
}

/// `solve_now` with the racing Z specified (None = plain, in slices of `SOLVE_SLICE` each).
fn solve_with(pool: Option<&rayon::ThreadPool>, x: &ScreenState, rollouts: u32, race_z: Option<f64>, cancel: &AtomicBool) -> Option<(Advice, u64)> {
    let clock = Instant::now();
    // With 0 futures the options have no results and conclude would panic (even if the config file says 0, count at least 1).
    let rollouts = rollouts.max(1);
    let run = || -> Option<Advice> {
        match (prepare(x, Some(rollouts)), race_z) {
            (Prepared::Done(a), _) => Some(a),
            (Prepared::Work { work, x, unknown, t0 }, Some(z)) => {
                let race = Race { z, first: RACE_FIRST, step: RACE_STEP, max: rollouts, paired: false };
                let r = play_raced_until(&work, &x, &race, true, &|| cancel.load(Ordering::Relaxed))?;
                // Eliminated options are normalized to the same number of futures (`same_n`); elimination itself does not change the choice.
                Some(conclude(&work, &x, unknown, t0, &same_n(&r.tallies)))
            }
            (Prepared::Work { work, x, unknown, t0 }, None) => {
                let total = work.futures();
                let mut tallies: Vec<Tally> = Vec::new();
                let mut from = 0;
                while from < total {
                    if cancel.load(Ordering::Relaxed) {
                        return None;
                    }
                    let to = (from + SOLVE_SLICE).min(total);
                    let part = play_work(&work, from, to, true);
                    if tallies.is_empty() {
                        tallies = part;
                    } else {
                        for (t, p) in tallies.iter_mut().zip(&part) {
                            (t.goal, t.reached, t.n) = (t.goal + p.goal, t.reached + p.reached, t.n + p.n);
                        }
                    }
                    from = to;
                }
                Some(conclude(&work, &x, unknown, t0, &tallies))
            }
        }
    };
    // par_iter with par=true uses this pool inside install (core count minus one, below normal priority), not the global pool.
    let advice = match pool {
        Some(p) => p.install(run)?,
        None => run()?,
    };
    Some((advice, clock.elapsed().as_millis() as u64))
}

/// A background solve: when it is dropped (replaced by a new one, confirmed, left the mode), the solver thread is told to give up.
struct Solving {
    sig: String,
    /// Not computed from a steady board (early solve or solving while the value ticks).
    early: bool,
    rx: Receiver<Solved>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Solving {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// A board that has advice.
struct Advised {
    sig: String,
    round: Option<i32>,
    hand: Vec<Option<String>>,
    /// The remaining reroll counts on screen at solve time.
    rerolls: Option<i32>,
    /// [`VoraxSession::hand_moves`] at solve time: if a confirm or reroll happened afterwards, it is not the same hand (the log line for
    /// dealing a new hand arrives about 0.6 seconds after the screen and does not count, otherwise the line arriving after the advice was given would invalidate the same-hand check).
    moves: u32,
    x: ScreenState,
    advice: Advice,
}

/// Normal number of instrument choices in one game (the ones revealed at the start do not count).
const MAX_PICKS: usize = 3;

/// When the value is ticking (`ultimate_pending`), how long "read again right after a read" lasts at most.
const PENDING_MAX_MS: i64 = 3000;

/// The last steady read (two identical reads in a row).
struct Stable {
    sig: String,
    reading: Reading,
    size: (usize, usize),
    /// The interface scale the reading was read at (`Done::s`): the boxes are drawn at it.
    ui: Option<f64>,
}

/// The gate that waits for the new hand.
struct Gate {
    since: i64,
    dealt: bool,
}

/// An instrument that was just confirmed: how many were held before confirming (not counting the ones revealed at the start), and the solver's acquisition round.
struct PendingPick {
    before: usize,
    round: i32,
    /// The chosen card `{en, zh, rawName}` (used by observe mode to match a new icon).
    card: Value,
    /// The solver id of the chosen card: only available when the card name itself was read (not inferred from the effect text). Used to
    /// record it into `game.held` when the new icon is not recognized.
    id: Option<u8>,
    /// The solver id of the card the tooltip last named in this hand before the confirm (`Game::hovered`, kept
    /// through reads with no tooltip), or else the one at the last card click (`Game::click_at`): when the new icon
    /// disagrees with the raised card but agrees with this one, the icon is taken.
    hovered: Option<u8>,
}

/// A game whose board or result page was closed before it ended (its `vorax_end` is not written yet). The game
/// server keeps such a game until it ends, however long it takes (2026-11-04: 45 and 91 minutes later, with a whole
/// Tartarus run in between), so a paused game ends only on an explicit signal, never on time: start surgery
/// ([`Cue::Fight`]), a confirmed give-up (GiveUp, then the result page closing), or a new game ([`Cue::NewGame`],
/// or the intro page hidden into a board). Going back to Tartarus, the game client closing or crashing, and Ascendant
/// closing all leave it paused (Ascendant finds it again on start, see [`paused_on_disk`]).
/// Its `game.round_row` (the last pick, with `chosen.tanks`) is kept too: it gets its `after` from the first steady
/// selection read after the resume, or is written with a null `after` when the paused game is ended instead
/// ([`VoraxSession::end_paused`]).
struct Paused {
    game: Game,
    /// Time of the pause (Main Destory): the back-filled `vorax_end` uses this time.
    at: i64,
    last_round: i32,
    /// The game's log folder, when it was found on disk at start ([`paused_on_disk`]); None for a game paused in
    /// this run of Ascendant, whose folder is still the log's current one.
    dir: Option<PathBuf>,
}

/// Whether `cue` takes the paused game `p` back as the same game (the one place this is decided; see
/// [`VoraxSession::paused_entry`] and [`VoraxSession::resume_paused`]): the board's Main Run always (a new game
/// would have ended the paused one first), and once the paused game reached the last round, its result page
/// (Reward Run), the start-surgery button or the battle map. A result page while the paused game is short of the
/// last round belongs to an older game already fought (the entry shows it before Intro), and is not taken.
/// A game whose rounds stopped being read short of the last one (the UI scaled so the reader sees no board,
/// 2026-11-06 06:15) is taken back by the start-surgery button or the battle map once it has at least [`LAST_ROUND`]
/// picks ([`by_picks`]), never by its result page alone.
fn takes_back(p: &Paused, cue: Option<Cue>) -> bool {
    match cue {
        Some(Cue::Page(Page::Main, PageState::Run)) => true,
        Some(Cue::Page(Page::Reward, PageState::Run)) => p.last_round >= LAST_ROUND,
        Some(Cue::Fight | Cue::BattleEntered) => p.last_round >= LAST_ROUND || by_picks(p),
        _ => false,
    }
}

/// Whether the paused game `p`, short of the last round by the rounds read (none read at all, or the reader lost the
/// board partway), has enough picks to have reached it: its `vorax_round` rows (rerolls not counted) stand in for the
/// round number there. The count is only a lower bound: every round takes at least one pick, but finished games
/// logged 17 to 23 of them, so 13 picks can still be round 8 or 9. It is therefore trusted only for start surgery and the battle map, which no game short of its last round
/// can show. The result page is not taken back on it: the entry's result page of an older game, given up at once
/// (Reward Run, GiveUp, Reward Destory), would otherwise take the paused game and end it.
fn by_picks(p: &Paused) -> bool {
    p.last_round < LAST_ROUND && p.game.picks >= LAST_ROUND as u32
}

/// The monsters in the six tanks, as `archetype/rarity` (None for an empty or unread slot): what a resumed board is
/// checked against ([`monsters_differ`]).
fn monsters(tanks: &[read::Tank]) -> Vec<Option<String>> {
    tanks.iter().map(|t| serde_json::to_value(t).ok().and_then(|v| monster_key(&v))).collect()
}

/// [`monsters`] for one tank as logged (`reading.tanks[i]`).
fn monster_key(v: &Value) -> Option<String> {
    let a = v.get("archetype")?.as_str()?;
    Some(format!("{a}/{}", v.get("rarity").and_then(Value::as_str).unwrap_or("?")))
}

/// Whether two tank rows hold other monsters: no slot read in both has the same monster. Unknown when either has
/// none read, which counts as not different.
fn monsters_differ(old: &[Option<String>], new: &[Option<String>]) -> bool {
    let read = |m: &[Option<String>]| m.iter().any(Option::is_some);
    read(old) && read(new) && !old.iter().zip(new).any(|(a, b)| a.is_some() && a == b)
}

/// UTC milliseconds of a log row's `at` (`2026-11-05T15:55:48.368Z`, see [`timestamp`]).
fn parse_at(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec, ms) = (num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?);
    // Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's days_from_civil).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}

/// The game left paused when Ascendant last closed (or lost track of it), from the newest log folder under `root`
/// that holds a game (folders with no game in them, as an intro page opened and closed, are passed over): a folder
/// with no `vorax_end` whose last `vorax_pause` row has no `vorax_resume` after it. Folders written before
/// 2026-11-06 have no `vorax_pause`: there the last `vorax_mode` row must be the board closing (Main Destory), and
/// the game is rebuilt from the steady reads and the last advice. The folder goes in [`Paused::dir`].
fn paused_on_disk(root: &Path) -> Option<Paused> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root).ok()?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.join("log.jsonl").is_file()).collect();
    dirs.sort();
    for dir in dirs.into_iter().rev() {
        let Ok(text) = std::fs::read_to_string(dir.join("log.jsonl")) else { continue };
        let rows: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        if !has_game(&rows) {
            continue;
        }
        let mut p = paused_in(&rows)?;
        p.dir = Some(dir);
        return Some(p);
    }
    None
}

/// Whether a folder's rows hold a game: a steady read, a pick, a pause or an ending.
fn has_game(rows: &[Value]) -> bool {
    rows.iter().any(|r| matches!(r["kind"].as_str(), Some("vorax_round" | "vorax_pause" | "vorax_end")) || (r["kind"] == "vorax_read" && r["steady"] == true))
}

/// [`paused_on_disk`] for the rows of one folder.
fn paused_in(rows: &[Value]) -> Option<Paused> {
    let is = |r: &Value, k: &str| r["kind"] == k;
    if rows.iter().any(|r| is(r, "vorax_end")) {
        return None;
    }
    let last_resume = rows.iter().rposition(|r| is(r, "vorax_resume"));
    if let Some(i) = rows.iter().rposition(|r| is(r, "vorax_pause")) {
        return if last_resume.is_some_and(|k| k > i) { None } else { paused_from_row(&rows[i]) };
    }
    let i = rows.iter().rposition(|r| is(r, "vorax_mode"))?;
    let m = &rows[i];
    if m["to"] != "off" || m["cue"] != "Page(Main, Destory)" || last_resume.is_some_and(|k| k > i) {
        return None;
    }
    let steady: Vec<&Value> = rows[..i].iter().filter(|r| is(r, "vorax_read") && r["steady"] == true).collect();
    let rounds_seen: BTreeSet<i32> = steady.iter().filter_map(|r| r.pointer("/reading/round")?.as_i64()).map(|n| n as i32).collect();
    let last_round = *rounds_seen.last()?;
    let choice = |r: &&&Value| matches!(r.pointer("/reading/kind").and_then(Value::as_str), Some("potion" | "instrument" | "protocol"));
    let monsters = steady.iter().rfind(choice).and_then(|r| r.pointer("/reading/tanks")?.as_array().map(|t| t.iter().map(monster_key).collect())).unwrap_or_default();
    let state = rows[..i].iter().rfind(|r| is(r, "vorax_advice")).map(|r| &r["state"]);
    let instrument = |name: &str| {
        crate::vorax::names::names().cards.iter().find(|c| c.category == crate::vorax::names::Category::Instrument && c.en == name).and_then(|c| c.id)
    };
    let held = state
        .and_then(|s| {
            let names = s["instruments"].as_array()?;
            let rounds = s["instrumentRounds"].as_array()?;
            Some(names.iter().zip(rounds).filter_map(|(n, r)| Some((instrument(n.as_str()?)?, r.as_i64()? as i32))).collect())
        })
        .unwrap_or_default();
    let refreshes = |k: &str| state.and_then(|s| s[k].as_i64()).map(|n| n as i32);
    let game = Game {
        rounds_seen,
        held,
        monsters,
        potion_rerolls: refreshes("potionRefreshes"),
        instrument_rerolls: refreshes("instrumentRefreshes"),
        ..Game::default()
    };
    Some(Paused { game, at: parse_at(m["at"].as_str()?)?, last_round, dir: None })
}

/// The fields of a `vorax_pause` row for game `g` ([`VoraxSession::pause`]; read back by [`paused_from_row`]).
fn pause_fields(g: &Game, last_round: i32, why: &str) -> Value {
    json!({
        "why": why,
        "lastRound": last_round,
        "roundsSeen": g.rounds_seen,
        "held": g.held,
        "fromCard": g.from_card,
        "openings": g.openings,
        "potionRerolls": g.potion_rerolls,
        "instrumentRerolls": g.instrument_rerolls,
        "monsters": g.monsters,
        "lastUltimate": g.last_ultimate,
        "finalPicked": g.final_picked,
        "pending": g.round_row,
        "kit": g.kit,
        // An instrument confirmed in the last round, not seen on the screen yet: taken into `held` after the resume.
        "pendingPick": g.pending_pick.as_ref().map(|p| json!({ "before": p.before, "round": p.round, "card": p.card, "id": p.id, "hovered": p.hovered })),
        // Play seen without a round read, and the picks made: what keeps a game whose board is never read paused
        // and lets its battle take it back ([`by_picks`]).
        "played": g.played,
        "picks": g.picks,
    })
}

/// The game kept in one `vorax_pause` row ([`VoraxSession::pause`]).
fn paused_from_row(r: &Value) -> Option<Paused> {
    let ints = |k: &str| r[k].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<i64>>()).unwrap_or_default();
    let held = r["held"].as_array().map(|a| a.iter().filter_map(|h| Some((u8::try_from(h[0].as_u64()?).ok()?, h[1].as_i64()? as i32))).collect()).unwrap_or_default();
    let game = Game {
        rounds_seen: ints("roundsSeen").into_iter().map(|n| n as i32).collect(),
        held,
        from_card: ints("fromCard").into_iter().map(|n| n as usize).collect(),
        openings: r["openings"].as_u64().map(|n| n as usize),
        potion_rerolls: r["potionRerolls"].as_i64().map(|n| n as i32),
        instrument_rerolls: r["instrumentRerolls"].as_i64().map(|n| n as i32),
        monsters: r["monsters"].as_array().map(|a| a.iter().map(|m| m.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        last_ultimate: r["lastUltimate"].as_i64(),
        final_picked: r["finalPicked"].as_bool().unwrap_or(false),
        round_row: Some(r["pending"].clone()).filter(|v| v.is_object()),
        kit: r["kit"].as_bool().unwrap_or(false),
        pending_pick: r["pendingPick"].as_object().and_then(|p| {
            Some(PendingPick {
                before: usize::try_from(p.get("before")?.as_u64()?).ok()?,
                round: i32::try_from(p.get("round")?.as_i64()?).ok()?,
                card: p.get("card").cloned().unwrap_or(Value::Null),
                id: p.get("id").and_then(Value::as_u64).and_then(|n| u8::try_from(n).ok()),
                hovered: p.get("hovered").and_then(Value::as_u64).and_then(|n| u8::try_from(n).ok()),
            })
        }),
        played: r["played"].as_bool().unwrap_or(false),
        picks: r["picks"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
        ..Game::default()
    };
    Some(Paused { game, at: parse_at(r["at"].as_str()?)?, last_round: r["lastRound"].as_i64()? as i32, dir: None })
}

/// The game that was just resumed: its round has not been read on a selection screen yet. Only a smaller round and
/// other monsters in the tanks together make it a new game: one misread number must not cut the game in two (the
/// round is often the same as before the pause, since one round has several selection screens).
#[derive(Clone)]
struct Resumed {
    at: i64,
    last_round: i32,
    monsters: Vec<Option<String>>,
}

impl Drop for VoraxSession {
    /// Program closed: a game on its board or result page is paused (written as `vorax_pause`, found again on the
    /// next start), a paused one stays paused, and an endgame battle still writes its ending from the last panel read
    /// (RunLog is dropped later, so the writes work).
    fn drop(&mut self) {
        if self.in_play_from(self.mode, None) {
            self.pause(self.clock, "ascendant closed", self.mode == Mode::Result);
        }
        if self.mode == Mode::Battle {
            self.end_battle(self.clock);
        }
    }
}

/// Per-game tracking state (reset on entering the mode).
#[derive(Default)]
struct Game {
    /// (Solver id, acquisition round) in acquisition order.
    held: Vec<(u8, i32)>,
    pending_pick: Option<PendingPick>,
    /// Remaining reroll counts last read on the potion and instrument screens.
    potion_rerolls: Option<i32>,
    instrument_rerolls: Option<i32>,
    /// Cues since the last Synced.
    cues: Vec<Cue>,
    /// The next potion screen is a choice inside the potion kit.
    kit: bool,
    /// The `vorax_round` row not written out yet (waiting for the steady read after the pick).
    round_row: Option<Value>,
    /// The last steady selection screen of the hand in play (its signature and reading): the `before`, hand and card
    /// positions of the next pick ([`VoraxSession::synced`] takes it). A steady read of another kind does not replace
    /// it: picking a potion that needs a target turns the screen into a resolving read before the confirm (2026-11-06
    /// 04:43 game, round 8: the resolving read became the pick's `before`, with no hand and no card). Nor does a read
    /// of the same hand with more cards unreadable (the tooltip covering a card name). Kept through a pause.
    choice: Option<(String, Reading)>,
    ended: bool,
    rounds_seen: BTreeSet<i32>,
    /// The game was played on its board whether or not a round was read: a move cue ([`VoraxSession::board_cue`]) or a
    /// `vorax_round` row. With `rounds_seen` it is what makes the game under way ([`VoraxSession::in_play_from`]).
    played: bool,
    /// The board was opened by its Main Run in this stretch (set again on every reopening, not kept in a pause row).
    board_run: bool,
    /// `vorax_round` rows of this game, rerolls not counted ([`by_picks`]).
    picks: u32,
    /// Time of the last Dealt: two lines in the same millisecond are merged.
    last_dealt: Option<i64>,
    /// Signature of the last saved frame.
    saved_sig: Option<String>,
    /// The kinds of screen ([`LOSSLESS`]) whose lossless frame this game has saved.
    lossless: Vec<&'static str>,
    /// Observe mode: this hand's last steady, complete selection screen read, kept for its evidence image
    /// ([`VoraxSession::save_evidence`]).
    steady_shot: Option<Shot>,
    /// The signature of the read whose evidence was saved for this hand's advice (`advised`).
    evidence_sig: Option<String>,
    /// The round whose evidence of a steady selection screen read with a problem was saved (`unreadable`, once a
    /// round).
    unreadable_round: Option<Option<i32>>,
    /// Signatures of this hand that were already solved early ([`VoraxSession::solve_ahead`]; cleared on confirm or reroll).
    tried: BTreeSet<String>,
    /// Number of instruments revealed at the start ([`VoraxSession::mark_openings`]; None until the round 1 instrument selection screen has been seen).
    openings: Option<usize>,
    /// Positions in `held` recorded from the card that was raised at confirm time (the new icon is unrecognized, or does not match the card): these slots do not look at
    /// the icons on screen ([`VoraxSession::screen_state_with`]).
    from_card: BTreeSet<usize>,
    /// The tanks' monsters on the last steady selection screen ([`monsters`]): a resumed board is checked against them.
    monsters: Vec<Option<String>>,
    /// The raised card last read in this hand (the card has often dropped before confirm, so the last read does not see it).
    raised_seen: Option<Seen>,
    /// The card raised before `raised_seen` when another card was raised after it: two cards raised in one hand,
    /// the last one under the tooltip, make the pick unknown ([`hover_pass`]). A card picked drops as soon as the
    /// mouse moves on, and the card the mouse passes over on its way to Confirm rises the same height
    /// (2026-11-08_18-44-54 round 2: `益生黴溶液` picked, `細肢藥粉-骨衛兵` read raised on the last three reads,
    /// and the row named it).
    raised_other: Option<Seen>,
    /// The card the left tooltip last pointed at. Text read with no tooltip shown (`0 / 2`) leaves it as it is
    /// (2026-11-06_13-46-25 round 1: `孵化囊` matched, then three reads of `0 / 2` wiped it and the pick was lost); a
    /// shown tooltip naming no card or a read of another hand drops it ([`hovered_kept`]), and the confirm or
    /// reroll takes it.
    hovered: Option<Seen>,
    /// The cards the left tooltip named in this hand, with the time of the frame each was read on (at most
    /// [`HOVER_LOG`]): a card click takes the one under the mouse from it ([`Game::click_at`]).
    hover_log: Vec<(i64, Seen)>,
    /// Time of the last card click of this hand: at the confirm the card the tooltip named on the last frame taken
    /// at or before it ([`Game::hover_log`]) is the pick when nothing else names it. It is looked up then, not when
    /// the click's line comes in, since frames taken before the click may still be in OCR. Moving the mouse on to
    /// a tank shows a monster's tooltip, which drops `hovered` (2026-11-06_20-03-09 round 10: `孿生激素-蠱蟲`
    /// hovered and clicked, then `暴食巨蠕蟲`, and the row's card was null).
    click_at: Option<i64>,
    /// The last final activity read on a board with no pending `+N` after the last round's pick (the fallback when
    /// the result screen is not read); see `final_picked`.
    last_ultimate: Option<i64>,
    /// The interface scale measured on this game's frames while the game log has given none ([`VoraxSession::ui`]):
    /// later frames are read at it directly instead of first as at the original setting and then again.
    measured_ui: Option<f64>,
    /// The last round's (13) pick was confirmed: only reads after it hold the game's final activity (ranked replay:
    /// the round 13 choice screen still showed 659506 of a final 833372).
    final_picked: bool,
    /// The last steady selection screen of this hand with every card name read ([`carry_names`]): a later read of
    /// the same hand with one card unread takes that card's name from it. Dropped on every deal, reroll and
    /// confirm cue, and never set from a frame taken before the last of them (`hand_from`).
    named_hand: Option<Reading>,
    /// The read just before this one when it was a selection screen with every card name read on it (none carried),
    /// steady or not, taken after the last deal, reroll or confirm cue: [`carry_names`] also takes a name from it
    /// (2026-11-06_16-26-30 round 4: the hand read whole once, not steady, at 46.370, then the hovered third card
    /// unread from 46.518 on, and no advice until 49.532). Dropped with `named_hand`.
    prev_named: Option<Reading>,
    /// Time of the last deal, reroll or confirm cue: frames taken before it belong to an earlier hand.
    hand_from: i64,
    /// The last settled final activity read on the board, final or not ([`VoraxSession::check_result`]).
    board_ultimate: Option<i64>,
    /// Time of the last Tool cue after the last round's pick (`final_picked`): round 13's end-of-round effects.
    final_tool: Option<i64>,
}

impl Game {
    /// The game was played: a round read, a move seen on its board, or a pick logged.
    fn has_play(&self) -> bool {
        !self.rounds_seen.is_empty() || self.played
    }
}

/// A card seen on the selection screen: the round, which card, that hand (the cards' English names).
type Seen = (Option<i32>, usize, Vec<Option<String>>);

/// Which card this read points at: when exactly one card is raised, that one; with `tooltip`, look at the left tooltip.
fn seen_card(r: &Reading, tooltip: bool) -> Option<Seen> {
    let k = if tooltip { hovered_card(r)? } else {
        match r.cards.iter().enumerate().filter(|(_, c)| c.raised).map(|(k, _)| k).collect::<Vec<_>>()[..] {
            [k] => k,
            _ => return None,
        }
    };
    Some((r.round, k, hand(r)))
}

/// The selection in this hand: same hand ([`same_cards`]) and round matches (the round is not compared when it
/// cannot be read).
fn seen_in(s: &Option<Seen>, r: &Reading) -> Option<usize> {
    s.as_ref().filter(|h| (h.0.is_none() || r.round.is_none() || h.0 == r.round) && same_cards(&h.2, &hand(r))).map(|h| h.1)
}

/// A raised card read (`s`): it becomes the last raised (`last`), and the one raised before it, when another card,
/// is kept in `other` ([`Game::raised_other`]). A card raised again does not clear `other`: picked, passed over,
/// then the first one raised again is still two cards.
fn note_raised(last: &mut Option<Seen>, other: &mut Option<Seen>, s: Seen) {
    if let Some(prev) = last.replace(s)
        && last.as_ref().is_some_and(|now| now.1 != prev.1)
    {
        *other = Some(prev);
    }
}

/// Whether more than one card of the hand was seen raised: the card raised on the last read, the last one raised in
/// the hand and another raised before it ([`Game::raised_other`]), each None when not of this hand.
fn mixed_raise(last_read: Option<usize>, last: Option<usize>, other: Option<usize>) -> bool {
    let mut cards: Vec<usize> = [last_read, last, other].into_iter().flatten().collect();
    cards.sort_unstable();
    cards.dedup();
    cards.len() > 1
}

/// Whether the card raised on the last read (`last_read`) is only hovered on the way to Confirm: the last read's
/// tooltip names it (`last_tip`) and another card was raised in this hand ([`mixed_raise`]).
fn hover_pass(last_read: Option<usize>, last_tip: Option<usize>, last: Option<usize>, other: Option<usize>) -> bool {
    last_read.is_some() && last_tip == last_read && mixed_raise(last_read, last, other)
}

/// Whether two reads of a hand are the same hand: as many cards (at least one), every card read in both is the same,
/// and at most one position is unread in either. The tooltip often covers the name of the card next to the one
/// pointed at (2026-11-06 04:43 game, round 2: the tooltip named the third card while the second was unread, and
/// the pick was lost because the steady hand had all three).
fn same_cards(a: &[Option<String>], b: &[Option<String>]) -> bool {
    let unread = a.iter().zip(b).filter(|(x, y)| x.is_none() || y.is_none()).count();
    !a.is_empty() && a.len() == b.len() && unread <= 1 && a.iter().zip(b).all(|(x, y)| x.is_none() || y.is_none() || x == y)
}

/// How many cards of the hand were not read.
fn unread_cards(r: &Reading) -> usize {
    r.cards.iter().filter(|c| c.en.is_none()).count()
}

/// Which card the left tooltip on the selection screen (when the mouse rests on a card) is about: the tooltip's card name contains that card's
/// printed name (after normalization; the English one on the English client, see vorax_learn::card_printed), and matches exactly one card.
/// With the Traditional Chinese name only, no English tooltip ever matched, so a neighbour the mouse passed over after the pick won
/// over the shown icon (2026-11-08_08-45-58 round 2).
fn hovered_card(r: &Reading) -> Option<usize> {
    let tip = read::norm(&r.tooltip);
    // A hyphen read as `一` still matches (see vorax_learn::tooltip_has).
    let hits: Vec<usize> = r.cards.iter().enumerate().filter(|(_, c)| super::vorax_learn::card_printed(c).is_some_and(|z| super::vorax_learn::tooltip_has(&tip, z))).map(|(k, _)| k).collect();
    match hits[..] {
        [k] => Some(k),
        _ => None,
    }
}

/// Whether two reads of hands are clearly different hands: another number of cards, or a position whose card was
/// read in both as different cards (an unread card says nothing).
fn other_hand(a: &[Option<String>], b: &[Option<String>]) -> bool {
    a.len() != b.len() || a.iter().zip(b).any(|(x, y)| x.is_some() && y.is_some() && x != y)
}

/// The hover to keep after the selection screen read `r`: a tooltip naming exactly one card of the hand replaces
/// it; text read where the tooltip goes while no tooltip is shown (`0 / 2`, `1 / 2`: always with the panel
/// uncovered, 4,890 reads in the saved games up to 13-46-25) leaves it, and so does no text; a shown tooltip naming
/// no card (the mouse on a tank: `朿 獸` in the 2026-11-04_11-09-37 game, round 6, after `石化脊髓溶液` was
/// pointed at on the way and `Twin Hormone - Pest` was the pick) drops it, and so does a read of another hand
/// (another round, or a position read as another card).
fn hovered_kept(old: Option<Seen>, r: &Reading) -> Option<Seen> {
    if read::norm(&r.tooltip).chars().count() >= 2 {
        if let Some(s) = seen_card(r, true) {
            return Some(s);
        }
        if r.panel_covered {
            return None;
        }
    }
    old.filter(|h| !(h.0.is_some() && r.round.is_some() && h.0 != r.round) && (r.cards.is_empty() || !other_hand(&h.2, &hand(r))))
}

/// How far (reference pixels) a card may sit from where it was on the read its name is carried from: the mouse
/// on a card pushes its neighbours about 47 aside (see `FAN_STEP` in `crate::vorax::read`).
const CARRY_SHIFT: i32 = 60;

/// How many tooltip sightings of a hand [`Game::hover_log`] keeps (reads come about every 0.15 s).
const HOVER_LOG: usize = 64;

/// One card of a selection screen read unread, the hand otherwise the same as on `named` (the read just before with
/// every name read, `Game::prev_named`, or the last steady read of this hand with every name read,
/// `Game::named_hand`): the same kind, round, reroll count (unread on one of
/// them is not the same) and number of cards, every other card the same card, and every card within [`CARRY_SHIFT`] of where it
/// was. The unread card then takes its name from `named` (`how` [`read::How::Carried`]), so one misread name does
/// not hold the advice back (2026-11-06_13-46-25: `鞣製皮革拘束帶` read as `丶皮革拘束帶` held the advice back
/// 1.2 s; `孿生激素-骨衛兵` dimmed by the hover on its neighbour). Returns the card carried. No name is guessed
/// from the text read: the card is only ever the one read on this very hand.
fn carry_names(r: &mut Reading, named: &Reading) -> Option<usize> {
    // The reroll count must read the same on both (a protocol screen has none on either): one read missing it could
    // be the hand after a reroll whose cue has not come yet.
    if !r.kind.is_choice() || r.kind != named.kind || r.round.is_none() || r.round != named.round || r.rerolls != named.rerolls || r.cards.len() < 2 || r.cards.len() != named.cards.len() {
        return None;
    }
    if named.cards.iter().any(|c| c.index.is_none()) {
        return None;
    }
    let unread: Vec<usize> = r.cards.iter().enumerate().filter(|(_, c)| c.index.is_none()).map(|(k, _)| k).collect();
    let [k] = unread[..] else { return None };
    let same = r.cards.iter().zip(&named.cards).enumerate().all(|(j, (c, n))| (j == k || c.index == n.index) && (c.x - n.x).abs() <= CARRY_SHIFT);
    if !same {
        return None;
    }
    let n = &named.cards[k];
    let c = &mut r.cards[k];
    (c.index, c.id, c.en, c.zh, c.how, c.why) = (n.index, n.id, n.en.clone(), n.zh.clone(), Some(read::How::Carried), None);
    Some(k)
}

/// The potion `en`'s targeting in the solver's data (None when `en` is not a potion there).
fn potion_targeting(en: &str) -> Option<vorax::data::Targeting> {
    vorax::data::data().potions.iter().find(|p| p.name == en).map(|p| p.targeting)
}

/// The archetype in each of the six tanks of board signature `sig` (`Some(Pest)`, or `_` for an empty tank); None
/// when the signature has no six tanks.
fn sig_tanks(sig: Option<&str>) -> Option<Vec<&str>> {
    let part = sig?.split('|').nth(3)?;
    let t: Vec<&str> = part.split(';').filter(|s| !s.is_empty()).collect();
    (t.len() == 6).then_some(t)
}

/// Whether potion `en` fits what the pick of its row shows (`cues` the row's cue names, `tanks` the tank under
/// each tank click, `before` and `after` the boards' signatures, `after` None when no board was read after it):
///
/// - Every tank click of the pick counts: a card click after the tank click does not drop the target
///   (2026-11-04_11-33-57 round 2: card, tank 3, card, confirm, and tank 3 took `Bewitching Tincture`).
/// - A potion with no target (and a potion kit) takes no tank click.
/// - A potion with targets takes at least one; more tanks than it takes is a changed mind only when no more of
///   the clicked tanks than it takes changed (which needs the board after it).
/// - With the confirm seen, a potion kit has no potion used after it and any other potion has one.
///
/// The archetypes on the board after are not compared: the instruments' reactions and fusions change them too (an
/// offline check over the saved games had `Twin Hormone - Pest` picks, the suggested card, with no more Pest tanks
/// after).
///
/// A card that is no potion of the solver's data, or not read, fits (nothing to tell it by); with no card click
/// among the cues (the game log not seen) the clicks are not checked. The two picks this
/// was made for (2026-11-06_12-23-50 round 6, `Spinal Solution - Aemberon` takes no target but tank 1 was clicked
/// twice; 2026-11-06_13-46-25 round 3, `Pure Leech Solution` takes one but two
/// tanks were clicked and both changed) were the suggested other card.
///
/// `Some(true)` fits, `Some(false)` does not, None cannot be told: more tanks were clicked than the potion takes
/// and a board before or after it is missing, so a changed mind cannot be told from another card.
fn pick_fit(en: Option<&str>, cues: &[String], tanks: &[Option<u8>], before: Option<&str>, after: Option<&str>) -> Option<bool> {
    use vorax::data::Targeting;
    let Some(en) = en else { return Some(true) };
    let Some(targeting) = potion_targeting(en) else { return Some(true) };
    // No card click among the cues: the game log's clicks were not seen (a pick always clicks its card), so only
    // the boards can tell.
    let clicks_seen = cues.iter().any(|c| c == "CardClick");
    let clicked = tanks;
    let most = match targeting {
        Targeting::Auto | Targeting::Box(_) => 0,
        Targeting::Select1 => 1,
        Targeting::Select1To2 => 2,
    };
    if clicks_seen && most == 0 && !clicked.is_empty() {
        return Some(false);
    }
    // A potion kit is confirmed without a potion used (it opens the kit's hand); any other potion is used.
    if cues.iter().any(|c| c == "Confirm") && matches!(targeting, Targeting::Box(_)) == cues.iter().any(|c| c == "PotionUsed") {
        return Some(false);
    }
    if clicks_seen && most > 0 {
        if clicked.is_empty() {
            return Some(false);
        }
        let mut set: Vec<u8> = clicked.iter().flatten().copied().collect();
        set.sort_unstable();
        set.dedup();
        if set.len() > most {
            let (Some(b), Some(a)) = (sig_tanks(before), sig_tanks(after)) else { return None };
            let changed = set.iter().filter(|&&t| b.get(t as usize) != a.get(t as usize)).count();
            if changed > most {
                return Some(false);
            }
        }
    }
    Some(true)
}

/// Seeds each card is played under by [`board_fits`].
const BOARD_SEEDS: u32 = 64;

/// Which cards of a potion hand can give the board read after the pick: each card of the solver state `x` (the
/// hand's selection screen, as solved) is played under [`BOARD_SEEDS`] seeds on the clicked tanks (every set of one
/// or two tanks when a click's tank is unknown), the round ended as the game ends it, and the card fits when on some
/// seed its six tanks (archetype, rarity, activity, count) are exactly the board `after`. None when it cannot be
/// told: no six tanks after, a card or instrument the solver does not know.
///
/// A card that fits is not proof (cards with no effect on this board give the same board), and the true card often
/// does not (an effect drawn from more outcomes than the seeds, or a state the screen does not show: of 536 saved
/// potion picks with a card, 210 had no card that fits), so only one card that fits while every other card of the
/// hand never does names the card ([`settle_pick`]). Of the 23 saved picks where one other card than the logged one
/// fitted that way, those checked against the user's account, the tooltip or the clicks were all the logged card
/// wrong: 2026-11-08_18-44-54 round 2 (`Probiotic Enzyme Solution`, the advised card the user picked, logged as the
/// `Slender-Limb Powder - Bone Sentry` hovered on the way to Confirm), 2026-11-06_21-42-03 rounds 6 and 8 (the
/// tooltip's card), 2026-11-06_04-43-15 round 6 (a one-target potion logged with no tank clicked).
fn board_fits(x: &ScreenState, clicks: &[Option<u8>], after: Option<&str>) -> Option<Vec<bool>> {
    use vorax::game::dispatch_with;
    use vorax::types::{Action, TANK_COUNT};
    let after = sig_tanks(after)?;
    let mut unknown = Vec::new();
    let base = vorax::advisor::to_state(x, &mut unknown);
    if !unknown.is_empty() || base.hand.len() != x.hand.len() {
        return None;
    }
    let target_sets: Vec<Vec<u8>> = if clicks.is_empty() {
        vec![Vec::new()]
    } else if clicks.iter().all(Option::is_some) {
        let mut t: Vec<u8> = clicks.iter().flatten().copied().collect();
        t.sort_unstable();
        t.dedup();
        vec![t]
    } else {
        let mut all = vec![Vec::new()];
        for a in 0..TANK_COUNT as u8 {
            all.push(vec![a]);
            all.extend((a + 1..TANK_COUNT as u8).map(|b| vec![a, b]));
        }
        all
    };
    let board = |s: &vorax::types::GameState| -> Vec<String> {
        s.tanks
            .iter()
            .map(|t| t.map_or_else(|| "_".to_string(), |m| format!("Some({:?}),Some({:?}),Some({}),Some({})", m.archetype, m.rarity, m.activity, m.quantity)))
            .collect()
    };
    let fits = (0..base.hand.len())
        .map(|k| {
            (0..BOARD_SEEDS).any(|seed| {
                target_sets.iter().any(|targets| {
                    let mut s = base;
                    s.rng_state = seed.wrapping_mul(2_654_435_761).wrapping_add(12_345);
                    s = dispatch_with(&s, Action::SelectCard { index: k as i64 }, false);
                    for &t in targets {
                        s = dispatch_with(&s, Action::ToggleTarget { tank: i64::from(t) }, false);
                    }
                    s = dispatch_with(&s, Action::Confirm, false);
                    board(&s) == after
                })
            })
        })
        .collect();
    Some(fits)
}

/// The pick's `vorax_round` row, not written yet, of the instrument screen of round `shown` (the screen's round; a
/// pending pick's round is the solver's acquisition round, see [`PendingPick`]): its card is the hand's card read
/// as instrument `icon` (`how` "icon"), or null with `missing` saying so when none of the hand is.
fn name_from_icon(row: &mut Value, icon: u8, shown: i32) {
    if row["phase"] != "instrument" || row["round"] != json!(shown) {
        return;
    }
    let name = vorax::data().instrument(icon).map(|i| i.name.clone());
    let k = row.pointer("/chosen/hand").and_then(Value::as_array).and_then(|h| h.iter().position(|c| c.as_str().is_some() && c.as_str().map(str::to_string) == name));
    let (card, how, missing) = match k {
        Some(k) => (json!(k), json!("icon"), json!([])),
        None => (Value::Null, Value::Null, json!(["card"])),
    };
    row["chosen"]["card"] = card;
    row["chosen"]["how"] = how;
    row["missing"] = missing;
}

/// Settle the card of a potion pick in its `vorax_round` row once the board after it is known (`after`, None when
/// none was read): the candidates kept at the confirm (`_pick`: the card raised on the last read, the one raised
/// earlier, then the tooltip's) are tried in that order, and the first that fits
/// ([`pick_fit`]) is the card; when none fits, the one card of the hand the rules allow (`how` "fit"), else the card
/// is null and `missing` says so. Better no card than a wrong one. The card raised on the last read also stays when it cannot be told ([`pick_fit`] None: more tanks
/// clicked than it takes and no board after it), as it did before it was checked: a changed mind is likelier
/// then than another card that happens to fit.
fn settle_pick(row: &mut Value, after: Option<&str>) {
    // The hand's solver state, kept at the confirm for the board check ([`board_fits`]).
    let state = row.as_object_mut().and_then(|o| o.remove("_state")).and_then(|v| serde_json::from_value::<ScreenState>(v).ok());
    let Some(pick) = row.as_object_mut().and_then(|o| o.remove("_pick")) else { return };
    let cues: Vec<String> = row["cues"].as_array().map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let tanks: Vec<Option<u8>> = row.pointer("/chosen/tanks").and_then(Value::as_array).map(|a| a.iter().map(|t| t.as_u64().and_then(|t| u8::try_from(t).ok())).collect()).unwrap_or_default();
    let before = row["before"].as_str().map(str::to_string);
    let hand = row.pointer("/chosen/hand").cloned().unwrap_or(Value::Null);
    // What the card was settled from, kept in the row (`chosen.basis`, the user's ask of 2026-11-09): each candidate
    // seen at the confirm with whether the rules allow it (true, false, or null when it cannot be told), then the
    // cards of the hand the rules allow when none of them stood.
    let mut basis: Vec<Value> = Vec::new();
    let found = pick.as_array().into_iter().flatten().find_map(|c| {
        let (k, how) = (c.get(0)?.as_u64()? as usize, c.get(1)?.as_str()?);
        let en = hand.get(k).and_then(Value::as_str);
        let fit = pick_fit(en, &cues, &tanks, before.as_deref(), after);
        basis.push(json!([k, how, fit]));
        (fit == Some(true) || (fit.is_none() && how == "raised")).then(|| (k, how.to_string()))
    });
    // No card seen picked, or none that fits: the one card of the hand the rules allow (its targeting known and
    // fitting the clicks, the tanks and whether a potion was used; 2026-11-08_18-41-59 round 1, a kit opened with
    // no potion used and the only kit in the hand), as `fit`. Two that fit (or a card's targeting unknown) leave it
    // unknown.
    let found = found.or_else(|| {
        let names: Vec<Option<&str>> = hand.as_array().map(|h| h.iter().map(Value::as_str).collect()).unwrap_or_default();
        let fits: Vec<usize> = (0..names.len())
            .filter(|&k| names[k].is_some_and(|en| potion_targeting(en).is_some()) && pick_fit(names[k], &cues, &tanks, before.as_deref(), after) == Some(true))
            .collect();
        let known = names.iter().all(|n| n.is_some_and(|en| potion_targeting(en).is_some()));
        basis.push(json!({ "fits": fits, "allKnown": known }));
        // a card clicked after the last tank click may be another card taken instead (2026-11-06_16-22-21 round 9:
        // the tank click ruled the advised `Brain Fog Tincture` out, and `Digestive Enzyme Solution` was left)
        let last_tank = cues.iter().rposition(|c| c == "TankClick");
        let changed_card = last_tank.is_some_and(|t| cues[t..].iter().any(|c| c == "CardClick"));
        match fits[..] {
            [k] if known && !changed_card && cues.iter().any(|c| c == "Confirm") => Some((k, "fit".to_string())),
            _ => None,
        }
    });
    // The board after it ([`board_fits`]): one card of the hand that gives it while every other card never does names
    // the card when none was settled (`how` "board"; 2026-11-08_18-44-54 round 2, the raised card only hovered on the
    // way to Confirm; the rules of [`pick_fit`] must allow it too), and takes the settled card down to none when that
    // card is not it: better no card than a wrong one, and the board alone does not replace a card seen picked.
    let only = state.as_ref().filter(|x| x.hand.len() == hand.as_array().map_or(0, Vec::len)).and_then(|x| board_fits(x, &tanks, after)).and_then(|fits| {
        basis.push(json!({ "board": fits }));
        let mut yes = fits.iter().enumerate().filter(|(_, f)| **f).map(|(k, _)| k);
        match (yes.next(), yes.next()) {
            (Some(k), None) => Some(k),
            _ => None,
        }
    });
    let found = match (found, only) {
        (None, Some(k)) if pick_fit(hand.get(k).and_then(Value::as_str), &cues, &tanks, before.as_deref(), after) == Some(true) => Some((k, "board".to_string())),
        (Some((k, _)), Some(j)) if j != k => None,
        (found, _) => found,
    };
    let (card, how) = found.map_or((Value::Null, Value::Null), |(k, h)| (json!(k), json!(h)));
    row["chosen"]["card"] = card.clone();
    row["chosen"]["how"] = how;
    row["chosen"]["basis"] = json!(basis);
    if card.is_null() {
        row["missing"] = json!(["card"]);
    }
}

/// The fields written to the log for the battle panel: the panel's numbers, the bonus as a share of activity, and the board's final activity.
fn battle_fields(b: &read::Battle, board: Option<i64>) -> Value {
    let share = match (b.activity, b.bonus) {
        (Some(a), Some(x)) if a > 0.0 => Some(((x / a) * 1000.0).round() / 1000.0),
        _ => None,
    };
    json!({
        "wave": b.wave.map(|w| w.0),
        "waves": b.wave.map(|w| w.1),
        "activity": b.activity,
        "bonus": b.bonus,
        "next": b.next,
        "percent": b.percent,
        "bonusShare": share,
        // Activity + bonus: when the threshold (next) cannot be read, comparing it against the reward ladder shows which tier was reached.
        "total": b.activity.zip(b.bonus).map(|(a, x)| a + x),
        "board": board,
        "raw": b.raw,
    })
}

/// Status line: `終局戰鬥 第 2/3 波：568.5k＋221.2k，掉落 +15%，下一格 854.0k` (endgame battle, wave 2/3: 568.5k + 221.2k,
/// drops +15%, next tier 854.0k). The `(+15%)` the game shows is the reward tiers' drop bonus, not the bonus's share of
/// the activity (see `vorax_view::with_drops`); it was written as that share until 2026-11-08.
fn battle_line(b: &read::Battle) -> String {
    // Rounded down, lower case k: same format as vorax_view::battle_k, so 1,116,900 never reads as 1.12M.
    let k = |v: Option<f64>| v.map_or_else(|| "?".to_string(), |v| if v >= 1e6 { format!("{:.2}M", (v / 1e4).floor() / 100.0) } else { format!("{:.1}k", (v / 100.0).floor() / 10.0) });
    let wave = b.wave.map_or_else(String::new, |(w, n)| format!(" 第 {w}/{n} 波"));
    let share = match b.percent {
        Some(p) if p >= 0 => format!("，掉落 +{p}%"),
        _ => String::new(),
    };
    let next = match b.next {
        Some(n) if n < 1e8 => format!("，下一格 {}", k(Some(n))),
        Some(_) => "，獎勵全拿".to_string(),
        None => String::new(),
    };
    format!("終局戰鬥{wave}：{}＋{}{share}{next}", k(b.activity), k(b.bonus))
}

/// The held instruments that were read (not counting the ones revealed at the start), left to right.
fn held_ids(r: &Reading) -> Vec<Option<u8>> {
    r.held.iter().filter(|h| !h.opening).map(|h| h.id).collect()
}

fn hand(r: &Reading) -> Vec<Option<String>> {
    r.cards.iter().map(|c| c.en.clone()).collect()
}

/// The next state of the mode (plan section 2.1).
fn next_mode(m: Mode, cue: Cue) -> Mode {
    use PageState::{Destory, Hide, Run};
    match cue {
        Cue::Page(Page::Intro, Run) => Mode::Intro,
        Cue::Page(Page::Main, Run) => Mode::Board,
        // The result page only follows a game: one appearing alone while Off (a reward page opened elsewhere) does not enter the mode.
        Cue::Page(Page::Reward, Run) if m != Mode::Off => Mode::Result,
        Cue::Page(Page::Main, Hide) if m == Mode::Board => Mode::Result,
        Cue::Page(Page::Main, Destory) if m != Mode::Battle => Mode::Off,
        Cue::Page(Page::Intro, Destory) if m == Mode::Intro => Mode::Off,
        // Start surgery: enter the endgame battle (read the left panel); only return to Off when leaving the battle map.
        Cue::Fight if m == Mode::Result => Mode::Battle,
        // Entering the battle map also starts the battle (the start-surgery button can be missed).
        Cue::BattleEntered if m == Mode::Result => Mode::Battle,
        Cue::LevelLeft if m == Mode::Battle => Mode::Off,
        // The result page closing leaves the mode: the game ends when the give-up was pressed first (a confirmed
        // give-up), and is paused otherwise. The give-up button alone (its confirmation can be cancelled) and 5621
        // (also sent when the player dies in the battle) are not the end.
        Cue::Page(Page::Reward, Destory) if m == Mode::Result => Mode::Off,
        _ => m,
    }
}

/// Read the left panel of one battle frame of a game drawn at interface scale `ui`, and the panel crop for saving
/// (reference size; at another interface scale the panel's crop grows with it, see
/// [`crate::vorax::ui_scale::read_battle`]).
fn read_panel(frame: &Rgba, ui: Option<f64>) -> (read::Battle, Rgba) {
    crate::vorax::ui_scale::read_battle(&read::to_reference(frame), ui)
}

/// How fast the endgame battle panel is read (milliseconds): the panel's numbers change several times a second, and the log does not need to be that dense.
const BATTLE_EVERY_MS: i64 = 1000;
/// How fast it is read close to the cap with the give-up hint on ([`VoraxSession::battle_every`]): the hint waits
/// for [`GIVE_UP_READS`] reads in a row at the cap, a second apart they put it up 1-2 s after the cap (the
/// 2026-11-09 evening games). One read takes 25-35 ms (median over the 99 saved battle panels, under 60 ms at
/// p99) and about 0.1 s of CPU, so they do not pile up (a read still running holds the next, `BattleRun::scan`).
const BATTLE_NEAR_EVERY_MS: i64 = 300;
/// Activity plus bonus at this share of the last reward tier ([`read::BATTLE_LAST`]) count as close to the cap: the
/// 2026-11-09 evening games gained 20-80K a second at the end, so the faster reads start a few seconds before it.
const BATTLE_NEAR_SHARE: f64 = 0.85;
/// Even when the numbers changed, a `vorax_battle` row is written at most this often (written immediately when a later wave is read).
const BATTLE_ROW_MS: i64 = 5000;
/// If the agreed battle read time has passed by this much and still no frame has arrived (auto read is off, or the `read_in` frame did not come):
/// [`VoraxSession::poll`] asks for the next one instead. When the UI repaints per [`VoraxSession::wants_read_in`] it waits this much longer.
pub const READ_IN_LATE_MS: u64 = 500;

/// How many times a choice screen read with a problem is read again while it stays the same (see `retry`).
const RETRY_READS: u32 = 6;

/// How long after the surgery page opens an `other` frame asks for the next one at once, and how soon (see `handle`).
const BOARD_EARLY_MS: i64 = 1500;
const BOARD_EARLY_EVERY_MS: u64 = 100;

/// Tracking of one endgame battle (during [`Mode::Battle`]).
#[derive(Default)]
struct BattleRun {
    /// Set on entering battle, cleared once the ending is written: the ending is written only once ([`VoraxSession::end_battle`]).
    on: bool,
    /// The board game's `vorax_end` is held back to the end of the battle: neither the result screen nor a settled
    /// board gave the final activity, so the battle panel's last activity is used ([`BoardFrom::Battle`]).
    end_deferred: bool,
    /// Time of start surgery (Fight).
    fight_at: i64,
    /// The board game's `vorax_end` is not written yet: waiting for the result screen that was still being read before entering battle (see [`VoraxSession::flush_board_end`]).
    board_end: bool,
    /// Roughly when the panel is next read (frame time): frames in between are discarded, and the next one is not requested.
    due: Option<i64>,
    /// The last wave written to the log.
    logged_wave: u32,
    /// Background read: the panel read, the panel crop (for saving) and the frame time.
    scan: Option<Receiver<(read::Battle, Rgba, i64)>>,
    /// Frame time when the last read started.
    last_read: i64,
    /// The panel last read and its crop.
    last: Option<read::Battle>,
    last_crop: Option<Rgba>,
    /// The last one written to the log, and its time.
    logged: Option<(read::Battle, i64)>,
    /// Waves whose crop was already saved (the first one read in each wave is saved).
    saved_waves: BTreeSet<u32>,
    /// The result screen's final activity when entering battle (the board's final activity).
    board: Option<i64>,
    /// Number of times the battle panel was read.
    reads: u32,
    /// The last [`BATTLE_END_READS`] panels read, oldest first, a bonus far below the ones before it dropped
    /// ([`BONUS_DROP`]): the ending is settled from them ([`settled_battle`]).
    recent: Vec<read::Battle>,
    /// How many reads had their bonus dropped as a misread ([`BONUS_DROP`]), written in `vorax_battle_end`.
    dropped: u32,
    /// Readable reads in a row with every reward tier taken ([`read::Battle::all_taken`]); a read of another next
    /// tier starts over, one with the next tier unread changes nothing.
    capped: u32,
    /// The frame size and interface scale of the last frame read: where the Give Up button is.
    frame: Option<((usize, usize), Option<f64>)>,
    /// The give-up hint was logged (`vorax_give_up_hint`, once a battle).
    give_up_logged: bool,
    /// Frame time of the first of the reads in a row at the cap ([`Self::capped`]): the hint's row gives the time from
    /// it (`sinceCapMs`).
    cap_at: Option<i64>,
    /// The last readable read was close to the cap and the hint not up yet: the panel is read faster
    /// ([`VoraxSession::battle_every`]).
    near_cap: bool,
}

/// Reads in a row with every reward tier taken before the give-up hint shows: one read alone may be a misread.
const GIVE_UP_READS: u32 = 2;

/// How many of the last battle reads the ending (`vorax_battle_end`) is settled from, taking the median of each
/// number: one read alone was wrong at the 2026-11-06 12:23 game's end (482.1K read as 48.2K). Reads are a second
/// apart and the panel stands still once the battle is won, so three outvote one misread without lagging much
/// behind a panel still counting up.
const BATTLE_END_READS: usize = 3;
/// A bonus read this many times smaller than the median of the reads before it is a misread with a digit lost,
/// not a bonus (it only grows while fighting): it is not counted. A lost digit makes it about a tenth (48.2K
/// after 481.3K, a hair over a tenth), so the cut is at a fifth.
const BONUS_DROP: f64 = 5.0;

/// The median of `values`, given oldest first (None when there are none); of an even count's two middle values,
/// the one read later (the 2026-11-03 ranked replay's battle has two reads, 422.2K then the final 381.9K).
fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut v: Vec<(usize, f64)> = values.enumerate().collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1));
    let n = v.len();
    match n {
        0 => None,
        _ if !n.is_multiple_of(2) => Some(v[n / 2].1),
        _ => Some(if v[n / 2 - 1].0 > v[n / 2].0 { v[n / 2 - 1].1 } else { v[n / 2].1 }),
    }
}

/// Keep one more battle read in `recent` (at most [`BATTLE_END_READS`]), its bonus dropped when it is
/// [`BONUS_DROP`] times smaller than the median of the reads kept before it. Returns whether it was dropped.
fn keep_battle_read(recent: &mut Vec<read::Battle>, mut panel: read::Battle) -> bool {
    let mut dropped = false;
    if let (Some(b), Some(m)) = (panel.bonus, median(recent.iter().filter_map(|p| p.bonus)))
        && b * BONUS_DROP < m
    {
        panel.bonus = None;
        dropped = true;
    }
    recent.push(panel);
    if recent.len() > BATTLE_END_READS {
        recent.remove(0);
    }
    dropped
}

/// The last reads the battle's ending is settled from, oldest first, for `vorax_battle_end`'s `recent`: each one's
/// activity and bonus as kept (a dropped bonus is null, see [`keep_battle_read`]) and its raw text.
fn recent_fields(recent: &[read::Battle]) -> Value {
    json!(recent.iter().map(|p| json!({ "activity": p.activity, "bonus": p.bonus, "raw": p.raw })).collect::<Vec<_>>())
}

/// The panel the battle ended on: the last read, with its activity and bonus the medians of the recent reads
/// (see [`keep_battle_read`]; a number none of them has stays unread).
fn settled_battle(recent: &[read::Battle], last: &read::Battle) -> read::Battle {
    if recent.is_empty() {
        return last.clone();
    }
    read::Battle {
        activity: median(recent.iter().filter_map(|p| p.activity)),
        bonus: median(recent.iter().filter_map(|p| p.bonus)),
        ..last.clone()
    }
}

/// The most a result screen's final activity may be over the last settled one read on the board ([`VoraxSession::
/// check_result`]). The last board is round 13's as a rule, and only round 13's end-of-round gain comes after it. The
/// largest gains of one protocol round in the saved games up to 2026-11-06_14-06-38: x1.777 (2026-11-04_11-31-18,
/// round 11 219,954 to round 12 390,910) and x1.755 (2026-11-03_20-35-25, round 12 639,813 to round 13 1,122,629);
/// round 13 to the end x1.097 (14-06-38, 775,436 to 850,496). A ghost digit in front of the value (the fading
/// `1?3?0` over 850496 on 14-06-38 full-other-0071) makes v under 1M into v + 1M, over twice v and so over twice the
/// board (never above v), and v of 1M or more into ten times it: x2 drops every such read and keeps every real gain
/// seen. A board read two rounds back could gain more than x2 (x3.1 compounded at most in these games); that read is
/// dropped too, and the ending falls back to the board or the battle panel.
const RESULT_OVER_BOARD: f64 = 2.0;

/// Live advice for Vorax.
pub struct VoraxSession {
    config: Config,
    mode: Mode,
    log: Option<RunLog>,
    /// Diagnostics: the last [`VoraxSession::point_reader`] (when, which client, the OCR tag), carried on `vorax_read` rows.
    point: std::sync::Mutex<Value>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    view: View,
    /// Time of the last cue or frame (UTC milliseconds).
    clock: i64,
    read_next: bool,
    /// How long until a frame is wanted (one per second in the endgame battle): handed to `Watcher::read_in`.
    read_in: Option<Duration>,
    news: Option<(bool, bool)>,
    /// The signature of a choice screen read with a problem, and how many times it was read so (see `RETRY_READS`).
    retry: (String, u32),
    /// When the mode last became Board (the surgery page opened, see `BOARD_EARLY_MS`).
    board_at: i64,
    /// Background read, and the latest frame that arrived during the read.
    scan: Option<Receiver<Done>>,
    queued: Option<(Rgba, i64, String)>,
    /// Background solve: the board signature, whether it started while the value was still ticking (solving while it ticks), and the result.
    solve: Option<Solving>,
    /// A result finished while the value was still ticking: shown only at the steady read after it stopped ticking (same signature, same solver input).
    /// Dropped on confirm, reroll or page change.
    early: Option<Solved>,
    pool: Option<Arc<rayon::ThreadPool>>,
    /// Signature of the last read: if the next one is the same, it counts as steady.
    last_sig: Option<String>,
    last_reading: Option<Reading>,
    stable: Option<Stable>,
    advised: Option<Advised>,
    /// Number of confirms, rerolls, new hands dealt and page changes in the log ([`Cue::Synced`], [`Cue::Reroll`],
    /// [`Cue::Confirm`], [`Cue::Picked`], [`Cue::Dealt`] and each mode switch add one): used by `same_hand`
    /// to judge whether the board has changed since the advice.
    moves: u32,
    /// Of those, the number of new hands dealt ([`Cue::Dealt`]): see [`VoraxSession::hand_moves`].
    deals: u32,
    /// When the current stretch of value ticking (`ultimate_pending`) started, and whether the stuck-log row was written; None when not ticking, or
    /// after a confirm or reroll.
    pending_since: Option<(i64, bool)>,
    /// The signature of the board `pending_since` is timed on: another board starts the stretch again.
    pending_sig: Option<String>,
    gate: Option<Gate>,
    game: Game,
    /// The game whose board was closed midway and is waiting to be resumed.
    paused: Option<Paused>,
    /// The game that was resumed and whose round is not confirmed yet.
    resumed: Option<Resumed>,
    /// The give-up button was pressed on the result page: the page closing next confirms it and ends the game
    /// (cleared when the result page opens again or closes).
    give_up: bool,
    /// The intro page was hidden after it opened: a board opening next is a new game (the fallback new-game signal
    /// when the Vorax charges are not in the log).
    intro_hidden: bool,
    /// The current game was opened while a paused game owned the log folder: it gets its own folder only once a
    /// new-game signal ends the paused game ([`VoraxSession::new_game_signal`]).
    folder_pending: bool,
    /// Endgame battle ([`Mode::Battle`]).
    battle: BattleRun,
    /// Observe mode (plan section 6).
    learn: Learn,
    /// Enter the mode only on a Traditional Chinese client (unit tests turn this off, so the global language setting is not touched).
    require_zh: bool,
    /// Private builds only: on a client language Vorax does not read (see `start_samples`) with observe mode on, the
    /// game is entered to save whole frames and nothing is read ([`samples::Samples`]).
    #[cfg(feature = "dev-samples")]
    samples: Option<samples::Samples>,
    /// Unit tests: the client language seen by `require_zh` (None: the global setting).
    #[cfg(test)]
    test_zh: Option<bool>,
    /// Instrument reroll counts seen at the start of recent games, oldest first ([`OPENINGS_FILE`]).
    openings: Vec<i32>,
    /// The last finished game ([`VoraxSession::last_game`]).
    last_game: Option<GameSummary>,
    /// The game's UI scale (the HUD's `PageScale` in millionths, [`Cue::UiScale`]), and the value last written as a
    /// `ui_scale` row in Vorax mode (None after leaving the mode, so entering writes it again).
    ui_scale: Option<u32>,
    ui_scale_logged: Option<u32>,
}

impl VoraxSession {
    pub fn new(config: Config) -> VoraxSession {
        let log = config.log_root.clone().map(RunLog::new);
        let view = View { rollouts: config.rollouts, ..View::default() };
        let mut learn = Learn::new(config.log_root.clone(), config.learn, config.strips).with_min_free(config.min_free_bytes);
        learn.set_observe(config.observe);
        let openings = config.log_root.as_deref().map(load_openings).unwrap_or_default();
        let paused = config.log_root.as_deref().and_then(paused_on_disk);
        VoraxSession {
            config,
            mode: Mode::Off,
            log,
            point: std::sync::Mutex::new(Value::Null),
            notify: None,
            view,
            clock: 0,
            read_next: false,
            read_in: None,
            news: None,
            retry: (String::new(), 0),
            board_at: 0,
            scan: None,
            queued: None,
            solve: None,
            early: None,
            pool: None,
            last_sig: None,
            last_reading: None,
            stable: None,
            advised: None,
            moves: 0,
            deals: 0,
            pending_since: None,
            pending_sig: None,
            gate: None,
            game: Game::default(),
            paused,
            resumed: None,
            give_up: false,
            intro_hidden: false,
            folder_pending: false,
            battle: BattleRun::default(),
            learn,
            require_zh: true,
            #[cfg(feature = "dev-samples")]
            samples: None,
            #[cfg(test)]
            test_zh: None,
            openings,
            last_game: None,
            ui_scale: None,
            ui_scale_logged: None,
        }
    }

    /// Settings changed; this is also allowed mid-mode: reads and solves in progress are collected as usual (a solve carries the rollout
    /// count it started with); if the log root folder changed, switch to a new RunLog (the old one is closed only after it finishes writing
    /// the rows already sent); when `run_log` is turned off, nothing is written afterwards. Turning `enabled` off does not leave the mode by itself:
    /// a caller that wants to leave calls [`VoraxSession::force_off`].
    pub fn configure(&mut self, config: Config) {
        if config.log_root != self.config.log_root {
            self.log = config.log_root.clone().map(RunLog::new);
            self.openings = config.log_root.as_deref().map(load_openings).unwrap_or_default();
            if self.paused.is_none() {
                self.paused = config.log_root.as_deref().and_then(paused_on_disk);
            }
        }
        self.view.rollouts = config.rollouts;
        self.learn.configure(config.log_root.clone(), config.learn, config.strips, config.min_free_bytes);
        self.learn.set_observe(config.observe);
        self.config = config;
    }

    /// Called when a background read or solve finishes (the UI requests a repaint).
    pub fn on_progress(&mut self, notify: impl Fn() + Send + Sync + 'static) {
        self.notify = Some(Arc::new(notify));
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// In Vorax mode.
    pub fn active(&self) -> bool {
        self.mode.active()
    }

    /// A read or solve is in progress.
    pub fn busy(&self) -> bool {
        self.scan.is_some() || self.solve.is_some()
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    /// The log of this game (for tests).
    pub fn log(&self) -> Option<&RunLog> {
        self.log.as_ref()
    }

    /// The solver input the current advice is based on (including the tracked acquisition rounds and reroll counts); None when there is no advice.
    pub fn state(&self) -> Option<&ScreenState> {
        self.advised.as_ref().filter(|a| self.stable.as_ref().is_some_and(|s| s.sig == a.sig)).map(|a| &a.x)
    }

    /// The current advice (for the same board as [`VoraxSession::state`]).
    pub fn advice(&self) -> Option<&Advice> {
        self.advised.as_ref().filter(|a| self.stable.as_ref().is_some_and(|s| s.sig == a.sig)).map(|a| &a.advice)
    }

    /// The last steady read.
    pub fn reading(&self) -> Option<&Reading> {
        self.stable.as_ref().map(|s| &s.reading)
    }

    /// The gate is closed: the frame captured at `at_ms` is not OCRed.
    pub fn gate_closed(&self, at_ms: i64) -> bool {
        self.gate.as_ref().is_some_and(|g| !g.dealt && at_ms - g.since < GATE_MS)
    }

    /// One log cue; `at_ms` is that line's time (None means it was found by looking back at program start). A cue
    /// received outside the mode that does not switch the mode does nothing.
    pub fn note_cue(&mut self, cue: Cue, at_ms: Option<i64>) -> ModeChange {
        if let Some(at) = at_ms {
            self.clock = self.clock.max(at);
        }
        let at = at_ms.unwrap_or(self.clock);
        // The UI scale: recorded and logged only, never a move, a mode switch, a pause or a resume.
        if let Cue::UiScale { micro } = cue {
            self.ui_scale = Some(micro);
            self.log_ui_scale(at, false);
            return ModeChange::None;
        }
        // A new game paid for ends a paused one; it is no move on the board (not kept in the game's cues).
        if cue == Cue::NewGame {
            self.new_game_signal(at);
            return ModeChange::None;
        }
        // The intro page hidden into the board: a new game even when its charge was not seen (only an intro page
        // opened does not count: 2026-11-04 08:54 it was opened and closed again).
        let into_board = cue == Cue::Page(Page::Main, PageState::Run) && self.mode == Mode::Intro && self.intro_hidden;
        // The board straight from the intro page, neither hidden nor paid for: the paused game is taken back.
        if cue == Cue::Page(Page::Main, PageState::Run)
            && self.mode == Mode::Intro
            && !into_board
            && self.paused.as_ref().is_some_and(|p| takes_back(p, Some(cue)))
            && let Some(p) = self.paused.take()
        {
            self.resume_paused(p, at, Some(cue));
        }
        match cue {
            Cue::GiveUp => self.give_up = true,
            Cue::Page(Page::Reward, PageState::Run) => self.give_up = false,
            Cue::Page(Page::Intro, PageState::Run | PageState::Destory) => self.intro_hidden = false,
            Cue::Page(Page::Intro, PageState::Hide) => self.intro_hidden = self.mode == Mode::Intro,
            _ => {}
        }
        if into_board {
            self.intro_hidden = false;
            self.new_game_signal(at);
        }
        let to = self.paused_entry(cue).unwrap_or_else(|| next_mode(self.mode, cue));
        if to != self.mode {
            let change = self.switch_on(to, &format!("{cue:?}"), at_ms.is_none(), at, Some(cue));
            if cue == Cue::Page(Page::Reward, PageState::Destory) {
                self.give_up = false;
            }
            return change;
        }
        if cue == Cue::Page(Page::Reward, PageState::Destory) {
            self.give_up = false;
        }
        if self.active() {
            self.learn.note_cue(cue, at);
        }
        if self.mode == Mode::Board {
            self.board_cue(cue, at);
        }
        ModeChange::None
    }

    /// Force back to Off (the Watcher was dropped or rebuilt, or page lines are not arriving): write one `vorax_mode` row
    /// (the cue is `why`), clear the boxes and the gate, and drop the read and solve in progress. Harmless to call repeatedly; returns
    /// `Left` when it was in the mode.
    pub fn force_off(&mut self, why: &str) -> ModeChange {
        if self.active() {
            return self.switch(Mode::Off, why, false, self.clock);
        }
        self.stop();
        self.gate = None;
        (self.view.boxes, self.view.gated) = (Vec::new(), false);
        ModeChange::None
    }

    /// Write one `game` row (same format as `game_row` in ui.rs): `signal` is `{signal:?}`, `stamp_ms`
    /// is the time of the log line, `read_at` is the time it was read from the log; `queued_ms` is how long it waited after being read
    /// (uses the system clock, for live use only).
    pub fn log_game(&mut self, signal: &str, stamp_ms: Option<i64>, read_at: SystemTime) {
        let ms = |t: SystemTime| t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let fields = json!({
            "signal": signal,
            "late_ms": stamp_ms.map(|at| ms(read_at) - at),
            "queued_ms": SystemTime::now().duration_since(read_at).map_or(0, |d| d.as_millis() as i64),
        });
        if let Some(log) = &self.log {
            log.event("game", read_at, fields);
        }
    }

    /// Deadlock guard: while in Vorax mode, receiving a log signal that only Tartarus has (everything other than `Signal::Vorax` is one;
    /// in two Vorax games on 2026-11-03 the log had not a single S16 line between the intro page and the result) means the player has
    /// left Vorax, for example the game crashed without writing the page's Destory, or the look-back at program start judged wrongly:
    /// treat it as leaving and return `Left`. Signals found by looking back (`at_ms` is None) do not count.
    #[cfg(windows)]
    pub fn note_other_signal(&mut self, signal: &super::gamelog::Signal, at_ms: Option<i64>) -> ModeChange {
        // Accumulated progress (Signal::Charges) is written by both Tartarus and Vorax, and Vorax deducts 12 at the very moment a game starts: it is not leaving Vorax.
        // Nor is the UI scale, logged on every level entered, the endgame battle's map included (capture.rs sends it
        // on as Cue::UiScale; a caller handing the raw signal here must not end the battle).
        if matches!(signal, super::gamelog::Signal::Vorax(_) | super::gamelog::Signal::Charges { .. } | super::gamelog::Signal::UiScale { .. }) {
            return ModeChange::None;
        }
        self.note_tartarus(&format!("{signal:?}"), at_ms)
    }

    /// The platform-independent version of [`VoraxSession::note_other_signal`]: `what` is the signal's name.
    pub fn note_tartarus(&mut self, what: &str, at_ms: Option<i64>) -> ModeChange {
        let Some(at) = at_ms.filter(|_| self.active()) else { return ModeChange::None };
        self.clock = self.clock.max(at);
        let change = self.switch(Mode::Off, &format!("tartarus {what}"), false, at);
        // Back in Tartarus: the board page shows it, not the Vorax game just ended.
        self.last_game = None;
        change
    }

    /// The game client language the reader reads: the language of the OCR engine made for the window language (the
    /// user picks it to be the game client's; with no OCR language of it installed Windows falls back to a profile
    /// language, and that one is what is read, see `crate::reader::ocr::client_language`); the window language when
    /// there is no OCR at all; None for a language the reader does not know. Unit tests set it with `test_zh`
    /// (Traditional Chinese or English) instead of the global language setting.
    pub fn client_language(&self) -> Option<crate::lang::Lang> {
        #[cfg(test)]
        if let Some(zh) = self.test_zh {
            return Some(if zh { crate::lang::Lang::ZhTw } else { crate::lang::Lang::En });
        }
        Self::client_of(crate::reader::ocr::language().as_deref())
    }

    /// The client an OCR language tag reads (see [`VoraxSession::client_language`]): the window language when there is
    /// no OCR at all.
    fn client_of(tag: Option<&str>) -> Option<crate::lang::Lang> {
        match tag {
            Some(tag) => crate::reader::ocr::game_lang(tag),
            None => Some(crate::lang::lang()),
        }
    }

    /// The client is Traditional Chinese ([`VoraxSession::client_language`]).
    fn client_zh(&self) -> bool {
        self.client_language() == Some(crate::lang::Lang::ZhTw)
    }

    /// Points the reader at the client ([`VoraxSession::client_language`]): the English client with the feature
    /// `en-vorax` (on by default since 1.1.0) and an English client, the Traditional Chinese one otherwise
    /// (`vorax::read::english`).
    /// Called before every frame (see `read`), on entering the mode and when the window language is switched. Returns whether it
    /// reads the English client.
    fn point_reader(&self) -> bool {
        // The OCR language is asked once: it is called before every frame.
        let ocr = crate::reader::ocr::language();
        #[cfg(test)]
        let client = if self.test_zh.is_some() { self.client_language() } else { Self::client_of(ocr.as_deref()) };
        #[cfg(not(test))]
        let client = Self::client_of(ocr.as_deref());
        let english = cfg!(feature = "en-vorax") && client == Some(crate::lang::Lang::En);
        // Diagnostics: why the reader reads the client it does (the OCR language tag the client was told from), carried
        // on every later `vorax_read` row as `point` (not a row of its own: no folder is opened for it).
        if let Ok(mut point) = self.point.lock() {
            *point = json!({ "atMs": now_ms(), "english": english, "client": client.map(|l| l.key()), "ocr": ocr, "lang": crate::lang::lang().key() });
        }
        // Unit tests set it for their own thread: reading tests of the other client run alongside.
        #[cfg(all(feature = "en-vorax", not(test)))]
        crate::vorax::read::set_english(english);
        #[cfg(all(feature = "en-vorax", test))]
        crate::vorax::read::set_english_here(Some(english));
        english
    }

    /// A client language Vorax does not read (neither Traditional Chinese nor, with `en-vorax`, English): private
    /// builds with observe mode on enter the game anyway, to save whole frames as samples ([`samples::Samples`]); the
    /// status still shows the NOT_ZH note (only the Traditional Chinese and English clients are read).
    /// Other builds never do.
    #[cfg(feature = "dev-samples")]
    fn start_samples(&mut self) -> bool {
        if !(self.config.learn || self.config.observe) || self.log.is_none() {
            self.samples = None;
            return false;
        }
        // A sampler kept from a pause goes on with its counts (see `new_game`).
        self.samples.get_or_insert_with(samples::Samples::default);
        true
    }

    #[cfg(not(feature = "dev-samples"))]
    fn start_samples(&mut self) -> bool {
        false
    }

    fn switch(&mut self, to: Mode, why: &str, lookback: bool, at: i64) -> ModeChange {
        self.switch_on(to, why, lookback, at, None)
    }

    /// `cue` is the log cue that caused the switch (None for a forced leave): Main Destory on the board pauses this game first,
    /// and a Main Run without an intro page resumes the paused game.
    fn switch_on(&mut self, to: Mode, why: &str, lookback: bool, at: i64, cue: Option<Cue>) -> ModeChange {
        let from = self.mode;
        // The page changed (for example from the board straight to the intro page without a Destory): earlier advice is no longer the same hand.
        self.moves += 1;
        if from == Mode::Battle && to != Mode::Battle {
            self.end_battle(at);
        }
        // Starting a fresh game right after the endgame battle (intro page or board) is also a new game. From Off,
        // entering the battle directly happens only when a paused game is taken back (see paused_entry).
        let entering = (!from.active() || from == Mode::Battle) && to.active() && to != from;
        if entering {
            if !self.config.enabled {
                return ModeChange::None;
            }
            // The reader reads the client it is given (vorax::read::english): the Traditional Chinese one, or with
            // the feature `en-vorax` (on by default since 1.1.0) the English one too; any other client is not entered (NOT_ZH).
            let english = self.point_reader();
            if self.require_zh && !self.client_zh() && !english && !self.start_samples() {
                self.view.blocked = Some(NOT_ZH.to_string());
                self.bump();
                return ModeChange::None;
            }
            // A Traditional Chinese client (or the check off) reads as usual: no samples.
            #[cfg(feature = "dev-samples")]
            if !self.require_zh || self.client_zh() || english {
                self.samples = None;
            }
            match self.paused.take() {
                Some(p) if takes_back(&p, cue) || self.samples_take_back(&p, cue) => self.resume_paused(p, at, cue),
                // Another page (the intro page) while a game is paused: not a new game yet, which only its charge or
                // the intro page hidden into the board tells; until then the paused game keeps the log folder.
                Some(mut p) => {
                    // A game found on disk at start: its folder becomes the current one now, so the intro page's rows go
                    // there as they would in this run, and no new folder is opened for a page that may start nothing.
                    if let Some(dir) = p.dir.take()
                        && let Some(log) = &mut self.log
                    {
                        log.continue_in(dir);
                        // The pause again ahead of the intro page's rows, stamped at its time, so the folder still ends
                        // with this game paused for the next start (an older folder has no pause row at all, and its
                        // last mode row would now be the intro page's).
                        self.log_event("vorax_pause", p.at, pause_fields(&p.game, p.last_round, "taken over at start"));
                    }
                    self.paused = Some(p);
                    self.new_game(at, false);
                    self.folder_pending = true;
                }
                None => self.new_game(at, true),
            }
            // Collecting samples: said under the note that the advice reads the Traditional Chinese client only,
            // set here since a new game and a resume both reset the view.
            #[cfg(feature = "dev-samples")]
            if self.samples.is_some() {
                self.view.blocked = Some(format!("{NOT_ZH}
{}", samples::NOTE));
                self.bump();
            }
        }
        if to == Mode::Board && cue == Some(Cue::Page(Page::Main, PageState::Run)) {
            self.game.board_run = true;
        }
        if to == Mode::Board && from != Mode::Board {
            self.board_at = at;
        }
        self.mode = to;
        // Leaving keeps the sampler: a pause, or the surgery page closed and opened to skip an animation, is the same
        // game; its counts start over only with a new game ([`VoraxSession::new_game`]).
        #[cfg(feature = "dev-samples")]
        if let Some(s) = &mut self.samples {
            s.mode(to, at);
        }
        self.log_event("vorax_mode", at, json!({ "from": from, "to": to, "cue": why, "lookback": lookback }));
        // Entering the mode (a new game's folder, or a paused game's taken back): the UI scale in use goes in again.
        if entering {
            self.log_ui_scale(at, true);
        }
        if to != Mode::Board {
            self.gate = None;
            // The board's solve (in progress, told to give up) and results not yet shown are all discarded.
            (self.solve, self.early) = (None, None);
        }
        if to.active() {
            self.view.mode = to;
            self.view.gated = false;
            match to {
                Mode::Intro => self.set_status(Status::Intro, vec![t("開局頁", "Intro page").into()]),
                Mode::Result => {
                    self.view.boxes.clear();
                    self.set_status(Status::Result, vec![t("結算畫面", "Result screen").into()]);
                }
                Mode::Battle => {
                    self.view.boxes.clear();
                    // Frames after start surgery are all battle screens: queued ones are not read. The last selection's vorax_round is written first,
                    // ahead of the ending (before, it would be delayed until leaving the battle, and dropped when a new game started directly).
                    self.queued = None;
                    self.finish_round(None, at);
                    // The board game is over: its ending is written at start surgery, and the battle writes vorax_battle_end separately. The time from the result screen to
                    // start surgery can be under 1 second, so a read still in progress is finished first and then the ending is written (see flush_board_end).
                    self.start_battle(at);
                    if self.scan.is_none() {
                        self.flush_board_end();
                    }
                    self.set_status(Status::Battle, vec![t("終局戰鬥：等左側面板", "Endgame battle: waiting for the panel").into()]);
                }
                _ => self.set_status(Status::Idle, vec![t("等盤面", "Waiting for the board").into()]),
            }
        } else {
            // A confirmed give-up: the give-up button, then the result page closing.
            let given_up = from == Mode::Result && cue == Some(Cue::Page(Page::Reward, PageState::Destory)) && self.give_up;
            if self.in_play_from(from, cue) && !given_up {
                // The board or result page was closed before the game ended (to skip an animation, to play Tartarus,
                // the client closed or lost): the game waits for its resume, its ending is not written.
                self.pause(at, why, from == Mode::Result);
            } else {
                self.finish_round(None, at);
                if self.game.has_play() {
                    // Left without reading the result screen's final activity: still write one row (ultimate is null).
                    self.log_end(None, at);
                }
            }
            self.stop();
            self.view = View { rollouts: self.config.rollouts, version: self.view.version, ..View::default() };
        }
        self.bump();
        match (from.active(), to.active()) {
            (false, true) => ModeChange::Entered,
            (true, false) => ModeChange::Left,
            _ => ModeChange::None,
        }
    }

    /// A new game: reset, and a new log folder unless `folder` is false (a paused game still owns the current one,
    /// see [`VoraxSession::folder_pending`]).
    fn new_game(&mut self, at: i64, folder: bool) {
        self.reset();
        self.last_game = None;
        self.resumed = None;
        self.folder_pending = false;
        self.learn.new_game();
        #[cfg(feature = "dev-samples")]
        if let Some(s) = &mut self.samples {
            *s = samples::Samples::default();
        }
        if folder && let Some(log) = &mut self.log {
            log.new_run();
        }
        // If the supplementary names file changed the name table: log one row per game, so when investigating a misread it is clear whether it caused it.
        let changes = crate::vorax::names::extra_changes();
        if !changes.is_empty() {
            self.log_event("vorax_names_extra", at, json!({ "file": crate::vorax::names::EXTRA_FILE, "changes": changes }));
        }
    }

    /// A new game was started (its charge spent, or the intro page hidden into the board): a paused game can no
    /// longer come back, so its ending is written (stamped at its pause) in its folder, and its result card is kept;
    /// the new game then gets a folder of its own when it was sharing that one.
    fn new_game_signal(&mut self, at: i64) {
        if let Some(p) = self.paused.take() {
            self.end_paused(p);
        }
        if std::mem::take(&mut self.folder_pending) {
            if let Some(log) = &mut self.log {
                log.new_run();
            }
            self.log_event("vorax_mode", at, json!({ "from": "off", "to": self.mode, "cue": "new game", "lookback": false }));
            self.log_ui_scale(at, true);
        }
    }

    /// Whether a game is under way on its board or result page in mode `m`, left on `cue`: a mode change away from it
    /// pauses it. It does not rest on the reader alone (2026-11-06 06:15: the UI scaled, every read was another screen
    /// with no round, and each skipped animation opened a new folder): a game played on its board ([`Game::has_play`])
    /// is under way too, and so is a board opened by its Main Run and closed by its Main Destory (the skip-animation
    /// pattern) even before a move is seen. A bare board left another way (Ascendant's watcher dropped, a Tartarus
    /// signal) is not paused on that alone.
    fn in_play_from(&self, m: Mode, cue: Option<Cue>) -> bool {
        let g = &self.game;
        let skipped = m == Mode::Board && g.board_run && cue == Some(Cue::Page(Page::Main, PageState::Destory));
        matches!(m, Mode::Board | Mode::Result) && !g.ended && (g.has_play() || skipped)
    }

    /// Pause the current game ([`Paused`]): one `vorax_pause` row keeps what is needed to take it back after
    /// Ascendant restarts ([`paused_on_disk`]). `result` is true when it was paused on its result page (its board is
    /// over, so its result page, start surgery or the battle map take it back).
    fn pause(&mut self, at: i64, why: &str, result: bool) {
        if let Some(older) = self.paused.take() {
            self.end_paused(older);
        }
        let seen = self.game.rounds_seen.last().copied().unwrap_or(0);
        let last_round = if result { seen.max(LAST_ROUND) } else { seen };
        self.log_event("vorax_pause", at, pause_fields(&self.game, last_round, why));
        self.paused = Some(Paused { game: std::mem::take(&mut self.game), at, last_round, dir: None });
        self.resumed = None;
    }

    /// The mode a cue switches to by taking the paused game back from Off, when it is not the one [`next_mode`]
    /// gives: a paused game that reached the last round goes on to its result screen (Reward Run, Off otherwise
    /// ignores it) or straight into its endgame battle (Fight or the battle map). The board's own Main Run is left
    /// to `next_mode`. Whether the cue takes the paused game back at all is decided in one place, [`takes_back`]
    /// (and, collecting samples, [`VoraxSession::samples_take_back`]).
    fn paused_entry(&self, cue: Cue) -> Option<Mode> {
        let p = self.paused.as_ref().filter(|_| self.mode == Mode::Off)?;
        if !takes_back(p, Some(cue)) && !self.samples_take_back(p, Some(cue)) {
            return None;
        }
        match cue {
            Cue::Page(Page::Reward, PageState::Run) => Some(Mode::Result),
            Cue::Fight | Cue::BattleEntered => Some(Mode::Battle),
            _ => None,
        }
    }

    /// Private builds collecting samples ([`samples::Samples`]): the result page (Reward Run) also takes the paused
    /// game back once it has a finished game's picks ([`by_picks`]). No round is read while collecting samples, so
    /// [`takes_back`] never sees the last round, and a game whose board was closed after its last confirm (the
    /// skip-animation habit) had its result page left out: 2026-11-07_14-43-32, Main Destory 0.67 s before Reward
    /// Run. No frame reaches the session while it is off, so the page is saved only with the game taken back into
    /// Result. The risk [`by_picks`] names (an older game's result page at entry, given up, ending this one) can at
    /// worst end a sample game early; no advice is given while collecting samples.
    #[cfg(feature = "dev-samples")]
    fn samples_take_back(&self, p: &Paused, cue: Option<Cue>) -> bool {
        self.samples.is_some() && cue == Some(Cue::Page(Page::Reward, PageState::Run)) && by_picks(p)
    }

    /// Without the feature (the public build) nothing more than [`takes_back`].
    #[cfg(not(feature = "dev-samples"))]
    fn samples_take_back(&self, _: &Paused, _: Option<Cue>) -> bool {
        false
    }

    /// Take the paused game back (the same game goes on): keep the instruments, the round and the pending
    /// `vorax_round` row, drop only the read and advice; the log goes in the same folder. A resolve strip still
    /// waiting from before the pause is dropped: the next frame is the reopened page, not the resolve animation.
    /// A game found on disk at start writes `folder` (its folder) in the `vorax_resume` row.
    fn resume_paused(&mut self, p: Paused, at: i64, cue: Option<Cue>) {
        self.reset();
        self.game = p.game;
        // An intro page opened meanwhile and left unplayed: the folder stays this game's.
        self.folder_pending = false;
        // A game found on disk at start goes on in its own folder.
        if let Some(dir) = &p.dir
            && let Some(log) = &mut self.log
        {
            log.continue_in(dir.clone());
        }
        self.learn.drop_strip();
        // Skipping an animation shows the world map for a moment before the page is drawn again: no whole frame
        // of it (see Learn::full_frame).
        self.learn.resumed(at);
        self.resumed = Some(Resumed { at: p.at, last_round: p.last_round, monsters: self.game.monsters.clone() });
        let via = cue.map(|c| format!("{c:?}"));
        let folder = p.dir.as_ref().map(|d| d.to_string_lossy().into_owned());
        // Taken back by its battle on its pick count, its round unknown: the row says so (an estimate, see by_picks).
        let estimated = (matches!(cue, Some(Cue::Fight | Cue::BattleEntered)) && p.last_round < LAST_ROUND).then_some("picks");
        let fields = json!({ "pausedMs": at - p.at, "lastRound": p.last_round, "cue": via, "folder": folder, "picks": self.game.picks, "lastRoundFrom": estimated });
        self.log_event("vorax_resume", at, fields);
    }

    /// Back-fill `vorax_end` for the paused game (the time is the moment of its pause), written in its own folder;
    /// its pending `vorax_round` row is written first, with a null `after`. Returns whether the game's summary was
    /// kept for the board page ([`VoraxSession::last_game`]). A game never played (its board opened and closed with
    /// no round read and no move, paused only by the skip-animation rule of `in_play_from`) is dropped instead: an
    /// ending with no rounds and no picks would say nothing.
    fn end_paused(&mut self, p: Paused) -> bool {
        if !p.game.has_play() {
            return false;
        }
        // A game found on disk at start ends in its own folder, through a log of its own (dropped at the end, which
        // waits for its writes), so the current folder and its numbering are left as they are.
        let own = match (&p.dir, &self.log) {
            (Some(dir), Some(_)) => dir.parent().map(|root| {
                let mut log = RunLog::new(root.to_path_buf());
                log.continue_in(dir.clone());
                log
            }),
            _ => None,
        };
        let current_log = own.map(|l| self.log.replace(l));
        let current = std::mem::replace(&mut self.game, p.game);
        self.finish_round(None, p.at);
        let kept = self.log_end_with(None, None, false, p.at);
        self.game = current;
        if let Some(log) = current_log {
            drop(std::mem::replace(&mut self.log, log));
        }
        kept
    }

    /// Called by the UI on every event; `tartarus` is a Tartarus signal: the board page goes back to Tartarus, so the
    /// last finished Vorax game's card is dropped. A paused game is never ended here, by time or by Tartarus: the
    /// game server keeps it, and it ends only on its own signals ([`Paused`]). `_now_ms` is no longer used.
    pub fn drop_paused(&mut self, _now_ms: i64, tartarus: bool) {
        if tartarus {
            self.last_game = None;
        }
    }

    /// A just-resumed game read its round on a selection screen: a smaller round than before the pause together with
    /// other monsters in the tanks means a new game (back-fill the old game's ending, open a new folder).
    fn check_resumed(&mut self, r: &Reading, at: i64) {
        let Some(round) = r.round.filter(|_| r.kind.is_choice()) else { return };
        let Some(res) = self.resumed.take() else { return };
        let now = monsters(&r.tanks);
        if round >= res.last_round || !monsters_differ(&res.monsters, &now) {
            return;
        }
        let old = std::mem::take(&mut self.game);
        self.end_paused(Paused { game: old, at: res.at, last_round: res.last_round, dir: None });
        self.learn.new_game();
        if let Some(log) = &mut self.log {
            log.new_run();
        }
        self.log_event("vorax_mode", at, json!({ "from": "board", "to": "board", "cue": format!("new game: round {round} after round {}, other monsters", res.last_round), "lookback": false }));
        self.log_ui_scale(at, true);
    }

    /// A game starts: discard everything from the previous game.
    fn reset(&mut self) {
        self.stop();
        self.game = Game::default();
        (self.last_sig, self.last_reading, self.stable, self.advised, self.gate) = (None, None, None, None, None);
        self.pending_since = None;
        self.view = View { rollouts: self.config.rollouts, version: self.view.version, ..View::default() };
    }

    /// Stop the read and solve (their results are not collected).
    fn stop(&mut self) {
        (self.scan, self.queued, self.solve, self.early) = (None, None, None, None);
        self.read_next = false;
    }

    fn board_cue(&mut self, cue: Cue, at: i64) {
        #[cfg(feature = "dev-samples")]
        if let Some(s) = &mut self.samples {
            s.cue(cue, at);
        }
        // The second Dealt line in the same millisecond is the same deal and is not counted separately.
        let repeat = cue == Cue::Dealt && self.game.last_dealt == Some(at);
        if matches!(cue, Cue::Synced | Cue::Reroll { .. } | Cue::Confirm | Cue::Picked | Cue::Dealt) && !repeat {
            self.moves += 1;
            if cue == Cue::Dealt {
                self.deals += 1;
            }
        }
        // A move of the player: the game is under way even when no round was read (see in_play_from).
        if matches!(cue, Cue::CardClick | Cue::TankClick { .. } | Cue::Confirm | Cue::PotionUsed | Cue::Picked | Cue::Protocol | Cue::Reroll { .. }) {
            self.game.played = true;
        }
        if cue == Cue::Tool && self.game.final_picked {
            self.game.final_tool = Some(at);
        }
        // The card under the mouse at a card click is looked up at the confirm ([`Game::click_at`]).
        if cue == Cue::CardClick {
            self.game.click_at = Some(at);
        }
        // A new deal, a reroll or a confirm: the next hand's names are never carried from this one's.
        if matches!(cue, Cue::Shuffle | Cue::Dealt | Cue::Synced | Cue::Reroll { .. } | Cue::Confirm | Cue::Picked | Cue::PotionUsed) {
            (self.game.named_hand, self.game.prev_named) = (None, None);
            self.game.hand_from = self.game.hand_from.max(at);
        }
        match cue {
            Cue::Synced => self.synced(at),
            Cue::Dealt if self.game.last_dealt == Some(at) => {}
            Cue::Dealt | Cue::Shuffle => {
                if cue == Cue::Dealt {
                    self.game.last_dealt = Some(at);
                }
                if let Some(g) = &mut self.gate {
                    g.dealt = true;
                }
                self.game.cues.push(cue);
            }
            _ => self.game.cues.push(cue),
        }
    }

    /// Confirm or reroll: the board is about to change. Hide the boxes, drop the read and solve, wait for the new hand; keep the previous selection's
    /// `vorax_round` row (written once the board after the selection is read).
    fn synced(&mut self, at: i64) {
        // the hand's last steady read, when it read otherwise than the advised one (see LOSSLESS)
        let (shot, advised) = (self.game.steady_shot.take(), self.game.evidence_sig.take());
        if let Some(shot) = shot.filter(|shot| advised.as_deref() != Some(shot.sig.as_str())) {
            self.save_evidence(&shot, "confirmed");
        }
        let cues = std::mem::take(&mut self.game.cues);
        self.finish_round(None, at);
        self.game.kit = false;
        let rerolled = cues.iter().any(|c| matches!(c, Cue::Reroll { .. }));
        // The tank under the cursor at each tank click, in click order (null when it could not be told): the
        // settlement takes the potion's targets from it before inferring them from the boards before and after.
        let tanks: Vec<Option<u8>> = cues.iter().filter_map(|c| if let Cue::TankClick { tank } = c { Some(*tank) } else { None }).collect();
        let cue_names: Vec<String> = cues.iter().map(|c| c.name()).collect();
        // The pick's board is this hand's last steady selection screen, never the resolving or other screen read
        // after it (see `Game::choice`).
        let choice = self.game.choice.take();
        let (raised_seen, hovered_seen) = (self.game.raised_seen.take(), self.game.hovered.take());
        let raised_other = self.game.raised_other.take();
        // The card under the mouse at the last card click: the last one the tooltip named on a frame taken at or
        // before it (frame and cue times share the UTC clock).
        let click_at = self.game.click_at.take();
        let clicked_seen = click_at.and_then(|c| self.game.hover_log.iter().rev().find(|(t, _)| *t <= c).map(|(_, s)| s.clone()));
        self.game.hover_log.clear();
        if let Some((sig, r)) = &choice {
            if r.round == Some(LAST_ROUND) && !rerolled {
                self.game.final_picked = true;
            }
            // The card raised on the last read, when that read is a selection screen of this hand (a resolving read
            // has no cards, and its positions would say nothing about this hand).
            let raised = self
                .last_reading
                .as_ref()
                .filter(|l| l.kind.is_choice() && l.cards.len() == r.cards.len() && (l.round.is_none() || r.round.is_none() || l.round == r.round))
                .and_then(|l| l.cards.iter().position(|c| c.raised));
            // The card has often dropped before confirm, so the last read does not see it raised (for protocols, 63 of 138 times the chosen card was not recorded):
            // use a card that was raised earlier in this hand, otherwise the card the tooltip pointed at most recently. Offline comparison against old logs:
            // the share of protocols whose pick could be recorded went from 54% to 86%, and tooltip vs known picks was wrong 1 time in 52.
            let earlier = seen_in(&raised_seen, r);
            let hovered = seen_in(&hovered_seen, r);
            // Last: the card the tooltip named at the last card click (see `Game::click_at`).
            let clicked = seen_in(&clicked_seen, r);
            // A reroll picks no card: the card raised or pointed at before it is not the move (2026-11-06_13-46-25
            // round 3, the reroll row had the card hovered before it).
            // Two cards raised in this hand and the last read's raised card under the tooltip: the mouse rests on it,
            // on its way to Confirm after picking another (a picked card drops as the mouse moves on, and a hovered one
            // rises the same height), so no card rather than a wrong one. A card picked last reads raised with the
            // tooltip gone or on another card (2026-11-08_18-44-54 round 4, 18-51-19 round 10, 2026-11-06_13-46-25
            // round 7: the advised card, picked and kept). An instrument's icon still names it ([`name_from_icon`]).
            let last_tip = self.last_reading.as_ref().filter(|l| l.kind.is_choice() && l.cards.len() == r.cards.len()).and_then(hovered_card);
            let mixed = hover_pass(raised, last_tip, earlier, seen_in(&raised_other, r));
            let raised = raised.filter(|_| !mixed);
            let (card, how) = match (raised, earlier, hovered, clicked) {
                _ if rerolled || mixed => (None, None),
                (Some(k), ..) => (Some(k), Some("raised")),
                (None, Some(k), ..) => (Some(k), Some("raisedEarlier")),
                (None, None, Some(k), _) => (Some(k), Some("tooltip")),
                (None, None, None, Some(k)) => (Some(k), Some("clicked")),
                (None, None, None, None) => (None, None),
            };
            // What the row lacks: a pick (not a reroll) whose card could not be told.
            let missing: Vec<&str> = if card.is_none() && !rerolled { vec!["card"] } else { vec![] };
            let mut row = json!({
                "round": r.round,
                "phase": if rerolled { json!("reroll") } else { json!(r.kind) },
                "before": sig,
                "chosen": { "card": card, "how": how, "hand": hand(r), "tankClicks": tanks.len(), "tanks": tanks },
                "cues": cue_names,
                "missing": missing,
            });
            // A potion's card is checked against the tank clicks and the board after it before the row is written
            // ([`settle_pick`]): the card raised on the last read, then the one raised earlier, then the tooltip's.
            // The last read's raised card is checked too: of the 73 saved potion rows logged as raised, one broke
            // the rules (2026-11-06_04-43-15 round 6, log line 271: `Leptomeningeal Solution`, one target, raised
            // in a fanned hand of five, yet no tank was clicked).
            if r.kind == Kind::Potion && !rerolled {
                // no candidate when the raised card was only hovered: [`settle_pick`] may still find the one card that fits
                let candidates: Vec<Value> = if mixed {
                    Vec::new()
                } else {
                    [(raised, "raised"), (earlier, "raisedEarlier"), (hovered, "tooltip"), (clicked, "clicked")].into_iter().filter_map(|(k, how)| k.map(|k| json!([k, how]))).collect()
                };
                row["_pick"] = json!(candidates);
                // the hand's solver state for the board check at settlement ([`board_fits`])
                if let Ok((x, _)) = self.screen_state(r)
                    && let Ok(v) = serde_json::to_value(&x)
                {
                    row["_state"] = v;
                }
            }
            self.game.round_row = Some(row);
            // Potion kit: the potion screen was confirmed without using a potion, confirming a selection or rerolling, so the next potion screen is a choice inside the kit.
            self.game.kit = r.kind == Kind::Potion && !cues.iter().any(|c| matches!(c, Cue::PotionUsed | Cue::Picked | Cue::Reroll { .. }));
            if r.kind == Kind::Instrument
                && cues.contains(&Cue::Picked)
                && let Some(shown) = r.round
            {
                // The solver's acquisition round: at 8k/28k the screen shows the round that just ended (see to_screen).
                let before = held_ids(r).len();
                let picked = raised.and_then(|k| r.cards.get(k));
                let card = picked.map_or(Value::Null, |c| json!({ "en": c.en, "zh": c.zh, "rawName": c.raw_name }));
                let named_id = |c: &read::Card| matches!(c.how, Some(read::How::Name | read::How::Partial | read::How::Tooltip)).then_some(c.id).flatten();
                // The raised card's id only from a name read on the card: one taken from the tooltip would make the
                // tooltip (`hovered`) its own second witness.
                let id = picked.filter(|c| c.how != Some(read::How::Tooltip)).and_then(named_id);
                let hovered = hovered.or(clicked).filter(|_| !mixed).and_then(|k| r.cards.get(k)).and_then(named_id);
                self.game.pending_pick = Some(PendingPick { before, round: shown + i32::from(before > 0), card, id, hovered });
            }
        } else {
            // No selection screen of this hand was read steady (a confirm right after the board reopened, or two
            // confirms with no hand read in between): the row keeps what is known (the round, the cues and the tanks
            // clicked) and says what it lacks, instead of taking another screen as its board.
            let picked = cues.iter().any(|c| matches!(c, Cue::CardClick | Cue::TankClick { .. } | Cue::Confirm | Cue::PotionUsed | Cue::Picked | Cue::Reroll { .. }));
            if self.stable.is_some() || picked {
                let round = self.stable.as_ref().and_then(|st| st.reading.round).or_else(|| self.game.rounds_seen.last().copied());
                if round == Some(LAST_ROUND) && !rerolled {
                    self.game.final_picked = true;
                }
                let missing: Vec<&str> = if rerolled { vec!["before", "hand"] } else { vec!["before", "hand", "card"] };
                self.game.round_row = Some(json!({
                    "round": round,
                    "phase": if rerolled { json!("reroll") } else { Value::Null },
                    "before": Value::Null,
                    "chosen": { "card": Value::Null, "how": Value::Null, "hand": [], "tankClicks": tanks.len(), "tanks": tanks },
                    "cues": cue_names,
                    "missing": missing,
                }));
            }
        }
        // Frames read before the confirm cannot be used to open the gate. After a confirm or reroll the value-ticking clock restarts (when rerolling repeatedly,
        // no steady board is read in between). Results finished during value ticking but not yet shown are also dropped.
        if self.game.round_row.is_some() {
            self.game.played = true;
            if !rerolled {
                self.game.picks += 1;
            }
        }
        (self.scan, self.queued, self.solve, self.early) = (None, None, None, None);
        self.game.tried.clear();
        self.pending_since = None;
        self.last_sig = None;
        self.gate = Some(Gate { since: at, dealt: false });
        self.view.gated = true;
        self.view.boxes.clear();
        self.set_status(Status::Resolving, vec![t("結算中：等新手牌", "Resolving: waiting for the next cards").into()]);
        self.bump();
    }

    /// The result of one game, one row per game: the final activity read on the result screen (null when it could not be read), the rounds seen,
    /// and the instruments acquired (solver id, acquisition round).
    /// When the result screen was not read (`ultimate` None), the last settled board's final activity is used and
    /// `ultimateFrom` says so (2026-11-05: the result screen was on show for only 0.54 s before start surgery).
    fn log_end(&mut self, ultimate: Option<i64>, at: i64) -> bool {
        let battled = self.battle.on;
        self.log_end_with(ultimate, None, battled, at)
    }

    /// [`VoraxSession::log_end`] with the battle panel's last activity as the last fallback (`battle`), and whether
    /// the game had an endgame battle (`battled`). Returns whether the game's summary was kept for the board page:
    /// only a game played to the last round (seen on a board, or its result screen read) or one that went into its
    /// battle gets one, so a game given up early shows no result card; its `vorax_end` row is written all the same.
    fn log_end_with(&mut self, ultimate: Option<i64>, battle: Option<i64>, battled: bool, at: i64) -> bool {
        if std::mem::replace(&mut self.game.ended, true) {
            return false;
        }
        // Which final activity is the game's: the result screen's (2026-11-06_13-46-25: 877,558; 14-06-38: 850,496).
        // The board's is the fallback only when the result screen was not read, and only from a read taken once round
        // 13's end-of-round effects are in (`last_ultimate`): the held instruments' end-of-round effects (points every
        // round, and so on) fire at the end of every round, rounds 11 to 13 included (crates/vorax game.rs `end_round`
        // runs `run_end_of_round` in round 13 too), so the board grows through the protocol rounds and round 13's gain
        // shows on no board before the result screen (14-06-38: the game log's last Precept_Surgery at 14:07:33.285,
        // the Tool cue at 33.494, the result page at 34.405; the last board read 775,436, the result screen 850,496).
        // The battle panel comes last: it runs a little over the result screen (878.9K for 877,558 and 851.8K for
        // 850,496, +0.15%; the 2026-11-03 ranked replay 836.4K for 833,372, +0.36%) and from 1M shows only 0.1M steps;
        // its bonus is the protocols' risk levels, added wave by wave.
        let (ultimate, from) = match (ultimate, self.game.last_ultimate, battle) {
            (Some(u), _, _) => (Some(u), Some(BoardFrom::ResultScreen)),
            (None, Some(u), _) => (Some(u), Some(BoardFrom::LastBoard)),
            (None, None, Some(u)) => (Some(u), Some(BoardFrom::Battle)),
            (None, None, None) => (None, None),
        };
        let rounds: Vec<i32> = self.game.rounds_seen.iter().copied().collect();
        let from_tag = from.map(BoardFrom::tag);
        self.log_event("vorax_end", at, json!({ "ultimate": ultimate, "ultimateFrom": from_tag, "roundsSeen": rounds, "held": self.game.held }));
        let finished = self.game.rounds_seen.contains(&LAST_ROUND) || battled || from == Some(BoardFrom::ResultScreen);
        if !finished {
            return false;
        }
        let instruments = self
            .game
            .held
            .iter()
            .map(|&(id, _)| {
                crate::vorax::names::names()
                    .cards
                    .iter()
                    .find(|c| c.category == crate::vorax::names::Category::Instrument && c.id == Some(id))
                    .map_or_else(|| format!("#{id}"), |c| c.shown().to_string())
            })
            .collect();
        self.last_game = Some(GameSummary {
            log_dir: self.log.as_ref().map(|l| l.dir().to_path_buf()),
            ended_at: at,
            board: ultimate,
            board_from: from,
            rounds: rounds.len(),
            instruments,
            battle: None,
        });
        true
    }

    /// The last finished Vorax game: kept after the mode ends, for the board page, until a new Vorax game starts or
    /// a Tartarus signal arrives. `ended_at` changes with each game.
    pub fn last_game(&self) -> Option<&GameSummary> {
        self.last_game.as_ref()
    }

    /// Write the `vorax_round` row not yet written; `after` is the signature of the steady board after the selection.
    fn finish_round(&mut self, after: Option<&str>, at: i64) {
        if let Some(mut row) = self.game.round_row.take() {
            row["after"] = json!(after);
            settle_pick(&mut row, after);
            self.log_event("vorax_round", at, row);
        }
    }

    /// One captured frame (the game client area at its original size); `at_ms` is the capture time and `trigger` is the reason.
    pub fn read(&mut self, frame: Rgba, at_ms: i64, trigger: &str) {
        if !self.active() {
            return;
        }
        self.clock = self.clock.max(at_ms);
        // The reader is pointed at the client again before every frame, not only on entering: on 2026-11-08 the test
        // build of 1.1.0 read a whole round of the English client with the Traditional Chinese templates (every screen
        // `other`) until the game was left and entered again. A wrong pointing now costs one frame at most. Unit tests
        // point the reader themselves (`test_zh`, `set_english_here`), so this is not done under test.
        #[cfg(not(test))]
        self.point_reader();
        // Collecting samples: the frame may be saved, and is never read; none of the resolve animation (the battle
        // has no gate).
        #[cfg(feature = "dev-samples")]
        if self.samples.is_some() {
            let (mode, ui) = (self.mode, self.ui());
            let animating = mode != Mode::Battle && self.gate_closed(at_ms);
            if let (Some(s), Some(log), false) = (&mut self.samples, &mut self.log, animating) {
                s.frame(&mut self.learn, log, &frame, at_ms, mode, ui);
            }
            return;
        }
        if self.mode == Mode::Battle {
            self.battle_frame(frame, at_ms);
            return;
        }
        // Observe mode: resolve strips and the intro reveal page are saved before the gate (no OCR during the resolve animation).
        if let Some(log) = &mut self.log {
            self.learn.before_read(log, &frame, at_ms, self.view.round);
            self.view.learn_note = self.learn.note().map(str::to_string);
        }
        if self.gate_closed(at_ms) {
            // Resolve animation: do not read, only tell Watch the screen is still changing.
            self.news = Some((true, false));
            return;
        }
        if self.config.inline {
            let done = read_frame(frame, at_ms, trigger.to_string(), self.ui());
            self.handle(done);
            return;
        }
        if self.scan.is_some() {
            self.queued = Some((frame, at_ms, trigger.to_string()));
            return;
        }
        self.start_read(frame, at_ms, trigger.to_string());
    }

    fn start_read(&mut self, frame: Rgba, at: i64, trigger: String) {
        let (tx, rx) = channel();
        self.scan = Some(rx);
        let notify = self.notify.clone();
        let ui = self.ui();
        std::thread::spawn(move || {
            if tx.send(read_frame(frame, at, trigger, ui)).is_ok()
                && let Some(n) = notify
            {
                n();
            }
        });
    }

    /// Collect background results; returns true when the [`View`] changed.
    pub fn poll(&mut self, now_ms: i64) -> bool {
        let before = self.view.version;
        self.clock = self.clock.max(now_ms);
        if let Some(rx) = &self.scan {
            match rx.try_recv() {
                Ok(done) => {
                    self.scan = None;
                    self.handle(done);
                    if let Some((frame, at, trigger)) = self.queued.take()
                        && self.active()
                    {
                        self.start_read(frame, at, trigger);
                    }
                }
                Err(TryRecvError::Disconnected) => self.scan = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(p) = &self.solve {
            match p.rx.try_recv() {
                Ok(solved) => {
                    self.solve = None;
                    self.solved(solved);
                }
                Err(TryRecvError::Disconnected) => self.solve = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.scan.is_none() && self.battle.board_end {
            self.flush_board_end();
        }
        if let Some(rx) = &self.battle.scan {
            match rx.try_recv() {
                Ok((panel, crop, at)) => {
                    self.battle.scan = None;
                    self.battle_read(panel, crop, at);
                }
                Err(TryRecvError::Disconnected) => {
                    self.battle.scan = None;
                    self.schedule_battle_read();
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        // The agreed frame did not come: ask for the next one directly.
        if let Some(due) = self.battle.due
            && self.battle.scan.is_none()
            && now_ms >= due + READ_IN_LATE_MS as i64
        {
            self.battle.due = None;
            self.read_next = true;
        }
        self.view.version != before
    }

    /// Enter the endgame battle: restart tracking, note the board's final activity, and ask for a frame.
    fn start_battle(&mut self, at: i64) {
        // The board's final activity only when known as final (the result screen, or a board read after round 13's
        // end-of-round effects): the last value on the board can be the one before the held instruments' last
        // end-of-round gain (2026-11-06_14-06-38: 775,436 on round 13's board, 850,496 on the result screen).
        let result = self.last_reading.as_ref().filter(|r| r.kind == Kind::Result).and_then(|r| r.ultimate);
        self.battle = BattleRun { on: true, fight_at: at, board_end: self.game.has_play(), board: result.or(self.game.last_ultimate), ..BattleRun::default() };
        self.view.battle = None;
        self.view.give_up = None;
        self.read_next = true;
    }

    /// Write the board game's `vorax_end` (the time is start surgery): if the result screen was read once, use its final activity (no need
    /// to wait for two identical reads), and null if it was not read. Not rewritten when it was already written from a steady result screen (`log_end`).
    fn flush_board_end(&mut self) {
        if !std::mem::take(&mut self.battle.board_end) {
            return;
        }
        let ultimate = self.last_reading.as_ref().filter(|r| r.kind == Kind::Result).and_then(|r| r.ultimate);
        if let Some(u) = ultimate.or(self.game.last_ultimate) {
            self.battle.board = Some(u);
        } else if self.battle.on && !self.game.ended {
            // No final activity yet: written when the battle ends, from the panel's last activity (end_battle).
            self.battle.end_deferred = true;
            return;
        }
        self.log_end(ultimate, self.battle.fight_at);
    }

    /// How long after one panel read the next one is due: [`BATTLE_NEAR_EVERY_MS`] with the give-up hint on, the last
    /// read close to the cap ([`BattleRun::near_cap`]) and the hint not up yet, else [`BATTLE_EVERY_MS`].
    fn battle_every(&self) -> i64 {
        if self.config.give_up_hint && self.battle.near_cap && self.view.give_up.is_none() {
            BATTLE_NEAR_EVERY_MS
        } else {
            BATTLE_EVERY_MS
        }
    }

    /// Schedule the next panel read ([`VoraxSession::battle_every`] after the last one) and ask the UI for a frame at that time.
    fn schedule_battle_read(&mut self) {
        if !self.battle.on {
            return;
        }
        let due = self.battle.last_read + self.battle_every();
        if self.battle.due != Some(due) {
            self.battle.due = Some(due);
            self.read_in = Some(Duration::from_millis((due - self.clock).max(0) as u64));
        }
    }

    /// One frame during the endgame battle: the left panel is read in the background once every [`VoraxSession::battle_every`]. The others are not read,
    /// and the next one is not requested (before, every frame requested the next one, and capture ran at full speed): the next read is scheduled once a read finishes or when the time has not come.
    fn battle_frame(&mut self, frame: Rgba, at: i64) {
        if !self.battle.on || self.battle.scan.is_some() {
            return;
        }
        if at - self.battle.last_read < self.battle_every() {
            self.schedule_battle_read();
            return;
        }
        self.battle.last_read = at;
        self.battle.due = None;
        let ui = self.ui();
        self.battle.frame = Some(((frame.w, frame.h), ui));
        if self.config.inline {
            let (panel, crop) = read_panel(&frame, ui);
            self.battle_read(panel, crop, at);
            return;
        }
        let (tx, rx) = channel();
        self.battle.scan = Some(rx);
        let notify = self.notify.clone();
        std::thread::spawn(move || {
            let (panel, crop) = read_panel(&frame, ui);
            if tx.send((panel, crop, at)).is_ok()
                && let Some(n) = notify
            {
                n();
            }
        });
    }

    /// A battle panel was read: update the screen, write one `vorax_battle` row when a later wave is read, and for other number changes write
    /// only at least [`BATTLE_ROW_MS`] apart (OCR noise makes numbers come and go, so a row cannot be written on every change), and save the panel
    /// crop of the first read in each wave. When it cannot be read (loading screen, covered), the next read is still scheduled.
    fn battle_read(&mut self, panel: read::Battle, crop: Rgba, at: i64) {
        if self.mode != Mode::Battle || !self.battle.on {
            return;
        }
        if !panel.readable() {
            self.schedule_battle_read();
            return;
        }
        self.battle.reads += 1;
        // This game's lossless battle frame: the panel read the same twice in a row (see LOSSLESS).
        let same = |a: &read::Battle, b: &read::Battle| (a.wave, a.activity, a.bonus, a.next) == (b.wave, b.activity, b.bonus, b.next);
        if self.battle.last.as_ref().is_some_and(|last| same(last, &panel))
            && self.wants_lossless(LOSSLESS[2])
            && self.learn.jpegs_allowed()
            && let Some(log) = &mut self.log
        {
            let name = format!("lossless-{}-{:04}.png", LOSSLESS[2], log.number());
            log.png_as(&name, crop.clone());
            self.game.lossless.push(LOSSLESS[2]);
            self.log_event("vorax_lossless", at, json!({ "screen": LOSSLESS[2], "file": name, "battle": battle_fields(&panel, self.battle.board) }));
        }
        let line = battle_line(&panel);
        self.set_status(Status::Battle, vec![line]);
        self.view.battle = Some(panel.clone());
        self.give_up(&panel, &crop, at);
        // after the hint: how close to the cap this read was sets when the next one is due
        self.schedule_battle_read();
        let wave = panel.wave.map_or(0, |w| w.0);
        let row_due = match &self.battle.logged {
            None => true,
            Some((last, when)) => {
                let changed = (last.wave, last.activity, last.bonus, last.next, last.percent) != (panel.wave, panel.activity, panel.bonus, panel.next, panel.percent);
                wave > self.battle.logged_wave || (changed && at - when >= BATTLE_ROW_MS)
            }
        };
        if row_due {
            self.log_event("vorax_battle", at, battle_fields(&panel, self.battle.board));
            self.battle.logged = Some((panel.clone(), at));
            self.battle.logged_wave = self.battle.logged_wave.max(wave);
        }
        if let Some((wave, _)) = panel.wave
            && self.battle.saved_waves.insert(wave)
            && self.learn.jpegs_allowed()
            && let Some(log) = &mut self.log
        {
            let name = log.jpeg("frame-battle", crop.clone(), FRAME_QUALITY);
            self.log_event("vorax_battle_frame", at, json!({ "wave": wave, "frame": name }));
        }
        if keep_battle_read(&mut self.battle.recent, panel.clone()) {
            self.battle.dropped += 1;
        }
        self.battle.last = Some(panel);
        self.battle.last_crop = Some(crop);
        self.bump();
    }

    /// The give-up hint after a readable battle read `panel` of the panel crop `crop` ([`View::give_up`]): with the
    /// setting on, every reward tier taken on [`GIVE_UP_READS`] reads in a row and the button on show
    /// ([`read::give_up_shown`]), the Give Up button's box ([`read::BATTLE_GIVE_UP`], scaled as
    /// the panel is at the frame's interface scale); a read of another next tier takes it down, one with the next
    /// tier unread leaves it as it is. Logged once a battle (`vorax_give_up_hint`), with the box (`box`), the time from
    /// the first read at the cap (`sinceCapMs`) and from the frame to the read done (`readMs`). Also whether the panel
    /// is close to the cap ([`BattleRun::near_cap`]): at the cap and not yet read [`GIVE_UP_READS`] times, or activity
    /// plus bonus at [`BATTLE_NEAR_SHARE`] of the last tier with another next tier (or the next tier unread).
    fn give_up(&mut self, panel: &read::Battle, crop: &Rgba, at: i64) {
        if panel.all_taken() {
            if self.battle.capped == 0 {
                self.battle.cap_at = Some(at);
            }
            self.battle.capped += 1;
        } else if panel.next.is_some() {
            self.battle.capped = 0;
            self.battle.cap_at = None;
        }
        let total = panel.activity.unwrap_or(0.0) + panel.bonus.unwrap_or(0.0);
        self.battle.near_cap = if self.battle.capped > 0 {
            self.battle.capped < GIVE_UP_READS
        } else {
            total >= BATTLE_NEAR_SHARE * read::BATTLE_LAST
        };
        // the button goes once the battle is won: no box on the empty place
        let button = self.battle.frame.is_some_and(|(_, ui)| read::give_up_shown(crop, ui));
        let shown = self.config.give_up_hint && self.battle.capped >= GIVE_UP_READS && button;
        self.view.give_up = shown.then_some(self.battle.frame).flatten().map(|((w, h), ui)| MarkBox {
            what: "give up".into(),
            at: read::to_frame(w, h, crate::vorax::ui_scale::ui_box(read::BATTLE_GIVE_UP, ui)),
            style: BoxStyle::Plain,
        });
        if self.view.give_up.is_some() && !self.battle.give_up_logged {
            self.battle.give_up_logged = true;
            // "box", not "at": the row's "at" is its time
            let fields = json!({
                "activity": panel.activity,
                "bonus": panel.bonus,
                "wave": panel.wave.map(|w| w.0),
                "box": self.view.give_up.as_ref().map(|b| b.at),
                "sinceCapMs": self.battle.cap_at.map(|c| at - c),
                "readMs": (self.clock - at).max(0),
            });
            self.log_event("vorax_give_up_hint", at, fields);
        }
    }

    /// Leave the endgame battle: write the last panel read as one `vorax_battle_end` row, and save its crop. Written only once:
    /// when leaving is blocked (Vorax turned off, not Traditional Chinese) the mode stays at Battle, and a later leave does not rewrite it.
    fn end_battle(&mut self, at: i64) {
        self.flush_board_end();
        if !self.battle.on {
            return;
        }
        self.read_in = None;
        let run = std::mem::take(&mut self.battle);
        self.view.battle = None;
        self.view.give_up = None;
        // The last read with its numbers settled over the last few reads (one read alone can lose a digit).
        let last = run.last.as_ref().map(|l| settled_battle(&run.recent, l));
        if run.end_deferred {
            // The board game's ending held back at start surgery: the panel's activity at its last read is the board's
            // final activity played out (null when no panel was read). Stamped at start surgery like the others.
            let activity = last.as_ref().and_then(|b| b.activity).map(|a| a.round() as i64);
            self.log_end_with(None, activity, true, run.fight_at);
        }
        let Some(last) = last else {
            self.log_event("vorax_battle_end", at, json!({ "board": run.board, "reads": 0 }));
            return;
        };
        if let Some(g) = &mut self.last_game {
            g.battle = Some(last.clone());
            g.ended_at = at;
        }
        let mut fields = battle_fields(&last, run.board);
        fields["reads"] = json!(run.reads);
        // How many of the last reads the activity and bonus are the medians of, those reads, and how many reads of
        // the whole battle had their bonus dropped as a misread (so the settling can be checked from the log).
        fields["settledFrom"] = json!(run.recent.len());
        fields["recent"] = recent_fields(&run.recent);
        fields["bonusDropped"] = json!(run.dropped);
        if let Some(crop) = run.last_crop
            && self.learn.jpegs_allowed()
            && let Some(log) = &mut self.log
        {
            fields["frame"] = json!(log.jpeg("frame-battle", crop, FRAME_QUALITY));
        }
        self.log_event("vorax_battle_end", at, fields);
    }

    /// A result screen's final activity below the last settled one read on the board is a misread (the held
    /// instruments' end-of-round effects keep adding to it through rounds 11 to 13: 2026-11-06_14-06-38 went
    /// 636,116, 703,976, 775,436 on the boards, 850,496 on the result screen; whether no instrument can lower it was
    /// not checked, and a wrong drop only falls back to the board or the battle panel). So is one far above it,
    /// see [`RESULT_OVER_BOARD`]: the fading digits of the result screen's opening (14-06-38 full-other-0071 shows a
    /// ghost `1?3?0` over 850496) could add a digit. Dropped, the ending falls back as when the result screen was
    /// not read.
    fn check_result(&self, r: &mut Reading) {
        if r.kind == Kind::Result
            && let (Some(u), Some(board)) = (r.ultimate, self.game.board_ultimate)
        {
            if u < board {
                r.ultimate = None;
                r.problems.push(format!("結算畫面讀到 {u}，比盤面最後讀到的 {board} 小，不採用"));
            } else if u as f64 > board as f64 * RESULT_OVER_BOARD {
                r.ultimate = None;
                r.problems.push(format!("結算畫面讀到 {u}，超過盤面最後讀到的 {board} 的 {RESULT_OVER_BOARD} 倍，不採用"));
            }
        }
    }

    /// The result of one read.
    fn handle(&mut self, mut d: Done) {
        if !self.active() {
            return;
        }
        // While the log gives no scale, keep what this frame was read at: a measured scale (or the cached one,
        // checked again on the frame) is reused for the next frames; a frame measured as the original clears it.
        if self.ui_scale.is_none() {
            self.game.measured_ui = d.s;
        }
        self.check_result(&mut d.reading);
        if self.mode == Mode::Battle {
            // The frame still being read before start surgery (mostly the result screen): only its final activity is used to write the ending; the status line is not changed,
            // no read row is written, and no frame is saved.
            if d.reading.kind == Kind::Result && d.reading.ultimate.is_some() {
                self.last_reading = Some(d.reading);
            }
            self.flush_board_end();
            return;
        }
        let mut r = d.reading;
        self.mark_openings(&mut r);
        // One card name misread on a hand otherwise read as before: its name from the read just before when that one
        // named every card, else from this hand's last fully named steady read (never from a frame of another deal,
        // see `Game::named_hand` and `Game::prev_named`).
        let prev = self.game.prev_named.take();
        if !prev.as_ref().is_some_and(|p| carry_names(&mut r, p).is_some())
            && let Some(named) = &self.game.named_hand
        {
            carry_names(&mut r, named);
        }
        // A name taken from the tooltip (`How::Tooltip`) is not passed on: only names read on the card are carried.
        let read_whole = r.kind.is_choice() && d.at >= self.game.hand_from && !r.cards.is_empty() && r.cards.iter().all(|c| c.index.is_some() && !matches!(c.how, Some(read::How::Carried | read::How::Tooltip)));
        self.game.prev_named = read_whole.then(|| r.clone());
        let size = (d.frame.w, d.frame.h);
        let sig = signature(&r);
        let steady = self.last_sig.as_deref() == Some(sig.as_str());
        self.news = Some((!steady, false));
        // A choice screen read with a problem (a field missing, such as the rerolls or a card name) is read again
        // after a while even if nothing on screen moves: auto read waits for a change, so on 2026-11-08 (English
        // client, round 6) a hand read without its rerolls had no advice for 2 s, until the player moved the mouse
        // onto a card. Waits 250, 500 ms, then 1 s each, at most RETRY_READS times for the same reading.
        if r.kind.is_choice() && !r.problems.is_empty() {
            let n = if self.retry.0 == sig { self.retry.1 + 1 } else { 1 };
            self.retry = (sig.clone(), n);
            if n <= RETRY_READS && self.read_in.is_none() {
                self.read_in = Some(Duration::from_millis(250 << (n - 1).min(2)));
            }
        } else {
            self.retry = (String::new(), 0);
        }
        // Right after the surgery page opens again (the game closes and opens it around every pick), the first frame
        // still shows the page coming in (`other`); auto read then waits for the screen to stop moving, and the hand
        // was read about 0.45 s after the page opened, advice at 0.8 s (2026-11-08). Within BOARD_EARLY_MS of the page
        // opening, an `other` frame asks for the next one at once.
        if self.mode == Mode::Board && r.kind == Kind::Other && d.at - self.board_at < BOARD_EARLY_MS && self.read_in.is_none() {
            self.read_in = Some(Duration::from_millis(BOARD_EARLY_EVERY_MS));
        }
        self.last_sig = Some(sig.clone());
        self.view.kind = Some(r.kind);
        if r.kind.is_choice() {
            if let Some(s) = seen_card(&r, false) {
                note_raised(&mut self.game.raised_seen, &mut self.game.raised_other, s);
            }
            self.game.hovered = hovered_kept(self.game.hovered.take(), &r);
            if read::norm(&r.tooltip).chars().count() >= 2
                && let Some(s) = seen_card(&r, true)
            {
                if self.game.hover_log.len() >= HOVER_LOG {
                    self.game.hover_log.remove(0);
                }
                self.game.hover_log.push((d.at, s));
            }
            if steady && d.at >= self.game.hand_from && r.cards.iter().all(|c| c.index.is_some() && c.how != Some(read::How::Tooltip)) {
                self.game.named_hand = Some(r.clone());
            }
        }
        if r.round.is_some() {
            self.view.round = r.round;
        }
        if r.ultimate.is_some() {
            self.view.ultimate = r.ultimate;
        }
        if (r.kind.is_choice() || r.kind == Kind::Resolving) && !r.ultimate_pending && r.ultimate.is_some() {
            self.game.board_ultimate = r.ultimate;
        }
        // A settled final activity on the board (a choice screen, or the board while a round resolves: the last round's
        // gains only show after its resolve, ranked replay t108.30 659506 -> t111.47): the fallback for a result screen
        // left before it is read.
        // Only once round 13's end-of-round effects are in: a read showing round 14 (the round counter moved past 13),
        // or taken after a Tool cue that came after the last pick (the instruments' end-of-round sound; 14-06-38:
        // Precept_Surgery 33.285, Tool 33.494). The two games that took this fallback (2026-11-05_17-43-38,
        // 2026-11-06_03-05-30) took it from resolving reads of round 14.
        let effects_in = r.round.is_some_and(|n| n > LAST_ROUND) || self.game.final_tool.is_some_and(|t| d.at > t);
        if self.game.final_picked && effects_in && (r.kind.is_choice() || r.kind == Kind::Resolving) && !r.ultimate_pending && r.ultimate.is_some() {
            self.game.last_ultimate = r.ultimate;
        }
        self.view.frame_size = Some(size);
        self.view.board = Some(describe(&r));
        // Observe mode: things that cannot be read, the tooltip, and unrecognized icons added by a confirmed instrument.
        if let Some(log) = &mut self.log {
            let icons = d.icons.clone().unwrap_or_default();
            self.learn.set_ui(d.s);
            self.learn.after_read(log, &d.frame, &r, steady, d.at, || icons.clone());
            if steady
                && r.kind.is_choice()
                && let Some(p) = &self.game.pending_pick
                && held_ids(&r).len() == p.before + 1
                && let Some(j) = r.held.iter().rposition(|h| !h.opening)
                && r.held[j].id.is_none()
            {
                self.learn.icon_pair(log, &d.frame, p.card.clone(), (j, r.held.len()), &icons, d.at);
            }
            // A board or result screen read as another screen: its whole frame, for other UI scales (HANDOFF 3c). The
            // intro page always reads so and is left out; the endgame battle never comes here.
            if matches!(self.mode, Mode::Board | Mode::Result) {
                let mode = if self.mode == Mode::Board { "board" } else { "result" };
                self.learn.full_frame(log, &d.frame, &r, d.at, mode);
            }
            self.view.learn_note = self.learn.note().map(str::to_string);
        }
        // Observe mode: this hand's last steady, complete selection screen, for its evidence image (see LOSSLESS).
        if steady && r.kind.is_choice() && r.problems.is_empty() && self.config.observe && self.log.is_some() {
            self.game.steady_shot = Some(Shot { sig: sig.clone(), at: d.at, round: r.round, frame: d.frame.clone(), ui: d.s });
        }
        // A steady selection screen read with a problem gets no advice and so no evidence above: its own, once a round
        // (role "unreadable"), so the misread can be checked on a lossless frame (2026-11-09: the English game's tank
        // values unread from round 5 on left only JPEG frames).
        if steady && r.kind.is_choice() && !r.problems.is_empty() && self.config.observe && self.game.unreadable_round != Some(r.round) {
            self.game.unreadable_round = Some(r.round);
            let shot = Shot { sig: sig.clone(), at: d.at, round: r.round, frame: d.frame.clone(), ui: d.s };
            self.save_evidence(&shot, "unreadable");
        }
        // This game's first steady pick and result screen as lossless PNGs (observe mode, see LOSSLESS).
        let lossless_kind = if r.kind.is_choice() { Some(LOSSLESS[0]) } else { (r.kind == Kind::Result).then_some(LOSSLESS[1]) };
        let mut png = None;
        if steady
            && let Some(kind) = lossless_kind
            && self.wants_lossless(kind)
            && self.learn.jpegs_allowed()
            && let Some(log) = &mut self.log
        {
            let name = format!("lossless-{kind}-{:04}.png", log.number());
            log.png_as(&name, d.frame.clone());
            self.game.lossless.push(kind);
            png = Some(name);
        }
        // A new steady board saves one frame (selection and result screens); observe mode does not save when it finds there is not enough space.
        let mut frame = None;
        if steady && (r.kind.is_choice() || r.kind == Kind::Result) && self.game.saved_sig.as_deref() != Some(sig.as_str()) {
            if let Some(log) = &mut self.log
                && self.learn.jpegs_allowed()
            {
                frame = Some(log.jpeg("frame", d.frame, FRAME_QUALITY));
            }
            self.game.saved_sig = Some(sig.clone());
        }
        let raised: Vec<usize> = r.cards.iter().enumerate().filter(|(_, c)| c.raised).map(|(k, _)| k).collect();
        self.log_event(
            "vorax_read",
            d.at,
            json!({ "trigger": d.trigger, "readMs": d.read_ms, "frame": frame, "png": png, "size": [size.0, size.1], "reading": &r, "signature": sig, "steady": steady, "gate": self.gate.is_some(), "raised": raised, "client": d.client, "point": self.point.lock().map(|p| p.clone()).unwrap_or(Value::Null) }),
        );
        if let (Some(file), Some(kind)) = (&png, lossless_kind) {
            self.log_event("vorax_lossless", d.at, json!({ "screen": kind, "file": file, "signature": sig, "reading": &r }));
        }
        // An early solve (or solving while it ticks) is for another board: if this read's signature differs it is dropped, and one in progress is also told to give up
        // (when a half-dealt hand is read mid deal animation, the new one need not queue behind it).
        // But when it is solving a board that is already steady it is not dropped: one differing read caused by the mouse tooltip should not make the steady board's result
        // be recomputed (when the board really changes, the next steady read replaces solve, and solved() accepts only results with the steady signature).
        let stable = self.stable.as_ref().map(|s| s.sig.clone());
        let keep = |k: &str| stable.as_deref() == Some(k);
        if self.early.as_ref().is_some_and(|e| e.sig != sig && !keep(&e.sig)) {
            self.early = None;
        }
        if self.solve.as_ref().is_some_and(|p| p.early && p.sig != sig && !keep(&p.sig)) {
            self.solve = None;
        }
        if steady {
            self.steady(r, sig, size, d.s, d.at);
        } else {
            if r.kind != Kind::Other {
                self.read_next = true;
            }
            self.unsteady(&r);
            self.solve_ahead(&r, &sig, d.at);
            self.last_reading = Some(r);
        }
        self.bump();
    }

    /// Early solve: when a selection screen is read for the first time (not yet steady) and can be read in full, start solving with this read's signature,
    /// and check and show the result when the same signature is read again (steady) ([`VoraxSession::choice`],
    /// [`VoraxSession::solved`]). The same signature in the same hand is solved early only once (consecutive reads, or
    /// reading it and then reading it back, do not repeat); the normal solve of a steady board is not interrupted while it is in progress.
    fn solve_ahead(&mut self, r: &Reading, sig: &str, at: i64) {
        if self.mode != Mode::Board || !r.kind.is_choice() || r.kind == Kind::Protocol {
            return;
        }
        if self.advised.as_ref().is_some_and(|a| a.sig == sig)
            || self.solve.as_ref().is_some_and(|p| !p.early || p.sig == sig)
            || self.early.as_ref().is_some_and(|e| e.sig == sig)
            || self.game.tried.contains(sig)
        {
            return;
        }
        if let Ok((x, rounds_known)) = self.screen_state_ahead(r) {
            self.game.tried.insert(sig.to_string());
            self.start_solve(sig.to_string(), x, rounds_known, at, true);
        }
    }

    /// This board was read for the first time: do a confirming extra read.
    fn unsteady(&mut self, r: &Reading) {
        if self.gate.is_some() {
            return;
        }
        // Same hand with only a card raised or covered, or the card-pick animation making a card unreadable: keep the advice and boxes as they are.
        if self.same_hand(r) || self.resolving_glitch(r) || self.garbled_read(r) {
            return;
        }
        self.view.boxes.clear();
        match r.kind {
            k if k.is_choice() => {
                self.keep_stale(r);
                self.set_status(Status::Confirming, vec![t("確認中", "Confirming").into()]);
            }
            Kind::Resolving => self.set_status(Status::Resolving, vec![t("結算中", "Resolving").into()]),
            Kind::Result => self.set_status(Status::Result, vec![t("結算畫面", "Result screen").into()]),
            _ if self.mode == Mode::Intro => self.set_status(Status::Intro, vec![t("開局頁", "Intro page").into()]),
            _ => self.set_status(Status::Idle, vec![t("等盤面", "Waiting for the board").into()]),
        }
    }

    /// Same hand as the one advice was given for: no confirm or reroll since (the log takes priority over the screen), round and remaining rerolls are the same
    /// (or cannot be read this time), the card count is the same, every card that can be read is the same as at advice time at the same position, and at most one card cannot be read. When a card is raised
    /// after picking, or the mouse tooltip covers the left panel or a neighboring card's name, the read misses one or two spots and the advice is still shown
    /// (2026-11-03 ranked game: after picking a card, the decision box disappeared).
    fn same_hand(&self, r: &Reading) -> bool {
        let Some(a) = &self.advised else { return false };
        let now = hand(r);
        a.moves == self.hand_moves()
            && r.kind.is_choice()
            && (r.round.is_none() || r.round == a.round)
            && (r.rerolls.is_none() || r.rerolls == a.rerolls)
            && now.len() == a.hand.len()
            && now.iter().filter(|c| c.is_none()).count() <= 1
            && now.iter().zip(&a.hand).all(|(n, h)| n.is_none() || n == h)
    }

    /// After advice was given the log has no confirm, reroll or deal, yet this (not yet steady) read has two or more cards unreadable:
    /// it is a garbled read as the mouse slides over the cards, not a new hand (the 2026-11-04 08:47 game, a five-card fan read as three
    /// with two of them garbled, and the boxes vanished for 0.5 seconds). A real change of hand always has a log line first; decide on the steady read.
    fn garbled_read(&self, r: &Reading) -> bool {
        let Some(a) = &self.advised else { return false };
        a.moves == self.hand_moves()
            && r.kind.is_choice()
            && (r.round.is_none() || r.round == a.round)
            && hand(r).iter().filter(|c| c.is_none()).count() >= 2
    }

    /// "Resolving" is read after advice was given and before any confirm or reroll: the card-raise animation after picking made the cards unreadable,
    /// it is not a real resolve (2026-11-04 ranked game: after picking a potion that needs a target monster, the read went back and forth between resolving and the selection screen,
    /// the boxes vanished for 2 seconds, and there were no boxes when the player clicked the monster). A real resolve always comes after the confirm, when hand_moves has already
    /// changed and the gate is open.
    /// New opening instruments without a template: the instruments revealed at the start are already held in round 1 before any instrument is picked, and when the icon
    /// is not recognized, advice used to be blocked for the whole game (`雜交試劑瓶`, 2026-11-03 21:29 ranked game). The icons read on the round 1 instrument selection screen
    /// before anything was picked are all from the start, so note the maximum count; after that, unrecognized slots at the far left up to that count are always treated as opening instruments
    /// (newly picked instruments are added on the right and the opening ones shift left; tonight in 16 games and over 1800 reads the opening one was always at held[0]).
    /// Verified only for holding <= 4 (1 opening plus 3 picked, and SLOT_X has only 4 slots): with more, check again whether the opening one
    /// could be pushed off screen. Opening instruments only change the initial board, and the solver skips them anyway.
    fn mark_openings(&mut self, r: &mut Reading) {
        if r.kind == Kind::Instrument && r.round == Some(1) && self.game.held.is_empty() && self.game.pending_pick.is_none() {
            self.game.openings = Some(self.game.openings.unwrap_or(0).max(r.held.len()));
        }
        // A normal instrument selection happens only MAX_PICKS times: if there are more icons than that, the extra slots at the far left are always revealed at the start.
        // Useful when Ascendant is opened midway without seeing round 1 (on 2026-11-04 18:28 after a version change it was reopened in round 4, and an unrecognized
        // opening instrument at the far left blocked advice). In all 1134 steady reads with 4 slots on 11-03/04, the far left was always an opening instrument.
        // Verified only up to 4 slots (the data has no 5 or more, and SLOT_X has only 4 slots): if an event or effect gives extra instruments,
        // this premise no longer holds and needs another look.
        if r.kind.is_choice() && r.held.len() > MAX_PICKS {
            self.game.openings = Some(self.game.openings.unwrap_or(0).max(r.held.len() - MAX_PICKS));
        }
        for h in r.held.iter_mut().take(self.game.openings.unwrap_or(0)) {
            if h.id.is_none() && !h.opening {
                h.opening = true;
                h.zh = Some("開局用具".to_string());
            }
        }
    }

    fn resolving_glitch(&self, r: &Reading) -> bool {
        r.kind == Kind::Resolving
            && self.gate.is_none()
            && self.advised.as_ref().is_some_and(|a| a.moves == self.hand_moves() && (r.round.is_none() || r.round == a.round))
    }

    /// With the same round and same hand, the previous advice is kept greyed out; if different, it is cleared.
    fn keep_stale(&mut self, r: &Reading) {
        let same = self.advised.as_ref().is_some_and(|a| a.round == r.round && a.hand == hand(r));
        if same {
            self.view.stale = self.view.advice.is_some();
        } else {
            (self.view.advice, self.view.stale) = (None, false);
        }
    }

    /// The same board was read twice in a row.
    fn steady(&mut self, r: Reading, sig: String, size: (usize, usize), ui: Option<f64>, at: i64) {
        // Look only at the round of steady reads: a wrongly read round number from a single read will not misjudge a resumed game as a new one.
        self.check_resumed(&r, at);
        if !r.ultimate_pending {
            self.pending_since = None;
        }
        // The green "+N" is not always a value counting. After a reroll (191 of 201 frames), or a pick that left the
        // final activity unchanged, the game shows "+0" over it, and on all 201 frames read the value did not change
        // again on the same board (all logs to 2026-11-07); it may stay until the value next changes. The value read
        // equal to the tanks' sum is settled: on 113 reads flagged counting with the final activity equal to the sum it
        // never changed again on the same board, while the flag took 1.0 s (median, 4.9 s at most) to clear. Where the
        // "+0" covers the digits so they are not read, the flag on one steady board for longer than PENDING_MAX_MS is
        // taken as settled too (2026-11-07_17-59-47 round 8: the advice solved meanwhile came 6.2 s late, only when a
        // tooltip covered the panel; 2026-11-03_15-45-04 the same, 5 s). Settled, the advice shows; the solver input
        // takes the tanks' sum.
        if r.ultimate_pending && self.pending_sig.as_deref() != Some(sig.as_str()) {
            (self.pending_since, self.pending_sig) = (None, Some(sig.clone()));
        }
        let mut r = r;
        if r.ultimate_pending && r.ultimate.is_some() && r.ultimate == r.tank_sum {
            r.ultimate_pending = false;
        }
        if r.ultimate_pending
            && let Some((since, logged)) = self.pending_since
            && at - since > PENDING_MAX_MS
        {
            r.ultimate_pending = false;
            if !logged {
                self.pending_since = Some((since, true));
                self.log_event("vorax_pending_stuck", at, json!({ "since": timestamp(at_time(since)), "ms": at - since, "settled": true }));
            }
        }
        if let Some(round) = r.round {
            self.game.rounds_seen.insert(round);
        }
        if r.kind.is_choice() && r.round.is_some() {
            self.game.monsters = monsters(&r.tanks);
        }
        if self.gate.is_some() {
            if !r.kind.is_choice() {
                self.last_reading = Some(r);
                return;
            }
            self.gate = None;
            self.view.gated = false;
        }
        // Before the pick's row is written: when the new icon is taken over the raised card, the row names it too.
        self.check_pick(&r, at);
        if r.kind.is_choice() || r.kind == Kind::Result {
            self.finish_round(Some(&sig), at);
        }
        if r.kind.is_choice() {
            let fuller = self
                .game
                .choice
                .as_ref()
                .is_some_and(|(_, c)| c.kind == r.kind && c.round == r.round && same_cards(&hand(c), &hand(&r)) && unread_cards(c) < unread_cards(&r));
            if !fuller {
                self.game.choice = Some((sig.clone(), r.clone()));
            }
        }
        match r.kind {
            Kind::Potion => self.game.potion_rerolls = r.rerolls.or(self.game.potion_rerolls),
            Kind::Instrument => {
                // The first instrument screen of a game, before any instrument is picked: its count is the opening
                // one (with Intern Shelley's bonus); remember it for games where it is never seen.
                if self.game.instrument_rerolls.is_none()
                    && r.round == Some(1)
                    && held_ids(&r).is_empty()
                    && let Some(n) = r.rerolls
                {
                    self.remember_opening(n);
                }
                self.game.instrument_rerolls = r.rerolls.or(self.game.instrument_rerolls)
            }
            _ => {}
        }
        (self.view.potion_rerolls, self.view.instrument_rerolls) = (self.game.potion_rerolls, self.game.instrument_rerolls);
        self.view.in_kit = self.game.kit && r.kind == Kind::Potion;
        if r.kind.is_choice() {
            self.view.tanks = r.tanks.clone();
            self.view.held = r.held.iter().map(|h| (h.zh.clone().or_else(|| h.en.clone()).unwrap_or_else(|| "?".to_string()), h.opening)).collect();
        }
        self.stable = Some(Stable { sig, reading: r.clone(), size, ui });
        match r.kind {
            Kind::Result => {
                self.view.boxes.clear();
                self.set_status(Status::Result, vec![t("結算畫面", "Result screen").into()]);
                if r.ultimate.is_some() {
                    self.log_end(r.ultimate, at);
                }
            }
            // The card-pick animation made the cards unreadable (see resolving_glitch): keep the advice and boxes as they are.
            Kind::Resolving if self.resolving_glitch(&r) => {}
            Kind::Resolving => {
                self.view.boxes.clear();
                self.set_status(Status::Resolving, vec![t("結算中", "Resolving").into()]);
            }
            Kind::Other => {
                self.view.boxes.clear();
                if self.mode == Mode::Intro {
                    self.set_status(Status::Intro, vec![t("開局頁", "Intro page").into()]);
                } else {
                    self.set_status(Status::Idle, vec![t("等盤面", "Waiting for the board").into()]);
                }
            }
            // The green +N at the upper right of the final activity is still there: the value is still ticking, so no advice is given (plan section 3.3). The signature does not include it,
            // so the next read of the same board after it stops ticking will solve.
            _ if r.ultimate_pending => {
                // The next read after ticking stops is the one that solves, so another read is requested right away: scheduled reads all land while the value is still ticking,
                // after that we would wait for mouse movement or other events before reading, and the advice after a reroll was up to 5 seconds late (2026-11-03 ranked game).
                // Ticking on one board for longer than PENDING_MAX_MS is taken as settled in steady() (and logged there), so
                // a "+N" misread as always present does not turn into nonstop full-speed capture plus OCR.
                self.pending_since.get_or_insert((at, false));
                self.read_next = true;
                // The same board that already got advice (the signature does not include the final activity): keep the advice and boxes as they are, no flicker.
                let advised = self.advised.as_ref().is_some_and(|a| self.stable.as_ref().is_some_and(|s| s.sig == a.sig));
                if !advised {
                    self.view.boxes.clear();
                    self.set_status(Status::Resolving, vec![t("結算中：數值還在跳", "Resolving: the numbers are still counting").into()]);
                    // Solve while it ticks: the signature is already steady, only the final activity is still ticking. The solver input does not include the final activity
                    // (to_screen checks it against the tank sums only when it is not ticking), so start solving first and show the result after ticking stops.
                    self.solve_early(at);
                }
            }
            _ => self.choice(at),
        }
        self.last_reading = Some(r);
    }

    /// The solver input: the board that was read plus the acquisition rounds and the two reroll counts tracked for this game. The second value is
    /// whether all acquisition rounds are known (the tracked ones were used).
    fn screen_state(&self, r: &Reading) -> Result<(ScreenState, bool), Vec<String>> {
        self.screen_state_with(r, &self.game.held, &self.game.from_card)
    }

    /// A pending instrument pick ([`PendingPick`]) recorded into `game.held` once the new icon shows on a selection
    /// screen `r`.
    fn check_pick(&mut self, r: &Reading, at: i64) {
        if !r.kind.is_choice() {
            return;
        }
        let Some(p) = &self.game.pending_pick else { return };
        // A newly acquired icon is added at the far right.
        let now = held_ids(r);
        if now.len() == p.before + 1 {
            match (now.last(), p.id) {
                // The card and the new icon do not match: the card wins (tonight 39 agreed and 1 disagreed, the wrong one being the VSR fallback recognizing `疫區聖母像`
                // as `獸筋絞肉索` at 0.97); one entry goes in the log.
                // Unless the card the tooltip named at confirm is the icon's: two witnesses against a raised flag
                // the hover put on a neighbouring card (2026-11-06_20-03-09 round 4: `髒污刮骨刀` hovered, the
                // suggested card, and its icon came; `鞣製皮革拘束帶` beside it read raised, and the solver held
                // the wrong instrument for the rest of the game).
                (Some(Some(icon)), Some(card)) if *icon != card => {
                    let icon_wins = p.hovered == Some(*icon);
                    self.log_event("vorax_pick_mismatch", at, json!({ "card": card, "icon": icon, "picked": p.card, "hovered": p.hovered, "kept": if icon_wins { "icon" } else { "card" } }));
                    if icon_wins {
                        self.game.held.push((*icon, p.round));
                        // The row names the icon's card rather than the raised one both the icon and the hover deny.
                        if let Some(row) = &mut self.game.round_row {
                            name_from_icon(row, *icon, p.round - i32::from(p.before > 0));
                        }
                    } else {
                        self.game.from_card.insert(self.game.held.len());
                        self.game.held.push((card, p.round));
                    }
                }
                (Some(Some(id)), _) => {
                    self.game.held.push((*id, p.round));
                    // No card was seen picked (the pick confirmed before any read raised or pointed at one,
                    // 2026-11-06_21-42-03 round 1: picked within 1.8 s, row 24 null): the icon names it.
                    if let Some(row) = &mut self.game.round_row
                        && row.pointer("/chosen/card").is_some_and(Value::is_null)
                    {
                        name_from_icon(row, *id, p.round - i32::from(p.before > 0));
                    }
                }
                // The new icon has no template yet (`異形螯針`, 2026-11-03 21:44 ranked game): use the card that was raised at confirm time,
                // or else the one the tooltip named (a shallow raise cleared by the reader leaves no raised card).
                (Some(None), _) => {
                    if let Some(id) = p.id.or(p.hovered) {
                        // The tooltip's card stands in for a raised card: logged, to look into a pick of another
                        // card than the one hovered.
                        if p.id.is_none() {
                            self.log_event("vorax_pick_fallback", at, json!({ "hovered": id, "icon": null, "round": p.round }));
                        }
                        self.game.from_card.insert(self.game.held.len());
                        self.game.held.push((id, p.round));
                    }
                }
                _ => {}
            }
            self.game.pending_pick = None;
        }
    }

    /// The solver input for a not-yet-steady read (early solve): an instrument just confirmed is recorded into `game.held` only on the steady read
    /// (steady); here it is counted in by the same rules, so the check after it becomes steady does not mismatch.
    fn screen_state_ahead(&self, r: &Reading) -> Result<(ScreenState, bool), Vec<String>> {
        let mut tracked = self.game.held.clone();
        let mut from_card = self.game.from_card.clone();
        if r.kind.is_choice()
            && let Some(p) = &self.game.pending_pick
        {
            let now = held_ids(r);
            if now.len() == p.before + 1 {
                match (now.last(), p.id) {
                    // The same rule as when it becomes steady: the icon when the hovered card agrees with it.
                    (Some(Some(icon)), Some(card)) if *icon != card && p.hovered != Some(*icon) => {
                        from_card.insert(tracked.len());
                        tracked.push((card, p.round));
                    }
                    (Some(Some(id)), _) => tracked.push((*id, p.round)),
                    (Some(None), _) => {
                        if let Some(id) = p.id.or(p.hovered) {
                            from_card.insert(tracked.len());
                            tracked.push((id, p.round));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.screen_state_with(r, &tracked, &from_card)
    }

    fn screen_state_with(&self, r: &Reading, tracked: &[(u8, i32)], from_card: &BTreeSet<usize>) -> Result<(ScreenState, bool), Vec<String>> {
        let now = held_ids(r);
        // When only part is known, none of it is given (ScreenState::instruments in advisor.rs). The slots recorded from the card
        // ignore the icons (unrecognized, or when they do not match the card the card wins); other slots must be recognized and identical.
        let known = now.len() == tracked.len() && now.iter().zip(tracked).enumerate().all(|(i, (n, (id, _)))| *n == Some(*id) || from_card.contains(&i));
        let held: Option<HeldOverride> = known.then(|| tracked.iter().map(|&(id, round)| (id, Some(round))).collect());
        let mut x = to_screen(r, held.as_ref())?;
        // The reroll count that is off screen: the one seen last, or the opening count if it was never seen.
        match x.phase {
            ScreenPhase::Potion => x.instrument_refreshes = Some(self.game.instrument_rerolls.unwrap_or_else(|| self.opening_instrument_rerolls())),
            ScreenPhase::Instrument => x.potion_refreshes = Some(self.game.potion_rerolls.unwrap_or(POTION_REROLLS)),
            ScreenPhase::Protocol => {}
        }
        Ok((x, known))
    }

    /// A steady selection screen: show the advice if there is some, otherwise solve.
    fn choice(&mut self, at: i64) {
        let Some(st) = &self.stable else { return };
        let sig = st.sig.clone();
        if self.advised.as_ref().is_some_and(|a| a.sig == sig) {
            self.show_advice();
            return;
        }
        // Already finished solving while the value was ticking: show it directly on this read after ticking stops (solved checks the solver input once more with this read).
        if let Some(e) = self.early.take()
            && e.sig == sig
        {
            self.solved(e);
            return;
        }
        let reading = st.reading.clone();
        if let Some(p) = &self.solve
            && p.sig == sig
        {
            // A solve started early (or during ticking) is not finished yet: the board is steady, so change the status to solving and wait for it.
            if p.early && self.view.status != Status::Solving {
                self.keep_stale(&reading);
                self.set_status(Status::Solving, vec![t("求解中", "Solving").into()]);
            }
            return;
        }
        self.view.boxes.clear();
        // Protocols are not solved: explain even when the tanks cannot be fully read (to_screen needs all tanks to be read).
        if reading.kind == Kind::Protocol {
            (self.view.advice, self.view.stale, self.advised) = (None, false, None);
            self.set_status(Status::Protocol, vec![t("手術方案只影響終局戰鬥，不影響最終活性：不給建議。", "Surgical Protocols change the endgame battle, not Ultimate Activity: no advice.").into()]);
            return;
        }
        match self.screen_state(&reading) {
            // Same hand with only one or two spots unreadable (card raised, tooltip): show the advice as before.
            Err(_) if self.same_hand(&reading) => self.show_advice(),
            Err(why) => {
                self.keep_stale(&reading);
                let mut lines = vec![t("讀不到：", "Unreadable:").to_string()];
                lines.extend(why.into_iter().take(WHY_SHOWN));
                lines.push(t("盤面有讀不到的地方，不用猜的盤面去解。", "Part of the board is unreadable: a guessed board is not solved.").into());
                self.set_status(Status::Unreadable, lines);
            }
            Ok((x, rounds_known)) => {
                self.keep_stale(&reading);
                self.set_status(Status::Solving, vec![t("求解中", "Solving").into()]);
                self.start_solve(sig, x, rounds_known, at, false);
            }
        }
    }

    /// A steady board whose value is still ticking (the signature was the same twice in a row): start solving first (solve while it ticks). Not done for protocols, unreadable
    /// boards, or a signature that is already being solved or already solved.
    fn solve_early(&mut self, at: i64) {
        let Some(st) = &self.stable else { return };
        if !st.reading.kind.is_choice() || st.reading.kind == Kind::Protocol {
            return;
        }
        let sig = st.sig.clone();
        if self.solve.as_ref().is_some_and(|p| p.sig == sig) || self.early.as_ref().is_some_and(|e| e.sig == sig) {
            return;
        }
        let reading = st.reading.clone();
        if let Ok((x, rounds_known)) = self.screen_state(&reading) {
            self.start_solve(sig, x, rounds_known, at, true);
        }
    }

    /// The "hand changed" count used for solving while ticking and for the same-hand check: [`VoraxSession::moves`] without new-hand deals. The card animation (Shuffle) first
    /// opens the gate, the board can already be read and the value is still ticking, and the Figure_Refresh of the same hand comes about 0.6 seconds later (ranked game
    /// 06:26:11.553 Shuffle, 11.7 read, 12.149 Figure_Refresh), so a result computed during ticking must not be dropped because of it.
    /// A real change of hand always has a 5619 first (confirm or reroll), by which time the result has already been dropped.
    fn hand_moves(&self) -> u32 {
        self.moves - self.deals
    }

    /// Start solving: with `inline` it finishes here, otherwise it runs in the background (one at a time, a new one replaces the old one).
    fn start_solve(&mut self, sig: String, x: ScreenState, rounds_known: bool, at: i64, early: bool) {
        let (pool, rollouts, moves) = (self.pool(), self.config.rollouts, self.hand_moves());
        if self.config.inline {
            if let Some((advice, ms)) = solve_now(pool.as_deref(), &x, rollouts, &AtomicBool::new(false)) {
                self.solved(Solved { sig, x, advice, ms, read_at: at, rounds_known, rollouts, early, moves });
            }
            return;
        }
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        // Replace the old solve: when the old Solving is dropped its thread is told to give up, so the new one need not queue behind it.
        self.solve = Some(Solving { sig: sig.clone(), early, rx, cancel: cancel.clone() });
        let notify = self.notify.clone();
        std::thread::spawn(move || {
            let Some((advice, ms)) = solve_now(pool.as_deref(), &x, rollouts, &cancel) else { return };
            if tx.send(Solved { sig, x, advice, ms, read_at: at, rounds_known, rollouts, early, moves }).is_ok()
                && let Some(n) = notify
            {
                n();
            }
        });
    }

    /// The pool for solving: core count minus one, below normal priority (built only once).
    fn pool(&mut self) -> Option<Arc<rayon::ThreadPool>> {
        if self.pool.is_none() {
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1).max(1));
            self.pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("vorax-solve-{i}"))
                .start_handler(|_| crate::solver::solve::set_priority(BELOW_NORMAL))
                .build()
                .ok()
                .map(Arc::new);
        }
        self.pool.clone()
    }

    /// A solve finished: shown only if the board is still the same one.
    /// For results of an early solve (computed on the first read) and of solving while ticking: wait for a steady read (the same signature twice in a row) and for
    /// the value to stop ticking, then check once with that read and show; if that point has not come, keep it.
    fn solved(&mut self, s: Solved) {
        // Results that arrive after leaving the board (result screen, intro page) are not collected.
        if !self.active() || self.mode != Mode::Board {
            return;
        }
        if s.early {
            // Steady, ticking stopped, gate open, and the same signature.
            let ready = self.gate.is_none() && self.stable.as_ref().is_some_and(|st| st.sig == s.sig && !st.reading.ultimate_pending);
            let why = if s.moves != self.hand_moves() {
                // A confirm, reroll or page change happened since (the log takes priority over the screen): not the same hand.
                Some("moves")
            } else if !ready {
                // Not steady yet or still ticking: wait for that read (choice) and show then; if the signature changed it is dropped in handle.
                self.early = Some(s);
                return;
            } else {
                // Recompute the solver input from the steady, stopped-ticking read (only now is the final activity checked against the tank sums); show it only if identical.
                let Some(st) = &self.stable else { return };
                match self.screen_state(&st.reading) {
                    Ok((x, _)) if same_state(&x, &s.x) => None,
                    Ok(_) => Some("state"),
                    Err(_) => Some("unreadable"),
                }
            };
            if let Some(why) = why {
                self.log_event("vorax_early_dropped", self.clock, json!({ "why": why, "readAt": timestamp(at_time(s.read_at)), "solveMs": s.ms }));
                // Do not show. If the board is already steady and ticking stopped, redo the normal flow (say unreadable if it cannot be read, re-solve if the state differs);
                // if that point has not come, a later steady read will solve as usual.
                if ready {
                    self.choice(self.clock);
                }
                return;
            }
        }
        let Some(st) = &self.stable else { return };
        if st.sig != s.sig || self.gate.is_some() {
            return;
        }
        let (round, hand) = (st.reading.round, hand(&st.reading));
        self.view.solve_ms = Some(s.ms);
        self.log_event(
            "vorax_advice",
            self.clock,
            json!({ "readAt": timestamp(at_time(s.read_at)), "state": &s.x, "advice": &s.advice, "rollouts": s.rollouts, "solveMs": s.ms, "roundsKnown": s.rounds_known, "early": s.early }),
        );
        let rerolls = st.reading.rerolls;
        // the evidence of the read this advice was solved on, once a hand
        if self.game.evidence_sig.is_none()
            && let Some(shot) = self.game.steady_shot.clone().filter(|shot| shot.sig == s.sig)
        {
            self.save_evidence(&shot, "advised");
            self.game.evidence_sig = Some(s.sig.clone());
        }
        self.advised = Some(Advised { sig: s.sig, round, hand, rerolls, moves: self.hand_moves(), x: s.x, advice: s.advice });
        self.show_advice();
        self.bump();
    }

    /// Show the current advice and boxes (when the advised card is raised, the card is not boxed).
    fn show_advice(&mut self) {
        let (Some(a), Some(st)) = (&self.advised, &self.stable) else { return };
        let rows = advice_rows(&a.x, &a.advice);
        let (w, h) = st.size;
        let boxes: Vec<MarkBox> = if self.gate.is_some() {
            vec![]
        } else {
            targets(&st.reading, &a.advice)
                .into_iter()
                .map(|tg| MarkBox {
                    what: match tg.what {
                        What::Card(k) => format!("card {}", k + 1),
                        What::Tank(i) => format!("tank {}", i + 1),
                        What::Reroll => "reroll".into(),
                    },
                    // A card or reroll box is on the interface, drawn at the scale the frame was read at.
                    at: read::to_frame(w, h, crate::vorax::ui_scale::ui_box((tg.x, tg.y, tg.half_w, tg.half_h), st.ui)),
                    style: if matches!(tg.what, What::Tank(_)) { BoxStyle::Loud } else { BoxStyle::Plain },
                })
                .collect()
        };
        (self.view.advice, self.view.stale, self.view.boxes) = (Some(rows), false, boxes);
        self.set_status(Status::Advised, vec![t("建議", "Advice").into()]);
    }

    fn set_status(&mut self, status: Status, lines: Vec<String>) {
        (self.view.status, self.view.lines) = (status, lines);
    }

    fn bump(&mut self) {
        self.view.version += 1;
    }

    fn log_event(&self, kind: &str, at: i64, fields: Value) {
        if let Some(log) = &self.log {
            log.event(kind, at_time(at), fields);
        }
    }

    /// Saves the evidence image ([`read::evidence`]) of `shot` as a lossless PNG and its `vorax_lossless` row with
    /// `role` (see [`LOSSLESS`]); nothing with observe mode off, no log, or too little free space.
    fn save_evidence(&mut self, shot: &Shot, role: &str) {
        if !self.config.observe || !self.learn.jpegs_allowed() {
            return;
        }
        let Some(log) = &mut self.log else { return };
        let reference = read::to_reference(&shot.frame);
        let original = match shot.ui.filter(|&s| !crate::vorax::ui_scale::is_original(s)) {
            Some(s) => crate::vorax::ui_scale::unscale(&reference, s, None),
            None => reference,
        };
        let name = format!("evidence-{:04}.png", log.number());
        log.png_as(&name, read::evidence(&original));
        self.log_event("vorax_lossless", self.clock, json!({ "screen": "hand", "role": role, "file": name, "readAt": timestamp(at_time(shot.at)), "signature": shot.sig, "round": shot.round }));
    }

    /// Whether this game still wants its lossless frame of `kind` ([`LOSSLESS`]): the general observe mode on, a log
    /// to write it in, and none of the kind saved yet.
    fn wants_lossless(&self, kind: &str) -> bool {
        self.config.observe && self.log.is_some() && !self.game.lossless.contains(&kind)
    }

    /// The game's interface scale relative to the original setting the reader was measured at
    /// ([`crate::vorax::ui_scale::from_micro`]), from the last [`Cue::UiScale`]; before one came, the scale last
    /// measured on this game's frames (`Game::measured_ui`, cleared with the game), else None.
    fn ui(&self) -> Option<f64> {
        self.ui_scale.map(crate::vorax::ui_scale::from_micro).or(self.game.measured_ui)
    }

    /// Write the UI scale ([`Cue::UiScale`]) as one `ui_scale` row, in Vorax mode only: when it changed since the
    /// last row, or always with `again` (entering the mode or a new game's folder). Nothing while it is unknown.
    fn log_ui_scale(&mut self, at: i64, again: bool) {
        let Some(micro) = self.ui_scale.filter(|_| self.active()) else { return };
        if !again && self.ui_scale_logged == Some(micro) {
            return;
        }
        self.ui_scale_logged = Some(micro);
        self.log_event("ui_scale", at, json!({ "scale": f64::from(micro) / 1e6, "micro": micro, "page": super::gamelog::HUD_PAGE }));
    }

    /// The instrument rerolls a game starts with, when this game's count has not been seen: learned from the start of
    /// recent games ([`OPENINGS_FILE`]), else [`INSTRUMENT_REROLLS`].
    fn opening_instrument_rerolls(&self) -> i32 {
        opening_default(&self.openings).unwrap_or(INSTRUMENT_REROLLS)
    }

    /// Remembers `n` as the opening instrument reroll count of this game (the newest [`OPENINGS_KEPT`] are kept,
    /// written to [`OPENINGS_FILE`] in the log root).
    fn remember_opening(&mut self, n: i32) {
        self.openings.push(n);
        let extra = self.openings.len().saturating_sub(OPENINGS_KEPT);
        self.openings.drain(..extra);
        if let Some(root) = &self.config.log_root {
            let _ = std::fs::create_dir_all(root);
            let _ = std::fs::write(root.join(OPENINGS_FILE), json!({ "instrument": self.openings }).to_string());
        }
    }

    /// True once: the UI calls `Watcher::read_next()` to read one more sample to confirm.
    pub fn take_read_next(&mut self) -> bool {
        std::mem::take(&mut self.read_next)
    }

    /// Once: how long until a frame is wanted (the panel is read once a second in the endgame battle). The UI hands it to `Watcher::read_in(after)`,
    /// and repaints at `after + READ_IN_LATE_MS` (when auto read is off, [`Self::poll`] asks for the next one).
    pub fn wants_read_in(&mut self) -> Option<Duration> {
        self.read_in.take()
    }

    /// After each read: hand it to `Watcher::report(news, off_board)`.
    pub fn take_news(&mut self) -> Option<(bool, bool)> {
        self.news.take()
    }

    /// The capture window changed: write a `window` row (also written at the start of each game's folder).
    pub fn log_window(&mut self, window: &str) {
        if let Some(log) = &mut self.log {
            log.window(json!({ "window": window, "ocr": crate::reader::ocr::language() }));
        }
    }

    /// The window language was switched, so OCR is made again for it: the window row is written again with the new
    /// `ocr` (as Tartarus's [`super::session::Session::log_language`]; nothing before the first window row), and the
    /// reader reads the new client from the next frame on, mid game too ([`VoraxSession::point_reader`]).
    pub fn log_language(&mut self) {
        if let Some(log) = &mut self.log {
            log.set_ocr(crate::reader::ocr::language());
        }
        self.point_reader();
    }

    /// The boxes drawn by the window changed: write one `vorax_view` row; `fields` is a JSON object, and `best`,
    /// `note`, `solving`, `screen` (the screen kind), `round` and `boxes` (the number of boxes to draw) are added.
    pub fn log_view(&mut self, fields: Value, at_ms: i64) {
        let mut line = json!({
            "best": self.view.advice.as_ref().map(|a| a.headline.clone()),
            // The advice's note as the window shows it (already in the client's language, see vorax::advice_rows),
            // e.g. the "cannot simulate" line of a chain card; null when the advice has none.
            "note": self.view.advice.as_ref().and_then(|a| a.note.clone()),
            "solving": self.view.status == Status::Solving,
            // The screen kind; not called kind, so it does not overwrite this row's kind (vorax_view).
            "screen": self.view.kind,
            "round": self.view.round,
            // The number of boxes to draw at this time: `marks` (the ones actually drawn) being fewer means the game is not in the foreground or the window position cannot be read.
            "boxes": self.view.boxes.len(),
        });
        if let (Value::Object(m), Value::Object(f)) = (&mut line, fields) {
            m.extend(f);
        }
        self.log_event("vorax_view", at_ms, line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const MAIN_RUN: &str = "[2026.11.03-06.25.46:278][268]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlayMain , PageRunState = Run ";
    const REWARD_DESTORY: &str = "[2026.11.03-06.25.38:193][235]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlayReward , PageRunState = Destory ";

    fn row(name: &str) -> String {
        format!("[2026.11.03-06.25.57:685][892]LogDataTable: Warning: UDataTable::FindRow : '{name}' requested row '{name}' not in DataTable '/Game/Audio/BluePrint/CD/AudioCDTable.AudioCDTable'.")
    }

    fn button(name: &str) -> String {
        format!("[2026.11.03-06.26.14:853][968]LogDataTable: Warning: UDataTable::FindRow : 'AudioMgr' requested row '{name}' not in DataTable '/Game/Audio/BluePrint/AudioAndUIDataTable.AudioAndUIDataTable'.")
    }

    #[test]
    fn cues_of_log_lines() {
        // Levels: entering the endgame battle map starts the battle, entering any other level (town 110, a map) means leaving it.
        assert_eq!(cue("[2026.11.03-06.27.33:493][812]TLLua: Display: [Game] LevelMgr:_EnterLevelSuccess() 230043"), Some(Cue::BattleEntered));
        assert_eq!(cue("[2026.11.03-06.27.33:493][812]TLLua: Display: [Game] LevelMgr:_EnterLevelSuccess() "), None);
        assert_eq!(cue("[2026.11.03-06.28.49:834][497]TLLua: Display: [Game] LevelMgr:_EnterLevelSuccess() 110"), Some(Cue::LevelLeft));
        assert_eq!(cue(MAIN_RUN), Some(Cue::Page(Page::Main, PageState::Run)));
        assert_eq!(cue(REWARD_DESTORY), Some(Cue::Page(Page::Reward, PageState::Destory)));
        assert_eq!(
            cue("[2026.11.03-06.25.42:003][660]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlay , PageRunState = Run "),
            Some(Cue::Page(Page::Intro, PageState::Run))
        );
        // Another kind of line for the same page (PageStack, the `S13GamePlayMain Run` line) does not count.
        assert_eq!(cue("[2026.11.03-06.25.46:278][268]TLLua: Display: [Game] S13GamePlayMain Run"), None);
        assert_eq!(
            cue("[2026.11.03-06.25.46:279][268]TLLua: Display: [Game] PageStack@                  [3] = PageName = S13GamePlayMain, PageType = FuncPage, PageAction = HideLastFuncPage"),
            None
        );
        assert_eq!(cue("[2026.11.03-06.25.57:706][895]TLLua: Display: [Game] PushMgr:RecvPushCombineMsg ProtoId = 5619, SynId = 16 "), Some(Cue::Synced));
        assert_eq!(cue("[2026.11.03-06.27.33:401][811]TLLua: Display: [Game] PushMgr:RecvPushCombineMsg ProtoId = 5621, SynId = 36 "), Some(Cue::Over));
        // RefreshBtn and RefreshBtn2 can be told apart.
        assert_eq!(cue(&button("S13GamePlayMain_C_RefreshBtn2")), Some(Cue::Reroll { instrument: true }));
        assert_eq!(cue(&button("S13GamePlayMain_C_RefreshBtn")), Some(Cue::Reroll { instrument: false }));
        assert_eq!(cue(&button("S13GamePlayMain_C_ComfirmBtn")), Some(Cue::Confirm));
        assert_eq!(cue(&button("S13GamePlayCard_C_SelectBtn")), Some(Cue::CardClick));
        assert_eq!(cue(&button("S13GamePlayMonsterItem_C_SelectBtn")), Some(Cue::TankClick { tank: None }));
        assert_eq!((Cue::TankClick { tank: Some(3) }.name(), Cue::Reroll { instrument: true }.name()), ("TankClick".to_string(), "Reroll { instrument: true }".to_string()));
        assert_eq!(cue(&button("S13GamePlayRelic_C_RelicBtn")), Some(Cue::Relic));
        assert_eq!(cue(&button("S13GamePlayReward_C_FightBtn")), Some(Cue::Fight));
        assert_eq!(cue(&button("S13GamePlayReward_C_GiveUpBtn")), Some(Cue::GiveUp));
        assert_eq!(cue(&button("S13GamePlay_C_ComfirmBtn2")), None);
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Agentia_Use")), Some(Cue::PotionUsed));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Selected_Confirm")), Some(Cue::Picked));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Selected")), None);
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Precept_Surgery")), Some(Cue::Protocol));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Figure_Refresh")), Some(Cue::Dealt));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Selected_Shuffle")), Some(Cue::Shuffle));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Tool_Surgery")), Some(Cue::Tool));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Monster_DisAppear")), Some(Cue::Gone));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Monster_change")), Some(Cue::Changed));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Show_Surgery")), Some(Cue::Reveal));
        assert_eq!(cue(&row("Play_UI_S13_KYZ_Card_Figure_Add_Small")), None);
        // AudioMgr@PlayAudio lines, S16 lines, and other lines with S13 in their names do not count.
        assert_eq!(cue("[2026.11.03-06.26.02:310][418]TLLua: Display: [Game] AudioMgr@PlayAudio  AudioEnum  == S13_KYZ_Card_Monster_DisAppear"), None);
        assert_eq!(cue("[2026.11.03-06.25.12:086][186]TLShipping: Display: [Game] AssetLoad@ AsyncLoadRequest! PathStr = UI_MiniMap_S13tufu' LoadingNum = 315"), None);
        assert_eq!(cue("[2026.10.28-15.24.42:062][871]LogTemp: ShowMgr@DispatchPageRunChange PageName = S16BoardMain , PageRunState = Run "), None);
        assert!(Cue::Dealt.deals() && Cue::Shuffle.deals() && !Cue::Synced.deals());
        assert!(Cue::Tool.resolves() && Cue::Gone.resolves() && Cue::Changed.resolves() && !Cue::Dealt.resolves());
    }

    /// The number of cues per line in two real game logs matches what grep counts (the logs are not under version control; skip if missing).
    #[test]
    fn cues_counted_in_the_logs() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("trainingdata/vorax");
        for (file, synced, over, dealt, potion, picked, protocol, rerolls, instrument_rerolls) in
            [("UE_game-20261103-vorax.log", 19, 2, 26, 10, 6, 3, 2, 2), ("UE_game-20261103-vorax2.log", 20, 2, 24, 10, 6, 3, 3, 2)]
        {
            let Ok(text) = std::fs::read(dir.join(file)) else {
                println!("skipped: {file} is missing");
                continue;
            };
            let cues: Vec<Cue> = String::from_utf8_lossy(&text).lines().filter_map(cue).collect();
            let n = |c: Cue| cues.iter().filter(|&&x| x == c).count();
            let rerolled = cues.iter().filter(|c| matches!(c, Cue::Reroll { .. })).count();
            assert_eq!(
                (n(Cue::Synced), n(Cue::Over), n(Cue::Dealt), n(Cue::PotionUsed), n(Cue::Picked), n(Cue::Protocol), rerolled, n(Cue::Reroll { instrument: true })),
                (synced, over, dealt, potion, picked, protocol, rerolls, instrument_rerolls),
                "{file}"
            );
            assert_eq!(n(Cue::Page(Page::Main, PageState::Run)), 1, "{file}");
        }
    }

    #[test]
    fn a_vorax_under_way_is_found_in_the_log() {
        let log = |lines: &[&str]| lines.iter().map(|l| format!("{l}\n")).collect::<String>().into_bytes();
        let intro = "[2026.11.03-06.25.42:003][660]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlay , PageRunState = Run ";
        let intro_hide = "[2026.11.03-06.25.46:263][268]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlay , PageRunState = Hide ";
        let other = "[2026.11.03-06.25.47:000][1]TLGame: Display: [Game] something else";
        let found = |bytes: Vec<u8>| {
            let end = bytes.len() as u64;
            under_way(&mut Cursor::new(bytes), end)
        };
        assert_eq!(found(log(&[intro, intro_hide, MAIN_RUN, other])), Some(Cue::Page(Page::Main, PageState::Run)));
        assert_eq!(found(log(&[intro, MAIN_RUN, REWARD_DESTORY, other])), None);
        // Program starts midway through the result screen: do not enter.
        let reward_run = "[2026.11.03-06.27.32:664][ 61]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlayReward , PageRunState = Run ";
        assert_eq!(found(log(&[intro, MAIN_RUN, reward_run, other])), None);
        assert_eq!(found(log(&[intro])), None);
        assert_eq!(found(log(&[other])), None);
        // Looking back sees only up to `end`.
        let bytes = log(&[MAIN_RUN, REWARD_DESTORY]);
        let end = (MAIN_RUN.len() + 1) as u64;
        assert_eq!(under_way(&mut Cursor::new(bytes), end), Some(Cue::Page(Page::Main, PageState::Run)));
    }

    fn session() -> VoraxSession {
        let mut s = VoraxSession::new(Config { enabled: true, rollouts: 64, learn: false, strips: false, log_root: None, inline: true, min_free_bytes: 0, observe: false, give_up_hint: false });
        s.require_zh = false;
        s
    }

    fn page(p: Page, st: PageState) -> Cue {
        Cue::Page(p, st)
    }

    #[test]
    fn the_mode_follows_the_pages() {
        use PageState::{Destory, Hide, Run};
        let mut s = session();
        // A lone result page while Off (from the previous game, or a reward page opened elsewhere) does not enter the mode.
        assert_eq!(s.note_cue(page(Page::Reward, Run), Some(1)), ModeChange::None);
        assert!(!s.active());
        assert_eq!(s.note_cue(Cue::GiveUp, Some(2)), ModeChange::None);
        assert_eq!(s.note_cue(page(Page::Reward, Destory), Some(3)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Off);
        // Other cues outside the mode do nothing.
        for c in [Cue::Synced, Cue::Dealt, Cue::Over, Cue::Fight, page(Page::Main, Hide), page(Page::Intro, Hide)] {
            assert_eq!(s.note_cue(c, Some(4)), ModeChange::None);
            assert_eq!(s.mode(), Mode::Off);
        }
        // One game: intro page, board, result, start surgery.
        assert_eq!(s.note_cue(page(Page::Intro, Run), Some(10)), ModeChange::Entered);
        assert_eq!(s.note_cue(page(Page::Intro, Hide), Some(11)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Intro);
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(12)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Board);
        assert_eq!(s.note_cue(Cue::Over, Some(13)), ModeChange::None);
        assert_eq!(s.note_cue(page(Page::Main, Hide), Some(14)), ModeChange::None);
        assert_eq!(s.note_cue(page(Page::Reward, Run), Some(15)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Result);
        // Start surgery: endgame battle (still in the mode); a page closing during the battle does not count as leaving; only leaving the battle map goes Off.
        assert_eq!(s.note_cue(Cue::Fight, Some(16)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Battle);
        for c in [Cue::Over, page(Page::Reward, Destory), page(Page::Main, Destory), page(Page::Intro, Destory)] {
            assert_eq!(s.note_cue(c, Some(17)), ModeChange::None);
            assert_eq!(s.mode(), Mode::Battle);
        }
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(18)), ModeChange::Left);
        for c in [Cue::Over, page(Page::Reward, Destory), page(Page::Main, Destory), page(Page::Intro, Destory), Cue::LevelLeft] {
            assert_eq!(s.note_cue(c, Some(19)), ModeChange::None);
        }
        assert!(!s.active());
        // Without seeing the intro page it is the board (program started midway); board Destory goes Off.
        assert_eq!(s.note_cue(page(Page::Main, Run), None), ModeChange::Entered);
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(20)), ModeChange::Left);
        // The intro page exits.
        assert_eq!(s.note_cue(page(Page::Intro, Run), Some(30)), ModeChange::Entered);
        assert_eq!(s.note_cue(page(Page::Intro, Destory), Some(31)), ModeChange::Left);
    }

    #[test]
    fn off_when_turned_off_or_not_zh() {
        let mut s = session();
        s.config.enabled = false;
        assert_eq!(s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1)), ModeChange::None);
        assert!(!s.active() && s.view().blocked.is_none());
        let mut s = session();
        (s.require_zh, s.test_zh) = (true, Some(false));
        // With the feature `en-vorax` (on by default since 1.1.0) the English client is read too (see an_english_client_is_read_with_en_vorax).
        if !cfg!(feature = "en-vorax") {
            assert_eq!(s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1)), ModeChange::None);
            assert!(!s.active() && s.view().blocked.is_some());
        }
    }

    /// Switching the window language mid game points the reader at the new client from the next frame on, without
    /// leaving and entering the mode again ([`VoraxSession::log_language`]).
    #[cfg(feature = "en-vorax")]
    #[test]
    fn a_language_switch_mid_game_points_the_reader_at_the_new_client() {
        let mut s = session();
        s.test_zh = Some(true);
        assert_eq!(s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1)), ModeChange::Entered);
        assert!(!crate::vorax::read::english());
        s.test_zh = Some(false);
        s.log_language();
        assert!(s.active() && crate::vorax::read::english());
        s.test_zh = Some(true);
        s.log_language();
        assert!(s.active() && !crate::vorax::read::english());
        crate::vorax::read::set_english_here(None);
    }

    /// [`VoraxSession::client_language`]: the test value when set; the OCR engine's tag is read as a game language
    /// (`crate::reader::ocr::game_lang`), another script's being none the reader knows.
    #[test]
    fn the_client_language_is_the_one_read() {
        use crate::lang::Lang;
        use crate::reader::ocr::game_lang;
        let mut s = session();
        s.test_zh = Some(false);
        assert_eq!(s.client_language(), Some(Lang::En));
        s.test_zh = Some(true);
        assert_eq!(s.client_language(), Some(Lang::ZhTw));
        assert_eq!([game_lang("en-US"), game_lang("en-GB"), game_lang("zh-Hant-TW"), game_lang("zh-TW"), game_lang("zh-HK")], [Some(Lang::En), Some(Lang::En), Some(Lang::ZhTw), Some(Lang::ZhTw), Some(Lang::ZhTw)]);
        assert_eq!([game_lang("ja-JP"), game_lang("zh-Hans-CN"), game_lang("ru")], [None, None, None]);
    }

    #[test]
    fn a_tartarus_signal_ends_the_mode() {
        let mut s = session();
        // Signals outside the mode, or found by looking back, do not count.
        assert_eq!(s.note_tartarus("Day", Some(1)), ModeChange::None);
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(2));
        assert_eq!(s.note_tartarus("Day", None), ModeChange::None);
        assert!(s.active());
        assert_eq!(s.note_tartarus("Day", Some(3)), ModeChange::Left);
        assert!(!s.active());
        assert_eq!(s.view().mode, Mode::Off);
    }

    /// vorax_view.rs hands over the Day and Treasure events received in the mode under these two names.
    #[test]
    fn a_treasure_or_a_day_ends_the_mode() {
        for what in ["treasure", "day"] {
            let mut s = session();
            s.note_cue(Cue::Page(Page::Intro, PageState::Run), Some(1));
            s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(2));
            assert_eq!(s.note_tartarus(what, Some(3)), ModeChange::Left, "{what}");
            assert_eq!((s.mode(), s.active()), (Mode::Off, false), "{what}");
            // Already Off: receiving it again does nothing.
            assert_eq!(s.note_tartarus(what, Some(4)), ModeChange::None, "{what}");
        }
    }

    #[test]
    fn forced_off_twice_is_safe() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-force-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        assert_eq!(s.force_off("watcher gone"), ModeChange::None);
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        s.note_cue(Cue::Synced, Some(2_000));
        assert!(s.gate_closed(2_100));
        assert_eq!(s.force_off("watcher gone"), ModeChange::Left);
        assert!(!s.active() && !s.gate_closed(2_100) && !s.busy() && s.view().boxes.is_empty() && !s.view().gated);
        assert_eq!(s.force_off("watcher gone"), ModeChange::None);
        s.log_game("Vorax(Synced)", Some(2_000), UNIX_EPOCH + Duration::from_millis(2_050));
        s.log().unwrap().wait();
        let text = std::fs::read_to_string(s.log().unwrap().dir().join("log.jsonl")).unwrap();
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["vorax_mode", "vorax_mode", "game"]);
        assert_eq!((&rows[1]["cue"], &rows[1]["to"]), (&json!("watcher gone"), &json!("off")));
        assert_eq!((&rows[2]["signal"], &rows[2]["late_ms"]), (&json!("Vorax(Synced)"), &json!(50)));
        // Log turned off: nothing is written afterwards, and nothing goes to the closed RunLog.
        s.configure(Config { log_root: None, ..s.config.clone() });
        assert!(s.log().is_none());
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(3_000));
        assert!(s.active());
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A synthetic potion board (round 1, 3 rerolls left): `ultimate` is the final activity on the left panel, `pending`
    /// is whether the green +N at the upper right is still there. The tank sum is 600. `first` replaces the first tank's quantity (to change the board).
    fn potion_board(pending: bool, ultimate: Option<i64>, first: i64) -> Reading {
        use vorax::{Archetype, Rarity};
        let tank = |a: Archetype, r: Rarity, act: i64, q: i64| read::Tank { archetype: Some(a), rarity: Some(r), activity: Some(act), quantity: Some(q), total: Some(act * q), ..read::Tank::default() };
        let empty = || read::Tank { empty: true, ..read::Tank::default() };
        let card = |en: &str, x: i32| read::Card {
            category: crate::vorax::names::Category::Potion,
            index: None,
            id: None,
            en: Some(en.to_string()),
            zh: None,
            how: None,
            raw_name: String::new(),
            raw_type: String::new(),
            raw_effect: String::new(),
            why: None,
            x,
            dy: 0,
            raised: false,
        };
        let tanks = vec![tank(Archetype::Pest, Rarity::Rare, 15, first), tank(Archetype::Agito, Rarity::Boss, 300, 1), tank(Archetype::Pest, Rarity::Magic, 5, 24), empty(), empty(), empty()];
        let tank_sum = Some(tanks.iter().filter_map(|t| t.total).sum());
        Reading {
            kind: Kind::Potion,
            round: Some(1),
            ultimate,
            ultimate_pending: pending,
            panel_covered: false,
            next_reward: Some(2000),
            rerolls: Some(3),
            tanks,
            held: vec![],
            cards: vec![card("Osteolytic Ointment", 1205), card("Viviparous Hormone", 1417), card("Digestive Enzyme Solution", 1629)],
            tank_sum,
            problems: vec![],
            tooltip: String::new(),
        }
    }

    /// Hand one read directly to `handle` (without going through OCR).
    fn feed(s: &mut VoraxSession, r: Reading, at: i64) {
        s.clock = s.clock.max(at);
        s.handle(Done { reading: r, frame: Rgba { w: 16, h: 9, data: vec![0; 16 * 9 * 4] }, at, trigger: "auto".into(), read_ms: 0, s: None, icons: None, client: Value::Null });
    }

    /// A choice screen read with a problem (here the rerolls unread) asks for another frame after 250 ms, 500 ms,
    /// then 1 s each, at most RETRY_READS times while it reads the same; a complete reading asks for none, and a
    /// changed reading starts over (2026-11-08: a hand without its rerolls had no advice until the mouse moved).
    #[test]
    fn a_choice_screen_read_with_a_problem_is_read_again() {
        let mut s = session();
        s.note_cue(page(Page::Intro, PageState::Run), Some(10));
        s.note_cue(page(Page::Main, PageState::Run), Some(12));
        let unread = || Reading { rerolls: None, problems: vec!["重擲次數讀不到".to_string()], ..potion_board(false, Some(108), 36) };
        let mut waits = Vec::new();
        for at in 20..30 {
            feed(&mut s, unread(), at);
            waits.push(s.wants_read_in().map(|d| d.as_millis()));
        }
        assert_eq!(waits[..7], [Some(250), Some(500), Some(1000), Some(1000), Some(1000), Some(1000), None], "{waits:?}");
        feed(&mut s, potion_board(false, Some(108), 36), 40);
        assert_eq!(s.wants_read_in(), None, "complete: no more reads");
        feed(&mut s, unread(), 41);
        assert_eq!(s.wants_read_in(), Some(Duration::from_millis(250)), "a new problem starts over");
    }

    /// Right after the surgery page opens, a frame still showing it come in (`other`) asks for the next frame at once;
    /// later an `other` frame waits for auto read as before.
    #[test]
    fn the_page_coming_in_is_read_again_at_once() {
        let mut s = session();
        s.note_cue(page(Page::Intro, PageState::Run), Some(10));
        s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
        let other = || Reading { kind: Kind::Other, round: None, cards: vec![], tanks: vec![], ..potion_board(false, None, 36) };
        feed(&mut s, other(), 1_090);
        assert_eq!(s.wants_read_in(), Some(Duration::from_millis(BOARD_EARLY_EVERY_MS)));
        feed(&mut s, other(), 1_000 + BOARD_EARLY_MS + 10);
        assert_eq!(s.wants_read_in(), None, "too late: auto read as before");
    }

    /// While the log gives no interface scale, the scale a frame was measured and read at is used for the next
    /// frames, until a frame reads as the original setting; a logged scale always wins and is not overwritten.
    #[test]
    fn a_measured_scale_is_kept_until_the_log_gives_one() {
        let mut s = session();
        s.note_cue(page(Page::Intro, PageState::Run), Some(10));
        s.note_cue(page(Page::Main, PageState::Run), Some(12));
        let read_at = |s: &mut VoraxSession, scale: Option<f64>, at: i64| {
            s.clock = s.clock.max(at);
            s.handle(Done { reading: potion_board(false, Some(108), 36), frame: Rgba { w: 16, h: 9, data: vec![0; 16 * 9 * 4] }, at, trigger: "auto".into(), read_ms: 0, s: scale, icons: None, client: Value::Null });
        };
        assert_eq!(s.ui(), None);
        read_at(&mut s, Some(1.26), 20);
        assert_eq!(s.ui(), Some(1.26));
        read_at(&mut s, None, 21);
        assert_eq!(s.ui(), None);
        read_at(&mut s, Some(1.5), 22);
        s.note_cue(Cue::UiScale { micro: 841_017 }, Some(23));
        let logged = crate::vorax::ui_scale::from_micro(841_017);
        assert_eq!(s.ui(), Some(logged));
        read_at(&mut s, Some(1.5), 24);
        assert_eq!(s.ui(), Some(logged));
    }

    /// A session for tests that write a log, and the rows read back from the log.
    fn logged(tag: &str) -> (VoraxSession, PathBuf) {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-early-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        (s, root)
    }

    /// [`logged`] on the Traditional Chinese client whatever this PC's OCR language: tests whose tooltips are the
    /// Traditional Chinese names (the hovered card matches the client's printed names, `vorax_learn::card_printed`).
    fn logged_zh(tag: &str) -> (VoraxSession, PathBuf) {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-early-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.test_zh = Some(true);
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        (s, root)
    }

    fn rows_of(s: VoraxSession, root: &std::path::Path) -> Vec<Value> {
        s.log().unwrap().wait();
        let text = std::fs::read_to_string(s.log().unwrap().dir().join("log.jsonl")).unwrap();
        drop(s);
        let _ = std::fs::remove_dir_all(root);
        text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    /// The rows of every log folder under the root folder (in creation order), after which the root folder is deleted.
    fn all_rows(s: VoraxSession, root: &std::path::Path) -> Vec<Vec<Value>> {
        drop(s);
        let dirs: Vec<PathBuf> = std::fs::read_dir(root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("log.jsonl").is_file()).collect();
        let mut rows: Vec<Vec<Value>> = dirs
            .iter()
            .map(|d| std::fs::read_to_string(d.join("log.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect())
            .collect();
        // When folder names fall in the same second a `-2` is added, so the name order is not necessarily the creation order: sort by the first row's time.
        rows.sort_by_key(|r: &Vec<Value>| r.first().map(|v| v["at"].as_str().unwrap_or("").to_string()));
        let _ = std::fs::remove_dir_all(root);
        rows
    }

    fn kinds<'a>(rows: &'a [Value], kind: &str) -> Vec<&'a Value> {
        rows.iter().filter(|r| r["kind"] == kind).collect()
    }

    /// The steady board of round `round` (read twice).
    fn steady_round(s: &mut VoraxSession, round: i32, at: i64) {
        steady_held(s, round, 0, at);
    }

    /// Round 13's board read twice with a final activity of `ultimate` (a result screen is checked against it,
    /// see `check_result`).
    fn steady_last_round(s: &mut VoraxSession, ultimate: i64, at: i64) {
        let r = || Reading { round: Some(13), ..potion_board(false, Some(ultimate), 12) };
        feed(s, r(), at);
        feed(s, r(), at + 60);
    }

    /// Same as above, with `icons` held icons at the upper right (recognized Incubation Sacs).
    fn steady_held(s: &mut VoraxSession, round: i32, icons: usize, at: i64) {
        let held = vec![read::Held { id: Some(2), en: None, zh: None, opening: false, score: 1.0 }; icons];
        let r = || Reading { round: Some(round), held: held.clone(), ..potion_board(false, Some(600), 12) };
        feed(s, r(), at);
        feed(s, r(), at + 60);
    }

    /// The endgame battle's give-up hint: with the setting on, every reward tier taken (next tier 999.9M) on two reads
    /// in a row boxes the Give Up button, at the frame's size and interface scale; a read of another next tier takes it
    /// down, one with the next tier unread leaves it; with the setting off, or leaving the battle, there is none.
    #[test]
    fn the_give_up_button_is_boxed_once_every_tier_is_taken() {
        let (mut s, _root) = logged("give-up");
        s.config.give_up_hint = true;
        s.note_cue(page(Page::Main, PageState::Hide), Some(2_000));
        s.note_cue(Cue::Fight, Some(3_000));
        assert_eq!(s.mode(), Mode::Battle);
        let panel = |next: Option<f64>| read::Battle { wave: Some((3, 3)), activity: Some(1_100_000.0), bonus: Some(496_800.0), next, percent: Some(15), raw: String::new() };
        // a panel crop with its Give Up button lit (big enough at the scales below), and one with the button gone
        let crop = || Rgba { w: 600, h: 520, data: vec![110; 600 * 520 * 4] };
        let won = || Rgba { w: 600, h: 520, data: vec![35; 600 * 520 * 4] };
        let all = Some(999_900_000.0);
        s.battle.frame = Some(((2560, 1440), None));
        s.battle_read(panel(Some(854_000.0)), crop(), 4_000);
        assert!(s.view.give_up.is_none());
        s.battle_read(panel(all), crop(), 5_000);
        assert!(s.view.give_up.is_none(), "one read at the cap is not enough");
        s.battle_read(panel(all), crop(), 6_000);
        assert_eq!(s.view.give_up.as_ref().map(|b| b.at), Some(read::BATTLE_GIVE_UP), "at 1440p and the original scale");
        s.battle_read(panel(None), crop(), 7_000);
        assert!(s.view.give_up.is_some(), "the next tier unread changes nothing");
        // 1080p, and the interface at 1.5 (the button scales about the left edge's middle, as the panel)
        s.battle.frame = Some(((1920, 1080), None));
        s.battle_read(panel(all), crop(), 8_000);
        let (x, y, hw, hh) = s.view.give_up.as_ref().unwrap().at;
        assert!((x - 193.5 * 0.75).abs() < 0.01 && (y - 831.0 * 0.75).abs() < 0.01 && (hw - 77.0 * 0.75).abs() < 0.01 && (hh - 22.0 * 0.75).abs() < 0.01);
        s.battle.frame = Some(((2560, 1440), Some(1.5)));
        s.battle_read(panel(all), crop(), 9_000);
        let (x, y, hw, _) = s.view.give_up.as_ref().unwrap().at;
        assert!((x - 193.5 * 1.5).abs() < 0.01 && (y - (720.0 + 111.0 * 1.5)).abs() < 0.01 && (hw - 77.0 * 1.5).abs() < 0.01, "{x} {y} {hw}");
        s.battle_read(panel(all), won(), 9_500);
        assert!(s.view.give_up.is_none(), "the button gone (the battle won): no box on the empty place");
        s.battle_read(panel(all), crop(), 9_800);
        assert!(s.view.give_up.is_some(), "the cap still counted");
        s.battle_read(panel(Some(1_120_000.0)), crop(), 10_000);
        assert!(s.view.give_up.is_none(), "another next tier read: down");
        s.battle_read(panel(all), crop(), 11_000);
        s.battle_read(panel(all), crop(), 12_000);
        assert!(s.view.give_up.is_some());
        // the setting off: none
        s.config.give_up_hint = false;
        s.battle_read(panel(all), crop(), 13_000);
        assert!(s.view.give_up.is_none());
        s.config.give_up_hint = true;
        s.battle_read(panel(all), crop(), 14_000);
        assert!(s.view.give_up.is_some());
        // leaving the battle map
        s.note_cue(Cue::LevelLeft, Some(15_000));
        assert!(s.view.give_up.is_none());
    }

    /// With the give-up hint on, the panel is read faster close to the cap: activity plus bonus at 85% of the last
    /// tier, or at the cap and read fewer than GIVE_UP_READS times; far from it, once the hint is up, or with the
    /// setting off, a second apart as before.
    #[test]
    fn the_panel_is_read_faster_close_to_the_cap() {
        let (mut s, _root) = logged("give-up-near");
        s.config.give_up_hint = true;
        s.note_cue(page(Page::Main, PageState::Hide), Some(2_000));
        s.note_cue(Cue::Fight, Some(3_000));
        assert_eq!(s.mode(), Mode::Battle);
        s.battle.frame = Some(((2560, 1440), None));
        let panel = |activity: f64, bonus: f64, next: Option<f64>| read::Battle { wave: Some((3, 3)), activity: Some(activity), bonus: Some(bonus), next, percent: Some(15), raw: String::new() };
        let crop = || Rgba { w: 600, h: 520, data: vec![110; 600 * 520 * 4] };
        let all = Some(read::BATTLE_ALL);
        // a read started at `at` (battle_frame sets last_read) and done: the next one is due this long after it
        let after = |s: &mut VoraxSession, b: read::Battle, at: i64| {
            s.battle.last_read = at;
            s.battle.due = None;
            s.battle_read(b, crop(), at);
            s.battle.due.map(|d| d - at)
        };
        // far from the cap: a second
        assert_eq!(after(&mut s, panel(500_000.0, 200_000.0, Some(721_000.0)), 4_000), Some(BATTLE_EVERY_MS));
        // 84% of 1.12M: still a second; 85%: faster
        assert_eq!(after(&mut s, panel(700_000.0, 240_000.0, Some(1_120_000.0)), 5_000), Some(BATTLE_EVERY_MS));
        assert_eq!(after(&mut s, panel(700_000.0, 252_000.0, Some(1_120_000.0)), 6_000), Some(BATTLE_NEAR_EVERY_MS));
        // the next tier unread close to the cap: still faster
        assert_eq!(after(&mut s, panel(720_000.0, 260_000.0, None), 6_300), Some(BATTLE_NEAR_EVERY_MS));
        assert_eq!(s.battle_every(), BATTLE_NEAR_EVERY_MS);
        // the cap read once: faster until the second
        assert_eq!(after(&mut s, panel(800_000.0, 330_000.0, all), 6_600), Some(BATTLE_NEAR_EVERY_MS));
        assert!(s.view.give_up.is_none());
        // the second: the hint up, back to a second
        assert_eq!(after(&mut s, panel(810_000.0, 335_000.0, all), 6_900), Some(BATTLE_EVERY_MS));
        assert!(s.view.give_up.is_some());
        assert_eq!(after(&mut s, panel(820_000.0, 340_000.0, all), 7_900), Some(BATTLE_EVERY_MS));
        // the setting off: a second even close to the cap
        s.config.give_up_hint = false;
        assert_eq!(after(&mut s, panel(800_000.0, 330_000.0, Some(1_120_000.0)), 8_900), Some(BATTLE_EVERY_MS));
        assert_eq!(after(&mut s, panel(800_000.0, 330_000.0, all), 9_900), Some(BATTLE_EVERY_MS));
        // a read that cannot be read keeps the pace of the last readable one
        s.config.give_up_hint = true;
        assert_eq!(after(&mut s, panel(700_000.0, 260_000.0, Some(1_120_000.0)), 10_900), Some(BATTLE_NEAR_EVERY_MS));
        let unreadable = read::Battle { activity: None, ..panel(0.0, 0.0, None) };
        assert_eq!(after(&mut s, unreadable, 11_200), Some(BATTLE_NEAR_EVERY_MS));
    }

    /// The give-up hint's row keeps its time in "at", the box in "box", and gives the time from the first read at the
    /// cap (`sinceCapMs`) and from the frame to the read done (`readMs`); a read of another tier starts the count over.
    #[test]
    fn the_give_up_row_has_its_time_and_the_delay() {
        let (mut s, root) = logged("give-up-row");
        s.config.give_up_hint = true;
        s.note_cue(page(Page::Main, PageState::Hide), Some(2_000));
        s.note_cue(Cue::Fight, Some(3_000));
        s.battle.frame = Some(((2560, 1440), None));
        let panel = |next: Option<f64>| read::Battle { wave: Some((3, 3)), activity: Some(900_000.0), bonus: Some(300_000.0), next, percent: Some(15), raw: String::new() };
        let crop = || Rgba { w: 600, h: 520, data: vec![110; 600 * 520 * 4] };
        let all = Some(read::BATTLE_ALL);
        s.battle_read(panel(all), crop(), 4_000);
        // a misread of the next tier: the count starts over
        s.battle_read(panel(Some(1_120_000.0)), crop(), 4_300);
        s.battle_read(panel(all), crop(), 4_600);
        // the read of the frame at 4,900 done at 5,020
        s.clock = 5_020;
        s.battle_read(panel(all), crop(), 4_900);
        assert!(s.view.give_up.is_some());
        let rows = rows_of(s, &root);
        let hint = kinds(&rows, "vorax_give_up_hint");
        assert_eq!(hint.len(), 1);
        assert_eq!(hint[0]["at"].as_str(), Some(timestamp(at_time(4_900)).as_str()), "the row's time");
        let b = &hint[0]["box"];
        assert_eq!(b.as_array().map(|a| a.len()), Some(4), "{b}");
        assert_eq!(b[0].as_f64().map(|x| x as f32), Some(read::BATTLE_GIVE_UP.0));
        assert_eq!(hint[0]["sinceCapMs"], 300, "from the read at 4,600, not 4,000");
        assert_eq!(hint[0]["readMs"], 120);
        assert_eq!((hint[0]["activity"].as_f64(), hint[0]["bonus"].as_f64(), hint[0]["wave"].as_u64()), (Some(900_000.0), Some(300_000.0), Some(3)));
    }

    /// Endgame battle: each panel read updates the status line, reading a later wave writes a row at once, other changes are written 5 seconds apart, one crop is
    /// saved per wave, and leaving the battle map writes the ending (including the bonus share of activity and the board's final activity) and saves the last crop.
    #[test]
    fn a_battle_logs_its_panel_and_ends_on_leaving_the_map() {
        let (mut s, root) = logged("battle");
        // The board's final activity, read after the last pick (the last value on the board alone is not final).
        s.game.last_ultimate = Some(833_372);
        s.note_cue(page(Page::Main, PageState::Hide), Some(2_000));
        assert_eq!(s.mode(), Mode::Result);
        s.note_cue(Cue::Fight, Some(3_000));
        assert_eq!(s.mode(), Mode::Battle);
        let panel = |wave: u32, activity: f64, bonus: f64, next: f64, percent: i32| read::Battle {
            wave: Some((wave, 3)),
            activity: Some(activity),
            bonus: Some(bonus),
            next: Some(next),
            percent: Some(percent),
            raw: String::new(),
        };
        let crop = || Rgba { w: 4, h: 4, data: vec![0; 64] };
        s.battle_read(panel(1, 91_100.0, 18_200.0, 114_000.0, 0), crop(), 4_000);
        // Same wave, small change in numbers: no row is written again within 5 seconds.
        s.battle_read(panel(1, 95_000.0, 19_000.0, 114_000.0, 0), crop(), 5_000);
        s.battle_read(panel(3, 568_500.0, 221_200.0, 854_000.0, 0), crop(), 6_000);
        assert!(s.view().lines[0].starts_with("終局戰鬥 第 3/3 波"), "{:?}", s.view().lines);
        // Not a battle panel (loading screen): no update.
        s.battle_read(read::Battle::default(), crop(), 7_000);
        // The percentage changed but it is under 5 seconds from the last row: not written (OCR noise comes and goes).
        s.battle_read(panel(3, 836_400.0, 381_900.0, 999_900_000.0, 15), crop(), 8_000);
        s.battle_read(panel(3, 836_400.0, 381_900.0, 999_900_000.0, 15), crop(), 11_500);
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(12_000)), ModeChange::Left);
        let dirs = all_rows(s, &root);
        let rows: Vec<&Value> = dirs.iter().flatten().collect();
        let kind = |k: &str| rows.iter().filter(|r| r["kind"] == k).copied().collect::<Vec<_>>();
        assert_eq!(kind("vorax_battle").len(), 3, "first, wave 3, last (5 s later)");
        assert_eq!(kind("vorax_battle_frame").len(), 2, "one crop per wave");
        let end = kind("vorax_battle_end");
        assert_eq!(end.len(), 1);
        assert_eq!((end[0]["activity"].as_f64(), end[0]["bonus"].as_f64(), end[0]["percent"].as_i64()), (Some(836_400.0), Some(381_900.0), Some(15)));
        assert_eq!((end[0]["board"].as_i64(), end[0]["reads"].as_i64(), end[0]["bonusShare"].as_f64()), (Some(833_372), Some(5), Some(0.457)));
        assert!(end[0]["frame"].as_str().is_some_and(|f| f.starts_with("frame-battle-")));
    }

    /// The 2026-11-06 12:23 game's battle ended on one read of 482.1K as 48.2K: the ending takes the median of the
    /// last reads, and a bonus a tenth of the ones before it is not counted, even when misreads come in a row.
    #[test]
    fn a_battle_ends_on_the_median_of_its_last_reads() {
        let (mut s, root) = logged("battle-median");
        s.note_cue(page(Page::Main, PageState::Hide), Some(2_000));
        s.note_cue(Cue::Fight, Some(3_000));
        let panel = |bonus: f64| read::Battle { wave: Some((3, 3)), activity: Some(1_000_000.0), bonus: Some(bonus), next: Some(999_900_000.0), percent: Some(15), raw: String::new() };
        let crop = || Rgba { w: 4, h: 4, data: vec![0; 64] };
        for (k, bonus) in [481_300.0, 482_100.0, 482_100.0, 48_200.0].into_iter().enumerate() {
            s.battle_read(panel(bonus), crop(), 4_000 + 1_000 * k as i64);
        }
        s.note_cue(Cue::LevelLeft, Some(9_000));
        let dirs = all_rows(s, &root);
        let end: Vec<&Value> = dirs.iter().flatten().filter(|r| r["kind"] == "vorax_battle_end").collect();
        assert_eq!((end[0]["activity"].as_f64(), end[0]["bonus"].as_f64(), end[0]["total"].as_f64()), (Some(1_000_000.0), Some(482_100.0), Some(1_482_100.0)));
        assert_eq!((end[0]["reads"].as_i64(), end[0]["settledFrom"].as_i64()), (Some(4), Some(3)));
        // The last three reads as kept (the misread's bonus dropped) and the count of dropped bonuses.
        assert_eq!(end[0]["recent"].as_array().map(|a| a.iter().map(|p| p["bonus"].as_f64()).collect::<Vec<_>>()), Some(vec![Some(482_100.0), Some(482_100.0), None]));
        assert_eq!(end[0]["bonusDropped"].as_i64(), Some(1));

        // Two misreads in a row: the median of three would take the misread, the drop rule keeps them out.
        let mut recent = Vec::new();
        for bonus in [481_300.0, 482_100.0, 48_200.0, 48_100.0] {
            keep_battle_read(&mut recent, panel(bonus));
        }
        assert_eq!(recent.iter().map(|p| p.bonus).collect::<Vec<_>>(), [Some(482_100.0), None, None]);
        assert_eq!(settled_battle(&recent, &panel(48_100.0)).bonus, Some(482_100.0));
        // A bonus that grows, or dips by OCR noise, is kept; the first read has nothing to compare with.
        let mut recent = Vec::new();
        for bonus in [18_200.0, 221_200.0, 381_900.0, 381_000.0] {
            keep_battle_read(&mut recent, panel(bonus));
        }
        assert_eq!(recent.iter().map(|p| p.bonus).collect::<Vec<_>>(), [Some(221_200.0), Some(381_900.0), Some(381_000.0)]);
        assert_eq!(settled_battle(&recent, &panel(381_000.0)).bonus, Some(381_000.0));
        // No read kept: the last read as it is.
        assert_eq!(settled_battle(&[], &panel(48_200.0)).bonus, Some(48_200.0));
        // Two reads: the later of the two (the 2026-11-03 replay: 422.2K, then the final 381.9K).
        assert_eq!(median([422_200.0, 381_900.0].into_iter()), Some(381_900.0));
        assert_eq!(median([381_900.0, 422_200.0].into_iter()), Some(422_200.0));
        assert_eq!(median(std::iter::empty()), None);
    }

    /// In the 2026-11-04 game the page closed after each round's confirm (Main Destory) and reopened 0.5 seconds later (Main Run, no
    /// intro page): it is resumed as the same game, with the instruments, opening count and round all kept, the log written in the same folder.
    /// No time limit and no Tartarus signal ends a paused game (2026-11-04: taken back 91 minutes later, after a whole Tartarus run), and
    /// closing Ascendant leaves it paused: no ending is written.
    #[test]
    fn a_board_closed_and_reopened_mid_game_is_the_same_game() {
        let (mut s, root) = logged("resume");
        s.game.held.push((6, 1));
        s.game.openings = Some(1);
        steady_round(&mut s, 1, 2_000);
        assert_eq!(s.note_cue(page(Page::Main, PageState::Destory), Some(3_000)), ModeChange::Left);
        assert!(s.paused.is_some());
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(3_500)), ModeChange::Entered);
        assert_eq!(s.game.held, vec![(6, 1)]);
        assert_eq!(s.game.openings, Some(1));
        // On resume it is often the next selection screen of the same round: the same round with as many icons is still the same game.
        steady_round(&mut s, 1, 3_800);
        assert!(s.resumed.is_none());
        assert_eq!(s.game.held, vec![(6, 1)]);
        steady_round(&mut s, 2, 4_000);
        assert!(s.resumed.is_none());
        assert_eq!(s.game.held, vec![(6, 1)], "a later round keeps the game");
        assert_eq!(s.game.rounds_seen.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
        s.note_cue(page(Page::Main, PageState::Destory), Some(5_000));
        // Tartarus played in between, 91 minutes away: still the same game.
        s.drop_paused(6_000, true);
        assert_eq!(s.note_tartarus("Day", Some(7_000)), ModeChange::None);
        assert!(s.paused.is_some(), "Tartarus does not end it");
        let later = 5_000 + 91 * 60 * 1000;
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(later)), ModeChange::Entered);
        assert_eq!(s.game.rounds_seen.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
        steady_round(&mut s, 2, later + 500);
        assert_eq!(s.game.held, vec![(6, 1)]);
        s.note_cue(page(Page::Main, PageState::Destory), Some(later + 1_000));
        // Program closed while still paused: it stays paused (the next start finds it, see paused_on_disk).
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1, "one folder for the game");
        assert!(kinds(&dirs[0], "vorax_end").is_empty(), "no ending: the game is not over");
        assert_eq!(kinds(&dirs[0], "vorax_resume").len(), 2);
        let pauses = kinds(&dirs[0], "vorax_pause");
        assert_eq!(pauses.len(), 3);
        assert_eq!((&pauses[2]["roundsSeen"], &pauses[2]["held"], &pauses[2]["lastRound"]), (&json!([1, 2]), &json!([[6, 1]]), &json!(2)));
    }

    /// What ends a paused game as a new one starts: its charge spent (Coin_826 down), the intro page hidden into the board, or, once taken back,
    /// a smaller round with other monsters in the tanks. Each one back-fills the old game's ending (stamped at its pause, in its folder), and
    /// the new game starts empty in a new folder.
    #[test]
    fn a_new_game_after_a_closed_board_starts_over() {
        type Between = fn(&mut VoraxSession);
        let cases: [(&str, Between); 4] = [
            ("charge", |s| {
                s.note_cue(page(Page::Intro, PageState::Run), Some(3_400));
                assert!(s.paused.is_some(), "the intro page alone is not a new game");
                s.note_cue(Cue::NewGame, Some(3_450));
                assert!(s.paused.is_none());
                s.note_cue(page(Page::Intro, PageState::Hide), Some(3_480));
                s.note_cue(page(Page::Main, PageState::Run), Some(3_500));
            }),
            ("intro-into-board", |s| {
                s.note_cue(page(Page::Intro, PageState::Run), Some(3_400));
                s.note_cue(page(Page::Intro, PageState::Hide), Some(3_480));
                s.note_cue(page(Page::Main, PageState::Run), Some(3_500));
            }),
            ("charge-off", |s| {
                // The charge seen with no intro page (Ascendant missed it): the paused game still ends.
                s.note_cue(Cue::NewGame, Some(3_450));
                s.note_cue(page(Page::Main, PageState::Run), Some(3_500));
            }),
            ("lower-other-monsters", |s| {
                s.note_cue(page(Page::Main, PageState::Run), Some(3_500));
                assert_eq!(s.game.held, vec![(6, 1)], "taken back until a round is read");
                let other = || {
                    let mut r = Reading { round: Some(1), ..potion_board(false, Some(600), 12) };
                    for t in &mut r.tanks {
                        if t.archetype.is_some() {
                            t.archetype = Some(vorax::Archetype::BoneSentry);
                        }
                    }
                    r
                };
                feed(s, other(), 4_000);
                feed(s, other(), 4_060);
            }),
        ];
        for (tag, between) in cases {
            let (mut s, root) = logged(&format!("restart-{tag}"));
            s.game.held.push((6, 1));
            steady_held(&mut s, 3, 1, 2_000);
            s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
            between(&mut s);
            assert!(s.game.held.is_empty(), "{tag}");
            assert!(s.paused.is_none() && s.resumed.is_none(), "{tag}");
            let dirs = all_rows(s, &root);
            assert_eq!(dirs.len(), 2, "{tag}: a new folder");
            let ends = kinds(&dirs[0], "vorax_end");
            assert_eq!(ends.len(), 1, "{tag}");
            assert_eq!(ends[0]["held"], json!([[6, 1]]), "{tag}");
            let off = kinds(&dirs[0], "vorax_mode").into_iter().find(|r| r["to"] == "off").unwrap().clone();
            assert_eq!(ends[0]["at"], off["at"], "{tag}: stamped at the Main Destory");
            assert!(kinds(&dirs[1], "vorax_end").is_empty(), "{tag}");
            assert!(!kinds(&dirs[1], "vorax_mode").is_empty(), "{tag}: the new folder starts with its mode");
        }
    }

    /// What does not end a paused game: the intro page opened and closed again, a smaller round read with the same monsters (a misread),
    /// and the entry's result page of an older game already fought, given up (Reward Run, GiveUp, Reward Destory) while the paused game is
    /// short of the last round. It is taken back by the next Main Run, in the same folder.
    #[test]
    fn a_paused_game_survives_what_is_not_its_end() {
        let (mut s, root) = logged("survives");
        s.game.held.push((6, 1));
        steady_held(&mut s, 3, 1, 2_000);
        s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        s.note_cue(page(Page::Intro, PageState::Run), Some(3_400));
        assert_eq!(s.mode(), Mode::Intro);
        assert_eq!(s.note_cue(page(Page::Intro, PageState::Destory), Some(3_600)), ModeChange::Left);
        assert!(s.paused.is_some());
        for (c, at) in [(page(Page::Reward, PageState::Run), 4_000), (Cue::GiveUp, 4_200), (page(Page::Reward, PageState::Destory), 4_400)] {
            assert_eq!(s.note_cue(c, Some(at)), ModeChange::None, "{c:?}");
            assert_eq!(s.mode(), Mode::Off, "{c:?}");
        }
        assert!(s.paused.is_some(), "the older game's result page is not this game");
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(5_000)), ModeChange::Entered);
        assert_eq!(s.game.held, vec![(6, 1)]);
        // A smaller round, but the same monsters: a misread, still the same game.
        steady_held(&mut s, 1, 1, 5_500);
        assert_eq!(s.game.held, vec![(6, 1)]);
        s.note_cue(page(Page::Main, PageState::Destory), Some(6_000));
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1, "one folder");
        assert!(kinds(&dirs[0], "vorax_end").is_empty());
        assert_eq!(kinds(&dirs[0], "vorax_resume").len(), 1);
    }

    /// Given up in round 1, then a whole Tartarus run, then the board again with no new-game signal: the same game in the same folder.
    #[test]
    fn a_game_left_for_tartarus_is_taken_back() {
        let (mut s, root) = logged("tartarus-between");
        steady_round(&mut s, 1, 2_000);
        s.note_cue(Cue::Synced, Some(2_500));
        s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        for (what, at) in [("RunStart", 60_000), ("Day", 70_000), ("Treasure", 80_000), ("RunEnd", 900_000)] {
            s.drop_paused(at, true);
            assert_eq!(s.note_tartarus(what, Some(at)), ModeChange::None);
        }
        assert!(s.paused.is_some());
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(1_000_000)), ModeChange::Entered);
        steady_round(&mut s, 1, 1_000_500);
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1, "the same folder");
        assert!(kinds(&dirs[0], "vorax_end").is_empty());
        let picks = kinds(&dirs[0], "vorax_round");
        assert_eq!(picks.len(), 1);
        assert!(picks[0]["after"].is_string(), "the pick before the pause gets the board after the resume");
    }

    /// A confirmed give-up on the result page (GiveUp, then Reward Destory) ends the game; the result page closed without it pauses the game,
    /// which its result page takes back.
    #[test]
    fn the_result_page_closed_pauses_and_a_give_up_ends() {
        let (mut s, root) = logged("give-up-result");
        steady_round(&mut s, LAST_ROUND, 2_000);
        s.note_cue(page(Page::Main, PageState::Hide), Some(3_000));
        assert_eq!(s.mode(), Mode::Result);
        // The give-up button alone (its confirmation can be cancelled) and 5621 do not end it.
        s.note_cue(Cue::GiveUp, Some(3_100));
        s.note_cue(Cue::Over, Some(3_150));
        assert_eq!(s.mode(), Mode::Result);
        s.note_cue(page(Page::Reward, PageState::Run), Some(3_200));
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Destory), Some(3_300)), ModeChange::Left);
        assert!(s.paused.is_some(), "closed without a confirmed give-up: paused");
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Run), Some(4_000)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Result);
        s.note_cue(Cue::GiveUp, Some(4_100));
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Destory), Some(4_300)), ModeChange::Left);
        assert!(s.paused.is_none());
        assert!(s.last_game().is_some(), "played to the last round: its card");
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1);
        let ends = kinds(&dirs[0], "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["at"].as_str(), Some(timestamp(at_time(4_300)).as_str()));
    }

    /// Ascendant restarted while a game was paused: the newest folder (its last pause row, no resume after it, no ending) is found on start,
    /// and the next Main Run takes the game back with its instruments, rounds and the pick still waiting for its board. Also the older
    /// folders, written before pause rows, that end with the board closing.
    #[test]
    fn a_game_paused_before_a_restart_is_taken_back() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // The first run: round 4 picked, the board closed, Ascendant closed.
        {
            let mut s = session();
            s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
            s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
            s.game.held.push((6, 1));
            steady_held(&mut s, 4, 1, 2_000);
            s.note_cue(Cue::TankClick { tank: Some(2) }, Some(2_200));
            s.note_cue(Cue::Synced, Some(2_500));
            s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
            s.log().unwrap().wait();
        }
        let first: Vec<PathBuf> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
        assert_eq!(first.len(), 1);
        let dir = first[0].clone();
        // The next start: found, and the board's Main Run takes it back.
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        let p = s.paused.as_ref().expect("found on start");
        assert_eq!((p.at, p.last_round, p.dir.as_deref()), (3_000, 4, Some(dir.as_path())));
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(9_000_000)), ModeChange::Entered);
        assert_eq!(s.game.held, vec![(6, 1)]);
        assert_eq!(s.game.rounds_seen.iter().copied().collect::<Vec<_>>(), vec![4]);
        assert!(s.game.round_row.as_ref().is_some_and(|r| r["chosen"]["tanks"] == json!([2])), "the pick waits for its board");
        assert_eq!(s.log().unwrap().dir(), dir.as_path(), "it goes on in its folder");
        let resume = kinds_now(&s, "vorax_resume");
        assert_eq!(resume[0]["folder"].as_str(), Some(dir.to_string_lossy().as_ref()));
        steady_held(&mut s, 5, 1, 9_000_500);
        s.note_cue(page(Page::Main, PageState::Destory), Some(9_001_000));
        drop(s);
        // Once more on the next start, and this time the next game's charge ends it: the ending goes in its folder,
        // stamped at its last pause, and nothing else was ever written elsewhere.
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        assert_eq!(s.paused.as_ref().map(|p| (p.at, p.last_round)), Some((9_001_000, 5)));
        s.note_cue(Cue::NewGame, Some(9_500_000));
        assert!(s.paused.is_none());
        drop(s);
        let dirs: Vec<PathBuf> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
        assert_eq!(dirs, std::slice::from_ref(&dir), "one folder for the game");
        let rows: Vec<Value> = std::fs::read_to_string(dir.join("log.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let ends = kinds(&rows, "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["at"].as_str(), Some(timestamp(at_time(9_001_000)).as_str()));
        assert_eq!(ends[0]["roundsSeen"], json!([4, 5]));
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        assert!(picks[0]["after"].is_string(), "the pick from before the restart got the board after it");
        let _ = std::fs::remove_dir_all(&root);
        // An older folder: no pause row, the board closing last.
        let rows: Vec<Value> = [
            json!({"at": "2026-11-05T16:02:06.498Z", "kind": "vorax_mode", "from": "off", "to": "intro", "cue": "Page(Intro, Run)"}),
            json!({"at": "2026-11-05T16:02:09.000Z", "kind": "vorax_read", "steady": true, "reading": {"kind": "potion", "round": 1, "tanks": [{"archetype": "pest", "rarity": "boss"}, null]}}),
            json!({"at": "2026-11-05T16:02:09.100Z", "kind": "vorax_advice", "state": {"instruments": [], "instrumentRounds": [], "potionRefreshes": 3, "instrumentRefreshes": 2}}),
            json!({"at": "2026-11-05T16:02:17.597Z", "kind": "vorax_mode", "from": "board", "to": "off", "cue": "Page(Main, Destory)"}),
        ]
        .into();
        let p = paused_in(&rows).expect("an older paused game");
        assert_eq!((p.last_round, p.game.potion_rerolls, p.game.monsters.clone()), (1, Some(3), vec![Some("pest/boss".to_string()), None]));
        assert_eq!(p.at, parse_at("2026-11-05T16:02:17.597Z").unwrap());
        // Ended, or taken back and still being played: not paused.
        let mut ended = rows.clone();
        ended.push(json!({"at": "2026-11-05T16:03:00.000Z", "kind": "vorax_end"}));
        assert!(paused_in(&ended).is_none());
        let mut back = rows.clone();
        back.push(json!({"at": "2026-11-05T16:03:00.000Z", "kind": "vorax_resume"}));
        back.push(json!({"at": "2026-11-05T16:03:00.000Z", "kind": "vorax_mode", "from": "off", "to": "board", "cue": "Page(Main, Run)"}));
        assert!(paused_in(&back).is_none());
        assert_eq!(parse_at(&timestamp(at_time(1_762_358_148_368))), Some(1_762_358_148_368));
    }

    /// Restarted with a game paused in folder A, the intro page opened and closed, the game taken back and paused again, restarted again:
    /// A is found again. The intro page's rows go into A (no folder is opened for it), and a newer folder with no game in it is passed over.
    #[test]
    fn a_restart_survives_an_intro_page_in_between() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-reopen-intro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let start = || {
            let mut s = session();
            s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
            s
        };
        {
            let mut s = start();
            s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
            s.game.held.push((6, 1));
            steady_held(&mut s, 4, 1, 2_000);
            s.note_cue(Cue::Synced, Some(2_500));
            s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        }
        let folders = || -> Vec<PathBuf> {
            let mut d: Vec<PathBuf> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect();
            d.sort();
            d
        };
        let a = folders()[0].clone();
        {
            let mut s = start();
            assert!(s.paused.is_some());
            s.note_cue(page(Page::Intro, PageState::Run), Some(5_000));
            assert_eq!(s.note_cue(page(Page::Intro, PageState::Destory), Some(6_000)), ModeChange::Left);
            assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(7_000)), ModeChange::Entered);
            assert_eq!(s.log().unwrap().dir(), a.as_path());
            steady_held(&mut s, 5, 1, 8_000);
            s.note_cue(page(Page::Main, PageState::Destory), Some(9_000));
        }
        assert_eq!(folders(), std::slice::from_ref(&a), "no folder for the intro page");
        // A newer folder with no game in it (an older Ascendant opened one for an intro page).
        let orphan = root.join("2099-01-01_00-00-00");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("log.jsonl"), json!({"at": "2099-01-01T00:00:00.000Z", "kind": "vorax_mode", "from": "off", "to": "intro"}).to_string() + "\n").unwrap();
        let s = start();
        let p = s.paused.as_ref().expect("A found again");
        assert_eq!((p.dir.as_deref(), p.at, p.last_round), (Some(a.as_path()), 9_000, 5));
        assert_eq!(p.game.rounds_seen.iter().copied().collect::<Vec<_>>(), vec![4, 5]);
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A folder of the older format (no pause row, ending with the board closing), restarted on, the intro page opened and closed, restarted
    /// again: the game is still found, at its original pause time (the pause row written ahead of the intro page's rows).
    #[test]
    fn an_older_paused_folder_survives_an_intro_page() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-reopen-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let a = root.join("2026-11-05_16-02-06");
        std::fs::create_dir_all(&a).unwrap();
        let rows = [
            json!({"at": "2026-11-05T16:02:06.498Z", "kind": "vorax_mode", "from": "off", "to": "intro", "cue": "Page(Intro, Run)"}),
            json!({"at": "2026-11-05T16:02:09.000Z", "kind": "vorax_read", "steady": true, "reading": {"kind": "potion", "round": 1, "tanks": [{"archetype": "pest", "rarity": "boss"}, null]}}),
            json!({"at": "2026-11-05T16:02:17.597Z", "kind": "vorax_mode", "from": "board", "to": "off", "cue": "Page(Main, Destory)"}),
        ];
        std::fs::write(a.join("log.jsonl"), rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let start = || {
            let mut s = session();
            s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
            s
        };
        let paused_at = parse_at("2026-11-05T16:02:17.597Z").unwrap();
        {
            let mut s = start();
            assert_eq!(s.paused.as_ref().map(|p| p.at), Some(paused_at));
            s.note_cue(page(Page::Intro, PageState::Run), Some(paused_at + 60_000));
            s.note_cue(page(Page::Intro, PageState::Destory), Some(paused_at + 61_000));
        }
        let s = start();
        let p = s.paused.as_ref().expect("still found");
        assert_eq!((p.dir.as_deref(), p.at, p.last_round), (Some(a.as_path()), paused_at, 1));
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Restarted with a game paused in folder A, then a new game: by its charge, or by the intro page hidden into the board. A gets exactly
    /// one ending, at its original pause time, and none of the new game's rows; the new game (its mode row and its steady reads) goes to B.
    #[test]
    fn a_new_game_after_a_restart_ends_the_old_one_in_its_folder() {
        type Begin = fn(&mut VoraxSession, i64);
        let cases: [(&str, Begin); 2] = [
            ("charge", |s, t| {
                s.note_cue(page(Page::Intro, PageState::Run), Some(t));
                s.note_cue(Cue::NewGame, Some(t + 100));
                s.note_cue(page(Page::Intro, PageState::Hide), Some(t + 200));
                s.note_cue(page(Page::Main, PageState::Run), Some(t + 300));
            }),
            ("hidden", |s, t| {
                s.note_cue(page(Page::Intro, PageState::Run), Some(t));
                s.note_cue(page(Page::Intro, PageState::Hide), Some(t + 200));
                s.note_cue(page(Page::Main, PageState::Run), Some(t + 300));
            }),
        ];
        for (tag, begin) in cases {
            let root = std::env::temp_dir().join(format!("ascendant-vorax-reopen-new-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let start = || {
                let mut s = session();
                s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
                s
            };
            {
                let mut s = start();
                s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
                steady_held(&mut s, 4, 1, 2_000);
                s.note_cue(Cue::Synced, Some(2_500));
                s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
            }
            let folders = || -> Vec<PathBuf> { std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).filter(|p| p.is_dir()).collect() };
            let a_dir = folders().pop().unwrap();
            let restart = 5_000_000;
            {
                let mut s = start();
                begin(&mut s, restart);
                assert!(s.paused.is_none(), "{tag}");
                assert_eq!(s.mode(), Mode::Board, "{tag}");
                steady_round(&mut s, 1, restart + 1_000);
            }
            // Folders named in the same second are not in creation order by name: B is the one that is not A.
            let dirs = folders();
            assert_eq!(dirs.len(), 2, "{tag}");
            let b_dir = dirs.into_iter().find(|d| *d != a_dir).unwrap();
            let read = |d: &PathBuf| -> Vec<Value> { std::fs::read_to_string(d.join("log.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect() };
            let (a, b) = (read(&a_dir), read(&b_dir));
            let ends = kinds(&a, "vorax_end");
            assert_eq!(ends.len(), 1, "{tag}");
            assert_eq!(ends[0]["at"].as_str(), Some(timestamp(at_time(3_000)).as_str()), "{tag}");
            let before = timestamp(at_time(restart));
            for k in ["vorax_read", "vorax_round"] {
                assert!(kinds(&a, k).iter().all(|r| r["at"].as_str().unwrap() < before.as_str()), "{tag}: no {k} of the new game in A");
            }
            assert!(!kinds(&b, "vorax_mode").is_empty(), "{tag}");
            assert!(kinds(&b, "vorax_read").iter().any(|r| r["steady"] == true), "{tag}");
            assert!(kinds(&b, "vorax_end").is_empty(), "{tag}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// The board straight from the intro page (no Hide, no charge spent) takes the paused game back; a charge spent on the board is no
    /// move of the game (not in its cues).
    #[test]
    fn the_board_straight_from_the_intro_page_takes_the_game_back() {
        let (mut s, root) = logged("intro-straight");
        s.game.held.push((6, 1));
        steady_held(&mut s, 3, 1, 2_000);
        s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        assert_eq!(s.note_cue(page(Page::Intro, PageState::Run), Some(3_400)), ModeChange::Entered);
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(3_500)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Board);
        assert!(s.paused.is_none());
        assert_eq!(s.game.held, vec![(6, 1)]);
        assert_eq!(s.game.rounds_seen.iter().copied().collect::<Vec<_>>(), vec![3]);
        s.note_cue(Cue::Confirm, Some(3_600));
        assert_eq!(s.note_cue(Cue::NewGame, Some(3_700)), ModeChange::None);
        assert_eq!(s.game.cues, [Cue::Confirm], "the charge is not a cue of the game");
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1);
        assert_eq!(kinds(&dirs[0], "vorax_resume").len(), 1);
        assert!(kinds(&dirs[0], "vorax_end").is_empty());
    }

    /// An instrument confirmed just before the board closed, and a potion kit opened, are kept across a restart: the new icon read after the
    /// resume goes into the held instruments with the pick's round.
    #[test]
    fn a_pick_waiting_at_a_restart_is_taken_into_held() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-reopen-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let start = || {
            let mut s = session();
            s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
            s
        };
        {
            let mut s = start();
            s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
            s.game.held.push((6, 1));
            steady_held(&mut s, 4, 1, 2_000);
            s.game.pending_pick = Some(PendingPick { before: 1, round: 5, card: json!({"en": "Clustered Eggs"}), id: None, hovered: None });
            s.game.kit = true;
            s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        }
        let mut s = start();
        let p = s.paused.as_ref().expect("found");
        assert!(p.game.kit);
        assert!(p.game.pending_pick.as_ref().is_some_and(|k| (k.before, k.round, &k.card, k.id) == (1, 5, &json!({"en": "Clustered Eggs"}), None)));
        s.note_cue(page(Page::Main, PageState::Run), Some(9_000));
        steady_held(&mut s, 5, 2, 9_500);
        assert_eq!(s.game.held, vec![(6, 1), (2, 5)]);
        assert!(s.game.pending_pick.is_none());
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Entering town (`_EnterLevelSuccess() 110`, also the first map after logging in) ends nothing on its own: not on the board, not on the
    /// result page, not while paused.
    #[test]
    fn entering_town_alone_ends_nothing() {
        let (mut s, root) = logged("town");
        steady_round(&mut s, 1, 2_000);
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(2_500)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Board);
        s.note_cue(page(Page::Main, PageState::Hide), Some(3_000));
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(3_100)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Result);
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Destory), Some(3_200)), ModeChange::Left);
        assert!(s.paused.is_some());
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(3_300)), ModeChange::None);
        assert!(s.paused.is_some());
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Run), Some(4_000)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Result);
        let dirs = all_rows(s, &root);
        assert!(kinds(&dirs[0], "vorax_end").is_empty());
    }

    /// The Vorax charges going down is a new game paid for (34 to 22, 2026-11-05); the first value seen, and a map run's +1, are not.
    #[test]
    fn a_charge_spent_is_a_new_game() {
        assert!(spent(Some(34), 22));
        assert!(!spent(None, 22));
        assert!(!spent(Some(22), 23));
        assert!(!spent(Some(22), 22));
    }

    /// The rows of `kind` written so far in the session's current folder.
    fn kinds_now(s: &VoraxSession, kind: &str) -> Vec<Value> {
        let log = s.log().unwrap();
        log.wait();
        let text = std::fs::read_to_string(log.dir().join("log.jsonl")).unwrap_or_default();
        text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|r| r["kind"] == kind).collect()
    }

    /// The 2026-11-06_14-06-38 game: round 13's board showed 775,436, the held instruments' end-of-round gain of round
    /// 13 came after it (result screen 850,496, battle panel 851.8K). A result screen read below the last board value is a misread and dropped; the
    /// battle's `board` is never the round 13 board's value (only a final one), and the ending falls back to the
    /// battle panel; so is one over twice the board (a ghost digit). A result screen read from the board's value to twice
    /// it is the final activity.
    #[test]
    fn the_result_screen_wins_and_a_read_below_the_board_is_dropped() {
        // A ghost digit in front (1,850,496) is over twice the board: dropped too.
        for (result, end, from, board) in [(Some(700_000), 851_800, "battle", None), (Some(850_496), 850_496, "result", Some(850_496)), (None, 851_800, "battle", None), (Some(1_850_496), 851_800, "battle", None), (Some(1_550_872), 1_550_872, "result", Some(1_550_872))] {
            let (mut s, root) = logged("result-final");
            let board_13 = || Reading { round: Some(13), ..potion_board(false, Some(775_436), 12) };
            feed(&mut s, board_13(), 2_000);
            feed(&mut s, board_13(), 2_060);
            s.note_cue(page(Page::Main, PageState::Hide), Some(3_000));
            feed(&mut s, Reading { kind: Kind::Result, round: None, cards: vec![], ultimate: result, ..potion_board(false, None, 12) }, 3_050);
            s.note_cue(Cue::Fight, Some(3_100));
            let panel = read::Battle { wave: Some((3, 3)), activity: Some(851_800.0), bonus: Some(391_100.0), next: Some(999_900_000.0), percent: Some(15), raw: String::new() };
            s.battle_read(panel, Rgba { w: 4, h: 4, data: vec![0; 64] }, 4_000);
            assert_eq!(s.note_cue(Cue::LevelLeft, Some(5_000)), ModeChange::Left);
            let dirs = all_rows(s, &root);
            let ends = kinds(&dirs[0], "vorax_end");
            assert_eq!((ends[0]["ultimate"].as_i64(), ends[0]["ultimateFrom"].as_str()), (Some(end), Some(from)), "{result:?}");
            let battle = kinds(&dirs[0], "vorax_battle_end");
            assert_eq!(battle[0]["board"].as_i64(), board, "{result:?}");
        }
    }

    /// Leaving after the result (Fight) without a board Destory: the ending is still written at once, with no pause. Even when the result screen was read only
    /// once (not steady) its final activity is used; when the battle read no panel the ending has reads 0 and is written only once.
    #[test]
    fn leaving_from_the_result_screen_ends_at_once() {
        let (mut s, root) = logged("result");
        steady_last_round(&mut s, 880_000, 2_000);
        s.note_cue(page(Page::Main, PageState::Hide), Some(3_000));
        feed(&mut s, Reading { kind: Kind::Result, ultimate: Some(901_000), ..potion_board(false, None, 12) }, 3_050);
        s.note_cue(Cue::Fight, Some(3_100));
        assert!(s.paused.is_none());
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(3_200)), ModeChange::Left);
        let dirs = all_rows(s, &root);
        let ends = kinds(&dirs[0], "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["ultimate"], json!(901_000));
        let battle = kinds(&dirs[0], "vorax_battle_end");
        assert_eq!(battle.len(), 1);
        assert_eq!((battle[0]["reads"].as_i64(), battle[0]["board"].as_i64()), (Some(0), Some(901_000)));
        // The last selection's vorax_round comes before the ending.
        let order: Vec<&str> = dirs[0].iter().filter_map(|r| r["kind"].as_str()).filter(|k| ["vorax_round", "vorax_end", "vorax_battle_end"].contains(k)).collect();
        assert_eq!(order.last(), Some(&"vorax_battle_end"));
    }

    /// The result screen left before it is read (2026-11-05: on show for 0.54 s): the ending uses the last settled board's
    /// final activity and says so; the game's summary stays for the board page after the battle, with the battle's
    /// panel, until a Tartarus signal clears it.
    #[test]
    fn an_unread_result_screen_falls_back_to_the_last_board() {
        let (mut s, root) = logged("fallback");
        steady_round(&mut s, 13, 2_000);
        // Before the last pick the board's value is not final: no fallback yet.
        assert_eq!(s.game.last_ultimate, None);
        s.game.final_picked = true;
        // After the last pick but before round 13's end-of-round effects (no Tool cue yet, still round 13): not
        // final either.
        steady_round(&mut s, 13, 2_500);
        assert_eq!(s.game.last_ultimate, None);
        // A Tool cue after the last pick: the reads after it are final.
        s.note_cue(Cue::Tool, Some(2_600));
        feed(&mut s, Reading { round: Some(13), ..potion_board(false, Some(700), 12) }, 2_590);
        assert_eq!(s.game.last_ultimate, None, "a frame taken before the Tool cue");
        steady_round(&mut s, 13, 2_700);
        assert_eq!(s.game.last_ultimate, Some(600));
        s.note_cue(page(Page::Main, PageState::Hide), Some(3_000));
        s.note_cue(Cue::Fight, Some(3_100));
        let panel = read::Battle { wave: Some((3, 3)), activity: Some(900.0), bonus: Some(100.0), next: Some(999_900_000.0), percent: Some(15), raw: String::new() };
        s.battle_read(panel.clone(), Rgba { w: 4, h: 4, data: vec![0; 64] }, 4_000);
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(5_000)), ModeChange::Left);
        assert!(!s.active());
        let g = s.last_game().expect("kept after the mode ends").clone();
        assert_eq!((g.board, g.board_from, g.rounds, g.ended_at), (Some(600), Some(BoardFrom::LastBoard), 1, 5_000));
        assert_eq!((g.battle.clone(), g.total()), (Some(panel), Some(1_000.0)));
        assert!(g.log_dir.is_some());
        s.drop_paused(6_000, false);
        assert!(s.last_game().is_some(), "not a Tartarus signal");
        s.drop_paused(6_000, true);
        assert!(s.last_game().is_none(), "Tartarus clears it");
        let dirs = all_rows(s, &root);
        let ends = kinds(&dirs[0], "vorax_end");
        assert_eq!((ends[0]["ultimate"].as_i64(), ends[0]["ultimateFrom"].as_str()), (Some(600), Some("board")));
        assert_eq!(kinds(&dirs[0], "vorax_battle_end")[0]["board"].as_i64(), Some(600));
    }

    /// The board's fallback takes a read of round 14 (the round counter past 13, as in the two games that used it,
    /// 2026-11-05_17-43-38 and 2026-11-06_03-05-30) without a Tool cue, and never a round 13 read before one.
    #[test]
    fn the_last_board_waits_for_round_13_end_of_round_effects() {
        let (mut s, _root) = logged("last-board-14");
        s.game.final_picked = true;
        let resolving = |round: i32, u: i64| Reading { kind: Kind::Resolving, round: Some(round), cards: vec![], ..potion_board(false, Some(u), 12) };
        feed(&mut s, resolving(13, 730_684), 2_000);
        assert_eq!(s.game.last_ultimate, None);
        feed(&mut s, resolving(14, 751_737), 2_100);
        assert_eq!(s.game.last_ultimate, Some(751_737));
        // A Tool cue before the last pick does not count.
        let (mut s, _root) = logged("last-board-tool");
        s.note_cue(Cue::Tool, Some(1_500));
        s.game.final_picked = true;
        feed(&mut s, resolving(13, 730_684), 2_000);
        assert_eq!((s.game.final_tool, s.game.last_ultimate), (None, None));
    }

    /// When the result screen is still being read in the background at start surgery: the ending is written after the read finishes (using its final activity), the status line stays at the endgame battle,
    /// and no read row is written.
    #[test]
    fn a_result_read_in_flight_at_the_fight_still_ends_the_game() {
        let (mut s, root) = logged("inflight");
        steady_last_round(&mut s, 880_000, 2_000);
        s.note_cue(page(Page::Main, PageState::Hide), Some(3_000));
        let (tx, rx) = channel();
        s.scan = Some(rx);
        s.note_cue(Cue::Fight, Some(3_100));
        assert_eq!(s.view().status, Status::Battle);
        let reading = Reading { kind: Kind::Result, ultimate: Some(905_500), ..potion_board(false, None, 12) };
        tx.send(Done { reading, frame: Rgba { w: 16, h: 9, data: vec![0; 16 * 9 * 4] }, at: 3_000, trigger: "auto".into(), read_ms: 0, s: None, icons: None, client: Value::Null }).unwrap();
        s.poll(3_300);
        assert_eq!(s.view().status, Status::Battle);
        assert_eq!(s.battle.board, Some(905_500));
        let dirs = all_rows(s, &root);
        let ends = kinds(&dirs[0], "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["ultimate"], json!(905_500));
        assert!(kinds(&dirs[0], "vorax_read").iter().all(|r| r["at"].as_str() < ends[0]["at"].as_str()));
    }

    /// In battle the panel is read only once a second: frames arriving during a read or before the time is up do not request the next one (before, capture ran at full speed all the time),
    /// the time of the agreed next read is handed to the UI (read_in); when the agreed frame does not come, poll asks for another.
    #[test]
    fn battle_frames_are_read_once_a_second() {
        let (mut s, root) = logged("cadence");
        s.config.inline = false;
        s.note_cue(page(Page::Main, PageState::Hide), Some(2_000));
        s.note_cue(Cue::Fight, Some(3_000));
        assert!(s.take_read_next(), "the first frame at once");
        let frame = || Rgba { w: 16, h: 9, data: vec![0; 16 * 9 * 4] };
        s.read(frame(), 3_100, "auto");
        assert!(s.battle.scan.is_some());
        s.read(frame(), 3_160, "auto");
        assert!(!s.take_read_next() && s.wants_read_in().is_none(), "a frame while reading asks for nothing");
        // Read finished (the result is handed over directly, without waiting for background OCR): about 1 second later.
        s.battle.scan = None;
        s.battle_read(read::Battle::default(), Rgba { w: 4, h: 4, data: vec![0; 64] }, 3_100);
        assert_eq!(s.wants_read_in(), Some(Duration::from_millis(940)));
        s.read(frame(), 3_500, "auto");
        assert!(!s.take_read_next() && s.wants_read_in().is_none(), "too early: the same appointment");
        s.poll(4_200);
        assert!(!s.take_read_next());
        s.poll(4_700);
        assert!(s.take_read_next(), "the appointed frame did not come");
        s.read(frame(), 4_750, "auto");
        assert!(s.battle.scan.is_some());
        // When leaving is blocked (Vorax turned off) the mode stays at Battle: the ending is written only once, and later frames are not read.
        s.config.enabled = false;
        s.battle.scan = None;
        s.note_cue(page(Page::Intro, PageState::Run), Some(5_000));
        s.read(frame(), 6_000, "auto");
        assert!(s.battle.scan.is_none() && s.wants_read_in().is_none());
        s.config.enabled = true;
        let dirs = all_rows(s, &root);
        let rows: Vec<&Value> = dirs.iter().flatten().collect();
        assert_eq!(rows.iter().filter(|r| r["kind"] == "vorax_battle_end").count(), 1);
    }

    /// New opening instrument without a template: an icon held before the first pick in round 1 is treated as the opening instrument and does not block advice; after that
    /// the leftmost slot is still treated as the opening one, and a newly picked one on the right stays unknown when it cannot be recognized.
    #[test]
    fn an_unknown_icon_held_before_the_first_pick_is_the_opening_one() {
        let unknown = || read::Held { id: None, en: None, zh: None, opening: false, score: 0.47 };
        let with = |kind: Kind, round: i32, held: Vec<read::Held>| Reading { kind, round: Some(round), held, ..potion_board(false, Some(600), 12) };
        // The round 1 instrument selection screen was not seen (Ascendant opened midway): it stays unknown as before.
        let mut s = session();
        let mut r = with(Kind::Potion, 3, vec![unknown()]);
        s.mark_openings(&mut r);
        assert!(!r.held[0].opening);

        let mut s = session();
        let mut r = with(Kind::Instrument, 1, vec![unknown()]);
        s.mark_openings(&mut r);
        assert!(r.held[0].opening && r.held[0].zh.as_deref() == Some("開局用具"));
        assert!(crate::vorax::to_screen(&r, None).is_ok(), "{:?}", crate::vorax::to_screen(&r, None).err());
        // After the first pick: the leftmost is still the opening instrument, and a newly added one on the right is unknown if it cannot be recognized.
        s.game.held.push((6, 1));
        let mut r = with(Kind::Potion, 2, vec![unknown(), unknown()]);
        s.mark_openings(&mut r);
        assert!(r.held[0].opening && !r.held[1].opening);
        assert_eq!(held_ids(&r), vec![None]);

        // Ascendant opened midway (round 1 not seen): among 4 slots the unrecognized leftmost one must be the opening instrument, and advice is still given.
        let known = |id: u8| read::Held { id: Some(id), en: None, zh: None, opening: false, score: 1.0 };
        let mut s = session();
        let mut r = with(Kind::Potion, 4, vec![unknown(), known(6), known(14), known(18)]);
        s.mark_openings(&mut r);
        assert!(r.held[0].opening && !r.held[1].opening);
        assert_eq!(held_ids(&r), vec![Some(6), Some(14), Some(18)]);
        // With 3 slots it cannot be certain (it may be 3 picks with no opening instrument): it stays unknown as before.
        let mut s = session();
        let mut r = with(Kind::Potion, 4, vec![unknown(), known(6), known(14)]);
        s.mark_openings(&mut r);
        assert!(!r.held[0].opening);
    }

    /// An instrument was picked that has no icon template yet (`異形螯針`): when the new icon is not recognized, the card id from confirm time is used, and advice is still given afterwards;
    /// only that one slot may be unknown, and when an already recognized slot reads as unknown there is still no advice. When the card name was not read (id is None) there is still no advice as before.
    #[test]
    fn a_picked_instrument_without_a_template_is_taken_from_the_card() {
        let known = |id: u8| read::Held { id: Some(id), en: None, zh: None, opening: false, score: 1.0 };
        let unknown = || read::Held { id: None, en: None, zh: None, opening: false, score: 0.4 };
        let with = |held: Vec<read::Held>| Reading { round: Some(5), held, ..potion_board(false, Some(600), 12) };
        let mut s = session();
        s.game.held.push((6, 1));
        let pick = |id: Option<u8>| PendingPick { before: 1, round: 5, card: Value::Null, id, hovered: None };
        // Early solve: after the confirm a new icon appears and cannot be recognized.
        s.game.pending_pick = Some(pick(None));
        assert!(s.screen_state_ahead(&with(vec![known(6), unknown()])).is_err());
        s.game.pending_pick = Some(pick(Some(20)));
        let (x, _) = s.screen_state_ahead(&with(vec![known(6), unknown()])).unwrap();
        assert_eq!(x.instruments.len(), 2);
        // No card read raised (a shallow raise the reader cleared) but the tooltip named one: that one, early and steady.
        s.game.pending_pick = Some(PendingPick { hovered: Some(20), ..pick(None) });
        let (x, _) = s.screen_state_ahead(&with(vec![known(6), unknown()])).unwrap();
        assert_eq!(x.instruments[1], vorax::data().instrument(20).unwrap().name);
        let mut t = session();
        t.game.held.push((6, 1));
        t.game.pending_pick = Some(PendingPick { hovered: Some(20), ..pick(None) });
        t.check_pick(&with(vec![known(6), unknown()]), 1_000);
        assert_eq!((t.game.held.clone(), t.game.from_card.contains(&1)), (vec![(6, 1), (20, 5)], true));
        // After the steady read it is recorded in held, and later screens still get advice.
        s.game.pending_pick = None;
        s.game.held.push((20, 5));
        s.game.from_card.insert(1);
        assert!(s.screen_state(&with(vec![known(6), unknown()])).is_ok());
        // An already recognized slot reads as unknown: no advice (a half-known held set is not solved).
        assert!(s.screen_state(&with(vec![unknown(), unknown()])).is_err());
        // The slot recorded from the card ignores the icon: if it is recognized later, or recognized as a different one, the card wins.
        assert!(s.screen_state(&with(vec![known(6), known(20)])).is_ok());
        let (x, _) = s.screen_state(&with(vec![known(6), known(11)])).unwrap();
        assert_eq!(x.instruments[1], vorax::data().instrument(20).unwrap().name);
        // The card and the new icon do not match (`疫區聖母像` recognized as `獸筋絞肉索`): the early solve also goes by the card.
        let mut s = session();
        s.game.pending_pick = Some(PendingPick { before: 0, round: 1, card: Value::Null, id: Some(19), hovered: None });
        let (x, _) = s.screen_state_ahead(&with(vec![known(11)])).unwrap();
        assert_eq!(x.instruments, [vorax::data().instrument(19).unwrap().name.clone()]);
        // Unless the card the tooltip named at confirm is the icon's (2026-11-06_20-03-09 round 4: `髒污刮骨刀`, 17,
        // hovered and its icon came; its neighbour `鞣製皮革拘束帶`, 21, read raised): the icon, early and steady.
        let mut s = session();
        s.game.pending_pick = Some(PendingPick { before: 0, round: 5, card: Value::Null, id: Some(21), hovered: Some(17) });
        let (x, _) = s.screen_state_ahead(&with(vec![known(17)])).unwrap();
        assert_eq!(x.instruments, [vorax::data().instrument(17).unwrap().name.clone()]);
        // The pick's row, not written yet, names the icon's card too.
        let row = |hand: Value, round: i32| json!({ "round": round, "phase": "instrument", "missing": [], "chosen": { "card": 2, "how": "raised", "hand": hand } });
        let hand = json!(["Rawhide Restraint", "Fouled Scraper", "Tanned Leather Restraint"]);
        s.game.round_row = Some(row(hand.clone(), 5));
        s.check_pick(&with(vec![known(17)]), 1_000);
        assert_eq!((s.game.held.clone(), s.game.from_card.is_empty()), (vec![(17, 5)], true));
        let r = s.game.round_row.clone().unwrap();
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"]), (&json!(1), &json!("icon")));
        // No card seen picked (2026-11-06_21-42-03 round 1, row 24: confirmed within 1.8 s): the icon names the row's
        // card; a row that already has one is left alone.
        for (card, how, want) in [(Value::Null, Value::Null, (json!(1), json!("icon"))), (json!(2), json!("tooltip"), (json!(2), json!("tooltip")))] {
            let mut s = session();
            s.game.pending_pick = Some(PendingPick { before: 0, round: 5, card: Value::Null, id: None, hovered: None });
            s.game.round_row = Some(json!({ "round": 5, "phase": "instrument", "missing": ["card"], "chosen": { "card": card, "how": how, "hand": hand.clone() } }));
            s.check_pick(&with(vec![known(17)]), 1_000);
            let r = s.game.round_row.clone().unwrap();
            assert_eq!((r["chosen"]["card"].clone(), r["chosen"]["how"].clone()), want);
            assert_eq!(s.game.held, vec![(17, 5)]);
        }
        // The icon's card not read in the hand: null, not the raised card. Another round's row is left alone.
        for (hand, round, want) in [(json!(["Rawhide Restraint", null, "Tanned Leather Restraint"]), 5, (Value::Null, Value::Null, json!(["card"]))), (hand.clone(), 4, (json!(2), json!("raised"), json!([])))] {
            let mut s = session();
            s.game.pending_pick = Some(PendingPick { before: 0, round: 5, card: Value::Null, id: Some(21), hovered: Some(17) });
            s.game.round_row = Some(row(hand, round));
            s.check_pick(&with(vec![known(17)]), 1_000);
            let r = s.game.round_row.clone().unwrap();
            assert_eq!((r["chosen"]["card"].clone(), r["chosen"]["how"].clone(), r["missing"].clone()), want, "round {round}");
        }
    }

    /// A garbled read as the mouse slides over the cards (different card count, two unreadable): with no log line it is not a new hand, and the boxes stay;
    /// after a confirm the same read removes the boxes as usual.
    #[test]
    fn a_garbled_read_of_the_same_hand_keeps_the_boxes() {
        let (mut s, root) = logged("garbled");
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        feed(&mut s, potion_board(false, Some(600), 12), 2_060);
        assert!(s.view().advice.is_some() && !s.view().boxes.is_empty(), "{:?}", s.view().status);
        let boxes = s.view().boxes.clone();
        let garbled = || {
            let mut r = potion_board(false, Some(600), 12);
            r.cards.truncate(2);
            r.cards[0].en = None;
            r.cards[1].en = None;
            r
        };
        feed(&mut s, garbled(), 2_200);
        assert_eq!(s.view().boxes, boxes);
        feed(&mut s, potion_board(false, Some(600), 12), 2_350);
        assert_eq!(s.view().boxes, boxes);
        // Only one unreadable and a different card count: still treated as a changed hand as before.
        let mut one = potion_board(false, Some(600), 12);
        one.cards.truncate(2);
        one.cards[0].en = None;
        feed(&mut s, one, 2_500);
        assert!(s.view().boxes.is_empty());
        let _ = rows_of(s, &root);
    }

    /// After picking a card, reads go back and forth between "resolving" and the selection screen (the card-raise animation): not confirmed yet, so the advice and boxes stay;
    /// only a resolving read after the confirm (5619) removes the boxes.
    #[test]
    fn a_resolving_read_before_confirming_keeps_the_boxes() {
        let (mut s, root) = logged("glitch");
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        feed(&mut s, potion_board(false, Some(600), 12), 2_060);
        assert!(s.view().advice.is_some() && !s.view().boxes.is_empty(), "{:?}", s.view().status);
        let boxes = s.view().boxes.clone();
        let resolving = || Reading { kind: Kind::Resolving, cards: vec![], rerolls: None, ..potion_board(false, Some(600), 12) };
        s.note_cue(Cue::CardClick, Some(2_100));
        feed(&mut s, resolving(), 2_150);
        feed(&mut s, resolving(), 2_300);
        assert_eq!(s.view().boxes, boxes, "a card animation is not the resolve");
        assert_eq!(s.view().status, Status::Advised);
        feed(&mut s, potion_board(false, Some(600), 12), 2_450);
        assert_eq!(s.view().boxes, boxes);
        // A resolving read after the confirm removes the boxes as usual.
        s.note_cue(Cue::Confirm, Some(2_500));
        s.note_cue(Cue::Synced, Some(2_510));
        feed(&mut s, resolving(), 2_600);
        feed(&mut s, resolving(), 2_700);
        assert!(s.view().boxes.is_empty());
        drop(rows_of(s, &root));
    }

    /// Solve while it ticks: solve while the value is still ticking, but do not show before it stops; the read after it stops shows it directly, with no further solve.
    #[test]
    fn solves_while_the_numbers_count_and_shows_when_they_stop() {
        let (mut s, root) = logged("shows");
        // A ticking board read twice (same signature, steady): the solve completes, but it is not shown.
        feed(&mut s, potion_board(true, Some(420), 12), 2_000);
        feed(&mut s, potion_board(true, Some(510), 12), 2_060);
        assert!(s.early.is_some(), "solved while counting");
        assert_eq!(s.view().status, Status::Resolving);
        assert!(s.view().advice.is_none() && s.view().boxes.is_empty() && s.advice().is_none());
        // A third read while still ticking: no further solve, and nothing is shown.
        feed(&mut s, potion_board(true, Some(590), 12), 2_120);
        assert!(s.early.is_some() && s.view().advice.is_none() && s.view().boxes.is_empty());
        // Ticking stopped (the final activity equals the tank sum): this read shows it, using the result computed during ticking.
        feed(&mut s, potion_board(false, Some(600), 12), 2_180);
        assert_eq!(s.view().status, Status::Advised);
        assert!(s.view().advice.is_some() && !s.view().boxes.is_empty() && s.early.is_none() && !s.busy());
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!(advice.len(), 1, "one solve");
        assert_eq!(advice[0]["early"], json!(true));
        assert!(rows.iter().all(|r| r["kind"] != "vorax_early_dropped"));
    }

    /// The green "+N" over a final activity that already equals the tanks' sum (the "+0" a reroll leaves, or a "+N"
    /// left after the count): settled at once, the advice solved meanwhile shows without waiting for the flag to clear.
    #[test]
    fn a_counting_flag_over_the_final_sum_does_not_hold_the_advice() {
        let (mut s, root) = logged("plus-zero-sum");
        feed(&mut s, potion_board(true, Some(420), 12), 2_000);
        feed(&mut s, potion_board(true, Some(510), 12), 2_060);
        assert!(s.early.is_some() && s.view().advice.is_none(), "counting below the sum: wait");
        feed(&mut s, potion_board(true, Some(600), 12), 2_120);
        assert_eq!(s.view().status, Status::Advised);
        assert!(s.view().advice.is_some() && !s.view().boxes.is_empty());
        let rows = rows_of(s, &root);
        assert!(kinds(&rows, "vorax_pending_stuck").is_empty());
        assert_eq!(kinds(&rows, "vorax_advice").len(), 1);
    }

    /// The green "+0" a reroll leaves over an unchanged final activity, read as counting on one steady board: past
    /// PENDING_MAX_MS it counts as settled and the advice solved meanwhile shows (2026-11-07_17-59-47 round 8 waited
    /// 6.2 s, until a tooltip covered the panel). A new board starts the wait again.
    #[test]
    fn a_plus_zero_left_on_a_steady_board_does_not_hold_the_advice() {
        let (mut s, root) = logged("plus-zero");
        feed(&mut s, potion_board(true, None, 12), 2_000);
        feed(&mut s, potion_board(true, None, 12), 2_060);
        assert!(s.early.is_some() && s.view().advice.is_none());
        feed(&mut s, potion_board(true, None, 12), 2_000 + PENDING_MAX_MS);
        assert!(s.view().advice.is_none(), "still within the wait");
        feed(&mut s, potion_board(true, None, 12), 2_100 + PENDING_MAX_MS);
        assert_eq!(s.view().status, Status::Advised);
        assert!(s.view().advice.is_some() && !s.view().boxes.is_empty());
        // Another board, counting: not settled by the earlier stretch.
        feed(&mut s, potion_board(true, None, 20), 2_200 + PENDING_MAX_MS);
        feed(&mut s, potion_board(true, None, 20), 2_260 + PENDING_MAX_MS);
        assert_eq!(s.view().status, Status::Resolving);
        let rows = rows_of(s, &root);
        let stuck = kinds(&rows, "vorax_pending_stuck");
        assert_eq!((stuck.len(), &stuck[0]["settled"]), (1, &json!(true)));
    }

    /// After ticking stops, the final activity and the tank sum do not add up (misread): the result computed during ticking is not shown, and the usual flow says unreadable.
    #[test]
    fn an_early_solve_is_dropped_when_the_settled_board_does_not_add_up() {
        let (mut s, root) = logged("unreadable");
        feed(&mut s, potion_board(true, None, 12), 2_000);
        feed(&mut s, potion_board(true, None, 12), 2_060);
        assert!(s.early.is_some());
        feed(&mut s, potion_board(false, Some(700), 12), 2_120);
        assert_eq!(s.view().status, Status::Unreadable);
        assert!(s.view().advice.is_none() && s.view().boxes.is_empty() && s.advice().is_none());
        let rows = rows_of(s, &root);
        assert!(rows.iter().all(|r| r["kind"] != "vorax_advice"));
        let dropped: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_early_dropped").collect();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0]["why"], json!("unreadable"));
    }

    /// After a hand change or a confirm, the result computed during ticking is dropped; when ticking stops and the board already differs, the old one is not shown either.
    #[test]
    fn an_early_solve_is_dropped_after_a_confirm_or_a_new_board() {
        // Confirm (5619): the gate closes and the result is dropped.
        let mut s = session();
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        assert!(s.early.is_some());
        s.note_cue(Cue::Synced, Some(2_100));
        assert!(s.early.is_none() && s.view().gated);
        // Only the confirm button (the log's moves changed, no 5619 yet): when ticking stops the one computed during ticking is not shown, and it is solved again.
        let (mut s, root) = logged("moves");
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        s.note_cue(Cue::Confirm, Some(2_100));
        feed(&mut s, potion_board(false, Some(600), 12), 2_180);
        assert_eq!(s.view().status, Status::Advised);
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!(advice.len(), 1);
        assert_eq!(advice[0]["early"], json!(false), "solved again after the confirm");
        assert_eq!(rows.iter().filter(|r| r["kind"] == "vorax_early_dropped").map(|r| r["why"].clone()).collect::<Vec<_>>(), [json!("moves")]);
        // The deal sound of the same hand (Figure_Refresh) arrives later than the card animation opening the gate: it does not count as a hand change, and it is shown directly when ticking stops.
        let (mut s, root) = logged("dealt");
        s.note_cue(Cue::Synced, Some(1_500));
        s.note_cue(Cue::Shuffle, Some(1_900));
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        s.note_cue(Cue::Dealt, Some(2_400));
        s.note_cue(Cue::Dealt, Some(2_400));
        feed(&mut s, potion_board(false, Some(600), 12), 2_500);
        assert_eq!(s.view().status, Status::Advised);
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!((advice.len(), &advice[0]["early"]), (1, &json!(true)));
        assert!(rows.iter().all(|r| r["kind"] != "vorax_early_dropped"));
        // When ticking stops it is a different board (the first tank's quantity changed): the old one is dropped; the new board is solved early on its first read,
        // and what is shown when it is steady is the new board's result.
        let (mut s, root) = logged("board");
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        feed(&mut s, potion_board(false, Some(615), 13), 2_120);
        assert!(s.view().advice.is_none() && s.view().boxes.is_empty(), "not the old board's advice");
        assert!(s.early.as_ref().is_some_and(|e| e.x.tanks[0].as_ref().unwrap().quantity == 13), "the old one dropped, the new one solved ahead");
        feed(&mut s, potion_board(false, Some(615), 13), 2_180);
        assert_eq!(s.view().status, Status::Advised);
        assert!(s.early.is_none());
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!(advice.len(), 1);
        assert_eq!((&advice[0]["early"], &advice[0]["state"]["tanks"][0]["quantity"]), (&json!(true), &json!(13)));
    }

    /// Early solve: starts computing on the first read (not yet steady), shows directly when the same signature is read again; dropped when the signature changes;
    /// the same signature in the same hand is solved early only once.
    #[test]
    fn solves_ahead_on_the_first_read() {
        // Solved already on the first read, but not shown; the second read (steady) shows it directly, with only one solve.
        let (mut s, root) = logged("ahead");
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        assert!(s.early.is_some() && s.view().advice.is_none() && s.view().boxes.is_empty() && s.advice().is_none());
        feed(&mut s, potion_board(false, Some(600), 12), 2_060);
        assert_eq!(s.view().status, Status::Advised);
        assert!(!s.view().boxes.is_empty() && s.early.is_none());
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!((advice.len(), &advice[0]["early"]), (1, &json!(true)));
        // The first read during the gate (after a confirm, until the deal animation opens the gate) also counts; a result from before the confirm is not kept.
        let (mut s, root) = logged("gate");
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        s.note_cue(Cue::Synced, Some(2_100));
        assert!(s.early.is_none() && s.game.tried.is_empty());
        s.note_cue(Cue::Shuffle, Some(2_600));
        feed(&mut s, potion_board(false, Some(615), 13), 2_700);
        assert!(s.gate.is_some() && s.early.is_some() && s.view().boxes.is_empty() && s.view().gated);
        feed(&mut s, potion_board(false, Some(615), 13), 2_760);
        assert_eq!(s.view().status, Status::Advised);
        assert!(!s.view().gated && !s.view().boxes.is_empty());
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!(advice.len(), 1);
        assert_eq!((&advice[0]["early"], &advice[0]["state"]["tanks"][0]["quantity"]), (&json!(true), &json!(13)));
        // A, B, A: when B is read A's result is dropped; reading A again does not solve early again (once per hand), and after it is steady it solves by the usual flow.
        let (mut s, root) = logged("once");
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        feed(&mut s, potion_board(false, Some(615), 13), 2_060);
        assert!(s.early.as_ref().is_some_and(|e| e.x.tanks[0].as_ref().unwrap().quantity == 13));
        feed(&mut s, potion_board(false, Some(600), 12), 2_120);
        assert!(s.early.is_none() && s.view().advice.is_none());
        feed(&mut s, potion_board(false, Some(600), 12), 2_180);
        assert_eq!(s.view().status, Status::Advised);
        let rows = rows_of(s, &root);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!(advice.len(), 1);
        assert_eq!((&advice[0]["early"], &advice[0]["state"]["tanks"][0]["quantity"]), (&json!(false), &json!(12)));
        // Protocols and boards that cannot be fully read are not solved early.
        let mut s = session();
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        feed(&mut s, Reading { kind: Kind::Protocol, ..potion_board(false, Some(600), 12) }, 2_000);
        assert!(s.early.is_none() && !s.busy());
        let mut r = potion_board(false, Some(600), 12);
        r.cards[1].en = None;
        feed(&mut s, r, 2_060);
        assert!(s.early.is_none() && !s.busy());
    }

    /// Background solve: when a different signature is read, the early solve in progress is told to give up and its result is not sent back; the new one completes as usual.
    #[test]
    fn a_newer_read_cancels_the_solve_ahead() {
        let mut s = session();
        (s.config.inline, s.config.rollouts) = (false, 16_384);
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        let first = s.solve.as_ref().map(|p| p.cancel.clone()).expect("solving ahead");
        feed(&mut s, potion_board(false, Some(615), 13), 2_060);
        assert!(first.load(Ordering::Relaxed), "the old solve was told to stop");
        assert!(s.solve.as_ref().is_some_and(|p| p.early && p.sig.contains("Some(13);")));
        feed(&mut s, potion_board(false, Some(615), 13), 2_120);
        let t0 = Instant::now();
        while s.busy() && t0.elapsed() < Duration::from_secs(120) {
            s.poll(s.clock);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(s.view().status, Status::Advised);
        assert_eq!(s.state().and_then(|x| x.tanks[0].as_ref()).map(|m| m.quantity), Some(13));
    }

    /// The sliced solve (plain, can be abandoned midway) gives exactly the same result as advise_plan.
    /// (Since 2026-11-04 `solve_now` defaults to racing, so this calls plain `solve_with(.., None, ..)` instead; the assertions are unchanged.)
    #[test]
    fn sliced_solve_equals_advise_plan() {
        let x = to_screen(&potion_board(false, Some(600), 12), None).unwrap();
        let (a, _) = solve_with(None, &x, 1_500, None, &AtomicBool::new(false)).unwrap();
        let b = vorax::advisor::advise_plan(&x, Some(1_500), true);
        assert_eq!((serde_json::to_value(&a.best).unwrap(), serde_json::to_value(&a.ranking).unwrap()), (serde_json::to_value(&b.best).unwrap(), serde_json::to_value(&b.ranking).unwrap()));
        assert!(solve_with(None, &x, 1_500, None, &AtomicBool::new(true)).is_none());
    }

    /// The racing `solve_now` (can be abandoned midway) gives exactly the same result as the crate's `advise_raced` (same Z, same batches);
    /// with a very large Z (almost no elimination) the choice is the same as plain.
    #[test]
    fn raced_solve_equals_advise_raced() {
        use vorax::advisor::{DEFAULT_SEED, advise_raced, choice_key};
        let x = to_screen(&potion_board(false, Some(600), 12), None).unwrap();
        let json = |a: &Advice| (serde_json::to_value(&a.best).unwrap(), serde_json::to_value(&a.ranking).unwrap(), serde_json::to_value(a.refresh_end).unwrap());
        let (a, _) = solve_now(None, &x, 4_096, &AtomicBool::new(false)).unwrap();
        let race = Race { z: RACE_Z.unwrap(), first: RACE_FIRST, step: RACE_STEP, max: 4_096, paired: false };
        let (b, futures) = advise_raced(&x, &race, true, DEFAULT_SEED);
        assert_eq!(json(&a), json(&b));
        assert!(futures > 0);
        assert!(solve_now(None, &x, 4_096, &AtomicBool::new(true)).is_none());
        let (wide, _) = solve_with(None, &x, 4_096, Some(1_000.0), &AtomicBool::new(false)).unwrap();
        let plain = vorax::advisor::advise_plan(&x, Some(4_096), true);
        assert_eq!(choice_key(&wide), choice_key(&plain));
    }

    /// The instrument reroll count on the first instrument screen of a game, before any pick (Intern Shelley adds 1 or
    /// 2 to it), is remembered in the log root and assumed for later games until their own count is seen; a later
    /// screen of the same game is not an opening. Ties between remembered counts go to the newest.
    #[test]
    fn the_opening_instrument_rerolls_are_learned() {
        let (mut s, root) = logged("openings");
        assert_eq!(s.opening_instrument_rerolls(), INSTRUMENT_REROLLS);
        let screen = |rerolls: i32| Reading { kind: Kind::Instrument, round: Some(1), rerolls: Some(rerolls), ..potion_board(false, Some(600), 12) };
        feed(&mut s, screen(4), 2_000);
        feed(&mut s, screen(4), 2_060);
        feed(&mut s, screen(3), 3_000);
        feed(&mut s, screen(3), 3_060);
        assert_eq!(s.openings, vec![4], "a reroll later in the game is not an opening");
        let again = VoraxSession::new(s.config.clone());
        assert_eq!(again.opening_instrument_rerolls(), 4);
        assert_eq!((opening_default(&[2, 4, 4, 2]), opening_default(&[2, 4, 4]), opening_default(&[])), (Some(2), Some(4), None));
        drop(again);
        let _ = rows_of(s, &root);
    }

    /// The solver input recomputed after ticking stops differs from the one computed during ticking (here the off-screen instrument reroll count changed):
    /// the result from ticking is not shown, it is dropped (why = state) and solved again.
    #[test]
    fn an_early_solve_is_dropped_when_the_state_changes() {
        let (mut s, root) = logged("state");
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        assert!(s.early.as_ref().is_some_and(|e| e.x.instrument_refreshes == Some(INSTRUMENT_REROLLS)));
        s.game.instrument_rerolls = Some(0);
        feed(&mut s, potion_board(false, Some(600), 12), 2_120);
        assert_eq!(s.view().status, Status::Advised);
        assert_eq!(s.state().map(|x| x.instrument_refreshes), Some(Some(0)));
        let rows = rows_of(s, &root);
        assert_eq!(rows.iter().filter(|r| r["kind"] == "vorax_early_dropped").map(|r| r["why"].clone()).collect::<Vec<_>>(), [json!("state")]);
        let advice: Vec<&Value> = rows.iter().filter(|r| r["kind"] == "vorax_advice").collect();
        assert_eq!((advice.len(), &advice[0]["early"]), (1, &json!(false)));
    }

    /// Background solve (the real-game path): when it starts during ticking and finishes before ticking stops it is kept, and the read after ticking stops shows it directly; when it is not finished
    /// by the time ticking stops, the status changes to solving, and it is shown when done.
    #[test]
    fn early_solves_in_the_background() {
        let wait = |s: &mut VoraxSession| {
            let t0 = Instant::now();
            while s.busy() && t0.elapsed() < Duration::from_secs(60) {
                s.poll(s.clock);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!s.busy());
        };
        let mut s = session();
        s.config.inline = false;
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        assert!(s.busy());
        wait(&mut s);
        assert!(s.early.is_some() && s.view().advice.is_none() && s.view().boxes.is_empty());
        feed(&mut s, potion_board(false, Some(600), 12), 2_120);
        assert_eq!(s.view().status, Status::Advised);
        assert!(!s.busy() && !s.view().boxes.is_empty());
        // Still computing when ticking stops: solving, and shown only when done.
        let mut s = session();
        s.config.inline = false;
        s.config.rollouts = 4096;
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        feed(&mut s, potion_board(true, Some(500), 12), 2_000);
        feed(&mut s, potion_board(true, Some(500), 12), 2_060);
        feed(&mut s, potion_board(false, Some(600), 12), 2_120);
        if s.busy() {
            assert_eq!(s.view().status, Status::Solving);
            assert!(s.view().advice.is_none() && s.view().boxes.is_empty());
        }
        wait(&mut s);
        assert_eq!(s.view().status, Status::Advised);
        assert!(!s.view().boxes.is_empty());
    }

    #[test]
    fn the_gate_waits_for_the_next_cards() {
        let mut s = session();
        s.note_cue(Cue::Page(Page::Main, PageState::Run), Some(1_000));
        assert!(!s.gate_closed(1_000));
        s.note_cue(Cue::Synced, Some(2_000));
        assert!(s.gate_closed(2_100) && s.view().gated && s.view().boxes.is_empty());
        assert_eq!(s.view().status, Status::Resolving);
        // Read after the timeout.
        assert!(!s.gate_closed(2_000 + GATE_MS));
        // New hand: two Figure_Refresh lines in the same millisecond count once.
        s.note_cue(Cue::Dealt, Some(2_500));
        s.note_cue(Cue::Dealt, Some(2_500));
        assert!(!s.gate_closed(2_600));
        assert_eq!(s.game.cues, [Cue::Dealt]);
        // Confirm once more: the gate restarts, and the card animation also opens it.
        s.note_cue(Cue::Synced, Some(3_000));
        assert!(s.gate_closed(3_100));
        s.note_cue(Cue::Shuffle, Some(3_200));
        assert!(!s.gate_closed(3_300));
        // Frames arriving while the gate is closed are not read, and only report that the screen is changing.
        s.note_cue(Cue::Synced, Some(4_000));
        s.read(Rgba { w: 1, h: 1, data: vec![0, 0, 0, 255] }, 4_100, "auto");
        assert_eq!(s.take_news(), Some((true, false)));
        assert!(s.reading().is_none() && !s.busy());
    }

    /// The board of round `round` in the skip-animation game: tank 1's quantity differs per round so each round has
    /// its own signature, and the final activity adds up to the tanks.
    fn skip_board(round: i32) -> Reading {
        let q = 12 + i64::from(round);
        Reading { round: Some(round), ..potion_board(false, Some(15 * q + 420), q) }
    }

    /// The 2026-11-05 15:54 game: after each round's confirm the player closed the board page and reopened it about
    /// 0.7 s later to skip the scoring animation (Main Destory, then Main Run); after round 13, Main Destory, then the
    /// result page (Reward Run), start surgery and the battle map. It stays one game in one folder, every pick's
    /// `vorax_round` gets the first steady read after the reopening as its `after`, the battle is entered, the ending
    /// takes the battle panel's activity (no result screen nor settled board was read), and the result card is kept.
    #[test]
    fn skipping_the_animation_keeps_one_game() {
        use PageState::{Destory, Run};
        let (mut s, root) = logged("skip");
        let mut afters: Vec<String> = Vec::new();
        for round in 1..=LAST_ROUND {
            let t = 2_000 * i64::from(round);
            let board = skip_board(round);
            if round > 1 {
                // The first steady read after the reopening is the board after the previous pick.
                afters.push(signature(&board));
            }
            feed(&mut s, board.clone(), t);
            feed(&mut s, board, t + 60);
            assert_eq!(s.mode(), Mode::Board, "round {round}");
            s.note_cue(Cue::TankClick { tank: Some(1) }, Some(t + 100));
            s.note_cue(Cue::Confirm, Some(t + 150));
            s.note_cue(Cue::Synced, Some(t + 200));
            assert_eq!(s.note_cue(page(Page::Main, Destory), Some(t + 300)), ModeChange::Left, "round {round}");
            assert!(s.paused.as_ref().is_some_and(|p| p.game.round_row.is_some()), "round {round}: the pick waits in the paused game");
            if round < LAST_ROUND {
                assert_eq!(s.note_cue(page(Page::Main, Run), Some(t + 1_000)), ModeChange::Entered, "round {round}");
            }
        }
        let t = 2_000 * i64::from(LAST_ROUND);
        assert_eq!(s.note_cue(page(Page::Reward, Run), Some(t + 1_000)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Result);
        let result = Reading { kind: Kind::Result, round: None, ultimate: None, cards: vec![], ..potion_board(false, None, 12) };
        afters.push(signature(&result));
        feed(&mut s, result.clone(), t + 1_100);
        feed(&mut s, result, t + 1_160);
        assert_eq!(s.note_cue(Cue::Fight, Some(t + 1_300)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Battle);
        assert_eq!(s.note_cue(Cue::BattleEntered, Some(t + 1_400)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Battle);
        let panel = read::Battle { wave: Some((3, 3)), activity: Some(836_400.0), bonus: Some(51_100.0), next: None, percent: Some(15), raw: String::new() };
        s.battle_read(panel.clone(), Rgba { w: 4, h: 4, data: vec![0; 64] }, t + 3_000);
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(t + 9_000)), ModeChange::Left);
        let g = s.last_game().expect("the result card is kept").clone();
        assert_eq!((g.board, g.board_from, g.rounds, g.battle), (Some(836_400), Some(BoardFrom::Battle), 13, Some(panel)));
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1, "one folder for the whole game");
        let rows = &dirs[0];
        assert_eq!(kinds(rows, "vorax_resume").len(), 13, "12 board reopenings and the result page");
        let picks = kinds(rows, "vorax_round");
        assert_eq!(picks.len(), 13);
        for (k, row) in picks.iter().enumerate() {
            assert_eq!(row["round"], json!(k + 1));
            assert_eq!(row["after"].as_str(), Some(afters[k].as_str()), "round {}", k + 1);
            assert_eq!((&row["chosen"]["tankClicks"], &row["chosen"]["tanks"]), (&json!(1), &json!([1])), "round {}", k + 1);
            assert!(row["cues"].as_array().unwrap().contains(&json!("TankClick")), "round {}", k + 1);
        }
        assert!(kinds(rows, "vorax_mode").iter().any(|r| r["to"] == "battle"));
        let ends = kinds(rows, "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!((ends[0]["ultimate"].as_i64(), ends[0]["ultimateFrom"].as_str()), (Some(836_400), Some("battle")));
        assert_eq!(ends[0]["roundsSeen"], json!((1..=13).collect::<Vec<i32>>()));
    }

    /// The 2026-11-06 06:15 game, played with the game's UI scaled so the reader saw no board: every read was another
    /// screen with no round. The charge was spent on the intro page; then after a pick or a few the board page was
    /// closed and reopened to skip the animation (Main Destory, BackBtn, HomeBtn, Main Run; the two buttons are no
    /// cues), 19 picks in 12 stretches; at the end the result page (Reward Run), start surgery and the battle map.
    /// Before, every reopening opened a new folder and no ending was written. Now it is one game in one folder: every
    /// Main Destory pauses it, every Main Run takes it back, start surgery takes it back on its pick count (the result
    /// page alone does not, see by_picks), and its ending is written when the battle ends.
    #[test]
    fn a_game_never_read_keeps_one_folder() {
        use PageState::{Destory, Hide, Run};
        let root = std::env::temp_dir().join(format!("ascendant-vorax-unread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        assert_eq!(s.note_cue(page(Page::Intro, Run), Some(1_000)), ModeChange::Entered);
        feed(&mut s, other_screen(), 1_050);
        feed(&mut s, other_screen(), 1_100);
        s.note_cue(Cue::NewGame, Some(1_200));
        s.note_cue(page(Page::Intro, Hide), Some(1_220));
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(1_250)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Board);
        // Picks per stretch between two closings, as in the 12 folders that game was cut into.
        let stretches: [i64; 12] = [5, 1, 1, 1, 1, 2, 1, 1, 3, 1, 1, 1];
        let mut t = 2_000;
        for (k, picks) in stretches.into_iter().enumerate() {
            if k > 0 {
                assert_eq!(s.note_cue(page(Page::Main, Run), Some(t)), ModeChange::Entered, "stretch {k}");
                assert!(s.paused.is_none(), "stretch {k}: taken back");
            }
            feed(&mut s, other_screen(), t + 100);
            feed(&mut s, other_screen(), t + 160);
            for j in 0..picks {
                let at = t + 300 + 1_000 * j;
                // An instrument or protocol pick, or a potion used on a tank.
                let moves: &[Cue] = if j % 2 == 0 { &[Cue::CardClick, Cue::Confirm, Cue::Picked] } else { &[Cue::CardClick, Cue::TankClick { tank: Some(0) }, Cue::Confirm, Cue::PotionUsed] };
                for (n, c) in moves.iter().enumerate() {
                    s.note_cue(*c, Some(at + 10 * n as i64));
                }
                s.note_cue(Cue::Synced, Some(at + 200));
                feed(&mut s, other_screen(), at + 400);
                feed(&mut s, other_screen(), at + 460);
            }
            t += 300 + 1_000 * picks;
            assert_eq!(s.note_cue(page(Page::Main, Destory), Some(t)), ModeChange::Left, "stretch {k}");
            assert!(s.paused.is_some(), "stretch {k}: paused");
            t += 1_000;
        }
        assert_eq!(s.paused.as_ref().map(|p| (p.last_round, p.game.picks)), Some((0, 19)));
        // The result page alone does not take it back (its round is unknown); start surgery does.
        assert_eq!(s.note_cue(page(Page::Reward, Run), Some(t)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Off);
        assert!(s.paused.is_some());
        assert_eq!(s.note_cue(Cue::Fight, Some(t + 500)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Battle);
        assert!(s.paused.is_none());
        assert_eq!(s.note_cue(Cue::BattleEntered, Some(t + 3_000)), ModeChange::None);
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(t + 60_000)), ModeChange::Left);
        assert!(s.last_game().is_some(), "a game that went into its battle keeps its card");
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1, "one folder for the whole game");
        let rows = &dirs[0];
        let modes = kinds(rows, "vorax_mode");
        let mode_cues = |cue: &str| modes.iter().filter(|r| r["cue"] == cue).count();
        let pauses = kinds(rows, "vorax_pause");
        assert_eq!((pauses.len(), mode_cues("Page(Main, Destory)")), (12, 12), "a pause at every Main Destory");
        assert!(pauses.iter().all(|p| p["played"] == true && p["lastRound"] == 0), "{pauses:?}");
        let resumes = kinds(rows, "vorax_resume");
        let resumed_by = |cue: &str| resumes.iter().filter(|r| r["cue"] == cue).count();
        // The first Main Run comes from the intro page (a new game); the 11 others are reopenings.
        assert_eq!((resumed_by("Page(Main, Run)"), mode_cues("Page(Main, Run)")), (11, 12), "a resume at every reopening");
        let fight: Vec<&&Value> = resumes.iter().filter(|r| r["cue"] == "Fight").collect();
        assert_eq!(fight.len(), 1);
        assert_eq!((&fight[0]["lastRoundFrom"], &fight[0]["picks"]), (&json!("picks"), &json!(19)), "the round is an estimate");
        assert!(resumes.iter().filter(|r| r["cue"] != "Fight").all(|r| r["lastRoundFrom"].is_null()));
        assert_eq!(kinds(rows, "vorax_round").len(), 19);
        assert!(modes.iter().any(|r| r["to"] == "battle"));
        let ends = kinds(rows, "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!((&ends[0]["ultimate"], &ends[0]["roundsSeen"]), (&Value::Null, &json!([])));
        assert_eq!(kinds(rows, "vorax_battle_end").len(), 1);
    }

    /// A game never read (no round) paused after its picks; the entry then shows the result page of an older game,
    /// given up at once (Reward Run, GiveUp, Reward Destory), and, while the paused game has fewer picks than the
    /// last round needs, start surgery and the battle map: none of them takes it back or ends it. With picks enough
    /// for the last round, the result page and its give-up still do not. A restart finds it on disk with its picks,
    /// and the next Main Run takes it back in its folder.
    #[test]
    fn an_older_result_page_does_not_take_a_game_never_read() {
        use PageState::{Destory, Run};
        for picks in [2, i64::from(LAST_ROUND) + 2] {
            let (mut s, root) = logged(&format!("unread-stale-{picks}"));
            feed(&mut s, other_screen(), 1_100);
            feed(&mut s, other_screen(), 1_160);
            for j in 0..picks {
                let at = 2_000 + 1_000 * j;
                s.note_cue(Cue::CardClick, Some(at));
                s.note_cue(Cue::Confirm, Some(at + 100));
                s.note_cue(Cue::Synced, Some(at + 200));
            }
            let t = 2_000 + 1_000 * picks;
            assert_eq!(s.note_cue(page(Page::Main, Destory), Some(t)), ModeChange::Left, "{picks}");
            let mut stale = vec![(page(Page::Reward, Run), t + 1_000), (Cue::GiveUp, t + 1_200), (page(Page::Reward, Destory), t + 1_400)];
            if picks < i64::from(LAST_ROUND) {
                stale.extend([(Cue::Fight, t + 1_600), (Cue::BattleEntered, t + 1_800)]);
            }
            for (c, at) in stale {
                assert_eq!(s.note_cue(c, Some(at)), ModeChange::None, "{picks}: {c:?}");
                assert_eq!(s.mode(), Mode::Off, "{picks}: {c:?}");
            }
            assert!(s.paused.as_ref().is_some_and(|p| i64::from(p.game.picks) == picks), "{picks}: still paused");
            s.log().unwrap().wait();
            drop(s);
            let mut s = session();
            s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
            let p = s.paused.as_ref().expect("found on start");
            assert_eq!((i64::from(p.game.picks), p.game.played, p.last_round), (picks, true, 0));
            assert_eq!(s.note_cue(page(Page::Main, Run), Some(t + 5_000)), ModeChange::Entered, "{picks}");
            assert!(s.paused.is_none(), "{picks}");
            let dirs = all_rows(s, &root);
            assert_eq!(dirs.len(), 1, "{picks}: one folder");
            assert!(kinds(&dirs[0], "vorax_end").is_empty(), "{picks}: not ended");
            assert_eq!(kinds(&dirs[0], "vorax_resume").len(), 1, "{picks}");
        }
    }

    /// `picks` picks on a board read as another screen (no round), starting at `t`; returns the time after them.
    fn unread_picks(s: &mut VoraxSession, picks: i64, t: i64) -> i64 {
        feed(s, other_screen(), t);
        feed(s, other_screen(), t + 60);
        for j in 0..picks {
            let at = t + 100 + 1_000 * j;
            s.note_cue(Cue::CardClick, Some(at));
            s.note_cue(Cue::Confirm, Some(at + 100));
            s.note_cue(Cue::Synced, Some(at + 200));
        }
        t + 100 + 1_000 * picks
    }

    /// The reader lost the board partway: round 3 read, then 13 more picks with no round. Paused short of the last
    /// round by its rounds read, it is still taken back by start surgery on its picks (not by its result page), and
    /// ends in its battle with the rounds that were read.
    #[test]
    fn a_game_read_then_lost_is_taken_back_by_its_battle() {
        use PageState::{Destory, Run};
        let (mut s, root) = logged("read-then-lost");
        steady_round(&mut s, 3, 1_100);
        let t = unread_picks(&mut s, i64::from(LAST_ROUND), 2_000);
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(t)), ModeChange::Left);
        assert_eq!(s.paused.as_ref().map(|p| (p.last_round, p.game.picks)), Some((3, 13)));
        assert_eq!(s.note_cue(page(Page::Reward, Run), Some(t + 500)), ModeChange::None);
        assert!(s.paused.is_some(), "not by its result page");
        assert_eq!(s.note_cue(Cue::Fight, Some(t + 1_000)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Battle);
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(t + 9_000)), ModeChange::Left);
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1);
        let resumes = kinds(&dirs[0], "vorax_resume");
        assert_eq!((resumes.len(), &resumes[0]["cue"], &resumes[0]["lastRoundFrom"]), (1, &json!("Fight"), &json!("picks")));
        let ends = kinds(&dirs[0], "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["roundsSeen"], json!([3]));
    }

    /// The battle map (230043) with no start-surgery button seen takes a game never read back on its picks too.
    #[test]
    fn the_battle_map_alone_takes_a_game_never_read_back() {
        use PageState::Destory;
        let (mut s, root) = logged("unread-battle-map");
        let t = unread_picks(&mut s, i64::from(LAST_ROUND), 2_000);
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(t)), ModeChange::Left);
        assert_eq!(s.note_cue(Cue::BattleEntered, Some(t + 1_000)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Battle);
        assert!(s.paused.is_none());
        assert_eq!(s.note_cue(Cue::LevelLeft, Some(t + 9_000)), ModeChange::Left);
        let dirs = all_rows(s, &root);
        assert_eq!(dirs.len(), 1);
        let resumes = kinds(&dirs[0], "vorax_resume");
        assert_eq!((resumes.len(), &resumes[0]["cue"], &resumes[0]["lastRoundFrom"]), (1, &json!("BattleEntered"), &json!("picks")));
        assert_eq!(kinds(&dirs[0], "vorax_end").len(), 1);
    }

    /// A board opened and closed with nothing read and no move is paused (the skip-animation rule), but when the next
    /// game starts it is dropped with no ending: an ending with no rounds and no picks would say nothing.
    #[test]
    fn a_board_closed_unplayed_leaves_no_empty_ending() {
        use PageState::{Destory, Hide, Run};
        let (mut s, root) = logged("unplayed");
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(1_500)), ModeChange::Left);
        assert!(s.paused.as_ref().is_some_and(|p| !p.game.has_play()));
        s.note_cue(page(Page::Intro, Run), Some(2_000));
        s.note_cue(Cue::NewGame, Some(2_100));
        assert!(s.paused.is_none());
        s.note_cue(page(Page::Intro, Hide), Some(2_200));
        s.note_cue(page(Page::Main, Run), Some(2_300));
        steady_round(&mut s, 1, 2_500);
        let dirs = all_rows(s, &root);
        assert!(dirs.iter().flatten().all(|r| r["kind"] != "vorax_end"), "no empty ending");
    }

    /// A `vorax_pause` row written before `played` and `picks` existed reads back as before: not played, no picks,
    /// its rounds still making it a game under way.
    #[test]
    fn an_older_pause_row_reads_back() {
        let rows = [
            json!({"at": "2026-11-06T03:00:00.000Z", "kind": "vorax_mode", "from": "off", "to": "board", "cue": "Page(Main, Run)"}),
            json!({"at": "2026-11-06T03:01:00.000Z", "kind": "vorax_pause", "why": "Page(Main, Destory)", "lastRound": 4, "roundsSeen": [3, 4], "held": [[6, 1]]}),
        ];
        let p = paused_in(&rows).expect("paused");
        assert_eq!((p.last_round, p.game.played, p.game.picks, p.game.held.clone()), (4, false, 0, vec![(6, 1)]));
        assert!(p.game.has_play());
        assert!(!by_picks(&p));
        assert_eq!(p.at, parse_at("2026-11-06T03:01:00.000Z").unwrap());
    }

    /// A game given up after one round and ended by the next game's charge: its `vorax_end` (and the last pick, with no
    /// board after it) is written as usual, but it leaves no result card.
    #[test]
    fn a_game_given_up_early_has_no_card() {
        // its own folder: the give-up hint's test is logged("give-up") too, and tests run at once
        let (mut s, root) = logged("given-up-early");
        steady_round(&mut s, 1, 2_000);
        s.note_cue(Cue::Synced, Some(2_500));
        s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        assert!(s.paused.is_some());
        s.note_cue(Cue::NewGame, Some(9_000));
        assert!(s.paused.is_none());
        assert!(s.last_game().is_none(), "no card for a game of one round");
        let dirs = all_rows(s, &root);
        let ends = kinds(&dirs[0], "vorax_end");
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["roundsSeen"], json!([1]));
        assert_eq!(ends[0]["at"].as_str(), Some(timestamp(at_time(3_000)).as_str()), "stamped at its pause");
        let picks = kinds(&dirs[0], "vorax_round");
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0]["after"], Value::Null);
    }

    /// A game paused after its last round and ended by the next game's charge keeps its result card (the new game's
    /// intro page does not clear it); a Tartarus signal later clears it as before.
    #[test]
    fn the_game_ended_by_a_new_one_keeps_its_card() {
        let (mut s, root) = logged("new-keeps");
        steady_round(&mut s, LAST_ROUND, 2_000);
        s.note_cue(Cue::Synced, Some(2_500));
        s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        s.note_cue(page(Page::Intro, PageState::Run), Some(3_100));
        s.note_cue(Cue::NewGame, Some(3_200));
        let g = s.last_game().expect("kept").clone();
        assert_eq!((g.ended_at, g.rounds), (3_000, 1));
        s.drop_paused(3_300, true);
        assert!(s.last_game().is_none(), "Tartarus clears it as before");
        let dirs = all_rows(s, &root);
        assert_eq!(kinds(&dirs[0], "vorax_end").len(), 1);
        assert_eq!(kinds(&dirs[0], "vorax_round")[0]["after"], Value::Null);
    }

    /// A resolve strip still waiting when the board page closed is dropped on the resume: it would otherwise be the
    /// reopened page saved as a strip with no round.
    #[test]
    fn a_resume_drops_the_waiting_strip() {
        let (mut s, root) = logged("strip");
        steady_round(&mut s, 1, 2_000);
        s.configure(Config { learn: true, strips: true, ..s.config.clone() });
        s.note_cue(Cue::Tool, Some(2_500));
        assert!(s.learn.strip_pending());
        s.note_cue(page(Page::Main, PageState::Destory), Some(3_000));
        assert!(s.learn.strip_pending(), "kept while paused");
        s.note_cue(page(Page::Main, PageState::Run), Some(3_700));
        assert!(s.paused.is_none() && s.active());
        assert!(!s.learn.strip_pending());
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A potion board of round `round` whose cards have Traditional Chinese names (what the tooltip is matched
    /// against), with `tooltip` as the left tooltip; the first tank's quantity changes with the round, so each round
    /// has its own signature.
    fn named_board(round: i32, tooltip: &str) -> Reading {
        let mut r = Reading { round: Some(round), rerolls: Some(0), tooltip: tooltip.to_string(), ..skip_board(round) };
        for (c, zh) in r.cards.iter_mut().zip(["骨軟膏", "活性育卵激素", "消化酶溶液"]) {
            c.zh = Some(zh.to_string());
        }
        r
    }

    /// The card-pick animation and the targeting step read as a resolving screen.
    fn resolving_of(round: i32, tooltip: &str) -> Reading {
        Reading { kind: Kind::Resolving, cards: vec![], rerolls: None, ultimate: None, ..named_board(round, tooltip) }
    }

    /// The reopened board page before its cards are drawn.
    fn other_screen() -> Reading {
        Reading { kind: Kind::Other, round: None, cards: vec![], tanks: vec![], rerolls: None, ultimate: None, ..potion_board(false, None, 12) }
    }

    /// Close the board page and reopen it (skipping the scoring animation), then the next round's board: the reopened
    /// page reads as another screen first, then the next round's selection screen settles.
    fn skip_to(s: &mut VoraxSession, round: i32, at: i64) {
        use PageState::{Destory, Run};
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(at)), ModeChange::Left);
        assert!(s.paused.as_ref().is_some_and(|p| p.game.round_row.is_some()), "the pick waits in the paused game");
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(at + 800)), ModeChange::Entered);
        feed(s, other_screen(), at + 850);
        feed(s, named_board(round, ""), at + 1_300);
        feed(s, named_board(round, ""), at + 1_440);
    }

    /// The 2026-11-06 04:43 game, round 8 (log line 317): the round's potion board settled, the tooltip pointed at
    /// the first card, the card was clicked and the screen read as resolving (steady, before the confirm), then the
    /// tank click, Confirm, PotionUsed and 5619; the board page was closed and reopened to skip the animation. The
    /// pick's `before` and hand are the potion board's, not the resolving screen's, its card is the one the tooltip
    /// named, and it gets the next round's board as `after` once the page is back.
    #[test]
    fn a_resolving_read_before_the_confirm_is_not_the_board_of_the_pick() {
        let (mut s, root) = logged_zh("pick-resolving");
        s.note_cue(Cue::Gone, Some(1_500));
        s.note_cue(Cue::Changed, Some(1_510));
        let board = named_board(8, "2 / 2");
        feed(&mut s, board.clone(), 2_000);
        feed(&mut s, board.clone(), 2_140);
        // The tooltip names the third card, a potion that takes a target (the first, `骨軟膏`, takes none, so a tank
        // click would not fit it, see pick_fit).
        feed(&mut s, named_board(8, "消 化 酶 溶 液"), 2_700);
        s.note_cue(Cue::CardClick, Some(2_800));
        feed(&mut s, resolving_of(8, "消 化 酶 溶 液"), 2_920);
        for k in 0..5 {
            feed(&mut s, resolving_of(8, if k == 4 { "聖 者" } else { "" }), 3_060 + 150 * k);
        }
        assert_eq!(s.stable.as_ref().map(|st| st.reading.kind), Some(Kind::Resolving), "the resolving screen was read steady");
        s.note_cue(Cue::TankClick { tank: Some(1) }, Some(3_800));
        s.note_cue(Cue::Confirm, Some(3_900));
        s.note_cue(Cue::PotionUsed, Some(3_910));
        s.note_cue(Cue::Synced, Some(3_920));
        let pending = s.game.round_row.clone().expect("the pick");
        assert_eq!(pending["before"].as_str(), Some(signature(&board).as_str()));
        skip_to(&mut s, 9, 4_300);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        let row = picks[0];
        assert_eq!((&row["round"], &row["phase"]), (&json!(8), &json!("potion")));
        assert_eq!(row["before"].as_str(), Some(signature(&board).as_str()));
        assert_eq!(row["after"].as_str(), Some(signature(&named_board(9, "")).as_str()));
        assert_eq!(row["chosen"]["hand"], json!(hand(&board)));
        assert_eq!((&row["chosen"]["card"], &row["chosen"]["how"]), (&json!(2), &json!("tooltip")));
        assert_eq!((&row["chosen"]["tankClicks"], &row["chosen"]["tanks"]), (&json!(1), &json!([1])));
        assert_eq!(row["missing"], json!([]));
        assert_eq!(row["cues"], json!(["Gone", "Changed", "CardClick", "TankClick", "Confirm", "PotionUsed"]));
    }

    /// The 2026-11-06 04:43 game, round 2 (log line 116): the tooltip named the third card while the second card's
    /// name was covered (unread), then the steady hand had all three cards again. The pick's card is the third one,
    /// and a steady read of the same hand with a card unread does not replace the full hand as the pick's board.
    #[test]
    fn a_tooltip_seen_with_a_card_unread_still_names_the_pick() {
        let (mut s, root) = logged_zh("pick-tooltip");
        let full = named_board(2, "");
        let covered = |tooltip: &str| {
            let mut r = named_board(2, tooltip);
            (r.cards[1].en, r.cards[1].zh) = (None, None);
            r
        };
        feed(&mut s, full.clone(), 2_000);
        feed(&mut s, full.clone(), 2_100);
        feed(&mut s, named_board(2, "消 化 酶 溶 液"), 2_600);
        feed(&mut s, covered("消 化 酶 溶 液"), 2_750);
        feed(&mut s, covered("消 化 酶 溶 液"), 2_830);
        feed(&mut s, covered(""), 3_100);
        assert_eq!(s.game.choice.as_ref().map(|(sig, _)| sig.as_str()), Some(signature(&full).as_str()), "the full hand stays the pick's board");
        feed(&mut s, named_board(2, "腐"), 3_700);
        feed(&mut s, named_board(2, "腐"), 3_840);
        s.note_cue(Cue::Shuffle, Some(3_900));
        s.note_cue(Cue::CardClick, Some(3_950));
        s.note_cue(Cue::TankClick { tank: Some(0) }, Some(4_000));
        s.note_cue(Cue::Confirm, Some(4_100));
        s.note_cue(Cue::PotionUsed, Some(4_110));
        s.note_cue(Cue::Synced, Some(4_120));
        skip_to(&mut s, 3, 4_300);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        let row = picks[0];
        assert_eq!((&row["round"], &row["phase"]), (&json!(2), &json!("potion")));
        assert_eq!(row["before"].as_str(), Some(signature(&full).as_str()));
        assert_eq!(row["after"].as_str(), Some(signature(&named_board(3, "")).as_str()));
        assert_eq!(row["chosen"]["hand"], json!(hand(&full)));
        assert_eq!((&row["chosen"]["card"], &row["chosen"]["how"]), (&json!(2), &json!("tooltip")));
        assert_eq!(row["chosen"]["tanks"], json!([0]));
        assert_eq!(row["missing"], json!([]));
    }

    /// A session on another client language than Traditional Chinese with observe mode on and a log folder.
    fn english_observing(tag: &str) -> (VoraxSession, PathBuf) {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-samples-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        (s.require_zh, s.test_zh) = (true, Some(false));
        s.configure(Config { log_root: Some(root.clone()), observe: true, ..s.config.clone() });
        (s, root)
    }

    /// Every file under `dir` whose name starts with `prefix`.
    fn files_named(dir: &std::path::Path, prefix: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_named(&p, prefix));
            } else if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(prefix)) {
                out.push(p);
            }
        }
        out
    }

    /// Private builds only, entered as the game log enters it: an English client with observe mode on enters the
    /// game to save frames and reads none (no `vorax_read`, no advice), the note shows under NOT_ZH; the surgery
    /// page closed and opened again (the skip-animation pattern, a pause and a resume) keeps the game's counts and
    /// its last frame, so the same screen is not saved again; a deal makes the next new screen a key frame.
    #[cfg(all(feature = "dev-samples", not(feature = "en-vorax")))]
    #[test]
    fn samples_save_frames_and_read_nothing() {
        use PageState::{Destory, Run};
        let (mut s, root) = english_observing("on");
        let note = |s: &VoraxSession| s.view().blocked.as_deref().is_some_and(|b| b.contains(NOT_ZH) && b.contains(samples::NOTE));
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(1_000)), ModeChange::Entered);
        assert!(note(&s), "{:?}", s.view().blocked);
        let frame = |v: u8| Rgba { w: 64, h: 36, data: vec![v; 64 * 36 * 4] };
        s.read(frame(10), 2_000, "auto");
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(2_200)), ModeChange::Left);
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(2_400)), ModeChange::Entered);
        assert!(note(&s), "the note again after the resume");
        s.read(frame(10), 4_500, "auto");
        s.read(frame(200), 4_600, "auto");
        s.board_cue(Cue::Dealt, 5_000);
        s.read(frame(120), 5_300, "auto");
        s.read(frame(60), 5_700, "auto");
        // A confirm closes the gate: the resolve animation is neither saved nor counted, until the deal opens it
        // (the times as a real confirm: the deal 1 s after it).
        s.board_cue(Cue::Synced, 6_000);
        assert!(s.gate_closed(6_700));
        s.read(frame(90), 6_700, "auto");
        s.board_cue(Cue::Dealt, 7_000);
        assert!(!s.gate_closed(7_700));
        s.read(frame(150), 7_700, "auto");
        assert!(s.view().advice.is_none());
        s.log().unwrap().wait();
        assert_eq!(files_named(&root, samples::PREFIX).len(), 4);
        let rows = rows_of(s, &root);
        assert!(kinds(&rows, "vorax_read").is_empty() && kinds(&rows, "vorax_advice").is_empty());
        let saved = kinds(&rows, samples::EVENT);
        let got: Vec<(&str, i64)> = saved.iter().map(|r| (r["why"].as_str().unwrap_or(""), r["index"].as_i64().unwrap_or(0))).collect();
        // 2.0 s the first frame; after the resume 4.5 s is the same screen as the last saved (not again), 4.6 s a new
        // one 2.6 s after it (index 2: the count went on); 5.3 s too early after the deal and inside the spacing;
        // 5.7 s the hand; 6.7 s inside the gate (not saved, though the confirm's hand was due); 7.7 s the next hand.
        assert_eq!(got, [("interval", 1), ("interval", 2), ("hand", 3), ("hand", 4)]);
        // Observe modes off: not entered, nothing saved.
        let (mut s, root) = english_observing("off");
        s.configure(Config { observe: false, learn: false, ..s.config.clone() });
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(1_000)), ModeChange::None);
        assert_eq!(s.view().blocked.as_deref(), Some(NOT_ZH));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A game on the board with a finished game's picks and no round read, closed after its last confirm: paused.
    fn paused_with_its_picks(s: &mut VoraxSession) {
        use PageState::{Destory, Run};
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(1_000)), ModeChange::Entered);
        s.game.picks = LAST_ROUND as u32;
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(2_000)), ModeChange::Left);
        assert_eq!(s.paused.as_ref().map(|p| (p.last_round, p.game.picks)), Some((0, LAST_ROUND as u32)));
    }

    /// Private builds only, as 2026-11-07_14-43-32: collecting samples, the board closed after the last confirm and
    /// the result page 0.67 s on takes the paused game back into Result (no round is read, its picks tell), so the
    /// page is saved; start surgery goes on into the battle. Not with en-vorax: an English client is then read for
    /// advice instead of collecting samples.
    #[cfg(all(feature = "dev-samples", not(feature = "en-vorax")))]
    #[test]
    fn collecting_samples_the_result_page_takes_the_game_back() {
        let (mut s, root) = english_observing("reward");
        paused_with_its_picks(&mut s);
        assert!(s.samples.is_some());
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Run), Some(2_670)), ModeChange::Entered);
        assert_eq!(s.mode(), Mode::Result);
        assert!(s.paused.is_none(), "taken back");
        assert!(s.samples.is_some(), "still collecting");
        assert_eq!(s.note_cue(Cue::Fight, Some(4_200)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Battle);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Not collecting samples (a Traditional Chinese client, any build), as before: the result page alone does not
    /// take back a paused game whose round is unknown.
    #[test]
    fn not_collecting_samples_the_result_page_alone_takes_nothing_back() {
        let mut s = session();
        paused_with_its_picks(&mut s);
        #[cfg(feature = "dev-samples")]
        assert!(s.samples.is_none());
        assert_eq!(s.note_cue(page(Page::Reward, PageState::Run), Some(2_670)), ModeChange::None);
        assert_eq!(s.mode(), Mode::Off);
        assert!(s.paused.is_some());
    }

    /// Without the feature (--no-default-features; the public build before 1.1.0), as in 1.0.2: an English client with observe mode on and a log folder
    /// does not enter the game, shows NOT_ZH alone, and saves nothing.
    /// With `en-vorax` (the default since 1.1.0): an English client with observe mode on and a log folder enters the game to
    /// be read (the English client's reading, vorax::read::english), with no note and no samples; a Traditional
    /// Chinese one after it is read as the Traditional Chinese client again.
    #[cfg(feature = "en-vorax")]
    #[test]
    fn an_english_client_is_read_with_en_vorax() {
        use PageState::{Destory, Run};
        let (mut s, root) = english_observing("en-vorax");
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(1_000)), ModeChange::Entered);
        assert!(s.active() && s.view().blocked.is_none(), "{:?}", s.view().blocked);
        assert!(crate::vorax::read::english());
        #[cfg(feature = "dev-samples")]
        assert!(s.samples.is_none());
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(2_000)), ModeChange::Left);
        s.test_zh = Some(true);
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(3_000)), ModeChange::Entered);
        assert!(!crate::vorax::read::english());
        crate::vorax::read::set_english_here(None);
        assert!(files_named(&root, "sample").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(not(any(feature = "dev-samples", feature = "en-vorax")))]
    #[test]
    fn an_english_client_saves_nothing_without_the_feature() {
        use PageState::Run;
        let (mut s, root) = english_observing("public");
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(1_000)), ModeChange::None);
        assert_eq!(s.view().blocked.as_deref(), Some(NOT_ZH));
        s.read(Rgba { w: 64, h: 36, data: vec![10; 64 * 36 * 4] }, 2_000, "auto");
        assert!(!s.active());
        assert!(files_named(&root, "sample").is_empty());
        let logs = files_named(&root, "log.jsonl");
        assert!(logs.iter().all(|l| std::fs::read_to_string(l).is_ok_and(|t| !t.contains("vorax_sample"))), "{logs:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The tooltip's card held in place of a raised card (the new icon has no template) leaves a `vorax_pick_fallback`
    /// row; a raised card does not.
    #[test]
    fn the_hovered_fallback_is_logged() {
        let (mut s, root) = logged("pick-fallback");
        let unknown = read::Held { id: None, en: None, zh: None, opening: false, score: 0.4 };
        let r = Reading { round: Some(5), held: vec![unknown], ..potion_board(false, Some(600), 12) };
        s.game.pending_pick = Some(PendingPick { before: 0, round: 5, card: Value::Null, id: None, hovered: Some(20) });
        s.check_pick(&r, 1_000);
        s.game.pending_pick = Some(PendingPick { before: 0, round: 5, card: Value::Null, id: Some(19), hovered: Some(20) });
        s.check_pick(&r, 2_000);
        assert_eq!(s.game.held, vec![(20, 5), (19, 5)]);
        let rows = rows_of(s, &root);
        let fallback = kinds(&rows, "vorax_pick_fallback");
        assert_eq!(fallback.len(), 1);
        assert_eq!((&fallback[0]["hovered"], &fallback[0]["round"]), (&json!(20), &json!(5)));
    }

    /// 2026-11-06_20-03-09 round 4 through the reads (`steady`): `髒污刮骨刀` (17) hovered, its neighbour
    /// `鞣製皮革拘束帶` (21) read raised, and the icon 17 shows on the next board. The icon is held and the pick's
    /// row, written on that same read, names the hovered card: the icon is checked before the row is written.
    #[test]
    fn an_icon_agreeing_with_the_hover_names_the_instrument_pick() {
        let (mut s, root) = logged_zh("pick-icon");
        let held = |ids: &[u8]| ids.iter().map(|&id| read::Held { id: Some(id), en: None, zh: None, opening: false, score: 1.0 }).collect::<Vec<_>>();
        let screen = |tooltip: &str, raised: Option<usize>| {
            let mut r = Reading { kind: Kind::Instrument, round: Some(4), rerolls: Some(0), tooltip: tooltip.to_string(), held: held(&[2, 14]), ..potion_board(false, Some(600), 12) };
            for (k, (c, (en, zh, id))) in r.cards.iter_mut().zip([("Rawhide Restraint", "生皮革拘束帶", 13), ("Fouled Scraper", "髒污刮骨刀", 17), ("Tanned Leather Restraint", "鞣製皮革拘束帶", 21)]).enumerate() {
                (c.en, c.zh, c.id, c.how, c.category) = (Some(en.into()), Some(zh.into()), Some(id), Some(read::How::Name), crate::vorax::names::Category::Instrument);
                c.raised = raised == Some(k);
            }
            r
        };
        s.game.held = vec![(2, 1), (14, 3)];
        feed(&mut s, screen("", None), 2_000);
        feed(&mut s, screen("", None), 2_100);
        feed(&mut s, screen("髒 污 刮 骨 刀", None), 2_600);
        feed(&mut s, screen("髒 污 刮 骨 刀", Some(2)), 2_700);
        feed(&mut s, screen("髒 污 刮 骨 刀", Some(2)), 2_800);
        for (cue, at) in [(Cue::CardClick, 2_850), (Cue::Confirm, 2_900), (Cue::Picked, 2_910), (Cue::Synced, 2_920)] {
            s.note_cue(cue, Some(at));
        }
        let next = Reading { round: Some(5), held: held(&[2, 14, 17]), ..potion_board(false, Some(700), 12) };
        feed(&mut s, next.clone(), 3_300);
        feed(&mut s, next, 3_400);
        assert_eq!(s.game.held, vec![(2, 1), (14, 3), (17, 5)]);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        assert_eq!((&picks[0]["phase"], &picks[0]["chosen"]["card"], &picks[0]["chosen"]["how"], &picks[0]["missing"]), (&json!("instrument"), &json!(1), &json!("icon"), &json!([])));
        let mismatch = kinds(&rows, "vorax_pick_mismatch");
        assert_eq!((&mismatch[0]["card"], &mismatch[0]["icon"], &mismatch[0]["kept"]), (&json!(21), &json!(17), &json!("icon")));
    }

    /// The English client's hover agrees with the icon too (2026-11-08_08-45-58 round 2): `Vermicular Spinal Cord` (12)
    /// picked and its icon shown, the mouse then over `Pig Iron Bone Saw` (15), read raised while the tooltip still named
    /// the pick. The tooltip was matched on the Traditional Chinese names only, so `hovered` was null, the raised card won
    /// and the wrong instrument was held for the rest of the run.
    #[cfg(feature = "en-vorax")]
    #[test]
    fn an_english_tooltip_names_the_hovered_instrument_too() {
        let (mut s, root) = logged("pick-icon-en");
        s.test_zh = Some(false);
        crate::vorax::read::set_english_here(Some(true));
        let names = crate::vorax::names::names();
        let index = |en: &str| names.cards.iter().position(|n| n.printed_en() == en).unwrap_or_else(|| panic!("{en}"));
        let held = |ids: &[u8]| ids.iter().map(|&id| read::Held { id: Some(id), en: None, zh: None, opening: false, score: 1.0 }).collect::<Vec<_>>();
        let screen = |tooltip: &str, raised: Option<usize>| {
            let mut r = Reading { kind: Kind::Instrument, round: Some(2), rerolls: Some(0), tooltip: tooltip.to_string(), held: held(&[7]), ..potion_board(false, Some(300), 12) };
            for (k, (c, (en, id))) in r.cards.iter_mut().zip([("Vermicular Spinal Cord", 12), ("Pig Iron Bone Saw", 15)]).enumerate() {
                let i = index(en);
                (c.en, c.zh, c.id, c.index, c.how, c.category) = (Some(en.into()), names.cards[i].zh.clone(), Some(id), Some(i), Some(read::How::Name), crate::vorax::names::Category::Instrument);
                c.raised = raised == Some(k);
            }
            r.cards.truncate(2);
            r
        };
        assert_eq!(hovered_card(&screen("Vermicular Spinal Cord", None)), Some(0));
        s.game.held = vec![(7, 1)];
        feed(&mut s, screen("", None), 2_000);
        feed(&mut s, screen("", None), 2_100);
        feed(&mut s, screen("Vermicular Spinal Cord", Some(0)), 2_600);
        feed(&mut s, screen("Vermicular Spinal Cord", Some(0)), 2_700);
        for (cue, at) in [(Cue::CardClick, 2_750), (Cue::Confirm, 2_800), (Cue::Picked, 2_810)] {
            s.note_cue(cue, Some(at));
        }
        feed(&mut s, screen("Vermicular Spinal Cord", Some(1)), 2_900);
        s.note_cue(Cue::Synced, Some(2_920));
        let next = Reading { round: Some(3), held: held(&[7, 12]), ..potion_board(false, Some(400), 12) };
        feed(&mut s, next.clone(), 3_300);
        feed(&mut s, next, 3_400);
        crate::vorax::read::set_english_here(None);
        assert_eq!(s.game.held, vec![(7, 1), (12, 3)]);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        assert_eq!(&picks[0]["chosen"]["card"], &json!(0), "{:?}", picks[0]);
        for m in kinds(&rows, "vorax_pick_mismatch") {
            assert_eq!(&m["kept"], &json!("icon"), "{m:?}");
        }
    }

    /// The card the tooltip named at the card click names the pick when the mouse then goes to a tank and a monster's
    /// tooltip drops the hover (2026-11-06_20-03-09 round 10, log lines 415 to 432: `孿生激素-蠱蟲` hovered and
    /// clicked, then `暴食巨蠕蟲`, and the row's card was null). It is looked up at the confirm: a frame taken before
    /// the click may come out of OCR after the click's line, and a card hovered only after the click is not it.
    #[test]
    fn the_card_hovered_at_the_click_names_the_pick() {
        let (mut s, root) = logged_zh("pick-clicked");
        let full = named_board(2, "");
        let monster = || Reading { panel_covered: true, ..named_board(2, "暴 食 巨 蠕 蟲") };
        feed(&mut s, full.clone(), 2_000);
        feed(&mut s, full.clone(), 2_100);
        feed(&mut s, named_board(2, "骨 軟 膏"), 2_600);
        s.note_cue(Cue::CardClick, Some(2_800));
        // Taken at 2.79 s, before the click, read after its line came in.
        feed(&mut s, named_board(2, "消 化 酶 溶 液"), 2_790);
        // On the way to the tank the mouse crosses the first card, then rests on a monster.
        feed(&mut s, named_board(2, "骨 軟 膏"), 2_900);
        feed(&mut s, monster(), 3_050);
        feed(&mut s, monster(), 3_200);
        assert!(s.game.hovered.is_none(), "the monster's tooltip drops the hover");
        s.note_cue(Cue::TankClick { tank: Some(0) }, Some(3_300));
        s.note_cue(Cue::Confirm, Some(3_400));
        s.note_cue(Cue::PotionUsed, Some(3_410));
        s.note_cue(Cue::Synced, Some(3_420));
        skip_to(&mut s, 3, 4_300);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        assert_eq!((&picks[0]["chosen"]["card"], &picks[0]["chosen"]["how"], &picks[0]["missing"]), (&json!(2), &json!("clicked"), &json!([])));
    }

    /// The card raised on the last read is checked like the others (2026-11-06_04-43-15 round 6, log line 271): the
    /// first card, `骨軟膏` (no target), is raised on the last read, yet a tank was clicked; the tooltip's third card
    /// (`消化酶溶液`, one target) fits and is the pick.
    #[test]
    fn a_card_raised_on_the_last_read_must_fit_the_pick() {
        let (mut s, root) = logged_zh("pick-raised-checked");
        let full = named_board(2, "");
        feed(&mut s, full.clone(), 2_000);
        feed(&mut s, full.clone(), 2_100);
        feed(&mut s, named_board(2, "消 化 酶 溶 液"), 2_600);
        let mut raised = named_board(2, "消 化 酶 溶 液");
        raised.cards[0].raised = true;
        feed(&mut s, raised.clone(), 2_800);
        feed(&mut s, raised, 2_940);
        s.note_cue(Cue::CardClick, Some(3_000));
        s.note_cue(Cue::TankClick { tank: Some(0) }, Some(3_100));
        s.note_cue(Cue::Confirm, Some(3_200));
        s.note_cue(Cue::PotionUsed, Some(3_210));
        s.note_cue(Cue::Synced, Some(3_220));
        skip_to(&mut s, 3, 4_300);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        let row = picks[0];
        assert_eq!(row["after"].as_str(), Some(signature(&named_board(3, "")).as_str()));
        assert_eq!((&row["chosen"]["card"], &row["chosen"]["how"], &row["missing"]), (&json!(2), &json!("tooltip"), &json!([])));
    }

    /// The 2026-11-06_13-46-25 game, round 1 (log lines 16 to 27): the tooltip named the card, then three reads of
    /// `0 / 2` matched no card; the hover is kept and the pick names the card. A read of another hand drops it.
    #[test]
    fn a_tooltip_matching_no_card_keeps_the_hover() {
        let (mut s, root) = logged_zh("pick-hover-kept");
        let full = named_board(2, "");
        feed(&mut s, full.clone(), 2_000);
        feed(&mut s, full.clone(), 2_100);
        feed(&mut s, named_board(2, "消 化 酶 溶 液"), 2_600);
        for k in 0..3 {
            feed(&mut s, named_board(2, "0 / 2"), 2_800 + 150 * k);
        }
        assert_eq!(s.game.hovered.as_ref().map(|h| h.1), Some(2), "`0 / 2` names no card: the hover stays");
        s.note_cue(Cue::CardClick, Some(3_500));
        s.note_cue(Cue::TankClick { tank: Some(0) }, Some(3_600));
        s.note_cue(Cue::Confirm, Some(3_700));
        s.note_cue(Cue::PotionUsed, Some(3_710));
        s.note_cue(Cue::Synced, Some(3_720));
        skip_to(&mut s, 3, 4_300);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!((&picks[0]["chosen"]["card"], &picks[0]["chosen"]["how"], &picks[0]["missing"]), (&json!(2), &json!("tooltip"), &json!([])));

        // hovered_kept alone: a matching tooltip replaces, text with no tooltip shown keeps, a shown tooltip naming
        // no card (a monster's) drops, another hand or round drops.
        let seen = seen_card(&named_board(2, "消 化 酶 溶 液"), true);
        assert_eq!(seen.as_ref().map(|h| h.1), Some(2));
        assert_eq!(hovered_kept(seen.clone(), &named_board(2, "0 / 2")).map(|h| h.1), Some(2));
        assert_eq!(hovered_kept(seen.clone(), &named_board(2, "")).map(|h| h.1), Some(2));
        let monster = Reading { panel_covered: true, ..named_board(2, "朿 獸") };
        assert!(hovered_kept(seen.clone(), &monster).is_none(), "the mouse went to a tank");
        assert_eq!(hovered_kept(seen.clone(), &named_board(2, "骨 軟 膏")).map(|h| h.1), Some(0));
        assert!(hovered_kept(seen.clone(), &named_board(3, "0 / 2")).is_none(), "another round");
        let mut other = named_board(2, "0 / 2");
        other.cards[0].en = Some("Xeno-Hormone".into());
        assert!(hovered_kept(seen.clone(), &other).is_none(), "another hand");
        // A card unread in this read is not another hand.
        let mut covered = named_board(2, "0 / 2");
        covered.cards[0].en = None;
        assert_eq!(hovered_kept(seen, &covered).map(|h| h.1), Some(2));
    }

    /// A reroll picks no card: the 2026-11-06_13-46-25 game's round 3 reroll row (log line 85) had the card hovered
    /// before it, with `how` tooltip.
    #[test]
    fn a_reroll_row_names_no_card() {
        let (mut s, root) = logged("reroll-no-card");
        let full = named_board(2, "");
        feed(&mut s, full.clone(), 2_000);
        feed(&mut s, full.clone(), 2_100);
        feed(&mut s, named_board(2, "消 化 酶 溶 液"), 2_600);
        let mut raised = named_board(2, "消 化 酶 溶 液");
        raised.cards[2].raised = true;
        feed(&mut s, raised, 2_700);
        s.note_cue(Cue::Shuffle, Some(3_000));
        s.note_cue(Cue::Reroll { instrument: false }, Some(3_010));
        s.note_cue(Cue::Synced, Some(3_020));
        let mut next = named_board(2, "");
        next.rerolls = Some(0);
        next.cards[0].en = Some("Xeno-Hormone".into());
        feed(&mut s, next.clone(), 3_400);
        feed(&mut s, next, 3_500);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0]["phase"], json!("reroll"));
        assert_eq!((&picks[0]["chosen"]["card"], &picks[0]["chosen"]["how"], &picks[0]["missing"]), (&Value::Null, &Value::Null, &json!([])));
    }

    /// [`pick_fit`] says the card fits ("cannot tell" and "does not fit" are both no).
    fn pick_fits(en: Option<&str>, cues: &[String], tanks: &[Option<u8>], before: Option<&str>, after: Option<&str>) -> bool {
        pick_fit(en, cues, tanks, before, after) == Some(true)
    }

    /// No card seen picked: the one card of the hand the rules allow is the pick (`fit`); two that fit, a card of
    /// unknown targeting in the hand, or a card clicked after the last tank click leave it unknown.
    #[test]
    fn the_one_card_the_rules_allow_is_the_pick() {
        let row = |hand: &[&str], cues: &[&str], tanks: &[Option<u8>]| {
            let mut r = json!({ "round": 1, "phase": "potion", "cues": cues, "missing": [], "_pick": [], "chosen": { "card": null, "how": null, "hand": hand, "tanks": tanks } });
            settle_pick(&mut r, None);
            (r.pointer("/chosen/card").and_then(Value::as_u64), r.pointer("/chosen/how").and_then(Value::as_str).map(str::to_string))
        };
        // 2026-11-08_18-41-59 round 1: confirmed with no potion used, the kit the only Box card
        let kit = ["Rare Potion Kit (Small)", "Probiotic Enzyme Solution", "Potent Xenopurge Powder"];
        assert_eq!(row(&kit, &["CardClick", "CardClick", "Confirm"], &[]), (Some(0), Some("fit".into())));
        // one tank clicked, one Select1 card among Auto ones (18-51-19 round 1)
        let one = ["Slender-Limb Powder - Bone Sentry", "Gray Matter Spinal Solution", "Twin Hormone - Pest"];
        assert_eq!(row(&one, &["CardClick", "TankClick", "Confirm", "PotionUsed"], &[Some(1)]), (Some(2), Some("fit".into())));
        // a card clicked after the tank: maybe another card taken (2026-11-06_16-22-21 round 9)
        let changed = ["Spinal Solution - Agito", "Digestive Enzyme Solution", "Brain Fog Tincture"];
        assert_eq!(row(&changed, &["CardClick", "TankClick", "CardClick", "Confirm", "PotionUsed"], &[Some(0)]), (None, None));
        // two Auto cards with no tank clicked: either
        let two = ["Probiotic Enzyme Solution", "Spinal Solution - Aemberon", "Slender-Limb Powder - Pest"];
        assert_eq!(row(&two, &["CardClick", "Confirm", "PotionUsed"], &[]), (None, None));
        // a card not in the potion table: its targeting unknown, nothing told
        assert_eq!(row(&["Rare Potion Kit (Small)", "Not A Potion", "Probiotic Enzyme Solution"], &["CardClick", "Confirm"], &[]), (None, None));
    }

    /// The tank clicks and the board after a potion pick against its card ([`pick_fit`], [`settle_pick`]), on the
    /// two picks that were recorded wrong.
    #[test]
    fn a_potion_pick_must_fit_its_tank_clicks_and_board() {
        let names = |v: &[&str]| v.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        // 2026-11-06_12-23-50 round 6 (log line 237): `Spinal Solution - Aemberon` (no target) was raised earlier, but
        // tank 1 was clicked twice, and it turned Pest with a new Pest group beside it: the suggested `Twin Hormone -
        // Pest` (the replay over the log finds no tooltip on it, so the row's card becomes null).
        let b6 = "Potion|Some(6)|Some(1)|Some(BoneSentry),Some(Rare),Some(194),Some(1162);_;_;_;_;_;|Twin Hormone - Pest;Spinal Solution - Aemberon;Anesthetic Tincture - Agito;|";
        let a6 = "Potion|Some(7)|Some(1)|Some(Pest),Some(Magic),Some(194),Some(1252);Some(Pest),Some(Magic),Some(5),Some(114);Some(BoneSentry),Some(Magic),Some(5),Some(69);_;_;_;|Bewitching Tincture;Anesthetic Tincture - Aemberon;Twin Hormone - Pest;|";
        let c6 = names(&["Gone", "Shuffle", "Dealt", "CardClick", "TankClick", "CardClick", "CardClick", "TankClick", "Confirm", "PotionUsed"]);
        let t6 = [Some(0), Some(0)];
        assert!(!pick_fits(Some("Spinal Solution - Aemberon"), &c6, &t6, Some(b6), Some(a6)));
        assert!(!pick_fits(Some("Spinal Solution - Aemberon"), &c6, &t6, Some(b6), None), "no target, yet tank clicks");
        // A card click after the tank click keeps the target (2026-11-04_11-33-57 round 2).
        assert!(pick_fits(Some("Bewitching Tincture"), &names(&["Shuffle", "CardClick", "TankClick", "CardClick", "Confirm", "PotionUsed"]), &[Some(2)], None, None));
        assert!(pick_fits(Some("Twin Hormone - Pest"), &c6, &t6, Some(b6), Some(a6)));
        // No card click seen (no game log): the clicks say nothing.
        assert!(pick_fits(Some("Spinal Solution - Aemberon"), &names(&["Confirm", "PotionUsed"]), &[], Some(b6), Some(a6)));
        // A kit is confirmed with no potion used, any other potion with one (2026-11-03_12-34-42 round 5: the
        // potion raised earlier was not the pick, the kit was).
        assert!(pick_fits(Some("Potion Kit (Small)"), &names(&["CardClick", "Confirm"]), &[], Some(b6), None));
        assert!(!pick_fits(Some("Potion Kit (Small)"), &names(&["CardClick", "Confirm", "PotionUsed"]), &[], Some(b6), None));
        assert!(!pick_fits(Some("Viviparous Hormone"), &names(&["CardClick", "Confirm"]), &[], Some(b6), None));
        // 2026-11-06_13-46-25 round 3 (log line 107): `Pure Leech Solution` (one target) was raised earlier, but tanks
        // 3 and 1 were clicked and both changed: the suggested `Xeno-Hormone` (one or two targets) on tanks 1 and 3.
        let b3 = "Potion|Some(3)|Some(1)|Some(BoneSentry),Some(Normal),Some(1),Some(81);Some(Pest),Some(Normal),Some(15),Some(57);Some(Aemberon),Some(Normal),Some(1),Some(81);Some(Pest),Some(Rare),Some(15),Some(130);Some(Pest),Some(Normal),Some(1),Some(81);_;|Pure Leech Solution;Green Bile Solution;Xeno-Hormone;|Some(2);";
        let a3 = "Instrument|Some(3)|Some(2)|Some(Pest),Some(Magic),Some(10),Some(159);Some(Pest),Some(Normal),Some(15),Some(192);Some(BoneSentry),Some(Normal),Some(6),Some(171);Some(Pest),Some(Rare),Some(15),Some(265);Some(Pest),Some(Normal),Some(1),Some(216);Some(BoneSentry),Some(Normal),Some(6),Some(171);|Sinew Garrote;Vermicular Spinal Cord;Tanned Leather Restraint;|Some(2);";
        let c3 = names(&["Shuffle", "CardClick", "TankClick", "TankClick", "Confirm", "PotionUsed"]);
        let t3 = [Some(2), Some(0)];
        assert!(!pick_fits(Some("Pure Leech Solution"), &c3, &t3, Some(b3), Some(a3)));
        assert!(!pick_fits(Some("Pure Leech Solution"), &c3, &t3, Some(b3), None), "two tanks, no board after to call it a changed mind");
        assert!(pick_fits(Some("Xeno-Hormone"), &c3, &t3, Some(b3), Some(a3)));
        // A changed mind: two tanks clicked for a one-target potion, only one of them changed.
        let a3one = a3.replacen("Some(Pest),Some(Magic),Some(10),Some(159)", "Some(BoneSentry),Some(Normal),Some(1),Some(81)", 1);
        assert!(pick_fits(Some("Pure Leech Solution"), &c3, &t3, Some(b3), Some(&a3one)));
        // A one-target potion with no tank click does not fit; an unread card or a card that is no potion fits.
        assert!(!pick_fits(Some("Pure Leech Solution"), &names(&["CardClick", "Confirm"]), &[], Some(b3), Some(a3)));
        assert!(pick_fits(None, &c3, &t3, Some(b3), Some(a3)));
        assert!(pick_fits(Some("Incubation Sac"), &c3, &t3, Some(b3), Some(a3)));
        // A potion kit takes no tank click.
        assert!(pick_fits(Some("Potion Kit (Small)"), &names(&["CardClick", "Confirm"]), &[], Some(b3), None));

        // settle_pick: the earlier card does not fit, the tooltip's does; neither fits; no candidates (raised).
        let row = |card: usize, how: &str, pick: Value| {
            json!({ "before": b3, "cues": c3, "missing": [], "chosen": { "card": card, "how": how, "hand": ["Pure Leech Solution", "Green Bile Solution", "Xeno-Hormone"], "tankClicks": 2, "tanks": [2, 0] }, "_pick": pick })
        };
        let mut r = row(0, "raisedEarlier", json!([[0, "raisedEarlier"], [2, "tooltip"]]));
        settle_pick(&mut r, Some(a3));
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"], &r["missing"], r.get("_pick")), (&json!(2), &json!("tooltip"), &json!([]), None));
        // what it was settled from: the earlier card the rules ruled out, then the tooltip's they allow
        assert_eq!(r["chosen"]["basis"], json!([[0, "raisedEarlier", false], [2, "tooltip", true]]));
        let mut r = row(0, "raisedEarlier", json!([[0, "raisedEarlier"]]));
        settle_pick(&mut r, Some(a3));
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"], &r["missing"]), (&Value::Null, &Value::Null, &json!(["card"])));
        let mut r = row(2, "raisedEarlier", json!([[2, "raisedEarlier"], [0, "tooltip"]]));
        settle_pick(&mut r, Some(a3));
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"]), (&json!(2), &json!("raisedEarlier")));
        let mut r = row(0, "raised", Value::Null);
        r.as_object_mut().unwrap().remove("_pick");
        settle_pick(&mut r, Some(a3));
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"]), (&json!(0), &json!("raised")));
        // The card raised on the last read is checked like the others: when it does not fit, the next candidate is
        // tried; when it fits, it stays.
        let mut r = row(0, "raised", json!([[0, "raised"], [2, "tooltip"]]));
        settle_pick(&mut r, Some(a3));
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"], &r["missing"]), (&json!(2), &json!("tooltip"), &json!([])));
        let mut r = row(2, "raised", json!([[2, "raised"], [0, "raisedEarlier"]]));
        settle_pick(&mut r, Some(a3));
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"]), (&json!(2), &json!("raised")));
        // Two tanks for a one-target potion and no board after: cannot be told. The card raised on the last read
        // stays; one raised earlier gives way to a card that fits.
        assert_eq!(pick_fit(Some("Pure Leech Solution"), &c3, &t3, Some(b3), None), None);
        assert_eq!(pick_fit(Some("Pure Leech Solution"), &c3, &t3, Some(b3), Some(a3)), Some(false));
        let mut r = row(0, "raised", json!([[0, "raised"], [2, "tooltip"]]));
        settle_pick(&mut r, None);
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"]), (&json!(0), &json!("raised")));
        let mut r = row(0, "raisedEarlier", json!([[0, "raisedEarlier"], [2, "tooltip"]]));
        settle_pick(&mut r, None);
        assert_eq!((&r["chosen"]["card"], &r["chosen"]["how"]), (&json!(2), &json!("tooltip")));
    }

    /// A minimal selection screen read rebuilt from a logged `vorax_read` row: kind, round, rerolls, the cards' names,
    /// raised flags and the tooltip (what the pick tracking looks at).
    fn reading_of_row(v: &Value) -> Option<Reading> {
        let r = v.get("reading")?;
        let kind = match r["kind"].as_str()? {
            "potion" => Kind::Potion,
            "instrument" => Kind::Instrument,
            "protocol" => Kind::Protocol,
            _ => return None,
        };
        let mut base = potion_board(false, None, 12);
        base.kind = kind;
        base.round = r["round"].as_i64().map(|x| x as i32);
        base.rerolls = r["rerolls"].as_i64().map(|x| x as i32);
        base.tooltip = r["tooltip"].as_str().unwrap_or("").to_string();
        base.panel_covered = r["panelCovered"].as_bool().unwrap_or(false);
        let template = base.cards[0].clone();
        base.cards = r["cards"]
            .as_array()?
            .iter()
            .map(|c| read::Card { en: c["en"].as_str().map(str::to_string), zh: c["zh"].as_str().map(str::to_string), raised: c["raised"].as_bool().unwrap_or(false), x: c["x"].as_i64().unwrap_or(0) as i32, ..template.clone() })
            .collect();
        // The name table's index, as the reader sets it: the English client's tooltip is matched by it (vorax_learn::card_printed).
        for c in &mut base.cards {
            c.index = c.en.as_deref().and_then(|en| crate::vorax::names::names().cards.iter().position(|n| n.en == en));
        }
        Some(base)
    }

    /// A logged pick's card and how.
    type Pick = (Option<usize>, Option<String>);

    /// The solver state of the last potion advice before the end of `rows` whose hand is `hand`, from the rows since
    /// the `vorax_round` row before it (None when that hand was not advised, or a card is unread).
    fn advised_state(rows: &[Value], hand: &[Option<String>]) -> Option<Value> {
        let from = rows.iter().rposition(|v| v["kind"] == "vorax_round").map_or(0, |k| k + 1);
        let hand: Vec<String> = hand.iter().cloned().collect::<Option<_>>()?;
        rows[from..].iter().rev().filter(|v| v["kind"] == "vorax_advice").find_map(|v| {
            let x: ScreenState = serde_json::from_value(v["state"].clone()).ok()?;
            (x.hand == hand && x.phase == ScreenPhase::Potion).then(|| v["state"].clone())
        })
    }

    /// A logged potion pick (`vorax_round` at log line `line`) re-derived from the reads logged since the round row
    /// before it, as the session derives it (the card raised on the last read, the last card raised, the hover, then
    /// [`settle_pick`]), with `fix` the reader's [`read::hover_fixes`] applied to each read first (rebuilt from the
    /// logged cards: names, how high each type text read and whether it read at all, the tooltip). None when the log
    /// is missing. It differs from the session in that there is no `clicked` candidate (cue times are not logged)
    /// and the reader's tooltip check is not run (`tip_fits` all None: the logged names are taken as read); the card
    /// raised on the last read is the first one raised, as in `synced`.
    fn repick(dir: &std::path::Path, line: usize, fix: bool) -> Option<Pick> {
        let text = std::fs::read_to_string(dir.join("log.jsonl")).ok()?;
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap_or(Value::Null)).collect();
        let row = rows.get(line - 1)?.clone();
        let from = rows[..line - 1].iter().rposition(|v| v["kind"] == "vorax_round").map_or(0, |k| k + 1);
        let hand: Vec<Option<String>> = row.pointer("/chosen/hand")?.as_array()?.iter().map(|c| c.as_str().map(str::to_string)).collect();
        let names = crate::vorax::names::names();
        let mut reads: Vec<Reading> = Vec::new();
        for v in rows[from..line - 1].iter().filter(|v| v["kind"] == "vorax_read") {
            let Some(mut r) = reading_of_row(v) else { continue };
            if r.round != row["round"].as_i64().map(|x| x as i32) || other_hand(&super::hand(&r), &hand) {
                continue;
            }
            for (c, l) in r.cards.iter_mut().zip(v.pointer("/reading/cards").and_then(Value::as_array).into_iter().flatten()) {
                c.dy = l["dy"].as_i64().unwrap_or(0) as i32;
                c.raw_type = l["rawType"].as_str().unwrap_or("").to_string();
                c.index = c.en.as_deref().and_then(|en| names.cards.iter().position(|n| n.en == en));
            }
            if fix {
                let tip = hovered_card(&r).and_then(|h| r.cards[h].index);
                let none = vec![None; r.cards.len()];
                read::hover_fixes(&mut r.cards, tip, &none);
            }
            reads.push(r);
        }
        let last = reads.last()?;
        let raised = last.cards.iter().position(|c| c.raised);
        let earlier = reads.iter().rev().find_map(|r| seen_card(r, false)).map(|s| s.1);
        let hovered = reads.iter().fold(None, hovered_kept).map(|s| s.1);
        let mut r = row.clone();
        let candidates: Vec<Value> = [(raised, "raised"), (earlier, "raisedEarlier"), (hovered, "tooltip")].into_iter().filter_map(|(k, how)| k.map(|k| json!([k, how]))).collect();
        r["_pick"] = json!(candidates);
        settle_pick(&mut r, row["after"].as_str());
        Some((r.pointer("/chosen/card").and_then(Value::as_u64).map(|c| c as usize), r.pointer("/chosen/how").and_then(Value::as_str).map(str::to_string)))
    }

    /// The logged picks a shallow raise on the far card recorded wrong, re-derived from their logged reads without
    /// and with the reader's fix (skipped when the folders are gone): 2026-11-06_21-42-03 round 6 (line 276) and
    /// round 8 (line 321), `細肢藥粉-骨衛兵` hovered and taken (a new Bone Sentry group on the board after), the
    /// first card recorded, with the fix the tooltip's third card; 2026-11-06_14-06-38 round 1 (line 47),
    /// `祛異藥粉` recorded while the board went from three tanks to six as `異種激素` does (one or two groups
    /// removed, four added), with the fix the first card, last read raised (line 34).
    #[test]
    fn logged_shallow_raises_no_longer_name_the_pick() {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax");
        let some = |k: usize, how: &str| -> Pick { (Some(k), Some(how.to_string())) };
        for (game, line, old, new) in [
            ("2026-11-06_21-42-03", 276, some(0, "raised"), some(2, "tooltip")),
            ("2026-11-06_21-42-03", 321, some(0, "raised"), some(2, "tooltip")),
            ("2026-11-06_14-06-38", 47, some(2, "raised"), some(0, "raisedEarlier")),
        ] {
            let dir = home.join(game);
            let Some(before) = repick(&dir, line, false) else {
                println!("skipped: {game} is missing");
                continue;
            };
            assert_eq!((before, repick(&dir, line, true).unwrap()), (old, new), "{game} line {line}");
        }
    }

    /// Every `vorax_round` row of the game log in `dir`, its card re-derived from the logged reads with the rules
    /// before 2026-11-06 (`old`: hover wiped by any text, reroll rows with a card, no potion check) and after (`new`:
    /// [`hovered_kept`], no card on a reroll, [`settle_pick`]): (log line, logged, old, new). A row logged as raised
    /// on its last read (the replay cannot see that read) keeps its card in `old`; in `new` a potion row's raised
    /// card is the first candidate of [`settle_pick`], like the session's, and other rows keep it. Rows from logs before
    /// `chosen.tanks` get one unknown tank per tank click.
    fn replayed(dir: &std::path::Path) -> Vec<(usize, Pick, Pick, Pick)> {
        replayed_with(dir, false, true)
    }

    /// The selection screen reads of one stretch of logged rows (`vorax_read` rows; other screens read as None),
    /// with `carry` the names carried as the session carries them ([`carry_names`] from the read just before or the
    /// last steady fully named read; a read that is not a selection screen ends the read just before).
    fn window_reads(rows: &[Value], carry: bool) -> Vec<Reading> {
        let index = |en: Option<&str>| en.and_then(|en| crate::vorax::names::names().cards.iter().position(|n| n.en == en));
        let (mut prev, mut named, mut last_sig): (Option<Reading>, Option<Reading>, Option<String>) = (None, None, None);
        let mut out = Vec::new();
        for v in rows.iter().filter(|v| v["kind"] == "vorax_read") {
            let Some(mut r) = reading_of_row(v) else {
                (prev, last_sig) = (None, None);
                continue;
            };
            if carry {
                for c in &mut r.cards {
                    c.index = index(c.en.as_deref());
                }
                let p = prev.take();
                if !p.as_ref().is_some_and(|p| carry_names(&mut r, p).is_some())
                    && let Some(n) = &named
                {
                    carry_names(&mut r, n);
                }
                let sig = signature(&r);
                let whole = r.cards.iter().all(|c| c.index.is_some());
                if whole && last_sig.as_deref() == Some(sig.as_str()) {
                    named = Some(r.clone());
                }
                prev = (whole && r.cards.iter().all(|c| c.how != Some(read::How::Carried))).then(|| r.clone());
                last_sig = Some(sig);
            }
            out.push(r);
        }
        out
    }

    /// [`replayed`], with the names carried as the session carries them when `carry`.
    /// [`replayed`] with `carry` the names carried ([`window_reads`]) and `board_check` the board check: a potion row's
    /// solver state is the last `vorax_advice` state of its hand before it (the session keeps its own read's state).
    fn replayed_with(dir: &std::path::Path, carry: bool, board_check: bool) -> Vec<(usize, Pick, Pick, Pick)> {
        let text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap_or(Value::Null)).collect();
        let pick_of = |v: &Value| -> Pick { (v.pointer("/chosen/card").and_then(Value::as_u64).map(|c| c as usize), v.pointer("/chosen/how").and_then(Value::as_str).map(str::to_string)) };
        let mut out = Vec::new();
        let mut window_start = 0;
        for (i, row) in rows.iter().enumerate() {
            if row["kind"] != "vorax_round" {
                continue;
            }
            let cards_en: Vec<Option<String>> = row.pointer("/chosen/hand").and_then(Value::as_array).map(|a| a.iter().map(|c| c.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let round = row["round"].as_i64().map(|x| x as i32);
            let phase = row["phase"].as_str().unwrap_or("").to_string();
            let mut board = potion_board(false, None, 12);
            board.round = round;
            let template = board.cards[0].clone();
            board.cards = cards_en.iter().map(|en| read::Card { en: en.clone(), ..template.clone() }).collect();
            // This hand's reads: from the previous round row up to the last read of this same hand.
            let reads: Vec<Reading> = window_reads(&rows[window_start..i], carry);
            window_start = i + 1;
            let last = reads.iter().rposition(|r| r.round == round && !other_hand(&hand(r), &cards_en));
            let reads = &reads[..last.map_or(0, |k| k + 1)];
            let (mut raised_seen, mut raised_other, mut old_hover, mut new_hover) = (None, None, None, None);
            for r in reads {
                if let Some(s) = seen_card(r, false) {
                    note_raised(&mut raised_seen, &mut raised_other, s);
                }
                if read::norm(&r.tooltip).chars().count() >= 2 {
                    old_hover = seen_card(r, true);
                }
                new_hover = hovered_kept(new_hover.take(), r);
            }
            let earlier = seen_in(&raised_seen, &board);
            let (old_h, new_h) = (seen_in(&old_hover, &board), seen_in(&new_hover, &board));
            let logged = pick_of(row);
            let rerolled = phase == "reroll";
            let raised = logged.1.as_deref() == Some("raised");
            let pick = |hover: Option<usize>| -> Pick {
                if raised {
                    return logged.clone();
                }
                match (earlier, hover) {
                    (Some(k), _) => (Some(k), Some("raisedEarlier".into())),
                    (None, Some(k)) => (Some(k), Some("tooltip".into())),
                    _ => (None, None),
                }
            };
            // The raised card only hovered on the way to Confirm after another was raised ([`hover_pass`], 2026-11-09):
            // no card (an instrument's icon names it later in the session, not in this replay).
            let last_tip = reads.last().and_then(hovered_card);
            let mixed = hover_pass(if raised { logged.0 } else { None }, last_tip, earlier, seen_in(&raised_other, &board));
            let old = if rerolled { logged.clone() } else { pick(old_h) };
            let mut new = if rerolled || mixed { (None, None) } else { pick(new_h) };
            if phase == "potion" && !rerolled {
                let last = if raised { logged.0 } else { None };
                let candidates: Vec<Value> = if mixed { Vec::new() } else { [(last, "raised"), (earlier, "raisedEarlier"), (new_h, "tooltip")].into_iter().filter_map(|(k, how)| k.map(|k| json!([k, how]))).collect() };
                {
                    let mut r = row.clone();
                    if r.pointer("/chosen/tanks").is_none_or(Value::is_null) {
                        let n = r["cues"].as_array().map_or(0, |a| a.iter().filter(|c| *c == "TankClick").count());
                        r["chosen"]["tanks"] = json!(vec![Value::Null; n]);
                    }
                    r["_pick"] = json!(candidates);
                    if board_check && let Some(x) = advised_state(&rows[..i], &cards_en) {
                        r["_state"] = x;
                    }
                    settle_pick(&mut r, row["after"].as_str());
                    new = pick_of(&r);
                }
            }
            out.push((i + 1, logged, old, new));
        }
        out
    }

    /// The board check of a potion pick ([`board_fits`] in [`settle_pick`]): the 2026-11-08_18-44-54 round 2 hand on a
    /// board of Pest groups and a Bone Sentry group, the board after it made by playing `Probiotic Enzyme Solution`.
    /// No card seen picked (the raised one only hovered): the board names it (`board`). The hovered card kept as the
    /// pick: none rather than it. A board that no card gives, or no state kept: the rows settle as before.
    #[test]
    fn the_board_after_a_potion_names_its_card_or_none() {
        use vorax::advisor::ScreenMonster;
        use vorax::game::dispatch_with;
        use vorax::types::Action;
        use vorax::{Archetype, Rarity};
        let m = |archetype: Archetype, rarity: Rarity, activity: i64, quantity: i64| Some(ScreenMonster { archetype, rarity, activity, quantity });
        let hand: Vec<String> = ["Probiotic Enzyme Solution", "Slender-Limb Powder - Bone Sentry", "Digestive Enzyme Solution"].map(String::from).to_vec();
        let x = ScreenState {
            tanks: vec![m(Archetype::Pest, Rarity::Normal, 1, 81), m(Archetype::Pest, Rarity::Normal, 1, 81), m(Archetype::Pest, Rarity::Magic, 5, 69), m(Archetype::BoneSentry, Rarity::Normal, 1, 154), None, None],
            hand: hand.clone(),
            instruments: vec!["Chrysalis Specimen".into(), "Incubation Sac".into()],
            instrument_rounds: Some(vec![1, 1]),
            phase: ScreenPhase::Potion,
            round: Some(2),
            potion_refreshes: Some(2),
            instrument_refreshes: Some(2),
            shelley: None,
        };
        // the board after playing card k under seed 0, as a board signature
        let after = |k: i64| {
            let mut s = vorax::advisor::to_state(&x, &mut Vec::new());
            s.rng_state = 12_345;
            for a in [Action::SelectCard { index: k }, Action::Confirm] {
                s = dispatch_with(&s, a, false);
            }
            let tanks: Vec<String> = s.tanks.iter().map(|t| t.map_or_else(|| "_".to_string(), |m| format!("Some({:?}),Some({:?}),Some({}),Some({})", m.archetype, m.rarity, m.activity, m.quantity))).collect();
            format!("Instrument|Some(2)|Some(2)|{};|x;|", tanks.join(";"))
        };
        let fits = board_fits(&x, &[], Some(&after(0))).unwrap();
        assert_eq!(fits, [true, false, false], "only the card played gives its board");
        let row = |pick: Value| {
            json!({ "round": 2, "phase": "potion", "missing": [], "cues": ["CardClick", "CardClick", "Confirm", "PotionUsed"], "_pick": pick, "_state": serde_json::to_value(&x).unwrap(),
                "chosen": { "card": null, "how": null, "hand": hand.clone(), "tankClicks": 0, "tanks": [] } })
        };
        let settled = |pick: Value, after: &str| {
            let mut r = row(pick);
            settle_pick(&mut r, Some(after));
            assert!(r.get("_state").is_none() && r.get("_pick").is_none());
            (r["chosen"]["card"].clone(), r["chosen"]["how"].clone(), r["missing"].clone(), r["chosen"]["basis"].clone())
        };
        // no card seen picked: the board names it
        let (card, how, missing, basis) = settled(json!([]), &after(0));
        assert_eq!((card, how, missing), (json!(0), json!("board"), json!([])));
        assert_eq!(basis.as_array().unwrap().last(), Some(&json!({ "board": [true, false, false] })));
        // the hovered card kept as the pick, the board another's: none
        let (card, how, missing, _) = settled(json!([[1, "raised"]]), &after(0));
        assert_eq!((card, how, missing), (Value::Null, Value::Null, json!(["card"])));
        // the card raised and the board agree: kept as raised
        assert_eq!(settled(json!([[0, "raised"]]), &after(0)).1, json!("raised"));
        // a board no card gives: settled as before (the raised card kept, none when none was seen)
        let nowhere = after(0).replacen("Some(Pest)", "Some(Aemberon)", 1);
        assert_ne!(nowhere, after(0));
        assert_eq!(settled(json!([[1, "raised"]]), &nowhere).0, json!(1));
        assert_eq!(settled(json!([]), &nowhere).0, Value::Null);
        // no state kept (a hand that was not read whole): as before
        let mut r = row(json!([[1, "raised"]]));
        r.as_object_mut().unwrap().remove("_state");
        settle_pick(&mut r, Some(&after(0)));
        assert_eq!(r["chosen"]["card"], json!(1));
    }

    /// The two games of 2026-11-06 replayed from their logs (skipped when the folders are gone): the picks recorded
    /// wrong or lost are now right or null, and every other row keeps its card.
    #[test]
    fn the_picks_of_the_12_23_and_13_46_games_replay_right() {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax");
        let some = |k: usize, how: &str| -> Pick { (Some(k), Some(how.to_string())) };
        for (game, want) in [
            // 13-46-25: round 1 instrument (`孵化囊`, the hover kept through `0 / 2`), round 3 reroll (no card), round 3
            // (`Xeno-Hormone`, not `Pure Leech Solution`).
            ("2026-11-06_13-46-25", vec![(27, some(0, "tooltip")), (85, (None, None)), (107, some(2, "tooltip"))]),
            // 12-23-50: round 6 (`Spinal Solution - Aemberon` takes no target, tank 1 was clicked: null); round 7, no card
            // seen picked, `Bewitching Tincture` (the advised card) the one card whose play gives the board after it.
            ("2026-11-06_12-23-50", vec![(237, (None, None)), (258, some(0, "board"))]),
        ] {
            let dir = home.join(game);
            if !dir.join("log.jsonl").is_file() {
                println!("skipped: {game} is missing");
                continue;
            }
            let rows = replayed(&dir);
            for (line, logged, old, new) in &rows {
                match want.iter().find(|w| w.0 == *line) {
                    // The old rules replayed give what was logged (the replay sees what the session saw).
                    Some((_, w)) => assert_eq!((new, old), (w, logged), "{game} line {line}"),
                    None => assert_eq!(new, old, "{game} line {line}: logged {logged:?}"),
                }
            }
        }
    }

    /// Two cards raised in one hand make the pick unknown ([`mixed_raise`]): the card raised on the last read, the
    /// last raised and the one before it, in this hand; one card raised again and again is one card.
    #[test]
    fn two_cards_raised_in_a_hand_give_no_card() {
        let hand = vec![Some("A".to_string()), Some("B".to_string()), Some("C".to_string())];
        let (mut last, mut other) = (None, None);
        note_raised(&mut last, &mut other, (Some(2), 0, hand.clone()));
        note_raised(&mut last, &mut other, (Some(2), 0, hand.clone()));
        assert!(other.is_none(), "the same card raised twice");
        note_raised(&mut last, &mut other, (Some(2), 1, hand.clone()));
        assert_eq!((last.as_ref().map(|s| s.1), other.as_ref().map(|s| s.1)), (Some(1), Some(0)));
        // raised again: still two cards
        note_raised(&mut last, &mut other, (Some(2), 0, hand.clone()));
        assert_eq!((last.as_ref().map(|s| s.1), other.as_ref().map(|s| s.1)), (Some(0), Some(1)));
        assert!(mixed_raise(Some(1), Some(1), Some(0)));
        assert!(mixed_raise(Some(1), Some(0), None), "the last read against the last raised");
        assert!(!mixed_raise(Some(1), Some(1), None));
        assert!(!mixed_raise(None, Some(1), None));
        assert!(!mixed_raise(None, None, None));
        // only hovered: two cards raised and the tooltip on the last read names the raised one
        assert!(hover_pass(Some(1), Some(1), Some(1), Some(0)));
        assert!(!hover_pass(Some(1), None, Some(1), Some(0)), "the tooltip gone: picked");
        assert!(!hover_pass(Some(1), Some(0), Some(1), Some(0)), "the tooltip on another card");
        assert!(!hover_pass(Some(1), Some(1), Some(1), None), "one card raised");
        assert!(!hover_pass(None, None, Some(1), Some(0)), "none raised on the last read");
    }

    /// The four games of 2026-11-08 evening replayed from their logs (skipped when the folders are gone): the potion of
    /// 18-44-54 round 2 (log line 52), picked `Probiotic Enzyme Solution` (the advised card, the user's account) then
    /// `Slender-Limb Powder - Bone Sentry` raised under the tooltip on the way to Confirm, is `Probiotic Enzyme
    /// Solution` from the board after it ([`board_fits`]: every Pest group's activity up, the Bone Sentry group
    /// unchanged); every other row logged as raised keeps its card (round 4 of the same game and 18-51-19 round 10
    /// raised two cards too, the last one picked, the tooltip gone). Prints every row whose replayed card differs from
    /// the logged one (`clicked` and `icon` rows: the replay has no click times or icons).
    #[test]
    fn the_picks_of_the_11_08_evening_games_replay_right() {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax");
        for (game, want) in [
            ("2026-11-08_18-41-59", vec![]),
            ("2026-11-08_18-44-54", vec![52usize]),
            ("2026-11-08_18-48-43", vec![]),
            ("2026-11-08_18-51-19", vec![]),
        ] {
            let dir = home.join(game);
            if !dir.join("log.jsonl").is_file() {
                println!("skipped: {game} is missing");
                continue;
            }
            for (line, logged, _old, new) in replayed(&dir) {
                if new != logged {
                    println!("{game} L{line}: logged {logged:?} -> {new:?}");
                }
                if want.contains(&line) {
                    assert_eq!(new, (Some(0), Some("board".into())), "{game} line {line}");
                } else if logged.1.as_deref().is_some_and(|h| ["raised", "raisedEarlier"].contains(&h)) {
                    assert_eq!(new, logged, "{game} line {line}");
                }
            }
        }
    }

    /// Every saved game (`ASC_VORAX_HOME`, default the repo's home vorax folder) replayed: prints the rows logged as
    /// raised that [`hover_pass`] leaves with no card, the rows whose card is the one the rules allow (`fit`), and how
    /// many rows were looked at.
    #[test]
    #[ignore]
    fn replay_hover_pass_all_games() {
        let home = std::env::var("ASC_VORAX_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax"));
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("log.jsonl").is_file()).collect();
        dirs.sort();
        let (mut raised, mut dropped) = (0, 0);
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            for (line, logged, _old, new) in replayed(&dir) {
                if logged.1.as_deref() == Some("raised") {
                    raised += 1;
                    if new.0.is_none() {
                        dropped += 1;
                        println!("HOVER-PASS {name} L{line} logged {logged:?}");
                    }
                }
                if new.1.as_deref() == Some("fit") {
                    println!("FIT {name} L{line} logged {logged:?} -> {new:?}");
                }
            }
        }
        println!("rows logged raised {raised}, now no card {dropped}");
    }

    /// Every saved game (`ASC_VORAX_HOME`, default the repo's home vorax folder; `ASC_VORAX_FROM` the first folder
    /// name looked at) replayed on its own client (the `point` of its reads): per game the pick rows, the ones logged
    /// with no card and the ones still with no card now, and a `MISSING` line for each of those.
    #[test]
    #[ignore]
    fn replay_missing_picks() {
        let home = std::env::var("ASC_VORAX_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax"));
        let from = std::env::var("ASC_VORAX_FROM").unwrap_or_default();
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("log.jsonl").is_file()).collect();
        dirs.retain(|d| d.file_name().unwrap().to_string_lossy().as_ref() >= from.as_str());
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
            let english = text.lines().find_map(|l| serde_json::from_str::<Value>(l).ok().and_then(|v| v.pointer("/point/english").and_then(Value::as_bool)));
            #[cfg(feature = "en-vorax")]
            crate::vorax::read::set_english_here(english);
            let (mut rows, mut logged_none, mut now_none) = (0, 0, 0);
            for (line, logged, _old, new) in replayed(&dir) {
                rows += 1;
                logged_none += usize::from(logged.0.is_none());
                if new.0.is_none() {
                    now_none += 1;
                    println!("MISSING {name} L{line} logged {logged:?}");
                }
            }
            println!("GAME {name} english {english:?} picks {rows} logged-none {logged_none} now-none {now_none}");
        }
        #[cfg(feature = "en-vorax")]
        crate::vorax::read::set_english_here(None);
    }

    /// Every `vorax_round` row of the saved games up to 2026-11-06_13-46-25 (`ASC_VORAX_HOME`, default the repo's
    /// home vorax folder) replayed ([`replayed`]): prints every row whose card the new rules change.
    #[test]
    #[ignore]
    fn replay_vorax_round_picks() {
        let home = std::env::var("ASC_VORAX_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax"));
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("log.jsonl").is_file()).collect();
        dirs.sort();
        dirs.retain(|d| d.file_name().unwrap().to_string_lossy().as_ref() <= "2026-11-06_13-46-25");
        let (mut seen, mut changed, mut differs) = (0, 0, 0);
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
            let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap_or(Value::Null)).collect();
            for (line, logged, old, new) in replayed(&dir) {
                seen += 1;
                let row = &rows[line - 1];
                if old != logged {
                    differs += 1;
                    println!("REPLAY-DIFF {name} L{line} logged {logged:?} replayed-old {old:?}");
                }
                if new != old {
                    changed += 1;
                    println!("CHANGED {name} L{line} r{:?} {} ({:?}, {:?}) -> ({:?}, {:?}) hand {} cues {} tanks {}", row["round"].as_i64(), row["phase"].as_str().unwrap_or(""), old.0, old.1, new.0, new.1, row["chosen"]["hand"], row["cues"], row.pointer("/chosen/tanks").cloned().unwrap_or(Value::Null));
                }
            }
        }
        println!("rows {seen}, changed {changed}, old replay differs {differs}");
    }

    /// The picks of the saved games (up to 13-46-25, and 14-06-38 and 16-26-30) replayed with and without names
    /// carried from the read just before ([`replayed_with`]): prints every row whose card changes.
    #[test]
    #[ignore]
    fn replay_picks_with_carried_names() {
        let home = std::env::var("ASC_VORAX_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax"));
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&home).unwrap().map(|e| e.unwrap().path()).filter(|p| p.join("log.jsonl").is_file()).collect();
        dirs.sort();
        dirs.retain(|d| {
            let n = d.file_name().unwrap().to_string_lossy().to_string();
            n.as_str() <= "2026-11-06_13-46-25" || n == "2026-11-06_14-06-38" || n == "2026-11-06_16-26-30"
        });
        let (mut seen, mut changed) = (0, 0);
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let plain = replayed_with(&dir, false, false);
            let carried = replayed_with(&dir, true, false);
            for ((line, _, _, a), (_, _, _, b)) in plain.iter().zip(&carried) {
                seen += 1;
                if a != b {
                    changed += 1;
                    println!("CARRY-CHANGED {name} L{line} {a:?} -> {b:?}");
                }
            }
        }
        println!("rows {seen}, changed {changed}");
    }

    /// A potion board whose cards carry a name table position (as a real read does), for [`carry_names`].
    fn indexed_board() -> Reading {
        let mut r = potion_board(false, Some(600), 12);
        for c in &mut r.cards {
            c.index = crate::vorax::names::names().cards.iter().position(|n| n.en == c.en.clone().unwrap_or_default());
            c.how = Some(read::How::Name);
        }
        assert!(r.cards.iter().all(|c| c.index.is_some()));
        r
    }

    /// One card name misread on the same hand takes the name from the hand's last fully named steady read; not with
    /// two unread, another card read differently, a card moved, another round, reroll count or kind, nor after a
    /// deal.
    #[test]
    fn one_misread_card_name_is_carried_within_the_hand() {
        let named = indexed_board();
        let unread = |k: usize| {
            let mut r = indexed_board();
            let c = &mut r.cards[k];
            (c.index, c.id, c.en, c.zh, c.how, c.raw_name) = (None, None, None, None, None, "丶皮革拘束帶".into());
            r
        };
        let mut r = unread(1);
        assert_eq!(carry_names(&mut r, &named), Some(1));
        assert_eq!((r.cards[1].en.as_deref(), r.cards[1].index, r.cards[1].how, r.cards[1].raw_name.as_str()), (Some("Viviparous Hormone"), named.cards[1].index, Some(read::How::Carried), "丶皮革拘束帶"));
        assert_eq!(signature(&r), signature(&named));
        // The hover pushes a neighbour 47 aside: still the same hand.
        let mut r = unread(1);
        r.cards[2].x += 47;
        assert_eq!(carry_names(&mut r, &named), Some(1));
        // Not carried.
        let mut two = unread(1);
        (two.cards[0].index, two.cards[0].en) = (None, None);
        let mut other = unread(1);
        (other.cards[0].index, other.cards[0].en) = (named.cards[2].index, Some("Xeno-Hormone".into()));
        let mut moved = unread(1);
        moved.cards[1].x += 100;
        let mut round = unread(1);
        round.round = Some(2);
        let mut rerolled = unread(1);
        rerolled.rerolls = Some(2);
        let mut no_count = unread(1);
        no_count.rerolls = None;
        let mut kind = unread(1);
        kind.kind = Kind::Instrument;
        for (what, mut r) in [("two unread", two), ("another card", other), ("moved", moved), ("another round", round), ("another reroll count", rerolled), ("reroll count unread on this read", no_count), ("another kind", kind)] {
            assert_eq!(carry_names(&mut r, &named), None, "{what}");
        }
        let mut partly = named.clone();
        partly.cards[0].index = None;
        assert_eq!(carry_names(&mut unread(1), &partly), None, "the named read must have every name");
        let mut uncounted = named.clone();
        uncounted.rerolls = None;
        assert_eq!(carry_names(&mut unread(1), &uncounted), None, "reroll count unread on the named read");

        // In the session: carried after a steady named read, not once a deal cue came.
        let (mut s, root) = logged("carry");
        feed(&mut s, named.clone(), 2_000);
        feed(&mut s, named.clone(), 2_100);
        assert!(s.game.named_hand.is_some());
        feed(&mut s, unread(1), 2_200);
        assert_eq!(s.last_sig.as_deref(), Some(signature(&named).as_str()), "the carried read keeps the steady signature");
        s.note_cue(Cue::Shuffle, Some(2_300));
        assert!(s.game.named_hand.is_none());
        feed(&mut s, unread(1), 2_400);
        assert_ne!(s.last_sig.as_deref(), Some(signature(&named).as_str()), "after a deal nothing is carried");
        // Frames taken before the deal cue do not name the next hand.
        feed(&mut s, named.clone(), 2_250);
        feed(&mut s, named.clone(), 2_260);
        assert!(s.game.named_hand.is_none());
        let rows = rows_of(s, &root);
        let reads = kinds(&rows, "vorax_read");
        assert_eq!(reads[2]["reading"]["cards"][1]["how"], json!("carried"));
        assert_eq!(reads[3]["reading"]["cards"][1]["en"], Value::Null);
    }

    /// One card unread on the read right after a read naming the whole hand, not steady: the name comes from that
    /// read (the hand had no steady fully named read yet); not across a deal cue, nor from a read further back.
    #[test]
    fn a_name_is_carried_from_the_read_just_before() {
        let named = indexed_board();
        let unread = |k: usize| {
            let mut r = indexed_board();
            let c = &mut r.cards[k];
            (c.index, c.id, c.en, c.zh, c.how, c.raw_name) = (None, None, None, None, None, "卵藥粉".into());
            c.x += 7;
            r
        };
        let (mut s, _root) = logged("carry-prev");
        feed(&mut s, named.clone(), 2_000);
        assert!(s.game.named_hand.is_none(), "one read is not steady");
        feed(&mut s, unread(2), 2_148);
        assert_eq!(s.last_sig.as_deref(), Some(signature(&named).as_str()), "carried, and steady with the read before");
        assert_ne!(s.view().status, Status::Unreadable);
        // A deal cue between them: not carried.
        let (mut s, _root) = logged("carry-prev-deal");
        feed(&mut s, named.clone(), 2_000);
        s.note_cue(Cue::Shuffle, Some(2_050));
        feed(&mut s, unread(2), 2_148);
        assert_ne!(s.last_sig.as_deref(), Some(signature(&named).as_str()));
        // A read between them with two cards unread (nothing to carry): the whole read is no longer the one just
        // before.
        let (mut s, _root) = logged("carry-prev-gap");
        feed(&mut s, named.clone(), 2_000);
        let mut other = unread(0);
        other.cards[0].x -= 7;
        (other.cards[1].index, other.cards[1].en) = (None, None);
        feed(&mut s, other, 2_070);
        feed(&mut s, unread(2), 2_148);
        assert_ne!(s.last_sig.as_deref(), Some(signature(&named).as_str()));
    }

    /// The 2026-11-06_16-26-30 game, round 4 after the reroll (log lines 117 to 124, skipped when the folder is
    /// gone): the hand read whole at 46.370 (not steady), then the hovered third card unread from 46.518 to 47.310,
    /// and the advice came only at 49.532. Fed again from the log, the read at 46.518 takes the third card's name
    /// from the one before and is steady, so the hand is solved there.
    #[test]
    fn the_16_26_hovered_card_does_not_hold_the_advice_back() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("home").join("vorax").join("2026-11-06_16-26-30");
        let Ok(text) = std::fs::read_to_string(dir.join("log.jsonl")) else {
            println!("skipped: 2026-11-06_16-26-30 is missing");
            return;
        };
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap_or(Value::Null)).collect();
        let index = |en: Option<&str>| en.and_then(|en| crate::vorax::names::names().cards.iter().position(|n| n.en == en));
        let (mut s, _root) = logged("hover-16-26");
        let mut solved_at = None;
        for (k, row) in rows.iter().enumerate().take(124).skip(116) {
            let Some(mut r) = reading_of_row(row) else { continue };
            for c in &mut r.cards {
                c.index = index(c.en.as_deref());
            }
            // The synthetic tanks of reading_of_row add up to 600.
            r.ultimate = Some(600);
            let at = 46_000 + 150 * (k as i64 - 116);
            feed(&mut s, r, at);
            if solved_at.is_none() && s.advised.is_some() {
                solved_at = Some(k + 1);
            }
        }
        assert_eq!(solved_at, Some(118), "solved at the read of log line 118 (46.518)");
    }

    /// Two reads are the same hand only with as many cards, no card read differently in both, and at most one
    /// position unread.
    #[test]
    fn same_cards_tells_one_hand_from_another() {
        let h = |v: &[Option<&str>]| v.iter().map(|c| c.map(str::to_string)).collect::<Vec<_>>();
        let full = h(&[Some("a"), Some("b"), Some("c")]);
        assert!(same_cards(&full, &full));
        assert!(same_cards(&full, &h(&[Some("a"), None, Some("c")])));
        assert!(same_cards(&h(&[None, Some("b"), Some("c")]), &full));
        // A position read in both with another name.
        assert!(!same_cards(&full, &h(&[Some("a"), Some("x"), Some("c")])));
        assert!(!same_cards(&h(&[Some("a"), None, Some("c")]), &h(&[Some("x"), None, Some("c")])));
        // Two positions unread, in one read or across both.
        assert!(!same_cards(&full, &h(&[None, None, Some("c")])));
        assert!(!same_cards(&h(&[None, Some("b"), Some("c")]), &h(&[Some("a"), None, Some("c")])));
        // Another card count, or no cards.
        assert!(!same_cards(&full, &h(&[Some("a"), Some("b")])));
        assert!(!same_cards(&full, &h(&[Some("a"), Some("b"), Some("c"), Some("d")])));
        assert!(!same_cards(&[], &[]));
    }

    /// Within one hand, a steady read with a card unread does not replace a fuller one as the pick's board, and a
    /// fuller read replaces a less full one.
    #[test]
    fn the_fuller_read_of_a_hand_is_the_board_of_the_pick() {
        let (mut s, root) = logged("pick-fuller");
        let full = named_board(8, "");
        let mut covered = named_board(8, "");
        covered.cards[0].en = None;
        feed(&mut s, covered.clone(), 2_000);
        feed(&mut s, covered.clone(), 2_100);
        assert_eq!(s.game.choice.as_ref().map(|(sig, _)| sig.clone()), Some(signature(&covered)), "the first steady read");
        feed(&mut s, full.clone(), 2_300);
        feed(&mut s, full.clone(), 2_400);
        assert_eq!(s.game.choice.as_ref().map(|(sig, _)| sig.clone()), Some(signature(&full)), "the fuller read replaces it");
        feed(&mut s, covered.clone(), 2_600);
        feed(&mut s, covered, 2_700);
        assert_eq!(s.game.choice.as_ref().map(|(sig, _)| sig.clone()), Some(signature(&full)), "a less full read does not");
        drop(rows_of(s, &root));
    }

    /// A board closed and reopened before the pick keeps its hand as the pick's board (the reopened page is confirmed
    /// before its board settles). A confirm with no selection screen of its hand read at all keeps the round, the
    /// cues and the tanks clicked, and says the board, the hand and the card are missing; a reroll lacks no card.
    #[test]
    fn a_pick_keeps_what_is_known_and_says_what_is_missing() {
        use PageState::{Destory, Run};
        let (mut s, root) = logged("pick-missing");
        let board = named_board(4, "");
        feed(&mut s, board.clone(), 2_000);
        feed(&mut s, board.clone(), 2_100);
        assert_eq!(s.note_cue(page(Page::Main, Destory), Some(2_500)), ModeChange::Left);
        assert_eq!(s.note_cue(page(Page::Main, Run), Some(3_000)), ModeChange::Entered);
        feed(&mut s, other_screen(), 3_050);
        s.note_cue(Cue::CardClick, Some(3_100));
        s.note_cue(Cue::TankClick { tank: Some(2) }, Some(3_200));
        s.note_cue(Cue::Confirm, Some(3_300));
        s.note_cue(Cue::Synced, Some(3_310));
        // A second confirm before any hand settled: nothing of its board is known.
        s.note_cue(Cue::TankClick { tank: None }, Some(3_400));
        s.note_cue(Cue::Confirm, Some(3_500));
        s.note_cue(Cue::Synced, Some(3_510));
        s.note_cue(Cue::Reroll { instrument: false }, Some(3_600));
        s.note_cue(Cue::Synced, Some(3_610));
        feed(&mut s, named_board(5, ""), 4_000);
        feed(&mut s, named_board(5, ""), 4_100);
        let rows = rows_of(s, &root);
        let picks = kinds(&rows, "vorax_round");
        assert_eq!(picks.len(), 3);
        assert_eq!(picks[0]["before"].as_str(), Some(signature(&board).as_str()), "the hand from before the pause");
        assert_eq!((&picks[0]["round"], &picks[0]["phase"], &picks[0]["chosen"]["hand"]), (&json!(4), &json!("potion"), &json!(hand(&board))));
        assert_eq!(picks[0]["chosen"]["tanks"], json!([2]));
        assert_eq!(picks[0]["missing"], json!(["card"]));
        assert_eq!((&picks[1]["round"], &picks[1]["phase"], &picks[1]["before"]), (&json!(4), &Value::Null, &Value::Null));
        assert_eq!((&picks[1]["chosen"]["card"], &picks[1]["chosen"]["hand"]), (&Value::Null, &json!([])));
        assert_eq!((&picks[1]["chosen"]["tankClicks"], &picks[1]["chosen"]["tanks"]), (&json!(1), &json!([null])));
        assert_eq!(picks[1]["missing"], json!(["before", "hand", "card"]));
        assert_eq!((&picks[2]["phase"], &picks[2]["missing"]), (&json!("reroll"), &json!(["before", "hand"])));
        assert_eq!(picks[2]["after"].as_str(), Some(signature(&named_board(5, "")).as_str()));
    }

    /// The tank under the cursor comes out the same at 1440p, 1080p (windowed, and on a monitor left of the main one),
    /// 4K and 21:9: the cursor is placed with `read::to_frame`, the inverse of the frame mapping. Between two tanks,
    /// above or below them, and outside the game it is unknown.
    #[test]
    fn the_tank_under_the_cursor_at_any_resolution() {
        let rects: [[i32; 4]; 5] = [[0, 0, 2560, 1440], [100, 50, 1920, 1080], [-1920, 0, 1920, 1080], [0, 0, 3840, 2160], [0, 0, 3440, 1440]];
        for rect in rects {
            let at = |x: f32, y: f32| {
                let (fx, fy, _, _) = read::to_frame(rect[2] as usize, rect[3] as usize, (x, y, 0.0, 0.0));
                tank_at(rect, (rect[0] + fx.round() as i32, rect[1] + fy.round() as i32))
            };
            for (i, &c) in read::TANK_X.iter().enumerate() {
                let c = c as f32;
                for (x, y) in [(c, 345.0), (c - 150.0, 120.0), (c + 150.0, 580.0)] {
                    assert_eq!(at(x, y), Some(i as u8), "{rect:?}: tank {i} at {x},{y}");
                }
                assert_eq!(at(c, 60.0), None, "{rect:?}: above tank {i}");
                assert_eq!(at(c, 650.0), None, "{rect:?}: below tank {i}");
            }
            for pair in read::TANK_X.windows(2) {
                let mid = (pair[0] + pair[1]) as f32 / 2.0;
                assert_eq!(at(mid, 345.0), None, "{rect:?}: between {pair:?}");
            }
            let [left, top, w, h] = rect;
            assert_eq!(tank_at(rect, (left - 1, top + h / 4)), None, "{rect:?}: left of the game");
            assert_eq!(tank_at(rect, (left + w, top + h / 4)), None, "{rect:?}: right of the game");
            assert_eq!(tank_at(rect, (left + w / 2, top - 1)), None, "{rect:?}: above the game");
        }
        assert_eq!(tank_at([0, 0, 0, 0], (0, 0)), None, "no client area");
    }

    /// Hand one read to `handle` with a frame of one gray level (two levels are plainly different screens).
    fn feed_gray(s: &mut VoraxSession, r: Reading, at: i64, gray: u8) {
        s.clock = s.clock.max(at);
        s.handle(Done { reading: r, frame: Rgba { w: 64, h: 36, data: vec![gray; 64 * 36 * 4] }, at, trigger: "auto".into(), read_ms: 0, s: None, icons: None, client: Value::Null });
    }

    fn other_read() -> Reading {
        Reading { kind: Kind::Other, round: None, cards: vec![], tanks: vec![], rerolls: None, ultimate: None, ..potion_board(false, None, 12) }
    }

    /// A board read as another screen saves its whole frame with the general observe mode on (vorax_learn off); the
    /// result screen too; the endgame battle and the intro page never do; with both observe modes off nothing is saved.
    #[test]
    fn unread_boards_save_whole_frames_only_on_the_board() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), observe: true, ..s.config.clone() });
        s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
        assert_eq!(s.mode(), Mode::Board);
        // One unread read alone (a round change's animation): none; a second in a row 100 ms later (a deal
        // animation): none; still unread 500 ms after the first: saved.
        feed_gray(&mut s, other_read(), 1_500, 40);
        assert!(kinds_now(&s, "vorax_full_frame").is_empty());
        feed_gray(&mut s, other_read(), 1_600, 40);
        assert!(kinds_now(&s, "vorax_full_frame").is_empty());
        feed_gray(&mut s, other_read(), 2_000, 40);
        // A read that is not another screen: none.
        feed_gray(&mut s, potion_board(false, None, 12), 5_000, 200);
        assert_eq!(kinds_now(&s, "vorax_full_frame").len(), 1);
        // The result screen read as another screen: saved.
        s.note_cue(page(Page::Reward, PageState::Run), Some(8_000));
        assert_eq!(s.mode(), Mode::Result);
        feed_gray(&mut s, other_read(), 8_500, 200);
        feed_gray(&mut s, other_read(), 9_000, 200);
        let full = kinds_now(&s, "vorax_full_frame");
        assert_eq!(full.len(), 2);
        assert_eq!((full[0]["mode"].as_str(), full[1]["mode"].as_str()), (Some("board"), Some("result")));
        assert!(full.iter().all(|r| r["file"].as_str().is_some_and(|f| f.starts_with("full-other-") && s.log().unwrap().dir().join(f).is_file())));
        // The endgame battle: handle leaves before any saving (a different screen, well after the last one).
        s.note_cue(Cue::Fight, Some(10_000));
        assert_eq!(s.mode(), Mode::Battle);
        feed_gray(&mut s, other_read(), 20_000, 40);
        assert_eq!(kinds_now(&s, "vorax_full_frame").len(), 2);
        // The intro page of a new game (its counters start over): always another screen, never saved.
        s.note_cue(Cue::LevelLeft, Some(30_000));
        s.note_cue(page(Page::Intro, PageState::Run), Some(31_000));
        assert_eq!(s.mode(), Mode::Intro);
        feed_gray(&mut s, other_read(), 32_000, 40);
        feed_gray(&mut s, other_read(), 40_000, 200);
        assert!(kinds_now(&s, "vorax_full_frame").is_empty(), "a new game's folder, nothing saved on the intro page");
        // Both observe modes off: a board read as another screen saves nothing.
        s.configure(Config { observe: false, learn: false, ..s.config.clone() });
        s.note_cue(page(Page::Main, PageState::Run), Some(50_000));
        assert_eq!(s.mode(), Mode::Board);
        feed_gray(&mut s, other_read(), 51_000, 40);
        assert!(kinds_now(&s, "vorax_full_frame").is_empty());
        let all = all_rows(s, &root);
        assert_eq!(all.iter().map(|rows| kinds(rows, "vorax_full_frame").len()).sum::<usize>(), 2);
    }

    /// With observe mode on, a steady selection screen read with a problem (no advice, so no "advised" evidence) saves
    /// its evidence image once a round as "unreadable"; a read that is not steady, the same round again, or observe
    /// mode off saves none.
    #[test]
    fn observe_mode_saves_an_unreadable_hand_once_a_round() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), observe: true, ..s.config.clone() });
        s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
        let unreadable = |s: &VoraxSession| kinds_now(s, "vorax_lossless").into_iter().filter(|r| r["role"] == "unreadable").collect::<Vec<_>>();
        let bad = |round: i32, ult: i64| {
            let mut r = Reading { round: Some(round), problems: vec!["水箱 1（Hollow Cocoon）未知：數值".into()], ..potion_board(false, Some(ult), 12) };
            (r.tanks[0].activity, r.tanks[0].quantity) = (None, None);
            r
        };
        feed(&mut s, bad(5, 600), 2_000);
        assert!(unreadable(&s).is_empty(), "one read: not steady");
        feed(&mut s, bad(5, 600), 2_060);
        assert_eq!(s.view().status, Status::Unreadable);
        let saved = unreadable(&s);
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0]["screen"].as_str(), saved[0]["round"].as_i64()), (Some("hand"), Some(5)));
        assert!(saved[0]["file"].as_str().is_some_and(|f| f.starts_with("evidence-") && f.ends_with(".png")));
        // the same round read otherwise, steady again: not saved again
        feed(&mut s, bad(5, 700), 2_500);
        feed(&mut s, bad(5, 700), 2_560);
        assert_eq!(unreadable(&s).len(), 1);
        // the next round: saved
        feed(&mut s, bad(6, 800), 3_000);
        feed(&mut s, bad(6, 800), 3_060);
        assert_eq!(unreadable(&s).iter().map(|r| r["round"].as_i64()).collect::<Vec<_>>(), [Some(5), Some(6)]);
        // observe mode off: none
        s.configure(Config { observe: false, ..s.config.clone() });
        feed(&mut s, bad(7, 900), 4_000);
        feed(&mut s, bad(7, 900), 4_060);
        assert_eq!(unreadable(&s).len(), 2);
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With the general observe mode on, each hand saves the evidence image of the read its advice was solved on and,
    /// when the hand's last steady read read otherwise, of that one too; a game saves the whole frame of its first
    /// steady pick and of the steady result screen, and the battle panel read the same twice, each named in a
    /// `vorax_lossless` row; observe mode off, none.
    #[test]
    fn observe_mode_saves_the_evidence_of_each_hand_losslessly() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-lossless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), observe: true, ..s.config.clone() });
        s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
        let rows = |s: &VoraxSession| kinds_now(s, "vorax_lossless");
        let role = |r: &Value| (r["screen"].as_str().unwrap_or("").to_string(), r["role"].as_str().unwrap_or("").to_string());
        // hand 1: steady and advised: its evidence ("advised") and the game's whole pick frame
        feed(&mut s, potion_board(false, Some(600), 12), 2_000);
        assert!(rows(&s).is_empty(), "one read: not steady");
        feed(&mut s, potion_board(false, Some(600), 12), 2_060);
        assert!(s.advice().is_some());
        let saved = rows(&s);
        assert_eq!(saved.iter().map(role).collect::<Vec<_>>(), [("pick".into(), String::new()), ("hand".into(), "advised".into())]);
        let advised = &saved[1];
        assert!(advised["file"].as_str().is_some_and(|f| f.starts_with("evidence-") && f.ends_with(".png")));
        let read_at = advised["readAt"].as_str().unwrap().to_string();
        assert!(kinds_now(&s, "vorax_read").iter().any(|r| r["at"].as_str() == Some(read_at.as_str())), "names the read it goes with");
        // the hand read otherwise before its end (a card raised, say): saved as "confirmed" at the hand's end
        feed(&mut s, potion_board(false, Some(600), 20), 2_500);
        feed(&mut s, potion_board(false, Some(600), 20), 2_560);
        s.note_cue(Cue::Confirm, Some(2_800));
        s.note_cue(Cue::Synced, Some(2_810));
        assert_eq!(rows(&s).iter().map(role).next_back(), Some(("hand".into(), "confirmed".into())));
        assert_eq!(rows(&s).len(), 3);
        // hand 2: advised, and its last steady read the same: one image
        s.note_cue(Cue::Dealt, Some(2_900));
        feed(&mut s, potion_board(false, Some(720), 20), 3_000);
        feed(&mut s, potion_board(false, Some(720), 20), 3_060);
        s.note_cue(Cue::Confirm, Some(3_300));
        s.note_cue(Cue::Synced, Some(3_310));
        assert_eq!(rows(&s).iter().filter(|r| r["role"] == "advised").count(), 2);
        assert_eq!(rows(&s).len(), 4, "the same read twice is saved once");
        // the result screen, steady: its whole frame
        s.note_cue(page(Page::Reward, PageState::Run), Some(8_000));
        let result = || Reading { kind: Kind::Result, ultimate: Some(851_000), ..other_read() };
        feed(&mut s, result(), 8_500);
        feed(&mut s, result(), 9_000);
        assert_eq!(rows(&s).last().map(role), Some(("result".into(), String::new())));
        // the battle: the panel read the same twice in a row saves the panel
        s.note_cue(Cue::Fight, Some(10_000));
        assert_eq!(s.mode(), Mode::Battle);
        let panel = read::Battle { wave: Some((1, 3)), activity: Some(91_100.0), bonus: Some(18_200.0), next: Some(114_000.0), percent: Some(0), raw: String::new() };
        let crop = || Rgba { w: 4, h: 4, data: vec![0; 64] };
        s.battle_read(panel.clone(), crop(), 11_000);
        assert_eq!(rows(&s).len(), 5, "one read alone");
        s.battle_read(panel.clone(), crop(), 12_000);
        let saved = rows(&s);
        assert_eq!(saved.len(), 6);
        assert_eq!((saved[5]["screen"].as_str(), saved[5]["battle"]["activity"].as_f64()), (Some("battle"), Some(91_100.0)));
        let dir = s.log().unwrap().dir().to_path_buf();
        s.log().unwrap().wait();
        for r in &saved {
            let png = std::fs::read(dir.join(r["file"].as_str().unwrap())).unwrap();
            assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "{}", r["file"]);
        }
        // a new game with observe mode off: none
        s.configure(Config { observe: false, ..s.config.clone() });
        s.note_cue(Cue::LevelLeft, Some(30_000));
        s.note_cue(page(Page::Main, PageState::Run), Some(31_000));
        feed(&mut s, potion_board(false, Some(600), 12), 31_500);
        feed(&mut s, potion_board(false, Some(600), 12), 31_560);
        s.note_cue(Cue::Synced, Some(32_000));
        let all = all_rows(s, &root);
        assert_eq!(all.iter().map(|rows| kinds(rows, "vorax_lossless").len()).sum::<usize>(), 6);
    }

    /// A `vorax_view` row carries the advice's note as the window shows it (the 12:23 game's rows had none, so a
    /// "cannot simulate" line could not be checked afterwards); null without advice or note.
    #[test]
    fn a_view_row_carries_the_advice_note() {
        let (mut s, root) = logged("view-note");
        let advice = |note: Option<&str>| crate::vorax::AdviceRows { headline: "建議：重擲".into(), note: note.map(str::to_string), rows: vec![], method: "推演".into(), unknown: vec![] };
        s.view.advice = Some(advice(Some("「子一代：寄生蠱卵」無法模擬。")));
        s.log_view(json!({ "marks": [] }), 2_000);
        s.view.advice = Some(advice(None));
        s.log_view(json!({ "marks": [] }), 3_000);
        s.view.advice = None;
        s.log_view(json!({ "marks": [] }), 4_000);
        let views = kinds_now(&s, "vorax_view");
        let notes: Vec<&Value> = views.iter().map(|r| &r["note"]).collect();
        assert_eq!(notes, [&json!("「子一代：寄生蠱卵」無法模擬。"), &Value::Null, &Value::Null]);
        assert_eq!(views[0]["best"], json!("建議：重擲"));
        let _ = all_rows(s, &root);
    }

    /// The 2026-11-06 12:23 game skipped animations by closing the board: each resume's first reads were the world
    /// map (with the character name), not saved; a board still unread over a second after the resume is.
    #[test]
    fn a_resumed_board_saves_no_whole_frame_in_its_first_second() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-full-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), observe: true, ..s.config.clone() });
        s.note_cue(page(Page::Main, PageState::Run), Some(1_000));
        steady_round(&mut s, 1, 2_000);
        assert_eq!(s.note_cue(page(Page::Main, PageState::Destory), Some(3_000)), ModeChange::Left);
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(3_550)), ModeChange::Entered);
        assert_eq!(kinds_now(&s, "vorax_resume").len(), 1);
        for (k, at) in [3_630, 3_700, 3_900, 4_400].into_iter().enumerate() {
            feed_gray(&mut s, other_read(), at, if k % 2 == 0 { 40 } else { 200 });
        }
        assert!(kinds_now(&s, "vorax_full_frame").is_empty(), "the world map right after the resume");
        feed_gray(&mut s, other_read(), 4_700, 40);
        assert_eq!(kinds_now(&s, "vorax_full_frame").len(), 1, "still unread over a second after the resume");
        let _ = all_rows(s, &root);
    }

    /// The UI scale: written in Vorax mode only, when it changes, and again on entering the mode; never a move: not in
    /// the game's cues, no mode change, no pause, no resume, no new folder.
    #[test]
    fn the_ui_scale_is_logged_without_touching_the_game() {
        let root = std::env::temp_dir().join(format!("ascendant-vorax-uiscale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut s = session();
        s.configure(Config { log_root: Some(root.clone()), ..s.config.clone() });
        let scale = |micro: u32| Cue::UiScale { micro };
        // Off: only remembered (a looked-back line too).
        assert_eq!(s.note_cue(scale(667_022), None), ModeChange::None);
        assert_eq!(s.mode(), Mode::Off);
        // Entering writes the value in use.
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(1_000)), ModeChange::Entered);
        steady_round(&mut s, 1, 2_000);
        let (moves, deals, cues) = (s.moves, s.deals, s.game.cues.clone());
        // The same value again: no row; a new one: a row. Neither is a move.
        assert_eq!(s.note_cue(scale(667_022), Some(3_000)), ModeChange::None);
        assert_eq!(s.note_cue(scale(842_409), Some(3_500)), ModeChange::None);
        assert_eq!(s.note_cue(scale(842_409), Some(3_600)), ModeChange::None);
        assert_eq!((s.mode(), s.moves, s.deals, &s.game.cues), (Mode::Board, moves, deals, &cues));
        assert!(s.paused.is_none() && s.resumed.is_none());
        // The board closed mid game: paused; the scale does not end, resume or replace the paused game.
        assert_eq!(s.note_cue(page(Page::Main, PageState::Destory), Some(4_000)), ModeChange::Left);
        assert!(s.paused.is_some());
        assert_eq!(s.note_cue(scale(1_000_000), Some(5_000)), ModeChange::None);
        assert!(s.paused.is_some() && s.mode() == Mode::Off);
        // Back to the board: the paused game is taken back, and the scale in use goes in again.
        assert_eq!(s.note_cue(page(Page::Main, PageState::Run), Some(6_000)), ModeChange::Entered);
        assert!(s.paused.is_none());
        let all = all_rows(s, &root);
        assert_eq!(all.len(), 1, "one game, one folder");
        let rows: Vec<(f64, u64, &str)> = kinds(&all[0], "ui_scale").iter().map(|r| (r["scale"].as_f64().unwrap(), r["micro"].as_u64().unwrap(), r["at"].as_str().unwrap())).collect();
        let at = |ms: i64| super::super::runlog::timestamp(at_time(ms));
        assert_eq!(
            rows,
            [(0.667022, 667_022, at(1_000).as_str()), (0.842409, 842_409, at(3_500).as_str()), (1.0, 1_000_000, at(6_000).as_str())]
        );
        assert!(all[0].iter().all(|r| r["cue"].as_str().is_none_or(|c| !c.contains("UiScale"))));
    }
}
