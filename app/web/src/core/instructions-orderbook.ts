/**
 * Instruction builders for the `order_book` program.
 *
 * Account order matches the program's `#[derive(Accounts)]` structs exactly.
 * The optional rails use Anchor's program-ID sentinel (there is no null account
 * on Solana): when a rail is off its accounts are the order_book program ID.
 *
 * Fills are denominated in the order's escrow token. An ask escrows base and
 * sells it; a bid escrows quote and buys base. `trade_id` is consumed from the
 * book's counter only when the settlement rail is on.
 */
import { PublicKey, TransactionInstruction } from '@solana/web3.js'
import { BorshWriter, encodeInstruction } from './borsh'
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  ORDER_BOOK_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  SYSTEM_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  orderBook,
  orderBookOrder,
  programData,
  settlementConfig,
  settlementEscrowToken,
  settlementVault,
} from './pdas'

type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

export type OrderSide = 'ask' | 'bid'

/** The `Side` enum's borsh encoding is its variant index. */
const sideIndex = (side: OrderSide) => (side === 'ask' ? 0 : 1)

function ix(name: string, keys: AccountKey[], encode: (w: BorshWriter) => void) {
  return encodeInstruction(name, encode).then(
    (data) =>
      new TransactionInstruction({
        programId: ORDER_BOOK_PROGRAM_ID,
        keys,
        data: Buffer.from(data),
      }),
  )
}

const sentinel = ORDER_BOOK_PROGRAM_ID

// -----------------------------------------------------------------------
// initialize_book
// -----------------------------------------------------------------------

export interface OrderBookInitializeArgs {
  authority: PublicKey
  baseMint: PublicKey
  quoteMint: PublicKey
  /** Canonical KYC config, or null to leave the KYC rail off. */
  kycConfig: PublicKey | null
  /** Canonical hooks authority PDA, or null for direct maker payouts. */
  hooksAuthority: PublicKey | null
  settlementSlots: bigint
}

export async function orderBookInitialize(args: OrderBookInitializeArgs) {
  const settlementOn = args.hooksAuthority !== null
  if (settlementOn !== (args.settlementSlots > 0n)) {
    throw new Error('hooks authority and settlement slots must be set together')
  }
  const keys: AccountKey[] = [
    { pubkey: orderBook(args.baseMint, args.quoteMint), isSigner: false, isWritable: true },
    { pubkey: args.authority, isSigner: true, isWritable: true },
    { pubkey: args.baseMint, isSigner: false, isWritable: false },
    { pubkey: args.quoteMint, isSigner: false, isWritable: false },
    { pubkey: programData(ORDER_BOOK_PROGRAM_ID), isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  return ix('initialize_book', keys, (w) => {
    w.raw((args.kycConfig ?? PublicKey.default).toBytes())
    w.raw((args.hooksAuthority ?? PublicKey.default).toBytes())
    w.u64(args.settlementSlots)
  })
}

// -----------------------------------------------------------------------
// create_order
// -----------------------------------------------------------------------

export interface OrderBookKycAccounts {
  /** Canonical KYC config PDA, read from the book. */
  config: PublicKey
  /** The trader's attestation ATA for the KYC mint (Token-2022). */
  attestation: PublicKey
}

export interface OrderBookCreateArgs {
  maker: PublicKey
  baseMint: PublicKey
  quoteMint: PublicKey
  orderId: bigint
  side: OrderSide
  /** Quote raw units per base raw unit, scaled by 1e9. */
  price: bigint
  /** Amount escrowed, in the order's escrow token. */
  amount: bigint
  expiryTimestamp: bigint
  /** Mint of the escrow token: base for an ask, quote for a bid. */
  escrowMint: PublicKey
  tokenProgramId: PublicKey
  kyc?: OrderBookKycAccounts
  /** The mints' transfer-hook accounts; see `hookAccountsForTransfers`. */
  hookAccounts?: AccountKey[]
}

export async function orderBookCreateOrder(args: OrderBookCreateArgs) {
  const book = orderBook(args.baseMint, args.quoteMint)
  const order = orderBookOrder(book, args.maker, args.orderId)
  const keys: AccountKey[] = [
    { pubkey: args.maker, isSigner: true, isWritable: true },
    { pubkey: book, isSigner: false, isWritable: false },
    { pubkey: args.baseMint, isSigner: false, isWritable: false },
    { pubkey: args.quoteMint, isSigner: false, isWritable: false },
    { pubkey: order, isSigner: false, isWritable: true },
    { pubkey: args.escrowMint, isSigner: false, isWritable: false },
    {
      pubkey: ata(order, args.escrowMint, args.tokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    {
      pubkey: ata(args.maker, args.escrowMint, args.tokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    { pubkey: args.kyc?.config ?? sentinel, isSigner: false, isWritable: false },
    { pubkey: args.kyc?.attestation ?? sentinel, isSigner: false, isWritable: false },
    { pubkey: args.kyc ? KYC_PROGRAM_ID : sentinel, isSigner: false, isWritable: false },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
    { pubkey: ASSOCIATED_TOKEN_PROGRAM_ID, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  keys.push(...(args.hookAccounts ?? []))

  return ix('create_order', keys, (w) => {
    w.u64(args.orderId)
    w.u8(sideIndex(args.side))
    w.u64(args.price)
    w.u64(args.amount)
    w.i64(args.expiryTimestamp)
  })
}

// -----------------------------------------------------------------------
// fill_order
// -----------------------------------------------------------------------

export interface OrderBookFillArgs {
  taker: PublicKey
  maker: PublicKey
  baseMint: PublicKey
  quoteMint: PublicKey
  orderId: bigint
  side: OrderSide
  /** Amount filled, in the order's escrow token. */
  amount: bigint
  /** Slippage bound on the counter-leg (quote for an ask, base for a bid). */
  maxCounterAmount: bigint
  /**
   * Caller-chosen settlement trade id. Ignored when `settlement` is false;
   * otherwise non-zero and unused, as the AMM swaps' ids are.
   */
  tradeId: bigint
  /** Each leg is transferred under the program that owns its mint. */
  baseTokenProgramId: PublicKey
  quoteTokenProgramId: PublicKey
  settlement: boolean
  kyc?: OrderBookKycAccounts
  hookAccounts?: AccountKey[]
}

export async function orderBookFillOrder(args: OrderBookFillArgs) {
  const book = orderBook(args.baseMint, args.quoteMint)
  const order = orderBookOrder(book, args.maker, args.orderId)
  const escrowMint = args.side === 'ask' ? args.baseMint : args.quoteMint
  const paymentMint = args.side === 'ask' ? args.quoteMint : args.baseMint
  const escrowTokenProgram =
    args.side === 'ask' ? args.baseTokenProgramId : args.quoteTokenProgramId
  const paymentTokenProgram =
    args.side === 'ask' ? args.quoteTokenProgramId : args.baseTokenProgramId
  const proceeds = args.settlement
    ? ata(hookAuthority(), paymentMint, paymentTokenProgram)
    : ata(args.maker, paymentMint, paymentTokenProgram)

  const keys: AccountKey[] = [
    { pubkey: args.taker, isSigner: true, isWritable: true },
    { pubkey: args.maker, isSigner: false, isWritable: false },
    // Read-only: a fill never writes the book now that ids are caller-named.
    { pubkey: book, isSigner: false, isWritable: false },
    { pubkey: order, isSigner: false, isWritable: true },
    { pubkey: args.baseMint, isSigner: false, isWritable: false },
    { pubkey: args.quoteMint, isSigner: false, isWritable: false },
    { pubkey: escrowMint, isSigner: false, isWritable: false },
    {
      pubkey: ata(order, escrowMint, escrowTokenProgram),
      isSigner: false,
      isWritable: true,
    },
    {
      pubkey: ata(args.taker, args.baseMint, args.baseTokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    {
      pubkey: ata(args.taker, args.quoteMint, args.quoteTokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    { pubkey: proceeds, isSigner: false, isWritable: true },
    {
      pubkey: args.settlement ? hookAuthority() : sentinel,
      isSigner: false,
      isWritable: args.settlement,
    },
    {
      pubkey: args.settlement ? settlementConfig() : sentinel,
      isSigner: false,
      isWritable: false,
    },
    {
      pubkey: args.settlement ? SETTLEMENT_PROGRAM_ID : sentinel,
      isSigner: false,
      isWritable: false,
    },
    {
      pubkey: args.settlement ? settlementVault(args.tradeId) : sentinel,
      isSigner: false,
      isWritable: args.settlement,
    },
    {
      pubkey: args.settlement ? settlementEscrowToken(args.tradeId) : sentinel,
      isSigner: false,
      isWritable: args.settlement,
    },
    {
      pubkey: args.settlement ? ASSOCIATED_TOKEN_PROGRAM_ID : sentinel,
      isSigner: false,
      isWritable: false,
    },
    {
      pubkey: args.settlement ? HOOKS_PROGRAM_ID : sentinel,
      isSigner: false,
      isWritable: false,
    },
    { pubkey: args.kyc?.config ?? sentinel, isSigner: false, isWritable: false },
    { pubkey: args.kyc?.attestation ?? sentinel, isSigner: false, isWritable: false },
    { pubkey: args.kyc ? KYC_PROGRAM_ID : sentinel, isSigner: false, isWritable: false },
    { pubkey: args.baseTokenProgramId, isSigner: false, isWritable: false },
    { pubkey: args.quoteTokenProgramId, isSigner: false, isWritable: false },
    { pubkey: SYSTEM_PROGRAM_ID, isSigner: false, isWritable: false },
  ]
  keys.push(...(args.hookAccounts ?? []))

  return ix('fill_order', keys, (w) => {
    w.u64(args.amount)
    w.u64(args.maxCounterAmount)
    w.u64(args.tradeId)
  })
}

// -----------------------------------------------------------------------
// cancel_order
// -----------------------------------------------------------------------

export interface OrderBookCancelArgs {
  maker: PublicKey
  baseMint: PublicKey
  quoteMint: PublicKey
  orderId: bigint
  side: OrderSide
  tokenProgramId: PublicKey
  hookAccounts?: AccountKey[]
}

export async function orderBookCancelOrder(args: OrderBookCancelArgs) {
  const book = orderBook(args.baseMint, args.quoteMint)
  const order = orderBookOrder(book, args.maker, args.orderId)
  const escrowMint = args.side === 'ask' ? args.baseMint : args.quoteMint
  const keys: AccountKey[] = [
    { pubkey: args.maker, isSigner: true, isWritable: true },
    { pubkey: book, isSigner: false, isWritable: false },
    { pubkey: order, isSigner: false, isWritable: true },
    { pubkey: args.baseMint, isSigner: false, isWritable: false },
    { pubkey: args.quoteMint, isSigner: false, isWritable: false },
    { pubkey: escrowMint, isSigner: false, isWritable: false },
    {
      pubkey: ata(order, escrowMint, args.tokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    {
      pubkey: ata(args.maker, escrowMint, args.tokenProgramId),
      isSigner: false,
      isWritable: true,
    },
    { pubkey: args.tokenProgramId, isSigner: false, isWritable: false },
  ]
  keys.push(...(args.hookAccounts ?? []))
  return ix('cancel_order', keys, () => {})
}

// -----------------------------------------------------------------------
// admin
// -----------------------------------------------------------------------

export async function orderBookSetPaused(book: PublicKey, authority: PublicKey, paused: boolean) {
  const keys: AccountKey[] = [
    { pubkey: book, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_paused', keys, (w) => w.bool(paused))
}

export async function orderBookSetAuthority(
  book: PublicKey,
  authority: PublicKey,
  newAuthority: PublicKey,
) {
  const keys: AccountKey[] = [
    { pubkey: book, isSigner: false, isWritable: true },
    { pubkey: authority, isSigner: true, isWritable: false },
  ]
  return ix('set_authority', keys, (w) => w.raw(newAuthority.toBytes()))
}

/**
 * A fresh settlement trade id for one routed fill.
 *
 * Ids are a cluster-wide namespace owned by the settlement program, and the
 * AMM engines' swaps name theirs the same way, so a book keeps no counter of
 * its own: a shared counter would let one taker take "the next" id with a
 * one-unit fill and make every other pending fill revert. Random keeps two
 * takers' concurrent fills apart; a repeated id still fails closed in the
 * settlement vault's `init`.
 */
export function randomTradeId(): bigint {
  const words = new Uint32Array(2)
  crypto.getRandomValues(words)
  const id = (BigInt(words[0]) << 32n) | BigInt(words[1])
  // 0 is reserved for "rail off".
  return id === 0n ? 1n : id
}

/** The canonical KYC config address, re-exported for page wiring. */
export const orderBookKycConfig = kycConfig
/** The canonical hooks authority address, re-exported for page wiring. */
export const orderBookHookAuthority = hookAuthority

/** Convenience for a trader's attestation ATA when the KYC rail is on. */
export const orderBookKycAttestation = (trader: PublicKey, kycMint: PublicKey) =>
  ata(trader, kycMint, TOKEN_2022_PROGRAM_ID)
