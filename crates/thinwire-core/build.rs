//! Keeps the local-only feature `keychain-security-cli` out of CI.
//!
//! That feature makes the app read and write its secrets through
//! `/usr/bin/security`. It exists so a developer's rebuild does not ask for
//! Keychain permission again. A release, and every CI job, must use the
//! Keychain API. GitHub Actions sets `GITHUB_ACTIONS` and most other CI
//! systems set `CI`, so a build with the feature stops when one of them is set.
//!
//! This is the second guard. `scripts/check-local-only-features.sh` is the
//! first: it keeps the feature name out of the workflows and out of every
//! other feature list, and it runs this script's check.

use std::env;

/// A variable that is set to something other than empty, `0` or `false`.
fn is_set(name: &str) -> bool {
    env::var(name).is_ok_and(|value| !matches!(value.trim(), "" | "0" | "false"))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CI");
    println!("cargo:rerun-if-env-changed=GITHUB_ACTIONS");

    if env::var_os("CARGO_FEATURE_KEYCHAIN_SECURITY_CLI").is_none() {
        return;
    }
    let under_ci = ["CI", "GITHUB_ACTIONS"].into_iter().any(is_set);
    assert!(!under_ci, "{MESSAGE}");
}

/// The text that the guard script looks for is "local builds only".
const MESSAGE: &str = concat!(
    "feature `keychain-security-cli` is for local builds only: ",
    "it reads secrets through /usr/bin/security. ",
    "Do not turn it on in CI or in a release. ",
    "Build locally with scripts/local-build.sh.",
);
