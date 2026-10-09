import { useId } from 'react'

/**
 * Small, dependency-free SVG visualisations shared by the LP dashboards.
 *
 * These render *model* shapes: the on-chain accounts here expose pool-level
 * accumulators and per-owner positions, not decoded per-bin reserves, so the
 * callers are responsible for labelling modelled values as such.
 */

function scale(value: number, min: number, max: number, outMin: number, outMax: number): number {
  if (max === min) return (outMin + outMax) / 2
  const t = (value - min) / (max - min)
  return outMin + t * (outMax - outMin)
}

export function Sparkline({
  values,
  height = 150,
  stroke = '#60a5fa',
  format,
  caption,
}: {
  values: number[]
  height?: number
  stroke?: string
  format?: (value: number) => string
  caption?: string
}) {
  const gradientId = `viz-grad-${useId().replace(/[^a-zA-Z0-9]/g, '')}`
  if (values.length < 2) return null

  const W = 600
  const H = height
  const min = Math.min(...values)
  const max = Math.max(...values)
  const stepX = W / (values.length - 1)
  const points = values.map((value, index) => {
    const x = index * stepX
    const y = scale(value, min, max, H - 10, 10)
    return `${x.toFixed(1)},${y.toFixed(1)}`
  })
  const line = `M${points.join(' L')}`
  const area = `${line} L${W},${H} L0,${H} Z`
  const fmt = format ?? ((value: number) => value.toLocaleString(undefined, { maximumFractionDigits: 2 }))

  return (
    <div className="viz-chart">
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="viz-svg" role="img">
        <defs>
          <linearGradient id={gradientId} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor={stroke} stopOpacity="0.35" />
            <stop offset="100%" stopColor={stroke} stopOpacity="0" />
          </linearGradient>
        </defs>
        <path d={area} fill={`url(#${gradientId})`} />
        <path d={line} fill="none" stroke={stroke} strokeWidth={2} vectorEffect="non-scaling-stroke" />
      </svg>
      <div className="viz-chart-foot">
        <span>{fmt(values[0])}</span>
        {caption ? <span className="viz-chart-caption">{caption}</span> : null}
        <span>{fmt(values[values.length - 1])}</span>
      </div>
    </div>
  )
}

export interface BinDatum {
  id: number
  value: number
  /** Highlight as the pool's current active bin. */
  active?: boolean
  /** 0–1 fraction of the bar drawn as the connected wallet's own liquidity. */
  overlay?: number
  title?: string
}

/**
 * Depth chart of discrete bins. Bar height and colour both encode relative
 * liquidity concentration; clicking a bar selects that bin.
 */
export function BinHistogram({
  bins,
  selectedId,
  onSelect,
  height = 180,
}: {
  bins: BinDatum[]
  selectedId?: number | null
  onSelect?: (id: number) => void
  height?: number
}) {
  if (bins.length === 0) return null
  const max = Math.max(...bins.map((bin) => bin.value), Number.EPSILON)

  return (
    <div className="viz-heatmap">
      <div className="viz-bars" style={{ height }}>
        {bins.map((bin) => {
          const ratio = Math.max(0, Math.min(1, bin.value / max))
          const barHeight = Math.max(4, ratio * (height - 26))
          const hue = 215 - ratio * 60
          const classes = ['viz-bar']
          if (bin.active) classes.push('is-active')
          if (bin.id === selectedId) classes.push('is-selected')
          return (
            <button
              key={bin.id}
              type="button"
              className={classes.join(' ')}
              style={{
                height: barHeight,
                background: `linear-gradient(180deg, hsl(${hue.toFixed(0)} 82% ${(32 + ratio * 28).toFixed(0)}%), hsla(${hue.toFixed(0)}, 70%, 30%, 0.35))`,
              }}
              onClick={onSelect ? () => onSelect(bin.id) : undefined}
              disabled={!onSelect}
              title={bin.title ?? `Bin ${bin.id}`}
            >
              {bin.overlay ? <span className="viz-bar-overlay" style={{ height: `${bin.overlay * 100}%` }} /> : null}
            </button>
          )
        })}
      </div>
    </div>
  )
}

export function Meter({
  value,
  max,
  tone = 'primary',
  label,
}: {
  value: number
  max: number
  tone?: 'primary' | 'warn' | 'danger'
  label?: string
}) {
  const pct = max > 0 ? Math.max(0, Math.min(1, value / max)) : 0
  return (
    <div>
      {label ? <div className="viz-meter-label">{label}</div> : null}
      <div className="viz-meter">
        <div className={`viz-meter-fill viz-meter-${tone}`} style={{ width: `${pct * 100}%` }} />
      </div>
    </div>
  )
}
