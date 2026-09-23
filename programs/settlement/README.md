# T+N Settlement Escrow Program

An Anchor-based Solana program that acts as the **clearinghouse for RWA trades**: the output of a swap is locked in a per-trade escrow PDA and cannot leave until the settlement slot has passed **and** the off-chain Transfer Agent has produced a valid secp256k1 signature authorizing the release — a T+1/T+2-style clearing cycle for tokenized real-world assets.

Program ID: `49ppDVbrZSqwLDj8sitSapEsMMRfC2Z7wCeMAwhPVQoY`

---

## 🌟 Overview

Real-world assets do not settle at the swap: an institution that buys a tokenized T-bill receives the tokens only after the Transfer Agent — the regulated party that performs compliance checks on the trade — has signed off. This program implements that flow on-chain:

1. **A trade executes on the AMM.** Instead of paying the buyer directly, the AMM's Hooks program routes the output tokens into this program (`initiate_settlement`), which sweeps them into a per-trade **SettlementVault PDA** and records `(trade_id, taker, mint, amount, settlement_slot)` with status `Pending`.
2. **T+N elapses.** The vault carries an absolute `settlement_slot`. Nothing can leave before `Clock::slot >= settlement_slot` — the on-chain analogue of a T+1/T+2 clearing window.
3. **The Transfer Agent signs off-chain.** They sign `keccak256(trade_id || taker || mint || amount)` with a secp256k1 key — exactly the fields recorded in the vault, so the signature authorizes this trade and nothing else.
4. **Anyone submits `settle`.** The program recovers the signer's public key from the signature and compares it against the key stored in the global `SettlementConfig` PDA. On a match, the escrow pays out to the taker and the vault is marked `Settled`.
5. **Or the authority cancels.** If the Transfer Agent refuses (compliance failure, KYC break), the config authority can `cancel_settlement` and return the funds to a refund account.

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
* `amount` is the vault's recorded amount, which is the **entire balance of the source account** at initiation time. The Transfer Agent must read the vault account (or the initiating trade) to know it before signing.

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
| `SettlementVault::LEN` | `98` bytes | 8 discriminator + 8 + 32 + 32 + 8 + 8 + 1 + 1. |
| `SettlementConfig::LEN` | `105` bytes | 8 discriminator + 32 + 64 + 1. |

Rent: creating one vault PDA costs ≈ 0.0016 SOL rent-exempt (98-byte account); the escrow token account ≈ 0.002 SOL. In the hooks flow both are charged to the hook authority PDA (see the Hooks program README).

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
            │                                     │  owns vault_token_account   │
            │        settle(TA sig)  ──▶ taker_token_account (recorded taker)
            │        cancel(authority) ─▶ refund_token_account
            ▼
   status: Pending ──▶ Settled / Cancelled  (terminal states)
```

### PDA Seeds Table

| Account | PDA Seeds Format | Description |
| :--- | :--- | :--- |
| **SettlementConfig** | `[b"config"]` | Global settings: the cancel authority and the authorized Transfer Agent's secp256k1 key. One per program. |
| **SettlementVault** | `[b"settlement", trade_id.to_le_bytes()]` | Per-trade escrow. `trade_id` is little-endian `u64`. Owns the escrow token account. |

### Account layout

`SettlementConfig` (105 bytes including the 8-byte discriminator):

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `authority` | `Pubkey` | May call `set_transfer_agent` and `cancel_settlement`. Assign a multisig. |
| `transfer_agent_secp256k1` | `[u8; 64]` | Uncompressed secp256k1 key of the Transfer Agent; zeroed until `set_transfer_agent` runs. |
| `bump` | `u8` | Config PDA bump. |

`SettlementVault` (98 bytes including the discriminator):

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `trade_id` | `u64` | Caller-chosen trade id; also the PDA seed. |
| `taker` | `Pubkey` | Recorded recipient; bound into the Transfer Agent's signed message and enforced at payout. |
| `token_mint` | `Pubkey` | Mint of the escrowed token; all downstream token accounts are checked against it. |
| `amount` | `u64` | Escrowed amount — the **full balance** of the source account at initiation. |
| `settlement_slot` | `u64` | Absolute unlock slot (T+N). |
| `status` | `SettlementStatus` | `Pending` / `Settled` / `Cancelled`; the last two are terminal. |
| `bump` | `u8` | Vault PDA bump, reused as the CPI signer seed on payout. |

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
pub fn initiate_settlement(ctx: Context<InitiateSettlement>, trade_id: u64, settlement_slot: u64) -> Result<()>
```

Creates the vault PDA for `trade_id` and sweeps the source account into escrow.

* **Parameters**:
  * `trade_id: u64`: Caller-chosen, must be unused — the PDA `init` fails if the trade id was ever escrowed before, even after settling or cancelling.
  * `settlement_slot: u64`: Absolute unlock slot, strictly in the future (`InvalidSettlementSlot`).
* **Flow**: record `(trade_id, taker, mint, amount, settlement_slot, Pending)` → verify the source is non-empty (`EmptySettlement`) → transfer the **entire source balance** into `vault_token_account`, signed by `authority` as an ordinary signer (in the hooks flow that signature arrives via the hooks program's PDA seeds — no program seeds are needed here).
* **Validations**: `source_token_account.owner == authority.key()` (`SourceNotOwnedByAuthority`); `vault_token_account` mint == `token_mint` and owner == the vault PDA (`MintMismatch` / `InvalidVaultTokenAccount`).
* **Accounts**: `settlement_vault` (init), `source_token_account` (mut), `vault_token_account` (mut), `token_mint`, `taker` (unchecked `AccountInfo` — recorded and later bound into the signed message), `authority` (signer, mut — also the rent payer), `token_program` (Interface), `system_program`.
* **Note**: the escrow token account must **already exist** — create the idempotent ATA of the vault PDA (`allowOwnerOffCurve: true`) in a prior instruction.

### 4. `settle`

```rust
pub fn settle(ctx: Context<Settle>, signature: [u8; 64], recovery_id: u8) -> Result<()>
```

Releases the escrow. **No signer is required** — the recovered signature is the authorization.

* **Parameters**: `signature: [u8; 64]` (compact `r || s`), `recovery_id: u8` (0–3).
* **Flow**: status must be `Pending` (`AlreadySettled`) → slot check (`SettlementPeriodNotOver`) → rebuild the 104-byte preimage from the vault's own `trade_id` / `taker` / `token_mint` / `amount` → `keccak256` → `secp256k1_recover` → compare against the config's agent key (`SignatureVerificationFailed` / `UnauthorizedTransferAgent`) → transfer the full `amount` to the taker's account, signed by the vault PDA (`[b"settlement", trade_id_le, bump]`) → mark `Settled`.
* **Validations**: `vault_token_account` mint/owner checks as above; `taker_token_account` mint == the vault's mint and owner == the vault's recorded `taker` (`InvalidTakerTokenAccount`) — the payout address is therefore pinned to the taker the Transfer Agent signed for.
* **Accounts**: `settlement_vault` (mut), `settlement_config`, `vault_token_account` (mut), `taker_token_account` (mut), `token_program` (Interface).
* **Note**: there is no deadline — a trade may be settled at any point after T+N while it is still `Pending`.

### 5. `cancel_settlement`

```rust
pub fn cancel_settlement(ctx: Context<CancelSettlement>) -> Result<()>
```

Compliance exit: returns the full escrow to a refund account and marks the trade `Cancelled`.

* **Parameters**: none.
* **Validations**: signer is the config `authority` (`Unauthorized`); status is `Pending` (`NotPending`); `refund_token_account` mint == the vault's mint (`MintMismatch`).
* **Accounts**: `settlement_vault` (mut), `settlement_config`, `vault_token_account` (mut), `refund_token_account` (mut), `authority` (signer), `token_program` (Interface).
* **Note**: the refund destination is any account of the right mint — the authority effectively chooses where the money goes. With a multisig as the config authority this is a controlled operation; a single key here is a single point of failure.

---

## 📄 Data Structures

### `SettlementStatus`

```rust
pub enum SettlementStatus {
    Pending,    // Escrowed; waiting for T+N and the Transfer Agent
    Settled,    // Released to the taker — terminal
    Cancelled,  // Returned to the refund account — terminal
}
```

### `SettlementVault` Account

```rust
pub struct SettlementVault {
    pub trade_id: u64,          // PDA seed; caller-chosen, unique forever
    pub taker: Pubkey,          // Recorded recipient, bound into the TA signature
    pub token_mint: Pubkey,     // Escrowed mint
    pub amount: u64,            // Full source balance swept at initiation
    pub settlement_slot: u64,   // Absolute unlock slot (T+N)
    pub status: SettlementStatus,
    pub bump: u8,               // Vault PDA bump (CPI signer seed on payout)
}
```

### `SettlementConfig` Account

```rust
pub struct SettlementConfig {
    pub authority: Pubkey,                      // Cancel + key-rotation authority (multisig)
    pub transfer_agent_secp256k1: [u8; 64],     // Uncompressed secp256k1 key (x || y)
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

/** Escrow token account: owned by a PDA, so allowOwnerOffCurve is required. */
const vaultTokenAta = (mint: PublicKey, vault: PublicKey) =>
  getAssociatedTokenAddressSync(mint, vault, true, TOKEN_2022_PROGRAM_ID);
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
  .initiateSettlement(tradeId, settlementSlot)
  .accounts({
    settlementVault: vault,
    sourceTokenAccount: ammOutputAta,        // must be owned by `authority`
    vaultTokenAccount: vaultTokenAta(mint, vault), // must already exist
    tokenMint: mint,
    taker: takerPubkey,                      // recorded, later bound into the TA message
    authority: ammAuthorityPubkey,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
    systemProgram: SystemProgram.programId,
  })
  .signers([ammAuthority]) // the hooks program supplies its PDA signature via CPI seeds instead
  .rpc();
```

### Settle with a real Transfer Agent signature

The repo's TS test mocks this step; production integrations sign exactly as the program verifies:

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
    vaultTokenAccount: vaultTokenAta(mint, vault),
    takerTokenAccount: getAssociatedTokenAddressSync(
      mint, vaultState.taker, false, TOKEN_2022_PROGRAM_ID
    ),
    tokenProgram: TOKEN_2022_PROGRAM_ID,
  })
  .rpc(); // permissionless — no signer needed
```

### Cancel a pending trade

```typescript
await program.methods
  .cancelSettlement()
  .accounts({
    settlementVault: vault,
    settlementConfig: configPda,
    vaultTokenAccount: vaultTokenAta(mint, vault),
    refundTokenAccount: refundAta, // any account of the escrowed mint
    authority: wallet.publicKey,   // config authority — a multisig in production
    tokenProgram: TOKEN_2022_PROGRAM_ID,
  })
  .rpc();
```

### Integration pitfalls

1. **The full source balance is escrowed.** `amount` is the entire balance of `source_token_account` at initiation, and `settle` always pays out exactly that amount. Point the instruction at a per-trade account (or an account holding exactly the trade's output), never at a shared vault holding unrelated funds.
2. **The escrow token account must pre-exist.** Neither `initiate_settlement` nor the hook creates it. Create the idempotent ATA of the vault PDA first — with `allowOwnerOffCurve: true`, since the owner is a PDA.
3. **`trade_id` is forever.** The PDA `init` makes every id single-use across the program's lifetime, including after `Settled`/`Cancelled`. Make your off-chain trade registry the source of uniqueness (a counter, or a hash of the trade ticket).
4. **Slots, not seconds.** Pass the *absolute* unlock slot to `initiate_settlement`; the Hooks program computes it from a relative `settlement_slots` for you. Slot pacing wobbles — do not promise wall-clock settlement times derived from slots.
5. **The signature binds `(trade_id, taker, mint, amount)` — nothing else.** It does not bind the settlement slot or any account addresses of the settlement program. A captured signature is valid only for its exact trade, but for *all time* on that trade (until consumed once, since status flips to `Settled`).
6. **keccak256 of the raw preimage.** Ethereum's `personal_sign` / `eth_sign` wrappers prepend a message prefix and will never verify. Hash exactly the 104 bytes of §1.
7. **Uncompressed key format.** `set_transfer_agent` wants 64 bytes `x || y`. Compressed (33 B) keys and Ethereum addresses (20 B) are the classic integration bug.
8. **Permissionless `settle` is a feature.** No wallet is required to submit — which means keepers can settle on schedule, and nobody can front-run the payout to a different address: the taker token account must be owned by the vault's recorded `taker`.
9. **One agent key globally.** The config holds a single secp256k1 key for *all* trades. Rotating it invalidates nothing already signed (a captured signature still verifies against the new key only if it was made by the *new* key — old signatures fail after rotation, which is usually the point).
10. **Token-2022 with transfer hooks.** Transfers go through the token interface, but transfer-hook extra accounts are *not* forwarded — mints with an active transfer hook will fail at the token program. (The `rwa_mint` program's whitelist hook must be satisfied by the participants, which it is in the whitelist-checked flow.)

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `InvalidSettlementSlot` (6000) | `Settlement slot must be in the future` | `settlement_slot <= Clock::slot` at initiation. |
| `SettlementPeriodNotOver` (6001) | `Settlement period has not yet ended` | `Clock::slot < vault.settlement_slot` at settle time. |
| `AlreadySettled` (6002) | `Trade is already settled or cancelled` | `settle` on a vault not in `Pending`. |
| `NotPending` (6003) | `Trade is not pending` | `cancel_settlement` on a vault not in `Pending`. |
| `SignatureVerificationFailed` (6004) | `Transfer Agent signature verification failed` | `secp256k1_recover` rejected the `(signature, recovery_id)`. |
| `UnauthorizedTransferAgent` (6005) | `Recovered key does not match the authorized Transfer Agent` | Signature is valid but was made by a different secp256k1 key. |
| `InvalidTransferAgentKey` (6006) | `The configured Transfer Agent key cannot be empty` | `set_transfer_agent` given the all-zero key. |
| `SourceNotOwnedByAuthority` (6007) | `The source token account is not owned by the signing authority` | The swept account belongs to someone other than the `authority` signer. |
| `MintMismatch` (6008) | `Token account mint does not match the settlement mint` | A token account's mint disagrees with `token_mint` / the vault's mint. |
| `InvalidVaultTokenAccount` (6009) | `Token account is not owned by the settlement vault PDA` | The escrow token account is not owned by the vault PDA. |
| `InvalidTakerTokenAccount` (6010) | `The taker token account does not belong to the vault's taker` | Payout destination not owned by the recorded `taker`. |
| `EmptySettlement` (6011) | `Nothing to settle: the source account is empty` | Source balance is zero at initiation. |
| `Unauthorized` (6012) | `Only the config authority may perform this action` | `set_transfer_agent` / `cancel_settlement` signer is not the config authority. |
| `UnauthorizedPool` (6013) | `Caller is not a pool authorized to route trades into settlement` | `initiate_settlement` signed by a pool that is not the one wired into this config. |
| `GateRequired` (6014) | `This settlement config requires initiation by a wired AMM pool` | `initiate_settlement` sent directly, without the wired pool CPI-ing it. |
| `UnauthorizedInitializer` (6015) | `Only the program's upgrade authority may initialize the settlement config` | `initialize` signed by a key that is not the program's upgrade authority — including the case where no authority is recorded at all. |

---

## 🧪 Testing & Verification

The TS integration tests in `tests/ts/rwa-dex.ts` (run by `anchor test` against localnet, with the Token-2022 program loaded from the fixtures pinned in `Anchor.toml`) cover:

* **Escrow creation.** A mock AMM vault holding 500 000 000 units of a Token-2022 RWA mint is swept into the `[b"settlement", 1]` vault PDA with `taker = Institution B` and `T+100` slots, recording status `Pending`.
* **Time-lock enforcement.** `settle` before the unlock slot reverts with `SettlementPeriodNotOver`, leaving the vault untouched.
* **Clock advance.** The validator clock is pushed forward ~150 slots and the vault becomes settleable.

The final signature-verified payout is stubbed in TS (the test documents that a real `@noble/secp256k1` signature over the §1 preimage would be supplied); the recover-and-compare path itself is implemented on-chain and enforced by `SignatureVerificationFailed` / `UnauthorizedTransferAgent`. There is no `tests/e2e` Rust suite for this program yet — wiring one against `target/deploy/settlement.so` (as the clmm/oracle/kyc_gate suites do with LiteSVM) is the natural next step.

```bash
anchor build -p settlement     # SBF build
anchor test                    # TS suite incl. the settlement flow above
```

### Known simplifications (vs. production clearinghouses)

* **No malleability normalization.** `settle` does not enforce `s < n/2`; a signature and its `(s' = n − s, recovery_id')` twin are both valid. Harmless here (the signer identity is fixed by the stored key, and each trade settles once), but worth normalizing if signatures are ever logged as audit artifacts.
* **One global agent key.** A single secp256k1 key authorizes every trade, for every mint. Production would key per-mint or per-asset-class, or take the key from per-trade compliance records.
* **No refund-destination constraint.** `cancel_settlement` checks only the refund account's mint; the authority chooses the destination. Combine with a multisig authority.
* **No deadline on `Pending`.** A trade can sit settleable indefinitely after T+N, and cancellable until settled. Production desks usually want a cutoff after which the escrow auto-refunds.
* **Full-balance escrow only.** No partial amounts, no partial settlements, no fee split between venue and desk — one vault per trade, swept and released whole.
* **No events.** State transitions are `msg!`-logged only; indexers should watch for vault PDA creation and status changes rather than waiting for Anchor events.
* **Rent is stranded.** Settling/cancelling does not close the vault account; the ~0.0016 SOL rent stays with the `Settled`/`Cancelled` record (useful as an audit trail, not free).
