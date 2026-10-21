#!/usr/bin/env bash
# Shared program manifest and helpers for the deploy scripts.
#
# Sourced by `scripts/deploy-localnet.sh` and `scripts/deploy-devnet.sh`; not
# meant to be run on its own.
#
# The program id in the table must match the crate's `declare_id!` and the id
# baked into `target/deploy/<name>-keypair.json`. Both deploy scripts verify all
# three agree before sending anything, because a mismatched keypair silently
# deploys to the wrong address and leaves every PDA-derived client broken.

# `name<TAB>program-id`, leaf-first. Deploy order does not matter on-chain
# (Anchor programs call each other by id, not by deploy sequence), but the order
# reads as the dependency graph: the compliance and oracle primitives, then the
# escrow and the hook that routes into it, then the four AMMs that wire up to
# all of them.
program_table() {
  cat <<'EOF'
oracle	4ENC2RRJGUhttqMvqztBBFtP7dn3cvMQZvgfBG1pmFoj
kyc_gate	7Tqk6HWJcnEPAdNdRy4ZfqreaNuT9nqFSCLdnLCYnDK1
transfer_hook	fo9ANuG3MfLiiQcqipXLNaSoPoMyp4ArAzJtwNbhxNS
rwa_mint	5GbeJh4sySASsdH2zRdFX85LuXnu5y3rBkqhVi76Lzho
settlement	58sKC5UqKZrMrSj5bGoEdyfwbckm2CwvrHPDCbESoLJ6
hooks	B5thQMyLNmPsgWVnsscaP4BFUMVW7QFwnufiJKq6Y1DA
order_book	3ACX2Lc67uZW65PA3QbYRLttmxE766eSDyD9Sd2yggwD
clmm	GKvqhzy7V6MN8YRuQchWahMJLG4cbHWdWzEWyoSG1XNU
dlmm	5JPnmJouZ3yE9Xwaacjtr5F2E1ShYGxJ1mR4UV9EFE3c
maverick	9hQXDPwodmaTZYaAEDVE8EWRKb3d39Wr2Lb3zFgSaBW4
twamm	FeApqgNan2ifjbhzK1dvDJxdEM45xEEgFJPpCzKr72H9
EOF
}

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1${2:+ ($2)}"
}

# Resolves the repo root from a script's location, so the scripts work from any
# cwd. Call it as `repo_root "${BASH_SOURCE[0]}"` from a top-level script: the
# path has to be the script's own, not this sourced file's.
repo_root() {
  # Keep this message free of quote characters: bash parses the `:?` word for
  # quotes even when the whole expansion sits inside double quotes.
  local script="${1:?repo_root needs the calling script path}"
  cd "$(dirname "$script")/.." && pwd
}

# Verifies that `target/deploy/<name>-keypair.json` really is the keypair for the
# program id the crates declare. Prints the name and id on one line on success.
verify_program_keypair() {
  local name="$1" expected_id="$2"
  # A separate statement on purpose: `local` expands all of its arguments before
  # it creates any of them, so `${name}` on the same line would read the caller's
  # variable (and trip `set -u` when there is none).
  local keypair="target/deploy/${name}-keypair.json"

  [ -f "$keypair" ] || die "missing program keypair: $keypair (run with --build)"
  [ -f "target/deploy/${name}.so" ] || die "missing program image: target/deploy/${name}.so (run with --build)"

  local actual_id
  actual_id="$(solana address --keypair "$keypair" 2>/dev/null)" \
    || die "could not read the address of $keypair"

  [ "$actual_id" = "$expected_id" ] || die "$name keypair is $actual_id but the program expects $expected_id
  fix: delete $keypair and re-run --build so Anchor derives a matching id, or
       update declare_id! in programs/$name/src/lib.rs and Anchor.toml"

  printf '%s\t%s\n' "$name" "$actual_id"
}

# `anchor build` regenerates the program keypairs from the ids in the crates, so
# running it is what keeps the keypairs and the program ids in step.
#
# `--arch v0` is load-bearing: Anchor 1.2 builds SBPF **v3** by default, and the
# Agave 3.1.x toolchain this repo pins (Anchor.toml `solana_version`) cannot
# parse a v3 image — `solana program deploy` fails client-side with
# "Failed to parse ELF file: invalid file header" before anything reaches the
# cluster. v0 is what `cargo-build-sbf` itself defaults to and what the local
# validator in `scripts/deploy-localnet.sh` accepts. Override with
# `SBF_ARCH=v1 bash ...` if a newer cluster loader requires it.
build_programs() {
  local arch="${SBF_ARCH:-v0}"
  log "Building SBF images (anchor build --arch $arch)"
  anchor build --arch "$arch"
}

# Reads the `name<TAB>id` table into the caller's loop:
#   while IFS=$'\t' read -r name id; do ... done < <(program_table)
