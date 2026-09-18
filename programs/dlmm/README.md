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

## 🛠️ Instructions API

### 1. `initialize_pool`
Creates the main `Pool` account, token vaults, and the initial `BinArray` containing the specified `active_bin_id`.

* **Parameters**:
  * `array_index: i64`: Index of the array holding `active_bin_id` (`bin_id / 70`).
  * `active_bin_id: i32`: Initial active bin ID where swapping will start.
  * `bin_step: u16`: Bin step size in basis points ($1 \le \text{bin\_step} \le 100$).
  * `base_fee_bps: u16`: Base swap fee in basis points ($\le 100$).
  * `authority: Pubkey`: Pool administrator key (e.g., multisig/DAO).
* **Accounts**: `payer`, `mint_a`, `mint_b`, `pool` (init), `bin_array` (init), `vault_a` (init), `vault_b` (init), `token_program`, `system_program`.

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
Swaps an input amount of Token A for Token B (or vice versa), walking through contiguous bin arrays starting at `active_bin_id`.

* **Parameters**:
  * `amount_in: u64`: Exact amount of input token provided.
  * `min_amount_out: u64`: Minimum required amount of output token (slippage check).
  * `is_a_to_b: bool`: `true` to swap A $\rightarrow$ B (moves bin index up); `false` to swap B $\rightarrow$ A (moves bin index down).
* **Accounts**: `pool` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`.
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

---

## 🧪 Testing & Verification

The program includes comprehensive inline unit tests covering bin pricing math, bin array tiling, Euclidean division edge cases, multi-bin swap fills, direction walking, dynamic fee calculations, decimal scaling, and rounding rules.

Run tests via Anchor or standard Cargo:
```bash
cargo test-sbf
# or
cargo test --lib
```
