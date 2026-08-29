#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

mkdir -p .anchor
solana-test-validator \
  --reset \
  --ledger .anchor/test-ledger \
  --rpc-port 8899 \
  --faucet-port 9900
