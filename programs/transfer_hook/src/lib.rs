//! # RWA Transfer Hook Program
//!
//! This program implements the SPL Transfer Hook Interface for Token-2022.
//! It enforces a Merkle-tree whitelist on every transfer of RWA tokens.
//! Only institutions on the whitelist can receive or send the token.
//!
//! ## How the proofs reach `execute`
//!
//! Token-2022 builds the hook's instruction data itself
//! (`spl_transfer_hook_interface::onchain::invoke_execute` writes exactly
//! `[discriminator][amount u64]`) and forwards **no** trailing bytes from the
//! transfer. A proof therefore cannot ride along in the instruction data: bytes
//! appended to `transfer_checked` make the token program reject the transfer
//! (`InvalidInstructionData`, it requires an exact data length) before the hook
//! is ever called.
//!
//! The proofs travel as **accounts** instead. Each whitelisted institution has
//! a `WhitelistEntry` PDA at `[b"whitelist", owner]` holding its Merkle path,
//! and the `ExtraAccountMetaList` tells Token-2022 to derive - for every
//! transfer - the two entries that belong to the sending and the receiving
//! token account's `owner` field. Token-2022 then appends the caller-supplied
//! accounts at those addresses to the hook's account list, and `execute` folds
//! the stored paths against the published root. See
//! `initialize_extra_account_meta_list` and `set_whitelist_entry`.

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

/// How many sibling hashes a `WhitelistEntry` can hold. 20 levels covers a
/// million institutions in one tree (and costs 640 of the entry's 681 bytes); a
/// deeper tree has to be sharded - per jurisdiction, say - rather than stored
/// here.
const MAX_PROOF_DEPTH: usize = 20;

/// Requires `signer` to be this program's **upgrade authority**, read from the
/// program's own `ProgramData` account.
///
/// `initialize` is the one instruction that hands out the whitelist authority,
/// so it must not be claimable by whoever gets there first. The upgrade
/// authority is the key that can already change the program, which makes it the
/// natural owner of that one-time setup and needs no key baked into the code.
///
/// The loader-layout parser itself lives in `program_governance` (L-7): the
/// three programs with a one-time initializer — this one, `kyc_gate` and
/// `settlement` — used to carry byte-identical copies, and a loader change
/// would have had to be found and fixed in three places.
fn require_upgrade_authority(program_data: &UncheckedAccount, signer: &Pubkey) -> Result<()> {
    program_governance::require_upgrade_authority(&crate::ID, program_data, signer)
        .map_err(|_| error!(ErrorCode::UnauthorizedInitializer))
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

        // Define the extra accounts `execute` needs. Token-2022 resolves each
        // of these itself from the list and appends the account the caller
        // supplied at the derived address, which is how the two Merkle proofs
        // are delivered:
        //
        //   * the sender's and the recipient's `WhitelistEntry` PDAs, derived
        //     from the `owner` field of the two token accounts in the transfer
        //     (`Seed::AccountData`, offset 32 - the base layout every SPL and
        //     Token-2022 token account shares), and
        //   * the `Config` PDA, which carries the pause flag and the root.
        //
        // `account_index` refers to the transfer instruction's own account
        // list, where 0 is the source token account and 2 the destination.
        // All three are read-only in `execute`, so they are resolved
        // read-only here too.
        let account_metas = hook_extra_account_metas()?;

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

        msg!("ExtraAccountMetaList initialized with whitelist-entry and config resolution");
        Ok(())
    }

    /// Creates or refreshes the whitelist entry at `[b"whitelist", owner]`.
    ///
    /// The authority vouches for a wallet that cannot sign for itself (an
    /// institution's vault PDA, a custodian-held account). The proof is checked
    /// against the live root **before** it is stored (`ProofNotInWhitelist`),
    /// so a mistyped or already-rotated path can never become an entry.
    ///
    /// **Rotation.** Publishing a new root leaves every entry where it is, and
    /// every entry whose proof no longer folds to the new root stops verifying -
    /// the root stays the single source of truth. A rotation is therefore
    /// `update_root` followed by one `set_whitelist_entry` per member, which is
    /// why this upserts (`init_if_needed`) instead of creating an account that
    /// would have to be closed and re-made to change the path.
    pub fn set_whitelist_entry(
        ctx: Context<SetWhitelistEntry>,
        proof: Vec<[u8; 32]>,
    ) -> Result<()> {
        let owner_key = ctx.accounts.owner.key();

        require_acceptable_entry(
            &ctx.accounts.config,
            &ctx.accounts.authority.key(),
            &owner_key,
            &proof,
        )?;

        // `init_if_needed` leaves a freshly created account zeroed, and Anchor
        // writes a zero-copy account's discriminator when the instruction
        // *exits* - so on the create path this has to be written here for
        // `load_mut` (which checks it) to be usable on both paths. On the refresh
        // path the bytes already match and nothing changes.
        {
            let mut data = ctx.accounts.whitelist_entry.as_ref().try_borrow_mut_data()?;
            require!(
                data.len() >= WhitelistEntry::DISCRIMINATOR.len(),
                ErrorCode::InvalidWhitelistEntry
            );
            data[..WhitelistEntry::DISCRIMINATOR.len()]
                .copy_from_slice(WhitelistEntry::DISCRIMINATOR);
        }

        let mut entry = ctx.accounts.whitelist_entry.load_mut()?;
        store_entry(&mut entry, &owner_key, &proof);

        msg!(
            "Whitelist entry set for {} ({} proof node(s))",
            owner_key,
            proof.len()
        );
        Ok(())
    }

    /// Closes a delisted institution's entry and refunds its rent to the
    /// authority. `close = authority` in the context does the accounting; the
    /// handler only has to prove the caller may do it.
    pub fn close_whitelist_entry(ctx: Context<CloseWhitelistEntry>) -> Result<()> {
        require_keys_eq!(
            ctx.accounts.config.authority,
            ctx.accounts.authority.key(),
            ErrorCode::Unauthorized
        );

        msg!(
            "Whitelist entry closed for {}",
            ctx.accounts.whitelist_entry.load()?.owner
        );
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
    /// The instruction data is the transfer `amount` (u64, little endian) and
    /// nothing else, because that is all Token-2022 writes. The two Merkle
    /// proofs arrive as the `source_entry` / `destination_entry` accounts, which
    /// the `ExtraAccountMetaList` derives from each token account's owner. Both
    /// sides are checked, not just the recipient.
    #[instruction(discriminator = ExecuteInstruction::SPL_DISCRIMINATOR_SLICE)]
    pub fn execute<'info>(ctx: Context<Execute<'info>>, _amount: u64) -> Result<()> {
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

        // 3. Each entry must belong to the wallet it is proving for. Token-2022
        //    derives the entry address from the token account's own `owner`
        //    field, but `execute` is a public instruction too, and a direct call
        //    can supply any two entries it likes. The accounts are still
        //    unforgeable - only this program can create one, and only the
        //    authority can ask it to - so re-checking the binding here is what
        //    makes the resolution path untrusted-but-safe.
        let source_entry = ctx.accounts.source_entry.load()?;
        let destination_entry = ctx.accounts.destination_entry.load()?;

        require_keys_eq!(
            source_entry.owner,
            source_owner_key,
            ErrorCode::InvalidWhitelistEntry
        );
        require_keys_eq!(
            destination_entry.owner,
            destination_owner_key,
            ErrorCode::InvalidWhitelistEntry
        );

        // 4. Both sides must fold to the same published root. Folding - not the
        //    existence of the account - is what enforces the whitelist: an entry
        //    whose path is stale after a root rotation stops verifying, so a
        //    delisted institution fails closed. Checking only the recipient
        //    would let a delisted holder keep selling out - the module contract
        //    is "whitelisted institutions can receive *or send* the token".
        require!(
            root_from_entry(&source_owner_key, &source_entry) == config.merkle_root,
            ErrorCode::SourceNotWhitelisted
        );
        require!(
            root_from_entry(&destination_owner_key, &destination_entry) == config.merkle_root,
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

/// The same fold over a stored entry, using only the `depth` slots that hold a
/// path. `depth` is clamped rather than trusted: it is written by
/// `set_whitelist_entry` under a length check, and a stored value that somehow
/// exceeded the array would otherwise panic the hook instead of refusing the
/// transfer.
fn root_from_entry(owner: &Pubkey, entry: &WhitelistEntry) -> [u8; 32] {
    let depth = usize::from(entry.depth).min(MAX_PROOF_DEPTH);
    root_from_proof(owner, &entry.proof[..depth])
}

/// The write-time gate for `set_whitelist_entry`, kept as a pure function so the
/// policy can be unit-tested without a validator (the instruction's own account
/// plumbing reads the Rent sysvar, which does not exist host-side).
///
/// A path that does not fold to the **live** root is refused rather than stored:
/// entries are only ever as current as the root they were written against, which
/// is what makes a rotation revoke them.
fn require_acceptable_entry(
    config: &Config,
    caller: &Pubkey,
    owner: &Pubkey,
    proof: &[[u8; 32]],
) -> Result<()> {
    require_keys_eq!(config.authority, *caller, ErrorCode::Unauthorized);

    // Without a published root there is nothing to fold against, and `execute`
    // refuses every transfer in that state anyway.
    require!(
        config.merkle_root != ZERO_ROOT,
        ErrorCode::WhitelistNotInitialized
    );
    require!(proof.len() <= MAX_PROOF_DEPTH, ErrorCode::ProofTooLong);
    require!(
        root_from_proof(owner, proof) == config.merkle_root,
        ErrorCode::ProofNotInWhitelist
    );

    Ok(())
}

/// The extra accounts `execute` needs on every transfer, in the order it
/// receives them.
///
/// Token-2022 resolves each of these itself from the mint's
/// `ExtraAccountMetaList` and appends the account the caller supplied at the
/// derived address, which is how the two Merkle proofs are delivered:
///
///   * the sender's and the recipient's `WhitelistEntry` PDAs, derived from the
///     `owner` field of the two token accounts in the transfer
///     (`Seed::AccountData`, offset 32 - the base layout every SPL and Token-2022
///     token account shares), and
///   * the `Config` PDA, which carries the pause flag and the root.
///
/// `account_index` refers to the transfer instruction's own account list, where 0
/// is the source token account and 2 the destination. All three are read-only in
/// `execute`, so they are resolved read-only here too.
///
/// Kept as a function (rather than inline in the instruction) so the resolution
/// itself can be tested host-side, where the instruction's own account creation
/// cannot run: `extra_account_metas_resolve_to_each_side_whitelist_entry` folds
/// these through the same `ExtraAccountMeta::resolve` Token-2022 calls.
fn hook_extra_account_metas() -> Result<Vec<ExtraAccountMeta>> {
    Ok(vec![
        ExtraAccountMeta::new_with_seeds(
            &[
                Seed::Literal {
                    bytes: b"whitelist".to_vec(),
                },
                Seed::AccountData {
                    account_index: 0, // source token account
                    data_index: 32,   // its `owner` field
                    length: 32,
                },
            ],
            false, // is_signer
            false, // is_writable
        )?,
        ExtraAccountMeta::new_with_seeds(
            &[
                Seed::Literal {
                    bytes: b"whitelist".to_vec(),
                },
                Seed::AccountData {
                    account_index: 2, // destination token account
                    data_index: 32,   // its `owner` field
                    length: 32,
                },
            ],
            false, // is_signer
            false, // is_writable
        )?,
        ExtraAccountMeta::new_with_seeds(
            &[Seed::Literal {
                bytes: b"config".to_vec(),
            }],
            false, // is_signer
            false, // is_writable
        )?,
    ])
}

/// Writes `owner`'s path into its entry, clearing the unused tail so a shorter
/// path cannot leave a longer one's bytes behind: `depth` governs the fold, but a
/// reviewer should never have to wonder what is in the slots past it.
fn store_entry(entry: &mut WhitelistEntry, owner: &Pubkey, proof: &[[u8; 32]]) {
    entry.owner = *owner;
    entry.depth = proof.len() as u8;
    entry.proof = [[0u8; 32]; MAX_PROOF_DEPTH];
    entry.proof[..proof.len()].copy_from_slice(proof);
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

/// One institution's membership: the Merkle path from its leaf
/// (`keccak(0x00 || owner)`) up to the root published in `Config`.
///
/// Stored rather than passed, because Token-2022 gives the hook no way to
/// receive instruction data it did not construct itself - and it constructs only
/// the amount. The PDA is `[b"whitelist", owner]`, which is what makes the entry
/// resolvable from a token account's `owner` field, and `owner` is stored so
/// `execute` can prove the entry it was handed is the one for the wallet being
/// checked.
///
/// **Zero-copy.** The struct is 673 bytes, and `execute` holds two of them; a
/// by-value `Account<WhitelistEntry>` deserialization blows the 4 KiB SBF stack
/// frame (the generated `execute` frame measured 4 160 bytes). `AccountLoader`
/// re-interprets the account's bytes in place instead, and `#[repr(C, packed)]`
/// keeps that layout byte-identical to the borsh layout and its alignment at 1,
/// which the in-place mapping requires.
#[account(zero_copy)]
#[repr(C, packed)]
pub struct WhitelistEntry {
    /// The wallet this entry admits. A token account whose `owner` is this key
    /// passes the whitelist (given the proof still folds to the live root).
    pub owner: Pubkey,
    /// How many of `proof`'s slots are meaningful. `0` is a single-member tree
    /// (a bare leaf is the root); the four-byte-per-node budget of an
    /// `ExtraAccountMeta` seed is unaffected, since the depth lives here.
    pub depth: u8,
    /// The sibling hashes, leaf-adjacent first. Slots past `depth` are zeroed.
    pub proof: [[u8; 32]; MAX_PROOF_DEPTH],
}

impl WhitelistEntry {
    // 8 (discriminator) + 32 (owner) + 1 (depth) + 20 * 32 (proof) = 681, the
    // same total the borsh layout would be: every field is byte-aligned, so
    // packing adds no padding.
    pub const LEN: usize = 8 + 32 + 1 + 32 * MAX_PROOF_DEPTH;
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
pub struct SetWhitelistEntry<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut)]
    pub authority: Signer<'info>,

    /// The institution's wallet, which does not have to sign: the authority
    /// vouches for it. Seeds for `whitelist_entry` are taken from its key and its
    /// key is the leaf that gets hashed, so the two can never disagree.
    /// CHECK: never read or written - it is used as a PDA seed and as the leaf
    /// preimage only.
    pub owner: UncheckedAccount<'info>,

    /// The entry to create, or the one to refresh when the root has rotated.
    #[account(
        init_if_needed,
        payer = authority,
        space = WhitelistEntry::LEN,
        seeds = [b"whitelist", owner.key().as_ref()],
        bump
    )]
    pub whitelist_entry: AccountLoader<'info, WhitelistEntry>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CloseWhitelistEntry<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,

    /// Rent for the closed entry is refunded here.
    #[account(mut)]
    pub authority: Signer<'info>,

    /// CHECK: seed source only, exactly as in `SetWhitelistEntry`.
    pub owner: UncheckedAccount<'info>,

    #[account(
        mut,
        close = authority,
        seeds = [b"whitelist", owner.key().as_ref()],
        bump
    )]
    pub whitelist_entry: AccountLoader<'info, WhitelistEntry>,
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

    /// CHECK: **Passed by the token program, deliberately unused.** Token-2022
    /// fixed account order puts the source account's owner here (see the SPL
    /// transfer-hook interface), so it must be accepted at this position — but
    /// this program must never trust it: nothing constrains what a caller can
    /// place in an unchecked slot when the hook is reached through other paths.
    /// The authority decision is read from `source_token.owner` above, which the
    /// token program itself guarantees to be the account's owner. Keep this
    /// account unused; a reviewer's eye is drawn to an unconstrained `owner`.
    pub owner: UncheckedAccount<'info>,

    // 5: Extra accounts resolved from ExtraAccountMetaList in the order defined
    /// CHECK: ExtraAccountMetaList account, PDA-verified against the same seeds
    /// Token-2022 derives when it resolves the hook's extra accounts.
    #[account(
        seeds = [b"extra-account-metas", mint.key().as_ref()],
        bump
    )]
    pub extra_account_meta_list: UncheckedAccount<'info>,

    /// The sender's whitelist entry, resolved by Token-2022 from the
    /// `ExtraAccountMetaList` as `[b"whitelist", source_token.owner]`.
    /// Zero-copy: two deserialized entries would not fit the SBF stack frame.
    pub source_entry: AccountLoader<'info, WhitelistEntry>,

    /// The recipient's whitelist entry, resolved the same way from
    /// `destination_token.owner`.
    pub destination_entry: AccountLoader<'info, WhitelistEntry>,

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
    #[msg("This entry does not belong to the wallet it is proving for.")]
    InvalidWhitelistEntry,
    #[msg("This Merkle path does not match the published whitelist root.")]
    ProofNotInWhitelist,
    #[msg("A whitelist entry holds at most 20 proof nodes.")]
    ProofTooLong,
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

    /// A serialized `WhitelistEntry` for `owner` carrying `proof` - the account
    /// Token-2022 resolves and hands to `execute`. Passing a proof the tree does
    /// not actually produce is how the tests below model a forged or stale
    /// entry; the fold in `execute` is what has to reject it.
    fn entry_data(owner: &Pubkey, proof: &[[u8; 32]]) -> Vec<u8> {
        // Written by hand rather than through `AnchorSerialize`: a zero-copy
        // account is not serialized by value, and the layout is exactly
        // `owner ‖ depth ‖ proof` (the packed repr adds no padding).
        assert!(proof.len() <= MAX_PROOF_DEPTH);

        let mut data = WhitelistEntry::DISCRIMINATOR.to_vec();
        data.extend_from_slice(owner.as_ref());
        data.push(proof.len() as u8);
        for slot in 0..MAX_PROOF_DEPTH {
            data.extend_from_slice(proof.get(slot).unwrap_or(&[0u8; 32]));
        }
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

    /// Calls the program's entrypoint the way Token-2022 calls the hook for a
    /// transfer from `source_owner` to `destination_owner`: the SPL
    /// discriminator, the amount, and the two entries Token-2022 would have
    /// resolved (which are built from the same owners).
    fn call_hook(
        discriminator: &[u8],
        root: [u8; 32],
        source_owner: &Pubkey,
        source_proof: &[[u8; 32]],
        destination_owner: &Pubkey,
        destination_proof: &[[u8; 32]],
        paused: bool,
    ) -> Result<()> {
        call_hook_with_entry_owners(
            discriminator,
            root,
            source_owner,
            source_owner,
            source_proof,
            destination_owner,
            destination_owner,
            destination_proof,
            paused,
        )
    }

    /// The same call, with the entries' stored `owner` fields settable
    /// separately from the token accounts' owners.
    ///
    /// Token-2022 always derives `[b"whitelist", owner]` from the token account,
    /// so the two agree on every hook call it makes; differing values model the
    /// direct call the binding check in `execute` exists for.
    #[allow(clippy::too_many_arguments)]
    fn call_hook_with_entry_owners(
        discriminator: &[u8],
        root: [u8; 32],
        source_token_owner: &Pubkey,
        source_entry_owner: &Pubkey,
        source_proof: &[[u8; 32]],
        destination_token_owner: &Pubkey,
        destination_entry_owner: &Pubkey,
        destination_proof: &[[u8; 32]],
        paused: bool,
    ) -> Result<()> {
        let program_id = crate::ID;
        let token_program = anchor_spl::token_2022::ID;

        let mint_key = Pubkey::new_unique();
        let source_key = Pubkey::new_unique();
        let destination_key = Pubkey::new_unique();

        // The PDAs Token-2022 resolves through the ExtraAccountMetaList: the
        // config, the meta list, and one entry per side.
        let (config_key, config_bump) = Pubkey::find_program_address(&[b"config"], &program_id);
        let (metas_key, _) =
            Pubkey::find_program_address(&[b"extra-account-metas", mint_key.as_ref()], &program_id);
        let (source_entry_key, _) =
            Pubkey::find_program_address(&[b"whitelist", source_token_owner.as_ref()], &program_id);
        let (destination_entry_key, _) = Pubkey::find_program_address(
            &[b"whitelist", destination_token_owner.as_ref()],
            &program_id,
        );

        let mut config_data = Config::DISCRIMINATOR.to_vec();
        Config {
            authority: Pubkey::new_unique(),
            merkle_root: root,
            bump: config_bump,
            paused,
        }
        .serialize(&mut config_data)
        .unwrap();

        let mut metas_data = vec![0u8; ExtraAccountMetaList::size_of(3).unwrap()];
        let mut mint_data = mint_data(6);
        let mut source_data = token_account_data(&mint_key, source_token_owner);
        let mut destination_data = token_account_data(&mint_key, destination_token_owner);

        let (mut l0, mut l1, mut l2, mut l3, mut l4, mut l5, mut l6, mut l7) = (
            0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64,
        );

        // The two proofs now arrive as accounts rather than instruction data:
        // Token-2022 resolves them from the meta list, so the harness has to
        // build the entries Token-2022 would have appended.
        let mut source_entry_data = entry_data(source_entry_owner, source_proof);
        let mut destination_entry_data = entry_data(destination_entry_owner, destination_proof);

        // Token-2022's hook account order, then the ExtraAccountMetas it
        // resolves from the list: source, mint, destination, owner, list, then
        // the sender's and the recipient's entries, then the config.
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
            account_info(source_token_owner, &token_program, &mut [], &mut l3, false),
            account_info(&metas_key, &program_id, &mut metas_data, &mut l4, false),
            account_info(
                &source_entry_key,
                &program_id,
                &mut source_entry_data,
                &mut l5,
                false,
            ),
            account_info(
                &destination_entry_key,
                &program_id,
                &mut destination_entry_data,
                &mut l6,
                false,
            ),
            account_info(&config_key, &program_id, &mut config_data, &mut l7, false),
        ];

        // The transfer amount and nothing else - exactly the data
        // `invoke_execute` builds for the hook.
        let mut data = Vec::new();
        data.extend_from_slice(discriminator);
        data.extend_from_slice(&1_000u64.to_le_bytes());

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

    /// The entry handed to `execute` has to be the one for the wallet being
    /// checked. Token-2022 derives it from the token account, but a direct caller
    /// can put any two entries in the account list, so the binding is checked in
    /// the handler.
    #[test]
    fn entries_are_bound_to_the_wallet_they_prove_for() {
        let source = Pubkey::new_unique();
        let destination = Pubkey::new_unique();
        let (root, source_proof, destination_proof) = pair_proofs(&source, &destination);

        // Both paths are real, but the sender's entry is stored under a key the
        // sender did not sign for.
        let err = call_hook_with_entry_owners(
            ExecuteInstruction::SPL_DISCRIMINATOR_SLICE,
            root,
            &source,
            &Pubkey::new_unique(),
            &[source_proof],
            &destination,
            &destination,
            &[destination_proof],
            false,
        )
        .unwrap_err();

        assert_eq!(
            error_code(err),
            u32::from(ErrorCode::InvalidWhitelistEntry)
        );
    }

    fn config_with(root: [u8; 32], authority: Pubkey) -> Config {
        Config {
            authority,
            merkle_root: root,
            bump: 0,
            paused: false,
        }
    }

    /// `set_whitelist_entry` accepts a path that folds to the live root, and the
    /// entry it writes is exactly what `execute` folds back - so the entry admits
    /// `member` and nobody else.
    #[test]
    fn set_whitelist_entry_stores_a_path_the_root_accepts() {
        let authority = Pubkey::new_unique();
        let member = Pubkey::new_unique();
        let sibling = hash(b"another whitelisted institution").to_bytes();
        // A two-leaf tree containing `member`: its proof is the sibling.
        let root = parent_hash(leaf_hash(&member), sibling);
        let config = config_with(root, authority);

        assert!(require_acceptable_entry(&config, &authority, &member, &[sibling]).is_ok());

        let mut entry = WhitelistEntry {
            owner: Pubkey::new_unique(),
            depth: 9,
            proof: [[1u8; 32]; MAX_PROOF_DEPTH],
        };
        store_entry(&mut entry, &member, &[sibling]);


        assert_eq!(entry.owner, member);
        assert_eq!(entry.depth, 1);
        assert_eq!(entry.proof[0], sibling);
        // The tail past `depth` is cleared, so nothing stale is left behind...
        assert_eq!(entry.proof[1], [0u8; 32]);
        assert_eq!(entry.proof[MAX_PROOF_DEPTH - 1], [0u8; 32]);
        // ...and the stored path is what `execute` folds.
        assert_eq!(root_from_entry(&member, &entry), root);
    }

    #[test]
    fn set_whitelist_entry_rejects_a_path_the_root_does_not_accept() {
        let authority = Pubkey::new_unique();
        let member = Pubkey::new_unique();

        // The published root, but a path that does not fold to it: what a
        // mistyped proof, or one from before a rotation, looks like.
        let config = config_with([3u8; 32], authority);
        let err = require_acceptable_entry(&config, &authority, &member, &[[7u8; 32]]).unwrap_err();
        assert_eq!(error_code(err), u32::from(ErrorCode::ProofNotInWhitelist));

        // A path longer than an entry can hold is refused before it is folded.
        let err = require_acceptable_entry(
            &config,
            &authority,
            &member,
            &vec![[7u8; 32]; MAX_PROOF_DEPTH + 1],
        )
        .unwrap_err();
        assert_eq!(error_code(err), u32::from(ErrorCode::ProofTooLong));

        // Only the authority may write entries.
        let err = require_acceptable_entry(&config, &Pubkey::new_unique(), &member, &[]).unwrap_err();
        assert_eq!(error_code(err), u32::from(ErrorCode::Unauthorized));

        // And nothing can be written before a root exists - there would be
        // nothing for `execute` to fold against.
        let err = require_acceptable_entry(
            &config_with(ZERO_ROOT, authority),
            &authority,
            &member,
            &[],
        )
        .unwrap_err();
        assert_eq!(
            error_code(err),
            u32::from(ErrorCode::WhitelistNotInitialized)
        );
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
        let mut metas_data = vec![0u8; ExtraAccountMetaList::size_of(3).unwrap()];
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

    /// Token-2022 resolves the two `WhitelistEntry` PDAs and the `Config` PDA
    /// from the mint's meta list on every transfer, reading each token account's
    /// `owner` field. This drives the same `ExtraAccountMeta::resolve` it calls,
    /// with the transfer's accounts in the order Token-2022 builds them, so a
    /// wrong `account_index` or data offset fails here rather than on a live
    /// transfer where the only symptom is an unresolvable account.
    #[test]
    fn extra_account_metas_resolve_to_each_side_whitelist_entry() {
        let program_id = crate::ID;
        let mint_key = Pubkey::new_unique();
        let source_key = Pubkey::new_unique();
        let destination_key = Pubkey::new_unique();
        let source_owner = Pubkey::new_unique();
        let destination_owner = Pubkey::new_unique();

        // Source, mint, destination, authority: the accounts Token-2022 hands
        // the resolver, and the indices the seed configurations refer to.
        let mut source_data = token_account_data(&mint_key, &source_owner);
        let mut destination_data = token_account_data(&mint_key, &destination_owner);
        let accounts: [(&Pubkey, &[u8]); 4] = [
            (&source_key, &source_data),
            (&mint_key, &[]),
            (&destination_key, &destination_data),
            (&source_owner, &[]),
        ];

        // The hook's own instruction data: the interface tag and the amount, as
        // `invoke_execute` builds it. None of these seeds read instruction data,
        // but a future one would, and this is the buffer it would see.
        let mut execute_data = ExecuteInstruction::SPL_DISCRIMINATOR_SLICE.to_vec();
        execute_data.extend_from_slice(&1_000u64.to_le_bytes());

        let resolved = hook_extra_account_metas()
            .unwrap()
            .iter()
            .map(|meta| {
                meta.resolve(&execute_data, &program_id, |index| {
                    accounts.get(index).map(|(key, data)| (*key, Some(*data)))
                })
                .unwrap()
            })
            .collect::<Vec<_>>();

        let expected = [
            Pubkey::find_program_address(&[b"whitelist", source_owner.as_ref()], &program_id).0,
            Pubkey::find_program_address(&[b"whitelist", destination_owner.as_ref()], &program_id)
                .0,
            Pubkey::find_program_address(&[b"config"], &program_id).0,
        ];

        assert_eq!(resolved.len(), expected.len());
        for (meta, expected_key) in resolved.iter().zip(expected.iter()) {
            assert_eq!(meta.pubkey, *expected_key);
            // Read-only: a transfer only has to carry them, and the two entries
            // are never written by `execute`.
            assert!(!meta.is_signer);
            assert!(!meta.is_writable);
        }
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
