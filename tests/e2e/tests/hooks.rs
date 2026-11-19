//! End-to-end tests for the `hooks` program's own instruction surface.
//!
//! The routed `after_swap` path is exercised by `order_book.rs`; this suite
//! covers the operator-facing `fund_hook_authority` instruction, which tops up
//! the PDA that pays rent for routed settlement vaults.

mod common;

use common::*;

const PROGRAM_ID: Address = address!("B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

/// `sha256("global:fund_hook_authority")[..8]`.
const IX_FUND_HOOK_AUTHORITY: [u8; 8] = [123, 229, 110, 241, 87, 21, 50, 116];

#[test]
fn fund_hook_authority_moves_lamports_into_the_pda() {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "hooks");
    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), LAMPORTS).unwrap();

    let hook_authority = hook_authority_pda();
    assert!(!exists(&svm, hook_authority), "the PDA starts empty");

    let amount = 12_345_678u64;
    let mut data = IX_FUND_HOOK_AUTHORITY.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    let fund = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(hook_authority, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    let before = svm.get_account(&payer.pubkey()).unwrap().lamports;
    send(&mut svm, &payer, &[fund]);

    assert_eq!(svm.get_account(&hook_authority).unwrap().lamports, amount);
    assert!(svm.get_account(&payer.pubkey()).unwrap().lamports < before);
}
