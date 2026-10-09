/** Price conversions shared by the pool pages' oracle/risk read-outs. */

/** Oracle observations store sqrt(price) as Q64.64; square it back to a price. */
export function sqrtPriceToPrice(sqrtX64: bigint): number {
  const sqrt = Number(sqrtX64) / 2 ** 64
  return sqrt * sqrt
}

/** Geometric bin price where bin 0 = 1.0 and one bin step is `binStepBps`. */
export function binPrice(activeBinId: number, binStepBps: number): number {
  return (1 + (binStepBps > 0 ? binStepBps : 10) / 10_000) ** activeBinId
}
