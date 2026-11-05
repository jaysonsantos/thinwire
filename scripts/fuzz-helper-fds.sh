#!/usr/bin/env bash
# Stress run of `helper::fds`: no descriptor of the app may reach a helper
# child. Each round plants random descriptors at random numbers, some with
# the close-on-exec mark and some without. Children start on several
# threads while other threads open and close files. Run it from time to
# time, inside `nix develop` on Linux or on macOS:
#   scripts/fuzz-helper-fds.sh             # 200 rounds, a new seed
#   scripts/fuzz-helper-fds.sh 5000        # 5000 rounds
#   THINWIRE_FD_STRESS_SEED=123 scripts/fuzz-helper-fds.sh 200   # repeat a run
# A failure prints the seed and the round. Not part of CI: it is slow.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

export THINWIRE_FD_STRESS_ROUNDS="${1:-200}"
cargo test -p thinwire-protocol --lib helper::fds::tests::fd_stress -- --ignored --exact --nocapture
