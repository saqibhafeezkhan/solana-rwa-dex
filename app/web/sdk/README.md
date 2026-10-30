# @rwa-dex/sdk

A typed TypeScript client for the RWA DEX programs. Framework-free: the same
call works in the React app, a Bun script, a test, or any Node process.

It exposes two layers:

1. **`RwaDexClient`** — one connection + one wallet, with a module per program.
   Actions build, sign, send and confirm; each module also has `.build` for the
   bare instruction when you want to compose your own transaction.
2. **The protocol surface** — every program ID, PDA derivation, instruction
   builder, account decoder, Merkle whitelist helper and on-chain walk
   derivation, in `src/core` (the only encoders the UI imports, so the two can
   never drift in account order or seeds).

The package is self-contained: the encoders, PDAs and decoders live in
`sdk/src/core`, and nothing here imports the web app.

## Install and build

```bash
cd sdk
bun run build      # dist/index.js (bundled ESM) + dist/types (declarations)
bun run typecheck  # strict typecheck of the source
npm pack           # inspect exactly what would be published
```

`@solana/web3.js` is a peer dependency (so consumers share one `PublicKey`),
`@noble/hashes` is a runtime dependency. Bun resolves the `"bun"` export
condition to `src/index.ts`, which is why tests and in-repo use run the source;
Node and every bundler read `dist/index.js`.

Inside this repository the app installs the package as a Bun workspace
(`"@rwa-dex/sdk": "workspace:*"`), so it imports the built `dist/` — exactly
what a published consumer gets. `bun run dev` and `bun run build` rebuild the SDK
first; after editing SDK source, run `bun run build:sdk` (or restart `dev`).
Browser consumers need the `Buffer` polyfill that web3.js v1 already requires:

```ts
import { Buffer } from 'buffer'
globalThis.Buffer ??= Buffer
```

## Layout

```text
sdk/
├── src/index.ts        public entry: client, modules, protocol surface
├── src/client.ts       RwaDexClient: send / simulate / wallet / module wiring
├── src/accounts.ts     typed account readers
├── src/queries.ts      program-account listings (orders, vaults)
├── src/programs/*.ts   one module per program (clmm … oracle)
└── src/core/*.ts       encoders, PDAs, decoders, walks, Merkle
```

Contents: [Quickstart](#quickstart) · [Client](#client) · [Modules](#modules) ·
[Swapping](#swapping-on-clmm) · [Orders](#resting-an-order) ·
[Settlement](#settlement) · [KYC & whitelist](#kyc-and-the-whitelist) ·
[RWA mints](#creating-an-rwa-mint) · [TWAMM keeper](#twamm-keeper) ·
[Reading state](#reading-state) · [Simulation](#simulating-and-failing-loudly) ·
[API reference](#api-reference) · [What's not included](#what-is-not-included) ·
[Conventions](#conventions)

## Quickstart

```ts
import { Connection, Keypair } from '@solana/web3.js'
import { RwaDexClient, keypairWallet } from '@rwa-dex/sdk'

const kp = Keypair.fromSecretKey(/* ... */)

const dex = new RwaDexClient({
  cluster: 'devnet',            // 'localnet' | 'devnet' | 'testnet' | 'mainnet-beta'
  wallet: keypairWallet(kp),
})

// Or bring your own endpoint / wallet provider:
const dex2 = new RwaDexClient({
  connection: new Connection('http://127.0.0.1:8899', 'confirmed'),
  wallet: { publicKey, signTransaction },  // Phantom/Backpack-style
  confirm: true,
})
```

`localnet` is the default, matching `Anchor.toml`. `bun run bootstrap:localnet`
deploys and seeds it.

## Client

`RwaDexClient` owns three things: the connection, the wallet, and the modules
bound to both.

| Option | Meaning |
| --- | --- |
| `cluster` | `localnet` (default), `devnet`, `testnet`, `mainnet-beta` — public RPC |
| `connection` | an explicit `Connection`; wins over `cluster` (custom RPC, Surfpool, a test double) |
| `wallet` | anything with `publicKey` and `signTransaction` (Phantom/Backpack-style); `keypairWallet(kp)` for scripts |
| `commitment` | `confirmed` by default |
| `confirm` | wait for confirmation on every send; default `true` |

Wallet lifecycle:

```ts
const injected = (window as any).phantom?.solana
dex.setWallet(injected)              // connect/disconnect from the app
dex.setWallet(null)
dex.setWallet(keypairWallet(kp))     // or a local keypair
dex.publicKey                        // PublicKey | null
```

The client does not prompt for a connection: whatever wallet you hand it is the
one that signs. Reads (`dex.accounts.*`, `dex.*.poolFor`, `wiringFor`, walks)
never need a wallet at all.

Two escape hatches cover everything the modules do not:

```ts
await dex.send([ix1, ix2], { signers: [extraKeypair] })  // any instructions
await dex.simulate(ix)                                   // logs, no send
```

`send` adds the 1.4M compute-unit budget the AMM walks need, signs with the
wallet plus any extra signers, and throws when the transaction lands with an
on-chain error — a resolved promise always means the state changed.

## Modules

| Module | Program | Highlights |
| --- | --- | --- |
| `dex.clmm` | concentrated liquidity | pool, positions over tick ranges, swaps, fees, breaker |
| `dex.dlmm` | bin AMM | pools, bin arrays, per-bin liquidity, swaps across arrays |
| `dex.maverick` | dynamic-distribution bins | bin kinds, `moveBins` keeper, hop arrays |
| `dex.twamm` | time-weighted AMM | LP shares, long-term orders, the permissionless crank |
| `dex.orders` | limit order book | books, resting asks/bids, fills, cancels |
| `dex.settlement` | T+N clearinghouse | initiate / settle (secp256k1) / cancel, per-mint agents |
| `dex.kyc` | KYC gate | soulbound attestations: issue / revoke / verify |
| `dex.rwa` | RWA mint factory | Token-2022 mints, hook-checked transfers, fees |
| `dex.hooks` | transfer hook + escrow router | whitelist root and entries, hook funding |
| `dex.oracle` | circuit breaker | owner-only `unpausePool`, oracle state |

## Swapping on CLMM

```ts
import { TOKEN_PROGRAM_ID } from '@rwa-dex/sdk'

const wiring = await dex.clmm.wiringFor(mintA, mintB) // kycConfig / oracle / settlement

const { signature } = await dex.clmm.swap({
  user: dex.publicKey!,
  mintA,
  mintB,
  amountIn: 1_000_000n,
  minAmountOut: 0n,
  isAToB: true,
  tradeId: 0n,                       // non-zero only when settlement is on
  ticks: [],                         // initialized ticks the walk may cross
  wiring,
  tokenProgramId: TOKEN_PROGRAM_ID,  // pass Token-2022 for a 2022 mint
})
```

A hook-gated mint needs its hook accounts appended. The same helper computes the
union for every leg of a multi-mint instruction:

```ts
const hookAccounts = await hookAccountsForTransfers(
  dex.connection,
  [mintA, mintB],
  [dex.publicKey!],                  // senders
  [dex.clmm.pdas.clmmPool(mintA, mintB)], // receivers (pool PDA / vault owner)
)
```

Token-2022 also rejects a transfer whose destination account does not exist.
`ensureAtaInstruction` answers with a create instruction (or `undefined`), ready
to prepend:

```ts
const ata = await ensureAtaInstruction(dex.connection, owner, mint, owner, TOKEN_PROGRAM_ID)
const ixs = [ata, await dex.clmm.build.addLiquidity(args)].filter(Boolean)
await dex.send(ixs)
```

## Resting an order

```ts
import { PublicKey } from '@solana/web3.js'

const book = await dex.orders.bookFor(baseMint, quoteMint)

const { signature } = await dex.orders.createOrder({
  maker: dex.publicKey!,
  baseMint,
  quoteMint,
  orderId: 1n,                                     // seeded from the clock by the UI
  side: 'ask',
  price: 2_000_000_000n,                           // quote per base, scaled by 1e9
  amount: 500n,
  expiryTimestamp: BigInt(Math.floor(Date.now() / 1000) + 3_600),
  escrowMint: baseMint,
  tokenProgramId: TOKEN_PROGRAM_ID,
  kyc: book.kycConfig.equals(PublicKey.default)
    ? undefined
    : dex.orders.kycAccountsFor(dex.publicKey!, book.kycConfig),
})
```

`newTradeId()` names a settlement trade for a fill on a routed book
(`tradeId: dex.orders.newTradeId()`); on a direct book it is ignored.

## Settlement

```ts
import { TOKEN_PROGRAM_ID, settlementEscrowAuthority } from '@rwa-dex/sdk'

await dex.settlement.initiate({
  tradeId,
  settlementSlots: BigInt(currentSlot + 20),   // absolute unlock slot, not a horizon
  amount: 1_000_000n,
  authority: dex.publicKey!,
  sourceTokenAccount,
  tokenMint,
  taker,
  pool: null,                                  // null on the standalone path
  tokenProgramId: TOKEN_PROGRAM_ID,
  // Hook-gated mint? The escrow is the recipient here, so carry its entry:
  hookAccounts: dex.rwa.transferHookAccounts(tokenMint, dex.publicKey!, settlementEscrowAuthority()),
})

await dex.settlement.settle({
  tradeId,
  tokenMint,
  takerTokenAccount,
  signature: agentSignature,                   // 64 bytes, r || s
  recoveryId,
  tokenProgramId: TOKEN_PROGRAM_ID,
  mintAgent: null,                             // or a per-mint agent override
  // Hook-gated mint? The escrow is the sender now, so carry its entry:
  hookAccounts: dex.rwa.transferHookAccounts(tokenMint, settlementEscrowAuthority(), taker),
})

await dex.settlement.cancel({
  tradeId,
  tokenMint,
  authority: dex.publicKey!,
  refundTokenAccount,
  tokenProgramId: TOKEN_PROGRAM_ID,
})
```

## KYC and the whitelist

```ts
await dex.kyc.issue(authority, institution)
await dex.kyc.verify(wallet)                     // the gate's own check

// A 4-member whitelist; each member is one call.
const tree = buildWhitelist([walletA, walletB, walletC, escrowAuthority])
await dex.hooks.updateRoot(authority, tree.root)          // Uint8Array or hex
await dex.hooks.setWhitelistEntry(authority, walletA, tree.proofFor(walletA))
```

`buildTransferProofs(owners, source, destination)` returns both paths a single
hook-checked transfer needs. Entries must be refreshed with a path for the new
root after a rotation.

## Creating an RWA mint

```ts
import { Keypair } from '@solana/web3.js'
import { TRANSFER_HOOK_PROGRAM_ID } from '@rwa-dex/sdk'

const mint = Keypair.generate()                  // signs the account creation for you
await dex.rwa.createMint({
  mint,
  authority: dex.publicKey!,
  params: {
    decimals: 6,
    transferHookProgram: TRANSFER_HOOK_PROGRAM_ID,
    delegate: updateAuthority,
    feeAuthority,
    name: 'Fund Share Class A',
    uri: 'https://example.com/metadata.json',
  },
})

// The mint's hook is not wired by creation: Token-2022 only resolves the hook's
// accounts once the mint's extra-account-metas exist. Without this every
// transfer of the new mint fails inside the token program.
await dex.hooks.initExtraAccountMetaList(dex.publicKey!, mint.publicKey)

// A whitelist-gated mint also needs entries for whoever will hold it — including
// the pool PDA that will trade it (see Conventions).
```

## TWAMM keeper

```ts
// Loads the pool, derives the expired prefix, executes virtual orders.
await dex.twamm.crank(mintA, mintB)

// The pieces, when you need them separately:
const walk = await dex.twamm.walks.insertion(mintA, mintB, newExpiry)
await dex.twamm.createLongTermOrder({ ..., walkOrders: walk })
```

## Reading state

```ts
const pool = await dex.clmm.poolFor(mintA, mintB)        // decoded, typed
const order = await dex.orders.orderFor(base, quote, maker, orderId)
const book = await dex.orders.list(base, quote)          // every resting order
const vault = await dex.settlement.vault(tradeId)
const mine = await dex.settlement.list({ taker: dex.publicKey! })
const meta = await dex.rwa.metadata(mint)                // name / symbol / uri
const fee = await dex.rwa.transferFee(mint)              // { basisPoints, maximumFee } | null
const program = await dex.accounts.tokenProgram(mint)    // legacy or Token-2022
```

Raw decoders (`decodeClmmPool`, `decodeOrder`, …) are exported for callers that
already hold account data.

## Simulating and failing loudly

```ts
const { err, logs } = await dex.simulate(ix)   // no wallet prompt, no send
```

`send` resolves only after the transaction is confirmed, and throws when it
landed with an on-chain error — a resolved promise always means the state
changed. `describeError(err)` turns an Anchor custom error into readable text.

## Composing your own transaction

Every module's `.build` returns a `TransactionInstruction`; `dex.send` accepts
one or many, plus extra signers:

```ts
await dex.send([ix1, ix2], { signers: [extraKeypair], skipPreflight: false })
```

## API reference

Every action returns `Promise<TxResult>` — `{ signature, blockhash,
lastValidBlockHeight }` — after signing, sending and (by default) confirming.
Every `.build.*` returns `Promise<TransactionInstruction>`. Reads are async and
return the decoded state described under [Reading state](#reading-state);
`wiringFor` returns `{ kycConfig, oracle, settlement }`.

### Client

| Member | Signature | Notes |
| --- | --- | --- |
| `new RwaDexClient(config?)` | `{ cluster?, connection?, wallet?, commitment?, confirm? }` | `cluster` defaults to `localnet`; `connection` wins over it |
| `createClient(config?)` | same | factory, same object |
| `keypairWallet(kp)` | `Keypair → WalletLike` | for scripts, seeders and tests |
| `.connection` `.commitment` `.wallet` | — | `wallet` is settable |
| `.publicKey` | `PublicKey \| null` | the connected wallet's key |
| `.setWallet(wallet)` | chainable | pass `null` to disconnect |
| `.pdas` | the PDA module | `dex.pdas.clmmPool(...)`, `dex.pdas.ata(...)` |
| `.accounts` | [account readers](#account-readers) | bound to the connection |
| `.send(ix \| ix[], TxOptions?)` | `TxOptions = { signers?, skipPreflight?, preflightCommitment?, confirm? }` | confirms by default; throws on on-chain failure |
| `.simulate(ix \| ix[], { feePayer?, signers? })` | `{ err, logs, unitsConsumed }` | no wallet prompt, no send |
| `.clmm` `.dlmm` `.maverick` `.twamm` `.orders` `.settlement` `.kyc` `.rwa` `.hooks` `.oracle` | modules | below |

### `dex.clmm`

| Method | Args |
| --- | --- |
| `initializePool` | `ClmmInitializePoolArgs` |
| `addLiquidity` | `ClmmAddLiquidityArgs` |
| `removeLiquidity` | `ClmmRemoveLiquidityArgs` |
| `collectFees` | `ClmmCollectFeesArgs` |
| `swap` | `ClmmSwapArgs` |
| `tripBreaker` | `{ pool }` |
| `setPaused` | `(pool, authority, paused)` |
| `setAuthority` | `(pool, authority, newAuthority)` |
| `build.*` | one per action, raw builder |
| `pool(address)` / `poolFor(mintA, mintB)` | decoded pool |
| `position(address)` / `positionFor(owner, mintA, mintB, lowerTick, upperTick)` | decoded position |
| `wiringFor(mintA, mintB)` | the pool's rails |
| `addressesFor(mintA, mintB)` | `{ pool, vaultA, vaultB }` |
| `tick(mintA, mintB, tick)` | the tick PDA |

### `dex.dlmm`

| Method | Args |
| --- | --- |
| `initializePool` | `DlmmInitializePoolArgs` |
| `initializeBinArray` | `DlmmInitializeBinArrayArgs` |
| `addLiquidity` / `removeLiquidity` / `swap` | the matching args type |
| `tripBreaker(pool)` | pool address |
| `setPaused` / `setAuthority` | `(pool, authority, …)` |
| `build.*` | one per action |
| `pool(address)` / `poolFor(mintA, mintB)` | decoded pool |
| `position(address)` / `positionFor(owner, mintA, mintB, binId)` | decoded bin position |
| `wiringFor(mintA, mintB)` | the pool's rails |
| `binArrayFor(mintA, mintB, index)` / `binArrayForBin(mintA, mintB, binId)` | bin-array address |
| `binArrayIndex(binId)` / `binArrayIndexesFor(binId, span?)` | container index / indexes for a walk |
| `addressesFor(mintA, mintB)` | `{ pool, vaultA, vaultB }` |

### `dex.maverick`

| Method | Args |
| --- | --- |
| `initializePool` | `MaverickInitializePoolArgs` |
| `initializeBinArray` | `(payer, pool, index)` |
| `addLiquidity` | `MaverickAddLiquidityArgs` (includes `kind`) |
| `removeLiquidity` | `MaverickRemoveLiquidityArgs` (includes `hopArrayIndexes`) |
| `moveBins` | `MaverickMoveBinsArgs` |
| `swap` | `MaverickSwapArgs` |
| `tripBreaker` / `setPaused` / `setAuthority` | as above |
| `build.*` | one per action |
| `pool` / `poolFor` / `position` / `positionFor` / `binArray*` / `binArrayIndexesFor(binId, span?)` / `wiringFor` | as DLMM |
| `hopsFor(mintA, mintB, binId)` | the move-chain array indexes a removal needs |
| `addressesFor(mintA, mintB)` | `{ pool, vaultA, vaultB }` |

### `dex.twamm`

| Method | Args |
| --- | --- |
| `initializePool` | `TwammInitializePoolArgs` |
| `addLiquidity` / `removeLiquidity` | the matching args type |
| `createLongTermOrder` | `TwammCreateOrderArgs` (includes `walkOrders`) |
| `executeVirtualOrders` | `TwammExecuteArgs` (includes `expiredOrders`) |
| `cancelOrder` | `TwammCancelOrderArgs` (includes `prefixOrders`) |
| `withdrawProceeds` | `TwammWithdrawArgs` |
| `crank(mintA, mintB)` | reads the pool, derives the expired prefix, executes |
| `setPaused` / `setAuthority` | as above |
| `build.*` | one per action |
| `walks.insertion(mintA, mintB, newExpiry)` | for a new order |
| `walks.expired(mintA, mintB, now?)` | for the crank |
| `walks.cancelPrefix(mintA, mintB, orderId, owner)` | for a cancel |
| `pool` / `poolFor` / `wiringFor` | decoded pool, rails |
| `lp(address)` / `lpFor(owner, mintA, mintB)` | LP position |
| `order(address)` / `orderFor(owner, mintA, mintB, orderId)` | long-term order |
| `addressesFor(mintA, mintB)` | `{ pool, vaultA, vaultB }` |

### `dex.orders`

| Method | Args |
| --- | --- |
| `initializeBook` | `OrderBookInitializeArgs` |
| `createOrder` | `OrderBookCreateArgs` |
| `fillOrder` | `OrderBookFillArgs` |
| `cancelOrder` | `OrderBookCancelArgs` |
| `setPaused` / `setAuthority` | `(book, authority, …)` |
| `build.*` | one per action |
| `book(address)` / `bookFor(baseMint, quoteMint)` | decoded book |
| `list(baseMint, quoteMint)` | every resting order on the book (program-account listing) |
| `order(address)` / `orderFor(baseMint, quoteMint, maker, orderId)` | decoded order |
| `escrowMintFor(baseMint, quoteMint, side)` | base for an ask, quote for a bid |
| `kycAccountsFor(trader, config?)` | `{ config, attestation }` for a KYC-wired book (defaults to the canonical config) |
| `newTradeId()` | a fresh settlement trade id |

### `dex.settlement`

| Method | Args |
| --- | --- |
| `initialize` | `(authority, poolsRequireGate)` |
| `setHooksAuthority` | `(authority, hooksAuthority)` |
| `setTransferAgent` | `(authority, secp256k1Pubkey)` — 64 bytes, `x \|\| y` |
| `initiate` | `SettlementInitiateArgs` |
| `settle` | `SettlementSettleArgs` |
| `cancel` | `SettlementCancelArgs` |
| `refundExpired` | `{ tradeId, tokenMint, refundTokenAccount, tokenProgramId, hookAccounts? }` |
| `close` | `{ tradeId, tokenMint, authority, tokenProgramId, residualAccount }` |
| `setMintAgent` / `clearMintAgent` | `{ authority, tokenMint, … }` |
| `build.*` | one per action |
| `config()` | decoded config |
| `vault(tradeId)` | decoded vault |
| `list({ taker? })` | every vault, or one taker's (program-account listing) |
| `mintAgent(mint)` | decoded per-mint agent |
| `escrowAuthority()` / `escrowToken(tradeId)` | the fixed escrow owner and one trade's escrow account |
| `pda(tradeId)` / `mintAgentPda(mint)` | derived addresses |

### `dex.kyc`

| Method | Args |
| --- | --- |
| `initializeMint` | `(authority)` |
| `issue` / `revoke` | `(authority, institution)` |
| `verify` | `(user)` — the gate's own check |
| `setAuthority` | `(currentAuthority, newAuthority)` |
| `build.*` | one per action |
| `config()` | decoded config |
| `attestation(owner)` / `attestationBalance(owner)` | the attestation ATA / its token account |
| `configPda()` / `mintPda()` | derived PDAs |

### `dex.rwa`

| Method | Args |
| --- | --- |
| `createMint` | `{ mint: Keypair, authority, params }` — signs the mint account for you |
| `mintTo` | `{ mint, destination, authority, amount }` |
| `transfer` | `RwaTransferArgs` |
| `updateTransferFee` | `{ mint, feeAuthority, basisPoints, maximumFee }` |
| `fundHookAuthority` | `{ payer, lamports }` |
| `build.*` | one per action |
| `transferHookAccounts(mint, sourceOwner, destinationOwner)` | accounts a hook-gated transfer must carry |
| `ata(owner, mint)` | the Token-2022 ATA |
| `mint(address)` / `metadata(mint)` / `transferFee(mint)` / `tokenAccount(address)` | decoded |
| `whitelistEntry(owner)` / `hookAuthority()` / `tokenProgram` | derived / constants |

### `dex.hooks`

| Method | Args |
| --- | --- |
| `fund` | `(payer, lamports)` |
| `afterSwap` | `HooksAfterSwapArgs` — CPI-only on chain, for tooling/tests |
| `initialize` | `(authority)` |
| `updateRoot` | `(authority, root: Uint8Array \| hex)` |
| `setWhitelistEntry` | `(authority, owner, proof[])` |
| `closeWhitelistEntry` | `(authority, owner)` |
| `setPaused` / `setAuthority` | `(authority, …)` |
| `initExtraAccountMetaList` | `(payer, rwaMint)` |
| `build.*` | one per action |
| `config()` / `whitelistEntry(owner)` | decoded config / entry row |
| `authorityPda()` / `entryPda(owner)` | the hook authority and whitelist-entry addresses |

### `dex.oracle`

| Method | Args |
| --- | --- |
| `unpausePool` | `(pool, owner)` |
| `build.unpausePool` | raw builder |
| `state(pool)` | decoded oracle watching that pool |
| `pda(pool)` | the oracle account address |

### Account readers

`dex.accounts` (or `accountsFor(connection)`) — all take an address:

`account`, `clmmPool`, `clmmPosition`, `dlmmPool`, `maverickPool`,
`binPosition`, `maverickBinArray`, `twammPool`, `twammLp`, `twammOrder`,
`orderBook`, `order`, `oracle`, `settlementConfig`, `settlementVault`,
`mintAgent`, `hookConfig`, `kycConfig`, `tokenAccount`, `mint`, `metadata`,
`tokenProgram`, `transferFee`.

For "every order on this book" / "my vaults" the program keeps no index, so
query helpers filter program accounts the same way the pages do:
`listOrders(connection, book)`, `listSettlementVaults(connection, { taker? })`
and the generic `programAccounts(connection, programId, filters)` — also
reachable as `dex.orders.list(baseMint, quoteMint)` and
`dex.settlement.list({ taker })`.

### Exported protocol surface

Everything below is also importable directly, without a client:

- **Program IDs** — `CLMM_PROGRAM_ID`, `DLMM_PROGRAM_ID`, `MAVERICK_PROGRAM_ID`,
  `TWAMM_PROGRAM_ID`, `ORACLE_PROGRAM_ID`, `KYC_PROGRAM_ID`, `HOOKS_PROGRAM_ID`,
  `SETTLEMENT_PROGRAM_ID`, `RWA_MINT_PROGRAM_ID`, `ORDER_BOOK_PROGRAM_ID`,
  `TRANSFER_HOOK_PROGRAM_ID`, both token programs, the ATA and system programs.
- **PDAs** — every derivation in `pdas`: `clmmPool`, `clmmVaultA/B`, `clmmTick`,
  `clmmPosition`, `dlmmPool`, `dlmmVaultA/B`, `dlmmBinArray(ForBin)`,
  `dlmmPosition`, `maverickPool`, `maverickVaultA/B`, `maverickBinArray(ForBin)`,
  `maverickPosition`, `twammPool`, `twammVaultA/B`, `twammLpPosition`,
  `twammOrder`, `orderBook`, `orderBookOrder`, `oracleState`, `kycConfig`,
  `kycMint`, `hookAuthority`, `settlementConfig`, `settlementVault`,
  `settlementEscrowAuthority`, `settlementEscrowToken`, `settlementMintAgent`,
  `transferHookConfig`, `transferHookExtraAccountMetaList`,
  `transferHookWhitelistEntry`, `programData`, `ata`, `findPda`, `i32Seed`,
  `i64Seed`, `sqrtPriceX64FromPrice`.
- **Builders** — `clmm*`, `dlmm*`, `maverick*`, `twamm*`, `orderBook*`,
  `settlement*`, `kyc*`, `oracleUnpausePool`, `rwa*`, `transferHook*`,
  `hooks*`, with their `*Args` types.
- **Decoders** — every `decode*` and `load*`, `Reader`, `AccountRow`,
  `ORDER_LEN`, `ORDER_BOOK_LEN`, `SETTLEMENT_VAULT_LEN`, `MINT_AGENT_LEN`,
  `mintTransferHookProgram`, `mintTransferFee`.
- **Borsh** — `BorshWriter`, `encodeInstruction`, `discriminator`, `snakeCase`.
- **Walks** — `deriveTwammInsertionWalk`, `deriveTwammExpiredWalk`,
  `deriveTwammPrefixWalk`, `deriveMaverickHopArrayIndexes`, `WALK_LIMIT`.
- **Whitelist** — `buildWhitelist`, `buildTransferProofs`, `hexToBytes`,
  `buildWhitelistTree`, `keccak256`, `leafHash`, `nodeHash`, `bytesToHex`,
  `compareBytes`.
- **Metadata** — `parseTokenMetadata`, `loadTokenMetadata`, `detectIdentifiers`.
- **Queries** — `programAccounts`, `listOrders`, `listSettlementVaults`.
- **Connection** — `connectionFor`, `RPC_ENDPOINTS`, `sendWithSigners`,
  `sendWithSignersDetailed`, `confirmTx`, `ensureAtaInstruction`,
  `createAtaInstruction`, `getInjectedWallet`, `hookAccountsForMint`,
  `hookAccountsForTransfers`, `tokenProgramForMint`, `transferFeeForMint`,
  `describeError`, `noneOf`, `lamportsToSol`, `shortAddress`,
  `loadLocalKeypair`.

## What is not included

Deliberate omissions, so nothing here is mistaken for a missing feature:

- **No IDL-generated client.** Instructions are hand-encoded against the
  programs' `#[derive(Accounts)]` structs, exactly like the app; there is no
  codegen step to drift.
- **No React state, hooks or components.** The UI layer stays in `src/ui`; the
  SDK is framework-free.
- **No portfolio aggregation or pricing.** Those are UI concerns
  (`src/ui/pricing.ts`, `src/ui/pages/PortfolioPage.tsx`) and depend on display choices,
  not on the programs.
- **No DLMM bin-array decoder.** `src/core` ships one only for Maverick arrays;
  the SDK returns the DLMM array **address** (`binArrayFor`, `binArrayForBin`)
  rather than inventing a layout.
- **No indexer.** `listOrders` / `listSettlementVaults` call
  `getProgramAccounts` on the RPC, which is fine for a book or a desk but not a
  protocol-wide history; use an indexer for that.
- **ESM only, no CJS bundle.** `dist/index.js` is a single ESM file; CJS
  consumers need a dynamic `import()`.
- **Not published.** `bun run build` produces a packable artifact, but the
  package has not been pushed to a registry and its `UNLICENSED` license has to
  be decided before it is.

## Conventions

- **Amounts are raw base units.** Apply `10^decimals` yourself; the SDK never
  rescales.
- **Numbers are `bigint`** where the chain uses `u64`/`u128`/`i64` (amounts,
  prices, ids, slots). Small integer fields (`tickSpacing`, `binStep`) stay
  `number`.
- **Order prices** are quote raw units per base raw unit, scaled by `1e9`.
- **Settlement slots are absolute**, not a horizon: resolve
  `currentSlot + N` before calling `initiate`.
- **`null` rails, not missing accounts.** The builders convert `kycConfig:
  null` / `hooksAuthority: null` into the program-ID sentinel Anchor reads as
  `None`.
- **Resolve the mint's token program.** The builders default `tokenProgramId`
  to legacy SPL Token; a Token-2022 (RWA) mint needs Token-2022 or its ATAs and
  vaults are derived under the wrong program. `await dex.accounts.tokenProgram(mint)`
  answers it from the account owner.
- **Token accounts must exist** before they are written to. `ensureAtaInstruction`
  reports whether a create is needed; Token-2022 refuses a transfer to a
  missing account.
- **Hook-gated mints need their hook accounts.** Use
  `hookAccountsForTransfers` (multi-leg instructions) or
  `rwa.transferHookAccounts` (one transfer); both whitelisted owners are
  checked, including an escrow PDA's or the moving pool PDA's. A pool can only
  trade a hook-gated mint after that PDA has an entry of its own
  (`transferHookWhitelistEntry(pool)`), so add it to the tree you publish.
- **One `trade_id` namespace** spans settlement and routed AMM swaps; reuse
  fails closed in the vault's `init`, so name a fresh id per fill
  (`orders.newTradeId()`).
- **Errors are loud.** `send` throws on an on-chain failure instead of
  reporting a confirmation; `describeError` formats Anchor custom codes.
