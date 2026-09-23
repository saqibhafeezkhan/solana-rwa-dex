# KYC Gate Program

An Anchor-based Solana program that acts as the **institutional bouncer** for the RWA DEX: it issues a **soulbound** (non-transferable) Token-2022 attestation to approved institutions, and exposes a read-only `verify_kyc` that pools CPI into before a swap.

Program ID: `7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1`

| Singleton | Address (bump) |
| :--- | :--- |
| `KycConfig` PDA | `AjGcFZbvBm5WKCixHRAnJjXsDtizDFyZsRHioyF4zigY` (253) |
| Attestation mint PDA | `BLutgEutFZqCcw6cFYryjNxmjs5ZRD8XjPLAxTbkC5oJ` (254) |

---

## 🌟 Overview

Real-world assets are not permissionless. An RWA pool has to be able to answer one question before every trade: *is the wallet on the other side an approved institution?* Off-chain KYC means nothing if the answer is not on-chain, and a mutable list somewhere else means a pool either trusts a second program's account layout or re-checks a cache that can go stale mid-block.

This program puts the answer in the institution's own wallet:

1. **The authority issues an attestation.** `initialize_kyc_mint` runs once, creating the attestation mint; `issue_kyc` mints exactly one token of it to an approved institution's token account.
2. **Token-2022 makes it soulbound.** The mint carries the `NonTransferable` extension, so the token program rejects every transfer of it — it cannot be sold, lent, or moved to a wallet the authority did not approve.
3. **Pools verify with accounts they already have.** Before a swap, a pool CPIs into `verify_kyc` with the trader and the trader's token account. If it holds the attestation the swap proceeds; if not, the CPI fails with `NotKycApproved` and the swap reverts with it.
4. **The authority can revoke.** `revoke_kyc` burns the attestation, and the institution is refused from that transaction on.
5. **The authority can be rotated.** `set_authority` moves the role onto a new key — a fresh multisig, say — without redeploying, and the institutions already holding attestations keep them.

### Key Characteristics

* **The attestation lives in the institution's wallet**, not in a list this program maintains. A pool reads it from an account a swap already carries — no extra PDA read per trade, and nothing to keep in sync.
* **Non-transferability is enforced by the token program**, not by this one. There is no code path — here or in any other program — that can move an attestation to an unapproved wallet.
* **Revocation needs no cooperation.** The mint's *permanent delegate* is the config PDA, so `revoke_kyc` can burn an attestation whose holder is no longer welcome (see *The Token-2022 rules that shape the design* below).
* **No key can mint.** The mint, freeze and permanent-delegate authority are the `KycConfig` PDA. Minting and burning happen only through `issue_kyc` / `revoke_kyc`, both gated on the configured authority.
* **One token per institution, exactly.** `issue_kyc` requires an empty account and `revoke_kyc` burns exactly one, so a revocation is always complete and the check "holds at least one" is a complete answer.
* **The role is transferable, the power is not.** `set_authority` hands the KYC role to a new key, but the mint's mint, freeze and permanent-delegate authorities remain the `KycConfig` PDA — so no key, old or new, can mint or burn outside the instructions.
* **`verify_kyc` is read-only and permissionless.** Anyone may ask whether a wallet is approved; the trader must be a signer, so a pool cannot clear a trade with someone else's attestation.
* **The mint is a PDA.** No keypair to deploy, no address to keep in a config file: any integrator derives it from `[b"kyc-mint"]` alone, and the pool's account list can be built from first principles.

---

## 🔒 Why a soulbound attestation instead of a whitelist PDA

A whitelist — a PDA this program writes `wallet → approved` into — is the obvious alternative. The token approach wins on three counts:

1. **Verification is a token account read, not a second lookup.** The pool passes the trader's account; the check is `mint`, `owner`, `amount`. A whitelist needs the trader's approval PDA too, derived and passed alongside, with its own seeds and staleness to reason about.
2. **The guarantee comes from the token program, not from a check this program performs.** A whitelist only protects a pool that remembers to read the list, and its contents are this program's state to keep correct — every rotation of the list is a write that some reader may have already passed. An attestation is an object the token program refuses to move, so a wallet can hold one only because the authority minted it, and a pool that sees one is looking at issuance itself rather than a copy of a decision.
3. **It composes with the rest of the token stack.** A pool that already handles Token-2022 accounts for the traded assets needs nothing new to read an attestation, and a compliance UI sees an institution's approval in the same place it sees its balances.

The `transfer_hook` program in this repo is the complementary half: it gates *who may receive an RWA token* (a merkle whitelist checked on every transfer). This program gates *who may trade on the DEX*. They are independent, and neither implies the other.

---

## ⚠️ The Token-2022 rules that shape the design

Three rules of Token-2022 drove decisions here that would otherwise look arbitrary. They are checked by the token program, not by this one, and each has a test:

1. **Burning is not the mint authority's job.** Token-2022 reserves `burn` for the account's owner, an approved delegate, or the **permanent delegate** — a mint authority can mint, but it cannot burn somebody else's tokens. So the mint carries `PermanentDelegate` pointing at the config PDA: without it, `revoke_kyc` would only work if the institution signed its own revocation.
2. **Non-transferability survives the delegate.** `NonTransferable` is checked on every transfer, so the permanent delegate can claw an attestation back but cannot move one anywhere.
3. **A non-transferable mint's accounts need extensions.** `process_mint_to` refuses to mint into an account that lacks `ImmutableOwner` **and** `NonTransferableAccount`. The token program writes both itself when the account goes through `InitializeAccount3` (or an ATA program recent enough to size accounts from the mint's required extensions) — a hand-made account without them is rejected — see [Onboarding an institution](#onboarding-an-institution).

Two more consequences worth knowing:

* **The mint's length is fixed at 206 bytes.** Token-2022 pads a mint's account to the base-*account* length and checks the total against `try_calculate_account_len::<Mint>(&extensions)` exactly, so the extension layout is not negotiable: base mint (82) · padding · account-type byte at 165 · `NonTransferable` TLV at 166 · `PermanentDelegate` TLV at 170 (4 + a 32-byte pubkey).
* **The account's required extensions mean 174 bytes** for an account of this mint: base (165) · account-type byte · `ImmutableOwner` TLV · `NonTransferableAccount` TLV.

---

## ⚙️ Program Constants

| Constant | Value | Purpose |
| :--- | :--- | :--- |
| `KYC_CONFIG_SEED` | `b"kyc-config"` | Seed of the config PDA (single, one config per deployment). |
| `KYC_MINT_SEED` | `b"kyc-mint"` | Seed of the attestation mint PDA. |
| `KYC_MINT_DECIMALS` | `0` | The attestation is a flag, not a currency. |
| `KYC_ATTESTATION_AMOUNT` | `1` | One token means approved. |
| `MINT_EXTENSIONS` | `[NonTransferable, PermanentDelegate]` | Soulbound and revocable, in TLV order. |

---

## 🔑 Account Architecture & PDAs

```
   KYC authority (a multisig in production)
            │
            │  initialize_kyc_mint · issue_kyc · revoke_kyc
            ▼
  ┌────────────────────────┐          ┌──────────────────────────────┐
  │    KycConfig (PDA)     │          │     KYC mint (PDA)           │
  │  [b"kyc-config"]       │  mint /  │  [b"kyc-mint"]               │
  │  authority, kyc_mint   │  freeze /│  Token-2022 mint, 206 bytes  │
  │  bump, mint_bump       │  delegate│  + NonTransferable           │
  └────────────────────────┘  ───────▶│  + PermanentDelegate         │
                                      └───────────────┬──────────────┘
                                                      │ 1 soulbound token
                                                      ▼
                          ┌────────────────────────────────────────────┐
                          │  institution's Token-2022 account          │
                          │  (ImmutableOwner, NonTransferableAccount)  │
                          └───────────────────┬────────────────────────┘
                                              │
   DEX pool ── CPI verify_kyc(config, trader, trader's account) ──┘
```

### PDA Seeds Table

| Account | PDA Seeds | Notes |
| :--- | :--- | :--- |
| `KycConfig` | `[b"kyc-config"]` | The authority, the mint, and both cached bumps. |
| KYC mint | `[b"kyc-mint"]` | Token-2022 mint owned by the token program; created by CPI to the system program with the mint PDA signing. |

All integer seeds are little-endian; both PDAs are singletons, since one deployment guards one DEX.

---

## 🛠️ Instructions API

### 1. `initialize_kyc_mint`

```rust
pub fn initialize_kyc_mint(ctx: Context<InitializeKycMint>) -> Result<()>
```

Creates the config and the attestation mint. One-time; a second call fails on the config PDA.

Restricted to the program's **upgrade authority**, which is read out of this program's `ProgramData` account (address-pinned to `[program]` under the BPF upgradeable loader, owner-pinned to the loader) and matched against the signer (`UnauthorizedInitializer`). The gate decides who may trade, so it must not be the first caller to find the program id who decides who runs it.

* **Accounts**: `kyc_config` (init, PDA, payer = `authority`), `kyc_mint` (PDA, created here), `authority` (signer, payer, must be the program's upgrade authority), `program_data` (this program's `ProgramData` PDA, read-only), `token_program` (Token-2022), `system_program`.
* **Effects**: creates the mint account (206 bytes, rent-exempt, owned by Token-2022 — the mint PDA signs the `create_account`), initializes `NonTransferable`, sets the config PDA as permanent delegate, initializes the base mint with zero decimals and the config PDA as mint *and* freeze authority, and stores `{ authority, kyc_mint, bump, mint_bump }`.
* **Note**: every byte of token state is written by the Token-2022 program through CPIs; this program cannot write the mint's data, because the account belongs to the token program.

### 2. `issue_kyc`

```rust
pub fn issue_kyc(ctx: Context<IssueKyc>) -> Result<()>
```

Issues one attestation to an approved institution.

* **Accounts**: `kyc_config` (PDA, read-only), `kyc_mint` (must equal `config.kyc_mint`), `institution_kyc_account` (writable, must be a Token-2022 account of the mint), `authority` (signer, must equal `config.authority`), `token_program`.
* **Checks**: the authority is the configured one (`Unauthorized`), and the destination account is empty (`AlreadyApproved`) — the one-token-per-institution invariant.
* **Effect**: mints exactly one token, signed by the config PDA.

### 3. `revoke_kyc`

```rust
pub fn revoke_kyc(ctx: Context<RevokeKyc>) -> Result<()>
```

Burns the attestation. The institution does not sign.

* **Accounts**: as `issue_kyc`.
* **Checks**: authority (`Unauthorized`), and the account actually holds one (`NotKycApproved`).
* **Effect**: burns one token as the mint's permanent delegate. The token account is left in place, empty.

### 4. `verify_kyc`

```rust
pub fn verify_kyc(ctx: Context<VerifyKyc>) -> Result<()>
```

The check a pool calls before a swap. Read-only, permissionless, and cheap.

* **Accounts**: `kyc_config` (PDA, read-only), `user` (signer, the trader — the wallet being checked), `user_kyc_account` (read-only).
* **Checks**, in order: the account belongs to the KYC mint (`WrongMint`), it belongs to `user` (`WrongOwner`), and it holds at least one attestation (`NotKycApproved`).
* **Note**: `user` must be a signer, which is what stops a pool from clearing a trade with a third party's attestation.

### 5. `set_authority`

```rust
pub fn set_authority(ctx: Context<SetAuthority>, new_authority: Pubkey) -> Result<()>
```

Rotates the KYC authority onto a new key, so the gate does not have to be redeployed to move the role onto a new multisig (or its vault).

* **Accounts**: `kyc_config` (writable), `authority` (signer, must be the *current* authority).
* **Parameters**: `new_authority` — a plain key, deliberately **not** a signer, so it can be a multisig vault PDA that cannot sign a transaction of its own. Verify the address off-chain before sending: the incoming key need not consent, and a typo hands the gate to a key nobody controls.
* **Checks**: the signer is the current authority (`Unauthorized`), and the new key is not the default pubkey (`InvalidAuthority`).
* **Effect**: writes `config.authority`. Nothing else changes — the mint's authorities stay the `KycConfig` PDA, so the incoming authority has exactly the outgoing one's power, and attestations already issued remain valid (the institutions holding them need not sign or be re-issued).

---

## 📄 Data Structures

### `KycConfig` — 74 bytes

```rust
pub struct KycConfig {
    pub authority: Pubkey,   // the only key that may issue or revoke
    pub kyc_mint: Pubkey,    // the soulbound attestation mint
    pub bump: u8,            // this PDA's bump (used to sign the CPIs)
    pub mint_bump: u8,       // the mint PDA's bump, published for integrators
}
```

### The attestation account

Not this program's account — it is a Token-2022 account of the KYC mint, owned by the institution, carrying `ImmutableOwner` and `NonTransferableAccount` (174 bytes). Pools read three fields of it: `mint`, `owner`, `amount`.

---

## 🔌 TypeScript Integration

Snippets use `@coral-xyz/anchor` with the IDL `anchor build` writes to `target/types/kyc_gate.ts`, plus `@solana/web3.js` and `@solana/spl-token`.

### PDAs and helpers

```typescript
import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_2022_PROGRAM_ID,
  getOrCreateAssociatedTokenAccount,
  getAccount,
} from "@solana/spl-token";
import { KycGate } from "./target/types/kyc_gate";

const provider = anchor.AnchorProvider.env();
anchor.setProvider(provider);
const program = anchor.workspace.KycGate as Program<KycGate>;

const KYC_CONFIG_SEED = Buffer.from("kyc-config");
const KYC_MINT_SEED = Buffer.from("kyc-mint");

const [kycConfig] = PublicKey.findProgramAddressSync([KYC_CONFIG_SEED], program.programId);
const [kycMint] = PublicKey.findProgramAddressSync([KYC_MINT_SEED], program.programId);
```

### Initialize (once)

```typescript
// `authority` must be the program's upgrade authority, and the wallet that
// signs this transaction is the one the check reads.
async function initializeKycMint(authority: PublicKey) {
  await program.methods
    .initializeKycMint()
    .accounts({
      kycConfig,
      kycMint,
      authority,
      programData: programDataPda, // [program] under BPFLoaderUpgradeab1e...
      tokenProgram: TOKEN_2022_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
}
```

### Onboarding an institution

The institution's token account must be a Token-2022 account carrying `ImmutableOwner` and `NonTransferableAccount`, which the token program writes when the account is initialized through `InitializeAccount3`. Letting the ATA program create it is the usual way — but **check your ATA program version**: it must be one that sizes the account from the mint's required extensions. `spl_associated_token_account` 1.1.1 (the version LiteSVM bundles, and the one an older client carries) sizes only the base account, so issuing against its ATAs fails with the token program's `NonTransferableNeedsImmutableOwnership`. If you cannot count on a newer ATA program, create the account yourself: `createAccount` with `174` bytes of space owned by Token-2022, then `createInitializeAccount3Instruction(account, kycMint, institution, TOKEN_2022_PROGRAM_ID)`.

```typescript
async function onboardInstitution(
  authority: anchor.Wallet,
  institution: PublicKey
) {
  // The mint's ATA, with the extensions the mint requires.
  const { address: institutionKycAccount } = await getOrCreateAssociatedTokenAccount(
    provider.connection,
    authority.payer,   // payer
    kycMint,
    institution,
    false,
    undefined,
    undefined,
    TOKEN_2022_PROGRAM_ID
  );

  await program.methods
    .issueKyc()
    .accounts({
      kycConfig,
      kycMint,
      institutionKycAccount,
      authority: authority.publicKey,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();

  return institutionKycAccount;
}

async function revokeKyc(authority: PublicKey, institutionKycAccount: PublicKey) {
  await program.methods
    .revokeKyc()
    .accounts({
      kycConfig,
      kycMint,
      institutionKycAccount,
      authority,
      tokenProgram: TOKEN_2022_PROGRAM_ID,
    })
    .rpc();
}
```

### Rotating the authority

```typescript
async function rotateAuthority(authority: PublicKey, newAuthority: PublicKey) {
  // `newAuthority` is an argument, not an account, so it does not sign and may be
  // a multisig vault PDA. Log it before sending: the outgoing authority signs
  // away the role, and nothing hands it back.
  await program.methods
    .setAuthority(newAuthority)
    .accounts({ kycConfig, authority })
    .rpc();
}
```

### Pre-checking a wallet in a UI

A client that wants the same answer the pool will get — without sending a transaction — only needs the account:

```typescript
async function isApproved(
  connection: anchor.web3.Connection,
  address: PublicKey
): Promise<boolean> {
  try {
    const account = await getAccount(
      connection,
      address,
      undefined,
      TOKEN_2022_PROGRAM_ID
    );
    return account.mint.equals(kycMint) && account.amount >= 1n;
  } catch {
    return false; // no such account, or not a Token-2022 account
  }
}
```

---

## 🦀 DEX Integration

The pool-side pattern, before pricing a swap (the `kyc_gate` dependency needs `features = ["cpi"]`):

```rust
let cpi_accounts = kyc_gate::cpi::accounts::VerifyKyc {
    kyc_config: ctx.accounts.kyc_config.to_account_info(),
    user: ctx.accounts.trader.to_account_info(),
    user_kyc_account: ctx.accounts.trader_kyc_account.to_account_info(),
};
let cpi_ctx = CpiContext::new(ctx.accounts.kyc_gate_program.key(), cpi_accounts);
kyc_gate::cpi::verify_kyc(cpi_ctx)?; // fails => the swap reverts
```

`verify_kyc` writes nothing, so a pool can call it as the first thing it does and let a failing CPI revert the whole swap.

---

## 🚨 Error Codes

| Error Code | Error Message | Description |
| :--- | :--- | :--- |
| `Unauthorized` (6000) | `Unauthorized to issue or revoke KYC` | The signer is not `KycConfig::authority`. |
| `NotKycApproved` (6001) | `User is not KYC approved` | The account holds no attestation — or `revoke_kyc` was called on an empty account. |
| `WrongMint` (6002) | `Token account does not belong to the KYC mint` | The account's mint is not the attestation mint (or a mint/burn was handed the wrong mint account). |
| `WrongOwner` (6003) | `Token account does not belong to the user` | The attestation belongs to some other wallet. |
| `AlreadyApproved` (6004) | `Institution already holds a KYC attestation` | `issue_kyc` on an account that is not empty. |
| `InvalidAuthority` (6005) | `The new authority cannot be the default pubkey` | `set_authority` aimed at key zero, which would freeze the role with no way to take it back. |
| `UnauthorizedInitializer` (6006) | `Only the program's upgrade authority may initialize the KYC gate` | `initialize_kyc_mint` signed by a key that is not the program's upgrade authority — including the case where no authority is recorded at all. |

Failures raised by the *token* program are reported with its own codes — `NonTransferableNeedsImmutableOwnership`, for instance, when someone tries to mint into an account that lacks the required extensions. (The token program's own source calls that name "not totally correct", since the same check also requires `NonTransferableAccount`; it is what to match on in a client, whichever extension is missing.)

---

## 🧪 Testing & Verification

```bash
cargo test -p kyc_gate     # 6 unit tests, no validator needed
anchor build -p kyc_gate   # SBF build (the e2e suite loads target/deploy/kyc_gate.so)
(cd tests/e2e && cargo test --test kyc_gate)   # 9 end-to-end tests
```

**Unit tests** cover the pieces that are pure: the attestation policy in all four outcomes (approved, wrong mint, wrong owner, empty), and both account sizes — the config's `LEN` against what it serializes, and the mint's required length against `try_calculate_account_len`.

**End-to-end tests** run the built program in a LiteSVM against the real Token-2022 image (LiteSVM bundles `spl_token_2022-11.0.0`, the same build the cluster runs) and real signed transactions. They pin down what unit tests cannot reach:

* the mint is created at the length the token program demands, with both extensions in TLV order, and the config PDA — not a wallet — as mint, freeze and permanent-delegate authority;
* an account created through `InitializeAccount3` really does come out carrying the two extensions the mint requires, and minting into a bare account really is refused by the token program;
* the gate can only be created by the program's upgrade authority: the first test boots the program with no upgrade authority on record and has a funded stranger refused, then records one and shows the stranger still refused and only the recorded key accepted;
* only the configured authority can issue or revoke, an impostor is refused before the token program is even called;
* an attestation is binary: one token, a second issue refused, a revoke that empties the account, a second revoke refused;
* the authority can be rotated, the refusal is total for the outgoing key (`Unauthorized` on both `issue_kyc` and `revoke_kyc`), the incoming one gains both powers, and an attestation issued before the rotation still verifies after it;
* `verify_kyc` accepts exactly the wallet that holds an attestation of the right mint and reports *why* it refuses otherwise, and the trader's signature is required;
* the attestation cannot be transferred — the property the design rests on — with the refusal coming from the token program, not from this one.

---

## ⚠️ Design Notes & Limits

1. **Nothing calls it yet.** The pools in this repo (`clmm`, `twamm`) do not CPI `verify_kyc`; wiring it in is the integration step that makes the gate real.
2. **One mint, one authority, per deployment.** The PDAs are singletons and the config is created once. Two DEX deployments that want separate KYC sets need separate program deployments (or a per-pool seed scheme).
3. **Rotation is single-step, with no handshake.** `set_authority` takes effect the moment the current authority signs it; the incoming key is never asked to accept. That is what allows a vault PDA (which cannot sign) to be the successor, but it also means a wrong-but-valid address is unrecoverable — there is no second authority to undo it, and the default pubkey is the only value rejected. Two-step handshakes, or a governance delay, would need more state than `KycConfig` holds.
4. **The authority is trusted absolutely.** It can issue to anyone and revoke anyone. That is the point of a gate, but it is worth saying: there is no expiry, no jurisdiction, no per-institution limit, and no on-chain appeal. Structured claims (expiry, tier, jurisdiction) would want a richer state account than an attestation token can hold.
5. **A wallet, not a legal entity.** The attestation is held by a key. A compromised institution key is a compromised attestation until the authority revokes it — which is exactly what `revoke_kyc` is for.
6. **Freeze authority is reserved, and unused.** The mint's freeze authority is the config PDA, but no instruction freezes. Revocation burns; freezing would leave the attestation in place at amount 1, which any check that reads only the balance would misread.
7. **The attestation account must be a Token-2022 account of the mint** with the extensions above. Legacy SPL accounts cannot hold it, and neither can an account created before the mint existed.
8. **`verify_kyc` answers one question.** It does not check that the trader's *other* accounts are clean, that the pool's counterparty is the same wallet, or anything about the traded asset — a pool composes it with whatever else it enforces. The RWA mint's own `transfer_hook` handles who may hold the asset.
9. **No fee, no funds, no upgrade path inside the program.** The authority can only issue and revoke; the program never moves value of its own.
