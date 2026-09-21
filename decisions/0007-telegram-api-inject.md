# Telegram API credentials: publisher inject and keychain override

**Status:** accepted

**Supersession:** Session, OTP, and TDLib database encryption-key storage stay on [0006](0006-live-telegram-tdlib.md). This decision only changes how `api_id` / `api_hash` are supplied.

## Context

Council lock (2026-09-21) forbids shipping every user through my.telegram.org on first run. Official binaries must carry a publisher `api_id` / `api_hash` pair that is **not** in the MIT git tree and **not** injected by public CI for fork PRs.

## Decision

- Official / publisher artifacts inject `TELEGRAM_API_ID` and `TELEGRAM_API_HASH` at compile or release time (`option_env!`). Values never land in source, committed `.env`, or public fork-PR CI.
- After first run of an official binary, login is **phone → code → optional 2FA** only.
- A power-user keychain override (Advanced UI) may store a custom pair. Override wins over the publisher pair.
- Dev builds without inject show a clear “set credentials / rebuild with TELEGRAM_API_ID” path. Never commit sample Telegram credentials.
- The stub / unauthorized banner drops only on TDLib `authorizationStateReady` (ADR 0006 events). Compiling `telegram-tdlib` is not enough.

## Consequences

`TelegramApiSource` + `resolve_telegram_api` sit in `thinwire-protocol`. Commands and logs still carry no secrets. Public `os-zips` on `main` stay unsigned CI toys without publisher inject unless a private release pipeline sets the env vars.
