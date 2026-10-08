#!/usr/bin/env bash
#
# Deploys every program in `scripts/programs.sh` to Solana **devnet**.
#
#   ./scripts/deploy-devnet.sh                  # build, confirm, deploy all ten
#   ./scripts/deploy-devnet.sh --yes            # non-interactive (CI)
#   ./scripts/deploy-devnet.sh --only oracle,kyc_gate
#   ./scripts/deploy-devnet.sh --no-build --payer ~/.config/solana/deployer.json
#
# This spends real (if worthless) SOL and writes to a shared public cluster, so
# it asks for confirmation unless `--yes` is passed. The payer becomes the
# upgrade authority for every program it deploys - which is what the one-time
# initializers in `transfer_hook`, `kyc_gate` and `settlement` check - so use the
# institutional multisig vault in production, not a personal key.
#
# The program addresses on devnet are the same as localnet: the ids are baked
# into each crate's `declare_id!` and into the program keypairs under
# target/deploy, so a client's PDAs do not change with the cluster. Every
# keypair is checked against its declared id before anything is sent.
#
# Nothing global is mutated: the cluster is passed as `--url` on every command
# rather than written into `solana config`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/programs.sh
source "$SCRIPT_DIR/programs.sh"

cd "$(repo_root "${BASH_SOURCE[0]}")"

RPC_URL="${SOLANA_RPC_URL:-https://api.devnet.solana.com}"
PAYER="${ANCHOR_WALLET:-$HOME/.config/solana/id.json}"
DO_BUILD=1
ASSUME_YES=0
ONLY=""
# Rough cost of ten program deploys plus buffer rent and the escrow/vault rent
# the wiring instructions pay later. Only a floor: the script warns, never blocks.
MIN_BALANCE_SOL=15

usage() {
  sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Flags:
  --yes                   Skip the confirmation prompt (required when not on a TTY).
  --only <a,b,...>        Deploy only these programs (comma-separated names).
  --no-build              Skip `anchor build` and deploy target/deploy as-is.
  --payer <path>          Fee payer and upgrade authority (default: Anchor.toml's
                          `~/.config/solana/id.json`, or $ANCHOR_WALLET).
  --url <rpc>             Cluster RPC endpoint (default: $SOLANA_RPC_URL, else
                          https://api.devnet.solana.com).
  -h, --help              This message.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --yes|-y)      ASSUME_YES=1 ;;
    --only)        ONLY="${2:?--only needs a comma-separated list}"; shift ;;
    --no-build)    DO_BUILD=0 ;;
    --payer)       PAYER="${2:?--payer needs a path}"; shift ;;
    --url)         RPC_URL="${2:?--url needs an endpoint}"; shift ;;
    -h|--help)     usage; exit 0 ;;
    *)             die "unknown flag: $1 (try --help)" ;;
  esac
  shift
done

require_cmd solana "install with: sh -c \"\$(curl -sSfL https://release.anza.xyz/stable/install)\""
require_cmd anchor "install with: cargo install --git https://github.com/coral-xyz/anchor avm --force && avm install latest"

[ -f "$PAYER" ] || die "payer keypair not found: $PAYER"
case "$RPC_URL" in
  *mainnet*) die "refusing to deploy to mainnet ($RPC_URL) - this script is for devnet" ;;
esac

# -----------------------------------------------------------------------
# 1. Which programs
# -----------------------------------------------------------------------

selected_table() {
  if [ -z "$ONLY" ]; then
    program_table
    return
  fi
  local -a wanted
  IFS=',' read -r -a wanted <<<"$ONLY"
  local name
  for name in "${wanted[@]}"; do
    [ -n "$name" ] || continue
    local row
    row="$(program_table | awk -F'\t' -v n="$name" '$1 == n')"
    [ -n "$row" ] || die "unknown program in --only: $name"
    printf '%s\n' "$row"
  done
}

# -----------------------------------------------------------------------
# 2. Build
# -----------------------------------------------------------------------

if [ "$DO_BUILD" -eq 1 ]; then
  build_programs
else
  log "Skipping build (--no-build)"
fi

# -----------------------------------------------------------------------
# 3. Preflight
# -----------------------------------------------------------------------

log "Cluster: $RPC_URL"
log "Payer / upgrade authority: $PAYER ($(solana address --keypair "$PAYER"))"

PAYER_ADDRESS="$(solana address --keypair "$PAYER")"
BALANCE_LAMPORTS="$(solana balance --url "$RPC_URL" --keypair "$PAYER" --lamports | awk '{print $1}')"
BALANCE_SOL="$(awk -v l="$BALANCE_LAMPORTS" 'BEGIN { printf "%.3f", l / 1000000000 }')"
log "Balance: ${BALANCE_SOL} SOL"

if awk -v b="$BALANCE_SOL" -v m="$MIN_BALANCE_SOL" 'BEGIN { exit !(b < m) }'; then
  warn "balance is under ${MIN_BALANCE_SOL} SOL; a deploy of ten programs usually needs more."
  warn "fund it with: solana airdrop 2 --url $RPC_URL --keypair $PAYER  (repeat as needed)"
fi

# -----------------------------------------------------------------------
# 4. Confirm
# -----------------------------------------------------------------------

plan="$(selected_table)"
count="$(printf '%s\n' "$plan" | grep -c .)"
log "About to deploy/upgrade $count program(s):"
printf '%s\n' "$plan" | while IFS=$'\t' read -r name id; do printf '  - %s -> %s\n' "$name" "$id"; done

if [ "$ASSUME_YES" -ne 1 ]; then
  [ -t 0 ] || die "refusing to deploy without confirmation on a non-interactive shell (pass --yes)"
  printf 'Continue? [y/N] '
  read -r reply
  case "$reply" in
    y|Y|yes|YES) ;;
    *) die "aborted" ;;
  esac
fi

# -----------------------------------------------------------------------
# 5. Deploy
# -----------------------------------------------------------------------

deployed=()
while IFS=$'\t' read -r name id; do
  [ -n "$name" ] || continue
  verify_program_keypair "$name" "$id" >/dev/null

  log "Deploying $name -> $id"
  solana program deploy \
    --url "$RPC_URL" \
    --keypair "$PAYER" \
    --upgrade-authority "$PAYER" \
    --program-id "target/deploy/${name}-keypair.json" \
    "target/deploy/${name}.so"

  deployed+=("$name:$id")
done <<<"$plan"

# -----------------------------------------------------------------------
# 6. Verify
# -----------------------------------------------------------------------

log "Verifying ${#deployed[@]} program(s) on devnet"
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

All ${#deployed[@]} program(s) are live on devnet.

Next steps (each is a separate privileged initializer, signed by the upgrade
authority above because that is what the programs check):

  1. kyc_gate::initialize_kyc_mint     - creates the soulbound attestation mint
  2. transfer_hook::initialize         - creates the whitelist config
     transfer_hook::initialize_extra_account_meta_list - once per RWA mint
  3. settlement::initialize            - creates the clearinghouse config
     settlement::set_transfer_agent    - installs the Transfer Agent's key

Point the client at devnet:
  export ANCHOR_PROVIDER_URL=$RPC_URL
EOF
