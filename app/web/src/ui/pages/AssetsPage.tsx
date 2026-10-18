import { useMemo, useState } from 'react'
import { Keypair, PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
import {
  ActionButton,
  EmptyHint,
  Field,
  FormRow,
  KeyValue,
  NumberInput,
  Panel,
  StatusLine,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import {
  TOKEN_2022_PROGRAM_ID,
  TRANSFER_HOOK_PROGRAM_ID,
  ata,
  transferHookConfig,
  transferHookExtraAccountMetaList,
  transferHookWhitelistEntry,
} from '../../core/pdas'
import { loadMint, loadTokenAccount, loadTransferHookConfig } from '../../core/decoders'
import { ensureAtaInstruction } from '../../core/connection'
import {
  rwaCreateMint,
  rwaMintTo,
  rwaTransfer,
  rwaUpdateTransferFee,
  transferHookInitExtraAccountMetaList,
  transferHookSetWhitelistEntry,
} from '../../core/instructions-rails'
import { buildTreeAndProve, bytesToHex } from '../../core/merkle'
import { loadWhitelist } from '../enterprise'

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/^0x/, '').replace(/[^0-9a-fA-F]/g, '')
  if (clean.length % 2 !== 0) throw new Error('hex length must be even')
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

export function AssetsPage() {
  const { connection, publicKey, send } = useApp()

  // create mint
  const [decimals, setDecimals] = useState('6')
  const [name, setName] = useState('US Treasury Series')
  const [symbol, setSymbol] = useState('UST')
  const [uri, setUri] = useState('https://example.invalid/ust.json')
  const [freshMint, setFreshMint] = useState<string | null>(null)

  // mint / transfer
  const [mint, setMint] = useState('')
  const [holder, setHolder] = useState('')
  const [amount, setAmount] = useState('1000000')
  const [destination, setDestination] = useState('')
  const [feeBps, setFeeBps] = useState('25')
  const [feeCap, setFeeCap] = useState('1000000')

  const mintKey = pk(mint || freshMint || '')
  const holderKey = pk(holder || publicKey?.toBase58() || '')
  const destKey = pk(destination)

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

  const holderBalance = useAccountState(
    async () => {
      if (!mintKey || !holderKey) return null
      try {
        return (await loadTokenAccount(connection, ata(holderKey, mintKey, TOKEN_2022_PROGRAM_ID))).amount
      } catch {
        return 0n
      }
    },
    [connection, mintKey?.toBase58(), holderKey?.toBase58()],
  )

  // The hook verifies a Keccak Merkle proof for BOTH wallets, against the root
  // published in the transfer-hook config. Both proofs are derived from the
  // imported institution whitelist here instead of being pasted as hex, and the
  // resulting root is compared with the on-chain one — a mismatch means the
  // transfer would be rejected, so it is shown before anything is sent.
  const [whitelistVersion, setWhitelistVersion] = useState(0)
  const whitelist = useMemo(() => {
    void whitelistVersion
    return loadWhitelist().flatMap((wallet) => {
      try {
        return [new PublicKey(wallet)]
      } catch {
        return []
      }
    })
  }, [whitelistVersion])

  // Depend on the base58 text, not on PublicKey objects (rebuilt every render).
  const holderKeyText = holderKey?.toBase58() ?? null
  const destKeyText = destKey?.toBase58() ?? null
  const derivedProofs = useMemo(() => {
    if (!holderKeyText || !destKeyText || whitelist.length === 0) return null
    try {
      return buildTreeAndProve(whitelist, new PublicKey(holderKeyText), new PublicKey(destKeyText))
    } catch {
      return null
    }
  }, [whitelist, holderKeyText, destKeyText])

  const hookConfig = useAccountState(
    async () => {
      try {
        return await loadTransferHookConfig(connection, transferHookConfig())
      } catch {
        return null
      }
    },
    [connection],
  )
  const onChainRoot = hookConfig.state ? bytesToHex(hookConfig.state.merkleRoot) : null
  const rootMatches = onChainRoot !== null && derivedProofs !== null && onChainRoot === derivedProofs.root

  // Matching roots are necessary but not sufficient: the transfer has to carry
  // both institutions' whitelist entries, because that is where the proofs live
  // now (Token-2022 gives the hook no way to receive them as instruction data).
  const whitelistEntries = useAccountState(
    async () => {
      if (!holderKeyText || !destKeyText) return null
      const [source, destination] = await connection.getMultipleAccountsInfo([
        transferHookWhitelistEntry(new PublicKey(holderKeyText)),
        transferHookWhitelistEntry(new PublicKey(destKeyText)),
      ])
      return { source: source !== null, destination: destination !== null }
    },
    [connection, holderKeyText, destKeyText],
  )
  const bothEntriesRegistered =
    whitelistEntries.state?.source === true && whitelistEntries.state?.destination === true

  const canTransfer =
    publicKey !== null && derivedProofs !== null && onChainRoot !== null && rootMatches && bothEntriesRegistered

  return (
    <>
      <div className="grid-2">
        <Panel
          title="Create RWA mint"
          subtitle="Token-2022 with TransferHook → MetadataPointer → PermanentDelegate → TransferFeeConfig, plus on-chain metadata. The hook program is baked in at creation."
        >
          <div className="form">
            <FormRow>
              <Field label="Decimals">
                <NumberInput value={decimals} onChange={(e) => setDecimals(e.target.value)} />
              </Field>
              <Field label="Name">
                <TextInput value={name} onChange={(e) => setName(e.target.value)} />
              </Field>
              <Field label="Symbol">
                <TextInput value={symbol} onChange={(e) => setSymbol(e.target.value)} />
              </Field>
            </FormRow>
            <Field label="URI (ISIN / NAV / prospectus)">
              <TextInput value={uri} onChange={(e) => setUri(e.target.value)} />
            </Field>
            <ActionButton
              label="Create mint (generates a fresh keypair)"
              onClick={() => {
                const mintKeypair = Keypair.generate()
                setFreshMint(mintKeypair.publicKey.toBase58())
                void send(
                  'RWA create mint',
                  [
                    rwaCreateMint(mintKeypair.publicKey, publicKey!, {
                      decimals: Number(decimals),
                      transferHookProgram: TRANSFER_HOOK_PROGRAM_ID,
                      delegate: publicKey!,
                      feeAuthority: publicKey!,
                      name,
                      symbol,
                      uri,
                    }),
                  ],
                  // The System Program needs the new mint's key to sign for
                  // its own account creation; partial-sign before the wallet.
                  [mintKeypair],
                )
              }}
              disabled={!publicKey}
            />
          </div>
        </Panel>

        <Panel
          title="Mint inspector"
          subtitle="Read any RWA mint and the connected holder's balance."
        >
          <FormRow>
            <Field label="Mint (defaults to the freshly created one)">
              <TextInput value={mint} onChange={(e) => setMint(e.target.value)} placeholder={freshMint ?? 'Base58 mint'} />
            </Field>
            <Field label="Holder">
              <TextInput value={holder} onChange={(e) => setHolder(e.target.value)} placeholder="Your wallet when empty" />
            </Field>
          </FormRow>
          {mintState.state ? (
            <KeyValue
              rows={[
                ['Decimals', mintState.state.decimals.toString()],
                ['Supply', mintState.state.supply.toString()],
                ['Hook program', TRANSFER_HOOK_PROGRAM_ID.toBase58()],
                ['Extra account metas', transferHookExtraAccountMetaList(mintKey!).toBase58()],
                ['Hook config', transferHookConfig().toBase58()],
              ]}
            />
          ) : (
            <EmptyHint>No mint loaded.</EmptyHint>
          )}
          {holderBalance.state !== null ? (
            <KeyValue rows={[['Holder balance', holderBalance.state.toString()]]} />
          ) : null}
        </Panel>
      </div>

      <Panel
        title="Mint, transfer & fees"
        subtitle="The hook checks BOTH wallets: each one's Merkle proof lives in its whitelist entry, which the transfer carries as an account (Token-2022 resolves it from the wallet's token account)."
      >
        <div className="form">
          <FormRow>
            <Field label="Amount">
              <NumberInput value={amount} onChange={(e) => setAmount(e.target.value)} />
            </Field>
            <Field label="Destination (transfer)">
              <TextInput value={destination} onChange={(e) => setDestination(e.target.value)} placeholder="Base58 wallet" />
            </Field>
          </FormRow>
          <FormRow>
            <Field
              label="Source proof (derived)"
              hint={`${derivedProofs?.sourceProof.length ?? 0} node(s) from the imported whitelist, keccak(0x00 ‖ wallet) · keccak(0x01 ‖ min ‖ max)`}
            >
              <TextInput readOnly value={derivedProofs ? derivedProofs.sourceProof.join(', ') || '—' : '—'} />
            </Field>
            <Field
              label="Destination proof (derived)"
              hint={`${derivedProofs?.destinationProof.length ?? 0} node(s) for the recipient`}
            >
              <TextInput readOnly value={derivedProofs ? derivedProofs.destinationProof.join(', ') || '—' : '—'} />
            </Field>
          </FormRow>
          <FormRow>
            <Field label="Computed root" hint="Built from the imported whitelist.">
              <TextInput readOnly value={derivedProofs?.root ?? '—'} />
            </Field>
            <Field label="On-chain root" hint="Published in the transfer-hook config.">
              <TextInput readOnly value={onChainRoot ?? 'not initialized'} />
            </Field>
            <ActionButton
              label="Reload whitelist"
              variant="secondary"
              onClick={() => setWhitelistVersion((version) => version + 1)}
            />
          </FormRow>
          {derivedProofs === null ? (
            <StatusLine
              error={`Import the institution whitelist on the Enterprise page first (${whitelist.length} wallet(s) loaded) — both the sender and the recipient must be in it.`}
            />
          ) : onChainRoot === null ? (
            <p className="viz-note">
              No on-chain transfer-hook config on this cluster yet, so the proofs cannot be checked against a published
              root. Initialize the hook and publish the root from the KYC &amp; Whitelist page.
            </p>
          ) : !rootMatches ? (
            <StatusLine error="The imported whitelist does not reproduce the on-chain Merkle root — re-import the exact whitelist that was published, or publish this root." />
          ) : !bothEntriesRegistered ? (
            <StatusLine
              error={`The proofs match the published root, but ${
                whitelistEntries.state?.source ? 'the recipient\u2019s' : 'the sender\u2019s'
              } whitelist entry is missing on chain. Register both entries below — a transfer carries them as accounts (Token-2022 derives them from each wallet), so a missing one fails before the hook is ever called.`}
            />
          ) : (
            <p className="viz-note">
              Proofs reproduce the on-chain root and both entries are registered: the hook will approve this transfer.
            </p>
          )}
          <FormRow>
            <ActionButton
              label="Register both whitelist entries"
              variant="secondary"
              onClick={() => {
                if (!publicKey || !derivedProofs || !holderKey || !destKey) return
                void send('Hook set whitelist entries', [
                  transferHookSetWhitelistEntry(publicKey, holderKey, derivedProofs.sourceProof.map(hexToBytes)),
                  transferHookSetWhitelistEntry(publicKey, destKey, derivedProofs.destinationProof.map(hexToBytes)),
                ]).then(() => whitelistEntries.refresh())
              }}
              disabled={!publicKey || !derivedProofs || !rootMatches}
            />
          </FormRow>
          <FormRow>
            <ActionButton
              label="Mint to my ATA"
              onClick={() => {
                if (!mintKey || !holderKey) return
                void send('RWA mint to', [
                  // The holder's ATA is created on demand; minting to a wallet
                  // that has never held this mint otherwise fails outright.
                  ensureAtaInstruction(connection, holderKey, mintKey, publicKey!, TOKEN_2022_PROGRAM_ID),
                  rwaMintTo(mintKey, ata(holderKey, mintKey, TOKEN_2022_PROGRAM_ID), publicKey!, BigInt(amount || '0')),
                ])
              }}
              disabled={!publicKey || !mintKey || !holderKey}
            />
            <ActionButton
              label="Transfer (hook-approved)"
              variant="secondary"
              onClick={() => {
                if (!mintKey || !holderKey || !destKey) return
                void send('RWA transfer', [
                  // Create the recipient's ATA when needed, then move the tokens.
                  ensureAtaInstruction(connection, destKey, mintKey, publicKey!, TOKEN_2022_PROGRAM_ID),
                  rwaTransfer({
                    source: ata(holderKey, mintKey, TOKEN_2022_PROGRAM_ID),
                    sourceOwner: holderKey,
                    destination: ata(destKey, mintKey, TOKEN_2022_PROGRAM_ID),
                    destinationOwner: destKey,
                    mint: mintKey,
                    // The creator's permanent delegate authorizes the transfer,
                    // so the fee authority need not be the source owner.
                    authority: publicKey!,
                    amount: BigInt(amount || '0'),
                  }),
                ])
              }}
              disabled={!publicKey || !mintKey || !holderKey || !destKey || !canTransfer}
            />
          </FormRow>
          <hr className="divider" />
          <FormRow>
            <Field label="New transfer fee (bps)">
              <NumberInput value={feeBps} onChange={(e) => setFeeBps(e.target.value)} />
            </Field>
            <Field label="New maximum fee">
              <NumberInput value={feeCap} onChange={(e) => setFeeCap(e.target.value)} />
            </Field>
            <ActionButton
              label="Update transfer fee"
              variant="secondary"
              onClick={() => {
                if (!mintKey) return
                void send('RWA update fee', [rwaUpdateTransferFee(mintKey, publicKey!, Number(feeBps), BigInt(feeCap || '0'))])
              }}
              disabled={!publicKey || !mintKey}
            />
            <ActionButton
              label="Register hook metas for this mint"
              variant="secondary"
              onClick={() => {
                if (!mintKey) return
                void send('Hook metas init', [transferHookInitExtraAccountMetaList(publicKey!, mintKey)])
              }}
              disabled={!publicKey || !mintKey}
            />
          </FormRow>
        </div>
      </Panel>
    </>
  )
}
