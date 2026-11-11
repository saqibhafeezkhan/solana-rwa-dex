#!/usr/bin/env bash
#
# Scans the staged diff for secrets before a commit is recorded.
# Wired up as the `pre-commit` stage in lefthook.yml.
#
# gitleaks is not a project dependency: scripts/install-gitleaks.sh drops a
# pinned, checksum-verified binary in .tools/ (gitignored). When neither that
# binary nor a PATH install exists, the hook warns and lets the commit through
# rather than blocking a fresh clone that has not run the installer yet.
set -euo pipefail

root="$(git rev-parse --show-toplevel)" || exit 0
cd "$root"

bin="$(command -v gitleaks || true)"
if [ -z "$bin" ] && [ -x "$root/.tools/gitleaks" ]; then
  bin="$root/.tools/gitleaks"
fi

if [ -z "$bin" ]; then
  echo "secret scan skipped: gitleaks not installed — run 'bash scripts/install-gitleaks.sh' once" >&2
  exit 0
fi

# --staged implies --pre-commit: scan only what this commit would add.
exec "$bin" git --pre-commit --staged --redact --no-banner --verbose
