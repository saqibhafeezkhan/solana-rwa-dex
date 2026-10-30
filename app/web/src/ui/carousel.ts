/**
 * Scroll arithmetic for the price tile rail.
 *
 * Kept pure so the arrow behaviour (one tile per press, clamped at both ends)
 * can be checked without a browser.
 */
export interface RailMetrics {
  scrollLeft: number
  scrollWidth: number
  clientWidth: number
}

/** How far the rail can scroll. Zero means every tile already fits. */
function railScrollRange(metrics: RailMetrics): number {
  return Math.max(0, metrics.scrollWidth - metrics.clientWidth)
}

/** Which ends the current offset sits at; either end disables its arrow. */
export function railEnds(metrics: RailMetrics): { atStart: boolean; atEnd: boolean } {
  const range = railScrollRange(metrics)
  const left = Math.max(0, Math.min(metrics.scrollLeft, range))
  return { atStart: left <= 1, atEnd: range <= 1 || left >= range - 1 }
}

/** One tile of travel: the tile itself plus the gap that follows it. */
export function railStep(tileWidth: number, gap: number): number {
  return tileWidth + gap
}

/** Where the rail lands after an arrow press, clamped to the scroll range. */
export function railTarget(metrics: RailMetrics, direction: -1 | 1, step: number): number {
  const range = railScrollRange(metrics)
  return Math.max(0, Math.min(metrics.scrollLeft + direction * step, range))
}
