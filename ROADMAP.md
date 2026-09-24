# Thinwire roadmap

Ordered work from product-council ADRs (`decisions/0001`–`0009`; `0010` comes with #33; `0011` is the local-only AGPL lock). ADRs are decisions; this file is the todo list.

Rule: an ADR accepted is not Done until the matching change is on `main`.

## Done (locks landed)

- Multi-protocol option B without Signal in v1 release builds — `0001`, `0004`, amended for local builds by `0011`
- egui + eframe + glow (no wgpu) — `0002`
- Main-only unsigned CI artifacts + caveat — `0003`
- System light/dark by default — `0005`
- Live TDLib path + hard constraints — `0006` (on `main` via #15 / `eff31de`)
- Official release inject for main OS zips — `0007` (on `main` via #19 / `5c46222`)
- Chat list + messages after `authorizationStateReady`. The worker loads the main chat list, opens a chat for recent messages, and sends text. Auth alone is not the inbox. WhatsApp, Discord, and Slack stay not-ready.
- Telegram feels usable — on `main` via #40 / `b670536`.
  - Resume a saved session at launch.
  - Chat list and thread scroll, plus loading states.
  - Compose and send: Enter sends, drafts per chat, failed text stays with Retry.
  - Login steps: keyboard flow and clear errors.
  - Message time, sender, and date separators.
  - Live-test fixes: TDLib closes at exit, a keychain-missing notice, and libc++ / mesa in the Nix dev shell.
  - The phone step names the kept data folder once.

## Next (parallel tracks)

Product lock change (2026-09-23): WhatsApp, Discord, and Slack run in parallel with Telegram. They do not wait for "Telegram feels usable". Each track keeps its cargo feature off in the default build and in public CI. Every safety rule stays: the WhatsApp ToS/ban gate, no Discord self-bots, and official Slack OAuth only.

### Telegram

1. **Telegram UX follow-ups**
    - [ ] #30 — load older messages when you scroll up in a chat.
    - [ ] #31 — open a chat from a click anywhere on its inbox row.
    - [ ] Follow-up: remove jargon from the account row ("status: stubbed", "Supported · TDLib") and from worker status text ("TDLib <code>").

### Shared

- [ ] #32 — desktop notifications for new messages.
- [ ] #33 — move app state into a frontend-independent core library (`crates/thinwire-core`, no egui / eframe / winit). ADR `0010-frontend-independent-core` records the boundary. The egui binary becomes one frontend.

### Other protocols (parallel with Telegram)

- [ ] #34 — WhatsApp experimental linked-device inbox (feature `whatsapp-web`, `0001`). Local-only AGPL (`0011`). Release builds and OS zips never enable it. Honest ToS labels. Full-screen ToS/ban gate before QR or pair. Never call it reliable.
- [ ] #35 — Discord bot/OAuth guild inbox (feature `discord-bot`, `0009`). No self-bots, no user tokens, no personal DMs. Not a personal client. The Telegram wait is gone on `main` (#40).
- [ ] #36 — Slack workspace-app OAuth inbox (feature `slack-oauth`, `0008`). Official OAuth v2 only. Not a personal desktop clone.

### Local-only AGPL

`0011` (2026-09-24). Release builds and OS zips never enable these features.

- [ ] #38 — research an AGPL helper process for WhatsApp and Signal. The output is an ADR with go, no-go, or later.
- [ ] #39 — Signal inbox behind feature `signal-local` (Presage / libsignal). Off by default. A full-screen notice comes before link. The notice says experimental, local build only, and AGPL.

## Spike (scaffold, not the default UI)

- `whatsapp-web` — experimental linked-device scaffold on `whatsapp-rust` (oxidezap), git revision pinned the same way ZapFast pins it. Local-only AGPL (`wacore-libsignal`, `0011`). Off unless that cargo feature is enabled. Default CI does not enable it. Release builds and OS zips never enable it. No ready WhatsApp account in the default build. Full-screen ToS/ban gate before QR or pair. Work continues in #34.
- `discord-bot` — bot/OAuth guild inbox scaffold on twilight (`0009`). Off by default. Not a personal Discord client. Work continues in #35.
- `slack-oauth` — workspace-app OAuth v2 / Socket Mode scaffold on `slack-morphism` (`0008`). Off by default. No Slack auth UI in the default shell. Work continues in #36.

## Explicitly not next

- AGPL crates in release builds and in OS zips. Features `whatsapp-web` and `signal-local` stay off there (`0011`).
- gpui / gpui-ce toolkit switch — parked until thinwire-shaped measurements exist (`0002`).
- A Signal path in the release binary. Local feature `signal-local` is #39. The helper process is #38.

## Pointers

- Decisions: `decisions/`
- Agent rules: `AGENTS.md` (symlink `CLAUDE.md`)
