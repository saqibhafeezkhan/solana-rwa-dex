//! End-to-end tests for the `rwa_mint` program's fee-administration path.
//!
//! `create_rwa_mint` needs the hook/metadata extensions and is covered by the
//! TypeScript lifecycle; this suite pins `update_transfer_fee`, which had no
//! executed test in any layer. It builds a real Token-2022 mint carrying a
//! `TransferFeeConfig` and checks the extension bytes after the CPI.

mod common;

use common::*;

const PROGRAM_ID: Address = address!("5GbeJh4sySASsdH2zRdFX85LuXnu5y3rBkqhVi76Lzho");
const TOKEN_2022_ID: Address = address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

const IX_UPDATE_TRANSFER_FEE: [u8; 8] = [135, 106, 57, 77, 93, 247, 210, 158];

const IX_TRANSFER_FEE_EXTENSION: u8 = 26;
const TRANSFER_FEE_INITIALIZE_CONFIG: u8 = 0;
const IX_INITIALIZE_MINT_2: u8 = 20;

/// Base account (165) + account type (1) + TLV header (4) + config value (108).
const FEE_MINT_LEN: usize = 165 + 1 + 4 + 108;
const TLV_OFFSET: usize = 166;
const EXTENSION_TRANSFER_FEE_CONFIG: u16 = 1;

/// Inside the config value: two optional authorities (32 each), the withheld
/// amount (8), the older fee (18), then the newer fee
/// (`epoch: u64`, `maximum_fee: u64`, `transfer_fee_basis_points: u16`).
const CONFIG_NEWER_FEE: usize = 32 + 32 + 8 + 18;
const NEWER_FEE_MAXIMUM: usize = CONFIG_NEWER_FEE + 8;
const NEWER_FEE_BPS: usize = CONFIG_NEWER_FEE + 16;

const OLD_BPS: u16 = 25;
const OLD_MAX: u64 = 1_000;
const NEW_BPS: u16 = 150;
const NEW_MAX: u64 = 5_000_000;

fn initialize_transfer_fee_config(mint: Address, authority: Address) -> Instruction {
    let mut data = vec![IX_TRANSFER_FEE_EXTENSION, TRANSFER_FEE_INITIALIZE_CONFIG];
    for _ in 0..2 {
        data.push(1); // COption::Some
        data.extend_from_slice(authority.as_ref());
    }
    data.extend_from_slice(&OLD_BPS.to_le_bytes());
    data.extend_from_slice(&OLD_MAX.to_le_bytes());
    Instruction {
        program_id: TOKEN_2022_ID,
        accounts: vec![
            AccountMeta::new(mint, false),
            AccountMeta::new_readonly(authority, true),
        ],
        data,
    }
}

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

/// Walk the mint's TLV entries to the `TransferFeeConfig` value.
fn transfer_fee_config_offset(data: &[u8]) -> usize {
    let mut at = TLV_OFFSET;
    while at + 4 <= data.len() {
        let kind = u16::from_le_bytes(data[at..at + 2].try_into().unwrap());
        let len = u16::from_le_bytes(data[at + 2..at + 4].try_into().unwrap()) as usize;
        let value = at + 4;
        if value + len > data.len() {
            break;
        }
        if kind == EXTENSION_TRANSFER_FEE_CONFIG {
            return value;
        }
        at = value + len;
    }
    panic!("the fixture mint carries no TransferFeeConfig");
}

#[test]
fn update_transfer_fee_changes_the_mint_extension() {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "rwa_mint");
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), LAMPORTS).unwrap();

    let mint = Keypair::new().pubkey();
    set_raw_account(&mut svm, mint, vec![0u8; FEE_MINT_LEN], TOKEN_2022_ID);
    send(
        &mut svm,
        &authority,
        &[
            initialize_transfer_fee_config(mint, authority.pubkey()),
            initialize_mint_2(mint, authority.pubkey()),
        ],
    );

    let data = data_of(&svm, mint);
    let config = transfer_fee_config_offset(&data);
    assert_eq!(u16_at(&data, config + NEWER_FEE_BPS), OLD_BPS);

    let mut data = IX_UPDATE_TRANSFER_FEE.to_vec();
    data.extend_from_slice(&NEW_BPS.to_le_bytes());
    data.extend_from_slice(&NEW_MAX.to_le_bytes());
    let update = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(mint, false),
            AccountMeta::new(authority.pubkey(), true),
            AccountMeta::new_readonly(TOKEN_2022_ID, false),
        ],
        data,
    };
    send(&mut svm, &authority, &[update]);

    let data = data_of(&svm, mint);
    let config = transfer_fee_config_offset(&data);
    assert_eq!(u16_at(&data, config + NEWER_FEE_BPS), NEW_BPS);
    assert_eq!(u64_at(&data, config + NEWER_FEE_MAXIMUM), NEW_MAX);
}
