# Frontend-independent core library

**Status:** accepted

## Context
The app state lives in the egui binary. `crates/thinwire/src/app/snapshot.rs` holds the inbox, the login flow, the drafts, and the command queue. `secrets.rs` holds the secret store. `settings.rs` holds the theme. `mod.rs` wires the adapter host. A TUI or a second GUI cannot use this code.

The product lock stays: protocol work runs off the UI thread, no secret goes on `AdapterCommand`, and no secret goes to a log. WhatsApp, Discord, and Slack work now runs in parallel with Telegram work. Those agents need one small, stable API to add their state and actions to.

Issue #33 asks for this split.

## Decision
Add the library crate `crates/thinwire-core`. The crate holds the state, the intents, the view model, the secret store, the settings, and the adapter host wiring. `crates/thinwire` becomes the egui frontend. It draws the view and sends intents.

The public API has four parts:

1. `Core`: the handle that a frontend owns. `Core::new(runtime, CoreConfig)` starts the adapter host and the keychain attach on the tokio runtime. `Core::dispatch(Intent)` applies one user action. `Core::pump()` applies the adapter events that arrived. `Core::view()` returns the view model. `Core::block_until_stopped(timeout)` closes the clients at exit.
2. `View<'_>`: a read-only borrow of the state. It derefs to the state type `Snapshot`, so a frontend reads fields and `&self` methods. It cannot call a mutating method. It adds the reads that need the secret store or the settings. The frontend does not clone the message list on each frame.
3. `Intent`: one enum for user actions. Protocol actions are in sub-enums: `TelegramIntent`, `WhatsAppIntent`, `DiscordIntent`, and `SlackIntent`. Typed secrets use `SecretText`. Its `Debug` output is redacted.
4. `ChangeSignal`: a `tokio::sync::watch` revision. The core bumps it when an adapter event arrives, while the keychain attach runs, and after each `dispatch`. A frontend waits with `changed().await` or checks `has_changed()`. The core gets the events through `AdapterHost::into_parts`. It returns a `HostSender`. `HostSender` keeps the Telegram login epoch rules (issue #42): `send` stamps and bumps the epoch, and `Core::pump` calls `deliver` just before it applies each event.

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

Protocol agents add a variant to their own intent sub-enum and a field to the state. They add one match arm in `Core::dispatch`. They do not change `View` or `ChangeSignal`.

Protocol agents also add their state fields to `Snapshot`. `View` shows them with no extra code.

### Adapter contract for the shell

The shell treats every protocol the same. Only the Telegram login and the first-run screen are Telegram-specific. An adapter follows these rules:

1. Send `AdapterEvent::Account { Linked }` before the first inbox event. Send `Account { Unlinked }` when the session ends (logout, revoke, ban). Only this event changes the link state. The shell drops inbox events of an unlinked protocol.
    - `Linking` after `Linked` is a reconnect. The session stays: rows, drafts, and open sends and retries. Inbox events and send answers still apply. New commands (open chat, Send, Retry, Refresh) wait for `Linked`. The selected chat loads again on `Linked` when its `OpenChat` was queued or had no answer yet when the reconnect started. A chat with loaded history does not load again.
    - `Linking` with no session before is a first login. The shell drops inbox events until `Linked`.
    - `Unlinked` from any other state ends the session. The shell drops that protocol's rows, messages, drafts, spinners, notes, and sends.
2. Use `Status` for the session status line only. A `Status { Error }` never unlinks and never hides the inbox.
3. Set `ProtocolCapabilities::sends_text` and `Conversation::writable`. The shell offers Send only for a linked protocol that sends text, in a writable chat.
4. Answer each `SendText` and `ResendMessage` with `SendAccepted` or `SendRejected` for its `request`. Nothing else from the adapter ends a send or a retry. Answer with `SendRejected` also after a reconnect that lost the request. As a safety net, the core ends a send or retry with no answer after 30 s (`SEND_TIMEOUT`, #69): the chat unlocks, and the text stays. A late `SendAccepted` for that request still counts, because the message went out. It clears the compose text or the draft only if it still holds exactly that text. It sets a retried row to sent and stops the tracking of a newer retry of that row. It removes only its own send from the timeout error. The core keeps an expired request for 5 min (`EXPIRED_KEEP`) and then drops it with its text. A late `SendRejected` changes nothing. The timeout error lists each expired send by chat. `pump` expires sends first and then applies the queued events, so the result does not depend on when the frontend pumps.
5. Report a failed command that leaves the session up as `CommandFailed`. It stops that command's spinner and shows the error.
6. Report information for the user as `Notice`. It is a note, not an error or a refusal.
7. Override `ProtocolAdapter::view_chat` to track the chat the user looks at (`ViewChat`), for example for unread counts. The default ignores it.
8. Put the generation of the begin command on every pairing payload: `WhatsAppBeginLink { generation }` for `WhatsAppQr` and `WhatsAppPairCode`, and the same for Signal (`SignalQr`, feature `signal-local`, #39) when it lands. The core assigns the generation. The shell shows only payloads of the current pairing and drops older ones.
9. End each load command with its answer or a failure. `LoadChats` ends with `ChatListLoaded`. `OpenChat` ends with `HistoryLoaded` for the chat. `LoadOlderMessages` ends with `OlderHistoryLoaded` for its `before_message_id`. A failure is `CommandFailed` for the chat (or for no chat), or an error from `handle` (the host shows it as a `Status` error). Without an answer the spinner of that load never stops. Load commands have no timeout.

#### Contract kit

`crates/thinwire-protocol/src/contract.rs` checks these rules in tests. A test links its adapter against its own offline fake and gives it to `Contract`. The kit sends commands through the host's `dispatch`, with the same routing and error handling as the app. `run_all` checks rules 1, 3, 4, 5, 7, and 9, and the `Shutdown` answer. `check_stream` checks rules 1, 3, 4, and 8 over every event. A failed check names its rule.

- The fake adapter, the Discord bot inbox, and the Slack workspace inbox run the whole kit in default CI.
- Telegram has no offline TDLib fake. Only its default-build stub runs the kit (start, shutdown, and the event stream).
- A new adapter adds one kit test next to its own tests.

Rejected:

- An actor core on a tokio task with owned view snapshots over a channel. Each key press in egui makes a round trip. An old snapshot can overwrite newer typed text. Each frame clones the full message list.
- A `ProtocolAdapter`-style trait per frontend. Two frontends do not need a plugin API.
- Frontends that send `AdapterCommand` directly. The login and send checks would be in each frontend.
