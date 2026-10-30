import { useState, useMemo } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
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
import { MerkleProofBuilder, MerkleTreeVisualizer } from '../components/MerkleProofBuilder'
import {
  TOKEN_2022_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  transferHookConfig,
  transferHookWhitelistEntry,
} from '@rwa-dex/sdk'
import { loadKycConfig, loadTokenAccount } from '@rwa-dex/sdk'
import {
  hooksFundHookAuthority,
  kycInitializeMint,
  kycIssue,
  kycRevoke,
  kycSetAuthority,
  kycVerify,
  transferHookCloseWhitelistEntry,
  transferHookInitialize,
  transferHookSetAuthority,
  transferHookSetPaused,
  transferHookSetWhitelistEntry,
  transferHookUpdateRoot,
} from '@rwa-dex/sdk'

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

  // ---- KYC attestations ----
  const [institution, setInstitution] = useState('')
  const [newAuthority, setNewAuthority] = useState('')

  // ---- Transfer hook whitelist ----
  const [rootHex, setRootHex] = useState('')
  const [whitelistInput, setWhitelistInput] = useState('')
  const [sourceWallet, setSourceWallet] = useState('')
  const [destWallet, setDestWallet] = useState('')
  const [entryWallet, setEntryWallet] = useState('')
  const [entryProofHex, setEntryProofHex] = useState('')

  // ---- Hooks escrow router ----
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

  // Parse the whitelist from a comma/space-separated list of Base58 pubkeys.
  const whitelist = useMemo(() => {
    if (!whitelistInput.trim()) return []
    return whitelistInput
      .split(/[,\s]+/)
      .map((s) => s.trim())
      .filter((s) => s.length > 0)
      .map((s) => {
        try {
          return new PublicKey(s)
        } catch {
          return null
        }
      })
      .filter((pk): pk is PublicKey => pk !== null)
  }, [whitelistInput])

  const sourceKey = useMemo(() => {
    if (sourceWallet) {
      try {
        return new PublicKey(sourceWallet)
      } catch {
        return null
      }
    }
    return publicKey ?? null
  }, [sourceWallet, publicKey])

  const destKey = useMemo(() => {
    if (!destWallet) return null
    try {
      return new PublicKey(destWallet)
    } catch {
      return null
    }
  }, [destWallet])

  // The whitelist entry the hook actually reads. Its proof is the same Merkle
  // path the builder below derives; the program refuses one that does not fold to
  // the published root, so pasting a stale path fails here rather than at the
  // next transfer.
  const entryOwner = useMemo(() => {
    if (!entryWallet) return null
    try {
      return new PublicKey(entryWallet)
    } catch {
      return null
    }
  }, [entryWallet])

  const entryProof = useMemo(() => {
    if (!entryProofHex.trim()) return [] // a single-member tree needs none
    try {
      return entryProofHex
        .split(/[,\s]+/)
        .filter((element) => element.length > 0)
        .map(hexToBytes)
    } catch {
      return null
    }
  }, [entryProofHex])

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
        {/* The gate and the escrow router share one column, so the router fills
            the space the gate alone used to leave empty. */}
        <div className="panel-stack">
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
              <p className="viz-note">
                The badge above is a balance read. Attestation integrity is the gate's own check: it rejects an
                attestation that does not belong to the KYC mint, to this wallet, or that holds nothing. Send it below to
                prove a KYC-wired pool would admit this wallet — a rejected transaction is the pool refusing the swap.
              </p>
              <ActionButton
                label="Verify my attestation on-chain"
                variant="secondary"
                onClick={() => void send('KYC verify', [kycVerify(publicKey!)])}
                disabled={!publicKey}
              />
            </div>
          </Panel>
  
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
        </div>

        <Panel
          title="Transfer-hook whitelist"
          subtitle="RWA mints route transfers through this hook: both sender and receiver must hold a whitelist entry whose Merkle path folds to the published root, and the transfer carries those two entries as accounts."
        >
          <KeyValue rows={[['Config PDA', transferHookConfig().toBase58()]]} />

          <MerkleTreeVisualizer owners={whitelist} />

          <div className="form">
            <FormRow>
              <Field
                label="Whitelist (comma/space-separated pubkeys)"
                hint="Institution wallets that may send or receive RWA tokens. The tree below is built from this list."
              >
                <TextInput
                  value={whitelistInput}
                  onChange={(e) => setWhitelistInput(e.target.value)}
                  placeholder="Pubkey1, Pubkey2, …"
                />
              </Field>
            </FormRow>

            <FormRow>
              <Field
                label="Whitelist root (64 hex)"
                hint="Root of keccak(0x00 ‖ wallet) leaves folded as keccak(0x01 ‖ min ‖ max) over sorted pairs."
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
              <Field
                label="Institution wallet (entry)"
                hint={`Entry PDA ${entryOwner ? transferHookWhitelistEntry(entryOwner).toBase58() : '—'}`}
              >
                <TextInput
                  value={entryWallet}
                  onChange={(e) => setEntryWallet(e.target.value)}
                  placeholder="Base58 pubkey"
                />
              </Field>
              <Field
                label="Proof (hex nodes, leaf-adjacent first)"
                hint="Build it below. Leave empty for a single-member tree; a path that does not fold to the root is rejected."
              >
                <TextInput
                  value={entryProofHex}
                  onChange={(e) => setEntryProofHex(e.target.value)}
                  placeholder="comma-separated 64-hex, or empty"
                />
              </Field>
            </FormRow>
            <FormRow>
              <ActionButton
                label="Register entry"
                variant="secondary"
                onClick={() =>
                  void send('Hook set whitelist entry', [
                    transferHookSetWhitelistEntry(publicKey!, entryOwner!, entryProof!),
                  ])
                }
                disabled={!publicKey || !entryOwner || entryProof === null}
              />
              <ActionButton
                label="Close entry (delist)"
                variant="danger"
                onClick={() =>
                  void send('Hook close whitelist entry', [
                    transferHookCloseWhitelistEntry(publicKey!, entryOwner!),
                  ])
                }
                disabled={!publicKey || !entryOwner}
              />
            </FormRow>
            <p className="viz-note">
              Entries are the on-chain copy of the whitelist: publishing a new root leaves them where they are, and every
              entry whose path no longer folds to it stops verifying — so re-register each institution after a rotation.
            </p>

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

          <hr className="divider" />

          <div className="form">
            <FormRow>
              <Field label="Source wallet (sender)" hint="Defaults to your wallet">
                <TextInput value={sourceWallet} onChange={(e) => setSourceWallet(e.target.value)} />
              </Field>
              <Field label="Destination wallet (recipient)">
                <TextInput value={destWallet} onChange={(e) => setDestWallet(e.target.value)} placeholder="Base58 pubkey" />
              </Field>
            </FormRow>
            {whitelist.length > 0 && sourceKey && destKey ? (
              <MerkleProofBuilder
                whitelist={whitelist}
                sourceWallet={sourceKey}
                destinationWallet={destKey}
                // The proof lives in the institution's entry PDA, so loading it
                // into the entry form is what makes a derived path registrable.
                onUseProof={(_side, proof, wallet) => {
                  setEntryWallet(wallet.toBase58())
                  setEntryProofHex(proof.join(', '))
                }}
              />
            ) : (
              <EmptyHint>
                Add whitelisted pubkeys and specify both wallets to build proofs.
              </EmptyHint>
            )}
          </div>
        </Panel>
      </div>
    </>
  )
}
