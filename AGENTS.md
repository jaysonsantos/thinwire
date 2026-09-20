# thinwire — agent notes

## Product lock (option B)

- All five protocols in scope for the shell: Telegram, WhatsApp, Signal, Discord, Slack
- README must keep the three risk bullets
- Never claim WhatsApp / Signal / Discord personal clients are “reliable”
- Discord: no self-bots / user-account automation
- UI: egui + eframe; protocol work off the UI thread

## Stack

- Rust 2021, egui/eframe, tokio for async adapters
- Telegram: TDLib / tdlib-rs preferred
- WhatsApp: unofficial linked-device path inspired by ZapFast (MIT) — ToS risk
- Signal: unsupported third-party path — breakage expected
- Slack: official OAuth only

## Commands

- `cargo build` / `cargo run` / `cargo test` / `cargo clippy -- -D warnings`
