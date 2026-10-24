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
  local runtime="$1"
  local log="${root}/log-${runtime}"
  local payload="${root}/work/payload-${runtime}"
  local returned="${root}/returned-${runtime}"
  local status=0
  : >"$payload"
  rm -f "$log" "$returned"
  (
    set -euo pipefail
    export PATH="${root}/bin:${PATH}"
    export FAKE_CODESIGN_LOG="$log"
    export TMPDIR="${root}/tmp"
    {
      echo 'sign_identity=TESTIDENTITY'
      echo "sign_use_hardened_runtime=${runtime}"
      cat "${root}/sign.sh"
    } >"${root}/run-${runtime}.sh"
    # Generated above from sign_with_identity. ShellCheck cannot follow it.
    # shellcheck disable=SC1090,SC1091
    source "${root}/run-${runtime}.sh"
    sign_with_identity "$payload"
    echo RETURNED >"$returned"
  ) || status=$?
  if [[ "$status" -ne 42 ]]; then
    echo "sign_use_hardened_runtime=${runtime}: expected exit 42, got ${status}" >&2
    exit 1
  fi
  if [[ -f "$returned" ]]; then
    echo "sign_use_hardened_runtime=${runtime}: the function returned after codesign failed" >&2
    exit 1
  fi
  if [[ -n "$(find "${root}/tmp" -type f -print)" ]]; then
    echo "sign_use_hardened_runtime=${runtime}: entitlements temp file was left behind" >&2
    find "${root}/tmp" -type f -print >&2
    exit 1
  fi
  if [[ ! -s "$log" ]]; then
    echo "sign_use_hardened_runtime=${runtime}: codesign was not called" >&2
    exit 1
  fi
  local expected
  if [[ "$runtime" -eq 1 ]]; then
    expected="--force --options runtime --timestamp=none --sign TESTIDENTITY ${payload}"
  else
    expected="--force --timestamp=none --sign TESTIDENTITY ${payload}"
  fi
  if [[ "$(cat "$log")" != "$expected" ]]; then
    echo "sign_use_hardened_runtime=${runtime}: unexpected codesign arguments" >&2
    echo "got: $(cat "$log")" >&2
    echo "want: ${expected}" >&2
    exit 1
  fi
}

run_case 0
run_case 1
echo "sign_with_identity propagated codesign exit 42"
