/**
 * Resting limit orders (`order_book` program).
 *
 * A book is keyed by the base/quote pair; an order is keyed by
 * `(book, maker, orderId)` and escrows base for an ask, quote for a bid.
 * Fills move the escrow leg and pay the counter-leg, either straight to the
 * maker or — on a settlement-routed book — into the T+N clearinghouse.
 */
import { PublicKey } from '@solana/web3.js'
import {
  orderBookCancelOrder,
  orderBookCreateOrder,
  orderBookFillOrder,
  orderBookInitialize,
  orderBookSetAuthority,
  orderBookSetPaused,
  randomTradeId,
  type OrderBookCancelArgs,
  type OrderBookCreateArgs,
  type OrderBookFillArgs,
  type OrderBookInitializeArgs,
  type OrderBookKycAccounts,
  type OrderSide,
} from '../core/instructions-orderbook'
import {
  TOKEN_2022_PROGRAM_ID,
  ata,
  kycConfig,
  kycMint,
  orderBook,
  orderBookOrder,
} from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'
import { listOrders } from '../queries'

export type { OrderSide }

export function createOrdersModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    initializeBook: (args: OrderBookInitializeArgs, tx?: TxOptions) =>
      sendBuilt(ctx, orderBookInitialize(args), tx),

    createOrder: (args: OrderBookCreateArgs, tx?: TxOptions) =>
      sendBuilt(ctx, orderBookCreateOrder(args), tx),

    fillOrder: (args: OrderBookFillArgs, tx?: TxOptions) =>
      sendBuilt(ctx, orderBookFillOrder(args), tx),

    cancelOrder: (args: OrderBookCancelArgs, tx?: TxOptions) =>
      sendBuilt(ctx, orderBookCancelOrder(args), tx),

    setPaused: (book: PublicKey, authority: PublicKey, paused: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, orderBookSetPaused(book, authority, paused), tx),

    setAuthority: (
      book: PublicKey,
      authority: PublicKey,
      newAuthority: PublicKey,
      tx?: TxOptions,
    ) => sendBuilt(ctx, orderBookSetAuthority(book, authority, newAuthority), tx),

    build: {
      initializeBook: orderBookInitialize,
      createOrder: orderBookCreateOrder,
      fillOrder: orderBookFillOrder,
      cancelOrder: orderBookCancelOrder,
      setPaused: orderBookSetPaused,
      setAuthority: orderBookSetAuthority,
    },

    // -------------------------------------------------------------- helpers

    /** The mint an order of this side escrows. */
    escrowMintFor: (baseMint: PublicKey, quoteMint: PublicKey, side: OrderSide) =>
      side === 'ask' ? baseMint : quoteMint,

    /**
     * The KYC accounts a create/fill carries on a KYC-wired book: the config the
     * book stores (the canonical PDA unless it was opened against another one)
     * and the trader's attestation — the KYC mint's ATA under Token-2022, since
     * the config PDA is not a mint and has no ATA.
     */
    kycAccountsFor: (
      trader: PublicKey,
      config: PublicKey = kycConfig(),
    ): OrderBookKycAccounts => ({
      config,
      attestation: ata(trader, kycMint(), TOKEN_2022_PROGRAM_ID),
    }),

    /** A fresh settlement trade id, for a fill on a settlement-routed book. */
    newTradeId: randomTradeId,

    // ---------------------------------------------------------------- reads

    book: (address: PublicKey) => accounts.orderBook(address),
    bookFor: (baseMint: PublicKey, quoteMint: PublicKey) =>
      accounts.orderBook(orderBook(baseMint, quoteMint)),
    /** Every resting order on a book (program-account listing). */
    list: (baseMint: PublicKey, quoteMint: PublicKey) =>
      listOrders(ctx.connection, orderBook(baseMint, quoteMint)),
    order: (address: PublicKey) => accounts.order(address),
    orderFor: (
      baseMint: PublicKey,
      quoteMint: PublicKey,
      maker: PublicKey,
      orderId: bigint,
    ) => accounts.order(orderBookOrder(orderBook(baseMint, quoteMint), maker, orderId)),
  }
}

export type OrdersModule = ReturnType<typeof createOrdersModule>
