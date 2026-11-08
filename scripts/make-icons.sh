#!/usr/bin/env bash
# Makes the app icon files from crates/thinwire/assets/icon.svg:
#   crates/thinwire/assets/icon-256.png  window icon (include_bytes! in main.rs)
#   crates/thinwire/assets/AppIcon.icns  macOS bundle icon (stage-os-artifact.sh)
# The output is in git, so CI does not need these tools. Run it in the nix
# shell after a change to the SVG: nix develop --command scripts/make-icons.sh
set -euo pipefail

cd "$(dirname "$0")/.."
assets=crates/thinwire/assets
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

rsvg-convert -w 1024 -h 1024 "$assets/icon.svg" -o "$work/icon-1024.png"
rsvg-convert -w 256 -h 256 "$assets/icon.svg" -o "$assets/icon-256.png"

# Pillow writes every ICNS size (16 to 1024 px) from the 1024 px image.
python3 -I - "$work/icon-1024.png" "$assets/AppIcon.icns" <<'PY'
import sys
from PIL import Image

Image.open(sys.argv[1]).convert("RGBA").save(sys.argv[2], format="ICNS")
PY

oxipng --quiet --opt max --strip safe "$assets/icon-256.png"
echo "Wrote $assets/icon-256.png and $assets/AppIcon.icns"
