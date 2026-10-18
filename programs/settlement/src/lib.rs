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
//!
//! ## Token-2022 and transfer-hook accounts
//!
//! Every move this program makes uses the **checked** transfer instruction.
//! A legacy `Transfer` fails with `MintRequiredForTransfer` (0x1f) for any
//! account of a Token-2022 mint carrying the transfer-hook extension — the
//! token program needs the mint to know which hook to run — and RWA mints
//! (`rwa_mint::create_rwa_mint`) always carry one.
//!
//! A hook-bearing mint additionally needs its hook's accounts inside the
//! transfer instruction: Token-2022 resolves the mint's
//! `ExtraAccountMetaList` against the instruction's account list and fails
//! with `IncorrectAccount` if a resolved address is missing. Those accounts
//! are caller-supplied through the instruction's `remaining_accounts` (the
//! hook program, the meta list, both whitelist entries, the hook config) and
//! are appended to the transfer's account list verbatim. They are *not*
//! trusted: Token-2022 re-derives each one from the mint's own list and the
//! token accounts' `owner` fields, and the hook re-checks the binding itself.
//!
//! The consequence worth knowing: the hook checks **both** sides of every
//! transfer, so the escrow token account's owner has to be a whitelist member.
//! Every escrow is owned by one **fixed** PDA of this program,
//! `[b"escrow_authority"]`, and holds its own rent-exempt token account at
//! `[b"escrow", trade_id]`: one `set_whitelist_entry` for the escrow authority
//! covers every trade, instead of one entry (and one root rotation) per trade
//! ID. The per-trade account is what keeps trades isolated — two escrows of the
//! same mint never share a balance — while the shared owner is what keeps the
//! whitelist stable. See `programs/settlement/README.md`.
//!
//! Those escrow accounts are created here, not by the associated-token program
//! (an ATA is derived from its owner, so a fixed owner would mean one shared
//! account per mint). Token-2022 refuses `InitializeAccount*` unless the new
//! account already has room for the extensions the mint requires on its
//! accounts — `TransferHookAccount` and `TransferFeeAmount` for an RWA mint —
//! so the length is computed here from the mint's own extension list.

use anchor_lang::solana_program::instruction::AccountMeta;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use anchor_lang::prelude::*;
use anchor_lang::system_program::{create_account, CreateAccount};
use anchor_spl::{
    token_2022::spl_token_2022::{
        extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
        instruction as token_instruction,
        state::{Account as SplAccount, Mint as SplMint},
    },
    token_interface::{Mint, TokenAccount, TokenInterface},
};
use solana_keccak_hasher::hash;
use solana_secp256k1_recover::secp256k1_recover;

declare_id!("58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6");

/// The AMM programs whose pool PDAs may route trades into settlement.
///
/// Pinned by address rather than crate dependency on purpose: the AMM
/// programs depend on this one for their CPI, so this crate cannot depend
/// back on them. A pool passes the gate only when
/// `pool.owner` is one of these programs AND `pool.key() == authority`
/// (the signer) — the PDA-signature property does the rest.
pub const POOL_PROGRAMS: [Pubkey; 4] = [
    // clmm
    pubkey!("GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU"),
    // dlmm
    pubkey!("5JPnmJouZ3yE9Xwaacjtr5F2E1ShYGxJ1mR4UV9EFE3c"),
    // maverick
    pubkey!("9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4"),
    // twamm
    pubkey!("FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9"),
];

/// Slots after `settlement_slot` at which a still-`Pending` escrow opens the
/// permissionless auto-refund path (`refund_expired`). The Transfer Agent can
/// release only before this point; after it, anyone may send the escrow back to
/// the account that funded it. Roughly one day at 2.5 slots/s — a program
/// constant rather than a per-trade argument so the hooks/AMM CPIs (which only
/// carry `settlement_slots`) did not have to change.
pub const EXPIRY_WINDOW_SLOTS: u64 = 216_000;

/// secp256k1 curve order / 2, big-endian. `settle` rejects `s > n/2` so a
/// signature has exactly one valid form (no `(s, n-s)` twin), which matters if
/// signatures are kept as compliance/audit artifacts.
const HALF_CURVE_ORDER: [u8; 32] = [
    0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0x5d, 0x57, 0x6e, 0x73, 0x57, 0xa4, 0x50, 0x1d, 0xdf, 0xe9, 0x2f, 0x46, 0x68, 0x1b, 0x20, 0xa0,
];

/// Big-endian `s <= n/2` check on the compact `r || s` signature.
fn is_low_s(signature: &[u8; 64]) -> bool {
    for i in 0..32 {
        let byte = signature[32 + i];
        if byte != HALF_CURVE_ORDER[i] {
            return byte < HALF_CURVE_ORDER[i];
        }
    }
    true
}

/// Requires `signer` to be this program's **upgrade authority**, read from the
/// program's own `ProgramData` account.
///
/// `initialize` hands out the settlement authority - the key that names the
/// Transfer Agent and can cancel trades - so it must not be claimable by
/// whoever gets there first. The upgrade authority is the key that can already
/// change the program, which makes it the natural owner of that one-time setup
/// and needs no key baked into the code.
///
/// The loader-layout parser itself lives in `program_governance` (L-7): the
/// three programs with a one-time initializer — this one, `transfer_hook` and
/// `kyc_gate` — used to carry byte-identical copies, and a loader change would
/// have had to be found and fixed in three places.
fn require_upgrade_authority(program_data: &UncheckedAccount, signer: &Pubkey) -> Result<()> {
    program_governance::require_upgrade_authority(&crate::ID, program_data, signer)
        .map_err(|_| error!(ErrorCode::UnauthorizedInitializer))
}

/// Moves `amount` with the **checked** transfer instruction, carrying whatever
/// transfer-hook accounts the caller supplied.
///
/// Both halves matter for an RWA mint:
///
/// * **Checked, not legacy.** A mint carrying the transfer-hook extension makes
///   Token-2022 reject `Transfer` for any of its accounts
///   (`MintRequiredForTransfer`, 0x1f): the token program needs the mint to know
///   which hook to run and with what decimals. `transfer_checked` carries it.
/// * **Hook accounts in the account list.** Token-2022 resolves the mint's
///   `ExtraAccountMetaList` against the *instruction's* account list and fails
///   (`IncorrectAccount`) if a resolved address is not there, and it only ever
///   passes the accounts the instruction carries on to the hook. Anchor's
///   `anchor_spl::token_interface::transfer_checked` builds a fixed four-account
///   instruction and ignores `remaining_accounts`, so the instruction has to be
///   assembled here for `hook_accounts` to reach the token program at all.
///
/// The escrow's own whitelist entry is part of `hook_accounts`, and the hook
/// checks **both** sides of every transfer - so escrowing an RWA mint means the
/// escrow token account's owner is a whitelist member like any institution.
/// See the module docs and the integration tests.
#[allow(clippy::too_many_arguments)]
fn transfer_checked_with_hook<'info>(
    token_program: &Pubkey,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    decimals: u8,
    amount: u64,
    hook_accounts: &[AccountInfo<'info>],
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    let mut ix = token_instruction::transfer_checked(
        token_program,
        from.key,
        mint.key,
        to.key,
        authority.key,
        &[],
        amount,
        decimals,
    )?;
    ix.accounts.extend(hook_accounts.iter().map(|account| {
        if account.is_writable {
            AccountMeta::new(*account.key, false)
        } else {
            AccountMeta::new_readonly(*account.key, false)
        }
    }));

    let mut account_infos = vec![from.clone(), mint.clone(), to.clone(), authority.clone()];
    account_infos.extend_from_slice(hook_accounts);

    // Empty seeds are `invoke`; the escrow release passes the escrow
    // authority's, since that PDA owns the token account being spent.
    invoke_signed(&ix, &account_infos, signer_seeds)?;
    Ok(())
}

/// The data length a fresh escrow token account needs for `mint`.
///
/// Token-2022 refuses `InitializeAccount*` unless the account it is initializing
/// is already long enough for every extension the mint requires on its
/// accounts (`TransferHookAccount` for a hook-bearing RWA mint,
/// `TransferFeeAmount` for a fee-bearing one), and it sizes that set with
/// `get_required_init_account_extensions` — the same helper used here, so the
/// account matches what the token program expects byte for byte. The
/// associated-token program did this for the vault's ATA; an escrow PDA has to
/// do it itself.
///
/// A mint of the legacy token program has no extensions and every account is
/// exactly `SplAccount::LEN`; `unpack` is only reached for a Token-2022 mint,
/// whose account layout the helper is defined against.
fn escrow_account_len(token_mint: &InterfaceAccount<Mint>) -> Result<usize> {
    let mint_info = token_mint.to_account_info();
    if mint_info.owner != &anchor_spl::token_2022::ID {
        // No extensions to size for: the helper returns the base length, 165.
        return ExtensionType::try_calculate_account_len::<SplAccount>(&[]).map_err(Into::into);
    }

    let data = mint_info.try_borrow_data()?;
    let mint = StateWithExtensions::<SplMint>::unpack(&data)
        .map_err(|_| error!(ErrorCode::InvalidVaultTokenAccount))?;
    let required =
        ExtensionType::get_required_init_account_extensions(&mint.get_extension_types()?);

    ExtensionType::try_calculate_account_len::<SplAccount>(&required).map_err(Into::into)
}

/// Reads a token account's `amount` field straight out of its bytes.
///
/// The escrow is passed as an `UncheckedAccount` because Anchor cannot validate
/// a token account that does not exist yet — this program creates it a few lines
/// before it reads it — so it cannot be a typed `InterfaceAccount`. Both SPL
/// Token and Token-2022 begin with the same base layout, so the offset is the
/// same for either: `mint (32) | owner (32) | amount (8, little endian)`.
fn token_amount(account: &UncheckedAccount) -> Result<u64> {
    let data = account.try_borrow_data()?;
    require!(data.len() >= 72, ErrorCode::InvalidVaultTokenAccount);

    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[64..72]);
    Ok(u64::from_le_bytes(bytes))
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
    /// `remaining_accounts` carries the transfer's hook accounts (see the
    /// module docs): the hook program, the mint's `extra-account-metas` PDA,
    /// the sender's and the escrow's whitelist entries, and the hook config.
    /// They are required for a hook-bearing RWA mint and harmless for a mint
    /// without a hook.
    pub fn initiate_settlement<'info>(
        ctx: Context<'info, InitiateSettlement<'info>>,
        trade_id: u64,
        settlement_slot: u64,
        amount: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;

        // 1. Verify we are in the future
        require!(settlement_slot > clock.slot, ErrorCode::InvalidSettlementSlot);

        // 1a. The amount is the caller's claim about *this trade's* flow, and it
        //     is checked against the account funding the escrow. Without it a
        //     caller would be escrowing whatever balance happened to be in the
        //     account, which is not necessarily the flow it is settling.
        require!(amount > 0, ErrorCode::InvalidSettlementAmount);
        require!(
            ctx.accounts.source_token_account.amount >= amount,
            ErrorCode::InvalidSettlementAmount
        );

        // 1b. The taker is recorded and bound into the agent's signed message
        //     at settle time. A default pubkey can never own a token account,
        //     so such a trade could only ever be cancelled or refunded: refuse
        //     it up front rather than escrowing an unsettleable position.
        require!(
            ctx.accounts.taker.key() != Pubkey::default(),
            ErrorCode::InvalidTaker
        );

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

        // 3. Setup Vault State. `amount` is written by step 5, once the escrow
        //    has reported what it actually received.
        {
            let vault = &mut ctx.accounts.settlement_vault;
            vault.trade_id = trade_id;
            vault.taker = ctx.accounts.taker.key();
            vault.token_mint = ctx.accounts.token_mint.key();
            vault.amount = 0;
            vault.settlement_slot = settlement_slot;
            // The release window closes one constant window after T+N; past it
            // only `refund_expired` can move the escrow, so a Pending trade can
            // no longer be stranded (nor released) indefinitely.
            vault.expiry_slot = settlement_slot
                .checked_add(EXPIRY_WINDOW_SLOTS)
                .ok_or(ErrorCode::InvalidSettlementSlot)?;
            vault.status = SettlementStatus::Pending;
            vault.pool = pool.unwrap_or_default();
            // The account that funded this escrow, and the only account
            // `cancel_settlement` is allowed to refund. Recording it here is
            // what stops a cancellation redirecting the escrow elsewhere.
            vault.refund_source = ctx.accounts.source_token_account.key();
            vault.bump = ctx.bumps.settlement_vault;
        }

        // 4. Create this trade's escrow token account. The account is a PDA of
        //    this program - `[b"escrow", trade_id]`, pinned by the accounts
        //    struct - and is owned by the fixed `[b"escrow_authority"]` PDA, so
        //    the balance itself is per-trade (two trades of the same mint never
        //    share one) while its owner is the same key for every trade. That is
        //    what lets one whitelist entry, instead of one per trade ID, cover
        //    every escrow the transfer hook will ever see.
        //
        //    Nobody else can create an account at this address: `create_account`
        //    needs the new account's own signature, and only this program can
        //    sign for its own PDA. `settlement_vault` is `init`, so this runs at
        //    most once per trade ID, always in the same transaction that funds
        //    the escrow - a failure leaves neither account behind.
        //
        //    The length comes from the mint, and the rent for it is paid by the
        //    initiator, as it was for the ATA this replaces.
        let escrow_len = escrow_account_len(&ctx.accounts.token_mint)?;
        let escrow_bump = [ctx.bumps.settlement_vault_token];
        let escrow_seeds: &[&[u8]] = &[b"escrow", &trade_id.to_le_bytes(), &escrow_bump];
        create_account(
            CpiContext::new_with_signer(
                ctx.accounts.system_program.key(),
                CreateAccount {
                    from: ctx.accounts.authority.to_account_info(),
                    to: ctx.accounts.settlement_vault_token.to_account_info(),
                },
                &[escrow_seeds],
            ),
            Rent::get()?.minimum_balance(escrow_len),
            escrow_len as u64,
            ctx.accounts.token_program.key,
        )?;

        // 5. Initialize it as a token account of this mint owned by the escrow
        //    authority. `initialize_account3` (tag 18) carries the owner in its
        //    data and needs no rent sysvar; it also writes every extension the
        //    mint requires on its accounts, which is why step 4 had to create
        //    the account at exactly the length the token program expects.
        let (escrow_authority, _) =
            Pubkey::find_program_address(&[b"escrow_authority"], &crate::ID);
        invoke(
            &token_instruction::initialize_account3(
                &ctx.accounts.token_program.key(),
                &ctx.accounts.settlement_vault_token.key(),
                &ctx.accounts.token_mint.key(),
                &escrow_authority,
            )?,
            &[
                ctx.accounts.settlement_vault_token.to_account_info(),
                ctx.accounts.token_mint.to_account_info(),
            ],
        )?;

        // 6. Transfer the trade's output out of the custody account and into the
        //    escrow, then record what the escrow actually received rather than
        //    what was asked for: a Token-2022 transfer-fee extension can skim
        //    part of the amount in flight, and `settle` pays out exactly the
        //    amount recorded here.
        //
        //    In the pool flow the source is the pool's per-trade custody
        //    account (owned by the pool PDA, seeded with this trade id) — so a
        //    pool cannot sweep its shared reserves into an escrow. In the
        //    standalone flow it is any custody account the signing authority
        //    owns.
        //
        //    The transfer is `transfer_checked` with the caller's hook accounts
        //    appended: a legacy `Transfer` is rejected outright by Token-2022
        //    for an RWA mint (`MintRequiredForTransfer`, 0x1f).
        let escrow_before = token_amount(&ctx.accounts.settlement_vault_token)?;
        transfer_checked_with_hook(
            &ctx.accounts.token_program.key(),
            &ctx.accounts.source_token_account.to_account_info(),
            &ctx.accounts.token_mint.to_account_info(),
            &ctx.accounts.settlement_vault_token.to_account_info(),
            &ctx.accounts.authority.to_account_info(),
            ctx.accounts.token_mint.decimals,
            amount,
            ctx.remaining_accounts,
            &[],
        )?;

        let received = token_amount(&ctx.accounts.settlement_vault_token)?
            .checked_sub(escrow_before)
            .ok_or(ErrorCode::InvalidVaultTokenAccount)?;
        require!(received > 0, ErrorCode::EmptySettlement);
        ctx.accounts.settlement_vault.amount = received;

        msg!(
            "Settlement initiated. Trade ID: {} unlocks at slot {}",
            trade_id,
            settlement_slot
        );
        Ok(())
    }

    /// Allows the taker to withdraw funds after settlement_slot has passed
    /// AND the Transfer Agent has signed off.
    ///
    /// `remaining_accounts` carries the release transfer's hook accounts, as in
    /// `initiate_settlement`; here the escrow is the sender, so the entry needed
    /// on that side is the escrow authority's own (the same entry for every
    /// trade).
    pub fn settle<'info>(
        ctx: Context<'info, Settle<'info>>,
        signature: [u8; 64],
        recovery_id: u8,
    ) -> Result<()> {
        let clock = Clock::get()?;

        // 1. Check status
        require!(
            ctx.accounts.settlement_vault.status == SettlementStatus::Pending,
            ErrorCode::AlreadySettled
        );

        // 1a. The release window closes at `expiry_slot`: a trade whose T+N
        //     window has lapsed is refunded via `refund_expired`, never
        //     released, so a stale signature cannot move funds after the desk's
        //     deadline.
        require!(
            clock.slot < ctx.accounts.settlement_vault.expiry_slot,
            ErrorCode::SettlementExpired
        );

        // 2. Check time lock (T+N has passed)
        require!(
            clock.slot >= ctx.accounts.settlement_vault.settlement_slot,
            ErrorCode::SettlementPeriodNotOver
        );

        // 2a. Reject high-`s` forms of an otherwise valid signature: the curve
        //     admits `(s, n - s)` twins, and keeping only `s <= n/2` gives each
        //     authorization exactly one wire form for audit logs.
        require!(is_low_s(&signature), ErrorCode::HighSignatureS);

        // 3. Verify Transfer Agent Signature
        // The Transfer Agent signs keccak256(trade_id || taker || mint || amount).
        // We recover the secp256k1 public key and check it against the
        // authorized key stored in the config PDA.
        let trade_id = ctx.accounts.settlement_vault.trade_id;
        let amount = ctx.accounts.settlement_vault.amount;

        let mut message = Vec::new();
        message.extend_from_slice(&trade_id.to_le_bytes());
        message.extend_from_slice(ctx.accounts.settlement_vault.taker.as_ref());
        message.extend_from_slice(ctx.accounts.settlement_vault.token_mint.as_ref());
        message.extend_from_slice(&amount.to_le_bytes());

        let message_hash = hash(&message).to_bytes();

        let recovered_pubkey =
            secp256k1_recover(&message_hash, recovery_id, &signature)
                .map_err(|_| error!(ErrorCode::SignatureVerificationFailed))?;

        // A per-mint override wins over the config's global key. The account is
        // optional (the settlement program ID is the None sentinel); when
        // present it must be the canonical `[b"agent", mint]` PDA, so a caller
        // cannot substitute a key of their own choosing.
        let expected_agent_key = match &ctx.accounts.mint_agent {
            Some(agent) => {
                let (canonical, _) = Pubkey::find_program_address(
                    &[b"agent", ctx.accounts.settlement_vault.token_mint.as_ref()],
                    &crate::ID,
                );
                require_keys_eq!(agent.key(), canonical, ErrorCode::InvalidMintAgent);
                agent.transfer_agent_secp256k1
            }
            None => ctx.accounts.settlement_config.transfer_agent_secp256k1,
        };

        require!(
            recovered_pubkey.0 == expected_agent_key,
            ErrorCode::UnauthorizedTransferAgent
        );

        // 4. Release funds. The escrow token account is owned by the escrow
        //    authority PDA, so the transfer CPI has to be signed with that
        //    PDA's seeds - the vault PDA no longer signs here, and without the
        //    escrow authority's signature the token program rejects the move.
        let escrow_bump = [ctx.bumps.escrow_authority];
        let signer_seeds: &[&[&[u8]]] = &[&[b"escrow_authority", &escrow_bump]];

        transfer_checked_with_hook(
            &ctx.accounts.token_program.key(),
            &ctx.accounts.vault_token_account.to_account_info(),
            &ctx.accounts.token_mint.to_account_info(),
            &ctx.accounts.taker_token_account.to_account_info(),
            &ctx.accounts.escrow_authority.to_account_info(),
            ctx.accounts.token_mint.decimals,
            amount,
            ctx.remaining_accounts,
            signer_seeds,
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
    ///
    /// `remaining_accounts` carries the refund transfer's hook accounts, as in
    /// `initiate_settlement`; the escrow is the sender here, so the entry needed
    /// on that side is the escrow authority's own.
    pub fn cancel_settlement<'info>(ctx: Context<'info, CancelSettlement<'info>>) -> Result<()> {
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

        // Return funds to the designated refund account. As in `settle`, the
        // escrow's owner is the escrow authority PDA, and only its own seeds can
        // authorize a transfer out.
        let trade_id = ctx.accounts.settlement_vault.trade_id;
        let amount = ctx.accounts.settlement_vault.amount;
        let escrow_bump = [ctx.bumps.escrow_authority];
        let signer_seeds: &[&[&[u8]]] = &[&[b"escrow_authority", &escrow_bump]];

        transfer_checked_with_hook(
            &ctx.accounts.token_program.key(),
            &ctx.accounts.vault_token_account.to_account_info(),
            &ctx.accounts.token_mint.to_account_info(),
            &ctx.accounts.refund_token_account.to_account_info(),
            &ctx.accounts.escrow_authority.to_account_info(),
            ctx.accounts.token_mint.decimals,
            amount,
            ctx.remaining_accounts,
            signer_seeds,
        )?;

        ctx.accounts.settlement_vault.status = SettlementStatus::Cancelled;
        msg!("Trade ID {} cancelled and refunded.", trade_id);
        Ok(())
    }

    /// Permissionless auto-refund for a trade whose T+N release window lapsed
    /// without a Transfer Agent signature. Returns the full escrow to the
    /// account that funded it and marks the trade `Expired`.
    ///
    /// No signer and no config are required: the destination is pinned to the
    /// vault's own `refund_source`, so a keeper (or anyone) can trigger the
    /// refund but cannot redirect a single unit.
    ///
    /// `remaining_accounts` carries the refund transfer's hook accounts, exactly
    /// as `cancel_settlement` does.
    pub fn refund_expired<'info>(ctx: Context<'info, RefundExpired<'info>>) -> Result<()> {
        let clock = Clock::get()?;
        let vault = &mut ctx.accounts.settlement_vault;

        require!(
            vault.status == SettlementStatus::Pending,
            ErrorCode::NotPending
        );
        require!(
            clock.slot >= vault.expiry_slot,
            ErrorCode::SettlementNotExpired
        );

        let trade_id = vault.trade_id;
        let amount = vault.amount;
        let escrow_bump = [ctx.bumps.escrow_authority];
        let signer_seeds: &[&[&[u8]]] = &[&[b"escrow_authority", &escrow_bump]];

        transfer_checked_with_hook(
            &ctx.accounts.token_program.key(),
            &ctx.accounts.vault_token_account.to_account_info(),
            &ctx.accounts.token_mint.to_account_info(),
            &ctx.accounts.refund_token_account.to_account_info(),
            &ctx.accounts.escrow_authority.to_account_info(),
            ctx.accounts.token_mint.decimals,
            amount,
            ctx.remaining_accounts,
            signer_seeds,
        )?;

        vault.status = SettlementStatus::Expired;
        msg!("Trade ID {} expired and refunded to its funder.", trade_id);
        Ok(())
    }

    /// Reclaims the rent of a terminal trade: closes the (empty) escrow token
    /// account and the vault PDA, paying both to the config authority. Only the
    /// config authority may call it, and only once the trade is `Settled`,
    /// `Cancelled` or `Expired` — a `Pending` escrow still holds value.
    pub fn close_settlement<'info>(ctx: Context<'info, CloseSettlement<'info>>) -> Result<()> {
        require!(
            ctx.accounts.settlement_vault.status != SettlementStatus::Pending,
            ErrorCode::StillPending
        );
        require!(
            ctx.accounts.vault_token_account.amount == 0,
            ErrorCode::EscrowNotEmpty
        );

        // Empty the token account into the config authority, signed by the
        // escrow authority that owns it. Closing is not a transfer, so a
        // hook-bearing mint needs no hook accounts here.
        let escrow_bump = [ctx.bumps.escrow_authority];
        let signer_seeds: &[&[&[u8]]] = &[&[b"escrow_authority", &escrow_bump]];
        invoke_signed(
            &token_instruction::close_account(
                &ctx.accounts.token_program.key(),
                &ctx.accounts.vault_token_account.key(),
                &ctx.accounts.authority.key(),
                &ctx.accounts.escrow_authority.key(),
                &[],
            )?,
            &[
                ctx.accounts.vault_token_account.to_account_info(),
                ctx.accounts.authority.to_account_info(),
                ctx.accounts.escrow_authority.to_account_info(),
            ],
            signer_seeds,
        )?;

        // `settlement_vault` is `close = authority`, so Anchor returns the
        // vault's rent as this instruction returns.
        msg!(
            "Settlement {} closed; rent returned to the config authority.",
            ctx.accounts.settlement_vault.trade_id
        );
        Ok(())
    }

    /// Sets (or creates) the per-mint Transfer Agent key. Config authority only.
    /// Passing the config's global key here is allowed too, so a desk can
    /// override one mint and later revert with `clear_mint_agent`.
    pub fn set_mint_agent(
        ctx: Context<SetMintAgent>,
        agent_secp256k1_pubkey: [u8; 64],
    ) -> Result<()> {
        require!(
            agent_secp256k1_pubkey != [0u8; 64],
            ErrorCode::InvalidTransferAgentKey
        );
        ctx.accounts.mint_agent.transfer_agent_secp256k1 = agent_secp256k1_pubkey;
        ctx.accounts.mint_agent.bump = ctx.bumps.mint_agent;
        msg!("Mint agent key set for {}", ctx.accounts.token_mint.key());
        Ok(())
    }

    /// Removes a per-mint override, reverting that mint to the config's global
    /// key, and refunds the override PDA's rent to the config authority.
    pub fn clear_mint_agent<'info>(ctx: Context<'info, ClearMintAgent<'info>>) -> Result<()> {
        msg!("Mint agent key cleared for {}", ctx.accounts.token_mint.key());
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
    /// The T+N window passed without a Transfer Agent release; the escrow was
    /// returned to its funder by the permissionless `refund_expired` path.
    Expired,
}

#[account]
pub struct SettlementVault {
    pub trade_id: u64,
    pub taker: Pubkey,
    pub token_mint: Pubkey,
    pub amount: u64,
    /// Absolute slot the Transfer Agent may release from (T+N).
    pub settlement_slot: u64,
    /// Absolute slot after which a still-`Pending` escrow can only be refunded
    /// (`settlement_slot + EXPIRY_WINDOW_SLOTS`). Set at initiation.
    pub expiry_slot: u64,
    pub status: SettlementStatus,
    /// The AMM pool PDA that initiated this trade, or the default pubkey for
    /// a standalone (authority-initiated) trade. Only this pool may cancel
    /// via the pool path.
    pub pool: Pubkey,
    /// The token account that funded the escrow at initiation. Only this
    /// account can receive a cancellation refund.
    pub refund_source: Pubkey,
    pub bump: u8,
}

impl SettlementVault {
    pub const LEN: usize = 8 + 8 + 32 + 32 + 8 + 8 + 8 + 1 + 32 + 32 + 1;
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

/// Per-mint Transfer Agent override. The config's `transfer_agent_secp256k1` is
/// the fallback; a `[b"agent", mint]` PDA set by the config authority replaces
/// it for that one mint, so different asset classes can be cleared by different
/// regulated parties. `settle` reads this account when the caller passes it.
#[account]
pub struct MintAgentKey {
    /// Uncompressed secp256k1 public key (`x || y`), same encoding as the
    /// config's global key.
    pub transfer_agent_secp256k1: [u8; 64],
    pub bump: u8,
}

impl MintAgentKey {
    pub const LEN: usize = 8 + 64 + 1;
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

    /// This trade's escrow token account, owned by the fixed escrow authority
    /// PDA. It does not exist before this instruction, so it is passed
    /// unchecked (Anchor cannot validate an account that is not there yet) and
    /// its address is pinned by the `seeds` constraint instead. The handler
    /// creates and initializes it, which is what writes the mint and the owner
    /// the later `settle`/`cancel_settlement` calls re-check.
    /// CHECK: address pinned by `seeds`; created and initialized by the handler.
    #[account(mut, seeds = [b"escrow", trade_id.to_le_bytes().as_ref()], bump)]
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

    /// Optional per-mint Transfer Agent override. Callers pass the settlement
    /// program ID (the Option None sentinel) when the mint has no override; the
    /// handler pins a supplied account to the canonical `[b"agent", mint]` PDA
    /// and uses its key instead of the config's global key.
    pub mint_agent: Option<Account<'info, MintAgentKey>>,

    /// The escrowed mint, required by the checked transfer Token-2022 needs for
    /// a hook-bearing (RWA) mint. Pinned to the mint the vault recorded.
    #[account(
        constraint =
            token_mint.key() == settlement_vault.token_mint
            @ ErrorCode::MintMismatch
    )]
    pub token_mint: InterfaceAccount<'info, Mint>,

    /// This trade's escrow token account: the `[b"escrow", trade_id]` PDA of
    /// this program, owned by `escrow_authority`. Address, mint and owner are
    /// all pinned, so a release can only ever move the tokens this vault
    /// recorded.
    #[account(
        mut,
        seeds = [b"escrow", settlement_vault.trade_id.to_le_bytes().as_ref()],
        bump,
        constraint =
            vault_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            vault_token_account.owner == escrow_authority.key()
            @ ErrorCode::InvalidVaultTokenAccount
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The PDA that owns every escrow token account. It is the transfer's
    /// authority - signed by this program's `invoke_signed`, never by the
    /// transaction - which is what makes the escrow unreachable to anyone else.
    /// CHECK: derived by seed and pinned by the `seeds` constraint; it holds no
    /// data of its own (it may not even exist as an account yet).
    #[account(seeds = [b"escrow_authority"], bump)]
    pub escrow_authority: UncheckedAccount<'info>,

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

    /// The escrowed mint, required by the checked transfer Token-2022 needs for
    /// a hook-bearing (RWA) mint. Pinned to the mint the vault recorded.
    #[account(
        constraint =
            token_mint.key() == settlement_vault.token_mint
            @ ErrorCode::MintMismatch
    )]
    pub token_mint: InterfaceAccount<'info, Mint>,

    /// This trade's escrow token account, pinned exactly as in `settle`.
    #[account(
        mut,
        seeds = [b"escrow", settlement_vault.trade_id.to_le_bytes().as_ref()],
        bump,
        constraint =
            vault_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            vault_token_account.owner == escrow_authority.key()
            @ ErrorCode::InvalidVaultTokenAccount
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The PDA that owns every escrow token account; the refund's authority.
    /// CHECK: derived by seed and pinned by the `seeds` constraint.
    #[account(seeds = [b"escrow_authority"], bump)]
    pub escrow_authority: UncheckedAccount<'info>,

    /// The escrow's refund destination. Constrained to the account recorded on
    /// the vault at initiation — the one that funded it — rather than merely
    /// mint-checked, so a cancellation returns the funds to their source and
    /// cannot be redirected to an account the canceller picks.
    #[account(
        mut,
        constraint =
            refund_token_account.key() == settlement_vault.refund_source
            @ ErrorCode::InvalidRefundAccount,
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

#[derive(Accounts)]
pub struct RefundExpired<'info> {
    #[account(
        mut,
        seeds = [b"settlement", &settlement_vault.trade_id.to_le_bytes()],
        bump = settlement_vault.bump
    )]
    pub settlement_vault: Account<'info, SettlementVault>,

    /// The escrowed mint; pinned to the vault's recorded mint.
    #[account(
        constraint = token_mint.key() == settlement_vault.token_mint @ ErrorCode::MintMismatch
    )]
    pub token_mint: InterfaceAccount<'info, Mint>,

    /// This trade's escrow token account, pinned exactly as in `settle`.
    #[account(
        mut,
        seeds = [b"escrow", settlement_vault.trade_id.to_le_bytes().as_ref()],
        bump,
        constraint =
            vault_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            vault_token_account.owner == escrow_authority.key()
            @ ErrorCode::InvalidVaultTokenAccount
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The PDA that owns every escrow token account; signs the refund.
    /// CHECK: derived by seed and pinned by the `seeds` constraint.
    #[account(seeds = [b"escrow_authority"], bump)]
    pub escrow_authority: UncheckedAccount<'info>,

    /// Pinned to the account that funded the escrow, exactly as in
    /// `cancel_settlement`: the auto-refund cannot be redirected.
    #[account(
        mut,
        constraint =
            refund_token_account.key() == settlement_vault.refund_source
            @ ErrorCode::InvalidRefundAccount,
        constraint =
            refund_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch
    )]
    pub refund_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct CloseSettlement<'info> {
    /// `close` returns the vault's rent to the config authority on success.
    #[account(
        mut,
        seeds = [b"settlement", &settlement_vault.trade_id.to_le_bytes()],
        bump = settlement_vault.bump,
        close = authority
    )]
    pub settlement_vault: Account<'info, SettlementVault>,

    #[account(
        seeds = [b"config"],
        bump = settlement_config.bump,
        has_one = authority @ ErrorCode::Unauthorized
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

    /// The config authority: receives both rent refunds.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        constraint = token_mint.key() == settlement_vault.token_mint @ ErrorCode::MintMismatch
    )]
    pub token_mint: InterfaceAccount<'info, Mint>,

    /// The trade's escrow token account; must be empty and is closed by the
    /// handler via the token program.
    #[account(
        mut,
        seeds = [b"escrow", settlement_vault.trade_id.to_le_bytes().as_ref()],
        bump,
        constraint =
            vault_token_account.mint == settlement_vault.token_mint
            @ ErrorCode::MintMismatch,
        constraint =
            vault_token_account.owner == escrow_authority.key()
            @ ErrorCode::InvalidVaultTokenAccount
    )]
    pub vault_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: derived by seed and pinned by the `seeds` constraint; signs the
    /// token account's close.
    #[account(seeds = [b"escrow_authority"], bump)]
    pub escrow_authority: UncheckedAccount<'info>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct SetMintAgent<'info> {
    #[account(
        seeds = [b"config"],
        bump = settlement_config.bump,
        has_one = authority @ ErrorCode::Unauthorized
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

    #[account(mut)]
    pub authority: Signer<'info>,

    /// The mint the override applies to; also names the `[b"agent", mint]` PDA.
    pub token_mint: InterfaceAccount<'info, Mint>,

    #[account(
        init_if_needed,
        payer = authority,
        space = MintAgentKey::LEN,
        seeds = [b"agent", token_mint.key().as_ref()],
        bump
    )]
    pub mint_agent: Account<'info, MintAgentKey>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ClearMintAgent<'info> {
    #[account(
        seeds = [b"config"],
        bump = settlement_config.bump,
        has_one = authority @ ErrorCode::Unauthorized
    )]
    pub settlement_config: Account<'info, SettlementConfig>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub token_mint: InterfaceAccount<'info, Mint>,

    /// `close` refunds the override's rent to the config authority.
    #[account(
        mut,
        seeds = [b"agent", token_mint.key().as_ref()],
        bump = mint_agent.bump,
        close = authority
    )]
    pub mint_agent: Account<'info, MintAgentKey>,
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
    #[msg("Trade is already settled, cancelled or expired")]
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
    #[msg("Escrow token account is not the trade's escrow PDA, or it is not owned by the escrow authority")]
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
    #[msg("Refund account is not the account that funded this escrow")]
    InvalidRefundAccount,
    #[msg("The settlement amount must be greater than zero and no larger than the source balance")]
    InvalidSettlementAmount,
    #[msg("The trade's T+N release window has expired; it can only be refunded")]
    SettlementExpired,
    #[msg("The trade has not reached its refund deadline yet")]
    SettlementNotExpired,
    #[msg("The secp256k1 signature is not in low-s form")]
    HighSignatureS,
    #[msg("The recorded taker cannot be the default pubkey")]
    InvalidTaker,
    #[msg("The per-mint agent account is not the canonical [b\"agent\", mint] PDA")]
    InvalidMintAgent,
    #[msg("The escrow still holds tokens; settle, cancel or refund it first")]
    EscrowNotEmpty,
    #[msg("The trade is still pending; settle, cancel or expire it before closing")]
    StillPending,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature_with_s(s: [u8; 32]) -> [u8; 64] {
        let mut signature = [0u8; 64];
        signature[32..].copy_from_slice(&s);
        signature
    }

    #[test]
    fn low_s_accepts_zero_and_the_half_order_boundary() {
        assert!(is_low_s(&signature_with_s([0u8; 32])));
        assert!(is_low_s(&signature_with_s(HALF_CURVE_ORDER)));
    }

    #[test]
    fn low_s_rejects_the_high_twin_and_maximum() {
        let mut above = HALF_CURVE_ORDER;
        // n/2 + 1: the smallest high-s value.
        for i in (0..32).rev() {
            if above[i] != 0xff {
                above[i] += 1;
                break;
            }
            above[i] = 0;
        }
        assert!(!is_low_s(&signature_with_s(above)));
        assert!(!is_low_s(&signature_with_s([0xffu8; 32])));
    }

    #[test]
    fn account_sizes_match_the_documented_layouts() {
        // 8 discriminator + 8 id + 32 taker + 32 mint + 8 amount +
        // 8 settlement_slot + 8 expiry_slot + 1 status + 32 pool +
        // 32 refund_source + 1 bump.
        assert_eq!(SettlementVault::LEN, 170);
        assert_eq!(MintAgentKey::LEN, 73);
        assert_eq!(SettlementConfig::LEN, 138);
    }

    #[test]
    fn the_expiry_window_extends_past_the_settlement_slot() {
        assert!(EXPIRY_WINDOW_SLOTS > 0);
        let settlement_slot = 1_000u64;
        let expiry = settlement_slot
            .checked_add(EXPIRY_WINDOW_SLOTS)
            .expect("no overflow for a realistic slot");
        assert!(expiry > settlement_slot);
    }
}
