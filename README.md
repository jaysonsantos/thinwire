# thinwire

Thin, low-resource desktop messenger shell in **Rust + egui**.
One UI for many protocols. Targets **Windows, macOS, and Linux**.
The UI thread must never block; protocol I/O runs async off the main thread.

## Status

Inbox shell (account switcher + conversations on the left, thread in the center) with protocol adapters behind a tokio channel. The UI only polls events; workers must not call egui APIs.

v1 protocols are Telegram, WhatsApp (experimental), Discord (bot/OAuth inbox only), and Slack OAuth. Signal is out of v1; this MIT binary does not link libsignal or Presage.

First-run offers Telegram (TDLib) and Slack (workspace OAuth). WhatsApp and Discord sit behind an experimental gate that shows the Critic risk notice before any QR or token step. Discord user-account / self-bot fields do not exist. Default builds stay compile-safe stubs (`--features telegram-tdlib` compiles the TDLib hook, still without login or secrets).

## Protocol support (honest)

| Protocol | Path | Support language |
| --- | --- | --- |
| Telegram | Official TDLib via Rust bindings (`tdlib-rs` planned) | Supported goal |
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

Optional later (local TDLib install; never commit `api_id` / `api_hash` / session strings):

```bash
cargo build -p thinwire --features telegram-tdlib
```

There is no distroless GUI container. This is a desktop egui app.

Pushes to `main` upload unsigned OS zip artifacts for Linux, macOS, and Windows. Retention is 7 days. These zips are not a release. They are not signed.

## Design rules

- MIT public repo under `jaysonsantos/thinwire`
- Do not infringe licenses of reference projects (e.g. ZapFast); preserve notices
- Prefer official APIs when they exist
- Never automate Discord as a normal user account
- Do not link AGPL libsignal / Presage into this binary
