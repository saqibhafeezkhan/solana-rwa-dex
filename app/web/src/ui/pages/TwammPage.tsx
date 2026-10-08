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
  hookAuthority,
  kycConfig,
  twammLpPosition,
  twammOrder,
  twammPool,
} from '../../core/pdas'
import {
  decodeTwammOrder,
  loadTwammPool,
} from '../../core/decoders'
import {
  twammAddLiquidity,
  twammCancelOrder,
  twammCreateLongTermOrder,
  twammExecuteVirtualOrders,
  twammInitializePool,
  twammRemoveLiquidity,
  twammSetPaused,
  twammWithdrawProceeds,
} from '../../core/instructions-twamm'

export function TwammPage() {
  const { connection, publicKey, send } = useApp()

  const [mintA, setMintA] = useState('')
  const [mintB, setMintB] = useState('')
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      return loadTwammPool(connection, poolAddress)
    },
    [connection, poolAddress?.toBase58()],
  )

  const wiring = pool.state
    ? {
        kycConfig: pool.state.kycConfig !== PublicKey.default,
        oracle: pool.state.oracleState !== PublicKey.default,
        settlement: pool.state.hooksAuthority !== PublicKey.default,
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
  const [walkOrders, setWalkOrders] = useState('')

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

  const parsedWalk = walkOrders
    .split(/[,\s]+/)
    .filter((token) => token.length > 0)
    .map((token) => new PublicKey(token))

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
                <Field label="Insertion walk (order pubkeys)" hint="Optional: live orders from order_head, excluding the first that expires after this one.">
                  <TextInput value={walkOrders} onChange={(e) => setWalkOrders(e.target.value)} placeholder="pubkey, pubkey" />
                </Field>
                <ActionButton
                  label="Create order"
                  onClick={() =>
                    void send('TWAMM create order', [
                      twammCreateLongTermOrder({
                        owner: publicKey!,
                        mintA: pool.state!.mintA,
                        mintB: pool.state!.mintB,
                        orderId: BigInt(orderId || '0'),
                        amountIn: BigInt(orderAmount || '0'),
                        durationSeconds: Number(duration),
                        isAToB,
                        walkOrders: parsedWalk,
                        wiring: { kycConfig: wiring.kycConfig, oracle: wiring.oracle },
                      }),
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
                        twammExecuteVirtualOrders({
                          mintA: pool.state!.mintA,
                          mintB: pool.state!.mintB,
                          expiredOrders: parsedWalk,
                          wiring: { oracle: wiring.oracle },
                        }),
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
                        twammCancelOrder({
                          owner: publicKey!,
                          mintA: pool.state!.mintA,
                          mintB: pool.state!.mintB,
                          orderId: BigInt(orderId || '0'),
                          isAToB,
                          prefixOrders: parsedWalk,
                          wiring: { oracle: wiring.oracle, settlement: wiring.settlement },
                        }),
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
