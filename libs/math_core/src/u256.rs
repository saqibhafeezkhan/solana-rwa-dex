//! 256-bit primitives.
//!
//! AMM math needs three things that do not fit in `u128`:
//!
//! 1. the exact 256-bit product of two `u128`s ([`U256::mul_u128`]),
//! 2. exact wide division, i.e. `(a * b) / c` where the intermediate product
//!    is 256 bits ([`mul_div_floor`], [`mul_div_ceil`]),
//! 3. `floor(num * 2^shift / den)` where both `num` and `den` are wide
//!    ([`div_shifted`]).
//!
//! [`U256`] is deliberately *not* a general-purpose big integer: it implements
//! exactly the operations above and nothing else. The division is Knuth's
//! Algorithm D (TAOCP vol. 2, 4.3.1) over 64-bit limbs, with a single-limb
//! fast path.

use core::cmp::Ordering;

use crate::{MathError, MathResult};

/// A 256-bit unsigned integer stored as four little-endian 64-bit limbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct U256([u64; 4]);

impl U256 {
    /// The additive identity.
    pub(crate) const ZERO: Self = Self([0; 4]);
    /// The multiplicative identity.
    pub(crate) const ONE: Self = Self([1, 0, 0, 0]);

    /// Zero-extends a 128-bit value.
    pub(crate) const fn from_u128(x: u128) -> Self {
        Self([x as u64, (x >> 64) as u64, 0, 0])
    }

    /// Returns `None` if the value does not fit in 128 bits.
    pub(crate) const fn to_u128(self) -> Option<u128> {
        if self.0[2] == 0 && self.0[3] == 0 {
            Some((self.0[0] as u128) | ((self.0[1] as u128) << 64))
        } else {
            None
        }
    }

    /// True when every limb is zero.
    pub(crate) const fn is_zero(self) -> bool {
        self.0[0] == 0 && self.0[1] == 0 && self.0[2] == 0 && self.0[3] == 0
    }

    /// Number of non-zero significant limbs (0 for zero, 4 for `u128::MAX + 1`).
    pub(crate) fn significant_limbs(self) -> usize {
        let mut n = 4;
        while n > 0 && self.0[n - 1] == 0 {
            n -= 1;
        }
        n
    }

    /// Number of significant bits (0 for zero).
    pub(crate) fn bit_len(self) -> u32 {
        let n = self.significant_limbs();
        if n == 0 {
            0
        } else {
            (n as u32) * 64 - self.0[n - 1].leading_zeros()
        }
    }

    /// Whether the bit at `index` is set. `index` must be below 256.
    #[cfg(test)]
    pub(crate) fn bit(self, index: u32) -> bool {
        debug_assert!(index < 256);
        self.0[(index / 64) as usize] & (1u64 << (index % 64)) != 0
    }

    /// Sets the bit at `index` in place. `index` must be below 256.
    pub(crate) fn set_bit(&mut self, index: u32) {
        debug_assert!(index < 256);
        self.0[(index / 64) as usize] |= 1u64 << (index % 64);
    }

    /// Checked addition.
    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        let mut out = [0u64; 4];
        let mut carry = 0u64;
        for i in 0..4 {
            let sum = (self.0[i] as u128) + (other.0[i] as u128) + (carry as u128);
            out[i] = sum as u64;
            carry = (sum >> 64) as u64;
        }
        if carry == 0 {
            Some(Self(out))
        } else {
            None
        }
    }

    /// Checked subtraction; `None` on underflow.
    pub(crate) fn checked_sub(self, other: Self) -> Option<Self> {
        let mut out = [0u64; 4];
        let mut borrow = 0u64;
        for i in 0..4 {
            let (diff, b1) = self.0[i].overflowing_sub(other.0[i]);
            let (diff, b2) = diff.overflowing_sub(borrow);
            out[i] = diff;
            borrow = u64::from(b1 || b2);
        }
        if borrow == 0 {
            Some(Self(out))
        } else {
            None
        }
    }

    /// Left shift that fails rather than dropping significant bits.
    pub(crate) fn checked_shl(self, shift: u32) -> Option<Self> {
        if shift == 0 {
            return Some(self);
        }
        if self.is_zero() {
            return Some(Self::ZERO);
        }
        if shift >= 256 || self.bit_len() + shift > 256 {
            return None;
        }
        let limb_shift = (shift / 64) as usize;
        let bit_shift = shift % 64;
        let mut out = [0u64; 4];
        if bit_shift == 0 {
            for i in (0..4 - limb_shift).rev() {
                out[i + limb_shift] = self.0[i];
            }
        } else {
            let mut carry = 0u64;
            for i in 0..4 - limb_shift {
                let value = ((self.0[i] as u128) << bit_shift) | (carry as u128);
                out[i + limb_shift] = value as u64;
                carry = (value >> 64) as u64;
            }
        }
        Some(Self(out))
    }

    /// Right shift, discarding shifted-out bits. Shifts of 256 or more give zero.
    pub(crate) fn shr(self, shift: u32) -> Self {
        if shift >= 256 {
            return Self::ZERO;
        }
        let limb_shift = (shift / 64) as usize;
        let bit_shift = shift % 64;
        let mut out = [0u64; 4];
        for i in 0..4 - limb_shift {
            let mut value = self.0[i + limb_shift] >> bit_shift;
            if bit_shift > 0 && i + limb_shift + 1 < 4 {
                value |= self.0[i + limb_shift + 1] << (64 - bit_shift);
            }
            out[i] = value;
        }
        Self(out)
    }

    /// Exact 256-bit product of two 128-bit values.
    pub(crate) fn mul_u128(a: u128, b: u128) -> Self {
        let (a_lo, a_hi) = (a as u64 as u128, a >> 64);
        let (b_lo, b_hi) = (b as u64 as u128, b >> 64);
        // a * b = a_lo*b_lo + (a_lo*b_hi + a_hi*b_lo) * 2^64 + a_hi*b_hi * 2^128
        let p0 = a_lo * b_lo;
        let p1 = a_lo * b_hi;
        let p2 = a_hi * b_lo;
        let p3 = a_hi * b_hi;
        let mut out = [p0 as u64, (p0 >> 64) as u64, 0, 0];
        Self::add_shifted_128(&mut out, p1, 1);
        Self::add_shifted_128(&mut out, p2, 1);
        Self::add_shifted_128(&mut out, p3, 2);
        Self(out)
    }

    /// `limbs += value * 2^(64 * shift)`, keeping only 256 bits.
    ///
    /// The caller guarantees the total sum stays below `2^256` (true for the
    /// decomposition used by [`U256::mul_u128`]).
    fn add_shifted_128(limbs: &mut [u64; 4], value: u128, shift: usize) {
        let mut carry = 0u64;
        let mut remaining = value;
        let mut i = shift;
        while i < 4 && (remaining != 0 || carry != 0) {
            let sum = (limbs[i] as u128) + (remaining as u64 as u128) + (carry as u128);
            limbs[i] = sum as u64;
            carry = (sum >> 64) as u64;
            remaining >>= 64;
            i += 1;
        }
        debug_assert!(remaining == 0 && carry == 0);
    }

    /// Exact division. Returns `None` only when `d` is zero.
    pub(crate) fn div_rem(self, d: Self) -> Option<(Self, Self)> {
        if d.is_zero() {
            return None;
        }
        let m = self.significant_limbs();
        let n = d.significant_limbs();
        if m < n {
            return Some((Self::ZERO, self));
        }
        if n == 1 {
            return Some(self.div_rem_limb(d.0[0]));
        }
        Some(self.div_rem_knuth(d, n))
    }

    /// Division by a single 64-bit limb.
    fn div_rem_limb(self, d: u64) -> (Self, Self) {
        let d = d as u128;
        let mut quotient = [0u64; 4];
        let mut rem = 0u128;
        for i in (0..4).rev() {
            let cur = (rem << 64) | (self.0[i] as u128);
            quotient[i] = (cur / d) as u64;
            rem = cur % d;
        }
        (Self(quotient), Self([rem as u64, 0, 0, 0]))
    }

    /// Knuth's Algorithm D for a divisor of at least two limbs.
    ///
    /// `n = d.significant_limbs()` must be at least 2 and no greater than
    /// `self.significant_limbs()`; the extra dividend limb needed for
    /// normalization is held in the scratch buffer below.
    fn div_rem_knuth(self, d: Self, n: usize) -> (Self, Self) {
        let m = self.significant_limbs();
        // D1: normalize so the divisor's top limb has its high bit set.
        let shift = d.0[n - 1].leading_zeros();
        let mut v = [0u64; 4];
        v[..n].copy_from_slice(&d.0[..n]);
        if shift > 0 {
            let mut carry = 0u64;
            for i in 0..n {
                let value = ((v[i] as u128) << shift) | (carry as u128);
                v[i] = value as u64;
                carry = (value >> 64) as u64;
            }
            debug_assert_eq!(carry, 0);
        }

        // Scratch dividend, one limb wider than the 4-limb input.
        let mut u = [0u64; 5];
        u[..4].copy_from_slice(&self.0);
        u[4] = 0;
        if shift > 0 {
            let mut carry = 0u64;
            for limb in u.iter_mut() {
                let value = ((*limb as u128) << shift) | (carry as u128);
                *limb = value as u64;
                carry = (value >> 64) as u64;
            }
            debug_assert_eq!(carry, 0);
        }

        let mut q = [0u64; 4];
        let v_top = v[n - 1] as u128;
        let v_next = v[n - 2] as u128;
        // D2..D7: one iteration per quotient limb.
        for j in (0..=m - n).rev() {
            // D3: estimate the quotient limb.
            let num = ((u[j + n] as u128) << 64) | (u[j + n - 1] as u128);
            let mut qhat = num / v_top;
            let mut rhat = num % v_top;
            if qhat > u64::MAX as u128 {
                qhat = u64::MAX as u128;
                rhat = num - qhat * v_top;
            }
            while rhat <= u64::MAX as u128
                && qhat * v_next > ((rhat << 64) | (u[j + n - 2] as u128))
            {
                qhat -= 1;
                rhat += v_top;
            }

            // D4: multiply and subtract.
            let mut qhat = qhat as u64;
            let mut borrow = 0u64;
            let mut carry = 0u64;
            for i in 0..n {
                let product = (qhat as u128) * (v[i] as u128) + (carry as u128);
                carry = (product >> 64) as u64;
                let (diff, b1) = u[j + i].overflowing_sub(product as u64);
                let (diff, b2) = diff.overflowing_sub(borrow);
                u[j + i] = diff;
                borrow = u64::from(b1 || b2);
            }
            let (diff, b1) = u[j + n].overflowing_sub(carry);
            let (diff, b2) = diff.overflowing_sub(borrow);
            u[j + n] = diff;
            let underflow = b1 || b2;

            // D6: qhat was one too large; add the divisor back.
            if underflow {
                qhat -= 1;
                let mut carry = 0u64;
                for i in 0..n {
                    let (sum, c1) = u[j + i].overflowing_add(v[i]);
                    let (sum, c2) = sum.overflowing_add(carry);
                    u[j + i] = sum;
                    carry = u64::from(c1 || c2);
                }
                u[j + n] = u[j + n].wrapping_add(carry);
            }
            q[j] = qhat;
        }

        // D8: the remainder is the low `n` limbs, de-normalized.
        let mut rem = [0u64; 4];
        if shift == 0 {
            rem[..n].copy_from_slice(&u[..n]);
        } else {
            for i in 0..n {
                let mut value = u[i] >> shift;
                if i + 1 < n {
                    value |= u[i + 1] << (64 - shift);
                }
                rem[i] = value;
            }
        }
        (Self(q), Self(rem))
    }
}

impl Ord for U256 {
    fn cmp(&self, other: &Self) -> Ordering {
        for i in (0..4).rev() {
            match self.0[i].cmp(&other.0[i]) {
                Ordering::Equal => continue,
                order => return order,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for U256 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `(a * b) / d`, rounded down.
///
/// Returns [`MathError::Overflow`] when the quotient does not fit in `u128`
/// (which for AMM use means the Q64.64 result is out of range).
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> MathResult<u128> {
    if d == 0 {
        return Err(MathError::DivByZero);
    }
    let (q, _) = U256::mul_u128(a, b).div_rem(U256::from_u128(d)).unwrap();
    q.to_u128().ok_or(MathError::Overflow)
}

/// `(a * b) / d`, rounded up.
pub(crate) fn mul_div_ceil(a: u128, b: u128, d: u128) -> MathResult<u128> {
    if d == 0 {
        return Err(MathError::DivByZero);
    }
    let (q, r) = U256::mul_u128(a, b).div_rem(U256::from_u128(d)).unwrap();
    let q = if r.is_zero() {
        q
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)?
    };
    q.to_u128().ok_or(MathError::Overflow)
}

/// `floor((a * b) / 2^64)`, the Q64.64 multiply without rounding up.
pub(crate) fn mul_shr(a: u128, b: u128) -> MathResult<u128> {
    U256::mul_u128(a, b)
        .shr(64)
        .to_u128()
        .ok_or(MathError::Overflow)
}

/// `floor((a * b) / 2^64)` rounded up instead of down.
pub(crate) fn mul_shr_ceil(a: u128, b: u128) -> MathResult<u128> {
    let (q, r) = U256::mul_u128(a, b)
        .div_rem(U256::from_u128(1u128 << 64))
        .unwrap();
    let q = if r.is_zero() {
        q
    } else {
        q.checked_add(U256::ONE).ok_or(MathError::Overflow)?
    };
    q.to_u128().ok_or(MathError::Overflow)
}

/// `floor(num * 2^shift / den)`, or the ceiling when `round_up` is set.
///
/// This is the single division primitive behind the price and amount formulas:
/// it keeps the full width of `num` and `den`, so there is no intermediate
/// truncation. `shift` must be at most 255.
pub(crate) fn div_shifted(
    num: U256,
    shift: u32,
    den: U256,
    round_up: bool,
) -> MathResult<u128> {
    debug_assert!(shift < 256);
    if den.is_zero() {
        return Err(MathError::DivByZero);
    }
    // num * 2^shift / den = (num / den) * 2^shift + ((num % den) * 2^shift) / den
    let (high, rem) = num.div_rem(den).unwrap();
    let high = high.checked_shl(shift).ok_or(MathError::Overflow)?;
    let (low, rem) = match rem.checked_shl(shift) {
        Some(shifted) => shifted.div_rem(den).unwrap(),
        // Shifting the remainder overflows 256 bits: shift one bit at a time.
        // Reachable only when `den` is close to 2^256, which the formulas in
        // this crate never produce.
        None => shift_div(rem, shift, den)?,
    };
    let mut quotient = high.checked_add(low).ok_or(MathError::Overflow)?;
    if round_up && !rem.is_zero() {
        quotient = quotient.checked_add(U256::ONE).ok_or(MathError::Overflow)?;
    }
    quotient.to_u128().ok_or(MathError::Overflow)
}

/// `floor(rem * 2^shift / den)` by binary long division, for the cases where
/// `rem * 2^shift` does not fit in 256 bits.
fn shift_div(rem: U256, shift: u32, den: U256) -> MathResult<(U256, U256)> {
    debug_assert!(shift < 256);
    let mut acc = rem;
    let mut quotient = U256::ZERO;
    for i in (0..shift).rev() {
        acc = acc.checked_shl(1).ok_or(MathError::Overflow)?;
        if acc >= den {
            acc = acc.checked_sub(den).ok_or(MathError::Overflow)?;
            quotient.set_bit(i);
        }
    }
    Ok((quotient, acc))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed })
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_u128(&mut self) -> u128 {
            ((self.next_u64() as u128) << 64) | self.next_u64() as u128
        }
    }

    /// Reference division by binary long division: slow, but simply correct.
    ///
    /// Only used with divisors below 2^250 so that the running remainder can
    /// always be shifted left without overflowing 256 bits.
    fn div_rem_reference(n: U256, d: U256) -> (U256, U256) {
        assert!(!d.is_zero());
        assert!(d.bit_len() <= 250);
        let mut quotient = U256::ZERO;
        let mut rem = U256::ZERO;
        for i in (0..n.bit_len()).rev() {
            rem = rem.checked_shl(1).unwrap();
            if n.bit(i) {
                rem = rem.checked_add(U256::ONE).unwrap();
            }
            if rem >= d {
                rem = rem.checked_sub(d).unwrap();
                quotient.set_bit(i);
            }
        }
        (quotient, rem)
    }

    #[test]
    fn knuth_matches_reference_division() {
        let mut rng = Rng::new(0x243f_6a88_85a3_08d3);
        for _ in 0..2_000 {
            let num = U256([rng.next_u64(), rng.next_u64(), rng.next_u64(), rng.next_u64()]);
            // Mix divisor shapes: one, two, three and four limbs.
            let limbs = 1 + (rng.next_u64() % 4) as usize;
            let mut d = [0u64; 4];
            for (i, limb) in d.iter_mut().enumerate().take(limbs) {
                *limb = rng.next_u64() | if i + 1 == limbs { 1 } else { 0 };
            }
            let d = U256(d);
            if d.bit_len() > 250 {
                continue;
            }
            assert_eq!(
                num.div_rem(d).unwrap(),
                div_rem_reference(num, d),
                "num={num:?} d={d:?}"
            );
        }
    }

    #[test]
    fn mul_u128_is_exact() {
        let mut rng = Rng::new(0x1319_8a2e_0370_7344);
        for _ in 0..2_000 {
            let a = rng.next_u128() | 1;
            let b = rng.next_u128();
            // a * b divided by a must give exactly b with no remainder.
            let (quotient, rem) = U256::mul_u128(a, b).div_rem(U256::from_u128(a)).unwrap();
            assert!(rem.is_zero());
            assert_eq!(quotient, U256::from_u128(b));
        }
    }

    #[test]
    fn mul_div_matches_exact_division_for_small_values() {
        let mut rng = Rng::new(1);
        for _ in 0..5_000 {
            let a = (rng.next_u64() % 1_000_000) as u128;
            let b = (rng.next_u64() % 1_000_000) as u128;
            let d = (rng.next_u64() % 1_000_000 + 1) as u128;
            assert_eq!(mul_div_floor(a, b, d).unwrap(), (a * b) / d);
            assert_eq!(mul_div_ceil(a, b, d).unwrap(), (a * b).div_ceil(d));
        }
    }

    #[test]
    fn mul_shr_rounds_down_and_mul_shr_ceil_rounds_up() {
        // 2^64 * (2^64 + 1) / 2^64 is exact.
        assert_eq!(mul_shr(1 << 64, (1 << 64) + 1).unwrap(), (1 << 64) + 1);
        assert_eq!(mul_shr_ceil(1 << 64, (1 << 64) + 1).unwrap(), (1 << 64) + 1);
        // 3 / 2^64 is below one unit, so floor and ceil differ.
        assert_eq!(mul_shr(3, 1).unwrap(), 0);
        assert_eq!(mul_shr_ceil(3, 1).unwrap(), 1);
        // Exact products need no adjustment.
        assert_eq!(mul_shr(3 << 64, 2 << 64).unwrap(), 6 << 64);
        assert_eq!(mul_shr_ceil(3 << 64, 2 << 64).unwrap(), 6 << 64);
    }

    #[test]
    fn div_shifted_matches_small_exact_division() {
        let mut rng = Rng::new(7);
        for _ in 0..2_000 {
            let a = (rng.next_u64() % 100_000) as u128;
            let b = (rng.next_u64() % 100_000) as u128;
            let d = (rng.next_u64() % 100_000 + 1) as u128;
            let num = U256::mul_u128(a, b);
            let den = U256::from_u128(d);
            assert_eq!(div_shifted(num, 64, den, false).unwrap(), (a * b * (1 << 64)) / d);
            assert_eq!(
                div_shifted(num, 64, den, true).unwrap(),
                (a * b * (1 << 64)).div_ceil(d)
            );
        }
    }

    #[test]
    fn division_by_zero_is_an_error() {
        assert_eq!(mul_div_floor(1, 1, 0), Err(MathError::DivByZero));
        assert_eq!(mul_div_ceil(1, 1, 0), Err(MathError::DivByZero));
        assert_eq!(
            div_shifted(U256::ONE, 64, U256::ZERO, false),
            Err(MathError::DivByZero)
        );
    }

    #[test]
    fn quotient_overflow_is_reported() {
        // (2^128 - 1)^2 / 1 does not fit in 128 bits.
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, 1), Err(MathError::Overflow));
        assert_eq!(mul_shr(u128::MAX, u128::MAX), Err(MathError::Overflow));
    }

    #[test]
    fn shifts_and_ordering() {
        let small = U256::from_u128(5);
        let big = U256([5, 0, 0, 1]);
        assert!(small < big);
        // 5 + 2^192 shifted right by one limb keeps only the top bit.
        assert_eq!(big.shr(64), U256([0, 0, 1, 0]));
        assert_eq!(U256([5, 6, 0, 0]).shr(64), U256::from_u128(6));
        assert_eq!(small.checked_shl(64), Some(U256([0, 5, 0, 0])));
        assert_eq!(U256([0, 0, 0, 1u64 << 63]).checked_shl(1), None);
        assert_eq!(U256::from_u128(1).bit_len(), 1);
        assert_eq!(U256::ZERO.bit_len(), 0);
        assert!(U256::ZERO.bit(200) == false);
    }
}
