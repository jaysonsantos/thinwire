#!/usr/bin/env bash
# Stage the WhatsApp helper program into a payload folder (ADR 0013).
#
#   scripts/stage-helper.sh <payload-dir> <app-binary>
#
# It copies the helper next to the app binary, with the AGPL-3.0 text, the
# offer of the helper source, and the notices of the protocol code. It
# refuses a helper with no client, and an app binary that has AGPL code.
# scripts/stage-os-artifact.sh calls it for each OS archive. Pull-request CI
# calls it on debug builds, so each OS proves it before main.
#
# Environment:
#   THINWIRE_WHATSAPP_HELPER_BIN  the helper binary (default: target/release)
#   SOURCE_REF                    commit or tag of this build (default: HEAD)
#   REQUIRE_AGPL_MARKER=1         fail if the helper shows no AGPL marker
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")" || exit 1

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/stage-helper.sh <payload-dir> <app-binary>" >&2
  exit 2
fi
payload="$1"
app_binary="$2"
helper_name=thinwire-whatsapp-helper

if [[ ! -f "$app_binary" ]]; then
  echo "::error::Missing the app binary at ${app_binary}."
  exit 1
fi

# region: notices

# Notices of the protocol code in the payload (ADR 0013 decision 8).
# Source file, then its name in THIRD_PARTY_NOTICES.
notices="$payload/THIRD_PARTY_NOTICES"
mkdir -p "$notices"
protocol_notices=(
  "crates/thinwire-whatsapp/LICENSE:${helper_name}-AGPL-3.0.txt"
  "third_party/whatsapp-rust/LICENSE:whatsapp-rust-LICENSE-MIT.txt"
  "third_party/tdlib-rs/LICENSE-APACHE:tdlib-rs-LICENSE-APACHE.txt"
  "third_party/tdlib-rs/LICENSE-MIT:tdlib-rs-LICENSE-MIT.txt"
  "third_party/twilight/LICENSE.md:twilight-LICENSE-ISC.md"
)
for entry in "${protocol_notices[@]}"; do
  notice_source="${entry%%:*}"
  if [[ ! -f "$notice_source" ]]; then
    echo "::error::Missing notice at ${notice_source}."
    exit 1
  fi
  cp "$notice_source" "${notices}/${entry##*:}"
done

# AGPL-3.0 section 6(d): say where the Corresponding Source of the helper
# is. SOURCE_REF is the commit or the tag of this build.
source_ref="${SOURCE_REF:-$(git rev-parse HEAD)}"
if [[ ! "$source_ref" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "::error::SOURCE_REF must be a commit or a tag name. Got: ${source_ref}"
  exit 1
fi
cat >"${notices}/${helper_name}-SOURCE.md" <<SOURCE_OFFER
# Source of ${helper_name}

${helper_name} is a separate program. Its license is AGPL-3.0-only. The
license text is ${helper_name}-AGPL-3.0.txt in this folder. The thinwire app
is MIT. The app starts the helper as a child process and talks to it over a
pipe. The app does not link the helper.

The Corresponding Source of this helper is the file
thinwire-helpers-${source_ref}-source.tar.gz. Get it from the same place as
this archive: the same GitHub Actions run, or the same release. It holds the
thinwire source of ${source_ref} and every dependency of the helper from
cargo vendor, also whatsapp-rust and wacore-libsignal.

The same source is at
https://github.com/jaysonsantos/thinwire/tree/${source_ref}

To build the helper from the tarball, read HELPER-SOURCE.md in it.
SOURCE_OFFER

# endregion: notices

# region: helper binary

if [[ -n "${THINWIRE_WHATSAPP_HELPER_BIN:-}" ]]; then
  helper_source="$THINWIRE_WHATSAPP_HELPER_BIN"
elif [[ -f "target/release/${helper_name}.exe" ]]; then
  helper_source="target/release/${helper_name}.exe"
else
  helper_source="target/release/${helper_name}"
fi
if [[ ! -f "$helper_source" ]]; then
  echo "::error::Missing the WhatsApp helper at ${helper_source}. Build it with: cargo build --release -p thinwire-whatsapp --features whatsapp-web --bin ${helper_name}"
  exit 1
fi
helper="${payload}/$(basename "$helper_source")"
cp "$helper_source" "$helper"
chmod 755 "$helper"
# A helper that was built without whatsapp-web has no client. Refuse it.
helper_version="$("$helper" --version)"
if [[ "$helper_version" != *"(whatsapp-web)"* ]]; then
  echo "::error::The WhatsApp helper has no client (${helper_version}). Build it with --features whatsapp-web."
  exit 1
fi

# endregion: helper binary

# region: binary check

# ADR 0013: the MIT app binary links no AGPL code. check-release-tree.sh is
# the exact check, on the cargo tree. This one reads the binaries: source
# paths and symbol names of wacore-libsignal must not be in the app. The
# helper has them, so the pattern is known to match.
agpl_marker='wacore[/\\_]libsignal'
if grep -a -q -E "$agpl_marker" "$app_binary"; then
  echo "::error::The app binary ${app_binary} contains wacore-libsignal code. The MIT app must not link AGPL code (ADR 0013)."
  exit 1
fi
if ! grep -a -q -E "$agpl_marker" "$helper"; then
  if [[ "${REQUIRE_AGPL_MARKER:-0}" == 1 ]]; then
    echo "::error::The helper binary ${helper} shows no wacore-libsignal marker, so the binary check of the app proves nothing."
    exit 1
  fi
  echo "::warning::The helper binary ${helper} shows no wacore-libsignal marker, so the binary check of the app proves nothing on this OS. The cargo tree check still holds."
fi

# endregion: binary check

echo "Staged ${helper} (${helper_version}) with its notices in ${notices}."
