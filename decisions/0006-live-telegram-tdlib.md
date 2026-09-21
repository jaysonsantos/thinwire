# Live Telegram TDLib auth

**Status:** accepted

## Context

[#11](https://github.com/jaysonsantos/thinwire/pull/11) landed the messenger-ux Telegram screens (`api_id` / `api_hash` → phone → code → optional 2FA) and OS keychain attach/flush. Those screens still advanced on the UI thread and labeled themselves a stub. v1 needs a real official TDLib / `tdlib-rs` client without blocking egui, without putting secrets on `AdapterCommand`, and without requiring native TDLib in CI.

## Decision

- Telegram auth is event-driven. The UI writes secrets into the existing `SecretStore` / `TelegramSecretVault` map, enqueues `AdapterCommand::TelegramAuth { step }` (step enum only), and applies `AdapterEvent::TelegramAuth { phase }` on the next poll.
- Default workspace builds keep feature `telegram-tdlib` **off**. CI does not download or link TDLib. The adapter still runs the auth state machine and reports `TelegramAuthPhase::Unavailable`. The shell shows a clear “TDLib unavailable” banner — not a live-login stub.
- `--features telegram-tdlib` compiles `tdlib-rs` (`download-tdlib`). A dedicated receive thread plus tokio tasks perform FFI and network I/O. TDLib authorization states map onto the same phases the UI already understands.
- Persistent vault keys (OS keychain when attached): `api_id`, `api_hash`, session marker. Ephemeral (memory only): phone, code, 2FA password.
- WhatsApp, Discord, and Slack stay experimental / not-ready chips. No Discord self-bots. Signal stays out of v1 (ADR 0004). Theme default stays System (ADR 0005).

## Consequences

CI feature matrix: `lint` / `test` / `build` stay feature-off. Local live login needs `--features telegram-tdlib` after a TDLib install (the crate downloads a prebuilt `tdjson` when that feature is on). README Critic risk bullets stay verbatim. Never log or commit `api_id`, `api_hash`, phone, code, password, or session material.
