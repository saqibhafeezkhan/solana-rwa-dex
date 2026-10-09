import { useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../AppContext'
import { ActionButton, Badge, DataTable, EmptyHint, Field, FormRow, KeyValue, Panel, TextInput } from '../components'
import { useAccountState } from '../useAccountState'
import { loadMint } from '../../core/decoders'
import { detectIdentifiers, loadTokenMetadata } from '../../core/metadata'

export function AuditPage() {
  const { connection, cluster, publicKey } = useApp()
  const [mint, setMint] = useState('')

  const signatures = useAccountState(
    async () => {
      if (!publicKey) return []
      return connection.getSignaturesForAddress(publicKey, { limit: 25 })
    },
    [connection, publicKey?.toBase58()],
  )

  // Block hashes are not returned by getSignaturesForAddress; fetch them for the
  // most recent rows (bounded, history can be pruned on public RPCs).
  const blockhashes = useAccountState(
    async () => {
      const list = signatures.state
      const map: Record<string, string> = {}
      if (!list || list.length === 0) return map
      await Promise.all(
        list.slice(0, 10).map(async (entry) => {
          try {
            const tx = await connection.getTransaction(entry.signature, { maxSupportedTransactionVersion: 0 })
            const message = tx?.transaction?.message as { recentBlockhash?: string } | undefined
            if (message?.recentBlockhash) map[entry.signature] = message.recentBlockhash
          } catch {
            // signature pruned or not yet indexed — leave blank
          }
        }),
      )
      return map
    },
    [connection, signatures.state],
  )

  const mintKey = (() => {
    try {
      return mint ? new PublicKey(mint) : null
    } catch {
      return null
    }
  })()

  const mintState = useAccountState(
    async () => {
      if (!mintKey) return null
      try {
        return await loadMint(connection, mintKey)
      } catch {
        return null
      }
    },
    [connection, mintKey?.toBase58()],
  )

  const metadata = useAccountState(
    async () => {
      if (!mintKey) return null
      try {
        return await loadTokenMetadata(connection, mintKey)
      } catch {
        return null
      }
    },
    [connection, mintKey?.toBase58()],
  )

  const rows = signatures.state ?? []
  const identifiers = detectIdentifiers(
    metadata.state ? `${metadata.state.name ?? ''} ${metadata.state.symbol ?? ''} ${metadata.state.uri ?? ''}` : '',
  )

  const explorerUrl = (signature: string) =>
    `https://explorer.solana.com/tx/${signature}${cluster === 'mainnet-beta' ? '' : `?cluster=${cluster}`}`

  return (
    <div className="audit-page">
      <Panel
        title="On-chain audit trail"
        subtitle="Every transaction this wallet signed, straight from the RPC, with slot, status and block hash — exportable as CSV or a printable statement."
        aside={
          <div className="proposal-actions">
            <Badge tone={signatures.error ? 'bad' : signatures.loading ? 'warn' : 'good'}>
              {signatures.error ? 'load failed' : signatures.loading ? 'loading…' : `${rows.length} txn`}
            </Badge>
            <ActionButton label="Rescan" variant="secondary" onClick={() => void signatures.refresh()} disabled={signatures.loading} />
            <ActionButton label="Print / PDF" variant="secondary" onClick={() => window.print()} disabled={rows.length === 0} />
          </div>
        }
      >
        {!publicKey ? <EmptyHint>Connect a wallet to build its audit trail.</EmptyHint> : null}
        <DataTable
          csvName={`rwadex-audit-${publicKey ? publicKey.toBase58().slice(0, 6) : 'wallet'}`}
          empty="No transactions found for this wallet on this cluster."
          rows={rows}
          columns={[
            {
              header: 'Signature',
              cell: (row) => (
                <a className="mono" href={explorerUrl(row.signature)} target="_blank" rel="noreferrer" title={row.signature}>
                  {row.signature.slice(0, 10)}…
                </a>
              ),
              text: (row) => row.signature,
            },
            { header: 'Slot', cell: (row) => row.slot.toString(), text: (row) => row.slot.toString() },
            {
              header: 'Block hash',
              cell: (row) => {
                const hash = blockhashes.state?.[row.signature]
                return hash ? (
                  <span className="mono" title={hash}>
                    {hash.slice(0, 10)}…
                  </span>
                ) : (
                  '—'
                )
              },
              text: (row) => blockhashes.state?.[row.signature] ?? '',
            },
            {
              header: 'Time',
              cell: (row) => (row.blockTime ? new Date(row.blockTime * 1000).toLocaleString() : '—'),
              text: (row) => (row.blockTime ? new Date(row.blockTime * 1000).toISOString() : ''),
            },
            {
              header: 'Status',
              cell: (row) => (row.err ? <Badge tone="bad">failed</Badge> : <Badge tone="good">success</Badge>),
              text: (row) => (row.err ? 'failed' : 'success'),
            },
          ]}
        />
        <p className="viz-note">
          Block hashes are fetched for the {Math.min(rows.length, 10)} most recent transactions only (older ones may be
          pruned by the RPC). Merkle proof verification is not shown here because proofs are only attached to transfer
          instructions, which the RPC does not decode.
        </p>
      </Panel>

      <Panel
        title="NAV attestation viewer"
        subtitle="Reads the Token-2022 metadata pointer: legal name, ticker and the off-chain document link an RWA mint publishes."
      >
        <FormRow>
          <Field label="RWA mint" hint="Any Token-2022 mint carrying the TokenMetadata extension.">
            <TextInput value={mint} onChange={(e) => setMint(e.target.value)} placeholder="Base58 mint" />
          </Field>
        </FormRow>

        {metadata.state ? (
          <>
            <KeyValue
              rows={[
                ['Legal name', metadata.state.name ?? '—'],
                ['Ticker', metadata.state.symbol ?? '—'],
                [
                  'Metadata URI',
                  metadata.state.uri ? (
                    <a key="uri" href={metadata.state.uri} target="_blank" rel="noreferrer" className="mono">
                      {metadata.state.uri}
                    </a>
                  ) : (
                    '—'
                  ),
                ],
                ['Decimals', mintState.state ? mintState.state.decimals.toString() : '—'],
                ['Total supply', mintState.state ? mintState.state.supply.toString() : '—'],
                ['Detected ISIN', identifiers.isin ?? '—'],
                ['Detected CUSIP', identifiers.cusip ?? '—'],
              ]}
            />
            <p className="viz-note">
              ISIN/CUSIP are detected from the on-chain metadata text; the authoritative attestation document is whatever
              the URI above points to.
            </p>
          </>
        ) : (
          <EmptyHint>{mintKey ? 'No TokenMetadata found on this mint.' : 'Enter an RWA mint to inspect.'}</EmptyHint>
        )}
      </Panel>
    </div>
  )
}
