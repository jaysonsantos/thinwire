#!/usr/bin/env bash
# Fail if a release build links AGPL Signal crates or the WhatsApp libsignal fork.
# The release feature set matches os-zips.yml: telegram-tdlib, default features otherwise.
# signal-local and whatsapp-web stay off.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

# Hyphenated crate names count: libsignal-service, libsignal-protocol, presage-*.
forbidden='(^|[^[:alnum:]_-])(presage|libsignal|wacore-libsignal)(-[[:alnum:]_-]*)?([^[:alnum:]_-]|$)'

line_is_forbidden() {
  grep -Eq "$forbidden" <<<"$1"
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
assert_guard "thinwire v0.1.0" miss
assert_guard "libsignalx v0.1.0" miss

check_tree() {
  local label="$1"
  shift
  local tree
  tree="$(cargo tree "$@" --prefix none)"
  if grep -E "$forbidden" <<<"$tree"; then
    echo "release cargo tree (${label}) contains presage, libsignal, libsignal-service, or wacore-libsignal" >&2
    exit 1
  fi
}

check_tree "default features" -p thinwire
check_tree "os-zips telegram-tdlib" -p thinwire --features telegram-tdlib
