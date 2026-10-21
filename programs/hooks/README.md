# AMM Hooks Program

An Anchor-based Solana program acting as the **post-swap hook for the RWA DEX**: immediately after a swap executes, `after_swap` CPIs into the Settlement program and routes the output tokens into the T+N settlement escrow instead of letting the taker take them home. It is the "Uniswap v4-style hook" of this repo — the glue between any AMM pool and the `settlement` clearinghouse.

Program ID: `B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA`

Depends on the Settlement program (`settlement = { path = "../settlement", features = ["cpi"] }`), Program ID `58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6`.

---

## 🌟 Overview

A swap on the AMM produces tokens in the pool's vault. Without this hook, the taker walks away with them instantly — settlement risk lives entirely off-chain. With it, the output is captured at the moment of the trade and pushed into a per-trade escrow that only opens after T+N **and** a Transfer Agent signature (see the Settlement program README):

1. **The AMM calls `after_swap`** after its swap math executes, passing `trade_id`, `settlement_slots` (the T+N length in slots) and `amount` (the output it just paid into the custody ATA).
2. **The hook computes the unlock slot** as `Clock::slot + settlement_slots` (checked, `SettlementSlotOverflow` on overflow) and derives the settlement vault PDA `[b"settlement", trade_id_le]` on the settlement program.
3. **It CPIs `settlement::initiate_settlement`**, signing with the **hook authority PDA** — a PDA of *this* program, which owns the custody ATA holding the output tokens. That ownership is the whole security model: only the hook authority can move funds out of the custody account, and only this program can invoke it with its seeds.
4. **The Settlement program locks the funds** in the per-trade vault PDA, recording `(trade_id, taker, mint, amount, unlock slot, Pending)`.

### Key Characteristics:

* **Derived authority, not a declared signer.** `hook_authority` is a PDA `[b"hook_authority"]` of this program, checked by seed and bump. A plain `AccountInfo` marked `signer` would let any caller satisfy the authority role with an arbitrary wallet — the derivation is what makes "the hook" a real, non-spoofable party.
* **The custody account is the hook authority's ATA, pinned by derivation.** `after_swap` fails with `InvalidCustodyAccount` unless `amm_vault` is exactly `get_associated_token_address_with_program_id(hook_authority, mint, token_program)`. Ownership alone would not be enough: the token program lets anyone create a token account whose `owner` is a PDA (setting the owner needs no signature from it), so an ownership-only check would let a caller choose where the output goes.
* **The settlement vault is pinned by seed.** The passed `settlement_vault` is checked to equal `Pubkey::find_program_address([b"settlement", trade_id_le], settlement::ID)` — `InvalidSettlementVault` otherwise. An attacker cannot swap in a look-alike account; the trade id deterministically names the escrow.
* **Escrow token account pinned to the Settlement program's escrow PDA.** `settlement_vault_token` must equal `[b"escrow", trade_id_le]` on the Settlement program; that program creates it as a token account owned by its fixed `[b"escrow_authority"]` PDA. The mint must agree (`MintMismatch`).
* **Relative T+N, absolute on-chain.** Callers pass `settlement_slots` (a duration); the hook turns it into the absolute `settlement_slot` the Settlement program stores (`Clock::slot + N`).
* **Anyone can keep the hook funded.** `fund_hook_authority` is permissionless: the Settlement program charges vault rent to the `authority` it receives — this PDA — so it needs lamports. Anyone may top it up. The AMM has to pass it **writable** in its swap instruction for that charge to go through.
* **Token-2022-ready.** Accounts are `InterfaceAccount`/`Interface<TokenInterface>`, matching the Settlement program's transfers; classic SPL and Token-2022 mints both work.

---

## 🔗 Who calls it

Every venue in the repo CPIs `after_swap`; the rail is a per-pool (or per-book)
configuration, not a deploy-time choice. The venue records the hooks authority and
the T+N horizon when the pool or book is created, and `Pubkey::default()` for the
authority means the rail is off and the taker is paid directly.

| Caller | When | Rail enabled by |
| :--- | :--- | :--- |
| `clmm` | after swap math, before the taker is paid | `pool.hooks_authority != default` (+ `pool.settlement_slots`) |
| `dlmm` | same | `pool.hooks_authority != default` |
| `maverick` | same | `pool.hooks_authority != default` |
| `twamm` | after a slice executes | `pool.hooks_authority != default` |
| `order_book` | after a fill is matched | `book.hooks_authority != default` (+ `book.settlement_slots`) |

The caller always moves the output into the hook authority's custody ATA first
and then calls `after_swap`, so the hook's job is to escrow what is already
sitting there. The caller must also pass `hook_authority` **writable**: the
Settlement program charges the new vault's rent to it.

---

## ⚙️ Program Constants

| Constant / Layout | Value | Purpose |
| :--- | :--- | :--- |
| Hook authority seed | `b"hook_authority"` | Single PDA of this program: owner/signer for the AMM vault and the settlement CPI. |
| Settlement vault PDA | `[b"settlement", trade_id.to_le_bytes()]` on `settlement::ID` | Recomputed by the hook to validate the caller-supplied escrow. |
| CPI target | `settlement::cpi::initiate_settlement` | The only external call the program makes. |
| Vault rent payer | the `hook_authority` PDA | Charged by `settlement::initiate_settlement` for the new vault account. |

---

## 🔑 Account Architecture & PDAs

```
        AMM pool program
   (calls after_swap post-trade)
              │
              ▼
   ┌──────────────────────────┐    owns     ┌──────────────────────┐
   │  hook_authority PDA      │────────────▶│  amm_vault           │
   │  [b"hook_authority"]     │             │  (swap output)       │
   └────────────┬─────────────┘             └──────────┬───────────┘
                │  CPI: initiate_settlement            │ tokens swept
                │  signer: hook_authority seeds        ▼
                │                          ┌───────────────────────────────┐
                └─────────────────────────▶│  settlement_vault PDA         │
                                           │  [b"settlement", trade_id_le] │
                                           │  records the trade             │
                                           │  status: Pending (T+N)        │
                                           └───────────────────────────────┘

   payer ──fund_hook_authority(lamports)──▶ hook_authority PDA (rent float)
```

### PDA Seeds Table

| Account | PDA Seeds Format | Program | Description |
| :--- | :--- | :--- | :--- |
| **HookAuthority** | `[b"hook_authority"]` | hooks | The program's identity in the flow: owns the AMM vault, signs the settlement CPI. |
| **SettlementVault** | `[b"settlement", trade_id.to_le_bytes()]` | settlement | Derived (not stored) by the hook and checked against the passed account. |

### `AfterSwap` account roles

| Account | Constraints | Role |
| :--- | :--- | :--- |
| `hook_authority` | PDA `[b"hook_authority"]`, bump from seeds; **mut** | CPI signer, and the owner `amm_vault` must be derived for. Writable because the Settlement program charges the new vault (and its escrow) to this PDA, and a CPI can only pass on privileges the caller's instruction already holds — read-only here makes the settlement CPI fail with a writable-privilege escalation. |
| `taker` | unchecked `AccountInfo` | Recorded recipient in the vault; bound into the Transfer Agent's signed message at settle time. |
| `amm_vault` | mut; `key == get_associated_token_address_with_program_id(hook_authority, token_mint, token_program)`, mint == `token_mint` | Swap-output account the trade's `amount` is swept from. Pinned by **address**, not by ownership (see [Pitfalls](#-pitfalls--design-notes)); pinned to the hook authority's ATA under the mint's token program. |
| `token_mint` | `InterfaceAccount<Mint>` | Mint of the escrowed token. |
| `settlement_vault` | mut; must equal `[b"settlement", trade_id_le]` on `settlement::ID` | The escrow the CPI creates. |
| `settlement_vault_token` | mut; must equal `[b"escrow", trade_id_le]` on `settlement::ID` | Escrow token account, created by the Settlement program in the CPI and owned by `[b"escrow_authority"]`. |
| `settlement_config` | typed `settlement::SettlementConfig` | The clearinghouse config the CPI passes through: it decides the initiation gate and carries the Transfer Agent key. |
| `settlement_program` | typed `Program<Settlement>` | The Settlement program itself; the CPI callee is pinned by address. |
| `token_program` / `system_program` | Interface / Program | Token & rent plumbing. |

---

## 🛠️ Instructions API

### 1. `after_swap`

```rust
pub fn after_swap<'info>(
    ctx: Context<'info, AfterSwap<'info>>,
    trade_id: u64,
    settlement_slots: u64,
    amount: u64,
) -> Result<()>
```

Captures the swap output into settlement. Intended to be called by the AMM pool program (or a keeper simulating it) immediately after the swap math executes.

* **Parameters**:
  * `trade_id: u64`: Caller-chosen, unique per trade — it names the settlement vault PDA, so a reused id fails at `init` on the settlement side (`init` on an existing account). Uniqueness is forever.
  * `settlement_slots: u64`: T+N length in slots, relative to now.
  * `amount: u64`: The output the AMM just paid into the custody ATA — the flow of *this* trade, not whatever balance happens to be there. Rejected (`InvalidAmount`) if zero or larger than `amm_vault.amount`.
* **Flow**: derive the vault PDA `[b"settlement", trade_id_le]` on `settlement::ID` → compute `target = Clock::slot + settlement_slots` (`SettlementSlotOverflow` on overflow) → CPI `settlement::initiate_settlement` with `hook_authority` as the signing `authority` (seeds `[b"hook_authority", bump]`) → the Settlement program records the trade and moves `amount` into the escrow token account.
* **Accounts**: see the table above.
* **Note**: the AMM's output tokens must be sitting in the hook authority's **ATA** for the output mint at call time. In production the pool's swap transfers output to `get_associated_token_address(hook_authority, mint)` — if it pays the user directly, this hook has nothing to capture.
* **Note**: there is no *caller* access control on `after_swap` — anyone may call it. What bounds it is the account set: the source must be the hook authority's ATA (so a caller can only move funds the AMM actually routed there) and `amount` is checked against that account's balance.

### 2. `fund_hook_authority`

```rust
pub fn fund_hook_authority(ctx: Context<FundHookAuthority>, lamports: u64) -> Result<()>
```

Tops up the hook authority PDA with system lamports. Permissionless — any payer, any amount.

* **Parameters**: `lamports: u64`.
* **Why it exists**: `settlement::initiate_settlement` runs `init` on the settlement vault with `payer = authority`, and the hook passes its own PDA as that authority. A PDA cannot be funded by a plain system transfer signed by itself, so this instruction performs the `invoke` on the payer's behalf. Without a funded PDA, the CPI fails with a rent/`InsufficientFunds`-style error, not a logic error in either program.
* **Accounts**: `payer` (signer, mut), `hook_authority` (mut, PDA), `system_program`.
* **Note**: fund it **before** the first hooked trade (or any time the float runs dry). The rent is per new vault (~0.0016 SOL for the 98-byte vault account) — one top-up covers many trades.

---

## 📄 Data Structures

This program holds **no state accounts** — all state lives in the Settlement program's `SettlementVault`/`SettlementConfig` PDAs. The only PDA is the lamport-carrying `hook_authority`, which has no data layout (it is a plain system account seeded by `[b"hook_authority"]`).

---

## 🔌 TypeScript Integration

Uses `@coral-xyz/anchor` with the IDL `anchor build` writes to `target/types/hooks.ts` (and `target/types/settlement.ts`), plus `@solana/web3.js` and `@solana/spl-token`.

### PDAs and helpers

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import { TOKEN_2022_PROGRAM_ID, getAssociatedTokenAddressSync } from "@solana/spl-token";
import { Hooks } from "../target/types/hooks";
import { Settlement } from "../target/types/settlement";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const hooks = anchor.workspace.Hooks as Program<Hooks>;
const settlement = anchor.workspace.Settlement as Program<Settlement>;

const hookAuthorityPda = PublicKey.findProgramAddressSync(
  [Buffer.from("hook_authority")],
  hooks.programId
)[0];

const settlementVaultPda = (tradeId: number | anchor.BN) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("settlement"), new anchor.BN(tradeId).toArrayLike(Buffer, "le", 8)],
    settlement.programId
  )[0];

/** AMM-side vault owned by the hook authority (allowOwnerOffCurve PDA ATA). */
const hookVaultAta = (mint: PublicKey) =>
  getAssociatedTokenAddressSync(mint, hookAuthorityPda, true, TOKEN_2022_PROGRAM_ID);

/** The Settlement program's escrow token account for a trade. */
const escrowTokenPda = (tradeId: number | anchor.BN) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("escrow"), new anchor.BN(tradeId).toArrayLike(Buffer, "le", 8)],
    settlement.programId
  )[0];
```

### One-time setup: fund the hook authority

```typescript
// Give the hook authority lamports — it pays the settlement vault's rent.
await hooks.methods
  .fundHookAuthority(new anchor.BN(1_000_000_000)) // 1 SOL float
  .accounts({ payer: wallet.publicKey, hookAuthority: hookAuthorityPda })
  .rpc();
```

The escrow token account is **not** created by the client: `initiate_settlement`
creates and initializes the `[b"escrow", trade_id_le]` PDA itself.

### Wire an AMM swap through the hook

```typescript
// The AMM's swap must pay its output into hookVaultAta(mint) — the account
// owned by the hook authority PDA — instead of the taker's wallet.

const tradeId = new anchor.BN(1);
const vault = settlementVaultPda(tradeId);
const settlementConfigPda = PublicKey.findProgramAddressSync(
  [Buffer.from("config")], settlement.programId
)[0];
const amount = new anchor.BN(250_000_000); // this trade's routed output, base units

await hooks.methods
  .afterSwap(tradeId, new anchor.BN(settlementSlots), amount) // e.g. 100 slots ≈ T+40s
  .accounts({
    hookAuthority: hookAuthorityPda,
    taker: takerPubkey,                    // recorded in the vault; signed-for at settle
    ammVault: hookVaultAta(mint),          // must be owned by hook_authority
    tokenMint: mint,
    settlementVault: vault,                // must equal the derived PDA
    settlementVaultToken: escrowTokenPda(tradeId),
    settlementConfig: settlementConfigPda,
    settlementProgram: settlement.programId,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
    systemProgram: SystemProgram.programId,
  })
  .rpc();
```

### Integrating a real AMM pool program

In the pool program, replace the user-payout leg of the swap with a transfer to the hook vault and a CPI — or, for pools you do not control, a keeper performs the two steps in one transaction:

```rust
// Inside the pool's swap, after the swap math:
let cpi_accounts = hooks::cpi::accounts::AfterSwap {
    hook_authority: ctx.accounts.hook_authority.to_account_info(),
    taker: ctx.accounts.user.to_account_info(),
    amm_vault: ctx.accounts.hook_vault.to_account_info(),
    token_mint: ctx.accounts.output_mint.to_account_info(),
    settlement_vault: ctx.accounts.settlement_vault.to_account_info(),
    settlement_vault_token: ctx.accounts.settlement_vault_token.to_account_info(),
    settlement_config: ctx.accounts.settlement_config.to_account_info(),
    settlement_program: ctx.accounts.settlement_program.to_account_info(),
    token_program: ctx.accounts.token_program.to_account_info(),
    system_program: ctx.accounts.system_program.to_account_info(),
};
let cpi_ctx = CpiContext::new(ctx.accounts.hooks_program.to_account_info(), cpi_accounts);
hooks::cpi::after_swap(cpi_ctx, trade_id, settlement_slots, amount_out)?;
```

Keep a `trade_id` allocator in the pool state (e.g. an incrementing `u64`) — the id names the escrow PDA, so it must never repeat.

### Integration pitfalls

1. **The AMM must pay into the hook authority's ATA.** Output tokens must land in `get_associated_token_address_with_program_id(hook_authority, output_mint, token_program)` *before* `after_swap` runs. A pool that pays the taker directly leaves nothing for the hook to capture, and the instruction's ATA check exists to make that failure loud (`InvalidCustodyAccount`) rather than silent.
2. **The escrow token account does not have to pre-exist.** The Settlement program creates the `[b"escrow", trade_id_le]` PDA in the CPI (owned by `[b"escrow_authority"]`), so the client passes that derived address, not an ATA. Creating an ATA for the vault PDA would put a different address in the account slot and fail the hook's pinning check.
3. **`trade_id` uniqueness is global and permanent.** The id determines the settlement vault PDA; `init` on an already-used id fails, including for `Settled`/`Cancelled` trades. Allocate it from the pool state, never from the caller.
4. **The amount is explicit, and the ATA is shared per mint.** `amount` is what this trade routed, so a second trade through the same ATA does not drag the first trade's leftovers into its escrow. Any balance already sitting in the ATA that no trade claimed stays there.
5. **Fund the hook authority first.** The vault rent is charged to the PDA. An unfunded PDA makes the CPI fail at rent — fund via `fund_hook_authority` (or a plain system transfer to the PDA) before going live.
6. **Two-hops-in-one-transaction.** The full flow — AMM swap → transfer to hook vault → `after_swap` → settlement CPI — fits in one transaction, but compute budget grows: swap math + two CPIs + a token transfer each. Budget accordingly (or split swap and hook across transactions *only* if you can tolerate the gap where the tokens sit in the hook vault unescrowed).
7. **`taker` is recorded, not verified, here.** The caller-supplied `taker` is written into the vault and later bound into the Transfer Agent's signature at settle time — the Transfer Agent's off-chain compliance review is what pins it. The hook itself does not (and cannot) verify that the taker is who received the swap.
8. **The hook is stateless.** Nothing in this program remembers trades. Observability means watching the settlement program's vault PDAs, not this program's state.
9. **Token-2022 with transfer hooks.** The escrow leg this instruction triggers needs the RWA mint's own hook accounts: the hook program, the mint's `extra-account-metas` PDA, both sides' `WhitelistEntry` PDAs and the hook config — Token-2022 resolves the mint's meta list against the transfer instruction's account list and fails (`IncorrectAccount`) when one is missing, and it builds the hook's data itself, so a proof can never be an argument. `after_swap` forwards whatever the caller puts in `remaining_accounts` straight into the settlement CPI; for a hooked mint the caller (the AMM, or a keeper composing the steps) has to supply them, and the **escrow's** owner — the fixed `[b"escrow_authority"]` PDA — has to be a whitelist member, as does the hook authority on the source side.
10. **Slippage/price logic stays in the AMM.** This program does no pricing, no minimums, no routing. It is a post-trade rail only.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `MintMismatch` (6000) | `Token account mint does not match the settlement mint` | `amm_vault` or `settlement_vault_token` mint disagrees with `token_mint`. |
| `InvalidSettlementVault` (6001) | `Settlement vault is not the expected settlement program PDA` | The passed escrow does not match `[b"settlement", trade_id_le]` on the Settlement program. |
| `InvalidVaultTokenAccount` (6002) | `Escrow token account is not the settlement escrow PDA` | `settlement_vault_token` is not `[b"escrow", trade_id_le]` on the Settlement program. |
| `SettlementSlotOverflow` (6003) | `Settlement slot addition overflowed` | `Clock::slot + settlement_slots` overflowed `u64`. |
| `InvalidCustodyAccount` (6004) | `Custody account is not the hook authority's ATA for the mint` | `amm_vault` is not `get_associated_token_address_with_program_id(hook_authority, token_mint, token_program)`. |
| `InvalidAmount` (6005) | `The routed amount must be greater than zero and no more than the custody balance` | `amount` is zero, or exceeds `amm_vault.amount` — the AMM did not route this trade's output. |

*(The hooks program defines its own `ErrorCode` enum starting at 6000, in declaration order — it is independent of the Settlement program's numbering, which re-uses some of the same names.)*

---

## 🧪 Testing & Verification

`after_swap` is covered from both ends:

* **Through a venue, in Rust.** `tests/e2e/tests/order_book.rs` loads
  `target/deploy/hooks.so` and `settlement.so` and drives a routed fill end to
  end (`routed_fill_accepts_any_nonzero_trade_id`,
  `settlement_routed_fill_escrows_then_releases_with_an_agent_signature`): the
  fill routes proceeds into a `Pending` vault, the test asserts the vault amount
  and the escrow balance, then advances the clock and releases with a real
  secp256k1 Transfer Agent signature. That is this program's CPI, its PDA
  derivation and the Settlement program's accounting in one transaction.
* **Directly, in TypeScript.** `tests/ts/rwa-dex.ts` drives
  `settlement::initiate_settlement` (transfer, time lock, release, refund)
  against a hook-gated Token-2022 mint, which is exactly the escrow leg
  `after_swap` invokes. The seeder's functionality suite drives the same CPI
  through a CLMM swap with the rail on.

What is still missing is a **negative** suite aimed at this program: a wrong
custody account, a mismatched settlement vault, a wrong mint and an unauthorized
caller should each fail with the specific error below. Those guards are
code-reviewed and reachable today, but not pinned by a test.

```bash
anchor build -p hooks         # builds the settlement CPI dependency too
anchor test                   # TS suite incl. the settlement flow above
```

### Known simplifications (vs. production hooks)

* **The caller decides the trade id.** `after_swap` accepts any unused id; uniqueness comes from the vault PDA's `init` failing on reuse, not from a registry. A production venue keeps its own monotonic counter: every caller today passes a caller-chosen id (a fill requires a non-zero `trade_id`), so id policy lives in the caller, not here.
* **Only `after_swap` is implemented.** A full Uniswap-v4-style hook set would add `before_swap` (e.g. settlement-window or KYC-gate checks), `after_liquidity_change`, fee routing, and similar lifecycle points; only the post-swap capture is built.
* **Single global hook authority.** One PDA fronts all pools and mints. A production build would namespace it per pool (`[b"hook_authority", pool.key()]`) so one pool's vault ownership cannot be reused to authorize another's capture, and per-pool funding floats stay separate.
* **No trade-id registry.** Uniqueness is enforced only by PDA collision (`init` failing). A production pool keeps its own monotonic `trade_id` counter in state.
* **No compute-budget management.** The hook adds two CPI hops to a swap; nothing here sets compute-unit limits or prioritization fees — the caller's transaction must budget for the extra work.
