# Maverick Dynamic Distribution AMM (DDAMM) Program

An Anchor-based Solana program implementing a **Dynamic Distribution AMM** in the style of Maverick: bin-based liquidity that *moves with the price* instead of sitting idle once the market has traded through it.

It shares the DLMM program's bin model - fixed Q64.64 prices per bin, bins packed into `BinArray` accounts, Token-2022 aware transfers, decimals-aware pricing - and adds the part that makes it Maverick: every bin has a **kind** that decides whether it follows the price, and a permissionless `move_bins` instruction that relocates out-of-range liquidity next to the active price without a swap.

---

## 🌟 Overview

Liquidity is discretized into **bins**. Each bin quotes one fixed exchange rate and holds its own reserves, so a swap fills at that rate with zero slippage until the bin's output reserve runs out, then crosses to the next bin. A trade walks the bin ladder, and the active bin ends up where the trade finished.

That model has a known cost: once the price has traded through a bin, that bin is out of range - a bin below the active bin holds only token B, a bin above it holds only token A - and its liquidity earns nothing until the price comes back.

Maverick's answer is to move it:

| Bin kind | Promise to the LP | When it moves |
| :--- | :--- | :--- |
| `Static` | The price it quotes is **fixed for the life of the liquidity** | Never |
| `Right` | Follows a rising price to the right | When the price is above it |
| `Left` | Follows a falling price to the left | When the price is below it |
| `Both` | Tracks the price in either direction | Whenever it is out of range |

### Key characteristics

* **Dynamic distribution**: `move_bins` carries out-of-range liquidity to a bin between it and the active bin, so idle capital is redeployed next to the price it will trade at.
* **Permissionless with on-chain guards**: anyone can call `move_bins` (that is the keeper job), and every guard that matters - out of range, direction, distance, kind, destination - is enforced by the program. Static bins never move.
* **Swap-free moves**: a move never trades and never moves tokens. It re-prices the source bin's reserves at the destination's price, mints the destination shares they are worth, and links the two bins.
* **No dilution**: shares are minted at the destination's own value per share (`moved_value * dest_supply / dest_value`), which leaves the existing LPs' claim untouched to the last unit, and gives the moved bin's LPs exactly the value they brought.
* **The move chain**: a moved bin keeps a link (destination bin id + share ratio) so LPs can always trace and withdraw their claim, however many times their bin has moved. `remove_liquidity` follows the chain.
* **Bin arrays**: bins are packed 50 to a `BinArray` account. 50 rather than the DLMM's 70 because a Maverick bin carries its kind and its move link.
* **Token-2022 & transfer hooks**: transfers are measured rather than assumed (transfer-fee extensions), and hook accounts are passed through transparently for RWA compliance checks.
* **Decimals-aware pricing**: raw-unit prices are scaled by `10^(decimals_a - decimals_b)`, so a 6-decimal stablecoin against a 9-decimal RWA prices correctly.
* **Dynamic fees**: the base fee is multiplied up while a bin is one-sided, when LPs are most exposed.
* **Non-trapping pause**: pausing blocks swaps, deposits and moves, but `remove_liquidity` keeps working.
* **Institutional wiring rails**: like the CLMM and DLMM, pools can fix a KYC gate, a TWAP circuit breaker and T+N settlement routing at birth — see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement).

---

## 📐 Mathematical Model

### 1. Bin price

Exactly as in the DLMM program:

$$P_{\text{whole}} = 1.0001^{(\text{bin\_id} \times \text{bin\_step})}, \qquad P_{\text{raw}} = P_{\text{whole}} \times 10^{(\text{decimals\_a} - \text{decimals\_b})}$$

A bin's `price_x64` is `P_raw` in Q64.64, i.e. raw token A per raw token B, and it is the price every amount in the bin is valued at.

### 2. Value and shares

Shares are denominated in the deposit's **value in token A** at the bin's price:

$$\text{value} = \text{amount\_a} \cdot 2^{64} + \text{amount\_b} \cdot P_{\text{raw}}$$

The first deposit into a bin defines the share unit (`shares = value`); later deposits are scaled by the existing supply (`shares = value * supply / reserve_value`), so the ratio of shares to value never moves.

### 3. Moving a bin (the merge)

`move_bins` merges the source bin into a destination nearer the active bin:

$$\text{moved\_value} = \text{value}(\text{source reserves at the destination's price})$$

$$\text{minted} = \begin{cases} \text{moved\_value} & \text{destination is empty} \\ \text{moved\_value} \times \text{dest\_supply} / \text{dest\_value} & \text{otherwise} \end{cases}$$

$$\text{share\_ratio} = \text{minted} \times 2^{64} / \text{source\_supply} \quad (\text{Q64.64})$$

The destination's value per share is unchanged:

$$\frac{V + \text{moved\_value}}{S + \text{moved\_value} \cdot S / V} = \frac{V}{S}$$

so existing LPs are not diluted, and the moved bin's LPs hold `share_ratio` destination shares per share they deposited - the same value they brought, re-priced at the price the bin will now trade at. All the divisions round down, so the error is at most one unit per hop and always in the pool's favour.

### 4. Following the chain

A bin that has already moved can be moved again from its destination, so links form a chain. Withdrawal converts the whole position along the chain - `shares' = shares * ratio / 2^64` per hop - and then slices the converted claim:

$$\text{burn} = \text{terminal\_shares} \cdot \text{shares} / \text{position\_shares}$$

Converting the whole position first and slicing after is what makes the parts of a position add back up to the whole, so a full withdrawal empties the claim exactly.

### 5. Dynamic fee

$$\text{Fee BPS} = \text{base\_fee\_bps} + \frac{\text{base\_fee\_bps} \times \text{utilization}}{100}$$

* **Balanced bin** (reserves A > 0 and B > 0): utilization = 50%
* **One-sided bin**: utilization = 90%

The fee is taken from what actually arrived and credited to the bin the trade started in, so it accrues to that bin's LPs, wherever the bin later moves:

$$fee = \Big\lfloor \frac{amount\_received \times fee\_bps}{10\,000} \Big\rfloor \qquad\quad amount\_to\_fill = amount\_received - fee$$

### 6. Swapping: quoting and rounding

A swap fills each bin at that bin's fixed raw price, spending the input down bin by bin and crossing whenever a bin runs out of the output token. Selling A (price rises, bins walk up):

$$out_B = \Big\lfloor \frac{in_A \times 2^{64}}{P_{raw}} \Big\rfloor \qquad\qquad in\_needed = \Big\lceil \frac{out \times P_{raw}}{2^{64}} \Big\rceil$$

Selling B mirrors it (`out_A = floor(in_B · P_raw / 2^64)`, `in_needed = ceil(out · 2^64 / P_raw)`). Outputs round down and the input that drains a bin rounds up, so a drained bin is left at exactly zero and a top-up can never cost more than it spent.

| Operation | Rounds | Why |
| :--- | :--- | :--- |
| Swap output | down | The pool never pays out more than its price says |
| Input needed to drain a bin | up | A drained bin is left holding no sliver of the output token |
| Swap fee | down | `u64` fee taken from what arrived |
| Deposit shares | down | A deposit cannot mint itself value |
| Withdrawal payout | down | The pool never pays out more than it holds |
| `moved_value * dest_supply / dest_value` | down (twice) | Merging cannot inflate the destination's value per share |
| Share ratio `minted * 2^64 / source_supply` | down | Converting a claim cannot create shares |

### 7. Bin indexing and the walk

Bins tile the number line and negative ids belong to negative arrays, with Euclidean division exactly as in the DLMM program:

$$index = \Big\lfloor \frac{bin\_id}{50} \Big\rfloor_{Euclidean} \qquad\quad offset = bin\_id \bmod 50$$

so bin 0 is offset 0 of array 0, bin 49 is offset 49 of array 0, bin 50 is offset 0 of array 1, and bin -1 is offset 49 of array -1. A swap walks the arrays the caller supplies, in walk order, starting at the array holding `active_bin_id`.

---

## 🔀 How a move is guarded

`move_bins(from_array_index, from_bin_id, to_bin_id)` is refused unless all of:

1. The source bin holds liquidity, holds reserves, and has not already moved.
2. The source's kind allows the direction - `Static` never moves, `Right` only right, `Left` only left, `Both` either way.

Checks 1 and 2 are made **before the destination's array is even located**, so a bin that could never move is refused without the caller having to pass a valid destination: `StaticCannotMove` and `BinAlreadyMoved` do not depend on what is in the remaining accounts. "Already moved" is also asked before "holds reserves", because a bin that has moved keeps its supply but has its reserves zeroed - the other order would report it as merely empty.
3. The source is **out of range**: below the active bin for a move right, above it for a move left. The bin the price is currently trading in can never be moved.
4. The destination is **between the source and the active bin** (the active bin itself included), so a move always brings liquidity closer to the price.
5. The destination is within `MAX_SHIFT_BINS` (one array's width) of the source.
6. The destination is empty, or of the same kind. A kind belongs to a bin, so mixing kinds inside one bin would silently change the terms of the liquidity already in it.
7. The pool is not paused.

Because a move only relocates reserves and mints shares against them - and requires the source to be out of range - it cannot be used to take value out of the pool, so it needs no authority.

### Worked example

A pool at parity, `bin_step = 10`, with the active bin at 10 (price ≈ 1.010 A per B):

| Step | Bin 0 (kind `Right`) | Bin 10 (active, kind `Both`) |
| :--- | :--- | :--- |
| Start | 1 000 B, 1 020 Q64 shares (1 000 B sold through it as the price rose) | 1 000 A + 1 000 B, 2 010 Q64 shares |
| `move_bins(0, 0, 10)` | reserves 0, supply 1 020 Q64, `move_state = {10, ratio}` | 1 000 A + 2 000 B, shares + 1 010 Q64 |
| `remove_liquidity(0)` | - | pays the moved bin's LPs their proportion, converted at `ratio` |

The destination's value per share is unchanged by the merge - about 2 010 A of value over 2 010 Q64 shares before, and 3 020 over 3 020 Q64 after (the bin's price is 1.010049, not exactly 1.01) - and the moved bin's LPs redeem the value of their 1 000 B at bin 10's price.

### Worked example: two hops, then a withdrawal

Links compose, so an LP's claim survives being moved twice. A position of **1 000 shares in bin 0** whose bin is merged into bin 5 at a ratio of `2 · 2^64`, and whose bin 5 liquidity is later merged into bin 9 at a ratio of `0.5 · 2^64`:

| Step | Position shares | Where they are worth it |
| :--- | ---: | :--- |
| Deposited | 1 000 | bin 0 |
| Hop 1: bin 0 → bin 5, ratio 2 | - | `1 000 × 2 = 2 000` shares of bin 5 |
| Hop 2: bin 5 → bin 9, ratio 0.5 | - | `2 000 × 0.5 = 1 000` shares of bin 9 |
| Withdraw 400 (`400/1000` of the position) | 600 | `1 000 × 400/1000 = 400` shares of bin 9 are burned |
| Withdraw the rest | 0 | the remaining 600 terminal shares are burned exactly |

The position account never changes its `bin_id` - it stays 0, the bin that was deposited into - and `remove_liquidity` walks the two links every time it is called. The remaining accounts for that call are the bin arrays of bins 5 and 9 if they live outside bin 0's array, and only the terminal bin's reserves are touched.

---

## ⚙️ Program Constants

| Parameter | Value | Description |
| :--- | :--- | :--- |
| `BINS_PER_ARRAY` | `50` | Bins per `BinArray` (a Maverick bin is 70 bytes, so 50 keeps the array under 4 KiB) |
| `MAX_BIN_STEP` | `100` | Maximum bin step in basis points (1.00%) |
| `MAX_BASE_FEE_BPS` | `100` | Maximum base fee in basis points (1.00%) |
| `MAX_BIN_ARRAY_SPAN` | `16` | How far from the active bin a new bin array may be created |
| `MAX_MINT_DECIMALS` | `9` | Highest mint decimals supported for decimal scaling |
| `MAX_SHIFT_BINS` | `50` | How far a single move may carry liquidity |
| `BPS_DENOMINATOR` | `10,000` | Basis-point denominator |

---

## 🔑 Account Architecture & PDAs

```
                     +------------------------+
                     |      Pool Account      |
                     | (PDA: [pool, A, B])    |
                     +-----------+------------+
                                 |
       +-------------------------+-------------------------+
       |                         |                         |
+------v-------+        +--------v-------+        +--------v--------+
|  Vault A     |        |  Vault B       |        |  BinArray       |
| [vault_a,P,A]|        | [vault_b,P,B]  |        | [bin_array,P,i] |
+--------------+        +----------------+        +--------+--------+
                                                            |
                                                   +--------v--------+
                                                   | Bins [0..49]    |
                                                   |  - price_x64    |
                                                   |  - reserve_a/b  |
                                                   |  - supply       |
                                                   |  - kind         |
                                                   |  - move_state   |
                                                   +-----------------+
```

| Account | PDA Seeds | Description |
| :--- | :--- | :--- |
| **Pool** | `[b"pool", mint_a.key(), mint_b.key()]` | Pool parameters, active bin, authority, pause flag, decimals. |
| **Vault A** | `[b"vault_a", pool.key(), mint_a.key()]` | Pool-owned token A vault. |
| **Vault B** | `[b"vault_b", pool.key(), mint_b.key()]` | Pool-owned token B vault. |
| **BinArray** | `[b"bin_array", pool.key(), index.to_le_bytes()]` | 50 contiguous bins. |
| **Position** | `[b"position", pool.key(), owner.key(), bin_id.to_le_bytes()]` | An LP's shares in the bin they deposited into (not where those shares have since moved). |

---

## 🔌 Pool Wiring: KYC, Oracle, Settlement

Like the CLMM and the DLMM, every Maverick pool fixes its **wiring policy** at `initialize_pool`, and every later swap runs the same three optional rails around the bin math. A field left as `Pubkey::default()` disables its rail:

| Rail | Pool field | Enabled behaviour per swap |
| :--- | :--- | :--- |
| **KYC** | `kyc_config` | CPIs `kyc_gate::verify_kyc` for the trader before any math runs — no attestation, no trade. |
| **Oracle** | `oracle_state` | CPIs `oracle::check_deviation` on the active bin's price pre-trade, then `oracle::record_observation` with the settled post-trade bin price, so the 30-minute TWAP stays current. Both are signed by the pool PDA — the oracle answers only the pool it belongs to. |
| **Settlement** | `hooks_authority` + `settlement_slots` | Swap output is NOT paid to the trader. It is routed into the hook authority's custody ATA and swept into the T+N settlement escrow (`hooks::after_swap` → `settlement::initiate_settlement`); the trader withdraws after `settlement_slots` with a Transfer Agent signature. `min_amount_out` is still checked against the exact amount leaving the pool's vault. The custody account is **pinned by derivation**: it must be the hook authority's ATA for the output mint, under the mint's token program, or the swap fails with `InvalidCustodyAccount`. |

**Ordering inside `swap`:** `0` policy/account checks → `0a` KYC CPI → `0b` circuit-breaker CPI → `1` pull input (measured) → `2` bin walk + fees → `3` payout (to trader or custody) → `4` oracle observation → `5` settlement escrow CPI. Every step after the walk can only revert the whole transaction; there is no partial state.

**Sentinel convention.** The rail accounts are mandatory either way. An enabled rail's accounts are validated against the pool's recorded policy (config equality, or PDA re-derivation for the oracle and hook authority). A disabled rail's accounts must all be **this AMM program's own ID** — Solana has no null account, so the program ID is the "unused here" sentinel; passing anything else reverts with `RailAccountUnexpected`.

**The hook authority is writable.** `hooks_authority` is passed writable (and as the sentinel when the rail is off): the settlement program charges the trade's vault and escrow to that PDA, and a CPI can only pass on the privileges the caller's instruction already holds.

**The rail's program travels with the rail.** `swap` also carries `kyc_program`, `oracle_program` and `hooks_program` — the real program id for each enabled rail, the executing program's own id otherwise. Every rail is reached by CPI, and a CPI target has to appear in the instruction's account list for the runtime to resolve it: leaving the oracle on but its program out is `Unknown program`, not a skipped check.

**Why the oracle price is recomputed, not read.** Bin prices are deterministic: `priced_bins` prices every bin of an array from `bin_step` and the mint decimals at creation, and afterwards only reserves move. The rail therefore calls `bin_price_raw(active_bin_id, ...)`, a pure function proven equal to what the array stores (the unit test compares it against `priced_bins` for both decimal scalings, several steps and negative bins) — no account read, and no trust in the caller to supply the price.

**Moves are unaffected.** `move_bins` and `add_liquidity` sit outside the rails by design: a move re-prices liquidity without trading, and a deposit is not a trade. Only `swap` routes value out of the pool, so only `swap` is KYC-gated, oracle-checked and settlement-routed.

**Settlement custody.** The output lands in the hook authority's ATA for the output mint, pinned by ATA derivation and re-checked by the settlement program as the escrow sweep's source. The pool's shared vaults are never the source of an escrow deposit, and the trader never momentarily holds the output.

---

## 🛠️ Instructions API

### 1. `initialize_pool`

Creates the `Pool`, both vaults and the first `BinArray`, with every bin in it priced.

* **Parameters**: `array_index: i64` (must be `bin_array_index(active_bin_id)`), `active_bin_id: i32`, `bin_step: u16` (1–100), `base_fee_bps: u16` (≤ 100), `authority: Pubkey`, plus the wiring policy — `kyc_config`, `max_deviation_bps` (`0` disables the oracle breaker), `hooks_authority` (`Pubkey::default()` disables a rail) and `settlement_slots` (non-zero iff `hooks_authority` is set). See [Pool Wiring](#-pool-wiring-kyc-oracle-settlement).
* **Accounts**: `payer`, `mint_a`, `mint_b`, `pool` (init), `bin_array` (init), `vault_a` (init), `vault_b` (init), `oracle_state` (the `[b"oracle", pool]` PDA when the breaker is on — created here with the pool PDA signing, so the threshold and owner cannot be front-run; pass the program-ID sentinel to leave it off), `kyc_program` (optional), `oracle_program` (optional), `hooks_program` (optional), `token_program`, `system_program`. The optional program accounts must be the real program whenever their rail is enabled (Anchor's `Option` sentinel: omit by passing the executing program's own ID).

Bins start unpriced by kind: a bin's kind is chosen by the first LP to fund it.

### 2. `initialize_bin_array`

Permissionlessly creates another priced `BinArray`, within `MAX_BIN_ARRAY_SPAN` arrays of the active bin.

* **Parameters**: `index: i64`.
* **Accounts**: `payer`, `pool`, `bin_array` (init), `system_program`.

### 3. `add_liquidity`

Deposits both tokens into a bin and mints LP shares.

* **Parameters**: `array_index: i64`, `bin_id: i32`, `kind: BinKind`, `amount_a: u64`, `amount_b: u64`.
* **Accounts**: `pool`, `bin_array` (mut), `position` (init_if_needed), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`, `system_program`.
* **Remaining accounts**: Token-2022 transfer hook accounts.
* **Rules**: the pool must not be paused; a moving kind (`Right`/`Left`/`Both`) requires the pool to already have static liquidity; the bin must not have been moved; a funded bin's kind must match the deposit's.

### 4. `remove_liquidity`

Burns shares and pays out the claim, following the bin's move chain to the bin that holds the assets.

* **Parameters**: `array_index: i64`, `bin_id: i32`, `shares: u128`, `min_amount_a: u64`, `min_amount_b: u64`.
* **Accounts**: `pool`, `bin_array` (mut), `position` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), `token_program`.
* **Remaining accounts**: every bin array the chain runs through (writable, matched by canonical PDA, order irrelevant), then the transfer-hook accounts.
* **Note**: works while the pool is paused.

### 5. `move_bins`

Relocates an out-of-range bin's liquidity to a bin nearer the active price. Permissionless.

* **Parameters**: `from_array_index: i64`, `from_bin_id: i32`, `to_bin_id: i32`.
* **Accounts**: `pool` (mut), `from_array` (mut).
* **Remaining accounts**: the destination bin's array, if it is not in `from_array`.
* **Note**: no tokens move; the source bin's reserves are zeroed and its `move_state` link records where its shares went.
* **Heap**: the destination array is read and written **in place** - the one bin is located by its fixed-stride offset (`bin_byte_offset`) and read/written through `read_bin_at` / `write_bin_at` - instead of being deserialized into a second `Account<BinArray>`. The SBF heap is a 32 KiB arena whose allocator never reclaims (`dealloc` is a no-op), so allocations are charged for the whole instruction however briefly they live; materialising a second 50-element `Vec<Bin>` alongside the source array is enough to abort the instruction with `memory allocation failed, out of memory`. Everywhere that reaches a bin in *another* array - the cross-array hop of `remove_liquidity` and the move-chain walk alike - goes through the same in-place helpers, and `hop_array_info` validates the array's pool, index and bin count straight out of its bytes rather than by deserializing it just to throw it away.

### 6. `swap`

Swaps through the bin arrays, starting at the active bin and crossing as bins run dry. Runs the pool's wiring rails around the math (see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement)).

* **Parameters**: `amount_in: u64`, `min_amount_out: u64`, `is_a_to_b: bool`, `trade_id: u64` (non-zero iff the settlement rail is on).
* **Accounts**: `pool` (mut), `mint_a`, `mint_b`, `vault_a` (mut), `vault_b` (mut), `user_token_a` (mut), `user_token_b` (mut), `user` (signer), plus the rail accounts `kyc_config` (mut), `user_kyc_account`, `oracle_state` (mut), `hooks_authority`, `custody_token_account` (mut), `settlement_config`, `settlement_program`, `settlement_vault` (mut), `settlement_vault_token` (mut), `associated_token_program`, then `token_program`, `system_program`. A disabled rail's accounts are passed as the **AMM program ID** — there is no null account on Solana, so the program's own ID is the unused sentinel.
* **Remaining accounts**: the `BinArray` accounts to walk, in walk order (ascending index when selling A, descending when selling B), then the transfer-hook accounts.

### 7. `set_paused` / 8. `set_authority`

Admin-only, signed by `pool.authority`: pause or resume trading, and rotate the authority. `set_authority` rejects `Pubkey::default()` (`InvalidAuthority`) — the zero key is not a wallet, so accepting it would burn the admin role and leave the pool permanently unpausable and unrotatable.

---

## 📄 Data Structures

```rust
pub enum BinKind { Static, Right, Left, Both }

pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub bin_step: u16,               // basis points, 1..=100
    pub base_fee_bps: u16,           // basis points, 0..=100
    pub active_bin_id: i32,          // bin the next swap starts in
    pub authority: Pubkey,           // admin: pause + rotate
    pub paused: bool,
    pub has_static_baseline: bool,   // any static bin funded yet?
    pub decimals_a: u8,
    pub decimals_b: u8,
    pub kyc_config: Pubkey,          // KYC gate config, default = rail off
    pub oracle_state: Pubkey,        // oracle state PDA, default = rail off
    pub hooks_authority: Pubkey,     // hooks authority PDA, default = rail off
    pub settlement_slots: u64,       // T+N horizon in slots (settlement rail)
    pub bump: u8,
    pub vault_a_bump: u8,
    pub vault_b_bump: u8,
}

pub struct MoveState {
    pub moved: bool,         // has this bin been merged into another?
    pub to_bin_id: i32,      // where the shares went (meaningful when moved)
    pub share_ratio: u128,   // destination shares per share here, Q64.64
}

pub struct Bin {
    pub price_x64: u128,        // raw A per raw B, Q64.64
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub supply: u128,           // LP shares denominated in this bin
    pub kind: BinKind,
    pub move_state: MoveState,  // moved flag + hop, fixed size
}

pub struct BinArray {
    pub pool: Pubkey,
    pub index: i64,
    pub bins: Vec<Bin>,         // exactly BINS_PER_ARRAY
}
```

**A `Bin` is fixed-stride once serialised.** `MoveState` is a fixed-size struct
(`moved` 1 + `to_bin_id` 4 + `share_ratio` 16 = **21** bytes), so every bin is
`Bin::LEN` = **70** bytes whether or not it has moved. `space = BinArray::LEN`
reserves `BINS_PER_ARRAY * Bin::LEN` exactly, with no spare room to grow into.

Off-chain readers can index with a constant stride: a bin starts at
`ARRAY_BINS + index * 70` and `move_state.moved` (offset 49) says whether the
hop that follows is meaningful. Nothing a bin's history does can shift the ones
after it. On-chain, `bin_byte_offset` is the same plain multiplication, which is
what lets the program reach one bin without materialising the whole vector.

```rust

pub struct Position {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub bin_id: i32,            // the bin deposited into, not where it moved
    pub liquidity_shares: u128,
}
```

---

## 🔌 TypeScript Integration

The snippets use the same stack as the other program docs: `@coral-xyz/anchor` with the IDL types from `target/types/maverick.ts`, plus `@solana/web3.js` and `@solana/spl-token`.

### PDAs, kinds and bin maths

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import { TOKEN_2022_PROGRAM_ID } from "@solana/spl-token";
import { Maverick } from "./target/types/maverick";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Maverick as Program<Maverick>;

/** BINS_PER_ARRAY in the program. */
const BINS_PER_ARRAY = 50;

const binArrayIndex = (binId: number) => Math.floor(binId / BINS_PER_ARRAY);
const binOffset = (binId: number) => ((binId % BINS_PER_ARRAY) + BINS_PER_ARRAY) % BINS_PER_ARRAY;

const i64 = (n: number) => { const b = Buffer.alloc(8); b.writeBigInt64LE(BigInt(n)); return b; };
const i32 = (n: number) => { const b = Buffer.alloc(4); b.writeInt32LE(n); return b; };

const poolPda = (mintA: PublicKey, mintB: PublicKey) =>
  PublicKey.findProgramAddressSync([Buffer.from("pool"), mintA.toBuffer(), mintB.toBuffer()], program.programId)[0];
const binArrayPda = (pool: PublicKey, index: number) =>
  PublicKey.findProgramAddressSync([Buffer.from("bin_array"), pool.toBuffer(), i64(index)], program.programId)[0];
const vaultPda = (pool: PublicKey, mint: PublicKey, side: "vault_a" | "vault_b") =>
  PublicKey.findProgramAddressSync([Buffer.from(side), pool.toBuffer(), mint.toBuffer()], program.programId)[0];
const positionPda = (pool: PublicKey, owner: PublicKey, binId: number) =>
  PublicKey.findProgramAddressSync([Buffer.from("position"), pool.toBuffer(), owner.toBuffer(), i32(binId)], program.programId)[0];

/** Anchor encodes a Rust enum as a single-key object. */
const kind = {
  static: { static: {} },
  right: { right: {} },
  left: { left: {} },
  both: { both: {} },
} as const;

/** A bin's kind comes back as `{ right: {} }`, and `movedTo` as null or { toBinId, shareRatio }. */
const kindOf = (bin: { kind: object }) => Object.keys(bin.kind)[0];
```

### Create a pool and fund the Static baseline

A moving kind is only accepted once the pool has static liquidity, and each bin's kind is fixed by the first LP to fund it:

```typescript
async function createPoolAndSeed(mintA: PublicKey, mintB: PublicKey) {
  const pool = poolPda(mintA, mintB);
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

  // Static liquidity anchors the price; nothing else can be funded before it.
  await addLiquidity(pool, activeBinId, kind.static, 1_000_000n, 1_000_000n);

  // A bin two below the price, in Right mode, so it will follow the price up.
  await addLiquidity(pool, activeBinId - 2, kind.right, 0n, 1_000_000n);
}

async function addLiquidity(pool: PublicKey, binId: number, binKind: object, amountA: bigint, amountB: bigint) {
  const arrayIndex = binArrayIndex(binId);

  await program.methods
    .addLiquidity(new anchor.BN(arrayIndex), binId, binKind as any, new anchor.BN(amountA.toString()), new anchor.BN(amountB.toString()))
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
    .rpc();
}
```

### Swap

Identical in shape to the DLMM's swap: the arrays are passed in walk order (ascending when selling A, descending when selling B) and the first one must hold `active_bin_id`.

```typescript
function walkAccounts(pool: PublicKey, activeBinId: number, isAToB: boolean, depth = 3): PublicKey[] {
  const start = binArrayIndex(activeBinId);
  const dir = isAToB ? 1 : -1;
  return Array.from({ length: depth }, (_, i) => binArrayPda(pool, start + i * dir));
}

async function swap(pool: PublicKey, amountIn: bigint, minAmountOut: bigint, isAToB: boolean) {
  const state = await program.account.pool.fetch(pool);

  // Rail accounts: real ones for enabled rails, the program ID sentinel for
  // disabled ones. Shown fully unwired; wire the real accounts the same way
  // when a pool was initialized with a policy. `trade_id` stays 0 unless
  // settlement routing is on.
  const sentinel = pool;
  const tradeId = new anchor.BN(0);

  await program.methods
    .swap(new anchor.BN(amountIn.toString()), new anchor.BN(minAmountOut.toString()), isAToB, tradeId)
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
      walkAccounts(pool, state.activeBinId, isAToB).map((pubkey) => ({ pubkey, isWritable: true, isSigner: false }))
    )
    .rpc();
}
```

### Move bins (the keeper job)

`move_bins` is permissionless: find a bin that is out of range, check that its kind allows the direction, and send it to a bin between it and the active bin. The destination's array is passed as a *remaining account* only when it is a different array from the source's; when both bins share an array it must **not** be passed again, because a transaction cannot carry the same account twice.

```typescript
/**
 * One keeper round: move the first out-of-range moving bin it finds into the
 * active bin. Real keepers do this in a loop and can afford to be smarter about
 * choosing the destination, but every choice here is one the program allows.
 */
async function rebalanceOnce(pool: PublicKey) {
  const state = await program.account.pool.fetch(pool);
  const active = state.activeBinId as number;
  const activeArray = binArrayIndex(active);

  // A move can carry at most BINS_PER_ARRAY bins, so only the neighbouring
  // arrays can hold a source that can reach the active bin in one hop.
  for (let index = activeArray - 1; index <= activeArray + 1; index++) {
    const fromArray = binArrayPda(pool, index);
    let array;
    try {
      array = await program.account.binArray.fetch(fromArray);
    } catch {
      continue; // that array was never initialized
    }

    for (let offset = 0; offset < BINS_PER_ARRAY; offset++) {
      const binId = index * BINS_PER_ARRAY + offset;
      const bin = array.bins[offset];
      const binKind = kindOf(bin);

      const isEmpty = bin.reserveA.isZero() && bin.reserveB.isZero();
      if (binKind === "static" || bin.movedTo !== null || bin.supply.isZero() || isEmpty) continue;

      const canMoveRight = binId < active && (binKind === "right" || binKind === "both");
      const canMoveLeft = binId > active && (binKind === "left" || binKind === "both");
      if (!canMoveRight && !canMoveLeft) continue;

      const destinationArray = binArrayIndex(active);
      const isSameArray = destinationArray === index;

      await program.methods
        .moveBins(new anchor.BN(index), binId, active)
        .accounts({ pool, fromArray })
        .remainingAccounts(
          isSameArray
            ? []
            : [{ pubkey: binArrayPda(pool, destinationArray), isWritable: true, isSigner: false }]
        )
        .rpc();

      return; // one move per transaction keeps CU and account locks simple
    }
  }
}
```

### Withdraw along the move chain

`remove_liquidity` follows the links from the position's bin, so the caller has to supply the arrays those links land in. They are matched by their canonical PDA, so the order among them does not matter:

```typescript
/** The arrays a withdrawal needs: one for each hop that leaves the position's own array. */
async function chainAccounts(pool: PublicKey, binId: number): Promise<PublicKey[]> {
  const arrays: PublicKey[] = [];
  let cursor = binId;
  const homeArray = binArrayIndex(binId);

  for (let hop = 0; hop < 8; hop++) {
    const index = binArrayIndex(cursor);
    const array = await program.account.binArray.fetch(binArrayPda(pool, index));
    const bin = array.bins[binOffset(cursor)];

    if (bin.movedTo === null) break; // terminal bin: this is where the assets are
    cursor = bin.movedTo.toBinId as number;

    const nextIndex = binArrayIndex(cursor);
    if (nextIndex !== homeArray && !arrays.some((a) => a.equals(binArrayPda(pool, nextIndex)))) {
      arrays.push(binArrayPda(pool, nextIndex));
    }
  }
  return arrays;
}

async function withdraw(pool: PublicKey, binId: number, shares: bigint, minA: bigint, minB: bigint) {
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
    .remainingAccounts(
      (await chainAccounts(pool, binId)).map((pubkey) => ({ pubkey, isWritable: true, isSigner: false }))
    )
    .rpc();
}
```

### Integration pitfalls

1. **A bin's kind is chosen once.** The first deposit sets it; every later deposit into that bin has to pass the same kind or fails with `BinKindMismatch`.
2. **Static first.** A `Right`, `Left` or `Both` deposit before any static bin is funded fails with `NoStaticBaseline` - the pool needs an anchor for its price.
3. **The position's `bin_id` never changes.** After a move, `remove_liquidity` still takes the *original* bin id (the one in the state's `binId`), and the remaining accounts have to cover the chain.
4. **A moved bin is a ledger, not a bin.** Its reserves are zero and its `movedTo` is set, so it can never be moved again or deposited into - `BinAlreadyMoved`.
5. **Move destinations are constrained.** The destination must be between the source and the active bin, within `MAX_SHIFT_BINS` (one array), and empty or of the same kind.
6. **Shifts are mark-to-market, not swaps.** No tokens move, the source's reserves are re-priced at the destination's price, and the destination's value per share is preserved to the last unit - which is why the instruction needs no authority beyond the move guards.
7. **Amounts are raw base units** everywhere, and `min_amount_out` / `min_amount_a` / `min_amount_b` are raw floors.
8. **The keeper should act on state, not on hope.** `move_bins` only succeeds when the bin is genuinely out of range; a keeper that acts on a stale read just wastes a transaction.
9. **Rail accounts are mandatory either way.** Every `swap` must pass all rail accounts: real ones for enabled rails, the AMM program ID in their place for disabled ones.

---

## 🚨 Error Codes

| Error | Message | Cause |
| :--- | :--- | :--- |
| `InvalidBinStep` | Invalid bin step | `bin_step` is 0 or above `MAX_BIN_STEP`. |
| `FeeTooHigh` | Fee too high | `base_fee_bps` above `MAX_BASE_FEE_BPS`. |
| `MathOverflow` | Math overflow | Checked arithmetic overflowed or underflowed. |
| `SlippageExceeded` | Slippage exceeded | Output below `min_amount_out` / the withdrawal floors. |
| `InsufficientLiquidity` | Insufficient liquidity to fill the swap | The supplied arrays cannot fill the trade. |
| `BinNotInitialized` | Bin has no price | The bin's array was never initialized. |
| `ZeroAmount` | Amount must be greater than zero | A zero deposit, swap or move. |
| `InsufficientShares` | Not enough LP shares | Burning more shares than the position holds. |
| `Unauthorized` | Position does not belong to this owner | Signer is not the position's owner. |
| `PoolPaused` | Pool is paused | Swap, deposit or move while paused. |
| `UnsupportedDecimals` | Mint decimals above the supported maximum | Above `MAX_MINT_DECIMALS` (9). |
| `NoBinArrays` | No bin array was supplied to swap through | Remaining accounts held no bin array. |
| `WrongBinArrayPool` | Bin array does not belong to this pool | `bin_array.pool` mismatch. |
| `InvalidBinArray` | Bin array is not the canonical PDA for its index | Wrong address, or a mis-sized account. |
| `BinArrayIndexMismatch` | Bin arrays must be supplied in walk order | Swap arrays out of order. |
| `BinNotInArray` | Bin does not belong to the given bin array | `bin_id` and `array_index` disagree. |
| `BinArrayOutOfRange` | Bin array is too far from the active bin | New array beyond `MAX_BIN_ARRAY_SPAN`. |
| `BinArrayNotWritable` | Bin array must be passed as writable | A bin array in remaining accounts is read-only. |
| `NoStaticBaseline` | Static baseline required before directional LPing | Moving kind funded before any static bin. |
| `StaticCannotMove` | Static bins cannot be migrated | `move_bins` on a `Static` bin. |
| `WrongMoveDirection` | Bin kind does not allow this direction | `Right` moving left, `Left` moving right, or a destination past the active bin. |
| `BinStillInRange` | Bin is still in range of the active bin | Moving the bin the price is trading in. |
| `ShiftTooFar` | A move cannot carry liquidity further than one bin array | Destination beyond `MAX_SHIFT_BINS`. |
| `BinKindMismatch` | Bin kind does not match the merge destination | Merging into a funded bin of another kind. |
| `BinAlreadyMoved` | Bin has already been moved | Depositing into, or moving, a bin whose liquidity left. |
| `MissingBinArray` | The bin array holding a moved bin was not supplied | A chain hop's array was not passed. |
| `MergeChainNotMonotonic` | Move chain does not advance toward the active bin | Corrupt state: a hop that would not advance, or that reverses direction. A withdrawal follows a monotonic chain to its end, so its length is not capped and LP funds are never stranded. |
| `InvalidSettlementSlots` | Settlement routing policy is inconsistent... | `settlement_slots` must be non-zero iff `hooks_authority` is set (init). |
| `WrongKycConfig` | KYC config account does not match the pool's recorded KYC policy | The passed `kyc_config` is not the pool's configured gate config. |
| `WrongOracleState` | Oracle state account does not match the pool's recorded oracle policy | The passed `oracle_state` is not the `[b"oracle", pool]` PDA. |
| `WrongHooksAuthority` | Hooks authority account is not the hooks program's hook_authority PDA | Not the hooks program's `[b"hook_authority"]` PDA. |
| `RailAccountUnexpected` | A rail account must be this program's own ID when its rail is disabled | A disabled rail's account was not passed as the AMM program ID. |
| `InvalidTradeId` | Trade id must be non-zero when settlement routing is on, zero when off | `trade_id` disagrees with the pool's settlement policy. |
| `InvalidOracleThreshold` | Oracle deviation threshold must be non-zero when the rail is on, zero when off | `max_deviation_bps` disagrees with whether the `oracle_state` account is present. |
| `OracleNotConfigured` | This pool has no oracle configured | A deviation check was requested for a pool with no oracle rail. |
| `InvalidAuthority` | The new authority cannot be the default pubkey | `set_authority` was given `Pubkey::default()`. |
| `InvalidCustodyAccount` | Settlement custody account is not the hook authority's ATA for the output mint | The `custody_token_account` passed to a settlement-routed swap is not the hook authority's ATA for the mint being sold. |

---

## 🧪 Testing & Verification

Inline unit tests (35) cover bin pricing and decimal scaling, bin array tiling, quoting and rounding rules, single and multi-bin fills in both directions, share math and the dynamic fee, every `move_bins` guard, the merge value invariant (both sides' value per share), the move chain (including the monotonicity guard an over-long or corrupt chain trips), and withdrawal slicing.

```bash
cargo test -p maverick
# or, for the on-chain target
anchor build && cargo test-sbf -p maverick
```
