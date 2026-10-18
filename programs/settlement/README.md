# T+N Settlement Escrow Program

An Anchor-based Solana program that acts as the **clearinghouse for RWA trades**: the output of a swap is locked in a per-trade escrow PDA and cannot leave until the settlement slot has passed **and** the off-chain Transfer Agent has produced a valid secp256k1 signature authorizing the release — a T+1/T+2-style clearing cycle for tokenized real-world assets.

Program ID: `58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6`

---

## 🌟 Overview

Real-world assets do not settle at the swap: an institution that buys a tokenized T-bill receives the tokens only after the Transfer Agent — the regulated party that performs compliance checks on the trade — has signed off. This program implements that flow on-chain:

1. **A trade executes on the AMM.** Instead of paying the buyer directly, the AMM's Hooks program routes the output tokens into this program (`initiate_settlement`), which sweeps them into a per-trade **SettlementVault PDA** and records `(trade_id, taker, mint, amount, settlement_slot)` with status `Pending`.
2. **T+N elapses.** The vault carries an absolute `settlement_slot`. Nothing can leave before `Clock::slot >= settlement_slot` — the on-chain analogue of a T+1/T+2 clearing window.
3. **The Transfer Agent signs off-chain.** They sign `keccak256(trade_id || taker || mint || amount)` with a secp256k1 key — exactly the fields recorded in the vault, so the signature authorizes this trade and nothing else.
4. **Anyone submits `settle`.** The program recovers the signer's public key from the signature and compares it against the key stored in the global `SettlementConfig` PDA. On a match, the escrow pays out to the taker and the vault is marked `Settled`.
5. **Or the authority cancels.** If the Transfer Agent refuses (compliance failure, KYC break), the config authority can `cancel_settlement` and return the funds to the account that funded the escrow.

### Key Characteristics:

* **Two independent gates.** Time alone releases nothing, and a signature alone releases nothing: `settle` requires *both* the slot to have passed and a recovering signature. Either gate alone is inert.
* **secp256k1, not ed25519.** The Transfer Agent is an off-chain institutional party; their key is an uncompressed secp256k1 public key (`x || y`, 64 bytes) stored in its native form in the config. A secp256k1 key can never equal a Solana ed25519 address, so it has to live on-chain in that form to be comparable with a recovered key.
* **Message-bound authorization.** The signed preimage is exactly `trade_id (8 LE) || taker (32) || mint (32) || amount (8 LE)` — all read from the vault, not from instruction arguments. A signature cannot be reused for another trade, another taker, another mint, or a different amount.
* **Permissionless settlement.** `settle` takes *no signer at all*: the Transfer Agent's recovered signature is the authorization. The taker, the AMM, or a keeper can submit it once it is valid.
* **Authority-gated cancellation.** Only the config `authority` (intended to be a multisig) may cancel, and only while the trade is `Pending`. Cancelling is the compliance exit; there is no way to cancel a `Settled` trade or to settle a `Cancelled` one.
* **Token-2022-ready transfers.** All token moves go through `anchor_spl::token_interface` (`InterfaceAccount`/`Interface<TokenInterface>`), so both classic SPL and Token-2022 mints work with the same instruction set.
* **One vault per trade.** The vault PDA is `[b"settlement", trade_id.to_le_bytes()]` — deterministic, discoverable from the trade id alone, and permanent: a trade id can never be re-escrowed, even after settling or cancelling.

---

## 📐 Mathematical Model

### 1. The signed message

The Transfer Agent signs a 104-byte preimage, hashed with **keccak256**:

```
preimage  =  trade_id.to_le_bytes()   ( 8 bytes)   // vault.trade_id
          ||  taker                   (32 bytes)   // vault.taker (Pubkey)
          ||  token_mint              (32 bytes)   // vault.token_mint (Pubkey)
          ||  amount.to_le_bytes()    ( 8 bytes)   // vault.amount
message   =  keccak256(preimage)      (32 bytes)
```

Notes the signing backend must respect:

* All integers are **little-endian** `u64`s. All keys are the raw 32-byte Solana `Pubkey` bytes.
* The hash is keccak256 of the raw concatenation — **not** Ethereum's `personal_message` (`\x19Ethereum Signed Message` prefixed) scheme. `eth_sign` / `personal_sign` output will *not* verify; the backend must hash exactly this preimage.
* `amount` is the vault's recorded amount: the amount the caller routed for this trade, as **measured in the escrow** after the transfer (the balance delta, net of any Token-2022 transfer fee). It is not necessarily the source account's entire balance. The Transfer Agent must read the vault account to know it before signing.

### 2. Recovering the signer's key

The program uses the `secp256k1_recover` syscall (via `solana-secp256k1-recover`) on `(message, recovery_id, signature)` where `signature` is the 64-byte compact `r || s` and `recovery_id` is `0..=3`. Recovery reconstructs the public key `Q` from the signature itself:

$$R = (r, y_R) \quad\text{(the curve point with } x = r\text{, branch chosen by } v\text{)} \qquad u_1 = -z\,r^{-1},\; u_2 = s\,r^{-1} \pmod n \qquad Q = u_1 G + u_2 R$$

The recovered 64-byte key (`x || y`) is then compared to `config.transfer_agent_secp256k1`:

$$Q \stackrel{?}{=} K_{agent}$$

Only a signature produced by the private key behind the stored public key passes. A failed recovery (malformed signature, bad recovery id) is `SignatureVerificationFailed`; a successful recovery by any other key is `UnauthorizedTransferAgent`. Any of the four recovery ids is accepted — the equality check is what gates access, not the id.

### 3. The time lock

The lock is denominated in **slots**, not timestamps:

| Check | Rule | Error |
| :--- | :--- | :--- |
| At initiation | `settlement_slot > Clock::slot` — strictly in the future | `InvalidSettlementSlot` |
| At settlement | `Clock::slot >= settlement_slot` — inclusive unlock | `SettlementPeriodNotOver` |

At the usual pace of 2.5 slots/second (400 ms), one slot ≈ 0.4 s:

| Window | Slots (approx.) |
| :--- | :--- |
| T+100 slots (test default) | ~40 s |
| T+1 hour | ~9 000 |
| T+1 day | ~216 000 |
| T+2 days | ~432 000 |

Slot pacing varies with network conditions; slots are the honest measure of "blocks elapsed" but not of wall-clock time. The Hooks program converts a relative `settlement_slots` into this absolute target (`Clock::slot + N`, checked).

### Worked example

Trade 1: Institution B buys 500 000 000 units of an RWA mint, and the AMM routes the output through the hook. The vault PDA records `trade_id = 1`, `taker = Institution B`, `amount = 500 000 000`, `settlement_slot = current + 100`, status `Pending` — and the escrow now holds the tokens.

1. **Too early.** A `settle` attempt 50 slots later reverts with `SettlementPeriodNotOver`. Nothing is written.
2. **Sign.** The Transfer Agent builds the preimage `01…00 (trade_id=1, LE) || taker_pubkey || mint_pubkey || 00 C4 65… (500 000 000, LE)`, hashes it with keccak256, and signs with their secp256k1 private key, yielding `(r, s, recovery_id)`.
3. **Settle.** After slot 100, anyone submits `settle(signature, recovery_id)`. The program re-derives the same preimage **from the vault's own fields**, recovers `Q`, and matches it against the config's stored agent key. The escrow transfers all 500 000 000 units to an account owned by the recorded taker and marks the vault `Settled`. A second `settle` attempt fails with `AlreadySettled`.

---

## ⚙️ Program Constants

| Constant / Layout | Value | Purpose |
| :--- | :--- | :--- |
| Config PDA seed | `b"config"` | Single global `SettlementConfig`. |
| Vault PDA seed | `b"settlement"` | Followed by the 8-byte little-endian `trade_id`. |
| Signed preimage | `104` bytes | `8 + 32 + 32 + 8` as laid out in §1. |
| Hash function | `keccak256` | `solana-keccak-hasher`; matches the recovery syscall's expectations. |
| Agent key format | `[u8; 64]` | Uncompressed secp256k1 public key (`x \|\| y`) — not compressed (33 B), not an Ethereum address (20 B). |
| Recovery id | `0..=3` | Passed as `u8`; only used to reconstruct `Q`. |
| `SettlementVault::LEN` | `170` bytes | 8 discriminator + 8 + 32 + 32 + 8 + 8 + 8 + 1 + 32 + 32 + 1. |
| `SettlementConfig::LEN` | `138` bytes | 8 discriminator + 32 + 64 + 1 + 32 + 1. |
| `MintAgentKey::LEN` | `73` bytes | 8 discriminator + 64-byte secp256k1 key + 1 bump. |
| `EXPIRY_WINDOW_SLOTS` | `216_000` | Slots after `settlement_slot` at which a still-`Pending` escrow can only be refunded (≈ one day at 2.5 slots/s). Fixed in the program, so the AMM/hooks CPIs did not need a new argument. |
| Mint agent PDA seed | `b"agent"` | Followed by the mint pubkey; a per-mint Transfer Agent override. |

Rent: creating one vault PDA costs ≈ 0.0028 SOL rent-exempt (170-byte account); the escrow token account ≈ 0.002 SOL. In the hooks flow both are charged to the hook authority PDA (see the Hooks program README). `close_settlement` returns both to the config authority once the trade is terminal.

---

## 🔑 Account Architecture & PDAs

```
   Transfer Agent (off-chain)                 SettlementConfig PDA
   secp256k1 keypair                         ┌────────────────────────────┐
        │                                    │ [b"config"]                │
        │ signs keccak256(trade_id ||        │ authority (multisig)       │
        │  taker || mint || amount)          │ transfer_agent_secp256k1   │
        ▼                                    └──────────────┬─────────────┘
   ┌──────────────────┐   initiate_settlement               │ recovered-key check
   │  AMM output acct │   (authority = hook PDA   ┌─────────▼───────────────────┐
   │  (hook authority)│    or the AMM itself)     │   SettlementVault PDA       │
   └────────┬─────────┘ ─────── tokens ─────────▶ │ [b"settlement", trade_id_le]│
            │                                     │  records the trade          │
            │                                     └──────────────┬──────────────┘
            │                                                    │ escrow
            │                                     ┌──────────────▼──────────────┐
            │                                     │  [b"escrow", trade_id_le]    │
            │                                     │  owned by escrow_authority   │
            │                                     └──────────────┬──────────────┘
            │        settle(TA sig)  ──▶ taker_token_account (recorded taker)
            │        cancel(authority) ─▶ refund_token_account
            ▼
   status: Pending ──▶ Settled / Cancelled / Expired  (terminal states)
```

`Pending` trades have a closed release window: `settlement_slot <= slot < expiry_slot`. Past `expiry_slot` only `refund_expired` can move the escrow, and any terminal trade can be closed to reclaim its rent.

### PDA Seeds Table

| Account | PDA Seeds Format | Description |
| :--- | :--- | :--- |
| **SettlementConfig** | `[b"config"]` | Global settings: the cancel authority, the authorized Transfer Agent's secp256k1 key, the initiation policy (`pools_require_gate`) and the hooks authority. One per program. |
| **SettlementVault** | `[b"settlement", trade_id.to_le_bytes()]` | Per-trade state. `trade_id` is little-endian `u64`. Records the trade; it does not own the escrow token account. |
| **EscrowAuthority** | `[b"escrow_authority"]` | One fixed owner for every trade's escrow token account (`[b"escrow", trade_id_le]`), so a single whitelist entry covers every settlement transfer. Signs releases and refunds. |
| **MintAgentKey** | `[b"agent", mint]` | Optional per-mint Transfer Agent override. When present and passed to `settle`, its key replaces the config's global key for that mint. Created/updated by `set_mint_agent`, removed (and rent refunded) by `clear_mint_agent`. |

### Account layout

`SettlementConfig` (138 bytes including the 8-byte discriminator):

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `authority` | `Pubkey` | May call `set_transfer_agent`, `set_hooks_authority` and `cancel_settlement`. Assign a multisig. |
| `transfer_agent_secp256k1` | `[u8; 64]` | Uncompressed secp256k1 key of the Transfer Agent; zeroed until `set_transfer_agent` runs. |
| `pools_require_gate` | `u8` | `1` when only wired AMM pool PDAs (or the hooks authority) may initiate; fixed at `initialize`. |
| `hooks_authority` | `Pubkey` | The hooks program's `hook_authority` PDA, whitelisted via `set_hooks_authority`; default disables hook routing. |
| `bump` | `u8` | Config PDA bump. |

`SettlementVault` (170 bytes including the discriminator):

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `trade_id` | `u64` | Caller-chosen trade id; also the PDA seed. |
| `taker` | `Pubkey` | Recorded recipient; bound into the Transfer Agent's signed message and enforced at payout. |
| `token_mint` | `Pubkey` | Mint of the escrowed token; all downstream token accounts are checked against it. |
| `amount` | `u64` | Escrowed amount — what the escrow account actually **received** at initiation (measured, not assumed). |
| `settlement_slot` | `u64` | Absolute unlock slot (T+N). |
| `expiry_slot` | `u64` | Absolute slot after which a still-`Pending` escrow can only be refunded (`settlement_slot + EXPIRY_WINDOW_SLOTS`). |
| `status` | `SettlementStatus` | `Pending` / `Settled` / `Cancelled` / `Expired`; everything but `Pending` is terminal. |
| `pool` | `Pubkey` | The AMM pool PDA that initiated the trade, or the default pubkey on the standalone path; only it may cancel via the pool path. |
| `refund_source` | `Pubkey` | The token account that funded the escrow. The **only** account `cancel_settlement` may refund to. |
| `bump` | `u8` | Vault PDA bump. Payout and refund are signed by the separate `[b"escrow_authority"]` PDA, not by this vault. |

`MintAgentKey` (73 bytes including the discriminator):

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `transfer_agent_secp256k1` | `[u8; 64]` | Uncompressed secp256k1 key for this mint; wins over the config's global key. |
| `bump` | `u8` | `[b"agent", mint]` PDA bump. |

---

## 🛠️ Instructions API

### 1. `initialize`

Creates the global `SettlementConfig` PDA with the caller as `authority` and a zeroed Transfer Agent key (trades cannot be authorized until `set_transfer_agent` runs).

Restricted to the program's **upgrade authority**, read out of this program's `ProgramData` account (address-pinned to `[program]` under the BPF upgradeable loader, owner-pinned to the loader) and matched against the signer (`UnauthorizedInitializer`). The config authority can release escrowed value with one signature, so who may create it must not be decided by whoever calls first.

* **Parameters**: none.
* **Accounts**: `settlement_config` (init, `[b"config"]`), `authority` (signer, rent payer, must be the program's upgrade authority), `program_data` (this program's `ProgramData` PDA, read-only), `system_program`.

### 2. `set_transfer_agent`

```rust
pub fn set_transfer_agent(ctx: Context<SetTransferAgent>, agent_secp256k1_pubkey: [u8; 64]) -> Result<()>
```

Sets — or rotates — the authorized Transfer Agent key. Config-authority only (`has_one = authority`); a zero key is rejected (`InvalidTransferAgentKey`). The key is the **uncompressed** 64-byte form: `getPublicKey(priv, /* compressed= */ false)` in most JS libraries, with the leading `0x04` byte stripped.

### 3. `initiate_settlement`

```rust
pub fn initiate_settlement(
    ctx: Context<InitiateSettlement>,
    trade_id: u64,
    settlement_slot: u64,
    amount: u64,
) -> Result<()>
```

Creates the vault PDA for `trade_id` and moves `amount` from the source account into escrow.

* **Parameters**:
  * `trade_id: u64`: Caller-chosen, must be unused — the PDA `init` fails if the trade id was ever escrowed before, even after settling or cancelling.
  * `settlement_slot: u64`: Absolute unlock slot, strictly in the future (`InvalidSettlementSlot`).
  * `amount: u64`: The flow of *this* trade. It is stated rather than inferred from the source balance, and checked against it (`InvalidSettlementAmount` when zero or larger than the balance) — otherwise a caller would be escrowing whatever happened to be sitting in the account.
* **Flow**: record `(trade_id, taker, mint, settlement_slot, Pending, pool, refund_source)` → create the `[b"escrow", trade_id_le]` token account owned by `[b"escrow_authority"]` → move `amount` into it with the **checked** transfer instruction (`transfer_checked`, mint and decimals included), signed by `authority` as an ordinary signer (in the hooks flow that signature arrives via the hooks program's PDA seeds — no program seeds are needed here) → measure the escrow's balance delta and store it in `vault.amount`, then require it to be non-zero (`EmptySettlement`).
* **Validations**: `source_token_account.owner == authority.key()` (`SourceNotOwnedByAuthority`); `vault_token_account` mint == `token_mint` and owner == the fixed `[b"escrow_authority"]` PDA (`MintMismatch` / `InvalidVaultTokenAccount`); the initiation policy (pool gate or hooks authority, below).
* **Accounts**: `settlement_vault` (init), `source_token_account` (mut), `vault_token_account` (mut), `token_mint`, `taker` (unchecked `AccountInfo` — recorded and later bound into the signed message), `authority` (signer, mut — also the rent payer), `pool` (optional), `settlement_config`, `token_program` (Interface), `system_program`, then `remaining_accounts`.
* **Note**: the escrow token account does **not** have to exist yet — this instruction creates it as the `[b"escrow", trade_id_le]` PDA and initializes it as a token account of `token_mint` owned by `[b"escrow_authority"]`. The client must pass that PDA (derive it with `settlementEscrowToken(tradeId)`), not an associated token account.
* **`remaining_accounts`**: the transfer's **hook accounts**, for a mint with an active transfer hook (`rwa_mint`'s whitelist hook): the hook program, the mint's `extra-account-metas` PDA, both sides' `WhitelistEntry` PDAs and the hook config. Token-2022 resolves the mint's meta list against the transfer instruction's own account list and fails with `IncorrectAccount` when one is missing, so they are appended to the transfer as-is. Harmless (and unread) for a mint without a hook.

### 4. `settle`

```rust
pub fn settle(ctx: Context<Settle>, signature: [u8; 64], recovery_id: u8) -> Result<()>
```

Releases the escrow. **No signer is required** — the recovered signature is the authorization.

* **Parameters**: `signature: [u8; 64]` (compact `r || s`), `recovery_id: u8` (0–3).
* **Flow**: status must be `Pending` (`AlreadySettled`) → the trade must still be inside its release window: `slot < expiry_slot` (`SettlementExpired`) and `slot >= settlement_slot` (`SettlementPeriodNotOver`) → the signature must be **low-s** (`HighSignatureS`; each authorization then has exactly one wire form for audit logs) → rebuild the 104-byte preimage from the vault's own `trade_id` / `taker` / `token_mint` / `amount` → `keccak256` → `secp256k1_recover` → compare against the mint's agent override when one was passed, otherwise the config's global key (`SignatureVerificationFailed` / `UnauthorizedTransferAgent` / `InvalidMintAgent`) → move the full `amount` to the taker's account with `transfer_checked`, signed by the escrow authority PDA (`[b"escrow_authority", bump]`) → mark `Settled`.
* **Validations**: `vault_token_account` mint/owner checks as above; `taker_token_account` mint == the vault's mint and owner == the vault's recorded `taker` (`InvalidTakerTokenAccount`) — the payout address is therefore pinned to the taker the Transfer Agent signed for; `token_mint.key() == vault.token_mint` (`MintMismatch`); a supplied `mint_agent` must be the canonical `[b"agent", mint]` PDA (`InvalidMintAgent`), so a caller cannot substitute a key of their choosing.
* **Accounts**: `settlement_vault` (mut), `settlement_config`, `mint_agent` (optional — the settlement program ID is Anchor's `None` sentinel), `token_mint`, `vault_token_account` (mut), `escrow_authority`, `taker_token_account` (mut), `token_program` (Interface), then `remaining_accounts` (the release transfer's hook accounts — here the **escrow** is the sender, so that side's entry is the escrow authority's own).
* **Note**: the release window is closed — a trade not released before `expiry_slot` can only be refunded by `refund_expired`.

### 5. `cancel_settlement`

```rust
pub fn cancel_settlement(ctx: Context<CancelSettlement>) -> Result<()>
```

Compliance exit: returns the full escrow to a refund account and marks the trade `Cancelled`.

* **Parameters**: none.
* **Validations**: signer is the config `authority` (`Unauthorized`) or the initiating pool PDA (pool path); status is `Pending` (`NotPending`); `refund_token_account.key() == settlement_vault.refund_source` (`InvalidRefundAccount`) and its mint == the vault's mint (`MintMismatch`); `token_mint.key() == vault.token_mint` (`MintMismatch`).
* **Accounts**: `settlement_vault` (mut), `settlement_config` (optional), `pool` (optional), `token_mint`, `vault_token_account` (mut), `escrow_authority`, `refund_token_account` (mut), `authority` (signer), `token_program` (Interface), then `remaining_accounts` (the refund transfer's hook accounts, the escrow side being the escrow authority's).
* **Note**: the refund goes back to the account that funded the escrow, recorded on the vault at initiation. The canceller cannot redirect it — the destination is pinned by the vault's own state, not chosen at cancellation time.

### 6. `refund_expired`

```rust
pub fn refund_expired(ctx: Context<RefundExpired>) -> Result<()>
```

Permissionless auto-refund for a trade whose T+N window lapsed without a Transfer Agent release.

* **Validations**: status is `Pending` (`NotPending`); `slot >= expiry_slot` (`SettlementNotExpired`); `refund_token_account.key() == vault.refund_source` (`InvalidRefundAccount`) and its mint matches (`MintMismatch`). No signer and no config account are needed: the destination is pinned by the vault's own state, so a keeper can trigger the refund but cannot redirect a single unit.
* **Flow**: `transfer_checked` the full `amount` from the escrow back to `refund_source`, signed by the escrow authority → status `Expired`.
* **Accounts**: `settlement_vault` (mut), `token_mint`, `vault_token_account` (mut), `escrow_authority`, `refund_token_account` (mut), `token_program` (Interface), then `remaining_accounts` (the refund transfer's hook accounts).

### 7. `close_settlement`

```rust
pub fn close_settlement(ctx: Context<CloseSettlement>) -> Result<()>
```

Reclaims the rent of a terminal trade: closes the empty escrow token account and the vault PDA, both paying the config authority.

* **Validations**: config authority only (`Unauthorized`); status is not `Pending` (`StillPending`); the escrow balance is zero (`EscrowNotEmpty`); the escrow account is the trade's PDA and owned by the escrow authority.
* **Accounts**: `settlement_vault` (mut, `close = authority`), `settlement_config`, `authority` (signer, mut), `token_mint`, `vault_token_account` (mut), `escrow_authority`, `token_program` (Interface).
* **Note**: closing is not a transfer, so a hook-bearing mint needs no hook accounts here.

### 8. `set_mint_agent` / `clear_mint_agent`

```rust
pub fn set_mint_agent(ctx: Context<SetMintAgent>, agent_secp256k1_pubkey: [u8; 64]) -> Result<()>
pub fn clear_mint_agent(ctx: Context<ClearMintAgent>) -> Result<()>
```

Per-mint Transfer Agent overrides, config-authority only. `set_mint_agent` creates the `[b"agent", mint]` PDA if needed (or updates it in place) with the same uncompressed 64-byte key format as the global key; a zero key is rejected (`InvalidTransferAgentKey`). `clear_mint_agent` closes the override and refunds its rent to the config authority, reverting that mint to the config's global key. Setting the global key again on one mint is allowed, so a desk can override a mint and later remove the override.

* **Accounts (`set`)**: `settlement_config`, `authority` (signer, mut), `token_mint`, `mint_agent` (init-if-needed, `[b"agent", mint]`), `system_program`.
* **Accounts (`clear`)**: `settlement_config`, `authority` (signer, mut), `token_mint`, `mint_agent` (mut, `close = authority`).

---

## 📄 Data Structures

### `SettlementStatus`

```rust
pub enum SettlementStatus {
    Pending,    // Escrowed; waiting for T+N and the Transfer Agent
    Settled,    // Released to the taker — terminal
    Cancelled,  // Returned to the refund account by the authority/pool — terminal
    Expired,    // Returned to the refund account by the permissionless deadline — terminal
}
```

### `SettlementVault` Account

```rust
pub struct SettlementVault {
    pub trade_id: u64,          // PDA seed; caller-chosen, unique forever
    pub taker: Pubkey,          // Recorded recipient, bound into the TA signature
    pub token_mint: Pubkey,     // Escrowed mint
    pub amount: u64,            // Measured amount that entered escrow at initiation
    pub settlement_slot: u64,   // Absolute unlock slot (T+N)
    pub expiry_slot: u64,       // After this slot a Pending trade can only be refunded
    pub status: SettlementStatus,
    pub pool: Pubkey,           // Initiating AMM pool, or default on the standalone path
    pub refund_source: Pubkey,  // The only account cancel/refund may pay
    pub bump: u8,               // Vault PDA bump (payout signs as escrow_authority)
}
```

### `SettlementConfig` Account

```rust
pub struct SettlementConfig {
    pub authority: Pubkey,                      // Cancel + key-rotation authority (multisig)
    pub transfer_agent_secp256k1: [u8; 64],     // Uncompressed secp256k1 key (x || y)
    pub pools_require_gate: u8,                 // 1 = only wired pool PDAs / hooks authority may initiate
    pub hooks_authority: Pubkey,                // Hooks PDA allowed to initiate; default disables hook routing
    pub bump: u8,
}
```

---

## 🔌 TypeScript Integration

Uses `@coral-xyz/anchor` with the IDL `anchor build` writes to `target/types/settlement.ts`, plus `@solana/spl-token`.

### PDAs and helpers

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import { Settlement } from "../target/types/settlement";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.Settlement as Program<Settlement>;

const configPda = PublicKey.findProgramAddressSync(
  [Buffer.from("config")],
  program.programId
)[0];

const vaultPda = (tradeId: number | anchor.BN) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("settlement"), new anchor.BN(tradeId).toArrayLike(Buffer, "le", 8)],
    program.programId
  )[0];

/** Escrow token account: [b"escrow", trade_id_le], owned by [b"escrow_authority"]. */
const escrowTokenPda = (tradeId: number | anchor.BN) =>
  PublicKey.findProgramAddressSync(
    [Buffer.from("escrow"), new anchor.BN(tradeId).toArrayLike(Buffer, "le", 8)],
    program.programId
  )[0];

const escrowAuthorityPda = PublicKey.findProgramAddressSync(
  [Buffer.from("escrow_authority")],
  program.programId
)[0];
```

### Set / rotate the Transfer Agent key

```typescript
// agentPubkey = secp256k1.getPublicKey(agentPriv, false).slice(1)  → 64 bytes (x || y)
await program.methods
  .setTransferAgent([...agentPubkey])
  .accounts({ settlementConfig: configPda, authority: wallet.publicKey })
  .rpc();
```

### Initiate a settlement (AMM / hook side)

```typescript
const tradeId = new anchor.BN(1);
const settlementSlot = new anchor.BN(currentSlot + 100); // T+100 slots, absolute
const vault = vaultPda(tradeId);

await program.methods
  .initiateSettlement(tradeId, settlementSlot, amount) // absolute slot, and the trade's flow
  .accounts({
    settlementVault: vault,
    sourceTokenAccount: ammOutputAta,        // must be owned by `authority`
    settlementVaultToken: escrowTokenPda(tradeId), // [b"escrow", trade_id_le]; created by the program
    tokenMint: mint,
    taker: takerPubkey,                      // recorded, later bound into the TA message
    authority: ammAuthorityPubkey,
    pool: program.programId,                 // the None sentinel: standalone (wallet) flow
    settlementConfig: configPda,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
    systemProgram: SystemProgram.programId,
  })
  // For a mint with an active transfer hook, the hook's accounts go here (see
  // "Transfer-hook accounts" below): the web client builds them with
  // `hookTransferAccounts(mint, sourceOwner, destinationOwner)`.
  .remainingAccounts(hookAccounts)
  .signers([ammAuthority]) // the hooks program supplies its PDA signature via CPI seeds instead
  .rpc();
```

### Settle with a real Transfer Agent signature

`tests/ts/rwa-dex.ts` signs exactly this way (the demo agent key is the secp256k1 generator point, i.e. the public key of the scalar `1`), so the code below is the same path the suite runs:

```typescript
// npm i keccak256 @noble/secp256k1
import keccak256 from "keccak256";
import { sign } from "@noble/secp256k1"; // any lib returning (64B compact sig, recovery id) works

const vaultState = await program.account.settlementVault.fetch(vault);

const preimage = Buffer.concat([
  vaultState.tradeId.toArrayLike(Buffer, "le", 8),
  vaultState.taker.toBuffer(),
  vaultState.tokenMint.toBuffer(),
  vaultState.amount.toArrayLike(Buffer, "le", 8),
]);
const messageHash = keccak256(preimage); // keccak256 of the raw preimage — NOT personal_sign

// @noble/secp256k1 v1.x: `{ recovered: true }` returns [64-byte sig, recovery id].
const [sig, recoveryId] = await sign(messageHash, transferAgentPrivateKey, { recovered: true });

await program.methods
  .settle([...sig], recoveryId)
  .accounts({
    settlementVault: vault,
    settlementConfig: configPda,
    // Pass the [b"agent", mint] PDA when the mint has an override; otherwise
    // the program ID is Anchor's Option None sentinel (global key).
    mintAgent: program.programId,
    tokenMint: mint,
    vaultTokenAccount: escrowTokenPda(tradeId),
    escrowAuthority: escrowAuthorityPda,
    takerTokenAccount: getAssociatedTokenAddressSync(
      mint, vaultState.taker, false, TOKEN_2022_PROGRAM_ID
    ),
    tokenProgram: TOKEN_2022_PROGRAM_ID,
  })
  .remainingAccounts(hookAccounts) // the escrow is the sender here
  .rpc(); // permissionless — no signer needed
```

### Cancel a pending trade

```typescript
await program.methods
  .cancelSettlement()
  .accounts({
    settlementVault: vault,
    settlementConfig: configPda,
    pool: program.programId,       // the None sentinel: the config-authority path
    tokenMint: mint,
    vaultTokenAccount: escrowTokenPda(tradeId),
    escrowAuthority: escrowAuthorityPda,
    refundTokenAccount: refundAta, // must equal vault.refund_source
    authority: wallet.publicKey,   // config authority — a multisig in production
    tokenProgram: TOKEN_2022_PROGRAM_ID,
  })
  .remainingAccounts(hookAccounts)
  .rpc();
```

### Refund an expired trade (permissionless)

```typescript
await program.methods
  .refundExpired()
  .accounts({
    settlementVault: vault,
    tokenMint: mint,
    vaultTokenAccount: escrowTokenPda(tradeId),
    escrowAuthority: escrowAuthorityPda,
    refundTokenAccount: refundAta, // vault.refund_source — pinned on chain
    tokenProgram: TOKEN_2022_PROGRAM_ID,
  })
  .remainingAccounts(hookAccounts)
  .rpc(); // anyone may crank it; the destination cannot be redirected
```

### Close a terminal trade and reclaim rent

```typescript
await program.methods
  .closeSettlement()
  .accounts({
    settlementVault: vault,          // must not be Pending
    settlementConfig: configPda,
    authority: wallet.publicKey,     // config authority, receives both rents
    tokenMint: mint,
    vaultTokenAccount: escrowTokenPda(tradeId), // must be empty
    escrowAuthority: escrowAuthorityPda,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
  })
  .rpc();
```

### Per-mint Transfer Agent keys

```typescript
const [mintAgent] = PublicKey.findProgramAddressSync(
  [Buffer.from("agent"), mint.toBuffer()],
  program.programId
);

await program.methods
  .setMintAgent([...agentPubkey]) // 64-byte uncompressed key for this mint
  .accounts({
    settlementConfig: configPda,
    authority: wallet.publicKey,
    tokenMint: mint,
    mintAgent,
    systemProgram: SystemProgram.programId,
  })
  .rpc();

// Passing `mintAgent` to `settle` is what makes the override apply; omitting it
// (the program-ID sentinel) checks the config's global key instead.
await program.methods.clearMintAgent().accounts({
  settlementConfig: configPda,
  authority: wallet.publicKey,
  tokenMint: mint,
  mintAgent,
}).rpc(); // closes the override, refunding its rent to the authority
```

### Transfer-hook accounts (RWA mints)

`rwa_mint` mints carry the transfer-hook extension, and moving them needs two things a legacy transfer does not provide:

* **the mint.** Token-2022 rejects `Transfer` for any account of a hook-bearing mint (`MintRequiredForTransfer`, 0x1f), so every move here is `transfer_checked` — which is why `settle` and `cancel_settlement` take a `token_mint` account (pinned to the vault's own mint), and `initiate_settlement` already had one.
* **the hook's accounts in the transfer's account list**: the hook program, the mint's `extra-account-metas` PDA, both sides' `WhitelistEntry` PDAs and the hook config. Token-2022 resolves the mint's meta list against the *instruction's* account list and fails with `IncorrectAccount` when one is missing, and `anchor_spl`'s typed transfer builds a fixed four-account instruction (it ignores `remaining_accounts`), so the program assembles the transfer itself to append them. Pass them as the instruction's `remaining_accounts`; the web client's `hookTransferAccounts(mint, sourceOwner, destinationOwner)` builds exactly that list.

The **escrow is a participant** as far as the hook is concerned: it checks both sides of every transfer, and Token-2022 derives each side's entry from its token account's `owner` field. Every escrow token account is owned by the fixed `[b"escrow_authority"]` PDA, so one `[b"whitelist", escrow_authority]` entry — registered by the whitelist authority with `transfer_hook::set_whitelist_entry` — covers every trade, exactly as it would for an institution's custody PDA. The funder (or taker) on the other side still needs its own entry.

### Integration pitfalls

1. **Point it at a per-trade account.** `amount` is stated and checked against the source balance, and the vault stores what the escrow actually received. A shared vault holding unrelated funds still invites mistakes — the flow of one trade should be one account (or an account holding exactly that trade's output).
2. **The escrow token account does not have to pre-exist.** `initiate_settlement` creates the `[b"escrow", trade_id_le]` PDA itself (as a plain token account owned by `[b"escrow_authority"]`), so a client passes that derived address, not an ATA. `settle` and `cancel_settlement` need it to exist, which it does once the trade is escrowed.
3. **`trade_id` is forever.** The PDA `init` makes every id single-use across the program's lifetime, including after `Settled`/`Cancelled`. Make your off-chain trade registry the source of uniqueness (a counter, or a hash of the trade ticket).
4. **Slots, not seconds.** Pass the *absolute* unlock slot to `initiate_settlement`; the Hooks program computes it from a relative `settlement_slots` for you. Slot pacing wobbles — do not promise wall-clock settlement times derived from slots.
5. **The signature binds `(trade_id, taker, mint, amount)` — nothing else.** It does not bind the settlement slot, the expiry slot or any account addresses of the settlement program. A captured signature is valid only for its exact trade, and only while that trade is `Pending` inside its release window — after `expiry_slot` the escrow can only be refunded, so the signature becomes unusable.
6. **keccak256 of the raw preimage.** Ethereum's `personal_sign` / `eth_sign` wrappers prepend a message prefix and will never verify. Hash exactly the 104 bytes of §1.
7. **Uncompressed key format.** `set_transfer_agent` wants 64 bytes `x || y`. Compressed (33 B) keys and Ethereum addresses (20 B) are the classic integration bug.
8. **Permissionless `settle` is a feature.** No wallet is required to submit — which means keepers can settle on schedule, and nobody can front-run the payout to a different address: the taker token account must be owned by the vault's recorded `taker`.
9. **Global key plus per-mint overrides.** The config holds the fallback secp256k1 key; `set_mint_agent` installs a `[b"agent", mint]` key that `settle` prefers when that account is passed. Rotating either invalidates nothing already released (each trade settles once), and old signatures fail after rotation — usually the point. Remember to pass the override account: without it `settle` silently checks the global key.
10. **Token-2022 with transfer hooks.** Every move is `transfer_checked`, and the hook accounts are forwarded from `remaining_accounts` (both above). Two consequences: a caller that forgets them gets `IncorrectAccount` from the token program rather than a settlement error, and the escrow's owner has to be a whitelist member — for a mint whose hook gates both sides, a settlement of an RWA mint is a *compliance* action, not just a token move.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `InvalidSettlementSlot` (6000) | `Settlement slot must be in the future` | `settlement_slot <= Clock::slot` at initiation. |
| `SettlementPeriodNotOver` (6001) | `Settlement period has not yet ended` | `Clock::slot < vault.settlement_slot` at settle time. |
| `AlreadySettled` (6002) | `Trade is already settled, cancelled or expired` | `settle` on a vault not in `Pending`. |
| `NotPending` (6003) | `Trade is not pending` | `cancel_settlement` on a vault not in `Pending`. |
| `SignatureVerificationFailed` (6004) | `Transfer Agent signature verification failed` | `secp256k1_recover` rejected the `(signature, recovery_id)`. |
| `UnauthorizedTransferAgent` (6005) | `Recovered key does not match the authorized Transfer Agent` | Signature is valid but was made by a different secp256k1 key. |
| `InvalidTransferAgentKey` (6006) | `The configured Transfer Agent key cannot be empty` | `set_transfer_agent` given the all-zero key. |
| `SourceNotOwnedByAuthority` (6007) | `The source token account is not owned by the signing authority` | The swept account belongs to someone other than the `authority` signer. |
| `MintMismatch` (6008) | `Token account mint does not match the settlement mint` | A token account's mint disagrees with `token_mint` / the vault's mint. |
| `InvalidVaultTokenAccount` (6009) | `Escrow token account is not the trade's escrow PDA, or it is not owned by the escrow authority` | `vault_token_account` is not `[b"escrow", trade_id_le]` or is not owned by `[b"escrow_authority"]`. |
| `InvalidTakerTokenAccount` (6010) | `The taker token account does not belong to the vault's taker` | Payout destination not owned by the recorded `taker`. |
| `EmptySettlement` (6011) | `Nothing to settle: the source account is empty` | Source balance is zero at initiation. |
| `Unauthorized` (6012) | `Only the config authority may perform this action` | `set_transfer_agent` / `cancel_settlement` signer is not the config authority. |
| `UnauthorizedPool` (6013) | `Caller is not a pool authorized to route trades into settlement` | `initiate_settlement` signed by a pool that is not the one wired into this config. |
| `GateRequired` (6014) | `This settlement config requires initiation by a wired AMM pool` | `initiate_settlement` sent directly, without the wired pool CPI-ing it. |
| `UnauthorizedInitializer` (6015) | `Only the program's upgrade authority may initialize the settlement config` | `initialize` signer is not the ProgramData upgrade authority. |
| `InvalidRefundAccount` (6016) | `Refund account is not the account that funded this escrow` | `refund_token_account` is not `vault.refund_source`. |
| `InvalidSettlementAmount` (6017) | `The settlement amount must be greater than zero and no larger than the source balance` | `amount` is zero, or larger than the source token account's balance. |
| `SettlementExpired` (6018) | `The trade's T+N release window has expired; it can only be refunded` | `settle` at or after `expiry_slot`. |
| `SettlementNotExpired` (6019) | `The trade has not reached its refund deadline yet` | `refund_expired` before `expiry_slot`. |
| `HighSignatureS` (6020) | `The secp256k1 signature is not in low-s form` | `settle` given the `(s, n-s)` twin of a valid signature. |
| `InvalidTaker` (6021) | `The recorded taker cannot be the default pubkey` | `initiate_settlement` with `taker = Pubkey::default()`. |
| `InvalidMintAgent` (6022) | `The per-mint agent account is not the canonical [b"agent", mint] PDA` | `settle` given an account other than the mint's override PDA. |
| `EscrowNotEmpty` (6023) | `The escrow still holds tokens; settle, cancel or refund it first` | `close_settlement` while the escrow balance is non-zero. |
| `StillPending` (6024) | `The trade is still pending; settle, cancel or expire it before closing` | `close_settlement` on a `Pending` vault. |

---

## 🧪 Testing & Verification

The TS integration tests in `tests/ts/rwa-dex.ts` (run by `anchor test` against a local validator) drive all three token moves against a **hook-gated Token-2022 RWA mint**, which is the case that used to fail:

* **Escrow creation.** Institution A's custody ATA holding 500 000 000 units of the RWA mint is swept into the `[b"settlement", 1]` vault PDA with `taker = Institution B` and `T+100` slots, and the test asserts the vault records how much landed and that the escrow holds exactly that.
* **Time-lock enforcement.** `settle` before the unlock slot reverts with `SettlementPeriodNotOver`, leaving the vault untouched.
* **Signature-verified release.** The validator clock is advanced past the unlock slot, the agent signs the §1 preimage with a real secp256k1 key (the demo key is the generator point, private scalar `1`), and `settle` pays the taker and empties the escrow.
* **Compliance refund.** A second trade is cancelled, asserting the escrow returns to the account that funded it.

All four pass. Both sides of every transfer have to be whitelisted members for this to work, so the suite publishes a whitelist that contains the institutions **and** the fixed `[b"escrow_authority"]` PDA, then registers an entry for each — the same `update_root` + one `set_whitelist_entry` per member sequence an operator would run. Because the escrow owner is fixed, one entry covers every trade.

The settlement crate's unit tests cover the low-s predicate (boundary, high twin, maximum), the account-size constants and the expiry arithmetic. The deadline and rent-reclamation paths (`refund_expired`, `close_settlement`) and the per-mint override (`set_mint_agent` / `clear_mint_agent`) have no end-to-end test yet: the TS suite cannot reach `refund_expired` without warping 216 000 slots, and it does not exercise `close_settlement`. The web client builds all four; treat them as code-reviewed but not integration-verified.

Before the fix these cases failed at the first transfer: `initiate_settlement` and `settle` moved tokens with the legacy `Transfer` CPI, which Token-2022 refuses for any account of a hook-bearing mint (`MintRequiredForTransfer`, 0x1f). Moving an RWA mint requires `transfer_checked`, the `token_mint` account (added to `settle` / `cancel_settlement` — an ABI change) and the mint's hook accounts appended to the transfer. The seeder (`bun run bootstrap:localnet`) covers the same three instructions through the web client against a plain SPL mint.

There is no `tests/e2e` Rust suite for this program yet — wiring one against `target/deploy/settlement.so` (as the clmm/oracle/kyc_gate suites do with LiteSVM) is the natural next step.

```bash
anchor build --arch v0         # SBF build (`--arch v0`: see scripts/programs.sh)
anchor test                    # TS suite + cargo tests, both green
bun run bootstrap:localnet     # deploys and seeds, then exercises every builder
```

### Known simplifications (vs. production clearinghouses)

* **One fixed escrow authority.** Every escrow token account is owned by `[b"escrow_authority"]`, so the whitelist needs one entry for the escrow side, not one per trade. The trade identity remains in the per-trade vault and `[b"escrow", trade_id_le]` account. A root rotation still invalidates every member's entry until it is refreshed — that is inherent to the Merkle whitelist, not to the escrow model.
* **Low-s is enforced.** `settle` rejects `s > n/2` (`HighSignatureS`), so each authorization has exactly one wire form — which is what an audit log needs.
* **Per-mint keys are opt-in.** An override only applies when `settle` is handed the `[b"agent", mint]` account; a client that omits it falls back to the global key. That is deliberate (the account list is the caller's statement of which compliance regime applies), but worth a client-side check.
* **Refunds return to the funder, with no override.** `cancel_settlement` pays `refund_source` — the account that funded the escrow — and nothing else. A desk that needs to refund a *different* account (the taker's, say) has to do that as a separate transfer after the cancellation.
* **The deadline is a program constant.** `EXPIRY_WINDOW_SLOTS` (216 000) is fixed for every trade rather than chosen per trade, so the AMM/hooks CPIs did not need a new argument. A desk that needs a different window must upgrade the program. `refund_expired` is permissionless, so a keeper can crank it; the destination is pinned to `refund_source`.
* **One amount per trade.** The escrowed amount is explicit and measured, but a trade cannot be partially escrowed, partially settled, or split between venue and desk — one vault per trade, released whole.
* **No events.** State transitions are `msg!`-logged only; indexers should watch for vault PDA creation and status changes rather than waiting for Anchor events.
* **Rent reclamation is manual.** Settling/cancelling does not automatically close the vault account or escrow token account in the same transaction (preserving the terminal record on-chain for audit inspection). The config authority reclaims both the vault PDA and escrow token account rent by calling `close_settlement` once the trade is terminal (`Settled`, `Cancelled`, or `Expired`).
