import { describe, expect, test } from 'bun:test'
import {
  ComputeBudgetProgram,
  Keypair,
  PublicKey,
  Transaction,
  type Connection,
} from '@solana/web3.js'
import {
  CLMM_PROGRAM_ID,
  ORDER_BOOK_PROGRAM_ID,
  ORDER_LEN,
  TOKEN_2022_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  ata,
  buildWhitelist,
  buildWhitelistTree,
  clmmPool,
  createClient,
  keypairWallet,
  kycConfig,
  kycMint,
  orderBook,
  orderBookOrder,
  railWiring,
} from '@rwa-dex/sdk'
import { clmmSwap } from '@rwa-dex/sdk'

/**
 * A connection that answers what the send path needs and records the raw
 * transaction. It proves the SDK's send/confirm wiring without a validator:
 * the same instructions the app would send go through `sendRawTransaction`.
 */
function fakeConnection() {
  const sent: Uint8Array[] = []
  const simulatedPrograms: string[] = []
  const queries: Array<{ programId: string; filters: unknown }> = []

  const connection = {
    rpcEndpoint: 'http://fake.local',
    getLatestBlockhash: async () => ({
      blockhash: '11111111111111111111111111111111',
      lastValidBlockHeight: 1_000,
    }),
    sendRawTransaction: async (raw: Uint8Array) => {
      sent.push(raw)
      return 'fake-signature'
    },
    confirmTransaction: async () => ({ context: { slot: 1 }, value: { err: null } }),
    getAccountInfo: async () => null,
    simulateTransaction: async (tx: Transaction) => {
      for (const ix of tx.instructions) simulatedPrograms.push(ix.programId.toBase58())
      return {
        context: { slot: 1 },
        value: { err: null, logs: ['Program log: ok'], unitsConsumed: 4242 },
      }
    },
    getProgramAccounts: async (programId: PublicKey, config?: { filters?: unknown }) => {
      queries.push({ programId: programId.toBase58(), filters: config?.filters })
      return []
    },
  } as unknown as Connection

  return { connection, sent, simulatedPrograms, queries }
}

/** An `Order` account body, written field for field as the program writes it. */
function encodeOrder(order: {
  book: PublicKey
  owner: PublicKey
  orderId: bigint
  side: number
  price: bigint
  originalAmount: bigint
  remainingAmount: bigint
  filledBase: bigint
  filledQuote: bigint
  createdAt: bigint
  expiryTimestamp: bigint
}): Uint8Array {
  const data = new Uint8Array(ORDER_LEN)
  data.set([1, 2, 3, 4, 5, 6, 7, 8], 0)
  data.set(order.book.toBytes(), 8)
  data.set(order.owner.toBytes(), 40)
  const view = new DataView(data.buffer)
  view.setBigUint64(72, order.orderId, true)
  data[80] = order.side
  view.setBigUint64(81, order.price, true)
  view.setBigUint64(89, order.originalAmount, true)
  view.setBigUint64(97, order.remainingAmount, true)
  view.setBigUint64(105, order.filledBase, true)
  view.setBigUint64(113, order.filledQuote, true)
  view.setBigInt64(121, order.createdAt, true)
  view.setBigInt64(129, order.expiryTimestamp, true)
  return data
}

describe('sdk client', () => {
  const kp = Keypair.generate()
  const mintA = Keypair.generate().publicKey
  const mintB = Keypair.generate().publicKey

  const swapArgs = (user: PublicKey) => ({
    user,
    mintA,
    mintB,
    amountIn: 1_000_000n,
    minAmountOut: 0n,
    isAToB: true,
    tradeId: 0n,
    ticks: [] as number[],
    wiring: { kycConfig: false, oracle: false, settlement: false },
    tokenProgramId: TOKEN_PROGRAM_ID,
  })

  test('builds, signs, sends and confirms through the module actions', async () => {
    const { connection, sent } = fakeConnection()
    const dex = createClient({ connection, wallet: keypairWallet(kp) })

    const result = await dex.clmm.swap(swapArgs(kp.publicKey))

    expect(result.signature).toBe('fake-signature')
    expect(result.blockhash).toBe('11111111111111111111111111111111')
    expect(sent).toHaveLength(1)
    expect(dex.publicKey?.equals(kp.publicKey)).toBe(true)
  })

  test('throws before building when no wallet is connected', async () => {
    const { connection } = fakeConnection()
    const dex = createClient({ connection })
    await expect(dex.clmm.swap(swapArgs(kp.publicKey))).rejects.toThrow(/no wallet connected/)
  })

  test('exposes the raw builders under .build, identical to the core encoder', async () => {
    const { connection } = fakeConnection()
    const dex = createClient({ connection })

    const ix = await dex.clmm.build.swap(swapArgs(kp.publicKey))
    const direct = await clmmSwap(swapArgs(kp.publicKey))

    expect(ix.programId.equals(CLMM_PROGRAM_ID)).toBe(true)
    // The pool PDA is the first account of a CLMM swap.
    expect(ix.keys[0].pubkey.equals(clmmPool(mintA, mintB))).toBe(true)
    expect(Buffer.from(ix.data).equals(Buffer.from(direct.data))).toBe(true)
  })

  test('simulates with the same compute budget as a send, without sending', async () => {
    const { connection, sent, simulatedPrograms } = fakeConnection()
    const dex = createClient({ connection, wallet: keypairWallet(kp) })

    const ix = await dex.clmm.build.swap(swapArgs(kp.publicKey))
    const result = await dex.simulate(ix)

    expect(result.err).toBeNull()
    expect(result.logs).toEqual(['Program log: ok'])
    expect(result.unitsConsumed).toBe(4242)
    expect(sent).toHaveLength(0)
    // A swap can exceed the default 200k budget; simulating without headroom
    // would report a healthy instruction as failing.
    expect(simulatedPrograms[0]).toBe(ComputeBudgetProgram.programId.toBase58())
    expect(simulatedPrograms).toContain(CLMM_PROGRAM_ID.toBase58())
  })

  test('an on-chain failure is surfaced, not reported as confirmed', async () => {
    const { connection } = fakeConnection()
    const failing = {
      ...connection,
      confirmTransaction: async () => ({
        context: { slot: 1 },
        value: { err: { InstructionError: [0, { Custom: 6000 }] } },
      }),
    } as unknown as Connection
    const dex = createClient({ connection: failing, wallet: keypairWallet(kp) })

    await expect(dex.clmm.swap(swapArgs(kp.publicKey))).rejects.toThrow(/failed/)
  })
})

describe('sdk module gaps', () => {
  const kp = Keypair.generate()

  test('rwa.createMint co-signs with the new mint keypair', async () => {
    const { connection, sent } = fakeConnection()
    const signed: string[] = []
    const wallet = {
      publicKey: kp.publicKey,
      async signTransaction<T extends Transaction | { serialize(): Uint8Array }>(
        tx: T,
      ): Promise<T> {
        const transaction = tx as Transaction
        for (const signature of transaction.signatures) {
          signed.push(signature.publicKey.toBase58())
        }
        transaction.partialSign(kp)
        return tx
      },
    }
    const dex = createClient({ connection, wallet })
    const mint = Keypair.generate()

    await dex.rwa.createMint({
      mint,
      authority: kp.publicKey,
      params: {
        decimals: 6,
        transferHookProgram: Keypair.generate().publicKey,
        delegate: Keypair.generate().publicKey,
        feeAuthority: Keypair.generate().publicKey,
        name: 'Fund Share Class A',
        symbol: 'FSCA',
        uri: 'https://example.com/meta.json',
      },
    })

    expect(sent).toHaveLength(1)
    // The system program needs the mint account itself to sign its creation.
    expect(signed).toContain(mint.publicKey.toBase58())
    expect(signed).toContain(kp.publicKey.toBase58())
  })

  test('settlement.settle refuses a signature that is not 64 bytes', () => {
    const { connection } = fakeConnection()
    const dex = createClient({ connection, wallet: keypairWallet(kp) })

    expect(() =>
      dex.settlement.settle({
        tradeId: 1n,
        tokenMint: Keypair.generate().publicKey,
        takerTokenAccount: Keypair.generate().publicKey,
        signature: new Uint8Array(10),
        recoveryId: 0,
        tokenProgramId: TOKEN_PROGRAM_ID,
      }),
    ).toThrow(/64 bytes/)
  })

  test('kycAccountsFor takes the config the book stores', () => {
    const { connection } = fakeConnection()
    const dex = createClient({ connection })
    const trader = Keypair.generate().publicKey
    const custom = Keypair.generate().publicKey

    expect(dex.orders.kycAccountsFor(trader).config.equals(kycConfig())).toBe(true)
    expect(dex.orders.kycAccountsFor(trader, custom).config.equals(custom)).toBe(true)
    expect(
      dex.orders
        .kycAccountsFor(trader, custom)
        .attestation.equals(ata(trader, kycMint(), TOKEN_2022_PROGRAM_ID)),
    ).toBe(true)
  })

  test('bin array walk indexes widen around the bin and stay ascending', () => {
    const { connection } = fakeConnection()
    const dex = createClient({ connection })

    expect(dex.dlmm.binArrayIndexesFor(0)).toEqual([0])
    expect(dex.dlmm.binArrayIndexesFor(0, 1)).toEqual([-1, 0, 1])
    expect(dex.dlmm.binArrayIndexesFor(140, 1)).toEqual([1, 2, 3])
    expect(dex.maverick.binArrayIndexesFor(0, 1)).toEqual([-1, 0, 1])
  })

  test('orders.list filters by book and decodes the rows', async () => {
    const { connection, queries } = fakeConnection()
    const base = Keypair.generate().publicKey
    const quote = Keypair.generate().publicKey
    const book = orderBook(base, quote)
    const maker = Keypair.generate().publicKey
    const row = {
      pubkey: orderBookOrder(book, maker, 42n),
      account: {
        owner: ORDER_BOOK_PROGRAM_ID,
        data: encodeOrder({
          book,
          owner: maker,
          orderId: 42n,
          side: 0,
          price: 2_000_000_000n,
          originalAmount: 500n,
          remainingAmount: 300n,
          filledBase: 200n,
          filledQuote: 400n,
          createdAt: 1_700_000_000n,
          expiryTimestamp: 1_800_000_000n,
        }),
        lamports: 1,
      },
    }
    const querying = {
      ...connection,
      getProgramAccounts: async (programId: PublicKey, config: { filters?: unknown }) => {
        queries.push({ programId: programId.toBase58(), filters: config.filters })
        return [row]
      },
    } as unknown as Connection
    const dex = createClient({ connection: querying })

    const orders = await dex.orders.list(base, quote)

    expect(orders).toHaveLength(1)
    expect(orders[0].book.equals(book)).toBe(true)
    expect(orders[0].orderId).toBe(42n)
    expect(orders[0].side).toBe('ask')
    expect(orders[0].remainingAmount).toBe(300n)

    const query = queries.at(-1)
    expect(query?.programId).toBe(ORDER_BOOK_PROGRAM_ID.toBase58())
    expect(query?.filters).toEqual([
      { dataSize: ORDER_LEN },
      { memcmp: { offset: 8, bytes: book.toBase58() } },
    ])
  })
})

describe('sdk helpers', () => {
  test('derives an order PDA from the pair, maker and id', () => {
    const base = Keypair.generate().publicKey
    const quote = Keypair.generate().publicKey
    const maker = Keypair.generate().publicKey

    const book = orderBook(base, quote)
    const expectedBook = PublicKey.findProgramAddressSync(
      [new TextEncoder().encode('book'), base.toBytes(), quote.toBytes()],
      ORDER_BOOK_PROGRAM_ID,
    )[0]
    expect(book.equals(expectedBook)).toBe(true)
    expect(orderBookOrder(book, maker, 7n).equals(orderBookOrder(book, maker, 7n))).toBe(true)
    expect(orderBookOrder(book, maker, 7n).equals(orderBookOrder(book, maker, 8n))).toBe(false)
  })

  test('the KYC attestation is the KYC mint ATA under Token-2022', () => {
    const trader = Keypair.generate().publicKey
    const expected = PublicKey.findProgramAddressSync(
      [trader.toBytes(), TOKEN_2022_PROGRAM_ID.toBytes(), kycMint().toBytes()],
      new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL'),
    )[0]
    expect(ata(trader, kycMint(), TOKEN_2022_PROGRAM_ID).equals(expected)).toBe(true)
  })

  test('buildWhitelist returns the published root and byte proofs', () => {
    const owners = [
      Keypair.generate().publicKey,
      Keypair.generate().publicKey,
      Keypair.generate().publicKey,
    ]
    const tree = buildWhitelist(owners)
    const core = buildWhitelistTree(owners)

    expect(tree.rootHex).toBe(core.root)
    expect(tree.root).toHaveLength(32)
    expect(tree.proofFor(owners[0]).every((proof) => proof.length === 32)).toBe(true)
  })

  test('rail wiring reads the pool keys, default meaning off', () => {
    expect(
      railWiring({
        kycConfig: kycConfig(),
        oracleState: Keypair.generate().publicKey,
        hooksAuthority: Keypair.generate().publicKey,
      }),
    ).toEqual({ kycConfig: true, oracle: true, settlement: true })

    expect(
      railWiring({
        kycConfig: PublicKey.default,
        oracleState: PublicKey.default,
        hooksAuthority: PublicKey.default,
      }),
    ).toEqual({ kycConfig: false, oracle: false, settlement: false })
  })
})
