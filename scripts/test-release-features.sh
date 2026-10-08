#!/usr/bin/env bash
# Build and test the protocol features that a release ships (ADR 0013
# decision 7). CI runs this inside `nix develop`.
#
# `scripts/test.sh` stays feature-off. This script is the one explicit job
# that turns on every feature that a release ships: `telegram-tdlib`,
# `slack-oauth`, `discord-bot`, and `whatsapp-web` for the app, and the AGPL
# WhatsApp helper with its client. `telegram-tdlib` downloads TDLib here. No
# publisher secret is set: pull-request CI never gets one.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

# The MIT app with the release protocol features. No AGPL crate is in this
# build: the WhatsApp client is behind the helper adapter.
release_features=(telegram-tdlib slack-oauth discord-bot whatsapp-web)
app_packages=(-p thinwire -p thinwire-core -p thinwire-protocol -p thinwire-ipc)
app_features="$(printf 'thinwire/%s,' "${release_features[@]}")"
app_features="${app_features%,}"
cargo clippy "${app_packages[@]}" --features "$app_features" --all-targets -- -D warnings
cargo test "${app_packages[@]}" --features "$app_features"

# The AGPL helper with its WhatsApp client. Its tests start the real helper
# process. No test starts a pairing, so no test opens a network session.
cargo clippy -p thinwire-whatsapp --features whatsapp-web --all-targets -- -D warnings
cargo test -p thinwire-whatsapp --features whatsapp-web

# Stage the helper next to an app binary, as a release does: notices, the
# source offer, the client check, and the check that the app has no AGPL code.
cargo build "${app_packages[@]}" --features "$app_features"
cargo build -p thinwire-whatsapp --features whatsapp-web --bin thinwire-whatsapp-helper
payload="$(mktemp -d)"
trap 'rm -rf "$payload"' EXIT
THINWIRE_WHATSAPP_HELPER_BIN="target/debug/thinwire-whatsapp-helper${THINWIRE_EXE_SUFFIX:-}" \
  REQUIRE_AGPL_MARKER=1 \
  scripts/stage-helper.sh "$payload" "target/debug/thinwire${THINWIRE_EXE_SUFFIX:-}"

scripts/check-release-tree.sh
scripts/check-agpl-deps.sh
