#!/usr/bin/env bash
# Keep the local-only feature `keychain-security-cli` out of CI and out of a
# release. That feature reads secrets through /usr/bin/security so a local
# rebuild does not ask for Keychain permission again. Only
# `scripts/local-build.sh` turns it on.
#
# CI runs this from `scripts/test.sh`. It checks:
#   1. the name is not in `.github/`, which holds every workflow;
#   2. the name is in no script but the two that own it;
#   3. in a Cargo.toml the name is only a feature definition: no `default`, no
#      other feature, and no dependency line turns it on;
#   4. no cargo call in `.github/`, `.cargo/` or a script uses `--all-features`
#      (it would turn the feature on);
#   5. `all-protocols` is the release feature list of
#      `scripts/test-release-features.sh`, so a local build has what a release has;
#   6. `crates/thinwire-core/build.rs` really stops a build under CI.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(dirname "$script_dir")"
cd "$repo_root"

feature="keychain-security-cli"
owners=("scripts/local-build.sh" "scripts/check-local-only-features.sh")

fail=0
problem() {
  echo "local-only feature guard: $*" >&2
  fail=1
}

# 1. Workflows.
if hits="$(grep -rn -- "$feature" .github 2>/dev/null)"; then
  problem "'$feature' is in a workflow:"
  echo "$hits" >&2
fi

# 2. Scripts.
for file in scripts/*; do
  [[ -f "$file" ]] || continue
  skip=0
  for owner in "${owners[@]}"; do
    [[ "$file" == "$owner" ]] && skip=1
  done
  [[ "$skip" -eq 1 ]] && continue
  if hits="$(grep -n -- "$feature" "$file")"; then
    problem "'$feature' is in $file, which CI may run:"
    echo "$hits" >&2
  fi
done

# 3. Manifests. A feature definition starts the line. A comment is fine.
for manifest in Cargo.toml crates/*/Cargo.toml; do
  if hits="$(grep -n -- "$feature" "$manifest" | grep -Ev "^[0-9]+:(${feature} =|[[:space:]]*#)" || true)"; then
    if [[ -n "$hits" ]]; then
      problem "'$feature' is in a feature list or a dependency of $manifest:"
      echo "$hits" >&2
    fi
  fi
done

# 4. `--all-features` in a cargo call. A comment or a string test is not a call.
if hits="$(grep -rnE '^[^#]*cargo[^#]*--all-features' .github .cargo scripts 2>/dev/null | grep -v '^scripts/check-local-only-features.sh:' || true)"; then
  if [[ -n "$hits" ]]; then
    problem "a cargo call uses --all-features, which turns '$feature' on:"
    echo "$hits" >&2
  fi
fi

# 5. all-protocols is the release list.
all_protocols="$(
  awk '/^all-protocols = \[/{on=1; next} on && /^\]/{exit} on{gsub(/[",[:space:]]/, ""); print}' crates/thinwire/Cargo.toml | sort
)"
release_list="$(
  sed -nE 's/^release_features=\((.*)\)$/\1/p' scripts/test-release-features.sh | tr ' ' '\n' | sort
)"
if [[ -z "$all_protocols" || -z "$release_list" ]]; then
  problem "could not read the all-protocols list or the release_features list"
elif [[ "$all_protocols" != "$release_list" ]]; then
  problem "all-protocols and release_features differ:"
  echo "all-protocols:    $(tr '\n' ' ' <<<"$all_protocols")" >&2
  echo "release_features: $(tr '\n' ' ' <<<"$release_list")" >&2
fi

# 6. The build script stops the feature under CI. `cargo check` runs the build
# script of thinwire-core before it checks the crate, so the build stops there.
for variable in CI GITHUB_ACTIONS; do
  if log="$(env "$variable=true" cargo check -p thinwire-core --features "$feature" 2>&1)"; then
    problem "a build with '$feature' passed with $variable=true"
  elif ! grep -q "local builds only" <<<"$log"; then
    problem "a build with '$feature' and $variable=true failed for another reason:"
    tail -5 <<<"$log" >&2
  fi
done

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi
echo "local-only feature guard: ok"
