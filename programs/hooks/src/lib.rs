//! # AMM Hooks Program
//!
//! This program acts as the "Hook" for the RWA DEX.
//! In a production CLMM/DLMM, the pool calls `after_swap` in this program.
//! Instead of letting the user take the tokens immediately, this Hook
//! intercepts them and locks them in the T+N Settlement Escrow.
//!
//! The hook moves tokens with a PDA of *this* program
//! (`hook_authority`), which must own the AMM vault token account. That
//! PDA signs the CPI into the Settlement program via `invoke_signed`.
//!
//! ## Wiring (AMM → Hook → Settlement)
//!
//! `after_swap` is the settlement rail of the wired AMM pools. The pool
//! transfers the swap output into a **per-trade custody account owned by
//! the hook authority** (never into a user wallet, and never out of the
//! pool's shared vaults — the vault-ownership constraint below makes any
//! other source fail), then calls this instruction. The hook:
//!
//! 1. computes the unlock slot `Clock::slot + settlement_slots`,
//! 2. derives the settlement vault PDA `[b"settlement", trade_id_le]`,
//! 3. pins the escrow token account to that vault PDA's **ATA** (created
//!    idempotently by the Settlement program itself, so it does not have
//!    to exist yet), and
//! 4. CPIs `settlement::initiate_settlement` with `hook_authority` as the
//!    signing `authority` and the settlement **config** attached, so the
//!    config's pool-gate policy whitelists this program's PDA via
//!    `set_hooks_authority` and lets the trade through.
//!
//! `settlement_program` is a typed `Program<Settlement>` account — the CPI
//! callee is pinned by address, so a look-alike program cannot receive the
//! hook authority's signature.

use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::get_associated_token_address_with_program_id,
    token_interface::{Mint, TokenAccount, TokenInterface},
};
use settlement::{cpi::accounts::InitiateSettlement, program::Settlement};

declare_id!("3DN93k4QRu7Ds2aL2Bba5VnDu18EoRzTNV18jJizAdoY");

#[program]
pub mod hooks {
    use super::*;

    /// Called by the AMM immediately after the swap math is calculated.
    /// `amount_out` tokens are sitting in the hook-authority-owned custody
    /// account. We CPI into the Settlement program to move them to the escrow.
    pub fn after_swap<'info>(
        ctx: Context<'info, AfterSwap<'info>>,
        trade_id: u64,
        settlement_slots: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;
        let target_settlement_slot = clock
            .slot
            .checked_add(settlement_slots)
            .ok_or_else(|| error!(ErrorCode::SettlementSlotOverflow))?;

        // The vault PDA of the Settlement program for this trade. The escrow
        // token account must be exactly this vault's ATA.
        let (settlement_vault, _) = Pubkey::find_program_address(
            &[b"settlement", &trade_id.to_le_bytes()],
            &settlement::ID,
        );

        // Pin the escrow account by address instead of parsing it: it may not
        // exist yet (the Settlement program creates it idempotently), so there
        // is nothing to parse. The ATA derivation guarantees that once created
        // it is a token account of this mint owned by the vault PDA.
        let expected_escrow = get_associated_token_address_with_program_id(
            &settlement_vault,
            &ctx.accounts.token_mint.key(),
            &ctx.accounts.token_program.key(),
        );
        require_keys_eq!(
            ctx.accounts.settlement_vault_token.key(),
            expected_escrow,
            ErrorCode::InvalidVaultTokenAccount
        );

        // Construct the CPI accounts for the Settlement program.
        //
        // `pool` is `Some(settlement_program_info)`: Anchor's `Option<T>`
        // treats an account whose key equals the executing program's own ID
        // as "absent", and the settlement program account is exactly that
        // sentinel (the hook authority is not a pool PDA; the config's
        // `hooks_authority` whitelist is what authorizes this flow on a
        // pool-gated config).
        let cpi_accounts = InitiateSettlement {
            settlement_vault: ctx.accounts.settlement_vault.to_account_info(),
            source_token_account: ctx.accounts.amm_vault.to_account_info(),
            settlement_vault_token: ctx.accounts.settlement_vault_token.to_account_info(),
            token_mint: ctx.accounts.token_mint.to_account_info(),
            taker: ctx.accounts.taker.to_account_info(),
            authority: ctx.accounts.hook_authority.to_account_info(),
            pool: Some(ctx.accounts.settlement_program.to_account_info()),
            settlement_config: ctx.accounts.settlement_config.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            associated_token_program: ctx.accounts.associated_token_program.to_account_info(),
            system_program: ctx.accounts.system_program.to_account_info(),
        };

        // The Settlement program's `authority` is our `hook_authority` PDA:
        // it "signs" the inner transfer through these seeds.
        let hook_bump = ctx.bumps.hook_authority;
        let signer_seeds: &[&[&[u8]]] = &[&[b"hook_authority", &[hook_bump]]];
        let cpi_ctx = CpiContext::new(
            ctx.accounts.settlement_program.key(),
            cpi_accounts,
        )
        .with_signer(signer_seeds);

        // Execute the CPI to lock the funds
        settlement::cpi::initiate_settlement(cpi_ctx, trade_id, target_settlement_slot)?;

        msg!(
            "AfterSwap hook executed. Funds routed to settlement vault {}.",
            settlement_vault
        );
        Ok(())
    }

    /// Tops up the hook authority PDA. The Settlement program charges rent
    /// for every settlement vault it creates to the `authority` passed in the
    /// CPI - which is this PDA - so it must hold lamports. Anyone may fund it.
    pub fn fund_hook_authority(
        ctx: Context<FundHookAuthority>,
        lamports: u64,
    ) -> Result<()> {
        let ix = anchor_lang::solana_program::system_instruction::transfer(
            &ctx.accounts.payer.key(),
            &ctx.accounts.hook_authority.key(),
            lamports,
        );
        anchor_lang::solana_program::program::invoke(
            &ix,
            &[
                ctx.accounts.payer.to_account_info(),
                ctx.accounts.hook_authority.to_account_info(),
            ],
        )?;

        msg!("Hook authority funded with {} lamports", lamports);
        Ok(())
    }
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
#[instruction(trade_id: u64)]
pub struct AfterSwap<'info> {
    /// PDA of this Hooks program. Acts as the authority to move tokens from
    /// the AMM vault; signs the settlement CPI with its seeds. Derived here
    /// rather than passed as a free `signer` AccountInfo - a plain
    /// `AccountInfo` marked `signer` would let any caller satisfy the
    /// "authority" role by signing with an arbitrary wallet.
    #[account(seeds = [b"hook_authority"], bump)]
    pub hook_authority: AccountInfo<'info>,

    /// The user who executed the swap
    /// CHECK: Recipient recorded in the settlement vault; bound into the
    /// Transfer Agent's signed message at settle time.
    pub taker: AccountInfo<'info>,

    /// The per-trade custody account holding the swap output. It must be
    /// owned by the hook authority PDA - the pool routes the trade's output
    /// here before calling - which is the only way this CPI can move funds
    /// and the guarantee that the pool's shared vaults stay untouched.
    #[account(
        mut,
        constraint = amm_vault.owner == hook_authority.key()
            @ ErrorCode::VaultNotOwnedByHook,
        constraint = amm_vault.mint == token_mint.key()
            @ ErrorCode::MintMismatch
    )]
    pub amm_vault: InterfaceAccount<'info, TokenAccount>,

    /// The mint of the token being locked
    pub token_mint: InterfaceAccount<'info, Mint>,

    /// The Settlement Vault PDA (created via CPI by the Settlement program).
    #[account(
        mut,
        constraint = settlement_vault.key()
            == Pubkey::find_program_address(
                &[b"settlement", &trade_id.to_le_bytes()],
                &settlement::ID
            )
            .0 @ ErrorCode::InvalidSettlementVault
    )]
    pub settlement_vault: AccountInfo<'info>,

    /// The escrow token account of the Settlement Vault PDA. Pinned to the
    /// vault's ATA in the handler; it may not exist yet (the Settlement
    /// program creates it).
    /// CHECK: address-pinned to the vault PDA's ATA in the handler.
    #[account(mut)]
    pub settlement_vault_token: AccountInfo<'info>,

    /// The clearinghouse config. Passed through to the Settlement program,
    /// whose gate policy authorizes this program's hook authority. Typed
    /// against the Settlement program's own account type, so the
    /// discriminator (and therefore the canonical config) is verified here.
    pub settlement_config: Account<'info, settlement::SettlementConfig>,

    /// The Settlement program itself. Typed, so the CPI callee is pinned by
    /// address - `invoke_signed` would otherwise hand the hook authority's
    /// signature to whatever program was passed here.
    pub settlement_program: Program<'info, Settlement>,

    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, anchor_spl::associated_token::AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FundHookAuthority<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// CHECK: The hooks program PDA, derived by seed; receives the lamports.
    #[account(mut, seeds = [b"hook_authority"], bump)]
    pub hook_authority: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[error_code]
pub enum ErrorCode {
    #[msg("The AMM vault is not owned by the hook authority PDA")]
    VaultNotOwnedByHook,
    #[msg("Token account mint does not match the settlement mint")]
    MintMismatch,
    #[msg("Settlement vault is not the expected settlement program PDA")]
    InvalidSettlementVault,
    #[msg("Escrow token account is not the settlement vault PDA's ATA")]
    InvalidVaultTokenAccount,
    #[msg("Settlement slot addition overflowed")]
    SettlementSlotOverflow,
}
