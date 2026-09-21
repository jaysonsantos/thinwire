# Thinwire roadmap

Ordered work from product-council ADRs (`decisions/0001`–`0007`). ADRs are decisions; this file is the todo list.

Rule: an ADR accepted is not Done until the matching change is on `main`.

## Done (locks landed)

- Multi-protocol option B without Signal in v1 — `0001`, `0004`
- egui + eframe + glow (no wgpu) — `0002`
- Main-only unsigned CI artifacts + caveat — `0003`
- System light/dark by default — `0005`
- Live TDLib path + hard constraints — `0006` (on `main` via #15 / `eff31de`)
- Publisher-owned `api_id`/`api_hash` inject *model* — `0007` (decision landed; release wiring is still Next)

## Next (locked order)

1. **Official release inject** for `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` from a private secret store (or publisher machine keychain for local official builds). Never in the MIT tree or public CI. Wire primary login so official builds do not need Advanced paste (`0007`).
2. **Chat list + messages** on TDLib after `authorizationStateReady` (auth alone is not enough).
3. **Other protocols** (only after Telegram feels usable):
  - WhatsApp — experimental, honest ToS labels (`0001`)
  - Discord — bot/OAuth inbox only, no self-bots (`0001`)
  - Slack — official OAuth (`0001`)

## Explicitly not next

- Signal in the MIT tree — `0004` (S2). Later only via a separate AGPL helper ADR if revisited.
- gpui / gpui-ce toolkit switch — parked until thinwire-shaped measurements exist (`0002`).
- Cargo-feature Signal or AGPL link into MIT thinwire — rejected.

## Pointers

- Decisions: `decisions/`
- Agent rules: `AGENTS.md` (symlink `CLAUDE.md`)
