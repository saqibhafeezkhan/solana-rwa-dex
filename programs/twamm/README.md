# Time-Weighted Average Market Maker (TWAMM) Program

An Anchor-based Solana program implementing a **TWAMM (Time-Weighted Average Market Maker)** for institutional block orders: an order escrows its full size up front, states a duration, and is then sold as a stream of virtual sub-orders against an embedded constant-product AMM. Opposing orders are netted against each other before the AMM is ever touched.

Program ID: `FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9`

---

## 🌟 Overview

A block order that goes straight into a constant-product pool pays for its own size: the first unit gets the quoted price and the last unit gets whatever is left of the curve. A TWAMM instead slices the order across time. The institution escrows the whole amount once, and the pool releases it at a constant rate — raw tokens per second — for as long as the order lasts. Whoever settles the elapsed time is doing nothing more than writing down the trades that *would* have happened.

The program is built around two ideas:

1. **Netting.** Flow from orders on opposite sides is matched against each other at the pool's marginal price before anything reaches the embedded AMM. The offsetting part of two facing block orders pays neither a fee nor any price impact — it never touches the curve at all.
2. **Per-period execution.** Only the residual imbalance each period is traded against the curve, once per period, rather than once per order or once per whole order lifetime.

### Key Characteristics:

* **Timestamps, not slots.** Rates are raw tokens per **second** (`Clock::unix_timestamp`). Slots are not evenly spaced in wall-clock time, so a slot-based TWAP drifts by exactly the amount a "time-weighted" order is supposed to control.
* **Escrowed, self-contained orders.** `create_long_term_order` pulls the input into the input vault immediately and records `sell_rate = escrow / duration`. An order cannot be over-sold: it can never sell more than it escrowed.
* **Opposing flow is free.** Crossed flow is settled at the marginal price with no AMM fee and no slippage. In the worked example below, two facing orders of 36 000 A and 72 000 B each execute at exactly 2.0 with the reserves untouched.
* **Proceeds accrue per unit, not per order.** Two Q64.64 accumulators (`acc_b_per_a`, `acc_a_per_b`) credit every raw unit sold with its share of the period's output. Settling is one addition per side per period no matter how many orders are live.
* **Exact per-order accounting.** An order snapshots the accumulator it is owed at creation and claims `sold * (accumulator_now − snapshot)`. It earns its share of the periods it was live for, and nothing that happened before it existed.
* **An expiry-ordered order list.** Orders are a singly linked list sorted by expiry, threaded through the order accounts and verified on every walk. An expired order can never be skipped, because the walk starts at the pool's own head; a live order can never be settled early. Expiry removes the order's rate, so the pool stops selling an escrow that has run out.
* **Embedded AMM with LP shares.** LPs deposit both tokens and hold `LpPosition` shares valued in token A at the marginal price; the AMM's fees accrue to the reserves, which lifts the value of every share without any fee bookkeeping.
* **Pool-owned vaults.** Vaults are PDAs owned by the pool, so a caller cannot substitute an account it controls. Every inbound transfer is *measured* after the fact (`transfer_and_measure`), because a Token-2022 transfer fee can skim part of an amount in flight.
* **Partial-rate refunds.** Integer division leaves a remainder (`escrow − sell_rate × duration`); withdrawal returns it along with the proceeds.
* **Orders can be pulled early.** `cancel_order` trades the time up to the call with the order still in the pool, keeps what it sold, refunds the rest of the escrow, and unlinks it — so a cancel never leaves the pool selling for an escrow that is no longer there.
* **A pause that cannot trap funds.** Admin can pause new orders and deposits. Executions and withdrawals stay open, so escrowed capital and LP liquidity always have a way out.
* **Wired like the rest of the DEX.** A pool can enforce the KYC gate, the oracle circuit breaker and the T+N settlement escrow — see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement). All three are optional and fixed at `initialize_pool`.

The program has no public `swap` instruction: the embedded AMM is reachable only from virtual order execution.

---

## 🔌 Pool Wiring: KYC, Oracle, Settlement

Like the CLMM/DLMM/Maverick pools, a TWAMM pool fixes its **wiring policy** at `initialize_pool` — `kyc_config`, `oracle_state`, `hooks_authority` + `settlement_slots`, all `Pubkey::default()`-able — but because there is no `swap`, the rails attach to different instructions:

| Rail | Pool field | Enabled behaviour |
| :--- | :--- | :--- |
| **KYC** | `kyc_config` | `create_long_term_order` CPIs `kyc_gate::verify_kyc` for the order owner before anything is escrowed — no attestation, no order. Only order **creation** is gated: anyone can still crank executions and an owner can always withdraw or cancel (an exit is not a new trade). |
| **Oracle** | `oracle_state` | Every instruction that settles virtual flow (`create_long_term_order`, `execute_virtual_orders`, `cancel_order`) CPIs `oracle::check_deviation` on the marginal price before the walk, then `oracle::record_observation` (signed by the pool PDA) after it, so the 30-minute TWAP stays current. `withdraw_proceeds` moves no price and runs no oracle. |
| **Settlement** | `hooks_authority` + `settlement_slots` | On `withdraw_proceeds` and `cancel_order`, the **proceeds** (the sold output) are routed into the hook authority's custody ATA and swept into the T+N settlement escrow (`hooks::after_swap` → `settlement::initiate_settlement`); the order owner withdraws after `settlement_slots` with a Transfer Agent signature. The **untraded refund** is exempt — it goes straight back to the owner, because it was never sold, so it was never a trade. |

**Rail account requirements per instruction:** `create_long_term_order` carries the KYC + oracle accounts; `execute_virtual_orders` carries only the oracle account (it is permissionless — no KYC, no settlement); `withdraw_proceeds` and `cancel_order` carry the settlement + oracle accounts.

**Sentinel convention.** The rail accounts are mandatory either way. An enabled rail's accounts are validated against the pool's recorded policy (config equality, or PDA re-derivation for the oracle and hook authority). A disabled rail's accounts must all be **this program's own ID** — Solana has no null account, so the program ID is the "unused here" sentinel; passing anything else reverts with `RailAccountUnexpected`.

---

## 📐 Mathematical Model

Notation: raw token units throughout. `R_a`, `R_b` are the embedded AMM's reserves; `P` is the marginal price of one raw unit of A in raw units of B.

### 1. Marginal price

```
P = R_b / R_a                                  (Q64.64, floor)
```

It is the ratio the reserves themselves define, so it is exactly the price at which crossed flow changes hands. `value_in_a` and the crossing logic share it, which is what keeps LP shares and netting consistent.

### 2. A segment

A segment is one stretch of time over which the pool's rates do not change. For `dt` seconds:

```
offered_a = sell_rate_a × dt
offered_b = sell_rate_b × dt
```

If both sides have flow, the netting step matches them:

```
b_in_a  = offered_b / P                       (b's offer, expressed in A)
cross_a = min(offered_a, b_in_a)
cross_b = cross_a × P                          (≤ offered_b)
```

The crossed legs trade between the two order groups at `P`. **The reserves do not move**: the A that A-sellers escrowed is exactly what B-sellers are buying, and vice versa. No fee is charged, because the AMM was not used.

### 3. The residual

What is left over goes to the embedded AMM — the A side first, then the B side against the reserves that leaves — with the standard constant-product, fee-on-input formula:

```
in_fee  = in × (10 000 − 30)                   // 0.30% fee
out     = in_fee × R_out / (10 000 × R_in + in_fee)
R_in   += in                                    // the whole input, fee included
R_out  −= out
```

The output rounds down and the input is added gross, so the fee stays behind in the reserve it was paid from and `k = R_a R_b` only ever moves up. The test suite asserts that across 500 consecutive segments of two-sided flow.

### 4. Proceeds accumulators

Output is credited to a side through a per-unit accumulator, in Q64.64:

```
acc_b_per_a += (cross_b + out_b) / offered_a    // B paid per raw A sold
acc_a_per_b += (cross_a + out_a) / offered_b    // A paid per raw B sold
```

An order's claim when its books close is

```
sold      = sell_rate × time_live        // = duration at an expiry, less if cancelled
time_live = expiry − created              //  or now − created
proceeds  = sold × (acc_now − snapshot) >> 64
```

`snapshot` is the accumulator at creation. The accumulator is only ever advanced by segments, and every segment an order is live for advances it, so the order's total is exactly its share of the flow it took part in — no more at an expiry, no less at a cancellation. Rounded down at each hop, so the pool never credits more than it took in.

### 5. Order list and expiries

The pool caches the head of the order list and its expiry (`order_head`, `order_head_expiry`). Every entry point that moves time forward — `execute_virtual_orders`, `create_long_term_order` and `cancel_order` — walks the list from that head:

* every account supplied must be the key the previous order points at (`next`);
* an order at the head whose expiry has passed **must** be supplied, or the walk fails (`MissingExpiredOrder`) — the pool refuses to keep selling for it;
* expiring removes the rate and settles the order in the same step.

Orders are inserted in expiry order, using the same walk: the caller supplies the orders that must come before the new one, and the cached `next_expiry` of the last one proves the caller did not stop early. Equal expiries are inserted after equal ones.

### 6. Stopping an order early

`cancel_order` takes the same three steps as an expiry, at the moment of the call instead of at `expiry`:

```
sold = sell_rate × (now − created)        // the stretch it was live for
proceeds = sold × (acc_now − snapshot) >> 64
refund   = escrow − sold
```

The order's rate stays in the pool for the segment ending at `now` (it *was* selling for that time), and only then is it subtracted. Cancelling therefore cannot leave a rate behind for an escrow that is no longer there. An order that has already reached its expiry cannot be cancelled — its escrow is fully committed by then, and `execute_virtual_orders` + `withdraw_proceeds` is the way to collect it (`OrderExpired`).

### 7. LP shares

```
value_in_a(a, b) = a + b / P                      (floor, raw A)
shares           = deposit_value × lp_supply / reserve_value     (first deposit: = deposit_value)
payout           = reserve × shares / lp_supply                   (floor)
```

At a fresh pool the deposit sets `P`, and its two legs are worth the same in A, so the genesis supply is `value_in_a(initial_a, initial_b, P) = 2 × initial_a` (up to rounding). A later deposit at the pool's own ratio buys exactly its pro-rata share of the supply.

### Worked example: netting two facing orders

Pool: `R_a = 1 000 000`, `R_b = 2 000 000`, so `P = 2.0`.

* Order 1 escrows 36 000 A over 3 600 s → `sell_rate_a = 10 A/s`.
* Order 2 escrows 72 000 B over 3 600 s → `sell_rate_b = 20 B/s`.

Both are live for the whole hour, so the pool's rates stay at 10 A/s and 20 B/s.

One settle of the whole hour (`execute_virtual_orders`, `dt = 3 600`):

| Step | Value |
| :--- | :--- |
| `offered_a` | 36 000 |
| `offered_b` | 72 000 |
| `b_in_a = 72 000 / 2.0` | 36 000 |
| `cross_a` | 36 000 (the whole A side) |
| `cross_b = 36 000 × 2.0` | 72 000 (the whole B side) |
| Residual to the AMM | 0 A, 0 B |
| Reserves after | unchanged: 1 000 000 / 2 000 000 |
| `acc_b_per_a` | 2.0 |
| `acc_a_per_b` | 0.5 |
| Order 1 proceeds | `36 000 × 2.0 = 72 000 B` — an average of exactly 2.0 B per A, no fee, no impact |
| Order 2 proceeds | `72 000 × 0.5 = 36 000 A` — an average of exactly 0.5 A per B |

The same 36 000 A order, settled alone against the curve, would have received **69 296 B** at an average of 1.9249 B per A. The difference is what the offsetting flow is worth.

---

## ⚙️ Program Constants

| Constant | Value | Purpose |
| :--- | :--- | :--- |
| `FEE_BPS` | `30` | Embedded AMM fee (0.30%) on the flow that reaches it. Crossed flow pays none. |
| `BPS_DENOMINATOR` | `10_000` | Basis-point denominator for the fee. |
| `MAX_DURATION_SECONDS` | `31_536_000` | Longest order duration (365 days). |
| `POOL_SEED` | `b"twamm-pool"` | Pool PDA. |
| `VAULT_A_SEED` / `VAULT_B_SEED` | `b"twamm-vault-a"` / `b"twamm-vault-b"` | Pool-owned vault PDAs. |
| `ORDER_SEED` | `b"twamm-order"` | Long-term order PDA. |
| `LP_SEED` | `b"twamm-lp"` | LP position PDA. |

---

## 🔑 Account Architecture & PDAs

```
                       ┌──────────────────────────────┐
                       │            Pool              │
                       │  reserves, sell rates,       │
                       │  accumulators, lp_supply,    │
                       │  order_head ────────┐        │
                       └─────────────────────┼────────┘
                             ▲               │ next / next_expiry
        signer seeds ────────┘               ▼
    ┌───────────────┐   ┌───────────────┐   ┌─────────────────────┐
    │   vault_a     │   │   vault_b     │   │   LongTermOrder     │──▶ next order
    │  (pool-owned) │   │  (pool-owned) │   │  rate, expiry,      │
    └───────────────┘   └───────────────┘   │  proceeds, snapshot │
        LP deposits +        LP deposits +  └─────────────────────┘
        order escrows        proceeds
```

An order is only ever settled in the order its expiry says, and an expired order is unlinked in the same transaction that settles it, so the list is always exactly the set of live orders.

### PDA Seeds Table

| Account | Seeds | Notes |
| :--- | :--- | :--- |
| `Pool` | `["twamm-pool", mint_a, mint_b]` | One pool per mint pair. Reserves and rates live here. |
| `vault_a` | `["twamm-vault-a", pool, mint_a]` | `token::authority = pool`; holds LP liquidity A and A order escrows. |
| `vault_b` | `["twamm-vault-b", pool, mint_b]` | Same, for B. |
| `LongTermOrder` | `["twamm-order", pool, owner, order_id (u64 LE)]` | `order_id` is caller-chosen and unique per owner and pool. |
| `LpPosition` | `["twamm-lp", pool, owner]` | Created on first deposit, reused afterwards. |

### Vault balance identity

```
vault_a = reserve_a + A escrows not yet sold + A owed to B-sellers + unrefunded dust
vault_b = reserve_b + B escrows not yet sold + B owed to A-sellers + unrefunded dust
```

Every instruction moves one of those terms and the matching tokens together. It is why an LP withdrawal can only ever pay out `reserve × shares / lp_supply`, and why it can never touch an order's escrow.

---

## 🛠️ Instructions API

### 1. `initialize_pool`

```rust
pub fn initialize_pool(
    ctx: Context<InitializePool>,
    initial_amount_a: u64,
    initial_amount_b: u64,
    authority: Pubkey,
    kyc_config: Pubkey,
    oracle_state: Pubkey,
    hooks_authority: Pubkey,
    settlement_slots: u64,
) -> Result<()>
```

Creates the pool, both pool-owned vaults and the payer's LP position in one go. The initial deposit **is** the genesis price (`P = initial_b / initial_a`) and the genesis LP supply. `payer` becomes the first LP; `authority` is the admin (pause and role rotation) and does not have to be the payer.

Requires both amounts to be non-zero — a TWAMM with no curve to trade against cannot execute anything.

The last four arguments wire the optional rails, exactly as on the CLMM/DLMM/Maverick pools: pass a real config/PDA address to enable a rail, `Pubkey::default()` to leave it off. The context's three optional program accounts are `Option`-typed and follow the same convention (pass `None` with `--account`-style remaining-account omission, or the program ID as the unchecked sentinel — see [Pool Wiring](#-pool-wiring-kyc-oracle-settlement)). `hooks_authority` and `settlement_slots` are validated together: setting one requires the other (`InvalidSettlementSlots`).

### 2. `add_liquidity`

```rust
pub fn add_liquidity(ctx: Context<AddLiquidity>, amount_a: u64, amount_b: u64) -> Result<()>
```

Deposits both tokens and mints shares against the deposit's value in token A at the marginal price. Both legs are required; a deposit that does not match the pool's ratio is simply priced at that ratio. Refused while the pool is paused. Deposits are measured on arrival, so a Token-2022 transfer fee cannot mint shares against tokens the vault never received.

### 3. `remove_liquidity`

```rust
pub fn remove_liquidity(
    ctx: Context<RemoveLiquidity>,
    shares: u128,
    min_amount_a: u64,
    min_amount_b: u64,
) -> Result<()>
```

Burns shares for the pro-rata slice of both reserves, rounded down. `min_amount_a` / `min_amount_b` protect against a swap landing between the quote and the transaction (`SlippageExceeded`). **Not gated on `paused`** — a pause stops new exposure, it never traps liquidity.

### 4. `create_long_term_order`

```rust
pub fn create_long_term_order<'info>(
    ctx: Context<'info, CreateLongTermOrder<'info>>,
    order_id: u64,
    amount_in: u64,
    duration_seconds: i64,
    is_a_to_b: bool,
) -> Result<()>
```

Escrows `amount_in` and starts selling it at `escrowed / duration_seconds` per second, until `now + duration_seconds`. `is_a_to_b = true` sells A for B; `false` sells B for A.

* `escrowed` is what the vault actually received, so the rate is derived from the measured amount.
* The rate must be at least 1 unit per second (`RateTooSmall`), and the duration at most `MAX_DURATION_SECONDS`.
* The rate is added to the pool **after** the walk, and the accumulator snapshot is taken at that point, so the order is owed nothing for flow from before it existed.
* Refused while the pool is paused.

**Remaining accounts — the insertion walk.** Supply the order list starting at the pool's `order_head`, up to but not including the first order that expires after this one. Every account must be writable. Expired orders along the way are settled, which makes creating an order a fine time to crank the queue.

| Case | Remaining accounts |
| :--- | :--- |
| Pool has no orders | none |
| New order expires before every live order | none (it becomes the head) |
| `A` and `B` expire before it, `C` after it | `[A, B]` — the live `A` and `B` are the insertion prefix |
| An expired `E` sits at the head | `[E]` plus the live prefix, in list order |

The program verifies the walk rather than trusting it: each account must be the key the previous one points at, the last one's cached `next_expiry` must be `0` or later than the new order's expiry (`OrderListMismatch`), and nothing expired may be left at the head (`MissingExpiredOrder`).

### 5. `execute_virtual_orders`

```rust
pub fn execute_virtual_orders(ctx: Context<ExecuteVirtualOrders>) -> Result<()>
```

Settles every virtual trade between the pool's last execution and `now`, settles and unlinks the orders that expired in that window, and advances `last_execution_timestamp`. Permissionless: whoever wants the pool current can pay for it.

If nothing has expired, pass no remaining accounts. If orders have expired, pass them from the head in list order — the walk demands them, which is what makes it impossible to skip one.

### 6. `cancel_order`

```rust
pub fn cancel_order(ctx: Context<CancelOrder>, order_id: u64) -> Result<()>
```

Stops a live order early. It keeps the output it earned for the time it was actually selling, the rest of its escrow comes back, and the order account is closed so its rent returns to the owner.

* The pool is traded up to the moment of the call **with this order's rate still in it** (it was selling for that time), and only then is the rate subtracted.
* Not gated on `paused` — cancelling is an exit.
* An order past its expiry is refused (`OrderExpired`): its escrow is fully committed, so settle it and withdraw instead. An order that is already settled is refused too (`OrderAlreadySettled`) — withdraw it.
* Cancelling in the same second the order was created is allowed and refunds everything.

**Remaining accounts — the unlink walk.** Supply the order list from the pool's `order_head` up to, but **not including**, the order being cancelled. The program verifies the link: after settling and popping whatever expired, the next key in the chain must be this order's key (`OrderListMismatch` otherwise), which is also what proves it is still in the list.

| Case | Remaining accounts |
| :--- | :--- |
| The order is the head | none |
| `A` sits in front of it, `B` in front of `A` | `[B, A]` |
| An expired `E` sits at the head | `[E, ...]` — the walk settles it on the way |

### 7. `withdraw_proceeds`

```rust
pub fn withdraw_proceeds(ctx: Context<WithdrawProceeds>, order_id: u64) -> Result<()>
```

Pays the settled order's `proceeds_out` in the output token, refunds the unsold remainder of the escrow in the input token, and **closes** the order account so its rent goes back to the owner.

An order becomes `settled` only when a walk meets its expiry, so call `execute_virtual_orders` first; until then the error is `OrderNotSettled`. The order's address is derived from the caller's own key, so there is no way to reach somebody else's order here.

### 8. `set_paused`

```rust
pub fn set_paused(ctx: Context<Admin>, paused: bool) -> Result<()>
```

Admin-only. While paused, `create_long_term_order` and `add_liquidity` are refused. `execute_virtual_orders`, `withdraw_proceeds`, `cancel_order` and `remove_liquidity` keep working.

### 9. `set_authority`

```rust
pub fn set_authority(ctx: Context<Admin>, new_authority: Pubkey) -> Result<()>
```

Admin-only. Rotates the pause key, so a pool does not have to be redeployed to move the role onto a new multisig.

---

## 📄 Data Structures

### `Pool` (236 bytes incl. discriminator)

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `mint_a` / `mint_b` | `Pubkey` | The pair this pool trades. Also its PDA seeds. |
| `reserve_a` / `reserve_b` | `u64` | Embedded AMM reserves, raw units. |
| `last_execution_timestamp` | `i64` | Every virtual trade up to this second has been settled. |
| `sell_rate_a` / `sell_rate_b` | `u64` | Raw units per second being sold by live orders, per side. |
| `acc_b_per_a` | `u128` | Q64.64 B per raw A sold since genesis. A claim is `sold × acc >> 64`. |
| `acc_a_per_b` | `u128` | The mirror image for B. |
| `order_head` | `Pubkey` | First order in the expiry-sorted list; all zeroes when none. |
| `order_head_expiry` | `i64` | Cached expiry of the head — the proof that a walk was complete. |
| `lp_supply` | `u128` | Total LP shares outstanding, denominated in raw A units. |
| `authority` | `Pubkey` | Pause / role-rotation key. |
| `paused` | `bool` | Refuses new orders and deposits. |
| `bump`, `vault_a_bump`, `vault_b_bump` | `u8` | Cached PDA bumps. |

### `LongTermOrder` (195 bytes incl. discriminator)

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `pool` / `owner` / `order_id` | `Pubkey` / `Pubkey` / `u64` | Identity; also the PDA seeds. |
| `is_a_to_b` | `bool` | Selling A for B when true. |
| `sell_rate` | `u64` | Raw input sold per second. |
| `amount_in` | `u64` | Escrow actually received. Zeroed when the order is withdrawn. |
| `sold` | `u64` | `sell_rate × time live`, fixed when the books are closed (all of it at an expiry, part of it at a cancellation). |
| `duration` / `created_timestamp` / `expiry_timestamp` | `i64` | The order's window. |
| `acc_snapshot` | `u128` | Output accumulator at creation — what it is owed from, not before. |
| `proceeds_out` | `u64` | Output owed, fixed when the books are closed. |
| `settled` | `bool` | The pool has closed the books: it met the expiry, or the order was cancelled. |
| `next` / `next_expiry` | `Pubkey` / `i64` | The expiry-ordered list. |
| `bump` | `u8` | Cached PDA bump. |

### `LpPosition` (89 bytes incl. discriminator)

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `pool` / `owner` | `Pubkey` | The pool the position is in, and who holds it. |
| `shares` | `u128` | Claim on the reserves, in raw A units at genesis. |
| `bump` | `u8` | Cached PDA bump. |

---

## 🔌 TypeScript Integration

The snippets use the same stack as the other program docs: `@coral-xyz/anchor` with the IDL types `anchor build` writes to `target/types/twamm.ts`, plus `@solana/web3.js` and `@solana/spl-token`.

### PDAs and helpers

The order PDA takes the owner and a little-endian `u64` tag, so one owner can run many orders on one pool:

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";
import { TOKEN_2022_PROGRAM_ID } from "@solana/spl-token";
import { Twamm } from "./target/types/twamm";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Twamm as Program<Twamm>;

/** The program's address; `anchor build` also leaves its keypair in target/deploy. */
const programId = program.programId;

const u64 = (n: number | bigint) => {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(BigInt(n));
  return b;
};

const poolPda = (mintA: PublicKey, mintB: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("twamm-pool"), mintA.toBuffer(), mintB.toBuffer()],
    programId
  )[0];

const vaultPda = (pool: PublicKey, mint: PublicKey, side: "twamm-vault-a" | "twamm-vault-b") =>
  PublicKey.findProgramAddressSync(
    [Buffer.from(side), pool.toBuffer(), mint.toBuffer()],
    programId
  )[0];

const orderPda = (pool: PublicKey, owner: PublicKey, orderId: bigint) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("twamm-order"), pool.toBuffer(), owner.toBuffer(), u64(orderId)],
    programId
  )[0];

const lpPda = (pool: PublicKey, owner: PublicKey) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("twamm-lp"), pool.toBuffer(), owner.toBuffer()],
    programId
  )[0];

/** 2^64, the Q64.64 unit, for turning fixed-point values into UI numbers. */
const Q64 = 2n ** 64n;
const q64ToNumber = (v: bigint) => Number(v) / Number(Q64);
```

### Create a pool and read it back

```typescript
async function createPool(mintA: PublicKey, mintB: PublicKey, initialA: bigint, initialB: bigint) {
  const pool = poolPda(mintA, mintB);
  const payer = provider.wallet.publicKey;

  await program.methods
    .initializePool(
      new anchor.BN(initialA.toString()),
      new anchor.BN(initialB.toString()),
      payer // admin: an institutional multisig in production
    )
    .accounts({
      payer,
      mintA,
      mintB,
      pool,
      vaultA: vaultPda(pool, mintA, "twamm-vault-a"),
      vaultB: vaultPda(pool, mintB, "twamm-vault-b"),
      lpPosition: lpPda(pool, payer),
      payerTokenA,
      payerTokenB,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();

  const state = await program.account.pool.fetch(pool);
  // Raw units: 1_000_000 A against 2_000_000 B is a price of 2.0 B per A.
  console.log("price", Number(state.reserveB) / Number(state.reserveA));
  console.log("lp shares", state.lpSupply.toString());
}
```

### Add and remove liquidity

```typescript
async function addLiquidity(pool: PublicKey, mintA: PublicKey, mintB: PublicKey, amountA: bigint, amountB: bigint) {
  await program.methods
    .addLiquidity(new anchor.BN(amountA.toString()), new anchor.BN(amountB.toString()))
    .accounts({
      pool,
      position: lpPda(pool, provider.wallet.publicKey),
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "twamm-vault-a"),
      vaultB: vaultPda(pool, mintB, "twamm-vault-b"),
      ownerTokenA,
      ownerTokenB,
      owner: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .rpc();
}

async function removeLiquidity(pool: PublicKey, mintA: PublicKey, mintB: PublicKey, shares: bigint) {
  await program.methods
    .removeLiquidity(new anchor.BN(shares.toString()), new anchor.BN(0), new anchor.BN(0))
    .accounts({
      pool,
      position: lpPda(pool, provider.wallet.publicKey),
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "twamm-vault-a"),
      vaultB: vaultPda(pool, mintB, "twamm-vault-b"),
      ownerTokenA,
      ownerTokenB,
      owner: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();
}
```

### Walk the order list

Every instruction that touches the list — `create_long_term_order`, `execute_virtual_orders` and `cancel_order` — needs it in order. The pool caches the head, and each order caches `next` and `nextExpiry`, so the whole list can be discovered by fetching one account at a time — no indexer required:

```typescript
/** The live orders, in expiry order, read straight off the pool + order chain. */
async function fetchOrderChain(pool: PublicKey) {
  const state = await program.account.pool.fetch(pool);
  const orders: { key: PublicKey; expiry: number }[] = [];

  // One `fetch` per link reads best; a production client should use
  // `getMultipleAccounts` on the keys it already knows, a page at a time.
  let key = state.orderHead as PublicKey;
  while (!key.equals(PublicKey.default)) {
    const order = await program.account.longTermOrder.fetch(key);
    orders.push({ key, expiry: order.expiryTimestamp.toNumber() });
    key = order.next as PublicKey;
  }
  return orders;
}

/** Expired orders from the head: what `execute_virtual_orders` wants. */
async function expiredPrefix(pool: PublicKey, now = Math.floor(Date.now() / 1000)) {
  const chain = await fetchOrderChain(pool);
  const expired = [];
  for (const order of chain) {
    if (order.expiry > now) break; // sorted by expiry, so we can stop here
    expired.push({ pubkey: order.key, isWritable: true, isSigner: false });
  }
  return expired;
}
```

### Create a long-term order

`remainingAccounts` is the insertion prefix: the orders that must come before the new one. Deriving it is the same walk, stopped at the first later expiry:

```typescript
async function createOrder(
  pool: PublicKey,
  mintA: PublicKey,
  mintB: PublicKey,
  orderId: bigint,
  amountIn: bigint,
  durationSeconds: number,
  isAToB: boolean
) {
  const now = Math.floor(Date.now() / 1000);
  const expiry = now + durationSeconds;

  const chain = await fetchOrderChain(pool);
  const prefix = [];
  for (const order of chain) {
    if (order.expiry > expiry) break;
    prefix.push({ pubkey: order.key, isWritable: true, isSigner: false });
  }

  const order = orderPda(pool, provider.wallet.publicKey, orderId);

  await program.methods
    .createLongTermOrder(
      new anchor.BN(orderId.toString()),
      new anchor.BN(amountIn.toString()),
      new anchor.BN(durationSeconds),
      isAToB
    )
    .accounts({
      pool,
      order,
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "twamm-vault-a"),
      vaultB: vaultPda(pool, mintB, "twamm-vault-b"),
      // Both owner token accounts are required by the instruction; only the
      // input side is moved, the other is unused.
      ownerTokenA,
      ownerTokenB,
      owner: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: anchor.web3.SystemProgram.programId,
    })
    .remainingAccounts(prefix)
    .rpc();

  const created = await program.account.longTermOrder.fetch(order);
  console.log("rate per second", created.sellRate.toString(), "expires", created.expiryTimestamp.toString());

  const poolState = await program.account.pool.fetch(pool);
  console.log("acc_b_per_a", q64ToNumber(poolState.accBPerA), "B per A sold so far");
}
```

### Execute, withdraw, cancel

```typescript
/** Permissionless crank: anyone can keep the pool current. */
async function executeVirtualOrders(pool: PublicKey, mintA: PublicKey, mintB: PublicKey) {
  const expired = await expiredPrefix(pool);

  await program.methods
    .executeVirtualOrders()
    .accounts({ pool, mintA, mintB })
    .remainingAccounts(expired)
    .rpc();
}

async function withdrawProceeds(
  pool: PublicKey,
  mintA: PublicKey,
  mintB: PublicKey,
  orderId: bigint
) {
  const order = orderPda(pool, provider.wallet.publicKey, orderId);

  // A withdrawal only works on a settled order, so crank first if needed.
  const state = await program.account.longTermOrder.fetch(order);
  if (!state.settled) {
    await executeVirtualOrders(pool, mintA, mintB);
  }

  await program.methods
    .withdrawProceeds(new anchor.BN(orderId.toString()))
    .accounts({
      pool,
      order,
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "twamm-vault-a"),
      vaultB: vaultPda(pool, mintB, "twamm-vault-b"),
      ownerTokenA,
      ownerTokenB,
      owner: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();
}

/** The orders in front of `order`, in list order. The target itself is left
 *  out: `cancel_order` walks up to it, and verifies the link itself. */
async function prefixBefore(pool: PublicKey, order: PublicKey) {
  const prefix = [];
  for (const entry of await fetchOrderChain(pool)) {
    if (entry.key.equals(order)) return prefix;
    prefix.push({ pubkey: entry.key, isWritable: true, isSigner: false });
  }
  throw new Error("order is not in the pool's list");
}

/** Stop a live order early: keep what it sold, refund the rest, close it. */
async function cancelOrder(
  pool: PublicKey,
  mintA: PublicKey,
  mintB: PublicKey,
  orderId: bigint
) {
  const order = orderPda(pool, provider.wallet.publicKey, orderId);

  await program.methods
    .cancelOrder(new anchor.BN(orderId.toString()))
    .accounts({
      pool,
      order,
      mintA,
      mintB,
      vaultA: vaultPda(pool, mintA, "twamm-vault-a"),
      vaultB: vaultPda(pool, mintB, "twamm-vault-b"),
      ownerTokenA,
      ownerTokenB,
      owner: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .remainingAccounts(await prefixBefore(pool, order))
    .rpc();
}
```

### Integration pitfalls

1. **`order_id` is part of the address.** Re-using one fails to create the account, which is the point: it is how a single wallet runs several orders on one pool. Check `fetchNullable` before picking an id if you do not want to track them yourself.
2. **Remaining accounts must be writable and in list order.** A walk that skips, reorders or over-supplies the list fails with `OrderListMismatch`; a live order handed to `execute_virtual_orders` fails with `OrderNotExpired`; an expired head left out fails with `MissingExpiredOrder`.
3. **Everything is in raw token units.** Rates are raw units per second, reserves are raw units, and `sell_rate = floor(amount_in / duration_seconds)`. Pair a 6-decimal token with a 9-decimal one and the per-second rates differ by a factor of a thousand — that is correct, not a bug.
4. **Amounts are `u64`, shares are `u128`.** Order sizes above `u64::MAX` raw units are not representable; split them across orders.
5. **A withdrawal is a two-step flow.** `withdraw_proceeds` never settles anything itself, so a caller must crank `execute_virtual_orders` at or after the expiry first (the snippet above does it automatically). `cancel_order` is the opposite: it does the trading itself, and refuses an order that has already reached its expiry.
6. **Use the mint's token program.** Vaults are created for whatever `token_program` the pool was initialised with, so legacy SPL Token mints need `TOKEN_PROGRAM_ID` and Token-2022 mints need `TOKEN_2022_PROGRAM_ID` on every call.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `MathOverflow` | `Math overflow` | Checked arithmetic overflowed or underflowed. |
| `ZeroAmount` | `Amount must be greater than zero` | A deposit, escrow or price leg was zero. |
| `ZeroDuration` | `Duration must be greater than zero` | `duration_seconds` was zero or negative. |
| `DurationTooLong` | `Duration is longer than the pool allows` | `duration_seconds` exceeds `MAX_DURATION_SECONDS`. |
| `RateTooSmall` | `Order rate rounds to zero: the amount is smaller than its duration in seconds` | `escrow / duration_seconds` floored to zero; shorten the duration or add size. |
| `InsufficientLiquidity` | `Insufficient liquidity` | The embedded AMM has a zero reserve, or a share payout was asked of an empty pool. |
| `InsufficientShares` | `Not enough LP shares` | Burning more shares than the position holds. |
| `SlippageExceeded` | `Slippage exceeded` | Withdrawal output below `min_amount_a` / `min_amount_b`. |
| `PoolPaused` | `Pool is paused` | New order or deposit attempted while paused. |
| `Unauthorized` | `Account does not belong to this owner` | Signer does not match the order or position owner. |
| `WrongOrder` | `Order id does not match the account` | `order_id` argument and the order account disagree. |
| `WrongOrderPool` | `Order belongs to a different pool` | Order's `pool` is not the pool passed in. |
| `OrderNotSettled` | `Order has not been settled yet: run execute_virtual_orders first` | Withdrawal before the pool met the order's expiry. |
| `OrderNotExpired` | `Order has not expired yet` | A live order was handed to `execute_virtual_orders`. |
| `OrderExpired` | `Order has expired: settle it with execute_virtual_orders, then withdraw its proceeds` | `cancel_order` on an order whose expiry has passed. |
| `OrderAlreadySettled` | `Order has already been settled: withdraw its proceeds instead` | `cancel_order` on an order the pool has already met the expiry of. |
| `OrderListMismatch` | `Order list walk got an account that is not the next order` | The supplied orders are not the next links of the list, or the insertion point was wrong. |
| `MissingExpiredOrder` | `Expired order was left out of the walk` | An expired order at the head was not supplied. |
| `OrderAccountNotWritable` | `Order account must be passed as writable` | Order account passed read-only. |
| `ClockWentBackwards` | `Clock moved backwards` | Chain timestamp below `last_execution_timestamp`. |

---

## 🧪 Testing & Verification

The program has 26 inline unit tests over the pure functions — the segment engine, the accumulators, order settlement and cancellation, the AMM and the share math. They run without a validator:

```bash
cargo test -p twamm
# or from the program directory
cargo test --lib
```

They pin the properties the design rests on:

* **AMM correctness** — the fee formula matches the textbook `(in × 997 × R) / (r × 1000 + in × 997)`, an output can never empty the reserve it drains, and `k` rises monotonically across 500 consecutive two-sided segments.
* **Netting** — equal and opposite flow at parity leaves the reserves exactly untouched and pays 1.0 per unit; crossing converts at the current price, not at parity; only the imbalance reaches the curve; a completely netted period still beats trading the same flow through the AMM.
* **Proceeds** — claims are proportional to flow sold, an order earns nothing for flow that predates it, a B-selling order settles against `acc_a_per_b`, and integer-division dust is tracked as refundable.
* **Cancellation** — a cancelled order is credited exactly the output the curve paid for the stretch it was live, and `sold + refund` equals its escrow at every possible cancellation second.
* **TWAMM behaviour** — slicing a size into 1 000 segments stays within 1% of a single swap of the same size against a static curve, and the per-segment claims add up to the output the curve actually paid.
* **Shares** — genesis shares are the deposit's value in A, a deposit at the marginal price buys exactly its pro-rata supply, and payouts round down.

The order-list walk is verified by construction rather than by assumption: it refuses an account that is not the next link, refuses to leave an expired order at the head, and refuses to insert past an order that should have come first. Those paths need live accounts, so cover them end-to-end with the Anchor test suite:

```bash
anchor test
```

---

## ⚠️ Design Notes & Limits

1. **No public swap.** The embedded AMM is only reachable through virtual order execution. Anyone wanting a spot fill should use the CLMM/DLMM programs in this workspace and let arbitrage keep this pool's price honest.
2. **A cancel keeps the terms it traded under.** The escrow comes back minus what the order already sold, and the sold part is credited at the pool's realised price rather than at the price when the order was created. If the price moved against you, cancelling does not undo that — it stops it from getting worse. Cancelling also needs the account walk, so it costs a transaction with one account per order in front of it.
3. **Settle is not automatic.** Someone has to call `execute_virtual_orders` (or create an order, which does the same walk). Until then the pool's rates and reserves are as of `last_execution_timestamp`, and no order can be withdrawn.
4. **Fees stay in the reserves.** The AMM's fee is paid into the reserve it came from, so an LP's return shows up as a rising share value rather than a separate claim — there is no fee-withdrawal instruction by design.
5. **Mints with transfer hooks are not supported.** Remaining-account space is fully used by the order list, so a Token-2022 mint that requires hook accounts will fail its transfer. Transfer-fee mints are supported and handled by measuring deposits.
6. **The walk costs accounts.** Inserting and executing scales with the number of orders that must come before, so trading a pool with hundreds of live orders needs a helper that discovers the chain (the `fetchOrderChain` snippet) and, eventually, a batched crank.
7. **Admin powers are limited to pause and rotation.** There is no mint authority, no reserve extraction and no upgrade path inside the program — the admin cannot move user funds.
