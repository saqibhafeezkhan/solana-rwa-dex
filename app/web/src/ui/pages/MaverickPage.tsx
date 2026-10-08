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
  MAVERICK_BINS_PER_ARRAY,
  TOKEN_PROGRAM_ID,
  ata,
  hookAuthority,
  kycConfig,
  maverickBinArrayIndex,
  maverickPool,
  maverickPosition,
} from '../../core/pdas'
import { decodeBinPosition, loadMaverickPool, loadTokenAccount } from '../../core/decoders'
import {
  maverickAddLiquidity,
  maverickInitializePool,
  maverickMoveBins,
  maverickRemoveLiquidity,
  maverickSetPaused,
  maverickSwap,
  maverickTripBreaker,
  type MaverickBinKind,
} from '../../core/instructions-maverick'

export function MaverickPage() {
  const { connection, publicKey, send } = useApp()

  const [mintA, setMintA] = useState('')
  const [mintB, setMintB] = useState('')
  const poolAddress = useMemoPool(mintA, mintB)

  const pool = useAccountState(
    async () => {
      if (!poolAddress) return null
      return loadMaverickPool(connection, poolAddress)
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
  const [hopArrays, setHopArrays] = useState('')
  const [minA, setMinA] = useState('0')
  const [minB, setMinB] = useState('0')

  // move bins
  const [fromBinId, setFromBinId] = useState('-60')
  const [toBinId, setToBinId] = useState('-20')

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
          return (await loadTokenAccount(connection, ata(publicKey, mint, TOKEN_PROGRAM_ID))).amount
        } catch {
          return 0n
        }
      }
      return { a: await read(pool.state.mintA), b: await read(pool.state.mintB) }
    },
    [connection, publicKey, pool.state?.address.toBase58()],
  )

  const parsedHopArrays = hopArrays
    .split(/[,\s]+/)
    .filter((token) => token.length > 0)
    .map((token) => BigInt(token))

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
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>

            <Panel
              title="Remove liquidity"
              subtitle="List hop array indexes (comma-separated) when the position's bin has been merged along the move chain."
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
                <Field label="Hop array indexes" hint="Leave empty when the position was never moved.">
                  <TextInput value={hopArrays} onChange={(e) => setHopArrays(e.target.value)} placeholder="-1, 0" />
                </Field>
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
                        hopArrayIndexes: parsedHopArrays,
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
                      }),
                    ])
                  }
                  disabled={!publicKey}
                />
              </div>
            </Panel>
          </div>

          <Panel title="Admin">
            <FormRow>
              <ActionButton
                label={pool.state.paused ? 'Unpause pool' : 'Pause pool'}
                variant="danger"
                onClick={() => void send('Maverick pause', [maverickSetPaused(pool.state!.address, publicKey!, !pool.state!.paused)])}
                disabled={!publicKey}
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
