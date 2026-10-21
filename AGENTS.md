# thinwire — agent notes

## Product lock (S2)

- v1 protocols: Telegram, WhatsApp (experimental), Discord (bot/OAuth inbox only), Slack OAuth, Signal (experimental; AGPL helper process, #248, ADR `0013`)
- Accepted plan, ADR `0013-ship-all-protocols-agpl-helpers` (2026-10-08): release builds ship every protocol, with WhatsApp and Signal in AGPL-3.0-only helper binaries over IPC. It supersedes `0004` (Signal out of v1) and `0011` (AGPL protocols local-only). Both helpers are in the tree: the WhatsApp helper (#246) and the Signal helper (#248)
- The MIT app binary never links AGPL code. Feature `whatsapp-web` has two parts. In `thinwire-whatsapp` it links the AGPL `wacore-libsignal` into the helper program `thinwire-whatsapp-helper`. In `thinwire` and `thinwire-core` it adds the WhatsApp screens and the MIT helper adapter, with no AGPL code. Feature `signal-local` has the same two parts: in `thinwire-signal` it links Presage and libsignal into the helper program `thinwire-signal-helper`, and in `thinwire` and `thinwire-core` it adds the Signal screens and the MIT helper adapter. Release builds and OS zips turn on both parts of both features. No MIT crate depends on `thinwire-whatsapp` or `thinwire-signal`, also not as an optional dependency. `scripts/check-agpl-deps.sh` and `scripts/check-release-tree.sh` enforce this
- Public CI can download AGPL source. Cargo fetches every git dependency in `Cargo.lock`, including optional ones. The default public CI jobs stay feature-off. One separate job, `release-features`, builds and tests every feature that a release ships (`telegram-tdlib`, `slack-oauth`, `discord-bot`, `whatsapp-web`, `signal-local`, and the WhatsApp and Signal helpers with their clients; `0013`). A change to a workflow file needs an owner push, because the bot token has no workflow scope. The helper crates are the only place AGPL code may link
- Code that links an AGPL library moves into an AGPL-licensed crate folder in this repo (#77). The root license stays MIT
- README must keep the three risk bullets
- Never claim WhatsApp or Discord personal clients are “reliable”
- Discord: no self-bots / user-account automation
- UI: egui + eframe; protocol work off the UI thread
- WhatsApp, Discord, and Slack work runs in parallel with Telegram (lock change 2026-09-23). It does not wait for "Telegram feels usable"
- Default build: First-run / Add account offer Telegram only. WA / Discord / Slack auth UI exists only when its cargo feature is on. Default and public CI jobs stay feature-off, except the `release-features` job of `0013`
- A test must hold in every feature build, because `release-features` turns on every release feature. A test of one protocol hides the other account rows with `test_support::show_only`. A test of the first-run screen ends the start-up resume first
- Theme default is System (follow OS light/dark live via egui `system_theme`; persist System \| Light \| Dark)
- README image points at `several_protocols-{light,dark}-1100x720.png` in `crates/thinwire/tests/snapshots/`; if that test is renamed or removed, update the README link
- Ordered next work: see `ROADMAP.md`

## Stack

- Rust 2024 workspace, egui/eframe, tokio for async adapters
- Telegram: TDLib / tdlib-rs preferred
- WhatsApp: unofficial linked-device path on `whatsapp-rust` (oxidezap) — ToS risk. Experimental client is feature `whatsapp-web` (`whatsapp-rust`, git rev pinned). Not the default UI. Default CI stays feature-off. Full-screen ToS/ban gate before any QR or pair UI. Session file stays in app-data. The helper encrypts `device.sqlite` with SQLCipher. The key is random and stays in the OS keychain (`whatsapp.store_key`). The key does not cross the pipe. This encryption does not stop a program that runs as the same user and can read the keychain. A keychain that cannot save the key does not link, and it does not write a store. A keychain read that fails keeps the account row, shows "Can't read the session key from Keychain. Allow access" and Retry, and does not move the store. Retry reads the key again. A missing or wrong key does not delete the store: the helper moves the folder aside and the row shows "Can't open the saved WhatsApp session. Link again. Your old data is kept." Never call it reliable. The client is `crates/thinwire-whatsapp` (AGPL-3.0-only, #77). It links AGPL `wacore-libsignal`, so it runs in the helper program `thinwire-whatsapp-helper` (`0013`, #246), not in the app. `thinwire-protocol` keeps an MIT stub, the MIT helper adapter, and `WhatsAppPhoneVault`. It does not depend on the AGPL crate.
- Helper process rules (`0013`, `0012` section 2 and 4):
    - The app talks to a helper only through `crates/thinwire-ipc`: one JSON object per line on the helper's stdin and stdout. A change that an older peer cannot read must raise `PROTOCOL_VERSION`.
    - `HelperAdapter` (`thinwire-protocol`, module `helper`) is the only code that starts and stops a helper. It holds one process at most. `handle` only queues a message, so no caller waits on the pipe.
    - The helper starts when the user accepts the gate. It stops when the app closes. If it ends, the adapter starts it again (1 s, 2 s, 4 s, 8 s) and sends the gate and the pairing again. After 5 failures the account row shows "Helper stopped." and Restart. Restart links a linked account again. A pairing that did not link ends at that stop: the user starts it again. A pairing also ends when the helper reports `Unlinked` or `AccountEnded`: no restart sends it again.
    - One helper process holds the lock file of the session folder. A second process refuses to run.
    - Each protocol has its own helper program and its own `HelperAdapter`. A stop of one helper does not stop the other. In the core, each protocol has its own pairing generation.
    - The phone number for a pair code crosses the pipe one time, in the `BeginLink` of the user. A restart sends the pairing again with no phone. `Debug` of a wire value shows no user data. Neither side logs message text, phone numbers, QR data, or pair codes.
    - Only the wire protocol goes to the helper's stdout. Logs go to stderr.
    - A helper writes only the log lines of thinwire targets (`helper::helper_log_target`). The protocol libraries log pairing data at level `info`, and the app logs each stderr line of a helper again. No `RUST_LOG` value turns a library target on.
- Discord: bot/OAuth guild inbox only — no self-bots / personal DMs / user tokens. Feature `discord-bot` (twilight HTTP) lists guild channels the bot can read, loads history, and sends as the bot. It runs in parallel with Telegram and does not wait for Telegram messages (lock change 2026-09-23; the code gate that waited is removed, `adfbb8c`, PR #40). Tests use a fake `DiscordApi`. ADR `0009-discord-bot-inbox-spike`. Default CI stays feature-off.
- Slack: official OAuth only — workspace app, not a personal desktop clone
- Signal: unofficial linked secondary device on `presage` — no supported third-party API, expect breakage. Experimental client is feature `signal-local` (`presage`, git rev pinned). Not the default UI. Default CI stays feature-off. Full-screen notice before any provisioning QR code. Session store stays in app-data (`thinwire/signal/`). The helper encrypts the live `session.sqlite`, the pending link file, and every retained `session.sqlite`. One OS keychain key (`signal.store_key`) opens all of them. The key does not cross the pipe. This encryption does not stop a program that runs as the same user and can read the keychain. A keychain that cannot save the key does not link, and it does not write a store. A keychain read that fails keeps the account row and does not move the store. Retry reads the key again. A missing or wrong key does not delete the store: the helper moves the folder aside and the row shows "Can't open the saved Signal session. Link again. Your old data is kept." The client is `crates/thinwire-signal` (AGPL-3.0-only). It links Presage and libsignal, so it runs in the helper program `thinwire-signal-helper` (`0013`, #248; supersedes S2 / `0004` and `0011`), not in the app. `thinwire-protocol` keeps an MIT stub and the MIT helper adapter. It does not depend on the AGPL crate. The build of the client needs `protoc`: the nix shell has it, and other CI jobs run `scripts/install-protoc.sh`.
- Secrets: `keyring` OS store for Telegram `api_id` / `api_hash` / session and the Discord bot token (`discord.bot_token`). Phone / code / 2FA stay in the memory vault only. UI thread is memory-only; OS I/O is `spawn_blocking`. `THINWIRE_KEYRING=memory` for CI/headless. Never log secrets. Never put secrets on `AdapterCommand`. Never commit a Discord token.
- Telegram live client is feature `telegram-tdlib` (`tdlib-rs`). Default CI stays feature-off. Unauthorized banner drops only on TDLib Ready. ADR `0006-live-tdlib`.
- Official `api_id` / `api_hash`: compile-time `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` inject, never in git or public fork-PR CI. Official main OS zips (`.github/workflows/os-zips.yml`) read those names from GitHub repository secrets; local builds export them before cargo; public `ci.yml` never sets them. Values live in GitHub secrets and in arcoiro under the thinwire path (SOPS + Terraform, not watchkeep), not in this tree. Optional Advanced keychain override wins and is not the primary login path. Dev without inject shows credentials missing (not my.telegram.org). ADR `0007-publisher-telegram-api-credentials`. End-user official UX is phone → code → optional 2FA.
- Slack workspace-app spike is feature `slack-oauth` (`slack-morphism` OAuth v2 / Socket Mode; `hyper` on from 2026-09-24, `axum` off). Default CI stays feature-off. No Slack auth UI in the default shell. Client id, client secret, app-level token, and workspace bot token use the publisher inject / OS keychain pattern. ADR `0008-slack-oauth-workspace-spike`.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/thinwire/` | Desktop binary: egui frontend. Draws the core view and sends intents |
| `crates/thinwire-core/` | Frontend-independent core: state, `Intent`, view, change signal, secret store, settings, host wiring, notification rules (`notify`), chats muted in thinwire (`mutes`, #153), the single-instance lock (`instance`) and the show endpoint a second launch uses (`show`, #237). No egui / eframe / winit (ADR `0010`) |
| `crates/thinwire-notify/` | OS desktop notifications on their own thread: Linux D-Bus through `notify-rust`, Windows WinRT toasts through `windows`, macOS UNUserNotificationCenter through `mac-usernotifications` inside `Thinwire.app` on macOS 12+ (show-only through `notify-rust` with no bundle or on macOS 11) (#161). No egui (#32) |
| `crates/thinwire-ipc/` | MIT wire protocol between the app and a helper process: line types, JSON line framing with a 1 MiB limit, `PROTOCOL_VERSION` (`0013`). No dependency on another thinwire crate |
| `crates/thinwire-protocol/` | `ProtocolAdapter` trait, host channel, capability metadata, Critic risk strings, and the Telegram / Slack / Discord adapters. Signal and WhatsApp here are MIT stubs and MIT helper adapters (`whatsapp_helper_adapter`, `signal_helper_adapter`). Module `helper` has the MIT `HelperAdapter` (app side) and `serve` (helper side) |
| `crates/thinwire-signal/` | AGPL-3.0-only Signal linked-device adapter and the helper program `thinwire-signal-helper` (`src/bin/helper.rs`, #248). No MIT crate depends on it. Feature `signal-local` links the client into the helper |
| `crates/thinwire-whatsapp/` | AGPL-3.0-only WhatsApp linked-device adapter (#77) and the helper program `thinwire-whatsapp-helper` (`src/bin/helper.rs`, #246). No MIT crate depends on it. Feature `whatsapp-web` links the client into the helper |
| `decisions/` | ADRs (0001 option B, 0002 glow, 0003 main-only artifacts, 0004 Signal out (superseded by 0013), 0005 system theme, `0006-live-tdlib`, `0007-publisher-telegram-api-credentials`, `0008-slack-oauth-workspace-spike`, `0009-discord-bot-inbox-spike`, `0010-frontend-independent-core`, `0011-agpl-protocols-local-only` (superseded by 0013), `0012-agpl-helper-process` (partly superseded by 0013), `0013-ship-all-protocols-agpl-helpers`) |
| `ROADMAP.md` | Ordered product-council todo list (ADRs stay in `decisions/`) |
| `scripts/` | `lint.sh`, `test.sh`, `all.sh`, `release.sh`, `check-core-deps.sh`, `check-release-tree.sh`, `check-agpl-deps.sh`, `test-release-features.sh` (every release feature, both helpers, and `cargo deny check advisories` on each helper graph, config `deny.toml`), `stage-os-artifact.sh`, `stage-helper.sh` (both helpers, notices, source offers, binary check), `helper-source.sh` (source tarball of the helpers), `install-protoc.sh` (pinned `protoc` for the CI jobs with no nix) — CI calls the same scripts |
| `third_party/` | License texts that release archives ship: TDLib, `tdlib-rs`, `twilight`, `whatsapp-rust` |
| `flake.nix` | Dev shell, with `protoc` for the Signal client. `.envrc` stays local (`source_up_if_exists` / `use flake` / `dotenv_if_exists .env`) |
| `.pre-commit-config.yaml` | prek hooks (fmt, clippy, taplo, typos, nixfmt, shellcheck, gitleaks, zizmor) |
| `.github/workflows/ci.yml` | Parallel lint/test/build plus the `check` guard. `release-features` runs `scripts/test-release-features.sh` (`0013`). `notify-os` runs clippy and tests of `thinwire-notify` on Windows and macOS, and on macOS stages a signed `Thinwire.app` with both helpers from a debug build, only when that crate, `scripts/stage-os-artifact.sh`, `scripts/stage-helper.sh`, `scripts/install-protoc.sh`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, or `ci.yml` changes. `helper-os` tests and stages both helpers on Windows and macOS when the helper code changes (`thinwire-ipc`, `thinwire-whatsapp`, `thinwire-signal`, and in `thinwire-protocol` the manifest, `adapter.rs`, `lib.rs`, `whatsapp.rs`, `signal.rs`, and `helper/`). `check` accepts the skip of these two OS jobs |
| `.github/workflows/os-zips.yml` | Main-only unsigned OS zips with `telegram-tdlib`, `discord-bot`, `whatsapp-web`, `signal-local`, publisher Telegram secrets, and the WhatsApp and Signal helpers (ADR 0003, 0007, 0013). Job `helper-source` uploads the source tarball of the helpers |
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
nix develop --command scripts/test-release-features.sh
```

There is no server container. Do not add a distroless GUI Docker image.
