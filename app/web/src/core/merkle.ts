/**
 * Merkle whitelist tree for the transfer-hook program.
 *
 * The on-chain hook hashes with **Keccak-256** (not SHA-256, and not SHA3-256 —
 * they differ in padding), using RFC 6962-style domain separation:
 *   leaf   = keccak(0x00 || pubkey)
 *   parent = keccak(0x01 || min(left, right) || max(left, right))
 *
 * `@noble/hashes` is the same library `scripts/seed-localnet.ts` uses to build
 * the whitelist tree, and both builders order leaves by hash, so the UI and the
 * seeder produce the same root for the same member set. The on-chain hook does
 * not rebuild the tree: it just folds the stored path from the configured root.
 *
 * This lives in `core` (not in the proof-builder component) because it is
 * protocol-critical: both the proof builder and the RWA transfer form rely on
 * producing the exact same root the hook checks.
 */
import { PublicKey } from '@solana/web3.js'
import { keccak_256 } from '@noble/hashes/sha3'

export function keccak256(data: Uint8Array): Uint8Array {
  return keccak_256(data)
}

/** `keccak(0x00 || pubkey)` — a leaf, domain-separated from inner nodes. */
export function leafHash(owner: PublicKey): Uint8Array {
  const prefix = new Uint8Array([0x00])
  const ownerBytes = owner.toBytes()
  const input = new Uint8Array(prefix.length + ownerBytes.length)
  input.set(prefix)
  input.set(ownerBytes, prefix.length)
  return keccak256(input)
}

/**
 * Byte-wise comparison of two hashes: negative, zero or positive.
 *
 * Not `<`: comparing two `Uint8Array`s with a relational operator coerces them
 * to strings (`"18,52,…"`) and compares those, which is a *different* order from
 * the program's `sort_unstable` over `[u8; 32]`. Rust folds each provided
 * sibling as `keccak(0x01 ‖ min ‖ max)` in byte order, so the pair order here
 * must match or every proof fails as `SourceNotWhitelisted` /
 * `DestinationNotWhitelisted` (or `ProofNotInWhitelist` for a registration) while
 * looking perfectly consistent here.
 */
export function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const shared = Math.min(a.length, b.length)
  for (let i = 0; i < shared; i++) {
    if (a[i] !== b[i]) return a[i] - b[i]
  }
  return a.length - b.length
}

/** `keccak(0x01 || min(l, r) || max(l, r))` — position-free parent. */
export function nodeHash(left: Uint8Array, right: Uint8Array): Uint8Array {
  const prefix = new Uint8Array([0x01])
  // Sort the pair so proofs stay position-free (RFC 6962), in byte order, which
  // is the order `transfer_hook::root_from_proof` sorts in.
  const sorted = compareBytes(left, right) <= 0 ? [left, right] : [right, left]
  const combined = new Uint8Array(prefix.length + sorted[0].length + sorted[1].length)
  combined.set(prefix)
  combined.set(sorted[0], prefix.length)
  combined.set(sorted[1], prefix.length + sorted[0].length)
  return keccak256(combined)
}

export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('')
}

/** A built whitelist tree: its root and a way to get any member's path. */
export interface WhitelistTree {
  /** Hex root to publish with `update_root`. */
  root: string
  /** The sibling path for one member, leaf-adjacent first (hex, 32 bytes each). */
  proofFor(owner: PublicKey): string[]
}

/**
 * Build a Merkle tree from a list of owner pubkeys.
 *
 * The tree uses the same rule as the Rust hook: a duplicated last node at any
 * odd level hashes with itself, and the proof for that node repeats the same
 * hash on the other side.
 *
 * `proofFor` exists because a tree is rarely just two members: registering an
 * entry is one call per member, and an escrow whose owner has to be whitelisted
 * for a settlement transfer (the hook checks both sides) is a member too.
 */
export function buildWhitelistTree(owners: PublicKey[]): WhitelistTree {
  if (owners.length === 0) throw new Error('whitelist is empty')

  // Leaves.
  const leaves = owners.map((owner) => {
    const h = leafHash(owner)
    return { hash: h, owner }
  })

  // Sort by hash so the tree is deterministic irrespective of input order.
  leaves.sort((a, b) => compareBytes(a.hash, b.hash))

  // Build levels bottom-up.
  type LevelNode = { hash: Uint8Array; isDuplicate: boolean }
  let level: LevelNode[] = leaves.map((l) => ({ hash: l.hash, isDuplicate: false }))

  const levelHashes: Uint8Array[][] = [leaves.map((l) => l.hash)]

  while (level.length > 1) {
    const next: LevelNode[] = []
    for (let i = 0; i < level.length; i += 2) {
      if (i === level.length - 1) {
        // Odd node: hash with itself.
        const h = nodeHash(level[i].hash, level[i].hash)
        next.push({ hash: h, isDuplicate: true })
      } else {
        const h = nodeHash(level[i].hash, level[i + 1].hash)
        next.push({ hash: h, isDuplicate: false })
      }
    }
    level = next
    levelHashes.push(level.map((n) => n.hash))
  }

  const root = level[0].hash

  // Walk back up from each leaf to produce the proof.
  function proofFor(owner: PublicKey): string[] {
    // Find the leaf index by matching owner (the leaf list was sorted by hash,
    // so find the matching hash for this owner).
    const leafHashBytes = leafHash(owner)
    let idx = -1
    for (let i = 0; i < leaves.length; i++) {
      if (leaves[i].hash.length === leafHashBytes.length &&
          leaves[i].hash.every((v, j) => v === leafHashBytes[j])) {
        idx = i
        break
      }
    }
    if (idx === -1) throw new Error('owner not in whitelist')

    const proof: string[] = []
    let currentIndex = idx

    for (let depth = 0; depth < levelHashes.length - 1; depth++) {
      const nodes = levelHashes[depth]
      const isLeft = currentIndex % 2 === 0
      const siblingIndex = isLeft ? currentIndex + 1 : currentIndex - 1

      if (siblingIndex >= nodes.length) {
        // Duplicate node: the sibling is the same hash.
        proof.push(bytesToHex(nodes[currentIndex]))
      } else {
        proof.push(bytesToHex(nodes[siblingIndex]))
      }

      currentIndex = Math.floor(currentIndex / 2)
    }

    return proof
  }

  return { root: bytesToHex(root), proofFor }
}

/**
 * Build a Merkle tree from a list of owner pubkeys and return the proofs for
 * `source` and `destination` — the two sides a transfer needs.
 */
export function buildTreeAndProve(
  owners: PublicKey[],
  source: PublicKey,
  destination: PublicKey,
): { root: string; sourceProof: string[]; destinationProof: string[] } {
  const tree = buildWhitelistTree(owners)
  return {
    root: tree.root,
    sourceProof: tree.proofFor(source),
    destinationProof: tree.proofFor(destination),
  }
}
