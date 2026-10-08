# Thinwire roadmap

Ordered work from product-council ADRs (`decisions/0001`–`0013`; `0013` ships every protocol, with WhatsApp and Signal in AGPL helper processes, and supersedes `0004`, `0011` and the Later part of `0012`; the WhatsApp helper is #246, the Signal helper is pending in #248). ADRs are decisions; this file is the todo list.

Rule: an ADR accepted is not Done until the matching change is on `main`.

## Done (locks landed)

- Multi-protocol option B without Signal in v1 release builds — `0001`, `0004`, amended for local builds by `0011`. Superseded by `0013` (see AGPL helpers below)
- egui + eframe + glow (no wgpu) — `0002`
- Main-only unsigned CI artifacts + caveat — `0003`
- System light/dark by default — `0005`
- Live TDLib path + hard constraints — `0006` (on `main` via #15 / `eff31de`)
- Official release inject for main OS zips — `0007` (on `main` via #19 / `5c46222`)
- Chat list + messages after `authorizationStateReady`. The worker loads the main chat list, opens a chat for recent messages, and sends text. Auth alone is not the inbox. WhatsApp and Slack stay not-ready. The Discord bot inbox is on `main` (#35).
- Telegram feels usable — on `main` via #40 / `b670536`.
  - Resume a saved session at launch.
  - Chat list and thread scroll, plus loading states.
  - Compose and send: Enter sends, drafts per chat, failed text stays with Retry.
  - Login steps: keyboard flow and clear errors.
  - Message time, sender, and date separators.
  - Live-test fixes: TDLib closes at exit, a keychain-missing notice, and libc++ / mesa in the Nix dev shell.
  - The phone step names the kept data folder once.
- Visual design pass — on `main` via #47.
- Load older messages when you scroll up in a chat — #30, on `main` via #61.
- Open a chat from a click anywhere on its inbox row — #31, on `main` via #60.
- Frontend-independent core (`crates/thinwire-core`, ADR `0010`) — #33, on `main` via #48.
- Protocol-independent shell — on `main` via #68.
- Desktop notifications for new messages — #32, on `main` via #87.
- Discord bot/OAuth guild inbox — #35, on `main` via #51.
- AGPL helper decision is Later — #38, ADR `0012`, on `main` via #75. The Later part is superseded by `0013`.
- Signal inbox, feature `signal-local`, local-only — #39, on `main` via #66.
- WhatsApp close during startup — #44, on `main` via #53.
- A send with no answer — #69, on `main` via #81.
- WhatsApp and Signal adapters live in AGPL crate folders — #77, on `main` via #183. The root license stays MIT.
- Stale Ready line on the status strip — #80, on `main` via #82.

## Next (parallel tracks)

Product lock change (2026-09-23): WhatsApp, Discord, and Slack run in parallel with Telegram. They do not wait for "Telegram feels usable". Each track keeps its cargo feature off in the default build and in public CI. Every safety rule stays: the WhatsApp ToS/ban gate, no Discord self-bots, and official Slack OAuth only.

### Telegram

1. **Telegram UX follow-ups**
    - [x] Remove jargon from the account row ("status: stubbed", "Supported · TDLib").
    - [ ] Remove jargon from the worker status text ("TDLib <code>"). The strip already maps the load failures to plain text and hides other lines that name TDLib. The worker strings still carry "(TDLib <code>)".

### Shared

- [ ] #153 — mute a chat for every protocol. PR #201.
- [ ] #161 — replace, dismiss, and click a notification on macOS and Windows. Windows is done via #194. macOS is #197. #197 waits for the user's Mac test.
- [ ] #173 — raise the window on a notification click on KDE Wayland. This waits for user decision D4.

### Other protocols (parallel with Telegram)

- [ ] #34 — WhatsApp experimental linked-device inbox (feature `whatsapp-web`, `0001`). The AGPL client runs in the WhatsApp helper program (`0013`, #246). Release builds turn it on. Honest ToS labels. Full-screen ToS/ban gate before QR or pair. Never call it reliable.
- [ ] #36 — Slack workspace-app OAuth inbox (feature `slack-oauth`, `0008`). Official OAuth v2 only. Not a personal desktop clone.

### AGPL helpers (`0013`)

`0013` (accepted 2026-10-08) supersedes `0011`: release builds ship WhatsApp and Signal in AGPL-3.0-only helper binaries over IPC. The MIT app binary never links AGPL code. Until the Signal helper lands, `signal-local` stays local-only.

- [x] #246 — the `release-features` CI job and the WhatsApp helper.
    - `crates/thinwire-ipc`: the wire protocol.
    - `HelperAdapter` in `thinwire-protocol`: start, restart, "Helper stopped." with Restart, and the missing-helper state.
    - `thinwire-whatsapp-helper` in `crates/thinwire-whatsapp`, with a lock on the session folder.
    - OS zips ship the helper, the AGPL text, the source offer, and the source tarball.
- [ ] #248 — the Signal helper.
- [ ] Slack in the OS zips. `0013` names `slack-oauth` as a release feature. The zips need the publisher `SLACK_CLIENT_ID`, `SLACK_CLIENT_SECRET`, and `SLACK_APP_TOKEN` secrets first.
- [ ] A live test of WhatsApp through the helper on Linux, macOS, and Windows (a spare number only).

## Spike (scaffold, not the default UI)

- `whatsapp-web` — experimental linked-device scaffold on `whatsapp-rust` (oxidezap), pinned to a git revision. The AGPL code (`wacore-libsignal`) runs in the helper program `thinwire-whatsapp-helper`, not in the app (`0013`, #246). Off unless that cargo feature is enabled. The default CI jobs do not enable it. Release builds and OS zips enable it. No ready WhatsApp account in the default build. Full-screen ToS/ban gate before QR or pair. Work continues in #34.
- `discord-bot` — bot/OAuth guild inbox on twilight HTTP (`0009`). Off by default. Shows when the feature is compiled. Not a personal Discord client. On `main` via #35.
- `slack-oauth` — workspace-app OAuth v2 / Socket Mode scaffold on `slack-morphism` (`0008`). Off by default. No Slack auth UI in the default shell. Work continues in #36.

## Explicitly not next

- AGPL code in the MIT app binary. WhatsApp and Signal ship only as helper binaries (`0013`). Feature `signal-local` stays off in release builds.
- gpui / gpui-ce toolkit switch — parked until thinwire-shaped measurements exist (`0002`).

## Pointers

- Decisions: `decisions/`
- Agent rules: `AGENTS.md` (symlink `CLAUDE.md`)
