#!/usr/bin/env bash
# ADR 0010: thinwire-core must not pull a GUI toolkit into its build.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

tree="$(cargo tree -p thinwire-core --edges normal,build --prefix none --all-features)"
if grep -E '^(egui|eframe|winit|epaint|egui-winit|egui_glow)[ @]' <<<"$tree"; then
  echo "thinwire-core depends on a GUI toolkit crate (ADR 0010)" >&2
  exit 1
fi
