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
  sqrtPriceX64: 1_000_000_000_000n,
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

