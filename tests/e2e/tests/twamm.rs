//! End-to-end tests for the TWAMM program: the compiled program on a LiteSVM,
//! against real SPL token accounts.
//!
//! The crate's unit tests cover the netting and accumulator math. These drive
//! the instruction handlers a keeper or desk actually calls: pool creation with
//! genesis liquidity, add/remove liquidity, `create_long_term_order`, the
//! permissionless `execute_virtual_orders` crank once an order expires,
//! `withdraw_proceeds`, and the pre-expiry `cancel_order` refund. Nothing in
//! this suite touches the oracle/KYC/settlement rails; those are wired pools'
//! concern and stay at their program-ID sentinels here.
//!
//! ```bash
//! ./scripts/build-programs.sh
//! cargo test --manifest-path tests/e2e/Cargo.toml --test twamm
//! ```

mod common;

use common::*;

/// The program under test; must match `declare_id!`.
const PROGRAM_ID: Address = address!("FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9");
const TOKEN_PROGRAM_ID: Address = address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// Anchor instruction discriminators from `target/idl/twamm.json`.
const IX_INITIALIZE_POOL: [u8; 8] = [95, 180, 10, 172, 84, 174, 232, 40];
const IX_ADD_LIQUIDITY: [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];
const IX_REMOVE_LIQUIDITY: [u8; 8] = [80, 85, 209, 72, 24, 206, 177, 108];
const IX_CREATE_LONG_TERM_ORDER: [u8; 8] = [55, 100, 62, 109, 101, 99, 74, 132];
const IX_EXECUTE_VIRTUAL_ORDERS: [u8; 8] = [250, 182, 236, 171, 48, 20, 207, 217];
const IX_WITHDRAW_PROCEEDS: [u8; 8] = [124, 68, 215, 12, 201, 136, 54, 72];
const IX_CANCEL_ORDER: [u8; 8] = [95, 129, 237, 240, 8, 49, 223, 132];

/// `Pool` layout (borsh, tightly packed after the discriminator).
const POOL_MINT_A: usize = 8;
const POOL_MINT_B: usize = 40;
const POOL_RESERVE_A: usize = 72;
const POOL_RESERVE_B: usize = 80;
const POOL_ORDER_HEAD: usize = 144;

/// `LpPosition` layout: pool, owner, shares.
const LP_SHARES: usize = 72;

/// `LongTermOrder` layout: pool, owner, id, side, rate, amount, sold, duration,
/// created, expiry, accumulator snapshot, proceeds, settled, next, next expiry,
/// bump.
const ORDER_ID: usize = 72;
const ORDER_SIDE: usize = 80;
const ORDER_AMOUNT_IN: usize = 89;
const ORDER_SOLD: usize = 97;
const ORDER_EXPIRY: usize = 121;
const ORDER_PROCEEDS: usize = 145;
const ORDER_SETTLED: usize = 153;
const ORDER_NEXT_EXPIRY: usize = 186;

/// The fixture's genesis amounts and the funded balance of each owner ATA.
const GENESIS_A: u64 = 1_000_000_000;
const GENESIS_B: u64 = 2_000_000_000;
const FUNDS: u64 = 10_000_000_000;

const ORDER_DURATION: i64 = 100;
const ORDER_AMOUNT: u64 = 10_000_000;

fn pool_pda(mint_a: Address, mint_b: Address) -> Address {
    Address::find_program_address(
        &[b"twamm-pool", mint_a.as_ref(), mint_b.as_ref()],
        &PROGRAM_ID,
    )
    .0
}

fn vault_a_pda(pool: Address, mint_a: Address) -> Address {
    Address::find_program_address(
        &[b"twamm-vault-a", pool.as_ref(), mint_a.as_ref()],
        &PROGRAM_ID,
    )
    .0
}

fn vault_b_pda(pool: Address, mint_b: Address) -> Address {
    Address::find_program_address(
        &[b"twamm-vault-b", pool.as_ref(), mint_b.as_ref()],
        &PROGRAM_ID,
    )
    .0
}

fn lp_pda(pool: Address, owner: Address) -> Address {
    Address::find_program_address(&[b"twamm-lp", pool.as_ref(), owner.as_ref()], &PROGRAM_ID).0
}

fn order_pda(pool: Address, owner: Address, order_id: u64) -> Address {
    Address::find_program_address(
        &[
            b"twamm-order",
            pool.as_ref(),
            owner.as_ref(),
            &order_id.to_le_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

struct Instructions;

impl Instructions {
    #[allow(clippy::too_many_arguments)]
    fn initialize_pool(
        payer: Address,
        mint_a: Address,
        mint_b: Address,
        initial_a: u64,
        initial_b: u64,
        authority: Address,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let token_a = associated_token_address(payer, mint_a);
        let token_b = associated_token_address(payer, mint_b);
        let mut data = IX_INITIALIZE_POOL.to_vec();
        data.extend_from_slice(&initial_a.to_le_bytes());
        data.extend_from_slice(&initial_b.to_le_bytes());
        data.extend_from_slice(authority.as_ref());
        data.extend_from_slice(Address::default().as_ref()); // kyc_config off
        data.extend_from_slice(&0u16.to_le_bytes()); // oracle off
        data.extend_from_slice(Address::default().as_ref()); // hooks off
        data.extend_from_slice(&0u64.to_le_bytes()); // settlement slots off
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(pool, false),
                AccountMeta::new(vault_a_pda(pool, mint_a), false),
                AccountMeta::new(vault_b_pda(pool, mint_b), false),
                AccountMeta::new(lp_pda(pool, payer), false),
                AccountMeta::new(token_a, false),
                AccountMeta::new(token_b, false),
                // Optional rail accounts, off via the program-ID sentinel.
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn add_liquidity(
        owner: Address,
        mint_a: Address,
        mint_b: Address,
        amount_a: u64,
        amount_b: u64,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_ADD_LIQUIDITY.to_vec();
        data.extend_from_slice(&amount_a.to_le_bytes());
        data.extend_from_slice(&amount_b.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(lp_pda(pool, owner), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_a_pda(pool, mint_a), false),
                AccountMeta::new(vault_b_pda(pool, mint_b), false),
                AccountMeta::new(associated_token_address(owner, mint_a), false),
                AccountMeta::new(associated_token_address(owner, mint_b), false),
                AccountMeta::new(owner, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn remove_liquidity(
        owner: Address,
        mint_a: Address,
        mint_b: Address,
        shares: u128,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_REMOVE_LIQUIDITY.to_vec();
        data.extend_from_slice(&shares.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(lp_pda(pool, owner), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_a_pda(pool, mint_a), false),
                AccountMeta::new(vault_b_pda(pool, mint_b), false),
                AccountMeta::new(associated_token_address(owner, mint_a), false),
                AccountMeta::new(associated_token_address(owner, mint_b), false),
                AccountMeta::new(owner, true),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn create_order(
        owner: Address,
        mint_a: Address,
        mint_b: Address,
        order_id: u64,
        amount_in: u64,
        duration: i64,
        is_a_to_b: bool,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_CREATE_LONG_TERM_ORDER.to_vec();
        data.extend_from_slice(&order_id.to_le_bytes());
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&duration.to_le_bytes());
        data.push(u8::from(is_a_to_b));
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(order_pda(pool, owner, order_id), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_a_pda(pool, mint_a), false),
                AccountMeta::new(vault_b_pda(pool, mint_b), false),
                AccountMeta::new(associated_token_address(owner, mint_a), false),
                AccountMeta::new(associated_token_address(owner, mint_b), false),
                AccountMeta::new(owner, true),
                // KYC / oracle rails are off: program-ID sentinels.
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn execute_virtual_orders(
        mint_a: Address,
        mint_b: Address,
        expired: &[Address],
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut accounts = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
            AccountMeta::new_readonly(PROGRAM_ID, false),
        ];
        for order in expired {
            accounts.push(AccountMeta::new(*order, false));
        }
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
            data: IX_EXECUTE_VIRTUAL_ORDERS.to_vec(),
        }
    }

    fn withdraw_proceeds(
        owner: Address,
        mint_a: Address,
        mint_b: Address,
        order_id: u64,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_WITHDRAW_PROCEEDS.to_vec();
        data.extend_from_slice(&order_id.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(order_pda(pool, owner, order_id), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_a_pda(pool, mint_a), false),
                AccountMeta::new(vault_b_pda(pool, mint_b), false),
                AccountMeta::new(associated_token_address(owner, mint_a), false),
                AccountMeta::new(associated_token_address(owner, mint_b), false),
                AccountMeta::new(owner, true),
                // Settlement rail off.
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(PROGRAM_ID, false),
                AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    fn cancel_order(
        owner: Address,
        mint_a: Address,
        mint_b: Address,
        order_id: u64,
        prefix: &[Address],
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_CANCEL_ORDER.to_vec();
        data.extend_from_slice(&order_id.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new(order_pda(pool, owner, order_id), false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a_pda(pool, mint_a), false),
            AccountMeta::new(vault_b_pda(pool, mint_b), false),
            AccountMeta::new(associated_token_address(owner, mint_a), false),
            AccountMeta::new(associated_token_address(owner, mint_b), false),
            AccountMeta::new(owner, true),
            // Oracle / settlement rails off.
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
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ];
        for order in prefix {
            accounts.push(AccountMeta::new(*order, false));
        }
        Instruction {
            program_id: PROGRAM_ID,
            accounts,
            data,
        }
    }
}

struct Fixture {
    svm: LiteSVM,
    payer: Keypair,
    mint_a: Address,
    mint_b: Address,
    pool: Address,
}

impl Fixture {
    fn token_a(&self) -> Address {
        associated_token_address(self.payer.pubkey(), self.mint_a)
    }

    fn token_b(&self) -> Address {
        associated_token_address(self.payer.pubkey(), self.mint_b)
    }

    fn vault_a(&self) -> Address {
        vault_a_pda(self.pool, self.mint_a)
    }

    fn vault_b(&self) -> Address {
        vault_b_pda(self.pool, self.mint_b)
    }

    fn order(&self, id: u64) -> Address {
        order_pda(self.pool, self.payer.pubkey(), id)
    }

    fn deposit(&mut self, amount_a: u64, amount_b: u64) {
        let ix = Instructions::add_liquidity(
            self.payer.pubkey(),
            self.mint_a,
            self.mint_b,
            amount_a,
            amount_b,
        );
        send(&mut self.svm, &self.payer, &[ix]);
    }
}

fn setup() -> Fixture {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "twamm");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();

    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    create_mint(&mut svm, payer.pubkey(), mint_a, 6);
    create_mint(&mut svm, payer.pubkey(), mint_b, 6);
    create_token_account(
        &mut svm,
        associated_token_address(payer.pubkey(), mint_a),
        mint_a,
        payer.pubkey(),
        FUNDS,
    );
    create_token_account(
        &mut svm,
        associated_token_address(payer.pubkey(), mint_b),
        mint_b,
        payer.pubkey(),
        FUNDS,
    );

    let pool = pool_pda(mint_a, mint_b);
    let initialize = Instructions::initialize_pool(
        payer.pubkey(),
        mint_a,
        mint_b,
        GENESIS_A,
        GENESIS_B,
        payer.pubkey(),
    );
    send(&mut svm, &payer, &[initialize]);

    Fixture {
        svm,
        payer,
        mint_a,
        mint_b,
        pool,
    }
}

#[test]
fn initialize_pool_seeds_the_vaults_and_the_genesis_position() {
    let mut f = setup();

    assert_eq!(address_at(&data_of(&f.svm, f.pool), POOL_MINT_A), f.mint_a);
    assert_eq!(address_at(&data_of(&f.svm, f.pool), POOL_MINT_B), f.mint_b);
    assert_eq!(u64_at(&data_of(&f.svm, f.pool), POOL_RESERVE_A), GENESIS_A);
    assert_eq!(u64_at(&data_of(&f.svm, f.pool), POOL_RESERVE_B), GENESIS_B);
    assert_eq!(
        address_at(&data_of(&f.svm, f.pool), POOL_ORDER_HEAD),
        Address::default()
    );

    assert_eq!(token_balance(&f.svm, f.vault_a()), GENESIS_A);
    assert_eq!(token_balance(&f.svm, f.vault_b()), GENESIS_B);
    assert_eq!(token_owner(&f.svm, f.vault_a()), f.pool);
    assert_eq!(token_owner(&f.svm, f.vault_b()), f.pool);
    assert_eq!(token_balance(&f.svm, f.token_a()), FUNDS - GENESIS_A);
    assert_eq!(token_balance(&f.svm, f.token_b()), FUNDS - GENESIS_B);

    let position = data_of(&f.svm, lp_pda(f.pool, f.payer.pubkey()));
    assert_eq!(address_at(&position, 8), f.pool);
    assert_eq!(address_at(&position, 40), f.payer.pubkey());
    assert!(u128_at(&position, LP_SHARES) > 0);

    // A deposit mints additional shares and moves exactly what it says.
    let before = u128_at(
        &data_of(&f.svm, lp_pda(f.pool, f.payer.pubkey())),
        LP_SHARES,
    );
    f.deposit(1_000_000, 2_000_000);
    let after = u128_at(
        &data_of(&f.svm, lp_pda(f.pool, f.payer.pubkey())),
        LP_SHARES,
    );
    assert!(after > before);
    assert_eq!(token_balance(&f.svm, f.vault_a()), GENESIS_A + 1_000_000);
    assert_eq!(token_balance(&f.svm, f.vault_b()), GENESIS_B + 2_000_000);
}

#[test]
fn an_lp_can_remove_liquidity_and_receive_its_share() {
    let mut f = setup();
    let shares = u128_at(
        &data_of(&f.svm, lp_pda(f.pool, f.payer.pubkey())),
        LP_SHARES,
    );

    let remove = Instructions::remove_liquidity(f.payer.pubkey(), f.mint_a, f.mint_b, shares);
    send(&mut f.svm, &f.payer, &[remove]);

    assert_eq!(token_balance(&f.svm, f.vault_a()), 0);
    assert_eq!(token_balance(&f.svm, f.vault_b()), 0);
    assert_eq!(token_balance(&f.svm, f.token_a()), FUNDS);
    assert_eq!(token_balance(&f.svm, f.token_b()), FUNDS);
}

#[test]
fn an_order_runs_to_expiry_and_withdraws_its_proceeds() {
    let mut f = setup();
    let order_id = 1;
    let created = now(&f.svm);

    let create = Instructions::create_order(
        f.payer.pubkey(),
        f.mint_a,
        f.mint_b,
        order_id,
        ORDER_AMOUNT,
        ORDER_DURATION,
        true,
    );
    send(&mut f.svm, &f.payer, &[create]);

    let order = data_of(&f.svm, f.order(order_id));
    assert_eq!(u64_at(&order, ORDER_ID), order_id);
    assert_eq!(order[ORDER_SIDE], 1);
    assert_eq!(u64_at(&order, ORDER_AMOUNT_IN), ORDER_AMOUNT);
    assert_eq!(
        i64_at(&order, ORDER_EXPIRY),
        created + ORDER_DURATION,
        "expiry is creation time plus the duration"
    );
    assert_eq!(u64_at(&order, ORDER_SOLD), 0);
    assert_eq!(order[ORDER_SETTLED], 0);
    assert_eq!(
        token_balance(&f.svm, f.vault_a()),
        GENESIS_A + ORDER_AMOUNT,
        "the whole order is escrowed at creation"
    );

    // Crank only once the order has expired. The solver sells the full input
    // into the embedded AMM and fixes the order's proceeds.
    set_time(&mut f.svm, created + ORDER_DURATION + 1);
    let execute = Instructions::execute_virtual_orders(f.mint_a, f.mint_b, &[f.order(order_id)]);
    send(&mut f.svm, &f.payer, &[execute]);

    let order = data_of(&f.svm, f.order(order_id));
    assert_eq!(order[ORDER_SETTLED], 1);
    assert_eq!(u64_at(&order, ORDER_SOLD), ORDER_AMOUNT);
    let proceeds = u64_at(&order, ORDER_PROCEEDS);
    assert!(proceeds > 0, "selling A must produce B");
    assert_eq!(u64_at(&order, ORDER_NEXT_EXPIRY), 0);

    // The owner claims the output; the order account stays (settled) so a
    // second claim cannot double-pay.
    let before_b = token_balance(&f.svm, f.token_b());
    let withdraw = Instructions::withdraw_proceeds(f.payer.pubkey(), f.mint_a, f.mint_b, order_id);
    send(&mut f.svm, &f.payer, &[withdraw]);
    assert_eq!(token_balance(&f.svm, f.token_b()), before_b + proceeds);

    assert!(
        !exists(&f.svm, f.order(order_id)),
        "withdrawal closes the order"
    );
    assert_eq!(token_balance(&f.svm, f.token_b()), before_b + proceeds);
}

#[test]
fn cancelling_before_expiry_refunds_the_unsold_input_and_closes_the_order() {
    let mut f = setup();
    let order_id = 2;
    let before_a = token_balance(&f.svm, f.token_a());

    let create = Instructions::create_order(
        f.payer.pubkey(),
        f.mint_a,
        f.mint_b,
        order_id,
        ORDER_AMOUNT,
        ORDER_DURATION,
        true,
    );
    send(&mut f.svm, &f.payer, &[create]);
    assert_eq!(token_balance(&f.svm, f.token_a()), before_a - ORDER_AMOUNT);

    // No virtual flow has been executed, so the whole escrow is unsold.
    let cancel = Instructions::cancel_order(f.payer.pubkey(), f.mint_a, f.mint_b, order_id, &[]);
    send(&mut f.svm, &f.payer, &[cancel]);

    assert!(
        !exists(&f.svm, f.order(order_id)),
        "cancel closes the order"
    );
    assert_eq!(token_balance(&f.svm, f.token_a()), before_a);
    assert_eq!(token_balance(&f.svm, f.vault_a()), GENESIS_A);
    assert_eq!(
        address_at(&data_of(&f.svm, f.pool), POOL_ORDER_HEAD),
        Address::default(),
        "the cancelled order leaves the list head empty"
    );
}
