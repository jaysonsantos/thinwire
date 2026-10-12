#!/usr/bin/env bash
# Install the protobuf compiler `protoc` on a CI runner (#248).
#
#   scripts/install-protoc.sh
#
# The Signal helper needs it: presage and libsignal compile `.proto` files
# when they build. The nix dev shell has `protoc` from flake.nix, so this
# script is for the jobs that do not use nix. Each OS gets the same pinned
# release from the protobuf project, and the script checks its SHA-256.
#
# In GitHub Actions the next steps find `protoc` on the PATH, and `PROTOC`
# names it. Elsewhere the script prints the folder.
#
# Environment:
#   PROTOC_DIR  where to put the release (default: a temp folder)
set -euo pipefail

version=36.1
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)
    asset=linux-x86_64
    sha256=c4bc672d9d49214dc8cafdceadf4df92182d6ca8e3ec65a56b2d7de5602669b4
    ;;
  Darwin-arm64)
    asset=osx-aarch_64
    sha256=de56d57afe30c5d191b11d24ff93dd4025728d7fb43b773886b2d3613e0bdbb2
    ;;
  MINGW*-x86_64 | MSYS*-x86_64 | CYGWIN*-x86_64)
    asset=win64
    sha256=390e515cb456e6a978553bdb57baf087b054885077fd6da7f7ff0160279c07d6
    ;;
  *)
    echo "::error::No pinned protoc release for $(uname -s) $(uname -m)."
    exit 1
    ;;
esac

# Git Bash on Windows: use its path form for the tools of this script, and
# the Windows form for the next steps.
unix_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -u "$1"
  else
    printf '%s\n' "$1"
  fi
}
native_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s\n' "$1"
  fi
}

base="$(unix_path "${RUNNER_TEMP:-${TMPDIR:-/tmp}}")"
dest="${PROTOC_DIR:-${base}/protoc-${version}}"
archive="${dest}.zip"
url="https://github.com/protocolbuffers/protobuf/releases/download/v${version}/protoc-${version}-${asset}.zip"

mkdir -p "$(dirname "$dest")"
curl --fail --silent --show-error --location --retry 3 --output "$archive" "$url"
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$archive" | cut -d' ' -f1)"
else
  got="$(shasum -a 256 "$archive" | cut -d' ' -f1)"
fi
if [[ "$got" != "$sha256" ]]; then
  echo "::error::The protoc archive has SHA-256 ${got}. The pinned value is ${sha256}."
  exit 1
fi

rm -rf "$dest"
mkdir -p "$dest"
unzip -q "$archive" -d "$dest"
rm -f "$archive"
if [[ -f "$dest/bin/protoc.exe" ]]; then
  protoc="$dest/bin/protoc.exe"
else
  protoc="$dest/bin/protoc"
fi
chmod 755 "$protoc"
"$protoc" --version

if [[ -n "${GITHUB_PATH:-}" && -n "${GITHUB_ENV:-}" ]]; then
  native_path "$dest/bin" >>"$GITHUB_PATH"
  echo "PROTOC=$(native_path "$protoc")" >>"$GITHUB_ENV"
else
  echo "protoc is in ${dest}/bin. Add that folder to the PATH."
fi
