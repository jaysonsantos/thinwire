//! eframe application: draws the core view and sends user intents to the core.

mod auth;
#[cfg(test)]
mod inbox_keys;
#[cfg(feature = "signal-local")]
mod signal_gate;
mod theme;
mod theme_mode;
mod thread_layout;
mod ui;
#[cfg(all(test, feature = "ui-snapshots"))]
mod ui_snapshots;
#[cfg(test)]
mod ui_tests;
#[cfg(feature = "whatsapp-web")]
mod whatsapp_gate;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eframe::egui;
use thinwire_core::notify::NotifyKey;
use thinwire_core::{Core, CoreConfig, Intent};
use thinwire_notify::Notifier;

pub use theme::install as install_theme;
pub use theme_mode::apply as apply_theme;
pub use thinwire_core::settings::Settings;

/// Longest wait for TDLib to close before the window closes anyway.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle repaint step (10 Hz). The change signal wakes the window at once for
/// core changes. This timer covers what the core does not see: the OS
/// light/dark switch in System mode (ADR 0005) and the close deadline.
const IDLE_REPAINT: Duration = Duration::from_millis(100);

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

/// Native thinwire window. Protocol and keychain work stay in the core,
/// off the UI thread.
pub struct ThinwireApp {
    core: Core,
    /// Owns the tokio worker threads. Taken at exit for a bounded
    /// `shutdown_timeout` (a plain drop waits for every blocking task, for
    /// example a stuck keychain call).
    runtime: Option<tokio::runtime::Runtime>,
    /// The close gate's deadline; it bounds the runtime shutdown at exit.
    exit_deadline: Option<Instant>,
    intents: Vec<Intent>,
    last_os_theme: Option<egui::Theme>,
    close_gate: CloseGate,
    /// OS notifications on their own thread (#32).
    notifier: Notifier,
    /// Chats of clicked notifications. The notifier thread fills it.
    clicks: Arc<Mutex<Vec<NotifyKey>>>,
    last_focus: Option<bool>,
    last_unread: Option<u32>,
}

impl ThinwireApp {
    #[must_use]
    pub fn new(settings: Settings, ctx: &egui::Context) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime for protocol adapters");
        let config = CoreConfig::new(settings);
        #[cfg(feature = "signal-local")]
        let core = Core::with_signal_adapter(
            runtime.handle(),
            config,
            Box::new(thinwire_signal::SignalAdapter::new()),
        );
        #[cfg(not(feature = "signal-local"))]
        let core = Core::new(runtime.handle(), config);
        repaint_on_change(&runtime, &core, ctx.clone());
        close_on_stop_signal(runtime.handle(), ctx.clone());
        let clicks = Arc::new(Mutex::new(Vec::new()));
        let clicked = Arc::clone(&clicks);
        let wake = ctx.clone();
        let notifier = Notifier::spawn(move |key| {
            lock(&clicked).push(key);
            wake.request_repaint();
        });
        Self {
            core,
            runtime: Some(runtime),
            exit_deadline: None,
            intents: Vec::new(),
            last_os_theme: None,
            close_gate: CloseGate::Open,
            notifier,
            clicks,
            last_focus: None,
            last_unread: None,
        }
    }

    /// Window focus changes and notification clicks become intents (#32).
    fn notification_intents(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|input| input.viewport().focused.unwrap_or(true));
        if self.last_focus != Some(focused) {
            self.last_focus = Some(focused);
            self.intents.push(Intent::WindowFocus(focused));
        }
        let clicked = std::mem::take(&mut *lock(&self.clicks));
        for key in clicked {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.intents.push(Intent::OpenFromNotification(key));
        }
    }

    /// "thinwire (3)" while unread messages wait in chats that are not
    /// muted (#32). Sent only when the count changes.
    fn update_title(&mut self, ctx: &egui::Context) {
        let unread = self.core.view().unread_total();
        if self.last_unread != Some(unread) {
            self.last_unread = Some(unread);
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title(unread)));
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
                    self.core.dispatch(Intent::Shutdown);
                }
                CloseAction::Hold => ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose),
            }
        }
        if self.close_gate.poll(Instant::now(), self.core.stopped()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// Window title with the unread count of chats that are not muted.
fn window_title(unread: u32) -> String {
    if unread == 0 {
        "thinwire".into()
    } else {
        format!("thinwire ({unread})")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Repaint when the core changes, so an idle window needs no poll timer.
fn repaint_on_change(runtime: &tokio::runtime::Runtime, core: &Core, ctx: egui::Context) {
    let mut signal = core.signal();
    runtime.spawn(async move {
        while signal.changed().await {
            ctx.request_repaint();
        }
    });
}

impl eframe::App for ThinwireApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.core.pump();
        self.handle_close(ctx);
        if theme_mode::follow_os_live(ctx, self.core.view().theme(), &mut self.last_os_theme) {
            ctx.request_repaint();
        }
        // Some platforms send no theme event. logic() still runs when the
        // window is hidden after a repaint request.
        ctx.request_repaint_after(IDLE_REPAINT);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let view = self.core.view();
        let mut hints = ui::Hints::from_view(&view);
        ui::draw(ui, &view, &mut hints, &mut self.intents);
        // Clear a hint only after its widget used it (qa L1).
        if hints.used_focus_compose() {
            self.core.take_focus_compose();
        }
        if hints.used_scroll_to_selected() {
            self.core.take_scroll_to_selected();
        }
        if hints.used_scroll_to_focused() {
            self.core.take_scroll_to_focused();
        }
        self.notification_intents(ui.ctx());
        for intent in self.intents.drain(..) {
            self.core.dispatch(intent);
        }
        self.notifier.send(self.core.take_notify());
        self.update_title(ui.ctx());
    }

    /// Safety net for an exit that skipped the close gate. The window is gone,
    /// so a short block here does not freeze the UI.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.close_gate != CloseGate::Done && !self.core.stopped() {
            let deadline = *self
                .exit_deadline
                .get_or_insert_with(|| Instant::now() + SHUTDOWN_TIMEOUT);
            self.core
                .block_until_stopped(deadline.saturating_duration_since(Instant::now()));
        }
        self.finish_exit();
    }
}

impl ThinwireApp {
    /// Last step at exit: try the keychain flush first, then stop the runtime
    /// within the close deadline instead of waiting for every blocking task.
    fn finish_exit(&mut self) {
        self.core.flush_keychain();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(exit_budget(self.exit_deadline, Instant::now()));
        }
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn close_waits_for_stopped_then_allows_the_next_request() {
        let start = Instant::now();
        let mut gate = CloseGate::Open;
        assert_eq!(gate.on_close_requested(start), CloseAction::HoldAndShutdown);
        assert!(matches!(gate, CloseGate::Waiting { .. }));
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
        let flush = finish.find("flush_keychain()").expect("flush first");
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

    /// ADR 0005: System mode follows the OS while idle. Some platforms send
    /// no theme event, so `logic()` asks for a repaint on every frame (qa M1).
    #[test]
    fn idle_repaint_keeps_the_os_theme_follow_live() {
        assert_eq!(IDLE_REPAINT, Duration::from_millis(100));
        let src = include_str!("mod.rs");
        let logic = &src[src.find("fn logic(").expect("logic")..];
        let logic = &logic[..logic.find("\n    fn ").expect("next fn")];
        assert!(logic.contains("theme_mode::follow_os_live("));
        let repaint = logic
            .find("ctx.request_repaint_after(IDLE_REPAINT);")
            .expect("unconditional idle repaint");
        let line_start = logic[..repaint].rfind('\n').expect("line");
        assert_eq!(
            &logic[line_start + 1..repaint],
            "        ",
            "the idle repaint is not inside an if"
        );
    }
}
