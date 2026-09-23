//! # RWA Transfer Hook Program
//!
//! This program implements the SPL Transfer Hook Interface for Token-2022.
//! It enforces a Merkle-tree whitelist on every transfer of RWA tokens.
//! Only institutions on the whitelist can receive or send the token.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{create_account, CreateAccount};
use anchor_spl::token_2022::spl_token_2022::extension::transfer_hook::TransferHook as TransferHookExtension;
use anchor_spl::token_interface::{get_mint_extension_data, Mint, TokenAccount};
// Import keccak hashing (split into its own crate in Solana SDK v5)
use solana_keccak_hasher::hashv;

use spl_discriminator::discriminator::SplDiscriminate;
use spl_tlv_account_resolution::account::ExtraAccountMeta;
use spl_tlv_account_resolution::seeds::Seed;
use spl_tlv_account_resolution::state::ExtraAccountMetaList;
use spl_transfer_hook_interface::instruction::ExecuteInstruction;

declare_id!("fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS");

/// Domain separation prefixes for the whitelist tree (RFC 6962 style).
///
/// Leaves and inner nodes are both Keccak-256, so without prefixes an inner
/// node could be presented as a leaf. Every leaf is `keccak(0x00 || pubkey)`
/// and every parent `keccak(0x01 || left || right)`, with the pair sorted so
/// proofs stay position-free. The client tree builder has to use the same two
/// prefixes - see the README.
const LEAF_PREFIX: [u8; 1] = [0x00];
const NODE_PREFIX: [u8; 1] = [0x01];

/// A root of all zeros is indistinguishable from "never set" and would halt
/// every transfer, so it is rejected rather than stored.
const ZERO_ROOT: [u8; 32] = [0u8; 32];

/// The BPF upgradeable loader, which owns this program's `ProgramData` account.
const BPF_LOADER_UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

/// Requires `signer` to be this program's **upgrade authority**, read from the
/// program's own `ProgramData` account.
///
/// `initialize` is the one instruction that hands out the whitelist authority,
/// so it must not be claimable by whoever gets there first. The upgrade
/// authority is the key that can already change the program, which makes it the
/// natural owner of that one-time setup and needs no key baked into the code.
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
    require!(
        data.len() >= 45,
        ErrorCode::UnauthorizedInitializer
    );
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
pub mod transfer_hook {
    use super::*;

    /// Initializes the hook program configuration.
    /// Creates a PDA to store the Merkle root, the authority and the halt flag.
    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        // Only the upgrade authority may set up the whitelist: otherwise the
        // first caller would become the authority for every mint using this
        // hook.
        require_upgrade_authority(&ctx.accounts.program_data, &ctx.accounts.authority.key())?;

        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        config.merkle_root = ZERO_ROOT; // No whitelist until `update_root`
        config.paused = false;
        config.bump = ctx.bumps.config;

        msg!("Transfer Hook initialized. Authority: {}", config.authority);
        Ok(())
    }

    /// Updates the Merkle root of the whitelist.
    /// Only the authority (multisig) can call this.
    ///
    /// A zero root is rejected (`InvalidRoot`): `execute` treats it as "no
    /// whitelist yet", so accepting it would silently halt every transfer.
    pub fn update_root(ctx: Context<UpdateRoot>, new_root: [u8; 32]) -> Result<()> {
        let config = &mut ctx.accounts.config;

        require_keys_eq!(
            config.authority,
            ctx.accounts.authority.key(),
            ErrorCode::Unauthorized
        );

        require!(new_root != ZERO_ROOT, ErrorCode::InvalidRoot);

        config.merkle_root = new_root;

        msg!("Merkle root updated to: {:?}", new_root);
        Ok(())
    }

    /// Halts (`true`) or resumes (`false`) every transfer.
    /// Only the authority (multisig) can call this.
    ///
    /// Because a zero root is not allowed, this is the supported way to take
    /// the whitelist out of service - clear the tree off-chain, pause, publish
    /// the new root, then resume - without ever leaving an ambiguous root.
    pub fn set_paused(ctx: Context<SetPaused>, paused: bool) -> Result<()> {
        let config = &mut ctx.accounts.config;

        require_keys_eq!(
            config.authority,
            ctx.accounts.authority.key(),
            ErrorCode::Unauthorized
        );

        config.paused = paused;

        msg!("Transfers paused: {}", paused);
        Ok(())
    }

    /// Rotates the whitelist authority, for when the institutional multisig
    /// (or its vault) changes. Only the current authority can call this.
    ///
    /// The new authority is taken as a plain key rather than a signer so that
    /// it can be a multisig vault PDA, which cannot sign a transaction
    /// directly - check the address off-chain before sending.
    pub fn set_authority(ctx: Context<SetAuthority>, new_authority: Pubkey) -> Result<()> {
        let config = &mut ctx.accounts.config;

        require_keys_eq!(
            config.authority,
            ctx.accounts.authority.key(),
            ErrorCode::Unauthorized
        );

        require_keys_neq!(
            new_authority,
            Pubkey::default(),
            ErrorCode::InvalidAuthority
        );

        config.authority = new_authority;

        msg!("Whitelist authority updated to: {}", new_authority);
        Ok(())
    }

    /// Initializes the ExtraAccountMetaList PDA.
    /// This tells Token-2022 to automatically resolve and pass the Config PDA during `execute`.
    ///
    /// The mint must already have a TransferHook extension pointing at this
    /// program (`MintNotConfigured` otherwise), which makes a wrong program id
    /// or a not-yet-configured mint fail here instead of on the first transfer.
    pub fn initialize_extra_account_meta_list(
        ctx: Context<InitializeExtraAccountMetaList>,
    ) -> Result<()> {
        // The mint has to actually point at this program, or Token-2022 will
        // never call this hook and every transfer fails somewhere far less
        // obvious. `rwa_mint::create_rwa_mint` sets this extension; the check
        // also catches a list created before the mint is configured.
        let hook =
            get_mint_extension_data::<TransferHookExtension>(&ctx.accounts.mint.to_account_info())
                .map_err(|_| error!(ErrorCode::MintNotConfigured))?;
        require!(
            Option::<Pubkey>::from(hook.program_id) == Some(*ctx.program_id),
            ErrorCode::MintNotConfigured
        );

        // Define the extra accounts needed during execute. The `Config` PDA is
        // read-only in `execute`, so it is resolved read-only here too.
        let account_metas = vec![ExtraAccountMeta::new_with_seeds(
            &[Seed::Literal {
                bytes: b"config".to_vec(),
            }],
            false, // is_signer
            false, // is_writable
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
    ///
    /// Token-2022 dispatches this handler with the SPL interface's own 8-byte
    /// discriminator (`spl-transfer-hook-interface:execute`), not with the
    /// `global:execute` sighash Anchor derives by default, so the discriminator
    /// is overridden below. Anchor 1.x dropped the `#[interface(...)]`
    /// attribute that used to do this; `#[instruction(discriminator = ...)]` on
    /// the handler is its replacement.
    ///
    /// The data after the discriminator is the transfer `amount` (u64, little
    /// endian) followed by the caller's hint bytes. Those hints are deserialized
    /// as two Merkle proofs below - `Vec<[u8; 32]>` for the sender and another
    /// for the recipient, each Borsh encoded (a 4-byte little-endian length and
    /// then the hashes) - because **both** sides of a transfer must be on the
    /// whitelist, not just the recipient.
    #[instruction(discriminator = ExecuteInstruction::SPL_DISCRIMINATOR_SLICE)]
    pub fn execute<'info>(
        ctx: Context<Execute<'info>>,
        _amount: u64,
        source_proof: Vec<[u8; 32]>,
        destination_proof: Vec<[u8; 32]>,
    ) -> Result<()> {
        let config = &ctx.accounts.config;

        // 1. Check the whitelist is live
        require!(!config.paused, ErrorCode::WhitelistPaused);
        require!(
            config.merkle_root != ZERO_ROOT,
            ErrorCode::WhitelistNotInitialized
        );

        // 2. Take both owners from the token accounts Token-2022 handed us,
        //    never from a caller-supplied key. (The `Execute` context also
        //    constrains `source_token.owner == owner`, so the source owner is
        //    the one the token program authenticated.)
        let source_owner_key = ctx.accounts.source_token.owner;
        let destination_owner_key = ctx.accounts.destination_token.owner;

        // 3. Both sides must prove membership in the same whitelist. Checking
        //    only the recipient would let a delisted holder keep selling out
        //    - the module contract is "whitelisted institutions can receive
        //    *or send* the token".
        require!(
            root_from_proof(&source_owner_key, &source_proof) == config.merkle_root,
            ErrorCode::SourceNotWhitelisted
        );
        require!(
            root_from_proof(&destination_owner_key, &destination_proof) == config.merkle_root,
            ErrorCode::DestinationNotWhitelisted
        );

        msg!(
            "Transfer approved. Source: {}, Destination: {}",
            source_owner_key,
            destination_owner_key
        );
        Ok(())
    }
}

/// Hash `owner` into a domain-separated leaf and fold `proof` up to the root it
/// proves: `keccak(0x00 || owner)` for the leaf, then `keccak(0x01 || l || r)`
/// over each sorted pair, exactly the tree the client builder must produce.
fn root_from_proof(owner: &Pubkey, proof: &[[u8; 32]]) -> [u8; 32] {
    let mut current_hash = hashv(&[&LEAF_PREFIX, owner.as_ref()]).to_bytes();
    for p in proof.iter() {
        let mut hashes = [current_hash, *p];
        hashes.sort_unstable();
        current_hash = hashv(&[&NODE_PREFIX, hashes[0].as_ref(), hashes[1].as_ref()]).to_bytes();
    }
    current_hash
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES
// -----------------------------------------------------------------------

#[account]
pub struct Config {
    pub authority: Pubkey,
    pub merkle_root: [u8; 32],
    pub bump: u8,
    /// While true, `execute` refuses every transfer. Set with `set_paused`.
    pub paused: bool,
}

impl Config {
    // discriminator + authority + root + bump + paused
    pub const LEN: usize = 8 + 32 + 32 + 1 + 1;
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

    /// This program's `ProgramData` account, which carries the upgrade
    /// authority that is allowed to initialize.
    /// CHECK: address and owner are pinned to this program's `ProgramData` PDA
    /// under the upgradeable loader, and the signer is matched against the
    /// authority it records, by `require_upgrade_authority`.
    pub program_data: UncheckedAccount<'info>,

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

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct SetPaused<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump = config.bump
    )]
    pub config: Account<'info, Config>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct SetAuthority<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump = config.bump
    )]
    pub config: Account<'info, Config>,

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
    /// CHECK: ExtraAccountMetaList account, PDA-verified against the same seeds
    /// Token-2022 derives when it resolves the hook's extra accounts.
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
    #[msg("The source wallet is not on the institutional whitelist.")]
    SourceNotWhitelisted,
    #[msg("The whitelist root has not been initialized.")]
    WhitelistNotInitialized,
    #[msg("Only the designated authority can update the whitelist.")]
    Unauthorized,
    #[msg("Transfers are paused by the whitelist authority.")]
    WhitelistPaused,
    #[msg("The Merkle root cannot be all zeros; use `set_paused` to halt transfers.")]
    InvalidRoot,
    #[msg("The new authority cannot be the default pubkey.")]
    InvalidAuthority,
    #[msg("This mint's transfer hook extension does not point at this program.")]
    MintNotConfigured,
    #[msg("Only the program's upgrade authority may initialize the whitelist.")]
    UnauthorizedInitializer,
}

// -----------------------------------------------------------------------
// TESTS
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::InstructionData;
    use anchor_spl::token_2022::spl_token_2022::extension::ExtensionType;
    use solana_keccak_hasher::hash;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// `AccountInfo` over caller-owned storage.
    #[allow(deprecated, reason = "`_unused` still has to be set to match the ABI")]
    fn account_info<'a>(
        key: &'a Pubkey,
        owner: &'a Pubkey,
        data: &'a mut [u8],
        lamports: &'a mut u64,
        is_writable: bool,
    ) -> AccountInfo<'a> {
        AccountInfo {
            key,
            lamports: Rc::new(RefCell::new(lamports)),
            data: Rc::new(RefCell::new(data)),
            owner,
            _unused: 0,
            is_signer: false,
            is_writable,
            executable: false,
        }
    }

    /// Same as `account_info`, but signing.
    fn signer_account_info<'a>(
        key: &'a Pubkey,
        owner: &'a Pubkey,
        data: &'a mut [u8],
        lamports: &'a mut u64,
        is_writable: bool,
    ) -> AccountInfo<'a> {
        let mut info = account_info(key, owner, data, lamports, is_writable);
        info.is_signer = true;
        info
    }

    /// An executable program account, e.g. the system program in a CPI context.
    fn program_account_info<'a>(key: &'a Pubkey, lamports: &'a mut u64) -> AccountInfo<'a> {
        let mut info = account_info(key, key, &mut [], lamports, false);
        info.executable = true;
        info
    }

    /// Packed base Token-2022 mint (82 bytes, no extensions).
    /// Layout: mint_authority (36) | supply (8) | decimals (1) | is_initialized (1) | freeze_authority (36)
    fn mint_data(decimals: u8) -> Vec<u8> {
        let mut data = vec![0u8; 82];
        data[44] = decimals;
        data[45] = 1; // is_initialized
        data
    }

    /// The same mint, with a `TransferHook` extension pointing at `hook`.
    ///
    /// An extended mint is first padded out to the token-account length (165,
    /// which is what stops it being mistaken for one), then the account-type
    /// byte, then the extension: a 4-byte TLV header (2-byte type, 2-byte
    /// length) and a 64-byte payload of `authority || program_id`, both 32
    /// bytes and zeroed while unset.
    fn mint_data_with_hook(hook: &Pubkey) -> Vec<u8> {
        let mut data = mint_data(6);
        data.resize(165, 0);
        data.push(1); // AccountType::Mint

        let mut payload = [0u8; 64];
        payload[32..].copy_from_slice(hook.as_ref());

        data.extend_from_slice(&u16::from(ExtensionType::TransferHook).to_le_bytes());
        data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        data.extend_from_slice(&payload);
        data
    }

    /// Packed Token-2022 token account (165 bytes, no extensions).
    /// Layout: mint (32) | owner (32) | amount (8) | delegate (36) | state (1) |
    /// is_native (12) | delegated_amount (8) | close_authority (36)
    fn token_account_data(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data[..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(owner.as_ref());
        data[108] = 1; // AccountState::Initialized
        data
    }

    /// The code a host-side call comes back with. The entrypoint converts
    /// Anchor errors into `ProgramError::Custom(code)`, so compare against that.
    fn error_code(err: anchor_lang::error::Error) -> u32 {
        match ProgramError::from(err) {
            ProgramError::Custom(code) => code,
            other => panic!("expected a custom program error, got {other:?}"),
        }
    }

    /// Calls the program's entrypoint the way Token-2022 calls the hook: the
    /// SPL discriminator, the amount, then the hint bytes.
    fn call_hook(
        discriminator: &[u8],
        root: [u8; 32],
        source_owner: &Pubkey,
        source_proof: &[[u8; 32]],
        destination_owner: &Pubkey,
        destination_proof: &[[u8; 32]],
        paused: bool,
    ) -> Result<()> {
        let program_id = crate::ID;
        let token_program = anchor_spl::token_2022::ID;

        let mint_key = Pubkey::new_unique();
        let source_key = Pubkey::new_unique();
        let destination_key = Pubkey::new_unique();

        // The two PDAs Token-2022 resolves through the ExtraAccountMetaList.
        let (config_key, config_bump) = Pubkey::find_program_address(&[b"config"], &program_id);
        let (metas_key, _) =
            Pubkey::find_program_address(&[b"extra-account-metas", mint_key.as_ref()], &program_id);

        let mut config_data = Config::DISCRIMINATOR.to_vec();
        Config {
            authority: Pubkey::new_unique(),
            merkle_root: root,
            bump: config_bump,
            paused,
        }
        .serialize(&mut config_data)
        .unwrap();

        let mut metas_data = vec![0u8; ExtraAccountMetaList::size_of(1).unwrap()];
        let mut mint_data = mint_data(6);
        let mut source_data = token_account_data(&mint_key, &source_owner);
        let mut destination_data = token_account_data(&mint_key, destination_owner);

        let (mut l0, mut l1, mut l2, mut l3, mut l4, mut l5) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);

        // Token-2022's hook account order, then the ExtraAccountMetas it
        // resolves from the list: source, mint, destination, owner, list, config.
        let accounts = vec![
            account_info(&source_key, &token_program, &mut source_data, &mut l0, true),
            account_info(&mint_key, &token_program, &mut mint_data, &mut l1, false),
            account_info(
                &destination_key,
                &token_program,
                &mut destination_data,
                &mut l2,
                true,
            ),
            account_info(source_owner, &token_program, &mut [], &mut l3, false),
            account_info(&metas_key, &program_id, &mut metas_data, &mut l4, false),
            account_info(&config_key, &program_id, &mut config_data, &mut l5, false),
        ];

        // amount (u64 LE) followed by the hint bytes, and the hints are the
        // Borsh encoding of `Vec<[u8; 32]>` - a 4-byte LE length then the
        // hashes, exactly what `rwa_mint::transfer` appends.
        let mut data = Vec::new();
        data.extend_from_slice(discriminator);
        data.extend_from_slice(&1_000u64.to_le_bytes());
        data.extend_from_slice(&(source_proof.len() as u32).to_le_bytes());
        for hash in source_proof {
            data.extend_from_slice(hash);
        }
        data.extend_from_slice(&(destination_proof.len() as u32).to_le_bytes());
        for hash in destination_proof {
            data.extend_from_slice(hash);
        }

        crate::entry(&program_id, &accounts, &data).map_err(Into::into)
    }

    /// `keccak(0x00 || pubkey)`, the leaf the hook hashes the destination into.
    fn leaf_hash(owner: &Pubkey) -> [u8; 32] {
        hashv(&[&LEAF_PREFIX, owner.as_ref()]).to_bytes()
    }

    /// `keccak(0x01 || left || right)` over the sorted pair.
    fn parent_hash(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
        let mut pair = [a, b];
        pair.sort_unstable();
        hashv(&[&NODE_PREFIX, pair[0].as_ref(), pair[1].as_ref()]).to_bytes()
    }

    /// A two-leaf whitelist tree over `a` and `b`: the root and each leaf's
    /// proof, so both sides of a transfer can be proven.
    fn pair_proofs(a: &Pubkey, b: &Pubkey) -> ([u8; 32], [u8; 32], [u8; 32]) {
        let la = leaf_hash(a);
        let lb = leaf_hash(b);
        (parent_hash(la, lb), lb, la) // (root, proof_for_a, proof_for_b)
    }

    /// `sha256("global:execute")[..8]`, the discriminator Anchor derives for a
    /// handler when it is not overridden.
    const ANCHOR_EXECUTE_DISCRIMINATOR: [u8; 8] = [0x82, 0xdd, 0xf2, 0x9a, 0x0d, 0xc1, 0xbd, 0x1d];

    /// `sha256("spl-transfer-hook-interface:execute")[..8]`, the tag Token-2022
    /// sends.
    const SPL_EXECUTE_DISCRIMINATOR: [u8; 8] = [0x69, 0x25, 0x65, 0xc5, 0x4b, 0xfb, 0x66, 0x1a];

    #[test]
    fn execute_uses_the_spl_interface_discriminator() {
        let wired = <crate::instruction::Execute as anchor_lang::Discriminator>::DISCRIMINATOR;

        // Both the interface's own constant and the hash it comes from.
        assert_eq!(wired, ExecuteInstruction::SPL_DISCRIMINATOR_SLICE);
        assert_eq!(wired, SPL_EXECUTE_DISCRIMINATOR);
        assert_ne!(wired, ANCHOR_EXECUTE_DISCRIMINATOR);
    }

    /// The SPL discriminator reaches the handler: a valid proof is approved.
    #[test]
    fn spl_dispatch_approves_a_whitelisted_transfer() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        // Both sides live in the same two-leaf tree.
        let (root, source_proof, destination_proof) = pair_proofs(&source, &destination);

        assert!(call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &source,
            &[source_proof],
            &destination,
            &[destination_proof],
            false,
        )
        .is_ok());
    }

    #[test]
    fn spl_dispatch_rejects_an_unlisted_destination() {
        let source = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        // The sender is whitelisted; the recipient is not in the tree at all.
        let (root, source_proof, _) = pair_proofs(&source, &other);

        let err = call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &source,
            &[source_proof],
            &destination,
            &[hash(b"not in this tree").to_bytes()],
            false,
        )
        .unwrap_err();

        assert_eq!(
            error_code(err),
            u32::from(ErrorCode::DestinationNotWhitelisted)
        );
    }

    /// The sender is checked too: a delisted holder cannot sell out to a
    /// whitelisted buyer.
    #[test]
    fn spl_dispatch_rejects_an_unlisted_source() {
        let source = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        // The recipient is whitelisted; the sender is not.
        let (root, _, destination_proof) = pair_proofs(&other, &destination);

        let err = call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &source,
            &[hash(b"not in this tree").to_bytes()],
            &destination,
            &[destination_proof],
            false,
        )
        .unwrap_err();

        assert_eq!(error_code(err), u32::from(ErrorCode::SourceNotWhitelisted));
    }

    #[test]
    fn spl_dispatch_rejects_an_uninitialized_whitelist() {
        let destination = Pubkey::new_unique();

        let err = call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            ZERO_ROOT,
            &Pubkey::new_unique(),
            &[],
            &destination,
            &[],
            false,
        )
        .unwrap_err();

        assert_eq!(
            error_code(err),
            u32::from(ErrorCode::WhitelistNotInitialized)
        );
    }

    /// The old Anchor `global:execute` sighash no longer reaches the handler.
    #[test]
    fn anchor_discriminator_is_not_accepted() {
        let destination = Pubkey::new_unique();

        let err = call_hook(
            &ANCHOR_EXECUTE_DISCRIMINATOR,
            [1u8; 32],
            &Pubkey::new_unique(),
            &[],
            &destination,
            &[],
            false,
        )
        .unwrap_err();

        assert_eq!(
            error_code(err),
            anchor_lang::error::ErrorCode::InstructionFallbackNotFound as u32
        );
    }

    /// A proof built the old way - `keccak(pubkey)` leaves and `keccak(l || r)`
    /// parents, no domain prefixes - must no longer verify.
    #[test]
    fn un_prefixed_proofs_do_not_verify() {
        let destination = Pubkey::new_unique();
        let sibling = hash(b"another whitelisted institution").to_bytes();

        let mut pair = [hash(destination.as_ref()).to_bytes(), sibling];
        pair.sort_unstable();
        let legacy_root = hashv(&[pair[0].as_ref(), pair[1].as_ref()]).to_bytes();

        // The sender is checked first, and no prefixed leaf folds up to the
        // legacy (unprefixed) root, so the transfer is refused before the
        // recipient is even considered.
        let err = call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            legacy_root,
            &destination,
            &[sibling],
            &destination,
            &[sibling],
            false,
        )
        .unwrap_err();

        assert_eq!(error_code(err), u32::from(ErrorCode::SourceNotWhitelisted));
    }

    /// A level with an odd number of nodes hashes the last one with itself, and
    /// the proof repeats that same hash - the rule the README's client tree
    /// builder implements.
    #[test]
    fn a_duplicated_level_verifies() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let c = Pubkey::new_unique();

        let ab = parent_hash(leaf_hash(&a), leaf_hash(&b));
        let cc = parent_hash(leaf_hash(&c), leaf_hash(&c));
        let root = parent_hash(ab, cc);

        // A transfer A -> B: A proves with B's leaf then the CC hash, and B
        // proves with A's leaf then the CC hash.
        assert!(call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &a,
            &[leaf_hash(&b), cc],
            &b,
            &[leaf_hash(&a), cc],
            false,
        )
        .is_ok());

        // A transfer C -> C exercises the duplicated last node on both sides.
        assert!(call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &c,
            &[leaf_hash(&c), ab],
            &c,
            &[leaf_hash(&c), ab],
            false,
        )
        .is_ok());
    }

    #[test]
    fn paused_whitelist_rejects_transfers() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let (root, source_proof, destination_proof) = pair_proofs(&source, &destination);

        let err = call_hook(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &source,
            &[source_proof],
            &destination,
            &[destination_proof],
            true,
        )
        .unwrap_err();

        assert_eq!(error_code(err), u32::from(ErrorCode::WhitelistPaused));
    }

    /// Builds the `Config` PDA and calls an authority-only instruction through
    /// the entrypoint, returning the result and the stored config.
    fn call_admin(
        data: Vec<u8>,
        authority: Pubkey,
        caller_is_authority: bool,
    ) -> (Result<()>, Config) {
        let program_id = crate::ID;
        let (config_key, config_bump) = Pubkey::find_program_address(&[b"config"], &program_id);

        let mut config_data = Config::DISCRIMINATOR.to_vec();
        Config {
            authority,
            merkle_root: [3u8; 32],
            bump: config_bump,
            paused: false,
        }
        .serialize(&mut config_data)
        .unwrap();

        let caller = if caller_is_authority {
            authority
        } else {
            Pubkey::new_unique()
        };

        let (mut l0, mut l1) = (0u64, 0u64);
        let accounts = vec![
            account_info(&config_key, &program_id, &mut config_data, &mut l0, true),
            signer_account_info(&caller, &program_id, &mut [], &mut l1, false),
        ];

        let result = crate::entry(&program_id, &accounts, &data).map_err(Into::into);

        let stored = {
            let bytes = accounts[0].data.borrow().to_vec();
            let mut slice = bytes.as_slice();
            Config::try_deserialize(&mut slice).unwrap()
        };

        (result, stored)
    }

    #[test]
    fn update_root_requires_the_authority() {
        let authority = Pubkey::new_unique();
        let (result, config) = call_admin(
            crate::instruction::UpdateRoot {
                new_root: [7u8; 32],
            }
            .data(),
            authority,
            false,
        );

        assert_eq!(
            error_code(result.unwrap_err()),
            u32::from(ErrorCode::Unauthorized)
        );
        assert_eq!(config.merkle_root, [3u8; 32]); // untouched
    }

    #[test]
    fn update_root_stores_a_non_zero_root() {
        let authority = Pubkey::new_unique();
        let new_root = [9u8; 32];

        let (result, config) = call_admin(
            crate::instruction::UpdateRoot { new_root }.data(),
            authority,
            true,
        );

        assert!(result.is_ok());
        assert_eq!(config.merkle_root, new_root);
    }

    #[test]
    fn update_root_rejects_a_zero_root() {
        let authority = Pubkey::new_unique();

        let (result, config) = call_admin(
            crate::instruction::UpdateRoot {
                new_root: ZERO_ROOT,
            }
            .data(),
            authority,
            true,
        );

        assert_eq!(
            error_code(result.unwrap_err()),
            u32::from(ErrorCode::InvalidRoot)
        );
        assert_eq!(config.merkle_root, [3u8; 32]); // untouched
    }

    #[test]
    fn set_paused_flips_the_flag() {
        let authority = Pubkey::new_unique();

        let (result, config) = call_admin(
            crate::instruction::SetPaused { paused: true }.data(),
            authority,
            true,
        );
        assert!(result.is_ok());
        assert!(config.paused);

        let (result, config) = call_admin(
            crate::instruction::SetPaused { paused: false }.data(),
            authority,
            true,
        );
        assert!(result.is_ok());
        assert!(!config.paused);

        let (result, _) = call_admin(
            crate::instruction::SetPaused { paused: true }.data(),
            authority,
            false,
        );
        assert_eq!(
            error_code(result.unwrap_err()),
            u32::from(ErrorCode::Unauthorized)
        );
    }

    #[test]
    fn set_authority_rotates_and_guards() {
        let authority = Pubkey::new_unique();
        let new_authority = Pubkey::new_unique();

        // A stranger cannot rotate it.
        let (result, config) = call_admin(
            crate::instruction::SetAuthority { new_authority }.data(),
            authority,
            false,
        );
        assert_eq!(
            error_code(result.unwrap_err()),
            u32::from(ErrorCode::Unauthorized)
        );
        assert_eq!(config.authority, authority);

        // The default pubkey is refused: it would freeze the whitelist.
        let (result, config) = call_admin(
            crate::instruction::SetAuthority {
                new_authority: Pubkey::default(),
            }
            .data(),
            authority,
            true,
        );
        assert_eq!(
            error_code(result.unwrap_err()),
            u32::from(ErrorCode::InvalidAuthority)
        );
        assert_eq!(config.authority, authority);

        // The authority rotates.
        let (result, config) = call_admin(
            crate::instruction::SetAuthority { new_authority }.data(),
            authority,
            true,
        );
        assert!(result.is_ok());
        assert_eq!(config.authority, new_authority);
    }

    /// Calls `initialize_extra_account_meta_list` with a hand-built mint.
    ///
    /// Only the guard is exercised: the happy path continues into a system
    /// program CPI, which cannot run outside an SVM.
    fn call_init_extra_account_meta_list(mint_bytes: Vec<u8>) -> Result<()> {
        let program_id = crate::ID;
        let mint_key = Pubkey::new_unique();
        let payer_key = Pubkey::new_unique();
        let system_id = anchor_lang::solana_program::system_program::ID;
        let (metas_key, _) =
            Pubkey::find_program_address(&[b"extra-account-metas", mint_key.as_ref()], &program_id);

        let mut mint_bytes = mint_bytes;
        let mut metas_data = vec![0u8; ExtraAccountMetaList::size_of(1).unwrap()];
        let (mut l0, mut l1, mut l2, mut l3) = (0u64, 0u64, 0u64, 0u64);

        let accounts = vec![
            signer_account_info(&payer_key, &system_id, &mut [], &mut l0, true),
            account_info(&metas_key, &program_id, &mut metas_data, &mut l1, true),
            account_info(
                &mint_key,
                &anchor_spl::token_2022::ID,
                &mut mint_bytes,
                &mut l2,
                false,
            ),
            program_account_info(&system_id, &mut l3),
        ];

        let data = crate::instruction::InitializeExtraAccountMetaList {}.data();

        crate::entry(&program_id, &accounts, &data).map_err(Into::into)
    }

    #[test]
    fn extra_account_meta_list_requires_the_mint_to_point_here() {
        // A mint with no TransferHook extension at all.
        let err = call_init_extra_account_meta_list(mint_data(6)).unwrap_err();
        assert_eq!(error_code(err), u32::from(ErrorCode::MintNotConfigured));

        // A mint whose hook is some other program.
        let err = call_init_extra_account_meta_list(mint_data_with_hook(&Pubkey::new_unique()))
            .unwrap_err();
        assert_eq!(error_code(err), u32::from(ErrorCode::MintNotConfigured));
    }
}
