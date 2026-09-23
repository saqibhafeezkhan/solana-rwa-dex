//! End-to-end tests for the Maverick (DDAMM) program: the compiled program, on a
//! LiteSVM, against real SPL token accounts.
//!
//! The unit tests inside the crate exercise the pure math and the merge rules on
//! their own. These exercise what no unit test can reach - the instruction
//! handlers, the account wiring, and the state a signed transaction leaves
//! behind. They pin down, in particular:
//!
//! * `initialize_pool` creates the pool, its vaults and its first bin array
//!   together, with every bin priced from the moment it exists.
//! * a bin's kind belongs to the bin: a directional bin is refused until the
//!   pool has a static baseline, and a later deposit has to agree with the kind
//!   already there.
//! * `move_bins` really carries a bin's liquidity toward the price - across bin
//!   arrays, through the whole chain - and the LP deposited in the bin it left
//!   behind can still withdraw, which is the fix for liquidity being stranded by
//!   a move.
//! * a static bin never moves, a moved bin never moves again, and a move is
//!   refused unless it brings liquidity closer to the active bin.
//! * the pool authority is the only account that can pause the pool, and a pause
//!   never traps LP funds.
//! * a pool wired to the oracle really CPIs it, and the breaker's refusal comes
//!   from the oracle.
//!
//! They need the SBF build, which `cargo test` does not produce:
//!
//! ```bash
//! anchor build && (cd tests/e2e && cargo test --test maverick)
//! ```

// Instruction builders take the account list in the program's order; grouping
// them into structs would only rename the arguments.
#![allow(clippy::too_many_arguments)]

mod common;

use common::*;

/// The program under test: this must match `declare_id!` in
/// `programs/maverick/src/lib.rs`.
const PROGRAM_ID: Address = address!("9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4");

/// Anchor instruction discriminators (`sha256("global:<name>")[..8]`), taken from
/// `target/idl/maverick.json` (a wrong one fails the first transaction loudly).
const IX_INITIALIZE_POOL: [u8; 8] = [95, 180, 10, 172, 84, 174, 232, 40];
const IX_INITIALIZE_BIN_ARRAY: [u8; 8] = [35, 86, 19, 185, 78, 212, 75, 211];
const IX_ADD_LIQUIDITY: [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];
const IX_REMOVE_LIQUIDITY: [u8; 8] = [80, 85, 209, 72, 24, 206, 177, 108];
const IX_MOVE_BINS: [u8; 8] = [198, 5, 59, 126, 31, 67, 107, 91];
const IX_SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
const IX_SET_PAUSED: [u8; 8] = [91, 60, 125, 192, 176, 225, 166, 218];
const IX_SET_AUTHORITY: [u8; 8] = [133, 250, 37, 21, 110, 163, 26, 121];

/// Bins in one array, from `BINS_PER_ARRAY`.
const BINS_PER_ARRAY: i64 = 50;
/// How many basis points one bin steps by, and what the pool charges.
const BIN_STEP: u16 = 10;
const BASE_FEE_BPS: u16 = 25;
/// The bin the pool opens on, and the array that holds it.
const ACTIVE_BIN: i32 = 0;
const ARRAY: i64 = 0;

/// How a bin follows the price, as `BinKind` serializes.
const KIND_STATIC: u8 = 0;
const KIND_RIGHT: u8 = 1;
const KIND_BOTH: u8 = 3;

/// `Pool` layout: the mints, the step, the fee, the active bin, the authority,
/// the pause flag and the static baseline flag, the decimals, then the wiring
/// policy and the three bumps.
const POOL_MINT_A: usize = 8;
const POOL_MINT_B: usize = 40;
const POOL_BIN_STEP: usize = 72;
const POOL_BASE_FEE_BPS: usize = 74;
const POOL_ACTIVE_BIN: usize = 76;
const POOL_AUTHORITY: usize = 80;
const POOL_PAUSED: usize = 112;
const POOL_STATIC_BASELINE: usize = 113;
const POOL_DECIMALS_A: usize = 114;
const POOL_KYC_CONFIG: usize = 116;
const POOL_ORACLE_STATE: usize = 148;
const POOL_HOOKS_AUTHORITY: usize = 180;
const POOL_SETTLEMENT_SLOTS: usize = 212;

/// `BinArray` layout: discriminator, pool, index, the bins vector's length
/// prefix, then the bins. One `Bin` is `{ price_x64: u128, reserve_a: u64,
/// reserve_b: u64, supply: u128, kind: u8, moved_to: Option<MoveRef> }`, where a
/// `MoveRef` is `{ to_bin_id: i32, share_ratio: u128 }` behind an `Option` tag.
const ARRAY_POOL: usize = 8;
const ARRAY_INDEX: usize = 40;
const ARRAY_BIN_COUNT: usize = 48;
const ARRAY_BINS: usize = 52;
const BIN_LEN: usize = 70;
const BIN_PRICE: usize = 0;
const BIN_RESERVE_A: usize = 16;
const BIN_RESERVE_B: usize = 24;
const BIN_SUPPLY: usize = 32;
const BIN_KIND: usize = 48;
const BIN_MOVED_TO: usize = 49;
const MOVED_TO_BIN_ID: usize = 50;
const MOVED_TO_RATIO: usize = 54;

/// `Position` layout: discriminator, pool, owner, bin id, shares.
const POSITION_OWNER: usize = 40;
const POSITION_BIN_ID: usize = 72;
const POSITION_SHARES: usize = 76;

/// maverick's errors, in `ErrorCode`'s order.
const ERR_SLIPPAGE_EXCEEDED: u32 = 6003;
const ERR_INSUFFICIENT_LIQUIDITY: u32 = 6004;
const ERR_INSUFFICIENT_SHARES: u32 = 6007;
const ERR_POOL_PAUSED: u32 = 6009;
const ERR_NO_BIN_ARRAYS: u32 = 6011;
const ERR_NO_STATIC_BASELINE: u32 = 6018;
const ERR_STATIC_CANNOT_MOVE: u32 = 6019;
const ERR_WRONG_MOVE_DIRECTION: u32 = 6020;
const ERR_BIN_STILL_IN_RANGE: u32 = 6021;
const ERR_BIN_KIND_MISMATCH: u32 = 6023;
const ERR_BIN_ALREADY_MOVED: u32 = 6024;
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

/// One bin's state out of its array's account data. `moved_to` is the hop the
/// bin recorded when it was merged away, if it ever was.
struct Bin {
    price: u128,
    reserve_a: u64,
    reserve_b: u64,
    supply: u128,
    kind: u8,
    moved_to: Option<(i32, u128)>,
}

fn bin(svm: &LiteSVM, pool: Address, bin_id: i32) -> Bin {
    let data = data_of(svm, bin_array_pda(pool, bin_array_index(bin_id)));
    let at = ARRAY_BINS + bin_offset(bin_id) * BIN_LEN;
    let moved_to = if data[at + BIN_MOVED_TO] == 1 {
        Some((
            i32_at(&data, at + MOVED_TO_BIN_ID),
            u128_at(&data, at + MOVED_TO_RATIO),
        ))
    } else {
        None
    };
    Bin {
        price: u128_at(&data, at + BIN_PRICE),
        reserve_a: u64_at(&data, at + BIN_RESERVE_A),
        reserve_b: u64_at(&data, at + BIN_RESERVE_B),
        supply: u128_at(&data, at + BIN_SUPPLY),
        kind: data[at + BIN_KIND],
        moved_to,
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
        let mut data = IX_INITIALIZE_BIN_ARRAY.to_vec();
        data.extend_from_slice(&index.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(bin_array_pda(pool, index), false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
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
        kind: u8,
        amount_a: u64,
        amount_b: u64,
    ) -> Instruction {
        let array_index = bin_array_index(bin_id);
        let mut data = IX_ADD_LIQUIDITY.to_vec();
        data.extend_from_slice(&array_index.to_le_bytes());
        data.extend_from_slice(&bin_id.to_le_bytes());
        data.push(kind);
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

    /// `remove_liquidity` follows the position's move chain, so the caller has to
    /// hand it the bin arrays that chain runs through - one for every hop whose
    /// bin lives outside the position's own array.
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
        hop_arrays: &[i64],
    ) -> Instruction {
        let array_index = bin_array_index(bin_id);
        let mut data = IX_REMOVE_LIQUIDITY.to_vec();
        data.extend_from_slice(&array_index.to_le_bytes());
        data.extend_from_slice(&bin_id.to_le_bytes());
        data.extend_from_slice(&shares.to_le_bytes());
        data.extend_from_slice(&min_amount_a.to_le_bytes());
        data.extend_from_slice(&min_amount_b.to_le_bytes());
        let mut accounts = vec![
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
        ];
        for index in hop_arrays {
            accounts.push(AccountMeta::new(bin_array_pda(pool, *index), false));
        }
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
            data,
        }
    }

    /// `move_bins` takes the destination's array as a remaining account when it
    /// is not the source's.
    fn move_bins(
        caller: Address,
        pool: Address,
        from_bin_id: i32,
        to_bin_id: i32,
        other_arrays: &[i64],
    ) -> Instruction {
        let from_array_index = bin_array_index(from_bin_id);
        let mut data = IX_MOVE_BINS.to_vec();
        data.extend_from_slice(&from_array_index.to_le_bytes());
        data.extend_from_slice(&from_bin_id.to_le_bytes());
        data.extend_from_slice(&to_bin_id.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(bin_array_pda(pool, from_array_index), false),
            AccountMeta::new_readonly(caller, true),
        ];
        for index in other_arrays {
            accounts.push(AccountMeta::new(bin_array_pda(pool, *index), false));
        }
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
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
        let mut data = IX_SET_PAUSED.to_vec();
        data.push(u8::from(paused));
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    fn set_authority(pool: Address, authority: Address, new_authority: Address) -> Instruction {
        let mut data = IX_SET_AUTHORITY.to_vec();
        data.extend_from_slice(new_authority.as_ref());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
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
    load_program(&mut svm, PROGRAM_ID, "maverick");
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
    fn open_bin_array(&mut self, index: i64) {
        let instruction =
            Instructions::initialize_bin_array(self.payer.pubkey(), self.pool, index);
        send(&mut self.svm, &self.payer, &[instruction]);
    }

    fn deposit(&mut self, bin_id: i32, kind: u8, amount_a: u64, amount_b: u64) {
        let instruction = Instructions::add_liquidity(
            self.payer.pubkey(),
            self.pool,
            self.mint_a,
            self.mint_b,
            self.user_token_a,
            self.user_token_b,
            bin_id,
            kind,
            amount_a,
            amount_b,
        );
        send(&mut self.svm, &self.payer, &[instruction]);
    }

    fn move_bins(&mut self, from_bin_id: i32, to_bin_id: i32, other_arrays: &[i64]) {
        let instruction = Instructions::move_bins(
            self.payer.pubkey(),
            self.pool,
            from_bin_id,
            to_bin_id,
            other_arrays,
        );
        send(&mut self.svm, &self.payer, &[instruction]);
    }

    fn move_bins_error(&mut self, from_bin_id: i32, to_bin_id: i32, other: &[i64]) -> u32 {
        let instruction = Instructions::move_bins(
            self.payer.pubkey(),
            self.pool,
            from_bin_id,
            to_bin_id,
            other,
        );
        send_error(&mut self.svm, &self.payer, &[instruction])
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

    fn withdraw(&mut self, bin_id: i32, shares: u128, hop_arrays: &[i64]) {
        let instruction = Instructions::remove_liquidity(
            self.payer.pubkey(),
            self.pool,
            self.mint_a,
            self.mint_b,
            self.user_token_a,
            self.user_token_b,
            bin_id,
            shares,
            0,
            0,
            hop_arrays,
        );
        send(&mut self.svm, &self.payer, &[instruction]);
    }

    fn position_shares(&self, bin_id: i32) -> u128 {
        let position = position_pda(self.pool, self.payer.pubkey(), bin_id);
        u128_at(&data_of(&self.svm, position), POSITION_SHARES)
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

    fn pool_data(&self) -> Vec<u8> {
        data_of(&self.svm, self.pool)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn initialize_pool_creates_its_vaults_and_a_fully_priced_bin_array() {
    let f = setup(BIN_STEP, Wiring::default());

    let pool = f.pool_data();
    assert_eq!(address_at(&pool, POOL_MINT_A), f.mint_a);
    assert_eq!(address_at(&pool, POOL_MINT_B), f.mint_b);
    assert_eq!(u16_at(&pool, POOL_BIN_STEP), BIN_STEP);
    assert_eq!(u16_at(&pool, POOL_BASE_FEE_BPS), BASE_FEE_BPS);
    assert_eq!(i32_at(&pool, POOL_ACTIVE_BIN), ACTIVE_BIN);
    assert_eq!(address_at(&pool, POOL_AUTHORITY), f.payer.pubkey());
    assert!(!bool_at(&pool, POOL_PAUSED));
    assert_eq!(u8_at(&pool, POOL_DECIMALS_A), 6);
    // No LP has anchored a fixed price yet, so directional bins are not on
    // offer until one does.
    assert!(!bool_at(&pool, POOL_STATIC_BASELINE));
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
    }

    // The first bin array exists, belongs to the pool, and prices every bin.
    let array = data_of(&f.svm, bin_array_pda(f.pool, ARRAY));
    assert_eq!(address_at(&array, ARRAY_POOL), f.pool);
    assert_eq!(i64_at(&array, ARRAY_INDEX), ARRAY);
    assert_eq!(u32_at(&array, ARRAY_BIN_COUNT), BINS_PER_ARRAY as u32);
    assert_eq!(bin(&f.svm, f.pool, ACTIVE_BIN).price, Q64);

    // Every bin starts out static and unmoved: the kind belongs to the LP who
    // funds it, and nothing has moved before it holds anything.
    let mut previous = 0u128;
    for id in 0..BINS_PER_ARRAY as i32 {
        let bin = bin(&f.svm, f.pool, id);
        assert!(bin.price > previous, "bin prices must rise with the bin id");
        assert_eq!(bin.kind, KIND_STATIC);
        assert!(bin.moved_to.is_none());
        assert_eq!(bin.supply, 0);
        previous = bin.price;
    }
}

#[test]
fn a_static_bin_takes_liquidity_and_its_lp_withdraws_the_same_value() {
    let mut f = setup(BIN_STEP, Wiring::default());
    let (amount_a, amount_b) = (400_000_000u64, 600_000_000u64);

    f.deposit(ACTIVE_BIN, KIND_STATIC, amount_a, amount_b);

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

    let funded = bin(&f.svm, f.pool, ACTIVE_BIN);
    assert_eq!(funded.reserve_a, amount_a);
    assert_eq!(funded.reserve_b, amount_b);
    assert_eq!(funded.supply, shares);
    assert_eq!(funded.kind, KIND_STATIC);
    assert_eq!(vault_balance(&f.svm, f.pool, f.mint_a, b"vault_a"), amount_a);
    assert_eq!(vault_balance(&f.svm, f.pool, f.mint_b, b"vault_b"), amount_b);
    // Funding a static bin is what anchors the pool's baseline.
    assert!(bool_at(&f.pool_data(), POOL_STATIC_BASELINE));

    // Nothing has traded and nothing has moved, so a full withdrawal pays the
    // deposit back exactly.
    f.withdraw(ACTIVE_BIN, shares, &[]);
    assert_eq!(f.balance_a(), FUNDS);
    assert_eq!(f.balance_b(), FUNDS);
    let emptied = bin(&f.svm, f.pool, ACTIVE_BIN);
    assert_eq!(emptied.reserve_a, 0);
    assert_eq!(emptied.reserve_b, 0);
    assert_eq!(emptied.supply, 0);

    // A position that no longer holds shares cannot be paid twice.
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
        &[],
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_INSUFFICIENT_SHARES
    );
}

#[test]
fn directional_liquidity_needs_a_static_baseline_and_the_bin_keeps_its_kind() {
    let mut f = setup(BIN_STEP, Wiring::default());

    // A directional bin is a promise that its liquidity may be moved away, so
    // it is only on offer once a fixed-price bin anchors the pool.
    let instruction = Instructions::add_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        1,
        KIND_RIGHT,
        100_000_000,
        100_000_000,
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_NO_STATIC_BASELINE
    );

    f.deposit(0, KIND_STATIC, 100_000_000, 100_000_000);
    assert!(bool_at(&f.pool_data(), POOL_STATIC_BASELINE));

    // Now it is, and the bin records the kind its LP funded.
    f.deposit(1, KIND_RIGHT, 100_000_000, 100_000_000);
    assert_eq!(bin(&f.svm, f.pool, 1).kind, KIND_RIGHT);

    // A bin's kind belongs to the bin: a later LP cannot quietly change the
    // terms of the liquidity already in it.
    let instruction = Instructions::add_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        1,
        KIND_STATIC,
        1_000_000,
        1_000_000,
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_BIN_KIND_MISMATCH
    );
}

#[test]
fn a_swap_fills_the_active_bin_and_crosses_the_empty_bins_between() {
    let mut f = setup(BIN_STEP, Wiring::default());

    let (thin_a, thin_b) = (1_000_000_000u64, 100_000_000u64);
    let (deep_a, deep_b) = (1_000_000_000u64, 1_000_000_000u64);
    f.deposit(0, KIND_STATIC, thin_a, thin_b);
    f.deposit(2, KIND_STATIC, deep_a, deep_b);

    let amount_in = 500_000_000u64;
    let before = f.balance_b();
    f.swap(amount_in, 0, true, &[ARRAY]);

    assert_eq!(f.active_bin(), 2, "the active bin follows the trade");
    let start = bin(&f.svm, f.pool, 0);
    let end = bin(&f.svm, f.pool, 2);
    assert_eq!(start.reserve_b, 0, "a drained bin is left holding no output");
    assert_eq!(bin(&f.svm, f.pool, 1).reserve_a, 0, "an empty bin is skipped");

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
    assert_eq!(f.active_bin(), 2);
}

#[test]
fn a_static_bin_never_moves_and_a_move_must_go_toward_the_price() {
    let mut f = setup(BIN_STEP, Wiring::default());
    f.deposit(0, KIND_STATIC, 1_000_000_000, 1_000_000_000);
    // A directional bin above the price, which only ever moves left.
    f.deposit(1, KIND_RIGHT, 1_000_000_000, 1_000_000_000);

    // A static bin's whole promise is its fixed price, so it is refused first,
    // whatever else is wrong with the move.
    assert_eq!(
        f.move_bins_error(0, 0 - 1, &[-1]),
        ERR_STATIC_CANNOT_MOVE
    );

    // A right bin only follows a rising price, so a move to the left is refused
    // before the range and destination checks are reached.
    assert_eq!(f.move_bins_error(1, 0, &[]), ERR_WRONG_MOVE_DIRECTION);

    // Moving right is the right direction for it - but the bin is above the
    // active bin, so it is still in range and there is nothing to carry it to.
    assert_eq!(f.move_bins_error(1, 2, &[]), ERR_BIN_STILL_IN_RANGE);

    // Nothing above moved, and the pool is unchanged.
    assert_eq!(bin(&f.svm, f.pool, 1).moved_to, None);
    assert_eq!(f.active_bin(), 0);
}

#[test]
fn move_bins_carries_liquidity_to_the_price_and_its_lp_withdraws_along_the_chain() {
    let mut f = setup(BIN_STEP, Wiring::default());
    f.deposit(0, KIND_STATIC, 1_000_000_000, 1_000_000_000);

    // The source bin lives two arrays below the active bin, so the deposit has
    // to arrive before the move can be made. `Both` follows the price either
    // way.
    f.open_bin_array(-1);
    f.open_bin_array(-2);
    let (amount_a, amount_b) = (100_000_000u64, 100_000_000u64);
    f.deposit(-60, KIND_BOTH, amount_a, amount_b);
    assert_eq!(f.balance_a(), FUNDS - 1_000_000_000 - amount_a);

    // A move is permissionless and carries the source's liquidity one hop
    // toward the price: out of array -2 and into array -1.
    f.move_bins(-60, -21, &[-1]);
    let moved = bin(&f.svm, f.pool, -60);
    assert_eq!(moved.reserve_a, 0, "the bin it left holds nothing");
    assert_eq!(moved.reserve_b, 0);
    assert_eq!(moved.supply, 1_000_000_000 + 1_000_000_000, "its ledger stays");
    let (hop, _) = moved.moved_to.expect("the hop is recorded for its LPs");
    assert_eq!(hop, -21);
    // The destination adopted the kind of the liquidity it took on.
    let first_hop = bin(&f.svm, f.pool, -21);
    assert_eq!((first_hop.reserve_a, first_hop.reserve_b), (amount_a, amount_b));
    assert_eq!(first_hop.kind, KIND_BOTH);

    // And then a second hop, so the chain the LP has to be followed along is
    // more than one link long.
    f.move_bins(-21, -1, &[]);
    let (hop, _) = bin(&f.svm, f.pool, -21)
        .moved_to
        .expect("the second hop is recorded too");
    assert_eq!(hop, -1);
    assert_eq!(bin(&f.svm, f.pool, -1).reserve_a, amount_a);

    // The LP deposited into bin -60. Its liquidity is two hops away, in another
    // bin array, and the withdrawal follows the chain to it - all in one call,
    // with the array the chain runs through as its only remaining account.
    let shares = f.position_shares(-60);
    f.withdraw(-60, shares, &[-1]);

    assert_eq!(f.position_shares(-60), 0);
    let terminal = bin(&f.svm, f.pool, -1);
    assert_eq!(terminal.supply, 0, "the terminal bin's shares are all burned");
    assert!(terminal.reserve_a <= 1 && terminal.reserve_b <= 1);
    // The LP is made whole: a move re-prices liquidity, it never takes any of
    // it, so the two hops cost at most a rounding unit each.
    let returned_a = FUNDS - f.balance_a();
    let returned_b = FUNDS - f.balance_b();
    assert!(returned_a >= amount_a, "a move must not lose the LP's A");
    assert!(returned_b >= amount_b, "a move must not lose the LP's B");
    assert!(returned_a - amount_a <= 2, "at most a rounding unit of A");

    let _ = returned_b;
}

#[test]
fn a_moved_bin_cannot_move_again_or_take_deposits() {
    let mut f = setup(BIN_STEP, Wiring::default());
    f.deposit(0, KIND_STATIC, 1_000_000_000, 1_000_000_000);
    f.deposit(-1, KIND_BOTH, 100_000_000, 100_000_000);
    f.move_bins(-1, 0, &[]);

    // Its liquidity is in another bin now, so nothing may be deposited into it
    // and it may not be moved a second time.
    let instruction = Instructions::add_liquidity(
        f.payer.pubkey(),
        f.pool,
        f.mint_a,
        f.mint_b,
        f.user_token_a,
        f.user_token_b,
        -1,
        KIND_BOTH,
        1_000_000,
        1_000_000,
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_BIN_ALREADY_MOVED
    );
    assert_eq!(f.move_bins_error(-1, 0, &[]), ERR_BIN_ALREADY_MOVED);
}

#[test]
fn only_the_authority_can_pause_the_pool_and_a_pause_never_traps_liquidity() {
    let mut f = setup(BIN_STEP, Wiring::default());
    f.deposit(0, KIND_STATIC, 1_000_000_000, 1_000_000_000);
    f.deposit(-1, KIND_BOTH, 100_000_000, 100_000_000);

    // A stranger holding no role is refused by the pool's `has_one`. It pays
    // for its own attempt so that it can sign it.
    let stranger = Keypair::new();
    f.svm.airdrop(&stranger.pubkey(), LAMPORTS).unwrap();
    let instruction = Instructions::set_paused(f.pool, stranger.pubkey(), true);
    let (code, _) = send_error_logs(&mut f.svm, &stranger, &[instruction]);
    assert_eq!(code, ERR_CONSTRAINT_HAS_ONE);

    let instruction = Instructions::set_paused(f.pool, f.payer.pubkey(), true);
    send(&mut f.svm, &f.payer, &[instruction]);
    assert!(bool_at(&f.pool_data(), POOL_PAUSED));

    // Paused: no swaps, no deposits and no moves...
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
        KIND_STATIC,
        1_000_000,
        1_000_000,
    );
    assert_eq!(
        send_error(&mut f.svm, &f.payer, &[instruction]),
        ERR_POOL_PAUSED
    );
    assert_eq!(f.move_bins_error(-1, 0, &[]), ERR_POOL_PAUSED);

    // ...but a withdrawal still works: a pause must never trap LP funds.
    let shares = f.position_shares(0);
    f.withdraw(0, shares, &[]);
    assert_eq!(f.balance_a(), FUNDS - 100_000_000);
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
    f.deposit(0, KIND_STATIC, 1_000_000_000, 10_000_000);
    f.deposit(5, KIND_STATIC, 1_000_000_000, 1_000_000_000);
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
    // A refused trade writes nothing.
    assert_eq!(f.active_bin(), active);
    let (recorded_at, recorded_price, _) = newest_observation(&f.svm, oracle);
    assert_eq!(
        (recorded_at, recorded_price),
        (opened_at + WINDOW_SECONDS, bin(&f.svm, f.pool, 5).price)
    );
}

#[test]
fn debug_dump() {
    let f = setup(WIRED_BIN_STEP, Wiring::default());
    let array = data_of(&f.svm, bin_array_pda(f.pool, ARRAY));
    println!("array len = {}", array.len());
    for id in 0..8 {
        let at = ARRAY_BINS + bin_offset(id) * BIN_LEN;
        println!(
            "id {id}: price={} ra={} rb={} supply={} kind={} opt={}",
            u128_at(&array, at),
            u64_at(&array, at + 16),
            u64_at(&array, at + 24),
            u128_at(&array, at + 32),
            array[at + 48],
            array[at + 49],
        );
    }
    println!("pool len = {}", f.pool_data().len());
    println!("bytes 110..120 = {:?}", &f.pool_data()[110..120]);
}
