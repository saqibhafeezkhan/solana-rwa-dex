import { useState } from 'react'
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
  StatusLine,
  TextInput,
} from '../components'
import { useAccountState } from '../useAccountState'
import {
  MAVERICK_BINS_PER_ARRAY,
  ata,
  hookAuthority,
  kycConfig,
  settlementEscrowAuthority,
  maverickBinArray,
  maverickBinArrayIndex,
  maverickPool,
  maverickPosition,
} from '@rwa-dex/sdk'
import {
  decodeBinPosition,
  loadMaverickBinArray,
  loadMaverickPool,
  loadTokenAccount,
} from '@rwa-dex/sdk'
import {
  maverickAddLiquidity,
  maverickInitializeBinArray,
  maverickInitializePool,
  maverickMoveBins,
  maverickRemoveLiquidity,
  maverickSetAuthority,
  maverickSetPaused,
  maverickSwap,
  maverickTripBreaker,
  type MaverickBinKind,
} from '@rwa-dex/sdk'
import { deriveMaverickHopArrayIndexes } from '@rwa-dex/sdk'
import { SAMPLE_MAVERICK_POOL } from '../sampleData'
import { useSampleMints } from '../useSampleMints'
import { useTokenProgram } from '../useTokenProgram'
import { hookAccountsForTransfers } from '@rwa-dex/sdk'
import { BinHistogram, type BinDatum } from '../charts'
import { OracleRiskPanel, PausedBanner } from '../OracleRiskPanel'
import { binPrice, q64ToPrice } from '../pricing'

const MODE_LABELS: Record<MaverickBinKind, string> = {
  static: 'Static',
  right: 'Right →',
  left: '← Left',
  both: 'Both ↔',
}

export function MaverickPage() {
  const { connection, publicKey, send, sampleMode } = useApp()

  const { mintA, setMintA, mintB, setMintB } = useSampleMints(SAMPLE_MAVERICK_POOL, sampleMode)
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      if (sampleMode) return SAMPLE_MAVERICK_POOL
      return loadMaverickPool(connection, poolAddress)
    },
    [connection, poolAddress?.toBase58(), sampleMode],
  )

  // ATAs and the token-program account must match the pool mint's program;
  // a Token-2022 pool's accounts are not derivable under legacy SPL Token.
  const tokenProgramId = useTokenProgram(pool.state?.mintA)
  const pendingMintA = (() => {
    try {
      return mintA ? new PublicKey(mintA) : null
    } catch {
      return null
    }
  })()
  const pendingTokenProgramId = useTokenProgram(pendingMintA)

  // The mints' transfer-hook accounts, for every owner a transfer can involve:
  // the wallet, the pool PDA that owns the vaults, and the hook authority whose
  // custody ATA receives a settlement-routed swap's output. One list covers
  // every instruction on the page — Token-2022 resolves each transfer's own meta
  // list against the accounts the instruction carries and ignores the rest — so
  // a hook-gated pool trades with the same builders a plain one does. Undefined
  // for the ordinary mint with no hook, which needs no extra accounts at all.
  const hookAccounts = useAccountState(
    async () => {
      if (!pool.state || sampleMode) return undefined
      // The settlement escrow authority is the destination owner when a
      // settlement-routed swap's output is moved from the hook authority's
      // custody account into escrow, so its entry has to travel with the swap
      // too (the AMM forwards these accounts into that second transfer).
      const owners = [publicKey, pool.state.address, hookAuthority(), settlementEscrowAuthority()].filter(
        (owner): owner is PublicKey => owner !== null,
      )
      return hookAccountsForTransfers(
        connection,
        [pool.state.mintA, pool.state.mintB],
        owners,
        owners,
      )
    },
    [connection, pool.state?.address.toBase58(), publicKey?.toBase58()],
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

  // init pool
  const [activeBinId, setActiveBinId] = useState('0')
  const [binStep, setBinStep] = useState('10')
  const [baseFeeBps, setBaseFeeBps] = useState('20')
  const [useKyc, setUseKyc] = useState(false)
  const [deviationBps, setDeviationBps] = useState('0')
  const [useSettlement, setUseSettlement] = useState(false)
  const [settlementSlots, setSettlementSlots] = useState('100')

  // liquidity
  const [binId, setBinId] = useState('0')
  const [kind, setKind] = useState<MaverickBinKind>('static')
  const [amountA, setAmountA] = useState('1000000')
  const [amountB, setAmountB] = useState('1000000')
  const [shares, setShares] = useState('1000')
  const [minA, setMinA] = useState('0')
  const [minB, setMinB] = useState('0')

  // mode simulator
  const [priceShiftPct, setPriceShiftPct] = useState(3)
  const [volumeMultiple, setVolumeMultiple] = useState(3)

  // move bins
  const [fromBinId, setFromBinId] = useState('-60')
  const [toBinId, setToBinId] = useState('-20')

  // admin
  const [newArrayIndex, setNewArrayIndex] = useState('0')
  const [newAuthority, setNewAuthority] = useState('')

  // on-chain bin read-out
  const [inspectBinId, setInspectBinId] = useState('0')

  // swap
  const [amountIn, setAmountIn] = useState('100000')
  const [minOut, setMinOut] = useState('0')
  const [isAToB, setIsAToB] = useState(true)
  const [tradeId, setTradeId] = useState('0')
  const [walkSpan, setWalkSpan] = useState('1')

  const myPosition = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const address = maverickPosition(pool.state.address, publicKey, Number(binId))
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
          return (await loadTokenAccount(connection, ata(publicKey, mint, tokenProgramId))).amount
        } catch {
          return 0n
        }
      }
      return { a: await read(pool.state.mintA), b: await read(pool.state.mintB) }
    },
    [connection, publicKey, pool.state?.address.toBase58(), tokenProgramId.toBase58()],
  )

  // The move chain lives on chain: each moved bin records where its shares went,
  // so the arrays a withdrawal must supply are read rather than typed.
  const hopChain = useAccountState(
    async () => {
      if (!pool.state) return null
      return await deriveMaverickHopArrayIndexes(connection, pool.state.address, Number(binId))
    },
    [connection, pool.state?.address.toBase58(), binId],
  )

  // The array the inspected bin lives in, read straight off the chain: bin
  // kinds, funding and the move chain are never derived client-side.
  const inspectArrayIndex = maverickBinArrayIndex(Number(inspectBinId || '0'))
  const inspectAddress = pool.state ? maverickBinArray(pool.state.address, inspectArrayIndex) : null
  const inspect = useAccountState(
    async () => {
      if (!inspectAddress) return null
      try {
        return await loadMaverickBinArray(connection, inspectAddress)
      } catch {
        return null
      }
    },
    [connection, inspectAddress?.toBase58()],
  )
  const fundedBins = (inspect.state?.bins ?? []).filter((bin) => bin.supply > 0n || bin.moved)
  const movedBins = fundedBins.filter((bin) => bin.moved)

  // Mode simulator: where the bin's liquidity ends up after a price move and
  // what that implies for fees versus impermanent loss. Everything here is a
  // forward projection from live pool parameters, not an on-chain reading.
  const projection = (() => {
    const stepBps = pool.state && pool.state.binStep > 0 ? pool.state.binStep : 10
    const active = pool.state?.activeBinId ?? 0
    const shiftBins = Math.round(priceShiftPct / 100 / (stepBps / 10_000))
    const appliedShift =
      kind === 'static'
        ? 0
        : kind === 'right'
          ? Math.max(0, shiftBins)
          : kind === 'left'
            ? Math.min(0, shiftBins)
            : shiftBins
    const span = 40
    const makeBins = (center: number): BinDatum[] => {
      const bins: BinDatum[] = []
      for (let offset = -span; offset <= span; offset++) {
        bins.push({
          id: center + offset,
          value: Math.exp(-Math.abs(offset) / 5),
          active: offset === 0,
          title: `Bin ${center + offset}`,
        })
      }
      return bins
    }
    const inRangePct = Math.max(0, 1 - Math.abs(appliedShift) / (span + 1))
    const ratio = 1 + priceShiftPct / 100
    const il = ratio > 0 ? (2 * Math.sqrt(ratio)) / (1 + ratio) - 1 : 0
    const dailyFeePct = (pool.state ? pool.state.baseFeeBps / 10_000 : 0) * volumeMultiple * inRangePct * 100
    const ilPct = il * 100
    return {
      appliedShift,
      beforeBins: makeBins(active),
      afterBins: makeBins(active + appliedShift),
      inRangePct,
      dailyFeePct,
      ilPct,
      netPct: dailyFeePct + ilPct,
      projectedCenter: active + appliedShift,
    }
  })()

  const walkIndexes = () => {
    const active = pool.state?.activeBinId ?? Number(activeBinId)
    const span = Math.max(1, Number(walkSpan) || 1)
    const first = maverickBinArrayIndex(active)
    const indexes: bigint[] = []
    for (let offset = 0; offset < span; offset++) {
      indexes.push(isAToB ? first + BigInt(offset) : first - BigInt(offset))
    }
    return indexes
  }

  return (
    <>
      <Panel title="Pool selector" subtitle="Maverick pools share the DLMM address scheme; bins carry a kind and a move chain.">
        <FormRow>
          <Field
            label="Mint A"
            hint={sampleMode ? 'Sample mode shows the demo pool’s mints — switch it off to enter your own.' : undefined}
          >
            <TextInput value={mintA} onChange={(e) => setMintA(e.target.value)} placeholder="Base58 mint" readOnly={sampleMode} />
          </Field>
          <Field label="Mint B">
            <TextInput value={mintB} onChange={(e) => setMintB(e.target.value)} placeholder="Base58 mint" readOnly={sampleMode} />
          </Field>
          <Field label="Derived pool PDA">
            <TextInput readOnly value={poolAddress?.toBase58() ?? ''} placeholder="—" />
          </Field>
        </FormRow>
        {pool.error ? <StatusLine error={pool.error} /> : <EmptyHint>No pool loaded yet.</EmptyHint>}
      </Panel>

      {pool.state ? (
        <>
          {pool.state.paused ? <PausedBanner /> : null}

          <div className="grid-2">
            <Panel
              title="Pool state"
              aside={
                <>
                  {pool.state.paused ? <Badge tone="bad">paused</Badge> : <Badge tone="good">live</Badge>}{' '}
                  {wiring.oracle ? <Badge tone="good">oracle</Badge> : null}{' '}
                  {wiring.settlement ? <Badge tone="warn">T+N</Badge> : null}
                </>
              }
            >
              <KeyValue
                rows={[
                  ['Active bin', pool.state.activeBinId.toString()],
                  ['Bin step (bps)', pool.state.binStep.toString()],
                  ['Base fee (bps)', pool.state.baseFeeBps.toString()],
                  ['Bins / array', MAVERICK_BINS_PER_ARRAY.toString()],
                  ['Authority', short(pool.state.authority)],
                ]}
              />
            </Panel>
            <Panel title="My position & balances" subtitle={`Position at bin ${binId}. Shares follow the move chain on withdrawal.`}>
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

          {wiring.oracle ? (
            <OracleRiskPanel
              pool={pool.state.address}
              spotPrice={binPrice(pool.state.activeBinId, pool.state.binStep)}
              spotLabel="Spot (bin)"
              priceKind="price"
              rawScale={10 ** (pool.state.decimalsA - pool.state.decimalsB)}
            />
          ) : null}

          <div className="grid-2">
            <Panel title="Add liquidity" subtitle="The first deposit into a bin fixes its kind; moving kinds need a static baseline first.">
              <div className="form">
                <FormRow>
                  <Field label="Bin id">
                    <NumberInput value={binId} onChange={(e) => setBinId(e.target.value)} />
                  </Field>
                  <Field label="Kind">
                    <select className="text-input" value={kind} onChange={(e) => setKind(e.target.value as MaverickBinKind)}>
                      <option value="static">Static</option>
                      <option value="right">Right (follow price up)</option>
                      <option value="left">Left (follow price down)</option>
                      <option value="both">Both</option>
                    </select>
                  </Field>
                </FormRow>
                <FormRow>
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
                    void send('Maverick add liquidity', [
                      maverickAddLiquidity({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        binId: Number(binId),
                        kind,
                        amountA: BigInt(amountA || '0'),
                        amountB: BigInt(amountB || '0'),
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>

            <Panel
              title="Remove liquidity"
              subtitle="The hop arrays are derived from the bin move chain; nothing to type even after a keeper has merged the bin."
            >
              <div className="form">
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
                <Field
                  label="Hop arrays (derived)"
                  hint="Read from the bin arrays; '—' means the position's bin was never moved."
                >
                  <TextInput
                    readOnly
                    value={
                      hopChain.state && hopChain.state.length > 0
                        ? hopChain.state.map((value) => value.toString()).join(', ')
                        : '—'
                    }
                  />
                </Field>
                <StatusLine error={hopChain.error} />
                <ActionButton
                  label="Remove liquidity"
                  variant="danger"
                  onClick={() =>
                    void send('Maverick remove liquidity', [
                      maverickRemoveLiquidity({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        binId: Number(binId),
                        shares: BigInt(shares || '0'),
                        minAmountA: BigInt(minA || '0'),
                        minAmountB: BigInt(minB || '0'),
                        hopArrayIndexes: hopChain.state ?? [],
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>
          </div>

          <div className="grid-2">
            <Panel title="Move bins (keeper)" subtitle="Permissionless: relocate out-of-range liquidity next to the active bin.">
              <div className="form">
                <FormRow>
                  <Field label="From bin">
                    <NumberInput value={fromBinId} onChange={(e) => setFromBinId(e.target.value)} />
                  </Field>
                  <Field label="To bin">
                    <NumberInput value={toBinId} onChange={(e) => setToBinId(e.target.value)} />
                  </Field>
                  <ActionButton
                    label="Move bins"
                    variant="secondary"
                    onClick={() =>
                      void send('Maverick move bins', [
                        maverickMoveBins({
                          pool: pool.state!.address,
                          fromArrayIndex: maverickBinArrayIndex(Number(fromBinId)),
                          fromBinId: Number(fromBinId),
                          toBinId: Number(toBinId),
                        }),
                      ])
                    }
                  />
                </FormRow>
              </div>
            </Panel>

            <Panel title="Swap" subtitle={`Walks ${walkIndexes().length} bin array(s) from the active bin.`}>
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
                    void send('Maverick swap', [
                      maverickSwap({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        amountIn: BigInt(amountIn || '0'),
                        minAmountOut: BigInt(minOut || '0'),
                        isAToB,
                        tradeId: BigInt(tradeId || '0'),
                        binArrayIndexes: walkIndexes(),
                        wiring,
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>
          </div>

          <Panel
            title="On-chain bins (kind & move chain)"
            subtitle="Read straight from the array account: which bins hold liquidity, the kind fixed by the first deposit, and where a keeper's move_bins sent each bin's shares."
            aside={
              <Badge tone={movedBins.length > 0 ? 'warn' : 'neutral'}>
                {inspect.state
                  ? `${fundedBins.length} funded / ${movedBins.length} moved`
                  : 'no array'}
              </Badge>
            }
          >
            <FormRow>
              <Field label="Bin id to inspect" hint={`Array ${inspectArrayIndex} covers bins ${Number(inspectArrayIndex) * MAVERICK_BINS_PER_ARRAY} – ${Number(inspectArrayIndex) * MAVERICK_BINS_PER_ARRAY + MAVERICK_BINS_PER_ARRAY - 1}.`}>
                <NumberInput value={inspectBinId} onChange={(e) => setInspectBinId(e.target.value)} />
              </Field>
              <ActionButton
                label="Inspect active bin"
                variant="secondary"
                onClick={() => setInspectBinId(pool.state!.activeBinId.toString())}
              />
              <ActionButton label="Refresh" variant="secondary" onClick={() => void inspect.refresh()} />
            </FormRow>
            <StatusLine error={inspect.error} />
            {inspect.state && inspect.state.pool.equals(pool.state.address) ? (
              <DataTable
                csvName="rwadex-maverick-bins"
                empty={`No funded bins in array ${inspect.state.index}.`}
                rows={fundedBins}
                columns={[
                  { header: 'Bin', cell: (bin) => bin.binId.toString(), text: (bin) => bin.binId.toString() },
                  {
                    header: 'Kind',
                    cell: (bin) => (
                      <Badge tone={bin.kind === 'static' ? 'neutral' : 'good'}>{MODE_LABELS[bin.kind]}</Badge>
                    ),
                    text: (bin) => bin.kind,
                  },
                  {
                    header: 'Price (raw A/B)',
                    cell: (bin) => q64ToPrice(bin.priceX64).toLocaleString(undefined, { maximumFractionDigits: 6 }),
                    text: (bin) => q64ToPrice(bin.priceX64).toString(),
                  },
                  { header: 'Reserve A', cell: (bin) => bin.reserveA.toString(), text: (bin) => bin.reserveA.toString() },
                  { header: 'Reserve B', cell: (bin) => bin.reserveB.toString(), text: (bin) => bin.reserveB.toString() },
                  { header: 'Shares', cell: (bin) => bin.supply.toString(), text: (bin) => bin.supply.toString() },
                  {
                    header: 'Move chain',
                    cell: (bin) =>
                      bin.moved ? (
                        <span className="mono">
                          → bin {bin.toBinId} @ {q64ToPrice(bin.shareRatio).toFixed(4)}×
                        </span>
                      ) : (
                        '—'
                      ),
                    text: (bin) =>
                      bin.moved ? `bin ${bin.toBinId} @ ${bin.shareRatio} Q64.64` : 'not moved',
                  },
                ]}
              />
            ) : inspect.state ? (
              <EmptyHint>
                Array {inspect.state.index} belongs to pool {short(inspect.state.pool)} — not the loaded pool.
              </EmptyHint>
            ) : (
              <EmptyHint>
                No bin array at index {inspectArrayIndex.toString()} yet — open it from the admin panel, then fund a bin.
              </EmptyHint>
            )}
            <p className="viz-note">
              Reserves are the tokens actually backing each bin and shares are its LP supply; a moved bin keeps its shares
              but zeroes its reserves, and withdrawals follow the chain above. This read-out is the pool's real state —
              the simulator below is the projection.
            </p>
          </Panel>

          <Panel
            title="Mode simulator"
            subtitle="Project how a bin's liquidity migrates as price moves, and what that implies for fees versus impermanent loss. The mode drives the bin kind used by add liquidity."
          >
            <div className="viz-mode-tabs">
              {(['static', 'right', 'left', 'both'] as MaverickBinKind[]).map((mode) => (
                <button
                  key={mode}
                  type="button"
                  className={`viz-mode-tab ${kind === mode ? 'is-active' : ''}`}
                  onClick={() => setKind(mode)}
                >
                  {MODE_LABELS[mode]}
                </button>
              ))}
            </div>

            <FormRow>
              <Field
                label={`Price / NAV move: ${priceShiftPct > 0 ? '+' : ''}${priceShiftPct}%`}
                hint="Positive moves the market up; bins re-centre according to the selected mode."
              >
                <input
                  className="viz-slider"
                  type="range"
                  min={-30}
                  max={30}
                  step={1}
                  value={priceShiftPct}
                  onChange={(e) => setPriceShiftPct(Number(e.target.value))}
                />
              </Field>
              <Field label="Assumed daily volume / TVL" hint="Feeds the fee projection only.">
                <NumberInput
                  value={String(volumeMultiple)}
                  onChange={(e) => setVolumeMultiple(Math.max(0, Number(e.target.value) || 0))}
                />
              </Field>
            </FormRow>

            <div className="viz-split" style={{ marginTop: 18 }}>
              <div>
                <div className="viz-meter-label">Before · active bin {pool.state.activeBinId}</div>
                <BinHistogram bins={projection.beforeBins} selectedId={Number(binId)} onSelect={(id) => setBinId(String(id))} height={140} />
              </div>
              <div>
                <div className="viz-meter-label">
                  Projected · {priceShiftPct > 0 ? '+' : ''}
                  {priceShiftPct}% ({MODE_LABELS[kind]})
                </div>
                <BinHistogram bins={projection.afterBins} height={140} />
              </div>
            </div>

            <div className="grid-4" style={{ marginTop: 18 }}>
              <div className="viz-stat">
                <div className="viz-stat-label">Bins migrated</div>
                <div className="viz-stat-value">
                  {projection.appliedShift > 0 ? '+' : ''}
                  {projection.appliedShift}
                </div>
                <div className="viz-stat-hint">mode {MODE_LABELS[kind]}</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">In-range depth</div>
                <div className="viz-stat-value">{(projection.inRangePct * 100).toFixed(0)}%</div>
                <div className="viz-stat-hint">liquidity still near the new price</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">Projected daily fees</div>
                <div className="viz-stat-value positive">+{projection.dailyFeePct.toFixed(2)}%</div>
                <div className="viz-stat-hint">model · {volumeMultiple}× daily volume</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">Impermanent loss</div>
                <div className={`viz-stat-value ${projection.ilPct < 0 ? 'negative' : 'positive'}`}>
                  {projection.ilPct.toFixed(2)}%
                </div>
                <div className="viz-stat-hint">vs simply holding</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">Net vs holding</div>
                <div className={`viz-stat-value ${projection.netPct < 0 ? 'negative' : 'positive'}`}>
                  {projection.netPct >= 0 ? '+' : ''}
                  {projection.netPct.toFixed(2)}%
                </div>
                <div className="viz-stat-hint">fees + IL (model)</div>
              </div>
            </div>

            <div className="form" style={{ marginTop: 16 }}>
              <ActionButton
                label={`Stage bin ${projection.projectedCenter} for add liquidity`}
                variant="secondary"
                onClick={() => setBinId(String(projection.projectedCenter))}
              />
            </div>
            <p className="viz-note">
              Fees and impermanent loss are projections from the pool's live base fee, bin step and your volume assumption.
              The mode writes the real bin kind on the next add-liquidity call.
            </p>
          </Panel>

          <Panel title="Admin" subtitle="Opening a bin array is permissionless; pause, authority and the breaker are admin-only.">
            <FormRow>
              <Field label="New bin array index" hint="A bin only takes liquidity once its array exists.">
                <NumberInput value={newArrayIndex} onChange={(e) => setNewArrayIndex(e.target.value)} />
              </Field>
              <ActionButton
                label="Initialize bin array"
                variant="secondary"
                onClick={() =>
                  void send('Maverick init bin array', [
                    maverickInitializeBinArray(publicKey!, pool.state!.address, BigInt(newArrayIndex || '0')),
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
                onClick={() => void send('Maverick pause', [maverickSetPaused(pool.state!.address, publicKey!, !pool.state!.paused)])}
                disabled={!publicKey}
              />
              <ActionButton
                label="Rotate authority"
                variant="secondary"
                onClick={() => void send('Maverick set authority', [maverickSetAuthority(pool.state!.address, publicKey!, new PublicKey(newAuthority))])}
                disabled={!publicKey || !newAuthority}
              />
              <ActionButton
                label="Trip circuit breaker"
                variant="danger"
                onClick={() => void send('Maverick trip breaker', [maverickTripBreaker(pool.state!.address)])}
              />
            </FormRow>
          </Panel>
        </>
      ) : null}

      <Panel title="Create Maverick pool">
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
              void send('Maverick initialize pool', [
                maverickInitializePool({
                  payer: publicKey!,
                  mintA: new PublicKey(mintA),
                  mintB: new PublicKey(mintB),
                  arrayIndex: maverickBinArrayIndex(Number(activeBinId)),
                  activeBinId: Number(activeBinId),
                  binStep: Number(binStep),
                  baseFeeBps: Number(baseFeeBps),
                  authority: publicKey!,
                  kycConfig: useKyc ? kycConfig() : null,
                  maxDeviationBps: Number(deviationBps),
                  hooksAuthority: useSettlement ? hookAuthority() : null,
                  settlementSlots: BigInt(useSettlement ? settlementSlots || '0' : '0'),
                  tokenProgramId: pendingTokenProgramId,
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

function useMemoPool(mintA: string, mintB: string): PublicKey | null {
  try {
    if (!mintA || !mintB) return null
    return maverickPool(new PublicKey(mintA), new PublicKey(mintB))
  } catch {
    return null
  }
}

function short(address: PublicKey): string {
  const text = address.toBase58()
  return `${text.slice(0, 6)}…${text.slice(-6)}`
}
