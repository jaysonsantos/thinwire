# Frontend-independent core library

**Status:** accepted (skeleton landed; state move follows the Telegram usable merge)

## Context
The app state lives in the egui binary. `crates/thinwire/src/app/snapshot.rs` holds the inbox, the login flow, the drafts, and the command queue. `secrets.rs` holds the secret store. `settings.rs` holds the theme. `mod.rs` wires the adapter host. A TUI or a second GUI cannot use this code.

The product lock stays: protocol work runs off the UI thread, no secret goes on `AdapterCommand`, and no secret goes to a log. WhatsApp, Discord, and Slack work now runs in parallel with Telegram work. Those agents need one small, stable API to add their state and actions to.

Issue #33 asks for this split.

## Decision
Add the library crate `crates/thinwire-core`. The crate holds the state, the intents, the view model, the secret store, the settings, and the adapter host wiring. `crates/thinwire` becomes the egui frontend. It draws the view and sends intents.

The public API has four parts:

1. `Core`: the handle that a frontend owns. `Core::new(runtime, config)` starts the adapter host on the tokio runtime. `Core::dispatch(Intent)` applies one user action. `Core::pump()` applies the adapter events that arrived. `Core::view()` returns the view model.
2. `View<'_>`: a read-only borrow of the state. Accessors return lists, the selection, the login step, the error block, and the status line. The frontend does not clone the message list on each frame.
3. `Intent`: one enum for user actions. Protocol actions are in sub-enums: `TelegramIntent`, `WhatsAppIntent`, `DiscordIntent`, and `SlackIntent`. Typed secrets use `SecretText`. Its `Debug` output is redacted.
4. `ChangeSignal`: a `tokio::sync::watch` revision. The core bumps it when an adapter event arrives, when the keychain attach ends, and after each `dispatch`. A frontend waits with `changed().await` or checks `has_changed()`.

Rules for the boundary:

- The core does not depend on egui, eframe, or winit. `scripts/check-core-deps.sh` runs `cargo tree -p thinwire-core` in `scripts/test.sh` and fails when one of them shows up. A unit test also checks the manifest.
- `Core` lives on the frontend thread. `dispatch` and `pump` change memory and send on channels only. Keychain I/O and settings writes run on `spawn_blocking`, as they do today.
- Frontends never build `AdapterCommand` values. The core checks each intent against its state first.
- Text that a frontend edits on each key press goes through `dispatch` at once. Examples are the search text, the draft, and the login fields. The core keeps one value, so the egui field and the core never disagree.
- One-shot hints (focus the compose field, scroll to the selected row) are `View` flags that the frontend takes with `Core::take_*`. They do not become intents.
- Toolkit mapping stays in the frontend. Examples are `ThemeMode` to `egui::ThemePreference`, colors, and key bindings.
- Cargo features `telegram-tdlib`, `whatsapp-web`, `discord-bot`, and `slack-oauth` move to the core. The frontend forwards them.

## Consequences
A TUI or a headless test can drive thinwire with no egui code. `crates/thinwire-core/examples/headless.rs` proves this and builds in CI through `cargo test --workspace` and `cargo clippy --all-targets`.

The snapshot tests move to the core with the code. They keep their assertions. The egui crate keeps only drawing and toolkit tests.

Protocol agents add a variant to their own intent sub-enum and a field to the state. They do not change `Core`, `View`, or `ChangeSignal`.

The state move waits for the Telegram usable branch to merge. That branch changes `snapshot.rs`. This beat lands the crate, `Intent`, `SecretText`, `ChangeSignal`, `ThemeMode`, and the dependency guard.

Rejected:

- An actor core on a tokio task with owned view snapshots over a channel. Each key press in egui makes a round trip. An old snapshot can overwrite newer typed text. Each frame clones the full message list.
- A `ProtocolAdapter`-style trait per frontend. Two frontends do not need a plugin API.
- Frontends that send `AdapterCommand` directly. The login and send checks would be in each frontend.
