# Option B: all five protocols at launch with honest ToS language

**Status:** accepted

## Context
Product-council (2026-09-20) locked a public MIT Rust+egui desktop multi-messenger for Windows, macOS, and Linux. The user chose option B over Telegram-first. Repo: https://github.com/jaysonsantos/thinwire (name filament was rejected for Google PBR collision).

## Decision
v1 ships Telegram, WhatsApp, Signal, Discord, and Slack in one non-blocking egui shell.

README must include Critic’s three bullets:
1. WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can ban or terminate the user’s personal account. License-clean crates do not grant Meta or Discord permission.
2. Signal has no supported third-party client API. Breakage and unsigned clients are expected. Do not call Signal, WhatsApp, or Discord reliable.
3. Fast and reliable applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). The other three are experimental modules with ToS risk and must not be marketed as production messaging.

Discord v1 is bot/OAuth inbox only, not personal DMs — no self-bots. Telegram: TDLib/tdlib-rs preferred. Slack: official OAuth.

## Consequences
Public product cannot claim reliability for unofficial protocols. Builder follows jayson-project-setup and jayson-rust-conventions. Layout expectations: flake.nix, GHA with CI guard `check` job, workspace crates, linters (prek).

Rejected: Telegram-only v1 (option A); name filament.
