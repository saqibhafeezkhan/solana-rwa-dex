# scripts

Operator scripts for building and deploying the eleven programs. The three
runnable scripts are plain bash with `set -euo pipefail`; `programs.sh` is
sourced and deliberately sets no shell options. They resolve the repository root
from their own location (so they work from any cwd) and **never write to your
global `solana config`** — the cluster is passed as `--url` on every command.

| Script | Purpose | Typical use |
| :--- | :--- | :--- |
| `programs.sh` | Shared manifest + helpers. Sourced, not executed. | — |
| `build-programs.sh` | Build the IDLs and the SBPF v1 images, no validator | Warm build, e2e prep |
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

`build-programs.sh` is the same build on its own, for the e2e suites or to warm
the cache before `deploy-localnet.sh --no-build`:

```bash
./scripts/build-programs.sh              # IDLs + SBPF v1 images
SBF_ARCH=v0 ./scripts/build-programs.sh  # only if the loader needs v0
```

It prints the arch of every image it produced, so a wrong-arch build is visible
before it is deployed.

Deploy order carries no on-chain meaning (programs call each other by id, not by
deploy sequence), but the order documents which primitives the AMMs wire up to.

The table is authoritative in the sense that **two** things must agree for a
deploy to be correct:

1. the row in `program_table()`;
2. the crate's `declare_id!` in `programs/<name>/src/lib.rs`.

On **devnet** a third value also has to agree: the address baked into
`target/deploy/<name>-keypair.json`. `verify_program_keypair` checks it against
the table and aborts with a fix-up hint before anything is sent — a mismatched
keypair would otherwise deploy the program to an address its own `declare_id!`
does not know, breaking every PDA-derived client while looking like a success.

On **localnet** no program keypair is involved at all. `deploy-localnet.sh`
loads the images into the validator's genesis block with
`--upgradeable-program <id> <so> <payer>`, which needs an address rather than
the keypair that owns it (program keypairs are not tracked in this repository),
naming the payer as upgrade authority so the `ProgramData` account exists for
the initializers that read it. It then upgrades each program in place by
address with `solana program deploy --program-id <id>`, which also replaces the
genesis entry with a real deployment slot.

Helpers exported to the deploy scripts:

| Function | Behaviour |
| :--- | :--- |
| `log` / `warn` / `die` | Coloured progress; `die` exits 1 with `error:` on stderr |
| `require_cmd <cmd> [hint]` | Aborts if the binary is missing, with an install hint |
| `repo_root "$BASH_SOURCE"` | Prints the repository root for a script path; callers `cd` to it. It must be given the *calling* script's path, not `programs.sh`'s |
| `usage_header <script>` | Prints a script's leading comment block for `--help`. Replaces the hard-coded `sed -n '2,23p'` ranges, which silently truncated `--help` as soon as a header changed |
| `verify_program_table` | Aborts on a duplicated name or id, or an empty table. A duplicate id is silent otherwise: the second deploy overwrites the first and `solana program show` still passes for both rows |
| `verify_program_keypair <name> <id>` | Keypair/image existence + address agreement (devnet). Explains that keypairs are untracked, and that `anchor keys sync` rewrites `declare_id!` when you mean to change an address |
| `build_programs` | `anchor build --ignore-keys`, then `cargo-build-sbf --arch "${SBF_ARCH:-v1}"` per program |
| `sbf_image_arch <image>` / `verify_sbf_arch <arch>` | Read the ELF `e_machine`; fail the build if any image is the wrong arch |
| `require_deployable_images` | `verify_sbf_arch "${SBF_ARCH:-v1}"` for the deploy scripts: validates the images already in `target/deploy`, which matters most under `--no-build` |

**Messages are double-quoted, so they contain no backticks.** A backtick there
runs the command while the message is built: an early draft of the keypair error
said `` `anchor keys sync` ``, and hitting it executed a real `keys sync` that
rewrote all eleven `declare_id!`s and the `[programs.localnet]` block before the
error printed. Quote commands with single quotes in those strings.

**The build is two passes for two different reasons.**

`anchor build` is what generates `target/idl/*.json`, which `anchor.workspace.*`
in the TypeScript tests reads. It refuses to build when
`target/deploy/<name>-keypair.json` does not match the crate's `declare_id!` —
which is the state of a fresh clone, because those keypairs are not tracked — so
`build_programs` passes `--ignore-keys`. The ids come from the crates either
way.

`anchor build` also hard-codes `--arch v0` when it invokes `cargo-build-sbf`,
and the Agave 3.1.10 toolchain pinned in `Anchor.toml` cannot use SBPF **v0**
images (`e_machine 0xF7`): `solana program deploy` fails client-side with
*"Failed to parse ELF file: invalid file header"*, and a validator that loaded
one at genesis answers *"Program is not deployed"* on every call. Forwarding
`-- --arch v1` through `anchor build` is rejected as a duplicate argument, so
the second pass rebuilds every program with `cargo-build-sbf --arch v1`
(`e_machine 0x107`). `verify_sbf_arch` then reads the header of all eleven
images and fails the build rather than the deploy if any is wrong. Override
with `SBF_ARCH=v0` only if a loader that accepts v0 is in use; the e2e suites
load whatever is in `target/deploy`.

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
| `--no-build` | build | Reuses existing `target/deploy/*.so`; they must be present |
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

Sequence: build (under `--no-build`, check the arch of the images already there)
→ reuse-or-start the validator with all eleven images in its genesis block →
wait up to 60 s for RPC → airdrop → `solana program deploy --program-id <id>`
each row (an in-place upgrade) → `solana program show` each id and fail if any is
missing.

It reports which validator it is reusing. A validator it started itself is named
by pid from `.anchor/test-validator.pid`; one it did **not** start (a bare
`start-local-validator.sh`, `anchor test`'s surfnet) gets a warning up front,
because its genesis block is unknown and a program may be missing or stale. If
the validator it starts dies during startup — port taken, ledger locked, an
image it refused to load — the script prints the last lines of
`.anchor/test-validator.log` and exits immediately instead of waiting out the
60 s RPC timeout.

Two operational details:

* **No external token-program fixtures are needed.** Token-2022 and the
  Associated Token program are built into `solana-test-validator` 3.1.10, so
  `Anchor.toml` declares no `[[test.validator.account]]` entries and there is no
  `tests/fixtures/` directory to populate.
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

`--only` is resolved before the build, so a typo does not cost a full compile.
Every selected keypair and image is then checked **before** the confirmation
prompt: the deploy loop used to verify each keypair as it reached it, so a
mismatch on the last program left the first ten upgraded on the shared cluster.
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
`9900`. It does **not** build, deploy, fund a payer, or load the project's
programs, and it writes no pid file.

A validator can only take programs at genesis, so `deploy-localnet.sh
--no-build` cannot add them to a chain this script started: the programs exist
in that validator's genesis block or they do not exist. Use
`deploy-localnet.sh` when you want the programs on chain, and this script only
when you want a chain and nothing on it. If you do point the deploy script at
one of these, it now says so up front instead of failing per program.

---

## Environment variables

| Variable | Read by | Effect |
| :--- | :--- | :--- |
| `ANCHOR_WALLET` | both deploy scripts | Default payer/upgrade-authority keypair path |
| `SOLANA_RPC_URL` | `deploy-devnet.sh` | Default cluster when `--url` is absent |
| `SBF_ARCH` | `programs.sh` (`build_programs`) | SBPF target arch; default `v1` |
| `ANCHOR_PROVIDER_URL` | clients, not the scripts | Set after a deploy so Anchor clients target that cluster |

## Requirements

`solana` (Agave CLI), `anchor` (via avm), a funded keypair, and `awk`/`sed`
available. The scripts are macOS-bash-3.2-safe on purpose: no associative
arrays, and `${arr[@]+...}` guards around possibly-empty arrays under `set -u`.
