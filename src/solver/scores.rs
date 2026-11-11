//! Valuing moves (js/solver.js 336-497): incident markers, patrol
//! destinations, prey marks, devours, swaps and the gift picks.

use super::chain::{best_meal, coin_of_fate, devour_delta, replace_odds};
use super::values::{activation, execution_value, fresh_value, stair_delta, swap_value};
use super::{Comp, Ctx, Dist, Params, Rng32, TOP, beasts, composition, dist_worth, door_dist, landings_left, seen_door, tier_values, worth};
use crate::engine::data::{Beast, Card, ObjectiveKind, RAINBOW, TYRANNY, Tool, data};
use crate::engine::devour::{Bite, devour_options, eater_mask, eating, sure_gain};
use crate::engine::incidents::{CHAIN_REACTION, EXECUTION_EFFICIENCY, SEWER_RATWAY};
use crate::engine::rules::{clockwise_path, fitting_incidents, land, marker_seen, steps_between};
use crate::engine::state::{Cell, Inmate, Pending, State};
use crate::engine::text::{an, describe, incident_name, incident_term, plural};
use crate::lang::{tr, trf};
use crate::engine::{Action, ChoiceId, apply};

/// Sorts best first, keeping ties in order (the JS stable sort on b - a).
fn best_first(out: &mut [(usize, f64)]) {
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
}

/// incidentScore: the value of the incident marker on a destination. Of a
/// seen marker only the two incidents the engine simulates have a value; an
/// unseen one also gets `w_incident` for the odds of any other.
pub fn incident_score(ctx: &Ctx, c: &Cell, comp: &Comp) -> f64 {
    // Behind a door the executioner is whoever comes out; a Chain Reaction
    // landing opens it from the side.
    let execution = || {
        if !c.sealed {
            return c.inmate.map_or(0.0, |i| execution_value(ctx, i.tier as usize, i.beast.index(), comp));
        }
        let (dist, beast) = door_dist(ctx.st, Some(c), !ctx.st.chain);
        let (beasts, share) = beasts(beast);
        let mut exec = 0.0;
        for (t, &p) in dist.iter().enumerate() {
            for b in beasts.clone() {
                exec += (p * share) * execution_value(ctx, t, b, comp);
            }
        }
        exec
    };
    match c.event {
        // A marker whose incident the player can't see is any that fits
        // its cell, at the draw odds.
        Some(_) if !marker_seen(ctx.st) => {
            let (mut exec, mut chain, mut total) = (0.0, 0.0, 0.0);
            for (id, w) in fitting_incidents(ctx.st, c.n) {
                let w = f64::from(w);
                total += w;
                match id {
                    EXECUTION_EFFICIENCY => exec += w,
                    CHAIN_REACTION => chain += w,
                    _ => {}
                }
            }
            if total == 0.0 {
                return 0.0;
            }
            let (exec, chain) = (exec / total, chain / total);
            let mut score = chain * ctx.p.w_chain + (1.0 - exec - chain) * ctx.p.w_incident;
            if exec > 0.0 {
                score += exec * execution();
            }
            score
        }
        Some(EXECUTION_EFFICIENCY) => execution(),
        Some(CHAIN_REACTION) => ctx.p.w_chain,
        _ => 0.0,
    }
}

/// Incidents whose effect rolls no dice, played once.
const SURE_INCIDENTS: [u32; 4] = [213, 231, 232, SEWER_RATWAY];

/// The seed of sample `k` of a copy landed on cell `n`: fixed, so a copy
/// never reads the game's own dice.
fn sample_seed(st: &State, n: usize, k: u32) -> u32 {
    st.day.wrapping_mul(0x9e37_79b9) ^ (n as u32).wrapping_mul(0x85eb_ca6b) ^ (k + 1).wrapping_mul(0xc2b2_ae35)
}

/// A copy of `st` with the warden landed on cell `n`, its marker's incident
/// pending. A Sewer Ratway choice in progress is this landing.
fn landed(st: &State, n: usize, seed: u32) -> State {
    let mut trial = st.clone();
    trial.rng = seed;
    trial.pending.clear();
    land(&mut trial, n);
    // Landing that completes the "trigger an event" objective queues an instrument pick; it does not affect this
    // event's value, so take it off first, otherwise the event action is rejected and the value computed as 0.
    trial.pending.retain(|p| !matches!(p, Pending::Tool { .. }));
    trial
}

/// The mean board gain, by tiers `(v, r)`, of `action` on a copy landed on
/// cell `n` over `samples` samples: a marker's incident (`Action::Event`) or
/// the Interrogation Room's Bestow. Instigate's marker adds no board value.
fn landing_gain(st: &State, n: usize, action: &Action, samples: u32, &(v, r): &([f64; 6], f64)) -> f64 {
    let mut gain = 0.0;
    for k in 0..samples {
        let seed = sample_seed(st, n, k);
        let mut trial = landed(st, n, seed);
        // Tyranny's extra Room cards run on landing, so they count as its gain.
        let before = if n == data().room && st.theme == Some(TYRANNY) {
            let mut plain = st.clone();
            plain.theme = None;
            board_value(&landed(&plain, n, seed), &v, r)
        } else {
            board_value(&trial, &v, r)
        };
        // A refused action (a Treasure gift queued first) still keeps what landing did.
        apply(&mut trial, action);
        gain += board_value(&trial, &v, r) - before;
    }
    gain / f64::from(samples)
}

/// The `why` of incidentScore: what waits on the marker, if there is one.
pub fn incident_why(st: &State, c: &Cell) -> Option<String> {
    let id = c.event?;
    Some(match id {
        _ if !marker_seen(st) => trf!("{} waits there", an(incident_term())),
        _ => trf!("{} waits there", an(incident_name(id))),
    })
}

/// The `why` patrolScores gives landing on cell `n`.
pub fn patrol_why(st: &State, n: usize) -> String {
    let c = &st.cells[n];
    if let Some(why) = incident_why(st, c) {
        return why;
    }
    if st.chain {
        let sealed = clockwise_path(st, n).iter().filter(|&&k| st.cells[k].sealed).count();
        trf!("the chain opens {} on the way", plural(sealed, "door"))
    } else if let (true, Some(nest)) = (c.sealed, c.nest) {
        trf!("it destroys the Rat Nest due on day {}", nest.due)
    } else if c.sealed {
        tr("a new Inmate, who may consume today").to_string()
    } else if let Some(i) = c.inmate {
        trf!("its {} can act today", describe(&i))
    } else if n == data().room {
        tr("Instigate or Bestow waits there").to_string()
    } else {
        tr("an empty cell").to_string()
    }
}

/// patrolScores: scores landing on each of `cells`, as (cell, score), best first.
pub fn patrol_scores(ctx: &mut Ctx, cells: &[usize]) -> Vec<(usize, f64)> {
    let st = ctx.st;
    let p = ctx.p;
    let (mut board, crowd) = composition(st, None);
    let swap_left = st.replace_left();
    // The door in cell c, by whether its opening sets off the "after opening"
    // instruments; an unseen door is valued once per kind of opening.
    let mut fresh: [Option<f64>; 2] = [None; 2];
    // The scratch bite each cell's opening gives up.
    let loss: Vec<f64> = st.cells.iter().map(|c| scratch_loss(ctx, c)).collect();
    let mut door_now = |ctx: &mut Ctx, board: &mut Comp, c: &Cell, after: bool| {
        let seen = seen_door(c);
        if let (false, Some(value)) = (seen, fresh[usize::from(after)]) {
            return value;
        }
        let (dist, beast) = door_dist(st, Some(c), after);
        let value = fresh_value(ctx, &dist, beast, board, crowd + 1, swap_left) - p.w_open * loss[c.n];
        if !seen {
            fresh[usize::from(after)] = Some(value);
        }
        value
    };
    let known = |ctx: &mut Ctx, cell: &Cell| {
        let (mut comp, crowd) = composition(st, Some(cell.n));
        let i = cell.inmate.expect("inmate");
        activation(ctx, i.tier as usize, i.beast.index(), &mut comp, crowd, swap_left)
    };
    let held = HELD_TERMS.iter().any(|&t| st.has(t)).then(|| Held::of(ctx, &loss));
    let tiers = gift_tiers(ctx.st, ctx.p, (ctx.v, ctx.r, ctx.last_day));
    let mut out = Vec::with_capacity(cells.len());
    for &n in cells {
        let c = &st.cells[n];
        let steps = steps_between(st.position, n);
        let mut score = p.w_step * steps as f64;
        // Objective progress: the steps walked, the doors opened, an incident set off.
        if let Some(goal) = &ctx.goal {
            let gain = match goal.kind {
                ObjectiveKind::MoveDistance => steps,
                ObjectiveKind::OpenCellCount if st.chain => clockwise_path(st, n).iter().filter(|&&k| st.cells[k].sealed).count(),
                ObjectiveKind::OpenCellCount => usize::from(c.sealed && n != data().room),
                ObjectiveKind::EventTriggerCount => usize::from(c.event.is_some() && n != data().room),
                _ => 0,
            };
            score += goal.term(p, gain as f64);
        }
        // Landing on a marker fires its incident, which can't be declined.
        score += incident_score(ctx, c, &board);
        // Any other known marker, and the Interrogation Room, are played on a copy.
        if let Some(id) = c.event.filter(|&id| marker_seen(st) && id != EXECUTION_EFFICIENCY && id != CHAIN_REACTION) {
            let samples = if SURE_INCIDENTS.contains(&id) { 1 } else { 3 };
            score += p.w_event * landing_gain(st, n, &Action::Event, samples, &tiers);
        } else if n == data().room {
            let bestow = Action::Choose { id: ChoiceId::Name(Card::Bestow.key().to_string()) };
            let t = if st.theme == Some(TYRANNY) { p.room_tyranny } else { 1.0 };
            score += t * p.w_event * landing_gain(st, n, &bestow, 3, &tiers);
        }
        // The sealed doors left after this landing's own openings.
        let (mut left, mut left_worth) = held.as_ref().map_or((0, 0.0), |h| (h.sealed, h.side));
        if held.is_some() && c.sealed && !st.chain {
            left -= 1;
            left_worth -= side_worth(ctx, c, &loss);
        }
        if st.chain {
            // Chain Reaction opens every door on the walk from the side.
            let mut opened = 0.0;
            for &k in clockwise_path(st, n).iter().filter(|&&k| st.cells[k].sealed) {
                let d = &st.cells[k];
                if held.is_some() {
                    left -= 1;
                    left_worth -= side_worth(ctx, d, &loss);
                }
                if k == n {
                    continue;
                }
                opened += side_worth(ctx, d, &loss);
            }
            let here = if c.sealed {
                door_now(ctx, &mut board, c, false)
            } else if c.inmate.is_some() {
                p.w_walk * known(ctx, c)
            } else {
                0.0
            };
            score += p.w_open * opened + here;
        } else if c.sealed {
            score += door_now(ctx, &mut board, c, true);
            // Squirming Iron Box: the patrol opens the door's own inmate and
            // destroys the nest, whose Rare Rat hatches if it is due in the run.
            if c.nest.is_some_and(|nest| nest.due <= st.max_days) {
                score -= ctx.v[st.params.nest_tier as usize];
            }
        } else if c.inmate.is_some() {
            let walk = p.w_walk * known(ctx, c);
            score += walk;
            // Blood-Stained Trap: a Lion here could mark prey today, if that is worth more than the landing is worth
            // as a meal-taker (`Params::w_hunt_walk`).
            if p.w_hunt_walk != 0.0
                && st.has(Tool::Trap)
                && st.used.hunt == 0
                && st.day < st.max_days
                && let Some(lion) = c.inmate.filter(|i| i.beast == Beast::Lion && st.link_of(i.id).is_none())
            {
                score += p.w_hunt_walk * (hunt_walk_gain(ctx, n, &lion) - walk).max(0.0);
            }
        }
        // Iron Tongs of Lust: a Replace left for the Tusk cell's inmate.
        if swap_left && c.ivory && c.on_board() {
            score += p.w_walk * tusk_gain(ctx);
        }
        if let Some(h) = &held {
            // A dice patrol onto a door sets off the "after opening" instruments.
            let opened = (c.sealed && !st.chain)
                .then(|| door_dist(st, Some(c), true).0);
            score += held_terms(ctx, h, n, steps, opened.as_ref(), left, left_worth);
        }
        // What tomorrow's destinations past this landing hold (`Params::w_ahead`).
        if p.w_ahead != 0.0 && !st.chain && st.day < st.max_days {
            score += p.w_ahead * ahead_value(ctx, n, &board);
        }
        // Executioner's Blacklist: the potential after the doors this landing opens (`Black`).
        if let Some(b) = &ctx.black {
            let opens = if st.chain { clockwise_path(st, n).iter().filter(|&&k| st.cells[k].sealed).count() } else { usize::from(c.sealed) };
            if opens > 0 {
                let (sealed, left) = (st.sealed_count(), landings_left(st).saturating_sub(1));
                score += b.potential(sealed.saturating_sub(opens), left) - b.potential(sealed, left);
            }
        }
        out.push((n, score));
    }
    best_first(&mut out);
    out
}

/// The instruments `held_terms` prices on a patrol landing.
const HELD_TERMS: [Tool; 6] = [Tool::Keys, Tool::Forgedkey, Tool::Fire, Tool::Blacklist, Tool::Shortchain, Tool::Longchain];

/// The board before a patrol, as the held instruments see it: the sealed
/// doors and their worth opened from the side, the open inmates' tiers, and
/// the scratch bite each cell's opening gives up.
struct Held {
    sealed: usize,
    side: f64,
    board: Vec<usize>,
    loss: Vec<f64>,
}

impl Held {
    fn of(ctx: &Ctx, loss: &[f64]) -> Held {
        let st = ctx.st;
        let doors = st.cells.iter().filter(|c| c.sealed);
        Held {
            sealed: doors.clone().count(),
            side: doors.map(|c| side_worth(ctx, c, loss)).sum(),
            board: st.cells.iter().filter(|c| c.on_board()).filter_map(|c| c.inmate).map(|i| i.tier as usize).collect(),
            loss: loss.to_vec(),
        }
    }
}

/// The worth of sealed cell `c` opened from the side (no "after opening"
/// instrument), less the scratch bite its opening gives up (`loss` by cell).
fn side_worth(ctx: &Ctx, c: &Cell, loss: &[f64]) -> f64 {
    let worth = if seen_door(c) { dist_worth(&ctx.v, ctx.r, &door_dist(ctx.st, Some(c), false).0) } else { ctx.side_value };
    worth - loss[c.n]
}

/// The scratch bite given up by opening sealed cell `c`. With an open Wolf of
/// the door's tier: half the most it gains biting the door over its best
/// other meal (it may not get its turn first). Without one: the share of the
/// open inmates that are lower Wolves, times 3, times the scratch rise.
fn scratch_loss(ctx: &mut Ctx, c: &Cell) -> f64 {
    let Some(h) = c.hidden.filter(|h| c.sealed && c.scratch && h.tier < RAINBOW) else { return 0.0 };
    let st = ctx.st;
    let (t, wolf) = (h.tier as usize, Beast::Wolf.index());
    let open = || st.cells.iter().filter(|w| w.on_board()).filter_map(|w| w.inmate.map(|i| (w.n, i)));
    let wolves: Vec<usize> = open().filter(|(_, i)| i.beast == Beast::Wolf && i.tier == h.tier).map(|(n, _)| n).collect();
    if wolves.is_empty() {
        let lower = open().filter(|(_, i)| i.beast == Beast::Wolf && i.tier < h.tier).count();
        let share = lower as f64 / open().count().max(1) as f64;
        return 3.0 * share * rise_worth(ctx, t, 1 + ctx.scratch_extra as i32);
    }
    let mut loss = 0.0f64;
    for n in wolves {
        let (mut comp, crowd) = composition(st, Some(n));
        let bite = devour_delta(ctx, t, wolf, (t, wolf), true, &mut comp, crowd);
        let other = best_meal(ctx, t, wolf, &mut comp, crowd).0.max(0.0);
        loss = loss.max(0.5 * (bite - other));
    }
    loss
}

/// What `by` tiers more are worth to an inmate of tier `t`; a Chromatic is
/// worth `r` and nothing rises past the top tier.
fn rise_worth(ctx: &Ctx, t: usize, by: i32) -> f64 {
    worth(ctx, (t as i32 + by).clamp(0, ctx.st.top_tier()) as usize) - worth(ctx, t)
}

/// What the held opening and movement instruments add to a dice landing on
/// cell `n`, `steps` away (engine after_opening and shackles). `opened` is the
/// tier odds of the inmate a patrol opening brings out, None when the landing
/// opens no door on the patrol; `left` doors stay sealed after the landing's
/// own openings, worth `left_worth` in all from the side.
fn held_terms(ctx: &Ctx, h: &Held, n: usize, steps: usize, opened: Option<&Dist>, left: usize, left_worth: f64) -> f64 {
    let st = ctx.st;
    let d = data();
    let w_open = ctx.p.w_open;
    let chance = |t: Tool| if st.has(t) { d.tool(t).chance / 100.0 } else { 0.0 };
    // A random sealed door left, opened from the side.
    let random_door = if left > 0 { left_worth / left as f64 } else { 0.0 };
    // The open inmates after the landing, the opened one included, and the
    // sum of what `by` tiers more are worth to them.
    let board = h.board.len() + usize::from(opened.is_some());
    let rises = |by: i32| {
        let mut sum: f64 = h.board.iter().map(|&t| rise_worth(ctx, t, by)).sum();
        if let Some(dist) = opened {
            sum += dist.iter().enumerate().map(|(t, p)| p * rise_worth(ctx, t, by)).sum::<f64>();
        }
        sum
    };
    let mut gain = 0.0;
    if let Some(dist) = opened {
        // Executioner's Blacklist: random inmates rise once fewer than
        // `max_sealed` doors stay locked. It fires last and counts the doors left after the doors are opened (engine
        // after_opening), so it sums the cases where fewer doors than the threshold remain, by the probability that
        // each of these three instruments (Searing Branding Iron, Keys, Forged Key) fires or not.
        let black = d.tool(Tool::Blacklist);
        if st.has(Tool::Blacklist) {
            let cells = d.cell_count;
            let key_doors = [n + cells - 1, n + 1].into_iter().filter(|&k| st.cells[k % cells].sealed && k % cells != d.room).count();
            let forged = d.tool(Tool::Forgedkey).count.unwrap_or(0);
            let mut fires = 0.0;
            for mask in 0..8u8 {
                let on = |bit: u8| mask & bit != 0;
                let p = [(1, Tool::Fire), (2, Tool::Keys), (4, Tool::Forgedkey)]
                    .iter()
                    .map(|&(bit, t)| if on(bit) { chance(t) } else { 1.0 - chance(t) })
                    .product::<f64>();
                if p == 0.0 {
                    continue;
                }
                let mut doors = left;
                if on(1) {
                    doors = doors.saturating_sub(1);
                }
                if on(2) {
                    doors = doors.saturating_sub(key_doors);
                }
                if on(4) {
                    doors = doors.saturating_sub(forged);
                }
                if doors < black.max_sealed.unwrap_or(0) {
                    fires += p;
                }
            }
            let picks = black.count.unwrap_or(0).min(board);
            gain += fires * picks as f64 / board as f64 * rises(black.tiers.unwrap_or(0));
        }
        // Searing Branding Iron: a copy of the opened inmate opens another door.
        if left > 0 {
            gain += chance(Tool::Fire) * w_open * dist_worth(&ctx.v, ctx.r, dist);
        }
        // Lost Ring of Keys: the doors on both sides open from the side.
        if st.has(Tool::Keys) {
            let cells = d.cell_count;
            let sides: f64 = [n + cells - 1, n + 1]
                .into_iter()
                .map(|k| &st.cells[k % cells])
                .filter(|c| c.sealed && c.n != d.room)
                .map(|c| side_worth(ctx, c, &h.loss))
                .sum();
            gain += chance(Tool::Keys) * w_open * sides;
        }
        // Forged Key: more random doors open from the side.
        let forged = d.tool(Tool::Forgedkey).count.unwrap_or(0).min(left);
        gain += chance(Tool::Forgedkey) * w_open * forged as f64 * random_door;
    }
    // Short-Chained Shackles: a short move opens random doors.
    let short = d.tool(Tool::Shortchain);
    if steps > 0 && steps <= short.max_steps.unwrap_or(0) {
        gain += chance(Tool::Shortchain) * w_open * short.count.unwrap_or(0).min(left) as f64 * random_door;
    }
    // Long-Chained Shackles: a long move raises a random open inmate.
    let long = d.tool(Tool::Longchain);
    if steps >= long.min_steps.unwrap_or(usize::MAX) && board > 0 {
        gain += chance(Tool::Longchain) * rises(long.tiers.unwrap_or(0)) / board as f64;
    }
    gain
}

/// The best prey mark a Lion `lion` standing in open cell `n` could make today, as `link_value` prices it: both rise
/// a tier on each of the mornings left, up to the mark's rounds.
fn hunt_walk_gain(ctx: &Ctx, n: usize, lion: &Inmate) -> f64 {
    let st = ctx.st;
    devour_options(st, n)
        .into_iter()
        .filter(|o| !o.scratch)
        .filter_map(|o| st.cells[o.cell].inmate.filter(|i| st.link_of(i.id).is_none()))
        .map(|prey| climb(ctx, lion.tier) + climb(ctx, prey.tier))
        .fold(0.0, f64::max)
}

/// What the cells 1 to 6 past a landing on `n` are worth to land on tomorrow (`Params::w_ahead`): a sealed door its
/// fresh inmate and the Blacklist potential's rise, a marker its incident score, an open unmarked Lion its best Hunt
/// with the Trap, the Interrogation Room a Bestow's mean rise; half the best of the six, half their mean, as three of
/// the six are offered.
fn ahead_value(ctx: &Ctx, n: usize, board: &Comp) -> f64 {
    let st = ctx.st;
    let p = ctx.p;
    let d = data();
    let cells = d.cell_count;
    let (sealed, left) = (st.sealed_count(), landings_left(st).saturating_sub(1));
    let mut values = [0.0f64; 6];
    for (k, v) in values.iter_mut().enumerate() {
        let m = (n + k + 1) % cells;
        let c = &st.cells[m];
        let mut value = 0.0;
        if c.event.is_some() {
            value += incident_score(ctx, c, board);
        }
        if m == d.room {
            let (mut sum, mut count) = (0.0, 0usize);
            for i in st.cells.iter().filter(|c| c.on_board()).filter_map(|c| c.inmate).filter(|i| i.tier < RAINBOW) {
                sum += rise_worth(ctx, i.tier as usize, 1);
                count += 1;
            }
            if count > 0 {
                value += p.w_event * sum / count as f64;
            }
        } else if c.sealed {
            value += p.w_open * ctx.open_value;
            if let Some(b) = &ctx.black
                && sealed > 0
            {
                value += b.potential(sealed - 1, left) - b.potential(sealed, left);
            }
        } else if let Some(i) = c.inmate
            && i.beast == Beast::Lion
            && p.w_hunt_walk != 0.0
            && st.has(Tool::Trap)
            && st.link_of(i.id).is_none()
        {
            value += p.w_hunt_walk * hunt_walk_gain(ctx, m, &i);
        }
        *v = value;
    }
    let best = values.iter().cloned().fold(0.0, f64::max);
    let mean = values.iter().sum::<f64>() / 6.0;
    0.5 * best + 0.5 * mean
}

/// linkValue: what a prey mark still promises; both inmates rise a tier each morning.
pub fn link_value(ctx: &Ctx, lion: u32, prey: u32, rounds: i32) -> f64 {
    // Count only the mornings left in this run: the prey marked on the last day will not rise again (a Hunt ends the
    // day; if not counted, the last day would pick Hunt and end the whole run early).
    let rounds = rounds.min(ctx.st.max_days.saturating_sub(ctx.st.day) as i32);
    let mut value = 0.0;
    for id in [lion, prey] {
        let Some(i) = ctx.st.cell_of(id).and_then(|n| ctx.st.cells[n].inmate) else { continue };
        let t = i.tier as usize;
        let to = TOP.min((i.tier + rounds) as usize);
        value += worth(ctx, to) - ctx.v[t];
    }
    value
}

/// linkLoss: the prey mark an action would end, valued (0 if none).
pub fn link_loss(ctx: &Ctx, inmate: Option<Inmate>, certain: bool) -> f64 {
    let Some(inmate) = inmate else { return 0.0 };
    let Some(k) = ctx.st.link_of(inmate.id) else { return 0.0 };
    let link = ctx.st.prey[k];
    // A Lion that devours re-rolls its beast; 1 in 4 times it stays a Lion.
    link_value(ctx, link.lion, link.prey, link.rounds) * if certain || link.prey == inmate.id { 1.0 } else { 0.75 }
}

/// coinLoss: the worth of Kin's Coin of Fate lost when `gone` leave the board;
/// `board` holds every open inmate below Rainbow.
fn coin_loss(ctx: &Ctx, board: &Comp, gone: &[Inmate]) -> f64 {
    if !ctx.st.has(Tool::Coin) {
        return 0.0;
    }
    let mut after = *board;
    for i in gone.iter().filter(|i| i.tier < RAINBOW) {
        after[i.tier as usize * 4 + i.beast.index()] -= 1;
    }
    coin_of_fate(ctx, board) - coin_of_fate(ctx, &after)
}

/// The open inmates' tiers by cell, as Ravenous Chains reads them, -1 where
/// none stands; empty without the Chains or a morning left.
fn chains_ring(st: &State) -> Vec<i8> {
    if !st.has(Tool::Chains) || st.max_days <= st.day {
        return Vec::new();
    }
    st.cells.iter().map(|c| c.inmate.filter(|_| !c.sealed).map_or(-1, |i| i.tier as i8)).collect()
}

/// chainsLoss: what Ravenous Chains takes on the next morning from the board
/// `ring` (`chains_ring`, 0 when empty): over the tied top inmates, the worth
/// of the open ring neighbours it removes less the expected worth of the
/// centre's rise, one roll per removal.
fn chains_loss(ctx: &Ctx, ring: &[i8]) -> f64 {
    let Some(&top) = ring.iter().max().filter(|&&t| t >= 0) else { return 0.0 };
    let def = data().tool(Tool::Chains);
    let (p, by) = (def.chance / 100.0, def.tiers.unwrap_or(0));
    let n = ring.len();
    let (mut tops, mut loss) = (0, 0.0);
    for c in (0..n).filter(|&c| ring[c] == top) {
        tops += 1;
        let sides = [ring[(c + n - 1) % n], ring[(c + 1) % n]];
        let removed = sides.iter().filter(|&&t| t >= 0).count();
        loss += sides.iter().filter(|&&t| t >= 0).map(|&t| worth(ctx, t as usize)).sum::<f64>();
        // At most two removals: j of them land in [1, removed, 1] ways.
        for j in 0..=removed {
            let odds = [1.0, removed as f64, 1.0][j] * p.powi(j as i32) * (1.0 - p).powi((removed - j) as i32);
            loss -= odds * rise_worth(ctx, top as usize, j as i32 * by);
        }
    }
    loss / f64::from(tops)
}

/// The Tusk's gain on a Replace in a marked cell: random open inmates below
/// Rainbow rise a tier each.
fn tusk_gain(ctx: &Ctx) -> f64 {
    let st = ctx.st;
    let board: Vec<usize> =
        st.cells.iter().filter_map(|c| c.inmate.filter(|i| !c.sealed && i.tier < RAINBOW)).map(|i| i.tier as usize).collect();
    let lift = board.iter().fold(0.0, |sum, &t| sum + ctx.v[t + 1] - ctx.v[t]) / board.len().max(1) as f64;
    (st.params.ivory_count as usize).min(board.len()) as f64 * lift
}

/// huntGain: what a Hunt on the target in `cell` adds, the value of its prey mark. A Lion already marked rises once a
/// morning however many marks it holds (engine prey_rise), so a second mark earns only its prey's climb.
fn hunt_gain(ctx: &Ctx, cell: usize) -> f64 {
    let st = ctx.st;
    let (lion, prey) = (st.cells[st.position].inmate.expect("lion"), st.cells[cell].inmate.expect("prey"));
    let value = link_value(ctx, lion.id, prey.id, st.params.prey_rounds as i32);
    if st.link_of(lion.id).is_none() {
        return value;
    }
    value - climb(ctx, lion.tier)
}

/// What an inmate of tier `t` gains from a prey mark: a tier on each of the mornings left, up to the mark's rounds.
fn climb(ctx: &Ctx, t: i32) -> f64 {
    let st = ctx.st;
    let rounds = (st.params.prey_rounds as i32).min(st.max_days.saturating_sub(st.day) as i32);
    worth(ctx, TOP.min((t + rounds) as usize)) - ctx.v[t as usize]
}

/// The worth of the best prey mark a Lion of tier `t` standing in cell `at` could make now, the climb of both, over
/// the unmarked inmates of tier `t` on the board it could eat (`can_eat`); 0 when there is none or no morning is left.
fn mark_worth(ctx: &Ctx, t: i32, at: usize) -> f64 {
    let st = ctx.st;
    let lion = Inmate { id: u32::MAX, beast: Beast::Lion, tier: t };
    let eats = eating(st);
    let any = st.cells.iter().any(|c| {
        c.n != at && c.on_board() && c.inmate.is_some_and(|i| i.tier == t && st.link_of(i.id).is_none() && eats(&lion, Some(&i)))
    });
    if any { 2.0 * climb(ctx, t) } else { 0.0 }
}

/// What a Replace of `old` in the warden's cell adds for the Hunt it may open (`Params::w_hunt_swap`): the odds the
/// new inmate is a Lion (one of the other three beasts, `Params::replace_weights`; none with the Iron Pestle), by the
/// odds `dist` of each tier, times `mark_worth`. 0 without the Trap, with today's Hunt made, on the last day, or when
/// the warden's inmate is a Lion already.
fn hunt_after_swap(ctx: &Ctx, old: &Inmate, dist: &Dist) -> f64 {
    let st = ctx.st;
    if !st.has(Tool::Trap) || st.used.hunt != 0 || st.day >= st.max_days || old.beast == Beast::Lion || st.has(Tool::Pestle) {
        return 0.0;
    }
    let w = st.params.replace_weights();
    let total: f64 = (0..4).filter(|&b| b != old.beast.index()).map(|b| f64::from(w[b])).sum();
    if total == 0.0 {
        return 0.0;
    }
    let lion = f64::from(w[Beast::Lion.index()]) / total;
    let sum: f64 = dist.iter().enumerate().take(TOP).filter(|&(_, &p)| p > 0.0).map(|(t, &p)| p * mark_worth(ctx, t as i32, st.position)).sum();
    lion * sum
}

/// bestHunt: the target among `options` whose prey mark is worth the most, with its gain.
pub fn best_hunt(ctx: &Ctx, options: &[usize]) -> Option<(usize, f64)> {
    options.iter().map(|&n| (n, hunt_gain(ctx, n))).fold(None, |best, (n, g)| match best {
        Some((_, b)) if b >= g => best,
        _ => Some((n, g)),
    })
}

/// devourScores: the given first or chain bites for the warden's inmate, as
/// (cell, score), best first.
pub fn devour_scores(ctx: &mut Ctx, options: Vec<Bite>) -> Vec<(usize, f64)> {
    let st = ctx.st;
    let here = &st.cells[st.position];
    let Some(eater) = here.inmate.filter(|_| !here.sealed) else { return Vec::new() };
    let (mut comp, crowd) = composition(st, Some(st.position));
    let board = composition(st, None).0;
    let tier = eater.tier as usize;
    let beast = eater.beast.index();
    let own = link_loss(ctx, Some(eater), false);
    let mut ring = chains_ring(st);
    let chains_before = chains_loss(ctx, &ring);
    let block = if ring.is_empty() { 0 } else { st.board_count() };
    // Executioner's Blacklist: a meal's cell seals again (`Params::reusable_cells`), one more door before the
    // instrument fires, or one firing fewer (`Black`, `Params::w_black_seal`).
    let reseal = match &ctx.black {
        Some(b) if st.params.reusable_cells => {
            let (sealed, left) = (st.sealed_count(), landings_left(st));
            ctx.p.w_black_seal * (b.potential(sealed + 1, left) - b.potential(sealed, left))
        }
        _ => 0.0,
    };
    let mut out: Vec<(usize, f64)> = options
        .into_iter()
        .map(|o| {
            let target = &st.cells[o.cell];
            let meal = if o.scratch { target.hidden } else { target.inmate }.expect("meal");
            let meal_link = if o.scratch { 0.0 } else { link_loss(ctx, Some(meal), true) };
            let same_link = own != 0.0 && meal_link != 0.0 && st.link_of(eater.id) == st.link_of(meal.id);
            let mut loss = if same_link { own.max(meal_link) } else { own + meal_link };
            // The eater leaves its tier and the meal the board (the chain after is not priced).
            let gone = [eater, meal];
            loss += coin_loss(ctx, &board, &gone[..if o.scratch { 1 } else { 2 }]);
            // Ravenous Chains: the meal's cell empties and the eater lands
            // its sure gain up (a Chromatic leaves for the vault).
            if !ring.is_empty() {
                let (e, m) = (ring[st.position], ring[o.cell]);
                let landing = (eater.tier + sure_gain(st, &eater, &meal, o.scratch, block)).min(st.top_tier());
                ring[o.cell] = -1;
                ring[st.position] = if landing >= RAINBOW { -1 } else { landing as i8 };
                loss += chains_loss(ctx, &ring) - chains_before;
                ring[o.cell] = m;
                ring[st.position] = e;
            }
            let stair_loss = if o.scratch { 0.0 } else { stair_delta(ctx, &mut comp, meal.tier as usize, meal.beast.index(), -1) };
            let delta = devour_delta(ctx, tier, beast, (meal.tier as usize, meal.beast.index()), o.scratch, &mut comp, crowd);
            let mut score = delta - loss + stair_loss;
            if !o.scratch {
                score += reseal;
            }
            // Objective progress: a Consume, or the eater's sure landing reaching the rarity.
            if let Some(goal) = &ctx.goal {
                let gain = match goal.kind {
                    ObjectiveKind::DevourCount => 1.0,
                    ObjectiveKind::ReachRating => {
                        let landing = eater.tier + sure_gain(st, &eater, &meal, o.scratch, st.board_count());
                        if landing.min(st.top_tier()) >= goal.rating { 1.0 } else { 0.0 }
                    }
                    _ => 0.0,
                };
                score += goal.term(ctx.p, gain);
            }
            (o.cell, score)
        })
        .collect();
    best_first(&mut out);
    out
}

/// swapGainHere: a swap in the warden's cell, the new inmate's worth over the
/// old one less any prey mark it ends, plus the Tusk bonus if the cell has one.
pub fn swap_gain_here(ctx: &Ctx) -> f64 {
    let st = ctx.st;
    let here = &st.cells[st.position];
    let Some(inmate) = here.inmate.filter(|_| !here.sealed && st.replace_left()) else {
        return f64::NEG_INFINITY;
    };
    let (comp, crowd) = composition(st, Some(st.position));
    let mut gain = swap_value(ctx, inmate.tier as usize, &comp, crowd - 1)
        - link_loss(ctx, Some(inmate), true)
        - coin_loss(ctx, &composition(st, None).0, &[inmate]);
    // Blood-Stained Trap: the Hunt a Replace may open.
    if ctx.p.w_hunt_swap != 0.0 {
        gain += ctx.p.w_hunt_swap * hunt_after_swap(ctx, &inmate, &replace_odds(ctx, inmate.tier as usize, &comp, crowd - 1).0);
    }
    // Gaoler's Execution Axe: a swap that raises the rarity, counting the
    // Stake's and the Maiden's rises, opens random sealed cells.
    if st.has(Tool::Axe) {
        let doors = data().tool(Tool::Axe).count.unwrap_or(0).min(st.sealed_count());
        let rise: f64 = replace_odds(ctx, inmate.tier as usize, &comp, crowd - 1).0.iter().skip(inmate.tier as usize + 1).sum();
        gain += rise * doors as f64 * ctx.p.w_open * ctx.open_value;
    }
    // Ravenous Chains: the new inmate's tier follows the Replace odds.
    let mut ring = chains_ring(st);
    if !ring.is_empty() {
        let before = chains_loss(ctx, &ring);
        let mut after = 0.0;
        for (t, &p) in replace_odds(ctx, inmate.tier as usize, &comp, crowd - 1).0.iter().enumerate().filter(|&(_, &p)| p > 0.0) {
            ring[st.position] = if t >= TOP { -1 } else { t as i8 };
            after += p * chains_loss(ctx, &ring);
        }
        gain -= after - before;
    }
    if here.ivory {
        return gain + tusk_gain(ctx);
    }
    if ctx.last_day || f64::from(inmate.tier) <= ctx.p.swap_max_tier { gain } else { f64::NEG_INFINITY }
}

/// boardValue: tier values `v` plus vaulted Rainbows worth `r` each, with a
/// bonus for inmates that have something to eat.
fn board_value(st: &State, v: &[f64; 6], r: f64) -> f64 {
    let mut value = st.vault.len() as f64 * r;
    let eaters = eater_mask(st);
    for c in &st.cells {
        let Some(i) = c.inmate.filter(|_| !c.sealed) else { continue };
        value += v[i.tier as usize] * if eaters & (1 << c.n) == 0 { 1.0 } else { 1.25 };
    }
    value
}

/// The tier values and Rainbow worth a gift pick weighs the board with. Two
/// inmates of a tier make one of the next, so early on an inmate counts as the
/// inmates it took to make: 2^t. With fewer than 4 days left the tiers still
/// to climb weigh more than the inmates spent, 3^t. The last day keeps the
/// strategy's own values, where each tier is worth its Riot loot.
fn gift_tiers(st: &State, p: &Params, (v, r, last_day): ([f64; 6], f64, bool)) -> ([f64; 6], f64) {
    if last_day {
        return (v, r);
    }
    let base: f64 = if st.max_days - st.day < 4 { 3.0 } else { 2.0 };
    let v = [0, 1, 2, 3, 4, 5].map(|t| base.powi(t));
    // A full vault stops inmates at Ultimate, as in `tier_values`.
    (v, if st.top_tier() < RAINBOW { v[TOP - 1] } else { v[TOP] * p.rainbow_mult })
}

/// bestByBoard: the option whose result has the highest board value; random
/// ones are sampled three times. Gifts (`gift`) weigh tiers by `gift_tiers`;
/// the Interrogation Room's cards by the strategy's tier values.
pub fn best_by_board<'k>(ctx: &Ctx, options: &[&'k str], gift: bool, roll: &mut Rng32) -> &'k str {
    let mut best: Option<(&str, f64)> = None;
    // Use a single copy, recopied whole from `ctx.st` before every trial application: a fresh clone each time would
    // reallocate every list again (2026-11-03: Opulence solve 4.5% faster).
    let mut trial: Option<State> = None;
    for &id in options {
        let mut value = 0.0;
        let samples = if ["ring", "horn", "instigate", "bestow"].contains(&id) { 3 } else { 1 };
        for _ in 0..samples {
            let trial = match &mut trial {
                Some(t) => {
                    t.copy_from(ctx.st);
                    t
                }
                None => trial.insert(ctx.st.clone()),
            };
            trial.rng = (roll.roll() * 4294967296.0) as u32;
            if !apply(trial, &Action::Choose { id: ChoiceId::Name(id.to_string()) }).is_ok() {
                continue;
            }
            let (v, r) = if gift {
                gift_tiers(trial, ctx.p, tier_values(trial, ctx.p))
            } else {
                let (v, r, _) = tier_values(trial, ctx.p);
                (v, r)
            };
            value += board_value(trial, &v, r);
        }
        value /= f64::from(samples);
        if best.is_none_or(|(_, v)| value > v) {
            best = Some((id, value));
        }
    }
    best.map_or(options[0], |(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::new_game;
    use crate::engine::state::{ObjectiveState, Pending, Phase};
    use crate::solver::{TUNED, context, landings_left};

    /// Opening instrument pick with the board not yet read (all locked): every simulated round randomly releases the
    /// opening inmates; once the board has been read they are not released.
    #[test]
    fn an_unseen_start_opens_the_start_rooms_in_each_round() {
        use crate::engine::state::{Cell, ToolReason};
        let mut st = new_game("START");
        st.cells = (0..st.cells.len()).map(Cell::blank).collect();
        st.tools.clear();
        st.pending = vec![Pending::Tool { options: vec![Tool::Whip, Tool::Saw, Tool::Keys], reason: ToolReason::Start }];
        let mut round = st.clone();
        crate::solver::solve::hide_future(&mut round, 7);
        assert_eq!(round.board_count(), data().open_rooms_at_start);
        // Only after the instrument pick comes the first day's patrol: with a rolled destination, the opening event
        // marks are added too.
        assert_eq!(round.phase, Phase::Patrol);
        assert!(!round.destinations.is_empty());
        assert!(round.cells.iter().filter(|c| c.event.is_some()).count() >= data().min_markers);
        // When the opening board has been read, compute from what was read.
        let n = (data().room + 2) % st.cells.len();
        st.cells[n].sealed = false;
        st.cells[n].inmate = Some(Inmate { id: 99, beast: Beast::Rat, tier: 0 });
        let mut round = st.clone();
        crate::solver::solve::hide_future(&mut round, 7);
        assert_eq!(round.board_count(), 1);
    }

    /// The solver's chain-consume valuation: a chained Skewered Rat Heads adds a tier only when a rat eats a rat
    /// (under the JS rules it adds for anything eaten).
    #[test]
    fn the_skewer_values_extra_tiers_only_on_a_rat() {
        use crate::solver::chain::devour_delta;
        let (rat, mammoth) = (Beast::Rat.index(), Beast::Mammoth.index());
        let delta = |rats_only: bool, meal: usize| {
            let mut st = bare(0);
            st.objective = None;
            st.tools = vec![Tool::Skewer];
            st.params.skewer_rats_only = rats_only;
            let mut ctx = context(&st, &TUNED);
            let mut comp = [0; 24];
            comp[meal] += 1;
            devour_delta(&mut ctx, 0, rat, (0, meal % 4), false, &mut comp, 2)
        };
        assert_eq!(delta(true, rat), delta(false, rat));
        assert!(delta(true, mammoth) < delta(false, mammoth));
    }

    /// A Hunt ends the day, and the prey marked on the last day will not rise again: no Hunt is advised on the last
    /// day.
    #[test]
    fn no_hunt_on_the_last_day() {
        let mut st = bare(0);
        st.objective = None;
        st.tools = vec![Tool::Trap];
        (st.phase, st.position) = (Phase::Manage, 3);
        st.cells[3].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: 1 });
        st.cells[4].inmate = Some(Inmate { id: 901, beast: Beast::Wolf, tier: 1 });
        st.day = st.max_days;
        let pick = crate::solver::decide(&st, &TUNED, &mut crate::solver::Rng32::new(1));
        assert!(!matches!(pick, Some(Action::Hunt { .. })), "{pick:?}");
        let ctx = context(&st, &TUNED);
        assert_eq!(best_hunt(&ctx, &[4]).map(|(_, gain)| gain), Some(0.0));
    }

    /// A block with every cell open and empty, the warden in cell 0, and the
    /// objective `id` (data/tartarus-data.json) not started.
    fn bare(id: u32) -> State {
        let mut st = new_game("goal");
        for c in &mut st.cells {
            (c.sealed, c.inmate, c.hidden, c.event, c.nest, c.known) = (false, None, None, None, None, 0);
        }
        st.tools.clear();
        (st.position, st.chain) = (0, false);
        st.objective = Some(ObjectiveState { id, progress: 0, done: false });
        st
    }

    #[test]
    fn a_lion_with_the_trap_ends_the_chain_for_its_hunt() {
        let mut st = bare(5);
        (st.phase, st.position) = (Phase::Manage, 3);
        st.cells[3].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: 2 });
        st.cells[4].inmate = Some(Inmate { id: 901, beast: Beast::Rat, tier: 2 });
        st.pending = vec![Pending::Chain { from: 3, eater_id: 900, options: vec![4] }];
        let pick = |st: &State| crate::solver::decide(st, &TUNED, &mut crate::solver::Rng32::new(1));
        assert_eq!(pick(&st), Some(Action::Chain { to: 4 }));
        st.tools.push(Tool::Trap);
        assert_eq!(pick(&st), Some(Action::EndChain));
    }

    #[test]
    fn a_move_distance_objective_favours_the_longer_walk() {
        let st = bare(5);
        let walks: Vec<usize> = (1..st.cells.len()).filter(|&n| n != data().room).collect();
        let (near, far) = (walks[0], walks[4]);
        let mut p = TUNED.clone();
        (p.w_step, p.w_goal) = (-0.5, 0.0);
        assert_eq!(patrol_scores(&mut context(&st, &p), &[near, far])[0].0, near);
        p.w_goal = 64.0;
        assert_eq!(patrol_scores(&mut context(&st, &p), &[near, far])[0].0, far);
    }

    #[test]
    fn a_devour_count_objective_adds_its_share_to_every_bite() {
        let mut st = bare(2);
        st.cells[0].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: 1 });
        st.cells[3].inmate = Some(Inmate { id: 901, beast: Beast::Rat, tier: 1 });
        let bite = || vec![Bite { cell: 3, scratch: false }];
        let mut p = Params { w_goal: 0.0, ..TUNED.clone() };
        let plain = devour_scores(&mut context(&st, &p), bite())[0].1;
        p.w_goal = 8.0;
        // Target 4 Consumes: one is a quarter of w_goal.
        assert_eq!(devour_scores(&mut context(&st, &p), bite())[0].1, plain + 2.0);
        st.objective = Some(ObjectiveState { id: 2, progress: 4, done: true });
        assert_eq!(devour_scores(&mut context(&st, &p), bite())[0].1, plain);
    }

    /// Holding the Blacklist, a landing on a door scores the potential's rise over a landing on an empty cell, and a
    /// bite pays the reseal: the fall of the potential, times `w_black_seal`. Without the Blacklist, or with
    /// `w_black` 0, neither is priced.
    #[test]
    fn the_blacklist_potential_prices_doors_and_reseals() {
        // The look-ahead is off: it prices the doors past each landing, which this test does not isolate.
        let tuned = Params { w_ahead: 0.0, ..TUNED.clone() };
        let mut st = bare(0);
        st.objective = None;
        (st.day, st.max_days, st.phase, st.position) = (5, 20, Phase::Patrol, 0);
        for n in 1..9 {
            st.cells[n].sealed = true;
        }
        st.cells[10].inmate = Some(Inmate { id: 900, beast: Beast::Rat, tier: 1 });
        st.cells[12].inmate = Some(Inmate { id: 901, beast: Beast::Wolf, tier: 2 });
        let (door, empty) = (3usize, 11usize);
        let scores = |st: &State, p: &Params| {
            let out = patrol_scores(&mut context(st, p), &[door, empty]);
            let at = |n: usize| out.iter().find(|(c, _)| *c == n).expect("scored").1;
            at(door) - at(empty)
        };
        let plain = Params { w_black: 0.0, ..tuned.clone() };
        let gap_without = scores(&st, &plain);
        assert!((scores(&st, &tuned) - gap_without).abs() < 1e-9, "no Blacklist held");
        st.tools.push(Tool::Blacklist);
        assert!((scores(&st, &plain) - gap_without).abs() < 1e-9, "w_black 0");
        let gap = scores(&st, &tuned) - gap_without;
        assert!(gap > 0.0, "{gap}");
        // Eight doors and 16 landings to come: the potential rises by one door's share of the firings.
        {
            let ctx = context(&st, &tuned);
            let b = ctx.black.as_ref().expect("potential");
            assert!(b.fires(7, 15) > b.fires(8, 15) && b.fires(8, 15) > 0.0);
            assert!((gap - (b.potential(7, 15) - b.potential(8, 15))).abs() < 1e-9);
        }
        // A bite: the warden with the Wolf, a Rat of its tier to eat.
        (st.phase, st.position) = (Phase::Manage, 12);
        st.cells[10].inmate = Some(Inmate { id: 900, beast: Beast::Rat, tier: 2 });
        let bite = || vec![Bite { cell: 10, scratch: false }];
        let without = devour_scores(&mut context(&st, &plain), bite())[0].1;
        let with = devour_scores(&mut context(&st, &tuned), bite())[0].1;
        let ctx = context(&st, &tuned);
        let b = ctx.black.as_ref().expect("potential");
        let fall = tuned.w_black_seal * (b.potential(9, 15) - b.potential(8, 15));
        assert!(fall < 0.0 && (with - without - fall).abs() < 1e-9, "{with} {without} {fall}");
    }

    /// In a Cage Fight the Blacklist is held without its potential: a door and an empty cell score as without it,
    /// and a bite pays no reseal.
    #[test]
    fn a_cage_fight_holds_no_blacklist_potential() {
        use crate::engine::data::CAGE_FIGHT;
        let mut st = bare(0);
        st.objective = None;
        (st.day, st.max_days, st.phase, st.position) = (5, 20, Phase::Patrol, 0);
        for n in 1..9 {
            st.cells[n].sealed = true;
        }
        st.cells[10].inmate = Some(Inmate { id: 900, beast: Beast::Rat, tier: 1 });
        st.tools.push(Tool::Blacklist);
        st.theme = Some(CAGE_FIGHT);
        assert!(context(&st, &TUNED).black.is_none());
        let plain = Params { w_black: 0.0, ..TUNED.clone() };
        let with = patrol_scores(&mut context(&st, &TUNED), &[3, 11]);
        let without = patrol_scores(&mut context(&st, &plain), &[3, 11]);
        assert_eq!(with, without);
        st.theme = None;
        assert!(context(&st, &TUNED).black.is_some());
    }

    /// A second mark on a Lion already marked is worth its prey's climb alone.
    #[test]
    fn a_marked_lions_second_mark_counts_the_prey_alone() {
        use crate::engine::state::PreyLink;
        let mut st = bare(0);
        st.objective = None;
        st.tools = vec![Tool::Trap];
        (st.day, st.max_days, st.phase, st.position) = (5, 20, Phase::Manage, 3);
        st.cells[3].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: 2 });
        st.cells[4].inmate = Some(Inmate { id: 901, beast: Beast::Rat, tier: 2 });
        st.cells[6].inmate = Some(Inmate { id: 902, beast: Beast::Wolf, tier: 2 });
        let (both, climb) = {
            let ctx = context(&st, &TUNED);
            (best_hunt(&ctx, &[4]).expect("a mark").1, worth(&ctx, 5) - ctx.v[2])
        };
        assert!((both - 2.0 * climb).abs() < 1e-9);
        st.prey.push(PreyLink { lion: 900, prey: 902, prey_beast: Beast::Wolf, rounds: 3, next: 6 });
        let second = best_hunt(&context(&st, &TUNED), &[4]).expect("a mark").1;
        assert!((second - climb).abs() < 1e-9, "{second} {climb}");
    }

    /// Holding the Trap with a Rat on the board, a Replace of the warden's Mammoth is worth a third of the mark a Lion
    /// of the new inmate's tier could put on it; not without the Trap, not when the warden's inmate is a Lion, and not
    /// once today's Hunt is made.
    #[test]
    fn a_replace_may_bring_a_lion_that_hunts() {
        let mut st = bare(0);
        st.objective = None;
        (st.day, st.max_days, st.phase, st.position) = (5, 20, Phase::Manage, 3);
        st.cells[3].inmate = Some(Inmate { id: 900, beast: Beast::Mammoth, tier: 1 });
        st.cells[7].inmate = Some(Inmate { id: 901, beast: Beast::Rat, tier: 1 });
        let gain = |st: &State, p: &Params| swap_gain_here(&context(st, p));
        let plain = Params { w_hunt_swap: 0.0, ..TUNED.clone() };
        let without = gain(&st, &plain);
        assert!((gain(&st, &TUNED) - without).abs() < 1e-9, "no Trap held");
        st.tools = vec![Tool::Trap];
        assert!((gain(&st, &plain) - without).abs() < 1e-9, "w_hunt_swap 0");
        let with = gain(&st, &TUNED);
        assert!(with > without, "{with} {without}");
        // The new inmate's tier odds say how often it matches the Rat; a Lion of tier 1 marking it climbs both.
        let want = {
            let ctx = context(&st, &TUNED);
            let (comp, crowd) = composition(&st, Some(3));
            let p1 = replace_odds(&ctx, 1, &comp, crowd - 1).0[1];
            TUNED.w_hunt_swap / 3.0 * p1 * 2.0 * (worth(&ctx, 4) - ctx.v[1])
        };
        assert!((with - without - want).abs() < 1e-9, "{} {want}", with - without);
        st.used.hunt = 1;
        assert!((gain(&st, &TUNED) - without).abs() < 1e-9, "Hunt made today");
        st.used.hunt = 0;
        st.cells[3].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: 1 });
        assert!((gain(&st, &TUNED) - gain(&st, &plain)).abs() < 1e-9, "a Lion already");
    }

    /// Offered the Blacklist and the Claws, the strategy takes the Blacklist, but in a Cage Fight the Claws: there it
    /// picks by the order of 2026-10-31.
    #[test]
    fn a_cage_fight_picks_instruments_by_the_old_order() {
        use crate::engine::data::CAGE_FIGHT;
        use crate::engine::state::ToolReason;
        let mut st = bare(0);
        st.pending = vec![Pending::Tool { options: vec![Tool::Claws, Tool::Blacklist], reason: ToolReason::Start }];
        let pick = |st: &State| crate::solver::decide(st, &TUNED, &mut Rng32::new(1));
        let take = |t: Tool| Some(Action::Choose { id: ChoiceId::Name(t.key().to_string()) });
        assert_eq!(pick(&st), take(Tool::Blacklist));
        st.theme = Some(CAGE_FIGHT);
        assert_eq!(pick(&st), take(Tool::Claws));
    }

    /// Two empty landings, one with sealed doors past it, one with nothing: with `w_ahead` the first scores more by
    /// the look-ahead, without it they tie.
    #[test]
    fn a_landing_with_doors_ahead_scores_the_look_ahead() {
        let mut st = bare(0);
        st.objective = None;
        (st.day, st.max_days, st.phase, st.position) = (5, 20, Phase::Patrol, 0);
        // Landing 3 has doors in 4, 5 and 6; landing 10 has only empty cells past it.
        for n in 4..7 {
            st.cells[n].sealed = true;
        }
        let gap = |p: &Params| {
            let out = patrol_scores(&mut context(&st, p), &[3, 10]);
            let at = |n: usize| out.iter().find(|(c, _)| *c == n).expect("scored").1;
            at(3) - at(10)
        };
        let plain = Params { w_ahead: 0.0, ..TUNED.clone() };
        let steps = plain.w_step * (3.0 - 10.0);
        assert!((gap(&plain) - steps).abs() < 1e-9, "{}", gap(&plain));
        let ctx = context(&st, &TUNED);
        // Three doors of six cells: best = a door, mean = half a door.
        let door = TUNED.w_open * ctx.open_value;
        let want = TUNED.w_ahead * (0.5 * door + 0.5 * door * 3.0 / 6.0);
        assert!((gap(&TUNED) - steps - want).abs() < 1e-9, "{} {want}", gap(&TUNED) - steps);
    }

    /// Today's landing counts while it is not yet made.
    #[test]
    fn landings_left_counts_todays_patrol() {
        let mut st = bare(0);
        (st.day, st.max_days, st.phase) = (20, 20, Phase::Patrol);
        assert_eq!(landings_left(&st), 1);
        st.phase = Phase::Manage;
        assert_eq!(landings_left(&st), 0);
        (st.day, st.phase) = (5, Phase::Patrol);
        assert_eq!(landings_left(&st), 16);
    }

    /// Holding the Trap, a landing on a Lion that could mark a Rat as prey scores more than a landing on the Rat;
    /// without the Trap the two score as before.
    #[test]
    fn the_trap_sends_the_patrol_to_a_lion_with_prey() {
        let mut st = bare(0);
        st.objective = None;
        (st.day, st.max_days, st.phase, st.position) = (5, 20, Phase::Patrol, 0);
        st.cells[4].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: 2 });
        st.cells[8].inmate = Some(Inmate { id: 901, beast: Beast::Rat, tier: 2 });
        let lion_over_rat = |st: &State, p: &Params| {
            let out = patrol_scores(&mut context(st, p), &[4, 8]);
            let at = |n: usize| out.iter().find(|(c, _)| *c == n).expect("scored").1;
            at(4) - at(8)
        };
        let plain = Params { w_hunt_walk: 0.0, ..TUNED.clone() };
        let before = lion_over_rat(&st, &plain);
        assert!((lion_over_rat(&st, &TUNED) - before).abs() < 1e-9, "no Trap held");
        st.tools.push(Tool::Trap);
        assert!((lion_over_rat(&st, &plain) - before).abs() < 1e-9, "w_hunt_walk 0");
        assert!(lion_over_rat(&st, &TUNED) > before + 1.0);
    }

    #[test]
    fn tyrannys_extra_room_cards_count_in_the_room_landing() {
        let mut st = bare(5);
        st.position = 3;
        st.cells[6].inmate = Some(Inmate { id: 900, beast: Beast::Lion, tier: RAINBOW - 1 });
        let (room, p) = (data().room, Params { room_tyranny: 1.0, ..TUNED.clone() });
        let plain = patrol_scores(&mut context(&st, &p), &[room])[0].1;
        // Tyranny's extra Bestow makes the Chromatic, whose Treasure comes before the Room's card.
        st.theme = Some(TYRANNY);
        assert!((patrol_scores(&mut context(&st, &p), &[room])[0].1 - plain).abs() < 1e-9);
    }
}
