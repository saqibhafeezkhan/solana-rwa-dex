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
* **Escrow token account pinned to the vault PDA.** `settlement_vault_token.owner == settlement_vault.key()` and mints must agree (`InvalidVaultTokenAccount` / `MintMismatch`).
* **Relative T+N, absolute on-chain.** Callers pass `settlement_slots` (a duration); the hook turns it into the absolute `settlement_slot` the Settlement program stores (`Clock::slot + N`).
* **Anyone can keep the hook funded.** `fund_hook_authority` is permissionless: the Settlement program charges vault rent to the `authority` it receives — this PDA — so it needs lamports. Anyone may top it up. The AMM has to pass it **writable** in its swap instruction for that charge to go through.
* **Token-2022-ready.** Accounts are `InterfaceAccount`/`Interface<TokenInterface>`, matching the Settlement program's transfers; classic SPL and Token-2022 mints both work.

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
                                           │  owns settlement_vault_token  │
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
| `settlement_vault_token` | mut; mint matches, owner == `settlement_vault` | Escrow token account — must already exist. |
| `settlement_program` | unchecked; identity enforced by the CPI (`invoke_signed` checks the callee) | The Settlement program itself. |
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
import {
  TOKEN_2022_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountIdempotentInstruction,
} from "@solana/spl-token";
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

const escrowTokenAta = (mint: PublicKey, vault: PublicKey) =>
  getAssociatedTokenAddressSync(mint, vault, true, TOKEN_2022_PROGRAM_ID);
```

### One-time setup: fund the hook authority and create the escrow account

```typescript
// 1. Give the hook authority lamports — it pays the settlement vault's rent.
await hooks.methods
  .fundHookAuthority(new anchor.BN(1_000_000_000)) // 1 SOL float
  .accounts({ payer: wallet.publicKey, hookAuthority: hookAuthorityPda })
  .rpc();

// 2. Create the settlement vault's token account BEFORE the first hooked trade.
//    The settlement program does not create it (see its README, pitfall #2).
const tradeId = new anchor.BN(1);
const vault = settlementVaultPda(tradeId);
await provider.sendAndConfirm(
  new anchor.web3.Transaction().add(
    createAssociatedTokenAccountIdempotentInstruction(
      wallet.publicKey, escrowTokenAta(mint, vault), vault, mint, TOKEN_2022_PROGRAM_ID
    )
  )
);
```

### Wire an AMM swap through the hook

```typescript
// The AMM's swap must pay its output into hookVaultAta(mint) — the account
// owned by the hook authority PDA — instead of the taker's wallet.

await hooks.methods
  .afterSwap(tradeId, new anchor.BN(settlementSlots)) // e.g. 100 slots ≈ T+40s
  .accounts({
    hookAuthority: hookAuthorityPda,
    taker: takerPubkey,                    // recorded in the vault; signed-for at settle
    ammVault: hookVaultAta(mint),          // must be owned by hook_authority
    tokenMint: mint,
    settlementVault: vault,                // must equal the derived PDA
    settlementVaultToken: escrowTokenAta(mint, vault),
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
2. **The escrow token account does not have to pre-exist.** The Settlement program creates it idempotently as the vault PDA's ATA, so a client may pass the empty system account a `client` would pass for an account it is about to create.
3. **`trade_id` uniqueness is global and permanent.** The id determines the settlement vault PDA; `init` on an already-used id fails, including for `Settled`/`Cancelled` trades. Allocate it from the pool state, never from the caller.
4. **The amount is explicit, and the ATA is shared per mint.** `amount` is what this trade routed, so a second trade through the same ATA does not drag the first trade's leftovers into its escrow. Any balance already sitting in the ATA that no trade claimed stays there.
5. **Fund the hook authority first.** The vault rent is charged to the PDA. An unfunded PDA makes the CPI fail at rent — fund via `fund_hook_authority` (or a plain system transfer to the PDA) before going live.
6. **Two-hops-in-one-transaction.** The full flow — AMM swap → transfer to hook vault → `after_swap` → settlement CPI — fits in one transaction, but compute budget grows: swap math + two CPIs + a token transfer each. Budget accordingly (or split swap and hook across transactions *only* if you can tolerate the gap where the tokens sit in the hook vault unescrowed).
7. **`taker` is recorded, not verified, here.** The caller-supplied `taker` is written into the vault and later bound into the Transfer Agent's signature at settle time — the Transfer Agent's off-chain compliance review is what pins it. The hook itself does not (and cannot) verify that the taker is who received the swap.
8. **The hook is stateless.** Nothing in this program remembers trades. Observability means watching the settlement program's vault PDAs, not this program's state.
9. **Token-2022 with transfer hooks.** Transfers go through the token interface, but transfer-hook extra accounts are *not* forwarded by `after_swap`. The repo's `rwa_mint` whitelist hook is satisfied at the participant level; mints with stricter hooks may need the accounts plumbed through `remaining_accounts`.
10. **Slippage/price logic stays in the AMM.** This program does no pricing, no minimums, no routing. It is a post-trade rail only.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `MintMismatch` (6000) | `Token account mint does not match the settlement mint` | `amm_vault` or `settlement_vault_token` mint disagrees with `token_mint`. |
| `InvalidSettlementVault` (6001) | `Settlement vault is not the expected settlement program PDA` | The passed escrow does not match `[b"settlement", trade_id_le]` on the Settlement program. |
| `InvalidVaultTokenAccount` (6002) | `Escrow token account is not the settlement vault PDA's ATA` | The escrow account is not `get_associated_token_address(settlement_vault, token_mint)`. |
| `SettlementSlotOverflow` (6003) | `Settlement slot addition overflowed` | `Clock::slot + settlement_slots` overflowed `u64`. |
| `InvalidCustodyAccount` (6004) | `Custody account is not the hook authority's ATA for the mint` | `amm_vault` is not `get_associated_token_address_with_program_id(hook_authority, token_mint, token_program)`. |
| `InvalidAmount` (6005) | `The routed amount must be greater than zero and no more than the custody balance` | `amount` is zero, or exceeds `amm_vault.amount` — the AMM did not route this trade's output. |

*(The hooks program defines its own `ErrorCode` enum starting at 6000, in declaration order — it is independent of the Settlement program's numbering, which re-uses some of the same names.)*

---

## 🧪 Testing & Verification

The TS integration tests in `tests/ts/rwa-dex.ts` run the settlement flow against localnet by **simulating the AMM with a wallet**: the wallet acts as the `authority` on `initiate_settlement` directly (the test notes that in a real flow, the AMM calls the Hook, which calls settlement), locks 500 000 000 units of a Token-2022 RWA mint with `taker = Institution B` and `T+100`, and asserts the time lock reverts an early settle with `SettlementPeriodNotOver`. The hook path itself (`after_swap` → CPI) is not exercised by an automated test yet; wiring `tests/e2e` coverage against `target/deploy/hooks.so` and `settlement.so` (LiteSVM, as the clmm/oracle/kyc_gate suites do) is the natural next step.

```bash
anchor build -p hooks         # builds the settlement CPI dependency too
anchor test                   # TS suite incl. the settlement flow above
```

### Known simplifications (vs. production hooks)

* **No pool integration.** No production AMM calls `after_swap` in this repo — the hook is exercised via the wallet-simulated flow in tests. Production wiring means the pool transfers output to the hook vault and CPIs (or a keeper composes both steps).
* **Only `after_swap` is implemented.** A full Uniswap-v4-style hook set would add `before_swap` (e.g. settlement-window or KYC-gate checks), `after_liquidity_change`, fee routing, and similar lifecycle points; only the post-swap capture is built.
* **Single global hook authority.** One PDA fronts all pools and mints. A production build would namespace it per pool (`[b"hook_authority", pool.key()]`) so one pool's vault ownership cannot be reused to authorize another's capture, and per-pool funding floats stay separate.
* **No trade-id registry.** Uniqueness is enforced only by PDA collision (`init` failing). A production pool keeps its own monotonic `trade_id` counter in state.
* **No compute-budget management.** The hook adds two CPI hops to a swap; nothing here sets compute-unit limits or prioritization fees — the caller's transaction must budget for the extra work.
