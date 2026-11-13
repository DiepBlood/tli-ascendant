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
    /// How the riot battle after a run went (fought or abandoned, the special boss, the chests opened), see
    /// [`RiotOutcome`]: sent once the game leaves the battle's level, or when the riot is abandoned. For the run log
    /// and the run review only: nothing is read or advised on it.
    RiotEnd(RiotEnd),
    /// A Netherrealm map's detail page ([`MapPage`]) shown, covered by another page or closed ([`PageState`]), see
    /// [`map_page`]. For the map affixes only (the affix frames): Tartarus and Vorax never get it.
    MapPage { page: MapPage, state: PageState },
    /// An affix button clicked on a map page: Add (`revert` false) or Revert. The game names them only `AddBtn` and
    /// `RevertBtn`, so they mean something only while a map page is open.
    AffixButton { revert: bool },
    /// The panel affixes of the level just entered, from the game log's AffixInfos block, see [`AffixBlock`].
    MapAffixes(MapAffixes),
}

/// Which Netherrealm map detail page: a normal map (its affix panel lists the affixes added with Netherrealm
/// Resonance), a boss map or a gold map (special reward maps, which the user says carry no affixes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapPage {
    Normal,
    Boss,
    Gold,
}

/// A map page's `PageRunState`: `Run` (shown), `Hide` (another page opened over it; it may show again) or `Destory`
/// (closed: the game enters the map 0.5 to 1.7 s later, or the visit was cancelled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageState {
    Shown,
    Hidden,
    Closed,
}

impl PageState {
    /// Its name in the affix samples' rows.
    pub fn name(self) -> &'static str {
        match self {
            PageState::Shown => "shown",
            PageState::Hidden => "hidden",
            PageState::Closed => "closed",
        }
    }
}

impl MapPage {
    /// Its name in the affix samples' rows.
    pub fn name(self) -> &'static str {
        match self {
            MapPage::Normal => "normal",
            MapPage::Boss => "boss",
            MapPage::Gold => "gold",
        }
    }
}

/// A map page opening or closing: `TipMsgShowMgr@DispatchPageRunChange PageName = MysteryMapDetail , PageRunState =
/// Run` (or `Hide`, `Destory`); `MysteryBossDetail` and `MysteryGoldMapDetail` are the boss and gold maps. The page
/// stack lines that also name the page (`PageStack@ ... PageName = MysteryMapDetail, PageType = ...`) are not taken.
pub fn map_page(line: &str) -> Option<Signal> {
    let rest = line.split_once("DispatchPageRunChange PageName = ")?.1;
    let (name, state) = rest.split_once(" , PageRunState = ")?;
    let page = match name {
        "MysteryMapDetail" => MapPage::Normal,
        "MysteryBossDetail" => MapPage::Boss,
        "MysteryGoldMapDetail" => MapPage::Gold,
        _ => return None,
    };
    let state = match state.trim() {
        "Run" => PageState::Shown,
        "Hide" => PageState::Hidden,
        "Destory" => PageState::Closed,
        _ => return None,
    };
    Some(Signal::MapPage { page, state })
}

/// An affix button click: `UIButton@ normal-click AddBtn` or `UIButton@ normal-click RevertBtn` (the whole name, so
/// `AddBtnX` is not taken).
fn affix_button(line: &str) -> Option<Signal> {
    match line.trim_end().rsplit_once("UIButton@ normal-click ")?.1 {
        "AddBtn" => Some(Signal::AffixButton { revert: false }),
        "RevertBtn" => Some(Signal::AffixButton { revert: true }),
        _ => None,
    }
}

/// Panel affix ids kept per level ([`MapAffixes`]): a panel shows six at most.
pub const MAP_AFFIX_MAX: usize = 16;

/// The levels of the hideout: entering one leaves the map (the logs of 2026-11-08 and 11-10 go map, 110, map, 110:
/// 163 map entries, one into another level without the hideout between, 235501, whose block names no panel affix).
/// Its block names no panel affix, or the last map's or another map's six (the map device's), never the hideout's own.
pub const HIDEOUT_LEVELS: &[u32] = &[110];

/// Whether an affix id is one the map page's affix panel shows (Netherrealm Resonance): 51xxxxx to 56xxxxx and
/// 35003xx, the ranges of every id the lab matched to panel text (91 ids, 2026-11-08). The AffixInfos block lists
/// about 50 to 110 other ids too (compass, vigil, season and the map's own modifiers).
pub fn is_panel_affix(id: u32) -> bool {
    (5_100_000..=5_699_999).contains(&id) || (3_500_300..=3_500_399).contains(&id)
}

/// The affixes of a level entered ([`Signal::MapAffixes`]): the panel's ids ([`is_panel_affix`], in the block's
/// order, at most [`MAP_AFFIX_MAX`]) and how many other ids the block listed. No other log text is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapAffixes {
    /// The level entered just before the block (`UGameMgr::EnterLevel(5308)`), when one was seen.
    pub level: Option<u32>,
    ids: [u32; MAP_AFFIX_MAX],
    n: u8,
    /// Ids of the block that are not panel affixes (and panel ids past [`MAP_AFFIX_MAX`]).
    pub others: u16,
}

impl MapAffixes {
    /// The panel affix ids.
    pub fn ids(&self) -> &[u32] {
        &self.ids[..self.n as usize]
    }

    /// The affixes of `level` with these ids (tests of the modules that take the signal).
    #[cfg(test)]
    pub fn of(level: Option<u32>, ids: &[u32]) -> Self {
        let mut a = MapAffixes { level, ..MapAffixes::default() };
        ids.iter().for_each(|&id| a.push(id));
        a
    }

    fn push(&mut self, id: u32) {
        if is_panel_affix(id) && (self.n as usize) < MAP_AFFIX_MAX {
            self.ids[self.n as usize] = id;
            self.n += 1;
        } else {
            self.others = self.others.saturating_add(1);
        }
    }
}

/// Follows the game log for the AffixInfos block the game writes on entering a level (after `UGameMgr::EnterLevel`):
/// a `[Game] AffixInfos` line, then untimed lines listing up to 30 affixes (`+1+Id [216000]` and the entry's
/// `+FId`, `+Tier`, `+DangerNumbers`, `+Description` lines, then `+31+...` when there are more), then every affix
/// again on its own: untimed `+Id [216000]` lines with the same fields, each closed by a timed `[Game] 216000` line. The block ends at the first timed line that is neither empty after `[Game]` nor a number.
/// The ids come from the closing number lines when there are any (the full list), else from the list's `+Id` lines.
/// A block with no ids gives nothing, except after a level entered: the hideout's (`110`) often has none, and the
/// affix page drops the map it shows on entering it (`affix_live`).
///
/// Each panel affix's text is kept too (its `+Description [...]` line, in the client's language, values
/// filled in) for the affix list to learn from ([`take_affix_texts`]); the signal itself stays ids only. The fields of
/// an affix come in no fixed order; the logs of 2026-11-08 hold three, one per game session: `+Id +FId +Tier
/// +DangerNumbers +Description`, `+FId +Id +Description +Tier +DangerNumbers` and `+Description +Id +DangerNumbers +FId
/// +Tier` (reading the text as the last `+Id`'s gave every text to the affix before it in the last one). So a text is taken
/// only from a reprinted entry, whose bounds the timed lines give whatever the order: the entry from its empty timed
/// line to the timed line naming its id holds that one `+Id` and one readable text, or nothing is learned from it.
#[derive(Default)]
pub struct AffixBlock {
    level: Option<u32>,
    open: bool,
    listed: Vec<u32>,
    repeated: Vec<u32>,
    /// The reprinted entry being read (since its empty timed line), and the texts found so far (first text per id).
    entry: Option<EntryLines>,
    texts: Vec<(u32, String)>,
}

/// The untimed lines of one reprinted affix, between its empty timed line and the timed line naming its id: what its
/// `+Id` lines name (`None` for one that does not read) and its readable texts.
#[derive(Default)]
struct EntryLines {
    ids: Vec<Option<u32>>,
    texts: Vec<String>,
}

/// Panel affix texts the game log gave on entering levels, waiting to be taken ([`take_affix_texts`]).
static AFFIX_TEXTS: std::sync::Mutex<Vec<(u32, String)>> = std::sync::Mutex::new(Vec::new());

/// The panel affix ids and texts ([`AffixBlock`]) found since the last call: each id with its `+Description` text as the
/// log wrote it (tags and values in, see `affix::learned::template`).
pub fn take_affix_texts() -> Vec<(u32, String)> {
    std::mem::take(&mut *AFFIX_TEXTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

impl AffixBlock {
    /// The [`Signal::MapAffixes`] of the block this line ends, if any.
    pub fn line(&mut self, line: &str) -> Option<Signal> {
        let mut done = None;
        if self.open {
            if !line.starts_with('[') {
                if line.contains("+Id [") {
                    let id = number_after(line, "+Id [");
                    if let Some(id) = id {
                        self.listed.push(id);
                    }
                    if let Some(entry) = &mut self.entry {
                        entry.ids.push(id);
                    }
                } else if let (Some(entry), Some((_, rest))) = (&mut self.entry, line.split_once("+Description [")) {
                    // the text runs to the line's closing ']' (it may hold brackets of its own); a line without one
                    // (cut short) and an untranslated key are not texts
                    if let Some(text) = rest.trim_end().strip_suffix(']')
                        && readable(text)
                    {
                        entry.texts.push(text.to_string());
                    }
                }
                return None;
            }
            match line.split_once("[Game]").map(|(_, rest)| rest.trim()) {
                // an empty timed line starts a reprinted entry (the first one also comes before the list)
                Some("") => {
                    self.entry = Some(EntryLines::default());
                    return None;
                }
                Some(rest) if rest.bytes().all(|b| b.is_ascii_digit()) => {
                    if let Ok(id) = rest.parse::<u32>() {
                        self.repeated.push(id);
                        // The entry's text is this id's when the entry names this id alone and holds one text; any
                        // other entry (the list before the first entry, an id that does not read or another id, two
                        // texts) teaches nothing: a text is never guessed onto an id.
                        if let Some(entry) = self.entry.take()
                            && entry.ids == [Some(id)]
                            && let [text] = entry.texts.as_slice()
                            && !self.texts.iter().any(|(known, _)| *known == id)
                        {
                            self.texts.push((id, text.clone()));
                        }
                    }
                    self.entry = None;
                    return None;
                }
                _ => done = self.close(),
            }
        }
        if let Some(level) = level_entered(line) {
            self.level = Some(level);
        } else if line.trim_end().ends_with("[Game] AffixInfos") {
            self.open = true;
        }
        done
    }

    /// The log ended: a block still open is closed, so the texts it holds count (as a later line would close it).
    pub fn finish(&mut self) {
        if self.open {
            let _ = self.close();
        }
    }

    fn close(&mut self) -> Option<Signal> {
        self.open = false;
        let ids = if self.repeated.is_empty() { std::mem::take(&mut self.listed) } else { std::mem::take(&mut self.repeated) };
        self.listed.clear();
        self.repeated.clear();
        self.entry = None;
        let texts = std::mem::take(&mut self.texts);
        let panel: Vec<(u32, String)> = texts.into_iter().filter(|(id, _)| is_panel_affix(*id) && ids.contains(id)).collect();
        if !panel.is_empty() {
            AFFIX_TEXTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(panel);
        }
        if ids.is_empty() && self.level.is_none() {
            return None;
        }
        let mut affixes = MapAffixes { level: self.level, ..MapAffixes::default() };
        ids.into_iter().for_each(|id| affixes.push(id));
        Some(Signal::MapAffixes(affixes))
    }
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

/// The kind of chest a riot battle's drop carrier is (`drop carrier triggered cfgId 1090NN`, one line per chest, all of
/// them at the battle's end): two sets of six, 109001-109006 and 109021-109026, by the rarity of the inmate it is for.
/// Checked on the 18 riots of 2026-11-08 and 11-09 against the run's last board: 109006 and 109026 are the colour
/// chests (at least the vaulted count in every run, as the player sees it), 109005 and 109025 the red ones (the
/// board's Ultimates, correlation 0.98), 109001-109004 and 109021-109024 the medium ones (every lower rarity, 0.94).
/// Each kind is at least its inmates in every run (one run's last read came before a last bite) and about 15 to 21 %
/// more in all: a Pact Spirit may open a warden's hoard once more (up to 16 %, the player says). The second set is
/// not that re-opening: the first set alone falls 8 to 16 % short of the inmates.
/// Not counted: the board task's chest (109041, 109042, 109051 or 109052, alone as the battle starts, one in every
/// run and two in 4 of 18, the player says a red or a colour one; which id is which is not known), and the special
/// boss's chest ([`RIOT_BOSS_CHEST`], 97 drops on 2026-11-07, which the player also calls a colour chest).
/// The chest sounds (`UI_S16_Red_Chest` and the like) agree but come fewer (9 for 19 chests in one run), so they are
/// not counted.
fn chest_kind(cfg: u32) -> Option<Chest> {
    match cfg {
        109_006 | 109_026 => Some(Chest::Color),
        109_005 | 109_025 => Some(Chest::Red),
        109_001..=109_004 | 109_021..=109_024 => Some(Chest::Medium),
        _ => None,
    }
}

/// The three chest kinds of a riot battle, see [`chest_kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chest {
    Red,
    Color,
    Medium,
}

/// How one run's riot battle went, see [`Signal::RiotEnd`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RiotEnd {
    /// The player started the riot battle (`CheckOut_StartChallenge`); false: abandoned on the run's checkout.
    pub fought: bool,
    /// The special boss ([`RIOT_BOSS_NPC`]) came, or its chest opened.
    pub boss: bool,
    /// Its death shout played or its chest opened.
    pub boss_killed: bool,
    /// Chests opened by kind, counted by their drop carriers (see [`chest_kind`]).
    pub red: u32,
    pub color: u32,
    pub medium: u32,
}

/// Where a run's riot stands for [`RiotOutcome`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RiotStage {
    /// No run under way that the log showed, or its riot already told.
    #[default]
    Idle,
    /// A run is under way: its checkout comes next.
    Run,
    /// The run's checkout opened: the player starts the riot battle or abandons it.
    Offered,
    /// The riot battle was started; `entered` once the game is in its level.
    Fighting { entered: bool },
}

/// Follows the game log for how each run's riot went ([`Signal::RiotEnd`]). The checkout page also opens again when
/// the player comes back to the prison after the battle, and they close it with its Abandon button (every one of the
/// 18 runs of 2026-11-08 and 11-09): only the checkout right after a run's board counts, so a run must have been seen
/// under way first (its Start, or a day's decision cards if the log was followed from mid-run; the board page is no
/// sign, it opens behind that second checkout too). Followed from mid-checkout or mid-battle, that run's riot is not
/// told.
#[derive(Default)]
pub struct RiotOutcome {
    stage: RiotStage,
    end: RiotEnd,
}

impl RiotOutcome {
    /// The run's [`Signal::RiotEnd`], on the line the game left the riot battle's level or the riot was abandoned.
    pub fn line(&mut self, line: &str) -> Option<Signal> {
        if line.contains("normal-click Btn_S16GamePlay_Start") || line.contains("AudioEnum  == UI_S16_Card_Up") {
            if !matches!(self.stage, RiotStage::Fighting { .. }) {
                (self.stage, self.end) = (RiotStage::Run, RiotEnd::default());
            }
            return None;
        }
        match self.stage {
            RiotStage::Idle => None,
            RiotStage::Run => {
                if line.contains("PageName = S16GamePlayCheckOut , PageRunState = Run") {
                    self.stage = RiotStage::Offered;
                }
                None
            }
            RiotStage::Offered => {
                if line.contains("normal-click Btn_S16GamePlay_CheckOut_StartChallenge") {
                    self.stage = RiotStage::Fighting { entered: false };
                    self.end = RiotEnd { fought: true, ..RiotEnd::default() };
                } else if line.contains("normal-click Btn_S16GamePlay_CheckOut_AbandonChallenge") {
                    self.stage = RiotStage::Idle;
                    return Some(Signal::RiotEnd(RiotEnd::default()));
                }
                None
            }
            RiotStage::Fighting { entered } => {
                if let Some(level) = level_entered(line) {
                    if level == RIOT_LEVEL {
                        self.stage = RiotStage::Fighting { entered: true };
                    } else if entered {
                        self.stage = RiotStage::Idle;
                        return Some(Signal::RiotEnd(std::mem::take(&mut self.end)));
                    }
                    return None;
                }
                if !entered {
                    return None;
                }
                let e = &mut self.end;
                match carrier(line) {
                    Some(RIOT_BOSS_CHEST) => (e.boss, e.boss_killed) = (true, true),
                    Some(cfg) => match chest_kind(cfg) {
                        Some(Chest::Red) => e.red += 1,
                        Some(Chest::Color) => e.color += 1,
                        Some(Chest::Medium) => e.medium += 1,
                        None => {}
                    },
                    None if monster(line) == Some(RIOT_BOSS_NPC) => e.boss = true,
                    None if line.contains(RIOT_BOSS_DIES) => (e.boss, e.boss_killed) = (true, true),
                    None => {}
                }
                None
            }
        }
    }
}

/// The number right after `key` in `line`, up to the first character that is not a digit.
fn number_after(line: &str, key: &str) -> Option<u32> {
    let rest = line.split_once(key)?.1;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Whether an affix text of the log is worth keeping for the affix list (`affix::learned::template` makes something
/// of it).
fn readable(text: &str) -> bool {
    crate::affix::learned::template(text).is_some()
}

/// The piece of a log [`look_back_affixes`] reads at a time.
const LOOK_BACK_PIECE: usize = 8 << 20;

/// The affixes the game's logs named before Ascendant started, for the affix list to
/// learn from as from the levels entered later ([`take_affix_texts`]): every `UE_game*.log` beside `log` (the log the
/// game writes now and its backups of the earlier ones; it keeps two), oldest first, read a piece at a time
/// ([`LOOK_BACK_PIECE`]) into an [`AffixBlock`]. Only the panel affixes' ids and texts are kept; no other part of the
/// logs (player names and ids among them) is held past its piece or written anywhere. A piece outside any block that
/// does not name one is not turned into text. Stops between pieces when `stopped`. Returns how many logs were read to
/// the end.
pub fn look_back_affixes(log: &Path, stopped: &AtomicBool) -> usize {
    let Some(read) = log.parent().and_then(|dir| std::fs::read_dir(dir).ok()) else { return 0 };
    let mut logs: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("UE_game") && n.ends_with(".log")))
        .collect();
    // UE_game-backup-<date and time>.log sorts before UE_game.log, the backups by date
    logs.sort();
    logs.iter().filter(|p| !stopped.load(Ordering::Relaxed) && affixes_in(p, stopped)).count()
}

/// Reads one log for [`look_back_affixes`]; true when read to its end.
fn affixes_in(path: &Path, stopped: &AtomicBool) -> bool {
    const NEEDLE: &[u8] = b"AffixInfos";
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut block = AffixBlock::default();
    let mut piece = vec![0u8; LOOK_BACK_PIECE];
    let mut carried: Vec<u8> = Vec::new();
    loop {
        if stopped.load(Ordering::Relaxed) {
            return false;
        }
        let n = match file.read(&mut piece) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return false,
        };
        carried.extend_from_slice(&piece[..n]);
        // whole lines only: the last one may go on in the next piece
        let complete = carried.iter().rposition(|&b| b == b'\n').map_or(0, |k| k + 1);
        let lines = &carried[..complete];
        if block.open || lines.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
            for line in String::from_utf8_lossy(lines).lines() {
                let _ = block.line(line);
            }
        }
        carried.drain(..complete);
    }
    for line in String::from_utf8_lossy(&carried).lines() {
        let _ = block.line(line);
    }
    block.finish();
    true
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
    // The map pages close as well as open (Hide, Destory), so they come before the filter below.
    if let Some(signal) = map_page(line).or_else(|| affix_button(line)) {
        return Some(signal);
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
    let mut outcome = RiotOutcome::default();
    let mut affixes = AffixBlock::default();
    // The game's frame rate on the board (`super::fps`).
    let mut frames = super::fps::FrameClock::default();
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
            outcome = RiotOutcome::default();
            affixes = AffixBlock::default();
            frames = super::fps::FrameClock::default();
        }
        if len == at {
            continue;
        }
        last_new = std::time::Instant::now();
        if len - at > MAX_READ {
            (at, partial) = (len - MAX_READ, String::new());
            // An AffixInfos block cut by the jump: the lines after it are not its, and a text would be kept under the
            // id read before the jump, for good (the learned list keeps the first text; review 2026-11-08).
            affixes = AffixBlock::default();
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
            if let Some(end) = outcome.line(line) {
                on(end, game_millis(line));
            }
            if let Some(level) = affixes.line(line) {
                on(level, game_millis(line));
            }
            if let Some(signal) = cards.signal(line) {
                on(signal, game_millis(line));
            }
            if let Some(second) = frames.line(line) {
                super::fps::note_second(second);
            }
        }
        partial.drain(..complete);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The riot boss signals of a game log's lines, as [`follow`] sends them.
    /// A riot battle's lines kept in tests/fixtures; None (and a note) when it is missing, as in the public repository,
    /// which has no tests folder.
    fn riot_fixture(name: &str) -> Option<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        if !path.is_file() {
            println!("skipped: {} is missing", path.display());
            return None;
        }
        Some(std::fs::read_to_string(&path).unwrap())
    }

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
        let Some(text) = riot_fixture("riot-boss-C.log") else { return };
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
        let Some(text) = riot_fixture("riot-boss-A.log") else { return };
        assert!(text.contains("Monster Created: npc id ") && text.contains("UDropInstanceMgr@ AddInstance!"));
        assert_eq!(riot_of(&text), []);
    }

    /// The riot outcome signals of a game log's lines, as [`follow`] sends them.
    fn outcome_of(text: &str) -> Vec<Signal> {
        let mut outcome = RiotOutcome::default();
        text.lines().filter_map(|line| outcome.line(line)).collect()
    }

    fn game_line(at: &str, text: &str) -> String {
        format!("[2026.11.09-20.{at}][1]TLLua: Display: [Game] {text}\n")
    }

    /// A run's lines up to its checkout, the riot battle started.
    fn run_to_riot() -> String {
        [
            game_line("14:26:243", "UIButton@ normal-click Btn_S16GamePlay_Start"),
            game_line("16:20:363", "TipMsgShowMgr@DispatchPageRunChange PageName = S16GamePlayCheckOut , PageRunState = Run"),
            game_line("16:21:673", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_StartChallenge"),
            game_line("16:21:866", "LevelMgr:_EnterLevelSuccess() 235501"),
            game_line("16:22:561", "UGameMgr::EnterLevel(235501) mode=2 reload=0."),
        ]
        .concat()
    }

    /// A riot battle as its lines go (the chests by their drop carriers; the chest sounds, fewer, and the battle's
    /// other carriers not counted), the checkout opened again after it and closed with Abandon (not an abandoned
    /// riot), then the next run abandoned.
    #[test]
    fn the_riot_outcome_of_a_run() {
        let carrier = |at: &str, cfg: u32| game_line(at, &format!("drop carrier triggered cfgId {cfg} ety id 5041"));
        let mut text = run_to_riot();
        text.push_str(&carrier("16:22:900", 109_041));
        text.push_str(&game_line("16:25:887", "AudioMgr@PlayAudio  AudioEnum  == UI_S16_Medium_Chest"));
        text.push_str(&game_line("16:28:094", "AudioMgr@PlayAudio  AudioEnum  == UI_S16_Color_Chest"));
        text.push_str(&game_line("16:44:132", "AudioMgr@PlayAudio  AudioEnum  == UI_S16_Red_Chest"));
        for (k, cfg) in [109_002, 109_022, 109_023, 109_003, 109_026, 109_006, 109_006, 109_005, 109_025, 109_004].into_iter().enumerate() {
            text.push_str(&carrier(&format!("16:46:{:03}", 100 + k * 30), cfg));
        }
        let town = game_line("16:54:145", "LevelMgr:_EnterLevelSuccess() 110");
        text.push_str(&town);
        text.push_str(&game_line("16:55:509", "UGameMgr::EnterLevel(110) mode=1 reload=0."));
        text.push_str(&game_line("16:59:367", "TipMsgShowMgr@DispatchPageRunChange PageName = S16GamePlayCheckOut , PageRunState = Run"));
        text.push_str(&game_line("17:00:400", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_AbandonChallenge"));
        let fought = RiotEnd { fought: true, boss: false, boss_killed: false, red: 2, color: 3, medium: 5 };
        assert_eq!(outcome_of(&text), [Signal::RiotEnd(fought)]);
        // The kinds: every carrier of the two sets, nothing else.
        let kinds: Vec<Option<Chest>> = [109_001, 109_004, 109_005, 109_006, 109_021, 109_024, 109_025, 109_026, 109_000, 109_007, 109_034, 109_041, 109_051].map(chest_kind).to_vec();
        let (m, r, c) = (Some(Chest::Medium), Some(Chest::Red), Some(Chest::Color));
        assert_eq!(kinds, [m, m, r, c, m, m, r, c, None, None, None, None, None]);
        // Sent on the line the game left the battle.
        let mut outcome = RiotOutcome::default();
        let sent: Vec<&str> = text.lines().filter(|line| outcome.line(line).is_some()).collect();
        assert_eq!(sent, [town.trim_end()]);
        // The next run abandoned on its checkout.
        text.push_str(&game_line("17:02:030", "UIButton@ normal-click Btn_S16GamePlay_Start"));
        text.push_str(&game_line("19:07:100", "TipMsgShowMgr@DispatchPageRunChange PageName = S16GamePlayCheckOut , PageRunState = Run"));
        text.push_str(&game_line("19:08:000", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_AbandonChallenge"));
        assert_eq!(outcome_of(&text), [Signal::RiotEnd(fought), Signal::RiotEnd(RiotEnd::default())]);
    }

    /// The special boss: created, killed, its chest; created and not killed. The boss's lines on another level, or
    /// a log followed from the checkout on (no run seen under way), tell nothing.
    #[test]
    fn the_riot_outcome_boss_and_unseen_runs() {
        let town = game_line("17:00:000", "LevelMgr:_EnterLevelSuccess() 110");
        let created = game_line("16:57:000", "Monster Created: npc id 5026001 rarity 4");
        let dies = game_line("16:58:000", "LogDataTable: Warning: UDataTable::FindRow : 'Play_Boss_Elite_Shout_YuZu_Die' requested row");
        let chest = game_line("16:59:000", "drop carrier triggered cfgId 109034 ety id 5041");
        let killed = RiotEnd { fought: true, boss: true, boss_killed: true, ..RiotEnd::default() };
        assert_eq!(outcome_of(&format!("{}{created}{dies}{town}", run_to_riot())), [Signal::RiotEnd(killed)]);
        assert_eq!(outcome_of(&format!("{}{chest}{town}", run_to_riot())), [Signal::RiotEnd(killed)]);
        let alive = RiotEnd { fought: true, boss: true, ..RiotEnd::default() };
        assert_eq!(outcome_of(&format!("{}{created}{town}", run_to_riot())), [Signal::RiotEnd(alive)]);
        // Before the battle's level: not counted.
        let start = game_line("14:26:243", "UIButton@ normal-click Btn_S16GamePlay_Start");
        let offered = game_line("16:20:363", "TipMsgShowMgr@DispatchPageRunChange PageName = S16GamePlayCheckOut , PageRunState = Run");
        let go = game_line("16:21:673", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_StartChallenge");
        let enter = game_line("16:21:866", "LevelMgr:_EnterLevelSuccess() 235501");
        let fought = RiotEnd { fought: true, ..RiotEnd::default() };
        assert_eq!(outcome_of(&format!("{start}{offered}{go}{created}{enter}{town}")), [Signal::RiotEnd(fought)]);
        // A checkout with no run seen under way (the log followed from there): nothing, abandoned or fought.
        let checkout = game_line("16:20:363", "TipMsgShowMgr@DispatchPageRunChange PageName = S16GamePlayCheckOut , PageRunState = Run");
        let abandon = game_line("16:21:000", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_AbandonChallenge");
        assert_eq!(outcome_of(&format!("{checkout}{abandon}")), []);
        let fight = game_line("16:21:673", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_StartChallenge");
        let riot = game_line("16:21:866", "LevelMgr:_EnterLevelSuccess() 235501");
        assert_eq!(outcome_of(&format!("{checkout}{fight}{riot}{created}{town}")), []);
        // A run seen by a day's decision cards only (followed from mid-run) counts; the board page is no sign.
        let cards = game_line("15:00:000", "AudioMgr@PlayAudio  AudioEnum  == UI_S16_Card_Up");
        assert_eq!(outcome_of(&format!("{cards}{checkout}{abandon}")), [Signal::RiotEnd(RiotEnd::default())]);
        let board = game_line("15:00:000", "TipMsgShowMgr@DispatchPageRunChange PageName = S16BoardMain , PageRunState = Run");
        assert_eq!(outcome_of(&format!("{board}{checkout}{abandon}")), []);
    }

    /// The 2026-11-07 14:39 riot battle (tests/fixtures/riot-boss-C.log) after a run's checkout: the boss came and was
    /// killed. The 14:30 battle (riot-boss-A.log) had none. The chests as the fixtures' carrier lines count them by
    /// hand: C 109005 x9, 109025 x5, 109006 x10, 109026 x2, 109002, 109003; A 109005 x6, 109025 x6, 109006 x8,
    /// 109026 x2, 109001 x2, 109021 (their other carriers, the boss's 109034 among them, are no chest of the three).
    #[test]
    fn the_riot_outcome_of_real_battles() {
        let (Some(with), Some(without)) = (riot_fixture("riot-boss-C.log"), riot_fixture("riot-boss-A.log")) else { return };
        let checkout = [
            game_line("14:26:243", "UIButton@ normal-click Btn_S16GamePlay_Start"),
            game_line("16:20:363", "TipMsgShowMgr@DispatchPageRunChange PageName = S16GamePlayCheckOut , PageRunState = Run"),
            game_line("16:21:673", "UIButton@ normal-click Btn_S16GamePlay_CheckOut_StartChallenge"),
        ]
        .concat();
        let killed = RiotEnd { fought: true, boss: true, boss_killed: true, red: 14, color: 12, medium: 2 };
        assert_eq!(outcome_of(&format!("{checkout}{with}")), [Signal::RiotEnd(killed)]);
        let none = RiotEnd { fought: true, boss: false, boss_killed: false, red: 12, color: 10, medium: 3 };
        assert_eq!(outcome_of(&format!("{checkout}{without}")), [Signal::RiotEnd(none)]);
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

    /// The map pages show, hide and close (normal, boss, gold); the page stack lines naming the same page and other
    /// pages give nothing.
    #[test]
    fn map_pages_open_and_close() {
        let page = |name: &str, state: &str| format!("[2026.11.08-06.26.10:000][600]TLLua: Display: [Game] TipMsgShowMgr@DispatchPageRunChange PageName = {name} , PageRunState = {state} ");
        assert_eq!(signal(&page("MysteryMapDetail", "Run")), Some(Signal::MapPage { page: MapPage::Normal, state: PageState::Shown }));
        assert_eq!(signal(&page("MysteryMapDetail", "Destory")), Some(Signal::MapPage { page: MapPage::Normal, state: PageState::Closed }));
        assert_eq!(signal(&page("MysteryBossDetail", "Hide")), Some(Signal::MapPage { page: MapPage::Boss, state: PageState::Hidden }));
        assert_eq!(signal(&page("MysteryGoldMapDetail", "Run")), Some(Signal::MapPage { page: MapPage::Gold, state: PageState::Shown }));
        assert_eq!(signal(&page("Mystery", "Run")), None);
        let stack = "[2026.11.08-06.26.10:000][600]TLLua: Display: [Game] PageStack@   [3] = PageName = MysteryMapDetail, PageType = FuncPage, PageAction = HideLastFuncPage";
        assert_eq!(signal(stack), None);
    }

    /// Add and Revert by their whole button name only.
    #[test]
    fn affix_buttons() {
        let click = |name: &str| format!("[2026.11.08-06.28.50:739][937]TLGame: Display: [Game] UIButton@ normal-click {name}");
        assert_eq!(signal(&click("AddBtn")), Some(Signal::AffixButton { revert: false }));
        assert_eq!(signal(&click("RevertBtn ")), Some(Signal::AffixButton { revert: true }));
        assert_eq!(signal(&click("AddBtnMore")), None);
    }

    /// The AffixInfos block of a level, made up in the game's format: the level line before it, a list cut after
    /// two entries ("+3+..."), every entry again closed by its number line, and the block's end at the next timed
    /// line. The ids come from the number lines: panel ids kept in order, the rest counted.
    #[test]
    fn affix_block_of_a_level() {
        let t = "[2026.11.08-06.26.17:925][636]TLLua: Display: [Game] ";
        let lines = [
            "[2026.11.08-06.26.17:872][636]TLGame: Display: [Game] UGameMgr::EnterLevel(5308) mode=2 reload=0.".to_string(),
            format!("{t}AffixInfos"),
            t.to_string(),
            "+BaseAttrInfoList".into(),
            "+BAffixInfoList+1+Id [216000]".into(),
            "|              | +FId [1141]".into(),
            "|              | +Description [<p>+15</p>% additional Compass]".into(),
            "|              +2+Id [5111007]".into(),
            "|              | +FId [1141]".into(),
            "|              +3+...".into(),
            "+PrefixInfoList".into(),
            t.to_string(),
            "+Id [216000]".into(),
            "+FId [1141]".into(),
            "+Description [<p>+15</p>% additional Compass]".into(),
            format!("{t}216000"),
            t.to_string(),
            "+Id [5111007]".into(),
            format!("{t}5111007"),
            t.to_string(),
            "+Id [3500303]".into(),
            format!("{t}3500303"),
            format!("{t}NetGameMgr:OnEnterAreaEnd()"),
        ];
        let mut block = AffixBlock::default();
        let got: Vec<Signal> = lines.iter().filter_map(|l| block.line(l)).collect();
        let [Signal::MapAffixes(a)] = got.as_slice() else { panic!("{got:?}") };
        assert_eq!(a.level, Some(5308));
        assert_eq!(a.ids(), &[5_111_007, 3_500_303]);
        assert_eq!(a.others, 1);
    }

    /// Each panel affix's text is kept (the `+Description` after its `+Id`) for the affix list; ids outside
    /// the panel's ranges are not kept.
    /// The tests that take from the shared queue of affix texts ([`take_affix_texts`]) run one at a time.
    static AFFIX_TEXTS_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn panel_affix_texts_are_kept() {
        let _turn = AFFIX_TEXTS_TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = "[2026.11.08-06.26.17:925][636]TLLua: Display: [Game] ";
        let lines = [
            format!("{t}AffixInfos"),
            t.to_string(),
            "+Id [216000]".into(),
            "+Description [<p>+15</p>% additional Compass <e id=507>Drop Quantity</e>]".into(),
            format!("{t}216000"),
            t.to_string(),
            "+Id [5112007]".into(),
            "+Description [<p>-20</p>% additional Area Damage for you [test]]".into(),
            "+Tier [0]".into(),
            format!("{t}5112007"),
            format!("{t}NetGameMgr:OnEnterAreaEnd()"),
        ];
        let _ = take_affix_texts();
        let mut block = AffixBlock::default();
        let got: Vec<Signal> = lines.iter().filter_map(|l| block.line(l)).collect();
        assert_eq!(got.len(), 1);
        let texts = take_affix_texts();
        assert!(texts.contains(&(5_112_007, "<p>-20</p>% additional Area Damage for you [test]".to_string())), "{texts:?}");
        assert!(!texts.iter().any(|(id, _)| *id == 216_000));
    }

    /// Three real AffixInfos blocks (tests/fixtures/affix, cut from the user's logs: field names in their order, affix
    /// ids and texts only), one per field order the game wrote: `+Id` first (an English client, 2026-11-08 daytime),
    /// `+FId` first (an English client, 2026-11-08) and `+Description` first (a Traditional Chinese client after the
    /// game restarted, 2026-11-08 evening).
    /// Read when a test runs, not built in: the public repository has no tests folder, and its `cargo test` must still
    /// build (review of 2026-11-09). None, and the test says skipped, when the file is not there. Kept for the test's
    /// run (leaked: a test process is short).
    fn block(name: &str) -> Option<&'static str> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/affix").join(name);
        let text = std::fs::read_to_string(path).ok()?;
        Some(Box::leak(text.into_boxed_str()))
    }

    /// The three blocks, or None (and a "skipped" line) without the fixtures.
    fn blocks() -> Option<(&'static str, &'static str, &'static str)> {
        match (block("block-id-first.txt"), block("block-fid-first.txt"), block("block-description-first.txt")) {
            (Some(a), Some(b), Some(c)) => Some((a, b, c)),
            _ => {
                println!("skipped: no tests/fixtures/affix/block-*.txt");
                None
            }
        }
    }

    /// The panel affix texts a block's lines teach.
    fn taught<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<(u32, String)> {
        let _ = take_affix_texts();
        let mut block = AffixBlock::default();
        let signals: Vec<Signal> = lines.into_iter().filter_map(|l| block.line(l)).collect();
        assert_eq!(signals.len(), 1, "one level");
        let mut texts = take_affix_texts();
        texts.sort();
        texts
    }

    /// Each real block teaches every panel affix's own text whichever order its fields come in: the texts agree with
    /// the default table's for the same id and language wherever the table has one (the table was made from logs of
    /// the `+Id` first order), and the `+Description` first block's six are the ones on its map's panel.
    #[test]
    fn affix_texts_in_the_three_real_field_orders() {
        use crate::affix::{Dict, dict::key, learned::template};
        let _turn = AFFIX_TEXTS_TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((id_first, fid_first_block, description_first)) = blocks() else { return };
        let table = Dict::builtin();
        // the least each block's texts are checked against the table: the table has only one of the `+FId` first
        // block's six in English, so that block's six are checked by their words below as well
        for (name, block, want_ids, least) in [
            ("id first", id_first, [5_121_004, 5_212_006, 5_311_021, 5_422_001, 5_521_026, 5_634_007], 2),
            ("fid first", fid_first_block, [3_500_303, 5_111_007, 5_212_010, 5_311_003, 5_422_014, 5_521_040], 1),
            ("description first", description_first, [5_111_007, 5_212_008, 5_311_013, 5_422_033, 5_521_001, 5_634_001], 2),
        ] {
            let texts = taught(block.lines());
            assert_eq!(texts.iter().map(|(id, _)| *id).collect::<Vec<_>>(), want_ids, "{name}: every panel affix's text");
            let mut checked = 0;
            for (id, raw) in &texts {
                let t = template(raw).expect("a readable text");
                let zh = t.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
                let Some(s) = table.sentences().iter().find(|s| s.ids.contains(&id.to_string())) else { continue };
                let known = if zh { &s.zh } else { &s.en };
                if known.is_empty() {
                    continue;
                }
                assert_eq!(key(&t), key(known), "{name}: {id} as the table has it");
                checked += 1;
            }
            assert!(checked >= least, "{name}: {checked} texts checked against the table");
        }
        let zh = taught(description_first.lines());
        let text = |id: u32| zh.iter().find(|(i, _)| *i == id).map_or("", |(_, t)| t.as_str());
        for (id, words) in [(5_634_001, "閃現"), (5_521_001, "移動速度"), (5_422_033, "首次受到持續傷害"), (5_311_013, "護甲值"), (5_212_008, "詛咒"), (5_111_007, "腐蝕抗性上限")] {
            assert!(text(id).contains(words), "{id}: {}", text(id));
        }
        let en = taught(fid_first_block.lines());
        let text = |id: u32| en.iter().find(|(i, _)| *i == id).map_or("", |(_, t)| t.as_str());
        for (id, words) in [
            (3_500_303, "Maim"),
            (5_111_007, "Max Erosion Resistance"),
            (5_212_010, "Blinded"),
            (5_311_003, "Low Energy Shield"),
            (5_422_014, "Damage Avoidance"),
            (5_521_040, "Focus Blessing"),
        ] {
            assert!(text(id).contains(words), "{id}: {}", text(id));
        }
    }

    /// The real `+Description` first block with every reprinted entry's fields in another order (reversed; `+Tier`
    /// first, `+Description` in the middle, `+Id` last; `+FId` first, then `+Id`, `+Description`, `+Tier`) teaches the
    /// same texts: no order gives a text to the wrong id.
    #[test]
    fn affix_texts_with_fields_in_any_order() {
        let _turn = AFFIX_TEXTS_TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((_, _, description_first)) = blocks() else { return };
        let want = taught(description_first.lines());
        assert_eq!(want.len(), 6, "the six panel affixes");
        let fid_rank = |l: &str| {
            let field = l.trim_start_matches(['|', ' ']);
            if field.contains("+FId") {
                0
            } else if field.contains("+Id [") {
                1
            } else if field.contains("+Description [") {
                2
            } else if field.contains("+Tier") {
                3
            } else {
                4
            }
        };
        let rank = |l: &str| {
            let field = l.trim_start_matches(['|', ' ']);
            if field.contains("+Tier") {
                0
            } else if field.contains("+FId") {
                1
            } else if field.contains("+Description [") {
                2
            } else if field.contains("+Id [") {
                4
            } else {
                3
            }
        };
        // every run of untimed lines (an entry's fields) reordered, the timed lines left where they are
        let reorder = |order: &dyn Fn(&mut Vec<&'static str>)| -> Vec<&'static str> {
            let mut out = Vec::new();
            let mut run: Vec<&'static str> = Vec::new();
            for l in description_first.lines() {
                if l.starts_with('[') {
                    order(&mut run);
                    out.append(&mut run);
                    out.push(l);
                } else {
                    run.push(l);
                }
            }
            order(&mut run);
            out.append(&mut run);
            out
        };
        let reversed = reorder(&|run: &mut Vec<&'static str>| run.reverse());
        let shuffled = reorder(&|run: &mut Vec<&'static str>| run.sort_by_key(|l| rank(l)));
        let fid_first = reorder(&|run: &mut Vec<&'static str>| run.sort_by_key(|l| fid_rank(l)));
        let original = description_first.lines().collect::<Vec<_>>();
        assert_ne!(reversed, original, "reversed differs from the file");
        assert_ne!(shuffled, original, "+Tier first differs from the file");
        assert_ne!(fid_first, original, "+FId first differs from the file");
        assert_eq!(taught(reversed), want, "fields reversed");
        assert_eq!(taught(shuffled), want, "+Tier first, +Description in the middle, +Id last");
        assert_eq!(taught(fid_first), want, "+FId first, then +Id, +Description, +Tier");
    }

    /// Only an entry that names its closing id alone and holds one readable text teaches it: not the list before the
    /// entries, a text after the entry's closing line, an entry with two texts, an id that does not read, an id other
    /// than the closing one, or no id; a line cut before its `]` and an untranslated key are not texts.
    #[test]
    fn affix_entries_that_do_not_pair_teach_nothing() {
        let _turn = AFFIX_TEXTS_TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = "[2026.11.08-06.26.17:925][636]TLLua: Display: [Game] ";
        let lines = [
            format!("{t}AffixInfos"),
            t.to_string(),
            "+BAffixInfoList+1+Id [5121004]".into(),
            "|              | +Description [<p>+1</p>% a list text]".into(),
            "|              +2+Id [5521026]".into(),
            "|              | +Description [<p>+2</p>% another list text]".into(),
            t.to_string(),
            "+Description [<p>+30</p>% additional Lightning Damage for monsters]".into(),
            "+Id [5121004]".into(),
            "+Description [<p>+30</p>% cut before its end".into(),
            "+Description [affix_class|description|5]".into(),
            format!("{t}5121004"),
            "+Description [<p>+3</p>% after the closing line]".into(),
            t.to_string(),
            "+Id [5521026]".into(),
            "+Description [<p>+4</p>% one]".into(),
            "+Description [<p>+5</p>% two]".into(),
            format!("{t}5521026"),
            t.to_string(),
            "+Id [5521027]".into(),
            "+Id [x]".into(),
            "+Description [<p>+6</p>% an id that does not read]".into(),
            format!("{t}5521027"),
            t.to_string(),
            "+Id [5521028]".into(),
            "+Description [<p>+7</p>% named 5521028, closed as 5521029]".into(),
            format!("{t}5521029"),
            t.to_string(),
            "+Description [<p>+8</p>% no id at all]".into(),
            format!("{t}5521030"),
            format!("{t}NetGameMgr:OnEnterAreaEnd()"),
        ];
        assert_eq!(taught(lines.iter().map(String::as_str)), vec![(5_121_004, "<p>+30</p>% additional Lightning Damage for monsters".to_string())]);
    }

    /// The look back reads every UE_game*.log of the folder, the backups first, and keeps their panel affixes' texts;
    /// other files are not read; a block the log ends in counts. (The real check: tests\affix_log_replay.rs on the
    /// user's logs.)
    #[test]
    fn looking_back_reads_every_game_log() {
        let _turn = AFFIX_TEXTS_TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("affix-look-back-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = "[2026.11.08-06.26.17:925][636]TLLua: Display: [Game] ";
        let block = |id: u32, text: &str, closed: bool| {
            let mut lines = vec![format!("{t}AffixInfos"), t.to_string(), format!("+Id [{id}]"), "+FId [1141]".into(), format!("+Description [{text}]"), format!("{t}{id}")];
            if closed {
                lines.push(format!("{t}NetGameMgr:OnEnterAreaEnd()"));
            }
            lines.join("\r\n") + "\r\n"
        };
        std::fs::write(dir.join("UE_game.log"), block(5121004, "<p>+30</p>% additional Lightning Damage for monsters", false)).unwrap();
        std::fs::write(dir.join("UE_game-backup-2026.11.08-06.36.02.log"), block(5311021, "<p>+15</p>% Blur effect for you", true)).unwrap();
        std::fs::write(dir.join("other.log"), block(5112007, "not a game log", true)).unwrap();
        let _ = take_affix_texts();
        assert_eq!(look_back_affixes(&dir.join("UE_game.log"), &AtomicBool::new(false)), 2);
        let texts = take_affix_texts();
        assert_eq!(texts.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [5_311_021, 5_121_004], "{texts:?}");
        assert_eq!(look_back_affixes(&dir.join("UE_game.log"), &AtomicBool::new(true)), 0, "stopped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A block without ids gives nothing, but after a level entered (the hideout's) its level with no id; a short list
    /// without number lines gives its own ids.
    #[test]
    fn affix_blocks_empty_or_short() {
        let t = "[2026.11.08-05.13.28:740][ 63]TLLua: Display: [Game] ";
        let mut block = AffixBlock::default();
        let empty = [format!("{t}AffixInfos"), t.to_string(), "+BaseAttrInfoList".into(), "+BAffixInfoList".into(), format!("{t}NetGameMgr:OnEnterAreaEnd()")];
        assert!(empty.iter().all(|l| block.line(l).is_none()));
        let hideout = "[2026.11.08-05.13.27:100][ 63]GameLog: Display: [Game] UGameMgr::EnterLevel(110)".to_string();
        let got: Vec<Signal> = std::iter::once(&hideout).chain(empty.iter()).filter_map(|l| block.line(l)).collect();
        let [Signal::MapAffixes(a)] = got.as_slice() else { panic!("{got:?}") };
        assert_eq!((a.level, a.ids(), a.others), (Some(110), &[][..], 0), "the hideout: its level, no id");
        let mut block = AffixBlock::default();
        let short = [format!("{t}AffixInfos"), "+BAffixInfoList+1+Id [5521040]".into(), "|              | +FId [1141]".into(), format!("{t}NetGameMgr:OnEnterAreaEnd()")];
        let got: Vec<Signal> = short.iter().filter_map(|l| block.line(l)).collect();
        let [Signal::MapAffixes(a)] = got.as_slice() else { panic!("{got:?}") };
        assert_eq!((a.level, a.ids(), a.others), (None, &[5_521_040][..], 0));
    }
}
