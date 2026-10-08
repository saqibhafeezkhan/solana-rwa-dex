/**
 * Instruction builders for the compliance rails (settlement clearinghouse,
 * KYC gate, oracle) and the token layer (RWA mint factory, transfer hook,
 * hooks escrow program).
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
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
  settlementVault,
  transferHookConfig,
  transferHookExtraAccountMetaList,
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
  /** T+N horizon in slots; must be in the future. */
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
}

export async function settlementInitiate(args: SettlementInitiateArgs) {
  const vault = settlementVault(args.tradeId)
  const vaultToken = ata(vault, args.tokenMint, args.tokenProgramId)
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
    { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
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
 */
export async function settlementSettle(
  tradeId: bigint,
  tokenMint: PublicKey,
  takerTokenAccount: PublicKey,
  signature: Uint8Array,
  recoveryId: number,
  tokenProgramId: PublicKey,
) {
  if (signature.length !== 64) throw new Error('the secp256k1 signature is 64 bytes (r || s)')
  const vault = settlementVault(tradeId)
  const keys: AccountKey[] = [
    { pubkey: vault, isSigner: false, isWritable: true },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: ata(vault, tokenMint, tokenProgramId), isSigner: false, isWritable: true },
    { pubkey: takerTokenAccount, isSigner: false, isWritable: true },
    { pubkey: tokenProgramId, isSigner: false, isWritable: false },
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
  /** Must equal `vault.refund_source` — the account that funded the escrow. */
  refundTokenAccount: PublicKey
  tokenProgramId: PublicKey
}

/** Compliance exit: returns the full escrow to the refund account. */
export async function settlementCancel(args: SettlementCancelArgs) {
  const vault = settlementVault(args.tradeId)
  const keys: AccountKey[] = [
    { pubkey: vault, isSigner: false, isWritable: true },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: SETTLEMENT_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: ata(vault, args.tokenMint, args.tokenProgramId), isSigner: false, isWritable: true },
    { pubkey: args.refundTokenAccount, isSigner: false, isWritable: true },
    { pubkey: args.authority, isSigner: true, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
  ]
  return instruction(SETTLEMENT_PROGRAM_ID, 'cancel_settlement', keys, () => {})
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
  destination: PublicKey
  mint: PublicKey
  authority: PublicKey
  amount: bigint
  /** Merkle proofs for the sending wallet's whitelist membership. */
  sourceProof: Uint8Array[]
  /** Merkle proofs for the receiving wallet's whitelist membership. */
  destinationProof: Uint8Array[]
}

/**
 * Token-2022 transfer that serializes both Merkle proofs into the trailing
 * instruction data; the hook checks source AND destination eligibility.
 */
export async function rwaTransfer(args: RwaTransferArgs) {
  const keys: AccountKey[] = [
    { pubkey: args.source, isSigner: false, isWritable: true },
    { pubkey: args.destination, isSigner: false, isWritable: true },
    { pubkey: args.mint, isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: false },
    { pubkey: TOKEN_2022_PROGRAM_ID, isSigner: false, isWritable: false },
    // Hook resolution order: the hook program, its extra-account-metas, config.
    { pubkey: TRANSFER_HOOK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: transferHookExtraAccountMetaList(args.mint), isSigner: false, isWritable: false },
    { pubkey: transferHookConfig(), isSigner: false, isWritable: false },
  ]
  return instruction(RWA_MINT_PROGRAM_ID, 'transfer', keys, (w) => {
    w.u64(args.amount)
    w.proofArray(args.sourceProof)
    w.proofArray(args.destinationProof)
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
 * from a wallet; it is exposed here for tooling/debugging only.
 */
export interface HooksAfterSwapArgs {
  tradeId: bigint
  taker: PublicKey
  /** The hook authority's custody ATA holding this trade's output. */
  custodyTokenAccount: PublicKey
  tokenMint: PublicKey
  tokenProgramId: PublicKey
}

export async function hooksAfterSwap(args: HooksAfterSwapArgs) {
  const vault = settlementVault(args.tradeId)
  const keys: AccountKey[] = [
    { pubkey: hookAuthority(), isSigner: false, isWritable: true },
    { pubkey: args.taker, isSigner: false, isWritable: false },
    { pubkey: args.custodyTokenAccount, isSigner: false, isWritable: true },
    { pubkey: args.tokenMint, isSigner: false, isWritable: false },
    { pubkey: vault, isSigner: false, isWritable: true },
    {
      pubkey: ata(vault, args.tokenMint, args.tokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    { pubkey: settlementConfig(), isSigner: false, isWritable: false },
    { pubkey: SETTLEMENT_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return instruction(HOOKS_PROGRAM_ID, 'after_swap', keys, (w) => w.u64(args.tradeId))
}
