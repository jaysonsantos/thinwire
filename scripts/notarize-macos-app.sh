#!/usr/bin/env bash
# `exit` would kill the shell that sourced the file.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
  echo "Run scripts/notarize-macos-app.sh. Do not source it." >&2
  return 1
fi

set -euo pipefail

notary_credential_state=""
notary_work=""
notary_extract=""
notary_key_file=""
notary_key_id=""
notary_issuer=""
notary_zip=""
notary_submit_json=""
notary_submission_id=""
notary_submission_status=""

signature_class=""
signature_timestamp=""
signature_hardened=""

notary_decision=""
notary_decision_message=""

cleanup_notary() {
  if [[ -n "$notary_work" ]]; then
    rm -rf "$notary_work"
    notary_work=""
  fi
  if [[ -n "$notary_extract" ]]; then
    rm -rf "$notary_extract"
    notary_extract=""
  fi
}

trim_edges() {
  local s="$1"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s' "$s"
}

# macOS base64 uses -D. GNU base64 does not, and the credential test runs on Linux.
decode_base64_file() {
  local payload="$1"
  local dest="$2"
  if printf '%s' "$payload" | base64 -D >"$dest" 2>/dev/null; then
    return 0
  fi
  printf '%s' "$payload" | base64 -d >"$dest" 2>/dev/null
}

silence_key_trace() {
  notary_xtrace=0
  case $- in
    *x*)
      notary_xtrace=1
      set +x
      ;;
  esac
}

restore_key_trace() {
  if [[ "${notary_xtrace:-0}" -eq 1 ]]; then
    set -x
  fi
}

read_notary_credential() {
  local key_raw key_id issuer key_stripped missing
  local xtrace_was_on=0
  silence_key_trace
  if [[ "${notary_xtrace:-0}" -eq 1 ]]; then
    xtrace_was_on=1
  fi
  key_raw="${MACOS_NOTARY_KEY:-}"
  key_id="$(trim_edges "${MACOS_NOTARY_KEY_ID:-}")"
  issuer="$(trim_edges "${MACOS_NOTARY_ISSUER:-}")"
  key_stripped="$(printf '%s' "$key_raw" | tr -d '[:space:]')"
  unset MACOS_NOTARY_KEY MACOS_NOTARY_KEY_ID MACOS_NOTARY_ISSUER

  if [[ -z "$key_stripped" && -z "$key_id" && -z "$issuer" ]]; then
    notary_credential_state="absent"
    if [[ "$xtrace_was_on" -eq 1 ]]; then
      restore_key_trace
    fi
    return 0
  fi

  missing=""
  if [[ -z "$key_stripped" ]]; then
    missing+=" MACOS_NOTARY_KEY"
  fi
  if [[ -z "$key_id" ]]; then
    missing+=" MACOS_NOTARY_KEY_ID"
  fi
  if [[ -z "$issuer" ]]; then
    missing+=" MACOS_NOTARY_ISSUER"
  fi
  if [[ -n "$missing" ]]; then
    if [[ "$xtrace_was_on" -eq 1 ]]; then
      restore_key_trace
    fi
    echo "::error::Notary credentials are incomplete. Missing${missing}."
    exit 1
  fi

  local key_id_pattern='^[A-Z0-9]{10}$'
  local issuer_pattern='^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
  if [[ ! "$key_id" =~ $key_id_pattern ]]; then
    if [[ "$xtrace_was_on" -eq 1 ]]; then
      restore_key_trace
    fi
    echo "::error::MACOS_NOTARY_KEY_ID must be 10 uppercase letters or digits."
    exit 1
  fi
  if [[ ! "$issuer" =~ $issuer_pattern ]]; then
    if [[ "$xtrace_was_on" -eq 1 ]]; then
      restore_key_trace
    fi
    echo "::error::MACOS_NOTARY_ISSUER must be a UUID."
    exit 1
  fi

  notary_work="$(mktemp -d)"
  chmod 700 "$notary_work"
  notary_key_file="${notary_work}/AuthKey.p8"
  notary_zip="${notary_work}/Thinwire.zip"
  notary_submit_json="${notary_work}/submit.json"
  local decoded=0
  if decode_base64_file "$key_stripped" "$notary_key_file"; then
    decoded=1
  fi
  chmod 600 "$notary_key_file"
  local size=0 first=""
  if [[ -f "$notary_key_file" ]]; then
    size="$(wc -c <"$notary_key_file" | tr -d '[:space:]')"
  fi
  if [[ "$size" -ge 1 && "$size" -le 4096 ]]; then
    IFS= read -r first <"$notary_key_file" || true
  fi
  if [[ "$decoded" -ne 1 || "$first" != "-----BEGIN PRIVATE KEY-----" ]]; then
    if [[ "$xtrace_was_on" -eq 1 ]]; then
      restore_key_trace
    fi
    echo "::error::MACOS_NOTARY_KEY did not decode to a private key."
    exit 1
  fi

  notary_credential_state="ready"
  notary_key_id="$key_id"
  notary_issuer="$issuer"
  if [[ "$xtrace_was_on" -eq 1 ]]; then
    restore_key_trace
  fi
}

read_signature_facts() {
  local app="$1"
  local base info status line identifier signature leaf other_authority
  base="$(basename "$app")"
  if [[ ! -d "$app" || "$base" != "Thinwire.app" ]]; then
    echo "::error::notarization: expected a Thinwire.app directory."
    exit 1
  fi
  if [[ ! -x "$app/Contents/MacOS/thinwire" ]]; then
    echo "::error::notarization: Thinwire.app is missing Contents/MacOS/thinwire."
    exit 1
  fi
  info="$(mktemp)"
  status=0
  codesign -dvv "$app" >"$info" 2>&1 || status=$?
  if [[ "$status" -ne 0 ]]; then
    rm -f "$info"
    echo "::error::notarization: codesign -dvv failed."
    exit 1
  fi

  identifier=""
  signature=""
  leaf=0
  other_authority=0
  signature_timestamp="missing"
  signature_hardened="no"
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      Identifier=*)
        identifier="${line#Identifier=}"
        ;;
      Signature=*)
        signature="${line#Signature=}"
        ;;
      "Authority=Developer ID Application:"*)
        leaf=1
        ;;
      Authority=*)
        other_authority=1
        ;;
      Timestamp=*)
        if [[ "${line#Timestamp=}" == "none" ]]; then
          signature_timestamp="none"
        elif [[ -n "${line#Timestamp=}" ]]; then
          signature_timestamp="secure"
        else
          signature_timestamp="missing"
        fi
        ;;
    esac
    # codesign prints the runtime flag on the CodeDirectory line.
    if [[ "$line" == *flags=* && "$line" == *"(runtime)"* ]]; then
      signature_hardened="yes"
    fi
  done <"$info"
  rm -f "$info"

  if [[ "$identifier" != "dev.jaysonsantos.thinwire" ]]; then
    echo "::error::notarization: Identifier is not dev.jaysonsantos.thinwire."
    exit 1
  fi
  if [[ "$signature" == "adhoc" ]]; then
    signature_class="adhoc"
  elif [[ "$leaf" -eq 1 ]]; then
    signature_class="developer_id"
  elif [[ "$other_authority" -eq 1 ]]; then
    signature_class="other"
  else
    echo "::error::notarization: codesign did not report a signature class."
    exit 1
  fi
}

must_notarize() {
  [[ "$signature_class" == "developer_id" ]]
}

notarizable() {
  must_notarize \
    && [[ "$signature_timestamp" == "secure" ]] \
    && [[ "$signature_hardened" == "yes" ]]
}

notary_failed_facts() {
  local message="Developer ID signature cannot be notarized:"
  local needs_comma=0
  if [[ "$signature_timestamp" != "secure" ]]; then
    message+=" no secure timestamp"
    needs_comma=1
  fi
  if [[ "$signature_hardened" != "yes" ]]; then
    if [[ "$needs_comma" -eq 1 ]]; then
      message+=","
    fi
    message+=" hardened runtime off"
  fi
  printf '%s\n' "$message"
}

decide_notary_action() {
  notary_decision=""
  notary_decision_message=""
  if [[ "$notary_credential_state" != "absent" && "$notary_credential_state" != "ready" ]]; then
    notary_decision="fail"
    notary_decision_message="Notary credential was not read."
    return 0
  fi
  if must_notarize; then
    if [[ "$notary_credential_state" == "absent" ]]; then
      notary_decision="fail"
      notary_decision_message="Developer ID builds must be notarized. Set MACOS_NOTARY_KEY, MACOS_NOTARY_KEY_ID, and MACOS_NOTARY_ISSUER."
      return 0
    fi
    if ! notarizable; then
      notary_decision="fail"
      notary_decision_message="$(notary_failed_facts)"
      return 0
    fi
    notary_decision="staple"
    return 0
  fi
  if [[ "$notary_credential_state" == "ready" ]]; then
    notary_decision="fail"
    notary_decision_message="Notary credentials are set, and this signature is not Developer ID Application."
    return 0
  fi
  notary_decision="skip"
  case "$signature_class" in
    adhoc)
      notary_decision_message="notarization: skipped (ad-hoc signature)"
      ;;
    other)
      notary_decision_message="notarization: skipped (not Developer ID Application)"
      ;;
    *)
      notary_decision="fail"
      notary_decision_message="notarization: unknown signature class."
      ;;
  esac
}

parse_notary_submission() {
  local parsed status=0
  notary_submission_id=""
  notary_submission_status=""
  if [[ ! -s "$notary_submit_json" ]]; then
    return 1
  fi
  parsed="$(
    python3 - "$notary_submit_json" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
except (OSError, json.JSONDecodeError):
    sys.exit(2)
if not isinstance(data, dict):
    sys.exit(2)
ident = data.get("id")
status = data.get("status")
if not isinstance(ident, str) or not isinstance(status, str):
    sys.exit(2)
if ident == "" or status == "":
    sys.exit(2)
print(ident)
print(status)
PY
  )" || status=$?
  if [[ "$status" -ne 0 ]]; then
    return 1
  fi
  notary_submission_id="${parsed%%$'\n'*}"
  notary_submission_status="${parsed#*$'\n'}"
  if [[ -z "$notary_submission_id" || -z "$notary_submission_status" ]]; then
    return 1
  fi
  if [[ "$notary_submission_id" == "$parsed" ]]; then
    return 1
  fi
}

fetch_notary_log() {
  local id="$1"
  if [[ -z "$id" ]]; then
    return 0
  fi
  xcrun notarytool log "$id" \
    --key "$notary_key_file" \
    --key-id "$notary_key_id" \
    --issuer "$notary_issuer" \
    --output-format json || true
}

reject_notary_status() {
  local id="$1"
  local submit_status="$2"
  fetch_notary_log "$id"
  if [[ "$submit_status" == "Invalid" || "$submit_status" == "Rejected" ]]; then
    echo "::error::notarization: notary rejected the app."
  else
    echo "::error::notarization: notary status ${submit_status}."
  fi
  exit 1
}

notarize_and_staple() {
  local app="$1"
  local status=0 transport_retries=0 staple_attempt=0 staple_status=0
  local max_transport_retries=3
  local max_staple_attempts=6
  local staple_retry_seconds=10
  if [[ "$notary_credential_state" != "ready" || -z "$notary_work" || -n "${MACOS_NOTARY_KEY+x}" ]]; then
    echo "::error::notarization: credential is not ready."
    exit 1
  fi
  echo "notarization: key id ${notary_key_id} issuer ${notary_issuer}"
  ditto -c -k --keepParent "$app" "$notary_zip"
  while true; do
    status=0
    xcrun notarytool submit "$notary_zip" \
      --key "$notary_key_file" \
      --key-id "$notary_key_id" \
      --issuer "$notary_issuer" \
      --wait \
      --timeout 20m \
      --output-format json \
      >"$notary_submit_json" || status=$?
    if parse_notary_submission; then
      echo "notarization: id ${notary_submission_id} status ${notary_submission_status}"
      if [[ "$notary_submission_status" == "Accepted" ]]; then
        break
      fi
      reject_notary_status "$notary_submission_id" "$notary_submission_status"
    fi
    transport_retries=$((transport_retries + 1))
    if [[ "$transport_retries" -gt "$max_transport_retries" ]]; then
      echo "::error::notarization: the upload did not return a status (exit ${status})."
      exit 1
    fi
  done
  rm -f "$notary_zip"
  staple_attempt=1
  while [[ "$staple_attempt" -le "$max_staple_attempts" ]]; do
    staple_status=0
    xcrun stapler staple "$app" || staple_status=$?
    if [[ "$staple_status" -eq 0 ]]; then
      break
    fi
    if [[ "$staple_attempt" -eq "$max_staple_attempts" ]]; then
      echo "::error::notarization: the ticket was not stapled."
      exit 1
    fi
    sleep "$staple_retry_seconds"
    staple_attempt=$((staple_attempt + 1))
  done
  assert_stapled "$app"
  echo "notarization: stapled"
}

assert_stapled() {
  local app="$1"
  local status=0
  xcrun stapler validate "$app" || status=$?
  if [[ "$status" -ne 0 ]]; then
    echo "::error::notarization: stapler validate failed."
    exit 1
  fi
  status=0
  codesign --verify --strict --deep "$app" || status=$?
  if [[ "$status" -ne 0 ]]; then
    echo "::error::notarization: codesign verify failed."
    exit 1
  fi
}

validate_archive() {
  local archive="$1"
  local dir status=0
  dir="$(mktemp -d)"
  notary_extract="$dir"
  COPYFILE_DISABLE=1 tar -xzf "$archive" -C "$dir" || status=$?
  if [[ "$status" -ne 0 ]]; then
    echo "::error::notarization: could not read the archive."
    exit 1
  fi
  if [[ ! -d "$dir/Thinwire.app" ]]; then
    echo "::error::notarization: the archive has no Thinwire.app."
    exit 1
  fi
  read_signature_facts "$dir/Thinwire.app"
  if must_notarize; then
    if notarizable; then
      assert_stapled "$dir/Thinwire.app"
      echo "notarization: validate ok"
      return 0
    fi
    echo "::error::$(notary_failed_facts)"
    exit 1
  fi
  echo "notarization: validate skipped"
}

usage() {
  echo "usage: scripts/notarize-macos-app.sh staple <Thinwire.app>" >&2
  echo "       scripts/notarize-macos-app.sh validate-archive <tar.gz>" >&2
}

main() {
  trap cleanup_notary EXIT
  case "${1:-}" in
    staple)
      if [[ "$#" -ne 2 ]]; then
        usage
        exit 2
      fi
      read_notary_credential
      read_signature_facts "$2"
      decide_notary_action
      case "$notary_decision" in
        skip)
          echo "$notary_decision_message"
          ;;
        staple)
          notarize_and_staple "$2"
          ;;
        fail)
          echo "::error::${notary_decision_message}"
          exit 1
          ;;
        *)
          echo "::error::notarization: no decision."
          exit 1
          ;;
      esac
      ;;
    validate-archive)
      if [[ "$#" -ne 2 ]]; then
        usage
        exit 2
      fi
      validate_archive "$2"
      ;;
    *)
      usage
      exit 2
      ;;
  esac
}

main "$@"
