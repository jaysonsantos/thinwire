# Thinwire roadmap

Ordered work from product-council ADRs (`decisions/0001`–`0009`). ADRs are decisions; this file is the todo list.

Rule: an ADR accepted is not Done until the matching change is on `main`.

## Done (locks landed)

- Multi-protocol option B without Signal in v1 — `0001`, `0004`
- egui + eframe + glow (no wgpu) — `0002`
- Main-only unsigned CI artifacts + caveat — `0003`
- System light/dark by default — `0005`
- Live TDLib path + hard constraints — `0006` (on `main` via #15 / `eff31de`)
- Official release inject for main OS zips — `0007` (on `main` via #19 / `5c46222`)
- Chat list + messages after `authorizationStateReady`. The worker loads the main chat list, opens a chat for recent messages, and sends text. Auth alone is not the inbox. WhatsApp, Discord, and Slack stay not-ready.

## Next (locked order)

1. **Other protocols** (only after Telegram feels usable):
  - WhatsApp — experimental, honest ToS labels (`0001`). Feature `whatsapp-web` is a scaffold spike, not the default UI, and not Done.
  - Discord — bot/OAuth inbox only, no self-bots (`0001`). Feature-gated twilight spike `discord-bot` is scaffolded (`0009`). The shell stays hidden until Telegram messages work. Not a shipped inbox and not a personal client.
  - Slack — official OAuth (`0001`). Feature-off spike `slack-oauth` records the workspace-app shape (`0008`) and stays out of the default UI.

## Spike (scaffold, not the default UI)

- `whatsapp-web` — experimental linked-device scaffold on `whatsapp-rust` (oxidezap), git revision pinned the same way ZapFast pins it. Off unless that cargo feature is enabled. Default CI does not enable it. No ready WhatsApp account in the default build. Full-screen ToS/ban gate before QR or pair. Telegram chat list and messages are on `main`; this spike stays off by default.
- `discord-bot` — bot/OAuth guild inbox scaffold on twilight (`0009`). Off by default. Hidden until Telegram messages exist. Not a personal Discord client.
- `slack-oauth` — workspace-app OAuth v2 / Socket Mode scaffold on `slack-morphism` (`0008`). Off by default. No Slack auth UI in the default shell.

## Explicitly not next

- Signal in the MIT tree — `0004` (S2). Later only via a separate AGPL helper ADR if revisited.
- gpui / gpui-ce toolkit switch — parked until thinwire-shaped measurements exist (`0002`).
- Cargo-feature Signal or AGPL link into MIT thinwire — rejected.

## Pointers

- Decisions: `decisions/`
- Agent rules: `AGENTS.md` (symlink `CLAUDE.md`)
