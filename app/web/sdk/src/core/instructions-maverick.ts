/**
 * Instruction builders for the Maverick program (dynamic-distribution bin AMM
 * whose liquidity follows the price). Account order matches the program's
 * `#[derive(Accounts)]` structs exactly.
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  MAVERICK_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  SYSTEM_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  kycMint,
  maverickBinArray,
  maverickBinArrayForBin,
  maverickPool,
  maverickPosition,
  maverickVaultA,
  maverickVaultB,
  oracleState,
  settlementConfig,
  settlementEscrowToken,
  settlementVault,
} from './pdas'

type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

function ix(name: string, keys: AccountKey[], encode: (w: BorshWriter) => void) {
  return encodeInstruction(name, encode).then(
    (data) => new TransactionInstruction({ programId: MAVERICK_PROGRAM_ID, keys, data: Buffer.from(data) }),
  )
}

const ZERO_KEY = PublicKey.default

/** Bin kinds, in the on-chain enum's order. */
export type MaverickBinKind = 'static' | 'right' | 'left' | 'both'

const BIN_KIND_U8: Record<MaverickBinKind, number> = {
  static: 0,
  right: 1,
  left: 2,
  both: 3,
}

const MAVERICK_BINS_PER_ARRAY = 50

function binArrayIndexFor(binId: number): bigint {
  return BigInt(Math.floor(binId / MAVERICK_BINS_PER_ARRAY))
}

// -----------------------------------------------------------------------
// initialize_pool
// -----------------------------------------------------------------------

export interface MaverickInitializePoolArgs {
  payer: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  arrayIndex: bigint
  activeBinId: number
  binStep: number
  baseFeeBps: number
  authority: PublicKey
  kycConfig: PublicKey | null
  maxDeviationBps: number
  hooksAuthority: PublicKey | null
  settlementSlots: bigint
  /** Defaults to legacy SPL Token; pass Token-2022 for a 2022-mint pool. */
  tokenProgramId?: PublicKey
}

export async function maverickInitializePool(args: MaverickInitializePoolArgs) {
  const pool = maverickPool(args.mintA, args.mintB)
  const tokenProgram = args.tokenProgramId ?? TOKEN_PROGRAM_ID
  const array = maverickBinArray(pool, args.arrayIndex)
  const oracleOn = args.maxDeviationBps > 0
  const settlementOn = args.hooksAuthority !== null
  if (settlementOn !== (args.settlementSlots > 0n)) {
    throw new Error('hooks_authority and settlement_slots must be set together')
  }

  const keys: AccountKey[] = [
    { pubkey: args.payer, isSigner: true, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: array, isSigner: false, isWritable: true },
    { pubkey: maverickVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: maverickVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: oracleOn ? oracleState(pool) : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: args.kycConfig ? KYC_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: tokenProgram, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]

  return ix('initialize_pool', keys, (w) => {
    w.u64(args.arrayIndex)
    w.i32(args.activeBinId)
    w.u16(args.binStep)
    w.u16(args.baseFeeBps)
    w.raw(args.authority.toBytes())
    w.raw((args.kycConfig ?? ZERO_KEY).toBytes())
    w.u16(args.maxDeviationBps)
    w.raw((args.hooksAuthority ?? ZERO_KEY).toBytes())
    w.u64(args.settlementSlots)
  })
}

// -----------------------------------------------------------------------
// initialize_bin_array
// -----------------------------------------------------------------------

export async function maverickInitializeBinArray(payer: PublicKey, pool: PublicKey, index: number | bigint) {
  const keys: AccountKey[] = [
    { pubkey: payer, isSigner: true, isWritable: true },
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: maverickBinArray(pool, index), isSigner: false, isWritable: true },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('initialize_bin_array', keys, (w) => {
    w.u64(index)
  })
}

// -----------------------------------------------------------------------
// add_liquidity
// -----------------------------------------------------------------------

export interface MaverickAddLiquidityArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  binId: number
  kind: MaverickBinKind
  amountA: bigint
  amountB: bigint
  tokenProgramId?: PublicKey
  /** The mint's transfer-hook accounts; see `hookTransferAccounts`. */
  hookAccounts?: AccountKey[]
}

export async function maverickAddLiquidity(args: MaverickAddLiquidityArgs) {
  const pool = maverickPool(args.mintA, args.mintB)
  const tokenProgram = args.tokenProgramId ?? TOKEN_PROGRAM_ID
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: maverickBinArrayForBin(pool, args.binId), isSigner: false, isWritable: true },
    { pubkey: maverickPosition(pool, args.user, args.binId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: maverickVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: maverickVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA, tokenProgram), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB, tokenProgram), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: tokenProgram, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  keys.push(...(args.hookAccounts ?? []))
  return ix('add_liquidity', keys, (w) => {
    w.u64(binArrayIndexFor(args.binId))
    w.i32(args.binId)
    w.u8(BIN_KIND_U8[args.kind])
    w.u64(args.amountA)
    w.u64(args.amountB)
  })
}

// -----------------------------------------------------------------------
// remove_liquidity (follows the move chain; hop arrays are remaining accounts)
// -----------------------------------------------------------------------

export interface MaverickRemoveLiquidityArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  binId: number
  shares: bigint
  minAmountA: bigint
  minAmountB: bigint
  /** Bin arrays the move chain runs through, one per hop outside the position's array. */
  hopArrayIndexes: (number | bigint)[]
  tokenProgramId?: PublicKey
  hookAccounts?: AccountKey[]
}

export async function maverickRemoveLiquidity(args: MaverickRemoveLiquidityArgs) {
  const pool = maverickPool(args.mintA, args.mintB)
  const tokenProgram = args.tokenProgramId ?? TOKEN_PROGRAM_ID
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: maverickBinArrayForBin(pool, args.binId), isSigner: false, isWritable: true },
    { pubkey: maverickPosition(pool, args.user, args.binId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: maverickVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: maverickVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA, tokenProgram), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB, tokenProgram), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: tokenProgram, isSigner: false, isWritable: false },
  ]
  for (const index of args.hopArrayIndexes) {
    keys.push({ pubkey: maverickBinArray(pool, index), isSigner: false, isWritable: true })
  }
  keys.push(...(args.hookAccounts ?? []))
  return ix('remove_liquidity', keys, (w) => {
    w.u64(binArrayIndexFor(args.binId))
    w.i32(args.binId)
    w.u128(args.shares)
    w.u64(args.minAmountA)
    w.u64(args.minAmountB)
  })
}

// -----------------------------------------------------------------------
// move_bins (permissionless keeper)
// -----------------------------------------------------------------------

export interface MaverickMoveBinsArgs {
  pool: PublicKey
  fromArrayIndex: number | bigint
  fromBinId: number
  toBinId: number
}

export async function maverickMoveBins(args: MaverickMoveBinsArgs) {
  const toIndex = binArrayIndexFor(args.toBinId)
  const keys: AccountKey[] = [
    { pubkey: args.pool, isSigner: false, isWritable: true },
    { pubkey: maverickBinArray(args.pool, args.fromArrayIndex), isSigner: false, isWritable: true },
  ]
  if (toIndex !== args.fromArrayIndex) {
    keys.push({ pubkey: maverickBinArray(args.pool, toIndex), isSigner: false, isWritable: true })
  }
  return ix('move_bins', keys, (w) => {
    w.u64(args.fromArrayIndex)
    w.i32(args.fromBinId)
    w.i32(args.toBinId)
  })
}

// -----------------------------------------------------------------------
// swap
// -----------------------------------------------------------------------

export interface MaverickSwapArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  amountIn: bigint
  minAmountOut: bigint
  isAToB: boolean
  tradeId: bigint
  binArrayIndexes: (number | bigint)[]
  wiring: { kycConfig: boolean; oracle: boolean; settlement: boolean }
  tokenProgramId?: PublicKey
  /** The mint's transfer-hook accounts; see `hookTransferAccounts`. */
  hookAccounts?: AccountKey[]
}

export async function maverickSwap(args: MaverickSwapArgs) {
  const pool = maverickPool(args.mintA, args.mintB)
  const tokenProgram = args.tokenProgramId ?? TOKEN_PROGRAM_ID
  const { kycConfig: kycOn, oracle: oracleOn, settlement: settlementOn } = args.wiring
  if (args.binArrayIndexes.length === 0) {
    throw new Error('a Maverick swap needs at least the active bin array in binArrayIndexes')
  }

  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: maverickVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: maverickVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA, tokenProgram), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB, tokenProgram), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: kycOn ? kycConfig() : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: kycOn },
    // Attestation account: the ATA of the KYC mint under Token-2022 (the
    // config PDA is not a mint, so an ATA of it can never exist).
    {
      pubkey: kycOn ? ata(args.user, kycMint(), TOKEN_2022_PROGRAM_ID) : MAVERICK_PROGRAM_ID,
      isSigner: false,
      isWritable: false,
    },
    { pubkey: oracleOn ? oracleState(pool) : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: settlementOn ? hookAuthority() : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? ata(hookAuthority(), args.isAToB ? args.mintB : args.mintA, tokenProgram)
        : MAVERICK_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? settlementConfig() : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? SETTLEMENT_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? settlementVault(args.tradeId) : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? settlementEscrowToken(args.tradeId)
        : MAVERICK_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? ASSOCIATED_TOKEN_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: kycOn ? KYC_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : MAVERICK_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: tokenProgram, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  for (const index of args.binArrayIndexes) {
    keys.push({ pubkey: maverickBinArray(pool, index), isSigner: false, isWritable: true })
  }
  keys.push(...(args.hookAccounts ?? []))
  return ix('swap', keys, (w) => {
    w.u64(args.amountIn)
    w.u64(args.minAmountOut)
    w.bool(args.isAToB)
    w.u64(args.tradeId)
  })
}

// -----------------------------------------------------------------------
// trip_breaker / admin
// -----------------------------------------------------------------------

export async function maverickTripBreaker(pool: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: oracleState(pool), isSigner: false, isWritable: true },
    { pubkey: ORACLE_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('trip_breaker', keys, () => {})
}

export async function maverickSetPaused(pool: PublicKey, authority: PublicKey, paused: boolean) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_paused', keys, (w) => w.bool(paused))
}

export async function maverickSetAuthority(pool: PublicKey, authority: PublicKey, newAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}
