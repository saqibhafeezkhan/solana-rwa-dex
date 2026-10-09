import { useState } from 'react'
import { Keypair, PublicKey } from '@solana/web3.js'
import { useApp } from '../AppContext'
import {
  ActionButton,
  EmptyHint,
  Field,
  FormRow,
  KeyValue,
  NumberInput,
  Panel,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import {
  TOKEN_2022_PROGRAM_ID,
  TRANSFER_HOOK_PROGRAM_ID,
  ata,
  transferHookConfig,
  transferHookExtraAccountMetaList,
} from '../../core/pdas'
import { loadMint, loadTokenAccount } from '../../core/decoders'
import {
  rwaCreateMint,
  rwaMintTo,
  rwaTransfer,
  rwaUpdateTransferFee,
  transferHookInitExtraAccountMetaList,
} from '../../core/instructions-rails'

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
  const [sourceProof, setSourceProof] = useState('')
  const [destProof, setDestProof] = useState('')
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

  const proofs = (text: string): Uint8Array[] => {
    if (!text.trim()) return []
    return text
      .split(/[,\s]+/)
      .filter((token) => token.length > 0)
      .map((token) => hexToBytes(token))
      .map((bytes) => {
        if (bytes.length !== 32) throw new Error('each proof element is 32 bytes of hex')
        return bytes
      })
  }

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
        subtitle="Transfers carry Merkle proofs for BOTH wallets — the hook checks sender and receiver eligibility."
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
              label="Source proof (32-byte hex elements)"
              hint="Path from the sender's leaf: leaf = sha256(0x00 ‖ wallet), node = sha256(0x01 ‖ min ‖ max) — keccak will be rejected by the hook."
            >
              <TextInput value={sourceProof} onChange={(e) => setSourceProof(e.target.value)} placeholder="ab12…, cd34…" />
            </Field>
            <Field label="Destination proof" hint="Same construction for the recipient's wallet.">
              <TextInput value={destProof} onChange={(e) => setDestProof(e.target.value)} />
            </Field>
          </FormRow>
          <FormRow>
            <ActionButton
              label="Mint to my ATA"
              onClick={() => {
                if (!mintKey || !holderKey) return
                void send('RWA mint to', [
                  rwaMintTo(mintKey, ata(holderKey, mintKey, TOKEN_2022_PROGRAM_ID), publicKey!, BigInt(amount || '0')),
                ])
              }}
              disabled={!publicKey || !mintKey || !holderKey}
            />
            <ActionButton
              label="Transfer (with proofs)"
              variant="secondary"
              onClick={() => {
                if (!mintKey || !holderKey || !destKey) return
                void send('RWA transfer', [
                  rwaTransfer({
                    source: ata(holderKey, mintKey, TOKEN_2022_PROGRAM_ID),
                    destination: ata(destKey, mintKey, TOKEN_2022_PROGRAM_ID),
                    mint: mintKey,
                    authority: holderKey,
                    amount: BigInt(amount || '0'),
                    sourceProof: proofs(sourceProof),
                    destinationProof: proofs(destProof),
                  }),
                ])
              }}
              disabled={!publicKey || !mintKey || !holderKey || !destKey}
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
