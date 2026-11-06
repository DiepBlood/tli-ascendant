//! Setup, day flow, doors and incident markers (js/engine.js lines 406-1260).
//! Function names follow the JS ones in snake case.

use super::data::{Beast, Gift, IRON_LAW, Incident, OPULENCE, ObjectiveKind, RAINBOW, Spawn, Tool, data};
use super::rng::{hash_seed, pick_index, pick_one, rand, shuffle};
use super::state::{
    Cell, EventRef, Inmate, ObjectiveState, Params, Pending, Phase, Roster, RosterRow, RunResult, State, Stats, ToolReason,
    Used, VaultEntry,
};
use std::collections::BTreeMap;

const VERSION: u32 = 4;

/// Who opened a door: a patrol, the run start, a side door (no triggers) or
/// an instrument whose effect lets "after opening" instruments fire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    Patrol,
    Start,
    Side,
    Tool,
    /// Door opened by the Three-Headed Thumb Ring (a Warden's Treasure reward): like Side it does not trigger the
    /// instruments that act after a door opens, and the Hook has no effect on it (`Params::ring_ignores_hook`).
    Gift,
}

/// Steps clockwise from a to b, so 0 means the same cell.
pub fn steps_between(a: usize, b: usize) -> usize {
    let cells = data().cell_count;
    (b + cells - a) % cells
}

pub fn weights<const N: usize>(list: &[u32; N]) -> [f64; N] {
    list.map(f64::from)
}

/// A held instrument's trigger roll at its data chance; no draw when not held.
pub fn fires(s: &mut State, tool: Tool) -> bool {
    s.has(tool) && rand(&mut s.rng) * 100.0 < data().tool(tool).chance
}

pub fn make_inmate(s: &mut State, beast: Beast, tier: i32) -> Inmate {
    let id = s.next_id;
    s.next_id += 1;
    Inmate { id, beast, tier: tier.clamp(0, s.top_tier()) }
}

/// Rarity odds for whoever is behind a door. The Sorting Hook keeps out Normals.
pub fn door_weights(s: &State) -> [f64; RAINBOW as usize] {
    let mut w = weights(&s.params.tier_weights);
    if s.has(Tool::Hook) && w[1..].iter().any(|&x| x > 0.0) {
        w[0] = 0.0;
    }
    w
}

pub fn roll_inmate(s: &mut State, tier_weights: &[f64]) -> Inmate {
    let tier = pick_index(&mut s.rng, tier_weights) as i32;
    let beast = Beast::ALL[pick_index(&mut s.rng, &weights(&s.params.beast_weights))];
    make_inmate(s, beast, tier)
}

/// Who comes out of a door: the inmate already behind it, unless the Sorting
/// Hook rules out a Normal the player hasn't seen. When `hook` is false (a door opened by the Ring) the
/// Hook does not apply.
fn door_inmate(s: &mut State, n: usize, hook: bool) -> Inmate {
    let held = s.has(Tool::Hook);
    let hook = hook && held;
    let cell = &s.cells[n];
    // A door the Hook does not apply to (the Ring): behind a door that Scales and the like have already drawn
    // according to the Hook and the player has not seen, redraw with the ordinary odds.
    let redraw = held && !hook && cell.known == 0;
    if let Some(h) = cell.hidden
        && !redraw
        && (h.tier > 0 || cell.known > 0 || !hook)
    {
        return h;
    }
    let w = if hook { door_weights(s) } else { weights(&s.params.tier_weights) };
    roll_inmate(s, &w)
}

/// Ends every prey mark that is no longer valid.
pub fn check_prey(s: &mut State) {
    let links = std::mem::take(&mut s.prey);
    s.prey = links
        .into_iter()
        .filter(|link| {
            let lion = s.board_cell_of(link.lion).and_then(|n| s.cells[n].inmate);
            let prey = s.board_cell_of(link.prey).and_then(|n| s.cells[n].inmate);
            matches!((lion, prey), (Some(l), Some(p)) if l.beast == Beast::Lion && p.beast == link.prey_beast)
        })
        .collect();
}

/// Empties a cell. With reusable cells it seals again.
pub fn empty_cell(s: &mut State, n: usize) -> Option<Inmate> {
    let sealed = s.params.reusable_cells;
    let cell = &mut s.cells[n];
    let inmate = cell.inmate.take();
    cell.hidden = None;
    cell.known = 0;
    cell.scratch = false;
    cell.nest = None;
    cell.ivory = false;
    cell.sealed = sealed;
    inmate
}

/// Draws up to `count` distinct instruments the warden can still take, by
/// weight, without the held ones and any a held instrument excludes.
pub fn draw_tools(s: &mut State, count: usize) -> Vec<Tool> {
    let d = data();
    let barred: Vec<Tool> = s.tools.iter().flat_map(|&t| d.tool(t).exclude.iter().copied()).collect();
    let mut pool: Vec<Tool> = d.tool_ids.iter().copied().filter(|t| !s.has(*t) && !barred.contains(t) && d.tool(*t).weight > 0).collect();
    let mut out = Vec::new();
    while out.len() < count && !pool.is_empty() {
        let w: Vec<f64> = pool.iter().map(|&t| f64::from(d.tool(t).weight)).collect();
        out.push(pool.remove(pick_index(&mut s.rng, &w)));
    }
    out
}

/// Rewards for growth: objective progress, and the vault for anyone who
/// reaches Chromatic.
pub fn after_growth(s: &mut State, inmate: Inmate) {
    let d = data();
    s.stats.best = s.stats.best.max(inmate.tier);
    if let Some(goal) = s.objective
        && d.objective(goal.id).is_some_and(|o| inmate.tier >= o.rating)
    {
        advance_objective(s, ObjectiveKind::ReachRating, 1);
    }
    if inmate.tier < RAINBOW {
        return;
    }
    let Some(n) = s.cell_of(inmate.id) else { return };
    empty_cell(s, n);
    s.vault.push(VaultEntry { id: inmate.id, beast: inmate.beast, day: s.day });
    s.cache += 1;
    if s.params.rewards {
        s.pending.push(Pending::Gift { options: Gift::ALL.to_vec(), picks: treasure_picks(s.theme) });
    }
}

/// The picks a Warden's Treasure gives in `theme`: Opulence gives more from the same treasure.
pub fn treasure_picks(theme: Option<u32>) -> u32 {
    let extra = if theme == Some(OPULENCE) { data().theme(OPULENCE).and_then(|t| t.value).unwrap_or(0) as u32 } else { 0 };
    1 + extra
}

/// Objective progress; a completed objective queues an instrument pick.
pub fn advance_objective(s: &mut State, kind: ObjectiveKind, amount: u32) {
    let d = data();
    let Some(goal) = s.objective else { return };
    if goal.done || amount == 0 {
        return;
    }
    let Some(objective) = d.objective(goal.id) else { return };
    if objective.kind != kind {
        return;
    }
    let progress = objective.target.min(goal.progress + amount);
    let done = progress >= objective.target;
    s.objective = Some(ObjectiveState { id: goal.id, progress, done });
    if !done || !s.params.rewards {
        return;
    }
    let options = draw_tools(s, d.instrument_picks);
    if !options.is_empty() {
        s.pending.push(Pending::Tool { options, reason: ToolReason::Objective });
    }
}

/// Opens a sealed cell; `forced` places that beast and tier instead of the
/// occupant behind the door.
pub fn open_cell(s: &mut State, n: usize, how: How, forced: Option<(Beast, i32)>) {
    if !s.cells[n].sealed {
        return;
    }
    let inmate = match forced {
        Some((beast, tier)) => make_inmate(s, beast, tier),
        None => door_inmate(s, n, !(how == How::Gift && s.params.ring_ignores_hook)),
    };
    let cell = &mut s.cells[n];
    cell.sealed = false;
    cell.inmate = Some(inmate);
    cell.hidden = None;
    cell.known = 0;
    cell.scratch = false;
    cell.nest = None;
    s.stats.opened += 1;
    // Rooms opened at the start of the run also count in the game (`Params::start_counts_open`).
    if how != How::Start || s.params.start_counts_open {
        advance_objective(s, ObjectiveKind::OpenCellCount, 1);
    }
    after_growth(s, inmate);
    if how != How::Patrol && how != How::Tool {
        return;
    }
    super::tools::after_opening(s, n, inmate.id)
}

/// Opens `count` random unopened cells for an instrument or incident.
pub fn open_random(s: &mut State, count: usize, how: How) {
    let mut doors = s.sealed_cells();
    shuffle(&mut s.rng, &mut doors);
    doors.truncate(count);
    for n in doors {
        open_cell(s, n, how, None);
    }
}

// ---------- Prison Incident markers ----------

/// SpawnCellState; the Interrogation Room never carries a marker.
pub fn incident_fits(id: u32, cell: &Cell) -> bool {
    let d = data();
    if cell.n == d.room {
        return false;
    }
    match d.incident(id).spawn {
        Spawn::Any => true,
        Spawn::Unopened => cell.sealed,
        Spawn::Opened => !cell.sealed,
    }
}

pub fn incident_weight(s: &State, id: u32) -> u32 {
    s.incident_weights.get(&id).copied().unwrap_or_else(|| data().incident(id).weight)
}

/// Whether an incident can be drawn now: weight above 0, no held instrument
/// in excludeRelic, under repeatLimit when the data sets one.
fn drawable(s: &State, e: &Incident) -> bool {
    incident_weight(s, e.id) > 0
        && !e.exclude.iter().any(|&t| s.has(t))
        && !(e.repeat_limit > 0 && s.incident_counts.get(&e.id).copied().unwrap_or(0) >= e.repeat_limit)
}

/// The incidents a marker on cell `n` could carry, with their draw weights.
pub fn fitting_incidents(s: &State, n: usize) -> impl Iterator<Item = (u32, u32)> + '_ {
    let cell = &s.cells[n];
    data().incidents.iter().filter(move |e| drawable(s, e) && incident_fits(e.id, cell)).map(move |e| (e.id, incident_weight(s, e.id)))
}

/// Whether the player sees a marker's incident: not in a game whose
/// incidents stay hidden, which includes every read board.
pub fn marker_seen(s: &State) -> bool {
    !s.params.hidden_incidents
}

/// Draws an incident by weight among those that fit at least one of `spots`.
pub fn draw_incident(s: &mut State, spots: &[usize]) -> Option<u32> {
    let d = data();
    // Which spawn rules the spots allow: `incident_fits` without the lookup.
    let (mut sealed, mut open) = (false, false);
    for &n in spots {
        let c = &s.cells[n];
        if c.n != d.room {
            sealed |= c.sealed;
            open |= !c.sealed;
        }
    }
    let (mut pool, mut w) = (Vec::new(), Vec::new());
    for e in &d.incidents {
        let fits = match e.spawn {
            Spawn::Any => sealed || open,
            Spawn::Unopened => sealed,
            Spawn::Opened => open,
        };
        if fits && drawable(s, e) {
            pool.push(e.id);
            w.push(f64::from(incident_weight(s, e.id)));
        }
    }
    if pool.is_empty() {
        return None;
    }
    Some(pool[pick_index(&mut s.rng, &w)])
}

/// Cells a new marker can go on: no marker yet, not the Interrogation Room,
/// and not the warden's cell when the params keep markers off it.
fn free_cells(s: &State) -> Vec<usize> {
    let room = data().room;
    let warden = s.params.markers_avoid_warden.then_some(s.position);
    s.cells.iter().filter(|c| c.n != room && c.event.is_none() && Some(c.n) != warden).map(|c| c.n).collect()
}

/// Puts a marker on `cell`, or on a random free cell that fits the drawn incident.
pub fn add_marker(s: &mut State, cell: Option<usize>) -> bool {
    let free = match cell {
        Some(n) => vec![n],
        None => free_cells(s),
    };
    let Some(id) = draw_incident(s, &free) else { return false };
    let fit: Vec<usize> = free.into_iter().filter(|&n| incident_fits(id, &s.cells[n])).collect();
    let at = fit[pick_one(&mut s.rng, fit.len())];
    s.cells[at].event = Some(id);
    *s.incident_counts.entry(id).or_insert(0) += 1;
    true
}

/// Drops markers whose cell no longer fits and tops the board up to the minimum.
pub fn sync_markers(s: &mut State) {
    for c in &mut s.cells {
        if let Some(id) = c.event
            && !incident_fits(id, c)
        {
            c.event = None;
        }
    }
    let mut count = s.cells.iter().filter(|c| c.event.is_some()).count();
    while count < data().min_markers && add_marker(s, None) {
        count += 1;
    }
}

// ---------- Setup ----------

/// newGame for the `empty` scenario (a full run) with default params.
pub fn new_game(seed: &str) -> State {
    new_game_with(seed, Params::default())
}

/// newGame for the `empty` scenario with the given params.
pub fn new_game_with(seed: &str, params: Params) -> State {
    let d = data();
    let seed: String = seed.trim().to_uppercase().chars().take(40).collect();
    let scenario = "empty".to_string();
    let mut s = State {
        v: VERSION,
        rng: hash_seed(&format!("{seed}|{scenario}")),
        scenario,
        seed,
        day: 1,
        max_days: params.drill_days,
        params,
        phase: Phase::Patrol,
        position: d.room,
        dice: Vec::new(),
        destinations: Vec::new(),
        chain: false,
        event: None,
        incident_counts: BTreeMap::new(),
        incident_weights: BTreeMap::new(),
        cells: (0..d.cell_count).map(Cell::blank).collect(),
        vault: Vec::new(),
        cache: 0,
        tools: Vec::new(),
        rerolls: 0,
        pending: Vec::new(),
        objective: None,
        theme: None,
        used: Used::default(),
        prey: Vec::new(),
        next_id: 1,
        stats: Stats::default(),
        result: None,
    };
    draw_run_goals(&mut s);
    run_start(&mut s);
    begin_day(&mut s);
    sync_markers(&mut s);
    s
}

/// The run's objective and special theme, from a stream of their own.
fn draw_run_goals(s: &mut State) {
    let d = data();
    let mut stream = hash_seed(&format!("{}|goals", s.seed));
    let mut objective = d.objective(s.params.objective);
    if objective.is_none() && d.objectives_per_run > 0 {
        let w: Vec<f64> = d.objectives.iter().map(|o| f64::from(o.weight)).collect();
        objective = Some(&d.objectives[pick_index(&mut stream, &w)]);
    }
    s.objective = objective.map(|o| ObjectiveState { id: o.id, progress: 0, done: false });
    let mut theme = u32::try_from(s.params.theme).ok().filter(|&id| d.theme(id).is_some());
    if s.params.theme == 0 {
        let roll = rand(&mut stream) * 10000.0;
        // The caller's runsWithoutTheme is 0 here.
        let pity = d.theme_pity_runs == 0;
        if pity || roll < d.theme_chance_per_10000 {
            let w: Vec<f64> = d.themes.iter().map(|t| f64::from(t.weight)).collect();
            theme = Some(d.themes[pick_index(&mut stream, &w)].id);
        }
    }
    s.theme = theme;
    if theme == Some(IRON_LAW) {
        let days = d.theme(IRON_LAW).and_then(|t| t.value).unwrap_or(0) as u32;
        s.max_days = s.max_days.max(days);
    }
}

/// The run start: random rooms open and the player picks an instrument.
fn run_start(s: &mut State) {
    let d = data();
    open_start_rooms(s);
    let options = draw_tools(s, d.instrument_picks);
    if !options.is_empty() {
        s.pending.insert(0, Pending::Tool { options, reason: ToolReason::Start });
    }
}

/// Rooms opened at random at the start of the run: releases the starting inmates.
pub fn open_start_rooms(s: &mut State) {
    let mut rooms = s.sealed_cells();
    shuffle(&mut s.rng, &mut rooms);
    rooms.truncate(data().open_rooms_at_start);
    for n in rooms {
        open_cell(s, n, How::Start, None);
    }
}

// ---------- Day flow ----------

/// Today's moves from the dice: the cells they land on, nearest first, the room only when a die
/// lands there (the S16 cards, docs/s16-screen.md). `roll_dice` draws distinct steps, so only a
/// state copied from a JS trace (tests/common `follow_js_repeat`) can hold a repeated step.
pub fn destinations_from_dice(s: &mut State) {
    let d = data();
    let pos = s.position;
    let mut ahead: Vec<usize> = Vec::new();
    for &pips in &s.dice {
        let n = (pos + pips as usize) % d.cell_count;
        if !ahead.contains(&n) {
            ahead.push(n);
        }
    }
    ahead.sort_by_key(|&n| steps_between(pos, n));
    s.destinations = ahead;
}

/// "-1 daily destination option" (Barbed Whip, Forged Key, Burning Stake).
pub fn fewer_dice(s: &State) -> i32 {
    s.tools.iter().map(|&t| data().tool(t).fewer_dice).sum()
}

/// Rolls today's dice and derives today's moves from them. The game draws the steps as distinct
/// values (46 of 46 recorded three-card mornings showed three different steps), so a die that
/// repeats an earlier one is redrawn: uniform over the distinct sets, and the same rng stream as
/// independent dice on a morning without a repeat.
pub fn roll_dice(s: &mut State) {
    let d = data();
    let fewer = rand(&mut s.rng) * 100.0 < f64::from(s.params.fewer_doors_chance);
    let pips = d.max_pips - d.min_pips + 1;
    let count = (s.params.destinations as i32 - i32::from(fewer) - fewer_dice(s)).clamp(1, pips as i32) as usize;
    let mut dice: Vec<u32> = Vec::with_capacity(count);
    while dice.len() < count {
        let die = d.min_pips + (rand(&mut s.rng) * f64::from(pips)).floor() as u32;
        if !dice.contains(&die) {
            dice.push(die);
        }
    }
    s.dice = dice;
    destinations_from_dice(s);
}

pub fn begin_day(s: &mut State) {
    let d = data();
    s.phase = Phase::Patrol;
    s.used = Used::default();
    check_prey(s);
    super::tools::prey_rise(s);
    check_prey(s);
    for n in 0..s.cells.len() {
        if n != d.room && s.cells[n].event.is_none() && rand(&mut s.rng) * 10000.0 < f64::from(s.params.incident_spawn_chance) {
            add_marker(s, Some(n));
        }
    }
    super::tools::hatch_nests(s);
    super::tools::morning_tools(s);
    roll_dice(s);
}

pub fn clockwise_path(s: &State, to: usize) -> Vec<usize> {
    let cells = data().cell_count;
    let mut path = Vec::new();
    let mut n = s.position;
    while n != to {
        n = (n + 1) % cells;
        path.push(n);
    }
    if path.is_empty() { vec![to] } else { path }
}

pub fn patrol_options(s: &State) -> Vec<usize> {
    if s.phase == Phase::Patrol { s.destinations.clone() } else { Vec::new() }
}

/// A move onto cell n and what the landing triggers.
pub fn land(s: &mut State, n: usize) {
    let d = data();
    let marker = if n == d.room { None } else { s.cells[n].event };
    s.cells[n].event = None;
    if s.chain {
        // Chain Reaction: every sealed door on the clockwise walk opens.
        for k in clockwise_path(s, n) {
            open_cell(s, k, How::Side, None);
        }
        s.chain = false;
    } else if n != d.room && s.cells[n].sealed {
        open_cell(s, n, How::Patrol, None);
    }
    let steps = steps_between(s.position, n);
    s.position = n;
    advance_objective(s, ObjectiveKind::MoveDistance, steps as u32);
    super::tools::shackles(s, steps);
    if n == d.room {
        super::gifts::interrogation_room_stop(s);
    }
    check_prey(s);
    match marker {
        Some(id) => {
            s.phase = Phase::Event;
            s.event = Some(EventRef { id, cell: n });
            // The game counts it as soon as an event cell is entered, and the objective's instrument pick appears
            // before the event window (`Params::events_count_on_landing`).
            if s.params.events_count_on_landing {
                advance_objective(s, ObjectiveKind::EventTriggerCount, 1);
            }
        }
        None => s.phase = Phase::Manage,
    }
}

pub fn advance_day(s: &mut State) {
    if s.day >= s.max_days {
        finish(s, false);
        return;
    }
    s.day += 1;
    begin_day(s)
}

pub fn finish(s: &mut State, early: bool) {
    s.phase = Phase::Ended;
    s.dice.clear();
    s.destinations.clear();
    s.event = None;
    s.chain = false;
    let roster = roster_of(s);
    let best = roster.rows.iter().map(|r| r.tier).fold(s.stats.best.max(0), i32::max);
    s.result = Some(RunResult { day: s.day, early, roster, cache: s.cache, best, stats: s.stats });
}

pub fn roster_of(s: &State) -> Roster {
    let mut rows: Vec<RosterRow> = s
        .cells
        .iter()
        .filter(|c| c.on_board())
        .filter_map(|c| c.inmate.map(|i| RosterRow { beast: i.beast, tier: i.tier, place: format!("Cell {:02}", c.n + 1) }))
        .collect();
    rows.extend(s.vault.iter().map(|v| RosterRow { beast: v.beast, tier: RAINBOW, place: format!("Vault, day {}", v.day) }));
    rows.sort_by(|a, b| b.tier.cmp(&a.tier).then(a.beast.index().cmp(&b.beast.index())));
    let by_tier = (0..=RAINBOW).map(|t| rows.iter().filter(|r| r.tier == t).count() as u32).collect();
    Roster { rows, by_tier }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rolls `mornings` seeded mornings and counts each pip, checking every roll is `count` distinct pips.
    fn roll_many(s: &mut State, mornings: usize, count: usize) -> [usize; 7] {
        let mut seen = [0usize; 7];
        for _ in 0..mornings {
            roll_dice(s);
            let mut dice = s.dice.clone();
            dice.sort_unstable();
            dice.dedup();
            assert_eq!(dice.len(), count, "dice {:?}", s.dice);
            for &p in &dice {
                assert!((1..=6).contains(&p), "pip {p}");
                seen[p as usize] += 1;
            }
        }
        seen
    }

    #[test]
    fn dice_are_distinct_and_uniform() {
        let mut s = new_game("DISTINCT-DICE");
        s.params.fewer_doors_chance = 0;
        s.tools.clear();
        let seen = roll_many(&mut s, 6000, 3);
        // Each pip shows on half the mornings: 3000 expected, sd about 39.
        for (pip, &n) in seen.iter().enumerate().skip(1) {
            assert!((2800..=3200).contains(&n), "pip {pip} seen {n} times");
        }
        s.tools.push(Tool::Whip);
        roll_many(&mut s, 2000, 2);
    }

    /// A board in the manage phase with the Warden standing on a cell that holds an inmate.
    fn manage_on(beast: Beast, tier: i32) -> (State, usize) {
        let mut s = new_game("RULES-1103");
        s.tools.clear();
        s.pending.clear();
        s.phase = Phase::Manage;
        let n = (data().room + 3) % s.cells.len();
        s.position = n;
        let inmate = make_inmate(&mut s, beast, tier);
        (s.cells[n].sealed, s.cells[n].inmate) = (false, Some(inmate));
        (s, n)
    }

    /// The Iron Maiden looks at the swapped-out inmate: when the other inmates higher and lower than it are equal in
    /// number, the swapped-in one gets +3 tiers; when they are not equal, nothing is added.
    #[test]
    fn the_maiden_reads_the_replaced_inmate() {
        let fresh_tier = |others: &[i32]| {
            let mut tiers = Vec::new();
            for seed in 0..40 {
                let (mut s, n) = manage_on(Beast::Wolf, 2);
                s.tools = vec![Tool::Maiden];
                for k in 0..s.cells.len() {
                    if k != n {
                        s.cells[k].inmate = None;
                    }
                }
                for (k, &t) in others.iter().enumerate() {
                    let c = (n + 2 + k) % s.cells.len();
                    let i = make_inmate(&mut s, Beast::Rat, t);
                    (s.cells[c].sealed, s.cells[c].inmate) = (false, Some(i));
                }
                s.rng = seed;
                let rolled = {
                    let mut plain = s.clone();
                    plain.tools.clear();
                    super::super::actions::swap(&mut plain);
                    plain.cells[n].inmate.expect("inmate").tier
                };
                assert!(super::super::actions::swap(&mut s).is_ok());
                tiers.push((rolled, s.cells[n].inmate.map_or(RAINBOW, |i| i.tier)));
            }
            tiers
        };
        // The swapped-out one is Rare (2): one other is 3 and one is 1, equal in number, so the swapped-in gets +3
        // (capped at Ultimate or Chromatic).
        for (rolled, got) in fresh_tier(&[3, 1]) {
            assert_eq!(got, (rolled + 3).min(RAINBOW), "level: {rolled} -> {got}");
        }
        // The other two are both 3: not equal, nothing added.
        for (rolled, got) in fresh_tier(&[3, 3]) {
            assert_eq!(got, rolled, "not level: {rolled} -> {got}");
        }
    }

    /// A hunt ends the day: once the prey is marked it goes straight into the next day's patrol; the JS rules (switch
    /// off) stay in the manage phase.
    #[test]
    fn a_hunt_ends_the_day() {
        for ends in [true, false] {
            let (mut s, n) = manage_on(Beast::Lion, 1);
            s.tools = vec![Tool::Trap];
            s.params.hunt_ends_day = ends;
            let k = (n + 1) % s.cells.len();
            let wolf = make_inmate(&mut s, Beast::Wolf, 1);
            (s.cells[k].sealed, s.cells[k].inmate) = (false, Some(wolf));
            let day = s.day;
            assert!(super::super::devour::hunt(&mut s, k).is_ok());
            assert_eq!(s.prey.len(), 1);
            if ends {
                assert_eq!((s.day, s.phase), (day + 1, Phase::Patrol));
            } else {
                assert_eq!((s.day, s.phase), (day, Phase::Manage));
            }
        }
    }

    /// Doors opened by the Ring are unaffected by the Hook and can come out Normal; doors opened by a patrol still
    /// never give a Normal.
    #[test]
    fn the_ring_ignores_the_sorting_hook() {
        let normals = |ring_ignores: bool, how: How| {
            let mut count = 0;
            for seed in 0..300 {
                let mut s = new_game("HOOK");
                s.tools = vec![Tool::Hook];
                s.params.ring_ignores_hook = ring_ignores;
                let n = (data().room + 4) % s.cells.len();
                (s.cells[n].sealed, s.cells[n].inmate, s.cells[n].hidden, s.cells[n].known) = (true, None, None, 0);
                s.rng = seed;
                open_cell(&mut s, n, how, None);
                count += usize::from(s.cells[n].inmate.is_some_and(|i| i.tier == 0));
            }
            count
        };
        assert!(normals(true, How::Gift) > 60, "the ring opens Normals");
        assert_eq!(normals(false, How::Gift), 0);
        assert_eq!(normals(true, How::Side), 0);
    }

    /// When the Hook is held, behind a door that Scales and the like already drew according to the Hook (unseen by
    /// the player), a Ring opening redraws with the ordinary odds, so it can still be Normal; patrols and other door
    /// openings still use the pre-drawn one.
    #[test]
    fn the_ring_redraws_a_door_drawn_under_the_hook() {
        let open = |how: How| {
            let mut normals = 0;
            for seed in 0..300 {
                let mut s = new_game("HOOK-SCOUT");
                s.tools = vec![Tool::Hook];
                let n = (data().room + 4) % s.cells.len();
                let behind = make_inmate(&mut s, Beast::Rat, 1);
                (s.cells[n].sealed, s.cells[n].inmate, s.cells[n].hidden, s.cells[n].known) = (true, None, Some(behind), 0);
                s.rng = seed;
                open_cell(&mut s, n, how, None);
                normals += usize::from(s.cells[n].inmate.is_some_and(|i| i.tier == 0));
            }
            normals
        };
        assert!(open(How::Gift) > 60);
        assert_eq!(open(How::Side), 0);
    }

    /// Rooms opened at the start of the run count toward the "open cells" objective; the JS rules do not count them.
    #[test]
    fn the_start_rooms_count_for_open_cells() {
        for counts in [true, false] {
            let params = Params { objective: 3, start_counts_open: counts, ..Params::default() };
            let s = new_game_with("START-OPEN", params);
            let progress = s.objective.expect("objective").progress as usize;
            assert_eq!(progress, if counts { s.board_count() } else { 0 }, "counts {counts}");
        }
    }

    /// The "trigger events" objective counts as soon as an event cell is entered, and is not counted again when the
    /// event is confirmed.
    #[test]
    fn an_incident_counts_on_landing() {
        let (mut s, _) = manage_on(Beast::Rat, 0);
        s.phase = Phase::Patrol;
        s.objective = Some(ObjectiveState { id: 4, progress: 0, done: false });
        let n = (s.position + 2) % s.cells.len();
        (s.cells[n].sealed, s.cells[n].inmate, s.cells[n].event) = (false, None, Some(super::super::incidents::CHAIN_REACTION));
        land(&mut s, n);
        assert_eq!(s.phase, Phase::Event);
        assert_eq!(s.objective.expect("objective").progress, 1);
        assert!(super::super::incidents::resolve_event(&mut s).is_ok());
        assert_eq!(s.objective.expect("objective").progress, 1);
    }

    /// The one swapped in is not the same beast kind; the other 3 kinds appear by the swap's beast weights
    /// (2026-11-03).
    #[test]
    fn a_replace_brings_another_beast() {
        let mut seen = [0; 4];
        for seed in 0..2000 {
            let (mut s, n) = manage_on(Beast::Lion, 1);
            s.rng = seed;
            assert!(super::super::actions::swap(&mut s).is_ok());
            seen[s.cells[n].inmate.expect("new inmate").beast.index()] += 1;
        }
        assert_eq!(seen[Beast::Lion.index()], 0, "{seen:?}");
        let w = super::super::state::REPLACE_BEAST_WEIGHTS;
        let total: u32 = w.iter().sum::<u32>() - w[Beast::Lion.index()];
        for b in [Beast::Mammoth, Beast::Wolf, Beast::Rat] {
            let share = f64::from(seen[b.index()]) / 2000.0;
            let want = f64::from(w[b.index()]) / f64::from(total);
            assert!((share - want).abs() < 0.04, "{b:?} {share:.3} vs {want:.3}: {seen:?}");
        }
    }

    /// With the Scapegoat's Guillotine held (a swap removes other inmates and the new one is always rarer), the one
    /// swapped in is also not the same beast kind.
    #[test]
    fn a_scapegoat_replace_brings_another_beast_too() {
        let min = data().tool(Tool::Scapegoat).min_others.unwrap_or(0);
        for seed in 0..100 {
            let (mut s, n) = manage_on(Beast::Wolf, 1);
            s.tools = vec![Tool::Scapegoat];
            let open: Vec<usize> = (0..s.cells.len()).filter(|&k| k != n && k != data().room).take(min + 2).collect();
            for k in open {
                let i = make_inmate(&mut s, Beast::Rat, 0);
                (s.cells[k].sealed, s.cells[k].inmate) = (false, Some(i));
            }
            s.rng = seed;
            assert!(super::super::actions::swap(&mut s).is_ok());
            let fresh = s.cells[n].inmate.expect("new inmate");
            assert_ne!(fresh.beast, Beast::Wolf, "seed {seed}");
            assert!(fresh.tier > 1, "seed {seed}");
        }
    }

    /// The Blacklist fires last among the instruments that act after a door opens: with keys and 7 doors left,
    /// patrolling a door, the keys open 2 more on either side leaving 4, and the Blacklist fires; when the keys did
    /// not trigger 6 are left and it does not fire.
    #[test]
    fn the_blacklist_counts_doors_after_the_keys() {
        let (mut fired, mut quiet) = (0, 0);
        // Run the same seed once with the Blacklist and once without (control): the Blacklist draws its random
        // numbers last, so the doors opened are the same on both sides, and a differing board means the Blacklist
        // fired.
        let run = |seed: u32, tools: Vec<Tool>| {
            let mut s = new_game("BLACKLIST");
            s.tools = tools;
            s.pending.clear();
            let room = data().room;
            let n = (room + 5) % s.cells.len();
            let len = s.cells.len() as isize;
            let ring = |k: isize| (n as isize + k).rem_euclid(len) as usize;
            let sealed = [n, ring(-1), ring(1), ring(3), ring(4), ring(5), ring(6)];
            for k in 0..s.cells.len() {
                let c = &mut s.cells[k];
                (c.hidden, c.known, c.scratch, c.nest, c.event, c.ivory) = (None, 0, false, None, None, false);
                c.sealed = k != room && sealed.contains(&k);
                c.inmate = None;
            }
            for k in 0..s.cells.len() {
                if k != room && !sealed.contains(&k) {
                    let i = make_inmate(&mut s, Beast::Rat, 0);
                    s.cells[k].inmate = Some(i);
                }
            }
            s.rng = seed;
            open_cell(&mut s, n, How::Patrol, None);
            let keys = !s.cells[ring(-1)].sealed && !s.cells[ring(1)].sealed;
            (s.cells.clone(), keys)
        };
        for seed in 0..200 {
            let (with, keys) = run(seed, vec![Tool::Blacklist, Tool::Keys]);
            let (without, _) = run(seed, vec![Tool::Keys]);
            if keys {
                assert_ne!(with, without, "seed {seed}: the keys left 4 doors but the blacklist did not fire");
                fired += 1;
            } else {
                assert_eq!(with, without, "seed {seed}: 6 doors left, the blacklist fired");
                quiet += 1;
            }
        }
        assert!(fired > 100 && quiet > 10, "{fired} {quiet}");
    }

    /// After a bite that day swapping is impossible; the JS engine rules (switch off) allow it.
    #[test]
    fn a_bite_uses_up_the_replace() {
        let (mut s, _) = manage_on(Beast::Wolf, 1);
        // When only the "eat" card is missing (the eater has no target), swapping is still possible.
        s.used.devour = 1;
        assert!(s.replace_left());
        s.used.bitten = true;
        assert!(!s.replace_left());
        assert!(!super::super::actions::swap(&mut s).is_ok());
        s.params.bite_ends_replace = false;
        assert!(super::super::actions::swap(&mut s).is_ok());
    }

    /// The skewered rat head gets 2 extra tiers only when a rat eats a rat; eating a Mammoth gives only the basic 1
    /// tier.
    #[test]
    fn the_skewer_adds_tiers_only_on_a_rat() {
        let mut s = new_game("SKEWER");
        s.tools = vec![Tool::Skewer];
        let rat = make_inmate(&mut s, Beast::Rat, 0);
        let other_rat = make_inmate(&mut s, Beast::Rat, 0);
        let mammoth = make_inmate(&mut s, Beast::Mammoth, 0);
        let gain = |s: &State, meal: &Inmate| super::super::devour::sure_gain(s, &rat, meal, false, 9);
        assert_eq!(gain(&s, &other_rat), 3);
        assert_eq!(gain(&s, &mammoth), 1);
        s.params.skewer_rats_only = false;
        assert_eq!(gain(&s, &mammoth), 3);
    }

    /// Opulence's same treasure: after one pick, the next time still offers all 4 rewards.
    #[test]
    fn an_opulence_pick_offers_every_reward_again() {
        let (mut s, _) = manage_on(Beast::Rat, 0);
        s.pending = vec![Pending::Gift { options: Gift::ALL.to_vec(), picks: 3 }];
        let pick = super::super::actions::ChoiceId::Name(Gift::Wine.key().to_string());
        assert!(super::super::actions::choose(&mut s, &pick).is_ok());
        assert_eq!(s.pending.first(), Some(&Pending::Gift { options: Gift::ALL.to_vec(), picks: 2 }));
    }

    #[test]
    fn loot_counts_the_vault_and_the_open_board() {
        let mut s = new_game("LOOT");
        for c in &mut s.cells {
            c.inmate = None;
        }
        s.vault = (0..2).map(|id| VaultEntry { id: 1000 + id, beast: Beast::ALL[0], day: 1 }).collect();
        let room = data().room;
        let cells: Vec<usize> = (0..s.cells.len()).filter(|&n| n != room).take(3).collect();
        for (&n, (tier, sealed)) in cells.iter().zip([(RAINBOW - 1, false), (RAINBOW - 2, false), (RAINBOW - 1, true)]) {
            let inmate = make_inmate(&mut s, Beast::ALL[0], tier);
            s.cells[n].sealed = sealed;
            s.cells[n].inmate = Some(inmate);
        }
        assert!((s.loot() - 2.25).abs() < 1e-9, "loot {}", s.loot());
    }

    /// The Elephant-Foot Maul sets a Mammoth's rarity to the meal's two tiers
    /// up: higher, the same, or lower (a Cage Fight's lower meal).
    #[test]
    fn the_maul_lands_a_mammoth_two_tiers_over_its_meal() {
        let mut s = new_game("MAUL");
        s.tools = vec![Tool::Maul];
        s.theme = Some(super::super::data::CAGE_FIGHT);
        let eater = make_inmate(&mut s, Beast::Mammoth, 3);
        for (meal_tier, gain) in [(0, -1), (1, 0), (3, 2), (4, 3)] {
            let meal = make_inmate(&mut s, Beast::Lion, meal_tier);
            assert_eq!(crate::engine::devour::sure_gain(&s, &eater, &meal, false, 10), gain, "meal tier {meal_tier}");
        }
    }

    /// With the vault full an Ultimate eats nothing, a Cage Fight's lower
    /// meal included; with room it eats as before.
    #[test]
    fn a_full_vault_leaves_an_ultimate_nothing_to_eat() {
        use crate::engine::devour::{can_eat, eater_mask};
        for (vaulted, eats) in [(9, true), (10, false)] {
            let mut s = new_game("ULTIMATE-DIET");
            s.theme = Some(super::super::data::CAGE_FIGHT);
            s.vault = (0..vaulted as u32).map(|id| VaultEntry { id: 1000 + id, beast: Beast::ALL[0], day: 1 }).collect();
            let lion = make_inmate(&mut s, Beast::Lion, RAINBOW - 1);
            let same = make_inmate(&mut s, Beast::Wolf, RAINBOW - 1);
            let lower = make_inmate(&mut s, Beast::Rat, 0);
            assert_eq!(can_eat(&s, &lion, Some(&same)), eats, "vault {vaulted}");
            assert_eq!(can_eat(&s, &lion, Some(&lower)), eats, "vault {vaulted}");
            for c in &mut s.cells {
                (c.sealed, c.inmate, c.scratch) = (true, None, false);
            }
            for (n, i) in [(1, lion), (2, same), (3, lower)] {
                (s.cells[n].sealed, s.cells[n].inmate) = (false, Some(i));
            }
            assert_eq!(eater_mask(&s) & 1 << 1 != 0, eats, "vault {vaulted}");
        }
    }

    /// Promotes an Ultimate on a board whose vault already holds `vaulted` Chromatics.
    fn promote_ultimate(vaulted: usize) -> State {
        let mut s = new_game("VAULT-CAP");
        s.pending.clear();
        s.vault = (0..vaulted as u32).map(|id| VaultEntry { id: 1000 + id, beast: Beast::ALL[0], day: 1 }).collect();
        s.cells[0].sealed = false;
        let ultimate = make_inmate(&mut s, Beast::ALL[0], RAINBOW - 1);
        s.cells[0].inmate = Some(ultimate);
        crate::engine::tools::promote(&mut s, ultimate, 1, false);
        s
    }

    #[test]
    fn a_full_vault_stops_inmates_at_ultimate() {
        let gift = |s: &State| s.pending.iter().any(|p| matches!(p, Pending::Gift { .. }));
        let full = promote_ultimate(10);
        assert_eq!(full.cells[0].inmate.map(|i| i.tier), Some(RAINBOW - 1));
        assert_eq!(full.vault.len(), 10);
        assert!(!gift(&full));
        let room = promote_ultimate(9);
        assert_eq!(room.cells[0].inmate, None);
        assert_eq!(room.vault.len(), 10);
        assert!(gift(&room));
    }

    #[test]
    fn new_markers_avoid_the_warden_and_mornings_add_none() {
        let markers = |s: &State| s.cells.iter().filter(|c| c.event.is_some()).count();
        let room = data().room;
        for seed in 0..300 {
            let mut s = new_game(&format!("MARKERS-{seed}"));
            s.position = (room + 1 + seed % (s.cells.len() - 1)) % s.cells.len();
            for c in &mut s.cells {
                c.event = None;
            }
            sync_markers(&mut s);
            for _ in 0..3 {
                add_marker(&mut s, None);
            }
            assert!(markers(&s) > 0, "seed {seed}: no marker placed");
            assert_eq!(s.cells[s.position].event, None, "seed {seed}: marker on the warden's cell");
            for c in &mut s.cells {
                c.event = None;
            }
            sync_markers(&mut s);
            assert_eq!(markers(&s), 1, "seed {seed}");
            begin_day(&mut s);
            assert_eq!(markers(&s), 1, "seed {seed}: a morning spawned a marker");
        }
    }

    fn offer(s: &State) -> (Vec<Tool>, ToolReason) {
        match s.pending.first() {
            Some(Pending::Tool { options, reason }) => (options.clone(), *reason),
            other => panic!("no instrument pick: {other:?}"),
        }
    }

    #[test]
    fn reroll_redraws_the_offer_and_spends_one() {
        use crate::engine::actions::{Action, apply};
        let mut s = new_game_with("REROLL", Params::default());
        s.rerolls = 2;
        for left in [1, 0] {
            let (before, reason) = offer(&s);
            let expected = draw_tools(&mut s.clone(), before.len());
            assert!(apply(&mut s, &Action::Reroll).is_ok());
            assert_eq!(s.rerolls, left);
            assert_eq!(offer(&s), (expected, reason));
        }
        let at_zero = offer(&s);
        assert!(!apply(&mut s, &Action::Reroll).is_ok());
        assert_eq!(offer(&s), at_zero);
    }

    #[test]
    fn reroll_needs_an_instrument_pick() {
        use crate::engine::actions::{Action, apply};
        let mut s = new_game_with("REROLL", Params::default());
        s.rerolls = 1;
        s.pending.clear();
        assert!(!apply(&mut s, &Action::Reroll).is_ok());
        assert_eq!(s.rerolls, 1);
    }
}
