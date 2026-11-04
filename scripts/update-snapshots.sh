#!/usr/bin/env bash
# Rewrite the checked-in UI snapshots from the current shell, then shrink them
# with oxipng. Run this inside `nix develop` on Linux. The shell sets lavapipe
# (`VK_DRIVER_FILES`) so the pixels match CI.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

UPDATE_SNAPSHOTS=1 cargo test -p thinwire --features ui-snapshots ui_snapshots -- --test-threads=1

# egui_kittest writes plain PNG files. The snapshots are most of the repository, so
# they go in recompressed. oxipng is lossless, and the tests compare pixels, so
# a recompressed image still passes. oxipng leaves a file alone when it cannot
# make it smaller, so a second run changes nothing. The `.old.png` backups that
# kittest keeps, and the `.new.png` and `.diff.png` of a failed run, stay out.
find crates/thinwire/tests/snapshots -name '*.png' \
    ! -name '*.old.png' ! -name '*.new.png' ! -name '*.diff.png' -print0 |
    xargs -0 oxipng --opt 4 --strip safe --quiet
