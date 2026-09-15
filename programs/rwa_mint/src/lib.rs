//! # RWA Mint Program
//!
//! Creates Token-2022 mints with all required extensions for RWA tokens:
//! - TransferHook:      Points to our whitelist enforcement program
//! - MetadataPointer:   Points to on-chain metadata (ISIN/CUSIP/NAV URI)
//! - TokenMetadata:     Embedded metadata (name/symbol/uri)
//! - TransferFee:       Captures maker/taker fees at the token layer
//! - PermanentDelegate: Enables regulatory seizure (multisig-gated)
//!
//! The mint account is created manually (not via Anchor `init`) because
//! Token-2022 extension sizes are dynamic and must be calculated first.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::system_program::{create_account, CreateAccount};

use anchor_spl::token_2022::{
    spl_token_2022::{
        extension::{
            metadata_pointer, transfer_fee, transfer_hook, ExtensionType,
        },
        instruction as token_instruction,
        state::Mint as SplMint,
    },
    Token2022,
};
use anchor_spl::token_interface::{
    spl_token_metadata_interface::instruction as metadata_instruction,
    Mint, TokenAccount,
};

declare_id!("Mint111111111111111111111111111111111111111");

#[program]
pub mod rwa_mint {
    use super::*;

    /// Creates a new RWA token with all required Token-2022 extensions.
    pub fn create_rwa_mint(
        ctx: Context<CreateRwaMint>,
        params: RwaMintParams,
    ) -> Result<()> {
        let mint_info = ctx.accounts.mint.to_account_info();
        let payer_info = ctx.accounts.authority.to_account_info();
        let token_program_info = ctx.accounts.token_program.to_account_info();

        // ---------------------------------------------------------------
        // 1. Calculate required space for all extensions
        // ---------------------------------------------------------------
        let extensions = vec![
            ExtensionType::TransferHook,
            ExtensionType::MetadataPointer,
            ExtensionType::PermanentDelegate,
            ExtensionType::TransferFeeConfig,
            // Uncomment for soul-bound / non-transferable tokens:
            // ExtensionType::NonTransferable,
        ];

        let space = ExtensionType::try_calculate_account_len::<SplMint>(&extensions)?;
        let lamports = Rent::get()?.minimum_balance(space);

        // ---------------------------------------------------------------
        // 2. Create the mint account with the correct size
        //    (The mint must be a signer, which it is in the accounts struct.)
        // ---------------------------------------------------------------
        create_account(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                CreateAccount {
                    from: payer_info.clone(),
                    to: mint_info.clone(),
                },
            ),
            lamports,
            space as u64,
            ctx.accounts.token_program.key,
        )?;

        // ---------------------------------------------------------------
        // 3. Initialize each extension BEFORE the base mint.
        //    Order matters: Token-2022 requires extensions to be set up
        //    before the base mint is initialized.
        // ---------------------------------------------------------------

        // 3a. TransferHook extension
        let hook_ix = transfer_hook::instruction::initialize(
            ctx.accounts.token_program.key,
            &mint_info.key(),
            Some(ctx.accounts.authority.key()),   // hook authority
            Some(params.transfer_hook_program),   // hook program id
        )?;
        invoke(
            &hook_ix,
            &[mint_info.clone(), token_program_info.clone()],
        )?;

        // 3b. MetadataPointer extension (points to the mint itself)
        let metadata_pointer_ix = metadata_pointer::instruction::initialize(
            ctx.accounts.token_program.key,
            &mint_info.key(),
            Some(ctx.accounts.authority.key()), // metadata authority
            Some(mint_info.key()),              // metadata address = mint
        )?;
        invoke(
            &metadata_pointer_ix,
            &[mint_info.clone(), token_program_info.clone()],
        )?;

        // 3c. PermanentDelegate extension
        let permanent_delegate_ix = token_instruction::initialize_permanent_delegate(
            ctx.accounts.token_program.key,
            &mint_info.key(),
            &params.delegate,
        )?;
        invoke(
            &permanent_delegate_ix,
            &[mint_info.clone(), token_program_info.clone()],
        )?;

        // 3d. TransferFeeConfig extension (initial fee: 0 bps, 0 max)
        let transfer_fee_ix =
            transfer_fee::instruction::initialize_transfer_fee_config(
                &ctx.accounts.token_program.key(),
                &mint_info.key(),
                Some(&params.fee_authority.key()), // config authority
                Some(&params.fee_authority.key()), // withdraw withheld authority
                0,                          // transfer_fee_basis_points
                0,                          // maximum_fee
            )?;
        invoke(
            &transfer_fee_ix,
            &[mint_info.clone(), token_program_info.clone()],
        )?;

        // ---------------------------------------------------------------
        // 4. Initialize the base mint (must come AFTER extensions)
        // ---------------------------------------------------------------
        let init_mint_ix = token_instruction::initialize_mint2(
            ctx.accounts.token_program.key,
            &mint_info.key(),
            &ctx.accounts.authority.key(),
            Some(&ctx.accounts.authority.key()),
            params.decimals,
        )?;
        invoke(
            &init_mint_ix,
            &[mint_info.clone(), token_program_info.clone()],
        )?;

        // ---------------------------------------------------------------
        // 5. Initialize on-chain token metadata (name, symbol, URI)
        // ---------------------------------------------------------------
        let metadata_init_ix = metadata_instruction::initialize(
            &ctx.accounts.token_program.key(),
            &mint_info.key(),                   // metadata account (== mint)
            &ctx.accounts.authority.key(),      // update authority
            &mint_info.key(),                   // mint
            &ctx.accounts.authority.key(),      // mint authority
            params.metadata.name.clone(),
            params.metadata.symbol.clone(),
            params.metadata.uri.clone(),
        );
        invoke(
            &metadata_init_ix,
            &[
                mint_info.clone(),                  // metadata account
                ctx.accounts.authority.to_account_info(), // update authority
                mint_info.clone(),                  // mint
                ctx.accounts.authority.to_account_info(), // mint authority
            ],
        )?;

        msg!("RWA Mint created: {}", mint_info.key());
        Ok(())
    }

    /// Mints RWA tokens to an approved institution.
    /// Only the mint authority (multisig) can call this.
    pub fn mint_to(
        ctx: Context<MintTo>,
        amount: u64,
    ) -> Result<()> {
        let cpi_accounts = anchor_spl::token_2022::MintTo {
            mint: ctx.accounts.mint.to_account_info(),
            to: ctx.accounts.destination.to_account_info(),
            authority: ctx.accounts.authority.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(
            ctx.accounts.token_program.key(),
            cpi_accounts,
        );

        anchor_spl::token_2022::mint_to(cpi_ctx, amount)?;

        msg!(
            "Minted {} RWA tokens to {}",
            amount,
            ctx.accounts.destination.owner
        );
        Ok(())
    }

    /// Transfers RWA tokens. The Transfer Hook is automatically invoked by
    /// Token-2022, which receives the Merkle proof from the extra instruction
    /// data appended below.
    pub fn transfer<'info>(
        ctx: Context<'info, Transfer<'info>>,
        amount: u64,
        proof: Vec<[u8; 32]>,
    ) -> Result<()> {
        // 1. Build the standard transfer_checked instruction
        let mut transfer_ix = token_instruction::transfer_checked(
            &ctx.accounts.token_program.key(),
            &ctx.accounts.source.key(),
            &ctx.accounts.mint.key(),
            &ctx.accounts.destination.key(),
            &ctx.accounts.authority.key(),
            &[],
            amount,
            ctx.accounts.mint.decimals,
        )?;

        // 2. Append Borsh-serialized Vec<[u8;32]> proof for the hook.
        //    Token-2022 forwards any trailing bytes to the hook program.
        let mut hook_data = Vec::with_capacity(4 + proof.len() * 32);
        hook_data.extend_from_slice(&(proof.len() as u32).to_le_bytes());
        for p in &proof {
            hook_data.extend_from_slice(p);
        }
        transfer_ix.data.extend_from_slice(&hook_data);

        // 3. Collect accounts: standard token accounts + any remaining
        //    accounts (hook program, config PDA, extra account meta list).
        let mut account_infos = vec![
            ctx.accounts.source.to_account_info(),
            ctx.accounts.mint.to_account_info(),
            ctx.accounts.destination.to_account_info(),
            ctx.accounts.authority.to_account_info(),
        ];
        for acc in ctx.remaining_accounts.iter() {
            account_infos.push(acc.clone());
        }

        invoke(&transfer_ix, &account_infos)?;

        msg!("Transferred {} RWA tokens", amount);
        Ok(())
    }

    /// Updates the transfer fee (only fee authority can call this).
    pub fn update_transfer_fee(
        ctx: Context<UpdateTransferFee>,
        new_fee_basis_points: u16,
        new_maximum_fee: u64,
    ) -> Result<()> {
        let ix = transfer_fee::instruction::set_transfer_fee(
            ctx.accounts.token_program.key,
            &ctx.accounts.mint.key(),
            &ctx.accounts.authority.key(),
            &[], // no multisig co-signers
            new_fee_basis_points,
            new_maximum_fee,
        )?;

        invoke(
            &ix,
            &[
                ctx.accounts.mint.to_account_info(),
                ctx.accounts.authority.to_account_info(),
                ctx.accounts.token_program.to_account_info(),
            ],
        )?;

        msg!(
            "Transfer fee updated: {} bps, max {}",
            new_fee_basis_points,
            new_maximum_fee
        );
        Ok(())
    }
}

// -----------------------------------------------------------------------
// PARAMETERS
// -----------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct RwaMintParams {
    pub decimals: u8,
    pub transfer_hook_program: Pubkey,
    pub delegate: Pubkey,
    pub fee_authority: Pubkey,
    pub metadata: TokenMetadataParams,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct TokenMetadataParams {
    pub name: String,
    pub symbol: String,
    pub uri: String, // Points to off-chain ISIN/CUSIP/NAV data
}

// -----------------------------------------------------------------------
// ACCOUNT CONTEXTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
pub struct CreateRwaMint<'info> {
    /// The mint account — created manually with dynamic size.
    /// It must be a signer so the System Program can create it.
    #[account(mut)]
    pub mint: Signer<'info>,

    /// Payer + mint authority + metadata authority.
    #[account(mut)]
    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token2022>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MintTo<'info> {
    #[account(mut)]
    pub mint: InterfaceAccount<'info, Mint>,

    #[account(mut)]
    pub destination: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token2022>,
}

#[derive(Accounts)]
pub struct Transfer<'info> {
    #[account(mut)]
    pub source: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub destination: InterfaceAccount<'info, TokenAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token2022>,

    // Remaining accounts are forwarded to the transfer hook:
    // - The hook program
    // - The `extra-account-metas` PDA
    // - The hook `config` PDA
    // - Any other accounts required by the hook
}

#[derive(Accounts)]
pub struct UpdateTransferFee<'info> {
    #[account(mut)]
    pub mint: InterfaceAccount<'info, Mint>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub token_program: Program<'info, Token2022>,
}