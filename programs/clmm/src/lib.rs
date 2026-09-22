//! # CLMM (Concentrated Liquidity Market Maker)
//!
//! This AMM uses a continuous price curve, but allows Liquidity Providers (LPs)
//! to concentrate their capital within specific price ranges (Ticks). The model
//! is Uniswap v3's: price is `token B / token A` stored as a Q64.64 square root
//! (`sqrt_price_x64`), and every position pins its liquidity to a tick range.
//!
//! ## Key Concepts:
//! - `sqrt_price_x64`: The current price of the pool, stored as a Q64.64 square root.
//! - `liquidity`: The active liquidity currently provided between the current tick and the next tick.
//! - `fee_growth_global`: An accumulator that tracks fees earned per unit of liquidity (Q64.64).
//! - `fee_growth_outside`: Per-tick mirror of the global accumulator, flipped on every crossing,
//!   which makes "fees earned inside a range" a subtraction of three numbers.
//! - `tick`: A discrete price point (`1.0001^tick`). When the price crosses a tick,
//!   the pool's active liquidity changes by the tick's `liquidity_net`.
//!
//! ## Swap walk protocol
//!
//! A swap can move the price across many ticks, and each tick is its own PDA, so the
//! caller supplies the ticks the trade may cross as **remaining accounts**, in walk
//! order: ascending ticks when selling B for A (price up), descending when selling A
//! for B (price down). The program stops at the last supplied tick, so a short walk
//! partially fills the trade and returns the unused input rather than failing.
//!
//! The order is not taken on trust. Every pool keeps a **tick bitmap** with one bit
//! per `tick_spacing`-wide cell, set while the tick at that cell carries liquidity,
//! and the walk is checked against it: each supplied account must be the very next
//! initialized tick the price reaches. A caller can therefore no longer skip a tick
//! to keep `pool.liquidity` from changing — that is the one piece of pool state a
//! trade cannot recompute, and from then on every price would be quoted against the
//! wrong active set.
//!
//! Crossing also has to be symmetric with the bitmap: because the search is anchored
//! on `tick_current` in tick units, a price that sits exactly on a boundary is
//! crossed again in the direction it is travelling (a zero-width step, exactly as
//! Uniswap v3 does), which is what activates or deactivates the ranges pinned there.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Transfer as SplTransfer};
use math_core::{self, MathError, Q64};

declare_id!("HLMAwimHtgMV8ME9X4RaoHCAdfU35hrovEg4AU5ifw7u");

/// Upper bound on the pool fee in pips (millionths). The math core rejects
/// anything at or above this.
const MAX_FEE_PIPS: u32 = math_core::FEE_DENOMINATOR - 1;

/// The swap fee charged on `amount_in`, in the same token:
/// `ceil(amount_in * fee_pips / (1_000_000 - fee_pips))`. Charging on the
/// consumed input (rather than the offered amount) is what makes a multi-tick
/// walk charge only for the part of the trade that actually executed. The
/// identity `amount_in + fee <= remaining` holds for every offered amount, so
/// the walk's budget can never go negative.
fn fee_on_input(amount_in: u128, fee_pips: u32) -> MathResultU128 {
    let denom = u128::from(math_core::FEE_DENOMINATOR - fee_pips);
    let numerator = amount_in
        .checked_mul(u128::from(fee_pips))
        .ok_or(MathError::Overflow)?;
    numerator
        .checked_add(denom)
        .and_then(|v| v.checked_sub(1))
        .and_then(|v| v.checked_div(denom))
        .ok_or(MathError::Overflow)
}

/// Local alias so the helper can mirror `math_core::MathResult` without
/// importing it twice under different names.
type MathResultU128 = core::result::Result<u128, MathError>;

/// Maps a `math_core` error onto the most specific program error.
fn math_err(e: MathError) -> Error {
    match e {
        MathError::TickOutOfBounds => error!(ErrorCode::InvalidTicks),
        MathError::SqrtPriceOutOfBounds => error!(ErrorCode::SqrtPriceOutOfBounds),
        MathError::InvalidFee => error!(ErrorCode::FeeTooHigh),
        _ => error!(ErrorCode::MathOverflow),
    }
}

// -----------------------------------------------------------------------
// TICK BITMAP
// -----------------------------------------------------------------------

/// Upper bound on a pool's bitmap, which is what keeps a pool inside a single
/// account. Across the full tick range this means `tick_spacing >= 14`.
const BITMAP_BITS_MAX: u32 = 65_536;

/// The cell a tick belongs to: `tick / tick_spacing` rounded down (Euclidean
/// division, so negative ticks round the same way positive ones do). Cells are
/// one spacing wide, which makes the mapping monotone and injective over the
/// ticks a pool accepts — every tick bound is a multiple of the spacing, so two
/// distinct bounds are always at least a full cell apart.
///
/// The result is signed: after a down-cross `tick_current` holds the tick *below*
/// the one crossed, which at the bottom of the range falls outside the range.
const fn cell_of(tick: i32, tick_spacing: i32) -> i64 {
    (tick as i64).div_euclid(tick_spacing as i64)
}

/// The bitmap bit for a tick, offset so that bit 0 is the cell holding
/// `MIN_TICK`. Signed; `-1` means "below every bit".
const fn bit_of(tick: i32, tick_spacing: i32) -> i64 {
    cell_of(tick, tick_spacing) - cell_of(math_core::MIN_TICK, tick_spacing)
}

/// Bits needed to cover the whole valid tick range at this spacing.
const fn bitmap_cells(tick_spacing: i32) -> u32 {
    let spacing = if tick_spacing < 1 { 1 } else { tick_spacing };
    (cell_of(math_core::MAX_TICK, spacing) - cell_of(math_core::MIN_TICK, spacing) + 1) as u32
}

/// Bytes needed to store `cells` bits.
const fn bitmap_bytes(cells: u32) -> usize {
    (cells as usize).div_ceil(8)
}

/// Reads one bit. Only the tests read bits directly; the instruction paths
/// search with `prev_set` / `next_set`.
#[cfg(test)]
fn bit_get(bits: &[u8], bit: u32) -> bool {
    let byte = (bit / 8) as usize;
    byte < bits.len() && bits[byte] & (1u8 << (bit % 8)) != 0
}

/// Sets one bit, reporting whether `bit` was inside `bits`.
fn bit_set(bits: &mut [u8], bit: u32) -> bool {
    let byte = (bit / 8) as usize;
    if byte >= bits.len() {
        return false;
    }
    bits[byte] |= 1u8 << (bit % 8);
    true
}

/// Clears one bit, ignoring bits outside `bits`.
fn bit_clear(bits: &mut [u8], bit: u32) {
    let byte = (bit / 8) as usize;
    if byte < bits.len() {
        bits[byte] &= !(1u8 << (bit % 8));
    }
}

/// Highest set bit of a non-zero byte.
const fn top_bit(byte: u8) -> u32 {
    7 - byte.leading_zeros()
}

/// Lowest set bit of a non-zero byte.
const fn bottom_bit(byte: u8) -> u32 {
    byte.trailing_zeros()
}

/// The highest set bit at or below `from`. `from` is signed and may sit below
/// bit 0; it is capped at the last bit, so a price past the top of the range
/// still finds the topmost set bit. `None` means the price cannot move further
/// in that direction.
fn prev_set(bits: &[u8], cells: u32, from: i64) -> Option<u32> {
    if bits.is_empty() || cells == 0 || from < 0 {
        return None;
    }
    let start = from.min(i64::from(cells) - 1) as u32;
    let mut byte = (start / 8) as usize;
    if byte >= bits.len() {
        return None;
    }
    // The byte holding `start` is masked so that only the bits at or below it
    // can match: the search is inclusive of `start`.
    let masked = bits[byte] & (0xFFu8 >> (7 - (start % 8)));
    if masked != 0 {
        return Some(byte as u32 * 8 + top_bit(masked));
    }
    // Otherwise walk bytes downward, skipping runs of zero bytes eight at a time.
    while byte > 0 {
        let low = byte.saturating_sub(8);
        if bits[low..byte].iter().any(|b| *b != 0) {
            let mut i = byte;
            while i > low {
                i -= 1;
                if bits[i] != 0 {
                    return Some(i as u32 * 8 + top_bit(bits[i]));
                }
            }
        }
        byte = low;
    }
    None
}

/// The lowest set bit at or above `from`. `None` means there is no initialized
/// tick left in that direction.
fn next_set(bits: &[u8], cells: u32, from: i64) -> Option<u32> {
    if bits.is_empty() || from < 0 || from >= i64::from(cells) {
        return None;
    }
    let start = from as u32;
    let mut byte = (start / 8) as usize;
    if byte >= bits.len() {
        return None;
    }
    let masked = bits[byte] & (0xFFu8 << (start % 8));
    if masked != 0 {
        let bit = byte as u32 * 8 + bottom_bit(masked);
        return (bit < cells).then_some(bit);
    }
    byte += 1;
    while byte < bits.len() {
        let high = (byte + 8).min(bits.len());
        if let Some(offset) = bits[byte..high].iter().position(|b| *b != 0) {
            let index = byte + offset;
            let bit = index as u32 * 8 + bottom_bit(bits[index]);
            return (bit < cells).then_some(bit);
        }
        byte = high;
    }
    None
}

/// The next initialized cell strictly beyond `bit` in the walk direction: lower
/// for a down walk, higher for an up walk.
fn walk_next(bits: &[u8], cells: u32, is_a_to_b: bool, bit: u32) -> Option<u32> {
    if is_a_to_b {
        prev_set(bits, cells, i64::from(bit) - 1)
    } else {
        next_set(bits, cells, i64::from(bit) + 1)
    }
}

/// Uniswap v3's `Tick.cross` plus the pool-side update: flip the tick's
/// fee-growth mirror, apply its net liquidity to the active set, and record
/// where the price now is.
///
/// The recorded tick is deliberately asymmetric — an up-cross leaves the price on
/// `tick_index`, a down-cross on `tick_index - 1`. That is v3's convention, and it
/// is what keeps a position whose bound sits exactly on the current price on the
/// correct side of it: active when the price walks up to the bound, inactive when
/// it walks down through it.
fn cross_tick(pool: &mut Pool, tick: &mut TickState, is_a_to_b: bool) -> Result<()> {
    tick.fee_growth_outside_a = pool
        .fee_growth_global_a
        .wrapping_sub(tick.fee_growth_outside_a);
    tick.fee_growth_outside_b = pool
        .fee_growth_global_b
        .wrapping_sub(tick.fee_growth_outside_b);

    let liquidity =
        i128::try_from(pool.liquidity).map_err(|_| error!(ErrorCode::MathOverflow))?;
    let next = if is_a_to_b {
        // Moving down: the ranges pinned to this tick are left behind.
        liquidity.checked_sub(tick.liquidity_net)
    } else {
        // Moving up: they join the active set.
        liquidity.checked_add(tick.liquidity_net)
    }
    .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    pool.liquidity = u128::try_from(next).map_err(|_| error!(ErrorCode::MathOverflow))?;

    pool.tick_current = if is_a_to_b {
        tick.tick_index - 1
    } else {
        tick.tick_index
    };
    Ok(())
}

#[program]
pub mod clmm {
    use super::*;

    /// Initializes a new CLMM pool with its two vaults.
    ///
    /// `initial_sqrt_price_x64` may be any price inside the usable Q64.64 range
    /// and does not have to sit on a tick boundary. `fee_pips` is the swap fee
    /// in millionths (3_000 = 0.3%).
    ///
    /// `kyc_config`, `oracle_state` and `hooks_authority` fix the pool's
    /// wiring policy at birth (`Pubkey::default()` leaves a rail disabled):
    ///
    /// * `kyc_config` — every swap CPIs the KYC gate for the trader.
    /// * `oracle_state` — every swap runs the oracle circuit breaker on its
    ///   start price and records the resulting price afterwards.
    /// * `hooks_authority` — swap output is routed into the T+N settlement
    ///   escrow (through the hooks program) instead of paid to the trader,
    ///   unlocking `settlement_slots` after the swap.
    ///
    /// The matching program accounts (`kyc_program`, `oracle_program`,
    /// `hooks_program`) must be passed when the policy is set; the context
    /// verifies each against its real program ID, which pins the config
    /// oracle/hook accounts to the genuine programs.
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        initial_sqrt_price_x64: u128,
        tick_spacing: i32,
        fee_pips: u32,
        kyc_config: Pubkey,
        oracle_state: Pubkey,
        hooks_authority: Pubkey,
        settlement_slots: u64,
    ) -> Result<()> {
        require!(tick_spacing > 0, ErrorCode::InvalidTickSpacing);
        require!(fee_pips <= MAX_FEE_PIPS, ErrorCode::FeeTooHigh);
        // The bitmap has to cover the whole valid tick range in one account, which
        // is what sets the floor on the spacing.
        let cells = bitmap_cells(tick_spacing);
        require!(cells <= BITMAP_BITS_MAX, ErrorCode::TickSpacingTooFine);

        // Settlement routing needs both halves of the wiring and a horizon.
        if hooks_authority != Pubkey::default() {
            require!(settlement_slots > 0, ErrorCode::InvalidSettlementSlots);
        } else {
            require!(settlement_slots == 0, ErrorCode::InvalidSettlementSlots);
        }

        let pool = &mut ctx.accounts.pool;

        pool.mint_a = ctx.accounts.mint_a.key();
        pool.mint_b = ctx.accounts.mint_b.key();
        pool.sqrt_price_x64 = initial_sqrt_price_x64;
        pool.liquidity = 0;
        pool.tick_current = math_core::tick_at_sqrt_price(initial_sqrt_price_x64)
            .map_err(math_err)?;
        pool.tick_spacing = tick_spacing;
        pool.fee_pips = fee_pips;
        pool.fee_growth_global_a = 0;
        pool.fee_growth_global_b = 0;
        pool.bump = ctx.bumps.pool;
        pool.bitmap_cells = cells;
        pool.tick_bitmap = vec![0u8; bitmap_bytes(cells)];
        pool.kyc_config = kyc_config;
        pool.oracle_state = oracle_state;
        pool.hooks_authority = hooks_authority;
        pool.settlement_slots = settlement_slots;

        msg!(
            "CLMM pool initialized at tick {} (fee {} pips, spacing {}). Wiring: kyc {}, oracle {}, hooks {} (T+{} slots)",
            pool.tick_current,
            fee_pips,
            tick_spacing,
            kyc_config,
            oracle_state,
            hooks_authority,
            settlement_slots
        );
        Ok(())
    }

    /// Adds concentrated liquidity between two ticks.
    ///
    /// Both ticks must be multiples of the pool's `tick_spacing`. The token
    /// amounts are computed for the position's exact range and the current
    /// price: token A only when the price is at or below the range, token B
    /// only when at or above it, and both legs when the price is inside.
    pub fn add_liquidity(
        ctx: Context<AddLiquidity>,
        lower_tick: i32,
        upper_tick: i32,
        liquidity_amount: u128,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let lower_tick_state = &mut ctx.accounts.lower_tick_state;
        let upper_tick_state = &mut ctx.accounts.upper_tick_state;
        let position = &mut ctx.accounts.position;

        require!(liquidity_amount > 0, ErrorCode::ZeroLiquidity);
        require!(lower_tick < upper_tick, ErrorCode::InvalidTicks);
        require!(
            math_core::tick_math::is_valid_tick(lower_tick)
                && math_core::tick_math::is_valid_tick(upper_tick),
            ErrorCode::InvalidTicks
        );
        require!(
            lower_tick % pool.tick_spacing == 0 && upper_tick % pool.tick_spacing == 0,
            ErrorCode::InvalidTickSpacing
        );

        // 1. Calculate token amounts required for this liquidity.
        // Token A is token0 and token B is token1 (price = B / A). The amounts
        // come from the user, so they round up.
        let sqrt_lower = math_core::sqrt_price_at_tick(lower_tick).map_err(math_err)?;
        let sqrt_upper = math_core::sqrt_price_at_tick(upper_tick).map_err(math_err)?;
        let (amount_a, amount_b) =
            position_amounts(pool, sqrt_lower, sqrt_upper, liquidity_amount, true)
                .map_err(math_err)?;

        // 2. A first touch binds the position to its owner and its range. The PDA
        // seeds already tie the address to those values, so recording them here is
        // what makes the `position.owner == user` checks in `remove_liquidity` and
        // `collect_fees` succeed — a position whose `owner` was left unset could
        // never be withdrawn from.
        if position.owner == Pubkey::default() {
            position.pool = pool.key();
            position.owner = ctx.accounts.user.key();
            position.lower_tick = lower_tick;
            position.upper_tick = upper_tick;
        } else {
            require!(
                position.owner == ctx.accounts.user.key()
                    && position.pool == pool.key()
                    && position.lower_tick == lower_tick
                    && position.upper_tick == upper_tick,
                ErrorCode::WrongPosition
            );
        }

        // 3. Accrue fees earned by the existing position BEFORE any state
        // changes, then grow it and re-base its fee snapshot.
        let (inside_a, inside_b) = fee_growth_inside(
            lower_tick_state,
            upper_tick_state,
            pool.tick_current,
            pool.fee_growth_global_a,
            pool.fee_growth_global_b,
        );
        accrue_fees(position, inside_a, inside_b)?;
        position.liquidity = position
            .liquidity
            .checked_add(liquidity_amount)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        position.fee_growth_inside_a = inside_a;
        position.fee_growth_inside_b = inside_b;

        // 3. Initialize the tick accounts the first time they are used.
        // By convention (Uniswap v3) all growth before a tick was initialized
        // is assumed to have happened below it: a tick at or below the current
        // price starts with the full global accumulator as its "outside".
        if lower_tick_state.liquidity_gross == 0 {
            lower_tick_state.pool = pool.key();
            lower_tick_state.tick_index = lower_tick;
            lower_tick_state.bump = ctx.bumps.lower_tick_state;
            lower_tick_state.fee_growth_outside_a = if lower_tick <= pool.tick_current {
                pool.fee_growth_global_a
            } else {
                0
            };
            lower_tick_state.fee_growth_outside_b = if lower_tick <= pool.tick_current {
                pool.fee_growth_global_b
            } else {
                0
            };
        }
        if upper_tick_state.liquidity_gross == 0 {
            upper_tick_state.pool = pool.key();
            upper_tick_state.tick_index = upper_tick;
            upper_tick_state.bump = ctx.bumps.upper_tick_state;
            upper_tick_state.fee_growth_outside_a = if upper_tick <= pool.tick_current {
                pool.fee_growth_global_a
            } else {
                0
            };
            upper_tick_state.fee_growth_outside_b = if upper_tick <= pool.tick_current {
                pool.fee_growth_global_b
            } else {
                0
            };
        }

        // 4. Update Tick States (gross liquidity and net crossing delta).
        lower_tick_state.liquidity_gross = lower_tick_state
            .liquidity_gross
            .checked_add(liquidity_amount)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        upper_tick_state.liquidity_gross = upper_tick_state
            .liquidity_gross
            .checked_add(liquidity_amount)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;

        let liquidity_delta =
            i128::try_from(liquidity_amount).map_err(|_| error!(ErrorCode::MathOverflow))?;
        lower_tick_state.liquidity_net = lower_tick_state
            .liquidity_net
            .checked_add(liquidity_delta)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        upper_tick_state.liquidity_net = upper_tick_state
            .liquidity_net
            .checked_sub(liquidity_delta)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;

        // 5. Record both ticks as initialized in the pool's bitmap: a swap checks
        // its walk against the bitmap, so the entry has to exist before any trade
        // can reach the tick.
        for tick_index in [lower_tick, upper_tick] {
            let bit = bit_of(tick_index, pool.tick_spacing);
            require!(
                bit >= 0 && bit < i64::from(pool.bitmap_cells),
                ErrorCode::TickOutOfBitmap
            );
            require!(
                bit_set(&mut pool.tick_bitmap, bit as u32),
                ErrorCode::TickOutOfBitmap
            );
        }

        // 6. Update Pool Liquidity if the position spans the current price.
        if pool.tick_current >= lower_tick && pool.tick_current < upper_tick {
            pool.liquidity = pool
                .liquidity
                .checked_add(liquidity_amount)
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        }

        // 7. Transfer tokens from user to pool vaults.
        if amount_a > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.key(),
                    SplTransfer {
                        from: ctx.accounts.user_token_a.to_account_info(),
                        to: ctx.accounts.vault_a.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                u64::try_from(amount_a).map_err(|_| error!(ErrorCode::MathOverflow))?,
            )?;
        }
        if amount_b > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.key(),
                    SplTransfer {
                        from: ctx.accounts.user_token_b.to_account_info(),
                        to: ctx.accounts.vault_b.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                u64::try_from(amount_b).map_err(|_| error!(ErrorCode::MathOverflow))?,
            )?;
        }

        msg!(
            "Added {} liquidity between ticks {} and {}",
            liquidity_amount,
            lower_tick,
            upper_tick
        );
        Ok(())
    }

    /// Removes liquidity from a position and claims the token amounts that
    /// liquidity currently backs (fees are claimed separately via
    /// `collect_fees`). `min_amount_a` / `min_amount_b` are checked before
    /// anything is written, so a failed slippage check leaves all state intact.
    pub fn remove_liquidity(
        ctx: Context<RemoveLiquidity>,
        lower_tick: i32,
        upper_tick: i32,
        liquidity_amount: u128,
        min_amount_a: u64,
        min_amount_b: u64,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let position = &mut ctx.accounts.position;
        let lower_tick_state = &mut ctx.accounts.lower_tick_state;
        let upper_tick_state = &mut ctx.accounts.upper_tick_state;

        require!(liquidity_amount > 0, ErrorCode::ZeroLiquidity);
        require!(
            position.liquidity >= liquidity_amount,
            ErrorCode::InsufficientShares
        );

        let sqrt_lower = math_core::sqrt_price_at_tick(lower_tick).map_err(math_err)?;
        let sqrt_upper = math_core::sqrt_price_at_tick(upper_tick).map_err(math_err)?;

        // 1. Accrue fees earned so far (they are unaffected by the burn).
        let (inside_a, inside_b) = fee_growth_inside(
            lower_tick_state,
            upper_tick_state,
            pool.tick_current,
            pool.fee_growth_global_a,
            pool.fee_growth_global_b,
        );
        accrue_fees(position, inside_a, inside_b)?;

        // 2. Compute the amounts this share of liquidity backs. The pool pays
        // out, so the amounts round down.
        let (amount_a, amount_b) =
            position_amounts(pool, sqrt_lower, sqrt_upper, liquidity_amount, false)
                .map_err(math_err)?;
        let amount_a = u64::try_from(amount_a).map_err(|_| error!(ErrorCode::MathOverflow))?;
        let amount_b = u64::try_from(amount_b).map_err(|_| error!(ErrorCode::MathOverflow))?;
        require!(amount_a >= min_amount_a, ErrorCode::SlippageExceeded);
        require!(amount_b >= min_amount_b, ErrorCode::SlippageExceeded);

        // 3. Shrink the position and the tick totals.
        position.liquidity = position
            .liquidity
            .checked_sub(liquidity_amount)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;

        lower_tick_state.liquidity_gross = lower_tick_state
            .liquidity_gross
            .checked_sub(liquidity_amount)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        upper_tick_state.liquidity_gross = upper_tick_state
            .liquidity_gross
            .checked_sub(liquidity_amount)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;

        let liquidity_delta =
            i128::try_from(liquidity_amount).map_err(|_| error!(ErrorCode::MathOverflow))?;
        lower_tick_state.liquidity_net = lower_tick_state
            .liquidity_net
            .checked_sub(liquidity_delta)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        upper_tick_state.liquidity_net = upper_tick_state
            .liquidity_net
            .checked_add(liquidity_delta)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;

        // 4. A tick stops existing once the last position pinned to it leaves, so
        // clear its bitmap bit: a walk must never be asked for a tick that carries
        // no liquidity. Both totals have to be zero — a non-zero net with zero
        // gross still matters, because crossing the tick moves the active set.
        for (tick_index, state) in [
            (lower_tick, &*lower_tick_state),
            (upper_tick, &*upper_tick_state),
        ] {
            if state.liquidity_gross == 0 && state.liquidity_net == 0 {
                let bit = bit_of(tick_index, pool.tick_spacing);
                require!(
                    bit >= 0 && bit < i64::from(pool.bitmap_cells),
                    ErrorCode::TickOutOfBitmap
                );
                bit_clear(&mut pool.tick_bitmap, bit as u32);
            }
        }

        // 5. Update the pool's active liquidity if the range spans the price.
        if pool.tick_current >= lower_tick && pool.tick_current < upper_tick {
            pool.liquidity = pool
                .liquidity
                .checked_sub(liquidity_amount)
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        }

        // 6. Pay out. The pool PDA signs for its vaults.
        // Copy the seed material out of `pool` first so the mutable borrow
        // ends before the CPI borrows the accounts.
        let (mint_a_key, mint_b_key, pool_bump) = (pool.mint_a, pool.mint_b, pool.bump);
        let pool_seeds: [&[u8]; 4] = [
            b"clmm-pool",
            mint_a_key.as_ref(),
            mint_b_key.as_ref(),
            &[pool_bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[&pool_seeds];
        let pool_info = ctx.accounts.pool.to_account_info();

        if amount_a > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    SplTransfer {
                        from: ctx.accounts.vault_a.to_account_info(),
                        to: ctx.accounts.user_token_a.to_account_info(),
                        authority: pool_info.clone(),
                    },
                    signer_seeds,
                ),
                amount_a,
            )?;
        }
        if amount_b > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    SplTransfer {
                        from: ctx.accounts.vault_b.to_account_info(),
                        to: ctx.accounts.user_token_b.to_account_info(),
                        authority: pool_info,
                    },
                    signer_seeds,
                ),
                amount_b,
            )?;
        }

        msg!(
            "Removed {} liquidity between ticks {} and {}",
            liquidity_amount,
            lower_tick,
            upper_tick
        );
        Ok(())
    }

    /// Claims the fees a position has earned. Fee accounting is updated with
    /// the current fee-growth snapshots before the transfer, so fees keep
    /// accruing cleanly after the claim.
    pub fn collect_fees(
        ctx: Context<CollectFees>,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let position = &mut ctx.accounts.position;
        let lower_tick_state = &ctx.accounts.lower_tick_state;
        let upper_tick_state = &ctx.accounts.upper_tick_state;

        // The PDA seeds already tie the account to this range; checking the
        // recorded range as well keeps the fee snapshot and the ticks it is read
        // against from ever coming from different ranges.
        require!(
            position.lower_tick == lower_tick && position.upper_tick == upper_tick,
            ErrorCode::WrongPosition
        );

        // 1. Bring the position's fee accounting up to date.
        let (inside_a, inside_b) = fee_growth_inside(
            lower_tick_state,
            upper_tick_state,
            pool.tick_current,
            pool.fee_growth_global_a,
            pool.fee_growth_global_b,
        );
        accrue_fees(position, inside_a, inside_b)?;

        let (owed_a, owed_b) = (position.tokens_owed_a, position.tokens_owed_b);
        position.tokens_owed_a = 0;
        position.tokens_owed_b = 0;

        if owed_a == 0 && owed_b == 0 {
            msg!("No fees to collect");
            return Ok(());
        }

        // 2. Transfer the owed fees. The pool PDA signs for its vaults.
        let pool_seeds: [&[u8]; 4] = [
            b"clmm-pool",
            pool.mint_a.as_ref(),
            pool.mint_b.as_ref(),
            &[pool.bump],
        ];
        let signer_seeds: &[&[&[u8]]] = &[&pool_seeds];
        let pool_info = ctx.accounts.pool.to_account_info();

        if owed_a > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    SplTransfer {
                        from: ctx.accounts.vault_a.to_account_info(),
                        to: ctx.accounts.user_token_a.to_account_info(),
                        authority: pool_info.clone(),
                    },
                    signer_seeds,
                ),
                owed_a,
            )?;
        }
        if owed_b > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    SplTransfer {
                        from: ctx.accounts.vault_b.to_account_info(),
                        to: ctx.accounts.user_token_b.to_account_info(),
                        authority: pool_info,
                    },
                    signer_seeds,
                ),
                owed_b,
            )?;
        }

        msg!("Collected {} A and {} B of fees", owed_a, owed_b);
        Ok(())
    }

    /// Executes a swap. The price moves through the supplied ticks until the
    /// input is consumed or the walk runs out, whichever comes first.
    ///
    /// `ctx.remaining_accounts` must contain the initialized `TickState` PDAs
    /// the trade may cross, in walk order:
    /// - selling A for B (price down): descending ticks, starting on the current
    ///   tick when the price sits exactly on a boundary;
    /// - selling B for A (price up): ascending ticks, all strictly above it.
    ///
    /// The walk is checked against the pool's tick bitmap rather than trusted, so
    /// each supplied account has to be the very next initialized tick the price
    /// reaches and a skipped tick is rejected. A trade that runs out of supplied
    /// ticks is partially filled: only the consumed input is pulled from the user.
    ///
    /// Pool-level rails, each enabled at `initialize_pool`, run around the
    /// swap math:
    /// * **KYC** (`kyc_config` set): the trader must hold a valid attestation
    ///   (`kyc_gate::verify_kyc` CPI) before any math runs.
    /// * **Oracle** (`oracle_state` set): `check_deviation` gates the start
    ///   price, and the settled price is recorded back into the TWAP.
    /// * **Settlement** (`hooks_authority` set): the output is NOT paid to
    ///   the trader. It is routed to the hook authority's custody ATA and
    ///   locked in the T+N escrow (`trade_id` names the trade); the trader
    ///   withdraws after `settlement_slots` and a Transfer Agent signature.
    ///   The output amount is checked against `min_amount_out` *before* it
    ///   leaves the pool's vault.
    pub fn swap(
        ctx: Context<Swap>,
        amount_in: u64,
        min_amount_out: u64,
        is_a_to_b: bool,
        trade_id: u64,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let ticks = &ctx.remaining_accounts;

        require!(amount_in > 0, ErrorCode::ZeroAmount);

        // 0. Pool-level rails: the accounts the policy demands must be present
        //    and canonical, and the ones it does not demand must be this
        //    program's own ID sentinel (so a caller cannot smuggle an unused
        //    rail's accounts past the policy).
        let settlement_on = pool.hooks_authority != Pubkey::default();
        if pool.kyc_config != Pubkey::default() {
            require_keys_eq!(
                ctx.accounts.kyc_config.key(),
                pool.kyc_config,
                ErrorCode::WrongKycConfig
            );
        } else {
            require_keys_eq!(
                ctx.accounts.kyc_config.key(),
                crate::ID,
                ErrorCode::RailAccountUnexpected
            );
            require_keys_eq!(
                ctx.accounts.user_kyc_account.key(),
                crate::ID,
                ErrorCode::RailAccountUnexpected
            );
        }
        if pool.oracle_state != Pubkey::default() {
            let (expected, _) = Pubkey::find_program_address(
                &[b"oracle", pool.key().as_ref()],
                &oracle::ID,
            );
            require_keys_eq!(
                ctx.accounts.oracle_state.key(),
                expected,
                ErrorCode::WrongOracleState
            );
        } else {
            require_keys_eq!(
                ctx.accounts.oracle_state.key(),
                crate::ID,
                ErrorCode::RailAccountUnexpected
            );
        }
        if settlement_on {
            let (expected_hooks, _) =
                Pubkey::find_program_address(&[b"hook_authority"], &hooks::ID);
            require_keys_eq!(
                ctx.accounts.hooks_authority.key(),
                expected_hooks,
                ErrorCode::WrongHooksAuthority
            );
            require!(trade_id != 0, ErrorCode::InvalidTradeId);
        } else {
            require!(trade_id == 0, ErrorCode::InvalidTradeId);
            for rail in [
                ctx.accounts.hooks_authority.as_ref(),
                ctx.accounts.custody_token_account.as_ref(),
                ctx.accounts.settlement_config.as_ref(),
                ctx.accounts.settlement_program.as_ref(),
                ctx.accounts.settlement_vault.as_ref(),
                ctx.accounts.settlement_vault_token.as_ref(),
                ctx.accounts.associated_token_program.as_ref(),
            ] {
                require_keys_eq!(
                    rail.key(),
                    crate::ID,
                    ErrorCode::RailAccountUnexpected
                );
            }
        }

        // 0a. KYC rail: the trader must hold a valid attestation before any
        //     math runs. `verify_kyc` is read-only on the gate side and
        //     requires the trader to be a signer — which they are, since they
        //     signed this swap.
        if pool.kyc_config != Pubkey::default() {
            let cpi_accounts = kyc_gate::cpi::accounts::VerifyKyc {
                kyc_config: ctx.accounts.kyc_config.to_account_info(),
                user: ctx.accounts.user.to_account_info(),
                user_kyc_account: ctx.accounts.user_kyc_account.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(
                kyc_gate::ID,
                cpi_accounts,
            );
            kyc_gate::cpi::verify_kyc(cpi_ctx)?;
        }

        // 0b. Oracle rail: circuit-break the trade before it is priced. The
        //     start price is what the walk will begin from; a price far from
        //     the 30-minute TWAP reverts the whole transaction here.
        if pool.oracle_state != Pubkey::default() {
            let start_price = pool.sqrt_price_x64;
            let cpi_accounts = oracle::cpi::accounts::CheckDeviation {
                oracle_state: ctx.accounts.oracle_state.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(
                oracle::ID,
                cpi_accounts,
            );
            oracle::cpi::check_deviation(cpi_ctx, start_price)?;
        }

        // 1. Check the supplied walk against the bitmap. Reading the order off the
        // pool instead of trusting the caller is what makes the walk sound:
        // skipping a tick would leave `pool.liquidity` stale, and from then on
        // every trade would be priced against the wrong active set. A down walk
        // may start on the current cell (the price can rest exactly on a boundary,
        // and that boundary has to be crossed); an up walk starts above it,
        // because the ranges beginning at the current tick are already active.
        let spacing = pool.tick_spacing;
        let cells = pool.bitmap_cells;
        let origin = bit_of(pool.tick_current, spacing);
        let mut expected = if is_a_to_b {
            prev_set(&pool.tick_bitmap, cells, origin)
        } else {
            next_set(&pool.tick_bitmap, cells, origin + 1)
        };

        let mut walk: Vec<(i32, usize)> = Vec::with_capacity(ticks.len());
        for (i, info) in ticks.iter().enumerate() {
            require!(info.is_writable, ErrorCode::TickNotWritable);
            let tick = Account::<TickState>::try_from(info)?;
            require!(tick.pool == pool.key(), ErrorCode::WrongTickAccount);

            // The tick carries the bump it was created with, so proving it is the
            // canonical PDA costs one hash instead of a `find_program_address`
            // search per tick in the walk.
            let canonical = Pubkey::create_program_address(
                &[
                    b"clmm-tick",
                    pool.key().as_ref(),
                    &tick.tick_index.to_le_bytes(),
                    &[tick.bump],
                ],
                &crate::ID,
            )
            .map_err(|_| error!(ErrorCode::WrongTickAccount))?;
            require!(canonical == info.key(), ErrorCode::WrongTickAccount);

            // The bitmap, not the caller, decides which tick comes next.
            let bit = bit_of(tick.tick_index, spacing);
            require!(
                bit >= 0 && bit < i64::from(cells) && Some(bit as u32) == expected,
                ErrorCode::TicksNotInOrder
            );
            walk.push((tick.tick_index, i));
            expected = walk_next(&pool.tick_bitmap, cells, is_a_to_b, bit as u32);
        }

        // 2. Walk the curve. Each iteration either consumes the rest of the input
        // or reaches (and crosses) the next supplied tick, so the loop is bounded
        // by the number of supplied ticks.
        let mut remaining = u128::from(amount_in);
        let mut total_out: u128 = 0;
        let mut cursor = 0usize;

        while remaining > 0 && cursor < walk.len() {
            let (next_tick_index, account_index) = walk[cursor];
            let sqrt_price_target =
                math_core::sqrt_price_at_tick(next_tick_index).map_err(math_err)?;

            // The price already rests exactly on this boundary: a previous trade
            // landed here, or the pool was initialized on it. It still has to be
            // crossed, consuming no input — that is what activates (or
            // deactivates) the ranges pinned to it, whichever way the price goes.
            if sqrt_price_target == pool.sqrt_price_x64 {
                let mut tick = Account::<TickState>::try_from(&ticks[account_index])?;
                cross_tick(pool, &mut tick, is_a_to_b)?;
                tick.exit(&crate::ID)?;
                cursor += 1;
                continue;
            }

            if pool.liquidity == 0 {
                // Nothing to trade against between here and the next boundary, and
                // the price cannot reach it without a step.
                break;
            }

            let step = math_core::compute_swap_step(
                pool.sqrt_price_x64,
                sqrt_price_target,
                pool.liquidity,
                remaining,
                pool.fee_pips,
                true, // exact_in
            )
            .map_err(math_err)?;

            // A dust budget (or liquidity so deep that one raw unit buys no
            // price movement at all) produces a zero-width step. Stop and
            // leave the dust with the user rather than spinning forever.
            if step.amount_in == 0 && step.amount_out == 0 {
                break;
            }

            // Apply the step to the pool state. `math_core` reports the fee on
            // the whole offered amount, which would overcharge a step that
            // reaches the tick boundary with input to spare, so the fee is
            // recomputed on the input the step actually consumed (Uniswap v3
            // semantics: fee = ceil(amount_in * f / (1 - f))).
            let fee_amount =
                fee_on_input(step.amount_in, pool.fee_pips).map_err(math_err)?;
            pool.sqrt_price_x64 = step.sqrt_price_next;
            remaining = remaining
                .checked_sub(step.amount_in)
                .and_then(|r| r.checked_sub(fee_amount))
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
            total_out = total_out
                .checked_add(step.amount_out)
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;

            // Credit this step's fee to the LPs of the active range. Selling A
            // pays token A, selling B pays token B.
            if fee_amount > 0 {
                let fee_growth_delta = fee_amount
                    .checked_mul(Q64)
                    .and_then(|v| v.checked_div(pool.liquidity))
                    .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
                if is_a_to_b {
                    pool.fee_growth_global_a = pool
                        .fee_growth_global_a
                        .checked_add(fee_growth_delta)
                        .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
                } else {
                    pool.fee_growth_global_b = pool
                        .fee_growth_global_b
                        .checked_add(fee_growth_delta)
                        .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
                }
            }

            // 3. Cross the tick when the step reached the boundary. Otherwise the
            // step stopped strictly inside the range, so `tick_current` has to
            // follow the price to wherever it landed — which is not necessarily a
            // stored tick. Leaving it behind here would make every later in-range
            // and fee-inside test read the wrong side of a boundary.
            if step.sqrt_price_next == sqrt_price_target {
                let mut tick = Account::<TickState>::try_from(&ticks[account_index])?;
                cross_tick(pool, &mut tick, is_a_to_b)?;
                tick.exit(&crate::ID)?;
                cursor += 1;
            } else {
                pool.tick_current =
                    math_core::tick_at_sqrt_price(pool.sqrt_price_x64).map_err(math_err)?;
            }
        }

        // 4. Settle: pull only what was consumed (input + fees), pay the output.
        let consumed = u64::try_from(u128::from(amount_in) - remaining)
            .map_err(|_| error!(ErrorCode::MathOverflow))?;
        let amount_out =
            u64::try_from(total_out).map_err(|_| error!(ErrorCode::MathOverflow))?;
        require!(consumed > 0, ErrorCode::InsufficientLiquidity);
        require!(amount_out >= min_amount_out, ErrorCode::SlippageExceeded);

        // Copy everything the CPIs and transfers need out of `pool` so the
        // mutable borrow ends before they borrow the accounts.
        let (mint_a_key, mint_b_key, pool_bump, settlement_slots, oracle_on) = (
            pool.mint_a,
            pool.mint_b,
            pool.bump,
            pool.settlement_slots,
            pool.oracle_state != Pubkey::default(),
        );
        let pool_seeds: [&[u8]; 4] = [
            b"clmm-pool",
            mint_a_key.as_ref(),
            mint_b_key.as_ref(),
            &[pool_bump],
        ];

        // 4a. Oracle rail: record the settled price so the TWAP stays current
        //     for the next trade's circuit-breaker check. `record_observation`
        //     requires the pool PDA as a signer; this CPI supplies it via the
        //     pool's own seeds, inside the transaction the pool is executing.
        if oracle_on {
            let settled_price = pool.sqrt_price_x64;
            let cpi_accounts = oracle::cpi::accounts::RecordObservation {
                oracle_state: ctx.accounts.oracle_state.to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            };
            let signer_seeds: &[&[&[u8]]] = &[&pool_seeds];
            let cpi_ctx = CpiContext::new(
                oracle::ID,
                cpi_accounts,
            )
            .with_signer(signer_seeds);
            oracle::cpi::record_observation(cpi_ctx, settled_price)?;
        }

        let (source_vault, dest_vault, user_source, user_dest) = if is_a_to_b {
            (
                &ctx.accounts.vault_a,
                &ctx.accounts.vault_b,
                &ctx.accounts.user_token_a,
                &ctx.accounts.user_token_b,
            )
        } else {
            (
                &ctx.accounts.vault_b,
                &ctx.accounts.vault_a,
                &ctx.accounts.user_token_b,
                &ctx.accounts.user_token_a,
            )
        };

        // Transfer input in (user signs).
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                SplTransfer {
                    from: user_source.to_account_info(),
                    to: source_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            consumed,
        )?;

        // Output destination: the trader's wallet — or, when settlement
        // routing is on, the hook authority's custody ATA, which the same
        // transaction immediately sweeps into the escrow. The trader never
        // holds the output; `min_amount_out` was checked against the exact
        // amount leaving the pool's vault either way.
        let output_destination = if settlement_on {
            ctx.accounts.custody_token_account.to_account_info()
        } else {
            user_dest.to_account_info()
        };
        let signer_seeds: &[&[&[u8]]] = &[&pool_seeds];
        let pool_info = ctx.accounts.pool.to_account_info();
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                SplTransfer {
                    from: dest_vault.to_account_info(),
                    to: output_destination,
                    authority: pool_info,
                },
                signer_seeds,
            ),
            amount_out,
        )?;

        // 5. Lock the routed output in the T+N settlement escrow through the
        //    hooks program. `hooks::after_swap` CPIs the settlement program,
        //    which creates the escrow accounts under this trade's vault PDA
        //    (unlocking at `Clock::slot + settlement_slots`) and sweeps the
        //    custody balance — exactly this trade's output — into it. The
        //    taker is the trader who sold here, so the Transfer Agent's
        //    signature — which binds (trade_id, taker, mint, amount) —
        //    releases to the same party that executed the swap.
        if settlement_on {
            let output_mint_account = if is_a_to_b {
                ctx.accounts.mint_b.to_account_info()
            } else {
                ctx.accounts.mint_a.to_account_info()
            };
            let cpi_accounts = hooks::cpi::accounts::AfterSwap {
                hook_authority: ctx.accounts.hooks_authority.to_account_info(),
                taker: ctx.accounts.user.to_account_info(),
                amm_vault: ctx.accounts.custody_token_account.to_account_info(),
                token_mint: output_mint_account,
                settlement_vault: ctx.accounts.settlement_vault.to_account_info(),
                settlement_vault_token: ctx.accounts.settlement_vault_token.to_account_info(),
                settlement_config: ctx.accounts.settlement_config.to_account_info(),
                settlement_program: ctx.accounts.settlement_program.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                associated_token_program: ctx.accounts.associated_token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            // The CPI callee is the hooks program; the settlement program
            // account in the list is the hooks program's own CPI target.
            let cpi_ctx = CpiContext::new(hooks::ID, cpi_accounts);
            hooks::cpi::after_swap(cpi_ctx, trade_id, settlement_slots)?;
        }

        msg!("Swap executed. In: {}, Out: {}", consumed, amount_out);
        Ok(())
    }
}

// -----------------------------------------------------------------------
// MATH HELPERS
// -----------------------------------------------------------------------

/// Token amounts (A, B) a position of `liquidity` over `[sqrt_lower,
/// sqrt_upper]` holds at the pool's current price. `round_up` selects the
/// direction used for deposits (round up: the user pays) vs withdrawals
/// (round down: the pool pays).
fn position_amounts(
    pool: &Pool,
    sqrt_lower: u128,
    sqrt_upper: u128,
    liquidity: u128,
    round_up: bool,
) -> math_core::MathResult<(u128, u128)> {
    let sqrt_price = pool.sqrt_price_x64;
    if sqrt_price <= sqrt_lower {
        // Price at or below the range: the position is entirely token A.
        Ok((
            math_core::get_amount_0_delta(sqrt_lower, sqrt_upper, liquidity, round_up)?,
            0,
        ))
    } else if sqrt_price < sqrt_upper {
        // Price inside the range: the current price is one endpoint.
        Ok((
            math_core::get_amount_0_delta(sqrt_lower, sqrt_price, liquidity, round_up)?,
            math_core::get_amount_1_delta(sqrt_price, sqrt_upper, liquidity, round_up)?,
        ))
    } else {
        // Price at or above the range: the position is entirely token B.
        Ok((
            0,
            math_core::get_amount_1_delta(sqrt_lower, sqrt_upper, liquidity, round_up)?,
        ))
    }
}

/// Fees earned inside `[lower, upper]` at the current tick, for both tokens,
/// as (global - below - above). The tick "outside" mirrors make this a pair
/// of subtractions; wrapping arithmetic is intentional (Uniswap v3).
fn fee_growth_inside(
    lower: &TickState,
    upper: &TickState,
    tick_current: i32,
    growth_global_a: u128,
    growth_global_b: u128,
) -> (u128, u128) {
    let below_a = if tick_current >= lower.tick_index {
        lower.fee_growth_outside_a
    } else {
        growth_global_a.wrapping_sub(lower.fee_growth_outside_a)
    };
    let above_a = if tick_current < upper.tick_index {
        upper.fee_growth_outside_a
    } else {
        growth_global_a.wrapping_sub(upper.fee_growth_outside_a)
    };
    let below_b = if tick_current >= lower.tick_index {
        lower.fee_growth_outside_b
    } else {
        growth_global_b.wrapping_sub(lower.fee_growth_outside_b)
    };
    let above_b = if tick_current < upper.tick_index {
        upper.fee_growth_outside_b
    } else {
        growth_global_b.wrapping_sub(upper.fee_growth_outside_b)
    };
    (
        growth_global_a.wrapping_sub(below_a).wrapping_sub(above_a),
        growth_global_b.wrapping_sub(below_b).wrapping_sub(above_b),
    )
}

/// Moves fees earned since the position's last snapshot into `tokens_owed`,
/// then re-bases the snapshot. `liquidity * delta` stays inside u128 because
/// the delta is bounded by (fees paid * 2^64 / pool liquidity) and fees are
/// u64 token amounts.
fn accrue_fees(position: &mut Position, inside_a: u128, inside_b: u128) -> Result<()> {
    if position.liquidity > 0 {
        let earned_a = position
            .liquidity
            .wrapping_mul(inside_a.wrapping_sub(position.fee_growth_inside_a))
            / Q64;
        let earned_b = position
            .liquidity
            .wrapping_mul(inside_b.wrapping_sub(position.fee_growth_inside_b))
            / Q64;
        let earned_a = u64::try_from(earned_a).map_err(|_| error!(ErrorCode::MathOverflow))?;
        let earned_b = u64::try_from(earned_b).map_err(|_| error!(ErrorCode::MathOverflow))?;
        position.tokens_owed_a = position
            .tokens_owed_a
            .checked_add(earned_a)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        position.tokens_owed_b = position
            .tokens_owed_b
            .checked_add(earned_b)
            .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
    }
    position.fee_growth_inside_a = inside_a;
    position.fee_growth_inside_b = inside_b;
    Ok(())
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES
// -----------------------------------------------------------------------

#[account]
pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    /// Uniswap v3 convention: the tick the price is in, except that a down-cross
    /// stores the tick *below* the boundary it crossed (see `cross_tick`).
    pub tick_current: i32,
    pub tick_spacing: i32,
    pub fee_pips: u32,
    pub fee_growth_global_a: u128,
    pub fee_growth_global_b: u128,
    pub bump: u8,
    /// Cells in `tick_bitmap`.
    pub bitmap_cells: u32,
    /// One bit per cell, set while the tick at that cell carries liquidity. This
    /// is what lets a swap prove it did not skip an initialized tick, and it is
    /// kept in step with the TickState accounts: set by `add_liquidity`, cleared
    /// by `remove_liquidity` once a tick's last position leaves.
    pub tick_bitmap: Vec<u8>,
    // -----------------------------------------------------------------
    // Pool-level wiring (all optional; set at `initialize_pool`).
    // -----------------------------------------------------------------

    /// KYC gate config PDA (`kyc_gate::KYC_CONFIG_SEED`). `Pubkey::default()`
    /// means the pool does not require KYC; otherwise every swap CPIs
    /// `kyc_gate::verify_kyc` for the trader first.
    pub kyc_config: Pubkey,
    /// Oracle state PDA (`oracle` program, seed `[b"oracle", pool_key]`).
    /// When set, every swap CPIs `check_deviation` with the price it is about
    /// to trade at and `record_observation` after the trade settles.
    pub oracle_state: Pubkey,
    /// Hooks program authority PDA (`[b"hook_authority"]`). When set, swap
    /// output is NOT paid to the trader: it is routed to a per-trade custody
    /// account owned by this PDA and locked in the T+N settlement escrow.
    pub hooks_authority: Pubkey,
    /// T+N length in slots, relative to the swap slot, for trades routed
    /// through settlement. Only meaningful when `hooks_authority` is set.
    pub settlement_slots: u64,
}

impl Pool {
    /// Everything but the bitmap itself: the 8-byte discriminator, the fixed
    /// fields, `bitmap_cells` and the Borsh length prefix of `tick_bitmap` —
    /// plus the wiring fields (two pubkeys, a pubkey, and a u64).
    pub const FIXED_LEN: usize =
        8 + 32 + 32 + 16 + 16 + 4 + 4 + 4 + 16 + 16 + 1 + 4 + 4 + 32 + 32 + 32 + 8;

    /// Account size for a pool with this tick spacing. The bitmap is sized to
    /// the spacing, so a pool only pays for the grid it actually uses.
    pub const fn len_for(tick_spacing: i32) -> usize {
        Self::FIXED_LEN + bitmap_bytes(bitmap_cells(tick_spacing))
    }
}

#[account]
pub struct TickState {
    pub pool: Pubkey,
    pub tick_index: i32,
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub fee_growth_outside_a: u128,
    pub fee_growth_outside_b: u128,
    /// The bump this tick PDA was created with. Storing it lets a swap verify the
    /// account is the canonical tick PDA with a single hash instead of a
    /// `find_program_address` search per tick in the walk.
    pub bump: u8,
}

impl TickState {
    pub const LEN: usize = 8 + 32 + 4 + 16 + 16 + 16 + 16 + 1;
}

#[account]
pub struct Position {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub lower_tick: i32,
    pub upper_tick: i32,
    pub liquidity: u128,
    /// Fee-growth snapshot inside the range at the last touch, used with
    /// `liquidity` to credit `tokens_owed`.
    pub fee_growth_inside_a: u128,
    pub fee_growth_inside_b: u128,
    pub tokens_owed_a: u64,
    pub tokens_owed_b: u64,
}

impl Position {
    pub const LEN: usize = 8 + 32 + 32 + 4 + 4 + 16 + 16 + 16 + 8 + 8;
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(initial_sqrt_price_x64: u128, tick_spacing: i32, fee_pips: u32)]
pub struct InitializePool<'info> {
    #[account(init, payer = authority, space = Pool::len_for(tick_spacing), seeds = [b"clmm-pool", mint_a.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub pool: Account<'info, Pool>,

    /// Pool-owned vault for token A. The pool PDA is the authority so it can
    /// sign withdrawals during swaps and liquidity removal.
    #[account(
        init,
        payer = authority,
        seeds = [b"clmm-vault-a", pool.key().as_ref(), mint_a.key().as_ref()],
        bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Account<'info, TokenAccount>,

    /// Pool-owned vault for token B.
    #[account(
        init,
        payer = authority,
        seeds = [b"clmm-vault-b", pool.key().as_ref(), mint_b.key().as_ref()],
        bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Account<'info, TokenAccount>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,

    #[account(mut)]
    pub authority: Signer<'info>,

    /// The KYC gate program. Required whenever `kyc_config` is set, so a
    /// pool cannot record a "KYC-required" policy backed by a look-alike
    /// program. Pass the KYC program itself when leaving KYC off.
    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    /// The oracle program, required whenever `oracle_state` is set.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,

    /// The hooks program, required whenever `hooks_authority` is set.
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(lower_tick: i32, upper_tick: i32, liquidity_amount: u128)]
pub struct AddLiquidity<'info> {
    #[account(mut, seeds = [b"clmm-pool", mint_a.key().as_ref(), mint_b.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    // Lower Tick Account
    #[account(init_if_needed, payer = user, space = TickState::LEN, seeds = [b"clmm-tick", pool.key().as_ref(), &lower_tick.to_le_bytes()], bump)]
    pub lower_tick_state: Account<'info, TickState>,

    // Upper Tick Account
    #[account(init_if_needed, payer = user, space = TickState::LEN, seeds = [b"clmm-tick", pool.key().as_ref(), &upper_tick.to_le_bytes()], bump)]
    pub upper_tick_state: Account<'info, TickState>,

    #[account(init_if_needed, payer = user, space = Position::LEN, seeds = [b"clmm-pos", pool.key().as_ref(), user.key().as_ref(), &lower_tick.to_le_bytes(), &upper_tick.to_le_bytes()], bump)]
    pub position: Account<'info, Position>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,

    #[account(mut, seeds = [b"clmm-vault-a", pool.key().as_ref(), mint_a.key().as_ref()], bump)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"clmm-vault-b", pool.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_token_a.mint == pool.mint_a @ ErrorCode::WrongMint)]
    pub user_token_a: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_token_b.mint == pool.mint_b @ ErrorCode::WrongMint)]
    pub user_token_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(lower_tick: i32, upper_tick: i32, liquidity_amount: u128, min_amount_a: u64, min_amount_b: u64)]
pub struct RemoveLiquidity<'info> {
    #[account(mut, seeds = [b"clmm-pool", mint_a.key().as_ref(), mint_b.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"clmm-pos", pool.key().as_ref(), user.key().as_ref(), &lower_tick.to_le_bytes(), &upper_tick.to_le_bytes()],
        bump,
        constraint = position.owner == user.key() @ ErrorCode::Unauthorized,
    )]
    pub position: Account<'info, Position>,

    #[account(
        mut,
        seeds = [b"clmm-tick", pool.key().as_ref(), &lower_tick.to_le_bytes()],
        bump,
        constraint = lower_tick_state.tick_index == lower_tick @ ErrorCode::WrongTickAccount,
    )]
    pub lower_tick_state: Account<'info, TickState>,

    #[account(
        mut,
        seeds = [b"clmm-tick", pool.key().as_ref(), &upper_tick.to_le_bytes()],
        bump,
        constraint = upper_tick_state.tick_index == upper_tick @ ErrorCode::WrongTickAccount,
    )]
    pub upper_tick_state: Account<'info, TickState>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,

    #[account(mut, seeds = [b"clmm-vault-a", pool.key().as_ref(), mint_a.key().as_ref()], bump)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"clmm-vault-b", pool.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_token_a.mint == pool.mint_a @ ErrorCode::WrongMint)]
    pub user_token_a: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_token_b.mint == pool.mint_b @ ErrorCode::WrongMint)]
    pub user_token_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(lower_tick: i32, upper_tick: i32)]
pub struct CollectFees<'info> {
    #[account(seeds = [b"clmm-pool", mint_a.key().as_ref(), mint_b.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    #[account(
        mut,
        seeds = [b"clmm-pos", pool.key().as_ref(), user.key().as_ref(), &lower_tick.to_le_bytes(), &upper_tick.to_le_bytes()],
        bump,
        constraint = position.owner == user.key() @ ErrorCode::Unauthorized,
    )]
    pub position: Account<'info, Position>,

    // Read-only: fee accounting only inspects the outside mirrors.
    #[account(
        seeds = [b"clmm-tick", pool.key().as_ref(), &lower_tick.to_le_bytes()],
        bump,
        constraint = lower_tick_state.tick_index == lower_tick @ ErrorCode::WrongTickAccount,
    )]
    pub lower_tick_state: Account<'info, TickState>,

    #[account(
        seeds = [b"clmm-tick", pool.key().as_ref(), &upper_tick.to_le_bytes()],
        bump,
        constraint = upper_tick_state.tick_index == upper_tick @ ErrorCode::WrongTickAccount,
    )]
    pub upper_tick_state: Account<'info, TickState>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,

    #[account(mut, seeds = [b"clmm-vault-a", pool.key().as_ref(), mint_a.key().as_ref()], bump)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"clmm-vault-b", pool.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_token_a.mint == pool.mint_a @ ErrorCode::WrongMint)]
    pub user_token_a: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_token_b.mint == pool.mint_b @ ErrorCode::WrongMint)]
    pub user_token_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(mut, seeds = [b"clmm-pool", mint_a.key().as_ref(), mint_b.key().as_ref()], bump = pool.bump)]
    pub pool: Account<'info, Pool>,

    pub mint_a: Account<'info, Mint>,
    pub mint_b: Account<'info, Mint>,

    #[account(mut, seeds = [b"clmm-vault-a", pool.key().as_ref(), mint_a.key().as_ref()], bump)]
    pub vault_a: Account<'info, TokenAccount>,
    #[account(mut, seeds = [b"clmm-vault-b", pool.key().as_ref(), mint_b.key().as_ref()], bump)]
    pub vault_b: Account<'info, TokenAccount>,

    #[account(mut, constraint = user_token_a.mint == pool.mint_a @ ErrorCode::WrongMint)]
    pub user_token_a: Account<'info, TokenAccount>,
    #[account(mut, constraint = user_token_b.mint == pool.mint_b @ ErrorCode::WrongMint)]
    pub user_token_b: Account<'info, TokenAccount>,

    #[account(mut)]
    pub user: Signer<'info>,

    /// KYC gate config, required iff `pool.kyc_config` is set (pass the pool
    /// PDA itself when the rail is off). The handler checks it against the
    /// pool's recorded policy; the KYC program validates its structure in the
    /// CPI.
    /// CHECK: equality-checked against `pool.kyc_config` in the handler.
    #[account(mut)]
    pub kyc_config: UncheckedAccount<'info>,

    /// The trader's KYC attestation account, required iff the KYC rail is on.
    /// CHECK: validated inside `kyc_gate::verify_kyc` via CPI.
    pub user_kyc_account: UncheckedAccount<'info>,

    /// Oracle state, required iff `pool.oracle_state` is set. The handler
    /// CPIs `check_deviation` with the pre-trade price and
    /// `record_observation` with the post-trade price.
    /// CHECK: seeds-verified as `[b"oracle", pool]` in the handler.
    #[account(mut)]
    pub oracle_state: UncheckedAccount<'info>,

    /// Hooks authority PDA, required iff `pool.hooks_authority` is set. Owns
    /// the custody account the swap output is routed into.
    /// CHECK: seeds-verified as the hooks program's `[b"hook_authority"]`
    /// PDA in the handler.
    pub hooks_authority: UncheckedAccount<'info>,

    /// The settlement custody account — the hook authority's ATA for the
    /// output mint — required iff the settlement rail is on. It receives the
    /// swap output; the same transaction then routes it into escrow, so its
    /// balance at hook time is exactly this trade's output.
    /// CHECK: address-pinned to the hook authority's ATA in the handler; the
    /// settlement program re-checks it as the escrow's source.
    #[account(mut)]
    pub custody_token_account: UncheckedAccount<'info>,

    /// The settlement clearinghouse config, required iff the settlement rail
    /// is on. Passed through to the hooks CPI.
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub settlement_config: UncheckedAccount<'info>,

    /// The settlement program, required iff the settlement rail is on.
    /// CHECK: validated by the hooks program (typed `Program<Settlement>`).
    pub settlement_program: UncheckedAccount<'info>,

    /// The settlement vault PDA for `trade_id`, required iff the settlement
    /// rail is on. The hooks program re-derives and checks it.
    /// CHECK: seeds-verified by the hooks program against `trade_id`.
    #[account(mut)]
    pub settlement_vault: UncheckedAccount<'info>,

    /// The escrow token account of the settlement vault PDA, required iff
    /// the settlement rail is on. Created by the settlement program.
    /// CHECK: address-pinned to the vault's ATA by the hooks program.
    #[account(mut)]
    pub settlement_vault_token: UncheckedAccount<'info>,

    /// The associated-token program, required iff the settlement rail is on
    /// (the settlement program creates the escrow ATA through it).
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub associated_token_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,

    // Remaining accounts: the initialized `TickState` PDAs the swap may cross,
    // in walk order (descending when selling A, ascending when selling B).
    // Validated in `swap`.
}

#[error_code]
pub enum ErrorCode {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid ticks")]
    InvalidTicks,
    #[msg("Tick spacing mismatch: ticks must be multiples of the pool's tick spacing")]
    InvalidTickSpacing,
    #[msg("Sqrt price out of bounds")]
    SqrtPriceOutOfBounds,
    #[msg("Fee too high")]
    FeeTooHigh,
    #[msg("Zero liquidity")]
    ZeroLiquidity,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Slippage exceeded")]
    SlippageExceeded,
    #[msg("Insufficient liquidity to fill the swap")]
    InsufficientLiquidity,
    #[msg("Not enough liquidity in the position")]
    InsufficientShares,
    #[msg("Position does not belong to this owner")]
    Unauthorized,
    #[msg("Token mint does not match the pool")]
    WrongMint,
    #[msg("Tick account is not a canonical, writable tick PDA of this pool")]
    WrongTickAccount,
    #[msg("Tick accounts must be supplied in the order the swap walks them")]
    TicksNotInOrder,
    #[msg("Tick account must be passed as writable")]
    TickNotWritable,
    #[msg("Tick spacing is too fine to fit the pool's tick bitmap")]
    TickSpacingTooFine,
    #[msg("Tick falls outside the pool's tick bitmap")]
    TickOutOfBitmap,
    #[msg("Position account does not match its owner or its range")]
    WrongPosition,
    #[msg("Settlement routing policy is inconsistent: hooks authority and settlement slots must be set together")]
    InvalidSettlementSlots,
    #[msg("KYC config account does not match the pool's recorded KYC policy")]
    WrongKycConfig,
    #[msg("Oracle state account does not match the pool's recorded oracle policy")]
    WrongOracleState,
    #[msg("Hooks authority account is not the hooks program's hook_authority PDA")]
    WrongHooksAuthority,
    #[msg("A rail account must be this program's own ID when its rail is disabled")]
    RailAccountUnexpected,
    #[msg("Trade id must be non-zero when settlement routing is on, zero when off")]
    InvalidTradeId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::AccountSerialize;

    const FEE_30BPS: u32 = 3_000;
    const D: u128 = math_core::FEE_DENOMINATOR as u128;

    fn tick(index: i32, outside_a: u128, outside_b: u128) -> TickState {
        TickState {
            pool: Pubkey::default(),
            tick_index: index,
            liquidity_gross: 0,
            liquidity_net: 0,
            fee_growth_outside_a: outside_a,
            fee_growth_outside_b: outside_b,
            bump: 0,
        }
    }

    // ---------------------------------------------------------------
    // fee_on_input
    // ---------------------------------------------------------------

    #[test]
    fn fee_is_zero_without_fee_or_input() {
        assert_eq!(fee_on_input(0, FEE_30BPS).unwrap(), 0);
        assert_eq!(fee_on_input(1_000_000, 0).unwrap(), 0);
    }

    #[test]
    fn fee_matches_the_documented_formula() {
        // ceil(1_000_000 * 3_000 / 997_000) = ceil(3_009.027) = 3_010
        // (not the naive floor of 3_000).
        assert_eq!(fee_on_input(1_000_000, FEE_30BPS).unwrap(), 3_010);
    }

    #[test]
    fn fee_on_a_one_unit_input_rounds_up_to_one() {
        assert_eq!(fee_on_input(1, FEE_30BPS).unwrap(), 1);
        // 3 pips on one unit is 1, but 0 pips stays free.
        assert_eq!(fee_on_input(1, 3).unwrap(), 1);
    }

    #[test]
    fn consumed_plus_fee_never_exceeds_the_offered_amount() {
        // The walk's budget identity: whenever the step consumed `amount_in`
        // out of an offered `offered` (i.e. amount_in <= the net of the
        // offered amount), the taken input plus its fee fits in `offered`.
        for fee_pips in [0u32, 1, 3, 100, FEE_30BPS, 100_000, 500_000] {
            let d = u128::from(math_core::FEE_DENOMINATOR - fee_pips);
            for offered in 1u128..=5_000 {
                let net = offered * d / D; // floor(offered * (1 - f))
                // A step only ever consumes `amount_in <= net`: reaching the
                // target consumes at most the net budget, and a partial price
                // move is driven by the net itself. `net == 0` means the
                // offered dust buys no price movement at all (a no-op step,
                // which the walk breaks out of instead of charging).
                for amount_in in 0..=net {
                    let fee = fee_on_input(amount_in, fee_pips).unwrap();
                    assert!(
                        amount_in + fee <= offered,
                        "f={fee_pips} offered={offered} in={amount_in} fee={fee}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_walk_budget_holds_for_real_steps() {
        // The swap loop charges `step.amount_in + fee_on_input(step.amount_in)`
        // against the offered amount, so that sum must never exceed it, or the
        // walk underflows on a legitimate swap.
        let current = math_core::sqrt_price_at_tick(0).unwrap();
        for &fee_pips in &[0u32, 1, 3, 100, FEE_30BPS, 100_000, 500_000] {
            for &liquidity in &[1u128, 1_000, 1_000_000, L, 1u128 << 60] {
                for &target_tick in &[-50_000i32, -1_000, -1, 1, 1_000, 50_000] {
                    let target = math_core::sqrt_price_at_tick(target_tick).unwrap();
                    for offered in 1u128..=3_000 {
                        let step = math_core::compute_swap_step(
                            current, target, liquidity, offered, fee_pips, true,
                        )
                        .unwrap();
                        let fee = fee_on_input(step.amount_in, fee_pips).unwrap();
                        assert!(
                            step.amount_in + fee <= offered,
                            "fee={fee_pips} liquidity={liquidity} target={target_tick} offered={offered} in={} fee={fee}",
                            step.amount_in
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fee_grows_with_the_input_and_the_rate() {
        let a = fee_on_input(1_000, FEE_30BPS).unwrap();
        let b = fee_on_input(2_000, FEE_30BPS).unwrap();
        assert!(a < b);
        assert!(fee_on_input(1_000, 100).unwrap() < fee_on_input(1_000, 10_000).unwrap());
    }

    // ---------------------------------------------------------------
    // fee_growth_inside
    // ---------------------------------------------------------------

    #[test]
    fn inside_growth_excludes_below_and_above() {
        // Global growth of (1000, 2000); the lower tick's mirror says 300/600
        // was earned *below* it and the upper's 400/800 *above* it.
        let lower = tick(-100, 300, 600);
        let upper = tick(200, 400, 800);
        let (a, b) = fee_growth_inside(&lower, &upper, 0, 1_000, 2_000);
        assert_eq!((a, b), (300, 600));
    }

    #[test]
    fn inside_growth_when_the_price_sits_below_the_range() {
        // With the price below the range the mirrors read
        // lower.outside - upper.outside, so the state must be consistent with
        // that configuration (lower.outside >= upper.outside).
        let lower = tick(-100, 500, 900);
        let upper = tick(200, 400, 800);
        let (a, b) = fee_growth_inside(&lower, &upper, -200, 1_000, 2_000);
        assert_eq!((a, b), (100, 100));
    }

    #[test]
    fn inside_growth_when_the_price_sits_above_the_range() {
        let lower = tick(-100, 300, 600);
        let upper = tick(200, 400, 800);
        let (a, b) = fee_growth_inside(&lower, &upper, 300, 1_000, 2_000);
        assert_eq!((a, b), (100, 200));
    }

    #[test]
    fn inside_growth_is_non_negative_for_a_consistent_state() {
        // A freshly initialized pair at a price inside the range: both ticks
        // start with zero outside (they were initialized above the price).
        let lower = tick(-1_000, 0, 0);
        let upper = tick(1_000, 0, 0);
        let (a, b) = fee_growth_inside(&lower, &upper, 0, 777, 888);
        assert_eq!((a, b), (777, 888));
    }

    // ---------------------------------------------------------------
    // accrue_fees
    // ---------------------------------------------------------------

    fn position(liquidity: u128, snapshot_a: u128, snapshot_b: u128) -> Position {
        Position {
            pool: Pubkey::default(),
            owner: Pubkey::default(),
            lower_tick: 0,
            upper_tick: 0,
            liquidity,
            fee_growth_inside_a: snapshot_a,
            fee_growth_inside_b: snapshot_b,
            tokens_owed_a: 0,
            tokens_owed_b: 0,
        }
    }

    #[test]
    fn fees_accrue_from_liquidity_times_growth_delta() {
        let mut pos = position(1_000, 10 * Q64, 10 * Q64);
        accrue_fees(&mut pos, 15 * Q64, 13 * Q64).unwrap();
        // (15 - 10) * 1_000 / 2^64 = 5_000 token A; (13 - 10) -> 3_000 B.
        assert_eq!((pos.tokens_owed_a, pos.tokens_owed_b), (5_000, 3_000));
        // The snapshot is re-based, so accruing again earns nothing new.
        accrue_fees(&mut pos, 15 * Q64, 13 * Q64).unwrap();
        assert_eq!((pos.tokens_owed_a, pos.tokens_owed_b), (5_000, 3_000));
    }

    #[test]
    fn a_zero_liquidity_position_rebases_without_collecting() {
        let mut pos = position(0, 0, 0);
        accrue_fees(&mut pos, 5 * Q64, 5 * Q64).unwrap();
        assert_eq!((pos.tokens_owed_a, pos.tokens_owed_b), (0, 0));
        assert_eq!(pos.fee_growth_inside_a, 5 * Q64);
    }

    // ---------------------------------------------------------------
    // position_amounts
    // ---------------------------------------------------------------

    fn pool_at(sqrt_price: u128) -> Pool {
        Pool {
            mint_a: Pubkey::default(),
            mint_b: Pubkey::default(),
            sqrt_price_x64: sqrt_price,
            liquidity: 0,
            tick_current: 0,
            tick_spacing: 60,
            fee_pips: 0,
            fee_growth_global_a: 0,
            fee_growth_global_b: 0,
            bump: 255,
            bitmap_cells: bitmap_cells(60),
            tick_bitmap: vec![0u8; bitmap_bytes(bitmap_cells(60))],
            kyc_config: Pubkey::default(),
            oracle_state: Pubkey::default(),
            hooks_authority: Pubkey::default(),
            settlement_slots: 0,
        }
    }

    const L: u128 = 1_000_000_000;

    #[test]
    fn a_range_below_the_price_needs_only_token_b() {
        let p = pool_at(math_core::sqrt_price_at_tick(0).unwrap());
        let lower = math_core::sqrt_price_at_tick(-1_000).unwrap();
        let upper = math_core::sqrt_price_at_tick(-500).unwrap();
        let (a, b) = position_amounts(&p, lower, upper, L, false).unwrap();
        assert_eq!(a, 0);
        assert!(b > 0);
    }

    #[test]
    fn a_range_above_the_price_needs_only_token_a() {
        let p = pool_at(math_core::sqrt_price_at_tick(0).unwrap());
        let lower = math_core::sqrt_price_at_tick(500).unwrap();
        let upper = math_core::sqrt_price_at_tick(1_000).unwrap();
        let (a, b) = position_amounts(&p, lower, upper, L, false).unwrap();
        assert_eq!(b, 0);
        assert!(a > 0);
    }

    #[test]
    fn a_range_spanning_the_price_splits_at_the_current_price() {
        let p = pool_at(math_core::sqrt_price_at_tick(0).unwrap());
        let lower = math_core::sqrt_price_at_tick(-1_000).unwrap();
        let upper = math_core::sqrt_price_at_tick(1_000).unwrap();
        let (a, b) = position_amounts(&p, lower, upper, L, false).unwrap();
        assert!(a > 0 && b > 0);
        // Over a symmetric range around 1.0 the two legs are within a tenth
        // of a percent of each other (the tick's own 5e-5 asymmetry).
        assert!(a.abs_diff(b) * 1_000 <= b, "{a} vs {b}");
        // Rounding up (a deposit) is never smaller than rounding down.
        let (a_up, b_up) = position_amounts(&p, lower, upper, L, true).unwrap();
        assert!(a_up >= a && b_up >= b);
    }

    #[test]
    fn a_range_ending_at_the_price_takes_a_single_token() {
        let price = math_core::sqrt_price_at_tick(0).unwrap();
        let p = pool_at(price);
        let upper = math_core::sqrt_price_at_tick(1_000).unwrap();
        // Price exactly at the lower endpoint: token A only.
        let (a, b) = position_amounts(&p, price, upper, L, false).unwrap();
        assert!(a > 0 && b == 0);
        let lower = math_core::sqrt_price_at_tick(-1_000).unwrap();
        // Price exactly at the upper endpoint: token B only.
        let (a, b) = position_amounts(&p, lower, price, L, false).unwrap();
        assert!(a == 0 && b > 0);
    }

    // ---------------------------------------------------------------
    // tick bitmap
    // ---------------------------------------------------------------

    /// The lowest tick a pool with this spacing can use: the first multiple of
    /// the spacing at or above `MIN_TICK`.
    fn lowest_valid_tick(spacing: i32) -> i32 {
        let s = i64::from(spacing);
        ((i64::from(math_core::MIN_TICK).div_euclid(s) + 1) as i32) * spacing
    }

    #[test]
    fn valid_ticks_map_to_dense_distinct_bits() {
        // Every tick a pool accepts gets its own bit, in increasing order, and
        // all of them fit inside the bitmap.
        for spacing in [14, 60, 200] {
            let cells = bitmap_cells(spacing);
            assert!(cells <= BITMAP_BITS_MAX, "spacing {spacing}: {cells} cells");
            let mut previous = -1i64;
            let mut index = lowest_valid_tick(spacing);
            while index <= math_core::MAX_TICK {
                let bit = bit_of(index, spacing);
                assert!(bit >= 0 && bit < i64::from(cells), "tick {index} -> bit {bit}");
                assert!(bit > previous, "tick {index} does not advance the bit");
                previous = bit;
                index += spacing;
            }
        }
    }

    #[test]
    fn the_spacing_floor_keeps_a_pool_in_one_account() {
        assert!(bitmap_cells(14) <= BITMAP_BITS_MAX);
        assert!(bitmap_cells(13) > BITMAP_BITS_MAX);
        // The bitmap is sized to the spacing, so a coarse pool stays small.
        assert!(Pool::len_for(60) < Pool::len_for(14));
        // Finer than the floor would not fit an account `init` can create.
        assert!(Pool::len_for(14) < 10 * 1024);
    }

    #[test]
    fn the_pool_account_has_room_for_everything_it_serializes() {
        // `Pool::len_for` is the `space` an `initialize_pool` creates, and the
        // serialized pool has to fit inside it exactly — a short account makes
        // every `exit` fail.
        let mut pool = pool_at(math_core::sqrt_price_at_tick(0).unwrap());
        for spacing in [14, 60, 200] {
            pool.tick_spacing = spacing;
            pool.bitmap_cells = bitmap_cells(spacing);
            pool.tick_bitmap = vec![0u8; bitmap_bytes(bitmap_cells(spacing))];
            let mut buffer = Vec::new();
            pool.try_serialize(&mut buffer).unwrap();
            assert_eq!(buffer.len(), Pool::len_for(spacing), "spacing {spacing}");
        }
    }

    #[test]
    fn bitmap_scans_find_the_neighbours() {
        let cells = 256u32;
        let mut bits = vec![0u8; bitmap_bytes(cells)];
        for bit in [0u32, 1, 63, 64, 200] {
            assert!(bit_set(&mut bits, bit));
        }

        // Downward is inclusive of the starting cell and capped at the top.
        assert_eq!(prev_set(&bits, cells, 500), Some(200));
        assert_eq!(prev_set(&bits, cells, 200), Some(200));
        assert_eq!(prev_set(&bits, cells, 199), Some(64));
        assert_eq!(prev_set(&bits, cells, 64), Some(64));
        assert_eq!(prev_set(&bits, cells, 63), Some(63));
        assert_eq!(prev_set(&bits, cells, 62), Some(1));
        assert_eq!(prev_set(&bits, cells, 1), Some(1));
        assert_eq!(prev_set(&bits, cells, 0), Some(0));
        assert_eq!(prev_set(&bits, cells, -1), None);

        // Upward, and past the last cell there is nothing left.
        assert_eq!(next_set(&bits, cells, 0), Some(0));
        assert_eq!(next_set(&bits, cells, 2), Some(63));
        assert_eq!(next_set(&bits, cells, 200), Some(200));
        assert_eq!(next_set(&bits, cells, 201), None);
        assert_eq!(next_set(&bits, cells, 256), None);
        assert_eq!(next_set(&bits, cells, -1), None);

        // Clearing takes a tick out of both scans.
        bit_clear(&mut bits, 64);
        assert!(!bit_get(&bits, 64));
        assert_eq!(prev_set(&bits, cells, 199), Some(63));
        assert_eq!(next_set(&bits, cells, 2), Some(63));
    }

    #[test]
    fn an_empty_bitmap_has_no_neighbours() {
        let cells = 64u32;
        let mut bits = vec![0u8; bitmap_bytes(cells)];
        assert_eq!(prev_set(&bits, cells, 63), None);
        assert_eq!(next_set(&bits, cells, 0), None);
        // Cells past the end are not addressable, so they stay clear.
        assert!(!bit_set(&mut bits, 64));
        assert!(bit_set(&mut bits, 63));
    }

    #[test]
    fn a_stray_high_bit_never_escapes_the_cell_count() {
        let cells = 10u32;
        let mut bits = vec![0u8; bitmap_bytes(cells)];
        bits[1] |= 0x80; // bit 15, past the end of a 10-cell bitmap
        assert_eq!(next_set(&bits, cells, 0), None);
        assert_eq!(prev_set(&bits, cells, 9), None);
    }

    #[test]
    fn the_walk_advances_one_initialized_tick_at_a_time() {
        let spacing = 60;
        let cells = bitmap_cells(spacing);
        let mut bits = vec![0u8; bitmap_bytes(cells)];
        for index in [-1_200, -60, 0, 60, 1_200] {
            assert!(bit_set(&mut bits, bit_of(index, spacing) as u32));
        }
        let bit_of_all = |indices: [i32; 4]| -> Vec<u32> {
            indices
                .iter()
                .map(|index| bit_of(*index, spacing) as u32)
                .collect()
        };

        // Down from a price sitting in tick 120, which carries nothing itself.
        let mut seen = Vec::new();
        let mut bit = prev_set(&bits, cells, bit_of(120, spacing));
        while let Some(current) = bit {
            seen.push(current);
            bit = walk_next(&bits, cells, true, current);
        }
        assert_eq!(seen, bit_of_all([60, 0, -60, -1_200]));

        // Up from the same place skips everything below it.
        assert_eq!(
            next_set(&bits, cells, bit_of(120, spacing) + 1),
            Some(bit_of(1_200, spacing) as u32)
        );

        // Starting exactly on an initialized tick: a down walk has to cross that
        // very tick (zero-width) and must not offer it up again next.
        let on_boundary = prev_set(&bits, cells, bit_of(60, spacing));
        assert_eq!(on_boundary, Some(bit_of(60, spacing) as u32));
        assert_eq!(
            walk_next(&bits, cells, true, on_boundary.unwrap()),
            Some(bit_of(0, spacing) as u32)
        );
        // An up walk skips it instead: the ranges starting there are already live.
        assert_eq!(
            next_set(&bits, cells, bit_of(60, spacing) + 1),
            Some(bit_of(1_200, spacing) as u32)
        );
    }

    #[test]
    fn the_lowest_tick_has_nothing_below_it() {
        let spacing = 60;
        let cells = bitmap_cells(spacing);
        let mut bits = vec![0u8; bitmap_bytes(cells)];
        let lowest = lowest_valid_tick(spacing);
        assert!(bit_set(&mut bits, bit_of(lowest, spacing) as u32));

        // After the price is pushed through the lowest tick, `tick_current` holds
        // the tick below it, which has no bit. A further down walk therefore finds
        // nothing: the price cannot move any lower, and the walk stops cleanly
        // rather than crossing the same tick a second time.
        let below = bit_of(lowest - 1, spacing);
        assert!(below <= 0);
        assert_eq!(prev_set(&bits, cells, below), None);
        // Coming back up, the lowest tick is the first thing the price meets.
        assert_eq!(
            next_set(&bits, cells, below + 1),
            Some(bit_of(lowest, spacing) as u32)
        );
    }

    // ---------------------------------------------------------------
    // cross_tick
    // ---------------------------------------------------------------

    #[test]
    fn crossing_moves_the_active_set_and_records_the_tick_by_direction() {
        let mut pool = pool_at(math_core::sqrt_price_at_tick(0).unwrap());
        let mut state = tick(60, 7, 9);
        state.liquidity_net = 500;
        pool.liquidity = 1_000;
        pool.fee_growth_global_a = 11;
        pool.fee_growth_global_b = 13;

        // Up: the tick's range joins the active set, its mirror flips, and the
        // price is recorded *on* the tick.
        cross_tick(&mut pool, &mut state, false).unwrap();
        assert_eq!(pool.liquidity, 1_500);
        assert_eq!(pool.tick_current, 60);
        assert_eq!(state.fee_growth_outside_a, 11u128.wrapping_sub(7));
        assert_eq!(state.fee_growth_outside_b, 13u128.wrapping_sub(9));

        // Down through the same tick: the range leaves again and the pool records
        // the tick *below*, which is what keeps a position whose bound sits exactly
        // on the current price on the inactive side while the price walks down.
        cross_tick(&mut pool, &mut state, true).unwrap();
        assert_eq!(pool.liquidity, 1_000);
        assert_eq!(pool.tick_current, 59);
        assert_eq!(state.fee_growth_outside_a, 7);
        assert_eq!(state.fee_growth_outside_b, 9);
    }

    #[test]
    fn a_crossing_cannot_push_liquidity_negative() {
        let mut pool = pool_at(math_core::sqrt_price_at_tick(0).unwrap());
        let mut state = tick(-60, 0, 0);
        state.liquidity_net = 10;
        pool.liquidity = 5;
        // Moving down past a positive net would take active liquidity below zero,
        // which means the pool's accounting and its ticks have diverged.
        assert!(cross_tick(&mut pool, &mut state, true).is_err());
        // Moving up from the same state is fine.
        assert!(cross_tick(&mut pool, &mut state, false).is_ok());
        assert_eq!(pool.liquidity, 15);
    }
}
