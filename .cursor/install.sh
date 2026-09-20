#!/usr/bin/env bash
# Cloud Agent bootstrap for thinwire (Rust + egui desktop app).
# Idempotent: safe to run repeatedly. Installs the system libraries eframe/winit
# need to build and to run on the headless X display, then warms the build cache.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

# System libraries for building and running the egui/eframe (winit + glow) GUI.
# The Rust toolchain, cmake and pkg-config already ship in the base image; these
# X11/Wayland/OpenGL libraries are the ones the desktop shell links and dlopens.
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  pkg-config \
  cmake \
  build-essential \
  libx11-6 \
  libxcursor1 \
  libxi6 \
  libxrandr2 \
  libxkbcommon0 \
  libxkbcommon-x11-0 \
  libxcb-xkb1 \
  libwayland-client0 \
  libgl1 \
  libglx-mesa0 \
  libgl1-mesa-dri \
  libegl1

# Warm the workspace build so the first agent command is fast and setup is
# verified end to end. Matches the canonical dev command from the README.
cargo build --workspace
