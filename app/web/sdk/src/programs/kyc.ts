/**
 * KYC gate: one soulbound Token-2022 attestation per institution, minted by the
 * issuing authority. A pool wired to the gate checks the trader's attestation
 * ATA (not a balance list), so `verify` is the exact check a swap will run.
 */
import type { PublicKey } from '@solana/web3.js'
import {
  kycInitializeMint,
  kycIssue,
  kycRevoke,
  kycSetAuthority,
  kycVerify,
} from '../core/instructions-rails'
import { TOKEN_2022_PROGRAM_ID, ata, kycConfig, kycMint } from '../core/pdas'
import { accountsFor } from '../accounts'
import { sendBuilt, type SdkContext, type TxOptions } from '../context'

export function createKycModule(ctx: SdkContext) {
  const accounts = accountsFor(ctx.connection)

  return {
    /** One-time setup, upgrade authority only. */
    initializeMint: (authority: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, kycInitializeMint(authority), tx),

    /** Mints one attestation to `institution`. */
    issue: (authority: PublicKey, institution: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, kycIssue(authority, institution), tx),

    /** Burns the attestation; the institution does not sign. */
    revoke: (authority: PublicKey, institution: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, kycRevoke(authority, institution), tx),

    /** The gate's own check — succeeds only if the attestation is valid today. */
    verify: (user: PublicKey, tx?: TxOptions) => sendBuilt(ctx, kycVerify(user), tx),

    setAuthority: (currentAuthority: PublicKey, newAuthority: PublicKey, tx?: TxOptions) =>
      sendBuilt(ctx, kycSetAuthority(currentAuthority, newAuthority), tx),

    build: {
      initializeMint: kycInitializeMint,
      issue: kycIssue,
      revoke: kycRevoke,
      verify: kycVerify,
      setAuthority: kycSetAuthority,
    },

    // ---------------------------------------------------------------- reads

    /** The gate's config and mint PDAs. */
    configPda: kycConfig,
    mintPda: kycMint,
    config: () => accounts.kycConfig(kycConfig()),
    /** The attestation account a wallet's KYC is proven by. */
    attestation: (owner: PublicKey) => ata(owner, kycMint(), TOKEN_2022_PROGRAM_ID),
    attestationBalance: (owner: PublicKey) =>
      accounts.tokenAccount(ata(owner, kycMint(), TOKEN_2022_PROGRAM_ID)),
  }
}

export type KycModule = ReturnType<typeof createKycModule>
