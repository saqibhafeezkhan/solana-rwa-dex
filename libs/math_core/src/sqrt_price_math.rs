//! Amount and price math for a single liquidity range or a single swap step.
//!
//! Volume 1 of the model is Uniswap v3's. The price is `token1 / token0`, so
//! paying token0 *in* pushes the price down and paying token1 in pushes it up,
//! and a position holding liquidity `L` over `[sqrt_lower, sqrt_upper]` holds
//!
//! ```text
//! amount0 = L * (1 / sqrt_lower - 1 / sqrt_upper)
//! amount1 = L * (sqrt_upper - sqrt_lower)
//! ```
//!
//! All sqrt prices are Q64.64 and all token amounts are raw integers. Rounding
//! is explicit everywhere and is not cosmetic: amounts owed *to* the pool round
//! up and amounts paid *out* round down, so rounding can never be farmed.

use crate::u256::{self, U256};
use crate::{MathError, MathResult, Q64};

/// Returns `(lower, upper)`. Every range formula is symmetric in its endpoints.
fn sorted(a: u128, b: u128) -> (u128, u128) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Token0 held by a position with `liquidity` over
/// `[sqrt_price_a, sqrt_price_b]`, rounded up when `round_up` is set.
///
/// Numerator and denominator are kept at full width, so the result is a true
/// floor (or ceiling) of `L * (1/lower - 1/upper)` with a single rounding step.
pub fn get_amount_0_delta(
    sqrt_price_a: u128,
    sqrt_price_b: u128,
    liquidity: u128,
    round_up: bool,
) -> MathResult<u128> {
    let (lower, upper) = sorted(sqrt_price_a, sqrt_price_b);
    if lower == 0 {
        return Err(MathError::SqrtPriceOutOfBounds);
    }
    // amount0 = L * 2^64 * (upper - lower) / (lower * upper)
    let numerator = U256::mul_u128(liquidity, upper - lower);
    let denominator = U256::mul_u128(lower, upper);
    u256::div_shifted(numerator, 64, denominator, round_up)
}

/// Token1 held by a position with `liquidity` over
/// `[sqrt_price_a, sqrt_price_b]`, rounded up when `round_up` is set.
pub fn get_amount_1_delta(
    sqrt_price_a: u128,
    sqrt_price_b: u128,
    liquidity: u128,
    round_up: bool,
) -> MathResult<u128> {
    let (lower, upper) = sorted(sqrt_price_a, sqrt_price_b);
    let difference = upper - lower;
    if round_up {
        u256::mul_shr_ceil(liquidity, difference)
    } else {
        u256::mul_shr(liquidity, difference)
    }
}

/// The sqrt price after `amount` token0 is paid into the pool (`add = true`) or
/// taken out of it (`add = false`):
///
/// ```text
/// add:    S' = L * S * 2^64 / (L * 2^64 + amount * S)   (price moves down)
/// remove: S' = L * S * 2^64 / (L * 2^64 - amount * S)   (price moves up)
/// ```
///
/// The new price is rounded **up**, the direction that never lets a caller
/// withdraw more than the pool owns.
///
/// Returns [`MathError::SqrtPriceOutOfBounds`] when removing the amount would
/// require an infinite price, and [`MathError::DivByZero`] when `liquidity` is
/// zero.
///
/// # Condition number
///
/// This formula inverts a difference of reciprocals, so one unit of `amount`
/// shifts the result by a relative `S' / (liquidity * 2^64)`. That is
/// negligible while the price is small compared to the liquidity - true for
/// every price a pool of that liquidity can actually support - but a caller
/// that asks to remove *all* of a pool's token0 at once is asking for a
/// division by something indistinguishable from zero and gets whatever the last
/// digit of the amount dictates.
///
/// [`compute_swap_step`](crate::compute_swap_step) never enters that regime: it
/// only moves the price as far as the target, so the amount it passes here is
/// bounded by the amount the target itself holds.
pub fn get_next_sqrt_price_from_amount_0(
    amount: u128,
    sqrt_price: u128,
    liquidity: u128,
    add: bool,
) -> MathResult<u128> {
    if liquidity == 0 {
        return Err(MathError::DivByZero);
    }
    if amount == 0 {
        return Ok(sqrt_price);
    }
    let numerator = U256::mul_u128(liquidity, sqrt_price);
    let scaled_liquidity = U256::mul_u128(liquidity, Q64);
    let delta = U256::mul_u128(amount, sqrt_price);
    let denominator = if add {
        scaled_liquidity
            .checked_add(delta)
            .ok_or(MathError::Overflow)?
    } else {
        match scaled_liquidity.checked_sub(delta) {
            Some(value) if !value.is_zero() => value,
            // The price would have to become infinite.
            _ => return Err(MathError::SqrtPriceOutOfBounds),
        }
    };
    u256::div_shifted(numerator, 64, denominator, true)
}

/// The sqrt price after `amount` token1 is paid into the pool (`add = true`) or
/// taken out of it (`add = false`):
///
/// ```text
/// add:    S' = S + floor(amount * 2^64 / L)   (price moves up)
/// remove: S' = S - ceil(amount * 2^64 / L)    (price moves down)
/// ```
///
/// The new price is rounded **down**, the direction that keeps payouts
/// conservative.
pub fn get_next_sqrt_price_from_amount_1(
    amount: u128,
    sqrt_price: u128,
    liquidity: u128,
    add: bool,
) -> MathResult<u128> {
    if liquidity == 0 {
        return Err(MathError::DivByZero);
    }
    if amount == 0 {
        return Ok(sqrt_price);
    }
    let amount = U256::from_u128(amount);
    let liquidity = U256::from_u128(liquidity);
    if add {
        // Round the increment down: the resulting price stays a lower bound.
        let increment = u256::div_shifted(amount, 64, liquidity, false)?;
        sqrt_price.checked_add(increment).ok_or(MathError::Overflow)
    } else {
        // Round the deduction up: the resulting price stays a lower bound.
        let deduction = u256::div_shifted(amount, 64, liquidity, true)?;
        sqrt_price.checked_sub(deduction).ok_or(MathError::Overflow)
    }
}

/// Spot conversion of `amount_1` into token0 at `sqrt_price`, rounded down.
///
/// This is a price conversion, not a range calculation: use
/// [`get_amount_0_delta`] whenever a position's actual token amounts are needed.
pub fn get_amount_0_for_amount_1(amount_1: u128, sqrt_price: u128) -> MathResult<u128> {
    if sqrt_price == 0 {
        return Err(MathError::DivByZero);
    }
    // amount0 = amount1 / price with price = sqrt_price^2 / 2^128.
    let scaled = u256::div_shifted(
        U256::from_u128(amount_1),
        64,
        U256::from_u128(sqrt_price),
        false,
    )?;
    u256::div_shifted(
        U256::from_u128(scaled),
        64,
        U256::from_u128(sqrt_price),
        false,
    )
}

/// Spot conversion of `amount_0` into token1 at `sqrt_price`, rounded down.
pub fn get_amount_1_for_amount_0(amount_0: u128, sqrt_price: u128) -> MathResult<u128> {
    let scaled = u256::mul_shr(amount_0, sqrt_price)?;
    u256::mul_shr(scaled, sqrt_price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_math::sqrt_price_at_tick;
    use crate::{MAX_TICK, MIN_TICK, Q64};

    /// Large enough that a one-tick range still produces token amounts.
    const L: u128 = 1_000_000_000;

    fn price(tick: i32) -> u128 {
        sqrt_price_at_tick(tick).unwrap()
    }

    #[test]
    fn zero_width_range_has_no_amounts() {
        let p = price(1_000);
        for round_up in [false, true] {
            assert_eq!(get_amount_0_delta(p, p, L, round_up).unwrap(), 0);
            assert_eq!(get_amount_1_delta(p, p, L, round_up).unwrap(), 0);
        }
    }

    #[test]
    fn single_tick_range_amounts_are_sane() {
        let lower = price(0);
        let upper = price(1);
        // amount1 is exactly L * (upper - lower) / 2^64.
        assert_eq!(
            get_amount_1_delta(lower, upper, L, false).unwrap(),
            L * (upper - lower) / Q64
        );
        // One tick at 1.0 is ~5e-5 in both directions, so the two amounts agree
        // to within the single rounding unit.
        let amount_0 = get_amount_0_delta(lower, upper, L, false).unwrap();
        let amount_1 = get_amount_1_delta(lower, upper, L, false).unwrap();
        assert!(amount_0 > 0 && amount_1 > 0);
        // Over one tick at price 1 the two sides differ only by the tick's own
        // 5e-5 asymmetry, well inside a tenth of a percent.
        assert!(
            amount_0.abs_diff(amount_1) * 1_000 <= amount_1,
            "{amount_0} vs {amount_1}"
        );
    }

    #[test]
    fn range_amounts_are_symmetric_in_the_endpoints() {
        let a = price(-500);
        let b = price(2_500);
        for round_up in [false, true] {
            assert_eq!(
                get_amount_0_delta(a, b, L, round_up).unwrap(),
                get_amount_0_delta(b, a, L, round_up).unwrap()
            );
            assert_eq!(
                get_amount_1_delta(a, b, L, round_up).unwrap(),
                get_amount_1_delta(b, a, L, round_up).unwrap()
            );
        }
    }

    #[test]
    fn rounding_up_is_never_smaller_than_rounding_down() {
        for (a, b) in [
            (-60_000, 60_000),
            (0, 1),
            (-443_636, 443_636),
            (400_000, 400_010),
            (-443_636, 443_636 - 1),
        ] {
            let (lower, upper) = (price(a), price(b));
            let down = get_amount_0_delta(lower, upper, L, false).unwrap();
            let up = get_amount_0_delta(lower, upper, L, true).unwrap();
            assert!(up >= down && up - down <= 1, "amount0 a={a} b={b}");
            let down = get_amount_1_delta(lower, upper, L, false).unwrap();
            let up = get_amount_1_delta(lower, upper, L, true).unwrap();
            assert!(up >= down && up - down <= 1, "amount1 a={a} b={b}");
        }
    }

    #[test]
    fn amounts_scale_with_liquidity() {
        let (lower, upper) = (price(-1_000), price(1_000));
        let base = get_amount_0_delta(lower, upper, L, false).unwrap();
        let doubled = get_amount_0_delta(lower, upper, L * 2, false).unwrap();
        assert!(doubled.abs_diff(base * 2) <= 2);
        assert_eq!(get_amount_0_delta(lower, upper, 0, true).unwrap(), 0);
        assert_eq!(get_amount_1_delta(lower, upper, 0, true).unwrap(), 0);
    }

    #[test]
    fn degenerate_ranges_are_rejected() {
        assert_eq!(
            get_amount_0_delta(0, price(1), L, false),
            Err(MathError::SqrtPriceOutOfBounds)
        );
    }

    /// The amount and price families are independent implementations of the same
    /// model, so they must agree.
    ///
    /// The check is a bracket rather than a tolerance, which makes it exact: the
    /// floored amount must not carry the price past the endpoint and the ceiled
    /// amount must at least reach it. Integer amounts, monotonicity of the price
    /// in the amount, and nothing else are needed for that to hold.
    ///
    /// The spans stay within a few thousand ticks because the token0 legs are
    /// ill-conditioned for enormous price ratios (see the note on
    /// [`get_next_sqrt_price_from_amount_0`]).
    #[test]
    fn amounts_and_prices_bracket_the_endpoint() {
        for (a, b) in [
            (0, 1),
            (0, 1_000),
            (-1_000, 1_000),
            (100, 200),
            (-50_000, 50_000),
            (-10_000, -9_000),
            (10_000, 11_000),
        ] {
            let (lower, upper) = (price(a), price(b));
            let from_below =
                |amount, add| get_next_sqrt_price_from_amount_0(amount, lower, L, add).unwrap();

            // Token0 out raises the price from lower towards upper.
            let out_floor = get_amount_0_delta(lower, upper, L, false).unwrap();
            let out_ceil = get_amount_0_delta(lower, upper, L, true).unwrap();
            assert!(
                from_below(out_floor, false) <= upper,
                "token0 out floor a={a} b={b}"
            );
            assert!(
                from_below(out_ceil, false) >= upper,
                "token0 out ceil a={a} b={b}"
            );

            // Token0 in lowers the price from upper back towards lower.
            let in_floor = get_next_sqrt_price_from_amount_0(out_floor, upper, L, true).unwrap();
            let in_ceil = get_next_sqrt_price_from_amount_0(out_ceil, upper, L, true).unwrap();
            assert!(in_ceil <= lower, "token0 in ceil a={a} b={b}");
            assert!(in_floor >= lower, "token0 in floor a={a} b={b}");

            // Token1 in raises the price from lower towards upper; the increment
            // is floored, so the ceiled amount reaches the endpoint within one
            // infinitesimal of the amount.
            let in_floor = get_amount_1_delta(lower, upper, L, false).unwrap();
            let in_ceil = get_amount_1_delta(lower, upper, L, true).unwrap();
            let reached_floor =
                get_next_sqrt_price_from_amount_1(in_floor, lower, L, true).unwrap();
            let reached_ceil = get_next_sqrt_price_from_amount_1(in_ceil, lower, L, true).unwrap();
            assert!(reached_floor <= upper, "token1 in floor a={a} b={b}");
            assert!(reached_ceil + 1 >= upper, "token1 in ceil a={a} b={b}");

            // Token1 out lowers the price from upper back towards lower.
            let back_floor = get_next_sqrt_price_from_amount_1(in_floor, upper, L, false).unwrap();
            let back_ceil = get_next_sqrt_price_from_amount_1(in_ceil, upper, L, false).unwrap();
            assert!(back_floor + 1 >= lower, "token1 out floor a={a} b={b}");
            assert!(back_ceil <= lower, "token1 out ceil a={a} b={b}");
        }
    }

    /// Token1 moves are linear in the price, so they stay exact across the whole
    /// tick range, where token0 moves cannot.
    #[test]
    fn token1_price_moves_span_the_whole_tick_range() {
        for (a, b) in [
            (MIN_TICK, MAX_TICK),
            (MIN_TICK, 0),
            (0, MAX_TICK),
            (-100_000, 100_000),
        ] {
            let (lower, upper) = (price(a), price(b));
            let amount = get_amount_1_delta(lower, upper, L, true).unwrap();
            let reached = get_next_sqrt_price_from_amount_1(amount, lower, L, true).unwrap();
            assert!(
                reached >= upper,
                "a={a} b={b} reached={reached} upper={upper}"
            );
            assert!(
                reached.saturating_sub(upper) <= Q64 / L + 2,
                "a={a} b={b} overshoot"
            );
        }
    }

    /// Documents the condition number called out on
    /// [`get_next_sqrt_price_from_amount_0`].
    #[test]
    fn token0_price_moves_are_ill_conditioned_at_extreme_price_ratios() {
        let (lower, upper) = (price(MIN_TICK), price(MAX_TICK));
        // The amount that spans the whole tick range is within one part in 5e19
        // of the amount that would empty the pool's token0 and send the price to
        // infinity, so a single unit of rounding in the amount moves the price by
        // a factor of ~2.7. This is the formula's condition number, not a defect:
        // the returned price is the exact result for the amount that was passed.
        let amount = get_amount_0_delta(lower, upper, L, true).unwrap();
        let reached = get_next_sqrt_price_from_amount_0(amount, lower, L, false).unwrap();
        assert!(reached > upper);
        // With liquidity scaled to the price level the same move is exact to
        // within a rounding unit, which is the regime swap math stays in.
        let realistic_liquidity = upper / (1 << 40);
        let amount = get_amount_0_delta(lower, upper, realistic_liquidity, true).unwrap();
        let reached =
            get_next_sqrt_price_from_amount_0(amount, lower, realistic_liquidity, false).unwrap();
        assert!(reached.abs_diff(upper) <= upper / 1_000_000);
    }

    #[test]
    fn price_moves_in_the_expected_direction() {
        let p = price(0);
        // Token1 in raises the price, token1 out lowers it.
        assert!(get_next_sqrt_price_from_amount_1(1_000, p, L, true).unwrap() > p);
        assert!(get_next_sqrt_price_from_amount_1(1_000, p, L, false).unwrap() < p);
        // Token0 in lowers the price, token0 out raises it.
        assert!(get_next_sqrt_price_from_amount_0(1_000, p, L, true).unwrap() < p);
        assert!(get_next_sqrt_price_from_amount_0(1_000, p, L, false).unwrap() > p);
    }

    #[test]
    fn removing_more_token0_than_the_pool_holds_is_an_error() {
        let p = price(0);
        assert_eq!(
            get_next_sqrt_price_from_amount_0(u128::MAX / 2, p, L, false),
            Err(MathError::SqrtPriceOutOfBounds)
        );
    }

    #[test]
    fn zero_amounts_leave_the_price_alone() {
        let p = price(-12_345);
        for add in [false, true] {
            assert_eq!(get_next_sqrt_price_from_amount_0(0, p, L, add).unwrap(), p);
            assert_eq!(get_next_sqrt_price_from_amount_1(0, p, L, add).unwrap(), p);
        }
    }

    #[test]
    fn zero_liquidity_is_an_error_for_price_moves() {
        let p = price(0);
        assert_eq!(
            get_next_sqrt_price_from_amount_0(1, p, 0, true),
            Err(MathError::DivByZero)
        );
        assert_eq!(
            get_next_sqrt_price_from_amount_1(1, p, 0, true),
            Err(MathError::DivByZero)
        );
    }

    #[test]
    fn spot_conversions_are_inverse() {
        let p = price(600);
        let amount_0 = 1_000u128;
        let amount_1 = get_amount_1_for_amount_0(amount_0, p).unwrap();
        let back = get_amount_0_for_amount_1(amount_1, p).unwrap();
        assert!(
            back <= amount_0 && amount_0 - back <= 2,
            "{back} vs {amount_0}"
        );
        assert_eq!(get_amount_0_for_amount_1(1, 0), Err(MathError::DivByZero));
    }

    #[test]
    fn spot_conversion_is_consistent_with_the_range_formula() {
        // For a tiny range around a price, the spot conversion approximates the
        // range formula.
        let p = price(1_000);
        let next = price(1_001);
        let range = get_amount_1_delta(p, next, L, false).unwrap();
        let spot =
            get_amount_1_for_amount_0(get_amount_0_delta(p, next, L, false).unwrap(), p).unwrap();
        // Not equal (the range formula is not a spot conversion), but the same
        // order of magnitude and within 1% of each other over one tick.
        assert!(range.abs_diff(spot) * 100 <= range, "{range} vs {spot}");
    }
}
