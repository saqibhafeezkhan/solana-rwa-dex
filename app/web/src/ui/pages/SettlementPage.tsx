import { useEffect, useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
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
  hookAuthority,
  settlementConfig,
  settlementEscrowAuthority,
  programData,
  settlementMintAgent,
  settlementVault,
} from '@rwa-dex/sdk'
import {
  SETTLEMENT_VAULT_LEN,
  decodeSettlementVault,
  loadMintAgent,
  loadSettlementConfig,
  loadSettlementVault,
  loadTokenAccount,
  type SettlementVaultView,
} from '@rwa-dex/sdk'
import {
  hookTransferAccounts,
  settlementCancel,
  settlementClearMintAgent,
  settlementClose,
  settlementInitialize,
  settlementInitiate,
  settlementRefundExpired,
  settlementSetHooksAuthority,
  settlementSetMintAgent,
  settlementSetTransferAgent,
  settlementSettle,
} from '@rwa-dex/sdk'
import { ensureAtaInstruction } from '@rwa-dex/sdk'

/** Slots a blank settlement-slot field defaults to before submission. */
const SETTLEMENT_HORIZON_SLOTS = 300

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
  // Per-mint Transfer Agent override: a mint's own key replaces the config's
  // global one for that asset class.
  const [agentMint, setAgentMint] = useState('')
  const [mintAgentHex, setMintAgentHex] = useState('')

  // `settlementInitialize` is restricted on chain to the program's upgrade
  // authority; read it so the button is only offered to the key that can use
  // it. ProgramData layout: u32 tag (3), u64 slot, Option discriminator, key.
  const upgradeAuthority = useAccountState(async () => {
    try {
      const info = await connection.getAccountInfo(programData(SETTLEMENT_PROGRAM_ID))
      if (!info || info.data.length < 45 || info.data.readUInt32LE(0) !== 3) return null
      if (info.data[12] !== 1) return null
      return new PublicKey(info.data.subarray(13, 45))
    } catch {
      return null
    }
  }, [connection])
  const isUpgradeAuthority =
    publicKey !== null && upgradeAuthority.state !== null && upgradeAuthority.state.equals(publicKey)

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
  const mintText = mintKey?.toBase58() ?? null
  const [mineOnly, setMineOnly] = useState(false)

  const vaultAddress = (() => {
    try {
      // Same fallback as the actions below: a blank id means trade 0, not 1,
      // so the inspector never shows a different vault than the one acted on.
      return settlementVault(BigInt(tradeId || '0'))
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

  // The lifecycle mint's agent override, when one is installed. `settle` must
  // name this account or the program checks the release against the global key
  // and rejects a mint-specific agent's signature.
  const mintAgentAddress = mintKey ? settlementMintAgent(mintKey) : null
  const mintAgent = useAccountState(
    async () => {
      if (!mintAgentAddress) return null
      try {
        return await loadMintAgent(connection, mintAgentAddress)
      } catch {
        return null
      }
    },
    [connection, mintAgentAddress?.toBase58()],
  )
  const agentMintKey = pk(agentMint)

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
      const rank = (status: string) =>
        status === 'pending' ? 0 : status === 'settled' ? 1 : status === 'expired' ? 2 : 3
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
    // Resolve the token program before populating the form: the ATAs, the
    // token-program account and the hook-account list are all derived from it,
    // so a trade must never be loaded with the previous trade's program while
    // the mint lookup is still in flight.
    let program: 'spl' | 't22' = tokenProgram
    try {
      const info = await connection.getAccountInfo(row.tokenMint)
      program = info && info.owner.equals(TOKEN_2022_PROGRAM_ID) ? 't22' : 'spl'
    } catch {
      // leave the current selection when the mint cannot be read
    }
    const programId = program === 't22' ? TOKEN_2022_PROGRAM_ID : TOKEN_PROGRAM_ID
    setTokenProgram(program)
    setTradeId(row.tradeId.toString())
    setMint(row.tokenMint.toBase58())
    setTaker(row.taker.toBase58())
    // A settlement-routed trade was funded by the hooks program's custody ATA,
    // which has no instruction that can pay out of it, so its refund has to go
    // to the recorded taker (S-1). Default the field to whichever destination
    // the program will accept for this trade.
    setSourceAta(
      (row.refundSource.equals(ata(hookAuthority(), row.tokenMint, programId))
        ? ata(row.taker, row.tokenMint, programId)
        : row.refundSource
      ).toBase58(),
    )
    // The agent signs the vault's stored amount, so show that value rather than
    // whatever was last typed into the initiation form.
    setAmount(row.amount.toString())
    // These belong to the trade being loaded: a signature and a taker ATA from
    // a previous escrow must not leak into this one.
    setTakerAta('')
    setSigHex('')
    setRecoveryId('0')
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

  // Keep the token program in step with the mint. A legacy/2022 mismatch
  // derives the wrong ATAs and passes the wrong program to the transfer CPI.
  useEffect(() => {
    if (!mintText) return
    let cancelled = false
    connection
      .getAccountInfo(new PublicKey(mintText))
      .then((info) => {
        if (!cancelled && info) {
          setTokenProgram(info.owner.equals(TOKEN_2022_PROGRAM_ID) ? 't22' : 'spl')
        }
      })
      .catch(() => undefined)
    return () => {
      cancelled = true
    }
  }, [connection, mintText])

  // Only the config authority can rotate keys or cancel; a gated config only
  // admits AMM/hook initiations, which a wallet transaction cannot satisfy.
  const isConfigAuthority =
    config.state !== null && publicKey !== null && config.state.authority.equals(publicKey)
  // A staged proposal keeps the signer it was built with, so a proposal staged
  // by a non-authority cannot be approved by a different wallet: the program
  // would still see the stager as `authority`. Only the config authority can
  // build these instructions; multisig mode then adds the approval step.
  const canManageConfig = config.state !== null && isConfigAuthority
  const signatureReady = sigHex.replace(/^0x/, '').replace(/[^0-9a-fA-F]/g, '').length === 128
  const canInitiate =
    publicKey !== null &&
    mintKey !== null &&
    takerKey !== null &&
    source !== null &&
    config.state !== null &&
    !config.state.poolsRequireGate &&
    // A trade id is single-use: once the inspector has a vault for it, a
    // second `initiate_settlement` fails with `account already in use`.
    !vault.loading &&
    vault.state === null
  // The actions below are keyed by the inspector's vault: settling with a mint
  // or taker the vault did not record (or cancelling to an account other than
  // its refund source) is rejected on chain, so refuse it here with a disabled
  // button instead of a failed transaction.
  const vaultPending = !vault.loading && vault.state !== null && vault.state.status === 'pending'
  const vaultMatchesForm =
    vault.state !== null &&
    mintKey !== null &&
    takerKey !== null &&
    mintKey.equals(vault.state.tokenMint) &&
    takerKey.equals(vault.state.taker)
  // A routed trade's refund goes to the recorded taker rather than to the
  // custody ATA that funded it (S-1). Both destinations are derived from vault
  // state, so a match on either is the only thing the program will accept.
  const refundMatches =
    vault.state !== null &&
    source !== null &&
    (source.equals(vault.state.refundSource) ||
      source.equals(ata(vault.state.taker, vault.state.tokenMint, tokenProgramId)))
  const mintMatchesVault = vault.state !== null && mintKey !== null && mintKey.equals(vault.state.tokenMint)
  const canCancel = canManageConfig && mintMatchesVault && refundMatches
  const vaultUnlocked =
    vault.state === null ||
    estimatedSlot === null ||
    estimatedSlot >= Number(vault.state.settlementSlot)
  // The release window is [settlement_slot, expiry_slot): past expiry a
  // pending escrow can only be refunded, never released.
  const vaultExpiredPending =
    vault.state !== null &&
    vault.state.status === 'pending' &&
    estimatedSlot !== null &&
    estimatedSlot >= Number(vault.state.expirySlot)
  const vaultTerminal = vault.state !== null && vault.state.status !== 'pending'
  const canRefundExpired = mintMatchesVault && refundMatches && vaultExpiredPending
  const canClose = canManageConfig && vaultTerminal && mintMatchesVault

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
                  // Once the config exists the flag is immutable; show the real
                  // value instead of the (unsaved) initialize-form choice.
                  value={
                    config.state
                      ? config.state.poolsRequireGate
                        ? 'on'
                        : 'off'
                      : poolsRequireGate
                        ? 'on'
                        : 'off'
                  }
                  onChange={(e) => setPoolsRequireGate(e.target.value === 'on')}
                  disabled={config.state !== null}
                >
                  <option value="on">Only wired AMM pools may initiate</option>
                  <option value="off">Standalone authority flow allowed</option>
                </select>
              </Field>
              <ActionButton
                label="Initialize config"
                variant="secondary"
                onClick={() =>
                  void send('Settlement initialize', [settlementInitialize(publicKey!, poolsRequireGate)]).then(
                    () => void config.refresh(),
                  )
                }
                disabled={!publicKey || !isUpgradeAuthority || config.loading || config.state !== null}
              />
            </FormRow>
            {config.state === null && publicKey && !isUpgradeAuthority ? (
              <p className="viz-note">
                {upgradeAuthority.state
                  ? `Only the settlement program's upgrade authority (${upgradeAuthority.state.toBase58().slice(0, 4)}…${upgradeAuthority.state.toBase58().slice(-4)}) can initialize the config.`
                  : 'Reading the settlement program\'s upgrade authority…'}
              </p>
            ) : null}
            <FormRow>
              <Field label="Transfer agent secp256k1 (128 hex, x‖y)" hint="Uncompressed pubkey without the 0x04 prefix.">
                <TextInput value={agentKeyHex} onChange={(e) => setAgentKeyHex(e.target.value)} />
              </Field>
              <ActionButton
                label="Set transfer agent"
                variant="secondary"
                onClick={() =>
                  void send('Set transfer agent', [
                    settlementSetTransferAgent(publicKey!, hexToBytes(agentKeyHex)),
                  ]).then(() => void config.refresh())
                }
                disabled={!canManageConfig || agentKeyHex.replace(/[^0-9a-fA-F]/g, '').length !== 128}
              />
            </FormRow>
            <FormRow>
              <Field
                label="Per-mint agent — mint"
                hint='Installs a [b"agent", mint] override; clear it to fall back to the global key.'
              >
                <TextInput value={agentMint} onChange={(e) => setAgentMint(e.target.value)} placeholder="Base58 mint" />
              </Field>
              <Field label="Per-mint agent secp256k1 (128 hex, x‖y)">
                <TextInput value={mintAgentHex} onChange={(e) => setMintAgentHex(e.target.value)} />
              </Field>
            </FormRow>
            <FormRow>
              <ActionButton
                label="Set mint agent"
                variant="secondary"
                onClick={() => {
                  if (!publicKey || !agentMintKey) return
                  void send('Set mint agent', [
                    settlementSetMintAgent({
                      authority: publicKey,
                      tokenMint: agentMintKey,
                      secp256k1Pubkey: hexToBytes(mintAgentHex),
                    }),
                  ]).then(() => {
                    void mintAgent.refresh()
                    void config.refresh()
                  })
                }}
                disabled={
                  !canManageConfig ||
                  !agentMintKey ||
                  mintAgentHex.replace(/[^0-9a-fA-F]/g, '').length !== 128
                }
              />
              <ActionButton
                label="Clear mint agent"
                variant="danger"
                onClick={() => {
                  if (!publicKey || !agentMintKey) return
                  void send('Clear mint agent', [
                    settlementClearMintAgent({ authority: publicKey, tokenMint: agentMintKey }),
                  ]).then(() => {
                    void mintAgent.refresh()
                    void config.refresh()
                  })
                }}
                disabled={!canManageConfig || !agentMintKey}
              />
            </FormRow>
            <FormRow>
              <Field label="Hooks authority (base58)" hint="Whitelist the hooks-authority PDA so AMM-routed output can be escrowed; its escrow-token-account source then becomes the escrow-authority PDA, not the taker, so a KYC grade on the taker does not block an escrow release.">
                <TextInput value={hooksAuth} onChange={(e) => setHooksAuth(e.target.value)} />
              </Field>
              <ActionButton
                label="Set hooks authority"
                variant="secondary"
                onClick={() => {
                  const key = pk(hooksAuth)
                  if (key) {
                    void send('Set hooks authority', [settlementSetHooksAuthority(publicKey!, key)]).then(
                      () => void config.refresh(),
                    )
                  }
                }}
                disabled={!canManageConfig || !pk(hooksAuth)}
              />
            </FormRow>
            {config.state ? (
              <p className="viz-note">
                {config.state.poolsRequireGate
                  ? 'This config is pool-gated: wallet-initiated escrows are refused. Initiation must come through a wired AMM pool or the hooks authority.'
                  : 'Standalone initiation is enabled: a signer may escrow tokens it owns.'}
                {publicKey && !isConfigAuthority
                  ? ' Only the config authority can rotate keys or cancel trades.'
                  : ''}
              </p>
            ) : null}
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
                  ['Expiry slot', vault.state.expirySlot.toString()],
                  [
                    'Transfer agent',
                    mintAgent.state
                      ? `per-mint override ${mintAgentAddress?.toBase58() ?? ''}`
                      : 'config global key',
                  ],
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
              header: 'Expiry slot',
              cell: (row) => row.expirySlot.toString(),
              text: (row) => row.expirySlot.toString(),
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
                <Badge
                  tone={
                    row.status === 'pending'
                      ? 'warn'
                      : row.status === 'settled'
                        ? 'good'
                        : row.status === 'expired'
                          ? 'neutral'
                          : 'bad'
                  }
                >
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
              hint={`Not a horizon: the program only checks it is in the future. Now ≈ ${estimatedSlot ?? '…'}; blank resolves T+300 against the live slot when sent.`}
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
                if (!publicKey || !source || !mintKey || !takerKey) return
                void send('Settlement initiate', [
                  (async () => {
                    // Resolve a blank field against the live slot at send time;
                    // a stale estimate could already be in the past.
                    const settlementSlots = slots
                      ? BigInt(slots)
                      : BigInt((await connection.getSlot('confirmed')) + SETTLEMENT_HORIZON_SLOTS)
                    return settlementInitiate({
                      tradeId: BigInt(tradeId || '0'),
                      settlementSlots,
                      amount: BigInt(amount || '0'),
                      authority: publicKey,
                      sourceTokenAccount: source,
                      tokenMint: mintKey,
                      taker: takerKey,
                      pool: null,
                      tokenProgramId,
                      // A hook-gated RWA mint rejects the escrow transfer without
                      // its transfer-hook accounts; extras are ignored otherwise.
                      hookAccounts:
                        tokenProgram === 't22'
                          ? hookTransferAccounts(mintKey, publicKey, settlementEscrowAuthority())
                          : [],
                    })
                  })(),
                ]).then(() => {
                  void vault.refresh()
                  void queue.refresh()
                })
              }}
              disabled={!canInitiate}
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
                if (!publicKey || !mintKey || !takerToken) return
                void send('Settlement settle', [
                  // Release to the taker's ATA, creating it when the default one
                  // is used and does not exist yet.
                  !takerAta && takerKey
                    ? ensureAtaInstruction(connection, takerKey, mintKey, publicKey, tokenProgramId)
                    : undefined,
                  settlementSettle(
                    BigInt(tradeId || '0'),
                    mintKey,
                    takerToken,
                    hexToBytes(sigHex),
                    Number(recoveryId),
                    tokenProgramId,
                    // Escrow (escrow authority) is the sender here, so its own
                    // whitelist entry is the source side.
                    tokenProgram === 't22' && takerKey
                      ? hookTransferAccounts(mintKey, settlementEscrowAuthority(), takerKey)
                      : [],
                    // The mint's override, when installed; null leaves the
                    // program checking the config's global key.
                    mintAgent.state ? mintAgentAddress : null,
                  ),
                ]).then(() => {
                  void vault.refresh()
                  void queue.refresh()
                })
              }}
              disabled={
                !publicKey ||
                !mintKey ||
                !takerToken ||
                (tokenProgram === 't22' && !takerKey) ||
                !vaultMatchesForm ||
                !vaultPending ||
                !vaultUnlocked ||
                vaultExpiredPending ||
                !signatureReady
              }
            />
            <ActionButton
              label="Cancel settlement"
              variant="danger"
              onClick={() => {
                if (!publicKey || !mintKey || !source) return
                void send('Settlement cancel', [
                  (async () => {
                    // The refund account's owner is the hook's destination side;
                    // it is whatever account funded the escrow, not necessarily
                    // the current config authority.
                    let refundOwner = publicKey
                    if (tokenProgram === 't22') {
                      try {
                        refundOwner = (await loadTokenAccount(connection, source)).owner
                      } catch {
                        // fall back to the signer; a hook mint will reject a mismatch
                      }
                    }
                    return settlementCancel({
                      tradeId: BigInt(tradeId || '0'),
                      tokenMint: mintKey,
                      authority: publicKey,
                      refundTokenAccount: source,
                      tokenProgramId,
                      hookAccounts:
                        tokenProgram === 't22'
                          ? hookTransferAccounts(mintKey, settlementEscrowAuthority(), refundOwner)
                          : [],
                    })
                  })(),
                ]).then(() => {
                  void vault.refresh()
                  void queue.refresh()
                })
              }}
              disabled={!canCancel || !vaultPending}
            />
            <ActionButton
              label="Refund expired escrow"
              variant="secondary"
              onClick={() => {
                if (!publicKey || !mintKey || !source) return
                void send('Settlement refund expired', [
                  (async () => {
                    // As in cancel, the refund account's owner is the hook's
                    // destination side; the destination itself is pinned on
                    // chain to the vault's recorded refund source.
                    let refundOwner = publicKey
                    if (tokenProgram === 't22') {
                      try {
                        refundOwner = (await loadTokenAccount(connection, source)).owner
                      } catch {
                        // fall back to the signer; a hook mint will reject a mismatch
                      }
                    }
                    return settlementRefundExpired({
                      tradeId: BigInt(tradeId || '0'),
                      tokenMint: mintKey,
                      refundTokenAccount: source,
                      tokenProgramId,
                      hookAccounts:
                        tokenProgram === 't22'
                          ? hookTransferAccounts(mintKey, settlementEscrowAuthority(), refundOwner)
                          : [],
                    })
                  })(),
                ]).then(() => {
                  void vault.refresh()
                  void queue.refresh()
                })
              }}
              disabled={!canRefundExpired}
            />
            <ActionButton
              label="Close & reclaim rent"
              variant="secondary"
              onClick={() => {
                if (!publicKey || !mintKey) return
                void send('Settlement close', [
                  // The close sweeps any residual escrow balance to the config
                  // authority's ATA, so that account has to exist first.
                  ensureAtaInstruction(connection, publicKey, mintKey, publicKey, tokenProgramId),
                  settlementClose({
                    tradeId: BigInt(tradeId || '0'),
                    tokenMint: mintKey,
                    authority: publicKey,
                    tokenProgramId,
                    residualAccount: ata(publicKey, mintKey, tokenProgramId),
                  }),
                ]).then(() => {
                  void vault.refresh()
                  void queue.refresh()
                })
              }}
              disabled={!canClose}
            />
          </FormRow>
        </div>
      </Panel>
    </>
  )
}
