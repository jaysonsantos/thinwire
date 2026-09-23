# Artifacts A: main-only unsigned OS zip artifacts

**Status:** accepted

## Context
Product-council (2026-09-20) locked CI packaging after option B. Developers need Windows, macOS, and Linux binaries from GitHub Actions. Those binaries must not look like a signed release. Pull requests must not upload OS zips. The project stays a desktop egui app. There is no distroless GUI Docker image.

Packaging correction (2026-09-23): zipping `dist/` and then uploading that zip made `actions/upload-artifact` wrap a second zip. The macOS download is an unsigned app bundle inside the single artifact zip.

## Decision
CI uploads unsigned OS artifacts on **push to `main` only**.

A matrix builds release binaries on Ubuntu, macOS, and Windows. Artifact names are `thinwire-linux-x86_64`, `thinwire-macos-arm64`, and `thinwire-windows-x86_64`. Retention is 7 days (short retention; delete-on-new-main is not needed). The workflow does not listen to `pull_request`. The upload job also requires `github.event_name == 'push'` and `github.ref == 'refs/heads/main'`.

`actions/upload-artifact` is the only zip layer. The job does not pre-zip. It uploads the payload directory. The Actions download is one zip.

Linux and Windows payloads are flat. The binary, `LICENSE`, and `THIRD_PARTY_NOTICES` sit at the zip root. Linux also ships the TDLib LLVM C++ runtime libraries and their copyright files.

The macOS payload is an unsigned `Thinwire.app` inside that zip. `Contents/MacOS/thinwire` is the release binary. `Contents/Info.plist` sets `CFBundleExecutable` to `thinwire`, `CFBundleIdentifier` to `dev.jaysonsantos.thinwire`, `CFBundleName` and `CFBundleDisplayName` to `Thinwire`, and `CFBundlePackageType` to `APPL`. `CFBundleShortVersionString` and `CFBundleVersion` are the workspace package version. `LICENSE` and `THIRD_PARTY_NOTICES` sit in `Contents/Resources`. There is no codesign and no notarization.

Existing lint, test, build, and `check` jobs stay as they are. README states that the artifacts are unsigned and are not a release.

## Consequences
A `main` push keeps three short-lived zips. Reviewers do not get OS zips from pull-request CI. Users must not treat the zips as a release or as signed software. A later signed-release path needs a new council lock.

Rejected: PR artifact uploads; distroless GUI image; delete-on-new-main (short retention is enough); a zip inside the artifact zip; codesign or notarization on these artifacts.
