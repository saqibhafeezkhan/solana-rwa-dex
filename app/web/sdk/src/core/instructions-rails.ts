/**
 * Instruction builders for the compliance rails (settlement clearinghouse,
 * KYC gate, oracle) and the token layer (RWA mint factory, transfer hook,
 * hooks escrow program).
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  RWA_MINT_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  SYSTEM_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TRANSFER_HOOK_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  kycMint,
  oracleState,
  programData,
  settlementConfig,
  settlementEscrowAuthority,
  settlementEscrowToken,
  settlementMintAgent,
  settlementVault,
  transferHookConfig,
  transferHookExtraAccountMetaList,
  transferHookWhitelistEntry,
} from './pdas'

type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

function instruction(
  programId: PublicKey,
  name: string,
  keys: AccountKey[],
  encode?: (w: BorshWriter) => void,
): Promise<TransactionInstruction> {
  return encodeInstruction(name, encode).then(
    (data) => new TransactionInstruction({ programId, keys, data: Buffer.from(data) }),
  )
}

// =======================================================================
// Settlement program
// =======================================================================

/** One-time setup, restricted to the program's upgrade authority. */
export async function settlementInitialize(authority: PublicKey, poolsRequireGate: boolean) {
  const keys: AccountKey[] = [
    { pubkey: settlementConfig(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: programData(SETTLEMENT_PROGRAM_ID), isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'initialize', keys, (w) => w.bool(poolsRequireGate))
}

export async function settlementSetHooksAuthority(authority: PublicKey, hooksAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: settlementConfig(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'set_hooks_authority', keys, (w) =>
    w.raw(hooksAuthority.toBytes()),
  )
}

/** The uncompressed 64-byte secp256k1 public key (x || y, no 0x04 prefix). */
export async function settlementSetTransferAgent(authority: PublicKey, secp256k1Pubkey: Uint8Array) {
  if (secp256k1Pubkey.length !== 64) {
    throw new Error('the transfer agent key is the uncompressed 64-byte form (x || y)')
  }
  const keys: AccountKey[] = [
    { pubkey: settlementConfig(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'set_transfer_agent', keys, (w) => w.raw(secp256k1Pubkey))
}

export interface SettlementInitiateArgs {
  tradeId: bigint
  /**
   * The **absolute** slot the escrow unlocks at — not a horizon. The program
   * only checks `settlement_slot > clock.slot`, so a T+N caller has to resolve
   * `currentSlot + N` itself before calling. `SettlementVault.settlement_slot`
   * stores this value verbatim, which is what the UI counts down to.
   */
  settlementSlots: bigint
  /** The exact flow to escrow; the program checks it against the source balance. */
  amount: bigint
  /** The account moving the tokens; must be owned by `authority`. */
  authority: PublicKey
  sourceTokenAccount: PublicKey
  tokenMint: PublicKey
  /** The eventual recipient; bound into the Transfer Agent's signed message. */
  taker: PublicKey
  /**
   * The AMM pool PDA on the pool path (authority = pool PDA, only from a
   * wired AMM's CPI), or null on the standalone path (a wallet signs).
   */
  pool: PublicKey | null
  tokenProgramId: PublicKey
  /**
   * The mint's transfer-hook accounts, when the mint is hook-gated (an RWA
   * mint): Token-2022 resolves the mint's meta list against the transfer
   * instruction's own account list and rejects the move without them. Here the
   * escrow is the recipient, so that side's entry is the escrow authority's own
   * — one entry, whatever the trade ID. See `hookTransferAccounts`.
   */
  hookAccounts?: AccountKey[]
}

export async function settlementInitiate(args: SettlementInitiateArgs) {
  const vault = settlementVault(args.tradeId)
  const vaultToken = settlementEscrowToken(args.tradeId)
  const keys: AccountKey[] = [
    { pubkey: vault, isSigner: false, isWritable: true },
    { pubkey: args.sourceTokenAccount, isSigner: false, isWritable: true },
    { pubkey: vaultToken, isSigner: false, isWritable: true },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: args.taker, isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: true },
    // `pool` is an Option: the settlement program's own ID is the None sentinel.
    { pubkey: args.pool ?? SETTLEMENT_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
    ...(args.hookAccounts ?? []),
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'initiate_settlement', keys, (w) => {
    w.u64(args.tradeId)
    w.u64(args.settlementSlots)
    w.u64(args.amount)
  })
}

/**
 * Releases the escrow. No Solana signer authorizes it — the recovered
 * secp256k1 signature does. `signature` is 64 bytes (r || s).
 *
 * `hookAccounts` carries the mint's transfer-hook accounts when the escrowed
 * mint is hook-gated (Token-2022 refuses the move without them); the escrow is
 * the sender here, so that side's entry is the escrow authority's own — the same
 * entry for every trade. See `hookTransferAccounts`.
 */
export async function settlementSettle(
  tradeId: bigint,
  tokenMint: PublicKey,
  takerTokenAccount: PublicKey,
  signature: Uint8Array,
  recoveryId: number,
  tokenProgramId: PublicKey,
  hookAccounts: AccountKey[] = [],
  /**
   * The mint's `[b"agent", mint]` override, when one is installed. Leave null
   * to verify against the config's global key; the settlement program ID is
   * passed as Anchor's `Option` None sentinel.
   */
  mintAgent: PublicKey | null = null,
) {
  if (signature.length !== 64) throw new Error('the secp256k1 signature is 64 bytes (r || s)')
  const vault = settlementVault(tradeId)
  const keys: AccountKey[] = [
    { pubkey: vault, isSigner: false, isWritable: true },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: mintAgent ?? SETTLEMENT_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: tokenMint, isSigner: false, isWritable: false },
    { pubkey: settlementEscrowToken(tradeId), isSigner: false, isWritable: true },
    // The escrow's owner: it signs the release inside the program, not here.
    { pubkey: settlementEscrowAuthority(), isSigner: false, isWritable: false },
    { pubkey: takerTokenAccount, isSigner: false, isWritable: true },
    { pubkey: tokenProgramId, isSigner: false, isWritable: false },
    ...hookAccounts,
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'settle', keys, (w) => {
    w.raw(signature)
    w.u8(recoveryId)
  })
}

export interface SettlementCancelArgs {
  tradeId: bigint
  tokenMint: PublicKey
  /** The config authority (standalone path). The pool path needs the pool PDA to sign. */
  authority: PublicKey
  /**
   * Where the escrow is refunded. Normally `vault.refund_source` — the account
   * that funded it — but on a settlement-routed trade the funder is the hooks
   * program's custody ATA, which nothing can pay out of, so the refund goes to
   * the vault's recorded taker instead: pass a token account of the escrow mint
   * owned by `vault.taker` in that case. Both destinations are decided by vault
   * state, so the canceller still cannot pick an arbitrary account.
   */
  refundTokenAccount: PublicKey
  tokenProgramId: PublicKey
  /** The mint's transfer-hook accounts, when the mint is hook-gated. */
  hookAccounts?: AccountKey[]
}

/** Compliance exit: returns the full escrow to the refund account. */
export async function settlementCancel(args: SettlementCancelArgs) {
  const vault = settlementVault(args.tradeId)
  const keys: AccountKey[] = [
    { pubkey: vault, isSigner: false, isWritable: true },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: SETTLEMENT_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: settlementEscrowToken(args.tradeId), isSigner: false, isWritable: true },
    { pubkey: settlementEscrowAuthority(), isSigner: false, isWritable: false },
    { pubkey: args.refundTokenAccount, isSigner: false, isWritable: true },
    { pubkey: args.authority, isSigner: true, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    ...(args.hookAccounts ?? []),
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'cancel_settlement', keys, () => {})
}

/**
 * Permissionless auto-refund once the T+N window has lapsed
 * (`clock.slot >= vault.expiry_slot`). The destination is derived on chain from
 * vault state — the funder, or the recorded taker on a settlement-routed trade —
 * so anyone may crank it but no one can redirect the escrow.
 *
 * The whole escrow balance is swept, not just the recorded amount, so a donated
 * unit cannot leave the account non-empty and block `close_settlement`.
 */
export async function settlementRefundExpired(args: {
  tradeId: bigint
  tokenMint: PublicKey
  refundTokenAccount: PublicKey
  tokenProgramId: PublicKey
  /** The mint's transfer-hook accounts, when the mint is hook-gated. */
  hookAccounts?: AccountKey[]
}) {
  const keys: AccountKey[] = [
    { pubkey: settlementVault(args.tradeId), isSigner: false, isWritable: true },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: settlementEscrowToken(args.tradeId), isSigner: false, isWritable: true },
    { pubkey: settlementEscrowAuthority(), isSigner: false, isWritable: false },
    { pubkey: args.refundTokenAccount, isSigner: false, isWritable: true },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    ...(args.hookAccounts ?? []),
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'refund_expired', keys, () => {})
}

/**
 * Reclaims the rent of a terminal trade: closes the empty escrow token account
 * and the vault PDA, both paying the config authority. Config authority only.
 */
export async function settlementClose(args: {
  tradeId: bigint
  tokenMint: PublicKey
  authority: PublicKey
  tokenProgramId: PublicKey
  /**
   * The config authority's ATA for the mint, which receives any residual
   * balance a terminal escrow still holds (a donation; `settle` moves exactly
   * the signed amount and refunds sweep the rest). It is pinned on chain to
   * that ATA, so it must exist before the close is sent.
   */
  residualAccount: PublicKey
}) {
  const keys: AccountKey[] = [
    { pubkey: settlementVault(args.tradeId), isSigner: false, isWritable: true },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: true },
    // Writable: a fee-bearing mint is credited the escrow's withheld fees
    // before the account is closed.
    { pubkey: args.tokenMint, isSigner: false, isWritable: true },
    { pubkey: settlementEscrowToken(args.tradeId), isSigner: false, isWritable: true },
    { pubkey: settlementEscrowAuthority(), isSigner: false, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    { pubkey: args.residualAccount, isSigner: false, isWritable: true },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'close_settlement', keys, () => {})
}

/** Installs (or updates) a mint's own Transfer Agent key. Config authority only. */
export async function settlementSetMintAgent(args: {
  authority: PublicKey
  tokenMint: PublicKey
  /** The uncompressed 64-byte secp256k1 key (x || y, no 0x04 prefix). */
  secp256k1Pubkey: Uint8Array
}) {
  if (args.secp256k1Pubkey.length !== 64) {
    throw new Error('the transfer agent key is the uncompressed 64-byte form (x || y)')
  }
  const keys: AccountKey[] = [
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: true },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: settlementMintAgent(args.tokenMint), isSigner: false, isWritable: true },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'set_mint_agent', keys, (w) =>
    w.raw(args.secp256k1Pubkey),
  )
}

/** Removes a mint's override, reverting it to the config's global key. */
export async function settlementClearMintAgent(args: {
  authority: PublicKey
  tokenMint: PublicKey
}) {
  const keys: AccountKey[] = [
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: true },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: settlementMintAgent(args.tokenMint), isSigner: false, isWritable: true },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'clear_mint_agent', keys, () => {})
}

// =======================================================================
// KYC gate program
// =======================================================================

/** One-time setup, restricted to the program's upgrade authority. */
export async function kycInitializeMint(authority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: kycConfig(), isSigner: false, isWritable: true },
    { pubkey: kycMint(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: programData(KYC_PROGRAM_ID), isSigner: false, isWritable: false },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(KYC_PROGRAM_ID, 'initialize_kyc_mint', keys, () => {})
}

/** Mints one soulbound attestation to the institution. */
export async function kycIssue(authority: PublicKey, institution: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: kycConfig(), isSigner: false, isWritable: false },
    { pubkey: kycMint(), isSigner: false, isWritable: true },
    { pubkey: ata(institution, kycMint(), TOKEN_2022_PROGRAM_ID), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(KYC_PROGRAM_ID, 'issue_kyc', keys, () => {})
}

/** Burns the attestation; the institution does not sign. */
export async function kycRevoke(authority: PublicKey, institution: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: kycConfig(), isSigner: false, isWritable: false },
    { pubkey: kycMint(), isSigner: false, isWritable: true },
    { pubkey: ata(institution, kycMint(), TOKEN_2022_PROGRAM_ID), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(KYC_PROGRAM_ID, 'revoke_kyc', keys, () => {})
}

/**
 * The gate's own check, exactly as a pool runs it before a swap: the wallet
 * signs, the config and its own attestation account are passed read-only. It
 * succeeds only when the attestation belongs to the KYC mint, belongs to this
 * wallet and holds at least one token — so a successful transaction here is
 * proof the KYC-wired pools would admit this wallet, not just that a balance
 * read looked non-zero.
 */
export async function kycVerify(user: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: kycConfig(), isSigner: false, isWritable: false },
    { pubkey: user, isSigner: true, isWritable: false },
    { pubkey: ata(user, kycMint(), TOKEN_2022_PROGRAM_ID), isSigner: false, isWritable: false },
  ]
  return instruction(KYC_PROGRAM_ID, 'verify_kyc', keys, () => {})
}

/** Rotates the issuing authority. Verify `newAuthority` off-chain first. */
export async function kycSetAuthority(currentAuthority: PublicKey, newAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: kycConfig(), isSigner: false, isWritable: true },
    { pubkey: currentAuthority, isSigner: true, isWritable: false },
  ]
  return instruction(KYC_PROGRAM_ID, 'set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}

// =======================================================================
// Oracle program (unpause; the pools drive every other instruction by CPI)
// =======================================================================

/** Owner-only reset after a circuit-breaker trip. Deliberately manual. */
export async function oracleUnpausePool(pool: PublicKey, owner: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: oracleState(pool), isSigner: false, isWritable: true },
    { pubkey: owner, isSigner: true, isWritable: false },
  ]
  return instruction(ORACLE_PROGRAM_ID, 'unpause_pool', keys, () => {})
}

// =======================================================================
// RWA mint factory program (Token-2022 + hook + metadata + transfer fee)
// =======================================================================

export interface RwaMintParamsInput {
  decimals: number
  transferHookProgram: PublicKey
  delegate: PublicKey
  feeAuthority: PublicKey
  name: string
  symbol: string
  uri: string
}

export async function rwaCreateMint(
  mintKeypair: PublicKey,
  authority: PublicKey,
  params: RwaMintParamsInput,
) {
  const keys: AccountKey[] = [
    { pubkey: mintKeypair, isSigner: true, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(RWA_MINT_PROGRAM_ID, 'create_rwa_mint', keys, (w) => {
    w.u8(params.decimals)
    w.raw(params.transferHookProgram.toBytes())
    w.raw(params.delegate.toBytes())
    w.raw(params.feeAuthority.toBytes())
    w.string(params.name)
    w.string(params.symbol)
    w.string(params.uri)
  })
}

export async function rwaMintTo(
  mint: PublicKey,
  destination: PublicKey,
  authority: PublicKey,
  amount: bigint,
) {
  const keys: AccountKey[] = [
    { pubkey: mint, isSigner: false, isWritable: true },
    { pubkey: destination, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(RWA_MINT_PROGRAM_ID, 'mint_to', keys, (w) => w.u64(amount))
}

export interface RwaTransferArgs {
  source: PublicKey
  /** Owner of `source` — its whitelist entry is derived from this wallet. */
  sourceOwner: PublicKey
  destination: PublicKey
  /** Owner of `destination` — likewise. */
  destinationOwner: PublicKey
  mint: PublicKey
  authority: PublicKey
  amount: bigint
}

/**
 * Token-2022 transfer whose hook accounts carry both Merkle proofs; the hook
 * checks source AND destination eligibility.
 *
 * The proofs are **not** instruction data. Token-2022 builds the hook's data
 * itself (`[discriminator][amount]`) and forwards no trailing bytes, so appending
 * proofs to a `transfer_checked` payload makes the token program reject the whole
 * transfer. Each side's proof travels in that institution's whitelist entry PDA
 * instead, which the mint's extra-account-metas list tells Token-2022 to derive
 * from the token accounts' owners — so the two entry accounts below have to be in
 * the transfer, and Token-2022 checks each one really is the entry for the wallet
 * it is spending from or paying to.
 */
/**
 * The accounts a Token-2022 transfer of a **hook-gated** mint has to carry, in
 * the order Token-2022 resolves them:
 *
 * 1. the hook program itself — Token-2022 CPI's into it, and the callee's
 *    program account has to be in the transfer's account list for that call to
 *    resolve (without it the failure is "Unknown program", from inside the
 *    token program),
 * 2. the mint's `extra-account-metas` PDA,
 * 3. the sender's whitelist entry (`[b"whitelist", sourceOwner]`),
 * 4. the recipient's whitelist entry,
 * 5. the hook config.
 *
 * Token-2022 re-derives 3 and 4 from each token account's own `owner` field, so
 * the addresses have to be the entries for the wallets that are actually sending
 * and receiving — a caller cannot substitute someone else's entry. Both sides
 * are checked by the hook, which is why the **escrow** account's owner needs an
 * entry of its own when a settlement move of an RWA mint is built.
 */
export function hookTransferAccounts(
  mint: PublicKey,
  sourceOwner: PublicKey,
  destinationOwner: PublicKey,
): AccountKey[] {
  return [
    { pubkey: TRANSFER_HOOK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: transferHookExtraAccountMetaList(mint), isSigner: false, isWritable: false },
    { pubkey: transferHookWhitelistEntry(sourceOwner), isSigner: false, isWritable: false },
    { pubkey: transferHookWhitelistEntry(destinationOwner), isSigner: false, isWritable: false },
    { pubkey: transferHookConfig(), isSigner: false, isWritable: false },
  ]
}

export async function rwaTransfer(args: RwaTransferArgs) {
  const keys: AccountKey[] = [
    { pubkey: args.source, isSigner: false, isWritable: true },
    { pubkey: args.destination, isSigner: false, isWritable: true },
    { pubkey: args.mint, isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: false },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
    ...hookTransferAccounts(args.mint, args.sourceOwner, args.destinationOwner),
  ]
  return instruction(RWA_MINT_PROGRAM_ID, 'transfer', keys, (w) => {
    w.u64(args.amount)
  })
}

export async function rwaUpdateTransferFee(
  mint: PublicKey,
  feeAuthority: PublicKey,
  newFeeBasisPoints: number,
  newMaximumFee: bigint,
) {
  const keys: AccountKey[] = [
    { pubkey: mint, isSigner: false, isWritable: true },
    { pubkey: feeAuthority, isSigner: true, isWritable: false },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(RWA_MINT_PROGRAM_ID, 'update_transfer_fee', keys, (w) => {
    w.u16(newFeeBasisPoints)
    w.u64(newMaximumFee)
  })
}

// =======================================================================
// Transfer hook program (whitelist governance)
// =======================================================================

/** One-time setup, restricted to the program's upgrade authority. */
export async function transferHookInitialize(authority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: transferHookConfig(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: programData(TRANSFER_HOOK_PROGRAM_ID), isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'initialize', keys, () => {})
}

/** Publishes a new whitelist Merkle root; all-zeros is refused (use pause). */
export async function transferHookUpdateRoot(authority: PublicKey, merkleRoot: Uint8Array) {
  if (merkleRoot.length !== 32) throw new Error('the merkle root is 32 bytes')
  const keys: AccountKey[] = [
    { pubkey: transferHookConfig(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'update_root', keys, (w) => w.raw(merkleRoot))
}

/**
 * Creates or refreshes an institution's whitelist entry at `[b"whitelist", owner]`.
 *
 * The program checks the proof against the published root before storing it, so a
 * path from a rotated tree is refused rather than written. Re-running this is how
 * a rotation is applied: a new root leaves every entry in place, and each one has
 * to be refreshed with the path that folds to the new root.
 */
export async function transferHookSetWhitelistEntry(
  authority: PublicKey,
  owner: PublicKey,
  proof: Uint8Array[],
) {
  const keys: AccountKey[] = [
    { pubkey: transferHookConfig(), isSigner: false, isWritable: false },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: owner, isSigner: false, isWritable: false },
    { pubkey: transferHookWhitelistEntry(owner), isSigner: false, isWritable: true },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'set_whitelist_entry', keys, (w) =>
    w.proofArray(proof),
  )
}

/** Delists an institution: closes its entry and refunds the rent to the authority. */
export async function transferHookCloseWhitelistEntry(authority: PublicKey, owner: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: transferHookConfig(), isSigner: false, isWritable: false },
    { pubkey: authority, isSigner: true, isWritable: true },
    { pubkey: owner, isSigner: false, isWritable: false },
    { pubkey: transferHookWhitelistEntry(owner), isSigner: false, isWritable: true },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'close_whitelist_entry', keys, () => {})
}

export async function transferHookSetPaused(authority: PublicKey, paused: boolean) {
  const keys: AccountKey[] = [
    { pubkey: transferHookConfig(), isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'set_paused', keys, (w) => w.bool(paused))
}

export async function transferHookSetAuthority(currentAuthority: PublicKey, newAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: transferHookConfig(), isSigner: false, isWritable: true },
    { pubkey: currentAuthority, isSigner: true, isWritable: false },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}

/** Registers a mint's extra-account-metas so Token-2022 resolves the hook. */
export async function transferHookInitExtraAccountMetaList(payer: PublicKey, rwaMint: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: payer, isSigner: true, isWritable: true },
    { pubkey: transferHookExtraAccountMetaList(rwaMint), isSigner: false, isWritable: true },
    { pubkey: rwaMint, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(TRANSFER_HOOK_PROGRAM_ID, 'initialize_extra_account_meta_list', keys, () => {})
}

// =======================================================================
// Hooks program (escrow router)
// =======================================================================

/** Tops up the hook authority PDA with system lamports. Permissionless. */
export async function hooksFundHookAuthority(payer: PublicKey, lamports: bigint) {
  const keys: AccountKey[] = [
    { pubkey: payer, isSigner: true, isWritable: true },
    { pubkey: hookAuthority(), isSigner: false, isWritable: true },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(HOOKS_PROGRAM_ID, 'fund_hook_authority', keys, (w) => w.u64(lamports))
}

/**
 * `after_swap` is invoked by the AMM pools through CPI, not called directly
 * from a wallet; it is exposed here for tooling/debugging only. Its take is
 * `(trade_id, settlement_slots, amount)`: the second is the T+N horizon in
 * slots (the program adds it to the live slot), the third is the exact flow to
 * escrow, which cannot exceed what the custody account holds.
 */
export interface HooksAfterSwapArgs {
  tradeId: bigint
  /** The T+N horizon in slots, measured from the slot the swap lands in. */
  settlementSlots: bigint
  /** The swap's output, as the AMM just paid it into the custody account. */
  amount: bigint
  taker: PublicKey
  /** The hook authority's custody ATA holding this trade's output. */
  custodyTokenAccount: PublicKey
  tokenMint: PublicKey
  tokenProgramId: PublicKey
  /**
   * The mint's transfer-hook accounts, when the escrowed mint is hook-gated:
   * the hook program, its meta list, the custody account's and the escrow
   * authority's whitelist entries, and the hook config. See
   * `hookTransferAccounts`.
   */
  hookAccounts?: AccountKey[]
}

export async function hooksAfterSwap(args: HooksAfterSwapArgs) {
  const vault = settlementVault(args.tradeId)
  const keys: AccountKey[] = [
    { pubkey: hookAuthority(), isSigner: false, isWritable: true },
    { pubkey: args.taker, isSigner: false, isWritable: false },
    { pubkey: args.custodyTokenAccount, isSigner: false, isWritable: true },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: vault, isSigner: false, isWritable: true },
    // The trade's escrow account, which the settlement program creates in the
    // CPI: it does not exist yet when this instruction is built.
    { pubkey: settlementEscrowToken(args.tradeId), isSigner: false, isWritable: true },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: SETTLEMENT_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
    ...(args.hookAccounts ?? []),
  ]
  return instruction(HOOKS_PROGRAM_ID, 'after_swap', keys, (w) => {
    w.u64(args.tradeId)
    w.u64(args.settlementSlots)
    w.u64(args.amount)
  })
}
