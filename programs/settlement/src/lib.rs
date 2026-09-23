//! # T+N Settlement Escrow
//!
//! This program acts as the clearinghouse for RWA trades.
//! When an institution buys an RWA token, the tokens are locked here.
//! They cannot be withdrawn until `settlement_slot` has passed AND
//! the off-chain Transfer Agent provides a valid secp256k1 signature
//! authorizing the release. This mimics the T+1/T+2 clearing cycle of
//! traditional securities.
//!
//! The Transfer Agent signs keccak256(trade_id || taker || mint || amount)
//! with a secp256k1 key. The program recovers the public key from the
//! signature and compares it against the key stored in the `config` PDA.
//! (A secp256k1 key can never equal a Solana ed25519 address, so the
//! authorized key has to live on-chain in its native form.)
//!
//! ## Pool-gated initiation
//!
//! A `SettlementConfig` may set `pools_require_gate`, which routes every
//! `initiate_settlement` call through a pool check: the signer must be a
//! **pool PDA** of one of the wired AMM programs (their IDs are pinned in
//! `POOL_PROGRAMS`). The check is ownership + signature, not
//! deserialization: the pool account is required to be *owned by* a wired
//! AMM program and to be the `authority` signer itself. That is sound
//! because a PDA signature can only be produced by `invoke_signed` from
//! the program that owns it — a wallet cannot make an AMM-owned account
//! sign, and no other program can sign for it either. Gated configs
//! refuse wallet-initiated trades entirely (`GateRequired`); ungated
//! configs keep the original "authority owns the source" flow, in which
//! the escrow is funded from a custody account the authority controls.
//!
//! ## Custody vs. reserves
//!
//! In the pool flow the swept account (`source_token_account`) must be
//! owned by the pool PDA. The AMM programs route each trade's swap output
//! into a per-trade **custody account** (seeded with the trade id) before
//! calling here, so the pool's shared reserves are never the source: the
//! handler cannot derive every AMM's custody seeds, but the ownership
//! rule plus the per-trade routing convention keeps reserves out of
//! reach — and `vault.pool` records which pool initiated, so its own
//! cancellation path can be audited.

use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::{create_idempotent, get_associated_token_address_with_program_id},
    token_interface::{transfer, Mint, TokenAccount, TokenInterface},
};
use solana_keccak_hasher::hash;
use solana_secp256k1_recover::secp256k1_recover;

declare_id!("49ppDVbrZSqwLDj8sitSapEsMMRfC2Z7wCeMAwhPVQoY");

/// The AMM programs whose pool PDAs may route trades into settlement.
///
/// Pinned by address rather than crate dependency on purpose: the AMM
/// programs depend on this one for their CPI, so this crate cannot depend
/// back on them. A pool passes the gate only when
/// `pool.owner` is one of these programs AND `pool.key() == authority`
/// (the signer) — the PDA-signature property does the rest.
pub const POOL_PROGRAMS: [Pubkey; 4] = [
    // clmm
    pubkey!("HLMAwimHtgMV8ME9X4RaoHCAdfU35hrovEg4AU5ifw7u"),
    // dlmm
    pubkey!("5JPnmJouZ3yE9Xwaacjtr5F2E1ShYGxJ1mR4UV9EFE3c"),
    // maverick
    pubkey!("9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4"),
    // twamm
    pubkey!("FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9"),
];

/// The BPF upgradeable loader, which owns this program's `ProgramData` account.
const BPF_LOADER_UPGRADEABLE: Pubkey = pubkey!("BPFLoaderUpgradeab1e11111111111111111111111");

/// Requires `signer` to be this program's **upgrade authority**, read from the
/// program's own `ProgramData` account.
///
/// `initialize` hands out the settlement authority - the key that names the
/// Transfer Agent and can cancel trades - so it must not be claimable by
/// whoever gets there first. The upgrade authority is the key that can already
/// change the program, which makes it the natural owner of that one-time setup
/// and needs no key baked into the code.
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
pub mod settlement {
    use super::*;

    /// Initializes the clearinghouse config: an authority (multisig), and,
    /// later, the authorized Transfer Agent's secp256k1 public key.
    ///
    /// `pools_require_gate` fixes the initiation policy: `true` means only
    /// pool PDAs of the wired AMM programs may route trades into escrow
    /// (wallet initiations are refused); `false` keeps the standalone flow
    /// in which the config authority initiates from its own custody
    /// account. The flag is immutable after initialization so the policy
    /// cannot be quietly relaxed by a key rotation.
    pub fn initialize(ctx: Context<Initialize>, pools_require_gate: bool) -> Result<()> {
        // Only the upgrade authority may set the clearinghouse up: otherwise the
        // first caller would name itself the Transfer Agent's counterparty and
        // own every cancellation.
        require_upgrade_authority(&ctx.accounts.program_data, &ctx.accounts.authority.key())?;

        let config = &mut ctx.accounts.settlement_config;
        config.authority = ctx.accounts.authority.key();
        config.transfer_agent_secp256k1 = [0u8; 64]; // Not set until `set_transfer_agent`
        config.hooks_authority = Pubkey::default(); // Not set until `set_hooks_authority`
        config.pools_require_gate = u8::from(pools_require_gate);
        config.bump = ctx.bumps.settlement_config;

        msg!(
            "Settlement config initialized. Authority: {}, pools_require_gate: {}",
            config.authority,
            config.pools_require_gate == 1
        );
        Ok(())
    }

    /// Whitelists the Hooks program's `hook_authority` PDA as a valid
    /// initiator, so swap output routed through the Hooks program can be
    /// escrowed even when the config is pool-gated (a PDA cannot present a
    /// pool account of its own). Only the config authority can call this;
    /// passing the default pubkey disables hook routing.
    pub fn set_hooks_authority(ctx: Context<SetTransferAgent>, hooks_authority: Pubkey) -> Result<()> {
        ctx.accounts.settlement_config.hooks_authority = hooks_authority;
        msg!("Hooks authority set: {}", hooks_authority);
        Ok(())
    }

    /// Sets (or rotates) the Transfer Agent's secp256k1 public key.
    /// Only the config authority can call this.
    pub fn set_transfer_agent(
        ctx: Context<SetTransferAgent>,
        agent_secp256k1_pubkey: [u8; 64],
    ) -> Result<()> {
        require!(
            agent_secp256k1_pubkey != [0u8; 64],
            ErrorCode::InvalidTransferAgentKey
        );

        ctx.accounts
            .settlement_config
            .transfer_agent_secp256k1 = agent_secp256k1_pubkey;

        msg!("Transfer Agent secp256k1 key updated");
        Ok(())
    }

    /// Called by the AMM pool programs (directly or through the Hooks
    /// program) immediately after a swap.
    ///
    /// Locks the swap output in the SettlementVault PDA. When the config
    /// requires the pool gate, the signer must be a pool PDA of a wired
    /// AMM program; otherwise the original authority-owned-custody flow
    /// applies.
    pub fn initiate_settlement(
        ctx: Context<InitiateSettlement>,
        trade_id: u64,
        settlement_slot: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;

        // 1. Verify we are in the future
        require!(settlement_slot > clock.slot, ErrorCode::InvalidSettlementSlot);

        // 2. Enforce the initiation policy and pick the trade's initiator.
        //
        //    `pool` is optional: callers omit it by passing the settlement
        //    program ID in its place (Anchor's `Option<T>` treats the
        //    executing program's own ID as the `None` sentinel). When
        //    present it must be owned by a wired AMM program and be the
        //    signer itself — see the module docs for why that is sound.
        let pool = match &ctx.accounts.pool {
            Some(info) => {
                require!(
                    POOL_PROGRAMS.contains(info.owner),
                    ErrorCode::UnauthorizedPool
                );
                require_keys_eq!(
                    info.key(),
                    ctx.accounts.authority.key(),
                    ErrorCode::UnauthorizedPool
                );
                Some(info.key())
            }
            None => None,
        };

        let config = &ctx.accounts.settlement_config;
        let hooks_initiated = config.hooks_authority != Pubkey::default()
            && ctx.accounts.authority.key() == config.hooks_authority;
        if config.pools_require_gate == 1 && pool.is_none() && !hooks_initiated {
            return Err(error!(ErrorCode::GateRequired));
        }

        // 3. Setup Vault State
        let amount = ctx.accounts.source_token_account.amount;
        {
            let vault = &mut ctx.accounts.settlement_vault;
            vault.trade_id = trade_id;
            vault.taker = ctx.accounts.taker.key();
            vault.token_mint = ctx.accounts.token_mint.key();
            vault.amount = amount;
            vault.settlement_slot = settlement_slot;
            vault.status = SettlementStatus::Pending;
            vault.pool = pool.unwrap_or_default();
            vault.bump = ctx.bumps.settlement_vault;
        }

        // 4. Create the escrow token account if it does not exist yet. The
        //    account is passed unchecked, so its address is pinned here to the
        //    vault PDA's ATA for this mint and token program — the ATA program
        //    then guarantees the created (or already existing) account is a
        //    token account of that mint owned by the vault PDA. The vault PDA
        //    "signs" the creation through its seeds; idempotence makes a
        //    pre-created account a no-op.
        let expected_escrow = get_associated_token_address_with_program_id(
            &ctx.accounts.settlement_vault.key(),
            &ctx.accounts.token_mint.key(),
            &ctx.accounts.token_program.key(),
        );
        require_keys_eq!(
            ctx.accounts.settlement_vault_token.key(),
            expected_escrow,
            ErrorCode::InvalidVaultTokenAccount
        );

        let bump = [ctx.bumps.settlement_vault];
        let vault_seeds: &[&[u8]] = &[b"settlement", &trade_id.to_le_bytes(), &bump];
        create_idempotent(CpiContext::new_with_signer(
            ctx.accounts.associated_token_program.key(),
            anchor_spl::associated_token::Create {
                payer: ctx.accounts.authority.to_account_info(),
                associated_token: ctx.accounts.settlement_vault_token.to_account_info(),
                authority: ctx.accounts.settlement_vault.to_account_info(),
                mint: ctx.accounts.token_mint.to_account_info(),
                token_program: ctx.accounts.token_program.to_account_info(),
                system_program: ctx.accounts.system_program.to_account_info(),
            },
            &[vault_seeds],
        ))?;

        // 5. Transfer tokens from the custody account to the escrow.
        //    In the pool flow the source is the pool's per-trade custody
        //    account (owned by the pool PDA, seeded with this trade id) — so a
        //    pool cannot sweep its shared reserves into an escrow. In the
        //    standalone flow it is any custody account the signing authority
        //    owns.
        require!(amount != 0, ErrorCode::EmptySettlement);
        transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                anchor_spl::token_interface::Transfer {
                    from: ctx.accounts.source_token_account.to_account_info(),
                    to: ctx.accounts.settlement_vault_token.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
            ),
            amount,
        )?;

        msg!(
            "Settlement initiated. Trade ID: {} unlocks at slot {}",
            trade_id,
            settlement_slot
        );
        Ok(())
    }

    /// Allows the taker to withdraw funds after settlement_slot has passed
    /// AND the Transfer Agent has signed off.
    pub fn settle(ctx: Context<Settle>, signature: [u8; 64], recovery_id: u8) -> Result<()> {
        let clock = Clock::get()?;

        // 1. Check status
        require!(
            ctx.accounts.settlement_vault.status == SettlementStatus::Pending,
            ErrorCode::AlreadySettled
        );

        // 2. Check time lock (T+N has passed)
        require!(
            clock.slot >= ctx.accounts.settlement_vault.settlement_slot,
            ErrorCode::SettlementPeriodNotOver
        );

        // 3. Verify Transfer Agent Signature
        // The Transfer Agent signs keccak256(trade_id || taker || mint || amount).
        // We recover the secp256k1 public key and check it against the
        // authorized key stored in the config PDA.
        let trade_id = ctx.accounts.settlement_vault.trade_id;
        let amount = ctx.accounts.settlement_vault.amount;
        let vault_bump = ctx.accounts.settlement_vault.bump;

        let mut message = Vec::new();
        message.extend_from_slice(&trade_id.to_le_bytes());
        message.extend_from_slice(ctx.accounts.settlement_vault.taker.as_ref());
        message.extend_from_slice(ctx.accounts.settlement_vault.token_mint.as_ref());
        message.extend_from_slice(&amount.to_le_bytes());

        let message_hash = hash(&message).to_bytes();

        let recovered_pubkey =
            secp256k1_recover(&message_hash, recovery_id, &signature)
                .map_err(|_| error!(ErrorCode::SignatureVerificationFailed))?;

        require!(
            recovered_pubkey.0 == ctx.accounts.settlement_config.transfer_agent_secp256k1,
            ErrorCode::UnauthorizedTransferAgent
        );

        // 4. Release funds. The escrow is the vault PDA, so the CPI must be
        //    signed with the vault's own seeds - without these the token
        //    program rejects the transfer.
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"settlement",
            &trade_id.to_le_bytes(),
            &[vault_bump],
        ]];

        transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                anchor_spl::token_interface::Transfer {
                    from: ctx.accounts.vault_token_account.to_account_info(),
                    to: ctx.accounts.taker_token_account.to_account_info(),
                    authority: ctx.accounts.settlement_vault.to_account_info(),
                },
            )
            .with_signer(signer_seeds),
            amount,
        )?;

        ctx.accounts.settlement_vault.status = SettlementStatus::Settled;

        msg!("Trade ID {} settled successfully.", trade_id);
        Ok(())
    }

    /// Cancels a pending trade and returns the funds to a refund account.
    ///
    /// Two paths, chosen by what was passed:
    /// * **Config path** - the config account is present and the signer is
    ///   its `authority`. This is the standalone flow (and the compliance
    ///   escape hatch for gated pools: the settlement authority can always
    ///   refund a stuck trade).
    /// * **Pool path** - the config is omitted (the settlement program ID is
    ///   passed in its place), the signer is the pool PDA recorded on the
    ///   vault at initiation, and the `pool` account proves it. The pool (a
    ///   hooks-governed desk) can unwind its own trade before settlement
    ///   without holding the config key.
    pub fn cancel_settlement(ctx: Context<CancelSettlement>) -> Result<()> {
        require!(
            ctx.accounts.settlement_vault.status == SettlementStatus::Pending,
            ErrorCode::NotPending
        );

        let authorized = if let Some(config) = &ctx.accounts.settlement_config {
            // The config path: the account must be the canonical config PDA
            // (its `Option` wrapper only proved settlement owns it) and the
            // signer must be its recorded authority.
            let (canonical, _) = Pubkey::find_program_address(&[b"config"], &crate::ID);
            config.key() == canonical && config.authority == ctx.accounts.authority.key()
        } else if let Some(pool) = &ctx.accounts.pool {
            pool.key() == ctx.accounts.authority.key()
                && ctx.accounts.settlement_vault.pool == pool.key()
                && POOL_PROGRAMS.contains(pool.owner)
        } else {
            false
        };
        require!(authorized, ErrorCode::Unauthorized);

        // Return funds to the designated refund account.
        let trade_id = ctx.accounts.settlement_vault.trade_id;
        let amount = ctx.accounts.settlement_vault.amount;
        let vault_bump = ctx.accounts.settlement_vault.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[
            b"settlement",
            &trade_id.to_le_bytes(),
            &[vault_bump],
        ]];

        transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                anchor_spl::token_interface::Transfer {
                    from: ctx.accounts.vault_token_account.to_account_info(),
                    to: ctx.accounts.refund_token_account.to_account_info(),
                    authority: ctx.accounts.settlement_vault.to_account_info(),
                },
            )
            .with_signer(signer_seeds),
            amount,
        )?;

        ctx.accounts.settlement_vault.status = SettlementStatus::Cancelled;
        msg!("Trade ID {} cancelled and refunded.", trade_id);
        Ok(())
    }
}

// -----------------------------------------------------------------------
// ACCOUNT STRUCTURES & ENUMS
// -----------------------------------------------------------------------

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum SettlementStatus {
    Pending,
    Settled,
    Cancelled,
}

#[account]
pub struct SettlementVault {
    pub trade_id: u64,
    pub taker: Pubkey,
    pub token_mint: Pubkey,
    pub amount: u64,
    pub settlement_slot: u64,
    pub status: SettlementStatus,
    /// The AMM pool PDA that initiated this trade, or the default pubkey for
    /// a standalone (authority-initiated) trade. Only this pool may cancel
    /// via the pool path.
    pub pool: Pubkey,
    pub bump: u8,
}

impl SettlementVault {
    pub const LEN: usize = 8 + 8 + 32 + 32 + 8 + 8 + 1 + 32 + 1;
}

/// Global clearinghouse settings: who may cancel trades, which
/// secp256k1 key the Transfer Agent signs with, and whether pool PDAs are
/// the only parties allowed to route trades into escrow.
#[account]
pub struct SettlementConfig {
    pub authority: Pubkey,
    /// Uncompressed secp256k1 public key (x || y, 64 bytes) of the Transfer
    /// Agent. Secp256k1 keys are not Solana addresses; they must be stored
    /// in their native form to be comparable with a recovered key.
    pub transfer_agent_secp256k1: [u8; 64],
    /// `1` when only pool PDAs of the wired AMM programs (or the whitelisted
    /// Hooks authority) may initiate settlements; `0` keeps the standalone
    /// authority-custody flow. Fixed at initialization.
    pub pools_require_gate: u8,
    /// The Hooks program's `hook_authority` PDA, whitelisted by the config
    /// authority so hook-routed swap output can be escrowed. The default
    /// pubkey disables hook routing.
    pub hooks_authority: Pubkey,
    pub bump: u8,
}

impl SettlementConfig {
    pub const LEN: usize = 8 + 32 + 64 + 1 + 32 + 1;
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

// Anchor's `Option<T>` treats an account whose key equals the executing
// program's own ID as the `None` sentinel: callers omit an optional account
// by passing the settlement program ID in its place — there is no system
// "null account". Both optional accounts below use that convention, and
// every consumer of them handles the `None` case explicitly.

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = SettlementConfig::LEN,
        seeds = [b"config"],
        bump
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

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
pub struct SetTransferAgent<'info> {
    #[account(
        mut,
        seeds = [b"config"],
        bump = settlement_config.bump,
        has_one = authority @ ErrorCode::Unauthorized
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(trade_id: u64)]
pub struct InitiateSettlement<'info> {
    #[account(
        init,
        payer = authority,
        space = SettlementVault::LEN,
        seeds = [b"settlement", trade_id.to_le_bytes().as_ref()],
        bump
    )]
    pub settlement_vault: Account<'info, SettlementVault>,

    /// The pool's per-trade custody account holding this trade's swap
    /// output (pool flow), or any account owned by the signing authority
    /// (standalone flow). The signer must own it, since it is the authority
    /// on the transferring CPI — this is what keeps a pool's shared
    /// reserves out of reach of a trade it did not route into custody.
    #[account(
        mut,
        constraint =
            source_token_account.owner == authority.key()
            @ ErrorCode::SourceNotOwnedByAuthority
    )]
    pub source_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The escrow token account, owned by the SettlementVault PDA. Created
    /// idempotently here as the vault PDA's ATA when it does not exist, so
    /// it is passed unchecked and validated in the handler after creation.
    /// CHECK: enforced by the handler (idempotent ATA of the vault PDA;
    /// mint and owner verified after creation).
    #[account(mut)]
    pub settlement_vault_token: UncheckedAccount<'info>,

    pub token_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: The eventual recipient of the tokens. Stored in the vault and
    /// bound into the Transfer Agent's signed message at settle time.
    pub taker: AccountInfo<'info>,

    /// The AMM pool PDA moving the tokens (pool flow), or the config
    /// authority moving its custody account (standalone flow). Signs the CPI.
    #[account(mut)]
    pub authority: Signer<'info>,

    /// The AMM pool, when the caller is one (pool flow). Omitted — replaced
    /// by the settlement program ID — on the standalone path. Ownership by
    /// a wired AMM program is checked in the handler.
    /// CHECK: owner-whitelisted against `POOL_PROGRAMS` in the handler; the
    /// PDA-signature property makes it a genuine pool PDA.
    pub pool: Option<UncheckedAccount<'info>>,

    /// The clearinghouse config. Required: it decides the gate policy and
    /// (at settle time) the Transfer Agent key.
    #[account(
        seeds = [b"config"],
        bump = settlement_config.bump
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, anchor_spl::associated_token::AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(
        mut,
        seeds = [b"settlement", &settlement_vault.trade_id.to_le_bytes()],
        bump = settlement_vault.bump
    )]
    pub settlement_vault: Account<'info, SettlementVault>,

    #[account(
        seeds = [b"config"],
        bump = settlement_config.bump
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

    #[account(
        mut,
        constraint =
            vault_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            vault_token_account.owner == settlement_vault.key()
            @ ErrorCode::InvalidVaultTokenAccount
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint =
            taker_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            taker_token_account.owner == settlement_vault.taker
            @ ErrorCode::InvalidTakerTokenAccount
    )]
    pub taker_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CancelSettlement<'info> {
    #[account(
        mut,
        seeds = [b"settlement", &settlement_vault.trade_id.to_le_bytes()],
        bump = settlement_vault.bump
    )]
    pub settlement_vault: Account<'info, SettlementVault>,

    /// The clearinghouse config on the standalone/authority path. Omitted
    /// (the settlement program ID is passed in its place) on the pool path.
    /// Deliberately constraint-free: Anchor's constraint evaluation on
    /// `Option` accounts is not the place to gamble, so the handler checks
    /// the canonical address and the `authority` link itself.
    pub settlement_config: Option<Account<'info, SettlementConfig>>,

    /// The initiating pool, on the pool path. Omitted — replaced by the
    /// settlement program ID — on the config path.
    /// CHECK: on the pool path the handler requires ownership by a wired
    /// AMM program and equality with both the signer and the vault's
    /// recorded `pool`.
    pub pool: Option<UncheckedAccount<'info>>,

    #[account(
        mut,
        constraint =
            vault_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            vault_token_account.owner == settlement_vault.key()
            @ ErrorCode::InvalidVaultTokenAccount
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint =
            refund_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch
    )]
    pub refund_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The config authority (standalone path) or the initiating pool PDA
    /// (pool path) cancelling the trade.
    pub authority: Signer<'info>,

    pub token_program: Interface<'info, TokenInterface>,
}

// -----------------------------------------------------------------------
// ERROR CODES
// -----------------------------------------------------------------------

#[error_code]
pub enum ErrorCode {
    #[msg("Settlement slot must be in the future")]
    InvalidSettlementSlot,
    #[msg("Settlement period has not yet ended")]
    SettlementPeriodNotOver,
    #[msg("Trade is already settled or cancelled")]
    AlreadySettled,
    #[msg("Trade is not pending")]
    NotPending,
    #[msg("Transfer Agent signature verification failed")]
    SignatureVerificationFailed,
    #[msg("Recovered key does not match the authorized Transfer Agent")]
    UnauthorizedTransferAgent,
    #[msg("The configured Transfer Agent key cannot be empty")]
    InvalidTransferAgentKey,
    #[msg("The source token account is not owned by the signing authority")]
    SourceNotOwnedByAuthority,
    #[msg("Token account mint does not match the settlement mint")]
    MintMismatch,
    #[msg("Token account is not owned by the settlement vault PDA")]
    InvalidVaultTokenAccount,
    #[msg("The taker token account does not belong to the vault's taker")]
    InvalidTakerTokenAccount,
    #[msg("Nothing to settle: the source account is empty")]
    EmptySettlement,
    #[msg("Only the config authority may perform this action")]
    Unauthorized,
    #[msg("Caller is not a pool authorized to route trades into settlement")]
    UnauthorizedPool,
    #[msg("This settlement config requires initiation by a wired AMM pool")]
    GateRequired,
    #[msg("Only the program's upgrade authority may initialize the settlement config")]
    UnauthorizedInitializer,
}
