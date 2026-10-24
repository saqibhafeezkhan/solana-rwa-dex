#!/usr/bin/env bun
/**
 * Seeds a local validator with the tokens, compliance rails and pools the web UI
 * reads, so every page can be exercised against real on-chain state.
 *
 *   bun run seed:localnet                        # seeds against 127.0.0.1:8899
 *   bun run seed:localnet -- --user <pubkey>     # also fund + KYC a browser wallet
 *   bun run seed:localnet -- --rails oracle,kyc  # wire the KYC rail into the pools
 *   bun run seed:localnet -- --no-verify         # skip the post-seed swap checks
 *
 * Everything is built with `app/web/src/core/*` — the same instruction builders,
 * PDA derivations and decoders the browser uses — so a successful run also proves
 * the client's encoding matches the deployed programs. The run is idempotent:
 * existing accounts are detected and skipped, and the token mints are remembered
 * in `.anchor/seed-localnet.json` so re-running never strands the balances the UI
 * is pointed at.
 *
 * The programs themselves are deployed by `scripts/bootstrap-localnet.sh`, which
 * runs that deploy script and then this one. The one-time initializers (kyc_gate,
 * transfer_hook, settlement) require the program's *upgrade authority*, so this
 * script's keypair must be the keypair those programs were deployed with.
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'
import {
  ComputeBudgetProgram,
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  type TransactionSignature,
} from '@solana/web3.js'
import bs58 from 'bs58'
import { secp256k1 } from '@noble/curves/secp256k1'
import { keccak_256 } from '@noble/hashes/sha3'
import {
  createAtaInstruction,
  describeError,
  sendWithSigners,
  type ClusterName,
  type WalletLike,
} from '../src/core/connection'
import {
  CLMM_PROGRAM_ID,
  DLMM_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  MAVERICK_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  ORDER_BOOK_PROGRAM_ID,
  RWA_MINT_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  TRANSFER_HOOK_PROGRAM_ID,
  TWAMM_PROGRAM_ID,
  ata,
  clmmPool,
  clmmPosition,
  clmmTick,
  dlmmBinArray,
  dlmmPool,
  dlmmPosition,
  hookAuthority,
  kycConfig,
  maverickBinArray,
  maverickPool,
  orderBook,
  orderBookOrder,
  maverickPosition,
  oracleState,
  settlementConfig,
  settlementEscrowAuthority,
  settlementEscrowToken,
  settlementVault,
  sqrtPriceX64FromPrice,
  transferHookConfig,
  transferHookExtraAccountMetaList,
  transferHookWhitelistEntry,
  twammLpPosition,
  twammOrder,
  twammPool,
} from '../src/core/pdas'
import {
  clmmAddLiquidity,
  clmmCollectFees,
  clmmInitializePool,
  clmmRemoveLiquidity,
  clmmSetAuthority,
  clmmSetPaused,
  clmmSwap,
  clmmTripBreaker,
} from '../src/core/instructions-clmm'
import {
  dlmmAddLiquidity,
  dlmmInitializeBinArray,
  dlmmInitializePool,
  dlmmRemoveLiquidity,
  dlmmSetAuthority,
  dlmmSetPaused,
  dlmmSwap,
  dlmmTripBreaker,
} from '../src/core/instructions-dlmm'
import {
  maverickAddLiquidity,
  maverickInitializeBinArray,
  maverickInitializePool,
  maverickMoveBins,
  maverickRemoveLiquidity,
  maverickSetAuthority,
  maverickSetPaused,
  maverickSwap,
  maverickTripBreaker,
} from '../src/core/instructions-maverick'
import {
  orderBookCreateOrder,
  orderBookInitialize,
} from '../src/core/instructions-orderbook'
import {
  twammAddLiquidity,
  twammCancelOrder,
  twammCreateLongTermOrder,
  twammExecuteVirtualOrders,
  twammInitializePool,
  twammRemoveLiquidity,
  twammSetAuthority,
  twammSetPaused,
  twammWithdrawProceeds,
} from '../src/core/instructions-twamm'
import {
  hooksFundHookAuthority,
  kycInitializeMint,
  kycIssue,
  kycRevoke,
  kycSetAuthority,
  kycVerify,
  oracleUnpausePool,
  rwaCreateMint,
  rwaMintTo,
  rwaTransfer,
  rwaUpdateTransferFee,
  settlementCancel,
  settlementInitialize,
  settlementInitiate,
  settlementSettle,
  settlementSetHooksAuthority,
  settlementSetTransferAgent,
  transferHookInitExtraAccountMetaList,
  transferHookInitialize,
  transferHookSetAuthority,
  transferHookSetPaused,
  transferHookSetWhitelistEntry,
  transferHookUpdateRoot,
} from '../src/core/instructions-rails'
import {
  decodeBinPosition,
  decodeClmmPool,
  decodeClmmPosition,
  decodeDlmmPool,
  decodeKycConfig,
  decodeMaverickBinArray,
  decodeMaverickPool,
  decodeMint,
  decodeOracleState,
  decodeSettlementConfig,
  decodeSettlementVault,
  decodeTwammLp,
  decodeTwammOrder,
  decodeTwammPool,
  loadTwammPool,
  type AccountRow,
} from '../src/core/decoders'

// -----------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------

const REPO_ROOT = resolve(import.meta.dir, '../../..')
const STATE_FILE = join(REPO_ROOT, '.anchor', 'seed-localnet.json')

/** The eleven programs the UI talks to, matching `scripts/programs.sh`. */
const PROGRAMS: [string, PublicKey][] = [
  ['oracle', ORACLE_PROGRAM_ID],
  ['kyc_gate', KYC_PROGRAM_ID],
  ['transfer_hook', TRANSFER_HOOK_PROGRAM_ID],
  ['rwa_mint', RWA_MINT_PROGRAM_ID],
  ['settlement', SETTLEMENT_PROGRAM_ID],
  ['hooks', HOOKS_PROGRAM_ID],
  ['order_book', ORDER_BOOK_PROGRAM_ID],
  ['clmm', CLMM_PROGRAM_ID],
  ['dlmm', DLMM_PROGRAM_ID],
  ['maverick', MAVERICK_PROGRAM_ID],
  ['twamm', TWAMM_PROGRAM_ID],
]

/** Six decimals on both legs, like USDC and a tokenised equity. */
const DECIMALS = 6
const ONE_TOKEN = 10n ** BigInt(DECIMALS)
/** 1,000,000.000000 of each mint, split between the authority and the demo wallet. */
const SEED_SUPPLY = 1_000_000n * ONE_TOKEN
/** 1,000.000000 of each leg into the pools. */
const POOL_LEG = 1_000n * ONE_TOKEN
const LAMPORTS_PER_SOL = 1_000_000_000
const SOL_TARGET = 10

const CLMM_TICK_SPACING = 64
const CLMM_FEE_PIPS = 3_000
/** ±64% around 1.0 at spacing 64, so the tick bitmap stays small. */
const CLMM_LOWER_TICK = -6_400
const CLMM_UPPER_TICK = 6_400
/** A second, narrow band the functionality suite opens (fees, remove, walk). */
const CLMM_NARROW_LOWER_TICK = -128
const CLMM_NARROW_UPPER_TICK = 128
/** Deep enough that a demo swap barely moves the price. */
const CLMM_LIQUIDITY = 100_000_000_000n

const DLMM_BIN_STEP = 10
const DLMM_BASE_FEE_BPS = 20
const MAVERICK_BIN_STEP = 5
const MAVERICK_BASE_FEE_BPS = 10

const ORACLE_MAX_DEVIATION_BPS = 1_000 // 10%
const SETTLEMENT_HORIZON_SLOTS = 300n

/**
 * The secp256k1 generator point, used as the demo Transfer Agent key. Its
 * private key is `1`, so a localnet client can produce the settle signature
 * without any key management. Never use this outside a local validator.
 */
const DEMO_AGENT_PUBKEY = Buffer.from(
  '79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798' +
    '483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8',
  'hex',
)

type Rail = 'oracle' | 'kyc' | 'settlement'

// -----------------------------------------------------------------------
// CLI
// -----------------------------------------------------------------------

interface Options {
  url: string
  cluster: ClusterName
  payerPath: string
  user?: string
  rails: Set<Rail>
  verify: boolean
  json: boolean
}

function parseArgs(argv: string[]): Options {
  const options: Options = {
    url: process.env.ANCHOR_PROVIDER_URL ?? 'http://127.0.0.1:8899',
    cluster: 'localnet',
    payerPath: process.env.ANCHOR_WALLET ?? join(homedir(), '.config/solana/id.json'),
    rails: new Set<Rail>(['oracle']),
    verify: true,
    json: false,
  }

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]
    const value = () => {
      const next = argv[++i]
      if (!next) die(`missing value for ${arg}`)
      return next
    }
    switch (arg) {
      case '--url':
        options.url = value()
        break
      case '--cluster':
        options.cluster = value() as ClusterName
        break
      case '--keypair':
      case '--payer':
        options.payerPath = value()
        break
      case '--user':
        options.user = value()
        break
      case '--rails': {
        options.rails = new Set<Rail>()
        for (const rail of value()
          .split(',')
          .map((entry) => entry.trim())
          .filter(Boolean)) {
          if (rail !== 'oracle' && rail !== 'kyc' && rail !== 'settlement') {
            die(`unknown rail: ${rail} (expected oracle, kyc or settlement)`)
          }
          options.rails.add(rail)
        }
        break
      }
      case '--no-verify':
        options.verify = false
        break
      case '--json':
        options.json = true
        break
      case '-h':
      case '--help':
        usage()
        process.exit(0)
      default:
        die(arg.startsWith('--') ? `unknown flag: ${arg} (try --help)` : `unexpected argument: ${arg}`)
    }
  }
  return options
}

function usage() {
  process.stdout.write(`Usage: bun run seed:localnet [flags]

Flags:
  --url <rpc>              Cluster RPC endpoint (default: $ANCHOR_PROVIDER_URL or
                           http://127.0.0.1:8899)
  --cluster <name>         Label for the manifest (default: localnet)
  --keypair, --payer <p>   Payer and authority. Must be the upgrade authority of
                           every program (default: ~/.config/solana/id.json)
  --user <pubkey>          Browser wallet to fund, mint to and KYC-approve. The
                           pools and the settlement queue are addressed to it, so
                           connect the UI with this wallet (default: the payer)
  --rails <a,b>            Rails wired into every pool: oracle, kyc, settlement
                           (default: oracle)
  --no-verify              Skip the post-seed decodes and swap checks
  --json                   Print the manifest as JSON instead of a table
  -h, --help               This message
`)
}

function log(message: string) {
  process.stdout.write(`\x1b[1;34m==>\x1b[0m ${message}\n`)
}

function step(message: string) {
  process.stdout.write(`  ${message}\n`)
}

function die(message: string): never {
  process.stderr.write(`\x1b[1;31merror:\x1b[0m ${message}\n`)
  process.exit(1)
}

const sleep = (ms: number) => new Promise((done) => setTimeout(done, ms))

// -----------------------------------------------------------------------
// SPL token instructions, hand-encoded
//
// `@solana/spl-token` is not a dependency of this app (the browser never builds
// token instructions — it calls the programs, which CPI into the token program),
// so the handful this seeder needs are written out rather than adding a package
// for a dev-only script.
// -----------------------------------------------------------------------

const u64le = (value: bigint) => {
  const bytes = Buffer.alloc(8)
  bytes.writeBigUInt64LE(value)
  return bytes
}

/** Token program tag 20 (`InitializeMint2`): no rent sysvar account needed. */
function initializeMint2Instruction(
  mint: PublicKey,
  decimals: number,
  mintAuthority: PublicKey,
  tokenProgramId: PublicKey,
): TransactionInstruction {
  return new TransactionInstruction({
    programId: tokenProgramId,
    keys: [{ pubkey: mint, isSigner: false, isWritable: true }],
    data: Buffer.concat([
      Buffer.from([20, decimals]),
      mintAuthority.toBuffer(),
      Buffer.from([0, 0, 0, 0]), // freeze authority: COption::None
    ]),
  })
}

/** Token program tag 7 (`MintTo`). */
function mintToInstruction(
  mint: PublicKey,
  destination: PublicKey,
  authorityKey: PublicKey,
  amount: bigint,
  tokenProgramId: PublicKey,
): TransactionInstruction {
  return new TransactionInstruction({
    programId: tokenProgramId,
    keys: [
      { pubkey: mint, isSigner: false, isWritable: true },
      { pubkey: destination, isSigner: false, isWritable: true },
      { pubkey: authorityKey, isSigner: true, isWritable: false },
    ],
    data: Buffer.concat([Buffer.from([7]), u64le(amount)]),
  })
}

/** A classic 82-byte SPL mint, in one transaction. */
function createClassicMintInstructions(payer: PublicKey, mint: PublicKey, owner: PublicKey) {
  return [
    SystemProgram.createAccount({
      fromPubkey: payer,
      newAccountPubkey: mint,
      space: 82,
      lamports: 1_461_600, // rent floor for 82 bytes
      programId: TOKEN_PROGRAM_ID,
    }),
    initializeMint2Instruction(mint, DECIMALS, owner, TOKEN_PROGRAM_ID),
  ]
}

// -----------------------------------------------------------------------
// Merkle whitelist (mirrors transfer_hook::root_from_proof)
//
// leaf = keccak(0x00 || owner), node = keccak(0x01 || min || max). The hook folds
// each proof with its own hash, so deriving the root by folding keeps the client
// and the program provably in step. The hash must be Keccak-256 (NOT SHA-256):
// `transfer_hook` folds proofs with `solana_keccak_hasher`, and a SHA-256 tree
// produces a root its `execute` rejects with `SourceNotWhitelisted`.
// -----------------------------------------------------------------------

const LEAF_PREFIX = new Uint8Array([0])
const NODE_PREFIX = new Uint8Array([1])

/** keccak256(part₀ ‖ part₁ ‖ …) — the same fold `root_from_proof` applies. */
function keccakParts(parts: Uint8Array[]): Uint8Array {
  const total = parts.reduce((sum, part) => sum + part.length, 0)
  const buffer = new Uint8Array(total)
  let offset = 0
  for (const part of parts) {
    buffer.set(part, offset)
    offset += part.length
  }
  return keccak_256(buffer)
}

const leafHash = (owner: PublicKey) => keccakParts([LEAF_PREFIX, owner.toBytes()])

async function nodeHash(left: Uint8Array, right: Uint8Array): Promise<Uint8Array> {
  const [a, b] = Buffer.compare(Buffer.from(left), Buffer.from(right)) <= 0 ? [left, right] : [right, left]
  return keccakParts([NODE_PREFIX, a, b])
}

/**
 * Pads a level to an even width by duplicating its last node, so an odd level
 * hashes its last node with itself. Applied at **every** level, not just the
 * leaves: an odd *intermediate* level (5 leaves -> 3 parents, 13 -> 7) would
 * otherwise read past the array and fold `undefined`.
 */
function pad(level: Uint8Array[]): Uint8Array[] {
  return level.length % 2 === 1 ? [...level, level[level.length - 1]] : [...level]
}

async function merkleRoot(leaves: Uint8Array[]): Promise<Uint8Array> {
  if (leaves.length === 0) throw new Error('a whitelist needs at least one member')
  let level = [...leaves]
  while (level.length > 1) {
    const padded = pad(level)
    const next: Uint8Array[] = []
    for (let i = 0; i < padded.length; i += 2) next.push(await nodeHash(padded[i], padded[i + 1]))
    level = next
  }
  return level[0]
}

/** The sibling path for one leaf of the same tree `merkleRoot` builds. */
async function merkleProof(leaves: Uint8Array[], index: number): Promise<Uint8Array[]> {
  let level = [...leaves]
  let cursor = index
  const proof: Uint8Array[] = []
  while (level.length > 1) {
    const padded = pad(level)
    proof.push(padded[cursor % 2 === 0 ? cursor + 1 : cursor - 1])
    const next: Uint8Array[] = []
    for (let i = 0; i < padded.length; i += 2) next.push(await nodeHash(padded[i], padded[i + 1]))
    level = next
    cursor = Math.floor(cursor / 2)
  }
  return proof
}

// -----------------------------------------------------------------------
// Chain plumbing
// -----------------------------------------------------------------------

let connection: Connection
let authority: Keypair

function localWallet(keypair: Keypair): WalletLike {
  return {
    publicKey: keypair.publicKey,
    signTransaction: async (tx: Transaction) => {
      tx.partialSign(keypair)
      return tx
    },
  } as WalletLike
}

const accountExists = async (address: PublicKey) => (await connection.getAccountInfo(address)) !== null

async function confirm(label: string, signature: TransactionSignature) {
  const deadline = Date.now() + 60_000
  while (Date.now() < deadline) {
    const { value } = await connection.getSignatureStatuses([signature], {
      searchTransactionHistory: true,
    })
    const status = value[0]
    if (status?.err) throw new Error(`${label}: transaction failed: ${JSON.stringify(status.err)}`)
    if (status?.confirmationStatus === 'confirmed' || status?.confirmationStatus === 'finalized') return
    await sleep(200)
  }
  throw new Error(`${label}: not confirmed within 60s (${signature})`)
}

/** Replays the instructions to surface the program logs behind a failure. */
async function logsFor(instructions: TransactionInstruction[], signers: Keypair[]): Promise<string[]> {
  try {
    const { blockhash } = await connection.getLatestBlockhash('confirmed')
    const tx = new Transaction().add(
      ComputeBudgetProgram.setComputeUnitLimit({ units: 1_400_000 }),
      ...instructions,
    )
    tx.recentBlockhash = blockhash
    tx.feePayer = authority.publicKey
    for (const signer of signers) tx.partialSign(signer)
    tx.partialSign(authority)
    const simulation = await connection.simulateTransaction(tx)
    return simulation.value.logs ?? []
  } catch {
    return []
  }
}

async function send(
  label: string,
  instructions: TransactionInstruction[],
  signers: Keypair[] = [],
): Promise<TransactionSignature> {
  try {
    const signature = await sendWithSigners(connection, localWallet(authority), instructions, signers)
    await confirm(label, signature)
    step(`${label}  ${signature.slice(0, 12)}…`)
    return signature
  } catch (cause) {
    // SEED_DEBUG=1 replays every program log instead of the error lines only -
    // the useful frame is often the one that did not say "Error".
    const verbose = process.env.SEED_DEBUG === '1'
    const logs = await logsFor(instructions, signers)
    const shown = verbose ? logs : logs.filter((line) => /Error|failed|panicked/.test(line))
    process.stderr.write(
      `\x1b[1;31m  ${label} failed:\x1b[0m ${describeError(cause)}\n` +
        shown.slice(-(verbose ? 80 : 6)).map((line) => `    ${line}\n`).join(''),
    )
    throw cause
  }
}

/**
 * Sends instructions that the program is expected to REFUSE, and returns the
 * program's own error. `send` prints every failure in red as if the suite had
 * broken, so the negative half of a guard (a revoked wallet must not pass the
 * KYC gate) is probed through the raw submitter instead.
 */
async function expectRejected(label: string, instructions: TransactionInstruction[]): Promise<string> {
  try {
    const signature = await sendWithSigners(connection, localWallet(authority), instructions, [])
    await confirm(label, signature)
  } catch (cause) {
    return describeError(cause)
  }
  throw new Error(`${label}: the program accepted what it should have refused`)
}

/** Skips the transaction when the account already exists, so runs are idempotent. */
async function sendUnlessExists(
  label: string,
  address: PublicKey,
  instructions: () => Promise<TransactionInstruction[]>,
  signers: Keypair[] = [],
): Promise<boolean> {
  if (await accountExists(address)) {
    step(`${label}  already initialized, skipped`)
    return false
  }
  await send(label, await instructions(), signers)
  return true
}

async function ensureAta(owner: PublicKey, mint: PublicKey, tokenProgramId: PublicKey) {
  const address = ata(owner, mint, tokenProgramId)
  if (await accountExists(address)) return address
  await send(`create ATA for ${mint.toBase58().slice(0, 6)}…`, [
    createAtaInstruction(authority.publicKey, owner, mint, tokenProgramId),
  ])
  return address
}

async function ensureSol(address: PublicKey, targetSol: number) {
  let balance = await connection.getBalance(address)
  let attempts = 0
  while (balance < targetSol * LAMPORTS_PER_SOL && attempts < 24) {
    attempts += 1
    try {
      const signature = await connection.requestAirdrop(address, LAMPORTS_PER_SOL)
      await confirm('airdrop', signature)
      balance += LAMPORTS_PER_SOL
    } catch {
      break
    }
  }
  return balance
}

async function readRow(address: PublicKey): Promise<AccountRow> {
  const info = await connection.getAccountInfo(address)
  if (!info) throw new Error(`account ${address.toBase58()} not found`)
  return { pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports }
}

const tokenBalance = (address: PublicKey) =>
  connection
    .getTokenAccountBalance(address)
    .then((response) => BigInt(response.value.amount))
    .catch(() => null)

// -----------------------------------------------------------------------
// Persistent state, so a re-run reuses the same mints
// -----------------------------------------------------------------------

interface SeedState {
  cluster?: string
  user?: string
  /**
   * A second whitelisted holder, so the transfer-hook path can be exercised
   * with two distinct owners (the payer and the demo wallet are often the same
   * key, and a self-transfer would not prove both Merkle proofs are checked).
   */
  counterparty?: string
  mints?: Record<string, { address: string; secret: string }>
}

function loadState(): SeedState {
  if (!existsSync(STATE_FILE)) return {}
  try {
    return JSON.parse(readFileSync(STATE_FILE, 'utf8')) as SeedState
  } catch {
    return {}
  }
}

/** Solana keypair files are either a JSON byte array or a base58 secret. */
function loadKeypair(path: string): Keypair {
  if (!existsSync(path)) {
    die(`keypair not found: ${path}\n  create one with: solana-keygen new -o ${path}`)
  }
  const raw = readFileSync(path, 'utf8').trim()
  const secret = raw.startsWith('[')
    ? Uint8Array.from(JSON.parse(raw) as number[])
    : bs58.decode(raw)
  return Keypair.fromSecretKey(secret)
}

/** The remembered mint keypair, or a fresh one when the ledger was reset. */
async function mintKeypair(state: SeedState, name: string): Promise<Keypair> {
  const remembered = state.mints?.[name]
  if (remembered) {
    const keypair = Keypair.fromSecretKey(bs58.decode(remembered.secret))
    if (await accountExists(keypair.publicKey)) return keypair
  }
  const fresh = Keypair.generate()
  state.mints = {
    ...(state.mints ?? {}),
    [name]: { address: fresh.publicKey.toBase58(), secret: bs58.encode(fresh.secretKey) },
  }
  return fresh
}

// -----------------------------------------------------------------------
// Seeding
// -----------------------------------------------------------------------

const created = {
  mints: {} as Record<string, string>,
  accounts: {} as Record<string, string>,
  pools: {} as Record<string, string>,
  whitelist: { root: '', proofs: {} as Record<string, string> },
  swaps: [] as { pool: string; amountIn: string; signature: string }[],
  exercises: [] as { name: string; status: 'ok' | 'fail' | 'skip'; detail: string }[],
}

interface Context {
  options: Options
  user: PublicKey
  counterparty: PublicKey
  state: SeedState
  mintA: PublicKey
  mintB: PublicKey
}

async function preflight(options: Options) {
  log(`Cluster ${options.url}`)
  try {
    await connection.getVersion()
  } catch (cause) {
    die(`cannot reach ${options.url} (${describeError(cause)})\n  start one with: ./scripts/bootstrap-localnet.sh`)
  }

  const missing: string[] = []
  for (const [name, programId] of PROGRAMS) {
    const info = await connection.getAccountInfo(programId)
    if (!info?.executable) missing.push(`${name} (${programId.toBase58()})`)
  }
  if (missing.length > 0) {
    die(
      `${missing.length} program(s) are not deployed on ${options.url}:\n` +
        missing.map((name) => `    ${name}\n`).join('') +
        '  deploy them with: ./scripts/bootstrap-localnet.sh',
    )
  }
  step(`${PROGRAMS.length} programs live`)
}

async function seedTokens(context: Context) {
  const { state } = context
  log('Tokens')
  for (const [name, symbol] of [
    ['musdc', 'mUSDC'],
    ['mtsla', 'mTSLA'],
  ] as const) {
    const keypair = await mintKeypair(state, name)
    if (await accountExists(keypair.publicKey)) {
      step(`${symbol} mint  ${keypair.publicKey.toBase58()}  (exists)`)
    } else {
      await send(`create ${symbol} mint`, createClassicMintInstructions(authority.publicKey, keypair.publicKey, authority.publicKey), [keypair])
    }
    if (name === 'musdc') context.mintA = keypair.publicKey
    else context.mintB = keypair.publicKey
    created.mints[name] = keypair.publicKey.toBase58()
  }

  const { mintA, mintB } = context
  const half = SEED_SUPPLY / 2n
  const holders: [string, PublicKey][] = [
    ['authority', authority.publicKey],
    ['wallet', context.user],
  ]
  for (const [label, owner] of holders) {
    if (label === 'wallet' && context.user.equals(authority.publicKey)) continue
    for (const [symbol, mint] of [
      ['mUSDC', mintA],
      ['mTSLA', mintB],
    ] as const) {
      const destination = await ensureAta(owner, mint, TOKEN_PROGRAM_ID)
      const balance = await tokenBalance(destination)
      if (balance !== null && balance >= half) {
        step(`${symbol} to ${label}  already funded`)
        continue
      }
      await send(`mint ${symbol} to ${label}`, [
        mintToInstruction(mint, destination, authority.publicKey, half, TOKEN_PROGRAM_ID),
      ])
    }
  }
}

async function seedKyc(context: Context) {
  log('KYC gate (soulbound Token-2022 attestations)')
  await sendUnlessExists('initialize KYC mint', kycConfig(), async () => [await kycInitializeMint(authority.publicKey)])

  const config = decodeKycConfig(await readRow(kycConfig()))
  created.accounts.kycConfig = kycConfig().toBase58()
  created.accounts.kycMint = config.kycMint.toBase58()

  for (const [label, owner] of [
    ['authority', authority.publicKey],
    ['wallet', context.user],
  ] as const) {
    if (label === 'wallet' && context.user.equals(authority.publicKey)) continue
    const destination = await ensureAta(owner, config.kycMint, TOKEN_2022_PROGRAM_ID)
    const balance = await tokenBalance(destination)
    if (balance !== null && balance > 0n) {
      step(`KYC attestation for ${label}  already issued`)
      continue
    }
    await send(`issue KYC to ${label}`, [await kycIssue(authority.publicKey, owner)])
  }
  created.accounts.kycAttestation = ata(authority.publicKey, config.kycMint, TOKEN_2022_PROGRAM_ID).toBase58()
}

async function seedTransferHook(whitelist: PublicKey[]) {
  log('Transfer-hook whitelist')
  await sendUnlessExists('initialize whitelist config', transferHookConfig(), async () => [
    await transferHookInitialize(authority.publicKey),
  ])
  created.accounts.transferHookConfig = transferHookConfig().toBase58()

  // The proofs are always recomputed and reported, even when the root is
  // already on chain: the Assets page asks for them as comma-separated 64-char
  // hex elements, so they are printed in exactly that format.
  //
  // Canonical leaf order: sort by leaf hash, the same rule the UI's
  // `buildWhitelistTree` uses. The hook folds whichever path it is handed, but
  // the UI rebuilds a tree from its member list and compares the root with the
  // published one, so one member set has to have exactly one root.
  const members = whitelist.map((wallet) => ({ wallet, leaf: leafHash(wallet) }))
  members.sort((a, b) => Buffer.compare(Buffer.from(a.leaf), Buffer.from(b.leaf)))
  const leaves = members.map((member) => member.leaf)
  const root = await merkleRoot(leaves)
  for (const [index, { wallet }] of members.entries()) {
    const proof = await merkleProof(leaves, index)
    const hex = proof.map((element) => Buffer.from(element).toString('hex')).join(',')
    created.whitelist.proofs[wallet.toBase58()] = hex
    step(`whitelist member ${wallet.toBase58()}`)
    step(`  proof: ${hex || '(single-member tree: empty proof)'}`)
  }
  created.whitelist.root = Buffer.from(root).toString('hex')

  const config = await readRow(transferHookConfig())
  const onChainRoot = config.data.subarray(8 + 32, 8 + 64)
  const rootChanged = !Buffer.from(onChainRoot).equals(Buffer.from(root))
  if (rootChanged) {
    await send('publish whitelist root', [await transferHookUpdateRoot(authority.publicKey, root)])
  } else {
    step('whitelist root already published')
  }

  // Publishing the root is not enough to make a wallet tradeable: the hook reads
  // each side's proof from that institution's own whitelist entry, because
  // Token-2022 gives it no way to receive proofs as instruction data. Each entry
  // is checked against the root above when it is written, so a stale tree fails
  // here instead of at the first transfer.
  //
  // A rotation leaves every entry in place but stale - the entry stores the path
  // it was written with - so a changed root means every entry is rewritten from
  // the new tree, including the ones that already exist. (`sendUnlessExists`
  // would skip exactly those and leave the whitelist unable to fold any proof.)
  for (const [index, { wallet }] of members.entries()) {
    const proof = await merkleProof(leaves, index)
    const label = `whitelist entry ${wallet.toBase58()}`
    const write = async () => [await transferHookSetWhitelistEntry(authority.publicKey, wallet, proof)]
    if (rootChanged) {
      await send(label, write())
    } else {
      await sendUnlessExists(label, transferHookWhitelistEntry(wallet), write)
    }
  }
}

async function seedRwaMint(context: Context) {
  const { state, user } = context
  log('RWA mint (Token-2022 + transfer hook + metadata + transfer fee)')
  const keypair = await mintKeypair(state, 'rwa')
  const address = keypair.publicKey
  created.mints.rwa = address.toBase58()

  await sendUnlessExists(
    'create RWA mint',
    address,
    async () => [
      await rwaCreateMint(address, authority.publicKey, {
        decimals: DECIMALS,
        transferHookProgram: TRANSFER_HOOK_PROGRAM_ID,
        delegate: authority.publicKey,
        feeAuthority: authority.publicKey,
        name: 'Tokenized Treasury Note',
        symbol: 'mTBILL',
        uri: 'https://example.com/rwa/mtbill.json',
      }),
    ],
    [keypair],
  )
  await sendUnlessExists(
    'register extra-account-metas',
    transferHookExtraAccountMetaList(address),
    async () => [await transferHookInitExtraAccountMetaList(authority.publicKey, address)],
  )

  // Holders need a Token-2022 ATA. Minting goes through the factory's
  // `mint_to`, which is authority-signed and does not run the transfer hook.
  const destination = await ensureAta(user, address, TOKEN_2022_PROGRAM_ID)
  created.accounts.rwaAta = destination.toBase58()
  const balance = await tokenBalance(destination)
  if (balance === null || balance === 0n) {
    await send('mint RWA tokens to the wallet', [
      await rwaMintTo(address, destination, authority.publicKey, 1_000n * ONE_TOKEN),
    ])
  } else {
    step('RWA balance already minted')
  }

  // The counterparty only receives; anyone may pay for its ATA.
  const counterparty = await ensureAta(context.counterparty, address, TOKEN_2022_PROGRAM_ID)
  created.accounts.rwaCounterpartyAta = counterparty.toBase58()
  step(`counterparty (whitelisted holder) ${context.counterparty.toBase58()}`)
}

async function seedRails() {
  log('Settlement clearinghouse + hooks escrow')
  await sendUnlessExists('initialize settlement config', settlementConfig(), async () => [
    await settlementInitialize(authority.publicKey, false),
  ])
  created.accounts.settlementConfig = settlementConfig().toBase58()
  created.accounts.hooksAuthority = hookAuthority().toBase58()

  const config = decodeSettlementConfig(await readRow(settlementConfig()))
  if (!config.hooksAuthority.equals(hookAuthority())) {
    await send('set hooks authority', [
      await settlementSetHooksAuthority(authority.publicKey, hookAuthority()),
    ])
  } else {
    step('hooks authority already set')
  }
  if (!config.transferAgentSecp256k1.some((byte) => byte !== 0)) {
    await send('set transfer agent (demo key, private key 1)', [
      await settlementSetTransferAgent(authority.publicKey, DEMO_AGENT_PUBKEY),
    ])
  } else {
    step('transfer agent already set')
  }

  // The hooks PDA owns the AMM-routed custody accounts and pays for the escrow
  // vault, so it needs rent headroom before the settlement rail can be used.
  if ((await connection.getBalance(hookAuthority())) < 2 * LAMPORTS_PER_SOL) {
    await send('fund hook authority PDA', [
      await hooksFundHookAuthority(authority.publicKey, BigInt(5 * LAMPORTS_PER_SOL)),
    ])
  } else {
    step('hook authority already funded')
  }
}

function poolWiring(options: Options) {
  return {
    kycConfig: options.rails.has('kyc') ? kycConfig() : null,
    maxDeviationBps: options.rails.has('oracle') ? ORACLE_MAX_DEVIATION_BPS : 0,
    hooksAuthority: options.rails.has('settlement') ? hookAuthority() : null,
    settlementSlots: options.rails.has('settlement') ? SETTLEMENT_HORIZON_SLOTS : 0n,
  }
}

async function seedOrderBook(context: Context) {
  const { mintA, mintB } = context
  log('Limit order book')
  const book = orderBook(mintA, mintB)
  if (await accountExists(book)) {
    step('order book already initialized')
  } else {
    await send('initialize order book', [
      await orderBookInitialize({
        authority: authority.publicKey,
        baseMint: mintA,
        quoteMint: mintB,
        kycConfig: null,
        hooksAuthority: hookAuthority(),
        settlementSlots: SETTLEMENT_HORIZON_SLOTS,
      }),
    ])
  }
  created.accounts.orderBook = book.toBase58()

  const expiry = BigInt(Math.floor(Date.now() / 1000) + 30 * 24 * 60 * 60)
  const orders: [bigint, 'ask' | 'bid', bigint, bigint][] = [
    [1n, 'ask', 2_000_000_000n, 25n * ONE_TOKEN],
    [2n, 'bid', 1_980_000_000n, 50n * ONE_TOKEN],
  ]
  for (const [id, side, price, amount] of orders) {
    const order = orderBookOrder(book, authority.publicKey, id)
    if (await accountExists(order)) {
      step(`order ${id} already placed`)
      continue
    }
    const escrowMint = side === 'ask' ? mintA : mintB
    await ensureAta(authority.publicKey, escrowMint, TOKEN_PROGRAM_ID)
    await send(`create order ${id}`, [
      await orderBookCreateOrder({
        maker: authority.publicKey,
        baseMint: mintA,
        quoteMint: mintB,
        orderId: id,
        side,
        price,
        amount,
        expiryTimestamp: expiry,
        escrowMint,
        tokenProgramId: TOKEN_PROGRAM_ID,
      }),
    ])
  }
  created.accounts.orderBookAsk = orderBookOrder(book, authority.publicKey, 1n).toBase58()
  created.accounts.orderBookBid = orderBookOrder(book, authority.publicKey, 2n).toBase58()
}

async function seedPools(context: Context) {
  const { mintA, mintB, options } = context
  log('Pools')
  const wiring = poolWiring(options)
  if (wiring.kycConfig && !(await accountExists(kycConfig()))) {
    die('--rails kyc needs the KYC gate initialized; run once without it first')
  }
  step(
    `rails: ${[...options.rails].join(', ') || 'none'} · oracle ${wiring.maxDeviationBps} bps · ` +
      `settlement ${wiring.settlementSlots} slots`,
  )

  // ---- CLMM ---------------------------------------------------------
  const clmmAddress = clmmPool(mintA, mintB)
  created.pools.clmm = clmmAddress.toBase58()
  await sendUnlessExists('CLMM initialize_pool', clmmAddress, async () => [
    await clmmInitializePool({
      authority: authority.publicKey,
      mintA,
      mintB,
      initialSqrtPriceX64: sqrtPriceX64FromPrice(1),
      tickSpacing: CLMM_TICK_SPACING,
      feePips: CLMM_FEE_PIPS,
      ...wiring,
    }),
  ])
  if (decodeClmmPool(await readRow(clmmAddress)).liquidity === 0n) {
    await ensureAta(authority.publicKey, mintA, TOKEN_PROGRAM_ID)
    await ensureAta(authority.publicKey, mintB, TOKEN_PROGRAM_ID)
    await send('CLMM add_liquidity (band around 1.0)', [
      await clmmAddLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        lowerTick: CLMM_LOWER_TICK,
        upperTick: CLMM_UPPER_TICK,
        liquidityAmount: CLMM_LIQUIDITY,
      }),
    ])
  } else {
    step('CLMM liquidity already added')
  }

  // ---- DLMM ---------------------------------------------------------
  const dlmmAddress = dlmmPool(mintA, mintB)
  created.pools.dlmm = dlmmAddress.toBase58()
  await sendUnlessExists('DLMM initialize_pool', dlmmAddress, async () => [
    await dlmmInitializePool({
      payer: authority.publicKey,
      mintA,
      mintB,
      arrayIndex: 0n,
      activeBinId: 0,
      binStep: DLMM_BIN_STEP,
      baseFeeBps: DLMM_BASE_FEE_BPS,
      authority: authority.publicKey,
      ...wiring,
    }),
  ])
  if (!(await accountExists(dlmmPosition(dlmmAddress, authority.publicKey, 0)))) {
    await send('DLMM add_liquidity (bin 0)', [
      await dlmmAddLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        binId: 0,
        amountA: POOL_LEG,
        amountB: POOL_LEG,
      }),
    ])
  } else {
    step('DLMM liquidity already added')
  }

  // ---- Maverick -----------------------------------------------------
  const maverickAddress = maverickPool(mintA, mintB)
  created.pools.maverick = maverickAddress.toBase58()
  await sendUnlessExists('Maverick initialize_pool', maverickAddress, async () => [
    await maverickInitializePool({
      payer: authority.publicKey,
      mintA,
      mintB,
      arrayIndex: 0n,
      activeBinId: 0,
      binStep: MAVERICK_BIN_STEP,
      baseFeeBps: MAVERICK_BASE_FEE_BPS,
      authority: authority.publicKey,
      ...wiring,
    }),
  ])
  if (!(await accountExists(maverickPosition(maverickAddress, authority.publicKey, 0)))) {
    // The first bin has to be the static baseline; the moving kinds unlock only
    // after it exists (`has_static_baseline`).
    await send('Maverick add_liquidity (bin 0, static)', [
      await maverickAddLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        binId: 0,
        kind: 'static',
        amountA: POOL_LEG,
        amountB: POOL_LEG,
      }),
    ])
  } else {
    step('Maverick liquidity already added')
  }

  // ---- TWAMM --------------------------------------------------------
  const twammAddress = twammPool(mintA, mintB)
  created.pools.twamm = twammAddress.toBase58()
  await sendUnlessExists('TWAMM initialize_pool', twammAddress, async () => [
    await twammInitializePool({
      payer: authority.publicKey,
      mintA,
      mintB,
      initialAmountA: POOL_LEG,
      initialAmountB: POOL_LEG,
      authority: authority.publicKey,
      ...wiring,
    }),
  ])
  const twamm = await loadTwammPool(connection, twammAddress)
  step(`TWAMM reserves ${twamm.reserveA} A / ${twamm.reserveB} B · lp supply ${twamm.lpSupply}`)

  const order = twammOrder(twammAddress, authority.publicKey, 1n)
  created.accounts.twammOrder = order.toBase58()
  if (await accountExists(order)) {
    step('TWAMM order already created')
    return
  }
  await send('TWAMM create_long_term_order (1h, 1,000 mUSDC → mTSLA)', [
    await twammCreateLongTermOrder({
      owner: authority.publicKey,
      mintA,
      mintB,
      orderId: 1n,
      amountIn: 1_000n * ONE_TOKEN,
      durationSeconds: 3_600,
      isAToB: true,
      walkOrders: [],
      wiring: { kycConfig: wiring.kycConfig !== null, oracle: wiring.maxDeviationBps > 0 },
    }),
  ])
  await send('TWAMM execute_virtual_orders (crank)', [
    await twammExecuteVirtualOrders({
      mintA,
      mintB,
      expiredOrders: [],
      wiring: { oracle: wiring.maxDeviationBps > 0 },
    }),
  ])
}

async function seedEscrow(context: Context): Promise<bigint> {
  log('T+N settlement escrow')
  const tradeId = 1n
  const vault = settlementVault(tradeId)
  created.accounts.settlementVault = vault.toBase58()
  // The trade's escrow account: a PDA of the settlement program, owned by the
  // fixed escrow authority above, created by `initiate_settlement` itself.
  created.accounts.settlementEscrow = settlementEscrowToken(tradeId).toBase58()
  if (await accountExists(vault)) {
    step('escrow 1 already exists')
    return tradeId
  }

  const source = await ensureAta(authority.publicKey, context.mintA, TOKEN_PROGRAM_ID)
  await ensureAta(context.user, context.mintA, TOKEN_PROGRAM_ID)
  // `initiate_settlement` takes the *absolute* slot it unlocks at (it only
  // checks `> clock.slot`), so the T+N horizon is resolved here.
  const slot = await connection.getSlot('confirmed')
  await send('settlement initiate_settlement (taker = the demo wallet)', [
    await settlementInitiate({
      tradeId,
      settlementSlots: BigInt(slot) + SETTLEMENT_HORIZON_SLOTS,
      amount: 250n * ONE_TOKEN,
      authority: authority.publicKey,
      sourceTokenAccount: source,
      tokenMint: context.mintA,
      taker: context.user,
      pool: null,
      tokenProgramId: TOKEN_PROGRAM_ID,
    }),
  ])
  const decoded = decodeSettlementVault(await readRow(vault))
  step(
    `escrow holds ${decoded.amount} mUSDC for ${context.user.toBase58()}, unlocks at slot ` +
      `${decoded.settlementSlot} (now ${slot})`,
  )
  return tradeId
}

// -----------------------------------------------------------------------
// Verification: read everything back with the UI's decoders, then trade through
// the UI's own instruction builders.
//
// Every check is an `attempt`: a failure is recorded and printed, never thrown,
// so one missing rail can never stop the rest of the suite (or the manifest)
// from completing. The results are summarised at the end.
// -----------------------------------------------------------------------

async function attempt(name: string, run: () => Promise<string | void>): Promise<void> {
  try {
    const detail = (await run()) ?? ''
    created.exercises.push({ name, status: 'ok', detail })
    step(`\x1b[32m✓\x1b[0m ${name}${detail ? `  ${detail}` : ''}`)
  } catch (cause) {
    const detail = describeError(cause)
    created.exercises.push({ name, status: 'fail', detail })
    process.stderr.write(`\x1b[1;31m  ✗ ${name}\x1b[0m  ${detail}\n`)
  }
}

function recordSkip(name: string, detail: string) {
  created.exercises.push({ name, status: 'skip', detail })
  step(`\x1b[33m•\x1b[0m ${name}  ${detail}`)
}

/** Polls the confirmed slot until it reaches `target` (or the deadline). */
async function waitForSlot(target: bigint) {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    if (BigInt(await connection.getSlot('confirmed')) >= target) return
    await sleep(400)
  }
  throw new Error(`settlement slot ${target} not reached within 30s`)
}

/**
 * The demo Transfer Agent's signature over the settlement preimage
 * `keccak256(trade_id ‖ taker ‖ mint ‖ amount)`. The private key is `1`, i.e.
 * the secp256k1 generator point already registered as the agent on chain.
 */
function signAgentMessage(tradeId: bigint, taker: PublicKey, mint: PublicKey, amount: bigint) {
  const message = Buffer.concat([u64le(tradeId), taker.toBuffer(), mint.toBuffer(), u64le(amount)])
  const signature = secp256k1.sign(keccak_256(message), 1n, { lowS: true })
  if (signature.recovery === undefined) throw new Error('signature has no recovery id')
  return { signature: signature.toCompactRawBytes(), recoveryId: signature.recovery }
}

/**
 * A distinct non-zero trade id per settlement-routed swap. A vault is created
 * once per id, so re-using one across pools would collide. With the rail off
 * the id must be exactly zero.
 */
// Derived from the clock so re-running against the same ledger never reuses an
// id (each id's vault is created once).
let nextSettlementTradeId = BigInt(Date.now()) * 1_000n
function routeTradeId(settlementOn: boolean): bigint {
  if (!settlementOn) return 0n
  nextSettlementTradeId += 1n
  return nextSettlementTradeId
}

/**
 * `trip_breaker` is a documented no-op until the oracle has two observations,
 * and it deliberately returns `PriceWithinThreshold` when the price is inside
 * the threshold. Both are healthy outcomes; only an unexpected error or a
 * pause that will not clear counts as a failure.
 */
async function exerciseTripBreaker(
  name: string,
  pool: PublicKey,
  build: () => Promise<TransactionInstruction>,
) {
  await attempt(name, async () => {
    const oracle = oracleState(pool)
    const before = decodeOracleState(await readRow(oracle))
    let outcome = ''
    try {
      await send(`  ${name}`, [await build()])
    } catch (cause) {
      const text = cause instanceof Error ? cause.message : String(cause)
      // Oracle error 6009 / 0x1779: the breaker ran, the price was inside the
      // threshold. Not a failure.
      if (!/PriceWithinThreshold|0x1779/i.test(text)) throw cause
      outcome = 'breaker ran; price inside the threshold'
    }
    const after = decodeOracleState(await readRow(oracle))
    if (!after.isPaused) {
      return (
        outcome ||
        `no-op: TWAP not armed yet (${before.observationCount} observation(s), no 30-minute window yet)`
      )
    }
    await send('  oracle unpause_pool', [await oracleUnpausePool(pool, authority.publicKey)])
    if (decodeOracleState(await readRow(oracle)).isPaused) throw new Error('unpause_pool left it paused')
    return 'tripped → unpaused'
  })
}

async function verify(context: Context) {
  const { mintA, mintB, options } = context
  log('Reading the seeded state back through the UI decoders')

  const clmmAddress = clmmPool(mintA, mintB)
  const clmm = decodeClmmPool(await readRow(clmmAddress))
  step(
    `CLMM        tick ${clmm.tickCurrent} · liquidity ${clmm.liquidity} · fee ${clmm.feePips} pips · ` +
      `kyc ${clmm.kycConfig.equals(PublicKey.default) ? 'off' : 'on'} · ` +
      `oracle ${clmm.oracleState.equals(PublicKey.default) ? 'off' : 'on'} · ` +
      `settlement ${clmm.hooksAuthority.equals(PublicKey.default) ? 'off' : 'on'}`,
  )
  if (!clmm.oracleState.equals(PublicKey.default)) {
    const oracle = decodeOracleState(await readRow(oracleState(clmmAddress)))
    step(`  oracle    ${oracle.observationCount} observation(s) · max deviation ${oracle.maxDeviationBps} bps · paused ${oracle.isPaused}`)
  }
  const dlmm = decodeDlmmPool(await readRow(dlmmPool(mintA, mintB)))
  step(`DLMM        active bin ${dlmm.activeBinId} · bin step ${dlmm.binStep} · decimals ${dlmm.decimalsA}/${dlmm.decimalsB}`)
  const maverick = decodeMaverickPool(await readRow(maverickPool(mintA, mintB)))
  step(`Maverick    active bin ${maverick.activeBinId} · bin step ${maverick.binStep}`)
  const twamm = decodeTwammPool(await readRow(twammPool(mintA, mintB)))
  step(`TWAMM       reserves ${twamm.reserveA}/${twamm.reserveB} · lp supply ${twamm.lpSupply}`)
  const order = decodeTwammOrder(await readRow(twammOrder(twammPool(mintA, mintB), authority.publicKey, 1n)))
  step(`  order 1   ${order.amountIn} in · ${order.duration}s · A→B ${order.isAToB}`)
  const rwa = decodeMint(await readRow(new PublicKey(created.mints.rwa)))
  step(`RWA mint    ${rwa.address.toBase58()} · ${rwa.decimals} decimals · supply ${rwa.supply}`)

  if (!options.verify) return

  log('Trading through the UI instruction builders')
  // The pools' rails are read off the pool account, exactly as the pages derive
  // them: a re-run without --rails still has to trade the pool that is already
  // on chain, and passing a rail the pool does not have is an error on-chain.
  const wiring = {
    kycConfig: !clmm.kycConfig.equals(PublicKey.default),
    oracle: !clmm.oracleState.equals(PublicKey.default),
    settlement: !clmm.hooksAuthority.equals(PublicKey.default),
  }
  step(
    `pool rails: ${wiring.kycConfig ? 'kyc ' : ''}${wiring.oracle ? 'oracle ' : ''}${wiring.settlement ? 'settlement' : ''}`.trimEnd() ||
      'none',
  )
  // Settlement routing sends the swap output into the hook authority's custody
  // ATA, which the AMMs transfer into directly - so the ATA must already exist
  // (a PDA can own an ATA; anyone may pay to create it).
  if (wiring.settlement) {
    await ensureAta(hookAuthority(), mintA, TOKEN_PROGRAM_ID)
    await ensureAta(hookAuthority(), mintB, TOKEN_PROGRAM_ID)
  }
  // A CLMM swap must list every initialized tick it may cross, in walk order.
  // The narrow -128 band only exists once the suite has opened it (or a prior
  // run did), so include it when it is on chain.
  const clmmWalkTicks: number[] = [CLMM_LOWER_TICK]
  if (await accountExists(clmmTick(clmmAddress, CLMM_NARROW_LOWER_TICK))) {
    clmmWalkTicks.unshift(CLMM_NARROW_LOWER_TICK)
  }
  const amountIn = 10n * ONE_TOKEN
  const beforeB = (await tokenBalance(ata(authority.publicKey, mintB, TOKEN_PROGRAM_ID))) ?? 0n

  const trade = async (pool: string, build: () => Promise<TransactionInstruction>) => {
    const instruction = await build()
    const signature = await send(`${pool} swap (A→B, ${amountIn} raw)`, [instruction])
    created.swaps.push({ pool, amountIn: amountIn.toString(), signature })
  }

  await attempt('CLMM swap A→B', () =>
    trade('CLMM', () =>
      clmmSwap({
        user: authority.publicKey,
        mintA,
        mintB,
        amountIn,
        minAmountOut: 1n,
        isAToB: true,
        tradeId: routeTradeId(wiring.settlement),
        ticks: clmmWalkTicks,
        wiring,
      }),
    ),
  )
  await attempt('DLMM swap A→B', () =>
    trade('DLMM', () =>
      dlmmSwap({
        user: authority.publicKey,
        mintA,
        mintB,
        amountIn,
        minAmountOut: 1n,
        isAToB: true,
        tradeId: routeTradeId(wiring.settlement),
        binArrayIndexes: [0n],
        wiring,
      }),
    ),
  )
  await attempt('Maverick swap A→B', () =>
    trade('Maverick', () =>
      maverickSwap({
        user: authority.publicKey,
        mintA,
        mintB,
        amountIn,
        minAmountOut: 1n,
        isAToB: true,
        tradeId: routeTradeId(wiring.settlement),
        binArrayIndexes: [0n],
        wiring,
      }),
    ),
  )

  const afterB = (await tokenBalance(ata(authority.publicKey, mintB, TOKEN_PROGRAM_ID))) ?? 0n
  const clmmAfter = decodeClmmPool(await readRow(clmmAddress))
  step(`received ${afterB - beforeB} mTSLA raw for ${3n * amountIn} mUSDC raw across three pools`)
  step(`CLMM tick moved ${clmm.tickCurrent} → ${clmmAfter.tickCurrent}`)

  // A plain await, not `attemptWithLimitation`: this is the self-check for the
  // whitelist, and it now runs against the stock Token-2022 a local validator
  // ships (the proofs travel as accounts, so no hint data is appended to the
  // transfer for the token program to reject).
  await verifyRwaTransfer(context)

  await exerciseAll(context, {
    clmm: clmmAddress,
    dlmm: dlmmPool(mintA, mintB),
    maverick: maverickPool(mintA, mintB),
    twamm: twammPool(mintA, mintB),
  })
}

/**
 * Moves one RWA token between two whitelisted owners with the UI's `rwaTransfer`
 * builder.
 *
 * The hook resolves each side's entry from the wallet's token account, so this
 * exercises the whole compliance path end to end: the two entries seeded above,
 * the mint's meta list, the proof fold in `execute`, and the token movement. A
 * wallet with no entry (or a stale one) fails here.
 */
async function verifyRwaTransfer(context: Context) {
  log('Transferring RWA tokens through the UI instruction builders')
  const mint = new PublicKey(created.mints.rwa)
  const source = ata(authority.publicKey, mint, TOKEN_2022_PROGRAM_ID)
  const destination = ata(context.counterparty, mint, TOKEN_2022_PROGRAM_ID)
  const amount = ONE_TOKEN

  const before = await tokenBalance(destination)
  await send('RWA transfer (two whitelisted owners, both entries on chain)', [
    await rwaTransfer({
      source,
      sourceOwner: authority.publicKey,
      destination,
      destinationOwner: context.counterparty,
      mint,
      authority: authority.publicKey,
      amount,
    }),
  ])
  const after = await tokenBalance(destination)
  step(`counterparty RWA balance ${before ?? 0n} → ${after ?? 0n} (+${(after ?? 0n) - (before ?? 0n)})`)
}

interface PoolAddresses {
  clmm: PublicKey
  dlmm: PublicKey
  maverick: PublicKey
  twamm: PublicKey
}

/**
 * Exercises every protocol instruction the UI can build that the seeder itself
 * does not already call, so a localnet boot proves the whole surface - not just
 * pool creation and a swap. Admin toggles are reverted (pause off, authority
 * restored, fee back to zero) so the pools are left usable for the UI.
 */
async function exerciseAll(context: Context, pools: PoolAddresses) {
  const { mintA, mintB, user } = context
  const clmmState = decodeClmmPool(await readRow(pools.clmm))
  const wiring = {
    kycConfig: !clmmState.kycConfig.equals(PublicKey.default),
    oracle: !clmmState.oracleState.equals(PublicKey.default),
    settlement: !clmmState.hooksAuthority.equals(PublicKey.default),
  }

  log('Exercising every remaining protocol instruction')
  step('each check is independent; failures are collected and never stop the seed')

  // ---- CLMM ---------------------------------------------------------
  const clmmWide = clmmPosition(pools.clmm, authority.publicKey, CLMM_LOWER_TICK, CLMM_UPPER_TICK)
  const clmmNarrowLower = CLMM_NARROW_LOWER_TICK
  const clmmNarrowUpper = CLMM_NARROW_UPPER_TICK

  await attempt('CLMM add_liquidity (narrow ±128 band)', async () => {
    await send('  CLMM add_liquidity (narrow band)', [
      await clmmAddLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        lowerTick: clmmNarrowLower,
        upperTick: clmmNarrowUpper,
        liquidityAmount: 1_000_000_000n,
      }),
    ])
    const position = decodeClmmPosition(
      await readRow(clmmPosition(pools.clmm, authority.publicKey, clmmNarrowLower, clmmNarrowUpper)),
    )
    return `position liquidity ${position.liquidity}`
  })

  await attempt('CLMM collect_fees (wide band)', async () => {
    const position = decodeClmmPosition(await readRow(clmmWide))
    const accountA = ata(authority.publicKey, mintA, TOKEN_PROGRAM_ID)
    const accountB = ata(authority.publicKey, mintB, TOKEN_PROGRAM_ID)
    const beforeA = (await tokenBalance(accountA)) ?? 0n
    const beforeB = (await tokenBalance(accountB)) ?? 0n
    await send('  CLMM collect_fees', [
      await clmmCollectFees({
        user: authority.publicKey,
        mintA,
        mintB,
        lowerTick: CLMM_LOWER_TICK,
        upperTick: CLMM_UPPER_TICK,
      }),
    ])
    const receivedA = ((await tokenBalance(accountA)) ?? 0n) - beforeA
    const receivedB = ((await tokenBalance(accountB)) ?? 0n) - beforeB
    return `position liquidity ${position.liquidity}; collected ${receivedA} mUSDC + ${receivedB} mTSLA`
  })

  await attempt('CLMM remove_liquidity (10% of the wide band)', async () => {
    const before = decodeClmmPosition(await readRow(clmmWide))
    await send('  CLMM remove_liquidity', [
      await clmmRemoveLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        lowerTick: CLMM_LOWER_TICK,
        upperTick: CLMM_UPPER_TICK,
        liquidityAmount: before.liquidity / 10n,
        minAmountA: 0n,
        minAmountB: 0n,
      }),
    ])
    const after = decodeClmmPosition(await readRow(clmmWide))
    return `liquidity ${before.liquidity} → ${after.liquidity}`
  })

  await exerciseTripBreaker('CLMM trip_breaker → oracle unpause', pools.clmm, () =>
    clmmTripBreaker({ pool: pools.clmm }),
  )

  await attempt('CLMM set_paused true → false', async () => {
    await send('  CLMM set_paused (true)', [await clmmSetPaused(pools.clmm, authority.publicKey, true)])
    if (!decodeClmmPool(await readRow(pools.clmm)).paused) throw new Error('pause flag did not set')
    await send('  CLMM set_paused (false)', [await clmmSetPaused(pools.clmm, authority.publicKey, false)])
    if (decodeClmmPool(await readRow(pools.clmm)).paused) throw new Error('pause flag did not clear')
    return 'paused true → false'
  })

  await attempt('CLMM set_authority rotate → restore', async () => {
    const temp = Keypair.generate()
    await send('  CLMM set_authority (temp)', [
      await clmmSetAuthority(pools.clmm, authority.publicKey, temp.publicKey),
    ])
    if (!decodeClmmPool(await readRow(pools.clmm)).authority.equals(temp.publicKey)) {
      throw new Error('authority was not rotated')
    }
    await send(
      '  CLMM set_authority (restore)',
      [await clmmSetAuthority(pools.clmm, temp.publicKey, authority.publicKey)],
      [temp],
    )
    if (!decodeClmmPool(await readRow(pools.clmm)).authority.equals(authority.publicKey)) {
      throw new Error('authority was not restored')
    }
    return 'rotated and restored'
  })

  // ---- DLMM ---------------------------------------------------------
  const dlmmBin = 10
  const dlmmBinPosition = dlmmPosition(pools.dlmm, authority.publicKey, dlmmBin)

  await attempt('DLMM initialize_bin_array (index 1)', async () => {
    const array = dlmmBinArray(pools.dlmm, 1)
    if (!(await accountExists(array))) {
      await send('  DLMM initialize_bin_array 1', [
        await dlmmInitializeBinArray({ payer: authority.publicKey, pool: pools.dlmm, index: 1 }),
      ])
    }
    return `array ${array.toBase58().slice(0, 8)}…`
  })

  await attempt(`DLMM add_liquidity (bin ${dlmmBin})`, async () => {
    if (await accountExists(dlmmBinPosition)) return 'position already funded on this ledger'
    await send('  DLMM add_liquidity (bin 10)', [
      await dlmmAddLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        binId: dlmmBin,
        amountA: 10n * ONE_TOKEN,
        amountB: 10n * ONE_TOKEN,
      }),
    ])
    const position = decodeBinPosition(await readRow(dlmmBinPosition))
    return `position shares ${position.shares}`
  })

  await attempt(`DLMM remove_liquidity (half of bin ${dlmmBin})`, async () => {
    if (!(await accountExists(dlmmBinPosition))) return 'no position to remove'
    const position = decodeBinPosition(await readRow(dlmmBinPosition))
    if (position.shares === 0n) return 'no shares left to remove'
    await send('  DLMM remove_liquidity (bin 10)', [
      await dlmmRemoveLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        binId: dlmmBin,
        shares: position.shares / 2n,
        minAmountA: 0n,
        minAmountB: 0n,
      }),
    ])
    const after = decodeBinPosition(await readRow(dlmmBinPosition))
    return `shares ${position.shares} → ${after.shares}`
  })

  await attempt('DLMM set_paused true → false', async () => {
    await send('  DLMM set_paused (true)', [await dlmmSetPaused(pools.dlmm, authority.publicKey, true)])
    if (!decodeDlmmPool(await readRow(pools.dlmm)).paused) throw new Error('pause flag did not set')
    await send('  DLMM set_paused (false)', [await dlmmSetPaused(pools.dlmm, authority.publicKey, false)])
    if (decodeDlmmPool(await readRow(pools.dlmm)).paused) throw new Error('pause flag did not clear')
    return 'paused true → false'
  })

  await attempt('DLMM set_authority rotate → restore', async () => {
    const temp = Keypair.generate()
    await send('  DLMM set_authority (temp)', [
      await dlmmSetAuthority(pools.dlmm, authority.publicKey, temp.publicKey),
    ])
    if (!decodeDlmmPool(await readRow(pools.dlmm)).authority.equals(temp.publicKey)) {
      throw new Error('authority was not rotated')
    }
    await send(
      '  DLMM set_authority (restore)',
      [await dlmmSetAuthority(pools.dlmm, temp.publicKey, authority.publicKey)],
      [temp],
    )
    if (!decodeDlmmPool(await readRow(pools.dlmm)).authority.equals(authority.publicKey)) {
      throw new Error('authority was not restored')
    }
    return 'rotated and restored'
  })

  await exerciseTripBreaker('DLMM trip_breaker → oracle unpause', pools.dlmm, () =>
    dlmmTripBreaker(pools.dlmm),
  )

  await attempt('DLMM swap B→A (the reverse direction)', async () => {
    await send('  DLMM swap B→A', [
      await dlmmSwap({
        user: authority.publicKey,
        mintA,
        mintB,
        amountIn: 5n * ONE_TOKEN,
        minAmountOut: 1n,
        isAToB: false,
        tradeId: routeTradeId(wiring.settlement),
        binArrayIndexes: [0n],
        wiring,
      }),
    ])
    return 'ok'
  })

  // ---- Maverick -----------------------------------------------------
  const maverickSourceBin = -60
  const maverickDestBin = -21
  const maverickSourcePosition = maverickPosition(
    pools.maverick,
    authority.publicKey,
    maverickSourceBin,
  )

  await attempt('Maverick initialize_bin_array (-1 and -2)', async () => {
    for (const index of [-1, -2]) {
      const array = maverickBinArray(pools.maverick, index)
      if (!(await accountExists(array))) {
        await send(`  Maverick initialize_bin_array ${index}`, [
          await maverickInitializeBinArray(authority.publicKey, pools.maverick, index),
        ])
      }
    }
    return 'arrays -2 and -1 open'
  })

  await attempt(`Maverick add_liquidity (bin ${maverickSourceBin}, kind both)`, async () => {
    if (await accountExists(maverickSourcePosition)) return 'bin -60 already funded on this ledger'
    await send('  Maverick add_liquidity (bin -60)', [
      await maverickAddLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        binId: maverickSourceBin,
        kind: 'both',
        amountA: 100n * ONE_TOKEN,
        amountB: 100n * ONE_TOKEN,
      }),
    ])
    const position = decodeBinPosition(await readRow(maverickSourcePosition))
    return `position shares ${position.shares}`
  })

  await attempt(
    `Maverick move_bins (${maverickSourceBin} → ${maverickDestBin}, toward the price)`,
    async () => {
      const position = decodeBinPosition(await readRow(maverickSourcePosition))
      const acted = position.shares > 0n
      if (acted) {
        await send('  Maverick move_bins', [
          await maverickMoveBins({
            pool: pools.maverick,
            fromArrayIndex: -2,
            fromBinId: maverickSourceBin,
            toBinId: maverickDestBin,
          }),
        ])
      }
      // Read the hop back off the array accounts with the same decoder the
      // Maverick page renders from, so the UI's kind/move-chain view is
      // validated against real chain state rather than assumed.
      const sourceArray = decodeMaverickBinArray(
        await readRow(maverickBinArray(pools.maverick, -2)),
      )
      const source = sourceArray.bins.find((bin) => bin.binId === maverickSourceBin)
      if (!source?.moved) throw new Error(`bin ${maverickSourceBin} is not recorded as moved`)
      if (source.toBinId !== maverickDestBin) {
        throw new Error(`bin ${maverickSourceBin} points at ${source.toBinId}, expected ${maverickDestBin}`)
      }
      if (source.reserveA !== 0n || source.reserveB !== 0n) {
        throw new Error(`moved bin kept reserves ${source.reserveA}/${source.reserveB}`)
      }
      if (source.supply === 0n) throw new Error('moved bin lost its share ledger')
      const destArray = decodeMaverickBinArray(await readRow(maverickBinArray(pools.maverick, -1)))
      const dest = destArray.bins.find((bin) => bin.binId === maverickDestBin)
      if (!dest) throw new Error(`destination bin ${maverickDestBin} is outside its array`)
      return (
        `bin ${maverickSourceBin} → bin ${maverickDestBin} on chain` +
        ` (reserves 0, ${source.supply} shares follow at ${source.shareRatio} Q64.64)` +
        `, destination holds ${dest.reserveA}/${dest.reserveB} with ${dest.supply} shares` +
        (acted ? '' : ' — move already on this ledger')
      )
    },
  )

  await attempt('Maverick remove_liquidity (follows the move chain)', async () => {
    const position = decodeBinPosition(await readRow(maverickSourcePosition))
    if (position.shares === 0n) return 'no shares left to remove'
    await send('  Maverick remove_liquidity', [
      await maverickRemoveLiquidity({
        user: authority.publicKey,
        mintA,
        mintB,
        binId: maverickSourceBin,
        shares: position.shares,
        minAmountA: 0n,
        minAmountB: 0n,
        hopArrayIndexes: [-1],
      }),
    ])
    const after = decodeBinPosition(await readRow(maverickSourcePosition))
    return `shares ${position.shares} → ${after.shares}`
  })

  await attempt('Maverick set_paused true → false', async () => {
    await send('  Maverick set_paused (true)', [
      await maverickSetPaused(pools.maverick, authority.publicKey, true),
    ])
    if (!decodeMaverickPool(await readRow(pools.maverick)).paused) throw new Error('pause flag did not set')
    await send('  Maverick set_paused (false)', [
      await maverickSetPaused(pools.maverick, authority.publicKey, false),
    ])
    if (decodeMaverickPool(await readRow(pools.maverick)).paused) throw new Error('pause flag did not clear')
    return 'paused true → false'
  })

  await attempt('Maverick set_authority rotate → restore', async () => {
    const temp = Keypair.generate()
    await send('  Maverick set_authority (temp)', [
      await maverickSetAuthority(pools.maverick, authority.publicKey, temp.publicKey),
    ])
    if (!decodeMaverickPool(await readRow(pools.maverick)).authority.equals(temp.publicKey)) {
      throw new Error('authority was not rotated')
    }
    await send(
      '  Maverick set_authority (restore)',
      [await maverickSetAuthority(pools.maverick, temp.publicKey, authority.publicKey)],
      [temp],
    )
    if (!decodeMaverickPool(await readRow(pools.maverick)).authority.equals(authority.publicKey)) {
      throw new Error('authority was not restored')
    }
    return 'rotated and restored'
  })

  await exerciseTripBreaker('Maverick trip_breaker → oracle unpause', pools.maverick, () =>
    maverickTripBreaker(pools.maverick),
  )

  await attempt('Maverick swap B→A (the reverse direction)', async () => {
    await send('  Maverick swap B→A', [
      await maverickSwap({
        user: authority.publicKey,
        mintA,
        mintB,
        amountIn: 5n * ONE_TOKEN,
        minAmountOut: 1n,
        isAToB: false,
        tradeId: routeTradeId(wiring.settlement),
        binArrayIndexes: [0n],
        wiring,
      }),
    ])
    return 'ok'
  })

  // ---- TWAMM --------------------------------------------------------
  const twammLp = twammLpPosition(pools.twamm, authority.publicKey)
  const order1 = twammOrder(pools.twamm, authority.publicKey, 1n)
  const order2 = twammOrder(pools.twamm, authority.publicKey, 2n)

  await attempt('TWAMM add_liquidity', async () => {
    await send('  TWAMM add_liquidity', [
      await twammAddLiquidity({
        owner: authority.publicKey,
        mintA,
        mintB,
        amountA: 100n * ONE_TOKEN,
        amountB: 100n * ONE_TOKEN,
      }),
    ])
    const lp = decodeTwammLp(await readRow(twammLp))
    return `lp shares ${lp.shares}`
  })

  await attempt('TWAMM remove_liquidity', async () => {
    const lp = decodeTwammLp(await readRow(twammLp))
    if (lp.shares === 0n) throw new Error('no LP shares to remove')
    await send('  TWAMM remove_liquidity', [
      await twammRemoveLiquidity({
        owner: authority.publicKey,
        mintA,
        mintB,
        shares: lp.shares / 5n,
        minAmountA: 0n,
        minAmountB: 0n,
      }),
    ])
    const after = decodeTwammLp(await readRow(twammLp))
    return `shares ${lp.shares} → ${after.shares}`
  })

  await attempt('TWAMM create_long_term_order (order 2, B→A)', async () => {
    if (await accountExists(order2)) return 'order 2 already exists'
    await send('  TWAMM create_long_term_order 2', [
      await twammCreateLongTermOrder({
        owner: authority.publicKey,
        mintA,
        mintB,
        orderId: 2n,
        amountIn: 500n * ONE_TOKEN,
        durationSeconds: 7_200,
        isAToB: false,
        walkOrders: [order1],
        wiring: { kycConfig: wiring.kycConfig, oracle: wiring.oracle },
      }),
    ])
    const order = decodeTwammOrder(await readRow(order2))
    return `sells ${order.amountIn} B→A over ${order.duration}s`
  })

  await attempt('TWAMM cancel_order (order 2)', async () => {
    if (!(await accountExists(order2))) return 'order 2 already cancelled'
    // A settlement-routed cancellation escrows the order's accrued proceeds,
    // and the hooks program refuses a zero amount - so crank once so the order
    // has some flow before it is cancelled.
    if (wiring.settlement) {
      await sleep(1_500)
      await send('  TWAMM execute_virtual_orders (accrue before cancel)', [
        await twammExecuteVirtualOrders({
          mintA,
          mintB,
          expiredOrders: [],
          wiring: { oracle: wiring.oracle },
        }),
      ])
    }
    await send('  TWAMM cancel_order 2', [
      await twammCancelOrder({
        owner: authority.publicKey,
        mintA,
        mintB,
        orderId: 2n,
        isAToB: false,
        prefixOrders: [order1],
        wiring: { oracle: wiring.oracle, settlement: wiring.settlement },
      }),
    ])
    if (await accountExists(order2)) throw new Error('cancelled order account was not closed')
    return 'order 2 cancelled and closed'
  })

  await attempt('TWAMM short order → crank → withdraw_proceeds', async () => {
    // Time-derived, so a re-run against the same ledger does not reuse an id
    // whose settlement escrow already exists.
    const orderId = BigInt(Date.now())
    const order3 = twammOrder(pools.twamm, authority.publicKey, orderId)
    if (!(await accountExists(order3))) {
      // A five-second order is the shortest thing that can be settled and
      // withdrawn inside a single seeder run. It expires before order 1, so it
      // splices in at the head and needs no prefix accounts on creation.
      await send('  TWAMM create_long_term_order 3 (5s)', [
        await twammCreateLongTermOrder({
          owner: authority.publicKey,
          mintA,
          mintB,
          orderId,
          amountIn: 5n * ONE_TOKEN,
          durationSeconds: 5,
          isAToB: true,
          walkOrders: [],
          wiring: { kycConfig: wiring.kycConfig, oracle: wiring.oracle },
        }),
      ])
    }
    let order = decodeTwammOrder(await readRow(order3))
    const waitMs = Number(order.expiryTimestamp) * 1_000 - Date.now() + 1_500
    if (waitMs > 0) {
      step(`  waiting ${Math.ceil(waitMs / 1_000)}s for order 3 to expire…`)
      await sleep(waitMs)
    }
    order = decodeTwammOrder(await readRow(order3))
    if (!order.settled) {
      await send('  TWAMM execute_virtual_orders (settle order 3)', [
        await twammExecuteVirtualOrders({
          mintA,
          mintB,
          expiredOrders: [order3],
          wiring: { oracle: wiring.oracle },
        }),
      ])
      order = decodeTwammOrder(await readRow(order3))
    }
    if (!order.settled) throw new Error('order 3 did not settle')
    await send('  TWAMM withdraw_proceeds 3', [
      await twammWithdrawProceeds({
        owner: authority.publicKey,
        mintA,
        mintB,
        orderId,
        isAToB: true,
        wiring: { settlement: wiring.settlement },
      }),
    ])
    if (await accountExists(order3)) throw new Error('withdrawn order account was not closed')
    return `paid ${order.proceedsOut} mTSLA and refunded the unsold mUSDC`
  })

  await attempt('TWAMM set_paused true → false', async () => {
    await send('  TWAMM set_paused (true)', [await twammSetPaused(pools.twamm, authority.publicKey, true)])
    if (!decodeTwammPool(await readRow(pools.twamm)).paused) throw new Error('pause flag did not set')
    await send('  TWAMM set_paused (false)', [await twammSetPaused(pools.twamm, authority.publicKey, false)])
    if (decodeTwammPool(await readRow(pools.twamm)).paused) throw new Error('pause flag did not clear')
    return 'paused true → false'
  })

  await attempt('TWAMM set_authority rotate → restore', async () => {
    const temp = Keypair.generate()
    await send('  TWAMM set_authority (temp)', [
      await twammSetAuthority(pools.twamm, authority.publicKey, temp.publicKey),
    ])
    if (!decodeTwammPool(await readRow(pools.twamm)).authority.equals(temp.publicKey)) {
      throw new Error('authority was not rotated')
    }
    await send(
      '  TWAMM set_authority (restore)',
      [await twammSetAuthority(pools.twamm, temp.publicKey, authority.publicKey)],
      [temp],
    )
    if (!decodeTwammPool(await readRow(pools.twamm)).authority.equals(authority.publicKey)) {
      throw new Error('authority was not restored')
    }
    return 'rotated and restored'
  })

  // ---- Compliance rails and token layer -----------------------------
  await attempt('KYC revoke → re-issue', async () => {
    const config = decodeKycConfig(await readRow(kycConfig()))
    const attestation = ata(user, config.kycMint, TOKEN_2022_PROGRAM_ID)
    await send('  KYC revoke', [await kycRevoke(authority.publicKey, user)])
    if ((await tokenBalance(attestation)) === 1n) throw new Error('revoke left the attestation in place')
    await send('  KYC issue', [await kycIssue(authority.publicKey, user)])
    if ((await tokenBalance(attestation)) !== 1n) throw new Error('re-issue did not restore it')
    return 'balance 1 → 0 → 1'
  })

  await attempt('KYC verify_kyc (the gate\u2019s own check)', async () => {
    const kyc = decodeKycConfig(await readRow(kycConfig()))
    await send('  KYC verify_kyc (approved wallet)', [await kycVerify(authority.publicKey)])
    await send('  KYC revoke (authority\u2019s own attestation)', [
      await kycRevoke(authority.publicKey, authority.publicKey),
    ])
    const rejection = await expectRejected('  KYC verify_kyc (revoked wallet)', [
      await kycVerify(authority.publicKey),
    ])
    await send('  KYC issue (restore)', [await kycIssue(authority.publicKey, authority.publicKey)])
    const attestation = ata(authority.publicKey, kyc.kycMint, TOKEN_2022_PROGRAM_ID)
    if ((await tokenBalance(attestation)) !== 1n) throw new Error('re-issue did not restore the attestation')
    await send('  KYC verify_kyc (approved again)', [await kycVerify(authority.publicKey)])
    return `accepted → refused (${rejection}) → accepted`
  })

  await attempt('KYC set_authority rotate → restore', async () => {
    const temp = Keypair.generate()
    await send('  KYC set_authority (temp)', [await kycSetAuthority(authority.publicKey, temp.publicKey)])
    await send(
      '  KYC set_authority (restore)',
      [await kycSetAuthority(temp.publicKey, authority.publicKey)],
      [temp],
    )
    if (!decodeKycConfig(await readRow(kycConfig())).authority.equals(authority.publicKey)) {
      throw new Error('KYC authority was not restored')
    }
    return 'rotated and restored'
  })

  await attempt('transfer_hook set_paused true → false', async () => {
    await send('  transfer_hook set_paused (true)', [await transferHookSetPaused(authority.publicKey, true)])
    await send('  transfer_hook set_paused (false)', [await transferHookSetPaused(authority.publicKey, false)])
    return 'paused true → false'
  })

  await attempt('transfer_hook set_authority rotate → restore', async () => {
    const temp = Keypair.generate()
    await send('  transfer_hook set_authority (temp)', [
      await transferHookSetAuthority(authority.publicKey, temp.publicKey),
    ])
    await send(
      '  transfer_hook set_authority (restore)',
      [await transferHookSetAuthority(temp.publicKey, authority.publicKey)],
      [temp],
    )
    return 'rotated and restored'
  })

  await attempt('RWA update_transfer_fee (25 bps → 0)', async () => {
    const mint = new PublicKey(created.mints.rwa)
    await send('  RWA update_transfer_fee (25 bps)', [
      await rwaUpdateTransferFee(mint, authority.publicKey, 25, 1_000_000n),
    ])
    await send('  RWA update_transfer_fee (restore 0)', [
      await rwaUpdateTransferFee(mint, authority.publicKey, 0, 0n),
    ])
    return 'fee 25 bps / max 1.000000 → 0 / 0'
  })

  // ---- Settlement: settle and cancel on their own escrows -----------
  const settleTradeId = 9001n
  if (await accountExists(settlementVault(settleTradeId))) {
    recordSkip('Settlement settle (T+N release)', `escrow ${settleTradeId} already used`)
  } else {
    await attempt('Settlement settle (T+N release, real secp256k1 signature)', async () => {
      const amount = 25n * ONE_TOKEN
      const source = await ensureAta(authority.publicKey, mintA, TOKEN_PROGRAM_ID)
      await ensureAta(user, mintA, TOKEN_PROGRAM_ID)
      const slot = await connection.getSlot('confirmed')
      await send('  settlement initiate_settlement (unlocks in 2 slots)', [
        await settlementInitiate({
          tradeId: settleTradeId,
          settlementSlots: BigInt(slot) + 2n,
          amount,
          authority: authority.publicKey,
          sourceTokenAccount: source,
          tokenMint: mintA,
          taker: user,
          pool: null,
          tokenProgramId: TOKEN_PROGRAM_ID,
        }),
      ])
      await waitForSlot(BigInt(slot) + 2n)
      const signed = signAgentMessage(settleTradeId, user, mintA, amount)
      await send('  settlement settle', [
        await settlementSettle(
          settleTradeId,
          mintA,
          ata(user, mintA, TOKEN_PROGRAM_ID),
          signed.signature,
          signed.recoveryId,
          TOKEN_PROGRAM_ID,
        ),
      ])
      const vault = decodeSettlementVault(await readRow(settlementVault(settleTradeId)))
      if (vault.status !== 'settled') throw new Error(`vault status is ${vault.status}`)
      return `released ${amount} mUSDC to the taker`
    })
  }

  const cancelTradeId = 9002n
  if (await accountExists(settlementVault(cancelTradeId))) {
    recordSkip('Settlement cancel (compliance refund)', `escrow ${cancelTradeId} already used`)
  } else {
    await attempt('Settlement cancel (compliance refund)', async () => {
      const amount = 15n * ONE_TOKEN
      const source = await ensureAta(authority.publicKey, mintA, TOKEN_PROGRAM_ID)
      const slot = await connection.getSlot('confirmed')
      await send('  settlement initiate_settlement (to be cancelled)', [
        await settlementInitiate({
          tradeId: cancelTradeId,
          settlementSlots: BigInt(slot) + 500n,
          amount,
          authority: authority.publicKey,
          sourceTokenAccount: source,
          tokenMint: mintA,
          taker: user,
          pool: null,
          tokenProgramId: TOKEN_PROGRAM_ID,
        }),
      ])
      const before = (await tokenBalance(source)) ?? 0n
      await send('  settlement cancel_settlement', [
        await settlementCancel({
          tradeId: cancelTradeId,
          tokenMint: mintA,
          authority: authority.publicKey,
          refundTokenAccount: source,
          tokenProgramId: TOKEN_PROGRAM_ID,
        }),
      ])
      const vault = decodeSettlementVault(await readRow(settlementVault(cancelTradeId)))
      if (vault.status !== 'cancelled') throw new Error(`vault status is ${vault.status}`)
      const refunded = ((await tokenBalance(source)) ?? 0n) - before
      return `refunded ${refunded} mUSDC to the payer`
    })
  }

  // ---- Hooks program: only reachable through a pool's CPI -----------
  if (wiring.settlement) {
    const routedTradeId = routeTradeId(true)
    await attempt('CLMM swap routed into T+N escrow (hooks::after_swap)', async () => {
      await send('  CLMM swap with settlement routing', [
        await clmmSwap({
          user: authority.publicKey,
          mintA,
          mintB,
          amountIn: 2n * ONE_TOKEN,
          minAmountOut: 1n,
          isAToB: true,
          tradeId: routedTradeId,
          // The narrow ±128 band was opened above, so an A→B walk now crosses
          // -128 before -6400 and must list the ticks in that order.
          ticks: [clmmNarrowLower, CLMM_LOWER_TICK],
          wiring,
        }),
      ])
      const vault = decodeSettlementVault(await readRow(settlementVault(routedTradeId)))
      if (vault.status !== 'pending') throw new Error(`vault status is ${vault.status}`)
      return `escrowed ${vault.amount} mTSLA for the taker`
    })
  } else {
    recordSkip(
      'CLMM swap routed into T+N escrow (hooks::after_swap)',
      'pool has no settlement rail (run with --rails oracle,settlement)',
    )
  }
}

// -----------------------------------------------------------------------
// Manifest
// -----------------------------------------------------------------------

function printManifest(options: Options, context: Context, tradeId: bigint) {
  const manifest = {
    cluster: options.cluster,
    rpc: options.url,
    authority: authority.publicKey.toBase58(),
    demoWallet: context.user.toBase58(),
    rails: [...options.rails],
    programs: Object.fromEntries(PROGRAMS.map(([name, id]) => [name, id.toBase58()])),
    mints: created.mints,
    accounts: created.accounts,
    pools: created.pools,
    tradeId: tradeId.toString(),
    transferAgentKey: DEMO_AGENT_PUBKEY.toString('hex'),
    whitelist: created.whitelist,
    ui: {
      mintsForPoolPages: [created.mints.musdc, created.mints.mtsla],
      rwaMintForAssetsAndAudit: created.mints.rwa,
      settlementPage: { tradeId: tradeId.toString(), taker: context.user.toBase58() },
      whitelistProofsForAssetsTransfer: created.whitelist.proofs,
    },
    swaps: created.swaps,
    exercises: created.exercises,
  }

  if (options.json) {
    process.stdout.write(`${JSON.stringify(manifest, null, 2)}\n`)
    return manifest
  }

  const rows: [string, string][] = [
    ['cluster', `${options.cluster} (${options.url})`],
    ['authority / upgrade authority', authority.publicKey.toBase58()],
    ['demo wallet — connect the UI with this', context.user.toBase58()],
    ['', ''],
    ['mint A — mUSDC (paste into the pool pages)', created.mints.musdc],
    ['mint B — mTSLA', created.mints.mtsla],
    ['RWA mint — Token-2022 (Assets / Audit)', created.mints.rwa],
    ['', ''],
    ['CLMM pool', created.pools.clmm],
    ['DLMM pool', created.pools.dlmm],
    ['Maverick pool', created.pools.maverick],
    ['TWAMM pool', created.pools.twamm],
    ['TWAMM order 1', created.accounts.twammOrder],
    ['Order book (paste into Orders)', created.accounts.orderBook],
    ['Order book orders', `${created.accounts.orderBookAsk} / ${created.accounts.orderBookBid}`],
    ['', ''],
    ['KYC config / attestation mint', `${created.accounts.kycConfig} / ${created.accounts.kycMint}`],
    ['Whitelist config', created.accounts.transferHookConfig],
    ['Settlement config', created.accounts.settlementConfig],
    [`Escrow trade ${tradeId} — taker`, created.accounts.settlementVault],
  ]

  process.stdout.write('\n')
  for (const [label, value] of rows) {
    process.stdout.write(label === '' ? '\n' : `  ${label.padEnd(42)} ${value}\n`)
  }
  return manifest
}

// -----------------------------------------------------------------------
// Entry point
// -----------------------------------------------------------------------

async function main() {
  const options = parseArgs(process.argv.slice(2))
  let userKey: PublicKey | undefined
  if (options.user) {
    try {
      userKey = new PublicKey(options.user)
    } catch {
      die(`--user is not a valid base58 pubkey: ${options.user}`)
    }
  }

  connection = new Connection(options.url, 'confirmed')
  authority = loadKeypair(options.payerPath)

  log('Preflight')
  step(`payer / authority: ${authority.publicKey.toBase58()} (${options.payerPath})`)
  await preflight(options)
  if ((await connection.getBalance(authority.publicKey)) < 2 * LAMPORTS_PER_SOL) {
    step('low balance, airdropping')
    await ensureSol(authority.publicKey, 20)
  }
  step(`balance ${((await connection.getBalance(authority.publicKey)) / LAMPORTS_PER_SOL).toFixed(2)} SOL`)

  const state = loadState()
  let counterparty: PublicKey
  if (state.counterparty) {
    counterparty = new PublicKey(state.counterparty)
  } else {
    counterparty = Keypair.generate().publicKey
    state.counterparty = counterparty.toBase58()
  }

  const context: Context = {
    options,
    user: userKey ?? authority.publicKey,
    counterparty,
    state,
    mintA: PublicKey.default,
    mintB: PublicKey.default,
  }

  log('Funding the demo wallet')
  await ensureSol(context.user, SOL_TARGET)
  step(`${context.user.toBase58()} holds ${((await connection.getBalance(context.user)) / LAMPORTS_PER_SOL).toFixed(2)} SOL`)

  await seedTokens(context)
  await seedKyc(context)
  // The settlement escrow authority is a member of the same tree, and that is
  // the whole point of it being one fixed PDA: the hook checks both sides of
  // every transfer, so an escrow needs an entry too - but this one entry covers
  // every trade ID, instead of one entry (and one root rotation) per trade.
  await seedTransferHook([
    authority.publicKey,
    context.user,
    context.counterparty,
    // Both sides of a settlement-routed RWA transfer: the hooks custody
    // account is the source (owned by the hook authority), and the trade's
    // escrow is the destination (owned by the escrow authority). Whichever
    // side is missing makes the hook reject the move.
    hookAuthority(),
    settlementEscrowAuthority(),
  ])
  created.accounts.settlementEscrowAuthority = settlementEscrowAuthority().toBase58()
  await seedRwaMint(context)
  await seedRails()
  await seedPools(context)
  await seedOrderBook(context)
  const tradeId = await seedEscrow(context)
  await verify(context)

  context.state = { ...context.state, cluster: options.cluster, user: context.user.toBase58() }
  const manifest = printManifest(options, context, tradeId)

  mkdirSync(join(REPO_ROOT, '.anchor'), { recursive: true })
  writeFileSync(STATE_FILE, `${JSON.stringify(context.state, null, 2)}\n`)
  writeFileSync(join(REPO_ROOT, '.anchor', 'seed-localnet.manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`)

  if (options.verify) {
    const passed = created.exercises.filter((item) => item.status === 'ok').length
    const failed = created.exercises.filter((item) => item.status === 'fail')
    const skipped = created.exercises.filter((item) => item.status === 'skip').length
    log(`Functionality suite: ${passed} passed, ${failed.length} failed, ${skipped} skipped`)
    for (const failure of failed) step(`\x1b[1;31m✗\x1b[0m ${failure.name}: ${failure.detail}`)
    if (failed.length > 0) process.exitCode = 1
  }

  log(`Done. State: ${STATE_FILE}`)
  log(`Start the UI: bun run dev — connect ${context.user.toBase58()} on localnet`)
}

main().catch((cause) => {
  die(describeError(cause))
})
