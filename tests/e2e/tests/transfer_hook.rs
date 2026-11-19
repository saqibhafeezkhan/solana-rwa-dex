//! End-to-end tests for the `transfer_hook` admin surface.
//!
//! `tests/ts/rwa-dex.ts` covers the hook as Token-2022 actually invokes it
//! (dual proofs during a transfer). This suite covers the instructions an
//! operator calls around that: the upgrade-authority-gated init, publishing a
//! root, writing an institution's entry, and closing it again.

mod common;

use common::*;
use sha3::{Digest, Keccak256};

const PROGRAM_ID: Address = address!("fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");

const IX_INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
const IX_UPDATE_ROOT: [u8; 8] = [58, 195, 57, 246, 116, 198, 170, 138];
const IX_SET_WHITELIST_ENTRY: [u8; 8] = [178, 251, 88, 181, 179, 221, 17, 4];
const IX_CLOSE_WHITELIST_ENTRY: [u8; 8] = [71, 39, 46, 83, 231, 156, 154, 203];

/// `WhitelistEntry.owner` (discriminator, then the owner key).
const ENTRY_OWNER: usize = 8;

fn config_pda() -> Address {
    Address::find_program_address(&[b"config"], &PROGRAM_ID).0
}

fn entry_pda(owner: Address) -> Address {
    Address::find_program_address(&[b"whitelist", owner.as_ref()], &PROGRAM_ID).0
}

/// `keccak(0x00 || pubkey)`, the leaf the program folds.
fn leaf_hash(owner: Address) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update([0u8]);
    hasher.update(owner.as_ref());
    hasher.finalize().into()
}

#[test]
fn root_publication_entry_lifecycle_and_close() {
    let mut svm = LiteSVM::new();
    load_program(&mut svm, PROGRAM_ID, "transfer_hook");
    let authority = Keypair::new();
    svm.airdrop(&authority.pubkey(), LAMPORTS).unwrap();
    set_upgrade_authority(&mut svm, PROGRAM_ID, authority.pubkey());

    let init = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(config_pda(), false),
            AccountMeta::new(authority.pubkey(), true),
            AccountMeta::new_readonly(program_data_pda(PROGRAM_ID), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data: IX_INITIALIZE.to_vec(),
    };
    send(&mut svm, &authority, &[init]);
    assert!(exists(&svm, config_pda()));

    // A single-member tree: the root is the institution's leaf and its proof is
    // empty. `update_root` rejects the zero root, so this is the live tree.
    let institution = Keypair::new().pubkey();
    let root = leaf_hash(institution);
    let mut data = IX_UPDATE_ROOT.to_vec();
    data.extend_from_slice(&root);
    let update = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(config_pda(), false),
            AccountMeta::new_readonly(authority.pubkey(), true),
        ],
        data,
    };
    send(&mut svm, &authority, &[update]);

    let entry = entry_pda(institution);
    let mut data = IX_SET_WHITELIST_ENTRY.to_vec();
    data.extend_from_slice(&0u32.to_le_bytes()); // empty proof
    let set = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new(authority.pubkey(), true),
            AccountMeta::new_readonly(institution, false),
            AccountMeta::new(entry, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    send(&mut svm, &authority, &[set]);
    assert_eq!(address_at(&data_of(&svm, entry), ENTRY_OWNER), institution);

    // A stranger cannot delist somebody else's entry...
    let stranger = Keypair::new();
    svm.airdrop(&stranger.pubkey(), LAMPORTS).unwrap();
    let close = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new(stranger.pubkey(), true),
            AccountMeta::new_readonly(institution, false),
            AccountMeta::new(entry, false),
        ],
        data: IX_CLOSE_WHITELIST_ENTRY.to_vec(),
    };
    send_error(&mut svm, &stranger, &[close]);
    assert!(exists(&svm, entry), "a refused close leaves the entry");

    // ...while the authority closes it and gets its rent back.
    let before = svm.get_account(&authority.pubkey()).unwrap().lamports;
    let close = Instruction {
        program_id: PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(config_pda(), false),
            AccountMeta::new(authority.pubkey(), true),
            AccountMeta::new_readonly(institution, false),
            AccountMeta::new(entry, false),
        ],
        data: IX_CLOSE_WHITELIST_ENTRY.to_vec(),
    };
    send(&mut svm, &authority, &[close]);
    assert!(!exists(&svm, entry));
    assert!(svm.get_account(&authority.pubkey()).unwrap().lamports > before);
}
