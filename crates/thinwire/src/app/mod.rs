//! eframe application: polls adapter events and draws the shell.

mod auth;
mod secrets;
mod settings;
mod snapshot;
mod theme;
mod thread_layout;
mod ui;
#[cfg(feature = "whatsapp-web")]
mod whatsapp_gate;

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use thinwire_protocol::{
    AdapterCommand, AdapterHost, DiscordAdapter, DiscordSecretVault, ProtocolId,
    TelegramSecretVault, WhatsAppPhoneVault, catalog,
};

use secrets::SecretStore;
use snapshot::Snapshot;

pub use settings::Settings;
pub use theme::install as install_theme;

/// Longest wait for TDLib to close before the window closes anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Least time the runtime gets at exit, so the last keychain flush can start.
const EXIT_FLOOR: Duration = Duration::from_millis(200);
/// Runtime budget at exit when no close deadline was recorded.
const EXIT_DEFAULT: Duration = Duration::from_secs(1);

/// Time left for the runtime at exit: up to the close deadline, never below
/// [`EXIT_FLOOR`]. A blocked keychain task cannot hold the exit longer.
fn exit_budget(deadline: Option<Instant>, now: Instant) -> Duration {
    deadline
        .map_or(EXIT_DEFAULT, |deadline| {
            deadline.saturating_duration_since(now)
        })
        .max(EXIT_FLOOR)
}

/// Poll step while `on_exit` waits for a late shutdown.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// What to do on a stop signal (SIGTERM, SIGINT, or Ctrl+C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalAction {
    /// First signal: close the window through the close gate, as the button does.
    CloseWindow,
    /// A later signal: the user insists. Exit now, without the TDLib close.
    ExitNow,
}

const fn on_stop_signal(seen_before: u32) -> SignalAction {
    if seen_before == 0 {
        SignalAction::CloseWindow
    } else {
        SignalAction::ExitNow
    }
}

/// Exit code for a forced exit after a second stop signal (128 + SIGINT).
const FORCED_EXIT_CODE: i32 = 130;

/// Turn stop signals into a window close, so the close gate closes TDLib.
/// Without this, a desktop logout or `kill` ends the process with TDLib open.
fn close_on_stop_signal(runtime: &tokio::runtime::Handle, ctx: egui::Context) {
    runtime.spawn(async move {
        let Some(mut signals) = StopSignals::install() else {
            tracing::warn!("stop signal handlers were not installed");
            return;
        };
        let mut seen = 0;
        while signals.recv().await {
            match on_stop_signal(seen) {
                SignalAction::CloseWindow => {
                    tracing::info!("stop signal; closing the window");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    ctx.request_repaint();
                }
                SignalAction::ExitNow => {
                    tracing::warn!("second stop signal; exiting without closing Telegram");
                    std::process::exit(FORCED_EXIT_CODE);
                }
            }
            seen += 1;
        }
    });
}

/// SIGTERM and SIGINT on Unix. Ctrl+C elsewhere.
#[cfg(unix)]
struct StopSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    fn install() -> Option<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Some(Self {
            terminate: signal(SignalKind::terminate()).ok()?,
            interrupt: signal(SignalKind::interrupt()).ok()?,
        })
    }

    /// `false` when the signal stream ended.
    async fn recv(&mut self) -> bool {
        tokio::select! {
            got = self.terminate.recv() => got.is_some(),
            got = self.interrupt.recv() => got.is_some(),
        }
    }
}

#[cfg(not(unix))]
struct StopSignals;

#[cfg(not(unix))]
impl StopSignals {
    fn install() -> Option<Self> {
        Some(Self)
    }

    async fn recv(&mut self) -> bool {
        tokio::signal::ctrl_c().await.is_ok()
    }
}

/// What to do with a window close request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    Allow,
    /// Keep the window open and send `Shutdown` once.
    HoldAndShutdown,
    Hold,
}

/// Holds the window open until TDLib closed cleanly, or until a deadline.
///
/// An exit while TDLib runs aborts the process and can damage its database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseGate {
    Open,
    Waiting { deadline: Instant },
    Done,
}

impl CloseGate {
    fn on_close_requested(&mut self, now: Instant) -> CloseAction {
        match *self {
            Self::Open => {
                *self = Self::Waiting {
                    deadline: now + SHUTDOWN_TIMEOUT,
                };
                CloseAction::HoldAndShutdown
            }
            Self::Waiting { .. } => CloseAction::Hold,
            Self::Done => CloseAction::Allow,
        }
    }

    /// UI commands flow only before a close request.
    const fn accepts_commands(self) -> bool {
        matches!(self, Self::Open)
    }

    /// `true` once: the window may close now.
    fn poll(&mut self, now: Instant, stopped: bool) -> bool {
        match *self {
            Self::Waiting { deadline } if stopped || now >= deadline => {
                if !stopped {
                    tracing::warn!("adapters did not close in time; closing the window anyway");
                }
                *self = Self::Done;
                true
            }
            Self::Open | Self::Waiting { .. } | Self::Done => false,
        }
    }
}

/// Native thinwire window. Protocol and keychain work stay off the UI thread.
pub struct ThinwireApp {
    /// Taken at exit for a bounded `shutdown_timeout` (a plain drop waits for
    /// every blocking task, for example a stuck keychain call).
    runtime: Option<tokio::runtime::Runtime>,
    handle: tokio::runtime::Handle,
    /// The close gate's deadline; it bounds the runtime shutdown at exit.
    exit_deadline: Option<Instant>,
    host: AdapterHost,
    snapshot: Snapshot,
    settings: Settings,
    secrets: Arc<SecretStore>,
    whatsapp_phone: Arc<WhatsAppPhoneVault>,
    last_os_theme: Option<egui::Theme>,
    close_gate: CloseGate,
}

impl ThinwireApp {
    #[must_use]
    pub fn new(settings: Settings, ctx: &egui::Context) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime for protocol adapters");
        let secrets = SecretStore::for_ui(runtime.handle());
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let host = AdapterHost::spawn(
            runtime.handle(),
            Arc::clone(&secrets) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&secrets) as Arc<dyn DiscordSecretVault>,
            Arc::clone(&whatsapp_phone),
        );
        // `for_ui` only schedules keychain attach. Discord's start can run
        // before that blocking read finishes, so arm it again once a token
        // is in memory. The hook sends a command; it does not touch the OS store.
        bind_discord_after_hydrate(&secrets, &host);
        close_on_stop_signal(runtime.handle(), ctx.clone());
        let mut snapshot = Snapshot::new();
        snapshot.status_text = secret_store_status_text(secrets.backend_name());
        let handle = runtime.handle().clone();
        Self {
            runtime: Some(runtime),
            handle,
            exit_deadline: None,
            host,
            snapshot,
            settings,
            secrets,
            whatsapp_phone,
            last_os_theme: None,
            close_gate: CloseGate::Open,
        }
    }

    /// Hold a close request until Telegram stopped, then close the window.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.viewport().close_requested()) {
            match self.close_gate.on_close_requested(Instant::now()) {
                CloseAction::Allow => {}
                CloseAction::HoldAndShutdown => {
                    self.exit_deadline = Some(Instant::now() + SHUTDOWN_TIMEOUT);
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.shutdown_all_adapters();
                    self.snapshot.status_text = "Closing…".into();
                }
                CloseAction::Hold => ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose),
            }
        }
        if self
            .close_gate
            .poll(Instant::now(), self.snapshot.all_stopped())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Ask every registered adapter to close its sessions (TDLib, the
    /// WhatsApp bot and its SQLite session, Discord, Slack). Each answers
    /// `Stopped`; the close gate waits for all of them within one deadline.
    fn shutdown_all_adapters(&self) {
        for caps in catalog() {
            self.host
                .send(AdapterCommand::Shutdown { protocol: caps.id });
        }
    }

    fn drain_events(&mut self) {
        for event in self.host.poll_events() {
            self.snapshot.apply(event);
        }
    }

    fn refresh_secret_store_status(&mut self) {
        if let Some(text) =
            refreshed_secret_store_status(&self.snapshot.status_text, self.secrets.backend_name())
        {
            self.snapshot.status_text = text;
        }
    }

    fn flush_commands(&mut self) {
        let commands = self.snapshot.take_commands();
        // While the window closes, a click must not reach a protocol worker.
        if self.close_gate.accepts_commands() {
            for command in commands {
                self.host.send(command);
            }
        }
        if self.snapshot.take_keychain_flush() {
            self.secrets.spawn_os_flush(&self.handle);
        }
        if self.snapshot.take_keychain_retry() {
            self.secrets.spawn_os_attach(&self.handle);
        }
        if let Some(job) = self.settings.take_persist_job() {
            self.handle.spawn_blocking(move || job.run());
        }
    }

    /// Last step at exit: try the keychain flush first, then stop the runtime
    /// within the close deadline instead of waiting for every blocking task.
    fn finish_exit(&mut self) {
        self.secrets.spawn_os_flush(&self.handle);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(exit_budget(self.exit_deadline, Instant::now()));
        }
    }
}

impl eframe::App for ThinwireApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.refresh_secret_store_status();
        self.snapshot.poll_resume(&self.secrets);
        self.handle_close(ctx);
        if self.settings.follow_os_live(ctx, &mut self.last_os_theme) {
            ctx.request_repaint();
        }
        // Poll the worker channel while idle so events do not wait on input.
        // logic() still runs when the window is hidden after a repaint request.
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(
            ui,
            &mut self.snapshot,
            &mut self.settings,
            &self.secrets,
            &self.whatsapp_phone,
        );
        self.flush_commands();
    }

    /// Safety net for an exit that skipped the close gate. The window is gone,
    /// so a short block here does not freeze the UI.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.close_gate != CloseGate::Done && !self.snapshot.all_stopped() {
            self.shutdown_all_adapters();
            let deadline = *self
                .exit_deadline
                .get_or_insert_with(|| Instant::now() + SHUTDOWN_TIMEOUT);
            while Instant::now() < deadline && !self.snapshot.all_stopped() {
                self.drain_events();
                std::thread::sleep(SHUTDOWN_POLL);
            }
            if !self.snapshot.all_stopped() {
                tracing::warn!("adapters did not close before exit");
            }
        }
        self.finish_exit();
    }
}

impl Drop for ThinwireApp {
    /// Safety net when `on_exit` did not run: still no unbounded wait.
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(exit_budget(self.exit_deadline, Instant::now()));
        }
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

const SECRET_STORE_STATUS_PREFIX: &str = "Sign in with Telegram to get started.";

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
    use std::time::Instant;

    use thinwire_protocol::{AdapterEvent, AdapterStatus, DiscordSecretVault, TelegramSecretVault};
    use tokio::runtime::Handle;

    use super::*;

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

        let deadline = Instant::now() + Duration::from_secs(3);
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

        let deadline = Instant::now() + Duration::from_secs(3);
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

    #[test]
    fn close_waits_for_stopped_then_allows_the_next_request() {
        let start = Instant::now();
        let mut gate = CloseGate::Open;
        assert!(gate.accepts_commands());
        assert_eq!(gate.on_close_requested(start), CloseAction::HoldAndShutdown);
        assert!(
            !gate.accepts_commands(),
            "no clicks reach a worker while closing"
        );
        assert_eq!(
            gate.on_close_requested(start),
            CloseAction::Hold,
            "one Shutdown only"
        );
        assert!(!gate.poll(start, false));
        assert!(gate.poll(start, true));
        assert!(!gate.poll(start, true), "close fires once");
        assert_eq!(gate.on_close_requested(start), CloseAction::Allow);
    }

    #[test]
    fn first_stop_signal_closes_the_window_and_a_second_one_exits() {
        assert_eq!(on_stop_signal(0), SignalAction::CloseWindow);
        assert_eq!(on_stop_signal(1), SignalAction::ExitNow);
        assert_eq!(on_stop_signal(5), SignalAction::ExitNow);
        let src = include_str!("mod.rs");
        let spawn = &src[src.find("fn close_on_stop_signal").expect("fn")..];
        let spawn = &spawn[..spawn.find("\n}\n").expect("end")];
        assert!(
            spawn.contains("ViewportCommand::Close"),
            "a signal goes through the same close gate as the button"
        );
        assert!(src.contains("SignalKind::terminate()"));
        assert!(src.contains("SignalKind::interrupt()"));
    }

    #[test]
    fn exit_budget_stays_within_the_close_deadline() {
        let now = Instant::now();
        assert_eq!(exit_budget(None, now), EXIT_DEFAULT);
        assert_eq!(
            exit_budget(Some(now + Duration::from_secs(3)), now),
            Duration::from_secs(3)
        );
        assert_eq!(
            exit_budget(Some(now), now + Duration::from_secs(2)),
            EXIT_FLOOR,
            "a passed deadline still gives the flush a short start"
        );
        assert!(exit_budget(Some(now + SHUTDOWN_TIMEOUT), now) <= SHUTDOWN_TIMEOUT);
    }

    #[test]
    fn exit_flushes_first_and_never_drops_the_runtime_unbounded() {
        let src = include_str!("mod.rs");
        let finish = &src[src.find("fn finish_exit(").expect("finish")..];
        let finish = &finish[..finish.find("\n    }\n").expect("end")];
        let flush = finish.find("spawn_os_flush").expect("flush first");
        let stop = finish
            .find("shutdown_timeout(exit_budget(")
            .expect("bounded stop");
        assert!(
            flush < stop,
            "keychain flush attempt before the runtime stops"
        );
        assert!(src.contains("runtime: Option<tokio::runtime::Runtime>"));
        let drop = &src[src.find("impl Drop for ThinwireApp").expect("drop")..];
        assert!(drop.contains("shutdown_timeout(exit_budget("));
        let on_exit = &src[src.find("fn on_exit(").expect("on_exit")..];
        let on_exit = &on_exit[..on_exit.find("\n    }\n").expect("end")];
        assert!(on_exit.contains("self.finish_exit()"));
    }

    #[test]
    fn close_gives_up_after_the_deadline() {
        let start = Instant::now();
        let mut gate = CloseGate::Open;
        gate.on_close_requested(start);
        assert!(!gate.poll(start + SHUTDOWN_TIMEOUT / 2, false));
        assert!(gate.poll(start + SHUTDOWN_TIMEOUT, false));
        assert_eq!(gate, CloseGate::Done);
    }

    #[tokio::test]
    async fn shutdown_reaches_every_adapter_and_each_reports_stopped() {
        let store = SecretStore::memory();
        let store = Arc::new(store);
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
        let deadline = Instant::now() + Duration::from_secs(3);
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
        let src = include_str!("mod.rs");
        let close = &src[src.find("fn handle_close(").expect("close")..];
        let close = &close[..close.find("\n    }\n").expect("end")];
        assert!(close.contains("self.shutdown_all_adapters()"));
        assert!(close.contains("self.snapshot.all_stopped()"));
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
}
