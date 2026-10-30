/**
 * Account loading, typed by the app's field-exact decoders.
 *
 * Every method takes an **address**; PDAs are derived with the helpers in
 * `pdas` (re-exported from the package root) or by the `*For` helpers on the
 * program modules. A missing account throws instead of decoding garbage, and a
 * wrong-shape account fails inside the decoder's bounds checks.
 */
import type { Connection, PublicKey } from '@solana/web3.js'
import {
  decodeBinPosition,
  decodeClmmPosition,
  decodeTwammLp,
  decodeTwammOrder,
  loadClmmPool,
  loadDlmmPool,
  loadKycConfig,
  loadMaverickBinArray,
  loadMaverickPool,
  loadMint,
  loadMintAgent,
  loadOracleState,
  loadOrder,
  loadOrderBook,
  loadSettlementConfig,
  loadSettlementVault,
  loadTokenAccount,
  loadTransferHookConfig,
  loadTwammPool,
  type AccountRow,
} from './core/decoders'
import { loadTokenMetadata } from './core/metadata'
import { tokenProgramForMint, transferFeeForMint } from './core/connection'

/** Fetches an account as the decoder's row shape (or throws when absent). */
export async function getAccount(connection: Connection, address: PublicKey): Promise<AccountRow> {
  const info = await connection.getAccountInfo(address)
  if (!info) throw new Error(`account ${address.toBase58()} not found on ${connection.rpcEndpoint}`)
  return { pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports }
}

async function decodeAt<T>(
  connection: Connection,
  address: PublicKey,
  decode: (row: AccountRow) => T,
): Promise<T> {
  return decode(await getAccount(connection, address))
}

/**
 * A client-bound set of account readers: `client.accounts.clmmPool(pool)`.
 * The raw decoders are exported from the package root for callers that already
 * hold a row (an indexer, a test fixture).
 */
export function accountsFor(connection: Connection) {
  return {
    account: (address: PublicKey) => getAccount(connection, address),

    clmmPool: (address: PublicKey) => loadClmmPool(connection, address),
    clmmPosition: (address: PublicKey) => decodeAt(connection, address, decodeClmmPosition),

    dlmmPool: (address: PublicKey) => loadDlmmPool(connection, address),
    maverickPool: (address: PublicKey) => loadMaverickPool(connection, address),
    /** Shared by DLMM and Maverick positions (same on-chain layout). */
    binPosition: (address: PublicKey) => decodeAt(connection, address, decodeBinPosition),
    maverickBinArray: (address: PublicKey) => loadMaverickBinArray(connection, address),

    twammPool: (address: PublicKey) => loadTwammPool(connection, address),
    twammLp: (address: PublicKey) => decodeAt(connection, address, decodeTwammLp),
    twammOrder: (address: PublicKey) => decodeAt(connection, address, decodeTwammOrder),

    orderBook: (address: PublicKey) => loadOrderBook(connection, address),
    order: (address: PublicKey) => loadOrder(connection, address),

    oracle: (address: PublicKey) => loadOracleState(connection, address),

    settlementConfig: (address: PublicKey) => loadSettlementConfig(connection, address),
    settlementVault: (address: PublicKey) => loadSettlementVault(connection, address),
    mintAgent: (address: PublicKey) => loadMintAgent(connection, address),

    hookConfig: (address: PublicKey) => loadTransferHookConfig(connection, address),
    kycConfig: (address: PublicKey) => loadKycConfig(connection, address),

    tokenAccount: (address: PublicKey) => loadTokenAccount(connection, address),
    mint: (address: PublicKey) => loadMint(connection, address),
    metadata: (mint: PublicKey) => loadTokenMetadata(connection, mint),

    /** The token program that owns the mint (legacy SPL Token or Token-2022). */
    tokenProgram: (mint: PublicKey) => tokenProgramForMint(connection, mint),
    /** The fee in force this epoch, or null when the mint has none. */
    transferFee: (mint: PublicKey) => transferFeeForMint(connection, mint),
  }
}

export type Accounts = ReturnType<typeof accountsFor>
