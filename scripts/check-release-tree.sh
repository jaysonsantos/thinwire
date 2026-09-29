#!/usr/bin/env bash
# Fail if a release build links AGPL Signal crates, the WhatsApp libsignal fork,
# or the AGPL protocol crates of this repository (thinwire-signal,
# thinwire-whatsapp; #77).
# The release feature set matches os-zips.yml: telegram-tdlib, default features otherwise.
# signal-local and whatsapp-web stay off.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

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
assert_guard "thinwire v0.1.0 (/home/me/thinwire-whatsapp/crates/thinwire)" miss
assert_guard "thinwire-whatsapp v0.1.0 (/home/me/thinwire/crates/thinwire-whatsapp)" hit
assert_guard "libsignalx v0.1.0" miss

check_tree() {
  local label="$1"
  shift
  local tree
  tree="$(cargo tree "$@" --prefix none | sed -E 's/ \(.*$//')"
  if grep -E "$forbidden" <<<"$tree"; then
    echo "release cargo tree (${label}) contains presage, libsignal, libsignal-service, wacore-libsignal, thinwire-signal, or thinwire-whatsapp" >&2
    exit 1
  fi
}

check_tree "default features" -p thinwire
check_tree "os-zips telegram-tdlib" -p thinwire --features telegram-tdlib
