import './App.css'

const stats = [
  { label: 'TVL', value: '$128.4M', change: '+8.2%' },
  { label: '24h Volume', value: '$31.7M', change: '+12.1%' },
  { label: 'Active Markets', value: '26', change: '+3' },
  { label: 'Risk Score', value: 'Low', change: 'Stable' },
]

const markets = [
  { name: 'US Treasury', symbol: 'UST', price: '99.82', change: '+0.12%', volume: '$8.2M', status: 'Live' },
  { name: 'Real Estate Fund', symbol: 'REF', price: '104.48', change: '+1.04%', volume: '$5.6M', status: 'Live' },
  { name: 'Private Credit', symbol: 'PCR', price: '97.25', change: '-0.34%', volume: '$4.7M', status: 'Watch' },
  { name: 'Carbon Credits', symbol: 'CNR', price: '62.10', change: '+2.76%', volume: '$2.9M', status: 'Live' },
]

const orderBook = [
  { price: '104.62', size: '18.4', bid: true },
  { price: '104.60', size: '12.9', bid: true },
  { price: '104.52', size: '8.6', bid: true },
  { price: '104.41', size: '14.2', bid: false },
  { price: '104.34', size: '9.5', bid: false },
  { price: '104.28', size: '20.1', bid: false },
]

const positions = [
  { pair: 'UST/USDC', side: 'Long', size: '18,500', pnl: '+$1,240' },
  { pair: 'REF/USDC', side: 'Short', size: '9,300', pnl: '+$480' },
  { pair: 'CNR/USDC', side: 'Long', size: '6,100', pnl: '-$140' },
]

function App() {
  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand-wrap">
          <div className="brand-mark">R</div>
          <div>
            <p className="eyebrow">Institutional RWA</p>
            <h1>RWADEX</h1>
          </div>
        </div>

        <nav className="nav">
          <a href="#">Markets</a>
          <a href="#">Portfolio</a>
          <a href="#">Strategies</a>
          <a href="#">Governance</a>
        </nav>

        <button className="connect-button">Connect wallet</button>
      </header>

      <main className="dashboard">
        <section className="hero-panel">
          <div>
            <p className="eyebrow accent">On-chain real-world asset exchange</p>
            <h2>Trade tokenized treasury and private credit assets</h2>
          </div>
          <div className="hero-actions">
            <button className="primary">Create order</button>
            <button className="secondary">View markets</button>
          </div>
        </section>

        <section className="stats-grid">
          {stats.map((stat) => (
            <article key={stat.label} className="stat-card">
              <span>{stat.label}</span>
              <strong>{stat.value}</strong>
              <em>{stat.change}</em>
            </article>
          ))}
        </section>

        <section className="content-grid">
          <div className="panel market-panel">
            <div className="panel-header">
              <h3>Markets</h3>
              <span className="badge">Updated 12s ago</span>
            </div>

            <div className="market-table">
              <div className="market-row table-head">
                <span>Asset</span>
                <span>Price</span>
                <span>24h</span>
                <span>Volume</span>
                <span>Status</span>
              </div>

              {markets.map((market) => (
                <div key={market.symbol} className="market-row">
                  <span className="asset-name">
                    <strong>{market.symbol}</strong>
                    {market.name}
                  </span>
                  <span>{market.price}</span>
                  <span className="change positive">{market.change}</span>
                  <span>{market.volume}</span>
                  <span className="status-pill">{market.status}</span>
                </div>
              ))}
            </div>
          </div>

          <div className="panel trade-panel">
            <div className="panel-header">
              <h3>Quick trade</h3>
              <span className="badge warning">Risk checked</span>
            </div>

            <div className="trade-tabs">
              <button className="active">Buy</button>
              <button>Sell</button>
            </div>

            <div className="trade-form">
              <label>
                Pair
                <select defaultValue="UST/USDC">
                  <option>UST/USDC</option>
                  <option>REF/USDC</option>
                  <option>CNR/USDC</option>
                </select>
              </label>

              <label>
                Amount
                <input type="text" defaultValue="2500" />
              </label>

              <label>
                Limit price
                <input type="text" defaultValue="99.82" />
              </label>

              <div className="trade-summary">
                <span>Estimated notional</span>
                <strong>$249,550</strong>
              </div>

              <button className="primary full">Place buy order</button>
            </div>
          </div>
        </section>

        <section className="bottom-grid">
          <div className="panel orderbook-panel">
            <div className="panel-header">
              <h3>Order book</h3>
            </div>

            <div className="book-list">
              {orderBook.map((row) => (
                <div key={row.price} className={`book-row ${row.bid ? 'bid' : 'ask'}`}>
                  <span>{row.price}</span>
                  <span>{row.size}</span>
                </div>
              ))}
            </div>
          </div>

          <div className="panel portfolio-panel">
            <div className="panel-header">
              <h3>Open positions</h3>
            </div>

            <div className="position-list">
              {positions.map((position) => (
                <div key={position.pair} className="position-row">
                  <div>
                    <strong>{position.pair}</strong>
                    <span>{position.side}</span>
                  </div>
                  <div>
                    <strong>{position.size}</strong>
                    <span>{position.pnl}</span>
                  </div>
                </div>
              ))}
            </div>
          </div>
        </section>
      </main>
    </div>
  )
}

export default App
