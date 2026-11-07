//! The game's own log, UE_game.log: it names each screen the game opens as
//! it opens it, 30 to 130 ms behind (measured 2026-10-28), while auto read may
//! rest or wait for an animated screen to settle. A run's theme scroll stayed
//! up 0.9 s and was never read: the log wakes auto read for it.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::latency::game_millis;

/// What a log line tells auto read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Signal {
    /// The prison opened (its board or its entry screen): no more rest.
    Prison,
    /// The run's theme scroll opened: read it before the player confirms it.
    Theme,
    /// A Warden's Treasure's gift was confirmed: a Chromatic Prisoner went
    /// to the vault, which the board may never show (the run's last day).
    Treasure,
    /// Day `cur` of `max` began: the riot countdown on screen can show the
    /// day before for a while (a whole patrol on 2026-10-28).
    Day { cur: u32, max: u32 },
    /// The player clicked something on the prison board (a cell, a door, a
    /// decision): the board changes, however faintly.
    Click,
    /// The player clicked a door or a decision card: the move on show was
    /// played, and the board changes as [`Signal::Click`] says.
    Decision,
    /// The player clicked a prison cell: on a cell pick (a Hunt's prey, an
    /// incident's cell) the move on show was played. Unseen, the pick's
    /// mark came back 0.9 s after a Hunt's click and moved to another cell
    /// as the board changed (2026-11-02, day 18).
    CellPicked,
    /// The run's objective was completed: its panel, often under the
    /// instrument pick it opens, read so 0.7 to 6.7 s later (2026-10-28).
    ObjectiveDone,
    /// The player pressed Consume: its target pick opens.
    Consume,
    /// An inmate was consumed, by the Consume or a chain's bite.
    Bite,
    /// The player ended the Consume target pick (its End button).
    ChainEnd,
    /// The player started a new run.
    RunStart,
    /// The riot screen opened: the run is over.
    RunEnd,
    /// A Warden's Treasure's pick opened: the first, the next after a gift
    /// confirmed, or the same again after View Prison Status.
    TreasureOpen,
    /// The decision cards came up, this many (0 when the log did not list
    /// them, see [`Cards`]), at this UTC time in ms (see [`age`]; 0 when the
    /// line had none): they fade in for a while.
    CardsUp(usize, i64),
    /// Incident `id`'s popup opened, see [`INCIDENT_SOUNDS`].
    Incident(u32),
    /// The player clicked a door: the warden moves.
    Moved,
    /// The player re-rolled an instrument pick's offer.
    Reroll,
    /// An instrument acted on the board, as the Wriggling Iron Box putting a
    /// Rat in a cell as a day starts: the board shows it a while later.
    Instrument,
    /// A Chromatic Prisoner flew into the vault. 18 of 21 runs on 2026-11-01
    /// played it once per Chromatic vaulted; three played it 1 or 2 times
    /// fewer, so it counts as a floor.
    Vaulted,
    /// A vault cell lights up (`Anim_Archive_LightUp`): once per colored monster contained; in the 2026-11-02 game
    /// all 7 monsters gave exactly 7. Counted separately from [`Signal::Vaulted`], and the larger of the two is
    /// used as the lower bound, so when one kind of sound effect is missed the other fills in.
    VaultLit,
    /// Vorax (S13) log cue: page, confirm, reroll, sound effect; see [`super::vorax_session::Cue`].
    Vorax(super::vorax_session::Cue),
    /// The accumulated season progress changed (the "37/48" under the Vorax and Tartarus icons in the game): `coin`
    /// is [`VORAX_COIN`] or [`TARTARUS_COIN`], `value` is the new value. See [`Charges`].
    Charges { coin: u16, value: u32 },
    /// The game's UI scale changed or was applied again: `micro` is the HUD page's ([`HUD_PAGE`]) `PageScale` in
    /// millionths (0.66702182888985 -> 667022). See [`ui_scale`].
    UiScale { micro: u32 },
    /// The riot battle after a run (level [`RIOT_LEVEL`]) had its special boss or the boss's chest; sent once the
    /// game leaves that level, see [`Riot`]. Only written to the run log: nothing is shown, read or advised on it.
    RiotBoss(RiotBoss),
}

/// The riot battle's level: the run's checkout (`S16GamePlayCheckOut`) starts it with
/// `Btn_S16GamePlay_CheckOut_StartChallenge`, the game logging `LevelMgr:_EnterLevelSuccess() 235501` and
/// `UGameMgr::EnterLevel(235501) mode=2`; the town (`110`) comes after it.
pub const RIOT_LEVEL: u32 = 235_501;
/// The riot battle's special boss (`Monster Created: npc id 5026001 rarity 4`, its skin `TaLuoSiDeYuZu`): one riot
/// battle in three on 2026-11-07, 35 s in, right after Liliah's line [`RIOT_BOSS_LINE`].
pub const RIOT_BOSS_NPC: u32 = 5_026_001;
/// Liliah's warning line 0.1 s before the boss comes: `DialogueDesc = 623136139 -> 623136139 Inmate power exceeds
/// expectations. Please exercise caution, Warden.`
const RIOT_BOSS_LINE: &str = "DialogueDesc = 623136139 ";
/// The boss's death shout, a FindRow warning line as the other sound effects the game has no row for.
const RIOT_BOSS_DIES: &str = "'Play_Boss_Elite_Shout_YuZu_Die'";
/// The drop carrier (chest) the boss leaves: `drop carrier triggered cfgId 109034`. Its 97 drops on 2026-11-07
/// against 0 to 28 for each of the other carriers of three riot battles.
pub const RIOT_BOSS_CHEST: u32 = 109_034;
/// How long after the boss's chest its drops (`UDropInstanceMgr@ AddInstance!` lines) are counted, unless another
/// carrier comes first: the 97 came within 0.3 s on 2026-11-07.
const CHEST_DROPS_WITHIN: i64 = 2_000;

/// What one riot battle's special boss did, see [`Signal::RiotBoss`]. Times are the game log lines' UTC ms (see
/// [`game_millis`]); None when a line had no time or did not come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RiotBoss {
    /// Bosses created ([`RIOT_BOSS_NPC`]); 0 when only the chest was seen.
    pub spawned: u32,
    /// When the first was created.
    pub spawned_at: Option<i64>,
    /// Liliah warned of it ([`RIOT_BOSS_LINE`]).
    pub warned: bool,
    /// The boss's death shout played.
    pub died: bool,
    /// When it first played.
    pub died_at: Option<i64>,
    /// Boss chests opened ([`RIOT_BOSS_CHEST`]).
    pub chests: u32,
    /// When the first chest opened.
    pub chest_at: Option<i64>,
    /// The drops of the boss's chests, see [`CHEST_DROPS_WITHIN`].
    pub chest_drops: u32,
}

/// Follows the game log line by line for the riot battle's special boss (see [`RIOT_LEVEL`]): only lines while the
/// game is in that level count, so a monster or chest of the same ids on another map is not taken. The level is
/// known from the level lines only, so a log followed from mid battle takes nothing until the next level.
#[derive(Default)]
pub struct Riot {
    level: Option<u32>,
    boss: RiotBoss,
    /// Drops are being counted for the chest opened at this time.
    counting: Option<i64>,
}

impl Riot {
    /// The [`Signal::RiotBoss`] of the riot battle just left, when the game left [`RIOT_LEVEL`] on this line and the
    /// boss or its chest was seen there.
    pub fn line(&mut self, line: &str) -> Option<Signal> {
        if let Some(level) = level_entered(line) {
            if self.level == Some(level) {
                return None;
            }
            let left = std::mem::take(&mut self.boss);
            self.level = Some(level);
            self.counting = None;
            return (left.spawned > 0 || left.chests > 0).then_some(Signal::RiotBoss(left));
        }
        if self.level != Some(RIOT_LEVEL) {
            return None;
        }
        let at = game_millis(line);
        let carrier = carrier(line);
        if let Some(opened) = self.counting {
            if carrier.is_some() || at.is_some_and(|at| at - opened > CHEST_DROPS_WITHIN) {
                self.counting = None;
            } else if line.contains("UDropInstanceMgr@ AddInstance!") {
                self.boss.chest_drops += 1;
            }
        }
        if carrier == Some(RIOT_BOSS_CHEST) {
            self.boss.chests += 1;
            self.boss.chest_at = self.boss.chest_at.or(at);
            // A chest line without a time counts its drops until the next carrier or the level's end.
            self.counting = Some(at.unwrap_or(i64::MAX - CHEST_DROPS_WITHIN));
        } else if monster(line) == Some(RIOT_BOSS_NPC) {
            self.boss.spawned += 1;
            self.boss.spawned_at = self.boss.spawned_at.or(at);
        } else if line.contains(RIOT_BOSS_LINE) {
            self.boss.warned = true;
        } else if line.contains(RIOT_BOSS_DIES) {
            self.boss.died = true;
            self.boss.died_at = self.boss.died_at.or(at);
        }
        None
    }
}

/// The number right after `key` in `line`, up to the first character that is not a digit.
fn number_after(line: &str, key: &str) -> Option<u32> {
    let rest = line.split_once(key)?.1;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The level a `LevelMgr:_EnterLevelSuccess() 235501` or `UGameMgr::EnterLevel(235501) mode=2 reload=0.` line enters.
fn level_entered(line: &str) -> Option<u32> {
    number_after(line, "LevelMgr:_EnterLevelSuccess() ").or_else(|| number_after(line, "UGameMgr::EnterLevel("))
}

/// The npc id of a `Monster Created: npc id 5026001 rarity 4` line.
fn monster(line: &str) -> Option<u32> {
    number_after(line, "Monster Created: npc id ")
}

/// The cfgId of a `drop carrier triggered cfgId 109034 ety id 5041` line.
fn carrier(line: &str) -> Option<u32> {
    number_after(line, "drop carrier triggered cfgId ")
}

/// The page whose `PageScale` is the game's UI scale setting: the HUD, applied on every level entered. The prison
/// board's pages (S16BoardMainCtrl and the like) always log 1 whatever the setting, so they are not taken. The Vorax
/// surgery pages (S13GamePlayMainCtrl, S13GamePlayRewardCtrl) log a `PageScale` of their own too (0.667, 0.841, 1.0
/// on 2026-11-06, equal to the HUD's): not taken here, but a source to cross-check the HUD's value against later.
pub const HUD_PAGE: &str = "FightCtrl";

/// The HUD's UI scale in millionths from one
/// `PageView@PageViewSetUIScale PageId = 27523_FightCtrl, PageScale = 0.66702182888985 0.667 0.66702182888985` line:
/// the first number is the scale applied to the page (the second is the setting as shown, the third the setting
/// unrounded). Measured 2026-11-06: 0.667 is near the smallest, about 0.842 the recommended, 1.0 the largest. Other
/// pages, and a value that is not a number or not in (0, 4], give None.
fn ui_scale(line: &str) -> Option<u32> {
    let rest = line.split("PageView@PageViewSetUIScale PageId = ").nth(1)?;
    let (page, scale) = rest.split_once(", PageScale = ")?;
    let name = page.split_once('_').map_or(page, |(_, name)| name).trim();
    if name != HUD_PAGE {
        return None;
    }
    let value: f64 = scale.split_whitespace().next()?.parse().ok()?;
    (value.is_finite() && value > 0.0 && value <= 4.0).then(|| (value * 1e6).round() as u32)
}

/// ID of the Vorax accumulated progress in the log: `ConsumMgr:ResreshConsumData Coin_826 Value 37` (checked on
/// 2026-11-04: every Vorax game deducts 12 at the moment it starts, matching the time of the vorax folder under
/// home).
pub const VORAX_COIN: u16 = 826;
/// Tartarus accumulated progress: `Coin_828` (every Tartarus game deducts 12, matching the time of runs under home).
pub const TARTARUS_COIN: u16 = 828;
/// Upper limit of the accumulated progress (fixed at 48 in the game, not written in the log).
pub const CHARGE_MAX: u32 = 48;
/// Amount used up to start one game.
pub const CHARGE_PER_RUN: u32 = 12;
/// At this value a nearly-full notice is shown (the user specified 46 on 2026-11-05).
pub const CHARGE_NEAR: u32 = 46;

/// Current values of the two accumulated progress counters; None when not yet seen in the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charges {
    pub vorax: Option<u32>,
    pub tartarus: Option<u32>,
}

/// State of one accumulated progress counter: normal, nearly full (>= [`CHARGE_NEAR`]), full (>= [`CHARGE_MAX`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    Normal,
    Near,
    Full,
}

impl Charges {
    /// Record one [`Signal::Charges`]; ignore IDs other than these two.
    pub fn set(&mut self, coin: u16, value: u32) {
        match coin {
            VORAX_COIN => self.vorax = Some(value),
            TARTARUS_COIN => self.tartarus = Some(value),
            _ => {}
        }
    }

    /// Both are known.
    pub fn complete(&self) -> bool {
        self.vorax.is_some() && self.tartarus.is_some()
    }
}

/// State of `value`, see [`Fill`].
pub fn fill(value: u32) -> Fill {
    if value >= CHARGE_MAX {
        Fill::Full
    } else if value >= CHARGE_NEAR {
        Fill::Near
    } else {
        Fill::Normal
    }
}

/// How many more games `value` allows.
pub fn runs_left(value: u32) -> u32 {
    value / CHARGE_PER_RUN
}

/// The (ID, value) of one `ConsumMgr:ResreshConsumData Coin_826 Value 37` line; only [`VORAX_COIN`] and
/// [`TARTARUS_COIN`] are recognized.
fn charge(line: &str) -> Option<(u16, u32)> {
    let rest = line.split("ConsumMgr:ResreshConsumData Coin_").nth(1)?;
    let (coin, value) = rest.split_once(" Value ")?;
    let coin: u16 = coin.trim().parse().ok()?;
    let value: u32 = value.split_whitespace().next()?.parse().ok()?;
    matches!(coin, VORAX_COIN | TARTARUS_COIN).then_some((coin, value))
}

/// The incidents whose popup plays a sound of its own,
/// `UI_S16_Prison_Incident_Window_` and the zh name's pinyin initials, as
/// matched to their popups on 2026-10-29. The others, Usurpation I to III
/// among them, play `_Gen`.
const INCIDENT_SOUNDS: [(&str, u32); 6] = [
    ("AXCZ", 203), // Behind Closed Doors, `暗箱操作`
    ("YFTX", 213), // Shared Fortune, `有福同享`
    ("CXXL", 222), // Execution Efficiency, `處刑效率`
    ("XB", 232),   // "Treasure" Hunt, `尋「寶」`
    ("LSFY", 242), // Chain Reaction, `連鎖反應`
    ("JY4", 255),  // Usurpation IV, `僭越 IV`
];

/// The decision cards a `UI_S16_Card_Up` brings: the game logs a
/// `[S16CardPose] SetSelected id=N_S16BoardActionCell` line per card in the
/// engine frame it plays the sound (1 to 3 cards at all 170 on 2026-10-29),
/// and again for each on hover.
#[derive(Default)]
pub struct Cards {
    /// The engine frame of the last pose lines, and their card ids.
    frame: Option<u32>,
    ids: Vec<String>,
}

impl Cards {
    /// The signal of one log line, as [`signal`], with the cards counted.
    pub fn signal(&mut self, line: &str) -> Option<Signal> {
        // [2026.10.29-23.42.08:803][611]
        let frame = line.get(25..).and_then(|s| s.strip_prefix('[')).and_then(|s| s.split(']').next()).and_then(|f| f.trim().parse().ok());
        if line.contains("[S16CardPose] SetSelected id=") {
            let id = line.split("id=").nth(1).and_then(|s| s.split('_').next()).unwrap_or_default();
            if frame != self.frame {
                (self.frame, self.ids) = (frame, Vec::new());
            }
            if !self.ids.iter().any(|i| i == id) {
                self.ids.push(id.to_string());
            }
            return None;
        }
        match signal(line)? {
            Signal::CardsUp(..) => Some(Signal::CardsUp(
                if frame.is_some() && frame == self.frame { self.ids.len() } else { 0 },
                super::latency::game_millis(line).unwrap_or(0),
            )),
            other => Some(other),
        }
    }
}

/// How long ago the game wrote a line stamped `at_ms` (UTC ms, see
/// [`super::latency::game_millis`]): the log comes 30 to 130 ms behind, which
/// counts when a screen's own timing matters. Zero for no stamp or a clock
/// off by more than 2 s.
pub fn age(at_ms: i64) -> Duration {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    match now - at_ms {
        ms @ 0..=2000 if at_ms > 0 => Duration::from_millis(ms as u64),
        _ => Duration::ZERO,
    }
}

/// The sound a log line says the game played, as `UI_S16_Prison_Task_Done`.
/// Each also has a `FindRow` warning line naming it `Play_UI_S16_...`, which
/// is not taken.
fn sound(line: &str) -> Option<&str> {
    let (_, sound) = line.split_once("AudioMgr@PlayAudio")?.1.split_once("==")?;
    Some(sound.trim())
}

/// The signal of one log line, if any. The game logs no board: no cell,
/// inmate or rarity, only what was clicked and the sounds it played.
pub fn signal(line: &str) -> Option<Signal> {
    if let Some((coin, value)) = charge(line) {
        return Some(Signal::Charges { coin, value });
    }
    if let Some(micro) = ui_scale(line) {
        return Some(Signal::UiScale { micro });
    }
    // The Vorax sound effects are FindRow warning lines, not AudioMgr lines, so check them first.
    if let Some(cue) = super::vorax_session::cue(line) {
        return Some(Signal::Vorax(cue));
    }
    // A line the game writes when it cannot find this animation, but it only appears when a vault cell lights up.
    if line.contains("Animation not found! Name = Anim_Archive_LightUp") {
        return Some(Signal::VaultLit);
    }
    if let Some(sound) = sound(line) {
        return match sound {
            "UI_S16_Prison_Task_Done" => Some(Signal::ObjectiveDone),
            "UI_S16_Eating_red_screen" => Some(Signal::Consume),
            "UI_S16_Click_Eat" => Some(Signal::Bite),
            "UI_S16_Click_Prison_Cell" => Some(Signal::CellPicked),
            "UI_S16_Click_Cancel" => Some(Signal::ChainEnd),
            "UI_S16_Card_Up" => Some(Signal::CardsUp(0, 0)),
            "UI_S16_Click_Go_Cell" => Some(Signal::Moved),
            "UI_S16_Torture_Click_Reset" => Some(Signal::Reroll),
            "UI_S16_Torture_Effect" => Some(Signal::Instrument),
            "UI_S16_Premium_Quality_Fly" => Some(Signal::Vaulted),
            _ => sound
                .strip_prefix("UI_S16_Prison_Incident_Window_")
                .and_then(|code| INCIDENT_SOUNDS.iter().find(|&&(c, _)| c == code))
                .map(|&(_, id)| Signal::Incident(id)),
        };
    }
    if line.contains("normal-click Btn_S16Board_ColoredPop_Confirm") {
        return Some(Signal::Treasure);
    }
    if line.contains("normal-click Btn_S16GamePlay_Start") {
        return Some(Signal::RunStart);
    }
    if line.contains("normal-click Btn_S16Board_ActionCell") || line.contains("normal-click Btn_S16Board_RouteCell") {
        return Some(Signal::Decision);
    }
    if line.contains("normal-click Btn_S16Board_") || line.contains("normal-click S16ViewBoardBtn_") {
        return Some(Signal::Click);
    }
    if line.contains("[S16LastDay] check ") {
        let number = |key: &str| line.split(key).nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok();
        return Some(Signal::Day { cur: number("cur=")?, max: number("max=")? });
    }
    if !line.contains("PageRunState = Run") {
        return None;
    }
    if line.contains("PageName = S16BoardThemePop ") {
        Some(Signal::Theme)
    } else if line.contains("PageName = S16GamePlayCheckOut ") {
        Some(Signal::RunEnd)
    } else if line.contains("PageName = S16BoardColoredPop ") {
        Some(Signal::TreasureOpen)
    } else if line.contains("PageName = S16BoardMain ") || line.contains("PageName = S16GamePlayEntry ") {
        Some(Signal::Prison)
    } else {
        None
    }
}

/// Where the game keeps its log under a Steam library.
const LOG: &str = r"steamapps\common\Torchlight Infinite\UE_game\TorchLight\Saved\Logs\UE_game.log";
/// Where the game keeps its log under its `UE_game` folder (the exe is in `UE_game\Binaries\Win64`), and under
/// %LOCALAPPDATA% when the install folder cannot be written to.
const LOG_IN_GAME: &str = r"TorchLight\Saved\Logs\UE_game.log";

/// Where the log being followed came from.
#[derive(Clone, Debug, PartialEq)]
pub enum LogSource {
    /// The file chosen in the settings.
    Manual,
    /// Worked out from the game window's exe ([`from_exe`]).
    GameFolder,
    /// %LOCALAPPDATA%\TorchLight\Saved\Logs.
    LocalAppData,
    /// A Steam library folder.
    Steam,
}

/// What the log follower is doing, for the settings page and the log light.
#[derive(Clone, Debug, PartialEq)]
pub enum LogStatus {
    /// Automatic: no game window and no log found yet; looked for again every [`LOG_RETRY`].
    Searching,
    /// Automatic: the game window is open but no candidate log exists (the log is off, or kept somewhere else).
    Missing,
    /// The file chosen in the settings does not exist; looked for again every [`LOG_RETRY`], with no fallback to
    /// the automatic search.
    ManualMissing(PathBuf),
    /// Following this file, but the game window is not open (nothing new is expected).
    GameClosed { path: PathBuf, source: LogSource },
    /// Following this file while the game runs, and it is being written.
    Following { path: PathBuf, source: LogSource },
    /// Following this file while the game runs, but it is not being written (the game's "open log" option is
    /// probably off), see [`follow_status`].
    Stale { path: PathBuf, source: LogSource },
}

/// How often a log that is not found is looked for again.
pub const LOG_RETRY: Duration = Duration::from_secs(3);
/// No new line for this long while the game runs means the log is not being written. Measured on the user's logs
/// (2026-11-05): once logged in the game writes every second (TCP pings, heartbeats; the longest gap in 344 minutes
/// was 11.6 s), but the login screen was silent for 117 s.
pub const STALE_AFTER: Duration = Duration::from_secs(180);
/// A log last written before the game process started is an old one (this session writes no log); it counts as
/// stale once the game has run this long.
pub const STALE_GRACE: Duration = Duration::from_secs(60);

/// The status of a followed log: `game_started` is when the game process started (None when its window is not
/// open), `since_new` how long the log has had nothing new, `modified` its last write time.
pub fn follow_status(
    path: &Path,
    source: &LogSource,
    game_started: Option<std::time::SystemTime>,
    since_new: Duration,
    modified: Option<std::time::SystemTime>,
    now: std::time::SystemTime,
) -> LogStatus {
    let (path, source) = (path.to_path_buf(), source.clone());
    let Some(started) = game_started else { return LogStatus::GameClosed { path, source } };
    let running = now.duration_since(started).unwrap_or_default();
    let old_file = modified.is_some_and(|m| m < started) && running >= STALE_GRACE;
    if since_new >= STALE_AFTER || old_file { LogStatus::Stale { path, source } } else { LogStatus::Following { path, source } }
}

/// The log of the game whose exe is `exe`:
/// `<install>\UE_game\Binaries\Win64\torchlight_infinite.exe` -> `<install>\UE_game\TorchLight\Saved\Logs\UE_game.log`.
pub fn from_exe(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.parent()?.join(LOG_IN_GAME))
}

/// The automatic candidates, in order: next to the game's exe (when its window is found), %LOCALAPPDATA%, then
/// Steam's library folders.
fn candidates(exe: Option<&Path>) -> Vec<(PathBuf, LogSource)> {
    let mut found: Vec<(PathBuf, LogSource)> = exe.and_then(from_exe).map(|p| (p, LogSource::GameFolder)).into_iter().collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        found.push((PathBuf::from(local).join(LOG_IN_GAME), LogSource::LocalAppData));
    }
    let steam = steam_dir();
    let vdf = std::fs::read_to_string(steam.join(r"steamapps\libraryfolders.vdf")).unwrap_or_default();
    found.extend(std::iter::once(steam).chain(library_paths(&vdf)).map(|lib| (lib.join(LOG), LogSource::Steam)));
    found
}

/// The log to follow: the file chosen in the settings when there is one (only that file), else the first
/// automatic candidate that exists ([`candidates`]). `exe` is the running game's exe (None when its window is not
/// open). The error is the status to show while there is none.
pub fn locate(manual: Option<&Path>, exe: Option<&Path>) -> Result<(PathBuf, LogSource), LogStatus> {
    match manual {
        Some(path) if path.is_file() => Ok((path.to_path_buf(), LogSource::Manual)),
        Some(path) => Err(LogStatus::ManualMissing(path.to_path_buf())),
        None => candidates(exe).into_iter().find(|(p, _)| p.is_file()).ok_or(if exe.is_some() { LogStatus::Missing } else { LogStatus::Searching }),
    }
}

/// The game's log found without a game window (%LOCALAPPDATA%, then Steam's library folders); None when it is in
/// none of them. Used by the command line tool.
pub fn find() -> Option<PathBuf> {
    locate(None, None).ok().map(|(path, _)| path)
}

/// The `"path"` values of Steam's libraryfolders.vdf.
fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let mut parts = line.split('"').filter(|s| !s.trim().is_empty());
            let (key, value) = (parts.next()?, parts.next()?);
            (key == "path").then(|| PathBuf::from(value.replace(r"\\", r"\")))
        })
        .collect()
}

/// Steam's folder as Steam records it for the user, else its default one.
fn steam_dir() -> PathBuf {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;
    let mut buf = [0u16; 520];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: the buffer and its size in bytes outlive the call.
    let read = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, w!(r"Software\Valve\Steam"), w!("SteamPath"), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr().cast()), Some(&mut size))
    };
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    if read.is_ok() && len > 0 {
        PathBuf::from(String::from_utf16_lossy(&buf[..len]))
    } else {
        PathBuf::from(r"C:\Program Files (x86)\Steam")
    }
}

/// How often the log is looked at.
const POLL: Duration = Duration::from_millis(50);
/// How long the followed log may be missing before [`follow`] gives up on it (a restart renames it only briefly).
#[cfg(not(test))]
const LOG_GONE: Duration = Duration::from_secs(10);
#[cfg(test)]
const LOG_GONE: Duration = Duration::from_secs(1);
/// The most of the log read at once: after a pause only its newest part matters.
const MAX_READ: u64 = 1 << 20;

/// The piece of the log [`day_under_way`] reads at a time, going back.
const DAY_LOOKBACK: u64 = 8 << 20;

/// The last day signal in `text`, if any.
fn last_day(text: &str) -> Option<Signal> {
    text.lines().rev().filter(|l| l.contains("[S16LastDay] check ")).find_map(signal)
}

/// The last day signal in the first `end` bytes of `file`, read back from
/// there a piece at a time: the day changes only with a new line, however
/// long ago the last was written. Maps between runs write fast: 33 MB came
/// after a run's last day in 7 minutes (2026-10-28, a 929 MB log). Stops early when `stopped`, between pieces, so a
/// long look back does not hold up the Watcher's drop.
fn day_under_way(file: &mut std::fs::File, end: u64, stopped: &AtomicBool) -> Option<Signal> {
    let mut to = end;
    while to > 0 && !stopped.load(Ordering::Relaxed) {
        let from = to.saturating_sub(DAY_LOOKBACK);
        let mut buf = Vec::new();
        file.seek(SeekFrom::Start(from)).ok()?;
        (&mut *file).take(to - from).read_to_end(&mut buf).ok()?;
        // A piece starts mid-line but for the first: its next piece holds that line.
        let start = if from == 0 { 0 } else { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |k| k + 1) };
        if let Some(day) = last_day(&String::from_utf8_lossy(&buf[start..])) {
            return Some(day);
        }
        // A piece without a line end is one long line: the piece before it is next.
        to = if start == buf.len() { from } else { from + start as u64 };
        if from == 0 {
            break;
        }
    }
    None
}

/// The last values of the two accumulated progress counters within the first `end` bytes of `file`, read backwards
/// chunk by chunk (same as [`day_under_way`]), stopping once both are found. When the game starts it lists all Coin
/// rows, and afterwards writes only on change, so they can be found even if the program is started midway; the one
/// absent from the whole log is None.
///
/// After starting the game and before playing Vorax or Tartarus, this reads all the way to the file head (330 MB is
/// about 42 chunks): each chunk is first searched in bytes for `ResreshConsumData` and is not converted to a string
/// if absent; `stopped` is checked between chunks, so turning off auto read or closing the program does not block
/// the Watcher's join (pointed out in the 2026-11-05 review).
fn charges_under_way(file: &mut std::fs::File, end: u64, stopped: &AtomicBool) -> Charges {
    const NEEDLE: &[u8] = b"ResreshConsumData";
    let mut found = Charges::default();
    let mut to = end;
    while to > 0 && !found.complete() && !stopped.load(Ordering::Relaxed) {
        let from = to.saturating_sub(DAY_LOOKBACK);
        let mut buf = Vec::new();
        if file.seek(SeekFrom::Start(from)).is_err() || (&mut *file).take(to - from).read_to_end(&mut buf).is_err() {
            break;
        }
        let start = if from == 0 { 0 } else { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |k| k + 1) };
        let piece = &buf[start..];
        let lines = if piece.windows(NEEDLE.len()).any(|w| w == NEEDLE) { String::from_utf8_lossy(piece) } else { std::borrow::Cow::Borrowed("") };
        for line in lines.lines().rev() {
            if let Some((coin, value)) = charge(line) {
                let mut one = Charges::default();
                one.set(coin, value);
                found.vorax = found.vorax.or(one.vorax);
                found.tartarus = found.tartarus.or(one.tartarus);
            }
        }
        to = if start == buf.len() { from } else { from + start as u64 };
        if from == 0 {
            break;
        }
    }
    found
}

/// How far back [`ui_scale_under_way`] looks: the HUD's scale line comes on every level entered (911 scale lines,
/// 190 of them the HUD's, in a 250 MB log on 2026-11-06), so one is normally within the last few MB.
const UI_SCALE_LOOKBACK: u64 = 8 * DAY_LOOKBACK;

/// The last HUD UI scale ([`ui_scale`]) within the first `end` bytes of `file`, read back a piece at a time as
/// [`day_under_way`] does, at most [`UI_SCALE_LOOKBACK`] bytes; a piece without the line's marker is not turned into
/// text. Stops early when `stopped`.
fn ui_scale_under_way(file: &mut std::fs::File, end: u64, stopped: &AtomicBool) -> Option<u32> {
    const NEEDLE: &[u8] = b"_FightCtrl, PageScale = ";
    let floor = end.saturating_sub(UI_SCALE_LOOKBACK);
    let mut to = end;
    while to > floor && !stopped.load(Ordering::Relaxed) {
        let from = to.saturating_sub(DAY_LOOKBACK).max(floor);
        let mut buf = Vec::new();
        file.seek(SeekFrom::Start(from)).ok()?;
        (&mut *file).take(to - from).read_to_end(&mut buf).ok()?;
        let start = if from == 0 { 0 } else { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |k| k + 1) };
        let piece = &buf[start..];
        if piece.windows(NEEDLE.len()).any(|w| w == NEEDLE)
            && let Some(micro) = String::from_utf8_lossy(piece).lines().rev().find_map(ui_scale)
        {
            return Some(micro);
        }
        to = if start == buf.len() { from } else { from + start as u64 };
        if from == floor {
            break;
        }
    }
    None
}

/// Follows the log at `path` from its end until `stopped`, calling `on` for
/// each signal with its line's time (UTC ms, [`game_millis`]). The day under way comes first, from the log already
/// written: a session started mid-run knew no day until the next began,
/// and read its countdown off the screen (2026-10-28). A log that shrinks
/// was started anew (the game restarted) and is followed from its start. Returns when `stopped`, or when the log
/// has been missing for [`LOG_GONE`] so the caller can look for it again. `tick` runs on every look at the log with
/// how long it has had nothing new (see [`follow_status`]).
pub fn follow(path: &Path, stopped: &AtomicBool, mut on: impl FnMut(Signal, Option<i64>), mut tick: impl FnMut(Duration)) {
    let mut at = std::fs::metadata(path).map_or(0, |m| m.len());
    if let Ok(mut file) = std::fs::File::open(path)
        && let Some(day) = day_under_way(&mut file, at, stopped)
    {
        on(day, None);
    }
    // When the program is started midway, whether a Vorax is in progress (looking back only the last 8 MB).
    if let Ok(mut file) = std::fs::File::open(path)
        && let Some(cue) = super::vorax_session::under_way(&mut file, at)
    {
        on(Signal::Vorax(cue), None);
    }
    // The UI scale in use (looking back at most UI_SCALE_LOOKBACK): Vorax logs it on entering its mode.
    if let Ok(mut file) = std::fs::File::open(path)
        && let Some(micro) = ui_scale_under_way(&mut file, at, stopped)
    {
        on(Signal::UiScale { micro }, None);
    }
    // Current values of the two accumulated progress counters (searched backwards until found; may read the whole
    // log, so it comes after Vorax).
    if let Ok(mut file) = std::fs::File::open(path) {
        let found = charges_under_way(&mut file, at, stopped);
        for (coin, value) in [(VORAX_COIN, found.vorax), (TARTARUS_COIN, found.tartarus)] {
            if let Some(value) = value {
                on(Signal::Charges { coin, value }, None);
            }
        }
    }
    let mut partial = String::new();
    let mut cards = Cards::default();
    let mut riot = Riot::default();
    let mut missing_since: Option<std::time::Instant> = None;
    let mut last_new = std::time::Instant::now();
    while !stopped.load(Ordering::Relaxed) {
        std::thread::sleep(POLL);
        tick(last_new.elapsed());
        let Ok(mut file) = std::fs::File::open(path) else {
            // Gone for good (the game moved or was reinstalled), not just being rotated by a restart: return so
            // the caller looks for the log again.
            if missing_since.get_or_insert_with(std::time::Instant::now).elapsed() >= LOG_GONE {
                return;
            }
            continue;
        };
        missing_since = None;
        let len = file.metadata().map_or(0, |m| m.len());
        if len < at {
            (at, partial) = (0, String::new());
            // A new log (the game restarted, maybe mid-battle): a battle the old log left open ended with it.
            riot = Riot::default();
        }
        if len == at {
            continue;
        }
        last_new = std::time::Instant::now();
        if len - at > MAX_READ {
            (at, partial) = (len - MAX_READ, String::new());
        }
        let mut buf = Vec::new();
        if file.seek(SeekFrom::Start(at)).is_err() || file.take(len - at).read_to_end(&mut buf).is_err() {
            continue;
        }
        at += buf.len() as u64;
        partial.push_str(&String::from_utf8_lossy(&buf));
        // The last line may still be written: it waits for its end.
        let complete = partial.rfind('\n').map_or(0, |k| k + 1);
        for line in partial[..complete].lines() {
            if let Some(boss) = riot.line(line) {
                on(boss, game_millis(line));
            }
            if let Some(signal) = cards.signal(line) {
                on(signal, game_millis(line));
            }
        }
        partial.drain(..complete);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The riot boss signals of a game log's lines, as [`follow`] sends them.
    fn riot_of(text: &str) -> Vec<Signal> {
        let mut riot = Riot::default();
        text.lines().filter_map(|line| riot.line(line)).collect()
    }

    /// The 2026-11-07 14:39 riot battle (tests/fixtures/riot-boss-C.log: its level, monster, dialogue, boss shout,
    /// carrier and drop lines from UE_game.log, and the town's level lines after it): one boss, warned, killed, and
    /// its chest's 97 drops; the 2 drops after the 2 s are not the chest's. The signal comes as the game enters the
    /// town, once.
    #[test]
    fn the_riot_boss_of_a_real_battle() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/riot-boss-C.log")).unwrap();
        let ms = |stamp: &str| game_millis(&format!("[2026.11.07-{stamp}][1]")).unwrap();
        let boss = RiotBoss {
            spawned: 1,
            spawned_at: Some(ms("14.39.55:306")),
            warned: true,
            died: true,
            died_at: Some(ms("14.39.56:228")),
            chests: 1,
            chest_at: Some(ms("14.39.58:053")),
            chest_drops: 97,
        };
        assert_eq!(riot_of(&text), [Signal::RiotBoss(boss)]);
        let town = text.lines().position(|l| l.contains("_EnterLevelSuccess() 110")).unwrap();
        let mut riot = Riot::default();
        let sent: Vec<usize> = text.lines().enumerate().filter_map(|(k, line)| riot.line(line).map(|_| k)).collect();
        assert_eq!(sent, [town], "sent on the town's first level line");
    }

    /// The 2026-11-07 14:30 riot battle had no boss (tests/fixtures/riot-boss-A.log, the same lines): no signal,
    /// though its other monsters and carriers drop as the boss's would.
    #[test]
    fn no_riot_boss_in_a_battle_without_one() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/riot-boss-A.log")).unwrap();
        assert!(text.contains("Monster Created: npc id ") && text.contains("UDropInstanceMgr@ AddInstance!"));
        assert_eq!(riot_of(&text), []);
    }

    /// Only the riot level counts, and only the boss's own ids: the same lines on another map, another npc or
    /// carrier whose id starts with the boss's, or a log followed from mid battle give nothing. A battle left
    /// without its level line (the log followed from the middle) is not taken either.
    #[test]
    fn the_riot_boss_only_in_the_riot_level() {
        let line = |stamp: &str, text: &str| format!("[2026.11.07-14.39.{stamp}][805]TLGame: Display: [Game] {text}\n");
        let boss = line("55:306", "Monster Created: npc id 5026001 rarity 4");
        let chest = line("58:053", "drop carrier triggered cfgId 109034 ety id 5041");
        let drop = line("58:313", "UDropInstanceMgr@ AddInstance! InstanceId = 507 InstanceIndex = 0");
        let enter = |level: &str| line("20:819", &format!("LevelMgr:_EnterLevelSuccess() {level}"));
        let town = enter("110");
        // Another map.
        assert_eq!(riot_of(&format!("{}{boss}{chest}{drop}{}", enter("5301"), enter("110"))), []);
        // No level line seen: the log followed from mid battle.
        assert_eq!(riot_of(&format!("{boss}{chest}{drop}{town}")), []);
        // Ids that only start like the boss's.
        let others = format!("{}{}", line("55:306", "Monster Created: npc id 50260011 rarity 4"), line("58:053", "drop carrier triggered cfgId 1090345 ety id 1"));
        assert_eq!(riot_of(&format!("{}{others}{drop}{town}", enter("235501"))), []);
        // The chest alone still counts, with its drop; the level's second line does not start it again.
        let again = line("23:459", "UGameMgr::EnterLevel(235501) mode=2 reload=0.");
        let got = riot_of(&format!("{}{again}{chest}{drop}{town}", enter("235501")));
        assert!(matches!(got.as_slice(), [Signal::RiotBoss(RiotBoss { spawned: 0, chests: 1, chest_drops: 1, died: false, .. })]), "{got:?}");
        // Another carrier ends the count, as do 2 s.
        let next = line("58:400", "drop carrier triggered cfgId 109006 ety id 9");
        let late = "[2026.11.07-14.40.00:200][805]TLGame: Display: [Game] UDropInstanceMgr@ AddInstance! InstanceId = 600 InstanceIndex = 0\n";
        let got = riot_of(&format!("{}{boss}{chest}{drop}{next}{drop}{town}", enter("235501")));
        assert!(matches!(got.as_slice(), [Signal::RiotBoss(RiotBoss { spawned: 1, chest_drops: 1, .. })]), "{got:?}");
        let got = riot_of(&format!("{}{boss}{chest}{drop}{}{late}{town}", enter("235501"), line("58:900", "UDropInstanceMgr@ AddInstance! InstanceId = 508 InstanceIndex = 1")));
        assert!(matches!(got.as_slice(), [Signal::RiotBoss(RiotBoss { chest_drops: 2, .. })]), "{got:?}");
    }

    /// Original text of the 2026-11-04 log: accumulated progress of Vorax (Coin_826) and Tartarus (Coin_828); other
    /// Coins do not count.
    #[test]
    fn charges_of_log_lines() {
        assert_eq!(signal("[2026.11.04-15.46.15:818][479]TLLua: Display: [Game] ConsumMgr:ResreshConsumData Coin_826 Value 48"), Some(Signal::Charges { coin: VORAX_COIN, value: 48 }));
        assert_eq!(signal("[2026.11.04-15.46.15:819][479]TLLua: Display: [Game] ConsumMgr:ResreshConsumData Coin_828 Value 36"), Some(Signal::Charges { coin: TARTARUS_COIN, value: 36 }));
        assert_eq!(signal("[2026.11.04-15.46.15:819][479]TLLua: Display: [Game] ConsumMgr:ResreshConsumData Coin_807 Value 7620"), None);
        assert_eq!(signal("[2026.11.04-15.46.15:819][479]TLLua: Display: [Game] ConsumMgr:ResreshConsumData Coin_826 Value"), None);
        let mut c = Charges::default();
        c.set(VORAX_COIN, 37);
        c.set(807, 7620);
        assert_eq!(c, Charges { vorax: Some(37), tartarus: None });
        assert_eq!([fill(37), fill(45), fill(46), fill(47), fill(48)], [Fill::Normal, Fill::Normal, Fill::Near, Fill::Near, Fill::Full]);
        assert_eq!([runs_left(11), runs_left(12), runs_left(37), runs_left(48)], [0, 1, 3, 4]);
    }

    /// The HUD's UI scale lines, as logged on 2026-11-06: only FightCtrl is taken; the prison board's pages (always 1)
    /// and other pages are not; broken lines give None and never panic.
    #[test]
    fn ui_scale_of_log_lines() {
        let real = "[2026.11.06-02.31.54:178][624]TLLua: Display: [Game] PageView@PageViewSetUIScale PageId = 27523_FightCtrl, PageScale = 0.66702182888985 0.667 0.66702182888985";
        assert_eq!(signal(real), Some(Signal::UiScale { micro: 667_022 }));
        let page = |page: &str, scale: &str| format!("[2026.11.06-04.41.02:709][407]TLLua: Display: [Game] PageView@PageViewSetUIScale PageId = {page}, PageScale = {scale}");
        assert_eq!(signal(&page("745_FightCtrl", "0.84240893363953 0.842 0.84240893363953")), Some(Signal::UiScale { micro: 842_409 }));
        assert_eq!(signal(&page("745_FightCtrl", "1.0 1.000 1.0")), Some(Signal::UiScale { micro: 1_000_000 }));
        // Board pages always log 1, whatever the setting: ignored, as are the other pages.
        assert_eq!(signal(&page("2210_S16BoardMainCtrl", "1 0.842 0.84240893363953")), None);
        assert_eq!(signal(&page("2210_S16BoardNewDayPopCtrl", "1 1.000 1.0")), None);
        assert_eq!(signal(&page("2210_S13GamePlayMainCtrl", "0.8407407283783 0.841 0.8407407283783")), None);
        assert_eq!(signal(&page("466_NoticeCtrl", "0.66702182888985 0.667 0.66702182888985")), None);
        assert_eq!(signal(&page("27523_FightCtrlX", "0.667 0.667 0.667")), None);
        // Broken lines.
        for scale in ["", " ", "abc", "-1 0.667 0.667", "0 0 0", "NaN", "inf", "1e9", "0.667garbage"] {
            assert_eq!(signal(&page("745_FightCtrl", scale)), None, "{scale:?}");
        }
        assert_eq!(signal("PageView@PageViewSetUIScale PageId = 745_FightCtrl"), None);
        assert_eq!(signal("PageView@PageViewSetUIScale PageId = , PageScale = 0.667"), None);
        assert_eq!(signal("PageView@PageViewSetUIScale PageId = FightCtrl, PageScale = 0.667"), Some(Signal::UiScale { micro: 667_000 }));
        assert_eq!(signal("PageView@PageViewSetUIScale PageId = 745_FightCtrl, PageScale = 0.66702182888985"), Some(Signal::UiScale { micro: 667_022 }));
    }

    /// The UI scale in use is found going back: the last HUD line, none when there is none within reach.
    #[test]
    fn ui_scale_is_found_going_back() {
        let path = std::env::temp_dir().join(format!("ascendant-uiscale-{}.log", std::process::id()));
        let line = |page: &str, value: &str| format!("[2026.11.06-02.31.54:178][624]TLLua: Display: [Game] PageView@PageViewSetUIScale PageId = {page}, PageScale = {value} 0.667 0.66702182888985\n");
        let filler = "[2026.11.06-02.31.54:178][1]TLGame: Display: [Game] something else\n".repeat(20_000);
        let text = format!("{}{}{}{}", line("745_FightCtrl", "0.66702182888985"), filler, line("745_FightCtrl", "1.0"), line("801_S16BoardMainCtrl", "1"));
        std::fs::write(&path, &text).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(ui_scale_under_way(&mut file, text.len() as u64, &AtomicBool::new(false)), Some(1_000_000));
        // Only the part before the second line: the first value.
        let first = line("745_FightCtrl", "0.66702182888985").len() + filler.len();
        assert_eq!(ui_scale_under_way(&mut file, first as u64, &AtomicBool::new(false)), Some(667_022));
        assert_eq!(ui_scale_under_way(&mut file, text.len() as u64, &AtomicBool::new(true)), None);
        std::fs::write(&path, &filler).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert_eq!(ui_scale_under_way(&mut file, filler.len() as u64, &AtomicBool::new(false)), None);
        let _ = std::fs::remove_file(&path);
    }

    /// Search backwards: take each one's last value; when only one is found the other is None.
    #[test]
    fn charges_are_found_going_back() {
        let path = std::env::temp_dir().join(format!("ascendant-charges-{}.log", std::process::id()));
        let line = |coin: u16, value: u32| format!("[2026.11.04-19.46.41:446][1]TLLua: Display: [Game] ConsumMgr:ResreshConsumData Coin_{coin} Value {value}\n");
        let filler = "[2026.11.04-19.46.41:446][1]TLGame: Display: [Game] something else\n".repeat(20_000);
        let text = format!("{}{}{}{}{}{}", line(VORAX_COIN, 36), line(TARTARUS_COIN, 12), filler, line(VORAX_COIN, 37), filler, line(807, 5));
        std::fs::write(&path, &text).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let found = charges_under_way(&mut file, text.len() as u64, &AtomicBool::new(false));
        assert_eq!(found, Charges { vorax: Some(37), tartarus: Some(12) });
        std::fs::write(&path, format!("{}{}", filler, line(TARTARUS_COIN, 48))).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let len = file.metadata().unwrap().len();
        assert_eq!(charges_under_way(&mut file, len, &AtomicBool::new(false)), Charges { vorax: None, tartarus: Some(48) });
        // Already stopping: do not read, nothing found.
        assert_eq!(charges_under_way(&mut file, len, &AtomicBool::new(true)), Charges::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn signals_of_log_lines() {
        let line = |page: &str, state: &str| format!("[2026.10.28-15.24.42:062][871]LogTemp: ShowMgr@DispatchPageRunChange PageName = {page} , PageRunState = {state} ");
        assert_eq!(signal(&line("S16BoardThemePop", "Run")), Some(Signal::Theme));
        assert_eq!(signal(&line("S16BoardThemePop", "Destory")), None);
        assert_eq!(signal(&line("S16BoardMain", "Run")), Some(Signal::Prison));
        assert_eq!(signal(&line("S16GamePlayEntry", "Run")), Some(Signal::Prison));
        assert_eq!(signal(&line("S16BoardTaskPop", "Run")), None);
        assert_eq!(signal("[2026.10.28-15.24.42:993][  4]UIButton@ normal-click Btn_S16Board_ThemePop_Confirm"), Some(Signal::Click));
        assert_eq!(signal("[2026.10.28-15.59.16:911][887]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_ColoredPop_Confirm"), Some(Signal::Treasure));
        assert_eq!(signal("[2026.10.28-15.59.16:911][887]LogDataTable: Warning: UDataTable::FindRow : 'AudioMgr' requested row 'S16BoardColoredPop_C_Btn_S16Board_ColoredPop_Confirm' not in DataTable"), None);
        assert_eq!(signal("[2026.10.28-17.06.32:995][123]TLLua: Display: [Game] [S16LastDay] check cur=17 max=20 played=false"), Some(Signal::Day { cur: 17, max: 20 }));
        assert_eq!(signal("[2026.10.28-19.39.40:636][1]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_Cell"), Some(Signal::Click));
        assert_eq!(signal("[2026.10.29-12.30.36:008][1]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_ActionCell"), Some(Signal::Decision));
        assert_eq!(signal("[2026.10.29-12.30.33:644][1]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_RouteCell"), Some(Signal::Decision));
        assert_eq!(signal("[2026.10.28-19.39.40:636][1]LogDataTable: Warning: UDataTable::FindRow : 'AudioMgr' requested row 'S16BoardMain_C_Btn_S16Board_Cell' not in DataTable"), None);
        let sound = |name: &str| format!("[2026.10.28-19.42.32:261][ 61]TLLua: Display: [Game] AudioMgr@PlayAudio  AudioEnum  == {name}");
        assert_eq!(signal(&sound("UI_S16_Prison_Task_Done")), Some(Signal::ObjectiveDone));
        assert_eq!(signal(&sound("UI_S16_Eating_red_screen")), Some(Signal::Consume));
        assert_eq!(signal(&sound("UI_S16_Click_Eat")), Some(Signal::Bite));
        assert_eq!(signal(&sound("UI_S16_Click_Cancel")), Some(Signal::ChainEnd));
        assert_eq!(signal(&sound("UI_S16_Card_Up")), Some(Signal::CardsUp(0, 0)));
        assert_eq!(signal(&sound("UI_S16_Prison_Incident_Window_LSFY")), Some(Signal::Incident(242)));
        assert_eq!(signal(&sound("UI_S16_Prison_Incident_Window_JY4")), Some(Signal::Incident(255)));
        assert_eq!(signal(&sound("UI_S16_Prison_Incident_Window_Gen")), None);
        assert_eq!(signal(&sound("UI_S16_Click_Go_Cell")), Some(Signal::Moved));
        assert_eq!(signal(&sound("UI_S16_Torture_Click_Reset")), Some(Signal::Reroll));
        assert_eq!(signal(&sound("UI_S16_Torture_Effect")), Some(Signal::Instrument));
        assert_eq!(signal(&sound("UI_S16_Premium_Quality_Fly")), Some(Signal::Vaulted));
        let lit = "[2026.11.02-22.51.03:999][175]TLShipping: Display: [Game] AnimationMgr@ Animation not found! Name = Anim_Archive_LightUp, Id = 1321_S16BoardArchiveCell_0000022D1EE273D8_Anim_Archive_LightUp";
        assert_eq!(signal(lit), Some(Signal::VaultLit));
        assert_eq!(signal(&sound("UI_S16_Click_Prison_Cell")), Some(Signal::CellPicked));
        assert_eq!(signal(&sound("UI_S16_Premium_Quality")), None);
        assert_eq!(signal(&sound("UI_S16_EatX2")), None);
        assert_eq!(signal("[2026.10.28-19.42.32:261][ 61]LogDataTable: Warning: UDataTable::FindRow : 'Play_UI_S16_Prison_Task_Done' requested row 'Play_UI_S16_Prison_Task_Done' not in DataTable"), None);
        assert_eq!(signal("[2026.10.28-19.42.15:681][1]TLGame: Display: [Game] UIButton@ normal-click Btn_S16GamePlay_Start"), Some(Signal::RunStart));
        assert_eq!(signal(&line("S16GamePlayCheckOut", "Run")), Some(Signal::RunEnd));
        assert_eq!(signal(&line("S16BoardColoredPop", "Run")), Some(Signal::TreasureOpen));
        assert_eq!(signal(&line("S16BoardColoredPop", "Destory")), None);
        assert_eq!(signal("[2026.10.28-19.48.11:890][1]TLGame: Display: [Game] UIButton@ normal-click Btn_S16GamePlay_CheckOut_StartChallenge"), None);
        // Vorax lines become Signal::Vorax; the S13 PlayAudio lines are not taken (only FindRow lines count).
        use super::super::vorax_session::{Cue, Page, PageState};
        assert_eq!(
            signal("[2026.11.03-06.25.46:278][268]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S13GamePlayMain , PageRunState = Run "),
            Some(Signal::Vorax(Cue::Page(Page::Main, PageState::Run)))
        );
        assert_eq!(signal("[2026.11.03-06.26.02:310][418]TLLua: Display: [Game] AudioMgr@PlayAudio  AudioEnum  == S13_KYZ_Card_Monster_DisAppear"), None);
        // Tartarus lines go through the Vorax matching and then stay as before.
        assert_eq!(
            signal("[2026.10.28-15.24.42:062][871]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = S16BoardColoredPop , PageRunState = Run "),
            Some(Signal::TreasureOpen)
        );
    }

    #[test]
    fn cards_counted() {
        let pose = |frame: u32, id: u32| format!("[2026.10.29-23.42.08:803][{frame}]TLLua: Display: [Game] [S16CardPose] SetSelected id={id}_S16BoardActionCell item=true rt=0.0,0.0 pace=1 sel=false hover=false group=false | sel=false prev=false trans=false");
        let up = |frame: u32| format!("[2026.10.29-23.42.08:803][{frame}]TLLua: Display: [Game] AudioMgr@PlayAudio  AudioEnum  == UI_S16_Card_Up");
        // 2026-10-29 23:42:08.803 UTC.
        const STAMP: i64 = 1_793_317_328_803;
        let mut cards = Cards::default();
        // A hover's poses in an earlier frame do not count.
        assert_eq!(cards.signal(&pose(410, 75746)), None);
        assert_eq!(cards.signal(&pose(611, 75746)), None);
        assert_eq!(cards.signal(&pose(611, 75747)), None);
        assert_eq!(cards.signal(&up(611)), Some(Signal::CardsUp(2, STAMP)));
        assert_eq!(cards.signal(&up(700)), Some(Signal::CardsUp(0, STAMP)));
        assert_eq!(cards.signal("[2026.10.29-12.30.36:008][  1]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_ActionCell"), Some(Signal::Decision));
    }

    #[test]
    fn the_last_day_written() {
        let log = "[2026.10.28-17.06.27:199][1]TLLua: Display: [Game] [S16LastDay] check cur=16 max=20 played=false\n\
                   [2026.10.28-17.06.28:478][2]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_RouteCell\n\
                   [2026.10.28-17.06.32:995][3]TLLua: Display: [Game] [S16LastDay] check cur=17 max=20 played=false\n\
                   [2026.10.28-17.06.33:713][4]TLGame: Display: [Game] UIButton@ normal-click Btn_S16Board_RouteCell\n";
        assert_eq!(last_day(log), Some(Signal::Day { cur: 17, max: 20 }));
        assert_eq!(last_day("[2026.10.28-17.06.33:713][4]TLGame: normal-click Btn_S16Board_RouteCell\n"), None);
    }

    #[test]
    fn library_folders() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"F:\\\\SteamLibrary\"\n\t\t\"label\"\t\t\"\"\n\t}\n}";
        assert_eq!(library_paths(vdf), vec![PathBuf::from(r"C:\Program Files (x86)\Steam"), PathBuf::from(r"F:\SteamLibrary")]);
    }

    /// The log next to the game's exe, as installed under a Steam library (2026-11-05) or any other folder.
    #[test]
    fn the_log_beside_the_exe() {
        let exe = Path::new(r"F:\SteamLibrary\steamapps\common\Torchlight Infinite\UE_game\Binaries\Win64\torchlight_infinite.exe");
        assert_eq!(
            from_exe(exe),
            Some(PathBuf::from(r"F:\SteamLibrary\steamapps\common\Torchlight Infinite\UE_game\TorchLight\Saved\Logs\UE_game.log"))
        );
        assert_eq!(from_exe(Path::new(r"D:\Games\TLI\UE_game\Binaries\Win64\torchlight_infinite.exe")), Some(PathBuf::from(r"D:\Games\TLI\UE_game\TorchLight\Saved\Logs\UE_game.log")));
        assert_eq!(from_exe(Path::new("game.exe")), None);
    }

    /// A file chosen in the settings is the only one used; the automatic search finds the log next to the exe
    /// before any other candidate.
    #[test]
    fn the_log_is_located_in_order() {
        let root = std::env::temp_dir().join(format!("ascendant-gamelog-locate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let exe = root.join(r"UE_game\Binaries\Win64\torchlight_infinite.exe");
        let beside = root.join(r"UE_game\TorchLight\Saved\Logs\UE_game.log");
        let manual = root.join("chosen.log");
        assert_eq!(locate(Some(&manual), Some(&exe)), Err(LogStatus::ManualMissing(manual.clone())));
        // No log anywhere: Missing while the game runs; Searching without it (unless Steam or %LOCALAPPDATA% has one
        // on this machine, which the test does not control).
        if candidates(None).iter().all(|(p, _)| !p.is_file()) {
            assert_eq!(locate(None, Some(&exe)), Err(LogStatus::Missing));
            assert_eq!(locate(None, None), Err(LogStatus::Searching));
        }
        std::fs::create_dir_all(beside.parent().unwrap()).unwrap();
        std::fs::write(&beside, "").unwrap();
        assert_eq!(locate(None, Some(&exe)), Ok((beside.clone(), LogSource::GameFolder)));
        assert_eq!(locate(Some(&manual), Some(&exe)), Err(LogStatus::ManualMissing(manual.clone())), "no fallback");
        std::fs::write(&manual, "").unwrap();
        assert_eq!(locate(Some(&manual), Some(&exe)), Ok((manual.clone(), LogSource::Manual)));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The log light: yellow without the game, red when the log was not written since the game started (after the
    /// grace) or has been silent for [`STALE_AFTER`].
    #[test]
    fn a_log_that_is_not_written_is_stale() {
        use std::time::SystemTime;
        let (path, source) = (Path::new("UE_game.log"), LogSource::Steam);
        let now = SystemTime::now();
        let ago = |s: u64| now - Duration::from_secs(s);
        let status = |started: Option<SystemTime>, since: u64, modified: SystemTime| follow_status(path, &source, started, Duration::from_secs(since), Some(modified), now);
        let following = LogStatus::Following { path: path.to_path_buf(), source: source.clone() };
        let stale = LogStatus::Stale { path: path.to_path_buf(), source: source.clone() };
        assert_eq!(status(None, 999, ago(9999)), LogStatus::GameClosed { path: path.to_path_buf(), source: source.clone() });
        assert_eq!(status(Some(ago(600)), 1, ago(1)), following);
        // The login screen was silent for 117 s: still fine.
        assert_eq!(status(Some(ago(600)), 117, ago(117)), following);
        assert_eq!(status(Some(ago(600)), 180, ago(180)), stale);
        // Last written before this game started: an old log, stale after the grace.
        assert_eq!(status(Some(ago(30)), 30, ago(3600)), following);
        assert_eq!(status(Some(ago(61)), 61, ago(3600)), stale);
    }

    /// A followed log that disappears for good is given up on, so the watcher looks for it again.
    #[test]
    fn following_stops_when_the_log_is_gone() {
        let path = std::env::temp_dir().join(format!("ascendant-gamelog-gone-{}.log", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let stopped = AtomicBool::new(false);
        let remover = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                let _ = std::fs::remove_file(&path);
            })
        };
        let started = std::time::Instant::now();
        follow(&path, &stopped, |_, _| {}, |_| {});
        remover.join().unwrap();
        assert!(started.elapsed() >= LOG_GONE && started.elapsed() < LOG_GONE + Duration::from_secs(5), "{:?}", started.elapsed());
    }
}
