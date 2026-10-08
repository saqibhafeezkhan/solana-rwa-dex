import { useState } from 'react'
import type { ReactNode } from 'react'
import { useApp } from './AppContext'
import type { ClusterName } from '../core/connection'

export function Panel({ title, subtitle, children, aside }: { title: string; subtitle?: string; children: ReactNode; aside?: ReactNode }) {
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

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
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

export function Select(props: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return <select className="text-input" {...props} />
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

export function Badge({ children, tone = 'neutral' }: { children: ReactNode; tone?: 'neutral' | 'good' | 'warn' | 'bad' }) {
  return <span className={`badge badge-${tone}`}>{children}</span>
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
  const { cluster, setCluster, publicKey, connecting, connect, disconnect } = useApp()
  const [menuOpen, setMenuOpen] = useState(false)

  const pages: [string, string][] = [
    ['overview', 'Overview'],
    ['portfolio', 'Portfolio'],
    ['clmm', 'CLMM'],
    ['dlmm', 'DLMM'],
    ['maverick', 'Maverick'],
    ['twamm', 'TWAMM'],
    ['settlement', 'Settlement'],
    ['kyc', 'KYC & Whitelist'],
    ['assets', 'RWA Assets'],
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
      <button className="nav-toggle" onClick={() => setMenuOpen((open) => !open)} aria-label="Menu">
        ☰
      </button>
      {menuOpen ? (
        <div className="nav-drawer" onClick={() => setMenuOpen(false)}>
          {pages.map(([id, label]) => (
            <button key={id} className={`nav-link ${page === id ? 'active' : ''}`} onClick={() => setPage(id)}>
              {label}
            </button>
          ))}
        </div>
      ) : null}
    </header>
  )
}

export function EmptyHint({ children }: { children: ReactNode }) {
  return <p className="empty-hint">{children}</p>
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
    const header = columns.map((column) => column.header).join(',')
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
    <div className="table-wrap">
      {csvName ? (
        <button className="btn btn-secondary table-export" onClick={exportCsv}>
          Export CSV
        </button>
      ) : null}
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
  )
}
