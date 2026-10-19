#!/usr/bin/env bash
# Stage dist/, then write one mode-preserving thinwire-$ARTIFACT.tar.gz.
# actions/upload-artifact wraps downloads in an outer zip and stores loose
# files as 644. The tar.gz is what keeps executable bits and the .app tree.
# macOS: Thinwire.app with an ad-hoc signature only (no Developer ID). Linux
# and Windows: flat binaries plus notices.
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
    widestring-LICENSE-APACHE.txt
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
  # Each helper gets the same ad-hoc signature as the app (ADR 0013). It is
  # code inside the bundle, so it gets its signature first.
  for helper_name in "${helper_names[@]}"; do
    codesign --force --sign - "$app/Contents/MacOS/${helper_name}"
  done
  # Ad-hoc signature for the whole bundle, so macOS knows its bundle id.
  # The notification center needs it (#161). It is not a Developer ID
  # signature and there is no notarization (ADR 0003).
  codesign --force --sign - "$app"
  codesign --verify --strict "$app"
  for helper_name in "${helper_names[@]}"; do
    codesign --verify --strict "$app/Contents/MacOS/${helper_name}"
  done
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
    ):
        if not any(name.endswith(f"THIRD_PARTY_NOTICES/{notice}") for name in names):
            raise SystemExit(f"{archive} is missing the notice {notice}")

print(f"archive ok: {norm(binary.name)} mode {oct(mode)}")
PY

echo "Staged payload:"
find dist -print | sort
echo "Archive: ${archive}"
