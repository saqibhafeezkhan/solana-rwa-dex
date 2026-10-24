import { describe, expect, test } from 'bun:test'
import { binPrice } from '../src/ui/pricing'

describe('binPrice', () => {
  test('bin zero is parity', () => {
    expect(binPrice(0, 10)).toBe(1)
  })

  test('a bin spans binStep ticks of 1.0001', () => {
    expect(binPrice(10, 10)).toBeCloseTo(1.0001 ** 100, 12)
    expect(binPrice(-1, 1)).toBeCloseTo(1 / 1.0001, 12)
  })
})
