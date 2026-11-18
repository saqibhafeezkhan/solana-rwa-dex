#!/usr/bin/env bash
#
# Builds the SBF images the e2e suites load, plus the IDLs the clients read.
#
#   ./scripts/build-programs.sh              # IDLs + SBPF v1 images
#   SBF_ARCH=v0 ./scripts/build-programs.sh  # only if a loader needs v0
#
# This is `build_programs` from scripts/programs.sh, which both deploy scripts
# call, exposed on its own for the test suites and for a warm build before
# `deploy-localnet.sh --no-build`. It is deliberately not a plain `anchor build`:
# that aborts on a keypair/declare_id mismatch and emits SBPF v0 images the
# pinned Agave 3.1.10 toolchain cannot load. See scripts/README.md.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/programs.sh
source "$SCRIPT_DIR/programs.sh"

cd "$(repo_root "${BASH_SOURCE[0]}")"

require_cmd anchor "install with: cargo install --git https://github.com/coral-xyz/anchor avm --force && avm install latest"
verify_program_table
build_programs

log "Images in target/deploy:"
while IFS=$'\t' read -r name _; do
  [ -n "$name" ] || continue
  printf '  %-13s %s\n' "$name" "$(sbf_image_arch "target/deploy/${name}.so")"
done < <(program_table)
