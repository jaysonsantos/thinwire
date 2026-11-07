#!/usr/bin/env bash
# Print major.minor.patch.r<count>.g<7 hex> for one commit.
# The count is the same number publish-nightly.sh puts in the archive name.
# A shallow repository would count too few commits, so this refuses one.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

sha="${1:-HEAD}"

workspace_version=$(awk '
  $0 == "[workspace.package]" { in_pkg = 1; next }
  in_pkg && /^\[/ { exit }
  in_pkg && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' Cargo.toml)
if [[ ! "$workspace_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "::error::No workspace version in Cargo.toml. Got: ${workspace_version}" >&2
  exit 1
fi

if [[ "$(git rev-parse --is-shallow-repository)" == "true" ]]; then
  echo "::error::Refusing to count commits in a shallow repository." >&2
  exit 1
fi

count=$(git rev-list --count "$sha")
full=$(git rev-parse "$sha")
short=${full:0:7}
printf '%s.r%s.g%s\n' "$workspace_version" "$count" "$short"
