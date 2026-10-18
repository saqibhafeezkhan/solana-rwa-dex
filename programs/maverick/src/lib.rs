//! # Maverick Dynamic Distribution AMM (DDAMM)
//!
//! A bin AMM whose liquidity follows the price. Bins are priced exactly as in
//! the DLMM program - `1.0001 ^ (bin_id * bin_step)`, scaled into raw token
//! units so a 6-decimal stablecoin quoted against a 9-decimal RWA prices
//! correctly - and a swap fills each bin at its fixed price, crossing to the
//! next bin in the direction of trade whenever one runs out of the output
//! token. Bins are packed into `BinArray` accounts, so a band of prices costs
//! one account instead of one per bin.
//!
//! What makes it Maverick rather than a plain DLMM is that bins move.
//!
//! ## Bin kinds
//!
//! Every bin carries a kind, chosen by the LP who funds it:
//!
//! - `Static` - the bin never moves. Its price is fixed for as long as it holds
//!   liquidity, so an LP who wants a fixed quote can have one.
//! - `Right` - the bin follows a rising price to the right.
//! - `Left` - the bin follows a falling price to the left.
//! - `Both` - the bin follows the price in either direction.
//!
//! Once the price has traded through a bin, the bin is out of range and holds
//! only one token: a bin below the active bin holds B (its A was bought as the
//! price rose through it) and a bin above it holds A. That liquidity only earns
//! again if the price comes back, so `move_bins` relocates it next to the active
//! bin. An LP who funds a `Right`, `Left` or `Both` bin is agreeing to have
//! their liquidity moved this way; the moved reserves are only ever re-priced at
//! the destination bin's price, never swapped.
//!
//! ## The move chain
//!
//! A move merges the source bin into a bin nearer the price. Merging cannot just
//! add the reserves and the shares together, because the two bins' shares are
//! worth different amounts. Instead the moved value is converted into shares of
//! the destination at the destination's own share price:
//!
//! ```text
//! minted = moved_value * destination_supply / destination_value
//! ```
//!
//! which leaves the destination's value per share exactly where it was. The
//! source bin then records a link to where its shares went - the destination bin
//! id and the number of destination shares per source share, in Q64.64 - and
//! zeroes its reserves. A bin that was already merged into another bin can be
//! merged again, so the links form a chain, and `remove_liquidity` follows that
//! chain from the bin the LP deposited into to the bin that currently holds the
//! assets, converting the position's shares hop by hop.
//!
//! Withdrawal accounts therefore have to carry the bin arrays the chain runs
//! through as remaining accounts, one writable array per hop that is not in the
//! position's own array.
//!
//! ## Moving is permissionless
//!
//! Anyone can call `move_bins` when it is valid, so keeping bins next to the
//! price is a keeper job rather than an admin one. The guards are all on chain:
//! the source has to be out of range, the destination has to be between the
//! source and the active bin, within one array's width, and of the same kind,
//! and a bin that has already merged cannot move again. Static bins never move,
//! and a paused pool refuses moves along with deposits and swaps.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::associated_token::get_associated_token_address_with_program_id;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4");

/// PDA seeds.
const POOL_SEED: &[u8] = b"pool";
const BIN_ARRAY_SEED: &[u8] = b"bin_array";
const VAULT_A_SEED: &[u8] = b"vault_a";
const VAULT_B_SEED: &[u8] = b"vault_b";
const POSITION_SEED: &[u8] = b"position";

/// Basis-point denominator used for every fee computation.
const BPS_DENOMINATOR: u128 = 10_000;

/// Bins stored in one `BinArray` account. A bin here is bigger than a DLMM bin
/// - it carries its kind and its move link - so 50 rather than 70 keeps an array
/// under 4 KiB.
pub const BINS_PER_ARRAY: usize = 50;

/// Widest bin step the pool accepts, in basis points.
const MAX_BIN_STEP: u16 = 100;
/// Highest base fee the pool accepts, in basis points.
const MAX_BASE_FEE_BPS: u16 = 100;
/// How many bin arrays away from the active bin a new array may be created, so
/// that nobody can open an array whose prices `math_core` cannot represent.
const MAX_BIN_ARRAY_SPAN: i64 = 16;

/// Highest number of decimals a pool's mints may use. This bounds how far a bin
/// price can be scaled when it is converted into raw token units.
const MAX_MINT_DECIMALS: u8 = 9;

/// How far a single move may carry liquidity, in bins. One array's width is
/// enough to reach the active bin from anywhere in the neighbouring array, and
/// it keeps a move from being a disguised way of rebuilding a whole band.
const MAX_SHIFT_BINS: u32 = BINS_PER_ARRAY as u32;

/// Shares minted to a bin's first depositor beyond the deposit's own value
/// (L-4, same as dlmm). The lock dilutes a first-depositor share-inflation
/// attempt: the attacker would have to burn `MIN_INITIAL_SHARES` of value to
/// move the share price, which makes the economics strictly worse than doing
/// nothing. The extra shares accrue to the bin as a whole, not to any account.
const MIN_INITIAL_SHARES: u128 = 1_000;


#[program]
pub mod maverick {
    use super::*;

    /// Creates a pool together with its two vaults and the bin array that holds
    /// `active_bin_id`. Every bin in that array is priced up front, so liquidity
    /// can be added anywhere inside it right away, and every bin starts out
    /// unmoved and unkinded: the kind of a bin is set by the first LP who funds
    /// it.
    ///
    /// `array_index` is the array's place on the number line, i.e.
    /// `bin_array_index(active_bin_id)`. It is an explicit argument rather than a
    /// value derived on chain because it is part of the array's PDA seeds, and
    /// Anchor's IDL builder can only resolve a seed that is an instruction
    /// argument or an account field. It is checked against `active_bin_id` here.
    ///
    /// `authority` becomes the pool's admin - in a regulated market this should
    /// be an institutional multisig - and is the only account that can pause
    /// trading or hand the role on.
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
    pub fn initialize_pool<'info>(
        ctx: Context<'info, InitializePool<'info>>,
        array_index: i64,
        active_bin_id: i32,
        bin_step: u16,
        base_fee_bps: u16,
        authority: Pubkey,
        kyc_config: Pubkey,
        max_deviation_bps: u16,
        hooks_authority: Pubkey,
        settlement_slots: u64,
    ) -> Result<()> {
        require!(
            array_index == bin_array_index(active_bin_id),
            ErrorCode::BinNotInArray
        );
        require!(
            bin_step > 0 && bin_step <= MAX_BIN_STEP,
            ErrorCode::InvalidBinStep
        );
        require!(base_fee_bps <= MAX_BASE_FEE_BPS, ErrorCode::FeeTooHigh);
        require!(
            ctx.accounts.mint_a.decimals <= MAX_MINT_DECIMALS
                && ctx.accounts.mint_b.decimals <= MAX_MINT_DECIMALS,
            ErrorCode::UnsupportedDecimals
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

        let decimals_a = ctx.accounts.mint_a.decimals;
        let decimals_b = ctx.accounts.mint_b.decimals;

        let pool = &mut ctx.accounts.pool;
        pool.mint_a = ctx.accounts.mint_a.key();
        pool.mint_b = ctx.accounts.mint_b.key();
        pool.bin_step = bin_step;
        pool.base_fee_bps = base_fee_bps;
        pool.active_bin_id = active_bin_id;
        pool.authority = authority;
        pool.paused = false;
        pool.has_static_baseline = false;
        pool.decimals_a = decimals_a;
        pool.decimals_b = decimals_b;
        pool.kyc_config = kyc_config;
        pool.oracle_state = oracle_state_key;
        pool.hooks_authority = hooks_authority;
        pool.settlement_slots = settlement_slots;
        pool.bump = ctx.bumps.pool;
        pool.vault_a_bump = ctx.bumps.vault_a;
        pool.vault_b_bump = ctx.bumps.vault_b;

        let array = &mut ctx.accounts.bin_array;
        array.pool = pool.key();
        array.index = array_index;
        array.bins = priced_bins(array_index, bin_step, decimals_a, decimals_b)?;

        msg!(
            "Maverick pool initialized. Active bin {}, array {}, price {} (Q64.64 raw A per raw B), authority {}. Wiring: kyc {}, oracle {}, hooks {} (T+{} slots)",
            active_bin_id,
            array_index,
            array.bins[bin_offset(active_bin_id)].price_x64,
            authority,
            kyc_config,
            oracle_state_key,
            hooks_authority,
            settlement_slots
        );
        Ok(())
    }

    /// Creates one more bin array, priced the same way, so the tradable band can
    /// be widened and moved-into bins have somewhere to come from. Permissionless,
    /// but bounded to `MAX_BIN_ARRAY_SPAN` arrays away from the pool's current
    /// active bin.
    pub fn initialize_bin_array(ctx: Context<InitializeBinArray>, index: i64) -> Result<()> {
        let pool = &ctx.accounts.pool;

        let active_array = bin_array_index(pool.active_bin_id);
        let span = index
            .checked_sub(active_array)
            .and_then(i64::checked_abs)
            .ok_or(ErrorCode::MathOverflow)?;
        require!(span <= MAX_BIN_ARRAY_SPAN, ErrorCode::BinArrayOutOfRange);

        let array = &mut ctx.accounts.bin_array;
        array.pool = pool.key();
        array.index = index;
        array.bins = priced_bins(index, pool.bin_step, pool.decimals_a, pool.decimals_b)?;

        msg!(
            "Bin array {} initialized. First bin {}, price {} (Q64.64 raw A per raw B)",
            index,
            array_first_bin_id(index)?,
            array.bins[0].price_x64
        );
        Ok(())
    }

    /// Deposits both tokens into a bin and mints LP shares to the caller.
    ///
    /// Shares are proportional to the deposit's value in token A, so LPs keep
    /// the same claim no matter when they enter the bin, and the deposit's value
    /// is measured at the bin's own price - the price the bin will trade at.
    ///
    /// `kind` is the movement mode the caller is funding. A bin's kind belongs to
    /// the bin, not to the deposit, so it is fixed by the first deposit and every
    /// later deposit has to agree with it. A kind that moves is only accepted
    /// once the pool has static liquidity to anchor its price, and a bin that has
    /// already been merged away cannot take deposits at all - its shares now live
    /// in the bin it moved into.
    ///
    /// `array_index` is the bin's array, `bin_array_index(bin_id)`; it is passed
    /// explicitly because it is part of the array's PDA seeds and checked here.
    pub fn add_liquidity<'info>(
        ctx: Context<'info, AddLiquidity<'info>>,
        array_index: i64,
        bin_id: i32,
        kind: BinKind,
        amount_a: u64,
        amount_b: u64,
    ) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        let bin_array = &mut ctx.accounts.bin_array;
        let position = &mut ctx.accounts.position;

        require!(!pool.paused, ErrorCode::PoolPaused);
        require!(
            array_index == bin_array_index(bin_id),
            ErrorCode::BinNotInArray
        );
        require_keys_eq!(bin_array.pool, pool.key(), ErrorCode::WrongBinArrayPool);
        require!(array_is_sized(&bin_array.bins), ErrorCode::InvalidBinArray);
        require!(amount_a > 0 || amount_b > 0, ErrorCode::ZeroAmount);
        if kind != BinKind::Static {
            require!(pool.has_static_baseline, ErrorCode::NoStaticBaseline);
        }

        let bin = &mut bin_array.bins[bin_offset(bin_id)];
        require!(bin.price_x64 > 0, ErrorCode::BinNotInitialized);
        require!(!bin.is_moved(), ErrorCode::BinAlreadyMoved);
        if bin.supply > 0 {
            require!(bin.kind == kind, ErrorCode::BinKindMismatch);
        } else {
            bin.kind = kind;
        }

        // 1. Move both legs into the pool-owned vaults and measure what the
        //    vaults actually received: a Token-2022 transfer-fee extension can
        //    skim part of the requested amount.
        let user = ctx.accounts.user.to_account_info();
        let hooks = ctx.remaining_accounts;

        let deposited_a = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.user_token_a,
            &ctx.accounts.mint_a,
            &mut ctx.accounts.vault_a,
            &user,
            hooks,
            &[],
            amount_a,
        )?;
        let deposited_b = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.user_token_b,
            &ctx.accounts.mint_b,
            &mut ctx.accounts.vault_b,
            &user,
            hooks,
            &[],
            amount_b,
        )?;

        // 2. Mint shares against the deposit's value in token A. The first
        //    deposit defines the share unit; later deposits are scaled by the
        //    existing supply so the ratio of shares to value never moves.
        let deposit_value = value_in_a(deposited_a, deposited_b, bin.price_x64)?;
        let shares_to_mint = if bin.supply == 0 {
            // First deposit: the deposit's value plus the L-4 minimum-share
            // lock, so the attacker in that finding pays for the inflation
            // attempt instead of the next depositor.
            deposit_value
                .checked_add(MIN_INITIAL_SHARES)
                .ok_or(ErrorCode::MathOverflow)?
        } else {
            let reserve_value = value_in_a(bin.reserve_a, bin.reserve_b, bin.price_x64)?;
            require!(reserve_value > 0, ErrorCode::InsufficientLiquidity);
            deposit_value
                .checked_mul(bin.supply)
                .ok_or(ErrorCode::MathOverflow)?
                .checked_div(reserve_value)
                .ok_or(ErrorCode::MathOverflow)?
        };
        require!(shares_to_mint > 0, ErrorCode::ZeroAmount);

        // 3. Update bin state and the caller's position.
        bin.reserve_a = bin
            .reserve_a
            .checked_add(deposited_a)
            .ok_or(ErrorCode::MathOverflow)?;
        bin.reserve_b = bin
            .reserve_b
            .checked_add(deposited_b)
            .ok_or(ErrorCode::MathOverflow)?;
        bin.supply = bin
            .supply
            .checked_add(shares_to_mint)
            .ok_or(ErrorCode::MathOverflow)?;

        position.pool = pool.key();
        position.owner = ctx.accounts.user.key();
        position.bin_id = bin_id;
        position.liquidity_shares = position
            .liquidity_shares
            .checked_add(shares_to_mint)
            .ok_or(ErrorCode::MathOverflow)?;

        if kind == BinKind::Static {
            pool.has_static_baseline = true;
        }

        msg!(
            "Added {:?} liquidity to bin {}. Shares minted: {}",
            kind,
            bin_id,
            shares_to_mint
        );
        Ok(())
    }

    /// Burns LP shares and pays out the slice of the bin's reserves they own.
    ///
    /// The shares an LP holds are denominated in the bin they deposited into.
    /// That bin may since have been merged into another one, and that one into
    /// another, so the claim is followed along the move chain to the bin that
    /// actually holds the assets, converting the shares at each hop. A position
    /// that has never been moved is simply paid out of its own bin, and a
    /// withdrawal of the whole position is converted without rounding so it can
    /// always empty its claim exactly.
    ///
    /// Remaining accounts, in order:
    /// 1. The bin arrays the move chain runs through, writable, one for each hop
    ///    whose bin lives outside the position's own array. They are matched by
    ///    their canonical PDA, so order among them does not matter.
    /// 2. The transfer-hook accounts, if a mint has a hook (anything this program
    ///    does not own).
    ///
    /// Deliberately not paused-gated: a paused pool still lets LPs out.
    pub fn remove_liquidity<'info>(
        ctx: Context<'info, RemoveLiquidity<'info>>,
        array_index: i64,
        bin_id: i32,
        shares: u128,
        min_amount_a: u64,
        min_amount_b: u64,
    ) -> Result<()> {
        let program_id = ctx.program_id;
        let remaining = ctx.remaining_accounts;
        let pool_key = ctx.accounts.pool.key();

        require!(
            array_index == bin_array_index(bin_id),
            ErrorCode::BinNotInArray
        );
        require_keys_eq!(
            ctx.accounts.bin_array.pool,
            pool_key,
            ErrorCode::WrongBinArrayPool
        );
        require!(
            array_is_sized(&ctx.accounts.bin_array.bins),
            ErrorCode::InvalidBinArray
        );
        require!(
            ctx.accounts.position.owner == ctx.accounts.user.key(),
            ErrorCode::Unauthorized
        );
        require!(
            ctx.accounts.position.bin_id == bin_id,
            ErrorCode::BinNotInArray
        );

        let position_shares = ctx.accounts.position.liquidity_shares;
        require!(
            shares > 0 && shares <= position_shares,
            ErrorCode::InsufficientShares
        );

        // 1. Follow the move chain to the bin that holds the assets, and convert
        //    the whole position's claim to that bin's shares.
        let (terminal_bin_id, terminal_shares) = resolve_position(position_shares, bin_id, |id| {
            load_bin(
                program_id,
                &pool_key,
                array_index,
                &ctx.accounts.bin_array.bins,
                id,
                remaining,
            )
        })?;

        // 2. The slice of that claim being burned. Converting the whole claim
        //    first means the parts of a position always add back up to the whole.
        let burn = claimed_shares(terminal_shares, shares, position_shares)?;
        require!(burn > 0, ErrorCode::ZeroAmount);

        ctx.accounts.position.liquidity_shares = position_shares
            .checked_sub(shares)
            .ok_or(ErrorCode::MathOverflow)?;

        // 3. Settle in the terminal bin, wherever it lives, and pay the user out
        //    of the pool-owned vaults.
        let terminal_index = bin_array_index(terminal_bin_id);
        let offset = bin_offset(terminal_bin_id);

        let (amount_a, amount_b) = if terminal_index == array_index {
            settle_withdrawal(
                &mut ctx.accounts.bin_array.bins[offset],
                burn,
                min_amount_a,
                min_amount_b,
            )?
        } else {
            // The terminal bin lives in another array, so settle in place: this
            // path is reached while the caller's own array is already
            // materialized, and a second `Vec<Bin>` does not fit in the heap.
            let account = hop_array_info(program_id, &pool_key, terminal_index, remaining)?;
            let mut data = account.try_borrow_mut_data()?;
            let at = bin_byte_offset(&data, offset)?;
            let mut bin = read_bin_at(&data, at)?;
            let paid = settle_withdrawal(&mut bin, burn, min_amount_a, min_amount_b)?;
            write_bin_at(&mut data, at, &bin)?;
            paid
        };

        let mint_a_key = ctx.accounts.pool.mint_a;
        let mint_b_key = ctx.accounts.pool.mint_b;
        let bump = [ctx.accounts.pool.bump];
        let pool_seeds: &[&[u8]] = &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];
        let pool_info = ctx.accounts.pool.to_account_info();
        let hooks = hook_accounts(program_id, remaining);

        if amount_a > 0 {
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.vault_a,
                &ctx.accounts.mint_a,
                &ctx.accounts.user_token_a.to_account_info(),
                &pool_info,
                &hooks,
                &[pool_seeds],
                amount_a,
            )?;
        }
        if amount_b > 0 {
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.vault_b,
                &ctx.accounts.mint_b,
                &ctx.accounts.user_token_b.to_account_info(),
                &pool_info,
                &hooks,
                &[pool_seeds],
                amount_b,
            )?;
        }

        msg!(
            "Removed liquidity from bin {}, settled in bin {}. Shares burned: {}",
            bin_id,
            terminal_bin_id,
            shares
        );
        // Note: an emptied position account stays alive; `add_liquidity` reuses
        // it via `init_if_needed`. Closing it here would race with Anchor's
        // account exit, which re-serializes the position after the handler runs.
        Ok(())
    }

    /// The core Maverick engine: moves an out-of-range bin's liquidity to a bin
    /// nearer the active price, so it can earn again instead of sitting idle.
    ///
    /// Anyone can call this (incentivized off-chain), because every guard that
    /// matters is checked here:
    ///
    /// - the source bin has to hold liquidity and must not already have moved;
    /// - its kind has to allow the direction - static bins never move, right
    ///   bins only move right, left bins only left, both bins either way;
    /// - it has to be out of range, i.e. below the active bin for a move right
    ///   and above it for a move left;
    /// - the destination has to be between the source and the active bin, within
    ///   `MAX_SHIFT_BINS`, and either be empty or be of the same kind;
    /// - the pool must not be paused.
    ///
    /// The move re-prices the source's reserves at the destination's price, mints
    /// the destination shares they are worth, and records the hop so the moved
    /// bin's LPs can withdraw from the destination (see `remove_liquidity`). No
    /// tokens move: a shift is a change of bin, not a trade, so it cannot be used
    /// to take value out of the pool.
    ///
    /// Remaining accounts: the destination bin's array, if the destination is not
    /// in the source bin's array. It is matched by its canonical PDA.
    pub fn move_bins(
        ctx: Context<MoveBins>,
        from_array_index: i64,
        from_bin_id: i32,
        to_bin_id: i32,
    ) -> Result<()> {
        let program_id = ctx.program_id;
        let pool = &ctx.accounts.pool;
        require!(!pool.paused, ErrorCode::PoolPaused);

        let pool_key = pool.key();
        let active_bin_id = pool.active_bin_id;
        let from_array = &mut ctx.accounts.from_array;
        require_keys_eq!(from_array.pool, pool_key, ErrorCode::WrongBinArrayPool);
        require!(array_is_sized(&from_array.bins), ErrorCode::InvalidBinArray);
        require!(
            from_array_index == bin_array_index(from_bin_id),
            ErrorCode::BinNotInArray
        );

        let source_offset = bin_offset(from_bin_id);
        let to_index = bin_array_index(to_bin_id);
        let to_offset = bin_offset(to_bin_id);

        // Read the bin being moved and judge it before the destination is
        // located, so the answer for a bin that could never move does not depend
        // on whether the caller passed the destination's array.
        let mut source = from_array.bins[source_offset];
        check_move_source(&source)?;

        if to_index == from_array_index {
            // Source and destination share an array, so one account serves both.
            let mut dest = from_array.bins[to_offset];
            check_move(active_bin_id, &source, &dest, from_bin_id, to_bin_id)?;
            merge_bins(&mut source, &mut dest, to_bin_id)?;
            from_array.bins[source_offset] = source;
            from_array.bins[to_offset] = dest;
        } else {
            // The destination is touched in place rather than deserialized into
            // a second `Account<BinArray>`. The source array is already
            // materialized by the account context, and a `Vec<Bin>` per array
            // does not fit in the 32 KiB heap twice over - the instruction used
            // to die here with `memory allocation failed, out of memory`. The
            // copy that comes out of a move is fixed size: every bin is
            // `Bin::LEN` bytes, so writing the destination back cannot shift the
            // rest of the array.
            let account = hop_array_info(program_id, &pool_key, to_index, ctx.remaining_accounts)?;
            let mut data = account.try_borrow_mut_data()?;
            let to_at = bin_byte_offset(&data, to_offset)?;
            let mut dest = read_bin_at(&data, to_at)?;

            check_move(active_bin_id, &source, &dest, from_bin_id, to_bin_id)?;
            merge_bins(&mut source, &mut dest, to_bin_id)?;

            from_array.bins[source_offset] = source;
            write_bin_at(&mut data, to_at, &dest)?;
        }

        msg!(
            "Moved bin {} ({:?}) into bin {}. Active bin: {}",
            from_bin_id,
            ctx.accounts.from_array.bins[source_offset].kind,
            to_bin_id,
            active_bin_id
        );
        Ok(())
    }

    /// Swaps through the bin arrays, starting at the pool's active bin.
    ///
    /// The input is measured on arrival, the fee is taken from what arrived and
    /// left in the bin the trade started in, and the remainder is filled bin by
    /// bin, crossing to the next bin whenever one runs dry. The active bin moves
    /// with the trade, so the next swap starts where this one finished and every
    /// bin the price has passed through is now out of range and free to be moved.
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
    ///
    /// Remaining accounts: every `BinArray` the trade may walk, in the order it
    /// will walk them (ascending index when selling A, descending when selling
    /// B), followed by any accounts the Token-2022 transfer hook needs. Bin
    /// arrays are told apart from hook accounts by owner, so hook accounts must
    /// not be owned by this program.
    pub fn swap<'info>(
        ctx: Context<'info, Swap<'info>>,
        amount_in: u64,
        min_amount_out: u64,
        is_a_to_b: bool,
        trade_id: u64,
    ) -> Result<()> {
        require!(amount_in > 0, ErrorCode::ZeroAmount);
        require!(!ctx.accounts.pool.paused, ErrorCode::PoolPaused);

        // 0. Pool-level rails: the accounts the policy demands must be present
        //    and canonical, and the ones it does not demand must be this
        //    program's own ID sentinel (so a caller cannot smuggle an unused
        //    rail's accounts past the policy).
        let pool_key = ctx.accounts.pool.key();
        let settlement_on = ctx.accounts.pool.hooks_authority != Pubkey::default();
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
                &[b"oracle", pool_key.as_ref()],
                &oracle::ID,
            );
            require_keys_eq!(
                oracle_state.key(),
                expected,
                ErrorCode::WrongOracleState
            );
        } else {
            require!(
                ctx.accounts.oracle_state.is_none(),
                ErrorCode::RailAccountUnexpected
            );
        }
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
            require!(trade_id != 0, ErrorCode::InvalidTradeId);
        } else {
            require!(trade_id == 0, ErrorCode::InvalidTradeId);
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

        // 0a. KYC rail: the trader must hold a valid attestation before any
        //     math runs. `verify_kyc` is read-only on the gate side and
        //     requires the trader to be a signer — which they are, since they
        //     signed this swap.
        if ctx.accounts.pool.kyc_config != Pubkey::default() {
            let cpi_accounts = kyc_gate::cpi::accounts::VerifyKyc {
                kyc_config: ctx
                    .accounts
                    .kyc_config
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
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
        //     start price is the active bin's price - what the walk will
        //     begin from. A price far from the 30-minute TWAP reverts the
        //     whole transaction here. Bin prices are deterministic (an array
        //     prices every bin from `bin_step` and the decimals at creation,
        //     and only reserves move afterwards), so the price comes from
        //     pure math instead of an account read.
        if ctx.accounts.pool.oracle_state != Pubkey::default() {
            let start_price = bin_price_raw(
                ctx.accounts.pool.active_bin_id,
                ctx.accounts.pool.bin_step,
                ctx.accounts.pool.decimals_a,
                ctx.accounts.pool.decimals_b,
            )?;
            // The oracle binds the check to the pool that owns it, so the pool
            // signs the CPI with its own seeds. The price above is priced from
            // pool state, so even a permissionless swap cannot forge it.
            let (mint_a_key, mint_b_key, pool_bump) = (
                ctx.accounts.pool.mint_a,
                ctx.accounts.pool.mint_b,
                ctx.accounts.pool.bump,
            );
            let bump = [pool_bump];
            let pool_seeds: &[&[u8]] =
                &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];
            let cpi_accounts = oracle::cpi::accounts::CheckDeviation {
                oracle_state: ctx
                    .accounts
                    .oracle_state
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
                pool: ctx.accounts.pool.to_account_info(),
            };
            let signer_seeds: &[&[&[u8]]] = &[pool_seeds];
            let cpi_ctx = CpiContext::new(oracle::ID, cpi_accounts).with_signer(signer_seeds);
            oracle::cpi::check_deviation(cpi_ctx, start_price)?;
        }

        let program_id = ctx.program_id;
        let remaining_accounts = ctx.remaining_accounts;
        let hooks = hook_accounts(program_id, remaining_accounts);
        require!(
            remaining_accounts.iter().any(|a| a.owner == program_id),
            ErrorCode::NoBinArrays
        );

        let pool_bump = ctx.accounts.pool.bump;
        let pool_info = ctx.accounts.pool.to_account_info();
        let mint_a_key = ctx.accounts.pool.mint_a;
        let mint_b_key = ctx.accounts.pool.mint_b;

        // 1. Take the input token and measure what the vault received.
        let user = ctx.accounts.user.to_account_info();
        let amount_in_received = if is_a_to_b {
            transfer_and_measure(
                &ctx.accounts.token_program,
                &ctx.accounts.user_token_a,
                &ctx.accounts.mint_a,
                &mut ctx.accounts.vault_a,
                &user,
                &hooks,
                &[],
                amount_in,
            )?
        } else {
            transfer_and_measure(
                &ctx.accounts.token_program,
                &ctx.accounts.user_token_b,
                &ctx.accounts.mint_b,
                &mut ctx.accounts.vault_b,
                &user,
                &hooks,
                &[],
                amount_in,
            )?
        };

        // 2. Walk the bins. Selling A moves up the bins (B gets scarcer), selling
        //    B moves down them.
        let dir: i32 = if is_a_to_b { 1 } else { -1 };
        let mut out_total: u128 = 0;

        {
            let pool = &mut ctx.accounts.pool;
            let mut cursor = pool.active_bin_id;
            let mut active_bin_id = pool.active_bin_id;
            let mut remaining: u64 = 0;
            let mut started = false;

            for account in remaining_accounts.iter() {
                if account.owner != program_id {
                    // A transfer-hook account, not a bin array: the walk ignores it.
                    continue;
                }
                if started && remaining == 0 {
                    break;
                }
                require!(account.is_writable, ErrorCode::BinArrayNotWritable);

                let mut array = Account::<BinArray>::try_from(account)?;
                require_keys_eq!(array.pool, pool_key, ErrorCode::WrongBinArrayPool);
                require!(array_is_sized(&array.bins), ErrorCode::InvalidBinArray);

                // The arrays must arrive in the order the trade walks them, and
                // each one must be the canonical bin array for its index.
                let expected_index = bin_array_index(cursor);
                require!(
                    array.index == expected_index,
                    ErrorCode::BinArrayIndexMismatch
                );
                let (expected_key, _) = bin_array_pda(&pool_key, expected_index, program_id);
                require_keys_eq!(expected_key, account.key(), ErrorCode::InvalidBinArray);

                if !started {
                    // 2a. Fee on what actually arrived, credited to the starting
                    //     bin so it accrues to that bin's LPs.
                    let bin = &mut array.bins[bin_offset(cursor)];
                    require!(bin.price_x64 > 0, ErrorCode::BinNotInitialized);

                    let fee = u64::try_from(
                        (amount_in_received as u128)
                            .checked_mul(compute_dynamic_fee(pool, bin)?)
                            .ok_or(ErrorCode::MathOverflow)?
                            .checked_div(BPS_DENOMINATOR)
                            .ok_or(ErrorCode::MathOverflow)?,
                    )
                    .map_err(|_| error!(ErrorCode::MathOverflow))?;

                    remaining = amount_in_received
                        .checked_sub(fee)
                        .ok_or(ErrorCode::MathOverflow)?;
                    require!(remaining > 0, ErrorCode::ZeroAmount);

                    if is_a_to_b {
                        bin.reserve_a = bin
                            .reserve_a
                            .checked_add(fee)
                            .ok_or(ErrorCode::MathOverflow)?;
                    } else {
                        bin.reserve_b = bin
                            .reserve_b
                            .checked_add(fee)
                            .ok_or(ErrorCode::MathOverflow)?;
                    }
                    started = true;
                }

                // 2b. Fill what this array can, starting at the cursor.
                let mut cursor_offset = bin_offset(cursor) as i32;
                fill_bins(
                    &mut array.bins,
                    &mut cursor_offset,
                    &mut active_bin_id,
                    dir,
                    &mut remaining,
                    &mut out_total,
                )?;
                array.exit(program_id)?;

                if remaining == 0 {
                    break;
                }
                // The cursor ran off the edge of this array: the next account has
                // to cover the next bin.
                cursor = array_first_bin_id(array.index)?
                    .checked_add(cursor_offset)
                    .ok_or(ErrorCode::MathOverflow)?;
            }

            require!(remaining == 0, ErrorCode::InsufficientLiquidity);
            pool.active_bin_id = active_bin_id;
        }

        // 3. Pay the output out of the opposite vault, signed by the pool PDA.
        //    The destination is the trader's wallet — or, when settlement
        //    routing is on, the hook authority's custody ATA, which the same
        //    transaction immediately sweeps into the escrow. The trader never
        //    holds the output; `min_amount_out` is checked against what the
        //    vault actually paid either way, before it leaves the pool.
        let out = u64::try_from(out_total).map_err(|_| error!(ErrorCode::MathOverflow))?;
        require!(out > 0, ErrorCode::ZeroAmount);

        let bump = [pool_bump];
        let pool_seeds: &[&[u8]] = &[POOL_SEED, mint_a_key.as_ref(), mint_b_key.as_ref(), &bump];

        let (out_vault, out_mint) = if is_a_to_b {
            (&ctx.accounts.vault_b, &ctx.accounts.mint_b)
        } else {
            (&ctx.accounts.vault_a, &ctx.accounts.mint_a)
        };

        // When settlement is on the output goes to the hook authority's
        // custody ATA, passed unchecked; the custody balance is measured by
        // the settlement sweep instead, so the payout helper is not used for
        // that leg. When it is off the payout is measured on the trader's
        // token account exactly like the input leg.
        let received_out = if settlement_on {
            transfer_checked(
                &ctx.accounts.token_program,
                out_vault,
                out_mint,
                ctx.accounts
                    .custody_token_account
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,

                &pool_info,
                &hooks,
                &[pool_seeds],
                out,
            )?;
            out
        } else if is_a_to_b {
            transfer_and_measure(
                &ctx.accounts.token_program,
                out_vault,
                out_mint,
                &mut ctx.accounts.user_token_b,
                &pool_info,
                &hooks,
                &[pool_seeds],
                out,
            )?
        } else {
            transfer_and_measure(
                &ctx.accounts.token_program,
                out_vault,
                out_mint,
                &mut ctx.accounts.user_token_a,
                &pool_info,
                &hooks,
                &[pool_seeds],
                out,
            )?
        };
        require!(received_out >= min_amount_out, ErrorCode::SlippageExceeded);

        // 4. Oracle rail: record the settled price so the TWAP stays current
        //    for the next trade's circuit-breaker check. `record_observation`
        //    requires the pool PDA as a signer; this CPI supplies it via the
        //    pool's own seeds, inside the transaction the pool is executing.
        //    The recorded price is the active bin's (post-trade) raw-unit
        //    price — the same quantity the pre-trade check compared.
        let oracle_on = ctx.accounts.pool.oracle_state != Pubkey::default();
        if oracle_on {
            let settled_price = bin_price_raw(
                ctx.accounts.pool.active_bin_id,
                ctx.accounts.pool.bin_step,
                ctx.accounts.pool.decimals_a,
                ctx.accounts.pool.decimals_b,
            )?;
            let cpi_accounts = oracle::cpi::accounts::RecordObservation {
                oracle_state: ctx
                    .accounts
                    .oracle_state
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
                authority: ctx.accounts.pool.to_account_info(),
            };
            let signer_seeds: &[&[&[u8]]] = &[&[
                POOL_SEED,
                mint_a_key.as_ref(),
                mint_b_key.as_ref(),
                &bump,
            ]];
            let cpi_ctx = CpiContext::new(oracle::ID, cpi_accounts).with_signer(signer_seeds);
            oracle::cpi::record_observation(cpi_ctx, settled_price)?;
        }

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
            // The custody account must be the hook authority's ATA for this
            // trade's output mint. Pinned by derivation rather than by
            // ownership alone: a PDA can own any number of token accounts and
            // the token program lets anyone create one owned by a PDA, so
            // ownership by itself would let a caller choose where the output
            // goes.
            require_keys_eq!(
                ctx
                    .accounts
                    .custody_token_account
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .key(),
                get_associated_token_address_with_program_id(
                    &ctx
                        .accounts
                        .hooks_authority
                        .as_ref()
                        .ok_or(ErrorCode::RailAccountUnexpected)?
                        .key(),
                    &output_mint_account.key(),
                    &ctx.accounts.token_program.key(),
                ),
                ErrorCode::InvalidCustodyAccount
            );

            let cpi_accounts = hooks::cpi::accounts::AfterSwap {
                hook_authority: ctx
                    .accounts
                    .hooks_authority
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
                taker: ctx.accounts.user.to_account_info(),
                amm_vault: ctx
                    .accounts
                    .custody_token_account
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
                token_mint: output_mint_account,
                settlement_vault: ctx
                    .accounts
                    .settlement_vault
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
                settlement_vault_token: ctx
                    .accounts
                    .settlement_vault_token
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .to_account_info(),
                settlement_config: ctx.accounts.settlement_config.to_account_info(),
                settlement_program: ctx.accounts.settlement_program.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            };
            // The CPI callee is the hooks program; the settlement program
            // account in the list is the hooks program's own CPI target. The
            // hook accounts go along too: the escrow leg is a second transfer
            // of the same (possibly hook-bearing) mint, and Token-2022 has to
            // resolve the mint's meta list against that instruction as well.
            // For a mint without a hook the slice is empty and ignored.
            let cpi_ctx = CpiContext::new(hooks::ID, cpi_accounts)
                .with_remaining_accounts(hooks.clone());
            hooks::cpi::after_swap(
                cpi_ctx,
                trade_id,
                ctx.accounts.pool.settlement_slots,
                out,
            )?;
        }

        msg!(
            "Swap executed. In: {}, Out: {}, active bin: {}",
            amount_in,
            received_out,
            ctx.accounts.pool.active_bin_id
        );
        Ok(())
    }

    /// Pauses or resumes the pool. While paused, swaps, deposits and moves are
    /// refused; `remove_liquidity` deliberately still works, so LPs are never
    /// trapped in a paused pool. Add a harder freeze - one that also blocks
    /// withdrawals - if a jurisdiction ever needs it.
    pub fn set_paused(ctx: Context<Admin>, paused: bool) -> Result<()> {
        let pool = &mut ctx.accounts.pool;
        pool.paused = paused;

        msg!("Pool {} paused: {}", pool.key(), paused);
        Ok(())
    }

    /// Hands the pool's admin role to `new_authority`, so the pause key can be
    /// rotated onto a new multisig without redeploying the pool.
    pub fn set_authority(ctx: Context<Admin>, new_authority: Pubkey) -> Result<()> {
        // The default pubkey can never satisfy `has_one = authority`, so rotating
        // onto it would leave the pool with no authority at all: unpausable and
        // unrotatable. kyc_gate and transfer_hook refuse it for the same reason.
        require_keys_neq!(
            new_authority,
            Pubkey::default(),
            ErrorCode::InvalidAuthority
        );

        let pool = &mut ctx.accounts.pool;
        pool.authority = new_authority;

        msg!("Pool {} authority: {}", pool.key(), new_authority);
        Ok(())
    }

    /// Permissionless circuit-breaker latch: pause the pool when its active
    /// bin's price deviates from its 30-minute oracle TWAP by more than the
    /// pool's threshold. Anyone may pay for it; the price is read from pool
    /// state and the oracle's `trip_breaker` requires the pool PDA's
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

        let price = bin_price_raw(
            pool.active_bin_id,
            pool.bin_step,
            pool.decimals_a,
            pool.decimals_b,
        )?;
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
// BIN INDEXING
//
// A bin array owns a contiguous run of `BINS_PER_ARRAY` bin ids, so bin ids
// below zero belong to array -1 and up, exactly like Meteora's DLMM. Both
// helpers use Euclidean division, which is what keeps bin -1 at offset
// BINS_PER_ARRAY - 1 of array -1 rather than in array 0.
// -----------------------------------------------------------------------

/// Array that holds `bin_id`.
fn bin_array_index(bin_id: i32) -> i64 {
    (bin_id as i64).div_euclid(BINS_PER_ARRAY as i64)
}

/// Position of `bin_id` inside its array.
fn bin_offset(bin_id: i32) -> usize {
    (bin_id as i64).rem_euclid(BINS_PER_ARRAY as i64) as usize
}

/// Lowest bin id of an array.
fn array_first_bin_id(array_index: i64) -> Result<i32> {
    let first = array_index
        .checked_mul(BINS_PER_ARRAY as i64)
        .ok_or(ErrorCode::MathOverflow)?;
    i32::try_from(first).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Canonical address of a pool's bin array.
fn bin_array_pda(pool: &Pubkey, index: i64, program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[BIN_ARRAY_SEED, pool.as_ref(), &index.to_le_bytes()],
        program_id,
    )
}

/// Whether a bin array holds the expected number of bins. Guards the indexing
/// below against a corrupted or hand-made account.
fn array_is_sized(bins: &[Bin]) -> bool {
    bins.len() == BINS_PER_ARRAY
}

// -----------------------------------------------------------------------
// IN-PLACE BIN ACCESS
//
// A handful of paths need one bin out of a bin array the caller passed in a
// remaining account. Deserialising the whole `Account<BinArray>` for that would
// allocate a 50-element `Vec<Bin>`, and the SBF heap is a 32 KiB arena whose
// allocator never reclaims (see `solana_program_entrypoint::BumpAllocator`), so
// allocations are charged for the rest of the instruction however briefly they
// are live. `move_bins` needs a *second* array on top of the one in its own
// account context, and that second `Vec` is what pushed it over the limit.
//
// These helpers read and write a single bin directly in the account's bytes, so
// such a path allocates nothing at all.
// -----------------------------------------------------------------------

/// How many bins a serialized array holds.
fn array_bin_count(data: &[u8]) -> Result<usize> {
    require!(
        data.len() >= BinArray::BINS_OFFSET,
        ErrorCode::InvalidBinArray
    );
    let count = u32::from_le_bytes(
        data[BinArray::COUNT_OFFSET..BinArray::BINS_OFFSET]
            .try_into()
            .map_err(|_| error!(ErrorCode::InvalidBinArray))?,
    );
    Ok(count as usize)
}

/// Byte offset of bin `index` inside a serialized array.
///
/// Every bin is `Bin::LEN` bytes, moved or not (L-8), so the offset is a plain
/// multiplication: nothing that happens to an earlier bin can shift it.
fn bin_byte_offset(data: &[u8], index: usize) -> Result<usize> {
    require!(index < array_bin_count(data)?, ErrorCode::InvalidBinArray);
    let at = BinArray::BINS_OFFSET + index * Bin::LEN;
    require!(at + Bin::LEN <= data.len(), ErrorCode::InvalidBinArray);
    Ok(at)
}

/// One bin, read straight out of a serialized array.
fn read_bin_at(data: &[u8], at: usize) -> Result<Bin> {
    require!(at + Bin::LEN <= data.len(), ErrorCode::InvalidBinArray);
    Bin::try_from_slice(&data[at..at + Bin::LEN]).map_err(|_| error!(ErrorCode::InvalidBinArray))
}

/// Writes `bin` back over the element at `at`.
///
/// The element is `Bin::LEN` bytes either way, so the write is fixed-size:
/// nothing after it moves, whether or not the bin being written is recording a
/// move.
fn write_bin_at(data: &mut [u8], at: usize, bin: &Bin) -> Result<()> {
    require!(at + Bin::LEN <= data.len(), ErrorCode::InvalidBinArray);

    let mut writer: &mut [u8] = &mut data[at..at + Bin::LEN];
    bin.serialize(&mut writer)
        .map_err(|_| error!(ErrorCode::InvalidBinArray))?;
    require!(writer.is_empty(), ErrorCode::InvalidBinArray);
    Ok(())
}

/// Prices every bin of an array, in raw token units.
///
/// The array's first bin is priced exactly, and the rest are stepped from it by
/// the bin's own price ratio. That costs one multiplication per bin instead of a
/// full tick conversion, which matters because these run inside an instruction.
///
/// Every bin is created unmoved and without a kind: a kind belongs to the
/// liquidity in a bin, so it is set by the first LP to fund it.
fn priced_bins(
    array_index: i64,
    bin_step: u16,
    decimals_a: u8,
    decimals_b: u8,
) -> Result<Vec<Bin>> {
    let first_bin_id = array_first_bin_id(array_index)?;
    // Bin 1 spans exactly one bin step above parity, so it is the step ratio.
    let step_price = calculate_bin_price(1, bin_step)?;

    let first_price = calculate_bin_price(first_bin_id, bin_step)?;
    let mut price_x64 = price_in_raw_units(first_price, decimals_a, decimals_b)?;
    let mut bins = Vec::with_capacity(BINS_PER_ARRAY);
    for offset in 0..BINS_PER_ARRAY {
        if offset > 0 {
            price_x64 = math_core::mul_shr(price_x64, step_price)
                .map_err(|_| error!(ErrorCode::MathOverflow))?;
        }
        bins.push(Bin {
            price_x64,
            reserve_a: 0,
            reserve_b: 0,
            supply: 0,
            kind: BinKind::default(),
            move_state: MoveState::default(),
        });
    }
    Ok(bins)
}

// -----------------------------------------------------------------------
// INTERNAL FUNCTIONS
//
// These are plain free functions: the `#[program]` macro treats every function
// declared inside the program module as an instruction handler (or, with three
// named arguments, as the fallback handler), so helpers cannot live in there.
// -----------------------------------------------------------------------

/// Q64.64 price of a bin in whole tokens, i.e. whole token A per whole token B
/// at that bin: `1.0001 ^ (bin_id * bin_step)`.
///
/// `math_core` ticks are one basis point wide (`1.0001^tick`), so a bin of
/// `bin_step` basis points spans `bin_step` ticks. Note that this compounds
/// `bin_step` times rather than multiplying a linear `bin_step / 10_000` step, so
/// a 100 bps bin is 1.010049 above parity, not 1.01.
///
/// This is the price a human would quote. Bins store `price_in_raw_units` of it,
/// which is the form the rest of the program can consume directly.
fn calculate_bin_price(bin_id: i32, bin_step: u16) -> Result<u128> {
    let tick = bin_id
        .checked_mul(bin_step as i32)
        .ok_or(ErrorCode::MathOverflow)?;
    let sqrt_price =
        math_core::sqrt_price_at_tick(tick).map_err(|_| error!(ErrorCode::MathOverflow))?;
    math_core::price_from_sqrt_price(sqrt_price).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Converts a whole-token price into the raw-unit price bins store, by
/// `10 ^ (decimals_a - decimals_b)`.
///
/// A whole token of A is `10^decimals_a` raw units and a whole token of B is
/// `10^decimals_b`, so the price of one raw unit of B in raw units of A is the
/// whole-token price scaled by that factor. A 6-decimal stablecoin quoted
/// against a 9-decimal token needs a price a thousand times smaller, because one
/// raw unit of the 9-decimal token is a thousandth of a raw unit of the
/// stablecoin. Skipping this is what would make the two sides of a bin worth
/// wildly different amounts.
///
/// Scaling up is exact; scaling down can only be represented to the nearest
/// Q64.64 value and rounds down, which is under one part in 10^19.
fn price_in_raw_units(price_x64: u128, decimals_a: u8, decimals_b: u8) -> Result<u128> {
    require!(
        decimals_a <= MAX_MINT_DECIMALS && decimals_b <= MAX_MINT_DECIMALS,
        ErrorCode::UnsupportedDecimals
    );

    let diff = decimals_a.abs_diff(decimals_b);
    if diff == 0 {
        return Ok(price_x64);
    }
    let factor = 10u64
        .checked_pow(diff as u32)
        .ok_or(ErrorCode::UnsupportedDecimals)?;
    let factor = math_core::from_u64(factor);

    let scaled = if decimals_a > decimals_b {
        math_core::mul_shr(price_x64, factor)
    } else {
        math_core::div_shr(price_x64, factor)
    };
    scaled.map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Value of `(amount_a, amount_b)` in token A, in Q64.64.
///
/// Both amounts are raw token units and `price_x64` is the bin's raw-unit price,
/// so the two legs are directly comparable no matter what decimals the mints
/// use. The result is in raw A units, which is what shares are denominated in.
///
/// Which bin's price to use is not a detail: measuring a moved bin's reserves at
/// the destination's price is what makes a swap-free move value-neutral.
///
/// Single-sided pricing (L-6, accepted): shares are denominated in token A
/// only, so for a bin composed almost entirely of token B all of the deposit
/// and withdrawal rounding error lands on the A leg. The `round_up` (user
/// pays) / `round_down` (pool pays) split at the call sites means the error
/// always favours the pool, never the user, so this is awareness-only — a
/// two-sided pricing (A and B claims separately) would double the position
/// state for a rounding benefit measured in single units.
fn value_in_a(amount_a: u64, amount_b: u64, price_x64: u128) -> Result<u128> {
    let a = (amount_a as u128)
        .checked_mul(math_core::Q64)
        .ok_or(ErrorCode::MathOverflow)?;
    let b = (amount_b as u128)
        .checked_mul(price_x64)
        .ok_or(ErrorCode::MathOverflow)?;
    a.checked_add(b)
        .ok_or_else(|| error!(ErrorCode::MathOverflow))
}

/// `reserve * shares / supply`, rounded down and range-checked against `u64`.
fn share_of(reserve: u64, shares: u128, supply: u128) -> Result<u64> {
    let share = (reserve as u128)
        .checked_mul(shares)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(supply)
        .ok_or(ErrorCode::MathOverflow)?;
    u64::try_from(share).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Destination shares per source share, in Q64.64.
///
/// `div_shr` works on a 256-bit numerator, so a bin whose shares are worth far
/// more than one destination share cannot overflow the ratio.
fn share_ratio(minted: u128, source_supply: u128) -> Result<u128> {
    math_core::div_shr(minted, source_supply).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Shares a claim of `shares` in a moved bin is worth in its destination, at the
/// hop's recorded ratio. Rounds down, which can lose at most one ulp per hop.
fn convert_shares(shares: u128, ratio: u128) -> Result<u128> {
    math_core::mul_shr(shares, ratio).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// The terminal shares a withdrawal of `shares` out of a position of
/// `position_shares` is entitled to, when the whole position is worth
/// `terminal_shares` in the bin that holds the assets.
///
/// Converting the whole position first and slicing it here - rather than
/// converting the slice - is what makes the parts of a position add back up to
/// the whole, down to the last unit.
fn claimed_shares(terminal_shares: u128, shares: u128, position_shares: u128) -> Result<u128> {
    if shares == position_shares {
        return Ok(terminal_shares);
    }
    let claimed = terminal_shares
        .checked_mul(shares)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(position_shares)
        .ok_or(ErrorCode::MathOverflow)?;
    Ok(claimed)
}

/// Follows a bin's move chain to the bin that holds its liquidity, converting
/// the shares at each hop. Returns the terminal bin id and the shares the claim
/// is worth there.
///
/// `lookup` reads one bin by id, so the same walk serves the chain inside a
/// single array and a chain that crosses arrays, and can be unit tested without
/// any accounts.
fn resolve_position(
    shares: u128,
    from_bin_id: i32,
    lookup: impl Fn(i32) -> Result<Bin>,
) -> Result<(i32, u128)> {
    let mut cursor = from_bin_id;
    let mut remaining = shares;
    let mut direction = 0i64;

    // There is deliberately no hop-count limit: a legitimate chain has to be
    // followed to its end, or the LP's funds are stranded in the bin it left
    // behind. `check_move` only records a move that brings liquidity *toward*
    // the active bin, and a bin moves at most once, so a real chain is strictly
    // monotonic and finite - it always terminates at a bin nobody has moved.
    // The guard below refuses anything else, so corrupt state cannot make this
    // loop forever.
    loop {
        let bin = lookup(cursor)?;
        if !bin.is_moved() {
            return Ok((cursor, remaining));
        }
        let hop = bin.move_state;

        let step = i64::from(hop.to_bin_id) - i64::from(cursor);
        require!(step != 0, ErrorCode::MergeChainNotMonotonic);
        require!(
            direction == 0 || direction == step.signum(),
            ErrorCode::MergeChainNotMonotonic
        );
        direction = step.signum();

        remaining = convert_shares(remaining, hop.share_ratio)?;
        require!(remaining > 0, ErrorCode::ZeroAmount);
        cursor = hop.to_bin_id;
    }
}

/// Whether a move of `source` into `dest` is one the pool allows.
///
/// Kept separate from the handler - and free of accounts - so that every guard
/// can be tested on its own. The rules, and why:
///
/// - a static bin's whole promise is a fixed price, so it never moves;
/// - a right bin only follows a rising price, and a left bin only a falling one,
///   because that is what an LP funding those kinds asked for;
/// - the source has to be out of range: below the active bin for a move right
///   (where its A has been bought out) and above it for a move left. Moving an
///   in-range bin would yank liquidity out from under the price;
/// - the destination has to be between the source and the active bin, so a move
///   always brings liquidity closer to the price and never leaves it behind;
/// - the destination has to be empty or of the same kind: a bin has one kind, so
///   mixing a static bin into a moving one (or the reverse) would silently
///   change the terms of the liquidity already in it.
/// What has to be true of the bin being moved, whatever it is being moved into.
///
/// Split out so `move_bins` can run it *before* it goes looking for the
/// destination's array: a static bin has no destination it could move to and a
/// bin that has already moved cannot move again, and neither answer should
/// depend on the caller having passed a destination array at all.
fn check_move_source(source: &Bin) -> Result<()> {
    // Asked before the reserves: a bin that has moved keeps its supply but its
    // reserves are zeroed, so the other order would report a moved bin as empty.
    require!(!source.is_moved(), ErrorCode::BinAlreadyMoved);
    require!(source.supply > 0, ErrorCode::ZeroAmount);
    require!(
        source.reserve_a > 0 || source.reserve_b > 0,
        ErrorCode::ZeroAmount
    );
    require!(source.kind != BinKind::Static, ErrorCode::StaticCannotMove);
    Ok(())
}

fn check_move(
    active_bin_id: i32,
    source: &Bin,
    dest: &Bin,
    from_bin_id: i32,
    to_bin_id: i32,
) -> Result<()> {
    check_move_source(source)?;
    require!(!dest.is_moved(), ErrorCode::BinAlreadyMoved);
    require!(from_bin_id != to_bin_id, ErrorCode::WrongMoveDirection);

    let moving_right = to_bin_id > from_bin_id;
    match source.kind {
        BinKind::Static => return Err(ErrorCode::StaticCannotMove.into()),
        BinKind::Right => require!(moving_right, ErrorCode::WrongMoveDirection),
        BinKind::Left => require!(!moving_right, ErrorCode::WrongMoveDirection),
        BinKind::Both => {}
    }

    if moving_right {
        require!(from_bin_id < active_bin_id, ErrorCode::BinStillInRange);
        require!(to_bin_id <= active_bin_id, ErrorCode::WrongMoveDirection);
    } else {
        require!(from_bin_id > active_bin_id, ErrorCode::BinStillInRange);
        require!(to_bin_id >= active_bin_id, ErrorCode::WrongMoveDirection);
    }

    require!(
        to_bin_id.abs_diff(from_bin_id) <= MAX_SHIFT_BINS,
        ErrorCode::ShiftTooFar
    );
    require!(dest.price_x64 > 0, ErrorCode::BinNotInitialized);
    require!(
        dest.supply == 0 || dest.kind == source.kind,
        ErrorCode::BinKindMismatch
    );
    Ok(())
}

/// Merges `source` into `dest` at `to_bin_id`: converts the source's reserves
/// into destination shares and records the hop the source's LPs have to follow.
///
/// The moved reserves are valued at the destination's price, because that is the
/// price they will trade at from now on, and minted at the destination's own
/// value per share:
///
/// ```text
/// minted = moved_value * dest_supply / dest_value
/// ```
///
/// so the destination's value per share does not move (up to one unit of
/// rounding) and neither, therefore, does the claim of the LPs it already had.
/// The multiplication is done in two 256-bit steps to avoid overflowing `u128`
/// on a large reserve, at the cost of one extra rounding-down unit.
fn merge_bins(source: &mut Bin, dest: &mut Bin, to_bin_id: i32) -> Result<()> {
    require!(source.supply > 0, ErrorCode::ZeroAmount);
    require!(!source.is_moved(), ErrorCode::BinAlreadyMoved);
    require!(!dest.is_moved(), ErrorCode::BinAlreadyMoved);

    let moved_value = value_in_a(source.reserve_a, source.reserve_b, dest.price_x64)?;
    require!(moved_value > 0, ErrorCode::ZeroAmount);

    let minted = if dest.supply == 0 {
        // A pristine destination adopts the source's kind, and its share unit is
        // the value deposited: the same rule `add_liquidity` uses.
        dest.kind = source.kind;
        moved_value
    } else {
        require!(dest.kind == source.kind, ErrorCode::BinKindMismatch);
        let dest_value = value_in_a(dest.reserve_a, dest.reserve_b, dest.price_x64)?;
        require!(dest_value > 0, ErrorCode::InsufficientLiquidity);

        let scaled = math_core::mul_shr(moved_value, dest.supply)
            .map_err(|_| error!(ErrorCode::MathOverflow))?;
        math_core::div_shr(scaled, dest_value).map_err(|_| error!(ErrorCode::MathOverflow))?
    };
    require!(minted > 0, ErrorCode::ZeroAmount);

    let ratio = share_ratio(minted, source.supply)?;
    require!(ratio > 0, ErrorCode::MathOverflow);

    dest.reserve_a = dest
        .reserve_a
        .checked_add(source.reserve_a)
        .ok_or(ErrorCode::MathOverflow)?;
    dest.reserve_b = dest
        .reserve_b
        .checked_add(source.reserve_b)
        .ok_or(ErrorCode::MathOverflow)?;
    dest.supply = dest
        .supply
        .checked_add(minted)
        .ok_or(ErrorCode::MathOverflow)?;

    // The source's reseves are gone but its supply stays: it is the ledger the
    // positions in that bin are denominated in, and the ratio above is what
    // turns those shares into the destination's.
    source.reserve_a = 0;
    source.reserve_b = 0;
    source.set_moved(to_bin_id, ratio);
    Ok(())
}

/// Burns `shares` of `bin`'s supply and returns the reserves they own, or fails
/// the slippage check without changing anything.
fn settle_withdrawal(bin: &mut Bin, shares: u128, min_a: u64, min_b: u64) -> Result<(u64, u64)> {
    require!(!bin.is_moved(), ErrorCode::BinAlreadyMoved);
    require!(shares > 0, ErrorCode::ZeroAmount);
    require!(bin.supply >= shares, ErrorCode::InsufficientShares);
    require!(bin.supply > 0, ErrorCode::InsufficientLiquidity);

    // Shares of the reserves, rounded down: the pool never pays out more than it
    // holds.
    let amount_a = share_of(bin.reserve_a, shares, bin.supply)?;
    let amount_b = share_of(bin.reserve_b, shares, bin.supply)?;
    require!(
        amount_a >= min_a && amount_b >= min_b,
        ErrorCode::SlippageExceeded
    );

    bin.reserve_a = bin
        .reserve_a
        .checked_sub(amount_a)
        .ok_or(ErrorCode::MathOverflow)?;
    bin.reserve_b = bin
        .reserve_b
        .checked_sub(amount_b)
        .ok_or(ErrorCode::MathOverflow)?;
    bin.supply = bin
        .supply
        .checked_sub(shares)
        .ok_or(ErrorCode::MathOverflow)?;
    Ok((amount_a, amount_b))
}

/// The remaining account holding the pool's bin array for `index`, checked
/// against the canonical PDA before it is trusted.
///
/// A move's destination array and a withdrawal's mid-chain arrays arrive this
/// way, so the check is what stops a caller passing an array of their own with a
/// favourable `Vec<Bin>` inside it. Callers must pass these accounts writable:
/// the same helper serves the array that ends up being written.
fn hop_array_info<'info>(
    program_id: &Pubkey,
    pool_key: &Pubkey,
    index: i64,
    remaining: &'info [AccountInfo<'info>],
) -> Result<&'info AccountInfo<'info>> {
    let (expected_key, _) = bin_array_pda(pool_key, index, program_id);

    for account in remaining {
        if account.owner != program_id || account.key() != expected_key {
            continue;
        }
        require!(account.is_writable, ErrorCode::BinArrayNotWritable);

        // Validated out of the raw bytes rather than by deserialising: the same
        // three checks, without building a `Vec<Bin>` that is then thrown away.
        // Callers that need a bin read it in place (see `read_bin_at`).
        let data = account.try_borrow_data()?;
        require!(data.len() >= BinArray::BINS_OFFSET, ErrorCode::InvalidBinArray);
        require_keys_eq!(
            Pubkey::try_from(&data[8..40]).map_err(|_| error!(ErrorCode::InvalidBinArray))?,
            *pool_key,
            ErrorCode::WrongBinArrayPool
        );
        require!(
            i64::from_le_bytes(
                data[40..48]
                    .try_into()
                    .map_err(|_| error!(ErrorCode::InvalidBinArray))?
            ) == index,
            ErrorCode::BinArrayIndexMismatch
        );
        require!(
            array_bin_count(&data)? == BINS_PER_ARRAY,
            ErrorCode::InvalidBinArray
        );
        return Ok(account);
    }

    Err(ErrorCode::MissingBinArray.into())
}

/// Reads one bin of the pool, from the array the caller already has or from a
/// remaining account, so following a move chain does not care which array a hop
/// lands in.
fn load_bin<'info>(
    program_id: &Pubkey,
    pool_key: &Pubkey,
    primary_index: i64,
    primary: &[Bin],
    bin_id: i32,
    remaining: &'info [AccountInfo<'info>],
) -> Result<Bin> {
    let index = bin_array_index(bin_id);
    let offset = bin_offset(bin_id);

    if index == primary_index {
        require!(array_is_sized(primary), ErrorCode::InvalidBinArray);
        return Ok(primary[offset]);
    }

    let account = hop_array_info(program_id, pool_key, index, remaining)?;
    let data = account.try_borrow_data()?;
    read_bin_at(&data, bin_byte_offset(&data, offset)?)
}

/// Dynamic fee in basis points: the pool's base fee plus a utilization penalty
/// that grows while a bin is one-sided, which is when LPs are most exposed.
fn compute_dynamic_fee(pool: &Pool, bin: &Bin) -> Result<u128> {
    let utilization = if bin.reserve_a > 0 && bin.reserve_b > 0 {
        50u128 // balanced bin
    } else {
        90u128 // imbalanced bin
    };

    // fee = base_fee + base_fee * utilization / 100
    let fee = (pool.base_fee_bps as u128)
        .checked_mul(utilization)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(100)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_add(pool.base_fee_bps as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    Ok(fee)
}

/// The raw-unit Q64.64 price of `bin_id` for a pool with this step and
/// decimals, recomputed from pure math instead of read from an account.
///
/// Bin arrays price every bin deterministically at creation
/// (`priced_bins`), and afterwards only reserves move - so the oracle rail
/// can quote the active bin's price without an account read, exactly as
/// `swap` sees it. This walk replicates `priced_bins` bit for bit: the
/// array's first bin is priced exactly and every later bin compounds one
/// `mul_shr` per bin step. (Chained multiplications round differently from
/// a single direct `1.0001^(tick)` evaluation, which is why this does not
/// just call `calculate_bin_price(bin_id)`.)
fn bin_price_raw(bin_id: i32, bin_step: u16, decimals_a: u8, decimals_b: u8) -> Result<u128> {
    let first = array_first_bin_id(bin_array_index(bin_id))?;
    let step_price = calculate_bin_price(1, bin_step)?;

    let mut price =
        price_in_raw_units(calculate_bin_price(first, bin_step)?, decimals_a, decimals_b)?;
    for _ in first..bin_id {
        price = math_core::mul_shr(price, step_price)
            .map_err(|_| error!(ErrorCode::MathOverflow))?;
    }
    Ok(price)
}

/// Output a bin would pay for `amount_in`, rounded down so the pool never pays
/// out more than the price says. `is_a_to_b` sells A for B.
fn quote_out(bin: &Bin, amount_in: u64, is_a_to_b: bool) -> Result<u128> {
    // Selling A: out B = in A * Q64 / price. Selling B: out A = in B * price / Q64.
    let (factor, divisor) = if is_a_to_b {
        (math_core::Q64, bin.price_x64)
    } else {
        (bin.price_x64, math_core::Q64)
    };
    require!(divisor > 0, ErrorCode::BinNotInitialized);

    Ok((amount_in as u128)
        .checked_mul(factor)
        .ok_or(ErrorCode::MathOverflow)?
        / divisor)
}

/// Input a bin needs to pay out `amount_out`, rounded up, which is the exact
/// amount that drains the bin when the fill is quoted with `quote_out`.
fn required_in_for_out(bin: &Bin, amount_out: u64, is_a_to_b: bool) -> Result<u64> {
    let (factor, divisor) = if is_a_to_b {
        (bin.price_x64, math_core::Q64)
    } else {
        (math_core::Q64, bin.price_x64)
    };
    require!(divisor > 0, ErrorCode::BinNotInitialized);

    let product = (amount_out as u128)
        .checked_mul(factor)
        .ok_or(ErrorCode::MathOverflow)?;
    let needed = product
        .checked_add(divisor - 1)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(divisor)
        .ok_or(ErrorCode::MathOverflow)?;

    u64::try_from(needed).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Fills `remaining` against the bins of one array, starting at `cursor` and
/// moving in `dir` (+1 when selling A, -1 when selling B).
///
/// Each bin pays what it holds: a bin with enough of the output token fills the
/// rest of the trade at its fixed price, and a bin without enough is drained to
/// exactly zero and the walk continues past it. Bins that hold none of the
/// output token are skipped without moving the active bin, so the pool's price
/// only moves for liquidity that was actually traded. A bin that has been moved
/// away holds no reserves at all, so the walk steps over it for free.
///
/// `cursor` is left either on the bin that filled the trade, or just past the
/// edge of the array - one more than the last offset, or -1 - meaning the caller
/// must continue the walk in the neighbouring array. `active` is left on the last
/// bin that took part, which is always a bin from a validated array.
fn fill_bins(
    bins: &mut [Bin],
    cursor: &mut i32,
    active: &mut i32,
    dir: i32,
    remaining: &mut u64,
    out_total: &mut u128,
) -> Result<()> {
    while *remaining > 0 {
        if *cursor < 0 || *cursor as usize >= bins.len() {
            return Ok(());
        }

        let bin = &mut bins[*cursor as usize];
        require!(bin.price_x64 > 0, ErrorCode::BinNotInitialized);

        let available = if dir > 0 {
            bin.reserve_b
        } else {
            bin.reserve_a
        };
        if available == 0 {
            *cursor += dir;
            continue;
        }

        let quoted = quote_out(bin, *remaining, dir > 0)?;
        if quoted <= available as u128 {
            // The bin can fill the rest of the trade at its price.
            let out = u64::try_from(quoted).map_err(|_| error!(ErrorCode::MathOverflow))?;
            if dir > 0 {
                bin.reserve_a = bin
                    .reserve_a
                    .checked_add(*remaining)
                    .ok_or(ErrorCode::MathOverflow)?;
                bin.reserve_b = bin
                    .reserve_b
                    .checked_sub(out)
                    .ok_or(ErrorCode::MathOverflow)?;
            } else {
                bin.reserve_b = bin
                    .reserve_b
                    .checked_add(*remaining)
                    .ok_or(ErrorCode::MathOverflow)?;
                bin.reserve_a = bin
                    .reserve_a
                    .checked_sub(out)
                    .ok_or(ErrorCode::MathOverflow)?;
            }
            *out_total += out as u128;
            *remaining = 0;
            *active = *cursor;
            return Ok(());
        }

        // Not enough output in this bin: take exactly what drains it and cross.
        // `required_in_for_out` rounds up and `quote_out` rounds down, so the
        // amount needed is always at most what is left to spend.
        let spend = required_in_for_out(bin, available, dir > 0)?;
        require!(spend <= *remaining, ErrorCode::MathOverflow);

        if dir > 0 {
            bin.reserve_a = bin
                .reserve_a
                .checked_add(spend)
                .ok_or(ErrorCode::MathOverflow)?;
            bin.reserve_b = 0;
        } else {
            bin.reserve_b = bin
                .reserve_b
                .checked_add(spend)
                .ok_or(ErrorCode::MathOverflow)?;
            bin.reserve_a = 0;
        }
        *out_total += available as u128;
        *remaining -= spend;
        *active = *cursor;
        *cursor += dir;
    }
    Ok(())
}

/// The remaining accounts that a Token-2022 transfer hook needs: everything this
/// program does not own. The bin arrays are picked out of the same slice by
/// owner, so hook accounts must not be owned by this program.
fn hook_accounts<'info>(
    program_id: &Pubkey,
    remaining: &[AccountInfo<'info>],
) -> Vec<AccountInfo<'info>> {
    remaining
        .iter()
        .filter(|account| account.owner != program_id)
        .cloned()
        .collect()
}

/// Transfers `amount` into `to`, returning how much `to` actually gained.
///
/// The delta is measured rather than assumed because Token-2022 transfer-fee
/// extensions can skim part of the amount in flight.
#[allow(clippy::too_many_arguments)]
fn transfer_and_measure<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &mut InterfaceAccount<'info, TokenAccount>,
    authority: &AccountInfo<'info>,
    hooks: &[AccountInfo<'info>],
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<u64> {
    if amount == 0 {
        return Ok(0);
    }

    let balance_before = to.amount;
    transfer_checked(
        token_program,
        from,
        mint,
        &to.to_account_info(),
        authority,
        hooks,
        signer_seeds,
        amount,
    )?;
    to.reload()?;

    to.amount
        .checked_sub(balance_before)
        .ok_or_else(|| error!(ErrorCode::MathOverflow))
}

/// `transfer_checked` (which validates the mint's decimals) for either the
/// legacy SPL Token program or Token-2022.
///
/// `to` is a plain `AccountInfo` because not every caller holds a typed
/// token account: the settlement rail pays into an unchecked custody
/// account whose type is pinned by address (the hook authority's ATA) and
/// re-validated downstream by the settlement program. The token program
/// itself still verifies the account is a token account of `mint`.
///
/// Token-2022 mints may carry a transfer hook, and the hook's extra accounts
/// must be appended to the transfer instruction itself - Anchor's
/// `transfer_checked` CPI does not do that, so those accounts (supplied by the
/// caller as remaining accounts) are added here.
#[allow(clippy::too_many_arguments)]
fn transfer_checked<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    hooks: &[AccountInfo<'info>],
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    // Only Token-2022 can have hooks, so only there do we need to hand-build the
    // instruction with the hook accounts appended.
    if token_program.key() == token_interface::ID && !hooks.is_empty() {
        let mut ix = token_interface::spl_token_2022::instruction::transfer_checked(
            token_program.key,
            &from.key(),
            &mint.key(),
            &to.key(),
            &authority.key(),
            &[],
            amount,
            mint.decimals,
        )?;
        ix.accounts.extend(hooks.iter().map(|account| {
            if account.is_writable {
                AccountMeta::new(*account.key, false)
            } else {
                AccountMeta::new_readonly(*account.key, false)
            }
        }));

        let mut infos: Vec<AccountInfo<'info>> = vec![
            from.to_account_info(),
            mint.to_account_info(),
            to.clone(),
            authority.clone(),
        ];
        infos.extend_from_slice(hooks);

        invoke_signed(&ix, &infos, signer_seeds)?;
        return Ok(());
    }

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

/// How a bin follows the price. See the module docs for what each kind promises
/// the LP who funds it.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BinKind {
    /// Fixed price: the bin never moves.
    #[default]
    Static,
    /// Follows a rising price to the right.
    Right,
    /// Follows a falling price to the left.
    Left,
    /// Follows the price in either direction.
    Both,
}

/// Whether a bin has moved, and where its shares went.
///
/// Fixed size - `moved` (1) + `to_bin_id` (4) + `share_ratio` (16) - so a
/// serialized `Bin` is always the same number of bytes. A variable-length
/// `Option` would be one byte when absent and twenty-one when present, which
/// makes the vector's element length depend on the bin's history and breaks any
/// consumer that indexes it (L-8). `moved == false` is the identity: the other
/// two fields are ignored.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MoveState {
    /// Set once the bin has been merged into another.
    pub moved: bool,
    /// Bin the shares now live in. Meaningful only when `moved`.
    pub to_bin_id: i32,
    /// Shares in that bin per share here, in Q64.64. Meaningful only when
    /// `moved`.
    pub share_ratio: u128,
}

impl MoveState {
    /// Serialized length: `bool` (1) + `i32` (4) + `u128` (16).
    pub const LEN: usize = 1 + 4 + 16;
}

#[account]
pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub bin_step: u16,
    pub base_fee_bps: u16,
    /// Bin the next swap starts in. Moves as trades cross bins.
    pub active_bin_id: i32,
    /// The only account that can pause the pool or rotate itself.
    pub authority: Pubkey,
    /// While paused the pool refuses swaps, deposits and moves. Withdrawals stay
    /// open.
    pub paused: bool,
    /// Whether any static bin holds liquidity yet. A bin whose kind moves is only
    /// accepted once one does.
    pub has_static_baseline: bool,
    /// Decimals of the two mints, which set the scale between whole-token prices
    /// and the raw-unit prices bins store. Needed to price new bin arrays.
    pub decimals_a: u8,
    pub decimals_b: u8,
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
    pub bump: u8,
    pub vault_a_bump: u8,
    pub vault_b_bump: u8,
}

impl Pool {
    pub const LEN: usize =
        8 + 32 + 32 + 2 + 2 + 4 + 32 + 1 + 1 + 1 + 1 + 1 + 32 + 32 + 32 + 8 + 1 + 1 + 1;
}

/// One bin's state: a fixed price, the reserves backing it, and what happens to
/// both when the price moves away.
///
/// The bin id is implicit - a `Bin` is addressed by its index inside a
/// `BinArray` - which is what lets an array hold a whole band in one account.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bin {
    /// Raw token A per raw token B, in Q64.64. See `price_in_raw_units`.
    pub price_x64: u128,
    pub reserve_a: u64,
    pub reserve_b: u64,
    /// Total LP shares outstanding for this bin. It stays put when the bin is
    /// moved: the positions in the bin are denominated in it, and `move_state`
    /// says what those shares are worth where the liquidity went.
    pub supply: u128,
    /// How this bin follows the price, fixed by the first deposit into it.
    pub kind: BinKind,
    /// Whether the bin has been merged into another one, which is also when its
    /// reserves are zeroed. Fixed size, so a `Bin` is always `Bin::LEN` bytes.
    pub move_state: MoveState,
}

impl Bin {
    /// Fixed size: price (16) + reserves (16) + supply (16) + kind (1) +
    /// `move_state` (21).
    pub const LEN: usize = 16 + 8 + 8 + 16 + 1 + MoveState::LEN;

    /// Whether this bin's liquidity has been moved into another bin.
    pub fn is_moved(&self) -> bool {
        self.move_state.moved
    }

    /// Records the hop this bin's shares took to `to_bin_id`.
    pub fn set_moved(&mut self, to_bin_id: i32, share_ratio: u128) {
        self.move_state = MoveState {
            moved: true,
            to_bin_id,
            share_ratio,
        };
    }
}

/// A contiguous run of bins, from `index * BINS_PER_ARRAY` upwards.
#[account]
#[derive(Debug)]
pub struct BinArray {
    pub pool: Pubkey,
    /// Which run of bin ids this array covers.
    pub index: i64,
    /// Exactly `BINS_PER_ARRAY` bins, ordered by ascending bin id.
    pub bins: Vec<Bin>,
}

impl BinArray {
    /// Discriminator + pool + index + the bins vector's length prefix + the bins.
    ///
    /// Every bin is `Bin::LEN` bytes, moved or not (L-8), so this is exact:
    /// `BINS_PER_ARRAY * Bin::LEN` fills the account with no spare room needed.
    pub const LEN: usize = 8 + 32 + 8 + 4 + BINS_PER_ARRAY * Bin::LEN;

    /// The bins vector's length prefix, in the account's data.
    const COUNT_OFFSET: usize = 8 + 32 + 8;

    /// The first bin, in the account's data.
    const BINS_OFFSET: usize = Self::COUNT_OFFSET + 4;
}

#[account]
pub struct Position {
    pub pool: Pubkey,
    pub owner: Pubkey,
    /// The bin the liquidity was deposited into. It does not change when the bin
    /// moves: `remove_liquidity` follows the move chain from here.
    pub bin_id: i32,
    pub liquidity_shares: u128,
}

impl Position {
    pub const LEN: usize = 8 + 32 + 32 + 4 + 16;
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(array_index: i64)]
pub struct InitializePool<'info> {
    /// Pays for the pool, its vaults and its first bin array. This is not the
    /// pool's admin - the `authority` in the instruction is.
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

    /// Bin array holding the pool's first active bin. Created with every bin in
    /// it priced, so liquidity can be added across the band immediately.
    #[account(
        init,
        payer = payer,
        space = BinArray::LEN,
        seeds = [BIN_ARRAY_SEED, pool.key().as_ref(), &array_index.to_le_bytes()],
        bump
    )]
    pub bin_array: Box<Account<'info, BinArray>>,

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

    /// The pool's oracle state PDA, created here with the pool PDA signing.
    /// Pass this program's own ID in place of the account to leave the rail
    /// off.
    /// Oracle state, created here by the oracle program with the pool signing.
    /// Optional: an off-rail pool passes this program's own ID as the sentinel
    /// (Anchor reads that as `None`, so the `mut` requirement is skipped), while
    /// a wired pool passes the writable `[b"oracle", pool]` PDA.
    /// CHECK: address-pinned to `[b"oracle", pool]` in the handler, then created
    /// by the oracle program's own `initialize` CPI.
    #[account(mut)]
    pub oracle_state: Option<UncheckedAccount<'info>>,

    /// The KYC gate program. Required whenever `kyc_config` is set, so a
    /// pool cannot record a "KYC-required" policy backed by a look-alike
    /// program. Pass the KYC program itself when leaving KYC off.
    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    /// The oracle program, required whenever `oracle_state` is set.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,

    /// The hooks program, required whenever `hooks_authority` is set.
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(index: i64)]
pub struct InitializeBinArray<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        seeds = [POOL_SEED, pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    #[account(
        init,
        payer = payer,
        space = BinArray::LEN,
        seeds = [BIN_ARRAY_SEED, pool.key().as_ref(), &index.to_le_bytes()],
        bump
    )]
    pub bin_array: Box<Account<'info, BinArray>>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(array_index: i64, bin_id: i32)]
pub struct AddLiquidity<'info> {
    /// Mutable because the first static deposit sets the pool's
    /// `has_static_baseline` flag, which is what unlocks the moving kinds.
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    #[account(
        mut,
        seeds = [BIN_ARRAY_SEED, pool.key().as_ref(), &array_index.to_le_bytes()],
        bump
    )]
    pub bin_array: Box<Account<'info, BinArray>>,

    #[account(
        init_if_needed,
        payer = user,
        space = Position::LEN,
        seeds = [POSITION_SEED, pool.key().as_ref(), user.key().as_ref(), &bin_id.to_le_bytes()],
        bump
    )]
    pub position: Box<Account<'info, Position>>,

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

    // Bound to the signer and to the pool's mints (M-6): the input leg is
    // authorized by `user` signing anyway, but pinning the owner means a
    // mis-wired integrator cannot route the swap output to an account the
    // trader does not control.
    #[account(mut, token::mint = mint_a, token::authority = user)]
    pub user_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, token::mint = mint_b, token::authority = user)]
    pub user_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    // Remaining accounts: the transfer-hook accounts, if a mint has a hook.
}

#[derive(Accounts)]
#[instruction(array_index: i64, bin_id: i32)]
pub struct RemoveLiquidity<'info> {
    #[account(
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// The array holding the bin the position was deposited into.
    #[account(
        mut,
        seeds = [BIN_ARRAY_SEED, pool.key().as_ref(), &array_index.to_le_bytes()],
        bump
    )]
    pub bin_array: Box<Account<'info, BinArray>>,

    #[account(
        mut,
        seeds = [POSITION_SEED, pool.key().as_ref(), user.key().as_ref(), &bin_id.to_le_bytes()],
        bump
    )]
    pub position: Box<Account<'info, Position>>,

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

    // Bound to the signer and to the pool's mints (M-6): the input leg is
    // authorized by `user` signing anyway, but pinning the owner means a
    // mis-wired integrator cannot route the swap output to an account the
    // trader does not control.
    #[account(mut, token::mint = mint_a, token::authority = user)]
    pub user_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, token::mint = mint_b, token::authority = user)]
    pub user_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    // Remaining accounts are, in order:
    // - The bin arrays the move chain runs through, writable, one per hop
    // - The transfer-hook accounts, if the mint has a hook
}

#[derive(Accounts)]
#[instruction(from_array_index: i64)]
pub struct MoveBins<'info> {
    #[account(
        mut,
        seeds = [POOL_SEED, pool.mint_a.as_ref(), pool.mint_b.as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

    /// The array holding the bin being moved. The destination's array, when it
    /// is a different one, comes in as a remaining account.
    #[account(
        mut,
        seeds = [BIN_ARRAY_SEED, pool.key().as_ref(), &from_array_index.to_le_bytes()],
        bump
    )]
    pub from_array: Box<Account<'info, BinArray>>,
    // Remaining accounts: the destination bin's array, if it is not in
    // `from_array`.
}

#[derive(Accounts)]
pub struct Swap<'info> {
    /// Mutable because the swap moves the active bin.
    #[account(
        mut,
        seeds = [POOL_SEED, mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump
    )]
    pub pool: Box<Account<'info, Pool>>,

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

    // Bound to the signer and to the pool's mints (M-6): the input leg is
    // authorized by `user` signing anyway, but pinning the owner means a
    // mis-wired integrator cannot route the swap output to an account the
    // trader does not control.
    #[account(mut, token::mint = mint_a, token::authority = user)]
    pub user_token_a: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, token::mint = mint_b, token::authority = user)]
    pub user_token_b: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user: Signer<'info>,

    /// KYC gate config, required iff `pool.kyc_config` is set (pass the pool
    /// PDA itself when the rail is off). The handler checks it against the
    /// pool's recorded policy; the KYC program validates its structure in the
    /// CPI.
    /// CHECK: equality-checked against `pool.kyc_config` in the handler. Optional:
    /// the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub kyc_config: Option<UncheckedAccount<'info>>,

    /// The trader's KYC attestation account, required iff the KYC rail is on.
    /// CHECK: validated inside `kyc_gate::verify_kyc` via CPI.
    pub user_kyc_account: UncheckedAccount<'info>,

    /// Oracle state, required iff `pool.oracle_state` is set. The handler
    /// CPIs `check_deviation` with the pre-trade price and
    /// `record_observation` with the post-trade price.
    /// CHECK: seeds-verified as `[b"oracle", pool]` in the handler. Optional:
    /// the program-ID sentinel means the rail is off.
    #[account(mut)]
    pub oracle_state: Option<UncheckedAccount<'info>>,

    /// Hooks authority PDA, required iff `pool.hooks_authority` is set. Owns
    /// the custody account the swap output is routed into, and **pays for the
    /// escrow** the hooks CPI creates (it is the settlement program's rent
    /// payer), so it has to be writable here or that CPI escalates its
    /// privileges and fails.
    /// CHECK: seeds-verified as the hooks program's `[b"hook_authority"]`
    /// PDA in the handler.
    #[account(mut)]
    pub hooks_authority: Option<UncheckedAccount<'info>>,

    /// The settlement custody account — the hook authority's ATA for the
    /// output mint — required iff the settlement rail is on. It receives the
    /// swap output; the same transaction then routes it into escrow, so its
    /// balance at hook time is exactly this trade's output.
    /// CHECK: address-pinned to the hook authority's ATA in the handler; the
    /// settlement program re-checks it as the escrow's source. Optional: the
    /// program-ID sentinel means the rail is off.
    #[account(mut)]
    pub custody_token_account: Option<UncheckedAccount<'info>>,

    /// The settlement clearinghouse config, required iff the settlement rail
    /// is on. Passed through to the hooks CPI.
    /// CHECK: validated by the settlement program via the hooks CPI.
    pub settlement_config: UncheckedAccount<'info>,

    /// The settlement program, required iff the settlement rail is on.
    /// CHECK: validated by the hooks program (typed `Program<Settlement>`).
    pub settlement_program: UncheckedAccount<'info>,

    /// The settlement vault PDA for `trade_id`, required iff the settlement
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

    /// The associated-token program. Part of the settlement rail's account list:
    /// the rail-off branch pins it to the program-ID sentinel like the other rail
    /// accounts. The rail itself no longer needs it - a settlement escrow is a
    /// PDA of the settlement program, not the vault's ATA, so the settlement
    /// program creates it with the system and token programs - but the account
    /// stays in the list to keep the wired pools' instruction layout unchanged.
    /// CHECK: pinned to this program's ID when the settlement rail is off.
    pub associated_token_program: UncheckedAccount<'info>,

    /// The KYC gate program, required whenever the KYC rail is on. The handler
    /// CPIs into it, and a CPI target has to appear in the instruction's account
    /// list for the runtime to resolve it.
    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    /// The oracle program, required whenever the oracle rail is on. Both oracle
    /// CPIs (`check_deviation`, `record_observation`) go through it.
    pub oracle_program: Option<Program<'info, oracle::program::Oracle>>,

    /// The hooks program, required whenever the settlement rail is on (it is the
    /// program that locks the output in escrow).
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,

    // Remaining accounts are, in order:
    // - The `BinArray` accounts the swap walks, in walk order
    // - The transfer-hook accounts, if the mint has a hook
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
    #[msg("Invalid bin step")]
    InvalidBinStep,
    #[msg("Fee too high")]
    FeeTooHigh,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Slippage exceeded")]
    SlippageExceeded,
    #[msg("Insufficient liquidity to fill the swap")]
    InsufficientLiquidity,
    #[msg("Bin has no price: its bin array was never initialized")]
    BinNotInitialized,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Not enough LP shares")]
    InsufficientShares,
    #[msg("Position does not belong to this owner")]
    Unauthorized,
    #[msg("Pool is paused")]
    PoolPaused,
    #[msg("Mint decimals above the supported maximum")]
    UnsupportedDecimals,
    #[msg("No bin array was supplied to swap through")]
    NoBinArrays,
    #[msg("Bin array does not belong to this pool")]
    WrongBinArrayPool,
    #[msg("Bin array is not the canonical PDA for its index")]
    InvalidBinArray,
    #[msg("Bin arrays must be supplied in the order the swap walks them")]
    BinArrayIndexMismatch,
    #[msg("Bin does not belong to the given bin array")]
    BinNotInArray,
    #[msg("Bin array is too far from the active bin")]
    BinArrayOutOfRange,
    #[msg("Bin array must be passed as writable")]
    BinArrayNotWritable,
    #[msg("Maverick pools require a Static baseline before directional LPing")]
    NoStaticBaseline,
    #[msg("Static bins cannot be migrated")]
    StaticCannotMove,
    #[msg("Bin kind does not allow movement in this direction")]
    WrongMoveDirection,
    #[msg("Bin is still in range of the active bin")]
    BinStillInRange,
    #[msg("A move cannot carry liquidity further than one bin array")]
    ShiftTooFar,
    #[msg("Bin kind does not match the kind of the bin it is being merged into")]
    BinKindMismatch,
    #[msg("Bin has already been moved: its liquidity is in another bin")]
    BinAlreadyMoved,
    #[msg("The bin array holding a moved bin was not supplied")]
    MissingBinArray,
    #[msg("Move chain does not advance toward the active bin")]
    MergeChainNotMonotonic,
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
    #[msg("Oracle deviation threshold must be non-zero when the rail is on, zero when off")]
    InvalidOracleThreshold,
    #[msg("This pool has no oracle configured")]
    OracleNotConfigured,
    #[msg("The new authority cannot be the default pubkey")]
    InvalidAuthority,
    #[msg("Settlement custody account is not the hook authority's ATA for the output mint")]
    InvalidCustodyAccount,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes a Q64.64 value into an `f64` for comparison against a reference.
    fn as_f64(value: u128) -> f64 {
        value as f64 / 2f64.powi(64)
    }

    fn assert_close(actual: u128, expected: f64) {
        let actual = as_f64(actual);
        let relative_error = (actual - expected).abs() / expected;
        assert!(
            relative_error < 1e-9,
            "expected ~{expected}, got {actual} (relative error {relative_error})"
        );
    }

    /// An array of bins with no reserves. Real arrays are priced in full when
    /// they are created, so these are priced at parity.
    fn empty_bins() -> Vec<Bin> {
        vec![
            Bin {
                price_x64: math_core::Q64,
                reserve_a: 0,
                reserve_b: 0,
                supply: 0,
                kind: BinKind::Static,
                move_state: MoveState::default(),
            };
            BINS_PER_ARRAY
        ]
    }

    fn bin_with(price_x64: u128, reserve_a: u64, reserve_b: u64) -> Bin {
        Bin {
            price_x64,
            reserve_a,
            reserve_b,
            supply: 1,
            kind: BinKind::Both,
            move_state: MoveState::default(),
        }
    }

    /// A bin that holds liquidity at parity with `share_value` units of value
    /// per share, i.e. a reserve of `n` whole tokens and `n * Q64` shares.
    fn balanced_bin(n: u64) -> Bin {
        Bin {
            price_x64: math_core::Q64,
            reserve_a: n,
            reserve_b: n,
            supply: (n as u128) * math_core::Q64,
            kind: BinKind::Both,
            move_state: MoveState::default(),
        }
    }

    fn test_pool() -> Pool {
        Pool {
            mint_a: Pubkey::default(),
            mint_b: Pubkey::default(),
            bin_step: 10,
            base_fee_bps: 30,
            active_bin_id: 0,
            authority: Pubkey::default(),
            paused: false,
            has_static_baseline: true,
            decimals_a: 0,
            decimals_b: 0,
            kyc_config: Pubkey::default(),
            oracle_state: Pubkey::default(),
            hooks_authority: Pubkey::default(),
            settlement_slots: 0,
            bump: 0,
            vault_a_bump: 0,
            vault_b_bump: 0,
        }
    }

    /// Value of a bin's reserves, in Q64.64 raw A units.
    fn bin_value(bin: &Bin) -> u128 {
        value_in_a(bin.reserve_a, bin.reserve_b, bin.price_x64).unwrap()
    }

    // -- bin pricing ----------------------------------------------------

    #[test]
    fn bin_zero_is_parity() {
        // Bin 0 is the reference bin, so its price is exactly 1.0.
        assert_eq!(calculate_bin_price(0, 1).unwrap(), math_core::Q64);
        assert_eq!(calculate_bin_price(0, 100).unwrap(), math_core::Q64);
    }

    #[test]
    fn bin_price_steps_one_basis_point_per_tick() {
        // A bin of 10 bps spans 10 ticks, so adjacent bins differ by 1.0001^10.
        let previous = calculate_bin_price(4, 10).unwrap();
        let current = calculate_bin_price(5, 10).unwrap();
        let ratio = as_f64(current) / as_f64(previous);
        assert!(
            (ratio - 1.0001f64.powi(10)).abs() < 1e-9,
            "bin step of 10 bps should be +0.100045%, got {ratio}"
        );
    }

    #[test]
    fn negative_bins_price_below_parity() {
        assert_close(calculate_bin_price(-1, 1).unwrap(), 1.0 / 1.0001);
    }

    #[test]
    fn out_of_range_bin_is_rejected() {
        // A bin that maps past the last usable tick cannot be priced.
        assert!(calculate_bin_price(i32::MAX, 100).is_err());
    }

    #[test]
    fn bin_prices_are_quoted_in_raw_units() {
        // Equal decimals: the raw-unit price is the whole-token price.
        assert_eq!(
            price_in_raw_units(math_core::Q64, 6, 6).unwrap(),
            math_core::Q64
        );

        // A has 9 decimals and B has 6, so one whole A is a thousand raw A per
        // raw B: the price scales up by 10^3, exactly.
        assert_eq!(
            price_in_raw_units(math_core::Q64, 9, 6).unwrap(),
            1_000 * math_core::Q64
        );

        // The other way round the price scales down by 10^3, which is not
        // representable exactly and rounds down.
        assert_eq!(
            price_in_raw_units(math_core::Q64, 6, 9).unwrap(),
            math_core::Q64 / 1_000
        );

        // A pool can never be created with decimals past the cap.
        assert!(price_in_raw_units(math_core::Q64, 10, 0).is_err());
        assert!(price_in_raw_units(math_core::Q64, 0, 200).is_err());
    }

    // -- bin arrays -----------------------------------------------------

    #[test]
    fn bin_arrays_tile_the_number_line() {
        assert_eq!(bin_array_index(0), 0);
        assert_eq!(bin_offset(0), 0);
        assert_eq!(bin_array_index(BINS_PER_ARRAY as i32 - 1), 0);
        assert_eq!(bin_array_index(BINS_PER_ARRAY as i32), 1);

        // Negative bins belong to array -1, counted down from its top.
        assert_eq!(bin_array_index(-1), -1);
        assert_eq!(bin_offset(-1), BINS_PER_ARRAY - 1);
        assert_eq!(bin_array_index(-(BINS_PER_ARRAY as i32)), -1);
        assert_eq!(bin_offset(-(BINS_PER_ARRAY as i32)), 0);
        assert_eq!(bin_array_index(-(BINS_PER_ARRAY as i32) - 1), -2);

        // A bin id always maps back to itself through its array and offset.
        for bin_id in -200..200i32 {
            let round_trip =
                array_first_bin_id(bin_array_index(bin_id)).unwrap() + bin_offset(bin_id) as i32;
            assert_eq!(round_trip, bin_id);
        }
    }

    #[test]
    fn priced_bins_span_the_whole_array() {
        let bins = priced_bins(0, 10, 0, 0).unwrap();
        assert_eq!(bins.len(), BINS_PER_ARRAY);
        // Array 0 starts at bin 0, which is parity.
        assert_eq!(bins[bin_offset(0)].price_x64, math_core::Q64);

        // Each step is one bin step of the previous price.
        let ratio = as_f64(bins[1].price_x64) / as_f64(bins[0].price_x64);
        assert!(
            (ratio - 1.0001f64.powi(10)).abs() < 1e-12,
            "expected a 10 bps step, got {ratio}"
        );

        // An array below zero is priced below parity, and every bin in an array
        // is priced so that liquidity can be added anywhere in it.
        let below = priced_bins(-1, 10, 0, 0).unwrap();
        assert_close(
            below[0].price_x64,
            1.0001f64.powi(-10 * BINS_PER_ARRAY as i32),
        );
        assert!(below.iter().all(|bin| bin.price_x64 > 0));
        assert!(below
            .windows(2)
            .all(|pair| pair[0].price_x64 < pair[1].price_x64));
    }

    #[test]
    fn account_sizes_match_their_layout() {
        // A bin carries its kind and its move link on top of the DLMM's four
        // fields, which is why it is 70 bytes rather than 48.
        assert_eq!(Bin::LEN, 16 + 8 + 8 + 16 + 1 + 1 + 4 + 16);
        // And that is why an array holds 50 bins rather than 70: it still has to
        // stay a cheap account to rent.
        assert!(
            BinArray::LEN < 4 * 1024,
            "BinArray::LEN is {}",
            BinArray::LEN
        );
    }

    #[test]
    fn fresh_bins_are_static_and_unmoved() {
        // A bin's kind belongs to the liquidity in it, so a priced bin starts
        // out with neither kind nor move link to speak of.
        let bins = priced_bins(0, 10, 0, 0).unwrap();
        assert!(bins
            .iter()
            .all(|bin| bin.kind == BinKind::Static && !bin.is_moved()));
    }

    // -- quoting and filling --------------------------------------------

    #[test]
    fn value_in_a_uses_the_bin_price() {
        // At parity, 1 000 A plus 500 B is worth 1 500 A (in Q64.64).
        assert_eq!(
            value_in_a(1_000, 500, math_core::Q64).unwrap(),
            1_500 * math_core::Q64
        );
        // B is worth two A each at price 2.0.
        assert_eq!(
            value_in_a(0, 500, 2 * math_core::Q64).unwrap(),
            1_000 * math_core::Q64
        );
    }

    #[test]
    fn quoting_at_parity_is_one_for_one() {
        let bin = bin_with(math_core::Q64, 1_000, 1_000);
        assert_eq!(quote_out(&bin, 100, true).unwrap(), 100);
        assert_eq!(quote_out(&bin, 100, false).unwrap(), 100);
        assert_eq!(required_in_for_out(&bin, 100, true).unwrap(), 100);
    }

    #[test]
    fn quoting_never_pays_out_more_than_the_input_is_worth() {
        // Price 2.0 A per B: selling B yields 2 A each, selling A yields half a B.
        let bin = bin_with(2 * math_core::Q64, 10_000, 10_000);
        assert_eq!(quote_out(&bin, 100, false).unwrap(), 200);
        assert_eq!(quote_out(&bin, 100, true).unwrap(), 50);

        // A price that does not divide evenly must not round in the trader's
        // favour: topping a quote back up always costs at most what was spent.
        let bin = bin_with(3 * math_core::Q64 / 2, 10_000, 10_000);
        for direction in [true, false] {
            for amount_in in 1..=5_000u64 {
                let out = u64::try_from(quote_out(&bin, amount_in, direction).unwrap()).unwrap();
                let needed = required_in_for_out(&bin, out, direction).unwrap();
                assert!(needed <= amount_in, "{needed} needed for {amount_in} spent");
            }
        }
    }

    #[test]
    fn fill_bins_fills_the_active_bin_at_its_price() {
        let mut bins = empty_bins();
        bins[3] = bin_with(math_core::Q64, 1_000, 1_000);

        let mut cursor = 3i32;
        let mut active = 3i32;
        let mut remaining = 100u64;
        let mut out_total = 0u128;
        fill_bins(
            &mut bins,
            &mut cursor,
            &mut active,
            1,
            &mut remaining,
            &mut out_total,
        )
        .unwrap();

        assert_eq!(remaining, 0, "the whole input should be filled");
        assert_eq!(out_total, 100);
        assert_eq!(active, 3, "the active bin stays on the bin that filled");
        assert_eq!(bins[3].reserve_a, 1_100);
        assert_eq!(bins[3].reserve_b, 900);
    }

    #[test]
    fn fill_bins_crosses_when_a_bin_runs_out() {
        let mut bins = empty_bins();
        // The first bin can only pay 100 B, the second has plenty more.
        bins[0] = bin_with(math_core::Q64, 0, 100);
        bins[1] = bin_with(math_core::Q64, 0, 1_000);

        let mut cursor = 0i32;
        let mut active = 0i32;
        let mut remaining = 150u64;
        let mut out_total = 0u128;
        fill_bins(
            &mut bins,
            &mut cursor,
            &mut active,
            1,
            &mut remaining,
            &mut out_total,
        )
        .unwrap();

        assert_eq!(remaining, 0);
        assert_eq!(out_total, 150);
        // The first bin is drained exactly, and it is left holding the A it took.
        assert_eq!(bins[0].reserve_b, 0);
        assert_eq!(bins[0].reserve_a, 100);
        // The rest of the trade filled in the second bin.
        assert_eq!(bins[1].reserve_a, 50);
        assert_eq!(bins[1].reserve_b, 950);
        assert_eq!(active, 1, "the active bin moved with the trade");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn fill_bins_skips_bins_with_nothing_to_pay_out() {
        let mut bins = empty_bins();
        // Bin 0 is empty and bin 1 has been moved away, so neither can pay.
        bins[1] = bin_with(math_core::Q64, 0, 100);
        bins[2] = bin_with(math_core::Q64, 0, 100);

        let mut cursor = 0i32;
        let mut active = 0i32;
        let mut remaining = 40u64;
        let mut out_total = 0u128;
        fill_bins(
            &mut bins,
            &mut cursor,
            &mut active,
            1,
            &mut remaining,
            &mut out_total,
        )
        .unwrap();

        assert_eq!(remaining, 0);
        assert_eq!(out_total, 40);
        // The empty bin was crossed without becoming the active bin.
        assert_eq!(active, 1);
        assert_eq!(bins[0].reserve_a, 0);
        assert_eq!(bins[1].reserve_a, 40);
    }

    #[test]
    fn fill_bins_hands_the_rest_of_the_array_back_to_the_caller() {
        // Only the last bin of the array can pay out, and it cannot cover the
        // whole trade: the caller has to continue in the next array.
        let mut bins = empty_bins();
        bins[BINS_PER_ARRAY - 1] = bin_with(math_core::Q64, 0, 10);

        let mut cursor = (BINS_PER_ARRAY - 1) as i32;
        let mut active = 0i32;
        let mut remaining = 50u64;
        let mut out_total = 0u128;
        fill_bins(
            &mut bins,
            &mut cursor,
            &mut active,
            1,
            &mut remaining,
            &mut out_total,
        )
        .unwrap();

        assert_eq!(remaining, 40, "the trade is not finished");
        assert_eq!(out_total, 10);
        assert_eq!(
            cursor, BINS_PER_ARRAY as i32,
            "one past the end of the array"
        );
        assert_eq!(active, (BINS_PER_ARRAY - 1) as i32);
    }

    #[test]
    fn fill_bins_walks_downwards_when_selling_b() {
        let mut bins = empty_bins();
        bins[5] = bin_with(math_core::Q64, 100, 0);
        bins[4] = bin_with(math_core::Q64, 1_000, 0);

        let mut cursor = 5i32;
        let mut active = 5i32;
        let mut remaining = 150u64;
        let mut out_total = 0u128;
        fill_bins(
            &mut bins,
            &mut cursor,
            &mut active,
            -1,
            &mut remaining,
            &mut out_total,
        )
        .unwrap();

        assert_eq!(remaining, 0);
        assert_eq!(out_total, 150);
        assert_eq!(bins[5].reserve_a, 0);
        assert_eq!(bins[5].reserve_b, 100);
        assert_eq!(bins[4].reserve_a, 950);
        assert_eq!(active, 4);
        assert_eq!(cursor, 4);
    }

    // -- shares and fees ------------------------------------------------

    #[test]
    fn withdrawals_round_in_the_pools_favour() {
        // 1 of 3 shares of 1 000 is 333.33, paid out as 333.
        assert_eq!(share_of(1_000, 1, 3).unwrap(), 333);
        assert_eq!(share_of(1_000, 3, 3).unwrap(), 1_000);
        assert_eq!(share_of(1_000, 0, 3).unwrap(), 0);
    }

    #[test]
    fn fees_rise_while_a_bin_is_one_sided() {
        let pool = test_pool();
        let balanced = bin_with(math_core::Q64, 1, 1);
        let one_sided = bin_with(math_core::Q64, 1, 0);

        assert_eq!(compute_dynamic_fee(&pool, &balanced).unwrap(), 45); // 30 + 30 * 50%
        assert_eq!(compute_dynamic_fee(&pool, &one_sided).unwrap(), 57); // 30 + 30 * 90%
    }

    #[test]
    fn bin_price_raw_matches_the_priced_array() {
        // The oracle rail quotes the active bin's price from pure math; it
        // must land exactly on what a created bin array stores for that bin,
        // for both directions of decimal scaling.
        for (decimals_a, decimals_b) in [(6u8, 6u8), (6, 9), (9, 6), (0, 0)] {
            for step in [1u16, 10, 100] {
                for bin_id in [0i32, 5, -5, 49, -50] {
                    let from_array = priced_bins(
                        bin_array_index(bin_id),
                        step,
                        decimals_a,
                        decimals_b,
                    )
                    .unwrap()[bin_offset(bin_id)]
                        .price_x64;
                    assert_eq!(
                        bin_price_raw(bin_id, step, decimals_a, decimals_b).unwrap(),
                        from_array,
                        "bin {bin_id} (step {step}, decimals {decimals_a}/{decimals_b})"
                    );
                }
            }
        }
    }

    // -- in-place bin access --------------------------------------------

    /// An account's worth of a bin array, priced from `index` the way
    /// `initialize_bin_array` writes one.
    ///
    /// Built byte for byte rather than through a serialiser, because `BinArray`
    /// is an `#[account]` - the interesting part of its layout is the vector's
    /// length prefix and the fixed `Bin::LEN` stride of the elements, and that
    /// is exactly what these tests are about. The buffer is `BinArray::LEN`,
    /// like a real account: every bin is written into its own `Bin::LEN` slot.
    fn serialized_array(index: i64, moves: &[(usize, i32)]) -> Vec<u8> {
        let mut bins = priced_bins(index, 10, 6, 6).unwrap();
        for (offset, hop) in moves {
            bins[*offset].reserve_a = 0;
            bins[*offset].reserve_b = 0;
            bins[*offset].set_moved(*hop, math_core::Q64);
        }

        let mut data = vec![0u8; BinArray::LEN];
        data[40..48].copy_from_slice(&index.to_le_bytes());
        data[BinArray::COUNT_OFFSET..BinArray::BINS_OFFSET]
            .copy_from_slice(&(BINS_PER_ARRAY as u32).to_le_bytes());

        let mut at = BinArray::BINS_OFFSET;
        for bin in &bins {
            let mut bytes = Vec::new();
            bin.serialize(&mut bytes).unwrap();
            assert_eq!(bytes.len(), Bin::LEN, "bins are a fixed stride");
            data[at..at + Bin::LEN].copy_from_slice(&bytes);
            at += Bin::LEN;
        }
        data
    }

    #[test]
    fn reading_a_bin_in_place_agrees_with_deserialising_the_array() {
        for moves in [vec![], vec![(0usize, 7i32)], vec![(3, 9), (17, 20)]] {
            let data = serialized_array(0, &moves);
            let bins = priced_bins(0, 10, 6, 6).unwrap();

            for offset in 0..BINS_PER_ARRAY {
                let at = bin_byte_offset(&data, offset).unwrap();
                let bin = read_bin_at(&data, at).unwrap();

                assert_eq!(bin.price_x64, bins[offset].price_x64, "bin {offset}");
                assert_eq!(bin.supply, bins[offset].supply);
                let moved = moves.iter().find(|(i, _)| *i == offset).map(|(_, h)| *h);
                let recorded = bin.is_moved().then_some(bin.move_state.to_bin_id);
                assert_eq!(recorded, moved, "bin {offset}");
            }
        }
    }

    #[test]
    fn a_move_does_not_shift_the_offsets_after_it() {
        // Every bin is `Bin::LEN` bytes, moved or not (L-8), so a bin that has
        // moved is no wider than one that has not: the offsets are a fixed stride
        // and nothing after a moved bin is displaced.
        let flat = serialized_array(0, &[]);
        let hopped = serialized_array(0, &[(2, 4)]);

        for offset in 0..BINS_PER_ARRAY {
            assert_eq!(
                bin_byte_offset(&hopped, offset).unwrap(),
                bin_byte_offset(&flat, offset).unwrap(),
                "bin {offset}"
            );
        }
    }

    #[test]
    fn writing_a_bin_in_place_round_trips() {
        let mut data = serialized_array(0, &[]);
        let at = bin_byte_offset(&data, 5).unwrap();

        let mut bin = read_bin_at(&data, at).unwrap();
        bin.reserve_a = 4242;
        bin.reserve_b = 24;
        bin.supply = 5 * math_core::Q64;
        bin.kind = BinKind::Left;
        write_bin_at(&mut data, at, &bin).unwrap();

        let back = read_bin_at(&data, at).unwrap();
        assert_eq!(back.reserve_a, 4242);
        assert_eq!(back.reserve_b, 24);
        assert_eq!(back.supply, 5 * math_core::Q64);
        assert_eq!(back.kind, BinKind::Left);
        // Nothing after it moved.
        assert_eq!(
            bin_byte_offset(&data, 6).unwrap(),
            bin_byte_offset(&serialized_array(0, &[]), 6).unwrap()
        );
    }

    #[test]
    fn the_in_place_writer_records_a_move() {
        // The element is fixed size, so writing a bin that has just moved is a
        // plain in-place overwrite: it is recorded and nothing after it shifts.
        let mut data = serialized_array(0, &[]);
        let at = bin_byte_offset(&data, 1).unwrap();
        let mut bin = read_bin_at(&data, at).unwrap();
        bin.set_moved(3, math_core::Q64);
        write_bin_at(&mut data, at, &bin).unwrap();

        let back = read_bin_at(&data, at).unwrap();
        assert!(back.is_moved());
        assert_eq!(back.move_state.to_bin_id, 3);
        assert_eq!(
            bin_byte_offset(&data, 2).unwrap(),
            bin_byte_offset(&serialized_array(0, &[]), 2).unwrap()
        );
    }

    #[test]
    fn an_out_of_range_bin_index_is_refused() {
        let data = serialized_array(0, &[]);
        assert!(bin_byte_offset(&data, BINS_PER_ARRAY).is_err());
        assert!(bin_byte_offset(&data, usize::MAX).is_err());
        // A truncated array is refused rather than read past its end.
        let short = &data[..BinArray::BINS_OFFSET + 3];
        assert!(bin_byte_offset(short, 0).is_err());
        assert!(bin_byte_offset(&data[..40], 0).is_err());
    }

    // -- moving bins ----------------------------------------------------

    /// A source bin below the price, holding only B, and an empty bin nearer the
    /// active bin to move it into.
    fn move_fixture(kind: BinKind) -> (Bin, Bin) {
        let source = Bin {
            reserve_b: 1_000,
            supply: 1_000 * math_core::Q64,
            kind,
            ..balanced_bin(0)
        };
        (source, balanced_bin(0))
    }

    #[test]
    fn static_bins_cannot_move() {
        // The whole promise of a static bin is a fixed price.
        let (source, dest) = move_fixture(BinKind::Static);
        assert!(check_move(5, &source, &dest, 0, 4).is_err());
    }

    #[test]
    fn a_kind_only_moves_the_way_it_follows() {
        // The source sits below the active bin, so only a move right is on offer.
        let (source, dest) = move_fixture(BinKind::Right);
        assert!(check_move(5, &source, &dest, 0, 4).is_ok());

        let (source, dest) = move_fixture(BinKind::Left);
        assert!(check_move(5, &source, &dest, 0, 4).is_err());
        // A left bin above the active bin is the mirror image.
        assert!(check_move(5, &source, &dest, 9, 6).is_ok());

        // A both bin takes either direction.
        let (source, dest) = move_fixture(BinKind::Both);
        assert!(check_move(5, &source, &dest, 0, 4).is_ok());
        assert!(check_move(5, &source, &dest, 9, 6).is_ok());
    }

    #[test]
    fn an_in_range_bin_cannot_be_moved() {
        // Moving a bin that the price is currently trading in would pull
        // liquidity out from under it.
        let (source, dest) = move_fixture(BinKind::Both);
        assert!(check_move(5, &source, &dest, 5, 4).is_err());
        assert!(check_move(5, &source, &dest, 5, 6).is_err());
    }

    #[test]
    fn a_move_has_to_approach_the_active_bin() {
        // The source is below the price, so the destination has to be above it
        // and no further than the active bin.
        let (source, dest) = move_fixture(BinKind::Both);
        assert!(check_move(5, &source, &dest, 3, 3).is_err(), "no self-move");
        assert!(
            check_move(5, &source, &dest, 3, 1).is_err(),
            "away from price"
        );
        assert!(
            check_move(5, &source, &dest, 3, 6).is_err(),
            "past the price"
        );
        assert!(
            check_move(5, &source, &dest, 3, 5).is_ok(),
            "up to the price"
        );
    }

    #[test]
    fn a_move_is_bounded_by_one_array() {
        let (source, dest) = move_fixture(BinKind::Both);
        // Far enough that the source and the destination need not even be
        // neighbours: the move has to be done in steps.
        assert!(check_move(1_000, &source, &dest, 0, MAX_SHIFT_BINS as i32 + 1).is_err());
        assert!(check_move(1_000, &source, &dest, 0, MAX_SHIFT_BINS as i32).is_ok());
    }

    #[test]
    fn a_bin_with_no_liquidity_cannot_move() {
        let dest = balanced_bin(0);

        // A bin that holds shares but no reserves has already had its value
        // moved, and a bin nobody has funded has neither shares nor reserves.
        let drained = Bin {
            supply: 1_000 * math_core::Q64,
            kind: BinKind::Both,
            ..balanced_bin(0)
        };
        assert!(check_move(5, &drained, &dest, 0, 4).is_err());

        let unfunded = Bin {
            kind: BinKind::Both,
            ..balanced_bin(0)
        };
        assert!(check_move(5, &unfunded, &dest, 0, 4).is_err());
    }

    #[test]
    fn a_kind_mismatch_stops_a_merge() {
        // A funded destination has a kind of its own, and mixing one kind of
        // liquidity into another would change the terms of what is already in
        // it. An empty destination takes the source's kind instead.
        let (source, dest) = move_fixture(BinKind::Both);
        assert!(
            check_move(5, &source, &dest, 0, 4).is_ok(),
            "an empty destination takes the source's kind"
        );

        let mut funded_dest = balanced_bin(1_000);
        funded_dest.kind = BinKind::Right;
        assert!(check_move(5, &source, &funded_dest, 0, 4).is_err());
    }

    // -- merging and the move chain -------------------------------------

    #[test]
    fn merging_preserves_the_value_of_every_share() {
        // Destination: 1 000 A + 1 000 B, and 1 000 Q64 shares, so two units of
        // value per share.
        let mut dest = balanced_bin(1_000);
        let per_share_before = bin_value(&dest) / dest.supply;
        assert_eq!(per_share_before, 2);

        // Source: an out-of-range bin holding 1 000 B, at the same value per
        // share, so 500 Q64 shares.
        let mut source = Bin {
            reserve_b: 1_000,
            supply: 500 * math_core::Q64,
            kind: BinKind::Both,
            ..balanced_bin(0)
        };

        merge_bins(&mut source, &mut dest, 9).unwrap();

        // The destination's existing LPs are not diluted.
        let per_share_after = bin_value(&dest) / dest.supply;
        assert_eq!(per_share_after, per_share_before);

        // The moved bin's LPs can redeem exactly what they had: 1 000 B at parity.
        let hop = source.move_state;
        assert_eq!(hop.to_bin_id, 9);
        let converted = convert_shares(source.supply, hop.share_ratio).unwrap();
        let redeemed = converted
            .checked_mul(per_share_after)
            .expect("value fits in u128");
        assert_close(redeemed, 1_000.0);

        // The source is empty, and says where its shares went.
        assert_eq!(source.reserve_a, 0);
        assert_eq!(source.reserve_b, 0);
        assert!(source.supply > 0, "the source stays the shares' ledger");
    }

    #[test]
    fn merging_into_an_empty_bin_seeds_the_share_unit() {
        let mut dest = balanced_bin(0);
        let mut source = Bin {
            reserve_b: 750,
            kind: BinKind::Right,
            ..balanced_bin(0)
        };
        source.supply = 750 * math_core::Q64;

        merge_bins(&mut source, &mut dest, 3).unwrap();

        // Shares are minted the way a first deposit is: value for value.
        assert_eq!(dest.supply, 750 * math_core::Q64);
        assert_eq!(dest.kind, BinKind::Right);
        assert_eq!(dest.reserve_b, 750);
        // A one-for-one conversion, since the share units are the same.
        assert_eq!(
            convert_shares(source.supply, source.move_state.share_ratio).unwrap(),
            source.supply
        );
    }

    #[test]
    fn merging_re_prices_the_moved_reserves() {
        // A right bin below the price holds B, and B is worth more in A terms at
        // the higher destination price. Crediting that is the point: the bin will
        // trade at the destination's price from now on.
        let mut dest = Bin {
            price_x64: 2 * math_core::Q64,
            ..balanced_bin(0)
        };
        let mut source = Bin {
            reserve_b: 1_000,
            kind: BinKind::Right,
            ..balanced_bin(0)
        };
        source.supply = 2_000 * math_core::Q64; // 1 000 B at the source's parity

        merge_bins(&mut source, &mut dest, 3).unwrap();

        // Minted at the destination's price of 2 A per B: the full 2 000 A worth.
        assert_eq!(dest.supply, 2_000 * math_core::Q64);
    }

    #[test]
    fn a_moved_bin_cannot_move_or_be_deposited_into_again() {
        let mut dest = balanced_bin(0);
        let mut source = Bin {
            reserve_b: 100,
            kind: BinKind::Both,
            ..balanced_bin(0)
        };
        source.supply = 100 * math_core::Q64;

        merge_bins(&mut source, &mut dest, 3).unwrap();

        // Its reserves are gone and its shares point elsewhere, so a second move
        // would double-count the claim.
        assert!(merge_bins(&mut source, &mut dest, 4).is_err());
        assert!(merge_bins(&mut dest, &mut source, 4).is_err());
    }

    #[test]
    fn merging_a_bin_with_nothing_in_it_is_rejected() {
        let mut dest = balanced_bin(0);
        let mut source = Bin {
            kind: BinKind::Both,
            supply: 10 * math_core::Q64,
            ..balanced_bin(0)
        };
        assert!(merge_bins(&mut source, &mut dest, 3).is_err());

        // A merge cannot overwrite the destination's kind either.
        let mut source = Bin {
            reserve_b: 10,
            kind: BinKind::Right,
            ..balanced_bin(0)
        };
        source.supply = 10 * math_core::Q64;
        let mut static_dest = balanced_bin(100);
        static_dest.kind = BinKind::Static;
        assert!(merge_bins(&mut source, &mut static_dest, 3).is_err());
    }

    #[test]
    fn convert_shares_applies_the_ratio() {
        // One share here is worth two there.
        assert_eq!(convert_shares(10, 2 * math_core::Q64).unwrap(), 20);
        // And a ratio of one half rounds down, never up.
        assert_eq!(convert_shares(10, math_core::Q64 / 2).unwrap(), 5);
        assert_eq!(convert_shares(1, math_core::Q64 / 2).unwrap(), 0);
    }

    #[test]
    fn resolve_position_follows_the_chain() {
        let base = 0i32;

        // An unmoved bin is its own terminal.
        let bins = vec![balanced_bin(0); 4];
        let lookup = |id: i32| -> Result<Bin> { Ok(bins[(id - base) as usize]) };
        assert_eq!(resolve_position(7, 0, lookup).unwrap(), (0, 7));

        // A chain of two hops: 20 shares are 40 in bin 1, which are 20 again in
        // bin 2.
        let mut bins = vec![balanced_bin(0); 4];
        bins[0].set_moved(1, 2 * math_core::Q64);
        bins[1].set_moved(2, math_core::Q64 / 2);
        let lookup = |id: i32| -> Result<Bin> { Ok(bins[(id - base) as usize]) };
        assert_eq!(resolve_position(20, 0, lookup).unwrap(), (2, 20));

        // A long chain is followed to its end rather than refused. Liquidity the
        // price has walked past can move many times, and its LP must be able to
        // withdraw from wherever it ended up - a hop cap here would strand the
        // funds in the bin it used to be in.
        let long = 64;
        let mut bins = vec![balanced_bin(0); long + 1];
        for (offset, bin) in bins.iter_mut().take(long).enumerate() {
            bin.set_moved(offset as i32 + 1, math_core::Q64);
        }
        let lookup = |id: i32| -> Result<Bin> { Ok(bins[(id - base) as usize]) };
        assert_eq!(resolve_position(5, 0, lookup).unwrap(), (long as i32, 5));

        // A hop that reverses direction is corrupt state (a real move only ever
        // goes toward the active bin) and is refused rather than looped over.
        let mut bins = vec![balanced_bin(0); 4];
        bins[0].set_moved(1, math_core::Q64);
        bins[1].set_moved(0, math_core::Q64);
        let lookup = |id: i32| -> Result<Bin> { Ok(bins[(id - base) as usize]) };
        assert!(resolve_position(1, 0, lookup).is_err());
    }

    #[test]
    fn the_parts_of_a_position_add_up_to_the_whole() {
        // Converting the whole claim before slicing it is what keeps partial
        // withdrawals from rounding the position's value away.
        let terminal_shares = 1_000_001u128;
        let position_shares = 3u128;

        let first = claimed_shares(terminal_shares, 1, position_shares).unwrap();
        let second = claimed_shares(terminal_shares, 2, position_shares).unwrap();
        let whole = claimed_shares(terminal_shares, 3, position_shares).unwrap();

        assert_eq!(whole, terminal_shares, "a full withdrawal is exact");
        assert!(first + second <= whole);
        assert!(first + second + 1 >= whole, "and loses at most one unit");
    }

    #[test]
    fn a_settled_withdrawal_never_pays_out_more_than_the_bin_holds() {
        let mut bin = balanced_bin(500); // 500 A + 500 B, 500 Q64 shares
        let supply = bin.supply;

        let (a, b) = settle_withdrawal(&mut bin, supply / 4, 0, 0).unwrap();
        assert_eq!((a, b), (125, 125));
        assert_eq!(bin.supply, supply - supply / 4);

        // The slippage floor is checked before anything is written.
        let snapshot = bin;
        let remaining_supply = bin.supply;
        assert!(settle_withdrawal(&mut bin, remaining_supply, 1_000, 0).is_err());
        assert_eq!(bin, snapshot);

        // And a bin that has moved holds nothing, so nothing can be settled in it.
        bin.set_moved(1, math_core::Q64);
        assert!(settle_withdrawal(&mut bin, 1, 0, 0).is_err());
    }
}
