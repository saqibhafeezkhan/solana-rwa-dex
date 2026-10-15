# RWA Transfer Hook Program

An Anchor-based implementation of the **SPL Transfer Hook Interface** for **SPL Token-2022 (Token Extensions)**.

This program enforces institutional compliance and regulatory whitelisting for Real World Asset (RWA) tokens. On every token transfer it cryptographically validates that **both** the sending and the receiving wallet belong to an authorized institution, each proved with a **Merkle Tree Proof** verified against an on-chain root. Gating both sides is the difference between "only whitelisted institutions may hold the token" and "a delisted holder may still sell out": a recipient-only check cannot stop an outflow.

Each institution's proof lives in its own `WhitelistEntry` PDA (`[b"whitelist", owner]`). Token-2022 derives those two addresses for every transfer from the token accounts' `owner` fields and hands them to `execute` as accounts, because it gives the hook no way to receive instruction data of its own — see [How the Proof Reaches the Hook](#how-the-proof-reaches-the-hook).

---

## Table of Contents

- [RWA Transfer Hook Program](#rwa-transfer-hook-program)
  - [Table of Contents](#table-of-contents)
  - [Overview](#overview)
    - [Why Merkle Trees?](#why-merkle-trees)
  - [Merkle Proof Maths](#merkle-proof-maths)
    - [Worked example: four institutions](#worked-example-four-institutions)
    - [Proof and transaction sizes](#proof-and-transaction-sizes)
  - [Architecture \& Workflow](#architecture--workflow)
  - [Accounts \& PDAs](#accounts--pdas)
    - [1. `Config` Account](#1-config-account)
    - [2. `ExtraAccountMetaList` Account](#2-extraaccountmetalist-account)
    - [3. `WhitelistEntry` Account](#3-whitelistentry-account)
  - [Program Instructions](#program-instructions)
    - [1. `initialize`](#1-initialize)
    - [2. `update_root`](#2-update_root)
    - [3. `set_paused`](#3-set_paused)
    - [4. `set_authority`](#4-set_authority)
    - [5. `initialize_extra_account_meta_list`](#5-initialize_extra_account_meta_list)
    - [6. `set_whitelist_entry`](#6-set_whitelist_entry)
    - [7. `close_whitelist_entry`](#7-close_whitelist_entry)
    - [8. `execute`](#8-execute)
  - [Client Usage Guide](#client-usage-guide)
    - [Prerequisites \& Dependencies](#prerequisites--dependencies)
    - [Step 1: Initialize Hook Config](#step-1-initialize-hook-config)
    - [Step 2: Generate Merkle Tree \& Proofs](#step-2-generate-merkle-tree--proofs)
    - [Step 3: Update Merkle Root](#step-3-update-merkle-root)
    - [Step 4: Initialize ExtraAccountMetaList for Mint](#step-4-initialize-extraaccountmetalist-for-mint)
    - [Step 5: Register a Whitelist Entry per Institution](#step-5-register-a-whitelist-entry-per-institution)
    - [Step 6: Execute a Compliant Token Transfer](#step-6-execute-a-compliant-token-transfer)
    - [Step 7: Verify a Proof Off-Chain](#step-7-verify-a-proof-off-chain)
    - [How the Proof Reaches the Hook](#how-the-proof-reaches-the-hook)
  - [Error Codes](#error-codes)
  - [Known Gaps \& Hardening](#known-gaps--hardening)
  - [Security \& Architecture Considerations](#security--architecture-considerations)

---

## Overview

In institutional Real World Asset (RWA) tokenization, regulatory frameworks (such as SEC rules, MiCA, and KYC/AML guidelines) require that assets only be held and transferred between verified and whitelisted parties.

### Why Merkle Trees?

- **One commitment, not a list**: the whitelist is a single **32-byte root** in the hook config, so a rotation is one `update_root` write and the hook never has to iterate a list of institutions.
- **Scalability**: millions of addresses are supported at logarithmic verification cost ($O(\log N)$ Keccak-256 hashes on-chain).
- **Auditability**: membership is proved from public inputs (a leaf and the sibling path) rather than asserted by a lookup. An auditor can rebuild the tree off-chain and check that the published root commits to exactly the institutions they expect.

What that costs is one small account per institution: the path has to be *stored* on chain to reach `execute` (see [How the Proof Reaches the Hook](#how-the-proof-reaches-the-hook)), and each `WhitelistEntry` is 681 bytes — about 0.0048 SOL of rent-exempt deposit, refunded by `close_whitelist_entry` when an institution is delisted.

---

## 🧮 Merkle Proof Maths

A whitelist of $N$ institutional wallets is folded into one 32-byte root with Keccak-256. Leaves and inner nodes are **domain separated** (RFC 6962 style), so the two can never be confused for one another:

$$\text{leaf}_i = \text{keccak256}\big(0\text{x}00 \;\|\; \text{wallet}_i\big) \qquad\qquad \text{parent}(a, b) = \text{keccak256}\big(0\text{x}01 \;\|\; \min(a,b) \;\|\; \max(a,b)\big)$$

Without the prefixes a leaf and an inner node are hashes of the same shape, so an inner node could be presented as a leaf and membership proved for a wallet that was never whitelisted. With them, `keccak256(0x00 ‖ …)` and `keccak256(0x01 ‖ …)` are disjoint preimages by construction. The prefixes are `LEAF_PREFIX` / `NODE_PREFIX` in `lib.rs`, and the client tree builder has to use the same two.

Sorting each pair before hashing is what makes the parent independent of left/right position, so a proof is a plain list of $\lceil \log_2 N \rceil$ sibling hashes with no direction bits (this is exactly the `hashes.sort_unstable()` in `execute`):

$$\text{root}' = \text{fold}\big(\text{leaf}(\text{owner}), \text{proof}\big) \qquad\qquad \text{root}' \stackrel{?}{=} \texttt{config.merkle\_root}$$

The order of the sibling hashes matters - they are folded in the order supplied, each sorted with the running hash - but the tree only has to be built with the same sorted-pair rule on both sides.

### Worked example: four institutions

```text
leafA = keccak(0x00 ‖ A)   leafB = leaf(B)   leafC = leaf(C)   leafD = leaf(D)
h01   = parent(leafA, leafB) = keccak(0x01 ‖ min ‖ max)
h23   = parent(leafC, leafD)
root  = parent(h01, h23)
```

To approve a transfer to institution **C**, the client sends `proof = [leafD, h01]` and the hook computes:

```text
step 1: current = keccak(0x00 ‖ C)     = leafC
step 2: current = parent(leafC, leafD) = h23
step 3: current = parent(h23, h01)     = root   ✅
```

Any other wallet, or any dropped/extra hash, lands somewhere else and the transfer is rejected with `DestinationNotWhitelisted`.

### Proof and transaction sizes

| Whitelisted institutions | Proof levels $\lceil \log_2 N \rceil$ | Proof bytes ($32 \times$ levels) |
| ---: | ---: | ---: |
| 10 | 4 | 128 |
| 1 000 | 10 | 320 |
| 1 000 000 | 20 | 640 |
| 4 × 10⁹ | 32 | 1 024 |

Verification costs one Keccak syscall per level, $O(\log N)$. A proof no longer rides in the transaction's *instruction data* (Token-2022 gives the hook no way to receive it there), so the packet ceiling is not the constraint: the proof sits in the institution's `WhitelistEntry`, and `MAX_PROOF_DEPTH` caps a stored path at 20 levels. A tree deeper than that has to be sharded into one tree per jurisdiction, each registered as its own whitelist.

---

## Architecture & Workflow

```
                             +------------------------+
                             | Authority / Multisig   |
                             +------------------------+
                                         |
                                (1) update_root(new_root)
                                         v
                             +------------------------+
                             |       Config PDA       |
                             |   [b"config"]          |
                             | - authority            |
                             | - merkle_root (32B)    |
                             | - paused (bool)        |
                             +------------------------+
                                         ^
                                         | Read during validation
                                         |
+--------------------+   Token-2022      |      +------------------------+
| Source Institution | -------------> CPI ----> |     Transfer Hook      |
|  Token Account     | transfer_checked  |      |       `execute`        |
+--------------------+                   |      +------------------------+
                                         |           |
                                         |           | 1. Refuse if paused
                                         |           | 2. Extract source & destination owners
                                         |           | 3. leaf = keccak(0x00 ‖ owner)
                                         |           | 4. Fold both proofs, compare to root
                                         |           v
                                         |       [ Transfer Approved / Rejected ]
                                         v
                             +------------------------+
                             | Destination Account    |
                             | (Must be whitelisted)  |
                             +------------------------+

                         The *source* account is checked too: a
                         transfer out of a delisted wallet fails here.
```

1. **Token-2022 Transfer Hook Extension**: When an RWA mint is created, it is configured with the `TransferHook` extension pointing to this program ID.
2. **Account Resolution**: When a client builds a transfer instruction, `@solana/spl-token` fetches the `ExtraAccountMetaList` PDA for the mint. This account defines additional accounts that Token-2022 must forward to the hook (specifically, the `Config` PDA, read-only).
3. **Execution & Verification**: Token-2022 invokes `execute` via Cross-Program Invocation (CPI). The hook refuses a paused whitelist, reads **both** owners out of the token accounts Token-2022 handed it, hashes each into a domain-separated leaf, folds each of the two supplied Merkle proofs into a candidate root, and rejects the transaction unless both match `config.merkle_root` — the source first (`SourceNotWhitelisted`), then the destination (`DestinationNotWhitelisted`). The owners always come from the token accounts, never from a caller-supplied key, so a signature proves what the token program itself recorded.
4. **Lifecycle**: the authority publishes roots with `update_root`, takes transfers out of service with `set_paused`, and rotates itself with `set_authority`.

---

## Accounts & PDAs

### 1. `Config` Account
Holds protocol-wide authority, the current Merkle root of whitelisted institutional addresses, and the halt flag.

- **Seeds**: `[b"config"]`
- **Fields**:
  - `authority: Pubkey` (32 bytes) – The admin or multisig that can update the root, rotate itself, and pause.
  - `merkle_root: [u8; 32]` (32 bytes) – Current Keccak-256 Merkle root. All zeros means no whitelist has been published yet, and it is not a state `update_root` will write.
  - `bump: u8` (1 byte) – Canonical bump seed.
  - `paused: bool` (1 byte) – While true, `execute` refuses every transfer.
- **Size**: `8 + 32 + 32 + 1 + 1 = 74 bytes`

Lifecycle: `initialize` creates it with an empty root and `paused = false`; `update_root` publishes a root; `set_paused` takes transfers out of service without touching the root, which is how the whitelist is "cleared" - a zero root would be indistinguishable from "never initialized", so it is rejected instead.

### 2. `ExtraAccountMetaList` Account
SPL TLV-encoded account required by the Token-2022 standard. For each Token-2022 mint using this hook, this PDA specifies which additional accounts must be resolved and passed to `execute`.

- **Seeds**: `[b"extra-account-metas", mint.key().as_ref()]`
- **Program Owner**: `transfer_hook` program.
- **Configured Extra Accounts** (resolved by Token-2022 on every transfer, in this order):
  1. `WhitelistEntry` (`[b"whitelist", <index 0>.owner]`, writable: `false`, signer: `false`) — the sender's entry. Index 0 is the transfer's source token account; the owner is read from its data at offset 32 (`Seed::AccountData`).
  2. `WhitelistEntry` (`[b"whitelist", <index 2>.owner]`, writable: `false`, signer: `false`) — the recipient's entry, from the destination token account.
  3. `Config` PDA (`[b"config"]`, writable: `false`, signer: `false`) - read-only, matching how `execute` takes the account.

---

### 3. `WhitelistEntry` Account
One institution's membership: the Merkle path from its leaf up to the published root, stored where Token-2022 can hand it to `execute`.

- **Seeds**: `[b"whitelist", owner.key().as_ref()]`
- **Program Owner**: `transfer_hook` program.
- **Size**: 681 bytes (8 discriminator + 32 `owner` + 1 `depth` + 32 × 20 proof slots).
- **Fields**: `owner` (the wallet this entry admits — `execute` requires it to equal the token account's owner), `depth` (how many slots hold a path; `0` for a single-member tree), `proof` (siblings leaf-adjacent first, zero-padded past `depth`).
- **Rent**: ~0.0048 SOL, paid by the authority on `set_whitelist_entry` and refunded on `close_whitelist_entry`.
- **Zero-copy**: declared `#[account(zero_copy)] #[repr(C, packed)]` and read through `AccountLoader`, because `execute` holds two entries at once and a by-value deserialization of the 673-byte structs overflows the 4 KiB SBF stack frame (`Account<WhitelistEntry>` measured a 4 160-byte `execute` frame and refused to build). Packing keeps the pod layout byte-identical to the borsh layout, and Anchor writes the discriminator at instruction exit — so `set_whitelist_entry` writes it itself before `load_mut`, since `init_if_needed` leaves a new account zeroed.

---

## Program Instructions

### 1. `initialize`
Initializes the global `Config` account: sets the authority, leaves the root empty (`[0u8; 32]`, i.e. *no whitelist yet*) and `paused = false`.

Restricted to the program's **upgrade authority**. `initialize` decides who the whitelist authority is, so a permissionless `initialize` would let the first caller — anyone watching the program id — own the compliance gate before its deployer did. The handler therefore reads the canonical `ProgramData` account (address-pinned to `[program]` under the BPF upgradeable loader, owner-pinned to the loader) and requires the signer to be the authority recorded there (`UnauthorizedInitializer`). Once initialized, the recorded authority rotates itself with `set_authority`.

```rust
pub fn initialize(ctx: Context<Initialize>) -> Result<()>
```

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | PDA initialized with seeds `[b"config"]`. |
| `authority` | `Signer<'info>` | Payer and admin authority for the hook. Must be the program's upgrade authority. |
| `program_data` | `UncheckedAccount<'info>` | This program's `ProgramData` PDA (`[program]` under the BPF upgradeable loader), where the upgrade authority is read from. |
| `system_program` | `Program<'info, System>` | Solana System Program. |

On a real cluster the upgrade authority is set by the deploying upgrade transaction; with `anchor test`/LiteSVM there is none until one is written, and `initialize` is refused until then — which is the point.

---

### 2. `update_root`
Updates the 32-byte Merkle root representing the set of whitelisted institutions.

```rust
pub fn update_root(ctx: Context<UpdateRoot>, new_root: [u8; 32]) -> Result<()>
```

**Parameters:**
- `new_root`: `[u8; 32]` – The new Keccak-256 root of whitelisted institutional wallet addresses. A root of all zeros is rejected with `InvalidRoot`, because it doubles as the "no whitelist yet" value `execute` checks; use `set_paused` to stop transfers instead.

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | Writable `Config` PDA (`[b"config"]`). |
| `authority` | `Signer<'info>` | Must match `config.authority`. |

---

### 3. `set_paused`
Halts or resumes every transfer. Authority-only, and the supported way to take the whitelist out of service: clear the tree off-chain, pause, publish the new root, resume.

```rust
pub fn set_paused(ctx: Context<SetPaused>, paused: bool) -> Result<()>
```

**Parameters:**
- `paused`: `bool` – `true` to refuse all transfers (`WhitelistPaused`), `false` to serve the stored root again.

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | Writable `Config` PDA (`[b"config"]`). |
| `authority` | `Signer<'info>` | Must match `config.authority`. |

---

### 4. `set_authority`
Rotates the whitelist authority, for when the institutional multisig (or its vault) changes. Authority-only, and the new key cannot be the default pubkey.

```rust
pub fn set_authority(ctx: Context<SetAuthority>, new_authority: Pubkey) -> Result<()>
```

**Parameters:**
- `new_authority`: `Pubkey` – The key that will be able to call `update_root`, `set_paused` and `set_authority`. Taken as a plain key rather than a signer so that it can be a multisig vault PDA, which cannot sign a transaction directly - verify the address off-chain before sending, since a typo hands the whitelist to a key nobody controls (the default pubkey is rejected for this reason).

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | Writable `Config` PDA (`[b"config"]`). |
| `authority` | `Signer<'info>` | Must match `config.authority`. |

---

### 5. `initialize_extra_account_meta_list`
Creates and initializes the `ExtraAccountMetaList` PDA for a specific Token-2022 mint. This is what tells Token-2022 which accounts `execute` needs on every transfer: the sender's and the recipient's `WhitelistEntry` PDAs — derived from the two token accounts' `owner` fields (`Seed::AccountData`) — and the `Config` PDA for the pause flag and the root.

The mint must already carry a `TransferHook` extension pointing at **this** program; otherwise the instruction fails with `MintNotConfigured`. That turns a wrong program id - or a list created before the mint was configured - into an error at setup time instead of a failed transfer later.

```rust
pub fn initialize_extra_account_meta_list(
    ctx: Context<InitializeExtraAccountMetaList>,
) -> Result<()>
```

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `payer` | `Signer<'info>` | Pays rent for the `ExtraAccountMetaList` account. |
| `extra_account_meta_list` | `UncheckedAccount<'info>` | PDA with seeds `[b"extra-account-metas", mint.key().as_ref()]`. |
| `mint` | `InterfaceAccount<'info, Mint>` | The Token-2022 mint account. |
| `system_program` | `Program<'info, System>` | Solana System Program. |

---

### 6. `set_whitelist_entry`
Creates or refreshes an institution's `WhitelistEntry` at `[b"whitelist", owner]`, which is where the hook reads that institution's Merkle path. Authority-only, and the supplied path is folded against the **live** root before it is stored (`ProofNotInWhitelist`), so a mistyped or already-rotated proof can never become an entry.

The institution does not sign: the authority vouches for it, which is what lets an institution be onboarded from a cold vault or a custodian's account.

The owner does not have to be a wallet. A program-owned custody account works the same way, **provided the address is stable**: the hook resolves the entry from a token account's `owner` field on both sides of every transfer, so anything that holds or sends this mint needs an entry written against the live root. A fixed PDA (an AMM custody account, the hooks program's `hook_authority`) is registered once. A *per-trade* PDA is not — a settlement escrow owned by a per-trade vault PDA needs a fresh entry (and therefore a rotated root, which every other member's entry has to be refreshed for) on every trade; see the "escrow's whitelist entry is per trade" note in `programs/settlement/README.md` and proposal P16 in `docs/proposed-features.md`.

```rust
pub fn set_whitelist_entry(ctx: Context<SetWhitelistEntry>, proof: Vec<[u8; 32]>) -> Result<()>
```

**Parameters:**
- `proof`: `Vec<[u8; 32]>` – The wallet's sibling hashes in fold order, leaf-adjacent first. An empty vec is valid and is what a single-member tree needs (the leaf is then the root). More than `MAX_PROOF_DEPTH` (20) nodes is refused with `ProofTooLong`.

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | `Config` PDA (`[b"config"]`) — read for the authority and the root. |
| `authority` | `Signer<'info>` | Must match `config.authority`; also pays the entry's rent. |
| `owner` | `UncheckedAccount<'info>` | The institution's wallet. Used as the PDA seed and as the hashed leaf; never read. |
| `whitelist_entry` | `Account<'info, WhitelistEntry>` | `init_if_needed`: created on first registration, overwritten on a refresh. |
| `system_program` | `Program<'info, System>` | Solana System Program. |

> **Rotation is a write, not a rebuild.** A new root leaves every entry where it is, and each one whose path no longer folds to the new root stops verifying — so access is revoked the moment the root changes. Re-granting it means calling this again with the path from the new tree, once per institution.

---

### 7. `close_whitelist_entry`
Delists an institution and refunds the entry's rent to the authority. The entry is the only on-chain trace of the membership, so this is the instruction that removes a `Pending`/existing entry without waiting for a root rotation.

```rust
pub fn close_whitelist_entry(ctx: Context<CloseWhitelistEntry>) -> Result<()>
```

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | `Config` PDA (`[b"config"]`). |
| `authority` | `Signer<'info>` | Must match `config.authority`; receives the refund. |
| `owner` | `UncheckedAccount<'info>` | The institution's wallet. Seed source only. |
| `whitelist_entry` | `Account<'info, WhitelistEntry>` | Closed (`close = authority`). |

---

### 8. `execute`
The core validation hook. Automatically called by Token-2022 upon every `transfer_checked` instruction.

```rust
#[instruction(discriminator = ExecuteInstruction::SPL_DISCRIMINATOR_SLICE)]
pub fn execute<'info>(ctx: Context<Execute<'info>>, _amount: u64) -> Result<()>
```

**Dispatch.** Token-2022 calls the hook with the 8-byte tag `sha256("spl-transfer-hook-interface:execute")[..8]` (`0x692565c54bfb661a`), not with the `global:execute` sighash Anchor derives (`0x82ddf29a0dc1bd1d`), so the handler overrides its discriminator. Anchor ≤ 0.30 spelled this `#[interface(spl_transfer_hook_interface::execute)]`; Anchor 1.x replaced that attribute with the `#[instruction(discriminator = ...)]` override above, which sets the `DISCRIMINATOR` constant that the generated dispatch and CPI helpers both use. Without it the tag would fall through to `InstructionFallbackNotFound` and no real transfer could ever pass.

The only argument is the amount: Token-2022 writes the hook's data itself, as the tag followed by `amount` (`u64`, little-endian), and forwards nothing else from the transfer. The two proofs arrive as the `source_entry` / `destination_entry` accounts instead.

**Checks, in order:** the whitelist is not paused (`WhitelistPaused`), a root has been published (`WhitelistNotInitialized`), each entry is the one for the wallet it is proving for (`InvalidWhitelistEntry` — `owner` inside the entry must equal the token account's owner, so a direct caller cannot pair an entry with someone else's wallet), then each owner's leaf is folded with the path stored in *its own* entry and compared with `config.merkle_root` — the source first (`SourceNotWhitelisted`), then the destination (`DestinationNotWhitelisted`). Both owners are read from the token accounts (`source_token.owner` is constrained to the `owner` signer), so neither can be supplied by the caller.

**Tests.** `cargo test -p transfer_hook` calls the program entrypoint with the real tag and hand-built `AccountInfo`s (19 tests): the tag against its own hash, the extra-account metas resolving to each side's entry PDA and the config, a transfer with both sides approved, a source that is not whitelisted even though the destination is, a destination that is not whitelisted even though the source is, an entry paired with the wrong wallet, an uninitialized root, a paused whitelist, a proof built the old un-prefixed way, a tree with a duplicated node, the old Anchor tag no longer dispatching, the zero-root rule, pausing, authority rotation, the write-time proof gate (`set_whitelist_entry` accepted, refused for a path the root does not accept, refused for a path past `MAX_PROOF_DEPTH`, refused for a non-authority, refused with no published root), and the mint check on `initialize_extra_account_meta_list`. The full path — entries resolved by Token-2022 from a real `transfer_checked`, the fold inside `execute`, and the token movement — is covered by `tests/ts/rwa-dex.ts` and by the localnet bootstrap's RWA self-check.

**Parameters:**
- `_amount`: `u64` – Transfer amount (passed by Token-2022 standard).

**Accounts (in strict order expected by Token-2022):**
1. `source_token`: `InterfaceAccount<'info, TokenAccount>` – Source token account.
2. `mint`: `InterfaceAccount<'info, Mint>` – Token-2022 mint.
3. `destination_token`: `InterfaceAccount<'info, TokenAccount>` – Destination token account.
4. `owner`: `UncheckedAccount<'info>` – Owner/authority of the source token account.
5. `extra_account_meta_list`: `UncheckedAccount<'info>` – PDA for extra account metadata.
6. `source_entry`: `Account<'info, WhitelistEntry>` – The sender's entry, resolved from `source_token.owner`.
7. `destination_entry`: `Account<'info, WhitelistEntry>` – The recipient's entry, resolved from `destination_token.owner`.
8. `config`: `Account<'info, Config>` – Hook configuration containing the Merkle root.

---

## Client Usage Guide

### Prerequisites & Dependencies

In your TypeScript project:

```bash
npm install @coral-xyz/anchor @solana/web3.js @solana/spl-token merkletreejs keccak256
```

---

### Step 1: Initialize Hook Config

Run once after deploying the hook program to establish the `authority` and `Config` PDA.

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import { TransferHook } from "./target/types/transfer_hook";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);

const program = anchor.workspace.TransferHook as Program<TransferHook>;

const [configPda] = PublicKey.findProgramAddressSync(
  [Buffer.from("config")],
  program.programId
);

async function initializeHook() {
  const tx = await program.methods
    .initialize()
    .accounts({
      config: configPda,
      authority: provider.wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("Hook initialized. Tx signature:", tx);
}
```

---

### Step 2: Generate Merkle Tree & Proofs

Institutions are hashed using **Keccak-256** with the two domain prefixes, and hashes at each level are sorted so that the parent does not depend on left/right position. A level holding an odd number of nodes hashes the last one with itself, and its proof repeats that same hash - that is what the on-chain fold expects.

```typescript
import { PublicKey } from "@solana/web3.js";
import keccak256 from "keccak256";

// Must match LEAF_PREFIX / NODE_PREFIX in programs/transfer_hook/src/lib.rs.
const LEAF_PREFIX = Buffer.from([0x00]);
const NODE_PREFIX = Buffer.from([0x01]);

const leaf = (wallet: PublicKey): Buffer =>
  keccak256(Buffer.concat([LEAF_PREFIX, wallet.toBuffer()]));

const parent = (a: Buffer, b: Buffer): Buffer =>
  keccak256(
    Buffer.compare(a, b) <= 0
      ? Buffer.concat([NODE_PREFIX, a, b])
      : Buffer.concat([NODE_PREFIX, b, a])
  );

/** Levels bottom-up; the root is the single hash of the last level. */
function buildTree(wallets: PublicKey[]): Buffer[][] {
  let level = wallets.map(leaf);
  const levels = [level];

  while (level.length > 1) {
    const next: Buffer[] = [];
    for (let i = 0; i < level.length; i += 2) {
      // An odd trailing node is hashed with itself.
      const sibling = i + 1 < level.length ? level[i + 1] : level[i];
      next.push(parent(level[i], sibling));
    }
    levels.push(next);
    level = next;
  }

  return levels;
}

/** Sibling hashes in fold order, each as `[u8; 32]` for the instruction. */
function getProofForWallet(levels: Buffer[][], wallet: PublicKey): number[][] {
  let index = levels[0].findIndex((h) => h.equals(leaf(wallet)));
  if (index < 0) throw new Error(`${wallet.toBase58()} is not in the whitelist`);

  const proof: Buffer[] = [];
  for (let depth = 0; depth < levels.length - 1; depth++) {
    const sibling = index ^ 1; // the other child of this parent
    proof.push(sibling < levels[depth].length ? levels[depth][sibling] : levels[depth][index]);
    index = Math.floor(index / 2);
  }

  return proof.map((p) => Array.from(p));
}

// Example
const whitelistedWallets: PublicKey[] = [
  new PublicKey("Institution111111111111111111111111111111111"),
  new PublicKey("Institution222222222222222222222222222222222"),
  new PublicKey("Institution333333333333333333333333333333333"),
];

const levels = buildTree(whitelistedWallets);
const root = Buffer.from(levels[levels.length - 1][0]); // 32 bytes for update_root
const proof = getProofForWallet(levels, whitelistedWallets[2]);
```

> **`merkletreejs` needs `hashLeaves: false`.** Its default flow hashes the leaves with the same function as the parents, which cannot express the prefixes. If you prefer the library, pass pre-hashed leaves (`whitelistedWallets.map(leaf)`) with `hashLeaves: false` and a node `hashFn` that prepends `NODE_PREFIX`, and check that its `sortPairs` ordering matches `parent` above. The explicit version above is easier to audit against `execute`.

---

### Step 3: Update Merkle Root

When institutions are onboarded or offboarded, the authority updates the root on-chain:

```typescript
async function updateWhitelistRoot(newRootBuffer: Buffer) {
  const rootArray = Array.from(newRootBuffer);

  const tx = await program.methods
    .updateRoot(rootArray)
    .accounts({
      config: configPda,
      authority: provider.wallet.publicKey,
    })
    .rpc();

  console.log("Merkle root updated. Tx signature:", tx);
}
```

---

### Step 4: Initialize ExtraAccountMetaList for Mint

When deploying an RWA mint with Token-2022, initialize the `ExtraAccountMetaList` PDA:

```typescript
import {
  ExtensionType,
  TOKEN_2022_PROGRAM_ID,
  createInitializeMintInstruction,
  createInitializeTransferHookInstruction,
  getMintLen,
} from "@solana/spl-token";

async function setupMintWithTransferHook(mintKeypair: anchor.web3.Keypair) {
  const extensions = [ExtensionType.TransferHook];
  const mintLen = getMintLen(extensions);
  const lamports = await provider.connection.getMinimumBalanceForRentExemption(mintLen);

  // 1. Create Mint Account with Transfer Hook Extension
  const createAccountIx = SystemProgram.createAccount({
    fromPubkey: provider.wallet.publicKey,
    newAccountPubkey: mintKeypair.publicKey,
    space: mintLen,
    lamports,
    programId: TOKEN_2022_PROGRAM_ID,
  });

  const initTransferHookIx = createInitializeTransferHookInstruction(
    mintKeypair.publicKey,
    provider.wallet.publicKey, // transfer hook authority
    program.programId,         // transfer hook program id
    TOKEN_2022_PROGRAM_ID
  );

  const initMintIx = createInitializeMintInstruction(
    mintKeypair.publicKey,
    6, // decimals
    provider.wallet.publicKey, // mint authority
    null, // freeze authority
    TOKEN_2022_PROGRAM_ID
  );

  const setupTx = new anchor.web3.Transaction().add(
    createAccountIx,
    initTransferHookIx,
    initMintIx
  );
  await provider.sendAndConfirm(setupTx, [mintKeypair]);

  // 2. Initialize ExtraAccountMetaList PDA for this Mint
  const [extraAccountMetaListPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("extra-account-metas"), mintKeypair.publicKey.toBuffer()],
    program.programId
  );

  const initMetaTx = await program.methods
    .initializeExtraAccountMetaList()
    .accounts({
      payer: provider.wallet.publicKey,
      extraAccountMetaList: extraAccountMetaListPda,
      mint: mintKeypair.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("ExtraAccountMetaList initialized:", initMetaTx);
}
```

---

### Step 5: Register a Whitelist Entry per Institution

Publishing the root does not make a wallet tradeable on its own: `execute` reads each side's path from that institution's own entry, so every member needs one. The program folds the path against the live root before storing it, so a path that is not in the current tree is rejected here (`ProofNotInWhitelist`) rather than at the next transfer.

```typescript
async function registerEntry(owner: PublicKey, proof: Buffer[]) {
  const [entry] = PublicKey.findProgramAddressSync(
    [Buffer.from("whitelist"), owner.toBuffer()],
    program.programId
  );

  const sig = await program.methods
    .setWhitelistEntry(proof.map((p) => Array.from(p))) // empty for a single-member tree
    .accounts({
      config: configPda,
      authority: provider.wallet.publicKey, // must match config.authority
      owner,
      whitelistEntry: entry,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log(`Whitelist entry for ${owner.toBase58()}:`, sig);
}

// Delisting: closes the entry and refunds its rent to the authority.
async function closeEntry(owner: PublicKey) {
  const [entry] = PublicKey.findProgramAddressSync(
    [Buffer.from("whitelist"), owner.toBuffer()],
    program.programId
  );

  await program.methods
    .closeWhitelistEntry()
    .accounts({
      config: configPda,
      authority: provider.wallet.publicKey,
      owner,
      whitelistEntry: entry,
    })
    .rpc();
}
```

> Running `registerEntry` again overwrites the stored path, which is how a rotation is applied: `updateWhitelistRoot`, then one `registerEntry` per member with the path from the new tree.

---

### Step 6: Execute a Compliant Token Transfer

When performing a transfer, the client uses `@solana/spl-token` helpers to build the `transferChecked` instruction; the helper resolves the hook's extra accounts itself, including the two entries for the wallets involved.

```typescript
import {
  createTransferCheckedWithTransferHookInstruction,
} from "@solana/spl-token";

async function transferRWA(
  mint: PublicKey,
  sourceTokenAccount: PublicKey,
  sourceOwnerWallet: PublicKey,
  destinationTokenAccount: PublicKey,
  destinationOwnerWallet: PublicKey,
  amount: bigint,
  decimals: number
) {
  // NOTE: no proofs are passed here. Token-2022 builds the hook's own data
  // (`[discriminator][amount]`) and forwards no trailing bytes, so the proofs
  // come from the two whitelist entries the helper resolves below. Both wallets
  // must already be registered (Step 5), or there is no account to resolve and
  // the transfer fails before the hook runs.

  const transferIx = await createTransferCheckedWithTransferHookInstruction(
    provider.connection,
    sourceTokenAccount,
    mint,
    destinationTokenAccount,
    provider.wallet.publicKey, // owner
    BigInt(amount),
    decimals,
    [], // multiSigners
    "confirmed",
    TOKEN_2022_PROGRAM_ID
  );

  const tx = new anchor.web3.Transaction().add(transferIx);
  const sig = await provider.sendAndConfirm(tx);

  console.log("Transfer successful. Signature:", sig);
}
```

---

### Step 7: Verify a Proof Off-Chain

Recompute the root from the proof exactly as `execute` does, before spending a transaction on it (reusing `leaf` / `parent` / `buildTree` / `getProofForWallet` from Step 2):

```typescript
function verify(wallet: PublicKey, proof: Buffer[], expectedRoot: Buffer): boolean {
  let current = leaf(wallet);
  for (const sibling of proof) current = parent(current, sibling);
  return current.equals(expectedRoot);
}

function assertWhitelisted(wallet: PublicKey) {
  const expectedRoot = Buffer.from(levels[levels.length - 1][0]);
  const proof = getProofForWallet(levels, wallet).map(Buffer.from);

  if (!verify(wallet, proof, expectedRoot)) {
    throw new Error(`${wallet.toBase58()} is not in the whitelist tree`);
  }

  return proof;
}
```

Reading the root back from the `Config` PDA is the check that catches a stale client: the whitelist is authoritative on-chain, so compare `config.merkleRoot` with your root before building the transfer. A zero root means no whitelist has been published yet, and a `paused` config means transfers are deliberately halted.

> **Keccak, not SHA3.** The npm `keccak256` package and `solana_keccak_hasher::hash`/`hashv` are both Keccak-256. `crypto.createHash("sha3-256")` is a different padding and produces different hashes, so every proof would fail. If you use `ethers`, `ethers.keccak256` is the correct one.

---

### How the Proof Reaches the Hook

**Not through the instruction data.** Token-2022 constructs the hook's instruction itself, in `spl_transfer_hook_interface::onchain::invoke_execute`:

| Offset | Bytes | Field |
| ---: | ---: | :--- |
| 0 | 8 | SPL interface discriminator (`spl-transfer-hook-interface:execute`) |
| 8 | 8 | `amount` (`u64`, little-endian) |

…and that is the whole payload. `invoke_execute` forwards **none** of the transfer instruction's trailing bytes, so a proof cannot be delivered as an argument. Appending one is not merely ignored: `transfer_checked` requires an exact data length, so extra bytes make Token-2022 reject the entire transfer with `InvalidInstructionData` before the hook is reached.

**The proofs travel as accounts.** Each institution has a `WhitelistEntry` PDA at `[b"whitelist", owner]` holding its path, and the mint's `ExtraAccountMetaList` tells Token-2022 to resolve, for every transfer, the entries belonging to the two token accounts' `owner` fields:

| Index in the transfer's accounts | Used as | Resolved entry |
| ---: | :--- | :--- |
| 0 | `source` token account | `[b"whitelist", source.owner[32..64]]` |
| 2 | `destination` token account | `[b"whitelist", destination.owner[32..64]]` |

That offset (32) is the `owner` field of the token account's base layout, which SPL Token and Token-2022 share, and it is read with `Seed::AccountData` in `initialize_extra_account_meta_list`. Two properties follow from deriving the address rather than believing a caller-supplied one: a transfer cannot present an entry for a wallet that is not the one spending or receiving, and an institution with no entry simply cannot be a party to a transfer — Token-2022 has no account to resolve, so the transaction fails before `execute` runs.

A client therefore has to include five hook accounts in the transfer's remaining accounts: the hook **program id itself** (Token-2022 CPI's into it, and the callee's program account must be in the transfer's account list for that call to resolve — without it the failure surfaces from inside the token program as `Unknown program`), the meta list, the sender's entry, the recipient's entry, and the config. Order among them does not matter: Token-2022 matches them by address. `programs/rwa_mint`'s `transfer` forwards exactly that set — see its README, or `core/instructions-rails.ts` in the web console, which builds the same list.

> **A plain `@solana/spl-token` transfer now works, if the entries exist.** `createTransferCheckedWithTransferHookInstruction` resolves the hook's extra accounts itself — including the two entries — so nothing has to be appended to the data. What it cannot do is *create* an entry for a wallet that has none; that is `set_whitelist_entry`.
>
> Note the amount the hook sees is the *pre-fee* transfer amount, while the destination is credited `amount - fee` and the fee is withheld in the destination token account. If `update_transfer_fee` has set a non-zero rate, use the fee-aware path (the fee branch of the hook helper, or `createTransferCheckedWithFeeInstruction`) so the client agrees the fee up front: `rwa_mint.transfer` as written builds a plain `transfer_checked`.

---

## Error Codes

| Code | Hex | Name | Message |
|------|-----|------|---------|
| `6000` | `0x1770` | `DestinationNotWhitelisted` | The destination wallet is not on the institutional whitelist. |
| `6001` | `0x1771` | `SourceNotWhitelisted` | The source wallet is not on the institutional whitelist. |
| `6002` | `0x1772` | `WhitelistNotInitialized` | The whitelist root has not been initialized. |
| `6003` | `0x1773` | `Unauthorized` | Only the designated authority can update the whitelist. |
| `6004` | `0x1774` | `WhitelistPaused` | Transfers are paused by the whitelist authority. |
| `6005` | `0x1775` | `InvalidRoot` | The Merkle root cannot be all zeros; use `set_paused` to halt transfers. |
| `6006` | `0x1776` | `InvalidAuthority` | The new authority cannot be the default pubkey. |
| `6007` | `0x1777` | `MintNotConfigured` | This mint's transfer hook extension does not point at this program. |
| `6008` | `0x1778` | `UnauthorizedInitializer` | Only the program's upgrade authority may initialize the whitelist. |
| `6009` | `0x1779` | `InvalidWhitelistEntry` | This entry does not belong to the wallet it is proving for. |
| `6010` | `0x177A` | `ProofNotInWhitelist` | This Merkle path does not match the published whitelist root. |
| `6011` | `0x177B` | `ProofTooLong` | A whitelist entry holds at most 20 proof nodes. |

---

## Known Gaps & Hardening

1. **A rotation is only half a write.** `update_root` replaces the whole whitelist atomically and revokes every entry whose path no longer folds to the new root — which is the desired failure direction (delisting is instant and cannot be forgotten), but it means re-granting access is `update_root` **plus** one `set_whitelist_entry` per member. For a large whitelist that is a keeper job, and there is no batch instruction yet (see `docs/proposed-features.md`, which proposes one for the settlement program's analogous flow). Announce rotations, because until the entries are refreshed the affected institutions simply cannot transfer.

2. **Entries are the on-chain footprint.** One 681-byte account per institution (~0.0048 SOL of rent-exempt deposit, refunded by `close_whitelist_entry`). Storing the path is what makes the proofs reachable at all; the alternative — trusting an entry's existence as proof of membership — would drop the root check, and with it the single-commitment audit story.

3. **`MAX_PROOF_DEPTH` is 20.** A tree deeper than that cannot be stored in an entry, so very large whitelists have to be sharded (one tree per jurisdiction, one hook registration per tree) rather than grown.

---

## Security & Architecture Considerations

1. **Owner Spoofing Prevention**:
   The hook reads `destination_token.owner` directly from the deserialized `destination_token` account (an Anchor `InterfaceAccount<'info, TokenAccount>`). Since Token-2022 validates account ownership before CPI, malicious callers cannot spoof the destination owner address.

2. **Domain separation and sorted pairs**:
   Leaves are `keccak256(0x00 ‖ pubkey)` and parents `keccak256(0x01 ‖ left ‖ right)`, so a leaf and an inner node can never be the same shape, and the pair is sorted (`hashes.sort_unstable()`) before hashing:

   ```rust
   let mut current_hash = hashv(&[&LEAF_PREFIX, destination_owner_key.as_ref()]).to_bytes();

   for p in proof.iter() {
       let mut hashes = [current_hash, *p];
       hashes.sort_unstable();
       current_hash = hashv(&[&NODE_PREFIX, hashes[0].as_ref(), hashes[1].as_ref()]).to_bytes();
   }
   ```

   The prefixes are what stop an inner node being presented as a leaf; the sorting is what makes verification deterministic without explicit left/right position bits in the proof.

3. **Authority Multisig**:
   The `authority` in `Config` should always be set to an institutional multisig (e.g., Squads) or a governance program rather than a single private key.

4. **Rent Exemption & Account Sizing**:
   The `ExtraAccountMetaList` PDA is dynamically sized using `ExtraAccountMetaList::size_of(...)` and allocated via CPI to `system_program::create_account` signed with PDA seeds.
