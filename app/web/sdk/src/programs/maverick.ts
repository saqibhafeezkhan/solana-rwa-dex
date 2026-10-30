/**
 * Maverick (dynamic-distribution bins) features.
 *
 * `removeLiquidity` needs the bin arrays the position's move chain runs
 * through; `hopsFor` derives them from on-chain links so they are never typed
 * by hand. `moveBins` is the permissionless keeper that advances the chain.
 */
import type { PublicKey } from '@solana/web3.js'
import {
  maverickAddLiquidity,
  maverickInitializeBinArray,
  maverickInitializePool,
  maverickMoveBins,
  maverickRemoveLiquidity,
  maverickSetAuthority,
  maverickSetPaused,
  maverickSwap,
  maverickTripBreaker,
  type MaverickAddLiquidityArgs,
  type MaverickInitializePoolArgs,
  type MaverickMoveBinsArgs,
  type MaverickRemoveLiquidityArgs,
  type MaverickSwapArgs,
} from '../core/instructions-maverick'
import {
  maverickBinArray,
  maverickBinArrayForBin,
  maverickBinArrayIndex,
  maverickPool,
  maverickPosition,
  maverickVaultA,
  maverickVaultB,
} from '../core/pdas'
import { deriveMaverickHopArrayIndexes } from '../core/walks'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'
import { railWiring } from '../wiring'

export function createMaverickModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    initializePool: (args: MaverickInitializePoolArgs, tx?: TxOptions) =>
      sendBuilt(ctx, maverickInitializePool(args), tx),

    initializeBinArray: (
      payer: PublicKey,
      pool: PublicKey,
      index: number | bigint,
      tx?: TxOptions,
    ) => sendBuilt(ctx, maverickInitializeBinArray(payer, pool, index), tx),

    addLiquidity: (args: MaverickAddLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, maverickAddLiquidity(args), tx),

    removeLiquidity: (args: MaverickRemoveLiquidityArgs, tx?: TxOptions) =>
      sendBuilt(ctx, maverickRemoveLiquidity(args), tx),

    moveBins: (args: MaverickMoveBinsArgs, tx?: TxOptions) =>
      sendBuilt(ctx, maverickMoveBins(args), tx),

    swap: (args: MaverickSwapArgs, tx?: TxOptions) => sendBuilt(ctx, maverickSwap(args), tx),

    tripBreaker: (pool: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, maverickTripBreaker(pool), tx),

    setPaused: (pool: PublicKey, authority: PublicKey, paused: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, maverickSetPaused(pool, authority, paused), tx),

    setAuthority: (
      pool: PublicKey,
      authority: PublicKey,
      newAuthority: PublicKey,
      tx?: TxOptions,
    ) => sendBuilt(ctx, maverickSetAuthority(pool, authority, newAuthority), tx),

    build: {
      initializePool: maverickInitializePool,
      initializeBinArray: maverickInitializeBinArray,
      addLiquidity: maverickAddLiquidity,
      removeLiquidity: maverickRemoveLiquidity,
      moveBins: maverickMoveBins,
      swap: maverickSwap,
      tripBreaker: maverickTripBreaker,
      setPaused: maverickSetPaused,
      setAuthority: maverickSetAuthority,
    },

    // ---------------------------------------------------------------- reads

    pool: (address: PublicKey) => accounts.maverickPool(address),
    poolFor: (mintA: PublicKey, mintB: PublicKey) => accounts.maverickPool(maverickPool(mintA, mintB)),
    /** The rails this pool runs, ready for `swap`'s `wiring` argument. */
    wiringFor: (mintA: PublicKey, mintB: PublicKey) =>
      accounts.maverickPool(maverickPool(mintA, mintB)).then(railWiring),
    binArray: (address: PublicKey) => accounts.maverickBinArray(address),
    binArrayFor: (mintA: PublicKey, mintB: PublicKey, index: number | bigint) =>
      accounts.maverickBinArray(maverickBinArray(maverickPool(mintA, mintB), index)),
    binArrayForBin: (mintA: PublicKey, mintB: PublicKey, binId: number) =>
      maverickBinArrayForBin(maverickPool(mintA, mintB), binId),
    binArrayIndex: maverickBinArrayIndex,
    /**
     * Array indexes for a swap walk: every container from `span` arrays below to
     * `span` above the bin's own, ascending. The program stops at the first
     * unused array, so a wider span than the trade crosses is harmless.
     */
    binArrayIndexesFor: (binId: number, span = 0) => {
      const center = Number(maverickBinArrayIndex(binId))
      const indexes: number[] = []
      for (let index = center - span; index <= center + span; index++) indexes.push(index)
      return indexes
    },
    position: (address: PublicKey) => accounts.binPosition(address),
    positionFor: (owner: PublicKey, mintA: PublicKey, mintB: PublicKey, binId: number) =>
      accounts.binPosition(maverickPosition(maverickPool(mintA, mintB), owner, binId)),

    /** The arrays a `removeLiquidity` of `binId` has to carry, excluding its own. */
    hopsFor: (mintA: PublicKey, mintB: PublicKey, binId: number) =>
      deriveMaverickHopArrayIndexes(ctx.connection, maverickPool(mintA, mintB), binId),

    addressesFor: (mintA: PublicKey, mintB: PublicKey) => {
      const pool = maverickPool(mintA, mintB)
      return { pool, vaultA: maverickVaultA(pool, mintA), vaultB: maverickVaultB(pool, mintB) }
    },
  }
}

export type MaverickModule = ReturnType<typeof createMaverickModule>
