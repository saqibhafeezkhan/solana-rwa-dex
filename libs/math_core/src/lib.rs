//! # Q64.64 Fixed-Point Math Core
//!
//! High-precision, zero-dependency Q64.64 fixed-point arithmetic library for Solana AMMs.
//! - Q64.64 format: 64 bits integer, 64 bits fractional, stored in `u128`.
//! - No float arithmetic or dynamic allocations (`no_std` compatible).
//! - 256-bit wide intermediate calculations to prevent precision loss and overflow.

#![no_std]

pub mod fixed_point;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_math;
pub mod u256;

use core::fmt;

/// Q64.64 fixed-point constant: 1.0 = 2^64
pub const Q64: u128 = 1u128 << 64;

/// Errors produced by arithmetic operations in `math_core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MathError {
    /// Arithmetic overflow or value out of representable range.
    Overflow,
    /// Division by zero.
    DivByZero,
    /// Tick index out of valid range `[MIN_TICK, MAX_TICK]`.
    TickOutOfBounds,
    /// Sqrt price out of valid range `[MIN_SQRT_PRICE, MAX_SQRT_PRICE]`.
    SqrtPriceOutOfBounds,
    /// Swap fee pips equal to or exceeding 1,000,000 (100%).
    InvalidFee,
}

impl fmt::Display for MathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MathError::Overflow => write!(f, "Arithmetic overflow"),
            MathError::DivByZero => write!(f, "Division by zero"),
            MathError::TickOutOfBounds => write!(f, "Tick out of bounds"),
            MathError::SqrtPriceOutOfBounds => write!(f, "Sqrt price out of bounds"),
            MathError::InvalidFee => write!(f, "Invalid swap fee"),
        }
    }
}

pub type MathResult<T> = Result<T, MathError>;

// Re-export key functions at crate root for convenience
pub use fixed_point::{
    div_ceil, div_shr, isqrt_wide, mul_ceil, mul_shr, price_from_sqrt_price, sqrt, sqrt_price,
};
pub use sqrt_price_math::{
    get_amount_0_delta, get_amount_0_for_amount_1, get_amount_1_delta, get_amount_1_for_amount_0,
    get_next_sqrt_price_from_amount_0, get_next_sqrt_price_from_amount_1,
};
pub use swap_math::{compute_swap_step, SwapStepResult, FEE_DENOMINATOR};
pub use tick_math::{
    sqrt_price_at_tick, tick_at_sqrt_price, MAX_SQRT_PRICE, MAX_TICK, MIN_SQRT_PRICE, MIN_TICK,
    TICK_RATIOS,
};

/// Converts a u64 integer into Q64.64 representation (x * 2^64).
pub const fn from_u64(x: u64) -> u128 {
    (x as u128) << 64
}

/// Converts a Q64.64 number back to u64 (truncating fractional part).
pub const fn to_u64(x: u128) -> u64 {
    (x >> 64) as u64
}
