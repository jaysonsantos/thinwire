# Option B: multi-protocol at launch with honest ToS language

**Status:** accepted

**Supersession:** Council lock S2 takes Signal out of v1. See [0004-signal-out-of-v1.md](0004-signal-out-of-v1.md). This ADR remains the record of option B versus Telegram-only; do not treat Signal-in-v1 as current product lock.

## Context
Product-council (2026-09-20) locked a public MIT Rust+egui desktop multi-messenger for Windows, macOS, and Linux. The user chose option B over Telegram-first. Repo: https://github.com/jaysonsantos/thinwire (name filament was rejected for Google PBR collision).

## Decision
v1 is a multi-protocol shell, not Telegram-only. Current v1 protocols (after S2) are Telegram, WhatsApp (experimental), Discord bot/OAuth inbox only, and Slack OAuth.

README must include Critic’s three shipped-risk bullets. They must not treat Signal as an in-app v1 module:
1. WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can ban or terminate the user’s personal account. License-clean crates do not grant Meta or Discord permission.
2. Do not call WhatsApp or Discord reliable. Unofficial WhatsApp clients and Discord user-account / self-bot paths can break or violate ToS.
3. Fast and reliable applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). WhatsApp is experimental and Discord is bot/OAuth inbox only. The app must not market them as production messaging.

Discord v1 is bot/OAuth inbox only, not personal DMs — no self-bots. Telegram: TDLib/tdlib-rs preferred. Slack: official OAuth.

## Consequences
Public product cannot claim reliability for unofficial protocols. Builder follows jayson-project-setup and jayson-rust-conventions. Layout expectations: flake.nix, GHA with CI guard `check` job, workspace crates, linters (prek).

Rejected: Telegram-only v1 (option A); name filament.
