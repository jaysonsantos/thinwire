#!/usr/bin/env bash
# L8 spike (#161), branch only. Run once on a Mac:
#
#   ./scripts/macos-l8.sh
#
# Part 1: UNUserNotificationCenter inside an ad-hoc signed Thinwire.app.
# Part 2: keychain. Build A (ad-hoc signed) saves a test secret. Build B is a
# rebuild with a different binary hash. B reads the secret: does macOS ask?
#
# Everything goes to /tmp/probe.log. The script enters `nix develop` itself
# when nix is present (L8_NO_NIX=1 skips that). CI=true or no terminal skips
# the questions and the click wait.
set -euo pipefail

LOG=/tmp/probe.log
NOTIFY_LOG=/tmp/probe-notify.log
SERVICE=dev.jaysonsantos.thinwire.l8-probe
BUNDLE_ID=dev.jaysonsantos.thinwire

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

# --- enter the nix dev shell once -------------------------------------------
if [[ -z ${L8_STARTED:-} ]]; then
  : >"$LOG"
  export L8_STARTED=1
  if [[ -z ${IN_NIX_SHELL:-} && -z ${L8_NO_NIX:-} ]] && command -v nix >/dev/null 2>&1; then
    nix_cmd=(nix --extra-experimental-features 'nix-command flakes' develop "$ROOT" --command)
    echo ">>> Preparing the nix dev shell (the first run downloads the Rust toolchain)..."
    if "${nix_cmd[@]}" true; then
      exec "${nix_cmd[@]}" "$ROOT/scripts/macos-l8.sh" "$@"
    fi
    echo "STEP nix-develop: err, falling back to cargo on PATH" | tee -a "$LOG"
  fi
fi

INTERACTIVE=1
if [[ ${CI:-} == true || ! -t 0 ]]; then
  INTERACTIVE=0
fi

log() { printf '%s\n' "$*" | tee -a "$LOG"; }
step() { log "STEP $1: $2"; }

# Instructions for the human. Also in the log, so the order is clear.
say() {
  printf '\n' | tee -a "$LOG"
  local line
  for line in "$@"; do
    printf '>>> %s\n' "$line" | tee -a "$LOG"
  done
}

# Run a command, copy its stdout and stderr to the terminal and the log.
# Returns the command's exit status.
logged() {
  local status=0
  "$@" 2>&1 | tee -a "$LOG" || status=${PIPESTATUS[0]}
  return "$status"
}

pause() {
  if [[ $INTERACTIVE == 1 ]]; then
    read -r -p ">>> Press Enter to start. " _ </dev/tty || true
  fi
}

# Cargo output is long: keep it out of the log unless the build fails.
build() {
  local name=$1
  shift
  local out=/tmp/probe-build-$name.log
  echo ">>> Building $name (output in $out)..."
  if "$@" >"$out" 2>&1; then
    step "build-$name" ok
    return 0
  fi
  step "build-$name" "err (last lines follow)"
  tail -n 40 "$out" | tee -a "$LOG"
  return 1
}

# make_bundle <dir> <executable>: Thinwire.app with the real bundle id, signed
# ad hoc like the release would be. Prints nothing; logs the signature.
make_bundle() {
  local app=$1/Thinwire.app
  rm -rf "$1"
  mkdir -p "$app/Contents/MacOS"
  cp "$2" "$app/Contents/MacOS/thinwire"
  cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>thinwire</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleName</key>
  <string>Thinwire</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>0.0.0</string>
  <key>CFBundleVersion</key>
  <string>0.0.0</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
</dict>
</plist>
PLIST
  logged /usr/bin/codesign --force --sign - "$app" || return 1
  logged /usr/bin/codesign --verify --verbose=2 "$app" || true
}

cdhash() {
  /usr/bin/codesign -dvvv "$1" 2>&1 | sed -n 's/^CDHash=//p'
}

# --- header -----------------------------------------------------------------
log "== thinwire L8 probe $(date -u +%Y-%m-%dT%H:%M:%SZ)"
logged /usr/bin/sw_vers || true
log "arch: $(uname -m)"
log "commit: $(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
log "rustc: $(rustc -V 2>&1 || echo missing)"
log "cargo: $(command -v cargo || echo missing)"
log "nix shell: ${IN_NIX_SHELL:-no}, interactive: $INTERACTIVE"

WORK=$ROOT/target/l8
mkdir -p "$WORK"

# --- part 1: notifications --------------------------------------------------
log ""
log "== Part 1: notifications (ad-hoc signed Thinwire.app through open)"
WAIT_CLICK=60
if [[ $INTERACTIVE == 0 ]]; then
  WAIT_CLICK=0
fi
if build notify cargo build -p thinwire-notify --example macos_probe --locked &&
  make_bundle "$WORK/notify" target/debug/examples/macos_probe; then
  say "Part 1: notifications." \
    "1. If macOS asks whether Thinwire may send notifications, click Allow." \
    "2. Watch the 'thinwire probe' notification: it shows, changes to a" \
    "   quiet second text, then goes away." \
    "3. A last notification says 'click this notification'. Click it within 60 s." \
    "   (This part takes up to about 2 minutes.)"
  pause
  : >"$NOTIFY_LOG"
  status=0
  /usr/bin/open -n -W --env "PROBE_WAIT_CLICK=$WAIT_CLICK" \
    --stdout "$NOTIFY_LOG" --stderr "$NOTIFY_LOG" "$WORK/notify/Thinwire.app" 2>&1 |
    tee -a "$LOG" || status=${PIPESTATUS[0]}
  tee -a "$LOG" <"$NOTIFY_LOG"
  step notify-open "exit $status"
else
  step notify "skipped, build or bundle failed"
fi

# --- part 2: keychain A/B ---------------------------------------------------
log ""
log "== Part 2: keychain, build A saves, rebuilt build B reads"
ACCOUNT="run-$(date +%Y%m%d%H%M%S)-$$"
log "service: $SERVICE, account: $ACCOUNT"
KEYCHAIN_TIMEOUT=120
if [[ $INTERACTIVE == 0 ]]; then
  KEYCHAIN_TIMEOUT=30
fi
export PROBE_KEYCHAIN_TIMEOUT=$KEYCHAIN_TIMEOUT
A_APP=$WORK/keychain-A/Thinwire.app
B_APP=$WORK/keychain-B/Thinwire.app
A_EXE=$A_APP/Contents/MacOS/thinwire
B_EXE=$B_APP/Contents/MacOS/thinwire
EXAMPLE=crates/thinwire-notify/examples/macos_keychain_probe.rs

have_a=0
have_b=0
if build keychain-A env L8_BUILD=A cargo build -p thinwire-notify --example macos_keychain_probe --locked &&
  make_bundle "$WORK/keychain-A" target/debug/examples/macos_keychain_probe; then
  have_a=1
fi
# L8_BUILD is an option_env!, so cargo rebuilds; the touch makes sure.
touch "$EXAMPLE"
if build keychain-B env L8_BUILD=B cargo build -p thinwire-notify --example macos_keychain_probe --locked &&
  make_bundle "$WORK/keychain-B" target/debug/examples/macos_keychain_probe; then
  have_b=1
fi

if [[ $have_a == 1 && $have_b == 1 ]]; then
  for app in "$A_APP" "$B_APP"; do
    log "-- $app"
    logged /usr/bin/shasum -a 256 "$app/Contents/MacOS/thinwire" || true
    logged /usr/bin/codesign -dvvv "$app" || true
  done
  sha_a=$(/usr/bin/shasum -a 256 "$A_EXE" | cut -d' ' -f1)
  sha_b=$(/usr/bin/shasum -a 256 "$B_EXE" | cut -d' ' -f1)
  cd_a=$(cdhash "$A_APP")
  cd_b=$(cdhash "$B_APP")
  step keychain-sha256 "A $sha_a B $sha_b $([[ $sha_a != "$sha_b" ]] && echo differ || echo SAME)"
  step keychain-cdhash "A $cd_a B $cd_b $([[ $cd_a != "$cd_b" ]] && echo differ || echo SAME)"
fi

if [[ $have_a == 1 ]]; then
  say "Part 2: keychain. Build A saves a test secret (service $SERVICE)." \
    "No prompt is expected for A."
  pause
  logged "$A_EXE" save "$ACCOUNT" || step keychain-A-save-exit "err $?"
  logged "$A_EXE" read "$ACCOUNT" || step keychain-A-read-exit "err $?"
else
  step keychain-A-save "skipped, build or bundle failed"
fi

if [[ $have_a == 1 && $have_b == 1 ]]; then
  say "Now build B (a rebuild, different hash) reads the same secret." \
    "If macOS shows a Keychain password or allow prompt, click Allow" \
    "(NOT Always Allow) and remember that it showed."
  pause
  logged "$B_EXE" read "$ACCOUNT" || step keychain-B-read-exit "err $?"
  if [[ $INTERACTIVE == 1 ]]; then
    answer=""
    while [[ $answer != y && $answer != n ]]; do
      read -r -p "Did macOS show a Keychain password/allow prompt during the read? [y/n] " answer </dev/tty || answer=n
      answer=$(printf '%s' "$answer" | tr '[:upper:]' '[:lower:]' | cut -c1)
    done
    if [[ $answer == y ]]; then
      step keychain-B-prompt yes
    else
      step keychain-B-prompt no
    fi
  else
    step keychain-B-prompt "not asked (no terminal or CI)"
  fi
else
  step keychain-B-read "skipped, build or bundle failed"
fi

# Build A owns the item's access list, so it deletes without a prompt.
if [[ $have_a == 1 ]] && logged "$A_EXE" delete "$ACCOUNT"; then
  :
elif /usr/bin/security delete-generic-password -s "$SERVICE" -a "$ACCOUNT" >/dev/null 2>&1; then
  step keychain-cleanup "ok (by security delete-generic-password)"
else
  step keychain-cleanup "err, remove '$SERVICE' / '$ACCOUNT' in Keychain Access if it is there"
fi

log ""
log "== end"
echo
echo "Done. Paste /tmp/probe.log into the chat."
