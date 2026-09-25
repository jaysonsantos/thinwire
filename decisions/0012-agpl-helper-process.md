# AGPL helper process for WhatsApp and Signal

**Status:** proposed

**Refs:** issue #38, ADR [0004](0004-signal-out-of-v1.md), ADR [0010](0010-frontend-independent-core.md), ADR [0011](0011-agpl-protocols-local-only.md)

**Date:** 2026-09-25

## Context

The thinwire app is MIT. Two protocols need AGPL code:

- WhatsApp: feature `whatsapp-web` links `whatsapp-rust`. That stack links `wacore-libsignal`. 31 source files of `wacore-libsignal` carry `SPDX-License-Identifier: AGPL-3.0-only`.
- Signal: planned feature `signal-local` (#39) will link Presage and libsignal. Both are AGPL.

ADR 0011 makes both features local-only. Release builds and OS zips never enable them. A CI guard enforces this.

Issue #38 asks one question: can a separate AGPL helper process carry WhatsApp and Signal? Then release builds can offer both protocols, and the MIT app stays free of AGPL code.

This ADR is an engineering analysis. It is not legal advice. Section 1 lists the questions for a lawyer.

## Options

1. **Go:** build the helper now. Release builds offer WhatsApp and Signal through it.
2. **No-go:** WhatsApp and Signal stay local-only features for good.
3. **Later:** keep ADR 0011 now. Keep the code ready for a helper. Start the helper when the conditions in "Revisit when" are true.

## 1. License boundary

The helper is a separate program only if the boundary is a real program boundary. These are the rules for the design:

- The helper is its own executable. The app does not link it, load it as a library, or share memory with it.
- The two programs talk only through a documented message protocol over a pipe. They exchange messages (chats, messages, statuses). They do not exchange internal data structures.
- The app works without the helper. Telegram, Discord, and Slack need no helper.
- The helper has its own repository, license file, version, and release.

The FSF GPL FAQ ("aggregate" and "plug-ins" entries) states that pipes, sockets, and command-line arguments are normally a boundary between separate programs. It also states that the semantics of the exchange count. A protocol that sends complex internal data structures can make two processes one program. The design below keeps the protocol small and at the level of user-visible data.

AGPL-3.0 section 13 adds a duty for users who interact with a modified program over a network. The helper runs on the user's own computer, and only the local app talks to it. For binaries that we distribute, the normal GPLv3 duty applies (section 6): provide the Corresponding Source.

Questions for a lawyer, before any release build offers the helper:

1. Is a JSON-lines protocol over the helper's stdin and stdout, at the level of section 2 below, a boundary between separate programs?
2. Can the MIT app start the helper as a child process, and can it download or find the helper? Or must the user install the helper as a separate step?
3. Can one release page offer both the MIT app zip and the AGPL helper zip? Does a source tarball of the same tag satisfy the source offer?
4. `whatsapp-rust` calls itself MIT, but it links AGPL files. What license applies to a helper binary that links it? Is AGPL-3.0-only for the full helper enough?
5. Does the MIT `thinwire-ipc` crate (section 2) stay MIT when the AGPL helper uses it?

## 2. IPC design

### Transport

- The app starts the helper as a child process. The two programs talk over the helper's stdin and stdout. The helper writes logs to stderr.
- There is no socket and no named pipe. No other process can connect. No listening endpoint exists, so no token or ACL is necessary.
- The app owns the helper's lifetime. If the app exits, the helper's stdin closes, and the helper shuts down.
- Frame format: one JSON object per line (UTF-8, `\n`). A line is at most 1 MiB. JSON lines are easy to read in a test and need no code generator. Cap'n Proto or protobuf give no gain at the rate of chat messages.

### Wire types

A new small MIT crate `thinwire-ipc` holds the wire types, versioned apart from `AdapterCommand` and `AdapterEvent`:

- `Hello { protocol_version, helper_version, protocols: Vec<ProtocolId> }` is the first line from the helper. The app refuses an unknown `protocol_version` and shows a note.
- `Request { id, protocol, command }` from the app. `command` is a wire copy of the `AdapterCommand` variants for that protocol.
- `Event { protocol, event }` from the helper. `event` is a wire copy of the `AdapterEvent` variants that the helper can send.
- `Shutdown` from the app, and `Stopped { protocol }` from the helper.

The wire types are separate from the in-process enums. So a change inside the MIT app does not break the helper, and the protocol stays at the level of user data.

### Mapping to the adapter contract (ADR 0010)

On the app side, one MIT `HelperAdapter` implements `ProtocolAdapter` for each protocol that the helper offers. The core and the frontends do not change. The host routes commands to it like to any other adapter.

| App side (`ProtocolAdapter`) | Wire | Helper side |
| --- | --- | --- |
| `start` | spawn, then read `Hello` | open the session store |
| `handle(command)` | `Request` | the WhatsApp or Signal adapter |
| `view_chat` | `Request { ViewChat }` | mark read, unread counts |
| `shutdown` | `Shutdown`, then wait for `Stopped` | close sessions |
| events | `Event` | every event in the ADR 0010 contract |

The helper must follow the adapter contract of ADR 0010: `Account { Linked / Linking / Unlinked }` before inbox events, one `SendAccepted` or `SendRejected` per `request`, `CommandFailed`, `Notice`, and the pairing `generation` on each QR and pair-code event.

WhatsApp commands on the wire: `Connect`, `LoadChats`, `OpenChat`, `SendText`, `ResendMessage`, `WhatsAppAcknowledgeRisk`, `WhatsAppBeginLink { generation }`, `WhatsAppCancelLink`. Signal uses the same list with its own pairing commands from #39.

### Secrets on the wire

- The session stores stay in the helper's app-data folder. They never cross the pipe.
- The phone number for a WhatsApp pair code is typed in the app. Today `WhatsAppPhoneVault` keeps it off `AdapterCommand`. Over IPC it must cross the pipe once. It goes in a separate wire field of type `SecretText`. Its `Debug` output is redacted. Neither side logs it.
- A QR payload and a pair code cross the pipe from the helper to the app. They keep the type `RedactedPairingSecret`, and neither side logs them.
- Message text crosses the pipe. That is the purpose of the helper. Neither side logs message text.

### Failure

- The helper exits or sends a line that does not parse: the `HelperAdapter` sends `Account { Linking }` and a `Notice`. It starts the helper again after 1 s, 2 s, 4 s, and so on, up to 60 s. After 5 failures it stops and sends `Account { Unlinked }` with a note.
- Every open `SendText` gets `SendRejected` after a restart (ADR 0010 rule 4).
- The helper is not installed: the app does not offer WhatsApp or Signal. It shows one line in Add account: "Needs the thinwire helper (AGPL). See the README."

## 3. Distribution

- A separate repository, for example `thinwire-helper`, with license AGPL-3.0-only. It depends on the MIT `thinwire-ipc` crate at a pinned version.
- A separate release artifact for each OS: `thinwire-helper-<version>-<os>-<arch>.zip`, plus the source tarball of the same tag. The release page links the source. This is the source offer.
- The app zip never contains the helper. The user installs the helper as a separate step. This keeps the app release free of AGPL code, as ADR 0004 and ADR 0011 require. Section 1, question 2, can relax this later.
- Discovery: the app looks for the helper at a fixed path under the app-data folder, then on `PATH`. A setting can name another path. The app does not download the helper.
- Version check: `Hello.protocol_version` must match. The app shows a note if the helper is too old or too new.
- Updates: the user replaces the helper binary. The app does not update it.
- macOS and Windows: the helper needs its own signature when the app gets one. Today the OS zips are unsigned (ADR 0003), so the helper zips are unsigned too.

## 4. Security

- The helper runs as the same user as the app. A desktop app cannot drop to a lower user without admin rights. The helper gets no extra rights.
- The helper keeps its session stores in its own folder: `<data_dir>/thinwire-helper/<protocol>/`, mode `0700` on Unix. The app never reads that folder.
- The helper listens on no port and no socket. Only its parent process can talk to it.
- The helper checks each line: size limit, known type, known protocol. It closes on a bad line.
- Logs: the helper writes to stderr. The app reads stderr and writes it at level `debug` with the prefix `helper:`. The helper never writes message text, phone numbers, QR data, or pair codes.
- Supply chain: the helper pins `whatsapp-rust` by git rev (as the app does today) and pins Presage and libsignal. Its CI runs `cargo deny` for advisories. Its license check allows AGPL.
- The WhatsApp ToS and ban risk do not change. The full-screen ban gate stays in the app, before any pairing command goes to the helper. The README risk bullets stay verbatim.

## 5. Cost

These are estimates for one engineer. They include tests and CI, but not the legal review.

| Work | Estimate |
| --- | --- |
| `thinwire-ipc` crate: wire types, framing, version check, tests | 2 to 3 days |
| `HelperAdapter` in the app: spawn, restart with backoff, discovery, settings | 3 to 4 days |
| Helper skeleton: stdin/stdout loop, WhatsApp adapter moved from `crates/thinwire-protocol/src/whatsapp/` | 3 to 5 days |
| Signal in the helper (after #39 exists as a local feature) | 5 to 10 days |
| Helper CI on 3 OS, release workflow, source tarball, README | 2 to 3 days |
| Total | about 3 to 5 weeks |

Recurring cost:

- Two release pipelines and two version numbers. Each change to the wire types needs a release of both.
- The `whatsapp-rust` stack changes often, and WhatsApp changes its protocol. The helper needs updates for that. This cost exists today for the local feature too.
- The helper CI builds libsignal on 3 OS. That adds CI minutes to each helper release.
- Support: users can run a helper that does not match the app. The version check and the notes must cover it.

## Recommendation

**Later.**

Reasons:

- The legal questions in section 1 are open. No release build can offer the helper before a lawyer answers them.
- Signal has no local feature yet (#39). The helper would carry only WhatsApp at first.
- WhatsApp is experimental and carries a ban risk. The local feature is enough for tests by the developers now.
- The adapter contract of ADR 0010 is stable. A `HelperAdapter` can use it later with no change to the core or the frontends. So a wait costs little.

Keep the door open now, at no extra cost:

- The WhatsApp and Signal adapters use only the `ProtocolAdapter` trait and the ADR 0010 events. They do not call the core or the UI.
- They keep their session stores in their own app-data folder.
- Secrets stay off `AdapterCommand` and out of logs, as today.
- New WhatsApp or Signal state goes through the adapter contract, not through special cases in the core.

### Revisit when

Start the "go" work when all of these are true:

1. A lawyer answers the questions in section 1, and the answers allow the design.
2. `signal-local` (#39) works in local builds, or the helper carries WhatsApp only by decision.
3. `whatsapp-web` passes live tests on Linux, macOS, and Windows.
4. A maintainer accepts to own the AGPL repository and its releases.

## Consequences

- ADR 0011 stays in force. Release builds and OS zips never enable `whatsapp-web` or `signal-local`.
- No new crate or repository now.
- If this ADR is accepted, a later ADR records "go" with the legal answers, and it supersedes the "later" part.
