/**
 * Whitelist helpers on top of the shared Keccak Merkle tree.
 *
 * The instruction builders want proofs as `Uint8Array[]`, while the tree
 * builder hands back hex strings (the same form `update_root` and the UI show).
 * These helpers bridge the two so callers never hand-convert.
 */
import type { PublicKey } from '@solana/web3.js'
import { buildWhitelistTree } from './core/merkle'

export function hexToBytes(hex: string): Uint8Array {
  const clean = hex.startsWith('0x') ? hex.slice(2) : hex
  if (clean.length % 2 !== 0) throw new Error('hex string must have an even length')
  const bytes = new Uint8Array(clean.length / 2)
  for (let i = 0; i < bytes.length; i++) {
    const byte = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16)
    if (Number.isNaN(byte)) throw new Error(`invalid hex byte at index ${i}`)
    bytes[i] = byte
  }
  return bytes
}

/** A built whitelist: the root to publish and each member's path in bytes. */
export interface Whitelist {
  /** 32-byte root, ready for `transferHook.updateRoot`. */
  root: Uint8Array
  rootHex: string
  /** The path for one member, ready for `transferHook.setWhitelistEntry`. */
  proofFor(owner: PublicKey): Uint8Array[]
}

/**
 * Builds the tree the transfer hook checks. Order of `owners` does not matter:
 * leaves are sorted by hash, so the same member set always yields the same root.
 */
export function buildWhitelist(owners: PublicKey[]): Whitelist {
  const tree = buildWhitelistTree(owners)
  return {
    root: hexToBytes(tree.root),
    rootHex: tree.root,
    proofFor: (owner) => tree.proofFor(owner).map(hexToBytes),
  }
}

/**
 * The proofs a single RWA transfer needs, for its two sides. Both sides are
 * checked by the hook, so both need an entry — including an escrow owner when
 * the transfer is a settlement move.
 */
export function buildTransferProofs(
  owners: PublicKey[],
  source: PublicKey,
  destination: PublicKey,
): { root: Uint8Array; rootHex: string; sourceProof: Uint8Array[]; destinationProof: Uint8Array[] } {
  const tree = buildWhitelist(owners)
  return {
    root: tree.root,
    rootHex: tree.rootHex,
    sourceProof: tree.proofFor(source),
    destinationProof: tree.proofFor(destination),
  }
}

