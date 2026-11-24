#!/usr/bin/env bash
# Keep `unsafe` code in two modules. The workspace lint `unsafe_code` is
# `deny`, so a module can allow it. Only these may:
# - `helper::fds` marks the descriptors of a helper child close-on-exec
#   between fork and exec.
# - `thinwire_notify::network_macos` reads the cost of the network from
#   Network.framework, which has no safe binding.
#
# CI runs this from `scripts/test.sh`. It fails when another file allows
# `unsafe_code`, or when the workspace lint is no longer `deny`.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

allowed="crates/thinwire-notify/src/network_macos.rs
crates/thinwire-protocol/src/helper/fds.rs"

grep -q '^unsafe_code = "deny"$' Cargo.toml || {
  echo "check-unsafe.sh: the workspace lint unsafe_code must be \"deny\"" >&2
  exit 1
}

found="$(grep -rln --include='*.rs' 'allow(unsafe_code)' crates | sort || true)"
if [[ "$found" != "$allowed" ]]; then
  echo "check-unsafe.sh: only these files may allow unsafe_code:" >&2
  echo "$allowed" >&2
  echo "Found:" >&2
  echo "${found:-<none>}" >&2
  exit 1
fi

echo "unsafe code only in:"
echo "$allowed"
