//! # KYC Gate Program
//!
//! The institutional bouncer for the RWA DEX.
//!
//! 1. The KYC authority calls `initialize_kyc_mint` once. It creates a
//!    **soulbound** attestation mint: a zero-decimal Token-2022 mint carrying the
//!    `NonTransferable` and `PermanentDelegate` extensions, whose mint, freeze
//!    and permanent-delegate authority is the `KycConfig` PDA.
//! 2. `issue_kyc` mints exactly one attestation token to an approved
//!    institution's token account; `revoke_kyc` burns it again.
//! 3. Before a swap, a pool CPIs into `verify_kyc`. If the trader's token account
//!    holds the attestation the swap proceeds; if not, the CPI fails with
//!    `NotKycApproved` and the swap reverts with it.
//!
//! ## Why a soulbound token instead of a whitelist PDA
//!
//! The attestation lives in the institution's *own* wallet, not in a list this
//! program maintains, so a pool verifies it with the accounts a swap already
//! carries: no extra PDA read per trade, and the answer cannot go stale in a
//! cache. Token-2022 enforces non-transferability itself, so an attestation can
//! never be sold, lent, or moved to a wallet the authority did not approve — the
//! only way to hold one is for the authority to mint it, and the only way to lose
//! one is for the authority to burn it. (The `transfer_hook` program in this repo
//! gates *who may receive an RWA token*; this program gates *who may trade on the
//! DEX*. They are complementary and independent.)
//!
//! ## Trust model
//!
//! Every authority the mint carries — mint, freeze, permanent delegate — is the
//! `KycConfig` PDA, never a wallet: not even the configured authority can mint,
//! burn or freeze outside the instructions below, and there is no instruction
//! that hands that power back to a key. `set_authority` rotates *which* key the
//! config names — so the gate is not bound to one signer forever — without
//! touching the mint, so the incoming key gains exactly the outgoing key's power
//! and no key ever gains direct mint or burn power. `verify_kyc` is read-only and
//! permissionless: anyone may ask whether a wallet is approved.
//!
//! Two rules of the token program shape the design, and neither is obvious:
//!
//! * **An attestation cannot be moved, by anyone.** `NonTransferable` is checked
//!   on every transfer, so not even the permanent delegate or an operator can
//!   move one to a wallet the authority did not approve.
//! * **Revoking needs a permanent delegate.** Token-2022 reserves burning to the
//!   account's owner (or a delegate); the mint authority cannot burn another
//!   wallet's tokens. Without the permanent delegate, `revoke_kyc` would only
//!   work if the institution signed it — which a sanctioned institution will not
//!   do.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::system_program::{create_account, CreateAccount};
use anchor_spl::token_2022::{
    self,
    spl_token_2022::{
        extension::ExtensionType, instruction as token_instruction, state::Mint as SplMint,
    },
    Token2022,
};
use anchor_spl::token_interface::{Mint, TokenAccount};

declare_id!("7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1");

/// Seed of the single `KycConfig` PDA: `[b"kyc-config"]`.
pub const KYC_CONFIG_SEED: &[u8] = b"kyc-config";

/// Seed of the attestation mint PDA: `[b"kyc-mint"]`.
pub const KYC_MINT_SEED: &[u8] = b"kyc-mint";

/// The attestation mint has no decimals: a token is a flag, not a currency.
pub const KYC_MINT_DECIMALS: u8 = 0;

/// One token is what "approved" means.
pub const KYC_ATTESTATION_AMOUNT: u64 = 1;

/// The extensions the attestation mint carries, in the order they are
/// initialized — which is the order the mint's TLV region records them.
///
/// * `NonTransferable` is what makes an attestation soulbound: the token program
///   rejects every transfer of this mint, by anyone.
/// * `PermanentDelegate` is what makes it *revocable*. Token-2022 reserves
///   burning to the account's owner (or a permanent delegate) — the mint
///   authority does not have that power — so without a permanent delegate,
///   `revoke_kyc` could not burn an institution's attestation without the
///   institution's own signature. Non-transferability still stops the delegate
///   from moving an attestation anywhere; it exists only to claw one back.
const MINT_EXTENSIONS: [ExtensionType; 2] = [
    ExtensionType::NonTransferable,
    ExtensionType::PermanentDelegate,
];

/// The BPF upgradeable loader, which owns this program's `ProgramData` account.
const BPF_LOADER_UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

/// Requires `signer` to be this program's **upgrade authority**, read from the
/// program's own `ProgramData` account.
///
/// `initialize_kyc_mint` hands out the KYC authority - the key that issues and
/// revokes every attestation - so it must not be claimable by whoever gets
/// there first. The upgrade authority is the key that can already change the
/// program, which makes it the natural owner of that one-time setup and needs
/// no key baked into the code.
///
/// The layout is the loader's stable bincode `UpgradeableLoaderState`: a 4-byte
/// variant tag (`ProgramData` == 3), an 8-byte slot, then the `Option<Pubkey>`
/// upgrade authority (1-byte tag, 32-byte key) - 45 bytes in all, which is the
/// length the loader itself documents (`size_of_programdata_metadata`).
fn require_upgrade_authority(program_data: &UncheckedAccount, signer: &Pubkey) -> Result<()> {
    let (expected, _) =
        Pubkey::find_program_address(&[crate::ID.as_ref()], &BPF_LOADER_UPGRADEABLE);
    require_keys_eq!(
        program_data.key(),
        expected,
        ErrorCode::UnauthorizedInitializer
    );
    require_keys_eq!(
        *program_data.owner,
        BPF_LOADER_UPGRADEABLE,
        ErrorCode::UnauthorizedInitializer
    );

    let data = program_data.try_borrow_data()?;
    require!(data.len() >= 45, ErrorCode::UnauthorizedInitializer);
    require!(
        u32::from_le_bytes(data[0..4].try_into().unwrap()) == 3,
        ErrorCode::UnauthorizedInitializer
    );
    // `None` (tag 0) means the program can no longer be upgraded; a keyless
    // authority cannot be proven, so initialization stays closed.
    require!(data[12] == 1, ErrorCode::UnauthorizedInitializer);
    let authority = Pubkey::new_from_array(data[13..45].try_into().unwrap());
    require_keys_eq!(authority, *signer, ErrorCode::UnauthorizedInitializer);
    Ok(())
}

#[program]
pub mod kyc_gate {
    use super::*;

    /// Creates the `KycConfig` PDA and the soulbound attestation mint.
    ///
    /// The mint is a PDA, so it needs no keypair and any integrator can derive
    /// its address from `KYC_MINT_SEED` alone. It is created by CPI to the system
    /// program — the mint account signs through its own PDA seeds — and every
    /// byte of its token state is written by the Token-2022 program through CPIs.
    /// This program never touches the mint's bytes, which it could not do anyway:
    /// the account belongs to Token-2022.
    pub fn initialize_kyc_mint(ctx: Context<InitializeKycMint>) -> Result<()> {
        // Only the upgrade authority may set the gate up: otherwise the first
        // caller would become the KYC authority for every pool that wires it.
        require_upgrade_authority(&ctx.accounts.program_data, &ctx.accounts.authority.key())?;

        let mint_info = ctx.accounts.kyc_mint.to_account_info();

        // 1. Size the account for both extensions. Token-2022 requires them to
        //    be initialized *before* the base mint and refuses to add one
        //    afterwards, and `process_initialize_mint` checks the account length
        //    against exactly this number, so the space is reserved up front and
        //    the helper is the only place that number comes from.
        let space = ExtensionType::try_calculate_account_len::<SplMint>(&MINT_EXTENSIONS)?;
        let lamports = Rent::get()?.minimum_balance(space);

        // 2. Allocate it rent-exempt and owned by Token-2022.
        let mint_seeds: &[&[u8]] = &[KYC_MINT_SEED, &[ctx.bumps.kyc_mint]];
        create_account(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                CreateAccount {
                    from: ctx.accounts.authority.to_account_info(),
                    to: mint_info.clone(),
                },
                &[mint_seeds],
            ),
            lamports,
            space as u64,
            &ctx.accounts.token_program.key(),
        )?;

        let config_key = ctx.accounts.kyc_config.key();

        // 3a. Soulbound. The extension's presence *is* the configuration: the
        //     token program now rejects every transfer of this mint.
        token_2022::initialize_non_transferable_mint(CpiContext::new(
            ctx.accounts.token_program.key(),
            token_2022::InitializeNonTransferableMint {
                mint: mint_info.clone(),
            },
        ))?;

        // 3b. Revocable: the config PDA as permanent delegate, so `revoke_kyc` can
        //     burn an attestation the institution is not co-operating with.
        let delegate_ix = token_instruction::initialize_permanent_delegate(
            &ctx.accounts.token_program.key(),
            &mint_info.key(),
            &config_key,
        )?;
        invoke(
            &delegate_ix,
            &[
                mint_info.clone(),
                ctx.accounts.token_program.to_account_info(),
            ],
        )?;

        // 4. Then the base mint: zero decimals, and the config PDA as both mint
        //    and freeze authority. The authority stored in the config can only
        //    reach that power through `issue_kyc` / `revoke_kyc`, which the
        //    program signs for; no key can mint directly.
        token_2022::initialize_mint2(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                token_2022::InitializeMint2 {
                    mint: mint_info.clone(),
                },
            ),
            KYC_MINT_DECIMALS,
            &config_key,
            Some(&config_key),
        )?;

        // 5. Record the config. The bump is cached because signing for the PDA
        //    (issuing, revoking) needs the canonical seeds back.
        let config = &mut ctx.accounts.kyc_config;
        config.authority = ctx.accounts.authority.key();
        config.kyc_mint = mint_info.key();
        config.bump = ctx.bumps.kyc_config;
        config.mint_bump = ctx.bumps.kyc_mint;

        msg!("KYC soulbound mint initialized: {}", mint_info.key());
        Ok(())
    }

    /// Issues one soulbound attestation to an institution.
    ///
    /// Only the configured authority may call this (enforced by `has_one` on the
    /// config, before the handler runs). An institution holds exactly one token:
    /// issuing to a wallet that already holds one is refused, which is what keeps
    /// a later `revoke_kyc` a complete revocation rather than a decrement.
    pub fn issue_kyc(ctx: Context<IssueKyc>) -> Result<()> {
        require!(
            ctx.accounts.institution_kyc_account.amount == 0,
            ErrorCode::AlreadyApproved
        );

        let config = &ctx.accounts.kyc_config;
        let config_seeds: &[&[u8]] = &[KYC_CONFIG_SEED, &[config.bump]];

        token_2022::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                token_2022::MintTo {
                    mint: ctx.accounts.kyc_mint.to_account_info(),
                    to: ctx.accounts.institution_kyc_account.to_account_info(),
                    authority: ctx.accounts.kyc_config.to_account_info(),
                },
                &[config_seeds],
            ),
            KYC_ATTESTATION_AMOUNT,
        )?;

        msg!(
            "KYC issued to institution {} (token account {})",
            ctx.accounts.institution_kyc_account.owner,
            ctx.accounts.institution_kyc_account.key()
        );
        Ok(())
    }

    /// Revokes KYC by burning the attestation token.
    ///
    /// Token-2022 reserves burning to the account's owner, or to a permanent
    /// delegate — the mint authority cannot burn another wallet's tokens — so the
    /// mint carries the config PDA as its permanent delegate and this handler
    /// signs for it. The token account is left in place: it now simply holds
    /// nothing, and the next `verify_kyc` refuses the institution.
    pub fn revoke_kyc(ctx: Context<RevokeKyc>) -> Result<()> {
        require!(
            ctx.accounts.institution_kyc_account.amount >= KYC_ATTESTATION_AMOUNT,
            ErrorCode::NotKycApproved
        );

        let config = &ctx.accounts.kyc_config;
        let config_seeds: &[&[u8]] = &[KYC_CONFIG_SEED, &[config.bump]];

        token_2022::burn(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                token_2022::Burn {
                    mint: ctx.accounts.kyc_mint.to_account_info(),
                    from: ctx.accounts.institution_kyc_account.to_account_info(),
                    authority: ctx.accounts.kyc_config.to_account_info(),
                },
                &[config_seeds],
            ),
            KYC_ATTESTATION_AMOUNT,
        )?;

        msg!(
            "KYC revoked for institution {}",
            ctx.accounts.institution_kyc_account.owner
        );
        Ok(())
    }

    /// The CPI entry point: called by a DEX pool before a swap.
    ///
    /// Read-only and permissionless — anyone may ask whether a wallet is
    /// approved. The wallet must be a signer, which is what stops a pool from
    /// clearing a trade with some *other* wallet's attestation.
    pub fn verify_kyc(ctx: Context<VerifyKyc>) -> Result<()> {
        let account = &ctx.accounts.user_kyc_account;
        check_attestation(
            account.amount,
            &account.mint,
            &ctx.accounts.kyc_config.kyc_mint,
            &account.owner,
            &ctx.accounts.user.key(),
        )
    }

    /// Rotates the KYC authority, so the gate can move onto a new multisig (or a
    /// new vault) without a redeploy. Only the current authority can call this
    /// (enforced by `has_one` on the config, before the handler runs).
    ///
    /// The new authority is taken as a plain key rather than a signer so that it
    /// can be a multisig vault PDA, which cannot sign a transaction directly —
    /// check the address off-chain before sending, since a typo hands the gate to
    /// a key nobody controls. The default pubkey is refused for exactly that
    /// reason.
    ///
    /// Nothing else changes: the mint's mint, freeze and permanent-delegate
    /// authorities stay the `KycConfig` PDA, so the incoming authority can only
    /// reach that power through `issue_kyc` / `revoke_kyc`, as the outgoing one
    /// did. Existing attestations are unaffected, and the institutions holding
    /// them need not be re-issued.
    pub fn set_authority(ctx: Context<SetAuthority>, new_authority: Pubkey) -> Result<()> {
        require_keys_neq!(
            new_authority,
            Pubkey::default(),
            ErrorCode::InvalidAuthority
        );

        let config = &mut ctx.accounts.kyc_config;
        let previous = config.authority;
        config.authority = new_authority;

        msg!("KYC authority updated: {} -> {}", previous, new_authority);
        Ok(())
    }
}

/// The whole of `verify_kyc`'s policy, as a pure function so it can be tested
/// without a validator: the account must belong to the KYC mint, belong to the
/// wallet being checked, and hold at least one attestation token.
fn check_attestation(
    amount: u64,
    account_mint: &Pubkey,
    expected_mint: &Pubkey,
    account_owner: &Pubkey,
    expected_owner: &Pubkey,
) -> Result<()> {
    require_keys_eq!(*account_mint, *expected_mint, ErrorCode::WrongMint);
    require_keys_eq!(*account_owner, *expected_owner, ErrorCode::WrongOwner);
    require!(amount >= KYC_ATTESTATION_AMOUNT, ErrorCode::NotKycApproved);
    Ok(())
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES & CONTEXTS
// -----------------------------------------------------------------------

/// The one piece of state this program owns. The attestations themselves live in
/// the institutions' wallets.
#[account]
pub struct KycConfig {
    /// The only key allowed to issue or revoke an attestation. A multisig in
    /// production.
    pub authority: Pubkey,
    /// The soulbound attestation mint.
    pub kyc_mint: Pubkey,
    /// Cached bump of this PDA, needed to sign the issue/revoke CPIs.
    pub bump: u8,
    /// Cached bump of the mint PDA, so integrators can derive the mint address
    /// without a `findProgramAddress` search.
    pub mint_bump: u8,
}

impl KycConfig {
    pub const LEN: usize = 8 + 32 + 32 + 1 + 1;
}

#[derive(Accounts)]
pub struct InitializeKycMint<'info> {
    #[account(
        init,
        payer = authority,
        space = KycConfig::LEN,
        seeds = [KYC_CONFIG_SEED],
        bump
    )]
    pub kyc_config: Account<'info, KycConfig>,

    /// The attestation mint. Created here by CPI to the system program, sized for
    /// the `NonTransferable` extension and owned by Token-2022, so its bytes are
    /// never laid out or read by this program.
    /// CHECK: the address is pinned by the `KYC_MINT_SEED` PDA seeds, and the
    /// account is created and initialized by the CPIs in the handler.
    #[account(mut, seeds = [KYC_MINT_SEED], bump)]
    pub kyc_mint: UncheckedAccount<'info>,

    /// Payer, and the authority recorded in the config.
    #[account(mut)]
    pub authority: Signer<'info>,

    /// This program's `ProgramData` account, which carries the upgrade
    /// authority that is allowed to initialize.
    /// CHECK: address and owner are pinned to this program's `ProgramData` PDA
    /// under the upgradeable loader, and the signer is matched against the
    /// authority it records, by `require_upgrade_authority`.
    pub program_data: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct IssueKyc<'info> {
    #[account(
        seeds = [KYC_CONFIG_SEED],
        bump = kyc_config.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub kyc_config: Account<'info, KycConfig>,

    #[account(mut, address = kyc_config.kyc_mint @ ErrorCode::WrongMint)]
    pub kyc_mint: InterfaceAccount<'info, Mint>,

    /// The institution's token account for the attestation mint. Token-2022
    /// accounts are required — a legacy token account cannot hold this mint.
    #[account(
        mut,
        constraint = institution_kyc_account.mint == kyc_config.kyc_mint @ ErrorCode::WrongMint,
    )]
    pub institution_kyc_account: InterfaceAccount<'info, TokenAccount>,

    /// The authority recorded in the config; it signs, it does not pay.
    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct RevokeKyc<'info> {
    #[account(
        seeds = [KYC_CONFIG_SEED],
        bump = kyc_config.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub kyc_config: Account<'info, KycConfig>,

    #[account(mut, address = kyc_config.kyc_mint @ ErrorCode::WrongMint)]
    pub kyc_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = institution_kyc_account.mint == kyc_config.kyc_mint @ ErrorCode::WrongMint,
    )]
    pub institution_kyc_account: InterfaceAccount<'info, TokenAccount>,

    /// The authority recorded in the config; it signs, it does not pay.
    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token2022>,
}

/// Context for rotating the KYC authority. The config is written, so it is
/// mutable this one time; the signer is the *current* authority.
#[derive(Accounts)]
pub struct SetAuthority<'info> {
    #[account(
        mut,
        seeds = [KYC_CONFIG_SEED],
        bump = kyc_config.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub kyc_config: Account<'info, KycConfig>,

    /// The authority recorded in the config; it signs, it does not pay.
    pub authority: Signer<'info>,
}

/// Context for a DEX pool calling `verify_kyc`. Read-only, and the trader is
/// required to be a signer.
#[derive(Accounts)]
pub struct VerifyKyc<'info> {
    #[account(seeds = [KYC_CONFIG_SEED], bump = kyc_config.bump)]
    pub kyc_config: Account<'info, KycConfig>,

    /// The wallet whose swap is being checked.
    pub user: Signer<'info>,

    /// That wallet's token account for the attestation mint.
    pub user_kyc_account: InterfaceAccount<'info, TokenAccount>,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Unauthorized to issue or revoke KYC")]
    Unauthorized,
    #[msg("User is not KYC approved")]
    NotKycApproved,
    #[msg("Token account does not belong to the KYC mint")]
    WrongMint,
    #[msg("Token account does not belong to the user")]
    WrongOwner,
    #[msg("Institution already holds a KYC attestation")]
    AlreadyApproved,
    #[msg("The new authority cannot be the default pubkey")]
    InvalidAuthority,
    #[msg("Only the program's upgrade authority may initialize the KYC gate")]
    UnauthorizedInitializer,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::AccountSerialize;

    fn key(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    /// The names all the checks report, so a rejection is explained rather than
    /// merely refused.
    fn rejection(
        amount: u64,
        account_mint: Pubkey,
        account_owner: Pubkey,
        expected_mint: Pubkey,
        expected_owner: Pubkey,
    ) -> String {
        format!(
            "{:?}",
            check_attestation(
                amount,
                &account_mint,
                &expected_mint,
                &account_owner,
                &expected_owner
            )
            .unwrap_err()
        )
    }

    #[test]
    fn an_attestation_verifies_for_the_matching_mint_and_owner() {
        let mint = key(1);
        let trader = key(2);
        // Exactly one token, and more than one, both count as approved.
        assert!(check_attestation(1, &mint, &mint, &trader, &trader).is_ok());
        assert!(check_attestation(7, &mint, &mint, &trader, &trader).is_ok());
    }

    #[test]
    fn an_empty_account_is_not_an_attestation() {
        let mint = key(1);
        let trader = key(2);
        let error = rejection(0, mint, trader, mint, trader);
        assert!(error.contains("NotKycApproved"), "{error}");
    }

    #[test]
    fn another_mints_account_is_refused() {
        let mint = key(1);
        let trader = key(2);
        // A wallet's account for some other token can never stand in.
        let error = rejection(1, key(9), trader, mint, trader);
        assert!(error.contains("WrongMint"), "{error}");
    }

    #[test]
    fn another_wallets_attestation_is_refused() {
        let mint = key(1);
        let trader = key(2);
        // Holding an attestation is not the check: holding *this wallet's* is.
        let error = rejection(1, mint, key(9), mint, trader);
        assert!(error.contains("WrongOwner"), "{error}");
    }

    #[test]
    fn the_attestation_mint_is_sized_for_its_extensions() {
        // The token program pads the account to the base-*account* length with
        // the account-type byte (166), then one TLV per extension: 4 bytes for the
        // zero-length NonTransferable, 4 + a 32-byte pubkey for the permanent
        // delegate. `process_initialize_mint` rejects any other length.
        let space = ExtensionType::try_calculate_account_len::<SplMint>(&MINT_EXTENSIONS).unwrap();
        assert_eq!(space, 206);
    }

    #[test]
    fn the_config_account_has_room_for_everything_it_serializes() {
        let config = KycConfig {
            authority: key(1),
            kyc_mint: key(2),
            bump: 254,
            mint_bump: 255,
        };
        let mut buffer = Vec::new();
        config.try_serialize(&mut buffer).unwrap();
        assert_eq!(
            buffer.len(),
            KycConfig::LEN,
            "LEN must cover the discriminator plus every field"
        );
    }
}
