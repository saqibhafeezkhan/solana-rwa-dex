/**
 * Transaction plumbing shared by every page: an RPC connection bound to the
 * selected cluster, wallet signing, ATA creation, and error decoding.
 */
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  ComputeBudgetProgram,
  type Cluster,
  type SendOptions,
  type TransactionSignature,
} from '@solana/web3.js'

export const RPC_ENDPOINTS: Record<Exclude<Cluster, 'devnet'> | 'devnet', string> = {
  'mainnet-beta': 'https://api.mainnet-beta.solana.com',
  testnet: 'https://api.testnet.solana.com',
  devnet: 'https://api.devnet.solana.com',
}

export type ClusterName = 'localnet' | 'devnet' | 'testnet' | 'mainnet-beta'

export function connectionFor(cluster: ClusterName): Connection {
  if (cluster === 'localnet') return new Connection('http://127.0.0.1:8899', 'confirmed')
  return new Connection(RPC_ENDPOINTS[cluster], 'confirmed')
}

/** The browser wallet (Phantom, Solflare, Backpack) and a local test keypair. */
export interface WalletLike {
  publicKey: PublicKey
  signTransaction<T extends Transaction | { serialize(): Uint8Array }>(tx: T): Promise<T>
}

/** Minimal Phantom-style injected provider. */
export interface InjectedProvider {
  isPhantom?: boolean
  publicKey?: PublicKey | { toString(): string } | null
  connect(): Promise<{ publicKey: PublicKey }>
  disconnect(): Promise<void>
  signAndSendTransaction?(tx: Transaction): Promise<{ signature: TransactionSignature }>
  signTransaction?(tx: Transaction): Promise<Transaction>
}

export function getInjectedWallet(): InjectedProvider | undefined {
  const w = window as unknown as { phantom?: { solana?: InjectedProvider }; solana?: InjectedProvider }
  return w.phantom?.solana ?? w.solana
}

/**
 * Anchor marks an optional account `None` when the account key equals the
 * executing program's own ID. Instructions that must leave a rail off pass
 * that program's ID as the sentinel.
 */
export function noneOf(programId: PublicKey): PublicKey {
  return programId
}

/**
 * Creates the transaction with the blockhash fetched fresh, signs with the
 * wallet, and sends. Returns the signature.
 */
export async function signAndSend(
  connection: Connection,
  wallet: WalletLike,
  instructions: TransactionInstruction[],
  options?: SendOptions,
): Promise<TransactionSignature> {
  return sendWithSigners(connection, wallet, instructions, [], options)
}

/**
 * The same, with extra keypairs that must co-sign (e.g. a freshly generated
 * mint keypair in `create_rwa_mint`, which the System Program requires as a
 * signer to create the account). The extra signers sign first, partially,
 * and the wallet's signature is appended on top.
 */
export async function sendWithSigners(
  connection: Connection,
  wallet: WalletLike,
  instructions: TransactionInstruction[],
  extraSigners: Keypair[],
  options?: SendOptions,
): Promise<TransactionSignature> {
  if (instructions.length === 0) throw new Error('no instructions to send')
  const { blockhash } = await connection.getLatestBlockhash('confirmed')

  const transaction = new Transaction()
  // AMM walks (ticks, bins, order lists) are account-heavy; buy some headroom.
  transaction.add(ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }))
  for (const instruction of instructions) transaction.add(instruction)
  transaction.recentBlockhash = blockhash
  transaction.feePayer = wallet.publicKey

  // Partial-sign with the extra keypairs before the wallet signs, so the
  // wallet provider only ever sees a transaction it can top up.
  for (const signer of extraSigners) {
    transaction.partialSign(signer)
  }

  const signed = await wallet.signTransaction(transaction)
  const raw = new Uint8Array(signed.serialize())
  return connection.sendRawTransaction(raw, {
    skipPreflight: options?.skipPreflight ?? false,
    preflightCommitment: options?.preflightCommitment ?? 'confirmed',
  })
}

export async function confirmTx(connection: Connection, signature: TransactionSignature) {
  const latest = await connection.getLatestBlockhash('confirmed')
  return connection.confirmTransaction(
    { signature, blockhash: latest.blockhash, lastValidBlockHeight: latest.lastValidBlockHeight },
    'confirmed',
  )
}

/** Creates an associated token account if it does not exist yet. */
export async function ensureAtaInstruction(
  connection: Connection,
  owner: PublicKey,
  mint: PublicKey,
  payer: PublicKey,
  tokenProgramId: PublicKey,
): Promise<TransactionInstruction | undefined> {
  const [ataAddress] = PublicKey.findProgramAddressSync(
    [owner.toBytes(), tokenProgramId.toBytes(), mint.toBytes()],
    new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'),
  )
  const account = await connection.getAccountInfo(ataAddress)
  if (account) return undefined
  return createAtaInstruction(payer, owner, mint, tokenProgramId)
}

/**
 * The associated-token-program `Create` instruction (empty data). Account order
 * is fixed by the ATA program: payer, the new account itself, its owner, the
 * mint, then the system and token programs. The new account is derived from the
 * owner here rather than passed in, so it can never drift from the PDA the rest
 * of the app addresses.
 */
export function createAtaInstruction(
  payer: PublicKey,
  owner: PublicKey,
  mint: PublicKey,
  tokenProgramId: PublicKey,
): TransactionInstruction {
  const associatedTokenAddress = PublicKey.findProgramAddressSync(
    [owner.toBytes(), tokenProgramId.toBytes(), mint.toBytes()],
    new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'),
  )[0]
  return new TransactionInstruction({
    programId: new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'),
    keys: [
      { pubkey: payer, isSigner: true, isWritable: true },
      { pubkey: associatedTokenAddress, isSigner: false, isWritable: true },
      { pubkey: owner, isSigner: false, isWritable: false },
      { pubkey: mint, isSigner: false, isWritable: false },
      { pubkey: SystemProgram.programId, isSigner: false, isWritable: false },
      { pubkey: tokenProgramId, isSigner: false, isWritable: false },
    ],
    data: new Uint8Array(),
  })
}

/**
 * Anchor error text from a raw RPC error. Anchor embeds the custom error code
 * in hex (0x1770+) inside the message; the programs' own codes start at 6000.
 */
export function describeError(error: unknown): string {
  if (!(error instanceof Error)) return String(error)
  const message = error.message
  const hexMatch = message.match(/custom program error: (0x[0-9a-fA-F]+)/)
  if (hexMatch) {
    const code = parseInt(hexMatch[1], 16)
    return `${message.split('.')[0]} (program error ${code})`
  }
  return message.length > 400 ? `${message.slice(0, 400)}…` : message
}

/**
 * A localhost dev keypair for localnet testing. The secret is intended for
 * development only and is generated per browser profile, not imported.
 */
export function loadLocalKeypair(secretJson: string): Keypair {
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(secretJson) as number[]))
}

export function lamportsToSol(lamports: number | bigint): string {
  return (Number(lamports) / 1_000_000_000).toFixed(4)
}

export function shortAddress(address: PublicKey | string): string {
  const text = typeof address === 'string' ? address : address.toBase58()
  return `${text.slice(0, 4)}…${text.slice(-4)}`
}

export async function assertAccountExists(connection: Connection, address: PublicKey, label: string) {
  const info = await connection.getAccountInfo(address)
  if (!info) throw new Error(`${label} ${address.toBase58()} not found on ${connection.rpcEndpoint}`)
  return info
}
