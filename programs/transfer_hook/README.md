# RWA Transfer Hook Program

An Anchor-based implementation of the **SPL Transfer Hook Interface** for **SPL Token-2022 (Token Extensions)**.

This program enforces institutional compliance and regulatory whitelisting for Real World Asset (RWA) tokens. On every token transfer, it cryptographically validates that the destination wallet belongs to an authorized institution using a **Merkle Tree Proof** verified against an on-chain root.

---

## Table of Contents

- [RWA Transfer Hook Program](#rwa-transfer-hook-program)
  - [Table of Contents](#table-of-contents)
  - [Overview](#overview)
    - [Why Merkle Trees?](#why-merkle-trees)
  - [Architecture \& Workflow](#architecture--workflow)
  - [Accounts \& PDAs](#accounts--pdas)
    - [1. `Config` Account](#1-config-account)
    - [2. `ExtraAccountMetaList` Account](#2-extraaccountmetalist-account)
  - [Program Instructions](#program-instructions)
    - [1. `initialize`](#1-initialize)
    - [2. `update_root`](#2-update_root)
    - [3. `initialize_extra_account_meta_list`](#3-initialize_extra_account_meta_list)
    - [4. `execute`](#4-execute)
  - [Client Usage Guide](#client-usage-guide)
    - [Prerequisites \& Dependencies](#prerequisites--dependencies)
    - [Step 1: Initialize Hook Config](#step-1-initialize-hook-config)
    - [Step 2: Generate Merkle Tree \& Proofs](#step-2-generate-merkle-tree--proofs)
    - [Step 3: Update Merkle Root](#step-3-update-merkle-root)
    - [Step 4: Initialize ExtraAccountMetaList for Mint](#step-4-initialize-extraaccountmetalist-for-mint)
    - [Step 5: Execute a Compliant Token Transfer](#step-5-execute-a-compliant-token-transfer)
  - [Error Codes](#error-codes)
  - [Security \& Architecture Considerations](#security--architecture-considerations)

---

## Overview

In institutional Real World Asset (RWA) tokenization, regulatory frameworks (such as SEC rules, MiCA, and KYC/AML guidelines) require that assets only be held and transferred between verified and whitelisted parties.

### Why Merkle Trees?

- **Cost Efficiency**: Storing thousands of whitelisted institution addresses directly in Solana accounts incurs high storage rent. A Merkle root stores the entire whitelist state in a single **32-byte hash**.
- **Scalability**: Updating the whitelist only requires updating the 32-byte root. Millions of addresses can be supported with logarithmic proof verification cost ($O(\log N)$ Keccak-256 hashes on-chain).
- **Privacy & Composability**: Only the transferring party needs to reveal their proof at execution time.

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
                             +------------------------+
                                         ^
                                         | Read during validation
                                         |
+--------------------+   Token-2022      |      +------------------------+
| Source Institution | -------------> CPI ----> |     Transfer Hook      |
|  Token Account     | transfer_checked  |      |       `execute`        |
+--------------------+                   |      +------------------------+
                                         |           |
                                         |           | 1. Extract destination.owner
                                         |           | 2. Hash owner with Keccak-256
                                         |           | 3. Verify Merkle proof against root
                                         |           v
                                         |       [ Transfer Approved / Rejected ]
                                         v
                             +------------------------+
                             | Destination Account    |
                             | (Must be whitelisted)  |
                             +------------------------+
```

1. **Token-2022 Transfer Hook Extension**: When an RWA mint is created, it is configured with the `TransferHook` extension pointing to this program ID.
2. **Account Resolution**: When a client builds a transfer instruction, `@solana/spl-token` fetches the `ExtraAccountMetaList` PDA for the mint. This account defines additional accounts that Token-2022 must forward to the hook (specifically, the `Config` PDA).
3. **Execution & Verification**: Token-2022 invokes `execute` via Cross-Program Invocation (CPI). The hook reads the destination token account's owner, hashes it, checks the supplied Merkle proof against the stored root, and rejects the transaction if the destination is unwhitelisted.

---

## Accounts & PDAs

### 1. `Config` Account
Holds protocol-wide authority and the current Merkle root of whitelisted institutional addresses.

- **Seeds**: `[b"config"]`
- **Fields**:
  - `authority: Pubkey` (32 bytes) – The admin or multisig that can update the Merkle root.
  - `merkle_root: [u8; 32]` (32 bytes) – Current Keccak-256 Merkle root.
  - `bump: u8` (1 byte) – Canonical bump seed.
- **Size**: `8 + 32 + 32 + 1 = 73 bytes`

### 2. `ExtraAccountMetaList` Account
SPL TLV-encoded account required by the Token-2022 standard. For each Token-2022 mint using this hook, this PDA specifies which additional accounts must be resolved and passed to `execute`.

- **Seeds**: `[b"extra-account-metas", mint.key().as_ref()]`
- **Program Owner**: `transfer_hook` program.
- **Configured Extra Accounts**:
  1. `Config` PDA (`[b"config"]`, writable: `true`, signer: `false`).

---

## Program Instructions

### 1. `initialize`
Initializes the global `Config` account, setting the authority and an empty root (`[0u8; 32]`).

```rust
pub fn initialize(ctx: Context<Initialize>) -> Result<()>
```

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | PDA initialized with seeds `[b"config"]`. |
| `authority` | `Signer<'info>` | Payer and admin authority for the hook. |
| `system_program` | `Program<'info, System>` | Solana System Program. |

---

### 2. `update_root`
Updates the 32-byte Merkle root representing the set of whitelisted institutions.

```rust
pub fn update_root(ctx: Context<UpdateRoot>, new_root: [u8; 32]) -> Result<()>
```

**Parameters:**
- `new_root`: `[u8; 32]` – The new Keccak-256 root of whitelisted institutional wallet addresses.

**Accounts:**
| Name | Type | Description |
|------|------|-------------|
| `config` | `Account<'info, Config>` | Writable `Config` PDA (`[b"config"]`). |
| `authority` | `Signer<'info>` | Must match `config.authority`. |

---

### 3. `initialize_extra_account_meta_list`
Creates and initializes the `ExtraAccountMetaList` PDA for a specific Token-2022 mint. This registers the `Config` PDA so Token-2022 automatically passes it to `execute`.

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

### 4. `execute`
The core validation hook. Automatically called by Token-2022 upon every `transfer_checked` instruction.

```rust
pub fn execute<'info>(
    ctx: Context<Execute<'info>>,
    _amount: u64,
    proof: Vec<[u8; 32]>,
) -> Result<()>
```

**Parameters:**
- `_amount`: `u64` – Transfer amount (passed by Token-2022 standard).
- `proof`: `Vec<[u8; 32]>` – Array of 32-byte sibling hashes forming the Merkle proof for the destination wallet.

**Accounts (in strict order expected by Token-2022):**
1. `source_token`: `InterfaceAccount<'info, TokenAccount>` – Source token account.
2. `mint`: `InterfaceAccount<'info, Mint>` – Token-2022 mint.
3. `destination_token`: `InterfaceAccount<'info, TokenAccount>` – Destination token account.
4. `owner`: `UncheckedAccount<'info>` – Owner/authority of the source token account.
5. `extra_account_meta_list`: `UncheckedAccount<'info>` – PDA for extra account metadata.
6. `config`: `Account<'info, Config>` – Hook configuration containing the Merkle root.

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

Institutions are hashed using **Keccak-256** directly on their 32-byte public keys. Hashes at each level are sorted to maintain deterministic parent computation.

```typescript
import { PublicKey } from "@solana/web3.js";
import { MerkleTree } from "merkletreejs";
import keccak256 from "keccak256";

// Whitelisted institutional wallet public keys
const whitelistedWallets: PublicKey[] = [
  new PublicKey("Institution111111111111111111111111111111111"),
  new PublicKey("Institution222222222222222222222222222222222"),
  new PublicKey("Institution333333333333333333333333333333333"),
];

// 1. Hash wallet public keys (leaves)
const leaves = whitelistedWallets.map((wallet) =>
  keccak256(wallet.toBuffer())
);

// 2. Build Merkle Tree with sorted pairs
const merkleTree = new MerkleTree(leaves, keccak256, { sortPairs: true });

// 3. Get 32-byte Root
const root = merkleTree.getRoot(); // Buffer of 32 bytes
console.log("Merkle Root (hex):", root.toString("hex"));

// 4. Generate proof for a specific destination
function getProofForWallet(wallet: PublicKey): number[][] {
  const leaf = keccak256(wallet.toBuffer());
  const proofBuffers = merkleTree.getProof(leaf);
  return proofBuffers.map((p) => Array.from(p.data));
}
```

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

### Step 5: Execute a Compliant Token Transfer

When performing a transfer, the client uses `@solana/spl-token` helpers to build the `transferChecked` instruction and attach the resolved extra accounts along with the Merkle proof.

```typescript
import {
  createTransferCheckedWithTransferHookInstruction,
} from "@solana/spl-token";

async function transferRWA(
  mint: PublicKey,
  sourceTokenAccount: PublicKey,
  destinationTokenAccount: PublicKey,
  destinationOwnerWallet: PublicKey,
  amount: bigint,
  decimals: number
) {
  // 1. Generate off-chain Merkle proof for destination
  const proof = getProofForWallet(destinationOwnerWallet);

  // 2. Create the transfer instruction with hook resolution
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

  // 3. If invoking the hook instruction with custom proof arguments:
  // Append proof data to instruction or pass through the execute CPI
  const tx = new anchor.web3.Transaction().add(transferIx);
  const sig = await provider.sendAndConfirm(tx);

  console.log("Transfer successful. Signature:", sig);
}
```

---

## Error Codes

| Code | Hex | Name | Message |
|------|-----|------|---------|
| `6000` | `0x1770` | `DestinationNotWhitelisted` | The destination wallet is not on the institutional whitelist. |
| `6001` | `0x1771` | `WhitelistNotInitialized` | The whitelist root has not been initialized. |
| `6002` | `0x1772` | `Unauthorized` | Only the designated authority can update the whitelist. |

---

## Security & Architecture Considerations

1. **Owner Spoofing Prevention**:
   The hook reads `destination_token.owner` directly from the deserialized `destination_token` account (an Anchor `InterfaceAccount<'info, TokenAccount>`). Since Token-2022 validates account ownership before CPI, malicious callers cannot spoof the destination owner address.

2. **Merkle Sorting & Second-Preimage Resistance**:
   Leaves and internal pairs are sorted (`hashes.sort_unstable()`) prior to hashing:

   ```rust
   let mut hashes = [current_hash, *p];
   hashes.sort_unstable();
   current_hash = hashv(&[hashes[0].as_ref(), hashes[1].as_ref()]).to_bytes();
   ```

   This ensures deterministic verification without requiring explicit left/right position bits in the proof.

3. **Authority Multisig**:
   The `authority` in `Config` should always be set to an institutional multisig (e.g., Squads) or a governance program rather than a single private key.

4. **Rent Exemption & Account Sizing**:
   The `ExtraAccountMetaList` PDA is dynamically sized using `ExtraAccountMetaList::size_of(...)` and allocated via CPI to `system_program::create_account` signed with PDA seeds.
