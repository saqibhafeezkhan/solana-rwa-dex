import { useMemo } from 'react'
import { useApp } from '../useApp'
import { 
  Badge, 
  EmptyHint, 
  KeyValue, 
  Panel, 
  PoolCard,
  PriceTileRow
} from '../components'
import { useAccountState } from '../useAccountState'
import {
  kycConfig,
  hookAuthority,
  settlementConfig,
  settlementVault,
  SETTLEMENT_PROGRAM_ID,
  ata,
  TOKEN_2022_PROGRAM_ID,
} from '../../core/pdas'
import {
  SETTLEMENT_VAULT_LEN,
  decodeSettlementVault,
  loadKycConfig,
  loadSettlementConfig,
  loadTokenAccount,
  type SettlementVaultView,
} from '../../core/decoders'
import { shortAddress } from '../../core/connection'
import {
  SAMPLE_SETTLEMENT_CONFIG,
  SAMPLE_KYC_CONFIG,
  SAMPLE_CLMM_POOL,
  SAMPLE_DLMM_POOL,
} from '../../core/sampleData'

export function OverviewPage() {
  const { connection, publicKey, cluster, sampleMode } = useApp()

  // Use sample data when sampleMode is on, otherwise fetch from chain
  const settlement = useAccountState(
    async () => {
      if (sampleMode) return SAMPLE_SETTLEMENT_CONFIG
      return loadSettlementConfig(connection, settlementConfig())
    },
    [connection, sampleMode],
  )
  const kyc = useAccountState(
    async () => {
      if (sampleMode) return SAMPLE_KYC_CONFIG
      return loadKycConfig(connection, kycConfig())
    },
    [connection, sampleMode],
  )
  const hooks = useAccountState(
    async () => {
      if (sampleMode) return { lamports: 1_000_000_000 }
      const info = await connection.getAccountInfo(hookAuthority())
      return info ? { lamports: info.lamports } : null
    },
    [connection, sampleMode],
  )

  // Sample settlement escrows for demonstration
  const myEscrows = useAccountState(
    async () => {
      if (!publicKey) return []
      if (sampleMode) {
        return [
          {
            address: settlementVault(1n),
            tradeId: 1n,
            taker: publicKey,
            tokenMint: SAMPLE_KYC_CONFIG.kycMint,
            amount: 1000000n,
            settlementSlot: 1234n,
            status: 'pending' as const,
            pool: SAMPLE_CLMM_POOL.address,
            refundSource: SAMPLE_CLMM_POOL.address,
          },
          {
            address: settlementVault(2n),
            tradeId: 2n,
            taker: publicKey,
            tokenMint: SAMPLE_KYC_CONFIG.kycMint,
            amount: 500000n,
            settlementSlot: 1240n,
            status: 'pending' as const,
            pool: SAMPLE_DLMM_POOL.address,
            refundSource: SAMPLE_DLMM_POOL.address,
          },
        ]
      }
      // Ask the RPC for vaults whose taker is this wallet rather than probing a
      // fixed trade-id range: anything above id 8 was invisible before.
      const response = await connection.getProgramAccounts(SETTLEMENT_PROGRAM_ID, {
        filters: [
          { dataSize: SETTLEMENT_VAULT_LEN },
          { memcmp: { offset: 8 + 8, bytes: publicKey.toBase58() } },
        ],
        encoding: 'base64',
      })
      const found: SettlementVaultView[] = []
      for (const item of response) {
        try {
          found.push(
            decodeSettlementVault({
              pubkey: item.pubkey,
              owner: item.account.owner,
              data: item.account.data,
              lamports: item.account.lamports,
            }),
          )
        } catch {
          // undecodable or partially-written vault; skip
        }
      }
      return found.sort((a, b) => (a.tradeId < b.tradeId ? -1 : a.tradeId > b.tradeId ? 1 : 0))
    },
    [connection, publicKey, sampleMode],
  )

  const kycAtaBalance = useAccountState(
    async () => {
      if (!publicKey) return null
      const kycMint = sampleMode ? SAMPLE_KYC_CONFIG.kycMint : kyc.state?.kycMint
      if (!kycMint) return 0n
      const address = ata(publicKey, kycMint, TOKEN_2022_PROGRAM_ID)
      if (sampleMode) return 100n // Sample KYC attestation
      try {
        const account = await loadTokenAccount(connection, address)
        return account.amount
      } catch {
        return 0n
      }
    },
    [connection, publicKey, kyc.state, sampleMode],
  )

  const walletSummary = useMemo(() => {
    if (!publicKey) return null
    return {
      address: publicKey.toBase58(),
    }
  }, [publicKey])

  // Sample protocol stats
  const protocolStats = useMemo(() => ({
    totalLiquidity: sampleMode ? '12,847,293' : '—',
    totalVolume24h: sampleMode ? '1,456,821' : '—',
    activePools: sampleMode ? 12 : '—',
    totalUsers: sampleMode ? '2,847' : '—',
    avgPoolReturn: sampleMode ? 0.042 : 0,
  }), [sampleMode])

  // Sample pool cards. Rendered as a rail so the arrows page through the pairs;
  // each tile names the page that manages it.
  const samplePools = useMemo(
    () =>
      sampleMode
        ? [
            {
              name: 'SOL/USDC',
              pair: 'SOL / USDC',
              price: '$142.85',
              change: 2.34,
              volume: '$2,847,293',
              liquidity: '$12,847,293',
              tvl: '$12,847,293',
              commission: '0.30%',
            },
            {
              name: 'ETH/USDC',
              pair: 'ETH / USDC',
              price: '$3,456.78',
              change: -1.22,
              volume: '$1,234,567',
              liquidity: '$5,678,901',
              tvl: '$5,678,901',
              commission: '0.25%',
            },
            {
              name: 'WBTC/USDC',
              pair: 'WBTC / USDC',
              price: '$67,890.12',
              change: 0.87,
              volume: '$3,456,789',
              liquidity: '$8,901,234',
              tvl: '$8,901,234',
              commission: '0.15%',
            },
            {
              name: 'WBTC/ETH',
              pair: 'WBTC / ETH',
              price: '$19.64',
              change: -0.42,
              volume: '$1,987,654',
              liquidity: '$7,451,002',
              tvl: '$7,451,002',
              commission: '0.20%',
            },
            {
              name: 'PYUSD/USDC',
              pair: 'PYUSD / USDC',
              price: '$1.0002',
              change: 0.01,
              volume: '$964,120',
              liquidity: '$4,120,880',
              tvl: '$4,120,880',
              commission: '0.05%',
            },
          ]
        : [],
    [sampleMode],
  )

  return (
    <>
      {/* Hero Stats */}
      <div className="hero-stats animate-fade-in-up">
        <div className="stat-card">
          <div className="stat-label">Total Liquidity</div>
          <div className="stat-value shimmer-text">${protocolStats.totalLiquidity}</div>
          <div className={`stat-change ${protocolStats.avgPoolReturn > 0 ? 'positive' : 'negative'}`}>
            {sampleMode
              ? `${protocolStats.avgPoolReturn > 0 ? '▲' : '▼'} ${Math.abs(Number(protocolStats.avgPoolReturn) * 100).toFixed(2)}% APY`
              : '— live aggregate unavailable'}
          </div>
        </div>
        <div className="stat-card">
          <div className="stat-label">24h Volume</div>
          <div className="stat-value">${protocolStats.totalVolume24h}</div>
          <div className="stat-change positive">{sampleMode ? '▲ +12.4% from yesterday' : '—'}</div>
        </div>
        <div className="stat-card">
          <div className="stat-label">Active Pools</div>
          <div className="stat-value">{protocolStats.activePools}</div>
          <div className="stat-change positive">{sampleMode ? '▲ +2 new this week' : '—'}</div>
        </div>
        <div className="stat-card">
          <div className="stat-label">Total Users</div>
          <div className="stat-value">{protocolStats.totalUsers}</div>
          <div className="stat-change positive">{sampleMode ? '▲ +847 this month' : '—'}</div>
        </div>
      </div>

      {/* DEX Hero Section */}
      <div className="dex-hero animate-fade-in-up stagger-1">
        <div className="dex-hero-content">
          <h2 className="dex-hero-title">Institutional RWA DEX</h2>
          <p className="dex-hero-subtitle">
            Concentrated liquidity · Bin-based AMMs · Time-weighted markets · T+N settlement
          </p>
          
          <div className="decentralized-grid">
            <div className="dex-card">
              <span className="dex-card-icon">🎯</span>
              <div className="dex-card-name">CLMM</div>
              <div className="dex-card-desc">Concentrated liquidity with tick walks. Uniswap v3 style.</div>
            </div>
            <div className="dex-card">
              <span className="dex-card-icon">📦</span>
              <div className="dex-card-name">DLMM</div>
              <div className="dex-card-desc">Bin-based liquidity with zero-slippage bins. Meteora style.</div>
            </div>
            <div className="dex-card">
              <span className="dex-card-icon">📈</span>
              <div className="dex-card-name">Maverick</div>
              <div className="dex-card-desc">Dynamic distribution AMM with price-following bins.</div>
            </div>
            <div className="dex-card">
              <span className="dex-card-icon">⏰</span>
              <div className="dex-card-name">TWAMM</div>
              <div className="dex-card-desc">Time-weighted execution for long-term orders.</div>
            </div>
            <div className="dex-card">
              <span className="dex-card-icon">🔒</span>
              <div className="dex-card-name">Settlement</div>
              <div className="dex-card-desc">T+N escrow released by Transfer Agent signature.</div>
            </div>
          </div>
        </div>
      </div>

      {/* Price tiles — paged with the rail's left/right arrows. There is no
          on-chain pool registry to enumerate live, so the rail only appears in
          sample mode; an empty rail would just render two dead arrows. */}
      {samplePools.length > 0 ? (
        <PriceTileRow label="Pool price tiles">
          {samplePools.map((pool) => (
            <PoolCard key={pool.name} {...pool} />
          ))}
        </PriceTileRow>
      ) : null}

      {/* Infrastructure Status - Full Width */}
      <Panel
        title="Infrastructure status"
        subtitle={sampleMode
          ? `Sample mode — showing demo data. Switch off to read live from ${cluster}.`
          : `Read live from ${cluster}. "Not deployed" means the instruction that creates the account has not run on this cluster yet.`}
      >
        <div className="animate-fade-in-up stagger-2">
          <KeyValue
            rows={[
              [
                'Settlement config',
                settlement.state ? (
                  <Badge key="gate" tone={settlement.state.poolsRequireGate ? 'good' : 'warn'} className="animate-scale-in">
                    gate {settlement.state.poolsRequireGate ? 'on' : 'off'}
                  </Badge>
                ) : (
                  <Badge key="missing" tone="bad">not deployed</Badge>
                ),
              ],
              [
                'Transfer agent key',
                settlement.state
                  ? settlement.state.transferAgentSecp256k1.some((b) => b !== 0)
                    ? 'configured'
                    : 'not set'
                  : '—',
              ],
              [
                'KYC mint',
                kyc.state ? shortAddress(kyc.state.kycMint) : <Badge key="kyc-missing" tone="bad">not deployed</Badge>,
              ],
              [
                'KYC authority',
                kyc.state ? shortAddress(kyc.state.authority) : '—',
              ],
              [
                'Hook authority balance',
                hooks.state ? (
                  <span key="hooks-balance" className="mono animate-pulse">${(hooks.state.lamports / 1e9).toFixed(3)} SOL</span>
                ) : (
                  <Badge key="unfunded" tone="warn">unfunded</Badge>
                ),
              ],
            ]}
          />
        </div>
      </Panel>

      {/* My Settlement Escrows */}
      <Panel
        title="My settlement escrows"
        subtitle="Trade outputs locked in T+N escrow addressed to the connected wallet."
      >
        {publicKey ? (
          myEscrows.state && myEscrows.state.length > 0 ? (
            <div className="kv">
              {myEscrows.state.map((vault, index) => (
                <div key={vault.address.toBase58()} className={`kv-row animate-fade-in-up stagger-${Math.min(index + 1, 5)}`}>
                  <span>
                    trade {vault.tradeId.toString()} · {vault.status}
                  </span>
                  <strong>
                    {vault.amount.toString()} @ slot {vault.settlementSlot.toString()}
                  </strong>
                </div>
              ))}
            </div>
          ) : (
            <EmptyHint>
              {myEscrows.error ? 'No escrow accounts readable yet.' : 'No open escrows found for this wallet.'}
            </EmptyHint>
          )
        ) : (
          <EmptyHint>Connect a wallet to inspect your escrows.</EmptyHint>
        )}
        {walletSummary ? (
          <p className="empty-hint mono animate-fade-in-up stagger-4">wallet {walletSummary.address}</p>
        ) : null}
        {kycAtaBalance.state !== null && publicKey ? (
          <p className="empty-hint animate-fade-in-up stagger-5">
            KYC attestation: {kycAtaBalance.state > 0n ? <Badge tone="good">approved</Badge> : <Badge tone="warn">none</Badge>}
          </p>
        ) : null}
      </Panel>
    </>
  )
}
