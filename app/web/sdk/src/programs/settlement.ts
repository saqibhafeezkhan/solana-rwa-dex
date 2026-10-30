/**
 * T+N settlement clearinghouse.
 *
 * A trade is one `trade_id`: the vault PDA records the escrow, its unlock slot
 * and its recipient, and release is authorised by a secp256k1 signature from
 * the Transfer Agent (or the mint's own agent override) rather than by a Solana
 * signer. That is why `settle` takes raw signature bytes.
 */
import type { PublicKey } from '@solana/web3.js'
import type { AccountKey } from '../core/connection'
import {
  settlementCancel,
  settlementClearMintAgent,
  settlementClose,
  settlementInitiate,
  settlementInitialize,
  settlementRefundExpired,
  settlementSetHooksAuthority,
  settlementSetMintAgent,
  settlementSetTransferAgent,
  settlementSettle,
  type SettlementCancelArgs,
  type SettlementInitiateArgs,
} from '../core/instructions-rails'
import {
  settlementConfig,
  settlementEscrowAuthority,
  settlementEscrowToken,
  settlementMintAgent,
  settlementVault,
} from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'
import { listSettlementVaults } from '../queries'

export interface SettlementSettleArgs {
  tradeId: bigint
  tokenMint: PublicKey
  /** The recipient's token account for the escrowed mint. */
  takerTokenAccount: PublicKey
  /** 64 bytes, `r || s`. */
  signature: Uint8Array
  recoveryId: number
  tokenProgramId: PublicKey
  /** The mint's hook accounts when the escrowed mint is hook-gated. */
  hookAccounts?: AccountKey[]
  /** A per-mint Transfer Agent override, or null for the config's global key. */
  mintAgent?: PublicKey | null
}

export function createSettlementModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    /** One-time setup, upgrade authority only. */
    initialize: (authority: PublicKey, poolsRequireGate: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, settlementInitialize(authority, poolsRequireGate), tx),

    setHooksAuthority: (authority: PublicKey, hooksAuthority: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, settlementSetHooksAuthority(authority, hooksAuthority), tx),

    /** The uncompressed 64-byte secp256k1 key (`x || y`). */
    setTransferAgent: (authority: PublicKey, secp256k1Pubkey: Uint8Array, tx?: TxOptions) =>
      sendBuilt(ctx, settlementSetTransferAgent(authority, secp256k1Pubkey), tx),

    initiate: (args: SettlementInitiateArgs, tx?: TxOptions) =>
      sendBuilt(ctx, settlementInitiate(args), tx),

    /** Releases the escrow against the Transfer Agent's secp256k1 signature. */
    settle: (args: SettlementSettleArgs, tx?: TxOptions) =>
      sendBuilt(
        ctx,
        settlementSettle(
          args.tradeId,
          args.tokenMint,
          args.takerTokenAccount,
          args.signature,
          args.recoveryId,
          args.tokenProgramId,
          args.hookAccounts ?? [],
          args.mintAgent ?? null,
        ),
        tx,
      ),

    /** Compliance exit: refunds the full escrow to the recorded refund account. */
    cancel: (args: SettlementCancelArgs, tx?: TxOptions) =>
      sendBuilt(ctx, settlementCancel(args), tx),

    /** Permissionless auto-refund once the window has lapsed. */
    refundExpired: (
      args: {
        tradeId: bigint
        tokenMint: PublicKey
        refundTokenAccount: PublicKey
        tokenProgramId: PublicKey
        hookAccounts?: AccountKey[]
      },
      tx?: TxOptions,
    ) => sendBuilt(ctx, settlementRefundExpired(args), tx),

    /** Reclaims the rent of a terminal trade (config authority only). */
    close: (
      args: {
        tradeId: bigint
        tokenMint: PublicKey
        authority: PublicKey
        tokenProgramId: PublicKey
        residualAccount: PublicKey
      },
      tx?: TxOptions,
    ) => sendBuilt(ctx, settlementClose(args), tx),

    /** Installs a per-mint Transfer Agent override. */
    setMintAgent: (
      args: { authority: PublicKey; tokenMint: PublicKey; secp256k1Pubkey: Uint8Array },
      tx?: TxOptions,
    ) => sendBuilt(ctx, settlementSetMintAgent(args), tx),

    clearMintAgent: (args: { authority: PublicKey; tokenMint: PublicKey }, tx?: TxOptions) =>
      sendBuilt(ctx, settlementClearMintAgent(args), tx),

    build: {
      initialize: settlementInitialize,
      setHooksAuthority: settlementSetHooksAuthority,
      setTransferAgent: settlementSetTransferAgent,
      initiate: settlementInitiate,
      settle: settlementSettle,
      cancel: settlementCancel,
      refundExpired: settlementRefundExpired,
      close: settlementClose,
      setMintAgent: settlementSetMintAgent,
      clearMintAgent: settlementClearMintAgent,
    },

    // ---------------------------------------------------------------- reads

    /** The fixed owner of every escrow — one whitelist entry for the whole desk. */
    escrowAuthority: settlementEscrowAuthority,
    escrowToken: settlementEscrowToken,
    pda: settlementVault,
    mintAgentPda: settlementMintAgent,

    config: () => accounts.settlementConfig(settlementConfig()),
    vault: (tradeId: bigint) => accounts.settlementVault(settlementVault(tradeId)),
    mintAgent: (mint: PublicKey) => accounts.mintAgent(settlementMintAgent(mint)),
    /** Every vault, or one taker's (program-account listing). */
    list: (options: { taker?: PublicKey } = {}) =>
      listSettlementVaults(ctx.connection, options),
  }
}

export type SettlementModule = ReturnType<typeof createSettlementModule>
