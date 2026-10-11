#!/usr/bin/env bash
# ADR 0013, #77: MIT crates must not depend on the AGPL protocol crates.
# AGPL code runs in helper programs, and the MIT app talks to them over a
# pipe. One exception stays until Signal has its helper (#248): the binary
# `thinwire` may name `thinwire-signal` as an optional dependency behind the
# local-only feature `signal-local`. No MIT crate may name `thinwire-whatsapp`.
# This guard keeps AGPL code out of the MIT app. It is not optional.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(dirname "$script_dir")"

# The MIT crate that may name an AGPL crate, and that crate. Only as an
# optional dependency.
optional_from="thinwire"
optional_to="thinwire-signal"

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
      if [[ "$name" == "$optional_from" && "$line" == *"optional = true"* ]] &&
        grep -qE "^${optional_to}( |=|\.)" <<<"$line"; then
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
      echo "${name} (default features) links an AGPL crate (ADR 0013)" >&2
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
# `with_dep`: 0 = no dependency, 1 = a normal one, optional = an optional one.
# The last two arguments are the names of the MIT crate and the AGPL crate.
fixture_workspace() {
  local root="$1" with_dep="$2" license_line="${3:-license = \"AGPL-3.0-or-later\"}"
  local app_name="${4:-fixture-app}" agpl_name="${5:-fixture-agpl}"
  mkdir -p "$root/crates/app/src" "$root/crates/agpl/src"
  printf '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n\n[workspace.package]\nlicense = "AGPL-3.0-or-later"\n' \
    >"$root/Cargo.toml"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n%s\n' "$agpl_name" "$license_line" \
    >"$root/crates/agpl/Cargo.toml"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\nlicense = "MIT"\n\n[dependencies]\n' "$app_name" \
    >"$root/crates/app/Cargo.toml"
  case "$with_dep" in
    1) printf '%s = { path = "../agpl" }\n' "$agpl_name" >>"$root/crates/app/Cargo.toml" ;;
    optional) printf '%s = { path = "../agpl", optional = true }\n' "$agpl_name" >>"$root/crates/app/Cargo.toml" ;;
  esac
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
# ADR 0013: the app must not name the WhatsApp crate, also not as an
# optional dependency. Only `thinwire-signal` has that exception (#248).
fixture_workspace "$fixture/optional-whatsapp" optional "" thinwire thinwire-whatsapp
if CARGO_NET_OFFLINE=true check_workspace "$fixture/optional-whatsapp" 0 >/dev/null 2>&1; then
  echo "check-agpl-deps self-test: thinwire with an optional thinwire-whatsapp dependency passed" >&2
  exit 1
fi
fixture_workspace "$fixture/optional-signal" optional "" thinwire thinwire-signal
if ! CARGO_NET_OFFLINE=true check_workspace "$fixture/optional-signal" 1; then
  echo "check-agpl-deps self-test: thinwire with an optional thinwire-signal dependency failed" >&2
  exit 1
fi
fixture_workspace "$fixture/optional-other" optional "" thinwire-core thinwire-signal
if CARGO_NET_OFFLINE=true check_workspace "$fixture/optional-other" 0 >/dev/null 2>&1; then
  echo "check-agpl-deps self-test: thinwire-core with an optional thinwire-signal dependency passed" >&2
  exit 1
fi

check_workspace "$repo_root" 1
