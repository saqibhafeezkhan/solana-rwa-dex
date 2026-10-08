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
PAYER="${ANCHOR_WALLET:-$HOME/.config/solana/id.json}"
DO_BUILD=1
DO_RESET=0
AIRDROP_SOL=500

usage() {
  sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Flags:
  --reset                 Start the validator with a fresh ledger (ignored when
                          a validator is already listening on the RPC port).
  --no-build              Skip `anchor build` and deploy target/deploy as-is.
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

[ -f "$PAYER" ] || die "payer keypair not found: $PAYER (create one with: solana-keygen new -o $PAYER)"

# -----------------------------------------------------------------------
# 1. Validator
# -----------------------------------------------------------------------

validator_is_up() {
  solana cluster-version --url "$RPC_URL" >/dev/null 2>&1
}

wait_for_rpc() {
  local waited=0
  until validator_is_up; do
    [ "$waited" -lt 60 ] || die "validator did not come up on $RPC_URL - see $VALIDATOR_LOG"
    sleep 1
    waited=$((waited + 1))
  done
}

start_validator() {
  mkdir -p .anchor

  # The two programs a Token-2022 pool needs and the local cluster does not ship:
  # pull them from the fixture dumps Anchor.toml already names, if they are
  # present. They are not committed, so their absence is a warning rather than a
  # failure - it only matters to the Token-2022 e2e suites.
  # `address:path` pairs, matching the two [[test.validator.account]] entries in
  # Anchor.toml. Plain strings rather than an associative array, so the script
  # still runs under the bash 3.2 that macOS ships.
  local -a fixture_args=()
  local spec address file
  for spec in \
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb:tests/fixtures/token_2022_program.json" \
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL:tests/fixtures/associated_token_program.json"
  do
    address="${spec%%:*}"
    file="${spec#*:}"
    if [ -f "$file" ]; then
      fixture_args+=(--account "$address" "$file")
    else
      warn "fixture missing, skipping: $file (see [[test.validator.account]] in Anchor.toml)"
    fi
  done

  log "Starting solana-test-validator on port $RPC_PORT (ledger: $LEDGER_DIR)"
  # `${arr[@]+...}` rather than a bare `"${arr[@]}"`: an empty array expands to
  # an unbound variable under `set -u` on older bash (macOS ships 3.2).
  solana-test-validator \
    --reset \
    --ledger "$LEDGER_DIR" \
    --rpc-port "$RPC_PORT" \
    --faucet-port "$FAUCET_PORT" \
    ${fixture_args[@]+"${fixture_args[@]}"} \
    >"$VALIDATOR_LOG" 2>&1 &

  echo $! > .anchor/test-validator.pid
  wait_for_rpc
  log "Validator up (pid $(cat .anchor/test-validator.pid), log $VALIDATOR_LOG)"
}

if validator_is_up; then
  if [ "$DO_RESET" -eq 1 ]; then
    warn "--reset ignored: a validator is already listening on $RPC_URL."
    warn "stop it first (kill \$(cat .anchor/test-validator.pid)) to get a fresh ledger."
  fi
  log "Reusing the validator already listening on $RPC_URL"
else
  start_validator
fi

# -----------------------------------------------------------------------
# 2. Build
# -----------------------------------------------------------------------

if [ "$DO_BUILD" -eq 1 ]; then
  build_programs
else
  log "Skipping build (--no-build)"
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

  log "Deploying $name -> $id"
  solana program deploy \
    --url "$RPC_URL" \
    --keypair "$PAYER" \
    --upgrade-authority "$PAYER" \
    --program-id "target/deploy/${name}-keypair.json" \
    "target/deploy/${name}.so"
}

deployed=()
while IFS=$'\t' read -r name id; do
  [ -n "$name" ] || continue
  verify_program_keypair "$name" "$id" >/dev/null
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
EOF
