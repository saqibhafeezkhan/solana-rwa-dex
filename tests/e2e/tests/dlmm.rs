//! End-to-end tests for the DLMM program: the compiled program, on a LiteSVM,
//! against real SPL token accounts.
//!
//! The unit tests inside the crate exercise the pure math. These exercise what
//! no unit test can reach - the instruction handlers, the account wiring, and
//! the state a signed transaction leaves behind. They pin down, in particular:
//!
//! * `initialize_pool` creates the pool, its two vaults and its first bin array
//!   together, with every bin in that array priced from the moment it exists.
//! * a swap fills the active bin at its fixed price, leaves the fee with the
//!   starting bin's LPs, and crosses into the next bin that holds liquidity -
//!   moving `active_bin_id` with it. The input the swap took in is exactly what
//!   the bins ended up holding, and the output is exactly what left the vault.
//! * `min_amount_out` is enforced against what the vault actually paid.
//! * the pool authority is the only account that can pause the pool, a paused
//!   pool still lets LPs withdraw, and the role can be rotated.
//! * a pool wired to the oracle really CPIs it: the breaker sees the price the
//!   pool is about to trade at, the settled price is recorded, and a trade whose
//!   start price has drifted past the threshold is refused by the oracle.
//!
//! They need the SBF build, which `cargo test` does not produce:
//!
//! ```bash
//! anchor build && (cd tests/e2e && cargo test --test dlmm)
//! ```

// Instruction builders take the account list in the program's order; grouping
// them into structs would only rename the arguments.
#![allow(clippy::too_many_arguments)]

mod common;

use common::*;

/// The program under test: this must match `declare_id!` in
/// `programs/dlmm/src/lib.rs` (the client-side strings are the only place the ID
/// is repeated here, because these tests never link the program crate).
const PROGRAM_ID: Address = address!("5JPnmJouZ3yE9Xwaacjtr5F2E1ShYGxJ1mR4UV9EFE3c");

/// Anchor instruction discriminators (`sha256("global:<name>")[..8]`), taken from
/// `target/idl/dlmm.json` (a wrong one fails the first transaction loudly).
const IX_INITIALIZE_POOL: [u8; 8] = [95, 180, 10, 172, 84, 174, 232, 40];
const IX_INITIALIZE_BIN_ARRAY: [u8; 8] = [35, 86, 19, 185, 78, 212, 75, 211];
const IX_ADD_LIQUIDITY: [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];
const IX_REMOVE_LIQUIDITY: [u8; 8] = [80, 85, 209, 72, 24, 206, 177, 108];
const IX_SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
const IX_SET_PAUSED: [u8; 8] = [91, 60, 125, 192, 176, 225, 166, 218];
const IX_SET_AUTHORITY: [u8; 8] = [133, 250, 37, 21, 110, 163, 26, 121];

/// Bins in one array, from `BINS_PER_ARRAY`.
const BINS_PER_ARRAY: i64 = 70;
/// How many basis points one bin steps by, and what the pool charges.
const BIN_STEP: u16 = 10;
const BASE_FEE_BPS: u16 = 25;
/// The bin the pool opens on, and the array that holds it.
const ACTIVE_BIN: i32 = 0;
const ARRAY: i64 = 0;

/// `Pool` layout: the two mints, the step, the fee, the active bin, the
/// authority and the pause flag, the decimals, then the wiring policy and the
/// three bumps.
const POOL_MINT_A: usize = 8;
const POOL_MINT_B: usize = 40;
const POOL_BIN_STEP: usize = 72;
const POOL_BASE_FEE_BPS: usize = 74;
const POOL_ACTIVE_BIN: usize = 76;
const POOL_AUTHORITY: usize = 80;
const POOL_PAUSED: usize = 112;
const POOL_DECIMALS_A: usize = 113;
const POOL_DECIMALS_B: usize = 114;
const POOL_KYC_CONFIG: usize = 115;
const POOL_ORACLE_STATE: usize = 147;
const POOL_HOOKS_AUTHORITY: usize = 179;
const POOL_SETTLEMENT_SLOTS: usize = 211;

/// `BinArray` layout: discriminator, pool, index, then the bins vector's length
/// prefix and the bins themselves. One `Bin` is
/// `{ price_x64: u128, reserve_a: u64, reserve_b: u64, supply: u128 }`.
const ARRAY_POOL: usize = 8;
const ARRAY_INDEX: usize = 40;
const ARRAY_BIN_COUNT: usize = 48;
const ARRAY_BINS: usize = 52;
const BIN_LEN: usize = 48;
const BIN_PRICE: usize = 0;
const BIN_RESERVE_A: usize = 16;
const BIN_RESERVE_B: usize = 24;
const BIN_SUPPLY: usize = 32;

/// `Position` layout: discriminator, pool, owner, bin id, shares.
const POSITION_OWNER: usize = 40;
const POSITION_BIN_ID: usize = 72;
const POSITION_SHARES: usize = 76;

/// dlmm's errors, in `ErrorCode`'s order.
const ERR_SLIPPAGE_EXCEEDED: u32 = 6003;
const ERR_INSUFFICIENT_LIQUIDITY: u32 = 6004;
const ERR_INSUFFICIENT_SHARES: u32 = 6007;
const ERR_POOL_PAUSED: u32 = 6009;
const ERR_NO_BIN_ARRAYS: u32 = 6011;
const ERR_BIN_ARRAY_OUT_OF_RANGE: u32 = 6016;
/// Anchor's own `ConstraintHasOne`, which is what a non-authority hits.
const ERR_CONSTRAINT_HAS_ONE: u32 = 2001;

/// The oracle rail's fixture: wide enough that a trade walking five 1% bins
/// moves the price past the threshold the pool was created with.
const WIRED_THRESHOLD_BPS: u16 = 488;
const WIRED_BIN_STEP: u16 = 100;
/// The window `check_deviation` averages over, from `DEVIATION_WINDOW_SECONDS`.
const WINDOW_SECONDS: i64 = 1800;

const Q64: u128 = 1u128 << 64;

// -----------------------------------------------------------------------
// Layouts
// -----------------------------------------------------------------------

fn bin_array_index(bin_id: i32) -> i64 {
    (bin_id as i64).div_euclid(BINS_PER_ARRAY)
}

fn bin_offset(bin_id: i32) -> usize {
    (bin_id as i64).rem_euclid(BINS_PER_ARRAY) as usize
}

fn pool_pda(mint_a: Address, mint_b: Address) -> Address {
    Address::find_program_address(&[b"pool", mint_a.as_ref(), mint_b.as_ref()], &PROGRAM_ID).0
}

fn bin_array_pda(pool: Address, index: i64) -> Address {
    Address::find_program_address(
        &[b"bin_array", pool.as_ref(), &index.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

fn vault_pda(prefix: &[u8], pool: Address, mint: Address) -> Address {
    Address::find_program_address(&[prefix, pool.as_ref(), mint.as_ref()], &PROGRAM_ID).0
}

fn position_pda(pool: Address, owner: Address, bin_id: i32) -> Address {
    Address::find_program_address(
        &[
            b"position",
            pool.as_ref(),
            owner.as_ref(),
            &bin_id.to_le_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

/// One bin's state out of its array's account data.
struct Bin {
    price: u128,
    reserve_a: u64,
    reserve_b: u64,
    supply: u128,
}

fn bin(svm: &LiteSVM, pool: Address, bin_id: i32) -> Bin {
    let data = data_of(svm, bin_array_pda(pool, bin_array_index(bin_id)));
    let at = ARRAY_BINS + bin_offset(bin_id) * BIN_LEN;
    Bin {
        price: u128_at(&data, at + BIN_PRICE),
        reserve_a: u64_at(&data, at + BIN_RESERVE_A),
        reserve_b: u64_at(&data, at + BIN_RESERVE_B),
        supply: u128_at(&data, at + BIN_SUPPLY),
    }
}

fn vault_balance(svm: &LiteSVM, pool: Address, mint: Address, prefix: &[u8]) -> u64 {
    token_balance(svm, vault_pda(prefix, pool, mint))
}

// -----------------------------------------------------------------------
// Instructions
// -----------------------------------------------------------------------

/// A pool's rail policy as `initialize_pool` records it, plus whether the swap
/// builder has to hand the swap the real oracle accounts.
#[derive(Clone, Copy, Default)]
struct Wiring {
    /// The oracle rail's threshold, and whether the rail is on at all.
    oracle: Option<u16>,
}

struct Instructions;

impl Instructions {
    fn initialize_pool(
        payer: Address,
        mint_a: Address,
        mint_b: Address,
        bin_step: u16,
        wiring: Wiring,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_INITIALIZE_POOL.to_vec();
        data.extend_from_slice(&ARRAY.to_le_bytes());
        data.extend_from_slice(&ACTIVE_BIN.to_le_bytes());
        data.extend_from_slice(&bin_step.to_le_bytes());
        data.extend_from_slice(&BASE_FEE_BPS.to_le_bytes());
        data.extend_from_slice(payer.as_ref()); // authority
        data.extend_from_slice(Address::default().as_ref()); // kyc_config
        data.extend_from_slice(&wiring.oracle.unwrap_or(0).to_le_bytes());
        data.extend_from_slice(Address::default().as_ref()); // hooks_authority
        data.extend_from_slice(&0u64.to_le_bytes()); // settlement_slots
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(pool, false),
                AccountMeta::new(bin_array_pda(pool, ARRAY), false),
                AccountMeta::new(vault_pda(b"vault_a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"vault_b", pool, mint_b), false),
                // The oracle state, created here by the oracle program's CPI
                // with the pool signing. The program-ID sentinel leaves the
                // rail off (Anchor reads it as `None`).
                AccountMeta::new(wiring.oracle.map_or(PROGRAM_ID, |_| oracle_pda(pool)), false),
                AccountMeta::new_readonly(PROGRAM_ID, false), // kyc_program: off
                AccountMeta::new_readonly(
                    wiring.oracle.map_or(PROGRAM_ID, |_| ORACLE_PROGRAM_ID),
                    false,
                ),
                AccountMeta::new_readonly(PROGRAM_ID, false), // hooks_program: off
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn initialize_bin_array(payer: Address, pool: Address, index: i64) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(bin_array_pda(pool, index), false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data: {
                let mut data = IX_INITIALIZE_BIN_ARRAY.to_vec();
                data.extend_from_slice(&index.to_le_bytes());
                data
            },
        }
    }

    fn add_liquidity(
        user: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        bin_id: i32,
        amount_a: u64,
        amount_b: u64,
    ) -> Instruction {
        let array_index = bin_array_index(bin_id);
        let mut data = IX_ADD_LIQUIDITY.to_vec();
        data.extend_from_slice(&array_index.to_le_bytes());
        data.extend_from_slice(&bin_id.to_le_bytes());
        data.extend_from_slice(&amount_a.to_le_bytes());
        data.extend_from_slice(&amount_b.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(bin_array_pda(pool, array_index), false),
                AccountMeta::new(position_pda(pool, user, bin_id), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_pda(b"vault_a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"vault_b", pool, mint_b), false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(user, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn remove_liquidity(
        user: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        bin_id: i32,
        shares: u128,
        min_amount_a: u64,
        min_amount_b: u64,
    ) -> Instruction {
        let array_index = bin_array_index(bin_id);
        let mut data = IX_REMOVE_LIQUIDITY.to_vec();
        data.extend_from_slice(&array_index.to_le_bytes());
        data.extend_from_slice(&bin_id.to_le_bytes());
        data.extend_from_slice(&shares.to_le_bytes());
        data.extend_from_slice(&min_amount_a.to_le_bytes());
        data.extend_from_slice(&min_amount_b.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(bin_array_pda(pool, array_index), false),
                AccountMeta::new(position_pda(pool, user, bin_id), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_pda(b"vault_a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"vault_b", pool, mint_b), false),
                AccountMeta::new(user_token_a, false),
                AccountMeta::new(user_token_b, false),
                AccountMeta::new(user, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            ],
            data,
        }
    }

    /// A swap on a pool with no rails wired: every rail account is this
    /// program's own ID sentinel, which is what the handler requires of a rail
    /// the pool's policy does not demand.
    fn swap(
        user: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        amount_in: u64,
        min_amount_out: u64,
        is_a_to_b: bool,
        wiring: Wiring,
        walk: &[i64],
    ) -> Instruction {
        let (oracle_state, oracle_program) = match wiring.oracle {
            Some(_) => (oracle_pda(pool), ORACLE_PROGRAM_ID),
            None => (PROGRAM_ID, PROGRAM_ID), // sentinel: rail off
        };
        let mut data = IX_SWAP.to_vec();
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&min_amount_out.to_le_bytes());
        data.push(u8::from(is_a_to_b));
        data.extend_from_slice(&0u64.to_le_bytes()); // trade_id: no settlement rail
        let mut accounts = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_pda(b"vault_a", pool, mint_a), false),
            AccountMeta::new(vault_pda(b"vault_b", pool, mint_b), false),
            AccountMeta::new(user_token_a, false),
            AccountMeta::new(user_token_b, false),
            AccountMeta::new(user, true),
            AccountMeta::new(PROGRAM_ID, false), // kyc_config: off
            AccountMeta::new_readonly(PROGRAM_ID, false), // user_kyc_account
            AccountMeta::new(oracle_state, false),
            AccountMeta::new(PROGRAM_ID, false), // hooks_authority: off
            AccountMeta::new(PROGRAM_ID, false), // custody_token_account
            AccountMeta::new_readonly(PROGRAM_ID, false), // settlement_config
            AccountMeta::new_readonly(PROGRAM_ID, false), // settlement_program
            AccountMeta::new(PROGRAM_ID, false), // settlement_vault
            AccountMeta::new(PROGRAM_ID, false), // settlement_vault_token
            AccountMeta::new_readonly(PROGRAM_ID, false), // associated_token_program
            AccountMeta::new_readonly(PROGRAM_ID, false), // kyc_program: off
            AccountMeta::new_readonly(oracle_program, false),
            AccountMeta::new_readonly(PROGRAM_ID, false), // hooks_program: off
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ];
        // The bin arrays the trade may walk, in the order it will walk them.
        for index in walk {
            accounts.push(AccountMeta::new(bin_array_pda(pool, *index), false));
        }
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
            data,
        }
    }

    fn set_paused(pool: Address, authority: Address, paused: bool) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data: {
                let mut data = IX_SET_PAUSED.to_vec();
                data.push(u8::from(paused));
                data
            },
        }
    }

    fn set_authority(pool: Address, authority: Address, new_authority: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data: {
                let mut data = IX_SET_AUTHORITY.to_vec();
                data.extend_from_slice(new_authority.as_ref());
                data
            },
        }
    }
}

// -----------------------------------------------------------------------
// Fixture
// -----------------------------------------------------------------------

/// A provisioned pool: the payer holds `FUNDS` of each mint, the pool exists
/// with its vaults and first bin array, and nothing has been deposited yet.
struct Fixture {
    svm: LiteSVM,
    payer: Keypair,
    mint_a: Address,
    mint_b: Address,
    user_token_a: Address,
    user_token_b: Address,
    pool: Address,
    wiring: Wiring,
}

fn setup(bin_step: u16, wiring: Wiring) -> Fixture {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    load_program(&mut svm, PROGRAM_ID, "dlmm");
    if wiring.oracle.is_some() {
        load_program(&mut svm, ORACLE_PROGRAM_ID, "oracle");
    }

    let payer_address = payer.pubkey();
    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    let user_token_a = Keypair::new().pubkey();
    let user_token_b = Keypair::new().pubkey();
    create_mint(&mut svm, payer_address, mint_a, 6);
    create_mint(&mut svm, payer_address, mint_b, 6);
    create_token_account(&mut svm, user_token_a, mint_a, payer_address, FUNDS);
    create_token_account(&mut svm, user_token_b, mint_b, payer_address, FUNDS);

    let pool = pool_pda(mint_a, mint_b);
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool(
            payer_address,
            mint_a,
            mint_b,
            bin_step,
            wiring,
        )],
    );

    Fixture {
        svm,
        payer,
        mint_a,
        mint_b,
        user_token_a,
        user_token_b,
        pool,
        wiring,
    }
}

impl Fixture {
    fn deposit(&mut self, bin_id: i32, amount_a: u64, amount_b: u64) {
        let instruction = Instructions::add_liquidity(
            self.payer.pubkey(),
            self.pool,
            self.mint_a,
            self.mint_b,
            self.user_token_a,
            self.user_token_b,
            bin_id,
            amount_a,
            amount_b,
        );
        send(&mut self.svm, &self.payer, &[instruction]);
    }

    fn swap(&mut self, amount_in: u64, min_amount_out: u64, is_a_to_b: bool, walk: &[i64]) {
        let instruction = Instructions::swap(
            self.payer.pubkey(),
            self.pool,
            self.mint_a,
            self.mint_b,
            self.user_token_a,
            self.user_token_b,
            amount_in,
            min_amount_out,
            is_a_to_b,
            self.wiring,
            walk,
        );
        send(&mut self.svm, &self.payer, &[instruction]);
    }

    fn swap_error(&mut self, amount_in: u64, is_a_to_b: bool) -> (u32, String) {
        let instruction = Instructions::swap(
            self.payer.pubkey(),
            self.pool,
            self.mint_a,
            self.mint_b,
            self.user_token_a,
            self.user_token_b,
            amount_in,
            0,
            is_a_to_b,
            self.wiring,
            &[ARRAY],
        );
        send_error_logs(&mut self.svm, &self.payer, &[instruction])
    }

    fn active_bin(&self) -> i32 {
        i32_at(&data_of(&self.svm, self.pool), POOL_ACTIVE_BIN)
    }

    fn balance_a(&self) -> u64 {
        token_balance(&self.svm, self.user_token_a)
    }

    fn balance_b(&self) -> u64 {
        token_balance(&self.svm, self.user_token_b)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn initialize_pool_creates_its_vaults_and_a_fully_priced_bin_array() {
    let f = setup(BIN_STEP, Wiring::default());

    let pool = data_of(&f.svm, f.pool);
    assert_eq!(address_at(&pool, POOL_MINT_A), f.mint_a);
    assert_eq!(address_at(&pool, POOL_MINT_B), f.mint_b);
    assert_eq!(u16_at(&pool, POOL_BIN_STEP), BIN_STEP);
    assert_eq!(u16_at(&pool, POOL_BASE_FEE_BPS), BASE_FEE_BPS);
    assert_eq!(i32_at(&pool, POOL_ACTIVE_BIN), ACTIVE_BIN);
    assert_eq!(address_at(&pool, POOL_AUTHORITY), f.payer.pubkey());
    assert!(!bool_at(&pool, POOL_PAUSED));
    assert_eq!(u8_at(&pool, POOL_DECIMALS_A), 6);
    assert_eq!(u8_at(&pool, POOL_DECIMALS_B), 6);
    // An unwired pool records zeroes for every rail and leaves its oracle off.
    assert_eq!(address_at(&pool, POOL_KYC_CONFIG), Address::default());
    assert_eq!(address_at(&pool, POOL_ORACLE_STATE), Address::default());
    assert_eq!(address_at(&pool, POOL_HOOKS_AUTHORITY), Address::default());
    assert_eq!(u64_at(&pool, POOL_SETTLEMENT_SLOTS), 0);

    // The vaults belong to the pool: the program is their only authority, so a
    // caller cannot substitute an account it controls.
    for (mint, prefix) in [(f.mint_a, b"vault_a"), (f.mint_b, b"vault_b")] {
        let vault = vault_pda(prefix, f.pool, mint);
        assert_eq!(token_owner(&f.svm, vault), f.pool);
        assert_eq!(token_mint(&f.svm, vault), mint);
        assert_eq!(token_balance(&f.svm, vault), 0);
    }

    // The first bin array exists, belongs to the pool, and prices every bin -
    // so liquidity can be added across the band without initializing anything.
    let array = data_of(&f.svm, bin_array_pda(f.pool, ARRAY));
    assert_eq!(address_at(&array, ARRAY_POOL), f.pool);
    assert_eq!(i64_at(&array, ARRAY_INDEX), ARRAY);
    assert_eq!(u32_at(&array, ARRAY_BIN_COUNT), BINS_PER_ARRAY as u32);
    assert_eq!(
        array.len(),
        ARRAY_BINS + BINS_PER_ARRAY as usize * BIN_LEN,
        "the array is sized for exactly BINS_PER_ARRAY bins"
    );

    // Bin 0 is the array's first bin, so it sits at parity: one raw A per raw B.
    assert_eq!(bin(&f.svm, f.pool, ACTIVE_BIN).price, Q64);
    let mut previous = 0u128;
    for id in 0..BINS_PER_ARRAY as i32 {
        let price = bin(&f.svm, f.pool, id).price;
        assert!(price > 0, "bin {id} was created unpriced");
        assert!(price > previous, "bin prices must rise with the bin id");
        previous = price;
    }
}

#[test]
fn an_lp_can_deposit_into_a_bin_and_withdraw_the_same_value() {
    let mut f = setup(BIN_STEP, Wiring::default());
    let (amount_a, amount_b) = (400_000_000u64, 600_000_000u64);

    f.deposit(ACTIVE_BIN, amount_a, amount_b);

    let position = position_pda(f.pool, f.payer.pubkey(), ACTIVE_BIN);
    let position_data = data_of(&f.svm, position);
    let shares = u128_at(&position_data, POSITION_SHARES);
    assert!(shares > 0, "a deposit must mint shares");
    assert_eq!(
        address_at(&position_data, POSITION_OWNER),
        f.payer.pubkey(),
        "the position has to record its owner, or it can never be withdrawn from"
    );
    assert_eq!(i32_at(&position_data, POSITION_BIN_ID), ACTIVE_BIN);

    // The deposit is in the bin's reserves and in the pool's own vaults.
    let funded = bin(&f.svm, f.pool, ACTIVE_BIN);
    assert_eq!(funded.reserve_a, amount_a);
    assert_eq!(funded.reserve_b, amount_b);
    assert_eq!(funded.supply, shares);
    assert_eq!(vault_balance(&f.svm, f.pool, f.mint_a, b"vault_a"), amount_a);
    assert_eq!(vault_balance(&f.svm, f.pool, f.mint_b, b"vault_b"), amount_b);
    assert_eq!(f.balance_a(), FUNDS - amount_a);
    assert_eq!(f.balance_b(), FUNDS - amount_b);

    // Nothing has traded, so a full withdrawal pays the deposit back exactly.
    let instruction = Instructions::remove_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        ACTIVE_BIN,
        shares,
        0,
        0,
    );
    send(&mut f.svm, &f.payer, &[instruction]);

    assert_eq!(f.balance_a(), FUNDS);
    assert_eq!(f.balance_b(), FUNDS);
    assert_eq!(vault_balance(&f.svm, f.pool, f.mint_a, b"vault_a"), 0);
    assert_eq!(vault_balance(&f.svm, f.pool, f.mint_b, b"vault_b"), 0);
    let emptied = bin(&f.svm, f.pool, ACTIVE_BIN);
    assert_eq!(emptied.reserve_a, 0);
    assert_eq!(emptied.reserve_b, 0);
    assert_eq!(emptied.supply, 0);

    // Handing it a position it can no longer cover is refused rather than
    // paying twice.
    let instruction = Instructions::remove_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        ACTIVE_BIN,
        1,
        0,
        0,
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_INSUFFICIENT_SHARES
    );
}

#[test]
fn a_swap_fills_the_active_bin_and_crosses_into_the_next_one_that_has_liquidity() {
    let mut f = setup(BIN_STEP, Wiring::default());

    // The starting bin is deliberately thin on B: a trade of the size below
    // drains it. Bin 1 stays empty, so the walk has to skip it.
    let (thin_a, thin_b) = (1_000_000_000u64, 100_000_000u64);
    let (deep_a, deep_b) = (1_000_000_000u64, 1_000_000_000u64);
    f.deposit(0, thin_a, thin_b);
    f.deposit(2, deep_a, deep_b);

    let amount_in = 500_000_000u64;
    let before = f.balance_b();
    f.swap(amount_in, 0, true, &[ARRAY]);

    // The trade ran out of B in bin 0, crossed the empty bin 1, and filled in
    // bin 2 - which is now where the next trade starts.
    assert_eq!(f.active_bin(), 2, "the active bin follows the trade");
    let start = bin(&f.svm, f.pool, 0);
    let end = bin(&f.svm, f.pool, 2);
    assert_eq!(start.reserve_b, 0, "a drained bin is left holding no output");
    assert_eq!(
        bin(&f.svm, f.pool, 1).reserve_a,
        0,
        "an empty bin is skipped, not filled"
    );

    // A 25 bps base fee against a bin holding both tokens is 37 bps: the fee is
    // left in the bin the trade started in, for that bin's LPs.
    let fee = amount_in * 37 / 10_000;
    assert_eq!(
        start.reserve_a,
        thin_a + fee + thin_b,
        "the start bin keeps the fee plus what it charged to drain itself"
    );
    assert_eq!(end.reserve_a, deep_a + (amount_in - fee - thin_b));

    // Every raw unit of input is in the bins, and exactly the output the vault
    // paid left them.
    let out = f.balance_b() - before;
    assert!(out > 0);
    assert_eq!(
        (start.reserve_a - thin_a) + (end.reserve_a - deep_a),
        amount_in
    );
    assert_eq!((thin_b - start.reserve_b) + (deep_b - end.reserve_b), out);
    assert_eq!(
        vault_balance(&f.svm, f.pool, f.mint_a, b"vault_a"),
        thin_a + deep_a + amount_in
    );
    assert_eq!(
        vault_balance(&f.svm, f.pool, f.mint_b, b"vault_b"),
        thin_b + deep_b - out
    );
    assert_eq!(f.balance_a(), FUNDS - (thin_a + deep_a) - amount_in);

    // A swap with no bin array to walk is refused instead of trading against
    // nothing.
    let instruction = Instructions::swap(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        1_000,
        0,
        true,
        f.wiring,
        &[],
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_NO_BIN_ARRAYS
    );

    // `min_amount_out` is checked against what the vault actually paid, and a
    // refused swap leaves no state behind.
    let instruction = Instructions::swap(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        10_000_000,
        out + 1,
        true,
        f.wiring,
        &[ARRAY],
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_SLIPPAGE_EXCEEDED
    );
    assert_eq!(f.balance_b(), before + out);
    assert_eq!(f.balance_a(), FUNDS - (thin_a + deep_a) - amount_in);
    assert_eq!(f.active_bin(), 2);
}

#[test]
fn a_swap_that_cannot_be_filled_is_refused_rather_than_partially_filled() {
    let mut f = setup(BIN_STEP, Wiring::default());
    f.deposit(0, 1_000_000_000, 10_000_000);

    // Bin 0 holds 10 B; asking for vastly more runs off the end of the array.
    let (code, _) = f.swap_error(5_000_000_000, true);
    assert_eq!(code, ERR_INSUFFICIENT_LIQUIDITY);
    assert_eq!(f.active_bin(), 0);
    assert_eq!(bin(&f.svm, f.pool, 0).reserve_b, 10_000_000);
}

#[test]
fn only_the_authority_can_pause_the_pool_and_a_pause_never_traps_liquidity() {
    let mut f = setup(BIN_STEP, Wiring::default());
    f.deposit(0, 1_000_000_000, 1_000_000_000);

    // A stranger holding no role is refused by the pool's `has_one`. The
    // stranger is the payer here so that it can sign its own attempt.
    let stranger = Keypair::new();
    f.svm.airdrop(&stranger.pubkey(), LAMPORTS).unwrap();
    let instruction = Instructions::set_paused(f.pool, stranger.pubkey(), true);
    let (code, _) = send_error_logs(&mut f.svm, &stranger, &[instruction]);
    assert_eq!(code, ERR_CONSTRAINT_HAS_ONE);
    assert!(!bool_at(&data_of(&f.svm, f.pool), POOL_PAUSED));

    let instruction = Instructions::set_paused(f.pool, f.payer.pubkey(), true);
    send(&mut f.svm, &f.payer, &[instruction]);
    assert!(bool_at(&data_of(&f.svm, f.pool), POOL_PAUSED));

    // Paused: no swaps and no deposits...
    let instruction = Instructions::swap(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        1_000_000,
        0,
        true,
        f.wiring,
        &[ARRAY],
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_POOL_PAUSED
    );
    let instruction = Instructions::add_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        0,
        1_000_000,
        1_000_000,
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_POOL_PAUSED
    );

    // ...but a withdrawal still works: a pause must never trap LP funds.
    let shares = u128_at(
        &data_of(&f.svm, position_pda(f.pool, f.payer.pubkey(), 0)),
        POSITION_SHARES,
    );
    let instruction = Instructions::remove_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        0,
        shares,
        0,
        0,
    );
    send(&mut f.svm, &f.payer, &[instruction]);
    assert_eq!(f.balance_a(), FUNDS);

    // The role can be rotated onto a new key, and the old one loses it.
    let new_authority = Keypair::new();
    let instruction =
        Instructions::set_authority(f.pool, f.payer.pubkey(), new_authority.pubkey());
    send(&mut f.svm, &f.payer, &[instruction]);
    assert_eq!(
        address_at(&data_of(&f.svm, f.pool), POOL_AUTHORITY),
        new_authority.pubkey()
    );

    let instruction = Instructions::set_paused(f.pool, f.payer.pubkey(), false);
    let (code, _) = send_error_logs(&mut f.svm, &f.payer, &[instruction]);
    assert_eq!(code, ERR_CONSTRAINT_HAS_ONE);

    // ...while the new holder can.
    let instruction = Instructions::set_paused(f.pool, new_authority.pubkey(), false);
    send_with(
        &mut f.svm,
        &f.payer,
        &[&f.payer, &new_authority],
        &[instruction],
    );
    assert!(!bool_at(&data_of(&f.svm, f.pool), POOL_PAUSED));
}

#[test]
fn a_permissionless_caller_can_open_another_bin_array_near_the_active_bin() {
    let mut f = setup(BIN_STEP, Wiring::default());

    let instruction =
        Instructions::initialize_bin_array(f.payer.pubkey(), f.pool, ARRAY + 1);
    send(&mut f.svm, &f.payer, &[instruction]);

    let array = data_of(&f.svm, bin_array_pda(f.pool, ARRAY + 1));
    assert_eq!(address_at(&array, ARRAY_POOL), f.pool);
    assert_eq!(i64_at(&array, ARRAY_INDEX), ARRAY + 1);
    assert_eq!(u32_at(&array, ARRAY_BIN_COUNT), BINS_PER_ARRAY as u32);
    // The new array continues the number line: its first bin is one array up.
    assert!(bin(&f.svm, f.pool, BINS_PER_ARRAY as i32).price > Q64);

    // Beyond `MAX_BIN_ARRAY_SPAN` arrays from the active bin there is nothing
    // to open: the helper checks the span against the pool's own active bin.
    let instruction = Instructions::initialize_bin_array(f.payer.pubkey(), f.pool, 17);
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_BIN_ARRAY_OUT_OF_RANGE
    );
}

#[test]
fn a_wired_pool_gates_its_swaps_on_the_oracle_it_created_with_the_pool() {
    let mut f = setup(
        WIRED_BIN_STEP,
        Wiring {
            oracle: Some(WIRED_THRESHOLD_BPS),
        },
    );

    // The oracle was created by the pool's own `initialize_pool`, through a CPI
    // the pool PDA signed, so a third party could not have configured it.
    let oracle = oracle_pda(f.pool);
    assert_eq!(owner_of(&f.svm, oracle), ORACLE_PROGRAM_ID);
    let state = data_of(&f.svm, oracle);
    assert_eq!(address_at(&state, STATE_POOL), f.pool);
    assert_eq!(u16_at(&state, STATE_THRESHOLD), WIRED_THRESHOLD_BPS);

    // Bin 0 pays out a little B; bin 5 is deep. The trade below drains bin 0,
    // skips the empty bins between, and settles in bin 5 - five 1% bins up.
    f.deposit(0, 1_000_000_000, 10_000_000);
    f.deposit(5, 1_000_000_000, 1_000_000_000);
    let opened_at = now(&f.svm);

    // Cold start: no price history, so the breaker allows the trade and the
    // pool then records the price it settled at.
    f.swap(1_000_000, 0, true, &[ARRAY]);
    assert_eq!(f.active_bin(), 0);
    let (recorded_at, recorded_price, _) = newest_observation(&f.svm, oracle);
    assert_eq!(
        (recorded_at, recorded_price),
        (opened_at, bin(&f.svm, f.pool, 0).price),
        "the pool records the price it settled at"
    );

    // The window closes exactly on that observation, so the TWAP is now the
    // price the pool traded at - and the trade still starts there, so it is
    // inside the threshold.
    set_time(&mut f.svm, opened_at + WINDOW_SECONDS);
    f.swap(20_000_000, 0, true, &[ARRAY]);
    assert_eq!(f.active_bin(), 5, "the trade walked five bins");
    let (recorded_at, recorded_price, _) = newest_observation(&f.svm, oracle);
    assert_eq!(
        (recorded_at, recorded_price),
        (opened_at + WINDOW_SECONDS, bin(&f.svm, f.pool, 5).price),
        "the settled price is the new active bin's, recorded by the pool"
    );

    // One second later the spot price is five bins above the TWAP the window
    // still holds (~5.1% > the pool's 4.88% threshold), so the pool's own
    // pre-trade check refuses the next trade.
    set_time(&mut f.svm, opened_at + WINDOW_SECONDS + 1);
    let active = f.active_bin();
    let (code, logs) = f.swap_error(1_000_000, true);
    assert_eq!(code, ERR_PRICE_DEVIATION_EXCEEDED);
    assert!(
        logs.contains(DEVIATION_LOG),
        "the refusal must come from the oracle, not from the pool's math:\n{logs}"
    );

    // A refused trade writes nothing: the price and the recorded observation
    // are exactly where the previous, successful swap left them.
    assert_eq!(f.active_bin(), active);
    let (recorded_at, recorded_price, _) = newest_observation(&f.svm, oracle);
    assert_eq!(
        (recorded_at, recorded_price),
        (opened_at + WINDOW_SECONDS, bin(&f.svm, f.pool, 5).price)
    );
}
