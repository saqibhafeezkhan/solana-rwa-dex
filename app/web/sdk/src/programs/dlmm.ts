/**
 * DLMM (bin-based AMM) features.
 *
 * A swap has to name the bin arrays it walks (`binArrayIndexes`, ascending when
 * selling A and descending when selling B); `binArrayIndexesFor` derives the
 * array of a bin and `binArrayFor`/`positionFor` load state by pair.
 */
import type { PublicKey } from '@solana/web3.js'
import {
  dlmmAddLiquidity,
  dlmmInitializeBinArray,
  dlmmInitializePool,
  dlmmRemoveLiquidity,
  dlmmSetAuthority,
  dlmmSetPaused,
  dlmmSwap,
  dlmmTripBreaker,
  type DlmmAddLiquidityArgs,
  type DlmmInitializeBinArrayArgs,
  type DlmmInitializePoolArgs,
  type DlmmRemoveLiquidityArgs,
  type DlmmSwapArgs,
} from '../core/instructions-dlmm'
import {
  dlmmBinArray,
  dlmmBinArrayForBin,
  dlmmBinArrayIndex,
  dlmmPool,
  dlmmPosition,
  dlmmVaultA,
  dlmmVaultB,
} from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'
import { railWiring } from '../wiring'

export function createDlmmModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    initializePool: (args: DlmmInitializePoolArgs, tx?: TxOptions) =>
      sendBuilt(ctx, dlmmInitializePool(args), tx),

    initializeBinArray: (args: DlmmInitializeBinArrayArgs, tx?: TxOptions) =>
      sendBuilt(ctx, dlmmInitializeBinArray(args), tx),

    addLiquidity: (args: DlmmAddLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, dlmmAddLiquidity(args), tx),

    removeLiquidity: (args: DlmmRemoveLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, dlmmRemoveLiquidity(args), tx),

    swap: (args: DlmmSwapArgs, tx?: TxOptions) => sendBuilt(ctx, dlmmSwap(args), tx),

    tripBreaker: (pool: PublicKey, tx?: TxOptions) => sendBuilt(ctx, dlmmTripBreaker(pool), tx),

    setPaused: (pool: PublicKey, authority: PublicKey, paused: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, dlmmSetPaused(pool, authority, paused), tx),

    setAuthority: (
      pool: PublicKey,
      authority: PublicKey,
      newAuthority: PublicKey,
      tx?: TxOptions,
    ) => sendBuilt(ctx, dlmmSetAuthority(pool, authority, newAuthority), tx),

    build: {
      initializePool: dlmmInitializePool,
      initializeBinArray: dlmmInitializeBinArray,
      addLiquidity: dlmmAddLiquidity,
      removeLiquidity: dlmmRemoveLiquidity,
      swap: dlmmSwap,
      tripBreaker: dlmmTripBreaker,
      setPaused: dlmmSetPaused,
      setAuthority: dlmmSetAuthority,
    },

    // ---------------------------------------------------------------- reads

    pool: (address: PublicKey) => accounts.dlmmPool(address),
    poolFor: (mintA: PublicKey, mintB: PublicKey) => accounts.dlmmPool(dlmmPool(mintA, mintB)),
    /** The rails this pool runs, ready for `swap`'s `wiring` argument. */
    wiringFor: (mintA: PublicKey, mintB: PublicKey) =>
      accounts.dlmmPool(dlmmPool(mintA, mintB)).then(railWiring),
    position: (address: PublicKey) => accounts.binPosition(address),
    positionFor: (owner: PublicKey, mintA: PublicKey, mintB: PublicKey, binId: number) =>
      accounts.binPosition(dlmmPosition(dlmmPool(mintA, mintB), owner, binId)),

    /** DLMM bin arrays have no decoder yet; this is their address. */
    binArrayFor: (mintA: PublicKey, mintB: PublicKey, index: number | bigint) =>
      dlmmBinArray(dlmmPool(mintA, mintB), index),
    binArrayForBin: (mintA: PublicKey, mintB: PublicKey, binId: number) =>
      dlmmBinArrayForBin(dlmmPool(mintA, mintB), binId),

    /** The container array's index for a bin id (70 bins per array). */
    binArrayIndex: dlmmBinArrayIndex,
    /**
     * Array indexes for a swap walk: every container from `span` arrays below to
     * `span` above the bin's own, ascending. Which half a trade needs depends on
     * its direction and size; the program stops at the first unused array.
     */
    binArrayIndexesFor: (binId: number, span = 0) => {
      const center = Number(dlmmBinArrayIndex(binId))
      const indexes: number[] = []
      for (let index = center - span; index <= center + span; index++) indexes.push(index)
      return indexes
    },

    addressesFor: (mintA: PublicKey, mintB: PublicKey) => {
      const pool = dlmmPool(mintA, mintB)
      return { pool, vaultA: dlmmVaultA(pool, mintA), vaultB: dlmmVaultB(pool, mintB) }
    },
  }
}

export type DlmmModule = ReturnType<typeof createDlmmModule>
