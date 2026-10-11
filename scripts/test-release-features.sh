#!/usr/bin/env bash
# Build and test the protocol features that a release ships (ADR 0013
# decision 7). CI runs this inside `nix develop`.
#
# `scripts/test.sh` stays feature-off. This script is the one explicit job
# that turns on every feature that a release ships: `telegram-tdlib`,
# `slack-oauth`, `discord-bot`, `whatsapp-web`, and `signal-local` for the
# app, and the AGPL WhatsApp and Signal helpers with their clients.
# `telegram-tdlib` downloads TDLib here. The Signal client needs `protoc`
# (the nix dev shell has it). No publisher secret is set: pull-request CI
# never gets one.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

# The MIT app with the release protocol features. No AGPL crate is in this
# build: the WhatsApp and Signal clients are behind the helper adapters.
release_features=(telegram-tdlib slack-oauth discord-bot whatsapp-web signal-local)
app_packages=(-p thinwire -p thinwire-core -p thinwire-protocol -p thinwire-ipc)
app_features="$(printf 'thinwire/%s,' "${release_features[@]}")"
app_features="${app_features%,}"
cargo clippy "${app_packages[@]}" --features "$app_features" --all-targets -- -D warnings
cargo test "${app_packages[@]}" --features "$app_features"

# The AGPL helper with its WhatsApp client. Its tests start the real helper
# process. No test starts a pairing, so no test opens a network session.
cargo clippy -p thinwire-whatsapp --features whatsapp-web --all-targets -- -D warnings
cargo test -p thinwire-whatsapp --features whatsapp-web

# The AGPL helper with its Signal client (#248). Its tests start the real
# helper process. No test starts a link, so no test opens a network session.
cargo clippy -p thinwire-signal --features signal-local --all-targets -- -D warnings
cargo test -p thinwire-signal --features signal-local

# ADR 0012 section 4: check the dependencies of each helper against the
# RustSec advisory database. The graph is the helper with its client, as a
# release builds it.
cargo deny --manifest-path crates/thinwire-whatsapp/Cargo.toml --features whatsapp-web check advisories
cargo deny --manifest-path crates/thinwire-signal/Cargo.toml --features signal-local check advisories

# Stage the helpers next to an app binary, as a release does: notices, the
# source offers, the client check, and the check that the app has no AGPL code.
cargo build "${app_packages[@]}" --features "$app_features"
cargo build -p thinwire-whatsapp --features whatsapp-web --bin thinwire-whatsapp-helper
cargo build -p thinwire-signal --features signal-local --bin thinwire-signal-helper
payload="$(mktemp -d)"
trap 'rm -rf "$payload"' EXIT
THINWIRE_WHATSAPP_HELPER_BIN="target/debug/thinwire-whatsapp-helper${THINWIRE_EXE_SUFFIX:-}" \
  THINWIRE_SIGNAL_HELPER_BIN="target/debug/thinwire-signal-helper${THINWIRE_EXE_SUFFIX:-}" \
  REQUIRE_AGPL_MARKER=1 \
  scripts/stage-helper.sh "$payload" "target/debug/thinwire${THINWIRE_EXE_SUFFIX:-}"

scripts/check-release-tree.sh
scripts/check-agpl-deps.sh
