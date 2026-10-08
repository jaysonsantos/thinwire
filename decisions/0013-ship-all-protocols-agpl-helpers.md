# 0013: Ship all protocols in release builds; AGPL protocols run in helper processes

**Status:** accepted (2026-10-08)

**Date:** 2026-10-08

**Supersedes:** [0004](0004-signal-out-of-v1.md) (council lock S2, Signal out of v1); [0011](0011-agpl-protocols-local-only.md) (AGPL protocols local-only); the "Later" recommendation, "Revisit when" list and section 3 "Distribution" of [0012](0012-agpl-helper-process.md).

**Refs:** issue #246, issue #248, issue #38, issue #77, ADR [0003](0003-main-only-ci-artifacts.md), ADR [0006](0006-live-tdlib.md), ADR [0010](0010-frontend-independent-core.md)

## Context

The owner asked on 2026-10-07: "Start the changes to include WhatsApp, discord, signal in the final binary, even if you have to use the ipc to comply with the licenses."

The thinwire app is MIT. Licenses of the protocol stacks:

- WhatsApp: `whatsapp-rust` is MIT. It links `wacore-libsignal`. That crate's `Cargo.toml` says MIT, but 31 files in `wacore/libsignal/src` carry `SPDX-License-Identifier: AGPL-3.0-only`. We treat it as AGPL-3.0-only. `cargo deny` cannot find this from metadata.
- Signal: presage, presage-store-sqlite and libsignal are AGPL-3.0.
- Discord: twilight-http and twilight-model 0.17.1 are ISC.
- Slack: slack-morphism 2.29.0 is Apache-2.0.
- Telegram: tdlib-rs 1.4.0 is Apache-2.0. TDLib is BSL-1.0. ADR 0006 does not change.

ADR 0012 analyzed a helper process and recommended "Later" until a lawyer answered its section 1 questions. The owner chose to start now, before a legal review. This ADR is an engineering decision. It is not legal advice. The section 1 questions of 0012 stay open.

## Decision

1. Telegram, Slack and Discord run in-process in the MIT app.
2. WhatsApp runs in its own helper binary, `thinwire-whatsapp-helper` (AGPL-3.0-only). Signal runs in a second helper binary, `thinwire-signal-helper` (AGPL-3.0-only). A crash of one helper does not stop the other. This replaces the single helper of 0012.
3. The MIT app talks to the helpers only over IPC. The MIT app crates never depend on a helper crate or on an AGPL crate.
4. The IPC design of 0012 section 2 applies (transport, wire types, ADR 0010 mapping, secrets on the wire, failure), and so does its section 4 (security). Exception: when the restart backoff of 0012 stops, each account row of that helper shows "Helper stopped. Restart". A click on Restart starts the helper again.
5. The UI never waits on IPC. A slow helper shows a busy account row, not a frozen window.
6. Distribution (replaces 0012 section 3):
    - The release archive for each OS contains the app and both helpers. The user does not install a helper as a separate step.
    - The app looks for each helper next to its own binary first (`Contents/MacOS/` in `Thinwire.app`). A setting can name another path. That path gets the same 0012 section 4 checks as the default path.
    - If a helper is missing, the app does not hide its protocol. In the add-account picker, that protocol shows as disabled, with the reason. An existing account of that protocol keeps its row and shows "<Protocol> helper missing. Reinstall thinwire." The start error says what failed, why, and what to do.
    - Signing follows ADR 0003: on macOS each helper gets the same ad-hoc signature as the app. There is no Developer ID and no notarization. Linux and Windows helpers are unsigned.
7. CI builds and tests every protocol feature that a release ships. This starts in the first PR of #246.
    - `scripts/check-agpl-deps.sh` and `scripts/check-release-tree.sh` change: they allow AGPL crates in the helper crates only, and they still fail if the MIT app binary links AGPL code.
8. Each release contains:
    - The AGPL-3.0 text for each helper, and the Corresponding Source (AGPL-3.0 section 1) through the same place as the binaries (section 6(d)). Each release attaches `thinwire-helpers-<tag>-source.tar.gz` next to the binaries. It holds the whole thinwire workspace of the tag, including `Cargo.lock` and the build scripts, and the `cargo vendor` output for the dependencies of both helper crates, including `whatsapp-rust`, `wacore-libsignal`, presage and libsignal. The tarball stays on the release as long as the binaries do. The release never depends on an outside git rev for the source offer.
    - The Apache-2.0 text and NOTICE for tdlib-rs, BSL-1.0 for TDLib, ISC for twilight.
    - The licenses of `egui_extras` and `image` (with the jpeg, png and webp decoders) once #244 adds them.
9. README: ToS and ban-risk bullets for every protocol in the release build. Signal gets its line again: no supported third-party API, expect breakage. The Signal gate screen shows the same text. The WhatsApp ban gate stays in the app. The user accepts it before the app sends any pairing command or phone number to the helper, so no QR or pair code is requested before the gate.

## Order

1. This ADR, the CI job and the WhatsApp helper (#246).
2. The Signal helper (#248).

## Open questions

- From 0012: which part starts and stops each helper, and how to prevent a second spawn on the same session store. Resolve in the #246 PR.
- From 0012: a helper restart must also end open `LoadChats` and `OpenChat` requests and clear their loading markers.
- The legal questions of 0012 section 1.

## Consequences

- Release builds offer WhatsApp and Signal. WhatsApp ships with #246; Signal ships only with #248. Until then the release archive contains only the WhatsApp helper, the app does not offer Signal, and the "helper missing" path of decision 6 does not fire for Signal. The MIT app binary still contains no AGPL code.
- Each release has three binaries per OS and more license files.
- CI builds libsignal on three OSes. This adds CI minutes to every run that builds the helpers.
- AGENTS.md and ROADMAP.md follow this ADR in the same PR. README follows with the #246 implementation PR.
- Mark 0004, 0011 and 0012 with a "Superseded by 0013" line. Do not change their other text.
