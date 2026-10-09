import { useState } from 'react'
import './App.css'
import { AppProvider } from './ui/AppContext'
import { Toasts, TopBar } from './ui/components'
import { OverviewPage } from './ui/pages/OverviewPage'
import { PortfolioPage } from './ui/pages/PortfolioPage'
import { ClmmPage } from './ui/pages/ClmmPage'
import { DlmmPage } from './ui/pages/DlmmPage'
import { MaverickPage } from './ui/pages/MaverickPage'
import { TwammPage } from './ui/pages/TwammPage'
import { SettlementPage } from './ui/pages/SettlementPage'
import { KycPage } from './ui/pages/KycPage'
import { AssetsPage } from './ui/pages/AssetsPage'
import { EnterprisePage } from './ui/pages/EnterprisePage'
import { AuditPage } from './ui/pages/AuditPage'

const PAGES: Record<string, () => React.JSX.Element> = {
  overview: OverviewPage,
  portfolio: PortfolioPage,
  clmm: ClmmPage,
  dlmm: DlmmPage,
  maverick: MaverickPage,
  twamm: TwammPage,
  settlement: SettlementPage,
  kyc: KycPage,
  assets: AssetsPage,
  enterprise: EnterprisePage,
  audit: AuditPage,
}

function Shell() {
  const [page, setPage] = useState('overview')
  const Page = PAGES[page] ?? OverviewPage
  return (
    <div className="app-shell">
      <TopBar page={page} setPage={setPage} />
      <main className="dashboard">
        <Page />
      </main>
      <Toasts />
    </div>
  )
}

export default function App() {
  return (
    <AppProvider>
      <Shell />
    </AppProvider>
  )
}
