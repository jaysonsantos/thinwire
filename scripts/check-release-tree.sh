#!/usr/bin/env bash
# ADR 0013: the MIT app binary of a release must link no AGPL code. AGPL
# code runs in the helper programs only.
#
# Fail if the app's cargo tree, with the release features, has an AGPL Signal
# crate, the WhatsApp libsignal fork, or an AGPL protocol crate of this
# repository (thinwire-signal, thinwire-whatsapp; #77).
# The release features come from the app build line of os-zips.yml, so this
# guard checks what a release builds. It also checks the app with every
# helper feature (`whatsapp-web`, `signal-local`): those features add the
# screens and the MIT helper adapter, never the AGPL client (#246, #248).
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(dirname "$script_dir")"
cd "$repo_root"

release_workflow=".github/workflows/os-zips.yml"

# Hyphenated crate names count: libsignal-service, libsignal-protocol, presage-*.
forbidden='(^|[^[:alnum:]_-])(presage|libsignal|wacore-libsignal|thinwire-signal|thinwire-whatsapp)(-[[:alnum:]_-]*)?([^[:alnum:]_-]|$)'

# `cargo tree` prints `name vX (path)`. Match the name, not the path: a
# checkout folder can be named like a crate (for example thinwire-whatsapp).
crate_part() {
  sed -E 's/ \(.*$//' <<<"$1"
}

line_is_forbidden() {
  grep -Eq "$forbidden" <<<"$(crate_part "$1")"
}

assert_guard() {
  local line="$1"
  local expect="$2"
  if line_is_forbidden "$line"; then
    if [[ "$expect" != "hit" ]]; then
      echo "release guard matched a safe line: ${line}" >&2
      exit 1
    fi
  elif [[ "$expect" == "hit" ]]; then
    echo "release guard missed: ${line}" >&2
    exit 1
  fi
}

assert_guard "libsignal-service v0.1.0" hit
assert_guard "libsignal-protocol v0.1.0" hit
assert_guard "presage v0.7.0" hit
assert_guard "presage-store-sled v0.6.0" hit
assert_guard "wacore-libsignal v0.1.0" hit
assert_guard "thinwire-signal v0.1.0" hit
assert_guard "thinwire-whatsapp v0.1.0" hit
assert_guard "thinwire v0.1.0" miss
assert_guard "thinwire-protocol v0.1.0" miss
assert_guard "thinwire-core v0.1.0" miss
assert_guard "thinwire-ipc v0.1.0" miss
assert_guard "thinwire v0.1.0 (/home/me/thinwire-whatsapp/crates/thinwire)" miss
assert_guard "thinwire-whatsapp v0.1.0 (/home/me/thinwire/crates/thinwire-whatsapp)" hit
assert_guard "libsignalx v0.1.0" miss

# The `--features` value of the app build line of the release workflow.
release_features() {
  local line
  line="$(grep -m1 -E 'cargo build .*-p thinwire( |$)' "$1" || true)"
  if [[ -z "$line" || "$line" == *"--all-features"* ]]; then
    echo "no app build line with a feature list in $1" >&2
    return 1
  fi
  sed -nE 's/.*--features[ =]([^ ]+).*/\1/p' <<<"$line"
}

# Print the forbidden crates in one cargo tree. Non-zero if it has one.
# `manifest` is the workspace manifest. The other arguments go to cargo tree.
check_tree() {
  local label="$1" manifest="$2"
  shift 2
  local tree
  tree="$(cargo tree --manifest-path "$manifest" "$@" --prefix none | sed -E 's/ \(.*$//')"
  if grep -E "$forbidden" <<<"$tree"; then
    echo "app cargo tree (${label}) contains presage, libsignal, libsignal-service, wacore-libsignal, thinwire-signal, or thinwire-whatsapp (ADR 0013)" >&2
    return 1
  fi
}

# Self-test (#246 "Guard test"): the check must fail when the app links an
# AGPL crate. A small workspace gives `thinwire` a dependency on an AGPL
# crate of this repository (`thinwire-whatsapp` by default). The check must
# fail on it, also when a feature turns the dependency on, and pass once the
# dependency is gone.
fixture_workspace() {
  local root="$1" dep="$2" agpl="${3:-thinwire-whatsapp}" feature="${4:-whatsapp-web}"
  mkdir -p "$root/crates/thinwire/src" "$root/crates/$agpl/src"
  printf '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n' >"$root/Cargo.toml"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\nlicense = "AGPL-3.0-only"\n' "$agpl" \
    >"$root/crates/$agpl/Cargo.toml"
  printf '[package]\nname = "thinwire"\nversion = "0.1.0"\nedition = "2021"\nlicense = "MIT"\n\n[features]\n%s = [%s]\n\n[dependencies]\n' \
    "$feature" "$([[ "$dep" == optional ]] && printf '"dep:%s"' "$agpl")" >"$root/crates/thinwire/Cargo.toml"
  case "$dep" in
    linked) printf '%s = { path = "../%s" }\n' "$agpl" "$agpl" >>"$root/crates/thinwire/Cargo.toml" ;;
    optional) printf '%s = { path = "../%s", optional = true }\n' "$agpl" "$agpl" >>"$root/crates/thinwire/Cargo.toml" ;;
  esac
  : >"$root/crates/thinwire/src/lib.rs"
  : >"$root/crates/$agpl/src/lib.rs"
}
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
fixture_workspace "$fixture/linked" linked
if CARGO_NET_OFFLINE=true check_tree "self-test" "$fixture/linked/Cargo.toml" -p thinwire >/dev/null 2>&1; then
  echo "check-release-tree self-test: an app that links thinwire-whatsapp passed" >&2
  exit 1
fi
fixture_workspace "$fixture/optional" optional
if CARGO_NET_OFFLINE=true check_tree "self-test" "$fixture/optional/Cargo.toml" -p thinwire --features whatsapp-web >/dev/null 2>&1; then
  echo "check-release-tree self-test: an app whose release feature links thinwire-whatsapp passed" >&2
  exit 1
fi
# The Signal crate, as `signal-local` linked it before #248.
fixture_workspace "$fixture/signal" optional thinwire-signal signal-local
if CARGO_NET_OFFLINE=true check_tree "self-test" "$fixture/signal/Cargo.toml" -p thinwire --features signal-local >/dev/null 2>&1; then
  echo "check-release-tree self-test: an app whose release feature links thinwire-signal passed" >&2
  exit 1
fi
fixture_workspace "$fixture/clean" none
if ! CARGO_NET_OFFLINE=true check_tree "self-test" "$fixture/clean/Cargo.toml" -p thinwire --features whatsapp-web; then
  echo "check-release-tree self-test: an app with no AGPL dependency failed" >&2
  exit 1
fi
printf '      run: cargo build --release -p thinwire --features one,two\n      run: cargo build --release -p thinwire-whatsapp --features three\n' \
  >"$fixture/workflow.yml"
if [[ "$(release_features "$fixture/workflow.yml")" != "one,two" ]]; then
  echo "check-release-tree self-test: the release features were not read from the app build line" >&2
  exit 1
fi

features="$(release_features "$release_workflow")"
if [[ -z "$features" ]]; then
  echo "the app build line of ${release_workflow} names no feature" >&2
  exit 1
fi

check_tree "default features" Cargo.toml -p thinwire
check_tree "release features: ${features}" Cargo.toml -p thinwire --features "$features"
# The helper features of the app, also when a release does not name one.
check_tree "helper features" Cargo.toml -p thinwire --features whatsapp-web,signal-local

# The guard must be able to see the AGPL code that it keeps out: each helper
# with its release feature has it.
helper_tree="$(cargo tree -p thinwire-whatsapp --features whatsapp-web --prefix none | sed -E 's/ \(.*$//')"
if ! grep -qE '^wacore-libsignal ' <<<"$helper_tree"; then
  echo "the WhatsApp helper tree has no wacore-libsignal: the release guard pattern is out of date" >&2
  exit 1
fi
helper_tree="$(cargo tree -p thinwire-signal --features signal-local --prefix none | sed -E 's/ \(.*$//')"
for needed in presage libsignal-protocol; do
  if ! grep -qE "^${needed} " <<<"$helper_tree"; then
    echo "the Signal helper tree has no ${needed}: the release guard pattern is out of date" >&2
    exit 1
  fi
done
