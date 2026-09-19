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
  - [Maths, Sizing \& Rent](#maths-sizing--rent)
    - [Account length](#account-length)
    - [Token metadata length](#token-metadata-length)
    - [Rent and lamports](#rent-and-lamports)
    - [Transfer fee maths](#transfer-fee-maths)
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
  - [Authority Roles](#authority-roles)
  - [Client Usage Guide](#client-usage-guide)
    - [Prerequisites \& Dependencies](#prerequisites--dependencies)
    - [Step 1: Create an RWA Token Mint](#step-1-create-an-rwa-token-mint)
    - [Step 2: Mint Tokens to an Approved Account](#step-2-mint-tokens-to-an-approved-account)
    - [Step 3: Transfer Tokens with Merkle Proof Forwarding](#step-3-transfer-tokens-with-merkle-proof-forwarding)
    - [Step 4: Update Transfer Fee Configuration](#step-4-update-transfer-fee-configuration)
    - [Step 5: Create the Token Account](#step-5-create-the-token-account)
    - [Step 6: Withdraw Withheld Fees](#step-6-withdraw-withheld-fees)
  - [End-to-End Integration Order](#end-to-end-integration-order)
  - [Common Failure Modes](#common-failure-modes)
  - [Security \& Architecture Considerations](#security--architecture-considerations)

---

## Overview

Institutional Real World Asset (RWA) tokens require multi-layered regulatory controls directly embedded at the token standard level. Rather than relying on custom wrapped tokens or proxy smart contracts, the `rwa_mint` program leverages **Solana Token-2022 (Token Extensions)** to enforce compliance natively.

### Why Manual Dynamic Mint Creation?

Standard Anchor account initialization (`#[account(init)]`) relies on static space allocation. However, Token-2022 mint accounts with dynamic extensions (such as embedded metadata strings of variable length) require dynamic byte calculation prior to account creation.

1. **Dynamic Space Calculation**: Extension sizes are calculated dynamically on-chain using `ExtensionType::try_calculate_account_len::<SplMint>(&extensions)`, plus `TokenMetadata::tlv_size_of()` for the metadata entry - the metadata is written into the same account a few instructions later, so it has to be allocated and funded up front (`mint_space`).
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

## 📐 Maths, Sizing & Rent

### Account length

The mint is created by hand rather than with `#[account(init)]` precisely because its length is not a constant. Every Token-2022 account with extensions starts its TLV area after `Account::LEN` (165 bytes, the *token account* length - that is what stops an extended mint being mistaken for a token account), followed by the 1-byte account type. Each extension then costs 2 bytes of type, 2 bytes of length, and its own state:

$$L = \underbrace{166}_{165 + 1} + \sum_{e \in E} \big(4 + |e|\big)$$

| Extension | State `\|e\|` | TLV `4 + \|e\|` |
| :--- | ---: | ---: |
| `TransferFeeConfig` | 108 | 112 |
| `MetadataPointer` | 64 | 68 |
| `TransferHook` | 64 | 68 |
| `PermanentDelegate` | 32 | 36 |
| **the four this program enables** | | **284 → `L = 450` bytes** |

`ExtensionType::try_calculate_account_len::<SplMint>(&extensions)` is exactly this sum, which is why the program does not hard-code it.

### Token metadata length

`TokenMetadata` is the one extension that cannot go in that sum: its length depends on the name, symbol and URI, and its `try_get_type_len` is `unreachable!()` in the Token Interface crate. It is written as one more TLV entry once the pointer extensions exist, with its own length helper:

$$L_{meta} = \texttt{TokenMetadata::tlv\_size\_of()} = \underbrace{12}_{\text{TLV header}} + 32 + 32 + (4 + |name|) + (4 + |symbol|) + (4 + |uri|) + 4 = 92 + |name| + |symbol| + |uri|$$

Note the header: fixed extensions use a 2-byte type and a 2-byte length, while a variable-length entry is keyed by an 8-byte SPL type discriminator and a 4-byte length, so it costs 12 bytes, not 4. The rest is 32 bytes of update authority, 32 of mint, the three strings each behind a 4-byte length prefix, and a 4 for the (empty) `additional_metadata` vector. For the example metadata - name 28 bytes, symbol 4, URI 50 - that is `92 + 82 = 174` bytes, so the mint wants `450 + 174 = 624` bytes.

### Rent and lamports

Rent exemption is a linear function of the account length (128 bytes of account overhead, two years at 3 480 lamports per byte-year):

$$\text{lamports} = (L + 128) \times 6\,960$$

| Account | Length | Rent-exempt minimum |
| :--- | ---: | ---: |
| The four extensions only | 450 B | 4 022 880 lamports ≈ 0.00402 SOL |
| With the 174 B metadata entry above | 624 B | 5 233 920 lamports ≈ 0.00523 SOL |

> **The metadata entry has to be funded before it is written.** The token program writes the metadata by calling `alloc_and_serialize_variable_len_extension`, whose own comment is *"allocate a TLV entry for the space and write it in, assumes that there's enough SOL for the new rent-exemption"*: it resizes the mint to `base + existing TLV + new TLV` (the new TLV length being `VariableLenPack::get_packed_len`, i.e. exactly what `tlv_size_of` reports) and the account itself has to already cover that rent. `create_rwa_mint` therefore sizes and funds the metadata as part of the account:
>
> ```rust
> let space = mint_space(&params.metadata)?;              // 450 + 92 + |name| + |symbol| + |uri|
> let lamports = Rent::get()?.minimum_balance(space);
> ```
>
> with the two pieces behind helpers so they stay testable:
>
> ```rust
> /// 12 + 32 + 32 + 3 length-prefixed strings + 4, via the same helper the TLV
> /// writer uses. Unused fields are fixed-width defaults.
> fn metadata_tlv_len(metadata: &TokenMetadataParams) -> Result<usize> {
>     let token_metadata = TokenMetadata {
>         name: metadata.name.clone(),
>         symbol: metadata.symbol.clone(),
>         uri: metadata.uri.clone(),
>         ..Default::default()
>     };
>     Ok(token_metadata.tlv_size_of()?)
> }
>
> fn mint_space(metadata: &TokenMetadataParams) -> Result<usize> {
>     let extensions = ExtensionType::try_calculate_account_len::<SplMint>(&MINT_EXTENSIONS)?;
>     extensions
>         .checked_add(metadata_tlv_len(metadata)?)
>         .ok_or_else(|| anchor_lang::error::Error::from(ProgramError::InvalidAccountData))
> }
> ```
>
> Change `MINT_EXTENSIONS` or the metadata and both numbers move together; the unit tests pin the 450 and the 92 so a silent drift shows up as a test failure rather than as a failed `create_rwa_mint`.

### Transfer fee maths

The `TransferFeeConfig` extension this program installs charges a fee on every transfer. With `bps` basis points and a `maximum_fee` cap, in **base units**:

$$fee = \begin{cases} 0 & bps = 0 \ \text{or}\ amount = 0 \\[4pt] \min\Big(\big\lceil amount \times bps / 10\,000 \big\rceil,\ maximum\_fee\Big) & \text{otherwise} \end{cases}$$

The division rounds **up**, so a non-zero rate always withholds at least one base unit, and the maximum fee is what a whale transfer pays. The destination is credited `amount - fee`; the fee is withheld inside the destination token account until the withdraw-withheld authority claims it.

| Transfer (6-decimal token) | `amount` (base units) | `fee` @ 25 bps, max 50 tokens | Destination receives |
| :--- | ---: | ---: | ---: |
| 0.001 tokens | 1 000 | 3 (ceil of 2.5) | 997 |
| 1 000 tokens | 1 000 000 000 | 2 500 000 (2.5 tokens) | 997.5 tokens |
| 100 000 tokens | 100 000 000 000 | 50 000 000 (capped) | 99 950 tokens |

Going the other way - to have the destination receive exactly `post_fee` - the transfer has to be grossed up, again ceil and capped:

$$pre\_fee = \Big\lceil \frac{post\_fee \times 10\,000}{10\,000 - bps}\Big\rceil$$

To deliver exactly 100 tokens at 25 bps: `ceil(100 000 000 × 10 000 / 9 975) = 100 250 627`, fee `= 250 627`, destination gets exactly `100 000 000`. Setting the rate with `update_transfer_fee` sets **both** numbers, so the cap has to be expressed in base units too - a 50-token cap on a 6-decimal mint is `50_000_000`.

> The program has no instruction to *claim* withheld fees; the fee authority uses `@solana/spl-token`'s `withdrawWithheldTokensFromMint` / `...FromAccounts` (signed by the withdraw-withheld authority) to sweep them.

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

## 🔑 Authority Roles

`create_rwa_mint` writes a remarkable number of authorities onto one mint. Keeping them straight matters: several default to the same key in the examples, but they can be split across a multisig, a compliance service and a fee treasury.

| Role | Comes from | Installed by | Powers |
| :--- | :--- | :--- | :--- |
| Mint authority | `authority` | `initialize_mint2` | `mint_to`, and it must sign the metadata `initialize` |
| Freeze authority | `authority` | `initialize_mint2` | Freeze / thaw any token account of this mint |
| Metadata update authority | `authority` | metadata `initialize` | Update name, symbol, URI and custom fields |
| Metadata pointer authority | `authority` | metadata pointer `initialize` | Re-point where the mint's metadata lives |
| Permanent delegate | `params.delegate` | permanent delegate `initialize` | Unconditionally transfer or burn from **any** account of this mint |
| Transfer fee config authority | `params.fee_authority` | transfer fee `initialize` | Set / update the fee rate and cap |
| Withdraw withheld authority | `params.fee_authority` | transfer fee `initialize` | Sweep withheld fees out of token accounts |
| Transfer hook authority | `authority` | transfer hook `initialize` | Change which program runs on every transfer |
| Hook program | `params.transfer_hook_program` | transfer hook `initialize` | Runs the compliance check on every transfer |

The permanent delegate is the sharpest of these - it bypasses the owner's signature - so in a real deployment it should be a Squads-style multisig or a program-controlled PDA, and the `delegate` passed here is the only place it is set.

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
  // The deployed hook program: `anchor keys list` prints it, and it has to match
  // the `declare_id!` in programs/transfer_hook/src/lib.rs.
  const transferHookProgramId = new PublicKey("fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS");
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

### Step 5: Create the Token Account

Holders need a Token-2022 token account before anything can be minted to them. An associated token account is the usual way:

```typescript
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountInstruction,
  getAssociatedTokenAddressSync,
  TOKEN_2022_PROGRAM_ID,
} from "@solana/spl-token";

async function createHolderTokenAccount(mint: PublicKey, owner: PublicKey) {
  const ata = getAssociatedTokenAddressSync(mint, owner, false, TOKEN_2022_PROGRAM_ID);

  const tx = new anchor.web3.Transaction().add(
    createAssociatedTokenAccountInstruction(
      provider.wallet.publicKey, // payer
      ata,
      owner,
      mint,
      TOKEN_2022_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID
    )
  );
  return provider.sendAndConfirm(tx);
}
```

A mint's token accounts may also need extensions of their own (`TransferHookAccount` for a hook mint, `TransferFeeAmount` for a fee-bearing one - see `get_required_init_account_extensions` in the Token Interface crate). The associated-token-account path is the one to prefer; if you build the token account by hand, size it (`getAccountLen([...])`), initialize the account extensions, then initialize the account.

---

### Step 6: Withdraw Withheld Fees

Fees withheld by a transfer sit inside destination token accounts, so the fee authority sweeps them with the standard helpers rather than with this program:

```typescript
import { createWithdrawWithheldTokensFromMintInstruction } from "@solana/spl-token";

async function claimFees(mint: PublicKey, feeDestination: PublicKey) {
  const ix = createWithdrawWithheldTokensFromMintInstruction(
    mint,
    feeDestination,
    provider.wallet.publicKey, // withdraw-withheld authority
    [],                        // multiSigners
    TOKEN_2022_PROGRAM_ID
  );
  return provider.sendAndConfirm(new anchor.web3.Transaction().add(ix));
}
```

---

## 🧭 End-to-End Integration Order

The programs interlock, and several steps fail in confusing ways if they happen out of order:

```text
1. deploy transfer_hook                     → fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS
2. transfer_hook.initialize                 → Config PDA
3. transfer_hook.update_root(non-zero root) → whitelist live
4. rwa_mint.create_rwa_mint(hook program)   → mint + 4 extensions + metadata
5. transfer_hook.initialize_extra_account_meta_list(mint)
6. create token accounts for holders
7. rwa_mint.mint_to(...)                    → supply issued
8. rwa_mint.transfer(amount, proof)         → hook verifies, tokens move
```

The two program ids are the ones `anchor keys list` prints (`rwa_mint` = `5GbeJh4sySASsdH2zRdFX85LuXnu5y3rBkqhVi76Lzho`, `transfer_hook` = `fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS`), and they match `Anchor.toml` and each program's `declare_id!`.

Two ordering rules bite in practice: the `ExtraAccountMetaList` PDA has to exist before the first transfer (otherwise Token-2022 cannot resolve the `Config` PDA and the hook fails), and a root has to be set before the first transfer (`update_root` rejects a zero root, and `execute` refuses an uninitialised whitelist). Step 8 must go through `rwa_mint.transfer`, which is the step that attaches the Merkle proof - see the transfer-hook program's README for the byte layout.

---

## 🚑 Common Failure Modes

| Symptom | Likely cause | Fix |
| :--- | :--- | :--- |
| `insufficient funds for rent` / account not rent-exempt during metadata init | Mint funded for the fixed extensions only - it was created before `mint_space` was used, or `MINT_EXTENSIONS`/the metadata changed without it | Size and fund with `mint_space(&params.metadata)` (see [Rent and lamports](#rent-and-lamports)) |
| `InvalidInstruction` after creating the account | An extension was initialized after `initialize_mint2`, or out of the required order | Keep the order: extensions (hook → pointer → delegate → fee) → base mint → metadata |
| `AccountNotRentExempt` on `create_rwa_mint` | `lamports` below `minimum_balance(space)` | Use `Rent::get()?.minimum_balance(space)` |
| `MissingRequiredSignature` | `mint` and `authority` are signers here, and both must sign | Add the mint keypair to `.signers([...])` and sign with the authority |
| `IncorrectMintAuthority` on metadata init | The metadata `initialize` requires the *mint* authority to sign | Initialize metadata from the same key that minted |
| `ExtensionNotFound` / `InvalidExtensionCombination` | Metadata initialized on a mint without the metadata pointer | Include `MetadataPointer` in the extension list |
| Transfer rejected by the hook | Destination wallet not whitelisted, stale proof, or a proof hashed with SHA3 instead of Keccak-256 | Rebuild the proof against the current root |
| Transfer fails inside the hook for want of the `Config` account | `ExtraAccountMetaList` not initialised for this mint, so Token-2022 cannot resolve the hook's extra accounts | Run `initialize_extra_account_meta_list` (step 5) |
| Fees never reach the treasury | Withheld fees stay in destination token accounts | Sweep them with the withdraw-withheld authority (Step 6) |
| `update_transfer_fee` fails | Signer is not the fee config authority | Sign with `params.fee_authority` |

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
