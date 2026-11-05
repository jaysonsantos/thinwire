#!/usr/bin/env bash
# Keep `unsafe` code in one module. The workspace lint `unsafe_code` is
# `deny`, so a module can allow it. Only `helper::fds` may: it marks the
# descriptors of a helper child close-on-exec between fork and exec.
#
# CI runs this from `scripts/test.sh`. It fails when another file allows
# `unsafe_code`, or when the workspace lint is no longer `deny`.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

allowed="crates/thinwire-protocol/src/helper/fds.rs"

grep -q '^unsafe_code = "deny"$' Cargo.toml || {
  echo "check-unsafe.sh: the workspace lint unsafe_code must be \"deny\"" >&2
  exit 1
}

found="$(grep -rln --include='*.rs' 'allow(unsafe_code)' crates || true)"
if [[ "$found" != "$allowed" ]]; then
  echo "check-unsafe.sh: only $allowed may allow unsafe_code. Found:" >&2
  echo "${found:-<none>}" >&2
  exit 1
fi

echo "unsafe code only in $allowed"
