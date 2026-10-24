import type { ClmmPoolState } from '../core/decoders'

/** Price conversions shared by the pool pages' oracle/risk read-outs. */

/** Oracle observations store sqrt(price) as Q64.64; square it back to a price. */
export function sqrtPriceToPrice(sqrtX64: bigint): number {
  const sqrt = Number(sqrtX64) / 2 ** 64
  return sqrt * sqrt
}

/**
 * Maverick bins store raw token A per raw token B as Q64.64 — a price, not a
 * square root, so unlike the oracle's observations there is nothing to square.
 */
export function q64ToPrice(priceX64: bigint): number {
  return Number(priceX64) / 2 ** 64
}

/**
 * Geometric bin price where bin 0 = 1.0. A bin spans `binStepBps` ticks and
 * each tick is one basis point wide, so the on-chain step is
 * `1.0001 ^ (bin * binStepBps)` — compounding, not a linear `binStep/10_000`
 * addition.
 */
export function binPrice(activeBinId: number, binStepBps: number): number {
  const step = binStepBps > 0 ? binStepBps : 10
  return 1.0001 ** (activeBinId * step)
}

/**
 * Token A / token B currently backing a CLMM position, given the pool's live
 * price. A range entirely below the price holds only A; entirely above holds
 * only B; a straddling range holds both.
 */
export function clmmAmounts(
  pool: Pick<ClmmPoolState, 'sqrtPriceX64'>,
  lowerTick: number,
  upperTick: number,
  liquidity: bigint,
): string {
  // Tick -> price ratio is 1.0001^tick; sqrt(P) Q64.64 -> price. Divide the
  // full value (not the integer part) or any price below 1 collapses to 0.
  const sqrtPrice = Number(pool.sqrtPriceX64) / 2 ** 64
  const price = sqrtPrice * sqrtPrice
  const lowerPrice = 1.0001 ** lowerTick
  const upperPrice = 1.0001 ** upperTick
  const liquidityNum = Number(liquidity)
  if (liquidityNum === 0) return '0 A · 0 B'
  // Approximate amounts from the v3 formulas, displayed not trusted.
  if (price <= lowerPrice) {
    return `${((liquidityNum * (1 / Math.sqrt(lowerPrice) - 1 / Math.sqrt(upperPrice))) / 1e6).toPrecision(4)} A · 0 B`
  }
  if (price >= upperPrice) {
    return `0 A · ${((liquidityNum * (Math.sqrt(upperPrice) - Math.sqrt(lowerPrice))) / 1e6).toPrecision(4)} B`
  }
  const amountA = (liquidityNum * (1 / Math.sqrt(price) - 1 / Math.sqrt(upperPrice))) / 1e6
  const amountB = (liquidityNum * (Math.sqrt(price) - Math.sqrt(lowerPrice))) / 1e6
  return `${amountA.toPrecision(4)} A · ${amountB.toPrecision(4)} B`
}
