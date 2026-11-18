#!/usr/bin/env bash
#
# A bare `solana-test-validator`: `--reset`, ledger `.anchor/test-ledger`, RPC
# 8899, faucet 9900. It loads no project programs, no fixture accounts and no
# funded payer, and it is not the same ledger a `deploy-localnet.sh` run leaves
# behind - programs can only enter a validator's genesis block when it starts,
# so `deploy-localnet.sh --no-build` cannot add them to this one unless matching
# program keypairs exist under target/deploy (they are not tracked).
#
# Use `./scripts/deploy-localnet.sh` when you want the programs on the chain;
# use this script only when you want a chain and nothing on it.
set -euo pipefail

cd "$(dirname "$0")/.."

mkdir -p .anchor
solana-test-validator \
  --reset \
  --ledger .anchor/test-ledger \
  --rpc-port 8899 \
  --faucet-port 9900
