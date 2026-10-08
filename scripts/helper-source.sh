#!/usr/bin/env bash
# Build the Corresponding Source tarball of the helper programs (ADR 0013
# decision 8, AGPL-3.0 sections 1 and 6(d)).
#
#   scripts/helper-source.sh <ref>     # a commit or a tag name
#
# It writes thinwire-helpers-<ref>-source.tar.gz in the repository root. The
# tarball holds the thinwire source of HEAD and every dependency from
# `cargo vendor`, also whatsapp-rust and wacore-libsignal. A build from it
# needs no network and no outside git revision. Publish it next to the
# binaries, and keep it as long as they stay.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(dirname "$script_dir")"
cd "$repo_dir"

ref="${1:-}"
if [[ ! "$ref" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "usage: scripts/helper-source.sh <ref>   (a commit or a tag name)" >&2
  exit 2
fi

name="thinwire-helpers-${ref}-source"
archive="${repo_dir}/${name}.tar.gz"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tree="${work}/${name}"

# The tracked files of HEAD only: no build output, no local file.
mkdir -p "$tree"
git archive --format=tar HEAD | tar -x -C "$tree"

# Every dependency in Cargo.lock, for every OS. `cargo vendor` prints the
# source replacement for `.cargo/config.toml`.
vendor_config="${work}/vendor-config.toml"
(
  cd "$tree"
  cargo vendor --locked --versioned-dirs vendor >"$vendor_config"
)
{
  echo
  echo "# Added by scripts/helper-source.sh: build from the vendor folder."
  cat "$vendor_config"
} >>"${tree}/.cargo/config.toml"

cat >"${tree}/HELPER-SOURCE.md" <<SOURCE_README
# Source of the thinwire helper programs

This tarball is the Corresponding Source of the helper programs of thinwire
at ${ref}. The helper programs are AGPL-3.0-only. The license text is in
crates/thinwire-whatsapp/LICENSE.

It holds:

- The thinwire source of ${ref}. The helper crate is crates/thinwire-whatsapp.
- Every dependency in vendor/, from cargo vendor. This includes whatsapp-rust
  and wacore-libsignal.
- .cargo/config.toml, which points cargo at vendor/.

Build the WhatsApp helper with the Rust toolchain of rust-toolchain.toml:

    cargo build --release --offline -p thinwire-whatsapp \\
      --features whatsapp-web --bin thinwire-whatsapp-helper

The program is target/release/thinwire-whatsapp-helper. Put it next to the
thinwire binary (Contents/MacOS in Thinwire.app), or name its path in the
thinwire settings file with the key whatsapp_helper.
SOURCE_README

rm -f "$archive"
tar -czf "$archive" -C "$work" "$name"

# The tarball must have the helper crate and the AGPL dependency source.
listing="$(tar -tzf "$archive")"
for needed in \
  "${name}/crates/thinwire-whatsapp/LICENSE" \
  "${name}/crates/thinwire-whatsapp/src/bin/helper.rs" \
  "${name}/HELPER-SOURCE.md" \
  "${name}/Cargo.lock"; do
  if ! grep -qxF "$needed" <<<"$listing"; then
    echo "the source tarball is missing ${needed}" >&2
    exit 1
  fi
done
for vendored in wacore-libsignal whatsapp-rust; do
  if ! grep -qE "^${name}/vendor/${vendored}-[^/]+/Cargo\.toml$" <<<"$listing"; then
    echo "the source tarball is missing the vendored ${vendored} crate" >&2
    exit 1
  fi
done

echo "Source tarball: ${archive}"
