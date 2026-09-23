//! End-to-end tests: the compiled kyc_gate program, on a LiteSVM, against the
//! real Token-2022 program and real signed transactions.
//!
//! The unit tests inside the program crate exercise the attestation policy, the
//! account size and the mint's extension size. These exercise what no unit test
//! can reach: the Token-2022 CPIs that create the soulbound mint, mint and burn
//! under a PDA authority, and the account requirements the token program itself
//! enforces. They pin down, in particular:
//!
//! * `initialize_kyc_mint` really creates a Token-2022 mint of the length the
//!   token program demands, carrying the `NonTransferable` extension, with the
//!   config PDA — not a wallet — as mint *and* freeze authority, and records the
//!   config the pools will read.
//! * only the configured authority can issue or revoke; an impostor is refused
//!   before the token program is even called.
//! * an attestation is binary: one token, a second issue refused, a revoke that
//!   leaves the account empty, and a second revoke refused.
//! * the authority can be rotated, and the rotation is total: the outgoing key
//!   loses `issue_kyc` / `revoke_kyc` at once, the incoming one gains them, and
//!   attestations already issued survive it untouched.
//! * `verify_kyc` accepts exactly the wallet that holds an attestation of the
//!   right mint, and reports *why* it refuses otherwise.
//! * the token program refuses to transfer the attestation — the property the
//!   whole design rests on — and refuses to mint one into an account that does
//!   not carry the extensions a non-transferable mint requires.
//!
//! They need the SBF build, which `cargo test` does not produce:
//!
//! ```bash
//! anchor build -p kyc_gate && (cd tests/e2e && cargo test --test kyc_gate)
//! ```

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
/// `programs/kyc_gate/src/lib.rs`.
const PROGRAM_ID: Address = address!("7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1");
/// Token-2022. LiteSVM bundles the `spl_token_2022-11.0.0` image under this id,
/// so these tests run against the real token program, not a mock.
const TOKEN_2022_ID: Address = address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");
/// The BPF upgradeable loader, which owns each program's `ProgramData` account.
/// The initializer check reads the authority recorded there, so these tests have
/// to write it: `add_program` leaves the field unset, which is exactly the state
/// `initialize_kyc_mint` must refuse.
const BPF_LOADER_UPGRADEABLE_ID: Address = address!("BPFLoaderUpgradeab1e11111111111111111111111");

/// Anchor instruction discriminators (`sha256("global:<name>")[..8]`), taken
/// from `target/idl/kyc_gate.json` (a wrong one fails the first transaction
/// loudly).
const IX_INITIALIZE_KYC_MINT: [u8; 8] = [9, 191, 40, 212, 165, 170, 113, 188];
const IX_ISSUE_KYC: [u8; 8] = [17, 74, 181, 26, 37, 126, 173, 108];
const IX_REVOKE_KYC: [u8; 8] = [170, 100, 20, 190, 192, 242, 236, 232];
const IX_VERIFY_KYC: [u8; 8] = [102, 127, 254, 101, 12, 246, 86, 71];
const IX_SET_AUTHORITY: [u8; 8] = [133, 250, 37, 21, 110, 163, 26, 121];

/// Program errors this test asserts on, in the enum's order.
const ERR_UNAUTHORIZED: u32 = 6000;
const ERR_NOT_KYC_APPROVED: u32 = 6001;
const ERR_WRONG_MINT: u32 = 6002;
const ERR_WRONG_OWNER: u32 = 6003;
const ERR_ALREADY_APPROVED: u32 = 6004;
const ERR_INVALID_AUTHORITY: u32 = 6005;
const ERR_UNAUTHORIZED_INITIALIZER: u32 = 6006;

/// `KycConfig` layout: discriminator, authority, mint, bump, mint bump.
const CONFIG_LEN: usize = 74;
const CONFIG_AUTHORITY: usize = 8;
const CONFIG_MINT: usize = 40;
const CONFIG_MINT_BUMP: usize = 73;

/// Token-2022 mint layout for a mint carrying `NonTransferable` and
/// `PermanentDelegate`: the 82-byte base mint, zero padding up to the base-*account*
/// length, the account-type byte the token program puts at that offset for both
/// mints and accounts, then one TLV per extension. `try_calculate_account_len`
/// returns exactly this, and `process_initialize_mint` demands the match.
const MINT_LEN: usize = 206;
const MINT_ACCOUNT_TYPE: usize = 165;
const MINT_TLV: usize = 166;
/// The permanent delegate's TLV follows the zero-length non-transferable entry.
const MINT_TLV_PERMANENT_DELEGATE: usize = MINT_TLV + 4;
/// Base mint fields, shared with the legacy SPL layout.
const MINT_AUTHORITY_TAG: usize = 0;
const MINT_AUTHORITY: usize = 4;
const MINT_SUPPLY: usize = 36;
const MINT_DECIMALS: usize = 44;
const MINT_IS_INITIALIZED: usize = 45;
const MINT_FREEZE_AUTHORITY_TAG: usize = 46;
const MINT_FREEZE_AUTHORITY: usize = 50;

/// `ExtensionType` discriminants and the account-type byte (`AccountType`).
const EXT_IMMUTABLE_OWNER: u16 = 7;
const EXT_NON_TRANSFERABLE: u16 = 9;
const EXT_PERMANENT_DELEGATE: u16 = 12;
const EXT_NON_TRANSFERABLE_ACCOUNT: u16 = 13;
const ACCOUNT_TYPE_MINT: u8 = 1;
const ACCOUNT_TYPE_ACCOUNT: u8 = 2;
/// A `PodCOption::Some` tag, and the `TokenAccount` layout the token program
/// uses for an account with no extensions (identical to the legacy layout).
const OPTION_SOME: u32 = 1;
const TOKEN_ACCOUNT_BASE_LEN: usize = 165;
const TOKEN_MINT: usize = 0;
const TOKEN_OWNER: usize = 32;
const TOKEN_AMOUNT: usize = 64;
const TOKEN_STATE: usize = 108;
const TOKEN_STATE_INITIALIZED: u8 = 1;
/// An attestation account: the base account, the account-type byte, and the two
/// zero-length extensions a non-transferable mint requires. This is what
/// `ExtensionType::try_calculate_account_len::<Account>` returns for
/// `[ImmutableOwner, NonTransferableAccount]`, and the minimum length Token-2022
/// accepts when it initializes such an account.
const KYC_ACCOUNT_LEN: usize = TOKEN_ACCOUNT_BASE_LEN + 1 + 4 + 4;

/// Token-2022 instruction discriminants, unchanged from the legacy program for
/// these two (the new instructions were appended).
const TOKEN_IX_TRANSFER_CHECKED: u8 = 12;
const TOKEN_IX_INITIALIZE_ACCOUNT_3: u8 = 18;
/// The system program's `CreateAccount` (discriminant 0: lamports, space, owner).
const SYSTEM_IX_CREATE_ACCOUNT: u32 = 0;

const LAMPORTS: u64 = 10_000_000_000;
/// Enough for a small account to be rent-exempt, so the authorities' airdrops
/// are not spent on a single account.
const ACCOUNT_LAMPORTS: u64 = 5_000_000;

// -----------------------------------------------------------------------
// Test harness
// -----------------------------------------------------------------------

/// LiteSVM's blockhash is static, so identical transactions would collide on
/// signature (`AlreadyProcessed`). Expiring it before each send chains a new
/// hash, giving every transaction a distinct, valid blockhash.
fn fresh_blockhash(svm: &mut LiteSVM) -> solana_hash::Hash {
    svm.expire_blockhash();
    svm.latest_blockhash()
}

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

fn send(svm: &mut LiteSVM, payer: &Keypair, instructions: &[Instruction]) {
    send_with(svm, payer, &[payer], instructions);
}

/// Sends a transaction that must fail, and returns the program error code if the
/// failure was a program error (None for e.g. a token-program error, which is
/// reported as a custom code from a different program).
fn send_error(
    svm: &mut LiteSVM,
    payer: &Keypair,
    signers: &[&Keypair],
    instructions: &[Instruction],
) -> String {
    let tx = Transaction::new_signed_with_payer(
        instructions,
        Some(&payer.pubkey()),
        signers,
        fresh_blockhash(svm),
    );
    match svm.send_transaction(tx) {
        Ok(_) => panic!("expected the transaction to fail"),
        Err(failure) => format!("{:?}", failure.err),
    }
}

/// The kyc_gate error code out of a failure, if the failure was one of ours.
fn kyc_error_code(failure: &str) -> Option<u32> {
    (6000..6100).find(|code| failure.contains(&format!("Custom({code})")))
}

/// Sends a transaction that must fail with one of this program's errors.
fn send_kyc_error(
    svm: &mut LiteSVM,
    payer: &Keypair,
    signers: &[&Keypair],
    instructions: &[Instruction],
    expected: u32,
) {
    let failure = send_error(svm, payer, signers, instructions);
    match kyc_error_code(&failure) {
        Some(code) => assert_eq!(code, expected, "wrong kyc_gate error: {failure}"),
        None => panic!("expected kyc_gate error {expected}, got {failure}"),
    }
}

fn data_of(svm: &LiteSVM, address: Address) -> Vec<u8> {
    svm.get_account(&address)
        .unwrap_or_else(|| panic!("account {address} does not exist"))
        .data
}

/// The loader's `ProgramData` account for the program under test: the account
/// the upgrade authority is read from, at `[program]` under the loader.
fn program_data_pda() -> Address {
    Address::find_program_address(&[PROGRAM_ID.as_ref()], &BPF_LOADER_UPGRADEABLE_ID).0
}

/// Marks `authority` as the program's upgrade authority by writing the metadata
/// the loader keeps in the head of `ProgramData` - the 4-byte `ProgramData` tag,
/// the slot, an `Option` tag of 1 and the authority - leaving the deployed
/// program bytes untouched. On a real cluster only an upgrade transaction can
/// write this; here it stands in for one, so initialization can be exercised the
/// way a real deployer would reach it.
fn set_upgrade_authority(svm: &mut LiteSVM, authority: Address) {
    let address = program_data_pda();
    let mut account = svm
        .get_account(&address)
        .expect("add_program creates the ProgramData account");
    assert!(
        account.data.len() >= 45,
        "the loader's metadata header is 45 bytes"
    );
    account.data[0..4].copy_from_slice(&3u32.to_le_bytes()); // AccountType::ProgramData
    account.data[12] = 1; // Option::Some
    account.data[13..45].copy_from_slice(authority.as_ref());
    svm.set_account(address, account).unwrap();
}

fn u16_at(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn address_at(data: &[u8], offset: usize) -> Address {
    Address::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

fn set_raw_account(svm: &mut LiteSVM, address: Address, data: Vec<u8>, owner: Address) {
    svm.set_account(
        address,
        Account {
            lamports: ACCOUNT_LAMPORTS,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// A bare Token-2022 account for `mint` owned by `owner`, with no extensions.
/// Written straight into the account because the token program will not
/// initialize such an account for a mint that requires extensions — which is
/// exactly the point of the test that uses it.
fn plain_token_account_data(mint: Address, owner: Address, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; TOKEN_ACCOUNT_BASE_LEN];
    data[TOKEN_MINT..TOKEN_MINT + 32].copy_from_slice(mint.as_ref());
    data[TOKEN_OWNER..TOKEN_OWNER + 32].copy_from_slice(owner.as_ref());
    data[TOKEN_AMOUNT..TOKEN_AMOUNT + 8].copy_from_slice(&amount.to_le_bytes());
    data[TOKEN_STATE] = TOKEN_STATE_INITIALIZED;
    data
}

/// Creates a Token-2022 account for the attestation mint through the token
/// program — the only path that writes the extensions this mint requires — and
/// returns its address.
fn token_program_account(svm: &mut LiteSVM, payer: &Keypair, owner: Address) -> Address {
    let account = Keypair::new();
    send_with(
        svm,
        payer,
        &[payer, &account],
        &[
            Instructions::create_account(
                payer.pubkey(),
                account.pubkey(),
                ACCOUNT_LAMPORTS,
                KYC_ACCOUNT_LEN as u64,
                TOKEN_2022_ID,
            ),
            Instructions::initialize_account_3(account.pubkey(), kyc_mint_pda(), owner),
        ],
    );
    account.pubkey()
}

/// The extension types an account's TLV region declares, in order.
fn extension_types(data: &[u8], tlv_start: usize) -> Vec<u16> {
    let mut types = Vec::new();
    let mut at = tlv_start;
    while at + 4 <= data.len() {
        let kind = u16_at(data, at);
        let len = u16_at(data, at + 2) as usize;
        if kind == 0 && len == 0 {
            break; // trailing zero padding is not an entry
        }
        types.push(kind);
        at += 4 + len;
    }
    types
}

// -----------------------------------------------------------------------
// PDAs
// -----------------------------------------------------------------------

fn kyc_config_pda() -> Address {
    Address::find_program_address(&[b"kyc-config"], &PROGRAM_ID).0
}

fn kyc_mint_pda() -> Address {
    Address::find_program_address(&[b"kyc-mint"], &PROGRAM_ID).0
}

// -----------------------------------------------------------------------
// Instructions
// -----------------------------------------------------------------------

struct Instructions;

impl Instructions {
    fn initialize_kyc_mint(authority: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(kyc_config_pda(), false),
                AccountMeta::new(kyc_mint_pda(), false),
                AccountMeta::new(authority, true),
                AccountMeta::new_readonly(program_data_pda(), false),
                AccountMeta::new_readonly(TOKEN_2022_ID, false),
                AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            ],
            data: IX_INITIALIZE_KYC_MINT.to_vec(),
        }
    }

    fn issue_kyc(authority: Address, institution_kyc_account: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
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

    fn revoke_kyc(authority: Address, institution_kyc_account: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(kyc_config_pda(), false),
                AccountMeta::new(kyc_mint_pda(), false),
                AccountMeta::new(institution_kyc_account, false),
                AccountMeta::new_readonly(authority, true),
                AccountMeta::new_readonly(TOKEN_2022_ID, false),
            ],
            data: IX_REVOKE_KYC.to_vec(),
        }
    }

    fn verify_kyc(user: Address, user_kyc_account: Address) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new_readonly(kyc_config_pda(), false),
                AccountMeta::new_readonly(user, true),
                AccountMeta::new_readonly(user_kyc_account, false),
            ],
            data: IX_VERIFY_KYC.to_vec(),
        }
    }

    /// Rotates the config's authority. The incoming key is an argument rather
    /// than an account, so it may be a multisig vault PDA that cannot sign a
    /// transaction of its own.
    fn set_authority(authority: Address, new_authority: Address) -> Instruction {
        let mut data = IX_SET_AUTHORITY.to_vec();
        data.extend_from_slice(new_authority.as_ref());
        Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(kyc_config_pda(), false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }

    /// The system program's `CreateAccount`, so a test can create an account the
    /// way an integrator would rather than writing bytes straight into the VM.
    fn create_account(
        from: Address,
        to: Address,
        lamports: u64,
        space: u64,
        owner: Address,
    ) -> Instruction {
        let mut data = SYSTEM_IX_CREATE_ACCOUNT.to_le_bytes().to_vec();
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
    /// requires on the new account, which is how ImmutableOwner and
    /// NonTransferableAccount are meant to be added.
    fn initialize_account_3(account: Address, mint: Address, owner: Address) -> Instruction {
        let mut data = vec![TOKEN_IX_INITIALIZE_ACCOUNT_3];
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

    /// The token program's own `TransferChecked`, used to prove the attestation
    /// cannot be moved.
    fn transfer_checked(
        source: Address,
        mint: Address,
        destination: Address,
        authority: Address,
        amount: u64,
        decimals: u8,
    ) -> Instruction {
        let mut data = vec![TOKEN_IX_TRANSFER_CHECKED];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(decimals);
        Instruction {
            program_id: TOKEN_2022_ID,
            accounts: vec![
                AccountMeta::new(source, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(authority, true),
            ],
            data,
        }
    }
}

// -----------------------------------------------------------------------
// Fixture
// -----------------------------------------------------------------------

struct Fixture {
    svm: LiteSVM,
    /// The KYC authority (and payer).
    authority: Keypair,
    /// An approved institution: holds the attestation, signs its own swaps.
    institution: Keypair,
    /// That institution's Token-2022 account for the attestation mint.
    institution_kyc_account: Address,
}

/// Boots a LiteSVM with the program and the Token-2022 image, initializes the
/// attestation mint, and onboards one institution with the token account an
/// integrator would give it.
fn setup() -> Fixture {
    let mut svm = LiteSVM::new();
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), LAMPORTS).unwrap();
    let program = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/kyc_gate.so"
    ))
    .expect("build the program first: `anchor build -p kyc_gate`");
    svm.add_program(PROGRAM_ID, &program).unwrap();
    // Initialization is the upgrade authority's to perform, as it is on a real
    // deploy where the authority is set by the deploying upgrade transaction.
    set_upgrade_authority(&mut svm, authority.pubkey());

    send(
        &mut svm,
        &authority,
        &[Instructions::initialize_kyc_mint(authority.pubkey())],
    );

    // The institution's attestation account, created the way an integrator
    // creates one: an account sized for the extensions this mint requires, then
    // Token-2022's `InitializeAccount3` writes those extensions itself.
    let institution = Keypair::new();
    svm.airdrop(&institution.pubkey(), LAMPORTS).unwrap();
    let institution_kyc_account = Keypair::new();
    send_with(
        &mut svm,
        &authority,
        &[&authority, &institution_kyc_account],
        &[
            Instructions::create_account(
                authority.pubkey(),
                institution_kyc_account.pubkey(),
                ACCOUNT_LAMPORTS,
                KYC_ACCOUNT_LEN as u64,
                TOKEN_2022_ID,
            ),
            Instructions::initialize_account_3(
                institution_kyc_account.pubkey(),
                kyc_mint_pda(),
                institution.pubkey(),
            ),
        ],
    );

    Fixture {
        svm,
        authority,
        institution,
        institution_kyc_account: institution_kyc_account.pubkey(),
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

/// The authority the config currently names.
fn config_authority(svm: &LiteSVM) -> Address {
    address_at(&data_of(svm, kyc_config_pda()), CONFIG_AUTHORITY)
}

#[test]
fn initialize_creates_a_soulbound_mint_owned_by_the_config_pda() {
    let fixture = setup();

    // The config names the authority and the mint, and caches both bumps.
    let config = data_of(&fixture.svm, kyc_config_pda());
    assert_eq!(config.len(), CONFIG_LEN, "KycConfig::LEN bytes");
    assert_eq!(
        address_at(&config, CONFIG_AUTHORITY),
        fixture.authority.pubkey()
    );
    assert_eq!(address_at(&config, CONFIG_MINT), kyc_mint_pda());
    assert!(
        config[CONFIG_MINT_BUMP] > 0,
        "the mint's bump is cached for clients"
    );

    // The mint is a Token-2022 mint of exactly the length the token program
    // insists on, carrying the NonTransferable extension.
    let mint = data_of(&fixture.svm, kyc_mint_pda());
    assert_eq!(
        mint.len(),
        MINT_LEN,
        "the token program demands this exact length"
    );
    assert_eq!(
        fixture.svm.get_account(&kyc_mint_pda()).unwrap().owner,
        TOKEN_2022_ID
    );
    assert_eq!(mint[MINT_ACCOUNT_TYPE], ACCOUNT_TYPE_MINT);
    assert_eq!(u16_at(&mint, MINT_TLV), EXT_NON_TRANSFERABLE);
    assert_eq!(u16_at(&mint, MINT_TLV + 2), 0, "a zero-length extension");
    // ...and the permanent delegate, which is the config PDA: revoking must not
    // need the institution's signature, and Token-2022 will not let the mint
    // authority burn anyone's tokens.
    assert_eq!(
        u16_at(&mint, MINT_TLV_PERMANENT_DELEGATE),
        EXT_PERMANENT_DELEGATE
    );
    assert_eq!(u16_at(&mint, MINT_TLV_PERMANENT_DELEGATE + 2), 32);
    assert_eq!(
        address_at(&mint, MINT_TLV_PERMANENT_DELEGATE + 4),
        kyc_config_pda()
    );

    // The authority of the mint - to mint and to freeze - is the config PDA, not
    // a wallet: no key can issue or burn an attestation behind the program's back.
    assert_eq!(u32_at(&mint, MINT_AUTHORITY_TAG), OPTION_SOME);
    assert_eq!(address_at(&mint, MINT_AUTHORITY), kyc_config_pda());
    assert_eq!(u32_at(&mint, MINT_FREEZE_AUTHORITY_TAG), OPTION_SOME);
    assert_eq!(address_at(&mint, MINT_FREEZE_AUTHORITY), kyc_config_pda());

    // Zero decimals: the attestation is a flag, not a currency, and nothing has
    // been minted yet.
    assert_eq!(mint[MINT_DECIMALS], 0);
    assert_eq!(mint[MINT_IS_INITIALIZED], 1);
    assert_eq!(u64_at(&mint, MINT_SUPPLY), 0);

    // Nothing else was left in the padding the token program allocates.
    assert!(
        mint[82..MINT_ACCOUNT_TYPE].iter().all(|byte| *byte == 0),
        "the padding between the base mint and the account type stays zero"
    );
}

#[test]
fn initialize_is_the_upgrade_authoritys_to_perform() {
    // Booted by hand rather than through `setup`, which records an authority:
    // this test needs to observe the state before one exists.
    let mut svm = LiteSVM::new();
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), LAMPORTS).unwrap();
    let impostor = Keypair::new();
    svm.airdrop(&impostor.pubkey(), LAMPORTS).unwrap();
    let program = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/kyc_gate.so"
    ))
    .expect("build the program first: `anchor build -p kyc_gate`");
    svm.add_program(PROGRAM_ID, &program).unwrap();

    // With no upgrade authority on record - the state a program is in before it
    // is deployed by an upgrade - initialization is closed to everyone.
    send_kyc_error(
        &mut svm,
        &impostor,
        &[&impostor],
        &[Instructions::initialize_kyc_mint(impostor.pubkey())],
        ERR_UNAUTHORIZED_INITIALIZER,
    );
    assert!(
        svm.get_account(&kyc_config_pda()).is_none(),
        "a refused initialization creates nothing"
    );

    // Once one is recorded, a stranger who pays for the accounts is still
    // refused - otherwise the first caller would come to own the gate.
    set_upgrade_authority(&mut svm, authority.pubkey());
    send_kyc_error(
        &mut svm,
        &impostor,
        &[&impostor],
        &[Instructions::initialize_kyc_mint(impostor.pubkey())],
        ERR_UNAUTHORIZED_INITIALIZER,
    );
    assert!(svm.get_account(&kyc_config_pda()).is_none());
    assert!(svm.get_account(&kyc_mint_pda()).is_none());

    // And the recorded key may create the gate, naming itself the authority.
    send(
        &mut svm,
        &authority,
        &[Instructions::initialize_kyc_mint(authority.pubkey())],
    );
    assert_eq!(config_authority(&svm), authority.pubkey());
}

#[test]
fn only_the_authority_may_issue_or_revoke() {
    let mut fixture = setup();
    let impostor = Keypair::new();
    fixture.svm.airdrop(&impostor.pubkey(), LAMPORTS).unwrap();

    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &impostor],
        &[Instructions::issue_kyc(
            impostor.pubkey(), // signs, but is not the configured authority
            fixture.institution_kyc_account,
        )],
        ERR_UNAUTHORIZED,
    );

    // Nothing was issued on the way out.
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);
    assert_eq!(u64_at(&account, TOKEN_AMOUNT), 0);
}

#[test]
fn the_authority_can_be_rotated_and_the_old_one_locked_out() {
    let mut fixture = setup();
    let successor = Keypair::new();
    fixture.svm.airdrop(&successor.pubkey(), LAMPORTS).unwrap();
    let impostor = Keypair::new();
    fixture.svm.airdrop(&impostor.pubkey(), LAMPORTS).unwrap();

    // An attestation is issued first, so the rotation can be shown not to disturb
    // the institutions already holding one.
    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
    );

    // A stranger cannot rotate it: the config's own authority must sign.
    send_kyc_error(
        &mut fixture.svm,
        &impostor,
        &[&impostor],
        &[Instructions::set_authority(
            impostor.pubkey(),
            impostor.pubkey(),
        )],
        ERR_UNAUTHORIZED,
    );
    assert_eq!(
        config_authority(&fixture.svm),
        fixture.authority.pubkey(),
        "a refused rotation changes nothing"
    );

    // The default pubkey is refused: a typo there would hand the gate to a key
    // nobody controls, and there is no other instruction that could take it back.
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority],
        &[Instructions::set_authority(
            fixture.authority.pubkey(),
            Address::default(),
        )],
        ERR_INVALID_AUTHORITY,
    );
    assert_eq!(config_authority(&fixture.svm), fixture.authority.pubkey());

    // The rotation. The successor does not sign it - the outgoing authority does -
    // which is what lets the new authority be a vault PDA.
    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::set_authority(
            fixture.authority.pubkey(),
            successor.pubkey(),
        )],
    );
    assert_eq!(config_authority(&fixture.svm), successor.pubkey());

    // The outgoing key is locked out of both sides of the gate...
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority],
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
        ERR_UNAUTHORIZED,
    );
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority],
        &[Instructions::revoke_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
        ERR_UNAUTHORIZED,
    );

    // ...while the attestation issued before the rotation is untouched, and still
    // verifies for the institution holding it.
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);
    assert_eq!(u64_at(&account, TOKEN_AMOUNT), 1);
    send(
        &mut fixture.svm,
        &fixture.institution,
        &[Instructions::verify_kyc(
            fixture.institution.pubkey(),
            fixture.institution_kyc_account,
        )],
    );

    // The incoming key stands in exactly as the outgoing one did: it revokes here,
    // and - because the mint's authorities are the config PDA, never a key - it
    // still cannot mint or burn behind the program's back.
    let mint = data_of(&fixture.svm, kyc_mint_pda());
    assert_eq!(address_at(&mint, MINT_AUTHORITY), kyc_config_pda());
    assert_eq!(
        address_at(&mint, MINT_TLV_PERMANENT_DELEGATE + 4),
        kyc_config_pda()
    );

    send(
        &mut fixture.svm,
        &successor,
        &[Instructions::revoke_kyc(
            successor.pubkey(),
            fixture.institution_kyc_account,
        )],
    );
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);
    assert_eq!(u64_at(&account, TOKEN_AMOUNT), 0);

    // And it can hand the role on in turn, so the gate is never bound to one key.
    send(
        &mut fixture.svm,
        &successor,
        &[Instructions::set_authority(
            successor.pubkey(),
            fixture.authority.pubkey(),
        )],
    );
    assert_eq!(config_authority(&fixture.svm), fixture.authority.pubkey());
}

#[test]
fn an_attestation_is_binary_and_reversible() {
    let mut fixture = setup();
    let institution = fixture.institution.pubkey();

    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
    );
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);
    assert_eq!(u64_at(&account, TOKEN_AMOUNT), 1, "one attestation token");
    // And the mint recorded the supply.
    let mint = data_of(&fixture.svm, kyc_mint_pda());
    assert_eq!(u64_at(&mint, MINT_SUPPLY), 1);

    // A second issue would break the one-token-per-institution invariant that
    // makes a revoke a complete revoke, so it is refused.
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority],
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
        ERR_ALREADY_APPROVED,
    );

    // The institution is approved...
    send_with(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &fixture.institution],
        &[Instructions::verify_kyc(
            institution,
            fixture.institution_kyc_account,
        )],
    );

    // ...until the authority revokes, which burns the token.
    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::revoke_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
    );
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);
    assert_eq!(u64_at(&account, TOKEN_AMOUNT), 0, "the burn left nothing");
    assert_eq!(
        u64_at(&data_of(&fixture.svm, kyc_mint_pda()), MINT_SUPPLY),
        0
    );

    // Revoking again has nothing to burn, and is refused by this program rather
    // than by the token program.
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority],
        &[Instructions::revoke_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
        ERR_NOT_KYC_APPROVED,
    );

    // And the institution is refused by the pool's check.
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &fixture.institution],
        &[Instructions::verify_kyc(
            institution,
            fixture.institution_kyc_account,
        )],
        ERR_NOT_KYC_APPROVED,
    );
}

#[test]
fn verify_kyc_approves_only_this_wallets_attestation() {
    let mut fixture = setup();
    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
    );

    // Another wallet's attestation does not approve this one: holding one is not
    // the check, holding *this wallet's* is. The other wallet really is approved
    // (it holds one), so the refusal can only be about the owner.
    let other = Keypair::new();
    let other_account = token_program_account(&mut fixture.svm, &fixture.authority, other.pubkey());
    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            other_account,
        )],
    );
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &fixture.institution],
        &[Instructions::verify_kyc(
            fixture.institution.pubkey(),
            other_account,
        )],
        ERR_WRONG_OWNER,
    );

    // An account of some other mint cannot stand in for an attestation.
    let foreign_mint = Keypair::new().pubkey();
    let foreign_account = Keypair::new().pubkey();
    set_raw_account(
        &mut fixture.svm,
        foreign_account,
        plain_token_account_data(foreign_mint, fixture.institution.pubkey(), 1),
        TOKEN_2022_ID,
    );
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &fixture.institution],
        &[Instructions::verify_kyc(
            fixture.institution.pubkey(),
            foreign_account,
        )],
        ERR_WRONG_MINT,
    );

    // An empty account of the right mint and owner holds no attestation.
    let empty_account = token_program_account(
        &mut fixture.svm,
        &fixture.authority,
        fixture.institution.pubkey(),
    );
    send_kyc_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &fixture.institution],
        &[Instructions::verify_kyc(
            fixture.institution.pubkey(),
            empty_account,
        )],
        ERR_NOT_KYC_APPROVED,
    );

    // The wallet that does hold one passes.
    send_with(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority, &fixture.institution],
        &[Instructions::verify_kyc(
            fixture.institution.pubkey(),
            fixture.institution_kyc_account,
        )],
    );
}

#[test]
fn the_attestation_cannot_be_transferred() {
    let mut fixture = setup();
    send(
        &mut fixture.svm,
        &fixture.authority,
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            fixture.institution_kyc_account,
        )],
    );

    // A second wallet's account for the same mint, to transfer into.
    let buyer = Keypair::new();
    let buyer_account = token_program_account(&mut fixture.svm, &fixture.authority, buyer.pubkey());

    // The token program enforces non-transferability, so the attestation cannot
    // be sold, lent, or moved: the transfer fails inside Token-2022, not here.
    let failure = send_error(
        &mut fixture.svm,
        &fixture.institution,
        &[&fixture.institution],
        &[Instructions::transfer_checked(
            fixture.institution_kyc_account,
            kyc_mint_pda(),
            buyer_account,
            fixture.institution.pubkey(),
            1,
            0,
        )],
    );
    assert!(
        kyc_error_code(&failure).is_none(),
        "the refusal must come from the token program, not from kyc_gate: {failure}"
    );

    // Nothing moved.
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);
    assert_eq!(u64_at(&account, TOKEN_AMOUNT), 1);
    let buyer_account_data = data_of(&fixture.svm, buyer_account);
    assert_eq!(u64_at(&buyer_account_data, TOKEN_AMOUNT), 0);
}

#[test]
fn the_token_program_writes_the_extensions_a_non_transferable_mint_requires() {
    let fixture = setup();
    let account = data_of(&fixture.svm, fixture.institution_kyc_account);

    // `InitializeAccount3` sized and initialized the account itself: the base
    // account, the account-type byte, and both extensions the mint requires.
    assert_eq!(account.len(), KYC_ACCOUNT_LEN);
    assert_eq!(account[TOKEN_ACCOUNT_BASE_LEN], ACCOUNT_TYPE_ACCOUNT);
    let types = extension_types(&account, TOKEN_ACCOUNT_BASE_LEN + 1);
    assert!(
        types.contains(&EXT_IMMUTABLE_OWNER) && types.contains(&EXT_NON_TRANSFERABLE_ACCOUNT),
        "a non-transferable mint's accounts need both, got {types:?}"
    );
    assert_eq!(
        address_at(&account, TOKEN_MINT),
        kyc_mint_pda(),
        "and it belongs to the mint"
    );
}

#[test]
fn a_mint_into_an_account_without_the_required_extensions_is_refused() {
    let mut fixture = setup();

    // A bare Token-2022 account - no ImmutableOwner, no NonTransferableAccount.
    // Token-2022 refuses to mint a non-transferable token into it, which is why
    // the account has to go through `InitializeAccount3` (or an ATA program that
    // does the same) rather than being created by hand.
    let plain_account = Keypair::new().pubkey();
    set_raw_account(
        &mut fixture.svm,
        plain_account,
        plain_token_account_data(kyc_mint_pda(), fixture.institution.pubkey(), 0),
        TOKEN_2022_ID,
    );

    let failure = send_error(
        &mut fixture.svm,
        &fixture.authority,
        &[&fixture.authority],
        &[Instructions::issue_kyc(
            fixture.authority.pubkey(),
            plain_account,
        )],
    );
    assert!(
        kyc_error_code(&failure).is_none(),
        "the token program refuses this, not kyc_gate: {failure}"
    );
    assert_eq!(
        u64_at(&data_of(&fixture.svm, plain_account), TOKEN_AMOUNT),
        0
    );
}
