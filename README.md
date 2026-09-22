# thinwire

Thin, low-resource desktop messenger shell in **Rust + egui**.
One UI for many protocols. Targets **Windows, macOS, and Linux**.
The UI thread must never block; protocol I/O runs async off the main thread.

## Status

Inbox shell (account switcher + conversations on the left, thread in the center) with protocol adapters behind a tokio channel. The UI only polls events; workers must not call egui APIs.

v1 protocols are Telegram, WhatsApp (experimental), Discord (bot/OAuth inbox only), and Slack OAuth. Signal is out of v1; this MIT binary does not link libsignal or Presage.

First-run and Add account offer Telegram (TDLib) only. Official binaries inject `api_id` / `api_hash` at compile time, so end users sign in with phone → code → optional 2FA. The primary login UI never opens on paste-api fields. Dev builds without inject show credentials missing (rebuild with `TELEGRAM_API_ID`, or Advanced keychain override) and do not send every user to my.telegram.org. Slack, WhatsApp, and Discord protocol stubs remain; their auth UI is not in this beat. Discord user-account / self-bot fields do not exist. Default CI builds keep `telegram-tdlib` off so they do not link or download TDLib; the unauthorized banner drops only after TDLib Ready. After Ready, the inbox loads the main chat list, opens a chat for recent messages, and sends text on the worker thread. Enable `--features telegram-tdlib` locally after a TDLib install for the live `tdlib-rs` client. Appearance defaults to the OS light/dark theme (System).

## Protocol support (honest)

| Protocol | Path | Support language |
| --- | --- | --- |
| Telegram | Official TDLib via Rust bindings (`tdlib-rs`) | Supported goal |
| Slack | Official Slack OAuth / API (workspace app) | Supported goal |
| WhatsApp | Unofficial Web / linked-device style (ZapFast / whatsapp-rust inspired) | **Experimental** |
| Discord | Bot/OAuth inbox only — no Discord user self-bots / personal DMs | Constrained / experimental |

## Risk notice (required)

1. WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can get the user’s personal account banned or terminated. License-clean crates do not grant Meta or Discord permission.
2. Do not call WhatsApp or Discord “reliable.” Unofficial WhatsApp clients and Discord user-account / self-bot paths can break or violate ToS.
3. “Fast and reliable” applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). WhatsApp is experimental and Discord is bot/OAuth inbox only. The app must not market them as production messaging.

## License

MIT. Keep third-party notices (including Boost for TDLib if bundled).

## Build

`rust-toolchain.toml` pins Rust 1.98.1. A Nix flake supplies the same tools as CI. `.envrc` stays local.

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p thinwire
```

With Nix:

```bash
nix develop --command scripts/lint.sh
nix develop --command scripts/test.sh
```

CI feature matrix: `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` run **without** `telegram-tdlib`. That is the merge-critical default.

Live TDLib (local install; `tdlib-rs` downloads a prebuilt `tdjson`; never commit `api_id` / `api_hash` / session strings):

```bash
# Official / local inject — export in the private release environment, not in git.
TELEGRAM_API_ID= TELEGRAM_API_HASH= cargo build -p thinwire --features telegram-tdlib
```

Official main OS zips read `TELEGRAM_API_ID` and `TELEGRAM_API_HASH` from GitHub repository secrets. Local builds export the same names before cargo (empty placeholders above). Public CI never sets them. A keychain override in Advanced wins over the publisher pair when both exist. Linux live builds need `libc++-dev` and `libc++abi-dev` because the feature statically links the prebuilt TDLib.

Telegram `api_id`, `api_hash`, and session material go to the OS secret store (`keyring`):

| Platform | Store |
| --- | --- |
| macOS | Keychain |
| Windows | Credential Manager |
| Linux | Kernel keyring (keyutils). Session-scoped; no D-Bus Secret Service required. |
| Linux headless / CI | Same keyutils probe, then in-memory if the kernel store is unavailable. Not written to a file. |

Set `THINWIRE_KEYRING=memory` to skip the OS keychain (CI and local headless). The UI thread only reads/writes an in-memory map; OS keychain attach and flush run on a tokio `spawn_blocking` worker. Never put secrets in the repo, `.env` committed files, logs, or CI artifacts. Tests use the memory backend and stay green without a desktop keychain or TDLib.

Theme preference is `System` (follow the OS, including live `ThemeChanged` updates), `Light`, or `Dark`. A missing `settings.toml` means System. The file lives under the platform config dir (`~/.config/thinwire/settings.toml` on Linux) and stores only the mode enum.

There is no distroless GUI container. This is a desktop egui app.

Pushes to `main` upload unsigned OS zip artifacts for Linux, macOS, and Windows (`telegram-tdlib` on, publisher credentials injected). Retention is 7 days. These zips are not a release. They are not signed. Each zip includes the MIT `LICENSE` and `THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt` (TDLib's Boost Software License). Linux zips also include the copyright files for the shipped `libc++`, `libc++abi`, and `libunwind`, plus the Apache-2.0 text those files cite. The workflow fails closed when `TELEGRAM_API_ID` or `TELEGRAM_API_HASH` is missing. Secret values live in GitHub repository secrets and in arcoiro under the thinwire path (SOPS + Terraform, not watchkeep), not in this tree. Public CI (`ci.yml`) and pull requests do not set them.

## Design rules

- MIT public repo under `jaysonsantos/thinwire`
- Do not infringe licenses of reference projects (e.g. ZapFast); preserve notices
- Prefer official APIs when they exist
- Never automate Discord as a normal user account
- Do not link AGPL libsignal / Presage into this binary
- Never commit or log Telegram `api_id` / `api_hash` / session strings
- Default appearance is System theme (ADR 0005)
- Live Telegram TDLib is feature-gated (`0006-live-tdlib`). Default CI stays `telegram-tdlib` off.
- Official Telegram `api_id` / `api_hash` are publisher inject (`0007-publisher-telegram-api-credentials`). Main OS zips read `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` from repository secrets; local builds export them before cargo; public CI never sets them. No embed in source. Optional Advanced keychain override is not the primary login path.
