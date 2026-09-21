# Concentrated Liquidity Market Maker (CLMM) Program

An Anchor-based Solana program implementing a **Uniswap v3-style Concentrated Liquidity Market Maker** with a continuous Q64.64 price curve, capital-efficient tick ranges, Uniswap-v3-exact fee accounting, and Token-2022-ready SPL token transfers.

Program ID: `HLMAwimHtgMV8ME9X4RaoHCAdfU35hrovEg4AU5ifw7u`

---

## 🌟 Overview

Instead of spreading liquidity uniformly across the entire price line (Uniswap v2 / constant-product), the CLMM lets each LP pin their capital to an arbitrary **tick range** `[lower_tick, upper_tick]`. Trades move a single pool-wide price along the curve; a position participates in a trade exactly while the price stays inside its range.

### Key Characteristics:

* **Continuous price curve.** Price is `token B / token A`, stored as `sqrt_price_x64` in Q64.64 fixed point. Every swap step is exact Q64.64 math from the shared `math_core` crate (bit-decomposed `1.0001^(t/2)` tick ratios, U256 intermediates, explicit asymmetric rounding).
* **Concentrated positions.** A position of liquidity `L` on `[lower, upper]` holds only token A when the price is below the range, only token B when above, and a shifting mix while the price is inside. Capital efficiency scales with `1 / (sqrt_upper − sqrt_lower)`.
* **Uniswap-v3-exact fee accounting.** The fee is charged on the input each swap step **actually consumed** — `ceil(in · f / (1 − f))` — so a multi-tick trade pays for exactly what executed, and fees accrue per unit of active liquidity through the `fee_growth_global` accumulators.
* **Bitmap-verified swap walk.** A swap passes the tick PDAs it may cross as **remaining accounts**, in walk order, and the program checks that order against the pool's **tick bitmap** instead of trusting it: each supplied account must be the very next initialized tick the price reaches. A caller cannot skip a tick to stop `pool.liquidity` from changing — that is the one piece of pool state a trade cannot recompute, and from then on every price would be quoted against the wrong active set.
* **Partial fills, never reverts on depth.** If the supplied ticks cannot absorb the whole input, the trade settles at the last reachable price: only the consumed input is pulled from the user and the rest stays in their wallet.
* **Rounding in the pool's favour.** Deposits round the amounts *up* (user pays), withdrawals and swap outputs round *down* (pool pays). The test suite asserts the walk's budget identity `in + fee ≤ offered` for every input from 0 to 5 000 across seven fee tiers.
* **Claim-style fee collection.** Fees accrue into `tokens_owed` on every position touch (`add_liquidity`, `remove_liquidity`, `collect_fees`) and are pulled on demand — no fee dust is ever minted implicitly.

---

## 📐 Mathematical Model

### 1. Price and ticks

A tick `t` is the price `1.0001^t`; the pool stores `sqrt(price) = 1.0001^(t/2)`:

$$S(t) = 1.0001^{t/2} \times 2^{64} \qquad\qquad P_{raw} = \left(\frac{S}{2^{64}}\right)^2 = \left(\frac{\text{amount B}}{\text{amount A}}\right)$$

`S` lives in `[MIN_SQRT_PRICE, MAX_SQRT_PRICE]` (ticks `−443 636 … 443 636`, price ratios of roughly `2^-64 … 2^64`). Conversions are computed by `math_core::sqrt_price_at_tick` / `tick_at_sqrt_price`, which are exact inverses of each other.

### 2. Position token amounts

For liquidity `L` on `[S_l, S_u]` at current price `S` (A = token0, B = token1):

$$
(\text{amount}_A, \text{amount}_B) = \begin{cases}
\left(L\left(\frac{1}{S_l} - \frac{1}{S_u}\right)2^{64},\; 0\right) & S \le S_l \quad\text{(price below: A only)}\\[6pt]
\left(L\left(\frac{1}{S_l} - \frac{1}{S}\right)2^{64},\; L\,(S_u - S)/2^{64}\right) & S_l < S < S_u \quad\text{(both legs split at S)}\\[6pt]
\left(0,\; L\,(S_u - S_l)/2^{64}\right) & S \ge S_u \quad\text{(price above: B only)}
\end{cases}
$$

Deposits round each leg **up**, withdrawals **down** (`math_core::get_amount_0_delta` / `get_amount_1_delta` with the explicit `round_up` flag).

### 3. Swap step

One step of the walk trades within the active liquidity `L` toward the next tick's `S_target`:

* **Direction** is implied: `S_target < S` means token A is sold (price falls), otherwise token B.
* The step either reaches `S_target` or consumes the remaining (net-of-fee) input, whichever comes first.
* Reaching the target takes `get_amount_0_delta(S_target, S, L, ceil)` (A in) or `get_amount_1_delta(S, S_target, L, ceil)` (B in); a partial move prices the offered amount directly.
* Output is computed over the realised price move: `get_amount_1_delta` (A in → B out) or `get_amount_0_delta` (B in → A out), floored.

### 4. Fee on the consumed input

`math_core::compute_swap_step` reports `fee_amount` on the whole *offered* amount, which is only correct for a single-shot swap. For the walk the fee is recomputed on the input the step actually consumed:

$$fee = \left\lceil \frac{in \times f}{1\,000\,000 - f} \right\rceil$$

Because the price-move budget is `offered · (1 − f)`, the identity `in + fee ≤ offered` always holds — the walk can never overspend its budget. (The tests verify this exactly over `offered ∈ [1, 5000]` and `f ∈ {0, 1, 3, 100, 3000, 100000, 500000}`.)

### 5. Fee growth accounting (Uniswap v3 scheme)

Each pool keeps per-token accumulators, in Q64.64 "growth per unit of liquidity":

$$G_{token} \mathrel{+}= \frac{fee_{token} \times 2^{64}}{L_{active}}$$

Each tick keeps a mirror pair `fee_growth_outside_{a,b}`. The growth earned **inside** a range is a difference of three numbers (arithmetic wraps mod 2^128 by convention, which keeps every expression well-defined):

$$G_{inside} = G_{global} - G_{below(lower)} - G_{above(upper)}$$

where each term reads the tick's `outside` or its complement depending on which side of the tick the current price is on. A position's fees are then:

$$fees_{owed} = \left\lfloor \frac{L_{position} \times (G_{inside}^{now} - G_{inside}^{snapshot})}{2^{64}} \right\rfloor$$

The snapshot re-bases on every touch; `L_position × Δ` cannot overflow `u128` because `Δ` is bounded by `(fees paid · 2^64) / L_active` with `fees` being `u64` token amounts.

### 6. Tick initialization and crossing

When a tick PDA is first initialized (`add_liquidity`), its `outside` mirrors start at the **full global accumulator if the tick is at or below the current price, else 0** — the Uniswap v3 convention that all growth so far happened "below" a fresh tick. Crossing a tick flips the mirror and moves the range in or out of the active set:

$$outside' = global - outside \qquad\qquad L' = \begin{cases} L - liquidity\_net & \text{crossing down (A→B)}\\ L + liquidity\_net & \text{crossing up (B→A)}\end{cases}$$

`tick_current` follows the direction of travel, exactly as Uniswap v3 records it: an up-cross stores the tick itself, a down-cross stores `tick_index − 1`. That asymmetry is what keeps a position whose bound sits exactly on the current price on the correct side of it — live when the price walks up to the bound, out of range when it walks down through it. So a price resting exactly on a boundary is **crossed again in the direction of travel** as a zero-width step that consumes no input, and a step that stops strictly inside a range recomputes `tick_current` from the new price instead of leaving it on the old tick.

Each tick also owns one bit in the pool's **tick bitmap**: set when the tick is first initialized, cleared once its last position leaves (`liquidity_gross == 0` and `liquidity_net == 0`). The bitmap is the pool's own record of which ticks exist at all, and it is what a swap's walk is validated against.

### Worked example: a swap that crosses one tick

Pool at price 1.0 (tick 0), `fee_pips = 3 000` (0.3%), active liquidity `L = 5 000 000` supplied by a position on `[-600, +600]`, and a second position on `[-1 200, -600]` below. A trader sells **1 000 000 raw A** with ticks `[−600]` supplied.

1. **Step 1** (target `S(−600)`, price falls): the input needed to reach the target is `L · (1.0001^300 − 1) ≈ 152 265` net; the fee on it is `ceil(152 265 × 3 000 / 997 000) = 459`. Consumed: 152 265 + 459 = 152 724, leaving 847 276. Output: `L · (1 − 1/1.0001^300) ≈ 147 771` B. The price now sits exactly on tick −600.
2. **Cross −600**: `outside` mirrors flip, `L' = L − liquidity_net(−600) = L − 5 000 000 = 0`, `tick_current = −600`.
3. **No more liquidity**: any further supplied tick would be traded against an empty active set, so the walk stops.
4. **Settle**: the pool pulls 152 724 A from the user — not the offered 1 000 000 — and pays out ≈147 771 B.

Had the user supplied tick `−1 200` as well, the walk would have continued through the second position's range in the same transaction (and the fee on that step would be computed on the input *that* step consumed, not on the whole trade).

---

## ⚙️ Program Constants

| Parameter | Value | Description |
| :--- | :--- | :--- |
| `MAX_FEE_PIPS` | `999 999` | Maximum pool fee in pips; `math_core` rejects anything ≥ 1 000 000 (100%). |
| `FEE_DENOMINATOR` | `1 000 000` | Fee denominator (pips, millionths) shared with `math_core`. |
| Tick range | `−443 636 … 443 636` | Usable tick range (`math_core::MIN_TICK`/`MAX_TICK`), price ratio ≈ `2^±64`. |
| `BITMAP_BITS_MAX` | `65 536` | Upper bound on a pool's tick bitmap, which keeps a pool (and its bitmap) inside one account: `create_account` caps a single account at 10 KiB. Across the full tick range that puts the floor at `tick_spacing = 14`; anything finer is rejected with `TickSpacingTooFine`. The bitmap is sized to the spacing, so a pool only pays for the grid it uses — spacing 60 gives 14 788 cells (1 849 bytes) and a 2 006-byte pool, spacing 14 gives 63 378 cells (7 923 bytes) and an 8 080-byte pool — both inside the 10 KiB a single `create_account` allows. |

Recommended fee tiers: 500 (0.05%) stable pairs, 3 000 (0.3%) standard, 10 000 (1%) exotic/volatile.

---

## 🔑 Account Architecture & Program Derived Addresses (PDAs)

```
                       +------------------------+
                       |      Pool Account      |
                       | PDA [clmm-pool, ...]   |
                       +-----------+------------+
                                   |
         +-------------------------+-------------------------+
         |                         |                         |
+--------v--------+       +--------v--------+       +--------v--------+
|  Vault A (PDA)  |       |  Vault B (PDA)  |       |  Tick (PDA)     |
|   clmm-vault-a  |       |   clmm-vault-b  |       |   clmm-tick     |
+-----------------+       +-----------------+       +--------+--------+
                                                             |
                                                    +--------v--------+
                                                    | liquidity_gross |
                                                    | liquidity_net   |
                                                    | fee_growth_out. |
                                                    +-----------------+

                       +------------------------+
                       |    Position (PDA)      |
                       |       clmm-pos         |
                       +------------------------+
```

The boxes show each account's seed *prefix*; the exact seed tuples are in the table below.

### PDA Seeds Table

| Account | PDA Seeds Format | Description |
| :--- | :--- | :--- |
| **Pool** | `[b"clmm-pool", mint_a.key(), mint_b.key()]` | Pool state: price, active liquidity, fee config, global accumulators, and the tick bitmap (one bit per tick-spacing-wide cell). |
| **Vault A** | `[b"clmm-vault-a", pool.key(), mint_a.key()]` | Pool-owned token A account; authority is the pool PDA. |
| **Vault B** | `[b"clmm-vault-b", pool.key(), mint_b.key()]` | Pool-owned token B account; authority is the pool PDA. |
| **TickState** | `[b"clmm-tick", pool.key(), tick_index.to_le_bytes()]` | Per-tick liquidity net/gross and fee-growth mirrors. |
| **Position** | `[b"clmm-pos", pool.key(), owner.key(), lower.to_le_bytes(), upper.to_le_bytes()]` | One LP position per (pool, owner, range). |

All integer seeds are **little-endian**, including negative `i32` tick indices.

---

## 🛠️ Instructions API

### 1. `initialize_pool`

Creates the pool and both vaults.

* **Parameters**:
  * `initial_sqrt_price_x64: u128`: Starting price as a Q64.64 square root (`math_core::sqrt_price_at_tick(t)` for a tick-based start; `1 << 64` = price 1.0). Must be inside the usable range.
  * `tick_spacing: i32`: Minimum tick separation (e.g. `60`). Enforced on every position, and used to size the pool's tick bitmap — it must be at least `14`, so that the bitmap can cover the whole tick range inside one account (`TickSpacingTooFine` otherwise).
  * `fee_pips: u32`: Swap fee in millionths (`3_000` = 0.3%). ≤ 999 999.
* **Accounts**: `authority` (signer, payer), `mint_a`, `mint_b`, `pool` (init), `vault_a` (init), `vault_b` (init), `token_program`, `system_program`.
* **Note**: vaults are PDAs with the pool PDA as token authority, so swaps and withdrawals are signed by the pool itself. The pool account is allocated at `Pool::len_for(tick_spacing)`, which includes its bitmap; the bitmap starts empty, because a pool has no initialized ticks until an LP adds liquidity.

### 2. `add_liquidity`

Deposits into a tick range and mints the position. `init_if_needed` on the tick and position PDAs makes first deposits and top-ups one instruction.

* **Parameters**: `lower_tick: i32`, `upper_tick: i32`, `liquidity_amount: u128`.
* **Validations**: `lower < upper`, both in the usable range and multiples of `tick_spacing`, amount > 0.
* **Flow**: bind a new position to its owner and range → compute amounts for the exact range and current price (rounded up) → accrue the position's existing fees → grow position and re-base its snapshot → initialize/accumulate both ticks (gross, net, and their bit in the pool bitmap) → bump pool liquidity if the range spans the price → transfer tokens in.
* **Accounts**: `pool` (mut), `lower_tick_state` (init_if_needed), `upper_tick_state` (init_if_needed), `position` (init_if_needed), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`, `system_program`.
* **Note**: adding to a range always earns the fees accrued so far (they are banked into `tokens_owed` before the snapshot re-bases); call `collect_fees` to withdraw them.
* **Note**: the first deposit records `pool`, `owner`, `lower_tick` and `upper_tick` on the position. `remove_liquidity` and `collect_fees` both require `position.owner == user`, so this write is what makes the position withdrawable at all; a later deposit re-checks all four fields (`WrongPosition`).

### 3. `remove_liquidity`

Burns liquidity, pays out the tokens it backs, banks any unclaimed fees into `tokens_owed`, and clears a tick's bitmap bit once the last position pinned to it leaves.

* **Parameters**: `lower_tick: i32`, `upper_tick: i32`, `liquidity_amount: u128`, `min_amount_a: u64`, `min_amount_b: u64`.
* **Validations**: amount > 0, sufficient position liquidity; output floors checked **before** any state write, so a failed slippage check leaves everything untouched.
* **Accounts**: `pool` (mut), `position` (mut), `lower_tick_state` (mut), `upper_tick_state` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`.

### 4. `collect_fees`

Claims the fees a position has earned. Read-only on the pool and ticks; only the position and the user's token accounts are writable.

* **Parameters**: `lower_tick: i32`, `upper_tick: i32` (the position's range). Both must match the range recorded on the position (`WrongPosition`), so the fee snapshot and the ticks it is read against can never come from different ranges.
* **Accounts**: `pool`, `position` (mut), `lower_tick_state`, `upper_tick_state`, `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`.

### 5. `swap`

Executes a swap, walking the supplied ticks until the input is consumed or the walk runs out.

* **Parameters**:
  * `amount_in: u64`: Offered input (an upper bound — only what executes is pulled).
  * `min_amount_out: u64`: Slippage floor on the received output.
  * `is_a_to_b: bool`: `true` sells A for B (price down, walk descends); `false` sells B for A (price up, walk ascends).
* **Accounts**: `pool` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`.
* **Remaining accounts**: the initialized `TickState` PDAs the trade may cross, **writable**, in walk order. The order is checked against the pool's tick bitmap, not trusted: the first account must be the nearest initialized tick in the direction of travel (for an A→B swap that is the greatest initialized tick `≤ tick_current`, which may be `tick_current` itself — a price resting on a boundary is crossed zero-width), and each following account must be the next initialized tick after the previous one, descending for A→B and ascending for B→A. Anything else fails with `TicksNotInOrder`, and supplying fewer ticks simply caps how far the price may travel.
* **Settlement**: pulls `consumed = amount_in − unspent`, pays `amount_out`, enforces `min_amount_out`, and fails with `InsufficientLiquidity` if nothing at all could trade.

---

## 📄 Data Structures

### `Pool` State
```rust
pub struct Pool {
    pub mint_a: Pubkey,             // Token A (token0) mint
    pub mint_b: Pubkey,             // Token B (token1) mint; price = B / A
    pub sqrt_price_x64: u128,       // sqrt(price) in Q64.64
    pub liquidity: u128,            // Active (in-range) liquidity
    pub tick_current: i32,          // Tick containing sqrt_price_x64
    pub tick_spacing: i32,          // Enforced grid for positions
    pub fee_pips: u32,              // Swap fee in millionths
    pub fee_growth_global_a: u128,  // Q64.64 fee growth per unit liquidity, token A
    pub fee_growth_global_b: u128,  // Q64.64 fee growth per unit liquidity, token B
    pub bump: u8,                   // Pool PDA bump (also the vault signer seed)
    pub bitmap_cells: u32,          // Bits in tick_bitmap
    pub tick_bitmap: Vec<u8>,       // One bit per tick-spacing-wide cell, set while that tick carries liquidity
}
```

The bitmap is the pool's authoritative list of initialized ticks — the walk of a swap is validated against it — and it costs 1 849 bytes at `tick_spacing = 60`.

### `TickState` Account
```rust
pub struct TickState {
    pub pool: Pubkey,                // Parent pool
    pub tick_index: i32,             // This tick
    pub liquidity_gross: u128,       // Total liquidity referencing this tick
    pub liquidity_net: i128,         // Delta applied to active L when crossed upward
    pub fee_growth_outside_a: u128,  // Fee growth mirror, token A
    pub fee_growth_outside_b: u128,  // Fee growth mirror, token B
    pub bump: u8,                    // Bump this PDA was created with (lets a swap check canonicity with one hash)
}
```

### `Position` Account
```rust
pub struct Position {
    pub pool: Pubkey,                // Written on first deposit, alongside the fields below
    pub owner: Pubkey,               // Written on first deposit; gates remove_liquidity / collect_fees
    pub lower_tick: i32,
    pub upper_tick: i32,
    pub liquidity: u128,
    pub fee_growth_inside_a: u128,   // Snapshot for fee accrual, token A
    pub fee_growth_inside_b: u128,   // Snapshot for fee accrual, token B
    pub tokens_owed_a: u64,          // Banked, unclaimed fees (token A)
    pub tokens_owed_b: u64,          // Banked, unclaimed fees (token B)
}
```

---

## 🔌 TypeScript Integration

Uses `@coral-xyz/anchor` with the IDL types `anchor build` writes to `target/types/clmm.ts`, plus `@solana/web3.js` and `@solana/spl-token`.

### PDAs and helpers

The two tick seeds are little-endian `i32`s — including negatives — so build them with an explicit `Int32LE` write:

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import { Clmm } from "./target/types/clmm";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Clmm as Program<Clmm>;

const i32le = (n: number) => {
  const b = Buffer.alloc(4);
  b.writeInt32LE(n);
  return b;
};

const poolPda = (mintA: PublicKey, mintB: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("clmm-pool"), mintA.toBuffer(), mintB.toBuffer()],
    program.programId
  )[0];

const vaultPda = (pool: PublicKey, mint: PublicKey, side: "clmm-vault-a" | "clmm-vault-b") =>
  PublicKey.findProgramAddressSync(
    [Buffer.from(side), pool.toBuffer(), mint.toBuffer()],
    program.programId
  )[0];

const tickPda = (pool: PublicKey, tick: number) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("clmm-tick"), pool.toBuffer(), i32le(tick)],
    program.programId
  )[0];

const positionPda = (pool: PublicKey, owner: PublicKey, lower: number, upper: number) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("clmm-pos"), pool.toBuffer(), owner.toBuffer(), i32le(lower), i32le(upper)],
    program.programId
  )[0];
```

### Create a pool

```typescript
async function createPool(mintA: PublicKey, mintB: PublicKey) {
  const pool = poolPda(mintA, mintB);
  // Price 1.0 as a Q64.64 sqrt price, 60-tick spacing, 0.3% fee.
  const SQRT_PRICE_1 = new anchor.BN(1).shln(64);

  await program.methods
    .initializePool(SQRT_PRICE_1, 60, 3_000)
    .accounts({
      authority: provider.wallet.publicKey,
      mintA,
      mintB,
      pool,
      vaultA: vaultPda(pool, mintA, "clmm-vault-a"),
      vaultB: vaultPda(pool, mintB, "clmm-vault-b"),
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();
}
```

### Add liquidity

```typescript
async function addLiquidity(
  pool: PublicKey, mintA: PublicKey, mintB: PublicKey,
  lowerTick: number, upperTick: number, liquidity: bigint
) {
  const owner = provider.wallet.publicKey;
  await program.methods
    .addLiquidity(lowerTick, upperTick, new anchor.BN(liquidity.toString()))
    .accounts({
      pool,
      lowerTickState: tickPda(pool, lowerTick),
      upperTickState: tickPda(pool, upperTick),
      position: positionPda(pool, owner, lowerTick, upperTick),
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "clmm-vault-a"),
      vaultB: vaultPda(pool, mintB, "clmm-vault-b"),
      userTokenA,
      userTokenB,
      user: owner,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();
}
```

### Swap with a tick walk

The caller chooses how far the price may travel by supplying ticks. The program validates that list against the pool's tick bitmap, so the client has to build it the same way — nearest initialized tick in the direction of travel first, then each next one that way. Read the bitmap straight off the pool account:

```typescript
const MIN_TICK = -443636;

/** Bit for a tick: its cell `floor(tick / spacing)` minus the lowest cell the
 *  bitmap covers (bit 0 is the cell holding MIN_TICK). */
function bitOf(tick: number, spacing: number): number {
  return Math.floor(tick / spacing) - Math.floor(MIN_TICK / spacing);
}

function isSet(bitmap: Uint8Array, bit: number): boolean {
  return bit >= 0 && (bitmap[bit >> 3] & (1 << (bit & 7))) !== 0;
}

/** The walk `swap` will accept: the next `depth` initialized ticks the price
 *  would reach. A down (A→B) walk may start on `tickCurrent` itself, since a
 *  price resting on a boundary is crossed zero-width; an up walk starts
 *  strictly above it, because the ranges there are already live. */
function walkTicks(
  state: { tickCurrent: number; tickSpacing: number; tickBitmap: number[] },
  isAToB: boolean, depth = 5
): number[] {
  const spacing = state.tickSpacing;
  const bitmap = Uint8Array.from(state.tickBitmap);
  const ticks: number[] = [];
  let cell = bitOf(state.tickCurrent, spacing) + (isAToB ? 0 : 1);
  while (ticks.length < depth) {
    let hit = -1;
    for (let c = cell; isAToB ? c >= 0 : c < bitmap.length * 8; c += isAToB ? -1 : 1) {
      if (isSet(bitmap, c)) { hit = c; break; }
    }
    if (hit < 0) break;                                   // nothing initialized that way
    ticks.push((hit + Math.floor(MIN_TICK / spacing)) * spacing);
    cell = hit + (isAToB ? -1 : 1);
  }
  return ticks;
}

async function swap(
  pool: PublicKey, mintA: PublicKey, mintB: PublicKey,
  amountIn: bigint, minAmountOut: bigint, isAToB: boolean
) {
  const state = await program.account.pool.fetch(pool);
  const ticks = walkTicks(state, isAToB);

  await program.methods
    .swap(new anchor.BN(amountIn.toString()), new anchor.BN(minAmountOut.toString()), isAToB)
    .accounts({
      pool,
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "clmm-vault-a"),
      vaultB: vaultPda(pool, mintB, "clmm-vault-b"),
      userTokenA,
      userTokenB,
      user: provider.wallet.publicKey,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .remainingAccounts(
      ticks.map((t) => ({ pubkey: tickPda(pool, t), isWritable: true, isSigner: false }))
    )
    .rpc();
}
```

### Remove liquidity & collect fees

```typescript
async function removeLiquidity(
  pool: PublicKey, mintA: PublicKey, mintB: PublicKey,
  lowerTick: number, upperTick: number, liquidity: bigint, minA: bigint, minB: bigint
) {
  const owner = provider.wallet.publicKey;
  await program.methods
    .removeLiquidity(
      lowerTick, upperTick,
      new anchor.BN(liquidity.toString()),
      new anchor.BN(minA.toString()), new anchor.BN(minB.toString())
    )
    .accounts({
      pool,
      position: positionPda(pool, owner, lowerTick, upperTick),
      lowerTickState: tickPda(pool, lowerTick),
      upperTickState: tickPda(pool, upperTick),
      mintA, mintB,
      vaultA: vaultPda(pool, mintA, "clmm-vault-a"),
      vaultB: vaultPda(pool, mintB, "clmm-vault-b"),
      userTokenA, userTokenB,
      user: owner,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .rpc();
}

async function collectFees(
  pool: PublicKey, mintA: PublicKey, mintB: PublicKey,
  lowerTick: number, upperTick: number
) {
  const owner = provider.wallet.publicKey;
  await program.methods
    .collectFees(lowerTick, upperTick)
    .accounts({
      pool,
      position: positionPda(pool, owner, lowerTick, upperTick),
      lowerTickState: tickPda(pool, lowerTick),
      upperTickState: tickPda(pool, upperTick),
      mintA, mintB,
      vaultA: vaultPda(pool, mintA, "clmm-vault-a"),
      vaultB: vaultPda(pool, mintB, "clmm-vault-b"),
      userTokenA, userTokenB,
      user: owner,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .rpc();
}
```

### Integration pitfalls

1. **The walk must match the bitmap.** Remaining accounts are checked against the pool's tick bitmap, so they must be the *next* initialized ticks in the direction of travel — not merely a sorted list of ticks you happen to know about. Skipping an initialized tick, or starting past one, fails with `TicksNotInOrder`. Re-read the pool between swaps: `tick_current` and the bitmap both change with every trade.
2. **Ticks must be canonical and writable.** Each remaining account is checked against its `[b"clmm-tick", pool, tick]` PDA and must be flagged writable (the walk flips fee mirrors and rewrites liquidity). Both failures fail fast with `WrongTickAccount` / `TickNotWritable`.
3. **The bitmap is the source of truth.** `pool.tick_bitmap` lists exactly the ticks that exist: set by `add_liquidity`, cleared once a tick's last position leaves. Build the walk from it (the `walkTicks` helper above does); a tick that was never initialized has no PDA to pass at all.
4. **Partial fills are normal.** Running out of supplied ticks is not an error: the trade settles at the last reachable price and leaves the unspent input in the wallet. Size `amount_in` as a ceiling and rely on `min_amount_out`.
5. **One position per (pool, owner, range).** The position PDA is keyed by both tick boundaries. Top-ups merge into it; overlapping-but-different ranges are separate positions.
6. **Everything is raw units.** Amounts are base units; `liquidity_amount` is a raw L value, not a token amount — derive it from a desired token amount by inverting the formulas in §2 (or quote off-chain with `math_core` and pass the L that reproduces it).
7. **Fees are claim-style.** Nothing pays fees automatically; fees accrue on touch and are moved by `collect_fees`. A dormant position's fees still accrue correctly (accounting only needs the current accumulators), but nothing is transferable until a touch.
8. **`is_a_to_b` is the only direction flag.** `true` = sell A, price falls, ticks descend. Mixing the direction up with the tick ordering is caught by validation, but the trade will not be the one you intended.
9. **Spacing floor.** `tick_spacing >= 14`: the pool's bitmap has to cover the whole tick range inside a single account, and finer grids do not fit.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `MathOverflow` (6000) | `Math overflow` | Checked arithmetic overflow/underflow or a failed integer conversion. |
| `InvalidTicks` (6001) | `Invalid ticks` | `lower >= upper`, or a tick outside the usable range. |
| `InvalidTickSpacing` (6002) | `Tick spacing mismatch: ticks must be multiples of the pool's tick spacing` | Position ticks not on the pool's grid, or `tick_spacing <= 0` at init. |
| `SqrtPriceOutOfBounds` (6003) | `Sqrt price out of bounds` | Initial price outside the usable Q64.64 range. |
| `FeeTooHigh` (6004) | `Fee too high` | `fee_pips >= 1 000 000` at init, or `math_core` rejected the fee. |
| `ZeroLiquidity` (6005) | `Zero liquidity` | `liquidity_amount` must be greater than zero. |
| `ZeroAmount` (6006) | `Amount must be greater than zero` | Swap input must be greater than zero. |
| `SlippageExceeded` (6007) | `Slippage exceeded` | Swap output below `min_amount_out`, or withdrawal below its floors. |
| `InsufficientLiquidity` (6008) | `Insufficient liquidity to fill the swap` | The walk could not trade anything (no active liquidity in the supplied ticks). |
| `InsufficientShares` (6009) | `Not enough liquidity in the position` | Burning more liquidity than the position holds. |
| `Unauthorized` (6010) | `Position does not belong to this owner` | Signer is not `position.owner`. |
| `WrongMint` (6011) | `Token mint does not match the pool` | A user token account's mint does not match the pool's `mint_a`/`mint_b`. |
| `WrongTickAccount` (6012) | `Tick account is not a canonical, writable tick PDA of this pool` | A tick account is not the canonical PDA for its tick, belongs to another pool, or a tick state disagrees with its seed. |
| `TicksNotInOrder` (6013) | `Tick accounts must be supplied in the order the swap walks them` | A remaining account is not the next initialized tick the pool's bitmap says comes next — skipped, reordered, or past the end of the bitmap. |
| `TickNotWritable` (6014) | `Tick account must be passed as writable` | A remaining tick account was passed read-only. |
| `TickSpacingTooFine` (6015) | `Tick spacing is too fine to fit the pool's tick bitmap` | `tick_spacing < 14` at init: the bitmap could not cover the tick range in one account. |
| `TickOutOfBitmap` (6016) | `Tick falls outside the pool's tick bitmap` | A tick bound does not map to a bit (only reachable from corrupted pool data). |
| `WrongPosition` (6017) | `Position account does not match its owner or its range` | A deposit or fee claim was given a range that the position does not record. |

---

## 🧪 Testing & Verification

Inline unit tests cover the fee formula (`ceil` semantics, budget identity `in + fee ≤ offered` over all inputs 0–5 000 across seven fee tiers and real swap steps), the inside-growth arithmetic in all three price configurations, snapshot re-basing, and the position-amount split below/inside/above the range including boundary-exact cases.

The tick bookkeeping added for the verified walk is covered directly: that valid ticks map to dense, strictly increasing bits and extreme ticks stay inside the bitmap; that the spacing floor (`14` fits, `13` does not) keeps a pool in one account; that the bitmap scans are inclusive downward, exclusive upward, capped at both ends and unaffected by bits past the last cell; that a walk steps tick by tick and stops at the end of the bitmap; that a down-cross at the lowest usable tick leaves `tick_current` below every bit, so the price cannot be walked any lower; and that crossing moves the active set and records `tick_current` on the tick going up but on the one below going down, and refuses to drive liquidity negative.

End-to-end tests live in `tests/e2e`. They run the *built* program image inside a Solana VM (LiteSVM) with the real SPL token program, and cover what unit tests cannot reach: that a deposit records its owner so the position can be withdrawn from at all, that the bitmap is set and cleared with the positions behind it, that a walk the bitmap disagrees with is rejected, that a two-tick swap leaves `tick_current`/`liquidity` where the crossings say, and that an LP can collect fees and withdraw. The crate is its own workspace on purpose — it depends on the Solana stack its VM is built from, which cannot share a Cargo graph with the programs' `solana-program 5.0`.

```bash
cargo test -p clmm             # unit tests (no SBF build needed)
anchor build -p clmm           # SBF build
(cd tests/e2e && cargo test)   # end-to-end tests, against target/deploy/clmm.so
```

### Known simplifications (vs. production Uniswap v3)

* **No circuit-breaker integration.** The `oracle` program in this repo is the price safety layer (a 30-minute TWAP plus a deviation guard and a permissionless `trip_breaker` latch), but nothing calls it yet: neither this program nor the TWAMM references the oracle, so a price far from the TWAP is not blocked on-chain. Wiring it in is a pool-side change — CPI `check_deviation` with the price the swap will actually execute at, before pricing the walk.
* **No mempool of price limits.** A swap cannot set a price limit of its own: the walk is the limit, and it is bounded by the pool's initialized ticks. A trader who wants a tighter stop supplies fewer ticks (a partial fill) and relies on `min_amount_out`.
* **No tick-flip staleness check.** Crossings flip mirrors unconditionally; a production build cross-checks against `tick_ceil`-style bounds to defend against pathological accumulator states.
* **No transfer-hook passthrough.** Transfers use the classic SPL `token::transfer`; RWA Token-2022 hooks (as in the dlmm/transfer_hook programs) are not wired through `remaining_accounts`.
* **No protocol fee/treasury cut**, no admin authority, no pause switch — the pool is fully permissionless once created.
