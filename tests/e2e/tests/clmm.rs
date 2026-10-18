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
    solana_clock::Clock,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_signer::Signer,
    solana_transaction::Transaction,
};

/// The program under test: this must match `declare_id!` in
/// `programs/clmm/src/lib.rs` (the client-side strings are the only place the
/// ID is repeated here, because these tests never link the program crate).
const PROGRAM_ID: Address = address!("GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU");
/// The oracle program, from `programs/oracle/src/lib.rs`. Only the wired-oracle
/// test loads it.
const ORACLE_PROGRAM_ID: Address = address!("4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj");
/// The rest of the rail stack: the KYC gate, the hooks program that locks the
/// output in escrow, and the settlement clearinghouse it CPIs into.
const KYC_PROGRAM_ID: Address = address!("7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1");
const HOOKS_PROGRAM_ID: Address = address!("B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA");
const SETTLEMENT_PROGRAM_ID: Address = address!("58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6");
/// Token-2022 (the KYC attestation mint is one, so the trader's account for it
/// has to be one) and the associated-token program, which the pool's swap lists
/// as part of its settlement rail and which builds the hook authority's custody
/// ATA.
const TOKEN_2022_ID: Address = address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ASSOCIATED_TOKEN_PROGRAM_ID: Address =
    address!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// The BPF upgradeable loader, which owns each program's `ProgramData` account:
/// where `kyc_gate::initialize_kyc_mint` and `settlement::initialize` read the
/// upgrade authority they are restricted to.
const BPF_LOADER_UPGRADEABLE_ID: Address =
    address!("BPFLoaderUpgradeab1e11111111111111111111111");

/// The rail programs' instruction discriminators, from their IDLs.
const IX_INITIALIZE_KYC_MINT: [u8; 8] = [9, 191, 40, 212, 165, 170, 113, 188];
const IX_ISSUE_KYC: [u8; 8] = [17, 74, 181, 26, 37, 126, 173, 108];
const IX_SETTLEMENT_INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
const IX_SET_HOOKS_AUTHORITY: [u8; 8] = [44, 238, 216, 211, 196, 234, 134, 241];
/// The system program's `CreateAccount` (discriminant 0) and Token-2022's
/// `InitializeAccount3`, which is what writes the extensions a non-transferable
/// mint requires onto a new account.
const IX_CREATE_ACCOUNT: u32 = 0;
const IX_INITIALIZE_ACCOUNT_3: u8 = 18;
/// Token-2022's `InitializeMint2` (decimals, authority, no rent sysvar), the
/// `TransferFeeExtension` prefix with its `InitializeTransferFeeConfig` tag, and
/// `MintTo` — what a fee-bearing mint and its funded account are built from.
const IX_INITIALIZE_MINT_2: u8 = 20;
const IX_TRANSFER_FEE_EXTENSION: u8 = 26;
const TRANSFER_FEE_INITIALIZE_CONFIG: u8 = 0;
const TRANSFER_FEE_SET: u8 = 5;
const IX_MINT_TO: u8 = 7;
/// An attestation account: the 165-byte base account, the account-type byte, and
/// the two zero-length extensions a non-transferable mint requires.
const KYC_ACCOUNT_LEN: usize = 165 + 1 + 4 + 4;
/// A mint that carries one `TransferFeeConfig` extension (a 108-byte entry: two
/// optional authorities, the withheld amount, and the older/newer fee), and the
/// token accounts a fee-bearing mint requires (`TransferFeeAmount`, an 8-byte
/// entry). The token program checks the length it is handed against
/// `ExtensionType::try_calculate_account_len` and initializes in place, so these
/// are that calculation written out: the base state padded out to the account
/// length (a mint with extensions is never shorter than a token account, so the
/// two cannot be confused), the account-type byte, and one TLV entry.
const FEE_MINT_LEN: usize = 165 + 1 + 4 + 108;
const FEE_ACCOUNT_LEN: usize = 165 + 1 + 4 + 8;

/// kyc_gate's own errors, from `ErrorCode` in order.
const ERR_NOT_KYC_APPROVED: u32 = 6001;
const ERR_KYC_WRONG_OWNER: u32 = 6003;
const TOKEN_PROGRAM_ID: Address = address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// Anchor instruction discriminators (`sha256("global:<name>")[..8]`), taken from
/// `target/idl/clmm.json` (a wrong one fails the first transaction loudly).
const IX_INITIALIZE_POOL: [u8; 8] = [95, 180, 10, 172, 84, 174, 232, 40];
const IX_ADD_LIQUIDITY: [u8; 8] = [181, 157, 89, 67, 143, 182, 52, 72];
const IX_REMOVE_LIQUIDITY: [u8; 8] = [80, 85, 209, 72, 24, 206, 177, 108];
const IX_COLLECT_FEES: [u8; 8] = [164, 152, 207, 99, 30, 186, 19, 182];
const IX_SWAP: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
const IX_TRIP_BREAKER: [u8; 8] = [141, 130, 124, 235, 77, 100, 183, 10];

/// SPL mint layout (82 bytes): authority, supply, decimals, initialized flag.
const MINT_DECIMALS: usize = 44;
const MINT_INITIALIZED: usize = 45;

/// Program errors this test asserts on, in the enum's order.
const ERR_INSUFFICIENT_SHARES: u32 = 6009;
const ERR_TICKS_NOT_IN_ORDER: u32 = 6013;
const ERR_TICK_SPACING_TOO_FINE: u32 = 6015;

const SPACING: i32 = 60;
const FEE_PIPS: u32 = 3_000;
/// The transfer fee the fee-mint fixture puts on its mint: 1%.
const FEE_BASIS_POINTS: u16 = 100;
/// The fixture's fee cap: high enough that no test transfer reaches it, so the
/// fee is always the percentage above.
const FEE_MAXIMUM: u64 = 1_000_000_000_000;
const MIN_TICK: i32 = -443_636;

/// The oracle rail's own fixture: the ranges are wide, so one swap can move the
/// price far enough that the next swap's price is a real deviation from the
/// TWAP built before it. `SQRT_PRICE_1` is the price the wired pool opens at.
const SQRT_PRICE_1: u128 = 1u128 << 64;
const WIRED_LOWER: i32 = -1_200;
const WIRED_UPPER: i32 = 1_200;
const WIRED_DEEP: i32 = -3_000;
/// What `check_deviation` tolerates, in sqrt-price basis points: the 4.88% the
/// wired pool is created with. A swap from `WIRED_LOWER` to `WIRED_DEEP` moves
/// the sqrt price ~8.6%, well past it.
const WIRED_THRESHOLD_BPS: u16 = 488;
/// The breaker's window, from `DEVIATION_WINDOW_SECONDS`.
const WINDOW_SECONDS: i64 = 1_800;
/// A swap large enough to reach both boundaries of the wired pool's ranges.
const WIRED_AMOUNT: u64 = 200_000_000_000;

/// `Pool` layout: the mints, then the sqrt price.
const POOL_SQRT_PRICE: usize = 72;
/// `OracleState` layout: pool, owner, flags, bump, threshold, observations.
/// Each `Observation` is `{ timestamp: i64, sqrt_price: u128, cumulative: u128 }`.
const STATE_INDEX: usize = 73;
const STATE_THRESHOLD: usize = 76;
const STATE_OBSERVATIONS: usize = 78;
const OBS_SIZE: usize = 40;
const OBS_TIMESTAMP: usize = 0;
const OBS_SQRT_PRICE: usize = 8;

/// The oracle's own errors, from `OracleError` in order.
const ERR_PRICE_DEVIATION_EXCEEDED: u32 = 6003;
/// clmm's error for a swap on a wired pool that did not pass the accounts its
/// policy demands.
const ERR_RAIL_ACCOUNT_UNEXPECTED: u32 = 6022;
const ERR_INVALID_CUSTODY_ACCOUNT: u32 = 6026;

/// The two ranges this test uses: one around the start price, one directly below
/// it, so a single A→B swap has to cross two ticks.
const LOWER: i32 = -120;
const UPPER: i32 = 120;
const DEEP: i32 = -240;

const LIQUIDITY: u128 = 1_000_000_000;
const FUNDS: u64 = 1_000_000_000_000;
const LAMPORTS: u64 = 10_000_000_000;
/// Enough for a small account to be rent-exempt.
const ACCOUNT_LAMPORTS: u64 = 5_000_000;

/// Token account layout (SPL): mint, owner, amount.
const TOKEN_AMOUNT_OFFSET: usize = 64;
/// Pool layout: discriminator, mints, sqrt price, liquidity, tick_current,
/// tick_spacing, fee_pips, two growth accumulators, authority (L-3), paused
/// (L-3), bump, bitmap cells, then the Borsh length prefix and bytes of the
/// bitmap.
const POOL_TICK_CURRENT: usize = 104;
const POOL_LIQUIDITY: usize = 88;
const POOL_BITMAP_CELLS: usize = 182;
const POOL_BITMAP_LEN: usize = 186;
const POOL_BITMAP: usize = 190;
/// The wiring tail after the bitmap bytes: `kyc_config`, `oracle_state` and
/// `hooks_authority` (32 bytes each) then `settlement_slots` (8).
const POOL_WIRING_TAIL: usize = 104;
/// Position layout: discriminator, pool, owner, lower, upper, liquidity, two fee
/// snapshots, then the two banked fee amounts.
const POSITION_OWNER: usize = 40;
const POSITION_LIQUIDITY: usize = 80;
const POSITION_OWED_A: usize = 128;

// -----------------------------------------------------------------------
// Test harness
// -----------------------------------------------------------------------

fn send(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) {
    send_with(svm, payer, &[payer], instructions);
}

/// The same, with extra signers: creating an attestation account, for instance,
/// needs that account's own key to sign.
fn send_with(
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
fn send_error(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) -> u32 {
    send_error_logs(svm, payer, instructions).0
}

/// The same, but keeping the logs, so a test can show *which* program refused:
/// a check that a CPI reached the oracle is the oracle's own error text.
fn send_error_logs(
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
            let code = (6000..6100)
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

fn i64_at(data: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

/// Moves the VM's clock forward by `epochs`. Token-2022 schedules a fee set with
/// `SetTransferFee` two epochs ahead, deliberately, so this is what makes it
/// effective without waiting real time.
fn warp_epochs(svm: &mut LiteSVM, epochs: u64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.epoch = clock.epoch.saturating_add(epochs);
    svm.set_sysvar::<Clock>(&clock);
}

/// The VM's clock, so the tests can reach the far end of the oracle's window.
fn set_time(svm: &mut LiteSVM, timestamp: i64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp = timestamp;
    svm.set_sysvar::<Clock>(&clock);
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
    create_mint_of(svm, authority, mint, TOKEN_PROGRAM_ID);
}

/// The same mint under either token program. A Token-2022 mint with no
/// extensions is exactly the legacy layout — the extensions live after the
/// 82-byte base, in a TLV area this does not write — so the only difference
/// that reaches the pool is the program that owns the account.
fn create_mint_of(svm: &mut LiteSVM, authority: Address, mint: Address, token_program: Address) {
    let mut data = vec![0u8; 82];
    data[..4].copy_from_slice(&1u32.to_le_bytes()); // COption::Some(authority)
    data[4..36].copy_from_slice(authority.as_ref());
    data[MINT_DECIMALS] = 6;
    data[MINT_INITIALIZED] = 1;
    set_raw_account(svm, mint, data, token_program);
}

/// Writes a token account owned by `owner` holding `amount`.
fn create_token_account(
    svm: &mut LiteSVM,
    account: Address,
    mint: Address,
    owner: Address,
    amount: u64,
) {
    create_token_account_of(svm, account, mint, owner, amount, TOKEN_PROGRAM_ID);
}

fn create_token_account_of(
    svm: &mut LiteSVM,
    account: Address,
    mint: Address,
    owner: Address,
    amount: u64,
    token_program: Address,
) {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // AccountState::Initialized
    set_raw_account(svm, account, data, token_program);
}

/// The gross amount a transfer must carry for `net` to arrive on a mint that
/// skims `basis_points` of it — the token program's `calculate_pre_fee_amount`
/// for an uncapped fee, which is what the fixture below sets.
fn gross_for_net(net: u64, basis_points: u16) -> u64 {
    if net == 0 || basis_points == 0 {
        return net;
    }
    let basis_points = u128::from(basis_points);
    let net = u128::from(net);
    let gross = (net * 10_000).div_ceil(10_000 - basis_points);
    u64::try_from(gross).expect("gross fits in u64")
}

/// Writes an empty Token-2022 account of `len` zero bytes. A mint or account that
/// carries an extension has to be *allocated* at the size the extension needs
/// before the token program's `Initialize*` instruction can write it, so the
/// tests hand it a blank buffer and let the program do the initialization.
fn blank_token_2022_account(svm: &mut LiteSVM, account: Address, len: usize) {
    set_raw_account(svm, account, vec![0u8; len], TOKEN_2022_ID);
}

/// A pool-ready fixture whose first leg carries a Token-2022 transfer fee of
/// `basis_points`, and a funded account for each leg.
///
/// The fee is a real extension on a real mint, built by the token program's own
/// instructions, because the vault's size and the transfers' shortfall are both
/// derived from it. Returns `(fee mint, plain mint, fee account, plain account)`.
fn fee_mint_fixture(
    svm: &mut LiteSVM,
    payer: &Keypair,
    basis_points: u16,
    maximum_fee: u64,
) -> (Address, Address, Address, Address) {
    let authority = payer.pubkey();
    let fee_mint = Keypair::new().pubkey();
    let plain_mint = Keypair::new().pubkey();
    let fee_account = Keypair::new().pubkey();
    let plain_account = Keypair::new().pubkey();

    blank_token_2022_account(svm, fee_mint, FEE_MINT_LEN);
    blank_token_2022_account(svm, fee_account, FEE_ACCOUNT_LEN);
    // The extension goes first: `InitializeMint2` requires the account to be
    // exactly as long as the extensions it already carries, so the fee entry has
    // to be written before the mint's own base state is initialized.
    send(
        svm,
        payer,
        &[
            Instructions::initialize_transfer_fee_config(
                fee_mint,
                authority,
                basis_points,
                maximum_fee,
            ),
            Instructions::initialize_mint_2(fee_mint, authority),
            Instructions::initialize_account_3(fee_account, fee_mint, authority),
            Instructions::mint_to(fee_mint, fee_account, authority, FUNDS),
        ],
    );
    create_mint_of(svm, authority, plain_mint, TOKEN_2022_ID);
    create_token_account_of(
        svm,
        plain_account,
        plain_mint,
        authority,
        FUNDS,
        TOKEN_2022_ID,
    );

    (fee_mint, plain_mint, fee_account, plain_account)
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
    Address::find_program_address(
        &[b"clmm-tick", pool.as_ref(), &tick.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

fn oracle_pda(pool: Address) -> Address {
    Address::find_program_address(&[b"oracle", pool.as_ref()], &ORACLE_PROGRAM_ID).0
}

fn kyc_config_pda() -> Address {
    Address::find_program_address(&[b"kyc-config"], &KYC_PROGRAM_ID).0
}

fn kyc_mint_pda() -> Address {
    Address::find_program_address(&[b"kyc-mint"], &KYC_PROGRAM_ID).0
}

/// The hooks program's authority PDA: it owns the swap's custody account and
/// signs the CPI that locks the output in escrow.
fn hook_authority_pda() -> Address {
    Address::find_program_address(&[b"hook_authority"], &HOOKS_PROGRAM_ID).0
}

fn settlement_config_pda() -> Address {
    Address::find_program_address(&[b"config"], &SETTLEMENT_PROGRAM_ID).0
}

fn settlement_vault_pda(trade_id: u64) -> Address {
    Address::find_program_address(
        &[b"settlement", &trade_id.to_le_bytes()],
        &SETTLEMENT_PROGRAM_ID,
    )
    .0
}

/// The settlement program's fixed escrow owner: every escrow token account is
/// owned by this PDA, so one whitelist entry covers every trade.
fn settlement_escrow_authority_pda() -> Address {
    Address::find_program_address(&[b"escrow_authority"], &SETTLEMENT_PROGRAM_ID).0
}

/// This trade's escrow token account: the settlement program's own PDA, which it
/// creates and initializes during the swap that funds it.
fn settlement_escrow_pda(trade_id: u64) -> Address {
    Address::find_program_address(&[b"escrow", &trade_id.to_le_bytes()], &SETTLEMENT_PROGRAM_ID).0
}

/// A legacy-token associated token account address: the custody account the
/// swap pays into is the hook authority's ATA for the output mint.
fn associated_token_address(owner: Address, mint: Address) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

/// A program's `ProgramData` account: `[program]` under the upgradeable loader.
fn program_data_pda(program: Address) -> Address {
    Address::find_program_address(&[program.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// Marks `authority` as `program`'s upgrade authority by writing the metadata the
/// loader keeps in the head of `ProgramData` - the 4-byte `ProgramData` tag, the
/// slot, an `Option` tag of 1 and the authority - leaving the program bytes
/// untouched. On a real cluster only an upgrade transaction can write this; here
/// it stands in for one. Both `kyc_gate::initialize_kyc_mint` and
/// `settlement::initialize` are restricted to whoever this names, so without it
/// they refuse every caller - which is the state a program is in before it is
/// deployed by an upgrade.
fn set_upgrade_authority(svm: &mut LiteSVM, program: Address, authority: Address) {
    let address = program_data_pda(program);
    let mut account = svm
        .get_account(&address)
        .unwrap_or_else(|| panic!("add_program creates {program}'s ProgramData account"));
    account.data[0..4].copy_from_slice(&3u32.to_le_bytes()); // AccountType::ProgramData
    account.data[12] = 1; // Option::Some
    account.data[13..45].copy_from_slice(authority.as_ref());
    svm.set_account(address, account).unwrap();
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
    (tick as i64).div_euclid(spacing as i64) - i64::from(MIN_TICK).div_euclid(spacing as i64)
}

fn bitmap_bit(svm: &LiteSVM, pool: Address, tick: i32) -> bool {
    let data = data_of(svm, pool);
    let bit = bit_of(tick, SPACING) as usize;
    data[POOL_BITMAP + bit / 8] & (1 << (bit % 8)) != 0
}

// -----------------------------------------------------------------------
// Instructions
// -----------------------------------------------------------------------

/// A pool's rail wiring as `initialize_pool` records it: the policy bytes the
/// pool stores, plus the program account each enabled rail needs. An `Option`
/// that is `None` writes the zero key and the program-ID sentinel, which is how
/// a rail is left off.
#[derive(Clone, Copy, Default)]
struct PoolRails {
    kyc_config: Option<Address>,
    oracle_state: Option<Address>,
    hooks_authority: Option<Address>,
    settlement_slots: u64,
}

impl PoolRails {
    fn none() -> Self {
        Self::default()
    }
}

/// Which rails a swap runs with, and the accounts the caller has to supply for
/// them. Everything else is derived: the oracle state from the pool, the
/// settlement vault and its escrow from the trade id, the custody account from
/// the hook authority and the output mint.
#[derive(Clone, Copy, Default)]
struct Wired {
    /// The trader's KYC attestation account.
    kyc: Option<Address>,
    /// The pool's oracle state PDA.
    oracle: Option<Address>,
    /// The trade whose escrow the output is locked in, when the settlement rail
    /// is on. The swap's `trade_id` argument is zero otherwise.
    escrow_trade_id: Option<u64>,
    /// Some other account to hand the swap as the custody account, instead of
    /// the hook authority's ATA. Only a test that wants the swap to refuse it
    /// sets this.
    custody_override: Option<Address>,
    /// The token program the pool was created under. Legacy SPL Token unless a
    /// test creates the pool over Token-2022 mints.
    token_program: Option<Address>,
}

struct Instructions;

impl Instructions {
    fn initialize_pool(
        payer: Address,
        mint_a: Address,
        mint_b: Address,
        spacing: i32,
        rails: PoolRails,
    ) -> Instruction {
        Self::initialize_pool_of(payer, mint_a, mint_b, spacing, rails, TOKEN_PROGRAM_ID)
    }

    /// The same under either token program: the program creates both vaults
    /// through whichever one it is handed, so a Token-2022 pool's vaults are
    /// 2022 accounts.
    fn initialize_pool_of(
        payer: Address,
        mint_a: Address,
        mint_b: Address,
        spacing: i32,
        rails: PoolRails,
        token_program: Address,
    ) -> Instruction {
        let pool = pool_pda(mint_a, mint_b);
        let mut data = IX_INITIALIZE_POOL.to_vec();
        data.extend_from_slice(&(1u128 << 64).to_le_bytes()); // price 1.0
        data.extend_from_slice(&spacing.to_le_bytes());
        data.extend_from_slice(&FEE_PIPS.to_le_bytes());
        data.extend_from_slice(rails.kyc_config.unwrap_or_default().as_ref());
        let threshold = if rails.oracle_state.is_some() {
            WIRED_THRESHOLD_BPS
        } else {
            0
        };
        data.extend_from_slice(&threshold.to_le_bytes());
        data.extend_from_slice(rails.hooks_authority.unwrap_or_default().as_ref());
        data.extend_from_slice(&rails.settlement_slots.to_le_bytes());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(pool, false),
                AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
                AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(payer, true),
                AccountMeta::new(rails.oracle_state.unwrap_or(PROGRAM_ID), false),
                AccountMeta::new_readonly(
                    rails.kyc_config.map_or(PROGRAM_ID, |_| KYC_PROGRAM_ID),
                    false,
                ),
                AccountMeta::new_readonly(
                    rails.oracle_state.map_or(PROGRAM_ID, |_| ORACLE_PROGRAM_ID),
                    false,
                ),
                AccountMeta::new_readonly(
                    rails
                        .hooks_authority
                        .map_or(PROGRAM_ID, |_| HOOKS_PROGRAM_ID),
                    false,
                ),
                AccountMeta::new_readonly(token_program, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    /// kyc_gate's `initialize_kyc_mint`, signed by the gate's upgrade authority:
    /// it creates the soulbound attestation mint and the config that names the
    /// KYC authority.
    fn initialize_kyc_mint(authority: Address) -> Instruction {
        Instruction {
            program_id: KYC_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(kyc_config_pda(), false),
                AccountMeta::new(kyc_mint_pda(), false),
                AccountMeta::new(authority, true),
                AccountMeta::new_readonly(program_data_pda(KYC_PROGRAM_ID), false),
                AccountMeta::new_readonly(TOKEN_2022_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data: IX_INITIALIZE_KYC_MINT.to_vec(),
        }
    }

    /// kyc_gate's `issue_kyc`: mints the single attestation token into an
    /// institution's account. Gate-authority only.
    fn issue_kyc(authority: Address, institution_kyc_account: Address) -> Instruction {
        Instruction {
            program_id: KYC_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(kyc_config_pda(), false),
                AccountMeta::new(kyc_mint_pda(), false),
                AccountMeta::new(institution_kyc_account, false),
                AccountMeta::new_readonly(authority, true),
                AccountMeta::new_readonly(TOKEN_2022_ID, false),
            ],
            data: IX_ISSUE_KYC.to_vec(),
        }
    }

    /// The system program's `CreateAccount`, so a test can create an attestation
    /// account the way an integrator would rather than writing bytes by hand.
    fn create_account(
        from: Address,
        to: Address,
        lamports: u64,
        space: u64,
        owner: Address,
    ) -> Instruction {
        let mut data = IX_CREATE_ACCOUNT.to_le_bytes().to_vec();
        data.extend_from_slice(&lamports.to_le_bytes());
        data.extend_from_slice(&space.to_le_bytes());
        data.extend_from_slice(owner.as_ref());
        Instruction {
            program_id: SYSTEM_PROGRAM_ID,
            accounts: vec![AccountMeta::new(from, true), AccountMeta::new(to, true)],
            data,
        }
    }

    /// Token-2022's `InitializeAccount3`: it initializes the extensions the mint
    /// requires on the new account, which is the only way to make an account the
    /// token program will mint a non-transferable token into.
    fn initialize_account_3(account: Address, mint: Address, owner: Address) -> Instruction {
        let mut data = vec![IX_INITIALIZE_ACCOUNT_3];
        data.extend_from_slice(owner.as_ref());
        Instruction {
            program_id: TOKEN_2022_ID,
            accounts: vec![
                AccountMeta::new(account, false),
                AccountMeta::new_readonly(mint, false),
            ],
            data,
        }
    }

    /// Token-2022's `InitializeMint2`: the mint account is written in place, so
    /// it must already be allocated at its final length and owned by the token
    /// program.
    fn initialize_mint_2(mint: Address, authority: Address) -> Instruction {
        let mut data = vec![IX_INITIALIZE_MINT_2];
        data.push(6); // decimals
        data.extend_from_slice(authority.as_ref());
        data.push(0); // no freeze authority
        Instruction {
            program_id: TOKEN_2022_ID,
            accounts: vec![AccountMeta::new(mint, false)],
            data,
        }
    }

    /// Token-2022's `InitializeTransferFeeConfig`: a fee that skims
    /// `basis_points` of every transfer, capped at `maximum_fee`. Both
    /// authorities are the payer, and the same fee is set for the current and
    /// the next epoch so the effective rate does not depend on the clock.
    fn initialize_transfer_fee_config(
        mint: Address,
        authority: Address,
        basis_points: u16,
        maximum_fee: u64,
    ) -> Instruction {
        let mut data = vec![IX_TRANSFER_FEE_EXTENSION, TRANSFER_FEE_INITIALIZE_CONFIG];
        for _ in 0..2 {
            data.push(1); // COption::Some
            data.extend_from_slice(authority.as_ref());
        }
        data.extend_from_slice(&basis_points.to_le_bytes());
        data.extend_from_slice(&maximum_fee.to_le_bytes());
        Instruction {
            program_id: TOKEN_2022_ID,
            accounts: vec![
                AccountMeta::new(mint, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    /// Token-2022's `MintTo`: it is how a test funds an account under a mint
    /// whose authority is the payer.
    fn mint_to(
        mint: Address,
        destination: Address,
        authority: Address,
        amount: u64,
    ) -> Instruction {
        let mut data = vec![IX_MINT_TO];
        data.extend_from_slice(&amount.to_le_bytes());
        Instruction {
            program_id: TOKEN_2022_ID,
            accounts: vec![
                AccountMeta::new(mint, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    /// Token-2022's `SetTransferFee`: the config authority changes what future
    /// transfers are skimmed, from the current epoch on. Its accounts are the
    /// mint and that authority.
    fn set_transfer_fee(
        mint: Address,
        authority: Address,
        basis_points: u16,
        maximum_fee: u64,
    ) -> Instruction {
        let mut data = vec![IX_TRANSFER_FEE_EXTENSION, TRANSFER_FEE_SET];
        data.extend_from_slice(&basis_points.to_le_bytes());
        data.extend_from_slice(&maximum_fee.to_le_bytes());
        Instruction {
            program_id: TOKEN_2022_ID,
            accounts: vec![
                AccountMeta::new(mint, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    /// settlement's `initialize`, signed by the clearinghouse's upgrade authority.
    /// A gated config refuses trades that did not come from a wired pool, so the
    /// hooks flow below needs `set_hooks_authority` to name the hook authority.
    fn settlement_initialize(authority: Address, pools_require_gate: bool) -> Instruction {
        let mut data = IX_SETTLEMENT_INITIALIZE.to_vec();
        data.push(u8::from(pools_require_gate));
        Instruction {
            program_id: SETTLEMENT_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(settlement_config_pda(), false),
                AccountMeta::new(authority, true),
                AccountMeta::new_readonly(program_data_pda(SETTLEMENT_PROGRAM_ID), false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data,
        }
    }

    /// settlement's `set_hooks_authority`: the one account a gated config admits
    /// without a pool.
    fn set_hooks_authority(authority: Address, hooks_authority: Address) -> Instruction {
        let mut data = IX_SET_HOOKS_AUTHORITY.to_vec();
        data.extend_from_slice(hooks_authority.as_ref());
        Instruction {
            program_id: SETTLEMENT_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(settlement_config_pda(), false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    fn trip_breaker(pool: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(pool, false),
                AccountMeta::new(oracle_pda(pool), false),
                AccountMeta::new_readonly(ORACLE_PROGRAM_ID, false),
            ],
            data: IX_TRIP_BREAKER.to_vec(),
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
        Self::add_liquidity_of(
            payer,
            pool,
            mint_a,
            mint_b,
            user_token_a,
            user_token_b,
            lower,
            upper,
            liquidity,
            TOKEN_PROGRAM_ID,
        )
    }

    fn add_liquidity_of(
        payer: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        lower: i32,
        upper: i32,
        liquidity: u128,
        token_program: Address,
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
                AccountMeta::new_readonly(token_program, false),
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
        Self::swap_wired(
            payer,
            pool,
            mint_a,
            mint_b,
            user_token_a,
            user_token_b,
            amount_in,
            is_a_to_b,
            walk,
            Wired::default(),
        )
    }

    /// A swap on a pool with rails wired. Every account a rail needs is either
    /// taken from `wired` or derived here, and a rail that is off has each of
    /// its accounts given this program's own ID as the sentinel.
    fn swap_wired(
        payer: Address,
        pool: Address,
        mint_a: Address,
        mint_b: Address,
        user_token_a: Address,
        user_token_b: Address,
        amount_in: u64,
        is_a_to_b: bool,
        walk: &[i32],
        wired: Wired,
    ) -> Instruction {
        let (oracle_state, oracle_program) = match wired.oracle {
            Some(state) => (state, ORACLE_PROGRAM_ID),
            None => (PROGRAM_ID, PROGRAM_ID), // sentinel: rail off
        };
        let (kyc, user_kyc_account, kyc_program) = match wired.kyc {
            Some(attestation) => (kyc_config_pda(), attestation, KYC_PROGRAM_ID),
            None => (PROGRAM_ID, PROGRAM_ID, PROGRAM_ID),
        };
        // The settlement rail: the output is paid into the hook authority's
        // custody ATA and swept into this trade's escrow, so the accounts are the
        // ones the hooks and settlement programs re-derive from `trade_id`.
        let trade_id = wired.escrow_trade_id.unwrap_or(0);
        let output_mint = if is_a_to_b { mint_b } else { mint_a };
        // The custody account is the hook authority's ATA for the output mint.
        // `custody_override` substitutes some other account for it, so a test
        // can show the swap refusing anything else.
        let (hooks_authority, custody, settlement_config, settlement_program) =
            match wired.escrow_trade_id {
                Some(_) => (
                    hook_authority_pda(),
                    wired.custody_override.unwrap_or_else(|| {
                        associated_token_address(hook_authority_pda(), output_mint)
                    }),
                    settlement_config_pda(),
                    SETTLEMENT_PROGRAM_ID,
                ),
                None => (PROGRAM_ID, PROGRAM_ID, PROGRAM_ID, PROGRAM_ID),
            };
        let settlement_vault = match wired.escrow_trade_id {
            Some(trade_id) => settlement_vault_pda(trade_id),
            None => PROGRAM_ID,
        };
        let settlement_vault_token = match wired.escrow_trade_id {
            Some(trade_id) => settlement_escrow_pda(trade_id),
            None => PROGRAM_ID,
        };
        let (associated_token_program, hooks_program) = match wired.escrow_trade_id {
            Some(_) => (ASSOCIATED_TOKEN_PROGRAM_ID, HOOKS_PROGRAM_ID),
            None => (PROGRAM_ID, PROGRAM_ID),
        };
        let mut data = IX_SWAP.to_vec();
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes()); // min_amount_out
        data.push(u8::from(is_a_to_b));
        data.extend_from_slice(&trade_id.to_le_bytes());
        let mut accounts = vec![
            AccountMeta::new(pool, false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_pda(b"clmm-vault-a", pool, mint_a), false),
            AccountMeta::new(vault_pda(b"clmm-vault-b", pool, mint_b), false),
            AccountMeta::new(user_token_a, false),
            AccountMeta::new(user_token_b, false),
            AccountMeta::new(payer, true),
            AccountMeta::new(kyc, false),                 // kyc_config
            AccountMeta::new_readonly(user_kyc_account, false), // user_kyc_account
            AccountMeta::new(oracle_state, false),        // oracle_state
            AccountMeta::new(hooks_authority, false), // hooks_authority (pays for the escrow)
            AccountMeta::new(custody, false),             // custody_token_account
            AccountMeta::new_readonly(settlement_config, false), // settlement_config
            AccountMeta::new_readonly(settlement_program, false), // settlement_program
            AccountMeta::new(settlement_vault, false),    // settlement_vault
            AccountMeta::new(settlement_vault_token, false), // settlement_vault_token
            AccountMeta::new_readonly(associated_token_program, false), // associated_token_program
            AccountMeta::new_readonly(kyc_program, false), // kyc_program
            AccountMeta::new_readonly(oracle_program, false), // oracle_program
            AccountMeta::new_readonly(hooks_program, false), // hooks_program
            AccountMeta::new_readonly(wired.token_program.unwrap_or(TOKEN_PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ];
        accounts.extend(
            walk.iter()
                .map(|tick| AccountMeta::new(tick_pda(pool, *tick), false)),
        );
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
        &[Instructions::initialize_pool(
            payer_address,
            mint_a,
            mint_b,
            SPACING,
            PoolRails::none(),
        )],
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

/// The newest observation in the oracle's ring buffer, as
/// `(timestamp, sqrt_price_x64, cumulative_price_x64)`. The state caches its
/// cursor, so the newest entry is whatever `observation_index` points at.
fn newest_observation(svm: &LiteSVM, oracle: Address) -> (i64, u128, u128) {
    let state = data_of(svm, oracle);
    let index = state[STATE_INDEX] as usize;
    let at = STATE_OBSERVATIONS + index * OBS_SIZE;
    (
        i64_at(&state, at + OBS_TIMESTAMP),
        u128_at(&state, at + OBS_SQRT_PRICE),
        u128_at(&state, at + OBS_SIZE - 16),
    )
}

/// A pool wired to the oracle: both programs are loaded, and `initialize_pool`
/// creates the pool's oracle with the pool PDA signing. The ranges are wide, so
/// a single swap moves the price far enough for the next check to be a real
/// deviation from the TWAP the earlier trades built.
fn setup_wired() -> Fixture {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    for (id, name) in [(PROGRAM_ID, "clmm"), (ORACLE_PROGRAM_ID, "oracle")] {
        let path = format!(
            "{}/../../target/deploy/{name}.so",
            env!("CARGO_MANIFEST_DIR")
        );
        let program = std::fs::read(&path)
            .unwrap_or_else(|_| panic!("build the program first: `anchor build -p {name}`"));
        svm.add_program(id, &program).unwrap();
    }

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
        &[Instructions::initialize_pool(
            payer_address,
            mint_a,
            mint_b,
            SPACING,
            PoolRails {
                oracle_state: Some(oracle_pda(pool)),
                ..PoolRails::none()
            },
        )],
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
    for (lower, upper) in [(WIRED_LOWER, WIRED_UPPER), (WIRED_DEEP, WIRED_LOWER)] {
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

/// A pool gated by the KYC program: `trader` holds an attestation and
/// `stranger` holds an empty account of the same mint, so the two differ only in
/// what the gate says about them.
struct KycFixture {
    svm: LiteSVM,
    authority: Keypair,
    trader: Keypair,
    stranger: Keypair,
    mint_a: Address,
    mint_b: Address,
    pool: Address,
    trader_a: Address,
    trader_b: Address,
    stranger_a: Address,
    stranger_b: Address,
    trader_kyc: Address,
    stranger_kyc: Address,
}

/// Boots the clmm and kyc_gate programs, creates the attestation mint, a pool
/// gated on its config, and two attestation accounts - one issued, one empty.
fn setup_kyc() -> KycFixture {
    let mut svm = LiteSVM::new();
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), LAMPORTS).unwrap();
    let trader = Keypair::new();
    svm.airdrop(&trader.pubkey(), LAMPORTS).unwrap();
    let stranger = Keypair::new();
    svm.airdrop(&stranger.pubkey(), LAMPORTS).unwrap();

    for (id, name) in [(PROGRAM_ID, "clmm"), (KYC_PROGRAM_ID, "kyc_gate")] {
        let path = format!(
            "{}/../../target/deploy/{name}.so",
            env!("CARGO_MANIFEST_DIR")
        );
        let program = std::fs::read(&path)
            .unwrap_or_else(|_| panic!("build the program first: `anchor build -p {name}`"));
        svm.add_program(id, &program).unwrap();
    }

    // The gate is the upgrade authority's to create, as it is after a real
    // deploy where the upgrading transaction records one.
    set_upgrade_authority(&mut svm, KYC_PROGRAM_ID, authority.pubkey());
    send(
        &mut svm,
        &authority,
        &[Instructions::initialize_kyc_mint(authority.pubkey())],
    );

    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    let (trader_a, trader_b) = (Keypair::new().pubkey(), Keypair::new().pubkey());
    let (stranger_a, stranger_b) = (Keypair::new().pubkey(), Keypair::new().pubkey());
    for mint in [mint_a, mint_b] {
        create_mint(&mut svm, authority.pubkey(), mint);
    }
    for (account, mint, owner) in [
        (trader_a, mint_a, trader.pubkey()),
        (trader_b, mint_b, trader.pubkey()),
        (stranger_a, mint_a, stranger.pubkey()),
        (stranger_b, mint_b, stranger.pubkey()),
    ] {
        create_token_account(&mut svm, account, mint, owner, FUNDS);
    }

    let pool = pool_pda(mint_a, mint_b);
    send(
        &mut svm,
        &authority,
        &[Instructions::initialize_pool(
            authority.pubkey(),
            mint_a,
            mint_b,
            SPACING,
            PoolRails {
                kyc_config: Some(kyc_config_pda()),
                ..PoolRails::none()
            },
        )],
    );

    let mut fixture = KycFixture {
        svm,
        authority,
        trader,
        stranger,
        mint_a,
        mint_b,
        pool,
        trader_a,
        trader_b,
        stranger_a,
        stranger_b,
        trader_kyc: Address::default(),
        stranger_kyc: Address::default(),
    };

    // Two positions, as in the unwired fixture, so a swap has somewhere to
    // trade. The trader deposits, so the positions belong to the trader.
    for (lower, upper) in [(LOWER, UPPER), (DEEP, LOWER)] {
        let instruction = Instructions::add_liquidity(
            fixture.trader.pubkey(),
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.trader_a,
            fixture.trader_b,
            lower,
            upper,
            LIQUIDITY,
        );
        let trader = fixture.trader.insecure_clone();
        send(&mut fixture.svm, &trader, &[instruction]);
    }

    // An attestation account has to go through Token-2022's `InitializeAccount3`:
    // the token program refuses to mint a non-transferable token into an account
    // that does not carry the extensions the mint requires.
    let (trader_kyc, stranger_kyc) = (Keypair::new(), Keypair::new());
    for (account, owner) in [
        (&trader_kyc, fixture.trader.pubkey()),
        (&stranger_kyc, fixture.stranger.pubkey()),
    ] {
        let instructions = [
            Instructions::create_account(
                fixture.authority.pubkey(),
                account.pubkey(),
                ACCOUNT_LAMPORTS,
                KYC_ACCOUNT_LEN as u64,
                TOKEN_2022_ID,
            ),
            Instructions::initialize_account_3(account.pubkey(), kyc_mint_pda(), owner),
        ];
        let authority = fixture.authority.insecure_clone();
        send_with(
            &mut fixture.svm,
            &authority,
            &[&authority, account],
            &instructions,
        );
    }
    fixture.trader_kyc = trader_kyc.pubkey();
    fixture.stranger_kyc = stranger_kyc.pubkey();

    // The gate issues exactly one: the trader's. The stranger's stays empty, and
    // is a valid account of the right mint, so the only thing the gate can say
    // about it is that it holds no attestation.
    let authority = fixture.authority.insecure_clone();
    send(
        &mut fixture.svm,
        &authority,
        &[Instructions::issue_kyc(authority.pubkey(), fixture.trader_kyc)],
    );
    fixture
}

/// A pool wired to the settlement rail: the swap output is paid into the hook
/// authority's custody account and swept into the T+N escrow for `trade_id`.
struct EscrowFixture {
    svm: LiteSVM,
    /// The pool's creator and the trader: it sells A and would normally be paid B.
    payer: Keypair,
    mint_a: Address,
    mint_b: Address,
    user_token_a: Address,
    user_token_b: Address,
    pool: Address,
    trade_id: u64,
    /// The settlement vault PDA for the trade, and this trade's escrow token
    /// account (a PDA of the settlement program, created during the swap).
    vault: Address,
    escrow: Address,
    /// The hook authority's ATA for the output mint: where the swap pays, and
    /// what `after_swap` sweeps into escrow.
    custody: Address,
}

/// Wallet layout: the vault's fields, in order.
const VAULT_TRADE_ID: usize = 8;
const VAULT_TAKER: usize = 16;
const VAULT_TOKEN_MINT: usize = 48;
const VAULT_AMOUNT: usize = 80;
const VAULT_SETTLEMENT_SLOT: usize = 88;
/// Absolute slot after which a pending escrow can only be refunded.
const VAULT_EXPIRY_SLOT: usize = 96;
const VAULT_STATUS: usize = 104;
/// `SettlementStatus::Pending`.
const STATUS_PENDING: u8 = 0;

/// The T+N delay the pool is created with.
const SETTLEMENT_SLOTS: u64 = 100;
const TRADE_ID: u64 = 7;

/// Boots the clmm, hooks and settlement programs, sets the clearinghouse up as a
/// gated one, and creates a pool whose output is escrowed. The escrow accounts
/// are left for the settlement program to create, as a client leaves them: the
/// swap's own transaction creates the vault and the trade's escrow token account.
fn setup_escrow() -> EscrowFixture {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    for (id, name) in [
        (PROGRAM_ID, "clmm"),
        (HOOKS_PROGRAM_ID, "hooks"),
        (SETTLEMENT_PROGRAM_ID, "settlement"),
    ] {
        let path = format!(
            "{}/../../target/deploy/{name}.so",
            env!("CARGO_MANIFEST_DIR")
        );
        let program = std::fs::read(&path)
            .unwrap_or_else(|_| panic!("build the program first: `anchor build -p {name}`"));
        svm.add_program(id, &program).unwrap();
    }

    // The clearinghouse is the upgrade authority's to create, and a gated config
    // admits only trades that came from a wired pool - or from the hook authority
    // it names, which is the path the AMM's rail takes.
    set_upgrade_authority(&mut svm, SETTLEMENT_PROGRAM_ID, payer.pubkey());
    send(
        &mut svm,
        &payer,
        &[Instructions::settlement_initialize(payer.pubkey(), true)],
    );
    send(
        &mut svm,
        &payer,
        &[Instructions::set_hooks_authority(
            payer.pubkey(),
            hook_authority_pda(),
        )],
    );

    // The hook authority pays for the vault and the escrow it creates, so it
    // needs lamports of its own.
    svm.airdrop(&hook_authority_pda(), LAMPORTS).unwrap();

    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    let user_token_a = Keypair::new().pubkey();
    let user_token_b = Keypair::new().pubkey();
    for mint in [mint_a, mint_b] {
        create_mint(&mut svm, payer.pubkey(), mint);
    }
    create_token_account(&mut svm, user_token_a, mint_a, payer.pubkey(), FUNDS);
    create_token_account(&mut svm, user_token_b, mint_b, payer.pubkey(), FUNDS);

    let pool = pool_pda(mint_a, mint_b);
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool(
            payer.pubkey(),
            mint_a,
            mint_b,
            SPACING,
            PoolRails {
                hooks_authority: Some(hook_authority_pda()),
                settlement_slots: SETTLEMENT_SLOTS,
                ..PoolRails::none()
            },
        )],
    );

    // The output of an A→B swap is token B, so the custody account is the hook
    // authority's ATA for mint B. It is created here because it has to exist
    // before the swap pays into it; the settlement program sweeps it empty.
    let custody = associated_token_address(hook_authority_pda(), mint_b);
    create_token_account(&mut svm, custody, mint_b, hook_authority_pda(), 0);

    for (lower, upper) in [(LOWER, UPPER), (DEEP, LOWER)] {
        let instruction = Instructions::add_liquidity(
            payer.pubkey(),
            pool,
            mint_a,
            mint_b,
            user_token_a,
            user_token_b,
            lower,
            upper,
            LIQUIDITY,
        );
        send(&mut svm, &payer, &[instruction]);
    }

    // Neither the vault nor its escrow exists yet - the settlement program
    // creates both during the swap (Anchor's `init` for the vault, `create_account`
    // for the escrow PDA). Pass them as empty system accounts, the way a client
    // passes an account it is about to create.
    let vault = settlement_vault_pda(TRADE_ID);
    let escrow = settlement_escrow_pda(TRADE_ID);
    for address in [vault, escrow] {
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

    EscrowFixture {
        svm,
        payer,
        mint_a,
        mint_b,
        user_token_a,
        user_token_b,
        pool,
        trade_id: TRADE_ID,
        vault,
        escrow,
        custody,
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn the_pool_is_created_with_a_bitmap_sized_to_its_spacing() {
    let fixture = setup();
    let pool = data_of(&fixture.svm, fixture.pool);
    assert_eq!(
        i32_at(&pool, POOL_TICK_CURRENT),
        0,
        "price starts at tick 0"
    );
    assert_eq!(
        u128_at(&pool, POOL_LIQUIDITY),
        LIQUIDITY,
        "only the range spanning the price is active"
    );
    let cells = u32::from_le_bytes(
        pool[POOL_BITMAP_CELLS..POOL_BITMAP_CELLS + 4]
            .try_into()
            .unwrap(),
    );
    let bytes = u32::from_le_bytes(
        pool[POOL_BITMAP_LEN..POOL_BITMAP_LEN + 4]
            .try_into()
            .unwrap(),
    );
    assert_eq!(cells, 14_788);
    assert_eq!(bytes, 1_849);
    assert_eq!(pool.len(), POOL_BITMAP + 1_849 + POOL_WIRING_TAIL);
}

#[test]
fn a_deposit_records_its_owner_and_marks_both_ticks() {
    let fixture = setup();
    // The position the deposit created belongs to the depositor and records the
    // range — this is what makes it withdrawable at all.
    let position = data_of(
        &fixture.svm,
        position_pda(fixture.pool, fixture.payer.pubkey(), LOWER, UPPER),
    );
    assert_eq!(
        address_at(&position, POSITION_OWNER),
        fixture.payer.pubkey()
    );
    assert_eq!(u128_at(&position, POSITION_LIQUIDITY), LIQUIDITY);

    // Both bounds are in the bitmap, and a tick nobody used is not.
    assert!(bitmap_bit(&fixture.svm, fixture.pool, LOWER));
    assert!(bitmap_bit(&fixture.svm, fixture.pool, UPPER));
    assert!(bitmap_bit(&fixture.svm, fixture.pool, DEEP));
    assert!(!bitmap_bit(&fixture.svm, fixture.pool, 180));
}

/// A pool whose mints are Token-2022 accounts, traded end to end.
///
/// Every account in play is owned by the 2022 program — both mints, both user
/// ATAs, the vaults `initialize_pool` creates, and the token-program account
/// itself — so the deposit and the swap only succeed if the program really
/// drives whichever token program it was handed. A legacy-token assumption
/// anywhere (an `Account<Mint>` that only unpacks 165-byte accounts, a CPI to
/// the legacy program's ID, an ATA derived under it) fails here.
#[test]
fn a_token_2022_pool_deposits_and_swaps() {
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
    create_mint_of(&mut svm, payer_address, mint_a, TOKEN_2022_ID);
    create_mint_of(&mut svm, payer_address, mint_b, TOKEN_2022_ID);
    for (account, mint) in [(user_token_a, mint_a), (user_token_b, mint_b)] {
        create_token_account_of(&mut svm, account, mint, payer_address, FUNDS, TOKEN_2022_ID);
    }

    let pool = pool_pda(mint_a, mint_b);
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool_of(
            payer_address,
            mint_a,
            mint_b,
            SPACING,
            PoolRails::none(),
            TOKEN_2022_ID,
        )],
    );

    // The vaults are 2022 accounts: `initialize_pool` created them through the
    // token program it was handed rather than the legacy one.
    for (prefix, mint) in [(b"clmm-vault-a", mint_a), (b"clmm-vault-b", mint_b)] {
        let vault = vault_pda(prefix, pool, mint);
        let info = svm
            .get_account(&vault)
            .unwrap_or_else(|| panic!("vault {vault} does not exist"));
        assert_eq!(
            info.owner, TOKEN_2022_ID,
            "the vault is a Token-2022 account"
        );
    }

    send(
        &mut svm,
        &payer,
        &[Instructions::add_liquidity_of(
            payer_address,
            pool,
            mint_a,
            mint_b,
            user_token_a,
            user_token_b,
            LOWER,
            UPPER,
            LIQUIDITY,
            TOKEN_2022_ID,
        )],
    );

    // The deposit landed: the position records the range and the pool's vault
    // holds the A leg the liquidity was minted against.
    let position = data_of(&svm, position_pda(pool, payer_address, LOWER, UPPER));
    assert_eq!(u128_at(&position, POSITION_LIQUIDITY), LIQUIDITY);
    let deposited_a = token_balance(&svm, vault_pda(b"clmm-vault-a", pool, mint_a));
    assert!(deposited_a > 0, "the pool's A vault holds the deposit");

    let balance_a_before = token_balance(&svm, user_token_a);
    send(
        &mut svm,
        &payer,
        &[Instructions::swap_wired(
            payer_address,
            pool,
            mint_a,
            mint_b,
            user_token_a,
            user_token_b,
            100_000_000,
            true,
            &[LOWER, DEEP],
            Wired {
                token_program: Some(TOKEN_2022_ID),
                ..Default::default()
            },
        )],
    );

    // Selling A bought B out of the pool, and only the consumed input plus fee
    // left the trader: the whole 2022 path — transfer in, transfer out under the
    // pool's signature — settled.
    assert!(
        token_balance(&svm, user_token_a) < balance_a_before,
        "the trader paid the input"
    );
    assert!(
        token_balance(&svm, vault_pda(b"clmm-vault-b", pool, mint_b)) > 0,
        "the pool holds the input it was paid"
    );
}

/// A Token-2022 mint that carries a transfer fee skims part of every transfer,
/// so a deposit at face value would leave the vault short of the liquidity the
/// position is minted for. `initialize_pool` sizes each vault from the
/// extensions the mint requires (`TransferFeeAmount` here) and the inbound
/// transfer is grossed up by the fee in force: the vault is credited exactly
/// what the position was priced for, and the depositor pays the fee on top.
#[test]
fn a_fee_bearing_pool_deposits_and_swaps() {
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
    let (fee_mint, plain_mint, fee_account, plain_account) =
        fee_mint_fixture(&mut svm, &payer, FEE_BASIS_POINTS, FEE_MAXIMUM);

    let pool = pool_pda(fee_mint, plain_mint);
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool_of(
            payer_address,
            fee_mint,
            plain_mint,
            SPACING,
            PoolRails::none(),
            TOKEN_2022_ID,
        )],
    );

    // The fee-bearing leg's vault was sized for the extension its mint requires;
    // the other leg, whose mint has none, is a plain 165-byte account. This is
    // what makes the deposit reachable: had the vault been created at the plain
    // length, the token program would have refused to initialize it and the pool
    // could not exist at all.
    let vault_fee = svm
        .get_account(&vault_pda(b"clmm-vault-a", pool, fee_mint))
        .expect("the fee leg's vault exists");
    assert_eq!(
        vault_fee.owner, TOKEN_2022_ID,
        "the fee leg's vault is a Token-2022 account"
    );
    assert_eq!(
        vault_fee.data.len(),
        FEE_ACCOUNT_LEN,
        "the vault was sized for the mint's TransferFeeAmount extension"
    );
    assert_eq!(
        svm.get_account(&vault_pda(b"clmm-vault-b", pool, plain_mint))
            .expect("the plain leg's vault exists")
            .data
            .len(),
        165,
        "a mint without extensions needs a plain account"
    );

    let vault_a = vault_pda(b"clmm-vault-a", pool, fee_mint);
    let fee_account_before = token_balance(&svm, fee_account);
    send(
        &mut svm,
        &payer,
        &[Instructions::add_liquidity_of(
            payer_address,
            pool,
            fee_mint,
            plain_mint,
            fee_account,
            plain_account,
            LOWER,
            UPPER,
            LIQUIDITY,
            TOKEN_2022_ID,
        )],
    );

    // The deposit landed in full: the position carries the liquidity the two
    // legs were computed for, and the fee-bearing vault holds exactly that leg.
    // The depositor paid the difference — the mint's fee — rather than the pool
    // being credited short.
    let deposited_a = token_balance(&svm, vault_a);
    assert!(deposited_a > 0, "the fee leg's vault holds the deposit");
    assert_eq!(
        u128_at(
            &data_of(&svm, position_pda(pool, payer_address, LOWER, UPPER)),
            POSITION_LIQUIDITY
        ),
        LIQUIDITY
    );
    assert_eq!(
        fee_account_before - token_balance(&svm, fee_account),
        gross_for_net(deposited_a, FEE_BASIS_POINTS),
        "the depositor sent the vault's amount plus the mint's fee"
    );

    // A payout *in* the fee-bearing mint is the un-grossed half of the rule: the
    // pool is debited exactly what it owes — grossing a payout up would draw on
    // the other LPs' reserves — and the mint's fee comes out of what the
    // recipient receives. Selling B returns A, so this trade runs first, while
    // the price is still at the tick the pool opened on.
    let vault_a_before = token_balance(&svm, vault_a);
    let trader_a_before = token_balance(&svm, fee_account);
    send(
        &mut svm,
        &payer,
        &[Instructions::swap_wired(
            payer_address,
            pool,
            fee_mint,
            plain_mint,
            fee_account,
            plain_account,
            1_000_000,
            false,
            &[UPPER],
            Wired {
                token_program: Some(TOKEN_2022_ID),
                ..Default::default()
            },
        )],
    );
    let paid_out = vault_a_before - token_balance(&svm, vault_a);
    let credited = token_balance(&svm, fee_account) - trader_a_before;
    assert!(paid_out > 0, "the pool paid the trade out of its A vault");
    // The fee the token program charges on the transfer, rounded up as it does.
    let fee = (u128::from(paid_out) * u128::from(FEE_BASIS_POINTS)).div_ceil(10_000);
    assert_eq!(
        u128::from(paid_out - credited),
        fee,
        "the mint's fee came out of the payout, not out of the pool"
    );

    // And the input side is grossed up: selling A credits the vault with the
    // input the curve priced, while the trader pays the fee on top of it.
    let vault_a_before = token_balance(&svm, vault_a);
    let trader_a_before = token_balance(&svm, fee_account);
    let trader_b_before = token_balance(&svm, plain_account);
    send(
        &mut svm,
        &payer,
        &[Instructions::swap_wired(
            payer_address,
            pool,
            fee_mint,
            plain_mint,
            fee_account,
            plain_account,
            10_000_000,
            true,
            &[LOWER, DEEP],
            Wired {
                token_program: Some(TOKEN_2022_ID),
                ..Default::default()
            },
        )],
    );
    let input_received = token_balance(&svm, vault_a) - vault_a_before;
    assert!(input_received > 0, "the pool was credited the swap's input");
    assert_eq!(
        trader_a_before - token_balance(&svm, fee_account),
        gross_for_net(input_received, FEE_BASIS_POINTS),
        "the trader sent the pool's input plus the mint's fee"
    );
    assert!(
        token_balance(&svm, plain_account) > trader_b_before,
        "the trader was paid the output"
    );
}

/// The issuer keeps the fee authority, so a mint can be fee-free while the pool
/// is funded and carry a fee by the time anyone trades. That must not brick the
/// pool: inbound transfers are grossed up by whatever fee is in force, so the
/// vault still nets the input the curve priced and the trader pays the fee on
/// top of it.
#[test]
fn a_fee_raised_after_funding_keeps_the_pool_trading() {
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
    // Fee-free at first, so the pool can be funded at all.
    let (fee_mint, plain_mint, fee_account, plain_account) =
        fee_mint_fixture(&mut svm, &payer, 0, 0);

    let pool = pool_pda(fee_mint, plain_mint);
    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool_of(
            payer_address,
            fee_mint,
            plain_mint,
            SPACING,
            PoolRails::none(),
            TOKEN_2022_ID,
        )],
    );
    send(
        &mut svm,
        &payer,
        &[Instructions::add_liquidity_of(
            payer_address,
            pool,
            fee_mint,
            plain_mint,
            fee_account,
            plain_account,
            LOWER,
            UPPER,
            LIQUIDITY,
            TOKEN_2022_ID,
        )],
    );

    let swap_input = |amount: u64| {
        Instructions::swap_wired(
            payer_address,
            pool,
            fee_mint,
            plain_mint,
            fee_account,
            plain_account,
            amount,
            true,
            &[LOWER, DEEP],
            Wired {
                token_program: Some(TOKEN_2022_ID),
                ..Default::default()
            },
        )
    };

    // Tradable while the fee is zero: this is the pool a trader would rely on.
    // A small trade so the price only nudges, leaving the walk below it intact.
    let vault_a = vault_pda(b"clmm-vault-a", pool, fee_mint);
    let funded_vault_a = token_balance(&svm, vault_a);
    send(&mut svm, &payer, &[swap_input(1_000)]);
    assert_eq!(
        token_balance(&svm, vault_a),
        funded_vault_a + 1_000,
        "the fee-free trade landed in full"
    );

    // The issuer raises the fee to 1% after the pool holds the LPs' money. The
    // token program applies it two epochs on, so the clock has to move first.
    send(
        &mut svm,
        &payer,
        &[Instructions::set_transfer_fee(
            fee_mint,
            payer_address,
            FEE_BASIS_POINTS,
            FEE_MAXIMUM,
        )],
    );
    warp_epochs(&mut svm, 2);

    let trader_a = token_balance(&svm, fee_account);
    let trader_b = token_balance(&svm, plain_account);
    let before_swap_vault_a = token_balance(&svm, vault_a);
    send(&mut svm, &payer, &[swap_input(1_000_000)]);

    // The pool keeps trading. The vault is credited the full input the curve
    // priced — not the fee-skimmed remainder — and the trader pays the newly
    // active fee on top, which is the difference between the two numbers.
    let received = token_balance(&svm, vault_a) - before_swap_vault_a;
    assert!(received > 0, "the trade landed after the fee was activated");
    assert_eq!(
        trader_a - token_balance(&svm, fee_account),
        gross_for_net(received, FEE_BASIS_POINTS),
        "the trader sent the pool's input plus the mint's fee"
    );
    assert!(
        token_balance(&svm, plain_account) > trader_b,
        "the trader was paid the output"
    );
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

    // One finer than the floor: the bitmap would not fit in one account.
    let instruction = Instructions::initialize_pool(
        payer.pubkey(),
        mint_a,
        mint_b,
        13,
        PoolRails::none(),
    );
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
        send_error(
            &mut fixture.svm,
            &fixture.payer,
            &[swap(&[LOWER, UPPER], true)]
        ),
        ERR_TICKS_NOT_IN_ORDER
    );

    // The walk the bitmap describes is accepted, and trades through both ticks.
    let before = token_balance(&fixture.svm, fixture.user_token_b);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[swap(&[LOWER, DEEP], true)],
    );
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

/// Sends a transaction that must fail on an Anchor account constraint (a
/// 2000-range error, which the program-error helper above deliberately does not
/// look for) and returns its error text.
fn send_constraint_error(
    svm: &mut LiteSVM,
    payer: &Keypair,
    instructions: &[Instruction],
) -> String {
    svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        &[payer],
        svm.latest_blockhash(),
    );
    match svm.send_transaction(tx) {
        Ok(_) => panic!("expected the transaction to fail"),
        Err(failure) => format!("{:?}", failure.err),
    }
}

/// A token account that is not the signer's is refused at account validation,
/// on either leg (M-6): the trader cannot route a swap's output through an
/// account they do not control, and cannot be tricked into pulling input from
/// one either.
#[test]
fn a_swap_refuses_token_accounts_the_trader_does_not_own() {
    let mut fixture = setup();
    let payer = fixture.payer.pubkey();
    let stranger = Keypair::new().pubkey();

    // An account of the right mint, but owned by someone else: the output leg.
    let foreign_b = Keypair::new().pubkey();
    create_token_account(&mut fixture.svm, foreign_b, fixture.mint_b, stranger, FUNDS);
    let err = send_constraint_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            foreign_b,
            100_000_000,
            true,
            &[LOWER, DEEP],
        )],
    );
    assert!(
        err.contains("Custom(2015)"),
        "expected a token-owner constraint (`ConstraintTokenOwner`), got {err}"
    );

    // The input leg is bound the same way.
    let foreign_a = Keypair::new().pubkey();
    create_token_account(&mut fixture.svm, foreign_a, fixture.mint_a, stranger, FUNDS);
    let err = send_constraint_error(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            foreign_a,
            fixture.user_token_b,
            100_000_000,
            true,
            &[LOWER, DEEP],
        )],
    );
    assert!(
        err.contains("Custom(2015)"),
        "expected a token-owner constraint (`ConstraintTokenOwner`), got {err}"
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
    assert!(
        earned < 100_000,
        "and only the fee, not the input: {earned}"
    );
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

/// The H-1 fix, end to end: `initialize_pool` creates the pool's oracle with
/// the pool PDA signing, fixing the threshold and unpause owner atomically with
/// the pool. The oracle state then records the pool, and the permissionless
/// `trip_breaker` reached through the pool proves the pool-signer CPI path.
/// The KYC rail on a swap: the gate's attestation is what admits a trade, and it
/// is checked before any of the swap's math runs. The trader here is the wallet
/// whose attestation the gate issued; the stranger holds an empty account of the
/// same mint, and nothing else about the two differs.
#[test]
fn a_kyc_gated_pool_admits_only_the_trader_holding_the_attestation() {
    let mut fixture = setup_kyc();

    // The pool recorded the gate's config as its policy, so every swap is gated.
    let pool_data = data_of(&fixture.svm, fixture.pool);
    let kyc_config_at = pool_data.len() - POOL_WIRING_TAIL;
    assert_eq!(
        address_at(&pool_data, kyc_config_at),
        kyc_config_pda(),
        "the pool's policy names the gate"
    );

    // Copied out of the fixture so the closure does not borrow it: the tests
    // below need `&mut svm` at the same time.
    let (pool, mint_a, mint_b) = (fixture.pool, fixture.mint_a, fixture.mint_b);
    let swap = |user: Address, token_a: Address, token_b: Address, attestation: Address| {
        Instructions::swap_wired(
            user,
            pool,
            mint_a,
            mint_b,
            token_a,
            token_b,
            100_000_000,
            true,
            &[LOWER],
            Wired {
                kyc: Some(attestation),
                ..Wired::default()
            },
        )
    };
    let open_pool = (
        u128_at(&pool_data, POOL_SQRT_PRICE),
        token_balance(&fixture.svm, fixture.stranger_a),
        token_balance(&fixture.svm, fixture.stranger_b),
    );

    // 1. A wallet whose attestation account is empty is refused by the gate —
    //    before the walk is even looked at.
    let code = send_error(
        &mut fixture.svm,
        &fixture.stranger,
        &[swap(
            fixture.stranger.pubkey(),
            fixture.stranger_a,
            fixture.stranger_b,
            fixture.stranger_kyc,
        )],
    );
    assert_eq!(code, ERR_NOT_KYC_APPROVED);

    // 2. Nor can it present someone else's: the attestation is bound to the
    //    wallet that holds it, so borrowing the trader's proves nothing.
    let code = send_error(
        &mut fixture.svm,
        &fixture.stranger,
        &[swap(
            fixture.stranger.pubkey(),
            fixture.stranger_a,
            fixture.stranger_b,
            fixture.trader_kyc,
        )],
    );
    assert_eq!(code, ERR_KYC_WRONG_OWNER);

    // 3. And a client that leaves the gate account out entirely — passing the
    //    program-ID sentinel an unwired pool would use — is refused by the pool's
    //    own policy check, rather than trading ungated.
    let code = send_error(
        &mut fixture.svm,
        &fixture.stranger,
        &[Instructions::swap(
            fixture.stranger.pubkey(),
            pool,
            mint_a,
            mint_b,
            fixture.stranger_a,
            fixture.stranger_b,
            100_000_000,
            true,
            &[LOWER],
        )],
    );
    assert_eq!(code, ERR_RAIL_ACCOUNT_UNEXPECTED);

    // None of the three moved anything, and the price is where it started: a
    // refused gate reverts the transaction, it does not half-trade it.
    assert_eq!(
        (
            u128_at(&data_of(&fixture.svm, fixture.pool), POOL_SQRT_PRICE),
            token_balance(&fixture.svm, fixture.stranger_a),
            token_balance(&fixture.svm, fixture.stranger_b),
        ),
        open_pool
    );

    // 4. The wallet the gate named trades, and the pool pays it out.
    let trader_b_before = token_balance(&fixture.svm, fixture.trader_b);
    send(
        &mut fixture.svm,
        &fixture.trader,
        &[swap(
            fixture.trader.pubkey(),
            fixture.trader_a,
            fixture.trader_b,
            fixture.trader_kyc,
        )],
    );
    assert!(
        token_balance(&fixture.svm, fixture.trader_b) > trader_b_before,
        "the attested trader was paid"
    );
    assert!(
        u128_at(&data_of(&fixture.svm, fixture.pool), POOL_SQRT_PRICE) < open_pool.0,
        "and the swap moved the price"
    );
}

/// The settlement rail on a swap. The trader is not paid: the output goes to the
/// hook authority's custody account and, in the same transaction, into the T+N
/// escrow the settlement program creates for this trade. What the test pins is
/// that the escrow really holds the output and really records the trade — the
/// swap has no other way to settle.
#[test]
fn a_settlement_wired_pool_locks_the_output_in_escrow_instead_of_paying_the_trader() {
    let mut fixture = setup_escrow();
    let payer = fixture.payer.pubkey();

    // The pool's policy names the hook authority the swap must route through.
    let pool_data = data_of(&fixture.svm, fixture.pool);
    let tail = pool_data.len() - POOL_WIRING_TAIL;
    assert_eq!(
        address_at(&pool_data, tail + 64),
        hook_authority_pda(),
        "the pool's settlement policy names the hook authority"
    );
    assert_eq!(u64_at(&pool_data, tail + 96), SETTLEMENT_SLOTS);

    let trader_b_before = token_balance(&fixture.svm, fixture.user_token_b);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap_wired(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            100_000_000,
            true,
            &[LOWER],
            Wired {
                escrow_trade_id: Some(fixture.trade_id),
                ..Wired::default()
            },
        )],
    );

    // The trader was paid nothing, and the custody account did not keep the
    // output either: the whole balance it received went into escrow.
    assert_eq!(
        token_balance(&fixture.svm, fixture.user_token_b),
        trader_b_before,
        "the trader is not paid on a settlement-wired pool"
    );
    assert_eq!(
        token_balance(&fixture.svm, fixture.custody),
        0,
        "the custody account was swept empty"
    );

    // The escrow holds it, and it is owned by the settlement program's fixed
    // escrow authority (not by the vault PDA, which is what keeps the whitelist
    // entry - and therefore the root - stable across trades).
    let escrow = data_of(&fixture.svm, fixture.escrow);
    assert_eq!(address_at(&escrow, 32), settlement_escrow_authority_pda());
    assert_eq!(
        address_at(&escrow, 0),
        fixture.mint_b,
        "the escrow is an account of the output mint"
    );
    let escrowed = token_balance(&fixture.svm, fixture.escrow);
    assert!(escrowed > 0, "the output is in escrow: {escrowed}");

    // And the vault records the trade the trader can now withdraw against.
    let vault = data_of(&fixture.svm, fixture.vault);
    assert_eq!(u64_at(&vault, VAULT_TRADE_ID), fixture.trade_id);
    assert_eq!(
        address_at(&vault, VAULT_TAKER),
        payer,
        "the taker is the wallet that sold"
    );
    assert_eq!(address_at(&vault, VAULT_TOKEN_MINT), fixture.mint_b);
    assert_eq!(
        u64_at(&vault, VAULT_AMOUNT),
        escrowed,
        "the vault records exactly what it escrowed"
    );
    assert_eq!(vault[VAULT_STATUS], STATUS_PENDING);
    assert!(
        u64_at(&vault, VAULT_SETTLEMENT_SLOT) > 0,
        "unlocking at a slot in the future"
    );
    assert!(
        u64_at(&vault, VAULT_EXPIRY_SLOT) > u64_at(&vault, VAULT_SETTLEMENT_SLOT),
        "the release window closes after the settlement slot"
    );
}

/// The custody account is pinned to the hook authority's ATA, not merely to
/// "some account the hook authority owns": the token program lets anyone create
/// a token account whose `owner` is a PDA, since setting the owner needs no
/// signature from it. An ownership check alone would therefore let a caller pick
/// where the trade's output goes.
#[test]
fn the_settlement_rail_refuses_a_custody_account_that_is_not_the_hook_authoritys_ata() {
    let mut fixture = setup_escrow();
    let payer = fixture.payer.pubkey();

    // A token account of the output mint owned by the hook authority — exactly
    // the shape an attacker can create — but not the ATA the pool pays into.
    let impostor = Keypair::new().pubkey();
    create_token_account(
        &mut fixture.svm,
        impostor,
        fixture.mint_b,
        hook_authority_pda(),
        0,
    );

    assert_eq!(
        address_at(&data_of(&fixture.svm, impostor), 32),
        hook_authority_pda(),
        "the impostor is owned by the hook authority, so ownership alone would pass"
    );

    assert_eq!(
        send_error(
            &mut fixture.svm,
            &fixture.payer,
            &[Instructions::swap_wired(
                payer,
                fixture.pool,
                fixture.mint_a,
                fixture.mint_b,
                fixture.user_token_a,
                fixture.user_token_b,
                100_000_000,
                true,
                &[LOWER],
                Wired {
                    escrow_trade_id: Some(fixture.trade_id),
                    custody_override: Some(impostor),
                    ..Wired::default()
                },
            )],
        ),
        ERR_INVALID_CUSTODY_ACCOUNT
    );
}

#[test]
fn initialize_pool_creates_the_oracle_with_the_pool_signing() {
    let mut svm = LiteSVM::new();
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    let clmm = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/clmm.so"
    ))
    .expect("build the program first: `anchor build -p clmm`");
    svm.add_program(PROGRAM_ID, &clmm).unwrap();
    let oracle = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/oracle.so"
    ))
    .expect("build the program first: `anchor build -p oracle`");
    svm.add_program(ORACLE_PROGRAM_ID, &oracle).unwrap();

    let payer_address = payer.pubkey();
    let mint_a = Keypair::new().pubkey();
    let mint_b = Keypair::new().pubkey();
    create_mint(&mut svm, payer_address, mint_a);
    create_mint(&mut svm, payer_address, mint_b);
    let pool = pool_pda(mint_a, mint_b);

    send(
        &mut svm,
        &payer,
        &[Instructions::initialize_pool(
            payer_address,
            mint_a,
            mint_b,
            SPACING,
            PoolRails {
                oracle_state: Some(oracle_pda(pool)),
                ..PoolRails::none()
            },
        )],
    );

    let state = data_of(&svm, oracle_pda(pool));
    assert_eq!(
        address_at(&state, 8),
        pool,
        "the oracle records the pool that signed for it"
    );
    assert_eq!(
        address_at(&state, 40),
        payer_address,
        "the unpause owner is the pool's admin"
    );
    assert_eq!(
        u16::from_le_bytes(state[76..78].try_into().unwrap()),
        488,
        "the threshold is fixed at pool creation"
    );

    // Re-wiring the same pool cannot create a second oracle: the pool account
    // already exists, so `initialize_pool` fails and the original config stands.
    let retry = svm.send_transaction(Transaction::new_signed_with_payer(
        &[Instructions::initialize_pool(
            payer_address,
            mint_a,
            mint_b,
            SPACING,
            PoolRails {
                oracle_state: Some(oracle_pda(pool)),
                ..PoolRails::none()
            },
        )],
        Some(&payer.pubkey()),
        &[&payer],
        svm.latest_blockhash(),
    ));
    assert!(retry.is_err(), "a pool cannot be initialized twice");

    // The permissionless trip instruction reaches the oracle through the pool
    // (which signs). With no price history yet it is a no-op, which is exactly
    // what proves the pool-signed CPI path works.
    send(&mut svm, &payer, &[Instructions::trip_breaker(pool)]);
}

/// The oracle rail on a swap, end to end. Both of the handler's oracle CPIs run
/// here: `check_deviation` gates the trade before the walk is priced, and
/// `record_observation` writes what the trade settled at afterwards. The test
/// then lets the breaker's 30-minute window close over an old price and shows
/// the guard refusing a trade at the new one — which is the only way to prove the
/// pre-trade check is reached with a live TWAP, and not merely skipped.
#[test]
fn a_wired_pool_circuit_breaks_itself_and_records_every_settlement() {
    let mut fixture = setup_wired();
    let oracle = oracle_pda(fixture.pool);
    let payer = fixture.payer.pubkey();

    // The oracle the pool created names the pool, and carries the threshold the
    // pool was created with.
    let state = data_of(&fixture.svm, oracle);
    assert_eq!(address_at(&state, 8), fixture.pool);
    assert_eq!(
        u16::from_le_bytes(
            state[STATE_THRESHOLD..STATE_THRESHOLD + 2]
                .try_into()
                .unwrap()
        ),
        WIRED_THRESHOLD_BPS
    );
    assert_eq!(
        newest_observation(&fixture.svm, oracle).1,
        0,
        "the genesis observation records no price yet"
    );

    // 1. The first trade. With no price history the breaker has nothing to
    //    compare against, so it allows the trade and logs why — and the pool
    //    then records the price it settled at, which is the observation the
    //    next check is measured against.
    let start = 1_000_000;
    set_time(&mut fixture.svm, start);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap_wired(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            WIRED_AMOUNT,
            true,
            &[WIRED_LOWER],
            Wired {
                oracle: Some(oracle),
                ..Wired::default()
            },
        )],
    );

    let settled = u128_at(&data_of(&fixture.svm, fixture.pool), POOL_SQRT_PRICE);
    assert!(
        settled < SQRT_PRICE_1,
        "selling A moved the price down: {settled}"
    );
    let (observed_at, observed_price, _) = newest_observation(&fixture.svm, oracle);
    assert_eq!(
        observed_at, start,
        "the settlement was recorded in the trade's own transaction"
    );
    assert_eq!(
        observed_price, settled,
        "and the recorded price is the one the pool settled at"
    );

    // 2. Let the window close over that price: the TWAP over the last 30 minutes
    //    is now, to the second, the price the pool has been sitting at. A trade
    //    priced from it is inside the threshold, so it executes — and moves the
    //    price a long way in the process.
    set_time(&mut fixture.svm, start + WINDOW_SECONDS);
    send(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap_wired(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            WIRED_AMOUNT,
            true,
            &[WIRED_DEEP],
            Wired {
                oracle: Some(oracle),
                ..Wired::default()
            },
        )],
    );

    let (_, second_price, cumulative) = newest_observation(&fixture.svm, oracle);
    assert!(
        second_price < settled,
        "the second trade settled lower still: {second_price}"
    );
    assert!(
        cumulative > 0,
        "...and its observation carries the accumulated time at the first price"
    );

    // 3. The next trade is priced from the new spot, while the TWAP still
    //    reflects the 30 minutes the pool spent at the old one. That is a
    //    deviation of ~8.6% of sqrt price against a 4.88% policy, and the guard
    //    refuses it — in the oracle, before the walk is ever priced.
    let (code, logs) = send_error_logs(
        &mut fixture.svm,
        &fixture.payer,
        &[Instructions::swap_wired(
            payer,
            fixture.pool,
            fixture.mint_a,
            fixture.mint_b,
            fixture.user_token_a,
            fixture.user_token_b,
            WIRED_AMOUNT,
            true,
            &[WIRED_DEEP],
            Wired {
                oracle: Some(oracle),
                ..Wired::default()
            },
        )],
    );
    assert_eq!(code, ERR_PRICE_DEVIATION_EXCEEDED);
    assert!(
        logs.contains("Price deviation exceeded"),
        "the refusal is the oracle's, not this program's:\n{logs}"
    );

    // A refused trade writes nothing: the recorded history still ends at the
    // second settlement, so the next trade is judged against the same TWAP.
    let (last_at, last_price, _) = newest_observation(&fixture.svm, oracle);
    assert_eq!(last_at, start + WINDOW_SECONDS);
    assert_eq!(last_price, second_price);
}

