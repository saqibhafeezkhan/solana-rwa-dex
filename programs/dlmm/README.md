# Dynamic Liquidity Market Maker (DLMM) Program

An Anchor-based Solana program implementing a **Bin-Based Dynamic Liquidity Market Maker (DLMM)** optimized for low-slippage trading, real-world assets (RWA), and Token-2022 compatibility.

---

## 🌟 Overview

The DLMM architecture discretizes pool liquidity into individual **price bins**. Each bin represents a single fixed exchange rate ($Q64.64$ fixed-point precision) and maintains independent token reserves.

### Key Characteristics:
* **Zero Slippage within Bins**: Swaps execute at the fixed bin rate until the bin's output reserve is exhausted.
* **Bin Arrays**: Bins are grouped into contiguous `BinArray` accounts (70 bins per array) to minimize account overhead and rent costs across tight peg bands.
* **Euclidean Bin Indexing**: Bin IDs tile the entire real number line ($\dots, -2, -1, 0, 1, 2, \dots$), making tight peg bands below parity affordable and mathematically precise.
* **Token-2022 & Transfer Hook Compatibility**: Built-in balance measurement (`transfer_and_measure`) handles transfer fees dynamically, and custom transfer hook accounts are passed through transparently to enable RWA compliance checks (KYC/AML, whitelist/blacklist policies).
* **Decimals-Aware Price Scaling**: Raw token amounts are scaled according to token mint decimals ($10^{\text{decimals\_a} - \text{decimals\_b}}$), preventing valuation distortion when trading assets with disparate decimal representations (e.g., 6-decimal USDC vs. 9-decimal SOL/RWA tokens).
* **Dynamic Fee Structure**: Base fee scales up during high-utilization or one-sided bin conditions to protect LPs against dynamic toxic flow.
* **Non-Trapping Pause Mechanism**: When a pool is paused by authority, deposits and swaps are rejected, but withdrawals (`remove_liquidity`) remain open so LP funds are never trapped.
* **Institutional Wiring Rails**: Like the CLMM, pools can fix a KYC gate, a TWAP circuit breaker and T+N settlement routing at birth — see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement).

---

## 📐 Mathematical Model & Bin Dynamics

### 1. Bin Price Calculation
The price of a bin in whole-token units is derived geometrically from basis-point tick steps:

$$P_{\text{whole}} = 1.0001^{(\text{bin\_id} \times \text{bin\_step})}$$

Where:
* `bin_step` is specified in basis points ($1 \text{ bps} = 0.01\%$).
* `bin_id = 0` corresponds to parity ($P_{\text{whole}} = 1.0$).
* Computation leverages `math_core::sqrt_price_at_tick` for accurate $Q64.64$ conversion.

### 2. Raw Unit Price Scaling
Since Solana token balances are handled in integer raw units, prices stored in bins are scaled into raw unit ratios:

$$P_{\text{raw}} = P_{\text{whole}} \times 10^{(\text{decimals\_a} - \text{decimals\_b})}$$

This guarantees that:
$$\text{Value in Token A} = \text{Amount}_A + \frac{\text{Amount}_B \times P_{\text{raw}}}{2^{64}}$$

### 3. Dynamic Fee Formula
Fee computation adds a utilization multiplier to the pool's `base_fee_bps`:

$$\text{Fee BPS} = \text{base\_fee\_bps} + \frac{\text{base\_fee\_bps} \times \text{utilization}}{100}$$

* **Balanced Bin** (Reserves $A > 0$ and $B > 0$): $\text{utilization} = 50\%$
* **Imbalanced Bin** (One-sided reserves): $\text{utilization} = 90\%$

The fee is charged on **what actually arrived** in the vault (a Token-2022 transfer fee can skim the amount in flight) and is credited to the bin the trade started in, so it accrues to the LPs of the bin the price was in:

$$fee = \Big\lfloor \frac{amount\_received \times fee\_bps}{10\,000} \Big\rfloor \qquad\quad amount\_to\_fill = amount\_received - fee$$

### 4. Bin indexing

Bins tile the whole number line, and negative ids belong to negative arrays. Both directions use Euclidean division, so bin `-1` is the *last* offset of array `-1` rather than a bin of array `0`:

$$index = \Big\lfloor \frac{bin\_id}{70} \Big\rfloor_{Euclidean} \qquad\qquad offset = bin\_id \bmod 70$$

| `bin_id` | `array_index` | `offset` |
| ---: | ---: | ---: |
| 0 | 0 | 0 |
| 69 | 0 | 69 |
| 70 | 1 | 0 |
| -1 | -1 | 69 |
| -70 | -1 | 0 |
| -71 | -2 | 69 |

### 5. Shares

Shares are denominated in the deposit's value in token A at the bin's price, $V = a \cdot 2^{64} + b \cdot P_{raw}$. The first deposit into a bin defines the share unit; later deposits are scaled by the existing supply, so the ratio of shares to value cannot move:

$$shares = \begin{cases} V_{deposit} & supply = 0 \\[4pt] \Big\lfloor \dfrac{V_{deposit} \times supply}{V_{reserves}} \Big\rfloor & \text{otherwise} \end{cases} \qquad\quad out = \Big\lfloor \dfrac{reserve \times shares}{supply} \Big\rfloor$$

Both round **down**, and a withdrawal checks its `min_amount_a` / `min_amount_b` floors before anything is written, so a failed slippage check leaves the bin untouched.

### 6. Quoting and rounding

A bin quotes its fixed price, and every division rounds in the pool's favour. `is_a_to_b` sells A for B:

$$out_B = \Big\lfloor \frac{in_A \times 2^{64}}{P_{raw}} \Big\rfloor \qquad\qquad out_A = \Big\lfloor \frac{in_B \times P_{raw}}{2^{64}} \Big\rfloor$$

When a bin cannot cover the rest of a trade it is drained to exactly zero, and the input that drains it is the *ceiling* of the quote, which is why draining never leaves a bin with a sliver of the output token:

$$in\_needed = \Big\lceil \frac{out \times P_{raw}}{2^{64}} \Big\rceil \;(A \rightarrow B) \qquad\qquad in\_needed = \Big\lceil \frac{out \times 2^{64}}{P_{raw}} \Big\rceil \;(B \rightarrow A)$$

Asymmetric rounding (inputs up, outputs down) means $\text{in\_needed}(\text{quote\_out}(x)) \le x$ for every amount, so top-ups can never cost the trader more than they spent - the unit tests assert exactly this over every input from 1 to 5 000.

### Worked example: a swap that crosses an array boundary

Pool with `bin_step = 10` bps, `base_fee_bps = 30`, active bin **69** (the last bin of array 0), and the two arrays below. A trader sells **1 500 raw A**.

| Bin | Array | Price $P_{raw}$ | A reserve | B reserve |
| ---: | ---: | ---: | ---: | ---: |
| 69 | 0 | 1.071432 | 1 000 | 1 000 |
| 70 | 1 | 1.071540 | 0 | 1 000 |

1. **Fee.** Bin 69 holds both tokens, so its utilization is 50% and the fee is $30 + 30 \times 50\% = 45$ bps. $fee = \lfloor 1500 \times 45 / 10\,000 \rfloor = 6$ raw A, credited to bin 69 (which now holds 1 006 A). $1\,494$ A is left to fill.
2. **Bin 69.** $\lfloor 1494 / 1.071432 \rfloor = 1\,394$ B is more than the 1 000 B it holds, so it is drained: it takes $\lceil 1000 \times 1.071432 \rceil = 1\,072$ A and pays out 1 000 B. Bin 69 is left holding only A (2 078 A, 0 B), and 422 A remains unfilled.
3. **Cross.** The cursor runs off the end of array 0, so the walk continues in the next array the caller supplied - array 1 - starting at bin 70.
4. **Bin 70.** $\lfloor 422 / 1.071540 \rfloor = 393$ B, which it holds, so the trade finishes at that bin's price. Active bin becomes 70.

**Result:** in 1 500 A, fee 6 A to bin 69's LPs, out 1 393 B. The remaining accounts were `[array 0, array 1]`, in ascending order because selling A walks up the bins; selling B would walk down and pass them in descending order.

---

## ⚙️ Program Constants

| Parameter | Value | Description |
| :--- | :--- | :--- |
| `BINS_PER_ARRAY` | `70` | Number of bins packed into a single `BinArray` account (< 4 KiB size) |
| `MAX_BIN_STEP` | `100` | Maximum allowed bin step size in basis points (1.00%) |
| `MAX_BASE_FEE_BPS` | `100` | Maximum allowed base fee in basis points (1.00%) |
| `MAX_BIN_ARRAY_SPAN` | `16` | Maximum distance (in array indices) from `active_bin_id` for creating new arrays |
| `MAX_MINT_DECIMALS` | `9` | Maximum token mint decimals supported for decimal scaling |
| `BPS_DENOMINATOR` | `10,000` | Basis point denominator used across fee calculations |

---

## 🔑 Account Architecture & Program Derived Addresses (PDAs)

```
                       +------------------------+
                       |      Pool Account      |
                       |  (PDA: [pool, A, B])   |
                       +-----------+------------+
                                   |
         +-------------------------+-------------------------+
         |                         |                         |
+--------v--------+       +--------v--------+       +--------v--------+
|  Vault A (PDA)  |       |  Vault B (PDA)  |       | BinArray (PDA)  |
| [vault_a, P, A] |       | [vault_b, P, B] |       | [bin_array,P,i] |
+-----------------+       +-----------------+       +--------+--------+
                                                             |
                                                    +--------v--------+
                                                    | Bins [0..69]    |
                                                    |  - price_x64    |
                                                    |  - reserve_a    |
                                                    |  - reserve_b    |
                                                    |  - supply       |
                                                    +-----------------+
```

### PDA Seeds Table

| Account | PDA Seeds Format | Description |
| :--- | :--- | :--- |
| **Pool** | `[b"pool", mint_a.key(), mint_b.key()]` | Main state account storing pool parameters, active bin, authority, and status. |
| **Vault A** | `[b"vault_a", pool.key(), mint_a.key()]` | Token vault owned by the Pool PDA for Token A. |
| **Vault B** | `[b"vault_b", pool.key(), mint_b.key()]` | Token vault owned by the Pool PDA for Token B. |
| **BinArray** | `[b"bin_array", pool.key(), index.to_le_bytes()]` | Holds 70 contiguous bins for array index `i`. |
| **Position** | `[b"position", pool.key(), owner.key(), bin_id.to_le_bytes()]` | Stores an LP's liquidity share balance in a specific bin. |

---

## 🔌 Pool Wiring: KYC, Oracle, Settlement

Like the CLMM, every DLMM pool fixes its **wiring policy** at `initialize_pool`, and every later swap runs the same three optional rails around the bin math. A field left as `Pubkey::default()` disables its rail:

| Rail | Pool field | Enabled behaviour per swap |
| :--- | :--- | :--- |
| **KYC** | `kyc_config` | CPIs `kyc_gate::verify_kyc` for the trader before any math runs — no attestation, no trade. |
| **Oracle** | `oracle_state` | CPIs `oracle::check_deviation` on the active bin's price pre-trade, then `oracle::record_observation` with the settled post-trade bin price, so the 30-minute TWAP stays current. Both are signed by the pool PDA — the oracle answers only the pool it belongs to. |
| **Settlement** | `hooks_authority` + `settlement_slots` | Swap output is NOT paid to the trader. It is routed into the hook authority's custody ATA and swept into the T+N settlement escrow (`hooks::after_swap` → `settlement::initiate_settlement`); the trader withdraws after `settlement_slots` with a Transfer Agent signature. `min_amount_out` is still checked against the exact amount leaving the pool's vault. |

**Ordering inside `swap`:** `0` policy/account checks → `0a` KYC CPI → `0b` circuit-breaker CPI → `1` pull input (measured) → `2` bin walk + fees → `3` payout (to trader or custody) → `4` oracle observation → `5` settlement escrow CPI. Every step after the walk can only revert the whole transaction; there is no partial state.

**Sentinel convention.** The rail accounts are mandatory either way. An enabled rail's accounts are validated against the pool's recorded policy (config equality, or PDA re-derivation for the oracle and hook authority). A disabled rail's accounts must all be **this AMM program's own ID** — Solana has no null account, so the program ID is the "unused here" sentinel; passing anything else reverts with `RailAccountUnexpected`.

**The hook authority is writable.** `hooks_authority` is passed writable (and as the sentinel when the rail is off): the settlement program charges the trade's vault and escrow to that PDA, and a CPI can only pass on the privileges the caller's instruction already holds.

**The rail's program travels with the rail.** `swap` also carries `kyc_program`, `oracle_program` and `hooks_program` — the real program id for each enabled rail, the executing program's own id otherwise. Every rail is reached by CPI, and a CPI target has to appear in the instruction's account list for the runtime to resolve it: leaving the oracle on but its program out is `Unknown program`, not a skipped check.

**Why the oracle price is recomputed, not read.** Bin prices are deterministic: `priced_bins` prices every bin of an array from `bin_step` and the mint decimals at creation, and afterwards only reserves move. The rail therefore calls `bin_price_raw(active_bin_id, ...)`, a pure function proven equal to what the array stores (the unit test compares it against `priced_bins` for both decimal scalings, several steps and negative bins) — no account read, and no trust in the caller to supply the price.

**Settlement custody.** The output lands in the hook authority's ATA for the output mint, pinned by ATA derivation and re-checked by the settlement program as the escrow sweep's source. The pool's shared vaults are never the source of an escrow deposit, and the trader never momentarily holds the output.

---

## 🛠️ Instructions API

### 1. `initialize_pool`
Creates the main `Pool` account, token vaults, and the initial `BinArray` containing the specified `active_bin_id`.

* **Parameters**:
  * `array_index: i64`: Index of the array holding `active_bin_id` (`bin_id / 70`).
  * `active_bin_id: i32`: Initial active bin ID where swapping will start.
  * `bin_step: u16`: Bin step size in basis points ($1 \le \text{bin\_step} \le 100$).
  * `base_fee_bps: u16`: Base swap fee in basis points ($\le 100$).
  * `authority: Pubkey`: Pool administrator key (e.g., multisig/DAO).
  * `kyc_config: Pubkey`: KYC gate config PDA, or `Pubkey::default()` to leave the KYC rail off (see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement)).
  * `max_deviation_bps: u16`: sqrt-price deviation threshold for the oracle circuit breaker, or `0` to leave it off. Non-zero creates the pool's oracle state **with the pool PDA signing**, atomically with the pool, so its threshold and unpause owner cannot be front-run.
  * `hooks_authority: Pubkey`: Hooks program authority PDA (`[b"hook_authority"]`), or `Pubkey::default()` to pay swap output straight to the trader.
  * `settlement_slots: u64`: T+N horizon in slots; must be non-zero iff `hooks_authority` is set.
* **Accounts**: `payer`, `mint_a`, `mint_b`, `pool` (init), `bin_array` (init), `vault_a` (init), `vault_b` (init), `oracle_state` (the `[b"oracle", pool]` PDA when the oracle rail is on — created here by the oracle program's `initialize` CPI with the pool PDA signing; pass the program-ID sentinel to leave it off), `kyc_program` (optional), `oracle_program` (optional), `hooks_program` (optional), `token_program`, `system_program`.

The three optional program accounts must be passed (as the real program) whenever the matching wiring field is set, so a pool cannot record a policy backed by a look-alike program. Anchor's `Option` convention: omit one by passing the executing program's own ID in its place.

---

### 2. `initialize_bin_array`
Permissionlessly initializes an extra `BinArray` account to extend the pool's tradable price range.

* **Parameters**:
  * `index: i64`: Array index to initialize. Must be within `MAX_BIN_ARRAY_SPAN` (16 arrays) of `active_bin_id`.
* **Accounts**: `payer`, `pool`, `bin_array` (init), `system_program`.

---

### 3. `add_liquidity`
Deposits Token A and/or Token B into a target bin and mints LP shares to the user's `Position` account.

* **Parameters**:
  * `array_index: i64`: Target bin array index.
  * `bin_id: i32`: Target bin ID.
  * `amount_a: u64`: Desired deposit amount of Token A.
  * `amount_b: u64`: Desired deposit amount of Token B.
* **Accounts**: `pool`, `bin_array` (mut), `position` (init_if_needed), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`, `system_program`.
* **Remaining Accounts**: Token-2022 transfer hook accounts (if applicable).

---

### 4. `remove_liquidity`
Burns LP shares from a `Position` and claims the proportional share of Token A and Token B reserves from the bin.

* **Parameters**:
  * `array_index: i64`: Bin array index.
  * `bin_id: i32`: Bin ID.
  * `shares: u128`: Amount of LP shares to burn.
  * `min_amount_a: u64`: Slippage protection floor for Token A received.
  * `min_amount_b: u64`: Slippage protection floor for Token B received.
* **Accounts**: `pool`, `bin_array` (mut), `position` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`.
* **Note**: Functions even when `pool.paused == true`.

---

### 5. `swap`
Swaps an input amount of Token A for Token B (or vice versa), walking through contiguous bin arrays starting at `active_bin_id`. Runs the pool's wiring rails around the math (see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement)).

* **Parameters**:
  * `amount_in: u64`: Exact amount of input token provided.
  * `min_amount_out: u64`: Minimum required amount of output token (slippage check).
  * `is_a_to_b: bool`: `true` to swap A $\rightarrow$ B (moves bin index up); `false` to swap B $\rightarrow$ A (moves bin index down).
  * `trade_id: u64`: Names the settlement escrow; must be non-zero iff the settlement rail is on.
* **Accounts**: `pool` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `kyc_config` (mut, rail-pinned), `user_kyc_account`, `oracle_state` (mut, rail-pinned), `hooks_authority`, `custody_token_account` (mut, rail-pinned), `settlement_config`, `settlement_program`, `settlement_vault` (mut, rail-pinned), `settlement_vault_token` (mut, rail-pinned), `associated_token_program`, `token_program`, `system_program`. Rail accounts follow the same Option convention as at init: a disabled rail's accounts are passed as the AMM program ID.
* **Remaining Accounts**:
  1. `BinArray` accounts (passed in walk order).
  2. Token-2022 transfer hook accounts (filtered by program ID ownership).

---

### 6. `set_paused`
Admin instruction to pause or resume swapping and liquidity additions.

* **Parameters**:
  * `paused: bool`: `true` to pause, `false` to unpause.
* **Accounts**: `pool` (mut), `authority` (signer).

---

### 7. `set_authority`
Admin instruction to transfer pool authority to a new public key.

* **Parameters**:
  * `new_authority: Pubkey`: Public key of new admin authority.
* **Accounts**: `pool` (mut), `authority` (signer).

---

## 📄 Data Structures

### `Pool` State
```rust
pub struct Pool {
    pub mint_a: Pubkey,        // Mint address of Token A
    pub mint_b: Pubkey,        // Mint address of Token B
    pub bin_step: u16,         // Step size in basis points (1..=100)
    pub base_fee_bps: u16,     // Base swap fee in basis points (0..=100)
    pub active_bin_id: i32,    // Current active bin ID
    pub authority: Pubkey,     // Admin pubkey
    pub paused: bool,          // Pause flag
    pub decimals_a: u8,        // Mint A decimals (max 9)
    pub decimals_b: u8,        // Mint B decimals (max 9)
    pub kyc_config: Pubkey,    // KYC gate config, default = rail off
    pub oracle_state: Pubkey,  // Oracle state PDA, default = rail off
    pub hooks_authority: Pubkey, // Hooks authority PDA, default = rail off
    pub settlement_slots: u64, // T+N horizon in slots (settlement rail)
    pub bump: u8,              // Pool PDA bump
    pub vault_a_bump: u8,      // Vault A PDA bump
    pub vault_b_bump: u8,      // Vault B PDA bump
}
```

### `Bin` Structure
```rust
pub struct Bin {
    pub price_x64: u128,  // Fixed price (raw A per raw B) in Q64.64
    pub reserve_a: u64,   // Reserve of Token A in raw units
    pub reserve_b: u64,   // Reserve of Token B in raw units
    pub supply: u128,     // Total LP shares issued for this bin
}
```

### `BinArray` Account
```rust
pub struct BinArray {
    pub pool: Pubkey,       // Parent Pool public key
    pub index: i64,          // Bin array index
    pub bins: Vec<Bin>,      // Exactly 70 Bin elements
}
```

### `Position` Account
```rust
pub struct Position {
    pub pool: Pubkey,              // Parent Pool public key
    pub owner: Pubkey,             // Position owner public key
    pub bin_id: i32,               // Target bin ID
    pub liquidity_shares: u128,    // LP shares held by owner
}
```

---

## 🔌 TypeScript Integration

The snippets use the same stack as the other program docs: `@coral-xyz/anchor` with the IDL types `anchor build` writes to `target/types/dlmm.ts`, plus `@solana/web3.js` and `@solana/spl-token`.

### PDAs and bin maths

Addresses are easy to get wrong because two of the seeds are little-endian integers:

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import { TOKEN_2022_PROGRAM_ID } from "@solana/spl-token";
import { Dlmm } from "./target/types/dlmm";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Dlmm as Program<Dlmm>;

/** BINS_PER_ARRAY in the program. */
const BINS_PER_ARRAY = 70;

/** Math.floor and % are already Euclidean for integers, which is what the
 *  program's div_euclid / rem_euclid do: bin -1 is array -1, offset 69. */
const binArrayIndex = (binId: number) => Math.floor(binId / BINS_PER_ARRAY);
const binOffset = (binId: number) => ((binId % BINS_PER_ARRAY) + BINS_PER_ARRAY) % BINS_PER_ARRAY;

const i64 = (n: number) => {
  const b = Buffer.alloc(8);
  b.writeBigInt64LE(BigInt(n));
  return b;
};
const i32 = (n: number) => {
  const b = Buffer.alloc(4);
  b.writeInt32LE(n);
  return b;
};

const poolPda = (mintA: PublicKey, mintB: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("pool"), mintA.toBuffer(), mintB.toBuffer()],
    program.programId
  )[0];

const binArrayPda = (pool: PublicKey, index: number) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("bin_array"), pool.toBuffer(), i64(index)],
    program.programId
  )[0];

const vaultPda = (pool: PublicKey, mint: PublicKey, side: "vault_a" | "vault_b") =>
  PublicKey.findProgramAddressSync(
    [Buffer.from(side), pool.toBuffer(), mint.toBuffer()],
    program.programId
  )[0];

const positionPda = (pool: PublicKey, owner: PublicKey, binId: number) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("position"), pool.toBuffer(), owner.toBuffer(), i32(binId)],
    program.programId
  )[0];
```

### Create a pool, then read it

```typescript
async function createPool(mintA: PublicKey, mintB: PublicKey, decimalsA: number, decimalsB: number) {
  const pool = poolPda(mintA, mintB);
  // `array_index` is part of the array's PDA seeds, so it is passed explicitly
  // and must be bin_array_index(active_bin_id) - the program checks it.
  const activeBinId = 0;
  const arrayIndex = binArrayIndex(activeBinId);

  await program.methods
    // args: arrayIndex, activeBinId, binStep, baseFeeBps, authority,
    //       kycConfig, maxDeviationBps, hooksAuthority, settlementSlots.
    // Rails off here: default config/PDA and a zero threshold.
    .initializePool(
      new anchor.BN(arrayIndex), activeBinId, 10, 30, provider.wallet.publicKey,
      PublicKey.default, 0, PublicKey.default, 0
    )
    .accounts({
      payer: provider.wallet.publicKey,
      mintA,
      mintB,
      pool,
      binArray: binArrayPda(pool, arrayIndex),
      vaultA: vaultPda(pool, mintA, "vault_a"),
      vaultB: vaultPda(pool, mintB, "vault_b"),
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();

  const state = await program.account.pool.fetch(pool);
  const array = await program.account.binArray.fetch(binArrayPda(pool, arrayIndex));
  const price = array.bins[binOffset(state.activeBinId)];

  // A Q64.64 price is `raw A per raw B`; divide by 2^64 for a UI number.
  const priceUi = Number(price.priceX64.toString()) / 2 ** 64;
  console.log("active bin", state.activeBinId, "price", priceUi, `(${decimalsA} vs ${decimalsB} decimals)`);
}
```

### Add liquidity

`mintA`, `vaultA`, `userTokenA` and their B-side pairs are the accounts from the pool you created above. Amounts are **raw base units**, and a deposit is valued at the bin's own price, so a 6-decimal stablecoin leg and a 9-decimal token leg are not comparable by their raw counts:

```typescript
async function addLiquidity(pool: PublicKey, binId: number, amountA: bigint, amountB: bigint) {
  const arrayIndex = binArrayIndex(binId);

  await program.methods
    .addLiquidity(new anchor.BN(arrayIndex), binId, new anchor.BN(amountA.toString()), new anchor.BN(amountB.toString()))
    .accounts({
      pool,
      binArray: binArrayPda(pool, arrayIndex),
      position: positionPda(pool, provider.wallet.publicKey, binId),
      mintA,
      mintB,
      vaultA,
      vaultB,
      userTokenA,
      userTokenB,
      user: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    // Remaining accounts: only the Token-2022 transfer-hook accounts, if any.
    .rpc();
}
```

### Swap

The swap walks bin arrays that you supply, starting at the pool's active bin, so the caller has to decide up front how far the price may travel and pass those arrays - writable, in walk order:

```typescript
/**
 * Every bin array a trade may walk, in walk order: ascending when selling A
 * (the price rises), descending when selling B. The program stops at the first
 * array that fills the trade, so extra arrays are harmless; a missing one is
 * `InsufficientLiquidity`.
 */
function walkAccounts(pool: PublicKey, activeBinId: number, isAToB: boolean, depth = 3): PublicKey[] {
  const start = binArrayIndex(activeBinId);
  const dir = isAToB ? 1 : -1;
  const accounts: PublicKey[] = [];
  for (let i = 0; i < depth; i++) {
    accounts.push(binArrayPda(pool, start + i * dir));
  }
  return accounts;
}

async function swap(pool: PublicKey, amountIn: bigint, minAmountOut: bigint, isAToB: boolean) {
  const state = await program.account.pool.fetch(pool);

  // Rail accounts: real ones for enabled rails, the program ID sentinel for
  // disabled ones. The snippet below shows a fully-unwired pool (all rails
  // off) — wire the real accounts the same way when a pool was initialized
  // with a policy. `trade_id` is 0 unless settlement routing is on.
  const sentinel = AMM_PROGRAM_ID; // stands in for every disabled rail
  const tradeId = new anchor.BN(0);

  await program.methods
    .swap(
      new anchor.BN(amountIn.toString()),
      new anchor.BN(minAmountOut.toString()),
      isAToB,
      tradeId,
    )
    .accounts({
      pool,
      mintA,
      mintB,
      vaultA,
      vaultB,
      userTokenA,
      userTokenB,
      user: provider.wallet.publicKey,
      kycConfig: sentinel,
      userKycAccount: sentinel,
      oracleState: sentinel,
      hooksAuthority: sentinel,
      custodyTokenAccount: sentinel,
      settlementConfig: sentinel,
      settlementProgram: sentinel,
      settlementVault: sentinel,
      settlementVaultToken: sentinel,
      associatedTokenProgram: sentinel,
      systemProgram: SystemProgram.programId,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .remainingAccounts(
      walkAccounts(pool, state.activeBinId, isAToB).map((pubkey) => ({
        pubkey,
        isWritable: true, // the walk writes every bin it touches
        isSigner: false,
      }))
    )
    .rpc();
}
```

### Remove liquidity

A withdrawal never has to walk anything: the position holds its shares in the bin's own ledger, and the bin's reserves are paid out proportionally.

```typescript
async function removeLiquidity(pool: PublicKey, binId: number, shares: bigint, minA: bigint, minB: bigint) {
  const arrayIndex = binArrayIndex(binId);

  await program.methods
    .removeLiquidity(new anchor.BN(arrayIndex), binId, new anchor.BN(shares.toString()), new anchor.BN(minA.toString()), new anchor.BN(minB.toString()))
    .accounts({
      pool,
      binArray: binArrayPda(pool, arrayIndex),
      position: positionPda(pool, provider.wallet.publicKey, binId),
      mintA,
      mintB,
      vaultA,
      vaultB,
      userTokenA,
      userTokenB,
      user: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();
}
```

### Integration pitfalls

1. **Array order is part of the interface.** A swap passing arrays out of walk order fails with `BinArrayIndexMismatch`; the first array must be the one holding `pool.active_bin_id`.
2. **Re-read the pool between swaps.** `active_bin_id` moves with every trade, so a cached array list goes stale - and the first array has to be the *current* active bin's array.
3. **Arrays must be writable and canonical.** They are matched against the `[b"bin_array", pool, index]` PDA, so a hand-made account with favourable reserves is rejected.
4. **Hook accounts go after the arrays.** The swap tells arrays from hook accounts by owner, so a remaining account owned by the DLMM program is treated as an array.
5. **`array_index` arguments are checked against `bin_array_index(bin_id)`.** Both `add_liquidity` and `remove_liquidity` take it because it is part of the array's seeds; it must be the computed index, never a guess.
6. **Everything is raw units.** Deposit, output floors and oracle-free prices are all in base units; multiply by `10^decimals` for a UI amount.
7. **A paused pool still allows withdrawals.** `remove_liquidity` is deliberately not gated on `paused`, so a pause never traps LP funds.
8. **Rail accounts are mandatory either way.** Every `swap` must pass all rail accounts: real ones for enabled rails, the AMM program ID in their place for disabled ones. There is no default sentinel account on Solana, so the program's own ID stands in for "unused".

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `InvalidBinStep` | `Invalid bin step` | `bin_step` is 0 or greater than `MAX_BIN_STEP` (100). |
| `FeeTooHigh` | `Fee too high` | `base_fee_bps` exceeds `MAX_BASE_FEE_BPS` (100). |
| `MathOverflow` | `Math overflow` | Checked arithmetic computation underflowed or overflowed. |
| `SlippageExceeded` | `Slippage exceeded` | Received swap or withdrawal output is below `min_amount_out` / `min_amount_a` / `min_amount_b`. |
| `InsufficientLiquidity` | `Insufficient liquidity to fill the swap` | Total liquidity across supplied bin arrays is insufficient for trade size. |
| `BinNotInitialized` | `Bin has no price: its bin array was never initialized` | Target bin's `price_x64` is 0. |
| `ZeroAmount` | `Amount must be greater than zero` | Provided input/deposit amount is zero. |
| `InsufficientShares` | `Not enough LP shares` | Attempted to burn more liquidity shares than available in `Position`. |
| `Unauthorized` | `Position does not belong to this owner` | Signer does not match `position.owner`. |
| `PoolPaused` | `Pool is paused` | Attempted swap or liquidity addition while pool is paused. |
| `UnsupportedDecimals` | `Mint decimals above the supported maximum` | Token decimals exceed `MAX_MINT_DECIMALS` (9). |
| `NoBinArrays` | `No bin array was supplied to swap through` | Remaining accounts contained no `BinArray` belonging to this program. |
| `WrongBinArrayPool` | `Bin array does not belong to this pool` | `bin_array.pool` does not match the target `pool`. |
| `InvalidBinArray` | `Bin array is not the canonical PDA for its index` | Account address does not match expected BinArray PDA. |
| `BinArrayIndexMismatch` | `Bin arrays must be supplied in the order the swap walks them` | Bin arrays passed to `swap` were not strictly ordered by index in trade walk direction. |
| `BinNotInArray` | `Bin does not belong to the given bin array` | Target `bin_id` maps to a different array index than supplied. |
| `BinArrayOutOfRange` | `Bin array is too far from the active bin` | New array index is $> 16$ arrays away from `active_bin_id`. |
| `BinArrayNotWritable` | `Bin array must be passed as writable` | BinArray account in remaining accounts was passed as read-only. |
| `InvalidSettlementSlots` | `Settlement routing policy is inconsistent...` | `settlement_slots` must be non-zero iff `hooks_authority` is set (init). |
| `WrongKycConfig` | `KYC config account does not match the pool's recorded KYC policy` | The passed `kyc_config` is not the pool's configured KYC gate config. |
| `WrongOracleState` | `Oracle state account does not match the pool's recorded oracle policy` | The passed `oracle_state` is not the `[b"oracle", pool]` PDA. |
| `WrongHooksAuthority` | `Hooks authority account is not the hooks program's hook_authority PDA` | The passed `hooks_authority` is not the hooks program's `[b"hook_authority"]` PDA. |
| `RailAccountUnexpected` | `A rail account must be this program's own ID when its rail is disabled` | A disabled rail's account was not passed as the AMM program ID. |
| `InvalidTradeId` | `Trade id must be non-zero when settlement routing is on, zero when off` | `trade_id` disagrees with the pool's settlement policy. |

---

## 🧪 Testing & Verification

The program includes comprehensive inline unit tests covering bin pricing math, bin array tiling, Euclidean division edge cases, multi-bin swap fills, direction walking, dynamic fee calculations, decimal scaling, and rounding rules.

Run tests via Anchor or standard Cargo:
```bash
cargo test-sbf
# or
cargo test --lib
```
