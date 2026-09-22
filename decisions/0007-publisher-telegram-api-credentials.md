# Publisher-owned Telegram api_id / api_hash

**Status:** accepted

## Context
User asked whether thinwire needs a Telegram app and how to store credentials. Researcher: Terms 2.1 require an app-owned `api_id` (my.telegram.org); tdesktop-style clients inject at build time. Critic: do not force every end user through my.telegram.org; do not commit credentials to MIT tree/CI. User locked (2026-09-21). Critic residual: primary login must not open on paste-api fields. Council lock (2026-09-22): GitHub Actions repository secrets win for official main OS-zip inject. Wiring is on `main` via #19 / `5c46222`.

## Decision
Publisher (Jayson / official thinwire releases) creates one Telegram app and owns `api_id` / `api_hash`.

- Official binaries: inject that pair at release/build time from a private secret store (or the publisher’s machine keychain for local official builds).
- Source of truth for official main OS-zip inject (`.github/workflows/os-zips.yml`): GitHub Actions repository secrets `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` win.
- Encrypted inventory copy: arcoiro thinwire Terraform/SOPS (not watchkeep, no separate secrets repo; PR #2812 / `b6292f8`). Rotate both until a sync path exists.
- Import from publisher env files: `APPID` → `TELEGRAM_API_ID`, `APPSECRET` → `TELEGRAM_API_HASH`.
- Scrub plaintext env files after set. Never commit values to the MIT tree or public CI (including fork PRs).
- Placeholders must be obviously invalid. Inject fails closed.
- Optional advanced override: power users may supply their own pair via OS keychain — **not** the primary login path.
- Primary login UI: phone/code (or QR) only. Never open with “paste api_id / api_hash.”
- Dev / unofficial builds without inject: show credentials missing — do **not** send every user to my.telegram.org.
- Normal end users sign in with phone/code only — they do not create their own app id for a normal install.
- Treat `api_hash` as an extractable client secret, not a user password. Sessions and TDLib db encryption keys remain under `0006` (keychain + app-data).
- Never put credentials on `AdapterCommand` or into logs.

## Consequences
Live-TDLib UX drops forced my.telegram.org for every user. Official inject is on `main` (#19 / `5c46222`). If the publisher id is flooded/abused, Telegram can kill it for all official installs — monitor and rotate via new inject.

Rejected: embedding credentials in the public git tree; requiring every end user to paste their own id for a normal install; primary UI that opens on api_id/api_hash fields.
