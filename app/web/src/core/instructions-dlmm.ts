/**
 * Instruction builders for the DLMM program (bin-based AMM). Account order
 * matches the program's `#[derive(Accounts)]` structs exactly. Optional rail
 * accounts are left off with the program-ID sentinel, which Anchor reads as
 * `None`.
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  DLMM_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  SYSTEM_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ata,
  dlmmBinArray,
  dlmmBinArrayForBin,
  dlmmPool,
  dlmmPosition,
  dlmmVaultA,
  dlmmVaultB,
  hookAuthority,
  kycConfig,
  oracleState,
  settlementConfig,
  settlementVault,
} from './pdas'

type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

function ix(name: string, keys: AccountKey[], encode: (w: BorshWriter) => void) {
  return encodeInstruction(name, encode).then(
    (data) => new TransactionInstruction({ programId: DLMM_PROGRAM_ID, keys, data: Buffer.from(data) }),
  )
}

const ZERO_KEY = PublicKey.default

// -----------------------------------------------------------------------
// initialize_pool
// -----------------------------------------------------------------------

export interface DlmmInitializePoolArgs {
  payer: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  /** The array index holding `activeBinId`, i.e. `bin_array_index(activeBinId)`. */
  arrayIndex: bigint
  activeBinId: number
  binStep: number
  baseFeeBps: number
  authority: PublicKey
  kycConfig: PublicKey | null
  maxDeviationBps: number
  hooksAuthority: PublicKey | null
  settlementSlots: bigint
}

export async function dlmmInitializePool(args: DlmmInitializePoolArgs) {
  const pool = dlmmPool(args.mintA, args.mintB)
  const array = dlmmBinArray(pool, args.arrayIndex)
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
    { pubkey: dlmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: dlmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: oracleOn ? oracleState(pool) : DLMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: args.kycConfig ? KYC_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
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

export interface DlmmInitializeBinArrayArgs {
  payer: PublicKey
  pool: PublicKey
  index: number | bigint
}

export async function dlmmInitializeBinArray(args: DlmmInitializeBinArrayArgs) {
  const keys: AccountKey[] = [
    { pubkey: args.payer, isSigner: true, isWritable: true },
    { pubkey: args.pool, isSigner: false, isWritable: false },
    { pubkey: dlmmBinArray(args.pool, args.index), isSigner: false, isWritable: true },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('initialize_bin_array', keys, (w) => {
    w.u64(args.index)
  })
}

// -----------------------------------------------------------------------
// add_liquidity
// -----------------------------------------------------------------------

export interface DlmmAddLiquidityArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  binId: number
  amountA: bigint
  amountB: bigint
}

export async function dlmmAddLiquidity(args: DlmmAddLiquidityArgs) {
  const pool = dlmmPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: dlmmBinArrayForBin(pool, args.binId), isSigner: false, isWritable: true },
    { pubkey: dlmmPosition(pool, args.user, args.binId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: dlmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: dlmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('add_liquidity', keys, (w) => {
    w.u64(dlmmBinArrayIndexFor(args.binId))
    w.i32(args.binId)
    w.u64(args.amountA)
    w.u64(args.amountB)
  })
}

function dlmmBinArrayIndexFor(binId: number): bigint {
  return BigInt(Math.floor(binId / 70))
}

// -----------------------------------------------------------------------
// remove_liquidity
// -----------------------------------------------------------------------

export interface DlmmRemoveLiquidityArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  binId: number
  shares: bigint
  minAmountA: bigint
  minAmountB: bigint
}

export async function dlmmRemoveLiquidity(args: DlmmRemoveLiquidityArgs) {
  const pool = dlmmPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: dlmmBinArrayForBin(pool, args.binId), isSigner: false, isWritable: true },
    { pubkey: dlmmPosition(pool, args.user, args.binId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: dlmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: dlmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('remove_liquidity', keys, (w) => {
    w.u64(dlmmBinArrayIndexFor(args.binId))
    w.i32(args.binId)
    w.u128(args.shares)
    w.u64(args.minAmountA)
    w.u64(args.minAmountB)
  })
}

// -----------------------------------------------------------------------
// swap
// -----------------------------------------------------------------------

export interface DlmmSwapArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  amountIn: bigint
  minAmountOut: bigint
  isAToB: boolean
  tradeId: bigint
  /** Bin arrays the trade walks, ascending when selling A, descending when selling B. */
  binArrayIndexes: (number | bigint)[]
  wiring: { kycConfig: boolean; oracle: boolean; settlement: boolean }
}

export async function dlmmSwap(args: DlmmSwapArgs) {
  const pool = dlmmPool(args.mintA, args.mintB)
  const { kycConfig: kycOn, oracle: oracleOn, settlement: settlementOn } = args.wiring
  if (args.binArrayIndexes.length === 0) {
    throw new Error('a DLMM swap needs at least the active bin array in binArrayIndexes')
  }

  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: dlmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: dlmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: kycOn ? kycConfig() : DLMM_PROGRAM_ID, isSigner: false, isWritable: kycOn },
    { pubkey: kycOn ? ata(args.user, kycConfig()) : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? oracleState(pool) : DLMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: settlementOn ? hookAuthority() : DLMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? ata(hookAuthority(), args.isAToB ? args.mintB : args.mintA)
        : DLMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? settlementConfig() : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? SETTLEMENT_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? settlementVault(args.tradeId) : DLMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? ata(settlementVault(args.tradeId), args.isAToB ? args.mintB : args.mintA)
        : DLMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? ASSOCIATED_TOKEN_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: kycOn ? KYC_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : DLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  for (const index of args.binArrayIndexes) {
    keys.push({ pubkey: dlmmBinArray(pool, index), isSigner: false, isWritable: true })
  }
  return ix('swap', keys, (w) => {
    w.u64(args.amountIn)
    w.u64(args.minAmountOut)
    w.bool(args.isAToB)
    w.u64(args.tradeId)
  })
}

// -----------------------------------------------------------------------
// trip_breaker
// -----------------------------------------------------------------------

export async function dlmmTripBreaker(pool: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: oracleState(pool), isSigner: false, isWritable: true },
    { pubkey: ORACLE_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('trip_breaker', keys, () => {})
}

// -----------------------------------------------------------------------
// set_paused / set_authority (admin)
// -----------------------------------------------------------------------

export async function dlmmSetPaused(pool: PublicKey, authority: PublicKey, paused: boolean) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_paused', keys, (w) => w.bool(paused))
}

export async function dlmmSetAuthority(pool: PublicKey, authority: PublicKey, newAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}
