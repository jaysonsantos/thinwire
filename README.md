# thinwire

Thin, low-resource desktop messenger shell in **Rust + egui**.
One UI for many protocols. Targets **Windows, macOS, and Linux**.
The UI thread must never block; protocol I/O runs async off the main thread.

## Status

Inbox shell (account switcher + conversations on the left, thread in the center) with protocol adapters behind a tokio channel. The UI only polls events; workers must not call egui APIs.

v1 protocols are Telegram, WhatsApp (experimental), Discord (bot/OAuth inbox only), and Slack OAuth. Signal is out of v1; this MIT binary does not link libsignal or Presage.

First-run and Add account offer Telegram (TDLib) only. Official binaries inject `api_id` / `api_hash` at compile time, so end users sign in with phone → code → optional 2FA. Each login step puts the cursor in its field. Enter submits and Esc cancels. A refused step shows a specific error, for example a wrong code or an expired code. The code step says where Telegram sent the code (app, SMS, or call). If Telegram sends a word or a phrase by SMS, the code field accepts letters. On every screen, Enter runs the main button. "Send a new code" asks Telegram for a new code. "Change number" goes back to the phone step. The primary login UI never opens on paste-api fields. Dev builds without inject show credentials missing (rebuild with `TELEGRAM_API_ID`, or Advanced keychain override) and do not send every user to my.telegram.org. Slack, WhatsApp, and Discord protocol stubs remain. Their inbox work runs in parallel with Telegram (#34, #35, #36). Their auth UI appears only when its cargo feature is on. Default builds show only Telegram in the filter tabs and the account switcher. Slack, WhatsApp, or Discord chrome appears only when its cargo feature is on. Discord user-account / self-bot fields do not exist. thinwire is not a personal Discord client. Default CI builds keep `telegram-tdlib`, `discord-bot`, `slack-oauth`, and `whatsapp-web` off so they do not link or download TDLib, do not compile the Discord bot inbox, do not compile `slack-morphism`, and do not compile the WhatsApp linked-device client. The unauthorized Telegram banner drops only after TDLib Ready. With a saved session, the next launch shows "Connecting to Telegram…" and opens the inbox without the login screen. If Telegram ended the session, the app shows the phone step with a notice. If the saved data folder cannot open, the app moves it aside. The app keeps that folder. The phone step names that folder once.  After Ready, the inbox loads the main chat list, opens a chat for recent messages, and sends text on the worker thread. The chat list and the thread scroll. The thread opens at the newest message and follows new messages only when the view is at the bottom. The inbox shows "Loading chats…" and "Loading messages…" until the rows arrive, and it shows a message for an empty chat or a search with no match. In compose, Enter sends and Shift+Enter adds a line. Each chat keeps its own draft. A failed send shows "Not sent" and keeps the text. Retry asks TDLib to resend that message (`resendMessages`). The thread shows messages as bubbles by side, with the local time on each message and day breaks (Today, Yesterday, then the date). Group chats name the sender at the start of each run. Private chats and channels do not show sender names. The chat list shows the time of the last message. When you close the window, the app closes TDLib first and then exits. The window closes after 5 seconds at most. Cancel during login also closes TDLib. SIGTERM and SIGINT (Ctrl+C) close the window through the same step. A second signal exits at once with code 130. Enable `--features telegram-tdlib` locally after a TDLib install for the live `tdlib-rs` client. The Discord bot/OAuth guild inbox is a separate spike: `--features discord-bot` (twilight, ADR `0009-discord-bot-inbox-spike`). On `main`, that inbox stays hidden until the feature is compiled and Telegram messages are live. #35 removes the Telegram wait. Slack is a workspace-app OAuth v2 and Socket Mode scaffold (`slack-oauth`, ADR `0008-slack-oauth-workspace-spike`) with no Slack auth screen. Publisher `SLACK_CLIENT_ID`, `SLACK_CLIENT_SECRET`, and `SLACK_APP_TOKEN` stay out of git and public CI. Appearance defaults to the OS light/dark theme (System).

The WhatsApp linked-device spike is cargo feature `whatsapp-web` (pinned `whatsapp-rust` from oxidezap, same style of git revision ZapFast uses). It is experimental and not the default UI. Default builds and public CI leave the feature off, so there is no WhatsApp pairing screen and the WhatsApp account is not marked ready. With the feature on, a full-screen ban acknowledgement comes before any QR or phone-pair UI. The device store stays in the platform app-data directory, not in git. This spike is not a supported messenger.

## Protocol support (honest)

| Protocol | Path | Support language |
| --- | --- | --- |
| Telegram | Official TDLib via Rust bindings (`tdlib-rs`) | Supported goal |
| Slack | Official Slack OAuth v2 / Socket Mode (workspace app, `slack-morphism`, feature `slack-oauth`) | Supported goal |
| WhatsApp | Unofficial Web / linked-device (`whatsapp-rust`, feature `whatsapp-web`, off by default) | **Experimental** |
| Discord | Bot/OAuth guild inbox spike (`discord-bot`, twilight). Not a personal Discord client. No user-account, self-bot, or personal DM path | Constrained / experimental |

## Risk notice (required)

1. WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can get the user’s personal account banned or terminated. License-clean crates do not grant Meta or Discord permission.
2. Do not call WhatsApp or Discord “reliable.” Unofficial WhatsApp clients and Discord user-account / self-bot paths can break or violate ToS.
3. “Fast and reliable” applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). WhatsApp is experimental and Discord is bot/OAuth inbox only. The app must not market them as production messaging.

## License

MIT. Keep third-party notices (including Boost for TDLib if bundled).

## Build

`rust-toolchain.toml` pins Rust 1.98.1 for rustup. The workspace MSRV (`rust-version`) is 1.88. A Nix flake supplies the same tools as CI. The Nix shell uses the nixpkgs `rustc`, not the rustup pin. `.envrc` stays local.

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

Live Telegram run in the Nix shell (Linux):

```bash
nix develop --command cargo run -p thinwire --features telegram-tdlib
```

The Linux dev shell adds the items that a live run needs:

- `llvmPackages.libcxx`. The static TDLib links and loads `libc++` and `libc++abi`. You do not need the distro `libc++-dev` packages in this shell.
- nixpkgs `mesa` for OpenGL. The shell sets `LD_LIBRARY_PATH`, `__EGL_VENDOR_LIBRARY_DIRS`, and `LIBGL_DRIVERS_PATH` to that mesa. On non-NixOS Linux, the Nix `libglvnd` does not find the host GL drivers without these variables.

GPU notes:

- AMD and Intel: the nixpkgs mesa works on non-NixOS Linux.
- NVIDIA: mesa does not drive the NVIDIA GPU. Start the app through [nixGL](https://github.com/nix-community/nixGL). Inside the shell, run `nix run --impure github:nix-community/nixGL#nixGLNvidia -- cargo run -p thinwire --features telegram-tdlib`.
- The binary loads `libc++` from the Nix store through the shell `LD_LIBRARY_PATH`. Outside the shell, the binary can fail to start.

CI feature matrix: `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` run **without** `telegram-tdlib`, **without** `discord-bot`, **without** `slack-oauth`, and **without** `whatsapp-web`. That is the merge-critical default.

Live TDLib (local install; `tdlib-rs` downloads a prebuilt `tdjson`; never commit `api_id` / `api_hash` / session strings):

```bash
# Official / local inject — export in the private release environment, not in git.
TELEGRAM_API_ID= TELEGRAM_API_HASH= cargo build -p thinwire --features telegram-tdlib
```

Official main OS zips read `TELEGRAM_API_ID` and `TELEGRAM_API_HASH` from GitHub repository secrets. Local builds export the same names before cargo (empty placeholders above). Public CI never sets them. A keychain override in Advanced wins over the publisher pair when both exist. Linux live builds outside Nix need `libc++-dev` and `libc++abi-dev` because the feature statically links the prebuilt TDLib. The Nix dev shell supplies them (see Build).

TDLib data folder:

- `THINWIRE_TDLIB_DIR` wins when it is set.
- With a keychain that saves, the folder is `$XDG_DATA_HOME/thinwire/tdlib`, then `~/.local/share/thinwire/tdlib`. On Unix, the app sets it to mode `0700`.
- With a memory-only keychain, the app uses a throwaway folder. The folder is under `$XDG_RUNTIME_DIR` when that is private, else under the temp folder. The name has 8 random bytes and the mode is `0700`. The app refuses a folder or symlink that is already at that path. A clean shutdown removes the folder.
- If TDLib reports "Wrong database encryption key", the app moves the old folder aside to `tdlib.stale-<unix seconds>`. It never deletes that folder. The phone step names that folder once. A lock error ("already in use") does not move the folder.
- Never commit a TDLib folder.

Discord bot inbox (local only; twilight HTTP client; this spike does not open a gateway; never commit a bot token):

```bash
cargo build -p thinwire --features discord-bot
```

The bot token lives in the OS keychain account `discord.bot_token` (service `thinwire`). A `Bearer` value is refused until its application provenance and `bot` scope are verified. Public CI must not set a token. There is no Discord login form.

Slack workspace-app spike (types only; this command does not call Slack). Register redirect `http://127.0.0.1:8976/slack/oauth/callback` on the Slack app when a later beat binds the loopback listener. Public CI must not set the env vars:

```bash
SLACK_CLIENT_ID= SLACK_CLIENT_SECRET= SLACK_APP_TOKEN= cargo check -p thinwire --features slack-oauth
```

Local check of the experimental WhatsApp spike (not the default UI; public CI does not pass this feature):

```bash
cargo check -p thinwire --features whatsapp-web
```

Telegram `api_id`, `api_hash`, and session material go to the OS secret store (`keyring`):

| Platform | Store |
| --- | --- |
| macOS | Keychain |
| Windows | Credential Manager |
| Linux | D-Bus Secret Service first (KDE Wallet or GNOME Keyring, pure Rust `zbus`, no `libdbus` at build time). Then kernel keyutils. Then memory. The app probes each store with one read before it uses it. |
| Linux headless / CI | The same order. Keyutils keeps secrets only until the computer restarts. Memory keeps them only for this process. Nothing goes to a file. |

Set `THINWIRE_KEYRING=memory` to skip the OS keychain (CI and local headless). The UI thread only reads/writes an in-memory map; OS keychain attach and flush run on a tokio `spawn_blocking` worker. Never put secrets in the repo, `.env` committed files, logs, or CI artifacts. Tests use the memory backend and stay green without a desktop keychain or TDLib. The status strip tells you how long the sign-in lasts:

- Memory only: "Sign-in is not saved on this device: keychain unavailable." The next launch cannot resume the session.
- Keyutils: "Sign-in is kept until you restart the computer."
- Secret Service, macOS Keychain, and Windows Credential Manager: no notice.

While the keychain opens, the app shows "Opening the keychain…". After 1 second it shows "Waiting for the keychain. Unlock it to continue." A locked wallet can put its unlock prompt behind the window.

Theme preference is `System` (follow the OS, including live `ThemeChanged` updates), `Light`, or `Dark`. A missing `settings.toml` means System. The file lives under the platform config dir (`~/.config/thinwire/settings.toml` on Linux) and stores only the mode enum.

There is no distroless GUI container. This is a desktop egui app.

Pushes to `main` upload unsigned OS artifacts for Linux, macOS, and Windows (`telegram-tdlib` on, publisher credentials injected). GitHub wraps each Actions download in an outer zip. Inside that zip is one `.tar.gz` that preserves executable bits. Retention is 7 days. These artifacts are not a release. They are not signed. Extract the tar.gz. Linux and Windows archives contain the binary, the MIT `LICENSE`, and `THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt` (TDLib's Boost Software License) at the top level. The macOS archive contains an unsigned `Thinwire.app` (`Contents/MacOS/thinwire` is executable, plus `Contents/Info.plist`). `LICENSE` and `THIRD_PARTY_NOTICES` are in `Contents/Resources`. Extracting the macOS tar.gz yields an app that can be launched unsigned. It is not notarized. Linux archives also include the copyright files for the shipped `libc++`, `libc++abi`, and `libunwind`, plus the Apache-2.0 text those files cite. The workflow fails closed when `TELEGRAM_API_ID` or `TELEGRAM_API_HASH` is missing, or when the tar.gz cannot be built with an executable binary. Secret values live in GitHub repository secrets and in arcoiro under the thinwire path (SOPS + Terraform, not watchkeep), not in this tree. Public CI (`ci.yml`) and pull requests do not set them.

## Design rules

- MIT public repo under `jaysonsantos/thinwire`
- Do not infringe licenses of reference projects (e.g. ZapFast); preserve notices
- Prefer official APIs when they exist
- Never automate Discord as a normal user account. Not a personal Discord client. Bot/OAuth guild inbox only, feature `discord-bot` (ADR `0009-discord-bot-inbox-spike`). Default CI stays feature-off.
- Do not link AGPL libsignal / Presage into this binary
- Never commit or log Telegram `api_id` / `api_hash` / session strings, or a Discord bot token
- Default appearance is System theme (ADR 0005)
- Live Telegram TDLib is feature-gated (`0006-live-tdlib`). Default CI stays `telegram-tdlib` off.
- WhatsApp linked-device spike is feature `whatsapp-web` (pinned `whatsapp-rust`). Experimental and not the default UI. Default CI stays feature-off. Full-screen ToS/ban gate before QR or pair. Device store stays in app-data.
- Official Telegram `api_id` / `api_hash` are publisher inject (`0007-publisher-telegram-api-credentials`). Main OS zips read `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` from repository secrets; local builds export them before cargo; public CI never sets them. No embed in source. Optional Advanced keychain override is not the primary login path.
- Slack official OAuth is a feature-gated workspace-app spike (`slack-oauth`, `slack-morphism`, `0008-slack-oauth-workspace-spike`). Default CI stays off. No Slack auth UI in the default shell. Publisher `SLACK_CLIENT_ID` / `SLACK_CLIENT_SECRET` / `SLACK_APP_TOKEN` and the workspace bot token stay in the OS keychain or compile-time inject, never in git or public CI.
