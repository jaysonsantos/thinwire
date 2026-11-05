#!/usr/bin/env bash
# Run sign_with_identity with a fake codesign that exits 42.
# The function must exit 42, and it must delete its entitlements file first.
# Do not execute stage-os-artifact.sh. That script writes dist/.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
stage="${script_dir}/stage-os-artifact.sh"
root="$(mktemp -d)"
cleanup() {
  rm -rf "$root"
}
trap cleanup EXIT

awk '
  /^sign_with_identity\(\) \{/ { capture = 1 }
  capture { print }
  capture && /^\}/ { exit }
' "$stage" >"${root}/sign.sh"

if ! grep -q '^sign_with_identity() {' "${root}/sign.sh"; then
  echo "could not read sign_with_identity from ${stage}" >&2
  exit 1
fi

mkdir -p "${root}/bin" "${root}/tmp" "${root}/work"
cat >"${root}/bin/codesign" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "-d" ]]; then
  exit 1
fi
printf '%s\n' "$*" >"${FAKE_CODESIGN_LOG:?}"
exit 42
EOF
chmod +x "${root}/bin/codesign"

run_case() {
  local name="$1"
  local runtime="$2"
  local notary="$3"
  local want_status="$4"
  local log="${root}/log-${name}"
  local out="${root}/out-${name}"
  local payload="${root}/work/payload-${name}"
  local returned="${root}/returned-${name}"
  local status=0
  : >"$payload"
  rm -f "$log" "$returned" "$out"
  (
    set -euo pipefail
    export PATH="${root}/bin:${PATH}"
    export FAKE_CODESIGN_LOG="$log"
    export TMPDIR="${root}/tmp"
    unset MACOS_NOTARY_KEY MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER
    case "$notary" in
      ready)
        export MACOS_NOTARY_KEY="notary-key-sentinel"
        export MACOS_NOTARY_KEY_ID="notary-id-sentinel"
        export MACOS_NOTARY_ISSUER="notary-issuer-sentinel"
        ;;
      partial)
        export MACOS_NOTARY_KEY="notary-key-sentinel"
        ;;
      absent) ;;
      *)
        echo "unknown notary case ${notary}" >&2
        exit 1
        ;;
    esac
    {
      echo 'sign_identity=TESTIDENTITY'
      echo "sign_use_hardened_runtime=${runtime}"
      cat "${root}/sign.sh"
    } >"${root}/run-${name}.sh"
    # Generated above from sign_with_identity. ShellCheck cannot follow it.
    # shellcheck disable=SC1090,SC1091
    source "${root}/run-${name}.sh"
    sign_with_identity "$payload"
    echo RETURNED >"$returned"
  ) >"$out" || status=$?
  if [[ "$status" -ne "$want_status" ]]; then
    echo "${name}: expected exit ${want_status}, got ${status}" >&2
    echo "stdout: $(cat "$out")" >&2
    exit 1
  fi
  if [[ -f "$returned" ]]; then
    echo "${name}: the function returned after it should have exited" >&2
    exit 1
  fi
  if [[ -n "$(find "${root}/tmp" -type f -print)" ]]; then
    echo "${name}: entitlements temp file was left behind" >&2
    find "${root}/tmp" -type f -print >&2
    exit 1
  fi
  if grep -F -e 'notary-key-sentinel' -e 'notary-id-sentinel' -e 'notary-issuer-sentinel' "$out"; then
    echo "${name}: the function printed a notary variable" >&2
    exit 1
  fi
  local expected=""
  case "$name" in
    runtime-off)
      expected="--force --timestamp=none --sign TESTIDENTITY ${payload}"
      ;;
    runtime-on)
      expected="--force --options runtime --timestamp --sign TESTIDENTITY ${payload}"
      ;;
  esac
  if [[ -n "$expected" ]]; then
    if [[ ! -s "$log" ]]; then
      echo "${name}: codesign was not called" >&2
      exit 1
    fi
    if [[ "$(cat "$log")" != "$expected" ]]; then
      echo "${name}: unexpected codesign arguments" >&2
      echo "got: $(cat "$log")" >&2
      echo "want: ${expected}" >&2
      exit 1
    fi
  elif [[ -s "$log" ]]; then
    echo "${name}: codesign ran before the notary variables were set" >&2
    echo "got: $(cat "$log")" >&2
    exit 1
  fi
  local gate_message=""
  case "$name" in
    runtime-on-absent)
      gate_message="::error::Developer ID builds must be notarized. Missing MACOS_NOTARY_KEY MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER."
      ;;
    runtime-on-partial)
      gate_message="::error::Developer ID builds must be notarized. Missing MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER."
      ;;
  esac
  if [[ -n "$gate_message" ]] && ! grep -qx "$gate_message" "$out"; then
    echo "${name}: notarization error did not name the empty variables" >&2
    echo "stdout: $(cat "$out")" >&2
    exit 1
  fi
}

run_case runtime-off 0 absent 42
run_case runtime-on 1 ready 42
run_case runtime-on-absent 1 absent 1
run_case runtime-on-partial 1 partial 1

if grep -q '::warning::MACOS_SIGNING_IDENTITY did not match' "$stage"; then
  echo "unmatched MACOS_SIGNING_IDENTITY must fail, not warn" >&2
  exit 1
fi
if ! grep -q '::error::MACOS_SIGNING_IDENTITY did not match a hash or name in the temporary keychain.' "$stage"; then
  echo "unmatched MACOS_SIGNING_IDENTITY must name the mismatch and exit" >&2
  exit 1
fi

echo "sign_with_identity propagated codesign exit 42"
