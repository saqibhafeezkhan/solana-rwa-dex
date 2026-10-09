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
  NumberInput,
  Panel,
  StatusLine,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import {
  DLMM_BINS_PER_ARRAY,
  TOKEN_PROGRAM_ID,
  ata,
  dlmmBinArrayIndex,
  dlmmPool,
  dlmmPosition,
  hookAuthority,
  kycConfig,
} from '../../core/pdas'
import { loadDlmmPool, loadTokenAccount, decodeBinPosition } from '../../core/decoders'
import { BinHistogram, type BinDatum } from '../charts'
import { OracleRiskPanel, PausedBanner } from '../OracleRiskPanel'
import { binPrice } from '../pricing'
import {
  dlmmAddLiquidity,
  dlmmInitializeBinArray,
  dlmmInitializePool,
  dlmmRemoveLiquidity,
  dlmmSetAuthority,
  dlmmSetPaused,
  dlmmSwap,
  dlmmTripBreaker,
} from '../../core/instructions-dlmm'
import { SAMPLE_DLMM_POOL } from '../../core/sampleData'

export function DlmmPage() {
  const { connection, publicKey, send } = useApp()
  const [sampleMode] = useState(() => {
    if (typeof window === 'undefined') return false
    return localStorage.getItem('rwadex.sampleMode') === 'true'
  })

  const [mintA, setMintA] = useState(() => sampleMode ? SAMPLE_DLMM_POOL.mintA.toBase58() : '')
  const [mintB, setMintB] = useState(() => sampleMode ? SAMPLE_DLMM_POOL.mintB.toBase58() : '')
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      if (sampleMode) return SAMPLE_DLMM_POOL
      return loadDlmmPool(connection, poolAddress)
    },
    [connection, poolAddress?.toBase58(), sampleMode],
  )

  const wiring = pool.state
    ? {
        // Value comparison: a decoded `Pubkey::default()` is all-zero bytes in a
        // fresh object, so `!== PublicKey.default` was true for every pool and
        // every rail was passed to the program whether it was wired or not.
        kycConfig: !pool.state.kycConfig.equals(PublicKey.default),
        oracle: !pool.state.oracleState.equals(PublicKey.default),
        settlement: !pool.state.hooksAuthority.equals(PublicKey.default),
      }
    : { kycConfig: false, oracle: false, settlement: false }

  // init pool form
  const [activeBinId, setActiveBinId] = useState('0')
  const [binStep, setBinStep] = useState('10')
  const [baseFeeBps, setBaseFeeBps] = useState('20')
  const [useKyc, setUseKyc] = useState(false)
  const [deviationBps, setDeviationBps] = useState('0')
  const [useSettlement, setUseSettlement] = useState(false)
  const [settlementSlots, setSettlementSlots] = useState('100')

  // liquidity form
  const [binId, setBinId] = useState('0')
  const [amountA, setAmountA] = useState('1000000')
  const [amountB, setAmountB] = useState('1000000')
  const [shares, setShares] = useState('1000')
  const [minA, setMinA] = useState('0')
  const [minB, setMinB] = useState('0')

  // swap form
  const [amountIn, setAmountIn] = useState('100000')
  const [minOut, setMinOut] = useState('0')
  const [isAToB, setIsAToB] = useState(true)
  const [tradeId, setTradeId] = useState('0')
  const [walkSpan, setWalkSpan] = useState('1')

  const [newArrayIndex, setNewArrayIndex] = useState('0')
  const [newAuthority, setNewAuthority] = useState('')
  const [navPrice, setNavPrice] = useState('1.01')

  const myPosition = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const address = dlmmPosition(pool.state.address, publicKey, Number(binId))
      try {
        const info = await connection.getAccountInfo(address)
        if (!info) return null
        return decodeBinPosition({ pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports })
      } catch {
        return null
      }
    },
    [connection, publicKey, pool.state?.address.toBase58(), binId],
  )

  const balances = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const read = async (mint: PublicKey) => {
        try {
          return (await loadTokenAccount(connection, ata(publicKey, mint, TOKEN_PROGRAM_ID))).amount
        } catch {
          return 0n
        }
      }
      return { a: await read(pool.state.mintA), b: await read(pool.state.mintB) }
    },
    [connection, publicKey, pool.state?.address.toBase58()],
  )

  const walkIndexes = () => {
    const active = pool.state?.activeBinId ?? Number(activeBinId)
    const span = Math.max(1, Number(walkSpan) || 1)
    const first = dlmmBinArrayIndex(active)
    const indexes: bigint[] = []
    // Selling A walks up in bin id (ascending arrays), selling B walks down.
    for (let offset = 0; offset < span; offset++) {
      indexes.push(isAToB ? first + BigInt(offset) : first - BigInt(offset))
    }
    return indexes
  }

  // Modelled depth profile around the live active bin. The connected wallet's
  // loaded position is overlaid when it falls inside the visible window.
  const heatBins: BinDatum[] = (() => {
    if (!pool.state) return []
    const active = pool.state.activeBinId
    const selected = Number(binId)
    // Centre on the active bin unless the user has picked a bin beyond the
    // window (e.g. a NAV shortcut); then follow the selection so it stays visible.
    const center = Number.isFinite(selected) && Math.abs(selected - active) > HEATMAP_SPAN ? selected : active
    const myBin = myPosition.state?.binId ?? null
    const bins: BinDatum[] = []
    for (let offset = -HEATMAP_SPAN; offset <= HEATMAP_SPAN; offset++) {
      const id = center + offset
      const base = Math.exp(-Math.abs(id - active) / 9)
      const own = myBin === id ? 0.9 : 0
      const value = base + own
      bins.push({
        id,
        value,
        active: id === active,
        overlay: own ? own / value : 0,
        title: `Bin ${id}${id === active ? ' · active' : ''} · modelled ${(base * 100).toFixed(0)}%${own ? ' · your position' : ''}`,
      })
    }
    return bins
  })()

  return (
    <>
      <Panel title="Pool selector" subtitle="DLMM pools are keyed by their two mints; bins are grouped 70 per array.">
        <FormRow>
          <Field label="Mint A">
            <TextInput value={mintA} onChange={(e) => setMintA(e.target.value)} placeholder="Base58 mint" />
          </Field>
          <Field label="Mint B">
            <TextInput value={mintB} onChange={(e) => setMintB(e.target.value)} placeholder="Base58 mint" />
          </Field>
          <Field label="Derived pool PDA">
            <TextInput readOnly value={poolAddress?.toBase58() ?? ''} placeholder="—" />
          </Field>
        </FormRow>
        {pool.error ? <StatusLine error={pool.error} /> : <EmptyHint>No pool loaded yet.</EmptyHint>}
      </Panel>

      {pool.state ? (
        <>
          {pool.state.paused ? <PausedBanner pool={pool.state.address} /> : null}

          <div className="grid-2">
            <Panel
              title="Pool state"
              aside={
                <>
                  {wiring.kycConfig ? <Badge tone="warn">KYC</Badge> : null}{' '}
                  {pool.state.paused ? <Badge tone="bad">paused</Badge> : <Badge tone="good">live</Badge>}{' '}
                  {wiring.oracle ? <Badge tone="good">oracle</Badge> : null}{' '}
                  {wiring.settlement ? <Badge tone="warn">T+N</Badge> : null}
                </>
              }
            >
              <div className="animate-fade-in-up">
                <KeyValue
                  rows={[
                    ['Active bin', pool.state.activeBinId.toString()],
                    ['Bin step (bps)', pool.state.binStep.toString()],
                    ['Base fee (bps)', pool.state.baseFeeBps.toString()],
                    ['Authority', short(pool.state.authority)],
                    ['Bins / array', DLMM_BINS_PER_ARRAY.toString()],
                    ['Mint A', <span className="mono animate-pulse">{short(pool.state.mintA)}</span>],
                    ['Mint B', <span className="mono animate-pulse">{short(pool.state.mintB)}</span>],
                  ]}
                />
              </div>
            </Panel>
            <Panel title="My position & balances" subtitle={`Position at bin ${binId} for the connected wallet.`}>
              {myPosition.state ? (
                <KeyValue rows={[['Bin', myPosition.state.binId.toString()], ['Shares', myPosition.state.shares.toString()]]} />
              ) : (
                <EmptyHint>No position at this bin.</EmptyHint>
              )}
              {balances.state ? (
                <KeyValue rows={[['Wallet A', balances.state.a.toString()], ['Wallet B', balances.state.b.toString()]]} />
              ) : null}
            </Panel>
          </div>

          <div className="grid-2">
            <Panel title="Swap" subtitle={`Walks ${walkIndexes().length} bin array(s) starting at the active bin.`}>
              <div className="form">
                <FormRow>
                  <Field label="Amount in">
                    <NumberInput value={amountIn} onChange={(e) => setAmountIn(e.target.value)} />
                  </Field>
                  <Field label="Min out">
                    <NumberInput value={minOut} onChange={(e) => setMinOut(e.target.value)} />
                  </Field>
                  <Field label="Direction">
                    <select className="text-input" value={isAToB ? 'a2b' : 'b2a'} onChange={(e) => setIsAToB(e.target.value === 'a2b')}>
                      <option value="a2b">Sell A (bins up)</option>
                      <option value="b2a">Sell B (bins down)</option>
                    </select>
                  </Field>
                  <Field label="Arrays to walk">
                    <NumberInput value={walkSpan} onChange={(e) => setWalkSpan(e.target.value)} />
                  </Field>
                  <Field label="Trade id" hint="Settlement pools only">
                    <NumberInput value={tradeId} onChange={(e) => setTradeId(e.target.value)} />
                  </Field>
                </FormRow>
                <ActionButton
                  label="Send swap"
                  onClick={() =>
                    void send('DLMM swap', [
                      dlmmSwap({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        amountIn: BigInt(amountIn || '0'),
                        minAmountOut: BigInt(minOut || '0'),
                        isAToB,
                        tradeId: BigInt(tradeId || '0'),
                        binArrayIndexes: walkIndexes(),
                        wiring,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>

            <Panel title="Liquidity" subtitle="Deposit one or both legs into a specific bin; shares are minted against value in token A.">
              <div className="form">
                <FormRow>
                  <Field label="Bin id">
                    <NumberInput value={binId} onChange={(e) => setBinId(e.target.value)} />
                  </Field>
                  <Field label="Amount A">
                    <NumberInput value={amountA} onChange={(e) => setAmountA(e.target.value)} />
                  </Field>
                  <Field label="Amount B">
                    <NumberInput value={amountB} onChange={(e) => setAmountB(e.target.value)} />
                  </Field>
                </FormRow>
                <ActionButton
                  label="Add liquidity"
                  onClick={() =>
                    void send('DLMM add liquidity', [
                      dlmmAddLiquidity({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        binId: Number(binId),
                        amountA: BigInt(amountA || '0'),
                        amountB: BigInt(amountB || '0'),
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
                <hr className="divider" />
                <FormRow>
                  <Field label="Burn shares">
                    <NumberInput value={shares} onChange={(e) => setShares(e.target.value)} />
                  </Field>
                  <Field label="Min out A">
                    <NumberInput value={minA} onChange={(e) => setMinA(e.target.value)} />
                  </Field>
                  <Field label="Min out B">
                    <NumberInput value={minB} onChange={(e) => setMinB(e.target.value)} />
                  </Field>
                </FormRow>
                <ActionButton
                  label="Remove liquidity"
                  variant="danger"
                  onClick={() =>
                    void send('DLMM remove liquidity', [
                      dlmmRemoveLiquidity({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        binId: Number(binId),
                        shares: BigInt(shares || '0'),
                        minAmountA: BigInt(minA || '0'),
                        minAmountB: BigInt(minB || '0'),
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>
          </div>

          {wiring.oracle ? (
            <OracleRiskPanel
              pool={pool.state.address}
              spotPrice={binPrice(pool.state.activeBinId, pool.state.binStep)}
              spotLabel="Spot (bin)"
            />
          ) : null}

          <Panel
            title="Bin liquidity heatmap"
            subtitle="Depth across discrete bins. Click a bin to load it into the liquidity form; hand-set price shortcuts land exactly on the NAV bins."
          >
            <BinHistogram
              bins={heatBins}
              selectedId={Number(binId)}
              onSelect={(id) => setBinId(String(id))}
            />
            <div className="viz-axis">
              <span>bin {heatBins[0]?.id}</span>
              <span>active {pool.state.activeBinId}</span>
              <span>bin {heatBins[heatBins.length - 1]?.id}</span>
            </div>
            <div className="viz-legend">
              <span><i style={{ background: 'linear-gradient(180deg, #7dd3fc, #0e7490)' }} />modelled depth</span>
              <span><i style={{ background: 'repeating-linear-gradient(45deg, rgba(255,255,255,0.6) 0 2px, transparent 2px 4px)', border: '1px solid rgba(148,163,184,0.3)' }} />your position</span>
              <span><i style={{ background: 'transparent', outline: '1px solid rgba(52,211,153,0.9)' }} />active bin</span>
              <span><i className="line" style={{ background: '#93c5fd' }} />selected</span>
            </div>
            <div className="form" style={{ marginTop: 18 }}>
              <FormRow>
                <Field label="Target NAV / price" hint="Bins are geometric: price ≈ (1 + bin_step/10000)^bin_id.">
                  <NumberInput value={navPrice} onChange={(e) => setNavPrice(e.target.value)} />
                </Field>
                <ActionButton
                  label="Select bin at NAV"
                  variant="secondary"
                  onClick={() => setBinId(String(binIdForPrice(Number(navPrice) || 1, pool.state!.binStep)))}
                />
                <ActionButton label="NAV $1.00" variant="secondary" onClick={() => setBinId('0')} />
                <ActionButton
                  label="NAV $1.01"
                  variant="secondary"
                  onClick={() => setBinId(String(binIdForPrice(1.01, pool.state!.binStep)))}
                />
              </FormRow>
            </div>
            <p className="viz-note">
              On-chain accounts here expose pool-level state and per-owner positions, not per-bin reserves — the depth
              profile is modelled from the active bin and your loaded position at bin {Number(binId)}. The active-bin and
              your-position markers are read from chain.
            </p>
          </Panel>

          <Panel title="Admin & maintenance" subtitle="Bin arrays are permissionless but bounded near the active bin; pause and authority are admin-only.">
            <div className="form">
              <FormRow>
                <Field label="New bin array index">
                  <NumberInput value={newArrayIndex} onChange={(e) => setNewArrayIndex(e.target.value)} />
                </Field>
                <ActionButton
                  label="Initialize bin array"
                  variant="secondary"
                  onClick={() =>
                    void send('DLMM init bin array', [
                      dlmmInitializeBinArray({
                        payer: publicKey!,
                        pool: pool.state!.address,
                        index: BigInt(newArrayIndex || '0'),
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
                <Field label="New authority">
                  <TextInput value={newAuthority} onChange={(e) => setNewAuthority(e.target.value)} placeholder="Base58 pubkey" />
                </Field>
              </FormRow>
              <FormRow>
                <ActionButton
                  label={pool.state.paused ? 'Unpause pool' : 'Pause pool'}
                  variant="danger"
                  onClick={() => void send('DLMM pause', [dlmmSetPaused(pool.state!.address, publicKey!, !pool.state!.paused)])}
                  disabled={!publicKey}
                />
                <ActionButton
                  label="Rotate authority"
                  variant="secondary"
                  onClick={() => void send('DLMM set authority', [dlmmSetAuthority(pool.state!.address, publicKey!, new PublicKey(newAuthority))])}
                  disabled={!publicKey || !newAuthority}
                />
                <ActionButton
                  label="Trip circuit breaker"
                  variant="danger"
                  onClick={() => void send('DLMM trip breaker', [dlmmTripBreaker(pool.state!.address)])}
                />
              </FormRow>
            </div>
          </Panel>
        </>
      ) : null}

      <Panel title="Create DLMM pool" subtitle="Bin step in bps (price per bin ≈ 2×bin_step bps). Rails fixed at birth.">
        <div className="form">
          <FormRow>
            <Field label="Active bin id">
              <NumberInput value={activeBinId} onChange={(e) => setActiveBinId(e.target.value)} />
            </Field>
            <Field label="Bin step (bps)">
              <NumberInput value={binStep} onChange={(e) => setBinStep(e.target.value)} />
            </Field>
            <Field label="Base fee (bps)">
              <NumberInput value={baseFeeBps} onChange={(e) => setBaseFeeBps(e.target.value)} />
            </Field>
          </FormRow>
          <FormRow>
            <Field label="Require KYC">
              <select className="text-input" value={useKyc ? 'on' : 'off'} onChange={(e) => setUseKyc(e.target.value === 'on')}>
                <option value="off">Off</option>
                <option value="on">On</option>
              </select>
            </Field>
            <Field label="Oracle deviation (bps, 0 = off)">
              <NumberInput value={deviationBps} onChange={(e) => setDeviationBps(e.target.value)} />
            </Field>
            <Field label="Settlement routing">
              <select className="text-input" value={useSettlement ? 'on' : 'off'} onChange={(e) => setUseSettlement(e.target.value === 'on')}>
                <option value="off">Off</option>
                <option value="on">On</option>
              </select>
            </Field>
            <Field label="T+N slots">
              <NumberInput value={settlementSlots} onChange={(e) => setSettlementSlots(e.target.value)} />
            </Field>
          </FormRow>
          <ActionButton
            label="Initialize pool"
            onClick={() =>
              void send('DLMM initialize pool', [
                dlmmInitializePool({
                  payer: publicKey!,
                  mintA: new PublicKey(mintA),
                  mintB: new PublicKey(mintB),
                  arrayIndex: dlmmBinArrayIndex(Number(activeBinId)),
                  activeBinId: Number(activeBinId),
                  binStep: Number(binStep),
                  baseFeeBps: Number(baseFeeBps),
                  authority: publicKey!,
                  kycConfig: useKyc ? kycConfig() : null,
                  maxDeviationBps: Number(deviationBps),
                  hooksAuthority: useSettlement ? hookAuthority() : null,
                  settlementSlots: BigInt(useSettlement ? settlementSlots || '0' : '0'),
                }),
              ])
            }
            disabled={!publicKey || !mintA || !mintB}
          />
        </div>
      </Panel>
    </>
  )
}

const HEATMAP_SPAN = 30

/** Bin id whose geometric price ≈ `price` for a bin step in bps (bin 0 = 1.0). */
function binIdForPrice(price: number, binStepBps: number): number {
  const step = 1 + (binStepBps > 0 ? binStepBps : 10) / 10_000
  return Math.round(Math.log(Math.max(price, 1e-9)) / Math.log(step))
}

function useMemoPool(mintA: string, mintB: string): PublicKey | null {
  try {
    if (!mintA || !mintB) return null
    return dlmmPool(new PublicKey(mintA), new PublicKey(mintB))
  } catch {
    return null
  }
}

function short(address: PublicKey): string {
  const text = address.toBase58()
  return `${text.slice(0, 6)}…${text.slice(-6)}`
}
