#!/usr/bin/env bash
set -euo pipefail
set +x

file="${1:?file to sign}"
key_pem="${THINWIRE_UPDATE_KEY:?THINWIRE_UPDATE_KEY is the ed25519 PEM}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$script_dir")"
pub_file="${THINWIRE_UPDATE_PUBLIC_KEY_FILE:-$repo/crates/thinwire/update-public-key.txt}"

tmp=$(mktemp)
chmod 600 "$tmp"
trap 'rm -f "$tmp"' EXIT
printf '%s\n' "$key_pem" >"$tmp"

derived=$(openssl pkey -in "$tmp" -pubout -outform DER | tail -c 32 | openssl base64 -A)
expected=$(tr -d '[:space:]' <"$pub_file")
if [[ "$derived" != "$expected" ]]; then
  echo "::error::The update key does not match update-public-key.txt." >&2
  exit 1
fi

openssl pkeyutl -sign -rawin -inkey "$tmp" -in "$file" | openssl base64 -A
printf '\n'
