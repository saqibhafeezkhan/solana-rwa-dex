/**
 * CLMM (concentrated liquidity) features.
 *
 * Actions are camelCase and send the transaction for you; the raw builders live
 * under `.build` when you need to compose your own transaction:
 *
 *   await dex.clmm.swap(args)              // build + sign + send + confirm
 *   const ix = await dex.clmm.build.swap(args)
 */
import type { PublicKey } from '@solana/web3.js'
import {
  clmmAddLiquidity,
  clmmCollectFees,
  clmmInitializePool,
  clmmRemoveLiquidity,
  clmmSetAuthority,
  clmmSetPaused,
  clmmSwap,
  clmmTripBreaker,
  type ClmmAddLiquidityArgs,
  type ClmmCollectFeesArgs,
  type ClmmInitializePoolArgs,
  type ClmmRemoveLiquidityArgs,
  type ClmmSwapArgs,
  type ClmmTripBreakerArgs,
} from '../core/instructions-clmm'
import { clmmPool, clmmPosition, clmmTick, clmmVaultA, clmmVaultB } from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'
import { railWiring } from '../wiring'

export function createClmmModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    initializePool: (args: ClmmInitializePoolArgs, tx?: TxOptions) =>
      sendBuilt(ctx, clmmInitializePool(args), tx),

    addLiquidity: (args: ClmmAddLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, clmmAddLiquidity(args), tx),

    removeLiquidity: (args: ClmmRemoveLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, clmmRemoveLiquidity(args), tx),

    collectFees: (args: ClmmCollectFeesArgs, tx?: TxOptions) =>
      sendBuilt(ctx, clmmCollectFees(args), tx),

    swap: (args: ClmmSwapArgs, tx?: TxOptions) => sendBuilt(ctx, clmmSwap(args), tx),

    /** CPIs the oracle's circuit breaker with the pool's own price. */
    tripBreaker: (args: ClmmTripBreakerArgs, tx?: TxOptions) =>
      sendBuilt(ctx, clmmTripBreaker(args), tx),

    setPaused: (pool: PublicKey, authority: PublicKey, paused: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, clmmSetPaused(pool, authority, paused), tx),

    setAuthority: (
      pool: PublicKey,
      authority: PublicKey,
      newAuthority: PublicKey,
      tx?: TxOptions,
    ) => sendBuilt(ctx, clmmSetAuthority(pool, authority, newAuthority), tx),

    /** The same features as instruction builders, without sending. */
    build: {
      initializePool: clmmInitializePool,
      addLiquidity: clmmAddLiquidity,
      removeLiquidity: clmmRemoveLiquidity,
      collectFees: clmmCollectFees,
      swap: clmmSwap,
      tripBreaker: clmmTripBreaker,
      setPaused: clmmSetPaused,
      setAuthority: clmmSetAuthority,
    },

    // ---------------------------------------------------------------- reads

    pool: (address: PublicKey) => accounts.clmmPool(address),
    poolFor: (mintA: PublicKey, mintB: PublicKey) => accounts.clmmPool(clmmPool(mintA, mintB)),
    /** The rails this pool runs, ready for `swap`'s `wiring` argument. */
    wiringFor: (mintA: PublicKey, mintB: PublicKey) =>
      accounts.clmmPool(clmmPool(mintA, mintB)).then(railWiring),
    position: (address: PublicKey) => accounts.clmmPosition(address),
    positionFor: (
      owner: PublicKey,
      mintA: PublicKey,
      mintB: PublicKey,
      lowerTick: number,
      upperTick: number,
    ) => accounts.clmmPosition(clmmPosition(clmmPool(mintA, mintB), owner, lowerTick, upperTick)),

    /** The pool and its two vaults, derived from the mint pair. */
    addressesFor: (mintA: PublicKey, mintB: PublicKey) => {
      const pool = clmmPool(mintA, mintB)
      return {
        pool,
        vaultA: clmmVaultA(pool, mintA),
        vaultB: clmmVaultB(pool, mintB),
      }
    },

    tick: (mintA: PublicKey, mintB: PublicKey, tick: number) =>
      clmmTick(clmmPool(mintA, mintB), tick),
  }
}

export type ClmmModule = ReturnType<typeof createClmmModule>
