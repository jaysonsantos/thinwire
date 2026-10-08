# Artifacts A: main-only unsigned OS zip artifacts

**Status:** accepted

## Context
Product-council (2026-09-20) locked CI packaging after option B. Developers need Windows, macOS, and Linux binaries from GitHub Actions. Those binaries must not look like a signed release. Pull requests must not upload OS zips. The project stays a desktop egui app. There is no distroless GUI Docker image.

Packaging correction (2026-09-23): `actions/upload-artifact` wraps every download in an outer zip and stores loose files as mode 644. Uploading `dist/` drops the executable bit, so `Thinwire.app` does not launch and the Linux binary is not runnable. The job uploads one `.tar.gz`. The outer zip is GitHub's wrapper. The tar.gz keeps Unix modes and the app bundle.

## Decision
CI uploads OS artifacts on **push to `main` only**. The Linux and Windows binaries are unsigned. The macOS app is ad-hoc signed, with no Developer ID, and not notarized.

A matrix builds release binaries on Ubuntu, macOS, and Windows. Artifact names are `thinwire-linux-x86_64`, `thinwire-macos-arm64`, and `thinwire-windows-x86_64`. Retention is 7 days (short retention; delete-on-new-main is not needed). The workflow does not listen to `pull_request`. The upload job also requires `github.event_name == 'push'` and `github.ref == 'refs/heads/main'`.

GitHub always wraps the Actions download in an outer zip. That wrapper is unavoidable. The uploaded file is one `.tar.gz` built with `tar` after staging. The tar.gz preserves executable bits. Extract it to get the payload. Directory upload alone is not a ready-to-run artifact.

Linux and Windows tar members are flat. The binary, `LICENSE`, and `THIRD_PARTY_NOTICES` sit at the archive root. Linux also ships the TDLib LLVM C++ runtime libraries and their copyright files. The Linux and macOS binaries are mode `755` inside the tar.gz.

The macOS tar.gz contains an ad-hoc signed `Thinwire.app` (no Developer ID, not notarized). `Contents/MacOS/thinwire` is the release binary and is executable in the archive. `Contents/Info.plist` sets `CFBundleExecutable` to `thinwire`, `CFBundleIdentifier` to `dev.jaysonsantos.thinwire`, `CFBundleName` and `CFBundleDisplayName` to `Thinwire`, and `CFBundlePackageType` to `APPL`. `CFBundleShortVersionString` and `CFBundleVersion` are the workspace package version. `LICENSE` and `THIRD_PARTY_NOTICES` sit in `Contents/Resources`. There is no Developer ID signature and no notarization. Extract the tar.gz to get the app. Gatekeeper quarantine still applies (see the amendment).

Amendment (2026-09-29, #161): the stage script gives the whole bundle an ad-hoc signature (`codesign --force --sign -`) and checks it (`codesign --verify --strict`). Reason: thinwire-notify uses `UNUserNotificationCenter`, and that API needs a signed bundle whose signature binds `Info.plist` and the bundle id. Before this step, only the linker signed the binary. An ad-hoc signature has no identity. Gatekeeper still treats the app as software from an unknown developer. A downloaded archive usually has the `com.apple.quarantine` attribute, and the extracted app keeps it, so macOS blocks the first launch. The documented path is Gatekeeper's per-app override: after the blocked launch, System Settings > Privacy & Security > Security > Open Anyway (shown for about an hour), confirmed with Open and the login password. macOS saves `Thinwire.app` as an exception to the security settings. The command-line alternative removes the attribute: `xattr -dr com.apple.quarantine Thinwire.app`. Gatekeeper then does not check that copy at its first launch. This decision stays: no Developer ID and no notarization.

Existing lint, test, build, and `check` jobs stay as they are. README states that the artifacts have no Developer ID signature and are not a release.

## Consequences
A `main` push keeps three short-lived artifacts. Each download is an outer zip around one tar.gz. Reviewers do not get OS artifacts from pull-request CI. Users must not treat them as a release or as signed software. A later signed-release path needs a new council lock.

Rejected: PR artifact uploads; distroless GUI image; delete-on-new-main (short retention is enough); uploading the raw directory (it drops execute bits); a zip whose only member is an identical zip; a Developer ID signature or notarization on these artifacts (the ad-hoc signature of the amendment is not a Developer ID signature).
