#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NOTARIZE="${script_dir}/notarize-macos-app.sh"
root="$(mktemp -d)"
cleanup() {
  rm -rf "$root"
}
trap cleanup EXIT

mkdir -p "${root}/bin" "${root}/app/Thinwire.app/Contents/MacOS"
printf '%s\n' '#!/bin/sh' 'exit 0' >"${root}/app/Thinwire.app/Contents/MacOS/thinwire"
chmod 755 "${root}/app/Thinwire.app/Contents/MacOS/thinwire"
APP="${root}/app/Thinwire.app"

python3 - "${root}/key.pem" "${root}/accepted.json" "${root}/invalid.json" "${root}/rejected.json" "${root}/progress.json" <<'PY'
import base64
import pathlib
import sys

pem, accepted, invalid, rejected, progress = sys.argv[1:]
pathlib.Path(pem).write_text(
    "-----BEGIN PRIVATE KEY-----\ndGVzdC1rZXk=\n-----END PRIVATE KEY-----\n",
    encoding="utf-8",
)
pathlib.Path(accepted).write_text(
    '{"id":"submission-1","status":"Accepted","message":"Processing complete"}\n',
    encoding="utf-8",
)
pathlib.Path(invalid).write_text(
    '{"id":"submission-2","status":"Invalid"}\n',
    encoding="utf-8",
)
pathlib.Path(rejected).write_text(
    '{"id":"submission-3","status":"Rejected"}\n',
    encoding="utf-8",
)
pathlib.Path(progress).write_text(
    '{"id":"submission-4","status":"In Progress"}\n',
    encoding="utf-8",
)
PY
KEY_B64="$(python3 -c 'import base64, pathlib, sys; print(base64.b64encode(pathlib.Path(sys.argv[1]).read_bytes()).decode())' "${root}/key.pem")"
KEY_ID="ABCD123456"
ISSUER="12345678-1234-1234-1234-123456789abc"
BAD_KEY_B64="$(python3 -c 'import base64; print(base64.b64encode(b"not a pem\n").decode())')"

DEV_ID=$'Identifier=dev.jaysonsantos.thinwire\nCodeDirectory v=20500 size=100 flags=0x10000(runtime) hashes=1+2 location=embedded\nAuthority=Developer ID Application: Example (TEAM)\nAuthority=Developer ID Certification Authority\nTimestamp=Nov 5, 2026 at 3:04:05 PM\n'
TIMESTAMP_NONE=$'Identifier=dev.jaysonsantos.thinwire\nCodeDirectory v=20500 size=100 flags=0x10000(runtime) hashes=1+2 location=embedded\nAuthority=Developer ID Application: Example (TEAM)\nTimestamp=none\n'
NO_RUNTIME=$'Identifier=dev.jaysonsantos.thinwire\nCodeDirectory v=20500 size=100 flags=0x0(none) hashes=1+2 location=embedded\nAuthority=Developer ID Application: Example (TEAM)\nTimestamp=Nov 5, 2026 at 3:04:05 PM\n'
BOTH_BAD=$'Identifier=dev.jaysonsantos.thinwire\nAuthority=Developer ID Application: Example (TEAM)\nTimestamp=none\nflags=0x0(none)\n'
ADHOC=$'Identifier=dev.jaysonsantos.thinwire\nSignature=adhoc\n'
OTHER=$'Identifier=dev.jaysonsantos.thinwire\nAuthority=Apple Development: Example (TEAM)\nTimestamp=none\nflags=0x0(none)\n'
CA_ONLY=$'Identifier=dev.jaysonsantos.thinwire\nAuthority=Developer ID Certification Authority\nCodeDirectory v=20500 size=100 flags=0x10000(runtime) hashes=1+2 location=embedded\nTimestamp=Nov 5, 2026 at 3:04:05 PM\n'
WRONG_ID=$'Identifier=com.example.other\nSignature=adhoc\n'

cat >"${root}/bin/codesign" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "-dvv" ]]; then
  cat "${FAKE_CODESIGN_DVV:?}"
  exit 0
fi
if [[ "${1:-}" == "--verify" ]]; then
  printf '%s\n' "$*" >>"${FAKE_CODESIGN_VERIFY_LOG:?}"
  exit "${FAKE_CODESIGN_VERIFY_STATUS:-0}"
fi
printf '%s\n' "unexpected codesign $*" >>"${FAKE_CODESIGN_VERIFY_LOG:?}"
exit 1
EOF

cat >"${root}/bin/ditto" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_DITTO_LOG:?}"
last=""
for last in "$@"; do
  :
done
mkdir -p "$(dirname "$last")"
touch "$last"
exit 0
EOF

cat >"${root}/bin/xcrun" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
tool="${1:-}"
shift || true
case "$tool" in
  notarytool)
    sub="${1:-}"
    shift || true
    case "$sub" in
      submit)
        printf '%s\n' "$*" >>"${FAKE_NOTARY_SUBMIT_LOG:?}"
        if [[ -n "${MACOS_NOTARY_KEY+x}" || -n "${MACOS_NOTARY_KEY_ID+x}" || -n "${MACOS_NOTARY_ISSUER+x}" ]]; then
          echo leak >>"${FAKE_NOTARY_ENV_LEAK:?}"
        fi
        key_path=""
        prev=""
        for arg in "$@"; do
          if [[ "$prev" == "--key" ]]; then
            key_path="$arg"
          fi
          prev="$arg"
        done
        if [[ -n "$key_path" ]]; then
          python3 -c 'import os, stat, sys; print(format(stat.S_IMODE(os.stat(sys.argv[1]).st_mode), "o"))' "$key_path" >>"${FAKE_KEY_MODE:?}"
          python3 -c 'import os, stat, sys; print(format(stat.S_IMODE(os.stat(sys.argv[1]).st_mode), "o"))' "$(dirname "$key_path")" >>"${FAKE_DIR_MODE:?}"
          first=""
          IFS= read -r first <"$key_path" || true
          if [[ "$first" == "-----BEGIN PRIVATE KEY-----" ]]; then
            echo pem-ok >>"${FAKE_KEY_MODE:?}"
          else
            echo pem-bad >>"${FAKE_KEY_MODE:?}"
          fi
        fi
        if [[ -n "${FAKE_NOTARY_SUBMIT_JSON:-}" && -f "${FAKE_NOTARY_SUBMIT_JSON}" ]]; then
          cat "${FAKE_NOTARY_SUBMIT_JSON}"
        fi
        exit "${FAKE_NOTARY_SUBMIT_STATUS:-0}"
        ;;
      log)
        printf '%s\n' "$*" >>"${FAKE_NOTARY_LOG_LOG:?}"
        printf '%s\n' '{"log":"opaque"}'
        exit 0
        ;;
      *)
        echo "unexpected notarytool ${sub}" >&2
        exit 1
        ;;
    esac
    ;;
  stapler)
    sub="${1:-}"
    shift || true
    printf '%s\n' "${sub} $*" >>"${FAKE_STAPLER_LOG:?}"
    if [[ "$sub" == "staple" ]]; then
      n=0
      if [[ -s "${FAKE_STAPLER_COUNT:?}" ]]; then
        n="$(cat "${FAKE_STAPLER_COUNT}")"
      fi
      n=$((n + 1))
      printf '%s\n' "$n" >"${FAKE_STAPLER_COUNT}"
      if [[ "$n" -le "${FAKE_STAPLER_FAIL_FIRST:-0}" ]]; then
        exit 1
      fi
      exit 0
    fi
    if [[ "$sub" == "validate" ]]; then
      exit "${FAKE_STAPLER_VALIDATE_STATUS:-0}"
    fi
    echo "unexpected stapler ${sub}" >&2
    exit 1
    ;;
  *)
    echo "unexpected xcrun ${tool}" >&2
    exit 1
    ;;
esac
EOF

cat >"${root}/bin/sleep" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${FAKE_SLEEP_LOG:?}"
exit 0
EOF
chmod +x "${root}/bin/codesign" "${root}/bin/ditto" "${root}/bin/xcrun" "${root}/bin/sleep"

reset_case_env() {
  unset MACOS_NOTARY_KEY MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER
  unset FAKE_NOTARY_SUBMIT_JSON
  export FAKE_NOTARY_SUBMIT_STATUS=0
  export FAKE_STAPLER_VALIDATE_STATUS=0
  export FAKE_STAPLER_FAIL_FIRST=0
  export FAKE_CODESIGN_VERIFY_STATUS=0
}

case_begin() {
  local name="$1"
  local dvv_text="$2"
  reset_case_env
  LAST_NAME="$name"
  LAST_DIR="${root}/runs/${name}"
  rm -rf "$LAST_DIR"
  mkdir -p "$LAST_DIR/tmp" "$LAST_DIR/logs"
  printf '%s' "$dvv_text" >"$LAST_DIR/dvv"
  export FAKE_CODESIGN_DVV="$LAST_DIR/dvv"
  export FAKE_DITTO_LOG="$LAST_DIR/logs/ditto"
  export FAKE_NOTARY_SUBMIT_LOG="$LAST_DIR/logs/submit"
  export FAKE_NOTARY_LOG_LOG="$LAST_DIR/logs/notary-log"
  export FAKE_NOTARY_ENV_LEAK="$LAST_DIR/logs/leak"
  export FAKE_STAPLER_LOG="$LAST_DIR/logs/stapler"
  export FAKE_STAPLER_COUNT="$LAST_DIR/logs/staple-count"
  export FAKE_SLEEP_LOG="$LAST_DIR/logs/sleep"
  export FAKE_CODESIGN_VERIFY_LOG="$LAST_DIR/logs/verify"
  export FAKE_KEY_MODE="$LAST_DIR/logs/key-mode"
  export FAKE_DIR_MODE="$LAST_DIR/logs/dir-mode"
  export TMPDIR="$LAST_DIR/tmp"
  : >"$FAKE_DITTO_LOG"
  : >"$FAKE_NOTARY_SUBMIT_LOG"
  : >"$FAKE_NOTARY_LOG_LOG"
  : >"$FAKE_NOTARY_ENV_LEAK"
  : >"$FAKE_STAPLER_LOG"
  : >"$FAKE_STAPLER_COUNT"
  : >"$FAKE_SLEEP_LOG"
  : >"$FAKE_CODESIGN_VERIFY_LOG"
  : >"$FAKE_KEY_MODE"
  : >"$FAKE_DIR_MODE"
}

use_ready_credential() {
  export MACOS_NOTARY_KEY="$KEY_B64"
  export MACOS_NOTARY_KEY_ID="$KEY_ID"
  export MACOS_NOTARY_ISSUER="$ISSUER"
}

run_script() {
  local status=0
  (
    export PATH="${root}/bin:${PATH}"
    bash "$NOTARIZE" "$@"
  ) >"$LAST_DIR/out" 2>"$LAST_DIR/err" || status=$?
  LAST_STATUS=$status
}

show_case() {
  echo "${LAST_NAME}: $1" >&2
  echo "stdout: $(cat "$LAST_DIR/out")" >&2
  echo "stderr: $(cat "$LAST_DIR/err")" >&2
}

expect_status() {
  local want="$1"
  if [[ "$LAST_STATUS" -ne "$want" ]]; then
    show_case "exit ${LAST_STATUS}, want ${want}"
    exit 1
  fi
}

expect_line() {
  local want="$1"
  if ! grep -qx "$want" "$LAST_DIR/out"; then
    show_case "missing line: ${want}"
    exit 1
  fi
}

expect_contains() {
  local want="$1"
  if ! grep -F -q "$want" "$LAST_DIR/out"; then
    show_case "missing text: ${want}"
    exit 1
  fi
}

refuse_secrets() {
  local file
  shopt -s nullglob
  for file in "$LAST_DIR/out" "$LAST_DIR/err" "$LAST_DIR/logs/"*; do
    if [[ -f "$file" ]] && grep -F -e '-----BEGIN PRIVATE KEY-----' -e "$KEY_B64" -e "$BAD_KEY_B64" "$file" >/dev/null; then
      show_case "secret leaked in ${file}"
      exit 1
    fi
  done
  shopt -u nullglob
}

assert_work_gone() {
  if find "$LAST_DIR/tmp" \( -name 'AuthKey.p8' -o -name 'Thinwire.zip' \) | grep -q .; then
    show_case "notary work files were left behind"
    find "$LAST_DIR/tmp" -print >&2
    exit 1
  fi
}

submit_count() {
  grep -c . "$FAKE_NOTARY_SUBMIT_LOG" || true
}

staple_count() {
  grep -c '^staple ' "$FAKE_STAPLER_LOG" || true
}

finish_case() {
  refuse_secrets
  assert_work_gone
}

case_begin accepted "$DEV_ID"
use_ready_credential
export FAKE_NOTARY_SUBMIT_JSON="${root}/accepted.json"
run_script staple "$APP"
expect_status 0
expect_contains "notarization: id submission-1 status Accepted"
expect_line "notarization: stapled"
expect_contains "notarization: key id ${KEY_ID} issuer ${ISSUER}"
if ! grep -F -q -- '-c -k --keepParent' "$FAKE_DITTO_LOG"; then
  show_case "ditto was not called with --keepParent"
  exit 1
fi
if ! grep -F -q -- "$APP" "$FAKE_DITTO_LOG"; then
  show_case "ditto did not see the app"
  exit 1
fi
if ! grep -F -q -- '--key' "$FAKE_NOTARY_SUBMIT_LOG" \
  || ! grep -F -q -- '--key-id' "$FAKE_NOTARY_SUBMIT_LOG" \
  || ! grep -F -q -- '--issuer' "$FAKE_NOTARY_SUBMIT_LOG" \
  || ! grep -F -q -- '--wait' "$FAKE_NOTARY_SUBMIT_LOG" \
  || ! grep -F -q -- '--timeout 20m' "$FAKE_NOTARY_SUBMIT_LOG"; then
  show_case "notarytool submit is missing a required flag"
  echo "submit: $(cat "$FAKE_NOTARY_SUBMIT_LOG")" >&2
  exit 1
fi
if [[ "$(submit_count)" -ne 1 ]]; then
  show_case "submit ran $(submit_count) times"
  exit 1
fi
if [[ -s "$FAKE_NOTARY_ENV_LEAK" ]]; then
  show_case "notary variables were still set at submit"
  exit 1
fi
if ! grep -qx '600' "$FAKE_KEY_MODE" || ! grep -qx 'pem-ok' "$FAKE_KEY_MODE"; then
  show_case "AuthKey.p8 was not a mode 0600 private key"
  echo "mode: $(cat "$FAKE_KEY_MODE")" >&2
  exit 1
fi
if ! grep -qx '700' "$FAKE_DIR_MODE"; then
  show_case "notary work dir was not mode 0700"
  echo "mode: $(cat "$FAKE_DIR_MODE")" >&2
  exit 1
fi
if ! grep -F -q -- "staple ${APP}" "$FAKE_STAPLER_LOG"; then
  show_case "stapler staple did not see the app"
  exit 1
fi
if [[ -s "$FAKE_SLEEP_LOG" ]]; then
  show_case "staple slept after the first success"
  exit 1
fi
finish_case

for terminal in invalid rejected; do
  json="${root}/${terminal}.json"
  case_begin "$terminal" "$DEV_ID"
  use_ready_credential
  export FAKE_NOTARY_SUBMIT_JSON="$json"
  run_script staple "$APP"
  expect_status 1
  expect_line "::error::notarization: notary rejected the app."
  if [[ "$(submit_count)" -ne 1 ]]; then
    show_case "terminal status was retried"
    exit 1
  fi
  if [[ ! -s "$FAKE_NOTARY_LOG_LOG" ]]; then
    show_case "notarytool log was not called"
    exit 1
  fi
  if [[ "$(staple_count)" -ne 0 ]]; then
    show_case "stapler staple ran after rejection"
    exit 1
  fi
  if [[ -s "$FAKE_DITTO_LOG" ]]; then
    :
  else
    show_case "ditto should run before a rejected submit"
    exit 1
  fi
  finish_case
done

case_begin transport "$DEV_ID"
use_ready_credential
export FAKE_NOTARY_SUBMIT_STATUS=1
run_script staple "$APP"
expect_status 1
expect_contains "the upload did not return a status (exit 1)."
if [[ "$(submit_count)" -ne 4 ]]; then
  show_case "transport retries were $(submit_count), want 4 submits"
  exit 1
fi
if [[ "$(staple_count)" -ne 0 || -s "$FAKE_NOTARY_LOG_LOG" ]]; then
  show_case "transport failure fetched a log or stapled"
  exit 1
fi
finish_case

case_begin in-progress "$DEV_ID"
use_ready_credential
export FAKE_NOTARY_SUBMIT_JSON="${root}/progress.json"
run_script staple "$APP"
expect_status 1
expect_line "::error::notarization: notary status In Progress."
if [[ "$(submit_count)" -ne 1 || "$(staple_count)" -ne 0 ]]; then
  show_case "In Progress was retried or stapled"
  exit 1
fi
finish_case

case_begin staple-retry "$DEV_ID"
use_ready_credential
export FAKE_NOTARY_SUBMIT_JSON="${root}/accepted.json"
export FAKE_STAPLER_FAIL_FIRST=2
run_script staple "$APP"
expect_status 0
expect_line "notarization: stapled"
if [[ "$(staple_count)" -ne 3 ]]; then
  show_case "staple attempts were $(staple_count), want 3"
  exit 1
fi
if [[ "$(cat "$FAKE_SLEEP_LOG")" != $'10\n10' ]]; then
  show_case "staple retry did not sleep 10 seconds twice"
  echo "sleep: $(cat "$FAKE_SLEEP_LOG")" >&2
  exit 1
fi
finish_case

case_begin dev-id-absent "$DEV_ID"
run_script staple "$APP"
expect_status 1
expect_line "::error::Developer ID builds must be notarized. Set MACOS_NOTARY_KEY, MACOS_NOTARY_KEY_ID, and MACOS_NOTARY_ISSUER."
if [[ -s "$FAKE_NOTARY_SUBMIT_LOG" || -s "$FAKE_DITTO_LOG" ]]; then
  show_case "absent credential called ditto or notarytool"
  exit 1
fi
finish_case

case_begin adhoc-absent "$ADHOC"
run_script staple "$APP"
expect_status 0
expect_line "notarization: skipped (ad-hoc signature)"
if [[ -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "ad-hoc signature called notarytool"
  exit 1
fi
finish_case

case_begin other-absent "$OTHER"
run_script staple "$APP"
expect_status 0
expect_line "notarization: skipped (not Developer ID Application)"
finish_case

case_begin ca-only-absent "$CA_ONLY"
run_script staple "$APP"
expect_status 0
expect_line "notarization: skipped (not Developer ID Application)"
finish_case

case_begin adhoc-ready "$ADHOC"
use_ready_credential
run_script staple "$APP"
expect_status 1
expect_line "::error::Notary credentials are set, and this signature is not Developer ID Application."
if [[ -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "non-Developer-ID signature was submitted"
  exit 1
fi
finish_case

case_begin other-ready "$OTHER"
use_ready_credential
run_script staple "$APP"
expect_status 1
expect_line "::error::Notary credentials are set, and this signature is not Developer ID Application."
finish_case

case_begin partial "$DEV_ID"
export MACOS_NOTARY_KEY_ID="notary-id-sentinel"
run_script staple "$APP"
expect_status 1
expect_line "::error::Notary credentials are incomplete. Missing MACOS_NOTARY_KEY MACOS_NOTARY_ISSUER."
if grep -F -q 'notary-id-sentinel' "$LAST_DIR/out" "$LAST_DIR/err"; then
  show_case "partial credential printed a value"
  exit 1
fi
if [[ -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "partial credential called notarytool"
  exit 1
fi
finish_case

case_begin bad-key-id "$DEV_ID"
export MACOS_NOTARY_KEY="$KEY_B64"
export MACOS_NOTARY_KEY_ID="abcd123456"
export MACOS_NOTARY_ISSUER="$ISSUER"
run_script staple "$APP"
expect_status 1
expect_line "::error::MACOS_NOTARY_KEY_ID must be 10 uppercase letters or digits."
if grep -F -q 'abcd123456' "$LAST_DIR/out" "$LAST_DIR/err"; then
  show_case "invalid key id was printed"
  exit 1
fi
finish_case

case_begin bad-key "$DEV_ID"
export MACOS_NOTARY_KEY="$BAD_KEY_B64"
export MACOS_NOTARY_KEY_ID="$KEY_ID"
export MACOS_NOTARY_ISSUER="$ISSUER"
run_script staple "$APP"
expect_status 1
expect_line "::error::MACOS_NOTARY_KEY did not decode to a private key."
if [[ -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "a bad key was submitted"
  exit 1
fi
finish_case

case_begin timestamp-none "$TIMESTAMP_NONE"
use_ready_credential
run_script staple "$APP"
expect_status 1
expect_line "::error::Developer ID signature cannot be notarized: no secure timestamp"
if [[ -s "$FAKE_DITTO_LOG" || -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "Timestamp=none reached ditto"
  exit 1
fi
finish_case

case_begin runtime-off "$NO_RUNTIME"
use_ready_credential
run_script staple "$APP"
expect_status 1
expect_line "::error::Developer ID signature cannot be notarized: hardened runtime off"
finish_case

case_begin both-facts "$BOTH_BAD"
use_ready_credential
run_script staple "$APP"
expect_status 1
expect_line "::error::Developer ID signature cannot be notarized: no secure timestamp, hardened runtime off"
finish_case

case_begin wrong-identifier "$WRONG_ID"
run_script staple "$APP"
expect_status 1
expect_contains "Identifier is not dev.jaysonsantos.thinwire."
finish_case

make_archive() {
  local dest="$LAST_DIR/thinwire-macos-arm64.tar.gz"
  COPYFILE_DISABLE=1 tar -czf "$dest" -C "${root}/app" Thinwire.app
  printf '%s' "$dest"
}

case_begin validate-dev-id "$DEV_ID"
archive="$(make_archive)"
run_script validate-archive "$archive"
expect_status 0
expect_line "notarization: validate ok"
if [[ -s "$FAKE_NOTARY_SUBMIT_LOG" || "$(staple_count)" -ne 0 ]]; then
  show_case "validate-archive submitted or stapled"
  exit 1
fi
if ! grep -q '^validate ' "$FAKE_STAPLER_LOG"; then
  show_case "validate-archive did not run stapler validate"
  exit 1
fi
if ! grep -q -- '--verify --strict --deep' "$FAKE_CODESIGN_VERIFY_LOG"; then
  show_case "validate-archive did not verify the signature"
  exit 1
fi
finish_case

case_begin validate-reject "$DEV_ID"
export FAKE_STAPLER_VALIDATE_STATUS=1
archive="$(make_archive)"
run_script validate-archive "$archive"
expect_status 1
expect_line "::error::notarization: stapler validate failed."
if [[ "$(staple_count)" -ne 0 || -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "a failed validate submitted or stapled"
  exit 1
fi
finish_case

case_begin validate-adhoc "$ADHOC"
use_ready_credential
archive="$(make_archive)"
run_script validate-archive "$archive"
expect_status 0
expect_line "notarization: validate skipped"
if [[ -s "$FAKE_STAPLER_LOG" || -s "$FAKE_NOTARY_SUBMIT_LOG" ]]; then
  show_case "ad-hoc archive called stapler or notarytool"
  exit 1
fi
finish_case

case_begin validate-timestamp-none "$TIMESTAMP_NONE"
archive="$(make_archive)"
run_script validate-archive "$archive"
expect_status 1
expect_line "::error::Developer ID signature cannot be notarized: no secure timestamp"
if [[ -s "$FAKE_STAPLER_LOG" ]]; then
  show_case "unstapleable archive called stapler"
  exit 1
fi
finish_case

run_script_direct() {
  LAST_NAME="usage"
  LAST_DIR="${root}/runs/usage"
  mkdir -p "$LAST_DIR"
  local status=0
  bash "$NOTARIZE" >"$LAST_DIR/out" 2>"$LAST_DIR/err" || status=$?
  LAST_STATUS=$status
}
run_script_direct
expect_status 2
if ! grep -F -q 'usage:' "$LAST_DIR/err"; then
  show_case "usage did not go to stderr"
  exit 1
fi

source_status=0
source_dir="${root}/runs/source"
mkdir -p "$source_dir"
# shellcheck disable=SC1090
source "$NOTARIZE" >"$source_dir/out" 2>"$source_dir/err" || source_status=$?
if [[ "$source_status" -eq 0 ]]; then
  echo "sourcing the notarize script should fail" >&2
  exit 1
fi
if declare -F read_notary_credential >/dev/null 2>&1; then
  echo "sourcing defined read_notary_credential" >&2
  exit 1
fi
if ! grep -F -q 'Do not source it.' "$source_dir/err"; then
  echo "sourcing did not say to run the file" >&2
  echo "stderr: $(cat "$source_dir/err")" >&2
  exit 1
fi

echo "notarize-macos-app covered staple, validate-archive, and the source guard"
