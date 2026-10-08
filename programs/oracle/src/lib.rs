//! # TWAP Oracle & Circuit Breaker
//!
//! This program stores historical price observations for a pool.
//! It provides two main functions:
//! 1. `get_twap`: Calculates the time-weighted average price over a window.
//! 2. `check_deviation`: Compares the current price to the TWAP. If the
//!    deviation exceeds the allowed threshold, it reverts (tripping the circuit breaker).
//!
//! This is critical for RWA tokens, which should never experience massive
//! flash-loan style price swings.

use anchor_lang::prelude::*;

declare_id!("4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj");

/// Number of historical observations to store in the ring buffer.
/// 100 observations allow for flexible TWAP windows.
pub const OBSERVATION_COUNT: usize = 100;

/// Circuit-breaker TWAP window used by `check_deviation` (30 minutes).
pub const DEVIATION_WINDOW_SECONDS: u32 = 1800;

/// Basis-point denominator for deviation math (10_000 bps = 100%).
const BPS_DENOMINATOR: u128 = 10_000;

#[program]
pub mod oracle {
    use super::*;

    /// Initializes the oracle state for the pool whose PDA is signing.
    ///
    /// The pool PDA must sign, which on-chain means only the AMM program that
    /// owns the pool can create its oracle (through `invoke_signed`, normally
    /// from the pool's own `initialize_pool`). That binding is what closes the
    /// front-running hole: a third party cannot create — and therefore cannot
    /// configure — the oracle for a pool PDA it does not control, and a pool's
    /// threshold and owner are fixed atomically with the pool itself.
    ///
    /// `owner` is the key allowed to `unpause_pool`. It is an explicit argument
    /// so the pool names its own admin/multisig instead of the rent payer.
    ///
    /// The state is a zero-copy account: a 4 078-byte `OracleState` cannot be
    /// borsh-deserialized onto the 4 KB SBF stack, so every handler touches
    /// the account's bytes in place via `AccountLoader`.
    pub fn initialize(ctx: Context<Initialize>, max_deviation_bps: u16, owner: Pubkey) -> Result<()> {
        require!(max_deviation_bps > 0, OracleError::InvalidThreshold);
        require_keys_neq!(owner, Pubkey::default(), OracleError::InvalidOwner);

        let pool_key = ctx.accounts.pool.key();
        let oracle_state = &mut ctx.accounts.oracle_state.load_init()?;
        let clock = Clock::get()?;

        oracle_state.pool = pool_key;
        oracle_state.owner = owner;
        oracle_state.is_paused = 0;
        oracle_state.observation_index = 0;
        oracle_state.observation_count = 1;
        oracle_state.bump = ctx.bumps.oracle_state;
        oracle_state.max_deviation_bps = max_deviation_bps;

        // Seed the ring buffer with the first observation. `sqrt_price_x64 = 0`
        // marks it as "price not yet known"; the first observation recorded by
        // the pool starts the accumulator.
        //
        // Only slot 0 is written: the account was just created, so every slot
        // is already zeroed (all-zero bytes *are* `Observation::default()`).
        oracle_state.observations[0] = Observation {
            timestamp: clock.unix_timestamp,
            sqrt_price_x64: 0,
            cumulative_price_x64: 0,
        };

        msg!("Oracle initialized for pool {}", pool_key);
        Ok(())
    }

    /// Called by the AMM pool after every swap to record the new price.
    pub fn record_observation(ctx: Context<RecordObservation>, sqrt_price_x64: u128) -> Result<()> {
        let oracle_state = &mut ctx.accounts.oracle_state.load_mut()?;
        let clock = Clock::get()?;

        // Verify caller is the authorized pool.
        require_keys_eq!(
            oracle_state.pool,
            ctx.accounts.authority.key(),
            OracleError::UnauthorizedPool
        );
        // The pool must never record a price going back in time: the TWAP
        // accumulator assumes strictly non-decreasing timestamps.
        require!(
            clock.unix_timestamp >= oracle_state.observations[oracle_state.observation_index as usize].timestamp,
            OracleError::TimestampMovedBackwards
        );

        write_observation(oracle_state, clock.unix_timestamp, sqrt_price_x64)?;
        Ok(())
    }

    /// Calculates the TWAP over a specified time window (in seconds).
    ///
    /// Returns the time-weighted average `sqrt_price_x64` over
    /// `[now - seconds_ago, now]`, or `None` if the window is not covered yet.
    pub fn get_twap(ctx: Context<GetTwap>, seconds_ago: u32) -> Result<Option<u128>> {
        get_twap_impl(&*ctx.accounts.oracle_state.load()?, seconds_ago)
    }

    /// The Circuit Breaker guard. Called by the pool before a swap executes.
    ///
    /// Pure check: if the incoming price deviates from the 30-minute TWAP by
    /// more than the stored `max_deviation_bps`, the instruction reverts —
    /// which also reverts the swap it guards. It deliberately does NOT persist
    /// the pause: a reverted instruction's writes are rolled back by the
    /// runtime, so latching is `trip_breaker`'s job, in its own transaction.
    ///
    /// The price is taken from the caller and the caller gates its own swap, so
    /// the instruction is only meaningful coming from the pool that owns this
    /// oracle. That pool must therefore sign: without the signature any program
    /// could probe the TWAP with arbitrary prices, and the check's answer would
    /// be about a price no pool ever had.
    pub fn check_deviation(
        ctx: Context<CheckDeviation>,
        current_sqrt_price_x64: u128,
    ) -> Result<()> {
        let oracle_state = &*ctx.accounts.oracle_state.load()?;

        // Only the pool this oracle was created for may run the check.
        require_keys_eq!(
            oracle_state.pool,
            ctx.accounts.pool.key(),
            OracleError::UnauthorizedPool
        );

        // If pool is already paused, block all trades.
        require!(oracle_state.is_paused == 0, OracleError::PoolPaused);

        // Use a 30-minute TWAP window for RWA stability.
        let twap_price = match get_twap_impl(oracle_state, DEVIATION_WINDOW_SECONDS)? {
            Some(twap) if twap != 0 => twap,
            // Not enough history (or no price recorded yet): nothing to
            // compare against, allow the trade. The alarm log is what a
            // keeper parses; without it the unguarded window (L-2) is only
            // visible by reading account state.
            _ => {
                msg!("ALARM: circuit breaker skipped (TWAP not available yet; < 2 observations)");
                return Ok(());
            }
        };

        // deviation_bps = |current - twap| / twap * 10_000
        let deviation_bps = diff(current_sqrt_price_x64, twap_price)
            .checked_mul(BPS_DENOMINATOR)
            .ok_or(OracleError::MathOverflow)?
            .checked_div(twap_price)
            .ok_or(OracleError::MathOverflow)?;

        require!(
            deviation_bps <= oracle_state.max_deviation_bps as u128,
            OracleError::PriceDeviationExceeded
        );
        Ok(())
    }

    /// Latches the circuit breaker's pause. Permissionless.
    ///
    /// `check_deviation` reverts when a price is out of line — and a reverted
    /// transaction cannot leave state behind, so the pause would never stick.
    /// This instruction evaluates the same deviation against the same stored
    /// threshold and, when it is exceeded, sets `is_paused` in a transaction
    /// that SUCCEEDS, which is what makes the pause survive. Anyone may call
    /// it (a keeper, an arbitrageur, the pool in a follow-up transaction): it
    /// can only ever pause, never unpause, and only when the math says so.
    pub fn trip_breaker(ctx: Context<TripBreaker>, current_sqrt_price_x64: u128) -> Result<()> {
        let mut oracle_state = ctx.accounts.oracle_state.load_mut()?;

        // The price this latches a pause on must be the pool's own, so only the
        // pool PDA may supply it. The pool reads it from its state (never from
        // a caller argument) in the instruction that CPIs here, which is what
        // stops an attacker from pausing a healthy pool with a forged price.
        require_keys_eq!(
            oracle_state.pool,
            ctx.accounts.pool.key(),
            OracleError::UnauthorizedPool
        );
        require!(oracle_state.is_paused == 0, OracleError::PoolPaused);

        let twap_price = match get_twap_impl(&oracle_state, DEVIATION_WINDOW_SECONDS)? {
            Some(twap) if twap != 0 => twap,
            // No comparable history: nothing to trip on (a no-op, not an
            // error). Alarm-tagged so keepers can watch for pools whose
            // breaker has never been armed.
            _ => {
                msg!("ALARM: trip_breaker no-op (TWAP not available yet; < 2 observations)");
                return Ok(());
            }
        };

        let deviation_bps = diff(current_sqrt_price_x64, twap_price)
            .checked_mul(BPS_DENOMINATOR)
            .ok_or(OracleError::MathOverflow)?
            .checked_div(twap_price)
            .ok_or(OracleError::MathOverflow)?;

        let threshold = oracle_state.max_deviation_bps;
        if deviation_bps > threshold as u128 {
            oracle_state.is_paused = 1;
            msg!(
                "Circuit breaker tripped: deviation {} bps > {} bps",
                deviation_bps,
                threshold
            );
            return Ok(());
        }
        Err(OracleError::PriceWithinThreshold.into())
    }

    /// Allows the authority to unpause the pool after a circuit breaker trip.
    pub fn unpause_pool(ctx: Context<UnpausePool>) -> Result<()> {
        let oracle_state = &mut ctx.accounts.oracle_state.load_mut()?;
        require_keys_eq!(
            oracle_state.owner,
            ctx.accounts.authority.key(),
            OracleError::Unauthorized
        );
        oracle_state.is_paused = 0;
        msg!("Pool unpaused by authority");
        Ok(())
    }
}

// -----------------------------------------------------------------------
// HELPER FUNCTIONS
// -----------------------------------------------------------------------

/// Advance the ring buffer by one observation, carrying the cumulative price.
///
/// Returns `MathOverflow` when the accumulator cannot represent the next
/// step (L-1): the previous silent fallback dropped the elapsed time from
/// the TWAP with no error and no event, and a quietly-wrong TWAP is worse
/// for a circuit breaker than one that stops. `get_twap_impl` already
/// propagates the same error on its extrapolation path, so the swap that
/// would have consumed the bad average reverts either way — the difference
/// is that now the *state* cannot drift from the caller's assumption, and
/// the error text names the actual failure instead of a deviation that was
/// never measured.
fn write_observation(state: &mut OracleState, now: i64, sqrt_price_x64: u128) -> Result<()> {
    let current_index = state.observation_index as usize;
    let last_observation = state.observations[current_index];

    // The accumulator only advances while a known price was live.
    let new_cumulative = if last_observation.sqrt_price_x64 == 0 || last_observation.timestamp == now
    {
        last_observation.cumulative_price_x64
    } else {
        let elapsed =
            u128::try_from(now - last_observation.timestamp).map_err(|_| error!(OracleError::MathOverflow))?;
        last_observation
            .cumulative_price_x64
            .checked_add(
                last_observation
                    .sqrt_price_x64
                    .checked_mul(elapsed)
                    .ok_or(OracleError::MathOverflow)?,
            )
            .ok_or(OracleError::MathOverflow)?
    };

    let new_index = (current_index + 1) % OBSERVATION_COUNT;
    state.observations[new_index] = Observation {
        timestamp: now,
        sqrt_price_x64,
        cumulative_price_x64: new_cumulative,
    };
    state.observation_index = new_index as u8;
    if state.observation_count < OBSERVATION_COUNT as u8 {
        state.observation_count += 1;
    }
    Ok(())
}

/// TWAP core shared by the `get_twap` handler and the circuit breaker.
///
/// `seconds_ago = 0` returns the latest known spot price.
fn get_twap_impl(oracle_state: &OracleState, seconds_ago: u32) -> Result<Option<u128>> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let target_timestamp = now.saturating_sub(seconds_ago as i64);

    let newer_obs = oracle_state.observations[oracle_state.observation_index as usize];
    let count = oracle_state.observation_count as usize;

    // The oldest observation still held: slot 0 until the buffer wraps, then
    // the slot right after the newest one. (Slot 0 is *not* the oldest once
    // the ring has wrapped — it was overwritten in the last cycle.)
    let oldest_idx = if count < OBSERVATION_COUNT {
        0
    } else {
        (oracle_state.observation_index as usize + 1) % OBSERVATION_COUNT
    };
    if count < 2 || oracle_state.observations[oldest_idx].timestamp > target_timestamp {
        // Not enough history: the buffer does not reach back to the target.
        return Ok(None);
    }

    // Walk backwards through the ring buffer to bracket the target:
    // `older_obs` is the closest observation at or before it, `upper_obs` the
    // closest one after it. (The oldest-slot check above guarantees the walk
    // finds an `older_obs`; `upper_obs` stays the newest if nothing sits
    // strictly between.)
    let mut older_obs = newer_obs;
    let mut upper_obs = newer_obs;
    for i in 1..count {
        let idx = (oracle_state.observation_index as usize + OBSERVATION_COUNT - i) % OBSERVATION_COUNT;
        let candidate = oracle_state.observations[idx];
        older_obs = candidate;
        if candidate.timestamp <= target_timestamp {
            break;
        }
        upper_obs = candidate;
    }

    if seconds_ago == 0 {
        // A spot request: the newest known price. `None` when no real price
        // has ever been recorded (genesis only).
        if newer_obs.sqrt_price_x64 == 0 {
            return Ok(None);
        }
        return Ok(Some(newer_obs.sqrt_price_x64));
    }

    // TWAP = (cumulative_now - cumulative_target) / (now - target)
    let window = now - target_timestamp;
    let newer_age = now - newer_obs.timestamp;

    // The newest observation's price has been live since its timestamp, so
    // the accumulator must be extrapolated forward to `now` before dividing.
    let cumulative_now = if newer_obs.sqrt_price_x64 == 0 {
        newer_obs.cumulative_price_x64
    } else {
        newer_obs
            .cumulative_price_x64
            .checked_add(
                newer_obs
                    .sqrt_price_x64
                    .checked_mul(u128::try_from(newer_age).unwrap_or(0))
                    .ok_or(OracleError::MathOverflow)?,
            )
            .ok_or(OracleError::MathOverflow)?
    };

    // The accumulator at the window boundary. Three cases:
    // * the boundary is at or after the newest observation -> the newest price
    //   has been live ever since, so extrapolate the accumulator forward to
    //   the boundary (skipping the stretch where the price was still unknown);
    // * the boundary is exactly an older observation       -> its cumulative;
    // * it falls strictly between two observations         -> interpolate.
    let cumulative_target = if target_timestamp >= newer_obs.timestamp {
        if newer_obs.sqrt_price_x64 == 0 {
            newer_obs.cumulative_price_x64
        } else {
            newer_obs
                .cumulative_price_x64
                .checked_add(
                    newer_obs
                        .sqrt_price_x64
                        .checked_mul(u128::try_from(
                            target_timestamp - newer_obs.timestamp,
                        )
                        .unwrap_or(0))
                        .ok_or(OracleError::MathOverflow)?,
                )
                .ok_or(OracleError::MathOverflow)?
        }
    } else if older_obs.timestamp == target_timestamp {
        older_obs.cumulative_price_x64
    } else {
        // Strictly between `older` and `upper`: here `older.timestamp < target
        // < upper.timestamp` by construction, so the span is positive unless
        // two observations share a second (degenerate; treat as at-`older`).
        let time_elapsed = upper_obs.timestamp - older_obs.timestamp;
        let into_window = target_timestamp - older_obs.timestamp;
        let delta = if time_elapsed <= 0 {
            0
        } else {
            upper_obs
                .cumulative_price_x64
                .saturating_sub(older_obs.cumulative_price_x64)
                .checked_mul(u128::try_from(into_window).unwrap_or(0))
                .ok_or(OracleError::MathOverflow)?
                .checked_div(u128::try_from(time_elapsed).unwrap_or(1))
                .unwrap_or(0)
        };
        older_obs
            .cumulative_price_x64
            .checked_add(delta)
            .unwrap_or(older_obs.cumulative_price_x64)
    };

    if cumulative_now <= cumulative_target {
        // A zero-length or all-unknown-price window: nothing to average.
        return Ok(None);
    }

    let twap = cumulative_now
        .saturating_sub(cumulative_target)
        .checked_div(u128::try_from(window).unwrap_or(1))
        .unwrap_or(0);

    if twap == 0 {
        Ok(None)
    } else {
        Ok(Some(twap))
    }
}

/// |a - b| for u128.
fn diff(a: u128, b: u128) -> u128 {
    if a > b {
        a - b
    } else {
        b - a
    }
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES
// -----------------------------------------------------------------------

#[account(zero_copy)]
#[repr(C, packed)]
pub struct OracleState {
    /// The pool PDA authorized to record observations.
    pub pool: Pubkey,
    /// Authority allowed to unpause after a circuit-breaker trip.
    pub owner: Pubkey,
    /// Set when the circuit breaker trips; blocks further deviation checks.
    /// A `u8` rather than `bool` because `bool` is not `Pod` (bytemuck refuses
    /// its invalid bit patterns); borsh-wise it is the same single byte.
    pub is_paused: u8,
    /// Ring-buffer cursor: index of the newest observation.
    pub observation_index: u8,
    /// How many slots of the ring buffer hold real observations (max 100).
    pub observation_count: u8,
    /// Cached PDA bump.
    pub bump: u8,
    /// Deviation threshold (sqrt-price bps) the breaker enforces; fixed at
    /// initialization.
    pub max_deviation_bps: u16,
    /// Historical observations, oldest first in insertion order.
    pub observations: [Observation; OBSERVATION_COUNT],
}

impl Default for OracleState {
    // A manual impl (tests only — `[Observation; 100]` has no derived `Default`).
    fn default() -> Self {
        Self {
            pool: Pubkey::default(),
            owner: Pubkey::default(),
            is_paused: 0,
            observation_index: 0,
            observation_count: 0,
            bump: 0,
            max_deviation_bps: 0,
            observations: [Observation::default(); OBSERVATION_COUNT],
        }
    }
}

impl OracleState {
    // 8 (discriminator) + pod size of the packed struct:
    // 32 (pool) + 32 (owner) + 1 (is_paused) + 1 (observation_index)
    // + 1 (observation_count) + 1 (bump) + 2 (max_deviation_bps)
    // + 100 * 40 (Observation) = 4 078.
    // Packed repr keeps the pod layout identical to the borsh layout and the
    // alignment at 1, which `AccountLoader`'s in-place byte mapping requires
    // (account data starts 8 bytes in — never 16-aligned).
    pub const LEN: usize = 8 + 32 + 32 + 1 + 1 + 1 + 1 + 2 + (40 * OBSERVATION_COUNT);
}

/// A single price observation. Zero-copy + packed: it lives inside the
/// pod-interpreted `OracleState`, where the memory layout must equal the
/// borsh layout (40 bytes, no padding).
#[zero_copy]
#[repr(C, packed)]
#[derive(Default)]
pub struct Observation {
    pub timestamp: i64,
    pub sqrt_price_x64: u128,
    pub cumulative_price_x64: u128,
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = payer,
        space = OracleState::LEN,
        seeds = [b"oracle", pool.key().as_ref()],
        bump
    )]
    pub oracle_state: AccountLoader<'info, OracleState>,

    /// The pool PDA this oracle records for. Must sign: only the AMM program
    /// that owns the pool can produce this signature, so a third party cannot
    /// create (and configure) the oracle under someone else's pool.
    pub pool: Signer<'info>,

    /// Pays for the oracle state. Separate from `pool`, which is a program
    /// account and cannot be a fee payer.
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RecordObservation<'info> {
    #[account(mut, seeds = [b"oracle", oracle_state.load().unwrap().pool.as_ref()], bump)]
    pub oracle_state: AccountLoader<'info, OracleState>,

    /// The pool PDA calling the oracle. Must be a signer.
    /// CHECK: verified as a signer and matched against `oracle_state.pool`.
    #[account(signer)]
    pub authority: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct GetTwap<'info> {
    #[account(seeds = [b"oracle", oracle_state.load().unwrap().pool.as_ref()], bump)]
    pub oracle_state: AccountLoader<'info, OracleState>,
}

#[derive(Accounts)]
pub struct CheckDeviation<'info> {
    #[account(seeds = [b"oracle", oracle_state.load().unwrap().pool.as_ref()], bump)]
    pub oracle_state: AccountLoader<'info, OracleState>,

    /// The pool PDA this oracle belongs to. Must sign — see
    /// `check_deviation`. The pool reads its own price from state and passes
    /// it, so the checked price is authenticated even though the pool's swap
    /// instruction stays permissionless.
    pub pool: Signer<'info>,
}

#[derive(Accounts)]
pub struct TripBreaker<'info> {
    #[account(mut, seeds = [b"oracle", oracle_state.load().unwrap().pool.as_ref()], bump)]
    pub oracle_state: AccountLoader<'info, OracleState>,

    /// The pool PDA. Must sign — see `initialize`. The pool instruction that
    /// reaches this CPI reads the pool's price from state and passes it along,
    /// so the price is authenticated even though this instruction is
    /// permissionless to trigger through the pool.
    pub pool: Signer<'info>,
}

#[derive(Accounts)]
pub struct UnpausePool<'info> {
    #[account(mut, seeds = [b"oracle", oracle_state.load().unwrap().pool.as_ref()], bump)]
    pub oracle_state: AccountLoader<'info, OracleState>,

    pub authority: Signer<'info>,
}

// -----------------------------------------------------------------------
// ERRORS
// -----------------------------------------------------------------------

#[error_code]
pub enum OracleError {
    #[msg("Unauthorized pool attempting to write to oracle")]
    UnauthorizedPool,
    #[msg("Unauthorized to unpause pool")]
    Unauthorized,
    #[msg("Pool is currently paused due to price deviation")]
    PoolPaused,
    #[msg("Price deviation exceeded maximum allowed threshold")]
    PriceDeviationExceeded,
    #[msg("Not enough observations to calculate TWAP")]
    NotEnoughObservations,
    #[msg("Observation for target timestamp not found")]
    ObservationNotFound,
    #[msg("Timestamp moved backwards")]
    TimestampMovedBackwards,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Deviation threshold must be greater than zero")]
    InvalidThreshold,
    #[msg("Price is within the deviation threshold; breaker did not trip")]
    PriceWithinThreshold,
    #[msg("The oracle owner cannot be the default pubkey")]
    InvalidOwner,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fill a state with one observation per second at a constant price.
    fn constant_price_state(price: u128, now: i64) -> OracleState {
        let mut state = OracleState::default();
        state.observation_count = 1;
        state.observation_index = 0;
        state.observations = [Observation::default(); OBSERVATION_COUNT];
        state.observations[0] = Observation {
            timestamp: now - 120,
            sqrt_price_x64: price,
            cumulative_price_x64: 0,
        };
        for t in (now - 119)..=now {
            write_observation(&mut state, t, price).unwrap();
        }
        state
    }

    #[test]
    fn ring_buffer_wraps_and_counts() {
        let mut state = OracleState::default();
        state.observation_count = 1;
        for t in 0..(OBSERVATION_COUNT as i64 + 10) {
            write_observation(&mut state, t, 1_000).unwrap();
        }
        assert_eq!(state.observation_count as usize, OBSERVATION_COUNT);
        // 110 writes after genesis: the cursor sits at 110 % 100.
        assert_eq!(state.observation_index as usize, 10);
        // The genesis slot was overwritten; slot 11 holds t = 10.
        // (Fields of packed structs are read by value; taking a reference,
        // as `assert_eq!` would, is a compile error.)
        let rewritten = state.observations[(10 + 1) % OBSERVATION_COUNT];
        let rewritten_ts = rewritten.timestamp;
        assert_eq!(rewritten_ts, 10);
    }

    #[test]
    fn constant_price_gives_constant_twap() {
        let now = 10_000_i64;
        let state = constant_price_state(2_000, now);
        // get_twap_impl reads Clock::get(); emulate by checking the accumulator
        // math directly: cumulative over the last 60s equals 60 * price.
        let newest = state.observations[state.observation_index as usize];
        let oldest = state.observations[0];
        let elapsed = newest.timestamp - oldest.timestamp;
        // TWAP between two snapshots = Δcumulative / Δtime = the constant price.
        let avg = (newest.cumulative_price_x64 - oldest.cumulative_price_x64)
            / elapsed as u128;
        assert_eq!(avg, 2_000);
    }

    #[test]
    fn changing_price_averages_correctly() {
        let mut state = OracleState::default();
        state.observation_count = 1;
        state.observations = [Observation::default(); OBSERVATION_COUNT];
        // 100 seconds at price 1_000, then 100 seconds at price 3_000.
        state.observations[0] = Observation {
            timestamp: 0,
            sqrt_price_x64: 1_000,
            cumulative_price_x64: 0,
        };
        for t in 1..=100 {
            write_observation(&mut state, t, 1_000).unwrap();
        }
        for t in 101..=200 {
            write_observation(&mut state, t, 3_000).unwrap();
        }
        // After 200 writes the cursor wrapped to slot 0: newest is t = 200,
        // and slot 1 holds t = 101. The window [101, 200] is all-3000.
        let newest = state.observations[state.observation_index as usize];
        let older = state.observations[1];
        let (newest_ts, newest_cum) = (newest.timestamp, newest.cumulative_price_x64);
        let (older_ts, older_cum) = (older.timestamp, older.cumulative_price_x64);
        assert_eq!(newest_ts, 200);
        let avg = (newest_cum - older_cum) / (newest_ts - older_ts) as u128;
        assert_eq!(avg, 3_000);
        // Full-history average carries the extra tick the old price stayed
        // live (the accumulator is genesis-based): 398_000 / 200 = 1990.
        assert_eq!(newest_cum / newest_ts as u128, 1_990);
    }

    #[test]
    fn deviation_math_is_symmetric() {
        assert_eq!(diff(120, 100), 20);
        assert_eq!(diff(100, 120), 20);
        // 20 over 100 in bps is 2000.
        assert_eq!(diff(120, 100) * 10_000 / 100, 2_000);
    }
}
