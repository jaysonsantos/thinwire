#!/usr/bin/env bash
# Copy a small GStreamer runtime into Thinwire.app (ADR 0015).
#
# Usage: scripts/bundle-gstreamer-macos.sh <Thinwire.app> [<GStreamer root>]
#
# The app links GStreamer when the `video` feature is on. This script does
# nothing when Contents/MacOS/thinwire does not link libgstreamer, so a build
# with no `video` feature stages as before.
#
# The runtime goes to Contents/Frameworks/GStreamer: the libraries in lib/,
# the plugins in lib/gstreamer-1.0/. The app scans that plugin folder at
# start (crates/thinwire/src/app/video/player.rs). Only the plugins in
# `plugins` below are copied, with the libraries that they need. All of them
# are LGPL GStreamer parts or permissive libraries. No plugin of
# gst-plugins-ugly, no GPL plugin, and no x264 is copied. Each library is
# thinned to the architecture of this machine. The caller signs every Mach-O
# file after this script.
set -euo pipefail

app="${1:?usage: bundle-gstreamer-macos.sh <Thinwire.app> [<GStreamer root>]}"
gst_root="${2:-/Library/Frameworks/GStreamer.framework/Versions/1.0}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
binary="$app/Contents/MacOS/thinwire"
dest="$app/Contents/Frameworks/GStreamer"
rpath="@executable_path/../Frameworks/GStreamer/lib"

# playbin with an appsink, MP4 and WebM, H.264, H.265 and AAC through
# VideoToolbox and AudioToolbox (applemedia), Opus, VP8 and VP9.
plugins=(
  coreelements
  playback
  typefindfunctions
  pbtypes
  app
  videoconvertscale
  audioconvert
  audioresample
  volume
  autodetect
  osxaudio
  isomp4
  matroska
  applemedia
  videoparsersbad
  audioparsers
  opus
  vpx
)

if ! otool -L "$binary" | grep -q '@rpath/libgstreamer-1.0'; then
  echo "GStreamer: the app does not link it. Nothing to bundle."
  exit 0
fi
if [[ ! -f "$gst_root/lib/libgstreamer-1.0.0.dylib" ]]; then
  echo "::error::The app links GStreamer, but ${gst_root} has no libgstreamer."
  exit 1
fi

arch="$(uname -m)"
mkdir -p "$dest/lib/gstreamer-1.0"

# The @rpath/ names that a Mach-O file links, one for each line.
rpath_deps() {
  otool -L "$1" | awk 'NR > 1 && $1 ~ /^@rpath\// { sub("^@rpath/", "", $1); print $1 }'
}

# The LC_RPATH entries of a Mach-O file, one for each line.
rpaths_of() {
  otool -l "$1" | awk '$1 == "cmd" && $2 == "LC_RPATH" { want = 1; next } want && $1 == "path" { print $2; want = 0 }'
}

# Copy one library or plugin, thinned to this architecture.
copy_thin() {
  local src="$1" dst="$2"
  if lipo "$src" -verify_arch "$arch" 2>/dev/null && [[ "$(lipo -archs "$src")" != "$arch" ]]; then
    lipo "$src" -thin "$arch" -output "$dst"
  else
    cp "$src" "$dst"
  fi
  chmod 644 "$dst"
}

# The copied paths, one for each line. Bash 3 on macOS has no associative
# arrays.
copied=""
was_copied() {
  grep -qxF "$1" <<<"$copied"
}

queue=()
for plugin in "${plugins[@]}"; do
  queue+=("gstreamer-1.0/libgst${plugin}.dylib")
done
# The GStreamer and GLib libraries that the app links itself.
while IFS= read -r dep; do
  if [[ -f "$gst_root/lib/$dep" ]]; then
    queue+=("$dep")
  fi
done < <(rpath_deps "$binary")

count=0
while ((${#queue[@]} > 0)); do
  rel="${queue[0]}"
  queue=("${queue[@]:1}")
  if was_copied "$rel"; then
    continue
  fi
  src="$gst_root/lib/$rel"
  if [[ ! -f "$src" ]]; then
    echo "::error::GStreamer file ${rel} is missing in ${gst_root}/lib."
    exit 1
  fi
  copy_thin "$src" "$dest/lib/$rel"
  copied+="${rel}"$'\n'
  count=$((count + 1))
  while IFS= read -r dep; do
    # A plugin names itself as @rpath/<file>.
    if [[ "$rel" == "gstreamer-1.0/${dep}" ]]; then
      continue
    fi
    if ! was_copied "$dep"; then
      queue+=("$dep")
    fi
  done < <(rpath_deps "$dest/lib/$rel")
done

# The app finds the libraries in the bundle only. Remove a build path.
while IFS= read -r entry; do
  if [[ "$entry" != @* ]]; then
    install_name_tool -delete_rpath "$entry" "$binary"
  fi
done < <(rpaths_of "$binary")
if ! rpaths_of "$binary" | grep -qxF "$rpath"; then
  install_name_tool -add_rpath "$rpath" "$binary"
fi

# Each copied file links only the bundle and the system.
status=0
while IFS= read -r -d '' file; do
  while IFS= read -r entry; do
    if [[ "$entry" != @* ]]; then
      install_name_tool -delete_rpath "$entry" "$file"
    fi
  done < <(rpaths_of "$file")
  while IFS= read -r line; do
    case "$line" in
      @rpath/* | /usr/lib/* | /System/*) ;;
      *)
        echo "::error::${file#"$app"/} links ${line}, which is not in the bundle or the system."
        status=1
        ;;
    esac
  done < <(otool -L "$file" | awk 'NR > 1 { print $1 }')
  while IFS= read -r dep; do
    if [[ ! -f "$dest/lib/$dep" && ! -f "$dest/lib/gstreamer-1.0/$dep" ]]; then
      echo "::error::${file#"$app"/} links @rpath/${dep}, which is not in the bundle."
      status=1
    fi
  done < <(rpath_deps "$file")
done < <(find "$dest" -type f -name '*.dylib' -print0)
if [[ "$status" -ne 0 ]]; then
  exit 1
fi

notices="$app/Contents/Resources/THIRD_PARTY_NOTICES"
mkdir -p "$notices"
for notice in "$repo_root"/third_party/gstreamer/*; do
  cp "$notice" "$notices/gstreamer-$(basename "$notice")"
done

echo "GStreamer: bundled ${count} files ($(du -sh "$dest" | cut -f1)) from ${gst_root}"
