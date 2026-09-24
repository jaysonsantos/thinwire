# AGPL protocols are local-only

**Status:** accepted

**Amends:** [0004](0004-signal-out-of-v1.md)

**Date:** 2026-09-24

## Context

The thinwire app is MIT. Feature `whatsapp-web` links `wacore-libsignal`. That crate carries `SPDX-License-Identifier: AGPL-3.0-only` in Signal Messenger source files. A Signal client links Presage and libsignal. Those crates are AGPL too.

Council lock S2 in ADR 0004 kept Signal out of the release binary. The user decision on 2026-09-24 allows the AGPL code in local builds only.

## Decision

`whatsapp-web` is a local-only cargo feature. `signal-local` is a planned local-only feature (#39).

- `whatsapp-web` can link `wacore-libsignal` on a local build.
- Planned feature `signal-local` (#39) will link Presage and libsignal on a local build.
- `whatsapp-web` stays off by default.
- Planned feature `signal-local` stays off by default.
- Release builds never enable `whatsapp-web`.
- Release builds never enable `signal-local`.
- OS zips never enable `whatsapp-web`.
- OS zips never enable `signal-local`.
- The three README risk bullets stay verbatim.
- Signal stays off the README protocol table.

Issue #38 researches a separate AGPL helper process. That research can recommend go, no-go, or later. This ADR does not choose that path.

ADR 0004 still blocks AGPL code in the release binary. This ADR is the exception for `whatsapp-web` now and for planned feature `signal-local` (#39).

## Consequences

- #39 adds feature `signal-local`. A full-screen notice comes before link. The notice says experimental, local build only, and AGPL.
- #39 adds a CI guard. The guard fails when a release build or `os-zips.yml` shows `presage`, `libsignal`, or `wacore-libsignal` in `cargo tree`.
- The same guard covers `whatsapp-web`.
- Default CI stays feature-off.
- The helper process, if any, comes from the #38 ADR. It is not this decision.
