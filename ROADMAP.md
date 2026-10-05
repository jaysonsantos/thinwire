# Thinwire roadmap

Ordered work from product-council ADRs (`decisions/0001`–`0012`; `0011` is the local-only AGPL lock; `0012` is the helper, accepted as Later). ADRs are decisions; this file is the todo list.

Rule: an ADR accepted is not Done until the matching change is on `main`.

## Done (locks landed)

- Multi-protocol option B without Signal in v1 release builds — `0001`, `0004`, amended for local builds by `0011`
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
- AGPL helper decision is Later — #38, ADR `0012`, on `main` via #75.
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

- [ ] #34 — WhatsApp experimental linked-device inbox (feature `whatsapp-web`, `0001`). Local-only AGPL (`0011`). Release builds and OS zips never enable it. Honest ToS labels. Full-screen ToS/ban gate before QR or pair. Never call it reliable.
- [ ] #36 — Slack workspace-app OAuth inbox (feature `slack-oauth`, `0008`). Official OAuth v2 only. Not a personal desktop clone.

### Local-only AGPL

`0011` (2026-09-24). Release builds and OS zips never enable these features. The helper decision is Later (`0012`, #38).

## Spike (scaffold, not the default UI)

- `whatsapp-web` — experimental linked-device scaffold on `whatsapp-rust` (oxidezap), git revision pinned the same way ZapFast pins it. Local-only AGPL (`wacore-libsignal`, `0011`). Off unless that cargo feature is enabled. Default CI does not enable it. Release builds and OS zips never enable it. No ready WhatsApp account in the default build. Full-screen ToS/ban gate before QR or pair. Work continues in #34.
- `discord-bot` — bot/OAuth guild inbox on twilight HTTP (`0009`). Off by default. Shows when the feature is compiled. Not a personal Discord client. On `main` via #35.
- `slack-oauth` — workspace-app OAuth v2 / Socket Mode scaffold on `slack-morphism` (`0008`). Off by default. No Slack auth UI in the default shell. Work continues in #36.

## Explicitly not next

- AGPL crates in release builds and in OS zips. Features `whatsapp-web` and `signal-local` stay off there (`0011`).
- gpui / gpui-ce toolkit switch — parked until thinwire-shaped measurements exist (`0002`).
- A Signal path in the release binary. Feature `signal-local` is on `main` and stays local-only (#39, #66).
- The AGPL helper stays unbuilt. ADR `0012` is Later (#38, #75).

## Pointers

- Decisions: `decisions/`
- Agent rules: `AGENTS.md` (symlink `CLAUDE.md`)
