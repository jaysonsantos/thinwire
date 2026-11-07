#!/usr/bin/env bash
# Publish the OS archives of one main commit on the rolling `nightly` release.
# The `nightly` job of os-zips.yml runs this after the archives and the helper
# source are built. It is a pre-release and never the latest release.
#
# Each archive gets the version in its name, for example
# thinwire-linux-x86_64-0.1.0.r812.gabc1234.tar.gz: the workspace version, the
# commit count of main, and the short commit. The count goes up on every push,
# so a package manager sorts the versions in push order. VERSION holds the
# version of the newest archives and SHA256SUMS their checksums.
#
# The AUR package updates once a day and names one versioned archive, so an
# archive stays on the release for KEEP_DAYS days before this script deletes it.
#
# Needs: DIST (the downloaded artifacts), SHA, GITHUB_REPOSITORY, GH_TOKEN.
# THINWIRE_UPDATE_KEY signs appcast.xml and the macOS tar. Without it the
# archives still upload, then this script exits 1 and does not upload an
# unsigned appcast. appcast.xml and appcast.ed25519 stay on the tag the way
# VERSION does. The macOS tar's BUILD_ID must match the version string.
set -euo pipefail

TAG=nightly
KEEP_DAYS=3

dist="${DIST:?DIST is the folder of the downloaded artifacts}"
sha="${SHA:?SHA is the commit of the archives}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY names the repository}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$script_dir")" || exit 1

workspace_version=$(awk '
  $0 == "[workspace.package]" { in_pkg = 1; next }
  in_pkg && /^\[/ { exit }
  in_pkg && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' Cargo.toml)
if [[ ! "$workspace_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "::error::No workspace version in Cargo.toml. Got: ${workspace_version}"
  exit 1
fi
count=$(git rev-list --count "$sha")
version="${workspace_version}.r${count}.g${sha:0:7}"

# A run of an older commit never replaces a newer nightly.
current=$(curl -fsSL "https://github.com/${repo}/releases/download/${TAG}/VERSION" 2>/dev/null || true)
if [[ "$current" =~ \.r([0-9]+)\.g ]] && ((BASH_REMATCH[1] >= count)); then
  echo "The nightly is ${current}, which is not older than ${version}. Nothing to publish."
  exit 0
fi

shopt -s nullglob
archives=("$dist"/thinwire-*.tar.gz)
if ((${#archives[@]} == 0)); then
  echo "::error::No archives in ${dist}."
  exit 1
fi
for archive in "${archives[@]}"; do
  name=$(basename "$archive" .tar.gz)
  case "$name" in
    thinwire-helpers-*-source) ;; # The source tarball already names its commit.
    *) mv "$archive" "${dist}/${name}-${version}.tar.gz" ;;
  esac
done
(cd "$dist" && sha256sum -- thinwire-*.tar.gz >SHA256SUMS && printf '%s\n' "$version" >VERSION)

xml_escape() {
  local s="$1"
  s=${s//&/&amp;}
  s=${s//</&lt;}
  s=${s//>/&gt;}
  s=${s//\"/&quot;}
  printf '%s' "$s"
}

write_appcast() {
  local mac_tar="$1"
  local subject clean description semver url length tar_sig
  subject=$(git log -1 --format=%s "$sha")
  clean=$(printf '%s' "$subject" | tr -d '\000-\010\013\014\016-\037')
  description=$(xml_escape "${clean} This is a build from main, not a release.")
  semver=$workspace_version
  url="https://github.com/${repo}/releases/download/${TAG}/thinwire-macos-arm64-${version}.tar.gz"
  length=$(wc -c <"$mac_tar" | tr -d ' ')
  set +x
  tar_sig=$(THINWIRE_UPDATE_KEY="$THINWIRE_UPDATE_KEY" "$script_dir/sign-update.sh" "$mac_tar" | tr -d '\n')
  cat >"$dist/appcast.xml" <<EOF
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" xmlns:thinwire="https://github.com/jaysonsantos/thinwire">
<channel>
<item>
<title>${version}</title>
<sparkle:version>${semver}</sparkle:version>
<sparkle:shortVersionString>${semver}</sparkle:shortVersionString>
<thinwire:build>${version}</thinwire:build>
<description>${description}</description>
<enclosure url="${url}" sparkle:edSignature="${tar_sig}" sparkle:os="macos" length="${length}" type="application/gzip"/>
</item>
</channel>
</rss>
EOF
  THINWIRE_UPDATE_KEY="$THINWIRE_UPDATE_KEY" "$script_dir/sign-update.sh" "$dist/appcast.xml" >"$dist/appcast.ed25519"
}

mac_tar="${dist}/thinwire-macos-arm64-${version}.tar.gz"
appcast_ok=0
build_id=""
member=""
if [[ ! -f "$mac_tar" ]]; then
  echo "::error::No macOS archive for ${version}."
else
  member=$(tar -tzf "$mac_tar" | grep -E '(^|/)Thinwire\.app/Contents/Resources/BUILD_ID$' | head -n 1 || true)
  if [[ -z "$member" ]]; then
    echo "::error::The macOS archive has no BUILD_ID."
  else
    build_id=$(tar -xOf "$mac_tar" "$member" | tr -d '\r' | head -n 1)
    if [[ "$build_id" != "$version" ]]; then
      echo "::error::BUILD_ID is ${build_id:-empty}, nightly version is ${version}."
    elif [[ -z "${THINWIRE_UPDATE_KEY:-}" ]]; then
      echo "::error::THINWIRE_UPDATE_KEY is not set. No appcast will be uploaded."
    else
      write_appcast "$mac_tar"
      appcast_ok=1
    fi
  fi
fi

notes="Build of ${sha} on main, version ${version}. This is not a release.

Each archive stays here for ${KEEP_DAYS} days. VERSION and SHA256SUMS describe the newest archives. appcast.xml and appcast.ed25519 are the macOS update feed. They stay on this tag."

if gh release view "$TAG" --repo "$repo" >/dev/null 2>&1; then
  gh api --method PATCH "repos/${repo}/git/refs/tags/${TAG}" -f sha="$sha" -F force=true >/dev/null
  gh release edit "$TAG" --repo "$repo" --title "Nightly ${version}" --notes "$notes" --prerelease --latest=false
else
  gh release create "$TAG" --repo "$repo" --target "$sha" --title "Nightly ${version}" --notes "$notes" \
    --prerelease --latest=false
fi
upload=("$dist"/thinwire-*.tar.gz "$dist/SHA256SUMS" "$dist/VERSION")
if [[ "$appcast_ok" -eq 1 ]]; then
  upload+=("$dist/appcast.xml" "$dist/appcast.ed25519")
fi
gh release upload "$TAG" --repo "$repo" --clobber "${upload[@]}"

cutoff=$(date -u -d "-${KEEP_DAYS} days" +%Y-%m-%dT%H:%M:%SZ)
gh api "repos/${repo}/releases/tags/${TAG}" \
  --jq ".assets[] | select(.name | endswith(\"${version}.tar.gz\") or . == \"SHA256SUMS\" or . == \"VERSION\" or . == \"appcast.xml\" or . == \"appcast.ed25519\" | not) | select(.created_at < \"${cutoff}\") | .id" |
  while read -r asset_id; do
    gh api --method DELETE "repos/${repo}/releases/assets/${asset_id}"
  done

if [[ "$appcast_ok" -ne 1 ]]; then
  echo "::error::The nightly archives were published. The appcast was not."
  exit 1
fi
echo "Published nightly ${version}."
