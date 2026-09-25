# thinwire — agent notes

## Product lock (S2)

- v1 protocols: Telegram, WhatsApp (experimental), Discord (bot/OAuth inbox only), Slack OAuth
- Signal is out of v1 release builds. Planned feature `signal-local` (#39, ADR `0011-agpl-protocols-local-only`) will link Presage and libsignal
- WhatsApp feature `whatsapp-web` is local-only. It can link AGPL `wacore-libsignal`. Release builds and OS zips never enable `whatsapp-web` or `signal-local` (`0011`)
- Public CI can download AGPL source. Cargo fetches every git dependency in `Cargo.lock`, including optional ones. Public CI does not build that code. Public CI does not link that code. Releases do not contain it (`0011`, 2026-09-25)
- Code that links an AGPL library moves into an AGPL-licensed crate folder in this repo (#77). The root license stays MIT
- README must keep the three risk bullets
- Never claim WhatsApp or Discord personal clients are “reliable”
- Discord: no self-bots / user-account automation
- UI: egui + eframe; protocol work off the UI thread
- WhatsApp, Discord, and Slack work runs in parallel with Telegram (lock change 2026-09-23). It does not wait for "Telegram feels usable"
- Default build: First-run / Add account offer Telegram only. WA / Discord / Slack auth UI exists only when its cargo feature is on. Default and public CI stay feature-off
- Theme default is System (follow OS light/dark live via egui `system_theme`; persist System \| Light \| Dark)
- Ordered next work: see `ROADMAP.md`

## Stack

- Rust 2024 workspace, egui/eframe, tokio for async adapters
- Telegram: TDLib / tdlib-rs preferred
- WhatsApp: unofficial linked-device path inspired by ZapFast (MIT) — ToS risk. Experimental spike is feature `whatsapp-web` (`whatsapp-rust`, git rev pinned). The feature is local-only because it links AGPL `wacore-libsignal` (`0011`). Release builds and OS zips never enable it. Not the default UI. Default CI stays feature-off. Full-screen ToS/ban gate before any QR or pair UI. Session file stays in app-data. Never call it reliable.
- Discord: bot/OAuth guild inbox only — no self-bots / personal DMs / user tokens. Feature `discord-bot` (twilight) is off by default. The inbox must not wait for Telegram messages (lock change 2026-09-23); the code gate that waited is removed (`adfbb8c`, PR #40). ADR `0009-discord-bot-inbox-spike`. Default CI stays feature-off.
- Slack: official OAuth only — workspace app, not a personal desktop clone
- Signal: out of v1 release builds (S2, amended by `0011`). Planned feature `signal-local` (#39) will link Presage and libsignal. Release builds and OS zips never enable it. Default CI stays feature-off.
- Secrets: `keyring` OS store for Telegram `api_id` / `api_hash` / session and the Discord bot token (`discord.bot_token`). Phone / code / 2FA stay in the memory vault only. UI thread is memory-only; OS I/O is `spawn_blocking`. `THINWIRE_KEYRING=memory` for CI/headless. Never log secrets. Never put secrets on `AdapterCommand`. Never commit a Discord token.
- Telegram live client is feature `telegram-tdlib` (`tdlib-rs`). Default CI stays feature-off. Unauthorized banner drops only on TDLib Ready. ADR `0006-live-tdlib`.
- Official `api_id` / `api_hash`: compile-time `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` inject, never in git or public fork-PR CI. Official main OS zips (`.github/workflows/os-zips.yml`) read those names from GitHub repository secrets; local builds export them before cargo; public `ci.yml` never sets them. Values live in GitHub secrets and in arcoiro under the thinwire path (SOPS + Terraform, not watchkeep), not in this tree. Optional Advanced keychain override wins and is not the primary login path. Dev without inject shows credentials missing (not my.telegram.org). ADR `0007-publisher-telegram-api-credentials`. End-user official UX is phone → code → optional 2FA.
- Slack workspace-app spike is feature `slack-oauth` (`slack-morphism` OAuth v2 / Socket Mode; `hyper` on from 2026-09-24, `axum` off). Default CI stays feature-off. No Slack auth UI in the default shell. Client id, client secret, app-level token, and workspace bot token use the publisher inject / OS keychain pattern. ADR `0008-slack-oauth-workspace-spike`.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/thinwire/` | Desktop binary: egui frontend. Draws the core view and sends intents |
| `crates/thinwire-core/` | Frontend-independent core: state, `Intent`, view, change signal, secret store, settings, host wiring. No egui / eframe / winit (ADR `0010`) |
| `crates/thinwire-protocol/` | `ProtocolAdapter` trait, host channel, capability metadata, Critic risk strings, and the Telegram / Slack / WhatsApp / Discord adapters (`telegram/`, `slack/`, `whatsapp/`, `discord/`) |
| `decisions/` | ADRs (0001 option B, 0002 glow, 0003 main-only artifacts, 0004 Signal out, 0005 system theme, `0006-live-tdlib`, `0007-publisher-telegram-api-credentials`, `0008-slack-oauth-workspace-spike`, `0009-discord-bot-inbox-spike`, `0010-frontend-independent-core`, `0011-agpl-protocols-local-only`) |
| `ROADMAP.md` | Ordered product-council todo list (ADRs stay in `decisions/`) |
| `scripts/` | `lint.sh`, `test.sh`, `all.sh`, `release.sh`, `check-core-deps.sh` — CI calls the same scripts |
| `flake.nix` | Dev shell. `.envrc` stays local (`source_up_if_exists` / `use flake` / `dotenv_if_exists .env`) |
| `.pre-commit-config.yaml` | prek hooks (fmt, clippy, taplo, typos, nixfmt, shellcheck, gitleaks, zizmor) |
| `.github/workflows/ci.yml` | Parallel lint/test/build plus the `check` guard |
| `.github/workflows/os-zips.yml` | Main-only unsigned OS zips with `telegram-tdlib` and publisher secrets (ADR 0003, 0007) |
| `.github/workflows/release-tag.yml` | Manual tag. No distroless GUI image |

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p thinwire
cargo run -p thinwire-core --example headless
nix develop --command scripts/lint.sh
nix develop --command scripts/test.sh
```

There is no server container. Do not add a distroless GUI Docker image.
