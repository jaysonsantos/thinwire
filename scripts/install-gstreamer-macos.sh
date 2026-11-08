#!/usr/bin/env bash
# Install the official GStreamer framework on a macOS runner (ADR 0015).
# The release build of the app links GStreamer from this framework, and
# scripts/bundle-gstreamer-macos.sh copies a small part of it into
# Thinwire.app. The version and the SHA-256 of each package are pinned.
# The runtime package has the libraries and the plugins. The devel package
# has the pkg-config files that the gstreamer-sys crates read.
set -euo pipefail

version=1.28.8
base="https://gstreamer.freedesktop.org/data/pkg/osx/${version}"
runtime_pkg="gstreamer-1.0-${version}-universal.pkg"
runtime_sha=d371ab04a7632713626836c2fc234e863cc98484c74c473eba99b0fbd9b3a3fe
devel_pkg="gstreamer-1.0-devel-${version}-universal.pkg"
devel_sha=e6d380bc17be7eccf13e0154567d4718fc9321da822408c7ca0cd808b3d4a3f7

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "::error::install-gstreamer-macos.sh runs on macOS only."
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fetch() {
  local name="$1" sha="$2"
  curl -fsSL --retry 3 -o "$work/$name" "$base/$name"
  if ! printf '%s  %s\n' "$sha" "$work/$name" | shasum -a 256 -c - >/dev/null; then
    echo "::error::The SHA-256 of ${name} is not the pinned value."
    exit 1
  fi
}

fetch "$runtime_pkg" "$runtime_sha"
fetch "$devel_pkg" "$devel_sha"
sudo installer -pkg "$work/$runtime_pkg" -target /
sudo installer -pkg "$work/$devel_pkg" -target /

root=/Library/Frameworks/GStreamer.framework/Versions/1.0
if [[ ! -f "$root/lib/libgstreamer-1.0.0.dylib" || ! -f "$root/lib/pkgconfig/gstreamer-1.0.pc" ]]; then
  echo "::error::The GStreamer framework is not complete at ${root}."
  exit 1
fi
if ! command -v pkg-config >/dev/null 2>&1; then
  brew install pkgconf
fi
echo "GStreamer ${version} installed at ${root}"
