#!/usr/bin/env bash
#
# Deploys every program in `scripts/programs.sh` to a local `solana-test-validator`.
#
#   ./scripts/deploy-localnet.sh                 # start (or reuse) a validator, build, deploy
#   ./scripts/deploy-localnet.sh --reset         # force a fresh ledger first
#   ./scripts/deploy-localnet.sh --no-build      # deploy the existing target/deploy images
#   ./scripts/deploy-localnet.sh --payer ~/.config/solana/deployer.json
#
# The payer is also the upgrade authority. That matters here: the one-time
# initializers in `transfer_hook`, `kyc_gate` and `settlement` read the upgrade
# authority out of the program's own ProgramData account, so the wallet that
# deploys them is the wallet allowed to initialize them.
#
# No program keypair is required, and none is tracked: the images are loaded into
# the validator's genesis block at the ids `programs.sh` declares, with the payer
# as upgrade authority. Creating a program at a fixed address with `solana
# program deploy` needs the keypair that owns that address; a genesis entry needs
# only the id. Each program is then upgraded in place by address.
#
# Nothing global is mutated: the cluster is passed as `--url` on every command
# rather than written into `solana config`, so your default cluster is untouched.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/programs.sh
source "$SCRIPT_DIR/programs.sh"

cd "$(repo_root "${BASH_SOURCE[0]}")"

RPC_PORT=8899
FAUCET_PORT=9900
RPC_URL="http://127.0.0.1:${RPC_PORT}"
LEDGER_DIR=".anchor/test-ledger"
VALIDATOR_LOG=".anchor/test-validator.log"
VALIDATOR_PID_FILE=".anchor/test-validator.pid"
PAYER="${ANCHOR_WALLET:-$HOME/.config/solana/id.json}"
DO_BUILD=1
DO_RESET=0
AIRDROP_SOL=500

usage() {
  usage_header "${BASH_SOURCE[0]}"
  cat <<'EOF'

Flags:
  --reset                 Start the validator with a fresh ledger (ignored when
                          a validator is already listening on the RPC port).
  --no-build              Skip the build and deploy target/deploy as-is.
  --payer <path>          Fee payer and upgrade authority (default: Anchor.toml's
                          `~/.config/solana/id.json`, or $ANCHOR_WALLET).
  --airdrop <sol>         Localnet airdrop amount for the payer (default: 500).
  --rpc-port <port>       Validator RPC port (default: 8899).
  --faucet-port <port>    Validator faucet port (default: 9900).
  -h, --help              This message.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --reset)       DO_RESET=1 ;;
    --no-build)    DO_BUILD=0 ;;
    --payer)       PAYER="${2:?--payer needs a path}"; shift ;;
    --airdrop)     AIRDROP_SOL="${2:?--airdrop needs an amount}"; shift ;;
    --rpc-port)    RPC_PORT="${2:?--rpc-port needs a port}"; RPC_URL="http://127.0.0.1:${RPC_PORT}"; shift ;;
    --faucet-port) FAUCET_PORT="${2:?--faucet-port needs a port}"; shift ;;
    -h|--help)     usage; exit 0 ;;
    *)             die "unknown flag: $1 (try --help)" ;;
  esac
  shift
done

require_cmd solana "install with: sh -c \"\$(curl -sSfL https://release.anza.xyz/stable/install)\""
require_cmd anchor "install with: cargo install --git https://github.com/coral-xyz/anchor avm --force && avm install latest"
verify_program_table

[ -f "$PAYER" ] || die "payer keypair not found: $PAYER (create one with: solana-keygen new -o $PAYER)"
PAYER_ADDRESS="$(solana address --keypair "$PAYER")" \
  || die "could not read the payer address from $PAYER"

# -----------------------------------------------------------------------
# 1. Build
# -----------------------------------------------------------------------

# The validator loads these images at genesis, so the build has to come first.
# Reusing an already-running validator is still fine: step 4 upgrades each
# program in place.
if [ "$DO_BUILD" -eq 1 ]; then
  build_programs
else
  log "Skipping build (--no-build)"
  # The images on disk are whatever ran last - possibly an `anchor test`
  # (SBPF v0). Check their arch here rather than discovering it at genesis load.
  require_deployable_images
fi

# -----------------------------------------------------------------------
# 2. Validator
# -----------------------------------------------------------------------

validator_is_up() {
  solana cluster-version --url "$RPC_URL" >/dev/null 2>&1
}

wait_for_rpc() {
  local pid="$1" waited=0
  until validator_is_up; do
    # A validator that died during startup (port taken, unreadable ledger, an
    # image it refused to load) used to surface only as a generic 60 s timeout.
    if ! kill -0 "$pid" 2>/dev/null; then
      if [ -s "$VALIDATOR_LOG" ]; then
        # `tr -d '\000'`: validator logs are padded with NULs when two processes
        # share the file, and a wall of NULs is the same as no message at all.
        printf '\n--- last lines of %s ---\n' "$VALIDATOR_LOG" >&2
        tail -c 4000 "$VALIDATOR_LOG" | tr -d '\000' | grep -av '^[[:space:]]*$' | tail -n 15 >&2
      fi
      die "solana-test-validator (pid $pid) exited during startup"
    fi
    [ "$waited" -lt 60 ] || die "validator still not answering on $RPC_URL after 60s - see $VALIDATOR_LOG"
    sleep 1
    waited=$((waited + 1))
  done
}

start_validator() {
  mkdir -p .anchor

  # Load every program at its declared id in the genesis block. Genesis needs
  # only an address, so this works from a clone with no program keypairs. Naming
  # the payer as upgrade authority keeps the ProgramData account the one-time
  # initializers in transfer_hook, kyc_gate and settlement read out of it, and
  # lets the deploy step below upgrade each image in place.
  local -a program_args=()
  local name id image
  while IFS=$'\t' read -r name id; do
    [ -n "$name" ] || continue
    image="target/deploy/${name}.so"
    [ -f "$image" ] || die "missing program image: $image (drop --no-build)"
    program_args+=(--upgradeable-program "$id" "$image" "$PAYER_ADDRESS")
  done < <(program_table)

  # Token-2022 and the Associated Token program are built into
  # solana-test-validator 3.1.10, so no fixture dumps or `--account` clones are
  # needed (Anchor.toml likewise declares no [[test.validator.account]] rows).
  log "Starting solana-test-validator on port $RPC_PORT (ledger: $LEDGER_DIR)"
  # `${arr[@]+...}` rather than a bare `"${arr[@]}"`: an empty array expands to
  # an unbound variable under `set -u` on older bash (macOS ships 3.2).
  solana-test-validator \
    --reset \
    --ledger "$LEDGER_DIR" \
    --rpc-port "$RPC_PORT" \
    --faucet-port "$FAUCET_PORT" \
    ${program_args[@]+"${program_args[@]}"} \
    >"$VALIDATOR_LOG" 2>&1 &

  local pid=$!
  printf '%s\n' "$pid" > "$VALIDATOR_PID_FILE"
  wait_for_rpc "$pid"
  log "Validator up (pid $pid, log $VALIDATOR_LOG)"
}

if validator_is_up; then
  # A validator this script did not start (a bare `start-local-validator.sh`, or
  # `anchor test`'s surfnet) has no project programs in its genesis block, and a
  # ledger started before the last build has stale images. Neither is visible
  # until a program is missing or wrong, so say which case we are in up front.
  running_pid=""
  if [ -f "$VALIDATOR_PID_FILE" ] && kill -0 "$(cat "$VALIDATOR_PID_FILE")" 2>/dev/null; then
    running_pid="$(cat "$VALIDATOR_PID_FILE")"
    log "Reusing the validator this script started (pid $running_pid) on $RPC_URL"
  else
    warn "a validator is already listening on $RPC_URL, but this script did not start it"
    warn "(no live pid in $VALIDATOR_PID_FILE). Its genesis block is unknown: if a"
    warn "program is missing below, stop that validator and re-run with --reset."
  fi
  if [ "$DO_RESET" -eq 1 ]; then
    warn "--reset ignored: a validator is already listening on $RPC_URL."
    warn "stop it first (kill ${running_pid:-that validator}) to get a fresh ledger."
  fi
else
  start_validator
fi

# -----------------------------------------------------------------------
# 3. Fund the payer
# -----------------------------------------------------------------------

log "Airdropping ${AIRDROP_SOL} SOL to the payer"
solana airdrop "$AIRDROP_SOL" --url "$RPC_URL" --keypair "$PAYER" >/dev/null \
  || warn "airdrop failed; continuing with the existing balance"

# -----------------------------------------------------------------------
# 4. Deploy
# -----------------------------------------------------------------------

deploy_one() {
  local name="$1" id="$2"
  local image="target/deploy/${name}.so" keypair="target/deploy/${name}-keypair.json"

  # A program in the genesis block (or left by a previous run) is upgraded by
  # address: no keypair involved, and the payer is the upgrade authority both
  # ways. The upgrade also replaces the genesis entry with a real deployment
  # slot, which is what `solana program show` and any slot-based tooling report.
  if solana program show "$id" --url "$RPC_URL" >/dev/null 2>&1; then
    log "Upgrading $name -> $id"
    solana program deploy \
      --url "$RPC_URL" \
      --keypair "$PAYER" \
      --upgrade-authority "$PAYER" \
      --program-id "$id" \
      "$image"
    return
  fi

  # Not on chain yet: at a fixed address that needs the keypair owning the
  # address. Use the one on disk when it matches; otherwise the only way to get
  # the program there is a genesis load, which needs a restart.
  if [ -f "$keypair" ] && [ "$(solana address --keypair "$keypair" 2>/dev/null)" = "$id" ]; then
    log "Deploying $name -> $id (from $keypair)"
    solana program deploy \
      --url "$RPC_URL" \
      --keypair "$PAYER" \
      --upgrade-authority "$PAYER" \
      --program-id "$keypair" \
      "$image"
    return
  fi

  die "$name is not deployed on $RPC_URL, and creating it at $id needs the keypair that owns that address
  fix: start from a fresh ledger so the images are loaded at genesis:
       kill \$(cat $VALIDATOR_PID_FILE) 2>/dev/null; ./scripts/deploy-localnet.sh --reset"
}

deployed=()
while IFS=$'\t' read -r name id; do
  [ -n "$name" ] || continue
  deploy_one "$name" "$id"
  deployed+=("$name:$id")
done < <(program_table)

# -----------------------------------------------------------------------
# 5. Verify
# -----------------------------------------------------------------------

log "Verifying ${#deployed[@]} programs on $RPC_URL"
failed=0
for entry in "${deployed[@]}"; do
  id="${entry#*:}"
  if solana program show "$id" --url "$RPC_URL" >/dev/null 2>&1; then
    printf '  ok   %s\n' "$entry"
  else
    printf '  FAIL %s\n' "$entry"
    failed=$((failed + 1))
  fi
done

[ "$failed" -eq 0 ] || die "$failed program(s) did not deploy"

cat <<EOF

All ${#deployed[@]} programs are live on localnet ($RPC_URL).

Point the client at it:
  export ANCHOR_PROVIDER_URL=$RPC_URL

Run the e2e suites against the deployed images:
  cargo test --manifest-path tests/e2e/Cargo.toml
  (cd tests && bun run test)
EOF
