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

notes="Build of ${sha} on main, version ${version}. This is not a release.

Each archive stays here for ${KEEP_DAYS} days. VERSION and SHA256SUMS describe the newest archives."

if gh release view "$TAG" --repo "$repo" >/dev/null 2>&1; then
  gh api --method PATCH "repos/${repo}/git/refs/tags/${TAG}" -f sha="$sha" -F force=true >/dev/null
  gh release edit "$TAG" --repo "$repo" --title "Nightly ${version}" --notes "$notes" --prerelease --latest=false
else
  gh release create "$TAG" --repo "$repo" --target "$sha" --title "Nightly ${version}" --notes "$notes" \
    --prerelease --latest=false
fi
gh release upload "$TAG" --repo "$repo" --clobber "$dist"/thinwire-*.tar.gz "$dist/SHA256SUMS" "$dist/VERSION"

cutoff=$(date -u -d "-${KEEP_DAYS} days" +%Y-%m-%dT%H:%M:%SZ)
gh api "repos/${repo}/releases/tags/${TAG}" \
  --jq ".assets[] | select(.name | endswith(\"${version}.tar.gz\") or . == \"SHA256SUMS\" or . == \"VERSION\" | not) | select(.created_at < \"${cutoff}\") | .id" |
  while read -r asset_id; do
    gh api --method DELETE "repos/${repo}/releases/assets/${asset_id}"
  done

echo "Published nightly ${version}."
