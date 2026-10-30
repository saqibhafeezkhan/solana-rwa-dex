/**
 * Localnet smoke test: proves that a seeded validator and `@rwa-dex/sdk` agree.
 *
 * It deploys nothing and seeds nothing. It reads the manifest
 * `bootstrap:localnet` wrote, verifies every program is deployed and every
 * seeded account decodes through the SDK, simulates a CLMM swap, and — unless
 * `--dry-run` — sends that swap and checks the T+N vault it created.
 *
 *   bun run smoke:localnet
 *   bun run smoke:localnet -- --dry-run
 *   bun run smoke:localnet -- --manifest ../../.anchor/seed-localnet.manifest.json
 *
 * Exit code is 0 only when every check passed.
 */
import { readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Connection, Keypair, PublicKey } from '@solana/web3.js'
import {
  RwaDexClient,
  TOKEN_PROGRAM_ID,
  ata,
  clmmPool,
  clmmTick,
  keypairWallet,
  randomTradeId,
} from '@rwa-dex/sdk'

interface SeedManifest {
  rpc: string
  authority: string
  demoWallet: string
  rails: string[]
  programs: Record<string, string>
  mints: { musdc: string; mtsla: string; rwa: string }
  accounts: {
    kycConfig: string
    kycMint: string
    kycAttestation: string
    transferHookConfig: string
    settlementConfig: string
    hooksAuthority: string
    settlementVault: string
  }
  pools: { clmm: string; dlmm: string; maverick: string; twamm: string }
  tradeId: string | number
  whitelist: { root: string; proofs?: Record<string, string> }
}

interface Options {
  manifest: string
  keypair: string
  rpc?: string
  dryRun: boolean
  amount: bigint
}

const DEFAULT_MANIFEST = fileURLToPath(
  new URL('../../../.anchor/seed-localnet.manifest.json', import.meta.url),
)

function parseArgs(argv: string[]): Options {
  const options: Options = {
    manifest: DEFAULT_MANIFEST,
    keypair: join(homedir(), '.config/solana/id.json'),
    dryRun: false,
    amount: 1_000_000n,
  }
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]
    if (arg === '--dry-run') options.dryRun = true
    else if (arg === '--manifest') options.manifest = argv[++i]
    else if (arg === '--keypair') options.keypair = argv[++i]
    else if (arg === '--rpc') options.rpc = argv[++i]
    else if (arg === '--amount') options.amount = BigInt(argv[++i])
    else if (arg === '-h' || arg === '--help') {
      console.log('usage: bun run smoke:localnet -- [--dry-run] [--manifest path] [--keypair path] [--rpc url] [--amount raw]')
      process.exit(0)
    } else throw new Error(`unknown argument: ${arg}`)
  }
  return options
}

const results: Array<{ name: string; ok: boolean; detail: string }> = []

async function check(name: string, fn: () => Promise<string> | string) {
  try {
    const detail = await fn()
    results.push({ name, ok: true, detail })
    console.log(`  \u2714 ${name}${detail ? ` \u2014 ${detail}` : ''}`)
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    results.push({ name, ok: false, detail })
    console.log(`  \u2718 ${name} \u2014 ${detail}`)
  }
}

function expect(condition: unknown, message: string): void {
  if (!condition) throw new Error(message)
}

const options = parseArgs(process.argv.slice(2))
const manifest = JSON.parse(readFileSync(options.manifest, 'utf8')) as SeedManifest
const rpc = options.rpc ?? manifest.rpc
const keypair = Keypair.fromSecretKey(
  Uint8Array.from(JSON.parse(readFileSync(options.keypair, 'utf8')) as number[]),
)

const connection = new Connection(rpc, 'confirmed')
const dex = new RwaDexClient({ connection, wallet: keypairWallet(keypair) })

const mintA = new PublicKey(manifest.mints.musdc)
const mintB = new PublicKey(manifest.mints.mtsla)
const demo = new PublicKey(manifest.demoWallet)
const kycMintAddress = new PublicKey(manifest.accounts.kycMint)
const poolAddress = clmmPool(mintA, mintB)

console.log(`smoke:localnet against ${rpc}`)
console.log(`  manifest ${options.manifest}`)
console.log(`  wallet   ${keypair.publicKey.toBase58()}${keypair.publicKey.equals(demo) ? ' (seeded demo wallet)' : ''}`)
if (!keypair.publicKey.equals(demo)) {
  console.log('  note: the seeded demo wallet is ' + demo.toBase58() + '; this wallet may hold no balances')
}

// ---------------------------------------------------------------- environment

await check('RPC answers getVersion', async () => {
  const version = await connection.getVersion()
  return `solana-core ${version['solana-core']}`
})

await check(`all ${Object.keys(manifest.programs).length} programs are deployed`, async () => {
  const missing: string[] = []
  for (const [name, address] of Object.entries(manifest.programs)) {
    const info = await connection.getAccountInfo(new PublicKey(address))
    if (!info?.executable) missing.push(name)
  }
  expect(missing.length === 0, `not executable: ${missing.join(', ')}`)
  return 'every .so loaded'
})

await check('mints decode with the seeded supply', async () => {
  const a = await dex.accounts.mint(mintA)
  const b = await dex.accounts.mint(mintB)
  expect(a.decimals === 6 && b.decimals === 6, `decimals ${a.decimals}/${b.decimals}, expected 6`)
  expect(a.supply > 0n && b.supply > 0n, 'mint supply is zero')
  return `mUSDC ${a.supply} / mTSLA ${b.supply} raw, 6 decimals`
})

await check('mints run under legacy SPL Token', async () => {
  const [a, b] = await Promise.all([
    dex.accounts.tokenProgram(mintA),
    dex.accounts.tokenProgram(mintB),
  ])
  expect(a.equals(TOKEN_PROGRAM_ID) && b.equals(TOKEN_PROGRAM_ID), 'expected TOKEN_PROGRAM_ID')
  return 'TOKEN_PROGRAM_ID'
})

// ------------------------------------------------------------- compliance rails

await check('KYC config matches the manifest', async () => {
  const config = await dex.accounts.kycConfig(new PublicKey(manifest.accounts.kycConfig))
  expect(config.kycMint.equals(kycMintAddress), 'kyc mint mismatch')
  expect(config.authority.equals(new PublicKey(manifest.authority)), 'kyc authority mismatch')
  return `mint ${config.kycMint.toBase58().slice(0, 8)}\u2026`
})

await check('attestation is the KYC mint ATA and holds one token', async () => {
  const attestation = ata(demo, kycMintAddress, dex.rwa.tokenProgram)
  expect(
    attestation.equals(new PublicKey(manifest.accounts.kycAttestation)),
    'derived attestation != manifest attestation',
  )
  const account = await dex.accounts.tokenAccount(attestation)
  expect(account.amount === 1n, `balance ${account.amount}, expected 1`)
  return `${account.amount} soulbound token`
})

await check('whitelist root and pause state decode', async () => {
  const config = await dex.accounts.hookConfig(new PublicKey(manifest.accounts.transferHookConfig))
  const root = Buffer.from(config.merkleRoot).toString('hex')
  expect(root === manifest.whitelist.root, `root ${root} != ${manifest.whitelist.root}`)
  expect(config.paused === false, 'transfer hook is paused')
  return `root ${root.slice(0, 12)}\u2026, unpaused`
})

await check('settlement config and the seeded vault decode', async () => {
  const config = await dex.settlement.config()
  expect(config.authority.equals(new PublicKey(manifest.authority)), 'config authority mismatch')
  expect(
    config.hooksAuthority.equals(new PublicKey(manifest.accounts.hooksAuthority)),
    'hooks authority mismatch',
  )
  const vault = await dex.settlement.vault(BigInt(manifest.tradeId))
  return `trade ${vault.tradeId} is ${vault.status}`
})

// ------------------------------------------------------------------ the pools

await check('all four pools decode against the seeded mints', async () => {
  const clmm = await dex.clmm.poolFor(mintA, mintB)
  const dlmm = await dex.dlmm.poolFor(mintA, mintB)
  const maverick = await dex.maverick.poolFor(mintA, mintB)
  const twamm = await dex.twamm.poolFor(mintA, mintB)

  for (const [name, pool] of [
    ['clmm', clmm],
    ['dlmm', dlmm],
    ['maverick', maverick],
    ['twamm', twamm],
  ] as const) {
    expect(pool.address.equals(new PublicKey(manifest.pools[name])), `${name} PDA != manifest address`)
    expect(pool.mintA.equals(mintA) && pool.mintB.equals(mintB), `${name} mint pair mismatch`)
  }
  expect(clmm.liquidity > 0n, 'clmm has no liquidity')
  expect(twamm.lpSupply > 0n, 'twamm has no LP shares')
  return `tick ${clmm.tickCurrent}, bins ${dlmm.activeBinId}/${maverick.activeBinId}, lp ${twamm.lpSupply}`
})

await check('pool wiring matches the seeded rails', async () => {
  const wiring = await dex.clmm.wiringFor(mintA, mintB)
  const expected = {
    kycConfig: manifest.rails.includes('kyc'),
    oracle: manifest.rails.includes('oracle'),
    settlement: manifest.rails.includes('settlement'),
  }
  expect(
    wiring.kycConfig === expected.kycConfig &&
      wiring.oracle === expected.oracle &&
      wiring.settlement === expected.settlement,
    `wiring ${JSON.stringify(wiring)} != expected ${JSON.stringify(expected)}`,
  )
  return `kyc=${wiring.kycConfig} oracle=${wiring.oracle} settlement=${wiring.settlement}`
})

// --------------------------------------------------------------- a real swap

const wiring = await dex.clmm.wiringFor(mintA, mintB)
const ticks: number[] = [-6_400]
try {
  await dex.accounts.account(clmmTick(poolAddress, -128))
  ticks.unshift(-128)
} catch {
  // the seeded narrow band was never opened on this cluster
}
const tradeId = wiring.settlement ? randomTradeId() : 0n
const amountIn = options.amount

const swapInstruction = await dex.clmm.build.swap({
  user: keypair.publicKey,
  mintA,
  mintB,
  amountIn,
  minAmountOut: 1n,
  isAToB: true,
  tradeId,
  ticks,
  wiring,
  tokenProgramId: TOKEN_PROGRAM_ID,
})

await check('CLMM swap simulates against seeded state', async () => {
  const result = await dex.simulate(swapInstruction, { feePayer: keypair.publicKey })
  expect(result.err === null, `simulation error ${JSON.stringify(result.err)}`)
  expect(result.logs !== null, 'no program logs returned')
  return `${result.unitsConsumed ?? '?'} CU, ${result.logs?.filter((line) => line.includes('Program log')).length ?? 0} program logs`
})

if (options.dryRun) {
  console.log('\n--dry-run: swap not sent')
} else {
  await check('CLMM swap lands and moves its output', async () => {
    const balanceOf = async (mint: PublicKey) => {
      try {
        return (await dex.accounts.tokenAccount(ata(keypair.publicKey, mint, TOKEN_PROGRAM_ID))).amount
      } catch {
        return 0n
      }
    }
    const beforeA = await balanceOf(mintA)

    const { signature } = await dex.send([swapInstruction])
    expect(signature.length > 0, 'no signature returned')

    const afterA = await balanceOf(mintA)
    expect(afterA < beforeA, `input balance did not drop (${beforeA} \u2192 ${afterA})`)

    let routed = 'direct payout'
    if (wiring.settlement) {
      const vault = await dex.settlement.vault(tradeId)
      expect(vault.taker.equals(keypair.publicKey), 'vault taker is not the trader')
      expect(vault.status === 'pending', `vault status is ${vault.status}`)
      expect(vault.amount > 0n, 'vault amount is zero')
      routed = `escrowed ${vault.amount} raw until slot ${vault.settlementSlot}`
    }
    return `${signature.slice(0, 12)}\u2026, spent ${beforeA - afterA}, ${routed}`
  })
}

// ---------------------------------------------------------------------- result

const failed = results.filter((result) => !result.ok)
console.log(
  `\n${results.length - failed.length}/${results.length} checks passed${
    failed.length > 0 ? `, ${failed.length} failed` : ''
  }`,
)
if (failed.length > 0) {
  process.exitCode = 1
}
