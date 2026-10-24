import { describe, expect, test } from 'bun:test'
import { Keypair } from '@solana/web3.js'
import { buildWhitelistTree, bytesToHex, leafHash, nodeHash } from '../src/core/merkle'

const hexToBytes = (hex: string) => Uint8Array.from(Buffer.from(hex, 'hex'))

/** Folds one leaf's path exactly as the on-chain `root_from_proof` does. */
function foldProof(leaf: Uint8Array, proof: string[]): string {
  let current = leaf
  for (const sibling of proof) current = nodeHash(current, hexToBytes(sibling))
  return bytesToHex(current)
}

describe('whitelist Merkle tree', () => {
  const owners = [Keypair.generate().publicKey, Keypair.generate().publicKey, Keypair.generate().publicKey]

  test('orders leaves canonically, so input order cannot change the root', () => {
    expect(buildWhitelistTree([...owners].reverse()).root).toBe(buildWhitelistTree(owners).root)
  })

  test('every generated proof folds back to the published root', () => {
    const tree = buildWhitelistTree(owners)
    for (const owner of owners) {
      expect(foldProof(leafHash(owner), tree.proofFor(owner))).toBe(tree.root)
    }
  })

  test('a single-member tree needs no proof', () => {
    const tree = buildWhitelistTree([owners[0]])
    expect(tree.proofFor(owners[0])).toEqual([])
    expect(bytesToHex(leafHash(owners[0]))).toBe(tree.root)
  })
})
