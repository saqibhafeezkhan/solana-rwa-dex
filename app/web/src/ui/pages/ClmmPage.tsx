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
  TOKEN_PROGRAM_ID,
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
  clmmSwap,
  clmmTripBreaker,
} from '../../core/instructions-clmm'
import { hookAuthority } from '../../core/pdas'
import { SAMPLE_CLMM_POOL } from '../../core/sampleData'

export function ClmmPage() {
  const { connection, publicKey, send } = useApp()
  const [sampleMode] = useState(() => {
    if (typeof window === 'undefined') return false
    return localStorage.getItem('rwadex.sampleMode') === 'true'
  })

  // ---- pool selector -------------------------------------------------
  const [mintA, setMintA] = useState(() => sampleMode ? SAMPLE_CLMM_POOL.mintA.toBase58() : '')
  const [mintB, setMintB] = useState(() => sampleMode ? SAMPLE_CLMM_POOL.mintB.toBase58() : '')
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      if (sampleMode) return SAMPLE_CLMM_POOL
      return loadClmmPool(connection, poolAddress)
    },
    [connection, poolAddress?.toBase58(), sampleMode],
  )

  const wiring = pool.state
    ? {
        kycConfig: pool.state.kycConfig !== PublicKey.default,
        oracle: pool.state.oracleState !== PublicKey.default,
        settlement: pool.state.hooksAuthority !== PublicKey.default,
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
          return (await loadTokenAccount(connection, ata(publicKey, mint, TOKEN_PROGRAM_ID))).amount
        } catch {
          return 0n
        }
      }
      return {
        a: await read(pool.state.mintA),
        b: await read(pool.state.mintB),
      }
    },
    [connection, publicKey, pool.state?.address.toBase58()],
  )

  const parsedWalkTicks = walkTicks
    .split(/[,\s]+/)
    .map((token) => token.trim())
    .filter((token) => token.length > 0)
    .map((token) => Number(token))
    .filter((tick) => Number.isFinite(tick))

  return (
    <>
      <Panel
        title="Pool selector"
        subtitle="CLMM pools are addressed by their two mints. Load one to trade it or provide liquidity."
      >
        <FormRow>
          <Field label="Mint A (token0)">
            <TextInput value={mintA} onChange={(e) => setMintA(e.target.value)} placeholder="Base58 mint" />
          </Field>
          <Field label="Mint B (token1)">
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
          <div className="grid-2">
            <Panel
              title="Pool state"
              aside={
                <>
                  {pool.state.paused ? <Badge tone="bad">paused</Badge> : <Badge tone="good">live</Badge>}{' '}
                  {wiring.kycConfig ? <Badge tone="warn">KYC</Badge> : null}{' '}
                  {wiring.oracle ? <Badge tone="good">oracle</Badge> : null}{' '}
                  {wiring.settlement ? <Badge tone="warn">T+N</Badge> : null}{' '}
                  {wiring.oracle ? <OracleBadge pool={pool.state.address} /> : null}
                </>
              }
            >
              <div className="animate-fade-in-up">
                <KeyValue
                  rows={[
                    ['Price √P (Q64.64)', <span className="animate-shimmer">{pool.state.sqrtPriceX64.toString()}</span>],
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

          <div className="grid-2">
            <Panel title="Swap" subtitle="Ticks the trade may cross go in the walk box, in walk order (descending selling A→B, ascending selling B→A).">
              <div className="form">
                <FormRow>
                  <Field label="Amount in (raw)">
                    <NumberInput value={amountIn} onChange={(e) => setAmountIn(e.target.value)} />
                  </Field>
                  <Field label="Min amount out">
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
                  <Field label="Liquidity (L)">
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
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>
          </div>
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
                }),
              ])
            }
            disabled={!publicKey || !mintA || !mintB}
          />
          {wiring.oracle && pool.state ? (
            <ActionButton
              label="Trip circuit breaker (arm)"
              variant="danger"                  onClick={() =>
                    void send('CLMM trip breaker', [clmmTripBreaker({ pool: pool.state!.address })])
                  }
            />
          ) : null}
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
