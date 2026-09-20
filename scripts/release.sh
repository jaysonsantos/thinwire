#!/usr/bin/env bash
# Tag a release from Conventional Commits. Pushes nothing.
#
#   scripts/release.sh          # version from git-cliff
#   scripts/release.sh 0.2.0    # version from the argument
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(dirname "$script_dir")"

cargo_manifest="Cargo.toml"
cargo_lock="Cargo.lock"
changelog="CHANGELOG.md"
version_pattern='^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$'

cd "$repo_dir"

command -v git-cliff >/dev/null || {
  echo "git-cliff is missing: enter the nix shell or run 'cargo install git-cliff'" >&2
  exit 1
}

[[ -z "$(git status --porcelain --untracked-files=no)" ]] || {
  echo "the tracked files have changes: commit or stash them first" >&2
  exit 1
}

version="${1:-$(git cliff --bumped-version)}"
version="${version#v}"
tag="v$version"

[[ "$version" =~ $version_pattern ]] || {
  echo "'$version' is not a version number" >&2
  exit 1
}

! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || {
  echo "the tag $tag exists" >&2
  exit 1
}

sed -i "/^\[workspace\.package\]/,/^\[/ s/^version = \".*\"\$/version = \"$version\"/" "$cargo_manifest"
cargo update --workspace --quiet

git cliff --tag "$tag" --output "$changelog"
notes="$(git cliff --tag "$tag" --unreleased --strip all | sed "/^## \[/d")"

git add -- "$cargo_manifest" "$cargo_lock" "$changelog"
git commit --quiet --message "chore(release): $tag"
git tag --annotate --cleanup=whitespace "$tag" --message "$notes"

echo "$tag is ready. Push it with: git push --follow-tags"
