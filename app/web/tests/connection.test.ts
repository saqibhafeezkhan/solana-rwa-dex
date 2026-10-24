import { describe, expect, test } from 'bun:test'
import type { Connection } from '@solana/web3.js'
import { confirmTx } from '../src/core/connection'

function fakeConnection(result: { value: { err: unknown; logs?: string[] } }): Connection {
  return {
    confirmTransaction: async () => ({ context: { slot: 1 }, value: result.value }),
  } as unknown as Connection
}

describe('confirmTx', () => {
  test('throws when the transaction landed with an on-chain error', async () => {
    const connection = fakeConnection({
      value: { err: { InstructionError: [0, { Custom: 6000 }] } },
    })
    await expect(confirmTx(connection, 'sig', 'blockhash', 42)).rejects.toThrow(/failed/)
  })

  test('returns the confirmation when the transaction succeeded', async () => {
    const connection = fakeConnection({ value: { err: null } })
    const result = await confirmTx(connection, 'sig', 'blockhash', 42)
    expect(result.value.err).toBeNull()
  })
})
