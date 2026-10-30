import type { PublicKey } from '@solana/web3.js'
import { useApp } from './useApp'
import { ActionButton, Badge, EmptyHint, KeyValue, Panel, StatusLine } from './components'
import { useAccountState } from './useAccountState'
import { Meter, Sparkline } from './charts'
import { ORACLE_PROGRAM_ID, oracleState } from '@rwa-dex/sdk'
import { loadOracleState } from '@rwa-dex/sdk'
import { sampleOracleState } from './sampleData'
import { oracleUnpausePool } from '@rwa-dex/sdk'
import { q64ToPrice } from './pricing'

function formatPrice(price: number | null): string {
  if (price === null || !Number.isFinite(price)) return '—'
  return price.toLocaleString(undefined, { maximumFractionDigits: 6 })
}

/**
 * Per-pool oracle read-out: TWAP versus the volatile spot price, the basis-point
 * deviation against the pool's threshold, and — when the circuit breaker has
 * tripped — the owner-only unpause.
 */
export function OracleRiskPanel({
  pool,
  spotPrice,
  spotLabel = 'Spot (pool)',
  priceKind = 'sqrt',
  rawScale = 1,
}: {
  pool: PublicKey
  spotPrice: number | null
  spotLabel?: string
  /**
   * What the pool records in `Observation.sqrt_price_x64`: CLMM writes a Q64.64
   * square root, while the bin engines write a Q64.64 price under the same
   * field name. The circuit breaker compares those stored values directly, so
   * the panel has to as well.
   */
  priceKind?: 'sqrt' | 'price'
  /** Whole-token A per B → raw A per B, i.e. `10^(decimalsA - decimalsB)`. */
  rawScale?: number
}) {
  const { connection, publicKey, send, sampleMode } = useApp()
  // Sample mode renders a pool that has no account on this cluster, so reading
  // the chain would show "no oracle" under a pool that advertises one. Use the
  // demo oracle instead, exactly as every other panel does.
  const state = useAccountState(
    async () => (sampleMode ? sampleOracleState(pool) : loadOracleState(connection, oracleState(pool))),
    [connection, pool.toBase58(), sampleMode],
  )

  const oracle = state.state
  const latestStored = oracle ? q64ToPrice(oracle.latestPrice) : null
  const toDisplay = (stored: number) =>
    priceKind === 'sqrt' ? stored * stored : stored / rawScale
  const twap = latestStored === null ? null : toDisplay(latestStored)
  const history =
    oracle?.observations
      .map((observation) => toDisplay(q64ToPrice(observation.sqrtPriceX64)))
      .filter((price) => Number.isFinite(price) && price > 0) ?? []
  // `check_deviation` compares the two stored values, not their squares, so
  // the percentage here has to use the same units to match the breaker.
  const spotStored =
    spotPrice === null
      ? null
      : priceKind === 'sqrt'
        ? Math.sqrt(spotPrice)
        : spotPrice * rawScale
  const deviationBps =
    spotStored !== null && latestStored !== null && latestStored > 0
      ? (Math.abs(spotStored - latestStored) / latestStored) * 10_000
      : null
  const threshold = oracle?.maxDeviationBps ?? 0
  const tone =
    deviationBps === null || threshold === 0
      ? 'primary'
      : deviationBps >= threshold
        ? 'danger'
        : deviationBps >= threshold * 0.7
          ? 'warn'
          : 'primary'

  return (
    <Panel
      title="Oracle & risk"
      subtitle="The TWAP oracle gates swaps: a spot/TWAP deviation past the pool's threshold trips the circuit breaker and pauses the pool. Below is the TWAP the oracle last wrote, the pool's own spot price, the gap between them and the threshold being watched — all read-only; when the pool has paused, the owner can unpause it from here."
      aside={
        oracle ? (
          <Badge tone={oracle.isPaused ? 'bad' : 'good'}>{oracle.isPaused ? 'paused' : 'live'}</Badge>
        ) : (
          <Badge tone="warn">no oracle</Badge>
        )
      }
    >
      <StatusLine error={state.error} />
      {oracle ? (
        <>
          <div className="grid-4">
            <div className="viz-stat">
              <div className="viz-stat-label">TWAP price</div>
              <div className="viz-stat-value">{formatPrice(twap)}</div>
              <div className="viz-stat-hint">latest observation</div>
            </div>
            <div className="viz-stat">
              <div className="viz-stat-label">{spotLabel}</div>
              <div className="viz-stat-value">{formatPrice(spotPrice)}</div>
              <div className="viz-stat-hint">volatile AMM price</div>
            </div>
            <div className="viz-stat">
              <div className="viz-stat-label">Deviation</div>
              <div
                className={`viz-stat-value ${
                  deviationBps !== null && threshold > 0 && deviationBps >= threshold ? 'negative' : ''
                }`}
              >
                {deviationBps === null ? '—' : `${deviationBps.toFixed(0)} bps`}
              </div>
              <div className="viz-stat-hint">{threshold > 0 ? `threshold ${threshold} bps` : 'rail off'}</div>
            </div>
            <div className="viz-stat">
              <div className="viz-stat-label">Observations</div>
              <div className="viz-stat-value">{oracle.observationCount}</div>
              <div className="viz-stat-hint">
                {oracle.latestTimestamp > 0n
                  ? new Date(Number(oracle.latestTimestamp) * 1000).toLocaleTimeString()
                  : 'no writes yet'}
              </div>
            </div>
          </div>

          {threshold > 0 && deviationBps !== null ? (
            <div style={{ marginTop: 16 }}>
              <Meter
                value={deviationBps}
                max={threshold}
                tone={tone}
                label={`deviation vs threshold (${threshold} bps)`}
              />
            </div>
          ) : null}

          {history.length >= 2 ? (
            <div style={{ marginTop: 16 }}>
              <Sparkline
                values={history}
                height={120}
                caption={`TWAP history — ${history.length} on-chain observation(s), oldest → newest`}
                format={(value) => value.toLocaleString(undefined, { maximumFractionDigits: 6 })}
              />
            </div>
          ) : null}

          <KeyValue
            rows={[
              ['Oracle account', oracle.address.toBase58()],
              ['Oracle program', ORACLE_PROGRAM_ID.toBase58()],
              ['Max deviation', `${oracle.maxDeviationBps} bps`],
            ]}
          />

          {oracle.isPaused ? (
            <ActionButton
              label="Unpause pool (oracle owner)"
              variant="secondary"
              onClick={() => void send('Oracle unpause', [oracleUnpausePool(pool, publicKey!)])}
              disabled={!publicKey}
            />
          ) : null}
        </>
      ) : (
        <EmptyHint>
          {state.error
            ? 'No oracle account for this pool — the oracle rail is likely off at creation.'
            : 'Loading oracle…'}
        </EmptyHint>
      )}
    </Panel>
  )
}

/**
 * Banner shown when a pool's admin pause is active. It deliberately has no
 * unpause action: an admin pause is cleared from the page's Admin panel, while
 * an oracle trip is cleared from the Oracle panel (they are separate switches,
 * and calling the oracle unpause on an admin-paused pool just fails).
 */
export function PausedBanner() {
  return (
    <div className="breaker-banner" role="alert">
      <span className="breaker-banner-text">
        ⚠ Pool paused: swaps, deposits and bin moves are refused; withdrawals stay open. Unpause from the Admin panel.
        If the oracle breaker also tripped, its owner must unpause that separately once spot is back inside the threshold.
      </span>
    </div>
  )
}
