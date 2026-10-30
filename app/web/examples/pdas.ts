#!/usr/bin/env bun
/**
 * Prints the custody accounts a hooked mint's whitelist has to contain, so they
 * can be registered *before* the pool, the book or the order is created — the
 * transfer hook checks the owner of both token accounts on every transfer, and a
 * missing entry fails before the hook is even reached.
 *
 *   bun examples/pdas.ts                       # the fixed accounts
 *   bun examples/pdas.ts --pair <BASE> <QUOTE>  # plus the pools and the book
 *   bun examples/pdas.ts --pair <BASE> <QUOTE> --maker <WALLET> \
 *     --order-id <ID> --escrow-mint <MINT>     # plus that order and its escrow
 *
 * Pure derivation: no RPC, no keypair, nothing signed. Add the printed addresses
 * to the whitelist field on `KYC`, press **Update root**, then register each
 * entry — the root only covers the set it was built from.
 */
import {
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ata,
  clmmPool,
  dlmmPool,
  hookAuthority,
  kycConfig,
  kycMint,
  maverickPool,
  orderBook,
  orderBookOrder,
  settlementConfig,
  settlementEscrowAuthority,
  transferHookConfig,
  twammPool,
} from '@rwa-dex/sdk'
import { PublicKey } from '@solana/web3.js'

const USAGE = `Usage: bun examples/pdas.ts [options]

Prints PDA addresses for the whitelist steps. No RPC, no keys.

Options:
  --pair <BASE> <QUOTE>   Add the four AMM pool PDAs and the order book
  --maker <PUBKEY>        Order owner (needs --pair and --order-id)
  --order-id <ID>         Order id, the u64 used in the order PDA
  --escrow-mint <MINT>    Also print that order's escrow token account
  -h, --help              This message

Examples:
  bun examples/pdas.ts
  bun examples/pdas.ts --pair <NAV_MINT> <TUSD_MINT>
  bun examples/pdas.ts --pair <BASE> <QUOTE> --maker <WALLET> \\
    --order-id 1700000000000 --escrow-mint <BASE>
`

interface Args {
  base?: PublicKey
  quote?: PublicKey
  maker?: PublicKey
  orderId?: bigint
  escrowMint?: PublicKey
}

function fail(message: string): never {
  console.error(`error: ${message}`)
  console.error('')
  console.error(USAGE.trimEnd())
  process.exit(1)
}

function pubkey(flag: string, value: string | undefined): PublicKey {
  if (value === undefined) fail(`${flag} needs a base58 address`)
  try {
    return new PublicKey(value)
  } catch {
    return fail(`${flag}: not a base58 address: ${value}`)
  }
}

function orderIdOf(value: string | undefined): bigint {
  if (value === undefined) fail('--order-id needs a number')
  if (!/^\d+$/.test(value)) fail(`--order-id must be an unsigned integer, got: ${value}`)
  const id = BigInt(value)
  if (id > 2n ** 64n - 1n) fail('--order-id must fit in a u64')
  return id
}

function parse(argv: string[]): Args {
  const args: Args = {}

  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i]
    switch (flag) {
      case '-h':
      case '--help':
        console.log(USAGE.trimEnd())
        process.exit(0)
        break
      case '--pair': {
        const base = pubkey('--pair', argv[i + 1])
        const quote = pubkey('--pair', argv[i + 2])
        args.base = base
        args.quote = quote
        i += 2
        break
      }
      case '--maker':
        args.maker = pubkey('--maker', argv[i + 1])
        i += 1
        break
      case '--order-id':
        args.orderId = orderIdOf(argv[i + 1])
        i += 1
        break
      case '--escrow-mint':
        args.escrowMint = pubkey('--escrow-mint', argv[i + 1])
        i += 1
        break
      default:
        fail(`unknown flag: ${flag}`)
    }
  }

  if ((args.maker !== undefined || args.orderId !== undefined) && (args.base === undefined || args.quote === undefined)) {
    fail('--maker and --order-id need --pair <BASE> <QUOTE>: the order PDA is seeded from the book')
  }
  if (args.orderId !== undefined && args.maker === undefined) {
    fail('--order-id needs --maker: the order PDA is seeded from its owner')
  }
  if (args.escrowMint !== undefined && args.orderId === undefined) {
    fail('--escrow-mint needs --maker and --order-id: the escrow is derived from the order')
  }

  return args
}

function section(title: string) {
  console.log('')
  console.log(title)
}

function row(label: string, address: PublicKey) {
  console.log(`  ${label.padEnd(32)} ${address.toBase58()}`)
}

const args = parse(process.argv.slice(2))

section('Fixed accounts')
row('hook authority', hookAuthority())
row('settlement escrow authority', settlementEscrowAuthority())
row('KYC config', kycConfig())
row('KYC mint', kycMint())
row('transfer-hook config', transferHookConfig())
row('settlement config', settlementConfig())

if (args.base !== undefined && args.quote !== undefined) {
  const base = args.base
  const quote = args.quote

  section('Pool PDAs (owned by every vault they hold)')
  row('CLMM pool', clmmPool(base, quote))
  row('DLMM pool', dlmmPool(base, quote))
  row('Maverick pool', maverickPool(base, quote))
  row('TWAMM pool', twammPool(base, quote))

  const book = orderBook(base, quote)
  section('Order book')
  row('book', book)

  if (args.maker !== undefined && args.orderId !== undefined) {
    const order = orderBookOrder(book, args.maker, args.orderId)
    section(`Order ${args.orderId}`)
    row('order', order)

    if (args.escrowMint !== undefined) {
      row('escrow (Token-2022)', ata(order, args.escrowMint, TOKEN_2022_PROGRAM_ID))
      row('escrow (SPL Token)', ata(order, args.escrowMint, TOKEN_PROGRAM_ID))
    }
  }
}

console.log('')
console.log('Register each address above on KYC → Transfer-hook whitelist, then')
console.log('Update root before registering the entries for that root.')
