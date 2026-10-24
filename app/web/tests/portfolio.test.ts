import { describe, expect, test } from 'bun:test'
import { clmmAmounts } from '../src/ui/pricing'
import type { decodeClmmPool } from '../src/core/decoders'
import { sqrtPriceX64FromPrice } from '../src/core/pdas'

const priceAt = (tick: number) => 1.0001 ** tick
const poolAt = (tick: number) =>
  ({ sqrtPriceX64: sqrtPriceX64FromPrice(priceAt(tick)) }) as unknown as ReturnType<typeof decodeClmmPool>

const LIQUIDITY = 1_000_000_000n

describe('CLMM position amounts', () => {
  const lower = -1000
  const upper = 100
  const pa = priceAt(lower)
  const pb = priceAt(upper)
  const liquidity = Number(LIQUIDITY)

  test('below the range the position is entirely token A', () => {
    const rendered = clmmAmounts(poolAt(-2000), lower, upper, LIQUIDITY)
    const shown = parseFloat(rendered)
    const expected = (liquidity * (1 / Math.sqrt(pa) - 1 / Math.sqrt(pb))) / 1e6
    expect(rendered).toContain('· 0 B')
    expect(Math.abs(shown - expected) / expected).toBeLessThan(1e-3)
  })

  test('above the range the position is entirely token B', () => {
    const rendered = clmmAmounts(poolAt(2000), lower, upper, LIQUIDITY)
    const shown = parseFloat(rendered.split('·')[1] ?? '')
    const expected = (liquidity * (Math.sqrt(pb) - Math.sqrt(pa))) / 1e6
    expect(rendered.startsWith('0 A')).toBe(true)
    expect(Math.abs(shown - expected) / expected).toBeLessThan(1e-3)
  })

  test('inside the range both legs are present', () => {
    const rendered = clmmAmounts(poolAt(0), lower, upper, LIQUIDITY)
    const [a, b] = rendered.split('·').map((part) => parseFloat(part))
    expect(a).toBeGreaterThan(0)
    expect(b).toBeGreaterThan(0)
  })
})
