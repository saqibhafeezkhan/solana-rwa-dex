import { useMemo } from 'react'
import { useApp } from '../AppContext'
import { Badge, EmptyHint, KeyValue, Panel } from '../components'
import { useAccountState } from '../useAccountState'
import {
  kycConfig,
  hookAuthority,
  settlementConfig,
  settlementVault,
  ata,
  TOKEN_2022_PROGRAM_ID,
} from '../../core/pdas'
import {
  loadKycConfig,
  loadSettlementConfig,
  loadSettlementVault,
  loadTokenAccount,
} from '../../core/decoders'
import { shortAddress } from '../../core/connection'

export function OverviewPage() {
  const { connection, publicKey, cluster } = useApp()

  const settlement = useAccountState(
    () => loadSettlementConfig(connection, settlementConfig()),
    [connection],
  )
  const kyc = useAccountState(() => loadKycConfig(connection, kycConfig()), [connection])
  const hooks = useAccountState(
    async () => {
      const info = await connection.getAccountInfo(hookAuthority())
      return info ? { lamports: info.lamports } : null
    },
    [connection],
  )

  // My settlement escrows: probe the first few trade ids of the connected wallet.
  const myEscrows = useAccountState(
    async () => {
      if (!publicKey) return []
      const found: Awaited<ReturnType<typeof loadSettlementVault>>[] = []
      for (let tradeId = 1; tradeId <= 8; tradeId++) {
        const address = settlementVault(BigInt(tradeId))
        const info = await connection.getAccountInfo(address)
        if (!info) continue
        try {
          const vault = await loadSettlementVault(connection, address)
          if (vault.taker.equals(publicKey)) found.push(vault)
        } catch {
          // undecodable or partially-written vault; skip
        }
      }
      return found
    },
    [connection, publicKey],
  )

  const kycAtaBalance = useAccountState(
    async () => {
      if (!publicKey || !kyc.state) return null
      const address = ata(publicKey, kyc.state.kycMint, TOKEN_2022_PROGRAM_ID)
      try {
        const account = await loadTokenAccount(connection, address)
        return account.amount
      } catch {
        return 0n
      }
    },
    [connection, publicKey, kyc.state],
  )

  const walletSummary = useMemo(() => {
    if (!publicKey) return null
    return {
      address: publicKey.toBase58(),
    }
  }, [publicKey])

  return (
    <>
      <div className="grid-2">
        <Panel title="Protocol map" subtitle="Every program in the RWA DEX and what it does. Open a tab to drive it.">
          <KeyValue
            rows={[
              ['CLMM', 'Concentrated liquidity (Uniswap v3 style) with tick walks'],
              ['DLMM', 'Bin-based liquidity with zero-slippage bins (Meteora style)'],
              ['Maverick', 'Dynamic distribution AMM whose bins follow the price'],
              ['TWAMM', 'Time-weighted execution of long-term orders'],
              ['Settlement', 'T+N escrow released by a Transfer Agent signature'],
              ['KYC gate', 'Soulbound attestations required by wired pools'],
              ['Oracle', 'TWAP circuit breaker around every swap'],
              ['RWA mint + hook', 'Token-2022 assets with Merkle-whitelisted transfers'],
            ]}
          />
        </Panel>

        <Panel
          title="Infrastructure status"
          subtitle={`Read live from ${cluster}. "Not deployed" means the instruction that creates the account has not run on this cluster yet.`}
        >
          <KeyValue
            rows={[
              [
                'Settlement config',
                settlement.state ? (
                  <Badge key="gate" tone={settlement.state.poolsRequireGate ? 'good' : 'warn'}>
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
                  `${(hooks.state.lamports / 1e9).toFixed(3)} SOL`
                ) : (
                  <Badge key="unfunded" tone="warn">unfunded</Badge>
                ),
              ],
            ]}
          />
        </Panel>
      </div>

      <Panel
        title="My settlement escrows"
        subtitle="Trade outputs locked in T+N escrow addressed to the connected wallet (trade ids 1–8)."
      >
        {publicKey ? (
          myEscrows.state && myEscrows.state.length > 0 ? (
            <div className="kv">
              {myEscrows.state.map((vault) => (
                <div key={vault.address.toBase58()} className="kv-row">
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
          <p className="empty-hint mono">wallet {walletSummary.address}</p>
        ) : null}
        {kycAtaBalance.state !== null && publicKey ? (
          <p className="empty-hint">
            KYC attestation: {kycAtaBalance.state > 0n ? <Badge tone="good">approved</Badge> : <Badge tone="warn">none</Badge>}
          </p>
        ) : null}
      </Panel>
    </>
  )
}
