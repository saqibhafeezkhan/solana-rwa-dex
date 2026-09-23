//! End-to-end tests: the compiled oracle program, on a LiteSVM, against real
//! transactions and the real `Clock` sysvar.
//!
//! The unit tests inside the program crate exercise the pure accumulator math.
//! These exercise what no unit test can reach — the Anchor account wiring, the
//! PDA derivation, the Borsh layout the chain actually writes, and the
//! circuit-breaker lifecycle across signed transactions. They pin down, in
//! particular:
//!
//! * `initialize` writes a genesis observation and the PDA the seeds describe;
//!   `record_observation` really advances the accumulator and the ring cursor,
//!   and only the pool signer may call it.
//! * TWAP over a window is `Δcumulative / Δtime`, interpolated across real
//!   observations recorded at different times — values only a full VM
//!   (borsh serialization, return data, clock control) can confirm.
//! * the breaker guard (`check_deviation`) reverts a price far from the
//!   30-minute TWAP; because a reverted instruction cannot persist state, the
//!   *latch* is a separate permissionless `trip_breaker` whose transaction
//!   succeeds — after which every check is refused until the owner unpause.
//! * a recording that moves time backwards is rejected.
//! * the ring buffer wraps at 100 observations and keeps serving TWAPs from
//!   the window it still holds.
//!
//! They need the SBF build, which `cargo test` does not produce:
//!
//! ```bash
//! anchor build -p oracle && (cd tests/e2e && cargo test --test oracle)
//! ```

use {
    litesvm::LiteSVM,
    solana_address::{address, Address},
    solana_clock::Clock,
    solana_hash::Hash as SolanaHash,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::Transaction,
};

/// The program under test: this must match `declare_id!` in
/// `programs/oracle/src/lib.rs`.
const PROGRAM_ID: Address = address!("4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// Anchor instruction discriminators (`sha256("global:<name>")[..8]`), taken
/// from `target/idl/oracle.json` (a wrong one fails the first transaction
/// loudly).
const IX_INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
const IX_RECORD_OBSERVATION: [u8; 8] = [37, 148, 41, 216, 83, 104, 162, 96];
const IX_GET_TWAP: [u8; 8] = [110, 181, 179, 141, 85, 10, 37, 120];
const IX_CHECK_DEVIATION: [u8; 8] = [122, 253, 206, 161, 253, 111, 194, 213];
const IX_TRIP_BREAKER: [u8; 8] = [141, 130, 124, 235, 77, 100, 183, 10];
const IX_UNPAUSE_POOL: [u8; 8] = [241, 148, 129, 243, 222, 125, 125, 160];

/// Program errors this test asserts on, in the enum's order.
const ERR_UNAUTHORIZED_POOL: u32 = 6000;
const ERR_UNAUTHORIZED: u32 = 6001;
const ERR_POOL_PAUSED: u32 = 6002;
const ERR_PRICE_DEVIATION_EXCEEDED: u32 = 6003;
const ERR_TIMESTAMP_MOVED_BACKWARDS: u32 = 6006;
const ERR_INVALID_THRESHOLD: u32 = 6008;
const ERR_PRICE_WITHIN_THRESHOLD: u32 = 6009;

/// Oracle state layout (borsh): discriminator, pool, owner, is_paused,
/// observation_index, observation_count, bump, max_deviation_bps, then the
/// observation array.
const STATE_POOL: usize = 8;
const STATE_IS_PAUSED: usize = 72;
const STATE_INDEX: usize = 73;
const STATE_COUNT: usize = 74;
const STATE_THRESHOLD: usize = 76;
const STATE_OBSERVATIONS: usize = 78;
/// Observation layout: timestamp (i64), sqrt_price (u128), cumulative (u128).
const OBS_SIZE: usize = 40;

const LAMPORTS: u64 = 10_000_000_000;
/// The Q64.64 sqrt-price of 1.0.
const PRICE_1: u128 = 1u128 << 64;
const PRICE_10: u128 = 10u128 << 64;
/// Half of the true-price sqrt-bps threshold: sqrt(1.1) ≈ 1.0488, so a 10%
/// true-price move is ~4.88% in sqrt-price.
const MAX_DEVIATION_BPS: u16 = 488;

/// The clock LiteSVM boots with (its genesis blockhash is from 2020-ish, but
/// the tests only ever move *relative* to whatever they read).
fn now(svm: &LiteSVM) -> i64 {
    svm.get_sysvar::<Clock>().unix_timestamp
}

fn set_time(svm: &mut LiteSVM, timestamp: i64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp = timestamp;
    svm.set_sysvar::<Clock>(&clock);
}

// -----------------------------------------------------------------------
// Test harness
// -----------------------------------------------------------------------

/// LiteSVM's blockhash is static, so identical transactions would collide on
/// signature (`AlreadyProcessed`). Expiring it before each send chains a new
/// hash, giving every transaction a distinct, valid blockhash.
fn fresh_blockhash(svm: &mut LiteSVM) -> SolanaHash {
    svm.expire_blockhash();
    svm.latest_blockhash()
}

fn send(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) {
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &[payer],
        fresh_blockhash(svm),
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
    send_error_signed(svm, payer, &[], instructions)
}

/// Like `send_error`, but with extra signers (e.g. a fresh pool keypair that
/// must sign `initialize`).
fn send_error_signed(
    svm: &mut LiteSVM,
    payer: &Keypair,
    extra_signers: &[&Keypair],
    instructions: &[Instruction],
) -> u32 {
    let mut signers: Vec<&Keypair> = vec![payer];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &signers,
        fresh_blockhash(svm),
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
            panic!(
                "no program error code in {text}\nlogs:\n{}",
                failure.meta.logs.join("\n")
            );
        }
    }
}

/// Sends and returns the transaction metadata (for `return_data`).
fn send_return<'a>(
    svm: &'a mut LiteSVM,
    payer: &Keypair,
    instructions: &[Instruction],
) -> litesvm::types::TransactionMetadata {
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &[payer],
        fresh_blockhash(svm),
    );
    svm.send_transaction(tx).unwrap_or_else(|failure| {
        panic!(
            "transaction failed: {:?}\nlogs:\n{}",
            failure.err,
            failure.meta.logs.join("\n")
        )
    })
}

fn data_of(svm: &LiteSVM, address: Address) -> Vec<u8> {
    svm.get_account(&address)
        .unwrap_or_else(|| panic!("account {address} does not exist"))
        .data
}

fn u128_at(data: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap())
}

fn i64_at(data: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn observation_at(data: &[u8], slot: usize) -> (i64, u128, u128) {
    let base = STATE_OBSERVATIONS + slot * OBS_SIZE;
    (
        i64_at(data, base),
        u128_at(data, base + 8),
        u128_at(data, base + 24),
    )
}

// -----------------------------------------------------------------------
// PDAs
// -----------------------------------------------------------------------

fn oracle_pda(pool: Address) -> Address {
    Address::find_program_address(&[b"oracle", pool.as_ref()], &PROGRAM_ID).0
}

// -----------------------------------------------------------------------
// Instructions
// -----------------------------------------------------------------------

struct Instructions;

impl Instructions {
    /// The pool account must sign (the pool PDA signs via `invoke_signed` in
    /// production). The tests use the payer as the "pool" for the
    /// signature-delegation cases; `owner` is the key allowed to unpause.
    fn initialize(payer: Address, pool: Address, max_deviation_bps: u16, owner: Address) -> Instruction {
        let mut data = IX_INITIALIZE.to_vec();
        data.extend_from_slice(&max_deviation_bps.to_le_bytes());
        data.extend_from_slice(owner.as_ref());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(oracle_pda(pool), false),
                AccountMeta::new_readonly(pool, true),
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn record_observation(pool: Address, signer: Address, sqrt_price_x64: u128) -> Instruction {
        let mut data = IX_RECORD_OBSERVATION.to_vec();
        data.extend_from_slice(&sqrt_price_x64.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(oracle_pda(pool), false),
                AccountMeta::new_readonly(signer, true),
            ],
            data,
        }
    }

    fn get_twap(pool: Address, seconds_ago: u32) -> Instruction {
        let mut data = IX_GET_TWAP.to_vec();
        data.extend_from_slice(&seconds_ago.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![AccountMeta::new_readonly(oracle_pda(pool), false)],
            data,
        }
    }

    /// The breaker guard, run by the pool the oracle belongs to. `check_deviation`
    /// binds the check to that pool, which must sign.
    fn check_deviation(pool: Address, current_sqrt_price_x64: u128) -> Instruction {
        Instructions::check_deviation_signed_by(pool, pool, current_sqrt_price_x64)
    }

    /// The same instruction signed by someone other than the pool, used to show
    /// that the oracle only answers the pool that owns it.
    fn check_deviation_signed_by(
        pool: Address,
        signer: Address,
        current_sqrt_price_x64: u128,
    ) -> Instruction {
        let mut data = IX_CHECK_DEVIATION.to_vec();
        data.extend_from_slice(&current_sqrt_price_x64.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(oracle_pda(pool), false),
                AccountMeta::new_readonly(signer, true),
            ],
            data,
        }
    }

    fn trip_breaker(pool: Address, current_sqrt_price_x64: u128) -> Instruction {
        let mut data = IX_TRIP_BREAKER.to_vec();
        data.extend_from_slice(&current_sqrt_price_x64.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(oracle_pda(pool), false),
                // The pool PDA must sign — the price is the pool's own.
                AccountMeta::new_readonly(pool, true),
            ],
            data,
        }
    }

    fn unpause_pool(pool: Address, authority: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(oracle_pda(pool), false),
                AccountMeta::new(authority, true),
            ],
            data: IX_UNPAUSE_POOL.to_vec(),
        }
    }
}

/// An initialized oracle whose "pool" is the payer key itself, so the tests
/// can sign recordings the way the real pool PDA would via CPI.
struct Fixture {
    svm: LiteSVM,
    payer: Keypair,
    pool: Address,
}

fn setup() -> Fixture {
    setup_at(1_000_000)
}

/// `setup_at(t)` pins the clock *before* initializing, so the genesis
/// observation is stamped at a known, controllable time.
fn setup_at(t: i64) -> Fixture {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    let program = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/oracle.so"
    ))
    .expect("build the program first: `anchor build -p oracle`");
    svm.add_program(PROGRAM_ID, &program).unwrap();

    set_time(&mut svm, t);

    let pool = payer.pubkey();
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize(
            payer.pubkey(),
            pool,
            MAX_DEVIATION_BPS,
            payer.pubkey(),
        )],
    );

    Fixture { svm, payer, pool }
}

/// Seeds a full 30-minute price-1 history: one observation per 600s across
/// the breaker window, then advances the clock so the newest price accrues.
fn seed_history(fixture: &mut Fixture) {
    for i in 0..4u32 {
        set_time(&mut fixture.svm, 1_000_000 + 600 * i as i64);
        send(
            &mut fixture.svm,
            &fixture.payer,
            &[Instructions::record_observation(
                fixture.pool,
                fixture.pool,
                PRICE_1,
            )],
        );
    }
    set_time(&mut fixture.svm, 1_000_000 + 4 * 600);
}

/// Records `prices` one per second starting at `start`, stepping the clock
/// between transactions.
fn record_from(fixture: &mut Fixture, start: i64, prices: &[u128]) {
    for (i, price) in prices.iter().enumerate() {
        set_time(&mut fixture.svm, start + i as i64);
        send(
            &mut fixture.svm,
            &fixture.payer,
            &[Instructions::record_observation(
                fixture.pool,
                fixture.pool,
                *price,
            )],
        );
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn initialize_writes_the_genesis_state_and_the_pda_matches_its_seeds() {
    let fixture = setup();
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));

    assert_eq!(
        &state[STATE_POOL..STATE_POOL + 32],
        fixture.pool.as_ref(),
        "the recorded pool is the one the seeds describe"
    );
    assert_eq!(state[STATE_IS_PAUSED], 0);
    assert_eq!(
        state[STATE_INDEX], 0,
        "the cursor starts at the genesis slot"
    );
    assert_eq!(
        state[STATE_COUNT], 1,
        "exactly one (genesis) observation exists"
    );
    assert_eq!(
        u16::from_le_bytes(
            state[STATE_THRESHOLD..STATE_THRESHOLD + 2]
                .try_into()
                .unwrap()
        ),
        MAX_DEVIATION_BPS,
        "the threshold is stored, not caller-supplied per check"
    );
    let (ts, price, cumulative) = observation_at(&state, 0);
    assert_eq!(
        ts,
        now(&fixture.svm),
        "the genesis observation is stamped now"
    );
    assert_eq!(price, 0, "the genesis price is 'not yet known'");
    assert_eq!(cumulative, 0);
}

#[test]
fn record_observation_advances_the_accumulator_by_price_times_seconds() {
    let mut fixture = setup();
    record_from(&mut fixture, 1_000_000, &[PRICE_1, PRICE_1, PRICE_1]);

    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_INDEX], 3, "the cursor follows the writes");
    assert_eq!(state[STATE_COUNT], 4, "genesis plus three recordings");

    let (_, _, cum3) = observation_at(&state, 3);
    // Genesis (price 0, accrues nothing), then two full seconds at price 1.
    assert_eq!(cum3, 2 * PRICE_1, "cumulative = Σ price × Δt");

    let (ts3, price3, _) = observation_at(&state, 3);
    assert_eq!(ts3, 1_000_002);
    assert_eq!(price3, PRICE_1);
}

#[test]
fn a_change_of_price_is_time_weighted_by_the_seconds_it_was_live() {
    let mut fixture = setup();
    record_from(
        &mut fixture,
        1_000_001,
        &[PRICE_1, PRICE_1, PRICE_1, PRICE_10],
    );

    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    let (_, _, cum4) = observation_at(&state, 4);
    // One second at 1.0 accrues PRICE_1; then the 10× price is only *written*,
    // not yet accrued — it starts accruing when the *next* observation lands.
    assert_eq!(cum4, 3 * PRICE_1);
    let (_, price4, _) = observation_at(&state, 4);
    assert_eq!(price4, PRICE_10);
}

#[test]
fn only_the_authorized_pool_may_record() {
    let mut fixture = setup();
    let impostor = Keypair::new();
    fixture.svm.airdrop(&impostor.pubkey(), LAMPORTS).unwrap();

    let code = send_error(
        &mut fixture.svm,
        &impostor,
        &[Instructions::record_observation(
            fixture.pool,
            impostor.pubkey(), // a signer, but not the recorded pool
            PRICE_1,
        )],
    );
    assert_eq!(code, ERR_UNAUTHORIZED_POOL);
}

#[test]
fn recordings_may_not_move_time_backwards() {
    let mut fixture = setup_at(5_000);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::record_observation(
            fixture.pool,
            fixture.pool,
            PRICE_1,
        )],
    );

    set_time(&mut fixture.svm, 4_999);
    let code = send_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::record_observation(
            fixture.pool,
            fixture.pool,
            PRICE_1,
        )],
    );
    assert_eq!(code, ERR_TIMESTAMP_MOVED_BACKWARDS);
}

#[test]
fn get_twap_returns_the_time_weighted_average_over_the_window() {
    let mut fixture = setup_at(1_000_000);
    // Genesis at t=1_000_000 (price unknown), then 1s steps: the price-1 era
    // covers [1_000_001, 1_000_003] and the price-10 era starts at 1_000_004.
    record_from(
        &mut fixture,
        1_000_001,
        &[PRICE_1, PRICE_1, PRICE_1, PRICE_10],
    );
    // Jump to t=1_000_010 (no new observation) so the newest price accrues.
    set_time(&mut fixture.svm, 1_000_010);

    // TWAP over [1_000_006, 1_000_010]: boundary at 1_000_006 lies in the
    // price-10 era (its accumulator covers 1_000_004..=1_000_006 by then),
    // and the whole window is at price 10.
    let meta = send_return(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::get_twap(fixture.pool, 4)],
    );
    let ret = meta.return_data.data;
    assert_eq!(ret.len(), 17, "Option<u128> is tag + 16 bytes");
    assert_eq!(ret[0], 1, "Some");
    let twap = u128::from_le_bytes(ret[1..17].try_into().unwrap());
    assert_eq!(twap, PRICE_10);

    // A mixed window: [1_000_000, 1_000_010] holds 3s at price 1 (1_000_001
    // ..=1_000_003), 6s at price 10 (1_000_004 ..=1_000_009) and 1s of the
    // unknown genesis price (1_000_000, accrues nothing).
    let meta = send_return(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::get_twap(fixture.pool, 10)],
    );
    let ret = meta.return_data.data;
    assert_eq!(ret[0], 1, "Some");
    let twap = u128::from_le_bytes(ret[1..17].try_into().unwrap());
    assert_eq!(twap, (3 * PRICE_1 + 6 * PRICE_10) / 10);

    // A window reaching past all history reports None, not a wrong number.
    let meta = send_return(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::get_twap(fixture.pool, 10_000)],
    );
    assert_eq!(meta.return_data.data[0], 0, "None");
}

#[test]
fn the_breaker_guard_reverts_a_deviating_price() {
    let mut fixture = setup_at(1_000_000);
    seed_history(&mut fixture);

    // A 10× spot against a 1.0 TWAP is ~9000 sqrt-bps of deviation — far
    // past the threshold. The guard reverts the transaction.
    let code = send_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::check_deviation(fixture.pool, PRICE_10)],
    );
    assert_eq!(code, ERR_PRICE_DEVIATION_EXCEEDED);

    // But a revert cannot leave state behind: the pause is NOT latched here.
    // Latching is `trip_breaker`'s job, in its own successful transaction.
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 0, "a reverted guard writes nothing");
}

#[test]
fn trip_breaker_latches_the_pause_in_a_successful_transaction() {
    let mut fixture = setup_at(1_000_000);
    seed_history(&mut fixture);

    // The permissionless trip: anyone can send it (here the payer), it
    // SUCCEEDS, and the pause sticks — this is the part a revert cannot do.
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::trip_breaker(fixture.pool, PRICE_10)],
    );
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 1, "the breaker latched");

    // Tripping again is refused while already paused.
    let code = send_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::trip_breaker(fixture.pool, PRICE_10)],
    );
    assert_eq!(code, ERR_POOL_PAUSED);

    // While paused, even a *reasonable* price is refused by the guard...
    let code = send_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::check_deviation(fixture.pool, PRICE_1)],
    );
    assert_eq!(code, ERR_POOL_PAUSED);

    // ...and a non-owner cannot lift the pause.
    let impostor = Keypair::new();
    fixture.svm.airdrop(&impostor.pubkey(), LAMPORTS).unwrap();
    let code = send_error(
        &mut fixture.svm,
        &impostor,
        &[Instructions::unpause_pool(fixture.pool, impostor.pubkey())],
    );
    assert_eq!(code, ERR_UNAUTHORIZED);

    // The recorded owner can, and trading may proceed afterwards.
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::unpause_pool(fixture.pool, fixture.pool)],
    );
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 0);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::check_deviation(fixture.pool, PRICE_1)],
    );
}

#[test]
fn trip_breaker_refuses_a_price_inside_the_threshold() {
    let mut fixture = setup_at(1_000_000);
    seed_history(&mut fixture);

    // A trip attempt with a well-behaved price must not pause anything — and
    // it is an error, so a keeper cannot "accidentally" pause a sane pool.
    let near = PRICE_1 + PRICE_1 / 100; // +1% in sqrt-price
    let code = send_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::trip_breaker(fixture.pool, near)],
    );
    assert_eq!(code, ERR_PRICE_WITHIN_THRESHOLD);
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 0);

    // The same price passes the guard, and the breaker stays untouched.
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::check_deviation(fixture.pool, near)],
    );
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 0);
}

#[test]
fn a_zero_threshold_is_rejected_at_initialization() {
    let mut fixture = setup_at(1_000_000);
    // A *fresh* pool keypair: the PDA has not been initialized, so the
    // handler's own threshold `require!` is what runs (not the `init` CPI).
    // The pool must sign, so it is an extra signer on the transaction.
    let fresh_pool = Keypair::new();
    fixture
        .svm
        .airdrop(&fresh_pool.pubkey(), LAMPORTS)
        .unwrap();
    let code = send_error_signed(
        &mut fixture.svm,
        &fixture.payer,
        &[&fresh_pool],
        &[Instructions::initialize(
            fixture.payer.pubkey(),
            fresh_pool.pubkey(),
            0,
            fixture.payer.pubkey(),
        )],
    );
    assert_eq!(code, ERR_INVALID_THRESHOLD);
}

/// The whole point of H-1's fix: an oracle cannot be created for a pool that
/// does not sign, so a third party cannot front-run (and mis-configure) it.
#[test]
fn initialize_requires_the_pool_signature() {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    let program = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/oracle.so"
    ))
    .expect("build the program first: `anchor build -p oracle`");
    svm.add_program(PROGRAM_ID, &program).unwrap();
    set_time(&mut svm, 1_000_000);

    // A pool that is not among the transaction's signers (a stand-in for a
    // pool PDA only the real AMM program can sign for).
    let foreign_pool = Keypair::new().pubkey();
    let mut ix = Instructions::initialize(
        payer.pubkey(),
        foreign_pool,
        MAX_DEVIATION_BPS,
        payer.pubkey(),
    );
    // The client cannot mark a pool it does not control as a signer, so the
    // required pool signature is absent and the program rejects the account.
    ix.accounts[1].is_signer = false;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        fresh_blockhash(&mut svm),
    );
    assert!(
        svm.send_transaction(tx).is_err(),
        "a pool that does not sign must not get an oracle"
    );
}

#[test]
fn the_guard_answers_only_the_pool_that_owns_the_oracle() {
    let mut fixture = setup_at(1_000_000);
    seed_history(&mut fixture);
    let impostor = Keypair::new();
    fixture.svm.airdrop(&impostor.pubkey(), LAMPORTS).unwrap();

    // The impostor signs its own transaction, but it is not the pool: the price
    // it would pass is a price no pool ever had, so the oracle refuses to answer
    // it rather than reporting on a trade that cannot happen.
    let code = send_error_signed(
        &mut fixture.svm,
        &impostor,
        &[&impostor],
        &[Instructions::check_deviation_signed_by(
            fixture.pool,
            impostor.pubkey(),
            PRICE_1,
        )],
    );
    assert_eq!(code, ERR_UNAUTHORIZED_POOL);

    // The pool itself, signed by the payer that is the pool, is answered as
    // before - the binding closed a door without changing the pool's own path.
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::check_deviation(fixture.pool, PRICE_1)],
    );
}

#[test]
fn the_breaker_skips_its_check_while_history_is_thin() {
    let mut fixture = setup();
    // Genesis only: no price yet. The breaker has nothing to compare against,
    // so it allows the trade rather than bricking a fresh pool…
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::check_deviation(fixture.pool, PRICE_10)],
    );
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 0, "a cold start is not a deviation");

    // …and the trip is a no-op for the same reason (it must NOT latch).
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::trip_breaker(fixture.pool, PRICE_10)],
    );
    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(state[STATE_IS_PAUSED], 0, "no history, no trip");
}

#[test]
fn the_ring_buffer_wraps_and_still_serves_its_window() {
    let mut fixture = setup();
    // Drive the cursor around the 100-slot ring: genesis at 1_000_000, then
    // 110 one-second recordings at price 1.
    let mut prices = vec![PRICE_1; 110];
    prices.push(PRICE_10); // the 111th write lands on the overwritten slot
    record_from(&mut fixture, 1_000_000, &prices);

    let state = data_of(&fixture.svm, oracle_pda(fixture.pool));
    assert_eq!(
        state[STATE_COUNT] as usize, 100,
        "the count saturates at the ring size"
    );
    assert_eq!(
        state[STATE_INDEX] as usize, 11,
        "111 writes after genesis: the cursor sits at 111 % 100"
    );
    // Write #k lands in slot k (mod 100): write #111 (the price-10 one) is
    // in slot 11, the cursor slot; slot 12 holds write #12 (t = 1_000_011).
    let (ts, price, _) = observation_at(&state, 11);
    assert_eq!(ts, 1_000_110);
    assert_eq!(price, PRICE_10);
    let (ts, _, _) = observation_at(&state, 12);
    assert_eq!(ts, 1_000_011);
    // And the wrapped-away slot really is gone.
    let (ts, price, cum) = observation_at(&state, 0);
    assert_eq!(ts, 1_000_099, "the genesis slot was overwritten");
    assert_eq!(price, PRICE_1);
    assert!(cum > 0);
}
