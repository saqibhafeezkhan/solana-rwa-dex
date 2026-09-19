//! One step of a swap: how far the price moves and what crosses the curve.
//!
//! A swap walks the price towards a target (the next initialised tick, or the
//! price limit) in steps. [`compute_swap_step`] computes a single step: it
//! either reaches the target or consumes all of the remaining input, whichever
//! comes first, and reports the input, the output and the fee.

use crate::sqrt_price_math::{
    get_amount_0_delta, get_amount_1_delta, get_next_sqrt_price_from_amount_0,
    get_next_sqrt_price_from_amount_1,
};
use crate::u256;
use crate::{MathError, MathResult};

/// Fee denominator: fees are expressed in millionths (pips).
pub const FEE_DENOMINATOR: u32 = 1_000_000;

/// Outcome of a single swap step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStepResult {
    /// Price at the end of the step; equals `sqrt_price_target` when the step
    /// was not limited by the remaining amount.
    pub sqrt_price_next: u128,
    /// Amount of the input token consumed by the step (excluding the fee).
    pub amount_in: u128,
    /// Amount of the output token paid out by the step.
    pub amount_out: u128,
    /// Input-token fee taken from this step's input; zero for exact-output
    /// swaps, where the fee is charged on the output side instead.
    pub fee_amount: u128,
}

/// Computes a single swap step from `sqrt_price_current` towards
/// `sqrt_price_target`, for a pool with `liquidity` and a fee of `fee_pips`
/// millionths of the input.
///
/// * `exact_in` - `amount_remaining` is the input still to be sold.
/// * `!exact_in` - `amount_remaining` is the output still to be bought.
///
/// The direction is implied: a target below the current price means token0 is
/// the input.
///
/// Rounding follows the rule that the pool is never on the losing side:
/// input amounts round up, output amounts round down, and the fee rounds up.
///
/// Returns [`MathError::InvalidFee`] when `fee_pips >= FEE_DENOMINATOR`.
pub fn compute_swap_step(
    sqrt_price_current: u128,
    sqrt_price_target: u128,
    liquidity: u128,
    amount_remaining: u128,
    fee_pips: u32,
    exact_in: bool,
) -> MathResult<SwapStepResult> {
    if fee_pips >= FEE_DENOMINATOR {
        return Err(MathError::InvalidFee);
    }
    if sqrt_price_current == sqrt_price_target || liquidity == 0 || amount_remaining == 0 {
        return Ok(SwapStepResult {
            sqrt_price_next: sqrt_price_current,
            amount_in: 0,
            amount_out: 0,
            fee_amount: 0,
        });
    }
    let zero_for_one = sqrt_price_current > sqrt_price_target;
    let fee_denominator = FEE_DENOMINATOR as u128;

    let sqrt_price_next = if exact_in {
        // The input available to move the price excludes the fee.
        let amount_remaining_less_fee = u256::mul_div_floor(
            amount_remaining,
            fee_denominator - fee_pips as u128,
            fee_denominator,
        )?;
        // Input needed to reach the target in full.
        let amount_in_to_target = if zero_for_one {
            get_amount_0_delta(sqrt_price_target, sqrt_price_current, liquidity, true)?
        } else {
            get_amount_1_delta(sqrt_price_current, sqrt_price_target, liquidity, true)?
        };
        if amount_in_to_target <= amount_remaining_less_fee {
            sqrt_price_target
        } else if zero_for_one {
            get_next_sqrt_price_from_amount_0(
                amount_remaining_less_fee,
                sqrt_price_current,
                liquidity,
                true,
            )?
        } else {
            get_next_sqrt_price_from_amount_1(
                amount_remaining_less_fee,
                sqrt_price_current,
                liquidity,
                true,
            )?
        }
    } else {
        // Output available at the target in full.
        let amount_out_to_target = if zero_for_one {
            get_amount_0_delta(sqrt_price_target, sqrt_price_current, liquidity, false)?
        } else {
            get_amount_1_delta(sqrt_price_current, sqrt_price_target, liquidity, false)?
        };
        if amount_out_to_target <= amount_remaining {
            sqrt_price_target
        } else if zero_for_one {
            get_next_sqrt_price_from_amount_1(
                amount_remaining,
                sqrt_price_current,
                liquidity,
                false,
            )?
        } else {
            get_next_sqrt_price_from_amount_0(
                amount_remaining,
                sqrt_price_current,
                liquidity,
                false,
            )?
        }
    };

    // Amounts actually exchanged over the realised price move.
    let (amount_in, amount_out) = if zero_for_one {
        (
            get_amount_0_delta(sqrt_price_next, sqrt_price_current, liquidity, true)?,
            get_amount_1_delta(sqrt_price_next, sqrt_price_current, liquidity, false)?,
        )
    } else {
        (
            get_amount_1_delta(sqrt_price_current, sqrt_price_next, liquidity, true)?,
            get_amount_0_delta(sqrt_price_current, sqrt_price_next, liquidity, false)?,
        )
    };

    // For exact input the fee is charged on the amount offered, not on the
    // amount consumed, and it rounds up.
    let fee_amount = if exact_in {
        u256::mul_div_ceil(amount_remaining, fee_pips as u128, fee_denominator)?
    } else {
        0
    };

    Ok(SwapStepResult {
        sqrt_price_next,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_math::sqrt_price_at_tick;
    use crate::Q64;

    const L: u128 = 1_000_000_000;
    const FEE_30BPS: u32 = 3_000;

    fn price(tick: i32) -> u128 {
        sqrt_price_at_tick(tick).unwrap()
    }

    #[test]
    fn rejects_an_out_of_range_fee() {
        let p = price(0);
        let target = price(100);
        assert_eq!(
            compute_swap_step(p, target, L, 1_000, FEE_DENOMINATOR, true),
            Err(MathError::InvalidFee)
        );
        assert_eq!(
            compute_swap_step(p, target, L, 1_000, FEE_DENOMINATOR + 1, false),
            Err(MathError::InvalidFee)
        );
    }

    #[test]
    fn no_op_steps_return_zeros() {
        let p = price(0);
        for (current, target, liquidity, amount) in [
            (p, p, L, 1_000u128),
            (p, price(10), 0, 1_000),
            (p, price(10), L, 0),
        ] {
            let step =
                compute_swap_step(current, target, liquidity, amount, FEE_30BPS, true).unwrap();
            assert_eq!(step.sqrt_price_next, current);
            assert_eq!(
                (step.amount_in, step.amount_out, step.fee_amount),
                (0, 0, 0)
            );
        }
    }

    #[test]
    fn reaches_the_target_when_the_input_is_large_enough() {
        let current = price(0);
        let target = price(10);
        let step = compute_swap_step(current, target, L, u128::MAX / 4, FEE_30BPS, true).unwrap();
        assert_eq!(step.sqrt_price_next, target);
        assert!(step.amount_in > 0 && step.amount_out > 0);
    }

    #[test]
    fn partial_step_stops_short_of_the_target() {
        let current = price(0);
        let target = price(1_000);
        let step = compute_swap_step(current, target, L, 1_000_000, FEE_30BPS, true).unwrap();
        // Stopped strictly between the two prices, on the way to the target.
        assert!(step.sqrt_price_next > current && step.sqrt_price_next < target);
        // The step consumed at most the input net of fees, never more.
        let net = 1_000_000 - (1_000_000 * FEE_30BPS as u128 / FEE_DENOMINATOR as u128);
        assert!(step.amount_in <= net, "{} > {net}", step.amount_in);
        assert_eq!(step.fee_amount, 3_000);
    }

    #[test]
    fn fee_is_rounded_up_and_zero_fee_is_free() {
        let current = price(0);
        let target = price(100);
        // 3 pips on 1 unit rounds up to 1.
        let step = compute_swap_step(current, target, L, 1, 3, true).unwrap();
        assert_eq!(step.fee_amount, 1);
        let step = compute_swap_step(current, target, L, 1, 0, true).unwrap();
        assert_eq!(step.fee_amount, 0);
    }

    #[test]
    fn exact_out_has_no_fee_field_and_respects_the_limit() {
        let current = price(0);
        let target = price(1_000);
        let step = compute_swap_step(current, target, L, 500_000, FEE_30BPS, false).unwrap();
        assert!(step.amount_out <= 500_000, "{}", step.amount_out);
        assert_eq!(step.fee_amount, 0);
        // A generous output limit reaches the target.
        let step = compute_swap_step(current, target, L, u128::MAX / 4, FEE_30BPS, false).unwrap();
        assert_eq!(step.sqrt_price_next, target);
    }

    #[test]
    fn direction_follows_the_target() {
        let current = price(0);
        let step = compute_swap_step(current, price(-100), L, 1_000_000, FEE_30BPS, true).unwrap();
        assert!(step.sqrt_price_next < current);
        let step = compute_swap_step(current, price(100), L, 1_000_000, FEE_30BPS, true).unwrap();
        assert!(step.sqrt_price_next > current);
    }

    #[test]
    fn a_sequence_of_steps_consumes_the_input_and_stops_at_the_last_target() {
        // Swap a fixed input across two ticks: the first step fills the first
        // range, the second consumes whatever is left.
        let start = price(0);
        let first_target = price(-1);
        let second_target = price(-2);
        let amount_in = 10_000_000u128;

        let step_1 = compute_swap_step(start, first_target, L, amount_in, FEE_30BPS, true).unwrap();
        assert_eq!(step_1.sqrt_price_next, first_target);
        let consumed = step_1.amount_in + step_1.fee_amount;
        assert!(consumed < amount_in);

        let step_2 = compute_swap_step(
            step_1.sqrt_price_next,
            second_target,
            L,
            amount_in - consumed,
            FEE_30BPS,
            true,
        )
        .unwrap();
        assert!(
            step_2.amount_in + step_2.fee_amount <= amount_in - consumed,
            "second step overspent"
        );
        assert!(step_2.sqrt_price_next >= second_target);
        assert!(step_2.amount_out > 0);
        // Price fell monotonically.
        assert!(step_2.sqrt_price_next < step_1.sqrt_price_next);
    }

    #[test]
    fn exact_in_and_exact_out_agree_in_the_limit() {
        // Buying `out` units for exactly the input that produces them: with the
        // same liquidity the two paths land close to each other.
        let current = price(0);
        let target = price(-1_000);
        let exact_in = compute_swap_step(current, target, L, 1_000_000, 0, true).unwrap();
        let exact_out =
            compute_swap_step(current, target, L, exact_in.amount_out, 0, false).unwrap();
        // Buying exactly what the exact-input swap sells lands on the same
        // price, up to the rounding of one raw amount unit.
        let tolerance = (Q64 / L) + 8;
        assert!(
            exact_in.sqrt_price_next.abs_diff(exact_out.sqrt_price_next) <= tolerance,
            "{:?} vs {:?}",
            exact_in.sqrt_price_next,
            exact_out.sqrt_price_next
        );
    }

    #[test]
    fn fee_reduces_the_price_move() {
        let current = price(0);
        let target = price(-1_000);
        let with_fee = compute_swap_step(current, target, L, 1_000_000, FEE_30BPS, true).unwrap();
        let without_fee = compute_swap_step(current, target, L, 1_000_000, 0, true).unwrap();
        // Same input, lower net input, so the price moves less.
        assert!(with_fee.sqrt_price_next > without_fee.sqrt_price_next);
    }
}
