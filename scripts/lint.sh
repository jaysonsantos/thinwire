#!/usr/bin/env bash
# Format and lint the workspace. CI runs this inside `nix develop`.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

# README hero images must stay present (paths under crates/thinwire/tests/snapshots/).
mapfile -t readme_snaps < <(grep -oE 'crates/thinwire/tests/snapshots/[^"[:space:]]+\.png' README.md | sort -u)
for snap in "${readme_snaps[@]}"; do
  if [[ ! -f "$snap" ]]; then
    echo "README references missing snapshot: $snap" >&2
    exit 1
  fi
done

if command -v prek >/dev/null; then
  prek run --all-files --show-diff-on-failure
  exit 0
fi

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

if command -v taplo >/dev/null; then
  taplo fmt --check
fi
if command -v typos >/dev/null; then
  typos
fi
if command -v shellcheck >/dev/null; then
  shellcheck scripts/*.sh
fi
if command -v nixfmt >/dev/null; then
  nixfmt --check flake.nix
fi
if command -v editorconfig-checker >/dev/null; then
  editorconfig-checker -exclude 'crates/thinwire-(signal|whatsapp)/LICENSE'
fi
if command -v gitleaks >/dev/null; then
  gitleaks detect --no-git --source . --redact
fi
if command -v zizmor >/dev/null && [[ -d .github/workflows ]]; then
  zizmor .github/workflows
fi
