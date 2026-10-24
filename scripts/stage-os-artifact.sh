#!/usr/bin/env bash
# Stage dist/, then write one mode-preserving thinwire-$ARTIFACT.tar.gz.
# actions/upload-artifact wraps downloads in an outer zip and stores loose
# files as 644. The tar.gz is what keeps executable bits and the .app tree.
# macOS: sign Thinwire.app with the identity in MACOS_SIGNING_P12 when that
# variable is set. An empty variable keeps the ad-hoc signature. Hardened
# runtime is only for a Developer ID Application identity. There is no
# notarization. Linux and Windows: flat binaries plus notices.
#
# The payload has three programs (ADR 0013): the MIT app, and next to it
# the AGPL-3.0-only WhatsApp helper and the AGPL-3.0-only Signal helper. The
# app does not link a helper. The AGPL text and the offer of the helper
# source ship with each one.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")" || exit 1

if [[ -z "${ARTIFACT:-}" ]]; then
  echo "::error::ARTIFACT is required to name the OS archive."
  exit 1
fi
if [[ ! "$ARTIFACT" =~ ^[a-z0-9_-]+$ ]]; then
  echo "::error::ARTIFACT must be a safe archive name. Got: ${ARTIFACT}"
  exit 1
fi

# Notices land in dist before upload. The tdlib-rs prebuilt
# zip has no license file. This copy is the Boost Software License
# from tdlib/td 11e254af695060d8890024dd7faa1cc2d6685ef8 (1.8.61).
mkdir -p dist/THIRD_PARTY_NOTICES
cp LICENSE dist/LICENSE
tdlib_license=third_party/tdlib/LICENSE_1_0.txt
if [[ ! -f "$tdlib_license" ]]; then
  echo "::error::Missing vendored TDLib Boost license at ${tdlib_license}."
  exit 1
fi
cp "$tdlib_license" dist/THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt
# The binary embeds Inter (SIL OFL 1.1). The OFL must ship with it.
inter_license=crates/thinwire/assets/fonts/OFL.txt
if [[ ! -f "$inter_license" ]]; then
  echo "::error::Missing Inter OFL license at ${inter_license}."
  exit 1
fi
cp "$inter_license" dist/THIRD_PARTY_NOTICES/Inter-OFL.txt
# Chat photos (ADR 0013 decision 8, issue #244). egui_extras and image,
# plus the jpeg, png, and webp decoders those crates link.
copy_app_notice() {
  local notice_source="$1" notice_name="$2"
  if [[ ! -f "$notice_source" ]]; then
    echo "::error::Missing notice at ${notice_source}."
    exit 1
  fi
  cp "$notice_source" "dist/THIRD_PARTY_NOTICES/${notice_name}"
}
copy_app_notice third_party/egui_extras/LICENSE-MIT egui_extras-LICENSE-MIT.txt
copy_app_notice third_party/egui_extras/LICENSE-APACHE egui_extras-LICENSE-APACHE.txt
copy_app_notice third_party/image/LICENSE-MIT image-LICENSE-MIT.txt
copy_app_notice third_party/image/LICENSE-APACHE image-LICENSE-APACHE.txt
copy_app_notice third_party/png/LICENSE-MIT png-LICENSE-MIT.txt
copy_app_notice third_party/png/LICENSE-APACHE png-LICENSE-APACHE.txt
copy_app_notice third_party/image-webp/LICENSE-MIT image-webp-LICENSE-MIT.txt
copy_app_notice third_party/image-webp/LICENSE-APACHE image-webp-LICENSE-APACHE.txt
copy_app_notice third_party/zune-jpeg/LICENSE-MIT zune-jpeg-LICENSE-MIT.txt
copy_app_notice third_party/zune-jpeg/LICENSE-APACHE zune-jpeg-LICENSE-APACHE.txt
copy_app_notice third_party/zune-jpeg/LICENSE-ZLIB zune-jpeg-LICENSE-ZLIB.txt
copy_app_notice third_party/zune-core/LICENSE-MIT zune-core-LICENSE-MIT.txt
copy_app_notice third_party/zune-core/LICENSE-APACHE zune-core-LICENSE-APACHE.txt
copy_app_notice third_party/zune-core/LICENSE-ZLIB zune-core-LICENSE-ZLIB.txt
# A second launch on Windows (#237). Unix uses the standard library.
copy_app_notice third_party/interprocess/LICENSE-0BSD.txt interprocess-LICENSE-0BSD.txt
copy_app_notice third_party/interprocess/LICENSE-APACHE.txt interprocess-LICENSE-APACHE.txt
copy_app_notice third_party/widestring/LICENSE-MIT widestring-LICENSE-MIT.txt
copy_app_notice third_party/widestring/LICENSE-APACHE widestring-LICENSE-APACHE.txt
copy_app_notice third_party/webbrowser/LICENSE-MIT webbrowser-LICENSE-MIT.txt
copy_app_notice third_party/webbrowser/LICENSE-APACHE webbrowser-LICENSE-APACHE.txt

copy_llvm_notice() {
  local lib="$1"
  local owner=""
  local pkg=""
  local doc=""
  local real=""
  owner=$(dpkg -S "$lib" 2>/dev/null || true)
  owner=${owner%%$'\n'*}
  if [[ -z "$owner" ]]; then
    real=$(readlink -f "$lib")
    owner=$(dpkg -S "$real" 2>/dev/null || true)
    owner=${owner%%$'\n'*}
  fi
  if [[ -z "$owner" ]]; then
    echo "::error::No package owns ${lib}; refusing to ship it without its redistribution notice."
    exit 1
  fi
  pkg=${owner%%: /*}
  pkg=${pkg%%:*}
  doc="/usr/share/doc/${pkg}/copyright"
  if [[ ! -f "$doc" ]]; then
    echo "::error::Redistribution notice missing at ${doc} for ${lib}."
    exit 1
  fi
  cp "$doc" "dist/THIRD_PARTY_NOTICES/${pkg}.copyright"
  # Debian copyright defers the Apache 2.0 text to this path.
  # The artifact is not a Debian system, so ship the license itself.
  if grep -q "/usr/share/common-licenses/Apache-2.0" "$doc"; then
    local apache=/usr/share/common-licenses/Apache-2.0
    if [[ ! -f "$apache" ]]; then
      echo "::error::LLVM copyright at ${doc} points at ${apache}, which is missing."
      exit 1
    fi
    cp "$apache" dist/THIRD_PARTY_NOTICES/Apache-2.0
  fi
}

workspace_version() {
  awk '
    $0 == "[workspace.package]" { in_pkg = 1; next }
    in_pkg && /^\[/ { exit }
    in_pkg && $1 == "version" && $2 == "=" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' Cargo.toml
}

# THINWIRE_BIN: stage another binary. Pull-request CI stages a macOS debug
# build with it, so the bundle signature step runs before main (#161).
if [[ -n "${THINWIRE_BIN:-}" ]]; then
  cp "$THINWIRE_BIN" dist/thinwire
  bin=dist/thinwire
elif [[ -f target/release/thinwire.exe ]]; then
  cp target/release/thinwire.exe dist/
  bin=dist/thinwire.exe
else
  cp target/release/thinwire dist/
  bin=dist/thinwire
fi

# The AGPL helpers, their license, their source offers, and the notices of
# the protocol code (ADR 0013). The same script runs in pull-request CI.
helper_names=(thinwire-whatsapp-helper thinwire-signal-helper)
"$script_dir/stage-helper.sh" dist "$bin"

if [[ "$(uname -s)" == "Linux" ]]; then
  # Static TDLib still loads the LLVM C++ runtime. Ship those sonames
  # beside the binary. Do not copy libc or other system loaders.
  copied=0
  while IFS= read -r lib; do
    if [[ -z "$lib" || ! -f "$lib" ]]; then
      echo "::error::Linux payload is missing a TDLib C++ runtime library. Refusing to upload a binary that cannot load TDLib."
      exit 1
    fi
    so="dist/$(basename "$lib")"
    cp -L "$lib" "$so"
    # $ORIGIN is a dynamic-linker token. libc++abi and libunwind resolve
    # from it whether patchelf writes DT_RPATH or DT_RUNPATH.
    # shellcheck disable=SC2016
    patchelf --set-rpath '$ORIGIN' "$so"
    copy_llvm_notice "$lib"
    copied=$((copied + 1))
  done < <(ldd "$bin" | awk '/libc\+\+\.so|libc\+\+abi\.so|libunwind\.so/ { print $3 }')
  if [[ "$copied" -lt 1 ]]; then
    echo "::error::Linux payload did not find the LLVM C++ runtime required by static TDLib. Refusing to upload."
    exit 1
  fi
  # shellcheck disable=SC2016
  patchelf --set-rpath '$ORIGIN' "$bin"
fi

# Temporary keychain used when MACOS_SIGNING_P12 is set. The EXIT trap
# below deletes the decoded p12 and the keychain, and restores the search
# list. It is a no-op until import creates those files.
signing_p12_file=""
signing_keychain=""
signing_keychain_dir=""
signing_keychain_password=""
signing_search_saved=0
signing_search_list=()
signing_xtrace=0
sign_identity=""
# 1 only for Developer ID Application. A self-signed identity has no Team ID.
sign_use_hardened_runtime=0
identity_hashes=()
identity_names=()
identity_subject_dn=""
identity_issuer_dn=""
identity_kind_result=""

cleanup_macos_signing() {
  if [[ -n "$signing_keychain" ]]; then
    security delete-keychain "$signing_keychain" >/dev/null 2>&1 || true
    signing_keychain=""
  fi
  if [[ -n "$signing_p12_file" ]]; then
    rm -f "$signing_p12_file"
    signing_p12_file=""
  fi
  if [[ -n "$signing_keychain_dir" ]]; then
    rm -rf "$signing_keychain_dir"
    signing_keychain_dir=""
  fi
  signing_keychain_password=""
  if [[ "$signing_search_saved" -eq 1 ]]; then
    signing_search_saved=0
    if [[ ${#signing_search_list[@]} -gt 0 ]]; then
      security list-keychains -d user -s "${signing_search_list[@]}" >/dev/null 2>&1 || true
    fi
  fi
}

# Install cleanup before import. A failure in security list-keychains
# still deletes the temporary keychain and the decoded certificate.
trap cleanup_macos_signing EXIT

trim_spaces() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

silence_xtrace() {
  signing_xtrace=0
  case $- in
    *x*)
      signing_xtrace=1
      set +x
      ;;
  esac
}

restore_xtrace() {
  if [[ "$signing_xtrace" -eq 1 ]]; then
    set -x
  fi
}

save_keychain_search_list() {
  local line trimmed
  signing_search_list=()
  while IFS= read -r line; do
    trimmed="$(trim_spaces "$line")"
    case "$trimmed" in
      \"*\")
        trimmed="${trimmed#\"}"
        trimmed="${trimmed%\"}"
        ;;
    esac
    if [[ -n "$trimmed" ]]; then
      signing_search_list+=("$trimmed")
    fi
  done < <(security list-keychains -d user)
  if [[ ${#signing_search_list[@]} -eq 0 ]]; then
    echo "::error::The user keychain search list is empty. Refusing to replace it."
    exit 1
  fi
  signing_search_saved=1
}

is_sha1() {
  local value="$1"
  [[ ${#value} -eq 40 ]] || return 1
  case "$value" in
    *[!0-9A-Fa-f]*) return 1 ;;
  esac
  return 0
}

lower_hex() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

# Read `security find-identity -p codesigning` on stdin.
# Print one matching identity per line: hash<TAB>name.
# Ignore the valid-only section. A self-signed certificate is not trusted
# in a new keychain, so that section can list zero identities.
parse_matching_identities() {
  local line in_matching=0 rest hash name
  in_matching=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      *"Matching identities"*)
        in_matching=1
        continue
        ;;
      *"Valid identities only"*)
        break
        ;;
    esac
    [[ "$in_matching" -eq 1 ]] || continue
    rest="$(trim_spaces "$line")"
    case "$rest" in
      [0-9]*) ;;
      *) continue ;;
    esac
    case "$rest" in
      *")"*) ;;
      *) continue ;;
    esac
    rest="${rest#*\)}"
    rest="$(trim_spaces "$rest")"
    hash="${rest%%[[:space:]]*}"
    if ! is_sha1 "$hash"; then
      continue
    fi
    rest="${rest#"$hash"}"
    rest="$(trim_spaces "$rest")"
    case "$rest" in
      \"*) ;;
      *) continue ;;
    esac
    rest="${rest#\"}"
    name="${rest%%\"*}"
    if [[ -z "$name" ]]; then
      continue
    fi
    printf '%s\t%s\n' "$hash" "$name"
  done
}

load_matching_identities() {
  local report line hash name
  identity_hashes=()
  identity_names=()
  report="$(mktemp)"
  # Do not pass the valid-only flag. See parse_matching_identities.
  security find-identity -p codesigning "$signing_keychain" >"$report"
  while IFS= read -r line; do
    hash="${line%%$'\t'*}"
    name="${line#*$'\t'}"
    if [[ -z "$hash" || -z "$name" ]]; then
      continue
    fi
    identity_hashes+=("$hash")
    identity_names+=("$name")
  done < <(parse_matching_identities <"$report")
  rm -f "$report"
}

# Set identity_kind_result to Developer ID Application, self-signed, or unknown.
# Name prefix wins. Otherwise compare certificate subject and issuer.
# Writes globals. Do not call this function inside a command substitution.
identity_kind() {
  local hash="$1" name="$2"
  local pem line current want_lc in_pem found subject issuer
  identity_subject_dn=""
  identity_issuer_dn=""
  identity_kind_result="unknown"
  case "$name" in
    "Developer ID Application:"*)
      identity_kind_result="Developer ID Application"
      return 0
      ;;
  esac
  pem="$(mktemp)"
  : >"$pem"
  current=""
  in_pem=0
  found=0
  want_lc="$(lower_hex "$hash")"
  while IFS= read -r line; do
    case "$line" in
      "SHA-1 hash: "*)
        current="$(trim_spaces "${line#"SHA-1 hash: "}")"
        current="$(lower_hex "$current")"
        : >"$pem"
        in_pem=0
        ;;
      "-----BEGIN CERTIFICATE-----")
        in_pem=1
        printf '%s\n' "$line" >>"$pem"
        ;;
      "-----END CERTIFICATE-----")
        if [[ "$in_pem" -eq 1 ]]; then
          printf '%s\n' "$line" >>"$pem"
        fi
        in_pem=0
        if [[ -n "$current" && "$current" == "$want_lc" ]]; then
          found=1
          break
        fi
        ;;
      *)
        if [[ "$in_pem" -eq 1 ]]; then
          printf '%s\n' "$line" >>"$pem"
        fi
        ;;
    esac
  done < <(security find-certificate -a -Z -p "$signing_keychain")
  if [[ "$found" -ne 1 ]]; then
    rm -f "$pem"
    identity_kind_result="unknown"
    return 0
  fi
  subject=""
  issuer=""
  if openssl x509 -in "$pem" -noout -subject -issuer -nameopt RFC2253 >"${pem}.dn" 2>/dev/null; then
    {
      IFS= read -r subject
      IFS= read -r issuer
    } <"${pem}.dn"
  fi
  rm -f "$pem" "${pem}.dn"
  identity_subject_dn="$(trim_spaces "${subject#subject=}")"
  identity_issuer_dn="$(trim_spaces "${issuer#issuer=}")"
  case "$identity_subject_dn" in
    *"Developer ID Application"*)
      identity_kind_result="Developer ID Application"
      return 0
      ;;
  esac
  if [[ -n "$identity_subject_dn" && "$identity_subject_dn" == "$identity_issuer_dn" ]]; then
    identity_kind_result="self-signed"
    return 0
  fi
  identity_kind_result="unknown"
}

log_parsed_identities() {
  local index=0
  while [[ "$index" -lt ${#identity_hashes[@]} ]]; do
    echo "codesign identity: ${identity_hashes[$index]} \"${identity_names[$index]}\""
    index=$((index + 1))
  done
}

import_macos_signing_identity() {
  local decoded_ok=0 count index chosen hash name kind want_lc
  save_keychain_search_list

  signing_p12_file="$(mktemp)"
  chmod 600 "$signing_p12_file"
  silence_xtrace
  if printf '%s' "$MACOS_SIGNING_P12" | tr -d '[:space:]' | base64 -D >"$signing_p12_file"; then
    decoded_ok=1
  fi
  restore_xtrace
  if [[ "$decoded_ok" -ne 1 || ! -s "$signing_p12_file" ]]; then
    echo "::error::MACOS_SIGNING_P12 is empty or is not valid base64."
    exit 1
  fi

  signing_keychain_password="$(openssl rand -base64 32)"
  signing_keychain_dir="$(mktemp -d)"
  signing_keychain="${signing_keychain_dir}/signing.keychain-db"
  silence_xtrace
  if ! security create-keychain -p "$signing_keychain_password" "$signing_keychain"; then
    restore_xtrace
    echo "::error::Could not create the temporary signing keychain."
    exit 1
  fi
  if ! security unlock-keychain -p "$signing_keychain_password" "$signing_keychain"; then
    restore_xtrace
    echo "::error::Could not unlock the temporary signing keychain."
    exit 1
  fi
  restore_xtrace
  chmod 600 "$signing_keychain"
  # Keep the keychain unlocked for the rest of this step.
  security set-keychain-settings -lut 21600 "$signing_keychain"
  security list-keychains -d user -s "$signing_keychain" "${signing_search_list[@]}" >/dev/null

  silence_xtrace
  if ! security import "$signing_p12_file" \
    -k "$signing_keychain" \
    -P "${MACOS_SIGNING_P12_PASSWORD-}" \
    -T /usr/bin/codesign \
    -f pkcs12; then
    restore_xtrace
    echo "::error::Could not import MACOS_SIGNING_P12 into the temporary keychain."
    exit 1
  fi
  if ! security set-key-partition-list -S apple-tool:,apple: -s \
    -k "$signing_keychain_password" "$signing_keychain"; then
    restore_xtrace
    echo "::error::Could not allow codesign to use the temporary keychain."
    exit 1
  fi
  restore_xtrace
  rm -f "$signing_p12_file"
  signing_p12_file=""
  signing_keychain_password=""
  unset MACOS_SIGNING_P12
  unset MACOS_SIGNING_P12_PASSWORD

  load_matching_identities
  count="${#identity_hashes[@]}"
  if [[ "$count" -eq 0 ]]; then
    echo "::error::The temporary keychain has no code signing identity."
    exit 1
  fi

  chosen=-1
  if [[ -n "${MACOS_SIGNING_IDENTITY:-}" ]]; then
    sign_identity="$MACOS_SIGNING_IDENTITY"
    want_lc="$(lower_hex "$MACOS_SIGNING_IDENTITY")"
    index=0
    while [[ "$index" -lt "$count" ]]; do
      if [[ "$(lower_hex "${identity_hashes[$index]}")" == "$want_lc" || "${identity_names[$index]}" == "$MACOS_SIGNING_IDENTITY" ]]; then
        chosen=$index
        break
      fi
      index=$((index + 1))
    done
  elif [[ "$count" -ne 1 ]]; then
    echo "::error::Expected exactly one code signing identity in the temporary keychain. Found ${count}."
    log_parsed_identities
    echo "Set MACOS_SIGNING_IDENTITY to one hash, or import a p12 that contains one identity."
    exit 1
  else
    chosen=0
    sign_identity="${identity_hashes[0]}"
  fi

  sign_use_hardened_runtime=0
  if [[ "$chosen" -ge 0 ]]; then
    hash="${identity_hashes[$chosen]}"
    name="${identity_names[$chosen]}"
    identity_kind "$hash" "$name"
    kind="$identity_kind_result"
    echo "codesign identity: ${hash} \"${name}\" (${kind})"
    if [[ "$kind" == "unknown" && -n "$identity_subject_dn" ]]; then
      echo "codesign identity subject: ${identity_subject_dn}"
      echo "codesign identity issuer: ${identity_issuer_dn}"
    fi
    if [[ "$kind" == "Developer ID Application" ]]; then
      sign_use_hardened_runtime=1
    fi
  else
    echo "::warning::MACOS_SIGNING_IDENTITY did not match a hash or name in the temporary keychain."
    log_parsed_identities
    kind="unmatched identity"
  fi
  if [[ "$sign_use_hardened_runtime" -eq 1 ]]; then
    echo "codesign hardened runtime: on (Developer ID Application)"
  else
    echo "codesign hardened runtime: off (${kind})"
  fi
  if [[ -n "${MACOS_SIGNING_IDENTITY:-}" ]]; then
    echo "codesign sign argument: ${MACOS_SIGNING_IDENTITY}"
  fi
}

macho_file() {
  local path="$1" magic
  [[ -f "$path" ]] || return 1
  magic="$(od -An -t x1 -N 4 "$path" 2>/dev/null | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')"
  case "$magic" in
    feedface | feedfacf | cefaedfe | cffaedfe | cafebabe | bebafeca) return 0 ;;
    *) return 1 ;;
  esac
}

# Sign one bundle or Mach-O with the stable identity. Keep an existing
# entitlement plist when the target already has one.
# Hardened runtime checks the Team ID of every library. A self-signed
# identity has no Team ID, so dlopen of a bundled libtdjson fails.
# --options runtime is only for Developer ID Application.
sign_with_identity() {
  local target="$1"
  local entitlements_from="${2:-$1}"
  local entitlements status
  local -a args
  entitlements="$(mktemp)"
  args=(--force)
  if [[ "$sign_use_hardened_runtime" -eq 1 ]]; then
    args+=(--options runtime)
  fi
  args+=(--timestamp=none --sign "$sign_identity")
  if codesign -d --entitlements :- "$entitlements_from" >"$entitlements" 2>/dev/null \
    && grep -q '<plist' "$entitlements"; then
    args+=(--entitlements "$entitlements")
  fi
  echo "codesign: ${target}"
  status=0
  # Keep the codesign status. A negated command in an if-test stores 0.
  # Delete the entitlements file before exit. The EXIT trap does not cover it.
  codesign "${args[@]}" "$target" || status=$?
  rm -f "$entitlements"
  if [[ "$status" -ne 0 ]]; then
    exit "$status"
  fi
}

# Deepest Mach-O first. The bundle itself is not a file, so it is signed
# by the caller after this function returns.
sign_nested_machos() {
  local app="$1" mode="$2"
  local list depth path
  list="$(mktemp)"
  while IFS= read -r -d '' path; do
    if macho_file "$path"; then
      depth="$(printf '%s' "$path" | awk -F/ '{print NF}')"
      printf '%s\t%s\n' "$depth" "$path" >>"$list"
    fi
  done < <(find "$app" -type f -print0)
  while IFS=$'\t' read -r depth path; do
    if [[ -z "$path" ]]; then
      continue
    fi
    if [[ "$mode" == "identity" ]]; then
      sign_with_identity "$path"
    else
      echo "codesign: ${path}"
      codesign --force --sign - "$path"
    fi
  done < <(sort -nr "$list")
  rm -f "$list"
}

sign_macos_app() {
  local app="$1"
  if [[ -n "${MACOS_SIGNING_P12:-}" ]]; then
    echo "macOS signing: stable identity"
    import_macos_signing_identity
    sign_nested_machos "$app" identity
    sign_with_identity "$app" "$app/Contents/MacOS/thinwire"
  else
    echo "macOS signing: ad-hoc signature (MACOS_SIGNING_P12 is empty)"
    sign_nested_machos "$app" adhoc
    # Ad-hoc signature for the whole bundle. The notification center needs
    # it (#161). It is not a Developer ID signature and there is no
    # notarization (ADR 0003).
    codesign --force --sign - "$app"
  fi
  codesign --verify --strict --deep --verbose=2 "$app"
  codesign -dvv "$app" 2>&1
}

if [[ "$(uname -s)" == "Darwin" ]]; then
  version="$(workspace_version)"
  if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "::error::Workspace package version must look like 1.2.3 (Cargo.toml [workspace.package]). Got: ${version:-empty}"
    exit 1
  fi
  app=dist/Thinwire.app
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  mv dist/thinwire "$app/Contents/MacOS/thinwire"
  chmod 755 "$app/Contents/MacOS/thinwire"
  # The app looks for each helper next to its own binary (ADR 0013).
  for helper_name in "${helper_names[@]}"; do
    mv "dist/${helper_name}" "$app/Contents/MacOS/${helper_name}"
    chmod 755 "$app/Contents/MacOS/${helper_name}"
  done
  mv dist/LICENSE "$app/Contents/Resources/LICENSE"
  mv dist/THIRD_PARTY_NOTICES "$app/Contents/Resources/THIRD_PARTY_NOTICES"
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
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${version}</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
EOF
  if [[ ! -s "$app/Contents/MacOS/thinwire" || ! -x "$app/Contents/MacOS/thinwire" ]]; then
    echo "::error::Thinwire.app is missing an executable Contents/MacOS/thinwire."
    exit 1
  fi
  if [[ ! -f "$app/Contents/Resources/LICENSE" || ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt" || ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/Inter-OFL.txt" ]]; then
    echo "::error::Thinwire.app Resources are missing LICENSE, the TDLib notice, or the Inter OFL."
    exit 1
  fi
  for notice in \
    egui_extras-LICENSE-MIT.txt \
    egui_extras-LICENSE-APACHE.txt \
    image-LICENSE-MIT.txt \
    image-LICENSE-APACHE.txt \
    png-LICENSE-MIT.txt \
    png-LICENSE-APACHE.txt \
    image-webp-LICENSE-MIT.txt \
    image-webp-LICENSE-APACHE.txt \
    zune-jpeg-LICENSE-MIT.txt \
    zune-jpeg-LICENSE-APACHE.txt \
    zune-jpeg-LICENSE-ZLIB.txt \
    zune-core-LICENSE-MIT.txt \
    zune-core-LICENSE-APACHE.txt \
    zune-core-LICENSE-ZLIB.txt \
    interprocess-LICENSE-0BSD.txt \
    interprocess-LICENSE-APACHE.txt \
    widestring-LICENSE-MIT.txt \
    widestring-LICENSE-APACHE.txt \
    webbrowser-LICENSE-MIT.txt \
    webbrowser-LICENSE-APACHE.txt
  do
    if [[ ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/${notice}" ]]; then
      echo "::error::Thinwire.app Resources are missing the image notice ${notice}."
      exit 1
    fi
  done
  for helper_name in "${helper_names[@]}"; do
    if [[ ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/${helper_name}-AGPL-3.0.txt" || ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/${helper_name}-SOURCE.md" ]]; then
      echo "::error::Thinwire.app Resources are missing the AGPL text or the source offer of ${helper_name}."
      exit 1
    fi
  done
  leftover="$(find dist -mindepth 1 -maxdepth 1 ! -name 'Thinwire.app' -print)"
  if [[ -n "$leftover" ]]; then
    echo "::error::macOS payload must be only Thinwire.app."
    printf '%s\n' "$leftover"
    exit 1
  fi
  # Each helper gets the same signature as the app (ADR 0013). Nested
  # Mach-O files are signed from the inside out, then the bundle. --deep
  # is only used on the verify inside sign_macos_app.
  sign_macos_app "$app"
fi

if [[ -f dist/thinwire.exe ]]; then
  leftover="$(find dist -mindepth 1 -maxdepth 1 \
    ! -name 'thinwire.exe' ! -name "${helper_names[0]}.exe" ! -name "${helper_names[1]}.exe" \
    ! -name 'LICENSE' ! -name 'THIRD_PARTY_NOTICES' -print)"
  if [[ -n "$leftover" ]]; then
    echo "::error::Windows payload must be only thinwire.exe, ${helper_names[0]}.exe, ${helper_names[1]}.exe, LICENSE, and THIRD_PARTY_NOTICES."
    printf '%s\n' "$leftover"
    exit 1
  fi
fi

if [[ -f dist/thinwire ]]; then
  chmod 755 dist/thinwire
fi
if [[ -f dist/thinwire.exe ]]; then
  chmod 755 dist/thinwire.exe
fi
if [[ -f dist/Thinwire.app/Contents/MacOS/thinwire ]]; then
  chmod 755 dist/Thinwire.app/Contents/MacOS/thinwire
fi
for so in dist/*.so dist/*.so.*; do
  if [[ -f "$so" ]]; then
    chmod 755 "$so"
  fi
done

archive="thinwire-${ARTIFACT}.tar.gz"
rm -f "$archive"
# macOS tar adds AppleDouble files unless this is set. Other tar ignores it.
COPYFILE_DISABLE=1 tar -czf "$archive" -C dist .
if [[ ! -s "$archive" ]]; then
  echo "::error::Failed to build ${archive}."
  exit 1
fi
if command -v python3 >/dev/null 2>&1; then
  py=python3
elif command -v python >/dev/null 2>&1; then
  py=python
else
  echo "::error::Need python3 or python to verify ${archive} keeps executable bits."
  exit 1
fi
"$py" - "$archive" <<'PY'
import sys
import tarfile

archive = sys.argv[1]
with tarfile.open(archive, "r:gz") as tf:
    members = [m for m in tf.getmembers() if m.name not in {".", "./"}]
    if not members:
        raise SystemExit(f"{archive} is empty")

    def norm(name: str) -> str:
        return name[2:] if name.startswith("./") else name

    names = [norm(m.name) for m in members]
    if any(name == "dist" or name.startswith("dist/") for name in names):
        raise SystemExit(f"{archive} nests dist/; upload the payload itself")

    binaries = []
    for member in members:
        name = norm(member.name)
        base = name.rsplit("/", 1)[-1]
        if member.isfile() and base in {"thinwire", "thinwire.exe"}:
            binaries.append(member)
    if len(binaries) != 1:
        found = [norm(m.name) for m in binaries]
        raise SystemExit(f"{archive} expected one thinwire binary, found {found}")

    binary = binaries[0]
    mode = binary.mode & 0o777
    if mode != 0o755:
        raise SystemExit(
            f"{norm(binary.name)} mode is {oct(mode)}; executable bit missing"
        )

    for member in members:
        name = norm(member.name)
        base = name.rsplit("/", 1)[-1]
        if member.isfile() and ".so" in base and (member.mode & 0o777) != 0o755:
            got = oct(member.mode & 0o777)
            raise SystemExit(f"{name} mode is {got}; executable bit missing")

    has_license = any(name == "LICENSE" or name.endswith("/LICENSE") for name in names)
    has_notice = any(name.endswith("tdlib-LICENSE_1_0.txt") for name in names)
    has_font_notice = any(name.endswith("Inter-OFL.txt") for name in names)
    if not has_license or not has_notice or not has_font_notice:
        raise SystemExit(f"{archive} is missing LICENSE, the TDLib notice, or the Inter OFL")

    # ADR 0013: one WhatsApp helper and one Signal helper next to the app
    # binary, each with the AGPL text and the source offer, and the notices
    # of the protocol code.
    binary_dir = norm(binary.name).rpartition("/")[0]
    for helper_name in ("thinwire-whatsapp-helper", "thinwire-signal-helper"):
        helpers = [
            m for m in members
            if m.isfile()
            and norm(m.name).rsplit("/", 1)[-1] in {helper_name, f"{helper_name}.exe"}
        ]
        if len(helpers) != 1:
            found = [norm(m.name) for m in helpers]
            raise SystemExit(f"{archive} expected one {helper_name}, found {found}")
        helper = helpers[0]
        if (helper.mode & 0o777) != 0o755:
            got = oct(helper.mode & 0o777)
            raise SystemExit(f"{norm(helper.name)} mode is {got}; executable bit missing")
        helper_dir = norm(helper.name).rpartition("/")[0]
        if helper_dir != binary_dir:
            raise SystemExit(
                f"{helper_name} is in '{helper_dir}', the app binary in '{binary_dir}'; "
                "the app looks for a helper next to its own binary"
            )
    for notice in (
        "thinwire-whatsapp-helper-AGPL-3.0.txt",
        "thinwire-whatsapp-helper-SOURCE.md",
        "thinwire-signal-helper-AGPL-3.0.txt",
        "thinwire-signal-helper-SOURCE.md",
        "whatsapp-rust-LICENSE-MIT.txt",
        "tdlib-rs-LICENSE-APACHE.txt",
        "twilight-LICENSE-ISC.md",
        "egui_extras-LICENSE-MIT.txt",
        "egui_extras-LICENSE-APACHE.txt",
        "image-LICENSE-MIT.txt",
        "image-LICENSE-APACHE.txt",
        "png-LICENSE-MIT.txt",
        "png-LICENSE-APACHE.txt",
        "image-webp-LICENSE-MIT.txt",
        "image-webp-LICENSE-APACHE.txt",
        "zune-jpeg-LICENSE-MIT.txt",
        "zune-jpeg-LICENSE-APACHE.txt",
        "zune-jpeg-LICENSE-ZLIB.txt",
        "zune-core-LICENSE-MIT.txt",
        "zune-core-LICENSE-APACHE.txt",
        "zune-core-LICENSE-ZLIB.txt",
        "interprocess-LICENSE-0BSD.txt",
        "interprocess-LICENSE-APACHE.txt",
        "widestring-LICENSE-MIT.txt",
        "widestring-LICENSE-APACHE.txt",
        "webbrowser-LICENSE-MIT.txt",
        "webbrowser-LICENSE-APACHE.txt",
    ):
        if not any(name.endswith(f"THIRD_PARTY_NOTICES/{notice}") for name in names):
            raise SystemExit(f"{archive} is missing the notice {notice}")

print(f"archive ok: {norm(binary.name)} mode {oct(mode)}")
PY

echo "Staged payload:"
find dist -print | sort
echo "Archive: ${archive}"
