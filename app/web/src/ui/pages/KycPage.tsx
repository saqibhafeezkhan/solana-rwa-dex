import { useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../AppContext'
import {
  ActionButton,
  Badge,
  EmptyHint,
  Field,
  FormRow,
  KeyValue,
  Panel,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import {
  TOKEN_2022_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  transferHookConfig,
} from '../../core/pdas'
import { loadKycConfig, loadTokenAccount } from '../../core/decoders'
import {
  hooksFundHookAuthority,
  kycInitializeMint,
  kycIssue,
  kycRevoke,
  kycSetAuthority,
  transferHookInitialize,
  transferHookSetAuthority,
  transferHookSetPaused,
  transferHookUpdateRoot,
} from '../../core/instructions-rails'

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/^0x/, '').replace(/[^0-9a-fA-F]/g, '')
  if (clean.length % 2 !== 0) throw new Error('hex length must be even')
  const out = new Uint8Array(clean.length / 2)
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16)
  return out
}

export function KycPage() {
  const { connection, publicKey, send } = useApp()

  const config = useAccountState(() => loadKycConfig(connection, kycConfig()), [connection])

  const [institution, setInstitution] = useState('')
  const [newAuthority, setNewAuthority] = useState('')
  const [rootHex, setRootHex] = useState('')
  const [fundLamports, setFundLamports] = useState('100000000')

  const myAttestation = useAccountState(
    async () => {
      if (!publicKey || !config.state) return null
      const address = ata(publicKey, config.state.kycMint, TOKEN_2022_PROGRAM_ID)
      try {
        return (await loadTokenAccount(connection, address)).amount
      } catch {
        return 0n
      }
    },
    [connection, publicKey, config.state?.kycMint.toBase58()],
  )

  const institutionAta = (() => {
    try {
      return institution && config.state ? ata(new PublicKey(institution), config.state.kycMint, TOKEN_2022_PROGRAM_ID) : null
    } catch {
      return null
    }
  })()

  return (
    <>
      <div className="grid-2">
        <Panel
          title="KYC gate"
          aside={config.state ? <Badge tone="good">deployed</Badge> : <Badge tone="bad">not deployed</Badge>}
          subtitle="Soulbound attestations. Pools wired with the KYC rail require one from every trader."
        >
          {config.state ? (
            <KeyValue
              rows={[
                ['Authority', config.state.authority.toBase58()],
                ['Attestation mint', config.state.kycMint.toBase58()],
                [
                  'My attestation',
                  myAttestation.state !== null ? (
                    myAttestation.state > 0n ? (
                      <Badge key="approved" tone="good">approved</Badge>
                    ) : (
                      <Badge key="none" tone="warn">none</Badge>
                    )
                  ) : (
                    '—'
                  ),
                ],
              ]}
            />
          ) : (
            <EmptyHint>{config.error ?? 'Run initialize once as the program deployer.'}</EmptyHint>
          )}
          <div className="form">
            <FormRow>
              <Field label="Institution wallet">
                <TextInput value={institution} onChange={(e) => setInstitution(e.target.value)} placeholder="Base58 pubkey" />
              </Field>
              <ActionButton
                label="Issue KYC"
                onClick={() => void send('KYC issue', [kycIssue(publicKey!, new PublicKey(institution))])}
                disabled={!publicKey || !institution}
              />
              <ActionButton
                label="Revoke KYC"
                variant="danger"
                onClick={() => void send('KYC revoke', [kycRevoke(publicKey!, new PublicKey(institution))])}
                disabled={!publicKey || !institution}
              />
            </FormRow>
            <FormRow>
              <Field label="New authority" hint="Verify the address off-chain first — the incoming key need not consent.">
                <TextInput value={newAuthority} onChange={(e) => setNewAuthority(e.target.value)} />
              </Field>
              <ActionButton
                label="Rotate KYC authority"
                variant="secondary"
                onClick={() => void send('KYC set authority', [kycSetAuthority(publicKey!, new PublicKey(newAuthority))])}
                disabled={!publicKey || !newAuthority}
              />
              <ActionButton
                label="Initialize KYC mint"
                variant="secondary"
                onClick={() => void send('KYC initialize', [kycInitializeMint(publicKey!)])}
                disabled={!publicKey}
              />
            </FormRow>
            {institutionAta ? (
              <p className="empty-hint mono">institution ATA {institutionAta.toBase58()}</p>
            ) : null}
          </div>
        </Panel>

        <Panel
          title="Transfer-hook whitelist"
          subtitle="RWA mints route transfers through this hook: both sender and receiver must hold a Merkle proof against the published root."
        >
          <KeyValue
            rows={[['Config PDA', transferHookConfig().toBase58()]]}
          />
          <div className="form">
            <FormRow>
              <Field
                label="Whitelist root (64 hex)"
                hint="Root of sha256(0x00 ‖ wallet) leaves folded as sha256(0x01 ‖ min ‖ max) over sorted pairs. Not keccak; an empty root is refused — pause instead."
              >
                <TextInput value={rootHex} onChange={(e) => setRootHex(e.target.value)} />
              </Field>
              <ActionButton
                label="Update root"
                onClick={() => void send('Hook update root', [transferHookUpdateRoot(publicKey!, hexToBytes(rootHex))])}
                disabled={!publicKey || rootHex.replace(/[^0-9a-fA-F]/g, '').length !== 64}
              />
            </FormRow>
            <FormRow>
              <ActionButton
                label="Pause transfers"
                variant="danger"
                onClick={() => void send('Hook pause', [transferHookSetPaused(publicKey!, true)])}
                disabled={!publicKey}
              />
              <ActionButton
                label="Resume transfers"
                variant="secondary"
                onClick={() => void send('Hook resume', [transferHookSetPaused(publicKey!, false)])}
                disabled={!publicKey}
              />
              <ActionButton
                label="Initialize hook"
                variant="secondary"
                onClick={() => void send('Hook initialize', [transferHookInitialize(publicKey!)])}
                disabled={!publicKey}
              />
            </FormRow>
            <FormRow>
              <Field label="New hook authority">
                <TextInput value={newAuthority} onChange={(e) => setNewAuthority(e.target.value)} />
              </Field>
              <ActionButton
                label="Rotate hook authority"
                variant="secondary"
                onClick={() => void send('Hook set authority', [transferHookSetAuthority(publicKey!, new PublicKey(newAuthority))])}
                disabled={!publicKey || !newAuthority}
              />
            </FormRow>
          </div>
        </Panel>
      </div>

      <Panel
        title="Hooks escrow router"
        subtitle="The hook authority PDA owns the custody account AMM swaps pay into and pays rent for the settlement escrow. Top it up when escrow creation starts failing."
      >
        <KeyValue rows={[['Hook authority PDA', hookAuthority().toBase58()]]} />
        <FormRow>
          <Field label="Lamports">
            <TextInput value={fundLamports} onChange={(e) => setFundLamports(e.target.value)} />
          </Field>
          <ActionButton
            label="Fund hook authority"
            variant="secondary"
            onClick={() => void send('Fund hook authority', [hooksFundHookAuthority(publicKey!, BigInt(fundLamports || '0'))])}
            disabled={!publicKey}
          />
        </FormRow>
      </Panel>
    </>
  )
}
