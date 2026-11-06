//! The game flow (src/engine/game.ts) and dealing (src/engine/draw.ts).

use crate::context::{create_monster, roll_archetype, roll_rarity, Ctx};
use crate::data::{data, Data, PotionRarity, StartTanks, Targeting, Timing};
use crate::effects;
use crate::rng::Rng;
use crate::stack::StackVec;
use crate::types::*;

pub const LAST_ROUND: i32 = 13;
pub const LAST_POTION_ROUND: i32 = 10;
pub const THRESHOLDS: [i64; 2] = [8000, 28000];
pub const MAX_INSTRUMENTS: usize = 3;

pub fn ultimate_activity(s: &GameState) -> i64 {
    s.tanks.iter().map(total).sum()
}

fn selected_potion(s: &GameState) -> Option<Targeting> {
    if s.phase != Phase::PotionSelect {
        return None;
    }
    let card = s.hand.get(s.selected? as usize)?;
    (card.kind == CardKind::Potion).then(|| data().potion(card.id).targeting)
}

/// The selected card's targeting, or None when it needs none (or the board is empty).
pub fn needs_targets(s: &GameState) -> Option<Targeting> {
    match selected_potion(s)? {
        t @ (Targeting::Select1 | Targeting::Select1To2) if s.tanks.iter().any(Option::is_some) => Some(t),
        _ => None,
    }
}

pub fn can_confirm(s: &GameState) -> bool {
    if s.phase == Phase::RoundEnd {
        return true;
    }
    let Some(sel) = s.selected else { return false };
    if s.phase == Phase::GameOver || s.hand.get(sel as usize).is_none() {
        return false;
    }
    if needs_targets(s).is_some() {
        !s.targets.is_empty()
    } else {
        true
    }
}

pub fn can_refresh(s: &GameState) -> bool {
    match s.phase {
        Phase::PotionSelect => s.potion_refreshes > 0,
        Phase::InstrumentSelect => s.instrument_refreshes > 0,
        _ => false,
    }
}

// ---------------------------------------------------------------------------- dealing

/// One item: uniformly while every weight is equal (the same dice as before weights existed), else by weight.
fn pick_by(rng: &mut Rng, pool: &[u8], w: &[f64; 256]) -> Option<u8> {
    let Some(&first) = pool.first() else { return None };
    let w0 = w[first as usize];
    if pool.iter().all(|&x| w[x as usize] == w0) {
        rng.pick(pool)
    } else {
        Some(rng.weighted(pool, |x| w[x as usize]))
    }
}

/// n distinct potions; each card independently has the Supreme chance to be Supreme.
pub fn deal_potions(d: &Data, rng: &mut Rng, n: usize, from_box: bool) -> Hand {
    let base: &[u8] = if from_box { &d.box_pool } else { &d.normal_pool };
    let weights = if from_box { &d.box_offer } else { &d.potion_offer };
    let mut used = [false; 256];
    let mut hand = Hand::new();
    let mut pool: StackVec<u8, 64> = StackVec::new();
    let fresh = |src: &[u8], used: &[bool; 256], pool: &mut StackVec<u8, 64>| {
        pool.clear();
        for &id in src {
            if !used[id as usize] {
                pool.push(id);
            }
        }
    };
    for _ in 0..n {
        let supreme = rng.chance(d.supreme_chance);
        fresh(if supreme { &d.supreme_pool } else { base }, &used, &mut pool);
        if pool.is_empty() {
            fresh(if supreme { base } else { &d.supreme_pool }, &used, &mut pool);
        }
        let p = if supreme && pool.get(0).is_some_and(|id| d.potion(id).rarity == PotionRarity::Supreme) {
            rng.pick(pool.as_slice())
        } else {
            pick_by(rng, pool.as_slice(), weights)
        };
        let Some(p) = p else { break };
        used[p as usize] = true;
        if hand.len() == MAX_HAND {
            break;
        }
        hand.push(Card { kind: CardKind::Potion, id: p });
    }
    hand
}

pub fn deal_instruments(d: &Data, rng: &mut Rng, basic: bool, equipped: &Equips) -> Hand {
    let src = if basic { &d.basic_pool } else { &d.advanced_pool };
    let mut available: StackVec<u8, 64> = StackVec::new();
    for &id in src {
        if !equipped.iter().any(|e| e.id == id) {
            available.push(id);
        }
    }
    let w = &d.instrument_offer;
    let mut out = Hand::new();
    let card = |id| Card { kind: CardKind::Instrument, id };
    let all_equal = match available.get(0) {
        None => true,
        Some(f) => available.iter().all(|&x| w[x as usize] == w[f as usize]),
    };
    if all_equal {
        let k = rng.sample_in_place(available.as_mut_slice(), 3);
        for &id in &available.as_slice()[..k] {
            out.push(card(id));
        }
        return out;
    }
    let mut left = available;
    while out.len() < 3 && !left.is_empty() {
        let id = rng.weighted(left.as_slice(), |x| w[x as usize]);
        let at = left.iter().position(|&x| x == id).unwrap();
        left.remove(at);
        out.push(card(id));
    }
    out
}

// ---------------------------------------------------------------------------- flow

/// Resolution order of the end-of-round instruments. Upstream: sorted by id. `rules.end_order_latest_first`: reverse pick order,
/// the last picked fires first (evidence in `Rules`). Pick order = `acquired_round` ascending, ties within a round follow the list order
/// (stable sort); once reversed, within the same round the later entries of the list also fire first.
pub fn end_of_round_order(equipped: &Equips, rules: Rules) -> Equips {
    let mut sorted = *equipped;
    if rules.end_order_latest_first {
        sorted.as_mut_slice().sort_by_key(|e| e.acquired_round);
        sorted.as_mut_slice().reverse();
    } else {
        sorted.as_mut_slice().sort_by_key(|e| e.id);
    }
    sorted
}

/// End-of-round and due periodic instruments, in id order (reverse pick order under `Rules::GAME`).
fn run_end_of_round(c: &mut Ctx, equipped: &Equips, round: i32, rules: Rules) {
    let sorted = end_of_round_order(equipped, rules);
    for e in &sorted {
        let Some(def) = c.d.instrument(e.id) else { continue };
        if !effects::has_end_of_round(e.id) {
            continue;
        }
        match def.timing {
            Timing::Periodic(every) => {
                let elapsed = round - e.acquired_round;
                if elapsed <= 0 || every == 0 || elapsed % every != 0 {
                    continue;
                }
            }
            Timing::End => {}
            Timing::Reactive(_) => continue,
        }
        effects::end_of_round(c, e.id);
    }
}

fn clear_selection(s: &mut GameState) {
    s.selected = None;
    s.targets.clear();
}

/// `deal` = false (added 2026-11-04): do not deal cards (the hand is cleared, no random numbers are drawn). Only for states that are
/// "scored by board value right after playing, then thrown away": board value does not read the hand or the RNG state, so the score is bit-for-bit identical.
fn start_instrument_pick(d: &Data, s: &mut GameState, rng: &mut Rng, deal: bool) {
    s.phase = Phase::InstrumentSelect;
    s.hand = if deal { deal_instruments(d, rng, s.instruments.is_empty(), &s.instruments) } else { Hand::new() };
    clear_selection(s);
}

/// Enter the current round's card phase; instrument-only rounds wait for the player to end them.
fn begin_round(d: &Data, s: &mut GameState, rng: &mut Rng, deal: bool) {
    s.open_box = None;
    clear_selection(s);
    if s.round > LAST_POTION_ROUND {
        s.phase = Phase::RoundEnd;
        s.hand.clear();
        return;
    }
    s.phase = Phase::PotionSelect;
    let forced = match &s.sandbox {
        Some(sb) if s.round == sb.start_round && !sb.forced_hand.is_empty() => Some(sb.forced_hand),
        _ => None,
    };
    s.hand = match forced {
        Some(ids) => {
            let mut h = Hand::new();
            for &id in &ids {
                h.push(Card { kind: CardKind::Potion, id });
            }
            h
        }
        None if deal => deal_potions(d, rng, 3, false),
        None => Hand::new(),
    };
}

fn end_round(d: &'static Data, s: &mut GameState, rng: &mut Rng, deal: bool) {
    let equipped = s.instruments;
    let round = s.round;
    let rules = s.rules;
    {
        let mut c = Ctx::with_rules(&mut s.tanks, rng, &equipped, d, rules);
        run_end_of_round(&mut c, &equipped, round, rules);
    }
    let ua = ultimate_activity(s);
    for k in 0..2 {
        let hit = if k == 0 { &mut s.t8000 } else { &mut s.t28000 };
        if *hit || ua < THRESHOLDS[k] {
            continue;
        }
        *hit = true;
        if s.instruments.len() + (s.pending_picks.max(0) as usize) < MAX_INSTRUMENTS {
            s.pending_picks += 1;
        }
    }
    if s.round >= LAST_ROUND {
        s.phase = Phase::GameOver;
        s.hand.clear();
        s.open_box = None;
        clear_selection(s);
        return;
    }
    s.round += 1;
    if s.pending_picks > 0 {
        return start_instrument_pick(d, s, rng, deal);
    }
    begin_round(d, s, rng, deal);
}

fn confirm(d: &'static Data, s: &mut GameState, rng: &mut Rng, deal: bool) {
    if s.phase == Phase::RoundEnd {
        return end_round(d, s, rng, deal);
    }
    let card = s.hand[s.selected.unwrap() as usize];
    if card.kind == CardKind::Instrument {
        s.instruments.push(Equipped { id: card.id, acquired_round: s.round });
        if s.pending_picks > 0 {
            s.pending_picks -= 1;
        }
        if s.pending_picks > 0 && s.instruments.len() < MAX_INSTRUMENTS {
            return start_instrument_pick(d, s, rng, deal);
        }
        s.pending_picks = 0;
        return begin_round(d, s, rng, deal);
    }
    let def = d.potion(card.id);
    if let Targeting::Box(n) = def.targeting {
        s.open_box = Some(def.id);
        s.hand = deal_potions(d, rng, n as usize, true);
        clear_selection(s);
        return;
    }
    let targets = if needs_targets(s).is_some() { s.targets } else { Targets::new() };
    let equipped = s.instruments;
    {
        let rules = s.rules;
        let mut c = Ctx::with_rules(&mut s.tanks, rng, &equipped, d, rules);
        effects::apply_potion(&mut c, card.id, targets.as_slice());
    }
    s.open_box = None;
    clear_selection(s);
    end_round(d, s, rng, deal);
}

fn refresh(d: &Data, s: &mut GameState, rng: &mut Rng) {
    if s.phase == Phase::PotionSelect {
        s.potion_refreshes -= 1;
        let kit = s.open_box.map(|id| d.potion(id).targeting);
        s.hand = match kit {
            Some(Targeting::Box(n)) => deal_potions(d, rng, n as usize, true),
            _ => deal_potions(d, rng, 3, false),
        };
    } else {
        s.instrument_refreshes -= 1;
        s.hand = deal_instruments(d, rng, s.instruments.is_empty(), &s.instruments);
    }
    clear_selection(s);
}

/// Whether playing the card at `index` on `targets` changes the board by itself, before the round ends.
pub fn card_changes(state: &GameState, index: usize, targets: &[u8]) -> bool {
    let d = data();
    let Some(card) = state.hand.get(index) else { return true };
    if card.kind != CardKind::Potion {
        return true;
    }
    if matches!(d.potion(card.id).targeting, Targeting::Box(_)) {
        return true;
    }
    if effects::potion_port(card.id).is_none() {
        return false;
    }
    let mut s = *state;
    let mut rng = Rng::new(s.rng_state);
    let before = s.tanks;
    let equipped = s.instruments;
    {
        let rules = s.rules;
        let mut c = Ctx::with_rules(&mut s.tanks, &mut rng, &equipped, d, rules);
        effects::apply_potion(&mut c, card.id, targets);
    }
    s.tanks != before
}

/// One action on a copy of the state; an action that isn't allowed returns the state unchanged.
pub fn dispatch(state: &GameState, action: Action) -> GameState {
    dispatch_with(state, action, true)
}

/// `dispatch`; when `deal` = false the next hand is not dealt at round end (see `start_instrument_pick`; added 2026-11-04).
#[inline]
pub fn dispatch_with(state: &GameState, action: Action, deal: bool) -> GameState {
    let mut s = *state;
    apply(&mut s, action, deal);
    s
}

/// Added 2026-11-04: `dispatch_with`, which modifies the given state directly (no extra copy). Disallowed actions return before any field is changed,
/// so it leaves the state untouched, just like `dispatch`.
#[inline]
pub fn apply(s: &mut GameState, action: Action, deal: bool) {
    let d = data();
    let mut rng = Rng::new(s.rng_state);
    match action {
        Action::SelectCard { index } => {
            if s.phase == Phase::GameOver || index < 0 || s.hand.get(index as usize).is_none() {
                return;
            }
            s.selected = Some(index as u8);
            s.targets.clear();
        }
        Action::ToggleTarget { tank } => {
            let Some(mode) = needs_targets(s) else { return };
            if tank < 0 || tank as usize >= TANK_COUNT || s.tanks[tank as usize].is_none() {
                return;
            }
            let t = tank as u8;
            if mode == Targeting::Select1 {
                s.targets = Targets::from_slice(&[t]);
            } else if let Some(at) = s.targets.iter().position(|&x| x == t) {
                s.targets.remove(at);
            } else if s.targets.len() < 2 {
                s.targets.push(t);
            }
        }
        Action::Confirm => {
            if !can_confirm(s) {
                return;
            }
            confirm(d, s, &mut rng, deal);
        }
        Action::Refresh => {
            if !can_refresh(s) {
                return;
            }
            refresh(d, s, &mut rng);
        }
    }
    s.rng_state = rng.state();
}

pub fn play(s: &GameState, actions: &[Action]) -> GameState {
    actions.iter().fold(*s, |x, &a| dispatch(&x, a))
}

fn random_start(d: &Data, rng: &mut Rng) -> Tanks {
    let mut tanks: Tanks = [None; TANK_COUNT];
    let n = match d.start_tanks {
        StartTanks::Three => 3,
        StartTanks::Recorded => 3 + rng.int(2),
    };
    for t in tanks.iter_mut().take(n) {
        let a = roll_archetype(rng);
        let r = roll_rarity(d, rng);
        *t = Some(create_monster(a, r, 0, 0));
    }
    tanks
}

pub fn create_game(settings: Settings, setup: Option<&SandboxSetup>, seed: Option<u32>) -> GameState {
    let d = data();
    let seed = seed.or(setup.and_then(|s| s.seed)).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.as_millis() as u32).unwrap_or(1)
    });
    let mut rng = Rng::new(seed);
    let tanks = match setup.and_then(|s| s.tanks) {
        Some(t) => t,
        None => random_start(d, &mut rng),
    };
    let mut s = GameState {
        tanks,
        instruments: setup.map(|s| Equips::from_slice(&s.instruments)).unwrap_or_default(),
        round: setup.map_or(1, |s| s.start_round),
        phase: Phase::InstrumentSelect,
        potion_refreshes: settings.potion_refreshes,
        instrument_refreshes: settings.instrument_refreshes,
        t8000: setup.is_some_and(|s| s.thresholds_used.0),
        t28000: setup.is_some_and(|s| s.thresholds_used.1),
        sandbox: setup.map(|s| Sandbox { start_round: s.start_round, forced_hand: StackVec::from_slice(&s.forced_hand) }),
        rules: settings.rules,
        ..Default::default()
    };
    // With no instruments at the start, deal 3 basic cards for the player to pick from (upstream behavior). The 2026-11-03 Traditional Chinese recorded game dealt only 1,
    // but the user said that game was a tutorial; the ranked game screenshot shows a pick of 3 with 2 rerolls, matching this, so no change.
    if s.instruments.is_empty() {
        start_instrument_pick(d, &mut s, &mut rng, true);
    } else {
        begin_round(d, &mut s, &mut rng, true);
    }
    s.rng_state = rng.state();
    s
}
