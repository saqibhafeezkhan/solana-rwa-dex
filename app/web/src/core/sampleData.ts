/** Sample / mock data for UI demonstration when on-chain deployment is not available.
 * These mirrors the shapes returned by the real decoders so the pages render
 * with realistic values and animations even on a fresh cluster.
 */

import { PublicKey } from '@solana/web3.js'
import type {
  ClmmPoolState,
  BinPoolState,
  TwammPoolState,
  SettlementConfigView,
  KycConfigView,
  OracleStateView,
  OrderBookState,
  OrderView,
} from './decoders'

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

function makePubkey(seed: string): PublicKey {
  // Deterministic fake pubkey from a seed string.
  const bytes = new Uint8Array(32)
  const encoder = new TextEncoder()
  const raw = encoder.encode(seed)
  for (let i = 0; i < 32; i++) bytes[i] = raw[i % raw.length] ^ (i + 99)
  return new PublicKey(bytes)
}

const FAKE_MINT_A = makePubkey('sample-mint-a')
const FAKE_MINT_B = makePubkey('sample-mint-b')
const FAKE_AUTHORITY = makePubkey('sample-authority')
const FAKE_KYC_MINT = makePubkey('sample-kyc-mint')
const FAKE_KYC_AUTHORITY = makePubkey('sample-kyc-authority')
const FAKE_HOOK_AUTH = makePubkey('sample-hook-auth')

// -----------------------------------------------------------------------
// CLMM sample pool
// -----------------------------------------------------------------------

export const SAMPLE_CLMM_POOL: ClmmPoolState = {
  address: makePubkey('sample-clmm-pool'),
  mintA: FAKE_MINT_A,
  mintB: FAKE_MINT_B,
  // Q64.64 sqrt(price) for price 1.0 — the scale `sqrtPriceX64FromPrice` writes,
  // so the page's price, amounts and oracle read-outs are all consistent.
  sqrtPriceX64: 2n ** 64n,
  liquidity: 500_000_000_000_000n,
  tickCurrent: 0,
  tickSpacing: 64,
  feePips: 3000,
  feeGrowthGlobalA: 12345n,
  feeGrowthGlobalB: 67890n,
  authority: FAKE_AUTHORITY,
  paused: false,
  kycConfig: FAKE_KYC_MINT,
  oracleState: makePubkey('sample-oracle'),
  hooksAuthority: FAKE_HOOK_AUTH,
  settlementSlots: 100n,
  bitmapCells: 0,
}

// -----------------------------------------------------------------------
// DLMM sample pool
// -----------------------------------------------------------------------

export const SAMPLE_DLMM_POOL: BinPoolState = {
  address: makePubkey('sample-dlmm-pool'),
  mintA: FAKE_MINT_A,
  mintB: FAKE_MINT_B,
  binStep: 10,
  baseFeeBps: 20,
  activeBinId: 0,
  authority: FAKE_AUTHORITY,
  paused: false,
  decimalsA: 6,
  decimalsB: 6,
  kycConfig: FAKE_KYC_MINT,
  oracleState: makePubkey('sample-oracle'),
  hooksAuthority: FAKE_HOOK_AUTH,
  settlementSlots: 100n,
}

// -----------------------------------------------------------------------
// Maverick sample pool
// -----------------------------------------------------------------------

export const SAMPLE_MAVERICK_POOL: BinPoolState = {
  address: makePubkey('sample-maverick-pool'),
  mintA: FAKE_MINT_A,
  mintB: FAKE_MINT_B,
  binStep: 5,
  baseFeeBps: 10,
  activeBinId: 50,
  authority: FAKE_AUTHORITY,
  paused: false,
  decimalsA: 6,
  decimalsB: 6,
  kycConfig: FAKE_KYC_MINT,
  oracleState: makePubkey('sample-oracle'),
  hooksAuthority: FAKE_HOOK_AUTH,
  settlementSlots: 200n,
}

// -----------------------------------------------------------------------
// TWAMM sample pool
// -----------------------------------------------------------------------

export const SAMPLE_TWAMM_POOL: TwammPoolState = {
  address: makePubkey('sample-twamm-pool'),
  mintA: FAKE_MINT_A,
  mintB: FAKE_MINT_B,
  reserveA: 1_000_000_000n,
  reserveB: 1_000_000_000n,
  lastExecutionTimestamp: 1_700_000_000n,
  sellRateA: 1_000_000n,
  sellRateB: 1_000_000n,
  accBPerA: 1_000_000_000_000n,
  accAPerB: 1_000_000_000_000n,
  orderHead: PublicKey.default,
  orderHeadExpiry: 0n,
  lpSupply: 500_000n,
  authority: FAKE_AUTHORITY,
  paused: false,
  kycConfig: FAKE_KYC_MINT,
  oracleState: makePubkey('sample-oracle'),
  hooksAuthority: FAKE_HOOK_AUTH,
  settlementSlots: 100n,
}

// -----------------------------------------------------------------------
// Order book sample
// -----------------------------------------------------------------------

export const SAMPLE_ORDER_BOOK: OrderBookState = {
  address: makePubkey('sample-order-book'),
  mintBase: FAKE_MINT_A,
  mintQuote: FAKE_MINT_B,
  authority: FAKE_AUTHORITY,
  kycConfig: FAKE_KYC_MINT,
  hooksAuthority: FAKE_HOOK_AUTH,
  settlementSlots: 100n,
  paused: false,
}

export const SAMPLE_ORDERS: OrderView[] = [
  {
    address: makePubkey('sample-order-ask-1'),
    book: SAMPLE_ORDER_BOOK.address,
    owner: makePubkey('sample-maker-1'),
    orderId: 1001n,
    side: 'ask',
    price: 2_000_000_000n,
    originalAmount: 250_000n,
    remainingAmount: 175_000n,
    filledBase: 75_000n,
    filledQuote: 150_000n,
    createdAt: 1_700_000_000n,
    expiryTimestamp: 1_800_000_000n,
  },
  {
    address: makePubkey('sample-order-bid-2'),
    book: SAMPLE_ORDER_BOOK.address,
    owner: makePubkey('sample-maker-2'),
    orderId: 1002n,
    side: 'bid',
    price: 1_980_000_000n,
    originalAmount: 400_000n,
    remainingAmount: 400_000n,
    filledBase: 0n,
    filledQuote: 0n,
    createdAt: 1_700_000_100n,
    expiryTimestamp: 1_800_000_000n,
  },
  {
    address: makePubkey('sample-order-ask-3'),
    book: SAMPLE_ORDER_BOOK.address,
    owner: makePubkey('sample-maker-3'),
    orderId: 1003n,
    side: 'ask',
    price: 2_010_000_000n,
    originalAmount: 90_000n,
    remainingAmount: 90_000n,
    filledBase: 0n,
    filledQuote: 0n,
    createdAt: 1_700_000_200n,
    expiryTimestamp: 1_800_000_000n,
  },
]

// -----------------------------------------------------------------------
// Settlement config sample
// -----------------------------------------------------------------------

export const SAMPLE_SETTLEMENT_CONFIG: SettlementConfigView = {
  address: makePubkey('sample-settlement-config'),
  authority: FAKE_AUTHORITY,
  transferAgentSecp256k1: new Uint8Array(64),
  poolsRequireGate: true,
  hooksAuthority: FAKE_HOOK_AUTH,
}

// -----------------------------------------------------------------------
// KYC config sample
// -----------------------------------------------------------------------

export const SAMPLE_KYC_CONFIG: KycConfigView = {
  address: makePubkey('sample-kyc-config'),
  authority: FAKE_KYC_AUTHORITY,
  kycMint: FAKE_KYC_MINT,
}

// -----------------------------------------------------------------------
// Oracle sample
// -----------------------------------------------------------------------

/**
 * A pool's oracle read-out for the demo pages. `latestPrice` is on the same
 * Q64.64 scale the on-chain accounts store, so the panel's TWAP-versus-spot gap
 * is a real basis-point figure: the TWAP tracks 10 bps below parity while the
 * threshold is 500 bps, which keeps every sample pool inside its rail.
 */
export function sampleOracleState(pool: PublicKey): OracleStateView {
  const parity = 2n ** 64n
  const latestTimestamp = 1_700_000_060n
  const latestPrice = parity - parity / 1000n
  const observations = [
    { timestamp: 1_700_000_000n, sqrtPriceX64: parity - parity / 500n },
    { timestamp: 1_700_000_030n, sqrtPriceX64: parity - parity / 1000n },
    { timestamp: latestTimestamp, sqrtPriceX64: latestPrice },
  ]
  return {
    address: makePubkey('sample-oracle'),
    pool,
    owner: FAKE_AUTHORITY,
    isPaused: false,
    maxDeviationBps: 500,
    observationCount: observations.length,
    latestTimestamp,
    latestPrice,
    observations,
  }
}

