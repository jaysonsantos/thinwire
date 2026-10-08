//! eframe application: draws the core view and sends user intents to the core.

mod auth;
#[cfg(test)]
mod inbox_keys;
mod motion;
#[cfg(feature = "signal-local")]
mod signal_gate;
pub mod startup;
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

/// Longest wait for TDLib to close before the window closes anyway. The
/// adapters derive their own shutdown bounds from this limit.
const SHUTDOWN_TIMEOUT: Duration = thinwire_protocol::APP_CLOSE_LIMIT;
/// Wait after the close deadline before the watchdog flushes and exits.
/// The normal exit path needs at most the notification flush and
/// [`EXIT_FLOOR`] inside this window, so the watchdog does not cut it short.
const WATCHDOG_MARGIN: Duration = Duration::from_millis(1500);
// The watchdog never cuts the normal exit path short.
const _: () =
    assert!(WATCHDOG_MARGIN.as_millis() > NOTIFY_FLUSH_LIMIT.as_millis() + EXIT_FLOOR.as_millis());
/// From the close deadline until the watchdog calls `process::exit`.
/// It waits [`WATCHDOG_MARGIN`], runs the keychain flush, then sleeps
/// [`EXIT_FLOOR`]. The single-instance lock stays held until that exit, so
/// [`startup::LOCK_WAIT`] includes this whole interval.
const WATCHDOG_LOCK_HOLD: Duration = WATCHDOG_MARGIN.saturating_add(EXIT_FLOOR);
const _: () =
    assert!(WATCHDOG_LOCK_HOLD.as_millis() == WATCHDOG_MARGIN.as_millis() + EXIT_FLOOR.as_millis());
/// Exit code when the watchdog ends the process. The user asked to close,
/// so it is a normal exit.
const WATCHDOG_EXIT_CODE: i32 = 0;
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
    /// Hide the window, keep the process, and send `Shutdown` once.
    HoldAndShutdown,
    Hold,
}

/// The window commands for a close request.
///
/// The first close hides the window at once, so nobody clicks close again
/// while the clients close. winit cannot hide a window on Wayland
/// (`set_visible` does nothing there), so on Wayland the window is
/// minimized instead. A later close only cancels the close again.
fn close_commands(action: CloseAction, wayland: bool) -> Vec<egui::ViewportCommand> {
    match action {
        CloseAction::Allow => Vec::new(),
        CloseAction::HoldAndShutdown => {
            let mut commands = vec![
                egui::ViewportCommand::CancelClose,
                egui::ViewportCommand::Visible(false),
            ];
            if wayland {
                commands.push(egui::ViewportCommand::Minimized(true));
            }
            commands
        }
        CloseAction::Hold => vec![egui::ViewportCommand::CancelClose],
    }
}

/// The window is a Wayland surface. Only there `Visible(false)` does nothing.
fn is_wayland(frame: &eframe::Frame) -> bool {
    use winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
    frame
        .window_handle()
        .is_ok_and(|handle| matches!(handle.as_raw(), RawWindowHandle::Wayland(_)))
}

/// End the process at `close_deadline + WATCHDOG_LOCK_HOLD` if it still runs.
///
/// The close gate is polled only in `logic()`, and `logic()` runs only on a
/// repaint. A hidden or minimized window may get no repaint: on Wayland,
/// winit holds `RedrawRequested` until the compositor sends a frame
/// callback, and a compositor may send none to a hidden surface. Then
/// neither the stopped check nor the deadline would fire, and a hidden
/// process would stay forever. This thread needs no repaint. In the normal
/// case the process exits first and the thread dies with it.
///
/// The thread wakes after [`WATCHDOG_MARGIN`], runs `before_exit`, then
/// sleeps [`EXIT_FLOOR`] before `exit`. That last sleep is part of
/// [`WATCHDOG_LOCK_HOLD`]: the lock is still held while it runs.
fn spawn_exit_watchdog(
    close_deadline: Instant,
    before_exit: impl FnOnce() + Send + 'static,
    exit: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("thinwire-exit-watchdog".into())
        .spawn(move || {
            sleep_until(close_deadline + WATCHDOG_MARGIN);
            tracing::warn!("the window did not finish closing in time; ending the process");
            before_exit();
            std::thread::sleep(EXIT_FLOOR);
            exit();
        })
}

fn sleep_until(at: Instant) {
    loop {
        let left = at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left);
    }
}

/// The first close: one `Shutdown`, then the exit watchdog.
fn begin_first_close(
    core: &mut Core,
    deadline: Instant,
    start_watchdog: impl FnOnce(&mut Core, Instant),
) {
    core.dispatch(Intent::Shutdown);
    start_watchdog(core, deadline);
}

/// Start the exit watchdog once. `flush` is the keychain hook from the core.
/// The watchdog sleeps [`EXIT_FLOOR`] after `flush` returns, before it exits.
fn arm_exit_watchdog(
    armed: &mut bool,
    flush: impl FnOnce() + Send + 'static,
    close_deadline: Instant,
) {
    if std::mem::replace(armed, true) {
        return;
    }
    let started = spawn_exit_watchdog(close_deadline, flush, || {
        std::process::exit(WATCHDOG_EXIT_CODE);
    });
    if let Err(error) = started {
        tracing::warn!(%error, "the exit watchdog did not start");
    }
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

    /// The close deadline while the gate waits.
    const fn deadline(&self) -> Option<Instant> {
        match *self {
            Self::Waiting { deadline } => Some(deadline),
            Self::Open | Self::Done => None,
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
    /// The exit watchdog runs. Started once, on the first close.
    watchdog: bool,
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
        let core = Core::with_replacement_adapters(runtime.handle(), config, local_only_adapters);
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
            watchdog: false,
            notifier,
            clicks,
            last_focus: None,
            last_unread: None,
        }
    }

    /// Send a window focus change to the core. `logic()` calls it before
    /// `pump()`, so a live message of this frame sees the new focus
    /// (#87 review).
    fn update_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|input| {
            let event = input.events.iter().rev().find_map(|event| match event {
                egui::Event::WindowFocused(focused) => Some(*focused),
                _ => None,
            });
            resolve_focus(input.viewport().focused, event, self.last_focus)
        });
        if self.last_focus != Some(focused) {
            self.last_focus = Some(focused);
            self.core.dispatch(Intent::WindowFocus(focused));
        }
    }

    /// Notification clicks become intents (#32).
    /// A show with no click leaves this empty: the window stays put.
    fn notification_intents(&mut self, ctx: &egui::Context) {
        let clicked = std::mem::take(&mut *lock(&self.clicks));
        let (raise, opens) = notification_clicks(&clicked);
        if raise {
            for _ in &opens {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                // `Focus` raises the window on X11, macOS, and Windows. On
                // Wayland, winit 0.30 cannot raise it: ask for attention, so the
                // taskbar entry highlights (#173).
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            }
        }
        self.intents.extend(opens);
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

    /// Hide the window on the first close and hold the process until the
    /// clients stopped, then close. The watchdog ends the process if this
    /// never runs again while the window is hidden.
    fn handle_close(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if ctx.input(|input| input.viewport().close_requested()) {
            let now = Instant::now();
            let action = self.close_gate.on_close_requested(now);
            let wayland = action == CloseAction::HoldAndShutdown && is_wayland(frame);
            for command in close_commands(action, wayland) {
                ctx.send_viewport_cmd(command);
            }
            if action == CloseAction::HoldAndShutdown {
                let deadline = self
                    .close_gate
                    .deadline()
                    .expect("HoldAndShutdown sets the close deadline");
                self.exit_deadline = Some(deadline);
                let watchdog = &mut self.watchdog;
                begin_first_close(&mut self.core, deadline, |core, deadline| {
                    let flush = core.keychain_flush_hook();
                    arm_exit_watchdog(watchdog, flush, deadline);
                });
            }
        }
        if self.close_gate.poll(Instant::now(), self.core.stopped()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            // The next pass reads the close: do not wait for the idle tick.
            ctx.request_repaint();
        }
    }
}

/// Longest wait at exit for the last notification dismisses.
const NOTIFY_FLUSH_LIMIT: Duration = Duration::from_millis(500);

/// Window focus for the notification rules. The platform value wins, then
/// the last focus event of this frame, then the last known value. Unknown
/// at start counts as unfocused, so the open chat still notifies where a
/// platform reports no focus (qa L4).
fn resolve_focus(reported: Option<bool>, event: Option<bool>, last: Option<bool>) -> bool {
    reported.or(event).or(last).unwrap_or(false)
}

/// The local-only AGPL clients of this build, in place of the MIT stubs
/// (ADR 0011, #77). A release build turns on neither feature: no client.
fn local_only_adapters(
    _phone: &Arc<thinwire_protocol::WhatsAppPhoneVault>,
) -> Vec<Box<dyn thinwire_protocol::ProtocolAdapter>> {
    vec![
        #[cfg(feature = "whatsapp-web")]
        Box::new(thinwire_whatsapp::WhatsAppAdapter::new(Arc::clone(_phone))),
        #[cfg(feature = "signal-local")]
        Box::new(thinwire_signal::SignalAdapter::new()),
    ]
}

/// Window title with the unread count of chats that are not muted.
fn window_title(unread: u32) -> String {
    if unread == 0 {
        "thinwire".into()
    } else {
        format!("thinwire ({unread})")
    }
}

/// A click raises the window and opens that chat.
/// No click: neither. Showing a notification does not fill `clicks`.
fn notification_clicks(clicks: &[NotifyKey]) -> (bool, Vec<Intent>) {
    if clicks.is_empty() {
        return (false, Vec::new());
    }
    let open = clicks
        .iter()
        .cloned()
        .map(Intent::OpenFromNotification)
        .collect();
    (true, open)
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
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.update_focus(ctx);
        self.core.pump();
        self.handle_close(ctx, frame);
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
        // Each send with no answer yet is named in the log (protocol and
        // chat id, never the text), so none ends without a trace.
        self.core.log_unfinished_sends();
        // Shutdown queued a dismiss for each shown notification: send them,
        // and give the OS a short time, so none stays after exit.
        self.notifier.send(self.core.take_notify());
        self.notifier.flush(NOTIFY_FLUSH_LIMIT);
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

    use thinwire_core::ProtocolId;

    use super::*;

    #[test]
    fn a_notification_show_without_a_click_does_not_raise_the_window() {
        let (raise, opens) = notification_clicks(&[]);
        assert!(!raise, "showing a notification does not raise the window");
        assert!(opens.is_empty(), "and does not change the open chat");

        let key = NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
        };
        let (raise, opens) = notification_clicks(std::slice::from_ref(&key));
        assert!(raise);
        assert_eq!(opens, vec![Intent::OpenFromNotification(key)]);
    }

    #[test]
    fn the_first_close_hides_the_window_and_starts_the_shutdown() {
        let start = Instant::now();
        let mut gate = CloseGate::Open;
        let action = gate.on_close_requested(start);
        assert_eq!(action, CloseAction::HoldAndShutdown);
        assert_eq!(gate.deadline(), Some(start + SHUTDOWN_TIMEOUT));
        assert_eq!(
            close_commands(action, false),
            vec![
                egui::ViewportCommand::CancelClose,
                egui::ViewportCommand::Visible(false),
            ],
            "the process stays, the window goes"
        );
        assert_eq!(
            close_commands(action, true),
            vec![
                egui::ViewportCommand::CancelClose,
                egui::ViewportCommand::Visible(false),
                egui::ViewportCommand::Minimized(true),
            ],
            "Wayland cannot hide a window: minimize it"
        );

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut core = Core::new(
            runtime.handle(),
            CoreConfig::new(Settings::in_memory()).with_memory_secrets(),
        );
        let deadline = start + SHUTDOWN_TIMEOUT;
        let mut watched = None;
        begin_first_close(&mut core, deadline, |core, at| {
            assert_eq!(
                core.view().status_text,
                "Closing…",
                "Shutdown is dispatched before the watchdog starts"
            );
            watched = Some(at);
        });
        assert_eq!(watched, Some(deadline), "the watchdog starts once");
    }

    #[test]
    fn a_second_close_is_a_no_op() {
        let start = Instant::now();
        let mut gate = CloseGate::Open;
        gate.on_close_requested(start);
        let later = start + SHUTDOWN_TIMEOUT / 2;
        let action = gate.on_close_requested(later);
        assert_eq!(action, CloseAction::Hold);
        assert_eq!(
            close_commands(action, false),
            vec![egui::ViewportCommand::CancelClose],
            "no second hide, no second Shutdown"
        );
        assert_eq!(
            gate.deadline(),
            Some(start + SHUTDOWN_TIMEOUT),
            "the deadline does not move"
        );
        assert_eq!(close_commands(CloseAction::Allow, true), Vec::new());
    }

    #[test]
    fn the_watchdog_ends_the_process_after_the_deadline_and_margin() {
        let (exited, exit) = std::sync::mpsc::channel();
        let (flushed, flush) = std::sync::mpsc::channel();
        let start = Instant::now();
        // A deadline already behind us: only the margin is left.
        let deadline = start.checked_sub(WATCHDOG_MARGIN).unwrap_or(start);
        let handle = spawn_exit_watchdog(
            deadline + Duration::from_millis(20),
            move || flushed.send(Instant::now()).expect("flush"),
            move || exited.send(Instant::now()).expect("exit"),
        )
        .expect("watchdog thread");
        let at = exit.recv_timeout(Duration::from_secs(5)).expect("exit ran");
        let flushed_at = flush.try_recv().expect("the flush runs before the exit");
        assert!(flushed_at <= at);
        // The exit is the margin plus the final EXIT_FLOOR sleep. Dropping
        // that sleep makes this fail, and LOCK_WAIT is tied to the same sum.
        assert!(at >= deadline + Duration::from_millis(20) + WATCHDOG_LOCK_HOLD);
        handle.join().expect("watchdog ends");
    }

    #[test]
    fn the_watchdog_waits_past_the_normal_exit_path() {
        let (exited, exit) = std::sync::mpsc::channel::<()>();
        let _handle = spawn_exit_watchdog(
            Instant::now(),
            || {},
            move || exited.send(()).expect("exit"),
        )
        .expect("watchdog thread");
        assert!(
            exit.recv_timeout(NOTIFY_FLUSH_LIMIT + EXIT_FLOOR).is_err(),
            "no exit while finish_exit may still run"
        );
        exit.recv_timeout(Duration::from_secs(5))
            .expect("exit after the margin");
    }

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
