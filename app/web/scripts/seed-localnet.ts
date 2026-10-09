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
import {
  createAtaInstruction,
  describeError,
  sendWithSigners,
  type WalletLike,
} from '../src/core/connection'
import {
  CLMM_PROGRAM_ID,
  DLMM_PROGRAM_ID,
  HOOKS_PROGRAM_ID,
  KYC_PROGRAM_ID,
  MAVERICK_PROGRAM_ID,
  ORACLE_PROGRAM_ID,
  RWA_MINT_PROGRAM_ID,
  SETTLEMENT_PROGRAM_ID,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  TRANSFER_HOOK_PROGRAM_ID,
  TWAMM_PROGRAM_ID,
  ata,
  clmmPool,
  dlmmPool,
  dlmmPosition,
  hookAuthority,
  kycConfig,
  maverickPool,
  maverickPosition,
  oracleState,
  settlementConfig,
  settlementVault,
  sqrtPriceX64FromPrice,
  transferHookConfig,
  transferHookExtraAccountMetaList,
  twammOrder,
  twammPool,
  type ClusterName,
} from '../src/core/pdas'
import { clmmAddLiquidity, clmmInitializePool, clmmSwap } from '../src/core/instructions-clmm'
import { dlmmAddLiquidity, dlmmInitializePool, dlmmSwap } from '../src/core/instructions-dlmm'
import {
  maverickAddLiquidity,
  maverickInitializePool,
  maverickSwap,
} from '../src/core/instructions-maverick'
import {
  twammCreateLongTermOrder,
  twammExecuteVirtualOrders,
  twammInitializePool,
} from '../src/core/instructions-twamm'
import {
  hooksFundHookAuthority,
  kycInitializeMint,
  kycIssue,
  rwaCreateMint,
  rwaMintTo,
  rwaTransfer,
  settlementInitialize,
  settlementInitiate,
  settlementSetHooksAuthority,
  settlementSetTransferAgent,
  transferHookInitExtraAccountMetaList,
  transferHookInitialize,
  transferHookUpdateRoot,
} from '../src/core/instructions-rails'
import {
  decodeClmmPool,
  decodeDlmmPool,
  decodeKycConfig,
  decodeMaverickPool,
  decodeMint,
  decodeOracleState,
  decodeSettlementConfig,
  decodeSettlementVault,
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

/** The ten programs the UI talks to, matching `scripts/programs.sh`. */
const PROGRAMS: [string, PublicKey][] = [
  ['oracle', ORACLE_PROGRAM_ID],
  ['kyc_gate', KYC_PROGRAM_ID],
  ['transfer_hook', TRANSFER_HOOK_PROGRAM_ID],
  ['rwa_mint', RWA_MINT_PROGRAM_ID],
  ['settlement', SETTLEMENT_PROGRAM_ID],
  ['hooks', HOOKS_PROGRAM_ID],
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
// leaf = sha256(0x00 || owner), node = sha256(0x01 || min || max). The hook folds
// each proof with its own hash, so deriving the root by folding keeps the client
// and the program provably in step.
// -----------------------------------------------------------------------

const LEAF_PREFIX = new Uint8Array([0])
const NODE_PREFIX = new Uint8Array([1])

async function sha256(parts: Uint8Array[]): Promise<Uint8Array> {
  const total = parts.reduce((sum, part) => sum + part.length, 0)
  const buffer = new Uint8Array(total)
  let offset = 0
  for (const part of parts) {
    buffer.set(part, offset)
    offset += part.length
  }
  return new Uint8Array(await crypto.subtle.digest('SHA-256', buffer))
}

const leafHash = (owner: PublicKey) => sha256([LEAF_PREFIX, owner.toBytes()])

async function nodeHash(left: Uint8Array, right: Uint8Array): Promise<Uint8Array> {
  const [a, b] = Buffer.compare(Buffer.from(left), Buffer.from(right)) <= 0 ? [left, right] : [right, left]
  return sha256([NODE_PREFIX, a, b])
}

/** Pads to an even width by duplicating the last leaf, then folds level by level. */
function pad(leaves: Uint8Array[]): Uint8Array[] {
  return leaves.length % 2 === 1 ? [...leaves, leaves[leaves.length - 1]] : [...leaves]
}

async function merkleRoot(leaves: Uint8Array[]): Promise<Uint8Array> {
  if (leaves.length === 0) throw new Error('a whitelist needs at least one member')
  let level = pad(leaves)
  while (level.length > 1) {
    const next: Uint8Array[] = []
    for (let i = 0; i < level.length; i += 2) next.push(await nodeHash(level[i], level[i + 1]))
    level = next
  }
  return level[0]
}

/** The sibling path for one leaf of the same tree `merkleRoot` builds. */
async function merkleProof(leaves: Uint8Array[], index: number): Promise<Uint8Array[]> {
  let level = pad(leaves)
  let cursor = index
  const proof: Uint8Array[] = []
  while (level.length > 1) {
    proof.push(level[cursor % 2 === 0 ? cursor + 1 : cursor - 1])
    const next: Uint8Array[] = []
    for (let i = 0; i < level.length; i += 2) next.push(await nodeHash(level[i], level[i + 1]))
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
  const leaves = await Promise.all(whitelist.map(leafHash))
  const root = await merkleRoot(leaves)
  for (const [index, wallet] of whitelist.entries()) {
    const proof = await merkleProof(leaves, index)
    const hex = proof.map((element) => Buffer.from(element).toString('hex')).join(',')
    created.whitelist.proofs[wallet.toBase58()] = hex
    step(`whitelist member ${wallet.toBase58()}`)
    step(`  proof: ${hex || '(single-member tree: empty proof)'}`)
  }
  created.whitelist.root = Buffer.from(root).toString('hex')

  const config = await readRow(transferHookConfig())
  const onChainRoot = config.data.subarray(8 + 32, 8 + 64)
  if (Buffer.from(onChainRoot).equals(Buffer.from(root))) {
    step('whitelist root already published')
    return
  }
  await send('publish whitelist root', [await transferHookUpdateRoot(authority.publicKey, root)])
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
// -----------------------------------------------------------------------

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
  const amountIn = 10n * ONE_TOKEN
  const beforeB = (await tokenBalance(ata(authority.publicKey, mintB, TOKEN_PROGRAM_ID))) ?? 0n

  const trade = async (pool: string, build: () => Promise<TransactionInstruction>) => {
    const instruction = await build()
    const signature = await send(`${pool} swap (A→B, ${amountIn} raw)`, [instruction])
    created.swaps.push({ pool, amountIn: amountIn.toString(), signature })
  }

  await trade('CLMM', () =>
    clmmSwap({
      user: authority.publicKey,
      mintA,
      mintB,
      amountIn,
      minAmountOut: 1n,
      isAToB: true,
      tradeId: 0n,
      ticks: [CLMM_LOWER_TICK],
      wiring,
    }),
  )
  await trade('DLMM', () =>
    dlmmSwap({
      user: authority.publicKey,
      mintA,
      mintB,
      amountIn,
      minAmountOut: 1n,
      isAToB: true,
      tradeId: 0n,
      binArrayIndexes: [0n],
      wiring,
    }),
  )
  await trade('Maverick', () =>
    maverickSwap({
      user: authority.publicKey,
      mintA,
      mintB,
      amountIn,
      minAmountOut: 1n,
      isAToB: true,
      tradeId: 0n,
      binArrayIndexes: [0n],
      wiring,
    }),
  )

  const afterB = (await tokenBalance(ata(authority.publicKey, mintB, TOKEN_PROGRAM_ID))) ?? 0n
  const clmmAfter = decodeClmmPool(await readRow(clmmAddress))
  step(`received ${afterB - beforeB} mTSLA raw for ${3n * amountIn} mUSDC raw across three pools`)
  step(`CLMM tick moved ${clmm.tickCurrent} → ${clmmAfter.tickCurrent}`)

  await verifyRwaTransfer(context)
}

/**
 * Moves one RWA token between two whitelisted owners with the UI's `rwaTransfer`
 * builder. This is the only path where the client's proof encoding has to match
 * `transfer_hook::root_from_proof` byte for byte, and a mismatch shows up as
 * `SourceNotWhitelisted` / `DestinationNotWhitelisted` rather than as a silently
 * wrong transfer.
 */
async function verifyRwaTransfer(context: Context) {
  log('Transferring RWA tokens through the UI instruction builders')
  const mint = new PublicKey(created.mints.rwa)
  const source = ata(authority.publicKey, mint, TOKEN_2022_PROGRAM_ID)
  const destination = ata(context.counterparty, mint, TOKEN_2022_PROGRAM_ID)
  const amount = ONE_TOKEN

  const proofs = (owner: PublicKey) => {
    const hex = created.whitelist.proofs[owner.toBase58()]
    if (hex === undefined) throw new Error(`no whitelist proof for ${owner.toBase58()}`)
    return hex.length === 0 ? [] : hex.split(',').map((element) => Uint8Array.from(Buffer.from(element, 'hex')))
  }

  const before = await tokenBalance(destination)
  await send('RWA transfer (two whitelisted owners, dual Merkle proofs)', [
    await rwaTransfer({
      source,
      destination,
      mint,
      authority: authority.publicKey,
      amount,
      sourceProof: proofs(authority.publicKey),
      destinationProof: proofs(context.counterparty),
    }),
  ])
  const after = await tokenBalance(destination)
  step(`counterparty RWA balance ${before ?? 0n} → ${after ?? 0n} (+${(after ?? 0n) - (before ?? 0n)})`)
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
  await seedTransferHook([authority.publicKey, context.user, context.counterparty])
  await seedRwaMint(context)
  await seedRails()
  await seedPools(context)
  const tradeId = await seedEscrow(context)
  await verify(context)

  context.state = { ...context.state, cluster: options.cluster, user: context.user.toBase58() }
  const manifest = printManifest(options, context, tradeId)

  mkdirSync(join(REPO_ROOT, '.anchor'), { recursive: true })
  writeFileSync(STATE_FILE, `${JSON.stringify(context.state, null, 2)}\n`)
  writeFileSync(join(REPO_ROOT, '.anchor', 'seed-localnet.manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`)

  log(`Done. State: ${STATE_FILE}`)
  log(`Start the UI: bun run dev — connect ${context.user.toBase58()} on localnet`)
}

main().catch((cause) => {
  die(describeError(cause))
})
