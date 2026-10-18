import { Children, useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { useApp } from './useApp'
import { railEnds, railStep, railTarget } from './carousel'
import { useAccountState } from './useAccountState'
import type { ClusterName } from '../core/connection'
import { TOKEN_2022_PROGRAM_ID, ata, kycConfig } from '../core/pdas'
import { loadKycConfig, loadTokenAccount } from '../core/decoders'
import { ROLE_LABELS, ROLES, type Role } from './enterprise'

/** Must match the gap between tiles in `.price-tile-track`. */
const TILE_GAP = 20

/**
 * Horizontal rail of price tiles with previous/next arrows. The tread scrolls
 * one tile at a time and the arrows dim once the rail reaches that end.
 */
export function PriceTileRow({ children, label }: { children: ReactNode; label?: string }) {
  const trackRef = useRef<HTMLDivElement | null>(null)
  const tileCount = Children.count(children)
  const [atStart, setAtStart] = useState(true)
  // Assume there is more to the right until the first measurement; a rail whose
  // tiles all fit dims both arrows on the frame after mount.
  const [atEnd, setAtEnd] = useState(false)

  const sync = () => {
    const track = trackRef.current
    if (!track) return
    const { atStart: start, atEnd: end } = railEnds(track)
    setAtStart(start)
    setAtEnd(end)
  }

  useEffect(() => {
    const track = trackRef.current
    if (!track) return
    sync()
    track.addEventListener('scroll', sync, { passive: true })
    window.addEventListener('resize', sync)
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(sync)
    observer?.observe(track)
    return () => {
      track.removeEventListener('scroll', sync)
      window.removeEventListener('resize', sync)
      observer?.disconnect()
    }
    // Re-measure whenever the tile set changes (sample data on/off).
  }, [tileCount])

  const step = (direction: -1 | 1) => {
    const track = trackRef.current
    if (!track) return
    const first = track.firstElementChild as HTMLElement | null
    const tile = first ? first.getBoundingClientRect().width : track.clientWidth * 0.9
    track.scrollTo({
      left: railTarget(track, direction, railStep(tile, TILE_GAP)),
      behavior: 'smooth',
    })
  }

  return (
    <section className="price-tile-row" aria-label={label ?? 'Price tiles'}>
      <button
        type="button"
        className="tile-arrow tile-arrow-prev"
        onClick={() => step(-1)}
        disabled={atStart}
        aria-label="Previous price tile"
        title="Previous price tile"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M15 5l-7 7 7 7" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      <div className="price-tile-track" ref={trackRef}>
        {children}
      </div>
      <button
        type="button"
        className="tile-arrow tile-arrow-next"
        onClick={() => step(1)}
        disabled={atEnd}
        aria-label="Next price tile"
        title="Next price tile"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M9 5l7 7-7 7" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
    </section>
  )
}

/**
 * Persistent compliance indicator. Reads the connected wallet's soulbound KYC
 * attestation balance and reports whether the institution is verified.
 */
function KycStatusBadge() {
  const { connection, publicKey } = useApp()
  const config = useAccountState(() => loadKycConfig(connection, kycConfig()), [connection])
  const balance = useAccountState(
    async () => {
      const mint = config.state?.kycMint
      if (!publicKey || !mint) return null
      try {
        return (await loadTokenAccount(connection, ata(publicKey, mint, TOKEN_2022_PROGRAM_ID))).amount
      } catch {
        return 0n
      }
    },
    [connection, publicKey?.toBase58(), config.state?.kycMint.toBase58()],
  )

  if (!publicKey || !config.state || balance.state === null) return null
  return balance.state > 0n ? (
    <span className="badge badge-good kyc-badge">Accredited: Verified</span>
  ) : (
    <span className="badge badge-warn kyc-badge">KYC: none</span>
  )
}

export function Panel({
  title,
  subtitle,
  children,
  aside,
}: {
  title: string
  subtitle?: string
  children: ReactNode
  aside?: ReactNode
}) {
  return (
    <section className="panel">
      <div className="panel-header">
        <div>
          <h3>{title}</h3>
          {subtitle ? <p className="panel-subtitle">{subtitle}</p> : null}
        </div>
        {aside}
      </div>
      {children}
    </section>
  )
}

export function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: ReactNode
}) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      {children}
      {hint ? <span className="field-hint">{hint}</span> : null}
    </label>
  )
}

export function TextInput(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input className="text-input" {...props} />
}

export function NumberInput(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input className="text-input" inputMode="decimal" {...props} />
}

export function Select({ children, ...props }: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select className="text-input" {...props}>
      {children}
    </select>
  )
}

export function ActionButton({
  label,
  onClick,
  disabled,
  variant = 'primary',
}: {
  label: string
  onClick: () => void
  disabled?: boolean
  variant?: 'primary' | 'secondary' | 'danger'
}) {
  return (
    <button className={`btn btn-${variant}`} onClick={onClick} disabled={disabled}>
      {label}
    </button>
  )
}

export function FormRow({ children }: { children: ReactNode }) {
  return <div className="form-row">{children}</div>
}

export function KeyValue({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <div className="kv">
      {rows.map(([key, value]) => (
        <div key={key} className="kv-row">
          <span>{key}</span>
          <strong>{value}</strong>
        </div>
      ))}
    </div>
  )
}

export function Badge({
  children,
  tone = 'neutral',
  className = '',
}: {
  children: ReactNode
  tone?: 'neutral' | 'good' | 'warn' | 'bad'
  className?: string
}) {
  return <span className={`badge badge-${tone} ${className}`.trim()}>{children}</span>
}

export function StatusLine({ error }: { error: string | null }) {
  if (!error) return null
  return <p className="status-error">{error}</p>
}

export function Toasts() {
  const { toasts, dismiss } = useApp()
  if (toasts.length === 0) return null
  return (
    <div className="toast-stack">
      {toasts.map((toast) => (
        <button key={toast.id} className={`toast toast-${toast.kind}`} onClick={() => dismiss(toast.id)}>
          <span>{toast.text}</span>
          {toast.signature ? (
            <code className="toast-sig">{toast.signature.slice(0, 18)}…</code>
          ) : null}
        </button>
      ))}
    </div>
  )
}

const CLUSTERS: ClusterName[] = ['localnet', 'devnet', 'testnet', 'mainnet-beta']

export function TopBar({ page, setPage }: { page: string; setPage: (page: string) => void }) {
  const {
    cluster,
    setCluster,
    publicKey,
    connecting,
    connect,
    disconnect,
    role,
    setRole,
    executionMode,
    setExecutionMode,
    sampleMode,
    setSampleMode,
  } = useApp()
  const [menuOpen, setMenuOpen] = useState(false)

  const pages: [string, string][] = [
    ['overview', 'Dashboard'],
    ['portfolio', 'Portfolio'],
    ['clmm', 'CLMM'],
    ['dlmm', 'DLMM'],
    ['maverick', 'Maverick'],
    ['twamm', 'TWAMM'],
    ['settlement', 'Settlement'],
    ['assets', 'RWA'],
    ['kyc', 'KYC'],
    ['enterprise', 'Enterprise'],
    ['audit', 'Audit'],
  ]

  return (
    <header className="topbar">
      <div className="brand-wrap">
        <div className="brand-mark">R</div>
        <div>
          <p className="eyebrow">Institutional RWA</p>
          <h1>RWADEX</h1>
        </div>
      </div>

      <nav className="nav">
        {pages.map(([id, label]) => (
          <button key={id} className={`nav-link ${page === id ? 'active' : ''}`} onClick={() => setPage(id)}>
            {label}
          </button>
        ))}
      </nav>

      <div className="topbar-right">
        <KycStatusBadge />
        <select
          className="cluster-select"
          value={role}
          onChange={(event) => setRole(event.target.value as Role)}
          aria-label="Role"
          title="Active role (RBAC)"
        >
          {ROLES.map((item) => (
            <option key={item} value={item}>
              {ROLE_LABELS[item]}
            </option>
          ))}
        </select>
        <button
          className={`sample-toggle ${executionMode === 'multisig' ? 'active' : ''}`}
          onClick={() => setExecutionMode(executionMode === 'multisig' ? 'direct' : 'multisig')}
          title="Route transactions to the multisig proposal queue instead of broadcasting"
        >
          <span className="sample-toggle-dot"></span>
          {executionMode === 'multisig' ? 'Multisig' : 'Direct'}
        </button>
        <button
          className={`sample-toggle ${sampleMode ? 'active' : ''}`}
          onClick={() => setSampleMode(!sampleMode)}
          aria-label="Toggle sample data"
        >
          <span className="sample-toggle-dot"></span>
          Sample Data
        </button>
        <select
          className="cluster-select"
          value={cluster}
          onChange={(event) => setCluster(event.target.value as ClusterName)}
          aria-label="Cluster"
        >
          {CLUSTERS.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
        {publicKey ? (
          <button className="connect-button" onClick={() => void disconnect()}>
            {publicKey.toBase58().slice(0, 4)}…{publicKey.toBase58().slice(-4)}
          </button>
        ) : (
          <button className="connect-button" onClick={() => void connect()} disabled={connecting}>
            {connecting ? 'Connecting…' : 'Connect wallet'}
          </button>
        )}
      </div>
      <button
        className="nav-toggle"
        onClick={() => setMenuOpen((open) => !open)}
        aria-label={menuOpen ? 'Close menu' : 'Open menu'}
        aria-expanded={menuOpen}
        aria-controls="nav-drawer"
      >
        ☰
      </button>
      {menuOpen ? (
        <>
          {/* Tapping anywhere outside the drawer dismisses it, so the menu
              never traps the page on a phone. */}
          <button className="nav-backdrop" aria-label="Close menu" onClick={() => setMenuOpen(false)} />
          <div className="nav-drawer" id="nav-drawer" onClick={() => setMenuOpen(false)}>
            {pages.map(([id, label]) => (
              <button key={id} className={`nav-link ${page === id ? 'active' : ''}`} onClick={() => setPage(id)}>
                {label}
              </button>
            ))}
          </div>
        </>
      ) : null}
    </header>
  )
}

export function EmptyHint({ children }: { children: ReactNode }) {
  return <p className="empty-hint">{children}</p>
}

export function TokenIcon({
  symbol,
  color,
  size = 40,
}: {
  symbol: string
  color?: string
  size?: number
}) {
  const bgColor = color || {
    'USDC': '#2775ca',
    'USDT': '#26a17b',
    'SOL': '#9945ff',
    'ETH': '#627eea',
    'BTC': '#f7931a',
    'UST': '#f59e0b',
    'EURC': '#3498db',
    'PYUSD': '#7c3aed',
  }[symbol.toUpperCase()] || '#60a5fa'
  
  const initials = symbol.slice(0, 2)
  
  return (
    <div 
      className="swap-token-icon"
      style={{ 
        width: size, 
        height: size, 
        borderRadius: size > 30 ? '50%' : '50%', 
        background: `linear-gradient(135deg, ${bgColor}, ${bgColor}dd)`,
        color: '#fff',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        fontSize: size > 20 ? '14px' : '10px',
        fontWeight: 700,
        boxShadow: `0 2px 8px ${bgColor}44`
      }}
    >
      {initials}
    </div>
  )
}

export function PriceDisplay({ price, change }: { price: string; change?: number }) {
  const isPositive = change !== undefined && change >= 0
  return (
    <div className="chart-ticker">
      <span className={`chart-price ${isPositive ? 'change-positive' : 'change-negative'}`}>
        {price}
      </span>
      {change !== undefined && (
        <span className={`chart-percent ${isPositive ? 'change-positive' : 'change-negative'}`}>
          {isPositive ? '+' : ''}{change.toFixed(2)}%
        </span>
      )}
    </div>
  )
}

export function PoolCard({
  name,
  pair,
  price,
  change,
  volume,
  liquidity,
  tvl,
  commission,
}: {
  name: string
  pair: string
  price: string
  change: number
  volume: string
  liquidity: string
  tvl: string
  commission: string
}) {
  const isPositive = change >= 0
  return (
    <div className="pool-card">
      <div className="pool-card-header">
        <span className="pool-card-name">{name}</span>
        <span className="pool-card-header-right">
          <span className="pool-card-pair">{pair}</span>
        </span>
      </div>
      <div className="pool-card-price">
        {price}
      </div>
      <div className={`pool-card-change ${isPositive ? 'positive' : 'negative'}`}>
        {Math.abs(change).toFixed(2)}%
      </div>
      <div className="pool-card-stats">
        <div className="pool-stat-row">
          <span className="pool-stat-label">24h Volume</span>
          <span className="pool-stat-value">{volume}</span>
        </div>
        <div className="pool-stat-row">
          <span className="pool-stat-label">Liquidity</span>
          <span className="pool-stat-value">{liquidity}</span>
        </div>
        <div className="pool-stat-row">
          <span className="pool-stat-label">TVL</span>
          <span className="pool-stat-value">{tvl}</span>
        </div>
        <div className="pool-stat-row">
          <span className="pool-stat-label">Commission</span>
          <span className="pool-stat-value">{commission}</span>
        </div>
      </div>
    </div>
  )
}

export interface TableColumn<T> {
  header: string
  /** Render one cell; return null to skip the row. */
  cell: (row: T) => ReactNode
  /** Optional text used for CSV export. */
  text?: (row: T) => string
}

export function DataTable<T>({
  columns,
  rows,
  empty,
  csvName,
}: {
  columns: TableColumn<T>[]
  rows: T[]
  empty: string
  csvName?: string
}) {
  if (rows.length === 0) return <EmptyHint>{empty}</EmptyHint>

  const exportCsv = () => {
    // Quote the header the same way values are quoted, so a header containing
    // a comma cannot shift every subsequent column.
    const header = columns.map((column) => JSON.stringify(column.header)).join(',')
    const lines = rows.map((row) =>
      columns
        .map((column) => {
          const text = column.text ? column.text(row) : String(column.cell(row) ?? '')
          return JSON.stringify(text)
        })
        .join(','),
    )
    const blob = new Blob([`${header}\n${lines.join('\n')}`], { type: 'text/csv' })
    const url = URL.createObjectURL(blob)
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = `${csvName ?? 'export'}.csv`
    anchor.click()
    URL.revokeObjectURL(url)
  }

  return (
    <div className="table-export-wrap">
      {csvName ? (
        <div className="table-head">
          <button className="btn btn-secondary table-export" onClick={exportCsv}>
            Export CSV
          </button>
        </div>
      ) : null}
      {/* The wrapper scrolls instead of letting a wide table squash its cells. */}
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <tr>
              {columns.map((column) => (
                <th key={column.header}>{column.header}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, index) => (
              <tr key={index}>
                {columns.map((column) => (
                  <td key={column.header}>{column.cell(row)}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}
