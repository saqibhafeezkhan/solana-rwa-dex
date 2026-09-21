//! End-to-end tests: the compiled program, on a LiteSVM, against real SPL token
//! accounts.
//!
//! The unit tests inside the crate exercise the pure math. These exercise what
//! no unit test can reach — the instruction handlers, the account wiring and the
//! state a signed transaction leaves behind. They pin down, in particular:
//!
//! * `add_liquidity` records the position's owner and range, and `remove_liquidity`
//!   really pays out afterwards. (A position whose `owner` was never written could
//!   never be withdrawn from, and nothing short of a full transaction notices.)
//! * the pool's tick bitmap tracks initialization: set by a deposit, cleared once
//!   the tick's last position leaves.
//! * a swap walk that skips an initialized tick, or starts on the wrong side of
//!   the price, is rejected instead of silently trading against stale liquidity.
//! * a swap that crosses two ticks leaves `tick_current`/`liquidity` where the
//!   bitmap says they should be, fees reach the LP that earned them, and an LP can
//!   collect them and withdraw.
//!
//! They need the SBF build, which `cargo test` does not produce:
//!
//! ```bash
//! anchor build -p clmm && (cd tests/e2e && cargo test)
//! ```

// Instruction builders take the account list in the program's order; grouping
// them into structs would only rename the arguments.
#![allow(clippy::too_many_arguments)]

use {
    litesvm::LiteSVM,
    solana_account::Account,
    solana_address::{address, Address},
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::Transaction,
};

/// The program under test: this must match `declare_id!` in
/// `programs/clmm/src/lib.rs` (the client-side strings are the only place the
/// ID is repeated here, because these tests never link the program crate).
const PROGRAM_ID: Address = address!("HLMAwimHtgMV8ME9X4RaoHCAdfU35hrovEg4AU5ifw7u");
const TOKEN_PROGRAM_ID: Address = address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// Anchor instruction discriminators (`sha256("global:<name>")[..8]`), taken from
/// `target/idl/clmm.json` (a wrong one fails the first transaction loudly).
const IX_INITIALIZE_POOL: [u8; 8] = [95, 180, 10, 172, 84, 174, 232, 40];
const IX_ADD_LIQUIDITY: [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];
const IX_REMOVE_LIQUIDITY: [u8; 8] = [80, 85, 209, 72, 24, 206, 177, 108];
const IX_COLLECT_FEES: [u8; 8] = [164, 152, 207, 99, 30, 186, 19, 182];
const IX_SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// SPL mint layout (82 bytes): authority, supply, decimals, initialized flag.
const MINT_DECIMALS: usize = 44;
const MINT_INITIALIZED: usize = 45;

/// Program errors this test asserts on, in the enum's order.
const ERR_INSUFFICIENT_SHARES: u32 = 6009;
const ERR_TICKS_NOT_IN_ORDER: u32 = 6013;
const ERR_TICK_SPACING_TOO_FINE: u32 = 6015;

const SPACING: i32 = 60;
const FEE_PIPS: u32 = 3_000;
const MIN_TICK: i32 = -443_636;

/// The two ranges this test uses: one around the start price, one directly below
/// it, so a single A→B swap has to cross two ticks.
const LOWER: i32 = -120;
const UPPER: i32 = 120;
const DEEP: i32 = -240;

const LIQUIDITY: u128 = 1_000_000_000;
const FUNDS: u64 = 1_000_000_000_000;
const LAMPORTS: u64 = 10_000_000_000;

/// Token account layout (SPL): mint, owner, amount.
const TOKEN_AMOUNT_OFFSET: usize = 64;
/// Pool layout: discriminator, mints, sqrt price, liquidity, tick_current,
/// tick_spacing, fee_pips, two growth accumulators, bump, bitmap cells, then the
/// Borsh length prefix and bytes of the bitmap.
const POOL_TICK_CURRENT: usize = 104;
const POOL_LIQUIDITY: usize = 88;
const POOL_BITMAP_CELLS: usize = 149;
const POOL_BITMAP_LEN: usize = 153;
const POOL_BITMAP: usize = 157;
/// Position layout: discriminator, pool, owner, lower, upper, liquidity, two fee
/// snapshots, then the two banked fee amounts.
const POSITION_OWNER: usize = 40;
const POSITION_LIQUIDITY: usize = 80;
const POSITION_OWED_A: usize = 128;

// -----------------------------------------------------------------------
// Test harness
// -----------------------------------------------------------------------

fn send(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) {
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &[payer],
        svm.latest_blockhash(),
    );
    if let Err(failure) = svm.send_transaction(tx) {
        panic!(
            "transaction failed: {:?}\nlogs:\n{}",
            failure.err,
            failure.meta.logs.join("\n")
        );
    }
}

/// Sends a transaction that must fail, and returns the program error code.
fn send_error(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) -> u32 {
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &[payer],
        svm.latest_blockhash(),
    );
    match svm.send_transaction(tx) {
        Ok(_) => panic!("expected the transaction to fail"),
        Err(failure) => {
            let text = format!("{:?}", failure.err);
            for code in 6000..6100 {
                if text.contains(&format!("Custom({code})")) {
                    return code;
                }
            }
            panic!("no program error code in {text}\nlogs:\n{}", failure.meta.logs.join("\n"));
        }
    }
}

fn data_of(svm: &LiteSVM, address: Address) -> Vec<u8> {
    svm.get_account(&address)
        .unwrap_or_else(|| panic!("account {address} does not exist"))
        .data
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn u128_at(data: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap())
}

fn i32_at(data: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn address_at(data: &[u8], offset: usize) -> Address {
    Address::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

fn set_raw_account(svm: &mut LiteSVM, address: Address, data: Vec<u8>, owner: Address) {
    svm.set_account(
        address,
        Account {
            lamports: LAMPORTS,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn token_balance(svm: &LiteSVM, address: Address) -> u64 {
    u64_at(&data_of(svm, address), TOKEN_AMOUNT_OFFSET)
}

/// Writes an initialized mint. The data is packed by hand rather than by an
/// `initialize_mint` instruction, so the setup works whichever SPL token
/// implementation the VM has loaded (newer LiteSVM builds swap in a trimmed
/// token program that rejects some initialize tags).
fn create_mint(svm: &mut LiteSVM, authority: Address, mint: Address) {
    let mut data = vec![0u8; 82];
    data[..4].copy_from_slice(&1u32.to_le_bytes()); // COption::Some(authority)
    data[4..36].copy_from_slice(authority.as_ref());
    data[MINT_DECIMALS] = 6;
    data[MINT_INITIALIZED] = 1;
    set_raw_account(svm, mint, data, TOKEN_PROGRAM_ID);
}

/// Writes a token account owned by `owner` holding `amount`.
fn create_token_account(
    svm: &mut LiteSVM,
    account: Address,
    mint: Address,
    owner: Address,
    amount: u64,
) {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // AccountState::Initialized
    set_raw_account(svm, account, data, TOKEN_PROGRAM_ID);
}

// -----------------------------------------------------------------------
// PDAs
// -----------------------------------------------------------------------

fn pool_pda(mint_a: Address, mint_b: Address) -> Address {
    Address::find_program_address(
        &[b"clmm-pool", mint_a.as_ref(), mint_b.as_ref()],
        &PROGRAM_ID,
    )
    .0
}

fn vault_pda(prefix: &[u8], pool: Address, mint: Address) -> Address {
    Address::find_program_address(&[prefix, pool.as_ref(), mint.as_ref()], &PROGRAM_ID).0
}

fn tick_pda(pool: Address, tick: i32) -> Address {
    Address::find_program_address(&[b"clmm-tick", pool.as_ref(), &tick.to_le_bytes()], &PROGRAM_ID).0
}

fn position_pda(pool: Address, owner: Address, lower: i32, upper: i32) -> Address {
    Address::find_program_address(
        &[
            b"clmm-pos",
            pool.as_ref(),
            owner.as_ref(),
            &lower.to_le_bytes(),
            &upper.to_le_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

/// The bit a tick owns, mirroring `bit_of` in the program: its cell
/// `floor(tick / spacing)` minus the cell holding `MIN_TICK`.
fn bit_of(tick: i32, spacing: i32) -> i64 {
    (tick as i64).div_euclid(spacing as i64)
        - i64::from(MIN_TICK).div_euclid(spacing as i64)
}

fn bitmap_bit(svm: &LiteSVM, pool: Address, tick: i32) -> bool {
    let data = data_of(svm, pool);
    let bit = bit_of(tick, SPACING) as usize;
    data[POOL_BITMAP + bit / 8] & (1 << (bit % 8)) != 0
}

// -----------------------------------------------------------------------
// Instructions
// -----------------------------------------------------------------------

struct Instructions;

impl Instructions {
    fn initialize_pool(
        payer: Address,
        mint_a: Address,
        mint_b: Address,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_INITIALIZE_POOL.to_vec();
        data.extend_from_slice(&(1u128 << 64).to_le_bytes()); // price 1.0
        data.extend_from_slice(&SPACING.to_le_bytes());
        data.extend_from_slice(&FEE_PIPS.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn add_liquidity(
        payer: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        lower: i32,
        upper: i32,
        liquidity: u128,
    ) -> Instruction {
        let mut data = IX_ADD_LIQUIDITY.to_vec();
        data.extend_from_slice(&lower.to_le_bytes());
        data.extend_from_slice(&upper.to_le_bytes());
        data.extend_from_slice(&liquidity.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(tick_pda(pool, lower), false),
                AccountMeta::new(tick_pda(pool, upper), false),
                AccountMeta::new(position_pda(pool, payer, lower, upper), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn swap(
        payer: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        amount_in: u64,
        is_a_to_b: bool,
        walk: &[i32],
    ) -> Instruction {
        let mut data = IX_SWAP.to_vec();
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); // min_amount_out
        data.push(u8::from(is_a_to_b));
        let mut accounts = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
            AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
            AccountMeta::new(user_token_a, false),
            AccountMeta::new(user_token_b, false),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ];
        accounts.extend(walk.iter().map(|tick| AccountMeta::new(tick_pda(pool, *tick), false)));
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
            data,
        }
    }

    fn collect_fees(
        payer: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        lower: i32,
        upper: i32,
    ) -> Instruction {
        let mut data = IX_COLLECT_FEES.to_vec();
        data.extend_from_slice(&lower.to_le_bytes());
        data.extend_from_slice(&upper.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(position_pda(pool, payer, lower, upper), false),
                AccountMeta::new_readonly(tick_pda(pool, lower), false),
                AccountMeta::new_readonly(tick_pda(pool, upper), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn remove_liquidity(
        payer: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        lower: i32,
        upper: i32,
        liquidity: u128,
    ) -> Instruction {
        let mut data = IX_REMOVE_LIQUIDITY.to_vec();
        data.extend_from_slice(&lower.to_le_bytes());
        data.extend_from_slice(&upper.to_le_bytes());
        data.extend_from_slice(&liquidity.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); // min_amount_a
        data.extend_from_slice(&0u64.to_le_bytes()); // min_amount_b
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(position_pda(pool, payer, lower, upper), false),
                AccountMeta::new(tick_pda(pool, lower), false),
                AccountMeta::new(tick_pda(pool, upper), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            ],
            data,
        }
    }
}

/// A provisioned pool: the payer's two token accounts hold `FUNDS` each, the pool
/// is initialized with two positions in it, and nothing has been traded yet.
struct Fixture {
    svm: LiteSVM,
    payer: Keypair,
    mint_a: Address,
    mint_b: Address,
    user_token_a: Address,
    user_token_b: Address,
    pool: Address,
}

fn setup() -> Fixture {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    let program = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/clmm.so"
    ))
    .expect("build the program first: `anchor build -p clmm`");
    svm.add_program(PROGRAM_ID, &program).unwrap();

    let payer_address = payer.pubkey();
    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    let user_token_a = Keypair::new().pubkey();
    let user_token_b = Keypair::new().pubkey();
    create_mint(&mut svm, payer_address, mint_a);
    create_mint(&mut svm, payer_address, mint_b);
    create_token_account(&mut svm, user_token_a, mint_a, payer_address, FUNDS);
    create_token_account(&mut svm, user_token_b, mint_b, payer_address, FUNDS);

    let pool = pool_pda(mint_a, mint_b);
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool(payer_address, mint_a, mint_b)],
    );

    let mut fixture = Fixture {
        svm,
        payer,
        mint_a,
        mint_b,
        user_token_a,
        user_token_b,
        pool,
    };

    // A range around the start price, and one directly below it: crossing the
    // first hands the trade to the second.
    for (lower, upper) in [(LOWER, UPPER), (DEEP, LOWER)] {
        let instruction = Instructions::add_liquidity(
            fixture.payer.pubkey(),
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            lower,
            upper,
            LIQUIDITY,
        );
        send(&mut fixture.svm, &fixture.payer, &[instruction]);
    }
    fixture
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn the_pool_is_created_with_a_bitmap_sized_to_its_spacing() {
    let fixture = setup();
    let pool = data_of(&fixture.svm, fixture.pool);
    assert_eq!(i32_at(&pool, POOL_TICK_CURRENT), 0, "price starts at tick 0");
    assert_eq!(
        u128_at(&pool, POOL_LIQUIDITY),
        LIQUIDITY,
        "only the range spanning the price is active"
    );
    let cells = u32::from_le_bytes(pool[POOL_BITMAP_CELLS..POOL_BITMAP_CELLS + 4].try_into().unwrap());
    let bytes = u32::from_le_bytes(pool[POOL_BITMAP_LEN..POOL_BITMAP_LEN + 4].try_into().unwrap());
    assert_eq!(cells, 14_788);
    assert_eq!(bytes, 1_849);
    assert_eq!(pool.len(), POOL_BITMAP + 1_849);
}

#[test]
fn a_deposit_records_its_owner_and_marks_both_ticks() {
    let fixture = setup();
    // The position the deposit created belongs to the depositor and records the
    // range — this is what makes it withdrawable at all.
    let position = data_of(&fixture.svm, position_pda(fixture.pool, fixture.payer.pubkey(), LOWER, UPPER));
    assert_eq!(address_at(&position, POSITION_OWNER), fixture.payer.pubkey());
    assert_eq!(u128_at(&position, POSITION_LIQUIDITY), LIQUIDITY);

    // Both bounds are in the bitmap, and a tick nobody used is not.
    assert!(bitmap_bit(&fixture.svm, fixture.pool, LOWER));
    assert!(bitmap_bit(&fixture.svm, fixture.pool, UPPER));
    assert!(bitmap_bit(&fixture.svm, fixture.pool, DEEP));
    assert!(!bitmap_bit(&fixture.svm, fixture.pool, 180));
}

#[test]
fn a_fine_tick_spacing_is_rejected() {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    let program = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/clmm.so"
    ))
    .expect("build the program first: `anchor build -p clmm`");
    svm.add_program(PROGRAM_ID, &program).unwrap();

    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    create_mint(&mut svm, payer.pubkey(), mint_a);
    create_mint(&mut svm, payer.pubkey(), mint_b);

    let pool = pool_pda(mint_a, mint_b);
    let mut data = IX_INITIALIZE_POOL.to_vec();
    data.extend_from_slice(&(1u128 << 64).to_le_bytes());
    data.extend_from_slice(&13i32.to_le_bytes()); // one finer than the floor
    data.extend_from_slice(&FEE_PIPS.to_le_bytes());
    let instruction = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
            AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    assert_eq!(
        send_error(&mut svm, &payer, &[instruction]),
        ERR_TICK_SPACING_TOO_FINE
    );
}

#[test]
fn the_walk_must_be_the_one_the_bitmap_expects() {
    let mut fixture = setup();
    let payer = fixture.payer.pubkey();
    let swap = |walk: &[i32], is_a_to_b: bool| {
        Instructions::swap(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            100_000_000,
            is_a_to_b,
            walk,
        )
    };

    // Skipping the tick the price meets first (LOWER) is rejected, so a caller
    // cannot stop `pool.liquidity` from being updated.
    assert_eq!(
        send_error(&mut fixture.svm, &fixture.payer, &[swap(&[DEEP], true)]),
        ERR_TICKS_NOT_IN_ORDER
    );
    // So is a tick on the wrong side of the price for the direction.
    assert_eq!(
        send_error(&mut fixture.svm, &fixture.payer, &[swap(&[UPPER], true)]),
        ERR_TICKS_NOT_IN_ORDER
    );
    // And one supplied after the walk has ended.
    assert_eq!(
        send_error(&mut fixture.svm, &fixture.payer, &[swap(&[LOWER, UPPER], true)]),
        ERR_TICKS_NOT_IN_ORDER
    );

    // The walk the bitmap describes is accepted, and trades through both ticks.
    let before = token_balance(&fixture.svm, fixture.user_token_b);
    send(&mut fixture.svm, &fixture.payer, &[swap(&[LOWER, DEEP], true)]);
    let pool = data_of(&fixture.svm, fixture.pool);
    assert_eq!(
        i32_at(&pool, POOL_TICK_CURRENT),
        DEEP - 1,
        "a down-cross records the tick below the boundary it crossed"
    );
    assert_eq!(
        u128_at(&pool, POOL_LIQUIDITY),
        0,
        "both ranges were crossed out, so the price stops moving"
    );
    let after = token_balance(&fixture.svm, fixture.user_token_b);
    assert!(after > before, "the swap paid out token B");
    assert!(
        FUNDS - token_balance(&fixture.svm, fixture.user_token_a) < 100_000_000,
        "only the consumed input is pulled from the trader"
    );
}

#[test]
fn an_lp_earns_fees_and_can_withdraw() {
    let mut fixture = setup();
    let payer = fixture.payer.pubkey();
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            100_000_000,
            true,
            &[LOWER, DEEP],
        )],
    );

    // The A-side fees of the swap belong to the range that was active. A swap
    // only moves the pool's accumulators — nothing touches the position — so the
    // claim is where they settle into the position and out to the LP.
    let position_address = position_pda(fixture.pool, payer, LOWER, UPPER);
    let fees_before = token_balance(&fixture.svm, fixture.user_token_a);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::collect_fees(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            LOWER,
            UPPER,
        )],
    );
    let earned = token_balance(&fixture.svm, fixture.user_token_a) - fees_before;
    // Around 0.3% of the ~12M actually swapped: fees, not principal.
    assert!(earned > 0, "the active range earned the swap fee");
    assert!(earned < 100_000, "and only the fee, not the input: {earned}");
    assert_eq!(
        u64_at(&data_of(&fixture.svm, position_address), POSITION_OWED_A),
        0,
        "and the claim leaves nothing owed"
    );

    // The price is now below the range, so the position is entirely token A.
    let a_before = token_balance(&fixture.svm, fixture.user_token_a);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::remove_liquidity(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            LOWER,
            UPPER,
            LIQUIDITY,
        )],
    );
    let a_after = token_balance(&fixture.svm, fixture.user_token_a);
    assert!(a_after > a_before, "the withdrawal paid out its token A");
    assert_eq!(
        u128_at(&data_of(&fixture.svm, position_address), POSITION_LIQUIDITY),
        0
    );

    // UPPER had no other position on it, so its bit is cleared; LOWER still backs
    // the range below it and stays.
    assert!(
        !bitmap_bit(&fixture.svm, fixture.pool, UPPER),
        "a tick with no liquidity left stops being initialized"
    );
    assert!(bitmap_bit(&fixture.svm, fixture.pool, LOWER));

    // Withdrawing once more is refused: the position is empty. (A different
    // amount, so the transaction is not a replay of the one above.)
    assert_eq!(
        send_error(
            &mut fixture.svm,
            &fixture.payer,
            &[Instructions::remove_liquidity(
                payer,
                fixture.pool,
                fixture.mint_a,
                fixture.mint_b,
                fixture.user_token_a,
                fixture.user_token_b,
                LOWER,
                UPPER,
                LIQUIDITY / 2,
            )],
        ),
        ERR_INSUFFICIENT_SHARES
    );
}
