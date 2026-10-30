/**
 * RWA DEX TypeScript SDK — the deployable programs, as an importable library.
 *
 * Two layers, both exported here:
 *
 * 1. `RwaDexClient` — a connection + wallet with one module per program
 *    (`clmm`, `dlmm`, `maverick`, `twamm`, `orders`, `settlement`, `kyc`,
 *    `rwa`, `hooks`, `oracle`). Every action signs, sends and confirms for you;
 *    every module also exposes `.build` for the bare instruction.
 * 2. The protocol surface — program IDs, every PDA, every instruction builder,
 *    every account decoder, the Merkle whitelist helpers and the on-chain walk
 *    derivations. Framework-free: usable from React, Node, Bun or a test.
 *
 * ```ts
 * import { RwaDexClient, keypairWallet } from './sdk'
 * const dex = new RwaDexClient({ cluster: 'devnet', wallet: keypairWallet(kp) })
 * const pool = await dex.clmm.poolFor(mintA, mintB)
 * ```
 */

// --------------------------------------------------------------- client API

export { RwaDexClient, createClient, keypairWallet } from './client'
export type { Pdas, RwaDexClientConfig } from './client'

export {
  instructionList,
  requireWallet,
  sendBuilt,
} from './context'
export type {
  SdkContext,
  SimulateOptions,
  SimulateResult,
  TxOptions,
  TxResult,
} from './context'

export { accountsFor, getAccount } from './accounts'
export type { Accounts } from './accounts'

export { listOrders, listSettlementVaults, programAccounts } from './queries'

export { railWiring } from './wiring'
export type { RailKeys, RailWiring } from './wiring'

export { buildTransferProofs, buildWhitelist, hexToBytes } from './merkle'
export type { Whitelist } from './merkle'

// ------------------------------------------------------------ program modules

export { createClmmModule } from './programs/clmm'
export type { ClmmModule } from './programs/clmm'

export { createDlmmModule } from './programs/dlmm'
export type { DlmmModule } from './programs/dlmm'

export { createMaverickModule } from './programs/maverick'
export type { MaverickModule } from './programs/maverick'

export { createTwammModule } from './programs/twamm'
export type { TwammModule } from './programs/twamm'

export { createOrdersModule } from './programs/orders'
export type { OrdersModule } from './programs/orders'

export { createSettlementModule } from './programs/settlement'
export type { SettlementModule, SettlementSettleArgs } from './programs/settlement'

export { createKycModule } from './programs/kyc'
export type { KycModule } from './programs/kyc'

export { createRwaModule } from './programs/rwa'
export type { CreateRwaMintArgs, RwaModule } from './programs/rwa'

export { createHooksModule } from './programs/hooks'
export type { HooksModule } from './programs/hooks'

export { createOracleModule } from './programs/oracle'
export type { OracleModule } from './programs/oracle'

// ------------------------------------------------------------ protocol surface

export * from './core/pdas'
export * from './core/borsh'
export * from './core/instructions-clmm'
export * from './core/instructions-dlmm'
export * from './core/instructions-maverick'
export * from './core/instructions-twamm'
export * from './core/instructions-orderbook'
export * from './core/instructions-rails'
export * from './core/decoders'
export * from './core/merkle'
export * from './core/metadata'
export * from './core/walks'

export {
  RPC_ENDPOINTS,
  confirmTx,
  connectionFor,
  createAtaInstruction,
  describeError,
  ensureAtaInstruction,
  getInjectedWallet,
  hookAccountsForMint,
  hookAccountsForTransfers,
  lamportsToSol,
  loadLocalKeypair,
  noneOf,
  sendWithSigners,
  sendWithSignersDetailed,
  shortAddress,
  tokenProgramForMint,
  transferFeeForMint,
} from './core/connection'
export type {
  AccountKey,
  ClusterName,
  InjectedProvider,
  SentTransaction,
  WalletLike,
} from './core/connection'
