#!/usr/bin/env bash
# Stage dist/ for actions/upload-artifact.
# That action is the only zip layer. Do not archive this directory first.
# macOS: unsigned Thinwire.app. Linux and Windows: flat binary plus notices.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")" || exit 1

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

if [[ -f target/release/thinwire.exe ]]; then
  cp target/release/thinwire.exe dist/
  bin=dist/thinwire.exe
else
  cp target/release/thinwire dist/
  bin=dist/thinwire
fi

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
  if [[ ! -f "$app/Contents/Resources/LICENSE" || ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt" ]]; then
    echo "::error::Thinwire.app Resources are missing LICENSE or the TDLib notice."
    exit 1
  fi
  leftover="$(find dist -mindepth 1 -maxdepth 1 ! -name 'Thinwire.app' -print)"
  if [[ -n "$leftover" ]]; then
    echo "::error::macOS payload must be only Thinwire.app."
    printf '%s\n' "$leftover"
    exit 1
  fi
fi

echo "Staged payload:"
find dist -print | sort
