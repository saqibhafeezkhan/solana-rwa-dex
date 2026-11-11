#!/usr/bin/env bash
#
# Installs a pinned gitleaks release into .tools/ (gitignored), no root needed.
# The pre-commit hook (scripts/gitleaks-staged.sh) prefers a PATH install and
# falls back to this binary.
set -euo pipefail

VERSION="8.30.1"

root="$(git rev-parse --show-toplevel)" || exit 1
dest="$root/.tools"

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$os" in
  linux | darwin) ;;
  *)
    echo "unsupported OS: $os — download a release from https://github.com/gitleaks/gitleaks/releases" >&2
    exit 1
    ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64) arch="x64" ;;
  arm64 | aarch64) arch="arm64" ;;
  *)
    echo "unsupported architecture: $arch — download a release from https://github.com/gitleaks/gitleaks/releases" >&2
    exit 1
    ;;
esac

asset="gitleaks_${VERSION}_${os}_${arch}.tar.gz"
base="https://github.com/gitleaks/gitleaks/releases/download/v${VERSION}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "downloading $asset"
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/gitleaks_${VERSION}_checksums.txt" -o "$tmp/checksums.txt"

( cd "$tmp" && grep -E "  ${asset}$" checksums.txt | sha256sum -c - )

mkdir -p "$dest"
tar -xzf "$tmp/$asset" -C "$dest" gitleaks
chmod +x "$dest/gitleaks"

echo "installed $("$dest/gitleaks" version) at .tools/gitleaks"
