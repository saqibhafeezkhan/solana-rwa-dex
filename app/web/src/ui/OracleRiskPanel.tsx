import type { PublicKey } from '@solana/web3.js'
import { useApp } from './AppContext'
import { ActionButton, Badge, EmptyHint, KeyValue, Panel, StatusLine } from './components'
import { useAccountState } from './useAccountState'
import { Meter, Sparkline } from './charts'
import { ORACLE_PROGRAM_ID, oracleState } from '../core/pdas'
import { loadOracleState } from '../core/decoders'
import { oracleUnpausePool } from '../core/instructions-rails'
import { sqrtPriceToPrice } from './pricing'

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
}: {
  pool: PublicKey
  spotPrice: number | null
  spotLabel?: string
}) {
  const { connection, publicKey, send } = useApp()
  const state = useAccountState(
    () => loadOracleState(connection, oracleState(pool)),
    [connection, pool.toBase58()],
  )

  const oracle = state.state
  const twap = oracle ? sqrtPriceToPrice(oracle.latestPrice) : null
  const history =
    oracle?.observations
      .map((observation) => sqrtPriceToPrice(observation.sqrtPriceX64))
      .filter((price) => Number.isFinite(price) && price > 0) ?? []
  const deviationBps = twap && spotPrice && twap > 0 ? (Math.abs(spotPrice - twap) / twap) * 10_000 : null
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

/** Global banner shown when a pool is paused, with the owner-only unpause. */
export function PausedBanner({ pool }: { pool: PublicKey }) {
  const { publicKey, send } = useApp()
  return (
    <div className="breaker-banner" role="alert">
      <span className="breaker-banner-text">
        ⚠ Pool paused: price deviation exceeded the oracle threshold. Swaps, deposits and bin moves are refused while the
        pool is paused; withdrawals stay open. Only the oracle/pool owner can unpause, and only once the spot price is back
        inside the threshold.
      </span>
      <button
        className="btn btn-secondary breaker-banner-action"
        onClick={() => void send('Oracle unpause', [oracleUnpausePool(pool, publicKey!)])}
        disabled={!publicKey}
      >
        Unpause pool
      </button>
    </div>
  )
}
