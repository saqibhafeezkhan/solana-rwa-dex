# `program_governance`

One shared check behind every "only the upgrade authority may do this once"
initializer in the protocol. It exists because the same ~40 lines of loader
parsing had been copied into three programs; a single copy means one place to
review and one place to fix.

```toml
program_governance = { path = "../../libs/program_governance" }
```

| Caller | Gated instruction | Why it is gated |
| :--- | :--- | :--- |
| `transfer_hook` | `initialize` | Sets the whitelist root/authority; front-running it would hand an attacker control of the compliance gate |
| `kyc_gate` | `initialize_kyc_mint` | Creates the soulbound attestation mint and its authority |
| `settlement` | `initialize` | Creates the clearinghouse config and the Transfer Agent slot |
| `order_book` | `initialize_book` | Book PDAs are deterministic per mint pair, so a front-run would own that market's pause key |

`order_book` calls the shared function directly; the other three keep a thin
local wrapper with the same name so their `Accounts` structs read consistently
(`require_upgrade_authority(&ctx.accounts.program_data, &authority.key())?`).

---

## API

```rust
pub fn require_upgrade_authority(
    program_id: &Pubkey,
    program_data: &UncheckedAccount,
    signer: &Pubkey,
) -> Result<()>;
```

Returns `GovernanceError::Denied` on every failure. Callers map it to their own
error variant (`UnauthorizedInitializer`) so the programs' public error surfaces
stay unchanged and clients keep their error tables.

`program_data` is an `UncheckedAccount` because its owner is the upgradeable
loader, not the calling program — Anchor cannot derive it as a typed account, and
the IDL does not declare it as a PDA, so clients pass it explicitly. The function
therefore does all the validation itself.

---

## What it checks, in order

| # | Check | Failure it prevents |
| :--- | :--- | :--- |
| 1 | `program_data.key() == find_program_address([program_id], bpf_loader_upgradeable)` | A caller supplying some other program's `ProgramData` (or any look-alike account) to borrow its authority |
| 2 | `*program_data.owner == bpf_loader_upgradeable::id()` | A plain system-owned account whose first bytes happen to parse |
| 3 | `data.len() >= 45` | Truncated buffer / out-of-bounds read |
| 4 | `u32::from_le_bytes(data[0..4]) == 3` | A different `UpgradeableLoaderState` variant (`Uninitialized`, `Buffer`, `Program`) |
| 5 | `data[12] == 1` | `authority: None` — a program that can no longer be upgraded has no key that can prove authority, so initialization stays closed |
| 6 | `Pubkey::new_from_array(data[13..45]) == *signer` | Anyone who is not the upgrade authority |

## The layout it parses

The upgradeable loader's bincode `UpgradeableLoaderState` for a deployed program:

| Offset | Size | Field |
| :--- | :--- | :--- |
| 0 | 4 | Variant tag (`ProgramData` = 3), little-endian |
| 4 | 8 | `slot` at which the program was last deployed |
| 12 | 1 | `Option` tag for the upgrade authority (`0` = none, `1` = some) |
| 13 | 32 | The upgrade-authority pubkey, present only when the tag is `1` |
| 45 | — | Program-specific data (`ProgramData` header ends here) |

45 bytes is exactly the loader's documented `size_of_programdata_metadata`, which
is why the length check doubles as the bound for the slice at `13..45`. The bytes
are read with `try_borrow_data()` and dropped before returning; no reference into
account data outlives the call.

---

## Security notes

* **The upgrade authority is the highest privilege in the system.** It can
  replace the program bytecode, so every other on-chain permission in that
  program is downstream of it. In production the upgrade authority should be a
  multisig vault (or burned after a final audit), not a deployer hot key.
* **One-time means one-time.** These initializers create PDA accounts with
  `init`, so a second call fails on the existing account regardless of who
  signs. The authority check is what decides *who* gets the one shot.
* **Rotating the authority rotates who can initialize.** `solana program
  set-upgrade-authority` changes the answer immediately; nothing is cached in
  program state.
* **Keyless programs cannot initialize.** If the upgrade authority is set to
  `None`, checks 5/6 fail and the initializer is permanently closed. That is the
  intended reading of "immutable": if no key can prove authority, no key gets to
  configure the program either.
* **It is not a runtime access-control layer.** Each instruction still declares
  its own `Signer` accounts; this helper only answers "is this signer the
  deployer". An instruction that forgets to call it is not protected by anything
  here.
* **It does not pin the deployed bytecode.** Nothing verifies that the program at
  `program_id` is built from this repository. Build verification
  (`solana-verify`) is the tool for that; `program_governance` only answers who
  may configure.

---

## Testing

There are no unit tests in this crate: the interesting behaviour is the loader
layout, which only exists on a real cluster. It is exercised through the
programs' own suites:

```bash
cargo test --workspace                            # callers' unit tests
cargo test --manifest-path tests/e2e/Cargo.toml   # e2e initializer guards
```

The e2e harness writes a `ProgramData` account with `set_upgrade_authority`,
then the `kyc_gate` suite asserts that a non-authority signer is rejected with
`UnauthorizedInitializer` (code 6006) both before and after the authority is set,
and that the correct signer succeeds. `order_book` and the AMM suites use the
same helper to make their one-time initializers callable.
