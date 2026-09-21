# Live TDLib replaces the Telegram stub

**Status:** accepted (api credentials model amended by [[decisions/0007-publisher-telegram-api-credentials]])

## Context
Step 3 landed Telegram stub login + system theme (`0005`) on main. User (2026-09-21) locked the next beat: live TDLib / tdlib-rs, replacing the stub. Critic live-TDLib pass (same day) added CI, db path, and ready-state constraints. Credential ownership later locked in `0007`.

## Decision
Replace the Telegram stub with a real TDLib path via `tdlib-rs` (or equivalent TDLib binding). Keep Telegram as the supported “fast and reliable” adapter under option B (`0001`).

Hard constraints:
- Client create, `receive`, file downloads, and all TDLib / FFI work stay on the tokio worker only — never on the egui thread. UI only gets events or polls.
- **App credentials** (`api_id` / `api_hash`): see `0007` — publisher inject for official builds; optional keychain override; never in MIT tree or public CI.
- **User session / TDLib db encryption keys:** OS keychain (or equivalent). Never on `AdapterCommand`, never in committed env files, CI logs, or crash dumps. Redact tracing. Gitignore `td.binlog` and TDLib db dirs.
- TDLib on-disk db lives under the platform app-data dir with user-only permissions — not under the project tree or world-readable home paths.
- Default CI: compile with stubs or a feature-gated `telegram-tdlib` build **without** network login. Live integration tests are manual or secret-gated; never on fork PRs.
- Drop the on-screen stub / “no live TDLib” banner only after `authorizationStateReady`. Until then show connecting/error from TDLib — do not advance as if logged in.
- Unfinished protocols stay gated or “not ready.”

## Consequences
Builder implements live TDLib on thinwire and lands this ADR (and `0007`) in-repo with the PR.

Rejected: keeping the stub as the shipped Telegram path; TDLib sync/FFI on the UI thread; CI that requires a real Telegram login on every PR.
