import { useState } from 'react'
import { PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
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
  ata,
  clmmPool,
  clmmPosition,
  kycConfig,
  oracleState,
  sqrtPriceX64FromPrice,
} from '../../core/pdas'
import {
  decodeClmmPosition,
  loadClmmPool,
  loadOracleState,
  loadTokenAccount,
} from '../../core/decoders'
import {
  clmmAddLiquidity,
  clmmCollectFees,
  clmmInitializePool,
  clmmRemoveLiquidity,
  clmmSetAuthority,
  clmmSetPaused,
  clmmSwap,
  clmmTripBreaker,
} from '../../core/instructions-clmm'
import { hookAuthority, settlementEscrowAuthority } from '../../core/pdas'
import { hookAccountsForTransfers, transferFeeForMint } from '../../core/connection'
import type { MintTransferFee } from '../../core/decoders'
import { SAMPLE_CLMM_POOL } from '../../core/sampleData'
import { useSampleMints } from '../useSampleMints'
import { useTokenProgram } from '../useTokenProgram'
import { Sparkline } from '../charts'
import { OracleRiskPanel, PausedBanner } from '../OracleRiskPanel'
import { sqrtPriceToPrice } from '../pricing'

export function ClmmPage() {
  const { connection, publicKey, send, sampleMode } = useApp()

  // ---- pool selector -------------------------------------------------
  const { mintA, setMintA, mintB, setMintB } = useSampleMints(SAMPLE_CLMM_POOL, sampleMode)
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      if (sampleMode) return SAMPLE_CLMM_POOL
      return loadClmmPool(connection, poolAddress)
    },
    [connection, poolAddress?.toBase58(), sampleMode],
  )

  // The pool mint's token program: legacy SPL unless the mint is a 2022
  // account. ATAs and the token-program account must agree with it.
  const tokenProgramId = useTokenProgram(pool.state?.mintA)
  const pendingMintA = (() => {
    try {
      return mintA ? new PublicKey(mintA) : null
    } catch {
      return null
    }
  })()
  const pendingTokenProgramId = useTokenProgram(pendingMintA)

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

  // ---- create pool ---------------------------------------------------
  const [initPrice, setInitPrice] = useState('1.0')
  const [tickSpacing, setTickSpacing] = useState('64')
  const [feePips, setFeePips] = useState('3000')
  const [useKyc, setUseKyc] = useState(false)
  const [deviationBps, setDeviationBps] = useState('0')
  const [useSettlement, setUseSettlement] = useState(false)
  const [settlementSlots, setSettlementSlots] = useState('100')

  // ---- liquidity -----------------------------------------------------
  const [lowerTick, setLowerTick] = useState('-1280')
  const [upperTick, setUpperTick] = useState('1280')
  const [liquidity, setLiquidity] = useState('1000000000000')
  const [removeShares, setRemoveShares] = useState('500000000000')
  const [minA, setMinA] = useState('0')
  const [minB, setMinB] = useState('0')

  // ---- swap ----------------------------------------------------------
  const [amountIn, setAmountIn] = useState('1000000')
  const [minOut, setMinOut] = useState('0')
  const [isAToB, setIsAToB] = useState(true)
  const [tradeId, setTradeId] = useState('0')
  const [walkTicks, setWalkTicks] = useState('')

  // Yield projection input; the fee rate itself is read from the pool.
  const [volumeMultiple, setVolumeMultiple] = useState(3)

  // admin
  const [newAuthority, setNewAuthority] = useState('')

  const myPosition = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const address = clmmPosition(
        pool.state.address,
        publicKey,
        Number(lowerTick),
        Number(upperTick),
      )
      try {
        const info = await connection.getAccountInfo(address)
        if (!info) return null
        return decodeClmmPosition({ pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports })
      } catch {
        return null
      }
    },
    [connection, publicKey, pool.state?.address.toBase58(), lowerTick, upperTick],
  )

  const myBalances = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const read = async (mint: PublicKey) => {
        try {
          return (await loadTokenAccount(connection, ata(publicKey, mint, tokenProgramId))).amount
        } catch {
          return 0n
        }
      }
      return {
        a: await read(pool.state.mintA),
        b: await read(pool.state.mintB),
      }
    },
    [connection, publicKey, pool.state?.address.toBase58(), tokenProgramId.toBase58()],
  )

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

  // The mints' Token-2022 transfer fees, if any. CLMM grosses an inbound
  // transfer up by the fee in force, so the amount typed into the swap box is
  // what the vault must receive — the wallet is debited more than that. Sample
  // mode keeps the fixture's mints, which carry no extension.
  const mintFees = useAccountState(
    async () => {
      if (!pool.state || sampleMode) return null
      const [a, b] = await Promise.all([
        transferFeeForMint(connection, pool.state.mintA),
        transferFeeForMint(connection, pool.state.mintB),
      ])
      return { a, b }
    },
    [connection, pool.state?.address.toBase58(), sampleMode],
  )
  const inputFee = isAToB ? (mintFees.state?.a ?? null) : (mintFees.state?.b ?? null)
  const outputFee = isAToB ? (mintFees.state?.b ?? null) : (mintFees.state?.a ?? null)
  const amountInRaw = parseRaw(amountIn)
  const maxDebit = inputFee && amountInRaw > 0n ? maxInputDebit(amountInRaw, inputFee) : null
  const liquidityFeeNote = describeMintFees(mintFees.state?.a ?? null, mintFees.state?.b ?? null)

  const parsedWalkTicks = walkTicks
    .split(/[,\s]+/)
    .map((token) => token.trim())
    .filter((token) => token.length > 0)
    .map((token) => Number(token))
    .filter((tick) => Number.isFinite(tick))

  // Fee yield is projected from the pool's live fee rate and an assumed volume
  // multiple; the accumulator and accrued-fee figures below are on-chain.
  const feeRate = pool.state ? pool.state.feePips / 1_000_000 : 0
  const dailyYieldPct = feeRate * volumeMultiple * 100
  const apyPct = dailyYieldPct * 365
  const yieldCurve = Array.from({ length: 31 }, (_, day) => dailyYieldPct * day)

  return (
    <>
      <Panel
        title="Pool selector"
        subtitle="CLMM pools are addressed by their two mints. Load one to trade it or provide liquidity."
      >
        <FormRow>
          <Field
            label="Mint A (token0)"
            hint={sampleMode ? 'Sample mode shows the demo pool’s mints — switch it off to enter your own.' : undefined}
          >
            <TextInput value={mintA} onChange={(e) => setMintA(e.target.value)} placeholder="Base58 mint" readOnly={sampleMode} />
          </Field>
          <Field label="Mint B (token1)">
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
                  {wiring.kycConfig ? <Badge tone="warn">KYC</Badge> : null}{' '}
                  {wiring.oracle ? <Badge tone="good">oracle</Badge> : null}{' '}
                  {wiring.settlement ? <Badge tone="warn">T+N</Badge> : null}{' '}
                  {mintFees.state?.a || mintFees.state?.b ? (
                    <Badge tone="warn">transfer fee</Badge>
                  ) : null}{' '}{' '}
                  {wiring.oracle ? <OracleBadge pool={pool.state.address} /> : null}
                </>
              }
            >
              <div className="animate-fade-in-up">
                <KeyValue
                  rows={[
                    ['Price √P (Q64.64)', <span key="price" className="animate-shimmer">{pool.state.sqrtPriceX64.toString()}</span>],
                    ['Active liquidity', pool.state.liquidity.toString()],
                    ['Current tick', pool.state.tickCurrent.toString()],
                    ['Tick spacing', pool.state.tickSpacing.toString()],
                    ['Fee (pips)', `${pool.state.feePips} (${(pool.state.feePips / 1e4).toFixed(2)}%)`],
                    ['Settlement slots', pool.state.settlementSlots.toString()],
                    [
                      'Mint A',
                      <span className="mono animate-pulse" key="a">
                        {short(pool.state.mintA)}
                      </span>,
                    ],
                    [
                      'Mint B',
                      <span className="mono animate-pulse" key="b">
                        {short(pool.state.mintB)}
                      </span>,
                    ],
                  ]}
                />
              </div>
            </Panel>

            <Panel title="My position" subtitle="Positions are keyed by wallet + tick range.">
              {myPosition.state ? (
                <KeyValue
                  rows={[
                    ['Range', `${myPosition.state.lowerTick} → ${myPosition.state.upperTick}`],
                    ['Liquidity', myPosition.state.liquidity.toString()],
                    ['Fees owed A', myPosition.state.tokensOwedA.toString()],
                    ['Fees owed B', myPosition.state.tokensOwedB.toString()],
                  ]}
                />
              ) : (
                <EmptyHint>No position at this range for the connected wallet.</EmptyHint>
              )}
              {myBalances.state ? (
                <KeyValue
                  rows={[
                    ['Wallet A', myBalances.state.a.toString()],
                    ['Wallet B', myBalances.state.b.toString()],
                  ]}
                />
              ) : null}
            </Panel>
          </div>

          {wiring.oracle ? (
            <OracleRiskPanel
              pool={pool.state.address}
              spotPrice={sqrtPriceToPrice(pool.state.sqrtPriceX64)}
              spotLabel="Spot (√P²)"
            />
          ) : null}

          <div className="grid-2">
            <Panel title="Swap" subtitle="Ticks the trade may cross go in the walk box, in walk order (descending selling A→B, ascending selling B→A).">
              <div className="form">
                <FormRow>
                  <Field
                    label="Amount in (raw)"
                    hint={
                      inputFee
                        ? `This mint charges a ${percent(inputFee.basisPoints)} transfer fee; the pool adds it on top` +
                          (maxDebit === null ? '' : `, so up to ${maxDebit.toString()} raw is debited`)
                        : 'The input the pool prices — no transfer fee on this mint'
                    }
                  >
                    <NumberInput value={amountIn} onChange={(e) => setAmountIn(e.target.value)} />
                  </Field>
                  <Field
                    label="Min amount out"
                    hint={
                      outputFee
                        ? `Checked against what leaves the vault; the mint's ${percent(outputFee.basisPoints)} fee comes out of what you receive`
                        : undefined
                    }
                  >
                    <NumberInput value={minOut} onChange={(e) => setMinOut(e.target.value)} />
                  </Field>
                  <Field label="Direction">
                    <select className="text-input" value={isAToB ? 'a2b' : 'b2a'} onChange={(e) => setIsAToB(e.target.value === 'a2b')}>
                      <option value="a2b">Sell A → Buy B</option>
                      <option value="b2a">Sell B → Buy A</option>
                    </select>
                  </Field>
                  <Field label="Trade id" hint="Non-zero only on settlement-wired pools">
                    <NumberInput value={tradeId} onChange={(e) => setTradeId(e.target.value)} />
                  </Field>
                </FormRow>
                <Field label="Tick walk (comma-separated)" hint="e.g. 1280, 2560, 3840 — initialized ticks in the direction of travel">
                  <TextInput value={walkTicks} onChange={(e) => setWalkTicks(e.target.value)} placeholder="1280, 2560" />
                </Field>
                <ActionButton
                  label="Send swap"
                  onClick={() =>
                    void send('CLMM swap', [
                      clmmSwap({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        amountIn: BigInt(amountIn || '0'),
                        minAmountOut: BigInt(minOut || '0'),
                        isAToB,
                        tradeId: BigInt(tradeId || '0'),
                        ticks: parsedWalkTicks,
                        wiring,
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey || !amountIn}
                />
              </div>
            </Panel>

            <Panel title="Add liquidity" subtitle="Both ticks must be multiples of the pool's tick spacing.">
              <div className="form">
                <FormRow>
                  <Field label="Lower tick">
                    <NumberInput value={lowerTick} onChange={(e) => setLowerTick(e.target.value)} />
                  </Field>
                  <Field label="Upper tick">
                    <NumberInput value={upperTick} onChange={(e) => setUpperTick(e.target.value)} />
                  </Field>
                  <Field label="Liquidity (L)" hint={liquidityFeeNote}>
                    <NumberInput value={liquidity} onChange={(e) => setLiquidity(e.target.value)} />
                  </Field>
                </FormRow>
                <ActionButton
                  label="Add liquidity"
                  onClick={() =>
                    void send('CLMM add liquidity', [
                      clmmAddLiquidity({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        lowerTick: Number(lowerTick),
                        upperTick: Number(upperTick),
                        liquidityAmount: BigInt(liquidity || '0'),
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
                <hr className="divider" />
                <FormRow>
                  <Field label="Remove liquidity (L)">
                    <NumberInput value={removeShares} onChange={(e) => setRemoveShares(e.target.value)} />
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
                    void send('CLMM remove liquidity', [
                      clmmRemoveLiquidity({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        lowerTick: Number(lowerTick),
                        upperTick: Number(upperTick),
                        liquidityAmount: BigInt(removeShares || '0'),
                        minAmountA: BigInt(minA || '0'),
                        minAmountB: BigInt(minB || '0'),
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
                <ActionButton
                  label="Collect fees"
                  variant="secondary"
                  onClick={() =>
                    void send('CLMM collect fees', [
                      clmmCollectFees({
                        user: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        lowerTick: Number(lowerTick),
                        upperTick: Number(upperTick),
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
            title="Yield & fee analytics"
            subtitle="Fee-growth accumulators and accrued fees are read on-chain; the yield curve is a forward projection from the pool's live fee rate."
          >
            <div className="grid-4">
              <div className="viz-stat">
                <div className="viz-stat-label">Fee growth A (on-chain)</div>
                <div className="viz-stat-value">{pool.state.feeGrowthGlobalA.toString()}</div>
                <div className="viz-stat-hint">Q128.128 accumulator</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">Fee growth B (on-chain)</div>
                <div className="viz-stat-value">{pool.state.feeGrowthGlobalB.toString()}</div>
                <div className="viz-stat-hint">Q128.128 accumulator</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">My accrued fees</div>
                <div className="viz-stat-value">
                  {myPosition.state
                    ? `${myPosition.state.tokensOwedA} / ${myPosition.state.tokensOwedB}`
                    : '—'}
                </div>
                <div className="viz-stat-hint">token A / token B since last collect</div>
              </div>
              <div className="viz-stat">
                <div className="viz-stat-label">Pool fee rate</div>
                <div className="viz-stat-value">{(pool.state.feePips / 10_000).toFixed(2)}%</div>
                <div className="viz-stat-hint">{pool.state.feePips} pips per swap</div>
              </div>
            </div>

            <FormRow>
              <Field label="Assumed daily volume / TVL" hint="Drives the projection only; not an on-chain value.">
                <NumberInput
                  value={String(volumeMultiple)}
                  onChange={(e) => setVolumeMultiple(Math.max(0, Number(e.target.value) || 0))}
                />
              </Field>
              <Field label="Projected daily yield">
                <TextInput readOnly value={`${dailyYieldPct.toFixed(3)}%`} />
              </Field>
              <Field label="Projected APY">
                <TextInput readOnly value={`${apyPct.toFixed(1)}%`} />
              </Field>
            </FormRow>

            <Sparkline
              values={yieldCurve}
              caption="cumulative gross fee yield · 30-day model"
              format={(value) => `${value.toFixed(2)}%`}
            />
            <p className="viz-note">
              Accrued fees and the fee-growth accumulators are read on-chain. Yield and the curve are forward projections
              from the live fee rate and your volume assumption; realised returns depend on actual volume, tick
              concentration and whether the range stays active.
            </p>
          </Panel>

          <Panel
            title="Admin"
            subtitle="The pool's own halt: the oracle breaker stops price-driven pools, this stops any pool — including one created without the oracle rail. Withdrawals and fee collection stay open while paused."
          >
            <div className="form">
              <FormRow>
                <ActionButton
                  label={pool.state.paused ? 'Unpause pool' : 'Pause pool'}
                  variant="danger"
                  onClick={() =>
                    void send('CLMM pause', [
                      clmmSetPaused(pool.state!.address, publicKey!, !pool.state!.paused),
                    ])
                  }
                  disabled={!publicKey}
                />
                <Field label="New authority">
                  <TextInput
                    value={newAuthority}
                    onChange={(e) => setNewAuthority(e.target.value)}
                    placeholder="Base58 pubkey"
                  />
                </Field>
                <ActionButton
                  label="Rotate authority"
                  variant="secondary"
                  onClick={() =>
                    void send('CLMM set authority', [
                      clmmSetAuthority(pool.state!.address, publicKey!, new PublicKey(newAuthority)),
                    ])
                  }
                  disabled={!publicKey || !newAuthority}
                />
              </FormRow>
              {wiring.oracle ? (
                <FormRow>
                  <ActionButton
                    label="Trip circuit breaker (arm)"
                    variant="danger"
                    onClick={() => void send('CLMM trip breaker', [clmmTripBreaker({ pool: pool.state!.address })])}
                  />
                </FormRow>
              ) : null}
            </div>
          </Panel>
        </>
      ) : null}

      <Panel
        title="Create CLMM pool"
        subtitle="Fee in millionths (3_000 = 0.3%). Rails are fixed at birth: KYC config, oracle threshold and settlement routing."
      >
        <div className="form">
          <FormRow>
            <Field label="Initial price (B per A)">
              <NumberInput value={initPrice} onChange={(e) => setInitPrice(e.target.value)} />
            </Field>
            <Field label="Tick spacing">
              <NumberInput value={tickSpacing} onChange={(e) => setTickSpacing(e.target.value)} />
            </Field>
            <Field label="Fee pips">
              <NumberInput value={feePips} onChange={(e) => setFeePips(e.target.value)} />
            </Field>
          </FormRow>
          <FormRow>
            <Field label="Require KYC">
              <select className="text-input" value={useKyc ? 'on' : 'off'} onChange={(e) => setUseKyc(e.target.value === 'on')}>
                <option value="off">Off</option>
                <option value="on">On (kyc-config PDA)</option>
              </select>
            </Field>
            <Field label="Oracle deviation (bps, 0 = off)">
              <NumberInput value={deviationBps} onChange={(e) => setDeviationBps(e.target.value)} />
            </Field>
            <Field label="Settlement routing">
              <select
                className="text-input"
                value={useSettlement ? 'on' : 'off'}
                onChange={(e) => setUseSettlement(e.target.value === 'on')}
              >
                <option value="off">Off</option>
                <option value="on">On (hook authority)</option>
              </select>
            </Field>
            <Field label="T+N slots">
              <NumberInput value={settlementSlots} onChange={(e) => setSettlementSlots(e.target.value)} />
            </Field>
          </FormRow>
          <ActionButton
            label="Initialize pool"
            onClick={() =>
              void send('CLMM initialize pool', [
                clmmInitializePool({
                  authority: publicKey!,
                  mintA: new PublicKey(mintA),
                  mintB: new PublicKey(mintB),
                  initialSqrtPriceX64: sqrtPriceX64FromPrice(Number(initPrice)),
                  tickSpacing: Number(tickSpacing),
                  feePips: Number(feePips),
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

function OracleBadge({ pool }: { pool: PublicKey }) {
  const { connection } = useApp()
  const state = useAccountState(() => loadOracleState(connection, oracleState(pool)), [connection, pool.toBase58()])
  if (!state.state) return null
  return state.state.isPaused ? <Badge tone="bad">paused</Badge> : <Badge tone="good">oracle</Badge>
}

function useMemoPool(mintA: string, mintB: string): PublicKey | null {
  try {
    if (!mintA || !mintB) return null
    return clmmPool(new PublicKey(mintA), new PublicKey(mintB))
  } catch {
    return null
  }
}

function short(address: PublicKey): string {
  const text = address.toBase58()
  return `${text.slice(0, 6)}…${text.slice(-6)}`
}

/** A raw amount from an input box, or 0 while it is empty or not a number. */
function parseRaw(value: string): bigint {
  try {
    return BigInt(value || '0')
  } catch {
    return 0n
  }
}

/** `100` basis points as `1.00%`. */
function percent(basisPoints: number): string {
  return `${(basisPoints / 100).toFixed(2)}%`
}

/**
 * The most a swap can debit for an input of `amountIn` on a fee-bearing input
 * mint: the program sends the input the curve consumed plus the mint's fee, and
 * the consumed part never exceeds `amountIn`, so this is the wallet's worst case.
 */
function maxInputDebit(amountIn: bigint, fee: MintTransferFee): bigint {
  const uncapped = (amountIn * BigInt(fee.basisPoints) + 9_999n) / 10_000n
  return amountIn + (uncapped > fee.maximumFee ? fee.maximumFee : uncapped)
}

/** Which legs carry a Token-2022 transfer fee, for a one-line note — or undefined. */
function describeMintFees(a: MintTransferFee | null, b: MintTransferFee | null): string | undefined {
  const legs: string[] = []
  if (a) legs.push(`A ${percent(a.basisPoints)}`)
  if (b) legs.push(`B ${percent(b.basisPoints)}`)
  return legs.length === 0
    ? undefined
    : `Transfer fee on ${legs.join(' and ')} — the depositor pays it on top of both legs`
}
