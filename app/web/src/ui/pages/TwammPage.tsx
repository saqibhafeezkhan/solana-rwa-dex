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
  hookAuthority,
  kycConfig,
  settlementEscrowAuthority,
  twammLpPosition,
  twammOrder,
  twammPool,
} from '../../core/pdas'
import {
  decodeTwammOrder,
  loadTwammPool,
} from '../../core/decoders'
import { Meter, Sparkline } from '../charts'
import { formatDuration, useNow } from '../useNow'
import {
  twammAddLiquidity,
  twammCancelOrder,
  twammCreateLongTermOrder,
  twammExecuteVirtualOrders,
  twammInitializePool,
  twammRemoveLiquidity,
  twammSetAuthority,
  twammSetPaused,
  twammWithdrawProceeds,
} from '../../core/instructions-twamm'
import {
  deriveTwammExpiredWalk,
  deriveTwammInsertionWalk,
  deriveTwammPrefixWalk,
} from '../../core/walks'
import { SAMPLE_TWAMM_POOL } from '../../core/sampleData'
import { useSampleMints } from '../useSampleMints'
import { useTokenProgram } from '../useTokenProgram'
import { hookAccountsForTransfers } from '../../core/connection'

export function TwammPage() {
  const { connection, publicKey, send, sampleMode } = useApp()

  const { mintA, setMintA, mintB, setMintB } = useSampleMints(SAMPLE_TWAMM_POOL, sampleMode)
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      if (sampleMode) return SAMPLE_TWAMM_POOL
      return loadTwammPool(connection, poolAddress)
    },
    [connection, poolAddress?.toBase58(), sampleMode],
  )

  // ATAs and the token-program account follow the pool mint's program.
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
  const [initialA, setInitialA] = useState('1000000000')
  const [initialB, setInitialB] = useState('1000000000')
  const [useKyc, setUseKyc] = useState(false)
  const [deviationBps, setDeviationBps] = useState('0')
  const [useSettlement, setUseSettlement] = useState(false)
  const [settlementSlots, setSettlementSlots] = useState('100')

  // LP
  const [addA, setAddA] = useState('1000000')
  const [addB, setAddB] = useState('1000000')
  const [shares, setShares] = useState('1000')
  const [minA, setMinA] = useState('0')
  const [minB, setMinB] = useState('0')

  // orders
  const [orderId, setOrderId] = useState('1')
  const [orderAmount, setOrderAmount] = useState('500000')
  const [duration, setDuration] = useState('3600')
  const [isAToB, setIsAToB] = useState(true)

  // admin
  const [newAuthority, setNewAuthority] = useState('')

  const lp = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const address = twammLpPosition(pool.state.address, publicKey)
      try {
        const info = await connection.getAccountInfo(address)
        if (!info) return null
        // LpPosition: pool (32) + owner (32) + shares (u128) + bump.
        return { address, shares: readU128(info.data, 8 + 32 + 32) }
      } catch {
        return null
      }
    },
    [connection, publicKey, pool.state?.address.toBase58()],
  )

  const myOrder = useAccountState(
    async () => {
      if (!publicKey || !pool.state) return null
      const address = twammOrder(pool.state.address, publicKey, BigInt(orderId || '1'))
      try {
        const info = await connection.getAccountInfo(address)
        if (!info) return null
        return decodeTwammOrder({ pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports })
      } catch {
        return null
      }
    },
    [connection, publicKey, pool.state?.address.toBase58(), orderId],
  )

  // Execution analytics for the tracked order. Progress, proceeds and the
  // achieved average are on-chain; the fill curve is a linear projection from
  // the order's sell rate (the crank may lag behind it).
  const now = useNow(1000)
  const order = myOrder.state
  const progress = order && order.amountIn > 0n ? Number(order.sold) / Number(order.amountIn) : 0
  const remaining = order ? order.amountIn - order.sold : 0n
  const achievedPrice = order && order.sold > 0n ? Number(order.proceedsOut) / Number(order.sold) : null
  // `achievedPrice` is output per input, so the spot reference has to be in
  // the same direction: B per A for an A→B order, A per B for B→A.
  const spotPrice = pool.state && order
    ? order.isAToB
      ? pool.state.reserveA > 0n
        ? Number(pool.state.reserveB) / Number(pool.state.reserveA)
        : null
      : pool.state.reserveB > 0n
        ? Number(pool.state.reserveA) / Number(pool.state.reserveB)
        : null
    : null
  const slippageBps =
    achievedPrice !== null && spotPrice !== null
      ? ((achievedPrice - spotPrice) / spotPrice) * 10_000
      : null
  const remainingMs = order ? Math.max(0, Number(order.expiryTimestamp) * 1000 - now) : 0
  const fillCurve = order
    ? Array.from({ length: 31 }, (_, index) => {
        const start = Number(order.createdTimestamp)
        const end = Number(order.expiryTimestamp)
        if (end <= start) return Number(order.sold)
        const at = start + ((end - start) * index) / 30
        return Math.min(Number(order.sellRate) * (at - start), Number(order.amountIn))
      })
    : []

  return (
    <>
      <Panel title="Pool selector" subtitle="TWAMM pools are keyed by their two mints; the genesis deposit defines the price.">
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
                  ['Reserve A', pool.state.reserveA.toString()],
                  ['Reserve B', pool.state.reserveB.toString()],
                  ['Sell rate A / s', pool.state.sellRateA.toString()],
                  ['Sell rate B / s', pool.state.sellRateB.toString()],
                  ['LP supply', pool.state.lpSupply.toString()],
                  ['Last execution', new Date(Number(pool.state.lastExecutionTimestamp) * 1000).toISOString()],
                  ['Order head expiry', pool.state.orderHeadExpiry.toString()],
                ]}
              />
            </Panel>
            <Panel title="My LP position & order" subtitle={`Order id ${orderId} for the connected wallet.`}>
              {lp.state ? (
                <KeyValue rows={[['LP shares', lp.state.shares.toString()]]} />
              ) : (
                <EmptyHint>No LP position.</EmptyHint>
              )}
              {myOrder.state ? (
                <KeyValue
                  rows={[
                    ['Direction', myOrder.state.isAToB ? 'A → B' : 'B → A'],
                    ['Escrowed', myOrder.state.amountIn.toString()],
                    ['Sold', myOrder.state.sold.toString()],
                    ['Rate / s', myOrder.state.sellRate.toString()],
                    ['Proceeds', myOrder.state.proceedsOut.toString()],
                    ['Status', myOrder.state.settled ? 'settled' : 'live'],
                    ['Expires', new Date(Number(myOrder.state.expiryTimestamp) * 1000).toISOString()],
                  ]}
                />
              ) : (
                <EmptyHint>No order at this id.</EmptyHint>
              )}
            </Panel>
          </div>

          {wiring.settlement ? (
            <Panel
              title="Settlement escrows (pool-wired)"
              subtitle="When settlement is wired, order proceeds are routed to T+N escrows. These are visible on the Settlement page, but you can track them here too."
            >
              <p className="viz-note">
                Settlement-wired TWAMM orders route their proceeds through the hooks program into T+N escrows.
                Open the Settlement page to see every escrow, and to settle or cancel one once its time lock has passed.
              </p>
            </Panel>
          ) : null}

          {order ? (
            <Panel
              title="Execution analytics"
              subtitle="Time-weighted execution for the tracked order: what has filled, the average price achieved against spot, and time to expiry."
              aside={<Badge tone={order.settled ? 'neutral' : remainingMs > 0 ? 'good' : 'warn'}>{order.settled ? 'settled' : remainingMs > 0 ? 'executing' : 'expired'}</Badge>}
            >
              <Meter
                value={Number(order.sold)}
                max={Number(order.amountIn) || 1}
                label={`${(progress * 100).toFixed(1)}% filled · ${order.sold.toString()} / ${order.amountIn.toString()}`}
              />

              <div className="grid-4" style={{ marginTop: 18 }}>
                <div className="viz-stat">
                  <div className="viz-stat-label">Escrowed</div>
                  <div className="viz-stat-value">{order.amountIn.toString()}</div>
                  <div className="viz-stat-hint">total order size</div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Remaining</div>
                  <div className="viz-stat-value">{remaining.toString()}</div>
                  <div className="viz-stat-hint">not yet sold</div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Proceeds</div>
                  <div className="viz-stat-value">{order.proceedsOut.toString()}</div>
                  <div className="viz-stat-hint">received so far</div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Time left</div>
                  <div className="viz-stat-value">{order.settled ? '—' : formatDuration(remainingMs)}</div>
                  <div className="viz-stat-hint">to expiry {new Date(Number(order.expiryTimestamp) * 1000).toLocaleString()}</div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Avg price achieved</div>
                  <div className="viz-stat-value">{achievedPrice === null ? '—' : achievedPrice.toFixed(6)}</div>
                  <div className="viz-stat-hint">proceeds / sold</div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Spot price</div>
                  <div className="viz-stat-value">{spotPrice === null ? '—' : spotPrice.toFixed(6)}</div>
                  <div className="viz-stat-hint">
                    {order.isAToB ? 'reserve B / reserve A' : 'reserve A / reserve B'}
                  </div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Slippage vs spot</div>
                  <div className={`viz-stat-value ${slippageBps !== null && slippageBps < 0 ? 'negative' : slippageBps !== null ? 'positive' : ''}`}>
                    {slippageBps === null ? '—' : `${slippageBps >= 0 ? '+' : ''}${slippageBps.toFixed(1)} bps`}
                  </div>
                  <div className="viz-stat-hint">negative = executed below spot</div>
                </div>
                <div className="viz-stat">
                  <div className="viz-stat-label">Sell rate</div>
                  <div className="viz-stat-value">{order.sellRate.toString()}</div>
                  <div className="viz-stat-hint">per second</div>
                </div>
              </div>

              <Sparkline
                values={fillCurve}
                caption="projected cumulative fill · linear from sell rate"
                format={(value) => value.toLocaleString(undefined, { maximumFractionDigits: 0 })}
              />
              <p className="viz-note">
                Progress, proceeds and the achieved average price are read on-chain. The curve projects a linear fill from the
                order's sell rate — actual execution depends on the crank and available counterparty flow.
              </p>
            </Panel>
          ) : null}

          <div className="grid-2">
            <Panel title="Long-term order" subtitle="Sells at a fixed rate per second until expiry. The walk box lists live orders from the head up to the insertion point.">
              <div className="form">
                <FormRow>
                  <Field label="Order id">
                    <NumberInput value={orderId} onChange={(e) => setOrderId(e.target.value)} />
                  </Field>
                  <Field label="Amount in">
                    <NumberInput value={orderAmount} onChange={(e) => setOrderAmount(e.target.value)} />
                  </Field>
                  <Field label="Duration (s)">
                    <NumberInput value={duration} onChange={(e) => setDuration(e.target.value)} />
                  </Field>
                  <Field label="Direction">
                    <select className="text-input" value={isAToB ? 'a2b' : 'b2a'} onChange={(e) => setIsAToB(e.target.value === 'a2b')}>
                      <option value="a2b">Sell A</option>
                      <option value="b2a">Sell B</option>
                    </select>
                  </Field>
                </FormRow>
                <p className="viz-note">
                  The order walk is read from the pool&rsquo;s live list when you send: the insertion point for a new
                  order, the expired head for a crank, and the prefix in front of the order for a cancel.
                </p>
                <ActionButton
                  label="Create order"
                  onClick={() =>
                    void send('TWAMM create order', [
                      (async () => {
                        const expiry =
                          BigInt(Math.floor(Date.now() / 1000)) + BigInt(Math.max(0, Number(duration) || 0))
                        const walk = await deriveTwammInsertionWalk(connection, pool.state!.orderHead, expiry)
                        return twammCreateLongTermOrder({
                          owner: publicKey!,
                          mintA: pool.state!.mintA,
                          mintB: pool.state!.mintB,
                          orderId: BigInt(orderId || '0'),
                          amountIn: BigInt(orderAmount || '0'),
                          durationSeconds: Number(duration),
                          isAToB,
                          walkOrders: walk,
                          wiring: { kycConfig: wiring.kycConfig, oracle: wiring.oracle },
                          tokenProgramId,
                          hookAccounts: hookAccounts.state ?? undefined,
                        })
                      })(),
                    ])
                  }
                  disabled={!publicKey}
                />
                <hr className="divider" />
                <FormRow>
                  <ActionButton
                    label="Execute virtual orders (crank)"
                    variant="secondary"
                    onClick={() =>
                      void send('TWAMM crank', [
                        (async () => {
                          const walk = await deriveTwammExpiredWalk(
                            connection,
                            pool.state!.orderHead,
                            BigInt(Math.floor(Date.now() / 1000)),
                          )
                          return twammExecuteVirtualOrders({
                            mintA: pool.state!.mintA,
                            mintB: pool.state!.mintB,
                            expiredOrders: walk,
                            wiring: { oracle: wiring.oracle },
                          })
                        })(),
                      ])
                    }
                  />
                  <ActionButton
                    label="Withdraw proceeds"
                    variant="secondary"
                    onClick={() =>
                      void send('TWAMM withdraw', [
                        twammWithdrawProceeds({
                          owner: publicKey!,
                          mintA: pool.state!.mintA,
                          mintB: pool.state!.mintB,
                          orderId: BigInt(orderId || '0'),
                          isAToB,
                          wiring: { settlement: wiring.settlement },
                          tokenProgramId,
                          hookAccounts: hookAccounts.state ?? undefined,
                        }),
                      ])
                    }
                    disabled={!publicKey}
                  />
                  <ActionButton
                    label="Cancel order"
                    variant="danger"
                    onClick={() =>
                      void send('TWAMM cancel order', [
                        (async () => {
                          const target = twammOrder(pool.state!.address, publicKey!, BigInt(orderId || '0'))
                          const walk = await deriveTwammPrefixWalk(connection, pool.state!.orderHead, target)
                          return twammCancelOrder({
                            owner: publicKey!,
                            mintA: pool.state!.mintA,
                            mintB: pool.state!.mintB,
                            orderId: BigInt(orderId || '0'),
                            isAToB,
                            prefixOrders: walk,
                            wiring: { oracle: wiring.oracle, settlement: wiring.settlement },
                            tokenProgramId,
                            hookAccounts: hookAccounts.state ?? undefined,
                          })
                        })(),
                      ])
                    }
                    disabled={!publicKey}
                  />
                </FormRow>
              </div>
            </Panel>

            <Panel title="Liquidity" subtitle="Deposits both legs; withdrawals are not gated on pause.">
              <div className="form">
                <FormRow>
                  <Field label="Amount A">
                    <NumberInput value={addA} onChange={(e) => setAddA(e.target.value)} />
                  </Field>
                  <Field label="Amount B">
                    <NumberInput value={addB} onChange={(e) => setAddB(e.target.value)} />
                  </Field>
                </FormRow>
                <ActionButton
                  label="Add liquidity"
                  onClick={() =>
                    void send('TWAMM add liquidity', [
                      twammAddLiquidity({
                        owner: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        amountA: BigInt(addA || '0'),
                        amountB: BigInt(addB || '0'),
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
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
                    void send('TWAMM remove liquidity', [
                      twammRemoveLiquidity({
                        owner: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        shares: BigInt(shares || '0'),
                        minAmountA: BigInt(minA || '0'),
                        minAmountB: BigInt(minB || '0'),
                        tokenProgramId,
                        hookAccounts: hookAccounts.state ?? undefined,
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
                <FormRow>
                  <ActionButton
                    label={pool.state.paused ? 'Unpause pool' : 'Pause pool'}
                    variant="danger"
                    onClick={() => void send('TWAMM pause', [twammSetPaused(pool.state!.address, publicKey!, !pool.state!.paused)])}
                    disabled={!publicKey}
                  />
                  <Field label="New authority">
                    <TextInput value={newAuthority} onChange={(e) => setNewAuthority(e.target.value)} placeholder="Base58 pubkey" />
                  </Field>
                  <ActionButton
                    label="Rotate authority"
                    variant="secondary"
                    onClick={() => void send('TWAMM set authority', [twammSetAuthority(pool.state!.address, publicKey!, new PublicKey(newAuthority))])}
                    disabled={!publicKey || !newAuthority}
                  />
                </FormRow>
              </div>
            </Panel>
          </div>
        </>
      ) : null}

      <Panel title="Create TWAMM pool" subtitle="The initial deposit is the genesis price P = initial_B / initial_A; the payer becomes the first LP.">
        <div className="form">
          <FormRow>
            <Field label="Initial amount A">
              <NumberInput value={initialA} onChange={(e) => setInitialA(e.target.value)} />
            </Field>
            <Field label="Initial amount B">
              <NumberInput value={initialB} onChange={(e) => setInitialB(e.target.value)} />
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
              void send('TWAMM initialize pool', [
                twammInitializePool({
                  payer: publicKey!,
                  mintA: new PublicKey(mintA),
                  mintB: new PublicKey(mintB),
                  initialAmountA: BigInt(initialA || '0'),
                  initialAmountB: BigInt(initialB || '0'),
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

function readU128(data: Uint8Array, offset: number): bigint {
  let value = 0n
  for (let i = offset + 15; i >= offset; i--) {
    value = (value << 8n) | BigInt(data[i] ?? 0)
  }
  return value
}

function useMemoPool(mintA: string, mintB: string): PublicKey | null {
  try {
    if (!mintA || !mintB) return null
    return twammPool(new PublicKey(mintA), new PublicKey(mintB))
  } catch {
    return null
  }
}
