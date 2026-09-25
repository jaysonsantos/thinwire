#!/usr/bin/env bash
# Rewrite the checked-in UI snapshots from the current shell.
# Run this inside `nix develop` on Linux. The shell sets lavapipe
# (`VK_DRIVER_FILES`) so the pixels match CI.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

UPDATE_SNAPSHOTS=1 cargo test -p thinwire --features ui-snapshots ui_snapshots -- --test-threads=1
