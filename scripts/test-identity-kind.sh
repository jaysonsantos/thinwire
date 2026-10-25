#!/usr/bin/env bash
# Check identity_kind for Developer ID Application, Apple Development,
# self-signed, and unknown.
# A name that starts with "Apple Development" is that type.
# --options runtime stays limited to Developer ID Application.
# Do not execute stage-os-artifact.sh. That script writes dist/.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
stage="${script_dir}/stage-os-artifact.sh"
root="$(mktemp -d)"
cleanup() {
  rm -rf "$root"
}
trap cleanup EXIT

extract_function() {
  local name="$1" dest="$2"
  awk -v fn="$name" '
    $0 ~ ("^" fn "\\(\\) \\{") { capture = 1 }
    capture { print }
    capture && /^\}/ { exit }
  ' "$stage" >"$dest"
  if ! grep -q "^${name}() {" "$dest"; then
    echo "could not read ${name} from ${stage}" >&2
    exit 1
  fi
}

extract_function trim_spaces "${root}/trim_spaces.sh"
extract_function lower_hex "${root}/lower_hex.sh"
extract_function identity_kind "${root}/identity_kind.sh"

mkdir -p "${root}/bin" "${root}/tmp"
cat >"${root}/bin/security" <<'EOF'
#!/usr/bin/env bash
: >"${FAKE_SECURITY_CALLED:?}"
if [[ -z "${FAKE_CERT_HASH:-}" ]]; then
  exit 0
fi
printf '%s\n' "SHA-1 hash: ${FAKE_CERT_HASH}"
printf '%s\n' "-----BEGIN CERTIFICATE-----"
printf '%s\n' "FAKECERT"
printf '%s\n' "-----END CERTIFICATE-----"
EOF
cat >"${root}/bin/openssl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "subject=${FAKE_SUBJECT:?}"
printf '%s\n' "issuer=${FAKE_ISSUER:?}"
EOF
chmod +x "${root}/bin/security" "${root}/bin/openssl"

# Generated above from stage-os-artifact.sh. ShellCheck cannot follow it.
# shellcheck disable=SC1090,SC1091
source "${root}/trim_spaces.sh"
# shellcheck disable=SC1090,SC1091
source "${root}/lower_hex.sh"
# shellcheck disable=SC1090,SC1091
source "${root}/identity_kind.sh"

export PATH="${root}/bin:${PATH}"
export TMPDIR="${root}/tmp"
export FAKE_SECURITY_CALLED="${root}/security-called"
hash="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
# identity_kind passes this path to security. The fake ignores the path.
# shellcheck disable=SC2034
signing_keychain="${root}/missing.keychain"

expect_kind() {
  local name="$1" want="$2" tools="$3"
  rm -f "$FAKE_SECURITY_CALLED"
  find "${root}/tmp" -type f -delete
  identity_kind_result=""
  identity_kind "$hash" "$name"
  if [[ "$identity_kind_result" != "$want" ]]; then
    echo "name ${name}: want ${want}, got ${identity_kind_result}" >&2
    exit 1
  fi
  if [[ "$tools" == "no" && -f "$FAKE_SECURITY_CALLED" ]]; then
    echo "name ${name}: security was called" >&2
    exit 1
  fi
  if [[ "$tools" == "yes" && ! -f "$FAKE_SECURITY_CALLED" ]]; then
    echo "name ${name}: security was not called" >&2
    exit 1
  fi
  if [[ -n "$(find "${root}/tmp" -type f -print)" ]]; then
    echo "name ${name}: certificate temp file was left behind" >&2
    find "${root}/tmp" -type f -print >&2
    exit 1
  fi
}

export FAKE_CERT_HASH=""
export FAKE_SUBJECT=""
export FAKE_ISSUER=""

expect_kind "Developer ID Application: Example (TEAM)" "Developer ID Application" no
expect_kind "Apple Development" "Apple Development" no
expect_kind "Apple Development: Example (TEAM)" "Apple Development" no
expect_kind "Apple Development Example (TEAM)" "Apple Development" no
expect_kind "Apple Developmental: Example (TEAM)" "unknown" yes
expect_kind "Apple Distribution: Example (TEAM)" "unknown" yes
expect_kind "Prefix Apple Development: Example (TEAM)" "unknown" yes

export FAKE_CERT_HASH="$hash"
export FAKE_SUBJECT="CN=Developer ID Application: Example (TEAM)"
export FAKE_ISSUER="CN=Apple Root CA"
expect_kind "Custom Name" "Developer ID Application" yes

export FAKE_SUBJECT="CN=Example"
export FAKE_ISSUER="CN=Example"
expect_kind "Local" "self-signed" yes

export FAKE_SUBJECT="CN=Example"
export FAKE_ISSUER="CN=Apple Worldwide Developer Relations"
expect_kind "Other" "unknown" yes

# Name prefix wins over the certificate subject.
export FAKE_SUBJECT="CN=Example"
export FAKE_ISSUER="CN=Example"
expect_kind "Apple Development: Example (TEAM)" "Apple Development" no

# The ${} names are literal text in the stage script, not this script.
# shellcheck disable=SC2016
if ! grep -F -q 'echo "codesign identity: ${hash} \"${name}\" (${kind})"' "$stage"; then
  echo "the identity log line must include the type" >&2
  exit 1
fi

enable_count="$(grep -c 'sign_use_hardened_runtime=1' "$stage" || true)"
if [[ "$enable_count" -ne 1 ]]; then
  echo "expected one hardened-runtime enable, found ${enable_count}" >&2
  exit 1
fi
enable_line="$(grep -n 'sign_use_hardened_runtime=1' "$stage" | cut -d: -f1)"
gate="$(sed -n "$((enable_line - 1))p" "$stage")"
case "$gate" in
  *'Apple Development'*)
    echo "Apple Development must not enable hardened runtime: ${gate}" >&2
    exit 1
    ;;
  *'Developer ID Application'*) ;;
  *)
    echo "hardened runtime enable is not gated on Developer ID Application: ${gate}" >&2
    exit 1
    ;;
esac

echo "identity_kind labeled Apple Development and kept the other types"
