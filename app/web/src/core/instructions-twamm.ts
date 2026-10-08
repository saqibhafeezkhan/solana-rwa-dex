/**
 * Instruction builders for the TWAMM program (time-weighted AMM with long-term
 * orders). Account order matches the program's `#[derive(Accounts)]` structs
 * exactly; the order-list walks are passed as remaining accounts.
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  SYSTEM_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  TWAMM_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  oracleState,
  settlementConfig,
  settlementVault,
  twammLpPosition,
  twammOrder,
  twammPool,
  twammVaultA,
  twammVaultB,
} from './pdas'

type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

function ix(name: string, keys: AccountKey[], encode: (w: BorshWriter) => void) {
  return encodeInstruction(name, encode).then(
    (data) => new TransactionInstruction({ programId: TWAMM_PROGRAM_ID, keys, data: Buffer.from(data) }),
  )
}

const ZERO_KEY = PublicKey.default

// -----------------------------------------------------------------------
// initialize_pool
// -----------------------------------------------------------------------

export interface TwammInitializePoolArgs {
  payer: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  /** Both non-zero; the ratio is the genesis price P = initial_b / initial_a. */
  initialAmountA: bigint
  initialAmountB: bigint
  authority: PublicKey
  kycConfig: PublicKey | null
  maxDeviationBps: number
  hooksAuthority: PublicKey | null
  settlementSlots: bigint
}

export async function twammInitializePool(args: TwammInitializePoolArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const oracleOn = args.maxDeviationBps > 0
  const settlementOn = args.hooksAuthority !== null
  if (args.initialAmountA === 0n || args.initialAmountB === 0n) {
    throw new Error('both initial amounts must be non-zero')
  }
  if (settlementOn !== (args.settlementSlots > 0n)) {
    throw new Error('hooks_authority and settlement_slots must be set together')
  }

  const keys: AccountKey[] = [
    { pubkey: args.payer, isSigner: true, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: twammVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: twammVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: twammLpPosition(pool, args.payer), isSigner: false, isWritable: true },
    { pubkey: ata(args.payer, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.payer, args.mintB), isSigner: false, isWritable: true },
    { pubkey: oracleOn ? oracleState(pool) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: args.kycConfig ? KYC_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]

  return ix('initialize_pool', keys, (w) => {
    w.u64(args.initialAmountA)
    w.u64(args.initialAmountB)
    w.raw(args.authority.toBytes())
    w.raw((args.kycConfig ?? ZERO_KEY).toBytes())
    w.u16(args.maxDeviationBps)
    w.raw((args.hooksAuthority ?? ZERO_KEY).toBytes())
    w.u64(args.settlementSlots)
  })
}

// -----------------------------------------------------------------------
// add_liquidity
// -----------------------------------------------------------------------

export interface TwammAddLiquidityArgs {
  owner: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  amountA: bigint
  amountB: bigint
}

export async function twammAddLiquidity(args: TwammAddLiquidityArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: twammLpPosition(pool, args.owner), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: twammVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: twammVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.owner, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('add_liquidity', keys, (w) => {
    w.u64(args.amountA)
    w.u64(args.amountB)
  })
}

// -----------------------------------------------------------------------
// remove_liquidity
// -----------------------------------------------------------------------

export interface TwammRemoveLiquidityArgs {
  owner: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  shares: bigint
  minAmountA: bigint
  minAmountB: bigint
}

export async function twammRemoveLiquidity(args: TwammRemoveLiquidityArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: twammLpPosition(pool, args.owner), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: twammVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: twammVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.owner, isSigner: true, isWritable: true },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('remove_liquidity', keys, (w) => {
    w.u128(args.shares)
    w.u64(args.minAmountA)
    w.u64(args.minAmountB)
  })
}

// -----------------------------------------------------------------------
// create_long_term_order
// -----------------------------------------------------------------------

export interface TwammCreateOrderArgs {
  owner: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  orderId: bigint
  amountIn: bigint
  /** Seconds; the rate `amountIn / duration` must be at least 1 unit/second. */
  durationSeconds: number
  isAToB: boolean
  /** The live order accounts from `pool.order_head` up to (not including) the
   * first order expiring after this one, in list order. */
  walkOrders: PublicKey[]
  wiring: { kycConfig: boolean; oracle: boolean }
}

export async function twammCreateLongTermOrder(args: TwammCreateOrderArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const { kycConfig: kycOn, oracle: oracleOn } = args.wiring

  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: twammOrder(pool, args.owner, args.orderId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: twammVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: twammVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.owner, isSigner: true, isWritable: true },
    { pubkey: kycOn ? kycConfig() : TWAMM_PROGRAM_ID, isSigner: false, isWritable: kycOn },
    { pubkey: kycOn ? ata(args.owner, kycConfig()) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? oracleState(pool) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: kycOn ? KYC_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  for (const order of args.walkOrders) {
    keys.push({ pubkey: order, isSigner: false, isWritable: true })
  }
  return ix('create_long_term_order', keys, (w) => {
    w.u64(args.orderId)
    w.u64(args.amountIn)
    w.i64(args.durationSeconds)
    w.bool(args.isAToB)
  })
}

// -----------------------------------------------------------------------
// execute_virtual_orders (permissionless crank)
// -----------------------------------------------------------------------

export interface TwammExecuteArgs {
  mintA: PublicKey
  mintB: PublicKey
  /** Expired orders at the head of the list, in list order (if any). */
  expiredOrders: PublicKey[]
  wiring: { oracle: boolean }
}

export async function twammExecuteVirtualOrders(args: TwammExecuteArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const oracleOn = args.wiring.oracle
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? oracleState(pool) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  for (const order of args.expiredOrders) {
    keys.push({ pubkey: order, isSigner: false, isWritable: true })
  }
  return ix('execute_virtual_orders', keys, () => {})
}

// -----------------------------------------------------------------------
// cancel_order
// -----------------------------------------------------------------------

export interface TwammCancelOrderArgs {
  owner: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  orderId: bigint
  /** Whether the order being cancelled sells A (fixes the output mint for settlement routing). */
  isAToB: boolean
  /** The order list from `order_head` up to (not including) this order. */
  prefixOrders: PublicKey[]
  wiring: { oracle: boolean; settlement: boolean }
}

export async function twammCancelOrder(args: TwammCancelOrderArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const oracleOn = args.wiring.oracle
  const settlementOn = args.wiring.settlement
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: twammOrder(pool, args.owner, args.orderId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: twammVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: twammVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.owner, isSigner: true, isWritable: true },
    { pubkey: oracleOn ? oracleState(pool) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: oracleOn },
    { pubkey: settlementOn ? hookAuthority() : TWAMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn ? ata(hookAuthority(), args.isAToB ? args.mintB : args.mintA) : TWAMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? settlementConfig() : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? SETTLEMENT_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? settlementVault(args.orderId) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn
        ? ata(settlementVault(args.orderId), args.isAToB ? args.mintB : args.mintA)
        : TWAMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? ASSOCIATED_TOKEN_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: oracleOn ? ORACLE_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  for (const order of args.prefixOrders) {
    keys.push({ pubkey: order, isSigner: false, isWritable: true })
  }
  return ix('cancel_order', keys, (w) => w.u64(args.orderId))
}

// -----------------------------------------------------------------------
// withdraw_proceeds
// -----------------------------------------------------------------------

export interface TwammWithdrawArgs {
  owner: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  orderId: bigint
  isAToB: boolean
  wiring: { settlement: boolean }
}

export async function twammWithdrawProceeds(args: TwammWithdrawArgs) {
  const pool = twammPool(args.mintA, args.mintB)
  const settlementOn = args.wiring.settlement
  const outputMint = args.isAToB ? args.mintB : args.mintA
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: false },
    { pubkey: twammOrder(pool, args.owner, args.orderId), isSigner: false, isWritable: true },
    { pubkey: args.mintA, isSigner: false, isWritable: false },
    { pubkey: args.mintB, isSigner: false, isWritable: false },
    { pubkey: twammVaultA(pool, args.mintA), isSigner: false, isWritable: true },
    { pubkey: twammVaultB(pool, args.mintB), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintA), isSigner: false, isWritable: true },
    { pubkey: ata(args.owner, args.mintB), isSigner: false, isWritable: true },
    { pubkey: args.owner, isSigner: true, isWritable: true },
    { pubkey: settlementOn ? hookAuthority() : TWAMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn ? ata(hookAuthority(), outputMint) : TWAMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? settlementConfig() : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? SETTLEMENT_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? settlementVault(args.orderId) : TWAMM_PROGRAM_ID, isSigner: false, isWritable: settlementOn },
    {
      pubkey: settlementOn ? ata(settlementVault(args.orderId), outputMint) : TWAMM_PROGRAM_ID,
      isSigner: false,
      isWritable: settlementOn,
    },
    { pubkey: settlementOn ? ASSOCIATED_TOKEN_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: settlementOn ? HOOKS_PROGRAM_ID : TWAMM_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('withdraw_proceeds', keys, (w) => w.u64(args.orderId))
}

// -----------------------------------------------------------------------
// admin
// -----------------------------------------------------------------------

export async function twammSetPaused(pool: PublicKey, authority: PublicKey, paused: boolean) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_paused', keys, (w) => w.bool(paused))
}

export async function twammSetAuthority(pool: PublicKey, authority: PublicKey, newAuthority: PublicKey) {
  const keys: AccountKey[] = [
    { pubkey: pool, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}
