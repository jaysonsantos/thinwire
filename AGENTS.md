# thinwire — agent notes

## Product lock (S2)

- v1 protocols: Telegram, WhatsApp (experimental), Discord (bot/OAuth inbox only), Slack OAuth, Signal (experimental; ships with #248 as an AGPL helper process, ADR `0013`)
- Accepted plan, ADR `0013-ship-all-protocols-agpl-helpers` (2026-10-08): release builds ship every protocol, with WhatsApp and Signal in AGPL-3.0-only helper binaries over IPC. It supersedes `0004` (Signal out of v1) and `0011` (AGPL protocols local-only). Implementation is pending: the WhatsApp helper in #246, the Signal helper in #248. Until they land, feature `signal-local` (#39) is the only Signal path and stays local-only. It links Presage and libsignal
- WhatsApp feature `whatsapp-web` can link AGPL `wacore-libsignal`. The MIT app binary never links AGPL code: release builds and OS zips never enable `whatsapp-web` or `signal-local` in the app. Under `0013` they get WhatsApp and Signal only through the helper binaries
- Public CI can download AGPL source. Cargo fetches every git dependency in `Cargo.lock`, including optional ones. Until #246 lands, public CI does not build or link that code, and releases do not contain it. Under `0013`, the default public CI jobs stay feature-off, and one separate release-feature job builds and tests every feature that a release ships (`telegram-tdlib`, `slack-oauth`, `discord-bot` and the helper crates). Its workflow change needs an owner push, because the bot token has no workflow scope. The helper crates are the only place AGPL code may link
- Code that links an AGPL library moves into an AGPL-licensed crate folder in this repo (#77). The root license stays MIT
- README must keep the three risk bullets
- Never claim WhatsApp or Discord personal clients are “reliable”
- Discord: no self-bots / user-account automation
- UI: egui + eframe; protocol work off the UI thread
- WhatsApp, Discord, and Slack work runs in parallel with Telegram (lock change 2026-09-23). It does not wait for "Telegram feels usable"
- Default build: First-run / Add account offer Telegram only. WA / Discord / Slack auth UI exists only when its cargo feature is on. Default and public CI jobs stay feature-off, except the release-feature job of `0013`
- Theme default is System (follow OS light/dark live via egui `system_theme`; persist System \| Light \| Dark)
- README image points at `several_protocols-{light,dark}-1100x720.png` in `crates/thinwire/tests/snapshots/`; if that test is renamed or removed, update the README link
- Ordered next work: see `ROADMAP.md`

## Stack

- Rust 2024 workspace, egui/eframe, tokio for async adapters
- Telegram: TDLib / tdlib-rs preferred
- WhatsApp: unofficial linked-device path on `whatsapp-rust` (oxidezap) — ToS risk. Experimental spike is feature `whatsapp-web` (`whatsapp-rust`, git rev pinned). It links AGPL `wacore-libsignal`, so it stays local-only until the WhatsApp helper of `0013` ships (#246). Release builds and OS zips never enable it in the app. Not the default UI. Default CI stays feature-off. Full-screen ToS/ban gate before any QR or pair UI. Session file stays in app-data. Never call it reliable. The client is `crates/thinwire-whatsapp` (AGPL-3.0-only, #77). `thinwire-protocol` keeps an MIT stub and `WhatsAppPhoneVault`. It does not depend on the AGPL crate.
- Discord: bot/OAuth guild inbox only — no self-bots / personal DMs / user tokens. Feature `discord-bot` (twilight HTTP) lists guild channels the bot can read, loads history, and sends as the bot. It runs in parallel with Telegram and does not wait for Telegram messages (lock change 2026-09-23; the code gate that waited is removed, `adfbb8c`, PR #40). Tests use a fake `DiscordApi`. ADR `0009-discord-bot-inbox-spike`. Default CI stays feature-off.
- Slack: official OAuth only — workspace app, not a personal desktop clone
- Signal: planned for release builds as the AGPL `thinwire-signal-helper` (`0013`, supersedes S2 / `0004` and `0011`). Implementation pending in #248. Until then feature `signal-local` (#39) is local-only. The client is `crates/thinwire-signal` (AGPL-3.0-only). It links Presage and libsignal. `thinwire-protocol` does not depend on it. Release builds and OS zips never enable it. Default CI stays feature-off.
- Secrets: `keyring` OS store for Telegram `api_id` / `api_hash` / session and the Discord bot token (`discord.bot_token`). Phone / code / 2FA stay in the memory vault only. UI thread is memory-only; OS I/O is `spawn_blocking`. `THINWIRE_KEYRING=memory` for CI/headless. Never log secrets. Never put secrets on `AdapterCommand`. Never commit a Discord token.
- Telegram live client is feature `telegram-tdlib` (`tdlib-rs`). Default CI stays feature-off. Unauthorized banner drops only on TDLib Ready. ADR `0006-live-tdlib`.
- Official `api_id` / `api_hash`: compile-time `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` inject, never in git or public fork-PR CI. Official main OS zips (`.github/workflows/os-zips.yml`) read those names from GitHub repository secrets; local builds export them before cargo; public `ci.yml` never sets them. Values live in GitHub secrets and in arcoiro under the thinwire path (SOPS + Terraform, not watchkeep), not in this tree. Optional Advanced keychain override wins and is not the primary login path. Dev without inject shows credentials missing (not my.telegram.org). ADR `0007-publisher-telegram-api-credentials`. End-user official UX is phone → code → optional 2FA.
- Slack workspace-app spike is feature `slack-oauth` (`slack-morphism` OAuth v2 / Socket Mode; `hyper` on from 2026-09-24, `axum` off). Default CI stays feature-off. No Slack auth UI in the default shell. Client id, client secret, app-level token, and workspace bot token use the publisher inject / OS keychain pattern. ADR `0008-slack-oauth-workspace-spike`.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/thinwire/` | Desktop binary: egui frontend. Draws the core view and sends intents |
| `crates/thinwire-core/` | Frontend-independent core: state, `Intent`, view, change signal, secret store, settings, host wiring, notification rules (`notify`), chats muted in thinwire (`mutes`, #153). No egui / eframe / winit (ADR `0010`) |
| `crates/thinwire-notify/` | OS desktop notifications on their own thread: Linux D-Bus through `notify-rust`, Windows WinRT toasts through `windows`, macOS UNUserNotificationCenter through `mac-usernotifications` inside `Thinwire.app` on macOS 12+ (show-only through `notify-rust` with no bundle or on macOS 11) (#161). No egui (#32) |
| `crates/thinwire-protocol/` | `ProtocolAdapter` trait, host channel, capability metadata, Critic risk strings, and the Telegram / Slack / Discord adapters. Signal and WhatsApp here are MIT stubs |
| `crates/thinwire-signal/` | AGPL-3.0-only Signal adapter. `thinwire` depends on it only with feature `signal-local`. `thinwire-protocol` does not depend on it |
| `crates/thinwire-whatsapp/` | AGPL-3.0-only WhatsApp linked-device adapter (#77). `thinwire` depends on it only with feature `whatsapp-web`. `thinwire-protocol` does not depend on it |
| `decisions/` | ADRs (0001 option B, 0002 glow, 0003 main-only artifacts, 0004 Signal out (superseded by 0013), 0005 system theme, `0006-live-tdlib`, `0007-publisher-telegram-api-credentials`, `0008-slack-oauth-workspace-spike`, `0009-discord-bot-inbox-spike`, `0010-frontend-independent-core`, `0011-agpl-protocols-local-only` (superseded by 0013), `0012-agpl-helper-process` (partly superseded by 0013), `0013-ship-all-protocols-agpl-helpers`) |
| `ROADMAP.md` | Ordered product-council todo list (ADRs stay in `decisions/`) |
| `scripts/` | `lint.sh`, `test.sh`, `all.sh`, `release.sh`, `check-core-deps.sh`, `check-release-tree.sh`, `check-agpl-deps.sh` — CI calls the same scripts |
| `flake.nix` | Dev shell. `.envrc` stays local (`source_up_if_exists` / `use flake` / `dotenv_if_exists .env`) |
| `.pre-commit-config.yaml` | prek hooks (fmt, clippy, taplo, typos, nixfmt, shellcheck, gitleaks, zizmor) |
| `.github/workflows/ci.yml` | Parallel lint/test/build plus the `check` guard. `notify-os` runs clippy and tests of `thinwire-notify` on Windows and macOS, and on macOS stages a signed `Thinwire.app` from a debug build, only when that crate, `scripts/stage-os-artifact.sh`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, or `ci.yml` changes; `check` accepts its skip |
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
