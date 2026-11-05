#!/usr/bin/env bash
# Local build with every protocol on. On macOS it also reads the Keychain
# through the `security` command. On Linux the secrets go through the native
# keyring, as in a release. Run it from a checkout, inside `nix develop`:
#
#   nix develop --command scripts/local-build.sh             # debug build
#   nix develop --command scripts/local-build.sh --release   # extra arguments go to cargo build
#
# It builds the app and both helper programs, each in its own cargo run, as
# `os-zips.yml` does for a release. The app finds a helper beside its own
# binary, and all three land in `target/<profile>/`. On macOS each run turns on
# the Keychain feature of `thinwire-core`, so the helpers read their store
# keys through `security` too.
#
# Do not merge the three into one run. Cargo unifies features across one run,
# so `telegram-tdlib` would reach the helpers: they would link TDLib, and the
# linker would print a duplicate-symbol warning for `tdlib_rs` against
# `openssl_sys`.
#
# Why: the native Keychain API asks for permission for every new binary, and a
# local build is a new binary every time. Apple signs /usr/bin/security and its
# path never changes, so the Keychain item trusts it and asks once. The first
# read of an item that an older build made can still ask once.
#
# LOCAL ONLY. Feature `keychain-security-cli` must never reach CI or a
# release: `crates/thinwire-core/build.rs` fails a build under CI, and
# `scripts/check-local-only-features.sh` keeps it out of the workflows. This
# script refuses to run under CI as well.
#
# `telegram-tdlib` downloads TDLib. The Telegram `api_id` and `api_hash` come
# from TELEGRAM_API_ID and TELEGRAM_API_HASH at build time (ADR 0007). Without
# them the app shows "credentials missing" and takes an Advanced override.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")"

die() {
  echo "local-build.sh: $*" >&2
  exit 1
}

# Same rule as build.rs: unset, empty, 0 and false mean "not in CI".
under_ci() {
  local value="${!1:-}"
  [[ -n "$value" && "$value" != "0" && "$value" != "false" ]]
}

if under_ci CI || under_ci GITHUB_ACTIONS; then
  die "local builds only: do not run this in CI or for a release"
fi

case "$(uname -s)" in
  Darwin)
    # The app forwards this feature to `thinwire-core`. A helper has no such
    # feature of its own, so it names the one of `thinwire-core` directly.
    app_features="all-protocols,keychain-security-cli"
    helper_features=",thinwire-core/keychain-security-cli"
    store="the security command store"
    ;;
  Linux)
    # `keychain-security-cli` reads the macOS Keychain. Linux keeps the
    # native keyring (Secret Service, or the kernel keyring).
    app_features="all-protocols"
    helper_features=""
    store="the native keyring"
    ;;
  *)
    die "macOS and Linux only"
    ;;
esac

if [[ -z "${TELEGRAM_API_ID:-}" || -z "${TELEGRAM_API_HASH:-}" ]]; then
  echo "note: TELEGRAM_API_ID or TELEGRAM_API_HASH is not set." >&2
  echo "      The app will show 'credentials missing' until you set an Advanced override." >&2
fi

cargo build -p thinwire \
  --features "$app_features" \
  --bin thinwire "$@"
cargo build -p thinwire-whatsapp \
  --features "whatsapp-web$helper_features" \
  --bin thinwire-whatsapp-helper "$@"
cargo build -p thinwire-signal \
  --features "signal-local$helper_features" \
  --bin thinwire-signal-helper "$@"

profile="debug"
for arg in "$@"; do
  [[ "$arg" == "--release" ]] && profile="release"
done
run="target/${profile}/thinwire"

# macOS: a bare binary has no bundle, so the notification center refuses
# it and a notification cannot open its chat. A small Thinwire.app with
# the bundle id of a release fixes that. The Keychain store reads through
# `security`, so the new path asks for no permission.
if [[ "$(uname -s)" == "Darwin" ]]; then
  out="target/${profile}"
  app="${out}/Thinwire.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS"
  for program in thinwire thinwire-whatsapp-helper thinwire-signal-helper; do
    # `/bin/cp -c` clones the file on APFS: no second copy of the bytes.
    # The `cp` of the Nix shell is GNU cp, which has no `-c`.
    /bin/cp -c "${out}/${program}" "$app/Contents/MacOS/${program}" 2>/dev/null \
      || cp "${out}/${program}" "$app/Contents/MacOS/${program}"
  done
  version="$(sed -nE '/^\[workspace.package\]/,/^\[/s/^version = "([^"]+)"/\1/p' Cargo.toml)"
  cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>thinwire</string>
  <key>CFBundleIdentifier</key>
  <string>dev.jaysonsantos.thinwire</string>
  <key>CFBundleName</key>
  <string>Thinwire</string>
  <key>CFBundleDisplayName</key>
  <string>Thinwire</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version:-0.0.0}</string>
  <key>CFBundleVersion</key>
  <string>${version:-0.0.0}</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
EOF
  # Ad-hoc signatures: the helpers first, then the app around them.
  codesign --force --sign - "$app/Contents/MacOS/thinwire-whatsapp-helper"
  codesign --force --sign - "$app/Contents/MacOS/thinwire-signal-helper"
  codesign --force --sign - "$app"
  run="${app}/Contents/MacOS/thinwire"
fi

echo
echo "Built with every protocol and $store."
echo "Run it with: ${run}"
echo "The helpers are beside it, where the app looks for them."
echo "(A custom --profile builds into target/<profile>/.)"
