//! Live advice for the Solver (src/strategy/advisor.ts): given what is on screen, rank the choices by expected
//! payoff. Read-only: it only says what it would play; nothing here acts on the game.

use crate::bot::*;
use crate::data::{data, Targeting};
use crate::game::{can_refresh, card_changes, create_game, ultimate_activity, LAST_POTION_ROUND};
use crate::types::*;
use serde::{Deserialize, Serialize, Serializer};
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenMonster {
    pub archetype: Archetype,
    pub rarity: Rarity,
    pub activity: i64,
    pub quantity: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScreenPhase {
    Potion,
    Instrument,
    Protocol,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenState {
    /// 6 entries, left to right; null = empty tank
    pub tanks: Vec<Option<ScreenMonster>>,
    /// card names as printed, left to right
    pub hand: Vec<String>,
    /// names of equipped instruments, in the order they were picked (added by Ascendant)
    ///
    /// Pick order (needed by the rules Ascendant added): under `Rules::GAME` the end-of-round instruments fire "last picked first"
    /// (`Rules::end_order_latest_first`). The order comes from `instrument_rounds` ascending; within the same round (or with all
    /// instruments treated as round 1 when no rounds are given) it follows the order of this list. So:
    /// - If the pick order is known, list the instruments in pick order (first picked first), or supply all `instrument_rounds`.
    /// - When the pick order cannot be read, fall back to the held icons at the top right of the screen, listed **left to right**. The game
    ///   adds a newly picked icon at the far right (ranked game frames2, t012.80 -> t019.00 -> t041.30 -> t051.50: `蒸骨坩堝`, Chrysalis Specimen,
    ///   Black Goat Gut Suture [id 14, 8k], Clustered Eggs [id 9, 28k]; the suture sits before the eggs with the smaller id, so it is not id order),
    ///   so left to right is the pick order.
    /// - If only some of the pick rounds are known, give none of them: unspecified ones count as round 1 and go to the front.
    pub instruments: Vec<String>,
    /// the round each instrument was picked in; round 1 when absent or 0
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument_rounds: Option<Vec<i32>>,
    pub phase: ScreenPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub potion_refreshes: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument_refreshes: Option<i32>,
    /// Intern Shelley level (only changes the instrument rerolls a new game starts with)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shelley: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct End {
    pub activity: f64,
    pub reached: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ranked {
    pub index: usize,
    pub targets: Vec<u8>,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<End>,
    /// the card changes nothing on this board (it only adds a monster, and the tanks are full)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_effect: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Best {
    Card(Ranked),
    Refresh,
    /// the choice doesn't change the score
    Nothing,
}

impl Serialize for Best {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Best::Card(r) => r.serialize(s),
            Best::Refresh => s.serialize_str("refresh"),
            Best::Nothing => s.serialize_none(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Advice {
    pub best: Best,
    pub ranking: Vec<Ranked>,
    pub board: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_end: Option<End>,
    /// true: potions judged by look-ahead to the end of the run; false: expected score after this round
    pub plan: bool,
    #[serde(rename = "final")]
    pub final_: bool,
    pub ms: u64,
    pub unknown: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Potion look-ahead: each card on offer, the next best options up to 4, and a reroll, each played to the end 64 times.
pub const LOOK_AHEAD: LookAhead = LookAhead { top: 4, playouts: 64, steer: Some(0.3), inner_samples: None, value: Some(true), halving: None };
/// Futures per instrument choice, each judged by its final score counted up to 840k.
pub const INSTRUMENT_FUTURES: u32 = 64;
/// How the rounds inside an instrument choice's futures are played: by the smarter player (the board value).
const INSTRUMENT_ROLLOUT: Policy = Policy { samples: 4, horizon: 0, roll_samples: None, steer: None, value: Some(true) };
/// A few rounds of look-ahead, only to order options that score the same this round.
const TIE_BREAK: Policy = Policy::new(32, 4);
/// How close a do-nothing move's value must be for a move that does something to go first instead.
const NO_EFFECT_MARGIN: f64 = 0.01;
const SEED: u32 = 0x5eed;

/// JavaScript's Math.round (halves up).
pub fn js_round(x: f64) -> f64 {
    let f = x.floor();
    if x - f >= 0.5 {
        f + 1.0
    } else {
        f
    }
}

pub fn is_xenopanacea(name: &str) -> bool {
    name.trim().to_lowercase().starts_with("xenopanacea")
}

/// A Supreme potion the simulator can't play out, always the advice when offered: a Xenopanacea (its card prints
/// only flavour text), a Holy Water rite ("… Holy Water: …", whose effect is another card later; Purest Holy
/// Water, with no colon, is simulated) or a Dissection (flavour text too, one after another for five rounds: a game
/// that took them all, Nov 4, 2026, ended past 18M against the 840k played for).
/// The Dissection rule is from vorax-solver-rs 0.3.0 (DinoRaos, MIT), crates/vorax/src/advisor.rs `is_always_take`.
/// Whether the card belongs to the always-take set; it does not look at the round. Whether a chain card can still be
/// finished is `fits_potion_rounds`, and the advice takes it only when both hold.
pub fn is_always_take(name: &str) -> bool {
    is_xenopanacea(name) || name.to_lowercase().contains("holy water:") || name.trim().to_lowercase().starts_with("dissection")
}

/// The chain potions: each card and how many steps are still to come after taking it. The next step is merged into
/// the next round's hand (5 cards with 2 extra potions, 4 with 1) without adding a round, and each step uses that
/// round's one pick, so a chain only finishes when the steps left fit in the potion rounds left.
const CHAINS: &[(&str, u8)] = &[
    // Holy Water rites. Provisional: that the next rite always comes in the very next round has no in-game evidence
    // yet. The second rite's English name starts with "Ritual", not "Second Rite".
    ("First Rite Holy Water: Confession", 3),
    ("Ritual Holy Water: Flagellation", 2),
    ("Third Rite Holy Water: Illumination", 1),
    ("Fourth Rite Holy Water: Eucharist", 0),
    // Dissections. Provisional: the order is inferred from the names and from upstream's screenshots of the 4-card
    // hands in rounds 7 and 13.
    ("Dissection Infusion: Morbid Corpse", 4),
    ("Dissection I: Vermectomy", 3),
    ("Dissection II: Daemonectomy", 2),
    ("Dissection III: Craniotomy", 1),
    ("Dissection Infusion: Intact Skeleton", 0),
    // Offspring, seen in a game (Nov 6, 2026, the 07-54-44 run, rounds 5 to 9). Not always taken (`is_always_take` is
    // false): the simulator can't play them, so they only get a note.
    ("Incubation Elixir: Hollow Cocoon", 4),
    ("F1 Gen: Parasite Egg", 3),
    ("F1 Gen: Vermin Egg Cluster", 3),
    ("F2 Gen: Parasite Pupae", 2),
    ("F2 Gen: Vermin Pupal Cluster", 2),
    ("F3 Gen: Parasitic Butterfly", 1),
    ("F3 Gen: Emergent Butterfly Swarm", 1),
    ("Incubation Elixir: Hollow Matriarch", 0),
];

/// How many chain steps are still to come after taking this card; None for a card that is not part of a chain (a
/// Xenopanacea works at once and is not one).
pub fn chain_steps_after(name: &str) -> Option<u8> {
    let name = name.trim();
    CHAINS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|&(_, steps)| steps)
}

/// The potion rounds after `round` (0 in the last one, never below 0).
fn potion_rounds_left(round: i32) -> i32 {
    (LAST_POTION_ROUND - round).max(0)
}

/// Whether the chain this card starts or continues can still be finished when it is taken in `round`: the steps left
/// must fit in the potion rounds after it (up to `LAST_POTION_ROUND`). True for a card outside a chain, and when the
/// round is not known (the advice then keeps its old behaviour).
pub fn fits_potion_rounds(name: &str, round: Option<i32>) -> bool {
    match (chain_steps_after(name), round) {
        // Same clamp as the note, so a misread round past the last potion round still lets a last step be taken.
        (Some(steps), Some(r)) => i32::from(steps) <= potion_rounds_left(r),
        _ => true,
    }
}

/// The card on screen that is taken without simulating (always take, and its chain can still be finished).
fn always_take_now(x: &ScreenState) -> Option<usize> {
    if x.phase != ScreenPhase::Potion {
        return None;
    }
    x.hand.iter().position(|n| is_always_take(n) && fits_potion_rounds(n, x.round))
}

/// The note for a potion hand the simulator compares as usual: one line for each offspring card (it can't be
/// simulated) and for each always-take chain card that can no longer be finished (it is not taken). None when there
/// is none.
pub fn chain_notes(x: &ScreenState) -> Option<String> {
    if x.phase != ScreenPhase::Potion {
        return None;
    }
    let left = |r: i32| {
        let n = potion_rounds_left(r);
        format!("{n} potion round{} left", if n == 1 { "" } else { "s" })
    };
    let mut lines: Vec<String> = Vec::new();
    for name in &x.hand {
        let Some(steps) = chain_steps_after(name) else { continue };
        let line = if !is_always_take(name) {
            match x.round {
                Some(r) => format!("{name}: can\u{2019}t be simulated; {}, the chain needs {steps}.", left(r)),
                None => format!("{name}: can\u{2019}t be simulated."),
            }
        } else {
            match x.round {
                Some(r) if !fits_potion_rounds(name, x.round) => format!("{name}: {}, the chain needs {steps}, so it is not taken.", left(r)),
                _ => continue,
            }
        };
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn lookup(phase_potion: bool, name: &str) -> Option<u8> {
    if phase_potion {
        data().potion_by_name(name)
    } else {
        data().instrument_by_name(name)
    }
}

fn is_kit(s: &GameState, index: usize) -> bool {
    s.hand.get(index).is_some_and(|c| c.kind == CardKind::Potion && matches!(data().potion(c.id).targeting, Targeting::Box(_)))
}

/// How many cards the potion kit opens (None if it is not a potion kit).
fn kit_size(s: &GameState, o: &Opt) -> Option<u8> {
    let Opt::Card { index, .. } = o else { return None };
    let c = s.hand.get(*index as usize).filter(|c| c.kind == CardKind::Potion)?;
    match data().potion(c.id).targeting {
        Targeting::Box(n) => Some(n),
        _ => None,
    }
}

/// A larger potion kit is never worse than a smaller one: opening more only gives more cards to choose from, so its expectation is never below that of the smaller. Checked in the engine
/// (2026-11-04): choosing a potion kit only sets `open_box` to it and deals n cards (play in `game.rs`; `deal_potions`
/// always uses the same box_pool and box_offer); it does not spend a reroll or end the round; a reroll inside the kit still spends
/// `potion_refreshes` and redeals the same n cards; the other uses of `open_box` (bot.rs) only check whether a box is open.
/// The potion kits in the game data come in only two sizes: 5 cards (large) and 3 cards (small, rare small). If potion kits ever differ in
/// anything besides size (different pools, costs), this rule needs a second look.
///
/// When simulation error (racing stopping one of them early) ranks a large kit behind a small one (2026-11-04 ranked game round 6:
/// with 4096 futures small 806390, large 789038; with 260k futures large 796845, small 794339), pull the large one up to equal
/// the small one (future count, mean and target rate all copied from the small one); on a tie the large one goes first. Processed from small to large, so with
/// three sizes large >= medium >= small is also guaranteed. `ahead` is (option, mean, target rate, future count).
fn kits_dominate(s: &GameState, ahead: &mut [(Opt, f64, f64, u32)]) {
    let key = |a: &(Opt, f64, f64, u32)| (a.3, a.1);
    let mut kits: Vec<(u8, usize)> = (0..ahead.len()).filter_map(|i| kit_size(s, &ahead[i].0).map(|n| (n, i))).collect();
    kits.sort();
    for &(big, i) in &kits {
        for &(small, j) in &kits {
            if small < big && key(&ahead[j]) > key(&ahead[i]) {
                (ahead[i].1, ahead[i].2, ahead[i].3) = (ahead[j].1, ahead[j].2, ahead[j].3);
            }
        }
    }
}

/// The engine state rebuilt from the screen; names it doesn't know go to `unknown` and are left out.
/// Uses the game rules (`Rules::GAME`).
pub fn to_state(x: &ScreenState, unknown: &mut Vec<String>) -> GameState {
    to_state_with(x, unknown, Rules::GAME)
}

/// `to_state`, with the rule switches given (added by Ascendant; the parity tests use `Rules::ORIGINAL`). Instruments keep
/// the order of `x.instruments` (pick order, see `ScreenState::instruments`); unrecognized names are skipped and the rest keep their relative order.
pub fn to_state_with(x: &ScreenState, unknown: &mut Vec<String>, rules: Rules) -> GameState {
    let mut equipped = Equips::new();
    for (i, n) in x.instruments.iter().enumerate() {
        match data().instrument_by_name(n) {
            Some(id) if equipped.len() < MAX_EQUIPPED => {
                let r = x.instrument_rounds.as_ref().and_then(|r| r.get(i).copied()).filter(|&r| r != 0).unwrap_or(1);
                equipped.push(Equipped { id, acquired_round: r });
            }
            Some(_) => {}
            None => unknown.push(n.clone()),
        }
    }
    let setup = SandboxSetup { tanks: None, instruments: vec![], start_round: 1, thresholds_used: (false, false), forced_hand: vec![], seed: None };
    let s = create_game(data().default_settings, Some(&setup), Some(1));
    let potion = x.phase == ScreenPhase::Potion;
    let mut hand = Hand::new();
    for n in &x.hand {
        match lookup(potion, n) {
            Some(id) if hand.len() < MAX_HAND => hand.push(Card { kind: if potion { CardKind::Potion } else { CardKind::Instrument }, id }),
            Some(_) => {}
            None => unknown.push(n.clone()),
        }
    }
    let mut tanks: Tanks = [None; TANK_COUNT];
    for (t, m) in tanks.iter_mut().zip(x.tanks.iter()) {
        *t = m.as_ref().map(|m| Monster { archetype: m.archetype, rarity: m.rarity, activity: m.activity, quantity: m.quantity });
    }
    let had = equipped.len() + (x.phase == ScreenPhase::Instrument) as usize;
    GameState {
        tanks,
        instruments: equipped,
        round: x.round.unwrap_or(1),
        phase: if potion { Phase::PotionSelect } else { Phase::InstrumentSelect },
        hand,
        potion_refreshes: x.potion_refreshes.unwrap_or(0),
        instrument_refreshes: x.instrument_refreshes.unwrap_or(0),
        // past thresholds only matter for whether more instruments are coming
        t8000: had >= 2,
        t28000: had >= 3,
        // Rule switches (added by Ascendant): the solver goes through to_state, using Rules::GAME
        rules,
        ..s
    }
}

/// The futures a board's advice needs, so slices of them can be played on other cores.
#[derive(Clone, Debug)]
pub enum Work {
    Potion { s: GameState, seed: u32, la: LookAhead, opts: Vec<(Opt, f64)>, futures: u32 },
    Instrument { s: GameState, seed: u32, choices: Vec<Pick>, futures: u32 },
}

impl Work {
    pub fn futures(&self) -> u32 {
        match self {
            Work::Potion { futures, .. } | Work::Instrument { futures, .. } => *futures,
        }
    }
}

pub enum Prepared {
    Done(Advice),
    Work { work: Work, x: ScreenState, unknown: Vec<String>, t0: Instant },
}

/// The advice that needs no futures (a protocol, a Xenopanacea or Holy Water rite), or the futures to play for it.
pub fn prepare(x: &ScreenState, futures: Option<u32>) -> Prepared {
    prepare_with(x, futures, Rules::GAME)
}

/// `prepare`, with the rule switches given (added by Ascendant).
pub fn prepare_with(x: &ScreenState, futures: Option<u32>, rules: Rules) -> Prepared {
    let t0 = Instant::now();
    let mut unknown = Vec::new();
    // Surgical Protocols change the endgame battle, not Ultimate Activity
    if x.phase == ScreenPhase::Protocol {
        return Prepared::Done(Advice { best: Best::Nothing, ranking: vec![], board: 0, refresh_end: None, plan: false, final_: true, ms: 0, unknown, note: None });
    }
    // a ULT the simulator can't play out (a Xenopanacea, a Holy Water rite, a Dissection) is always taken: it's worth more than
    // anything beside it. A chain card whose chain can no longer be finished is not: it is compared like any card the
    // simulator doesn't know (left out of the hand, listed in `unknown`).
    if x.phase == ScreenPhase::Potion {
        if let Some(xeno) = always_take_now(x) {
            let pick = Ranked { index: xeno, targets: vec![], value: 0.0, end: None, no_effect: None };
            let board = ultimate_activity(&to_state_with(&ScreenState { hand: vec![], ..x.clone() }, &mut Vec::new(), rules));
            return Prepared::Done(Advice {
                best: Best::Card(pick.clone()),
                ranking: vec![pick],
                board,
                refresh_end: None,
                plan: false,
                final_: true,
                ms: t0.elapsed().as_millis() as u64,
                unknown,
                note: Some(if is_xenopanacea(&x.hand[xeno]) { "A Xenopanacea: always take it when it’s offered.".into() } else { format!("{}: always take it when it’s offered.", x.hand[xeno]) }),
            });
        }
    }
    let s = to_state_with(x, &mut unknown, rules);
    let work = if s.phase == Phase::PotionSelect {
        let n = futures.unwrap_or(LOOK_AHEAD.playouts);
        Work::Potion { s, seed: SEED, la: LookAhead { playouts: n, ..LOOK_AHEAD }, opts: shortlist(&s, SEED, &LOOK_AHEAD), futures: n }
    } else {
        let mut choices: Vec<Pick> = (0..s.hand.len()).map(Pick::Card).collect();
        if can_refresh(&s) {
            choices.push(Pick::Refresh);
        }
        Work::Instrument { s, seed: SEED, choices, futures: futures.unwrap_or(INSTRUMENT_FUTURES) }
    };
    Prepared::Work { work, x: x.clone(), unknown, t0 }
}

/// Futures `from` to `to` (excluded) of every option; `par` spreads them over every core.
pub fn play_work(w: &Work, from: u32, to: u32, par: bool) -> Vec<Tally> {
    match w {
        Work::Potion { s, seed, la, opts, .. } => {
            let o: Vec<Opt> = opts.iter().map(|c| c.0).collect();
            potion_futures(s, *seed, la, &o, from, to, par)
        }
        Work::Instrument { s, seed, choices, .. } => instrument_futures(s, *seed, choices, &Chooser::Ev(INSTRUMENT_ROLLOUT), Goal::Target, from, to, par),
    }
}

fn to_ranked(o: &Opt, value: f64) -> Ranked {
    match o {
        Opt::Card { index, targets } => Ranked { index: *index as usize, targets: targets.as_slice().to_vec(), value, end: None, no_effect: None },
        Opt::Refresh => Ranked { index: usize::MAX, targets: vec![], value, end: None, no_effect: None },
    }
}

/// Marks the moves that change nothing, and puts a move that does something first when it's about as good.
pub fn mark_no_effect(s: &GameState, ranking: Vec<Ranked>) -> Vec<Ranked> {
    let marked: Vec<Ranked> = ranking
        .into_iter()
        .map(|r| if card_changes(s, r.index, &r.targets) { r } else { Ranked { no_effect: Some(true), ..r } })
        .collect();
    let Some(top) = marked.first() else { return marked };
    if top.no_effect.is_none() {
        return marked;
    }
    let top_value = top.value;
    match marked.iter().position(|r| r.no_effect.is_none() && r.value >= top_value * (1.0 - NO_EFFECT_MARGIN)) {
        Some(i) => {
            let mut out = vec![marked[i].clone()];
            out.extend(marked.iter().enumerate().filter(|(k, _)| *k != i).map(|(_, r)| r.clone()));
            out
        }
        None => marked,
    }
}

/// Options tied with the best this round are put in order by playing a few more rounds.
fn break_ties(s: &GameState, ranking: Vec<Ranked>, seed: u32) -> Vec<Ranked> {
    let Some(first) = ranking.first() else { return ranking };
    let top = first.value;
    let n = ranking.iter().filter(|r| r.value == top).count();
    if n < 2 {
        return ranking;
    }
    let mut tied: Vec<(f64, Ranked)> = ranking[..n].iter().map(|r| (expected(s, &Opt::card(r.index, &r.targets), &TIE_BREAK, seed, 0), r.clone())).collect();
    tied.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<Ranked> = tied.into_iter().map(|(_, r)| r).collect();
    out.extend(ranking[n..].iter().cloned());
    out
}

/// The engine's hand leaves unknown cards out: each choice gets its place on screen.
fn on_screen(x: &ScreenState, mut a: Advice) -> Advice {
    let potion = x.phase == ScreenPhase::Potion;
    let at: Vec<usize> = x.hand.iter().enumerate().filter(|(_, n)| lookup(potion, n).is_some()).map(|(i, _)| i).collect();
    let place = |r: &mut Ranked| r.index = at.get(r.index).copied().unwrap_or(r.index);
    a.ranking.iter_mut().for_each(place);
    if let Best::Card(r) = &mut a.best {
        place(r);
    }
    a
}

/// The advice from the futures played (every option's totals).
pub fn conclude(work: &Work, x: &ScreenState, unknown: Vec<String>, t0: Instant, tallies: &[Tally]) -> Advice {
    let mean = |t: &Tally| t.goal / t.n as f64;
    let reached = |t: &Tally| t.reached as f64 / t.n as f64;
    let (s, ranking, best, refresh_end, plan) = match work {
        Work::Potion { s, opts, .. } => {
            // options dropped early by halving (fewer futures) rank below the ones kept
            let mut ahead: Vec<(Opt, f64, f64, u32)> = opts.iter().zip(tallies).map(|(c, t)| (c.0, mean(t), reached(t), t.n)).collect();
            kits_dominate(s, &mut ahead);
            ahead.sort_by(|a, b| {
                b.3.cmp(&a.3)
                    .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
                    .then(kit_size(s, &b.0).cmp(&kit_size(s, &a.0)))
            });
            let reroll = ahead.iter().position(|a| a.0 == Opt::Refresh);
            let refresh_end = reroll.map(|i| End { activity: ahead[i].1, reached: ahead[i].2 });
            let ranked: Vec<Ranked> = ahead
                .iter()
                .enumerate()
                .filter(|(i, _)| Some(*i) != reroll)
                .map(|(_, a)| Ranked { end: Some(End { activity: a.1, reached: a.2 }), ..to_ranked(&a.0, js_round(a.1)) })
                .collect();
            let ranking = mark_no_effect(s, ranked);
            let mut best = if reroll == Some(0) { Best::Refresh } else { ranking.first().cloned().map_or(Best::Nothing, Best::Card) };
            // a Potion Kit on offer is taken before a refresh
            if best == Best::Refresh {
                if let Some(k) = ranking.iter().find(|r| is_kit(s, r.index)) {
                    best = Best::Card(k.clone());
                }
            }
            (s, ranking, best, refresh_end, true)
        }
        Work::Instrument { s, choices, .. } => {
            let mut ranking: Vec<Ranked> = choices
                .iter()
                .zip(tallies)
                .filter_map(|(c, t)| match c {
                    Pick::Card(i) => Some(Ranked { index: *i, targets: vec![], value: js_round(mean(t)), end: Some(End { activity: mean(t), reached: reached(t) }), no_effect: None }),
                    Pick::Refresh => None,
                })
                .collect();
            ranking.sort_by(|a, b| b.value.partial_cmp(&a.value).unwrap_or(std::cmp::Ordering::Equal));
            let refresh_end = choices.iter().position(|c| *c == Pick::Refresh).map(|r| End { activity: mean(&tallies[r]), reached: reached(&tallies[r]) });
            let best = match (refresh_end, ranking.first()) {
                (Some(e), Some(top)) if e.activity > top.value => Best::Refresh,
                (_, Some(top)) => Best::Card(top.clone()),
                (Some(_), None) => Best::Refresh,
                (None, None) => Best::Nothing,
            };
            (s, ranking, best, refresh_end, false)
        }
    };
    on_screen(x, Advice { best, ranking, board: ultimate_activity(s), refresh_end, plan, final_: true, ms: t0.elapsed().as_millis() as u64, unknown, note: chain_notes(x) })
}

/// The Solver's advice, with every future played (spread over every core when `par`).
pub fn advise_plan(x: &ScreenState, futures: Option<u32>, par: bool) -> Advice {
    advise_halved(x, futures, false, par)
}

/// `advise_plan`, the potions' futures spread by successive halving when `halving` (`halved_futures`): the same
/// futures on average, most of them on the close calls between the best options.
pub fn advise_halved(x: &ScreenState, futures: Option<u32>, halving: bool, par: bool) -> Advice {
    advise_halved_with(x, futures, halving, par, Rules::GAME)
}

/// `advise_halved`, with the rule switches given (added by Ascendant).
pub fn advise_halved_with(x: &ScreenState, futures: Option<u32>, halving: bool, par: bool, rules: Rules) -> Advice {
    match prepare_with(x, futures, rules) {
        Prepared::Done(a) => a,
        Prepared::Work { work, x, unknown, t0 } => {
            let tallies = match &work {
                Work::Potion { s, seed, la, opts, .. } if halving => {
                    let o: Vec<Opt> = opts.iter().map(|c| c.0).collect();
                    halved_futures(s, *seed, la, &o, par)
                }
                _ => play_work(&work, 0, work.futures(), par),
            };
            conclude(&work, &x, unknown, t0, &tallies)
        }
    }
}

/// `prepare_with`, with the random seed given (added by Ascendant: when comparing solver settings offline, different seeds show whether the result is steady).
/// When `seed` is `SEED` it is identical to `prepare_with`; the potion shortlist (`shortlist`) is also recomputed with this seed.
pub fn prepare_seeded(x: &ScreenState, futures: Option<u32>, rules: Rules, seed: u32) -> Prepared {
    match prepare_with(x, futures, rules) {
        Prepared::Work { work, x, unknown, t0 } => {
            let work = match work {
                Work::Potion { s, la, futures, opts, .. } => {
                    let opts = if seed == SEED { opts } else { shortlist(&s, seed, &LOOK_AHEAD) };
                    Work::Potion { s, seed, la, opts, futures }
                }
                Work::Instrument { s, choices, futures, .. } => Work::Instrument { s, seed, choices, futures },
            };
            Prepared::Work { work, x, unknown, t0 }
        }
        done => done,
    }
}

/// `advise_halved`, with the random seed given (added by Ascendant). When `seed` is `SEED` it is identical to `advise_halved`.
pub fn advise_seeded(x: &ScreenState, futures: Option<u32>, halving: bool, par: bool, seed: u32) -> Advice {
    match prepare_seeded(x, futures, Rules::GAME, seed) {
        Prepared::Done(a) => a,
        Prepared::Work { work, x, unknown, t0 } => {
            let tallies = match &work {
                Work::Potion { s, seed, la, opts, .. } if halving => {
                    let o: Vec<Opt> = opts.iter().map(|c| c.0).collect();
                    halved_futures(s, *seed, la, &o, par)
                }
                _ => play_work(&work, 0, work.futures(), par),
            };
            conclude(&work, &x, unknown, t0, &tallies)
        }
    }
}

/// Default random seed (added by Ascendant, for callers of `advise_seeded`).
pub const DEFAULT_SEED: u32 = SEED;

// ---------------------------------------------------------------------------- racing (early stopping, added 2026-11-04)

/// Racing (early stopping) settings: each option first plays `first` futures, then is checked every `step` futures, up to `max`.
#[derive(Clone, Copy, Debug)]
pub struct Race {
    /// Threshold: the gap must exceed `z` standard errors
    pub z: f64,
    pub first: u32,
    pub step: u32,
    pub max: u32,
    /// true: paired-difference standard error (the same future index = common random numbers); false: the pooled standard error of the two options, sqrt(s1^2/n1 + s2^2/n2).
    pub paired: bool,
}

/// Racing result: the totals of each option (same format as `play_work`, can go straight to `conclude`), and how many futures were played in total.
pub struct Raced {
    pub tallies: Vec<Tally>,
    pub futures: u64,
}

/// The advice's "choice" itself (without the numbers), used to compare whether two judgments are the same: (0 = card, 1 = reroll, 2 = none, position on screen, target).
pub type ChoiceKey = (u8, usize, Vec<u8>);

pub fn choice_key(a: &Advice) -> ChoiceKey {
    match &a.best {
        Best::Card(r) => (0, r.index, r.targets.clone()),
        Best::Refresh => (1, 0, vec![]),
        Best::Nothing => (2, 0, vec![]),
    }
}

/// The `ChoiceKey` for each option (in `Work` order), with the position replaced by the position on screen (same as `on_screen`).
fn option_keys(work: &Work, x: &ScreenState) -> Vec<ChoiceKey> {
    let potion = x.phase == ScreenPhase::Potion;
    let at: Vec<usize> = x.hand.iter().enumerate().filter(|(_, n)| lookup(potion, n).is_some()).map(|(i, _)| i).collect();
    let place = |i: usize| at.get(i).copied().unwrap_or(i);
    match work {
        Work::Potion { opts, .. } => opts
            .iter()
            .map(|(o, _)| match o {
                Opt::Card { index, targets } => (0, place(*index as usize), targets.as_slice().to_vec()),
                Opt::Refresh => (1, 0, vec![]),
            })
            .collect(),
        Work::Instrument { choices, .. } => choices
            .iter()
            .map(|c| match c {
                Pick::Card(i) => (0, place(*i), vec![]),
                Pick::Refresh => (1, 0, vec![]),
            })
            .collect(),
    }
}

/// Plays futures from..to only for the `alive` options, giving each one's result (the same batch of futures as `play_work`).
fn play_values(work: &Work, alive: &[usize], from: u32, to: u32, par: bool) -> Vec<Vec<Tally>> {
    match work {
        Work::Potion { s, seed, la, opts, .. } => {
            let o: Vec<Opt> = alive.iter().map(|&i| opts[i].0).collect();
            potion_future_values(s, *seed, la, &o, from, to, par)
        }
        Work::Instrument { s, seed, choices, .. } => {
            let c: Vec<Pick> = alive.iter().map(|&i| choices[i]).collect();
            instrument_future_values(s, *seed, &c, &Chooser::Ev(INSTRUMENT_ROLLOUT), Goal::Target, from, to, par)
        }
    }
}

/// Racing: after each batch, use `conclude` (with all its rules: Refresh judgment, potion kit priority, cards that do not change the board yielding, etc.) to compute the current choice d;
/// for each option j still running, push d's mean down and j's mean up by half of z x standard error each (the gap shrinks by z standard errors in total), then run
/// `conclude` again: if the choice is still not j (and when pushed the other way the choice is still d), j is eliminated (no longer played). Stop when only d is left running (or `max` is reached).
/// When judging, eliminated options are converted to the same future count (`same_n`), so elimination itself does not change the choice.
pub fn play_raced(work: &Work, x: &ScreenState, race: &Race, par: bool) -> Raced {
    play_raced_until(work, x, race, par, &|| false).expect("never stopped")
}

/// `play_raced`, checking `stop` before each batch starts: if true, give up (return None). Ascendant's `solve_now` uses it,
/// so that a new read does not have to queue behind an old solve.
pub fn play_raced_until(work: &Work, x: &ScreenState, race: &Race, par: bool, stop: &dyn Fn() -> bool) -> Option<Raced> {
    let keys = option_keys(work, x);
    let n = keys.len();
    let mut tallies = vec![Tally::default(); n];
    let mut vals: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut alive: Vec<usize> = (0..n).collect();
    let t0 = Instant::now();
    let decide = |t: &[Tally]| choice_key(&conclude(work, x, Vec::new(), t0, &same_n(t)));
    let mut from = 0u32;
    while !alive.is_empty() && from < race.max {
        if stop() {
            return None;
        }
        // Advance at least 1 future (so first/step set to 0 do not spin in place).
        let to = if from == 0 { race.first } else { from + race.step }.min(race.max).max(from + 1);
        for (&i, v) in alive.iter().zip(play_values(work, &alive, from, to, par)) {
            for t in v {
                tallies[i].goal += t.goal;
                tallies[i].reached += t.reached;
                tallies[i].n += t.n;
                vals[i].push(t.goal);
            }
        }
        from = to;
        if from >= race.max || alive.len() <= 1 {
            break;
        }
        let Some(d) = keys.iter().position(|k| *k == decide(&tallies)) else { break };
        let mean = |i: usize| tallies[i].goal / tallies[i].n as f64;
        let var = |i: usize| {
            let m = mean(i);
            let v = &vals[i];
            v.iter().map(|g| (g - m) * (g - m)).sum::<f64>() / (v.len().max(2) - 1) as f64
        };
        let se = |a: usize, b: usize| -> f64 {
            if race.paired {
                paired_se(&vals[a], &vals[b])
            } else {
                (var(a) / vals[a].len() as f64 + var(b) / vals[b].len() as f64).sqrt()
            }
        };
        // When the two options' futures are identical so far (for example both capped at 840k), the standard error is 0: that must not count as "surely equal" and eliminate one;
        // instead use the standard error computed from the average variance of the options still running as a lower bound (it is 0 only when no option has any variance).
        let floor = {
            let v: f64 = alive.iter().map(|&i| var(i)).sum::<f64>() / alive.len() as f64;
            (2.0 * v / from as f64).sqrt()
        };
        let mut drop = Vec::new();
        for &j in &alive {
            if j == d {
                continue;
            }
            let e = se(d, j);
            let half = race.z * if e > 0.0 { e } else { floor } / 2.0;
            let shifted = |sign: f64| {
                let mut t = tallies.clone();
                t[d].goal = (mean(d) - sign * half) * t[d].n as f64;
                t[j].goal = (mean(j) + sign * half) * t[j].n as f64;
                decide(&t)
            };
            // j still is not chosen when pushed up, and the choice does not change when j is pushed down (the case where a reroll only "triggers" a potion kit:
            // the reroll itself is never chosen, but the choice would change if it dropped, so it must not be eliminated).
            if shifted(1.0) != keys[j] && shifted(-1.0) == keys[d] {
                drop.push(j);
            }
        }
        alive.retain(|i| !drop.contains(i));
        if alive.len() == 1 && alive[0] == d {
            break;
        }
    }
    let futures = tallies.iter().map(|t| t.n as u64).sum();
    Some(Raced { tallies, futures })
}

/// Converts eliminated options to the same future count as the option with the most (mean unchanged). The potion ordering in `conclude` compares future counts first,
/// so without the conversion, elimination itself would change the choice (e.g. when a reroll is best the kit is taken instead; once the reroll is eliminated it no longer ranks first, and the choice changes with it).
/// The option whose future count is already the largest is kept as is (bit-for-bit identical).
pub fn same_n(t: &[Tally]) -> Vec<Tally> {
    let most = t.iter().map(|t| t.n).max().unwrap_or(0);
    t.iter()
        .map(|t| if t.n == most || t.n == 0 { *t } else { Tally { goal: t.goal / t.n as f64 * most as f64, reached: (t.reached as f64 / t.n as f64 * most as f64).round() as u32, n: most } })
        .collect()
}

/// The racing version of `advise_seeded` (plain): the result and the total future count.
pub fn advise_raced(x: &ScreenState, race: &Race, par: bool, seed: u32) -> (Advice, u64) {
    match prepare_seeded(x, Some(race.max), Rules::GAME, seed) {
        Prepared::Done(a) => (a, 0),
        Prepared::Work { work, x, unknown, t0 } => {
            let r = play_raced(&work, &x, race, par);
            (conclude(&work, &x, unknown, t0, &same_n(&r.tallies)), r.futures)
        }
    }
}

/// Paired standard error of the mean difference between two options played on the same futures (common random
/// numbers): `a[k]` and `b[k]` must come from the same future k; only the first min(len) futures are used. 0 when
/// either is empty. The same math the racing uses (`Race::paired`).
pub fn paired_se(a: &[f64], b: &[f64]) -> f64 {
    let m = a.len().min(b.len());
    if m == 0 {
        return 0.0;
    }
    let diff = |k: usize| a[k] - b[k];
    let md = (0..m).map(diff).sum::<f64>() / m as f64;
    let vd = (0..m).map(|k| (diff(k) - md) * (diff(k) - md)).sum::<f64>() / (m.max(2) - 1) as f64;
    (vd / m as f64).sqrt()
}

/// Sample standard deviation of single futures' values (n - 1 in the denominator); 0 when empty.
pub fn sd(values: &[f64]) -> f64 {
    let n = values.len();
    if n == 0 {
        return 0.0;
    }
    let m = values.iter().sum::<f64>() / n as f64;
    (values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (n.max(2) - 1) as f64).sqrt()
}

// ---------------------------------------------------------------------------- every option scored (added 2026-11-05, for the game review)

/// Every option of one screen played on the same futures (`score_options`), all lists in the same order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scored {
    /// Each option as a `ChoiceKey` (position on screen, like `choice_key`).
    pub keys: Vec<ChoiceKey>,
    /// Mean goal: the final Ultimate Activity capped at 840k (`value::TARGET`).
    pub mean: Vec<f64>,
    /// Share of futures that reached 840k.
    pub reached: Vec<f64>,
    /// The goal of each single future, `values[option][k]`; future k is the same future for every option.
    pub values: Vec<Vec<f64>>,
}

impl Scored {
    /// The option with the highest mean (the first one on a tie).
    pub fn best(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for i in 0..self.mean.len() {
            match best {
                Some(b) if self.mean[i] <= self.mean[b] => {}
                _ => best = Some(i),
            }
        }
        best
    }

    /// The position of option `key`.
    pub fn find(&self, key: &ChoiceKey) -> Option<usize> {
        self.keys.iter().position(|k| k == key)
    }
}

/// The work for every option of a screen: every potion option `options` gives (each card on each target, not only the
/// live advice's `shortlist`) and a reroll while one is allowed, or every instrument and a reroll. None for a
/// protocol or a hand with a Xenopanacea, or a Holy Water rite or Dissection whose chain can still be finished: the
/// cases where `prepare` returns `Prepared::Done`.
fn full_work(x: &ScreenState, futures: u32) -> Option<Work> {
    if x.phase == ScreenPhase::Protocol || always_take_now(x).is_some() {
        return None;
    }
    let s = to_state(x, &mut Vec::new());
    Some(if s.phase == Phase::PotionSelect {
        let mut opts: Vec<(Opt, f64)> = options(&s).iter().map(|o| (*o, 0.0)).collect();
        if s.open_box.is_none() && can_refresh(&s) {
            opts.push((Opt::Refresh, 0.0));
        }
        Work::Potion { s, seed: SEED, la: LookAhead { playouts: futures, ..LOOK_AHEAD }, opts, futures }
    } else {
        let mut choices: Vec<Pick> = (0..s.hand.len()).map(Pick::Card).collect();
        if can_refresh(&s) {
            choices.push(Pick::Refresh);
        }
        Work::Instrument { s, seed: SEED, choices, futures }
    })
}

/// The options `score_options` would play, without playing them; None in the same cases.
pub fn all_option_keys(x: &ScreenState) -> Option<Vec<ChoiceKey>> {
    full_work(x, 0).map(|w| option_keys(&w, x)).filter(|k| !k.is_empty())
}

/// Plays every option of `x` on the same `futures` futures (common random numbers, so `paired_se` applies) and gives
/// each option's results. None for a protocol, for a hand the simulator cannot play out (`prepare` returns
/// `Prepared::Done`), or when there is nothing to play.
pub fn score_options(x: &ScreenState, futures: u32, par: bool) -> Option<Scored> {
    let work = full_work(x, futures)?;
    let keys = option_keys(&work, x);
    if keys.is_empty() || futures == 0 {
        return None;
    }
    let all: Vec<usize> = (0..keys.len()).collect();
    let played = play_values(&work, &all, 0, futures, par);
    let values: Vec<Vec<f64>> = played.iter().map(|v| v.iter().map(|t| t.goal).collect()).collect();
    let mean: Vec<f64> = played.iter().map(|v| v.iter().map(|t| t.goal).sum::<f64>() / v.len().max(1) as f64).collect();
    let reached: Vec<f64> = played.iter().map(|v| v.iter().map(|t| f64::from(t.reached)).sum::<f64>() / v.len().max(1) as f64).collect();
    Some(Scored { keys, mean, reached, values })
}

/// Rank the current choices. `plan`: potions judged by look-ahead to the end of the run (the Solver's advice);
/// otherwise by this round's expected score over `samples` outcomes. Instruments are always judged by the end.
pub fn advise(x: &ScreenState, samples: u32, plan: bool, par: bool) -> Advice {
    advise_with(x, samples, plan, par, Rules::GAME)
}

/// `advise`, with the rule switches given (added by Ascendant; the parity tests use `Rules::ORIGINAL`).
pub fn advise_with(x: &ScreenState, samples: u32, plan: bool, par: bool, rules: Rules) -> Advice {
    if x.phase != ScreenPhase::Potion || plan {
        return advise_halved_with(x, None, false, par, rules);
    }
    let t0 = Instant::now();
    if always_take_now(x).is_some() {
        if let Prepared::Done(a) = prepare_with(x, None, rules) {
            return a;
        }
    }
    let mut unknown = Vec::new();
    let s = to_state_with(x, &mut unknown, rules);
    let p = Policy::new(samples, 0);
    let mut ranked: Vec<Ranked> = options(&s).iter().map(|o| to_ranked(o, js_round(expected(&s, o, &p, SEED, 0)))).collect();
    ranked.sort_by(|a, b| b.value.partial_cmp(&a.value).unwrap_or(std::cmp::Ordering::Equal));
    let ranking = mark_no_effect(&s, break_ties(&s, ranked, SEED));
    let ua = ultimate_activity(&s);
    let mut best = match ranking.first() {
        Some(r) if !(r.value <= ua as f64 && s.potion_refreshes > 0) => Best::Card(r.clone()),
        _ => Best::Refresh,
    };
    if best == Best::Refresh {
        if let Some(k) = ranking.iter().find(|r| is_kit(&s, r.index)) {
            best = Best::Card(k.clone());
        }
    }
    on_screen(x, Advice { best, ranking, board: ua, refresh_end: None, plan: false, final_: false, ms: t0.elapsed().as_millis() as u64, unknown, note: chain_notes(x) })
}

/// Every card and instrument name the screen reader should know about.
pub fn names() -> (Vec<String>, Vec<String>) {
    let d = data();
    (d.potions.iter().map(|p| p.name.clone()).collect(), d.instruments.iter().map(|i| i.name.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A large potion kit (opens 5 cards) falls behind a small potion kit (opens 3 cards) because of simulation error: it is pulled up to equal the small one and ranked first;
    /// when it is already better nothing moves, and neither do the other options.
    #[test]
    fn a_large_kit_never_ranks_below_a_small_one() {
        let d = data();
        let mut s = create_game(d.default_settings, None, Some(1));
        let id = |name: &str| d.potion_by_name(name).unwrap();
        s.hand = Hand::new();
        for name in ["Fresh Spinal Powder", "Potion Kit (Small)", "Potion Kit (Large)"] {
            s.hand.push(Card { kind: CardKind::Potion, id: id(name) });
        }
        let small = (Opt::card(1, &[]), 806_390.0, 0.75, 4096);
        let large = (Opt::card(2, &[]), 789_038.0, 0.72, 512);
        let other = (Opt::card(0, &[]), 783_479.0, 0.63, 4096);
        let mut ahead = vec![small, large, other];
        kits_dominate(&s, &mut ahead);
        assert_eq!((ahead[1].1, ahead[1].2, ahead[1].3), (806_390.0, 0.75, 4096));
        assert_eq!((ahead[0].1, ahead[2].1), (806_390.0, 783_479.0));
        // Small, rare small (both open 3 cards) and large are all in hand: the large one is pulled up to the better of the two small ones.
        s.hand = Hand::new();
        for name in ["Potion Kit (Small)", "Rare Potion Kit (Small)", "Potion Kit (Large)"] {
            s.hand.push(Card { kind: CardKind::Potion, id: id(name) });
        }
        let mut ahead = vec![(Opt::card(0, &[]), 790_000.0, 0.70, 4096), (Opt::card(1, &[]), 800_000.0, 0.74, 4096), (Opt::card(2, &[]), 780_000.0, 0.69, 512)];
        kits_dominate(&s, &mut ahead);
        assert_eq!(ahead[2].1, 800_000.0);
        assert_eq!((ahead[0].1, ahead[1].1), (790_000.0, 800_000.0), "the 3-card kits are not compared with each other");
        // The large one is already better: no change.
        let mut ahead = vec![(Opt::card(1, &[]), 780_000.0, 0.7, 4096), (Opt::card(2, &[]), 790_000.0, 0.72, 4096)];
        kits_dominate(&s, &mut ahead);
        assert_eq!(ahead[1].1, 790_000.0);
        assert_eq!(ahead[0].1, 780_000.0);
    }

    /// A potion screen with six equal tanks, the hand given, in `round`.
    fn screen(hand: &[&str], round: Option<i32>) -> ScreenState {
        let tank = serde_json::json!({ "archetype": "pest", "rarity": "magic", "activity": 81, "quantity": 340 });
        serde_json::from_value(serde_json::json!({ "tanks": [tank, tank, tank, tank, tank, tank], "hand": hand, "instruments": [], "phase": "potion", "round": round })).unwrap()
    }

    /// `name` fits up to `last` and not in the round after it (when that is still a potion round).
    fn fits_up_to(name: &str, last: i32) {
        for r in 1..=last {
            assert!(fits_potion_rounds(name, Some(r)), "{name} in round {r}");
        }
        for r in last + 1..=LAST_POTION_ROUND {
            assert!(!fits_potion_rounds(name, Some(r)), "{name} in round {r}");
        }
    }

    #[test]
    fn holy_water_rites_fit_by_round() {
        assert_eq!(LAST_POTION_ROUND, 10);
        assert!(fits_potion_rounds("First Rite Holy Water: Confession", Some(7)));
        assert!(!fits_potion_rounds("First Rite Holy Water: Confession", Some(8)));
        assert!(fits_potion_rounds("Ritual Holy Water: Flagellation", Some(8)));
        assert!(!fits_potion_rounds("Ritual Holy Water: Flagellation", Some(9)));
        assert!(fits_potion_rounds("Third Rite Holy Water: Illumination", Some(9)));
        assert!(!fits_potion_rounds("Third Rite Holy Water: Illumination", Some(10)));
        fits_up_to("First Rite Holy Water: Confession", 7);
        fits_up_to("Ritual Holy Water: Flagellation", 8);
        fits_up_to("Third Rite Holy Water: Illumination", 9);
        fits_up_to("Fourth Rite Holy Water: Eucharist", 10);
    }

    #[test]
    fn dissections_fit_by_round() {
        fits_up_to("Dissection Infusion: Morbid Corpse", 6);
        fits_up_to("Dissection I: Vermectomy", 7);
        fits_up_to("Dissection II: Daemonectomy", 8);
        fits_up_to("Dissection III: Craniotomy", 9);
        fits_up_to("Dissection Infusion: Intact Skeleton", 10);
    }

    #[test]
    fn a_xenopanacea_ignores_the_round() {
        for name in ["Xenopanacea", "Xenopanacea: Osteotrophy", "Xenopanacea: Brood"] {
            assert_eq!(chain_steps_after(name), None, "{name}");
            for r in 1..=LAST_POTION_ROUND + 1 {
                assert!(fits_potion_rounds(name, Some(r)), "{name} in round {r}");
            }
            assert!(matches!(prepare(&screen(&["Mutagenic Powder", name], Some(10)), Some(2)), Prepared::Done(_)), "{name}");
        }
    }

    /// Without a round the advice is what it was before the round check: every always-take card is taken.
    #[test]
    fn an_unknown_round_keeps_the_old_behaviour() {
        for &(name, _) in CHAINS {
            assert!(fits_potion_rounds(name, None), "{name}");
        }
        let x = screen(&["Mutagenic Powder", "Bewitching Tincture", "First Rite Holy Water: Confession"], None);
        let Prepared::Done(a) = prepare(&x, Some(2)) else { panic!("not taken without a round") };
        assert!(matches!(&a.best, Best::Card(r) if r.index == 2), "{:?}", a.best);
        assert!(all_option_keys(&x).is_none());
    }

    /// A rite that can no longer be finished is compared like a card the simulator doesn't know, with a note saying why.
    #[test]
    fn a_late_rite_is_compared_and_noted() {
        let hand = ["Mutagenic Powder", "Bewitching Tincture", "First Rite Holy Water: Confession"];
        assert!(matches!(prepare(&screen(&hand, Some(7)), Some(2)), Prepared::Done(_)));
        assert!(all_option_keys(&screen(&hand, Some(7))).is_none());
        let late = screen(&hand, Some(8));
        assert!(matches!(prepare(&late, Some(2)), Prepared::Work { .. }));
        assert!(all_option_keys(&late).is_some_and(|k| k.iter().all(|k| k.1 != 2)), "the rite is not an option");
        let a = advise(&late, 2, false, false);
        assert!(!matches!(&a.best, Best::Card(r) if r.index == 2), "{:?}", a.best);
        assert_eq!(a.unknown, vec!["First Rite Holy Water: Confession".to_string()]);
        assert_eq!(a.note.as_deref(), Some("First Rite Holy Water: Confession: 2 potion rounds left, the chain needs 3, so it is not taken."));
        assert_eq!(
            chain_notes(&screen(&["Dissection II: Daemonectomy"], Some(9))).as_deref(),
            Some("Dissection II: Daemonectomy: 1 potion round left, the chain needs 2, so it is not taken.")
        );
        // the last rite still fits in the last potion round
        assert!(matches!(prepare(&screen(&["Mutagenic Powder", "Fourth Rite Holy Water: Eucharist"], Some(10)), Some(2)), Prepared::Done(_)));
        assert_eq!(chain_notes(&screen(&["Fourth Rite Holy Water: Eucharist"], Some(10))), None);
    }

    /// The offspring chain is never always taken; the advice compares the rest and notes the chain.
    #[test]
    fn offspring_are_noted_not_taken() {
        let steps = [
            ("Incubation Elixir: Hollow Cocoon", 4),
            ("F1 Gen: Parasite Egg", 3),
            ("F1 Gen: Vermin Egg Cluster", 3),
            ("F2 Gen: Parasite Pupae", 2),
            ("F2 Gen: Vermin Pupal Cluster", 2),
            ("F3 Gen: Parasitic Butterfly", 1),
            ("F3 Gen: Emergent Butterfly Swarm", 1),
            ("Incubation Elixir: Hollow Matriarch", 0),
        ];
        for (name, n) in steps {
            assert_eq!(chain_steps_after(name), Some(n), "{name}");
            assert!(!is_always_take(name), "{name}");
        }
        let x = screen(&["Mutagenic Powder", "F1 Gen: Parasite Egg", "Bewitching Tincture"], Some(6));
        assert!(always_take_now(&x).is_none());
        let a = advise(&x, 2, false, false);
        assert!(!matches!(&a.best, Best::Card(r) if r.index == 1), "{:?}", a.best);
        assert_eq!(a.note.as_deref(), Some("F1 Gen: Parasite Egg: can\u{2019}t be simulated; 4 potion rounds left, the chain needs 3."));
        // past the last round it can be finished in, and with an always-take rite that can't be finished beside it
        let x = screen(&["F2 Gen: Parasite Pupae", "First Rite Holy Water: Confession", "F2 Gen: Parasite Pupae"], Some(9));
        assert_eq!(
            chain_notes(&x).as_deref(),
            Some(
                "F2 Gen: Parasite Pupae: can\u{2019}t be simulated; 1 potion round left, the chain needs 2.\nFirst Rite Holy Water: Confession: 1 potion round left, the chain needs 3, so it is not taken."
            )
        );
        assert_eq!(chain_notes(&screen(&["Incubation Elixir: Hollow Matriarch"], None)).as_deref(), Some("Incubation Elixir: Hollow Matriarch: can\u{2019}t be simulated."));
        assert_eq!(chain_notes(&screen(&["Mutagenic Powder", "Xenopanacea: Brood"], Some(9))), None);
    }
}
