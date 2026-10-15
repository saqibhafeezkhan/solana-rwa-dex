/**
 * Instruction builders for the CLMM program. Every builder returns an
 * `Instruction` ready to be appended to a transaction; account order matches
 * the program's `#[derive(Accounts)]` structs exactly.
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  CLMM_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  SYSTEM_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ata,
  clmmPool,
  clmmPosition,
  clmmTick,
  clmmVaultA,
  clmmVaultB,
  hookAuthority,
  kycConfig,
  kycMint,
  oracleState,
  settlementConfig,
  settlementEscrowToken,
  settlementVault,
} from './pdas'

type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

function ix(name: string, keys: AccountKey[], encode: (w: BorshWriter) => void) {
  return encodeInstruction(name, encode).then(
    (data) =>
      new TransactionInstruction({
        programId: CLMM_PROGRAM_ID,
        keys,
        data: Buffer.from(data),
      }),
  )
}

// -----------------------------------------------------------------------
// initialize_pool
// -----------------------------------------------------------------------

export interface ClmmInitializePoolArgs {
  authority: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  initialSqrtPriceX64: bigint
  tickSpacing: number
  feePips: number
  /** Pass a real KYC config PDA to require KYC, or null to leave the rail off. */
  kycConfig: PublicKey | null
  /** Oracle threshold in bps; 0 leaves the oracle rail off. */
  maxDeviationBps: number
  /** Pass the hooks authority PDA to route swaps into settlement, or null. */
  hooksAuthority: PublicKey | null
  /** Required (non-zero) iff hooksAuthority is set. */
  settlementSlots: bigint
}

export async function clmmInitializePool(args: ClmmInitializePoolArgs) {
  const pool = clmmPool(args.mintA, args.mintB)
  const vaultA = clmmVaultA(pool, args.mintA)
  const vaultB = clmmVaultB(pool, args.mintB)
  const oracleOn = args.maxDeviationBps > 0
  const oracle = oracleOn ? oracleState(pool) : CLMM_PROGRAM_ID
  const settlementOn = args.hooksAuthority !== null
  if (settlementOn && args.settlementSlots === 0n) {
    throw new Error('settlement_slots must be > 0 when the settlement rail is enabled')
  }
  if (!settlementOn && args.settlementSlots !== 0n) {
    throw new Error('settlement_slots must be 0 when the settlement rail is disabled')
  }

  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: vaultA, isSigner: false, isWritable: true },
    { pubkey: vaultB, isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: args.authority, isSigner: true, isWritable: true },
    { pubkey: oracle, isSigner: false, isWritable: oracleOn },
    { pubkey: args.kycConfig ? KYC_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]

  return ix('initialize_pool', keys, (w) => {
      w.u128(args.initialSqrtPriceX64)
      w.i32(args.tickSpacing)
      w.u32(args.feePips)
      w.raw((args.kycConfig ?? PublicKey.default).toBytes())
      w.u16(args.maxDeviationBps)
      w.raw((args.hooksAuthority ?? PublicKey.default).toBytes())
      w.u64(args.settlementSlots)
  })
}

// -----------------------------------------------------------------------
// add_liquidity
// -----------------------------------------------------------------------

export interface ClmmAddLiquidityArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  lowerTick: number
  upperTick: number
  liquidityAmount: bigint
}

export async function clmmAddLiquidity(args: ClmmAddLiquidityArgs) {
  const pool = clmmPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: clmmTick(pool, args.lowerTick), isSigner: false, isWritable: true },
    { pubkey: clmmTick(pool, args.upperTick), isSigner: false, isWritable: true },
    { pubkey: clmmPosition(pool, args.user, args.lowerTick, args.upperTick), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: clmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: clmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('add_liquidity', keys, (w) => {
    w.i32(args.lowerTick)
    w.i32(args.upperTick)
    w.u128(args.liquidityAmount)
  })
}

// -----------------------------------------------------------------------
// remove_liquidity
// -----------------------------------------------------------------------

export interface ClmmRemoveLiquidityArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  lowerTick: number
  upperTick: number
  liquidityAmount: bigint
  minAmountA: bigint
  minAmountB: bigint
}

export async function clmmRemoveLiquidity(args: ClmmRemoveLiquidityArgs) {
  const pool = clmmPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: clmmPosition(pool, args.user, args.lowerTick, args.upperTick), isSigner: false, isWritable: true },
    { pubkey: clmmTick(pool, args.lowerTick), isSigner: false, isWritable: true },
    { pubkey: clmmTick(pool, args.upperTick), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: clmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: clmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('remove_liquidity', keys, (w) => {
    w.i32(args.lowerTick)
    w.i32(args.upperTick)
    w.u128(args.liquidityAmount)
    w.u64(args.minAmountA)
    w.u64(args.minAmountB)
  })
}

// -----------------------------------------------------------------------
// collect_fees
// -----------------------------------------------------------------------

export interface ClmmCollectFeesArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  lowerTick: number
  upperTick: number
}

export async function clmmCollectFees(args: ClmmCollectFeesArgs) {
  const pool = clmmPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: clmmPosition(pool, args.user, args.lowerTick, args.upperTick), isSigner: false, isWritable: true },
    { pubkey: clmmTick(pool, args.lowerTick), isSigner: false, isWritable: false },
    { pubkey: clmmTick(pool, args.upperTick), isSigner: false, isWritable: false },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: clmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: clmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('collect_fees', keys, (w) => {
    w.i32(args.lowerTick)
    w.i32(args.upperTick)
  })
}

// -----------------------------------------------------------------------
// swap
// -----------------------------------------------------------------------

export interface ClmmSwapArgs {
  user: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  amountIn: bigint
  minAmountOut: bigint
  isAToB: boolean
  /** Non-zero only when the pool routes output into T+N settlement. */
  tradeId: bigint
  /** Initialized ticks the trade may cross, in walk order. */
  ticks: number[]
  /** The pool's wiring; derive from the pool account when known. */
  wiring: {
    kycConfig: boolean
    oracle: boolean
    settlement: boolean
  }
}

export async function clmmSwap(args: ClmmSwapArgs) {
  const pool = clmmPool(args.mintA, args.mintB)
  const { kycConfig: kycOn, oracle: oracleOn, settlement: settlementOn } = args.wiring

  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: clmmVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: clmmVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.user, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.user, isSigner: true, isWritable: true },
    { pubkey: kycOn ? kycConfig() : CLMM_PROGRAM_ID, isSigner: false, isWritable: kycOn },
    // The trader's attestation account: the ATA of the KYC *mint* (a
    // kyc_gate PDA) under Token-2022 — not of the config PDA, and not under
    // the legacy token program, either of which yields an account that can
    // never exist and fails the gate's mint/owner checks.
    {
      pubkey: kycOn ? ata(args.user, kycMint(), TOKEN_2022_PROGRAM_ID) : CLMM_PROGRAM_ID,
      isSigner: false,
      isWritable: false,
    },
    { pubkey: oracleOn ? oracleState(pool) : CLMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: settlementOn ? hookAuthority() : CLMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? ata(hookAuthority(), args.isAToB ? args.mintB : args.mintA)
        : CLMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? settlementConfig() : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? SETTLEMENT_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? settlementVault(args.tradeId) : CLMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? settlementEscrowToken(args.tradeId)
        : CLMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? ASSOCIATED_TOKEN_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: kycOn ? KYC_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : CLMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  for (const tick of args.ticks) {
    keys.push({ pubkey: clmmTick(pool, tick), isSigner: false, isWritable: true })
  }
  return ix('swap', keys, (w) => {
    w.u64(args.amountIn)
    w.u64(args.minAmountOut)
    w.bool(args.isAToB)
    w.u64(args.tradeId)
  })
}

// -----------------------------------------------------------------------
// trip_breaker (CPIs oracle::trip_breaker with the pool's own price)
// -----------------------------------------------------------------------

export interface ClmmTripBreakerArgs {
  pool: PublicKey
}

export async function clmmTripBreaker(args: ClmmTripBreakerArgs) {
  const keys: AccountKey[] = [
    { pubkey: args.pool, isSigner: false, isWritable: false },
    { pubkey: oracleState(args.pool), isSigner: false, isWritable: true },
    { pubkey: ORACLE_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('trip_breaker', keys, () => {})
}

// -----------------------------------------------------------------------
// set_paused / set_authority (admin)
// -----------------------------------------------------------------------

/**
 * Pauses or resumes the pool. While paused `swap` and `add_liquidity` are
 * refused; `remove_liquidity` and `collect_fees` keep working so capital is
 * never trapped.
 */
export async function clmmSetPaused(pool: PublicKey, authority: PublicKey, paused: boolean) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_paused', keys, (w) => w.bool(paused))
}

export async function clmmSetAuthority(pool: PublicKey, authority: PublicKey, newAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}
