//! Harness shared by the AMM end-to-end suites.
//!
//! Each suite loads one or more compiled programs into a `LiteSVM` and drives
//! them with real signed transactions, so these helpers cover what every suite
//! needs on the way in: mints and token accounts written by hand (which works
//! whichever SPL token implementation the VM ships), blockhashes that never
//! collide, and the clock, so a test can reach the far end of an oracle window.
//!
//! Nothing here is linked into, or deployed with, the on-chain programs.

// Each suite pulls a different subset of these; the ones it does not use are
// still part of the shared harness.
#![allow(dead_code)]

pub use litesvm::LiteSVM;
pub use solana_account::Account;
pub use solana_address::{address, Address};
pub use solana_clock::Clock;
pub use solana_instruction::{AccountMeta, Instruction};
pub use solana_keypair::Keypair;
pub use solana_signer::Signer;
pub use solana_transaction::Transaction;

/// The programs the AMMs wire themselves to, from each crate's `declare_id!`.
pub const ORACLE_PROGRAM_ID: Address = address!("4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj");
pub const KYC_PROGRAM_ID: Address = address!("7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1");
pub const HOOKS_PROGRAM_ID: Address = address!("3DN93k4QRu7Ds2aL2Bba5VnDu18EoRzTNV18jJizAdoY");
pub const SETTLEMENT_PROGRAM_ID: Address =
    address!("49ppDVbrZSqwLDj8sitSapEsMMRfC2Z7wCeMAwhPVQoY");

pub const TOKEN_PROGRAM_ID: Address = address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_ID: Address = address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Address =
    address!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const BPF_LOADER_UPGRADEABLE_ID: Address =
    address!("BPFLoaderUpgradeab1e11111111111111111111111");
pub const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// SPL token layouts: a mint is 82 bytes with its `decimals` and `initialized`
/// flag at the end, an account is 165 bytes with its amount at offset 64.
pub const MINT_DECIMALS: usize = 44;
pub const MINT_INITIALIZED: usize = 45;
pub const TOKEN_AMOUNT_OFFSET: usize = 64;

/// Lamports for a funded payer, and for the small accounts a test writes by
/// hand: enough to be rent-exempt.
pub const LAMPORTS: u64 = 10_000_000_000;
pub const ACCOUNT_LAMPORTS: u64 = 5_000_000;
/// What each of a fixture's two token accounts starts with.
pub const FUNDS: u64 = 1_000_000_000_000;

/// Loads a program's built SBF image from `target/deploy`. `name` is the file
/// stem, which is also the directory under `programs/`.
pub fn load_program(svm: &mut LiteSVM, id: Address, name: &str) {
    let path = format!(
        "{}/../../target/deploy/{name}.so",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|_| panic!("build the programs first: `anchor build` (missing {path})"));
    svm.add_program(id, &bytes).unwrap();
}

// -----------------------------------------------------------------------
// Transactions
// -----------------------------------------------------------------------

pub fn send(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) {
    send_with(svm, payer, &[payer], instructions);
}

/// The same, with extra signers: creating an account also needs that account's
/// own key to sign.
pub fn send_with(
    svm: &mut LiteSVM,
    payer: &Keypair,
    signers: &[&Keypair],
    instructions: &[Instruction],
) {
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        signers,
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
pub fn send_error(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) -> u32 {
    send_error_logs(svm, payer, instructions).0
}

/// The same, but keeping the logs, so a test can show *which* program refused:
/// a check that a CPI reached the oracle is the oracle's own error text.
pub fn send_error_logs(
    svm: &mut LiteSVM,
    payer: &Keypair,
    instructions: &[Instruction],
) -> (u32, String) {
    // The VM's blockhash is static, so a retry of an instruction already sent
    // would collide on signature (`AlreadyProcessed`) instead of executing.
    svm.expire_blockhash();
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
            // Anchor's own constraint errors live in 2000..3000 (a `has_one`
            // failure is 2001); the programs' own start at 6000.
            let code = (2000..3000)
                .chain(6000..6100)
                .find(|code| text.contains(&format!("Custom({code})")))
                .unwrap_or_else(|| {
                    panic!(
                        "no program error code in {text}\nlogs:\n{}",
                        failure.meta.logs.join("\n")
                    )
                });
            (code, failure.meta.logs.join("\n"))
        }
    }
}

/// Whether an account exists at all: a closed order account does not.
pub fn exists(svm: &LiteSVM, address: Address) -> bool {
    svm.get_account(&address).is_some()
}

// -----------------------------------------------------------------------
// Account reads
// -----------------------------------------------------------------------

pub fn data_of(svm: &LiteSVM, address: Address) -> Vec<u8> {
    svm.get_account(&address)
        .unwrap_or_else(|| panic!("account {address} does not exist"))
        .data
}

pub fn owner_of(svm: &LiteSVM, address: Address) -> Address {
    svm.get_account(&address)
        .unwrap_or_else(|| panic!("account {address} does not exist"))
        .owner
}

pub fn u8_at(data: &[u8], offset: usize) -> u8 {
    data[offset]
}

pub fn bool_at(data: &[u8], offset: usize) -> bool {
    data[offset] != 0
}

pub fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

pub fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

pub fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

pub fn u128_at(data: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap())
}

pub fn i32_at(data: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

pub fn i64_at(data: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

pub fn address_at(data: &[u8], offset: usize) -> Address {
    Address::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

pub fn token_balance(svm: &LiteSVM, address: Address) -> u64 {
    u64_at(&data_of(svm, address), TOKEN_AMOUNT_OFFSET)
}

pub fn token_mint(svm: &LiteSVM, address: Address) -> Address {
    address_at(&data_of(svm, address), 0)
}

pub fn token_owner(svm: &LiteSVM, address: Address) -> Address {
    address_at(&data_of(svm, address), 32)
}

/// The VM's clock, so the tests can reach the far end of the oracle's window.
pub fn now(svm: &LiteSVM) -> i64 {
    svm.get_sysvar::<Clock>().unix_timestamp
}

pub fn set_time(svm: &mut LiteSVM, timestamp: i64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp = timestamp;
    svm.set_sysvar::<Clock>(&clock);
}

// -----------------------------------------------------------------------
// Account writes
// -----------------------------------------------------------------------

pub fn set_raw_account(svm: &mut LiteSVM, address: Address, data: Vec<u8>, owner: Address) {
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

/// Writes an initialized mint. The data is packed by hand rather than by an
/// `initialize_mint` instruction, so the setup works whichever SPL token
/// implementation the VM has loaded (newer LiteSVM builds swap in a trimmed
/// token program that rejects some initialize tags).
pub fn create_mint(svm: &mut LiteSVM, authority: Address, mint: Address, decimals: u8) {
    let mut data = vec![0u8; 82];
    data[..4].copy_from_slice(&1u32.to_le_bytes()); // COption::Some(authority)
    data[4..36].copy_from_slice(authority.as_ref());
    data[MINT_DECIMALS] = decimals;
    data[MINT_INITIALIZED] = 1;
    set_raw_account(svm, mint, data, TOKEN_PROGRAM_ID);
}

/// Writes a token account owned by `owner` holding `amount`.
pub fn create_token_account(
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

/// A program's `ProgramData` account: `[program]` under the upgradeable loader.
pub fn program_data_pda(program: Address) -> Address {
    Address::find_program_address(&[program.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// Marks `authority` as `program`'s upgrade authority by writing the metadata the
/// loader keeps in the head of `ProgramData` - the 4-byte `ProgramData` tag, the
/// slot, an `Option` tag of 1 and the authority - leaving the program bytes
/// untouched. On a real cluster only an upgrade transaction can write this; here
/// it stands in for one.
pub fn set_upgrade_authority(svm: &mut LiteSVM, program: Address, authority: Address) {
    let program_data = program_data_pda(program);
    let mut account = svm
        .get_account(&program_data)
        .unwrap_or_else(|| panic!("add_program creates {program}'s ProgramData account"));
    account.data[0..4].copy_from_slice(&3u32.to_le_bytes()); // AccountType::ProgramData
    account.data[12] = 1; // Option::Some
    account.data[13..45].copy_from_slice(authority.as_ref());
    svm.set_account(program_data, account).unwrap();
}

// -----------------------------------------------------------------------
// Rail PDAs
// -----------------------------------------------------------------------

pub fn oracle_pda(pool: Address) -> Address {
    Address::find_program_address(&[b"oracle", pool.as_ref()], &ORACLE_PROGRAM_ID).0
}

pub fn kyc_config_pda() -> Address {
    Address::find_program_address(&[b"kyc-config"], &KYC_PROGRAM_ID).0
}

/// The hooks program's authority PDA: it owns the custody account a routed
/// output is paid into and pays for the escrow the settlement program creates.
pub fn hook_authority_pda() -> Address {
    Address::find_program_address(&[b"hook_authority"], &HOOKS_PROGRAM_ID).0
}

pub fn settlement_config_pda() -> Address {
    Address::find_program_address(&[b"config"], &SETTLEMENT_PROGRAM_ID).0
}

pub fn settlement_vault_pda(trade_id: u64) -> Address {
    Address::find_program_address(
        &[b"settlement", &trade_id.to_le_bytes()],
        &SETTLEMENT_PROGRAM_ID,
    )
    .0
}

/// A legacy-token associated token account address.
pub fn associated_token_address(owner: Address, mint: Address) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

// -----------------------------------------------------------------------
// The oracle program's own error codes, in `OracleError`'s order.
// -----------------------------------------------------------------------

pub const ERR_UNAUTHORIZED_POOL: u32 = 6000;
pub const ERR_PRICE_DEVIATION_EXCEEDED: u32 = 6003;

/// The text the oracle logs when the circuit breaker refuses a trade. It is the
/// proof that a refusal came from the oracle and not from the AMM's own math.
pub const DEVIATION_LOG: &str = "Price deviation exceeded maximum allowed threshold";

/// `OracleState` layout: pool, owner, flags, bump, threshold, then the
/// observation ring buffer. Each `Observation` is
/// `{ timestamp: i64, sqrt_price: u128, cumulative: u128 }`.
pub const STATE_POOL: usize = 8;
pub const STATE_INDEX: usize = 73;
pub const STATE_THRESHOLD: usize = 76;
pub const STATE_OBSERVATIONS: usize = 78;
pub const OBS_SIZE: usize = 40;
pub const OBS_TIMESTAMP: usize = 0;
pub const OBS_PRICE: usize = 8;

/// The newest observation in the oracle's ring buffer, as
/// `(timestamp, price, cumulative)`. The state caches its cursor, so the newest
/// entry is whatever `observation_index` points at.
pub fn newest_observation(svm: &LiteSVM, oracle: Address) -> (i64, u128, u128) {
    let state = data_of(svm, oracle);
    let index = state[STATE_INDEX] as usize;
    let at = STATE_OBSERVATIONS + index * OBS_SIZE;
    (
        i64_at(&state, at + OBS_TIMESTAMP),
        u128_at(&state, at + OBS_PRICE),
        u128_at(&state, at + OBS_SIZE - 16),
    )
}
