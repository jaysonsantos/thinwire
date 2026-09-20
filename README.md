# thinwire

Thin, low-resource desktop messenger shell in **Rust + egui**.
One UI for many protocols. Targets **Windows, macOS, and Linux**.
The UI thread must never block; protocol I/O runs async off the main thread.

## Status

Scaffold only. Protocol adapters are not production-ready.

## Protocol support (honest)

| Protocol | Path | Support language |
| --- | --- | --- |
| Telegram | Official TDLib via Rust bindings (`tdlib-rs` planned) | Supported goal |
| Slack | Official Slack OAuth / API (workspace app) | Supported goal |
| WhatsApp | Unofficial Web / linked-device style (ZapFast / whatsapp-rust inspired) | **Experimental** |
| Signal | Unsupported third-party stacks (e.g. presage / libsignal) | **Experimental** |
| Discord | Official bot/OAuth only — no Discord user self-bots | Constrained / experimental |

## Risk notice (required)

1. WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can get the user’s personal account banned or terminated. License-clean crates do not grant Meta or Discord permission.
2. Signal has no supported third-party client API. Breakage and unsigned clients are expected. Do not call Signal, WhatsApp, or Discord “reliable.”
3. “Fast and reliable” applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). The other three are experimental modules with ToS risk. The app must not market them as production messaging.

## License

MIT. Keep third-party notices (including Boost for TDLib if bundled).

## Build

```bash
cargo run
```

## Design rules

- MIT public repo under `jaysonsantos/thinwire`
- Do not infringe licenses of reference projects (e.g. ZapFast); preserve notices
- Prefer official APIs when they exist
- Never automate Discord as a normal user account
