#!/usr/bin/env bash
# Stage the helper programs into a payload folder (ADR 0013): the WhatsApp
# helper (#246) and the Signal helper (#248).
#
#   scripts/stage-helper.sh <payload-dir> <app-binary>
#
# It copies each helper next to the app binary, with the AGPL-3.0 text, the
# offer of the helper source, and the notices of the protocol code. It
# refuses a helper with no client, and an app binary that has AGPL code.
# scripts/stage-os-artifact.sh calls it for each OS archive. Pull-request CI
# calls it on debug builds, so each OS proves it before main.
#
# Environment:
#   THINWIRE_WHATSAPP_HELPER_BIN  the WhatsApp helper (default: target/release)
#   THINWIRE_SIGNAL_HELPER_BIN    the Signal helper (default: target/release)
#   SOURCE_REF                    commit or tag of this build (default: HEAD)
#   REQUIRE_AGPL_MARKER=1         fail if a helper shows no AGPL marker
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")" || exit 1

if [[ $# -ne 2 ]]; then
  echo "usage: scripts/stage-helper.sh <payload-dir> <app-binary>" >&2
  exit 2
fi
payload="$1"
app_binary="$2"

if [[ ! -f "$app_binary" ]]; then
  echo "::error::Missing the app binary at ${app_binary}."
  exit 1
fi

# AGPL-3.0 section 6(d): each source offer says where the Corresponding
# Source is. SOURCE_REF is the commit or the tag of this build.
source_ref="${SOURCE_REF:-$(git rev-parse HEAD)}"
if [[ ! "$source_ref" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "::error::SOURCE_REF must be a commit or a tag name. Got: ${source_ref}"
  exit 1
fi

notices="$payload/THIRD_PARTY_NOTICES"
mkdir -p "$notices"

copy_notice() {
  local notice_source="$1" notice_name="$2"
  if [[ ! -f "$notice_source" ]]; then
    echo "::error::Missing notice at ${notice_source}."
    exit 1
  fi
  cp "$notice_source" "${notices}/${notice_name}"
}

# Notices of the MIT protocol code in the payload (ADR 0013 decision 8).
copy_notice third_party/whatsapp-rust/LICENSE whatsapp-rust-LICENSE-MIT.txt
copy_notice third_party/tdlib-rs/LICENSE-APACHE tdlib-rs-LICENSE-APACHE.txt
copy_notice third_party/tdlib-rs/LICENSE-MIT tdlib-rs-LICENSE-MIT.txt
copy_notice third_party/twilight/LICENSE.md twilight-LICENSE-ISC.md

# Stage one helper program.
#
#   $1  protocol name, for messages
#   $2  file name of the helper, without `.exe`
#   $3  crate folder of the helper
#   $4  cargo feature that links the client
#   $5  path of the helper binary, or empty for target/release
#   $6  AGPL code in the helper, in words, for the source offer
#   $7  pattern of the AGPL code in a binary: source paths and symbol names.
#       It must not match the text of a screen of the app.
stage_one() {
  local protocol="$1" helper_name="$2" crate="$3" feature="$4" helper_source="$5"
  local agpl_code="$6" agpl_marker="$7" helper helper_version

  # region: notices

  copy_notice "crates/${crate}/LICENSE" "${helper_name}-AGPL-3.0.txt"
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
cargo vendor, also ${agpl_code}.

The same source is at
https://github.com/jaysonsantos/thinwire/tree/${source_ref}

To build the helper from the tarball, read HELPER-SOURCE.md in it.
SOURCE_OFFER

  # endregion: notices

  # region: helper binary

  if [[ -z "$helper_source" ]]; then
    if [[ -f "target/release/${helper_name}.exe" ]]; then
      helper_source="target/release/${helper_name}.exe"
    else
      helper_source="target/release/${helper_name}"
    fi
  fi
  if [[ ! -f "$helper_source" ]]; then
    echo "::error::Missing the ${protocol} helper at ${helper_source}. Build it with: cargo build --release -p ${crate} --features ${feature} --bin ${helper_name}"
    exit 1
  fi
  helper="${payload}/$(basename "$helper_source")"
  cp "$helper_source" "$helper"
  chmod 755 "$helper"
  # A helper that was built without its client feature has no client.
  # Refuse it.
  helper_version="$("$helper" --version)"
  if [[ "$helper_version" != *"(${feature})"* ]]; then
    echo "::error::The ${protocol} helper has no client (${helper_version}). Build it with --features ${feature}."
    exit 1
  fi

  # endregion: helper binary

  # region: binary check

  # ADR 0013: the MIT app binary links no AGPL code. check-release-tree.sh
  # is the exact check, on the cargo tree. This one reads the binaries:
  # source paths and symbol names of the AGPL code must not be in the app.
  # The helper has them, so the pattern is known to match.
  if grep -a -q -E "$agpl_marker" "$app_binary"; then
    echo "::error::The app binary ${app_binary} contains code of ${agpl_code}. The MIT app must not link AGPL code (ADR 0013)."
    exit 1
  fi
  if ! grep -a -q -E "$agpl_marker" "$helper"; then
    if [[ "${REQUIRE_AGPL_MARKER:-0}" == 1 ]]; then
      echo "::error::The helper binary ${helper} shows no marker of ${agpl_code}, so the binary check of the app proves nothing."
      exit 1
    fi
    echo "::warning::The helper binary ${helper} shows no marker of ${agpl_code}, so the binary check of the app proves nothing on this OS. The cargo tree check still holds."
  fi

  # endregion: binary check

  echo "Staged ${helper} (${helper_version}) with its notices in ${notices}."
}

stage_one WhatsApp thinwire-whatsapp-helper thinwire-whatsapp whatsapp-web \
  "${THINWIRE_WHATSAPP_HELPER_BIN:-}" \
  "whatsapp-rust and wacore-libsignal" \
  'wacore[/\\_]libsignal'

# The Signal screens of the app say "presage" and "libsignal" in plain
# words. The pattern takes only source paths and crate names with `_`.
stage_one Signal thinwire-signal-helper thinwire-signal signal-local \
  "${THINWIRE_SIGNAL_HELPER_BIN:-}" \
  "presage and libsignal" \
  'presage[/\\]src|presage_store_sqlite|libsignal_(protocol|service|core)'
