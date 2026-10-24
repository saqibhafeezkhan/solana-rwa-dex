import { PublicKey } from '@solana/web3.js'
import { useApp } from '../useApp'
import { ActionButton, Badge, DataTable, EmptyHint, KeyValue, Panel, StatusLine } from '../components'
import { useAccountState } from '../useAccountState'
import {
  CLMM_PROGRAM_ID,
  DLMM_PROGRAM_ID,
  MAVERICK_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  TWAMM_PROGRAM_ID,
} from '../../core/pdas'
import { clmmAmounts } from '../pricing'
import {
  decodeBinPosition,
  decodeClmmPosition,
  decodeClmmPool,
  decodeDlmmPool,
  decodeMaverickPool,
  decodeSettlementVault,
  decodeTwammLp,
  decodeTwammOrder,
  decodeTwammPool,
  type AccountRow,
} from '../../core/decoders'

interface PositionRow {
  key: string
  protocol: string
  label: string
  detail: string
  amounts: string
  account: PublicKey
}

interface OrderRow {
  key: string
  id: string
  direction: string
  escrowed: string
  sold: string
  proceeds: string
  status: string
  expiry: string
  account: PublicKey
}

interface EscrowRow {
  key: string
  tradeId: string
  amount: string
  mint: string
  status: string
  settlementSlot: string
  account: PublicKey
}

const memcmpOwner = (owner: PublicKey) => ({
  memcmp: { offset: 8 + 32, bytes: owner.toBase58() },
})

async function fetchOwnedRows(
  connection: ReturnType<typeof useApp>['connection'],
  programId: PublicKey,
  owner: PublicKey,
): Promise<AccountRow[]> {
  const response = await connection.getProgramAccounts(programId, {
    filters: [memcmpOwner(owner)],
    encoding: 'base64',
  })
  return response.map((item) => ({
    pubkey: item.pubkey,
    owner: item.account.owner,
    data: item.account.data,
    lamports: item.account.lamports,
  }))
}

const u32le = (data: Uint8Array, at: number) =>
  data[at] | (data[at + 1] << 8) | (data[at + 2] << 16) | (data[at + 3] << 24)

function asciiAt(data: Uint8Array, at: number, length: number): string | null {
  let out = ''
  for (let i = 0; i < length; i++) {
    const byte = data[at + i]
    if (byte === undefined || byte < 0x20 || byte > 0x7e) return null
    out += String.fromCharCode(byte)
  }
  return out
}

/**
 * Extracts the Token-2022 `symbol` from a mint carrying the TokenMetadata
 * extension: after `key(4) + update_authority(32) + mint(32)` come three
 * borsh strings (name, symbol, uri). The base mint and extension TLVs shift
 * that prefix around, so scan for the first (short printable string, short
 * printable string) pair — the name + symbol pair.
 */
function extractSymbol(data: Uint8Array): string | null {
  for (let p = 82; p + 8 <= data.length; p++) {
    const nameLen = u32le(data, p)
    if (nameLen < 1 || nameLen > 32) continue
    const name = asciiAt(data, p + 4, nameLen)
    if (!name) continue
    const symAt = p + 4 + nameLen
    const symLen = u32le(data, symAt)
    if (symLen < 1 || symLen > 10) continue
    const symbol = asciiAt(data, symAt + 4, symLen)
    if (symbol) return symbol
  }
  return null
}

async function loadMintLabel(
  connection: ReturnType<typeof useApp>['connection'],
  cache: Map<string, string>,
  mint: PublicKey,
): Promise<string> {
  const key = mint.toBase58()
  const cached = cache.get(key)
  if (cached) return cached
  let label = `${key.slice(0, 4)}…${key.slice(-4)}`
  try {
    const info = await connection.getAccountInfo(mint)
    if (info) {
      const symbol = extractSymbol(info.data)
      if (symbol) label = symbol
    }
  } catch {
    // keep the truncated address
  }
  cache.set(key, label)
  return label
}

export function PortfolioPage() {
  const { connection, publicKey } = useApp()

  const scan = useAccountState(
    async () => {
      if (!publicKey) return null
      const symbolCache = new Map<string, string>()

      // ---- CLMM positions -----------------------------------------------
      const clmmPositions: PositionRow[] = []
      const clmmRows = await fetchOwnedRows(connection, CLMM_PROGRAM_ID, publicKey)
      for (const row of clmmRows) {
        // `Position::LEN` is 144 bytes and a pool account is thousands; the
        // owner memcmp already excludes pools/ticks, so anything larger than a
        // position here is not one. (This used to be `< 200`, which filtered
        // out exactly the positions it was meant to keep.)
        if (row.data.length > 200) continue
        try {
          const position = decodeClmmPosition(row)
          const pool = decodeClmmPool(await rawRow(connection, position.pool))
          const amounts = clmmAmounts(pool, position.lowerTick, position.upperTick, position.liquidity)
          const symbolA = await loadMintLabel(connection, symbolCache, pool.mintA)
          const symbolB = await loadMintLabel(connection, symbolCache, pool.mintB)
          const fees =
            position.tokensOwedA > 0n || position.tokensOwedB > 0n
              ? ` · fees ${position.tokensOwedA} ${symbolA} / ${position.tokensOwedB} ${symbolB}`
              : ''
          clmmPositions.push({
            key: position.address.toBase58(),
            protocol: 'CLMM',
            label: `${position.lowerTick} → ${position.upperTick}`,
            detail: `${symbolA}/${symbolB}`,
            amounts: `${amounts}${fees}`,
            account: position.address,
          })
        } catch {
          // not a position-shaped account; skip
        }
      }

      // ---- DLMM / Maverick positions -------------------------------------
      const binPositions: PositionRow[] = []
      for (const [programId, protocol, decodePool] of [
        [DLMM_PROGRAM_ID, 'DLMM', decodeDlmmPool],
        [MAVERICK_PROGRAM_ID, 'Maverick', decodeMaverickPool],
      ] as const) {
        const rows = await fetchOwnedRows(connection, programId, publicKey)
        for (const row of rows) {
          // `Position::LEN` is 92 bytes (8 + 32 + 32 + 4 + 16) — the smallest
          // accounts either program owns. (This used to compare against 89,
          // which matches no account on either program, so no bin position ever
          // reached the table.)
          if (row.data.length !== 92) continue
          try {
            const position = decodeBinPosition(row)
            const pool = decodePool(await rawRow(connection, position.pool))
            const symbolA = await loadMintLabel(connection, symbolCache, pool.mintA)
            const symbolB = await loadMintLabel(connection, symbolCache, pool.mintB)
            const range =
              position.binId === pool.activeBinId
                ? 'in range'
                : position.binId > pool.activeBinId
                  ? 'above price'
                  : 'below price'
            binPositions.push({
              key: position.address.toBase58(),
              protocol,
              label: `bin ${position.binId}`,
              detail: `${symbolA}/${symbolB} · ${range}`,
              amounts: `${position.shares} shares`,
              account: position.address,
            })
          } catch {
            // skip
          }
        }
      }

      // ---- TWAMM LP + orders ----------------------------------------------
      const twammLpRows = await fetchOwnedRows(connection, TWAMM_PROGRAM_ID, publicKey)
      const twammPositions: PositionRow[] = []
      const orders: OrderRow[] = []
      for (const row of twammLpRows) {
        // An LpPosition is exactly 89 bytes; orders are larger. Decode each by
        // shape instead of `continue`-ing away the orders before they are read.
        if (row.data.length === 89) {
          try {
            const lp = decodeTwammLp(row)
            const pool = decodeTwammPool(await rawRow(connection, lp.pool))
            const symbolA = await loadMintLabel(connection, symbolCache, pool.mintA)
            const symbolB = await loadMintLabel(connection, symbolCache, pool.mintB)
            const sharePct =
              pool.lpSupply > 0n
                ? ` (${((Number(lp.shares) / Number(pool.lpSupply)) * 100).toFixed(2)}% of pool)`
                : ''
            twammPositions.push({
              key: lp.address.toBase58(),
              protocol: 'TWAMM LP',
              label: `${lp.shares} shares${sharePct}`,
              detail: `${symbolA}/${symbolB}`,
              amounts: `reserves ${pool.reserveA} ${symbolA} · ${pool.reserveB} ${symbolB}`,
              account: lp.address,
            })
          } catch {
            // skip
          }
          continue
        }
        if (row.data.length < 89) continue
        try {
          const order = decodeTwammOrder(row)
          const pool = decodeTwammPool(await rawRow(connection, order.pool))
          const symbolA = await loadMintLabel(connection, symbolCache, pool.mintA)
          const symbolB = await loadMintLabel(connection, symbolCache, pool.mintB)
          orders.push({
            key: order.address.toBase58(),
            id: order.orderId.toString(),
            direction: order.isAToB ? `${symbolA} → ${symbolB}` : `${symbolB} → ${symbolA}`,
            escrowed: order.amountIn.toString(),
            sold: order.sold.toString(),
            proceeds: order.proceedsOut.toString(),
            status: order.settled ? 'settled' : order.expiryTimestamp * 1000n < BigInt(Date.now()) ? 'expired' : 'live',
            expiry: new Date(Number(order.expiryTimestamp) * 1000).toLocaleString(),
            account: order.address,
          })
        } catch {
          // skip
        }
      }

      // ---- Settlement escrows ----------------------------------------------
      // The vault's taker sits at offset 16 (discriminator + trade id).
      const escrows: EscrowRow[] = []
      const vaultRows = await connection.getProgramAccounts(SETTLEMENT_PROGRAM_ID, {
        filters: [{ memcmp: { offset: 8 + 8, bytes: publicKey.toBase58() } }],
        encoding: 'base64',
      })
      for (const item of vaultRows) {
        try {
          const vault = decodeSettlementVault({
            pubkey: item.pubkey,
            owner: item.account.owner,
            data: item.account.data,
            lamports: item.account.lamports,
          })
          const mintLabel = await loadMintLabel(connection, symbolCache, vault.tokenMint)
          escrows.push({
            key: vault.address.toBase58(),
            tradeId: vault.tradeId.toString(),
            amount: vault.amount.toString(),
            mint: mintLabel,
            status: vault.status,
            settlementSlot: vault.settlementSlot.toString(),
            account: vault.address,
          })
        } catch {
          // skip
        }
      }
      return {
        positions: [...clmmPositions, ...binPositions, ...twammPositions],
        orders,
        escrows,
      }
    },
    [connection, publicKey?.toBase58()],
  )

  const positions = scan.state?.positions ?? []
  const orders = scan.state?.orders ?? []
  const escrows = scan.state?.escrows ?? []

  return (
    <>
      <Panel
        title="Portfolio"
        subtitle={`Everything the connected wallet owns across CLMM, DLMM, Maverick, TWAMM and settlement — scanned live from ${connection.rpcEndpoint}.`}
        aside={
          <Badge tone={scan.error ? 'bad' : scan.loading ? 'warn' : 'good'}>
            {scan.error ? 'scan failed' : scan.loading ? 'scanning…' : `${positions.length}+${orders.length}+${escrows.length} accounts`}
          </Badge>
        }
      >
        <StatusLine error={scan.error} />
        {publicKey ? (
          <KeyValue
            rows={[
              ['Wallet', publicKey.toBase58()],
              ['Positions', positions.length.toString()],
              ['TWAMM orders', orders.length.toString()],
              ['Settlement escrows', escrows.length.toString()],
            ]}
          />
        ) : null}
        {!publicKey ? <EmptyHint>Connect a wallet to scan your holdings.</EmptyHint> : null}
        <ActionButton label="Rescan" variant="secondary" onClick={() => void scan.refresh()} disabled={scan.loading} />
      </Panel>

      <Panel title="Liquidity positions" subtitle="CLMM tick ranges, per-bin DLMM/Maverick positions and TWAMM LP shares.">
        <DataTable
          csvName="rwadex-positions"
          empty="No liquidity positions found for this wallet on this cluster."
          rows={positions}
          columns={[
            { header: 'Protocol', cell: (row) => row.protocol, text: (row) => row.protocol },
            { header: 'Pair', cell: (row) => row.detail, text: (row) => row.detail },
            { header: 'Range / Bin', cell: (row) => row.label, text: (row) => row.label },
            { header: 'Holdings', cell: (row) => row.amounts, text: (row) => row.amounts },
            {
              header: 'Account',
              cell: (row) => (
                <span title={row.account.toBase58()} className="mono">
                  {short(row.account)}
                </span>
              ),
              text: (row) => row.account.toBase58(),
            },
          ]}
        />
      </Panel>

      <Panel title="TWAMM orders" subtitle="Long-term orders created by this wallet, live and settled.">
        <DataTable
          csvName="rwadex-orders"
          empty="No TWAMM orders found for this wallet on this cluster."
          rows={orders}
          columns={[
            { header: 'Order id', cell: (row) => row.id, text: (row) => row.id },
            { header: 'Direction', cell: (row) => row.direction, text: (row) => row.direction },
            { header: 'Escrowed', cell: (row) => row.escrowed, text: (row) => row.escrowed },
            { header: 'Sold', cell: (row) => row.sold, text: (row) => row.sold },
            { header: 'Proceeds', cell: (row) => row.proceeds, text: (row) => row.proceeds },
            {
              header: 'Status',
              cell: (row) => (
                <Badge key={row.status} tone={row.status === 'live' ? 'good' : row.status === 'settled' ? 'neutral' : 'warn'}>
                  {row.status}
                </Badge>
              ),
              text: (row) => row.status,
            },
            { header: 'Expiry', cell: (row) => row.expiry, text: (row) => row.expiry },
            {
              header: 'Account',
              cell: (row) => (
                <span title={row.account.toBase58()} className="mono">
                  {short(row.account)}
                </span>
              ),
              text: (row) => row.account.toBase58(),
            },
          ]}
        />
      </Panel>

      <Panel title="Settlement escrows" subtitle="T+N escrows addressed to this wallet as taker, pending or released.">
        <DataTable
          csvName="rwadex-escrows"
          empty="No settlement escrows found for this wallet on this cluster."
          rows={escrows}
          columns={[
            { header: 'Trade id', cell: (row) => row.tradeId, text: (row) => row.tradeId },
            { header: 'Amount', cell: (row) => row.amount, text: (row) => row.amount },
            { header: 'Mint', cell: (row) => row.mint, text: (row) => row.mint },
            {
              header: 'Status',
              cell: (row) => (
                <Badge
                  key={row.status}
                  tone={
                    row.status === 'pending'
                      ? 'warn'
                      : row.status === 'settled'
                        ? 'good'
                        : row.status === 'expired'
                          ? 'neutral'
                          : 'bad'
                  }
                >
                  {row.status}
                </Badge>
              ),
              text: (row) => row.status,
            },
            { header: 'Settlement slot', cell: (row) => row.settlementSlot, text: (row) => row.settlementSlot },
            {
              header: 'Account',
              cell: (row) => (
                <span title={row.account.toBase58()} className="mono">
                  {short(row.account)}
                </span>
              ),
              text: (row) => row.account.toBase58(),
            },
          ]}
        />
      </Panel>
    </>
  )
}

/** Fetches one raw account as an AccountRow (throws when missing). */
async function rawRow(connection: ReturnType<typeof useApp>['connection'], address: PublicKey): Promise<AccountRow> {
  const info = await connection.getAccountInfo(address)
  if (!info) throw new Error(`account ${address.toBase58()} not found`)
  return { pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports }
}

function short(address: PublicKey): string {
  const text = address.toBase58()
  return `${text.slice(0, 4)}…${text.slice(-4)}`
}
