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

/** Geometric bin price where bin 0 = 1.0 and one bin step is `binStepBps`. */
export function binPrice(activeBinId: number, binStepBps: number): number {
  return (1 + (binStepBps > 0 ? binStepBps : 10) / 10_000) ** activeBinId
}
