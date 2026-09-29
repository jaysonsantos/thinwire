#!/usr/bin/env bash
# ADR 0011, #77: MIT crates must not depend on the AGPL protocol crates.
# The binary `thinwire` may list them, but only as optional dependencies
# behind the local-only features (`signal-local`, `whatsapp-web`).
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

agpl=()
mit=()
for manifest in crates/*/Cargo.toml; do
  name="$(grep -m1 -E '^name = ' "$manifest" | sed -E 's/^name = "(.*)"/\1/')"
  if grep -qE '^license = "AGPL-3\.0-only"' "$manifest"; then
    agpl+=("$name")
  else
    mit+=("$name")
  fi
done
if ((${#agpl[@]} == 0)); then
  echo "no AGPL crate found; the check reads crates/*/Cargo.toml" >&2
  exit 1
fi
pattern="$(IFS='|'; echo "${agpl[*]}")"

status=0
for manifest in crates/*/Cargo.toml; do
  name="$(grep -m1 -E '^name = ' "$manifest" | sed -E 's/^name = "(.*)"/\1/')"
  if grep -qE '^license = "AGPL-3\.0-only"' "$manifest"; then
    continue
  fi
  while IFS= read -r line; do
    if [[ "$name" == "thinwire" && "$line" == *"optional = true"* ]]; then
      continue
    fi
    echo "${manifest}: MIT crate depends on an AGPL crate: ${line}" >&2
    status=1
  done < <(grep -E "^(${pattern})( |=|\.)" "$manifest" || true)
done

# Default features, as public CI and release builds use them.
for name in "${mit[@]}"; do
  tree="$(cargo tree -p "$name" --edges normal,build --prefix none)"
  if grep -E "^(${pattern})[ @]" <<<"$tree"; then
    echo "${name} (default features) links an AGPL crate (ADR 0011)" >&2
    status=1
  fi
done
exit "$status"
