#!/usr/bin/env bash
# ADR 0010: thinwire-core must not pull a GUI toolkit into its build.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

# Default features only, as public CI builds. Never `--all-features`: it turns
# on `telegram-tdlib`, which downloads TDLib (public CI stays feature-off).
# The core features only forward to thinwire-protocol, not to a GUI crate.
tree="$(cargo tree -p thinwire-core --edges normal,build --prefix none)"
if grep -E '^(egui|eframe|winit|epaint|egui-winit|egui_glow)[ @]' <<<"$tree"; then
  echo "thinwire-core depends on a GUI toolkit crate (ADR 0010)" >&2
  exit 1
fi
