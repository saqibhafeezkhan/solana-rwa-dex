/**
 * TWAMM (time-weighted AMM with long-term orders) features.
 *
 * Orders live in an expiry-sorted linked list on chain, and every order
 * instruction has to carry a prefix of it. The `walks` helpers read the links
 * for you; `crank` additionally reads the pool and derives the expired prefix,
 * so a keeper is one call.
 */
import type { PublicKey } from '@solana/web3.js'
import {
  twammAddLiquidity,
  twammCancelOrder,
  twammCreateLongTermOrder,
  twammExecuteVirtualOrders,
  twammInitializePool,
  twammRemoveLiquidity,
  twammSetAuthority,
  twammSetPaused,
  twammWithdrawProceeds,
  type TwammAddLiquidityArgs,
  type TwammCancelOrderArgs,
  type TwammCreateOrderArgs,
  type TwammExecuteArgs,
  type TwammInitializePoolArgs,
  type TwammRemoveLiquidityArgs,
  type TwammWithdrawArgs,
} from '../core/instructions-twamm'
import {
  twammLpPosition,
  twammOrder,
  twammPool,
  twammVaultA,
  twammVaultB,
} from '../core/pdas'
import {
  deriveTwammExpiredWalk,
  deriveTwammInsertionWalk,
  deriveTwammPrefixWalk,
} from '../core/walks'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'
import { railWiring } from '../wiring'

export function createTwammModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    initializePool: (args: TwammInitializePoolArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammInitializePool(args), tx),

    addLiquidity: (args: TwammAddLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammAddLiquidity(args), tx),

    removeLiquidity: (args: TwammRemoveLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammRemoveLiquidity(args), tx),

    createLongTermOrder: (args: TwammCreateOrderArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammCreateLongTermOrder(args), tx),

    executeVirtualOrders: (args: TwammExecuteArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammExecuteVirtualOrders(args), tx),

    cancelOrder: (args: TwammCancelOrderArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammCancelOrder(args), tx),

    withdrawProceeds: (args: TwammWithdrawArgs, tx?: TxOptions) =>
      sendBuilt(ctx, twammWithdrawProceeds(args), tx),

    /**
     * The permissionless crank, done for you: loads the pool, derives the
     * expired prefix from the live list, and executes virtual orders with the
     * pool's own oracle wiring.
     */
    crank: async (
      mintA: PublicKey,
      mintB: PublicKey,
      tx?: TxOptions,
    ) => {
      const pool = await accounts.twammPool(twammPool(mintA, mintB))
      const now = BigInt(Math.floor(Date.now() / 1000))
      const expiredOrders = await deriveTwammExpiredWalk(ctx.connection, pool.orderHead, now)
      const wiring = railWiring(pool)
      return ctx.send(
        [
          await twammExecuteVirtualOrders({
            mintA,
            mintB,
            expiredOrders,
            wiring: { oracle: wiring.oracle },
          }),
        ],
        tx,
      )
    },

    setPaused: (pool: PublicKey, authority: PublicKey, paused: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, twammSetPaused(pool, authority, paused), tx),

    setAuthority: (
      pool: PublicKey,
      authority: PublicKey,
      newAuthority: PublicKey,
      tx?: TxOptions,
    ) => sendBuilt(ctx, twammSetAuthority(pool, authority, newAuthority), tx),

    build: {
      initializePool: twammInitializePool,
      addLiquidity: twammAddLiquidity,
      removeLiquidity: twammRemoveLiquidity,
      createLongTermOrder: twammCreateLongTermOrder,
      executeVirtualOrders: twammExecuteVirtualOrders,
      cancelOrder: twammCancelOrder,
      withdrawProceeds: twammWithdrawProceeds,
      setPaused: twammSetPaused,
      setAuthority: twammSetAuthority,
    },

    // ---------------------------------------------------------------- walks

    walks: {
      /** Accounts `createLongTermOrder` carries for an order expiring at `newExpiry`. */
      insertion: (mintA: PublicKey, mintB: PublicKey, newExpiry: bigint) =>
        accounts
          .twammPool(twammPool(mintA, mintB))
          .then((pool) => deriveTwammInsertionWalk(ctx.connection, pool.orderHead, newExpiry)),
      /** Accounts `executeVirtualOrders` carries for expired orders at `now`. */
      expired: (mintA: PublicKey, mintB: PublicKey, now?: bigint) =>
        accounts.twammPool(twammPool(mintA, mintB)).then((pool) =>
          deriveTwammExpiredWalk(
            ctx.connection,
            pool.orderHead,
            now ?? BigInt(Math.floor(Date.now() / 1000)),
          ),
        ),
      /** Accounts in front of one order, for `cancelOrder`. */
      cancelPrefix: (mintA: PublicKey, mintB: PublicKey, orderId: bigint, owner: PublicKey) =>
        accounts.twammPool(twammPool(mintA, mintB)).then((pool) =>
          deriveTwammPrefixWalk(
            ctx.connection,
            pool.orderHead,
            twammOrder(pool.address, owner, orderId),
          ),
        ),
    },

    // ---------------------------------------------------------------- reads

    pool: (address: PublicKey) => accounts.twammPool(address),
    poolFor: (mintA: PublicKey, mintB: PublicKey) => accounts.twammPool(twammPool(mintA, mintB)),
    lp: (address: PublicKey) => accounts.twammLp(address),
    lpFor: (owner: PublicKey, mintA: PublicKey, mintB: PublicKey) =>
      accounts.twammLp(twammLpPosition(twammPool(mintA, mintB), owner)),
    order: (address: PublicKey) => accounts.twammOrder(address),
    orderFor: (owner: PublicKey, mintA: PublicKey, mintB: PublicKey, orderId: bigint) =>
      accounts.twammOrder(twammOrder(twammPool(mintA, mintB), owner, orderId)),

    /** The rails this pool runs, ready for an order's `wiring` argument. */
    wiringFor: (mintA: PublicKey, mintB: PublicKey) =>
      accounts.twammPool(twammPool(mintA, mintB)).then(railWiring),

    addressesFor: (mintA: PublicKey, mintB: PublicKey) => {
      const pool = twammPool(mintA, mintB)
      return { pool, vaultA: twammVaultA(pool, mintA), vaultB: twammVaultB(pool, mintB) }
    },
  }
}

export type TwammModule = ReturnType<typeof createTwammModule>
