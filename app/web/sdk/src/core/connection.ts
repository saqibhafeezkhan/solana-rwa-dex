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
import { TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, TRANSFER_HOOK_PROGRAM_ID } from './pdas'
import { mintTransferFee, mintTransferHookProgram, type MintTransferFee } from './decoders'
import { hookTransferAccounts } from './instructions-rails'

/** An instruction account entry, as the builders take it. */
export type AccountKey = { pubkey: PublicKey; isSigner: boolean; isWritable: boolean }

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
  on?(event: 'accountChanged', handler: (publicKey: PublicKey | { toString(): string } | null) => void): void
  on?(event: 'disconnect', handler: () => void): void
  off?(event: 'accountChanged', handler: (publicKey: PublicKey | { toString(): string } | null) => void): void
  off?(event: 'disconnect', handler: () => void): void
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

/** A sent transaction plus the blockhash it was built against. */
export interface SentTransaction {
  signature: TransactionSignature
  blockhash: string
  lastValidBlockHeight: number
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
  return (await sendWithSignersDetailed(connection, wallet, instructions, extraSigners, options)).signature
}

/**
 * Sends like `sendWithSigners` but also returns the blockhash the transaction
 * was built against, so the caller can confirm it with the same one. Fetching
 * a *fresh* blockhash to confirm is wrong: the RPC then watches a blockhash
 * the transaction never used, and the wait can outlive the real validity
 * window or fail outright.
 */
export async function sendWithSignersDetailed(
  connection: Connection,
  wallet: WalletLike,
  instructions: TransactionInstruction[],
  extraSigners: Keypair[],
  options?: SendOptions,
): Promise<SentTransaction> {
  if (instructions.length === 0) throw new Error('no instructions to send')
  const { blockhash, lastValidBlockHeight } = await connection.getLatestBlockhash('confirmed')

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
  const signature = await connection.sendRawTransaction(raw, {
    skipPreflight: options?.skipPreflight ?? false,
    preflightCommitment: options?.preflightCommitment ?? 'confirmed',
  })
  return { signature, blockhash, lastValidBlockHeight }
}

/**
 * Waits for a signature. Pass the blockhash the transaction was built with;
 * without it the latest blockhash is used as a best-effort fallback.
 */
export async function confirmTx(
  connection: Connection,
  signature: TransactionSignature,
  blockhash?: string,
  lastValidBlockHeight?: number,
) {
  let hash = blockhash
  let height = lastValidBlockHeight
  if (!hash || height === undefined) {
    const latest = await connection.getLatestBlockhash('confirmed')
    hash = latest.blockhash
    height = latest.lastValidBlockHeight
  }
  const result = await connection.confirmTransaction(
    { signature, blockhash: hash, lastValidBlockHeight: height },
    'confirmed',
  )
  // `confirmTransaction` resolves once the signature is processed even when
  // the transaction itself failed on chain, so its `err` has to be surfaced
  // here: otherwise a reverted swap or a rejected hook transfer is reported as
  // "confirmed" and the caller refreshes as if it had landed.
  if (result.value.err) {
    throw new Error(`transaction ${signature} failed: ${JSON.stringify(result.value.err)}`)
  }
  return result
}

/**
 * The token program that owns a mint: legacy SPL Token unless the mint is a
 * Token-2022 account. Every ATA and every token-program account passed to an
 * AMM instruction must agree with this, or the derived addresses are wrong and
 * the transfer CPI fails.
 */
export async function tokenProgramForMint(
  connection: Connection,
  mint: PublicKey,
): Promise<PublicKey> {
  const info = await connection.getAccountInfo(mint)
  return info && info.owner.equals(TOKEN_2022_PROGRAM_ID) ? TOKEN_2022_PROGRAM_ID : TOKEN_PROGRAM_ID
}

/**
 * The transfer fee a mint charges, or null when it has none.
 *
 * The token program picks between a mint's older and newer fee by the clock's
 * epoch, so the current one is read from the chain rather than assumed. Every
 * engine debits or credits this fee differently — `clmm` adds it on top of its
 * input so the vault nets the priced amount, `dlmm`/`maverick`/`twamm` price on
 * what actually arrives — which is why the UI only shows the rate and leaves the
 * arithmetic to the program.
 */
export async function transferFeeForMint(
  connection: Connection,
  mint: PublicKey,
): Promise<MintTransferFee | null> {
  const info = await connection.getAccountInfo(mint)
  if (!info || !info.owner.equals(TOKEN_2022_PROGRAM_ID)) return null
  const { epoch } = await connection.getEpochInfo()
  return mintTransferFee(info.data, BigInt(epoch))
}

/**
 * The accounts a transfer of `mint` has to carry, or undefined when it needs
 * none.
 *
 * Token-2022 resolves a hook-gated mint's `ExtraAccountMetaList` against the
 * transfer instruction's own account list, so every instruction that moves such
 * a mint — a pool deposit, a swap leg, a fee payout — needs its hook accounts
 * appended as remaining accounts. The hook checks the **source and destination
 * owners**, which is why they have to be the wallets (or PDAs) actually sending
 * and receiving: for a pool, one side is the pool PDA that owns the vaults, and
 * that PDA needs a whitelist entry of its own before the pool can trade a
 * hook-gated mint at all.
 *
 * A hook program other than this project's whitelist hook is refused rather
 * than guessed at: the meta list belongs to that program, and a wrong account
 * list is a failed transfer, not a silent one.
 */
export async function hookAccountsForMint(
  connection: Connection,
  mint: PublicKey,
  sourceOwner: PublicKey,
  destinationOwner: PublicKey,
): Promise<AccountKey[] | undefined> {
  const info = await connection.getAccountInfo(mint)
  if (!info || !info.owner.equals(TOKEN_2022_PROGRAM_ID)) return undefined
  const hookProgram = mintTransferHookProgram(info.data)
  if (!hookProgram) return undefined
  if (!hookProgram.equals(TRANSFER_HOOK_PROGRAM_ID)) {
    throw new Error(
      `mint ${mint.toBase58()} runs the transfer hook ${hookProgram.toBase58()}, ` +
        'which this client cannot resolve; pass its hook accounts explicitly',
    )
  }
  return hookTransferAccounts(mint, sourceOwner, destinationOwner)
}

/**
 * The union of the hook accounts every `senders` → `receivers` pairing needs,
 * for a whole instruction.
 *
 * An AMM instruction moves several mints at once (a deposit moves both legs, a
 * swap moves one in and the other out), and Token-2022 resolves each transfer's
 * own meta list against the accounts the instruction carries. So the accounts
 * for every leg in play are concatenated here and deduplicated by address —
 * extras are ignored by the token program, missing ones fail the transfer.
 */
export async function hookAccountsForTransfers(
  connection: Connection,
  mints: PublicKey[],
  senders: PublicKey[],
  receivers: PublicKey[],
): Promise<AccountKey[] | undefined> {
  const pairs = senders.flatMap((sender) => receivers.map((receiver) => ({ sender, receiver })))
  const lists = await Promise.all(
    mints.flatMap((mint) =>
      pairs.map(({ sender, receiver }) => hookAccountsForMint(connection, mint, sender, receiver)),
    ),
  )

  const seen = new Set<string>()
  const merged: AccountKey[] = []
  for (const list of lists) {
    for (const key of list ?? []) {
      const id = key.pubkey.toBase58()
      if (seen.has(id)) continue
      seen.add(id)
      merged.push(key)
    }
  }
  return merged.length > 0 ? merged : undefined
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
