import { useEffect, useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../AppContext'
import {
  ActionButton,
  Badge,
  DataTable,
  EmptyHint,
  Field,
  FormRow,
  KeyValue,
  NumberInput,
  Panel,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import { formatDuration, useNow } from '../useNow'
import {
  SETTLEMENT_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ata,
  settlementConfig,
  settlementVault,
} from '../../core/pdas'
import {
  decodeSettlementVault,
  loadSettlementConfig,
  loadSettlementVault,
  type SettlementVaultView,
} from '../../core/decoders'
import {
  settlementCancel,
  settlementInitialize,
  settlementInitiate,
  settlementSetHooksAuthority,
  settlementSetTransferAgent,
  settlementSettle,
} from '../../core/instructions-rails'

/** Slots a blank settlement-slot field defaults to before submission. */
const SETTLEMENT_HORIZON_SLOTS = 300

/**
 * `SettlementVault::LEN`. The config account is a different size, so a
 * `dataSize` filter isolates vaults without needing the account discriminator.
 */
const SETTLEMENT_VAULT_LEN = 162

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/^0x/, '').replace(/[^0-9a-fA-F]/g, '')
  if (clean.length % 2 !== 0) throw new Error('hex string must have an even length')
  const out = new Uint8Array(clean.length / 2)
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16)
  return out
}

function pk(value: string): PublicKey | null {
  try {
    return value ? new PublicKey(value) : null
  } catch {
    return null
  }
}

export function SettlementPage() {
  const { connection, publicKey, send } = useApp()

  const config = useAccountState(
    () => loadSettlementConfig(connection, settlementConfig()),
    [connection],
  )

  const [poolsRequireGate, setPoolsRequireGate] = useState(true)
  const [agentKeyHex, setAgentKeyHex] = useState('')
  const [hooksAuth, setHooksAuth] = useState('')

  const [tradeId, setTradeId] = useState('1')
  // Empty means "T+300 from the live slot": an absolute slot in the past is
  // refused by the program, and a hard-coded one goes stale within minutes.
  const [slots, setSlots] = useState('')
  const [amount, setAmount] = useState('1000000')
  const [mint, setMint] = useState('')
  const [sourceAta, setSourceAta] = useState('')
  const [taker, setTaker] = useState('')

  const [sigHex, setSigHex] = useState('')
  const [recoveryId, setRecoveryId] = useState('0')
  const [takerAta, setTakerAta] = useState('')
  const [tokenProgram, setTokenProgram] = useState<'spl' | 't22'>('spl')

  const tokenProgramId = tokenProgram === 'spl' ? TOKEN_PROGRAM_ID : TOKEN_2022_PROGRAM_ID
  const mintKey = pk(mint)
  const [mineOnly, setMineOnly] = useState(false)

  const vaultAddress = (() => {
    try {
      return settlementVault(BigInt(tradeId || '1'))
    } catch {
      return null
    }
  })()

  const vault = useAccountState(
    async () => {
      if (!vaultAddress) return null
      try {
        return await loadSettlementVault(connection, vaultAddress)
      } catch {
        return null
      }
    },
    [connection, vaultAddress?.toBase58()],
  )

  // Every SettlementVault on the cluster, pending first — so any trade can be
  // loaded, settled or cancelled even when this wallet did not initiate it.
  // "Mine only" narrows to escrows addressed to this wallet as taker.
  const queue = useAccountState(
    async () => {
      const filters: Array<{ dataSize: number } | { memcmp: { offset: number; bytes: string } }> = [
        { dataSize: SETTLEMENT_VAULT_LEN },
      ]
      if (mineOnly && publicKey) {
        filters.push({ memcmp: { offset: 8 + 8, bytes: publicKey.toBase58() } })
      }
      const response = await connection.getProgramAccounts(SETTLEMENT_PROGRAM_ID, {
        filters,
        encoding: 'base64',
      })
      const vaults: SettlementVaultView[] = []
      for (const item of response) {
        try {
          vaults.push(
            decodeSettlementVault({
              pubkey: item.pubkey,
              owner: item.account.owner,
              data: item.account.data,
              lamports: item.account.lamports,
            }),
          )
        } catch {
          // partially-written or undecodable vault; skip
        }
      }
      const rank = (status: string) => (status === 'pending' ? 0 : status === 'settled' ? 1 : 2)
      return vaults.sort((a, b) =>
        a.status !== b.status
          ? rank(a.status) - rank(b.status)
          : a.tradeId < b.tradeId
            ? -1
            : a.tradeId > b.tradeId
              ? 1
              : 0,
      )
    },
    [connection, publicKey?.toBase58(), mineOnly],
  )

  /**
   * Populates the lifecycle form from a vault row: the refund account has to be
   * the vault's own `refund_source` for cancel to pass, and the token program is
   * read off the mint so settle/cancel encode against the right one.
   */
  const loadEscrow = async (row: SettlementVaultView) => {
    setTradeId(row.tradeId.toString())
    setMint(row.tokenMint.toBase58())
    setTaker(row.taker.toBase58())
    setSourceAta(row.refundSource.toBase58())
    try {
      const info = await connection.getAccountInfo(row.tokenMint)
      setTokenProgram(info && info.owner.equals(TOKEN_2022_PROGRAM_ID) ? 't22' : 'spl')
    } catch {
      // leave the current selection when the mint cannot be read
    }
  }

  // Live slot: anchor to a recent read and advance at Solana's ~400ms per slot.
  const now = useNow(1000)
  const [slotBase, setSlotBase] = useState<{ slot: number; at: number } | null>(null)
  useEffect(() => {
    let alive = true
    const read = () => {
      connection
        .getSlot()
        .then((slot) => {
          if (alive) setSlotBase({ slot, at: Date.now() })
        })
        .catch(() => undefined)
    }
    read()
    const id = setInterval(read, 15_000)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [connection])
  const estimatedSlot = slotBase ? slotBase.slot + Math.floor((now - slotBase.at) / 400) : null

  const source = sourceAta ? pk(sourceAta) : publicKey && mintKey ? ata(publicKey, mintKey, tokenProgramId) : null
  const takerKey = pk(taker)
  const takerToken = takerAta
    ? pk(takerAta)
    : takerKey && mintKey
      ? ata(takerKey, mintKey, tokenProgramId)
      : null

  return (
    <>
      <div className="grid-2">
        <Panel
          title="Clearinghouse config"
          aside={config.state ? <Badge tone="good">deployed</Badge> : <Badge tone="bad">not deployed</Badge>}
          subtitle="Global settlement settings. Initialize is restricted to the program's upgrade authority (the deployer)."
        >
          {config.state ? (
            <KeyValue
              rows={[
                ['Authority', config.state.authority.toBase58()],
                ['Pool gate', config.state.poolsRequireGate ? 'only wired AMM pools' : 'standalone allowed'],
                [
                  'Transfer agent key',
                  config.state.transferAgentSecp256k1.some((b) => b !== 0)
                    ? `${config.state.transferAgentSecp256k1.length} bytes set`
                    : 'not set',
                ],
                ['Hooks authority', config.state.hooksAuthority.toBase58()],
              ]}
            />
          ) : (
            <EmptyHint>{config.error ?? 'Initialize the config to begin.'}</EmptyHint>
          )}
          <div className="form">
            <FormRow>
              <Field label="Pool gate">
                <select
                  className="text-input"
                  value={poolsRequireGate ? 'on' : 'off'}
                  onChange={(e) => setPoolsRequireGate(e.target.value === 'on')}
                >
                  <option value="on">Only wired AMM pools may initiate</option>
                  <option value="off">Standalone authority flow allowed</option>
                </select>
              </Field>
              <ActionButton
                label="Initialize config"
                variant="secondary"
                onClick={() => void send('Settlement initialize', [settlementInitialize(publicKey!, poolsRequireGate)])}
                disabled={!publicKey}
              />
            </FormRow>
            <FormRow>
              <Field label="Transfer agent secp256k1 (128 hex, x‖y)" hint="Uncompressed pubkey without the 0x04 prefix.">
                <TextInput value={agentKeyHex} onChange={(e) => setAgentKeyHex(e.target.value)} />
              </Field>
              <ActionButton
                label="Set transfer agent"
                variant="secondary"
                onClick={() => void send('Set transfer agent', [settlementSetTransferAgent(publicKey!, hexToBytes(agentKeyHex))])}
                disabled={!publicKey || agentKeyHex.replace(/[^0-9a-fA-F]/g, '').length !== 128}
              />
            </FormRow>
            <FormRow>
              <Field label="Hooks authority (base58)" hint="Whitelist the hooks PDA so AMM-routed output can be escrowed.">
                <TextInput value={hooksAuth} onChange={(e) => setHooksAuth(e.target.value)} />
              </Field>
              <ActionButton
                label="Set hooks authority"
                variant="secondary"
                onClick={() => {
                  const key = pk(hooksAuth)
                  if (key) void send('Set hooks authority', [settlementSetHooksAuthority(publicKey!, key)])
                }}
                disabled={!publicKey || !pk(hooksAuth)}
              />
            </FormRow>
          </div>
        </Panel>

        <Panel
          title="Escrow inspector"
          subtitle="Every trade id has one vault PDA. Pending escrows release after the settlement slot AND a valid Transfer Agent signature."
        >
          {vault.state ? (
            <>
              <KeyValue
                rows={[
                  ['Trade id', vault.state.tradeId.toString()],
                  ['Status', vault.state.status],
                  ['Amount', vault.state.amount.toString()],
                  ['Taker', vault.state.taker.toBase58()],
                  ['Mint', vault.state.tokenMint.toBase58()],
                  ['Settlement slot', vault.state.settlementSlot.toString()],
                  ['Refund source', vault.state.refundSource.toBase58()],
                ]}
              />
              <p className="empty-hint mono">vault PDA {vault.state.address.toBase58()}</p>
            </>
          ) : (
            <EmptyHint>No escrow at trade id {tradeId}.</EmptyHint>
          )}
          <ActionButton label="Refresh" variant="secondary" onClick={() => void vault.refresh()} />
        </Panel>
      </div>

      <Panel
        title="Settlement escrows"
        subtitle="Every escrow on this cluster, pending first, with a live countdown to the settlement slot. Load any row to populate the lifecycle form below — settle with the agent's signature, or cancel for a compliance refund."
        aside={
          <Badge tone={queue.state && queue.state.some((row) => row.status === 'pending') ? 'warn' : 'neutral'}>
            {queue.state
              ? `${queue.state.filter((row) => row.status === 'pending').length} pending / ${queue.state.length} total`
              : 'scanning…'}
          </Badge>
        }
      >
        <label className="field" style={{ flexDirection: 'row', alignItems: 'center', gap: 8 }}>
          <input
            type="checkbox"
            checked={mineOnly}
            onChange={(event) => setMineOnly(event.target.checked)}
            disabled={!publicKey}
          />
          <span className="field-label">
            Mine only {publicKey ? `(taker = ${publicKey.toBase58().slice(0, 4)}…${publicKey.toBase58().slice(-4)})` : '(connect a wallet)'}
          </span>
        </label>
        <DataTable
          csvName="rwadex-settlement-queue"
          empty="No settlement escrows found on this cluster."
          rows={queue.state ?? []}
          columns={[
            { header: 'Trade id', cell: (row) => row.tradeId.toString(), text: (row) => row.tradeId.toString() },
            {
              header: 'Asset',
              cell: (row) => {
                const mint = row.tokenMint.toBase58()
                return <span title={mint} className="mono">{`${mint.slice(0, 4)}…${mint.slice(-4)}`}</span>
              },
              text: (row) => row.tokenMint.toBase58(),
            },
            { header: 'Notional', cell: (row) => row.amount.toString(), text: (row) => row.amount.toString() },
            {
              header: 'Settlement slot',
              cell: (row) => row.settlementSlot.toString(),
              text: (row) => row.settlementSlot.toString(),
            },
            {
              header: 'Countdown',
              cell: (row) => {
                if (row.status !== 'pending') return '—'
                if (estimatedSlot === null) return `${row.settlementSlot.toString()} slots`
                const slotsLeft = Number(row.settlementSlot) - estimatedSlot
                if (slotsLeft <= 0) return <Badge tone="good">ready</Badge>
                return `${formatDuration(slotsLeft * 400)} (~${slotsLeft} slots)`
              },
              text: (row) => row.settlementSlot.toString(),
            },
            {
              header: 'Taker',
              cell: (row) => (
                <span title={row.taker.toBase58()} className="mono">{`${row.taker.toBase58().slice(0, 4)}…${row.taker.toBase58().slice(-4)}`}</span>
              ),
              text: (row) => row.taker.toBase58(),
            },
            {
              header: 'Refund source',
              cell: (row) => (
                <span title={row.refundSource.toBase58()} className="mono">{`${row.refundSource.toBase58().slice(0, 4)}…${row.refundSource.toBase58().slice(-4)}`}</span>
              ),
              text: (row) => row.refundSource.toBase58(),
            },
            {
              header: 'Status',
              cell: (row) => (
                <Badge tone={row.status === 'pending' ? 'warn' : row.status === 'settled' ? 'good' : 'bad'}>
                  {row.status}
                </Badge>
              ),
              text: (row) => row.status,
            },
            {
              header: 'Load',
              cell: (row) => (
                <button
                  className="btn btn-secondary btn-compact"
                  onClick={() => void loadEscrow(row)}
                >
                  Load
                </button>
              ),
              text: () => '',
            },
          ]}
        />
      </Panel>

      <Panel
        title="Escrow lifecycle"
        subtitle="Initiate locks a flow from an account the signer owns; settle releases it with the agent's secp256k1 signature (no Solana signer needed); cancel is the compliance exit."
      >
        <div className="form">
          <FormRow>
            <Field label="Trade id">
              <NumberInput value={tradeId} onChange={(e) => setTradeId(e.target.value)} />
            </Field>
            <Field
              label="Settlement slot (absolute)"
              hint={`Not a horizon: the program only checks it is in the future. Now ≈ ${estimatedSlot ?? '…'}; blank means T+300 from that.`}
            >
              <NumberInput
                value={slots}
                onChange={(e) => setSlots(e.target.value)}
                placeholder={estimatedSlot !== null ? String(estimatedSlot + 300) : 'absolute slot'}
              />
            </Field>
            <Field label="Amount">
              <NumberInput value={amount} onChange={(e) => setAmount(e.target.value)} />
            </Field>
            <Field label="Mint">
              <TextInput value={mint} onChange={(e) => setMint(e.target.value)} placeholder="Base58 mint" />
            </Field>
          </FormRow>
          <FormRow>
            <Field label="Source token account" hint="Defaults to your ATA for the mint.">
              <TextInput value={sourceAta} onChange={(e) => setSourceAta(e.target.value)} />
            </Field>
            <Field label="Taker (recipient wallet)">
              <TextInput value={taker} onChange={(e) => setTaker(e.target.value)} placeholder="Base58 pubkey" />
            </Field>
            <Field label="Token program">
              <select
                className="text-input"
                value={tokenProgram}
                onChange={(e) => setTokenProgram(e.target.value as 'spl' | 't22')}
              >
                <option value="spl">SPL Token</option>
                <option value="t22">Token-2022</option>
              </select>
            </Field>
          </FormRow>
          <FormRow>
            <ActionButton
              label="Initiate settlement (lock escrow)"
              onClick={() => {
                if (!source || !mintKey || !takerKey) return
                void send('Settlement initiate', [
                  settlementInitiate({
                    tradeId: BigInt(tradeId || '0'),
                    settlementSlots: BigInt(
                      slots || String((estimatedSlot ?? 0) + SETTLEMENT_HORIZON_SLOTS),
                    ),
                    amount: BigInt(amount || '0'),
                    authority: publicKey!,
                    sourceTokenAccount: source,
                    tokenMint: mintKey,
                    taker: takerKey,
                    pool: null,
                    tokenProgramId,
                  }),
                ])
              }}
              disabled={!publicKey || !mintKey || !takerKey || !source}
            />
          </FormRow>
          <hr className="divider" />
          <FormRow>
            <Field label="Signature r‖s (128 hex)" hint="Agent signs (trade_id ‖ taker ‖ mint ‖ amount), keccak256, secp256k1.">
              <TextInput value={sigHex} onChange={(e) => setSigHex(e.target.value)} />
            </Field>
            <Field label="Recovery id">
              <NumberInput value={recoveryId} onChange={(e) => setRecoveryId(e.target.value)} />
            </Field>
            <Field label="Taker token account" hint="Defaults to the taker's ATA.">
              <TextInput value={takerAta} onChange={(e) => setTakerAta(e.target.value)} />
            </Field>
          </FormRow>
          <FormRow>
            <ActionButton
              label="Settle (release escrow)"
              onClick={() => {
                if (!mintKey || !takerToken) return
                void send('Settlement settle', [
                  settlementSettle(
                    BigInt(tradeId || '0'),
                    mintKey,
                    takerToken,
                    hexToBytes(sigHex),
                    Number(recoveryId),
                    tokenProgramId,
                  ),
                ])
              }}
              disabled={!mintKey || !takerToken || sigHex.replace(/[^0-9a-fA-F]/g, '').length !== 128}
            />
            <ActionButton
              label="Cancel settlement"
              variant="danger"
              onClick={() => {
                if (!publicKey || !mintKey || !source) return
                void send('Settlement cancel', [
                  settlementCancel({
                    tradeId: BigInt(tradeId || '0'),
                    tokenMint: mintKey,
                    authority: publicKey,
                    refundTokenAccount: source,
                    tokenProgramId,
                  }),
                ])
              }}
              disabled={!publicKey || !mintKey || !source}
            />
          </FormRow>
        </div>
      </Panel>
    </>
  )
}
