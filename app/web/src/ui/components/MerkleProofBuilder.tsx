import { useMemo, useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { keccak256 } from './KeccakShim'

// Re-export from components barrel - since this file is in the components dir,
// we reference the barrel file directly.
import { Panel, Field, TextInput, ActionButton, FormRow, Badge, EmptyHint } from '../components'

function copyToClipboard(text: string) {
  navigator.clipboard.writeText(text).catch(() => {})
}


/**
 * Merkle proof builder for the transfer-hook whitelist.
 *
 * Domain-separation prefixes (RFC 6962 style, Keccak-256):
 *   leaf   = keccak(0x00 || pubkey)
 *   parent = keccak(0x01 || min(left, right) || max(left, right))
 *
 * The builder uses the KeccakShim for hashing. A Keccak library must be loaded
 * (see KeccakShim.ts for options).
 */

function leafHash(owner: PublicKey): Uint8Array {
  const prefix = new Uint8Array([0x00])
  const ownerBytes = owner.toBytes()
  const input = new Uint8Array(prefix.length + ownerBytes.length)
  input.set(prefix)
  input.set(ownerBytes, prefix.length)
  return keccak256(input)
}

function nodeHash(left: Uint8Array, right: Uint8Array): Uint8Array {
  const prefix = new Uint8Array([0x01])
  // Sort the pair so proofs stay position-free (RFC 6962).
  const sorted = left < right ? [left, right] : [right, left]
  const combined = new Uint8Array(prefix.length + sorted[0].length + sorted[1].length)
  combined.set(prefix)
  combined.set(sorted[0], prefix.length)
  combined.set(sorted[1], prefix.length + sorted[0].length)
  return keccak256(combined)
}

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, '0'))
    .join('')
}

/**
 * Build a Merkle tree from a list of owner pubkeys and return the proofs for
 * `source` and `destination`.
 *
 * The tree uses the same rule as the Rust hook: a duplicated last node at any
 * odd level hashes with itself, and the proof for that node repeats the same
 * hash on the other side.
 */
function buildTreeAndProve(
  owners: PublicKey[],
  source: PublicKey,
  destination: PublicKey,
): { root: string; sourceProof: string[]; destinationProof: string[] } {
  if (owners.length === 0) throw new Error('whitelist is empty')

  // Leaves.
  const leaves = owners.map((owner) => {
    const h = leafHash(owner)
    return { hash: h, owner }
  })

  // Sort by hash so the tree is deterministic irrespective of input order.
  leaves.sort((a, b) => {
    for (let i = 0; i < a.hash.length; i++) {
      if (a.hash[i] !== b.hash[i]) return a.hash[i] - b.hash[i]
    }
    return 0
  })

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

  return {
    root: bytesToHex(root),
    sourceProof: proofFor(source),
    destinationProof: proofFor(destination),
  }
}

export function MerkleProofBuilder({
  whitelist,
  sourceWallet,
  destinationWallet,
}: {
  /** Whitelisted institution pubkeys (the tree leaves). */
  whitelist: PublicKey[]
  sourceWallet: PublicKey
  destinationWallet: PublicKey
}) {
  const [sourceProof, setSourceProof] = useState<string[]>([])
  const [destProof, setDestProof] = useState<string[]>([])
  const [root, setRoot] = useState<string | null>(null)

  // Compute inside `useMemo` without touching state: a `setState` during render
  // is a side effect (and an error thrown here would take the whole page down).
  const { result, buildError } = useMemo(() => {
    if (whitelist.length === 0 || !sourceWallet || !destinationWallet) {
      return { result: null, buildError: null as string | null }
    }
    try {
      return { result: buildTreeAndProve(whitelist, sourceWallet, destinationWallet), buildError: null }
    } catch (err) {
      return { result: null, buildError: (err as Error).message }
    }
  }, [whitelist, sourceWallet, destinationWallet])

  const apply = () => {
    if (!result) return
    setSourceProof(result.sourceProof)
    setDestProof(result.destinationProof)
    setRoot(result.root)
  }

  const clear = () => {
    setSourceProof([])
    setDestProof([])
    setRoot(null)
  }

  // Proofs are already hex strings; join them directly.
  const sourceHex = sourceProof.length > 0 ? sourceProof.join(', ') : ''
  const destHex = destProof.length > 0 ? destProof.join(', ') : ''

  if (whitelist.length === 0) {
    return (
      <Panel
        title="Merkle proof builder"
        subtitle="Whitelist is empty — add institutions on the Enterprise page first, then come back here to build the tree."
      >
        <EmptyHint>No whitelisted wallets to build a tree from.</EmptyHint>
      </Panel>
    )
  }

  return (
    <Panel
      title="Merkle proof builder"
      subtitle="Press Build proofs to compute the Keccak leaf proofs for the whitelisted sender and recipient shown above, then copy each side's hex into the RWA transfer form on the assets page — the hook checks source AND destination membership, and the computed root must match the one published in the transfer-hook config or the transfer is rejected."
      aside={
        root ? (
          <Badge tone="good">tree built · {whitelist.length} leaves</Badge>
        ) : (
          <Badge tone="warn">ready</Badge>
        )
      }
    >
      <div className="grid-2">
        <div className="form">
          <FormRow>
            <Field label="Source wallet (sender)">
              <TextInput readOnly value={sourceWallet.toBase58()} />
            </Field>
            <Field label="Destination wallet (recipient)">
              <TextInput readOnly value={destinationWallet.toBase58()} />
            </Field>
          </FormRow>
          <ActionButton label="Build proofs" onClick={apply} disabled={!result} />
          <ActionButton label="Clear" variant="secondary" onClick={clear} />
        </div>

        <div className="form">
          <Field label="Whitelist size">
            <TextInput readOnly value={`${whitelist.length} institutions`} />
          </Field>
          <Field label="Tree depth">
            <TextInput readOnly value={`${Math.ceil(Math.log2(whitelist.length)) + 1} levels`} />
          </Field>
        </div>
      </div>

      {buildError ? (
        <p className="status-error" style={{ marginTop: 12 }}>{buildError}</p>
      ) : result ? (
        <>
          <div className="grid-2" style={{ marginTop: 16 }}>
            <Panel title="Source proof (sender)">
              <div className="form">
                <Field label="Proof elements (32-byte hex, comma-separated)">
                  <TextInput readOnly value={sourceHex || '—'} />
                </Field>
                <ActionButton label="Copy" variant="secondary" onClick={() => copyToClipboard(sourceHex)} disabled={!sourceHex} />
              </div>
              {sourceProof.length > 0 ? (
                <div className="kv" style={{ marginTop: 8 }}>
                  {sourceProof.map((elem, i) => (
                    <div key={i} className="kv-row">
                      <span>Node {i + 1}</span>
                      <code className="mono" style={{ fontSize: 12 }}>
                        {elem.slice(0, 8)}…{elem.slice(-8)}
                      </code>
                      <button
                        className="btn btn-secondary btn-compact"
                        style={{ marginLeft: 8 }}
                        onClick={() => copyToClipboard(elem)}
                        title="Copy element"
                      >
                        copy
                      </button>
                    </div>
                  ))}
                </div>
              ) : null}
            </Panel>

            <Panel title="Destination proof (recipient)">
              <div className="form">
                <Field label="Proof elements (32-byte hex, comma-separated)">
                  <TextInput readOnly value={destHex || '—'} />
                </Field>
                <ActionButton label="Copy" variant="secondary" onClick={() => copyToClipboard(destHex)} disabled={!destHex} />
              </div>
              {destProof.length > 0 ? (
                <div className="kv" style={{ marginTop: 8 }}>
                  {destProof.map((elem, i) => (
                    <div key={i} className="kv-row">
                      <span>Node {i + 1}</span>
                      <code className="mono" style={{ fontSize: 12 }}>
                        {elem.slice(0, 8)}…{elem.slice(-8)}
                      </code>
                      <button
                        className="btn btn-secondary btn-compact"
                        style={{ marginLeft: 8 }}
                        onClick={() => copyToClipboard(elem)}
                        title="Copy element"
                      >
                        copy
                      </button>
                    </div>
                  ))}
                </div>
              ) : null}
            </Panel>
          </div>

          <div style={{ marginTop: 16 }}>
            <Field label="Computed Merkle root">
              <TextInput readOnly value={root || '—'} />
            </Field>
            <ActionButton label="Copy root" variant="secondary" onClick={() => copyToClipboard(root || '')} disabled={!root} />
            <p className="viz-note" style={{ marginTop: 8 }}>
              This root must match the one published in the transfer-hook config for the transfer to be approved.
              The same two prefixes are used everywhere: leaf = keccak(0x00 || pubkey), node = keccak(0x01 || min || max).
            </p>
          </div>
        </>
      ) : null}
    </Panel>
  )
}

/**
 * Lightweight tree visualizer for the whitelist. Shows leaf hashes and the
 * root only — full tree rendering gets large fast, so we keep it compact.
 */
export function MerkleTreeVisualizer({
  owners,
}: {
  owners: PublicKey[]
}) {
  const tree = useMemo(() => {
    if (owners.length === 0) return null
    const leaves = owners.map((owner) => ({
      owner,
      hash: bytesToHex(leafHash(owner)).slice(0, 16) + '…',
    }))
    leaves.sort((a, b) => (a.owner.toBase58() < b.owner.toBase58() ? -1 : 1))
    return {
      leaves,
      root: '…',
      size: owners.length,
    }
  }, [owners])

  if (!tree) return null

  return (
    <div className="merkle-tree" style={{ marginTop: 12 }}>
      <div className="merkle-tree-label">Whitelist ({tree.size} leaves)</div>
      <div className="merkle-leaves">
        {tree.leaves.map((leaf) => (
          <div key={leaf.owner.toBase58()} className="merkle-leaf" title={leaf.owner.toBase58()}>
            <span className="merkle-leaf-hash">{leaf.hash}</span>
            <span className="merkle-leaf-owner">{leaf.owner.toBase58().slice(0, 4)}…{leaf.owner.toBase58().slice(-4)}</span>
          </div>
        ))}
      </div>
      <p className="viz-note">
        One leaf per whitelisted institution, hashed as keccak(0x00 || pubkey) and ordered by pubkey. A transfer is approved
        only when both the sender and the recipient appear in this list.
      </p>
    </div>
  )
}
