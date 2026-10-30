/**
 * Hook programs: whitelist governance (`transfer_hook`) and the settlement
 * escrow router (`hooks`).
 *
 * The router's `after_swap` is invoked by the AMM pools through CPI, not by a
 * wallet; it is exposed for tooling and tests. `fund` keeps its authority PDA
 * rent-exempt.
 */
import type { PublicKey } from '@solana/web3.js'
import {
  hooksAfterSwap,
  hooksFundHookAuthority,
  transferHookCloseWhitelistEntry,
  transferHookInitExtraAccountMetaList,
  transferHookInitialize,
  transferHookSetAuthority,
  transferHookSetPaused,
  transferHookSetWhitelistEntry,
  transferHookUpdateRoot,
  type HooksAfterSwapArgs,
} from '../core/instructions-rails'
import { hookAuthority, transferHookConfig, transferHookWhitelistEntry } from '../core/pdas'
import { accountsFor } from '../accounts'
import { hexToBytes } from '../merkle'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'

export function createHooksModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    /** Tops up the hook authority PDA with lamports; permissionless. */
    fund: (payer: PublicKey, lamports: bigint, tx?: TxOptions) =>
      sendBuilt(ctx, hooksFundHookAuthority(payer, lamports), tx),

    /** CPI-only on chain; exposed for tooling and local tests. */
    afterSwap: (args: HooksAfterSwapArgs, tx?: TxOptions) =>
      sendBuilt(ctx, hooksAfterSwap(args), tx),

    // ------------------------------------------------- whitelist governance

    /** One-time transfer-hook setup, upgrade authority only. */
    initialize: (authority: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, transferHookInitialize(authority), tx),

    /** Publishes a new whitelist root; a hex string or the raw 32 bytes. */
    updateRoot: (authority: PublicKey, root: Uint8Array | string, tx?: TxOptions) =>
      sendBuilt(
        ctx,
        transferHookUpdateRoot(authority, typeof root === 'string' ? hexToBytes(root) : root),
        tx,
      ),

    /**
     * Creates or refreshes one institution's entry. Re-running this with the
     * path for the new root is how a rotation is applied.
     */
    setWhitelistEntry: (
      authority: PublicKey,
      owner: PublicKey,
      proof: Uint8Array[],
      tx?: TxOptions,
    ) => sendBuilt(ctx, transferHookSetWhitelistEntry(authority, owner, proof), tx),

    closeWhitelistEntry: (authority: PublicKey, owner: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, transferHookCloseWhitelistEntry(authority, owner), tx),

    setPaused: (authority: PublicKey, paused: boolean, tx?: TxOptions) =>
      sendBuilt(ctx, transferHookSetPaused(authority, paused), tx),

    setAuthority: (currentAuthority: PublicKey, newAuthority: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, transferHookSetAuthority(currentAuthority, newAuthority), tx),

    /** Registers a mint's extra-account-metas so Token-2022 resolves its hook. */
    initExtraAccountMetaList: (payer: PublicKey, rwaMint: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, transferHookInitExtraAccountMetaList(payer, rwaMint), tx),

    build: {
      fund: hooksFundHookAuthority,
      afterSwap: hooksAfterSwap,
      initialize: transferHookInitialize,
      updateRoot: transferHookUpdateRoot,
      setWhitelistEntry: transferHookSetWhitelistEntry,
      closeWhitelistEntry: transferHookCloseWhitelistEntry,
      setPaused: transferHookSetPaused,
      setAuthority: transferHookSetAuthority,
      initExtraAccountMetaList: transferHookInitExtraAccountMetaList,
    },

    // ---------------------------------------------------------------- reads

    authorityPda: hookAuthority,
    entryPda: transferHookWhitelistEntry,
    config: () => accounts.hookConfig(transferHookConfig()),
    whitelistEntry: (owner: PublicKey) =>
      accounts.account(transferHookWhitelistEntry(owner)),
  }
}

export type HooksModule = ReturnType<typeof createHooksModule>
