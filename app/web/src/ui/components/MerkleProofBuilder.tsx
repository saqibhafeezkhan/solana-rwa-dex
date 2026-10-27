import { useMemo, useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { buildTreeAndProve, bytesToHex, compareBytes, leafHash } from '../../core/merkle'

// Re-export from components barrel - since this file is in the components dir,
// we reference the barrel file directly.
import { Panel, Field, TextInput, ActionButton, FormRow, Badge, EmptyHint } from '../components'

function copyToClipboard(text: string) {
  navigator.clipboard.writeText(text).catch(() => {})
}


/**
 * Merkle proof builder for the transfer-hook whitelist. The tree maths
 * (domain separation, canonical ordering) lives in `core/merkle` so the RWA
 * transfer form can derive the same proofs without duplicating it.
 */

export function MerkleProofBuilder({
  whitelist,
  sourceWallet,
  destinationWallet,
  onUseProof,
}: {
  /** Whitelisted institution pubkeys (the tree leaves). */
  whitelist: PublicKey[]
  sourceWallet: PublicKey
  destinationWallet: PublicKey
  /**
   * Loads a built proof into the caller's whitelist-entry form. The transfer
   * itself carries the entry PDAs, not the proofs, so this is how a derived
   * path reaches `set_whitelist_entry` without retyping hex.
   */
  onUseProof?: (side: 'source' | 'destination', proof: string[], wallet: PublicKey) => void
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
      subtitle="Press Build proofs to compute the Keccak path for the sender and the recipient. The proofs are stored in each institution's whitelist entry (the transfer only carries the two entry accounts), so use a side's proof to fill the entry form above — or copy it. The computed root must equal the one published in the transfer-hook config, or the registration is rejected."
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
                <div className="form-row">
                  <ActionButton label="Copy" variant="secondary" onClick={() => copyToClipboard(sourceHex)} disabled={!sourceHex} />
                  {onUseProof ? (
                    <ActionButton
                      label="Use for sender entry"
                      variant="secondary"
                      onClick={() => onUseProof('source', sourceProof, sourceWallet)}
                    />
                  ) : null}
                </div>
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
                <div className="form-row">
                  <ActionButton label="Copy" variant="secondary" onClick={() => copyToClipboard(destHex)} disabled={!destHex} />
                  {onUseProof ? (
                    <ActionButton
                      label="Use for recipient entry"
                      variant="secondary"
                      onClick={() => onUseProof('destination', destProof, destinationWallet)}
                    />
                  ) : null}
                </div>
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
    const leaves = owners.map((owner) => {
      const hash = leafHash(owner)
      return { owner, hash: `${bytesToHex(hash).slice(0, 16)}…`, order: hash }
    })
    // Match the tree builder: leaves are ordered by hash, not by address.
    leaves.sort((a, b) => compareBytes(a.order, b.order))
    return {
      leaves,
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
        One leaf per whitelisted institution, hashed as keccak(0x00 || pubkey) and ordered by leaf hash — the same order
        the proof builder and the on-chain hook fold. A transfer is approved only when both the sender and the recipient
        appear in this list.
      </p>
    </div>
  )
}
