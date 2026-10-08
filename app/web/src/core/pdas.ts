/**
 * Program addresses and every PDA the DEX programs derive. The seeds mirror
 * the on-chain `seeds = [...]` declarations one for one; u64/i64 seeds are
 * little-endian, as Anchor writes them.
 */
import { PublicKey } from '@solana/web3.js'

export const CLMM_PROGRAM_ID = new PublicKey('GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU')
export const DLMM_PROGRAM_ID = new PublicKey('5JPnmJouZ3yE9Xwaacjtr5F2E1ShYGxJ1mR4UV9EFE3c')
export const MAVERICK_PROGRAM_ID = new PublicKey('9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4')
export const TWAMM_PROGRAM_ID = new PublicKey('FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9')
export const ORACLE_PROGRAM_ID = new PublicKey('4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj')
export const KYC_PROGRAM_ID = new PublicKey('7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1')
export const HOOKS_PROGRAM_ID = new PublicKey('B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA')
export const SETTLEMENT_PROGRAM_ID = new PublicKey('58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6')
export const RWA_MINT_PROGRAM_ID = new PublicKey('5GbeJh4sySASsdH2zRdFX85LuXnu5y3rBkqhVi76Lzho')
export const TRANSFER_HOOK_PROGRAM_ID = new PublicKey('fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS')

export const TOKEN_PROGRAM_ID = new PublicKey('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA')
export const TOKEN_2022_PROGRAM_ID = new PublicKey('TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb')
export const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL')
export const SYSTEM_PROGRAM_ID = new PublicKey('11111111111111111111111111111111')
export const BPF_LOADER_UPGRADEABLE_ID = new PublicKey('BPFLoaderUpgradeab1e11111111111111111111111')

const textEncoder = new TextEncoder()

function seedOf(seed: string | Uint8Array): Uint8Array {
  return typeof seed === 'string' ? textEncoder.encode(seed) : seed
}

function leBytes(value: number | bigint, width: 4 | 8): Uint8Array {
  const bytes = new Uint8Array(width)
  let v = BigInt(value)
  for (let i = 0; i < width; i++) {
    bytes[i] = Number(v & 0xffn)
    v >>= 8n
  }
  return bytes
}

/** i64/i32 seeds are little-endian, exactly as Anchor's `to_le_bytes()`. */
export const i32Seed = (value: number) => leBytes(value, 4)
export const i64Seed = (value: number | bigint) => leBytes(value, 8)
export const u64Seed = i64Seed

export function findPda(seeds: (Uint8Array | PublicKey)[], programId: PublicKey): PublicKey {
  const parts = seeds.map((seed) => (seed instanceof PublicKey ? seed.toBytes() : seed))
  const [pda] = PublicKey.findProgramAddressSync(parts, programId)
  return pda
}

// -----------------------------------------------------------------------
// CLMM (concentrated liquidity)
// -----------------------------------------------------------------------

export const clmmPool = (mintA: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('clmm-pool'), mintA, mintB], CLMM_PROGRAM_ID)

export const clmmVaultA = (pool: PublicKey, mintA: PublicKey) =>
  findPda([seedOf('clmm-vault-a'), pool, mintA], CLMM_PROGRAM_ID)

export const clmmVaultB = (pool: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('clmm-vault-b'), pool, mintB], CLMM_PROGRAM_ID)

export const clmmTick = (pool: PublicKey, tick: number) =>
  findPda([seedOf('clmm-tick'), pool, i32Seed(tick)], CLMM_PROGRAM_ID)

export const clmmPosition = (pool: PublicKey, owner: PublicKey, lowerTick: number, upperTick: number) =>
  findPda([seedOf('clmm-pos'), pool, owner, i32Seed(lowerTick), i32Seed(upperTick)], CLMM_PROGRAM_ID)

// -----------------------------------------------------------------------
// DLMM (dynamic liquidity, bins)
// -----------------------------------------------------------------------

export const dlmmPool = (mintA: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('pool'), mintA, mintB], DLMM_PROGRAM_ID)

export const dlmmVaultA = (pool: PublicKey, mintA: PublicKey) =>
  findPda([seedOf('vault_a'), pool, mintA], DLMM_PROGRAM_ID)

export const dlmmVaultB = (pool: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('vault_b'), pool, mintB], DLMM_PROGRAM_ID)

/** Bin arrays hold 70 bins each; the array index is part of the seeds. */
export const DLMM_BINS_PER_ARRAY = 70

export const dlmmBinArrayIndex = (binId: number): bigint =>
  BigInt(Math.floor(Number(binId) / DLMM_BINS_PER_ARRAY))

export const dlmmBinArray = (pool: PublicKey, index: number | bigint) =>
  findPda([seedOf('bin_array'), pool, i64Seed(index)], DLMM_PROGRAM_ID)

export const dlmmBinArrayForBin = (pool: PublicKey, binId: number) =>
  dlmmBinArray(pool, dlmmBinArrayIndex(binId))

export const dlmmPosition = (pool: PublicKey, owner: PublicKey, binId: number) =>
  findPda([seedOf('position'), pool, owner, i32Seed(binId)], DLMM_PROGRAM_ID)

// -----------------------------------------------------------------------
// Maverick (dynamic-distribution bins)
// -----------------------------------------------------------------------

export const maverickPool = (mintA: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('pool'), mintA, mintB], MAVERICK_PROGRAM_ID)

export const maverickVaultA = (pool: PublicKey, mintA: PublicKey) =>
  findPda([seedOf('vault_a'), pool, mintA], MAVERICK_PROGRAM_ID)

export const maverickVaultB = (pool: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('vault_b'), pool, mintB], MAVERICK_PROGRAM_ID)

export const MAVERICK_BINS_PER_ARRAY = 50

export const maverickBinArrayIndex = (binId: number): bigint =>
  BigInt(Math.floor(Number(binId) / MAVERICK_BINS_PER_ARRAY))

export const maverickBinArray = (pool: PublicKey, index: number | bigint) =>
  findPda([seedOf('bin_array'), pool, i64Seed(index)], MAVERICK_PROGRAM_ID)

export const maverickBinArrayForBin = (pool: PublicKey, binId: number) =>
  maverickBinArray(pool, maverickBinArrayIndex(binId))

export const maverickPosition = (pool: PublicKey, owner: PublicKey, binId: number) =>
  findPda([seedOf('position'), pool, owner, i32Seed(binId)], MAVERICK_PROGRAM_ID)

// -----------------------------------------------------------------------
// TWAMM (time-weighted)
// -----------------------------------------------------------------------

export const twammPool = (mintA: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('twamm-pool'), mintA, mintB], TWAMM_PROGRAM_ID)

export const twammVaultA = (pool: PublicKey, mintA: PublicKey) =>
  findPda([seedOf('twamm-vault-a'), pool, mintA], TWAMM_PROGRAM_ID)

export const twammVaultB = (pool: PublicKey, mintB: PublicKey) =>
  findPda([seedOf('twamm-vault-b'), pool, mintB], TWAMM_PROGRAM_ID)

export const twammLpPosition = (pool: PublicKey, owner: PublicKey) =>
  findPda([seedOf('twamm-lp'), pool, owner], TWAMM_PROGRAM_ID)

export const twammOrder = (pool: PublicKey, owner: PublicKey, orderId: number | bigint) =>
  findPda([seedOf('twamm-order'), pool, owner, u64Seed(orderId)], TWAMM_PROGRAM_ID)

// -----------------------------------------------------------------------
// Shared rails: oracle, KYC gate, hooks, settlement
// -----------------------------------------------------------------------

export const oracleState = (pool: PublicKey) =>
  findPda([seedOf('oracle'), pool], ORACLE_PROGRAM_ID)

export const kycConfig = () => findPda([seedOf('kyc-config')], KYC_PROGRAM_ID)

export const kycMint = () => findPda([seedOf('kyc-mint')], KYC_PROGRAM_ID)

export const hookAuthority = () => findPda([seedOf('hook_authority')], HOOKS_PROGRAM_ID)

export const settlementConfig = () => findPda([seedOf('config')], SETTLEMENT_PROGRAM_ID)

export const settlementVault = (tradeId: number | bigint) =>
  findPda([seedOf('settlement'), u64Seed(tradeId)], SETTLEMENT_PROGRAM_ID)

export const transferHookConfig = () => findPda([seedOf('config')], TRANSFER_HOOK_PROGRAM_ID)

export const transferHookExtraAccountMetaList = (mint: PublicKey) =>
  findPda([seedOf('extra-account-metas'), mint], TRANSFER_HOOK_PROGRAM_ID)

export const programData = (program: PublicKey) =>
  findPda([program.toBytes()], BPF_LOADER_UPGRADEABLE_ID)

// -----------------------------------------------------------------------
// Token accounts
// -----------------------------------------------------------------------

export const ata = (owner: PublicKey, mint: PublicKey, tokenProgram = TOKEN_PROGRAM_ID) =>
  PublicKey.findProgramAddressSync(
    [owner.toBytes(), tokenProgram.toBytes(), mint.toBytes()],
    ASSOCIATED_TOKEN_PROGRAM_ID,
  )[0]

// -----------------------------------------------------------------------
// Chain-side helpers
// -----------------------------------------------------------------------

/**
 * Q64.64 price used by CLMM pools: `sqrt(price) * 2^64`. `price` is expressed
 * in raw token units of B per raw unit of A. Precision is ~1e-9 relative,
 * which is plenty for an initial price the pool will trade away from.
 */
export function sqrtPriceX64FromPrice(price: number): bigint {
  if (price <= 0 || !Number.isFinite(price)) throw new Error('price must be a positive number')
  // isqrt(price * 1e18) ≈ sqrt(price) * 1e9, so shifting by 64 and dividing
  // by 1e9 yields sqrt(price) * 2^64 exactly as the program reads it.
  const scaled = BigInt(Math.round(price * 1e9)) * 1_000_000_000n
  let x = scaled
  let y = (x + 1n) / 2n
  while (y < x) {
    x = y
    y = (x + scaled / x) / 2n
  }
  return (x << 64n) / 1_000_000_000n
}
