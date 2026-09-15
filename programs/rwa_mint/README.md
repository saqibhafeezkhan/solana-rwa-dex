# RWA Mint Program

An Anchor-based manager for creating and managing **SPL Token-2022 (Token Extensions)** mints tailored specifically for Real World Asset (RWA) tokenization.

This program orchestrates the creation of complex RWA token mints with multiple native extensions enabled—including Transfer Hook enforcement, embedded Token Metadata, Metadata Pointers, Permanent Delegates for regulatory compliance, and configurable Transfer Fees.

---

## Table of Contents

- [RWA Mint Program](#rwa-mint-program)
  - [Table of Contents](#table-of-contents)
  - [Overview](#overview)
    - [Why Manual Dynamic Mint Creation?](#why-manual-dynamic-mint-creation)
    - [Enabled Token-2022 Extensions](#enabled-token-2022-extensions)
  - [Architecture \& Workflow](#architecture--workflow)
  - [Data Structures \& Parameters](#data-structures--parameters)
    - [1. `RwaMintParams`](#1-rwamintparams)
    - [2. `TokenMetadataParams`](#2-tokenmetadataparams)
  - [Program Instructions](#program-instructions)
    - [1. `create_rwa_mint`](#1-create_rwa_mint)
    - [2. `mint_to`](#2-mint_to)
    - [3. `transfer`](#3-transfer)
    - [4. `update_transfer_fee`](#4-update_transfer_fee)
  - [Account Contexts](#account-contexts)
    - [1. `CreateRwaMint`](#1-createrwamint)
    - [2. `MintTo`](#2-mintto)
    - [3. `Transfer`](#3-transfer)
    - [4. `UpdateTransferFee`](#4-updatetransferfee)
  - [Client Usage Guide](#client-usage-guide)
    - [Prerequisites \& Dependencies](#prerequisites--dependencies)
    - [Step 1: Create an RWA Token Mint](#step-1-create-an-rwa-token-mint)
    - [Step 2: Mint Tokens to an Approved Account](#step-2-mint-tokens-to-an-approved-account)
    - [Step 3: Transfer Tokens with Merkle Proof Forwarding](#step-3-transfer-tokens-with-merkle-proof-forwarding)
    - [Step 4: Update Transfer Fee Configuration](#step-4-update-transfer-fee-configuration)
  - [Security \& Architecture Considerations](#security--architecture-considerations)

---

## Overview

Institutional Real World Asset (RWA) tokens require multi-layered regulatory controls directly embedded at the token standard level. Rather than relying on custom wrapped tokens or proxy smart contracts, the `rwa_mint` program leverages **Solana Token-2022 (Token Extensions)** to enforce compliance natively.

### Why Manual Dynamic Mint Creation?

Standard Anchor account initialization (`#[account(init)]`) relies on static space allocation. However, Token-2022 mint accounts with dynamic extensions (such as embedded metadata strings of variable length) require dynamic byte calculation prior to account creation.

1. **Dynamic Space Calculation**: Extension sizes are calculated dynamically on-chain using `ExtensionType::try_calculate_account_len::<SplMint>(&extensions)`.
2. **Strict Extension Initialization Ordering**: Token-2022 requires all extension configurations (Transfer Hook, Metadata Pointer, Permanent Delegate, Transfer Fee) to be initialized **before** the base mint itself is initialized.
3. **Atomic Setup**: The program creates the System account, initializes all 4 extensions, initializes the base mint, and populates on-chain metadata in a single atomic transaction.

### Enabled Token-2022 Extensions

| Extension | Purpose in RWA Context |
|-----------|------------------------|
| **TransferHook** | Links the token mint to the whitelist enforcement program (enforces Merkle-tree verified institutional transfer restrictions). |
| **MetadataPointer** | Points the SPL Metadata Interface directly to the mint account itself. |
| **TokenMetadata** | Embeds token name, symbol, and off-chain URI (pointing to ISIN, CUSIP, and Net Asset Value / NAV documentation) directly inside the mint. |
| **PermanentDelegate** | Assigns a designated regulatory/admin authority capable of seizing or transferring tokens under legal court order or compliance requirements. |
| **TransferFeeConfig** | Configures optional protocol/maker/taker transfer fees managed by a dedicated fee authority. |
| *(Optional) NonTransferable* | Can be enabled for soul-bound / non-transferable institutional identity or credential tokens. |

---

## Architecture & Workflow

```
                             +-------------------------------+
                             |    Issuer / Authority Key     |
                             +-------------------------------+
                                             |
                                 (1) create_rwa_mint(params)
                                             v
                             +-------------------------------+
                             |    RWA Mint Program           |
                             |  (Mint11111111111111...)      |
                             +-------------------------------+
                                 |         |         |         |
             +-------------------+         |         +-------------------+
             | (1a) Calculate size         |                             | (1d) Init Base Mint
             v                             v (1c) Init Metadata          v
+--------------------------+   +-----------------------+   +--------------------------+
|  System Program          |   | Token-2022 Program    |   | Token-2022 Mint Account  |
|  create_account          |   | - TransferHook        |   | - Decimals: 6            |
+--------------------------+   | - MetadataPointer     |   | - Mint Auth: Authority   |
                               | - PermanentDelegate   |   | - Freeze Auth: Authority |
                               | - TransferFeeConfig   |   +--------------------------+
                               +-----------------------+

---------------------------------------------------------------------------------------

                             +-------------------------------+
                             |       Transfer Action         |
                             +-------------------------------+
                                             |
                                    (2) transfer(amount, proof)
                                             v
                             +-------------------------------+
                             |    RWA Mint Program           |
                             |  - Appends serialized proof   |
                             |  - Invokes CPI transfer       |
                             +-------------------------------+
                                             |
                                     CPI (transfer_checked)
                                             v
                             +-------------------------------+
                             |    Token-2022 Program        |
                             |  - Invokes Transfer Hook      |
                             +-------------------------------+
                                             |
                                     CPI (execute + proof)
                                             v
                             +-------------------------------+
                             |     Transfer Hook Program     |
                             |  - Validates Merkle Proof     |
                             +-------------------------------+
```

---

## Data Structures & Parameters

### 1. `RwaMintParams`

Parameters passed to `create_rwa_mint` to configure the mint properties and authority mappings.

```rust
pub struct RwaMintParams {
    pub decimals: u8,
    pub transfer_hook_program: Pubkey,
    pub delegate: Pubkey,
    pub fee_authority: Pubkey,
    pub metadata: TokenMetadataParams,
}
```

- `decimals: u8` – Number of decimal places for the token (typically `6` for USD-backed or financial assets).
- `transfer_hook_program: Pubkey` – Program ID of the whitelist transfer hook program.
- `delegate: Pubkey` – Public key of the compliance permanent delegate authority.
- `fee_authority: Pubkey` – Public key authorized to update or collect transfer fees.
- `metadata: TokenMetadataParams` – Embedded token metadata parameters.

### 2. `TokenMetadataParams`

Embedded SPL token metadata attributes.

```rust
pub struct TokenMetadataParams {
    pub name: String,
    pub symbol: String,
    pub uri: String,
}
```

- `name: String` – Human-readable token name (e.g., `"Institutional US Treasury Fund"`).
- `symbol: String` – Ticker symbol (e.g., `"USTB"`).
- `uri: String` – Off-chain URI linking to legal prospectus, ISIN/CUSIP identifier, or NAV JSON payload.

---

## Program Instructions

### 1. `create_rwa_mint`

Creates a new Token-2022 mint account, allocates space for all required extensions, initializes extensions, initializes the base mint, and writes on-chain metadata.

```rust
pub fn create_rwa_mint(
    ctx: Context<CreateRwaMint>,
    params: RwaMintParams,
) -> Result<()>
```

**Parameters:**
- `params`: `RwaMintParams` – Mint configuration object.

---

### 2. `mint_to`

Mints tokens to a designated destination token account. Can only be invoked by the mint authority.

```rust
pub fn mint_to(
    ctx: Context<MintTo>,
    amount: u64,
) -> Result<()>
```

**Parameters:**
- `amount`: `u64` – Quantity of tokens to mint (in base units / atomic units).

---

### 3. `transfer`

Executes a Token-2022 transfer and serializes the off-chain Merkle proof into the trailing instruction data for automatic forwarding to the transfer hook program.

```rust
pub fn transfer<'info>(
    ctx: Context<'info, Transfer<'info>>,
    amount: u64,
    proof: Vec<[u8; 32]>,
) -> Result<()>
```

**Parameters:**
- `amount`: `u64` – Number of tokens to transfer.
- `proof`: `Vec<[u8; 32]>` – Merkle proof array verifying destination wallet eligibility.

---

### 4. `update_transfer_fee`

Updates the transfer fee rate and maximum fee cap. Requires signature from `fee_authority`.

```rust
pub fn update_transfer_fee(
    ctx: Context<UpdateTransferFee>,
    new_fee_basis_points: u16,
    new_maximum_fee: u64,
) -> Result<()>
```

**Parameters:**
- `new_fee_basis_points`: `u16` – Fee basis points (e.g., `25` = 0.25%).
- `new_maximum_fee`: `u64` – Maximum fee cap in base token units.

---

## Account Contexts

### 1. `CreateRwaMint`

```rust
pub struct CreateRwaMint<'info> {
    pub mint: Signer<'info>,
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}
```

| Account | Type | Description |
|---------|------|-------------|
| `mint` | `Signer<'info>` (mut) | Keypair of the new mint account. Must sign for system account creation. |
| `authority` | `Signer<'info>` (mut) | Payer, mint authority, and metadata update authority. |
| `token_program` | `Program<'info, Token2022>` | SPL Token-2022 Program (`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`). |
| `system_program` | `Program<'info, System>` | Solana System Program. |

---

### 2. `MintTo`

```rust
pub struct MintTo<'info> {
    pub mint: InterfaceAccount<'info, Mint>,
    pub destination: InterfaceAccount<'info, TokenAccount>,
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token2022>,
}
```

| Account | Type | Description |
|---------|------|-------------|
| `mint` | `InterfaceAccount<'info, Mint>` (mut) | The Token-2022 mint. |
| `destination` | `InterfaceAccount<'info, TokenAccount>` (mut) | Destination token account receiving the minted tokens. |
| `authority` | `Signer<'info>` (mut) | Mint authority signing the minting operation. |
| `token_program` | `Program<'info, Token2022>` | SPL Token-2022 Program. |

---

### 3. `Transfer`

```rust
pub struct Transfer<'info> {
    pub source: InterfaceAccount<'info, TokenAccount>,
    pub destination: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token2022>,
}
```

| Account | Type | Description |
|---------|------|-------------|
| `source` | `InterfaceAccount<'info, TokenAccount>` (mut) | Source token account. |
| `destination` | `InterfaceAccount<'info, TokenAccount>` (mut) | Destination token account. |
| `mint` | `InterfaceAccount<'info, Mint>` | Token-2022 mint. |
| `authority` | `Signer<'info>` | Owner / delegate of the source token account. |
| `token_program` | `Program<'info, Token2022>` | SPL Token-2022 Program. |
| `remaining_accounts` | `AccountInfo<'info>` | Extra accounts required for transfer hook execution (`ExtraAccountMetaList` PDA, `Config` PDA, Transfer Hook Program). |

---

### 4. `UpdateTransferFee`

```rust
pub struct UpdateTransferFee<'info> {
    pub mint: InterfaceAccount<'info, Mint>,
    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token2022>,
}
```

| Account | Type | Description |
|---------|------|-------------|
| `mint` | `InterfaceAccount<'info, Mint>` (mut) | Token-2022 mint account. |
| `authority` | `Signer<'info>` (mut) | Must match configured `fee_authority`. |
| `token_program` | `Program<'info, Token2022>` | SPL Token-2022 Program. |

---

## Client Usage Guide

### Prerequisites & Dependencies

In your TypeScript project:

```bash
npm install @coral-xyz/anchor @solana/web3.js @solana/spl-token
```

---

### Step 1: Create an RWA Token Mint

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { TOKEN_2022_PROGRAM_ID } from "@solana/spl-token";
import { RwaMint } from "./target/types/rwa_mint";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);

const program = anchor.workspace.RwaMint as Program<RwaMint>;

async function createRwaMint() {
  const mintKeypair = Keypair.generate();
  const transferHookProgramId = new PublicKey("Hook111111111111111111111111111111111111111");
  const permanentDelegate = provider.wallet.publicKey;
  const feeAuthority = provider.wallet.publicKey;

  const mintParams = {
    decimals: 6,
    transferHookProgram: transferHookProgramId,
    delegate: permanentDelegate,
    feeAuthority: feeAuthority,
    metadata: {
      name: "Institutional US Treasury Fund",
      symbol: "USTB",
      uri: "https://arweave.net/isin-us1234567890-metadata.json",
    },
  };

  const tx = await program.methods
    .createRwaMint(mintParams)
    .accounts({
      mint: mintKeypair.publicKey,
      authority: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .signers([mintKeypair])
    .rpc();

  console.log("RWA Mint Created:", mintKeypair.publicKey.toBase58());
  console.log("Transaction Signature:", tx);
}
```

---

### Step 2: Mint Tokens to an Approved Account

```typescript
import { getAssociatedTokenAddressSync } from "@solana/spl-token";

async function mintToDestination(
  mint: PublicKey,
  destinationOwner: PublicKey,
  amount: number
) {
  const destinationAta = getAssociatedTokenAddressSync(
    mint,
    destinationOwner,
    false,
    TOKEN_2022_PROGRAM_ID
  );

  const tx = await program.methods
    .mintTo(new anchor.BN(amount))
    .accounts({
      mint: mint,
      destination: destinationAta,
      authority: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();

  console.log(`Minted ${amount} tokens to ${destinationAta.toBase58()}. Tx:`, tx);
}
```

---

### Step 3: Transfer Tokens with Merkle Proof Forwarding

```typescript
async function transferWithProof(
  mint: PublicKey,
  sourceAta: PublicKey,
  destinationAta: PublicKey,
  transferHookProgram: PublicKey,
  proofBuffers: Buffer[],
  amount: number
) {
  // Convert proof buffers to byte arrays
  const proofArray = proofBuffers.map((b) => Array.from(b));

  // Resolve hook PDAs required for remaining_accounts
  const [extraAccountMetaListPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("extra-account-metas"), mint.toBuffer()],
    transferHookProgram
  );

  const [hookConfigPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("config")],
    transferHookProgram
  );

  const tx = await program.methods
    .transfer(new anchor.BN(amount), proofArray)
    .accounts({
      source: sourceAta,
      destination: destinationAta,
      mint: mint,
      authority: provider.wallet.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .remainingAccounts([
      { pubkey: transferHookProgram, isWritable: false, isSigner: false },
      { pubkey: extraAccountMetaListPda, isWritable: false, isSigner: false },
      { pubkey: hookConfigPda, isWritable: false, isSigner: false },
    ])
    .rpc();

  console.log("Compliant Transfer Executed. Signature:", tx);
}
```

---

### Step 4: Update Transfer Fee Configuration

```typescript
async function updateFee(
  mint: PublicKey,
  newBasisPoints: number,
  newMaxFee: number
) {
  const tx = await program.methods
    .updateTransferFee(newBasisPoints, new anchor.BN(newMaxFee))
    .accounts({
      mint: mint,
      authority: provider.wallet.publicKey, // fee authority
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();

  console.log(`Updated transfer fee to ${newBasisPoints} bps. Tx:`, tx);
}
```

---

## Security & Architecture Considerations

1. **Extension Initialization Precedence**:
   Token-2022 strictly requires extension memory headers to be initialized **prior** to calling `initialize_mint2`. Attempting to initialize extensions after base mint initialization will result in program failure (`InstructionError`).

2. **Proof Data Forwarding via Trailing Bytes**:
   The `transfer` instruction packs the Borsh-serialized Merkle proof into the instruction data of `transfer_checked`. Token-2022 parses trailing instruction data and passes it verbatim to the registered Transfer Hook program CPI context.

3. **Permanent Delegate Authority**:
   The `PermanentDelegate` key is stored on-chain inside the mint extension. This authority possesses unconditional transfer capabilities over all token accounts associated with the mint, fulfilling regulatory compliance requirements for asset recovery and frozen asset administration.

4. **Fee Authority Isolation**:
   The `TransferFeeConfig` authority operates independently of the `mint_authority` and `freeze_authority`, enabling delegated financial administration or automated fee adjustment contracts.
