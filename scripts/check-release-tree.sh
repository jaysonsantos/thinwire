#!/usr/bin/env bash
# Fail if a release build links AGPL Signal crates or the WhatsApp libsignal fork.
# The release feature set matches os-zips.yml: telegram-tdlib, default features otherwise.
# signal-local and whatsapp-web stay off.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

forbidden='(^|[^[:alnum:]_-])(presage|libsignal|wacore-libsignal)([^[:alnum:]_-]|$)'

check_tree() {
  local label="$1"
  shift
  local tree
  tree="$(cargo tree "$@" --prefix none)"
  if grep -E "$forbidden" <<<"$tree"; then
    echo "release cargo tree (${label}) contains presage, libsignal, or wacore-libsignal" >&2
    exit 1
  fi
}

check_tree "default features" -p thinwire
check_tree "os-zips telegram-tdlib" -p thinwire --features telegram-tdlib
