# scripts

Operator scripts for building and deploying the eleven programs. The three
runnable scripts are plain bash with `set -euo pipefail`; `programs.sh` is
sourced and deliberately sets no shell options. They resolve the repository root
from their own location (so they work from any cwd) and **never write to your
global `solana config`** — the cluster is passed as `--url` on every command.

| Script | Purpose | Typical use |
| :--- | :--- | :--- |
| `programs.sh` | Shared manifest + helpers. Sourced, not executed. | — |
| `deploy-localnet.sh` | Start/reuse a `solana-test-validator`, build, deploy all programs, verify | First-time setup, e2e runs |
| `deploy-devnet.sh` | Build, confirm, deploy/upgrade on devnet, print next steps | Staging deploys |
| `start-local-validator.sh` | Minimal reset validator on the default ports | When you only want a chain |

---

## `programs.sh` — the single source of deploy truth

`program_table()` prints one `name<TAB>program-id` row per program, ordered
leaf-first so it reads as the dependency graph:

```
oracle → kyc_gate → transfer_hook → rwa_mint → settlement → hooks
       → order_book → clmm → dlmm → maverick → twamm
```

Deploy order carries no on-chain meaning (programs call each other by id, not by
deploy sequence), but the order documents which primitives the AMMs wire up to.

The table is authoritative in the sense that **three** things must agree for a
deploy to be correct:

1. the row in `program_table()`;
2. the crate's `declare_id!` in `programs/<name>/src/lib.rs`;
3. the address baked into `target/deploy/<name>-keypair.json`.

`verify_program_keypair` checks (3) against (1) and aborts with a fix-up hint
before anything is sent. A mismatched keypair would otherwise deploy the program
to an address its own `declare_id!` does not know, breaking every PDA-derived
client while looking like a success. Note that (2) is only checked indirectly:
`anchor build` regenerates the keypairs from the crates, which is why running a
build (rather than `--no-build`) is what keeps the three in step.

Helpers exported to the deploy scripts:

| Function | Behaviour |
| :--- | :--- |
| `log` / `warn` / `die` | Coloured progress; `die` exits 1 with `error:` on stderr |
| `require_cmd <cmd> [hint]` | Aborts if the binary is missing, with an install hint |
| `repo_root "$BASH_SOURCE"` | Prints the repository root for a script path; callers `cd` to it. It must be given the *calling* script's path, not `programs.sh`'s |
| `verify_program_keypair <name> <id>` | Keypair/image existence + address agreement |
| `build_programs` | `anchor build --arch "${SBF_ARCH:-v0}"` |

**`--arch v0` is load-bearing.** Anchor 1.2 builds SBPF **v3** by default, and
the Agave 3.1.x toolchain pinned in `Anchor.toml` cannot parse a v3 image —
`solana program deploy` fails client-side with *"Failed to parse ELF file:
invalid file header"* before anything reaches the cluster. `v0` is also what
`cargo-build-sbf` defaults to and what the local validator accepts. Override
with `SBF_ARCH=v1 bash scripts/...` only if a newer loader requires it; the e2e
suites read whatever images are in `target/deploy`, so the arch used for tests
must be one the local validator accepts.

---

## `deploy-localnet.sh`

```bash
./scripts/deploy-localnet.sh                       # start (or reuse) validator, build, deploy
./scripts/deploy-localnet.sh --reset               # fresh ledger (only if no validator is up)
./scripts/deploy-localnet.sh --no-build            # deploy existing target/deploy images
./scripts/deploy-localnet.sh --payer ~/.config/solana/deployer.json
./scripts/deploy-localnet.sh --rpc-port 8900 --faucet-port 9901 --airdrop 1000
```

| Flag | Default | Notes |
| :--- | :--- | :--- |
| `--reset` | off | Ignored (with a warning) when a validator is already listening; stop it first |
| `--no-build` | build | Skips `anchor build`; the keypair check still runs |
| `--payer <path>` | `$ANCHOR_WALLET`, else `~/.config/solana/id.json` | Fee payer **and** upgrade authority |
| `--airdrop <sol>` | `500` | Failure only warns; the script continues on the existing balance |
| `--rpc-port <port>` | `8899` | Also sets the RPC URL |
| `--faucet-port <port>` | `9900` | |

Files it owns under `.anchor/`:

| Path | Contents |
| :--- | :--- |
| `.anchor/test-ledger` | Validator ledger (safe to delete when the validator is stopped) |
| `.anchor/test-validator.log` | Validator stdout/stderr |
| `.anchor/test-validator.pid` | PID of the validator it started |

Sequence: reuse-or-start the validator → wait up to 60 s for RPC → build (unless
`--no-build`) → airdrop → verify every keypair → `solana program deploy` each
row → `solana program show` each id and fail if any is missing.

Two operational details:

* **The Token-2022 fixtures are optional.** `Anchor.toml` declares
  `[[test.validator.account]]` entries that clone the Token-2022 and Associated
  Token programs from `tests/fixtures/*.json`. Those dumps are not committed, so
  a missing file is a warning, not a failure — but Token-2022 e2e suites and the
  TypeScript integration suite cannot run without them.
* **The payer becomes the upgrade authority.** That is deliberate: the one-time
  initializers in `transfer_hook`, `kyc_gate` and `settlement` read the upgrade
  authority out of the program's own `ProgramData` account, so whichever wallet
  deploys is the only wallet allowed to initialize. Rotating the payer later
  means a `solana program set-upgrade-authority` step.

At the end it prints the `ANCHOR_PROVIDER_URL` export and the e2e command:

```bash
export ANCHOR_PROVIDER_URL=http://127.0.0.1:8899
cargo test --manifest-path tests/e2e/Cargo.toml
```

---

## `deploy-devnet.sh`

```bash
./scripts/deploy-devnet.sh                              # build, confirm, deploy everything
./scripts/deploy-devnet.sh --yes                        # non-interactive (CI)
./scripts/deploy-devnet.sh --only oracle,kyc_gate       # subset, in table order
./scripts/deploy-devnet.sh --no-build --payer ~/.config/solana/deployer.json
./scripts/deploy-devnet.sh --url https://api.devnet.solana.com
```

| Flag | Default | Notes |
| :--- | :--- | :--- |
| `--yes` / `-y` | prompt | Required on a non-interactive shell; the script refuses to guess |
| `--only a,b` | all | Unknown names abort; the subset keeps table order |
| `--no-build` | build | Deploy existing images |
| `--payer <path>` | `$ANCHOR_WALLET`, else `~/.config/solana/id.json` | Fee payer and upgrade authority |
| `--url <rpc>` | `$SOLANA_RPC_URL`, else `https://api.devnet.solana.com` | `*mainnet*` is refused outright |

It prints the cluster, payer address and balance before asking for confirmation,
and warns (never blocks) when the balance is under `MIN_BALANCE_SOL = 15` — a
floor for eleven deploys plus buffer rent and the escrow/vault rent the wiring
instructions pay later.

Because `solana program deploy` on an existing program id is an **upgrade**, the
script is idempotent for code changes: re-running it upgrades in place and keeps
the same addresses. It does not touch program *state*, so a re-deploy never
re-runs initializers — a config change still needs its own instruction.

The closing message lists the privileged initializers that must still be run
once, signed by the upgrade authority:

1. `kyc_gate::initialize_kyc_mint` — soulbound attestation mint;
2. `transfer_hook::initialize` — whitelist config; plus
   `initialize_extra_account_meta_list` once per RWA mint;
3. `settlement::initialize` — clearinghouse config; plus
   `set_transfer_agent` to install the Transfer Agent key;
4. `order_book::initialize_book` — one per `base`/`quote` pair. It is gated the
   same way because the book PDA is deterministic: a permissionless initializer
   could be front-run, and the winner would hold that market's pause key. The
   four rails it fixes (`kyc_config`, `hooks_authority`, `settlement_slots`) are
   arguments, so a deploy does not pre-create any book.

---

## `start-local-validator.sh`

The minimal variant: `--reset`, ledger `.anchor/test-ledger`, RPC `8899`, faucet
`9900`. It does **not** build, deploy, fund a payer, or add the fixture accounts.
Use it when you want a bare chain (for example with `solana-test-validator`
already running in another terminal) and `deploy-localnet.sh --no-build` to add
the programs.

---

## Environment variables

| Variable | Read by | Effect |
| :--- | :--- | :--- |
| `ANCHOR_WALLET` | both deploy scripts | Default payer/upgrade-authority keypair path |
| `SOLANA_RPC_URL` | `deploy-devnet.sh` | Default cluster when `--url` is absent |
| `SBF_ARCH` | `programs.sh` (`build_programs`) | SBPF target arch; default `v0` |
| `ANCHOR_PROVIDER_URL` | clients, not the scripts | Set after a deploy so Anchor clients target that cluster |

## Requirements

`solana` (Agave CLI), `anchor` (via avm), a funded keypair, and `awk`/`sed`
available. The scripts are macOS-bash-3.2-safe on purpose: no associative
arrays, and `${arr[@]+...}` guards around possibly-empty arrays under `set -u`.
