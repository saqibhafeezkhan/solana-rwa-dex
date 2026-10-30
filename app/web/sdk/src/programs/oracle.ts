/**
 * Oracle circuit breaker.
 *
 * Every other oracle instruction is driven by the pools through CPI; from
 * outside, the only action is the owner's deliberate `unpausePool` after an
 * incident. The state is keyed by the pool it watches.
 */
import type { PublicKey } from '@solana/web3.js'
import { oracleUnpausePool } from '../core/instructions-rails'
import { oracleState } from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'

export function createOracleModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    /** Owner-only reset after a breaker trip. */
    unpausePool: (pool: PublicKey, owner: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, oracleUnpausePool(pool, owner), tx),

    build: { unpausePool: oracleUnpausePool },

    // ---------------------------------------------------------------- reads

    pda: oracleState,
    /** The oracle watching `pool` (the argument is the pool's address). */
    state: (pool: PublicKey) => accounts.oracle(oracleState(pool)),
  }
}

export type OracleModule = ReturnType<typeof createOracleModule>
