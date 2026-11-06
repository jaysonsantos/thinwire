#!/usr/bin/env bash
# Point the AUR package thinwire-nightly-bin at the newest nightly release.
# The `AUR nightly` workflow runs this once a day in an Arch Linux container.
#
# The PKGBUILD lives in the AUR repository, and a person pushes its first
# version after a review. This script changes only pkgver, pkgrel, the
# checksums, and .SRCINFO. It does nothing when the AUR already has the
# newest version.
#
# Needs: AUR_SSH_PRIVATE_KEY (the key of the AUR account, push only),
# GITHUB_REPOSITORY. Runs as root in the container: makepkg runs as an
# unprivileged user, because it refuses root.
set -euo pipefail

PKGNAME=thinwire-nightly-bin
AUR_HOST=aur.archlinux.org
# From the AUR home page, https://aur.archlinux.org/.
AUR_HOST_KEY_SHA256=SHA256:RFzBCUItH9LZS0cKB5UE6ceAYhBD5C8GeOBip8Z11+4
BUILD_USER=builder

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY names the repository}"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

if ! version=$(curl -fsSL "https://github.com/${repo}/releases/download/nightly/VERSION"); then
  echo "::error::The nightly release of ${repo} has no VERSION file yet."
  exit 1
fi
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.r[0-9]+\.g[0-9a-f]{7}$ ]]; then
  echo "::error::The nightly VERSION is not a version: ${version}"
  exit 1
fi

git clone --quiet "https://${AUR_HOST}/${PKGNAME}.git" "$work/pkg"
cd "$work/pkg"
if [[ ! -f PKGBUILD ]]; then
  echo "::error::The AUR has no ${PKGNAME} yet. Review the PKGBUILD and push its first version by hand."
  exit 1
fi
current=$(sed -n 's/^pkgver=//p' PKGBUILD)
if [[ "$current" == "$version" ]]; then
  echo "${PKGNAME} is already ${version}."
  exit 0
fi

sed -i -e "s/^pkgver=.*/pkgver=${version}/" -e 's/^pkgrel=.*/pkgrel=1/' PKGBUILD
id -u "$BUILD_USER" >/dev/null 2>&1 || useradd --create-home "$BUILD_USER"
chown -R "$BUILD_USER" "$work/pkg"
runuser -u "$BUILD_USER" -- updpkgsums
runuser -u "$BUILD_USER" -- sh -c 'makepkg --printsrcinfo > .SRCINFO'

# The checksum of the archive must match the SHA256SUMS of the release.
linux_archive="thinwire-linux-x86_64-${version}.tar.gz"
expected=$(curl -fsSL "https://github.com/${repo}/releases/download/nightly/SHA256SUMS" |
  awk -v name="$linux_archive" '$2 == name { print $1 }')
if [[ -z "$expected" ]] || ! grep -q "$expected" .SRCINFO; then
  echo "::error::The checksum of ${linux_archive} in .SRCINFO does not match SHA256SUMS of the release."
  exit 1
fi

mkdir -p ~/.ssh
chmod 700 ~/.ssh
known_hosts=~/.ssh/known_hosts
ssh-keyscan -t ed25519 "$AUR_HOST" 2>/dev/null >"$known_hosts.new"
if [[ "$(ssh-keygen -lf "$known_hosts.new" | awk '{ print $2 }')" != "$AUR_HOST_KEY_SHA256" ]]; then
  echo "::error::The host key of ${AUR_HOST} does not match the fingerprint on the AUR home page."
  exit 1
fi
mv "$known_hosts.new" "$known_hosts"
key_file=~/.ssh/aur
(umask 077 && printf '%s\n' "${AUR_SSH_PRIVATE_KEY:?AUR_SSH_PRIVATE_KEY is the AUR push key}" >"$key_file")

git config --global --add safe.directory "$work/pkg"
git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add PKGBUILD .SRCINFO
git commit --quiet -m "Update to ${version}"
GIT_SSH_COMMAND="ssh -i ${key_file} -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes" \
  git push "ssh://aur@${AUR_HOST}/${PKGNAME}.git" HEAD:master
echo "Pushed ${PKGNAME} ${version} to the AUR."
