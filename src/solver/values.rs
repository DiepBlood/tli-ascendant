//! Valuing positions (js/solver.js 255-334): chain readiness, swaps, what an
//! inmate can do once the warden stands with it, and Executions.

use super::chain::{best_meal, eats_at, fed, replace_odds};
use super::{Comp, Ctx, TOP, beasts, dist_worth, worth};
use crate::engine::data::data;

/// stair: how ready the block is for chains; at each tier, the share of beasts
/// an eater could re-roll into and still find a meal there, weighted by tier.
pub fn stair(ctx: &Ctx, comp: &Comp) -> f64 {
    let mut value = 0.0;
    for t in 0..TOP {
        let fed = (0..4).filter(|&b| fed(ctx, t, b, comp)).count();
        value += (fed as f64 / 4.0) * ctx.v[t];
    }
    value
}

/// stairDelta: change in readiness from adding (+1) or removing (-1) one inmate.
pub fn stair_delta(ctx: &Ctx, comp: &mut Comp, tier: usize, beast: usize, sign: i32) -> f64 {
    if ctx.p.w_stair == 0.0 || tier >= TOP {
        return 0.0;
    }
    let before = stair(ctx, comp);
    comp[tier * 4 + beast] += sign;
    let after = stair(ctx, comp);
    comp[tier * 4 + beast] -= sign;
    ctx.p.w_stair * (after - before)
}

/// stairFresh: expected readiness change from a new inmate with tier odds
/// `dist`, of beast `beast` when it is known, else any of the 4 at even odds.
pub fn stair_fresh(ctx: &Ctx, dist: &[f64], beast: Option<usize>, comp: &mut Comp) -> f64 {
    if ctx.p.w_stair == 0.0 {
        return 0.0;
    }
    let (beasts, share) = beasts(beast);
    // `stair_delta` of each new inmate from who has a meal now: one more
    // inmate only feeds the eaters it is a meal for, and only if none of its
    // tier and beast was on the board. Summed as `stair` sums.
    let fed_now: [[bool; 4]; TOP] = std::array::from_fn(|t| std::array::from_fn(|b| fed(ctx, t, b, comp)));
    let value = |fed: &[[bool; 4]; TOP]| (0..TOP).fold(0.0, |value, t| value + (fed[t].iter().filter(|&&f| f).count() as f64 / 4.0) * ctx.v[t]);
    let before = value(&fed_now);
    let mut sum = 0.0;
    for (t, &p) in dist.iter().enumerate() {
        if p == 0.0 || t >= TOP {
            continue;
        }
        for b in beasts.clone() {
            let mut after = fed_now;
            if comp[t * 4 + b] == 0 {
                for (et, row) in after.iter_mut().enumerate() {
                    for (eb, f) in row.iter_mut().enumerate() {
                        *f |= eats_at(ctx, et, eb, t, b);
                    }
                }
            }
            sum += (p * share) * (ctx.p.w_stair * (value(&after) - before));
        }
    }
    sum
}

/// swapValue: what swapping an inmate of this tier is worth, on average, with
/// `others` other open inmates (`comp` below Rainbow) that Scapegoat's
/// Guillotine may remove.
pub fn swap_value(ctx: &Ctx, tier: usize, comp: &Comp, others: i64) -> f64 {
    let (dist, loss) = replace_odds(ctx, tier, comp, others);
    dist_worth(&ctx.v, ctx.r, &dist) - ctx.v[tier] - loss
}

/// activation: the inmate's best devour or a swap, whichever is worth more,
/// and never below zero (it can just wait).
pub fn activation(ctx: &mut Ctx, tier: usize, beast: usize, comp: &mut Comp, crowd: i64, swap_left: bool) -> f64 {
    let mut value: f64 = 0.0;
    if tier < TOP {
        value = value.max(best_meal(ctx, tier, beast, comp, crowd).0);
    }
    if swap_left && (ctx.last_day || tier as f64 <= ctx.p.swap_max_tier) {
        value = value.max(swap_value(ctx, tier, comp, crowd - 1));
    }
    value
}

/// freshActivation: what a brand-new inmate (tier odds `dist`) could do today,
/// of beast `beast` when it is known (the Iron Pestle keeps the replaced
/// inmate's), else any of the 4 at even odds.
pub fn fresh_activation(ctx: &mut Ctx, dist: &[f64], beast: Option<usize>, comp: &mut Comp, crowd: i64, swap_left: bool) -> f64 {
    let (beasts, share) = beasts(beast);
    let mut sum = 0.0;
    for (t, &p) in dist.iter().enumerate() {
        if p == 0.0 {
            continue;
        }
        for b in beasts.clone() {
            sum += p * share * activation(ctx, t, b, comp, crowd, swap_left);
        }
    }
    sum
}

/// freshValue: a new inmate behind a door, its own worth plus what it could
/// do today from the warden's cell; of beast `beast` when the player has seen it.
pub fn fresh_value(ctx: &mut Ctx, dist: &[f64], beast: Option<usize>, comp: &mut Comp, crowd: i64, swap_left: bool) -> f64 {
    let worth = dist_worth(&ctx.v, ctx.r, dist);
    ctx.p.w_open * worth + ctx.p.w_now * fresh_activation(ctx, dist, beast, comp, crowd, swap_left) + stair_fresh(ctx, dist, beast, comp)
}

/// executionValue: an Execution by an inmate of this tier and beast.
pub fn execution_value(ctx: &Ctx, tier: usize, beast: usize, comp: &Comp) -> f64 {
    let prey = &data().food_chain[beast];
    let mut count = 0;
    let mut lost = 0.0;
    for t in 0..tier {
        for f in prey {
            let k = comp[t * 4 + f.index()];
            count += k;
            lost += f64::from(k) * ctx.v[t];
        }
    }
    if count == 0 {
        return 0.0;
    }
    let to = TOP.min((tier as i32 + count) as usize);
    worth(ctx, to) - ctx.v[tier] - lost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::data::{CAGE_FIGHT, Tool};
    use crate::engine::new_game;
    use crate::solver::{Rng32, TUNED, context};

    /// `stair_fresh` works from who is fed before the new inmate; it must
    /// match `stair_delta` of each tier and beast to the bit.
    #[test]
    fn stair_fresh_is_the_sum_of_its_deltas() {
        let mut roll = Rng32::new(7);
        let tools = [vec![], vec![Tool::Guillotine], vec![Tool::Maul], vec![Tool::Crown, Tool::Skewer]];
        for k in 0..400 {
            let mut st = new_game("stair");
            st.tools = tools[k % tools.len()].clone();
            st.theme = (k % 7 == 0).then_some(CAGE_FIGHT);
            let ctx = context(&st, &TUNED);
            let mut comp: Comp = [0; 24];
            for c in comp.iter_mut().take(TOP * 4) {
                *c = (roll.roll() * 3.0) as i32 - i32::from(roll.roll() < 0.1);
            }
            let dist = [0.3, 0.25, 0.2, 0.15, 0.1, 0.0];
            let beast = (k % 5 < 4).then_some(k % 5);
            let (beasts, share) = beasts(beast);
            let mut want = 0.0;
            for (t, &p) in dist.iter().enumerate() {
                for b in beasts.clone() {
                    if p != 0.0 {
                        want += (p * share) * stair_delta(&ctx, &mut comp, t, b, 1);
                    }
                }
            }
            let before = comp;
            assert_eq!(stair_fresh(&ctx, &dist, beast, &mut comp).to_bits(), want.to_bits(), "board {k}");
            assert_eq!(comp, before);
        }
    }

    /// A context takes the chain memo a context before left when nothing
    /// its values depend on differs: each best meal must match a fresh
    /// memo's to the bit, boards and tools changing in between.
    #[test]
    fn a_kept_chain_memo_gives_a_fresh_ones_values() {
        let mut roll = Rng32::new(11);
        let tools = [vec![], vec![Tool::Guillotine], vec![Tool::Muzzle], vec![Tool::Revenge]];
        let boards: Vec<(crate::engine::state::State, Comp, usize, usize)> = (0..300)
            .map(|k| {
                let mut st = new_game("memo");
                st.tools = tools[k / 50 % tools.len()].clone();
                st.theme = (k % 3 == 0).then_some(CAGE_FIGHT);
                st.day = if k % 11 == 0 { st.max_days } else { 3 };
                let mut comp: Comp = [0; 24];
                for c in comp.iter_mut().take(TOP * 4) {
                    *c = (roll.roll() * 3.0) as i32;
                }
                (st, comp, k % TOP, k % 4)
            })
            .collect();
        let meal = |(st, comp, t, b): &(crate::engine::state::State, Comp, usize, usize)| {
            let mut ctx = context(st, &TUNED);
            let mut comp = *comp;
            best_meal(&mut ctx, *t, *b, &mut comp, 12).0.to_bits()
        };
        let kept: Vec<u64> = boards.iter().map(meal).collect();
        let fresh: Vec<u64> = boards
            .iter()
            .map(|board| {
                crate::solver::forget_chain_memos();
                meal(board)
            })
            .collect();
        assert_eq!(kept, fresh);
    }
}
