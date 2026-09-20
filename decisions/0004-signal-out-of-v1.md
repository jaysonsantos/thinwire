# Signal is out of v1 (council lock S2)

**Status:** accepted

## Context

Option B ([0001](0001-option-b-multi-protocol.md)) chose a multi-protocol shell with honest ToS language over Telegram-only. The original option B list included Signal. A public MIT binary that talks to Signal would pull AGPL libsignal / Presage into the product.

Council lock S2 (2026-09-20) takes Signal out of v1.

## Decision

Signal is **out of v1**. Do not link AGPL libsignal or Presage into the MIT thinwire binary. Do not offer Signal in the shell, catalog, or experimental gate. Critic shipped-risk bullets must not treat Signal as an in-app module.

v1 protocols are only:

- Telegram (TDLib / supported goal)
- WhatsApp (experimental, unofficial linked-device; ToS / ban risk stays)
- Discord bot/OAuth inbox only (not personal DMs; no self-bots)
- Slack OAuth

README protocol table and Critic bullets stay honest: WhatsApp is experimental, Discord self-bots are refused, unofficial clients are not called “reliable.”

This supersedes the Signal-in-v1 portion of ADR 0001. Option B otherwise stands (multi-protocol shell, honest ToS language, Discord constrained).

## Consequences

No Signal adapter, `ProtocolId::Signal`, or user-facing Signal auth path in v1. Cargo manifests and the lockfile must not depend on `presage` or `libsignal`. A later major version may revisit Signal only with an explicit license and product decision.
