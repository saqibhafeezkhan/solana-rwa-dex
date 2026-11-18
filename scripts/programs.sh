#!/usr/bin/env bash
# Shared program manifest and helpers for the deploy scripts.
#
# Sourced by `scripts/deploy-localnet.sh` and `scripts/deploy-devnet.sh`; not
# meant to be run on its own.
#
# The program id in the table must match the crate's `declare_id!`. On devnet it
# must also match the keypair it deploys with, and `deploy-devnet.sh` verifies
# that before sending anything, because a mismatched keypair silently deploys to
# the wrong address and leaves every PDA-derived client broken. Localnet needs
# no keypair at all: `deploy-localnet.sh` loads the images into the validator's
# genesis block at these ids instead.

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

# Prints a script's leading comment block (line 2 up to the first line that is
# not a comment) with the `# ` markers stripped, for `--help`. The deploy scripts
# used to hard-code a line range, so editing the header silently truncated
# `--help` or spilled the shebang and `set -euo pipefail` into it.
usage_header() {
  local line
  while IFS= read -r line; do
    case "$line" in
      '#')  printf '\n' ;;
      '#'*) printf '%s\n' "${line#'# '}" ;;
      *)    break ;;
    esac
  done < <(sed -n '2,$p' "$1")
}

# A duplicated id in the table means two programs fight for one address: the
# later deploy silently overwrites the earlier program, and the final
# `solana program show` check still passes for both rows. An empty table is the
# same class of bug in the other direction - a deploy that reports success after
# deploying nothing.
verify_program_table() {
  local name id seen_names="" seen_ids="" count=0
  while IFS=$'\t' read -r name id; do
    [ -n "$name" ] || continue
    case " $seen_names " in *" $name "*) die "duplicate program name in program_table: $name" ;; esac
    case " $seen_ids " in *" $id "*) die "duplicate program id in program_table: $id" ;; esac
    seen_names="$seen_names $name"
    seen_ids="$seen_ids $id"
    count=$((count + 1))
  done < <(program_table)
  [ "$count" -gt 0 ] || die "program_table is empty"
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

  # Program keypairs are not tracked, and no build regenerates them: `build_programs`
  # passes --ignore-keys, and a plain `anchor build` would mint a fresh keypair and
  # so a *different* address. Saying "run with --build" here sent readers after a
  # fix that changes every PDA their clients derive.
  # No backticks in these messages: they are double-quoted arguments, so a
  # backtick runs the command inside it. That is not hypothetical - a message
  # that read `anchor keys sync` executed a real sync and rewrote every
  # declare_id! while printing the error. Single quotes are literal here.
  [ -f "$keypair" ] || die "missing program keypair: $keypair
  the keypair that owns $expected_id is not tracked, so a fresh clone cannot deploy to devnet
  fix: restore it from the deployer's backup, or move to a new address on purpose with
       'anchor keys sync' (rewrites declare_id! in the crate and Anchor.toml) and update
       the row in scripts/programs.sh"
  [ -f "target/deploy/${name}.so" ] || die "missing program image: target/deploy/${name}.so (run with --build)"

  local actual_id
  actual_id="$(solana address --keypair "$keypair" 2>/dev/null)" \
    || die "could not read the address of $keypair"

  [ "$actual_id" = "$expected_id" ] || die "$name keypair is $actual_id but the program expects $expected_id
  deploying it would put the program at an address its own declare_id! does not know
  fix: restore the keypair that owns $expected_id, or move to $actual_id on purpose with
       'anchor keys sync' (rewrites declare_id! in the crate and Anchor.toml) and update
       the row in scripts/programs.sh"

  printf '%s\t%s\n' "$name" "$actual_id"
}

# Two passes, because each answers a different question and neither folds into
# the other. Both behaviours were reproduced against the toolchain `[toolchain]`
# in Anchor.toml pins (anchor-cli 1.2.0, Agave 3.1.10, platform-tools v1.52):
#
#   1. `anchor build` is what generates `target/idl/*.json`, which
#      `anchor.workspace.*` in tests/ts reads. It refuses to build at all when
#      `target/deploy/<name>-keypair.json` does not match the crate's
#      `declare_id!` - the normal state of a fresh clone, since program keypairs
#      are not tracked. `--ignore-keys` skips that check; the ids come from the
#      crates either way.
#   2. `anchor build` hard-codes `--arch v0` when it invokes cargo-build-sbf, so
#      the `.so` files it leaves behind are SBPF **v0** (`e_machine 0xF7`). The
#      Agave 3.1.10 CLI pinned here cannot consume those: `solana program
#      deploy` fails client-side with "Failed to parse ELF file: invalid file
#      header", and a validator that loaded one at genesis answers "Program is
#      not deployed" on every call. `--arch v1` cannot be forwarded through
#      `anchor build -- ...` (cargo-build-sbf rejects the duplicate argument),
#      so the images are rebuilt for arch **v1** (`e_machine 0x107`) directly.
#
# The IDL pass runs first: the second pass overwrites the images it writes.
build_programs() {
  local arch="${SBF_ARCH:-v1}"
  require_cmd cargo-build-sbf "ships with the Agave CLI (cargo build-sbf)"

  log "Generating IDLs (anchor build --ignore-keys)"
  anchor build --ignore-keys

  log "Building SBF images (cargo-build-sbf --arch $arch)"
  local name
  while IFS=$'\t' read -r name _; do
    [ -n "$name" ] || continue
    cargo-build-sbf --arch "$arch" --manifest-path "programs/${name}/Cargo.toml"
  done < <(program_table)

  verify_sbf_arch "$arch"
}

# Reads the ELF `e_machine` field (offset 18, little-endian u16). Without this a
# wrong-arch image only surfaces at deploy or invoke time, as a message that
# names neither the arch nor the file.
sbf_image_arch() {
  local image="$1" machine
  machine="$(od -An -tx1 -j18 -N2 "$image" | tr -d ' \n')"
  case "$machine" in
    f700) printf 'v0\n' ;;
    0701) printf 'v1\n' ;;
    *)    printf 'unknown(%s)\n' "$machine" ;;
  esac
}

# Fails at build time rather than at deploy time if any image is not `expected`.
verify_sbf_arch() {
  local expected="$1" name image actual failed=0
  while IFS=$'\t' read -r name _; do
    [ -n "$name" ] || continue
    image="target/deploy/${name}.so"
    [ -f "$image" ] || die "missing program image: $image (did the build fail?)"
    actual="$(sbf_image_arch "$image")"
    if [ "$actual" != "$expected" ]; then
      warn "$image is SBPF $actual, expected $expected"
      failed=$((failed + 1))
    fi
  done < <(program_table)
  [ "$failed" -eq 0 ] || die "$failed image(s) are not SBPF $expected; the pinned toolchain cannot load them"
}

# Both deploy scripts call this before sending anything. The images must be the
# arch the local CLI accepts, which is the one `build_programs` produced
# (`SBF_ARCH`, default v1). The silent version of this failure: a plain
# `anchor build` or an `anchor test` overwrites target/deploy with SBPF v0, and
# `--no-build` then deploys those images - `solana program deploy` rejects them
# client-side with "Failed to parse ELF file", and a validator that genesis-loads
# one answers "Program is not deployed" on every call, naming neither arch nor
# file.
require_deployable_images() {
  verify_sbf_arch "${SBF_ARCH:-v1}"
}

# Reads the `name<TAB>id` table into the caller's loop:
#   while IFS=$'\t' read -r name id; do ... done < <(program_table)
