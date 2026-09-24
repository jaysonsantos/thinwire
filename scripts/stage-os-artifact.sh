#!/usr/bin/env bash
# Stage dist/, then write one mode-preserving thinwire-$ARTIFACT.tar.gz.
# actions/upload-artifact wraps downloads in an outer zip and stores loose
# files as 644. The tar.gz is what keeps executable bits and the .app tree.
# macOS: unsigned Thinwire.app. Linux and Windows: flat binary plus notices.
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
  if [[ ! -f "$app/Contents/Resources/LICENSE" || ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt" || ! -f "$app/Contents/Resources/THIRD_PARTY_NOTICES/Inter-OFL.txt" ]]; then
    echo "::error::Thinwire.app Resources are missing LICENSE, the TDLib notice, or the Inter OFL."
    exit 1
  fi
  leftover="$(find dist -mindepth 1 -maxdepth 1 ! -name 'Thinwire.app' -print)"
  if [[ -n "$leftover" ]]; then
    echo "::error::macOS payload must be only Thinwire.app."
    printf '%s\n' "$leftover"
    exit 1
  fi
fi

if [[ -f dist/thinwire.exe ]]; then
  leftover="$(find dist -mindepth 1 -maxdepth 1 \
    ! -name 'thinwire.exe' ! -name 'LICENSE' ! -name 'THIRD_PARTY_NOTICES' -print)"
  if [[ -n "$leftover" ]]; then
    echo "::error::Windows payload must be only thinwire.exe, LICENSE, and THIRD_PARTY_NOTICES."
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

print(f"archive ok: {norm(binary.name)} mode {oct(mode)}")
PY

echo "Staged payload:"
find dist -print | sort
echo "Archive: ${archive}"
