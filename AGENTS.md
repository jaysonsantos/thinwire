# thinwire — agent notes

## Product lock (option B)

- All five protocols in scope for the shell: Telegram, WhatsApp, Signal, Discord, Slack
- README must keep the three risk bullets
- Never claim WhatsApp / Signal / Discord personal clients are “reliable”
- Discord: no self-bots / user-account automation
- UI: egui + eframe; protocol work off the UI thread

## Stack

- Rust 2024 workspace, egui/eframe, tokio for async adapters
- Telegram: TDLib / tdlib-rs preferred
- WhatsApp: unofficial linked-device path inspired by ZapFast (MIT) — ToS risk
- Signal: unsupported third-party path — breakage expected
- Slack: official OAuth only

## Layout

| Path | Purpose |
| --- | --- |
| `crates/thinwire/` | Desktop binary: egui shell, first-run, auth stubs, inbox |
| `crates/thinwire-protocol/` | `ProtocolAdapter` trait, host channel, capability metadata, Critic risk strings |
| `scripts/` | `lint.sh`, `test.sh`, `all.sh`, `release.sh` — CI calls the same scripts |
| `flake.nix` | Dev shell. `.envrc` stays local (`source_up_if_exists` / `use flake` / `dotenv_if_exists .env`) |
| `.pre-commit-config.yaml` | prek hooks (fmt, clippy, taplo, typos, nixfmt, shellcheck, gitleaks, zizmor) |
| `.github/workflows/ci.yml` | Parallel lint/test/build plus the `check` guard |
| `.github/workflows/release-tag.yml` | Manual tag. No distroless GUI image |

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p thinwire
nix develop --command scripts/lint.sh
nix develop --command scripts/test.sh
```

There is no server container. Do not add a distroless GUI Docker image.
