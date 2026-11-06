//! The app's state without the UI: the reading of the last frame, the
//! player's corrections and held instruments, and the ranking, which a
//! worker thread fills in rounds so the window shows it as it firms up.

use crate::board::{CellModel, ChoiceKind, EventMark, Model, Step, build_state};
use crate::engine::actions::apply;
use crate::engine::incidents::{CHAIN_REACTION, USURPATION_I, usurped};
use crate::engine::data::{Beast, CAGE_FIGHT, Gift, IRON_LAW, RAINBOW, Tool, VAULT_CAP, data};
use crate::engine::rules::treasure_picks;
use crate::engine::text::{cell_label, cell_list, inmate_name, instrument, instrument_term, theme_name, tool_name};
use crate::lang::{tr, trf};
use crate::reader::model::{RUN_DAYS, Warning, WarningKind, iron_law_days, least_sure, to_model};
use crate::reader::panels::{Panels, Screen, Spot, card_places, card_spot, check_doors, read_panels_titled, two_card_row};
use crate::reader::scene::{CELLS, Scene, read_scene};
use crate::reader::layout::{Layout, is_16_9, lay_out_found};
use crate::reader::click;
use crate::reader::{REF_H, REF_W, Rgba};
use crate::solver::TUNED;
use super::gamelog::{self, Signal};
use super::runlog::{RunLog, timestamp};
use crate::solver::advise::{Advice, Pick, advise, also_good, prefer_change, why_shown};
use serde_json::json;
use crate::engine::{Action, ChoiceId};
use crate::engine::state::{Pending, State};
use crate::solver::solve::{Rollouts, SolveOption, instant, instant_bite};
use crate::engine::devour::{devour_options, eats, offer_chain, start_options, upgrade_chance};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::SystemTime;

/// Rounds the worker plays between two partial rankings.
pub const ROUND: usize = 50;
/// The seed of the board's hidden parts and of the playouts, as in `ascendant-cli solve`.
const SEED: u32 = 1;

/// A correction to a cell the reader got wrong.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellEdit {
    Locked,
    Empty,
    Inmate { beast: Beast, tier: i64 },
}

/// What the reader made of a frame, with the player's corrections applied.
#[derive(Clone, Debug)]
pub struct Reading {
    pub model: Model,
    pub warnings: Vec<Warning>,
    /// The screen the frame shows.
    pub screen: Screen,
    /// "N days until Riot", read or kept from an earlier reading (see [`DaysClock`]).
    pub days_left: Option<u32>,
    /// When the frame was read (corrections keep it).
    pub at: SystemTime,
    /// The eater on show is the one the game's jaws called for, its portrait
    /// still changing after its bite (see `eater_from_jaws`): the advice
    /// shows, its mark not, until a read shows the eater or the next read takes the same eater from the same jaws
    /// (see `Session::inferred_from`). A Wolf so taken
    /// was a Lion 0.23 s on, and the mark went from End Chain to cell 09
    /// (2026-11-02, day 15; day 13 the same).
    pub eater_inferred: bool,
}

impl Reading {
    /// The lines saying why the frames after this reading were not read
    /// (a popup, a screen without the board, a window narrower than 16:9, the
    /// theme scroll): empty unless the reading on show is a kept one. The
    /// next reading installed starts without them.
    pub fn kept_lines(&self) -> Vec<&str> {
        self.warnings.iter().filter(|w| kept_line(&w.text)).map(|w| w.text.as_str()).collect()
    }
}

/// Whether a warning is a line [`Session::take_read`] puts over a kept reading.
fn kept_line(text: &str) -> bool {
    is_line(text, POPUP)
        || is_line(text, OFF_BOARD)
        || is_line(text, FADING)
        || is_line(text, BITE_UNSHOWN)
        || is_line(text, REPLACE_UNSHOWN)
        || is_line(text, DAY_UNSHOWN)
        || is_line(text, CARDS_FADING)
        || is_line(text, OFFER_PARTIAL)
        || begins(text, THEME_READ)
        || begins(text, SHAPE)
}

/// Whether `text` is the line `en`, in English or translated: the language
/// can change between making a line and checking it.
fn is_line(text: &str, en: &'static str) -> bool {
    text == en || text == tr(en)
}

/// Whether `text` starts with `en`, in English or translated; see [`is_line`].
fn begins(text: &str, en: &'static str) -> bool {
    text.starts_with(en) || text.starts_with(tr(en))
}

/// The days left until the Riot over a run's readings. The counter only
/// counts down, so a read equal to or lower than the last count is taken, a
/// missing read keeps the last count, and a higher read is a misread unless
/// the same value reads twice in a row (a new run).
#[derive(Clone, Copy, Debug, Default)]
pub struct DaysClock {
    left: Option<u32>,
    /// A higher read waiting for its second reading.
    higher: Option<u32>,
}

impl DaysClock {
    /// Takes one reading's days read and gives the count to use: None
    /// until a first read.
    pub fn update(&mut self, read: Option<u32>) -> Option<u32> {
        let higher = self.higher.take();
        match (read, self.left) {
            (Some(r), Some(left)) if r > left && higher != Some(r) => self.higher = Some(r),
            (Some(r), _) => self.left = Some(r),
            (None, _) => {}
        }
        self.left
    }

    /// [`DaysClock::update`], and whether it took a higher count (a new run).
    /// A `sure` read (the game's log names its day) needs no second reading.
    fn update_run(&mut self, read: Option<u32>, sure: bool) -> (Option<u32>, bool) {
        let before = self.left;
        if sure {
            self.higher = None;
            self.left = read.or(self.left);
        }
        let left = if sure { self.left } else { self.update(read) };
        (left, matches!((before, left), (Some(b), Some(l)) if l > b))
    }
}

/// Whether `screen` is a pick that covers the board with no board read under
/// it (`under`): the run start is assumed.
fn run_start(screen: Screen, under: Option<&Model>) -> bool {
    matches!(screen, Screen::InstrumentPick | Screen::TreasurePick) && under.is_none()
}

/// The warning for a days count kept from an earlier reading, when `read`
/// is not the count in use.
fn kept_days(read: Option<u32>, left: Option<u32>) -> Option<Warning> {
    let left = left.filter(|&n| read != Some(n))?;
    let (kind, text) = match read {
        Some(r) => (WarningKind::Read, trf!("The days counter read {0}, more than the last count: {1} day(s) until Riot kept until {0} reads again.", r, left)),
        None => (WarningKind::Note, trf!("The days counter is hidden: {} day(s) until Riot kept from the last read.", left)),
    };
    Some(Warning { cell: None, kind, text })
}

/// The board under a treasure pick after a consume, and the eater's model
/// index. The day's consume starts in the warden's cell and chain bites keep
/// the eater there; at Chromatic it leaves for the vault and its cell locks
/// again, as every consumed cell does, the last meal's cell (`meal`) too.
/// None when the warden's cell holds no inmate: the board read already shows
/// it gone. With no Consume target screen read since the board (`meal` None)
/// and the Ravenous Chains held, the Chromatic comes from the Chains at the
/// start of a day instead when they can lift the board's top inmate that far;
/// the removed neighbours are the returned list.
fn vaulted(board: &Model, meal: Option<Meal>, chains: bool) -> Option<(Model, usize, Vec<usize>)> {
    if meal.is_none() && chains && let Some(v) = chains_vaulted(board) {
        return Some(v);
    }
    let n = usize::try_from(board.warden).ok()?;
    let &CellModel::Inmate { beast, event, .. } = board.cells.get(n)? else { return None };
    let mut m = board.clone();
    m.cells[n] = CellModel::Sealed { event };
    m.vault.push(beast);
    if let Some(meal) = meal
        && let Some(&CellModel::Inmate { event, .. }) = m.cells.get(meal.cell)
    {
        m.cells[meal.cell] = CellModel::Sealed { event };
    }
    Some((m, n, Vec::new()))
}

/// The board after the Ravenous Chains sent the board's top inmate to the
/// vault: it and its neighbours on the ring lock. None when the top rarity is
/// shared (the Chains pick one at random) or the Chains' tiers, one roll per
/// neighbour, can't lift it to Chromatic.
fn chains_vaulted(board: &Model) -> Option<(Model, usize, Vec<usize>)> {
    if board.vault.len() >= VAULT_CAP as usize {
        return None;
    }
    let inmate = |c: &CellModel| match *c {
        CellModel::Inmate { beast, tier, .. } => Some((tier, beast)),
        _ => None,
    };
    let top = board.cells.iter().filter_map(|c| inmate(c).map(|(t, _)| t)).max()?;
    let [(n, beast)] = board.cells.iter().enumerate().filter_map(|(n, c)| inmate(c).filter(|&(t, _)| t == top).map(|(_, b)| (n, b))).collect::<Vec<_>>()[..] else {
        return None;
    };
    let len = board.cells.len();
    let sides: Vec<usize> = [n + len - 1, n + 1].map(|k| k % len).into_iter().filter(|&k| inmate(&board.cells[k]).is_some()).collect();
    let gain = data().tool(Tool::Chains).tiers.unwrap_or(0) * sides.len() as i32;
    if top + i64::from(gain) < i64::from(RAINBOW) {
        return None;
    }
    let mut m = board.clone();
    for &k in sides.iter().chain([&n]) {
        let event = m.cells[k].event();
        m.cells[k] = CellModel::Sealed { event };
    }
    m.vault.push(beast);
    Some((m, n, sides))
}

/// What a pick over the board (a treasure's or an instrument's) came after.
/// The pick hides the decision buttons, and View Prison Status under it shows
/// none either, so the day's Consume, Replace and Hunt as spent come from here.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PickAfter {
    /// A Consume, which ends the day: nothing is left.
    Consume,
    /// A Replace, whose new inmate can reach Chromatic: the Consume is left
    /// (a 2026-10-28 run consumed after two such treasures).
    Replace { hunt_used: bool },
    /// The patrol, with no manage screen yet (an incident, an objective's
    /// reward): nothing is spent.
    Patrol,
    /// A new day's start, its counter read on the way (see
    /// [`Session::pick_opens`]): nothing is spent.
    DayStart,
}

impl PickAfter {
    /// The day's Consume, Replace and Hunt as spent.
    fn used(self) -> (bool, bool, bool) {
        match self {
            PickAfter::Consume => (true, true, true),
            PickAfter::Replace { hunt_used } => (false, true, hunt_used),
            PickAfter::Patrol | PickAfter::DayStart => (false, false, false),
        }
    }
}

/// The least time from a target screen that shows a bite to the next bite
/// done: the player's click and the bite's animation (1.4 s on 2026-10-28
/// chains).
const NEXT_BITE: std::time::Duration = std::time::Duration::from_millis(1000);

/// The Legendary tier: the Crimson Wine raises the inmates below it (see
/// `engine::gifts`).
const LEGENDARY: i64 = 3;

/// The most bites a chain line shows.
const CHAIN_PLAN_MAX: usize = 12;

/// The line under a treasure pick after a Replace.
const REPLACED: &str = "The Warden's Treasure follows a Replace: the board under it may have changed. Check it with View Prison Status.";
/// The line under a pick right after the patrol.
const AFTER_PATROL: &str = "This pick follows the patrol: an incident or the warden's move may have changed the board under it. Check it with View Prison Status.";

/// The meal picked on the last Consume target screen read: the only target
/// the screen offered (certain), the one the read after the bite showed eaten
/// in place of the one advised (certain, see [`meal_read`]), or the one
/// Ascendant advised there.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Meal {
    cell: usize,
    certain: bool,
}

/// The cells, as model indices, of the prey the board's Lions have marked.
fn prey_cells(m: &Model) -> impl Iterator<Item = usize> + '_ {
    m.cells.iter().filter_map(|c| match c {
        CellModel::Inmate { prey: Some(mark), .. } => mark.cell.and_then(|x| usize::try_from(x).ok()),
        _ => None,
    })
}

/// True when a Consume target screen read shows a bite since `before`: the
/// inmate in the warden's cell changed, or another inmate left its cell. The
/// screen then picks a chain's next meal, not the day's first.
fn bitten(before: &Model, read: &Model) -> bool {
    let eater = usize::try_from(read.warden).ok();
    before.cells.iter().zip(&read.cells).enumerate().any(|(n, pair)| match pair {
        (CellModel::Inmate { beast, tier, .. }, CellModel::Inmate { beast: b, tier: t, .. }) => {
            Some(n) == eater && (beast, tier) != (b, t)
        }
        (CellModel::Inmate { .. }, CellModel::Empty { .. } | CellModel::Sealed { .. }) => true,
        _ => false,
    })
}

/// True when a Consume target screen read shows a bite since `before`: an
/// inmate other than the eater left its cell, or, with the meal not known,
/// [`bitten`]. Mid-bite the eater's portrait reads as anything (a Normal
/// Rat for a Normal Mammoth on a 2026-10-29 run), the meal still in its cell.
/// A cell under the big jaws (`jawed`, model indices) is a target still: its
/// read, spoiled by them, counts for nothing (2026-10-30: a frame from before
/// the bite took for one, and the advice named the meal about to go).
fn bite_shown(before: &Model, read: &Model, meal: Option<usize>, jawed: &[usize]) -> bool {
    let eater = usize::try_from(read.warden).ok();
    let left = before.cells.iter().zip(&read.cells).enumerate().any(|(n, pair)| {
        Some(n) != eater && !jawed.contains(&n) && matches!(pair, (CellModel::Inmate { .. }, CellModel::Empty { .. } | CellModel::Sealed { .. }))
    });
    // A scratched door eaten locks again: no inmate left it, and the bite
    // was held BITE_SHOWS for one (2026-11-01, day 15).
    let door = meal.is_some_and(|m| matches!((before.cells.get(m), read.cells.get(m)), (Some(CellModel::Scratch { .. }), Some(CellModel::Sealed { .. } | CellModel::Empty { .. }))));
    left || door || (meal.is_none() && bitten(before, read))
}

/// What a Consume target read right after a bite shows of its meal, see [`meal_read`].
#[derive(Clone, Debug, PartialEq)]
enum MealRead {
    /// Nothing shows against the meal advised: it stands.
    Advised,
    /// The meal advised is still in its cell after the bite, and this other target is the one that went.
    Other(usize),
    /// The meal advised is still in its cell after the bite, and the read does not show which of these other targets
    /// went.
    Unknown(Vec<usize>),
}

/// Which target a bite ate, by a Consume target read after it (`read`, the reader's warnings `warns`, the jaws
/// `jaws`, model indices) against the board before it (`before`), the targets offered before it (`offered`) and the
/// one advised (`advised`). The player clicks fast and need not take the advice: on 2026-11-06 day 7 (08:17:44 UTC)
/// "Chain on into 16" was advised with 14 as good, 14 was eaten, and Avenger's Casket put a Legendary Mammoth there
/// under the jaws, its read unsure. Taken as 16, cell 14 kept the Rare Alpha Wolf eaten, the eater had nothing to
/// eat on the board read, and no advice showed for 2.8 s until the player cancelled the chain.
///
/// The advice is taken as the meal unless the read shows it uneaten: its cell read for sure as the same inmate,
/// while the bite shows (the eater read risen for sure, or another target out of the jaws read for sure as changed).
/// The meal is then the one other target not read for sure as the same, whose read differs from before; with
/// none, the advice still stands (a refill that looks the same); with more, or one that reads as before, which
/// went is unknown, and the read is held and read again (see `Session::take_read`).
fn meal_read(before: &Model, read: &Model, warns: &[Warning], jaws: &[usize], offered: &[usize], advised: usize, unedited: impl Fn(usize) -> bool) -> MealRead {
    if offered.len() < 2 || !offered.contains(&advised) {
        return MealRead::Advised;
    }
    let sure = |n: usize| unedited(n) && !warns.iter().any(|w| w.cell == Some(n) && begins(&w.text, CHECK_CELL));
    let sight = |m: &Model, n: usize| m.cells.get(n).map(cell_sight);
    let was_inmate = |n: usize| matches!(before.cells.get(n), Some(CellModel::Inmate { .. }));
    // Read for sure as the same inmate as before.
    let kept = |n: usize| sure(n) && was_inmate(n) && sight(before, n) == sight(read, n);
    // Read as another inmate, or as none out of the jaws: a cell under them read without an inmate is spoiled by them.
    let differs = |n: usize| {
        matches!(before.cells.get(n), Some(CellModel::Inmate { .. } | CellModel::Scratch { .. }))
            && sight(before, n) != sight(read, n)
            && (matches!(read.cells.get(n), Some(CellModel::Inmate { .. })) || !jaws.contains(&n))
    };
    let eater_risen = usize::try_from(before.warden).is_ok_and(|w| {
        let sure_read = sure_eater(before, read, warns, &unedited);
        matches!((before.cells.get(w), sure_read.cells.get(w)), (Some(CellModel::Inmate { tier: from, .. }), Some(CellModel::Inmate { tier, .. })) if tier > from)
    });
    let shows = eater_risen || offered.iter().any(|&n| n != advised && !jaws.contains(&n) && sure(n) && differs(n));
    if !shows || !kept(advised) {
        return MealRead::Advised;
    }
    let rest: Vec<usize> = offered.iter().copied().filter(|&n| n != advised && !kept(n)).collect();
    match rest[..] {
        [] => MealRead::Advised,
        [n] if differs(n) => MealRead::Other(n),
        _ => MealRead::Unknown(rest),
    }
}

/// Whether two moves on the manage screen click the same card: a Consume
/// or a Hunt, whatever its target, is its card.
fn same_card(a: &Action, b: &Action) -> bool {
    match (a, b) {
        (Action::Devour { .. }, Action::Devour { .. }) | (Action::Hunt { .. }, Action::Hunt { .. }) => true,
        _ => a == b,
    }
}

/// The eater `cell` in model cell `n` as [`Session::eater_seen`] keeps it.
fn eater_in(cell: &CellModel, n: usize) -> Option<(usize, Beast, i64)> {
    match *cell {
        CellModel::Inmate { beast, tier, .. } => Some((n, beast, tier)),
        _ => None,
    }
}

/// Whether `read` shows the eater of `before` no higher: a bite raises it
/// (and re-rolls its beast), so the read caught its rise. A Wolf that the
/// Fanged Guillotine kept from rising is held until [`BITE_SHOWS`].
fn eater_misread(before: &Model, read: &Model) -> bool {
    let Ok(eater) = usize::try_from(before.warden) else { return false };
    match (before.cells.get(eater), read.cells.get(eater)) {
        (Some(CellModel::Inmate { tier: from, .. }), Some(CellModel::Inmate { tier, .. })) => tier <= from,
        _ => false,
    }
}

/// `read` with the eater's cell as the reading would take it: a read of it the reader warns on (`warns`), unless the
/// player corrected the cell (`unedited` false), gives way to the cell of `before`, as on the Consume target screen
/// (see `Session::rebuilt`).
fn sure_eater(before: &Model, read: &Model, warns: &[Warning], unedited: impl Fn(usize) -> bool) -> Model {
    let mut sure = read.clone();
    if let Ok(eater) = usize::try_from(before.warden)
        && unedited(eater)
        && warns.iter().any(|w| w.cell == Some(eater) && begins(&w.text, CHECK_CELL))
        && let (Some(cell), Some(was)) = (sure.cells.get_mut(eater), before.cells.get(eater))
    {
        *cell = was.clone();
    }
    sure
}

/// Whether the eater `read` shows unrisen after eating the inmate `before`
/// held in cell `meal` may stay so: the bite could fail to raise it (the
/// Fanged Guillotine's Oppress on a Wolf eating below its rarity), and the
/// targets the screen marks (`jaws`, model indices) are just those of the
/// eater as read. Such a bite was held [`BITE_SHOWS`] for a rise that never
/// came, twice in a 2026-10-31 run: once until the player bit again with no
/// advice on show.
fn kept_from_rising(before: &Model, read: &Model, meal: usize, jaws: &[usize]) -> bool {
    let (Ok(eater), false) = (usize::try_from(read.warden), jaws.is_empty()) else { return false };
    let (was, _) = build_state(before, SEED);
    let (now, _) = build_state(read, SEED);
    let (Some(eater_was), Some(food)) = (was.cells.get(eater).and_then(|c| c.inmate), was.cells.get(meal).and_then(|c| c.inmate)) else {
        return false;
    };
    let targets: Vec<usize> = devour_options(&now, eater).iter().map(|b| b.cell).collect();
    upgrade_chance(&was, &eater_was, &food) < 1.0 && targets.len() == jaws.len() && jaws.iter().all(|n| targets.contains(n))
}

/// Something to click on a panel, the layout its spot is in, and the spot.
type SpotAt = (Spot, Layout, (i32, i32));

/// What a reading shares with the one whose advice is held on show while a
/// read solves again (see `Session::shown_advice`): its screen, day, the
/// warden's cell and inmate, the moves spent today, and the treasure gifts
/// the game's log saw confirmed: a gift held from a pick shows on no other
/// pick (see also `Session::note_shown`).
type HeldKey = (Screen, i64, i64, Option<CellModel>, [bool; 3], usize);

fn held_key(r: &Reading, gifts: usize) -> HeldKey {
    let m = &r.model;
    let here = usize::try_from(m.warden).ok().and_then(|w| m.cells.get(w)).cloned();
    (r.screen, m.day, m.warden, here, [m.devour_used, m.swap_used, m.hunt_used], gifts)
}

/// A card of the patrol on show, read in full, that misses a read later is
/// the door read before: its glow flickering, "Go to 10" read as nothing for
/// 0.4 s, and the advice moved from 10 to 12 as the player clicked 10
/// (2026-11-01, day 19). The same patrol is the same day, warden's cell and
/// number of cards, the cards read the same.
fn fill_doors(doors: &mut [Option<u8>], was: &Reading, days: Option<u32>, warden: u8) {
    let dest = &was.model.dest;
    if was.screen == Screen::Patrol
        && was.days_left == days
        && was.model.warden + 1 == i64::from(warden)
        && dest.len() == doors.len()
        && doors.iter().zip(dest).all(|(d, &n)| d.is_none_or(|d| usize::from(d) == n + 1))
    {
        for (door, &n) in doors.iter_mut().zip(dest) {
            *door = door.or(u8::try_from(n + 1).ok());
        }
    }
}

/// Puts the model's objective at its target, as the game's log says it
/// completed; true when that changed it.
fn complete_objective(model: &mut Model) -> bool {
    let Some(goal) = &mut model.objective else { return false };
    let Some(target) = data().objective(goal.id).map(|o| o.target) else { return false };
    let changed = goal.progress < target;
    goal.progress = target;
    changed
}

/// Whether a low-confidence read of a cell warns: only when its result differs from the cell in the reading on show
/// before it (`before`), so a right read with a low confidence does not warn on every read (2026-11-05: most of a
/// run's warnings were reads that were right). A cell the warning does not name (`now` None) always warns.
fn warns_again(now: Option<&CellModel>, before: Option<&CellModel>) -> bool {
    now.is_none() || now != before
}

/// Which low-confidence cell warnings of a read show. `keys` pairs each warning's key (see [`steady_key`]) with
/// whether [`warns_again`] lets it warn; `shown` is the keys shown on the reads before on the same screen and day. A
/// warning once shown stays on show while the reads still give it: the second read of the same result hid it, so it
/// showed for one read (about 0.2 s) and went. One not shown stays hidden until its result changes, which gives a new
/// key. Gives each key's shown flag, and the keys shown now to keep for the next read.
fn steady_shown(keys: Vec<(String, bool)>, shown: &BTreeSet<String>) -> (BTreeMap<String, bool>, BTreeSet<String>) {
    let flags: BTreeMap<String, bool> = keys.into_iter().map(|(k, warns)| {
        let on = warns || shown.contains(&k);
        (k, on)
    }).collect();
    let kept = flags.iter().filter(|(_, on)| **on).map(|(k, _)| k.clone()).collect();
    (flags, kept)
}

/// The screen and day the warnings shown are kept for (see [`steady_shown`]), `was` the last read's: a read with no
/// days count keeps the day before, so one unread countdown does not drop the warnings on show. Another screen, or
/// another day read, starts afresh; a new run clears them apart from this.
fn warned_scope((screen, days): (Screen, Option<u32>), was: Option<(Screen, Option<u32>)>) -> (Screen, Option<u32>) {
    (screen, days.or(was.and_then(|(_, d)| d)))
}

/// The key of a low-confidence cell warning: its text without the
/// confidence number, so the same read on the same cell gives the same key.
fn steady_key(w: &Warning) -> Option<String> {
    let (text, _) = w.text.split_once("confidence ").or_else(|| w.text.split_once(tr("confidence ")))?;
    begins(text, CHECK_CELL).then(|| text.to_string())
}

/// How the reader's warnings about one cell's read start.
const CHECK_CELL: &str = "Check cell ";

/// How the warning about Consume targets the board read does not allow starts.
const TARGETS_DIFFER: &str = "The Consume targets on screen are ";

/// The eater in `st` once its bite's change shows: a bite re-rolls the
/// eater's beast and raises its rarity (see [`crate::engine::devour::do_devour`]),
/// and the Consume target screen marks the next targets (`jaws`, model
/// indices) before the portrait changes, for 1 to 3 s (4 of 29 chain bites of
/// a 2026-10-28 run gave no advice that long). Each beast, a rarity up, whose
/// targets on the board cover the jawed cells may stand in (a Lion and a Wolf
/// both eat Rats); those whose targets are just the jawed cells, if any, as
/// the game marks every target. The state with it and its chain offered,
/// and the inmate, when every such beast gets the same instant pick; None
/// otherwise.
/// The portrait read may have risen already, its beast misread mid-change:
/// its rarity stands in too. A Rare Rat so read was taken a rarity up, as a
/// Legendary Rat, whose targets took in a cell the game did not mark, and
/// no advice showed for 1.8 s of a Cage Fight chain (2026-11-01). Where the
/// eater's beast is known (`beast`: a Cage Fight's bites keep it), only it.
fn eater_from_jaws(st: &State, jaws: &[usize], beast: Option<Beast>) -> Option<(State, crate::engine::state::Inmate)> {
    let n = st.position;
    let old = st.cells.get(n)?.inmate?;
    if jaws.is_empty() {
        return None;
    }
    let beasts = beast.map_or(Beast::ALL.to_vec(), |b| vec![b]);
    let mut tiers = vec![(old.tier + 1).min(RAINBOW - 1), old.tier];
    tiers.dedup();
    let fits: Vec<(State, crate::engine::state::Inmate, bool)> = beasts
        .iter()
        .flat_map(|&beast| tiers.iter().map(move |&tier| (beast, tier)))
        .filter_map(|(beast, tier)| {
            let mut s = st.clone();
            let eater = crate::engine::state::Inmate { beast, tier, ..old };
            s.cells[n].inmate = Some(eater);
            offer_chain(&mut s, old.id);
            let options = s.pending.iter().find_map(|p| match p {
                Pending::Chain { options, .. } => Some(options.clone()),
                _ => None,
            })?;
            jaws.iter().all(|c| options.contains(c)).then_some((s, eater, options.len() == jaws.len()))
        })
        .collect();
    let exact = fits.iter().any(|(_, _, exact)| *exact);
    let mut fits: Vec<_> = fits.into_iter().filter(|(_, _, e)| *e || !exact).map(|(s, eater, _)| (s, eater)).collect();
    // Of those that fit as well, a rarity up, as the bite raises it.
    let risen = fits.iter().any(|(_, e)| e.tier > old.tier);
    fits.retain(|(_, e)| !risen || e.tier > old.tier);
    let picks: Vec<Option<Action>> = fits.iter().map(|(s, _)| instant(s).map(|i| i.action)).collect();
    let agree = picks.first().is_some_and(Option::is_some) && picks.windows(2).all(|w| w[0] == w[1]);
    agree.then(|| fits.into_iter().next()).flatten()
}

/// The targets of a Consume target pick: the engine's `options` on the read
/// board that the screen marks with big jaws (`jawed`, model indices). When
/// the two differ, a warning on the eater's cell; when none of the jawed
/// cells is among the options, or no jaws were read, the options as they are.
pub fn consume_targets(options: &[usize], jawed: &[usize], eater: usize) -> (Vec<usize>, Option<Warning>) {
    if jawed.is_empty() || (jawed.len() == options.len() && options.iter().all(|n| jawed.contains(n))) {
        return (options.to_vec(), None);
    }
    let text = trf!(
        "The Consume targets on screen are {} and the board read allows {}: check the eater in cell {} and those cells.",
        cell_list(jawed),
        cell_list(options),
        cell_label(eater),
    );
    let both: Vec<usize> = options.iter().copied().filter(|n| jawed.contains(n)).collect();
    let targets = if both.is_empty() { options.to_vec() } else { both };
    (targets, Some(Warning { cell: Some(eater), kind: WarningKind::Read, text }))
}

/// The cells a Consume target screen marks with big jaws, as model indices.
fn jawed(scene: &Scene) -> Vec<usize> {
    CELLS.filter(|&c| scene.cell(c).target).map(|c| c as usize - 1).collect()
}

/// Avenger's Casket opens a locked door after a bite with a meal for the
/// eater, at its rarity, and the target screen puts the big jaws on it, which
/// spoil its read. A jawed cell (`jawed`, model indices) that `m` holds
/// without an inmate becomes that meal: the eater's rarity and the beast
/// `read` shows when the eater eats it, else what the eater eats, with a
/// warning on the cell when that is more than one beast.
pub fn casket_meals(m: &mut Model, read: &Model, jawed: &[usize]) -> Vec<Warning> {
    let eater = usize::try_from(m.warden).ok().and_then(|w| m.cells.get(w));
    let Some(&CellModel::Inmate { beast: eater, tier, .. }) = eater else {
        return Vec::new();
    };
    let food = eats(eater);
    let mut warnings = Vec::new();
    for &n in jawed {
        if matches!(m.cells[n], CellModel::Inmate { .. }) {
            continue;
        }
        let seen = match read.cells[n] {
            CellModel::Inmate { beast, .. } => Some(beast).filter(|b| food.contains(b)),
            _ => None,
        };
        let beast = seen.unwrap_or(food[0]);
        if seen.is_none() && food.len() > 1 {
            let text = trf!(
                "Check cell {}: a new Inmate from {}, taken as {}: click the cell to correct it.",
                cell_label(n),
                tool_name(Tool::Revenge.key()),
                inmate_name(tier as i32, beast),
            );
            warnings.push(Warning { cell: Some(n), kind: WarningKind::Read, text });
        }
        m.cells[n] = CellModel::Inmate { beast, tier, ivory: m.cells[n].ivory(), prey: None, event: m.cells[n].event() };
    }
    warnings
}

/// The line shown over the last reading while a game dialog covers the board.
const POPUP: &str = "A popup covers the game: showing the last reading.";
/// The line shown over the last reading while the board is not on screen.
const OFF_BOARD: &str = "The prison board is not on screen: showing the last reading.";
/// The line shown over the last reading while the board fades in after a Consume target screen.
const FADING: &str = "The board is still coming in: showing the last reading.";
/// How many consecutive reads of the same theme the theme charm needs before it is trusted: no theme was ever misread
/// in 6466 screenshots, so this is insurance (2026-11-03).
const THEME_ICON_READS: usize = 3;

/// How the line shown over the last reading after the run theme scroll starts.
const THEME_READ: &str = "The run's theme: ";
/// The line for a read kept between runs, see [`Session::install`].
const RUN_OVER_WAIT: &str = "The run is over: waiting for the next run.";
/// The line for a treasure pick that opened after a gift was taken, see [`Session::install`].
const NEXT_TREASURE: &str = "The next Warden's Treasure: the gift just taken may have changed the board under it. View Prison Status shows it.";
/// The line for a treasure pick in a row after the Crimson Wine was advised, see [`Session::took_wine`].
const WINE_TAKEN: &str = "The next Warden's Treasure: the Crimson Wine advised before it raised every inmate below Legendary one tier on the board. If you took another gift, check it with View Prison Status.";
/// The line for a treasure pick after a consume whose eater did not read, see [`Session::build`].
const LOST_EATER: &str = "The Warden's Treasure follows a Consume whose eater did not read: the board under it may be out of date. Check it with View Prison Status.";
/// How long the run's end shows on the game, see [`Session::notice`].
const RUN_OVER_NOTICE: std::time::Duration = std::time::Duration::from_secs(8);
/// How long the solve can find no advice before the reason shows on the
/// game, see [`Session::notice`]: a Consume target read while the eater
/// still changes after its bite lasted up to 2.8 s.
const NO_ADVICE_NOTICE: std::time::Duration = std::time::Duration::from_secs(3);
/// The most Warden's Treasures [`Session::treasures_in_a_row`] counts.
const TREASURES_MAX: usize = 6;

/// The longest a move played hides its mark (see [`Session::mark`]) when no
/// read shows its result: a click the game took without a change.
const PLAYED_HIDE: std::time::Duration = std::time::Duration::from_millis(2500);
/// How long after a bite the game's log counted a Consume target read that
/// shows no bite is the screen from before it, see [`Session::take_read`].
const BITE_SHOWS: std::time::Duration = std::time::Duration::from_millis(1500);
/// How long after a devour a target-screen read must record the conditions of the hold check, see
/// [`Session::unshown_why`].
const UNSHOWN_LOGGED: std::time::Duration = std::time::Duration::from_millis(3000);
/// From this long after a bite, a board read again the same whose targets
/// still differ from the game's marks is no longer changing: its advice
/// shows (see [`Session::shown_advice`]). Each of 3 bites so held to
/// BITE_SHOWS in 2026-11-01 and 11-02 runs (two with Avenger's Casket)
/// showed the same advice 0.4 s on.
const DIFFER_STEADY: std::time::Duration = std::time::Duration::from_millis(1000);
/// The line shown over the last reading while a Consume target screen does not show the last bite yet.
const BITE_UNSHOWN: &str = "The board does not show the last bite yet: showing the last reading.";
/// A manage screen read after Replace was clicked, Replace gone, with the
/// warden's cell still holding the inmate replaced: the new one shows about
/// 0.45 s after the click (the log's rarity sound comes at 0.09 s), and the
/// advice ended the day on the old inmate (2026-10-30, three times in four
/// runs). Held for [`REPLACE_SHOWS`] at most.
const REPLACE_UNSHOWN: &str = "The new inmate is not on show yet: showing the last reading.";
const REPLACE_SHOWS: std::time::Duration = std::time::Duration::from_millis(1000);
/// The hold message for a day change not yet drawn on the board: the patrol cards have appeared but the on-screen
/// countdown is still the previous day, and the day-break effects have not finished playing either (2026-11-03 day 6:
/// cell 13 ranked up and cell 15 disappeared, and the board was only updated 0.27 s later; the advice first gave 08
/// and the mark then jumped to 10). Held for at most [`DAY_SHOWS`]: the countdown may also stay on the previous day
/// for the whole patrol (2026-10-28 day 6), so we cannot wait forever.
const DAY_UNSHOWN: &str = "The new day is not on the board yet: showing the last reading.";
const DAY_SHOWS: std::time::Duration = std::time::Duration::from_millis(1000);
/// How long after the log tells of the click a Replace's flash and fade-in
/// last: observe frames showed the old inmate at +0.06 s, a white flash at
/// +0.16 s, the new one fading in at +0.27 s and on show at +0.38 s
/// (2026-10-30). A read begun before is held whatever its cell reads.
const REPLACE_FADE: std::time::Duration = std::time::Duration::from_millis(330);
/// How long after a Replace a warden's cell that reads as before is taken as
/// the new inmate when the Iron Pestle is held: it keeps the replaced
/// inmate's beast, so a Normal Mammoth replaced by another looked unchanged,
/// and the reads were held REPLACE_SHOWS while the player clicked on
/// (2026-11-01, twice in a run). The new one is on show by about 0.45 s.
const REPLACE_LOOKALIKE: std::time::Duration = std::time::Duration::from_millis(550);
/// How long the decision cards fade in after the game's log says they came
/// up, see [`Session::take_read`].
pub const CARDS_IN: std::time::Duration = std::time::Duration::from_millis(600);
/// How long after the decision cards come up the board under them reads
/// right, see [`Session::cards_shown`].
const CARDS_BOARD: std::time::Duration = std::time::Duration::from_millis(100);
/// How long a patrol read with fewer than three destination cards waits for
/// the rest, see [`Session::take_read`]. Two cards in the centred two-card positions do not wait (see
/// [`two_card_row`]): that is already a full row.
const DOORS_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);
/// How long past [`DOORS_SETTLE`] the read that settles a short patrol comes.
const DOORS_MARGIN: std::time::Duration = std::time::Duration::from_millis(20);
/// The line shown over the last reading while the decision cards fade in.
const CARDS_FADING: &str = "The decision cards are still coming in: showing the last reading.";
/// A pick read with fewer than its 3 offers: the cards come in one by one
/// after a Re-roll, and a read of one of 3 advised it and marked it 0.2 s
/// before the full offer gave another (2026-10-30). The same short offer
/// read for [`OFFER_STEADY`] stands, so an offer the reader keeps missing
/// is still advised.
const OFFER_PARTIAL: &str = "The offer is still coming in: showing the last reading.";
/// How long a short offer has to stay the same to stand: the cards took
/// 0.8 s to come in, and two reads 0.2 s apart saw the same 2 of 3, which
/// then stood and advised a Re-roll on them (2026-10-30).
const OFFER_STEADY: std::time::Duration = std::time::Duration::from_millis(1000);
/// How long after a Re-roll the offer it replaced is taken for its cards
/// turning: the new offer read 0.87 s after one on 2026-10-31.
const REROLL_SHOWS: std::time::Duration = std::time::Duration::from_millis(1500);
/// How far below the best move, in Chromatics' worth, a Cage Fight chain's
/// planned bite may rate and still be advised (see `Session::keep_to_chain`):
/// its chains' near ties were 0.01 to 0.06 apart (2026-10-31).
const CAGE_PLAN_SLACK: f64 = 0.15;
/// Below this confidence a cell read makes a patrol's or manage screen's
/// first read wait for one more (see `Session::take_read`). Between 0.2 and
/// 0.4 the advice still gave way to another within the screen in 3 of 132
/// first reads, and in 11 of 3,055 above (2026-10-31 to 11-02): a manage
/// screen read as its cards rose took a door's glow for Magic (0.36), and
/// "Replace the Mammoth" became "Consume the Wolf" 0.18 s on.
const FIRST_DOUBT: f32 = 0.4;
/// The line logged for a changed board read held for one more read, see [`Session::install`].
const CHANGED: &str = "The board changed: reading it again to confirm.";

/// Cells in which a board read with a hidden counter on the run's last day
/// must differ from the board on show to be taken as a screen after the run
/// (the riot screen missed). In the cloud run logs such reads differed from
/// the last day's board in 6 to 14 of 16 cells; 43 of about 16,000 same-day
/// read pairs in normal play differed in 6 or more.
const AFTER_RUN_CELLS: usize = 6;

/// Reads in a row without the board (see [`Panels::off_board`]) on the run's
/// last day that end the run: the riot screen can be gone before it reads
/// (on 2026-10-28 a player clicked past it 1.2 s after it opened, while its
/// title still faded in, and the run ended 15 s later on the riot fight). The
/// riot fight that follows has no board either; two in a row let a lone
/// frame of some animation pass.
const LAST_DAY_OFF_READS: usize = 2;

/// What the player sees of a cell for [`AFTER_RUN_CELLS`]: locked or not,
/// the beast and the tier.
fn cell_sight(cell: &CellModel) -> (bool, Option<Beast>, Option<i64>) {
    match cell {
        CellModel::Sealed { .. } => (true, None, None),
        CellModel::Inmate { beast, tier, .. } | CellModel::Scouted { beast, tier, .. } => (false, Some(*beast), Some(*tier)),
        CellModel::Scratch { tier, .. } => (false, None, Some(*tier)),
        _ => (false, None, None),
    }
}

/// How long after the game's log tells of a new day its door cards read.
pub const DOORS_SHOW: std::time::Duration = std::time::Duration::from_millis(200);
/// How long after a day's start an instrument acting then is the day's
/// (see [`Signal::Instrument`]), and how long after it the board shows its
/// effect: the Wriggling Iron Box's Rat was in by reads begun 0.3 s on, not
/// by those begun 0.1 s on (2026-11-01, 9 days of a run).
const DAY_EFFECT: std::time::Duration = std::time::Duration::from_millis(150);
const EFFECT_SHOWS: std::time::Duration = std::time::Duration::from_millis(350);
/// How long after the game's log tells of a Consume pressed its target screen
/// reads: reads begun 0.23 s on had it, those before read the manage screen
/// (2026-10-30, 41 Consumes; the log line comes ~0.03 s late).
pub const CONSUME_SHOWS: std::time::Duration = std::time::Duration::from_millis(220);
/// How long after the game's log tells of a bite the board shows it: the
/// first read taken began 0.35 to 0.52 s on, a median 0.60 s to its end, as
/// reads begun before were held (2026-10-30, 15 bites a chain went on from).
pub const BITE_READABLE: std::time::Duration = std::time::Duration::from_millis(370);

/// Playouts per option before the playouts can overrule the strategy's pick.
const LIVE_MIN: usize = 150;
/// Playouts per option before the advice shows, with its mark, before the
/// solve ends: in 143 solves of two 2026-10-30 runs, the advice at 150
/// changed later 17 times, at 500 four times (half the solves end by 900).
/// The mark waited for the solve's end, 0.11 to 0.17 s after it began and
/// up to 0.4 s.
const SHOW_MIN: usize = 500;
/// Playouts per option a slow solve stops at once it has run [`SLOW_SOLVE`]:
/// a Cage Fight board with Avenger's Casket took about 1 s to its 1600
/// (0.1 to 0.3 s in other runs), as its chains run long. On 120 Cage Fight
/// boards against 6400-playout solves, stopping at 1000 lost 0.0027
/// Chromatics a decision more than 1600, at 500 0.0034 (2026-11-01; the
/// user chose 1000 for the time: about 0.65 s for 1 s).
const ENOUGH: usize = 1000;
/// How long a solve runs before it stops at [`ENOUGH`] playouts.
const SLOW_SOLVE: std::time::Duration = std::time::Duration::from_millis(400);


/// A cell whose inmate read lower than the same beast on the board: the cell's index, the beast and the tier read.
type TierDrop = (usize, Beast, i64);

/// An eater taken from the jaws (see `eater_from_jaws`): its cell, beast and rarity, the jaws it was taken from, and
/// when its read was made. See `Session::inferred_from`.
type Inference = ((usize, Beast, i64), Vec<usize>, SystemTime);

/// The cells of `read` whose inmate is the same beast as on `board` at a lower tier; other changes (a cell still in
/// its animation, a misread of another beast) are not drops and do not count.
fn dropped_cells(read: &Model, board: &Model) -> Vec<TierDrop> {
    read.cells
        .iter()
        .zip(&board.cells)
        .enumerate()
        .filter_map(|(n, (now, before))| match (now, before) {
            (CellModel::Inmate { beast: a, tier: t, .. }, CellModel::Inmate { beast: b, tier: u, .. }) if a == b && t < u => Some((n, *a, *t)),
            _ => None,
        })
        .collect()
}

/// Whether a live read showing `drops` (see [`dropped_cells`]) is held for one more read. `held` is the drops of
/// the read held just before it, if one was (empty when it was held for another reason), and `changed` whether a
/// Bite, Consume or Replace was told of since the last read taken. A drop waits for at most one read: a read that
/// follows a held read already showing a drop is taken, whatever its drops and whatever other cells did, so the read
/// confirming a drop is never held again (it was, and a real drop waited 2 reads with the old advice on show).
fn holds_drop(drops: &[TierDrop], held: Option<&[TierDrop]>, changed: bool) -> bool {
    !changed && !drops.is_empty() && held.is_none_or(|seen| seen.is_empty())
}

/// The instruments' keys, as the settings keep them.
fn tool_keys(tools: &[Tool]) -> Vec<&'static str> {
    tools.iter().map(|t| t.key()).collect()
}

/// What the reader saw in a frame, the frame's size, the frame itself
/// when the run log keeps it, and the interface scale it was read at.
type Scanned = (Scene, Panels, (usize, usize), Option<Rgba>, f32);

/// What [`Session::take_read`] did with a read.
enum Taken {
    /// The reading on show again, unchanged.
    Same,
    /// A reading was installed.
    Installed,
    /// A theme scroll, a popup, a screen without the board or a read between
    /// runs: nothing was installed; the line says so.
    Kept(String),
    /// Nothing changed, or the riot screen ended the run.
    Skipped,
}

/// Reads the frame at the reference size the reader's regions are measured
/// on, as `load_png` does, at the interface scale measured near `hint`; see
/// [`shape_line`] for the shapes refused. `keep` gives the frame back at its
/// own size.
fn scan(frame: Rgba, keep: bool, hint: Option<f32>) -> Scanned {
    let size = (frame.w, frame.h);
    let (f, title, u, found) = lay_out_found(&frame, hint);
    let (scene, mut panels) = (read_scene(&f), read_panels_titled(&f, title.as_ref().unwrap_or(&f)));
    // Patrol door cards: a door number that cannot be reached or breaks the near-to-far order is read again or pushed
    // back (see `check_doors`).
    if let Some(warden) = scene.warden {
        check_doors(&f, &mut panels, warden);
    }
    // Without the prison's top bar no board or pick is on screen, whatever
    // reads as one: the hideout's and the lobby's red read as a Consume
    // target pick (2026-10-28), and were solved.
    if !found && matches!(panels.screen, Screen::Patrol | Screen::Manage | Screen::ChainTarget | Screen::CellPick | Screen::Interrogation | Screen::InstrumentPick | Screen::TreasurePick) {
        panels.screen = Screen::Unknown;
        panels.days = None;
    }
    (scene, panels, size, keep.then_some(frame), u)
}

/// How the line for a game window narrower than 16:9 starts.
const SHAPE: &str = "The game window is ";

/// A window as wide as 16:9 or wider is read: [`crate::reader::layout::lay_out_scaled`] lays it out as
/// the reference frame. A window narrower than 16:9 (by more than 1%) has
/// never been seen, so how the game lays it out is untested: such a frame is
/// not read.
fn shape_line((w, h): (usize, usize)) -> Option<String> {
    let narrow = w * 9 < h * 16 && !is_16_9(w, h);
    narrow.then(|| trf!("The game window is {} x {}, narrower than 16:9: that layout is untested and isn't read. Set the game to a 16:9 or wider size: fullscreen or borderless, or a window such as 1920 x 1080 that is not maximized.", w, h))
}

/// A read's interface scale as its log line gives it ("u"): to 3 decimals.
fn logged_scale(u: f32) -> f64 {
    (f64::from(u) * 1000.0).round() / 1000.0
}

/// Adds to a read's log fields why auto read took the frame ("trigger"), how
/// long after the screen first moved ("moved_ms") and how long the scan took
/// ("scan_ms").
fn timing(fields: &mut serde_json::Value, trigger: Option<Trigger>, scan_ms: Option<u64>) {
    if let Some(t) = trigger {
        fields["trigger"] = json!(t.reason);
        fields["moved_ms"] = json!(t.moved_ms);
        fields["steps"] = json!(t.steps);
        fields["drift"] = json!(t.drift);
        if t.ahead {
            fields["ahead"] = json!(true);
        }
    }
    if let Some(ms) = scan_ms {
        fields["scan_ms"] = json!(ms);
    }
}

/// A block changes when its mean gray moves by more than this.
const BLOCK_DIFF: u8 = 16;
/// Two fingerprints differ when at least this many blocks changed.
const CHANGED_BLOCKS: usize = 4;

pub use crate::reader::fingerprint;
use crate::reader::PRINT;

/// True when two fingerprints show different screens: other sizes, or at
/// least [`CHANGED_BLOCKS`] blocks whose gray moved by more than [`BLOCK_DIFF`].
/// A hover glow on one cell may count, and so may an animation: a re-read of
/// the same scene keeps the solve.
pub fn differs(a: &[u8], b: &[u8]) -> bool {
    a.len() != b.len() || changed(a, b) >= CHANGED_BLOCKS
}

/// The blocks whose gray moved by more than [`BLOCK_DIFF`] between two prints of one size.
fn changed(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > BLOCK_DIFF).count()
}

/// A sample holds when at most this many blocks moved since the one before;
/// a read also needs it since the one [`SETTLE`] samples before. Measured at 4 samples a second
/// on a 2026-10-17 recording: the Interrogation Room's animation moves 7 to 13
/// blocks a sample, a screen change 40 to 280. At 10 samples a second a slow
/// fade can move fewer blocks than that between two samples, but not over
/// the [`SETTLE`] samples before.
pub const HELD_BLOCKS: usize = 16;

/// Samples in a row that have to hold before auto read reads: 3 at one sample
/// about every 75 ms (60 ms apart plus the grab) waits about 0.23 s for
/// transitions to settle.
pub const SETTLE: usize = 3;

/// Samples in a row a screen may differ from the last one read without
/// settling before it is read anyway, about 1.2 s at one sample every 75 ms:
/// on the last day the frame's edges pulse by up to 90 blocks a sample and
/// never hold, and the sparkles of a treasure or instrument pick kept 59
/// reads of a 2026-10-28 run waiting 1.7 s.
pub const MAX_WAIT: usize = 16;

/// Blocks a sample may move from the one before, and from the one [`SETTLE`]
/// samples before, on a screen whose animation never holds but never goes
/// anywhere either: the sparkles of a treasure or instrument pick. Such a
/// screen is read after [`ANIMATED_SAMPLES`] samples instead of [`MAX_WAIT`]
/// (a 2026-10-28 run waited 2.9 to 3.8 s for treasure picks). A screen
/// change moves 40 to 280 blocks a sample, and the last day's pulsing edges
/// up to 90.
pub const ANIMATED_BLOCKS: usize = 40;

/// Samples in a row within [`ANIMATED_BLOCKS`] of the one before, two of
/// them past [`HELD_BLOCKS`], before such a screen is read: about 0.6 s.
pub const ANIMATED_SAMPLES: usize = 8;

/// Samples since the last read after which a screen that moved fewer than
/// [`CHANGED_BLOCKS`] blocks from it, but at least one, is read again: 5 s at
/// about 13 samples a second. A Prison Incident "!" that appears on a cell moves only
/// 2 blocks (measured on a player's screenshot), and a hover glow that comes
/// and goes costs at most one read per 5 s.
pub const RECHECK: usize = 66;

/// Off-board reads in a row that showed nothing new (the hideout, a map)
/// after which a screen change under [`BIG_CHANGE`] blocks waits
/// [`IDLE_GAP`] samples since the last read: off the board an animation
/// that never settles was read every 2 s for minutes (2026-10-28 runs: 279
/// such reads in 521 s). On the board, prints of reads that showed nothing
/// new count as no change instead (see [`ALIKE`]), so nothing waits there.
pub const IDLE_READS: usize = 2;

/// Off-board reads in a row that end observe mode's frames: a screen in the
/// prison without the board (an animation, a pick) is one or two.
const OBSERVE_OFF: usize = 3;

/// Prints of reads that showed nothing new kept to compare samples with: a
/// sample within [`CHANGED_BLOCKS`] of one is no change. An animation that
/// never settles for long (the cell pick's pulsing brackets, the
/// Interrogation Room) was read about 3 times a second for nothing (200 of
/// 231 reads alike on one cell pick); once each of its phases was read, it
/// is read no more, while a move of the player's, unlike any, still is.
pub const ALIKE: usize = 8;

/// Why auto read read a sample, and how long after the screen first moved
/// from the last read, for the run log.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Trigger {
    /// "settled", "animated", "treasure" (a treasure pick's cards in place), "max wait",
    /// "confirm" (a held read's next sample) or "recheck".
    pub reason: &'static str,
    /// Milliseconds from the first sample that moved to the read; none for a confirm.
    pub moved_ms: Option<u64>,
    /// A frame read ahead holds against this one: that read stands for it, see [`Watch::take_ahead`].
    pub ahead: bool,
    /// Blocks each of the last samples moved from the one before, oldest
    /// first (`u16::MAX` for another size), and from the one [`SETTLE`]
    /// samples before: what the thresholds are tuned on.
    pub steps: [u16; 4],
    pub drift: u16,
    /// A pick's Confirm button showed lit on a sample since the last read
    /// (see [`crate::reader::scene::confirm_lit_raw`]): a pick was made, though
    /// no read saw it.
    pub lit: bool,
    /// The last read was a treasure pick and a sample since showed its cards
    /// gone: the pick closed (to the next treasure, or View Prison Status),
    /// not just a card picked on it.
    pub left: bool,
}

/// Blocks a sample must move from the last read to be read at once after
/// [`IDLE_READS`] idle reads: a screen change moves 40 to 280 (see [`HELD_BLOCKS`]).
pub const BIG_CHANGE: usize = 40;

/// Blocks a sample must move from the last read to be read at once after
/// twice [`IDLE_READS`] idle reads, a quarter of the print: in the hideout
/// the crowd and the camera move 40 blocks and more from one read to the
/// next, which kept a read every 1.1 to 2 s; a loading screen or the prison
/// board replaces most of the screen.
pub const WAKE_CHANGE: usize = PRINT.0 * PRINT.1 / 4;

/// Samples since the last read a smaller change waits after idle reads:
/// 7.5 s, and 15 s from twice [`IDLE_READS`] idle reads on, at the
/// watcher's resting pace (see [`Watch::resting`]).
pub const IDLE_GAP: [usize; 2] = [15, 30];

/// Auto read's decision, one fingerprint per sample of the game: read a screen
/// once it differs from the last one read and has settled, or has differed
/// for longer than [`MAX_WAIT`] samples; read a screen that moved less than
/// that, but by at least one block, once [`RECHECK`] samples passed since the
/// last read.
#[derive(Default)]
pub struct Watch {
    /// The print of the last screen read; none before the first.
    read: Option<Vec<u8>>,
    /// The prints of the last [`SETTLE`] samples, oldest first.
    recent: std::collections::VecDeque<Vec<u8>>,
    /// Samples in a row that held.
    stable: usize,
    /// Samples in a row that differed from the last screen read.
    waited: usize,
    /// Samples since the last screen read.
    since: usize,
    /// The next sample counts as read: a screenshot is on show.
    adopt: bool,
    /// The next sample is read whatever it shows: a held read asks for one more.
    reread: bool,
    /// Off-board reads in a row that showed nothing new; see [`IDLE_READS`].
    idle_off: usize,
    /// Off-board reads in a row, new or not, since a read of the board (None
    /// before one); see [`Watch::observing`].
    board_off: Option<usize>,
    /// Prints of reads that showed nothing new since the last read that did; see [`ALIKE`].
    alike: Vec<Vec<u8>>,
    /// When a sample first moved from the last read.
    moved_at: Option<std::time::Instant>,
    /// Why the last read was taken; see [`Watch::take_trigger`].
    fired: Option<Trigger>,
    /// Blocks the last samples moved from the one before, oldest first.
    steps: std::collections::VecDeque<usize>,
    /// Samples in a row within [`ANIMATED_BLOCKS`] of the one before.
    loose: usize,
    /// Blocks the last sample moved from the one [`SETTLE`] samples before.
    drift: usize,
    /// The print of the frame read ahead while the screen settles, see [`Watch::take_ahead`].
    ahead: Option<Vec<u8>>,
    /// The last sample is to be read ahead.
    send_ahead: bool,
    /// The screen moved by [`BIG_CHANGE`] blocks or more from the last read
    /// and has not been read since: the watcher samples at full pace.
    big_move: bool,    /// A sample since the last read showed a pick's Confirm lit, see [`Watch::note_lit`].
    lit_seen: bool,
    /// The last screen read was a treasure pick (see [`crate::reader::treasure_shown`]).
    read_treasure: bool,
    /// A sample since that read showed the pick's cards gone, see [`Trigger::left`].
    left_treasure: bool,
}

impl Watch {
    /// Takes a sample's print; true when it should be read, which records it as read.
    pub fn sample(&mut self, print: Vec<u8>) -> bool {
        let moved = |p: &Vec<u8>| if p.len() == print.len() { changed(p, &print) } else { usize::MAX };
        let step = self.recent.back().map_or(usize::MAX, moved);
        self.stable = if step <= HELD_BLOCKS { self.stable + 1 } else { 0 };
        self.loose = if step <= ANIMATED_BLOCKS { self.loose + 1 } else { 0 };
        if self.steps.len() == ANIMATED_SAMPLES {
            self.steps.pop_front();
        }
        self.steps.push_back(step);
        // A slow fade holds from one sample to the next but not across the samples since SETTLE ago.
        self.drift = self.recent.front().map_or(0, moved);
        let drifted = self.drift > HELD_BLOCKS;
        if self.recent.len() == SETTLE {
            self.recent.pop_front();
        }
        self.recent.push_back(print.clone());
        if self.read_treasure && !crate::reader::treasure_shown(&print) {
            self.left_treasure = true;
        }
        // A frame read ahead stands only while the screen shows it still.
        if self.ahead.as_ref().is_some_and(|a| moved(a) >= CHANGED_BLOCKS) {
            self.ahead = None;
        }
        if std::mem::take(&mut self.adopt) {
            self.mark_read(print);
            return false;
        }
        if std::mem::take(&mut self.reread) {
            return self.fire("confirm", print);
        }
        self.since += 1;
        let n = self.read.as_ref().filter(|r| r.len() == print.len()).map(|r| changed(r, &print));
        // A phase of an animation read before, with nothing new, is no change.
        let n = n.map(|n| self.alike.iter().filter(|a| a.len() == print.len()).map(|a| changed(a, &print)).fold(n, usize::min));
        // Off the board after twice IDLE_READS idle reads only a new scene wakes the watcher.
        let deep = self.idle_off >= 2 * IDLE_READS;
        let wake = if deep { WAKE_CHANGE } else { BIG_CHANGE };
        // In deep rest only a new scene that holds wakes it: in a map the
        // fight and the camera move 600 to 1,000 blocks every sample, which
        // kept the full pace and a read every 1.7 s (486 reads in 20 min).
        self.big_move = n.is_none_or(|n| n >= wake) && (!deep || self.stable >= 1);
        if let Some(n) = n.filter(|&n| n < CHANGED_BLOCKS) {
            self.waited = 0;
            self.moved_at = None;
            self.ahead = None;
            if n > 0 && self.since >= RECHECK {
                return self.fire("recheck", print);
            }
            return false;
        }
        self.moved_at.get_or_insert_with(std::time::Instant::now);
        self.waited += 1;
        // Off the board, after reads that showed nothing new, a small change waits.
        if self.idle_off >= IDLE_READS && n.is_some_and(|n| n < wake) && self.since < IDLE_GAP[usize::from(deep)] {
            return false;
        }
        if self.stable >= SETTLE && !drifted {
            return self.fire("settled", print);
        }
        // A treasure pick's cards in place: its gifts never change, so it is
        // read at once, not once its sparkles settle (the first read came 1.2 s
        // after the pick opened, 23 times out of 23 on 2026-10-28).
        if !self.read_treasure && crate::reader::treasure_shown(&print) {
            return self.fire("treasure", print);
        }
        // Sparkles that keep flickering in place: past held, never further.
        // One move past held that then holds settles instead.
        let flickers = self.steps.iter().filter(|&&s| s > HELD_BLOCKS).count() >= 2;
        if self.waited >= ANIMATED_SAMPLES && self.loose >= ANIMATED_SAMPLES && flickers && self.drift <= ANIMATED_BLOCKS {
            return self.fire("animated", print);
        }
        // In deep rest a screen that never holds is read once per IDLE_GAP.
        if self.waited > MAX_WAIT && (!deep || self.since >= IDLE_GAP[1]) {
            return self.fire("max wait", print);
        }
        // The first sample that stands still is read ahead: if the screen
        // settles on it, its read is ready about when the settled read would
        // start. A sample that only holds can be a fade still going (a
        // rarity glow fading in moved 20, 15, 9 and 0 blocks, and its read
        // ahead missed the new rarity), so it waits for one that is still.
        if step < CHANGED_BLOCKS && self.ahead.is_none() {
            self.ahead = Some(print);
            self.send_ahead = true;
        }
        false
    }

    /// Reads `print`, noting why.
    fn fire(&mut self, reason: &'static str, print: Vec<u8>) -> bool {
        let moved_ms = self.moved_at.map(|t| t.elapsed().as_millis() as u64);
        let to_u16 = |n: usize| n.min(u16::MAX as usize) as u16;
        let mut steps = [u16::MAX; 4];
        for (slot, &s) in steps.iter_mut().rev().zip(self.steps.iter().rev()) {
            *slot = to_u16(s);
        }
        self.fired = Some(Trigger {
            reason,
            moved_ms: moved_ms.filter(|_| reason != "confirm"),
            ahead: reason == "settled" && self.ahead.is_some(),
            steps,
            drift: to_u16(self.drift),
            lit: std::mem::take(&mut self.lit_seen),
            left: self.left_treasure,
        });
        self.mark_read(print);
        true
    }

    /// Notes that the sample about to be taken shows a pick's Confirm lit;
    /// the next read's trigger says so, see [`Trigger::lit`].
    pub fn note_lit(&mut self, lit: bool) {
        self.lit_seen |= lit;
    }

    /// Off the board after reads that showed nothing new (see
    /// [`IDLE_READS`]): the watcher samples at its resting pace.
    pub fn resting(&self) -> bool {
        self.idle_off >= IDLE_READS && !self.big_move
    }

    /// Whether observe mode keeps frames: in the prison only, from its first
    /// read of the board until [`OBSERVE_OFF`] reads in a row without it.
    /// Resting alone came too late: a session started in the hideout kept
    /// 190 MB in its first 2 minutes, as its reads there were news
    /// (2026-10-30).
    pub fn observing(&self) -> bool {
        self.board_off.is_some_and(|n| n < OBSERVE_OFF) && !self.resting()
    }

    /// True once after a sample that is to be read ahead: the screen moved
    /// from the last read and this sample stands still, moving fewer than
    /// [`CHANGED_BLOCKS`] blocks from the one before. A settled read whose
    /// trigger says [`Trigger::ahead`] (the screen still shows it) stands for it.
    pub fn take_ahead(&mut self) -> bool {
        std::mem::take(&mut self.send_ahead)
    }

    /// Why the last read was taken, once.
    pub fn take_trigger(&mut self) -> Option<Trigger> {
        self.fired.take()
    }

    /// Records a screen read some other way (the read hotkey).
    pub fn mark_read(&mut self, print: Vec<u8>) {
        self.read_treasure = crate::reader::treasure_shown(&print);
        self.left_treasure = false;
        self.read = Some(print);
        self.waited = 0;
        self.since = 0;
        self.moved_at = None;
        self.ahead = None;
        self.send_ahead = false;
        self.big_move = false;
        self.lit_seen = false;
    }

    /// Leaves the rest off the board at once: the game's log says the prison
    /// opened (see [`crate::app::gamelog`]), whose entry screen never settles.
    pub fn wake(&mut self) {
        self.idle_off = 0;
    }

    /// Counts the next sample as read, so an opened screenshot stays on show
    /// until the game's screen changes.
    pub fn adopt_next(&mut self) {
        self.adopt = true;
    }

    /// Reads the next sample whatever it shows, so a held read (see
    /// [`Session::read_live`]) gets the one more read that confirms it.
    pub fn read_next(&mut self) {
        self.reread = true;
    }

    /// Takes whether the last read showed anything new, and whether it was
    /// off the board: one that did not keeps its print as alike (see
    /// [`ALIKE`]); off the board it counts toward [`IDLE_READS`].
    pub fn report(&mut self, news: bool, off_board: bool) {
        self.board_off = match self.board_off {
            _ if !off_board => Some(0),
            off => off.map(|n| n + 1),
        };
        if news {
            self.alike.clear();
            self.idle_off = 0;
            return;
        }
        if let Some(read) = self.read.clone() {
            if self.alike.len() == ALIKE {
                self.alike.remove(0);
            }
            self.alike.push(read);
        }
        self.idle_off = if off_board { self.idle_off + 1 } else { 0 };
    }
}

/// Whether the running read is of a frame read ahead (see [`Session::read_ahead`]).
#[derive(Clone, Copy, PartialEq)]
enum Ahead {
    /// The running read, if any, is no read ahead: it is installed.
    No,
    /// Kept until auto read takes it or reads another frame.
    Waiting,
}

struct Progress {
    rounds: usize,
    done: bool,
    options: Vec<SolveOption>,
}

pub struct Session {
    pub reading: Option<Reading>,
    /// Set by the riot screen that ends a run: the line the window shows in
    /// place of the board and the advice, see [`Session::install`].
    pub run_over: Option<String>,
    /// Why the last read installed nothing while no reading is on show:
    /// a theme scroll, a popup or a screen without the board.
    pub missed: Option<String>,
    pub tools: Vec<Tool>,
    /// The run's theme, an id of `data().themes`: read from the lobby's
    /// scroll or set by the player; None for a run without one.
    pub theme: Option<u32>,
    pub pick: Option<Pick>,
    /// `build_state` objections: the model can't be played, nothing is solved.
    pub problems: Vec<String>,
    pub ranking: Vec<SolveOption>,
    /// Rounds behind `ranking`.
    pub rounds: usize,
    pub solving: bool,
    /// Playouts per option for a full solve.
    pub rollouts: usize,
    /// The state the running solve plays from.
    state: Option<State>,
    /// The stored read, its days replaced by the clock's count.
    scene: Option<(Scene, Panels, SystemTime)>,
    /// The size of the last frame read, before it was scaled.
    size: (usize, usize),
    /// The interface scale the last frame was read at (see
    /// [`crate::reader::layout::ui_scale`]): the next read searches near it.
    /// A game setting, so a new run keeps it.
    ui_scale: Option<f32>,
    /// The days counter as the stored read gave it.
    days_read: Option<u32>,
    days: DaysClock,
    /// How many instruments were held before the read whose higher count
    /// waits for its second reading: a new run clears these and keeps any
    /// picked since.
    held_before_higher: usize,
    /// The instrument advised on the instrument pick screen on show or last
    /// read: held from the next screen on, see [`Session::install`].
    advised_tool: Option<Tool>,
    /// The instrument panel's read when the player last set the held list
    /// (None inside: unread or unsure); None when the player has not set it.
    /// While reads give the same panel read, the player's list stands; the
    /// session's own changes to the list (an advised pick, a new run) clear it.
    held_edit: Option<Option<Vec<Tool>>>,
    /// The player set the held list on the pick on show: nothing is inferred
    /// from it.
    pick_named: bool,
    /// The last read was kept as the board fading in, see [`Session::take_read`].
    fade_kept: bool,
    /// Reads in a row without the board on the run's last day, see [`LAST_DAY_OFF_READS`].
    last_day_off: usize,
    /// The session's lines for the stored read (a held list it changed).
    notes: Vec<String>,
    /// The session changed `tools` or `theme`, see [`Session::take_tools_changed`].
    tools_changed: bool,
    /// `theme` was set (scroll or panel) after the last board read: a new
    /// run keeps it, see [`Session::install`].
    theme_fresh: bool,
    /// The theme and its count read consecutively from the theme charm at the board's bottom left (see
    /// [`THEME_ICON_READS`]); counted only while the theme is still unknown.
    theme_icon_seen: Option<(u32, usize)>,
    /// A theme deduced mid-read (from the charm, or the Iron Law implied by the day count): the board must be solved
    /// again even though it did not change, since the theme is not in the board and is only passed in when solving.
    theme_set: bool,
    /// The picks taken from the Warden's Treasure open since its first pick
    /// screen; None when no treasure is open, see [`Session::install`].
    treasure: Option<u32>,
    /// The Warden's Treasures left in a row, the open one included: one per
    /// Chromatic Prisoner the vault took (four came at once on a 2026-10-28
    /// morning); 0 when none is open, see [`Session::install`].
    treasures: u32,
    /// The treasure pick on show had a gift picked, its Confirm lit.
    gift_picked: bool,
    /// The Chromatic Prisoners in the vault that treasures were counted for
    /// this run, see [`Session::treasures_in_a_row`].
    vault_paid: usize,
    /// The Chromatics in the vault on the run's first board this session
    /// read, their treasures taken before it (see `treasures_paid`).
    vault_base: usize,
    /// The treasure line to show on the game itself while the pick lasts, see [`Session::notice`].
    treasure_line: Option<String>,
    /// When the run-over line was set, see [`Session::notice`].
    run_over_at: Option<std::time::Instant>,
    /// The last run's check (see [`super::health`]), and its channel while
    /// it runs.
    pub health: Option<super::health::Report>,
    health_rx: Option<Receiver<super::health::Report>>,
    /// Since when `problems` has held one, see [`Session::notice`].
    problems_since: Option<std::time::Instant>,
    /// The last model of a patrol, manage or Sewer Ratway cell pick screen, corrections included:
    /// what an instrument or treasure pick covers.
    board: Option<Model>,
    /// The board since the last manage screen, with the cells and the warden of
    /// any Consume target screen read after it (earlier bites locked): what a
    /// treasure pick after a consume covers, see [`vaulted`]. None after a
    /// patrol or cell pick screen.
    consume: Option<Model>,
    /// The longest vault read this run and the day it was read: a treasure
    /// pick reading fewer keeps it, and the run end counts it. A reading on
    /// an earlier day is a new run's.
    vault_top: Option<(i64, Vec<Beast>)>,
    /// The day each Prey mark on the board was first read, by the prey's
    /// cell: the reader reads every mark at its full rounds, and a mark loses
    /// one each day after. A reading on an earlier day is a new run's.
    prey_seen: Vec<(usize, i64)>,
    /// A Consume target screen read since the last manage screen showed a
    /// bite: the target screen is a chain's (see [`bitten`]).
    chained: bool,
    /// A Consume target screen was read since the last manage screen: a pick
    /// now follows a Consume.
    consuming: bool,
    /// When a Consume target screen last showed a bite since the manage screen.
    bite_seen: Option<std::time::Instant>,
    /// The bites the game's log counted this run, and how many of them the
    /// last read taken knew of; None until the log says one: a session
    /// without the log goes by the reads alone. See [`Session::note_game`].
    log_bites: Option<(u32, u32)>,
    /// The log's bite count when the player last pressed Consume: a target
    /// pick ended with no bite since leaves the day's Consume unspent.
    bites_at_consume: Option<u32>,
    /// When the game's log last counted a bite, see [`BITE_SHOWS`].
    bite_at: Option<std::time::Instant>,
    /// The reading on show, its targets differing from the game's marks, was
    /// read again the same at least [`DIFFER_STEADY`] after the last bite.
    differ_steady: bool,
    /// When the game's log said the decision cards came up, until a door or
    /// a decision is clicked, and how many came up; see [`CARDS_IN`].
    cards_up: Option<(std::time::Instant, usize)>,
    /// From when frames show what a read begun before can't: the board under
    /// the decision cards ([`CARDS_BOARD`] after they came up), a new day's
    /// doors, the target screen after a Consume ([`CONSUME_SHOWS`]) or the
    /// board after a bite ([`BITE_READABLE`]); see [`Session::scan_stale`].
    fresh_at: Option<std::time::Instant>,
    /// The game's log said Chain Reaction fired, and no door was clicked
    /// since: the next move opens every unopened cell on its path.
    chain_armed: bool,
    /// The warden's cell (model index) when the game's log last said a
    /// door was clicked, see [`Session::cards_shown`].
    moved_from: Option<i64>,
    /// The cards of a patrol read with fewer than three, and since when, see
    /// [`DOORS_SETTLE`].
    short_doors: Option<(Vec<u8>, std::time::Instant)>,
    /// The settled advice last shown, with its reading's screen and day,
    /// until a decision or a bite: see [`Session::shown_advice`].
    held: Option<(HeldKey, Advice)>,
    /// The running solve's advice held from one progress to the next past
    /// [`SHOW_MIN`], and its best move at the last progress: see
    /// [`Session::settled`].
    advice_steady: bool,
    last_best: Option<Action>,
    /// The decision buttons read since the cards came up, until a decision
    /// is clicked, with their spots: none leaves before a click.
    cards_seen: Vec<(Spot, Layout, (i32, i32))>,
    /// The instruments of the last pick read short of its 3, and since when,
    /// see [`OFFER_PARTIAL`].
    short_offer: Option<(Vec<Tool>, std::time::Instant)>,
    /// A popup or a pick was read since the last patrol or manage screen,
    /// see the board change hold in [`Session::take_read`].
    overlay_seen: bool,
    /// The last instrument offer read in full, its cards' spots and the
    /// instruments held then, until a Re-roll: a card the player clicked
    /// reads dim and its name went unread.
    full_offer: Option<(Vec<Tool>, Vec<SpotAt>, Vec<Tool>)>,
    /// The eater's cell and inmate the jaws called for after the last bite,
    /// see [`Session::solve`]; it stands until the read shows it risen.
    inferred_eater: Option<(usize, CellModel)>,
    /// The eater's cell, beast and rarity a chain's target screen read since
    /// the last bite, with targets the game marks: an eater taken from the
    /// jaws as it is shows its mark (see `Reading::eater_inferred`). A Rare
    /// Lion read, a frame of its portrait still changing after took it so
    /// again, and the mark on 16 went for 0.43 s until the bite (2026-11-02, day 7). An eater taken from the jaws
    /// steadily counts as seen too (see `inferred_from`).
    eater_seen: Option<(usize, Beast, i64)>,
    /// Diagnostics: every condition of the hold check (`unshown`) on the target screen after a devour, carried out by
    /// the next read or miss log record. On 2026-11-02 day 11, a read 0.49 s after the devour still showed the eater
    /// as it was before the bite but it was not held, and screenshots could not reproduce it, so the actual log is
    /// needed to find which condition did not hold.
    unshown_why: Option<serde_json::Value>,
    /// When the target screen issues the warning that the game's target differs from what the board allows, the jaws
    /// (model index) the game marked; an empty list when a chain's target screen read with no jaws ended the chain
    /// (see `solve`); None otherwise. See `jaws_moved` in `take_read`.
    differ_jaws: Option<Vec<usize>>,
    /// The jaws (model indices) of the read before the one being taken, among those that got as far as the
    /// same-reading check in [`Session::take_read`]; see `jaws_moved`.
    jaws_last: Option<Vec<usize>>,
    /// The eater (cell, beast, rarity) the last solve took from the jaws (see `eater_from_jaws`), those jaws, and
    /// when its read was made; None when the last solve took no eater from the jaws. The same eater taken from the
    /// same jaws on the next read solved, past [`DIFFER_STEADY`] after the bite, is steady: it counts as seen (see
    /// `eater_seen`), and its mark shows.
    inferred_from: Option<Inference>,
    /// The cells of the board when the latest bite (the game log's Bite) was taken during a chain: the door the
    /// Avenger's Casket opened one bite earlier is a cell in this board that already holds an inmate (see the
    /// Avenger's Casket check of ChainTarget). None when there is no log.
    bite_cells: Option<Vec<CellModel>>,
    /// The targets one of which the last bite ate, when the read taken after it could not tell which (see
    /// `meal_read`) and `meal` is None; empty otherwise.
    meal_doubt: Vec<usize>,
    /// The chain's next bites, or the day's first, the last solve's board allows before they are narrowed to the
    /// jaws read (see `consume_targets`); None when it solved no target screen. See [`Session::offered`].
    offered_wide: Option<Vec<usize>>,
    /// The manage screen's advice to Consume, shown on its target screen
    /// while that solves, see [`Session::shown_advice`]; until a bite.
    manage_bite: Option<Advice>,
    /// The last Consume target read showed the eater's cell locked after an
    /// Exalted eater's bite: it went to the vault, see [`Session::solve`].
    eater_vaulted: bool,
    /// The full offer a Re-roll replaced and when, see [`REROLL_SHOWS`].
    rerolled: Option<(Vec<Tool>, std::time::Instant)>,
    /// The last read taken came after a bite the game's log counted: its
    /// Consume target screen is a chain's, whatever its cells show.
    bite_logged: bool,
    /// The game's log said the run's objective completed.
    objective_done: bool,
    /// The day the game's log last said Consume was pressed on.
    consumed_on: Option<u32>,
    /// When the game's log said the player clicked a door or a decision
    /// card, until a read begun since installs: the move on show is played.
    played_at: Option<std::time::Instant>,
    /// A gift was confirmed since a treasure's pick last opened, as the
    /// game's log says: the next to open is a new treasure in a row (or the
    /// same treasure's next pick).
    gift_unanswered: bool,
    /// When the game's log last said a treasure's pick opened, and when the
    /// read of the reading on show began: see [`Session::note_shown`].
    pick_opened: Option<std::time::Instant>,
    reading_scan: Option<std::time::Instant>,
    /// The gifts confirmed since the board was last seen, as advised: an
    /// Opulence treasure's three picks show no board in between, and each
    /// pick was solved on the board from before the first. The same Brand
    /// of Original Sin was advised five times running, its inmates still
    /// without a target on that board (2026-11-01, day 19).
    unseen_gifts: Vec<Gift>,
    /// The Chromatics the game's log saw fly into the vault since
    /// `gifts_taken` was last reset (see [`Signal::Vaulted`]).
    vault_flies: usize,
    /// The number of cells lit in the vault over the same period ([`Signal::VaultLit`]), see [`Session::vaulted_seen`].
    vault_lights: usize,
    /// When the game's log last said a day began, and until when an
    /// instrument acting then keeps the board from showing its effect (see
    /// [`EFFECT_SHOWS`]).
    day_at: Option<std::time::Instant>,
    effect_until: Option<std::time::Instant>,
    /// The game's log said a new treasure's pick opened right after a
    /// gift, not yet taken by a read; see [`Session::note_game`].
    treasure_reopened: bool,
    /// What the pick on show came after, as the day was seen when it opened
    /// (see [`Session::pick_after`]); kept for the picks in a row and View
    /// Prison Status under them, None off a pick.
    pick_after: Option<PickAfter>,
    /// The board under the pick on show was read on the way to it (see
    /// [`Session::pick_opens`]): no line asks for View Prison Status.
    pick_seen: bool,
    /// The board last read between screens, its counter on show and no
    /// decision (a day's end, a Replace's or an incident's effect), since the
    /// last board screen.
    glimpse: Option<Model>,
    /// While the board between a treasure's picks shows (a gift's effect),
    /// whether the pick stays on show with picks left; None once another
    /// screen reads. See [`Session::install`].
    gift_gap: Option<bool>,
    /// The last Usurpation stage a popup named this run, see [`Session::solve`].
    usurped: Option<u32>,
    /// The advised action and the chain it starts, see [`Session::chain_plan`].
    chain_plan: Option<(Action, Vec<usize>)>,
    /// The chain as planned when it started, its bites in order: the advice
    /// keeps to it while its next bite is about as good, see [`Session::advice`].
    chain_follow: Vec<usize>,
    /// The planned meal the last bite took, see [`Session::note_chain_plan`].
    eaten: Option<usize>,
    /// The number each badge of the chain shows, by cell: given when the
    /// chain is planned, counting the bites already taken, and kept as its
    /// bites go (see [`Session::chain_spots`]).
    chain_numbers: Vec<(usize, usize)>,
    /// The move the advice last showed once the playouts counted: the advice
    /// keeps to it while it stays about as good, see [`Session::keep_shown`].
    shown: Option<Action>,
    /// The Warden's Treasure gifts confirmed this run, as the game's log
    /// says (see [`Session::note_treasure`]).
    gifts_taken: usize,
    /// The day the game's log last said began and the run's days, see
    /// [`Session::note_day`].
    log_day: Option<(u32, u32)>,
    /// The OCR cache counts at the last read logged (see [`crate::reader::ocr::cache_counts`]).
    ocr_seen: (u64, u64),
    /// The meal picked on the last Consume target screen since the last
    /// patrol, manage or cell pick screen: what a treasure pick covers, see
    /// [`vaulted`].
    meal: Option<Meal>,
    /// Per low-confidence cell warning of the stored read (see [`steady_key`]), whether it is shown (see
    /// [`warns_again`]).
    steady: BTreeMap<String, bool>,
    /// The low-confidence cell warnings shown on the reads taken on the screen and day in `warned_on`: they stay on
    /// show while reads still give them (see [`steady_shown`]).
    warned_keys: BTreeSet<String>,
    /// The screen and riot countdown `warned_keys` were shown on; another clears them.
    warned_on: Option<(Screen, Option<u32>)>,
    edits: BTreeMap<u8, CellEdit>,
    /// Per cell, an incident marker the player added (true) or removed.
    markers: BTreeMap<u8, bool>,
    rx: Option<Receiver<Progress>>,
    /// A frame the reader is working on, off the UI thread.
    scan: Option<Receiver<Scanned>>,
    /// The newest frame that arrived while `scan` ran, and whether it came
    /// from the game; read when it ends.
    queued: Option<(Rgba, bool)>,
    /// The frame `scan` reads came from the game, see [`Session::read_live`].
    live: bool,
    /// The game's screen shows the stored read: it came from the game and no
    /// read since was kept, see [`Session::mark`].
    on_screen: bool,
    /// The last read was held as a changed board: the next one is taken
    /// whatever it shows.
    confirming: bool,
    /// Bites, Consumes and Replaces (a decision on the manage screen) the game's log has told of, and how many of
    /// them had been told when the last read was taken: a cell read lower with none in between is an unsteady read
    /// (see [`holds_drop`]).
    changers: u32,
    changers_seen: u32,
    /// The rarity drops of the last read held (empty when it was held for another reason), see [`holds_drop`]: the
    /// read after one held with a drop is taken. Like `confirming`, the next read takes it, whatever it does.
    drop_held: Option<Vec<TierDrop>>,
    /// The last read was a chain's target screen with no jaws, held for one more read (see the jawless hold in
    /// [`Session::take_read`]); true when the reading on show predates the bite, whose advice then does not show
    /// (see [`Session::shown_advice`]). Like `drop_held`, the next read takes it, whatever it does.
    jawless_held: Option<bool>,
    /// A read was held and waits for one more, see [`Session::wants_reread`].
    reread: bool,
    /// The board read shows an eater still changing, see [`Session::wants_recheck`].
    recheck: bool,
    /// A patrol read with fewer than three cards waits this much longer, see [`Session::wants_read_in`].
    read_in: Option<std::time::Duration>,
    /// Whether the last read showed anything new, and was off the board, see [`Session::take_news`].
    news: Option<(bool, bool)>,
    /// Why auto read read the frame being read, when its scan started and how long it took, for the run log.
    trigger: Option<Trigger>,
    scan_started: Option<std::time::Instant>,
    scan_ms: Option<u64>,
    /// Whether `scan` reads a frame ahead, see [`Session::read_ahead`].
    ahead: Ahead,
    /// A frame read ahead was installed and auto read has not taken it yet;
    /// what its install showed, reported once auto read takes it (see
    /// [`Session::read_triggered`]).
    ahead_shown: Option<Option<(bool, bool)>>,
    /// The line the last read kept the reading on show with, if it did.
    last_kept: Option<String>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The run log the session writes its reads, advice and corrections to; none by default.
    pub log: Option<RunLog>,
    /// Paused while playing Vorax (see [`Session::suspend`]): no screen reads, no game log,
    /// no marks, no log writes.
    suspended: bool,
}

impl Session {
    /// `days` is the days count the held `tools` were saved at: a higher
    /// count read twice is a new run and clears them.
    pub fn new(rollouts: usize, tools: Vec<Tool>, theme: Option<u32>, days: Option<u32>) -> Session {
        Session {
            reading: None,
            run_over: None,
            missed: None,
            tools,
            theme,
            pick: None,
            problems: Vec::new(),
            ranking: Vec::new(),
            rounds: 0,
            solving: false,
            rollouts: rollouts.max(1),
            state: None,
            scene: None,
            size: (REF_W, REF_H),
            ui_scale: None,
            days_read: None,
            days: DaysClock { left: days, higher: None },
            held_before_higher: 0,
            advised_tool: None,
            held_edit: None,
            pick_named: false,
            fade_kept: false,
            last_day_off: 0,
            notes: Vec::new(),
            tools_changed: false,
            theme_fresh: false,
            theme_icon_seen: None,
            theme_set: false,
            treasure: None,
            treasures: 0,
            gift_picked: false,
            vault_paid: 0,
            vault_base: 0,
            treasure_line: None,
            run_over_at: None,
            health: None,
            health_rx: None,
            problems_since: None,
            board: None,
            consume: None,
            vault_top: None,
            prey_seen: Vec::new(),
            chained: false,
            consuming: false,
            bite_seen: None,
            log_bites: None,
            bites_at_consume: None,
            bite_at: None,
            differ_steady: false,
            cards_up: None,
            fresh_at: None,
            chain_armed: false,
            moved_from: None,
            short_doors: None,
            held: None,
            advice_steady: false,
            last_best: None,
            cards_seen: Vec::new(),
            short_offer: None,
            full_offer: None,
            rerolled: None,
            inferred_eater: None,
            eater_seen: None,
            unshown_why: None,
            differ_jaws: None,
            jaws_last: None,
            inferred_from: None,
            bite_cells: None,
            meal_doubt: Vec::new(),
            offered_wide: None,
            manage_bite: None,
            eater_vaulted: false,
            overlay_seen: false,
            bite_logged: false,
            objective_done: false,
            consumed_on: None,
            played_at: None,
            gift_unanswered: false,
            pick_opened: None,
            reading_scan: None,
            unseen_gifts: Vec::new(),
            vault_flies: 0,
            vault_lights: 0,
            day_at: None,
            effect_until: None,
            treasure_reopened: false,
            pick_after: None,
            pick_seen: false,
            glimpse: None,
            gift_gap: None,
            usurped: None,
            chain_plan: None,
            chain_follow: Vec::new(),
            eaten: None,
            chain_numbers: Vec::new(),
            shown: None,
            gifts_taken: 0,
            log_day: None,
            ocr_seen: (0, 0),
            meal: None,
            steady: BTreeMap::new(),
            warned_keys: BTreeSet::new(),
            warned_on: None,
            edits: BTreeMap::new(),
            markers: BTreeMap::new(),
            rx: None,
            scan: None,
            queued: None,
            live: false,
            on_screen: false,
            confirming: false,
            changers: 0,
            changers_seen: 0,
            drop_held: None,
            jawless_held: None,
            reread: false,
            recheck: false,
            read_in: None,
            news: None,
            last_kept: None,
            trigger: None,
            scan_started: None,
            scan_ms: None,
            ahead: Ahead::No,
            ahead_shown: None,
            notify: None,
            log: None,
            suspended: false,
        }
    }

    /// True while a frame from [`Session::read_in_background`] is being read.
    pub fn scanning(&self) -> bool {
        self.scan.is_some() && self.ahead == Ahead::No
    }

    /// Called from the worker after each partial ranking (the UI asks for a repaint).
    pub fn on_progress(&mut self, notify: impl Fn() + Send + Sync + 'static) {
        self.notify = Some(Arc::new(notify));
    }

    /// Reads a frame with the reader (OCR included) on a worker thread;
    /// [`Session::poll`] takes the result (see [`Session::install`]). A frame
    /// that arrives while a read runs waits for it; only the newest waiting
    /// one is read next.
    pub fn read_in_background(&mut self, frame: Rgba) {
        self.read(frame, false);
    }

    /// [`Session::read_in_background`] for a frame captured from the game: a
    /// changed board read is held for one more read, see [`Session::install`].
    pub fn read_live(&mut self, frame: Rgba) {
        self.read(frame, true);
    }

    /// [`Session::read_live`] for a frame auto read took, with why, for the run log.
    /// A frame auto read whose trigger says [`Trigger::ahead`] is not read:
    /// the frame read ahead stands for it.
    pub fn read_triggered(&mut self, frame: Rgba, trigger: Option<Trigger>) {
        let ahead = trigger.is_some_and(|t| t.ahead);
        // The read ahead is on show already: auto read only confirms it.
        if ahead && let Some(news) = self.ahead_shown.take() {
            self.news = news;
            return;
        }
        self.trigger = trigger;
        if ahead && self.ahead == Ahead::Waiting {
            self.ahead = Ahead::No;
            return;
        }
        self.read(frame, true);
    }

    /// Reads a frame from the game that may settle into the next auto read,
    /// unless another read runs: the read is kept until auto read takes it
    /// (see [`Session::read_triggered`]) and dropped by any other read, a
    /// newer frame read ahead included.
    pub fn read_ahead(&mut self, frame: Rgba) {
        if self.suspended {
            return;
        }
        self.drop_ahead();
        if self.scan.is_some() {
            return;
        }
        self.start_scan(frame, true);
        self.ahead = Ahead::Waiting;
    }

    /// Drops a frame read ahead, its read running or done: a running one
    /// ends unheard.
    fn drop_ahead(&mut self) {
        self.ahead_shown = None;
        if self.ahead == Ahead::Waiting {
            self.scan = None;
            self.ahead = Ahead::No;
        }
    }

    fn read(&mut self, frame: Rgba, live: bool) {
        if self.suspended {
            return;
        }
        self.drop_ahead();
        if self.scan.is_some() && !self.scan_stale() {
            self.queued = Some((frame, live));
            return;
        }
        // The running read ends unheard, and the frame waiting is older.
        self.queued = None;
        self.start_scan(frame, live);
    }

    /// Whether the running read began before [`Session::fresh_at`] and now
    /// is after: its frame tells nothing a frame now won't. Waiting for it
    /// held the first read that could advise to a median 0.19 s after the
    /// cards came up, 0.22 s after the doors lit up (2026-10-30).
    fn scan_stale(&self) -> bool {
        let (Some(fresh), Some(started)) = (self.fresh_at, self.scan_started) else { return false };
        started < fresh && std::time::Instant::now() >= fresh
    }

    /// True once after a read was held: the UI reads the game again.
    pub fn wants_reread(&mut self) -> bool {
        std::mem::take(&mut self.reread)
    }

    /// True once after a solve found the eater of a chain still changing
    /// after its bite: the UI reads the game again soon, as a change that
    /// faint may not wake auto read (4 s on a dark board, 2026-10-28).
    pub fn wants_recheck(&mut self) -> bool {
        std::mem::take(&mut self.recheck)
    }

    /// Once after a patrol read with fewer than three cards waited (see
    /// [`DOORS_SETTLE`]): how long until the same cards count, when the UI
    /// reads the game again. A day that offers two doors shows no change
    /// after its cards are in, so no read came until the 5 s recheck: two-door
    /// patrols advised a median 0.76 s after the screen's last read, up to
    /// 5.4 s, three-door ones 0.33 s (2026-10-31; 15 of a run's 20 days).
    pub fn wants_read_in(&mut self) -> Option<std::time::Duration> {
        self.read_in.take()
    }

    /// Whether the last read showed anything new, and was off the board,
    /// once per read; see [`Watch::report`].
    pub fn take_news(&mut self) -> Option<(bool, bool)> {
        self.news.take()
    }

    fn start_scan(&mut self, frame: Rgba, live: bool) {
        self.scan_started = Some(std::time::Instant::now());
        let (tx, rx) = channel();
        self.scan = Some(rx);
        self.live = live;
        let (notify, keep, hint) = (self.notify.clone(), self.log.is_some(), self.ui_scale);
        std::thread::spawn(move || {
            if tx.send(scan(frame, keep, hint)).is_ok()
                && let Some(notify) = &notify
            {
                notify();
            }
        });
    }

    /// Takes a read frame. When it gives the scene on show again (the same
    /// model with the current corrections, screen and days), only the read
    /// time and warnings change: the corrections and the running solve stay.
    /// Otherwise the corrections go and a solve starts. A game dialog, a
    /// frame without the board (see [`Panels::off_board`]) or a frame of a
    /// window narrower than 16:9 (see [`shape_line`]) keeps the last
    /// reading and its solve on show, with a line that says so. The riot
    /// screen ends the run, see [`Session::end_run`]. Until the next run
    /// starts (a days counter reading at least one under the run's length,
    /// or an instrument pick with the counter hidden), a read is kept with
    /// [`RUN_OVER_WAIT`]. On the run's last day a board read with the counter
    /// hidden that differs from the board on show in [`AFTER_RUN_CELLS`]
    /// cells or more is taken as the riot screen missed and ends the run,
    /// and so are [`LAST_DAY_OFF_READS`] reads in a row without the board.
    ///
    /// The reader can't name held instruments, so they follow the advice: a
    /// new run (see [`DaysClock`]) clears them, and the instrument advised on
    /// a pick is taken as the one picked: held from the next screen that is
    /// not a dialog, unknown or View Prison Status. View Prison Status over a
    /// pick gives the board under it and keeps the pick on show.
    ///
    /// The run theme scroll sets the theme and keeps the last reading on
    /// show. A new run clears the theme unless it was set after the old
    /// run's last board read; the riot screen clears it too.
    ///
    /// A Warden's Treasure opens on its pick screen and closes on the next
    /// decision screen. The board shown after a pick, with other cells than
    /// the board under the pick, is the pick's effect: while the treasure
    /// has picks left (Opulence gives more), it becomes the board the next
    /// pick covers and the pick stays on show.
    ///
    /// A low-confidence cell read warns only when its result differs from the reading on show before it (see
    /// [`warns_again`]); once shown, it stays on show while reads on the same screen and day still give it (see
    /// [`steady_shown`]); a new run warns again.
    ///
    /// A frame from the game (see [`Session::read_live`]) whose board differs
    /// from the last read's on the same screen and day is held, with no line
    /// on show, and the next read is taken whatever it shows: a blip that
    /// shows the old board again keeps the corrections and the running solve.
    ///
    /// The run log gets the reading and, when it shows another screen or
    /// board than the last frame saved, the frame. A read that keeps the last
    /// reading (or, with none on show, sets [`Session::missed`]) is logged as
    /// a miss with its frame, saved once per screen in a row and once a
    /// minute while that screen lasts; a miss off the board or between runs
    /// saves no frame.
    fn install(&mut self, (scene, panels, size, frame, u): Scanned) {
        self.ui_scale = Some(u);
        let screen = panels.screen;
        let taken = self.take_read(scene, panels, size);
        // A read shows nothing new when it changes nothing on show: the same
        // reading, or the same kept line as the read before.
        let line = match &taken {
            Taken::Kept(line) => Some(line.clone()),
            _ => None,
        };
        let news = match &taken {
            Taken::Same => false,
            Taken::Kept(line) => self.last_kept.as_ref() != Some(line),
            Taken::Installed | Taken::Skipped => true,
        };
        // A day banner hides the counter on the board itself: it does not count as off the board.
        let off_board = matches!(&taken, Taken::Kept(line) if (is_line(line, OFF_BOARD) && screen != Screen::DayBanner) || is_line(line, RUN_OVER_WAIT));
        self.news = Some((news, off_board));
        self.last_kept = line;
        match taken {
            Taken::Installed | Taken::Same => {
                // A read begun after the move was played shows its result.
                if matches!(taken, Taken::Installed) && self.played_at.is_some_and(|at| self.scan_started.is_some_and(|t| t > at)) {
                    self.played_at = None;
                }
                if matches!(taken, Taken::Installed) {
                    self.reading_scan = self.scan_started;
                }
                self.missed = None;
                self.log_read(frame);
            }
            Taken::Kept(line) => {
                // A changed board held for one more read leaves the screen as
                // it was read, its mark too.
                if !is_line(&line, CHANGED) {
                    self.on_screen = false;
                }
                // Off the board or between runs the frame shows another game screen.
                let frame = frame.filter(|_| !is_line(&line, OFF_BOARD) && !is_line(&line, RUN_OVER_WAIT));
                self.log_miss(screen, line, frame);
            }
            Taken::Skipped => {}
        }
    }

    /// [`Session::install`] without the log.
    fn take_read(&mut self, scene: Scene, mut panels: Panels, size: (usize, usize)) -> Taken {
        let (live, confirming) = (self.live, std::mem::take(&mut self.confirming));
        // The drops of the read held just before this one go with `confirming`: only the read right after a hold may
        // use them, so a read returning early (a popup, a fade, a kept line) leaves none for a later read, whose
        // drop would pass unheld (see [`holds_drop`]).
        let drop_held = self.drop_held.take();
        // Likewise the jawless hold: only the read right after it confirms that the jaws are gone.
        let jawless_before = self.jawless_held.take().is_some();
        self.scan = None;
        self.size = size;
        // Record only the conditions of this read, not those of the previous one.
        self.unshown_why = None;
        let shape = shape_line(size);
        // The lobby's scroll names the coming run's theme and shows no days counter.
        if shape.is_none()
            && let Some(id) = panels.theme
        {
            self.tools_changed |= self.theme != Some(id);
            self.theme = Some(id);
            self.theme_fresh = true;
        }
        // The theme charm at the board's bottom left (reader::theme): when Ascendant was opened mid-run, or the
        // lobby's scroll was missed, the theme is not yet known; the same theme read THEME_ICON_READS times in a row
        // is adopted and the board is solved again. A charm that cannot be read (covered by a panel, left sidebar
        // collapsed) does not count and does not reset the count; a known theme (scroll, or the Iron Law implied by
        // the day count) is never overwritten or cleared. In 2026-11-02_20-24-01 the opening scroll was not read, and
        // the charm already recognised Tyranny.
        if self.theme.is_some() {
            self.theme_icon_seen = None;
        } else if shape.is_none()
            && matches!(panels.screen, Screen::Patrol | Screen::Manage | Screen::InstrumentPick | Screen::CellPick | Screen::Interrogation)
            && let Some(id) = scene.theme_icon.id()
        {
            let count = match self.theme_icon_seen {
                Some((seen, n)) if seen == id => n + 1,
                _ => 1,
            };
            self.theme_icon_seen = Some((id, count));
            if count >= THEME_ICON_READS {
                self.theme = Some(id);
                self.tools_changed = true;
                self.theme_set = true;
                self.theme_icon_seen = None;
            }
        }
        // A Usurpation stage's popup: the next stage joins the incident
        // pool, see [`Session::solve`].
        if let Some(id) = panels.incident {
            self.note_usurpation(id);
        }
        // The next treasure's banner ("An inmate reached Chromatic: Warden's
        // Treasure obtained") over the board between two picks shows View
        // Prison Status, as an incident's popup does, and its glow hides the
        // countdown: the board under it is the gap's (see `gift_gap`), and
        // the rooms a Three-Headed Thumb Ring opened went unread on
        // 2026-10-29. The game's log names the day.
        let over_pick = self.treasure.is_some() && self.scene.as_ref().is_some_and(|(_, over, _)| over.screen == Screen::TreasurePick);
        if panels.screen == Screen::Popup && panels.incident.is_none() && over_pick {
            panels.screen = Screen::Unknown;
            if panels.days.is_none() {
                panels.days = self.log_day.and_then(|(cur, max)| max.checked_sub(cur)).or(self.days.left);
            }
        }
        // No patrol follows a Consume the same day: the screen on its way to
        // the target pick read as one, and its advice showed a move
        // (2026-10-29). It is a screen in between.
        if panels.screen == Screen::Patrol && self.consumed_on.is_some() && self.consumed_on == self.log_day.map(|(cur, _)| cur) {
            panels.screen = Screen::Unknown;
        }
        // A patrol's cards flip in one by one: one read with a single card of
        // three advised it, and the advice moved 0.37 s later as the player
        // clicked (2026-10-30). Fewer than three wait for the rest, or for the
        // same cards to hold a while (a day can offer fewer).
        // A door the dice can't reach is a misread: the doors lie 1 to 6
        // cells clockwise of the warden. Cards flipping in read a "7" that
        // was no door, and its advice and mark showed for 0.2 s (2026-10-30;
        // 16 of 682 patrol reads, all such). It drops, and the rest wait.
        if panels.screen == Screen::Patrol
            && let Some(warden) = scene.warden
        {
            let reach = |door: u8| (1..=6).contains(&((u16::from(door) + 16 - u16::from(warden)) % 16));
            for door in &mut panels.doors {
                if door.is_some_and(|d| !reach(d)) {
                    *door = None;
                }
            }
            if let Some(r) = &self.reading {
                fill_doors(&mut panels.doors, r, panels.days, warden);
            }
        }
        // A patrol read begun before an instrument's day start effect shows
        // waits for a read after it (see `Signal::Instrument`).
        if panels.screen == Screen::Patrol
            && let (Some(until), Some(started)) = (self.effect_until, self.scan_started)
            && started < until
        {
            panels.screen = Screen::Unknown;
            self.read_in = Some(until.saturating_duration_since(std::time::Instant::now()) + DOORS_MARGIN);
        }
        if panels.screen == Screen::Patrol {
            let doors: Vec<u8> = panels.doors.iter().flatten().copied().collect();
            // Two door cards in the centred two-card positions are already a full row, so do not wait for a third: a
            // run holding the Burning Stake has two doors on all 20 days, and each day would wait about 0.4 s extra
            // (2026-11-04 13-06-39, from the new day to the advice: median 0.5 s, versus 0.09-0.27 s for runs with
            // three doors).
            let xs: Vec<i32> = panels.spots.iter().filter(|(spot, _, _)| matches!(spot, Spot::Door(_))).map(|&(_, _, (x, _))| x).collect();
            // A row altered by `check_doors` must also be read the same several times in a row to be adopted: when
            // the Warden's position is misread, the door it pushes back may not exist.
            if doors.len() == 2 && two_card_row(&xs) && !panels.doors_fixed {
                self.short_doors = None;
            } else if doors.len() < 3 || panels.doors_fixed {
                let held = match &self.short_doors {
                    Some((seen, at)) if *seen == doors => at.elapsed(),
                    _ => {
                        self.short_doors = Some((doors, std::time::Instant::now()));
                        std::time::Duration::ZERO
                    }
                };
                if held < DOORS_SETTLE {
                    panels.screen = Screen::Unknown;
                    self.read_in = Some(DOORS_SETTLE - held + DOORS_MARGIN);
                }
            } else {
                self.short_doors = None;
            }
        }
        // Nor does one follow the day's manage step at its count: the board
        // between End and the day banner read as the ended day's patrol, and
        // its advice and mark showed for 0.1 s (2026-10-30, 5 days left; the
        // next day's patrol showed 4).
        if panels.screen == Screen::Patrol
            && panels.days.is_some()
            && self.reading.as_ref().is_some_and(|r| matches!(r.screen, Screen::Manage | Screen::ChainTarget) && r.days_left == panels.days)
        {
            panels.screen = Screen::Unknown;
        }
        // A card the reader named only because the instrument icons read showed no Blood-Stained Trap (see
        // `Panels::by_icons`) is no name when the Trap is held as the session knows it (the instrument pick and the
        // game's log): an icon misread for sure would turn a Lion's Hunt, its name unread under the mouse, into
        // Replace. With a Lion on the warden's cell, such a card stays unnamed.
        let lion = |m: Option<Beast>| m == Some(Beast::Lion);
        let warden_lion = lion(scene.warden.and_then(|w| scene.cells.get(usize::from(w).checked_sub(2)?)).and_then(|c| c.inmate).map(|(b, _)| b))
            || lion(self.board.as_ref().and_then(|b| match usize::try_from(b.warden).ok().and_then(|w| b.cells.get(w)) {
                Some(CellModel::Inmate { beast, .. }) => Some(*beast),
                _ => None,
            }));
        if self.tools.contains(&Tool::Trap) && warden_lion && !panels.by_icons.is_empty() {
            let guessed = std::mem::take(&mut panels.by_icons);
            panels.decision.retain(|b| !guessed.contains(b));
            panels.spots.retain(|(spot, _, _)| !matches!(spot, Spot::Button(b) if guessed.contains(b)));
        }
        // The board fading in after a Consume target screen reads the
        // instrument panel as empty, and its cells unsure, for a frame. A run
        // loses no instrument, so such a read is kept once and the next taken.
        let fading = !self.fade_kept
            && self.reading.as_ref().is_some_and(|r| r.screen == Screen::ChainTarget)
            && !self.tools.is_empty()
            && panels.held.as_ref().is_some_and(Vec::is_empty);
        self.fade_kept = fading;
        // A target screen read just after a bite the game's log counted can
        // show the board from before it, the meal still in its cell and the
        // eater as it was: for 0.5 to 0.7 s on a 2026-10-29 run, whose advice
        // named the meal just eaten again. It is held until the bite shows
        // (see `bite_shown`), for a while: a bite on a dark board can leave
        // its cells as they were. The eater's rise flashes over its portrait
        // after the meal has gone: a read then took a Magic Wolf for a Normal
        // Mammoth, and the advice ended the chain, then went on with it
        // 0.5 s later (2026-10-30). An eater read no higher than before the
        // bite is held as well; its beast tells nothing, as a bite re-rolls it.
        let lookalike = self.tools.contains(&Tool::Pestle) && self.played_at.is_some_and(|at| at.elapsed() >= REPLACE_LOOKALIKE);
        let replace_unshown = panels.screen == Screen::Manage
            && self.played_at.is_some_and(|at| at.elapsed() < REPLACE_SHOWS)
            && !lookalike
            && !panels.decision.contains(&"Replace")
            && self.reading.as_ref().is_some_and(|r| {
                let Ok(w) = usize::try_from(r.model.warden) else { return false };
                let read = to_model(&scene, &panels, &self.tools, Some(&r.model), self.run_days()).0;
                let fading = self.played_at.zip(self.scan_started).is_some_and(|(at, scan)| scan < at + REPLACE_FADE);
                r.screen == Screen::Manage && !r.model.swap_used && (fading || (read.warden == r.model.warden && read.cells.get(w) == r.model.cells.get(w)))
            });
        // A day change not yet drawn on the board (see DAY_UNSHOWN): the countdown read from the patrol screen is one
        // day ahead of the game log's day count; after the log's day change, hold for DAY_SHOWS.
        let day_unshown = panels.screen == Screen::Patrol
            && self.log_day.and_then(|(cur, max)| max.checked_sub(cur)).is_some_and(|left| panels.days == Some(left + 1))
            && self.day_at.is_some_and(|at| at.elapsed() < DAY_SHOWS);
        let cage = self.theme == Some(CAGE_FIGHT);
        let casket = self.tools.contains(&Tool::Revenge);
        // The Crowded Beastcage opens the cell just eaten again at once, with an inmate of any rarity (see
        // `crate::engine::devour`): as with the casket, no inmate leaves the board.
        let refills = casket || self.tools.contains(&Tool::Beastcage);
        // Set when the only thing holding the read below is the refill read with its eater in doubt.
        let mut refill_unsure = false;
        // Set when the read shows the meal advised uneaten but not which target went (see `meal_read`).
        let mut meal_doubt = false;
        let unshown = panels.screen == Screen::ChainTarget
            && self.log_bites.is_some_and(|(logged, read)| logged > read)
            && self.bite_at.is_some_and(|at| at.elapsed() < BITE_SHOWS)
            && self.consume.as_ref().is_some_and(|before| {
                let (read, warns) = to_model(&scene, &panels, &self.tools, Some(before), self.run_days());
                let jaws = jawed(&scene);
                // The meal the read shows eaten, which need not be the one advised.
                let (meal, doubt) = self.meal_shown(before, &read, &warns, &jaws);
                let (meal, doubt) = (meal.map(|meal| meal.cell), !doubt.is_empty());
                // Avenger's Casket can put its new inmate in the cell just
                // eaten: no inmate leaves the board, and the bite was held
                // BITE_SHOWS while the eater had risen (2026-10-31, the
                // player bit on 1.4 s after). An eater risen shows the bite.
                // So can the Crowded Beastcage: held so, each bite it refilled waited the whole BITE_SHOWS.
                let refilled = refills && meal.is_some_and(|m| matches!(read.cells.get(m), Some(CellModel::Inmate { .. })));
                // Only an eater read for sure counts as risen: the reading takes an unsure cell's read from the board
                // before (see `rebuilt`), so it would show the eater unrisen, and the casket's new inmate, which comes
                // at the eater's rarity (see `casket_meals`), at the rarity from before the bite. On 2026-11-06 day 8
                // (02:57:55 UTC) a Normal Lion's bite read as a Magic Mammoth at confidence 0.11: the Magic Wolf the
                // casket put in cell 03 was taken as Normal, the targets differed from the game's marks for 1.1 s,
                // and the advice that then showed ended the chain on that board. 0.08 s on, the eater read Magic.
                let sure = sure_eater(before, &read, &warns, |n| !self.edits.contains_key(&(n as u8 + 1)));
                let risen = refilled && !eater_misread(before, &sure);
                let held = (!bite_shown(before, &read, meal, &jaws) && !risen) || (!cage && eater_misread(before, &read) && !meal.is_some_and(|meal| kept_from_rising(before, &read, meal, &jaws)));
                refill_unsure = held && refilled && !eater_misread(before, &read) && eater_misread(before, &sure);
                // A meal in doubt is held too, until a read tells which target went, for BITE_SHOWS at most.
                meal_doubt = doubt;
                held || doubt
            });
        // The read held for an eater in doubt is read again at once: the eater read for sure 0.08 s on. So is one
        // held for its meal in doubt.
        if refill_unsure || meal_doubt {
            self.reread = true;
        }
        // Diagnostics (see `unshown_why`): for a target screen within UNSHOWN_LOGGED after a devour, record the value
        // of each of the conditions above, plus how the eater looked before the bite and in this read.
        self.unshown_why = (panels.screen == Screen::ChainTarget && self.bite_at.is_some_and(|at| at.elapsed() < UNSHOWN_LOGGED)).then(|| {
            let before = self.consume.as_ref();
            let read = before.map(|b| to_model(&scene, &panels, &self.tools, Some(b), self.run_days()));
            let jaws = jawed(&scene);
            // As the hold check takes it: by the read only right after a bite the game's log told of.
            let bite_unread = self.log_bites.is_some_and(|(logged, read)| logged > read);
            let (meal, meal_doubt) = match (before, &read) {
                (Some(b), Some((r, warns))) if bite_unread => self.meal_shown(b, r, warns, &jaws),
                _ => (self.meal_picked(), Vec::new()),
            };
            let meal = meal.map(|meal| meal.cell);
            let read = read.map(|(r, _)| r);
            let eater = |m: &Model| usize::try_from(m.warden).ok().and_then(|n| m.cells.get(n).cloned());
            let pair = before.zip(read.as_ref());
            json!({
                "unshown": unshown,
                "bite_ms": self.bite_at.map(|at| at.elapsed().as_millis() as u64),
                "log_bites": self.log_bites,
                "consume": before.is_some(),
                "meal": meal,
                "meal_doubt": meal_doubt,
                "jaws": jaws,
                "before_eater": before.and_then(eater),
                "read_eater": read.as_ref().and_then(eater),
                "bite_shown": pair.map(|(b, r)| bite_shown(b, r, meal, &jaws)),
                "misread": pair.map(|(b, r)| eater_misread(b, r)),
                "kept_from_rising": pair.zip(meal).map(|((b, r), m)| kept_from_rising(b, r, m, &jaws)),
            })
        });
        // A Cage Fight's badges number the bites ahead, to click in a row: a
        // read from before the bite shows is the reading on show, so they
        // stay where a held read would hide them. Solved, it advised the meal
        // just eaten again, then the next (2026-10-31, "Chain 02" after
        // "Consume 02").
        // With no jaws at all, do not hold the board here: the game has ended the chain. This read is still held by
        // the BITE_UNSHOWN below, which hides the numbers, and once the board is drawn `solve` sees no jaws and
        // clears the chain (see there), after the jawless hold below where it applies.
        if unshown && cage && !jawed(&scene).is_empty() {
            return Taken::Same;
        }
        // The decision cards fade in once they come up, and a read begun in
        // the first 0.4 s took Replace's dim name as used: its advice said to
        // end the day, then to Replace (7 times in three 2026-10-29 runs),
        // and the same frame read again lost Consume instead. Such a read,
        // with fewer buttons than the log's cards, waits for one after the
        // fade. Without the count, one missing Consume or Replace waits: a
        // day with nothing to Consume then waits the fade out, and the
        // player clicked before its advice twice in a 2026-10-29 run.
        // The cards' count and Consume's red art tell the buttons before
        // their names do (see `cards_shown`): the read is the manage screen.
        // No card goes before a click: one read since the cards came up and
        // dim now is still there. The name under the mouse dims (2026-10-29:
        // a hovered Replace read as used, and the advice turned to ending the
        // day as the player clicked Replace).
        if self.cards_up.is_some() && panels.screen == Screen::Manage {
            for (spot, layout, at) in &self.cards_seen {
                if let Spot::Button(b) = spot
                    && !panels.decision.contains(b)
                {
                    panels.decision.push(b);
                    panels.spots.push((spot.clone(), *layout, *at));
                }
            }
        }
        let missing = |cards: usize| match cards {
            0 => !(panels.decision.contains(&"Replace") && panels.decision.contains(&"Consume")),
            n => panels.decision.len() < n,
        };
        let cards_window = self.cards_up.zip(self.scan_started).map(|((up, cards), scan)| (scan.saturating_duration_since(up), cards)).filter(|&(since, _)| since < CARDS_IN);
        let mut cards_fading = false;
        if let Some((since, cards)) = cards_window
            && (panels.screen == Screen::Unknown || (panels.screen == Screen::Manage && missing(cards)))
        {
            match self.cards_shown(&scene, &panels, since, cards) {
                Some(buttons) => {
                    panels.screen = Screen::Manage;
                    panels.spots = card_places(cards).iter().zip(&buttons).map(|(&k, &b)| (Spot::Button(b), Layout::Board, card_spot(k))).collect();
                    panels.decision = buttons;
                }
                None => cards_fading = panels.screen == Screen::Manage,
            }
        }
        if self.cards_up.is_some() && panels.screen == Screen::Manage && !cards_fading {
            self.cards_seen = panels.spots.iter().filter(|(spot, _, _)| matches!(spot, Spot::Button(_))).cloned().collect();
        }
        // A card the player picked reads dim: its name went unread for 20 s
        // on 2026-10-30, the 2 cards left advised a Re-roll, and the player
        // re-rolled away the best instrument. An offer doesn't shrink: a
        // short one within the last full one is that one, until a Re-roll.
        // The offer a Re-roll replaced shows again as its cards turn: read
        // 0.69 s after one on 2026-10-31, it was advised and marked for
        // 0.18 s before the new offer read. It waits for the new one, for
        // [`REROLL_SHOWS`] at most.
        let same = |a: &[Tool], b: &[Tool]| a.len() == b.len() && a.iter().all(|t| b.contains(t));
        self.rerolled = self.rerolled.take().filter(|(_, at)| at.elapsed() < REROLL_SHOWS);
        let rerolling = panels.screen == Screen::InstrumentPick && self.rerolled.as_ref().is_some_and(|(offer, _)| same(offer, &panels.instruments));
        if panels.screen == Screen::InstrumentPick && panels.instruments.len() == 3 && !rerolling {
            self.rerolled = None;
        }
        if panels.screen == Screen::InstrumentPick && !rerolling {
            let card = |s: &SpotAt| matches!(s.0, Spot::Card(_));
            if panels.instruments.len() == 3 {
                self.full_offer = Some((panels.instruments.clone(), panels.spots.iter().filter(|s| card(s)).cloned().collect(), self.tools.clone()));
            } else if let Some((offer, spots, held)) = &self.full_offer
                && *held == self.tools
                && !panels.instruments.is_empty()
                && panels.instruments.iter().all(|t| offer.contains(t))
            {
                panels.instruments = offer.clone();
                panels.spots.retain(|s| !card(s));
                panels.spots.extend(spots.iter().cloned());
            }
        }
        let short = panels.screen == Screen::InstrumentPick && (1..3).contains(&panels.instruments.len());
        let since = self.short_offer.as_ref().filter(|(offer, _)| *offer == panels.instruments).map(|&(_, at)| at);
        let offer_partial = short && since.is_none_or(|at| at.elapsed() < OFFER_STEADY);
        self.short_offer = short.then(|| (panels.instruments.clone(), since.unwrap_or_else(std::time::Instant::now)));
        // The riot screen missed: on the run's last day, screens without the
        // board in a row. Not with the game's log, which tells the run's end.
        let last_day = self.log_day.is_none() && self.reading.as_ref().is_some_and(|r| r.model.day >= r.model.max_days);
        if shape.is_none() && last_day && panels.screen == Screen::Unknown && panels.off_board() {
            self.last_day_off += 1;
            if self.last_day_off >= LAST_DAY_OFF_READS {
                self.last_day_off = 0;
                self.end_run();
                return Taken::Skipped;
            }
        } else {
            self.last_day_off = 0;
        }
        if panels.screen == Screen::Popup {
            self.overlay_seen = true;
        }
        let kept = shape.clone().or_else(|| match panels.screen {
            Screen::Theme => Some(trf!("The run's theme: {}.", theme_name(self.theme).unwrap_or_default())),
            Screen::Popup => Some(tr(POPUP).to_string()),
            _ if panels.off_board() => Some(tr(OFF_BOARD).to_string()),
            // The day is over under its banner: solved as the manage step,
            // with nothing read as spent, it advised the Replace just played
            // again (2026-10-30, day 6).
            Screen::DayBanner if self.reading.is_some() => Some(tr(OFF_BOARD).to_string()),
            _ if fading => Some(tr(FADING).to_string()),
            _ if unshown => Some(tr(BITE_UNSHOWN).to_string()),
            _ if replace_unshown => Some(tr(REPLACE_UNSHOWN).to_string()),
            _ if day_unshown => Some(tr(DAY_UNSHOWN).to_string()),
            _ if cards_fading => Some(tr(CARDS_FADING).to_string()),
            _ if offer_partial || rerolling => Some(tr(OFFER_PARTIAL).to_string()),
            // The decision cards came up since: a cell pick on screen is the
            // one just made, fading out with the warden already moved. Solved
            // again, it advised another cell for 0.07 s (2026-10-31).
            Screen::CellPick if self.cards_up.is_some() => Some(tr(FADING).to_string()),
            _ => None,
        });
        if let Some(line) = kept {
            if let Some(r) = &mut self.reading {
                r.warnings.retain(|w| !kept_line(&w.text));
                // Only a window of the wrong shape is the player's to fix.
                let kind = if shape.is_some() { WarningKind::Read } else { WarningKind::Note };
                r.warnings.insert(0, Warning { cell: None, kind, text: line.clone() });
            } else {
                self.missed = Some(match panels.screen {
                    _ if shape.is_some() || panels.screen == Screen::Theme => line.clone(),
                    Screen::Popup => tr("A popup covered the game.").to_string(),
                    _ => tr("The last read found no board or pick on screen.").to_string(),
                });
            }
            return Taken::Kept(line);
        }
        if panels.screen == Screen::RunEnd {
            self.end_run();
            return Taken::Skipped;
        }
        // Between runs only a run start is taken: the screens players stay on
        // after the riot (chest rooms and such) read as junk boards.
        let starts_run = panels.days.is_some_and(|left| i64::from(left) >= RUN_DAYS - 1)
            || (panels.screen == Screen::InstrumentPick && panels.days.is_none());
        if self.run_over.is_some() && !starts_run {
            self.missed = Some(tr(RUN_OVER_WAIT).to_string());
            return Taken::Kept(tr(RUN_OVER_WAIT).to_string());
        }
        // The riot screen missed: a board with a hidden counter on the last
        // day, far from the board on show, is a screen after the run. Not
        // with the game's log, which tells the run's end: a Cage Fight's last
        // chain read far from the board under the red tint, and ended the
        // run mid-chain, its badges gone (2026-10-31).
        if self.log_day.is_none()
            && self.days.left == Some(0)
            && panels.days.is_none()
            && matches!(panels.screen, Screen::Patrol | Screen::Manage | Screen::ChainTarget | Screen::Unknown)
            && let Some(r) = &self.reading
        {
            let model = to_model(&scene, &panels, &self.tools, None, self.run_days()).0;
            let differ = model.cells.iter().zip(&r.model.cells).filter(|(a, b)| cell_sight(a) != cell_sight(b)).count();
            if differ >= AFTER_RUN_CELLS {
                self.end_run();
                return Taken::Skipped;
            }
        }
        // One read in about ten same-screen board changes is a blip that
        // shows the old board again within a second: a live read that
        // changes the board is held until one more read confirms it.
        // The first board read after a popup or a pick may catch the board
        // still changing under it (a rise's glow, rooms a treasure opened):
        // it advised and marked one move, and the next read another 0.3 s on
        // (2026-10-30, twice in four runs). It waits for one more read too.
        let after_overlay = self.overlay_seen && matches!(panels.screen, Screen::Patrol | Screen::Manage);
        // So does a patrol's or manage screen's first read with a cell read in
        // doubt: a patrol's advice gave way to another within 0.1 s in 2 of 30
        // such patrols, and in 2 of 485 without one, the player clicking the
        // first once; a manage screen's in 1 of 32, and 0 of 659 (2026-11-01).
        let doubtful_first = matches!(panels.screen, Screen::Patrol | Screen::Manage)
            && least_sure(&scene) < FIRST_DOUBT
            && self.scene.as_ref().is_none_or(|(_, last, _)| (last.screen, last.days) != (panels.screen, panels.days));
        // The same inmate type in the same cell read lower than on the board: when a rank-up effect covers the cell,
        // the reader reads it as Normal with no drop in confidence. On 2026-11-04 day 12, at the day change, the
        // Colossal Rat in cell 9 went from Legendary to Ultimate and was covered by a red shield, so it was read as
        // Normal, and the advice first said "go to 02" and only 0.23 s later changed to the Interrogation Room. In
        // the historical logs such a drop on the patrol / manage screen happened about 50 times: 21 recovered on the
        // next frame, 29 were real drops (the inmate was swapped for a lower-rank one of the same type), and a real
        // drop only costs one extra wait. Only the first frame of a screen is looked at: later changes within the
        // same screen are already blocked by the "board changed" condition below; a cell the user manually raised
        // also costs only one extra wait.
        let drops: Vec<TierDrop> = match &self.board {
            Some(board) if matches!(panels.screen, Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::Interrogation) => {
                dropped_cells(&to_model(&scene, &panels, &self.tools, Some(board), self.run_days()).0, board)
            }
            _ => Vec::new(),
        };
        let tier_dropped = matches!(panels.screen, Screen::Patrol | Screen::Manage)
            && self.scene.as_ref().is_none_or(|(_, last, _)| (last.screen, last.days) != (panels.screen, panels.days))
            && !drops.is_empty();
        if live
            && !confirming
            && matches!(panels.screen, Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::Interrogation)
            // The first read after a pick has the counter still covered: it
            // was taken as it came, and neither wait above held it (two
            // patrols after a treasure, 2026-11-01 and 2026-11-02).
            && (after_overlay
                || doubtful_first
                || tier_dropped
                || panels.days.is_some() && self.scene.as_ref().is_some_and(|(last, last_panels, _)| {
                    (last_panels.screen, last_panels.days) == (panels.screen, panels.days)
                        && to_model(&scene, &panels, &self.tools, self.board.as_ref(), self.run_days()).0
                            != to_model(last, last_panels, &self.tools, self.board.as_ref(), self.run_days()).0
                }))
        {
            // The drops this held read shows already wait here: the confirming read is not held for them again.
            self.drop_held = Some(drops);
            self.confirming = true;
            self.reread = true;
            return Taken::Kept(tr(CHANGED).to_string());
        }
        // A cell read lower than on the board with no Bite, Consume or Replace told of since the last read taken is an
        // unsteady read, held even when it is the read confirming a change: on 2026-11-05 day 8 (13:14:27 UTC) an
        // Ultimate Colossal Rat read as Normal on the confirming read, and its 200-playout advice "End the day" showed
        // until the next read 0.18 s on turned it into "Replace". Only the dropped cells are looked at, and a drop
        // waits for one read at most (see [`holds_drop`]): the next read is taken however other cells read.
        if live && holds_drop(&drops, drop_held.as_deref(), self.changers != self.changers_seen) {
            self.drop_held = Some(drops);
            self.confirming = true;
            self.reread = true;
            return Taken::Kept(tr(CHANGED).to_string());
        }
        // A chain's target screen with no jaws ends the chain (see `solve`), but one such read can be a blip: on
        // 2026-11-05 day 15 (18:01:32 UTC), 0.57 s after a bite, with the Crowded Beastcage refilling the cell just
        // eaten, two reads showed no jaws, the chain's advice went, and the jaws on cell 04 that came 0.13 s later
        // never brought it back (the player waited 9.7 s and cancelled). In a later run a "Chain on" showed for
        // 0.08 s before one jawless read cleared it. Within BITE_SHOWS of a bite, or with an instrument that refills
        // a cell on a bite (Avenger's Casket, the Crowded Beastcage), a jawless read in a chain is held once, its
        // advice and mark kept as with a changed board; the next read is taken whatever it shows, so two jawless
        // reads in a row still end the chain. A chain a jawless read already ended (an empty `differ_jaws`, see
        // `solve`) is not held again. Only the chain is kept: the advice of a reading from before the bite (the
        // game's log told of a move played since, or, without the log, the held read shows a bite on it) would name
        // the meal just eaten, and does not show while held.
        let in_chain = self.chained || self.log_bites.is_some_and(|(logged, read)| logged > read);
        let ended = self.differ_jaws.as_ref().is_some_and(Vec::is_empty);
        if live
            && !jawless_before
            && panels.screen == Screen::ChainTarget
            && in_chain
            && !ended
            && !self.eater_vaulted
            && jawed(&scene).is_empty()
            && (refills || self.bite_at.is_some_and(|at| at.elapsed() < BITE_SHOWS))
        {
            let stale = self.played_at.is_some()
                || (self.log_bites.is_none()
                    && self.reading.as_ref().is_some_and(|r| bitten(&r.model, &to_model(&scene, &panels, &self.tools, Some(&r.model), self.run_days()).0)));
            self.jawless_held = Some(stale);
            self.reread = true;
            return Taken::Kept(tr(CHANGED).to_string());
        }
        self.changers_seen = self.changers;
        match panels.screen {
            Screen::Patrol | Screen::Manage => self.overlay_seen = false,
            Screen::TreasurePick | Screen::InstrumentPick | Screen::CellPick => self.overlay_seen = true,
            _ => {}
        }
        self.run_over = None;
        // A target screen read after the chain's last bite offers nothing: the
        // meal picked before it stays the last one (2026-10-28: two treasures
        // warned the last meal was unknown, the board read with it locked).
        // A read too soon after a target screen that showed a bite has no
        // bite of its own in between: the meal eaten stays the last one
        // (2026-10-28: a chain's eater went Chromatic on its bite, the jaws
        // already on the next target, and the treasure 0.7 s later locked
        // that target too).
        // The game's log counts the bites: a read with none since the last
        // has none of its own, however long after it came.
        let too_soon = self.bite_seen.is_some_and(|at| at.elapsed() < NEXT_BITE);
        self.bite_logged = self.log_bites.is_some_and(|(logged, read)| logged > read);
        let bit = if self.log_bites.is_some() { self.bite_logged } else { !too_soon };
        // A pick in between (a bite's eater went Chromatic) leaves the bite
        // to the target screen after it.
        let counts_bites = matches!(panels.screen, Screen::ChainTarget | Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::PrisonStatus);
        if let Some((logged, _)) = self.log_bites.filter(|_| counts_bites) {
            self.log_bites = Some((logged, logged));
        }
        // The meal is the target the read shows eaten (see `meal_read`), not always the one advised: taken as the
        // advice, a casket's new inmate in the cell the player ate stood as the inmate eaten (2026-11-06 day 7). A
        // meal still in doubt past BITE_SHOWS (see the hold above) is unknown rather than guessed.
        if self.reading.as_ref().is_some_and(|r| r.screen == Screen::ChainTarget) && bit {
            let shown = self.consume.as_ref().filter(|_| self.bite_logged && panels.screen == Screen::ChainTarget).map(|before| {
                let (read, warns) = to_model(&scene, &panels, &self.tools, Some(before), self.run_days());
                self.meal_shown(before, &read, &warns, &jawed(&scene))
            });
            let (meal, doubt) = shown.unwrap_or_else(|| (self.meal_picked(), Vec::new()));
            if !doubt.is_empty() {
                self.meal = None;
            } else if let Some(meal) = meal {
                self.meal = Some(meal);
            }
            self.meal_doubt = doubt;
        }
        // The game's log names the day, and the riot countdown read can be
        // wrong: the day before for 0.4 s on day 17 and the whole patrol of
        // day 6 of a 2026-10-28 run, whose advice went by the wrong day; and
        // 15 read as 1 after a day 5 instrument pick, whose 15 read next
        // then took a new run and cleared the theme; and the run start's 20,
        // fading in under the instrument pick (day 0), read as 2. The count
        // so set is the one read, so no warning says it was kept.
        let log_left = self.log_day.and_then(|(cur, max)| max.checked_sub(cur));
        if let Some(left) = log_left
            && panels.days.is_some()
        {
            panels.days = Some(left);
        }
        // The run start's instrument pick (the log's day 0) hides the
        // countdown now and then, and a higher count waits for a second
        // read: a session started after the last run kept its 0 until day 1
        // (2026-10-28). The log's count needs no second read.
        if self.log_day.is_some_and(|(cur, _)| cur == 0) && panels.screen == Screen::InstrumentPick {
            panels.days = log_left;
        }
        let sure = log_left.is_some() && panels.days == log_left;
        self.days_read = panels.days;
        // A pick over no board read is taken as the run start (see `to_model`),
        // and its hidden counter as the run start's too: a count kept from an
        // earlier read or a past session can be stale. A lower count kept is a
        // past run's, so the instruments and theme held with it go.
        // The board between the run start's popups and its instrument pick
        // shows 0: no counter of the run, and it let the pick's 20 clear the
        // theme just read (2026-10-31).
        let counter_read = panels.days.is_some_and(|left| left > 0);
        let (days, new_run) = if run_start(panels.screen, self.board.as_ref()) && panels.days.is_none() {
            let start = self.run_days() as u32;
            let new_run = self.days.left.is_some_and(|left| left < start);
            if new_run {
                self.held_before_higher = self.tools.len();
            }
            self.days = DaysClock { left: Some(start), higher: None };
            (Some(start), new_run)
        } else {
            let (days, new_run) = self.days.update_run(panels.days, sure);
            // A sure higher count waits for no second read: the instruments
            // held all belong to the run before.
            if new_run && sure {
                self.held_before_higher = self.tools.len();
            }
            (days, new_run)
        };
        panels.days = days;
        if new_run && let Some(log) = &mut self.log {
            log.new_run();
        }
        if self.days.higher.is_some() {
            self.held_before_higher = self.tools.len();
        }
        let mut notes = Vec::new();
        let mut held_changed = false;
        if new_run {
            self.vault_paid = 0;
            self.objective_done = false;
        }
        let stale = if new_run { self.held_before_higher.min(self.tools.len()) } else { 0 };
        if stale > 0 {
            self.tools.drain(..stale);
            self.held_edit = None;
            notes.push(tr("A new run: the held instruments were cleared.").to_string());
            held_changed = true;
        }
        if new_run {
            self.theme_icon_seen = None;
        }
        if new_run && !self.theme_fresh && self.theme.take().is_some() {
            notes.push(tr("A new run: the theme was cleared.").to_string());
            held_changed = true;
        }
        // A higher count waiting for its second read already belongs to the
        // new run; the screens between the theme scroll and the run's first
        // counter (the instrument pick, the day banner) do too: on 2026-10-28
        // they let the run's first board clear the theme just read.
        if self.days.higher.is_none() && counter_read {
            self.theme_fresh = false;
        }
        // Only Iron Law runs past 20 days, so a longer count names the theme
        // when the lobby's scroll was not read; after the new-run clear above,
        // which would drop it again.
        if days.is_some_and(|left| i64::from(left) > RUN_DAYS) && self.theme != Some(IRON_LAW) {
            self.tools_changed = true;
            self.theme = Some(IRON_LAW);
            self.theme_fresh = true;
            self.theme_set = true;
        }
        // A theme just deduced: the same board must also be solved again.
        held_changed |= std::mem::take(&mut self.theme_set);
        if !matches!(panels.screen, Screen::InstrumentPick | Screen::PrisonStatus | Screen::Unknown) {
            self.pick_named = false;
            if let Some(t) = self.advised_tool.take()
                && !self.tools.contains(&t)
            {
                self.tools.push(t);
                self.held_edit = None;
                held_changed = true;
            }
        }
        // A sure read of the instrument panel names the held list, unless the
        // player set it while the panel read the same and the session has not
        // changed it since. A new run cleared the list above and a run loses
        // no instrument, so an empty panel read over a held list is a misread.
        if let Some(held) = &panels.held
            && !(held.is_empty() && !self.tools.is_empty())
            && self.held_edit.as_ref() != Some(&panels.held)
        {
            self.held_edit = None;
            if *held != self.tools {
                self.tools = held.clone();
                held_changed = true;
                let names: Vec<&str> = held.iter().map(|&t| instrument(t)).collect();
                notes.push(trf!("The {} panel shows: {}.", instrument_term(), names.join(tr(", "))));
            }
        }
        // The board shown over an open treasure's pick: a flicker while View
        // Prison Status opens when its cells are the pick's, else the pick's
        // effect, the board the next pick covers while picks are left, and
        // the one a new treasure pick (as a Chromatic gives) covers after.
        let mut picked = None;
        let mut after_pick = false;
        // A later frame of the board between two picks (see `gift_gap`): the
        // gift's effect still playing out at the first, as Brass Horn of
        // Strife's fights were (2026-10-28), the latest stands. It is the
        // same pick, counted once.
        let gap = self.gift_gap;
        if panels.screen != Screen::Unknown {
            self.gift_gap = None;
        }
        if panels.screen == Screen::Unknown
            && self.days_read.is_some()
            && let Some(keep) = gap
        {
            let model = self.gap_board(&scene, &panels);
            if self.board.as_ref().is_some_and(|b| b.cells == model.cells) {
                return Taken::Skipped;
            }
            picked = Some(model);
            after_pick = keep;
        } else if panels.screen == Screen::Unknown
            && self.days_read.is_some()
            && let Some(taken) = self.treasure
            && let (Some((_, over, _)), Some(r)) = (&self.scene, &self.reading)
            && over.screen == Screen::TreasurePick
        {
            let model = self.gap_board(&scene, &panels);
            if model.cells == r.model.cells {
                return Taken::Skipped;
            }
            picked = Some(model);
            after_pick = taken + 1 < treasure_picks(self.theme);
            self.treasure = after_pick.then_some(taken + 1);
            if !after_pick {
                self.treasures = self.treasures.saturating_sub(1);
                self.gift_picked = false;
            }
            self.gift_gap = Some(after_pick);
        }
        // A treasure pick whose Confirm went dark after a gift was picked is
        // the next treasure in a row: the gift was taken. View Prison Status
        // in between may drop the pick, so it forgets it.
        let mut treasure_opened = false;
        match panels.screen {
            Screen::TreasurePick | Screen::InstrumentPick if self.pick_after.is_none() => self.pick_opens(panels.screen),
            Screen::Patrol | Screen::Manage | Screen::ChainTarget | Screen::CellPick | Screen::Interrogation | Screen::DayBanner => self.pick_after = None,
            _ => {}
        }
        match panels.screen {
            Screen::TreasurePick => {
                // A pick's Confirm lit on a sample since the last read: a
                // gift picked and confirmed between two reads.
                let lit = self.trigger.is_some_and(|t| t.lit);
                // With the game's log, its word on a treasure in a row stands.
                let reopened = std::mem::take(&mut self.treasure_reopened);
                let next = if self.log_day.is_some() {
                    reopened
                } else {
                    (self.gift_picked || lit) && !panels.gift_picked && self.trigger.is_some_and(|t| t.left)
                };
                if self.treasure.is_none() && self.treasures == 0 {
                    self.treasures = self.treasures_in_a_row(&panels);
                    if self.treasures > 1 {
                        let line = trf!("{} Warden's Treasures in a row: the advice plans them together.", self.treasures);
                        notes.push(line.clone());
                        self.treasure_line = Some(line);
                    }
                // Only a pick whose cards left the screen since is a new one:
                // a lit Confirm flickers while a card is picked, and once read
                // dark gave a treasure in a row that never came; a treasure
                // right after a pick shows no board in between (2026-10-28).
                } else if next {
                    // The log's count is the vault's less the treasures taken.
                    self.treasures = match self.treasures_paid() {
                        Some(paid) => {
                            let now = self.vault_top.as_ref().map_or(0, |(_, v)| v.len()).max(panels.vault.len());
                            now.saturating_sub(paid).max(self.flown_unpaid()).clamp(1, TREASURES_MAX) as u32
                        }
                        None => self.treasures.saturating_sub(1).max(1),
                    };
                    treasure_opened = true;
                    if self.took_wine() {
                        notes.push(tr(WINE_TAKEN).to_string());
                        self.treasure_line = Some(trf!("Warden's Treasure, {} left with this one: the Crimson Wine's tiers are on the board.", self.treasures));
                    } else {
                        notes.push(tr(NEXT_TREASURE).to_string());
                        self.treasure_line = Some(trf!("Warden's Treasure, {} left with this one: the board may have changed, View Prison Status shows it.", self.treasures));
                    }
                }
                self.gift_picked = panels.gift_picked;
                self.treasure = Some(self.treasure.unwrap_or(0));
            }
            // The player looked at the board: the treasure's line has done its job.
            Screen::PrisonStatus => {
                self.gift_picked = false;
                self.treasure_line = None;
            }
            Screen::Patrol | Screen::Manage | Screen::ChainTarget | Screen::CellPick | Screen::Interrogation | Screen::InstrumentPick | Screen::DayBanner => {
                self.treasure = None;
                self.treasures = 0;
                self.gift_unanswered = false;
                self.treasure_reopened = false;
                self.gift_picked = false;
                self.treasure_line = None;
            }
            _ => {}
        }
        // View Prison Status shows the real board under a pick, and so does
        // the board after a treasure's pick: it becomes the last board read,
        // in place of any board inferred after the consume. With picks left,
        // or under View Prison Status, the pick stays on show, solved on that
        // board; after the last pick the scene becomes this read.
        let under_status = panels.screen == Screen::PrisonStatus
            && self.scene.as_ref().is_some_and(|(_, over, _)| matches!(over.screen, Screen::TreasurePick | Screen::InstrumentPick));
        let keep_pick = after_pick || under_status;
        if picked.is_some() || under_status {
            self.board = Some(picked.unwrap_or_else(|| to_model(&scene, &panels, &self.tools, self.board.as_ref(), self.run_days()).0));
            self.unseen_gifts.clear();
            self.consume = None;
            self.meal = None;
            self.meal_doubt.clear();
            self.chained = false;
            // View Prison Status shows the whole board, its vault too, though
            // the reading on show stays the pick's (2026-10-28: its 6 read,
            // the run end said 4).
            if under_status
                && let Some(board) = &self.board
                && self.vault_top.as_ref().is_none_or(|(_, v)| board.vault.len() > v.len())
            {
                self.vault_top = Some((board.day, board.vault.clone()));
            }
        }
        let (scene, panels) = match self.scene.take() {
            Some((pick, mut over, _)) if keep_pick => {
                over.days = panels.days;
                (pick, over)
            }
            _ => (scene, panels),
        };
        self.scene = Some((scene, panels, SystemTime::now()));
        // A pick kept on show is not the screen the game shows.
        self.on_screen = live && !keep_pick;
        self.steady.clear();
        // The cells of the reading on show before this one (a new run has none).
        let before: Vec<CellModel> = if new_run { Vec::new() } else { self.reading.as_ref().map(|r| r.model.cells.clone()).unwrap_or_default() };
        // The stored read's warnings, none dropped yet (`steady` is empty).
        let keys: Vec<(String, bool)> = self
            .build()
            .map(|r| r.warnings.iter().filter_map(|w| steady_key(w).map(|k| (k, warns_again(w.cell.and_then(|n| r.model.cells.get(n)), w.cell.and_then(|n| before.get(n)))))).collect())
            .unwrap_or_default();
        // Warnings shown before stay on show for the same screen and day; a new run or another screen or day starts
        // afresh.
        let warned_on = self.scene.as_ref().map(|(_, p, _)| warned_scope((p.screen, p.days), self.warned_on));
        if new_run || warned_on != self.warned_on {
            self.warned_keys.clear();
        }
        self.warned_on = warned_on;
        (self.steady, self.warned_keys) = steady_shown(keys, &self.warned_keys);
        // Compared as shown, the boards before merged in: a target or Hunt
        // pick read raw never matches it, and was solved again on every read
        // (128 times in 36 s on one Hunt pick, its advice flickering).
        let jaws_moved = self.jaws_moved();
        let jaws_now = self.scene.as_ref().map(|(scene, _, _)| jawed(scene));
        // The first read of jaws after a jawless read ended the chain waits for a second (see `jaws_moved`): read
        // again at once, as a still screen would otherwise wait for the next timed read.
        if self.differ_jaws.as_ref().is_some_and(Vec::is_empty) && jaws_now.as_ref().is_some_and(|now| !now.is_empty()) && jaws_now != self.jaws_last {
            self.reread = true;
        }
        self.jaws_last = jaws_now;
        if !held_changed
            && !treasure_opened
            && !jaws_moved
            && let Some(new) = self.rebuilt()
            && self.reading.as_ref().is_some_and(|old| (&old.model, old.screen, old.days_left) == (&new.model, new.screen, new.days_left))
        {
            // The solve's word that the targets differ from the game's marks
            // stands for the same board: dropped, the bite it advised on that
            // board showed for 0.07 s before the right one (2026-11-01).
            let mut new = new;
            if let Some(old) = &self.reading {
                new.warnings.extend(old.warnings.iter().filter(|w| begins(&w.text, TARGETS_DIFFER)).cloned());
            }
            if new.warnings.iter().any(|w| begins(&w.text, TARGETS_DIFFER)) && self.bite_at.is_some_and(|at| at.elapsed() >= DIFFER_STEADY) {
                self.differ_steady = true;
            }
            // The eater taken from the jaws, read as taken: it is seen, and
            // a frame of it still changing after is no guess (see `eater_seen`).
            if self.reading.as_ref().is_some_and(|old| old.eater_inferred)
                && !new.eater_inferred
                && new.screen == Screen::ChainTarget
                && self.chained
                && !new.warnings.iter().any(|w| begins(&w.text, TARGETS_DIFFER))
                && let Ok(n) = usize::try_from(new.model.warden)
            {
                self.eater_seen = new.model.cells.get(n).and_then(|c| eater_in(c, n));
            }
            self.reading = Some(new);
            return Taken::Same;
        }
        self.tools_changed |= held_changed;
        self.notes = notes;
        self.edits.clear();
        self.markers.clear();
        self.rebuild();
        Taken::Installed
    }

    /// Logs a read that installed nothing: its screen, the line it gave,
    /// the frame size and `frame` when its screen differs from the last
    /// frame saved.
    fn log_miss(&mut self, screen: Screen, line: String, frame: Option<Rgba>) {
        let why = self.unshown_why.take();
        let Some(log) = &mut self.log else { return };
        let file = frame.and_then(|f| log.frame(screen, None, f));
        let mut fields = json!({
            "screen": format!("{screen:?}"),
            "line": line,
            "size": [self.size.0, self.size.1],
            "frame": file,
        });
        if let Some(why) = why {
            fields["unshown"] = why;
        }
        timing(&mut fields, self.trigger.take(), self.scan_ms);
        log.event("miss", SystemTime::now(), fields);
    }

    /// Logs the window auto read captures, as the watcher describes it, and
    /// the language Windows OCR reads in (null with none installed).
    pub fn log_window(&mut self, window: &str) {
        if let Some(log) = &mut self.log {
            log.window(json!({ "window": window, "ocr": crate::reader::ocr::language() }));
        }
    }

    /// Logs the reading on show, and `frame` when it shows another screen or
    /// board than the last frame saved.
    fn log_read(&mut self, frame: Option<Rgba>) {
        let ocr = crate::reader::ocr::cache_counts();
        let seen = std::mem::replace(&mut self.ocr_seen, ocr);
        let why = self.unshown_why.take();
        let (Some(log), Some(r)) = (&mut self.log, &self.reading) else { return };
        let file = frame.and_then(|f| log.frame(r.screen, Some(&r.model), f));
        let warnings: Vec<&str> = r.warnings.iter().map(|w| w.text.as_str()).collect();
        let mut fields = json!({
            "screen": format!("{:?}", r.screen),
            "days_left": r.days_left,
            "tools": tool_keys(&self.tools),
            "theme": data().theme_name(self.theme),
            "warnings": warnings,
            "frame": file,
            "model": r.model,
        });
        // Why no advice came: `solve` sets these after the read.
        if !self.problems.is_empty() {
            fields["problems"] = json!(self.problems);
        }
        // OCR regions given back by the cache and read by the engine since the last read logged.
        fields["ocr_cached"] = json!([ocr.0 - seen.0, ocr.1 - seen.1]);
        // The interface scale this read was laid out at (see `crate::reader::layout::measured_scale`), to 3 decimals.
        if let Some(u) = self.ui_scale {
            fields["u"] = json!(logged_scale(u));
        }
        if let Some(why) = why {
            fields["unshown"] = why;
        }
        timing(&mut fields, self.trigger.take(), self.scan_ms);
        log.event("read", r.at, fields);
    }

    /// Logs the advice of a finished solve: the pick and the picks about as
    /// good, with the loot the playouts expect of the pick and of the best
    /// (`ev`, `best_ev`) and the spread of the best's playouts (`sd`): a run
    /// ending far from what its days expected tells of a model wrong.
    fn log_advice(&self) {
        let (Some(log), Some(a)) = (&self.log, self.advice()) else { return };
        // A lone move plays no rounds: its mean of 0 is no prediction.
        let pick = a.option.and_then(|k| self.ranking.get(k)).filter(|o| o.n > 0);
        let best = self.ranking.first().filter(|o| o.n > 0);
        let round = |x: f64| (x * 1000.0).round() / 1000.0;
        // The memo the solver keeps in reserve (for measurement, see solver::held_memos).
        let (memo_held, memo_bytes) = crate::solver::held_memos();
        let fields = json!({
            "read_at": self.reading.as_ref().map(|r| timestamp(r.at)),
            "pick": a.label,
            "also_good": also_good(&self.ranking, a.option).into_iter().map(|k| self.ranking[k].label.as_str()).collect::<Vec<_>>(),
            "rounds": self.rounds,
            "ev": pick.map(|o| round(o.mean)),
            "best_ev": best.map(|o| round(o.mean)),
            "sd": best.map(|o| round(o.se * (o.n as f64).sqrt())),
            "memo_held": memo_held,
            "memo_mb": memo_bytes / (1 << 20),
        });
        log.event("advice", SystemTime::now(), fields);
    }

    /// Corrects screen cell `cell` (2-16; 1 is the Interrogation Room) and re-solves.
    pub fn edit_cell(&mut self, cell: u8, edit: CellEdit) -> Result<(), String> {
        self.check_editable(cell)?;
        self.edits.insert(cell, edit);
        self.log_edit(cell, format!("{edit:?}"));
        self.rebuild();
        Ok(())
    }

    /// Adds or removes the incident marker on screen cell `cell` (2-16) and re-solves.
    pub fn set_marker(&mut self, cell: u8, on: bool) -> Result<(), String> {
        self.check_editable(cell)?;
        self.markers.insert(cell, on);
        self.log_edit(cell, format!("Marker {}", if on { "on" } else { "off" }));
        self.rebuild();
        Ok(())
    }

    fn check_editable(&self, cell: u8) -> Result<(), String> {
        let count = self.reading.as_ref().ok_or("no board read yet")?.model.cells.len();
        if !(2..=count).contains(&(cell as usize)) {
            return Err(format!("cell {cell} can't be edited"));
        }
        Ok(())
    }

    fn log_edit(&self, cell: u8, edit: String) {
        if let Some(log) = &self.log {
            log.event("edit", SystemTime::now(), json!({ "cell": cell, "edit": edit }));
        }
    }

    /// True once after the session changed the held instruments or the theme
    /// itself (the scroll, a new run, an advised pick, the run's end), so the
    /// UI saves them.
    pub fn take_tools_changed(&mut self) -> bool {
        std::mem::take(&mut self.tools_changed)
    }

    /// The days until Riot in use: None until a first read and after a run's end.
    pub fn days_left(&self) -> Option<u32> {
        self.days.left
    }

    /// Sets the held instruments (the panel shows only icons) and re-solves;
    /// the lines about the session's own changes to them go.
    pub fn set_tools(&mut self, tools: Vec<Tool>) {
        if let Some(log) = &self.log {
            log.event("edit", SystemTime::now(), json!({ "tools": tool_keys(&tools) }));
        }
        self.tools = tools;
        self.held_edit = Some(self.scene.as_ref().and_then(|(_, p, _)| p.held.clone()));
        self.notes.clear();
        if self.reading.as_ref().is_some_and(|r| r.screen == Screen::InstrumentPick) {
            self.advised_tool = None;
            self.pick_named = true;
        }
        if self.scene.is_some() {
            self.rebuild();
        }
    }

    /// Sets the run's theme (the scroll was missed or misread) and re-solves.
    pub fn set_theme(&mut self, theme: Option<u32>) {
        if let Some(log) = &self.log {
            log.event("edit", SystemTime::now(), json!({ "theme": data().theme_name(theme) }));
        }
        self.theme = theme;
        self.theme_fresh = true;
        if self.reading.is_some() {
            self.solve();
        }
    }

    /// A line important enough to show on the game itself (see
    /// [`crate::app::mark::Notice`]), the first of: a game window of the
    /// wrong shape; the run's end, for [`RUN_OVER_NOTICE`]; and while the
    /// reading on show is the game's screen, treasures in a row or the next
    /// one after a gift was taken, a board under a pick that only View
    /// Prison Status shows, and why no advice came once that lasted
    /// [`NO_ADVICE_NOTICE`].
    pub fn notice(&self) -> Option<String> {
        if self.suspended {
            return None;
        }
        // A window of the wrong shape stops every read.
        if let Some(line) = self.last_kept.as_ref().filter(|l| begins(l, SHAPE)) {
            return Some(line.clone());
        }
        // The run's end shows on the game with the inmates vaulted only; the run
        // check stays in the Ascendant window (status bar) and the run report.
        if let (Some(line), Some(at)) = (&self.run_over, self.run_over_at)
            && at.elapsed() < RUN_OVER_NOTICE
        {
            return Some(line.clone());
        }
        let r = self.reading.as_ref().filter(|_| self.on_screen)?;
        if r.screen == Screen::TreasurePick
            && let Some(line) = &self.treasure_line
        {
            return Some(line.clone());
        }
        if let Some(w) = r.warnings.iter().find(|w| w.kind == WarningKind::CheckStatus) {
            return Some(w.text.clone());
        }
        if matches!(r.screen, Screen::Manage | Screen::ChainTarget)
            && let Some(line) = self.chain_line()
        {
            return Some(line);
        }
        // A moment without advice is an animation; longer, the player waits for a mark that won't come.
        if let (Some(problem), Some(since)) = (self.problems.first(), self.problems_since)
            && since.elapsed() >= NO_ADVICE_NOTICE
        {
            return Some(trf!("No advice: {}", problem));
        }
        None
    }

    /// The treasure pick the running solve plays: the gifts on offer and the
    /// picks left of the treasure, this one included.
    pub fn treasure_pick(&self) -> Option<(&[Gift], u32)> {
        match self.state.as_ref()?.pending.first()? {
            Pending::Gift { options, picks } => Some((options, *picks)),
            _ => None,
        }
    }

    /// The Warden's Treasures a treasure pick that opens now begins: the
    /// Chromatic Prisoners in the vault past those counted before, at least
    /// one and at most [`TREASURES_MAX`]. Counted against the vault paid
    /// for, not the board before: a frame between two treasures can show the
    /// vault empty (2026-10-28: "6 in a row" for one).
    fn treasures_in_a_row(&mut self, panels: &Panels) -> u32 {
        let now = self.vault_top.as_ref().map_or(0, |(_, v)| v.len()).max(panels.vault.len());
        let paid = self.treasures_paid().unwrap_or(self.vault_paid);
        let n = now.saturating_sub(paid).max(self.flown_unpaid()).clamp(1, TREASURES_MAX);
        self.vault_paid = paid + n;
        n as u32
    }

    /// The treasures the game's log saw Chromatics fly into the vault for,
    /// less those taken: the treasures in a row left, this one with them, at
    /// the least. The vault panel is dimmed between them: after Usurpation IV
    /// vaulted six at once, five treasures were each advised as the last,
    /// until View Prison Status showed the vault (2026-11-01, day 15).
    fn flown_unpaid(&self) -> usize {
        self.vaulted_seen().saturating_sub(self.gifts_taken / treasure_picks(self.theme) as usize)
    }

    /// The number of Chromatics vaulted as seen in the game log: of the two counts, the fly-in sound effect and the
    /// vault cell lighting up, take the larger, and when one of them is missed the other fills in (2026-11-03).
    fn vaulted_seen(&self) -> usize {
        self.vault_flies.max(self.vault_lights)
    }

    /// The board read between a treasure's picks. The vault panel between
    /// picks is not read (see `build`): the board's stands. The next
    /// treasure's banner glows over the top row: an inmate read unsure there
    /// keeps the board's rarity (three read Normal for Chromatic on
    /// 2026-10-29), and a room opened since is taken as read.
    fn gap_board(&self, scene: &Scene, panels: &Panels) -> Model {
        let (mut model, warnings) = to_model(scene, panels, &self.tools, self.board.as_ref(), self.run_days());
        let Some(board) = self.board.as_ref() else { return model };
        model.vault = board.vault.clone();
        for n in warnings.iter().filter(|w| begins(&w.text, CHECK_CELL)).filter_map(|w| w.cell) {
            if let (Some(CellModel::Inmate { beast: was, .. }), Some(CellModel::Inmate { beast, .. })) = (board.cells.get(n), model.cells.get(n))
                && was == beast
            {
                model.cells[n] = board.cells[n].clone();
            }
        }
        model
    }

    /// The treasures taken this run, as the game's log counts their gifts
    /// confirmed; None without the log (it names each day). A treasure in a
    /// row that the vault read missed paid none from the vault, and the
    /// next treasure counted the vault's Chromatics again: 3 in a row for 1
    /// on 2026-10-29 after 5 in a row read as 1.
    ///
    /// A session started mid-run counts the vault it first read as paid: one
    /// started on day 12 with 5 vaulted called the next treasure 6 in a row
    /// (2026-10-31).
    fn treasures_paid(&self) -> Option<usize> {
        self.log_day.map(|_| self.vault_base + self.gifts_taken / treasure_picks(self.theme) as usize)
    }

    /// The model again from the stored read, the tools and the edits; then a solve.
    fn rebuild(&mut self) {
        let Some(reading) = self.rebuilt() else { return };
        self.reading = Some(reading);
        self.differ_steady = false;
        self.solve();
    }

    /// The reading [`Session::rebuild`] puts on show: the stored read with
    /// the boards before merged in (a target or Hunt pick hides some of the
    /// board). Taking the same read again gives the same reading.
    fn rebuilt(&mut self) -> Option<Reading> {
        let mut reading = self.build()?;
        // The Hunt's target pick shows no cards, and its white glow on a
        // lone target spoils that cell's read: the cells and the day's used
        // Consume and Replace are the same day's manage screen's.
        let prey = reading.screen == Screen::CellPick && reading.model.choice.kind == ChoiceKind::Prey;
        if prey && let Some(m) = self.consume.as_ref().filter(|m| m.day == reading.model.day) {
            reading.model.cells = m.cells.clone();
            (reading.model.devour_used, reading.model.swap_used) = (m.devour_used, m.swap_used);
        }
        if matches!(reading.screen, Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::PrisonStatus) {
            self.board = Some(reading.model.clone());
            self.unseen_gifts.clear();
            self.chained = false;
            self.meal = None;
            self.meal_doubt.clear();
            // The chain is over: without the game's log no Bite clears the
            // eater seen, and a later chain's eater could pass for it.
            self.eater_seen = None;
            // If the log stopped midway there is no Consume to clear it: the board from the earlier bite's chain no
            // longer counts either.
            self.bite_cells = None;
        }
        match reading.screen {
            Screen::Unknown if reading.days_left.is_some() => self.glimpse = Some(reading.model.clone()),
            Screen::Patrol | Screen::Manage | Screen::ChainTarget | Screen::CellPick | Screen::PrisonStatus => self.glimpse = None,
            _ => {}
        }
        match reading.screen {
            Screen::CellPick if prey => {}
            Screen::Patrol | Screen::CellPick | Screen::PrisonStatus => self.consume = None,
            Screen::Manage => {
                self.consume = Some(reading.model.clone());
                self.consuming = false;
                self.bite_seen = None;
            }
            Screen::ChainTarget => {
                self.consuming = true;
                // The screen hides the counter, the vault and the decision
                // buttons: only its cells and warden are taken. A cell bitten
                // just before may still show as open while its inmate fades:
                // it locks again, as every consumed cell does.
                // The red jaws over a target spoil its read: a cell read with
                // low confidence keeps the board's cell, and its warning goes,
                // unless the player corrected it.
                let before = self.consume.take().or_else(|| self.board.clone());
                let mut read = reading.model.clone();
                let unedited = |n: usize| !self.edits.contains_key(&(n as u8 + 1));
                if let Some(m) = &before {
                    let unsure = |w: &Warning| begins(&w.text, CHECK_CELL) && w.cell.is_some_and(unedited);
                    for n in reading.warnings.iter().filter(|w| unsure(w)).filter_map(|w| w.cell) {
                        read.cells[n] = m.cells[n].clone();
                    }
                    reading.warnings.retain(|w| !unsure(w));
                }
                let mut m = before.unwrap_or_else(|| reading.model.clone());
                // The board before this read: when there is no log Bite, the Avenger's Casket check below uses it
                // (see `bite_cells`).
                let was = m.cells.clone();
                // A bite on a dark board can leave its cells as they were.
                // With the game's log on, the board shows a bite only by an
                // inmate leaving its cell: the eater's portrait misread on a
                // frame grabbed before the click took it for one, and the
                // advice named the meal about to go again (2026-10-30).
                let targets = self.scene.as_ref().map_or(Vec::new(), |(scene, _, _)| jawed(scene));
                let shown = if self.log_bites.is_some() { bite_shown(&m, &read, self.meal_picked().map(|meal| meal.cell), &targets) } else { bitten(&m, &read) };
                let bite = shown || self.bite_logged;
                self.chained |= bite;
                if bite {
                    self.bite_seen = Some(std::time::Instant::now());
                }
                // A bite the board shows is the eater's change: without the
                // game's log, whose Bite clears it, the eater seen goes here.
                if shown && self.log_bites.is_none() {
                    self.eater_seen = None;
                }
                let jaws = self.scene.as_ref().map_or(Vec::new(), |(scene, _, _)| jawed(scene));
                let eater = usize::try_from(m.warden).ok();
                // An Exalted eater's bite makes it Chromatic, and the vault
                // takes it: its cell reads locked. The board keeps it there
                // for the treasure's `vaulted`, but its chain is over: solved
                // on, it advised a bite as the treasure came up (2026-10-31).
                // Any read of the chain counts, not just the bite's own: the
                // vault took one eater 1 s on, after two reads still showed it.
                // The cell as read, before an unsure read gives way to the
                // board's: the vault's sparkle over the lock read with low
                // confidence, and the eater came back (2026-10-31).
                self.eater_vaulted = self.chained
                    && eater.is_some_and(|n| {
                        matches!(
                            (&m.cells[n], &reading.model.cells[n]),
                            (&CellModel::Inmate { tier, .. }, CellModel::Sealed { .. } | CellModel::Scratch { .. }) if tier + 1 >= i64::from(RAINBOW)
                        )
                    });
                for (n, (cell, read)) in m.cells.iter_mut().zip(&read.cells).enumerate() {
                    *cell = match (&*cell, read) {
                        // The eater leaves its cell only for the vault (see `vaulted`):
                        // mid-bite its cell has read as a Scratch (2026-10-28), and the
                        // treasure after it then found no eater to vault.
                        (CellModel::Inmate { .. }, read) if Some(n) == eater && !matches!(read, CellModel::Inmate { .. }) => cell.clone(),
                        // The game marks only cells with an inmate as targets: one
                        // read without one is spoiled by the jaws over it. Taken
                        // as eaten, the casket's meal was guessed there before
                        // any bite, and the advice left a Legendary Wolf for a
                        // Normal Lion (2026-10-31).
                        (CellModel::Inmate { .. }, read) if jaws.contains(&n) && !matches!(read, CellModel::Inmate { .. }) => cell.clone(),
                        (CellModel::Inmate { .. }, &CellModel::Empty { event }) => CellModel::Sealed { event },
                        // A consumed cell locks again as its inmate fades: read
                        // open in between, the board changed and solved again,
                        // and a near tie went from "Chain 10" to "End"
                        // (2026-10-31). No door opens empty on this screen.
                        (CellModel::Sealed { .. }, CellModel::Empty { .. }) => cell.clone(),
                        // The screen's red tint hides the Tusk mark (a 2026-10-28
                        // chain read 2, 1 and then none of the 4 Tusks the board
                        // showed, and its advice moved): the same beast keeps
                        // the board's. A target under the big jaws keeps its
                        // rarity too: only the eater's changes before a bite,
                        // and a bright phase of the jaws read a Rare Mammoth
                        // as Exalted.
                        (&CellModel::Inmate { beast: before, ivory, tier: board_tier, .. }, &CellModel::Inmate { beast, tier, prey, event, .. }) if before == beast => {
                            let tier = if jaws.contains(&n) && unedited(n) { board_tier } else { tier };
                            CellModel::Inmate { beast, tier, ivory, prey, event }
                        }
                        // A scratched door shows its inmate's rarity, and a bite
                        // that opens it shows the inmate fading in: a Magic
                        // door's Wolf read Normal, the Magic eater had no meal on
                        // the board read, and no advice showed for 1.1 s while
                        // the game marked it (2026-11-01, day 1).
                        (&CellModel::Scratch { tier: door, .. }, &CellModel::Inmate { beast, tier, ivory, prey, event }) if tier < door => {
                            CellModel::Inmate { beast, tier: door, ivory, prey, event }
                        }
                        _ => read.clone(),
                    };
                }
                // The board before may be the day's patrol.
                (m.warden, m.step) = (reading.model.warden, Step::Manage);
                // The eater the jaws called for (see `eater_from_jaws`) stands
                // over a read of it lower, its rise still to show: such a read
                // solved again on the old eater and hid the mark (2026-10-31).
                if let Some((n, taken)) = self.inferred_eater.clone()
                    && usize::try_from(m.warden).ok() == Some(n)
                    && self.bite_at.is_some_and(|at| at.elapsed() < BITE_SHOWS)
                    && matches!((&m.cells[n], &taken), (CellModel::Inmate { tier: read, .. }, CellModel::Inmate { tier, .. }) if read < tier)
                {
                    reading.eater_inferred = self.eater_seen != eater_in(&taken, n);
                    m.cells[n] = taken;
                }
                // Only Avenger's Casket and the Crowded Beastcage open a door on
                // a bite, and the jaws mark its new inmate only when the eater
                // can eat it: at the eater's rarity, unless the eater may eat
                // across rarities (the Cage Fight, a Wolf with the Guillotine,
                // a Mammoth with the Maul), see `casket_meals`.
                let eater = usize::try_from(m.warden).ok().and_then(|w| match m.cells.get(w) {
                    Some(&CellModel::Inmate { beast, tier, .. }) => Some((beast, tier)),
                    _ => None,
                });
                let eater_tier = eater.map(|(_, tier)| tier);
                let across = self.theme == Some(CAGE_FIGHT)
                    || eater.is_some_and(|(beast, _)| (beast == Beast::Wolf && self.tools.contains(&Tool::Guillotine)) || (beast == Beast::Mammoth && self.tools.contains(&Tool::Maul)));
                let casket = self.tools.contains(&Tool::Revenge);
                let reopens = casket || (self.tools.contains(&Tool::Beastcage) && !across);
                // The casket may open the bitten cell again at once: after a
                // bite, jaws over the meal's cell mark the casket's new inmate,
                // not the meal (a 2026-10-28 chain kept the eaten Magic Mammoth
                // there and offered only the other target). The Beastcage
                // opens the bitten cell again too.
                if bite
                    && reopens
                    && let Some(meal) = self.meal.filter(|meal| jaws.contains(&meal.cell) && unedited(meal.cell))
                {
                    m.cells[meal.cell] = CellModel::Sealed { event: m.cells[meal.cell].event() };
                }
                // With the meal in doubt (see `meal_doubt`), each target that may have gone, jawed and read otherwise
                // than before, is the new inmate: left as the inmate from before the bite, under the jaws keeping its
                // rarity, the eater could have nothing to eat on the board until the next bite. Not when the eater may
                // eat across rarities (see `across`): a target left uneaten can be jawed again then, its read spoiled
                // by the jaws, and refilled at the eater's rarity it would be a guess.
                if bite && reopens && !across && self.meal.is_none() {
                    for &n in self.meal_doubt.iter().filter(|&&n| jaws.contains(&n) && unedited(n)) {
                        if was.get(n).map(cell_sight) != reading.model.cells.get(n).map(cell_sight) {
                            m.cells[n] = CellModel::Sealed { event: m.cells[n].event() };
                        }
                    }
                }
                // A door the casket opened during the chain reads its meal's
                // rarity wrong while the meal's glow comes in, and the jaws
                // keep that rarity: a Rare Wolf for an Exalted Lion's meal on a
                // 2026-10-28 chain, whose advice went. A jawed new inmate off
                // the eater's rarity is misread.
                // A door opened by an earlier bite in the chain holds an inmate of the eater's rarity at that time,
                // and later reads reuse the previous board: after the eater ranks up and is compared against it, a
                // rare Alpha Wolf was changed to Legendary, and with a leftover jaws mark from the previous round
                // happening to be there, the advice was 04, which the eater could not eat, and it only jumped to 16
                // 0.2 s later (2026-11-03 day 5). "An earlier bite" must look at the board at the moment of the bite,
                // not the board before this read: at the first read after a bite the eater has not finished
                // transforming and its rarity is still the pre-bite one, so the door opened by this bite was left at
                // that rarity, and later reads kept treating it as opened by an earlier bite. A red jaws made the new
                // door read one tier lower, the eater read as having no target, and the whole chain had no advice
                // (two runs on 2026-11-03 day 7: cell 05, a Magic Mammoth, was read as a Rare Berserk Lion and the
                // player bit by themselves 0.3 s later; cell 12, a Magic Colossal Rat, was read as Normal, and the
                // chain ended after 4.8 s without advice).
                if casket
                    && !across
                    && let Some(board) = &self.board
                {
                    // A cell just eaten that opens again holds a new inmate, so compare by the eater's current rarity.
                    let meal = self.meal.map(|meal| meal.cell);
                    // A target that may have been the meal in doubt (see `meal_doubt`) counts as the cell just eaten:
                    // jawed after the bite, it holds a new inmate, as one left at its rarity from before is no target.
                    let eaten = |n: usize| Some(n) == meal || (meal.is_none() && self.meal_doubt.contains(&n));
                    let at_bite = self.bite_cells.as_ref().unwrap_or(&was);
                    for &n in jaws.iter().filter(|&&n| unedited(n)) {
                        // The cell just eaten, jawed again, holds the casket's new inmate too, though the day's board
                        // still has the meal there: left out, the rarity it was first taken at stood for the rest of
                        // the bite, whatever the eater read later (2026-11-06 day 8: a Magic Wolf taken as Normal from
                        // an eater not yet risen, and the targets differed from the game's marks until the chain
                        // ended).
                        let opened = !matches!(board.cells.get(n), Some(CellModel::Inmate { .. })) || eaten(n);
                        let earlier = match at_bite.get(n) {
                            Some(cell @ CellModel::Inmate { .. }) if !eaten(n) => Some(cell.clone()),
                            _ => None,
                        };
                        let expected = match earlier {
                            Some(CellModel::Inmate { tier, .. }) => Some(tier),
                            _ => eater_tier,
                        };
                        if opened && matches!(m.cells[n], CellModel::Inmate { tier, .. } if Some(tier) != expected) {
                            m.cells[n] = earlier.unwrap_or_else(|| CellModel::Sealed { event: m.cells[n].event() });
                        }
                    }
                }
                // The casket opens a door only on a bite.
                if self.chained
                    && let Some((scene, _, _)) = &self.scene
                {
                    let cells: Vec<usize> = jawed(scene).into_iter().filter(|&n| unedited(n)).collect();
                    reading.warnings.extend(casket_meals(&mut m, &reading.model, &cells));
                }
                // Before the first bite the day's consume is still open.
                m.devour_used = self.chained;
                m.tools = reading.model.tools.clone();
                // The solve takes this board, not the bare read.
                reading.model = m.clone();
                self.consume = Some(m);
            }
            _ => {}
        }
        let (day, vault) = (reading.model.day, &reading.model.vault);
        // Only a board screen's vault counts: on a frame in between, a passing
        // glow once read 9 Chromatic Prisoners for none, and the treasure that
        // followed was taken as 6 in a row (2026-10-28).
        let board_screen = matches!(reading.screen, Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::PrisonStatus | Screen::Interrogation);
        if board_screen && self.vault_top.as_ref().is_none_or(|(d, v)| day < *d || vault.len() > v.len()) {
            // The run's first board this session, or a new run's: the
            // Chromatics already vaulted had their treasures before.
            if self.vault_top.as_ref().is_none_or(|(d, _)| day < *d) {
                self.gifts_taken = 0;
                self.vault_flies = 0;
                self.vault_lights = 0;
                self.vault_base = vault.len();
            }
            self.vault_top = Some((day, vault.clone()));
        }
        if matches!(reading.screen, Screen::Patrol | Screen::Manage | Screen::CellPick | Screen::PrisonStatus) {
            let seen = &self.prey_seen;
            self.prey_seen = prey_cells(&reading.model)
                .map(|x| seen.iter().copied().find(|&(c, d)| c == x && d <= day).unwrap_or((x, day)))
                .collect();
        }
        Some(reading)
    }

    /// The days the run lasts: Iron Law extends it.
    fn run_days(&self) -> i64 {
        match self.theme {
            Some(IRON_LAW) => iron_law_days(),
            _ => RUN_DAYS,
        }
    }

    /// The reading of the stored read with the tools and the edits.
    fn build(&self) -> Option<Reading> {
        let (scene, panels, at) = self.scene.as_ref()?;
        let chains = self.tools.contains(&Tool::Chains);
        let pick = matches!(panels.screen, Screen::TreasurePick | Screen::InstrumentPick);
        let after_consume = self.consume.as_ref().filter(|_| panels.screen == Screen::TreasurePick);
        // View Prison Status read since drops the manage board, and the line with it.
        let replaced = after_consume.is_some() && matches!(self.pick_after, Some(PickAfter::Replace { .. }));
        let vaulted = after_consume.and_then(|m| vaulted(m, self.meal, chains));
        // A treasure after a consume whose eater did not read: the consume's
        // board, bites included, not the day's board from before it.
        let lost_eater = vaulted.is_none() && after_consume.is_some() && !replaced;
        // An instrument pick a chain's bite opened (the objective done) covers
        // the chain's board too, not the day's board from before it: a pick
        // after three bites was solved with the three meals back in their
        // cells (2026-11-01, day 6).
        let in_chain = self.consume.as_ref().filter(|_| panels.screen == Screen::InstrumentPick && self.chained);
        let under = vaulted.as_ref().map(|(m, _, _)| m).or(after_consume).or(in_chain).or(self.board.as_ref());
        let assumed_days = run_start(panels.screen, under) && self.days_read.is_none();
        let (mut model, mut warnings) = to_model(scene, panels, &self.tools, under, self.run_days());
        if self.objective_done {
            complete_objective(&mut model);
        }
        model.chain_armed |= self.chain_armed;
        // A sealed door's glow (a Scratch mark) read unsure came and went
        // between reads of one patrol, and each change solved again and moved
        // the advice (2026-10-30: "Go to 11" became "Go to 13"): an unsure
        // read of it keeps the reading on show's.
        if let Some(prev) = self.reading.as_ref().filter(|r| r.screen == panels.screen && r.model.day == model.day) {
            let door = |c: &CellModel| matches!(c, CellModel::Sealed { .. } | CellModel::Scratch { .. });
            for n in warnings.iter().filter(|w| begins(&w.text, CHECK_CELL)).filter_map(|w| w.cell) {
                if let (Some(was), Some(now)) = (prev.model.cells.get(n), model.cells.get(n))
                    && door(was)
                    && door(now)
                    && was != now
                    && !self.edits.contains_key(&(n as u8 + 1))
                {
                    model.cells[n] = was.clone();
                }
            }
        }
        // The same day's spent actions before and after View Prison Status,
        // which reads every one spent (see `PickAfter`).
        if pick && let Some(after) = self.pick_after {
            (model.devour_used, model.swap_used, model.hunt_used) = after.used();
        }
        // A pick right after the patrol covers the board as the patrol left
        // it; the incident or the move that opened the pick may have changed it.
        if pick && !self.pick_seen && self.pick_after == Some(PickAfter::Patrol) && under.is_some_and(|b| b.step == Step::Patrol) {
            warnings.push(Warning { cell: None, kind: WarningKind::CheckStatus, text: tr(AFTER_PATROL).to_string() });
        }
        // A frame between screens keeps the board's vault: its panel can read
        // empty slots as filled under a passing glow.
        if panels.screen == Screen::Unknown
            && let Some(board) = under
        {
            model.vault = board.vault.clone();
        }
        // Chromatics never leave the vault in a run: a read that shows fewer
        // (the panel did not read, or only some of its slots) keeps the last
        // board's. A day before the board's is a new run.
        if let Some(board) = under.filter(|b| b.day <= model.day && b.vault.len() > model.vault.len()) {
            model.vault = board.vault.clone();
        }
        // The board under a treasure pick can be View Prison Status's with
        // the vault panel unread: the pick keeps the run's longest vault.
        if panels.screen == Screen::TreasurePick
            && let Some((_, vault)) = self.vault_top.as_ref().filter(|(day, v)| *day <= model.day && v.len() > model.vault.len())
        {
            model.vault = vault.clone();
        }
        // An incident marker leaves its cell only when the warden lands there,
        // but a frame can miss it: day 18 of a 2026-10-28 run read cell 16's
        // marker, then not 0.3 s later, and the advice left it. The day's
        // patrol keeps the markers its board showed in cells read the same.
        if panels.screen == Screen::Patrol
            && let Some(board) = under.filter(|b| b.day == model.day && b.step == Step::Patrol)
        {
            for (cell, before) in model.cells.iter_mut().zip(&board.cells) {
                let mut kept = before.clone();
                if let CellModel::Sealed { event }
                | CellModel::Empty { event }
                | CellModel::Inmate { event, .. }
                | CellModel::Scouted { event, .. }
                | CellModel::Scratch { event, .. } = &mut kept
                {
                    *event = None;
                }
                if cell.event().is_none() && before.event().is_some() && *cell == kept {
                    *cell = before.clone();
                }
            }
        }
        for cell in &mut model.cells {
            if let CellModel::Inmate { prey: Some(mark), .. } = cell
                && let Some(&(_, first)) = self.prey_seen.iter().find(|&&(c, d)| Some(c as i64) == mark.cell && d <= model.day)
            {
                mark.rounds = (mark.rounds - (model.day - first)).max(1);
            }
        }
        warnings.retain(|w| steady_key(w).and_then(|k| self.steady.get(&k)).is_none_or(|&shown| shown));
        if lost_eater {
            warnings.push(Warning { cell: None, kind: WarningKind::CheckStatus, text: tr(LOST_EATER).to_string() });
        }
        if replaced {
            warnings.push(Warning { cell: None, kind: WarningKind::CheckStatus, text: tr(REPLACED).to_string() });
        } else if let Some((_, n, removed)) = vaulted {
            let left = trf!("The Warden's Treasure follows a Chromatic Prisoner: the inmate in cell {} left for the vault", cell_label(n));
            let removed: Vec<String> = removed.into_iter().map(cell_label).collect();
            // Without the cells it changed known, only View Prison Status shows
            // the board. A last meal known or taken as the advised target is a note: a
            // chain that vaults its eater opens the treasure before its last
            // bite reads, and players follow the advice (60 of 303 reads of a
            // 2026-10-28 run showed it as a warning, every time right).
            let kind = if !removed.is_empty() || self.meal.is_none() { WarningKind::CheckStatus } else { WarningKind::Note };
            let text = match self.meal {
                _ if !removed.is_empty() => trf!(
                    "{}, lifted by the {} at the start of the day, which removed {}: check them with View Prison Status.",
                    left,
                    tool_name(Tool::Chains.key()),
                    match &removed[..] {
                        [one] => trf!("the inmate in cell {}", one),
                        _ => trf!("the inmates in cells {}", removed.join(tr(" and "))),
                    },
                ),
                Some(Meal { cell, certain: true }) => trf!("{} after consuming cell {}.", left, cell_label(cell)),
                Some(Meal { cell, certain: false }) => trf!("{}. Cell {} is taken as its last meal, the target Ascendant advised: click the cell to correct it.", left, cell_label(cell)),
                None => trf!("{}. The last cell it consumed is not known and still shows its inmate: check it with View Prison Status.", left),
            };
            warnings.push(Warning { cell: None, kind, text });
        }
        // The Consume target screen always hides the counter; the run start's
        // count is assumed, not kept.
        if panels.screen != Screen::ChainTarget && !assumed_days {
            warnings.extend(kept_days(self.days_read, panels.days));
        }
        warnings.extend(self.notes.iter().map(|text| Warning::note(text.clone())));
        for (&cell, edit) in &self.edits {
            let n = cell as usize - 1;
            // A correction keeps the cell's incident marker and Tusk.
            let (event, ivory) = (model.cells[n].event(), model.cells[n].ivory());
            model.cells[n] = match *edit {
                CellEdit::Locked => CellModel::Sealed { event },
                CellEdit::Empty => CellModel::Empty { event },
                CellEdit::Inmate { beast, tier } => CellModel::Inmate { beast, tier, ivory, prey: None, event },
            };
        }
        // The Office and a Nest carry no marker.
        for (&cell, &on) in &self.markers {
            if let CellModel::Sealed { event }
            | CellModel::Empty { event }
            | CellModel::Inmate { event, .. }
            | CellModel::Scouted { event, .. }
            | CellModel::Scratch { event, .. } = &mut model.cells[cell as usize - 1]
            {
                *event = on.then_some(EventMark::Unknown);
            }
        }
        Some(Reading { model, warnings, screen: panels.screen, days_left: panels.days, at: *at, eater_inferred: false })
    }

    /// The meal picked on the Consume target screen on show: the only target
    /// the engine offers there, or else the advised one.
    /// What a pick on `screen` that opens now came after: a Consume once a
    /// Consume target screen was read since the manage screen, else a
    /// Replace; the patrol when no manage screen was read since it (or no
    /// board at all, for an instrument pick). A treasure with nothing read
    /// before it is taken to follow a Consume, as the reader takes it.
    /// A pick opens on `screen`: what it came after, and the board it covers.
    /// After anything but a Consume (whose board [`vaulted`] works out), a
    /// board read between screens since the last board screen is the one
    /// under the pick: a new day's instruments (Kin's Coin of Fate), a Replace
    /// or an incident change the board before the pick shows, and on
    /// 2026-10-28 those reads showed it just as View Prison Status did after.
    /// One a day further on is a new day's start, with nothing spent, after a
    /// Consume too: a treasure opening day 18 of a 2026-10-28 run covered the
    /// board of day 17's chain, whose eaters had left, until View Prison
    /// Status showed the one read in between.
    fn pick_opens(&mut self, screen: Screen) {
        let mut after = self.pick_after(screen);
        self.pick_seen = false;
        let consumed_before = |seen: &Model| self.consume.as_ref().is_some_and(|m| seen.day > m.day);
        // With no Consume under way, a pick taken to follow one has no board
        // of its own: a treasure the Interrogation Room's Bestow gave covered
        // a board before an Ultimate went to the vault, though a read in
        // between showed it gone, and its advice was a Crimson Vintage that
        // raised nothing (2026-10-30).
        if (after != PickAfter::Consume || self.consume.is_none() || self.glimpse.as_ref().is_some_and(consumed_before))
            && let Some(mut seen) = self.glimpse.take()
        {
            self.consuming = false;
            // The opening instruments: this run has not read a board yet (the previous run's was cleared at RunEnd),
            // day 1, before the patrol, all cells are always locked. The opening fade-in frame read the 13 locked
            // cells as empty cells and was recorded as a glimpse, so the instruments were solved on an almost empty
            // prison (2026-11-03 two runs: advice ev 1.9 and 2.3, against 6.5 for a normal opening). A board with
            // empty cells but no inmates on day 1 is impossible, so replace it with all locked; the target and day
            // count still use what was read.
            if screen == Screen::InstrumentPick
                && self.board.is_none()
                && seen.day == 1
                && !seen.cells.iter().any(|c| matches!(c, CellModel::Inmate { .. }))
            {
                let start = Model::blank();
                (seen.cells, seen.warden) = (start.cells, start.warden);
            }
            if self.board.as_ref().is_some_and(|b| seen.day > b.day) {
                after = PickAfter::DayStart;
            } else {
                // The day goes on to its decisions after the pick.
                seen.step = Step::Manage;
            }
            self.board = Some(seen);
            self.consume = None;
            self.meal = None;
            self.meal_doubt.clear();
            self.pick_seen = true;
        }
        self.pick_after = Some(after);
    }

    fn pick_after(&self, screen: Screen) -> PickAfter {
        match &self.consume {
            Some(_) if self.consuming => PickAfter::Consume,
            Some(m) => PickAfter::Replace { hunt_used: m.hunt_used },
            None if self.board.as_ref().is_some_and(|b| b.step == Step::Patrol) => PickAfter::Patrol,
            None if self.board.is_none() && screen == Screen::InstrumentPick => PickAfter::Patrol,
            None => PickAfter::Consume,
        }
    }

    /// The targets the last solve offered: the chain's next bites, or the day's first, as the board allows them before
    /// they are narrowed to the jaws read (see `offered_wide`). Narrowed to jaws read in part, they were one cell, the
    /// meal was taken for that one for sure, and the player could have eaten another (see [`meal_read`]).
    fn offered(&self) -> Option<Vec<usize>> {
        let st = self.state.as_ref()?;
        if let Some(wide) = &self.offered_wide {
            return Some(wide.clone());
        }
        let chain = st.pending.iter().find_map(|p| match p {
            Pending::Chain { options, .. } => Some(options.clone()),
            _ => None,
        });
        Some(chain.unwrap_or_else(|| start_options(st).iter().map(|o| o.cell).collect()))
    }

    /// The meal of a bite the game's log told of, by a Consume target read after it (`read`, the reader's warnings
    /// `warns`, the jaws `jaws`, model indices) against the board before it (`before`): the meal picked (see
    /// [`Session::meal_picked`]), or the other target the read shows eaten instead (see [`meal_read`]). When the read
    /// shows the meal advised uneaten but not which target went, the meal is None and the targets it may be are given:
    /// the meal is in doubt. Otherwise they are none.
    fn meal_shown(&self, before: &Model, read: &Model, warns: &[Warning], jaws: &[usize]) -> (Option<Meal>, Vec<usize>) {
        let Some(picked) = self.meal_picked() else { return (None, Vec::new()) };
        let offered = self.offered().unwrap_or_default();
        let unedited = |n: usize| !self.edits.contains_key(&(n as u8 + 1));
        match meal_read(before, read, warns, jaws, &offered, picked.cell, unedited) {
            MealRead::Advised => (Some(picked), Vec::new()),
            MealRead::Other(cell) => (Some(Meal { cell, certain: true }), Vec::new()),
            MealRead::Unknown(rest) => (None, rest),
        }
    }

    fn meal_picked(&self) -> Option<Meal> {
        let offered = self.offered()?;
        if let [cell] = offered[..] {
            return Some(Meal { cell, certain: true });
        }
        match self.advice()?.action {
            Action::Devour { to } | Action::Chain { to } => Some(Meal { cell: to, certain: false }),
            _ => None,
        }
    }

    /// A treasure in a row opened with no board shown since the last one,
    /// whose advised gift was the Crimson Wine: its tiers go on the board
    /// under the new pick, as the engine plays it (every inmate below
    /// Legendary one tier up), the advice taken as the gift picked. True
    /// when it did. The other gifts' effects are random, and only View
    /// Prison Status shows them.
    fn took_wine(&mut self) -> bool {
        let wine = Action::Choose { id: ChoiceId::Name(Gift::Wine.key().to_string()) };
        if self.advice().is_none_or(|a| a.action != wine) {
            return false;
        }
        let Some(mut board) = self.reading.as_ref().map(|r| r.model.clone()) else { return false };
        for cell in &mut board.cells {
            if let CellModel::Inmate { tier, .. } = cell
                && *tier < LEGENDARY
            {
                *tier += 1;
            }
        }
        self.board = Some(board);
        self.consume = None;
        self.meal = None;
        self.meal_doubt.clear();
        if self.unseen_gifts.last() == Some(&Gift::Wine) {
            self.unseen_gifts.pop();
        }
        true
    }

    /// The game's log says a Warden's Treasure's gift was confirmed: the run
    /// end counts the Chromatic Prisoners vaulted by them too.
    pub fn note_treasure(&mut self) {
        if self.suspended {
            return;
        }
        self.gifts_taken += 1;
        self.gift_unanswered = true;
        if self.reading.as_ref().is_some_and(|r| r.screen == Screen::TreasurePick)
            && let Some(Action::Choose { id: ChoiceId::Name(key) }) = &self.shown
            && let Some(gift) = Gift::from_key(key)
        {
            self.unseen_gifts.push(gift);
        }
    }

    /// The game's log says day `cur` of `max` began. A patrol read before
    /// it, its countdown still on the day before (see [`Session::take_read`]),
    /// moves to the day and is solved again.
    pub fn note_day(&mut self, cur: u32, max: u32) {
        if self.suspended {
            return;
        }
        // "cur=0 max=0" comes as a finished run is abandoned for the next:
        // no run is under way, and no countdown reads 0 for it.
        if max == 0 {
            self.log_day = None;
            return;
        }
        self.log_day = Some((cur, max));
        // The log writes the day as the banner goes and the day's doors light up.
        self.fresh_at = Some(std::time::Instant::now() + DOORS_SHOW);
        self.day_at = Some(std::time::Instant::now());
        let Some(r) = &mut self.reading else { return };
        if cur >= 1 && r.screen == Screen::Patrol && r.model.max_days == i64::from(max) && r.model.day + 1 == i64::from(cur) {
            r.model.day = i64::from(cur);
            r.days_left = max.checked_sub(cur);
            self.days.left = r.days_left;
            self.solve();
        }
    }

    /// The buttons of the decision cards the game's log said came up
    /// (`cards`, 0 when it did not count them), for a read begun `since`
    /// after, while their names fade in too dark to read: End is always
    /// last, three are Consume, Replace and End, and two Consume or Replace
    /// by the left card's red art, or while it is too dark by whether the
    /// warden's inmate has a bite. None when that does not settle them: too
    /// soon (the board read 9 of 91 times wrong in the first 0.1 s, 3 of 223
    /// after, on 2026-10-29), the Interrogation Room's cards, Replace and Hunt
    /// both fitting (their cards look alike), or the art not clear yet.
    fn cards_shown(&self, scene: &Scene, panels: &Panels, since: std::time::Duration, cards: usize) -> Option<Vec<&'static str>> {
        if since < CARDS_BOARD || panels.days.is_none() {
            return None;
        }
        let warden = scene.warden.filter(|&w| usize::from(w) != data().room + 1 && w >= 2)?;
        // After a patrol the arrow leaves its cell and the door opens on an
        // inmate: a read without both is the board from before the move (a
        // 2026-10-29 read 0.26 s after the cards came up showed the warden
        // still on the patrol's cell, and its advice said to Replace there).
        if self.moved_from.is_some_and(|w| w == i64::from(warden) - 1) {
            return None;
        }
        let here = scene.cell(warden);
        if here.locked || here.inmate.is_none() {
            return None;
        }
        // Hunt joins only on a Lion's cell with the Blood-Stained Trap held,
        // between Replace and End, and its card looks like Replace's: where
        // either fits, the names decide.
        let hunt = self.tools.contains(&Tool::Trap) && here.inmate.is_some_and(|(beast, _)| beast == Beast::Lion);
        let red = |n: usize| panels.card_red[card_places(n)[0]];
        match (cards, hunt) {
            (1, _) => Some(vec!["End"]),
            // The left card's art shows its colour only from about 0.3 s on;
            // before that the board tells (2026-10-30: 14 of 20 decisions had
            // two cards, recognised 0.2 s later than three).
            (2, _) => match red(2).or_else(|| (!hunt).then(|| self.bite_here(scene, panels))) {
                Some(true) => Some(vec!["Consume", "End"]),
                Some(false) if !hunt => Some(vec!["Replace", "End"]),
                _ => None,
            },
            (3, false) => Some(vec!["Consume", "Replace", "End"]),
            (3, true) => (!red(3)?).then(|| vec!["Replace", "Hunt", "End"]),
            (4, _) => Some(vec!["Consume", "Replace", "Hunt", "End"]),
            _ => None,
        }
    }

    /// Whether the warden's inmate has a bite on this board: the game shows
    /// the Consume card just then (932 of 938 manage reads to 2026-10-30;
    /// the others had both moves spent, one card, or a misread cell).
    fn bite_here(&self, scene: &Scene, panels: &Panels) -> bool {
        let mut panels = panels.clone();
        (panels.screen, panels.decision) = (Screen::Manage, vec!["Consume", "End"]);
        let model = to_model(scene, &panels, &self.tools, self.board.as_ref(), self.run_days()).0;
        !start_options(&build_state(&model, SEED).0).is_empty()
    }

    /// Incident `id` fired, by its popup read or the game's log: a
    /// Usurpation stage beyond the last seen.
    fn note_usurpation(&mut self, id: u32) {
        if (USURPATION_I..USURPATION_I + 4).contains(&id) && self.usurped.is_none_or(|seen| id > seen) {
            self.usurped = Some(id);
        }
    }

    /// What else the game's log says happened (see [`Signal`]). Each comes
    /// 30 to 130 ms after the click or sound, before any read of what it
    /// changed.
    /// - An objective completed: a pick, often the instrument pick it opens,
    ///   hides its panel, and the reading kept its count until the board
    ///   read again, 0.7 to 6.7 s on (2026-10-28).
    /// - Consume pressed: a pick now follows a Consume though no target
    ///   screen read; ended with no bite, it was not spent.
    /// - A bite: the next Consume target screen is a chain's, and the meal
    ///   advised was eaten, whatever the read shows.
    /// - A run's start: a run still under way missed its riot screen.
    /// - The riot screen: the run is over, read or not.
    /// - The decision cards came up: a read during their fade waits.
    /// - Chain Reaction fired: the next door's path opens; a door clicked
    ///   spends it.
    pub fn note_game(&mut self, signal: Signal) {
        if self.suspended {
            return;
        }
        match signal {
            // The objective completes on a move, a bite or an incident: the
            // reading on show is already played. Solved again it moved its
            // advice after the player (2026-10-29: a patrol taken said "Go to
            // the Interrogation Room" until the instrument pick read), so the
            // next read takes it.
            Signal::ObjectiveDone => {
                self.objective_done = true;
                if let Some(r) = &mut self.reading {
                    complete_objective(&mut r.model);
                }
            }
            Signal::Consume => {
                self.changers += 1;
                let logged = self.log_bites.map_or(0, |(logged, _)| logged);
                self.log_bites.get_or_insert((0, 0));
                self.bites_at_consume = Some(logged);
                // A new devour has not bitten yet: the board from the previous chain's bite does not count.
                self.bite_cells = None;
                self.consumed_on = self.log_day.map(|(cur, _)| cur);
                if self.consume.is_some() {
                    self.consuming = true;
                }
                self.fresh_at = Some(std::time::Instant::now() + CONSUME_SHOWS);
            }
            // The meal on show was eaten: its mark goes until the next target
            // reads (see `BITE_SHOWS`).
            Signal::Bite => {
                self.changers += 1;
                let (logged, read) = self.log_bites.unwrap_or((0, 0));
                self.log_bites = Some((logged + 1, read));
                self.bite_at = Some(std::time::Instant::now());
                self.bite_cells = self.consume.as_ref().map(|m| m.cells.clone());
                self.differ_steady = false;
                self.fresh_at = self.bite_at.map(|at| at + BITE_READABLE);
                self.held = None;
                self.inferred_eater = None;
                self.inferred_from = None;
                self.eater_seen = None;
                self.manage_bite = None;
                // In a Cage Fight the chain's badges number the bites ahead:
                // the one on the meal just eaten goes at once, and the mark,
                // left on that meal, hides like any other theme's until a
                // read shows the bite. It named the meal just eaten again,
                // then the next, after 7 bites of a 2026-10-31 run.
                self.eaten = None;
                if self.theme == Some(CAGE_FIGHT) && !self.chain_follow.is_empty() {
                    let eaten = self.chain_follow.remove(0);
                    self.eaten = Some(eaten);
                    if let Some((_, plan)) = &mut self.chain_plan {
                        plan.retain(|&n| n != eaten);
                    }
                }
                self.played_at = self.bite_at;
            }
            Signal::ChainEnd => {
                let at = self.bites_at_consume.take();
                if at.is_some() && self.log_bites.map(|(logged, _)| logged) == at {
                    self.consuming = false;
                }
            }
            // A pick opening right after a gift, the treasure's picks all
            // taken, is a new treasure in a row. The last pick, fading out,
            // read as one (2026-10-29: a Three-Headed Thumb Ring's treasure
            // warned of a next that never came).
            Signal::TreasureOpen => {
                self.pick_opened = Some(std::time::Instant::now());
                if std::mem::take(&mut self.gift_unanswered) && self.gifts_taken.is_multiple_of(treasure_picks(self.theme) as usize) {
                    self.treasure_reopened = true;
                }
            }
            Signal::CellPicked if !self.reading.as_ref().is_some_and(|r| r.screen == Screen::CellPick) => {}
            Signal::Decision | Signal::CellPicked => {
                // A decision on the manage screen may be a Replace.
                if self.reading.as_ref().is_some_and(|r| r.screen == Screen::Manage) {
                    self.changers += 1;
                }
                self.held = None;
                self.inferred_eater = None;
                self.inferred_from = None;
                self.eater_seen = None;
                self.played_at = Some(std::time::Instant::now());
                self.cards_up = None;
                self.fresh_at = None;
                self.cards_seen.clear();
            }
            // When the cards came up, by the log line's stamp: the log comes
            // 30 to 130 ms behind, and a read begun 0.1 s after the cards
            // counted as begun with them (2026-10-30).
            Signal::CardsUp(cards, at) => {
                let now = std::time::Instant::now();
                self.cards_up = Some((now.checked_sub(gamelog::age(at)).unwrap_or(now), cards));
                self.fresh_at = self.cards_up.map(|(up, _)| up + CARDS_BOARD);
                self.cards_seen.clear();
            }
            // The reader never sees Chain Reaction's effect waiting, so the
            // patrols after it went by doors alone (2026-10-29: days 6 and 10
            // of a run). The popup is often gone before a read.
            Signal::Incident(CHAIN_REACTION) => self.chain_armed = true,
            Signal::Incident(id) => self.note_usurpation(id),
            // An instrument acting as the day starts changes the board under
            // the doors: the Wriggling Iron Box's Rat came in after a read
            // that advised another door, and the advice moved as the player
            // clicked it (2026-11-01, day 16).
            Signal::Vaulted => self.vault_flies += 1,
            Signal::VaultLit => self.vault_lights += 1,
            Signal::Instrument => {
                if self.day_at.is_some_and(|at| at.elapsed() < DAY_EFFECT) {
                    let until = std::time::Instant::now() + EFFECT_SHOWS;
                    self.effect_until = Some(until);
                    self.fresh_at = Some(self.fresh_at.map_or(until, |at| at.max(until)));
                }
            }
            Signal::Reroll => {
                let offer = self.full_offer.take().map(|(offer, _, _)| offer);
                self.rerolled = offer.map(|offer| (offer, std::time::Instant::now()));
                self.short_offer = None;
            }
            Signal::Moved => {
                self.chain_armed = false;
                self.manage_bite = None;
                self.moved_from = self.reading.as_ref().map(|r| r.model.warden);
            }
            Signal::RunStart if self.reading.is_some() && self.run_over.is_none() => self.end_run(),
            Signal::RunEnd if self.reading.is_some() => self.end_run(),
            _ => {}
        }
    }

    /// The cells, as model indices and in order, of the chain the advised
    /// Consume or chain bite starts, that bite first. Only in a Cage Fight
    /// run: there the eater keeps its beast and its rarity only rises, so
    /// the targets after the next bite are known before it, and the player
    /// can click them in a row rather than wait for a read after each bite
    /// (the theme's long chains took much of a 2026-10-28 run).
    pub fn chain_plan(&self) -> &[usize] {
        let plan = self.chain_plan.as_ref().map_or(&[][..], |(_, plan)| plan.as_slice());
        // The plan carries over past the eater's rank-up (see `note_chain_plan`: if the advice is unchanged it is not
        // re-ordered), and the tail may be longer than the number of bites the eater can still take: on 2026-11-03
        // day 6 with a Rare Berserk Lion 04 -> 08 was planned, later it was read as Ultimate and still carried 08,
        // and after biting 04 the Chromatic flew into the vault. Cut to `bites_left`, so the next bite is unchanged.
        let left = self.bites_left().map_or(plan.len(), |n| n.max(1));
        &plan[..plan.len().min(left)]
    }

    /// How many more bites the eater can take in Cage Fight, given only when certain: each bite raises it at least
    /// one tier (`sure_gain`), and an Ultimate that bites once more flies into the vault as a Chromatic, so an eater
    /// read as Ultimate has 1 bite. Lower rarities are not cut: after the bite the eater will read one tier higher
    /// (2026-11-01 a Rare Colossal Rat read as Legendary, 3 frames in a row), and cutting by what was read would hide
    /// the number of the bite that was actually taken later; cutting only Ultimate got 6 chains right in 17 Cage
    /// Fight runs, with no wrong cuts. Not cut when it is not Cage Fight, when the Elephant-Foot Maul's Mammoth may
    /// drop a rank, when the Fanged Guillotine's Wolf eating a lower rank may fail to rank up, or when the vault is
    /// full so the eater stays at Ultimate and does not fly away.
    fn bites_left(&self) -> Option<usize> {
        let st = self.state.as_ref().filter(|_| self.theme == Some(CAGE_FIGHT))?;
        if st.top_tier() < RAINBOW {
            return None;
        }
        let eater = st.cells.get(st.position)?.inmate?;
        if (st.has(Tool::Maul) && eater.beast == Beast::Mammoth) || (st.has(Tool::Guillotine) && eater.beast == Beast::Wolf) {
            return None;
        }
        (eater.tier == RAINBOW - 1).then_some(1)
    }

    /// Where the chain's targets are (see [`Session::chain_plan`]), in order,
    /// as [`Session::mark`] gives a target: centre and half size in frame
    /// px, with the number its badge shows. The numbers stay as the bites
    /// go: renumbered from 1 after each bite, they read as another chain
    /// (the player, 2026-10-31). Empty for a chain of one bite, or while
    /// the screen on show is not the one read.
    pub fn chain_spots(&self) -> Vec<(usize, (f32, f32, f32, f32))> {
        let (Some((_, panels, _)), Some(r), Some(scale)) = (&self.scene, &self.reading, self.ui_scale) else { return Vec::new() };
        if self.suspended || !self.on_screen || panels.screen != r.screen || !matches!(r.screen, Screen::Manage | Screen::ChainTarget) {
            return Vec::new();
        }
        // The board's cells stand where a Consume target pick shows them.
        self.chain_badges()
            .into_iter()
            .filter_map(|(to, number)| Some((number, click::target(Screen::ChainTarget, &panels.spots, &Action::Chain { to })?)))
            .map(|(number, c)| (number, click::to_frame(&c, self.size.0, self.size.1, scale)))
            .collect()
    }

    /// The chain's bites still to come (see [`Session::chain_plan`]), as
    /// model indices, with the numbers their badges show; empty for a chain
    /// planned with one bite.
    pub fn chain_badges(&self) -> Vec<(usize, usize)> {
        if self.chain_numbers.len() < 2 {
            return Vec::new();
        }
        let number = |k: usize, n: usize| self.chain_numbers.iter().find(|&&(cell, _)| cell == n).map_or(k + 1, |&(_, number)| number);
        self.chain_plan().iter().enumerate().map(|(k, &n)| (n, number(k, n))).collect()
    }

    /// The line naming a chain of more than one bite (see [`Session::chain_plan`]).
    pub fn chain_line(&self) -> Option<String> {
        let plan = self.chain_plan();
        (plan.len() > 1).then(|| trf!("Chain order: {}", plan.iter().map(|&n| cell_label(n)).collect::<Vec<_>>().join(" → ")))
    }

    /// Works out the advice's chain when the advice changed.
    fn note_chain_plan(&mut self) {
        // While the playouts run, the chain on show stands as long as its
        // next bite is still on offer: the strategy's pick and each
        // progress's leader took turns among near ties, and the badges
        // jumped four times in 0.15 s (2026-10-31).
        if !self.settled() && self.plan_offered() {
            return;
        }
        // A solve of a board read before the last bite is behind the game:
        // the chain on show, the bite taken off, stands. Planned again, it
        // named the cell just eaten and renumbered the rest (badges 3 and 4
        // turned 4 and 5, then back, 2026-10-31).
        if self.reading.as_ref().zip(self.bite_at).is_some_and(|(r, bite)| r.at.elapsed().unwrap_or_default() > bite.elapsed()) {
            return;
        }
        let action = self.advice().map(|a| a.action);
        if self.chain_plan.as_ref().map(|(a, _)| a) == action.as_ref() {
            return;
        }
        // Nor one read while the bite still plays: the meal just eaten
        // shows on the board for about 0.5 s, and a plan led by it went
        // within 0.5 s (badges 3 and 4 on 15 and 07 after 15 was eaten,
        // 2026-10-31). A door the Casket opened there shows past it. The
        // meal eaten is the plan's next only if the player kept to it: a
        // planned cell the board shows empty was eaten instead.
        let fresh_bite = self.bite_at.is_some_and(|at| at.elapsed() < BITE_SHOWS);
        let unseen = self.reading.as_ref().is_some_and(|r| self.chain_follow.iter().all(|&n| matches!(r.model.cells.get(n), Some(CellModel::Inmate { .. }))));
        if fresh_bite
            && unseen
            && let (Some(eaten), Some(a)) = (self.eaten, &action)
            && self.plan_chain(a).contains(&eaten)
        {
            return;
        }
        let plan = match &action {
            // The chain planned at its start stands while the advice keeps to it.
            Some(Action::Chain { to }) if self.chain_next() == Some(*to) => self.chain_rest(),
            Some(a) => {
                let plan = self.plan_chain(a);
                self.chain_follow = plan.clone();
                // The badges count on from the bites this chain already took.
                // Only in the chain: the count since the last Consume ran on
                // past a chain that ended in the vault, and a plan on the next
                // manage screen started at 3.
                let taken = if self.chained { self.log_bites.zip(self.bites_at_consume).map_or(0, |((logged, _), at)| logged.saturating_sub(at)) as usize } else { 0 };
                self.chain_numbers = plan.iter().enumerate().map(|(k, &n)| (n, taken + k + 1)).collect();
                plan
            }
            None => Vec::new(),
        };
        self.chain_plan = action.map(|a| (a, plan));
    }

    /// Whether the chain on show (see [`Session::chain_plan`]) starts with a
    /// bite the state solved offers.
    fn plan_offered(&self) -> bool {
        let (Some((_, plan)), Some(st)) = (&self.chain_plan, &self.state) else { return false };
        if !self.reading.as_ref().is_some_and(|r| matches!(r.screen, Screen::Manage | Screen::ChainTarget)) {
            return false;
        }
        let Some(&next) = plan.first() else { return false };
        let chain = st.pending.iter().find_map(|p| match p {
            Pending::Chain { options, .. } => Some(options.clone()),
            _ => None,
        });
        chain.unwrap_or_else(|| start_options(st).iter().map(|o| o.cell).collect()).contains(&next)
    }

    /// The bites of the planned chain still to come: its cells that still
    /// hold an inmate on the board read, in order.
    fn chain_rest(&self) -> Vec<usize> {
        let Some(r) = self.reading.as_ref() else { return Vec::new() };
        self.chain_follow.iter().copied().filter(|&n| matches!(r.model.cells.get(n), Some(CellModel::Inmate { .. }))).collect()
    }

    /// The planned chain's next bite, see [`Session::chain_rest`].
    fn chain_next(&self) -> Option<usize> {
        self.chain_rest().first().copied()
    }

    /// `a`, or the planned chain's next bite (see [`Session::chain_follow`])
    /// when `a` is another bite and the playouts find the planned one about
    /// as good: re-solved after each bite, the advice moved between such
    /// bites, and with it the numbers the player was clicking in a row
    /// (2026-10-28).
    ///
    /// A Cage Fight's chain is clicked by its badges: there the planned bite
    /// stands, ending the chain too, while it rates within
    /// [`CAGE_PLAN_SLACK`] of the best. Re-ranked after each bite among
    /// near ties, the chain was planned again and the badges' numbers moved
    /// (2026-10-31: 05, 14, 07 became 12, 14 after the first bite, the
    /// playouts rating 12 0.01 above 14).
    fn keep_to_chain(&self, a: Advice) -> Advice {
        let cage = self.theme == Some(CAGE_FIGHT);
        let Some(next) = self.chain_next() else { return a };
        let to = match a.action {
            Action::Chain { to } => to,
            Action::EndChain if cage => usize::MAX,
            _ => return a,
        };
        if to == next {
            return a;
        }
        let planned = Action::Chain { to: next };
        let Some(k) = self.ranking.iter().position(|o| o.action == planned) else { return a };
        let close = also_good(&self.ranking, a.option).contains(&k)
            || (cage && a.option.is_some_and(|best| self.ranking[best].mean - self.ranking[k].mean <= CAGE_PLAN_SLACK));
        if !close {
            return a;
        }
        Advice { action: planned, label: self.ranking[k].label.clone(), why: a.why, option: Some(k) }
    }

    /// `a`, or the best chain bite the playouts rate about as good when `a`
    /// ends the chain: ending it gives the day's Consume back, so such a bite
    /// is the same meal in one click. The advice said "End the chain", then
    /// "Consume 15" on the cards that came back (2026-10-30, day 1).
    fn prefer_bite(&self, a: Advice) -> Advice {
        if a.action != Action::EndChain {
            return a;
        }
        match also_good(&self.ranking, a.option).into_iter().find(|&k| matches!(self.ranking[k].action, Action::Chain { .. })) {
            Some(k) => Advice { action: self.ranking[k].action.clone(), label: self.ranking[k].label.clone(), why: a.why, option: Some(k) },
            None => a,
        }
    }

    /// The chain `action` starts, played on with the strategy's bites (see
    /// [`instant`]) on a copy of the solved state, whose random draws are the
    /// same each time, so the line holds still. Empty outside a Cage Fight.
    fn plan_chain(&self, action: &Action) -> Vec<usize> {
        let (Some(st), Action::Devour { to } | Action::Chain { to }) = (self.state.as_ref(), action) else { return Vec::new() };
        if self.theme != Some(CAGE_FIGHT) {
            return Vec::new();
        }
        let mut st = st.clone();
        // The Avenger's Casket's door is a roll, not a target to plan: rolled
        // here, it put a cell the chain had eaten in the plan again (badges
        // 15 and 15 on 2026-10-31).
        st.tools.retain(|&t| t != Tool::Revenge);
        let mut plan = vec![*to];
        if !apply(&mut st, action).is_ok() {
            return Vec::new();
        }
        while plan.len() < CHAIN_PLAN_MAX {
            match instant(&st).map(|i| i.action) {
                Some(next @ Action::Chain { to }) if apply(&mut st, &next).is_ok() => plan.push(to),
                _ => break,
            }
        }
        plan
    }

    /// What the window recommends now; see [`advise`].
    pub fn advice(&self) -> Option<Advice> {
        let a = advise(self.pick.as_ref(), &self.ranking, self.live())?;
        let a = self.keep_shown(self.prefer_bite(self.keep_to_chain(a)));
        // A treasure that changes nothing gives way to a tied one that acts,
        // after the advice on show is kept: kept, the Three-Headed Thumb Ring
        // a treasure before advised came back on a board with no locked cell
        // left, over the Crimson Vintage tied with it (2026-11-02).
        Some(match &self.state {
            Some(st) => prefer_change(st, &self.ranking, a),
            None => a,
        })
    }

    /// The advice to show: none until the playouts settle it. The strategy's
    /// own pick, shown before the playouts counted, gave way to their best in
    /// 19 of 44 decisions of a 2026-10-30 run, about 0.05 s later: the advice
    /// changed under the player's eyes. See [`SHOW_MIN`].
    pub fn shown_advice(&self) -> Option<Advice> {
        // A jawless read held in a chain over a reading from before the bite (see `jawless_held`): that reading's
        // advice may name the meal just eaten, and the player clicks within 0.1 s.
        if self.jawless_held == Some(true) {
            return None;
        }
        // Nor while no read was taken since a bite the game's log told of (a read held as the bite does not show
        // yet, see BITE_UNSHOWN): the target screen's reading on show is the board before it, and its advice names
        // the meal just eaten. Its mark hid, but the advice stood for up to BITE_SHOWS. Any target screen read taken
        // counts the bite as read (see `take_read`), so advice on a read since the bite is not hidden. A Cage Fight's
        // badges number the bites ahead and stay on show (see `take_read`).
        if self.reading.as_ref().is_some_and(|r| r.screen == Screen::ChainTarget)
            && self.log_bites.is_some_and(|(logged, read)| logged > read)
            && self.theme != Some(CAGE_FIGHT)
        {
            return None;
        }
        // Just after a bite, a board read whose targets differ from the
        // game's marks is still changing (the eater's rise, a door the
        // Avenger's Casket opened glowing in): its advice gave way within
        // 0.1 s in every chain flip of three casket runs (2026-10-31). None
        // until a read agrees, for BITE_SHOWS at most, or the same board
        // reads again past DIFFER_STEADY.
        if self.reading.as_ref().is_some_and(|r| r.screen == Screen::ChainTarget && r.warnings.iter().any(|w| begins(&w.text, TARGETS_DIFFER)))
            && self.bite_at.is_some_and(|at| at.elapsed() < BITE_SHOWS)
            && !self.differ_steady
        {
            return None;
        }
        if self.settled() {
            return self.advice();
        }
        // The day's first bite, settled on the manage screen, is the same
        // pick on its target screen: solved again there from nothing, a
        // Cage Fight's took over 1 s and the player bit first (2026-10-31).
        if let (Some(r), Some(st), Some(a)) = (&self.reading, &self.state, &self.manage_bite)
            && r.screen == Screen::ChainTarget
            && !self.chained
            && let Action::Devour { to } = a.action
            && start_options(st).iter().any(|o| o.cell == to)
        {
            let option = self.ranking.iter().position(|o| o.action == a.action);
            return Some(Advice { option, ..a.clone() });
        }
        // A read of the same screen and day solves again: the advice on show
        // stays until the new one settles. Hidden meanwhile, the mark went
        // off and on as a patrol's cards read in (2026-10-30, 13 times in a
        // run).
        // Only on the same warden's cell, inmate and moves spent: a Replace
        // played changes them, and its mark would sit where Consume may be
        // now, before the game's log tells of the click (30 to 60 ms on).
        let r = self.reading.as_ref()?;
        let (key, held) = self.held.as_ref()?;
        let legal = self.ranking.is_empty() || self.ranking.iter().any(|o| o.action == held.action);
        // Its row is the new ranking's: the old row's index once crashed the
        // window's list of moves while the new ranking was still empty (a
        // Hunt's prey pick, 2026-10-30).
        let option = self.ranking.iter().position(|o| o.action == held.action);
        let now = held_key(r, self.gifts_taken);
        // Off the manage screen, a move still open on the board read stays
        // too: an eater risen on a chain's target screen changed the warden's
        // cell, and the mark went off for up to 0.23 s and came back on the
        // same move, 8 times in 7 runs and never another (2026-11-01).
        let still_open = || {
            r.screen != Screen::Manage
                && (key.0, key.1, key.4, key.5) == (now.0, now.1, now.4, now.5)
                && self.state.as_ref().is_some_and(|st| apply(&mut st.clone(), &held.action).is_ok())
        };
        ((*key == now || still_open()) && legal).then(|| Advice { option, ..held.clone() })
    }

    /// Whether the advice is settled enough to show: the solve is done, or
    /// past [`SHOW_MIN`] playouts its best move, clear of the others' margin
    /// of error, held from one progress to the next.
    fn settled(&self) -> bool {
        !self.solving || self.advice_steady
    }

    /// Notes the move advised once the playouts count, see [`Session::keep_shown`].
    fn note_shown(&mut self) {
        if self.live()
            && let Some(a) = self.advice()
        {
            self.shown = Some(a.action.clone());
            if self.reading.as_ref().is_some_and(|r| r.screen == Screen::Manage) {
                self.manage_bite = matches!(a.action, Action::Devour { .. }).then(|| a.clone());
            }
            // A move played since the reading's read is not held again: the
            // board before a bite, its eater not risen yet, gave the next
            // read its key, and the advice to eat the cell just eaten showed
            // again for 15 ms (2026-10-31, twice in a run).
            // Nor is a read whose targets differ from the game's marks (see
            // `shown_advice`): never shown, it was once the eater risen read
            // a chain target still open, and its bite showed for 0.16 s
            // before the right one (2026-11-01, day 12).
            // Nor a treasure's pick gone by (see `stale_pick`).
            // Nor an eater taken from the jaws (see `Reading::eater_inferred`):
            // held, its End Chain stayed on show with its mark while the
            // eater, read a Lion, solved again, and the mark went to 09 after.
            if self.settled()
                && self.played_at.is_none()
                && let Some(r) = &self.reading
                && !r.eater_inferred
                && !r.warnings.iter().any(|w| begins(&w.text, TARGETS_DIFFER))
                && !self.stale_pick(r)
            {
                self.held = Some((held_key(r, self.gifts_taken), a));
            }
        }
    }

    /// A treasure's pick read before it opened, or after its gift was
    /// confirmed: the pick fading out reads as the one on show for 0.4 s,
    /// and its gift, held then, showed on the next treasure in a row for up
    /// to 0.09 s (2026-11-01, six times in twelve runs).
    fn stale_pick(&self, r: &Reading) -> bool {
        r.screen == Screen::TreasurePick && (self.gift_unanswered || self.pick_opened.is_some_and(|open| self.reading_scan.is_none_or(|scan| scan < open)))
    }

    /// Whether the playouts count yet, see [`advise`].
    fn live(&self) -> bool {
        !self.solving || self.rounds >= LIVE_MIN
    }

    /// `a`, or the move the advice showed before (see [`Session::shown`])
    /// while the playouts rate it as good: re-solved after each read, two
    /// moves within the margin of error took turns (the day's first bite on
    /// the manage screen and on its target screen, 2026-10-28).
    fn keep_shown(&self, a: Advice) -> Advice {
        let Some(shown) = self.shown.as_ref().filter(|&s| *s != a.action) else { return a };
        // A Hunt advised on the manage screen is the same prey on its pick:
        // there "Hunt 15" turned into "Hunt 11", within the margin of error
        // (2026-10-30).
        let same = |action: &Action| match shown {
            Action::Hunt { cell } => *action == Action::Choose { id: ChoiceId::Cell(*cell) },
            _ => action == shown,
        };
        // On the manage screen a move on the card shown keeps its target while
        // the playouts run: past SHOW_MIN such moves show early (see
        // `Session::poll`), and "Hunt 09" gave way to "Hunt 12", the same
        // card and a Mammoth alike, 0.06 s later (2026-11-02, 6 times in 8
        // runs). The solve's end rates it as ever. Only a move shown on this
        // very screen (its held key): `shown` outlives the day.
        let here = |r: &Reading| self.held.as_ref().is_some_and(|(key, held)| *key == held_key(r, self.gifts_taken) && held.action == *shown);
        let manage = self.solving && self.reading.as_ref().is_some_and(|r| r.screen == Screen::Manage && here(r)) && same_card(shown, &a.action);
        let kept = if manage { (0..self.ranking.len()).find(|&k| same(&self.ranking[k].action)) } else { also_good(&self.ranking, a.option).into_iter().find(|&k| same(&self.ranking[k].action)) };
        match kept {
            Some(k) => Advice { action: self.ranking[k].action.clone(), label: self.ranking[k].label.clone(), why: a.why, option: Some(k) },
            None => a,
        }
    }

    /// Where the player clicks for the advice: the target's centre, half
    /// width and half height in the game frame's pixels. None while no advice
    /// shows (see [`Session::shown_advice`]), for a reading from an opened file, and while the game shows
    /// another screen than the reading's (a read since was kept or ended the
    /// run).
    pub fn mark(&self) -> Option<(f32, f32, f32, f32)> {
        self.mark_or_why().ok()
    }

    /// [`Session::mark`]; when there is no mark it gives the reason, logged to the view row (2026-11-03 a piece of
    /// advice came out and the box appeared only 9 s later, and the log could not tell why).
    pub fn mark_or_why(&self) -> Result<(f32, f32, f32, f32), &'static str> {
        let (Some((_, panels, _)), Some(r)) = (&self.scene, &self.reading) else { return Err("unread") };
        if self.suspended {
            return Err("suspended");
        }
        if !self.on_screen {
            return Err("off_screen");
        }
        if panels.screen != r.screen {
            return Err("screen");
        }
        // A move played leaves its mark where the next screen may put
        // another button: a Replace's card became Consume, and a click there
        // 0.2 s after the advice moved to End consumed (2026-10-29, twice).
        if self.played_at.is_some_and(|at| at.elapsed() < PLAYED_HIDE) {
            return Err("played");
        }
        // An eater taken from the jaws may read as another beast once its
        // portrait shows, and the advice with it (see `Reading::eater_inferred`).
        if r.eater_inferred {
            return Err("inferred");
        }
        let advice = self.shown_advice().ok_or("no_advice")?;
        // On the target screen the chain's badges number this bite 1
        // already: the mark on it as well doubled it (2026-10-31). The
        // manage screen's mark is on the Consume card, not a cell.
        if panels.screen == Screen::ChainTarget
            && let Action::Devour { to } | Action::Chain { to } = advice.action
            && self.chain_plan().first() == Some(&to)
            && !self.chain_spots().is_empty()
        {
            return Err("badges");
        }
        let click = click::target(panels.screen, &panels.spots, &advice.action).ok_or("target")?;
        Ok(click::to_frame(&click, self.size.0, self.size.1, self.ui_scale.ok_or("scale")?))
    }

    /// The line the window shows under the advice `a`; see [`why_shown`].
    pub fn why_shown(&self, a: &Advice) -> String {
        match &self.state {
            Some(st) => why_shown(st, &self.ranking, a),
            None => a.why.clone(),
        }
    }

    /// Keeps the instrument advised on an instrument pick.
    fn note_advised_tool(&mut self) {
        if !self.pick_named
            && self.reading.as_ref().is_some_and(|r| r.screen == Screen::InstrumentPick)
            && let Some(advice) = self.advice()
        {
            // An advised Re-roll keeps the pick on show with a new offer:
            // nothing is held from it.
            self.advised_tool = match &advice.action {
                Action::Choose { id: ChoiceId::Name(key) } => Tool::from_key(key),
                _ => None,
            };
        }
    }

    /// Starts a solve of the current model on a worker thread, replacing any
    /// running one: rounds of [`ROUND`] playouts until `rollouts`, each
    /// reported to [`Session::poll`]. Dropping the old receiver stops the
    /// old worker at its next send.
    /// The game's jaws differ from when it issued the "target mismatch" warning: this read does not count as the same
    /// read, so solve again. After a bite, the jaws of the target the previous bite did not eat linger for another
    /// 0.66 s; if we kept using the warning from then, once the jaws cleared the advice would still be hidden, and
    /// the player clicked by themselves 1.03 s after the bite (2026-11-02 day 7). Likewise jaws read after a jawless
    /// read ended the chain (an empty `differ_jaws`), once two reads in a row show the same jaws (`jaws_last` holds
    /// the read before's): the reader looks for jaws on every frame, and one misread would flash a chain's advice.
    fn jaws_moved(&self) -> bool {
        let Some((scene, _, _)) = &self.scene else { return false };
        let now = jawed(scene);
        match &self.differ_jaws {
            Some(was) if was.is_empty() => !now.is_empty() && self.jaws_last.as_ref() == Some(&now),
            Some(was) => now != *was,
            None => false,
        }
    }

    pub fn solve(&mut self) {
        self.differ_jaws = None;
        // Only the solve right after one that took the eater from the jaws compares with it (see `inferred_from`).
        let inferred_before = self.inferred_from.take();
        let shown_plan = self.chain_plan.take();
        self.stop();
        // The chain on show stays through a read that solves to nothing (an
        // eater mid-rise read with no target): its badges went and came back
        // 0.2 s on (2026-10-31). A solve that runs checks it below.
        self.chain_plan = shown_plan;
        let Some(reading) = &self.reading else { return };
        let reading_pick = reading.screen == Screen::TreasurePick;
        let (mut st, problems) = build_state(&reading.model, SEED);
        if !problems.is_empty() {
            self.problems = problems;
            return;
        }
        st.theme = self.theme;
        // The Usurpation stages the run went through: each gives way to the
        // next, which only shows in the run so far.
        if let Some(last) = self.usurped {
            usurped(&mut st, last);
        }
        // Each treasure the game's log saw taken, and the one being picked,
        // follows a Chromatic vaulted: the pick hides the vault panel, and
        // the board under it can predate the latest. Day 20 of a 2026-10-28
        // run read 8 for 10, and the gift advised changed once View Prison
        // Status showed 10. Only the count matters to the solve.
        if reading.screen == Screen::TreasurePick {
            let vaulted = (self.gifts_taken / treasure_picks(self.theme) as usize + 1).max(self.vault_base + self.vaulted_seen()).min(VAULT_CAP as usize);
            while st.vault.len() < vaulted {
                st.vault.push(crate::engine::state::VaultEntry { id: st.next_id, beast: Beast::Rat, day: st.day });
                st.next_id += 1;
            }
        }
        // A treasure pick always offers all four gifts (build_state lists
        // them), for the picks the treasure has left.
        if let Some(Pending::Gift { picks, .. }) = st.pending.first_mut() {
            // With the game's log, the gifts it saw confirmed say how many of
            // the treasure's picks are gone: no board shows between an
            // Opulence treasure's picks to count them by.
            let taken = match self.log_day {
                Some(_) => (self.gifts_taken % treasure_picks(self.theme) as usize) as u32,
                None => self.treasure.unwrap_or(0),
            };
            *picks = treasure_picks(self.theme).saturating_sub(taken).max(1);
            // The treasures after this one in a row, each with all four gifts.
            for _ in 1..self.treasures {
                st.pending.insert(1, Pending::Gift { options: Gift::ALL.to_vec(), picks: treasure_picks(self.theme) });
            }
        }
        // A Consume target screen picks the day's first bite, or a chain's
        // next, among the cells the screen marks as targets.
        let target = reading.screen == Screen::ChainTarget;
        if target && self.chained {
            // The eater went to the vault (see `eater_vaulted`): the chain is
            // over, and the treasure comes next.
            // After a bite no jaws at all are on screen: the game has ended the chain (the eater became Chromatic and
            // flew into the vault, or an Ultimate had nothing to eat when the vault was full). The board's eater may
            // still be at its pre-bite rarity: the Crowded Beastcage made the cell just eaten open an inmate again at
            // once, so reads between the two bites were all treated as not yet drawn and held, the eater's cell was
            // read as empty and kept, and the solve then planned another chain (2026-11-03 day 7 "Chain on into 14").
            // Across the runs there were 144 target screens with no jaws within 3 s after a bite, and none of them
            // bit again.
            // A jawless read right after a bite, or with an instrument that refills a cell, is held once before it
            // gets here (see `take_read`).
            let no_jaws = self.scene.as_ref().is_some_and(|(scene, _, _)| jawed(scene).is_empty());
            let Some(eater) = st.cells[st.position].inmate.filter(|_| !self.eater_vaulted && !no_jaws) else {
                self.chain_plan = None;
                // The same jaws read twice in a row after this on the same board are no longer the same read (see
                // `jaws_moved`): the screen was still drawing them, and the chain goes on. Compared as the same read, the jaws on
                // cell 04 that came 0.13 s after two jawless reads were never solved, and the player waited 9.7 s
                // (2026-11-05 day 15).
                if no_jaws {
                    self.differ_jaws = Some(Vec::new());
                }
                return;
            };
            // The first bite has already been taken in the game: the board's swap_used is still the pre-bite value,
            // and the simulation must not swap again.
            st.used.bitten = true;
            offer_chain(&mut st, eater.id);
            // The game opens this screen only while the eater has a target, so
            // a board read that gives it none is wrong (the eater's rarity not
            // risen yet, a misread target) and gets no advice. So is one whose
            // targets miss every cell the game marks: on a dark day 12 of a
            // 2026-10-28 run the eater read as before its bite for 4 s, and
            // the advice named a cell the game did not mark.
            let jaws = self.scene.as_ref().map_or(Vec::new(), |(scene, _, _)| jawed(scene));
            let options = st.pending.iter().find_map(|p| match p {
                Pending::Chain { options, .. } => Some(options.clone()),
                _ => None,
            });
            let missed = options.as_ref().is_some_and(|o| !jaws.is_empty() && jaws.iter().all(|c| !o.contains(c)));
            let marked = options.as_ref().is_some_and(|o| !jaws.is_empty() && o.len() == jaws.len() && jaws.iter().all(|c| o.contains(c)));
            if marked && let Some(r) = &self.reading && !r.eater_inferred {
                self.eater_seen = r.model.cells.get(st.position).and_then(|c| eater_in(c, st.position));
            }
            if options.is_none() || missed {
                self.recheck = true;
                let mut before = st.clone();
                before.pending.retain(|p| !matches!(p, Pending::Chain { .. }));
                // A Cage Fight's bites keep the eater's beast: the day's board has it.
                let beast = self.board.as_ref().filter(|_| self.theme == Some(CAGE_FIGHT)).and_then(|b| match b.cells.get(st.position) {
                    Some(CellModel::Inmate { beast, .. }) => Some(*beast),
                    _ => None,
                });
                match eater_from_jaws(&before, &jaws, beast) {
                    Some((inferred, eater)) => {
                        // The same eater taken from the same jaws on two reads in a row, the later one past
                        // DIFFER_STEADY after the bite, is steady: it counts as seen, and its mark shows. Hidden
                        // until a read showed the eater, the mark never came on a chain whose eater kept reading
                        // as before its rise (2026-11-05 day 7, "Chain on into 10" on two reads 0.42 s apart).
                        let taken = (st.position, eater.beast, i64::from(eater.tier));
                        let read_at = self.reading.as_ref().map(|r| r.at);
                        let steady = inferred_before.as_ref().is_some_and(|(was, was_jaws, was_at)| *was == taken && *was_jaws == jaws && Some(*was_at) != read_at)
                            && self.bite_at.is_none_or(|at| at.elapsed() >= DIFFER_STEADY);
                        if steady {
                            self.eater_seen = Some(taken);
                        }
                        self.inferred_from = read_at.map(|at| (taken, jaws.clone(), at));
                        if let Some(r) = &mut self.reading {
                            r.warnings.push(Warning::note(trf!(
                                "The eater in cell {} still changes after its bite: taken as the {} the targets the game marks call for, until it reads.",
                                cell_label(st.position),
                                inmate_name(eater.tier, eater.beast)
                            )));
                            // On show as taken: its portrait read so is the
                            // same reading, and the solve and its mark stay.
                            // Read in later, it solved again, its mark off
                            // and on, 4 times in a 2026-10-31 run.
                            if let Some(cell @ CellModel::Inmate { .. }) = r.model.cells.get_mut(st.position) {
                                if let CellModel::Inmate { beast, tier, .. } = cell {
                                    (*beast, *tier) = (eater.beast, i64::from(eater.tier));
                                }
                                self.inferred_eater = Some((st.position, cell.clone()));
                            }
                            // The eater read as taken since the bite is no guess.
                            r.eater_inferred = self.eater_seen != Some((st.position, eater.beast, i64::from(eater.tier)));
                        }
                        st = inferred;
                    }
                    // Advice by the board read names a cell the game does not
                    // take: none until the eater reads (2026-10-29: a day 23
                    // chain's eater read as before its bite for 0.3 s, and
                    // the advice went to an unmarked cell and back).
                    None if missed => {
                        self.problems = vec![trf!(
                            "The Consume targets on screen are {} and the board read allows {}: check the eater in cell {} and those cells.",
                            cell_list(&jaws),
                            cell_list(options.as_deref().unwrap_or_default()),
                            cell_label(st.position),
                        )];
                        return;
                    }
                    // No target on the board read and none marked: the game
                    // ends the chain itself (an Ultimate with the vault full
                    // eats nothing, see `Diet::can_eat`).
                    None if jaws.is_empty() => {
                        self.chain_plan = None;
                        // As above: the same jaws read twice later on the same board solve again.
                        self.differ_jaws = Some(Vec::new());
                        return;
                    }
                    None => {
                        self.problems = vec![trf!(
                            "The eater in cell {} has nothing to Consume on the board read, but the game marks targets: it may still be changing after its bite, and the board is read again once the screen stops. If this stays, check the eater and the marked cells.",
                            cell_label(st.position)
                        )];
                        return;
                    }
                }
            }
        }
        let first = target && !self.chained;
        let mut warning = None;
        let mut bites = Vec::new();
        if target {
            let jawed = self.scene.as_ref().map_or(Vec::new(), |(scene, _, _)| jawed(scene));
            let (eater, chain) = (st.position, st.pending.iter_mut().find_map(|p| match p {
                Pending::Chain { options, .. } => Some(options),
                _ => None,
            }));
            if let Some(options) = chain {
                self.offered_wide = Some(options.clone());
                (*options, warning) = consume_targets(options, &jawed, eater);
            } else if first {
                let options: Vec<usize> = start_options(&st).iter().map(|o| o.cell).collect();
                self.offered_wide = Some(options.clone());
                (bites, warning) = consume_targets(&options, &jawed, eater);
            }
            self.differ_jaws = warning.is_some().then_some(jawed);
        }
        if let Some(r) = &mut self.reading {
            r.warnings.retain(|w| !begins(&w.text, TARGETS_DIFFER));
            r.warnings.extend(warning);
        }
        let pick = if first { instant_bite(&st, &bites) } else { instant(&st) };
        self.pick = pick.map(|i| Pick { action: i.action, label: i.label, why: i.why });
        self.state = Some(st.clone());
        // The chain on show carries over while its next bite is on offer
        // (see `note_chain_plan`).
        if !self.plan_offered() {
            self.chain_plan = None;
        }
        self.note_advised_tool();
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.solving = true;
        (self.advice_steady, self.last_best) = (false, None);
        let (notify, rollouts) = (self.notify.clone(), self.rollouts);
        let unseen = if reading_pick { self.unseen_gifts.clone() } else { Vec::new() };
        // The playouts use all cores but one, below normal priority so the
        // game keeps its frame rate, once they can overrule the strategy's
        // pick: before, at normal priority, as the game busy with a new day
        // held a patrol's advice back 3 s (0.5 s in a replay, 2026-10-28).
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1).max(1));
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut r = Rollouts::with(&st, SEED, &TUNED, first.then_some(&bites[..])).after(&unseen);
            loop {
                #[cfg(windows)]
                {
                    use windows::Win32::System::Threading::{THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL};
                    let priority = if r.rounds() < LIVE_MIN { THREAD_PRIORITY_NORMAL } else { THREAD_PRIORITY_BELOW_NORMAL };
                    crate::solver::solve::set_priority(priority.0);
                }
                let ran = r.run_on(ROUND.min(rollouts - r.rounds()), threads);
                let slow = r.rounds() >= ENOUGH && started.elapsed() >= SLOW_SOLVE;
                let done = ran == 0 || r.rounds() >= rollouts || slow;
                let progress = Progress { rounds: r.rounds(), done, options: r.ranking() };
                if tx.send(progress).is_err() {
                    return;
                }
                if let Some(notify) = &notify {
                    notify();
                }
                if done {
                    return;
                }
            }
        });
    }

    /// Drops the solve, its ranking and pick; the worker stops at its next send.
    fn stop(&mut self) {
        self.chain_plan = None;
        self.offered_wide = None;
        self.ranking.clear();
        self.rounds = 0;
        self.pick = None;
        self.state = None;
        self.problems.clear();
        self.solving = false;
        self.rx = None;
    }

    /// Pause on entering Vorax: solving stops, a half-read screen is dropped, and afterwards no screen reads, no
    /// game log, no marks or tooltips, no log writes, until [`Session::resume`].
    pub fn suspend(&mut self) {
        if self.suspended {
            return;
        }
        self.stop();
        self.drop_moment();
        self.suspended = true;
    }

    /// Resume on leaving Vorax. The run's state (theme, instruments, day count, vault, treasure chain, incidents, the
    /// log's day count) is kept as it was: most of it can only be accumulated from the game log and cannot be read
    /// back from the screen; a new run has its own path (see [`Session::run_over`]). Only reads, time and chains are
    /// cleared, and the next read starts from scratch.
    pub fn resume(&mut self) {
        if !self.suspended {
            return;
        }
        self.drop_moment();
        self.suspended = false;
    }

    /// Whether paused during Vorax (see [`Session::suspend`]).
    pub fn suspended(&self) -> bool {
        self.suspended
    }

    /// State cleared on pause and resume: the screen being read, the read being shown, holds tied to
    /// the game log's time, and a chain in progress (when leaving Vorax it is not still in a chain).
    fn drop_moment(&mut self) {
        self.scan = None;
        self.queued = None;
        self.ahead = Ahead::No;
        self.ahead_shown = None;
        self.reread = false;
        self.recheck = false;
        self.read_in = None;
        self.news = None;
        self.trigger = None;
        self.scan_started = None;
        self.reading_scan = None;
        self.last_kept = None;
        self.reading = None;
        self.missed = None;
        self.scene = None;
        self.on_screen = false;
        self.confirming = false;
        self.drop_held = None;
        self.fade_kept = false;
        self.edits.clear();
        self.markers.clear();
        self.steady.clear();
        self.warned_keys.clear();
        self.warned_on = None;
        self.problems_since = None;
        self.held = None;
        self.shown = None;
        self.advice_steady = false;
        self.last_best = None;
        self.played_at = None;
        self.cards_up = None;
        self.cards_seen.clear();
        self.fresh_at = None;
        self.short_doors = None;
        self.short_offer = None;
        self.full_offer = None;
        self.rerolled = None;
        self.pick_opened = None;
        self.day_at = None;
        self.effect_until = None;
        self.moved_from = None;
        self.glimpse = None;
        self.gift_gap = None;
        // The whole chain has ended.
        self.consume = None;
        self.chained = false;
        self.consuming = false;
        self.meal = None;
        self.meal_doubt.clear();
        self.bite_seen = None;
        self.bite_at = None;
        self.bite_logged = false;
        self.log_bites = None;
        self.bites_at_consume = None;
        self.differ_steady = false;
        self.differ_jaws = None;
        self.jaws_last = None;
        self.inferred_from = None;
        self.jawless_held = None;
        self.bite_cells = None;
        self.unshown_why = None;
        self.inferred_eater = None;
        self.eater_seen = None;
        self.eater_vaulted = false;
        self.manage_bite = None;
        self.chain_follow.clear();
        self.chain_numbers.clear();
        self.eaten = None;
    }

    /// The riot screen: the run is over. The solve stops and the board goes
    /// with the run (nothing on it can be played or corrected any more); the
    /// line says how many Chromatic Prisoners the run vaulted, the most any
    /// of its readings showed.
    /// The held instruments and the days count go too. The riot screen read
    /// again keeps the line. The run log gets a run_end and the next run's
    /// folder only when the last reading was on the run's last day: the riot
    /// screen also shows right after a run ends, and a board read in between
    /// (a day 1 with an empty vault) is no run of its own.
    fn end_run(&mut self) {
        if self.run_over.is_some() {
            return;
        }
        // The game's log counts the gifts confirmed: the run's last
        // treasures leave no board to read (2026-10-28: 4 read for 6).
        let taken = (self.gifts_taken / treasure_picks(self.theme) as usize).min(VAULT_CAP as usize);
        let vaulted = match self.vault_top.as_ref().map(|(_, v)| v.len()) {
            Some(read) => Some(read.max(taken)),
            None => (taken > 0).then_some(taken),
        };
        self.run_over = Some(match vaulted {
            Some(1) => tr("The run is over: 1 Chromatic Prisoner vaulted.").to_string(),
            Some(n) => trf!("The run is over: {} Chromatic Prisoners vaulted.", n),
            None => tr("The run is over.").to_string(),
        });
        self.run_over_at = Some(std::time::Instant::now());
        if let Some(log) = &mut self.log
            && self.reading.as_ref().is_some_and(|r| r.model.day >= r.model.max_days)
        {
            log.event("run_end", SystemTime::now(), json!({ "vaulted": vaulted }));
            // The run's check, once its lines are on disk (see `health`).
            let (dir, written) = (log.dir().to_path_buf(), log.flushed());
            let (tx, rx) = channel();
            (self.health, self.health_rx) = (None, Some(rx));
            let notify = self.notify.clone();
            std::thread::spawn(move || {
                let _ = written.recv();
                let crash = super::settings::app_dir().map(|d| d.join("crash.log"));
                if let Some(report) = super::health::check(&dir, crash.as_deref()) {
                    let _ = std::fs::write(dir.join("health.txt"), report.text());
                    let _ = tx.send(report);
                }
                if let Some(notify) = &notify {
                    notify();
                }
            });
            log.new_run();
        }
        self.stop();
        self.reading = None;
        self.missed = None;
        self.scene = None;
        self.board = None;
        self.consume = None;
        self.vault_top = None;
        self.gifts_taken = 0;
        self.vault_flies = 0;
        self.vault_lights = 0;
        self.unseen_gifts.clear();
        self.log_day = None;
        self.chain_follow.clear();
        self.chain_numbers.clear();
        self.shown = None;
        self.vault_paid = 0;
        self.vault_base = 0;
        self.treasure_line = None;
        self.prey_seen.clear();
        self.chained = false;
        self.consuming = false;
        self.bite_seen = None;
        self.log_bites = None;
        self.bites_at_consume = None;
        self.bite_at = None;
        self.bite_cells = None;
        self.cards_up = None;
        self.fresh_at = None;
        self.cards_seen.clear();
        self.chain_armed = false;
        self.moved_from = None;
        self.held = None;
        self.manage_bite = None;
        self.bite_logged = false;
        self.objective_done = false;
        self.consumed_on = None;
        self.gift_unanswered = false;
        self.treasure_reopened = false;
        self.pick_after = None;
        self.pick_seen = false;
        self.glimpse = None;
        self.gift_gap = None;
        self.usurped = None;
        self.meal = None;
        self.meal_doubt.clear();
        self.advised_tool = None;
        self.held_edit = None;
        self.pick_named = false;
        self.steady.clear();
        self.warned_keys.clear();
        self.warned_on = None;
        self.days = DaysClock::default();
        self.treasure = None;
        self.theme_fresh = false;
        self.theme_icon_seen = None;
        self.theme_set = false;
        if self.theme.take().is_some() {
            self.tools_changed = true;
        }
        if !self.tools.is_empty() {
            self.tools.clear();
            self.tools_changed = true;
        }
    }

    /// Takes the worker's latest ranking; true when something changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self.problems.is_empty() {
            self.problems_since = None;
        } else {
            self.problems_since.get_or_insert_with(std::time::Instant::now);
        }
        if let Some(scan) = &self.scan {
            match scan.try_recv() {
                Ok(scanned) => {
                    changed = self.scanned(scanned);
                    self.read_queued();
                }
                // The reader thread panicked: the spinner must not stick.
                Err(TryRecvError::Disconnected) => {
                    self.scan = None;
                    self.ahead = Ahead::No;
                    self.read_queued();
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(report) = self.health_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.health = Some(report);
            self.health_rx = None;
            changed = true;
        }
        let Some(rx) = &self.rx else { return changed };
        let was_solving = self.solving;
        while let Ok(p) = rx.try_recv() {
            self.ranking = p.options;
            self.rounds = p.rounds;
            self.solving = !p.done;
            changed = true;
        }
        // The advice settles once past SHOW_MIN it stays the same from one
        // progress to the next: a chain's advice past 500 playouts moved
        // from 06 to 04 32 ms after it showed (2026-10-30).
        // A move within the margin of error of another clicked elsewhere
        // waits for the solve's end: "Replace" showed past 500 playouts and
        // gave way to "End" 0.17 s later, as the player clicked Replace
        // (2026-10-30). Showing such ties at 1000 playouts was tried: a
        // Consume shown so gave way to Replace 0.08 s later in the next run.
        // On the manage screen a Consume (or a Hunt) is its card whatever
        // its target: moves alike there need not wait, as the first click is
        // the same (ties held the advice 0.1 s longer in a 2026-10-30 run).
        if changed && self.solving && self.rounds >= SHOW_MIN {
            let manage = self.reading.as_ref().is_some_and(|r| r.screen == Screen::Manage);
            let alike = |a: &Action, b: &Action| if manage { same_card(a, b) } else { a == b };
            let advice = self.advice();
            let clear = advice.as_ref().is_some_and(|a| also_good(&self.ranking, a.option).iter().all(|&k| alike(&self.ranking[k].action, &a.action)));
            let best = advice.map(|a| a.action);
            let held = best.as_ref().zip(self.last_best.as_ref()).is_some_and(|(b, last)| alike(b, last));
            self.advice_steady |= clear && held;
            self.last_best = best;
        }
        if changed {
            self.note_advised_tool();
            self.note_chain_plan();
            self.note_shown();
        }
        if was_solving && !self.solving {
            self.log_advice();
        }
        changed
    }

    /// Takes the end of `scan` and installs its read. A read ahead is shown at
    /// once rather than when auto read takes its frame, about 0.2 s later: the
    /// frame shows the screen standing still (see [`Watch::take_ahead`]), and
    /// a screen that moves on after it is read again. What it showed is
    /// reported to auto read once auto read takes the frame.
    fn scanned(&mut self, scanned: Scanned) -> bool {
        self.scan_ms = self.scan_started.map(|t| t.elapsed().as_millis() as u64);
        self.scan = None;
        let ahead = std::mem::replace(&mut self.ahead, Ahead::No) == Ahead::Waiting;
        if ahead {
            self.live = true;
            self.trigger = Some(Trigger { reason: "ahead", ..Trigger::default() });
        }
        self.install(scanned);
        if ahead {
            self.ahead_shown = Some(self.news.take());
        }
        true
    }

    /// Starts a read of the frame that waited for the last one, if any.
    fn read_queued(&mut self) {
        if let Some((frame, live)) = self.queued.take() {
            self.start_scan(frame, live);
        }
    }

    /// Blocks until the pending reads, the running solve and the run log's
    /// writes finish (for tests).
    pub fn wait(&mut self) {
        while let Some(scan) = self.scan.take() {
            match scan.recv() {
                Ok(scanned) => {
                    self.scanned(scanned);
                }
                Err(_) => self.ahead = Ahead::No,
            }
            self.read_queued();
        }
        let was_solving = self.solving;
        while self.solving {
            let Some(rx) = &self.rx else { break };
            match rx.recv() {
                Ok(p) => {
                    self.ranking = p.options;
                    self.rounds = p.rounds;
                    self.solving = !p.done;
                }
                Err(_) => self.solving = false,
            }
        }
        self.note_advised_tool();
        self.note_chain_plan();
        self.note_shown();
        if was_solving {
            self.log_advice();
        }
        if let Some(log) = &self.log {
            log.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scale logged with a read keeps 3 decimals, as JSON numbers.
    #[test]
    fn read_log_scale_has_three_decimals() {
        assert_eq!(logged_scale(1.0), 1.0);
        assert_eq!(logged_scale(0.833_333_3), 0.833);
        assert_eq!(logged_scale(1.333_6), 1.334);
        assert_eq!(json!(logged_scale(0.75)).to_string(), "0.75");
        assert_eq!(json!(logged_scale(0.833_333_3)).to_string(), "0.833");
    }

    fn inmate(beast: Beast, tier: i64) -> CellModel {
        CellModel::Inmate { beast, tier, ivory: false, prey: None, event: None }
    }

    /// A board with the warden on cell 3 (index 2) eating a Rat on cell 5.
    fn board() -> Model {
        let mut m = Model::blank();
        m.warden = 2;
        m.cells[2] = inmate(Beast::Mammoth, 0);
        m.cells[4] = inmate(Beast::Rat, 0);
        m.cells[6] = inmate(Beast::Wolf, 1);
        m
    }

    #[test]
    fn a_rarity_drop_with_nothing_in_between_is_unsteady() {
        // 2026-11-05 day 8 (13:14:27 UTC): the Ultimate Colossal Rat in cell 07 (index 6) read as Normal on the
        // manage screen, with only Moved, Decision (on the patrol screen) and an incident since the board.
        let mut before = board();
        before.cells[6] = inmate(Beast::Rat, 4);
        let mut read = before.clone();
        read.cells[6] = inmate(Beast::Rat, 0);
        assert_eq!(dropped_cells(&read, &before), vec![(6, Beast::Rat, 0)]);
        assert!(holds_drop(&dropped_cells(&read, &before), None, false));
        // A Bite, Consume or Replace in between explains it: taken as it is.
        assert!(!holds_drop(&dropped_cells(&read, &before), None, true));
        // Another beast in the cell (a Replace's new inmate) or a rise is no drop.
        read.cells[6] = inmate(Beast::Wolf, 0);
        assert!(dropped_cells(&read, &before).is_empty());
        assert!(!holds_drop(&dropped_cells(&read, &before), None, false));
        read.cells[6] = inmate(Beast::Rat, 5);
        assert!(dropped_cells(&read, &before).is_empty());
        assert!(dropped_cells(&before, &before).is_empty());
    }

    #[test]
    fn the_same_drop_on_the_next_read_is_taken_while_other_cells_change() {
        let mut board = board();
        board.cells[6] = inmate(Beast::Rat, 4);
        // The first read with the drop: held for one more read.
        let mut first = board.clone();
        first.cells[6] = inmate(Beast::Rat, 0);
        let held = dropped_cells(&first, &board);
        assert!(holds_drop(&held, None, false));
        // The next read shows the same drop while another cell still animates (the Rat on cell 05 read as a
        // Wolf, then empty): the drop alone is compared, and the read is taken.
        let mut second = first.clone();
        second.cells[4] = inmate(Beast::Wolf, 0);
        let drops = dropped_cells(&second, &board);
        assert_eq!(drops, held);
        assert!(!holds_drop(&drops, Some(held.as_slice()), false));
        second.cells[4] = CellModel::Empty { event: None };
        assert!(!holds_drop(&dropped_cells(&second, &board), Some(held.as_slice()), false));
    }

    #[test]
    fn the_read_confirming_a_drop_is_not_held_again() {
        let mut board = board();
        board.cells[6] = inmate(Beast::Rat, 4);
        let mut read = board.clone();
        read.cells[6] = inmate(Beast::Rat, 0);
        let drops = dropped_cells(&read, &board);
        // Held on a screen's first read for the drop (or a board change) with the drop on it: the confirming read
        // showing it is taken, so a real drop waits one read, not two.
        assert!(!holds_drop(&drops, Some(drops.as_slice()), false));
        // A drop waits once at most: after a held read with a drop, the next read is taken even with another drop.
        let mut other = read.clone();
        other.cells[2] = inmate(Beast::Mammoth, 0);
        board.cells[2] = inmate(Beast::Mammoth, 2);
        assert!(!holds_drop(&dropped_cells(&other, &board), Some(drops.as_slice()), false));
        // A read held for a board change with no drop on it, and a drop first seen on the confirming read (the
        // 2026-11-05 day 8 case): that read is held once.
        assert!(holds_drop(&drops, Some(&[] as &[TierDrop]), false));
        // A Bite, Consume or Replace told of: taken at once.
        assert!(!holds_drop(&drops, Some(&[] as &[TierDrop]), true));
    }

    /// A board scene read sure: open empty cells, the warden on cell 3, and `inmates` as (screen cell, beast, tier,
    /// beast confidence).
    fn scene_with(inmates: &[(u8, Beast, u8, f32)]) -> Scene {
        use crate::reader::scene::CellRead;
        let sure = CellRead { locked_conf: 1.0, beast_conf: 1.0, tier_conf: 1.0, halo: 1.0, event_conf: 1.0, jaws_conf: 1.0, ..Default::default() };
        let mut scene = Scene { cells: vec![sure; 15], warden: Some(3), warden_conf: 1.0, ..Default::default() };
        for &(cell, beast, tier, conf) in inmates {
            let r = &mut scene.cells[cell as usize - 2];
            r.inmate = Some((beast, tier));
            r.beast_conf = conf;
        }
        scene
    }

    /// The panels of `screen` with `days` until the riot and every decision button on show.
    fn screen_panels(screen: Screen, days: Option<u32>) -> Panels {
        Panels {
            screen,
            days,
            doors: Vec::new(),
            decision: vec!["Consume", "Replace", "End"],
            instruments: Vec::new(),
            rerolls: 0,
            vault: Vec::new(),
            tool_icons: false,
            theme: None,
            incident: None,
            objective: None,
            held: None,
            spots: Vec::new(),
            gift_picked: false,
            card_red: [None; 7],
            doors_fixed: false,
            by_icons: Vec::new(),
        }
    }

    /// A live session with the board on show read from `high` on the manage screen, 10 days left.
    fn session_on(high: &Scene) -> Session {
        let manage = screen_panels(Screen::Manage, Some(10));
        let mut s = Session::new(100, Vec::new(), None, Some(10));
        s.live = true;
        s.board = Some(to_model(high, &manage, &[], None, RUN_DAYS).0);
        s.scene = Some((high.clone(), manage, SystemTime::now()));
        s
    }

    /// The Colossal Rat on cell 07 read Ultimate, then Normal.
    fn rat_scenes() -> (Scene, Scene) {
        (
            scene_with(&[(3, Beast::Mammoth, 0, 1.0), (7, Beast::Rat, 4, 1.0)]),
            scene_with(&[(3, Beast::Mammoth, 0, 1.0), (7, Beast::Rat, 0, 1.0)]),
        )
    }

    #[test]
    fn a_real_drop_through_take_read_waits_one_read() {
        let size = (REF_W, REF_H);
        let (high, low) = rat_scenes();
        let mut s = session_on(&high);
        let manage = screen_panels(Screen::Manage, Some(10));
        // The drop's first read: held, as the board changed with the drop on it.
        assert!(matches!(s.take_read(low.clone(), manage.clone(), size), Taken::Kept(line) if is_line(&line, CHANGED)));
        // The read confirming it shows the same drop: taken, not held a second time.
        assert!(!matches!(s.take_read(low, manage, size), Taken::Kept(_)));
        assert!(s.reading.as_ref().is_some_and(|r| matches!(r.model.cells[6], CellModel::Inmate { beast: Beast::Rat, tier: 0, .. })));
    }

    #[test]
    fn a_drop_after_a_read_that_returned_early_is_held_again() {
        let size = (REF_W, REF_H);
        let (high, low) = rat_scenes();
        let mut s = session_on(&high);
        let cell_pick = screen_panels(Screen::CellPick, Some(10));
        assert!(matches!(s.take_read(low.clone(), screen_panels(Screen::Manage, Some(10)), size), Taken::Kept(line) if is_line(&line, CHANGED)));
        // The read after the hold is a popup: kept before the drops are looked at.
        assert!(matches!(s.take_read(low.clone(), screen_panels(Screen::Popup, None), size), Taken::Kept(line) if is_line(&line, POPUP)));
        // A cell pick then reads the drop: the hold before the popup no longer counts, and this read is held once.
        assert!(matches!(s.take_read(low.clone(), cell_pick.clone(), size), Taken::Kept(line) if is_line(&line, CHANGED)));
        assert!(!matches!(s.take_read(low, cell_pick, size), Taken::Kept(_)));
    }

    /// The panels of a Consume target screen: Cancel on show, the counter hidden.
    fn target_panels() -> Panels {
        let mut panels = screen_panels(Screen::ChainTarget, None);
        panels.decision = vec!["Cancel"];
        panels.spots = vec![(Spot::Button("Cancel"), Layout::Board, (2300, 1070))];
        panels
    }

    /// `scene` with the big jaws over the screen cells `cells`.
    fn with_jaws(scene: &Scene, cells: &[u8]) -> Scene {
        let mut scene = scene.clone();
        for &cell in cells {
            scene.cells[cell as usize - 2].target = true;
        }
        scene
    }

    /// A live session in a chain on `board`, its first bite taken.
    fn chain_on(board: &Scene) -> Session {
        let mut s = session_on(board);
        s.chained = true;
        s
    }

    /// The chain bites the solved state offers, as model indices.
    fn chain_options(s: &Session) -> Option<Vec<usize>> {
        s.state.as_ref()?.pending.iter().find_map(|p| match p {
            Pending::Chain { options, .. } => Some(options.clone()),
            _ => None,
        })
    }

    /// A chain's boards before and after a bite: a Normal Lion on cell 03 (the warden's) marked to eat the Normal
    /// Rat on cell 07; then, cell 07 eaten, the Lion risen to Magic, to eat the Magic Rat on cell 09. With the jaws.
    fn bite_boards() -> (Scene, Scene, Scene) {
        let before = with_jaws(&scene_with(&[(3, Beast::Lion, 0, 1.0), (7, Beast::Rat, 0, 1.0), (9, Beast::Rat, 1, 1.0)]), &[7]);
        let after = scene_with(&[(3, Beast::Lion, 1, 1.0), (9, Beast::Rat, 1, 1.0)]);
        let jawed = with_jaws(&after, &[9]);
        (before, after, jawed)
    }

    fn installs(s: &mut Session, scene: &Scene, panels: Panels) {
        s.install((scene.clone(), panels, (REF_W, REF_H), None, 1.0));
    }

    /// A chain on [`bite_boards`]: its first board read and advised, then `bitten` set up as the game's bite would.
    fn chain_advised(bitten: impl FnOnce(&mut Session)) -> Session {
        let (before, _, _) = bite_boards();
        let mut s = chain_on(&before);
        installs(&mut s, &before, target_panels());
        assert_eq!(chain_options(&s), Some(vec![6]));
        s.wait();
        assert!(s.shown_advice().is_some() && s.mark().is_some(), "the chain advised and marked");
        bitten(&mut s);
        s
    }

    /// Right after a bite, or with an instrument that refills a cell, a chain's target screen read once with no jaws
    /// over a reading taken since the bite keeps the advice and its mark; the jaws read next go on with the chain
    /// (2026-11-05 day 5: "Chain on into 08" showed for 0.08 s before one jawless read cleared it).
    #[test]
    fn a_jawless_read_in_a_chain_keeps_its_advice_and_mark() {
        let (_, after, jawed) = bite_boards();
        for refill in [false, true] {
            let mut s = chain_advised(|s| {
                if refill {
                    s.tools = vec![Tool::Beastcage];
                } else {
                    s.bite_at = Some(std::time::Instant::now());
                }
            });
            // The bite read with the jaws on cell 09: the chain goes on.
            installs(&mut s, &jawed, target_panels());
            assert!(s.last_kept.is_none());
            assert_eq!(chain_options(&s), Some(vec![8]));
            s.wait();
            let (advice, mark) = (s.shown_advice().map(|a| a.action), s.mark());
            assert!(advice.is_some() && mark.is_some());
            // One read with no jaws: held, the advice and its mark as they were.
            installs(&mut s, &after, target_panels());
            assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, CHANGED)), "refill {refill}");
            assert_eq!(chain_options(&s), Some(vec![8]));
            assert_eq!(s.shown_advice().map(|a| a.action), advice);
            assert_eq!(s.mark(), mark);
            // The jaws back: the same chain, advice and mark.
            installs(&mut s, &jawed, target_panels());
            assert!(s.last_kept.is_none());
            assert_eq!(chain_options(&s), Some(vec![8]));
            assert_eq!(s.shown_advice().map(|a| a.action), advice);
            assert_eq!(s.mark(), mark);
        }
    }

    /// A target screen read after the bite of [`bite_boards`] with Avenger's Casket held: the casket put a Magic Rat in
    /// cell 07, the cell just eaten, and the jaws mark it and cell 09. They spoil cell 07's rarity, which reads Normal
    /// for sure (2026-11-06 day 8, frame-0082). `eater` is how the eater on cell 03 reads: beast, rarity, confidence.
    fn refill_read(eater: (Beast, u8, f32)) -> Scene {
        let (beast, tier, conf) = eater;
        with_jaws(&scene_with(&[(3, beast, tier, conf), (7, Beast::Rat, 0, 1.0), (9, Beast::Rat, 1, 1.0)]), &[7, 9])
    }

    /// A chain on [`bite_boards`] with Avenger's Casket held, its bite told of by the game's log `ago` before now.
    fn casket_bitten(ago: std::time::Duration) -> Session {
        chain_advised(|s| {
            s.tools = vec![Tool::Revenge];
            s.note_game(Signal::Bite);
            s.bite_at = s.bite_at.map(|at| at - ago);
        })
    }

    /// Whether the reading on show warns that the board's targets differ from the game's marks.
    fn targets_differ(s: &Session) -> bool {
        s.reading.as_ref().is_some_and(|r| r.warnings.iter().any(|w| begins(&w.text, TARGETS_DIFFER)))
    }

    /// The cell `n` of the reading on show is `beast` at `tier`.
    fn reads_as(s: &Session, n: usize, beast: Beast, tier: i64) -> bool {
        s.reading.as_ref().is_some_and(|r| matches!(r.model.cells[n], CellModel::Inmate { beast: b, tier: t, .. } if (b, t) == (beast, tier)))
    }

    /// Right after a bite the casket refilled, a read whose eater is in doubt (its portrait mid-change) is not taken:
    /// its reading would show the eater unrisen, and the new inmate at the rarity from before the bite. It is read
    /// again at once, and the next read, the eater risen for sure, gives the new inmate the eater's rarity, the
    /// targets the game marks, and the advice (2026-11-06 day 8: the targets differed for 1.1 s, and the advice then
    /// ended the chain on that board).
    #[test]
    fn a_casket_refill_waits_for_an_eater_read_for_sure() {
        let mut s = casket_bitten(std::time::Duration::ZERO);
        s.reread = false;
        installs(&mut s, &refill_read((Beast::Mammoth, 1, 0.11)), target_panels());
        assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, BITE_UNSHOWN)), "{:?}", s.last_kept);
        assert!(s.reread, "read again at once");
        // The reading on show is still the board before the bite, its mark hidden as the bite was played.
        assert_eq!(chain_options(&s), Some(vec![6]));
        assert!(s.mark().is_none());
        assert!(s.shown_advice().is_none(), "no advice for the meal just eaten");
        // The eater read risen for sure: the new inmate at its rarity, the board's targets the game's.
        installs(&mut s, &refill_read((Beast::Lion, 1, 1.0)), target_panels());
        assert!(s.last_kept.is_none());
        assert!(reads_as(&s, 2, Beast::Lion, 1) && reads_as(&s, 6, Beast::Rat, 1));
        assert!(!targets_differ(&s));
        assert_eq!(chain_options(&s), Some(vec![6, 8]));
        s.wait();
        assert!(matches!(s.shown_advice().map(|a| a.action), Some(Action::Chain { to: 6 | 8 })));
    }

    /// The casket's new inmate in the cell just eaten, taken at the rarity of an eater not yet risen (a read past
    /// BITE_SHOWS, so not held), follows the eater's rise on the next read rather than standing for the rest of the
    /// bite with the targets differing from the game's marks.
    #[test]
    fn a_casket_refill_follows_the_eater_risen_later() {
        let mut s = casket_bitten(BITE_SHOWS);
        installs(&mut s, &refill_read((Beast::Mammoth, 1, 0.11)), target_panels());
        assert!(s.last_kept.is_none(), "past BITE_SHOWS the read is taken: the hold has an end");
        assert!(reads_as(&s, 2, Beast::Lion, 0) && reads_as(&s, 6, Beast::Rat, 0));
        assert!(targets_differ(&s));
        installs(&mut s, &refill_read((Beast::Lion, 1, 1.0)), target_panels());
        assert!(reads_as(&s, 2, Beast::Lion, 1) && reads_as(&s, 6, Beast::Rat, 1));
        assert!(!targets_differ(&s));
        assert_eq!(chain_options(&s), Some(vec![6, 8]));
    }

    /// The board of 2026-11-06 day 7 (08:17:39 to 08:17:44 UTC) as the run's log.jsonl read it, the warden on cell 11
    /// as `eater` (beast, rarity), cells 14 and 16 as given (beast, rarity, confidence), and the jaws on `jaws`.
    fn day7_scene(eater: (Beast, u8), cell14: (Beast, u8, f32), cell16: (Beast, u8, f32), jaws: &[u8]) -> Scene {
        let mut scene = scene_with(&[
            (2, Beast::Mammoth, 2, 1.0),
            (3, Beast::Lion, 2, 1.0),
            (4, Beast::Wolf, 3, 1.0),
            (5, Beast::Lion, 2, 1.0),
            (7, Beast::Mammoth, 1, 1.0),
            (9, Beast::Lion, 2, 1.0),
            (10, Beast::Wolf, 0, 1.0),
            (11, eater.0, eater.1, 1.0),
            (12, Beast::Rat, 3, 1.0),
            (13, Beast::Wolf, 0, 1.0),
            (14, cell14.0, cell14.1, cell14.2),
            (16, cell16.0, cell16.1, cell16.2),
        ]);
        for cell in [6_usize, 8, 15] {
            scene.cells[cell - 2].locked = true;
        }
        scene.warden = Some(11);
        with_jaws(&scene, jaws)
    }

    /// A replay of 2026-11-06 day 7 (08:17:44 UTC), Avenger's Casket held: the Rare Berserk Lion on cell 11 had the
    /// Rare Alpha Wolf on 14 and the Rare Colossal Rat on 16 to eat, "Chain on into 16" was advised with 14 as good,
    /// and the player ate 14. The eater rose to a Legendary Colossal Rat, the casket put a Legendary Mammoth on 14
    /// under the jaws, read unsure, and 16 read as before. Taken as 16, cell 14 kept the Wolf, the eater had nothing
    /// to eat on the board read, and no advice showed for 2.8 s. The target not advised, eaten, is the meal: the first
    /// read the bite shows on gives the casket's Mammoth and the advice. With 13 days left and 1600 playouts the solve
    /// here advises 16 as it did live; should it advise 14, 16 is eaten instead.
    #[test]
    fn a_casket_refill_of_the_target_not_advised_gives_advice_at_once() {
        let rare_wolf = (Beast::Wolf, 2, 1.0);
        let rare_rat = (Beast::Rat, 2, 1.0);
        let before = day7_scene((Beast::Lion, 2), rare_wolf, rare_rat, &[14, 16]);
        // 13 days left, as on day 7, and the live solve's playouts.
        let manage = screen_panels(Screen::Manage, Some(13));
        let mut s = Session::new(1600, Vec::new(), None, Some(13));
        s.live = true;
        s.board = Some(to_model(&before, &manage, &[], None, RUN_DAYS).0);
        s.scene = Some((before.clone(), manage, SystemTime::now()));
        s.chained = true;
        s.tools = vec![Tool::Revenge, Tool::Longchain];
        installs(&mut s, &before, target_panels());
        assert_eq!(chain_options(&s), Some(vec![13, 15]));
        s.wait();
        let Some(Action::Chain { to: advised }) = s.shown_advice().map(|a| a.action) else {
            panic!("the chain advised: {:?}", s.shown_advice());
        };
        let eaten = if advised == 15 { 13 } else { 15 };
        s.note_game(Signal::Bite);
        // The first read after the bite (frame-0080): the eater not risen yet, both targets unsure. Held as before.
        installs(&mut s, &day7_scene((Beast::Lion, 2), (Beast::Wolf, 0, 0.26), (Beast::Rat, 2, 0.05), &[14, 16]), target_panels());
        assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, BITE_UNSHOWN)), "{:?}", s.last_kept);
        // The bite shown (frame-0081): the eater risen, the casket's Legendary Mammoth unsure under the jaws.
        let refill = (Beast::Mammoth, 3, 0.02);
        let after = if eaten == 13 { day7_scene((Beast::Rat, 3), refill, rare_rat, &[14]) } else { day7_scene((Beast::Rat, 3), rare_wolf, refill, &[16]) };
        installs(&mut s, &after, target_panels());
        assert!(s.last_kept.is_none(), "advised {advised}: {:?}", s.last_kept);
        assert_eq!(s.meal, Some(Meal { cell: eaten, certain: true }), "advised {advised}");
        assert!(reads_as(&s, 10, Beast::Rat, 3) && reads_as(&s, eaten, Beast::Mammoth, 3), "advised {advised}");
        assert!(s.problems.is_empty(), "advised {advised}: {:?}", s.problems);
        assert_eq!(chain_options(&s), Some(vec![eaten]));
        s.wait();
        assert_eq!(s.shown_advice().map(|a| a.action), Some(Action::Chain { to: eaten }), "advised {advised}");
    }

    /// A chain's board before a bite with two targets: a Normal Berserk Lion on cell 03 (the warden's) that may eat
    /// the Normal Alpha Wolf on cell 07 or the Normal Colossal Rat on cell 09, and a Magic Colossal Rat on cell 11 for
    /// the Lion risen to Magic.
    const TWO_TARGETS: [(u8, Beast, u8, f32); 4] = [(3, Beast::Lion, 0, 1.0), (7, Beast::Wolf, 0, 1.0), (9, Beast::Rat, 0, 1.0), (11, Beast::Rat, 1, 1.0)];

    /// The beast of [`TWO_TARGETS`] on model cell `n`.
    fn two_targets_beast(n: usize) -> Beast {
        TWO_TARGETS.iter().find(|&&(c, ..)| usize::from(c) == n + 1).map_or_else(|| panic!("no inmate on {n}"), |&(_, b, ..)| b)
    }

    /// A chain on [`TWO_TARGETS`] with `tools` held, advised, its bite then told of by the game's log: the session and
    /// the target advised (model index).
    fn two_targets_bitten(tools: &[Tool]) -> (Session, usize) {
        let before = with_jaws(&scene_with(&TWO_TARGETS), &[7, 9]);
        let mut s = chain_on(&before);
        s.tools = tools.to_vec();
        installs(&mut s, &before, target_panels());
        assert_eq!(chain_options(&s), Some(vec![6, 8]));
        s.wait();
        let Some(Action::Chain { to }) = s.shown_advice().map(|a| a.action) else {
            panic!("the chain advised: {:?}", s.shown_advice());
        };
        s.note_game(Signal::Bite);
        (s, to)
    }

    /// The read after a bite on [`TWO_TARGETS`]: the Lion risen to Magic, model cell `eaten` empty or holding `now`
    /// (beast, rarity, confidence), the other cells as they were, and the jaws on `jaws`.
    fn two_targets_after(eaten: usize, now: Option<(Beast, u8, f32)>, jaws: &[u8]) -> Scene {
        let mut inmates: Vec<(u8, Beast, u8, f32)> = TWO_TARGETS.iter().copied().filter(|&(c, ..)| usize::from(c) != eaten + 1).collect();
        inmates[0] = (3, Beast::Lion, 1, 1.0);
        if let Some((beast, tier, conf)) = now {
            inmates.push((eaten as u8 + 1, beast, tier, conf));
        }
        with_jaws(&scene_with(&inmates), jaws)
    }

    /// The other target of [`TWO_TARGETS`] (model index) and the Magic inmate a casket would put in it, of the other
    /// beast the Lion eats.
    fn other_target(n: usize) -> (usize, Beast) {
        if n == 6 { (8, Beast::Wolf) } else { (6, Beast::Rat) }
    }

    /// The player eats the target not advised: with no instrument the cell empties, with Avenger's Casket a new inmate
    /// comes in under the jaws, read unsure. Either way the target eaten is the meal, on the first read the bite
    /// shows on, and the one advised stays on the board as it was.
    #[test]
    fn a_bite_of_the_target_not_advised_is_its_meal() {
        for tools in [vec![], vec![Tool::Revenge]] {
            let (mut s, advised) = two_targets_bitten(&tools);
            let (eaten, refill) = other_target(advised);
            let after = if tools.is_empty() {
                two_targets_after(eaten, None, &[11])
            } else {
                two_targets_after(eaten, Some((refill, 1, 0.11)), &[eaten as u8 + 1, 11])
            };
            installs(&mut s, &after, target_panels());
            assert!(s.last_kept.is_none(), "tools {tools:?}: {:?}", s.last_kept);
            assert_eq!(s.meal, Some(Meal { cell: eaten, certain: true }), "tools {tools:?}");
            assert!(reads_as(&s, advised, two_targets_beast(advised), 0), "tools {tools:?}");
            let options = if tools.is_empty() { vec![10] } else { vec![eaten.min(10), eaten.max(10)] };
            assert_eq!(chain_options(&s), Some(options), "tools {tools:?}");
            if !tools.is_empty() {
                assert!(reads_as(&s, eaten, refill, 1) && !targets_differ(&s), "tools {tools:?}");
            }
            s.wait();
            assert!(matches!(s.shown_advice().map(|a| a.action), Some(Action::Chain { .. })), "tools {tools:?}");
        }
    }

    /// The player eats the target advised: the meal is the advice, as before, the read taken at once, with or without
    /// the casket's new inmate in it.
    #[test]
    fn a_bite_of_the_target_advised_is_read_as_before() {
        for tools in [vec![], vec![Tool::Revenge]] {
            let (mut s, advised) = two_targets_bitten(&tools);
            s.reread = false;
            let (other, _) = other_target(advised);
            let refill = two_targets_beast(other);
            let after = if tools.is_empty() {
                two_targets_after(advised, None, &[11])
            } else {
                two_targets_after(advised, Some((refill, 1, 0.11)), &[advised as u8 + 1, 11])
            };
            installs(&mut s, &after, target_panels());
            assert!(s.last_kept.is_none(), "tools {tools:?}: {:?}", s.last_kept);
            assert!(!s.reread, "tools {tools:?}");
            assert_eq!(s.meal, Some(Meal { cell: advised, certain: false }), "tools {tools:?}");
            assert!(reads_as(&s, other, two_targets_beast(other), 0), "tools {tools:?}");
            let options = if tools.is_empty() { vec![10] } else { vec![advised.min(10), advised.max(10)] };
            assert_eq!(chain_options(&s), Some(options), "tools {tools:?}");
            if !tools.is_empty() {
                assert!(reads_as(&s, advised, refill, 1), "tools {tools:?}");
            }
        }
    }

    /// With three targets, the one advised read as before after the bite and two others unsure, which went is unknown:
    /// the read is held, as one that does not show the bite yet, and read again at once. The next read, one of the
    /// two read for sure as before, tells the other.
    #[test]
    fn a_meal_in_doubt_is_held_and_read_again() {
        let mut t = ThreeTargets::bitten(std::time::Duration::ZERO);
        let scene = t.read(0.11);
        installs(&mut t.s, &scene, target_panels());
        let advised = t.advised;
        assert!(t.s.last_kept.as_deref().is_some_and(|line| is_line(line, BITE_UNSHOWN)), "advised {advised}: {:?}", t.s.last_kept);
        assert!(t.s.reread, "read again at once");
        assert!(t.s.shown_advice().is_none());
        let scene = t.read(1.0);
        installs(&mut t.s, &scene, target_panels());
        assert!(t.s.last_kept.is_none(), "advised {advised}: {:?}", t.s.last_kept);
        assert_eq!(t.s.meal, Some(Meal { cell: t.eaten, certain: true }), "advised {advised}");
        assert!(t.s.meal_doubt.is_empty());
        assert!(reads_as(&t.s, t.eaten, t.refill, 1), "advised {advised}");
        assert_eq!(chain_options(&t.s), Some(vec![t.eaten.min(10), t.eaten.max(10)]));
    }

    /// A chain with three targets and Avenger's Casket held (see [`a_meal_in_doubt_is_held_and_read_again`]): one is
    /// advised, `eaten` is eaten and refilled by the casket with a Magic `refill`, and `unsure` stays.
    struct ThreeTargets {
        s: Session,
        targets: Vec<(u8, Beast, u8, f32)>,
        advised: usize,
        eaten: usize,
        unsure: usize,
        refill: Beast,
    }

    impl ThreeTargets {
        /// The chain advised, its bite then told of by the game's log `ago` before now.
        fn bitten(ago: std::time::Duration) -> ThreeTargets {
            let mut targets = TWO_TARGETS.to_vec();
            targets.push((13, Beast::Wolf, 0, 1.0));
            let before = with_jaws(&scene_with(&targets), &[7, 9, 13]);
            let mut s = chain_on(&before);
            s.tools = vec![Tool::Revenge];
            installs(&mut s, &before, target_panels());
            assert_eq!(chain_options(&s), Some(vec![6, 8, 12]));
            s.wait();
            let Some(Action::Chain { to: advised }) = s.shown_advice().map(|a| a.action) else {
                panic!("the chain advised: {:?}", s.shown_advice());
            };
            let mut others = [6, 8, 12].into_iter().filter(|&n| n != advised);
            let (eaten, unsure) = (others.next().unwrap(), others.next().unwrap());
            let refill = if targets.iter().any(|&(c, b, ..)| usize::from(c) == eaten + 1 && b == Beast::Rat) { Beast::Wolf } else { Beast::Rat };
            s.note_game(Signal::Bite);
            s.bite_at = s.bite_at.map(|at| at - ago);
            s.reread = false;
            ThreeTargets { s, targets, advised, eaten, unsure, refill }
        }

        /// The read after the bite: the Lion risen to Magic for sure, the casket's new inmate on `eaten` unsure under
        /// the jaws, the inmate on `unsure` as before at confidence `unsure_conf`, the one advised as before for sure.
        fn read(&self, unsure_conf: f32) -> Scene {
            let mut now: Vec<(u8, Beast, u8, f32)> = self.targets.iter().copied().filter(|&(c, ..)| usize::from(c) != self.eaten + 1).collect();
            now[0] = (3, Beast::Lion, 1, 1.0);
            for cell in &mut now {
                if usize::from(cell.0) == self.unsure + 1 {
                    cell.3 = unsure_conf;
                }
            }
            now.push((self.eaten as u8 + 1, self.refill, 1, 0.11));
            with_jaws(&scene_with(&now), &[self.eaten as u8 + 1, 11])
        }
    }

    /// A meal still in doubt past BITE_SHOWS is not held: it is unknown (no meal), not guessed. The casket's new
    /// inmate under the jaws on one of the targets in doubt, read otherwise than before, is still taken as new, at the
    /// eater's rarity: left as the inmate from before the bite, the eater could have nothing to eat until its next
    /// bite.
    #[test]
    fn a_meal_in_doubt_past_bite_shows_is_unknown_and_its_refill_taken() {
        let mut t = ThreeTargets::bitten(BITE_SHOWS);
        let scene = t.read(0.11);
        installs(&mut t.s, &scene, target_panels());
        assert!(t.s.last_kept.is_none(), "past BITE_SHOWS the read is taken: {:?}", t.s.last_kept);
        assert_eq!(t.s.meal, None);
        let mut doubt = t.s.meal_doubt.clone();
        doubt.sort_unstable();
        assert_eq!(doubt, vec![t.eaten.min(t.unsure), t.eaten.max(t.unsure)]);
        assert!(reads_as(&t.s, t.eaten, t.refill, 1), "advised {}", t.advised);
        let unsure_beast = t.targets.iter().find(|&&(c, ..)| usize::from(c) == t.unsure + 1).map(|&(_, b, ..)| b).unwrap();
        assert!(reads_as(&t.s, t.unsure, unsure_beast, 0), "the target out of the jaws stays");
        assert!(!targets_differ(&t.s));
        assert_eq!(chain_options(&t.s), Some(vec![t.eaten.min(10), t.eaten.max(10)]));
        t.s.wait();
        assert!(matches!(t.s.shown_advice().map(|a| a.action), Some(Action::Chain { .. })));
    }

    /// With an eater that may eat across rarities (a Wolf with the Fanged Guillotine), the targets left uneaten are
    /// jawed again after the bite, their reads spoiled by the jaws: a meal in doubt past BITE_SHOWS leaves them as they
    /// were, not refilled at the eater's rarity as the casket's new inmate.
    #[test]
    fn a_meal_in_doubt_across_rarities_leaves_the_targets_jawed_again() {
        let targets = [(3, Beast::Wolf, 1, 1.0), (7, Beast::Rat, 1, 1.0), (9, Beast::Rat, 0, 1.0), (13, Beast::Rat, 0, 1.0)];
        let before = with_jaws(&scene_with(&targets), &[7, 9, 13]);
        let mut s = chain_on(&before);
        s.tools = vec![Tool::Revenge, Tool::Guillotine];
        installs(&mut s, &before, target_panels());
        assert_eq!(chain_options(&s), Some(vec![6, 8, 12]));
        s.wait();
        let Some(Action::Chain { to: advised }) = s.shown_advice().map(|a| a.action) else {
            panic!("the chain advised: {:?}", s.shown_advice());
        };
        let mut others = [6, 8, 12].into_iter().filter(|&n| n != advised);
        let (eaten, kept) = (others.next().unwrap(), others.next().unwrap());
        let tier_of = |n: usize| targets.iter().find(|&&(c, ..)| usize::from(c) == n + 1).map(|&(_, _, t, _)| t).unwrap();
        s.note_game(Signal::Bite);
        s.bite_at = s.bite_at.map(|at| at - BITE_SHOWS);
        // The Wolf risen to Rare; the advised Rat as before; the casket's new Rat on `eaten` and the Rat on `kept`
        // read unsure a rarity up under the jaws.
        let now = [
            (3, Beast::Wolf, 2, 1.0),
            (advised as u8 + 1, Beast::Rat, tier_of(advised), 1.0),
            (eaten as u8 + 1, Beast::Rat, 2, 0.11),
            (kept as u8 + 1, Beast::Rat, tier_of(kept) + 1, 0.11),
        ];
        let jaws: Vec<u8> = [advised, eaten, kept].iter().map(|&n| n as u8 + 1).collect();
        installs(&mut s, &with_jaws(&scene_with(&now), &jaws), target_panels());
        assert!(s.last_kept.is_none(), "{:?}", s.last_kept);
        assert_eq!(s.meal, None, "advised {advised}");
        assert!(s.meal_doubt.contains(&kept), "advised {advised}: {:?}", s.meal_doubt);
        assert!(reads_as(&s, kept, Beast::Rat, i64::from(tier_of(kept))), "advised {advised}: the Rat left uneaten stays");
    }

    /// Jaws read in part narrow the chain's targets to one (see `consume_targets`), which was then the meal for sure:
    /// the player eating the other target, unmarked on that read, took its cell for the one advised, as on 2026-11-06
    /// day 7. The targets the board allows count: the target eaten is the meal.
    #[test]
    fn a_bite_off_jaws_read_in_part_is_its_meal() {
        let before = with_jaws(&scene_with(&TWO_TARGETS), &[9]);
        let mut s = chain_on(&before);
        s.tools = vec![Tool::Revenge];
        installs(&mut s, &before, target_panels());
        assert_eq!(chain_options(&s), Some(vec![8]));
        assert!(targets_differ(&s));
        s.wait();
        assert_eq!(s.shown_advice().map(|a| a.action), Some(Action::Chain { to: 8 }));
        s.note_game(Signal::Bite);
        // The Alpha Wolf on 07 eaten, the casket's Magic Colossal Rat there unsure under the jaws.
        installs(&mut s, &two_targets_after(6, Some((Beast::Rat, 1, 0.11)), &[7, 11]), target_panels());
        assert!(s.last_kept.is_none(), "{:?}", s.last_kept);
        assert_eq!(s.meal, Some(Meal { cell: 6, certain: true }));
        assert!(reads_as(&s, 6, Beast::Rat, 1) && reads_as(&s, 8, Beast::Rat, 0));
        assert!(!targets_differ(&s));
        assert_eq!(chain_options(&s), Some(vec![6, 10]));
    }

    /// `meal_read` leaves the advice standing without evidence against it: with the bite not shown (the eater not
    /// risen, the only other target changed under the jaws), with the advised cell itself read unsure, and with
    /// another target read unsure but as before (unknown, never taken as the meal).
    #[test]
    fn meal_read_keeps_the_advice_without_evidence() {
        let model = |scene: &Scene| to_model(scene, &target_panels(), &[], None, RUN_DAYS);
        let (before, _) = model(&with_jaws(&scene_with(&TWO_TARGETS), &[7, 9]));
        let all = |_: usize| true;
        let read = |inmates: &[(u8, Beast, u8, f32)], jaws: &[u8]| model(&with_jaws(&scene_with(inmates), jaws));
        // Not shown: the Lion as before, cell 09 read a rarity up under the jaws.
        let (now, warns) = read(&[(3, Beast::Lion, 0, 1.0), (7, Beast::Wolf, 0, 1.0), (9, Beast::Rat, 1, 1.0), (11, Beast::Rat, 1, 1.0)], &[9]);
        let unsure = |warns: &[Warning]| warns.iter().any(|w| begins(&w.text, CHECK_CELL));
        assert!(!unsure(&warns));
        assert_eq!(meal_read(&before, &now, &warns, &[8], &[6, 8], 6, all), MealRead::Advised);
        // The advised cell unsure, the eater risen.
        let (now, warns) = read(&[(3, Beast::Lion, 1, 1.0), (7, Beast::Wolf, 0, 0.11), (11, Beast::Rat, 1, 1.0)], &[11]);
        assert!(unsure(&warns));
        assert_eq!(meal_read(&before, &now, &warns, &[10], &[6, 8], 6, all), MealRead::Advised);
        // The other target unsure but read as before, the eater risen: which went is unknown.
        let (now, warns) = read(&[(3, Beast::Lion, 1, 1.0), (7, Beast::Wolf, 0, 1.0), (9, Beast::Rat, 0, 0.11), (11, Beast::Rat, 1, 1.0)], &[11]);
        assert_eq!(meal_read(&before, &now, &warns, &[10], &[6, 8], 6, all), MealRead::Unknown(vec![8]));
        // And for sure as before: nothing against the advice (a refill that looks the same).
        let (now, warns) = read(&[(3, Beast::Lion, 1, 1.0), (7, Beast::Wolf, 0, 1.0), (9, Beast::Rat, 0, 1.0), (11, Beast::Rat, 1, 1.0)], &[11]);
        assert_eq!(meal_read(&before, &now, &warns, &[10], &[6, 8], 6, all), MealRead::Advised);
    }

    /// A decision card the reader named Replace only because the instrument icons read showed no Blood-Stained Trap
    /// is dropped when the Trap is known held and a Lion is on the warden's cell: it may be the Lion's Hunt, its name
    /// unread under the mouse. Without the Trap held, or with another beast there, it stands.
    #[test]
    fn a_replace_named_by_the_icons_alone_is_dropped_with_the_trap_held() {
        let lion = scene_with(&[(3, Beast::Lion, 1, 1.0), (7, Beast::Rat, 1, 1.0)]);
        let wolf = scene_with(&[(3, Beast::Wolf, 1, 1.0), (7, Beast::Rat, 1, 1.0)]);
        for (tools, scene, kept) in [(vec![Tool::Trap], &lion, false), (vec![], &lion, true), (vec![Tool::Trap], &wolf, true)] {
            let mut s = session_on(scene);
            s.tools = tools.clone();
            let mut manage = screen_panels(Screen::Manage, Some(10));
            manage.by_icons = vec!["Replace"];
            // A change of the board's buttons is held for one more read (see `CHANGED`).
            for _ in 0..2 {
                if !matches!(s.take_read(scene.clone(), manage.clone(), (REF_W, REF_H)), Taken::Kept(_)) {
                    break;
                }
            }
            let replace = s.reading.as_ref().map(|r| !r.model.swap_used);
            assert_eq!(replace, Some(kept), "tools {tools:?}, lion {}", std::ptr::eq(scene, &lion));
        }
    }

    /// With no instrument held, a bite the game's log told of with no read taken since (the read after it held, as
    /// the board does not show the bite yet) shows no advice: the reading on show is the board before the bite, and
    /// its advice named the meal just eaten for up to BITE_SHOWS. The first read taken after shows its own.
    #[test]
    fn no_advice_shows_between_a_bite_and_a_read_of_it() {
        let (before, _, jawed) = bite_boards();
        let mut s = chain_advised(|s| s.note_game(Signal::Bite));
        assert!(s.shown_advice().is_none());
        // The board before the bite read again: held, still no advice.
        installs(&mut s, &before, target_panels());
        assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, BITE_UNSHOWN)), "{:?}", s.last_kept);
        assert!(s.shown_advice().is_none() && s.mark().is_none());
        // The bite read: its advice shows.
        installs(&mut s, &jawed, target_panels());
        assert!(s.last_kept.is_none());
        s.wait();
        assert!(matches!(s.shown_advice().map(|a| a.action), Some(Action::Chain { to: 8 })));
    }

    /// With the Crowded Beastcage alone held, the cell just eaten opens again at once, so no inmate leaves the board:
    /// a read with the eater risen for sure shows the bite and is taken, not held for the whole BITE_SHOWS. A read
    /// with the eater in doubt is held and read again at once, as with the casket.
    #[test]
    fn a_beastcage_refill_with_the_eater_risen_is_taken_at_once() {
        let mut s = chain_advised(|s| {
            s.tools = vec![Tool::Beastcage];
            s.note_game(Signal::Bite);
        });
        s.reread = false;
        installs(&mut s, &refill_read((Beast::Mammoth, 1, 0.11)), target_panels());
        assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, BITE_UNSHOWN)), "{:?}", s.last_kept);
        assert!(s.reread && s.shown_advice().is_none());
        installs(&mut s, &refill_read((Beast::Lion, 1, 1.0)), target_panels());
        assert!(s.last_kept.is_none(), "{:?}", s.last_kept);
        assert!(reads_as(&s, 2, Beast::Lion, 1) && reads_as(&s, 6, Beast::Rat, 1));
        assert_eq!(chain_options(&s), Some(vec![6, 8]));
        s.wait();
        assert!(matches!(s.shown_advice().map(|a| a.action), Some(Action::Chain { to: 6 | 8 })));
    }

    /// With no new inmate in the cell just eaten (no casket, or none put there), a read with the eater in doubt is
    /// taken right after the bite as before, and asks for no read at once.
    #[test]
    fn an_eater_in_doubt_without_a_refill_is_taken_as_before() {
        let eaten = with_jaws(&scene_with(&[(3, Beast::Mammoth, 1, 0.11), (9, Beast::Rat, 1, 1.0)]), &[9]);
        for tools in [vec![], vec![Tool::Revenge]] {
            let mut s = chain_advised(|s| {
                s.tools = tools.clone();
                s.note_game(Signal::Bite);
            });
            s.reread = false;
            installs(&mut s, &eaten, target_panels());
            assert!(s.last_kept.is_none(), "tools {tools:?}: {:?}", s.last_kept);
            assert!(!s.reread, "tools {tools:?}");
            // The eater the jaws call for stands in, as it did.
            assert!(reads_as(&s, 2, Beast::Lion, 1), "tools {tools:?}");
        }
    }

    /// Two jawless reads in a row end the chain; jaws read after on the same board solve it again rather than pass
    /// for the same reading (2026-11-05 day 15: the jaws on cell 04 came 0.13 s after, and no advice showed).
    #[test]
    fn jaws_back_after_a_chain_ended_solve_again() {
        let (_, after, jawed) = bite_boards();
        for fresh_bite in [true, false] {
            let mut s = chain_advised(|s| {
                if fresh_bite {
                    s.bite_at = Some(std::time::Instant::now());
                }
            });
            if fresh_bite {
                installs(&mut s, &after, target_panels());
                assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, CHANGED)));
            }
            // A jawless read taken: the chain is over, no advice.
            installs(&mut s, &after, target_panels());
            assert!(s.last_kept.is_none(), "fresh bite {fresh_bite}");
            assert_eq!(chain_options(&s), None);
            assert!(s.shown_advice().is_none() && s.mark().is_none());
            // Another jawless read changes nothing, and is not held.
            installs(&mut s, &after, target_panels());
            assert!(s.last_kept.is_none() && chain_options(&s).is_none());
            // The jaws come on the same board: the first read of them waits for a second, which solves again
            // rather than passing for the same reading, and the chain goes on.
            installs(&mut s, &jawed, target_panels());
            assert!(chain_options(&s).is_none() && s.shown_advice().is_none());
            installs(&mut s, &jawed, target_panels());
            assert_eq!(chain_options(&s), Some(vec![8]));
            s.wait();
            assert!(s.shown_advice().is_some() && s.mark().is_some());
        }
    }

    /// After a chain a jawless read ended, one read of jaws (a misread: the reader looks for them on every frame)
    /// solves nothing; jaws in between that differ, or a jawless read, start the count again.
    #[test]
    fn one_jaws_misread_after_a_chain_ended_solves_nothing() {
        let (_, after, jawed) = bite_boards();
        let mut s = chain_advised(|_| {});
        installs(&mut s, &after, target_panels());
        assert_eq!(chain_options(&s), None);
        // Other jaws on the same board (a jaw read over the eater's own cell).
        let other = with_jaws(&after, &[3, 9]);
        for (k, scene) in [&jawed, &after, &jawed, &other, &jawed].into_iter().enumerate() {
            s.reread = false;
            installs(&mut s, scene, target_panels());
            // Each first read of new jaws asks for the next read at once.
            assert_eq!(s.reread, k != 1, "read {k}");
            assert_eq!(chain_options(&s), None, "read {k}");
            assert!(s.shown_advice().is_none() && s.mark().is_none(), "read {k}: {:?} {:?}", s.shown_advice().map(|a| a.action), s.mark_or_why());
        }
        installs(&mut s, &jawed, target_panels());
        assert_eq!(chain_options(&s), Some(vec![8]));
    }

    /// A jawless read held over a reading from before the bite (the game's log told of the bite, no read taken
    /// since) keeps the chain but shows no advice: the reading's would name the meal just eaten.
    #[test]
    fn a_jawless_read_held_over_the_board_before_the_bite_shows_no_advice() {
        let (_, after, jawed) = bite_boards();
        let mut s = chain_advised(|s| s.note_game(Signal::Bite));
        assert!(s.shown_advice().is_none(), "the advice before the bite, naming the meal just eaten, goes at the bite");
        installs(&mut s, &after, target_panels());
        assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, CHANGED)));
        assert_eq!(chain_options(&s), Some(vec![6]), "the chain kept");
        assert!(s.shown_advice().is_none() && s.mark().is_none());
        // The next read goes on with the chain.
        installs(&mut s, &jawed, target_panels());
        assert_eq!(chain_options(&s), Some(vec![8]));
        s.wait();
        assert!(s.shown_advice().is_some());
        // Without the game's log, the held read showing the bite on the board on show tells the same.
        let mut s = chain_advised(|s| s.tools = vec![Tool::Beastcage]);
        installs(&mut s, &after, target_panels());
        assert!(s.last_kept.as_deref().is_some_and(|line| is_line(line, CHANGED)));
        assert!(s.shown_advice().is_none());
    }

    /// Off a fresh bite and with no instrument that refills a cell, a jawless read ends the chain at once, as
    /// before; a target screen off a chain is never held for its jaws.
    #[test]
    fn a_jawless_read_off_a_fresh_bite_ends_the_chain_at_once() {
        let (before, after, _) = bite_boards();
        let mut s = chain_advised(|s| s.bite_at = std::time::Instant::now().checked_sub(BITE_SHOWS + std::time::Duration::from_millis(100)));
        installs(&mut s, &after, target_panels());
        assert!(s.last_kept.is_none());
        assert_eq!(chain_options(&s), None);
        assert!(s.shown_advice().is_none());
        // The day's first target screen, no bite yet: a read with no jaws is taken as it comes.
        let mut board = before.clone();
        board.cells[7 - 2].target = false;
        let mut s = session_on(&board);
        s.bite_at = Some(std::time::Instant::now());
        s.tools = vec![Tool::Revenge];
        installs(&mut s, &board, target_panels());
        assert!(s.last_kept.is_none());
        assert!(s.state.is_some());
    }

    /// An eater the jaws call for, read on two reads in a row with the same jaws, shows its mark; one such read does
    /// not (2026-11-05 day 7: "Chain on into 10" stayed unmarked on two reads 0.42 s apart).
    #[test]
    fn the_same_eater_taken_from_the_jaws_twice_shows_its_mark() {
        // A Normal Rat on cell 03 can't eat the Magic Mammoth the game marks on cell 07: the bite raised it to a
        // Magic Rat, its portrait not yet risen. Only a Rat eats a Mammoth.
        let board = scene_with(&[(3, Beast::Rat, 0, 1.0), (7, Beast::Mammoth, 1, 1.0), (9, Beast::Wolf, 0, 1.0)]);
        let jaws = with_jaws(&board, &[7]);
        let mut s = chain_on(&board);
        installs(&mut s, &jaws, target_panels());
        s.wait();
        assert!(s.reading.as_ref().is_some_and(|r| r.eater_inferred), "the eater taken from the jaws");
        assert!(matches!(s.reading.as_ref().map(|r| &r.model.cells[2]), Some(CellModel::Inmate { beast: Beast::Rat, tier: 1, .. })));
        assert_eq!(s.mark_or_why(), Err("inferred"));
        assert!(s.shown_advice().is_some(), "its advice shows");
        // The next read, the eater still read as before: the same eater from the same jaws, steady.
        installs(&mut s, &jaws, target_panels());
        s.wait();
        assert!(s.reading.as_ref().is_some_and(|r| !r.eater_inferred));
        assert!(s.shown_advice().is_some());
        assert!(s.mark().is_some(), "{:?}", s.mark_or_why());
        // A bite: the next eater taken from the jaws is a guess again.
        s.note_game(Signal::Bite);
        s.played_at = None;
        s.bite_at = std::time::Instant::now().checked_sub(BITE_SHOWS);
        installs(&mut s, &jaws, target_panels());
        s.wait();
        assert_eq!(s.mark_or_why(), Err("inferred"));
    }

    /// Within DIFFER_STEADY of the bite, the same eater taken from the same jaws on the next read is still a guess:
    /// the eater's portrait still changes (a Wolf so taken was a Lion 0.23 s on, 2026-11-02).
    #[test]
    fn the_same_eater_from_the_jaws_right_after_a_bite_is_still_a_guess() {
        let board = scene_with(&[(3, Beast::Rat, 0, 1.0), (7, Beast::Mammoth, 1, 1.0), (9, Beast::Wolf, 0, 1.0)]);
        let jaws = with_jaws(&board, &[7]);
        let mut s = chain_on(&board);
        s.bite_at = Some(std::time::Instant::now());
        installs(&mut s, &jaws, target_panels());
        s.wait();
        assert_eq!(s.mark_or_why(), Err("inferred"));
        // The next read takes the portrait mid-change for a Magic Wolf: solved again, the jaws call for the same
        // Magic Rat, but the bite is too fresh.
        let mut wolf = jaws.clone();
        wolf.cells[3 - 2].inmate = Some((Beast::Wolf, 1));
        installs(&mut s, &wolf, target_panels());
        s.wait();
        assert!(matches!(s.reading.as_ref().map(|r| &r.model.cells[2]), Some(CellModel::Inmate { beast: Beast::Rat, tier: 1, .. })));
        assert_eq!(s.mark_or_why(), Err("inferred"));
    }

    /// The same eater taken from other jaws on the next read is no steady read: those jaws are what it was taken
    /// from. The same jaws read once more make it steady.
    #[test]
    fn the_same_eater_from_other_jaws_is_still_a_guess() {
        // Magic Mammoths on cells 07 and 12, the game marking one of them at a time.
        let board = scene_with(&[(3, Beast::Rat, 0, 1.0), (7, Beast::Mammoth, 1, 1.0), (9, Beast::Wolf, 0, 1.0), (12, Beast::Mammoth, 1, 1.0)]);
        let mut s = chain_on(&board);
        installs(&mut s, &with_jaws(&board, &[7]), target_panels());
        s.wait();
        assert_eq!(s.mark_or_why(), Err("inferred"));
        let other = with_jaws(&board, &[12]);
        installs(&mut s, &other, target_panels());
        s.wait();
        assert!(matches!(s.reading.as_ref().map(|r| &r.model.cells[2]), Some(CellModel::Inmate { beast: Beast::Rat, tier: 1, .. })));
        assert_eq!(s.mark_or_why(), Err("inferred"));
        installs(&mut s, &other, target_panels());
        s.wait();
        assert!(s.mark().is_some(), "{:?}", s.mark_or_why());
    }

    #[test]
    fn a_warning_stays_through_a_hidden_countdown_and_goes_with_the_screen() {
        let size = (REF_W, REF_H);
        // The Rat on cell 07 read with a beast confidence of 0.2: a "Check cell" warning on its model index 6.
        let unsure = scene_with(&[(3, Beast::Mammoth, 0, 1.0), (7, Beast::Rat, 0, 0.2)]);
        let warned = |s: &Session| s.reading.as_ref().is_some_and(|r| r.warnings.iter().any(|w| w.cell == Some(6) && begins(&w.text, CHECK_CELL)));
        // Not live: no read is held.
        let mut s = Session::new(100, Vec::new(), None, Some(10));
        assert!(!matches!(s.take_read(unsure.clone(), screen_panels(Screen::Manage, Some(10)), size), Taken::Kept(_)));
        assert!(warned(&s));
        // The same result again with the countdown hidden, then read again: the warning stays on show.
        assert!(!matches!(s.take_read(unsure.clone(), screen_panels(Screen::Manage, None), size), Taken::Kept(_)));
        assert!(warned(&s));
        assert!(!matches!(s.take_read(unsure.clone(), screen_panels(Screen::Manage, Some(10)), size), Taken::Kept(_)));
        assert!(warned(&s));
        // Another screen: the warnings shown are dropped, and the same result does not warn anew.
        assert!(!matches!(s.take_read(unsure, screen_panels(Screen::CellPick, Some(10)), size), Taken::Kept(_)));
        assert!(!warned(&s));
    }

    #[test]
    fn a_missing_countdown_keeps_the_warnings_day() {
        let manage = Some((Screen::Manage, Some(10)));
        assert_eq!(warned_scope((Screen::Manage, None), manage), (Screen::Manage, Some(10)));
        // Another day or another screen is another scope.
        assert_eq!(warned_scope((Screen::Manage, Some(9)), manage), (Screen::Manage, Some(9)));
        assert_ne!(Some(warned_scope((Screen::CellPick, None), manage)), manage);
        assert_eq!(warned_scope((Screen::Manage, None), None), (Screen::Manage, None));
    }

    #[test]
    fn a_low_confidence_read_warns_only_when_it_changed() {
        let (normal, magic) = (inmate(Beast::Rat, 0), inmate(Beast::Rat, 1));
        // The same result as the reading before: no warning, however low the confidence.
        assert!(!warns_again(Some(&normal), Some(&normal)));
        // Another result, or the first read of the cell: it warns.
        assert!(warns_again(Some(&magic), Some(&normal)));
        assert!(warns_again(Some(&normal), None));
        // A warning about no cell always shows.
        assert!(warns_again(None, Some(&normal)));
        assert!(warns_again(None, None));
    }

    #[test]
    fn a_warning_shown_stays_on_the_next_read_of_the_same_result() {
        let key = "Check cell 07: Rat, ".to_string();
        // The first read of the cell's new result warns.
        let (flags, shown) = steady_shown(vec![(key.clone(), true)], &BTreeSet::new());
        assert_eq!(flags.get(&key), Some(&true));
        // The second read gives the same result, which alone would not warn: the warning stays on show, and so on
        // for the reads after it.
        let (flags, shown) = steady_shown(vec![(key.clone(), false)], &shown);
        assert_eq!(flags.get(&key), Some(&true));
        let (flags, shown) = steady_shown(vec![(key.clone(), false)], &shown);
        assert_eq!(flags.get(&key), Some(&true));
        // Once the reads no longer give it, it is gone, and the same result read again later does not warn anew.
        let (_, shown) = steady_shown(Vec::new(), &shown);
        assert!(shown.is_empty());
        let (flags, _) = steady_shown(vec![(key.clone(), false)], &shown);
        assert_eq!(flags.get(&key), Some(&false));
        // A warning never shown stays hidden while its result stays the same.
        let (flags, shown) = steady_shown(vec![(key.clone(), false)], &BTreeSet::new());
        assert_eq!(flags.get(&key), Some(&false));
        assert!(shown.is_empty());
    }

    #[test]
    fn a_bite_shows_when_a_meal_leaves_its_cell() {
        let before = board();
        // The screen before the bite: nothing shown, meal known or not.
        assert!(!bite_shown(&before, &before, Some(4), &[]));
        assert!(!bite_shown(&before, &before, None, &[]));
        // Mid-bite the eater's portrait misreads while the meal still shows
        // (2026-10-29: a Normal Rat for a Normal Mammoth).
        let mut misread = before.clone();
        misread.cells[2] = inmate(Beast::Rat, 0);
        assert!(!bite_shown(&before, &misread, Some(4), &[]));
        // With the meal unknown, the eater's change is all there is.
        assert!(bite_shown(&before, &misread, None, &[]));
        // The meal's cell locks: shown.
        let mut eaten = before.clone();
        eaten.cells[4] = CellModel::Sealed { event: None };
        assert!(bite_shown(&before, &eaten, Some(4), &[]));
        // Another cell than the meal advised left: the player ate that one.
        let mut other = before.clone();
        other.cells[6] = CellModel::Empty { event: None };
        assert!(bite_shown(&before, &other, Some(4), &[]));
        // The eater's own cell reading empty mid-bite is no bite shown.
        let mut eater_gone = before.clone();
        eater_gone.cells[2] = CellModel::Empty { event: None };
        assert!(!bite_shown(&before, &eater_gone, Some(4), &[]));
        // A target under the jaws read empty is no bite: the jaws spoil it.
        assert!(!bite_shown(&before, &other, Some(4), &[6]));
        // A scratched door eaten locks again.
        let mut door = before.clone();
        door.cells[8] = CellModel::Scratch { tier: 0, event: None };
        let mut opened = door.clone();
        opened.cells[8] = CellModel::Sealed { event: None };
        assert!(!bite_shown(&door, &door, Some(8), &[]));
        assert!(bite_shown(&door, &opened, Some(8), &[]));
    }

    /// A patrol read again with the warden's cell changed (an eater risen
    /// is the same on a chain's target screen) keeps the door advised while
    /// the door is still offered; a door gone, or the manage screen, does not.
    #[test]
    fn advice_still_open_stays_on_show_while_solving_again() {
        let mut model = board();
        (model.day, model.dest) = (3, vec![4, 6]);
        let reading = |model: &Model, screen| Reading { model: model.clone(), warnings: Vec::new(), screen, days_left: None, at: SystemTime::now(), eater_inferred: false };
        let advice = |cell| Advice { action: Action::Patrol { cell }, label: String::new(), why: String::new(), option: None };
        let mut s = Session::new(100, Vec::new(), None, None);
        let before = reading(&model, Screen::Patrol);
        model.cells[2] = inmate(Beast::Mammoth, 2);
        s.reading = Some(reading(&model, Screen::Patrol));
        s.state = Some(build_state(&model, SEED).0);
        s.solving = true;
        s.held = Some((held_key(&before, 0), advice(4)));
        assert_eq!(s.shown_advice().map(|a| a.action), Some(Action::Patrol { cell: 4 }));
        s.held = Some((held_key(&before, 0), advice(8)));
        assert!(s.shown_advice().is_none(), "a door not offered");
        s.held = Some((held_key(&reading(&board(), Screen::Manage), 0), advice(4)));
        s.reading = Some(reading(&model, Screen::Manage));
        assert!(s.shown_advice().is_none(), "the manage screen");
    }

    #[test]
    fn a_card_missed_on_the_patrol_on_show_is_the_door_read_before() {
        let mut model = board();
        (model.warden, model.dest) = (5, vec![6, 9, 11]);
        let was = Reading { model, warnings: Vec::new(), screen: Screen::Patrol, days_left: Some(1), at: SystemTime::now(), eater_inferred: false };
        let mut doors = vec![Some(7), None, Some(12)];
        fill_doors(&mut doors, &was, Some(1), 6);
        assert_eq!(doors, vec![Some(7), Some(10), Some(12)]);
        // Another day, warden's cell or card read is another patrol.
        for (days, warden, first) in [(Some(2), 6, Some(7)), (Some(1), 7, Some(7)), (Some(1), 6, Some(8))] {
            let mut doors = vec![first, None, Some(12)];
            fill_doors(&mut doors, &was, days, warden);
            assert_eq!(doors[1], None);
        }
        // Fewer cards than before: a day can offer fewer.
        let mut doors = vec![Some(7), None];
        fill_doors(&mut doors, &was, Some(1), 6);
        assert_eq!(doors, vec![Some(7), None]);
    }

    #[test]
    fn an_instrument_acting_as_the_day_starts_holds_its_patrol() {
        let mut s = Session::new(100, Vec::new(), None, None);
        s.note_game(Signal::Instrument);
        assert!(s.effect_until.is_none(), "no day start");
        s.note_day(16, 20);
        s.note_game(Signal::Instrument);
        let now = std::time::Instant::now();
        assert!(s.effect_until.is_some_and(|at| at > now + EFFECT_SHOWS - std::time::Duration::from_millis(50)));
        assert!(s.fresh_at >= s.effect_until);
    }

    /// On a board with no locked cell left the Thumb Ring changes nothing: a
    /// Crimson Vintage tied with it is advised, even with the Ring on show
    /// from the treasure before.
    #[test]
    fn a_treasure_that_changes_nothing_is_not_kept() {
        let mut m = board();
        for cell in &mut m.cells[1..] {
            if matches!(cell, CellModel::Sealed { .. }) {
                *cell = CellModel::Empty { event: None };
            }
        }
        let mut st = build_state(&m, SEED).0;
        st.pending = vec![Pending::Gift { options: Gift::ALL.to_vec(), picks: 1 }];
        assert!(st.sealed_cells().is_empty());
        let row = |gift: Gift, mean: f64, gap_se: f64| SolveOption {
            label: gift.key().to_string(),
            action: Action::Choose { id: ChoiceId::Name(gift.key().to_string()) },
            mean,
            se: 0.05,
            gap_se,
            n: 1600,
            best: 0.0,
            policy: false,
        };
        let mut s = Session::new(100, Vec::new(), None, None);
        s.state = Some(st);
        s.ranking = vec![row(Gift::Ring, 11.49, 0.0), row(Gift::Wine, 11.47, 0.05), row(Gift::Horn, 11.0, 0.05)];
        s.shown = Some(s.ranking[0].action.clone());
        assert_eq!(s.advice().map(|a| a.action), Some(s.ranking[1].action.clone()));
    }

    /// An eater taken from the jaws mid-change shows its advice but no mark;
    /// a read that shows the eater brings the mark back.
    #[test]
    fn an_eater_taken_from_the_jaws_shows_no_mark() {
        let reading = Reading { model: board(), warnings: Vec::new(), screen: Screen::ChainTarget, days_left: None, at: SystemTime::now(), eater_inferred: true };
        let panels = Panels {
            screen: Screen::ChainTarget,
            days: None,
            doors: Vec::new(),
            decision: vec!["Cancel"],
            instruments: Vec::new(),
            rerolls: 0,
            vault: Vec::new(),
            tool_icons: false,
            theme: None,
            incident: None,
            objective: None,
            held: None,
            spots: vec![(Spot::Button("Cancel"), Layout::Board, (2300, 1070))],
            gift_picked: false,
            card_red: [None; 7],
            doors_fixed: false,
            by_icons: Vec::new(),
        };
        let advice = Advice { action: Action::EndChain, label: String::new(), why: String::new(), option: None };
        let mut s = Session::new(100, Vec::new(), None, None);
        s.scene = Some((Scene::default(), panels, SystemTime::now()));
        s.on_screen = true;
        s.ui_scale = Some(1.0);
        s.reading = Some(reading.clone());
        s.solving = true;
        s.held = Some((held_key(&reading, s.gifts_taken), advice));
        assert!(s.shown_advice().is_some(), "the advice shows");
        assert!(s.mark().is_none(), "no mark on an eater taken from the jaws");
        s.reading = Some(Reading { eater_inferred: false, ..reading });
        assert!(s.mark().is_some(), "the eater read: the mark shows");
    }

    /// The advice on an eater taken from the jaws is not held: the eater
    /// read as another beast solves again with nothing of it on show.
    #[test]
    fn advice_on_an_eater_taken_from_the_jaws_is_not_held() {
        let reading = Reading { model: board(), warnings: Vec::new(), screen: Screen::ChainTarget, days_left: None, at: SystemTime::now(), eater_inferred: true };
        let end = SolveOption { label: String::new(), action: Action::EndChain, mean: 9.0, se: 0.05, gap_se: 0.0, n: 1600, best: 0.0, policy: false };
        let mut s = Session::new(100, Vec::new(), None, None);
        s.ranking = vec![end];
        s.reading = Some(reading.clone());
        s.note_shown();
        assert!(s.held.is_none(), "not held on an eater taken from the jaws");
        s.reading = Some(Reading { eater_inferred: false, ..reading });
        s.note_shown();
        assert!(s.held.is_some(), "held once the eater reads");
    }

    /// The eater a chain's target screen read stands until the next bite or
    /// decision: the next eater is a guess again.
    #[test]
    fn the_eater_seen_goes_with_a_bite_or_a_decision() {
        let mut s = Session::new(100, Vec::new(), None, None);
        let lion = CellModel::Inmate { beast: Beast::Lion, tier: 2, ivory: false, prey: None, event: None };
        assert_eq!(eater_in(&lion, 14), Some((14, Beast::Lion, 2)));
        assert_eq!(eater_in(&CellModel::Empty { event: None }, 14), None);
        for signal in [Signal::Bite, Signal::Decision] {
            s.eater_seen = eater_in(&lion, 14);
            s.note_game(signal);
            assert_eq!(s.eater_seen, None);
        }
    }

    /// Paused during Vorax: no screen reads, no game log, no marks; after resume the run's state
    /// is kept, and chains and reads start from scratch.
    #[test]
    fn a_suspended_session_reads_and_notes_nothing() {
        let mut s = Session::new(100, vec![Tool::Coin], Some(CAGE_FIGHT), None);
        s.note_day(5, 20);
        s.note_game(Signal::Consume);
        s.note_game(Signal::Bite);
        s.gifts_taken = 2;
        s.suspend();
        assert!(s.suspended());
        assert_eq!(s.log_bites, None);
        s.note_game(Signal::Bite);
        s.note_day(6, 20);
        s.note_treasure();
        s.read_in_background(Rgba { w: 4, h: 4, data: vec![0; 64] });
        assert!(!s.scanning());
        assert_eq!((s.log_bites, s.log_day, s.gifts_taken), (None, Some((5, 20)), 2));
        assert!(s.mark().is_none() && s.notice().is_none() && s.chain_spots().is_empty());
        s.resume();
        assert!(!s.suspended());
        assert_eq!((s.tools.as_slice(), s.theme, s.log_day, s.gifts_taken), (&[Tool::Coin][..], Some(CAGE_FIGHT), Some((5, 20)), 2));
        s.note_game(Signal::Bite);
        assert!(s.log_bites.is_some());
    }

    /// In Cage Fight, when the eater is read as Ultimate, the chain numbers are cut to 1 bite; lower rarities, a full
    /// vault, the Elephant-Foot Maul's Mammoth, the Fanged Guillotine's Wolf, and non-Cage-Fight runs are not cut.
    #[test]
    fn a_cage_fight_chain_shows_only_the_bites_the_eater_can_take() {
        use crate::engine::state::{Inmate, VaultEntry};
        let plan = |beast: Beast, tier: i32, tools: Vec<Tool>, full: bool, theme: Option<u32>| {
            let mut st = build_state(&board(), SEED).0;
            let at = st.position;
            st.cells[at].inmate = Some(Inmate { id: 900, beast, tier });
            st.tools = tools;
            if full {
                st.params.vault_cap = Some(1);
                st.vault = vec![VaultEntry { id: 901, beast, day: 1 }];
            }
            let mut s = Session::new(100, Vec::new(), theme, None);
            s.state = Some(st);
            s.chain_plan = Some((Action::Chain { to: 3 }, vec![3, 7, 8]));
            s.chain_plan().to_vec()
        };
        let cage = Some(CAGE_FIGHT);
        assert_eq!(plan(Beast::Lion, RAINBOW - 3, Vec::new(), false, cage), vec![3, 7, 8]);
        assert_eq!(plan(Beast::Lion, RAINBOW - 2, Vec::new(), false, cage), vec![3, 7, 8]);
        assert_eq!(plan(Beast::Lion, RAINBOW - 1, Vec::new(), false, cage), vec![3]);
        assert_eq!(plan(Beast::Lion, RAINBOW - 1, Vec::new(), true, cage), vec![3, 7, 8]);
        assert_eq!(plan(Beast::Mammoth, RAINBOW - 1, vec![Tool::Maul], false, cage), vec![3, 7, 8]);
        assert_eq!(plan(Beast::Wolf, RAINBOW - 1, vec![Tool::Guillotine], false, cage), vec![3, 7, 8]);
        assert_eq!(plan(Beast::Lion, RAINBOW - 1, Vec::new(), false, None), vec![3, 7, 8]);
    }

    /// 2026-11-02 day 7: when the "target mismatch" warning was issued the game marked 11 and 13 (11 was the jaws
    /// left over from the previous bite); once the leftover cleared and only 13 remained, it no longer counts as the
    /// same read. No comparison when there is no such warning.
    #[test]
    fn jaws_changed_since_the_targets_differed_solve_again() {
        let scene = |cells: &[u8]| {
            let mut scene = Scene { cells: vec![Default::default(); 15], ..Default::default() };
            for &c in cells {
                scene.cells[c as usize - 2].target = true;
            }
            scene
        };
        let panels = Panels {
            screen: Screen::ChainTarget,
            days: None,
            doors: Vec::new(),
            decision: vec!["Cancel"],
            instruments: Vec::new(),
            rerolls: 0,
            vault: Vec::new(),
            tool_icons: false,
            theme: None,
            incident: None,
            objective: None,
            held: None,
            spots: Vec::new(),
            gift_picked: false,
            card_red: [None; 7],
            doors_fixed: false,
            by_icons: Vec::new(),
        };
        let mut s = Session::new(100, Vec::new(), None, None);
        s.scene = Some((scene(&[11, 13]), panels.clone(), SystemTime::now()));
        assert_eq!(jawed(&s.scene.as_ref().unwrap().0), vec![10, 12]);
        assert!(!s.jaws_moved(), "no warning, nothing to compare");
        s.differ_jaws = Some(vec![10, 12]);
        assert!(!s.jaws_moved(), "the same jaws");
        s.scene = Some((scene(&[13]), panels, SystemTime::now()));
        assert!(s.jaws_moved(), "the leftover jaws on 11 went");
    }

    /// A gift confirmed ends its pick: the pick, read again as it fades out,
    /// is held no more, and the next treasure in a row shows nothing from it.
    #[test]
    fn a_gift_confirmed_is_not_held_for_the_next_pick() {
        let reading = Reading { model: board(), warnings: Vec::new(), screen: Screen::TreasurePick, days_left: None, at: SystemTime::now(), eater_inferred: false };
        let advice = Advice { action: Action::Choose { id: ChoiceId::Name("wine".into()) }, label: String::new(), why: String::new(), option: None };
        let mut s = Session::new(100, Vec::new(), None, None);
        s.reading = Some(reading.clone());
        s.solving = true;
        s.held = Some((held_key(&reading, s.gifts_taken), advice.clone()));
        assert!(s.shown_advice().is_some());
        s.note_treasure();
        assert!(s.shown_advice().is_none());
        s.held = Some((held_key(&reading, s.gifts_taken), advice));
        assert!(s.shown_advice().is_some(), "held on the pick read after the gift");
        // The pick read again as it fades out, after its gift, holds nothing;
        // nor does a read begun before the next pick opened.
        let before = std::time::Instant::now();
        assert!(s.stale_pick(&reading));
        s.note_game(Signal::TreasureOpen);
        s.reading_scan = Some(before);
        assert!(s.stale_pick(&reading));
        s.reading_scan = Some(std::time::Instant::now());
        assert!(!s.stale_pick(&reading));
    }

    #[test]
    fn chromatics_flown_into_the_vault_are_treasures_left() {
        let mut s = Session::new(100, Vec::new(), None, None);
        for _ in 0..6 {
            s.note_game(Signal::Vaulted);
        }
        assert_eq!(s.flown_unpaid(), 6);
        s.note_treasure();
        s.note_treasure();
        assert_eq!(s.flown_unpaid(), 4);
        s.theme = Some(crate::engine::data::OPULENCE);
        assert_eq!(s.flown_unpaid(), 6, "two gifts are one Opulence treasure's picks, none paid in full");
    }

    /// A gift confirmed with no board shown since is played out by the next
    /// pick's solve, as advised; a pick whose advice is not a gift adds none.
    #[test]
    fn gifts_confirmed_unseen_are_kept_for_the_next_pick() {
        let reading = Reading { model: board(), warnings: Vec::new(), screen: Screen::TreasurePick, days_left: None, at: SystemTime::now(), eater_inferred: false };
        let mut s = Session::new(100, Vec::new(), None, None);
        s.reading = Some(reading);
        s.shown = Some(Action::Choose { id: ChoiceId::Name(Gift::Brand.key().to_string()) });
        s.note_treasure();
        s.note_treasure();
        assert_eq!(s.unseen_gifts, vec![Gift::Brand, Gift::Brand]);
        s.shown = Some(Action::End);
        s.note_treasure();
        assert_eq!(s.unseen_gifts.len(), 2);
    }

    /// A Cage Fight Rat, risen to Rare and read mid-change as another beast,
    /// stands in as the Rare Rat whose targets are the jawed cells, not as a
    /// Legendary Rat that would also take the Legendary Mammoth.
    #[test]
    fn an_eater_read_risen_stands_in_at_its_rarity() {
        let mut m = Model::blank();
        m.warden = 2;
        m.cells[2] = inmate(Beast::Wolf, 2);
        for (n, tier) in [(1, 0), (3, 1), (7, 0), (12, 3), (13, 2)] {
            m.cells[n] = inmate(Beast::Mammoth, tier);
        }
        let mut st = build_state(&m, SEED).0;
        st.theme = Some(CAGE_FIGHT);
        let (_, eater) = eater_from_jaws(&st, &[1, 3, 7, 13], Some(Beast::Rat)).expect("an eater");
        assert_eq!((eater.beast, eater.tier), (Beast::Rat, 2));
    }

    #[test]
    fn a_wolf_the_guillotine_may_keep_from_rising_is_not_held() {
        // A Legendary Wolf with the Fanged Guillotine eats a Magic Rat; a
        // Rare Rat is left for it (2026-10-31, day 15).
        let mut before = Model::blank();
        (before.warden, before.tools) = (2, vec![Tool::Guillotine]);
        before.cells[2] = inmate(Beast::Wolf, 4);
        before.cells[4] = inmate(Beast::Rat, 1);
        before.cells[7] = inmate(Beast::Rat, 3);
        let mut read = before.clone();
        read.cells[4] = CellModel::Sealed { event: None };
        assert!(eater_misread(&before, &read));
        // The jaws on the unrisen Wolf's one target: it did not rise.
        assert!(kept_from_rising(&before, &read, 4, &[7]));
        // Jaws the Wolf as read does not take, or none read: its rise is to come.
        assert!(!kept_from_rising(&before, &read, 4, &[5, 7]));
        assert!(!kept_from_rising(&before, &read, 4, &[]));
        // Without the Guillotine every bite raises the eater.
        (before.tools, read.tools) = (Vec::new(), Vec::new());
        before.cells[4] = inmate(Beast::Rat, 4);
        assert!(!kept_from_rising(&before, &read, 4, &[7]));
    }

    #[test]
    fn an_eater_read_no_higher_is_mid_rise() {
        let before = board();
        let mut read = board();
        // Risen, its beast re-rolled, as a bite does: the read stands.
        read.cells[2] = inmate(Beast::Wolf, 1);
        assert!(!eater_misread(&before, &read));
        // No higher: its rise was on show.
        read.cells[2] = inmate(Beast::Rat, 0);
        assert!(eater_misread(&before, &read));
        let mut risen = board();
        risen.cells[2] = inmate(Beast::Mammoth, 2);
        read.cells[2] = inmate(Beast::Lion, 1);
        assert!(eater_misread(&risen, &read));
    }

    #[test]
    fn consume_targets_keep_the_marked_ones() {
        // The screen marks 5 of the board's 5 and 7: only 5.
        let (targets, warning) = consume_targets(&[4, 6], &[4], 2);
        assert_eq!(targets, vec![4]);
        assert!(warning.is_some());
        // The same cells: no warning.
        assert_eq!(consume_targets(&[4, 6], &[6, 4], 2), (vec![4, 6], None));
    }

    #[test]
    fn a_read_begun_before_the_cards_board_gives_way() {
        let mut s = Session::new(100, Vec::new(), None, None);
        let now = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        s.note_game(Signal::CardsUp(3, 0));
        s.fresh_at = Some(now - ms(10));
        s.scan_started = Some(now - ms(20));
        assert!(s.scan_stale());
        // Begun once the board was in: it stands.
        s.scan_started = Some(now - ms(5));
        assert!(!s.scan_stale());
        // The board not in yet: a frame now is no better.
        s.fresh_at = Some(now + ms(50));
        s.scan_started = Some(now - ms(60));
        assert!(!s.scan_stale());
        // A day's doors lit up: a read begun under the banner gives way.
        s.note_game(Signal::Decision);
        assert!(!s.scan_stale());
        s.scan_started = Some(now - ms(30));
        s.note_day(5, 20);
        assert!(s.fresh_at.is_some_and(|at| at > now + DOORS_SHOW - ms(50)));
        assert!(!s.scan_stale());
        // A bite: frames show it only BITE_READABLE on, a Consume's targets
        // CONSUME_SHOWS on.
        s.note_game(Signal::Bite);
        assert!(s.fresh_at.is_some_and(|at| at > now + BITE_READABLE - ms(50)));
        assert!(!s.scan_stale());
        s.note_game(Signal::Consume);
        assert!(s.fresh_at.is_some_and(|at| at > now + CONSUME_SHOWS - ms(50) && at < now + BITE_READABLE));
    }

    #[test]
    fn frames_are_observed_in_the_prison_only() {
        let mut watch = Watch::default();
        // Started in the hideout: its reads are news, but no board.
        watch.report(true, true);
        assert!(!watch.observing());
        watch.report(true, false);
        assert!(watch.observing());
        // A screen without the board in the prison keeps it on.
        watch.report(true, true);
        watch.report(true, true);
        assert!(watch.observing());
        watch.report(true, true);
        assert!(!watch.observing());
    }

    #[test]
    fn consumes_click_the_same_card() {
        assert!(same_card(&Action::Devour { to: 3 }, &Action::Devour { to: 7 }));
        assert!(same_card(&Action::Hunt { cell: 3 }, &Action::Hunt { cell: 7 }));
        assert!(!same_card(&Action::Devour { to: 3 }, &Action::Hunt { cell: 3 }));
        assert!(!same_card(&Action::Patrol { cell: 3 }, &Action::Patrol { cell: 7 }));
        assert!(same_card(&Action::End, &Action::End));
    }
}
