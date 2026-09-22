# Thinwire roadmap

Ordered work from product-council ADRs (`decisions/0001`–`0007`). ADRs are decisions; this file is the todo list.

Rule: an ADR accepted is not Done until the matching change is on `main`.

## Done (locks landed)

- Multi-protocol option B without Signal in v1 — `0001`, `0004`
- egui + eframe + glow (no wgpu) — `0002`
- Main-only unsigned CI artifacts + caveat — `0003`
- System light/dark by default — `0005`
- Live TDLib path + hard constraints — `0006` (on `main` via #15 / `eff31de`)
- Official release inject for main OS zips — `0007`. `.github/workflows/os-zips.yml` builds `--features telegram-tdlib` with `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` from GitHub repository secrets and fails closed when either secret is missing. Real values live in GitHub secrets and SOPS (arcoiro), not in this MIT tree. Public `ci.yml` and pull requests do not set them. The job refuses to upload a credentials-missing binary until those repository secrets exist.

## Next (locked order)

1. **Chat list + messages** on TDLib after `authorizationStateReady` (auth alone is not enough).
2. **Other protocols** (only after Telegram feels usable):
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
