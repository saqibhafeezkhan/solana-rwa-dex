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
    Ok(numerator
        .checked_add(denom)
        .and_then(|v| v.checked_sub(1))
        .and_then(|v| v.checked_div(denom))
        .ok_or(MathError::Overflow)?)
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

#[program]
pub mod clmm {
    use super::*;

    /// Initializes a new CLMM pool with its two vaults.
    ///
    /// `initial_sqrt_price_x64` may be any price inside the usable Q64.64 range
    /// and does not have to sit on a tick boundary. `fee_pips` is the swap fee
    /// in millionths (3_000 = 0.3%).
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        initial_sqrt_price_x64: u128,
        tick_spacing: i32,
        fee_pips: u32,
    ) -> Result<()> {
        require!(tick_spacing > 0, ErrorCode::InvalidTickSpacing);
        require!(fee_pips <= MAX_FEE_PIPS, ErrorCode::FeeTooHigh);

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

        msg!(
            "CLMM pool initialized at tick {} (fee {} pips)",
            pool.tick_current,
            fee_pips
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

        // 2. Accrue fees earned by the existing position BEFORE any state
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

        // 5. Update Pool Liquidity if the position spans the current price.
        if pool.tick_current >= lower_tick && pool.tick_current < upper_tick {
            pool.liquidity = pool
                .liquidity
                .checked_add(liquidity_amount)
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        }

        // 6. Transfer tokens from user to pool vaults.
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

        // 4. Update the pool's active liquidity if the range spans the price.
        if pool.tick_current >= lower_tick && pool.tick_current < upper_tick {
            pool.liquidity = pool
                .liquidity
                .checked_sub(liquidity_amount)
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
        }

        // 5. Pay out. The pool PDA signs for its vaults.
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
    /// - selling A for B (price down): strictly descending ticks, first below
    ///   `pool.tick_current`;
    /// - selling B for A (price up): strictly ascending ticks, first above
    ///   `pool.tick_current`.
    ///
    /// A trade that exhausts the supplied ticks is partially filled: only the
    /// consumed input is pulled from the user and the rest stays with them.
    pub fn swap(
        ctx: Context<Swap>,
        amount_in: u64,
        min_amount_out: u64,
        is_a_to_b: bool,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let ticks = &ctx.remaining_accounts;

        require!(amount_in > 0, ErrorCode::ZeroAmount);

        // 1. Validate the walk up front: every remaining account must be a
        // canonical, writable TickState PDA of this pool, ordered strictly in
        // the walk direction starting beyond the current tick.
        let mut walk: Vec<(i32, usize)> = Vec::with_capacity(ticks.len());
        for (i, info) in ticks.iter().enumerate() {
            require!(info.is_writable, ErrorCode::TickNotWritable);
            let tick = Account::<TickState>::try_from(info)?;
            require!(tick.pool == pool.key(), ErrorCode::WrongTickAccount);
            let expected = Pubkey::find_program_address(
                &[
                    b"clmm-tick",
                    pool.key().as_ref(),
                    &tick.tick_index.to_le_bytes(),
                ],
                &crate::ID,
            )
            .0;
            require!(expected == info.key(), ErrorCode::WrongTickAccount);
            match walk.last() {
                None => {
                    let beyond = if is_a_to_b {
                        tick.tick_index < pool.tick_current
                    } else {
                        tick.tick_index > pool.tick_current
                    };
                    require!(beyond, ErrorCode::TicksNotInOrder);
                }
                Some(&(prev_index, _)) => {
                    let ordered = if is_a_to_b {
                        tick.tick_index < prev_index
                    } else {
                        tick.tick_index > prev_index
                    };
                    require!(ordered, ErrorCode::TicksNotInOrder);
                }
            }
            walk.push((tick.tick_index, i));
        }

        // 2. Walk the curve. Each iteration either consumes the rest of the
        // input or reaches (and crosses) the next supplied tick, so the loop
        // is bounded by the number of supplied ticks.
        let mut remaining = u128::from(amount_in);
        let mut total_out: u128 = 0;
        let mut cursor = 0usize;

        while remaining > 0 && cursor < walk.len() {
            if pool.liquidity == 0 {
                // Nothing to trade against until the price reaches a tick that
                // carries liquidity, which cannot happen without a step.
                break;
            }

            let (next_tick_index, account_index) = walk[cursor];
            let sqrt_price_target = math_core::sqrt_price_at_tick(next_tick_index)
                .map_err(math_err)?;
            if sqrt_price_target == pool.sqrt_price_x64 {
                // Defensive: the price already sits exactly on this tick.
                cursor += 1;
                continue;
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

            // 3. Cross the tick when the step reached the boundary.
            if step.sqrt_price_next == sqrt_price_target {
                let mut tick =
                    Account::<TickState>::try_from(&ticks[account_index])?;

                // Flip the fee-growth mirror: everything the tick called
                // "outside" becomes "inside" and vice versa. Wrapping
                // arithmetic is intentional (Uniswap v3 convention).
                tick.fee_growth_outside_a = pool
                    .fee_growth_global_a
                    .wrapping_sub(tick.fee_growth_outside_a);
                tick.fee_growth_outside_b = pool
                    .fee_growth_global_b
                    .wrapping_sub(tick.fee_growth_outside_b);
                tick.exit(&crate::ID)?;

                pool.tick_current = tick.tick_index;

                // The crossed tick's range enters (or leaves) the active set.
                let liquidity_now =
                    i128::try_from(pool.liquidity).map_err(|_| error!(ErrorCode::MathOverflow))?;
                let liquidity_next = if is_a_to_b {
                    // Moving down: the tick's range is left behind.
                    liquidity_now.checked_sub(tick.liquidity_net)
                } else {
                    // Moving up: the tick's range joins the active set.
                    liquidity_now.checked_add(tick.liquidity_net)
                }
                .ok_or_else(|| error!(ErrorCode::MathOverflow))?;
                pool.liquidity =
                    u128::try_from(liquidity_next).map_err(|_| error!(ErrorCode::MathOverflow))?;

                cursor += 1;
            }
        }

        // 4. Settle: pull only what was consumed (input + fees), pay the output.
        let consumed = u64::try_from(u128::from(amount_in) - remaining)
            .map_err(|_| error!(ErrorCode::MathOverflow))?;
        let amount_out =
            u64::try_from(total_out).map_err(|_| error!(ErrorCode::MathOverflow))?;
        require!(consumed > 0, ErrorCode::InsufficientLiquidity);
        require!(amount_out >= min_amount_out, ErrorCode::SlippageExceeded);

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

        // Transfer output out (the pool PDA signs via its bump seeds).
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
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                SplTransfer {
                    from: dest_vault.to_account_info(),
                    to: user_dest.to_account_info(),
                    authority: pool_info,
                },
                signer_seeds,
            ),
            amount_out,
        )?;

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
    pub tick_current: i32,
    pub tick_spacing: i32,
    pub fee_pips: u32,
    pub fee_growth_global_a: u128,
    pub fee_growth_global_b: u128,
    pub bump: u8,
}

impl Pool {
    pub const LEN: usize = 8 + 32 + 32 + 16 + 16 + 4 + 4 + 4 + 16 + 16 + 1;
}

#[account]
pub struct TickState {
    pub pool: Pubkey,
    pub tick_index: i32,
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub fee_growth_outside_a: u128,
    pub fee_growth_outside_b: u128,
}

impl TickState {
    pub const LEN: usize = 8 + 32 + 4 + 16 + 16 + 16 + 16;
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
pub struct InitializePool<'info> {
    #[account(init, payer = authority, space = Pool::LEN, seeds = [b"clmm-pool", mint_a.key().as_ref(), mint_b.key().as_ref()], bump)]
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

    pub token_program: Program<'info, Token>,

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
