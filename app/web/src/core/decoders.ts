/**
 * Account decoders. Each reader matches the on-chain struct field for field:
 * Anchor accounts start with an 8-byte discriminator, then borsh-serialized
 * fields in declaration order. Every read is bounds-checked against the
 * account length so a wrong account fails loudly instead of producing garbage.
 */
import { PublicKey, type Connection } from '@solana/web3.js'
import { MAVERICK_BINS_PER_ARRAY } from './pdas'

export class Reader {
  readonly data: Uint8Array
  offset: number

  constructor(data: Uint8Array, offset = 8) {
    this.data = data
    this.offset = offset
  }

  private take(width: number): Uint8Array {
    if (this.offset + width > this.data.length) {
      throw new Error(`account too short: need ${this.offset + width} bytes, have ${this.data.length}`)
    }
    const slice = this.data.subarray(this.offset, this.offset + width)
    this.offset += width
    return slice
  }

  get remaining(): number {
    return this.data.length - this.offset
  }

  u8(): number {
    return this.take(1)[0]
  }

  bool(): boolean {
    return this.u8() !== 0
  }

  private unsigned(width: number): bigint {
    let value = 0n
    const bytes = this.take(width)
    for (let i = width - 1; i >= 0; i--) value = (value << 8n) | BigInt(bytes[i])
    return value
  }

  u16(): number {
    return Number(this.unsigned(2))
  }

  u32(): number {
    return Number(this.unsigned(4))
  }

  u64(): bigint {
    return this.unsigned(8)
  }

  u128(): bigint {
    return this.unsigned(16)
  }

  i32(): number {
    return Number(BigInt.asIntN(32, this.unsigned(4)))
  }

  i64(): bigint {
    return BigInt.asIntN(64, this.unsigned(8))
  }

  pubkey(): PublicKey {
    return new PublicKey(this.take(32))
  }

  /** A borsh `String`: u32 length + utf8. */
  string(): string {
    const length = this.u32()
    return new TextDecoder().decode(this.take(length))
  }

  /** A borsh `Option<T>`: a 1-byte tag, then the value when present. */
  optionBool(): boolean | null {
    const tag = this.u8()
    if (tag === 0) return null
    return this.bool()
  }
}

export interface AccountRow {
  pubkey: PublicKey
  owner: PublicKey
  data: Uint8Array
  lamports: number
}

async function fetchRow(connection: Connection, address: PublicKey): Promise<AccountRow> {
  const info = await connection.getAccountInfo(address)
  if (!info) throw new Error(`account ${address.toBase58()} not found`)
  return { pubkey: address, owner: info.owner, data: info.data, lamports: info.lamports }
}

// -----------------------------------------------------------------------
// CLMM
// -----------------------------------------------------------------------

/** CLMM Pool: fixed fields then the bitmap Vec (u32 len + bytes). */
export interface ClmmPoolState {
  address: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  sqrtPriceX64: bigint
  liquidity: bigint
  tickCurrent: number
  tickSpacing: number
  feePips: number
  feeGrowthGlobalA: bigint
  feeGrowthGlobalB: bigint
  /** Admin pause key (L-3). */
  authority: PublicKey
  paused: boolean
  kycConfig: PublicKey
  oracleState: PublicKey
  hooksAuthority: PublicKey
  settlementSlots: bigint
  bitmapCells: number
}

export function decodeClmmPool(row: AccountRow): ClmmPoolState {
  const r = new Reader(row.data)
  const mintA = r.pubkey()
  const mintB = r.pubkey()
  const sqrtPriceX64 = r.u128()
  const liquidity = r.u128()
  const tickCurrent = r.i32()
  const tickSpacing = r.i32()
  const feePips = r.u32()
  const feeGrowthGlobalA = r.u128()
  const feeGrowthGlobalB = r.u128()
  const authority = r.pubkey()
  const paused = r.bool()
  r.u8() // bump
  const bitmapCells = r.u32()
  // The bitmap Vec is `u32 len + ceil(cells/8)` bytes. The length prefix has
  // just been read, so resume at the reader's current position plus the bytes:
  // counting the prefix again here is what used to shift every wiring field by
  // four bytes (and silently point the swap rails at the wrong accounts).
  const bitmapBytes = r.u32()
  const wiring = new Reader(row.data, r.offset + bitmapBytes)
  const kycConfig = wiring.pubkey()
  const oracleState = wiring.pubkey()
  const hooksAuthority = wiring.pubkey()
  const settlementSlots = wiring.u64()
  return {
    address: row.pubkey,
    mintA,
    mintB,
    sqrtPriceX64,
    liquidity,
    tickCurrent,
    tickSpacing,
    feePips,
    feeGrowthGlobalA,
    feeGrowthGlobalB,
    authority,
    paused,
    kycConfig,
    oracleState,
    hooksAuthority,
    settlementSlots,
    bitmapCells,
  }
}

export async function loadClmmPool(connection: Connection, address: PublicKey) {
  return decodeClmmPool(await fetchRow(connection, address))
}

/** CLMM Position. */
export interface ClmmPositionState {
  address: PublicKey
  pool: PublicKey
  owner: PublicKey
  lowerTick: number
  upperTick: number
  liquidity: bigint
  tokensOwedA: bigint
  tokensOwedB: bigint
}

export function decodeClmmPosition(row: AccountRow): ClmmPositionState {
  const r = new Reader(row.data)
  const pool = r.pubkey()
  const owner = r.pubkey()
  const lowerTick = r.i32()
  const upperTick = r.i32()
  const liquidity = r.u128()
  r.u128() // fee_growth_inside_a
  r.u128() // fee_growth_inside_b
  const tokensOwedA = r.u64()
  const tokensOwedB = r.u64()
  return { address: row.pubkey, pool, owner, lowerTick, upperTick, liquidity, tokensOwedA, tokensOwedB }
}

// -----------------------------------------------------------------------
// DLMM / Maverick pools share a layout until `paused`
// -----------------------------------------------------------------------

export interface BinPoolState {
  address: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  binStep: number
  baseFeeBps: number
  activeBinId: number
  authority: PublicKey
  paused: boolean
  decimalsA: number
  decimalsB: number
  kycConfig: PublicKey
  oracleState: PublicKey
  hooksAuthority: PublicKey
  settlementSlots: bigint
}

function decodeBinPool(row: AccountRow, maverickExtra: boolean): BinPoolState {
  const r = new Reader(row.data)
  const mintA = r.pubkey()
  const mintB = r.pubkey()
  const binStep = r.u16()
  const baseFeeBps = r.u16()
  const activeBinId = r.i32()
  const authority = r.pubkey()
  const paused = r.bool()
  if (maverickExtra) r.bool() // has_static_baseline
  const decimalsA = r.u8()
  const decimalsB = r.u8()
  const kycConfig = r.pubkey()
  const oracleState = r.pubkey()
  const hooksAuthority = r.pubkey()
  const settlementSlots = r.u64()
  return {
    address: row.pubkey,
    mintA,
    mintB,
    binStep,
    baseFeeBps,
    activeBinId,
    authority,
    paused,
    decimalsA,
    decimalsB,
    kycConfig,
    oracleState,
    hooksAuthority,
    settlementSlots,
  }
}

export const decodeDlmmPool = (row: AccountRow) => decodeBinPool(row, false)
export const decodeMaverickPool = (row: AccountRow) => decodeBinPool(row, true)

export async function loadDlmmPool(connection: Connection, address: PublicKey) {
  return decodeDlmmPool(await fetchRow(connection, address))
}

export async function loadMaverickPool(connection: Connection, address: PublicKey) {
  return decodeMaverickPool(await fetchRow(connection, address))
}

// -----------------------------------------------------------------------
// Maverick bin arrays: bin kinds and the move chain
// -----------------------------------------------------------------------

/** How a bin follows the price, as the program's `BinKind` enum encodes it. */
export type MaverickBinKindView = 'static' | 'right' | 'left' | 'both'

const BIN_KINDS: MaverickBinKindView[] = ['static', 'right', 'left', 'both']

/**
 * One bin inside an array. `binId` is the bin's own id; the array's fields are
 * only meaningful once the bin is funded: `kind` is fixed by the first deposit
 * and `moveState` records where the shares went when a keeper merged it.
 */
export interface MaverickBin {
  binId: number
  priceX64: bigint
  reserveA: bigint
  reserveB: bigint
  supply: bigint
  kind: MaverickBinKindView
  /** True once a keeper merged this bin into `toBinId`. */
  moved: boolean
  /** Bin the shares now live in. Meaningful only when `moved`. */
  toBinId: number
  /** Destination shares per share here, Q64.64. Meaningful only when `moved`. */
  shareRatio: bigint
}

export interface MaverickBinArrayView {
  address: PublicKey
  pool: PublicKey
  /** Which run of bin ids this array covers. */
  index: bigint
  bins: MaverickBin[]
}

/**
 * A `BinArray` is a fixed band of `MAVERICK_BINS_PER_ARRAY` bins, so every bin's
 * id is derived from the array's index rather than stored per bin.
 */
export function decodeMaverickBinArray(row: AccountRow): MaverickBinArrayView {
  const r = new Reader(row.data)
  const pool = r.pubkey()
  const index = r.i64()
  const count = r.u32()
  const bins: MaverickBin[] = []
  for (let i = 0; i < count; i++) {
    const priceX64 = r.u128()
    const reserveA = r.u64()
    const reserveB = r.u64()
    const supply = r.u128()
    const kind = BIN_KINDS[r.u8()] ?? 'static'
    const moved = r.bool()
    const toBinId = r.i32()
    const shareRatio = r.u128()
    bins.push({
      binId: Number(index) * MAVERICK_BINS_PER_ARRAY + i,
      priceX64,
      reserveA,
      reserveB,
      supply,
      kind,
      moved,
      toBinId,
      shareRatio,
    })
  }
  return { address: row.pubkey, pool, index, bins }
}

export async function loadMaverickBinArray(connection: Connection, address: PublicKey) {
  return decodeMaverickBinArray(await fetchRow(connection, address))
}

/** DLMM/Maverick Position: pool, owner, bin_id, shares. */
export interface BinPositionState {
  address: PublicKey
  pool: PublicKey
  owner: PublicKey
  binId: number
  shares: bigint
}

export function decodeBinPosition(row: AccountRow): BinPositionState {
  const r = new Reader(row.data)
  return {
    address: row.pubkey,
    pool: r.pubkey(),
    owner: r.pubkey(),
    binId: r.i32(),
    shares: r.u128(),
  }
}

// -----------------------------------------------------------------------
// TWAMM
// -----------------------------------------------------------------------

export interface TwammPoolState {
  address: PublicKey
  mintA: PublicKey
  mintB: PublicKey
  reserveA: bigint
  reserveB: bigint
  lastExecutionTimestamp: bigint
  sellRateA: bigint
  sellRateB: bigint
  accBPerA: bigint
  accAPerB: bigint
  orderHead: PublicKey
  orderHeadExpiry: bigint
  lpSupply: bigint
  authority: PublicKey
  paused: boolean
  kycConfig: PublicKey
  oracleState: PublicKey
  hooksAuthority: PublicKey
  settlementSlots: bigint
}

export function decodeTwammPool(row: AccountRow): TwammPoolState {
  const r = new Reader(row.data)
  const mintA = r.pubkey()
  const mintB = r.pubkey()
  const reserveA = r.u64()
  const reserveB = r.u64()
  const lastExecutionTimestamp = r.i64()
  const sellRateA = r.u64()
  const sellRateB = r.u64()
  const accBPerA = r.u128()
  const accAPerB = r.u128()
  const orderHead = r.pubkey()
  const orderHeadExpiry = r.i64()
  const lpSupply = r.u128()
  const authority = r.pubkey()
  const paused = r.bool()
  const kycConfig = r.pubkey()
  const oracleState = r.pubkey()
  const hooksAuthority = r.pubkey()
  const settlementSlots = r.u64()
  return {
    address: row.pubkey,
    mintA,
    mintB,
    reserveA,
    reserveB,
    lastExecutionTimestamp,
    sellRateA,
    sellRateB,
    accBPerA,
    accAPerB,
    orderHead,
    orderHeadExpiry,
    lpSupply,
    authority,
    paused,
    kycConfig,
    oracleState,
    hooksAuthority,
    settlementSlots,
  }
}

export async function loadTwammPool(connection: Connection, address: PublicKey) {
  return decodeTwammPool(await fetchRow(connection, address))
}

export interface TwammOrderState {
  address: PublicKey
  pool: PublicKey
  owner: PublicKey
  orderId: bigint
  isAToB: boolean
  sellRate: bigint
  amountIn: bigint
  sold: bigint
  duration: bigint
  createdTimestamp: bigint
  expiryTimestamp: bigint
  proceedsOut: bigint
  settled: boolean
}

/** TWAMM LpPosition: pool, owner, shares, bump. */
export function decodeTwammLp(row: AccountRow): { address: PublicKey; pool: PublicKey; shares: bigint } {
  const r = new Reader(row.data)
  const pool = r.pubkey()
  r.pubkey() // owner
  const shares = r.u128()
  return { address: row.pubkey, pool, shares }
}

export function decodeTwammOrder(row: AccountRow): TwammOrderState {
  const r = new Reader(row.data)
  const pool = r.pubkey()
  const owner = r.pubkey()
  const orderId = r.u64()
  const isAToB = r.bool()
  const sellRate = r.u64()
  const amountIn = r.u64()
  const sold = r.u64()
  const duration = r.i64()
  const createdTimestamp = r.i64()
  const expiryTimestamp = r.i64()
  r.u128() // acc_snapshot
  const proceedsOut = r.u64()
  const settled = r.bool()
  return {
    address: row.pubkey,
    pool,
    owner,
    orderId,
    isAToB,
    sellRate,
    amountIn,
    sold,
    duration,
    createdTimestamp,
    expiryTimestamp,
    proceedsOut,
    settled,
  }
}

// -----------------------------------------------------------------------
// Oracle
// -----------------------------------------------------------------------

export interface OracleObservation {
  timestamp: bigint
  /** sqrt(price) as Q64.64, exactly as stored on chain. */
  sqrtPriceX64: bigint
}

export interface OracleStateView {
  address: PublicKey
  pool: PublicKey
  owner: PublicKey
  isPaused: boolean
  maxDeviationBps: number
  observationCount: number
  latestTimestamp: bigint
  latestPrice: bigint
  /** Valid observations, oldest first (the on-chain ring buffer unwound). */
  observations: OracleObservation[]
}

/** `[Observation; 100]`, each `{ timestamp i64, sqrt_price u128, cumulative u128 }`. */
const ORACLE_OBSERVATION_COUNT = 100
const ORACLE_OBSERVATION_LEN = 40

export function decodeOracleState(row: AccountRow): OracleStateView {
  const r = new Reader(row.data)
  const pool = r.pubkey()
  const owner = r.pubkey()
  const isPaused = r.u8() !== 0
  const observationIndex = r.u8()
  const observationCount = r.u8()
  r.u8() // bump
  const maxDeviationBps = r.u16()
  const base = r.offset
  const latest = new Reader(row.data, base + observationIndex * ORACLE_OBSERVATION_LEN)

  // `initialize` writes a genesis observation into slot 0 and starts the cursor
  // there, so while the buffer is not full the valid slots are exactly 0..count-1
  // in insertion order; once it has wrapped the oldest is the slot right after the
  // newest. Read in that order so the TWAP sparkline (oldest → newest) is correct
  // from the first observation on.
  const observations: OracleObservation[] = []
  if (observationCount > 0 && row.data.length >= base + ORACLE_OBSERVATION_LEN) {
    const start =
      observationCount < ORACLE_OBSERVATION_COUNT
        ? 0
        : (observationIndex + 1) % ORACLE_OBSERVATION_COUNT
    for (let i = 0; i < observationCount; i++) {
      const index = (start + i) % ORACLE_OBSERVATION_COUNT
      const offset = base + index * ORACLE_OBSERVATION_LEN
      if (offset + ORACLE_OBSERVATION_LEN > row.data.length) break
      const reader = new Reader(row.data, offset)
      observations.push({
        timestamp: reader.i64(),
        sqrtPriceX64: reader.u128(),
      })
    }
  }

  return {
    address: row.pubkey,
    pool,
    owner,
    isPaused,
    maxDeviationBps,
    observationCount,
    latestTimestamp: latest.i64(),
    latestPrice: latest.u128(),
    observations,
  }
}

export async function loadOracleState(connection: Connection, address: PublicKey) {
  return decodeOracleState(await fetchRow(connection, address))
}

// -----------------------------------------------------------------------
// Settlement
// -----------------------------------------------------------------------

export interface SettlementConfigView {
  address: PublicKey
  authority: PublicKey
  transferAgentSecp256k1: Uint8Array
  poolsRequireGate: boolean
  hooksAuthority: PublicKey
}

export function decodeSettlementConfig(row: AccountRow): SettlementConfigView {
  const r = new Reader(row.data)
  const authority = r.pubkey()
  const transferAgentSecp256k1 = r.data.subarray(r.offset, r.offset + 64)
  r.offset += 64
  const poolsRequireGate = r.u8() !== 0
  const hooksAuthority = r.pubkey()
  return {
    address: row.pubkey,
    authority,
    transferAgentSecp256k1,
    poolsRequireGate,
    hooksAuthority,
  }
}

export async function loadSettlementConfig(connection: Connection, address: PublicKey) {
  return decodeSettlementConfig(await fetchRow(connection, address))
}

const SETTLEMENT_STATUS = ['pending', 'settled', 'cancelled'] as const

export interface SettlementVaultView {
  address: PublicKey
  tradeId: bigint
  taker: PublicKey
  tokenMint: PublicKey
  amount: bigint
  settlementSlot: bigint
  status: (typeof SETTLEMENT_STATUS)[number]
  pool: PublicKey
  refundSource: PublicKey
}

export function decodeSettlementVault(row: AccountRow): SettlementVaultView {
  const r = new Reader(row.data)
  const tradeId = r.u64()
  const taker = r.pubkey()
  const tokenMint = r.pubkey()
  const amount = r.u64()
  const settlementSlot = r.u64()
  const status = SETTLEMENT_STATUS[r.u8()] ?? 'pending'
  const pool = r.pubkey()
  const refundSource = r.pubkey()
  return { address: row.pubkey, tradeId, taker, tokenMint, amount, settlementSlot, status, pool, refundSource }
}

export async function loadSettlementVault(connection: Connection, address: PublicKey) {
  return decodeSettlementVault(await fetchRow(connection, address))
}

// -----------------------------------------------------------------------
// KYC gate
// -----------------------------------------------------------------------

export interface KycConfigView {
  address: PublicKey
  authority: PublicKey
  kycMint: PublicKey
}

export function decodeKycConfig(row: AccountRow): KycConfigView {
  const r = new Reader(row.data)
  return { address: row.pubkey, authority: r.pubkey(), kycMint: r.pubkey() }
}

export async function loadKycConfig(connection: Connection, address: PublicKey) {
  return decodeKycConfig(await fetchRow(connection, address))
}

// -----------------------------------------------------------------------
// SPL token account (legacy layout, also matches Token-2022 base)
// -----------------------------------------------------------------------

export interface TokenAccountView {
  address: PublicKey
  mint: PublicKey
  owner: PublicKey
  amount: bigint
}

export function decodeTokenAccount(row: AccountRow): TokenAccountView {
  const r = new Reader(row.data, 0)
  const mint = r.pubkey()
  const owner = r.pubkey()
  const amount = r.u64()
  return { address: row.pubkey, mint, owner, amount }
}

export async function loadTokenAccount(connection: Connection, address: PublicKey) {
  return decodeTokenAccount(await fetchRow(connection, address))
}

// -----------------------------------------------------------------------
// Token mint (legacy + Token-2022 base layout)
// -----------------------------------------------------------------------

export interface MintView {
  address: PublicKey
  decimals: number
  supply: bigint
}

export function decodeMint(row: AccountRow): MintView {
  // Legacy/2022 mint: COption authority tag (4) + authority (32) + supply (8)
  // + decimals (1) + is_initialized (1).
  const supply = new Reader(row.data, 36).u64()
  const decimals = new Reader(row.data, 44).u8()
  return { address: row.pubkey, decimals, supply }
}

export async function loadMint(connection: Connection, address: PublicKey) {
  return decodeMint(await fetchRow(connection, address))
}
