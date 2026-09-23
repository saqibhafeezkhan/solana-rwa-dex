//! # TWAMM (Time-Weighted Average Market Maker)
//!
//! A block order that hits a constant-product pool pays for its own size: the
//! first unit gets the quoted price and the last unit gets whatever is left of
//! the curve. A TWAMM lets an institution escrow the whole block up front and
//! then sells it in small pieces over a period of time, so the average
//! execution price tracks the market instead of moving it.
//!
//! ## How execution works
//!
//! An order escrows its input, states a duration, and is split into a rate: the
//! escrowed amount divided by the duration, in raw token units per second. The
//! pool holds the sum of the rates of every live order on each side, and the
//! virtual trades of a period are settled by anyone who calls
//! `execute_virtual_orders`.
//!
//! Settling a period is where the two ideas of the design meet:
//!
//! - **Opposing flow is netted.** What an A-seller and a B-seller have in common
//!   is traded directly between the two groups at the pool's marginal price, so
//!   the offsetting part of two large orders pays neither a fee nor a price
//!   impact. Only the residual imbalance is handed to the embedded AMM.
//! - **The residual is traded once per period, not once per order.** Every
//!   settle advances the pool's reserves by one segment of constant-product
//!   trade, so an hour-long order executes against the curve in as many pieces
//!   as there were settle calls rather than in one lump.
//!
//! Both properties are worth being precise about, because a TWAMM is often sold
//! as "you get a better price than dumping". Against a *static* curve, slicing a
//! size into pieces is path-neutral - the test suite pins that at within 1% of a
//! single swap of the same size - so the advantage is not the slicing itself. It
//! is that the order never has to be filled at one instant: the price it trades
//! at can recover between segments (and on a real venue, other flow and
//! arbitrage move it), while the offsetting half of two facing orders costs
//! neither a fee nor any impact at all.
//!
//! The proceeds of a period are credited through two per-unit accumulators
//! (`acc_b_per_a`, `acc_a_per_b`, both Q64.64 output per raw unit sold), so
//! crediting costs one addition per side no matter how many orders are live. An
//! order snapshots the accumulator it is owed at creation and, when it expires,
//! claims `sold * (accumulator_now - snapshot)`: exactly its share of the flow
//! it was part of, and none of the flow that happened before it existed.
//!
//! ## The order list
//!
//! The pool's rate has to stop exactly when an order expires, otherwise the
//! pool would keep selling an escrow that has run out. Orders therefore form a
//! singly linked list, sorted by expiry, threaded through the order accounts
//! themselves (`next` / `next_expiry`) with the head cached on the pool
//! (`order_head` / `order_head_expiry`).
//!
//! Callers walk that list by passing the orders as remaining accounts, and the
//! walk is verified against the chain rather than trusted: each supplied account
//! must be the key the previous one points at, so an expired order can never be
//! skipped and a live order can never be settled early. Every entry point takes
//! the list from the head, and every one insists that nothing expired is left
//! behind.
//!
//! A live order can also be stopped early with `cancel_order`. It trades the
//! time up to the call with the order still in the pool, then takes its rate
//! out, credits what it sold, refunds the rest of the escrow and unlinks it -
//! so the pool is never left selling for an escrow that is no longer there.
//!
//! ## Liquidity
//!
//! The embedded AMM is funded by LPs who deposit both tokens and hold shares in
//! an `LpPosition`. Shares are valued in raw token A at the marginal price
//! (`value_in_a`), so a deposit is worth the same whenever it arrives, and the
//! AMM's fee accrues to the reserves, which lifts every share's value without
//! any fee bookkeeping.
//!
//! Vaults are pool-owned PDAs: the program is the only authority that can move
//! them, so a caller cannot substitute an account it controls for a vault.
//! Every deposit is measured on arrival (`transfer_and_measure`) because a
//! Token-2022 transfer fee can skim part of a transfer in flight.
//!
//! ## Trust model and limits
//!
//! The pool has an authority that can pause it. Pausing refuses new orders and
//! deposits; it never blocks a withdrawal, so escrowed capital and LP liquidity
//! are never trapped. The program deliberately has no public `swap`: the
//! embedded AMM is reachable only from virtual order execution.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9");

/// PDA seeds.
const POOL_SEED: &[u8] = b"twamm-pool";
const VAULT_A_SEED: &[u8] = b"twamm-vault-a";
const VAULT_B_SEED: &[u8] = b"twamm-vault-b";
const ORDER_SEED: &[u8] = b"twamm-order";
const LP_SEED: &[u8] = b"twamm-lp";

/// Fee the embedded AMM charges on the flow it trades, in basis points. Flow
/// that two opposing orders net out pays nothing: it never reaches the AMM.
const FEE_BPS: u128 = 30;
/// Denominator for [`FEE_BPS`].
const BPS_DENOMINATOR: u128 = 10_000;

/// Longest duration an order may state, in seconds. A year is far past any
/// institutional execution window and keeps the rate arithmetic in range.
const MAX_DURATION_SECONDS: i64 = 365 * 24 * 60 * 60;

#[program]
pub mod twamm {
    use super::*;

    /// Creates the pool with its two pool-owned vaults and seeds the embedded
    /// AMM. The initialiser is the pool's first LP: the genesis shares are
    /// minted to `payer`'s position, and at a fresh pool the two legs of the
    /// deposit are worth the same in token A, so the supply starts at twice the
    /// A leg (`value_in_a(initial_a, initial_b, price)`).
    ///
    /// `authority` becomes the pool's admin - in a regulated market this should
    /// be an institutional multisig - and is the only account that can pause the
    /// pool or hand the role on.
    ///
    /// `kyc_config`, `oracle_state` and `hooks_authority` fix the pool's
    /// wiring policy at birth (`Pubkey::default()` leaves a rail disabled):
    ///
    /// * `kyc_config` — `create_long_term_order` CPIs the KYC gate for the
    ///   order owner before escrowing anything.
    /// * `oracle_state` — every instruction that settles virtual flow runs the
    ///   oracle circuit breaker on the pre-trade marginal price and records
    ///   the post-trade one.
    /// * `hooks_authority` — order proceeds (not the untraded refund) are
    ///   routed into the T+N settlement escrow (through the hooks program)
    ///   instead of paid to the order owner, unlocking `settlement_slots`
    ///   after the order settles. A TWAMM has no spot swap, so this is the
    ///   only value-bearing flow the settlement rail can capture.
    ///
    /// The matching program accounts (`kyc_program`, `oracle_program`,
    /// `hooks_program`) must be passed when the policy is set; the context
    /// verifies each against its real program ID, which pins the config
    /// oracle/hook accounts to the genuine programs.
    pub fn initialize_pool(
        ctx: Context<InitializePool>,
        initial_amount_a: u64,
        initial_amount_b: u64,
        authority: Pubkey,
        kyc_config: Pubkey,
        max_deviation_bps: u16,
        hooks_authority: Pubkey,
        settlement_slots: u64,
    ) -> Result<()> {
        require!(
            initial_amount_a > 0 && initial_amount_b > 0,
            ErrorCode::ZeroAmount
        );

        // Settlement routing needs both halves of the wiring and a horizon.
        if hooks_authority != Pubkey::default() {
            require!(settlement_slots > 0, ErrorCode::InvalidSettlementSlots);
        } else {
            require!(settlement_slots == 0, ErrorCode::InvalidSettlementSlots);
        }

        // Oracle rail: create the pool's oracle state now, with the pool PDA
        // signing, so its threshold and unpause owner are fixed with the pool
        // and no one can front-run a different configuration. Passing this
        // program's own ID in place of the account leaves the rail off.
        let oracle_state_key = if let Some(oracle_state) = ctx.accounts.oracle_state.as_ref() {
            require!(max_deviation_bps > 0, ErrorCode::InvalidOracleThreshold);
            let (expected, _) = Pubkey::find_program_address(
                &[b"oracle", ctx.accounts.pool.key().as_ref()],
                &oracle::ID,
            );
            require_keys_eq!(
                oracle_state.key(),
                expected,
                ErrorCode::WrongOracleState
            );

            let mint_a_key = ctx.accounts.mint_a.key();
            let mint_b_key = ctx.accounts.mint_b.key();
            let pool_bump = [ctx.bumps.pool];
            let pool_seeds: &[&[u8]] =
                &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &pool_bump];
            let signer_seeds: &[&[&[u8]]] = &[pool_seeds];
            let cpi_accounts = oracle::cpi::accounts::Initialize {
                oracle_state: oracle_state.to_account_info(),
                pool: ctx.accounts.pool.to_account_info(),
                payer: ctx.accounts.payer.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            let cpi_ctx = CpiContext::new(oracle::ID, cpi_accounts).with_signer(signer_seeds);
            oracle::cpi::initialize(cpi_ctx, max_deviation_bps, authority)?;
            expected
        } else {
            require!(max_deviation_bps == 0, ErrorCode::InvalidOracleThreshold);
            Pubkey::default()
        };

        let now = Clock::get()?.unix_timestamp;
        let mint_a = ctx.accounts.mint_a.key();
        let mint_b = ctx.accounts.mint_b.key();
        let payer = ctx.accounts.payer.key();
        let signer_seeds: &[&[&[u8]]] = &[];

        // 1. Move both legs in and measure what the vaults received.
        let deposited_a = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.payer_token_a,
            &ctx.accounts.mint_a,
            &mut ctx.accounts.vault_a,
            &ctx.accounts.payer.to_account_info(),
            signer_seeds,
            initial_amount_a,
        )?;
        let deposited_b = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.payer_token_b,
            &ctx.accounts.mint_b,
            &mut ctx.accounts.vault_b,
            &ctx.accounts.payer.to_account_info(),
            signer_seeds,
            initial_amount_b,
        )?;
        require!(deposited_a > 0 && deposited_b > 0, ErrorCode::ZeroAmount);

        // 2. The deposit *is* the genesis price, so the shares it buys are its
        //    value in token A at that price.
        let price_x64 = marginal_price(deposited_a, deposited_b)?;
        let shares = value_in_a(deposited_a, deposited_b, price_x64)?;

        let pool = &mut ctx.accounts.pool;
        pool.mint_a = mint_a;
        pool.mint_b = mint_b;
        pool.reserve_a = deposited_a;
        pool.reserve_b = deposited_b;
        pool.last_execution_timestamp = now;
        pool.sell_rate_a = 0;
        pool.sell_rate_b = 0;
        pool.acc_b_per_a = 0;
        pool.acc_a_per_b = 0;
        pool.order_head = Pubkey::default();
        pool.order_head_expiry = 0;
        pool.lp_supply = shares;
        pool.authority = authority;
        pool.paused = false;
        pool.kyc_config = kyc_config;
        pool.oracle_state = oracle_state_key;
        pool.hooks_authority = hooks_authority;
        pool.settlement_slots = settlement_slots;
        pool.bump = ctx.bumps.pool;
        pool.vault_a_bump = ctx.bumps.vault_a;
        pool.vault_b_bump = ctx.bumps.vault_b;

        let position = &mut ctx.accounts.lp_position;
        position.pool = pool.key();
        position.owner = payer;
        position.shares = shares;
        position.bump = ctx.bumps.lp_position;

        msg!(
            "TWAMM pool initialized. Reserves {} A / {} B, {} LP shares, authority {}. Wiring: kyc {}, oracle {}, hooks {} (T+{} slots)",
            deposited_a,
            deposited_b,
            shares,
            authority,
            kyc_config,
            oracle_state_key,
            hooks_authority,
            settlement_slots
        );
        Ok(())
    }

    /// Deposits both tokens and mints LP shares against the deposit's value in
    /// token A at the pool's current marginal price.
    ///
    /// Both sides are required, and each leg is valued at the marginal price, so
    /// a deposit that does not match the pool's ratio simply gets priced at that
    /// ratio. That is the same trade every LP at the same ratio gets.
    pub fn add_liquidity(ctx: Context<AddLiquidity>, amount_a: u64, amount_b: u64) -> Result<()> {
        require!(amount_a > 0 && amount_b > 0, ErrorCode::ZeroAmount);
        require!(!ctx.accounts.pool.paused, ErrorCode::PoolPaused);

        let pool_key = ctx.accounts.pool.key();
        let owner_key = ctx.accounts.owner.key();
        let signer_seeds: &[&[&[u8]]] = &[];

        // 1. Price the deposit and measure what the vaults actually received.
        let price_x64 = marginal_price(ctx.accounts.pool.reserve_a, ctx.accounts.pool.reserve_b)?;
        let deposited_a = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.owner_token_a,
            &ctx.accounts.mint_a,
            &mut ctx.accounts.vault_a,
            &ctx.accounts.owner.to_account_info(),
            signer_seeds,
            amount_a,
        )?;
        let deposited_b = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.owner_token_b,
            &ctx.accounts.mint_b,
            &mut ctx.accounts.vault_b,
            &ctx.accounts.owner.to_account_info(),
            signer_seeds,
            amount_b,
        )?;
        require!(deposited_a > 0 && deposited_b > 0, ErrorCode::ZeroAmount);

        // 2. Shares are proportional to value, so the ratio of shares to value
        //    never moves: the first deposit defines the unit, later deposits are
        //    scaled by the existing supply.
        let deposit_value = value_in_a(deposited_a, deposited_b, price_x64)?;
        let pool = &mut ctx.accounts.pool;
        let shares = if pool.lp_supply == 0 {
            deposit_value
        } else {
            let reserve_value = value_in_a(pool.reserve_a, pool.reserve_b, price_x64)?;
            require!(reserve_value > 0, ErrorCode::InsufficientLiquidity);
            deposit_value
                .checked_mul(pool.lp_supply)
                .ok_or(ErrorCode::MathOverflow)?
                .checked_div(reserve_value)
                .ok_or(ErrorCode::MathOverflow)?
        };
        require!(shares > 0, ErrorCode::ZeroAmount);

        pool.reserve_a = pool
            .reserve_a
            .checked_add(deposited_a)
            .ok_or(ErrorCode::MathOverflow)?;
        pool.reserve_b = pool
            .reserve_b
            .checked_add(deposited_b)
            .ok_or(ErrorCode::MathOverflow)?;
        pool.lp_supply = pool
            .lp_supply
            .checked_add(shares)
            .ok_or(ErrorCode::MathOverflow)?;

        let position = &mut ctx.accounts.position;
        position.pool = pool_key;
        position.owner = owner_key;
        position.shares = position
            .shares
            .checked_add(shares)
            .ok_or(ErrorCode::MathOverflow)?;

        msg!(
            "Added {} A and {} B for {} shares",
            deposited_a,
            deposited_b,
            shares
        );
        Ok(())
    }

    /// Burns LP shares and pays out the proportional slice of the reserves,
    /// rounded down so the pool never pays more than the curve holds.
    ///
    /// Deliberately not blocked by a pause: a paused pool still lets LPs out.
    pub fn remove_liquidity(
        ctx: Context<RemoveLiquidity>,
        shares: u128,
        min_amount_a: u64,
        min_amount_b: u64,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let position = &mut ctx.accounts.position;

        require!(
            position.owner == ctx.accounts.owner.key(),
            ErrorCode::Unauthorized
        );
        require!(
            shares > 0 && shares <= position.shares,
            ErrorCode::InsufficientShares
        );

        let amount_a = share_of(pool.reserve_a, shares, pool.lp_supply)?;
        let amount_b = share_of(pool.reserve_b, shares, pool.lp_supply)?;
        require!(
            amount_a >= min_amount_a && amount_b >= min_amount_b,
            ErrorCode::SlippageExceeded
        );

        pool.reserve_a = pool
            .reserve_a
            .checked_sub(amount_a)
            .ok_or(ErrorCode::MathOverflow)?;
        pool.reserve_b = pool
            .reserve_b
            .checked_sub(amount_b)
            .ok_or(ErrorCode::MathOverflow)?;
        pool.lp_supply = pool
            .lp_supply
            .checked_sub(shares)
            .ok_or(ErrorCode::MathOverflow)?;
        position.shares = position
            .shares
            .checked_sub(shares)
            .ok_or(ErrorCode::MathOverflow)?;

        // Pay out of the pool-owned vaults, signed by the pool PDA.
        let mint_a_key = pool.mint_a;
        let mint_b_key = pool.mint_b;
        let bump = [pool.bump];
        let pool_seeds: &[&[u8]] = &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];
        let pool_info = ctx.accounts.pool.to_account_info();

        if amount_a > 0 {
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.vault_a,
                &ctx.accounts.mint_a,
                &ctx.accounts.owner_token_a.to_account_info(),
                &pool_info,
                &[pool_seeds],
                amount_a,
            )?;
        }
        if amount_b > 0 {
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.vault_b,
                &ctx.accounts.mint_b,
                &ctx.accounts.owner_token_b.to_account_info(),
                &pool_info,
                &[pool_seeds],
                amount_b,
            )?;
        }

        msg!(
            "Removed liquidity. Shares burned: {}, out: {} A / {} B",
            shares,
            amount_a,
            amount_b
        );
        Ok(())
    }

    /// Escrows `amount_in` and starts selling it at a constant rate for
    /// `duration_seconds`.
    ///
    /// `order_id` is a caller-chosen tag: together with the owner it makes up the
    /// order's address, so the same owner can hold many orders on one pool. The
    /// account must not exist yet, which is what makes the id unique rather than
    /// something the program has to check.
    ///
    /// Remaining accounts: the order list from the pool's head up to, but not
    /// including, the first order that expires after this one. The walk settles
    /// any expired orders it passes on the way, so creating an order is also a
    /// chance to crank the queue.
    ///
    /// Pool-level rails: the KYC rail (enabled at `initialize_pool` via
    /// `kyc_config`) gates the owner before anything is escrowed, and the
    /// oracle rail (`oracle_state`) circuit-breaks the virtual flow the walk
    /// settles, then records the settled price. Rail accounts that the pool's
    /// policy does not demand are passed as this program's own ID.
    pub fn create_long_term_order<'info>(
        ctx: Context<'info, CreateLongTermOrder<'info>>,
        order_id: u64,
        amount_in: u64,
        duration_seconds: i64,
        is_a_to_b: bool,
    ) -> Result<()> {
        require!(amount_in > 0, ErrorCode::ZeroAmount);
        require!(duration_seconds > 0, ErrorCode::ZeroDuration);
        require!(
            duration_seconds <= MAX_DURATION_SECONDS,
            ErrorCode::DurationTooLong
        );
        require!(!ctx.accounts.pool.paused, ErrorCode::PoolPaused);

        // 0. Rails: policy checks first, then the KYC CPI — no attestation, no
        //    order. The oracle pre-trade CPI runs below, right before the walk
        //    moves the price.
        if ctx.accounts.pool.kyc_config != Pubkey::default() {
            let kyc_config = ctx
                .accounts
                .kyc_config
                .as_ref()
                .ok_or(ErrorCode::RailAccountUnexpected)?;
            require_keys_eq!(
                kyc_config.key(),
                ctx.accounts.pool.kyc_config,
                ErrorCode::WrongKycConfig
            );
            kyc_gate_check(
                kyc_config,
                &ctx.accounts.owner,
                &ctx.accounts.user_kyc_account,
            )?;
        } else {
            require!(
                ctx.accounts.kyc_config.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            require_keys_eq!(
                ctx.accounts.user_kyc_account.key(),
                crate::ID,
                ErrorCode::RailAccountUnexpected
            );
        }
        if ctx.accounts.pool.oracle_state != Pubkey::default() {
            let oracle_state = ctx
                .accounts
                .oracle_state
                .as_ref()
                .ok_or(ErrorCode::RailAccountUnexpected)?;
            let (expected, _) = Pubkey::find_program_address(
                &[b"oracle", ctx.accounts.pool.key().as_ref()],
                &oracle::ID,
            );
            require_keys_eq!(
                oracle_state.key(),
                expected,
                ErrorCode::WrongOracleState
            );
            oracle_check_pre_trade(&ctx.accounts.pool, oracle_state)?;
        } else {
            require!(
                ctx.accounts.oracle_state.is_none(),
                ErrorCode::RailAccountUnexpected
            );
        }

        let now = Clock::get()?.unix_timestamp;
        let expiry_timestamp = now
            .checked_add(duration_seconds)
            .ok_or(ErrorCode::MathOverflow)?;
        let pool_key = ctx.accounts.pool.key();
        let order_key = ctx.accounts.order.key();
        let owner_key = ctx.accounts.owner.key();
        let program_id = ctx.program_id;
        let remaining = ctx.remaining_accounts;
        let signer_seeds: &[&[&[u8]]] = &[];

        // 1. Bring the pool up to date and find the new order's place in the
        //    list. The walk settles every order that expired before `now`.
        let pool = &mut ctx.accounts.pool;
        let (predecessor, cursor) = walk_order_list(
            pool,
            &pool_key,
            program_id,
            now,
            remaining,
            WalkMode::Insert {
                expiry: expiry_timestamp,
            },
        )?;

        // 2. Escrow the input and measure it, then derive the rate from what the
        //    vault actually received.
        let escrowed = if is_a_to_b {
            transfer_and_measure(
                &ctx.accounts.token_program,
                &ctx.accounts.owner_token_a,
                &ctx.accounts.mint_a,
                &mut ctx.accounts.vault_a,
                &ctx.accounts.owner.to_account_info(),
                signer_seeds,
                amount_in,
            )?
        } else {
            transfer_and_measure(
                &ctx.accounts.token_program,
                &ctx.accounts.owner_token_b,
                &ctx.accounts.mint_b,
                &mut ctx.accounts.vault_b,
                &ctx.accounts.owner.to_account_info(),
                signer_seeds,
                amount_in,
            )?
        };
        require!(escrowed > 0, ErrorCode::ZeroAmount);

        let duration_seconds = duration_seconds as u64;
        let sell_rate = escrowed / duration_seconds;
        require!(sell_rate > 0, ErrorCode::RateTooSmall);

        // 3. Close the walk before the order exists: its snapshot must be the
        //    accumulator as it stands after all time up to `now` was traded.
        finish_walk(pool, cursor, now)?;

        // 4. Splice the order into the list. Where its predecessor is known the
        //    predecessor is relinked; otherwise the order becomes the head.
        let (next, next_expiry) = match predecessor {
            Some(index) => {
                let mut pred = Account::<LongTermOrder>::try_from(&remaining[index])?;
                let link = (pred.next, pred.next_expiry);
                pred.next = order_key;
                pred.next_expiry = expiry_timestamp;
                pred.exit(program_id)?;
                link
            }
            None => {
                let link = (pool.order_head, pool.order_head_expiry);
                pool.order_head = order_key;
                pool.order_head_expiry = expiry_timestamp;
                link
            }
        };

        // 5. Record the order and add its rate to the side it sells on. The
        //    snapshot is taken after the walk, so the order is owed nothing for
        //    flow that happened before it existed.
        {
            let order = &mut ctx.accounts.order;
            order.pool = pool_key;
            order.owner = owner_key;
            order.order_id = order_id;
            order.is_a_to_b = is_a_to_b;
            order.sell_rate = sell_rate;
            order.amount_in = escrowed;
            order.sold = 0;
            order.duration = duration_seconds as i64;
            order.created_timestamp = now;
            order.expiry_timestamp = expiry_timestamp;
            order.acc_snapshot = if is_a_to_b {
                pool.acc_b_per_a
            } else {
                pool.acc_a_per_b
            };
            order.proceeds_out = 0;
            order.settled = false;
            order.next = next;
            order.next_expiry = next_expiry;
            order.bump = ctx.bumps.order;
        }

        if is_a_to_b {
            pool.sell_rate_a = pool
                .sell_rate_a
                .checked_add(sell_rate)
                .ok_or(ErrorCode::MathOverflow)?;
        } else {
            pool.sell_rate_b = pool
                .sell_rate_b
                .checked_add(sell_rate)
                .ok_or(ErrorCode::MathOverflow)?;
        }

        // 6. Oracle rail: record the settled price so the TWAP stays current
        //    for the next circuit-breaker check.
        if let Some(oracle_state) = ctx.accounts.oracle_state.as_ref() {
            oracle_record_post_trade(&ctx.accounts.pool, oracle_state)?;
        }

        msg!(
            "Long-term order {} created. Selling {} {} at {} per second until {}",
            order_id,
            escrowed,
            if is_a_to_b { "A" } else { "B" },
            sell_rate,
            expiry_timestamp
        );
        Ok(())
    }

    /// Settles every virtual trade between the pool's last execution and now,
    /// for every live order, and drops the orders that expired on the way.
    ///
    /// Permissionless: whoever wants the pool caught up can pay for it. Nothing
    /// here can be skipped, because the walk starts at the pool's own head and
    /// refuses to leave an expired order behind.
    ///
    /// Remaining accounts: the expired orders starting at the pool's head, in
    /// list order. Pass none when nothing has expired.
    ///
    /// Oracle rail: this instruction moves the price, so when the pool was
    /// initialized with `oracle_state` it is circuit-broken before the walk
    /// and the settled price is recorded after it. It carries no KYC and no
    /// settlement accounts: it initiates nothing and routes nothing.
    pub fn execute_virtual_orders(ctx: Context<ExecuteVirtualOrders>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let pool_key = ctx.accounts.pool.key();
        let program_id = ctx.program_id;
        let remaining = ctx.remaining_accounts;
        let pool = &mut ctx.accounts.pool;

        // Oracle rail, pre-trade: the walk is about to net and trade virtual
        // flow at the marginal price.
        if pool.oracle_state != Pubkey::default() {
            let oracle_state = ctx
                .accounts
                .oracle_state
                .as_ref()
                .ok_or(ErrorCode::RailAccountUnexpected)?;
            let (expected, _) = Pubkey::find_program_address(
                &[b"oracle", pool_key.as_ref()],
                &oracle::ID,
            );
            require_keys_eq!(
                oracle_state.key(),
                expected,
                ErrorCode::WrongOracleState
            );
            oracle_check_pre_trade(pool, oracle_state)?;
        } else {
            require!(
                ctx.accounts.oracle_state.is_none(),
                ErrorCode::RailAccountUnexpected
            );
        }

        let (_, cursor) = walk_order_list(
            pool,
            &pool_key,
            program_id,
            now,
            remaining,
            WalkMode::SettleExpired,
        )?;
        finish_walk(pool, cursor, now)?;

        // Oracle rail, post-trade.
        if let Some(oracle_state) = ctx.accounts.oracle_state.as_ref() {
            oracle_record_post_trade(pool, oracle_state)?;
        }

        msg!("Virtual orders executed to {}", now);
        Ok(())
    }

    /// Pays out a settled order: the output token it earned, plus a refund of
    /// any input the rate's integer division left in escrow. The order account
    /// is closed and its rent returns to the owner.
    ///
    /// An order is `settled` once the pool has met its expiry, which happens
    /// inside `execute_virtual_orders` or `create_long_term_order` when they
    /// walk past it. Nothing to settle yet means nothing to withdraw: call
    /// `execute_virtual_orders` first.
    ///
    /// `order_id` is the tag the order was created with. It is part of the
    /// order's address, so the caller cannot reach somebody else's order here.
    ///
    /// Settlement rail: on a wired pool the proceeds — sold tokens — are paid
    /// into the hook authority's custody ATA and locked in the T+N escrow for
    /// the order's id in the same transaction. The untraded refund still goes
    /// straight back to the owner: it was never sold, so it was never a trade.
    /// Nothing is traded here, so the oracle rail has no price to check.
    pub fn withdraw_proceeds(ctx: Context<WithdrawProceeds>, order_id: u64) -> Result<()> {
        let order = &mut ctx.accounts.order;
        require!(order.order_id == order_id, ErrorCode::WrongOrder);
        require!(order.settled, ErrorCode::OrderNotSettled);
        require!(
            order.owner == ctx.accounts.owner.key(),
            ErrorCode::Unauthorized
        );
        require_keys_eq!(
            order.pool,
            ctx.accounts.pool.key(),
            ErrorCode::WrongOrderPool
        );

        // 0. Settlement rail policy: the order's id names the escrow when the
        //    rail is on; when it is off, the rail's accounts must be this
        //    program's own ID sentinel.
        let settlement_on = ctx.accounts.pool.hooks_authority != Pubkey::default();
        if settlement_on {
            let (expected_hooks, _) =
                Pubkey::find_program_address(&[b"hook_authority"], &hooks::ID);
            require_keys_eq!(
                ctx
                    .accounts
                    .hooks_authority
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .key(),
                expected_hooks,
                ErrorCode::WrongHooksAuthority
            );
            require!(order_id != 0, ErrorCode::InvalidTradeId);
        } else {
            require!(
                ctx.accounts.hooks_authority.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            for rail in [
                ctx.accounts.settlement_config.as_ref(),
                ctx.accounts.settlement_program.as_ref(),
                ctx.accounts.associated_token_program.as_ref(),
            ] {
                require_keys_eq!(
                    rail.key(),
                    crate::ID,
                    ErrorCode::RailAccountUnexpected
                );
            }
            require!(
                ctx.accounts.custody_token_account.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            require!(
                ctx.accounts.settlement_vault.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            require!(
                ctx.accounts.settlement_vault_token.is_none(),
                ErrorCode::RailAccountUnexpected
            );
        }

        let proceeds = order.proceeds_out;
        // Integer division means `sell_rate * duration` can be a little short of
        // the escrow; the shortfall is the owner's to take back.
        let refund = order
            .amount_in
            .checked_sub(order.sold)
            .ok_or(ErrorCode::MathOverflow)?;
        let is_a_to_b = order.is_a_to_b;
        order.proceeds_out = 0;
        order.amount_in = 0;

        // The proceeds' destination: the hook authority's custody ATA on a
        // wired pool, the owner's token account otherwise. The refund always
        // goes back to the owner.
        let (output_mint, output_account, input_account) = if is_a_to_b {
            (
                &ctx.accounts.mint_b,
                if settlement_on {
                    ctx
                        .accounts
                        .custody_token_account
                        .as_ref()
                        .ok_or(ErrorCode::RailAccountUnexpected)?
                        .to_account_info()
                } else {
                    ctx.accounts.owner_token_b.to_account_info()
                },
                ctx.accounts.owner_token_a.to_account_info(),
            )
        } else {
            (
                &ctx.accounts.mint_a,
                if settlement_on {
                    ctx
                        .accounts
                        .custody_token_account
                        .as_ref()
                        .ok_or(ErrorCode::RailAccountUnexpected)?
                        .to_account_info()
                } else {
                    ctx.accounts.owner_token_a.to_account_info()
                },
                ctx.accounts.owner_token_b.to_account_info(),
            )
        };

        pay_order_out(
            is_a_to_b,
            &ctx.accounts.token_program,
            &ctx.accounts.mint_a,
            &ctx.accounts.mint_b,
            &ctx.accounts.vault_a,
            &ctx.accounts.vault_b,
            &output_account,
            &input_account,
            &ctx.accounts.pool,
            proceeds,
            refund,
        )?;

        // Settlement rail: sweep the proceeds — exactly what the custody
        // account now holds — into the T+N escrow. The taker is the order
        // owner, so the Transfer Agent's signature binds the release to the
        // party that placed the order.
        if settlement_on {
            settle_proceeds(
                ctx
                    .accounts
                    .hooks_authority
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                &ctx.accounts.owner,
                ctx
                    .accounts
                    .custody_token_account
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                output_mint,
                ctx
                    .accounts
                    .settlement_vault
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                ctx
                    .accounts
                    .settlement_vault_token
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                &ctx.accounts.settlement_config,
                &ctx.accounts.settlement_program,
                &ctx.accounts.associated_token_program,
                &ctx.accounts.system_program,
                &ctx.accounts.token_program,
                order_id,
                ctx.accounts.pool.settlement_slots,
            )?;
        }

        msg!(
            "Order {} settled: {} out, {} refunded",
            order_id,
            proceeds,
            refund
        );
        Ok(())
    }

    /// Stops a live order early. It keeps the output it has earned for the time
    /// it was actually selling, the rest of its escrow comes back, and the order
    /// account is closed so its rent returns to the owner.
    ///
    /// Cancelling is not a withdrawal: [`withdraw_proceeds`] pays out an order
    /// the pool has already met the expiry of, while this one takes the order out
    /// of the pool's rates before that point. The pool is traded up to the moment
    /// of the call first, and the order is credited for that stretch, so a cancel
    /// never leaves the pool selling for an escrow that is no longer there.
    ///
    /// It is deliberately not blocked by a pause: it is an exit.
    ///
    /// `order_id` is the tag the order was created with, which is part of the
    /// order's address.
    ///
    /// Remaining accounts: the order list from the pool's head up to, but not
    /// including, the order being cancelled, in list order. The walk settles
    /// every expired order it passes, so cancelling also cranks the queue.
    ///
    /// Pool-level rails: the oracle rail circuit-breaks the virtual flow the
    /// walk settles (and records the price after it), and the settlement rail
    /// routes the cancelled order's proceeds — sold tokens — into the T+N
    /// escrow for `order_id`, while the untraded refund still goes straight
    /// back to the owner.
    ///
    /// An order that has already reached its expiry is out of scope: its escrow
    /// is fully committed by then, and `execute_virtual_orders` followed by
    /// `withdraw_proceeds` is the way to collect it.
    pub fn cancel_order(ctx: Context<CancelOrder>, order_id: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let order_key = ctx.accounts.order.key();
        let pool_key = ctx.accounts.pool.key();
        let program_id = ctx.program_id;
        let remaining = ctx.remaining_accounts;

        let order = &ctx.accounts.order;
        require!(order.order_id == order_id, ErrorCode::WrongOrder);
        require!(!order.settled, ErrorCode::OrderAlreadySettled);
        require_keys_eq!(order.pool, pool_key, ErrorCode::WrongOrderPool);
        require!(
            order.owner == ctx.accounts.owner.key(),
            ErrorCode::Unauthorized
        );
        require!(now < order.expiry_timestamp, ErrorCode::OrderExpired);

        // 0. Rails: policy checks before anything is traded or paid out.
        let settlement_on = ctx.accounts.pool.hooks_authority != Pubkey::default();
        if settlement_on {
            let (expected_hooks, _) =
                Pubkey::find_program_address(&[b"hook_authority"], &hooks::ID);
            require_keys_eq!(
                ctx
                    .accounts
                    .hooks_authority
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .key(),
                expected_hooks,
                ErrorCode::WrongHooksAuthority
            );
            require!(order_id != 0, ErrorCode::InvalidTradeId);
        } else {
            require!(
                ctx.accounts.hooks_authority.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            for rail in [
                ctx.accounts.settlement_config.as_ref(),
                ctx.accounts.settlement_program.as_ref(),
                ctx.accounts.associated_token_program.as_ref(),
            ] {
                require_keys_eq!(
                    rail.key(),
                    crate::ID,
                    ErrorCode::RailAccountUnexpected
                );
            }
            require!(
                ctx.accounts.custody_token_account.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            require!(
                ctx.accounts.settlement_vault.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            require!(
                ctx.accounts.settlement_vault_token.is_none(),
                ErrorCode::RailAccountUnexpected
            );
        }
        if ctx.accounts.pool.oracle_state != Pubkey::default() {
            let oracle_state = ctx
                .accounts
                .oracle_state
                .as_ref()
                .ok_or(ErrorCode::RailAccountUnexpected)?;
            let (expected, _) =
                Pubkey::find_program_address(&[b"oracle", pool_key.as_ref()], &oracle::ID);
            require_keys_eq!(
                oracle_state.key(),
                expected,
                ErrorCode::WrongOracleState
            );
            oracle_check_pre_trade(&ctx.accounts.pool, oracle_state)?;
        } else {
            require!(
                ctx.accounts.oracle_state.is_none(),
                ErrorCode::RailAccountUnexpected
            );
        }

        // 1. Settle every expired order in front of this one, and find the link
        //    that points at it. The pool's time is left at the walk's cursor.
        let pool = &mut ctx.accounts.pool;
        let (predecessor, cursor) = walk_order_list(
            pool,
            &pool_key,
            program_id,
            now,
            remaining,
            WalkMode::Unlink { order_key },
        )?;

        // 2. Trade the stretch up to `now` with the order's rate still in the
        //    pool: it was selling for that time, so it is owed for it.
        finish_walk(pool, cursor, now)?;

        // 3. Take its rate out, credit what it sold, and unlink it.
        let order = &mut ctx.accounts.order;
        let lived = u128::try_from(
            now.checked_sub(order.created_timestamp)
                .ok_or(ErrorCode::MathOverflow)?,
        )
        .map_err(|_| error!(ErrorCode::MathOverflow))?;
        let sold = u64::try_from(
            (order.sell_rate as u128)
                .checked_mul(lived)
                .ok_or(ErrorCode::MathOverflow)?,
        )
        .map_err(|_| error!(ErrorCode::MathOverflow))?;

        if order.is_a_to_b {
            pool.sell_rate_a = pool
                .sell_rate_a
                .checked_sub(order.sell_rate)
                .ok_or(ErrorCode::MathOverflow)?;
        } else {
            pool.sell_rate_b = pool
                .sell_rate_b
                .checked_sub(order.sell_rate)
                .ok_or(ErrorCode::MathOverflow)?;
        }
        close_order_books(order, pool, sold)?;

        let next = order.next;
        let next_expiry = order.next_expiry;
        let is_a_to_b = order.is_a_to_b;
        let proceeds = order.proceeds_out;
        let refund = order
            .amount_in
            .checked_sub(sold)
            .ok_or(ErrorCode::MathOverflow)?;
        order.proceeds_out = 0;
        order.amount_in = 0;

        match predecessor {
            Some(index) => {
                let mut pred = Account::<LongTermOrder>::try_from(&remaining[index])?;
                pred.next = next;
                pred.next_expiry = next_expiry;
                pred.exit(program_id)?;
            }
            None => {
                pool.order_head = next;
                pool.order_head_expiry = next_expiry;
            }
        }

        // 4. Pay out what it earned and refund the escrow it never used. On a
        //    wired pool the proceeds go to the hook authority's custody ATA
        //    and are locked in the T+N escrow below; the refund always goes
        //    straight back to the owner.
        let (output_mint, output_account, input_account) = if is_a_to_b {
            (
                &ctx.accounts.mint_b,
                if settlement_on {
                    ctx
                        .accounts
                        .custody_token_account
                        .as_ref()
                        .ok_or(ErrorCode::RailAccountUnexpected)?
                        .to_account_info()
                } else {
                    ctx.accounts.owner_token_b.to_account_info()
                },
                ctx.accounts.owner_token_a.to_account_info(),
            )
        } else {
            (
                &ctx.accounts.mint_a,
                if settlement_on {
                    ctx
                        .accounts
                        .custody_token_account
                        .as_ref()
                        .ok_or(ErrorCode::RailAccountUnexpected)?
                        .to_account_info()
                } else {
                    ctx.accounts.owner_token_a.to_account_info()
                },
                ctx.accounts.owner_token_b.to_account_info(),
            )
        };

        pay_order_out(
            is_a_to_b,
            &ctx.accounts.token_program,
            &ctx.accounts.mint_a,
            &ctx.accounts.mint_b,
            &ctx.accounts.vault_a,
            &ctx.accounts.vault_b,
            &output_account,
            &input_account,
            &ctx.accounts.pool,
            proceeds,
            refund,
        )?;

        // 5. Oracle rail: the walk traded the pool up to `now`, so record the
        //    settled price.
        if let Some(oracle_state) = ctx.accounts.oracle_state.as_ref() {
            oracle_record_post_trade(&ctx.accounts.pool, oracle_state)?;
        }

        // 6. Settlement rail: sweep the proceeds — exactly what the custody
        //    account now holds — into the T+N escrow. The taker is the order
        //    owner, so the Transfer Agent's signature binds the release to
        //    the party that placed the order.
        if settlement_on {
            settle_proceeds(
                ctx
                    .accounts
                    .hooks_authority
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                &ctx.accounts.owner,
                ctx
                    .accounts
                    .custody_token_account
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                output_mint,
                ctx
                    .accounts
                    .settlement_vault
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                ctx
                    .accounts
                    .settlement_vault_token
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                &ctx.accounts.settlement_config,
                &ctx.accounts.settlement_program,
                &ctx.accounts.associated_token_program,
                &ctx.accounts.system_program,
                &ctx.accounts.token_program,
                order_id,
                ctx.accounts.pool.settlement_slots,
            )?;
        }

        msg!(
            "Order {} cancelled after {} seconds: {} out, {} refunded",
            order_id,
            lived,
            proceeds,
            refund
        );
        Ok(())
    }

    /// Pauses or resumes the pool. While paused, new orders and deposits are
    /// refused; executions and withdrawals stay open, so capital already
    /// committed can always be brought back.
    pub fn set_paused(ctx: Context<Admin>, paused: bool) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.paused = paused;

        msg!("Pool {} paused: {}", pool.key(), paused);
        Ok(())
    }

    /// Hands the pool's admin role to `new_authority`, so the pause key can be
    /// rotated onto a new multisig without redeploying the pool.
    pub fn set_authority(ctx: Context<Admin>, new_authority: Pubkey) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.authority = new_authority;

        msg!("Pool {} authority: {}", pool.key(), new_authority);
        Ok(())
    }

    /// Permissionless circuit-breaker latch: pause the pool when its marginal
    /// price deviates from its 30-minute oracle TWAP by more than the pool's
    /// threshold. Anyone may pay for it; the price is read from the pool's
    /// reserves and the oracle's `trip_breaker` requires the pool PDA's
    /// signature, so the price can never be forged.
    pub fn trip_breaker(ctx: Context<TripOracle>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        require!(
            pool.oracle_state != Pubkey::default(),
            ErrorCode::OracleNotConfigured
        );
        let (expected, _) = Pubkey::find_program_address(
            &[b"oracle", pool.key().as_ref()],
            &oracle::ID,
        );
        require_keys_eq!(
            ctx.accounts.oracle_state.key(),
            expected,
            ErrorCode::WrongOracleState
        );

        let price = marginal_price(pool.reserve_a, pool.reserve_b)?;
        let mint_a_key = pool.mint_a;
        let mint_b_key = pool.mint_b;
        let bump = [pool.bump];
        let pool_seeds: &[&[u8]] = &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];
        let signer_seeds: &[&[&[u8]]] = &[pool_seeds];
        let cpi_accounts = oracle::cpi::accounts::TripBreaker {
            oracle_state: ctx.accounts.oracle_state.to_account_info(),
            pool: ctx.accounts.pool.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(oracle::ID, cpi_accounts).with_signer(signer_seeds);
        oracle::cpi::trip_breaker(cpi_ctx, price)
    }
}

// -----------------------------------------------------------------------
// VIRTUAL ORDER EXECUTION
//
// Free functions, not methods on the program module: the `#[program]` macro
// treats every function declared inside it as an instruction handler.
// -----------------------------------------------------------------------

/// What a walk over the order list is for.
#[derive(Clone, Copy)]
enum WalkMode {
    /// Settle the expired orders at the head, and stop. Passing the walk an
    /// order that has not expired is a caller error.
    SettleExpired,
    /// Settle the expired orders, then find where an order expiring at `expiry`
    /// belongs. The returned index is the account it must be linked after;
    /// `None` means it becomes the new head.
    Insert { expiry: i64 },
    /// Settle the expired orders, then stop at `order_key` - the next order in
    /// the list, which must *not* be one of the supplied accounts. The returned
    /// index is the account it must be unlinked from; `None` means it is the
    /// head.
    Unlink { order_key: Pubkey },
}

/// Walks the pool's expiry-ordered order list from the head, settling every
/// order that has expired as it goes, and stops where `mode` says. The pool's
/// time is left at the walk's cursor: closing it is [`finish_walk`]'s job,
/// because a caller may have to take a rate out of the pool in between.
///
/// Returns the index of the last account it passed - the one whose `next` links
/// to the first order it did not settle - and the timestamp it advanced to.
///
/// The walk trusts nothing but the chain of `next` pointers: each account must
/// be the key the previous order points at. That is what makes the list
/// complete - an expired order cannot be left out, because the pool's head is
/// the first key the walk demands, and the order after it the second - without
/// the program having to enumerate anything.
///
/// In [`WalkMode::Insert`] the walk also checks, through the cached
/// `next_expiry` of the last order it passed, that the caller did not stop too
/// early: an order expiring before the new one may not be skipped.
fn walk_order_list<'info>(
    pool: &mut Pool,
    pool_key: &Pubkey,
    program_id: &Pubkey,
    now: i64,
    accounts: &'info [AccountInfo<'info>],
    mode: WalkMode,
) -> Result<(Option<usize>, i64)> {
    require!(
        now >= pool.last_execution_timestamp,
        ErrorCode::ClockWentBackwards
    );

    let mut cursor = pool.last_execution_timestamp;
    let mut expected = pool.order_head;
    let mut predecessor: Option<usize> = None;
    // `next_expiry` cached on the last order the walk passed. Together with the
    // pool's own cache it proves where the list continues.
    let mut tail_next_expiry: i64 = 0;

    for (index, account) in accounts.iter().enumerate() {
        require_keys_eq!(account.key(), expected, ErrorCode::OrderListMismatch);
        require!(account.is_writable, ErrorCode::OrderAccountNotWritable);

        let mut order = Account::<LongTermOrder>::try_from(account)?;
        require_keys_eq!(order.pool, *pool_key, ErrorCode::WrongOrderPool);

        if order.expiry_timestamp > now {
            // A live order. It is only ever useful as a link in the list - the
            // insertion point's predecessor, or the order in front of the one
            // being cancelled - and never as an account to settle.
            match mode {
                WalkMode::SettleExpired => return err!(ErrorCode::OrderNotExpired),
                WalkMode::Insert { expiry } => require!(
                    order.expiry_timestamp <= expiry,
                    ErrorCode::OrderListMismatch
                ),
                WalkMode::Unlink { .. } => {}
            }

            predecessor = Some(index);
            tail_next_expiry = order.next_expiry;
            expected = order.next;
            continue;
        }

        // 1. Trade the time up to this order's expiry, while its rate is still
        //    part of the pool's.
        execute_segment(pool, order.expiry_timestamp - cursor)?;
        cursor = order.expiry_timestamp;

        // 2. Its rate leaves with it, so the pool stops selling an escrow that
        //    has run out.
        if order.is_a_to_b {
            pool.sell_rate_a = pool
                .sell_rate_a
                .checked_sub(order.sell_rate)
                .ok_or(ErrorCode::MathOverflow)?;
        } else {
            pool.sell_rate_b = pool
                .sell_rate_b
                .checked_sub(order.sell_rate)
                .ok_or(ErrorCode::MathOverflow)?;
        }

        // 3. Credit it the whole flow of its life, and unlink it.
        settle_order(&mut order, pool)?;
        let next = order.next;
        let next_expiry = order.next_expiry;
        order.exit(program_id)?;

        pool.order_head = next;
        pool.order_head_expiry = next_expiry;
        expected = next;
    }

    // The head of the list must now be empty or live: anything expired that is
    // still at the head was not supplied, and this walk refuses to leave it.
    require!(
        pool.order_head_expiry == 0 || pool.order_head_expiry > now,
        ErrorCode::MissingExpiredOrder
    );

    match mode {
        // A live order already errored out above, so nothing was taken as an
        // insertion point.
        WalkMode::SettleExpired => {}
        // The insertion point is only trustworthy if the list does not continue
        // with an order that should have come before the new one.
        WalkMode::Insert { expiry } => {
            let tail = match predecessor {
                Some(_) => tail_next_expiry,
                None => pool.order_head_expiry,
            };
            require!(tail == 0 || tail > expiry, ErrorCode::OrderListMismatch);
        }
        // The walk must have stopped exactly where the caller said it would:
        // `expected` is the next order in the chain.
        WalkMode::Unlink { order_key } => {
            require_keys_eq!(expected, order_key, ErrorCode::OrderListMismatch);
        }
    }

    Ok((predecessor, cursor))
}

/// Closes a walk: trades the time left in it at the pool's current rates, and
/// records the pool as executed up to `now`.
fn finish_walk(pool: &mut Pool, cursor: i64, now: i64) -> Result<()> {
    execute_segment(pool, now - cursor)?;
    pool.last_execution_timestamp = now;
    Ok(())
}

/// Executes `dt` seconds of virtual orders at the pool's current rates.
///
/// The two sides are netted first: whatever they have in common is traded
/// between the two order groups at the pool's marginal price, so offsetting flow
/// pays neither a fee nor a price impact. Only the residual imbalance is handed
/// to the embedded AMM, and it is handed over once per segment - which is what
/// keeps a long order's average price near the market's instead of paying for
/// its own size.
///
/// The AMM's fee is charged on the input and left in the reserve it was paid
/// from, so fees accrue to the reserves and lift `k` for the LPs.
fn execute_segment(pool: &mut Pool, dt: i64) -> Result<()> {
    if dt <= 0 {
        return Ok(());
    }

    let dt = dt as u128;
    let offered_a = (pool.sell_rate_a as u128)
        .checked_mul(dt)
        .ok_or(ErrorCode::MathOverflow)?;
    let offered_b = (pool.sell_rate_b as u128)
        .checked_mul(dt)
        .ok_or(ErrorCode::MathOverflow)?;
    if offered_a == 0 && offered_b == 0 {
        return Ok(());
    }

    let offered_a_u64 = u64::try_from(offered_a).map_err(|_| error!(ErrorCode::MathOverflow))?;
    let offered_b_u64 = u64::try_from(offered_b).map_err(|_| error!(ErrorCode::MathOverflow))?;

    // 1. Net the two sides against each other at the marginal price. The B
    //    sellers' offer, expressed in A, is what their flow can absorb.
    let price_x64 = marginal_price(pool.reserve_a, pool.reserve_b)?;
    let (cross_a, cross_b) = if offered_a_u64 > 0 && offered_b_u64 > 0 {
        let b_in_a = math_core::to_u64(
            math_core::div_shr(math_core::from_u64(offered_b_u64), price_x64)
                .map_err(|_| error!(ErrorCode::MathOverflow))?,
        );
        let cross_a = offered_a_u64.min(b_in_a);
        let cross_b = math_core::to_u64(
            math_core::mul_shr(math_core::from_u64(cross_a), price_x64)
                .map_err(|_| error!(ErrorCode::MathOverflow))?,
        )
        .min(offered_b_u64);
        (cross_a, cross_b)
    } else {
        (0, 0)
    };

    // 2. Hand the residual to the embedded AMM, the A side first and the B side
    //    against the reserves that leaves. Output rounds down and the input is
    //    added gross, so both steps keep `k` moving up.
    let mut reserve_a = pool.reserve_a;
    let mut reserve_b = pool.reserve_b;

    let out_b = amount_out(offered_a_u64 - cross_a, reserve_a, reserve_b)?;
    reserve_a = reserve_a
        .checked_add(offered_a_u64 - cross_a)
        .ok_or(ErrorCode::MathOverflow)?;
    reserve_b = reserve_b
        .checked_sub(out_b)
        .ok_or(ErrorCode::MathOverflow)?;

    let out_a = amount_out(offered_b_u64 - cross_b, reserve_b, reserve_a)?;
    reserve_b = reserve_b
        .checked_add(offered_b_u64 - cross_b)
        .ok_or(ErrorCode::MathOverflow)?;
    reserve_a = reserve_a
        .checked_sub(out_a)
        .ok_or(ErrorCode::MathOverflow)?;

    pool.reserve_a = reserve_a;
    pool.reserve_b = reserve_b;

    // 3. Credit every unit sold with its share of the output through the
    //    accumulators, one addition per side no matter how many orders are live.
    if offered_a > 0 {
        let paid = cross_b as u128 + out_b as u128;
        pool.acc_b_per_a = pool
            .acc_b_per_a
            .checked_add(
                math_core::div_shr(paid, offered_a).map_err(|_| error!(ErrorCode::MathOverflow))?,
            )
            .ok_or(ErrorCode::MathOverflow)?;
    }
    if offered_b > 0 {
        let paid = cross_a as u128 + out_a as u128;
        pool.acc_a_per_b = pool
            .acc_a_per_b
            .checked_add(
                math_core::div_shr(paid, offered_b).map_err(|_| error!(ErrorCode::MathOverflow))?,
            )
            .ok_or(ErrorCode::MathOverflow)?;
    }

    Ok(())
}

/// Closes an order's books: it sold `sold` raw units of its input, so it is owed
/// its share of the output accumulator's move since it was created.
///
/// `sold` is the rate times the time the order was live, which the pool has
/// traded by the time this runs either way - all of it at an expiry, part of it
/// at a cancellation.
fn close_order_books(order: &mut LongTermOrder, pool: &Pool, sold: u64) -> Result<()> {
    let accumulator = if order.is_a_to_b {
        pool.acc_b_per_a
    } else {
        pool.acc_a_per_b
    };
    let accrued = accumulator
        .checked_sub(order.acc_snapshot)
        .ok_or(ErrorCode::MathOverflow)?;
    // The accumulator is a Q64.64 output-per-input rate, so the order's share is
    // `sold * rate / 2^64`.
    let proceeds = math_core::mul_shr(sold as u128, accrued)
        .map_err(|_| error!(ErrorCode::MathOverflow))?;

    order.sold = sold;
    order.proceeds_out = u64::try_from(proceeds).map_err(|_| error!(ErrorCode::MathOverflow))?;
    order.settled = true;
    Ok(())
}

/// Closes an expired order's books: the walk traded exactly `[created, expiry]`
/// at its rate, so that is the amount it sold, dust included.
fn settle_order(order: &mut LongTermOrder, pool: &Pool) -> Result<()> {
    let duration = order
        .expiry_timestamp
        .checked_sub(order.created_timestamp)
        .ok_or(ErrorCode::MathOverflow)?;
    let duration = u128::try_from(duration).map_err(|_| error!(ErrorCode::MathOverflow))?;
    let sold = u64::try_from(
        (order.sell_rate as u128)
            .checked_mul(duration)
            .ok_or(ErrorCode::MathOverflow)?,
    )
    .map_err(|_| error!(ErrorCode::MathOverflow))?;

    close_order_books(order, pool, sold)
}

// -----------------------------------------------------------------------
// EMBEDDED AMM AND SHARE MATH
// -----------------------------------------------------------------------

/// Constant-product output for `amount_in`, fee included and rounded down, so
/// the pool never pays more than the curve says.
///
/// The fee is charged on the input: the numerator counts only the part of the
/// input that buys output, while the denominator is increased by that same part
/// against the *whole* input's worth of reserves, which is what leaves the fee
/// behind in `reserve_in` and grows `k`.
fn amount_out(amount_in: u64, reserve_in: u64, reserve_out: u64) -> Result<u64> {
    if amount_in == 0 {
        return Ok(0);
    }
    require!(
        reserve_in > 0 && reserve_out > 0,
        ErrorCode::InsufficientLiquidity
    );

    let in_with_fee = (amount_in as u128)
        .checked_mul(BPS_DENOMINATOR - FEE_BPS)
        .ok_or(ErrorCode::MathOverflow)?;
    let numerator = in_with_fee
        .checked_mul(reserve_out as u128)
        .ok_or(ErrorCode::MathOverflow)?;
    let denominator = (reserve_in as u128)
        .checked_mul(BPS_DENOMINATOR)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_add(in_with_fee)
        .ok_or(ErrorCode::MathOverflow)?;

    let out = u64::try_from(numerator / denominator).map_err(|_| error!(ErrorCode::MathOverflow))?;
    require!(out < reserve_out, ErrorCode::InsufficientLiquidity);
    Ok(out)
}

/// Q64.64 marginal price of one raw unit of A, in raw units of B.
fn marginal_price(reserve_a: u64, reserve_b: u64) -> Result<u128> {
    require!(
        reserve_a > 0 && reserve_b > 0,
        ErrorCode::InsufficientLiquidity
    );
    Ok(((reserve_b as u128) << 64) / reserve_a as u128)
}

/// Value of `(amount_a, amount_b)` in raw units of A.
///
/// Both legs are raw token units and `price_x64` is the raw-unit marginal price,
/// so the two legs are comparable whatever decimals the mints use. The B leg is
/// divided by the marginal price and rounded down; the result is a plain raw
/// amount, not a Q64.64 one, which is what keeps share arithmetic in range.
fn value_in_a(amount_a: u64, amount_b: u64, price_x64: u128) -> Result<u128> {
    let b = math_core::div_shr(amount_b as u128, price_x64)
        .map_err(|_| error!(ErrorCode::MathOverflow))?;
    (amount_a as u128)
        .checked_add(b)
        .ok_or_else(|| error!(ErrorCode::MathOverflow))
}

/// `reserve * shares / supply`, rounded down and range-checked against `u64`.
fn share_of(reserve: u64, shares: u128, supply: u128) -> Result<u64> {
    require!(supply > 0, ErrorCode::InsufficientLiquidity);
    let share = (reserve as u128)
        .checked_mul(shares)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(supply)
        .ok_or(ErrorCode::MathOverflow)?;
    u64::try_from(share).map_err(|_| error!(ErrorCode::MathOverflow))
}

// -----------------------------------------------------------------------
// POOL-LEVEL RAILS (KYC / ORACLE / SETTLEMENT)
//
// The TWAMM has no spot swap, so the rails attach to the instructions that
// move value instead - and each instruction carries only the accounts its
// rails use:
//
// * `create_long_term_order` — KYC (the owner starts a trade) + oracle (the
//   walk settles virtual flow and moves the price).
// * `execute_virtual_orders` — oracle only: a permissionless crank that
//   moves the price, so it is circuit-broken, but it initiates nothing and
//   routes nothing.
// * `cancel_order` — oracle (it trades the pool up to now) + settlement
//   (its proceeds are sold tokens).
// * `withdraw_proceeds` — settlement only: nothing is traded, so there is no
//   price to check, but the proceeds must be routed into escrow.
//
// A rail that is off is passed the executing program's own ID in place of
// its accounts (Anchor's `Option` sentinel - there is no null account on
// Solana, so the program ID stands in for "unused here").
// -----------------------------------------------------------------------

/// KYC rail: the order owner must hold a valid attestation before an order
/// is escrowed. `verify_kyc` is read-only on the gate side and requires the
/// trader to be a signer - which they are, since they signed the create.
fn kyc_gate_check<'info>(
    kyc_config: &UncheckedAccount<'info>,
    owner: &Signer<'info>,
    user_kyc_account: &UncheckedAccount<'info>,
) -> Result<()> {
    let cpi_accounts = kyc_gate::cpi::accounts::VerifyKyc {
        kyc_config: kyc_config.to_account_info(),
        user: owner.to_account_info(),
        user_kyc_account: user_kyc_account.to_account_info(),
    };
    kyc_gate::cpi::verify_kyc(CpiContext::new(kyc_gate::ID, cpi_accounts))
}

/// Oracle rail, pre-trade: circuit-break against the 30-minute TWAP before
/// the pool's reserves move. The price checked is the marginal price the
/// virtual flow is about to be netted and traded at.
///
/// `check_deviation` binds the check to the pool that owns the oracle, so the
/// pool signs the CPI with its own seeds - the same way `record_observation`
/// does. The price is computed from pool state here, never taken from a caller.
fn oracle_check_pre_trade<'info>(
    pool: &Account<'info, Pool>,
    oracle_state: &UncheckedAccount<'info>,
) -> Result<()> {
    let start_price = marginal_price(pool.reserve_a, pool.reserve_b)?;
    let bump = [pool.bump];
    let pool_seeds: &[&[u8]] = &[POOL_SEED, pool.mint_a.as_ref(), pool.mint_b.as_ref(), &bump];
    let cpi_accounts = oracle::cpi::accounts::CheckDeviation {
        oracle_state: oracle_state.to_account_info(),
        pool: pool.to_account_info(),
    };
    let signer_seeds: &[&[&[u8]]] = &[pool_seeds];
    let cpi_ctx = CpiContext::new(oracle::ID, cpi_accounts).with_signer(signer_seeds);
    oracle::cpi::check_deviation(cpi_ctx, start_price)
}

/// Oracle rail, post-trade: record the settled marginal price so the TWAP
/// stays current for the next circuit-breaker check. `record_observation`
/// requires the pool PDA as a signer; the CPI supplies it via the pool's own
/// seeds, inside the transaction the pool is executing.
fn oracle_record_post_trade<'info>(
    pool: &Account<'info, Pool>,
    oracle_state: &UncheckedAccount<'info>,
) -> Result<()> {
    let settled_price = marginal_price(pool.reserve_a, pool.reserve_b)?;
    let mint_a_key = pool.mint_a;
    let mint_b_key = pool.mint_b;
    let bump = [pool.bump];
    let pool_seeds: &[&[u8]] = &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];

    let cpi_accounts = oracle::cpi::accounts::RecordObservation {
        oracle_state: oracle_state.to_account_info(),
        authority: pool.to_account_info(),
    };
    let signer_seeds: &[&[&[u8]]] = &[pool_seeds];
    let cpi_ctx = CpiContext::new(oracle::ID, cpi_accounts).with_signer(signer_seeds);
    oracle::cpi::record_observation(cpi_ctx, settled_price)
}

/// Settlement rail: sweeps `proceeds` of an order's output token into the T+N
/// escrow through the hooks program. The proceeds were just paid into the hook
/// authority's custody ATA, whose balance is now exactly this payout;
/// `hooks::after_swap` locks it in the settlement vault PDA for `trade_id`,
/// unlocking at `Clock::slot + settlement_slots`. The taker is the order
/// owner, so the Transfer Agent's signature - which binds (trade_id, taker,
/// mint, amount) - releases to the party that placed the order.
///
/// The untraded refund never goes through escrow: it was never sold, so it
/// goes straight back to the owner even on a wired pool.
#[allow(clippy::too_many_arguments)]
fn settle_proceeds<'info>(
    hooks_authority: &UncheckedAccount<'info>,
    taker: &Signer<'info>,
    custody_token_account: &UncheckedAccount<'info>,
    output_mint: &InterfaceAccount<'info, Mint>,
    settlement_vault: &UncheckedAccount<'info>,
    settlement_vault_token: &UncheckedAccount<'info>,
    settlement_config: &UncheckedAccount<'info>,
    settlement_program: &UncheckedAccount<'info>,
    associated_token_program: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
    token_program: &Interface<'info, TokenInterface>,
    trade_id: u64,
    settlement_slots: u64,
) -> Result<()> {
    let cpi_accounts = hooks::cpi::accounts::AfterSwap {
        hook_authority: hooks_authority.to_account_info(),
        taker: taker.to_account_info(),
        amm_vault: custody_token_account.to_account_info(),
        token_mint: output_mint.to_account_info(),
        settlement_vault: settlement_vault.to_account_info(),
        settlement_vault_token: settlement_vault_token.to_account_info(),
        settlement_config: settlement_config.to_account_info(),
        settlement_program: settlement_program.to_account_info(),
        token_program: token_program.to_account_info(),
        associated_token_program: associated_token_program.to_account_info(),
        system_program: system_program.to_account_info(),
    };
    // The CPI callee is the hooks program; the settlement program account in
    // the list is the hooks program's own CPI target.
    let cpi_ctx = CpiContext::new(hooks::ID, cpi_accounts);
    hooks::cpi::after_swap(cpi_ctx, trade_id, settlement_slots)
}

// -----------------------------------------------------------------------
// TRANSFERS
// -----------------------------------------------------------------------

/// Transfers `amount` into `to` and reports what `to` actually gained, because
/// a Token-2022 transfer fee can skim part of the amount in flight.
fn transfer_and_measure<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &mut InterfaceAccount<'info, TokenAccount>,
    authority: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<u64> {
    if amount == 0 {
        return Ok(0);
    }

    let before = to.amount;
    transfer_checked(
        token_program,
        from,
        mint,
        &to.to_account_info(),
        authority,
        signer_seeds,
        amount,
    )?;
    to.reload()?;

    to.amount
        .checked_sub(before)
        .ok_or_else(|| error!(ErrorCode::MathOverflow))
}

/// Pays an order out of the pool-owned vaults: its output token first, then the
/// refund of whatever input it never sold, both signed by the pool PDA.
///
/// `output_account` and `input_account` are passed in by the caller so the
/// settlement rail can route the proceeds somewhere other than the owner's
/// wallet: on a wired pool the proceeds go to the hook authority's custody
/// ATA, while the refund — input that was never sold — always goes back to
/// the owner.
///
/// Sold A and withdrawing B, plus any unsold A (and the mirror image for a
/// B-selling order), so the two vaults and the two mints swap roles with
/// `is_a_to_b`.
#[allow(clippy::too_many_arguments)]
fn pay_order_out<'info>(
    is_a_to_b: bool,
    token_program: &Interface<'info, TokenInterface>,
    mint_a: &InterfaceAccount<'info, Mint>,
    mint_b: &InterfaceAccount<'info, Mint>,
    vault_a: &InterfaceAccount<'info, TokenAccount>,
    vault_b: &InterfaceAccount<'info, TokenAccount>,
    output_account: &AccountInfo<'info>,
    input_account: &AccountInfo<'info>,
    pool: &Account<'info, Pool>,
    proceeds: u64,
    refund: u64,
) -> Result<()> {
    if proceeds == 0 && refund == 0 {
        return Ok(());
    }

    let bump = [pool.bump];
    let mint_a_key = pool.mint_a;
    let mint_b_key = pool.mint_b;
    let pool_seeds: &[&[u8]] = &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];
    let signer_seeds: &[&[&[u8]]] = &[pool_seeds];
    let pool_info = pool.to_account_info();

    let (output_vault, output_mint, input_vault, input_mint) = if is_a_to_b {
        (vault_b, mint_b, vault_a, mint_a)
    } else {
        (vault_a, mint_a, vault_b, mint_b)
    };

    if proceeds > 0 {
        transfer_checked(
            token_program,
            output_vault,
            output_mint,
            output_account,
            &pool_info,
            signer_seeds,
            proceeds,
        )?;
    }
    if refund > 0 {
        transfer_checked(
            token_program,
            input_vault,
            input_mint,
            input_account,
            &pool_info,
            signer_seeds,
            refund,
        )?;
    }

    Ok(())
}

/// `transfer_checked` (which validates the mint's decimals) for either the
/// legacy SPL Token program or Token-2022.
///
/// `to` is a plain `AccountInfo` because not every caller holds a typed
/// token account: the settlement rail pays proceeds into an unchecked custody
/// account whose type is pinned by address (the hook authority's ATA) and
/// re-validated downstream by the hooks and settlement programs. The token
/// program itself still verifies the account is a token account of `mint`.
fn transfer_checked<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked {
                from: from.to_account_info(),
                mint: mint.to_account_info(),
                to: to.clone(),
                authority: authority.clone(),
            },
            signer_seeds,
        ),
        amount,
        mint.decimals,
    )
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES
// -----------------------------------------------------------------------

/// The pool: the embedded AMM's reserves, the live order flow on each side, the
/// accumulators orders settle against, and the head of the order list.
#[account]
pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    /// Reserves of the embedded AMM, in raw token units.
    pub reserve_a: u64,
    pub reserve_b: u64,
    /// Timestamp every virtual trade up to which has been settled.
    pub last_execution_timestamp: i64,
    /// Total raw A per second being sold by live orders.
    pub sell_rate_a: u64,
    /// Total raw B per second being sold by live orders.
    pub sell_rate_b: u64,
    /// Output B per raw A virtually sold since the pool was created, as a Q64.64
/// rate. An order's claim on it is `sold * acc_b_per_a >> 64`.
    pub acc_b_per_a: u128,
    /// Output A per raw B virtually sold since the pool was created, Q64.64.
    pub acc_a_per_b: u128,
    /// First order in the expiry-sorted list; all zeroes when there is none.
    pub order_head: Pubkey,
    /// Expiry cached for `order_head`, which is what lets a walk prove that it
    /// was handed every expired order.
    pub order_head_expiry: i64,
    /// Total LP shares outstanding.
    pub lp_supply: u128,
    /// The only account that can pause the pool or rotate itself.
    pub authority: Pubkey,
    /// While paused the pool refuses new orders and deposits. Executions and
    /// withdrawals stay open.
    pub paused: bool,
    // -----------------------------------------------------------------
    // Pool-level wiring (all optional; set at `initialize_pool`).
    // -----------------------------------------------------------------

    /// KYC gate config PDA (`kyc_gate::KYC_CONFIG_SEED`). `Pubkey::default()`
    /// means the pool does not require KYC; otherwise order creation CPIs
    /// `kyc_gate::verify_kyc` for the order owner first.
    pub kyc_config: Pubkey,
    /// Oracle state PDA (`oracle` program, seed `[b"oracle", pool_key]`).
    /// When set, every instruction that settles virtual flow CPIs
    /// `check_deviation` with the pre-trade marginal price and
    /// `record_observation` with the post-trade one.
    pub oracle_state: Pubkey,
    /// Hooks program authority PDA (`[b"hook_authority"]`). When set, order
    /// proceeds are NOT paid to the order owner: they are routed to a
    /// per-trade custody account owned by this PDA and locked in the T+N
    /// settlement escrow. The untraded refund still goes back to the owner.
    pub hooks_authority: Pubkey,
    /// T+N length in slots, relative to the payout slot, for proceeds routed
    /// through settlement. Only meaningful when `hooks_authority` is set.
    pub settlement_slots: u64,
    pub bump: u8,
    pub vault_a_bump: u8,
    pub vault_b_bump: u8,
}

impl Pool {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 8 + 8 + 8 + 8 + 16 + 16 + 32 + 8 + 16 + 32 + 1
        + 32 + 32 + 32 + 8 + 1 + 1 + 1;
}

/// One long-term order: a rate, an expiry, and what the pool owes it.
#[account]
pub struct LongTermOrder {
    pub pool: Pubkey,
    pub owner: Pubkey,
    /// Caller-chosen tag, unique per owner and pool.
    pub order_id: u64,
    /// Selling A for B when true, B for A when false.
    pub is_a_to_b: bool,
    /// Raw input sold per second.
    pub sell_rate: u64,
    /// Raw input escrowed at creation.
    pub amount_in: u64,
    /// Raw input the pool actually sold, fixed when the order settles.
    pub sold: u64,
    pub duration: i64,
    pub created_timestamp: i64,
    pub expiry_timestamp: i64,
    /// Output-per-input accumulator as it stood when the order was created, so
    /// it is owed nothing for flow that came before it.
    pub acc_snapshot: u128,
    /// Output owed to the owner, fixed when the order settles.
    pub proceeds_out: u64,
    /// Set once the pool has met the expiry and closed the order's books.
    pub settled: bool,
    /// Next order in the expiry-sorted list; all zeroes at the end.
    pub next: Pubkey,
    /// Expiry cached for `next`.
    pub next_expiry: i64,
    pub bump: u8,
}

impl LongTermOrder {
    pub const LEN: usize =
        8 + 32 + 32 + 8 + 1 + 8 + 8 + 8 + 8 + 8 + 8 + 16 + 8 + 1 + 32 + 8 + 1;
}

/// An LP's claim on the embedded AMM's reserves.
#[account]
pub struct LpPosition {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub shares: u128,
    pub bump: u8,
}

impl LpPosition {
    pub const LEN: usize = 8 + 32 + 32 + 16 + 1;
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializePool<'info> {
    /// Pays for the pool, its vaults and the position, and receives the genesis
    /// LP shares. This is not the pool's admin - the `authority` in the
    /// instruction is.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init,
        payer = payer,
        space = Pool::LEN,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// Pool-owned vault for mint A.
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_A_SEED, pool.key().as_ref(), mint_a.key().as_ref()],
        bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Pool-owned vault for mint B.
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_B_SEED, pool.key().as_ref(), mint_b.key().as_ref()],
        bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The genesis LP's position.
    #[account(
        init,
        payer = payer,
        space = LpPosition::LEN,
        seeds = [LP_SEED, pool.key().as_ref(), payer.key().as_ref()],
        bump
    )]
    pub lp_position: Box<Account<'info, LpPosition>>,

    #[account(mut)]
    pub payer_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub payer_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The pool's oracle state PDA, created here with the pool PDA signing.
    /// Optional: an off-rail pool passes this program's own ID as the sentinel
    /// (Anchor reads that as `None`, so the `mut` requirement is skipped), while
    /// a wired pool passes the writable `[b"oracle", pool]` PDA.
    /// CHECK: address-pinned to `[b"oracle", pool]` in the handler, then created
    /// by the oracle program's own `initialize` CPI.
    #[account(mut)]
    pub oracle_state: Option<UncheckedAccount<'info>>,

    /// The KYC gate program, required iff `kyc_config` is set. Validated
    /// against the config's owner in the handler.
    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    /// The oracle program, required iff `oracle_state` is set. Validated
    /// against the state's owner in the handler.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,

    /// The hooks program, required iff `hooks_authority` is set. Validated
    /// against the authority's owner in the handler.
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct AddLiquidity<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    #[account(
        init_if_needed,
        payer = owner,
        space = LpPosition::LEN,
        seeds = [LP_SEED, pool.key().as_ref(), owner.key().as_ref()],
        bump
    )]
    pub position: Box<Account<'info, LpPosition>>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [VAULT_A_SEED, pool.key().as_ref(), mint_a.key().as_ref()],
        bump = pool.vault_a_bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [VAULT_B_SEED, pool.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.vault_b_bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RemoveLiquidity<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    #[account(
        mut,
        seeds = [LP_SEED, pool.key().as_ref(), owner.key().as_ref()],
        bump = position.bump
    )]
    pub position: Box<Account<'info, LpPosition>>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [VAULT_A_SEED, pool.key().as_ref(), mint_a.key().as_ref()],
        bump = pool.vault_a_bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [VAULT_B_SEED, pool.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.vault_b_bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    pub owner: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(order_id: u64)]
pub struct CreateLongTermOrder<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// `order_id` is part of the address, so the same owner can run several
    /// orders on one pool, and a re-used id simply fails to create.
    #[account(
        init,
        payer = owner,
        space = LongTermOrder::LEN,
        seeds = [ORDER_SEED, pool.key().as_ref(), owner.key().as_ref(), &order_id.to_le_bytes()],
        bump
    )]
    pub order: Box<Account<'info, LongTermOrder>>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [VAULT_A_SEED, pool.key().as_ref(), mint_a.key().as_ref()],
        bump = pool.vault_a_bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [VAULT_B_SEED, pool.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.vault_b_bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    /// KYC gate config, required iff `pool.kyc_config` is set (pass this
    /// program's own ID when the rail is off). The handler checks it against
    /// the pool's recorded policy; the KYC program validates its structure in
    /// the CPI. Optional: the program-ID sentinel means the rail is off.
    /// CHECK: equality-checked against `pool.kyc_config` in the handler.
    #[account(mut)]
    pub kyc_config: Option<UncheckedAccount<'info>>,

    /// The order owner's KYC attestation account, required iff the KYC rail
    /// is on.
    /// CHECK: validated inside `kyc_gate::verify_kyc` via CPI.
    pub user_kyc_account: UncheckedAccount<'info>,

    /// Oracle state, required iff `pool.oracle_state` is set. The handler
    /// CPIs `check_deviation` before the walk and `record_observation` after
    /// it. Optional: the program-ID sentinel means the rail is off.
    /// CHECK: seeds-verified as `[b"oracle", pool]` in the handler.
    #[account(mut)]
    pub oracle_state: Option<UncheckedAccount<'info>>,

    /// The KYC gate program, required whenever the KYC rail is on. The handler
    /// CPIs into it, and a CPI target has to appear in the instruction's account
    /// list for the runtime to resolve it.
    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    /// The oracle program, required whenever the oracle rail is on. Both oracle
    /// CPIs (`check_deviation`, `record_observation`) go through it.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    // Remaining accounts: the order list from the pool's head up to the first
    // order that expires after this one.
}

#[derive(Accounts)]
pub struct ExecuteVirtualOrders<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    /// Oracle state, required iff `pool.oracle_state` is set (pass this
    /// program's own ID when the rail is off). Optional, so the sentinel skips
    /// the `mut` requirement.
    /// CHECK: seeds-verified as `[b"oracle", pool]` in the handler.
    #[account(mut)]
    pub oracle_state: Option<UncheckedAccount<'info>>,

    /// The oracle program, required whenever the oracle rail is on. Both oracle
    /// CPIs (`check_deviation`, `record_observation`) go through it.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,
    // Remaining accounts: the expired orders at the head of the list, in list
    // order.
}

#[derive(Accounts)]
#[instruction(order_id: u64)]
pub struct WithdrawProceeds<'info> {
    #[account(
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// Closed on withdrawal, so its rent goes back to the owner.
    #[account(
        mut,
        close = owner,
        seeds = [ORDER_SEED, pool.key().as_ref(), owner.key().as_ref(), &order_id.to_le_bytes()],
        bump = order.bump
    )]
    pub order: Box<Account<'info, LongTermOrder>>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [VAULT_A_SEED, pool.key().as_ref(), mint_a.key().as_ref()],
        bump = pool.vault_a_bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [VAULT_B_SEED, pool.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.vault_b_bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    /// Hooks authority PDA, required iff `pool.hooks_authority` is set (pass
    /// this program's own ID when the rail is off). Owns the custody account
    /// the proceeds are routed into, and **pays for the escrow** the hooks CPI
    /// creates (it is the settlement program's rent payer), so it must be
    /// writable here or that CPI escalates its privileges and fails. Optional,
    /// so the sentinel can stand in for it when the rail is off — the runtime
    /// demotes an executable account to read-only, and `mut` would then fail.
    /// CHECK: seeds-verified as the hooks program's `[b"hook_authority"]`
    /// PDA in the handler.
    #[account(mut)]
    pub hooks_authority: Option<UncheckedAccount<'info>>,

    /// The settlement custody account — the hook authority's ATA for the
    /// output mint — required iff the settlement rail is on. It receives the
    /// proceeds; the same transaction then locks them in escrow.
    /// CHECK: address-pinned to the hook authority's ATA in the handler; the
    /// hooks and settlement programs re-check it as the sweep's source.
    /// Optional: the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub custody_token_account: Option<UncheckedAccount<'info>>,

    /// The settlement clearinghouse config, required iff the settlement rail
    /// is on. Passed through to the hooks CPI.
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub settlement_config: UncheckedAccount<'info>,

    /// The settlement program, required iff the settlement rail is on.
    /// CHECK: validated by the hooks program (typed `Program<Settlement>`).
    pub settlement_program: UncheckedAccount<'info>,

    /// The settlement vault PDA for `order_id`, required iff the settlement
    /// rail is on. The hooks program re-derives and checks it.
    /// CHECK: seeds-verified by the hooks program against `trade_id`. Optional:
    /// the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub settlement_vault: Option<UncheckedAccount<'info>>,

    /// The escrow token account of the settlement vault PDA, required iff
    /// the settlement rail is on. Created by the settlement program.
    /// CHECK: address-pinned to the vault's ATA by the hooks program. Optional:
    /// the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub settlement_vault_token: Option<UncheckedAccount<'info>>,

    /// The associated-token program, required iff the settlement rail is on
    /// (the settlement program creates the escrow ATA through it).
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub associated_token_program: UncheckedAccount<'info>,

    /// The hooks program, required whenever the settlement rail is on (it is
    /// the program that locks the proceeds in escrow).
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(order_id: u64)]
pub struct CancelOrder<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// Closed on cancellation, so its rent goes back to the owner.
    #[account(
        mut,
        close = owner,
        seeds = [ORDER_SEED, pool.key().as_ref(), owner.key().as_ref(), &order_id.to_le_bytes()],
        bump = order.bump
    )]
    pub order: Box<Account<'info, LongTermOrder>>,

    pub mint_a: Box<InterfaceAccount<'info, Mint>>,
    pub mint_b: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [VAULT_A_SEED, pool.key().as_ref(), mint_a.key().as_ref()],
        bump = pool.vault_a_bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [VAULT_B_SEED, pool.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.vault_b_bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    /// Oracle state, required iff `pool.oracle_state` is set (pass this
    /// program's own ID when the rail is off). The handler CPIs
    /// `check_deviation` before the walk and `record_observation` after it.
    /// Optional: the program-ID sentinel means the rail is off.
    /// CHECK: seeds-verified as `[b"oracle", pool]` in the handler.
    #[account(mut)]
    pub oracle_state: Option<UncheckedAccount<'info>>,

    /// Hooks authority PDA, required iff `pool.hooks_authority` is set (pass
    /// this program's own ID when the rail is off). Owns the custody account
    /// the proceeds are routed into, and **pays for the escrow** the hooks CPI
    /// creates (it is the settlement program's rent payer), so it must be
    /// writable here or that CPI escalates its privileges and fails. Optional,
    /// so the sentinel can stand in for it when the rail is off — the runtime
    /// demotes an executable account to read-only, and `mut` would then fail.
    /// CHECK: seeds-verified as the hooks program's `[b"hook_authority"]`
    /// PDA in the handler.
    #[account(mut)]
    pub hooks_authority: Option<UncheckedAccount<'info>>,

    /// The settlement custody account — the hook authority's ATA for the
    /// output mint — required iff the settlement rail is on.
    /// CHECK: address-pinned to the hook authority's ATA in the handler.
    /// Optional: the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub custody_token_account: Option<UncheckedAccount<'info>>,

    /// The settlement clearinghouse config, required iff the settlement rail
    /// is on. Passed through to the hooks CPI.
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub settlement_config: UncheckedAccount<'info>,

    /// The settlement program, required iff the settlement rail is on.
    /// CHECK: validated by the hooks program (typed `Program<Settlement>`).
    pub settlement_program: UncheckedAccount<'info>,

    /// The settlement vault PDA for `order_id`, required iff the settlement
    /// rail is on. The hooks program re-derives and checks it.
    /// CHECK: seeds-verified by the hooks program against `trade_id`. Optional:
    /// the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub settlement_vault: Option<UncheckedAccount<'info>>,

    /// The escrow token account of the settlement vault PDA, required iff
    /// the settlement rail is on. Created by the settlement program.
    /// CHECK: address-pinned to the vault's ATA by the hooks program. Optional:
    /// the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub settlement_vault_token: Option<UncheckedAccount<'info>>,

    /// The associated-token program, required iff the settlement rail is on.
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub associated_token_program: UncheckedAccount<'info>,

    /// The oracle program, required whenever the oracle rail is on. The handler
    /// trades the pool up to now before cancelling, so both oracle CPIs run.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,

    /// The hooks program, required whenever the settlement rail is on (it is
    /// the program that locks the proceeds in escrow).
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    // Remaining accounts: the order list from the pool's head up to, but not
    // including, the order being cancelled.
}

#[derive(Accounts)]
pub struct TripOracle<'info> {
    #[account(
        seeds = [POOL_SEED, pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// The pool's oracle state PDA. Writable: a trip latches `is_paused`.
    /// CHECK: address-pinned to `[b"oracle", pool]` in the handler.
    #[account(mut)]
    pub oracle_state: UncheckedAccount<'info>,

    /// The oracle program. The CPI target must appear in the instruction's
    /// account list for the runtime to resolve it.
    pub oracle_program: Program<'info, oracle::program::Oracle>,
}

/// Admin-only instructions: the pool's authority and nobody else.
#[derive(Accounts)]
pub struct Admin<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump,
        has_one = authority
    )]
    pub pool: Box<Account<'info, Pool>>,

    pub authority: Signer<'info>,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Duration must be greater than zero")]
    ZeroDuration,
    #[msg("Duration is longer than the pool allows")]
    DurationTooLong,
    #[msg("Order rate rounds to zero: the amount is smaller than its duration in seconds")]
    RateTooSmall,
    #[msg("Insufficient liquidity")]
    InsufficientLiquidity,
    #[msg("Not enough LP shares")]
    InsufficientShares,
    #[msg("Slippage exceeded")]
    SlippageExceeded,
    #[msg("Pool is paused")]
    PoolPaused,
    #[msg("Account does not belong to this owner")]
    Unauthorized,
    #[msg("Order id does not match the account")]
    WrongOrder,
    #[msg("Order belongs to a different pool")]
    WrongOrderPool,
    #[msg("Order has not been settled yet: run execute_virtual_orders first")]
    OrderNotSettled,
    #[msg("Order has not expired yet")]
    OrderNotExpired,
    #[msg("Order has expired: settle it with execute_virtual_orders, then withdraw its proceeds")]
    OrderExpired,
    #[msg("Order has already been settled: withdraw its proceeds instead")]
    OrderAlreadySettled,
    #[msg("Order list walk got an account that is not the next order")]
    OrderListMismatch,
    #[msg("Expired order was left out of the walk")]
    MissingExpiredOrder,
    #[msg("Order account must be passed as writable")]
    OrderAccountNotWritable,
    #[msg("Clock moved backwards")]
    ClockWentBackwards,
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
    #[msg("Order id must be non-zero when settlement routing is on")]
    InvalidTradeId,
    #[msg("Oracle deviation threshold must be non-zero when the rail is on, zero when off")]
    InvalidOracleThreshold,
    #[msg("This pool has no oracle configured")]
    OracleNotConfigured,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a Q64.64 value into an `f64`, for comparison against a reference.
    fn as_f64(value: u128) -> f64 {
        value as f64 / 2f64.powi(64)
    }

    /// A pool with the given reserves and nothing else going on.
    fn pool_with(reserve_a: u64, reserve_b: u64) -> Pool {
        Pool {
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
            reserve_a,
            reserve_b,
            last_execution_timestamp: 0,
            sell_rate_a: 0,
            sell_rate_b: 0,
            acc_b_per_a: 0,
            acc_a_per_b: 0,
            order_head: Pubkey::default(),
            order_head_expiry: 0,
            lp_supply: 0,
            authority: Pubkey::default(),
            paused: false,
            bump: 0,
            vault_a_bump: 0,
            vault_b_bump: 0,
            kyc_config: Pubkey::default(),
            oracle_state: Pubkey::default(),
            hooks_authority: Pubkey::default(),
            settlement_slots: 0,
        }
    }

    fn order_selling_a(sell_rate: u64, created: i64, expiry: i64, acc_snapshot: u128) -> LongTermOrder {
        LongTermOrder {
            pool: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            order_id: 1,
            is_a_to_b: true,
            sell_rate,
            amount_in: sell_rate * (expiry - created) as u64,
            sold: 0,
            duration: expiry - created,
            created_timestamp: created,
            expiry_timestamp: expiry,
            acc_snapshot,
            proceeds_out: 0,
            settled: false,
            next: Pubkey::default(),
            next_expiry: 0,
            bump: 0,
        }
    }

    /// What an order settles for: the amount it sold, times the move in the
    /// output-per-input accumulator over its life.
    fn claim(sold: u64, acc_delta: u128) -> u64 {
        u64::try_from(math_core::mul_shr(sold as u128, acc_delta).unwrap()).unwrap()
    }

    /// Product of the reserves. Fees and rounding may only ever raise it.
    fn k(pool: &Pool) -> u128 {
        (pool.reserve_a as u128) * (pool.reserve_b as u128)
    }

    // -- embedded AMM ---------------------------------------------------

    #[test]
    fn amount_out_matches_the_constant_product_formula() {
        let out = amount_out(1_000, 1_000_000, 2_000_000).unwrap();
        // The textbook 0.3% form: (in * 997 * R) / (r * 1000 + in * 997).
        let reference = (997u128 * 1_000 * 2_000_000) / (1_000 * 1_000_000 + 997 * 1_000);
        assert_eq!(out as u128, reference);
    }

    #[test]
    fn amount_out_never_empties_the_output_reserve() {
        let out = amount_out(u64::MAX, 1_000, 1_000).unwrap();
        assert!(out < 1_000, "a swap must leave the reserve it drains non-empty");
    }

    #[test]
    fn zero_input_pays_nothing_and_needs_no_liquidity() {
        assert_eq!(amount_out(0, 0, 0).unwrap(), 0);
    }

    #[test]
    fn fees_lift_k() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 100;
        let before = k(&pool);

        execute_segment(&mut pool, 100).unwrap();

        assert!(k(&pool) > before, "the AMM's fee must accrue to the reserves");
        assert_eq!(pool.reserve_a, 1_010_000, "the whole input stays in reserve A");
    }

    // -- netting --------------------------------------------------------

    #[test]
    fn opposing_flow_nets_out_at_the_marginal_price() {
        // Equal and opposite flow at parity: every unit an A-seller offers is
        // met by a B-seller, so the curve is never touched.
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 10;
        pool.sell_rate_b = 10;

        execute_segment(&mut pool, 100).unwrap();

        assert_eq!((pool.reserve_a, pool.reserve_b), (1_000_000, 1_000_000));
        // Both sides sold 1 000, and both were paid 1 000 at a price of 1.0.
        assert_eq!(as_f64(pool.acc_b_per_a), 1.0);
        assert_eq!(as_f64(pool.acc_a_per_b), 1.0);
        assert_eq!(claim(1_000, pool.acc_b_per_a), 1_000);
    }

    #[test]
    fn crossing_converts_at_the_current_price_not_at_parity() {
        // Two B per A: half as much B buys the same A.
        let mut pool = pool_with(1_000_000, 2_000_000);
        pool.sell_rate_a = 10;
        pool.sell_rate_b = 20;

        execute_segment(&mut pool, 100).unwrap();

        assert_eq!((pool.reserve_a, pool.reserve_b), (1_000_000, 2_000_000));
        assert_eq!(as_f64(pool.acc_b_per_a), 2.0, "1 000 A bought 2 000 B");
        assert_eq!(as_f64(pool.acc_a_per_b), 0.5, "2 000 B bought 1 000 A");
    }

    #[test]
    fn only_the_imbalance_reaches_the_amm() {
        // 1 000 A offered against 400 A worth of B: 400 crosses, 600 trades.
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 10;
        pool.sell_rate_b = 4;

        execute_segment(&mut pool, 100).unwrap();

        assert_eq!(pool.reserve_a, 1_000_600, "only the residual A was added");
        let out_b = amount_out(600, 1_000_000, 1_000_000).unwrap();
        assert_eq!(pool.reserve_b, 1_000_000 - out_b);
        // The A side was paid the crossed B plus the AMM's output; the B side
        // only ever got the crossed A, all of it at the price of 1.0.
        assert_eq!(as_f64(pool.acc_a_per_b), 1.0, "400 B bought 400 A");
        let claimed = claim(1_000, pool.acc_b_per_a);
        assert!(
            claimed <= 400 + out_b && claimed + 1 >= 400 + out_b,
            "the A side is owed the crossed B plus the AMM output ({claimed})"
        );
    }

    #[test]
    fn one_sided_flow_is_priced_by_the_curve() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 10;

        execute_segment(&mut pool, 100).unwrap();

        let out_b = amount_out(1_000, 1_000_000, 1_000_000).unwrap();
        assert_eq!(pool.reserve_a, 1_001_000);
        assert_eq!(pool.reserve_b, 1_000_000 - out_b);
        // The accumulator rounds down twice (once into a rate, once out of it),
        // so a claim can come up a unit short of the AMM's output, never over.
        let claimed = claim(1_000, pool.acc_b_per_a);
        assert!(claimed <= out_b && claimed + 1 >= out_b, "claim {claimed} vs {out_b}");
    }

    #[test]
    fn a_segment_with_no_flow_is_a_no_op() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        execute_segment(&mut pool, 100).unwrap();
        execute_segment(&mut pool, 0).unwrap();
        assert_eq!((pool.reserve_a, pool.reserve_b), (1_000_000, 1_000_000));
        assert_eq!((pool.acc_b_per_a, pool.acc_a_per_b), (0, 0));
    }

    #[test]
    fn a_longer_segment_pays_less_than_many_short_ones_of_the_same_length() {
        // Both settle 1 000 A of one-sided flow: the split one trades it in ten
        // pieces, the single one in one. The split one is never *worse* by more
        // than the extra rounding it pays for, and it lets the price move
        // between pieces, which is the whole point of a TWAMM.
        let mut split = pool_with(1_000_000, 1_000_000);
        split.sell_rate_a = 10;
        for _ in 0..10 {
            execute_segment(&mut split, 10).unwrap();
        }

        let mut lump = pool_with(1_000_000, 1_000_000);
        lump.sell_rate_a = 100;
        execute_segment(&mut lump, 10).unwrap();

        let split_out = claim(1_000, split.acc_b_per_a);
        let lump_out = claim(1_000, lump.acc_b_per_a);
        assert!(split_out >= lump_out - 10, "{split_out} B split vs {lump_out} B lumped");
    }

    #[test]
    fn an_empty_pool_refuses_to_price_flow() {
        let mut pool = pool_with(0, 0);
        pool.sell_rate_a = 10;
        assert!(execute_segment(&mut pool, 100).is_err());
    }

    // -- proceeds -------------------------------------------------------

    #[test]
    fn proceeds_are_proportional_to_flow_sold() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 40; // two orders of 10 and 30 per second

        execute_segment(&mut pool, 100).unwrap();

        let acc = pool.acc_b_per_a;
        let small = claim(10 * 100, acc);
        let large = claim(30 * 100, acc);
        // Within a unit: rounding happens once per claim.
        let error = (large as f64 - 3.0 * small as f64).abs();
        assert!(error <= 3.0, "expected 3:1, got {large}:{small}");
    }

    #[test]
    fn an_order_is_owed_nothing_for_flow_from_before_it_existed() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 10;

        execute_segment(&mut pool, 100).unwrap();
        let snapshot = pool.acc_b_per_a;

        // A second order joins here, selling the same rate.
        execute_segment(&mut pool, 100).unwrap();

        let incumbent = claim(1_000, pool.acc_b_per_a);
        let newcomer = claim(1_000, pool.acc_b_per_a - snapshot);
        assert!(
            (newcomer as f64 - claim(1_000, snapshot) as f64).abs() <= 2.0,
            "the newcomer earns only its own segment ({newcomer})"
        );
        assert!(
            (incumbent as f64 - 2.0 * newcomer as f64).abs() <= 4.0,
            "the incumbent earns both segments ({incumbent} vs {newcomer})"
        );
    }

    #[test]
    fn settle_order_pays_the_orders_share_of_the_accumulator() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 100;
        execute_segment(&mut pool, 1_000).unwrap();

        let mut order = order_selling_a(100, 0, 1_000, 0);
        settle_order(&mut order, &pool).unwrap();

        assert!(order.settled);
        assert_eq!(order.sold, 100_000);
        assert_eq!(order.proceeds_out as u128, claim(100_000, pool.acc_b_per_a) as u128);
        assert_eq!(order.amount_in - order.sold, 0, "100 per second for 1 000s");
    }

    #[test]
    fn a_short_rate_leaves_a_refundable_remainder() {
        // 1 000 units over 3 seconds is 333 per second, so 1 unit of the escrow
        // is never sold and comes back at withdrawal.
        let rate = 1_000 / 3;
        assert_eq!(rate, 333);
        let mut order = order_selling_a(0, 0, 0, 0);
        order.sell_rate = rate;
        order.amount_in = 1_000;
        order.created_timestamp = 0;
        order.expiry_timestamp = 3;

        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = rate;
        execute_segment(&mut pool, 3).unwrap();
        settle_order(&mut order, &pool).unwrap();

        assert_eq!(order.sold, 999);
        assert_eq!(order.amount_in - order.sold, 1);
    }

    #[test]
    fn cancelling_credits_what_the_order_sold_and_refunds_the_rest() {
        // 1 000 units over 10 seconds of selling, cancelled after 4.
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 100;
        execute_segment(&mut pool, 4).unwrap();

        let mut order = order_selling_a(100, 0, 10, 0);
        order.amount_in = 1_000;
        close_order_books(&mut order, &pool, 100 * 4).unwrap();

        assert!(order.settled);
        assert_eq!(order.sold, 400);
        assert_eq!(
            order.amount_in - order.sold,
            600,
            "the escrow it never sold comes back"
        );

        // The claim is the pool's own book-keeping: for one-sided flow, every
        // unit the A side sold is paid out of reserve B and nothing else is.
        let paid_by_the_curve = (1_000_000 - pool.reserve_b) as u128;
        let proceeds = order.proceeds_out as u128;
        assert!(
            proceeds <= paid_by_the_curve && paid_by_the_curve - proceeds <= 1,
            "proceeds {proceeds} vs output {paid_by_the_curve}"
        );
    }

    #[test]
    fn a_cancellation_never_pays_out_more_than_the_escrow() {
        // Whatever second an order is cancelled at, what it sold plus what it is
        // refunded is exactly what it escrowed.
        let amount_in = 1_000u64;
        let rate = amount_in / 10; // a 10 second order
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = rate;

        for lived in 0..=10u64 {
            let mut order = order_selling_a(rate, 0, 10, 0);
            order.amount_in = amount_in;
            close_order_books(&mut order, &pool, rate * lived).unwrap();

            assert_eq!(order.sold + (amount_in - order.sold), amount_in);
            assert!(order.sold <= amount_in, "sold {} > escrow", order.sold);
        }
    }

    #[test]
    fn the_output_accumulator_is_the_one_for_the_side_that_sold() {
        // A B-selling order is paid in A, so it settles against `acc_a_per_b`.
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_b = 100;
        execute_segment(&mut pool, 1_000).unwrap();

        let mut order = order_selling_a(100, 0, 1_000, 0);
        order.is_a_to_b = false;
        settle_order(&mut order, &pool).unwrap();

        assert_eq!(order.sold, 100_000);
        assert_eq!(order.proceeds_out as u128, claim(100_000, pool.acc_a_per_b) as u128);
        assert_ne!(pool.acc_a_per_b, pool.acc_b_per_a);
    }

    // -- the TWAMM property ---------------------------------------------

    #[test]
    fn slicing_is_path_neutral_against_a_static_curve() {
        const SIZE: u64 = 100_000;
        const SEGMENTS: u64 = 1_000;

        // A TWAMM: 100 A per second for 1 000 seconds, settled every second.
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = SIZE / SEGMENTS;

        // The seller's side of the books, segment by segment: it is owed the
        // rates of the segment it was live for, not one rate for the total.
        let mut owed = 0u64;
        for _ in 0..SEGMENTS {
            let before = pool.acc_b_per_a;
            execute_segment(&mut pool, 1).unwrap();
            owed += claim(SIZE / SEGMENTS, pool.acc_b_per_a - before);
        }
        let sliced_out = 1_000_000 - pool.reserve_b;

        // The same size in one shot. Against a curve that does not move, the
        // sliced total matches it closely - the gap is the rounding each
        // segment pays for - which is what makes an order's fill time a choice
        // rather than a cost.
        let one_shot_out = amount_out(SIZE, 1_000_000, 1_000_000).unwrap();
        assert!(
            sliced_out as f64 >= one_shot_out as f64 * 0.99,
            "slicing ({sliced_out} B) should stay within 1% of one swap ({one_shot_out} B)"
        );
        // And every unit the A side sold is claimable: the pool never credits
        // itself more than rounding dust along the way.
        assert!(
            owed <= sliced_out && sliced_out - owed <= SEGMENTS,
            "claims {owed} vs output {sliced_out}"
        );
    }

    #[test]
    fn netting_beats_settling_both_sides_against_the_amm() {
        // Equal and opposite flow at parity. Netted, each side is paid the
        // other's token at 1.0: no fee, no impact. Traded through the AMM one
        // side after the other, the same flow would pay the curve's spread.
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 10;
        pool.sell_rate_b = 10;

        execute_segment(&mut pool, 100).unwrap();

        let netted = claim(1_000, pool.acc_b_per_a);
        let through_the_amm = amount_out(1_000, 1_000_000, 1_000_000).unwrap();
        assert_eq!(netted, 1_000);
        assert!(
            netted > through_the_amm,
            "crossing pays {netted} B, the curve would only pay {through_the_amm} B"
        );
        assert_eq!(pool.reserve_a, 1_000_000, "crossed flow never touches the curve");
    }

    #[test]
    fn every_segment_keeps_k_moving_up() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 7;
        pool.sell_rate_b = 3;

        let mut previous = k(&pool);
        for _ in 0..500 {
            execute_segment(&mut pool, 1).unwrap();
            let current = k(&pool);
            assert!(current >= previous, "k fell from {previous} to {current}");
            previous = current;
        }
    }

    // -- LP shares ------------------------------------------------------

    #[test]
    fn genesis_shares_are_the_deposits_value_in_a() {
        // At a fresh pool the deposit sets the price, and the two legs are worth
        // the same in A, so the supply starts at twice the A leg.
        let price = marginal_price(1_000, 1_000).unwrap();
        assert_eq!(value_in_a(1_000, 1_000, price).unwrap(), 2_000);

        let price = marginal_price(1_000, 4_000).unwrap();
        assert_eq!(value_in_a(1_000, 4_000, price).unwrap(), 2_000);
    }

    #[test]
    fn depositing_at_the_marginal_price_does_not_move_the_share_price() {
        let price = marginal_price(500_000, 1_500_000).unwrap();
        let supply = value_in_a(500_000, 1_500_000, price).unwrap();
        let reserve_value = value_in_a(500_000, 1_500_000, price).unwrap();

        // A deposit at the pool's own ratio buys exactly its pro-rata supply.
        let deposit = value_in_a(50_000, 150_000, price).unwrap();
        let shares = deposit * supply / reserve_value;
        assert_eq!(shares, deposit);
        assert_eq!(shares * 10, supply);
    }

    #[test]
    fn shares_are_paid_out_pro_rata_and_round_down() {
        assert_eq!(share_of(1_000_000, 1, 4).unwrap(), 250_000);
        // Rounds down, so the pool never pays out more than it holds.
        assert_eq!(share_of(5, 1, 3).unwrap(), 1);
        assert_eq!(share_of(100, 0, 3).unwrap(), 0);
    }

    #[test]
    fn a_zero_supply_pays_nothing_out() {
        assert!(share_of(1_000, 1, 0).is_err());
    }

    // -- list bookkeeping -----------------------------------------------

    #[test]
    fn the_accumulator_only_moves_on_flow() {
        let mut pool = pool_with(1_000_000, 1_000_000);
        pool.sell_rate_a = 10;
        execute_segment(&mut pool, 100).unwrap();

        let acc = pool.acc_b_per_a;
        // A one-sided order that has already missed a period starts from the
        // accumulator as it stands, which is what makes the snapshot exact.
        let mut late = order_selling_a(10, 100, 200, acc);
        execute_segment(&mut pool, 100).unwrap();
        settle_order(&mut late, &pool).unwrap();

        assert!(late.proceeds_out > 0);
        assert!(late.sold == 1_000);
    }
}
