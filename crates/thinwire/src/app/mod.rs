//! eframe application: polls adapter events and draws the shell.

mod auth;
mod secrets;
mod settings;
mod snapshot;
mod thread_layout;
mod ui;
#[cfg(feature = "whatsapp-web")]
mod whatsapp_gate;

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use thinwire_protocol::{
    AdapterCommand, AdapterHost, DiscordAdapter, DiscordSecretVault, ProtocolId,
    TelegramSecretVault, WhatsAppPhoneVault,
};

use secrets::SecretStore;
use snapshot::Snapshot;

pub use settings::Settings;

/// Longest wait for TDLib to close before the window closes anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
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
                    tracing::warn!("telegram did not close in time; closing the window anyway");
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
    runtime: tokio::runtime::Runtime,
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
        Self {
            runtime,
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.host.send(AdapterCommand::Shutdown {
                        protocol: ProtocolId::Telegram,
                    });
                    self.snapshot.status_text = "Closing Telegram…".into();
                }
                CloseAction::Hold => ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose),
            }
        }
        if self
            .close_gate
            .poll(Instant::now(), self.snapshot.telegram_stopped())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
            self.secrets.spawn_os_flush(self.runtime.handle());
        }
        if self.snapshot.take_keychain_retry() {
            self.secrets.spawn_os_attach(self.runtime.handle());
        }
        if let Some(job) = self.settings.take_persist_job() {
            self.runtime.handle().spawn_blocking(move || job.run());
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
        if self.close_gate == CloseGate::Done || self.snapshot.telegram_stopped() {
            return;
        }
        self.host.send(AdapterCommand::Shutdown {
            protocol: ProtocolId::Telegram,
        });
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        while Instant::now() < deadline {
            self.drain_events();
            if self.snapshot.telegram_stopped() {
                return;
            }
            std::thread::sleep(SHUTDOWN_POLL);
        }
        tracing::warn!("telegram did not close before exit");
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
    fn close_gives_up_after_the_deadline() {
        let start = Instant::now();
        let mut gate = CloseGate::Open;
        gate.on_close_requested(start);
        assert!(!gate.poll(start + SHUTDOWN_TIMEOUT / 2, false));
        assert!(gate.poll(start + SHUTDOWN_TIMEOUT, false));
        assert_eq!(gate, CloseGate::Done);
    }

    #[tokio::test]
    async fn shutdown_reaches_the_telegram_adapter_and_reports_stopped() {
        let store = SecretStore::memory();
        let store = Arc::new(store);
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let mut host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&store) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&store) as Arc<dyn DiscordSecretVault>,
            whatsapp_phone,
        );
        host.send(AdapterCommand::Shutdown {
            protocol: ProtocolId::Telegram,
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if host.poll_events().iter().any(|event| {
                matches!(
                    event,
                    AdapterEvent::Stopped {
                        protocol: ProtocolId::Telegram
                    }
                )
            }) {
                return;
            }
            assert!(Instant::now() < deadline, "no Stopped event");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
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
