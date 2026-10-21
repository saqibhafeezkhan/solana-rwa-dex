//! End-to-end tests: the compiled `order_book` program on a LiteSVM.
//!
//! These exercise the parts unit tests cannot: Anchor's PDA/ATA constraints,
//! the escrow transfer signed by the order PDA, partial fills against real token
//! accounts, and the close path that returns the maker's rent. The settlement
//! rail is exercised by the AMM suites (it is the same `hooks::after_swap`
//! composition), so these tests run with the rail off and focus on the order
//! lifecycle itself.
//!
//! ```bash
//! anchor build -p order_book && (cd tests/e2e && cargo test --test order_book)
//! ```

mod common;

use common::*;
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};

/// The program under test; must match `declare_id!`.
const PROGRAM_ID: Address = address!("3ACX2Lc67uZW65PA3QbYRLttmxE766eSDyD9Sd2yggwD");

/// Anchor instruction discriminators, from `target/idl/order_book.json`.
const IX_INITIALIZE_BOOK: [u8; 8] = [229, 238, 30, 198, 244, 0, 186, 239];
const IX_CREATE_ORDER: [u8; 8] = [141, 54, 37, 207, 237, 210, 250, 215];
const IX_FILL_ORDER: [u8; 8] = [232, 122, 115, 25, 199, 143, 136, 162];
const IX_CANCEL_ORDER: [u8; 8] = [95, 129, 237, 240, 8, 49, 223, 132];
const IX_SET_PAUSED: [u8; 8] = [91, 60, 125, 192, 176, 225, 166, 218];

/// The settlement rail's program and instructions.
const HOOKS_PROGRAM_ID: Address = address!("B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA");
const SETTLEMENT_PROGRAM_ID: Address =
    address!("58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6");
const IX_SETTLEMENT_INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
const IX_SET_HOOKS_AUTHORITY: [u8; 8] = [44, 238, 216, 211, 196, 234, 134, 241];
const IX_SET_TRANSFER_AGENT: [u8; 8] = [35, 191, 232, 217, 214, 140, 220, 143];
const IX_SETTLE: [u8; 8] = [175, 42, 185, 87, 144, 131, 102, 212];

/// `SettlementVault` layout: trade id, taker, mint, amount, release slot,
/// expiry slot, status, pool, refund source, bump.
const VAULT_AMOUNT: usize = 80;
const VAULT_SETTLEMENT_SLOT: usize = 88;
const VAULT_STATUS: usize = 104;
const STATUS_SETTLED: u8 = 1;

/// Program errors this suite asserts on, in the enum's order.
const ERR_ZERO_AMOUNT: u32 = 6001;
const ERR_BOOK_PAUSED: u32 = 6004;
const ERR_ORDER_EXPIRED: u32 = 6014;
const ERR_INSUFFICIENT_ORDER_AMOUNT: u32 = 6015;
const ERR_SLIPPAGE_EXCEEDED: u32 = 6016;
const ERR_WRONG_ESCROW_MINT: u32 = 6010;

/// One quote raw unit per base raw unit, scaled by 1e9.
const PRICE_2: u64 = 2_000_000_000;
/// `Order` layout: discriminator, book, owner, id, side, price, original,
/// remaining, filled_base, filled_quote, created_at, expiry, bump.
const ORDER_REMAINING: usize = 97;

fn book_pda(base: Address, quote: Address) -> Address {
    Address::find_program_address(&[b"book", base.as_ref(), quote.as_ref()], &PROGRAM_ID).0
}

fn order_pda(book: Address, maker: Address, order_id: u64) -> Address {
    Address::find_program_address(
        &[b"order", book.as_ref(), maker.as_ref(), &order_id.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

fn keypair() -> Keypair {
    Keypair::new()
}

fn mint_at(svm: &mut LiteSVM, authority: Address) -> Address {
    let mint = keypair().pubkey();
    create_mint(svm, authority, mint, 6);
    mint
}

fn token_account(svm: &mut LiteSVM, mint: Address, owner: Address, amount: u64) -> Address {
    // The program requires the maker's proceeds account to be its canonical
    // ATA, so every fixture account is created at its ATA address.
    let account = associated_token_address(owner, mint);
    create_token_account(svm, account, mint, owner, amount);
    account
}

// -----------------------------------------------------------------------
// Instruction builders
// -----------------------------------------------------------------------

fn initialize_book(svm: &mut LiteSVM, payer: &Keypair, base: Address, quote: Address) {
    let book = book_pda(base, quote);
    let mut data = IX_INITIALIZE_BOOK.to_vec();
    data.extend_from_slice(&[0u8; 32]); // kyc_config: rail off
    data.extend_from_slice(&[0u8; 32]); // hooks_authority: rail off
    data.extend_from_slice(&0u64.to_le_bytes()); // settlement_slots

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(book, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(base, false),
            AccountMeta::new_readonly(quote, false),
            AccountMeta::new_readonly(program_data_pda(PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    send(svm, payer, &[ix]);
}

fn create_order(
    svm: &mut LiteSVM,
    maker: &Keypair,
    base: Address,
    quote: Address,
    order_id: u64,
    is_ask: bool,
    amount: u64,
    expiry: i64,
    maker_token: Address,
) {
    let book = book_pda(base, quote);
    let order = order_pda(book, maker.pubkey(), order_id);
    let escrow_mint = if is_ask { base } else { quote };
    let escrow = associated_token_address(order, escrow_mint);

    // The ATA would normally be created by the `init_if_needed` constraint via
    // the associated-token program. The VM fixture writes token accounts by
    // hand, so create it up front; Anchor then validates it in place.
    create_token_account(svm, escrow, escrow_mint, order, 0);

    let mut data = IX_CREATE_ORDER.to_vec();
    data.extend_from_slice(&order_id.to_le_bytes());
    data.push(if is_ask { 0 } else { 1 }); // Side
    data.extend_from_slice(&PRICE_2.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&expiry.to_le_bytes());

    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(maker.pubkey(), true),
            AccountMeta::new_readonly(book, false),
            AccountMeta::new_readonly(base, false),
            AccountMeta::new_readonly(quote, false),
            AccountMeta::new(order, false),
            AccountMeta::new_readonly(escrow_mint, false),
            AccountMeta::new(escrow, false),
            AccountMeta::new(maker_token, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    send(svm, maker, &[ix]);
}

#[allow(clippy::too_many_arguments)]
fn fill_order_ix(
    base: Address,
    quote: Address,
    maker: Address,
    order_id: u64,
    taker: Address,
    taker_base: Address,
    taker_quote: Address,
    proceeds: Address,
    is_ask: bool,
    amount: u64,
    max_counter: u64,
) -> Instruction {
    let book = book_pda(base, quote);
    let order = order_pda(book, maker, order_id);
    let escrow_mint = if is_ask { base } else { quote };
    let escrow = associated_token_address(order, escrow_mint);

    let mut data = IX_FILL_ORDER.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_counter.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // trade_id: rail off

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(taker, true),
            AccountMeta::new_readonly(maker, false),
            AccountMeta::new_readonly(book, false),
            AccountMeta::new(order, false),
            AccountMeta::new_readonly(base, false),
            AccountMeta::new_readonly(quote, false),
            AccountMeta::new_readonly(escrow_mint, false),
            AccountMeta::new(escrow, false),
            AccountMeta::new(taker_base, false),
            AccountMeta::new(taker_quote, false),
            AccountMeta::new(proceeds, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

fn cancel_order_ix(
    base: Address,
    quote: Address,
    maker: Address,
    order_id: u64,
    is_ask: bool,
) -> Instruction {
    let book = book_pda(base, quote);
    let order = order_pda(book, maker, order_id);
    let escrow_mint = if is_ask { base } else { quote };
    let escrow = associated_token_address(order, escrow_mint);
    let maker_token = associated_token_address(maker, escrow_mint);

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(maker, true),
            AccountMeta::new_readonly(book, false),
            AccountMeta::new(order, false),
            AccountMeta::new_readonly(base, false),
            AccountMeta::new_readonly(quote, false),
            AccountMeta::new_readonly(escrow_mint, false),
            AccountMeta::new(escrow, false),
            AccountMeta::new(maker_token, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: IX_CANCEL_ORDER.to_vec(),
    }
}

// -----------------------------------------------------------------------
// Settlement rail
// -----------------------------------------------------------------------

fn settlement_config() -> Address {
    Address::find_program_address(&[b"config"], &SETTLEMENT_PROGRAM_ID).0
}

fn settlement_escrow(trade_id: u64) -> Address {
    Address::find_program_address(
        &[b"escrow", &trade_id.to_le_bytes()],
        &SETTLEMENT_PROGRAM_ID,
    )
    .0
}

fn settlement_escrow_authority() -> Address {
    Address::find_program_address(&[b"escrow_authority"], &SETTLEMENT_PROGRAM_ID).0
}

fn settlement_initialize_ix(authority: Address) -> Instruction {
    let mut data = IX_SETTLEMENT_INITIALIZE.to_vec();
    data.push(1); // pools_require_gate: the hooks authority path is the one used
    Instruction {
        program_id: SETTLEMENT_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(settlement_config(), false),
            AccountMeta::new(authority, true),
            AccountMeta::new_readonly(program_data_pda(SETTLEMENT_PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

fn set_hooks_authority_ix(authority: Address) -> Instruction {
    let mut data = IX_SET_HOOKS_AUTHORITY.to_vec();
    data.extend_from_slice(hook_authority_pda().as_ref());
    Instruction {
        program_id: SETTLEMENT_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(settlement_config(), false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

fn set_transfer_agent_ix(authority: Address, agent: [u8; 64]) -> Instruction {
    let mut data = IX_SET_TRANSFER_AGENT.to_vec();
    data.extend_from_slice(&agent);
    Instruction {
        program_id: SETTLEMENT_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(settlement_config(), false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

fn initialize_book_wired(
    svm: &mut LiteSVM,
    payer: &Keypair,
    base: Address,
    quote: Address,
    settlement_slots: u64,
) {
    let mut data = IX_INITIALIZE_BOOK.to_vec();
    data.extend_from_slice(&[0u8; 32]); // KYC off
    data.extend_from_slice(hook_authority_pda().as_ref());
    data.extend_from_slice(&settlement_slots.to_le_bytes());
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(book_pda(base, quote), false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(base, false),
            AccountMeta::new_readonly(quote, false),
            AccountMeta::new_readonly(program_data_pda(PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    send(svm, payer, &[ix]);
}

#[allow(clippy::too_many_arguments)]
fn fill_order_wired_ix(
    fixture: &RailWorld,
    amount: u64,
    max_counter: u64,
    trade_id: u64,
) -> Instruction {
    let book = book_pda(fixture.base, fixture.quote);
    let order = order_pda(book, fixture.maker.pubkey(), 21);
    let escrow = associated_token_address(order, fixture.base);
    let vault = settlement_vault_pda(trade_id);
    let vault_token = settlement_escrow(trade_id);

    let mut data = IX_FILL_ORDER.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_counter.to_le_bytes());
    data.extend_from_slice(&trade_id.to_le_bytes());

    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(fixture.taker.pubkey(), true),
            AccountMeta::new_readonly(fixture.maker.pubkey(), false),
            AccountMeta::new_readonly(book, false),
            AccountMeta::new(order, false),
            AccountMeta::new_readonly(fixture.base, false),
            AccountMeta::new_readonly(fixture.quote, false),
            AccountMeta::new_readonly(fixture.base, false), // escrow mint (ask)
            AccountMeta::new(escrow, false),
            AccountMeta::new(fixture.taker_base, false),
            AccountMeta::new(fixture.taker_quote, false),
            AccountMeta::new(fixture.custody, false),
            AccountMeta::new(hook_authority_pda(), false),
            AccountMeta::new_readonly(settlement_config(), false),
            AccountMeta::new_readonly(SETTLEMENT_PROGRAM_ID, false),
            AccountMeta::new(vault, false),
            AccountMeta::new(vault_token, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(HOOKS_PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

// -----------------------------------------------------------------------
// Fixture
// -----------------------------------------------------------------------

struct World {
    svm: LiteSVM,
    payer: Keypair,
    base: Address,
    quote: Address,
    maker: Keypair,
    taker: Keypair,
    maker_base: Address,
    maker_quote: Address,
    taker_base: Address,
    taker_quote: Address,
    expiry: i64,
}

fn world() -> World {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "order_book");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    set_upgrade_authority(&mut svm, PROGRAM_ID, payer.pubkey());

    let base = mint_at(&mut svm, payer.pubkey());
    let quote = mint_at(&mut svm, payer.pubkey());
    let maker = keypair();
    let taker = keypair();
    svm.airdrop(&maker.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&taker.pubkey(), 10_000_000_000).unwrap();

    let maker_base = token_account(&mut svm, base, maker.pubkey(), 1_000_000);
    let maker_quote = token_account(&mut svm, quote, maker.pubkey(), 1_000_000);
    let taker_base = token_account(&mut svm, base, taker.pubkey(), 1_000_000);
    let taker_quote = token_account(&mut svm, quote, taker.pubkey(), 1_000_000);

    initialize_book(&mut svm, &payer, base, quote);
    let expiry = now(&svm) + 10_000;

    World {
        svm,
        payer,
        base,
        quote,
        maker,
        taker,
        maker_base,
        maker_quote,
        taker_base,
        taker_quote,
        expiry,
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn ask_fills_partially_then_completely() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);
    let order = order_pda(book, w.maker.pubkey(), 7);
    let escrow = associated_token_address(order, w.base);

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        7,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    // The escrow holds the maker's base; its own base account was debited.
    assert_eq!(token_balance(&w.svm, escrow), 1_000);
    assert_eq!(token_balance(&w.svm, w.maker_base), 999_000);

    // First fill: half the order, at 2 quote per base.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        7,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        400,
        800,
    );
    send(&mut w.svm, &w.taker, &[ix]);

    assert_eq!(token_balance(&w.svm, escrow), 600);
    assert_eq!(
        u64_at(&data_of(&w.svm, order), ORDER_REMAINING),
        600,
        "order must record the partial fill"
    );
    assert_eq!(token_balance(&w.svm, w.taker_base), 1_000_400);
    assert_eq!(token_balance(&w.svm, w.taker_quote), 999_200);
    assert_eq!(token_balance(&w.svm, w.maker_quote), 1_000_800);

    // Second fill completes it.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        7,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        600,
        1_200,
    );
    send(&mut w.svm, &w.taker, &[ix]);

    assert_eq!(token_balance(&w.svm, escrow), 0);
    assert_eq!(u64_at(&data_of(&w.svm, order), ORDER_REMAINING), 0);
    assert_eq!(token_balance(&w.svm, w.taker_base), 1_001_000);
    assert_eq!(token_balance(&w.svm, w.taker_quote), 998_000);
    assert_eq!(token_balance(&w.svm, w.maker_quote), 1_002_000);

    // Cleaning up a filled order closes both accounts and refunds the rent.
    let ix = cancel_order_ix(w.base, w.quote, w.maker.pubkey(), 7, true);
    send(&mut w.svm, &w.maker, &[ix]);
    assert!(!exists(&w.svm, order));
    assert!(!exists(&w.svm, escrow));
}

#[test]
fn bid_pays_base_and_receives_quote() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);
    let order = order_pda(book, w.maker.pubkey(), 9);
    let escrow = associated_token_address(order, w.quote);

    // A bid escrows quote: 2,000 quote at 2 quote/base buys up to 1,000 base.
    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        9,
        false,
        2_000,
        w.expiry,
        w.maker_quote,
    );
    assert_eq!(token_balance(&w.svm, escrow), 2_000);
    assert_eq!(token_balance(&w.svm, w.maker_quote), 998_000);

    // The taker sells 500 base into it and receives 1,000 quote.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        9,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_base, // proceeds are base for a bid
        false,
        1_000,
        500,
    );
    send(&mut w.svm, &w.taker, &[ix]);

    assert_eq!(token_balance(&w.svm, escrow), 1_000);
    assert_eq!(u64_at(&data_of(&w.svm, order), ORDER_REMAINING), 1_000);
    // Taker sold 500 base for 1,000 quote.
    assert_eq!(token_balance(&w.svm, w.taker_base), 999_500);
    assert_eq!(token_balance(&w.svm, w.taker_quote), 1_001_000);
    // Maker received the base it was bidding for.
    assert_eq!(token_balance(&w.svm, w.maker_base), 1_000_500);
}

#[test]
fn cancel_refunds_unfilled_escrow_and_closes() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);
    let order = order_pda(book, w.maker.pubkey(), 3);
    let escrow = associated_token_address(order, w.base);

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        3,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    // Fill 250, then cancel: 750 base must come back and both accounts close.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        3,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        250,
        500,
    );
    send(&mut w.svm, &w.taker, &[ix]);

    let ix = cancel_order_ix(w.base, w.quote, w.maker.pubkey(), 3, true);
    send(&mut w.svm, &w.maker, &[ix]);

    assert_eq!(token_balance(&w.svm, w.maker_base), 999_750);
    assert!(!exists(&w.svm, order));
    assert!(!exists(&w.svm, escrow));
}

#[test]
fn donation_to_the_escrow_does_not_block_cancel() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);
    let order = order_pda(book, w.maker.pubkey(), 5);
    let escrow = associated_token_address(order, w.base);

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        5,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    // Fill 250, then donate one raw base unit to the order's publicly
    // derivable escrow. The token program refuses to close an account with a
    // non-zero balance, so refunding only `remaining_amount` would revert here
    // and lock the maker's 750 base forever.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        5,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        250,
        500,
    );
    send(&mut w.svm, &w.taker, &[ix]);

    let mut escrow_data = data_of(&w.svm, escrow);
    let dusted = u64_at(&escrow_data, TOKEN_AMOUNT_OFFSET) + 1;
    escrow_data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8]
        .copy_from_slice(&dusted.to_le_bytes());
    set_raw_account(&mut w.svm, escrow, escrow_data, TOKEN_PROGRAM_ID);

    // Cancel sweeps the whole escrow: 750 unfilled + 1 donated.
    let ix = cancel_order_ix(w.base, w.quote, w.maker.pubkey(), 5, true);
    send(&mut w.svm, &w.maker, &[ix]);

    assert_eq!(token_balance(&w.svm, w.maker_base), 999_751);
    assert!(!exists(&w.svm, order));
    assert!(!exists(&w.svm, escrow));
}

#[test]
fn expired_order_cannot_fill_but_can_be_cancelled() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);
    let order = order_pda(book, w.maker.pubkey(), 11);

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        11,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    set_time(&mut w.svm, w.expiry + 1);
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        11,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        100,
        200,
    );
    assert_eq!(send_error(&mut w.svm, &w.taker, &[ix]), ERR_ORDER_EXPIRED);

    // The exit stays open after expiry.
    let ix = cancel_order_ix(w.base, w.quote, w.maker.pubkey(), 11, true);
    send(&mut w.svm, &w.maker, &[ix]);
    assert!(!exists(&w.svm, order));
}

#[test]
fn fill_guards_reject_bad_sizes_and_slippage() {
    let mut w = world();

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        13,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    // Over-fill.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        13,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        1_001,
        2_002,
    );
    assert_eq!(
        send_error(&mut w.svm, &w.taker, &[ix]),
        ERR_INSUFFICIENT_ORDER_AMOUNT
    );

    // The price is 200 quote for 100 base; 199 is not enough.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        13,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        100,
        199,
    );
    assert_eq!(send_error(&mut w.svm, &w.taker, &[ix]), ERR_SLIPPAGE_EXCEEDED);

    // Zero is not a fill.
    let ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        13,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        0,
        0,
    );
    assert_eq!(send_error(&mut w.svm, &w.taker, &[ix]), ERR_ZERO_AMOUNT);
}

#[test]
fn fill_rejects_the_other_mints_escrow() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        17,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    // An ask escrows base. Point the fill at the order's *quote* ATA instead:
    // the `token::mint` constraint accepts it (the ATA and the mint agree), so
    // the guard has to refuse it by side rather than letting the token program
    // fail later on a mint mismatch or paying the taker from the wrong leg.
    let order = order_pda(book, w.maker.pubkey(), 17);
    let other_ata = associated_token_address(order, w.quote);
    create_token_account(&mut w.svm, other_ata, w.quote, order, 0);

    let mut ix = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        17,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        100,
        200,
    );
    ix.accounts[6].pubkey = w.quote;
    ix.accounts[7].pubkey = other_ata;

    assert_eq!(
        send_error(&mut w.svm, &w.taker, &[ix]),
        ERR_WRONG_ESCROW_MINT
    );
}

#[test]
fn paused_book_refuses_orders_and_fills_but_allows_cancel() {
    let mut w = world();
    let book = book_pda(w.base, w.quote);
    let order = order_pda(book, w.maker.pubkey(), 15);

    create_order(
        &mut w.svm,
        &w.maker,
        w.base,
        w.quote,
        15,
        true,
        1_000,
        w.expiry,
        w.maker_base,
    );

    let mut data = IX_SET_PAUSED.to_vec();
    data.push(1);
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(book, false),
            AccountMeta::new_readonly(w.payer.pubkey(), true),
        ],
        data,
    };
    send(&mut w.svm, &w.payer, &[ix]);

    let fill = fill_order_ix(
        w.base,
        w.quote,
        w.maker.pubkey(),
        15,
        w.taker.pubkey(),
        w.taker_base,
        w.taker_quote,
        w.maker_quote,
        true,
        100,
        200,
    );
    assert_eq!(send_error(&mut w.svm, &w.taker, &[fill]), ERR_BOOK_PAUSED);

    // Cancelling is an exit and stays open.
    let ix = cancel_order_ix(w.base, w.quote, w.maker.pubkey(), 15, true);
    send(&mut w.svm, &w.maker, &[ix]);
    assert!(!exists(&w.svm, order));
}

// -----------------------------------------------------------------------
// Settlement-routed fills
// -----------------------------------------------------------------------

struct RailWorld {
    svm: LiteSVM,
    payer: Keypair,
    maker: Keypair,
    taker: Keypair,
    base: Address,
    quote: Address,
    maker_base: Address,
    maker_quote: Address,
    taker_base: Address,
    taker_quote: Address,
    /// The hook authority's ATA for the payment mint (quote, for an ask).
    custody: Address,
}

fn rail_world() -> RailWorld {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "order_book");
    load_program(&mut svm, HOOKS_PROGRAM_ID, "hooks");
    load_program(&mut svm, SETTLEMENT_PROGRAM_ID, "settlement");

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    set_upgrade_authority(&mut svm, PROGRAM_ID, payer.pubkey());
    set_upgrade_authority(&mut svm, SETTLEMENT_PROGRAM_ID, payer.pubkey());

    // A gated clearinghouse admits the hook authority, which is the signer of
    // the `after_swap` CPI this test drives.
    send(
        &mut svm,
        &payer,
        &[
            settlement_initialize_ix(payer.pubkey()),
            set_hooks_authority_ix(payer.pubkey()),
        ],
    );
    // The hook authority pays the rent of the vault and its escrow.
    svm.airdrop(&hook_authority_pda(), LAMPORTS).unwrap();

    let base = mint_at(&mut svm, payer.pubkey());
    let quote = mint_at(&mut svm, payer.pubkey());
    let maker = keypair();
    let taker = keypair();
    svm.airdrop(&maker.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&taker.pubkey(), 10_000_000_000).unwrap();

    let maker_base = token_account(&mut svm, base, maker.pubkey(), 1_000_000);
    let maker_quote = token_account(&mut svm, quote, maker.pubkey(), 1_000_000);
    let taker_base = token_account(&mut svm, base, taker.pubkey(), 1_000_000);
    let taker_quote = token_account(&mut svm, quote, taker.pubkey(), 1_000_000);

    initialize_book_wired(&mut svm, &payer, base, quote, 100);
    let expiry = now(&svm) + 10_000;

    // The payment mint's custody account has to exist before the fill pays
    // into it; the settlement program creates the vault and its escrow.
    let custody = associated_token_address(hook_authority_pda(), quote);
    create_token_account(&mut svm, custody, quote, hook_authority_pda(), 0);
    for address in [settlement_vault_pda(1), settlement_escrow(1)] {
        svm.set_account(
            address,
            Account {
                lamports: 0,
                data: vec![],
                owner: SYSTEM_PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    }

    create_order(
        &mut svm,
        &maker,
        base,
        quote,
        21,
        true,
        1_000,
        expiry,
        maker_base,
    );

    RailWorld {
        svm,
        payer,
        maker,
        taker,
        base,
        quote,
        maker_base,
        maker_quote,
        taker_base,
        taker_quote,
        custody,
    }
}

fn settle_ix(w: &RailWorld, signature: [u8; 64], recovery_id: u8) -> Instruction {
    let mut data = IX_SETTLE.to_vec();
    data.extend_from_slice(&signature);
    data.push(recovery_id);
    Instruction {
        program_id: SETTLEMENT_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(settlement_vault_pda(1), false),
            AccountMeta::new_readonly(settlement_config(), false),
            AccountMeta::new_readonly(SETTLEMENT_PROGRAM_ID, false), // no mint override
            AccountMeta::new_readonly(w.quote, false),
            AccountMeta::new(settlement_escrow(1), false),
            AccountMeta::new_readonly(settlement_escrow_authority(), false),
            AccountMeta::new(w.maker_quote, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

#[test]
fn routed_fill_accepts_any_nonzero_trade_id() {
    let mut w = rail_world();

    // The fixture only seeds vault 1, and the book has no counter any more: a
    // caller names its own id, so a fill is never blocked by another fill's id
    // (or by a griefer taking "the next one").
    let fill = fill_order_wired_ix(&w, 100, 200, 2);
    send(&mut w.svm, &w.taker, &[fill]);

    let vault = data_of(&w.svm, settlement_vault_pda(2));
    assert_eq!(u64_at(&vault, VAULT_AMOUNT), 200);
    assert_eq!(token_balance(&w.svm, settlement_escrow(2)), 200);
    assert_eq!(token_balance(&w.svm, w.custody), 0, "the custody account was swept");

    let order = order_pda(book_pda(w.base, w.quote), w.maker.pubkey(), 21);
    assert_eq!(u64_at(&data_of(&w.svm, order), ORDER_REMAINING), 900);
}

#[test]
fn settlement_routed_fill_escrows_then_releases_with_an_agent_signature() {
    let mut w = rail_world();

    // The Transfer Agent key the release will be checked against.
    let signing = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let encoded = signing.verifying_key().to_encoded_point(false);
    let mut agent = [0u8; 64];
    agent.copy_from_slice(&encoded.as_bytes()[1..]);
    send(
        &mut w.svm,
        &w.payer,
        &[set_transfer_agent_ix(w.payer.pubkey(), agent)],
    );

    // 100 base at 2 quote/base: the taker pays 200 quote, which is escrowed
    // for the maker rather than paid to it.
    let fill = fill_order_wired_ix(&w, 100, 200, 1);
    send(&mut w.svm, &w.taker, &[fill]);

    let vault = data_of(&w.svm, settlement_vault_pda(1));
    assert_eq!(u64_at(&vault, VAULT_AMOUNT), 200, "the fill's proceeds are escrowed");
    assert_eq!(vault[VAULT_STATUS], 0, "the vault starts pending");
    assert_eq!(token_balance(&w.svm, settlement_escrow(1)), 200);
    assert_eq!(token_balance(&w.svm, w.custody), 0, "the custody account was swept");
    assert_eq!(
        token_balance(&w.svm, w.maker_quote),
        1_000_000,
        "nothing is paid to the maker before release"
    );
    assert_eq!(token_balance(&w.svm, w.taker_base), 1_000_100);
    assert_eq!(token_balance(&w.svm, w.taker_quote), 999_800);

    // Move the clock to the T+N slot the vault recorded, then sign the release
    // preimage the same way the Transfer Agent does: keccak256(trade_id ‖
    // taker ‖ mint ‖ amount), with the maker as the recorded taker.
    let settlement_slot = u64_at(&vault, VAULT_SETTLEMENT_SLOT);
    let mut clock = w.svm.get_sysvar::<Clock>();
    clock.slot = settlement_slot;
    w.svm.set_sysvar(&clock);

    let amount = token_balance(&w.svm, settlement_escrow(1));
    let mut message = Vec::new();
    message.extend_from_slice(&1u64.to_le_bytes());
    message.extend_from_slice(w.maker.pubkey().as_ref());
    message.extend_from_slice(w.quote.as_ref());
    message.extend_from_slice(&amount.to_le_bytes());
    let digest = Keccak256::digest(&message);
    let (signature, recovery_id) = signing.sign_prehash_recoverable(digest.as_slice()).unwrap();
    let mut signature_bytes = [0u8; 64];
    signature_bytes.copy_from_slice(&signature.to_bytes());

    let release = settle_ix(&w, signature_bytes, recovery_id.to_byte());
    send(&mut w.svm, &w.payer, &[release]);

    assert_eq!(token_balance(&w.svm, w.maker_quote), 1_000_200);
    assert_eq!(token_balance(&w.svm, settlement_escrow(1)), 0);
    assert_eq!(
        data_of(&w.svm, settlement_vault_pda(1))[VAULT_STATUS],
        STATUS_SETTLED
    );
    assert_eq!(token_balance(&w.svm, w.maker_base), 999_000);
}
