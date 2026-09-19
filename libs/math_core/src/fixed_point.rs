//! Q64.64 fixed-point arithmetic.
//!
//! A Q64.64 number is a `u128` holding `value * 2^64`: the top 64 bits are the
//! integer part and the bottom 64 bits are the fraction. `1.0` is
//! [`Q64`](crate::Q64) = `2^64`.
//!
//! Every operation here reports overflow with [`MathError::Overflow`] instead
//! of wrapping or panicking, which matters because the crate is compiled with
//! `overflow-checks = true` in release builds.

use crate::u256::{self, U256};
use crate::{MathError, MathResult};

/// `floor(x / 2) + floor(y / 2) + (x & y & 1)`, i.e. `floor((x + y) / 2)`
/// without the intermediate addition overflowing.
const fn average_floor(x: u128, y: u128) -> u128 {
    (x >> 1) + (y >> 1) + ((x & y) & 1)
}

/// Multiplies two Q64.64 numbers, rounding down.
///
/// `mul_shr(2.0, 3.0) == 6.0`.
pub fn mul_shr(a: u128, b: u128) -> MathResult<u128> {
    u256::mul_shr(a, b)
}

/// Multiplies two Q64.64 numbers, rounding up.
pub fn mul_ceil(a: u128, b: u128) -> MathResult<u128> {
    u256::mul_shr_ceil(a, b)
}

/// Divides two Q64.64 numbers (`a / b`), rounding down.
pub fn div_shr(a: u128, b: u128) -> MathResult<u128> {
    if b == 0 {
        return Err(MathError::DivByZero);
    }
    u256::div_shifted(U256::from_u128(a), 64, U256::from_u128(b), false)
}

/// Divides two Q64.64 numbers (`a / b`), rounding up.
pub fn div_ceil(a: u128, b: u128) -> MathResult<u128> {
    if b == 0 {
        return Err(MathError::DivByZero);
    }
    u256::div_shifted(U256::from_u128(a), 64, U256::from_u128(b), true)
}

/// Integer square root: `floor(sqrt(x))`.
///
/// Accepts the whole `u128` range, including `u128::MAX`.
pub fn sqrt(x: u128) -> MathResult<u128> {
    if x < 2 {
        return Ok(x);
    }
    // x / 2 + 1 is strictly greater than sqrt(x) for x >= 2, and never
    // overflows (unlike the usual `(x + 1) >> 1` formulation).
    let mut z = x / 2 + 1;
    loop {
        let y = average_floor(z, x / z);
        if y >= z {
            return Ok(z);
        }
        z = y;
    }
}

/// Integer square root of the 256-bit value `hi * 2^128 + lo`.
///
/// Used to take the square root of a Q64.64 price without overflowing: for an
/// encoded price `p`, `isqrt_wide(p >> 64, p << 64)` is `floor(sqrt(p) * 2^32)`.
pub fn isqrt_wide(hi: u128, lo: u128) -> MathResult<u128> {
    let n = U256::from_u128(hi)
        .checked_shl(128)
        .ok_or(MathError::Overflow)?
        .checked_add(U256::from_u128(lo))
        .ok_or(MathError::Overflow)?;
    if n.is_zero() {
        return Ok(0);
    }
    if hi == 0 {
        return sqrt(lo);
    }
    let bits = n.bit_len();
    // Start from an upper bound for the root: 2^ceil(bits / 2) >= sqrt(n).
    let mut x = if bits >= 255 {
        u128::MAX
    } else {
        1u128 << bits.div_ceil(2)
    };
    loop {
        let (quotient, _) = n.div_rem(U256::from_u128(x)).unwrap();
        let quotient = quotient.to_u128().ok_or(MathError::Overflow)?;
        let y = average_floor(x, quotient);
        if y >= x {
            return Ok(x);
        }
        x = y;
    }
}

/// Square root of a Q64.64 value, in Q64.64: `floor(sqrt(price * 2^64))`.
///
/// For `price = 4.0` the result is `2.0` encoded, i.e. `2 * Q64`.
pub fn sqrt_price(price: u128) -> MathResult<u128> {
    isqrt_wide(price >> 64, price << 64)
}

/// Inverse of [`sqrt_price`]: `floor(sqrt_price^2 / 2^64)`, in Q64.64.
pub fn price_from_sqrt_price(sqrt_price: u128) -> MathResult<u128> {
    u256::mul_shr(sqrt_price, sqrt_price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Q64;

    /// Decimal string to Q64.64, e.g. `encode("1.5") == 1.5 * Q64`.
    fn encode(value: &str) -> u128 {
        let (int, frac) = value.split_once('.').unwrap_or((value, ""));
        let int = int.parse::<u128>().unwrap();
        let mut frac_val = 0u128;
        let mut denom = 1u128;
        for &b in frac.as_bytes() {
            if b.is_ascii_digit() {
                frac_val = frac_val * 10 + (b - b'0') as u128;
                denom *= 10;
            }
        }
        int * Q64
            + if denom > 1 {
                (frac_val * Q64) / denom
            } else {
                0
            }
    }

    #[test]
    fn mul_and_div_round_trip() {
        assert_eq!(mul_shr(Q64, Q64).unwrap(), Q64);
        assert_eq!(mul_shr(2 * Q64, 3 * Q64).unwrap(), 6 * Q64);
        assert_eq!(div_shr(6 * Q64, 3 * Q64).unwrap(), 2 * Q64);
        assert_eq!(div_shr(Q64, 3 * Q64).unwrap(), Q64 / 3);
    }

    #[test]
    fn mul_rounds_down_and_mul_ceil_rounds_up() {
        // (1 + 2^-64) * (1 + 2^-64) is not representable exactly.
        let a = Q64 + 1;
        assert_eq!(mul_shr(a, a).unwrap(), Q64 + 2);
        assert_eq!(mul_ceil(a, a).unwrap(), Q64 + 3);
        // Dividing by the integer 3 is the same as dividing by 3.0 in Q64.64.
        assert_eq!(div_shr(Q64 + 1, 3 * Q64).unwrap(), (Q64 + 1) / 3);
        assert_eq!(div_ceil(Q64 + 1, 3 * Q64).unwrap(), (Q64 + 1).div_ceil(3));
    }

    #[test]
    fn division_by_zero_is_reported() {
        assert_eq!(div_shr(1, 0), Err(MathError::DivByZero));
        assert_eq!(div_ceil(1, 0), Err(MathError::DivByZero));
    }

    #[test]
    fn sqrt_handles_the_whole_range() {
        assert_eq!(sqrt(0).unwrap(), 0);
        assert_eq!(sqrt(1).unwrap(), 1);
        assert_eq!(sqrt(2).unwrap(), 1);
        assert_eq!(sqrt(3).unwrap(), 1);
        assert_eq!(sqrt(4).unwrap(), 2);
        assert_eq!(sqrt(99).unwrap(), 9);
        assert_eq!(sqrt(100).unwrap(), 10);
        // No overflow panic on the extremes (the old implementation panicked).
        assert_eq!(sqrt(u128::MAX).unwrap(), (1u128 << 64) - 1);
        assert_eq!(sqrt(1u128 << 126).unwrap(), 1u128 << 63);
    }

    #[test]
    fn sqrt_is_the_integer_square_root() {
        let mut x = 1u128;
        while x < 1u128 << 100 {
            let s = sqrt(x).unwrap();
            assert!(s * s <= x, "s^2 > x for x={x}");
            match (s + 1).checked_mul(s + 1) {
                Some(next) => assert!(next > x, "(s+1)^2 <= x for x={x}"),
                None => {} // (s + 1)^2 overflows, so it certainly exceeds x
            }
            x = x * 3 + 1;
        }
    }

    #[test]
    fn sqrt_price_matches_the_definition() {
        // sqrt(4.0) == 2.0, sqrt(1.0) == 1.0, sqrt(0.25) == 0.5
        assert_eq!(sqrt_price(4 * Q64).unwrap(), 2 * Q64);
        assert_eq!(sqrt_price(Q64).unwrap(), Q64);
        assert_eq!(sqrt_price(Q64 / 4).unwrap(), Q64 / 2);
        assert_eq!(sqrt_price(0).unwrap(), 0);
        // sqrt of the maximum representable value stays in range
        let root = sqrt_price(u128::MAX).unwrap();
        assert_eq!(root, u128::MAX >> 32);
    }

    #[test]
    fn price_from_sqrt_price_inverts_sqrt_price() {
        for price in [Q64, 4 * Q64, 9 * Q64, Q64 / 4, 1_000 * Q64] {
            let root = sqrt_price(price).unwrap();
            let back = price_from_sqrt_price(root).unwrap();
            // Rounding the root down loses up to ~2*sqrt(price) units, which is
            // a relative error of about 2^-62.
            assert!(back <= price);
            assert!(price - back <= price / 1_000_000 + 8, "{price} -> {back}");
        }
    }

    #[test]
    fn encoded_helpers_agree_with_arithmetic() {
        let one_and_a_half = encode("1.5");
        assert_eq!(one_and_a_half, Q64 + Q64 / 2);
        assert_eq!(mul_shr(encode("0.5"), encode("0.5")).unwrap(), Q64 / 4);
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        // (2^127-ish)^2 does not fit in Q64.64.
        assert_eq!(mul_shr(u128::MAX, 2 * Q64), Err(MathError::Overflow));
        assert_eq!(div_shr(u128::MAX, Q64 / 2), Err(MathError::Overflow));
    }
}
