//! End-to-end tests: the compiled `settlement` program on a LiteSVM.
//!
//! The other suites exercise the settlement rail through `hooks` / `order_book`
//! (a routed trade). These drive the program directly on the standalone
//! (authority-custody) flow, with a mint that carries a real Token-2022
//! transfer fee — the case `close_settlement` had no path out of before T-1:
//! the escrow withholds the fee its incoming transfer paid, and the token
//! program refuses to close an account that still holds one.
//!
//! ```bash
//! anchor build -p settlement && (cd tests/e2e && cargo test --test settlement)
//! ```

mod common;

use common::*;
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};

/// The program under test; must match `declare_id!`.
const PROGRAM_ID: Address = address!("58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6");
const TOKEN_2022_ID: Address = address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const TOKEN_PROGRAM_ID: Address = address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// Anchor instruction discriminators, from `target/idl/settlement.json`.
const IX_INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
const IX_INITIATE_SETTLEMENT: [u8; 8] = [21, 206, 83, 9, 54, 135, 177, 194];
const IX_CANCEL_SETTLEMENT: [u8; 8] = [33, 241, 96, 62, 228, 178, 1, 120];
const IX_CLOSE_SETTLEMENT: [u8; 8] = [45, 247, 83, 183, 24, 102, 0, 68];
const IX_SETTLE: [u8; 8] = [175, 42, 185, 87, 144, 131, 102, 212];
const IX_SET_TRANSFER_AGENT: [u8; 8] = [35, 191, 232, 217, 214, 140, 220, 143];
const IX_SET_MINT_AGENT: [u8; 8] = [160, 32, 15, 23, 222, 69, 167, 104];
const IX_CLEAR_MINT_AGENT: [u8; 8] = [234, 6, 234, 124, 166, 51, 125, 251];

/// Token-2022 instruction tags: `InitializeMint2`, `InitializeAccount3`,
/// `MintTo`, the transfer-fee extension prefix and its initialize tag.
const IX_INITIALIZE_MINT_2: u8 = 20;
const IX_INITIALIZE_ACCOUNT_3: u8 = 18;
const IX_MINT_TO: u8 = 7;
const IX_TRANSFER_FEE_EXTENSION: u8 = 26;
const TRANSFER_FEE_INITIALIZE_CONFIG: u8 = 0;

/// `TransferFeeConfig` (108 bytes: two optional authorities, the withheld
/// amount, and the older/newer fee) and `TransferFeeAmount` (8 bytes). A mint
/// or account carrying an extension must be allocated at the token program's
/// own computed length before its `Initialize*` instruction can write it.
const FEE_MINT_LEN: usize = 165 + 1 + 4 + 108;
const FEE_ACCOUNT_LEN: usize = 165 + 1 + 4 + 8;
/// The base account state ends at 165; byte 165 is the account type and the TLV
/// entries start at 166.
const TLV_OFFSET: usize = 166;
const EXTENSION_TRANSFER_FEE_CONFIG: u16 = 1;
const EXTENSION_TRANSFER_FEE_AMOUNT: u16 = 2;
/// Inside a `TransferFeeConfig` value: two 32-byte authorities, then the amount
/// the mint has collected from the accounts that received its tokens.
const CONFIG_WITHHELD_OFFSET: usize = 64;

/// `SettlementVault` layout: trade id, taker, mint, amount, settlement slot,
/// expiry slot, status, pool, refund source, bump.
const VAULT_AMOUNT: usize = 80;
const VAULT_STATUS: usize = 104;
const STATUS_CANCELLED: u8 = 2;

/// The settlement program's own errors this suite asserts on, in `ErrorCode`'s
/// order.
const ERR_UNAUTHORIZED: u32 = 6012;
const ERR_UNAUTHORIZED_TRANSFER_AGENT: u32 = 6005;
const ERR_INVALID_TRANSFER_AGENT_KEY: u32 = 6006;

/// `MintAgentKey` layout: discriminator, the 64-byte uncompressed secp256k1 key,
/// the bump.
const MINT_AGENT_KEY_LEN: usize = 8 + 64 + 1;

/// The transfer fee the fixture puts on its mint: 1%, capped high enough that
/// no test transfer reaches the cap.
const FEE_BASIS_POINTS: u16 = 100;
const FEE_MAXIMUM: u64 = 1_000_000_000_000;

/// What the desk's custody account is minted, the amount it escrows, and what
/// the fee arithmetic leaves behind.
const FUNDS: u64 = 1_000_000_000;
const TRADE_AMOUNT: u64 = 1_000;
/// 1% of the 1 000 escrowed: the escrow withholds 10 and records 990.
const INITIATION_FEE: u64 = 10;
const ESCROWED: u64 = TRADE_AMOUNT - INITIATION_FEE;
/// 1% of the 990 refunded, withheld in the desk's account this time. Token-2022
/// rounds the fee up, so 9.9 becomes 10.
const REFUND_FEE: u64 = 10;

fn vault_pda(trade_id: u64) -> Address {
    Address::find_program_address(&[b"settlement", &trade_id.to_le_bytes()], &PROGRAM_ID).0
}

fn escrow_pda(trade_id: u64) -> Address {
    Address::find_program_address(&[b"escrow", &trade_id.to_le_bytes()], &PROGRAM_ID).0
}

fn escrow_authority_pda() -> Address {
    Address::find_program_address(&[b"escrow_authority"], &PROGRAM_ID).0
}

fn config_pda() -> Address {
    Address::find_program_address(&[b"config"], &PROGRAM_ID).0
}

fn mint_agent_pda(mint: Address) -> Address {
    Address::find_program_address(&[b"agent", mint.as_ref()], &PROGRAM_ID).0
}

/// The canonical associated token account under a chosen token program.
fn ata(owner: Address, mint: Address, token_program: Address) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM_ID,
    )
    .0
}

// -----------------------------------------------------------------------
// Token-2022 fixture
// -----------------------------------------------------------------------

fn blank_token_2022_account(svm: &mut LiteSVM, account: Address, len: usize) {
    set_raw_account(svm, account, vec![0u8; len], TOKEN_2022_ID);
}

/// Token-2022's `InitializeTransferFeeConfig`: both authorities are the payer
/// and the same fee is set for this and the next epoch, so the effective rate
/// does not depend on the clock.
fn initialize_transfer_fee_config(mint: Address, authority: Address) -> Instruction {
    let mut data = vec![IX_TRANSFER_FEE_EXTENSION, TRANSFER_FEE_INITIALIZE_CONFIG];
    for _ in 0..2 {
        data.push(1); // COption::Some
        data.extend_from_slice(authority.as_ref());
    }
    data.extend_from_slice(&FEE_BASIS_POINTS.to_le_bytes());
    data.extend_from_slice(&FEE_MAXIMUM.to_le_bytes());
    Instruction {
        program_id: TOKEN_2022_ID,
        accounts: vec![
            AccountMeta::new(mint, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

/// Token-2022's `InitializeMint2`: decimals 6, `authority` mints, no freeze
/// authority. The account must already be allocated and owned by the token
/// program.
fn initialize_mint_2(mint: Address, authority: Address) -> Instruction {
    let mut data = vec![IX_INITIALIZE_MINT_2];
    data.push(6);
    data.extend_from_slice(authority.as_ref());
    data.push(0);
    Instruction {
        program_id: TOKEN_2022_ID,
        accounts: vec![AccountMeta::new(mint, false)],
        data,
    }
}

/// Token-2022's `InitializeAccount3`, which is what writes the
/// `TransferFeeAmount` extension the mint requires on every account.
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

fn mint_to(mint: Address, destination: Address, authority: Address, amount: u64) -> Instruction {
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

/// Walks a Token-2022 account's TLV entries for `extension` and returns the
/// offset of its value, or `None` when the account does not carry it.
fn extension_offset(data: &[u8], extension: u16) -> Option<usize> {
    let mut at = TLV_OFFSET;
    while at + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[at], data[at + 1]]);
        let len = u16::from_le_bytes([data[at + 2], data[at + 3]]) as usize;
        let value = at + 4;
        if value + len > data.len() || (kind == 0 && len == 0) {
            return None;
        }
        if kind == extension {
            return Some(value);
        }
        at = value + len;
    }
    None
}

/// The fee a mint has collected from its accounts (`withheld_amount`).
fn withheld_on_mint(svm: &LiteSVM, mint: Address) -> u64 {
    let data = data_of(svm, mint);
    let at = extension_offset(&data, EXTENSION_TRANSFER_FEE_CONFIG)
        .expect("the fixture's mint carries TransferFeeConfig");
    u64_at(&data, at + CONFIG_WITHHELD_OFFSET)
}

/// The fee an account is holding for the mint (`withheld_amount`).
fn withheld_on_account(svm: &LiteSVM, account: Address) -> u64 {
    let data = data_of(svm, account);
    let at = extension_offset(&data, EXTENSION_TRANSFER_FEE_AMOUNT)
        .expect("a fee-bearing mint's accounts carry TransferFeeAmount");
    u64_at(&data, at)
}

// -----------------------------------------------------------------------
// Settlement fixture
// -----------------------------------------------------------------------

struct World {
    svm: LiteSVM,
    payer: Keypair,
    mint: Address,
    /// The desk's own custody account, which funds the escrow.
    custody: Address,
    /// The config authority's canonical ATA for the mint, where a residual
    /// balance would be swept.
    residual: Address,
    token_program: Address,
}

/// A world holding a real 1% Token-2022 fee mint.
fn fee_world() -> World {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "settlement");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    set_upgrade_authority(&mut svm, PROGRAM_ID, payer.pubkey());

    initialize_config(&mut svm, &payer);

    // A real 1% fee mint, built by the token program's own instructions.
    let authority = payer.pubkey();
    let mint = Keypair::new().pubkey();
    let custody = Keypair::new().pubkey();
    let residual = ata(authority, mint, TOKEN_2022_ID);
    blank_token_2022_account(&mut svm, mint, FEE_MINT_LEN);
    blank_token_2022_account(&mut svm, custody, FEE_ACCOUNT_LEN);
    blank_token_2022_account(&mut svm, residual, FEE_ACCOUNT_LEN);
    send(
        &mut svm,
        &payer,
        &[
            initialize_transfer_fee_config(mint, authority),
            initialize_mint_2(mint, authority),
            initialize_account_3(custody, mint, authority),
            initialize_account_3(residual, mint, authority),
            mint_to(mint, custody, authority, FUNDS),
        ],
    );

    World {
        svm,
        payer,
        mint,
        custody,
        residual,
        token_program: TOKEN_2022_ID,
    }
}

/// The same fixture over a legacy mint, which has no extensions: the harvest is
/// skipped and the close path must stay byte-for-byte what it was.
fn plain_world() -> World {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "settlement");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();
    set_upgrade_authority(&mut svm, PROGRAM_ID, payer.pubkey());
    initialize_config(&mut svm, &payer);

    let mint = Keypair::new().pubkey();
    create_mint(&mut svm, payer.pubkey(), mint, 6);
    let custody = Keypair::new().pubkey();
    create_token_account(&mut svm, custody, mint, payer.pubkey(), 1_000);
    let residual = associated_token_address(payer.pubkey(), mint);
    create_token_account(&mut svm, residual, mint, payer.pubkey(), 0);

    World {
        svm,
        payer,
        mint,
        custody,
        residual,
        token_program: TOKEN_PROGRAM_ID,
    }
}

/// Initializes the clearinghouse in the standalone flow: wallet initiations are
/// allowed (`pools_require_gate` false), which is the desk-custody path a
/// settlement can be cancelled on directly.
fn initialize_config(svm: &mut LiteSVM, payer: &Keypair) {
    let mut data = IX_INITIALIZE.to_vec();
    data.push(0);
    let initialize = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(config_pda(), false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(program_data_pda(PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    send(svm, payer, &[initialize]);
}

fn initiate_settlement(w: &World, trade_id: u64, settlement_slot: u64, amount: u64) -> Instruction {
    let mut data = IX_INITIATE_SETTLEMENT.to_vec();
    data.extend_from_slice(&trade_id.to_le_bytes());
    data.extend_from_slice(&settlement_slot.to_le_bytes());
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(vault_pda(trade_id), false),
            AccountMeta::new(w.custody, false),
            AccountMeta::new(escrow_pda(trade_id), false),
            AccountMeta::new_readonly(w.mint, false),
            AccountMeta::new_readonly(w.payer.pubkey(), false), // taker
            AccountMeta::new(w.payer.pubkey(), true),           // authority
            AccountMeta::new_readonly(PROGRAM_ID, false),       // pool: None
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new_readonly(w.token_program, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

fn cancel_settlement(w: &World, trade_id: u64) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(vault_pda(trade_id), false),
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new_readonly(PROGRAM_ID, false), // pool: None
            AccountMeta::new_readonly(w.mint, false),
            AccountMeta::new(escrow_pda(trade_id), false),
            AccountMeta::new_readonly(escrow_authority_pda(), false),
            AccountMeta::new(w.custody, false), // == vault.refund_source
            AccountMeta::new_readonly(w.payer.pubkey(), true),
            AccountMeta::new_readonly(w.token_program, false),
        ],
        data: IX_CANCEL_SETTLEMENT.to_vec(),
    }
}

fn close_settlement(w: &World, trade_id: u64) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(vault_pda(trade_id), false),
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new(w.payer.pubkey(), true),
            AccountMeta::new(w.mint, false), // writable: the harvest credits it
            AccountMeta::new(escrow_pda(trade_id), false),
            AccountMeta::new_readonly(escrow_authority_pda(), false),
            AccountMeta::new_readonly(w.token_program, false),
            AccountMeta::new(w.residual, false),
        ],
        data: IX_CLOSE_SETTLEMENT.to_vec(),
    }
}

fn set_transfer_agent(w: &World, agent: [u8; 64]) -> Instruction {
    let mut data = IX_SET_TRANSFER_AGENT.to_vec();
    data.extend_from_slice(&agent);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(config_pda(), false), // the key is stored in it
            AccountMeta::new_readonly(w.payer.pubkey(), true),
        ],
        data,
    }
}

fn set_mint_agent(w: &World, authority: Address, agent: [u8; 64]) -> Instruction {
    let mut data = IX_SET_MINT_AGENT.to_vec();
    data.extend_from_slice(&agent);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new(authority, true),
            AccountMeta::new_readonly(w.mint, false),
            AccountMeta::new(mint_agent_pda(w.mint), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    }
}

fn clear_mint_agent(w: &World) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new(w.payer.pubkey(), true),
            AccountMeta::new_readonly(w.mint, false),
            AccountMeta::new(mint_agent_pda(w.mint), false),
        ],
        data: IX_CLEAR_MINT_AGENT.to_vec(),
    }
}

/// The release path. `agent` is the per-mint override account when the mint has
/// one; the program ID is the `Option::None` sentinel, which makes `settle` use
/// the config's global key.
fn settle(
    w: &World,
    trade_id: u64,
    signature: [u8; 64],
    recovery_id: u8,
    agent: Option<Address>,
) -> Instruction {
    let mut data = IX_SETTLE.to_vec();
    data.extend_from_slice(&signature);
    data.push(recovery_id);
    Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(vault_pda(trade_id), false),
            AccountMeta::new_readonly(config_pda(), false),
            match agent {
                Some(agent) => AccountMeta::new_readonly(agent, false),
                None => AccountMeta::new_readonly(PROGRAM_ID, false),
            },
            AccountMeta::new_readonly(w.mint, false),
            AccountMeta::new(escrow_pda(trade_id), false),
            AccountMeta::new_readonly(escrow_authority_pda(), false),
            AccountMeta::new(w.residual, false), // the taker's account
            AccountMeta::new_readonly(w.token_program, false),
        ],
        data,
    }
}

/// A deterministic secp256k1 key and its uncompressed 64-byte form (`x || y`,
/// no `0x04` prefix), which is what the program stores.
fn agent_key(seed: u8) -> (SigningKey, [u8; 64]) {
    let signing = SigningKey::from_slice(&[seed; 32]).expect("a valid scalar");
    let encoded = signing.verifying_key().to_encoded_point(false);
    let mut key = [0u8; 64];
    key.copy_from_slice(&encoded.as_bytes()[1..]);
    (signing, key)
}

/// Signs the release preimage the Transfer Agent signs:
/// `keccak256(trade_id || taker || mint || amount)`.
fn release_signature(
    signing: &SigningKey,
    trade_id: u64,
    taker: Address,
    mint: Address,
    amount: u64,
) -> ([u8; 64], u8) {
    let mut message = Vec::new();
    message.extend_from_slice(&trade_id.to_le_bytes());
    message.extend_from_slice(taker.as_ref());
    message.extend_from_slice(mint.as_ref());
    message.extend_from_slice(&amount.to_le_bytes());
    let digest = Keccak256::digest(&message);
    let (signature, recovery_id) = signing
        .sign_prehash_recoverable(digest.as_slice())
        .expect("signing works");
    let mut bytes = [0u8; 64];
    bytes.copy_from_slice(&signature.to_bytes());
    (bytes, recovery_id.to_byte())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

/// T-1: a transfer fee is withheld in the account that *received* the transfer,
/// and the token program refuses to close an account that still holds one. The
/// escrow received the trade's amount, so on a fee-bearing mint it holds that
/// transfer's fee forever — the balance sweep cannot move it — and
/// `close_settlement` used to fail after the trade was already terminal. It now
/// harvests the withheld fee to the mint first, where the issuer's own withdraw
/// instruction collects it, and the rent comes back.
#[test]
fn close_settlement_harvests_a_fee_bearing_escrows_withheld_fee() {
    let mut w = fee_world();
    let trade_id = 7;
    let settlement_slot = w.svm.get_sysvar::<Clock>().slot + 100;

    let initiate = initiate_settlement(&w, trade_id, settlement_slot, TRADE_AMOUNT);
    send(&mut w.svm, &w.payer, &[initiate]);

    // The fee is charged on the way in: the vault records what the escrow
    // actually received, and the fee itself sits in the escrow's withheld
    // amount, where the program's accounting never looks.
    let vault = data_of(&w.svm, vault_pda(trade_id));
    assert_eq!(u64_at(&vault, VAULT_AMOUNT), ESCROWED, "net of the fee");
    assert_eq!(token_balance(&w.svm, escrow_pda(trade_id)), ESCROWED);
    assert_eq!(
        withheld_on_account(&w.svm, escrow_pda(trade_id)),
        INITIATION_FEE,
        "the escrow withheld the incoming transfer's fee"
    );
    assert_eq!(token_balance(&w.svm, w.custody), FUNDS - TRADE_AMOUNT);

    // Refund it to the desk. The whole spendable balance moves; the withheld
    // fee does not, and the refund transfer withholds its own fee in the desk's
    // account.
    let cancel = cancel_settlement(&w, trade_id);
    send(&mut w.svm, &w.payer, &[cancel]);
    assert_eq!(token_balance(&w.svm, escrow_pda(trade_id)), 0);
    assert_eq!(
        withheld_on_account(&w.svm, escrow_pda(trade_id)),
        INITIATION_FEE,
        "a refund cannot move the withheld fee"
    );
    assert_eq!(
        token_balance(&w.svm, w.custody),
        FUNDS - TRADE_AMOUNT + ESCROWED - REFUND_FEE
    );
    assert_eq!(withheld_on_account(&w.svm, w.custody), REFUND_FEE);
    assert_eq!(
        data_of(&w.svm, vault_pda(trade_id))[VAULT_STATUS],
        STATUS_CANCELLED
    );

    // The close is where this used to abort: the escrow has a zero balance but
    // still holds the initiation fee, so the token program refused to close it.
    let close = close_settlement(&w, trade_id);
    send(&mut w.svm, &w.payer, &[close]);

    assert!(
        !exists(&w.svm, escrow_pda(trade_id)),
        "the escrow is closed"
    );
    assert!(!exists(&w.svm, vault_pda(trade_id)), "the vault is closed");
    assert_eq!(
        withheld_on_mint(&w.svm, w.mint),
        INITIATION_FEE,
        "the escrow's withheld fee was harvested to the mint"
    );
    assert_eq!(
        token_balance(&w.svm, w.residual),
        0,
        "nothing was left to sweep"
    );
}

/// The same fee-less code path must stay exactly as it was: a legacy mint has
/// no extensions at all, so the harvest is skipped and the close is unchanged.
#[test]
fn close_settlement_still_closes_a_plain_mint_escrow() {
    let mut w = plain_world();
    let trade_id = 1;
    let settlement_slot = w.svm.get_sysvar::<Clock>().slot + 100;

    let initiate = initiate_settlement(&w, trade_id, settlement_slot, 1_000);
    send(&mut w.svm, &w.payer, &[initiate]);
    assert_eq!(token_balance(&w.svm, escrow_pda(trade_id)), 1_000);

    let cancel = cancel_settlement(&w, trade_id);
    send(&mut w.svm, &w.payer, &[cancel]);
    let close = close_settlement(&w, trade_id);
    send(&mut w.svm, &w.payer, &[close]);

    assert!(!exists(&w.svm, escrow_pda(trade_id)));
    assert!(!exists(&w.svm, vault_pda(trade_id)));
    assert_eq!(token_balance(&w.svm, w.custody), 1_000, "refunded in full");
}

/// A per-mint override replaces the config's global Transfer Agent key for that
/// one mint. The account is optional, so this pins the three properties that
/// matter: it is the canonical `[b"agent", mint]` PDA, the key it records is
/// what `settle` verifies against, and the global key stops working for the
/// mint once it exists.
#[test]
fn a_mint_agent_override_is_what_settle_verifies_against() {
    let mut w = plain_world();
    let (global_signing, global_key) = agent_key(7);
    let (mint_signing, per_mint_key) = agent_key(9);

    let set_global = set_transfer_agent(&w, global_key);
    send(&mut w.svm, &w.payer, &[set_global]);

    // Only the config authority may install an override; a stranger with a
    // funded key cannot create the PDA.
    let stranger = Keypair::new();
    w.svm.airdrop(&stranger.pubkey(), LAMPORTS).unwrap();
    let denied = set_mint_agent(&w, stranger.pubkey(), per_mint_key);
    assert_eq!(
        send_error(&mut w.svm, &stranger, &[denied]),
        ERR_UNAUTHORIZED
    );
    assert!(
        !exists(&w.svm, mint_agent_pda(w.mint)),
        "nothing was created"
    );

    let set = set_mint_agent(&w, w.payer.pubkey(), per_mint_key);
    send(&mut w.svm, &w.payer, &[set]);

    let record = data_of(&w.svm, mint_agent_pda(w.mint));
    assert_eq!(record.len(), MINT_AGENT_KEY_LEN);
    assert_eq!(
        &record[8..72],
        &per_mint_key[..],
        "the key is stored as given"
    );

    // An empty key is refused rather than stored, and the good key survives.
    let zero = set_mint_agent(&w, w.payer.pubkey(), [0u8; 64]);
    assert_eq!(
        send_error(&mut w.svm, &w.payer, &[zero]),
        ERR_INVALID_TRANSFER_AGENT_KEY
    );
    assert_eq!(
        &data_of(&w.svm, mint_agent_pda(w.mint))[8..72],
        &per_mint_key[..]
    );

    let trade_id = 1;
    let settlement_slot = w.svm.get_sysvar::<Clock>().slot + 100;
    let initiate = initiate_settlement(&w, trade_id, settlement_slot, 1_000);
    send(&mut w.svm, &w.payer, &[initiate]);
    let mut clock = w.svm.get_sysvar::<Clock>();
    clock.slot = settlement_slot;
    w.svm.set_sysvar(&clock);

    // The config's global key no longer releases this mint's trade.
    let (signature, recovery_id) =
        release_signature(&global_signing, trade_id, w.payer.pubkey(), w.mint, 1_000);
    let global_release = settle(
        &w,
        trade_id,
        signature,
        recovery_id,
        Some(mint_agent_pda(w.mint)),
    );
    assert_eq!(
        send_error(&mut w.svm, &w.payer, &[global_release]),
        ERR_UNAUTHORIZED_TRANSFER_AGENT
    );
    assert_eq!(
        token_balance(&w.svm, escrow_pda(trade_id)),
        1_000,
        "nothing moved"
    );

    // The per-mint key does.
    let (signature, recovery_id) =
        release_signature(&mint_signing, trade_id, w.payer.pubkey(), w.mint, 1_000);
    let release = settle(
        &w,
        trade_id,
        signature,
        recovery_id,
        Some(mint_agent_pda(w.mint)),
    );
    send(&mut w.svm, &w.payer, &[release]);

    assert_eq!(
        token_balance(&w.svm, w.residual),
        1_000,
        "the taker is paid"
    );
    assert_eq!(token_balance(&w.svm, escrow_pda(trade_id)), 0);
}

/// Clearing the override closes its PDA (rent back to the config authority) and
/// reverts the mint to the config's global key, which is what a later `settle`
/// with no agent account is checked against.
#[test]
fn clear_mint_agent_restores_the_global_key_and_refunds_the_override_rent() {
    let mut w = plain_world();
    let (global_signing, global_key) = agent_key(7);
    let (_, per_mint_key) = agent_key(9);

    let set_global = set_transfer_agent(&w, global_key);
    send(&mut w.svm, &w.payer, &[set_global]);
    let set = set_mint_agent(&w, w.payer.pubkey(), per_mint_key);
    send(&mut w.svm, &w.payer, &[set]);

    let agent_account = mint_agent_pda(w.mint);
    let rent = w
        .svm
        .get_account(&agent_account)
        .expect("the override exists")
        .lamports;
    assert!(rent > 0);

    let before = w.svm.get_account(&w.payer.pubkey()).unwrap().lamports;
    let clear = clear_mint_agent(&w);
    send(&mut w.svm, &w.payer, &[clear]);
    let after = w.svm.get_account(&w.payer.pubkey()).unwrap().lamports;

    assert!(!exists(&w.svm, agent_account), "the override is closed");
    assert!(after > before, "its rent came back to the config authority");

    // Back on the global key: a trade released by it, with the override account
    // omitted (the program-ID sentinel), now settles.
    let trade_id = 1;
    let settlement_slot = w.svm.get_sysvar::<Clock>().slot + 100;
    let initiate = initiate_settlement(&w, trade_id, settlement_slot, 1_000);
    send(&mut w.svm, &w.payer, &[initiate]);
    let mut clock = w.svm.get_sysvar::<Clock>();
    clock.slot = settlement_slot;
    w.svm.set_sysvar(&clock);

    let (signature, recovery_id) =
        release_signature(&global_signing, trade_id, w.payer.pubkey(), w.mint, 1_000);
    let release = settle(&w, trade_id, signature, recovery_id, None);
    send(&mut w.svm, &w.payer, &[release]);

    assert_eq!(token_balance(&w.svm, w.residual), 1_000);
    assert_eq!(token_balance(&w.svm, escrow_pda(trade_id)), 0);
}
