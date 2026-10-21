# Tests

Three independent test layers sit at three different levels of the stack. They
answer different questions and are run by different commands; none of them
replaces another.

| Layer | Location | Runtime | What it proves | Count |
| :--- | :--- | :--- | :--- | :--- |
| Rust e2e | `tests/e2e` | LiteSVM against the **built SBF images** | Instruction guards, PDA derivation, accounting and error paths of the programs | 63 tests, 6 files |
| TypeScript integration | `tests/ts` | `mocha` against a **live `solana-test-validator`** | The real client-side flow: Token-2022, the Merkle whitelist, secp256k1 Transfer Agent signatures, escrow release/refund | 11 lifecycle cases + 1 smoke |
| Web unit tests | `app/web/tests` | `bun test` | Off-chain decoding, instruction encoding and math used by the UI | 7 tests |

The Rust programs also carry inline `#[cfg(test)]` modules for pure helpers
(oracle TWAP math, `math_core`, and the order-book pricing helpers). Those run
under `cargo test --workspace` (209 tests at the time of writing) and need no
validator.

---

## 1. Rust e2e — `tests/e2e`

```bash
anchor build                       # writes target/deploy/*.so
cargo test --manifest-path tests/e2e/Cargo.toml
cargo test --manifest-path tests/e2e/Cargo.toml --test order_book
```

**It is deliberately a separate Cargo workspace.** The tests execute a compiled
program inside a Solana VM, so they need the Solana crate stack the VM is built
from — versions that cannot share a Cargo graph with the programs' own
`solana-program 5.0`, because Cargo refuses to keep two semver-compatible
versions of the same crate. `[workspace]` in `tests/e2e/Cargo.toml` cuts the
tests off from the root workspace, and `.cargo/config.toml` redirects their
build output to `../../target/e2e` so the repository keeps one target directory.
Nothing here is linked into or deployed with the on-chain programs.

**The images are what is tested, not the crates.** Each suite loads `.so` files
from `target/deploy` into LiteSVM via `svm.add_program(...)`. Editing a program
without re-running `anchor build` therefore tests the *old* bytecode — the most
common way to get a confusing green run.

### Layout

```
tests/e2e/
├── Cargo.toml                  # workspace of its own; LiteSVM + solana-* dev-deps
├── .cargo/config.toml          # target-dir = ../../target/e2e
└── tests/
    ├── common/mod.rs           # shared harness: deploy, fund, send, derive, assert
    ├── clmm.rs                 # 14 tests
    ├── oracle.rs               # 14 tests
    ├── order_book.rs           # 10 tests
    ├── kyc_gate.rs             #  9 tests
    ├── maverick.rs             #  9 tests
    └── dlmm.rs                 #  7 tests
```

`common/mod.rs` is the only shared code between suites. It owns the parts that
are easy to get subtly wrong: loading a program image under its declared id
(`load_program(svm, id, "order_book")`, which reads
`target/deploy/<name>.so`), a funded payer, transactions whose blockhash is
refreshed per send so a retry is not rejected as `AlreadyProcessed`, clock warps
for T+N and expiry windows, and `send_error` / `send_error_logs`, which return
the program error code (Anchor constraint errors live in 2000..3000, the
programs' own in 6000..6100) instead of a vague failure. A test that hand-rolls
those tends to pass for the wrong reason.

Conventions every suite follows:

* **SPL layouts are written by hand, not by the token program.** The harness
  writes mint and token-account bytes directly (`create_mint`,
  `create_token_account`, `set_raw_account`) so a suite works whichever SPL
  implementation the VM ships, and so a test can construct the *invalid* account
  a guard is supposed to reject. The trade-off is that these tests do not
  exercise the token program's own checks — that is what the TypeScript layer is for.
* **Ids are declared per suite and must match `declare_id!`.** `common` exports
  the dependency ids (oracle, kyc_gate, hooks, settlement); each suite also
  declares the program under test. A program-id rotation therefore needs a
  deliberate edit here rather than silently deriving PDAs at stale addresses.
* **Instruction data is raw.** Suites hard-code the 8-byte Anchor discriminators
  (comment says where they come from) and build instructions with
  `Instruction { .. }` instead of generated clients, which keeps the harness
  independent of `target/idl` regeneration but means an instruction-signature
  change is a visible edit rather than a compile error.
* **Errors are matched by code.** `send_error(..) == ERR_ORDER_EXPIRED` (or the
  equivalent constant) keeps a test from passing on an unrelated failure such as
  rent, a missing signer or `AccountAlreadyInUse`.
* **State is read back and asserted.** A successful transaction is not the
  assertion; the test reads the account afterwards (`data_of`, `u64_at`,
  `token_balance`) and checks the fields the instruction claims to have written.
* **Time is warped, not slept.** T+N and expiry tests move the VM clock forward
  (`set_time`) rather than waiting.

### Coverage matrix

| File | Program | Covers (from the test names) |
| :--- | :--- | :--- |
| `clmm.rs` | `clmm` | Bitmap sized to tick spacing, deposit ownership + tick marking, Token-2022 and fee-bearing swaps, fee raised after funding, spacing/walk rejection, token-account ownership refusal, LP fees and withdrawal, KYC-gated pool, settlement-wired pool (escrow instead of payout, custody account pinned to the hook authority's ATA), pool-signed oracle creation, a wired pool tripping its own breaker |
| `oracle.rs` | `oracle` | Genesis state + PDA seeds, accumulator = price × seconds, time-weighted price changes, pool-only recording, no backward timestamps, TWAP window, guard reverts a deviating price, `trip_breaker` latches in a successful transaction, inside-threshold refusal, zero threshold rejected, pool signature required at init and at the guard, thin-history skip, ring-buffer wrap |
| `order_book.rs` | `order_book` (+ `hooks`, `settlement`) | Ask fills partially then completely, bid pays base and receives quote, cancel refunds the unfilled escrow and closes, donations to escrow do not block cancel, expired order cannot fill but can cancel, size/slippage guards, another mint's escrow rejected, pause blocks orders/fills but not cancels, caller-named trade ids, and the wired rail: a routed fill escrows through `hooks::after_swap` and releases with a real secp256k1 agent signature |
| `kyc_gate.rs` | `kyc_gate` | Soulbound mint owned by the config PDA, upgrade-authority-only initialization, authority-only issue/revoke, rotation locks out the old key, attestation is binary and reversible, `verify_kyc` approves only the holder's own account, non-transferability, required Token-2022 extensions (and a mint into a bare account refused) |
| `maverick.rs` | `maverick` | Vault + fully priced bin-array init, static-bin deposit/withdraw at equal value, directional liquidity needs a static baseline, swap crossing empty bins, static bins never move and moves follow the price, `move_bins` carries liquidity and its LP withdraws along the chain, a moved bin cannot move again or take deposits, authority-only pause that never traps liquidity, oracle-gated swaps |
| `dlmm.rs` | `dlmm` | Vault + bin-array init, bin deposit/withdraw at equal value, swap fills the active bin and crosses into the next funded one, an unfillable swap is refused rather than partially filled, authority-only pause that never traps liquidity, permissionless bin-array opening near the active bin, oracle-gated swaps |

Not covered by a dedicated Rust suite today: `twamm`, `rwa_mint` and
`transfer_hook`. `hooks` and `settlement` have no suite of their own either, but
their composed path is exercised by `order_book.rs`, which loads both images and
drives a routed fill through escrow and release. `transfer_hook` and
`settlement`'s direct API are also covered end-to-end in the TypeScript layer,
where their interesting paths are Token-2022 and signature-shaped.

### Adding a suite

1. Add `tests/e2e/tests/<program>.rs` with `mod common;` and a `PROGRAM_ID`
   constant that matches the crate's `declare_id!`.
2. `load_program(&mut svm, PROGRAM_ID, "<name>")` — the helper resolves
   `target/deploy/<name>.so`, and panics with "build the programs first" if it
   is missing.
3. Write the happy path first, then one test per guard, each asserting the exact
   error code and that no state changed.
4. Wire it into the matrix above and re-run the whole suite, not just the new file.

---

## 2. TypeScript integration — `tests/ts`

```bash
# terminal 1
./scripts/deploy-localnet.sh          # validator + all 11 programs
# terminal 2
cd tests && bun install && bun run test
# or, from app/web:
bun run test:anchor
```

These tests talk to a **live cluster** through `@coral-xyz/anchor`, so they cover
what the LiteSVM suites do not: real Token-2022 transfer-hook resolution against
the deployed programs, the whitelist tree built by the console's own Merkle
helper, and the exact account list a client must assemble.

`rwa-dex.ts` walks one continuous institutional lifecycle in order, sharing
state between cases:

1. initialize the transfer hook and the RWA mint;
2. mint to Institution A;
3. register Merkle whitelist entries for both institutions;
4. transfer between two whitelisted institutions;
5. refuse a transfer to a wallet with no entry;
6. refuse an entry whose proof is not in the tree;
7. invalidate stale entries when the root is rotated;
8. lock tokens in the T+N escrow through the hook;
9. refuse to settle before T+N;
10. advance the clock and release with a real Transfer Agent signature;
11. refund a cancelled escrow to the account that funded it.

`test_anchor_litesvm.ts` is a one-case connectivity smoke test (`getVersion`).
It is the canary for "the provider is pointed at nothing".

Notes:

* The suite imports Merkle helpers from `app/web/src/core/merkle.ts` on purpose:
  the whitelist tree the tests build has to be the same one the console builds,
  or the two drift and only the on-chain hook notices.
* `secp256k1` arrives transitively with `@solana/web3.js`; the release path is
  signed for real rather than stubbed, so the exact keccak256 preimage is under
  test.
* The `ProgramData` account is derived manually (`findProgramAddressSync` on the
  upgradeable loader) because the IDL does not declare it as a PDA — the
  initializers in `transfer_hook`, `kyc_gate` and `settlement` read the upgrade
  authority from it.
* Cases share state, so a failure early in the sequence cascades. When one
  fails, read the first failure, not the last.

---

## 3. Web unit tests — `app/web/tests`

```bash
cd app/web && bun test
```

`decoders.test.ts` covers the decoders the console relies on: account
discriminators, `Book`/`Order` layouts, instruction builders and the amount/price
rounding the UI displays. These run in-process with no chain, so they are the
fastest signal when touching `app/web/src/core`.

---

## 4. Known stale artifact

`tests/test_initialize.rs` is a leftover from the original `anchor init`
scaffold: it imports a `solana_dex` crate (Counter program) that no longer exists
and is **not** a member of any workspace, so no command compiles or runs it. It
is safe to delete; it is listed here so a reader does not mistake it for a live
suite or try to "fix" it.

---

## 5. Full verification sequence

```bash
anchor build                                            # SBF images
cargo test --workspace                                  # unit tests, all crates
cargo test --manifest-path tests/e2e/Cargo.toml         # 63 e2e tests
cd app/web && bun test && bunx tsc -b && bunx oxlint .  # web unit + types + lint
./scripts/deploy-localnet.sh && (cd tests && bun run test)   # 11 TS cases
```

Each layer fails independently. A green `cargo test --workspace` says nothing
about the SBF images (they are built separately), and a green e2e run says
nothing about the client's account ordering — which is why the TypeScript layer
exists.
