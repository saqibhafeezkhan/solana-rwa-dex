/**
 * Keccak-256 for the Merkle proof builder.
 *
 * The transfer-hook program hashes with **Keccak-256** (not SHA-256, and not
 * SHA3-256 — they differ in padding), using RFC 6962-style domain separation:
 *   leaf   = keccak(0x00 || pubkey)
 *   parent = keccak(0x01 || min(l, r) || max(l, r))
 *
 * `@noble/hashes` is the same library `scripts/seed-localnet.ts` uses to build
 * the whitelist tree, so the UI and the seeder agree byte for byte.
 */
import { keccak_256 } from '@noble/hashes/sha3'

export function keccak256(data: Uint8Array): Uint8Array {
  return keccak_256(data)
}

/** Keccak-256 is bundled, so it is always available. */
export function hasKeccak(): boolean {
  return true
}
