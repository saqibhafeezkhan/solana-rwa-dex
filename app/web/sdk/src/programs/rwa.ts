/**
 * RWA token layer: the mint factory (Token-2022 + transfer hook + metadata +
 * transfer fee), the whitelist-gated transfer, and hook-authority funding.
 *
 * `createMint` takes a `Keypair` (not a pubkey) because the System Program
 * requires the new mint account to sign; it is added as an extra signer for you.
 */
import type { Keypair, PublicKey } from '@solana/web3.js'
import {
  hookTransferAccounts,
  hooksFundHookAuthority,
  rwaCreateMint,
  rwaMintTo,
  rwaTransfer,
  rwaUpdateTransferFee,
  type RwaMintParamsInput,
  type RwaTransferArgs,
} from '../core/instructions-rails'
import {
  TOKEN_2022_PROGRAM_ID,
  ata,
  hookAuthority,
  transferHookWhitelistEntry,
} from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'

export interface CreateRwaMintArgs {
  /** The new mint's keypair (its pubkey becomes the mint address). */
  mint: Keypair
  authority: PublicKey
  params: RwaMintParamsInput
}

export function createRwaModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    /** Creates a Token-2022 RWA mint with hook, metadata and transfer fee. */
    createMint: (args: CreateRwaMintArgs, tx?: TxOptions) =>
      sendBuilt(ctx, rwaCreateMint(args.mint.publicKey, args.authority, args.params), {
        ...tx,
        signers: [args.mint, ...(tx?.signers ?? [])],
      }),

    mintTo: (
      args: { mint: PublicKey; destination: PublicKey; authority: PublicKey; amount: bigint },
      tx?: TxOptions,
    ) => sendBuilt(ctx, rwaMintTo(args.mint, args.destination, args.authority, args.amount), tx),

    /** A hook-checked transfer; both sides' whitelist entries are appended. */
    transfer: (args: RwaTransferArgs, tx?: TxOptions) =>
      sendBuilt(ctx, rwaTransfer(args), tx),

    updateTransferFee: (
      args: {
        mint: PublicKey
        feeAuthority: PublicKey
        basisPoints: number
        maximumFee: bigint
      },
      tx?: TxOptions,
    ) =>
      sendBuilt(
        ctx,
        rwaUpdateTransferFee(args.mint, args.feeAuthority, args.basisPoints, args.maximumFee),
        tx,
      ),

    /** Tops up the hook authority PDA with lamports; permissionless. */
    fundHookAuthority: (
      args: { payer: PublicKey; lamports: bigint },
      tx?: TxOptions,
    ) => sendBuilt(ctx, hooksFundHookAuthority(args.payer, args.lamports), tx),

    build: {
      createMint: rwaCreateMint,
      mintTo: rwaMintTo,
      transfer: rwaTransfer,
      updateTransferFee: rwaUpdateTransferFee,
    },

    // -------------------------------------------------------------- helpers

    /** The accounts a hook-gated transfer of `mint` must carry. */
    transferHookAccounts: (mint: PublicKey, sourceOwner: PublicKey, destinationOwner: PublicKey) =>
      hookTransferAccounts(mint, sourceOwner, destinationOwner),

    /** A wallet's (or escrow PDA's) whitelist entry address. */
    whitelistEntry: transferHookWhitelistEntry,
    hookAuthority,
    tokenProgram: TOKEN_2022_PROGRAM_ID,
    /** The associated token account for a mint this program issued. */
    ata: (owner: PublicKey, mint: PublicKey) => ata(owner, mint, TOKEN_2022_PROGRAM_ID),

    // ---------------------------------------------------------------- reads

    mint: (address: PublicKey) => accounts.mint(address),
    metadata: (mint: PublicKey) => accounts.metadata(mint),
    /** The fee in force this epoch (null when the mint charges none). */
    transferFee: (mint: PublicKey) => accounts.transferFee(mint),
    tokenAccount: (address: PublicKey) => accounts.tokenAccount(address),
  }
}

export type RwaModule = ReturnType<typeof createRwaModule>
