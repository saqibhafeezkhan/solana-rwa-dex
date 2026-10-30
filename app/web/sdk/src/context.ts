/**
 * The context every program module binds to, plus the transaction options the
 * `send` helpers accept.
 *
 * A module never touches the network directly: it builds an instruction with
 * the package's field-exact encoder (`./core/instructions-*.ts`) and hands it to
 * `ctx.send`, which owns signing, sending and confirmation. That keeps the SDK
 * usable from a browser wallet, a Bun script or a test with the same call.
 */
import type {
  Commitment,
  Connection,
  Keypair,
  PublicKey,
  TransactionInstruction,
  TransactionSignature,
} from '@solana/web3.js'
import type { WalletLike } from './core/connection'

export interface TxOptions {
  /** Extra keypairs that must co-sign (a fresh mint, an escrow keypair, ...). */
  signers?: Keypair[]
  skipPreflight?: boolean
  preflightCommitment?: Commitment
  /** Wait for confirmation before resolving. Defaults to the client setting. */
  confirm?: boolean
}

/** What a send resolves to once the transaction landed (or was submitted). */
export interface TxResult {
  signature: TransactionSignature
  /** The blockhash the transaction was built against, for re-confirmation. */
  blockhash: string
  lastValidBlockHeight: number
}

export interface SimulateOptions {
  signers?: Keypair[]
  /** Defaults to the connected wallet's key. */
  feePayer?: PublicKey
}

export interface SimulateResult {
  err: unknown
  logs: string[] | null
  unitsConsumed: number | undefined
}

export interface SdkContext {
  readonly connection: Connection
  readonly wallet: WalletLike | null
  readonly commitment: Commitment
  send(
    instructions: TransactionInstruction | TransactionInstruction[],
    options?: TxOptions,
  ): Promise<TxResult>
  simulate(
    instructions: TransactionInstruction | TransactionInstruction[],
    options?: SimulateOptions,
  ): Promise<SimulateResult>
  requireWallet(): WalletLike
}

/** Throws a clear error when an action needs a signer and none is connected. */
export function requireWallet(wallet: WalletLike | null): WalletLike {
  if (!wallet) {
    throw new Error(
      'no wallet connected: pass `wallet` to new RwaDexClient({...}) or call client.setWallet(wallet)',
    )
  }
  return wallet
}

export function instructionList(
  instructions: TransactionInstruction | TransactionInstruction[],
): TransactionInstruction[] {
  return Array.isArray(instructions) ? instructions : [instructions]
}

/**
 * Awaits a builder and sends the single instruction it produced. Every module
 * action is one line of this, so the call shape stays uniform:
 *
 *   await dex.clmm.swap(args, { signers: [] })
 */
export async function sendBuilt(
  ctx: SdkContext,
  built: Promise<TransactionInstruction>,
  options?: TxOptions,
): Promise<TxResult> {
  return ctx.send([await built], options)
}
