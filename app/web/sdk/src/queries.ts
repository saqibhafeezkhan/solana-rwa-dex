/**
 * Account listings.
 *
 * A single-address read (`dex.accounts.*`) cannot answer "every order on this
 * book" — the program keeps no such index, so a client filters program accounts
 * by discriminator-sized data and a `memcmp` on the stored key. These helpers
 * do exactly what the pages do, with the same filters and the shared decoders.
 */
import type { Connection, GetProgramAccountsFilter, PublicKey } from '@solana/web3.js'
import {
  ORDER_BOOK_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
} from './core/pdas'
import {
  ORDER_LEN,
  SETTLEMENT_VAULT_LEN,
  decodeOrder,
  decodeSettlementVault,
  type AccountRow,
  type OrderView,
  type SettlementVaultView,
} from './core/decoders'

/**
 * Fetches program accounts as decoder rows. Failures inside a row's decoder are
 * not swallowed: a listing that silently drops accounts hides a wrong filter.
 */
export async function programAccounts(
  connection: Connection,
  programId: PublicKey,
  filters: GetProgramAccountsFilter[] = [],
): Promise<AccountRow[]> {
  const response = await connection.getProgramAccounts(programId, { filters })
  return response.map(({ pubkey, account }) => ({
    pubkey,
    owner: account.owner,
    data: account.data,
    lamports: account.lamports,
  }))
}

/**
 * Every resting order on one book, straight from the program.
 *
 * Orders store their book at offset 8 (after the 8-byte discriminator), and
 * `Order::LEN` separates them from the smaller book account, so the filter is
 * exact rather than a size heuristic.
 */
export async function listOrders(
  connection: Connection,
  book: PublicKey,
): Promise<OrderView[]> {
  const rows = await programAccounts(connection, ORDER_BOOK_PROGRAM_ID, [
    { dataSize: ORDER_LEN },
    { memcmp: { offset: 8, bytes: book.toBase58() } },
  ])
  return rows.map(decodeOrder)
}

/**
 * Every settlement vault, optionally narrowed to one taker.
 *
 * A vault stores `trade_id (8)` then `taker (32)` after the discriminator, so a
 * taker filter is a `memcmp` at offset 16 — the same filter the settlement page
 * uses for "mine only".
 */
export async function listSettlementVaults(
  connection: Connection,
  options: { taker?: PublicKey } = {},
): Promise<SettlementVaultView[]> {
  const filters: GetProgramAccountsFilter[] = [{ dataSize: SETTLEMENT_VAULT_LEN }]
  if (options.taker) {
    filters.push({ memcmp: { offset: 16, bytes: options.taker.toBase58() } })
  }
  const rows = await programAccounts(connection, SETTLEMENT_PROGRAM_ID, filters)
  return rows.map(decodeSettlementVault)
}
