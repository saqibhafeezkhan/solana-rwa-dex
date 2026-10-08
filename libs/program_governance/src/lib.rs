//! Shared one-time-initializer guard for the RWA DEX programs (L-7).
//!
//! `transfer_hook`, `kyc_gate` and `settlement` each restrict their one-time
//! privileged setup to the program's own upgrade authority. The check used to
//! be three byte-identical copies; this crate is the single copy they all call.
//!
//! The layout it parses is the loader's stable bincode `UpgradeableLoaderState`:
//! a 4-byte variant tag (`ProgramData` == 3), an 8-byte slot, then the
//! `Option<Pubkey>` upgrade authority (1-byte tag, 32-byte key) — 45 bytes in
//! all, which is the length the loader itself documents
//! (`size_of_programdata_metadata`).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;

/// The `ProgramData` PDA of `program_id` must carry `signer` as its upgrade
/// authority. Every failure collapses onto the caller's `UnauthorizedInitializer`
/// variant so the programs keep their own error enums at the boundary.
pub fn require_upgrade_authority(
    program_id: &Pubkey,
    program_data: &UncheckedAccount,
    signer: &Pubkey,
) -> Result<()> {
    let (expected, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    require_keys_eq!(program_data.key(), expected, GovernanceError::Denied);
    require_keys_eq!(
        *program_data.owner,
        bpf_loader_upgradeable::id(),
        GovernanceError::Denied
    );

    let data = program_data.try_borrow_data()?;
    require!(data.len() >= 45, GovernanceError::Denied);
    require!(
        u32::from_le_bytes(data[0..4].try_into().unwrap()) == 3,
        GovernanceError::Denied
    );
    // `None` (tag 0) means the program can no longer be upgraded; a keyless
    // authority cannot be proven, so initialization stays closed.
    require!(data[12] == 1, GovernanceError::Denied);
    let authority = Pubkey::new_from_array(data[13..45].try_into().unwrap());
    require_keys_eq!(authority, *signer, GovernanceError::Denied);
    Ok(())
}

#[error_code]
pub enum GovernanceError {
    #[msg("Only the program's upgrade authority may perform this action")]
    Denied,
}
