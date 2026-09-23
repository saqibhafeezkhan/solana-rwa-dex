# TWAP Oracle & Circuit Breaker Program

An Anchor-based Solana program that acts as the **price safety layer** for RWA pools: it accumulates a time-weighted average price (TWAP) from observations recorded by the pool itself, and exposes a **circuit breaker** that reverts any trade whose price deviates too far from that TWAP.

Program ID: `4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj`

---

## 🌟 Overview

Real-world assets are not memecoins. A tokenized T-bill or fund share should track its NAV — a sudden 40% "price" is almost always a flash loan, a thin book, or a broken quote, never a real market. RWA pools therefore refuse to price trades off spot alone. The pattern this program implements:

1. **The pool records its own prices.** After every swap (or tick change), the pool PDA calls `record_observation` with its new `sqrt_price_x64`. The oracle is passive storage: only the authorized pool can write.
2. **The oracle accumulates time under price.** Every observation extends a cumulative price accumulator: `cumulative += last_price × elapsed_seconds`. This is the same design Uniswap v3 uses for its oracle — one accumulator, any window.
3. **TWAP over any window is one division.** For a window `[t₀, now]`: `TWAP = (cumulative_now − cumulative_t₀) / (now − t₀)`, interpolating the accumulator at `t₀` if it falls between two observations.
4. **The circuit breaker compares spot to TWAP.** `check_deviation` — which only the pool that owns the oracle may call — computes `|spot − TWAP| / TWAP` in basis points and reverts the transaction when it exceeds the stored threshold; the pool checks this before executing a swap. Latching the pause is a separate instruction: a reverted transaction cannot leave state behind, so the permissionless `trip_breaker` evaluates the same math and, when the price is out of line, sets `is_paused = 1` in a transaction that *succeeds*.

Because the accumulator is genesis-based (it only ever grows), a TWAP over a longer window is always available as long as the ring buffer still holds the boundary.

### Key Characteristics:

* **Pool-authoritative writes.** `record_observation` requires the pool PDA itself as a `Signer` and verifies it matches `oracle_state.pool`. Nobody else — not the admin — can write a price.
* **PDA-derived state, one per pool.** The oracle account is `[b"oracle", pool_key]`, so it is always discoverable from the pool and cannot be swapped for a look-alike account. Every instruction that takes a price from its caller — `initialize`, `check_deviation`, `trip_breaker` — additionally requires the pool PDA itself as a `Signer`, and every one of them verifies it against `oracle_state.pool`. `record_observation` does the same, which is why nothing in this program trusts a price a stranger supplied.
* **100-observation ring buffer.** `OBSERVATION_COUNT = 100` slots wrap around; the oldest observation is overwritten once the buffer is full. 100 updates is ample for a 30-minute breaker window at typical swap frequency; pools that swap every slot for 30 minutes straight would want to raise the count.
* **Genesis-based accumulator with extrapolation.** The newest observation's price is extrapolated forward to `now` (`cumulative_newest + price × (now − newest_ts)`) before the window division, so a TWAP stays correct even when the last swap was long ago.
* **Interpolation at the window boundary.** If `now − seconds_ago` falls strictly between two stored observations, the accumulator is linearly interpolated at the boundary, exactly as Uniswap's `observe` does.
* **Graceful cold start.** With no history, the breaker does not block trading forever: until the window is covered *and* a non-zero price exists, `check_deviation` skips the comparison and logs why. The genesis observation records `sqrt_price = 0` ("price not yet known"), and the accumulator only advances while a known price is live.
* **Timestamps, not slots.** Everything is measured in `Clock::unix_timestamp` seconds, matching the TWAMM program's convention — slots are not evenly spaced in wall-clock time.
* **Revert to block, latch to pause.** `check_deviation` is a pure guard: it reverts the swap it guards and writes nothing. `trip_breaker` is what persists the pause — anyone may trigger it *through the pool* (the pool PDA must sign and supplies its own price, so a forged price cannot pause a healthy pool), it only ever pauses (never unpauses), it will not pause a price inside the threshold, and it is a no-op when no comparable history exists. Only the oracle `owner` (named by the pool at initialization) can `unpause_pool`. There is no admin bypass of the pool-signer rule.
* **Created atomically with the pool.** `initialize` requires the pool PDA's signature, so the AMM creates (and configures) the oracle inside `initialize_pool`. No third party can front-run the pool and claim its `[b"oracle", pool]` key with a hostile threshold or owner.

---

## 📐 Mathematical Model

Notation: `Sᵢ` is the spot price (`sqrt_price_x64`) written by observation `i` at time `tᵢ`. All prices are fixed-point sqrt-price ×2⁶⁴ as used by the CLMM.

### 1. The accumulator

Each new observation at time `t` extends the accumulator:

```
Δt         = t − t_last
cumulative = cumulative_last + S_last × Δt        // only while S_last > 0
```

`cumulative` has units of **price × seconds** (×2⁶⁴). It is the integral of price over time: exactly what a time-weighted average divides by the elapsed seconds.

### 2. TWAP over a window

For a window ending at `now` and starting `seconds_ago` earlier, with boundary `t₀ = now − seconds_ago`:

```
cumulative_t₀ = interpolate(cumulative, t₀)       // between the two observations bracketing t₀
cumulative_now = cumulative_newest + S_newest × (now − t_newest)

TWAP = (cumulative_now − cumulative_t₀) / (now − t₀)
```

Interpolation at `t₀` (between older `a` and newer `b`):

```
cumulative_t₀ = cumulative_a + (cumulative_b − cumulative_a) × (t₀ − t_a) / (t_b − t_a)
```

All divisions floor; every step uses checked arithmetic, saturating to the previous accumulator value rather than panicking. Because the accumulator is monotonic, `cumulative_now ≥ cumulative_t₀` whenever the window is covered.

Special cases:

| Case | Result |
| :--- | :--- |
| `seconds_ago = 0` | Latest spot price (no division, no interpolation). |
| Boundary lands exactly on an observation | That observation's stored data; no interpolation. |
| Buffer does not reach back to `t₀` | `None` — the window is not covered yet. |
| Fewer than 2 observations, or no non-zero price in the window | `None`. |

### 3. The circuit breaker

```
deviation_bps = |S_spot − TWAP| × 10 000 / TWAP
```

`S_spot` is the price the pool is about to trade at. `max_deviation_bps` is fixed once at `initialize` (the pool's policy), e.g. `500` = 5%.

Two instructions evaluate it:

* **`check_deviation(spot)`** — the guard the pool calls before a swap, signed by the pool (`UnauthorizedPool` for anyone else). Above the threshold it returns `PriceDeviationExceeded`, which reverts the swap. It writes nothing: the runtime discards the writes of a reverted instruction, so it deliberately does not try to latch `is_paused` itself.
* **`trip_breaker(spot)`** — the permissionless latch. It computes the same deviation and, when it exceeds the threshold, sets `is_paused = 1` and **succeeds**, which is what makes the pause survive. A price inside the threshold is an error (`PriceWithinThreshold`) so a keeper cannot pause a sane pool, and a pool with no comparable history is a no-op.

Note the prices are **sqrt-prices**: a 1% price move in true price `P` is a 0.5% move in `√P`, so a threshold of `500 bps` on sqrt-price ≈ `10%` on true price for small deviations. Pools wanting a true-price threshold `T` should pass `T / 2` in sqrt-bps.

### Worked example

Observations (one per second, accumulator genesis-based):

| `t` | `S` | `cumulative` after this observation |
| :--- | :--- | :--- |
| 0 (genesis) | 0 | 0 |
| 1 | 2.0 | 0 (genesis price unknown, nothing accrued) |
| 2 | 2.0 | 2.0 × 1 = 2.0 |
| 3 | 2.0 | 4.0 |

At `t = 3`, TWAP over the last 2 seconds: boundary `t₀ = 1` lands on an observation, `cumulative_now = 4.0 + 2.0 × 0 = 4.0`, `cumulative_t₀ = 0` → `TWAP = 4.0 / 2 = 2.0`. ✔

Now suppose a flash loan pumps the pool to `S = 4.0` at `t = 4`. `cumulative_now = 4.0 + 2.0 × 1 = 6.0`. TWAP over 30 min boundary (say `t₀ = 1`): `(6.0 − 0) / 3 = 2.0`. Deviation: `|4.0 − 2.0| × 10 000 / 2.0 = 10 000 bps = 100%` — far above any sane threshold. `check_deviation` reverts that swap; `trip_breaker` then latches `is_paused = 1` in its own successful transaction, and the pool stays paused until the authority investigates and calls `unpause_pool`.

---

## ⚙️ Program Constants

| Constant | Value | Purpose |
| :--- | :--- | :--- |
| `OBSERVATION_COUNT` | `100` | Ring-buffer slots. Each holds one `Observation` (40 bytes). |
| `DEVIATION_WINDOW_SECONDS` | `1800` | TWAP window used internally by `check_deviation` (30 minutes). |
| `BPS_DENOMINATOR` | `10_000` | Basis-point denominator for the deviation math. |
| Oracle PDA seed | `b"oracle"` | `[b"oracle", pool_key]` → the per-pool oracle state. |

---

## 🔑 Account Architecture & PDAs

```
   pool PDA (signer)                oracle state PDA
  ┌──────────────────┐   record    ┌─────────────────────────────┐
  │  clmm / twamm    │ ──────────▶ │ [b"oracle", pool_key]       │
  │  calls after     │             │  pool, owner, is_paused     │
  │  every swap      │             │  ring buffer of 100         │
  └──────────────────┘             │  Observation { ts, S, cum } │
           │                       └─────────────────────────────┘
           │  check_deviation(spot) before a swap
           └──────────────▶ deviates ⟹ revert (no state survives)
           │  trip_breaker — via the pool (pool signs, price read from state)
           └──────────────▶ deviates ⟹ is_paused = 1
                          ▲
   authority (owner) ─────┘  unpause_pool after investigation
```

### PDA Seeds Table

| Account | Seeds | Notes |
| :--- | :--- | :--- |
| `OracleState` | `[b"oracle", pool_key]` | One per pool. `pool_key` is the pool's own PDA address. |

### Account layout

`OracleState` is **4 078 bytes** including the 8-byte discriminator:

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `pool` | `Pubkey` | The only PDA allowed to record observations. |
| `owner` | `Pubkey` | Authority that may `unpause_pool`. |
| `is_paused` | `u8` | Circuit-breaker state (`0`/`1`); blocks further checks when tripped. A `u8`, not a `bool`, because `bool` is not `Pod`. |
| `observation_index` | `u8` | Ring-buffer cursor: slot of the newest observation. |
| `observation_count` | `u8` | Real observations held so far (caps at 100). |
| `bump` | `u8` | Cached PDA bump. |
| `max_deviation_bps` | `u16` | Deviation threshold the breaker enforces (sqrt-price bps); fixed at initialization. |
| `observations` | `[Observation; 100]` | The ring buffer. |

`Observation` is 40 bytes:

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `timestamp` | `i64` | Unix seconds when the price was recorded. |
| `sqrt_price_x64` | `u128` | Spot sqrt-price ×2⁶⁴ at that moment (`0` = unknown, genesis only). |
| `cumulative_price_x64` | `u128` | Genesis-based accumulator: ∫ price dt, in price-seconds ×2⁶⁴. |

---

## 🛠️ Instructions API

### 1. `initialize`

```rust
pub fn initialize(ctx: Context<Initialize>, max_deviation_bps: u16, owner: Pubkey) -> Result<()>
```

Creates the `OracleState` PDA for `pool_key`, stamps the genesis observation at `now` with `sqrt_price = 0`, records `owner` as the address allowed to `unpause_pool`, and fixes the breaker's threshold `max_deviation_bps` (non-zero — `0` is rejected with `InvalidThreshold`; a default `owner` is rejected with `InvalidOwner`).

**The pool PDA must sign.** The AMM that owns the pool creates the oracle by CPI from `initialize_pool`, with the pool signing, so the oracle is configured atomically with the pool and cannot be front-run: there is no window in which a third party can claim the oracle key and set the threshold. `payer` (separate from `pool`, since a program account cannot be a fee payer) funds the rent.

### 2. `record_observation`

```rust
pub fn record_observation(ctx: Context<RecordObservation>, sqrt_price_x64: u128) -> Result<()>
```

Appends one observation: advances the accumulator by `S_last × Δt`, writes the new price at `index + 1 (mod 100)`, and bumps the cursor/count. Only the pool PDA (signer, and equal to `oracle_state.pool`) may call it. Refuses timestamps moving backwards (`TimestampMovedBackwards`) — the accumulator's correctness depends on monotonic time.

### 3. `get_twap`

```rust
pub fn get_twap(ctx: Context<GetTwap>, seconds_ago: u32) -> Result<Option<u128>>
```

Read-only view for clients and the pool: the TWAP over `[now − seconds_ago, now]`, or `None` when the window is not covered or no price has been recorded yet. `seconds_ago = 0` is the latest spot price.

### 4. `check_deviation`

```rust
pub fn check_deviation(ctx: Context<CheckDeviation>, current_sqrt_price_x64: u128) -> Result<()>
```

The breaker guard, called by the pool **before** executing a swap. Compares the incoming spot price to the 30-minute TWAP (`DEVIATION_WINDOW_SECONDS`) and reverts with `PriceDeviationExceeded` when the deviation exceeds the stored `max_deviation_bps`. Always refused while already paused (`PoolPaused`). Skips the check — without pausing — when the history is not yet meaningful, and logs the reason. Its account is read-only: this instruction reverts, it never pauses.

`pool` must sign and equal `oracle_state.pool` (`UnauthorizedPool` otherwise). The price is an argument, and the caller is the one gating its own swap, so a check that any third party could run would be an answer about a price no pool ever had — and the pool is the only party holding the price in its own state. The AMMs build this CPI with the pool's own seeds, exactly as they do for `record_observation`.

### 5. `trip_breaker`

```rust
pub fn trip_breaker(ctx: Context<TripBreaker>, current_sqrt_price_x64: u128) -> Result<()>
```

The latch. **Anyone may trigger it, but only through the pool**: `pool` must sign and equal `oracle_state.pool`, and the AMM builds the CPI itself, reading `current_sqrt_price_x64` from the pool's own state — never from a caller argument. That is what stops an attacker from pausing a healthy pool with a forged price. When the pool's price exceeds the threshold it sets `is_paused = 1` in a transaction that **succeeds** — the part a revert cannot do. Returns `PriceWithinThreshold` when the price is inside the threshold, `PoolPaused` when already paused, and is a silent no-op when there is no comparable history. It can only pause, never unpause.

### 6. `unpause_pool`

```rust
pub fn unpause_pool(ctx: Context<UnpausePool>) -> Result<()>
```

Owner-only reset of `is_paused` after a trip. Deliberately manual: a trip means something anomalous happened, and a human (or governance multisig) should confirm the pool is healthy before trading resumes. Consider assigning `owner` to a multisig.

---

## 🔌 TypeScript Integration

Snippets use `@coral-xyz/anchor` with the IDL `anchor build` writes to `target/types/oracle.ts`, plus `@solana/web3.js`.

### PDAs and helpers

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import { Oracle } from "./target/types/oracle";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Oracle as Program<Oracle>;
const programId = program.programId;

const oraclePda = (poolKey: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("oracle"), poolKey.toBuffer()],
    programId
  )[0];

/** 2^64, the Q64.64 unit, for turning fixed-point values into UI numbers. */
const Q64 = 2n ** 64n;
const q64ToNumber = (v: bigint | number) => Number(v) / Number(Q64);

/** Allowed deviation in sqrt-price bps: 488 ≈ 10% on true price. */
const MAX_DEVIATION_BPS = 488;
```

### Initialize and record

```typescript
// The pool program calls this via CPI from `initialize_pool`, with the pool PDA
// signing; it cannot be called for someone else's pool from a wallet.
async function initializeOracle(poolKey: PublicKey, payer: PublicKey, owner: PublicKey) {
  await program.methods
    .initialize(MAX_DEVIATION_BPS, owner) // threshold + unpause owner, both fixed at init
    .accounts({
      oracleState: oraclePda(poolKey),
      pool: poolKey, // the pool PDA, which must sign
      payer,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();
}

// The pool program calls this via CPI after each swap; shown here for testing.
async function recordObservation(poolKey: PublicKey, sqrtPriceX64: bigint) {
  await program.methods
    .recordObservation(new anchor.BN(sqrtPriceX64.toString()))
    .accounts({
      oracleState: oraclePda(poolKey),
      authority: poolKey, // the pool PDA itself must sign
    })
    .signers([]) // signing is delegated to the pool PDA via CPI
    .rpc();
}
```

### Read the TWAP from your client

`get_twap` is available on-chain, but clients usually want the same number for display or pre-trade checks. Reimplementing the accumulator client-side is straightforward because the state is self-describing:

```typescript
async function twap(poolKey: PublicKey, secondsAgo: number): Promise<bigint | null> {
  const o = await program.account.oracleState.fetch(oraclePda(poolKey));
  const obs = o.observations as { timestamp: anchor.BN; sqrtPriceX64: string; cumulativePriceX64: string }[];

  const now = Math.floor(Date.now() / 1000);
  const target = now - secondsAgo;
  const newest = obs[o.observationIndex as number];
  const newestTs = newest.timestamp.toNumber();
  if (o.observationCount < 2) return null;

  // Extrapolate the newest price forward to `now`.
  const newestS = BigInt(newest.sqrtPriceX64);
  const newestCum = BigInt(newest.cumulativePriceX64);
  const cumNow = newestCum + newestS * BigInt(now - newestTs);

  // Walk backwards for the observation that brackets `target`.
  const count = o.observationCount as number;
  let older = newest;
  for (let i = 1; i < count; i++) {
    const idx = ((o.observationIndex as number) + 100 - i) % 100;
    older = obs[idx];
    if (older.timestamp.toNumber() <= target) break;
  }
  const olderTs = older.timestamp.toNumber();
  if (olderTs > target) return null; // window not covered

  let cumTarget = BigInt(older.cumulativePriceX64);
  if (olderTs !== target) {
    // Interpolate between `older` and the observation right after it.
    const newerIdx = (obs.indexOf(older) + 1) % 100;
    const newer = obs[newerIdx];
    const span = newer.timestamp.toNumber() - olderTs;
    const into = target - olderTs;
    cumTarget +=
      (BigInt(newer.cumulativePriceX64) - BigInt(older.cumulativePriceX64)) *
      BigInt(into) / BigInt(span);
  }

  const twapRaw = (cumNow - cumTarget) / BigInt(now - target);
  return twapRaw === 0n ? null : twapRaw;
}
```

### Pre-trade safety check from a pool

The intended CPI pattern, in three lines of pool-side logic:

```rust
// In the pool program, before executing a swap at `new_sqrt_price`:
let cpi_accounts = oracle::cpi::accounts::CheckDeviation {
    oracle_state: ctx.accounts.oracle_state.to_account_info(),
    pool: ctx.accounts.pool.to_account_info(), // the pool: it must sign
};
let signer_seeds: &[&[&[u8]]] = &[&pool_seeds]; // [b"clmm-pool", mint_a, mint_b, bump]
let cpi_ctx = CpiContext::new(ctx.accounts.oracle_program.to_account_info(), cpi_accounts)
    .with_signer(signer_seeds);
oracle::cpi::check_deviation(cpi_ctx, new_sqrt_price)?;
```

The guard's accounts are read-only, and the threshold is not a parameter — it was fixed at `initialize` and read from the state. Because the price is an argument, the pool must sign: the oracle refuses the call (`UnauthorizedPool`) unless the signer is the pool recorded in its state. Latching the pause is a separate, permissionless `trip_breaker` call (its account *is* writable); `check_deviation` alone can never leave `is_paused` set.

### Integration pitfalls

1. **Only the pool signs `record_observation`.** A wallet cannot record prices directly, by design — for testing, fund a PDA and sign on its behalf, or CPI from the real pool program.
2. **Two big u128s per observation.** The 100-slot buffer costs ~4 KB of rent. That is cheap, but a pool recording on *every slot* for hours will wrap the buffer — that is fine (old data is overwritten), just remember the max TWAP window is bounded by what the buffer still holds.
3. **Timestamps must be monotonic.** `record_observation` refuses time going backwards. In practice validator clock skew within a slot can produce equal timestamps — handled (`Δt = 0` accrues nothing) — but negative deltas revert.
4. **Sqrt-price semantics.** All prices are `sqrt_price_x64`. Halve your true-price deviation threshold when passing `max_deviation_bps`.
5. **`get_twap` returns `Option<u128>`.** `None` means "not enough history", not "price is zero". The breaker treats it as *allow* with a log; a stricter pool may choose to treat cold-start swaps differently.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `UnauthorizedPool` (6000) | `Unauthorized pool attempting to write to oracle` | `record_observation`, `check_deviation` or `trip_breaker` signer is not the pool recorded in the oracle state. |
| `Unauthorized` (6001) | `Unauthorized to unpause pool` | `unpause_pool` signer is not the oracle `owner`. |
| `PoolPaused` (6002) | `Pool is currently paused due to price deviation` | Breaker already tripped; checks and further trips are refused until unpause. |
| `PriceDeviationExceeded` (6003) | `Price deviation exceeded maximum allowed threshold` | `check_deviation` saw spot vs TWAP outside `max_deviation_bps` and reverted. |
| `NotEnoughObservations` (6004) | `Not enough observations to calculate TWAP` | Declared but never returned: `get_twap` answers `None` instead. |
| `ObservationNotFound` (6005) | `Observation for target timestamp not found` | Declared but never returned: an unbracketed window also answers `None`. |
| `TimestampMovedBackwards` (6006) | `Timestamp moved backwards` | A recording attempted to rewind the clock. |
| `MathOverflow` (6007) | `Math overflow` | Checked arithmetic overflowed. |
| `InvalidThreshold` (6008) | `Deviation threshold must be greater than zero` | `initialize` was called with `max_deviation_bps = 0`. |
| `PriceWithinThreshold` (6009) | `Price is within the deviation threshold; breaker did not trip` | `trip_breaker` called with a well-behaved price; nothing is paused. |
| `InvalidOwner` (6010) | `The oracle owner cannot be the default pubkey` | `initialize` was called with `owner = Pubkey::default()`: the pool could never unpause. |

---

## 🧪 Testing & Verification

The crate carries inline unit tests over the pure accumulator/ring-buffer logic (no validator needed):

```bash
cargo test -p oracle
```

They pin:

* **Ring-buffer integrity** — 110 writes into 100 slots wrap the cursor to `110 % 100`, cap `observation_count` at 100, and overwrite exactly the oldest slot.
* **Constant price in, constant TWAP out** — a price of 2.0 held over a window yields Δcumulative/Δtime = 2.0 exactly.
* **Step prices average by duration** — 100 s at 1 000 then 100 s at 3 000 gives a 3 000 average over the second window and a genesis-based average of 1 990 over the whole 200 s (the extra tick the first price stayed live is visible and correct).

Build the IDL exactly as `anchor build` does:

```bash
cargo check -p oracle --features idl-build
```

The PDA-signer path and the breaker's trip/pause/unpause lifecycle need a VM with the compiled program and a controllable `Clock`. `tests/e2e` is a Cargo workspace of its own that loads `target/deploy/oracle.so` into a LiteSVM and drives real, signed transactions:

```bash
anchor build -p oracle && (cd tests/e2e && cargo test --test oracle)
```

That suite pins the genesis state and PDA layout byte-for-byte, the accumulator and the 100-slot ring wrap, TWAP values only real Borsh serialization can confirm, the `Option<u128>` return data, the pool-signer rule on `initialize`, `record_observation` and `check_deviation` (an impostor that signs but is not the pool is refused with `UnauthorizedPool`), rejected backwards timestamps, and the whole `check_deviation` → `trip_breaker` → `unpause_pool` lifecycle.

---

## ⚠️ Design Notes & Limits

1. **The oracle never prices anything by itself.** It stores what the pool tells it and blocks trades that look wrong. Price discovery stays with the CLMM/DLMM/TWAMM pools; arbitrage keeps those honest between observations.
2. **100 observations bound the usable window.** A pool swapping constantly for longer than ~100 updates cannot serve a 30-minute TWAP from history; raise `OBSERVATION_COUNT` (and the account size) if your update rate demands it.
3. **A trip is a halt, not a rebalance.** Nothing stabilizes the price while paused — the breaker exists to give humans time to look, not to defend the price automatically. `unpause_pool` should follow an investigation, ideally by a multisig.
4. **The breaker trusts the pool's spot input.** `check_deviation` takes the incoming price as an argument from the calling pool; a buggy pool could pass the TWAP itself and never trip. Integration should pass the same price the swap will actually execute at.
5. **No fee, no token, no admin upgrade path inside the program.** The owner can only unpause; writes are PDA-gated; there is no mint authority and no fund movement of any kind.
