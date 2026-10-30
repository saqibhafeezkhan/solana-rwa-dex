#!/usr/bin/env bash
#
# Deploys every program and then seeds a local validator with the tokens, rails
# and pools the web UI reads — one command from a clean checkout to a clickable
# dashboard.
#
#   ./scripts/bootstrap-localnet.sh                   # build, deploy, seed
#   ./scripts/bootstrap-localnet.sh --reset            # fresh ledger first
#   ./scripts/bootstrap-localnet.sh --no-build         # redeploy the built images
#   ./scripts/bootstrap-localnet.sh --user <pubkey>    # fund + KYC a browser wallet
#   ./scripts/bootstrap-localnet.sh --rails oracle,kyc # wire the KYC rail too
#   ./scripts/bootstrap-localnet.sh --skip-deploy      # only re-seed
#
# The deploy half is `scripts/deploy-localnet.sh` (validator, build, upgrade
# authority, program verification). The seed half is `scripts/seed-localnet.ts`
# in the web app, which builds every instruction with the client's own builders,
# so a green run also proves the browser's encoding matches the programs.
#
# The payer is the upgrade authority, and the one-time initializers (kyc_gate,
# transfer_hook, settlement) check exactly that, so keep the two on the same
# keypair — they already default to ~/.config/solana/id.json.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
WEB_DIR="$REPO_ROOT/app/web"

# shellcheck source=scripts/programs.sh
source "$REPO_ROOT/scripts/programs.sh"

usage() {
  sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Flags:
  Deploy half (passed to scripts/deploy-localnet.sh):
    --reset                 Start the validator with a fresh ledger
    --no-build              Skip `anchor build`
    --payer <path>          Fee payer and upgrade authority
    --rpc-port <port>       Validator RPC port (default: 8899)

  Seed half (passed to app/web/scripts/seed-localnet.ts):
    --user <pubkey>         Browser wallet to fund, mint to and KYC-approve
    --rails <a,b>           Rails wired into every pool: oracle, kyc, settlement
                            (default: oracle)
    --no-verify             Skip the post-seed decodes and swap checks

  Both halves:
    --skip-deploy           Do not deploy; just re-seed the running validator
    -h, --help              This message
EOF
}

require_cmd bun "install from https://bun.sh"
require_cmd solana "install with: sh -c \"\$(curl -sSfL https://release.anza.xyz/stable/install)\""

DO_DEPLOY=1
RPC_PORT=8899
declare -a DEPLOY_ARGS=()
declare -a SEED_ARGS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-deploy) DO_DEPLOY=0 ;;
    --reset|--no-build)
      DEPLOY_ARGS+=("$1")
      ;;
    --rpc-port)
      RPC_PORT="${2:?--rpc-port needs a value}"
      DEPLOY_ARGS+=("$1" "$RPC_PORT")
      shift
      ;;
    --payer|--airdrop)
      DEPLOY_ARGS+=("$1" "${2:?$1 needs a value}")
      shift
      ;;
    --user|--rails|--url|--cluster)
      SEED_ARGS+=("$1" "${2:?$1 needs a value}")
      shift
      ;;
    --no-verify)
      SEED_ARGS+=("$1")
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown flag: $1 (try --help)"
      ;;
  esac
  shift
done

if [ ! -d "$WEB_DIR/node_modules" ]; then
  log "Installing web dependencies (bun install)"
  (cd "$WEB_DIR" && bun install)
fi

if [ "$DO_DEPLOY" -eq 1 ]; then
  log "Deploying the programs (scripts/deploy-localnet.sh)"
  # `${arr[@]+...}` keeps an empty array safe under `set -u` on bash 3.2.
  bash "$REPO_ROOT/scripts/deploy-localnet.sh" ${DEPLOY_ARGS[@]+"${DEPLOY_ARGS[@]}"}
else
  log "Skipping the program deploy (--skip-deploy)"
fi

# The seeder talks to the same validator the deploy script started.
RPC_URL="http://127.0.0.1:${RPC_PORT}"
if ! curl -fsS -X POST -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"getVersion"}' "$RPC_URL" >/dev/null 2>&1; then
  die "no validator answering on $RPC_URL — run without --skip-deploy, or start one with
       bash $REPO_ROOT/scripts/deploy-localnet.sh"
fi

log "Seeding tokens, rails and pools (bun run seed:localnet)"
(cd "$WEB_DIR" && bun run seed:localnet ${SEED_ARGS[@]+-- "${SEED_ARGS[@]}"})

cat <<EOF

Next:
  1. Start the UI:      cd app/web && bun run dev
  2. Connect the wallet printed above (import ~/.config/solana/id.json into
     Phantom/Solflare, or re-run with --user <your browser wallet>).
  3. Network: localnet (http://127.0.0.1:8899).
  4. Paste the two printed mints into the CLMM, DLMM, Maverick and TWAMM pages.
EOF
