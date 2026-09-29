#!/usr/bin/env bash
# ADR 0011, #77: MIT crates must not depend on the AGPL protocol crates.
# The binary `thinwire` may list them, but only as optional dependencies
# behind the local-only features (`signal-local`, `whatsapp-web`).
# This guard keeps AGPL code out of release builds. It is not optional.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(dirname "$script_dir")"

# True if a Cargo `license` value names an AGPL license: every AGPL SPDX id
# (AGPL-3.0, AGPL-3.0-only, AGPL-3.0-or-later), also inside an OR / AND
# expression. SPDX ids match without case.
is_agpl() {
  grep -qiE '(^|[^[:alnum:].+-])AGPL-[0-9]' <<<"$1"
}

# The `license` value of one crate manifest. `license.workspace = true` (or
# `license = { workspace = true }`) takes the value of [workspace.package].
license_of() {
  local manifest="$1" root="$2" value
  value="$(sed -nE 's/^license[[:space:]]*=[[:space:]]*"(.*)"/\1/p' "$manifest" | head -n1)"
  if [[ -z "$value" ]] &&
    grep -qE '^license([[:space:]]*\.[[:space:]]*workspace|[[:space:]]*=[[:space:]]*\{[^}]*workspace)[^}]*true' "$manifest"; then
    value="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$root/Cargo.toml" |
      sed -nE 's/^license[[:space:]]*=[[:space:]]*"(.*)"/\1/p' | head -n1)"
  fi
  printf '%s' "$value"
}

# True if the crate in the folder of `manifest` is AGPL: by its license value,
# or by a license file with the AGPL text (a `license-file` crate).
crate_is_agpl() {
  local manifest="$1" root="$2" dir
  dir="$(dirname "$manifest")"
  if is_agpl "$(license_of "$manifest" "$root")"; then
    return 0
  fi
  grep -qls "GNU AFFERO GENERAL PUBLIC LICENSE" "$dir"/LICENSE* "$dir"/COPYING* 2>/dev/null
}

crate_name() {
  grep -m1 -E '^name = ' "$1" | sed -E 's/^name = "(.*)"/\1/'
}

# Check the workspace at `root`. Non-zero if an MIT crate depends on an AGPL
# crate. `require_agpl` = 1: a workspace with no AGPL crate is an error.
check_workspace() {
  local root="$1" require_agpl="$2" manifest name line tree pattern status=0
  local agpl=() mit=()
  for manifest in "$root"/crates/*/Cargo.toml; do
    name="$(crate_name "$manifest")"
    if crate_is_agpl "$manifest" "$root"; then
      agpl+=("$name")
    else
      mit+=("$name")
    fi
  done
  if ((${#agpl[@]} == 0)); then
    if [[ "$require_agpl" == 1 ]]; then
      echo "no AGPL crate found; the check reads crates/*/Cargo.toml" >&2
      return 1
    fi
    return 0
  fi
  pattern="$(
    IFS='|'
    echo "${agpl[*]}"
  )"

  for manifest in "$root"/crates/*/Cargo.toml; do
    name="$(crate_name "$manifest")"
    if crate_is_agpl "$manifest" "$root"; then
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

  # Default features, as public CI and release builds use them. The edges
  # skip dev-dependencies on purpose: they never reach a release build.
  for name in "${mit[@]}"; do
    tree="$(cargo tree --manifest-path "$root/Cargo.toml" -p "$name" --edges normal,build --prefix none)"
    if grep -E "^(${pattern})[ @]" <<<"$tree"; then
      echo "${name} (default features) links an AGPL crate (ADR 0011)" >&2
      status=1
    fi
  done
  return "$status"
}

# Self-tests: the guard must see every AGPL license form.
assert_license() {
  local value="$1" want="$2" got=miss
  if is_agpl "$value"; then
    got=hit
  fi
  if [[ "$got" != "$want" ]]; then
    echo "check-agpl-deps self-test: license '${value}': want ${want}, got ${got}" >&2
    exit 1
  fi
}
assert_license "AGPL-3.0" hit
assert_license "AGPL-3.0-only" hit
assert_license "AGPL-3.0-or-later" hit
assert_license "agpl-3.0-only" hit
assert_license "MIT OR AGPL-3.0-or-later" hit
assert_license "(Apache-2.0 AND AGPL-3.0-only)" hit
assert_license "AGPL-3.0-only WITH some-exception" hit
assert_license "MIT" miss
assert_license "MIT OR Apache-2.0" miss
assert_license "LGPL-3.0-only" miss
assert_license "GPL-3.0-or-later" miss
assert_license "" miss

# A small workspace where an MIT crate depends on an AGPL-3.0-or-later crate.
# The check must fail on it, and pass once the dependency is gone.
# `license_line` is the license line of the AGPL crate. With
# `license.workspace = true` the workspace gives the AGPL value.
fixture_workspace() {
  local root="$1" with_dep="$2" license_line="${3:-license = \"AGPL-3.0-or-later\"}"
  mkdir -p "$root/crates/app/src" "$root/crates/agpl/src"
  printf '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n\n[workspace.package]\nlicense = "AGPL-3.0-or-later"\n' \
    >"$root/Cargo.toml"
  printf '[package]\nname = "fixture-agpl"\nversion = "0.1.0"\nedition = "2021"\n%s\n' "$license_line" \
    >"$root/crates/agpl/Cargo.toml"
  printf '[package]\nname = "fixture-app"\nversion = "0.1.0"\nedition = "2021"\nlicense = "MIT"\n\n[dependencies]\n' \
    >"$root/crates/app/Cargo.toml"
  if [[ "$with_dep" == 1 ]]; then
    printf 'fixture-agpl = { path = "../agpl" }\n' >>"$root/crates/app/Cargo.toml"
  fi
  : >"$root/crates/app/src/lib.rs"
  : >"$root/crates/agpl/src/lib.rs"
}
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
fixture_workspace "$fixture/bad" 1
if CARGO_NET_OFFLINE=true check_workspace "$fixture/bad" 0 >/dev/null 2>&1; then
  echo "check-agpl-deps self-test: an MIT crate with an AGPL-3.0-or-later dependency passed" >&2
  exit 1
fi
fixture_workspace "$fixture/inherited" 1 "license.workspace = true"
if CARGO_NET_OFFLINE=true check_workspace "$fixture/inherited" 0 >/dev/null 2>&1; then
  echo "check-agpl-deps self-test: a dependency with an inherited AGPL license passed" >&2
  exit 1
fi
fixture_workspace "$fixture/good" 0
if ! CARGO_NET_OFFLINE=true check_workspace "$fixture/good" 1; then
  echo "check-agpl-deps self-test: a clean fixture workspace failed" >&2
  exit 1
fi

check_workspace "$repo_root" 1
