#!/usr/bin/env bash
# Install the official GStreamer framework on a macOS runner (ADR 0015).
#
# Usage: scripts/install-gstreamer-macos.sh <pkg-config folder>
#
# The release build of the app links GStreamer from this framework, and
# scripts/bundle-gstreamer-macos.sh copies a small part of it into
# Thinwire.app. The version and the SHA-256 of the package are pinned.
#
# The runtime package has the libraries and the plugins. The devel package
# (800 MB) has the pkg-config files, but the gstreamer-sys crates need only
# the name and the folder of each library. So this script writes small
# pkg-config files in <pkg-config folder>, and the build reads them through
# PKG_CONFIG_PATH.
set -euo pipefail

pc_dir="${1:?usage: install-gstreamer-macos.sh <pkg-config folder>}"
version=1.28.8
# The GLib version of the GStreamer 1.28.8 framework.
glib_version=2.82.4
base="https://gstreamer.freedesktop.org/data/pkg/osx/${version}"
runtime_pkg="gstreamer-1.0-${version}-universal.pkg"
runtime_sha=d371ab04a7632713626836c2fc234e863cc98484c74c473eba99b0fbd9b3a3fe
root=/Library/Frameworks/GStreamer.framework/Versions/1.0

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "::error::install-gstreamer-macos.sh runs on macOS only."
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The server sometimes ends a transfer early. Continue the same file.
fetch() {
  local name="$1" sha="$2" try
  for try in 1 2 3 4 5 6; do
    if curl -fsSL --http1.1 -C - --retry 3 --retry-all-errors -o "$work/$name" "$base/$name"; then
      break
    fi
    echo "download of ${name} stopped (try ${try}); continuing"
    sleep 5
  done
  if ! printf '%s  %s\n' "$sha" "$work/$name" | shasum -a 256 -c - >/dev/null; then
    echo "::error::The SHA-256 of ${name} is not the pinned value."
    exit 1
  fi
}

fetch "$runtime_pkg" "$runtime_sha"
sudo installer -pkg "$work/$runtime_pkg" -target /

# One pkg-config file for each library that a gstreamer-rs crate links.
write_pc() {
  local name="$1" lib="$2" pc_version="$3"
  if [[ ! -f "$root/lib/lib${lib}.0.dylib" ]]; then
    echo "::error::The GStreamer framework has no lib${lib}.0.dylib."
    exit 1
  fi
  cat >"$pc_dir/${name}.pc" <<EOF
libdir=${root}/lib

Name: ${name}
Description: ${name} of the GStreamer ${version} framework
Version: ${pc_version}
Libs: -L\${libdir} -l${lib}
Cflags:
EOF
}

mkdir -p "$pc_dir"
write_pc glib-2.0 glib-2.0 "$glib_version"
write_pc gobject-2.0 gobject-2.0 "$glib_version"
write_pc gio-2.0 gio-2.0 "$glib_version"
write_pc gstreamer-1.0 gstreamer-1.0 "$version"
write_pc gstreamer-base-1.0 gstbase-1.0 "$version"
write_pc gstreamer-app-1.0 gstapp-1.0 "$version"
write_pc gstreamer-video-1.0 gstvideo-1.0 "$version"

if ! command -v pkg-config >/dev/null 2>&1; then
  brew install pkgconf
fi
PKG_CONFIG_PATH="$pc_dir" pkg-config --libs gstreamer-video-1.0 glib-2.0
echo "GStreamer ${version} installed at ${root}"
