# Artifacts A: main-only unsigned OS zip artifacts

**Status:** accepted

## Context
Product-council (2026-09-20) locked CI packaging after option B. Developers need Windows, macOS, and Linux binaries from GitHub Actions. Those binaries must not look like a signed release. Pull requests must not upload OS zips. The project stays a desktop egui app. There is no distroless GUI Docker image.

## Decision
CI uploads unsigned OS zip artifacts on **push to `main` only**.

A matrix builds release binaries on Ubuntu, macOS, and Windows. Each job zips the binary with a clear OS name. Artifact retention is 7 days (short retention; delete-on-new-main is not needed). The workflow does not listen to `pull_request`. The upload job also requires `github.event_name == 'push'` and `github.ref == 'refs/heads/main'`.

Existing lint, test, build, and `check` jobs stay as they are. README states that the zips are unsigned and are not a release.

## Consequences
A `main` push keeps three short-lived zips. Reviewers do not get OS zips from pull-request CI. Users must not treat the zips as a release or as signed software. A later signed-release path needs a new council lock.

Rejected: PR artifact uploads; distroless GUI image; delete-on-new-main (short retention is enough).
