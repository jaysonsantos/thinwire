#!/usr/bin/env bash
# Run every workspace test. CI runs this inside `nix develop`.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

cargo test --workspace
scripts/check-core-deps.sh
