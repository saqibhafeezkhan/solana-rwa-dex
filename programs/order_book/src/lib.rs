//! # Limit Order Book
//!
//! The four AMM engines can only trade against a pool. This program adds the
//! missing primitive for institutional desks: a **resting maker order** that a
//! counterparty can fill, fully or in pieces, at a price the maker chose.
//!
//! ## Lifecycle
//!
//! * `create_order` escrows the maker's side into an associated token account
//!   owned by the order PDA. The maker states a price (quote per base, scaled by
//!   [`PRICE_SCALE`]), an expiry and a side:
//!   - **Ask** escrows base and sells it for quote.
//!   - **Bid** escrows quote and buys base with it.
//! * `fill_order` fills some or all of the escrow. The fill amount is always
//!   denominated in the order's escrow token, so a partial fill never has to
//!   guess how the other leg rounds: the taker receives `amount` and pays the
//!   computed counter-leg, bounded by `max_counter_amount`.
//! * `cancel_order` refunds whatever is left and closes both the order and its
//!   escrow account, returning all rent to the maker. It also works after the
//!   order has been filled completely, as the cleanup path.
//!
//! ## Proceeds and the T+N rail
//!
//! A maker's proceeds are the taker's payment. If the book was initialized with
//! a `hooks_authority`, those proceeds are **not** paid to the maker directly:
//! they are paid into the hooks program's custody ATA for the payment mint and
//! then locked in the T+N settlement escrow by `hooks::after_swap`. The
//! settlement vault records the maker as its taker, so the Transfer Agent's
//! release signature binds the payout to the party that placed the order. Each
//! routed fill names its own `trade_id`, which keys its settlement vault. The
//! id is chosen by the caller - non-zero and unique across the settlement
//! program's namespace - exactly as the AMM engines' swaps choose theirs, so
//! fills of different orders never contend for one book-wide counter.
//!
//! With the rail off (`hooks_authority == Pubkey::default()`) the same transfer
//! pays the maker's own ATA and no settlement accounts are used.
//!
//! ## Compliance
//!
//! When the book has a `kyc_config`, `create_order` verifies the maker and
//! `fill_order` verifies the taker against `kyc_gate` before any token moves.
//! Cancellation is deliberately not gated: an exit must never be blocked.
//!
//! ## Trust model and limits
//!
//! Creating a book is restricted to the program's upgrade authority, so a market
//! cannot be squatted with an authority key the venue does not control. The
//! book's authority can pause it; pausing refuses new orders and fills but never
//! blocks cancellation. Orders are rejected after expiry, and the program holds
//! no shared vault: every order's funds sit in its own escrow account, so one
//! maker's order can never reach another's balance.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::AccountMeta;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::associated_token::{get_associated_token_address_with_program_id, AssociatedToken};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

declare_id!("3ACX2Lc67uZW65PA3QbYRLttmxE766eSDyD9Sd2yggwD");

/// PDA seeds.
const BOOK_SEED: &[u8] = b"book";
const ORDER_SEED: &[u8] = b"order";

/// Price scale: `price` is quote raw units per base raw unit, multiplied by
/// this factor. A price of `2_000_000_000` means one base unit costs two quote
/// units, independent of either mint's decimals.
pub const PRICE_SCALE: u128 = 1_000_000_000;

/// The side a maker is resting on.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// Sell base for quote; the escrow holds base.
    Ask,
    /// Buy base with quote; the escrow holds quote.
    Bid,
}

#[program]
pub mod order_book {
    use super::*;

    /// Creates a book for one `base`/`quote` pair.
    ///
    /// Only the program's upgrade authority may create a book: the book PDA is
    /// deterministic, so a permissionless initializer could be front-run by
    /// someone who then owns the pause key for that market forever.
    ///
    /// `kyc_config` and `hooks_authority` are fixed at creation. Pass
    /// `Pubkey::default()` (and zero `settlement_slots`) to leave either rail
    /// off. When the settlement rail is on, `hooks_authority` must be the hooks
    /// program's canonical `[b"hook_authority"]` PDA and the slot horizon must be
    /// non-zero.
    pub fn initialize_book(
        ctx: Context<InitializeBook>,
        kyc_config: Pubkey,
        hooks_authority: Pubkey,
        settlement_slots: u64,
    ) -> Result<()> {
        require!(
            ctx.accounts.base_mint.key() != ctx.accounts.quote_mint.key(),
            ErrorCode::IdenticalMints
        );

        program_governance::require_upgrade_authority(
            &crate::ID,
            &ctx.accounts.program_data,
            &ctx.accounts.authority.key(),
        )
        .map_err(|_| error!(ErrorCode::UnauthorizedInitializer))?;

        if kyc_config != Pubkey::default() {
            let (expected, _) = Pubkey::find_program_address(&[b"kyc-config"], &kyc_gate::ID);
            require_keys_eq!(kyc_config, expected, ErrorCode::WrongKycConfig);
        }

        if hooks_authority != Pubkey::default() {
            let (expected, _) = Pubkey::find_program_address(&[b"hook_authority"], &hooks::ID);
            require_keys_eq!(hooks_authority, expected, ErrorCode::WrongHooksAuthority);
            require!(settlement_slots > 0, ErrorCode::InvalidSettlementSlots);
        } else {
            require!(settlement_slots == 0, ErrorCode::InvalidSettlementSlots);
        }

        let book = &mut ctx.accounts.book;
        book.mint_base = ctx.accounts.base_mint.key();
        book.mint_quote = ctx.accounts.quote_mint.key();
        book.authority = ctx.accounts.authority.key();
        book.kyc_config = kyc_config;
        book.hooks_authority = hooks_authority;
        book.settlement_slots = settlement_slots;
        book.paused = false;
        book.bump = ctx.bumps.book;

        msg!(
            "Order book {} initialized for {} / {}. KYC {}, settlement {} (T+{} slots)",
            book.key(),
            book.mint_base,
            book.mint_quote,
            kyc_config,
            hooks_authority,
            settlement_slots
        );
        Ok(())
    }

    /// Places a resting order and escrows its input.
    ///
    /// `price` is quote raw units per base raw unit, scaled by
    /// [`PRICE_SCALE`]. `expiry_timestamp` is an absolute unix timestamp; the
    /// order can be filled until it, and cancelled at any time.
    ///
    /// The escrow is an ATA owned by the order PDA. Its mint is `base` for an
    /// ask and `quote` for a bid, and the deposit is measured after transfer so
    /// a Token-2022 transfer fee cannot leave the order promising more than the
    /// escrow actually holds.
    pub fn create_order<'info>(
        ctx: Context<'info, CreateOrder<'info>>,
        order_id: u64,
        side: Side,
        price: u64,
        amount: u64,
        expiry_timestamp: i64,
    ) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        require!(price > 0, ErrorCode::ZeroPrice);
        require!(!ctx.accounts.book.paused, ErrorCode::BookPaused);

        let now = Clock::get()?.unix_timestamp;
        require!(expiry_timestamp > now, ErrorCode::OrderAlreadyExpired);

        let expected_escrow_mint = match side {
            Side::Ask => ctx.accounts.base_mint.key(),
            Side::Bid => ctx.accounts.quote_mint.key(),
        };
        require_keys_eq!(
            ctx.accounts.escrow_mint.key(),
            expected_escrow_mint,
            ErrorCode::WrongEscrowMint
        );

        // KYC rail: the maker must hold an attestation before anything is
        // escrowed. Rail accounts are checked and used here, never trusted.
        kyc_gate_check(
            &ctx.accounts.book,
            &ctx.accounts.kyc_config,
            &ctx.accounts.user_kyc_account,
            &ctx.accounts.kyc_program,
            &ctx.accounts.maker,
        )?;

        let received = transfer_and_measure(
            &ctx.accounts.token_program,
            &ctx.accounts.maker_token,
            &ctx.accounts.escrow_mint,
            &mut ctx.accounts.escrow_token,
            &ctx.accounts.maker.to_account_info(),
            ctx.remaining_accounts,
            &[],
            amount,
        )?;
        require!(received > 0, ErrorCode::ZeroAmount);

        let order = &mut ctx.accounts.order;
        order.book = ctx.accounts.book.key();
        order.owner = ctx.accounts.maker.key();
        order.order_id = order_id;
        order.side = side;
        order.price = price;
        order.original_amount = received;
        order.remaining_amount = received;
        order.filled_base = 0;
        order.filled_quote = 0;
        order.created_at = now;
        order.expiry_timestamp = expiry_timestamp;
        order.bump = ctx.bumps.order;

        msg!(
            "Order {} {} on book {}: {} escrowed at price {}/{}",
            order_id,
            match side {
                Side::Ask => "ask",
                Side::Bid => "bid",
            },
            order.book,
            received,
            price,
            PRICE_SCALE
        );
        Ok(())
    }

    /// Fills all or part of a resting order.
    ///
    /// `amount` is in the order's escrow token: base for an ask, quote for a
    /// bid. The taker receives that amount and pays the counter-leg derived
    /// from the order's price, rounded so the maker is never short-changed.
    /// `max_counter_amount` is the taker's slippage bound (quote for an ask,
    /// base for a bid).
    ///
    /// `trade_id` keys the per-fill settlement vault and is required (non-zero)
    /// on a routed book; it is ignored when the rail is off. Any taker names it,
    /// as the AMM swaps do, so two fills of different orders do not compete for
    /// a shared counter. A duplicate id fails closed: the settlement program's
    /// vault `init` rejects an id whose vault already exists.
    ///
    /// Remaining accounts carry the transfer-hook accounts for both mints (see
    /// the module docs of `hooks`/`settlement`). They are empty for mints
    /// without a transfer hook.
    pub fn fill_order<'info>(
        ctx: Context<'info, FillOrder<'info>>,
        amount: u64,
        max_counter_amount: u64,
        trade_id: u64,
    ) -> Result<()> {
        require!(amount > 0, ErrorCode::ZeroAmount);
        require!(!ctx.accounts.book.paused, ErrorCode::BookPaused);

        let now = Clock::get()?.unix_timestamp;
        let order = &mut ctx.accounts.order;
        require!(now < order.expiry_timestamp, ErrorCode::OrderExpired);
        require!(
            amount <= order.remaining_amount,
            ErrorCode::InsufficientOrderAmount
        );
        require!(
            ctx.accounts.maker.key() == order.owner,
            ErrorCode::WrongMaker
        );

        // The escrow mint must be the one the order's side escrows: an ask
        // holds base, a bid holds quote. `escrow_token` is pinned below to the
        // order's ATA for `escrow_mint`, so this is what stops a fill from
        // being pointed at the order's account for the other mint.
        let expected_escrow_mint = match order.side {
            Side::Ask => ctx.accounts.base_mint.key(),
            Side::Bid => ctx.accounts.quote_mint.key(),
        };
        require_keys_eq!(
            ctx.accounts.escrow_mint.key(),
            expected_escrow_mint,
            ErrorCode::WrongEscrowMint
        );

        let order_info = order.to_account_info();
        let (base_amount, quote_amount) = fill_legs(order.side, amount, order.price)?;
        let (counter_amount, payment_is_base) = match order.side {
            Side::Ask => (quote_amount, false),
            Side::Bid => (base_amount, true),
        };
        require!(
            counter_amount <= max_counter_amount,
            ErrorCode::SlippageExceeded
        );

        // Write the order's new state *before* any external call. A transfer
        // hook on either mint runs arbitrary code mid-instruction, and Anchor
        // only flushes account data at the end of the instruction, so a
        // re-entrant fill would otherwise read the stale `remaining_amount` and
        // over-fill. `exit` serializes the account now; if a later step fails,
        // the whole transaction - including this write - is rolled back.
        order.remaining_amount = order
            .remaining_amount
            .checked_sub(amount)
            .ok_or(ErrorCode::MathOverflow)?;
        order.filled_base = order
            .filled_base
            .checked_add(base_amount)
            .ok_or(ErrorCode::MathOverflow)?;
        order.filled_quote = order
            .filled_quote
            .checked_add(quote_amount)
            .ok_or(ErrorCode::MathOverflow)?;
        order.exit(&crate::ID)?;

        // KYC rail: the counterparty must be attested before it can trade.
        kyc_gate_check(
            &ctx.accounts.book,
            &ctx.accounts.kyc_config,
            &ctx.accounts.user_kyc_account,
            &ctx.accounts.kyc_program,
            &ctx.accounts.taker,
        )?;

        // The two legs can live under different token programs (the RWA base is
        // typically Token-2022 while the quote is legacy USDC), so each transfer
        // uses the program that owns its mint. The escrow is always on the
        // order's own side; the payment is always on the other side.
        let (escrow_token_program, payment_token_program) = match order.side {
            Side::Ask => (
                &ctx.accounts.base_token_program,
                &ctx.accounts.quote_token_program,
            ),
            Side::Bid => (
                &ctx.accounts.quote_token_program,
                &ctx.accounts.base_token_program,
            ),
        };
        require_keys_eq!(
            ctx.accounts.escrow_token.key(),
            get_associated_token_address_with_program_id(
                &order.key(),
                &ctx.accounts.escrow_mint.key(),
                &escrow_token_program.key(),
            ),
            ErrorCode::InvalidEscrowAccount
        );

        let (payment_mint, payment_source, receive_destination, receive_mint) = if payment_is_base {
            (
                &ctx.accounts.base_mint,
                &ctx.accounts.taker_base,
                &ctx.accounts.taker_quote,
                &ctx.accounts.quote_mint,
            )
        } else {
            (
                &ctx.accounts.quote_mint,
                &ctx.accounts.taker_quote,
                &ctx.accounts.taker_base,
                &ctx.accounts.base_mint,
            )
        };

        let settlement_on = ctx.accounts.book.hooks_authority != Pubkey::default();
        if settlement_on {
            let hooks_authority = ctx
                .accounts
                .hooks_authority
                .as_ref()
                .ok_or(ErrorCode::RailAccountUnexpected)?;
            require_keys_eq!(
                hooks_authority.key(),
                ctx.accounts.book.hooks_authority,
                ErrorCode::WrongHooksAuthority
            );
            require_keys_eq!(
                ctx.accounts.proceeds_token.key(),
                get_associated_token_address_with_program_id(
                    &hooks_authority.key(),
                    &payment_mint.key(),
                    &payment_token_program.key(),
                ),
                ErrorCode::InvalidCustodyAccount
            );
            require_keys_eq!(
                ctx.accounts.proceeds_token.owner,
                hooks_authority.key(),
                ErrorCode::InvalidCustodyAccount
            );
            require_keys_eq!(
                ctx.accounts.proceeds_token.mint,
                payment_mint.key(),
                ErrorCode::InvalidCustodyAccount
            );
            require!(
                ctx.accounts.hooks_program.is_some(),
                ErrorCode::RailAccountUnexpected
            );

            // One settlement vault per fill. The caller names the id, as the
            // AMMs do, rather than consuming a book-wide counter: a shared
            // counter lets any taker take the next id with a one-unit fill and
            // make every other pending fill revert. A duplicate id fails
            // closed in the settlement program's vault `init`.
            require!(trade_id != 0, ErrorCode::InvalidTradeId);

            let (expected_vault, _) = Pubkey::find_program_address(
                &[b"settlement", &trade_id.to_le_bytes()],
                &settlement::ID,
            );
            let (expected_escrow, _) = Pubkey::find_program_address(
                &[b"escrow", &trade_id.to_le_bytes()],
                &settlement::ID,
            );
            require_keys_eq!(
                ctx.accounts
                    .settlement_vault
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .key(),
                expected_vault,
                ErrorCode::WrongSettlementVault
            );
            require_keys_eq!(
                ctx.accounts
                    .settlement_vault_token
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?
                    .key(),
                expected_escrow,
                ErrorCode::WrongSettlementVault
            );
            let (expected_config, _) = Pubkey::find_program_address(&[b"config"], &settlement::ID);
            require_keys_eq!(
                ctx.accounts.settlement_config.key(),
                expected_config,
                ErrorCode::WrongSettlementConfig
            );
            require_keys_eq!(
                ctx.accounts.settlement_program.key(),
                settlement::ID,
                ErrorCode::WrongSettlementProgram
            );

            // Pay the taker's leg into the hook custody account, then lock
            // exactly what arrived in the settlement escrow. Measuring the
            // proceeds account (rather than trusting `counter_amount`) keeps
            // a Token-2022 transfer fee from over-stating the escrow.
            let received = transfer_and_measure(
                payment_token_program,
                payment_source,
                payment_mint,
                &mut ctx.accounts.proceeds_token,
                &ctx.accounts.taker.to_account_info(),
                ctx.remaining_accounts,
                &[],
                counter_amount,
            )?;
            require!(received > 0, ErrorCode::ZeroAmount);

            settle_proceeds(
                hooks_authority,
                &ctx.accounts.maker,
                &ctx.accounts.proceeds_token,
                payment_mint,
                ctx.accounts
                    .settlement_vault
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                ctx.accounts
                    .settlement_vault_token
                    .as_ref()
                    .ok_or(ErrorCode::RailAccountUnexpected)?,
                &ctx.accounts.settlement_config,
                &ctx.accounts.settlement_program,
                &ctx.accounts.system_program,
                payment_token_program,
                ctx.remaining_accounts,
                trade_id,
                ctx.accounts.book.settlement_slots,
                received,
            )?;
        } else {
            require!(
                ctx.accounts.hooks_authority.is_none()
                    && ctx.accounts.settlement_vault.is_none()
                    && ctx.accounts.settlement_vault_token.is_none()
                    && ctx.accounts.hooks_program.is_none(),
                ErrorCode::RailAccountUnexpected
            );
            for rail in [
                &ctx.accounts.settlement_config,
                &ctx.accounts.settlement_program,
                &ctx.accounts.associated_token_program,
            ] {
                require_keys_eq!(rail.key(), crate::ID, ErrorCode::RailAccountUnexpected);
            }
            require_keys_eq!(
                ctx.accounts.proceeds_token.key(),
                get_associated_token_address_with_program_id(
                    &order.owner,
                    &payment_mint.key(),
                    &payment_token_program.key(),
                ),
                ErrorCode::InvalidProceedsAccount
            );
            require_keys_eq!(
                ctx.accounts.proceeds_token.owner,
                order.owner,
                ErrorCode::InvalidProceedsAccount
            );
            require_keys_eq!(
                ctx.accounts.proceeds_token.mint,
                payment_mint.key(),
                ErrorCode::InvalidProceedsAccount
            );

            transfer_checked(
                payment_token_program,
                payment_source,
                payment_mint,
                &ctx.accounts.proceeds_token.to_account_info(),
                &ctx.accounts.taker.to_account_info(),
                ctx.remaining_accounts,
                &[],
                counter_amount,
            )?;
        }

        // Release the maker's escrow to the taker, signed by the order PDA.
        let bump = [order.bump];
        let order_seeds: &[&[u8]] = &[
            ORDER_SEED,
            order.book.as_ref(),
            order.owner.as_ref(),
            &order.order_id.to_le_bytes(),
            &bump,
        ];
        let signer_seeds: &[&[&[u8]]] = &[order_seeds];
        transfer_checked(
            escrow_token_program,
            &ctx.accounts.escrow_token,
            receive_mint,
            &receive_destination.to_account_info(),
            &order_info,
            ctx.remaining_accounts,
            signer_seeds,
            amount,
        )?;

        msg!(
            "Order {} filled {} ({} base / {} quote). Remaining {}",
            order.order_id,
            amount,
            base_amount,
            quote_amount,
            order.remaining_amount
        );
        Ok(())
    }

    /// Cancels a resting order and returns its unfilled escrow.
    ///
    /// The escrow's whole balance is transferred back to the maker, the escrow
    /// token account is closed, and the order account is closed by Anchor - so
    /// all rent returns to the maker. Cancelling a fully filled order is the
    /// normal cleanup path: there is nothing to refund, but both accounts still
    /// close.
    ///
    /// Not gated by KYC or by a pause: an exit must never be blocked.
    pub fn cancel_order<'info>(ctx: Context<'info, CancelOrder<'info>>) -> Result<()> {
        let maker_key = ctx.accounts.maker.key();
        let escrow_mint_key = ctx.accounts.escrow_mint.key();
        let order = &mut ctx.accounts.order;

        require!(order.owner == maker_key, ErrorCode::Unauthorized);
        let expected_escrow_mint = match order.side {
            Side::Ask => ctx.accounts.base_mint.key(),
            Side::Bid => ctx.accounts.quote_mint.key(),
        };
        require_keys_eq!(
            escrow_mint_key,
            expected_escrow_mint,
            ErrorCode::WrongEscrowMint
        );

        let order_info = order.to_account_info();
        // Sweep the escrow's whole balance rather than just the recorded
        // `remaining_amount`. Anyone can send tokens to the order's publicly
        // derivable ATA, and the token program refuses to close an account with
        // a non-zero balance, so refunding only the recorded amount would let a
        // one-unit donation brick cancellation and trap the maker's funds. Any
        // extra is a gift to the maker, who owns the order.
        let refund = ctx.accounts.escrow_token.amount;
        if refund > 0 {
            let bump = [order.bump];
            let order_seeds: &[&[u8]] = &[
                ORDER_SEED,
                order.book.as_ref(),
                order.owner.as_ref(),
                &order.order_id.to_le_bytes(),
                &bump,
            ];
            let signer_seeds: &[&[&[u8]]] = &[order_seeds];
            transfer_checked(
                &ctx.accounts.token_program,
                &ctx.accounts.escrow_token,
                &ctx.accounts.escrow_mint,
                &ctx.accounts.maker_token.to_account_info(),
                &order_info,
                ctx.remaining_accounts,
                signer_seeds,
                refund,
            )?;
        }
        order.remaining_amount = 0;

        close_token_account(
            &ctx.accounts.token_program,
            &ctx.accounts.escrow_token,
            &ctx.accounts.maker.to_account_info(),
            &order_info,
            &[&[
                ORDER_SEED,
                order.book.as_ref(),
                order.owner.as_ref(),
                &order.order_id.to_le_bytes(),
                &[order.bump],
            ]],
        )?;

        msg!("Order {} cancelled; {} refunded", order.order_id, refund);
        Ok(())
    }

    /// Pauses or resumes the book. Pausing refuses new orders and fills;
    /// cancellations stay open so committed capital can always exit.
    pub fn set_paused(ctx: Context<Admin>, paused: bool) -> Result<()> {
        ctx.accounts.book.paused = paused;
        msg!("Book {} paused: {}", ctx.accounts.book.key(), paused);
        Ok(())
    }

    /// Hands the book's admin role to `new_authority`.
    pub fn set_authority(ctx: Context<Admin>, new_authority: Pubkey) -> Result<()> {
        require_keys_neq!(
            new_authority,
            Pubkey::default(),
            ErrorCode::InvalidAuthority
        );
        ctx.accounts.book.authority = new_authority;
        msg!(
            "Book {} authority set to {}",
            ctx.accounts.book.key(),
            new_authority
        );
        Ok(())
    }
}

// -----------------------------------------------------------------------
// ACCOUNT STATE
// -----------------------------------------------------------------------

/// One market: a base/quote pair plus its compliance and settlement policy.
#[account]
pub struct Book {
    pub mint_base: Pubkey,
    pub mint_quote: Pubkey,
    /// The only key that can pause the book or rotate itself.
    pub authority: Pubkey,
    /// KYC gate config PDA; `Pubkey::default()` means the rail is off.
    pub kyc_config: Pubkey,
    /// Hooks program authority PDA; `Pubkey::default()` means proceeds are paid
    /// directly to the maker instead of being escrowed for T+N settlement.
    pub hooks_authority: Pubkey,
    /// T+N length in slots for settlement-routed proceeds.
    pub settlement_slots: u64,
    pub paused: bool,
    pub bump: u8,
}

impl Book {
    pub const LEN: usize = 8 + 32 + 32 + 32 + 32 + 32 + 8 + 1 + 1;
}

/// One resting maker order. Its escrow token account is an ATA owned by this
/// PDA, so `remaining_amount` is the order's entire claim on the escrow.
#[account]
pub struct Order {
    pub book: Pubkey,
    pub owner: Pubkey,
    pub order_id: u64,
    pub side: Side,
    /// Quote raw units per base raw unit, scaled by [`PRICE_SCALE`].
    pub price: u64,
    /// What the escrow received at creation (net of any transfer fee).
    pub original_amount: u64,
    /// Unfilled escrow, in the escrow token: base for an ask, quote for a bid.
    pub remaining_amount: u64,
    /// Cumulative base exchanged over the order's life.
    pub filled_base: u64,
    /// Cumulative quote exchanged over the order's life.
    pub filled_quote: u64,
    pub created_at: i64,
    pub expiry_timestamp: i64,
    pub bump: u8,
}

impl Order {
    pub const LEN: usize = 8 + 32 + 32 + 8 + 1 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 1;
}

// -----------------------------------------------------------------------
// INSTRUCTION ACCOUNTS
// -----------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeBook<'info> {
    #[account(
        init,
        payer = authority,
        space = Book::LEN,
        seeds = [BOOK_SEED, base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump
    )]
    pub book: Box<Account<'info, Book>>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    /// This program's `ProgramData` account; the signer must be its upgrade
    /// authority.
    /// CHECK: address and owner are pinned to this program's `ProgramData` PDA
    /// under the upgradeable loader, and the signer is matched against the
    /// authority it records, by `require_upgrade_authority`.
    pub program_data: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(order_id: u64)]
pub struct CreateOrder<'info> {
    #[account(mut)]
    pub maker: Signer<'info>,

    #[account(
        seeds = [BOOK_SEED, base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump = book.bump,
        constraint = book.mint_base == base_mint.key() @ ErrorCode::WrongBook,
        constraint = book.mint_quote == quote_mint.key() @ ErrorCode::WrongBook,
    )]
    pub book: Box<Account<'info, Book>>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    /// Closed by `cancel_order`, which returns its rent to the maker.
    #[account(
        init,
        payer = maker,
        space = Order::LEN,
        seeds = [ORDER_SEED, book.key().as_ref(), maker.key().as_ref(), &order_id.to_le_bytes()],
        bump
    )]
    pub order: Box<Account<'info, Order>>,

    /// The escrow's mint: base for an ask, quote for a bid. The handler checks
    /// it against the side.
    pub escrow_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The order's escrow: one ATA per order, owned by the order PDA.
    #[account(
        init_if_needed,
        payer = maker,
        associated_token::mint = escrow_mint,
        associated_token::authority = order,
        associated_token::token_program = token_program,
    )]
    pub escrow_token: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        token::mint = escrow_mint,
        token::authority = maker,
    )]
    pub maker_token: Box<InterfaceAccount<'info, TokenAccount>>,

    /// KYC config, required iff the book has one. Pass this program's ID as the
    /// `Option` sentinel when the rail is off.
    /// CHECK: address-pinned to the book's recorded config in the handler.
    pub kyc_config: Option<UncheckedAccount<'info>>,

    /// The maker's soulbound attestation ATA. Required iff the KYC rail is on.
    /// CHECK: verified by the KYC gate CPI.
    pub user_kyc_account: Option<UncheckedAccount<'info>>,

    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    // Remaining accounts: the transfer-hook accounts for the escrow mint.
}

#[derive(Accounts)]
pub struct FillOrder<'info> {
    #[account(mut)]
    pub taker: Signer<'info>,

    /// The maker named by the order. Read-only; used as the settlement vault's
    /// beneficiary (the T+N release is bound to this key).
    /// CHECK: constrained to `order.owner` below.
    pub maker: UncheckedAccount<'info>,

    #[account(
        seeds = [BOOK_SEED, base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump = book.bump,
        constraint = book.mint_base == base_mint.key() @ ErrorCode::WrongBook,
        constraint = book.mint_quote == quote_mint.key() @ ErrorCode::WrongBook,
    )]
    pub book: Box<Account<'info, Book>>,

    #[account(
        mut,
        seeds = [ORDER_SEED, book.key().as_ref(), maker.key().as_ref(), &order.order_id.to_le_bytes()],
        bump = order.bump,
        has_one = book @ ErrorCode::WrongBook,
        constraint = order.owner == maker.key() @ ErrorCode::WrongMaker,
    )]
    pub order: Box<Account<'info, Order>>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The order's escrow mint: base for an ask, quote for a bid.
    pub escrow_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        token::mint = escrow_mint,
        token::authority = order,
    )]
    pub escrow_token: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, token::mint = base_mint, token::authority = taker)]
    pub taker_base: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut, token::mint = quote_mint, token::authority = taker)]
    pub taker_quote: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Where the maker's proceeds go: the maker's ATA when the settlement rail
    /// is off, or the hooks authority's custody ATA when it is on. The mint is
    /// checked against the order's side in the handler.
    #[account(mut)]
    pub proceeds_token: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Hooks authority PDA, required iff the book's settlement rail is on
    /// (pass this program's ID when it is off). It pays for the settlement
    /// vault, so it must be writable here.
    /// CHECK: seeds-verified as the hooks program's `[b"hook_authority"]` PDA.
    #[account(mut)]
    pub hooks_authority: Option<UncheckedAccount<'info>>,

    /// The settlement clearinghouse config, required iff the rail is on.
    /// CHECK: address-pinned to `[b"config"]` in the handler and re-validated by
    /// the settlement program.
    pub settlement_config: UncheckedAccount<'info>,

    /// The settlement program, required iff the rail is on.
    /// CHECK: pinned to the settlement program ID in the handler.
    pub settlement_program: UncheckedAccount<'info>,

    /// The per-fill settlement vault, required iff the rail is on.
    /// CHECK: address-pinned to `[b"settlement", trade_id]` in the handler.
    #[account(mut)]
    pub settlement_vault: Option<UncheckedAccount<'info>>,

    /// The per-fill settlement escrow token account, required iff the rail is
    /// on. The settlement program creates it during the hooks CPI.
    /// CHECK: address-pinned to `[b"escrow", trade_id]` in the handler.
    #[account(mut)]
    pub settlement_vault_token: Option<UncheckedAccount<'info>>,

    /// The associated-token program. Part of the instruction layout; pinned to
    /// this program's ID when the rail is off.
    /// CHECK: pinned to this program's ID when the settlement rail is off.
    pub associated_token_program: UncheckedAccount<'info>,

    /// The hooks program, required iff the rail is on (it locks the proceeds).
    pub hooks_program: Option<Program<'info, hooks::program::Hooks>>,

    /// KYC config, required iff the book has one.
    /// CHECK: address-pinned to the book's recorded config in the handler.
    pub kyc_config: Option<UncheckedAccount<'info>>,

    /// The taker's soulbound attestation ATA, required iff the KYC rail is on.
    /// CHECK: verified by the KYC gate CPI.
    pub user_kyc_account: Option<UncheckedAccount<'info>>,

    pub kyc_program: Option<Program<'info, kyc_gate::program::KycGate>>,

    /// The token program of the base mint, and of the quote mint. They may
    /// differ (Token-2022 RWA base, legacy quote); each leg's transfer uses the
    /// program that owns its mint.
    pub base_token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
    // Remaining accounts: the transfer-hook accounts for both mints.
}

#[derive(Accounts)]
pub struct CancelOrder<'info> {
    #[account(mut)]
    pub maker: Signer<'info>,

    #[account(
        seeds = [BOOK_SEED, base_mint.key().as_ref(), quote_mint.key().as_ref()],
        bump = book.bump,
        constraint = book.mint_base == base_mint.key() @ ErrorCode::WrongBook,
        constraint = book.mint_quote == quote_mint.key() @ ErrorCode::WrongBook,
    )]
    pub book: Box<Account<'info, Book>>,

    #[account(
        mut,
        close = maker,
        seeds = [ORDER_SEED, book.key().as_ref(), maker.key().as_ref(), &order.order_id.to_le_bytes()],
        bump = order.bump,
        has_one = book @ ErrorCode::WrongBook,
        constraint = order.owner == maker.key() @ ErrorCode::Unauthorized,
    )]
    pub order: Box<Account<'info, Order>>,

    pub base_mint: Box<InterfaceAccount<'info, Mint>>,
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The order's escrow mint: base for an ask, quote for a bid.
    pub escrow_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        constraint = escrow_token.key() == get_associated_token_address_with_program_id(
            &order.key(),
            &escrow_mint.key(),
            &token_program.key(),
        ) @ ErrorCode::InvalidEscrowAccount,
        token::mint = escrow_mint,
        token::authority = order,
    )]
    pub escrow_token: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        token::mint = escrow_mint,
        token::authority = maker,
    )]
    pub maker_token: Box<InterfaceAccount<'info, TokenAccount>>,

    pub token_program: Interface<'info, TokenInterface>,
    // Remaining accounts: the transfer-hook accounts for the escrow mint.
}

#[derive(Accounts)]
pub struct Admin<'info> {
    #[account(
        mut,
        seeds = [BOOK_SEED, book.mint_base.as_ref(), book.mint_quote.as_ref()],
        bump = book.bump,
        has_one = authority @ ErrorCode::Unauthorized,
    )]
    pub book: Box<Account<'info, Book>>,

    pub authority: Signer<'info>,
}

// -----------------------------------------------------------------------
// RAILS
// -----------------------------------------------------------------------

/// KYC rail: verify `trader` against the book's config before value moves.
///
/// When the rail is off every KYC account must be the program-ID sentinel, so
/// a caller cannot smuggle a made-up config into a book that never opted in.
fn kyc_gate_check<'info>(
    book: &Book,
    kyc_config: &Option<UncheckedAccount<'info>>,
    user_kyc_account: &Option<UncheckedAccount<'info>>,
    kyc_program: &Option<Program<'info, kyc_gate::program::KycGate>>,
    trader: &Signer<'info>,
) -> Result<()> {
    if book.kyc_config == Pubkey::default() {
        require!(
            kyc_config.is_none() && user_kyc_account.is_none() && kyc_program.is_none(),
            ErrorCode::RailAccountUnexpected
        );
        return Ok(());
    }

    let kyc_config = kyc_config
        .as_ref()
        .ok_or(ErrorCode::RailAccountUnexpected)?;
    require_keys_eq!(kyc_config.key(), book.kyc_config, ErrorCode::WrongKycConfig);
    let (expected, _) = Pubkey::find_program_address(&[b"kyc-config"], &kyc_gate::ID);
    require_keys_eq!(kyc_config.key(), expected, ErrorCode::WrongKycConfig);
    let cpi_accounts = kyc_gate::cpi::accounts::VerifyKyc {
        kyc_config: kyc_config.to_account_info(),
        user: trader.to_account_info(),
        user_kyc_account: user_kyc_account
            .as_ref()
            .ok_or(ErrorCode::RailAccountUnexpected)?
            .to_account_info(),
    };
    kyc_gate::cpi::verify_kyc(CpiContext::new(kyc_gate::ID, cpi_accounts))
}

/// Settlement rail: routes `amount` of the payment mint, already sitting in the
/// hooks authority's custody ATA, into the T+N escrow for `trade_id`.
///
/// The beneficiary recorded in the vault is the maker, so the Transfer Agent's
/// release signature binds to the party that placed the order. The hook
/// accounts are forwarded to the settlement transfer as well, because a
/// hook-bearing RWA mint resolves its meta list on that transfer too.
#[allow(clippy::too_many_arguments)]
fn settle_proceeds<'info>(
    hooks_authority: &UncheckedAccount<'info>,
    maker: &UncheckedAccount<'info>,
    custody_token_account: &InterfaceAccount<'info, TokenAccount>,
    payment_mint: &InterfaceAccount<'info, Mint>,
    settlement_vault: &UncheckedAccount<'info>,
    settlement_vault_token: &UncheckedAccount<'info>,
    settlement_config: &UncheckedAccount<'info>,
    settlement_program: &UncheckedAccount<'info>,
    system_program: &Program<'info, System>,
    token_program: &Interface<'info, TokenInterface>,
    hooks: &[AccountInfo<'info>],
    trade_id: u64,
    settlement_slots: u64,
    amount: u64,
) -> Result<()> {
    require_keys_eq!(
        custody_token_account.key(),
        get_associated_token_address_with_program_id(
            &hooks_authority.key(),
            &payment_mint.key(),
            &token_program.key(),
        ),
        ErrorCode::InvalidCustodyAccount
    );

    let cpi_accounts = hooks::cpi::accounts::AfterSwap {
        hook_authority: hooks_authority.to_account_info(),
        taker: maker.to_account_info(),
        amm_vault: custody_token_account.to_account_info(),
        token_mint: payment_mint.to_account_info(),
        settlement_vault: settlement_vault.to_account_info(),
        settlement_vault_token: settlement_vault_token.to_account_info(),
        settlement_config: settlement_config.to_account_info(),
        settlement_program: settlement_program.to_account_info(),
        token_program: token_program.to_account_info(),
        system_program: system_program.to_account_info(),
    };
    let cpi_ctx = CpiContext::new(hooks::ID, cpi_accounts).with_remaining_accounts(hooks.to_vec());
    hooks::cpi::after_swap(cpi_ctx, trade_id, settlement_slots, amount)
}

// -----------------------------------------------------------------------
// TRANSFERS
// -----------------------------------------------------------------------

/// Transfers `amount` into `to` and reports what `to` actually gained, because
/// a Token-2022 transfer fee can skim part of a transfer in flight.
fn transfer_and_measure<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &mut InterfaceAccount<'info, TokenAccount>,
    authority: &AccountInfo<'info>,
    hooks: &[AccountInfo<'info>],
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<u64> {
    if amount == 0 {
        return Ok(0);
    }

    let before = to.amount;
    transfer_checked(
        token_program,
        from,
        mint,
        &to.to_account_info(),
        authority,
        hooks,
        signer_seeds,
        amount,
    )?;
    to.reload()?;
    to.amount
        .checked_sub(before)
        .ok_or_else(|| error!(ErrorCode::MathOverflow))
}

/// `transfer_checked` for legacy SPL Token or Token-2022, carrying the mint's
/// transfer-hook accounts when it has them.
///
/// The hand-built instruction is needed because Anchor's `transfer_checked` CPI
/// ignores `remaining_accounts`, while Token-2022 resolves a hook mint's
/// `ExtraAccountMetaList` against the instruction's own account list.
fn transfer_checked<'info>(
    token_program: &Interface<'info, TokenInterface>,
    from: &InterfaceAccount<'info, TokenAccount>,
    mint: &InterfaceAccount<'info, Mint>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    hooks: &[AccountInfo<'info>],
    signer_seeds: &[&[&[u8]]],
    amount: u64,
) -> Result<()> {
    if token_program.key() == token_interface::ID && !hooks.is_empty() {
        let mut ix = token_interface::spl_token_2022::instruction::transfer_checked(
            token_program.key,
            &from.key(),
            &mint.key(),
            &to.key(),
            &authority.key(),
            &[],
            amount,
            mint.decimals,
        )?;
        ix.accounts.extend(hooks.iter().map(|account| {
            if account.is_writable {
                AccountMeta::new(*account.key, false)
            } else {
                AccountMeta::new_readonly(*account.key, false)
            }
        }));

        let mut infos: Vec<AccountInfo<'info>> = vec![
            from.to_account_info(),
            mint.to_account_info(),
            to.clone(),
            authority.clone(),
        ];
        infos.extend_from_slice(hooks);

        invoke_signed(&ix, &infos, signer_seeds)?;
        return Ok(());
    }

    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            token_program.key(),
            TransferChecked {
                from: from.to_account_info(),
                mint: mint.to_account_info(),
                to: to.clone(),
                authority: authority.clone(),
            },
            signer_seeds,
        ),
        amount,
        mint.decimals,
    )
}

/// Closes an order's escrow account, signed by the order PDA, and returns its
/// rent to `destination`.
fn close_token_account<'info>(
    token_program: &Interface<'info, TokenInterface>,
    account: &InterfaceAccount<'info, TokenAccount>,
    destination: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    invoke_signed(
        &token_interface::spl_token_2022::instruction::close_account(
            token_program.key,
            &account.key(),
            &destination.key(),
            &authority.key(),
            &[],
        )?,
        &[
            account.to_account_info(),
            destination.clone(),
            authority.clone(),
        ],
        signer_seeds,
    )?;
    Ok(())
}

// -----------------------------------------------------------------------
// PRICE MATH
// -----------------------------------------------------------------------

/// Quote owed for `base` raw units, rounded **up** so the maker is never short.
fn quote_for_base(base: u64, price: u64) -> Result<u64> {
    let numerator = (base as u128)
        .checked_mul(price as u128)
        .ok_or(ErrorCode::MathOverflow)?;
    let quote = numerator
        .checked_add(PRICE_SCALE - 1)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(PRICE_SCALE)
        .ok_or(ErrorCode::MathOverflow)?;
    u64::try_from(quote).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Base bought for `quote` raw units, rounded **down** so the maker is never
/// short.
fn base_for_quote(quote: u64, price: u64) -> Result<u64> {
    let base = (quote as u128)
        .checked_mul(PRICE_SCALE)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(price as u128)
        .ok_or(ErrorCode::MathOverflow)?;
    u64::try_from(base).map_err(|_| error!(ErrorCode::MathOverflow))
}

/// Resolves a fill's two legs: the amount exchanged in base and in quote.
///
/// `amount` is always in the order's escrow token. An ask sells `amount` base;
/// a bid spends `amount` quote. Both legs are required to be non-zero, so a
/// price that rounds a leg to dust is rejected rather than silently giving away
/// value.
fn fill_legs(side: Side, amount: u64, price: u64) -> Result<(u64, u64)> {
    require!(amount > 0, ErrorCode::ZeroAmount);
    require!(price > 0, ErrorCode::ZeroPrice);

    let (base, quote) = match side {
        Side::Ask => (amount, quote_for_base(amount, price)?),
        Side::Bid => (base_for_quote(amount, price)?, amount),
    };
    require!(base > 0 && quote > 0, ErrorCode::ZeroFillLeg);
    Ok((base, quote))
}

// -----------------------------------------------------------------------
// ERRORS
// -----------------------------------------------------------------------

#[error_code]
pub enum ErrorCode {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Price must be greater than zero")]
    ZeroPrice,
    #[msg("The fill rounds one leg to zero")]
    ZeroFillLeg,
    #[msg("The order book is paused")]
    BookPaused,
    #[msg("Account does not belong to this owner")]
    Unauthorized,
    #[msg("Only the program's upgrade authority may initialize a book")]
    UnauthorizedInitializer,
    #[msg("The base and quote mints must be different")]
    IdenticalMints,
    #[msg("Account does not belong to this book")]
    WrongBook,
    #[msg("The order account does not match its maker")]
    WrongMaker,
    #[msg("The escrow mint does not match the order's side")]
    WrongEscrowMint,
    #[msg("The escrow token account is not the order's ATA for this mint")]
    InvalidEscrowAccount,
    #[msg("The proceeds account is not the maker's ATA for this mint")]
    InvalidProceedsAccount,
    #[msg("The order is already expired")]
    OrderAlreadyExpired,
    #[msg("The order has expired")]
    OrderExpired,
    #[msg("The order does not have that much left to fill")]
    InsufficientOrderAmount,
    #[msg("The fill would pay more than the taker allowed")]
    SlippageExceeded,
    #[msg("A settlement trade id must be non-zero")]
    InvalidTradeId,
    #[msg(
        "Settlement routing policy is inconsistent: hooks authority and slots must be set together"
    )]
    InvalidSettlementSlots,
    #[msg("KYC config account does not match the book's recorded policy")]
    WrongKycConfig,
    #[msg("Hooks authority account is not the hooks program's hook_authority PDA")]
    WrongHooksAuthority,
    #[msg("Settlement vault is not the expected PDA for this trade id")]
    WrongSettlementVault,
    #[msg("Settlement config is not the canonical clearinghouse config")]
    WrongSettlementConfig,
    #[msg("Settlement program account is not the settlement program")]
    WrongSettlementProgram,
    #[msg("Settlement custody account is not the hook authority's ATA for this mint")]
    InvalidCustodyAccount,
    #[msg("A rail account must be this program's own ID when its rail is disabled")]
    RailAccountUnexpected,
    #[msg("The new authority cannot be the default pubkey")]
    InvalidAuthority,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_legs_round_up() {
        // 3 base at 1.5 quote each = 4.5 quote, and the maker must receive 5.
        assert_eq!(fill_legs(Side::Ask, 3, 1_500_000_000).unwrap(), (3, 5));
        // Exact prices do not acquire a stray unit.
        assert_eq!(
            fill_legs(Side::Ask, 1_000, 2_000_000_000).unwrap(),
            (1_000, 2_000)
        );
    }

    #[test]
    fn bid_legs_round_down() {
        // 5 quote at 2 quote per base buys 2.5 base, rounded down to 2.
        assert_eq!(fill_legs(Side::Bid, 5, 2_000_000_000).unwrap(), (2, 5));
        assert_eq!(
            fill_legs(Side::Bid, 2_000, 2_000_000_000).unwrap(),
            (1_000, 2_000)
        );
    }

    #[test]
    fn fills_reject_dust_and_zero_inputs() {
        assert!(fill_legs(Side::Ask, 0, PRICE_SCALE as u64).is_err());
        assert!(fill_legs(Side::Ask, 1, 0).is_err());
        // One quote at an enormous price buys less than one base unit.
        assert!(fill_legs(Side::Bid, 1, 2_000_000_000_000).is_err());
    }

    #[test]
    fn price_math_does_not_overflow() {
        assert_eq!(quote_for_base(1_000_000, 1_000_000_000).unwrap(), 1_000_000);
        // The intermediate fits in u128, but the result no longer fits in u64:
        // the conversion fails instead of wrapping.
        assert!(quote_for_base(u64::MAX, u64::MAX).is_err());
        assert!(base_for_quote(u64::MAX, 1).is_err());
        // A tiny price still yields a valid quote for a base unit.
        assert_eq!(quote_for_base(PRICE_SCALE as u64, 1).unwrap(), 1);
    }

    #[test]
    fn price_scale_is_one_billion() {
        // The frontend and the docs quote prices as `human_price * 1e9`.
        assert_eq!(PRICE_SCALE, 1_000_000_000);
        assert_eq!(quote_for_base(10, 1_000_000_000).unwrap(), 10);
    }
}
