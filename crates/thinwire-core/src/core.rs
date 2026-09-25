//! The `Core` handle: owns the state, the secret store, the settings, and
//! the adapter host. A frontend owns one `Core` on its own thread.
//!
//! `dispatch` and `pump` change memory and send on channels only. Keychain
//! I/O and settings writes run on `spawn_blocking`. Adapters run on the tokio
//! worker. So a frontend thread never waits on protocol or OS I/O.

use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "whatsapp-web")]
use thinwire_protocol::WhatsAppPhoneVault as PhoneVault;
use thinwire_protocol::{
    AdapterCommand, AdapterEvent, AdapterHost, DiscordAdapter, DiscordSecretVault, HostSender,
    ProtocolId, TelegramSecretVault, WhatsAppPhoneVault, catalog,
};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::intent::{
    AuthField, DiscordIntent, Intent, SlackIntent, TelegramIntent, WhatsAppIntent,
};
use crate::secrets::SecretStore;
use crate::settings::Settings;
use crate::signal::{ChangeNotifier, ChangeSignal, WeakNotifier, change_channel};
use crate::state::{AuthScreen, Snapshot};
use crate::view::View;

/// How often the keychain watch wakes frontends while attach runs. The
/// "unlock the keychain" text needs a redraw after one second.
const KEYCHAIN_WATCH_STEP: Duration = Duration::from_millis(250);
/// Poll step of [`Core::block_until_stopped`].
const STOP_POLL: Duration = Duration::from_millis(50);
/// Status line while the clients close.
const CLOSING_STATUS: &str = "Closing…";
/// First status line. The keychain backend does not change it.
const SECRET_STORE_STATUS_PREFIX: &str = "Sign in with Telegram to get started.";

/// Start-up input for [`Core::new`].
#[derive(Debug, Clone)]
pub struct CoreConfig {
    settings: Settings,
    memory_secrets: bool,
}

impl CoreConfig {
    /// Settings from the platform config dir. Missing file means System.
    #[must_use]
    pub fn load() -> Self {
        Self::new(Settings::load())
    }

    /// Use settings the caller loaded, for example from a test path.
    #[must_use]
    pub const fn new(settings: Settings) -> Self {
        Self {
            settings,
            memory_secrets: false,
        }
    }

    /// Keep every secret in memory, as `THINWIRE_KEYRING=memory` does.
    /// For headless runs and tests. Nothing reaches the OS keychain.
    #[must_use]
    pub const fn with_memory_secrets(mut self) -> Self {
        self.memory_secrets = true;
        self
    }
}

/// Frontend-independent app core (ADR 0010).
///
/// # Threads
///
/// Call every method from the frontend thread (for example the egui UI
/// thread, or the `main` loop of a headless frontend). Do not call a method
/// from a task on the adapter runtime (the `runtime` given to
/// [`Core::new`]), and not inside `Runtime::block_on`. The methods are
/// synchronous and never yield: [`Core::block_until_stopped`] sleeps until
/// the adapters stop. On the adapter runtime, such a call holds a worker
/// thread that the adapters need, and it can wait for itself (#55).
///
/// To wait for a change, `block_on` only [`ChangeSignal::changed`], then call
/// [`Core::pump`] after `block_on` returns. The headless example does this.
pub struct Core {
    runtime: Handle,
    /// Command side of the host. It also delivers stamped login events.
    commands: HostSender,
    events: UnboundedReceiver<AdapterEvent>,
    state: Snapshot,
    settings: Settings,
    secrets: Arc<SecretStore>,
    whatsapp_phone: Arc<WhatsAppPhoneVault>,
    notifier: ChangeNotifier,
    closing: bool,
}

impl Core {
    /// Start the adapter host and the keychain attach on `runtime`.
    ///
    /// `THINWIRE_KEYRING=memory` keeps every secret in memory.
    #[must_use]
    pub fn new(runtime: &Handle, config: CoreConfig) -> Self {
        let secrets = if config.memory_secrets {
            Arc::new(SecretStore::memory())
        } else {
            SecretStore::for_ui(runtime)
        };
        Self::with_store(runtime, config, secrets)
    }

    fn with_store(runtime: &Handle, config: CoreConfig, secrets: Arc<SecretStore>) -> Self {
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let host = AdapterHost::spawn(
            runtime,
            Arc::clone(&secrets) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&secrets) as Arc<dyn DiscordSecretVault>,
            Arc::clone(&whatsapp_phone),
        );
        // `for_ui` only schedules keychain attach. Discord's start can run
        // before that blocking read finishes, so arm it again once a token
        // is in memory. The hook sends a command; it does not touch the OS store.
        bind_discord_after_hydrate(&secrets, &host);
        let (notifier, _) = change_channel();
        let (commands, host_events) = host.into_parts();
        let events = forward_events(runtime, host_events, notifier.downgrade());
        watch_keychain(runtime, Arc::clone(&secrets), &notifier);
        let mut state = Snapshot::new();
        state.status_text = secret_store_status_text(secrets.backend_name());
        Self {
            runtime: runtime.clone(),
            commands,
            events,
            state,
            settings: config.settings,
            secrets,
            whatsapp_phone,
            notifier,
            closing: false,
        }
    }

    /// A new change signal. It fires after adapter events, keychain progress,
    /// and each [`Self::dispatch`].
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    #[must_use]
    pub fn signal(&self) -> ChangeSignal {
        self.notifier.subscribe()
    }

    /// The state for this redraw.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    #[must_use]
    pub fn view(&self) -> View<'_> {
        View {
            state: &self.state,
            secrets: &self.secrets,
            settings: &self.settings,
        }
    }

    /// Apply the adapter events that arrived and run time-based checks.
    /// Returns true when at least one event was applied.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    pub fn pump(&mut self) -> bool {
        let mut applied = false;
        while let Ok(event) = self.events.try_recv() {
            // The login epoch check runs here, on the thread that sends
            // Cancel, not in the forward task (issue #42, PR #49).
            if let Some(event) = self.commands.deliver(event) {
                self.state.apply(event);
                applied = true;
            }
        }
        if let Some(text) =
            refreshed_secret_store_status(&self.state.status_text, self.secrets.backend_name())
        {
            self.state.status_text = text;
        }
        self.state.poll_resume(&self.secrets);
        self.state.expire_sends();
        self.state.sync_viewed();
        self.flush();
        applied
    }

    /// Apply one user action, then queue its commands for the worker.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    pub fn dispatch(&mut self, intent: Intent) {
        match intent {
            Intent::SelectProtocol(protocol) => self.state.select_protocol(protocol),
            Intent::SelectConversation { id } => self.state.select_conversation(id),
            Intent::MoveInbox { delta } => self.state.move_inbox_selection(delta),
            Intent::FocusInbox { id } => self.state.focus_inbox_row(id),
            Intent::SetFilter(filter) => self.state.set_filter(filter),
            Intent::SetSearch(text) => self.state.set_search(text.into_inner()),
            Intent::Refresh => self.state.refresh_visible(),
            Intent::DismissError => self.state.error = None,
            Intent::Key(key) => self.state.center_key(key, &self.secrets),
            Intent::SetDraft {
                protocol,
                conversation_id,
                text,
            } => self
                .state
                .set_draft(protocol, &conversation_id, text.into_inner()),
            Intent::SendDraft {
                protocol,
                conversation_id,
            } => {
                if self.state.is_selected_chat(protocol, &conversation_id) {
                    self.state.send_compose();
                } else {
                    tracing::warn!("send dropped: its chat is no longer selected");
                }
            }
            Intent::Retry { message_id } => self.state.retry_send(&message_id),
            Intent::LoadOlderMessages {
                protocol,
                conversation_id,
            } => {
                if self.state.is_selected_chat(protocol, &conversation_id) {
                    self.state.load_older();
                }
            }
            Intent::RetryKeychain => self.state.retry_keychain(),
            Intent::SetTheme(theme) => self.settings.set_theme(theme),
            Intent::Shutdown => self.shutdown(),
            Intent::Telegram(intent) => self.telegram(intent),
            Intent::WhatsApp(intent) => self.whatsapp(intent),
            Intent::Discord(DiscordIntent::Connect) => {
                self.state.queue(AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                });
            }
            Intent::Slack(SlackIntent::Connect) => {
                self.state.queue(AdapterCommand::Connect {
                    protocol: ProtocolId::Slack,
                });
            }
        }
        self.state.sync_viewed();
        self.flush();
        self.notifier.notify();
    }

    /// Chat to focus the compose field on, once.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    pub fn take_focus_compose(&mut self) -> bool {
        self.state.take_focus_compose()
    }

    /// The selected row moved; scroll it into view, once.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    pub fn take_scroll_to_selected(&mut self) -> bool {
        self.state.take_scroll_to_selected()
    }

    /// The keyboard highlight moved; scroll that row into view, once.
    pub fn take_scroll_to_focused(&mut self) -> bool {
        self.state.take_scroll_to_focused()
    }

    /// Start a keychain flush of the persistent keys on the runtime. The
    /// caller does not wait. The app calls it last at exit.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    pub fn flush_keychain(&self) {
        self.secrets.spawn_os_flush(&self.runtime);
    }

    /// Every adapter answered [`Intent::Shutdown`] with `Stopped`.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.state.all_stopped()
    }

    /// Shut down and wait up to `timeout` for the clients to close.
    ///
    /// Blocks the caller. Use it only after the window is gone. Never call
    /// it on the adapter runtime: it waits for tasks that run there. A
    /// `spawn_blocking` thread is fine.
    ///
    /// Frontend thread only. See [`Core`] "Threads".
    pub fn block_until_stopped(&mut self, timeout: Duration) -> bool {
        self.shutdown();
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if self.stopped() {
                return true;
            }
            if Instant::now() >= deadline {
                tracing::warn!("adapters did not close before exit");
                return false;
            }
            std::thread::sleep(STOP_POLL);
        }
    }

    fn shutdown(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        // Every adapter closes its sessions (TDLib, the WhatsApp session,
        // Discord, Slack) and answers `Stopped`. `stopped` waits for all.
        for caps in catalog() {
            self.send(AdapterCommand::Shutdown { protocol: caps.id });
        }
        self.state.status_text = CLOSING_STATUS.into();
    }

    fn telegram(&mut self, intent: TelegramIntent) {
        let store = &self.secrets;
        match intent {
            TelegramIntent::AddAccount => self.state.open_add_account(store),
            TelegramIntent::OpenApiOverride => self.state.open_api_override(store),
            TelegramIntent::SetField(field, value) => {
                // Intents apply after the frame. A key typed in the same frame
                // as Escape must not refill a cleared form (qa L2).
                if self.state.auth == AuthScreen::Idle {
                    return;
                }
                let value = value.expose().to_owned();
                match field {
                    AuthField::ApiId => self.state.telegram_api_id = value,
                    AuthField::ApiHash => self.state.telegram_api_hash = value,
                    AuthField::Phone => self.state.telegram_phone = value,
                    AuthField::Code => self.state.telegram_code = value,
                    AuthField::Password => self.state.telegram_2fa = value,
                }
            }
            TelegramIntent::Submit => self.state.advance_telegram(store),
            TelegramIntent::Cancel => self.state.cancel_auth(store),
            TelegramIntent::ChangeNumber => self.state.change_number(),
            TelegramIntent::ResendCode => self.state.resend_code(),
        }
    }

    #[cfg(feature = "whatsapp-web")]
    fn whatsapp(&mut self, intent: WhatsAppIntent) {
        let phone: &PhoneVault = &self.whatsapp_phone;
        match intent {
            WhatsAppIntent::OpenRiskGate => self.state.open_whatsapp_risk_gate(),
            WhatsAppIntent::CloseGate => self.state.close_whatsapp_gate(phone),
            WhatsAppIntent::AcknowledgeRisk => self.state.acknowledge_whatsapp_risk(),
            WhatsAppIntent::SetPhone(value) => {
                self.state.whatsapp_phone = value.expose().to_owned()
            }
            WhatsAppIntent::BeginLink => self.state.begin_whatsapp_link(phone),
            WhatsAppIntent::CancelLink => self.state.cancel_whatsapp_link(phone),
        }
    }

    /// Feature off: the WhatsApp screens do not exist, so nothing happens.
    #[cfg(not(feature = "whatsapp-web"))]
    fn whatsapp(&mut self, intent: WhatsAppIntent) {
        let _ = (intent, &self.whatsapp_phone);
    }

    fn flush(&mut self) {
        let commands = self.state.take_commands();
        // While the clients close, a click must not reach a protocol worker.
        if !self.closing {
            for command in commands {
                self.send(command);
            }
        }
        if self.state.take_keychain_flush() {
            self.secrets.spawn_os_flush(&self.runtime);
        }
        if self.state.take_keychain_retry() {
            // Read again off the caller thread, with a fresh watch: the old
            // one stopped at the failed read.
            self.secrets.spawn_os_retry(&self.runtime);
            watch_keychain(&self.runtime, Arc::clone(&self.secrets), &self.notifier);
        }
        if let Some(job) = self.settings.take_persist_job() {
            self.runtime.spawn_blocking(move || job.run());
        }
    }

    fn send(&self, command: AdapterCommand) {
        if !self.commands.send(command) {
            tracing::warn!("adapter host command channel closed");
        }
    }
}

/// Move adapter events to the core queue and wake the frontends.
fn forward_events(
    runtime: &Handle,
    mut from_host: UnboundedReceiver<AdapterEvent>,
    notifier: WeakNotifier,
) -> UnboundedReceiver<AdapterEvent> {
    let (to_core, events) = unbounded_channel();
    runtime.spawn(async move {
        while let Some(event) = from_host.recv().await {
            // The core is gone when its queue or its signal is gone.
            if to_core.send(event).is_err() || !notifier.notify() {
                return;
            }
        }
    });
    events
}

/// Wake the frontends while the keychain attach runs, and once when it ends.
///
/// The watch stops when the attach settles, when the read fails (Try again
/// starts a new watch), or when the core is gone: it holds only a weak
/// notifier (PR #48 review).
fn watch_keychain(runtime: &Handle, secrets: Arc<SecretStore>, notifier: &ChangeNotifier) {
    runtime.spawn(watch_until(
        move || secrets.attach_settled() || secrets.read_failed(),
        notifier.downgrade(),
        KEYCHAIN_WATCH_STEP,
    ));
}

/// Notify every `step` until `done` is true, then once more.
///
/// Check first, then notify: the last wake comes after the watch saw the
/// end. So the frame for it sees the end too (qa M2). Stops at once when
/// the core is gone.
async fn watch_until(done: impl Fn() -> bool, notifier: WeakNotifier, step: Duration) {
    loop {
        let finished = done();
        if !notifier.notify() || finished {
            return;
        }
        tokio::time::sleep(step).await;
    }
}

fn bind_discord_after_hydrate(secrets: &SecretStore, host: &AdapterHost) {
    if !DiscordAdapter::bot_inbox_compiled() {
        return;
    }
    let commands = host.command_sender();
    secrets.on_discord_token_hydrated(move || {
        let _ = commands.send(AdapterCommand::Connect {
            protocol: ProtocolId::Discord,
        });
    });
}

fn secret_store_status_text(backend: &str) -> String {
    let _ = backend;
    SECRET_STORE_STATUS_PREFIX.to_string()
}

fn refreshed_secret_store_status(current: &str, backend: &str) -> Option<String> {
    if current != SECRET_STORE_STATUS_PREFIX {
        return None;
    }
    let next = secret_store_status_text(backend);
    (current != next).then_some(next)
}

#[cfg(test)]
mod tests {
    use thinwire_protocol::AdapterStatus;

    use super::*;
    use crate::ThemeMode;

    const WAIT: Duration = Duration::from_secs(3);

    fn temp_settings() -> Settings {
        let path = std::env::temp_dir()
            .join("thinwire-core-tests")
            .join(format!(
                "{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ))
            .join("settings.toml");
        Settings::load_from(path)
    }

    fn memory_core() -> Core {
        Core::new(
            &Handle::current(),
            CoreConfig::new(temp_settings()).with_memory_secrets(),
        )
    }

    /// Wait for the signal, then pump, until `done` is true.
    async fn pump_until(core: &mut Core, mut done: impl FnMut(&Core) -> bool) {
        let mut signal = core.signal();
        let result = tokio::time::timeout(WAIT, async {
            loop {
                core.pump();
                if done(core) {
                    return;
                }
                signal.changed().await;
            }
        })
        .await;
        assert!(result.is_ok(), "core did not reach the state in time");
    }

    #[test]
    fn the_headless_example_calls_core_off_the_runtime() {
        let example = include_str!("../examples/headless.rs");
        let wait = &example[example.find("runtime.block_on(").expect("block_on")..];
        let wait = &wait[..wait.find("});").expect("end of block_on")];
        assert!(
            !wait.contains("core."),
            "no Core call inside block_on (#55)"
        );
        assert!(wait.contains("signal.changed()"));
        let core = include_str!("core.rs");
        let doc = &core[core.find("/// # Threads").expect("threads doc")..];
        let doc = &doc[..doc.find("pub struct Core").expect("struct")];
        assert!(doc.contains("not inside `Runtime::block_on`"));
        let methods = core[core.find("impl Core {").expect("impl")..]
            .matches("Frontend thread only. See [`Core`] \"Threads\".")
            .count();
        assert!(methods >= 9, "each sync method states the rule: {methods}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adapter_events_wake_the_signal_and_reach_the_view() {
        let mut core = memory_core();
        pump_until(&mut core, |core| {
            core.view()
                .accounts
                .iter()
                .any(|row| row.caps.id == ProtocolId::Telegram && row.detail != row.caps.detail)
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_changes_the_view_and_fires_the_signal() {
        let mut core = memory_core();
        let mut signal = core.signal();
        signal.mark_seen();
        core.dispatch(Intent::SetSearch("alice".into()));
        assert!(signal.has_changed());
        assert_eq!(core.view().search, "alice");
        core.state.selected_conversation = Some("telegram:1".into());
        core.dispatch(Intent::SetDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            text: "hello".into(),
        });
        assert_eq!(core.view().compose, "hello");
        core.dispatch(Intent::SetTheme(ThemeMode::Dark));
        assert_eq!(core.view().theme(), ThemeMode::Dark);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn typed_login_fields_stay_in_the_state_and_off_debug() {
        let mut core = memory_core();
        core.state.auth = AuthScreen::TelegramPhone;
        core.dispatch(Intent::Telegram(TelegramIntent::SetField(
            AuthField::Phone,
            "+15550100".into(),
        )));
        assert_eq!(core.view().telegram_phone, "+15550100");
        let shown = format!(
            "{:?}",
            Intent::Telegram(TelegramIntent::SetField(
                AuthField::Phone,
                "+15550100".into()
            ))
        );
        assert!(!shown.contains("5550100"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_reports_stopped_and_drops_later_commands() {
        let mut core = memory_core();
        core.dispatch(Intent::Shutdown);
        assert_eq!(core.view().status_text, CLOSING_STATUS);
        core.dispatch(Intent::Refresh);
        pump_until(&mut core, Core::stopped).await;
        assert!(core.closing);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn block_until_stopped_returns_after_the_clients_close() {
        let mut core = memory_core();
        let stopped = tokio::task::spawn_blocking(move || core.block_until_stopped(WAIT))
            .await
            .expect("join");
        assert!(stopped);
    }

    #[tokio::test]
    async fn discord_arms_after_keychain_hydrate() {
        if !DiscordAdapter::bot_inbox_compiled() {
            return;
        }
        let store = SecretStore::detached_for_test();
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let mut host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&store) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&store) as Arc<dyn DiscordSecretVault>,
            Arc::clone(&whatsapp_phone),
        );
        bind_discord_after_hydrate(&store, &host);

        let deadline = Instant::now() + WAIT;
        let mut saw_missing = false;
        while !saw_missing {
            for event in host.poll_events() {
                if let AdapterEvent::Status { detail, .. } = &event {
                    assert!(!detail.contains("fixture-bot-token"));
                    if detail.contains("bot token is not in the OS keychain") {
                        saw_missing = true;
                    }
                }
            }
            if saw_missing {
                break;
            }
            if Instant::now() > deadline {
                panic!("discord started without reporting a missing token");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        store.complete_discord_hydrate_for_test(Some("fixture-bot-token"));

        let deadline = Instant::now() + WAIT;
        loop {
            for event in host.poll_events() {
                if let AdapterEvent::Status { status, detail, .. } = &event {
                    assert!(!detail.contains("fixture-bot-token"));
                    if *status == AdapterStatus::Stubbed
                        && detail.contains("bot token is in the OS keychain")
                        && !detail.contains("not in the OS keychain")
                    {
                        return;
                    }
                }
            }
            if Instant::now() > deadline {
                panic!("discord did not arm after keychain hydrate");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn shutdown_reaches_every_adapter_and_each_reports_stopped() {
        let store = Arc::new(SecretStore::memory());
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let mut host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&store) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&store) as Arc<dyn DiscordSecretVault>,
            whatsapp_phone,
        );
        for caps in catalog() {
            host.send(AdapterCommand::Shutdown { protocol: caps.id });
        }
        let mut snapshot = Snapshot::new();
        let deadline = Instant::now() + WAIT;
        while !snapshot.all_stopped() {
            for event in host.poll_events() {
                snapshot.apply(event);
            }
            assert!(
                Instant::now() < deadline,
                "every adapter must answer Shutdown with Stopped"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let src = include_str!("core.rs");
        let shutdown = &src[src.find("fn shutdown(&mut self)").expect("shutdown")..];
        let shutdown = &shutdown[..shutdown.find("\n    }\n").expect("end")];
        assert!(shutdown.contains("for caps in catalog()"));
        let stopped = &src[src.find("pub fn stopped(").expect("stopped")..];
        let stopped = &stopped[..stopped.find("\n    }\n").expect("end")];
        assert!(stopped.contains("self.state.all_stopped()"));
    }

    #[test]
    fn secret_store_status_stays_calm_across_backend_attach() {
        let current = secret_store_status_text("memory");
        assert_eq!(current, "Sign in with Telegram to get started.");
        assert_eq!(refreshed_secret_store_status(&current, "os-keychain"), None);
    }

    #[test]
    fn secret_store_status_does_not_clobber_auth_copy() {
        assert_eq!(
            refreshed_secret_store_status("Telegram: enter a phone number.", "os-keychain"),
            None
        );
    }

    /// qa M2: the attach ends right after a wake. One more wake must come
    /// after the watch saw the end, or the resume frame never runs.
    #[tokio::test]
    async fn keychain_watch_wakes_again_after_the_attach_ends() {
        let (notifier, mut signal) = change_channel();
        let probe = notifier.subscribe();
        let seen_at = Arc::new(std::sync::Mutex::new(None));
        let record = Arc::clone(&seen_at);
        let checks = std::sync::atomic::AtomicU32::new(0);
        let settled = move || {
            // Not settled at the first check; the attach ends before the second.
            let done = checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1;
            if done {
                let mut probe = probe.clone();
                *record.lock().expect("lock") = Some(probe.mark_seen());
            }
            done
        };
        tokio::time::timeout(
            WAIT,
            watch_until(settled, notifier.downgrade(), Duration::from_millis(1)),
        )
        .await
        .expect("watch ends");
        let seen = seen_at.lock().expect("lock").expect("saw the end");
        let last = signal.mark_seen();
        assert!(
            last > seen,
            "no wake after the end: seen {seen}, last {last}"
        );
    }

    #[test]
    fn ready_attach_sets_the_backend_under_the_same_lock() {
        let store = SecretStore::detached_for_test();
        let _ = store.finish_ready(
            std::collections::HashMap::new(),
            Some(crate::secrets::OsBackend::KernelKeyring),
        );
        assert!(store.attach_settled());
        assert_eq!(
            store.persistence(),
            crate::secrets::Persistence::UntilRestart
        );
    }

    /// qa L2: Escape and a typed key in one frame. `Cancel` applies first, so
    /// the late `SetField` must not refill the cleared field.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_field_edit_after_cancel_is_dropped() {
        let mut core = memory_core();
        core.state.auth = AuthScreen::TelegramPhone;
        core.dispatch(Intent::Telegram(TelegramIntent::SetField(
            AuthField::Phone,
            "+1555".into(),
        )));
        assert_eq!(core.view().telegram_phone, "+1555");
        core.dispatch(Intent::Telegram(TelegramIntent::Cancel));
        core.dispatch(Intent::Telegram(TelegramIntent::SetField(
            AuthField::Phone,
            "+15550".into(),
        )));
        assert_eq!(core.view().auth, AuthScreen::Idle);
        assert!(core.view().telegram_phone.is_empty());
    }

    /// qa L4: after `Shutdown`, a later intent sends nothing to the host.
    #[tokio::test(flavor = "multi_thread")]
    async fn closing_drops_commands_from_later_intents() {
        let mut core = memory_core();
        let (probe, mut sent) = unbounded_channel();
        core.commands = HostSender::for_test(probe);

        core.dispatch(Intent::Refresh);
        assert!(
            sent.try_recv().is_ok(),
            "before close, Refresh reaches the host"
        );
        while sent.try_recv().is_ok() {}

        core.dispatch(Intent::Shutdown);
        for caps in catalog() {
            assert_eq!(
                sent.try_recv().ok(),
                Some(AdapterCommand::Shutdown { protocol: caps.id }),
                "one Shutdown per adapter"
            );
        }
        core.dispatch(Intent::Refresh);
        core.dispatch(Intent::Discord(DiscordIntent::Connect));
        core.dispatch(Intent::Shutdown);
        core.pump();
        assert!(
            sent.try_recv().is_err(),
            "no command reaches the host while closing"
        );
    }

    /// PR #48 review (P1): a headless frontend that skips the WhatsApp ban
    /// gate cannot start pairing. The normal order still works.
    #[tokio::test(flavor = "multi_thread")]
    async fn whatsapp_intents_without_the_gate_reach_no_adapter() {
        let mut core = memory_core();
        let (probe, mut sent) = unbounded_channel();
        core.commands = HostSender::for_test(probe);

        core.dispatch(Intent::WhatsApp(WhatsAppIntent::AcknowledgeRisk));
        core.dispatch(Intent::WhatsApp(WhatsAppIntent::SetPhone(
            "15550100".into(),
        )));
        core.dispatch(Intent::WhatsApp(WhatsAppIntent::BeginLink));
        assert!(
            sent.try_recv().is_err(),
            "no pairing command without the risk gate"
        );

        core.dispatch(Intent::WhatsApp(WhatsAppIntent::OpenRiskGate));
        core.dispatch(Intent::WhatsApp(WhatsAppIntent::AcknowledgeRisk));
        core.dispatch(Intent::WhatsApp(WhatsAppIntent::BeginLink));
        let mut got = Vec::new();
        while let Ok(command) = sent.try_recv() {
            got.push(command);
        }
        if cfg!(feature = "whatsapp-web") {
            assert!(matches!(
                got.as_slice(),
                [
                    AdapterCommand::WhatsAppAcknowledgeRisk,
                    AdapterCommand::WhatsAppBeginLink { .. }
                ]
            ));
        } else {
            assert!(got.is_empty(), "feature off: WhatsApp intents do nothing");
        }
    }

    /// PR #48 review: the keychain watch must not outlive the core. After a
    /// drop, the change signal ends (no task keeps a strong notifier).
    #[tokio::test(flavor = "multi_thread")]
    async fn keychain_watch_stops_when_the_core_is_dropped() {
        // A detached store never settles and never fails: the watch loops.
        let store = SecretStore::detached_for_test();
        let core = Core::with_store(
            &Handle::current(),
            CoreConfig::new(temp_settings()),
            Arc::clone(&store),
        );
        let mut signal = core.signal();
        drop(core);
        let ended = tokio::time::timeout(WAIT, async { while signal.changed().await {} }).await;
        assert!(ended.is_ok(), "a detached task still holds the notifier");
        assert!(!store.attach_settled(), "the watch had no end but the drop");
    }

    /// PR #48 review: a failed read ends the watch after one last wake.
    #[tokio::test]
    async fn keychain_watch_stops_at_a_failed_read() {
        let store = SecretStore::detached_for_test();
        store.fail_attach_for_test();
        let (notifier, mut signal) = change_channel();
        let watched = Arc::clone(&store);
        tokio::time::timeout(
            WAIT,
            watch_until(
                move || watched.attach_settled() || watched.read_failed(),
                notifier.downgrade(),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("the watch ends at ReadFailed");
        assert!(signal.has_changed(), "one last wake shows Try again");
        assert_eq!(signal.mark_seen(), 1, "no wake loop after the failure");
    }

    /// Try again leaves `ReadFailed` at once and starts a fresh watch.
    #[test]
    fn keychain_retry_starts_a_fresh_watch() {
        let src = include_str!("core.rs");
        let retry = &src[src.find("take_keychain_retry()").expect("retry")..];
        let retry = &retry[..retry.find("\n        }").expect("end")];
        let attach = retry.find("spawn_os_retry(").expect("retry attach");
        let watch = retry.find("watch_keychain(").expect("fresh watch");
        assert!(
            attach < watch,
            "the phase leaves ReadFailed before the watch"
        );
    }

    /// PR #49 through the core path: a login event that an old client queued
    /// before Cancel is dropped when the core applies it. A login event of
    /// the current epoch still applies.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_drops_an_old_epoch_login_event_in_the_core() {
        use thinwire_protocol::TelegramAuthPhase;

        let mut core = memory_core();
        let (queue, events) = unbounded_channel();
        core.events = events;
        let stamped = |epoch, phase| AdapterEvent::Login {
            epoch,
            event: Box::new(AdapterEvent::TelegramAuth { phase }),
        };

        // Queued by the old client before the user pressed Cancel.
        queue
            .send(stamped(0, TelegramAuthPhase::Ready))
            .expect("queue");
        core.dispatch(Intent::Telegram(TelegramIntent::Cancel));
        core.pump();
        assert!(
            !core.view().telegram_authorized,
            "an old-epoch Ready must not sign in after Cancel"
        );

        // The next client runs under the bumped epoch: its events apply.
        queue
            .send(stamped(1, TelegramAuthPhase::Ready))
            .expect("queue");
        core.pump();
        assert!(core.view().telegram_authorized);
    }

    /// PR #48 review (P1): one frame with a click on chat B and an edit of
    /// chat A. The frontend sends `SelectConversation(B)` first, then the
    /// edit. The edit must stay with A.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_same_frame_edit_and_select_keep_each_draft_in_its_chat() {
        use crate::state::test_support::ready_with_chats;

        let mut core = memory_core();
        core.state = ready_with_chats(&core.secrets);
        core.dispatch(Intent::SelectConversation {
            id: "telegram:1".into(),
        });
        // The same frame: click on chat 2, then the key typed in chat 1.
        core.dispatch(Intent::SelectConversation {
            id: "telegram:2".into(),
        });
        core.dispatch(Intent::SetDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            text: "for Ada only".into(),
        });
        assert_eq!(core.view().compose, "", "chat 2 gets no text from chat 1");

        core.dispatch(Intent::SelectConversation {
            id: "telegram:1".into(),
        });
        assert_eq!(
            core.view().compose,
            "for Ada only",
            "chat 1 keeps its draft"
        );
    }

    /// PR #48 review (P1): a send named for chat A after a click on chat B
    /// sends nothing to B.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_send_for_another_chat_is_dropped() {
        use crate::state::test_support::ready_with_chats;

        let mut core = memory_core();
        core.state = ready_with_chats(&core.secrets);
        let (probe, mut sent) = unbounded_channel();
        core.commands = HostSender::for_test(probe);
        core.dispatch(Intent::SelectConversation {
            id: "telegram:1".into(),
        });
        core.dispatch(Intent::SetDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            text: "for Ada only".into(),
        });
        core.dispatch(Intent::SelectConversation {
            id: "telegram:2".into(),
        });
        while sent.try_recv().is_ok() {}

        core.dispatch(Intent::SendDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
        });
        let mut commands = Vec::new();
        while let Ok(command) = sent.try_recv() {
            commands.push(command);
        }
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, AdapterCommand::SendText { .. })),
            "no send after the selection moved: {commands:?}"
        );

        // Back on chat 1, the same send goes to chat 1.
        core.dispatch(Intent::SelectConversation {
            id: "telegram:1".into(),
        });
        core.dispatch(Intent::SendDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
        });
        let mut sends = Vec::new();
        while let Ok(command) = sent.try_recv() {
            if let AdapterCommand::SendText {
                conversation_id, ..
            } = command
            {
                sends.push(conversation_id);
            }
        }
        assert_eq!(sends, vec!["telegram:1".to_owned()]);
    }

    /// Plan item 9 through the core: a click sends `ViewChat`, and nothing
    /// goes out after `Shutdown`.
    #[tokio::test(flavor = "multi_thread")]
    async fn view_chat_goes_out_on_selection_and_not_after_shutdown() {
        use crate::state::test_support::ready_with_chats;

        let mut core = memory_core();
        core.state = ready_with_chats(&core.secrets);
        let (probe, mut sent) = unbounded_channel();
        core.commands = HostSender::for_test(probe);
        core.dispatch(Intent::SelectConversation {
            id: "telegram:2".into(),
        });
        let mut views = Vec::new();
        while let Ok(command) = sent.try_recv() {
            if let AdapterCommand::ViewChat {
                conversation_id, ..
            } = command
            {
                views.push(conversation_id);
            }
        }
        assert_eq!(views, vec![Some("telegram:2".to_owned())]);

        core.dispatch(Intent::Shutdown);
        while sent.try_recv().is_ok() {}
        core.dispatch(Intent::SelectConversation {
            id: "telegram:1".into(),
        });
        assert!(
            sent.try_recv().is_err(),
            "no ViewChat while the clients close"
        );
    }
}
