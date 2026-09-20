#!/usr/bin/env bash
# Lint, test, and build. No GUI Docker image: this is a desktop egui app.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

"$script_dir/lint.sh"
"$script_dir/test.sh"
cargo build --workspace
