//! The strategy, a port of js/solver.js lines 16-546: it scores every legal
//! move from what the player can see and plays the best one. Sums run in the
//! JS order and sorts are stable, so a position gets the same pick as the JS.

pub mod advise;
pub mod bench;
pub mod chain;
pub mod scores;
pub mod solve;
pub mod values;

use crate::engine::data::{Beast, CAGE_FIGHT, LOOT, ObjectiveKind, RAINBOW, Tool, data};
use crate::engine::devour::{can_devour_now, devour_options, hunt_targets, start_options};
use crate::engine::rng::rand;
use crate::engine::rules::door_weights;
use crate::engine::state::{Cell, Pending, Phase, State};
use crate::engine::{Action, ChoiceId};
use chain::replace_odds;
use scores::{best_by_board, best_hunt, devour_scores, patrol_scores, swap_gain_here};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use values::fresh_activation;

/// Torture Instruments by Chromatics per run (TOOL_RANK), as the strategy
/// picks and re-rolls them: measured 2026-10-31 holding each from the start,
/// 100,000 strategy games each on seed 81 (Trap 8.68 down to Scapegoat's
/// Guillotine 4.39). The order kept from js/solver.js ranked the Squirming
/// Iron Box 30th (10th measured) and the Execution Axe 29th (13th); this
/// order gained +0.24 to +0.25 +- 0.003 Chromatics a run on seeds 91 and 101
/// (400,000 games each), and one point fewer runs at 2 or fewer.
pub const TOOL_RANK: [Tool; 31] = [
    Tool::Trap, Tool::Maul, Tool::Crown, Tool::Coin, Tool::Rope, Tool::Tongs, Tool::Skewer, Tool::Claws,
    Tool::Revenge, Tool::Ironbox, Tool::Longchain, Tool::Keys, Tool::Axe, Tool::Hook, Tool::Saw, Tool::Collar,
    Tool::Maiden, Tool::Muzzle, Tool::Blacklist, Tool::Beastcage, Tool::Chains, Tool::Guillotine, Tool::Fire,
    Tool::Whip, Tool::Shortchain, Tool::Stake, Tool::Dogtag, Tool::Scales, Tool::Pestle, Tool::Forgedkey,
    Tool::Scapegoat,
];

/// The top tier (Rainbow) as an index.
const TOP: usize = RAINBOW as usize;

/// Strategy weights in tier-value units; see js/solver.js 29-43.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub tier_base: f64,
    pub rainbow_mult: f64,
    pub w_open: f64,
    pub w_now: f64,
    pub w_walk: f64,
    pub w_step: f64,
    pub w_chain: f64,
    /// An incident's or the Interrogation Room's board gain, played on a copy.
    pub w_event: f64,
    pub w_stair: f64,
    pub min_devour: f64,
    pub swap_max_tier: f64,
    /// Progress toward the run's objective, whose completion gives a second
    /// Torture Instrument pick: a move that completes it earns `w_goal`, a
    /// part of it the same share of `w_goal`.
    pub w_goal: f64,
    /// The board value of an unseen marker's other incidents, those besides
    /// Execution Efficiency and Chain Reaction.
    pub w_incident: f64,
    /// With a Re-roll left, an instrument offer is re-rolled when its best
    /// instrument sits at this TOOL_RANK index or lower down.
    pub reroll_rank: f64,
    /// The Room's landing weight factor under Tyranny, whose extra Instigates
    /// place markers the Bestow gain does not count.
    pub room_tyranny: f64,
}

impl Params {
    /// The weight with field name `name` (such as `w_step`), for the
    /// strategy bench's `--set`.
    pub fn weight_mut(&mut self, name: &str) -> Option<&mut f64> {
        Some(match name {
            "tier_base" => &mut self.tier_base,
            "rainbow_mult" => &mut self.rainbow_mult,
            "w_open" => &mut self.w_open,
            "w_now" => &mut self.w_now,
            "w_walk" => &mut self.w_walk,
            "w_step" => &mut self.w_step,
            "w_chain" => &mut self.w_chain,
            "w_event" => &mut self.w_event,
            "w_stair" => &mut self.w_stair,
            "min_devour" => &mut self.min_devour,
            "swap_max_tier" => &mut self.swap_max_tier,
            "w_goal" => &mut self.w_goal,
            "w_incident" => &mut self.w_incident,
            "reroll_rank" => &mut self.reroll_rank,
            "room_tyranny" => &mut self.room_tyranny,
            _ => return None,
        })
    }
}

/// The tuned weights (js/solver.js withDefaults(TUNED)), what `decide(st, null)` plays.
pub static TUNED: Params = Params {
    tier_base: 2.75,
    rainbow_mult: 1.0,
    // With TOOL_RANK as measured, doors weigh more and steps less: 1.0 and
    // 0.25 over 0.5 and 0.75 gained +0.019 to +0.025 +- 0.002 Chromatics a run
    // (seeds 161 and 171, 400,000 games each) and no more runs at 2 or fewer;
    // 1.5 gained a little more with more such runs (2026-10-31).
    w_open: 1.0,
    w_now: 1.5,
    w_walk: 0.5,
    w_step: 0.25,
    w_chain: 16.0,
    w_event: 4.0,
    w_stair: 1.0,
    min_devour: -2.0,
    swap_max_tier: 2.0,
    // With the objective's cache counted as loot, 100,000 strategy games
    // (seed 11) gained +0.009 +- 0.003 Chromatics a run at 32 over 0, and
    // 0.11 points fewer runs at 2 or fewer; 4 to 64 all gained (2026-10-31).
    w_goal: 32.0,
    // An unseen marker (every read board's) is worth landing on over almost
    // anything else: 400,000 strategy games each on seeds 41, 51 and 61
    // gained +0.008 to +0.009 +- 0.001 Chromatics a run at 512 over 64, and
    // 0.10 to 0.12 points fewer runs at 2 or fewer; 256 gained less, 1024 and
    // 4096 no more (2026-10-31).
    w_incident: 512.0,
    // With TOOL_RANK as measured: +0.031 to +0.035 Chromatics a run with one
    // re-roll over 8 (seeds 91 and 101, 200,000 games each), and 0.5 to 0.7
    // points fewer runs at 2 or fewer; 12 and up gained less.
    reroll_rank: 11.0,
    room_tyranny: 4.0,
};

/// mulberry32 (rng32), for the solver's own dice (never the game's).
pub struct Rng32(u32);

impl Rng32 {
    pub fn new(seed: u32) -> Rng32 {
        Rng32(seed)
    }

    pub fn roll(&mut self) -> f64 {
        rand(&mut self.0)
    }
}

/// Open inmates by tier and beast (index tier * 4 + beast).
pub type Comp = [i32; 24];

/// composition: the open inmates below Rainbow leaving out cell `exclude`,
/// plus how many are on the block in all.
pub fn composition(st: &State, exclude: Option<usize>) -> (Comp, i64) {
    let mut comp = [0; 24];
    let mut crowd = 0;
    for c in &st.cells {
        let Some(i) = c.inmate.filter(|_| !c.sealed) else { continue };
        crowd += 1;
        if Some(c.n) == exclude || i.tier >= RAINBOW {
            continue;
        }
        comp[i.tier as usize * 4 + i.beast.index()] += 1;
    }
    (comp, crowd)
}

/// Tier odds, Normal to Rainbow.
pub type Dist = [f64; TOP + 1];

/// normalize: negative weights count as 0; all zero leaves all zero.
pub fn normalize<const N: usize>(weights: [f64; N]) -> [f64; N] {
    let total = weights.iter().fold(0.0, |sum, w| sum + w.max(0.0));
    let total = if total == 0.0 { 1.0 } else { total };
    weights.map(|w| w.max(0.0) / total)
}

/// Who each beast eats without tools: by eater, a bit per beast it eats.
pub fn food_chain() -> [u8; 4] {
    Beast::ALL.map(|b| data().food_chain[b.index()].iter().fold(0, |m, f| m | 1 << f.index()))
}

/// Who each beast eats with the Skewered Rat Heads and the Iron Crown, as in `food_chain`.
pub fn eats_table(st: &State) -> [u8; 4] {
    let mut eats = food_chain();
    if st.has(Tool::Skewer) {
        eats[Beast::Rat.index()] |= 1 << Beast::Rat.index();
    }
    if st.has(Tool::Crown) {
        eats[Beast::Lion.index()] = 0b1111;
    }
    eats
}

/// The beasts set in a `food_chain` mask, by index in ascending order.
pub fn foods(mask: u8) -> impl Iterator<Item = usize> {
    (0..4).filter(move |&f| mask >> f & 1 == 1)
}

/// The hasher of the chain memo (FxHash's multiply-rotate mix): its keys are
/// counts the solver builds, so SipHash's guard against chosen keys only costs.
#[derive(Default, Clone, Copy)]
pub struct MemoHasher(u64);

impl MemoHasher {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for MemoHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
    }
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }
    fn write_u128(&mut self, n: u128) {
        self.add(n as u64);
        self.add((n >> 64) as u64);
    }
    fn finish(&self) -> u64 {
        // The low bits are poorly distributed after the multiplication, and the low bits decide the hash table's
        // bucket; as rustc-hash does, rotate them up (2026-11-03: solve 14% faster with Cage Fight + Avenger's
        // Casket, 9% with the Fanged Guillotine).
        self.0.rotate_left(26)
    }
}

/// Everything a decision needs, computed once per position (context).
pub struct Ctx<'a> {
    pub st: &'a State,
    pub p: &'a Params,
    pub eats: [u8; 4],
    /// Who each beast counters without tools, for the cross-tier bites.
    pub chain: [u8; 4],
    pub v: [f64; 6],
    pub r: f64,
    /// The odds of each count of extra tiers a successful bite rolls (Rusty
    /// Saw, Barbed Whip, and the Barbed Collar on chained bites), by whether
    /// the bite is chained.
    pub extra: [[f64; 4]; 2],
    pub muzzle: bool,
    pub skewer: bool,
    /// A chained Skewered Rat Heads adds tiers when eating any target (JS engine); otherwise only when eating a rat
    /// (`Params::skewer_rats_only`).
    pub skewer_any: bool,
    pub crown: bool,
    /// Whether the chain-consume valuation counts the Avenger's Casket opening a door and sending food. Not counted
    /// in the Cage Fight (approximation): on 137 boards with Cage Fight plus the Avenger's Casket, solving to 1600
    /// rounds has the same loss (0.0057 chromatic per decision), and the time drops from 196 to 80 ms; a normal run
    /// saves only about 10%, so it is still counted there (2026-11-03).
    pub revenge: bool,
    /// Crowded Beastcage's chance that a meal's cell opens again after a bite;
    /// 0 without it or without reusable cells (the cell stays unsealed then).
    pub beastcage: f64,
    /// Elephant-Foot Maul: a Mammoth eats higher countered inmates.
    pub maul: bool,
    /// Fanged Guillotine: a Wolf eats lower countered inmates, with Oppress.
    pub fangs: bool,
    /// Cage Fight theme: any eater eats lower countered inmates and keeps its beast.
    pub cage_fight: bool,
    /// By the eater's beast, a bit per beast whose lower-tier counts its
    /// chain can read (the memo key of `chain::feed`).
    pub down: [u8; 4],
    pub scratch_extra: usize,
    /// The worth of whoever comes out of an unseen door on a patrol or
    /// instrument opening (`door_dist`).
    pub open_value: f64,
    /// The worth of an unseen door opened from the side (Chain Reaction walk,
    /// Crowded Beastcage), which sets off no "after opening" instrument.
    pub side_value: f64,
    /// The run's last day: what counts is the Riot loot, so every tier below
    /// Chromatic is worth only its cache (`LOOT`) and a Replace of any tier
    /// may pay.
    pub last_day: bool,
    /// The run's objective while it is open and `w_goal` is not 0.
    pub goal: Option<Goal>,
    pub memo: Memo,
    /// What `memo`'s values depend on, see [`MemoSig`].
    memo_sig: MemoSig,
}

/// The chain memo of a `Ctx` (see `chain::feed`).
pub type Memo = HashMap<u128, f64, BuildHasherDefault<MemoHasher>>;

/// What the chain memo's values depend on besides their keys: the tier
/// worths and every tool and theme `chain::feed` reads. Two contexts that
/// share it give each key the same value, to the bit.
#[derive(Clone, Copy, PartialEq)]
pub struct MemoSig {
    worth: [u64; 7],
    cage: u64,
    extra: [u64; 8],
    food: [u8; 12],
    flags: u8,
}

impl MemoSig {
    fn of(ctx: &Ctx) -> MemoSig {
        let bits = |x: f64| x.to_bits();
        let mut worth = [0; 7];
        for (w, v) in worth.iter_mut().zip(ctx.v.iter().chain([&ctx.r])) {
            *w = bits(*v);
        }
        let mut food = [0; 12];
        food[..4].copy_from_slice(&ctx.eats);
        food[4..8].copy_from_slice(&ctx.chain);
        food[8..].copy_from_slice(&ctx.down);
        let flags = [ctx.muzzle, ctx.skewer, ctx.skewer_any, ctx.crown, ctx.revenge, ctx.maul, ctx.fangs, ctx.cage_fight].iter().fold(0, |f, &b| f << 1 | u8::from(b));
        MemoSig { worth, cage: bits(ctx.beastcage * ctx.side_value), extra: std::array::from_fn(|i| bits(ctx.extra[i / 4][i % 4])), food, flags }
    }
}

/// The memos dropped `Ctx`s leave for the next on their thread: this many,
/// the latest first, each of at most [`MEMO_KEEP`] counts (about 3 MB).
/// A playout's days and its last day differ in tier worths, and each
/// playout starts again from the board read.
const MEMO_SLOTS: usize = 4;
const MEMO_KEEP: usize = 1 << 16;
/// The counts a new memo has room for: grown from none, its rehashes took
/// 2% of a Cage Fight solve, and a memo made this big 6% less time in all
/// (2026-11-01; room for 65536 took more).
const MEMO_ROOM: usize = 1 << 14;

thread_local! {
    /// The memos and what their values depend on, see [`MEMO_SLOTS`]. The
    /// next context with the same takes its values: in a Cage Fight the
    /// chains read millions of counts a playout, mostly the same from one
    /// decision to the next; a memo cleared each time took 4x a plain run's
    /// time (2026-10-31). The playout threads live on (see `Rollouts::play`),
    /// and their memos with them: about 150 MB held between solves.
    static SPARE_MEMOS: std::cell::RefCell<Spare> = const { std::cell::RefCell::new(Spare(Vec::new())) };
}

/// The contents of [`SPARE_MEMOS`]; subtracted from [`held_memos`] when the thread ends.
struct Spare(Vec<(MemoSig, Memo)>);

impl Drop for Spare {
    fn drop(&mut self) {
        self.0.iter().for_each(|(_, memo)| note_taken(memo));
    }
}

/// How many memos the [`SPARE_MEMOS`] of all threads together hold on to, and about how many bytes (for measuring,
/// see [`held_memos`]). On 2026-11-03 the whole program took about 1.05 GB when idle; this is used to confirm how much
/// of it is memos: clear() does not return capacity, so the kept memos stay at the maximum size they ever grew to.
static MEMO_HELD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static MEMO_HELD_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Bytes allocated by `memo` (approximate): hashbrown's bucket count is a power of 2 and holds at most 7/8, with one
/// entry per bucket plus a control byte.
fn memo_bytes(memo: &Memo) -> usize {
    match memo.capacity() {
        0 => 0,
        cap => (cap * 8 / 7).next_power_of_two() * (std::mem::size_of::<(u128, f64)>() + 1),
    }
}

fn note_kept(memo: &Memo) {
    MEMO_HELD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    MEMO_HELD_BYTES.fetch_add(memo_bytes(memo), std::sync::atomic::Ordering::Relaxed);
}

fn note_taken(memo: &Memo) {
    MEMO_HELD.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    MEMO_HELD_BYTES.fetch_sub(memo_bytes(memo), std::sync::atomic::Ordering::Relaxed);
}

/// Memos kept in reserve by all threads: (count, approximate bytes).
pub fn held_memos() -> (usize, usize) {
    (MEMO_HELD.load(std::sync::atomic::Ordering::Relaxed), MEMO_HELD_BYTES.load(std::sync::atomic::Ordering::Relaxed))
}

/// Drops this thread's kept memos (see [`MEMO_SLOTS`]).
#[cfg(test)]
pub fn forget_chain_memos() {
    SPARE_MEMOS.with(|spare| spare.borrow_mut().0.drain(..).for_each(|(_, memo)| note_taken(&memo)));
}

/// The spare memo for a context with `sig`: its own values when one was
/// kept, else the oldest emptied, or a new one.
fn take_memo(sig: MemoSig) -> Memo {
    SPARE_MEMOS.with(|spare| {
        let spare = &mut spare.borrow_mut().0;
        if let Some(k) = spare.iter().position(|(kept, _)| *kept == sig) {
            let memo = spare.remove(k).1;
            note_taken(&memo);
            return memo;
        }
        if spare.len() < MEMO_SLOTS {
            return Memo::with_capacity_and_hasher(MEMO_ROOM, Default::default());
        }
        let mut memo = spare.pop().map(|(_, memo)| memo).inspect(note_taken).unwrap_or_default();
        memo.clear();
        memo
    })
}

impl Drop for Ctx<'_> {
    fn drop(&mut self) {
        let mut memo = std::mem::take(&mut self.memo);
        if memo.len() > MEMO_KEEP {
            memo.clear();
        }
        let sig = self.memo_sig;
        note_kept(&memo);
        SPARE_MEMOS.with(|spare| spare.borrow_mut().0.insert(0, (sig, memo)));
    }
}

/// The run's open objective, as the strategy prices progress toward it.
#[derive(Clone, Copy, Debug)]
pub struct Goal {
    pub kind: ObjectiveKind,
    /// The rarity a ReachRating objective names.
    pub rating: i32,
    pub target: f64,
    /// The progress still to make.
    pub left: f64,
}

impl Goal {
    /// The objective of `st`; None when there is none, it is done, or `w_goal` is 0.
    fn of(st: &State, p: &Params) -> Option<Goal> {
        let goal = st.objective.filter(|g| !g.done && p.w_goal != 0.0)?;
        let o = data().objective(goal.id)?;
        Some(Goal { kind: o.kind, rating: o.rating, target: f64::from(o.target), left: f64::from(o.target - goal.progress) })
    }

    /// `w_goal * gain / target`, with `gain` capped at the progress left.
    pub fn term(&self, p: &Params, gain: f64) -> f64 {
        p.w_goal * gain.min(self.left) / self.target
    }
}

/// The strategy's tier values `v` and vaulted Rainbow worth `r` on `st`, and
/// whether it is the run's last day, where every tier below Chromatic is worth
/// only its Riot loot (`LOOT`) in vaulted Chromatics.
fn tier_values(st: &State, p: &Params) -> ([f64; 6], f64, bool) {
    let mut v = [0, 1, 2, 3, 4, 5].map(|t| p.tier_base.powi(t));
    // A full vault stops inmates at Ultimate: the top tier is worth no more.
    let r = if st.top_tier() < RAINBOW { v[RAINBOW as usize - 1] } else { v[RAINBOW as usize] * p.rainbow_mult };
    let last_day = st.day >= st.max_days;
    if last_day {
        // Full vault or not.
        let chromatic = v[RAINBOW as usize] * p.rainbow_mult;
        for (t, x) in v[..TOP].iter_mut().enumerate() {
            *x = LOOT[t] * chromatic;
        }
    }
    (v, r, last_day)
}

pub fn context<'a>(st: &'a State, p: &'a Params) -> Ctx<'a> {
    let (v, r, last_day) = tier_values(st, p);
    let open_value = dist_worth(&v, r, &door_dist(st, None, true).0);
    let side_value = dist_worth(&v, r, &door_dist(st, None, false).0);
    let chain = food_chain();
    let cage_fight = st.theme == Some(CAGE_FIGHT);
    // Under Cage Fight the eater keeps its beast; otherwise any eater may
    // re-roll into a Wolf, which eats down with the Fanged Guillotine.
    let down = std::array::from_fn(|b| {
        if cage_fight {
            chain[b]
        } else if st.has(Tool::Guillotine) {
            chain[Beast::Wolf.index()]
        } else {
            0
        }
    });
    let mut ctx = Ctx {
        st,
        p,
        eats: eats_table(st),
        chain,
        down,
        v,
        r,
        extra: [false, true].map(|chained| extra_tiers(st, chained)),
        muzzle: st.has(Tool::Muzzle),
        skewer: st.has(Tool::Skewer),
        skewer_any: st.has(Tool::Skewer) && !st.params.skewer_rats_only,
        crown: st.has(Tool::Crown),
        revenge: st.has(Tool::Revenge) && st.cells.iter().any(|c| c.sealed) && !cage_fight,
        beastcage: if st.has(Tool::Beastcage) && st.params.reusable_cells { data().tool(Tool::Beastcage).chance / 100.0 } else { 0.0 },
        maul: st.has(Tool::Maul),
        fangs: st.has(Tool::Guillotine),
        cage_fight,
        scratch_extra: 1 + usize::from(st.params.scratch_stacks_base),
        open_value,
        side_value,
        last_day,
        goal: Goal::of(st, p),
        memo: Memo::default(),
        memo_sig: MemoSig { worth: [0; 7], cage: 0, extra: [0; 8], food: [0; 12], flags: 0 },
    };
    ctx.memo_sig = MemoSig::of(&ctx);
    ctx.memo = take_memo(ctx.memo_sig);
    ctx
}

/// doorDist: tier odds (Normal to Rainbow) for whoever comes out of sealed
/// cell `cell`, or of an unseen door when None, and its beast when the player
/// has seen it (engine door_inmate). A seen door holds its known rarity (even
/// a Normal under the Sorting Hook); an unseen one rolls `door_weights`, with
/// no Normal under the Hook. On a patrol or instrument opening (`after`),
/// Felon's Dog Tag may raise the inmate. A scratched door prices at its known
/// rarity; the scratch bite its opening gives up is charged by `scratch_loss`.
pub fn door_dist(st: &State, cell: Option<&Cell>, after: bool) -> (Dist, Option<usize>) {
    let seen = cell.filter(|c| seen_door(c)).and_then(|c| c.hidden.map(|h| (h, c.known)));
    let mut dist = [0.0; TOP + 1];
    match seen {
        Some((h, _)) => dist[h.tier as usize] = 1.0,
        None => dist[..TOP].copy_from_slice(&normalize(door_weights(st))),
    }
    if after && st.has(Tool::Dogtag) {
        let def = data().tool(Tool::Dogtag);
        let (c, by, top) = (def.chance / 100.0, def.tiers.unwrap_or(0), st.top_tier());
        let mut raised = [0.0; TOP + 1];
        for (t, &p) in dist.iter().enumerate() {
            raised[t] += p * (1.0 - c);
            raised[(t as i32 + by).clamp(0, top) as usize] += p * c;
        }
        dist = raised;
    }
    (dist, seen.filter(|&(_, k)| k >= 2).map(|(h, _)| h.beast.index()))
}

/// Whether `door_dist` prices sealed cell `c` by what the player saw behind it.
pub fn seen_door(c: &Cell) -> bool {
    c.known > 0 && c.hidden.is_some()
}

/// The worth of an inmate of tier t; a Chromatic is worth `r`.
pub fn worth(ctx: &Ctx, t: usize) -> f64 {
    if t >= TOP { ctx.r } else { ctx.v[t] }
}

/// The beasts an inmate may be and the odds of each: beast `beast` when it is
/// known, else any of the 4 at even odds.
pub fn beasts(beast: Option<usize>) -> (std::ops::Range<usize>, f64) {
    let beasts = beast.map_or(0..4, |b| b..b + 1);
    let share = 1.0 / beasts.len() as f64;
    (beasts, share)
}

/// The expected worth of an inmate with tier odds `dist`; a Chromatic is worth `r`.
pub fn dist_worth(v: &[f64; 6], r: f64, dist: &[f64]) -> f64 {
    dist.iter().enumerate().fold(0.0, |sum, (t, p)| sum + p * if t >= TOP { r } else { v[t] })
}

/// The odds of 0 to 3 extra tiers on a successful bite (engine do_devour):
/// the held Saw, Whip and, on a chained bite, Collar each roll on their own.
fn extra_tiers(st: &State, chained: bool) -> [f64; 4] {
    let mut odds = [1.0, 0.0, 0.0, 0.0];
    for t in [Tool::Saw, Tool::Whip, Tool::Collar] {
        if !st.has(t) || (t == Tool::Collar && !chained) {
            continue;
        }
        let def = data().tool(t);
        let (p, k) = (def.chance / 100.0, def.tiers.unwrap_or(0) as usize);
        let mut next = [0.0; 4];
        for (i, &q) in odds.iter().enumerate() {
            next[i] += (1.0 - p) * q;
            // Each adds one tier in the data, so three at most.
            next[(i + k).min(3)] += p * q;
        }
        odds = next;
    }
    odds
}

/// decide: the move the strategy would play now, or None once the run is over.
pub fn decide(st: &State, p: &Params, roll: &mut Rng32) -> Option<Action> {
    let mut ctx = context(st, p);
    if st.phase == Phase::Ended {
        return None;
    }
    let name = |key: &str| Some(Action::Choose { id: ChoiceId::Name(key.to_string()) });
    if let Some(choice) = st.pending.first() {
        return match choice {
            Pending::Chain { from, options, .. } => {
                // A Hunt still open ends the chain: it is worth more than another bite.
                // When a Hunt ends the day (`Params::hunt_ends_day`), it must be worth more than the next bite before
                // the chain ends to hunt.
                let targets = hunt_targets(st);
                if !targets.is_empty() && !st.params.hunt_ends_day {
                    return Some(Action::EndChain);
                }
                let hunt = best_hunt(&ctx, &targets).filter(|&(_, gain)| gain > 0.0).map_or(f64::NEG_INFINITY, |(_, gain)| gain);
                let bites = devour_options(st, *from).into_iter().filter(|o| options.contains(&o.cell)).collect();
                let next = devour_scores(&mut ctx, bites);
                Some(match next.first() {
                    Some(&(to, score)) if score >= p.min_devour && score >= hunt => Action::Chain { to },
                    _ => Action::EndChain,
                })
            }
            Pending::Gift { options, .. } => {
                let keys: Vec<&str> = options.iter().map(|g| g.key()).collect();
                name(best_by_board(&ctx, &keys, true, roll))
            }
            Pending::Interrogation { options } => {
                let keys: Vec<&str> = options.iter().map(|c| c.key()).collect();
                name(best_by_board(&ctx, &keys, false, roll))
            }
            // Sewer Ratway: the cell a patrol would score best.
            Pending::Cell { options, .. } => {
                Some(Action::Choose { id: ChoiceId::Cell(patrol_scores(&mut ctx, options)[0].0) })
            }
            // The Hunt's target pick: the prey mark worth the most.
            Pending::Prey { options } => best_hunt(&ctx, options).map(|(n, _)| Action::Choose { id: ChoiceId::Cell(n) }),
            Pending::Tool { options, .. } => {
                let best = TOOL_RANK.iter().position(|t| options.contains(t));
                if st.rerolls > 0 && best.unwrap_or(TOOL_RANK.len()) as f64 >= p.reroll_rank {
                    return Some(Action::Reroll);
                }
                name(best.map_or(options[0], |i| TOOL_RANK[i]).key())
            }
        };
    }
    // An incident is confirmed, never declined (S16BoardEventPop.lua:17-38).
    if st.phase == Phase::Event && st.event.is_some() {
        return Some(Action::Event);
    }
    if st.phase == Phase::Patrol {
        let scores = patrol_scores(&mut ctx, &st.destinations);
        return Some(scores.first().map_or(Action::End, |&(cell, _)| Action::Patrol { cell }));
    }
    let hunt = best_hunt(&ctx, &hunt_targets(st)).filter(|&(_, gain)| gain > 0.0);
    // Manage: a Hunt goes first, as a devour after it already counts the
    // prey marks it would end. In the game a Hunt ends the day (`Params::hunt_ends_day`), so the Hunt is only done
    // when it is worth more than the best remaining move today (devour, replace or ending today).
    if let Some((cell, _)) = hunt
        && !st.params.hunt_ends_day
    {
        return Some(Action::Hunt { cell });
    }
    let hunt_over = |value: f64| hunt.filter(|&(_, gain)| gain > value.max(0.0)).map(|(cell, _)| Action::Hunt { cell });
    let swap_gain = swap_gain_here(&ctx);
    if can_devour_now(st) {
        let devours = devour_scores(&mut ctx, start_options(st));
        if let Some(&(to, best)) = devours.first().filter(|d| d.1 >= p.min_devour) {
            // A swap first can be worth more than a weak devour: the new
            // inmate may still devour today.
            let first = if swap_gain > f64::NEG_INFINITY { swap_first(&mut ctx, swap_gain) } else { f64::NEG_INFINITY };
            if first > best {
                return Some(hunt_over(first).unwrap_or(Action::Swap));
            }
            return Some(hunt_over(best).unwrap_or(Action::Devour { to }));
        }
    }
    // On the last day a Replace buys only what the new inmate can still do today.
    let gain = if ctx.last_day && swap_gain > f64::NEG_INFINITY { swap_first(&mut ctx, swap_gain) } else { swap_gain };
    Some(hunt_over(gain).unwrap_or(if gain > 0.0 { Action::Swap } else { Action::End }))
}

/// A Replace in the warden's cell worth `swap_gain`, plus what the new inmate
/// may still do today.
fn swap_first(ctx: &mut Ctx, swap_gain: f64) -> f64 {
    let st = ctx.st;
    let (mut comp, crowd) = composition(st, Some(st.position));
    let old = st.cells[st.position].inmate;
    let dist = replace_odds(ctx, old.map_or(0, |i| i.tier as usize), &comp, crowd - 1).0;
    // Iron Pestle: the new inmate keeps the old one's beast.
    let beast = old.filter(|_| st.has(Tool::Pestle)).map(|i| i.beast.index());
    // In the game the incoming beast is one of the other 3 beasts (`Params::replace_new_beast`), following the replace
    // beast weights (`Params::replace_beast_weights`).
    let other = old.filter(|_| beast.is_none() && st.params.replace_new_beast).map(|i| i.beast.index());
    let now = match other {
        Some(old_beast) => {
            let w = st.params.replace_weights();
            let total: f64 = (0..4).filter(|&b| b != old_beast).map(|b| f64::from(w[b])).sum();
            let mut sum = 0.0;
            for b in (0..4).filter(|&b| b != old_beast && w[b] > 0) {
                sum += f64::from(w[b]) / total * fresh_activation(ctx, &dist, Some(b), &mut comp, crowd, false);
            }
            sum
        }
        None => fresh_activation(ctx, &dist, beast, &mut comp, crowd, false),
    };
    swap_gain + ctx.p.w_now * now
}

/// The strategy's bite once Consume is pressed: decide's own, else the best
/// scored of today's targets; None when the inmate has none. Only bites
/// into the cells of `only` count.
pub fn decide_bite(st: &State, p: &Params, roll: &mut Rng32, only: &[usize]) -> Option<Action> {
    if let Some(bite @ Action::Devour { to }) = decide(st, p, roll)
        && only.contains(&to)
    {
        return Some(bite);
    }
    let mut ctx = context(st, p);
    let options = start_options(st).into_iter().filter(|o| only.contains(&o.cell)).collect();
    devour_scores(&mut ctx, options).first().map(|&(to, _)| Action::Devour { to })
}
