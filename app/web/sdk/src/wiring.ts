/**
 * Rail wiring, read from pool state.
 *
 * Every AMM instruction that moves tokens carries optional KYC, oracle and
 * settlement accounts. On chain the rails are off when their stored key is the
 * default pubkey, so the same three booleans are derived the same way for all
 * four engines — pass `await dex.clmm.wiringFor(mintA, mintB)` straight into
 * `swap`'s `wiring` argument instead of remembering which rail a pool runs.
 */
import { PublicKey } from '@solana/web3.js'

/** The `wiring` argument the AMM builders take. */
export interface RailWiring {
  kycConfig: boolean
  oracle: boolean
  settlement: boolean
}

/** The subset of any pool account that says which rails are on. */
export interface RailKeys {
  kycConfig: PublicKey
  oracleState: PublicKey
  hooksAuthority: PublicKey
}

export function railWiring(pool: RailKeys): RailWiring {
  return {
    kycConfig: !pool.kycConfig.equals(PublicKey.default),
    oracle: !pool.oracleState.equals(PublicKey.default),
    settlement: !pool.hooksAuthority.equals(PublicKey.default),
  }
}
