/**
 * The SDK entry object: one connection + one wallet, and a module per program.
 *
 * ```ts
 * import { RwaDexClient, keypairWallet } from '../sdk'
 *
 * const dex = new RwaDexClient({ cluster: 'devnet', wallet: keypairWallet(kp) })
 * const { signature } = await dex.clmm.swap({ ... })
 * ```
 *
 * Everything is built on the package's own encoders and PDAs (`./core`), which
 * the app imports directly too, so the SDK and the UI can never drift in account
 * order or seed derivation.
 */
import {
  ComputeBudgetProgram,
  Transaction,
  type Commitment,
  type Connection,
  type Keypair,
  type PublicKey,
  type TransactionInstruction,
} from '@solana/web3.js'
import {
  confirmTx,
  connectionFor,
  sendWithSignersDetailed,
  type ClusterName,
  type WalletLike,
} from './core/connection'
import * as pdas from './core/pdas'
import { accountsFor, type Accounts } from './accounts'
import {
  instructionList,
  requireWallet,
  type SdkContext,
  type SimulateOptions,
  type SimulateResult,
  type TxOptions,
  type TxResult,
} from './context'
import { createClmmModule, type ClmmModule } from './programs/clmm'
import { createDlmmModule, type DlmmModule } from './programs/dlmm'
import { createMaverickModule, type MaverickModule } from './programs/maverick'
import { createTwammModule, type TwammModule } from './programs/twamm'
import { createOrdersModule, type OrdersModule } from './programs/orders'
import { createSettlementModule, type SettlementModule } from './programs/settlement'
import { createKycModule, type KycModule } from './programs/kyc'
import { createRwaModule, type RwaModule } from './programs/rwa'
import { createHooksModule, type HooksModule } from './programs/hooks'
import { createOracleModule, type OracleModule } from './programs/oracle'

/** A Keypair dressed as a wallet, for scripts, seeders and tests. */
export function keypairWallet(keypair: Keypair): WalletLike {
  return {
    publicKey: keypair.publicKey,
    async signTransaction<T extends Transaction | { serialize(): Uint8Array }>(tx: T): Promise<T> {
      ;(tx as Transaction).partialSign(keypair)
      return tx
    },
  }
}

export interface RwaDexClientConfig {
  /** Which cluster's public RPC to use. Default: `localnet`. */
  cluster?: ClusterName
  /** An explicit connection (custom RPC, Surfpool, a test double). Wins over `cluster`. */
  connection?: Connection
  wallet?: WalletLike | null
  commitment?: Commitment
  /** Confirm each send before resolving. Default: true. */
  confirm?: boolean
}

/** Program IDs and every PDA helper, reachable as `dex.pdas`. */
export type Pdas = typeof pdas

export class RwaDexClient implements SdkContext {
  readonly connection: Connection
  readonly commitment: Commitment
  readonly pdas: Pdas = pdas
  readonly accounts: Accounts

  wallet: WalletLike | null
  /** Whether `send` awaits on-chain confirmation. */
  confirmTransactions: boolean

  readonly clmm: ClmmModule
  readonly dlmm: DlmmModule
  readonly maverick: MaverickModule
  readonly twamm: TwammModule
  readonly orders: OrdersModule
  readonly settlement: SettlementModule
  readonly kyc: KycModule
  readonly rwa: RwaModule
  readonly hooks: HooksModule
  readonly oracle: OracleModule

  constructor(config: RwaDexClientConfig = {}) {
    this.connection = config.connection ?? connectionFor(config.cluster ?? 'localnet')
    this.commitment = config.commitment ?? 'confirmed'
    this.wallet = config.wallet ?? null
    this.confirmTransactions = config.confirm ?? true
    this.accounts = accountsFor(this.connection)

    this.clmm = createClmmModule(this)
    this.dlmm = createDlmmModule(this)
    this.maverick = createMaverickModule(this)
    this.twamm = createTwammModule(this)
    this.orders = createOrdersModule(this)
    this.settlement = createSettlementModule(this)
    this.kyc = createKycModule(this)
    this.rwa = createRwaModule(this)
    this.hooks = createHooksModule(this)
    this.oracle = createOracleModule(this)
  }

  /** The connected wallet's key, or null when no wallet is set. */
  get publicKey(): PublicKey | null {
    return this.wallet?.publicKey ?? null
  }

  setWallet(wallet: WalletLike | null): this {
    this.wallet = wallet
    return this
  }

  requireWallet(): WalletLike {
    return requireWallet(this.wallet)
  }

  /**
   * Signs (with the wallet plus any extra signers), sends and — unless
   * `confirm: false` — waits for confirmation. An on-chain failure throws, so
   * a resolved promise always means the transaction landed.
   */
  async send(
    instructions: TransactionInstruction | TransactionInstruction[],
    options: TxOptions = {},
  ): Promise<TxResult> {
    const wallet = this.requireWallet()
    const list = instructionList(instructions)
    if (list.length === 0) throw new Error('no instructions to send')

    const sent = await sendWithSignersDetailed(this.connection, wallet, list, options.signers ?? [], {
      skipPreflight: options.skipPreflight,
      preflightCommitment: options.preflightCommitment ?? this.commitment,
    })

    if (options.confirm ?? this.confirmTransactions) {
      await confirmTx(this.connection, sent.signature, sent.blockhash, sent.lastValidBlockHeight)
    }
    return sent
  }

  /**
   * Runs the instructions without sending them, returning the program logs —
   * the fastest way to debug a failing swap or a rejected hook transfer.
   * No wallet signature is requested: simulation does not verify signatures.
   */
  async simulate(
    instructions: TransactionInstruction | TransactionInstruction[],
    options: SimulateOptions = {},
  ): Promise<SimulateResult> {
    const list = instructionList(instructions)
    if (list.length === 0) throw new Error('no instructions to simulate')
    const feePayer = options.feePayer ?? this.wallet?.publicKey
    if (!feePayer) {
      throw new Error('simulate needs a fee payer: connect a wallet or pass { feePayer }')
    }

    const transaction = new Transaction()
    // Same headroom `send` gives the real transaction: without it the account-heavy
    // AMM walks exceed the default budget and a healthy instruction looks broken.
    transaction.add(ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }))
    for (const instruction of list) transaction.add(instruction)
    transaction.feePayer = feePayer
    transaction.recentBlockhash = (
      await this.connection.getLatestBlockhash(this.commitment)
    ).blockhash
    for (const signer of options.signers ?? []) transaction.partialSign(signer)

    const result = await this.connection.simulateTransaction(transaction)
    return {
      err: result.value.err,
      logs: result.value.logs,
      unitsConsumed: result.value.unitsConsumed,
    }
  }
}

/** Convenience factory, `new RwaDexClient(config)` under a shorter name. */
export function createClient(config: RwaDexClientConfig = {}): RwaDexClient {
  return new RwaDexClient(config)
}
