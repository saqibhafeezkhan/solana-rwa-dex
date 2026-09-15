//! # RWA Transfer Hook Program
//!
//! This program implements the SPL Transfer Hook Interface for Token-2022.
//! It enforces a Merkle-tree whitelist on every transfer of RWA tokens.
//! Only institutions on the whitelist can receive or send the token.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{create_account, CreateAccount};
// Import keccak hashing (split into its own crate in Solana SDK v5)
use solana_keccak_hasher::{hash, hashv};
// FIX: Import SPL token types
use anchor_spl::token_interface::{Mint, TokenAccount};

use spl_tlv_account_resolution::account::ExtraAccountMeta;
use spl_tlv_account_resolution::seeds::Seed;
use spl_tlv_account_resolution::state::ExtraAccountMetaList;
use spl_transfer_hook_interface::instruction::ExecuteInstruction;

declare_id!("Hook111111111111111111111111111111111111111");

#[program]
pub mod transfer_hook {
    use super::*;

    /// Initializes the hook program configuration.
    /// Creates a PDA to store the Merkle root and authority.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        config.merkle_root = [0u8; 32]; // Empty initially
        config.bump = ctx.bumps.config;

        msg!("Transfer Hook initialized. Authority: {}", config.authority);
        Ok(())
    }

    /// Updates the Merkle root of the whitelist.
    /// Only the authority (multisig) can call this.
    pub fn update_root(ctx: Context<UpdateRoot>, new_root: [u8; 32]) -> Result<()> {
        let config = &mut ctx.accounts.config;

        require_keys_eq!(
            config.authority,
            ctx.accounts.authority.key(),
            ErrorCode::Unauthorized
        );

        config.merkle_root = new_root;

        msg!("Merkle root updated to: {:?}", new_root);
        Ok(())
    }

    /// Initializes the ExtraAccountMetaList PDA.
    /// This tells Token-2022 to automatically resolve and pass the Config PDA during `execute`.
    pub fn initialize_extra_account_meta_list(
        ctx: Context<InitializeExtraAccountMetaList>,
    ) -> Result<()> {
        // Define the extra accounts needed during execute
        let account_metas = vec![ExtraAccountMeta::new_with_seeds(
            &[Seed::Literal {
                bytes: b"config".to_vec(),
            }],
            false, // is_signer
            true,  // is_writable
        )?];

        // Calculate required account size and rent
        let account_size = ExtraAccountMetaList::size_of(account_metas.len())? as u64;
        let lamports = Rent::get()?.minimum_balance(account_size as usize);

        let mint = ctx.accounts.mint.key();
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"extra-account-metas",
            mint.as_ref(),
            &[ctx.bumps.extra_account_meta_list],
        ]];

        // Create the ExtraAccountMetaList account via CPI (required for dynamic sizing)
        create_account(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                CreateAccount {
                    from: ctx.accounts.payer.to_account_info(),
                    to: ctx.accounts.extra_account_meta_list.to_account_info(),
                },
            )
            .with_signer(signer_seeds),
            lamports,
            account_size,
            ctx.program_id,
        )?;

        // Write the configuration to the ExtraAccountMetaList account
        ExtraAccountMetaList::init::<ExecuteInstruction>(
            &mut ctx.accounts.extra_account_meta_list.try_borrow_mut_data()?,
            &account_metas,
        )?;

        msg!("ExtraAccountMetaList initialized with Config PDA resolution");
        Ok(())
    }

    /// THE CORE HOOK - Called by Token-2022 on every transfer.
    /// NOTE: For Token-2022 integration, a fallback handler is needed to route
    /// the SPL Transfer Hook discriminator to this function.
    pub fn execute<'info>(
        ctx: Context<Execute<'info>>,
        _amount: u64,
        proof: Vec<[u8; 32]>,
    ) -> Result<()> {
        let config = &ctx.accounts.config;

        // 1. Check if whitelist is initialized
        require!(
            config.merkle_root != [0u8; 32],
            ErrorCode::WhitelistNotInitialized
        );

        // 2. Securely get the destination owner directly from the destination token account
        let destination_owner_key = ctx.accounts.destination_token.owner;

        // 3. Hash the destination wallet address to create the leaf node
        let mut current_hash = hash(destination_owner_key.as_ref()).to_bytes();

        // 4. Compute the expected root using the provided proof iteratively
        for p in proof.iter() {
            // Sort hashes to ensure consistent parent computation regardless of left/right child order
            let mut hashes = [current_hash, *p];
            hashes.sort_unstable();

            // Hash the sorted pair together
            current_hash = hashv(&[hashes[0].as_ref(), hashes[1].as_ref()]).to_bytes();
        }

        // 5. Verify the computed root matches the stored root
        require!(
            current_hash == config.merkle_root,
            ErrorCode::DestinationNotWhitelisted
        );

        msg!("Transfer approved. Destination: {}", destination_owner_key);
        Ok(())
    }
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES
// -----------------------------------------------------------------------

#[account]
pub struct Config {
    pub authority: Pubkey,
    pub merkle_root: [u8; 32],
    pub bump: u8,
}

impl Config {
    pub const LEN: usize = 8 + 32 + 32 + 1; // discriminator + authority + root + bump
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = Config::LEN,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, Config>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateRoot<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump = config.bump
    )]
    pub config: Account<'info, Config>,

    #[account(mut)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct InitializeExtraAccountMetaList<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// CHECK: ExtraAccountMetaList Account, must use these exact seeds per SPL standard
    #[account(
        mut,
        seeds = [b"extra-account-metas", mint.key().as_ref()],
        bump
    )]
    pub extra_account_meta_list: UncheckedAccount<'info>,

    pub mint: InterfaceAccount<'info, Mint>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Execute<'info> {
    // 1-4: Standard accounts passed by Token-2022 in this exact order
    #[account(token::mint = mint, token::authority = owner)]
    pub source_token: InterfaceAccount<'info, TokenAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(token::mint = mint)]
    pub destination_token: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: source token account owner
    pub owner: UncheckedAccount<'info>,

    // 5: Extra accounts resolved from ExtraAccountMetaList in the order defined
    #[account(
        seeds = [b"extra-account-metas", mint.key().as_ref()],
        bump
    )]
    pub extra_account_meta_list: UncheckedAccount<'info>,

    /// Config PDA - automatically resolved and appended by Token-2022
    #[account(seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
}

#[error_code]
pub enum ErrorCode {
    #[msg("The destination wallet is not on the institutional whitelist.")]
    DestinationNotWhitelisted,
    #[msg("The whitelist root has not been initialized.")]
    WhitelistNotInitialized,
    #[msg("Only the designated authority can update the whitelist.")]
    Unauthorized,
}
